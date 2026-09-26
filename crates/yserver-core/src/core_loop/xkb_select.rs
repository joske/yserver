//! Xorg's per-client XKB state and event selection (#171 phase 4c):
//! XkbUseExtension's "initialised" flag (every other XKB request answers
//! BadAccess without it), a port of `ProcXkbSelectEvents` (xkb/xkb.c) into
//! the per-client and per-device detail masks, and the recipient filters
//! of Xorg's XKB event senders (xkb/xkbEvents.c), including
//! `XkbSendLegacyMapNotify`'s core MappingNotify.

use crate::{
    core_loop::fanout::fanout_event_to_clients,
    server::{ServerState, XkbClientState, XkbInterest},
};
use yserver_protocol::x11::{self, ClientId, error};

/// `XkbSelectEvents` event indices (`XkbNewKeyboardNotify` .. ).
const NEW_KEYBOARD_NOTIFY: u32 = 0;
const MAP_NOTIFY: u32 = 1;
const STATE_NOTIFY: u32 = 2;
const CONTROLS_NOTIFY: u32 = 3;
const INDICATOR_STATE_NOTIFY: u32 = 4;
const INDICATOR_MAP_NOTIFY: u32 = 5;
const NAMES_NOTIFY: u32 = 6;
const COMPAT_MAP_NOTIFY: u32 = 7;
const BELL_NOTIFY: u32 = 8;
const ACTION_MESSAGE: u32 = 9;
const ACCESS_X_NOTIFY: u32 = 10;
const EXTENSION_DEVICE_NOTIFY: u32 = 11;
/// `XkbMapNotifyMask`.
const MAP_NOTIFY_MASK: u16 = 1 << MAP_NOTIFY;

/// `XkbKeySymsMask` / `XkbModifierMapMask` (MapNotify details).
const KEY_SYMS_MASK: u16 = 1 << 1;
const MODIFIER_MAP_MASK: u16 = 1 << 2;
/// `XkbNKN_KeycodesMask`.
const NKN_KEYCODES_MASK: u16 = 1 << 0;

/// The keycode range every client knows (`clients[i]->minKC/maxKC`): the
/// connection setup's, which yserver never changes.
const CLIENT_MIN_KEYCODE: u8 = 8;
const CLIENT_MAX_KEYCODE: u8 = 255;

/// `_XkbErrCode2`.
fn err_code2(a: u32, b: u32) -> u32 {
    (a << 24) | (b & 0x00ff_ffff)
}

/// Whether `client` called XkbUseExtension successfully.
#[must_use]
pub fn xkb_initialized(state: &ServerState, client: ClientId) -> bool {
    state
        .xkb_clients
        .get(&client.0)
        .is_some_and(|c| c.initialized)
}

/// Whether `client`'s XkbUseExtension asked for version 0.65
/// (`_XkbClientIsAncient`).
#[must_use]
pub fn xkb_ancient(state: &ServerState, client: ClientId) -> bool {
    state.xkb_clients.get(&client.0).is_some_and(|c| c.ancient)
}

/// Port of `ProcXkbUseExtension`'s bookkeeping: whether the requested
/// version is supported (1.x, or the pre-release 0.65), and if so the client
/// becomes XKB-initialised (the first time only). Body: wantedMajor(2)
/// wantedMinor(2).
pub fn use_extension(state: &mut ServerState, client: ClientId, body: &[u8]) -> bool {
    let b = |i: usize| body.get(i).copied().unwrap_or(0);
    let major = u16::from_le_bytes([b(0), b(1)]);
    let minor = u16::from_le_bytes([b(2), b(3)]);
    let supported = major == 1 || (major == 0 && minor == 65);
    let c = state.xkb_clients.entry(client.0).or_default();
    if supported && !c.initialized {
        c.initialized = true;
        c.ancient = major == 0;
    }
    supported
}

/// A detail mask of the XkbSelectEvents request, as its wire size.
enum Detail<'a> {
    U8(&'a mut u8),
    U16(&'a mut u16),
    U32(&'a mut u32),
}

/// Port of `ProcXkbSelectEvents` after its size and BadAccess checks: the
/// map details (`affectMap`/`map`) into the client's `mapNotifyMask`, then
/// each other event of `affectWhich` cleared, selected in full
/// (`selectAll`) or refined by its affect/value detail pair. Like Xorg it
/// changes the masks as it goes, so an error leaves the earlier events
/// changed. Body: deviceSpec(2) affectWhich(2) clear(2) selectAll(2)
/// affectMap(2) map(2), then the detail pairs. The device isn't looked up
/// (yserver has one keyboard; no XKB request validates the device spec).
///
/// # Errors
///
/// `(code, errorValue)`: BadValue for an event index past
/// ExtensionDeviceNotify or an illegal detail bit, BadMatch for a value
/// bit outside its affect mask, BadLength for missing or extra detail
/// bytes.
pub fn select_events(
    state: &mut ServerState,
    client: ClientId,
    body: &[u8],
) -> Result<(), (u8, u32)> {
    let w = |i: usize| u16::from_le_bytes([body[i], body[i + 1]]);
    let (device_spec, affect_which, clear, select_all) = (w(0), w(2), w(4), w(6));
    let (affect_map, map) = (w(8), w(10));
    {
        let c = state.xkb_clients.entry(client.0).or_default();
        if affect_which & MAP_NOTIFY_MASK != 0 && affect_map != 0 {
            c.map_notify_mask &= !affect_map;
            c.map_notify_mask |= affect_map & map;
        }
    }
    if affect_which & !MAP_NOTIFY_MASK == 0 {
        return Ok(());
    }
    let mut from = 12usize;
    let mut data_left = body.len() - 12;
    let mut mask_left = affect_which & !MAP_NOTIFY_MASK;
    let mut ndx = 0u32;
    while mask_left != 0 {
        let bit = 1u16 << ndx;
        if bit & mask_left == 0 {
            ndx += 1;
            continue;
        }
        mask_left &= !bit;
        let (mut xkb_client, mut interest) = (
            *state.xkb_clients.entry(client.0).or_default(),
            *state
                .xkb_interests
                .entry((client.0, device_spec))
                .or_default(),
        );
        let detail = detail_for(ndx, &mut xkb_client, &mut interest);
        let Some((detail, legal)) = detail else {
            return Err((error::BAD_VALUE, err_code2(33, u32::from(bit))));
        };
        let result = if clear & bit != 0 {
            set_detail(detail, false);
            Ok(0)
        } else if select_all & bit != 0 {
            set_detail(detail, true);
            Ok(0)
        } else {
            refine_detail(detail, legal, ndx, &body[from..], data_left)
        };
        state.xkb_clients.insert(client.0, xkb_client);
        state
            .xkb_interests
            .insert((client.0, device_spec), interest);
        let consumed = result?;
        from += consumed;
        data_left -= consumed;
        ndx += 1;
    }
    if data_left > 2 {
        return Err((error::BAD_LENGTH, 0));
    }
    Ok(())
}

/// The detail mask event `ndx` selects into, with its legal bits
/// (`XkbAll*EventsMask`); `None` past ExtensionDeviceNotify.
fn detail_for<'a>(
    ndx: u32,
    client: &'a mut XkbClientState,
    interest: &'a mut XkbInterest,
) -> Option<(Detail<'a>, u32)> {
    Some(match ndx {
        NEW_KEYBOARD_NOTIFY => (Detail::U16(&mut client.new_keyboard_notify_mask), 0x0007),
        STATE_NOTIFY => (Detail::U16(&mut interest.state), 0x3fff),
        CONTROLS_NOTIFY => (Detail::U32(&mut interest.controls), 0xf800_1fff),
        INDICATOR_STATE_NOTIFY => (Detail::U32(&mut interest.indicator_state), 0xffff_ffff),
        INDICATOR_MAP_NOTIFY => (Detail::U32(&mut interest.indicator_map), 0xffff_ffff),
        NAMES_NOTIFY => (Detail::U16(&mut interest.names), 0x3fff),
        COMPAT_MAP_NOTIFY => (Detail::U8(&mut interest.compat), 0x03),
        BELL_NOTIFY => (Detail::U8(&mut interest.bell), 0x01),
        ACTION_MESSAGE => (Detail::U8(&mut interest.action_message), 0x01),
        ACCESS_X_NOTIFY => (Detail::U16(&mut interest.access_x), 0x7f),
        EXTENSION_DEVICE_NOTIFY => (Detail::U16(&mut interest.extension_device), 0x801f),
        _ => return None,
    })
}

fn set_detail(detail: Detail<'_>, all: bool) {
    match detail {
        Detail::U8(m) => *m = if all { u8::MAX } else { 0 },
        Detail::U16(m) => *m = if all { u16::MAX } else { 0 },
        Detail::U32(m) => *m = if all { u32::MAX } else { 0 },
    }
}

/// One affect/value detail pair: `CHK_MASK_MATCH`, `CHK_MASK_LEGAL`, then
/// the affected bits replaced. Returns the bytes consumed (a CARD8 pair
/// takes four, as Xorg reads it).
fn refine_detail(
    detail: Detail<'_>,
    legal: u32,
    ndx: u32,
    from: &[u8],
    data_left: usize,
) -> Result<usize, (u8, u32)> {
    let size = match detail {
        Detail::U8(_) => 1,
        Detail::U16(_) => 2,
        Detail::U32(_) => 4,
    };
    if data_left < size * 2 {
        return Err((error::BAD_LENGTH, 0));
    }
    let read = |at: usize| -> u32 {
        match size {
            1 => u32::from(from[at]),
            2 => u32::from(u16::from_le_bytes([from[at], from[at + 1]])),
            _ => u32::from_le_bytes([from[at], from[at + 1], from[at + 2], from[at + 3]]),
        }
    };
    let (affect, value) = (read(0), read(size));
    if value & !affect != 0 {
        return Err((error::BAD_MATCH, err_code2(ndx, value & !affect)));
    }
    if affect & !legal != 0 {
        return Err((error::BAD_VALUE, err_code2(ndx, affect & !legal)));
    }
    // Every mask is at most as wide as its wire field.
    #[allow(clippy::cast_possible_truncation)]
    match detail {
        Detail::U8(m) => *m = (*m & !(affect as u8)) | ((affect & value) as u8),
        Detail::U16(m) => *m = (*m & !(affect as u16)) | ((affect & value) as u16),
        Detail::U32(m) => *m = (*m & !affect) | (affect & value),
    }
    Ok(if size == 1 { 4 } else { size * 2 })
}

/// XkbUseExtension + `XkbSelectEvents(device_spec, which, which)` as libX11
/// sends it (`selectAll` = `which`, every map detail when `which` has
/// MapNotify): the client selects every detail of the events in `which`.
pub fn xkb_select_events(state: &mut ServerState, client: u32, device_spec: u16, which: u16) {
    let _ = use_extension(state, ClientId(client), &[1, 0, 0, 0]);
    let mut body = Vec::with_capacity(12);
    for v in [device_spec, which, 0, which, 0xff, 0xff] {
        body.extend_from_slice(&v.to_le_bytes());
    }
    if which & MAP_NOTIFY_MASK == 0 {
        body[8] = 0;
    }
    let _ = select_events(state, ClientId(client), &body);
}

fn connected(state: &ServerState, ids: impl Iterator<Item = u32>) -> Vec<ClientId> {
    let mut out: Vec<ClientId> = ids
        .filter(|id| state.clients.contains_key(id))
        .map(ClientId)
        .collect();
    out.sort_by_key(|c| c.0);
    out.dedup_by_key(|c| c.0);
    out
}

/// `XkbSendMapNotify`'s recipients: `clients[i]->mapNotifyMask & changed`.
pub(crate) fn map_notify_recipients(state: &ServerState, changed: u16) -> Vec<ClientId> {
    connected(
        state,
        state
            .xkb_clients
            .iter()
            .filter(|(_, c)| c.map_notify_mask & changed != 0)
            .map(|(id, _)| *id),
    )
}

/// `XkbSendNewKeyboardNotify`'s recipients:
/// `clients[i]->newKeyboardNotifyMask & changed`.
pub(crate) fn new_keyboard_recipients(state: &ServerState, changed: u16) -> Vec<ClientId> {
    connected(
        state,
        state
            .xkb_clients
            .iter()
            .filter(|(_, c)| c.new_keyboard_notify_mask & changed != 0)
            .map(|(id, _)| *id),
    )
}

/// The recipients of an event Xorg delivers through the device's interest
/// list: XKB-initialised clients whose interest (any device spec) passes
/// `wants`.
pub(crate) fn interest_recipients(
    state: &ServerState,
    wants: impl Fn(&XkbInterest) -> bool,
) -> Vec<ClientId> {
    connected(
        state,
        state
            .xkb_interests
            .iter()
            .filter(|((id, _), i)| xkb_initialized(state, ClientId(*id)) && wants(i))
            .map(|((id, _), _)| *id),
    )
}

/// Clients that selected XkbSelectEvents event `bit` with any detail.
pub(crate) fn subscribers(state: &ServerState, bit: u16) -> Vec<ClientId> {
    match u32::from(bit).trailing_zeros() {
        NEW_KEYBOARD_NOTIFY => new_keyboard_recipients(state, u16::MAX),
        MAP_NOTIFY => map_notify_recipients(state, u16::MAX),
        ndx => interest_recipients(state, |i| match ndx {
            STATE_NOTIFY => i.state != 0,
            CONTROLS_NOTIFY => i.controls != 0,
            INDICATOR_STATE_NOTIFY => i.indicator_state != 0,
            INDICATOR_MAP_NOTIFY => i.indicator_map != 0,
            NAMES_NOTIFY => i.names != 0,
            COMPAT_MAP_NOTIFY => i.compat != 0,
            BELL_NOTIFY => i.bell != 0,
            ACTION_MESSAGE => i.action_message != 0,
            ACCESS_X_NOTIFY => i.access_x != 0,
            EXTENSION_DEVICE_NOTIFY => i.extension_device != 0,
            _ => false,
        }),
    }
}

/// Which XKB event a legacy core MappingNotify accompanies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LegacyCause {
    MapNotify,
    NewKeyboardNotify,
}

/// Port of `XkbSendLegacyMapNotify`'s core part: MappingNotify(Keyboard)
/// when the keysyms changed (or, for NewKeyboardNotify, the keycodes) and
/// MappingNotify(Modifier) when the modifier map did (or the keycodes), to
/// every client but an XKB-initialised one that didn't select the change
/// as a MapNotify detail, and never to an XKB-initialised one for a
/// NewKeyboardNotify. Returns whether the keymap / modifier map changed,
/// for the XI companion events.
pub(crate) fn send_legacy_core_map_notify(
    state: &mut ServerState,
    cause: LegacyCause,
    changed: u16,
    first_key: u8,
    num_keys: u8,
) -> (bool, bool) {
    let (keymap_changed, modmap_changed) = match cause {
        LegacyCause::NewKeyboardNotify => {
            let k = changed & NKN_KEYCODES_MASK != 0;
            (k, k)
        }
        LegacyCause::MapNotify => (
            changed & KEY_SYMS_MASK != 0,
            changed & MODIFIER_MAP_MASK != 0,
        ),
    };
    if !keymap_changed && !modmap_changed {
        return (false, false);
    }
    let recipients: Vec<ClientId> = state
        .clients
        .keys()
        .copied()
        .filter(|id| {
            let c = state.xkb_clients.get(id).copied().unwrap_or_default();
            match cause {
                LegacyCause::MapNotify => !c.initialized || c.map_notify_mask & changed != 0,
                LegacyCause::NewKeyboardNotify => !c.initialized,
            }
        })
        .map(ClientId)
        .collect();
    let first = first_key.max(CLIENT_MIN_KEYCODE);
    let count = if i32::from(first_key) + i32::from(num_keys) - 1 <= i32::from(CLIENT_MAX_KEYCODE) {
        num_keys
    } else {
        CLIENT_MAX_KEYCODE - CLIENT_MIN_KEYCODE + 1
    };
    if keymap_changed {
        let _dropped = fanout_event_to_clients(state, &recipients, |buf, seq, order| {
            let _ = x11::write_mapping_notify_event(buf, order, seq, 1, first, count);
        });
    }
    if modmap_changed {
        let _dropped = fanout_event_to_clients(state, &recipients, |buf, seq, order| {
            let _ = x11::write_mapping_notify_event(buf, order, seq, 0, 0, 0);
        });
    }
    (keymap_changed, modmap_changed)
}

/// `XkbSendLegacyMapNotify` whole: the core MappingNotify, then the XI
/// `DeviceMappingNotify` companions (master keyboard) to every client that
/// selected them.
pub(crate) fn send_legacy_map_notify(
    state: &mut ServerState,
    cause: LegacyCause,
    changed: u16,
    first_key: u8,
    num_keys: u8,
) {
    let (keymap, modmap) = send_legacy_core_map_notify(state, cause, changed, first_key, num_keys);
    let dev = crate::xinput::DEVICEID_MASTER_KEYBOARD;
    if keymap {
        crate::core_loop::xi1_focus::emit_device_mapping_notify(
            state, None, dev, 1, first_key, num_keys,
        );
    }
    if modmap {
        crate::core_loop::xi1_focus::emit_device_mapping_notify(state, None, dev, 0, 0, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(
        dev: u16,
        affect: u16,
        clear: u16,
        all: u16,
        amap: u16,
        map: u16,
        rest: &[u8],
    ) -> Vec<u8> {
        let mut b = Vec::new();
        for v in [dev, affect, clear, all, amap, map] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(rest);
        b
    }

    /// `ProcXkbUseExtension`: 1.x and the pre-release 0.65 are supported
    /// (0.65 marks the client ancient); anything else leaves the client
    /// uninitialised.
    #[test]
    fn use_extension_initialises_supported_versions_only() {
        let mut state = ServerState::new();
        assert!(!use_extension(&mut state, ClientId(1), &[2, 0, 0, 0]));
        assert!(!xkb_initialized(&state, ClientId(1)));
        assert!(use_extension(&mut state, ClientId(1), &[1, 0, 0, 0]));
        assert!(xkb_initialized(&state, ClientId(1)));
        assert!(!xkb_ancient(&state, ClientId(1)));
        assert!(use_extension(&mut state, ClientId(2), &[0, 0, 65, 0]));
        assert!(xkb_initialized(&state, ClientId(2)) && xkb_ancient(&state, ClientId(2)));
    }

    /// The probe's selection (`XkbSelectEvents(core, 0x0fff, clear 0,
    /// selectAll 0x0fff, affectMap 0xff, map 0xff)`): every detail of every
    /// event; MapNotify's from affectMap/map, NewKeyboardNotify's per
    /// client.
    #[test]
    fn select_all_selects_every_detail() {
        let mut state = ServerState::new();
        select_events(
            &mut state,
            ClientId(1),
            &body(0x100, 0x0fff, 0, 0x0fff, 0xff, 0xff, &[]),
        )
        .unwrap();
        let c = state.xkb_clients[&1];
        assert_eq!(
            (c.map_notify_mask, c.new_keyboard_notify_mask),
            (0xff, 0xffff)
        );
        let i = state.xkb_interests[&(1, 0x100)];
        assert_eq!(
            (i.state, i.controls, i.names, i.compat),
            (0xffff, u32::MAX, 0xffff, 0xff)
        );
    }

    /// Detail pairs replace the affected bits; a CARD8 pair takes four
    /// bytes; `clear` wins over `selectAll`.
    #[test]
    fn select_events_refines_details() {
        let mut state = ServerState::new();
        // StateNotify (bit 2): affect 0x00ff value 0x0011; CompatMapNotify
        // (bit 7): affect 0x03 value 0x01 (+2 pad bytes).
        let rest = [0xff, 0x00, 0x11, 0x00, 0x03, 0x01, 0, 0];
        select_events(
            &mut state,
            ClientId(1),
            &body(0x100, 0x0084, 0, 0, 0, 0, &rest),
        )
        .unwrap();
        let i = state.xkb_interests[&(1, 0x100)];
        assert_eq!((i.state, i.compat), (0x0011, 0x01));
        select_events(
            &mut state,
            ClientId(1),
            &body(0x100, 0x0004, 0x0004, 0x0004, 0, 0, &[]),
        )
        .unwrap();
        assert_eq!(state.xkb_interests[&(1, 0x100)].state, 0);
    }

    /// `ProcXkbSelectEvents`' errors: an event index past
    /// ExtensionDeviceNotify (BadValue, `_XkbErrCode2(33, bit)`), a value
    /// outside its affect mask (BadMatch), an illegal detail bit
    /// (BadValue), a missing detail pair (BadLength).
    #[test]
    fn select_events_errors() {
        let mut state = ServerState::new();
        let r = select_events(
            &mut state,
            ClientId(1),
            &body(0x100, 0x1000, 0, 0x1000, 0, 0, &[]),
        );
        assert_eq!(r, Err((error::BAD_VALUE, (33 << 24) | 0x1000)));
        let r = select_events(
            &mut state,
            ClientId(1),
            &body(0x100, 0x0004, 0, 0, 0, 0, &[0x01, 0, 0x03, 0]),
        );
        assert_eq!(r, Err((error::BAD_MATCH, (2 << 24) | 0x02)));
        let r = select_events(
            &mut state,
            ClientId(1),
            &body(0x100, 0x0004, 0, 0, 0, 0, &[0x00, 0x40, 0, 0]),
        );
        assert_eq!(r, Err((error::BAD_VALUE, (2 << 24) | 0x4000)));
        let r = select_events(
            &mut state,
            ClientId(1),
            &body(0x100, 0x0004, 0, 0, 0, 0, &[]),
        );
        assert_eq!(r, Err((error::BAD_LENGTH, 0)));
    }
}
