use super::*;

pub(super) fn handle_query_keymap(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
) -> io::Result<RequestOutcome> {
    debug!("client {} #{} QueryKeymap", client_id.0, sequence.0);
    let keys_down = state.keys_down;
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let byte_order = client.byte_order;
    let mut buf = x11::fixed_reply(byte_order, sequence, 0, 2);
    buf.extend_from_slice(&keys_down);
    Ok(write_to_client(client, client_id, &buf))
}

pub(super) fn handle_get_pointer_mapping(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
) -> io::Result<RequestOutcome> {
    debug!("client {} #{} GetPointerMapping", client_id.0, sequence.0);
    // Xorg reads PickPointer(client)->button->numButtons and its map
    // (`dix/devices.c:1994-2016`).
    let button_count = usize::from(
        xi1_device_button_count(&state.xi_devices, crate::xinput::DEVICEID_MASTER_POINTER)
            .unwrap_or(0),
    );
    let map: Vec<u8> = (0..button_count)
        .map(|index| {
            state
                .pointer_mapping_override
                .as_ref()
                .and_then(|mapping| mapping.get(index).copied())
                .unwrap_or_else(|| u8::try_from(index + 1).unwrap_or(u8::MAX))
        })
        .collect();
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let byte_order = client.byte_order;
    #[allow(clippy::cast_possible_truncation)]
    let mut buf = x11::fixed_reply(
        byte_order,
        sequence,
        map.len() as u8,
        map.len().div_ceil(4) as u32,
    );
    buf.extend_from_slice(&[0u8; 24]);
    buf.extend_from_slice(&map);
    while !buf.len().is_multiple_of(4) {
        buf.push(0);
    }
    Ok(write_to_client(client, client_id, &buf))
}

/// Build a generic 32-byte stub reply: header + length(0) + zeros.
/// Caller fills `data` byte (offset 1) and any non-zero typed field.
fn stub_reply_32(byte_order: x11::ClientByteOrder, sequence: SequenceNumber, data: u8) -> Vec<u8> {
    let mut buf = x11::fixed_reply(byte_order, sequence, data, 0);
    buf.resize(32, 0);
    buf
}

/// GetKeyboardControl (103): 52-byte reply (length=5). Reflects
/// `ServerState::keyboard_control` (mirrors Xorg
/// `ProcGetKeyboardControl`, `dix/devices.c:2257`).
pub(super) fn handle_get_keyboard_control(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
) -> io::Result<RequestOutcome> {
    debug!("client {} #{} GetKeyboardControl", client_id.0, sequence.0);
    let kc = state.keyboard_control.clone();
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let byte_order = client.byte_order;
    // Layout: reply(1) global_auto_repeat(1) seq(2) length=5(4)
    //         led_mask u32(4) key_click_pct u8(1) bell_pct u8(1)
    //         bell_pitch u16(2) bell_duration u16(2) pad(2)
    //         auto_repeats u8[32]   = 32 + 20 = 52 bytes
    let mut buf = x11::fixed_reply(byte_order, sequence, u8::from(kc.global_auto_repeat), 5);
    let mut tmp = Vec::with_capacity(4);
    x11::write_u32(byte_order, &mut tmp, kc.led_mask);
    buf.extend_from_slice(&tmp);
    buf.push(kc.key_click_percent);
    buf.push(kc.bell_percent);
    tmp.clear();
    x11::write_u16(byte_order, &mut tmp, kc.bell_pitch);
    buf.extend_from_slice(&tmp);
    tmp.clear();
    x11::write_u16(byte_order, &mut tmp, kc.bell_duration);
    buf.extend_from_slice(&tmp);
    buf.extend_from_slice(&[0, 0]); // pad
    buf.extend_from_slice(&kc.auto_repeats);
    debug_assert_eq!(buf.len(), 52);
    Ok(write_to_client(client, client_id, &buf))
}

/// ChangeKeyboardControl (102): value-mask driven update of
/// `ServerState::keyboard_control`. Mirrors Xorg
/// `DoChangeKeyboardControl` (`dix/devices.c:2050`): validate and
/// stage into a copy, commit only on full success.
pub(super) fn handle_change_keyboard_control(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    const KB_KEY_CLICK_PERCENT: u32 = 0x01;
    const KB_BELL_PERCENT: u32 = 0x02;
    const KB_BELL_PITCH: u32 = 0x04;
    const KB_BELL_DURATION: u32 = 0x08;
    const KB_LED: u32 = 0x10;
    const KB_LED_MODE: u32 = 0x20;
    const KB_KEY: u32 = 0x40;
    const KB_AUTO_REPEAT_MODE: u32 = 0x80;
    debug!(
        "client {} #{} ChangeKeyboardControl",
        client_id.0, sequence.0
    );
    if body.len() < 4 {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_LENGTH,
            0,
            header.opcode,
        );
    }
    let mask = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
    // Xorg BadLength: req_len must be exactly 2 + Ones(vmask) units.
    if body.len() != 4 + 4 * mask.count_ones() as usize {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_LENGTH,
            0,
            header.opcode,
        );
    }
    let bad_value = |state: &mut ServerState, value: u32| {
        emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            value,
            header.opcode,
        )
    };
    let mut ctrl = state.keyboard_control.clone();
    // Sentinel: "no KBLed/KBKey given" → led-mode / auto-repeat-mode
    // apply to all leds / the global flag (Xorg DO_ALL).
    let mut led: Option<u8> = None;
    let mut key: Option<u8> = None;
    let mut values = body[4..].chunks_exact(4);
    let mut bit = 1u32;
    while bit != 0 {
        let item = bit & mask;
        bit <<= 1;
        if item == 0 {
            continue;
        }
        let Some(raw) = values
            .next()
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        else {
            break; // unreachable: length validated above
        };
        match item {
            KB_KEY_CLICK_PERCENT => {
                let t = raw as i8;
                if t == -1 {
                    ctrl.key_click_percent = 0; // DEFAULT_KEYBOARD_CLICK
                } else if !(0..=100).contains(&t) {
                    return bad_value(state, i32::from(t) as u32);
                } else {
                    ctrl.key_click_percent = t as u8;
                }
            }
            KB_BELL_PERCENT => {
                let t = raw as i8;
                if t == -1 {
                    ctrl.bell_percent = 50; // DEFAULT_BELL
                } else if !(0..=100).contains(&t) {
                    return bad_value(state, i32::from(t) as u32);
                } else {
                    ctrl.bell_percent = t as u8;
                }
            }
            KB_BELL_PITCH => {
                let t = raw as i16;
                if t == -1 {
                    ctrl.bell_pitch = 400; // DEFAULT_BELL_PITCH
                } else if t < 0 {
                    return bad_value(state, i32::from(t) as u32);
                } else {
                    ctrl.bell_pitch = t as u16;
                }
            }
            KB_BELL_DURATION => {
                let t = raw as i16;
                if t == -1 {
                    ctrl.bell_duration = 100; // DEFAULT_BELL_DURATION
                } else if t < 0 {
                    return bad_value(state, i32::from(t) as u32);
                } else {
                    ctrl.bell_duration = t as u16;
                }
            }
            KB_LED => {
                let l = raw as u8;
                if !(1..=32).contains(&l) {
                    return bad_value(state, u32::from(l));
                }
                if mask & KB_LED_MODE == 0 {
                    return emit_x11_error(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_MATCH,
                        0,
                        header.opcode,
                    );
                }
                led = Some(l);
            }
            KB_LED_MODE => {
                let t = raw as u8;
                match (t, led) {
                    (0, None) => ctrl.led_mask = 0,
                    (0, Some(l)) => ctrl.led_mask &= !(1u32 << (l - 1)),
                    (1, None) => ctrl.led_mask = !0,
                    (1, Some(l)) => ctrl.led_mask |= 1u32 << (l - 1),
                    _ => return bad_value(state, u32::from(t)),
                }
            }
            KB_KEY => {
                let k = raw as u8;
                // min_keycode advertised in the setup reply is 8;
                // max is 255 so no upper check is reachable for a u8.
                if k < 8 {
                    return bad_value(state, u32::from(k));
                }
                if mask & KB_AUTO_REPEAT_MODE == 0 {
                    return emit_x11_error(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_MATCH,
                        0,
                        header.opcode,
                    );
                }
                key = Some(k);
            }
            KB_AUTO_REPEAT_MODE => {
                let t = raw as u8;
                match (t, key) {
                    (0, None) => ctrl.global_auto_repeat = false,
                    (1, None) => ctrl.global_auto_repeat = true,
                    (2, None) => ctrl.global_auto_repeat = true, // DEFAULT_AUTOREPEAT
                    (0..=2, Some(k)) => {
                        let i = usize::from(k >> 3);
                        let m = 1 << (k & 7);
                        match t {
                            0 => ctrl.auto_repeats[i] &= !m,
                            1 => ctrl.auto_repeats[i] |= m,
                            _ => {
                                ctrl.auto_repeats[i] = (ctrl.auto_repeats[i] & !m)
                                    | (crate::server::DEFAULT_AUTO_REPEATS[i] & m);
                            }
                        }
                        // Xorg XkbDisableComputedAutoRepeats: a mapping
                        // change no longer re-derives this key's repeat.
                        ctrl.auto_repeats_explicit[i] |= m;
                    }
                    _ => return bad_value(state, u32::from(t)),
                }
            }
            _ => {
                // Unknown mask bit: Xorg's default case → BadValue.
                return bad_value(state, mask);
            }
        }
    }
    state.keyboard_control = ctrl;
    Ok(RequestOutcome::Handled)
}

/// Bell (104): validate percent (header data byte, INT8 −100..=100,
/// Xorg `ProcBell` `dix/devices.c:2288`). yserver has no audible
/// bell output; the request succeeds without side effects, like
/// Xorg on a bell-less device.
pub(super) fn handle_bell(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
) -> io::Result<RequestOutcome> {
    let percent = header.data as i8;
    debug!(
        "client {} #{} Bell percent={}",
        client_id.0, sequence.0, percent
    );
    if !(-100..=100).contains(&percent) {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            i32::from(percent) as u32,
            header.opcode,
        );
    }
    Ok(RequestOutcome::Handled)
}

/// ChangePointerControl (105): update `ServerState::pointer_control`.
/// Mirrors Xorg `ProcChangePointerControl` (`dix/devices.c:2326`):
/// stage into a copy, commit only on full success.
pub(super) fn handle_change_pointer_control(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    debug!(
        "client {} #{} ChangePointerControl",
        client_id.0, sequence.0
    );
    // Body: accel_num i16, accel_denom i16, threshold i16,
    // do_accel u8, do_thresh u8 = 8 bytes.
    if body.len() < 8 {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_LENGTH,
            0,
            header.opcode,
        );
    }
    let accel_num = i16::from_le_bytes([body[0], body[1]]);
    let accel_denom = i16::from_le_bytes([body[2], body[3]]);
    let threshold = i16::from_le_bytes([body[4], body[5]]);
    let do_accel = body[6];
    let do_thresh = body[7];
    let bad_value = |state: &mut ServerState, value: u32| {
        emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            value,
            header.opcode,
        )
    };
    if do_accel > 1 {
        return bad_value(state, u32::from(do_accel));
    }
    if do_thresh > 1 {
        return bad_value(state, u32::from(do_thresh));
    }
    let mut ctrl = state.pointer_control.clone();
    if do_accel == 1 {
        match accel_num {
            -1 => ctrl.accel_numerator = 2, // DEFAULT_PTR_NUMERATOR
            n if n < 0 => return bad_value(state, i32::from(n) as u32),
            n => ctrl.accel_numerator = n as u16,
        }
        match accel_denom {
            -1 => ctrl.accel_denominator = 1, // DEFAULT_PTR_DENOMINATOR
            n if n <= 0 => return bad_value(state, i32::from(n) as u32),
            n => ctrl.accel_denominator = n as u16,
        }
    }
    if do_thresh == 1 {
        match threshold {
            -1 => ctrl.threshold = 4, // DEFAULT_PTR_THRESHOLD
            n if n < 0 => return bad_value(state, i32::from(n) as u32),
            n => ctrl.threshold = n as u16,
        }
    }
    state.pointer_control = ctrl;
    Ok(RequestOutcome::Handled)
}

/// GetPointerControl (106): 32-byte reply reflecting
/// `ServerState::pointer_control` (Xorg `ProcGetPointerControl`,
/// `dix/devices.c:2405`).
pub(super) fn handle_get_pointer_control(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
) -> io::Result<RequestOutcome> {
    debug!("client {} #{} GetPointerControl", client_id.0, sequence.0);
    let pc = state.pointer_control.clone();
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let byte_order = client.byte_order;
    let mut buf = x11::fixed_reply(byte_order, sequence, 0, 0);
    let mut tmp = Vec::with_capacity(2);
    x11::write_u16(byte_order, &mut tmp, pc.accel_numerator);
    buf.extend_from_slice(&tmp);
    tmp.clear();
    x11::write_u16(byte_order, &mut tmp, pc.accel_denominator);
    buf.extend_from_slice(&tmp);
    tmp.clear();
    x11::write_u16(byte_order, &mut tmp, pc.threshold);
    buf.extend_from_slice(&tmp);
    buf.resize(32, 0);
    Ok(write_to_client(client, client_id, &buf))
}

/// SetPointerMapping (116): reply with status=0 + MappingNotify fanout.
pub(super) fn handle_set_pointer_mapping(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    debug!("client {} #{} SetPointerMapping", client_id.0, sequence.0);
    let n = usize::from(header.data);
    let Some(map) = body.get(..n) else {
        return emit_x11_error(state, client_id, sequence, x11::error::BAD_LENGTH, 0, 116);
    };
    // Xorg ProcSetPointerMapping requires the selected pointer's button
    // count and unique nonzero entries (`dix/devices.c:1904-1918`).
    let current_len = usize::from(
        xi1_device_button_count(&state.xi_devices, crate::xinput::DEVICEID_MASTER_POINTER)
            .unwrap_or(0),
    );
    if n != current_len {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            u32::from(header.data),
            116,
        );
    }
    let mut seen = [false; 256];
    for &b in map {
        if b != 0 {
            if seen[usize::from(b)] {
                return emit_x11_error(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    u32::from(b),
                    116,
                );
            }
            seen[usize::from(b)] = true;
        }
    }
    state.pointer_mapping_override = Some(map.to_vec());
    // MappingNotify (request=2 = Pointer) to every connected client.
    let targets: Vec<ClientId> = state.clients.keys().map(|id| ClientId(*id)).collect();
    let _dropped = fanout_event_to_clients(state, &targets, |buf, seq, order| {
        let _ = x11::write_mapping_notify_event(buf, order, seq, 2, 0, 0);
    });
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let buf = stub_reply_32(client.byte_order, sequence, 0); // status=Success
    Ok(write_to_client(client, client_id, &buf))
}

/// SetModifierMapping (118): MappingNotify fanout, then reply.
pub(super) fn handle_set_modifier_mapping(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    debug!("client {} #{} SetModifierMapping", client_id.0, sequence.0);
    let kpm = header.data;
    let need = usize::from(kpm) * 8;
    let Some(keycodes) = body.get(..need) else {
        return emit_x11_error(state, client_id, sequence, x11::error::BAD_LENGTH, 0, 118);
    };
    // Xorg ProcSetModifierMapping: BadValue for a refused map, else the
    // change_modmap status in the reply.
    let status = match change_modifier_mapping(state, backend, origin, kpm, keycodes, None, |_| {})
    {
        ModmapChangeOutcome::Success => 0,
        ModmapChangeOutcome::Busy => 1,
        ModmapChangeOutcome::BadValue(value) => {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                value,
                118,
            );
        }
    };
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let buf = stub_reply_32(client.byte_order, sequence, status);
    Ok(write_to_client(client, client_id, &buf))
}

/// Result of [`change_modifier_mapping`] (Xorg `change_modmap`'s return).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ModmapChangeOutcome {
    /// Applied (`MappingSuccess`).
    Success,
    /// A key that is or would be a modifier is down: nothing applied
    /// (`MappingBusy`).
    Busy,
    /// Refused; the error value (Xorg `client->errorValue`).
    BadValue(u32),
}

/// Xorg's `change_modmap` (dix/inpututils.c) for SetModifierMapping and XI
/// SetDeviceModifierMapping (`xi_device`: the XI device; `None` = core).
///
/// - `build_modmap_from_modkeymap`: row `i / kpm` of the request is modifier
///   `i / kpm`; a keycode listed twice is BadValue with value 0.
/// - `check_modmap_change`: a keycode outside the keycode range is BadValue
///   (value: the lowest such keycode); MappingBusy if a new modifier key is
///   down, or an old one is, where Xorg's loop over the old ones stops short
///   of the last keycode (255).
///
/// Then the backend edits its XKB keymap's modmap and the events go out in
/// Xorg's order (`XkbApplyMappingChange` → `XkbSendNotification`):
/// XkbMapNotify, core MappingNotify(Modifier), XkbControlsNotify for per-key
/// repeat changes, XkbIndicatorMapNotify. A backend without an XKB keymap
/// gets the map stored for readback instead. An identical map still applies
/// and notifies, as on Xorg. Keyboard devices share the one keymap, so the XI
/// request changes it for every device (Xorg: the device, its master or
/// slaves).
///
/// `on_applied` runs once the change took effect and before any notification
/// goes out: the XI request sends its DeviceMappingNotify there, so its
/// clients see it ahead of the XKB and core notifications, as on Xorg.
pub(super) fn change_modifier_mapping(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    kpm: u8,
    keycodes: &[u8],
    xi_device: Option<u16>,
    on_applied: impl FnOnce(&mut ServerState),
) -> ModmapChangeOutcome {
    const MIN_KEYCODE: usize = 8;
    const MAX_KEYCODE: usize = 255;
    let mut modmap = [0u8; 256];
    for (i, &kc) in keycodes.iter().enumerate() {
        if kc == 0 {
            continue;
        }
        if modmap[usize::from(kc)] != 0 {
            return ModmapChangeOutcome::BadValue(0);
        }
        modmap[usize::from(kc)] = 1 << (i / usize::from(kpm.max(1)));
    }
    let down = |kc: usize| state.keys_down[kc >> 3] & (1 << (kc & 7)) != 0;
    for (kc, _) in modmap.iter().enumerate().filter(|(_, m)| **m != 0) {
        if !(MIN_KEYCODE..=MAX_KEYCODE).contains(&kc) {
            return ModmapChangeOutcome::BadValue(u32::try_from(kc).unwrap_or(0));
        }
        if down(kc) {
            return ModmapChangeOutcome::Busy;
        }
    }
    let current = xi_device
        .and_then(|dev| state.xi1_modifier_map.get(&dev).cloned())
        .or_else(|| state.modifier_mapping_override.clone())
        .or_else(|| backend.get_modifier_mapping(origin).ok());
    if let Some((cur_kpm, cur_keys)) = current
        && cur_keys.iter().take(8 * usize::from(cur_kpm)).any(|&kc| {
            (MIN_KEYCODE..MAX_KEYCODE).contains(&usize::from(kc)) && down(usize::from(kc))
        })
    {
        return ModmapChangeOutcome::Busy;
    }

    // Xorg XkbSendLegacyMapNotify → XIShouldNotify: the core MappingNotify
    // is for clients whose master keyboard changed. An XI request on a slave
    // reaches the master only as its lastSlave, which the device an XI client
    // remaps isn't on Xorg (XTEST drives a slave of its own there).
    let core_notify = xi_device.is_none_or(|dev| dev == crate::xinput::DEVICEID_MASTER_KEYBOARD);
    let Some(change) = backend.set_modifier_mapping(&modmap) else {
        match xi_device {
            Some(dev) => {
                state.xi1_modifier_map.insert(dev, (kpm, keycodes.to_vec()));
            }
            None => state.modifier_mapping_override = Some((kpm, keycodes.to_vec())),
        }
        on_applied(state);
        if core_notify {
            let targets: Vec<ClientId> = state.clients.keys().map(|id| ClientId(*id)).collect();
            let _dropped = fanout_event_to_clients(state, &targets, |buf, seq, order| {
                let _ = x11::write_mapping_notify_event(buf, order, seq, 0, 0, 0);
            });
        }
        return ModmapChangeOutcome::Success;
    };
    // The keymap is the one modifier map now: drop any stored readback.
    state.modifier_mapping_override = None;
    state.xi1_modifier_map.clear();
    on_applied(state);
    let xkb_event_base = backend.xkb_info().map_or(0, |(_maj, ev, _err)| ev);
    crate::core_loop::xkb_layout::send_xkb_map_notify(state, xkb_event_base, change.map_notify);
    if core_notify {
        // Xorg XkbSendLegacyMapNotify: MapNotify's changes decide who gets
        // the core MappingNotify(Modifier).
        crate::core_loop::xkb_select::send_legacy_core_map_notify(
            state,
            crate::core_loop::xkb_select::LegacyCause::MapNotify,
            change.map_notify.changed,
            change.map_notify.first_key_sym,
            change.map_notify.n_key_syms,
        );
    }
    crate::core_loop::xkb_layout::send_keyboard_mapping_followups(
        state,
        xkb_event_base,
        &change,
        (crate::core_loop::xkb_layout::X_SET_MODIFIER_MAPPING, 0),
    );
    ModmapChangeOutcome::Success
}

pub(super) fn handle_change_keyboard_mapping(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    let first_keycode = body.first().copied().unwrap_or(8);
    let kpk = body.get(1).copied().unwrap_or(0);
    let count = header.data;
    // Xorg ProcChangeKeyboardMapping (dix/devices.c); BadLength is checked at dispatch.
    if first_keycode < 8 {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            u32::from(first_keycode),
            100,
        );
    }
    // Xorg reports keySymsPerKeyCode as the error value for both conditions.
    if u32::from(first_keycode) + u32::from(count) > 256 || kpk == 0 {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            u32::from(kpk),
            100,
        );
    }
    // XkbApplyMappingChange changes nothing (and notifies nothing) for zero keys.
    if count == 0 {
        return Ok(RequestOutcome::Handled);
    }
    // Body: first_keycode(1) keysyms_per_keycode(1) pad(2) then count × kpk CARD32 keysyms.
    let xkb_change = apply_keymap_change(
        state,
        backend,
        first_keycode,
        kpk,
        count,
        &body[4.min(body.len())..],
    );
    // Xorg's order (XkbSendNotification): XkbMapNotify, then the core
    // MappingNotify (XkbSendLegacyMapNotify), then the ControlsNotify of a
    // per-key repeat change and an IndicatorMapNotify.
    let xkb_event_base = backend.xkb_info().map_or(0, |(_maj, ev, _err)| ev);
    if let Some(change) = &xkb_change {
        crate::core_loop::xkb_layout::send_xkb_map_notify(state, xkb_event_base, change.map_notify);
    }
    // The core MappingNotify (Xorg XkbSendLegacyMapNotify): every client
    // but an XKB client that didn't select the change as a MapNotify detail.
    legacy_keyboard_mapping_notify(state, xkb_change.as_ref(), first_keycode, count);
    if let Some(change) = &xkb_change {
        crate::core_loop::xkb_layout::send_keyboard_mapping_followups(
            state,
            xkb_event_base,
            change,
            (crate::core_loop::xkb_layout::X_CHANGE_KEYBOARD_MAPPING, 0),
        );
    }
    debug!(
        "client {} #{} ChangeKeyboardMapping",
        client_id.0, sequence.0
    );
    Ok(RequestOutcome::Handled)
}

/// Hand the rows to the backend's XKB keymap (Xorg `XkbApplyMappingChange`),
/// else to `state.keymap_overrides`. Returns what the XKB keymap change did,
/// for the XKB notifications; `None` for the core-only store.
pub(super) fn apply_keymap_change(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    first_keycode: u8,
    kpk: u8,
    count: u8,
    syms: &[u8],
) -> Option<crate::backend::KeyboardMappingChange> {
    let n = usize::from(count) * usize::from(kpk);
    let keysyms: Vec<u32> = syms
        .chunks_exact(4)
        .take(n)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    let change = backend.change_keyboard_mapping(first_keycode, kpk, &keysyms);
    if change.is_none() {
        store_keymap_overrides(state, first_keycode, kpk, count, syms);
    }
    change
}

/// The core MappingNotify(Keyboard) of a keyboard mapping change, as Xorg's
/// `XkbSendLegacyMapNotify` sends it from the change's MapNotify: to every
/// client but an XKB-initialised one whose map details miss the change.
/// Without an XKB keymap (`None`) the change counts as KeySyms over the
/// request's keys.
pub(super) fn legacy_keyboard_mapping_notify(
    state: &mut ServerState,
    change: Option<&crate::backend::KeyboardMappingChange>,
    first_keycode: u8,
    count: u8,
) {
    let (changed, first, num) = change.map_or((0x0002, first_keycode, count), |c| {
        let m = c.map_notify;
        (m.changed, m.first_key_sym, m.n_key_syms)
    });
    crate::core_loop::xkb_select::send_legacy_core_map_notify(
        state,
        crate::core_loop::xkb_select::LegacyCause::MapNotify,
        changed,
        first,
        num,
    );
}

/// Install `count` keysym rows starting at `first_keycode` into
/// `state.keymap_overrides`, each row `kpk` keysyms wide, read from
/// `syms` as little-endian CARD32s (the wire is normalised to native
/// byte order before reaching here). Shared by core `ChangeKeyboardMapping`
/// (opcode 100) and XI1 `ChangeDeviceKeyMapping` (minor 25) — Xorg drives
/// both through the same `XkbApplyMappingChange`, so the device map and the
/// core map must stay one store.
fn store_keymap_overrides(
    state: &mut ServerState,
    first_keycode: u8,
    kpk: u8,
    count: u8,
    syms: &[u8],
) {
    for i in 0..usize::from(count) {
        let mut row: Vec<u32> = Vec::with_capacity(usize::from(kpk));
        for j in 0..usize::from(kpk) {
            let off = (i * usize::from(kpk) + j) * 4;
            let Some(b) = syms.get(off..off + 4) else {
                break;
            };
            row.push(u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
        }
        #[allow(clippy::cast_possible_truncation)]
        state
            .keymap_overrides
            .insert(first_keycode.wrapping_add(i as u8), row);
    }
}

/// Fetch the merged keyboard mapping for keycodes `[first_keycode,
/// first_keycode + keycode_count)`: the backend (host/KMS) keymap with
/// any `ChangeKeyboardMapping` rows layered on top. Returns
/// `(keysyms_per_keycode, keysyms)` where `keysyms.len() ==
/// keycode_count * keysyms_per_keycode`. Shared by core
/// `GetKeyboardMapping` (opcode 101) and XI1 `GetDeviceKeyMapping`
/// (minor 24) — Xorg derives both from the same `XkbGetCoreMap`.
pub(super) fn fetch_merged_keymap(
    state: &ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    first_keycode: u8,
    keycode_count: u8,
) -> (u8, Vec<u32>) {
    let proxied = backend
        .get_keyboard_mapping(origin, first_keycode, keycode_count)
        .ok();
    // Merge ChangeKeyboardMapping rows over the backend keymap.
    {
        let (mut kpc, mut keysyms) = proxied.unwrap_or((4, Vec::new()));
        if keysyms.is_empty() {
            keysyms = vec![0u32; usize::from(keycode_count) * usize::from(kpc)];
        }
        let override_kpk = (0..keycode_count)
            .filter_map(|i| {
                state
                    .keymap_overrides
                    .get(&first_keycode.wrapping_add(i))
                    .map(Vec::len)
            })
            .max()
            .unwrap_or(0);
        if override_kpk > usize::from(kpc) {
            // Widen each row to the override width.
            let old = usize::from(kpc);
            let mut widened = Vec::with_capacity(usize::from(keycode_count) * override_kpk);
            for row in keysyms.chunks(old.max(1)) {
                widened.extend_from_slice(row);
                widened.extend(std::iter::repeat_n(0u32, override_kpk - row.len()));
            }
            keysyms = widened;
            #[allow(clippy::cast_possible_truncation)]
            {
                kpc = override_kpk as u8;
            }
        }
        let w = usize::from(kpc);
        for i in 0..usize::from(keycode_count) {
            #[allow(clippy::cast_possible_truncation)]
            let kc = first_keycode.wrapping_add(i as u8);
            if let Some(row) = state.keymap_overrides.get(&kc) {
                for j in 0..w {
                    keysyms[i * w + j] = row.get(j).copied().unwrap_or(0);
                }
            }
        }
        (kpc, keysyms)
    }
}

pub(super) fn handle_get_keyboard_mapping(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    debug!("client {} #{} GetKeyboardMapping", client_id.0, sequence.0);
    let first_keycode = body.first().copied().unwrap_or(8);
    let keycode_count = body.get(1).copied().unwrap_or(0);
    // Xorg ProcGetKeyboardMapping: first < min_keycode or first +
    // count - 1 > max_keycode → BadValue.
    if first_keycode < 8 {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            u32::from(first_keycode),
            101,
        );
    }
    if u32::from(first_keycode) + u32::from(keycode_count) > 256 {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            u32::from(first_keycode) + u32::from(keycode_count) - 1,
            101,
        );
    }
    let merged = fetch_merged_keymap(state, backend, origin, first_keycode, keycode_count);
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let byte_order = client.byte_order;
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    x11::write_get_keyboard_mapping_reply_from_keysyms(
        &mut buf, byte_order, sequence, merged.0, &merged.1,
    )?;
    Ok(write_to_client(client, client_id, &buf))
}

pub(super) fn handle_get_modifier_mapping(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
) -> io::Result<RequestOutcome> {
    debug!("client {} #{} GetModifierMapping", client_id.0, sequence.0);
    let proxied = state
        .modifier_mapping_override
        .clone()
        .or_else(|| backend.get_modifier_mapping(origin).ok());
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let byte_order = client.byte_order;
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    if let Some((kpm, keys)) = proxied {
        x11::write_get_modifier_mapping_reply_with_keycodes(
            &mut buf, byte_order, sequence, kpm, &keys,
        )?;
    } else {
        x11::write_get_modifier_mapping_reply(&mut buf, byte_order, sequence)?;
    }
    Ok(write_to_client(client, client_id, &buf))
}
