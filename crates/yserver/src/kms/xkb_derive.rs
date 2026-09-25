//! Xorg's server-side derivation of a keymap's key actions, virtual modifier
//! map and virtual modifier bindings (#171).
//!
//! xkbcommon computes all of these when it compiles a keymap but exposes none
//! of them, while XKB clients read Xorg's values in GetMap and in the
//! `XkbMapNotify` ranges of a modifier-map change. This module reads the compat
//! interprets, the key types' modifiers and the keys' explicit properties from
//! xkbcommon's own V1 dump ([`crate::kms::xkb_edit`]) and replays Xorg:
//!
//! - `XkbApplyCompatMapToKey` (xkb/XKBMisc.c): per key, the interpret each
//!   keysym matches (`_XkbFindMatchingInterp`) gives its actions and, for the
//!   interprets that name one, its virtual modifier map;
//! - `XkbUpdateDescActions` / `XkbApplyVirtualModChanges` (xkb/xkbUtils.c,
//!   xkb/XKBMisc.c): after a modifier-map change, which keys' actions and
//!   vmodmap changed, which virtual modifiers got a new real mapping, and the
//!   `XkbChanges` ranges Xorg reports for that, including the range
//!   arithmetic's quirks (`_XkbAddKeyChange` grows a range by one key at a
//!   time, and the final merge keeps the smaller end).

use std::collections::BTreeMap;

use xkbcommon::xkb::{self, Keycode, Keymap};

use crate::kms::xkb_edit::{self, XkbEditError};

/// `XkbNumVirtualMods`.
pub(crate) const NUM_VMODS: usize = 16;

/// `XkbSA_ClearLocks` (Set/LatchMods).
const SA_CLEAR_LOCKS: u8 = 0x01;
/// `XkbSA_LatchToLock` (LatchMods).
const SA_LATCH_TO_LOCK: u8 = 0x02;
/// `XkbSA_UseModMapMods`.
const SA_USE_MOD_MAP_MODS: u8 = 0x04;
/// `XkbSA_LockNoLock` / `XkbSA_LockNoUnlock` (LockMods `affect=`).
const SA_LOCK_NO_LOCK: u8 = 0x01;
const SA_LOCK_NO_UNLOCK: u8 = 0x02;

/// `XkbMapNotify.changed` bits (XKB.h `Xkb*Mask`).
pub(crate) const KEY_TYPES_MASK: u16 = 1 << 0;
pub(crate) const KEY_SYMS_MASK: u16 = 1 << 1;
pub(crate) const MODIFIER_MAP_MASK: u16 = 1 << 2;
pub(crate) const KEY_ACTIONS_MASK: u16 = 1 << 4;
pub(crate) const VIRTUAL_MODS_MASK: u16 = 1 << 6;
pub(crate) const VIRTUAL_MOD_MAP_MASK: u16 = 1 << 7;

/// The four types Xorg's keymap always starts with (`XkbNumRequiredTypes`,
/// in their fixed indices 0..=3); every other type follows in definition
/// order. xkbcommon keeps plain definition order.
const REQUIRED_TYPES: [&str; 4] = ["ONE_LEVEL", "TWO_LEVEL", "ALPHABETIC", "KEYPAD"];

/// `XkbSI_*` match operators.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MatchOp {
    NoneOf,
    AnyOfOrNone,
    AnyOf,
    AllOf,
    Exactly,
}

/// A key action, as far as Xorg's modifier handling looks into it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    /// `SetMods` / `LatchMods` / `LockMods`: wire type 1 / 2 / 3.
    Mods {
        kind: u8,
        flags: u8,
        real_mods: u8,
        vmods: u16,
    },
    /// `ISOLock`: only its virtual modifiers matter here.
    IsoLock { vmods: u16 },
    /// Any other action.
    Other,
}

impl Action {
    /// The virtual modifiers the action names (`XkbModActionVMods`).
    fn vmods(self) -> u16 {
        match self {
            Self::Mods { vmods, .. } | Self::IsoLock { vmods } => vmods,
            Self::Other => 0,
        }
    }
}

#[derive(Clone, Debug)]
struct Interpret {
    /// `None` = `Any` (`NoSymbol` in Xorg).
    sym: Option<u32>,
    op: MatchOp,
    mods: u8,
    level_one_only: bool,
    virtual_mod: Option<u8>,
    /// `None` = no action (`NoAction`).
    action: Option<Action>,
    /// `XkbSI_AutoRepeat`.
    repeat: bool,
}

/// A key's explicit properties (Xorg `server->explicit`), from its entry.
#[derive(Clone, Debug, Default)]
struct ExplicitKey {
    /// `XkbExplicitInterpretMask`: the entry's own actions, per keysym slot.
    actions: Option<Vec<Option<Action>>>,
    /// `XkbExplicitVModMapMask`: the entry's own vmodmap.
    vmodmap: Option<u16>,
    /// `XkbExplicitAutoRepeatMask`: the entry fixes its auto-repeat.
    repeat: bool,
}

#[derive(Clone, Debug, Default)]
struct KeyModel {
    /// Keysyms group-major, `width` per group (`XkbKeySymsPtr`).
    syms: Vec<u32>,
    /// `XkbKeyGroupsWidth`.
    width: usize,
    explicit: ExplicitKey,
}

/// What `XkbApplyCompatMapToKey` finds for one key under a modmap.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct KeyDerivation {
    /// Some keysym matched an interpret with an action (`found`).
    pub found: bool,
    /// The vmodmap the matching interprets give (meaningful when `found`).
    pub vmodmap: u16,
    /// Per keysym slot, the interpret's action and the modifiers
    /// `_XkbSetActionKeyMods` fills its `modMapMods` with.
    pub actions: Vec<Option<(Action, u8)>>,
    /// The auto-repeat of the interpret level 1 of group 1 matched, if it
    /// matched one with an action (`interps[0]`).
    pub level1_repeat: Option<bool>,
}

impl KeyDerivation {
    fn action_vmods(&self) -> u16 {
        self.actions
            .iter()
            .flatten()
            .fold(0, |acc, (a, _)| acc | a.vmods())
    }
}

/// Xorg's view of a compiled keymap's derived parts.
#[derive(Clone, Debug)]
pub(crate) struct KeymapModel {
    /// Virtual modifier names by Xorg index (xkbcommon's declaration
    /// order, which is Xorg's: `mod_get_index(name) - 8`).
    pub vmod_names: Vec<String>,
    /// The real mapping of each virtual modifier (`server->vmods`).
    pub vmods: [u8; NUM_VMODS],
    /// The virtual modifiers of each key type (`type->mods.vmods`), in
    /// Xorg's type order.
    type_vmods: Vec<u16>,
    interprets: Vec<Interpret>,
    keys: Vec<KeyModel>,
    /// `map->modmap`.
    pub modmap: [u8; 256],
    pub min_keycode: u8,
    pub max_keycode: u8,
    /// The vmodmap Xorg keeps for keys no interpret matches any more (see
    /// [`Self::with_stale_vmodmap`]).
    stale_vmodmap: BTreeMap<u8, u16>,
}

/// What a modifier-map change does, as Xorg's `XkbApplyMappingChange`
/// reports it in `XkbMapNotify`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ModmapChange {
    pub changed: u16,
    pub first_type: u8,
    pub n_types: u8,
    pub first_key_act: u8,
    pub n_key_acts: u8,
    pub first_mod_map_key: u8,
    pub n_mod_map_keys: u8,
    pub first_vmod_map_key: u8,
    pub n_vmod_map_keys: u8,
    pub virtual_mods: u16,
    /// `server->vmods` afterwards.
    pub vmods: [u8; NUM_VMODS],
    /// The virtual modifiers some key's vmodmap names afterwards (their
    /// mapping is the modmap of those keys; the others kept theirs).
    pub present: u16,
    /// The vmodmap of the keys no interpret matches afterwards but that
    /// kept one (Xorg leaves such a key's vmodmap alone).
    pub stale_vmodmap: BTreeMap<u8, u16>,
}

impl KeymapModel {
    /// Read `keymap`'s interprets, types and explicit key properties from
    /// its V1 dump, and its virtual modifier mapping from xkbcommon.
    pub(crate) fn new(keymap: &Keymap) -> Result<Self, XkbEditError> {
        let text = crate::kms::xkb_edit::keymap_text(keymap);
        let (vmod_names, vmods) = vmod_mappings(keymap);
        let vmod_bit = |name: &str| {
            vmod_names
                .iter()
                .position(|n| n.eq_ignore_ascii_case(name))
                .map(|i| 1u16 << i)
        };

        let (texts, defaults) = xkb_edit::interpret_texts(&text)?;
        let default_level_one = defaults
            .iter()
            .any(|(f, v)| f.eq_ignore_ascii_case("useModMapMods") && is_level_one(v));
        let default_repeat = defaults
            .iter()
            .any(|(f, v)| f.eq_ignore_ascii_case("repeat") && is_true(v));
        let interprets = texts
            .iter()
            .filter_map(|t| {
                parse_interpret(t, default_level_one, default_repeat, &vmod_bit, &vmod_names)
            })
            .collect();

        let mut type_vmods = vec![0u16; REQUIRED_TYPES.len()];
        for (name, modifiers) in xkb_edit::type_modifier_texts(&text)? {
            let (_, vm) = parse_mods(&modifiers, &vmod_bit);
            match REQUIRED_TYPES.iter().position(|r| *r == name) {
                Some(i) => type_vmods[i] = vm,
                None => type_vmods.push(vm),
            }
        }

        let (min_keycode, max_keycode) = crate::kms::xkb::clamped_keycode_bounds(keymap);
        let mut keys = vec![KeyModel::default(); 256];
        for kc in min_keycode..=max_keycode {
            let key = Keycode::new(u32::from(kc));
            let groups = keymap.num_layouts_for_key(key).min(4);
            let width = (0..groups)
                .map(|g| keymap.num_levels_for_key(key, g))
                .max()
                .unwrap_or(0) as usize;
            let mut syms = Vec::with_capacity(width * groups as usize);
            for g in 0..groups {
                let levels = keymap.num_levels_for_key(key, g) as usize;
                for l in 0..width {
                    syms.push(if l < levels {
                        keymap
                            .key_get_syms_by_level(key, g, u32::try_from(l).unwrap_or(0))
                            .first()
                            .map_or(0, |s| s.raw())
                    } else {
                        0
                    });
                }
            }
            keys[usize::from(kc)] = KeyModel {
                syms,
                width,
                explicit: ExplicitKey::default(),
            };
        }
        for (name, explicit) in xkb_edit::key_explicit_texts(&text)? {
            let Some(kc) = keymap
                .key_by_name(&name)
                .and_then(|k| u8::try_from(k.raw()).ok())
            else {
                continue;
            };
            let key = &mut keys[usize::from(kc)];
            if !explicit.actions.is_empty() {
                let mut slots = vec![None; key.syms.len()];
                for (group, actions) in &explicit.actions {
                    for (level, a) in actions.iter().enumerate() {
                        let slot = (group.saturating_sub(1)) * key.width + level;
                        if level < key.width
                            && let Some(s) = slots.get_mut(slot)
                        {
                            *s = parse_action(a, &vmod_bit);
                        }
                    }
                }
                key.explicit.actions = Some(slots);
            }
            if let Some(v) = &explicit.vmods {
                key.explicit.vmodmap = Some(parse_mods(v, &vmod_bit).1);
            }
            key.explicit.repeat = explicit.repeat;
        }

        Ok(Self {
            vmod_names,
            vmods,
            type_vmods,
            interprets,
            keys,
            modmap: crate::kms::xkb::keymap_modmap(keymap),
            min_keycode,
            max_keycode,
            stale_vmodmap: BTreeMap::new(),
        })
    }

    /// The vmodmap Xorg still has for keys no interpret matches any more:
    /// `XkbApplyCompatMapToKey` leaves a key's vmodmap alone when it finds
    /// nothing, so after a mapping change such a key keeps the vmodmap its
    /// interprets gave it before. xkbcommon has no such state; the backend
    /// keeps it beside the keymap ([`ModmapChange::stale_vmodmap`]).
    pub(crate) fn with_stale_vmodmap(mut self, stale: &BTreeMap<u8, u16>) -> Self {
        self.stale_vmodmap = stale.clone();
        self
    }

    /// `_XkbFindMatchingInterp`: the first matching interpret for `sym`,
    /// preferring one that names the keysym over an `Any` one.
    fn find_interp(&self, sym: u32, real_mods: u8, level: usize) -> Option<&Interpret> {
        let mut any = None;
        for interp in &self.interprets {
            if interp.sym.is_some_and(|s| s != sym) {
                continue;
            }
            let mods = if level == 0 || !interp.level_one_only {
                real_mods
            } else {
                0
            };
            let matched = match interp.op {
                MatchOp::NoneOf => interp.mods & mods == 0,
                MatchOp::AnyOfOrNone => mods == 0 || interp.mods & mods != 0,
                MatchOp::AnyOf => interp.mods & mods != 0,
                MatchOp::AllOf => interp.mods & mods == interp.mods,
                MatchOp::Exactly => interp.mods == mods,
            };
            if matched {
                if interp.sym.is_some() {
                    return Some(interp);
                }
                any.get_or_insert(interp);
            }
        }
        any
    }

    /// `XkbApplyCompatMapToKey` for key `kc` with modmap `mods`, ignoring its
    /// explicit properties (the callers check those).
    pub(crate) fn derive_key(&self, kc: u8, mods: u8) -> KeyDerivation {
        let key = &self.keys[usize::from(kc)];
        let mut out = KeyDerivation {
            actions: vec![None; key.syms.len()],
            ..KeyDerivation::default()
        };
        let mut vmodmap = 0u16;
        for (n, &sym) in key.syms.iter().enumerate() {
            if sym == 0 {
                continue;
            }
            let level = n % key.width.max(1);
            let Some(interp) = self.find_interp(sym, mods, level) else {
                continue;
            };
            let Some(action) = interp.action else {
                continue;
            };
            out.found = true;
            if n == 0 {
                out.level1_repeat = Some(interp.repeat);
            }
            let eff = if n == 0 || !interp.level_one_only {
                if let Some(v) = interp.virtual_mod {
                    vmodmap |= 1 << v;
                }
                mods
            } else {
                0
            };
            out.actions[n] = Some((action, eff));
        }
        out.vmodmap = vmodmap;
        out
    }

    /// The key's auto-repeat as `XkbApplyCompatMapToKey` derives it: the
    /// repeat flag of the interpret level 1 matched, else on (no keysyms, no
    /// interpret, or level 1 matched none). Meaningful when
    /// [`Self::derives_repeat`].
    pub(crate) fn key_repeats(&self, kc: u8) -> bool {
        let d = self.derive_key(kc, self.modmap[usize::from(kc)]);
        match d.level1_repeat {
            Some(repeat) if d.found => repeat,
            _ => true,
        }
    }

    /// Whether a mapping change re-derives the key's auto-repeat: not when
    /// its entry carries its own actions (`XkbApplyCompatMapToKey` skips
    /// `XkbExplicitInterpretMask` keys) or its own `repeat=`.
    pub(crate) fn derives_repeat(&self, kc: u8) -> bool {
        let explicit = &self.keys[usize::from(kc)].explicit;
        explicit.actions.is_none() && !explicit.repeat
    }

    /// The vmodmap of every key (`server->vmodmap`): the entry's own, else
    /// what its interprets give (none for a key with its own actions, which
    /// the interprets skip). A key no interpret matches keeps the vmodmap it
    /// had ([`Self::with_stale_vmodmap`]; none in a keymap as compiled).
    pub(crate) fn vmodmap(&self) -> [u16; 256] {
        let mut out = [0u16; 256];
        for kc in self.min_keycode..=self.max_keycode {
            let key = &self.keys[usize::from(kc)];
            out[usize::from(kc)] = match key.explicit.vmodmap {
                Some(v) => v,
                None if key.explicit.actions.is_some() => 0,
                None => {
                    let d = self.derive_key(kc, self.modmap[usize::from(kc)]);
                    if d.found {
                        d.vmodmap
                    } else {
                        self.stale_vmodmap.get(&kc).copied().unwrap_or(0)
                    }
                }
            };
        }
        out
    }

    /// The key's modifier actions as GetMap sends them (`xkbModAction`
    /// wire bytes, one per keysym slot, `NoAction` where the slot has
    /// none), or `None` when it has no modifier action. Other action types
    /// are not sent.
    pub(crate) fn key_mod_actions(&self, kc: u8) -> Option<Vec<[u8; 8]>> {
        let key = &self.keys[usize::from(kc)];
        let mods = self.modmap[usize::from(kc)];
        let slots: Vec<Option<(Action, u8)>> = match &key.explicit.actions {
            Some(actions) => actions.iter().map(|a| a.map(|a| (a, mods))).collect(),
            None => self.derive_key(kc, mods).actions,
        };
        if !slots
            .iter()
            .flatten()
            .any(|(a, _)| matches!(a, Action::Mods { .. }))
        {
            return None;
        }
        Some(
            slots
                .iter()
                .map(|s| match s {
                    Some((action, eff)) => self.wire_mod_action(*action, *eff),
                    None => [0; 8],
                })
                .collect(),
        )
    }

    /// An action's wire bytes after `_XkbSetActionKeyMods(eff)`; only
    /// modifier actions have any (`[0; 8]` = `NoAction`).
    fn wire_mod_action(&self, action: Action, eff: u8) -> [u8; 8] {
        let Action::Mods {
            kind,
            flags,
            real_mods,
            vmods,
        } = action
        else {
            return [0; 8];
        };
        let real = if flags & SA_USE_MOD_MAP_MODS != 0 {
            eff
        } else {
            real_mods
        };
        let mask = real | self.vmods_to_real(vmods);
        let [vhi, vlo] = vmods.to_be_bytes();
        [kind, flags, mask, real, vhi, vlo, 0, 0]
    }

    /// `XkbMaskForVMask`.
    fn vmods_to_real(&self, vmask: u16) -> u8 {
        (0..NUM_VMODS)
            .filter(|i| vmask & (1 << i) != 0)
            .fold(0, |acc, i| acc | self.vmods[i])
    }

    /// This model with `modmap` in place of the keymap's modmap: the keymap
    /// a modifier-map change leaves, before it is compiled.
    pub(crate) fn with_modmap(&self, modmap: &[u8; 256]) -> Self {
        Self {
            modmap: *modmap,
            ..self.clone()
        }
    }

    /// Xorg's `XkbApplyMappingChange` for a new modmap: which keys'
    /// actions and vmodmap change, the virtual modifier mapping afterwards,
    /// and the `XkbMapNotify` fields.
    pub(crate) fn modmap_change(&self, new_modmap: &[u8; 256]) -> ModmapChange {
        let (min, max) = (self.min_keycode, self.max_keycode);
        let num = max.wrapping_sub(min).wrapping_add(1);
        let mut c =
            self.update_desc_actions(&self.with_modmap(new_modmap), min, num, MODIFIER_MAP_MASK);
        c.first_mod_map_key = min;
        c.n_mod_map_keys = num;
        c
    }

    /// Xorg's `XkbUpdateDescActions(first, num)` (with
    /// `XkbApplyVirtualModChanges`) after the keys `first..first+num` got new
    /// keysyms (ChangeKeyboardMapping) or the modmap changed
    /// (SetModifierMapping): `self` is the keymap before, `new` the one after
    /// (same keycodes), `changed` the `XkbMapNotify` bits the caller already
    /// set. Returns the change with Xorg's ranges, the virtual modifier
    /// mapping afterwards and the vmodmap Xorg keeps for keys no interpret
    /// matches any more.
    pub(crate) fn update_desc_actions(
        &self,
        new: &KeymapModel,
        first: u8,
        num: u8,
        changed: u16,
    ) -> ModmapChange {
        let (min, max) = (new.min_keycode, new.max_keycode);
        let mut c = ModmapChange {
            changed,
            vmods: self.vmods,
            stale_vmodmap: self.stale_vmodmap.clone(),
            ..ModmapChange::default()
        };

        // XkbApplyCompatMapToKey over the keys.
        let mut vmodmap = self.vmodmap();
        let last = (u16::from(first) + u16::from(num)).min(u16::from(max) + 1);
        for kc in (u16::from(first)..last).filter_map(|k| u8::try_from(k).ok()) {
            let i = usize::from(kc);
            if new.keys[i].explicit.actions.is_some() {
                continue; // XkbExplicitInterpretMask: nothing to do
            }
            let derived = new.derive_key(kc, new.modmap[i]);
            let mut key_changed = 0u16;
            if derived.found {
                key_changed |= KEY_ACTIONS_MASK;
                if new.keys[i].explicit.vmodmap.is_none() && vmodmap[i] != derived.vmodmap {
                    key_changed |= VIRTUAL_MOD_MAP_MASK;
                    vmodmap[i] = derived.vmodmap;
                }
                c.stale_vmodmap.remove(&kc);
            } else {
                if self.keys[i].explicit.actions.is_none()
                    && self.derive_key(kc, self.modmap[i]).found
                {
                    // key_acts[key] != 0: the actions go away.
                    key_changed |= KEY_ACTIONS_MASK;
                }
                // The vmodmap stays what it was.
                if vmodmap[i] != 0 && new.keys[i].explicit.vmodmap.is_none() {
                    c.stale_vmodmap.insert(kc, vmodmap[i]);
                } else {
                    c.stale_vmodmap.remove(&kc);
                }
            }
            let tmp = key_changed & c.changed;
            if tmp & KEY_ACTIONS_MASK != 0 {
                add_key_change(&mut c.first_key_act, &mut c.n_key_acts, kc);
            } else if key_changed & KEY_ACTIONS_MASK != 0 {
                c.first_key_act = kc;
                c.n_key_acts = 1;
            }
            if tmp & VIRTUAL_MOD_MAP_MASK != 0 {
                add_key_change(&mut c.first_vmod_map_key, &mut c.n_vmod_map_keys, kc);
            } else if key_changed & VIRTUAL_MOD_MAP_MASK != 0 {
                c.first_vmod_map_key = kc;
                c.n_vmod_map_keys = 1;
            }
            c.changed |= key_changed;
        }

        // New real mappings for the virtual modifiers some vmodmap names.
        if c.changed & (VIRTUAL_MOD_MAP_MASK | MODIFIER_MAP_MASK) != 0 {
            let mut new_vmods = [0u8; NUM_VMODS];
            for kc in min..=max {
                let v = vmodmap[usize::from(kc)];
                c.present |= v;
                for (i, m) in new_vmods.iter_mut().enumerate() {
                    if v & (1 << i) != 0 {
                        *m |= new.modmap[usize::from(kc)];
                    }
                }
            }
            for (i, (&new_vmod, vmod)) in new_vmods.iter().zip(c.vmods.iter_mut()).enumerate() {
                if c.present & (1 << i) != 0 && new_vmod != *vmod {
                    c.changed |= VIRTUAL_MODS_MASK;
                    c.virtual_mods |= 1 << i;
                    *vmod = new_vmod;
                }
            }
        } else {
            c.present = (min..=max).fold(0, |p, kc| p | vmodmap[usize::from(kc)]);
        }

        if c.changed & VIRTUAL_MODS_MASK != 0 {
            // XkbApplyVirtualModChanges: the types naming a changed vmod…
            for (idx, &tv) in new.type_vmods.iter().enumerate() {
                if tv & c.virtual_mods == 0 {
                    continue;
                }
                let idx = i32::try_from(idx).unwrap_or(i32::MAX);
                if c.changed & KEY_TYPES_MASK != 0 {
                    let last = i32::from(c.first_type) + i32::from(c.n_types) - 1;
                    if idx < i32::from(c.first_type) {
                        c.first_type = byte(idx);
                        c.n_types = byte(last - idx + 1);
                    } else if idx > last {
                        c.n_types = byte(idx - i32::from(c.first_type) + 1);
                    }
                } else {
                    c.changed |= KEY_TYPES_MASK;
                    c.first_type = byte(idx);
                    c.n_types = 1;
                }
            }
            // …and the keys whose actions name one.
            let hit: Vec<u8> = (min..=max)
                .filter(|&kc| new.key_action_vmods(kc) & c.virtual_mods != 0)
                .collect();
            if let (Some(&lo), Some(&hi)) = (hit.first(), hit.last()) {
                let (mut lo, mut hi) = (i32::from(lo), i32::from(hi));
                if c.changed & KEY_ACTIONS_MASK != 0 {
                    lo = lo.min(i32::from(c.first_key_act));
                    hi = hi.max(i32::from(c.first_key_act) + i32::from(c.n_key_acts) - 1);
                }
                c.changed |= KEY_ACTIONS_MASK;
                c.first_key_act = byte(lo);
                c.n_key_acts = byte(hi - lo + 1);
            }
        }

        // The closing merge with the keys' range (xkbUtils.c): it keeps the
        // smaller last key, so the range ends where the loop's did.
        if c.changed & KEY_ACTIONS_MASK != 0 {
            let old_last = c.first_key_act.wrapping_add(c.n_key_acts).wrapping_sub(1);
            let mut new_last = first.wrapping_add(num).wrapping_sub(1);
            if first < c.first_key_act {
                c.first_key_act = first;
            }
            if new_last > old_last {
                new_last = old_last;
            }
            c.n_key_acts = new_last.wrapping_sub(c.first_key_act).wrapping_add(1);
        } else {
            c.changed |= KEY_ACTIONS_MASK;
            c.first_key_act = first;
            c.n_key_acts = num;
        }
        c
    }

    /// The virtual modifiers the key's actions name (its own, else what its
    /// interprets give).
    fn key_action_vmods(&self, kc: u8) -> u16 {
        match &self.keys[usize::from(kc)].explicit.actions {
            Some(actions) => actions.iter().flatten().fold(0, |a, x| a | x.vmods()),
            None => self
                .derive_key(kc, self.modmap[usize::from(kc)])
                .action_vmods(),
        }
    }
}

/// An `int` stored into a CARD8 field, as C does.
fn byte(v: i32) -> u8 {
    (v & 0xff) as u8
}

/// `_XkbAddKeyChange`, byte arithmetic included: a key past the range grows
/// it by one, whatever the gap.
fn add_key_change(first: &mut u8, num: &mut u8, key: u8) {
    let last = first.wrapping_add(*num);
    if key < *first {
        *first = key;
        *num = last.wrapping_sub(key).wrapping_add(1);
    } else if key > last {
        *num = last.wrapping_sub(*first).wrapping_add(1);
    }
}

/// The keymap's virtual modifiers (Xorg index order) and the real mapping
/// xkbcommon resolves each to: a state with only that modifier depressed
/// reports it as its real modifiers.
pub(crate) fn vmod_mappings(keymap: &Keymap) -> (Vec<String>, [u8; NUM_VMODS]) {
    let mut names = Vec::new();
    let mut mappings = [0u8; NUM_VMODS];
    for idx in 8..keymap.num_mods().min(8 + NUM_VMODS as u32) {
        let mut state = xkb::State::new(keymap);
        state.update_mask(1 << idx, 0, 0, 0, 0, 0);
        let real = state.serialize_mods(xkb::STATE_MODS_EFFECTIVE) & 0xff;
        mappings[names.len()] = u8::try_from(real).unwrap_or(0);
        names.push(keymap.mod_get_name(idx).to_owned());
    }
    (names, mappings)
}

fn is_true(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "true" | "yes" | "on"
    )
}

fn is_level_one(v: &str) -> bool {
    let v = v.trim();
    v.eq_ignore_ascii_case("level1") || v.eq_ignore_ascii_case("levelone")
}

/// `Shift+Mod1+NumLock` → (real mods, virtual mods); `all`, `none` and hex
/// values as xkbcommon writes them.
fn parse_mods(text: &str, vmod_bit: &dyn Fn(&str) -> Option<u16>) -> (u8, u16) {
    let mut real = 0u8;
    let mut virt = 0u16;
    for part in text.split('+') {
        let p = part.trim();
        if p.is_empty() || p.eq_ignore_ascii_case("none") {
            continue;
        }
        if p.eq_ignore_ascii_case("all") {
            real = 0xff;
            continue;
        }
        if let Some(hex) = p.strip_prefix("0x") {
            real |= u8::try_from(u32::from_str_radix(hex, 16).unwrap_or(0) & 0xff).unwrap_or(0);
            continue;
        }
        if let Some(i) = crate::kms::xkb::REAL_MOD_NAMES
            .iter()
            .position(|n| n.eq_ignore_ascii_case(p))
        {
            real |= 1 << i;
        } else if let Some(bit) = vmod_bit(p) {
            virt |= bit;
        }
    }
    (real, virt)
}

/// `SetMods(modifiers=LevelThree,clearLocks)` → the action; `NoAction()`
/// → `None`.
fn parse_action(text: &str, vmod_bit: &dyn Fn(&str) -> Option<u16>) -> Option<Action> {
    let (name, args) = text.split_once('(').unwrap_or((text, ")"));
    let name = name.trim();
    let args: Vec<&str> = args
        .trim_end()
        .strip_suffix(')')
        .unwrap_or(args)
        .split(',')
        .map(str::trim)
        .collect();
    let mods_arg = || {
        args.iter()
            .find_map(|a| {
                let (k, v) = a.split_once('=')?;
                let k = k.trim();
                (k.eq_ignore_ascii_case("modifiers") || k.eq_ignore_ascii_case("mods"))
                    .then(|| v.trim())
            })
            .unwrap_or("none")
    };
    let has_flag = |f: &str| args.iter().any(|a| a.eq_ignore_ascii_case(f));
    let kind = if name.eq_ignore_ascii_case("NoAction") {
        return None;
    } else if name.eq_ignore_ascii_case("SetMods") {
        1
    } else if name.eq_ignore_ascii_case("LatchMods") {
        2
    } else if name.eq_ignore_ascii_case("LockMods") {
        3
    } else if name.eq_ignore_ascii_case("ISOLock") {
        return Some(Action::IsoLock {
            vmods: parse_mods(mods_arg(), vmod_bit).1,
        });
    } else {
        return Some(Action::Other);
    };
    let mods = mods_arg();
    let mut flags = 0u8;
    let mut named = String::new();
    for part in mods.split('+') {
        if part.trim().eq_ignore_ascii_case("modMapMods") {
            flags |= SA_USE_MOD_MAP_MODS;
        } else {
            named.push_str(part);
            named.push('+');
        }
    }
    let (real_mods, vmods) = parse_mods(&named, vmod_bit);
    if kind == 3 {
        let affect = args.iter().find_map(|a| {
            let (k, v) = a.split_once('=')?;
            k.trim().eq_ignore_ascii_case("affect").then(|| v.trim())
        });
        flags |= match affect.map(str::to_ascii_lowercase).as_deref() {
            Some("lock") => SA_LOCK_NO_UNLOCK,
            Some("unlock") => SA_LOCK_NO_LOCK,
            Some("neither") => SA_LOCK_NO_LOCK | SA_LOCK_NO_UNLOCK,
            _ => 0,
        };
    } else {
        if has_flag("clearLocks") {
            flags |= SA_CLEAR_LOCKS;
        }
        if kind == 2 && has_flag("latchToLock") {
            flags |= SA_LATCH_TO_LOCK;
        }
    }
    Some(Action::Mods {
        kind,
        flags,
        real_mods,
        vmods,
    })
}

/// One `interpret` as Xorg's `XkbSymInterpretRec`; `None` for a keysym
/// this xkbcommon can't name (it can't have compiled it either).
fn parse_interpret(
    t: &xkb_edit::InterpretText,
    default_level_one: bool,
    default_repeat: bool,
    vmod_bit: &dyn Fn(&str) -> Option<u16>,
    vmod_names: &[String],
) -> Option<Interpret> {
    let (sym_text, pred) = match t.head.split_once('+') {
        Some((s, p)) => (s.trim(), p.trim()),
        None => (t.head.trim(), "AnyOfOrNone(all)"),
    };
    let sym = if sym_text.eq_ignore_ascii_case("Any") || sym_text.eq_ignore_ascii_case("NoSymbol") {
        None
    } else {
        let s = xkb::keysym_from_name(sym_text, xkb::KEYSYM_NO_FLAGS).raw();
        if s == 0 {
            log::debug!("xkb: interpret for unknown keysym {sym_text:?} ignored");
            return None;
        }
        Some(s)
    };
    let (op_text, mods_text) = pred.split_once('(').unwrap_or((pred, "all)"));
    let op = match op_text.trim().to_ascii_lowercase().as_str() {
        "noneof" => MatchOp::NoneOf,
        "anyofornone" => MatchOp::AnyOfOrNone,
        "anyof" => MatchOp::AnyOf,
        "allof" => MatchOp::AllOf,
        "exactly" => MatchOp::Exactly,
        other => {
            log::debug!("xkb: interpret match {other:?} ignored");
            return None;
        }
    };
    let (mods, _) = parse_mods(mods_text.trim_end_matches(')'), vmod_bit);
    let mut interp = Interpret {
        sym,
        op,
        mods,
        level_one_only: default_level_one,
        virtual_mod: None,
        action: None,
        repeat: default_repeat,
    };
    for field in &t.fields {
        let Some((k, v)) = field.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        if k.eq_ignore_ascii_case("virtualModifier") || k.eq_ignore_ascii_case("virtualMod") {
            interp.virtual_mod = vmod_names
                .iter()
                .position(|n| n.eq_ignore_ascii_case(v))
                .and_then(|i| u8::try_from(i).ok());
        } else if k.eq_ignore_ascii_case("useModMapMods") {
            interp.level_one_only = is_level_one(v);
        } else if k.eq_ignore_ascii_case("action") {
            interp.action = parse_action(v, vmod_bit);
        } else if k.eq_ignore_ascii_case("repeat") {
            interp.repeat = is_true(v);
        }
    }
    Some(interp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kms::xkb::golden_keymap;

    #[test]
    fn add_key_change_grows_by_one_key() {
        // Xorg's _XkbAddKeyChange: a key right after the range doesn't grow
        // it, one further out grows it by one.
        let (mut first, mut num) = (205u8, 1u8);
        add_key_change(&mut first, &mut num, 206);
        assert_eq!((first, num), (205, 1));
        let (mut first, mut num) = (204u8, 1u8);
        add_key_change(&mut first, &mut num, 205);
        add_key_change(&mut first, &mut num, 206);
        assert_eq!((first, num), (204, 2));
        add_key_change(&mut first, &mut num, 100);
        assert_eq!((first, num), (100, 107));
    }

    /// The virtual modifier table of the frozen keymaps, as Xorg's GetMap
    /// reports it (golden `-vmods` rows of xorg-xkb-set-modifier-mapping.txt).
    #[test]
    fn vmod_mappings_match_xorg() {
        for layout in ["gb", "us"] {
            let (names, vmods) = vmod_mappings(&golden_keymap(layout, None));
            assert_eq!(
                names,
                [
                    "NumLock",
                    "Alt",
                    "LevelThree",
                    "Super",
                    "LevelFive",
                    "Meta",
                    "Hyper",
                    "ScrollLock"
                ],
                "{layout}"
            );
            assert_eq!(
                &vmods[..8],
                &[0x10, 0x08, 0x80, 0x40, 0x20, 0x08, 0x00, 0x00],
                "{layout}"
            );
        }
    }
}
