use crate::{
    access::{Decision, EventBatch, EventSubject},
    backend::{Backend, OriginContext},
    core_loop::process_request::{
        RequestOutcome,
        input_ctl::{
            ModmapChangeOutcome, apply_keymap_change, change_modifier_mapping, fetch_merged_keymap,
            legacy_keyboard_mapping_notify,
        },
        motion_history_range,
        xi::{
            XI1_ERROR_BAD_DEVICE, XI1_KEY_MAX, XI1_KEY_MIN, dispatch::xi_write_reply,
            xi1_device_button_count, xi1_device_has_buttons, xi1_device_has_keys,
            xi1_device_has_valuators, xi1_device_valid, xi1_device_valuator_count, xi1_error,
            xi1_zero_reply,
        },
    },
    server::ServerState,
};
use log::debug;
use std::io;
use yserver_protocol::{
    x11,
    x11::{ClientByteOrder, ClientId, SequenceNumber},
};

// GetDeviceMotionEvents: { start, stop, deviceid }. Motion
// history needs valuators; the reply axis count comes from the live
// class (`Xi/gtmotion.c:107-120`).
pub(super) fn handle_xi1_get_device_motion_events(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    byte_order: ClientByteOrder,
    minor: u8,
) -> io::Result<RequestOutcome> {
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    {
        let dev = u16::from(*body.get(8).unwrap_or(&0));
        if !xi1_device_valid(&state.xi_devices, dev) {
            return xi1_error(
                state,
                client_id,
                sequence,
                XI1_ERROR_BAD_DEVICE,
                u32::from(dev),
                minor,
            );
        }
        if !xi1_device_has_valuators(&state.xi_devices, dev) {
            return xi1_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, minor);
        }
        let start = u32::from_le_bytes(body[0..4].try_into().expect("four bytes"));
        let stop = u32::from_le_bytes(body[4..8].try_into().expect("four bytes"));
        let history = motion_history_range(state, start, stop);
        const XI_ABSOLUTE: u8 = 1;
        let axis_count =
            xi1_device_valuator_count(&state.xi_devices, dev).expect("validated valuator class");
        let words_per_event = 1 + u32::from(axis_count);
        let length_words = u32::try_from(history.len())
            .unwrap_or(u32::MAX)
            .saturating_mul(words_per_event);
        let mut reply = x11::fixed_reply(byte_order, sequence, minor, length_words);
        x11::write_u32(
            byte_order,
            &mut reply,
            u32::try_from(history.len()).unwrap_or(u32::MAX),
        );
        reply.push(axis_count);
        reply.push(XI_ABSOLUTE);
        reply.extend_from_slice(&[0u8; 18]);
        for record in history {
            x11::write_u32(byte_order, &mut reply, record.time);
            x11::write_u32(
                byte_order,
                &mut reply,
                i32::from(record.root_x).cast_unsigned(),
            );
            x11::write_u32(
                byte_order,
                &mut reply,
                i32::from(record.root_y).cast_unsigned(),
            );
            for _ in 2..axis_count {
                x11::write_u32(byte_order, &mut reply, 0);
            }
        }
        buf.extend_from_slice(&reply);
    }
    xi_write_reply(state, client_id, buf)
}

// GetFeedbackControl: { deviceid }. yserver models one keyboard
// control + one pointer control (the same state core
// GetKeyboardControl/GetPointerControl expose). Report the device
// class's single default feedback (id 0): a KbdFeedbackState for
// key devices, a PtrFeedbackState for pointer devices — mirroring
// Xorg Xi/getfctl.c, where the KbdFeedback ctrl *is* the keyboard
// control.
pub(super) fn handle_xi1_get_feedback_control(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    byte_order: ClientByteOrder,
    minor: u8,
) -> io::Result<RequestOutcome> {
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    {
        let dev = u16::from(*body.first().unwrap_or(&0));
        if !xi1_device_valid(&state.xi_devices, dev) {
            return xi1_error(
                state,
                client_id,
                sequence,
                XI1_ERROR_BAD_DEVICE,
                u32::from(dev),
                minor,
            );
        }
        let feedbacks = if xi1_device_has_keys(&state.xi_devices, dev) {
            let kc = &state.keyboard_control;
            x11::encode_kbd_feedback_state(
                byte_order,
                0,
                kc.bell_pitch,
                kc.bell_duration,
                kc.led_mask,
                kc.global_auto_repeat,
                kc.key_click_percent,
                kc.bell_percent,
                &kc.auto_repeats,
            )
        } else {
            let pc = &state.pointer_control;
            x11::encode_ptr_feedback_state(
                byte_order,
                0,
                pc.accel_numerator,
                pc.accel_denominator,
                pc.threshold,
            )
        };
        x11::write_get_feedback_control_reply(&mut buf, byte_order, sequence, 1, &feedbacks)?;
    }
    xi_write_reply(state, client_id, buf)
}

// ChangeFeedbackControl (void): { mask, deviceid, feedbackclass }
// followed by the class-specific control. Yserver advertises one
// KbdFeedback (class 0/id 0) on key devices and one PtrFeedback
// (class 1/id 0) on pointer devices. Both alias the shared core
// keyboard/pointer control, just like Xorg.
pub(super) fn handle_xi1_change_feedback_control(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    minor: u8,
) -> io::Result<RequestOutcome> {
    {
        let dev = u16::from(*body.get(4).unwrap_or(&0));
        if !xi1_device_valid(&state.xi_devices, dev) {
            return xi1_error(
                state,
                client_id,
                sequence,
                XI1_ERROR_BAD_DEVICE,
                u32::from(dev),
                minor,
            );
        }
        let mask = u32::from_le_bytes([
            *body.first().unwrap_or(&0),
            *body.get(1).unwrap_or(&0),
            *body.get(2).unwrap_or(&0),
            *body.get(3).unwrap_or(&0),
        ]);
        let feedback_class = *body.get(5).unwrap_or(&u8::MAX);
        match feedback_class {
            0 => {
                if body.len() != 28 {
                    return xi1_error(state, client_id, sequence, x11::error::BAD_LENGTH, 0, minor);
                }
                if !xi1_device_has_keys(&state.xi_devices, dev) || body[9] != 0 {
                    return xi1_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, minor);
                }
                let mut control = state.keyboard_control.clone();
                let mut bad_value = |value: i32| {
                    xi1_error(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_VALUE,
                        value.cast_unsigned(),
                        minor,
                    )
                };
                if mask & (1 << 0) != 0 {
                    let value = body[14] as i8;
                    control.key_click_percent = match value {
                        -1 => crate::server::KeyboardControlState::new().key_click_percent,
                        0..=100 => value.cast_unsigned(),
                        _ => return bad_value(i32::from(value)),
                    };
                }
                if mask & (1 << 1) != 0 {
                    let value = body[15] as i8;
                    control.bell_percent = match value {
                        -1 => crate::server::KeyboardControlState::new().bell_percent,
                        0..=100 => value.cast_unsigned(),
                        _ => return bad_value(i32::from(value)),
                    };
                }
                if mask & (1 << 2) != 0 {
                    let value = i16::from_le_bytes([body[16], body[17]]);
                    control.bell_pitch = match value {
                        -1 => crate::server::KeyboardControlState::new().bell_pitch,
                        0.. => value.cast_unsigned(),
                        _ => return bad_value(i32::from(value)),
                    };
                }
                if mask & (1 << 3) != 0 {
                    let value = i16::from_le_bytes([body[18], body[19]]);
                    control.bell_duration = match value {
                        -1 => crate::server::KeyboardControlState::new().bell_duration,
                        0.. => value.cast_unsigned(),
                        _ => return bad_value(i32::from(value)),
                    };
                }
                if mask & (1 << 4) != 0 {
                    let led_mask = u32::from_le_bytes(body[20..24].try_into().expect("four bytes"));
                    let led_values =
                        u32::from_le_bytes(body[24..28].try_into().expect("four bytes"));
                    control.led_mask = (control.led_mask & !led_mask) | (led_values & led_mask);
                }
                let key = body[12];
                if mask & (1 << 6) != 0 {
                    if !(XI1_KEY_MIN..=XI1_KEY_MAX).contains(&key) {
                        return bad_value(i32::from(key));
                    }
                    if mask & (1 << 7) == 0 {
                        return xi1_error(
                            state,
                            client_id,
                            sequence,
                            x11::error::BAD_MATCH,
                            0,
                            minor,
                        );
                    }
                }
                if mask & (1 << 7) != 0 {
                    let mode = body[13];
                    if mask & (1 << 6) == 0 {
                        control.global_auto_repeat = match mode {
                            0 => false,
                            1 => true,
                            2 => crate::server::KeyboardControlState::new().global_auto_repeat,
                            _ => return bad_value(i32::from(mode)),
                        };
                    } else {
                        let index = usize::from(key >> 3);
                        let bit = 1 << (key & 7);
                        match mode {
                            0 => control.auto_repeats[index] &= !bit,
                            1 => control.auto_repeats[index] |= bit,
                            2 => {
                                control.auto_repeats[index] = (control.auto_repeats[index] & !bit)
                                    | (crate::server::DEFAULT_AUTO_REPEATS[index] & bit);
                            }
                            _ => return bad_value(i32::from(mode)),
                        }
                    }
                }
                state.keyboard_control = control;
            }
            1 => {
                if body.len() != 20 {
                    return xi1_error(state, client_id, sequence, x11::error::BAD_LENGTH, 0, minor);
                }
                if !xi1_device_has_valuators(&state.xi_devices, dev) || body[9] != 0 {
                    return xi1_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, minor);
                }
                let mut control = state.pointer_control.clone();
                let defaults = crate::server::PointerControlState::new();
                for (bit, offset, target, default, denominator) in [
                    (
                        0,
                        14,
                        &mut control.accel_numerator,
                        defaults.accel_numerator,
                        false,
                    ),
                    (
                        1,
                        16,
                        &mut control.accel_denominator,
                        defaults.accel_denominator,
                        true,
                    ),
                    (2, 18, &mut control.threshold, defaults.threshold, false),
                ] {
                    if mask & (1 << bit) == 0 {
                        continue;
                    }
                    let value = i16::from_le_bytes([body[offset], body[offset + 1]]);
                    if value == -1 {
                        *target = default;
                    } else if value < 0 || (denominator && value == 0) {
                        return xi1_error(
                            state,
                            client_id,
                            sequence,
                            x11::error::BAD_VALUE,
                            i32::from(value).cast_unsigned(),
                            minor,
                        );
                    } else {
                        *target = value.cast_unsigned();
                    }
                }
                state.pointer_control = control;
            }
            _ => {
                return xi1_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, minor);
            }
        }
        debug!(
            "client {} #{} XI1 ChangeFeedbackControl device={dev} class={feedback_class} mask=0x{mask:x}",
            client_id.0, sequence.0,
        );
    }
    xi_write_reply(state, client_id, Vec::new())
}

// GetDeviceKeyMapping: { deviceid, firstKeyCode, count }. Range
// checks against the advertised 8..=255 keycode space.
pub(super) fn handle_xi1_get_device_key_mapping(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    byte_order: ClientByteOrder,
    minor: u8,
) -> io::Result<RequestOutcome> {
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    {
        let dev = u16::from(*body.first().unwrap_or(&0));
        let first = *body.get(1).unwrap_or(&0);
        let count = *body.get(2).unwrap_or(&0);
        if !xi1_device_valid(&state.xi_devices, dev) {
            return xi1_error(
                state,
                client_id,
                sequence,
                XI1_ERROR_BAD_DEVICE,
                u32::from(dev),
                minor,
            );
        }
        if !xi1_device_has_keys(&state.xi_devices, dev) {
            return xi1_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, minor);
        }
        if first < XI1_KEY_MIN {
            return xi1_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(first),
                minor,
            );
        }
        if u16::from(first) + u16::from(count) > 256 {
            return xi1_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(count),
                minor,
            );
        }
        // The XI1 keyboard devices (3 master, 5 slave) share the
        // server's single physical keymap — Xorg's XkbGetCoreMap is
        // per-device, but yserver models one keyboard. Reuse the core
        // GetKeyboardMapping path so the device map matches what the
        // client sees via opcode 101.
        let (kpc, keysyms) = fetch_merged_keymap(state, backend, origin, first, count);
        x11::write_get_device_key_mapping_reply(&mut buf, byte_order, sequence, kpc, &keysyms)?;
    }
    xi_write_reply(state, client_id, buf)
}

// ChangeDeviceKeyMapping (void): { deviceid, firstKeyCode,
// keySymsPerKeyCode, keyCodes }.
pub(super) fn handle_xi1_change_device_key_mapping(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    byte_order: ClientByteOrder,
    minor: u8,
) -> io::Result<RequestOutcome> {
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    {
        let dev = u16::from(*body.first().unwrap_or(&0));
        let first = *body.get(1).unwrap_or(&0);
        let count = *body.get(3).unwrap_or(&0);
        if !xi1_device_valid(&state.xi_devices, dev) {
            return xi1_error(
                state,
                client_id,
                sequence,
                XI1_ERROR_BAD_DEVICE,
                u32::from(dev),
                minor,
            );
        }
        if first < XI1_KEY_MIN || u16::from(first) + u16::from(count) > 256 {
            return xi1_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(first),
                minor,
            );
        }
        // Write the keysym rows into the shared override store the
        // read paths (minor 24 / core opcode 101) merge over the
        // backend keymap. Body: deviceid(1) firstKeyCode(1)
        // keySymsPerKeyCode(1) keyCodes(1) then keyCodes×kpk CARD32s.
        // Without this the round-trip silently dropped the change
        // (XTS XChangeDeviceKeyMapping-3).
        let kpk = *body.get(2).unwrap_or(&0);
        // Xorg XkbApplyMappingChange changes and notifies nothing for
        // zero keys.
        let xkb_change = if count == 0 {
            None
        } else {
            apply_keymap_change(
                state,
                backend,
                first,
                kpk,
                count,
                &body[4.min(body.len())..],
            )
        };
        // Same XKB notifications as the core request (Xorg: both reach
        // XkbApplyMappingChange): MapNotify first, ControlsNotify last.
        let xkb_event_base = backend.xkb_info().map_or(0, |(_maj, ev, _err)| ev);
        if let Some(change) = &xkb_change {
            crate::core_loop::xkb_layout::send_xkb_map_notify(
                state,
                xkb_event_base,
                change.map_notify,
            );
        }
        // The core MappingNotify only when the master keyboard changed
        // (Xorg XkbSendLegacyMapNotify → XIShouldNotify), as for
        // SetDeviceModifierMapping.
        if count != 0 && dev == crate::xinput::DEVICEID_MASTER_KEYBOARD {
            legacy_keyboard_mapping_notify(state, xkb_change.as_ref(), first, count);
        }
        // ChangeDeviceKeyMapping is void (no reply), so the event
        // ordering question is moot: send to originator + others.
        // request_kind=1 = MappingKeyboard.
        if crate::core_loop::xi1_focus::xi1_client_wants_device_mapping_notify(
            state, client_id, dev,
        ) {
            let event_description = crate::xinput::xi1_event_description(
                crate::xinput::XI_DEVICE_MAPPING_NOTIFY_OFFSET,
            );
            let events = EventBatch::new(std::slice::from_ref(&event_description));
            if let Decision::Allow = state.check_receive(
                client_id,
                EventSubject::Device(dev),
                &events,
                state.receive_request,
            ) {
                let time = state.timestamp_now();
                #[allow(clippy::cast_possible_truncation)]
                let device_byte = dev as u8;
                crate::xinput::encode_xi1_device_mapping_notify(
                    &mut buf,
                    byte_order,
                    crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_MAPPING_NOTIFY_OFFSET,
                    device_byte,
                    sequence,
                    time,
                    1,
                    first,
                    count,
                );
            }
        }
        crate::core_loop::xi1_focus::emit_device_mapping_notify(
            state,
            Some(client_id),
            dev,
            1,
            first,
            count,
        );
        if let Some(change) = &xkb_change {
            crate::core_loop::xkb_layout::send_keyboard_mapping_followups(
                state,
                xkb_event_base,
                change,
                (crate::core_loop::xkb_layout::X_CHANGE_KEYBOARD_MAPPING, 0),
            );
        }
        debug!(
            "client {} #{} XI1 ChangeDeviceKeyMapping device={dev}",
            client_id.0, sequence.0
        );
    }
    xi_write_reply(state, client_id, buf)
}

// GetDeviceModifierMapping: { deviceid }. Keyboard devices
// share the core keymap, so the reply is the same
// modifier→keycode table the core GetModifierMapping handler
// serves (Xorg Xi/getmmap.c reads the per-device key class —
// ours all alias the core keyboard).
//
// This MUST be real data, not a zero stub: XTS's
// Setup_Extension_DeviceInfo only treats a device as
// modifier-capable when `max_keypermod > 0`, and a zero reply
// silently knocked out every ModMask-gated test (GrabDeviceButton,
// SetDeviceModifierMapping, UngrabDeviceKey-2, ... → UNTESTED).
//
// Wire layout (xGetDeviceModifierMappingReply, XIproto.h:1031):
// numKeyPerModifier lives at byte 8 — NOT the core reply's
// byte-1 data slot — followed by 8*numKeyPerModifier keycodes.
pub(super) fn handle_xi1_get_device_modifier_mapping(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    byte_order: ClientByteOrder,
    minor: u8,
) -> io::Result<RequestOutcome> {
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    {
        let dev = u16::from(*body.first().unwrap_or(&0));
        if !xi1_device_valid(&state.xi_devices, dev) {
            return xi1_error(
                state,
                client_id,
                sequence,
                XI1_ERROR_BAD_DEVICE,
                u32::from(dev),
                minor,
            );
        }
        if !xi1_device_has_keys(&state.xi_devices, dev) {
            return xi1_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, minor);
        }
        // Prefer the per-device override stored by
        // `XSetDeviceModifierMapping`; fall back to the backend's
        // shared core map. xts5 SetDeviceModifierMapping-1 sets
        // the map and immediately reads it back.
        let (kpm, keycodes) = state
            .xi1_modifier_map
            .get(&dev)
            .cloned()
            .unwrap_or_else(|| {
                backend
                    .get_modifier_mapping(origin)
                    .unwrap_or((0, Vec::new()))
            });
        debug_assert_eq!(keycodes.len(), 8 * usize::from(kpm));
        let length_words = u32::from(kpm) * 2; // 8*kpm bytes / 4
        // Byte 1 = RepType = X_GetDeviceModifierMapping (Xi/getmmap.c).
        let mut reply = x11::fixed_reply(byte_order, sequence, minor, length_words);
        reply.push(kpm); // byte 8: numKeyPerModifier
        reply.extend_from_slice(&[0u8; 23]); // bytes 9..=31: pad
        reply.extend_from_slice(&keycodes);
        debug!(
            "client {} #{} XI1 GetDeviceModifierMapping device={dev} kpm={kpm}",
            client_id.0, sequence.0
        );
        buf.extend_from_slice(&reply);
    }
    xi_write_reply(state, client_id, buf)
}

// SetDeviceModifierMapping: { deviceid, numKeyPerModifier,
//   pad1, 8 × numKeyPerModifier keycodes }.
pub(super) fn handle_xi1_set_device_modifier_mapping(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    byte_order: ClientByteOrder,
    minor: u8,
) -> io::Result<RequestOutcome> {
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    {
        let dev = u16::from(*body.first().unwrap_or(&0));
        let kpm = *body.get(1).unwrap_or(&0);
        if !xi1_device_valid(&state.xi_devices, dev) {
            return xi1_error(
                state,
                client_id,
                sequence,
                XI1_ERROR_BAD_DEVICE,
                u32::from(dev),
                minor,
            );
        }
        if !xi1_device_has_keys(&state.xi_devices, dev) {
            return xi1_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, minor);
        }
        let need = 8usize * usize::from(kpm);
        let keycodes: Vec<u8> = (0..need).filter_map(|i| body.get(4 + i).copied()).collect();
        if keycodes.len() != need {
            return xi1_error(state, client_id, sequence, x11::error::BAD_LENGTH, 0, minor);
        }
        // Xorg ProcXSetDeviceModifierMapping → change_modmap, the core
        // request's path: a keycode outside the keycode range (xts5
        // SetDeviceModifierMapping-8 probes MinKeyCode-1) or listed
        // twice is BadValue; a held modifier key is MappingBusy.
        // Busy changed nothing and notifies nothing (Xi/setmmap.c). On
        // success the DeviceMappingNotify goes out before the XKB and
        // core notifications: to the requester (xts5 does
        // `Expect_Event` then `Expect_Reply`, so ahead of the reply too)
        // and to every other client that selected it.
        // request_kind=0 = MappingModifier.
        let notify_device = |state: &mut ServerState| {
            if crate::core_loop::xi1_focus::xi1_client_wants_device_mapping_notify(
                state, client_id, dev,
            ) {
                let event_description = crate::xinput::xi1_event_description(
                    crate::xinput::XI_DEVICE_MAPPING_NOTIFY_OFFSET,
                );
                let events = EventBatch::new(std::slice::from_ref(&event_description));
                let time = state.timestamp_now();
                #[allow(clippy::cast_possible_truncation)]
                let device_byte = dev as u8;
                let _dropped = crate::core_loop::fanout::fanout_event_to_clients_with_receive(
                    state,
                    &[client_id],
                    EventSubject::Device(dev),
                    &events,
                    |buf, seq, order| {
                        crate::xinput::encode_xi1_device_mapping_notify(
                            buf,
                            order,
                            crate::server::XI_FIRST_EVENT
                                + crate::xinput::XI_DEVICE_MAPPING_NOTIFY_OFFSET,
                            device_byte,
                            seq,
                            time,
                            0,
                            0,
                            0,
                        );
                    },
                );
            }
            crate::core_loop::xi1_focus::emit_device_mapping_notify(
                state,
                Some(client_id),
                dev,
                0,
                0,
                0,
            );
        };
        let status = match change_modifier_mapping(
            state,
            backend,
            origin,
            kpm,
            &keycodes,
            Some(dev),
            notify_device,
        ) {
            ModmapChangeOutcome::Success => 0,
            ModmapChangeOutcome::Busy => 1,
            ModmapChangeOutcome::BadValue(value) => {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    value,
                    minor,
                );
            }
        };
        // xSetDeviceModifierMappingReply: RepType @1, success @8.
        let mut reply = x11::fixed_reply(byte_order, sequence, minor, 0);
        reply.push(status);
        reply.extend_from_slice(&[0u8; 23]);
        buf.extend_from_slice(&reply);
    }
    xi_write_reply(state, client_id, buf)
}

// GetDeviceButtonMapping: { deviceid }. The map length follows the
// device's current ButtonClass, including masters whose shape was
// copied from their last slave; Xorg returns b->numButtons and
// b->map (`Xi/getbmap.c:107-115`).
pub(super) fn handle_xi1_get_device_button_mapping(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    byte_order: ClientByteOrder,
    minor: u8,
) -> io::Result<RequestOutcome> {
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    {
        let dev = u16::from(*body.first().unwrap_or(&0));
        if !xi1_device_valid(&state.xi_devices, dev) {
            return xi1_error(
                state,
                client_id,
                sequence,
                XI1_ERROR_BAD_DEVICE,
                u32::from(dev),
                minor,
            );
        }
        if !xi1_device_has_buttons(&state.xi_devices, dev) {
            return xi1_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, minor);
        }
        let button_count = usize::from(
            xi1_device_button_count(&state.xi_devices, dev).expect("validated button class"),
        );
        // nElts lives at byte 8 (first byte after the reply header);
        // map bytes follow the 32-byte header, padded to 4 bytes.
        // Use the device's stored map if set (xts5
        // SetDeviceButtonMapping-1 verifies that a custom map
        // round-trips); otherwise identity.
        let mut reply = x11::fixed_reply(
            byte_order,
            sequence,
            0,
            u32::try_from(button_count.div_ceil(4)).unwrap_or(u32::MAX),
        );
        reply.push(u8::try_from(button_count).expect("XI ButtonClass fits XI1 nElts"));
        reply.extend_from_slice(&[0u8; 23]);
        let stored = state.xi1_button_map.get(&dev).cloned();
        for i in 0..button_count {
            let mapped = stored
                .as_ref()
                .and_then(|m| m.get(i).copied())
                .unwrap_or_else(|| u8::try_from(i + 1).expect("XI ButtonClass fits XI1"));
            reply.push(mapped);
        }
        while !reply.len().is_multiple_of(4) {
            reply.push(0);
        }
        buf.extend_from_slice(&reply);
    }
    xi_write_reply(state, client_id, buf)
}

// SetDeviceButtonMapping: { deviceid, map_length }. Xorg passes the
// supplied length directly to ApplyPointerMapping; it is not capped
// by ButtonClass count (`Xi/setbmap.c:111-121`).
pub(super) fn handle_xi1_set_device_button_mapping(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    byte_order: ClientByteOrder,
    minor: u8,
) -> io::Result<RequestOutcome> {
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    {
        let dev = u16::from(*body.first().unwrap_or(&0));
        let map_length = *body.get(1).unwrap_or(&0);
        if !xi1_device_valid(&state.xi_devices, dev) {
            return xi1_error(
                state,
                client_id,
                sequence,
                XI1_ERROR_BAD_DEVICE,
                u32::from(dev),
                minor,
            );
        }
        if !xi1_device_has_buttons(&state.xi_devices, dev) {
            return xi1_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, minor);
        }
        // Read the map bytes from the body (after the 4-byte
        // header that holds deviceid/map_length/pad). Xorg's
        // ApplyPointerMapping only checks device access and whether a
        // changed held button makes the mapping busy, then copies the
        // supplied bytes (`dix/inpututils.c:43-124`).
        let map_bytes: Vec<u8> = (0..usize::from(map_length))
            .filter_map(|i| body.get(4 + i).copied())
            .collect();
        let current_map = state.xi1_button_map.get(&dev);
        let buttons_down = state
            .xi1_device_input_state
            .get(&dev)
            .map(|device_state| &device_state.buttons_down);
        let mapping_busy = map_bytes.iter().enumerate().any(|(index, new_mapping)| {
            let button = index + 1;
            let previous_mapping = current_map
                .and_then(|mapping| mapping.get(index))
                .copied()
                .unwrap_or_else(|| u8::try_from(button).unwrap_or(u8::MAX));
            let button_is_down = buttons_down.is_some_and(|down| {
                down.get(button / 8)
                    .is_some_and(|byte| byte & (1 << (button % 8)) != 0)
            });
            previous_mapping != *new_mapping && button_is_down
        });
        if mapping_busy {
            // Xorg ApplyPointerMapping returns MappingBusy before copying
            // the map or emitting MappingNotify (`dix/inpututils.c:62-65`).
            let mut reply = xi1_zero_reply(byte_order, sequence);
            reply[8] = 1; // MappingBusy
            buf.extend_from_slice(&reply);
        } else {
            // Xorg copies exactly the supplied prefix into the device's
            // existing map (`dix/inpututils.c:78-80`). Seed an implicit
            // identity map from the current ButtonClass, then retain any
            // tail when a shorter prefix is written.
            let button_count = usize::from(
                xi1_device_button_count(&state.xi_devices, dev).expect("validated button class"),
            );
            let mut updated = state.xi1_button_map.get(&dev).cloned().unwrap_or_else(|| {
                (1..=button_count)
                    .map(|button| u8::try_from(button).unwrap_or(u8::MAX))
                    .collect()
            });
            while updated.len() < map_bytes.len() {
                updated.push(u8::try_from(updated.len() + 1).unwrap_or(u8::MAX));
            }
            updated[..map_bytes.len()].copy_from_slice(&map_bytes);
            state.xi1_button_map.insert(dev, updated);
            // Reply first, then DeviceMappingNotify event in the same
            // outbound write. request_kind=2 = MappingPointer.
            buf.extend_from_slice(&xi1_zero_reply(byte_order, sequence));
            if crate::core_loop::xi1_focus::xi1_client_wants_device_mapping_notify(
                state, client_id, dev,
            ) {
                let event_description = crate::xinput::xi1_event_description(
                    crate::xinput::XI_DEVICE_MAPPING_NOTIFY_OFFSET,
                );
                let events = EventBatch::new(std::slice::from_ref(&event_description));
                if let Decision::Allow = state.check_receive(
                    client_id,
                    EventSubject::Device(dev),
                    &events,
                    state.receive_request,
                ) {
                    let time = state.timestamp_now();
                    #[allow(clippy::cast_possible_truncation)]
                    let device_byte = dev as u8;
                    crate::xinput::encode_xi1_device_mapping_notify(
                        &mut buf,
                        byte_order,
                        crate::server::XI_FIRST_EVENT
                            + crate::xinput::XI_DEVICE_MAPPING_NOTIFY_OFFSET,
                        device_byte,
                        sequence,
                        time,
                        2,
                        0,
                        0,
                    );
                }
            }
            crate::core_loop::xi1_focus::emit_device_mapping_notify(
                state,
                Some(client_id),
                dev,
                2,
                0,
                0,
            );
        }
    }
    xi_write_reply(state, client_id, buf)
}

// QueryDeviceState: { deviceid }. Snapshot of the device's
// key / button / valuator state (Xorg reads current classes at
// `Xi/queryst.c:131-157`) — the same
// source data DeviceStateNotify reports, in the request-reply
// envelope: num_classes at reply byte 8, then xKeyState /
// xButtonState (36 bytes each: class, length, count, pad,
// 32-byte down-bitmask) and xValuatorState (4 bytes + one
// INT32 per axis). `xinput query-state` consumes this.
//
// num_keys here is max_key_code - min_key_code + 1 (248) —
// ONE MORE than the deviceStateNotify event's max-min (247);
// Xorg is inconsistent between the two and clients are built
// against that, so mirror it.
pub(super) fn handle_xi1_query_device_state(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    byte_order: ClientByteOrder,
    minor: u8,
) -> io::Result<RequestOutcome> {
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    {
        let dev = u16::from(*body.first().unwrap_or(&0));
        if !xi1_device_valid(&state.xi_devices, dev) {
            return xi1_error(
                state,
                client_id,
                sequence,
                XI1_ERROR_BAD_DEVICE,
                u32::from(dev),
                minor,
            );
        }
        let dev_state = state
            .xi1_device_input_state
            .get(&dev)
            .copied()
            .unwrap_or_default();
        let class_shape = state
            .xi_devices
            .device(dev)
            .expect("validated XI device")
            .class_shape;
        let mut data: Vec<u8> = Vec::new();
        let mut num_classes = 0u8;
        if xi1_device_has_keys(&state.xi_devices, dev) {
            // xKeyState: keycodes 8..=255 → num_keys = 248.
            data.push(0); // class = KeyClass
            data.push(36); // length
            data.push(248); // num_keys (max - min + 1)
            data.push(0); // pad
            data.extend_from_slice(&dev_state.keys_down);
            num_classes += 1;
        }
        if xi1_device_has_buttons(&state.xi_devices, dev) {
            data.push(1); // class = ButtonClass
            data.push(36); // length
            data.push(class_shape.button_count()); // num_buttons
            data.push(0); // pad
            data.extend_from_slice(&dev_state.buttons_down);
            num_classes += 1;
        }
        if xi1_device_has_valuators(&state.xi_devices, dev) {
            let valuator_count = usize::from(class_shape.valuator_count());
            let valuator_length = 4 + 4 * valuator_count;
            data.push(2); // class = ValuatorClass
            data.push(u8::try_from(valuator_length).expect("XI valuator block fits XI1"));
            data.push(class_shape.valuator_count()); // num_valuators
            data.push(dev_state.valuator_mode); // mode (in-proximity)
            // Stored axis values (Xorg axisVal): axes 0/1 track
            // the sprite under real motion; fakes write their
            // explicit payload.
            for value in dev_state.valuators.iter().take(valuator_count) {
                x11::write_u32(byte_order, &mut data, value.cast_unsigned());
            }
            num_classes += 1;
        }
        debug_assert_eq!(data.len() % 4, 0);
        let length_words = u32::try_from(data.len() / 4).unwrap_or(u32::MAX);
        let mut reply = x11::fixed_reply(byte_order, sequence, 0, length_words);
        reply.push(num_classes); // byte 8
        reply.extend_from_slice(&[0u8; 23]); // bytes 9..=31: pad
        reply.extend_from_slice(&data);
        debug!(
            "client {} #{} XI1 QueryDeviceState device={dev} classes={num_classes}",
            client_id.0, sequence.0
        );
        buf.extend_from_slice(&reply);
    }
    xi_write_reply(state, client_id, buf)
}

// DeviceBell (void): { deviceid, feedbackid, feedbackclass,
// percent }. Yserver exposes KbdFeedback id 0 on key devices but has
// no bell procedure, and exposes no BellFeedback. Xorg
// `Xi/devbell.c::ProcXDeviceBell` returns BadValue for both cases;
// unlike core Bell, this is not a successful bell-less no-op.
#[allow(clippy::needless_return)]
pub(super) fn handle_xi1_device_bell(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    minor: u8,
) -> io::Result<RequestOutcome> {
    let dev = u16::from(*body.first().unwrap_or(&0));
    let feedback_id = *body.get(1).unwrap_or(&0);
    let feedback_class = *body.get(2).unwrap_or(&0);
    #[allow(clippy::cast_possible_wrap)]
    let percent = *body.get(3).unwrap_or(&0) as i8;
    if !xi1_device_valid(&state.xi_devices, dev) {
        return xi1_error(
            state,
            client_id,
            sequence,
            XI1_ERROR_BAD_DEVICE,
            u32::from(dev),
            minor,
        );
    }
    // Xorg validates percent before looking up the feedback.
    if !(-100..=100).contains(&percent) {
        return xi1_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            u32::from(*body.get(3).unwrap_or(&0)),
            minor,
        );
    }
    if !matches!(feedback_class, 0 | 5) {
        return xi1_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            u32::from(feedback_class),
            minor,
        );
    }
    let feedback_exists =
        feedback_class == 0 && feedback_id == 0 && xi1_device_has_keys(&state.xi_devices, dev);
    if !feedback_exists {
        return xi1_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            u32::from(feedback_id),
            minor,
        );
    }
    // The feedback exists, but its BellProc is absent.
    return xi1_error(state, client_id, sequence, x11::error::BAD_VALUE, 0, minor);
}

// SetDeviceValuators: { deviceid, first_valuator, num_valuators,
//   pad1, INT32 values[num_valuators] }. Body layout follows
// `xSetDeviceValuatorsReq` (XIproto.h:1144-1153).
pub(super) fn handle_xi1_set_device_valuators(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    byte_order: ClientByteOrder,
    minor: u8,
) -> io::Result<RequestOutcome> {
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    {
        let dev = u16::from(*body.first().unwrap_or(&0));
        let first = *body.get(1).unwrap_or(&0);
        let num = *body.get(2).unwrap_or(&0);
        if !xi1_device_valid(&state.xi_devices, dev) {
            return xi1_error(
                state,
                client_id,
                sequence,
                XI1_ERROR_BAD_DEVICE,
                u32::from(dev),
                minor,
            );
        }
        if !xi1_device_has_valuators(&state.xi_devices, dev) {
            return xi1_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, minor);
        }
        // `IsXTestDevice` rejects the XTEST pointer with BadMatch,
        // ahead of the axis-range check (`Xi/setdval.c:113-114`).
        if dev == crate::xinput::DEVICEID_XTEST_POINTER {
            return xi1_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, minor);
        }
        let axis_count =
            xi1_device_valuator_count(&state.xi_devices, dev).expect("validated valuator class");
        if u16::from(first) + u16::from(num) > u16::from(axis_count) {
            return xi1_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(num),
                minor,
            );
        }
        // Xorg checks against `dev->valuator->numAxes`
        // (`Xi/setdval.c:110-117`) and
        // `ProcXSetDeviceValuators` returns
        // `AlreadyGrabbed` when another client holds an active
        // grab on the device. `xSetDeviceValuatorsReply.status`
        // sits at byte 8 (right after the 8-byte standard reply
        // header) — XIproto.h:1249-1261.
        const ALREADY_GRABBED: u8 = 1;
        let grabbed_elsewhere = state
            .xi1_active_grabs
            .get(&dev)
            .is_some_and(|g| g.owner != client_id);
        if grabbed_elsewhere {
            let mut reply = x11::fixed_reply(byte_order, sequence, 0, 0);
            reply.push(ALREADY_GRABBED); // byte 8: status
            reply.extend_from_slice(&[0u8; 23]); // bytes 9-31: pad
            buf.extend_from_slice(&reply);
        } else {
            // Persist the new axis values into
            // `xi1_device_input_state` so the next
            // DeviceStateNotify / QueryDeviceState reports them.
            // xts5 XSetDeviceValuators 1 / 2 read each axis back
            // immediately after the set and expect the supplied
            // value verbatim.
            let entry = state.xi1_device_input_state.entry(dev).or_default();
            for i in 0..usize::from(num) {
                let off = 4 + i * 4;
                let Some(slice) = body.get(off..off + 4) else {
                    break;
                };
                let value = i32::from_le_bytes(slice.try_into().expect("4-byte slice"));
                let idx = usize::from(first) + i;
                if let Some(slot) = entry.valuators.get_mut(idx) {
                    *slot = value;
                }
            }
            buf.extend_from_slice(&xi1_zero_reply(byte_order, sequence));
        }
    }
    xi_write_reply(state, client_id, buf)
}

// GetDeviceControl: { control, deviceid }. DEVICE_RESOLUTION(1)
// is the only control defined for XI 1.x core; it needs
// valuators. Reply layout (`xGetDeviceControlReply` +
// `xDeviceResolutionState`, XIproto.h):
//   bytes 0..32  standard reply header (length in 32-bit
//                units of the trailing buf)
//   bytes 32..40 xDeviceResolutionState header:
//                control(2)=1, length(2)=trailing-bytes,
//                num_valuators(4)
//   bytes 40..   3 × num_valuators × CARD32 — resolutions,
//                min_resolutions, max_resolutions.
// Xorg sizes and serializes this from `v->numAxes`
// (`Xi/getdctl.c:192-219`).
// xts5 ChangeDeviceControl-1/-2 set + read back via
// GetDeviceControl; without a real reply the test SEGFAULTs.
pub(super) fn handle_xi1_get_device_control(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    byte_order: ClientByteOrder,
    minor: u8,
) -> io::Result<RequestOutcome> {
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    {
        let control = u16::from_le_bytes([*body.first().unwrap_or(&0), *body.get(1).unwrap_or(&0)]);
        let dev = u16::from(*body.get(2).unwrap_or(&0));
        if !xi1_device_valid(&state.xi_devices, dev) {
            return xi1_error(
                state,
                client_id,
                sequence,
                XI1_ERROR_BAD_DEVICE,
                u32::from(dev),
                minor,
            );
        }
        if control != 1 {
            return xi1_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(control),
                minor,
            );
        }
        if !xi1_device_has_valuators(&state.xi_devices, dev) {
            return xi1_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, minor);
        }
        let num_axes = usize::from(
            xi1_device_valuator_count(&state.xi_devices, dev).expect("validated valuator class"),
        );
        let stored = state.xi1_resolution.get(&dev).cloned();
        let resolutions: Vec<[i32; 3]> = (0..num_axes)
            .map(|i| {
                stored
                    .as_ref()
                    .and_then(|v| v.get(i).copied())
                    .unwrap_or([0, 0, 0])
            })
            .collect();
        // 8 bytes xDeviceResolutionState header + 3 × num_axes × 4
        let trailing_bytes = 8 + 3 * num_axes * 4;
        let length_units = u32::try_from(trailing_bytes / 4).unwrap_or(u32::MAX);
        let mut reply = x11::fixed_reply(byte_order, sequence, 0, length_units);
        // Pad bytes 8..32 of the standard reply header.
        reply.extend_from_slice(&[0u8; 24]);
        // xDeviceResolutionState header
        x11::write_u16(byte_order, &mut reply, 1); // control = DEVICE_RESOLUTION
        #[allow(clippy::cast_possible_truncation)]
        let ctl_len = trailing_bytes as u16;
        x11::write_u16(byte_order, &mut reply, ctl_len);
        x11::write_u32(byte_order, &mut reply, u32::try_from(num_axes).unwrap_or(0));
        // Three runs over axes: resolution, min_resolution, max_resolution.
        for r in &resolutions {
            x11::write_u32(byte_order, &mut reply, r[0].cast_unsigned());
        }
        for r in &resolutions {
            x11::write_u32(byte_order, &mut reply, r[1].cast_unsigned());
        }
        for r in &resolutions {
            x11::write_u32(byte_order, &mut reply, r[2].cast_unsigned());
        }
        buf.extend_from_slice(&reply);
    }
    xi_write_reply(state, client_id, buf)
}

// ChangeDeviceControl: { control, deviceid } + xDeviceCtl
// { control, length, first_valuator, num_valuators, pad2,
//   resolutions[] }.
pub(super) fn handle_xi1_change_device_control(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    byte_order: ClientByteOrder,
    minor: u8,
) -> io::Result<RequestOutcome> {
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    {
        if body.len() < 4 {
            return Ok(RequestOutcome::Handled);
        }
        let control = u16::from_le_bytes([body[0], body[1]]);
        let dev = u16::from(body[2]);
        if !xi1_device_valid(&state.xi_devices, dev) {
            return xi1_error(
                state,
                client_id,
                sequence,
                XI1_ERROR_BAD_DEVICE,
                u32::from(dev),
                minor,
            );
        }
        if control != 1 {
            return xi1_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(control),
                minor,
            );
        }
        // Embedded xDeviceCtl follows at body[4..]; its control id
        // must agree with the request's (Xorg Xi/chgdctl.c →
        // BadMatch on mismatch).
        if body.len() >= 6 {
            let ctl_control = u16::from_le_bytes([body[4], body[5]]);
            if ctl_control != control {
                return xi1_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, minor);
            }
        }
        if !xi1_device_has_valuators(&state.xi_devices, dev) {
            return xi1_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, minor);
        }
        // Xorg initializes relative axes without physical resolution
        // metadata as resolution/min_resolution/max_resolution = 0/0/0
        // (`dix/devices.c::InitValuatorClassDeviceStruct`). Yserver's
        // synthetic relative axes do the same, so zero is the sole valid
        // value and round-trips through GetDeviceControl; nonzero values
        // are correctly outside the advertised range.
        let mut staged: Vec<(usize, i32)> = Vec::new();
        if body.len() >= 10 {
            let first = body[8];
            let num = body[9];
            // Xorg rejects ranges past the current ValuatorClass
            // (`Xi/chgdctl.c:157-160`).
            let axis_count = xi1_device_valuator_count(&state.xi_devices, dev)
                .expect("validated valuator class");
            if u16::from(first) + u16::from(num) > u16::from(axis_count) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    u32::from(num),
                    minor,
                );
            }
            for i in 0..usize::from(num) {
                let off = 12 + i * 4;
                if off + 4 > body.len() {
                    break;
                }
                let res =
                    i32::from_le_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]]);
                if res != 0 {
                    return xi1_error(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_VALUE,
                        res.cast_unsigned(),
                        minor,
                    );
                }
                staged.push((usize::from(first) + i, res));
            }
        }
        // Xorg `Xi/chgdctl.c::ProcXChangeDeviceControl` returns
        // `AlreadyGrabbed` (status byte 8) when another client
        // holds an active grab on the device — xts5
        // ChangeDeviceControl-10.
        const ALREADY_GRABBED: u8 = 1;
        let grabbed_elsewhere = state
            .xi1_active_grabs
            .get(&dev)
            .is_some_and(|g| g.owner != client_id);
        let status = if grabbed_elsewhere {
            ALREADY_GRABBED
        } else {
            // Persist the validated resolutions so the matching
            // `XGetDeviceControl` reports them.
            if !staged.is_empty() {
                let entry = state.xi1_resolution.entry(dev).or_insert_with(|| {
                    vec![
                        [0, 0, 0];
                        usize::from(
                            xi1_device_valuator_count(&state.xi_devices, dev)
                                .expect("validated valuator class")
                        )
                    ]
                });
                for (axis, value) in &staged {
                    if let Some(row) = entry.get_mut(*axis) {
                        row[0] = *value;
                    }
                }
            }
            0
        };
        let mut reply = x11::fixed_reply(byte_order, sequence, 0, 0);
        reply.push(status);
        reply.extend_from_slice(&[0u8; 23]);
        buf.extend_from_slice(&reply);
    }
    xi_write_reply(state, client_id, buf)
}
