//! XKB replies encoded from the model, as Xorg's `ProcXkbGet*` /
//! `XkbSend*` (xkb/xkb.c) build them: GetMap, GetCompatMap, GetNames,
//! GetIndicatorMap, GetNamedIndicator, GetControls and GetKbdByName's
//! nested blocks.

use super::{
    EXPLICIT_COMPONENTS_MASK, KB_DEFAULT, KEY_ACTIONS_MASK, KEY_BEHAVIORS_MASK, KEY_SYMS_MASK,
    KEY_TYPES_MASK, MODIFIER_MAP_MASK, NUM_GROUPS, NUM_INDICATORS, NUM_VMODS, VIRTUAL_MOD_MAP_MASK,
    VIRTUAL_MODS_MASK, XkbDesc, padded,
};

/// `XkbAllMapComponentsMask`.
const ALL_MAP_COMPONENTS: u16 = 0xff;
/// `XkbAllNamesMask`.
const ALL_NAMES: u32 = 0x3fff;
/// GetNames parts (XKB.h `Xkb*NamesMask`); note the section order in a
/// reply differs from the bit order for the last five.
const INDICATOR_NAMES: u32 = 1 << 8;
const KEY_NAMES: u32 = 1 << 9;
const KEY_ALIASES: u32 = 1 << 10;
const VIRTUAL_MOD_NAMES: u32 = 1 << 11;
const GROUP_NAMES: u32 = 1 << 12;
const RG_NAMES: u32 = 1 << 13;
/// `XkbNoIndicator`.
const NO_INDICATOR: u8 = 0xff;
/// X11 error codes.
pub(crate) const BAD_VALUE: u8 = 2;
pub(crate) const BAD_MATCH: u8 = 8;

/// An X error a request draws (Xorg's return code and `errorValue`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct XkbError {
    pub code: u8,
    pub value: u32,
}

/// `_XkbErrCode2`.
fn err_code2(a: u32, b: u32) -> u32 {
    (a << 24) | (b & 0x00ff_ffff)
}

/// `_XkbErrCode3`.
fn err_code3(a: u32, b: u32, c: u32) -> u32 {
    err_code2(a, (b << 16) | c)
}

/// `_XkbErrCode4`.
fn err_code4(a: u32, b: u32, c: u32, d: u32) -> u32 {
    err_code3(a, b, (c << 8) | d)
}

/// The 32-byte X error packet for `err` on XKB request `minor` (sequence
/// filled in by the caller).
pub(crate) fn error_packet(err: XkbError, major: u8, minor: u8) -> Vec<u8> {
    let mut r = vec![0u8; 32];
    r[1] = err.code;
    r[4..8].copy_from_slice(&err.value.to_le_bytes());
    r[8..10].copy_from_slice(&u16::from(minor).to_le_bytes());
    r[10] = major;
    r
}

/// The ranges and parts of one GetMap reply (`xkbGetMapReply` before
/// `XkbComputeGetMapReplySize` trims it).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct MapRequest {
    pub present: u16,
    pub first_type: u8,
    pub n_types: u8,
    pub first_key_sym: u8,
    pub n_key_syms: u8,
    pub first_key_act: u8,
    pub n_key_acts: u8,
    pub first_key_behavior: u8,
    pub n_key_behaviors: u8,
    pub virtual_mods: u16,
    pub first_key_explicit: u8,
    pub n_key_explicit: u8,
    pub first_mod_map_key: u8,
    pub n_mod_map_keys: u8,
    pub first_vmod_map_key: u8,
    pub n_vmod_map_keys: u8,
}

impl MapRequest {
    /// Every part of the whole description (`full = XkbAllMapComponentsMask`).
    pub(crate) fn full(desc: &XkbDesc) -> Self {
        let (min, n) = (desc.min_key_code, desc.num_keys());
        Self {
            present: ALL_MAP_COMPONENTS,
            first_type: 0,
            n_types: u8::try_from(desc.types.len()).unwrap_or(u8::MAX),
            first_key_sym: min,
            n_key_syms: n,
            first_key_act: min,
            n_key_acts: n,
            first_key_behavior: min,
            n_key_behaviors: n,
            virtual_mods: 0xffff,
            first_key_explicit: min,
            n_key_explicit: n,
            first_mod_map_key: min,
            n_mod_map_keys: n,
            first_vmod_map_key: min,
            n_vmod_map_keys: n,
        }
    }
}

/// `CHK_KEY_RANGE`.
fn chk_key_range(err: u32, first: u8, num: u8, desc: &XkbDesc) -> Result<(), XkbError> {
    if u32::from(first) + u32::from(num) - 1 > u32::from(desc.max_key_code) {
        return Err(XkbError {
            code: BAD_VALUE,
            value: err_code4(
                err,
                u32::from(first),
                u32::from(num),
                u32::from(desc.max_key_code),
            ),
        });
    }
    if first < desc.min_key_code {
        return Err(XkbError {
            code: BAD_VALUE,
            value: err_code3(err + 1, u32::from(first), u32::from(desc.min_key_code)),
        });
    }
    Ok(())
}

/// Port of `ProcXkbGetMap`'s request handling: the reply's parts and
/// ranges for a GetMap request `body` (after the 4-byte header).
pub(crate) fn get_map_request(desc: &XkbDesc, body: &[u8]) -> Result<MapRequest, XkbError> {
    let b = |i: usize| body.get(i).copied().unwrap_or(0);
    let full = u16::from_le_bytes([b(2), b(3)]);
    let partial = u16::from_le_bytes([b(4), b(5)]);
    if full & partial != 0 {
        return Err(XkbError {
            code: BAD_MATCH,
            value: err_code2(0x01, u32::from(full & partial)),
        });
    }
    if full & !ALL_MAP_COMPONENTS != 0 {
        return Err(XkbError {
            code: BAD_VALUE,
            value: err_code2(0x02, u32::from(full & !ALL_MAP_COMPONENTS)),
        });
    }
    if partial & !ALL_MAP_COMPONENTS != 0 {
        return Err(XkbError {
            code: BAD_VALUE,
            value: err_code2(0x03, u32::from(partial & !ALL_MAP_COMPONENTS)),
        });
    }
    let all = MapRequest::full(desc);
    let mut rep = MapRequest {
        present: partial | full,
        ..MapRequest::default()
    };
    let num_types = u8::try_from(desc.types.len()).unwrap_or(u8::MAX);
    if full & KEY_TYPES_MASK != 0 {
        rep.n_types = num_types;
    } else if partial & KEY_TYPES_MASK != 0 {
        if u32::from(b(6)) + u32::from(b(7)) > u32::from(num_types) {
            return Err(XkbError {
                code: BAD_VALUE,
                value: err_code4(0x04, u32::from(num_types), u32::from(b(6)), u32::from(b(7))),
            });
        }
        rep.first_type = b(6);
        rep.n_types = b(7);
    }
    let parts: [(u16, usize, u32); 6] = [
        (KEY_SYMS_MASK, 8, 0x05),
        (KEY_ACTIONS_MASK, 10, 0x07),
        (KEY_BEHAVIORS_MASK, 12, 0x09),
        (EXPLICIT_COMPONENTS_MASK, 16, 0x0b),
        (MODIFIER_MAP_MASK, 18, 0x0d),
        (VIRTUAL_MOD_MAP_MASK, 20, 0x0f),
    ];
    for (mask, at, err) in parts {
        let (first, n) = if full & mask != 0 {
            (all.first_key_sym, all.n_key_syms)
        } else if partial & mask != 0 {
            chk_key_range(err, b(at), b(at + 1), desc)?;
            (b(at), b(at + 1))
        } else {
            (0, 0)
        };
        match mask {
            KEY_SYMS_MASK => (rep.first_key_sym, rep.n_key_syms) = (first, n),
            KEY_ACTIONS_MASK => (rep.first_key_act, rep.n_key_acts) = (first, n),
            KEY_BEHAVIORS_MASK => (rep.first_key_behavior, rep.n_key_behaviors) = (first, n),
            EXPLICIT_COMPONENTS_MASK => (rep.first_key_explicit, rep.n_key_explicit) = (first, n),
            MODIFIER_MAP_MASK => (rep.first_mod_map_key, rep.n_mod_map_keys) = (first, n),
            _ => (rep.first_vmod_map_key, rep.n_vmod_map_keys) = (first, n),
        }
    }
    if full & VIRTUAL_MODS_MASK != 0 {
        rep.virtual_mods = 0xffff;
    } else if partial & VIRTUAL_MODS_MASK != 0 {
        rep.virtual_mods = u16::from_le_bytes([b(14), b(15)]);
    }
    Ok(rep)
}

/// Port of `XkbComputeGetMapReplySize` + `XkbSendMap`: the whole GetMap
/// reply (40-byte header + sections).
pub(crate) fn encode_map(desc: &XkbDesc, req: MapRequest) -> Vec<u8> {
    let mut rep = req;
    let mut body: Vec<u8> = Vec::new();
    // Trim what's empty, as the XkbSize* helpers do.
    if rep.present & KEY_TYPES_MASK == 0 || rep.n_types < 1 {
        rep.present &= !KEY_TYPES_MASK;
        rep.first_type = 0;
        rep.n_types = 0;
    }
    if rep.present & KEY_SYMS_MASK == 0 || rep.n_key_syms < 1 {
        rep.present &= !KEY_SYMS_MASK;
        rep.first_key_sym = 0;
        rep.n_key_syms = 0;
    }
    if rep.present & KEY_ACTIONS_MASK == 0 || rep.n_key_acts < 1 {
        rep.present &= !KEY_ACTIONS_MASK;
        rep.first_key_act = 0;
        rep.n_key_acts = 0;
    }
    if rep.present & KEY_BEHAVIORS_MASK == 0 || rep.n_key_behaviors < 1 {
        rep.present &= !KEY_BEHAVIORS_MASK;
        rep.first_key_behavior = 0;
        rep.n_key_behaviors = 0;
    }
    if rep.present & VIRTUAL_MODS_MASK == 0 || rep.virtual_mods == 0 {
        rep.present &= !VIRTUAL_MODS_MASK;
        rep.virtual_mods = 0;
    }
    if rep.present & EXPLICIT_COMPONENTS_MASK == 0 || rep.n_key_explicit < 1 {
        rep.present &= !EXPLICIT_COMPONENTS_MASK;
        rep.first_key_explicit = 0;
        rep.n_key_explicit = 0;
    }
    if rep.present & MODIFIER_MAP_MASK == 0 || rep.n_mod_map_keys < 1 {
        rep.present &= !MODIFIER_MAP_MASK;
        rep.first_mod_map_key = 0;
        rep.n_mod_map_keys = 0;
    }
    if rep.present & VIRTUAL_MOD_MAP_MASK == 0 || rep.n_vmod_map_keys < 1 {
        rep.present &= !VIRTUAL_MOD_MAP_MASK;
        rep.first_vmod_map_key = 0;
        rep.n_vmod_map_keys = 0;
    }
    let keys = |first: u8, n: u8| (0..n).map(move |i| first.wrapping_add(i));

    // KeyTypes.
    for t in desc
        .types
        .iter()
        .skip(usize::from(rep.first_type))
        .take(usize::from(rep.n_types))
    {
        body.push(t.mods.mask);
        body.push(t.mods.real);
        body.extend_from_slice(&t.mods.vmods.to_le_bytes());
        body.push(t.num_levels);
        body.push(u8::try_from(t.map.len()).unwrap_or(u8::MAX));
        body.push(u8::from(t.preserve.is_some()));
        body.push(0);
        for e in &t.map {
            body.push(u8::from(e.active));
            body.push(e.mods.mask);
            body.push(e.level);
            body.push(e.mods.real);
            body.extend_from_slice(&e.mods.vmods.to_le_bytes());
            body.extend_from_slice(&[0, 0]);
        }
        if !t.map.is_empty()
            && let Some(pre) = &t.preserve
        {
            for i in 0..t.map.len() {
                let p = pre.get(i).copied().unwrap_or_default();
                body.push(p.mask);
                body.push(p.real);
                body.extend_from_slice(&p.vmods.to_le_bytes());
            }
        }
    }
    // KeySyms.
    let mut total_syms = 0usize;
    for kc in keys(rep.first_key_sym, rep.n_key_syms) {
        let k = &desc.keys[usize::from(kc)];
        let n = k.num_syms();
        body.extend_from_slice(&k.kt_index);
        body.push(k.group_info);
        body.push(k.width);
        body.extend_from_slice(&u16::try_from(n).unwrap_or(u16::MAX).to_le_bytes());
        for i in 0..n {
            body.extend_from_slice(&k.syms.get(i).copied().unwrap_or(0).to_le_bytes());
        }
        total_syms += n;
    }
    // KeyActions.
    let mut total_acts = 0usize;
    if rep.n_key_acts > 0 {
        let counts: Vec<u8> = keys(rep.first_key_act, rep.n_key_acts)
            .map(|kc| {
                if desc.key_has_actions(kc) {
                    u8::try_from(desc.keys[usize::from(kc)].num_syms()).unwrap_or(u8::MAX)
                } else {
                    0
                }
            })
            .collect();
        body.extend_from_slice(&counts);
        body.resize(body.len() + padded(counts.len()) - counts.len(), 0);
        for (kc, &n) in keys(rep.first_key_act, rep.n_key_acts).zip(&counts) {
            for slot in 0..usize::from(n) {
                body.extend_from_slice(&desc.key_action(kc, slot));
            }
            total_acts += usize::from(n);
        }
    }
    // KeyBehaviors.
    let mut total_beh = 0usize;
    for kc in keys(rep.first_key_behavior, rep.n_key_behaviors) {
        let b = desc.behaviors[usize::from(kc)];
        if b.kind != KB_DEFAULT {
            body.extend_from_slice(&[kc, b.kind, b.data, 0]);
            total_beh += 1;
        }
    }
    // VirtualMods.
    if rep.virtual_mods != 0 {
        let mut n = 0;
        for i in 0..NUM_VMODS {
            if rep.virtual_mods & (1 << i) != 0 {
                body.push(desc.vmods[i]);
                n += 1;
            }
        }
        body.resize(body.len() + padded(n) - n, 0);
    }
    // ExplicitComponents.
    let mut total_expl = 0usize;
    if rep.n_key_explicit > 0 {
        let start = body.len();
        for kc in keys(rep.first_key_explicit, rep.n_key_explicit) {
            let e = desc.explicit[usize::from(kc)];
            if e != 0 {
                body.extend_from_slice(&[kc, e]);
                total_expl += 1;
            }
        }
        let n = body.len() - start;
        body.resize(start + padded(n), 0);
    }
    // ModifierMap.
    let mut total_mm = 0usize;
    if rep.n_mod_map_keys > 0 {
        let start = body.len();
        for kc in keys(rep.first_mod_map_key, rep.n_mod_map_keys) {
            let m = desc.modmap[usize::from(kc)];
            if m != 0 {
                body.extend_from_slice(&[kc, m]);
                total_mm += 1;
            }
        }
        let n = body.len() - start;
        body.resize(start + padded(n), 0);
    }
    // VirtualModMap.
    let mut total_vmm = 0usize;
    for kc in keys(rep.first_vmod_map_key, rep.n_vmod_map_keys) {
        let v = desc.vmodmap[usize::from(kc)];
        if v != 0 {
            body.push(kc);
            body.push(0);
            body.extend_from_slice(&v.to_le_bytes());
            total_vmm += 1;
        }
    }

    let mut r = vec![0u8; 40];
    r[0] = 1;
    r[1] = 1;
    let length = u32::try_from((8 + body.len()) / 4).unwrap_or(u32::MAX);
    r[4..8].copy_from_slice(&length.to_le_bytes());
    r[10] = desc.min_key_code;
    r[11] = desc.max_key_code;
    r[12..14].copy_from_slice(&rep.present.to_le_bytes());
    r[14] = rep.first_type;
    r[15] = rep.n_types;
    r[16] = u8::try_from(desc.types.len()).unwrap_or(u8::MAX);
    r[17] = rep.first_key_sym;
    r[18..20].copy_from_slice(&u16::try_from(total_syms).unwrap_or(u16::MAX).to_le_bytes());
    r[20] = rep.n_key_syms;
    r[21] = rep.first_key_act;
    r[22..24].copy_from_slice(&u16::try_from(total_acts).unwrap_or(u16::MAX).to_le_bytes());
    r[24] = rep.n_key_acts;
    r[25] = rep.first_key_behavior;
    r[26] = rep.n_key_behaviors;
    r[27] = u8::try_from(total_beh).unwrap_or(u8::MAX);
    r[28] = rep.first_key_explicit;
    r[29] = rep.n_key_explicit;
    r[30] = u8::try_from(total_expl).unwrap_or(u8::MAX);
    r[31] = rep.first_mod_map_key;
    r[32] = rep.n_mod_map_keys;
    r[33] = u8::try_from(total_mm).unwrap_or(u8::MAX);
    r[34] = rep.first_vmod_map_key;
    r[35] = rep.n_vmod_map_keys;
    r[36] = u8::try_from(total_vmm).unwrap_or(u8::MAX);
    r[38..40].copy_from_slice(&rep.virtual_mods.to_le_bytes());
    r.extend_from_slice(&body);
    r
}

/// GetMap (minor 8) for a request `body`.
pub(crate) fn reply_get_map(desc: &XkbDesc, body: &[u8]) -> Result<Vec<u8>, XkbError> {
    Ok(encode_map(desc, get_map_request(desc, body)?))
}

/// GetCompatMap (minor 10): `ProcXkbGetCompatMap` + `XkbSendCompatMap`.
/// Request body: deviceSpec(2) groups(1) getAllSI(1) firstSI(2) nSI(2).
pub(crate) fn reply_get_compat_map(desc: &XkbDesc, body: &[u8]) -> Result<Vec<u8>, XkbError> {
    let b = |i: usize| body.get(i).copied().unwrap_or(0);
    let groups = b(2);
    let get_all = b(3) != 0;
    let num_si = desc.compat.len();
    let (first, n) = if get_all {
        (0usize, num_si)
    } else {
        let first = usize::from(u16::from_le_bytes([b(4), b(5)]));
        let n = usize::from(u16::from_le_bytes([b(6), b(7)]));
        if n > 0 && first + n > num_si {
            return Err(XkbError {
                code: BAD_VALUE,
                value: err_code2(0x05, u32::try_from(num_si).unwrap_or(0)),
            });
        }
        (first, n)
    };
    Ok(encode_compat_map(desc, groups, first, n))
}

/// The GetCompatMap reply for interprets `first..first+n` and `groups`.
pub(crate) fn encode_compat_map(desc: &XkbDesc, groups: u8, first: usize, n: usize) -> Vec<u8> {
    let mut body = Vec::new();
    for si in desc.compat.iter().skip(first).take(n) {
        body.extend_from_slice(&si.sym.to_le_bytes());
        body.push(si.mods);
        body.push(si.match_);
        body.push(si.virtual_mod);
        body.push(si.flags);
        body.extend_from_slice(&si.act);
    }
    for g in 0..NUM_GROUPS {
        if groups & (1 << g) != 0 {
            let m = desc.group_compat[g];
            body.push(m.mask);
            body.push(m.real);
            body.extend_from_slice(&m.vmods.to_le_bytes());
        }
    }
    let mut r = vec![0u8; 32];
    r[0] = 1;
    r[1] = 1;
    r[4..8].copy_from_slice(&u32::try_from(body.len() / 4).unwrap_or(0).to_le_bytes());
    r[8] = groups;
    r[10..12].copy_from_slice(&u16::try_from(first).unwrap_or(0).to_le_bytes());
    r[12..14].copy_from_slice(&u16::try_from(n).unwrap_or(0).to_le_bytes());
    r[14..16].copy_from_slice(&u16::try_from(desc.compat.len()).unwrap_or(0).to_le_bytes());
    r.extend_from_slice(&body);
    r
}

/// GetIndicatorMap (minor 13) for the indicators in `which`.
pub(crate) fn encode_indicator_map(desc: &XkbDesc, which: u32) -> Vec<u8> {
    let mut body = Vec::new();
    for (i, m) in desc.indicators.iter().enumerate() {
        if which & (1 << i) != 0 {
            body.push(m.flags);
            body.push(m.which_groups);
            body.push(m.groups);
            body.push(m.which_mods);
            body.push(m.mods.mask);
            body.push(m.mods.real);
            body.extend_from_slice(&m.mods.vmods.to_le_bytes());
            body.extend_from_slice(&m.ctrls.to_le_bytes());
        }
    }
    let mut r = vec![0u8; 32];
    r[0] = 1;
    r[1] = 1;
    r[4..8].copy_from_slice(&u32::try_from(body.len() / 4).unwrap_or(0).to_le_bytes());
    r[8..12].copy_from_slice(&which.to_le_bytes());
    r[12..16].copy_from_slice(&desc.phys_indicators.to_le_bytes());
    r[16] = u8::try_from(which.count_ones()).unwrap_or(0);
    r.extend_from_slice(&body);
    r
}

/// GetIndicatorMap for a request body `deviceSpec(2) pad(2) which(4)`;
/// a body too short to carry `which` asks for all of them.
pub(crate) fn reply_get_indicator_map(desc: &XkbDesc, body: &[u8]) -> Vec<u8> {
    let which = body
        .get(4..8)
        .map_or(u32::MAX, |w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]));
    encode_indicator_map(desc, which)
}

/// GetNamedIndicator (minor 15): `ProcXkbGetNamedIndicator`. Body:
/// deviceSpec(2) ledClass(2) ledID(2) pad(2) indicator(4). `lit` is the
/// effective indicator state.
pub(crate) fn reply_get_named_indicator(
    desc: &XkbDesc,
    lit: u32,
    body: &[u8],
    intern_atom: &mut dyn FnMut(&str) -> u32,
) -> Vec<u8> {
    let requested = body
        .get(8..12)
        .map_or(0, |a| u32::from_le_bytes([a[0], a[1], a[2], a[3]]));
    let mut r = vec![0u8; 32];
    r[0] = 1;
    r[1] = 1;
    r[8..12].copy_from_slice(&requested.to_le_bytes());
    r[15] = NO_INDICATOR;
    r[28] = 1; // supported
    for i in 0..NUM_INDICATORS {
        let Some(name) = desc.names.indicators[i].as_deref() else {
            continue;
        };
        if intern_atom(name) != requested {
            continue;
        }
        let m = desc.indicators[i];
        r[12] = 1;
        r[13] = u8::from(lit & (1 << i) != 0);
        r[14] = u8::from(desc.phys_indicators & (1 << i) != 0);
        r[15] = u8::try_from(i).unwrap_or(0);
        r[16] = m.flags;
        r[17] = m.which_groups;
        r[18] = m.groups;
        r[19] = m.which_mods;
        r[20] = m.mods.mask;
        r[21] = m.mods.real;
        r[22..24].copy_from_slice(&m.mods.vmods.to_le_bytes());
        r[24..28].copy_from_slice(&m.ctrls.to_le_bytes());
        break;
    }
    r
}

/// The GetNames reply for `which`, `ProcXkbGetNames` +
/// `XkbComputeGetNamesReplySize` + `XkbSendNames`.
pub(crate) fn encode_names(
    desc: &XkbDesc,
    which: u32,
    intern_atom: &mut dyn FnMut(&str) -> u32,
) -> Vec<u8> {
    let names = &desc.names;
    let mut which = which;
    let mut atom = |n: &Option<String>| n.as_deref().map_or(0, &mut *intern_atom);
    let n_types = desc.types.len();
    let mut body: Vec<u8> = Vec::new();
    let components = [
        &names.keycodes,
        &names.geometry,
        &names.symbols,
        &names.phys_symbols,
        &names.types,
        &names.compat,
    ];
    for (bit, c) in components.iter().enumerate() {
        if which & (1 << bit) != 0 {
            body.extend_from_slice(&atom(c).to_le_bytes());
        }
    }
    if which & (1 << 6) != 0 {
        for t in &desc.types {
            body.extend_from_slice(&atom(&t.name).to_le_bytes());
        }
    }
    let mut n_kt_levels = 0usize;
    if which & (1 << 7) != 0 {
        for t in &desc.types {
            body.push(if t.level_names.is_some() {
                t.num_levels
            } else {
                0
            });
        }
        body.resize(body.len() + padded(n_types) - n_types, 0);
        for t in &desc.types {
            if let Some(ln) = &t.level_names {
                for l in 0..usize::from(t.num_levels) {
                    let a = ln.get(l).cloned().flatten();
                    body.extend_from_slice(&atom(&a).to_le_bytes());
                    n_kt_levels += 1;
                }
            }
        }
    }
    let mask_of = |slots: &[Option<String>]| {
        slots
            .iter()
            .enumerate()
            .filter(|(_, n)| n.is_some())
            .fold(0u32, |m, (i, _)| m | (1 << i))
    };
    let indicators = mask_of(&names.indicators);
    let vmods = mask_of(&names.vmods);
    let groups = mask_of(&names.groups);
    let mut rep_indicators = 0u32;
    let mut rep_vmods = 0u32;
    let mut rep_groups = 0u32;
    if which & INDICATOR_NAMES != 0 {
        rep_indicators = indicators;
        if indicators == 0 {
            which &= !INDICATOR_NAMES;
        }
        for n in names.indicators.iter().filter(|n| n.is_some()) {
            body.extend_from_slice(&atom(n).to_le_bytes());
        }
    }
    // XKB.h bits (KeyNames 1<<9, KeyAliases 1<<10, VirtualModNames 1<<11,
    // GroupNames 1<<12, RGNames 1<<13); the sections follow in
    // `XkbSendNames` order: vmods, groups, keys, aliases, radio groups.
    if which & VIRTUAL_MOD_NAMES != 0 {
        rep_vmods = vmods;
        if vmods == 0 {
            which &= !VIRTUAL_MOD_NAMES;
        }
        for n in names.vmods.iter().filter(|n| n.is_some()) {
            body.extend_from_slice(&atom(n).to_le_bytes());
        }
    }
    if which & GROUP_NAMES != 0 {
        rep_groups = groups;
        if groups == 0 {
            which &= !GROUP_NAMES;
        }
        for n in names.groups.iter().filter(|n| n.is_some()) {
            body.extend_from_slice(&atom(n).to_le_bytes());
        }
    }
    let (first_key, n_keys) = (desc.min_key_code, desc.num_keys());
    if which & KEY_NAMES != 0 {
        for kc in first_key..=desc.max_key_code {
            body.extend_from_slice(&names.keys[usize::from(kc)]);
        }
    }
    let mut n_aliases = 0usize;
    if which & KEY_ALIASES != 0 && !names.key_aliases.is_empty() {
        for (real, alias) in &names.key_aliases {
            body.extend_from_slice(real);
            body.extend_from_slice(alias);
        }
        n_aliases = names.key_aliases.len();
    } else {
        which &= !KEY_ALIASES;
    }
    // `ProcXkbGetNames` fills nRadioGroups up front and never clears it.
    let n_rg = names.radio_groups.len();
    if which & RG_NAMES != 0 && n_rg > 0 {
        for n in &names.radio_groups {
            body.extend_from_slice(&atom(n).to_le_bytes());
        }
    } else {
        which &= !RG_NAMES;
    }

    let mut r = vec![0u8; 32];
    r[0] = 1;
    r[1] = 1;
    r[4..8].copy_from_slice(&u32::try_from(body.len() / 4).unwrap_or(0).to_le_bytes());
    r[8..12].copy_from_slice(&which.to_le_bytes());
    r[12] = desc.min_key_code;
    r[13] = desc.max_key_code;
    r[14] = u8::try_from(n_types).unwrap_or(u8::MAX);
    r[15] = u8::try_from(rep_groups).unwrap_or(0);
    r[16..18].copy_from_slice(&u16::try_from(rep_vmods).unwrap_or(0).to_le_bytes());
    r[18] = first_key;
    r[19] = n_keys;
    r[20..24].copy_from_slice(&rep_indicators.to_le_bytes());
    r[24] = u8::try_from(n_rg).unwrap_or(u8::MAX);
    r[25] = u8::try_from(n_aliases).unwrap_or(u8::MAX);
    r[26..28].copy_from_slice(&u16::try_from(n_kt_levels).unwrap_or(u16::MAX).to_le_bytes());
    r.extend_from_slice(&body);
    r
}

/// GetNames (minor 17) for a request body `deviceSpec(2) pad(2) which(4)`.
pub(crate) fn reply_get_names(
    desc: &XkbDesc,
    body: &[u8],
    intern_atom: &mut dyn FnMut(&str) -> u32,
) -> Result<Vec<u8>, XkbError> {
    let which = body
        .get(4..8)
        .map_or(ALL_NAMES, |w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]));
    if which & !ALL_NAMES != 0 {
        return Err(XkbError {
            code: BAD_VALUE,
            value: err_code2(0x01, which & !ALL_NAMES),
        });
    }
    Ok(encode_names(desc, which, intern_atom))
}

/// XKB GetControls (minor 6), fixed 92 bytes (`xkbGetControlsReply`). The
/// group count is the model's; the per-key repeat is filled in by the core
/// loop, which owns it. Offsets per XKBproto.h (see the test).
pub(crate) fn reply_get_controls(desc: &XkbDesc) -> Vec<u8> {
    let mut r = vec![0u8; 92];
    r[0] = 1;
    r[1] = 1;
    r[4..8].copy_from_slice(&15u32.to_le_bytes());
    // xkbcommon-x11's get_controls requires 1..=4.
    r[9] = desc.num_groups.clamp(1, 4);
    // groupsWrap: XkbInitControls' XkbSetGroupInfo(1, XkbWrapIntoRange, 0).
    r[10] = 0x01;
    r[20..22].copy_from_slice(&500_u16.to_le_bytes());
    r[22..24].copy_from_slice(&33_u16.to_le_bytes());
    r[56..60].copy_from_slice(&crate::kms::xkb::XKB_ENABLED_CONTROLS.to_le_bytes());
    r
}

/// XkbGBN_* component bits.
pub(crate) const GBN_TYPES: u16 = 1 << 0;
pub(crate) const GBN_COMPAT_MAP: u16 = 1 << 1;
pub(crate) const GBN_CLIENT_SYMBOLS: u16 = 1 << 2;
pub(crate) const GBN_SERVER_SYMBOLS: u16 = 1 << 3;
pub(crate) const GBN_INDICATOR_MAP: u16 = 1 << 4;
pub(crate) const GBN_KEY_NAMES: u16 = 1 << 5;
pub(crate) const GBN_GEOMETRY: u16 = 1 << 6;
pub(crate) const GBN_OTHER_NAMES: u16 = 1 << 7;

/// GetKbdByName's nested GetMap block (`ProcXkbGetKbdByName`): the parts
/// the reported components carry.
pub(crate) fn kbd_by_name_map(desc: &XkbDesc, reported: u16) -> Vec<u8> {
    let all = MapRequest::full(desc);
    let mut req = MapRequest::default();
    if reported & (GBN_TYPES | GBN_CLIENT_SYMBOLS) != 0 {
        req.present |= KEY_TYPES_MASK;
        req.n_types = all.n_types;
    }
    if reported & GBN_CLIENT_SYMBOLS != 0 {
        req.present |= KEY_SYMS_MASK | MODIFIER_MAP_MASK;
        (req.first_key_sym, req.n_key_syms) = (all.first_key_sym, all.n_key_syms);
        (req.first_mod_map_key, req.n_mod_map_keys) = (all.first_mod_map_key, all.n_mod_map_keys);
    }
    if reported & GBN_SERVER_SYMBOLS != 0 {
        req.present |= KEY_ACTIONS_MASK
            | KEY_BEHAVIORS_MASK
            | VIRTUAL_MODS_MASK
            | EXPLICIT_COMPONENTS_MASK
            | VIRTUAL_MOD_MAP_MASK;
        req.virtual_mods = 0xffff;
        (req.first_key_act, req.n_key_acts) = (all.first_key_act, all.n_key_acts);
        (req.first_key_behavior, req.n_key_behaviors) =
            (all.first_key_behavior, all.n_key_behaviors);
        (req.first_key_explicit, req.n_key_explicit) = (all.first_key_explicit, all.n_key_explicit);
        (req.first_vmod_map_key, req.n_vmod_map_keys) =
            (all.first_vmod_map_key, all.n_vmod_map_keys);
    }
    encode_map(desc, req)
}

/// GetKbdByName's nested GetNames block: every name for OtherNames, the
/// key names and aliases for KeyNames.
pub(crate) fn kbd_by_name_names(
    desc: &XkbDesc,
    reported: u16,
    intern_atom: &mut dyn FnMut(&str) -> u32,
) -> Vec<u8> {
    let mut which = if reported & GBN_OTHER_NAMES != 0 {
        ALL_NAMES
    } else {
        0
    };
    if reported & GBN_KEY_NAMES != 0 {
        which |= KEY_NAMES;
        if !desc.names.key_aliases.is_empty() {
            which |= KEY_ALIASES;
        }
    } else {
        which &= !(KEY_NAMES | KEY_ALIASES);
    }
    encode_names(desc, which, intern_atom)
}
