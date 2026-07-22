//! `_XKB_RULES_NAMES` runtime-layout hook.
//!
//! When a client (libxklavier / setxkbmap) writes the
//! `_XKB_RULES_NAMES` root-window property, we parse the RMLVO,
//! recompile the keymap in the backend, then notify clients so they
//! re-query the new layout: core `MappingNotify` to ALL clients
//! (Keyboard + Modifier) plus `XkbNewKeyboardNotify` / `XkbMapNotify`
//! to XKB-subscribed clients. This mirrors Xorg's `GetKbdByName`
//! full-reload notification path.

use crate::{
    backend::Backend,
    core_loop::fanout::fanout_event_to_clients,
    properties::{PropertyFormat, PropertyValue},
    resources::ROOT_WINDOW,
    server::ServerState,
};
use yserver_protocol::x11::{self, AtomId, ClientId};

/// Parsed `_XKB_RULES_NAMES` property value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RulesNames {
    pub rules: String,
    pub model: String,
    pub layout: String,
    pub variant: String,
    pub options: Option<String>,
}

/// Parse the NUL-separated `_XKB_RULES_NAMES` value
/// (`rules\0model\0layout\0variant\0options`). Returns `None` if fewer
/// than the 3 load-bearing fields (rules, model, layout) are present, or
/// if `layout` is empty.
#[must_use]
pub fn parse_rules_names(bytes: &[u8]) -> Option<RulesNames> {
    let s = std::str::from_utf8(bytes).ok()?;
    let mut it = s.split('\0');
    let rules = it.next()?.to_string();
    let model = it.next()?.to_string();
    let layout = it.next()?.to_string();
    if layout.is_empty() {
        return None;
    }
    let variant = it.next().unwrap_or("").to_string();
    let options = match it.next() {
        Some(o) if !o.is_empty() => Some(o.to_string()),
        _ => None,
    };
    Some(RulesNames {
        rules,
        model,
        layout,
        variant,
        options,
    })
}

/// Predefined X11 atom `XA_STRING` — the type of `_XKB_RULES_NAMES`.
const XA_STRING: u32 = 31;

/// Publish (or refresh) the `_XKB_RULES_NAMES` root-window property from
/// the backend's active RMLVO, the way Xorg seeds it at init and keeps it
/// current. `setxkbmap` reads this (`XkbRF_GetNamesProp`) to learn the
/// current rules before applying a new layout; without it, it falls back
/// to its compiled-in default `rules='base'` and the keyboard load fails.
///
/// The value is five NUL-terminated fields in the exact field order the
/// existing [`parse_rules_names`] decodes:
/// `rules\0model\0layout\0variant\0options\0` (each field, including a
/// possibly-empty options field, is NUL-terminated).
///
/// No-op when the backend has no real keymap (`current_xkb_rules_names`
/// returns `None`).
///
/// This writes through [`crate::resources::ResourceTable::set_window_property`]
/// directly, NOT the client `ChangeProperty` request handler, so it does
/// NOT re-enter the `apply_rules_names_change` recompile hook (that hook
/// lives inside `handle_change_property` and only runs for client
/// requests) — there is no recompile loop.
pub(crate) fn publish_xkb_rules_names(state: &mut ServerState, backend: &dyn Backend) {
    let Some(names) = backend.current_xkb_rules_names() else {
        return;
    };
    let mut data = Vec::new();
    for field in &names {
        data.extend_from_slice(field.as_bytes());
        data.push(0);
    }
    let atom = state.atoms.intern("_XKB_RULES_NAMES", false);
    state.resources.set_window_property(
        ROOT_WINDOW,
        atom,
        PropertyValue {
            r#type: AtomId(XA_STRING),
            format: PropertyFormat::F8,
            data,
        },
    );
}

/// Apply a `_XKB_RULES_NAMES` change: recompile the keymap in the
/// backend, then notify clients (core MappingNotify to all; XKB
/// New-Keyboard / Map notify to subscribed clients) so already-running
/// clients re-query the new layout. No-op if the RMLVO is unchanged or
/// fails to compile.
pub fn apply_rules_names_change(state: &mut ServerState, backend: &mut dyn Backend, value: &[u8]) {
    let Some(names) = parse_rules_names(value) else {
        return;
    };
    let Some((min_kc, max_kc)) = backend.set_keymap_rmlvo(
        &names.rules,
        &names.model,
        &names.layout,
        &names.variant,
        names.options.as_deref(),
    ) else {
        return; // unchanged or failed to compile
    };
    let xkb_event_base = backend.xkb_info().map_or(0, |(_maj, ev, _err)| ev);
    let count = max_kc.saturating_sub(min_kc).saturating_add(1);

    // A full RMLVO reload replaces the whole key-symbol table in Xorg
    // (XkbGetKeyboardByName), so any keycodes previously overridden by
    // `ChangeKeyboardMapping` must NOT keep shadowing the new backend
    // keymap — otherwise those keycodes stay stuck on the pre-switch
    // layout forever, while every other key correctly reflects the new
    // one. Without this, `fetch_merged_keymap` (GetKeyboardMapping)
    // keeps layering the stale rows on top of the freshly recompiled
    // keymap for any keycode an app ever remapped.
    state.keymap_overrides.clear();

    // 1. Core MappingNotify(Keyboard) + MappingNotify(Modifier) to ALL
    //    clients — mirrors Xorg's XkbSendLegacyMapNotify on a keymap reload.
    let all: Vec<ClientId> = state.clients.keys().map(|id| ClientId(*id)).collect();
    let _dropped = fanout_event_to_clients(state, &all, |buf, seq, order| {
        let _ = x11::write_mapping_notify_event(buf, order, seq, 1, min_kc, count);
    });
    let _dropped = fanout_event_to_clients(state, &all, |buf, seq, order| {
        let _ = x11::write_mapping_notify_event(buf, order, seq, 0, 0, 0);
    });

    // 2. XKB events only to clients that selected them (XkbSelectEvents).
    let nkn = subscribers(state, 0x0001); // XkbNewKeyboardNotifyMask
    let _dropped = fanout_event_to_clients(state, &nkn, |buf, seq, order| {
        let _ = x11::write_xkb_new_keyboard_notify(
            buf,
            order,
            seq,
            xkb_event_base,
            1,
            min_kc,
            max_kc,
            min_kc,
            max_kc,
            0,      // requestMajor — internal rules-names change, no request
            0,      // requestMinor
            0x0001, // changed = XkbNKN_KeycodesMask
        );
    });
    let mapn = subscribers(state, 0x0002); // XkbMapNotifyMask
    let _dropped = fanout_event_to_clients(state, &mapn, |buf, seq, order| {
        // n_types = 4 in phase A; a later task (C2) changes this to the
        // backend's derived type count once GetMap publishes the real table.
        let _ = x11::write_xkb_map_notify(buf, order, seq, xkb_event_base, 1, min_kc, max_kc, 4);
    });
    log::info!(
        "xkb: applied layout '{}' (variant '{}'); notified {} clients",
        names.layout,
        names.variant,
        all.len()
    );
}

/// Merge an `XkbSelectEvents` request into the stored per-(client, device)
/// mask. `affect_which` names the events this request touches; `clear` names
/// (within `affect_which`) the ones to deselect. Events in `affect_which` and
/// not in `clear` become selected; events outside `affect_which` keep their
/// prior selection. This covers BOTH the `selectAll` path (first request) and
/// the detail-refinement path (second request) of the real client handshake.
#[must_use]
pub fn xkb_select_merge(old: u16, affect_which: u16, clear: u16) -> u16 {
    (old & !affect_which) | (affect_which & !clear)
}

/// Decode the group-lock target from an `xkbLatchLockStateReq` body (the bytes
/// after the 4-byte request header). Returns `Some(groupLock)` iff the
/// `lockGroup` BOOL is set, else `None` (a non-group LatchLockState).
///
/// Body layout: deviceSpec@0..2, affectModLocks@2, modLocks@3,
/// lockGroup(BOOL)@4, groupLock@5, affectModLatches@6, modLatches@7, pad@8,
/// latchGroup@9, groupLatch@10..12.
#[must_use]
pub fn parse_latch_lock_group(body: &[u8]) -> Option<u8> {
    if body.len() < 6 {
        return None;
    }
    if body[4] != 0 { Some(body[5]) } else { None }
}

/// Clients whose XkbSelectEvents top mask (any device-spec) includes `bit`.
/// NOTE: the stored value is a `u16` today; a later task (D2b) changes it to
/// a struct, after which this reads `m.top & bit`.
pub(crate) fn subscribers(state: &ServerState, bit: u16) -> Vec<ClientId> {
    let mut out: Vec<ClientId> = state
        .xkb_select_event_masks
        .iter()
        .filter(|((_cid, _dev), mask)| **mask & bit != 0)
        .map(|((cid, _dev), _)| ClientId(*cid))
        .collect();
    // A client may select under multiple device-specs; collapse to one id.
    // (fanout_event_to_clients also dedups internally — this keeps the
    // returned list clean for the caller's log count, not load-bearing.)
    out.sort_by_key(|c| c.0);
    out.dedup_by_key(|c| c.0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{backend::recording::RecordingBackend, server::ServerState};

    fn names5(a: &[&str; 5]) -> [String; 5] {
        std::array::from_fn(|i| a[i].to_string())
    }

    #[test]
    fn publish_writes_nul_separated_string_property() {
        let mut state = ServerState::new();
        let backend = RecordingBackend::new().with_xkb_rules_names(names5(&[
            "evdev",
            "pc105",
            "us,de",
            ",",
            "grp:alt_shift_toggle",
        ]));

        // RED precondition: the property is absent before the call.
        let atom_before = state.atoms.intern("_XKB_RULES_NAMES", false);
        assert!(
            state
                .resources
                .window_property(ROOT_WINDOW, atom_before)
                .is_none(),
            "_XKB_RULES_NAMES must be absent before publish"
        );

        publish_xkb_rules_names(&mut state, &backend);

        let atom = state.atoms.intern("_XKB_RULES_NAMES", false);
        let prop = state
            .resources
            .window_property(ROOT_WINDOW, atom)
            .expect("property published");
        assert_eq!(prop.r#type, AtomId(31), "type = XA_STRING");
        assert_eq!(prop.format, PropertyFormat::F8, "format 8");
        assert_eq!(
            prop.data.as_slice(),
            b"evdev\0pc105\0us,de\0,\0grp:alt_shift_toggle\0",
            "five NUL-terminated fields, including trailing NUL"
        );
    }

    #[test]
    fn publish_round_trips_through_parser() {
        let mut state = ServerState::new();
        let rmlvo = names5(&["evdev", "pc105", "us,de", ",", "grp:alt_shift_toggle"]);
        let backend = RecordingBackend::new().with_xkb_rules_names(rmlvo);

        publish_xkb_rules_names(&mut state, &backend);

        let atom = state.atoms.intern("_XKB_RULES_NAMES", false);
        let bytes = state
            .resources
            .window_property(ROOT_WINDOW, atom)
            .expect("published")
            .data
            .clone();

        // Feed the produced bytes back through the existing inverse:
        // the parser must recover the same RMLVO.
        let parsed = parse_rules_names(&bytes).expect("re-parses");
        assert_eq!(parsed.rules, "evdev");
        assert_eq!(parsed.model, "pc105");
        assert_eq!(parsed.layout, "us,de");
        assert_eq!(parsed.variant, ",");
        assert_eq!(parsed.options.as_deref(), Some("grp:alt_shift_toggle"));
    }

    #[test]
    fn publish_is_noop_without_keymap() {
        let mut state = ServerState::new();
        let backend = RecordingBackend::new(); // no RMLVO → current_xkb_rules_names == None

        publish_xkb_rules_names(&mut state, &backend);

        let atom = state.atoms.intern("_XKB_RULES_NAMES", false);
        assert!(
            state.resources.window_property(ROOT_WINDOW, atom).is_none(),
            "no property published when the backend has no keymap"
        );
    }

    #[test]
    fn apply_rules_names_change_clears_stale_keymap_overrides() {
        // A client (e.g. xmodmap) had previously overridden keycode 38
        // via ChangeKeyboardMapping. Model that directly on `keymap_overrides`
        // rather than going through the request handler — this test only
        // needs to prove the recompile path clears it.
        let mut state = ServerState::new();
        state.keymap_overrides.insert(38, vec![0x0071]); // stale 'q' row
        assert!(
            !state.keymap_overrides.is_empty(),
            "precondition: an override is staged before the layout switch"
        );

        let mut backend = RecordingBackend::new().with_keymap_rmlvo_result((8, 255));

        apply_rules_names_change(&mut state, &mut backend, b"evdev\0pc105\0us\0dvorak\0\0");

        assert!(
            state.keymap_overrides.is_empty(),
            "a full RMLVO reload must drop stale ChangeKeyboardMapping rows, \
             the way Xorg's XkbGetKeyboardByName replaces the whole key-symbol \
             table — otherwise keycode 38 stays stuck on the pre-switch layout \
             forever while every other key correctly reflects the new one"
        );
    }

    #[test]
    fn parse_rules_names_full() {
        let bytes = b"evdev\0pc105\0be\0\0\0";
        let r = parse_rules_names(bytes).expect("parses");
        assert_eq!(r.rules, "evdev");
        assert_eq!(r.model, "pc105");
        assert_eq!(r.layout, "be");
        assert_eq!(r.variant, "");
        assert_eq!(r.options, None);
    }

    #[test]
    fn parse_rules_names_with_variant_and_options() {
        let bytes = b"evdev\0pc105\0us\0intl\0ctrl:nocaps\0";
        let r = parse_rules_names(bytes).expect("parses");
        assert_eq!(r.layout, "us");
        assert_eq!(r.variant, "intl");
        assert_eq!(r.options.as_deref(), Some("ctrl:nocaps"));
    }

    #[test]
    fn parse_rules_names_too_few_fields_is_none() {
        assert!(parse_rules_names(b"evdev\0pc105\0").is_none());
    }

    #[test]
    fn xkb_select_events_two_request_handshake_keeps_statenotify() {
        // Req1: affectWhich=0x07, clear=0, (selectAll handled by affectWhich&!clear)
        let m1 = xkb_select_merge(0x0000, 0x0007, 0x0000);
        assert_eq!(m1, 0x0007);
        // Req2: affectWhich=0x04 (StateNotify), clear=0 — must NOT drop the prior 0x07
        let m2 = xkb_select_merge(m1, 0x0004, 0x0000);
        assert_eq!(
            m2 & 0x04,
            0x04,
            "StateNotify selection survives the detail-refine request"
        );
        assert_eq!(m2, 0x0007, "other selections preserved");
    }

    #[test]
    fn parse_latch_lock_group_decodes_capture() {
        // deviceSpec=0x0100, affectModLocks=0, modLocks=0, lockGroup=1, groupLock=1, ...
        let body = [
            0x00, 0x01, 0x00, 0x00, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        assert_eq!(parse_latch_lock_group(&body), Some(1));
        // lockGroup=0 -> None (a non-group LatchLockState, e.g. mod latch only)
        let body2 = [
            0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        assert_eq!(parse_latch_lock_group(&body2), None);
    }
}
