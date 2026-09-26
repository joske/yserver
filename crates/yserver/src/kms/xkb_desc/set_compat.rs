//! XKB SetCompatMap and SetIndicatorMap (#171 phase 4d): Xorg's
//! `ProcXkbSetCompatMap` / `_XkbSetCompatMap` and `ProcXkbSetIndicatorMap` /
//! `_XkbSetIndicatorMap` (xkb/xkb.c) over the model, ported literally.
//!
//! SetCompatMap, as Xorg orders it:
//!
//! 1. [`check_compat_map`]: `_XkbSetCompatMap`'s dry run — firstSI past the
//!    interprets (BadValue) and the request length (BadLength) — plus the
//!    apply pass's `firstSI + nSI > USHRT_MAX` refusal, which Xorg reaches
//!    before it changes anything;
//! 2. [`decode_compat_map`] + [`XkbDesc::set_compat_map`]: the interprets
//!    stored from firstSI (the broken `Any+AnyOfOrNone(all)->Private`
//!    interpret skipped and the rest closed up), `truncateSI`, the group
//!    compat maps, then (`recomputeActions`) `XkbUpdateActions` over the
//!    whole keycode range.
//!
//! SetIndicatorMap: [`check_indicator_map`] (which=0 is a successful no-op,
//! then the length, then `CHK_MASK_LEGAL` on each map's whichGroups and
//! whichMods), then [`XkbDesc::set_indicator_map`]. The lit state and the
//! notifications of `XkbApplyLedMapChanges` are the backend's (it owns the
//! cooking state the indicators are read from).
//!
//! Offsets are into the whole request (the 4-byte header included), so they
//! read as Xorg's pointer arithmetic from `stuff`.

use super::{
    IndicatorMap, Mods, NUM_GROUPS, NUM_INDICATORS, SymInterpret, XkbChanges, XkbDesc,
    reply::{BAD_LENGTH, BAD_VALUE, XkbError, err_code2},
};

/// `sz_xkbSetCompatMapReq`.
const SET_COMPAT_MAP_REQ_SIZE: usize = 16;
/// `sz_xkbSymInterpretWireDesc`.
const SYM_INTERPRET_WIRE_SIZE: usize = 16;
/// `sz_xkbModsWireDesc`.
const MODS_WIRE_SIZE: usize = 4;
/// `sz_xkbSetIndicatorMapReq`.
const SET_INDICATOR_MAP_REQ_SIZE: usize = 12;
/// `sz_xkbIndicatorMapWireDesc`.
const INDICATOR_MAP_WIRE_SIZE: usize = 12;
/// `XkbIM_UseAnyGroup` / `XkbIM_UseAnyMods`.
const IM_USE_ANY_GROUP: u8 = 0x0f;
const IM_USE_ANY_MODS: u8 = 0x1f;
/// `XkbSA_XFree86Private`.
const SA_XFREE86_PRIVATE: u8 = 0x86;

fn u16_at(req: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([req[at], req[at + 1]])
}

fn u32_at(req: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([req[at], req[at + 1], req[at + 2], req[at + 3]])
}

/// `xkbSetCompatMapReq`'s fields after the 4-byte header, plus the request
/// length in bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SetCompatMapHeader {
    pub length: usize,
    pub device_spec: u16,
    pub recompute_actions: bool,
    pub truncate_si: bool,
    pub groups: u8,
    pub first_si: u16,
    pub n_si: u16,
}

impl SetCompatMapHeader {
    /// The header of a whole request `req` (at least 16 bytes).
    pub(crate) fn parse(req: &[u8]) -> Self {
        Self {
            length: req.len(),
            device_spec: u16_at(req, 4),
            recompute_actions: req[7] != 0,
            truncate_si: req[8] != 0,
            groups: req[9],
            first_si: u16_at(req, 10),
            n_si: u16_at(req, 12),
        }
    }
}

/// Port of `_XkbSetCompatMap`'s dry run, then the one check of its apply
/// pass that comes before any change (`firstSI + nSI > USHRT_MAX`). The
/// BadLength and that BadValue carry Xorg's stale `client->errorValue`, 0
/// for a client with no earlier error.
pub(crate) fn check_compat_map(desc: &XkbDesc, h: &SetCompatMapHeader) -> Result<(), XkbError> {
    let mut len = SET_COMPAT_MAP_REQ_SIZE;
    if h.n_si > 0 || h.truncate_si {
        let num_si = u32::try_from(desc.compat.len()).unwrap_or(u32::MAX);
        if u32::from(h.first_si) > num_si {
            return Err(XkbError {
                code: BAD_VALUE,
                value: err_code2(0x02, num_si),
            });
        }
        len += usize::from(h.n_si) * SYM_INTERPRET_WIRE_SIZE;
    }
    // Xorg counts only the XkbNumKbdGroups (4) low bits; higher bits carry
    // no data and are ignored, though CompatMapNotify still reports them.
    len += (h.groups & ((1 << NUM_GROUPS) - 1)).count_ones() as usize * MODS_WIRE_SIZE;
    if len / 4 != h.length / 4 {
        return Err(XkbError {
            code: BAD_LENGTH,
            value: 0,
        });
    }
    if h.n_si > 0 && u32::from(h.first_si) + u32::from(h.n_si) > u32::from(u16::MAX) {
        return Err(XkbError {
            code: BAD_VALUE,
            value: 0,
        });
    }
    Ok(())
}

/// A decoded SetCompatMap: its interprets and group compat maps
/// (`(realMods, virtualMods)` per bit of `groups`), in wire order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SetCompatMap {
    pub sis: Vec<SymInterpret>,
    pub groups: Vec<(u8, u16)>,
}

/// Read a checked request's interprets and group compat maps.
pub(crate) fn decode_compat_map(h: &SetCompatMapHeader, req: &[u8]) -> SetCompatMap {
    let mut p = SET_COMPAT_MAP_REQ_SIZE;
    let mut m = SetCompatMap::default();
    for _ in 0..h.n_si {
        let w = &req[p..p + SYM_INTERPRET_WIRE_SIZE];
        m.sis.push(SymInterpret {
            sym: u32_at(w, 0),
            mods: w[4],
            match_: w[5],
            virtual_mod: w[6],
            flags: w[7],
            act: std::array::from_fn(|i| w[8 + i]),
        });
        p += SYM_INTERPRET_WIRE_SIZE;
    }
    for g in 0..NUM_GROUPS {
        if h.groups & (1 << g) != 0 {
            m.groups.push((req[p + 1], u16_at(req, p + 2)));
            p += MODS_WIRE_SIZE;
        }
    }
    m
}

/// The broken interpret Xorg refuses to store: `Any` + `AnyOfOrNone(all)`
/// → a Private action (`_XkbSetCompatMap`'s "Skipping broken ..." check).
fn is_broken_any_interpret(si: &SymInterpret) -> bool {
    si.sym == 0
        && si.match_ == super::SI_ANY_OF_OR_NONE
        && si.mods == 0xff
        && si.act[0] == SA_XFREE86_PRIVATE
}

impl XkbDesc {
    /// Port of `_XkbSetCompatMap` after its checks: the interprets from
    /// firstSI (the broken ones skipped, the old interprets after the
    /// request's range closed up behind the stored ones), `truncateSI`, the
    /// group compat maps (mask resolved from the virtual modifiers), then
    /// for `recomputeActions` `XkbUpdateActions(min_key_code, XkbNumKeys)`.
    /// Returns the recompute's changes (none without `recomputeActions`).
    pub(crate) fn set_compat_map(
        &mut self,
        h: &SetCompatMapHeader,
        m: &SetCompatMap,
    ) -> XkbChanges {
        let first = usize::from(h.first_si);
        let end = first + usize::from(h.n_si);
        if h.n_si > 0 {
            // num_si becomes firstSI + nSI when that grows it or when
            // truncating; the stored interprets are the unskipped ones, and
            // the old interprets past the request's range (kept only when
            // num_si didn't become `end`) move down by the skipped count.
            let tail: Vec<SymInterpret> = if !h.truncate_si && end < self.compat.len() {
                self.compat[end..].to_vec()
            } else {
                Vec::new()
            };
            self.compat.truncate(first);
            self.compat.extend(
                m.sis
                    .iter()
                    .filter(|si| !is_broken_any_interpret(si))
                    .copied(),
            );
            self.compat.extend(tail);
        } else if h.truncate_si {
            self.compat.truncate(first);
        }
        let mut wire = m.groups.iter();
        for g in 0..NUM_GROUPS {
            if h.groups & (1 << g) != 0 {
                let &(real, vmods) = wire.next().unwrap_or(&(0, 0));
                let mut mask = real;
                if vmods != 0 {
                    mask |= self.vmods_to_real(vmods);
                }
                self.group_compat[g] = Mods { mask, real, vmods };
            }
        }
        let mut changes = XkbChanges::default();
        if h.recompute_actions {
            let (min, num) = (self.min_key_code, self.num_keys());
            self.update_desc_actions(min, num, &mut changes);
        }
        changes
    }

    /// `XkbIM_InUse` over every indicator map (the server's macro:
    /// flags, whichGroups, whichMods or ctrls): `sli->mapsPresent`.
    pub(crate) fn maps_present(&self) -> u32 {
        (0..NUM_INDICATORS)
            .filter(|&i| {
                let m = self.indicators[i];
                m.flags != 0 || m.which_groups != 0 || m.which_mods != 0 || m.ctrls != 0
            })
            .fold(0, |bits, i| bits | (1 << i))
    }

    /// The indicators with a name: `sli->namesPresent`.
    pub(crate) fn names_present(&self) -> u32 {
        (0..NUM_INDICATORS)
            .filter(|&i| self.names.indicators[i].is_some())
            .fold(0, |bits, i| bits | (1 << i))
    }

    /// Port of `_XkbSetIndicatorMap`'s stores: each map in `which`, its
    /// mask and real modifiers both the wire `mods` byte (Xorg ignores the
    /// `realMods` byte), the virtual modifiers resolved into the mask.
    pub(crate) fn set_indicator_map(&mut self, which: u32, maps: &[WireIndicatorMap]) {
        let mut wire = maps.iter();
        for i in 0..NUM_INDICATORS {
            if which & (1 << i) == 0 {
                continue;
            }
            let Some(w) = wire.next() else {
                break;
            };
            let mut mask = w.mods;
            if w.vmods != 0 {
                mask = w.mods | self.vmods_to_real(w.vmods);
            }
            self.indicators[i] = IndicatorMap {
                flags: w.flags,
                which_groups: w.which_groups,
                groups: w.groups,
                which_mods: w.which_mods,
                mods: Mods {
                    mask,
                    real: w.mods,
                    vmods: w.vmods,
                },
                ctrls: w.ctrls,
            };
        }
    }
}

/// An indicator map as SetIndicatorMap carries it
/// (`xkbIndicatorMapWireDesc`; its `realMods` byte isn't read).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct WireIndicatorMap {
    pub flags: u8,
    pub which_groups: u8,
    pub groups: u8,
    pub which_mods: u8,
    pub mods: u8,
    pub vmods: u16,
    pub ctrls: u32,
}

/// Port of `ProcXkbSetIndicatorMap`'s checks, after its size and BadAccess
/// checks: `Ok(None)` for which=0 (Success, nothing done, whatever the
/// length), else the request length (BadLength, Xorg's stale errorValue 0)
/// and each map's whichGroups and whichMods (BadValue
/// `_XkbErrCode2(indicator, illegal bits)`), then the decoded maps.
pub(crate) fn check_indicator_map(
    req: &[u8],
) -> Result<Option<(u32, Vec<WireIndicatorMap>)>, XkbError> {
    let which = u32_at(req, 8);
    if which == 0 {
        return Ok(None);
    }
    let n = which.count_ones() as usize;
    if req.len() / 4 != (SET_INDICATOR_MAP_REQ_SIZE + n * INDICATOR_MAP_WIRE_SIZE) / 4 {
        return Err(XkbError {
            code: BAD_LENGTH,
            value: 0,
        });
    }
    let mut maps = Vec::with_capacity(n);
    let mut p = SET_INDICATOR_MAP_REQ_SIZE;
    for i in 0..NUM_INDICATORS {
        if which & (1 << i) == 0 {
            continue;
        }
        let w = &req[p..p + INDICATOR_MAP_WIRE_SIZE];
        let i32_ = u32::try_from(i).unwrap_or(0);
        for (mask, legal) in [(w[1], IM_USE_ANY_GROUP), (w[3], IM_USE_ANY_MODS)] {
            if mask & !legal != 0 {
                return Err(XkbError {
                    code: BAD_VALUE,
                    value: err_code2(i32_, u32::from(mask & !legal)),
                });
            }
        }
        maps.push(WireIndicatorMap {
            flags: w[0],
            which_groups: w[1],
            groups: w[2],
            which_mods: w[3],
            mods: w[4],
            vmods: u16_at(w, 6),
            ctrls: u32_at(w, 8),
        });
        p += INDICATOR_MAP_WIRE_SIZE;
    }
    Ok(Some((which, maps)))
}
