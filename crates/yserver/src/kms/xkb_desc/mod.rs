//! Xorg's server-side XKB keyboard description (`XkbDescRec`), as a Rust
//! model (#171 phase 4).
//!
//! The model is authoritative: every XKB and core readback (GetMap,
//! GetCompatMap, GetNames, GetIndicatorMap, GetKbdByName's blocks,
//! GetKeyboardMapping, GetModifierMapping) encodes it, and every mutation
//! (ChangeKeyboardMapping, SetModifierMapping) is a literal port of Xorg's
//! handler over it. xkbcommon only cooks keys: after each mutation the model
//! is written out as complete V1 text ([`writer`]) and compiled, and that
//! keymap drives `xkb_state`.
//!
//! The model is seeded from xkbcommon's compile of a keymap loaded by name
//! ([`seed`]); nothing else re-seeds it.

pub(crate) mod action;
mod case;
#[cfg(test)]
pub(crate) mod gate;
#[cfg(test)]
pub(crate) mod probe;
pub(crate) mod reply;
pub(crate) mod seed;
pub(crate) mod set_map;
pub(crate) mod text;
pub(crate) mod writer;

#[cfg(test)]
pub(crate) mod tests;

/// `XkbNumVirtualMods`.
pub(crate) const NUM_VMODS: usize = 16;
/// `XkbNumIndicators`.
pub(crate) const NUM_INDICATORS: usize = 32;
/// `XkbNumKbdGroups`.
pub(crate) const NUM_GROUPS: usize = 4;
/// The required types' names (`XkbNumRequiredTypes`), at their fixed
/// indices.
pub(crate) const REQUIRED_TYPE_NAMES: [&str; 4] =
    ["ONE_LEVEL", "TWO_LEVEL", "ALPHABETIC", "KEYPAD"];

/// `XkbMapNotify.changed` / GetMap part bits (XKB.h `Xkb*Mask`).
pub(crate) const KEY_TYPES_MASK: u16 = 1 << 0;
pub(crate) const KEY_SYMS_MASK: u16 = 1 << 1;
pub(crate) const MODIFIER_MAP_MASK: u16 = 1 << 2;
pub(crate) const EXPLICIT_COMPONENTS_MASK: u16 = 1 << 3;
pub(crate) const KEY_ACTIONS_MASK: u16 = 1 << 4;
pub(crate) const KEY_BEHAVIORS_MASK: u16 = 1 << 5;
pub(crate) const VIRTUAL_MODS_MASK: u16 = 1 << 6;
pub(crate) const VIRTUAL_MOD_MAP_MASK: u16 = 1 << 7;

/// `server->explicit` bits (`XkbExplicit*Mask`).
pub(crate) const EXPLICIT_KEY_TYPES: u8 = 0x0f;
pub(crate) const EXPLICIT_INTERPRET: u8 = 0x10;
pub(crate) const EXPLICIT_AUTO_REPEAT: u8 = 0x20;
pub(crate) const EXPLICIT_BEHAVIOR: u8 = 0x40;
pub(crate) const EXPLICIT_VMODMAP: u8 = 0x80;

/// Key action types (`XkbSA_*`).
pub(crate) const SA_NO_ACTION: u8 = 0x00;
pub(crate) const SA_SET_MODS: u8 = 0x01;
pub(crate) const SA_LATCH_MODS: u8 = 0x02;
pub(crate) const SA_LOCK_MODS: u8 = 0x03;
pub(crate) const SA_SET_GROUP: u8 = 0x04;
pub(crate) const SA_LATCH_GROUP: u8 = 0x05;
pub(crate) const SA_LOCK_GROUP: u8 = 0x06;
pub(crate) const SA_MOVE_PTR: u8 = 0x07;
pub(crate) const SA_PTR_BTN: u8 = 0x08;
pub(crate) const SA_LOCK_PTR_BTN: u8 = 0x09;
pub(crate) const SA_SET_PTR_DFLT: u8 = 0x0a;
pub(crate) const SA_ISO_LOCK: u8 = 0x0b;
pub(crate) const SA_TERMINATE: u8 = 0x0c;
pub(crate) const SA_SWITCH_SCREEN: u8 = 0x0d;
pub(crate) const SA_SET_CONTROLS: u8 = 0x0e;
pub(crate) const SA_LOCK_CONTROLS: u8 = 0x0f;
pub(crate) const SA_ACTION_MESSAGE: u8 = 0x10;
pub(crate) const SA_REDIRECT_KEY: u8 = 0x11;
pub(crate) const SA_DEVICE_BTN: u8 = 0x12;
pub(crate) const SA_LOCK_DEVICE_BTN: u8 = 0x13;
pub(crate) const SA_DEVICE_VALUATOR: u8 = 0x14;

/// `XkbSA_UseModMapMods` (mods and ISOLock actions).
pub(crate) const SA_USE_MOD_MAP_MODS: u8 = 0x04;

/// `XkbSI_*`.
pub(crate) const SI_AUTO_REPEAT: u8 = 0x01;
pub(crate) const SI_LOCKING_KEY: u8 = 0x02;
pub(crate) const SI_LEVEL_ONE_ONLY: u8 = 0x80;
pub(crate) const SI_OP_MASK: u8 = 0x7f;
pub(crate) const SI_NONE_OF: u8 = 0;
pub(crate) const SI_ANY_OF_OR_NONE: u8 = 1;
pub(crate) const SI_ANY_OF: u8 = 2;
pub(crate) const SI_ALL_OF: u8 = 3;
pub(crate) const SI_EXACTLY: u8 = 4;
/// `XkbNoModifier`: an interpret that names no virtual modifier.
pub(crate) const NO_MODIFIER: u8 = 0xff;

/// `XkbKB_Default` / `XkbKB_Lock`.
pub(crate) const KB_DEFAULT: u8 = 0;
pub(crate) const KB_LOCK: u8 = 1;

/// `group_info` flags (`XkbOutOfRangeGroupInfo`).
pub(crate) const CLAMP_INTO_RANGE: u8 = 0x40;
pub(crate) const REDIRECT_INTO_RANGE: u8 = 0x80;

/// A wire action (`xkbActionWireDesc`, 8 bytes).
pub(crate) type Action = [u8; 8];

/// `XkbModsRec`: the resolved mask plus its real and virtual parts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Mods {
    pub mask: u8,
    pub real: u8,
    pub vmods: u16,
}

/// `XkbKTMapEntryRec`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct KtEntry {
    pub active: bool,
    pub mods: Mods,
    pub level: u8,
}

/// `XkbKeyTypeRec`, with its name and level names (atoms in Xorg).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct KeyType {
    pub mods: Mods,
    pub num_levels: u8,
    pub map: Vec<KtEntry>,
    /// One per map entry, or none (`type->preserve == NULL`).
    pub preserve: Option<Vec<Mods>>,
    pub name: Option<String>,
    /// `type->level_names`: `None` = no array; else one per level, `None`
    /// being atom 0 (also what yserver reports for a slot Xorg leaves
    /// uninitialised).
    pub level_names: Option<Vec<Option<String>>>,
}

/// `XkbSymMapRec` plus the key's keysyms.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct KeySyms {
    pub kt_index: [u8; 4],
    pub group_info: u8,
    pub width: u8,
    /// `width * groups` keysyms, group-major.
    pub syms: Vec<u32>,
}

impl KeySyms {
    /// `XkbKeyNumGroups`.
    pub(crate) fn num_groups(&self) -> usize {
        usize::from(self.group_info & 0x0f)
    }

    /// `XkbKeyNumSyms`.
    pub(crate) fn num_syms(&self) -> usize {
        self.num_groups() * usize::from(self.width)
    }
}

/// `XkbBehavior`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Behavior {
    pub kind: u8,
    pub data: u8,
}

/// `XkbSymInterpretRec`, its action in wire form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SymInterpret {
    /// `NoSymbol` (0) = `Any`.
    pub sym: u32,
    pub mods: u8,
    /// Operator | `XkbSI_LevelOneOnly`.
    pub match_: u8,
    /// Virtual modifier index, [`NO_MODIFIER`] for none.
    pub virtual_mod: u8,
    pub flags: u8,
    pub act: Action,
}

/// `XkbIndicatorMapRec`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct IndicatorMap {
    pub flags: u8,
    pub which_groups: u8,
    pub groups: u8,
    pub which_mods: u8,
    pub mods: Mods,
    pub ctrls: u32,
}

/// A key name as the wire carries it: four bytes, zero padded.
pub(crate) type KeyName = [u8; 4];

/// `XkbNamesRec`, names as strings (atoms live in the core loop).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Names {
    pub keycodes: Option<String>,
    pub geometry: Option<String>,
    pub symbols: Option<String>,
    pub phys_symbols: Option<String>,
    pub types: Option<String>,
    pub compat: Option<String>,
    pub vmods: [Option<String>; NUM_VMODS],
    pub indicators: [Option<String>; NUM_INDICATORS],
    pub groups: [Option<String>; NUM_GROUPS],
    /// Per keycode.
    pub keys: Vec<KeyName>,
    /// `(real, alias)` in Xorg's order.
    pub key_aliases: Vec<(KeyName, KeyName)>,
    pub radio_groups: Vec<Option<String>>,
}

impl Default for Names {
    fn default() -> Self {
        Self {
            keycodes: None,
            geometry: None,
            symbols: None,
            phys_symbols: None,
            types: None,
            compat: None,
            vmods: Default::default(),
            indicators: std::array::from_fn(|_| None),
            groups: Default::default(),
            keys: vec![[0; 4]; 256],
            key_aliases: Vec::new(),
            radio_groups: Vec::new(),
        }
    }
}

/// Xorg's `XkbDescRec` for yserver's one keyboard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct XkbDesc {
    pub min_key_code: u8,
    pub max_key_code: u8,
    /// Xorg index order; the first four are the required types.
    pub types: Vec<KeyType>,
    /// Per keycode (256).
    pub keys: Vec<KeySyms>,
    /// `server->key_acts`: per keycode, none or one action per keysym slot.
    pub acts: Vec<Option<Vec<Action>>>,
    pub behaviors: Vec<Behavior>,
    pub explicit: Vec<u8>,
    /// `map->modmap`: the full byte.
    pub modmap: Vec<u8>,
    /// `server->vmodmap`: stored, as Xorg does.
    pub vmodmap: Vec<u16>,
    /// `server->vmods`: each virtual modifier's real mapping.
    pub vmods: [u8; NUM_VMODS],
    /// `compat->sym_interpret`.
    pub compat: Vec<SymInterpret>,
    /// `compat->groups`.
    pub group_compat: [Mods; NUM_GROUPS],
    pub indicators: [IndicatorMap; NUM_INDICATORS],
    /// `indicators->phys_indicators`.
    pub phys_indicators: u32,
    pub names: Names,
    /// `ctrls->num_groups`.
    pub num_groups: u8,
    /// `ctrls->per_key_repeat` as the compat map derives it.
    pub per_key_repeat: [u8; 32],
}

/// `XkbMapChangesRec`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct MapChanges {
    pub changed: u16,
    pub first_type: u8,
    pub num_types: u8,
    pub first_key_sym: u8,
    pub num_key_syms: u8,
    pub first_key_act: u8,
    pub num_key_acts: u8,
    pub first_key_behavior: u8,
    pub num_key_behaviors: u8,
    pub first_key_explicit: u8,
    pub num_key_explicit: u8,
    pub first_modmap_key: u8,
    pub num_modmap_keys: u8,
    pub first_vmodmap_key: u8,
    pub num_vmodmap_keys: u8,
    pub vmods: u16,
}

/// `XkbChangesRec`, the parts the ported handlers fill in, plus the per-key
/// repeat decisions (Xorg writes them into `per_key_repeat`, which it copied
/// from the core feedback first; the core loop owns that copy here).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct XkbChanges {
    pub map: MapChanges,
    /// `changes->ctrls.changed_ctrls & XkbPerKeyRepeatMask` (against the
    /// model's own `per_key_repeat`).
    pub per_key_repeat_changed: bool,
    /// `(keycode, repeats)` for each key `XkbApplyCompatMapToKey` set the
    /// repeat bit of (no `XkbExplicitAutoRepeatMask`).
    pub repeats: Vec<(u8, bool)>,
    /// `changes->indicators.map_changes`.
    pub indicator_map_changes: u32,
    /// `changes->compat.changed_groups`.
    pub compat_changed_groups: u8,
}

/// An `int` stored into a CARD8 field, as C does.
fn byte(v: i32) -> u8 {
    (v & 0xff) as u8
}

/// `XkbPaddedSize`.
pub(crate) fn padded(n: usize) -> usize {
    n.div_ceil(4) * 4
}

/// `_XkbAddKeyChange`, byte arithmetic included: a key past the range grows
/// it by one, whatever the gap.
pub(crate) fn add_key_change(first: &mut u8, num: &mut u8, key: u8) {
    let last = first.wrapping_add(*num);
    if key < *first {
        *first = key;
        *num = last.wrapping_sub(key).wrapping_add(1);
    } else if key > last {
        *num = last.wrapping_sub(*first).wrapping_add(1);
    }
}

/// The virtual modifiers a modifier action names (`XkbModActionVMods`).
pub(crate) fn mod_action_vmods(act: &Action) -> u16 {
    u16::from_be_bytes([act[4], act[5]])
}

/// The virtual modifiers an ISOLock action names.
fn iso_action_vmods(act: &Action) -> u16 {
    u16::from_be_bytes([act[6], act[7]])
}

/// Port of Xorg's `XkbConvertCase` (xkb/xkbUtils.c): `(lower, upper)`.
pub(crate) fn convert_case(sym: u32) -> (u32, u32) {
    let (mut lower, mut upper) = (sym, sym);
    match sym >> 8 {
        0 => match sym {
            0x41..=0x5a | 0xc0..=0xd6 | 0xd8..=0xde => lower += 0x20,
            0x61..=0x7a | 0xe0..=0xf6 | 0xf8..=0xfe => upper -= 0x20,
            _ => {}
        },
        1 => match sym {
            0x1a1 => lower = 0x1b1,
            0x1a3..=0x1a6 | 0x1a9..=0x1ac | 0x1ae..=0x1af => lower += 0x10,
            0x1b1 => upper = 0x1a1,
            0x1b3..=0x1b6 | 0x1b9..=0x1bc | 0x1be..=0x1bf => upper -= 0x10,
            0x1c0..=0x1de => lower += 0x20,
            0x1e0..=0x1fe => upper -= 0x20,
            _ => {}
        },
        2 => match sym {
            0x2a1..=0x2a6 | 0x2ab..=0x2ac => lower += 0x10,
            0x2b1..=0x2b6 | 0x2bb..=0x2bc => upper -= 0x10,
            0x2c5..=0x2de => lower += 0x20,
            0x2e5..=0x2fe => upper -= 0x20,
            _ => {}
        },
        3 => match sym {
            0x3a3..=0x3ac => lower += 0x10,
            0x3b3..=0x3bc => upper -= 0x10,
            0x3bd => lower = 0x3bf,
            0x3bf => upper = 0x3bd,
            0x3c0..=0x3de => lower += 0x20,
            0x3e0..=0x3fe => upper -= 0x20,
            _ => {}
        },
        6 => match sym {
            0x6b1..=0x6bf => lower -= 0x10,
            0x6a1..=0x6af => upper += 0x10,
            0x6e0..=0x6ff => lower -= 0x20,
            0x6c0..=0x6df => upper += 0x20,
            _ => {}
        },
        7 => match sym {
            0x7a1..=0x7ab => lower += 0x10,
            0x7b1..=0x7bb if sym != 0x7b6 && sym != 0x7ba => upper -= 0x10,
            0x7c1..=0x7d9 => lower += 0x20,
            0x7e1..=0x7f9 if sym != 0x7f3 => upper -= 0x20,
            _ => {}
        },
        _ => {}
    }
    (lower, upper)
}

/// `XkbKSIsKeypad`: `XK_KP_Space..=XK_KP_Equal`.
pub(crate) fn is_keypad(sym: u32) -> bool {
    (0xff80..=0xffbd).contains(&sym)
}

impl XkbDesc {
    /// An empty description over keycodes 8..=255 (Xorg's range, which
    /// yserver never changes).
    pub(crate) fn empty() -> Self {
        Self {
            min_key_code: 8,
            max_key_code: 255,
            types: Vec::new(),
            keys: vec![KeySyms::default(); 256],
            acts: vec![None; 256],
            behaviors: vec![Behavior::default(); 256],
            explicit: vec![0; 256],
            modmap: vec![0; 256],
            vmodmap: vec![0; 256],
            vmods: [0; NUM_VMODS],
            compat: Vec::new(),
            group_compat: [Mods::default(); NUM_GROUPS],
            indicators: [IndicatorMap::default(); NUM_INDICATORS],
            phys_indicators: 0,
            names: Names::default(),
            num_groups: 1,
            per_key_repeat: [0; 32],
        }
    }

    /// `XkbNumKeys`.
    pub(crate) fn num_keys(&self) -> u8 {
        self.max_key_code
            .wrapping_sub(self.min_key_code)
            .wrapping_add(1)
    }

    /// `XkbVirtualModsToReal` / `XkbMaskForVMask`.
    pub(crate) fn vmods_to_real(&self, vmask: u16) -> u8 {
        (0..NUM_VMODS)
            .filter(|i| vmask & (1 << i) != 0)
            .fold(0, |acc, i| acc | self.vmods[i])
    }

    /// `XkbKeyGroupWidth`: the level count of group `g`'s type.
    pub(crate) fn group_width(&self, kc: u8, g: usize) -> usize {
        let t = usize::from(
            self.keys[usize::from(kc)]
                .kt_index
                .get(g)
                .copied()
                .unwrap_or(0),
        );
        self.types.get(t).map_or(0, |t| usize::from(t.num_levels))
    }

    /// `XkbKeyHasActions`.
    pub(crate) fn key_has_actions(&self, kc: u8) -> bool {
        self.acts[usize::from(kc)].is_some()
    }

    /// The key's actions, one per keysym slot (`XkbKeyActionsPtr` over
    /// `XkbKeyNumSyms`), `NoAction` where the key has none.
    pub(crate) fn key_action(&self, kc: u8, slot: usize) -> Action {
        self.acts[usize::from(kc)]
            .as_ref()
            .and_then(|a| a.get(slot).copied())
            .unwrap_or([0; 8])
    }

    /// `XkbResizeKeyActions(key, needed)`: none for 0, else one action per
    /// slot, keeping the existing ones.
    fn resize_key_actions(&mut self, kc: u8, needed: usize) -> &mut Vec<Action> {
        let slot = &mut self.acts[usize::from(kc)];
        let v = slot.get_or_insert_with(Vec::new);
        v.resize(needed, [0; 8]);
        v
    }

    /// Whether `kc` repeats in `per_key_repeat`.
    pub(crate) fn repeats(&self, kc: u8) -> bool {
        self.per_key_repeat[usize::from(kc >> 3)] & (1 << (kc & 7)) != 0
    }

    fn set_repeat(&mut self, kc: u8, on: bool) {
        let (i, bit) = (usize::from(kc >> 3), 1u8 << (kc & 7));
        if on {
            self.per_key_repeat[i] |= bit;
        } else {
            self.per_key_repeat[i] &= !bit;
        }
    }

    /// `_XkbFindMatchingInterp`: the first matching interpret for `sym`,
    /// preferring one that names the keysym over an `Any` one.
    fn find_matching_interp(&self, sym: u32, real_mods: u8, level: usize) -> Option<usize> {
        let mut rtrn = None;
        for (i, interp) in self.compat.iter().enumerate() {
            if interp.sym != 0 && interp.sym != sym {
                continue;
            }
            let mods = if level == 0 || interp.match_ & SI_LEVEL_ONE_ONLY == 0 {
                real_mods
            } else {
                0
            };
            let matched = match interp.match_ & SI_OP_MASK {
                SI_NONE_OF => interp.mods & mods == 0,
                SI_ANY_OF_OR_NONE => mods == 0 || interp.mods & mods != 0,
                SI_ANY_OF => interp.mods & mods != 0,
                SI_ALL_OF => interp.mods & mods == interp.mods,
                SI_EXACTLY => interp.mods == mods,
                _ => false,
            };
            if matched {
                if interp.sym != 0 {
                    return Some(i);
                }
                rtrn.get_or_insert(i);
            }
        }
        rtrn
    }

    /// `_XkbSetActionKeyMods`: fill a modifier or ISOLock action's mask from
    /// the key's modmap (`UseModMapMods`) and its virtual modifiers.
    pub(crate) fn set_action_key_mods(&self, act: &mut Action, mods: u8) {
        match act[0] {
            SA_SET_MODS | SA_LATCH_MODS | SA_LOCK_MODS => {
                if act[1] & SA_USE_MOD_MAP_MODS != 0 {
                    act[2] = mods;
                    act[3] = mods;
                }
                let tmp = mod_action_vmods(act);
                if tmp != 0 {
                    act[2] |= self.vmods_to_real(tmp);
                }
            }
            SA_ISO_LOCK => {
                if act[1] & SA_USE_MOD_MAP_MODS != 0 {
                    act[2] = mods;
                    act[3] = mods;
                }
                let tmp = iso_action_vmods(act);
                if tmp != 0 {
                    act[2] |= self.vmods_to_real(tmp);
                }
            }
            _ => {}
        }
    }

    /// Port of Xorg's `XkbApplyCompatMapToKey` (xkb/XKBMisc.c).
    pub(crate) fn apply_compat_map_to_key(&mut self, key: u8, changes: &mut XkbChanges) {
        if key < self.min_key_code || key > self.max_key_code {
            return;
        }
        let k = usize::from(key);
        let explicit = self.explicit[k];
        if explicit & EXPLICIT_INTERPRET != 0 {
            return;
        }
        let mods = self.modmap[k];
        let n_syms = self.keys[k].num_syms();
        let width = usize::from(self.keys[k].width).max(1);
        let mut interps: Vec<Option<usize>> = vec![None; n_syms];
        let mut found = 0;
        for (n, slot) in interps.iter_mut().enumerate() {
            let sym = self.keys[k].syms[n];
            if sym != 0 {
                let level = n % width;
                *slot = self
                    .find_matching_interp(sym, mods, level)
                    .filter(|&i| self.compat[i].act[0] != SA_NO_ACTION);
                if slot.is_some() {
                    found += 1;
                }
            }
        }
        let mut changed = 0u16;
        if found == 0 {
            if self.acts[k].is_some() {
                self.acts[k] = None;
                changed |= KEY_ACTIONS_MASK;
            }
        } else {
            changed |= KEY_ACTIONS_MASK;
            let mut new_vmodmask = 0u16;
            let mut acts = vec![[0u8; 8]; n_syms];
            for (n, interp) in interps.iter().enumerate() {
                let Some(i) = *interp else {
                    continue;
                };
                let si = self.compat[i];
                let mut act = si.act;
                let eff = if n == 0 || si.match_ & SI_LEVEL_ONE_ONLY == 0 {
                    if si.virtual_mod != NO_MODIFIER {
                        new_vmodmask |= 1 << (si.virtual_mod & 0x0f);
                    }
                    mods
                } else {
                    0
                };
                self.set_action_key_mods(&mut act, eff);
                acts[n] = act;
            }
            *self.resize_key_actions(key, n_syms) = acts;
            if explicit & EXPLICIT_VMODMAP == 0 && self.vmodmap[k] != new_vmodmask {
                changed |= VIRTUAL_MOD_MAP_MASK;
                self.vmodmap[k] = new_vmodmask;
            }
            if let Some(i0) = interps[0] {
                let si = self.compat[i0];
                if si.flags & SI_LOCKING_KEY != 0 && explicit & EXPLICIT_BEHAVIOR == 0 {
                    self.behaviors[k].kind = KB_LOCK;
                    changed |= KEY_BEHAVIORS_MASK;
                }
                if explicit & EXPLICIT_AUTO_REPEAT == 0 {
                    let old = self.repeats(key);
                    let on = si.flags & SI_AUTO_REPEAT != 0;
                    self.set_repeat(key, on);
                    changes.repeats.push((key, on));
                    if old != on {
                        changes.per_key_repeat_changed = true;
                    }
                }
            }
        }
        if found == 0 || interps.first().copied().flatten().is_none() {
            if explicit & EXPLICIT_AUTO_REPEAT == 0 {
                let old = self.repeats(key);
                self.set_repeat(key, true);
                changes.repeats.push((key, true));
                if !old {
                    changes.per_key_repeat_changed = true;
                }
            }
            if explicit & EXPLICIT_BEHAVIOR == 0 && self.behaviors[k].kind == KB_LOCK {
                self.behaviors[k].kind = KB_DEFAULT;
                changed |= KEY_BEHAVIORS_MASK;
            }
        }
        let mc = &mut changes.map;
        let tmp = changed & mc.changed;
        if tmp & KEY_ACTIONS_MASK != 0 {
            add_key_change(&mut mc.first_key_act, &mut mc.num_key_acts, key);
        } else if changed & KEY_ACTIONS_MASK != 0 {
            mc.changed |= KEY_ACTIONS_MASK;
            mc.first_key_act = key;
            mc.num_key_acts = 1;
        }
        if tmp & KEY_BEHAVIORS_MASK != 0 {
            add_key_change(&mut mc.first_key_behavior, &mut mc.num_key_behaviors, key);
        } else if changed & KEY_BEHAVIORS_MASK != 0 {
            mc.changed |= KEY_BEHAVIORS_MASK;
            mc.first_key_behavior = key;
            mc.num_key_behaviors = 1;
        }
        if tmp & VIRTUAL_MOD_MAP_MASK != 0 {
            add_key_change(&mut mc.first_vmodmap_key, &mut mc.num_vmodmap_keys, key);
        } else if changed & VIRTUAL_MOD_MAP_MASK != 0 {
            mc.changed |= VIRTUAL_MOD_MAP_MASK;
            mc.first_vmodmap_key = key;
            mc.num_vmodmap_keys = 1;
        }
        mc.changed |= changed;
    }

    /// Port of Xorg's `XkbUpdateDescActions` (xkb/xkbUtils.c).
    pub(crate) fn update_desc_actions(&mut self, first: u8, num: u8, changes: &mut XkbChanges) {
        let end = u16::from(first) + u16::from(num);
        for key in u16::from(first)..end {
            if let Ok(key) = u8::try_from(key) {
                self.apply_compat_map_to_key(key, changes);
            }
        }

        if changes.map.changed & (VIRTUAL_MOD_MAP_MASK | MODIFIER_MAP_MASK) != 0 {
            let mut new_vmods = [0u8; NUM_VMODS];
            let mut present = 0u16;
            for key in self.min_key_code..=self.max_key_code {
                let v = self.vmodmap[usize::from(key)];
                if v == 0 {
                    continue;
                }
                for (i, m) in new_vmods.iter_mut().enumerate() {
                    if v & (1 << i) != 0 {
                        present |= 1 << i;
                        *m |= self.modmap[usize::from(key)];
                    }
                }
            }
            for (i, &new_vmod) in new_vmods.iter().enumerate() {
                if present & (1 << i) != 0 && new_vmod != self.vmods[i] {
                    changes.map.changed |= VIRTUAL_MODS_MASK;
                    changes.map.vmods |= 1 << i;
                    self.vmods[i] = new_vmod;
                }
            }
        }
        if changes.map.changed & VIRTUAL_MODS_MASK != 0 {
            let vmods = changes.map.vmods;
            self.apply_virtual_mod_changes(vmods, changes);
        }

        let mc = &mut changes.map;
        if mc.changed & KEY_ACTIONS_MASK != 0 {
            let old_last = mc
                .first_key_act
                .wrapping_add(mc.num_key_acts)
                .wrapping_sub(1);
            let mut new_last = first.wrapping_add(num).wrapping_sub(1);
            if first < mc.first_key_act {
                mc.first_key_act = first;
            }
            if new_last > old_last {
                new_last = old_last;
            }
            mc.num_key_acts = new_last.wrapping_sub(mc.first_key_act).wrapping_add(1);
        } else {
            mc.changed |= KEY_ACTIONS_MASK;
            mc.first_key_act = first;
            mc.num_key_acts = num;
        }
    }

    /// Port of Xorg's `XkbApplyVirtualModChanges` (xkb/XKBMisc.c), minus
    /// the internal/ignore-lock controls (yserver models neither).
    pub(crate) fn apply_virtual_mod_changes(&mut self, changed: u16, changes: &mut XkbChanges) {
        if changed == 0 {
            return;
        }
        for i in 0..self.types.len() {
            if self.types[i].mods.vmods & changed != 0 {
                self.update_key_type_virtual_mods(i, changes);
            }
        }
        for i in 0..NUM_INDICATORS {
            let map = self.indicators[i];
            if map.mods.vmods & changed != 0 {
                let new_mask = self.vmods_to_real(map.mods.vmods) | map.mods.real;
                if new_mask != map.mods.mask {
                    self.indicators[i].mods.mask = new_mask;
                    changes.indicator_map_changes |= 1 << i;
                }
            }
        }
        for g in 0..NUM_GROUPS {
            let gc = self.group_compat[g];
            let new_mask = self.vmods_to_real(gc.vmods) | gc.real;
            if gc.mask != new_mask {
                self.group_compat[g].mask = new_mask;
                changes.compat_changed_groups |= 1 << g;
            }
        }
        let (mut high, mut low) = (0i32, -1i32);
        for key in self.min_key_code..=self.max_key_code {
            let k = usize::from(key);
            let Some(acts) = self.acts[k].clone() else {
                continue;
            };
            let mut acts = acts;
            let mut hit = false;
            for act in acts.iter_mut().take(self.keys[k].num_syms()) {
                if act[0] != SA_NO_ACTION && self.update_action_virtual_mods(act, changed) {
                    hit = true;
                }
            }
            if hit {
                if low < 0 {
                    low = i32::from(key);
                }
                high = i32::from(key);
            }
            self.acts[k] = Some(acts);
        }
        if low > 0 {
            let mc = &mut changes.map;
            if mc.changed & KEY_ACTIONS_MASK != 0 {
                if i32::from(mc.first_key_act) < low {
                    low = i32::from(mc.first_key_act);
                }
                let last = i32::from(mc.first_key_act) + i32::from(mc.num_key_acts) - 1;
                if last > high {
                    high = last;
                }
            }
            mc.changed |= KEY_ACTIONS_MASK;
            mc.first_key_act = byte(low);
            mc.num_key_acts = byte(high - low + 1);
        }
    }

    /// Port of `XkbUpdateActionVirtualMods`, ISOLock's operator-precedence
    /// slip (`(vmods != 0) & changed`) included.
    fn update_action_virtual_mods(&self, act: &mut Action, changed: u16) -> bool {
        match act[0] {
            SA_SET_MODS | SA_LATCH_MODS | SA_LOCK_MODS => {
                let tmp = mod_action_vmods(act);
                if tmp & changed != 0 {
                    act[2] = act[3] | self.vmods_to_real(tmp);
                    return true;
                }
            }
            SA_ISO_LOCK => {
                let tmp = iso_action_vmods(act);
                if u16::from(tmp != 0) & changed != 0 {
                    act[2] = act[3] | self.vmods_to_real(tmp);
                    return true;
                }
            }
            _ => {}
        }
        false
    }

    /// Port of `XkbUpdateKeyTypeVirtualMods`.
    fn update_key_type_virtual_mods(&mut self, index: usize, changes: &mut XkbChanges) {
        let tvm = self.types[index].mods.vmods;
        let mask = self.vmods_to_real(tvm);
        let resolved: Vec<(u8, bool)> = self.types[index]
            .map
            .iter()
            .map(|e| {
                if e.mods.vmods != 0 {
                    let m = self.vmods_to_real(e.mods.vmods);
                    (e.mods.real | m, m != 0)
                } else {
                    (e.mods.mask, true)
                }
            })
            .collect();
        let t = &mut self.types[index];
        t.mods.mask = t.mods.real | mask;
        if !t.map.is_empty() && tvm != 0 {
            for (e, (m, active)) in t.map.iter_mut().zip(resolved) {
                if e.mods.vmods != 0 {
                    e.mods.mask = m;
                    e.active = active;
                } else {
                    e.active = true;
                }
            }
        }
        let type_ndx = i32::try_from(index).unwrap_or(i32::MAX);
        let mc = &mut changes.map;
        if mc.changed & KEY_TYPES_MASK != 0 {
            let last = i32::from(mc.first_type) + i32::from(mc.num_types) - 1;
            if type_ndx < i32::from(mc.first_type) {
                mc.first_type = byte(type_ndx);
                mc.num_types = byte(last - type_ndx + 1);
            } else if type_ndx > last {
                mc.num_types = byte(type_ndx - i32::from(mc.first_type) + 1);
            }
        } else {
            mc.changed |= KEY_TYPES_MASK;
            mc.first_type = byte(type_ndx);
            mc.num_types = 1;
        }
    }

    /// Port of Xorg's `XkbKeyTypesForCoreSymbols` (xkb/XKBMisc.c, §12.2 and
    /// §12.4): the group count, the types in `types_inout` and the keysyms
    /// `groupsWidth` apart in `xkb_syms`.
    fn key_types_for_core_symbols(
        &self,
        core: &[u32],
        protected: u8,
        types_inout: &mut [usize; NUM_GROUPS],
        xkb_syms: &mut Vec<u32>,
    ) -> usize {
        let map_width = core.len();
        let cs = |i: usize| core.get(i).copied().unwrap_or(0);
        let prot = |i: usize| protected & (1 << i) != 0;
        let num_types = self.types.len();
        let levels = |t: usize| self.types.get(t).map_or(0, |t| usize::from(t.num_levels));
        let mut n_syms = [0usize; NUM_GROUPS];
        let mut gw = 2usize;
        for i in 0..NUM_GROUPS {
            if prot(i) && types_inout[i] < num_types {
                n_syms[i] = levels(types_inout[i]);
                gw = gw.max(n_syms[i]);
            } else {
                types_inout[i] = 1; // XkbTwoLevelIndex
                n_syms[i] = 2;
            }
        }
        n_syms[0] = n_syms[0].max(2);
        n_syms[1] = n_syms[1].max(2);
        let off = |g: usize, l: usize| g * gw + l;
        let mut out = vec![0u32; NUM_GROUPS * gw];
        out[off(0, 0)] = cs(0);
        out[off(0, 1)] = cs(1);
        for i in 2..n_syms[0] {
            out[off(0, i)] = cs(2 + i);
        }
        out[off(1, 0)] = cs(2);
        out[off(1, 1)] = cs(3);
        let tmp = 2 + (n_syms[0] - 2);
        for i in 2..n_syms[1] {
            out[off(1, i)] = cs(tmp + i);
        }
        let mut replicated = false;
        if protected & !1 == 0 {
            let width = n_syms[0];
            replicated = true;
            if (width > 0 && cs(0) != cs(2)) || (width > 1 && cs(1) != cs(3)) {
                replicated = false;
            }
            let mut i = 2;
            while i < width && replicated {
                if cs(2 + i) != cs(i + width) {
                    replicated = false;
                }
                i += 1;
            }
            let mut j = 2;
            while replicated && j < NUM_GROUPS && map_width >= width * (j + 1) {
                let mut i = 0;
                while i < width && replicated {
                    if cs(if i < 2 { i } else { 2 + i }) != cs(i + width * j) {
                        replicated = false;
                    }
                    i += 1;
                }
                j += 1;
            }
        }
        let mut n_groups;
        if replicated {
            n_syms[1] = 0;
            n_syms[2] = 0;
            n_syms[3] = 0;
            n_groups = 1;
        } else {
            let mut tmp = n_syms[0] + n_syms[1];
            if tmp >= map_width && protected & 0b1100 == 0 {
                n_syms[2] = 0;
                n_syms[3] = 0;
                n_groups = 2;
            } else {
                n_groups = 3;
                for i in 0..n_syms[2] {
                    out[off(2, i)] = cs(tmp);
                    tmp += 1;
                }
                if tmp < map_width || protected & 0b1000 != 0 {
                    n_groups = 4;
                    for i in 0..n_syms[3] {
                        out[off(3, i)] = cs(tmp);
                        tmp += 1;
                    }
                } else {
                    n_syms[3] = 0;
                }
            }
        }
        let mut empty = 0u8;
        for i in 0..n_groups {
            let b = off(i, 0);
            if n_syms[i] > 1 && out[b + 1] == 0 && out[b] != 0 {
                let (lower, upper) = convert_case(out[b]);
                if upper != lower {
                    out[b] = lower;
                    out[b + 1] = upper;
                    if !prot(i) {
                        types_inout[i] = 2; // XkbAlphabeticIndex
                    }
                } else if !prot(i) {
                    types_inout[i] = 0; // XkbOneLevelIndex
                }
            }
            if !prot(i) && types_inout[i] == 1 {
                if is_keypad(out[b]) || is_keypad(out[b + 1]) {
                    types_inout[i] = 3; // XkbKeypadIndex
                } else {
                    let (lower, upper) = convert_case(out[b]);
                    if out[b] == lower && out[b + 1] == upper {
                        types_inout[i] = 2;
                    }
                }
            }
            if out[b] == 0 && (1..n_syms[i]).all(|n| out[b + n] == 0) {
                empty |= 1 << i;
            }
        }
        if empty != 0 {
            for i in (0..n_groups).rev() {
                if empty & (1 << i) == 0 || prot(i) {
                    break;
                }
                n_groups -= 1;
            }
        }
        if n_groups < 1 {
            *xkb_syms = out;
            return 0;
        }
        if n_groups > 1 && empty & 0b11 == 0b10 {
            if protected & 0b11 == 0 {
                n_syms[1] = n_syms[0];
                types_inout[1] = types_inout[0];
                out.copy_within(0..2, 2);
            } else if types_inout[0] == types_inout[1] {
                let n0 = n_syms[0];
                out.copy_within(0..n0, n0);
            }
        }
        if n_groups > 1 {
            let mut all_one = levels(types_inout[0]) == 1;
            let mut same = true;
            let mut canonical = true;
            let mut i = 1;
            while (all_one || same) && i < n_groups {
                same = same && types_inout[i] == types_inout[0];
                if all_one {
                    all_one = levels(types_inout[i]) == 1;
                }
                if types_inout[i] > 3 {
                    canonical = false;
                }
                i += 1;
            }
            if (same || canonical) && protected & (EXPLICIT_KEY_TYPES & !1) == 0 {
                let mut identical = true;
                let mut i = 1;
                while identical && i < n_groups {
                    if n_syms[i] != n_syms[0] {
                        identical = false;
                    }
                    let mut s = 0;
                    while identical && s < n_syms[i] {
                        if out[off(i, s)] != out[s] {
                            identical = false;
                        }
                        s += 1;
                    }
                    i += 1;
                }
                if identical {
                    n_groups = 1;
                }
            }
            if all_one && n_groups > 1 {
                let mut p = n_syms[0];
                n_syms[0] = 1;
                for i in 1..n_groups {
                    out[i] = out[p];
                    p += n_syms[i];
                    n_syms[i] = 1;
                }
            }
        }
        *xkb_syms = out;
        n_groups
    }

    /// Port of Xorg's `XkbChangeTypesOfKey` (xkb/XKBMisc.c) for all groups
    /// (`XkbAllGroupsMask`, the only way yserver calls it).
    fn change_types_of_key(
        &mut self,
        key: u8,
        n_groups: usize,
        new_types: &[usize; NUM_GROUPS],
        mc: &mut MapChanges,
    ) {
        let k = usize::from(key);
        if n_groups == 0 {
            self.keys[k].kt_index = [0; 4];
            self.keys[k].group_info &= 0xf0;
            self.keys[k].syms.clear();
            self.acts[k] = None;
            return;
        }
        let n_old_groups = self.keys[k].num_groups();
        let old_width = usize::from(self.keys[k].width);
        let width = new_types[..n_groups]
            .iter()
            .map(|&t| self.types.get(t).map_or(0, |t| usize::from(t.num_levels)))
            .max()
            .unwrap_or(0);
        if n_groups > usize::from(self.num_groups) {
            self.num_groups = u8::try_from(n_groups).unwrap_or(4);
        }
        let n_groups_u8 = u8::try_from(n_groups).unwrap_or(4);
        if width != old_width || n_groups != n_old_groups {
            if n_old_groups == 0 {
                self.keys[k].syms = vec![0; width * n_groups];
                self.keys[k].group_info = (self.keys[k].group_info & 0xf0) | n_groups_u8;
                self.keys[k].width = u8::try_from(width).unwrap_or(u8::MAX);
                for (i, &t) in new_types.iter().enumerate().take(n_groups) {
                    self.keys[k].kt_index[i] = u8::try_from(t).unwrap_or(0);
                }
                return;
            }
            let old_syms = self.keys[k].syms.clone();
            let mut syms = vec![0u32; width * n_groups];
            let n_copy_of = |me: &Self, i: usize| {
                let old_levels = me.group_width(key, i);
                let new_levels = me
                    .types
                    .get(new_types[i])
                    .map_or(0, |t| usize::from(t.num_levels));
                old_levels.min(new_levels)
            };
            for i in 0..n_groups.min(n_old_groups) {
                let n = n_copy_of(self, i);
                for l in 0..n {
                    syms[i * width + l] = old_syms.get(i * old_width + l).copied().unwrap_or(0);
                }
            }
            if let Some(old_acts) = self.acts[k].clone() {
                let mut acts = vec![[0u8; 8]; width * n_groups];
                for i in 0..n_groups.min(n_old_groups) {
                    let n = n_copy_of(self, i);
                    for l in 0..n {
                        acts[i * width + l] =
                            old_acts.get(i * old_width + l).copied().unwrap_or([0; 8]);
                    }
                }
                self.acts[k] = Some(acts);
            }
            self.keys[k].syms = syms;
            self.keys[k].group_info = (self.keys[k].group_info & 0xf0) | n_groups_u8;
            self.keys[k].width = u8::try_from(width).unwrap_or(u8::MAX);
        }
        for (i, &t) in new_types.iter().enumerate().take(n_groups) {
            self.keys[k].kt_index[i] = u8::try_from(t).unwrap_or(0);
        }
        self.keys[k].width = u8::try_from(width).unwrap_or(u8::MAX);
        if mc.changed & KEY_SYMS_MASK != 0 {
            add_key_change(&mut mc.first_key_sym, &mut mc.num_key_syms, key);
        } else {
            mc.changed |= KEY_SYMS_MASK;
            mc.first_key_sym = key;
            mc.num_key_syms = 1;
        }
    }

    /// Port of Xorg's `XkbUpdateKeyTypesFromCore` (xkb/xkbUtils.c): the
    /// ChangeKeyboardMapping rows `core` (`kpk` keysyms per keycode, from
    /// `first`) become each key's groups, types and keysyms.
    pub(crate) fn update_key_types_from_core(
        &mut self,
        core: &[u32],
        kpk: u8,
        first: u8,
        num: u8,
        changes: &mut XkbChanges,
    ) {
        let mut num = num;
        if u16::from(first) + u16::from(num) - 1 > u16::from(self.max_key_code) {
            num = self.max_key_code.wrapping_sub(first).wrapping_add(1);
        }
        let kpk = usize::from(kpk);
        for i in 0..usize::from(num) {
            let key = first.wrapping_add(u8::try_from(i).unwrap_or(0));
            let k = usize::from(key);
            let row = core.get(i * kpk..(i + 1) * kpk).unwrap_or(&[]);
            let explicit = self.explicit[k] & EXPLICIT_KEY_TYPES;
            let mut types: [usize; NUM_GROUPS] =
                std::array::from_fn(|g| usize::from(self.keys[k].kt_index[g]));
            let mut tsyms = Vec::new();
            let n_groups = self.key_types_for_core_symbols(row, explicit, &mut types, &mut tsyms);
            let mut mc = changes.map;
            self.change_types_of_key(key, n_groups, &types, &mut mc);
            changes.map = mc;
            let n = self.keys[k].num_syms();
            for (s, slot) in self.keys[k].syms.iter_mut().enumerate().take(n) {
                *slot = tsyms.get(s).copied().unwrap_or(0);
            }
        }
        let mc = &mut changes.map;
        if mc.changed & KEY_SYMS_MASK != 0 {
            let old_last = mc
                .first_key_sym
                .wrapping_add(mc.num_key_syms)
                .wrapping_sub(1);
            let mut new_last = first.wrapping_add(num).wrapping_sub(1);
            if first < mc.first_key_sym {
                mc.first_key_sym = first;
            }
            if old_last > new_last {
                new_last = old_last;
            }
            mc.num_key_syms = new_last.wrapping_sub(mc.first_key_sym).wrapping_add(1);
        } else {
            mc.changed |= KEY_SYMS_MASK;
            mc.first_key_sym = first;
            mc.num_key_syms = num;
        }
    }

    /// Xorg's `XkbApplyMappingChange` for ChangeKeyboardMapping:
    /// `XkbUpdateKeyTypesFromCore` then `XkbUpdateActions` over the keys.
    pub(crate) fn apply_keyboard_mapping(
        &mut self,
        first: u8,
        kpk: u8,
        num: u8,
        core: &[u32],
    ) -> XkbChanges {
        let mut changes = XkbChanges::default();
        if first == 0 || num == 0 {
            return changes;
        }
        self.update_key_types_from_core(core, kpk, first, num, &mut changes);
        self.update_desc_actions(first, num, &mut changes);
        changes
    }

    /// Xorg's `XkbApplyMappingChange` for SetModifierMapping: the modmap
    /// replaced, `XkbUpdateActions` over the whole keycode range.
    pub(crate) fn apply_modifier_mapping(&mut self, modmap: &[u8; 256]) -> XkbChanges {
        let mut changes = XkbChanges::default();
        let num = self.num_keys();
        changes.map.changed |= MODIFIER_MAP_MASK;
        changes.map.first_modmap_key = self.min_key_code;
        changes.map.num_modmap_keys = num;
        self.modmap.copy_from_slice(modmap);
        self.update_desc_actions(self.min_key_code, num, &mut changes);
        changes
    }

    /// The indicators whose map lights in the effective `lit` state from a
    /// cooking state, by name: bit N = indicator N (Xorg `effectiveState`).
    pub(crate) fn indicators_lit(&self, state: &xkbcommon::xkb::State) -> u32 {
        (0..NUM_INDICATORS)
            .filter(|&i| {
                writer::indicator_name(self, i).is_some_and(|name| state.led_name_is_active(&name))
            })
            .fold(0, |bits, i| bits | (1 << i))
    }

    /// Port of Xorg's `XkbGetCoreMap` (xkb/xkbUtils.c): the core
    /// (GetKeyboardMapping) view, one width for the whole map.
    pub(crate) fn core_map(&self) -> crate::kms::xkb::CoreKeyMap {
        let groups: Vec<Vec<Vec<u32>>> = (self.min_key_code..=self.max_key_code)
            .map(|kc| self.key_groups(kc))
            .collect();
        crate::kms::xkb::core_map_from_groups(self.min_key_code, &groups)
    }

    /// A key's groups, each its own type's level count wide.
    pub(crate) fn key_groups(&self, kc: u8) -> Vec<Vec<u32>> {
        let key = &self.keys[usize::from(kc)];
        let w = usize::from(key.width);
        (0..key.num_groups())
            .map(|g| {
                (0..self.group_width(kc, g))
                    .map(|l| key.syms.get(g * w + l).copied().unwrap_or(0))
                    .collect()
            })
            .collect()
    }

    /// Port of Xorg's `generate_modkeymap` (dix/inpututils.c) over the
    /// modmap: `(keycodes_per_modifier, 8 rows)`.
    pub(crate) fn modifier_mapping(&self) -> (u8, Vec<u8>) {
        let mut rows: [Vec<u8>; 8] = Default::default();
        for kc in self.min_key_code..=self.max_key_code {
            for (row, kcs) in rows.iter_mut().enumerate() {
                if self.modmap[usize::from(kc)] & (1u8 << row) != 0 {
                    kcs.push(kc);
                }
            }
        }
        let kpm = rows.iter().map(Vec::len).max().unwrap_or(0);
        let mut data = Vec::with_capacity(8 * kpm);
        for row in &rows {
            for i in 0..kpm {
                data.push(row.get(i).copied().unwrap_or(0));
            }
        }
        (u8::try_from(kpm).unwrap_or(u8::MAX), data)
    }

    /// Xorg's level lookup for a key type: the first active entry whose mask
    /// equals the modifiers under the type's mask; level 0 when none.
    /// Returns `(level, preserve mask)`.
    #[cfg(test)]
    pub(crate) fn type_level(&self, type_index: usize, mods: u8) -> (u8, u8) {
        let Some(t) = self.types.get(type_index) else {
            return (0, 0);
        };
        let m = mods & t.mods.mask;
        for (i, e) in t.map.iter().enumerate() {
            if e.active && e.mods.mask == m {
                let pre = t
                    .preserve
                    .as_ref()
                    .and_then(|p| p.get(i))
                    .map_or(0, |p| p.mask);
                return (e.level, pre);
            }
        }
        (0, 0)
    }

    /// Xorg's `XkbKeyGroupIndex`-style effective group for a key: an
    /// out-of-range group wraps, clamps or redirects per `group_info`.
    #[cfg(test)]
    pub(crate) fn effective_group(&self, kc: u8, group: usize) -> Option<usize> {
        let key = &self.keys[usize::from(kc)];
        let n = key.num_groups();
        if n == 0 {
            return None;
        }
        if group < n {
            return Some(group);
        }
        Some(match key.group_info & 0xc0 {
            REDIRECT_INTO_RANGE => {
                let g = usize::from((key.group_info >> 4) & 0x03);
                if g >= n { 0 } else { g }
            }
            CLAMP_INTO_RANGE => n - 1,
            _ => group % n,
        })
    }
}
