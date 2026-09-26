//! XKB SetMap (#171 phase 4c): Xorg's `ProcXkbSetMap` (xkb/xkb.c) over the
//! model, ported literally.
//!
//! Three steps, as Xorg orders them:
//!
//! 1. [`check_length`]: `_XkbSetMapCheckLength` (BadLength);
//! 2. [`check`]: `_XkbSetMapChecks` (the keycode range, then
//!    `CheckKeyTypes`, `CheckKeySyms`, `CheckKeyActions`,
//!    `CheckKeyBehaviors`, `CheckVirtualMods`, `CheckKeyExplicit`,
//!    `CheckModifierMap`, `CheckVirtualModMap`, each BadValue with Xorg's
//!    `_XkbErrCodeN` errorValue), including its request-bounds checks and its
//!    edits of the request (an empty behaviors/explicit/modmap/vmodmap part
//!    is dropped from `present`; an ancient client's keycode range is
//!    replaced by the server's);
//! 3. [`decode`] + [`XkbDesc::set_map`]: `_XkbSetMap` — `XkbChangeKeycodeRange`
//!    and its NewKeyboardNotify, `SetKeyTypes` (with `XkbResizeKeyType`'s
//!    key-width resizing), `SetKeySyms`, `SetKeyActions`, `SetKeyBehaviors`,
//!    `SetVirtualMods`, `SetKeyExplicit`, `SetModifierMap`,
//!    `SetVirtualModMap` and the `XkbSetMapRecomputeActions` recompute.
//!
//! Offsets are into the whole request (the 4-byte header included), so they
//! read as Xorg's pointer arithmetic from `stuff`.

use super::{
    Action, Behavior, EXPLICIT_COMPONENTS_MASK, KEY_ACTIONS_MASK, KEY_BEHAVIORS_MASK,
    KEY_SYMS_MASK, KEY_TYPES_MASK, KeySyms, KeyType, KtEntry, MODIFIER_MAP_MASK, MapChanges, Mods,
    NUM_VMODS, VIRTUAL_MOD_MAP_MASK, VIRTUAL_MODS_MASK, XkbChanges, XkbDesc, padded,
    reply::{BAD_LENGTH, BAD_MATCH, BAD_VALUE, XkbError, err_code2, err_code3, err_code4},
};

/// `sz_xkbSetMapReq`.
const SET_MAP_REQ_SIZE: usize = 36;
/// `XkbSetMapResizeTypes` / `XkbSetMapRecomputeActions`.
const RESIZE_TYPES: u16 = 1 << 0;
const RECOMPUTE_ACTIONS: u16 = 1 << 1;
/// `XkbAllMapComponentsMask`.
const ALL_MAP_COMPONENTS: u16 = 0xff;
/// `XkbNumRequiredTypes`.
const NUM_REQUIRED_TYPES: u32 = 4;
/// `XkbMinLegalKeyCode`.
const MIN_LEGAL_KEYCODE: u8 = 8;
/// `XkbKB_*` behavior types.
const KB_PERMANENT: u8 = 0x80;
const KB_RADIO_GROUP: u8 = 0x02;
const KB_OVERLAY1: u8 = 0x03;
const KB_OVERLAY2: u8 = 0x04;
/// `XkbKB_RGAllowNone` / `XkbMaxRadioGroups`.
const KB_RG_ALLOW_NONE: u8 = 0x80;
const MAX_RADIO_GROUPS: u32 = 32;

/// `xkbSetMapReq`'s fields after the 4-byte header, plus the request length
/// in bytes. `check` edits `present` and the keycode range as Xorg's checks
/// edit the request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SetMapHeader {
    pub length: usize,
    pub device_spec: u16,
    pub present: u16,
    pub flags: u16,
    pub min_key_code: u8,
    pub max_key_code: u8,
    pub first_type: u8,
    pub n_types: u8,
    pub first_key_sym: u8,
    pub n_key_syms: u8,
    pub total_syms: u16,
    pub first_key_act: u8,
    pub n_key_acts: u8,
    pub total_acts: u16,
    pub first_key_behavior: u8,
    pub n_key_behaviors: u8,
    pub total_key_behaviors: u8,
    pub first_key_explicit: u8,
    pub n_key_explicit: u8,
    pub total_key_explicit: u8,
    pub first_mod_map_key: u8,
    pub n_mod_map_keys: u8,
    pub total_mod_map_keys: u8,
    pub first_vmod_map_key: u8,
    pub n_vmod_map_keys: u8,
    pub total_vmod_map_keys: u8,
    pub virtual_mods: u16,
}

fn u16_at(req: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([req[at], req[at + 1]])
}

fn u32_at(req: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([req[at], req[at + 1], req[at + 2], req[at + 3]])
}

impl SetMapHeader {
    /// The header of a whole request `req` (at least 36 bytes).
    pub(crate) fn parse(req: &[u8]) -> Self {
        let b = |i: usize| req[i];
        Self {
            length: req.len(),
            device_spec: u16_at(req, 4),
            present: u16_at(req, 6),
            flags: u16_at(req, 8),
            min_key_code: b(10),
            max_key_code: b(11),
            first_type: b(12),
            n_types: b(13),
            first_key_sym: b(14),
            n_key_syms: b(15),
            total_syms: u16_at(req, 16),
            first_key_act: b(18),
            n_key_acts: b(19),
            total_acts: u16_at(req, 20),
            first_key_behavior: b(22),
            n_key_behaviors: b(23),
            total_key_behaviors: b(24),
            first_key_explicit: b(25),
            n_key_explicit: b(26),
            total_key_explicit: b(27),
            first_mod_map_key: b(28),
            n_mod_map_keys: b(29),
            total_mod_map_keys: b(30),
            first_vmod_map_key: b(31),
            n_vmod_map_keys: b(32),
            total_vmod_map_keys: b(33),
            virtual_mods: u16_at(req, 34),
        }
    }
}

fn bad_value(value: u32) -> XkbError {
    XkbError {
        code: BAD_VALUE,
        value,
    }
}

/// `CHK_MASK_LEGAL(0x01, stuff->present, XkbAllMapComponentsMask)`.
pub(crate) fn check_present(h: &SetMapHeader) -> Result<(), XkbError> {
    if h.present & !ALL_MAP_COMPONENTS != 0 {
        return Err(bad_value(err_code2(
            0x01,
            u32::from(h.present & !ALL_MAP_COMPONENTS),
        )));
    }
    Ok(())
}

/// Port of `_XkbSetMapCheckLength`: the request's length must be exactly
/// what its present parts say. errorValue is Xorg's stale
/// `client->errorValue`, 0 for a client with no earlier error.
pub(crate) fn check_length(h: &SetMapHeader, req: &[u8]) -> Result<(), XkbError> {
    let bad = XkbError {
        code: BAD_LENGTH,
        value: 0,
    };
    let req_len = h.length;
    let mut len = SET_MAP_REQ_SIZE;
    if req_len < len {
        return Err(bad);
    }
    let add = |len: &mut usize, new: usize| -> Result<(), XkbError> {
        if *len > req_len.saturating_sub(new) || new > req_len {
            return Err(bad);
        }
        *len += new;
        Ok(())
    };
    if h.present & KEY_TYPES_MASK != 0 {
        let mut at = SET_MAP_REQ_SIZE;
        for _ in 0..h.n_types {
            add(&mut len, padded(8))?;
            let map_count = usize::from(req[at + 5]);
            let preserve = req[at + 6] != 0;
            add(&mut len, map_count * 4)?;
            if preserve {
                add(&mut len, map_count * 4)?;
            }
            at += 8 + map_count * 4 + if preserve { map_count * 4 } else { 0 };
        }
    }
    if h.present & KEY_SYMS_MASK != 0 {
        let mut at = len;
        for _ in 0..h.n_key_syms {
            add(&mut len, 8)?;
            let n_syms = usize::from(u16_at(req, at + 6));
            add(&mut len, n_syms * 4)?;
            at += 8 + n_syms * 4;
        }
    }
    if h.present & KEY_ACTIONS_MASK != 0 {
        add(
            &mut len,
            usize::from(h.total_acts) * 8 + padded(usize::from(h.n_key_acts)),
        )?;
    }
    if h.present & KEY_BEHAVIORS_MASK != 0 {
        add(&mut len, usize::from(h.total_key_behaviors) * 4)?;
    }
    if h.present & VIRTUAL_MODS_MASK != 0 {
        add(&mut len, padded(h.virtual_mods.count_ones() as usize))?;
    }
    if h.present & EXPLICIT_COMPONENTS_MASK != 0 {
        add(&mut len, padded(usize::from(h.total_key_explicit) * 2))?;
    }
    if h.present & MODIFIER_MAP_MASK != 0 {
        add(&mut len, padded(usize::from(h.total_mod_map_keys) * 2))?;
    }
    if h.present & VIRTUAL_MOD_MAP_MASK != 0 {
        add(&mut len, usize::from(h.total_vmod_map_keys) * 4)?;
    }
    if len == req_len { Ok(()) } else { Err(bad) }
}

/// `_XkbCheckRequestBounds`: `from..to` is a non-empty range inside the
/// request.
fn in_bounds(req_len: usize, from: usize, to: usize) -> bool {
    from < to && from < req_len && to <= req_len
}

/// `CHK_REQ_KEY_RANGE2`: first..first+num-1 inside the request's keycodes.
fn chk_req_key_range(err: u32, first: u8, num: u8, h: &SetMapHeader) -> Result<(), u32> {
    let last = (u32::from(first) + u32::from(num)).wrapping_sub(1);
    if last > u32::from(h.max_key_code) {
        return Err(err_code4(
            err,
            u32::from(first),
            u32::from(num),
            u32::from(h.max_key_code),
        ));
    }
    if first < h.min_key_code {
        return Err(err_code3(
            err + 1,
            u32::from(first),
            u32::from(h.min_key_code),
        ));
    }
    Ok(())
}

/// Per-type level counts and per-key keysym counts the checks build
/// (`mapWidths`, `symsPerKey`), sized for Xorg's worst-case indices.
struct Widths {
    map_widths: [u8; 512],
    syms_per_key: [u16; 256],
}

/// Port of `CheckKeyTypes`: `Ok(nTypes)` (the type count the rest checks
/// against) or the errorValue.
fn check_key_types(
    desc: &XkbDesc,
    h: &SetMapHeader,
    req: &[u8],
    at: &mut usize,
    w: &mut Widths,
) -> Result<usize, u32> {
    let num_types = desc.types.len();
    let levels = |i: usize| desc.types.get(i).map_or(0, |t| t.num_levels);
    let first = usize::from(h.first_type);
    let n = usize::from(h.n_types);
    if first > num_types {
        return Err(err_code3(
            0x01,
            u32::from(h.first_type),
            u32::try_from(num_types).unwrap_or(0),
        ));
    }
    let n_maps = if h.flags & RESIZE_TYPES != 0 {
        let n_maps = first + n;
        if n_maps < NUM_REQUIRED_TYPES as usize {
            return Err(err_code4(
                0x02,
                u32::from(h.first_type),
                u32::from(h.n_types),
                NUM_REQUIRED_TYPES,
            ));
        }
        n_maps
    } else if h.present & KEY_TYPES_MASK != 0 {
        if first + n > num_types {
            return Err(u32::try_from(first + n).unwrap_or(0));
        }
        num_types
    } else {
        for i in 0..num_types {
            w.map_widths[i] = levels(i);
        }
        return Ok(num_types);
    };
    for i in 0..first {
        w.map_widths[i] = levels(i);
    }
    let mut p = *at;
    for i in 0..n {
        let i32_ = u32::try_from(i).unwrap_or(0);
        if !in_bounds(h.length, p, p + 8) {
            return Err(err_code3(0x0b, u32::from(h.n_types), i32_));
        }
        let ndx = i + first;
        let ndx32 = u32::try_from(ndx).unwrap_or(0);
        let width = req[p + 4];
        let (real, vmods) = (req[p + 1], u16_at(req, p + 2));
        if width < 1 {
            return Err(err_code3(0x04, ndx32, u32::from(width)));
        } else if (ndx == 0 && width != 1) || (width != 2 && (1..=3).contains(&ndx)) {
            return Err(err_code3(0x05, ndx32, u32::from(width)));
        }
        let n_entries = usize::from(req[p + 5]);
        let preserve = req[p + 6] != 0;
        let next = if n_entries > 0 {
            let map_at = p + 8;
            if !in_bounds(h.length, map_at, map_at + 4 * n_entries) {
                return Err(err_code3(0x0c, i32_, u32::from(req[p + 5])));
            }
            let pre_at = map_at + 4 * n_entries;
            if preserve && !in_bounds(h.length, pre_at, pre_at + 4 * n_entries) {
                return Err(err_code3(0x0d, i32_, u32::from(req[p + 5])));
            }
            for e in 0..n_entries {
                let e32 = u32::try_from(e).unwrap_or(0);
                let ea = map_at + 4 * e;
                let (level, ereal, evmods) = (req[ea], req[ea + 1], u16_at(req, ea + 2));
                if ereal & !real != 0 {
                    return Err(err_code4(0x06, e32, u32::from(ereal), u32::from(real)));
                }
                if evmods & !vmods != 0 {
                    return Err(err_code3(0x07, e32, u32::from(evmods)));
                }
                if level >= width {
                    return Err(err_code4(0x08, e32, u32::from(width), u32::from(level)));
                }
                if preserve {
                    let pa = pre_at + 4 * e;
                    let (preal, pvmods) = (req[pa + 1], u16_at(req, pa + 2));
                    if preal & !ereal != 0 {
                        return Err(err_code4(0x09, e32, u32::from(preal), u32::from(ereal)));
                    }
                    if pvmods & !evmods != 0 {
                        return Err(err_code3(0x0a, e32, u32::from(pvmods)));
                    }
                }
            }
            pre_at + if preserve { 4 * n_entries } else { 0 }
        } else {
            p + 8
        };
        w.map_widths[ndx] = width;
        p = next;
    }
    for i in first + n..n_maps {
        w.map_widths[i] = levels(i);
    }
    *at = p;
    Ok(n_maps)
}

/// Port of `CheckKeySyms`, its closing loop included: that loop restarts at
/// keycode `nKeySyms` (Xorg reuses the count as a keycode) and recomputes
/// `symsPerKey` from the current map for every keycode from there to the
/// maximum.
fn check_key_syms(
    desc: &XkbDesc,
    h: &SetMapHeader,
    req: &[u8],
    n_types: usize,
    at: &mut usize,
    w: &mut Widths,
) -> Result<(), u32> {
    chk_req_key_range(0x11, h.first_key_sym, h.n_key_syms, h)?;
    let mut p = *at;
    for i in 0..usize::from(h.n_key_syms) {
        let kc = usize::from(h.first_key_sym) + i;
        let (kc32, i32_) = (
            u32::try_from(kc).unwrap_or(0),
            u32::try_from(i).unwrap_or(0),
        );
        if !in_bounds(h.length, p, p + 8) {
            return Err(err_code3(0x18, kc32, i32_));
        }
        let n_syms = u16_at(req, p + 6);
        let n_groups = usize::from(req[p + 4] & 0x0f);
        if n_groups > super::NUM_GROUPS {
            return Err(err_code3(0x14, kc32, u32::try_from(n_groups).unwrap_or(0)));
        }
        if n_groups > 0 {
            let mut width = 0u8;
            for g in 0..n_groups {
                let kt = req[p + g];
                if usize::from(kt) >= n_types {
                    return Err(err_code4(
                        0x15,
                        kc32,
                        u32::try_from(g).unwrap_or(0),
                        u32::from(kt),
                    ));
                }
                width = width.max(w.map_widths[usize::from(kt)]);
            }
            if req[p + 5] != width {
                return Err(err_code3(0x16, kc32, u32::from(req[p + 5])));
            }
            let total = u16::from(width) * u16::try_from(n_groups).unwrap_or(0);
            w.syms_per_key[kc] = total;
            if total != n_syms {
                return Err(err_code4(0x16, kc32, u32::from(n_syms), u32::from(total)));
            }
        } else if n_syms != 0 {
            return Err(err_code3(0x17, kc32, u32::from(n_syms)));
        }
        let syms_at = p + 8;
        let end = syms_at + 4 * usize::from(n_syms);
        if n_syms != 0 && !in_bounds(h.length, syms_at, end) {
            return Err(err_code3(0x19, kc32, u32::from(n_syms)));
        }
        p = end;
    }
    for i in usize::from(h.n_key_syms)..=usize::from(desc.max_key_code) {
        let key = &desc.keys[i];
        let mut width = 0u8;
        for g in 0..key.num_groups() {
            let kt = key.kt_index[g];
            if usize::from(kt) >= n_types {
                return Err(err_code4(
                    0x18,
                    u32::try_from(i).unwrap_or(0),
                    u32::try_from(g).unwrap_or(0),
                    u32::from(kt),
                ));
            }
            width = width.max(w.map_widths[usize::from(kt)]);
        }
        w.syms_per_key[i] = u16::from(width) * u16::try_from(key.num_groups()).unwrap_or(0);
    }
    *at = p;
    Ok(())
}

/// Port of `CheckKeyActions`: each key's action count is 0 or its keysym
/// count. The actions themselves aren't bounds-checked (Xorg doesn't).
fn check_key_actions(h: &SetMapHeader, req: &[u8], at: &mut usize, w: &Widths) -> Result<(), u32> {
    chk_req_key_range(0x21, h.first_key_act, h.n_key_acts, h)?;
    let mut p = *at;
    let mut n_acts = 0usize;
    for i in 0..usize::from(h.n_key_acts) {
        let kc = usize::from(h.first_key_act) + i;
        let kc32 = u32::try_from(kc).unwrap_or(0);
        if !in_bounds(h.length, p, p + 1) {
            return Err(err_code3(0x24, kc32, u32::try_from(i).unwrap_or(0)));
        }
        let count = req[p];
        if count != 0 {
            if u16::from(count) == w.syms_per_key[kc] {
                n_acts += usize::from(count);
            } else {
                return Err(err_code3(0x23, kc32, u32::from(count)));
            }
        }
        p += 1;
    }
    let n = usize::from(h.n_key_acts);
    if n % 4 != 0 {
        p += 4 - n % 4;
    }
    *at = p + 8 * n_acts;
    Ok(())
}

/// Port of `CheckKeyBehaviors` (an empty part is dropped from the request).
fn check_key_behaviors(
    desc: &XkbDesc,
    h: &mut SetMapHeader,
    req: &[u8],
    at: &mut usize,
) -> Result<(), u32> {
    if h.present & KEY_BEHAVIORS_MASK == 0 || h.n_key_behaviors < 1 {
        h.present &= !KEY_BEHAVIORS_MASK;
        h.n_key_behaviors = 0;
        return Ok(());
    }
    let first = u32::from(h.first_key_behavior);
    let last = first + u32::from(h.n_key_behaviors) - 1;
    if first < u32::from(h.min_key_code) {
        return Err(err_code3(0x31, first, u32::from(h.min_key_code)));
    }
    if last > u32::from(h.max_key_code) {
        return Err(err_code3(0x32, last, u32::from(h.max_key_code)));
    }
    let mut p = *at;
    for i in 0..u32::from(h.total_key_behaviors) {
        if !in_bounds(h.length, p, p + 4) {
            return Err(err_code3(0x36, first, i));
        }
        let (key, kind, data) = (req[p], req[p + 1], req[p + 2]);
        let key32 = u32::from(key);
        if key32 < first || key32 > last {
            return Err(err_code4(0x33, first, last, key32));
        }
        let cur = desc.behaviors[usize::from(key)];
        if kind & KB_PERMANENT != 0 && (cur.kind != kind || cur.data != data) {
            return Err(err_code3(0x33, key32, u32::from(kind)));
        }
        if kind == KB_RADIO_GROUP && u32::from(data & !KB_RG_ALLOW_NONE) > MAX_RADIO_GROUPS {
            return Err(err_code4(0x34, key32, u32::from(data), MAX_RADIO_GROUPS));
        }
        if kind == KB_OVERLAY1 || kind == KB_OVERLAY2 {
            // CHK_KEY_RANGE2(0x35, key, 1, xkb)
            if key > desc.max_key_code {
                return Err(err_code4(0x35, key32, 1, u32::from(desc.max_key_code)));
            } else if key < desc.min_key_code {
                return Err(err_code3(0x36, key32, u32::from(desc.min_key_code)));
            }
        }
        p += 4;
    }
    *at = p;
    Ok(())
}

/// Port of `CheckVirtualMods`.
fn check_virtual_mods(h: &SetMapHeader, at: &mut usize) -> Result<(), u32> {
    if h.present & VIRTUAL_MODS_MASK == 0 || h.virtual_mods == 0 {
        return Ok(());
    }
    let n_mods = h.virtual_mods.count_ones() as usize;
    if !in_bounds(h.length, *at, *at + padded(n_mods)) {
        return Err(err_code3(0x37, u32::try_from(n_mods).unwrap_or(0), 16));
    }
    *at += padded(n_mods);
    Ok(())
}

/// The shared shape of `CheckKeyExplicit` / `CheckModifierMap`: a keycode
/// range and `total` two-byte (key, value) entries inside it, padded.
/// `err` is the part's error base (0x51 / 0x61); `bounds_err` computes the
/// out-of-request errorValue.
#[allow(clippy::too_many_arguments)]
fn check_key_pairs(
    h: &SetMapHeader,
    req: &[u8],
    at: &mut usize,
    first: u8,
    num: u8,
    total: u8,
    err: u32,
    bounds_err: impl Fn(u32, u32, u32) -> u32,
) -> Result<(), u32> {
    let first = u32::from(first);
    let last = first + u32::from(num) - 1;
    if first < u32::from(h.min_key_code) {
        return Err(err_code3(err, first, u32::from(h.min_key_code)));
    }
    if last > u32::from(h.max_key_code) {
        return Err(err_code3(err + 1, last, u32::from(h.max_key_code)));
    }
    let start = *at;
    let mut p = start;
    for i in 0..u32::from(total) {
        if !in_bounds(h.length, p, p + 2) {
            return Err(bounds_err(first, last, i));
        }
        let key = u32::from(req[p]);
        if key < first || key > last {
            return Err(err_code4(err + 2, first, last, key));
        }
        p += 2;
    }
    *at = start + padded(p - start);
    Ok(())
}

/// Port of `CheckKeyExplicit` (every explicit bit is legal, so its 0x52
/// bits error can't fire).
fn check_key_explicit(h: &mut SetMapHeader, req: &[u8], at: &mut usize) -> Result<(), u32> {
    if h.present & EXPLICIT_COMPONENTS_MASK == 0 || h.n_key_explicit < 1 {
        h.present &= !EXPLICIT_COMPONENTS_MASK;
        h.n_key_explicit = 0;
        return Ok(());
    }
    let hc = *h;
    check_key_pairs(
        &hc,
        req,
        at,
        hc.first_key_explicit,
        hc.n_key_explicit,
        hc.total_key_explicit,
        0x51,
        |first, last, i| err_code4(0x54, first, last, i),
    )
}

/// Port of `CheckModifierMap`.
fn check_modifier_map(h: &mut SetMapHeader, req: &[u8], at: &mut usize) -> Result<(), u32> {
    if h.present & MODIFIER_MAP_MASK == 0 || h.n_mod_map_keys < 1 {
        h.present &= !MODIFIER_MAP_MASK;
        h.n_mod_map_keys = 0;
        return Ok(());
    }
    let hc = *h;
    check_key_pairs(
        &hc,
        req,
        at,
        hc.first_mod_map_key,
        hc.n_mod_map_keys,
        hc.total_mod_map_keys,
        0x61,
        |_, _, i| err_code3(0x64, u32::from(hc.total_mod_map_keys), i),
    )
}

/// Port of `CheckVirtualModMap`.
fn check_virtual_mod_map(h: &mut SetMapHeader, req: &[u8], at: &mut usize) -> Result<(), u32> {
    if h.present & VIRTUAL_MOD_MAP_MASK == 0 || h.n_vmod_map_keys < 1 {
        h.present &= !VIRTUAL_MOD_MAP_MASK;
        h.n_vmod_map_keys = 0;
        return Ok(());
    }
    let first = u32::from(h.first_vmod_map_key);
    let last = first + u32::from(h.n_vmod_map_keys) - 1;
    if first < u32::from(h.min_key_code) {
        return Err(err_code3(0x71, first, u32::from(h.min_key_code)));
    }
    if last > u32::from(h.max_key_code) {
        return Err(err_code3(0x72, last, u32::from(h.max_key_code)));
    }
    let mut p = *at;
    for i in 0..u32::from(h.total_vmod_map_keys) {
        if !in_bounds(h.length, p, p + 4) {
            return Err(err_code3(0x74, first, i));
        }
        let key = u32::from(req[p]);
        if key < first || key > last {
            return Err(err_code4(0x73, first, last, key));
        }
        p += 4;
    }
    *at = p;
    Ok(())
}

/// Port of `_XkbSetMapChecks`: whether the request can be applied to
/// `desc`, without changing it. Edits `h` as Xorg edits the request.
pub(crate) fn check(
    desc: &XkbDesc,
    h: &mut SetMapHeader,
    req: &[u8],
    client_is_ancient: bool,
) -> Result<(), XkbError> {
    if desc.min_key_code != h.min_key_code || desc.max_key_code != h.max_key_code {
        if client_is_ancient {
            h.min_key_code = desc.min_key_code;
            h.max_key_code = desc.max_key_code;
        } else {
            let (min, max) = (u32::from(h.min_key_code), u32::from(h.max_key_code));
            if h.min_key_code < MIN_LEGAL_KEYCODE {
                return Err(bad_value(err_code3(2, min, max)));
            }
            if h.min_key_code > h.max_key_code {
                return Err(XkbError {
                    code: BAD_MATCH,
                    value: err_code3(3, min, max),
                });
            }
        }
    }
    let mut w = Widths {
        map_widths: [0; 512],
        syms_per_key: [0; 256],
    };
    let mut at = SET_MAP_REQ_SIZE;
    let n_types = check_key_types(desc, h, req, &mut at, &mut w).map_err(bad_value)?;
    for i in desc.min_key_code..desc.max_key_code {
        let key = &desc.keys[usize::from(i)];
        let mut width = 0u8;
        for g in 0..key.num_groups() {
            let kt = key.kt_index[g];
            if usize::from(kt) >= n_types {
                return Err(bad_value(err_code4(
                    0x13,
                    u32::from(i),
                    u32::try_from(g).unwrap_or(0),
                    u32::from(kt),
                )));
            }
            width = width.max(w.map_widths[usize::from(kt)]);
        }
        w.syms_per_key[usize::from(i)] =
            u16::from(width) * u16::try_from(key.num_groups()).unwrap_or(0);
    }
    if h.present & KEY_SYMS_MASK != 0 {
        check_key_syms(desc, h, req, n_types, &mut at, &mut w).map_err(bad_value)?;
    }
    if h.present & KEY_ACTIONS_MASK != 0 {
        check_key_actions(h, req, &mut at, &w).map_err(bad_value)?;
    }
    if h.present & KEY_BEHAVIORS_MASK != 0 {
        check_key_behaviors(desc, h, req, &mut at).map_err(bad_value)?;
    }
    if h.present & VIRTUAL_MODS_MASK != 0 {
        check_virtual_mods(h, &mut at).map_err(bad_value)?;
    }
    if h.present & EXPLICIT_COMPONENTS_MASK != 0 {
        check_key_explicit(h, req, &mut at).map_err(bad_value)?;
    }
    if h.present & MODIFIER_MAP_MASK != 0 {
        check_modifier_map(h, req, &mut at).map_err(bad_value)?;
    }
    if h.present & VIRTUAL_MOD_MAP_MASK != 0 {
        check_virtual_mod_map(h, req, &mut at).map_err(bad_value)?;
    }
    if at / 4 != h.length / 4 {
        return Err(XkbError {
            code: BAD_LENGTH,
            value: u32::try_from(at.wrapping_sub(SET_MAP_REQ_SIZE)).unwrap_or(0),
        });
    }
    Ok(())
}

/// A key type as SetMap carries it (`xkbKeyTypeWireDesc` and its
/// `xkbKTSetMapEntryWireDesc` / `xkbModsWireDesc` entries).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct WireType {
    pub real: u8,
    pub vmods: u16,
    pub num_levels: u8,
    /// `(level, real mods, virtual mods)`.
    pub entries: Vec<(u8, u8, u16)>,
    /// `(real mods, virtual mods)` per entry.
    pub preserve: Option<Vec<(u8, u16)>>,
}

/// A decoded SetMap: its parts in wire order, as `_XkbSetMap` reads them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SetMap {
    pub types: Vec<WireType>,
    pub key_syms: Vec<KeySyms>,
    /// Per key of the actions range: none, or one action per keysym.
    pub acts: Vec<Vec<Action>>,
    /// `(key, type, data)`.
    pub behaviors: Vec<(u8, u8, u8)>,
    /// One real mapping per virtual modifier in `virtualMods`, in order.
    pub vmods: Vec<u8>,
    pub explicit: Vec<(u8, u8)>,
    pub modmap: Vec<(u8, u8)>,
    pub vmodmap: Vec<(u8, u16)>,
    /// Where the parts end (Xorg's `values` after `_XkbSetMap`'s Set*).
    pub end: usize,
}

/// Read the parts of a checked request as `_XkbSetMap` walks them (only the
/// parts `h.present` still names). `None` when a part would run past the
/// request, which only a request Xorg reads out of its buffer can do.
pub(crate) fn decode(h: &SetMapHeader, req: &[u8]) -> Option<SetMap> {
    let mut m = SetMap::default();
    let mut p = SET_MAP_REQ_SIZE;
    let get = |from: usize, n: usize| req.get(from..from + n);
    if h.present & KEY_TYPES_MASK != 0 {
        for _ in 0..h.n_types {
            let t = get(p, 8)?;
            let n = usize::from(t[5]);
            let preserve = t[6] != 0;
            let entries = get(p + 8, 4 * n)?;
            let mut wt = WireType {
                real: t[1],
                vmods: u16::from_le_bytes([t[2], t[3]]),
                num_levels: t[4],
                entries: entries
                    .chunks_exact(4)
                    .map(|e| (e[0], e[1], u16::from_le_bytes([e[2], e[3]])))
                    .collect(),
                preserve: None,
            };
            p += 8 + 4 * n;
            // SetKeyTypes skips the preserve entries of a type without map
            // entries (there are none).
            if preserve && n > 0 {
                let pre = get(p, 4 * n)?;
                wt.preserve = Some(
                    pre.chunks_exact(4)
                        .map(|e| (e[1], u16::from_le_bytes([e[2], e[3]])))
                        .collect(),
                );
                p += 4 * n;
            } else if preserve {
                wt.preserve = Some(Vec::new());
            }
            m.types.push(wt);
        }
    }
    if h.present & KEY_SYMS_MASK != 0 {
        for _ in 0..h.n_key_syms {
            let s = get(p, 8)?;
            let n = usize::from(u16::from_le_bytes([s[6], s[7]]));
            let syms = get(p + 8, 4 * n)?;
            m.key_syms.push(KeySyms {
                kt_index: [s[0], s[1], s[2], s[3]],
                group_info: s[4],
                width: s[5],
                syms: syms.chunks_exact(4).map(|c| u32_at(c, 0)).collect(),
            });
            p += 8 + 4 * n;
        }
    }
    if h.present & KEY_ACTIONS_MASK != 0 {
        let n = usize::from(h.n_key_acts);
        let counts = get(p, n)?.to_vec();
        p += padded(n);
        for c in counts {
            let acts = get(p, 8 * usize::from(c))?;
            m.acts.push(
                acts.chunks_exact(8)
                    .map(|a| std::array::from_fn(|i| a[i]))
                    .collect(),
            );
            p += 8 * usize::from(c);
        }
    }
    if h.present & KEY_BEHAVIORS_MASK != 0 {
        for _ in 0..h.total_key_behaviors {
            let b = get(p, 4)?;
            m.behaviors.push((b[0], b[1], b[2]));
            p += 4;
        }
    }
    if h.present & VIRTUAL_MODS_MASK != 0 && h.virtual_mods != 0 {
        let n = h.virtual_mods.count_ones() as usize;
        m.vmods = get(p, n)?.to_vec();
        p += padded(n);
    }
    let pairs = |p: &mut usize, total: u8| -> Option<Vec<(u8, u8)>> {
        let n = usize::from(total);
        let v = get(*p, 2 * n)?
            .chunks_exact(2)
            .map(|e| (e[0], e[1]))
            .collect();
        *p += padded(2 * n);
        Some(v)
    };
    if h.present & EXPLICIT_COMPONENTS_MASK != 0 {
        m.explicit = pairs(&mut p, h.total_key_explicit)?;
    }
    if h.present & MODIFIER_MAP_MASK != 0 {
        m.modmap = pairs(&mut p, h.total_mod_map_keys)?;
    }
    if h.present & VIRTUAL_MOD_MAP_MASK != 0 {
        for _ in 0..h.total_vmod_map_keys {
            let v = get(p, 4)?;
            m.vmodmap.push((v[0], u16::from_le_bytes([v[2], v[3]])));
            p += 4;
        }
    }
    m.end = p;
    Some(m)
}

/// The keycode range change of a SetMap, for its NewKeyboardNotify.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct KeycodeRangeChange {
    pub old_min: u8,
    pub old_max: u8,
    pub min: u8,
    pub max: u8,
}

/// What `_XkbSetMap` did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SetMapResult {
    pub changes: XkbChanges,
    /// The keycode range differed: the NewKeyboardNotify's range (Xorg
    /// sends it first, and then no other notification).
    pub range: Option<KeycodeRangeChange>,
    /// Xorg's length re-check after the parts failed (a request its checks
    /// walked differently): BadLength with this errorValue; the parts
    /// stay applied, the recompute and the notification don't happen.
    pub bad_length: Option<u32>,
}

/// The range merge every Set* helper ends with: `first..=last` joined with
/// what `changes` already has (int arithmetic, stored into CARD8s).
fn merge_range(
    changed: bool,
    old_first: u8,
    old_num: u8,
    first: u32,
    last: u32,
    first_lt: impl Fn(u32, u32) -> bool,
) -> (u8, u8) {
    let (mut first, mut last) = (first, last);
    if changed {
        let old_last = (u32::from(old_first) + u32::from(old_num)).wrapping_sub(1);
        if first_lt(u32::from(old_first), first) {
            first = u32::from(old_first);
        }
        if old_last > last {
            last = old_last;
        }
    }
    let byte = |v: u32| (v & 0xff) as u8;
    (byte(first), byte(last.wrapping_sub(first).wrapping_add(1)))
}

/// `_ExtendRange` (xkb/XKBMAlloc.c).
fn extend_range(changed: &mut u16, flag: u16, kc: u8, first: &mut u8, num: &mut u8) {
    if *changed & flag == 0 {
        *changed |= flag;
        *first = kc;
        *num = 1;
    } else {
        let last = i32::from(*first) + i32::from(*num) - 1;
        if kc < *first {
            *first = kc;
            *num = ((last - i32::from(kc)) + 1) as u8;
        } else if i32::from(kc) > last {
            *num = ((i32::from(kc) - i32::from(*first)) + 1) as u8;
        }
    }
}

impl XkbDesc {
    /// Port of `XkbChangeKeycodeRange` (xkb/XKBMAlloc.c): the range only
    /// grows, the new keys start empty. (yserver's range is 8..=255, the
    /// legal maximum, so on yserver this never changes anything.)
    fn change_keycode_range(&mut self, min: u8, max: u8, mc: &mut MapChanges) {
        if min < self.min_key_code {
            let (lo, hi) = (usize::from(min), usize::from(self.min_key_code));
            self.clear_keys(lo, hi);
            extend_range(
                &mut mc.changed,
                KEY_SYMS_MASK,
                min,
                &mut mc.first_key_sym,
                &mut mc.num_key_syms,
            );
            extend_range(
                &mut mc.changed,
                MODIFIER_MAP_MASK,
                min,
                &mut mc.first_modmap_key,
                &mut mc.num_modmap_keys,
            );
            extend_range(
                &mut mc.changed,
                KEY_BEHAVIORS_MASK,
                min,
                &mut mc.first_key_behavior,
                &mut mc.num_key_behaviors,
            );
            extend_range(
                &mut mc.changed,
                KEY_ACTIONS_MASK,
                min,
                &mut mc.first_key_act,
                &mut mc.num_key_acts,
            );
            // Xorg extends the vmodmap range from first_modmap_key.
            extend_range(
                &mut mc.changed,
                VIRTUAL_MOD_MAP_MASK,
                min,
                &mut mc.first_modmap_key,
                &mut mc.num_vmodmap_keys,
            );
            self.min_key_code = min;
        }
        if max > self.max_key_code {
            // Xorg clears from the old maximum, that key included.
            self.clear_keys(usize::from(self.max_key_code), 256);
            extend_range(
                &mut mc.changed,
                KEY_SYMS_MASK,
                max,
                &mut mc.first_key_sym,
                &mut mc.num_key_syms,
            );
            extend_range(
                &mut mc.changed,
                MODIFIER_MAP_MASK,
                max,
                &mut mc.first_modmap_key,
                &mut mc.num_modmap_keys,
            );
            extend_range(
                &mut mc.changed,
                KEY_BEHAVIORS_MASK,
                max,
                &mut mc.first_key_behavior,
                &mut mc.num_key_behaviors,
            );
            extend_range(
                &mut mc.changed,
                KEY_ACTIONS_MASK,
                max,
                &mut mc.first_key_act,
                &mut mc.num_key_acts,
            );
            extend_range(
                &mut mc.changed,
                VIRTUAL_MOD_MAP_MASK,
                max,
                &mut mc.first_modmap_key,
                &mut mc.num_vmodmap_keys,
            );
            self.max_key_code = max;
        }
    }

    /// Keys `lo..hi` emptied (key map, modmap, behaviors, actions, vmodmap,
    /// names), as `XkbChangeKeycodeRange`'s memsets.
    fn clear_keys(&mut self, lo: usize, hi: usize) {
        for k in lo..hi {
            self.keys[k] = KeySyms::default();
            self.modmap[k] = 0;
            self.behaviors[k] = Behavior::default();
            self.acts[k] = None;
            self.vmodmap[k] = 0;
            self.names.keys[k] = [0; 4];
        }
    }

    /// `XkbResizeKeyActions` for a key that has actions or gets them: none
    /// for 0; the current ones when the key's keysyms already cover
    /// `needed`; else `needed` actions keeping the current ones (Xorg's
    /// reallocating path; its spare-capacity path starts the key over with
    /// zeroed actions, which depends on its allocator state).
    fn resize_key_actions_xorg(&mut self, kc: u8, needed: usize) {
        let k = usize::from(kc);
        if needed == 0 {
            self.acts[k] = None;
            return;
        }
        let old = self.keys[k].num_syms();
        if self.acts[k].is_some() && old >= needed {
            return;
        }
        let mut v = self.acts[k].take().unwrap_or_default();
        v.truncate(old.min(needed));
        v.resize(needed, [0; 8]);
        self.acts[k] = Some(v);
    }

    /// Port of `XkbResizeKeyType` (xkb/XKBMAlloc.c) as `SetKeyTypes` calls
    /// it: the map and preserve arrays, the level names (new slots are
    /// uninitialised in Xorg; None here), and the keysyms of the keys whose
    /// width the level count change affects.
    fn resize_key_type(&mut self, ndx: usize, map_count: usize, preserve: bool, new_levels: u8) {
        let t = &mut self.types[ndx];
        if map_count == 0 {
            t.map.clear();
            t.preserve = None;
        } else if !preserve {
            t.preserve = None;
        }
        let old_levels = t.num_levels;
        if new_levels > old_levels || t.level_names.is_none() {
            let mut names = t.level_names.take().unwrap_or_default();
            names.resize(usize::from(new_levels), None);
            t.level_names = Some(names);
        }
        let uses = |key: &KeySyms| {
            (0..key.num_groups())
                .rev()
                .any(|g| usize::from(key.kt_index[g & 3]) == ndx)
        };
        let mut matching: Vec<usize> = Vec::new();
        if new_levels > old_levels {
            for i in usize::from(self.min_key_code)..=usize::from(self.max_key_code) {
                let key = &self.keys[i];
                if key.width < old_levels || key.width >= new_levels {
                    continue;
                }
                if uses(key) {
                    matching.push(i);
                }
            }
            if !matching.is_empty() {
                // Each group moves to `new_levels` apart; the key's width
                // stays, so its keysyms read back at the old stride.
                let nl = usize::from(new_levels);
                for &i in &matching {
                    let key = &mut self.keys[i];
                    let (width, groups) = (usize::from(key.width), key.num_groups());
                    let mut buf = vec![0u32; groups * nl];
                    for g in (0..groups).rev() {
                        for l in 0..width {
                            buf[nl * g + l] = key.syms.get(width * g + l).copied().unwrap_or(0);
                        }
                    }
                    buf.truncate(groups * width);
                    key.syms = buf;
                }
                self.types[ndx].num_levels = new_levels;
                return;
            }
        } else if new_levels < old_levels {
            for i in usize::from(self.min_key_code)..=usize::from(self.max_key_code) {
                let key = &self.keys[i];
                if key.width < old_levels {
                    continue;
                }
                if uses(key) {
                    matching.push(i);
                }
            }
        }
        let first_clear = usize::from(new_levels.min(old_levels));
        for &i in &matching {
            let key = &mut self.keys[i];
            let width = usize::from(key.width);
            let n_clear = width.saturating_sub(first_clear);
            for g in (0..key.num_groups()).rev() {
                if usize::from(key.kt_index[g & 3]) == ndx && n_clear > 0 {
                    let len = key.syms.len();
                    let (from, to) = (
                        (g * width + first_clear).min(len),
                        (g * width + width).min(len),
                    );
                    key.syms[from..to].fill(0);
                }
            }
        }
        self.types[ndx].num_levels = new_levels;
    }

    /// Port of `SetKeyTypes`: `num_types` only grows; a new index starts
    /// as a zeroed type (no name, no level names).
    fn set_key_types(&mut self, h: &SetMapHeader, types: &[WireType], mc: &mut MapChanges) {
        let end = usize::from(h.first_type) + usize::from(h.n_types);
        while self.types.len() < end {
            self.types.push(KeyType {
                mods: Mods::default(),
                num_levels: 0,
                map: Vec::new(),
                preserve: None,
                name: None,
                level_names: None,
            });
        }
        for (i, wt) in types.iter().enumerate() {
            let ndx = usize::from(h.first_type) + i;
            self.resize_key_type(ndx, wt.entries.len(), wt.preserve.is_some(), wt.num_levels);
            let mask = wt.real | self.vmods_to_real(wt.vmods);
            let map: Vec<KtEntry> = wt
                .entries
                .iter()
                .map(|&(level, real, vmods)| {
                    let mut e = KtEntry {
                        active: true,
                        mods: Mods {
                            mask: real,
                            real,
                            vmods,
                        },
                        level,
                    };
                    if vmods != 0 {
                        let tmp = self.vmods_to_real(vmods);
                        e.active = tmp != 0;
                        e.mods.mask |= tmp;
                    }
                    e
                })
                .collect();
            let preserve = wt.preserve.as_ref().filter(|_| !map.is_empty()).map(|p| {
                p.iter()
                    .map(|&(real, vmods)| Mods {
                        mask: real | self.vmods_to_real(vmods),
                        real,
                        vmods,
                    })
                    .collect()
            });
            let t = &mut self.types[ndx];
            t.mods = Mods {
                mask,
                real: wt.real,
                vmods: wt.vmods,
            };
            t.num_levels = wt.num_levels;
            t.map = map;
            t.preserve = preserve;
        }
        let first = u32::from(h.first_type);
        let last = (first + u32::from(h.n_types)).wrapping_sub(1);
        (mc.first_type, mc.num_types) = merge_range(
            mc.changed & KEY_TYPES_MASK != 0,
            mc.first_type,
            mc.num_types,
            first,
            last,
            |old, first| old < first,
        );
        mc.changed |= KEY_TYPES_MASK;
    }

    /// Port of `SetKeySyms`; the group count follows the keys (it can
    /// shrink here). Xorg's ControlsNotify for it reaches nobody (its
    /// changedControls is 0), so none is sent.
    fn set_key_syms(&mut self, h: &SetMapHeader, key_syms: &[KeySyms], mc: &mut MapChanges) {
        for (i, wire) in key_syms.iter().enumerate() {
            let kc = h.first_key_sym.wrapping_add(u8::try_from(i).unwrap_or(0));
            let k = usize::from(kc);
            if !wire.syms.is_empty() {
                self.keys[k].syms.clone_from(&wire.syms);
            }
            if self.key_has_actions(kc) {
                self.resize_key_actions_xorg(kc, wire.num_syms());
            }
            let key = &mut self.keys[k];
            key.kt_index = wire.kt_index;
            key.group_info = wire.group_info;
            key.width = wire.width;
        }
        let first = u32::from(h.first_key_sym);
        let last = (first + u32::from(h.n_key_syms)).wrapping_sub(1);
        (mc.first_key_sym, mc.num_key_syms) = merge_range(
            mc.changed & KEY_SYMS_MASK != 0,
            mc.first_key_sym,
            mc.num_key_syms,
            first,
            last,
            |old, first| old < first,
        );
        mc.changed |= KEY_SYMS_MASK;
        let groups = (self.min_key_code..=self.max_key_code)
            .map(|kc| self.keys[usize::from(kc)].num_groups())
            .max()
            .unwrap_or(0);
        self.num_groups = u8::try_from(groups).unwrap_or(4);
    }

    /// Port of `SetKeyActions`.
    fn set_key_actions(&mut self, h: &SetMapHeader, acts: &[Vec<Action>], mc: &mut MapChanges) {
        for (i, a) in acts.iter().enumerate() {
            let kc = h.first_key_act.wrapping_add(u8::try_from(i).unwrap_or(0));
            if a.is_empty() {
                self.acts[usize::from(kc)] = None;
            } else {
                self.resize_key_actions_xorg(kc, a.len());
                let v = self.acts[usize::from(kc)].get_or_insert_with(Vec::new);
                if v.len() < a.len() {
                    v.resize(a.len(), [0; 8]);
                }
                v[..a.len()].copy_from_slice(a);
            }
        }
        let first = u32::from(h.first_key_act);
        let last = (first + u32::from(h.n_key_acts)).wrapping_sub(1);
        (mc.first_key_act, mc.num_key_acts) = merge_range(
            mc.changed & KEY_ACTIONS_MASK != 0,
            mc.first_key_act,
            mc.num_key_acts,
            first,
            last,
            |old, first| old < first,
        );
        mc.changed |= KEY_ACTIONS_MASK;
    }

    /// Port of `SetKeyBehaviors`. The range is zeroed first, so a
    /// permanent behavior in it is replaced too. (Xorg also grows
    /// `xkbi->nRadioGroups` here, runtime state no request reads back.)
    fn set_key_behaviors(
        &mut self,
        h: &SetMapHeader,
        behaviors: &[(u8, u8, u8)],
        mc: &mut MapChanges,
    ) {
        let first = usize::from(h.first_key_behavior);
        for b in &mut self.behaviors[first..first + usize::from(h.n_key_behaviors)] {
            *b = Behavior::default();
        }
        for &(key, kind, data) in behaviors {
            let cur = &mut self.behaviors[usize::from(key)];
            if cur.kind & KB_PERMANENT == 0 {
                *cur = Behavior { kind, data };
            }
        }
        let first = u32::from(h.first_key_behavior);
        let last = first + u32::from(h.n_key_behaviors) - 1;
        let req_first = first;
        (mc.first_key_behavior, mc.num_key_behaviors) = merge_range(
            mc.changed & KEY_BEHAVIORS_MASK != 0,
            mc.first_key_behavior,
            mc.num_key_behaviors,
            first,
            last,
            move |old, _| old < req_first,
        );
        mc.changed |= KEY_BEHAVIORS_MASK;
    }

    /// Port of `SetVirtualMods`: each sent mapping that differs is stored
    /// and marked changed.
    fn set_virtual_mods(&mut self, h: &SetMapHeader, vmods: &[u8], mc: &mut MapChanges) {
        if h.virtual_mods == 0 {
            return;
        }
        let mut n = 0;
        for i in 0..NUM_VMODS {
            let bit = 1u16 << i;
            if h.virtual_mods & bit != 0 {
                let v = vmods.get(n).copied().unwrap_or(0);
                if self.vmods[i] != v {
                    mc.changed |= VIRTUAL_MODS_MASK;
                    mc.vmods |= bit;
                    self.vmods[i] = v;
                }
                n += 1;
            }
        }
    }

    /// The shared tail of `SetKeyExplicit` / `SetModifierMap` /
    /// `SetVirtualModMap`: the range and its first/num fields — but never
    /// the `changed` bit (Xorg's quirk: MapNotify reports the range without
    /// the part).
    fn quiet_range(changed: bool, first_field: &mut u8, num_field: &mut u8, first: u8, num: u8) {
        if first > 0 {
            let f = u32::from(first);
            let last = f + u32::from(num) - 1;
            (*first_field, *num_field) =
                merge_range(changed, *first_field, *num_field, f, last, |old, f| old < f);
        }
    }

    /// Port of `_XkbSetMap` after its checks: the parts applied in wire
    /// order, then the recompute.
    pub(crate) fn set_map(&mut self, h: &SetMapHeader, m: &SetMap) -> SetMapResult {
        let mut changes = XkbChanges::default();
        let mut nkn = None;
        if self.min_key_code != h.min_key_code || self.max_key_code != h.max_key_code {
            let (old_min, old_max) = (self.min_key_code, self.max_key_code);
            self.change_keycode_range(h.min_key_code, h.max_key_code, &mut changes.map);
            nkn = Some(KeycodeRangeChange {
                old_min,
                old_max,
                min: self.min_key_code,
                max: self.max_key_code,
            });
        }
        let mc = &mut changes.map;
        if h.present & KEY_TYPES_MASK != 0 {
            self.set_key_types(h, &m.types, mc);
        }
        if h.present & KEY_SYMS_MASK != 0 {
            self.set_key_syms(h, &m.key_syms, mc);
        }
        if h.present & KEY_ACTIONS_MASK != 0 {
            self.set_key_actions(h, &m.acts, mc);
        }
        if h.present & KEY_BEHAVIORS_MASK != 0 {
            self.set_key_behaviors(h, &m.behaviors, mc);
        }
        if h.present & VIRTUAL_MODS_MASK != 0 {
            self.set_virtual_mods(h, &m.vmods, mc);
        }
        if h.present & EXPLICIT_COMPONENTS_MASK != 0 {
            let first = usize::from(h.first_key_explicit);
            self.explicit[first..first + usize::from(h.n_key_explicit)].fill(0);
            for &(key, bits) in &m.explicit {
                self.explicit[usize::from(key)] = bits;
            }
            Self::quiet_range(
                mc.changed & EXPLICIT_COMPONENTS_MASK != 0,
                &mut mc.first_key_explicit,
                &mut mc.num_key_explicit,
                h.first_key_explicit,
                h.n_key_explicit,
            );
        }
        if h.present & MODIFIER_MAP_MASK != 0 {
            let first = usize::from(h.first_mod_map_key);
            self.modmap[first..first + usize::from(h.n_mod_map_keys)].fill(0);
            for &(key, mods) in &m.modmap {
                self.modmap[usize::from(key)] = mods;
            }
            Self::quiet_range(
                mc.changed & MODIFIER_MAP_MASK != 0,
                &mut mc.first_modmap_key,
                &mut mc.num_modmap_keys,
                h.first_mod_map_key,
                h.n_mod_map_keys,
            );
        }
        if h.present & VIRTUAL_MOD_MAP_MASK != 0 {
            let first = usize::from(h.first_vmod_map_key);
            self.vmodmap[first..first + usize::from(h.n_vmod_map_keys)].fill(0);
            for &(key, vmods) in &m.vmodmap {
                self.vmodmap[usize::from(key)] = vmods;
            }
            Self::quiet_range(
                mc.changed & VIRTUAL_MOD_MAP_MASK != 0,
                &mut mc.first_vmodmap_key,
                &mut mc.num_vmodmap_keys,
                h.first_vmod_map_key,
                h.n_vmod_map_keys,
            );
        }
        if m.end / 4 != h.length / 4 {
            return SetMapResult {
                changes,
                range: nkn,
                bad_length: Some(u32::try_from(m.end.wrapping_sub(SET_MAP_REQ_SIZE)).unwrap_or(0)),
            };
        }
        if h.flags & RECOMPUTE_ACTIONS != 0 {
            let mc = changes.map;
            let span = |first: u8, num: u8| {
                if num > 0 {
                    (first, first.wrapping_add(num).wrapping_sub(1))
                } else {
                    (0, 0)
                }
            };
            let (mut first, mut last) = span(mc.first_key_sym, mc.num_key_syms);
            let (first_mm, last_mm) = span(mc.first_modmap_key, mc.num_modmap_keys);
            if last > 0 && last_mm > 0 {
                first = first.min(first_mm);
                last = last.max(last_mm);
            } else if last_mm > 0 {
                (first, last) = (first_mm, last_mm);
            }
            if last > 0 {
                let num = last.wrapping_sub(first).wrapping_add(1);
                self.update_desc_actions(first, num, &mut changes);
            }
        }
        SetMapResult {
            changes,
            range: nkn,
            bad_length: None,
        }
    }
}

/// A whole-description SetMap of `desc` (every part, every key), as a
/// client that read GetMap would send it back: the decoder's round-trip
/// input.
#[cfg(test)]
pub(crate) fn encode(desc: &XkbDesc, flags: u16) -> Vec<u8> {
    let (min, max) = (desc.min_key_code, desc.max_key_code);
    let keys = || min..=max;
    let mut body = Vec::new();
    for t in &desc.types {
        body.extend_from_slice(&[t.mods.mask, t.mods.real]);
        body.extend_from_slice(&t.mods.vmods.to_le_bytes());
        body.push(t.num_levels);
        body.push(u8::try_from(t.map.len()).unwrap());
        body.push(u8::from(t.preserve.is_some()));
        body.push(0);
        for e in &t.map {
            body.extend_from_slice(&[e.level, e.mods.real]);
            body.extend_from_slice(&e.mods.vmods.to_le_bytes());
        }
        if let Some(p) = &t.preserve {
            for m in p {
                body.extend_from_slice(&[m.mask, m.real]);
                body.extend_from_slice(&m.vmods.to_le_bytes());
            }
        }
    }
    let mut total_syms = 0usize;
    for kc in keys() {
        let k = &desc.keys[usize::from(kc)];
        let n = k.num_syms();
        body.extend_from_slice(&k.kt_index);
        body.extend_from_slice(&[k.group_info, k.width]);
        body.extend_from_slice(&u16::try_from(n).unwrap().to_le_bytes());
        for s in 0..n {
            body.extend_from_slice(&k.syms.get(s).copied().unwrap_or(0).to_le_bytes());
        }
        total_syms += n;
    }
    let counts: Vec<u8> = keys()
        .map(|kc| {
            if desc.key_has_actions(kc) {
                u8::try_from(desc.keys[usize::from(kc)].num_syms()).unwrap()
            } else {
                0
            }
        })
        .collect();
    body.extend_from_slice(&counts);
    body.resize(body.len() + padded(counts.len()) - counts.len(), 0);
    for (kc, &n) in keys().zip(&counts) {
        for s in 0..usize::from(n) {
            body.extend_from_slice(&desc.key_action(kc, s));
        }
    }
    let total_acts: usize = counts.iter().map(|&c| usize::from(c)).sum();
    let behaviors: Vec<u8> = keys()
        .filter(|&kc| desc.behaviors[usize::from(kc)].kind != 0)
        .collect();
    for &kc in &behaviors {
        let b = desc.behaviors[usize::from(kc)];
        body.extend_from_slice(&[kc, b.kind, b.data, 0]);
    }
    body.extend_from_slice(&desc.vmods);
    let pairs = |body: &mut Vec<u8>, v: &[(u8, u8)]| {
        let start = body.len();
        for &(k, x) in v {
            body.extend_from_slice(&[k, x]);
        }
        let n = body.len() - start;
        body.resize(start + padded(n), 0);
    };
    let explicit: Vec<(u8, u8)> = keys()
        .map(|kc| (kc, desc.explicit[usize::from(kc)]))
        .filter(|&(_, e)| e != 0)
        .collect();
    pairs(&mut body, &explicit);
    let modmap: Vec<(u8, u8)> = keys()
        .map(|kc| (kc, desc.modmap[usize::from(kc)]))
        .filter(|&(_, m)| m != 0)
        .collect();
    pairs(&mut body, &modmap);
    let vmodmap: Vec<(u8, u16)> = keys()
        .map(|kc| (kc, desc.vmodmap[usize::from(kc)]))
        .filter(|&(_, v)| v != 0)
        .collect();
    for &(kc, v) in &vmodmap {
        body.push(kc);
        body.push(0);
        body.extend_from_slice(&v.to_le_bytes());
    }
    let n = desc.num_keys();
    let byte = |v: usize| u8::try_from(v).unwrap();
    let mut req = vec![0, 9, 0, 0];
    req.extend_from_slice(&0x100u16.to_le_bytes());
    req.extend_from_slice(&ALL_MAP_COMPONENTS.to_le_bytes());
    req.extend_from_slice(&flags.to_le_bytes());
    req.extend_from_slice(&[min, max, 0, byte(desc.types.len()), min, n]);
    req.extend_from_slice(&u16::try_from(total_syms).unwrap().to_le_bytes());
    req.extend_from_slice(&[min, n]);
    req.extend_from_slice(&u16::try_from(total_acts).unwrap().to_le_bytes());
    req.extend_from_slice(&[min, n, byte(behaviors.len())]);
    req.extend_from_slice(&[min, n, byte(explicit.len())]);
    req.extend_from_slice(&[min, n, byte(modmap.len())]);
    req.extend_from_slice(&[min, n, byte(vmodmap.len())]);
    req.extend_from_slice(&0xffffu16.to_le_bytes());
    req.extend_from_slice(&body);
    let words = u16::try_from(req.len() / 4).unwrap();
    req[2..4].copy_from_slice(&words.to_le_bytes());
    req
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kms::xkb_desc::tests::{FIXTURES, seeded};

    /// The nine recorded xkbcomp uploads' SetMap (Xvfb 21.1.24,
    /// `xorg-xkbcomp-steps.txt`).
    pub(crate) const CASES: [&str; 9] = [
        "identity", "swap", "newtype", "droptype", "compat", "capsctrl", "explicit", "range",
        "usru",
    ];

    pub(crate) fn recorded_set_map(case: &str) -> Vec<u8> {
        let path = format!(
            "{}/src/kms/testdata/xkbcomp-requests/{case}/1-SetMap.bin",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    fn checked(desc: &XkbDesc, req: &[u8]) -> (SetMapHeader, SetMap) {
        let mut h = SetMapHeader::parse(req);
        check_present(&h).expect("present");
        check_length(&h, req).expect("length");
        check(desc, &mut h, req, false).expect("checks");
        let m = decode(&h, req).expect("decodes");
        assert_eq!(m.end, h.length, "the parts end at the request length");
        (h, m)
    }

    /// Decoder round trip: GetMap's view of each fixture sent back as a
    /// SetMap passes Xorg's length and request checks, and decodes to the
    /// fixture's own types, keysyms, actions, behaviors, virtual modifiers,
    /// explicit components, modifier map and virtual modifier map.
    #[test]
    fn set_map_of_a_description_decodes_to_it() {
        for (layout, options, case) in FIXTURES {
            let desc = seeded(layout, options);
            let req = encode(&desc, 0);
            let (h, m) = checked(&desc, &req);
            assert_eq!(h.present, ALL_MAP_COMPONENTS, "{case}");
            let types: Vec<WireType> = desc
                .types
                .iter()
                .map(|t| WireType {
                    real: t.mods.real,
                    vmods: t.mods.vmods,
                    num_levels: t.num_levels,
                    entries: t
                        .map
                        .iter()
                        .map(|e| (e.level, e.mods.real, e.mods.vmods))
                        .collect(),
                    preserve: t
                        .preserve
                        .as_ref()
                        .map(|p| p.iter().map(|m| (m.real, m.vmods)).collect()),
                })
                .collect();
            assert_eq!(m.types, types, "{case}: types");
            let keys = || desc.min_key_code..=desc.max_key_code;
            for (kc, k) in keys().zip(&m.key_syms) {
                let d = &desc.keys[usize::from(kc)];
                assert_eq!(
                    (k.kt_index, k.group_info, k.width, &k.syms[..]),
                    (d.kt_index, d.group_info, d.width, &d.syms[..d.num_syms()]),
                    "{case}: keysyms of {kc}"
                );
            }
            for (kc, a) in keys().zip(&m.acts) {
                let want: Vec<Action> = desc.acts[usize::from(kc)]
                    .as_ref()
                    .map(|v| v[..desc.keys[usize::from(kc)].num_syms()].to_vec())
                    .unwrap_or_default();
                assert_eq!(a, &want, "{case}: actions of {kc}");
            }
            let behaviors: Vec<(u8, u8, u8)> = keys()
                .filter(|&kc| desc.behaviors[usize::from(kc)].kind != 0)
                .map(|kc| {
                    let b = desc.behaviors[usize::from(kc)];
                    (kc, b.kind, b.data)
                })
                .collect();
            assert_eq!(m.behaviors, behaviors, "{case}: behaviors");
            assert_eq!(m.vmods, desc.vmods.to_vec(), "{case}: vmods");
            let nonzero = |v: &[u8]| -> Vec<(u8, u8)> {
                keys()
                    .map(|kc| (kc, v[usize::from(kc)]))
                    .filter(|&(_, x)| x != 0)
                    .collect()
            };
            assert_eq!(m.explicit, nonzero(&desc.explicit), "{case}: explicit");
            assert_eq!(m.modmap, nonzero(&desc.modmap), "{case}: modmap");
            let vmodmap: Vec<(u8, u16)> = keys()
                .map(|kc| (kc, desc.vmodmap[usize::from(kc)]))
                .filter(|&(_, v)| v != 0)
                .collect();
            assert_eq!(m.vmodmap, vmodmap, "{case}: vmodmap");
        }
    }

    /// The recorded xkbcomp SetMap requests pass Xorg's checks against the
    /// gb server they were sent to (Xorg accepted each, `= ok`) and decode
    /// to the end of the request, with the headers the golden decodes.
    #[test]
    fn recorded_set_maps_decode() {
        let desc = seeded("gb", None);
        let golden = include_str!("../testdata/xorg-xkbcomp-steps.txt");
        for case in CASES {
            let req = recorded_set_map(case);
            let (h, m) = checked(&desc, &req);
            let line = golden
                .lines()
                .skip_while(|l| *l != format!("## case {case}"))
                .find(|l| l.contains("XKEYBOARD-Request(9): SetMap"))
                .expect("decoded header");
            let want = format!(
                "present=0x{:04x} flags=0x{:x} minKeyCode={} maxKeyCode={} types={}+{} syms={}+{} total={} acts={}+{} total={}",
                h.present,
                h.flags,
                h.min_key_code,
                h.max_key_code,
                h.first_type,
                h.n_types,
                h.first_key_sym,
                h.n_key_syms,
                h.total_syms,
                h.first_key_act,
                h.n_key_acts,
                h.total_acts
            );
            assert!(line.contains(&want), "{case}: {line}\n  vs {want}");
            assert_eq!(m.types.len(), usize::from(h.n_types), "{case}");
            assert_eq!(m.key_syms.len(), usize::from(h.n_key_syms), "{case}");
            let total: usize = m.key_syms.iter().map(|k| k.syms.len()).sum();
            assert_eq!(total, usize::from(h.total_syms), "{case}");
        }
    }
}
