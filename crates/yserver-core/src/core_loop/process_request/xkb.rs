use crate::{
    access::{AccessMode, RequestOpcodes, ResourceKind},
    backend::{Backend, OriginContext},
    core_loop::{
        fanout::fanout_event_to_clients,
        process_request::{
            RequestOutcome, X_KB_GET_CONTROLS, X_KB_SELECT_EVENTS, X_KB_SET_COMPAT_MAP,
            X_KB_SET_CONTROLS, X_KB_SET_GEOMETRY, X_KB_SET_INDICATOR_MAP, X_KB_SET_MAP,
            X_KB_SET_NAMES, X_KB_USE_EXTENSION, XKB_LAST_REQUEST, emit_x11_error_with_minor,
            write_to_client,
        },
    },
    server::ServerState,
};
use log::debug;
use std::io;
use yserver_protocol::{
    x11,
    x11::{ClientId, RequestHeader, SequenceNumber},
};

const X_KB_PER_CLIENT_FLAGS: u8 = 21;
const XKB_PCF_ALL_FLAGS_MASK: u32 = 0x1f;
const XKB_PCF_AUTO_RESET_CONTROLS_MASK: u32 = 1 << 2;
const XKB_ALL_BOOLEAN_CONTROLS_MASK: u32 = 0x1fff;
const XKB_USE_CORE_KEYBOARD: u16 = 0x0100;
const XKB_USE_CORE_POINTER: u16 = 0x0200;
const XKB_DEFAULT_XI_ID: u16 = 0x0400;

#[allow(clippy::too_many_arguments)]
pub(super) fn handle_xkb_request(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    let minor = header.data;
    if minor > XKB_LAST_REQUEST {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_REQUEST,
            0,
            u16::from(minor),
            header.opcode,
        );
    }
    // KMS owns the core keyboard and its client-specific detectable-repeat
    // setting. Leave nested host-X11 requests on their existing proxy path.
    if minor == X_KB_PER_CLIENT_FLAGS
        && (origin.is_none() || backend.xkb_per_client_flags_in_core())
    {
        return handle_per_client_flags(state, backend, client_id, sequence, header, body);
    }
    debug!(
        "client {} #{} XkbProxy minor={}",
        client_id.0, sequence.0, minor
    );
    // Xorg's per-request order: the request size (REQUEST_AT_LEAST_SIZE,
    // for the handlers ported so far), then BadAccess for a client that
    // hasn't called XkbUseExtension (every XKB request but UseExtension).
    let min_body = match minor {
        X_KB_SELECT_EVENTS => 12,
        5 => 12, // LatchLockState
        X_KB_SET_CONTROLS => 96,
        X_KB_SET_MAP => 32,
        X_KB_SET_COMPAT_MAP => 12,
        X_KB_SET_INDICATOR_MAP => 8,
        X_KB_SET_NAMES | X_KB_SET_GEOMETRY => 24,
        _ => 0,
    };
    if body.len() < min_body {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_LENGTH,
            0,
            u16::from(minor),
            header.opcode,
        );
    }
    if minor != X_KB_USE_EXTENSION
        && !crate::core_loop::xkb_select::xkb_initialized(state, client_id)
    {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_ACCESS,
            0,
            u16::from(minor),
            header.opcode,
        );
    }
    if minor == 3
        && body.len() >= 24
        && let Err((code, value)) = crate::core_loop::process_request::validate_resource_if_present(
            state,
            client_id,
            yserver_protocol::x11::ResourceId(u32::from_le_bytes(
                body[20..24].try_into().expect("four bytes"),
            )),
            ResourceKind::Window,
            AccessMode::GET_ATTR,
            RequestOpcodes::for_request(header),
        )
    {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            code,
            value,
            u16::from(minor),
            header.opcode,
        );
    }
    let byte_order = state
        .clients
        .get(&client_id.0)
        .map_or(x11::ClientByteOrder::LittleEndian, |c| c.byte_order);
    if minor == X_KB_USE_EXTENSION {
        // Whether the client is supported is the core loop's decision (it
        // tracks the client's XKB state), and so is the reply, which must
        // be in the client's byte order. The backend still sees the request:
        // a nested backend initialises XKB on its host connection with it.
        let supported = crate::core_loop::xkb_select::use_extension(state, client_id, body);
        let _backend_reply = {
            let atoms = &mut state.atoms;
            let mut intern = |name: &str| atoms.intern(name, false).0;
            backend.xkb_proxy(origin, minor, body, &mut intern)
        };
        let Some(client) = state.clients.get_mut(&client_id.0) else {
            return Ok(RequestOutcome::Handled);
        };
        let mut buf = Vec::with_capacity(32);
        x11::write_xkb_use_extension_reply(&mut buf, byte_order, sequence, supported)?;
        return Ok(write_to_client(client, client_id, &buf));
    }
    // SelectEvents is applied here; on success the request still reaches
    // the backend (a nested backend forwards it to its host).
    if minor == X_KB_SELECT_EVENTS
        && let Err((code, value)) =
            crate::core_loop::xkb_select::select_events(state, client_id, body)
    {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            code,
            value,
            u16::from(minor),
            header.opcode,
        );
    }
    // SetControls: the repeat controls are core state for every backend. On
    // success the request still reaches the backend (a nested backend
    // forwards it to its host).
    if minor == X_KB_SET_CONTROLS
        && let Err((code, value)) =
            crate::core_loop::xkb_controls::set_controls(state, &*backend, header.opcode, body)
    {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            code,
            value,
            u16::from(minor),
            header.opcode,
        );
    }
    // The Set* requests the backend's keyboard description implements (a
    // port of Xorg's handler): its events, then its error.
    let ancient = crate::core_loop::xkb_select::xkb_ancient(state, client_id);
    let set_outcome = {
        let atoms = &state.atoms;
        let atom_name = |atom: u32| atoms.name(x11::AtomId(atom)).map(str::to_owned);
        backend.xkb_set(minor, body, ancient, &atom_name)
    };
    if let Some(outcome) = set_outcome {
        crate::core_loop::xkb_layout::resync_xkb_announced(state, backend);
        send_xkb_set_events(state, &*backend, header.opcode, minor, &outcome.events);
        if let Some((code, value)) = outcome.error {
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                code,
                value,
                u16::from(minor),
                header.opcode,
            );
        }
        return Ok(RequestOutcome::Handled);
    }
    // XkbLatchLockState (minor 5): a group lock switches the authoritative
    // keyboard group, a modifier lock sets/clears real modifier locks (this
    // is the path `numlockx` takes via XkbLockModifiers), and either one
    // broadcasts XkbStateNotify to subscribed clients. When a request carries
    // both, both are applied before one XkbStateNotify, as in Xorg.
    // LatchLockState is a void request (no reply), so this runs in the
    // pre-proxy region; the backend's xkb_proxy minor-5 returns None.
    if minor == 5 {
        // Xorg validates the whole request before applying any part of it.
        if let Err((code, value)) = crate::core_loop::xkb_layout::check_latch_lock_state(body) {
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                code,
                value,
                u16::from(minor),
                header.opcode,
            );
        }
        // Modifier and group latches are validated but not applied yet.
        if body[6] != 0 || body[9] != 0 {
            debug!(
                "client {} #{} XkbLatchLockState latches not applied: \
                 affectModLatches={:#04x} latchGroup={}",
                client_id.0, sequence.0, body[6], body[9]
            );
        }
        let requested_group = crate::core_loop::xkb_layout::parse_latch_lock_group(body);
        let requested_mods = crate::core_loop::xkb_layout::parse_latch_lock_mods(body);
        // Neither component present (e.g. a latch-only request): the state
        // is left untouched and nothing is sent.
        if requested_group.is_some() || requested_mods.is_some() {
            // `changed` covers this request's change only.
            crate::core_loop::xkb_layout::resync_xkb_announced(state, backend);
            if let Some(group) = requested_group {
                backend.set_locked_group(group);
            }
            if let Some((affect, values)) = requested_mods {
                backend.set_locked_mods(affect, values);
            }
            crate::core_loop::xkb_layout::announce_xkb_state(
                state,
                backend,
                crate::core_loop::xkb_layout::XkbStateCause {
                    keycode: 0,
                    event_type: 0,
                    request: (header.opcode, 5),
                },
            );
        }
    }
    // XkbGetKbdByName (minor 23): a real keymap load + reply, NOT the
    // minimal stub. The backend owns the keymap/rmlvo/atom-resolution, so it
    // parses the body, loads the requested multi-group keymap, and assembles
    // the nested-block reply; on a successful load it hands back the
    // XkbNewKeyboardInfo this core loop broadcasts as XkbNewKeyboardNotify to
    // ALL clients (Xorg ProcXkbGetKbdByName: XkbSendNewKeyboardNotify after
    // the reply). The fanout lives here because the client tables do.
    if minor == 23 {
        let kbn = {
            let atoms = &mut state.atoms;
            let mut intern = |name: &str| atoms.intern(name, false).0;
            backend.xkb_get_kbd_by_name(body, &mut intern)
        };
        if let Some((mut bytes, notify)) = kbn {
            crate::core_loop::xkb_layout::resync_xkb_announced(state, backend);
            stamp_reply_sequence(&mut bytes, byte_order, sequence);
            // Broadcast NewKeyboardNotify on a successful load, BEFORE writing
            // the reply (Xorg order is reply-then-notify, but the notify fans
            // out to a different client set; ordering across clients is
            // independent, and emitting first keeps the reply write last so an
            // early return can't drop it).
            if let Some(info) = notify {
                let base = backend.xkb_info().map_or(0, |(_maj, ev, _err)| ev);
                let all: Vec<ClientId> = state.clients.keys().map(|id| ClientId(*id)).collect();
                // A full XkbGetKeyboardByName load replaces the whole
                // key-symbol table in Xorg, so stale ChangeKeyboardMapping
                // overrides must not keep shadowing it (see the identical
                // fix + rationale in `apply_rules_names_change`).
                state.keymap_overrides.clear();
                // Legacy core MappingNotify(Keyboard) + MappingNotify(Modifier)
                // to ALL clients, mirroring Xorg's XkbSendLegacyMapNotify
                // companion to XkbSendNewKeyboardNotify. XKB-unaware clients
                // (any plain-X11 WM that never calls XkbSelectEvents — e.g.
                // one that resolves keybinds via XKeysymToKeycode once at
                // startup) never see XkbNewKeyboardNotify, so without this
                // they keep grabbing the pre-switch keycodes forever: typing
                // reflects the new layout (translation is per-keystroke) but
                // WM keyboard shortcuts stay stuck on the old one, because
                // nothing ever told the WM to re-resolve keysym→keycode and
                // re-`GrabKey`. This is the same pair `apply_rules_names_change`
                // sends for the `_XKB_RULES_NAMES`-ChangeProperty path; real
                // `setxkbmap` goes through *this* XkbGetKbdByName path instead,
                // so it needs the same legacy fanout here.
                let count = info
                    .max_keycode
                    .saturating_sub(info.min_keycode)
                    .saturating_add(1);
                let _dropped = fanout_event_to_clients(state, &all, |buf, seq, order| {
                    let _ = x11::write_mapping_notify_event(
                        buf,
                        order,
                        seq,
                        1,
                        info.min_keycode,
                        count,
                    );
                });
                let _dropped = fanout_event_to_clients(state, &all, |buf, seq, order| {
                    let _ = x11::write_mapping_notify_event(buf, order, seq, 0, 0, 0);
                });
                let _dropped = fanout_event_to_clients(state, &all, |buf, seq, order| {
                    let _ = x11::write_xkb_new_keyboard_notify(
                        buf,
                        order,
                        seq,
                        base,
                        1,
                        info.min_keycode,
                        info.max_keycode,
                        info.old_min_keycode,
                        info.old_max_keycode,
                        136, // requestMajor = XkbReqCode
                        23,  // requestMinor = X_kbGetKbdByName
                        info.changed,
                    );
                });
                // Refresh `_XKB_RULES_NAMES` on the root so it reflects the
                // newly-loaded layout — Xorg keeps this property current on
                // every keymap change (e.g. Cinnamon's runtime layout-add).
                // `notify` is Some only when the keymap actually changed.
                crate::core_loop::xkb_layout::publish_xkb_rules_names(state, &*backend);
            }
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            return Ok(write_to_client(client, client_id, &bytes));
        }
        // Backend declined (no real keymap, e.g. test/v1 fixtures): fall
        // through to the proxy's minimal-reply path below.
    }
    // Thread an atom interner so the backend can populate
    // VirtualModNames in the GetNames reply (atoms live in the core
    // loop's table; `backend` is a disjoint borrow from `state.atoms`).
    let reply = {
        let atoms = &mut state.atoms;
        let mut intern = |name: &str| atoms.intern(name, false).0;
        backend
            .xkb_proxy(origin, minor, body, &mut intern)
            .ok()
            .flatten()
    };
    if let Some(mut bytes) = reply {
        stamp_reply_sequence(&mut bytes, byte_order, sequence);
        // GetControls: the repeat controls are the core loop's (Xorg keeps
        // XKB `per_key_repeat` and the feedback's `autoRepeats` in sync, and
        // RepeatKeys with its global auto-repeat), not the backend's.
        if minor == X_KB_GET_CONTROLS {
            crate::core_loop::xkb_controls::patch_get_controls_reply(state, &mut bytes);
        }
        let Some(client) = state.clients.get_mut(&client_id.0) else {
            return Ok(RequestOutcome::Handled);
        };
        return Ok(write_to_client(client, client_id, &bytes));
    }
    Ok(RequestOutcome::Handled)
}

fn handle_per_client_flags(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    // xkbPerClientFlagsReq is 28 bytes on the wire: a 4-byte extension
    // header followed by this exact 24-byte body.
    if body.len() != 24 {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_LENGTH,
            0,
            u16::from(X_KB_PER_CLIENT_FLAGS),
            header.opcode,
        );
    }
    if !crate::core_loop::xkb_select::xkb_initialized(state, client_id) {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_ACCESS,
            0,
            u16::from(X_KB_PER_CLIENT_FLAGS),
            header.opcode,
        );
    }

    let Some(client) = state.clients.get(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let order = client.byte_order;
    let u16_at = |offset: usize| {
        let bytes: [u8; 2] = body[offset..offset + 2]
            .try_into()
            .expect("PerClientFlags field is in the exact-size body");
        match order {
            x11::ClientByteOrder::LittleEndian => u16::from_le_bytes(bytes),
            x11::ClientByteOrder::BigEndian => u16::from_be_bytes(bytes),
        }
    };
    let u32_at = |offset: usize| {
        let bytes: [u8; 4] = body[offset..offset + 4]
            .try_into()
            .expect("PerClientFlags field is in the exact-size body");
        match order {
            x11::ClientByteOrder::LittleEndian => u32::from_le_bytes(bytes),
            x11::ClientByteOrder::BigEndian => u32::from_be_bytes(bytes),
        }
    };
    let device_spec = u16_at(0);
    let change = u32_at(4);
    let value = u32_at(8);
    let ctrls_to_change = u32_at(12);
    let auto_ctrls = u32_at(16);
    let auto_ctrl_values = u32_at(20);

    // XKB's existing core-keyboard replies report device ID 1. Other numeric
    // IDs resolve through the same live XI registry used by device requests,
    // whose role comes from the device's classes. Keep the XKB sentinels as
    // aliases; distinguish a present pointer from a missing device so the
    // error value matches Xorg's BadClass/BadDevice lookup results.
    let device_error_reason = match device_spec {
        XKB_USE_CORE_KEYBOARD | XKB_DEFAULT_XI_ID => None,
        XKB_USE_CORE_POINTER => Some(2), // XkbErr_BadClass
        id if id == u16::from(crate::core_loop::xkb_layout::XKB_DEVICE_ID) => None,
        id => match state.xi_devices.device(id) {
            None => Some(1), // XkbErr_BadDevice
            Some(_)
                if !crate::core_loop::process_request::xi1_device_has_keys(
                    &state.xi_devices,
                    id,
                ) =>
            {
                Some(2)
            } // XkbErr_BadClass
            Some(_) => None,
        },
    };
    if let Some(reason) = device_error_reason {
        let error_base = backend
            .xkb_info()
            .map_or(0, |(_, _, first_error)| first_error);
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            error_base,
            crate::core_loop::xkb_select::err_code2(reason, u32::from(device_spec)),
            u16::from(X_KB_PER_CLIENT_FLAGS),
            header.opcode,
        );
    }
    if change & !XKB_PCF_ALL_FLAGS_MASK != 0 {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            crate::core_loop::xkb_select::err_code2(0x01, change & !XKB_PCF_ALL_FLAGS_MASK),
            u16::from(X_KB_PER_CLIENT_FLAGS),
            header.opcode,
        );
    }
    if value & !change != 0 {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_MATCH,
            crate::core_loop::xkb_select::err_code2(0x02, value & !change),
            u16::from(X_KB_PER_CLIENT_FLAGS),
            header.opcode,
        );
    }
    if change & XKB_PCF_AUTO_RESET_CONTROLS_MASK != 0
        && value & XKB_PCF_AUTO_RESET_CONTROLS_MASK != 0
    {
        if ctrls_to_change & !XKB_ALL_BOOLEAN_CONTROLS_MASK != 0 {
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                crate::core_loop::xkb_select::err_code2(
                    0x03,
                    ctrls_to_change & !XKB_ALL_BOOLEAN_CONTROLS_MASK,
                ),
                u16::from(X_KB_PER_CLIENT_FLAGS),
                header.opcode,
            );
        }
        if auto_ctrls & !ctrls_to_change != 0 {
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                x11::error::BAD_MATCH,
                crate::core_loop::xkb_select::err_code2(0x04, auto_ctrls & !ctrls_to_change),
                u16::from(X_KB_PER_CLIENT_FLAGS),
                header.opcode,
            );
        }
        if auto_ctrl_values & !auto_ctrls != 0 {
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                x11::error::BAD_MATCH,
                crate::core_loop::xkb_select::err_code2(0x05, auto_ctrl_values & !auto_ctrls),
                u16::from(X_KB_PER_CLIENT_FLAGS),
                header.opcode,
            );
        }
    }

    // All checks precede mutation. Preserve other PerClientFlags bits, and
    // maintain AutoResetControls only because its values are part of this
    // request's reply; only DetectableAutoRepeat affects event delivery.
    let xkb = state.xkb_clients.entry(client_id.0).or_default();
    xkb.per_client_flags = (xkb.per_client_flags & !change) | value;
    if change & XKB_PCF_AUTO_RESET_CONTROLS_MASK != 0 {
        if value & XKB_PCF_AUTO_RESET_CONTROLS_MASK == 0 {
            xkb.auto_reset_ctrls = 0;
            xkb.auto_reset_ctrl_values = 0;
        } else {
            xkb.auto_reset_ctrls =
                (xkb.auto_reset_ctrls & !ctrls_to_change) | (auto_ctrls & ctrls_to_change);
            xkb.auto_reset_ctrl_values = (xkb.auto_reset_ctrl_values & !ctrls_to_change)
                | (auto_ctrl_values & ctrls_to_change);
        }
    }
    let flags = *xkb;
    let mut reply = [0u8; 32];
    reply[0] = 1;
    reply[1] = 1;
    let put_u16 = |out: &mut [u8], value: u16| {
        out.copy_from_slice(&match order {
            x11::ClientByteOrder::LittleEndian => value.to_le_bytes(),
            x11::ClientByteOrder::BigEndian => value.to_be_bytes(),
        });
    };
    let put_u32 = |out: &mut [u8], value: u32| {
        out.copy_from_slice(&match order {
            x11::ClientByteOrder::LittleEndian => value.to_le_bytes(),
            x11::ClientByteOrder::BigEndian => value.to_be_bytes(),
        });
    };
    put_u16(&mut reply[2..4], sequence.0);
    put_u32(&mut reply[8..12], XKB_PCF_ALL_FLAGS_MASK);
    put_u32(
        &mut reply[12..16],
        flags.per_client_flags & XKB_PCF_ALL_FLAGS_MASK,
    );
    put_u32(&mut reply[16..20], flags.auto_reset_ctrls);
    put_u32(&mut reply[20..24], flags.auto_reset_ctrl_values);
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    Ok(write_to_client(client, client_id, &reply))
}

/// Write `sequence` into bytes 2..4 of a reply a backend built, in the
/// client's byte order.
fn stamp_reply_sequence(
    bytes: &mut [u8],
    byte_order: x11::ClientByteOrder,
    sequence: SequenceNumber,
) {
    if let Some(field) = bytes.get_mut(2..4) {
        field.copy_from_slice(&match byte_order {
            x11::ClientByteOrder::LittleEndian => sequence.0.to_le_bytes(),
            x11::ClientByteOrder::BigEndian => sequence.0.to_be_bytes(),
        });
    }
}

/// Send what an XKB Set* request did, in Xorg's order: each
/// `XkbSendNewKeyboardNotify` / `XkbSendNotification` with its legacy core
/// MappingNotify, the per-key repeat re-derivation copied to the core
/// keyboard feedback, `_XkbSetCompatMap`'s CompatMapNotify and
/// `XkbApplyLedMapChanges`' indicator notifications. `major`/`minor` are the
/// request's (the events' cause).
fn send_xkb_set_events(
    state: &mut ServerState,
    backend: &dyn Backend,
    major: u8,
    minor: u8,
    events: &[crate::backend::XkbSetEvent],
) {
    use crate::{
        backend::XkbSetEvent,
        core_loop::xkb_select::{self, LegacyCause},
    };
    let base = backend.xkb_info().map_or(0, |(_maj, ev, _err)| ev);
    for event in events {
        match event {
            XkbSetEvent::NewKeyboard(info) => {
                let recipients = xkb_select::new_keyboard_recipients(state, info.changed);
                let _dropped = fanout_event_to_clients(state, &recipients, |buf, seq, order| {
                    let _ = x11::write_xkb_new_keyboard_notify(
                        buf,
                        order,
                        seq,
                        base,
                        1,
                        info.min_keycode,
                        info.max_keycode,
                        info.old_min_keycode,
                        info.old_max_keycode,
                        major,
                        minor,
                        info.changed,
                    );
                });
                let num = info
                    .max_keycode
                    .wrapping_sub(info.min_keycode)
                    .wrapping_add(1);
                xkb_select::send_legacy_map_notify(
                    state,
                    LegacyCause::NewKeyboardNotify,
                    info.changed,
                    info.min_keycode,
                    num,
                );
            }
            XkbSetEvent::Notification(change) => {
                let m = change.map_notify;
                if m.changed != 0 {
                    crate::core_loop::xkb_layout::send_xkb_map_notify(state, base, m);
                    xkb_select::send_legacy_map_notify(
                        state,
                        LegacyCause::MapNotify,
                        m.changed,
                        m.first_key_sym,
                        m.n_key_syms,
                    );
                }
                crate::core_loop::xkb_layout::send_keyboard_mapping_followups(
                    state,
                    base,
                    change,
                    (major, minor),
                );
            }
            XkbSetEvent::Repeats(repeats) => {
                let _ = crate::core_loop::xkb_layout::apply_repeats_to_core(state, repeats);
            }
            XkbSetEvent::CompatMap(notify) => {
                crate::core_loop::xkb_layout::send_xkb_compat_map_notify(state, base, *notify);
            }
            XkbSetEvent::IndicatorMaps(change) => {
                crate::core_loop::xkb_layout::send_indicator_maps_change(state, base, change);
            }
            XkbSetEvent::Names(notify) => {
                crate::core_loop::xkb_layout::send_xkb_names_notify(state, base, *notify);
            }
            XkbSetEvent::IndicatorNames {
                leds_defined,
                state: lit,
            } => {
                crate::core_loop::xkb_layout::send_indicator_names_change(
                    state,
                    base,
                    *leds_defined,
                    *lit,
                );
            }
        }
    }
}
