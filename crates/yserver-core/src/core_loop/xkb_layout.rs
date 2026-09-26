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
    backend::{Backend, KeyboardMappingChange},
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
    // n_types = 4 in phase A; a later task (C2) changes this to the
    // backend's derived type count once GetMap publishes the real table.
    send_xkb_map_notify(
        state,
        xkb_event_base,
        x11::XkbMapNotify::whole_keymap(1, min_kc, max_kc, 4),
    );
    log::info!(
        "xkb: applied layout '{}' (variant '{}'); notified {} clients",
        names.layout,
        names.variant,
        all.len()
    );
}

/// Send an `XkbMapNotify` carrying `notify` to every client whose map
/// details include one of its changes (Xorg `XkbSendMapNotify`:
/// `mapNotifyMask & changed`). Returns the recipients.
pub(crate) fn send_xkb_map_notify(
    state: &mut ServerState,
    xkb_event_base: u8,
    notify: x11::XkbMapNotify,
) -> Vec<ClientId> {
    let recipients = crate::core_loop::xkb_select::map_notify_recipients(state, notify.changed);
    let _dropped = fanout_event_to_clients(state, &recipients, |buf, seq, order| {
        let _ = x11::write_xkb_map_notify(buf, order, seq, xkb_event_base, notify);
    });
    recipients
}

/// yserver's one XKB keyboard, as every XKB reply and event names it.
const XKB_DEVICE_ID: u8 = 1;
/// `XkbPerKeyRepeatMask` in `XkbControlsNotify.changedControls`.
const XKB_PER_KEY_REPEAT_MASK: u32 = 1 << 30;
/// Core `X_ChangeKeyboardMapping`: the cause Xorg's `XkbApplyMappingChange`
/// stamps on the events of both the core and the XI request.
pub(crate) const X_CHANGE_KEYBOARD_MAPPING: u8 = 100;
/// Core `X_SetModifierMapping`: the cause of a modifier-map change's events,
/// for the core and the XI request alike (`XkbApplyMappingChange`).
pub(crate) const X_SET_MODIFIER_MAPPING: u8 = 118;

/// Seed the core per-key auto-repeat from the backend's keymap, as Xorg's
/// `XkbFinishInit` copies the keymap's `per_key_repeat` into the keyboard
/// feedback's `autoRepeats` (xkb/xkbInit.c). Without a keymap the Xorg
/// `DEFAULT_AUTOREPEATS` stay.
pub fn seed_keyboard_auto_repeats(state: &mut ServerState, backend: &dyn Backend) {
    if let Some(bits) = backend.keymap_auto_repeats() {
        state.keyboard_control.auto_repeats = bits;
    }
}

/// The notifications of a keymap mapping change that follow its
/// `XkbMapNotify` and core `MappingNotify`, in Xorg's order
/// (`XkbSendNotification`): `XkbControlsNotify` for a per-key repeat change
/// (`cause` = the request's major and minor opcode), then
/// `XkbIndicatorMapNotify` for the indicator maps a virtual modifier change
/// altered.
pub(crate) fn send_keyboard_mapping_followups(
    state: &mut ServerState,
    xkb_event_base: u8,
    change: &KeyboardMappingChange,
    cause: (u8, u8),
) {
    apply_keyboard_mapping_repeats(state, xkb_event_base, change, cause);
    if change.indicator_map_changed != 0 {
        send_indicator_notify(
            state,
            xkb_event_base,
            x11::XkbIndicatorNotifyKind::Map,
            change.indicator_state,
            change.indicator_map_changed,
        );
    }
    if change.compat_changed_groups != 0 {
        send_xkb_compat_map_notify(
            state,
            xkb_event_base,
            x11::XkbCompatMapNotify {
                device_id: XKB_DEVICE_ID,
                changed_groups: change.compat_changed_groups,
                first_si: 0,
                n_si: 0,
                n_total_si: change.compat_total_si,
            },
        );
    }
}

/// `XkbSendCompatMapNotify`: to every XKB-initialised client with any
/// compat map interest (`compatNotifyMask` ≠ 0).
pub(crate) fn send_xkb_compat_map_notify(
    state: &mut ServerState,
    xkb_event_base: u8,
    notify: x11::XkbCompatMapNotify,
) {
    let recipients = crate::core_loop::xkb_select::interest_recipients(state, |i| i.compat != 0);
    let _dropped = fanout_event_to_clients(state, &recipients, |buf, seq, order| {
        let _ = x11::write_xkb_compat_map_notify(buf, order, seq, xkb_event_base, notify);
    });
}

/// `XkbXI_IndicatorMapsMask` / `XkbXI_IndicatorStateMask` /
/// `XkbXI_AllFeaturesMask` (ExtensionDeviceNotify reason and features).
const XI_INDICATOR_MAPS_MASK: u16 = 1 << 3;
const XI_INDICATOR_STATE_MASK: u16 = 1 << 4;
const XI_ALL_FEATURES_MASK: u16 = 0x001f;

/// The notifications of new indicator maps, in Xorg's order
/// (`XkbApplyLedMapChanges` → `XkbFlushLedEvents`: `XkbSendNotification`'s
/// IndicatorMapNotify and, when the maps turned indicators on or off,
/// IndicatorStateNotify; then `XkbSendExtensionDeviceNotify` with reason
/// IndicatorMaps, plus IndicatorState when the state changed, for the core
/// keyboard's default LED feedback: class KbdFeedbackClass (0), id 0).
pub(crate) fn send_indicator_maps_change(
    state: &mut ServerState,
    xkb_event_base: u8,
    change: &crate::backend::XkbIndicatorMapsChange,
) {
    send_indicator_notify(
        state,
        xkb_event_base,
        x11::XkbIndicatorNotifyKind::Map,
        change.state,
        change.maps_changed,
    );
    let mut reason = XI_INDICATOR_MAPS_MASK;
    if change.state_changed != 0 {
        send_indicator_notify(
            state,
            xkb_event_base,
            x11::XkbIndicatorNotifyKind::State,
            change.state,
            change.state_changed,
        );
        reason |= XI_INDICATOR_STATE_MASK;
    }
    let notify = x11::XkbExtensionDeviceNotify {
        device_id: XKB_DEVICE_ID,
        reason,
        led_class: 0,
        led_id: 0,
        leds_defined: change.leds_defined,
        led_state: change.state,
        first_btn: 0,
        n_btns: 0,
        supported: XI_ALL_FEATURES_MASK,
        unsupported: 0,
    };
    let recipients = crate::core_loop::xkb_select::interest_recipients(state, |i| {
        i.extension_device & reason != 0
    });
    let _dropped = fanout_event_to_clients(state, &recipients, |buf, seq, order| {
        let _ = x11::write_xkb_extension_device_notify(buf, order, seq, xkb_event_base, notify);
    });
}

/// Apply the per-key auto-repeat a mapping change re-derived for its keys
/// (Xorg `XkbUpdateActions`: `per_key_repeat` is copied from the core bits,
/// the changed keys are recomputed, and the result is copied back), then
/// send `XkbControlsNotify(PerKeyRepeat)` when a bit actually changed, with
/// `cause` (the request's major and minor opcode) as its cause.
pub(crate) fn apply_keyboard_mapping_repeats(
    state: &mut ServerState,
    xkb_event_base: u8,
    change: &KeyboardMappingChange,
    cause: (u8, u8),
) {
    if !apply_repeats_to_core(state, &change.repeats) {
        return;
    }
    let notify = x11::XkbControlsNotify {
        device_id: XKB_DEVICE_ID,
        num_groups: change.num_groups,
        changed_controls: XKB_PER_KEY_REPEAT_MASK,
        enabled_controls: change.enabled_controls,
        enabled_control_changes: 0,
        keycode: 0,
        event_type: 0,
        request_major: cause.0,
        request_minor: cause.1,
    };
    let recipients = crate::core_loop::xkb_select::interest_recipients(state, |i| {
        i.controls & XKB_PER_KEY_REPEAT_MASK != 0
    });
    let _dropped = fanout_event_to_clients(state, &recipients, |buf, seq, order| {
        let _ = x11::write_xkb_controls_notify(buf, order, seq, xkb_event_base, notify);
    });
}

/// Copy re-derived per-key repeat bits into the core keyboard feedback
/// (the end of Xorg's `XkbUpdateActions`); whether a bit changed. Keys a
/// client set with `ChangeKeyboardControl` keep their bit (Xorg marks them
/// `XkbExplicitAutoRepeatMask`).
pub(crate) fn apply_repeats_to_core(state: &mut ServerState, repeats: &[(u8, bool)]) -> bool {
    let control = &mut state.keyboard_control;
    let mut changed = false;
    for &(kc, repeats) in repeats {
        let (i, bit) = (usize::from(kc >> 3), 1u8 << (kc & 7));
        if control.auto_repeats_explicit[i] & bit != 0 {
            continue;
        }
        let old = control.auto_repeats[i];
        if repeats {
            control.auto_repeats[i] |= bit;
        } else {
            control.auto_repeats[i] &= !bit;
        }
        changed |= control.auto_repeats[i] != old;
    }
    changed
}

/// Send `XkbIndicatorMapNotify` / `XkbIndicatorStateNotify` for the
/// indicators in `changed` (`lit` = the indicators on) to the XKB-initialised
/// clients whose indicator map / state interest includes one of them
/// (`XkbSendIndicatorNotify`: `iMapNotifyMask` / `iStateNotifyMask`).
fn send_indicator_notify(
    state: &mut ServerState,
    xkb_event_base: u8,
    kind: x11::XkbIndicatorNotifyKind,
    lit: u32,
    changed: u32,
) {
    let notify = x11::XkbIndicatorNotify {
        kind,
        device_id: XKB_DEVICE_ID,
        state: lit,
        changed,
    };
    let recipients = crate::core_loop::xkb_select::interest_recipients(state, |i| match kind {
        x11::XkbIndicatorNotifyKind::Map => i.indicator_map & changed != 0,
        x11::XkbIndicatorNotifyKind::State => i.indicator_state & changed != 0,
    });
    let _dropped = fanout_event_to_clients(state, &recipients, |buf, seq, order| {
        let _ = x11::write_xkb_indicator_notify(buf, order, seq, xkb_event_base, notify);
    });
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

pub(crate) use crate::core_loop::xkb_select::subscribers;

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

    fn install_client(state: &mut ServerState, id: u32) -> std::os::unix::net::UnixStream {
        use std::{
            collections::{HashMap, HashSet, VecDeque},
            sync::{Arc, Mutex, atomic::AtomicU16},
        };
        let (server_side, peer) = std::os::unix::net::UnixStream::pair().unwrap();
        state.clients.insert(
            id,
            crate::server::ClientState {
                writer: Arc::new(Mutex::new(crate::transport::Transport::Unix(server_side))),
                byte_order: x11::ClientByteOrder::LittleEndian,
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
                focused_window: ROOT_WINDOW,
                reader_control: None,
                is_local: true,
                fd_passing: true,
            },
        );
        peer
    }

    fn read_available(peer: &mut std::os::unix::net::UnixStream) -> Vec<u8> {
        use std::io::Read;
        peer.set_nonblocking(true).unwrap();
        let mut out = Vec::new();
        let mut buf = [0u8; 256];
        loop {
            match peer.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("read failed: {e}"),
            }
        }
        out
    }

    /// The MapNotify helper reaches exactly the clients that selected
    /// XkbMapNotify (under any device spec), each getting the encoder's
    /// bytes for the given fields.
    #[test]
    fn send_xkb_map_notify_reaches_map_notify_subscribers_only() {
        let mut state = ServerState::new();
        let mut map_sub = install_client(&mut state, 5);
        let mut state_sub = install_client(&mut state, 6);
        let mut both_sub = install_client(&mut state, 7);
        crate::core_loop::xkb_select::xkb_select_events(&mut state, 5, 0x0100, 0x0002);
        crate::core_loop::xkb_select::xkb_select_events(&mut state, 6, 0x0100, 0x0004);
        crate::core_loop::xkb_select::xkb_select_events(&mut state, 7, 0x0003, 0x0007);

        let notify = x11::XkbMapNotify {
            device_id: 3,
            changed: 0x0006, // KeySyms|ModifierMap
            min_keycode: 8,
            max_keycode: 255,
            first_key_sym: 38,
            n_key_syms: 1,
            first_mod_map_key: 38,
            n_mod_map_keys: 1,
            ..x11::XkbMapNotify::default()
        };
        let sent = send_xkb_map_notify(&mut state, 85, notify);
        assert_eq!(sent, vec![ClientId(5), ClientId(7)]);

        let mut expected = Vec::new();
        x11::write_xkb_map_notify(
            &mut expected,
            x11::ClientByteOrder::LittleEndian,
            x11::SequenceNumber(0),
            85,
            notify,
        )
        .unwrap();
        assert_eq!(read_available(&mut map_sub), expected);
        assert_eq!(read_available(&mut both_sub), expected);
        assert!(
            read_available(&mut state_sub).is_empty(),
            "StateNotify-only client"
        );
    }

    /// The `_XKB_RULES_NAMES` reload announces a whole-keymap MapNotify
    /// through the helper.
    #[test]
    fn apply_rules_names_change_sends_whole_keymap_map_notify() {
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 5);
        crate::core_loop::xkb_select::xkb_select_events(&mut state, 5, 0x0100, 0x0002);
        let mut backend = RecordingBackend::new().with_keymap_rmlvo_result((8, 255));

        apply_rules_names_change(&mut state, &mut backend, b"evdev\0pc105\0de\0\0\0");

        let bytes = read_available(&mut peer);
        let map_notify = bytes
            .chunks_exact(32)
            .find(|ev| ev[1] == 1 && ev[0] != 34)
            .expect("an XkbMapNotify");
        assert_eq!(&map_notify[10..12], &0x0007u16.to_le_bytes(), "changed");
        assert_eq!(map_notify[12..14], [8, 255], "min/max keycode");
        assert_eq!(map_notify[15], 4, "nTypes");
        assert_eq!(map_notify[16..18], [8, 248], "keysym range");
        assert_eq!(map_notify[24..26], [8, 248], "modmap range");
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

    /// The real client handshake: SelectEvents(selectAll 0x07), then a
    /// StateNotify detail refinement: the first request's selections stay.
    #[test]
    fn xkb_select_events_two_request_handshake_keeps_statenotify() {
        use crate::core_loop::xkb_select::{select_events, subscribers, use_extension};
        let mut state = ServerState::new();
        let _peer = install_client(&mut state, 5);
        assert!(use_extension(&mut state, ClientId(5), &[1, 0, 0, 0]));
        let req = |affect: u16, all: u16, rest: &[u8]| {
            let mut b = Vec::new();
            for v in [0x100u16, affect, 0, all, 0xff, 0xff] {
                b.extend_from_slice(&v.to_le_bytes());
            }
            b.extend_from_slice(rest);
            b
        };
        select_events(&mut state, ClientId(5), &req(0x0007, 0x0007, &[])).unwrap();
        // StateNotify: affect GroupState|ModifierState, value the same.
        select_events(
            &mut state,
            ClientId(5),
            &req(0x0004, 0, &[0x11, 0, 0x11, 0]),
        )
        .unwrap();
        for bit in [0x0001, 0x0002, 0x0004] {
            assert_eq!(
                subscribers(&state, bit),
                vec![ClientId(5)],
                "event bit {bit:#x}"
            );
        }
        assert_eq!(state.xkb_interests[&(5, 0x100)].state, 0xffff);
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
