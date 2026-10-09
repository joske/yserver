use super::*;

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
    debug!(
        "client {} #{} XkbProxy minor={}",
        client_id.0, sequence.0, minor
    );
    // Xorg's per-request order: the request size (REQUEST_AT_LEAST_SIZE,
    // for the handlers ported so far), then BadAccess for a client that
    // hasn't called XkbUseExtension (every XKB request but UseExtension).
    let min_body = match minor {
        X_KB_SELECT_EVENTS => 12,
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
    // The Set* requests the backend's keyboard description implements (a
    // port of Xorg's handler): its events, then its error.
    let ancient = crate::core_loop::xkb_select::xkb_ancient(state, client_id);
    let set_outcome = {
        let atoms = &state.atoms;
        let atom_name = |atom: u32| atoms.name(x11::AtomId(atom)).map(str::to_owned);
        backend.xkb_set(minor, body, ancient, &atom_name)
    };
    if let Some(outcome) = set_outcome {
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
    // keyboard group and broadcasts XkbStateNotify to subscribed clients.
    // LatchLockState is a void request (no reply), so this runs in the
    // pre-proxy region; the backend's xkb_proxy minor-5 returns None.
    if minor == 5
        && let Some(requested_group) = crate::core_loop::xkb_layout::parse_latch_lock_group(body)
    {
        let old_group = backend.current_group();
        backend.set_locked_group(requested_group);
        let group = backend.current_group();
        if group == old_group {
            return Ok(RequestOutcome::Handled);
        }
        let base = backend.xkb_info().map_or(0, |(_maj, ev, _err)| ev);
        let (eff_mods, base_mods, latched_mods, locked_mods) = backend.current_xkb_mods();
        let subs = crate::core_loop::xkb_layout::subscribers(state, 0x0004);
        let notify = x11::XkbStateNotify {
            device_id: 1,
            mods: eff_mods,
            base_mods,
            latched_mods,
            locked_mods,
            group,
            locked_group: group,
            changed: 0x0090, // XkbGroupStateMask | XkbGroupLockMask
            keycode: 0,
            event_type: 0, // caused by an XKB request, not a key
            request_major: 136,
            request_minor: 5,
        };
        let _dropped = fanout_event_to_clients(state, &subs, |buf, seq, order| {
            let _ = x11::write_xkb_state_notify(buf, order, seq, base, notify);
        });
        state.last_xkb_mods = eff_mods;
        // Dedup anchor: record the group we just announced so the
        // compiled-`grp:`-key-action path in `key_event_fanout_to_state`
        // (Driver 2) doesn't re-emit a redundant StateNotify on the
        // next key. (Driver 2 sets the same field after its own emit.)
        state.last_xkb_group = group;
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
        // GetControls: the per-key repeat is the core keyboard feedback's
        // (Xorg keeps XKB `per_key_repeat` and `autoRepeats` in sync), which
        // lives here, not in the backend.
        if minor == 6 && bytes.len() >= 92 {
            bytes[60..92].copy_from_slice(&state.keyboard_control.auto_repeats);
        }
        let Some(client) = state.clients.get_mut(&client_id.0) else {
            return Ok(RequestOutcome::Handled);
        };
        return Ok(write_to_client(client, client_id, &bytes));
    }
    Ok(RequestOutcome::Handled)
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
