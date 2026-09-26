//! XKB SetNames (#171 phase 4e): Xorg's `ProcXkbSetNames` /
//! `_XkbSetNamesCheck` / `_XkbSetNames` (xkb/xkb.c) over the model, ported
//! literally.
//!
//! 1. [`check_set_names`]: `ProcXkbSetNames`' device-independent checks (the
//!    names mask, then for each of the six component names a bounds check
//!    of one more word, whether or not the component is sent, and its atom)
//!    and `_XkbSetNamesCheck`'s, in Xorg's order, resolving every atom into
//!    its name on the way (names are strings in the model);
//! 2. [`XkbDesc::set_names`]: `_XkbSetNames` — `XkbAllocNames` (level-name
//!    arrays for every type without one, the alias and radio-group counts),
//!    then each part stored as Xorg stores it, the NamesNotify fields filled
//!    in as Xorg fills them, its field slips included: `nLevelNames` is the
//!    request's `nTypes`, `changedVirtualMods` is overwritten by the group
//!    names mask and `changedGroupNames` is never set.
//!
//! Offsets are into the whole request (the 4-byte header included), so they
//! read as Xorg's pointer arithmetic from `stuff`.

use super::{
    KeyName, NUM_GROUPS, NUM_INDICATORS, NUM_VMODS, REQUIRED_TYPE_NAMES, XkbDesc, padded,
    reply::{
        ALL_NAMES, BAD_ACCESS, BAD_ATOM, BAD_LENGTH, BAD_MATCH, BAD_VALUE, GROUP_NAMES,
        INDICATOR_NAMES, KEY_ALIASES, KEY_NAMES, KEY_TYPE_NAMES, KT_LEVEL_NAMES, RG_NAMES,
        VIRTUAL_MOD_NAMES, XkbError, err_code2, err_code3, err_code4,
    },
};

/// `sz_xkbSetNamesReq`.
const SET_NAMES_REQ_SIZE: usize = 28;
/// The six component names (keycodes, geometry, symbols, phys_symbols,
/// types, compat), bits 0..=5 of `which`, in wire order.
const NUM_COMPONENTS: usize = 6;

fn u16_at(req: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([req[at], req[at + 1]])
}

fn u32_at(req: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([req[at], req[at + 1], req[at + 2], req[at + 3]])
}

fn key_name_at(req: &[u8], at: usize) -> KeyName {
    [req[at], req[at + 1], req[at + 2], req[at + 3]]
}

/// `xkbSetNamesReq`'s fields after the 4-byte header, plus the request
/// length in bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SetNamesHeader {
    pub length: usize,
    pub device_spec: u16,
    pub virtual_mods: u16,
    pub which: u32,
    pub first_type: u8,
    pub n_types: u8,
    pub first_kt_level: u8,
    pub n_kt_levels: u8,
    pub indicators: u32,
    pub group_names: u8,
    pub n_radio_groups: u8,
    pub first_key: u8,
    pub n_keys: u8,
    pub n_key_aliases: u8,
}

impl SetNamesHeader {
    /// The header of a whole request `req` (at least 28 bytes).
    pub(crate) fn parse(req: &[u8]) -> Self {
        Self {
            length: req.len(),
            device_spec: u16_at(req, 4),
            virtual_mods: u16_at(req, 6),
            which: u32_at(req, 8),
            first_type: req[12],
            n_types: req[13],
            first_kt_level: req[14],
            n_kt_levels: req[15],
            indicators: u32_at(req, 16),
            group_names: req[20],
            n_radio_groups: req[21],
            first_key: req[22],
            n_keys: req[23],
            n_key_aliases: req[24],
        }
    }
}

/// A checked SetNames, its atoms resolved (`None` = atom `None`). Each part
/// is present only when `which` has it; the masked parts in bit order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SetNames {
    /// Per component bit: `Some(name)` when sent.
    pub components: [Option<Option<String>>; NUM_COMPONENTS],
    pub type_names: Vec<Option<String>>,
    /// Per type from `firstKTLevel`: its level names (none for width 0).
    pub level_names: Vec<Vec<Option<String>>>,
    pub indicators: Vec<Option<String>>,
    pub vmods: Vec<Option<String>>,
    pub groups: Vec<Option<String>>,
    pub keys: Vec<KeyName>,
    /// `(real, alias)`.
    pub key_aliases: Vec<(KeyName, KeyName)>,
    pub radio_groups: Vec<Option<String>>,
}

/// The walk over a request's data words: Xorg's `tmp`, its
/// `_XkbCheckRequestBounds` and its atom checks.
struct Walk<'a> {
    req: &'a [u8],
    at: usize,
    atom_name: &'a dyn Fn(u32) -> Option<String>,
    /// `client->errorValue` so far: the value a BadLength from a bounds
    /// check carries (Xorg leaves it stale; 0 for a client with no earlier
    /// error, and `_XkbSetNamesCheck` sets it without failing for a
    /// required type's name).
    error_value: u32,
}

impl Walk<'_> {
    /// `_XkbCheckRequestBounds(client, stuff, from, to)`: a non-empty range
    /// inside the request.
    fn bounds(&self, from: usize, to: usize) -> Result<(), XkbError> {
        if from < to && from < self.req.len() && to <= self.req.len() {
            Ok(())
        } else {
            Err(XkbError {
                code: BAD_LENGTH,
                value: self.error_value,
            })
        }
    }

    /// One atom (`_XkbCheckAtoms` over one word): `None` stays `None`, an
    /// atom that isn't valid is BadAtom with the atom as errorValue.
    fn atom(&mut self) -> Result<Option<String>, XkbError> {
        let atom = u32_at(self.req, self.at);
        self.at += 4;
        if atom == 0 {
            return Ok(None);
        }
        (self.atom_name)(atom).map(Some).ok_or(XkbError {
            code: BAD_ATOM,
            value: atom,
        })
    }

    /// `_XkbCheckAtoms` over `n` words.
    fn atoms(&mut self, n: usize) -> Result<Vec<Option<String>>, XkbError> {
        (0..n).map(|_| self.atom()).collect()
    }

    /// `_XkbCheckMaskedAtoms`: one word per bit of `present` below
    /// `n_atoms` (higher bits consume nothing).
    fn masked_atoms(
        &mut self,
        n_atoms: usize,
        present: u32,
    ) -> Result<Vec<Option<String>>, XkbError> {
        (0..n_atoms)
            .filter(|i| present & (1 << i) != 0)
            .map(|_| self.atom())
            .collect()
    }
}

/// Port of `ProcXkbSetNames`' checks after its size and BadAccess checks
/// (the core loop's) and `_XkbSetNamesCheck`, in Xorg's order; the decoded
/// request when it passes. `atom_name` resolves a valid atom.
pub(crate) fn check_set_names(
    desc: &XkbDesc,
    h: &SetNamesHeader,
    req: &[u8],
    atom_name: &dyn Fn(u32) -> Option<String>,
) -> Result<SetNames, XkbError> {
    // CHK_MASK_LEGAL(0x01, stuff->which, XkbAllNamesMask)
    if h.which & !ALL_NAMES != 0 {
        return Err(XkbError {
            code: BAD_VALUE,
            value: err_code2(0x01, h.which & !ALL_NAMES),
        });
    }
    let mut w = Walk {
        req,
        at: SET_NAMES_REQ_SIZE,
        atom_name,
        error_value: 0,
    };
    let mut m = SetNames::default();
    // The device-independent part: one more word must be there before each
    // component, sent or not.
    for (bit, slot) in m.components.iter_mut().enumerate() {
        w.bounds(w.at, w.at + 4)?;
        if h.which & (1 << bit) != 0 {
            *slot = Some(w.atom()?);
        }
    }

    // _XkbSetNamesCheck
    let num_types = desc.types.len();
    if h.which & KEY_TYPE_NAMES != 0 {
        let (first, n) = (usize::from(h.first_type), usize::from(h.n_types));
        if n < 1 {
            return Err(XkbError {
                code: BAD_VALUE,
                value: err_code2(0x02, u32::from(h.n_types)),
            });
        }
        if first + n > num_types {
            return Err(XkbError {
                code: BAD_VALUE,
                value: err_code4(
                    0x03,
                    u32::from(h.first_type),
                    u32::from(h.n_types),
                    u32::try_from(num_types).unwrap_or(u32::MAX),
                ),
            });
        }
        if first < REQUIRED_TYPE_NAMES.len() {
            return Err(XkbError {
                code: BAD_ACCESS,
                value: err_code2(0x04, u32::from(h.first_type)),
            });
        }
        w.bounds(w.at, w.at + 4 * n)?;
        m.type_names = w.atoms(n)?;
        // `_XkbCheckTypeName` only sets the errorValue (Xorg's check never
        // fails). A None name would make Xorg's strcmp dereference NULL;
        // here it simply isn't a required type's name.
        for (i, name) in m.type_names.iter().enumerate() {
            if name
                .as_deref()
                .is_some_and(|n| REQUIRED_TYPE_NAMES.contains(&n))
            {
                w.error_value = err_code2(0x05, u32::try_from(i).unwrap_or(0));
            }
        }
    }
    if h.which & KT_LEVEL_NAMES != 0 {
        let (first, n) = (usize::from(h.first_kt_level), usize::from(h.n_kt_levels));
        if n < 1 {
            return Err(XkbError {
                code: BAD_VALUE,
                value: err_code2(0x05, u32::from(h.n_kt_levels)),
            });
        }
        if first + n > num_types {
            return Err(XkbError {
                code: BAD_VALUE,
                value: err_code4(
                    0x06,
                    u32::from(h.first_kt_level),
                    u32::from(h.n_kt_levels),
                    u32::try_from(num_types).unwrap_or(u32::MAX),
                ),
            });
        }
        let width_at = w.at;
        w.at += padded(n);
        w.bounds(width_at, w.at)?;
        for i in 0..n {
            let width = req[width_at + i];
            if width == 0 {
                m.level_names.push(Vec::new());
                continue;
            }
            let levels = desc.types[first + i].num_levels;
            if width != levels {
                return Err(XkbError {
                    code: BAD_MATCH,
                    value: err_code4(
                        0x07,
                        u32::try_from(first + i).unwrap_or(0),
                        u32::from(levels),
                        u32::from(width),
                    ),
                });
            }
            w.bounds(w.at, w.at + 4 * usize::from(width))?;
            m.level_names.push(w.atoms(usize::from(width))?);
        }
    }
    if h.which & INDICATOR_NAMES != 0 {
        if h.indicators == 0 {
            return Err(XkbError {
                code: BAD_MATCH,
                value: 0x08,
            });
        }
        w.bounds(w.at, w.at + 4 * h.indicators.count_ones() as usize)?;
        m.indicators = w.masked_atoms(NUM_INDICATORS, h.indicators)?;
    }
    if h.which & VIRTUAL_MOD_NAMES != 0 {
        if h.virtual_mods == 0 {
            return Err(XkbError {
                code: BAD_MATCH,
                value: 0x09,
            });
        }
        w.bounds(w.at, w.at + 4 * h.virtual_mods.count_ones() as usize)?;
        m.vmods = w.masked_atoms(NUM_VMODS, u32::from(h.virtual_mods))?;
    }
    if h.which & GROUP_NAMES != 0 {
        if h.group_names == 0 {
            return Err(XkbError {
                code: BAD_MATCH,
                value: 0x0a,
            });
        }
        w.bounds(w.at, w.at + 4 * h.group_names.count_ones() as usize)?;
        m.groups = w.masked_atoms(NUM_GROUPS, u32::from(h.group_names))?;
    }
    if h.which & KEY_NAMES != 0 {
        if h.first_key < desc.min_key_code {
            return Err(XkbError {
                code: BAD_VALUE,
                value: err_code3(0x0b, u32::from(desc.min_key_code), u32::from(h.first_key)),
            });
        }
        if i32::from(h.first_key) + i32::from(h.n_keys) - 1 > i32::from(desc.max_key_code)
            || h.n_keys < 1
        {
            return Err(XkbError {
                code: BAD_VALUE,
                value: err_code4(
                    0x0c,
                    u32::from(desc.max_key_code),
                    u32::from(h.first_key),
                    u32::from(h.n_keys),
                ),
            });
        }
        let n = usize::from(h.n_keys);
        w.bounds(w.at, w.at + 4 * n)?;
        m.keys = (0..n).map(|i| key_name_at(req, w.at + 4 * i)).collect();
        w.at += 4 * n;
    }
    if h.which & KEY_ALIASES != 0 && h.n_key_aliases > 0 {
        let n = usize::from(h.n_key_aliases);
        w.bounds(w.at, w.at + 8 * n)?;
        m.key_aliases = (0..n)
            .map(|i| {
                let at = w.at + 8 * i;
                (key_name_at(req, at), key_name_at(req, at + 4))
            })
            .collect();
        w.at += 8 * n;
    }
    if h.which & RG_NAMES != 0 {
        if h.n_radio_groups < 1 {
            return Err(XkbError {
                code: BAD_VALUE,
                value: err_code2(0x0d, u32::from(h.n_radio_groups)),
            });
        }
        let n = usize::from(h.n_radio_groups);
        w.bounds(w.at, w.at + 4 * n)?;
        m.radio_groups = w.atoms(n)?;
    }
    // `(tmp - (CARD32 *) stuff) != client->req_len`
    if w.at != req.len() {
        return Err(XkbError {
            code: BAD_LENGTH,
            value: u32::try_from(req.len() / 4).unwrap_or(u32::MAX),
        });
    }
    Ok(m)
}

impl XkbDesc {
    /// Port of `_XkbSetNames` after its checks: `XkbAllocNames`, the parts
    /// stored as Xorg stores them, and the NamesNotify Xorg fills in
    /// (`device_id` left 0 for the caller).
    pub(crate) fn set_names(
        &mut self,
        h: &SetNamesHeader,
        m: &SetNames,
    ) -> yserver_protocol::x11::XkbNamesNotify {
        // XkbAllocNames(xkb, which, nRadioGroups, nKeyAliases): every type
        // without level names gets a zeroed array of its level count; the
        // alias and radio-group arrays take the request's counts.
        if h.which & KT_LEVEL_NAMES != 0 {
            for t in &mut self.types {
                if t.level_names.is_none() {
                    t.level_names = Some(vec![None; usize::from(t.num_levels)]);
                }
            }
        }
        let zero_alias = ([0u8; 4], [0u8; 4]);
        if h.which & KEY_ALIASES != 0 && h.n_key_aliases > 0 {
            self.names
                .key_aliases
                .resize(usize::from(h.n_key_aliases), zero_alias);
        }
        if h.which & RG_NAMES != 0 && h.n_radio_groups > 0 {
            self.names
                .radio_groups
                .resize(usize::from(h.n_radio_groups), None);
        }

        let mut nn = yserver_protocol::x11::XkbNamesNotify {
            changed: u16::try_from(h.which).unwrap_or(u16::MAX),
            ..Default::default()
        };
        let names = &mut self.names;
        let slots = [
            &mut names.keycodes,
            &mut names.geometry,
            &mut names.symbols,
            &mut names.phys_symbols,
            &mut names.types,
            &mut names.compat,
        ];
        for (slot, sent) in slots.into_iter().zip(&m.components) {
            if let Some(name) = sent {
                slot.clone_from(name);
            }
        }
        if h.which & KEY_TYPE_NAMES != 0 && h.n_types > 0 {
            let first = usize::from(h.first_type);
            for (i, name) in m.type_names.iter().enumerate() {
                self.types[first + i].name.clone_from(name);
            }
            nn.first_type = h.first_type;
            nn.n_types = h.n_types;
        }
        if h.which & KT_LEVEL_NAMES != 0 {
            let first = usize::from(h.first_kt_level);
            for (i, wire) in m.level_names.iter().enumerate() {
                // `if (type->level_names)`: always true after XkbAllocNames.
                if let Some(ln) = self.types[first + i].level_names.as_mut() {
                    if ln.len() < wire.len() {
                        ln.resize(wire.len(), None);
                    }
                    ln[..wire.len()].clone_from_slice(wire);
                }
            }
            nn.first_level_name = 0;
            nn.n_level_names = h.n_types;
        }
        let names = &mut self.names;
        let copy_masked = |dest: &mut [Option<String>], present: u32, wire: &[Option<String>]| {
            let mut wire = wire.iter();
            for (i, slot) in dest.iter_mut().enumerate() {
                if present & (1 << i) != 0
                    && let Some(name) = wire.next()
                {
                    slot.clone_from(name);
                }
            }
        };
        if h.which & INDICATOR_NAMES != 0 {
            copy_masked(&mut names.indicators, h.indicators, &m.indicators);
            nn.changed_indicators = h.indicators;
        }
        if h.which & VIRTUAL_MOD_NAMES != 0 {
            copy_masked(&mut names.vmods, u32::from(h.virtual_mods), &m.vmods);
            nn.changed_virtual_mods = h.virtual_mods;
        }
        if h.which & GROUP_NAMES != 0 {
            copy_masked(&mut names.groups, u32::from(h.group_names), &m.groups);
            nn.changed_virtual_mods = u16::from(h.group_names);
        }
        if h.which & KEY_NAMES != 0 {
            let first = usize::from(h.first_key);
            names.keys[first..first + m.keys.len()].copy_from_slice(&m.keys);
            nn.first_key = h.first_key;
            nn.n_keys = h.n_keys;
        }
        if h.which & KEY_ALIASES != 0 {
            if h.n_key_aliases > 0 {
                names.key_aliases.clone_from(&m.key_aliases);
            } else {
                names.key_aliases.clear();
            }
            nn.n_aliases = u8::try_from(names.key_aliases.len()).unwrap_or(u8::MAX);
        }
        if h.which & RG_NAMES != 0 {
            if h.n_radio_groups > 0 {
                names.radio_groups.clone_from(&m.radio_groups);
            } else {
                names.radio_groups.clear();
            }
            nn.n_radio_groups = u8::try_from(names.radio_groups.len()).unwrap_or(u8::MAX);
        }
        nn
    }
}
