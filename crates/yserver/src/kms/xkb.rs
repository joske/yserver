use xkbcommon::xkb::{Keycode, Keymap};

/// Clamp an xkbcommon keymap's keycode range into the X11 CARD8 `[8, 255]`
/// range used on the wire, guaranteeing `min <= max`.
pub(super) fn clamped_keycode_bounds(keymap: &Keymap) -> (u8, u8) {
    let min = u8::try_from(keymap.min_keycode().raw()).unwrap_or(8).max(8);
    let max = u8::try_from(keymap.max_keycode().raw().min(255))
        .unwrap_or(255)
        .max(min);
    (min, max)
}

/// Per-key data extracted from `xkbcommon::Keymap`, ready to lay
/// out into the `KeySymMap` wire structure xkb.xml defines.
struct KeyData {
    /// Width — the max level count across all published groups,
    /// capped at the wire `width` field's u8 range. xkb.xml uses a
    /// single width for every group in the entry. With
    /// `num_groups == 0` this is 0 and `nSyms == 0`, yielding the
    /// 8-byte fixed `KeySymMap` header.
    width: u8,
    /// Number of keyboard groups published for this key (0..=4);
    /// xkb.xml stores it in `groupInfo`'s low nibble.
    num_groups: u8,
    /// Per-group index into the `KeyTypes` table — 0 (`ONE_LEVEL`)
    /// when that group's level count is <= 1, 1 (`TWO_LEVEL`)
    /// otherwise. Groups beyond `num_groups` stay 0.
    kt_index: [u8; 4],
    /// `nSyms = width * num_groups` keysyms in group-major order:
    /// `[g0_l0, g0_l1, …, g1_l0, …]`. Levels beyond a group's own
    /// level count are filled with `NoSymbol` (0).
    syms: Vec<u32>,
    /// Real-modifier mask this key activates (0 when it is not a
    /// modifier key). Drives the synthesized `SetMods`/`LockMods`
    /// KeyAction so xkbcommon clients can update modifier state from
    /// key events — without it a client that starts with a modifier
    /// latched (e.g. kitty launched from a `super + Return` chord)
    /// can never clear it and every key resolves to NoSymbol (GH #59).
    mod_bit: u8,
    /// `true` for a lock modifier (Caps_Lock / Num_Lock) → `LockMods`;
    /// `false` for a plain modifier (Shift/Control/Super/…) → `SetMods`.
    mod_lock: bool,
}

/// Modifier-map entry for the `ModifierMap` section: keycode plus
/// the bitset of standard X11 modifier bits the key triggers
/// (Shift=0x01, Lock=0x02, Control=0x04, Mod1..Mod5=0x08..0x80).
struct ModMapEntry {
    keycode: u8,
    mods: u8,
}

/// The real-modifier mask (X11 KeyButMask bits 0..=7) a key actually
/// activates in THIS keymap, by pressing it in a scratch xkb state and
/// reading the effective mods. Correct across layouts/options (e.g. the
/// lv3 chooser binds ISO_Level3_Shift to Mod5, not the keysym-table guess).
fn real_mod_mask_for_keycode(keymap: &Keymap, kc: u32) -> u8 {
    let mut st = xkbcommon::xkb::State::new(keymap);
    st.update_key(
        xkbcommon::xkb::Keycode::new(kc),
        xkbcommon::xkb::KeyDirection::Down,
    );
    let mut mask = 0u8;
    for (name, bit) in [
        ("Shift", 0x01u8),
        ("Lock", 0x02),
        ("Control", 0x04),
        ("Mod1", 0x08),
        ("Mod2", 0x10),
        ("Mod3", 0x20),
        ("Mod4", 0x40),
        ("Mod5", 0x80),
    ] {
        if st.mod_name_is_active(name, xkbcommon::xkb::STATE_MODS_EFFECTIVE) {
            mask |= bit;
        }
    }
    mask
}

/// Map a level-0 keysym to the XKB *virtual* modifier name it
/// realises, or `None` for keys that are pure real modifiers
/// (Shift/Lock/Control) or non-modifiers. These names are the XKB
/// convention GDK/mutter match against (`gdkkeys-x11.c`'s
/// `update_keymaps` looks up "Super"/"Hyper"/"Meta"/"Alt"/…), so the
/// real-modifier *binding* for each is derived from the keymap (via
/// [`real_mod_mask_for_keycode`]) while only the name↔keysym pairing is
/// convention.
fn vmod_name_for_keysym(sym: u32) -> Option<&'static str> {
    match sym {
        0xFFE9 | 0xFFEA => Some("Alt"),   // Alt_L, Alt_R
        0xFFE7 | 0xFFE8 => Some("Meta"),  // Meta_L, Meta_R
        0xFFEB | 0xFFEC => Some("Super"), // Super_L, Super_R
        0xFFED | 0xFFEE => Some("Hyper"), // Hyper_L, Hyper_R
        0xFF7F => Some("NumLock"),        // Num_Lock
        0xFE03 => Some("LevelThree"),     // ISO_Level3_Shift
        0xFF14 => Some("ScrollLock"),     // Scroll_Lock
        _ => None,
    }
}

/// XKB caps virtual modifiers at 16 (`XkbNumVirtualMods`).
const XKB_NUM_VIRTUAL_MODS: usize = 16;

/// XKB caps keyboard groups at 4 (`XkbNumKbdGroups`). A `KeySymMap`
/// entry's `kt_index` is a fixed `[u8; 4]`, one type per group.
const XKB_NUM_KBD_GROUPS: u8 = 4;

const XKB_MAP_PART_KEY_TYPES: u16 = 1 << 0;
const XKB_MAP_PART_KEY_SYMS: u16 = 1 << 1;
const XKB_MAP_PART_MODIFIER_MAP: u16 = 1 << 2;
const XKB_MAP_PART_EXPLICIT_COMPONENTS: u16 = 1 << 3;
const XKB_MAP_PART_KEY_ACTIONS: u16 = 1 << 4;
const XKB_MAP_PART_VIRTUAL_MODS: u16 = 1 << 6;
const XKB_MAP_PART_VIRTUAL_MOD_MAP: u16 = 1 << 7;

/// Map components this backend can actually serialize into GetMap.
/// KeyBehaviors (bit 5) is intentionally absent.
const XKB_MAP_PARTS_EMITTED: u16 = XKB_MAP_PART_KEY_TYPES
    | XKB_MAP_PART_KEY_SYMS
    | XKB_MAP_PART_MODIFIER_MAP
    | XKB_MAP_PART_EXPLICIT_COMPONENTS
    | XKB_MAP_PART_KEY_ACTIONS
    | XKB_MAP_PART_VIRTUAL_MODS
    | XKB_MAP_PART_VIRTUAL_MOD_MAP;

/// Virtual-modifier description derived from the live keymap.
///
/// Mutter/GDK devirtualize keybinding modifiers (`<Super>`, `<Alt>`)
/// by reading the XKB virtual-modifier section: it matches a vmod by
/// *name* (`VirtualModNames`, GetNames) then resolves it to a real
/// modifier mask via `XkbVirtualModsToReal` over the `vmods[]`
/// bindings (GetMap). With both empty (yserver's prior behaviour),
/// `<Super>` resolves to 0 and `<Super>p` collapses to bare `p`.
pub(super) struct VirtualModData {
    /// Bitmask (16-bit) of which vmod indices are present.
    pub present_mask: u16,
    /// `bindings[i]` = real-modifier mask bound to vmod index `i`.
    pub bindings: [u8; XKB_NUM_VIRTUAL_MODS],
    /// `(vmod_index, name)` for each present vmod, in index order.
    pub names: Vec<(u8, &'static str)>,
    /// `VirtualModMap`: `(keycode, vmod_bits)` pairs.
    pub vmodmap: Vec<(u8, u16)>,
}

/// Build the virtual-modifier section from the live keymap. Vmod
/// indices are assigned in first-seen order over the keycode range;
/// the same assignment feeds GetMap (`vmods[]` + `VirtualModMap`) and
/// GetNames (`VirtualModNames`), so a client matching by name reads a
/// consistent real-modifier binding.
pub(super) fn virtual_mods_from_keymap(keymap: &Keymap) -> VirtualModData {
    let (min_kc, max_kc) = clamped_keycode_bounds(keymap);

    let mut present_mask: u16 = 0;
    let mut bindings = [0u8; XKB_NUM_VIRTUAL_MODS];
    let mut names: Vec<(u8, &'static str)> = Vec::new();
    let mut vmodmap: Vec<(u8, u16)> = Vec::new();
    // Name → assigned vmod index, so repeated keys (L/R) share an index.
    let mut index_for_name: Vec<(&'static str, u8)> = Vec::new();

    for kc_raw in min_kc..=max_kc {
        let kc = Keycode::new(u32::from(kc_raw));
        if keymap.num_layouts_for_key(kc) == 0 {
            continue;
        }
        let level_syms = keymap.key_get_syms_by_level(kc, 0, 0);
        let Some(sym) = level_syms.first().map(|s| s.raw()) else {
            continue;
        };
        let Some(name) = vmod_name_for_keysym(sym) else {
            continue;
        };
        // Real-modifier binding probed from THIS keymap (same source as
        // the modmap): the lv3 chooser binds ISO_Level3_Shift→Mod5, which
        // the keysym table got wrong (it guessed Mod1).
        let real_mod = real_mod_mask_for_keycode(keymap, u32::from(kc_raw));

        let idx = if let Some((_, idx)) = index_for_name.iter().find(|(n, _)| *n == name) {
            *idx
        } else {
            let idx = u8::try_from(index_for_name.len()).unwrap_or(0);
            if usize::from(idx) >= XKB_NUM_VIRTUAL_MODS {
                continue; // out of vmod slots; ignore extras
            }
            index_for_name.push((name, idx));
            names.push((idx, name));
            idx
        };
        present_mask |= 1 << idx;
        bindings[usize::from(idx)] |= real_mod;
        vmodmap.push((kc_raw, 1 << idx));
    }

    VirtualModData {
        present_mask,
        bindings,
        names,
        vmodmap,
    }
}

/// Build the core `GetModifierMapping` table from the live keymap.
///
/// Returns `(keycodes_per_modifier, data)` where `data` is
/// `8 * keycodes_per_modifier` bytes: the keycodes assigned to each
/// of Shift, Lock, Control, Mod1, Mod2, Mod3, Mod4, Mod5 in that
/// order, zero-padded per row. Derived by probing each keycode's
/// real-modifier mask in the live keymap ([`real_mod_mask_for_keycode`])
/// — the same source of truth as the XKB `GetMap` modifier-map — so the
/// core and XKB views of "which key is Super/Alt/…" never disagree.
pub(super) fn modifier_mapping_from_keymap(keymap: &Keymap) -> (u8, Vec<u8>) {
    // One row per standard X11 modifier bit, indexed by bit position
    // (Shift=0, Lock=1, Control=2, Mod1=3, …, Mod5=7).
    let mut rows: [Vec<u8>; 8] = Default::default();

    let (min_kc, max_kc) = clamped_keycode_bounds(keymap);

    for kc_raw in min_kc..=max_kc {
        let kc = Keycode::new(u32::from(kc_raw));
        if keymap.num_layouts_for_key(kc) == 0 {
            continue;
        }
        if keymap.key_get_syms_by_level(kc, 0, 0).is_empty() {
            continue;
        }
        // Real-modifier mask probed from the live keymap (same source as
        // the XKB GetMap modifier-map), so the core and XKB views never
        // disagree. A key may activate more than one real-mod bit; push
        // it into every matching row.
        let mask = real_mod_mask_for_keycode(keymap, u32::from(kc_raw));
        if mask == 0 {
            continue;
        }
        for (row, kcs) in rows.iter_mut().enumerate() {
            if mask & (1u8 << row) != 0 {
                kcs.push(kc_raw);
            }
        }
    }

    let kpm = rows.iter().map(Vec::len).max().unwrap_or(0).max(1);
    let mut data = Vec::with_capacity(8 * kpm);
    for row in &rows {
        for i in 0..kpm {
            data.push(row.get(i).copied().unwrap_or(0));
        }
    }
    (u8::try_from(kpm).unwrap_or(u8::MAX), data)
}

/// X11 modifier-map bitmask that picks `Shift` (real-mod bit 0). The
/// derived `TWO_LEVEL` key-type's map entry uses it to say "Shift
/// selects level 1"; referenced from the GetMap structural tests.
#[cfg(test)]
const SHIFT_MASK: u8 = 0x01;

/// XKB UseExtension reply (minor=0). Fixed 32 bytes.
/// Reports success and server protocol version 1.0.
pub(super) fn reply_use_extension() -> Vec<u8> {
    let mut r = vec![0u8; 32];
    r[0] = 1; // reply type
    r[1] = 1; // success
    // [2..4] sequence: rewritten by caller
    // [4..8] extra length in 4-byte units = 0
    r[8] = 1; // server-major
    r[9] = 0; // server-minor
    r
}

/// XKB GetControls reply (minor=6). Fixed 92 bytes
/// (`sz_xkbGetControlsReply`). Field offsets follow `xkbGetControlsReply`
/// in `/usr/include/X11/extensions/XKBproto.h`:
///   [0] type, [1] deviceID, [2..4] seq, [4..8] length,
///   [8] mkDfltBtn, [9] numGroups, [10] groupsWrap, [11] internalMods,
///   [12] ignoreLockMods, [13] internalRealMods,
///   [14] ignoreLockRealMods, [15] pad1,
///   [16..18] internalVMods, [18..20] ignoreLockVMods,
///   [20..22] repeatDelay, [22..24] repeatInterval,
///   [24..26] slowKeysDelay, [26..28] debounceDelay,
///   [28..30] mkDelay, [30..32] mkInterval,
///   [32..34] mkTimeToMax, [34..36] mkMaxSpeed,
///   [36..38] mkCurve, [38..40] axOptions,
///   [40..42] axTimeout, [42..44] axtOptsMask,
///   [44..46] axtOptsValues, [46..48] pad2,
///   [48..52] axtCtrlsMask, [52..56] axtCtrlsValues,
///   [56..60] enabledCtrls, [60..92] perKeyRepeat[32].
///
/// xkbcommon's `get_controls` checks
/// `reply->numGroups > 0 && reply->numGroups <= 4` (verified
/// against `objdump` of libxkbcommon-x11.so.0.13.1) — so the
/// previous reply with `numGroups=0` and `repeatDelay/interval/
/// enabledCtrls` written at the wrong offsets failed the keymap
/// build for any xkbcommon-using client.
pub(super) fn reply_get_controls(keymap: &Keymap) -> Vec<u8> {
    let mut r = vec![0u8; 92];
    r[0] = 1; // reply type
    r[1] = 1; // deviceID = 1
    // [4..8] extra length = (92-32)/4 = 15
    r[4..8].copy_from_slice(&15u32.to_le_bytes());
    // numGroups: xkbcommon requires 1..=4; clamp from keymap.num_layouts()
    // (may be 0 for an empty keymap).
    let num_groups: u8 = u8::try_from(keymap.num_layouts()).unwrap_or(1).clamp(1, 4);
    r[9] = num_groups;
    // Repeat delay = 500ms, interval = 33ms (≈30 Hz)
    r[20..22].copy_from_slice(&500_u16.to_le_bytes());
    r[22..24].copy_from_slice(&33_u16.to_le_bytes());
    // EnabledControls: RepeatKeys (bit 0) | PerKeyRepeat — pick
    // RepeatKeys so xkbcommon enables auto-repeat by default.
    r[56..60].copy_from_slice(&0x0000_0001_u32.to_le_bytes());
    r
}

fn get_map_requested_parts(body: &[u8]) -> u16 {
    // Request body after the core X11 4-byte header:
    // deviceSpec:CARD16, full:MASK, partial:MASK, ...
    if body.len() < 6 {
        return XKB_MAP_PARTS_EMITTED;
    }

    let full = u16::from_le_bytes([body[2], body[3]]);
    let partial = u16::from_le_bytes([body[4], body[5]]);
    (full | partial) & XKB_MAP_PARTS_EMITTED
}

pub(super) fn reply_get_map_for_request(keymap: &Keymap, body: &[u8]) -> Vec<u8> {
    reply_get_map_for_parts(keymap, get_map_requested_parts(body))
}

/// XKB GetMap reply (minor=8). Builds a wire-correct full reply from
/// `xkbcommon::Keymap` — real key types, per-key syms, and a
/// modifier map — so xkbcommon-x11 clients (wezterm via xkbcommon-rs,
/// every modern toolkit using libxkbcommon) get a usable keymap
/// rather than `NULL`.
///
/// What's published:
/// * Two `KeyTypes`: `ONE_LEVEL` (no modifiers, single level) and
///   `TWO_LEVEL` (Shift→level 1). Wider keys still slot into
///   `TWO_LEVEL`; xkbcommon then derives higher levels from the
///   `kt_index` + level data without us having to encode
///   `ALPHABETIC` / `KEYPAD`.
/// * Per-key `KeySymMap` entries covering every keyboard group
///   (`num_groups = num_layouts_for_key`, capped at 4), `width =
///   max level count across those groups`, and the keysyms pulled
///   straight from xkbcommon in group-major order. Keys with no
///   syms get the 8-byte header only (width=0, num_groups=0).
/// * `ModifierMap` populated by probing each key's real-modifier mask
///   in the live keymap (`real_mod_mask_for_keycode`) so Shift/Ctrl/Alt/
///   Lock/AltGr translate correctly on the client — option-agnostic
///   (e.g. ISO_Level3_Shift→Mod5 under lv3:ralt_switch, not Mod1).
/// * `KeyActions` advertises the full `[min, max]` range with
///   per-key counts of zero — xkbcommon's `get_actions` requires
///   exact range coverage, but accepts no actions per key.
/// * Other sections (`KeyBehaviors`, `VirtualMods`,
///   `ExplicitComponents`, `VirtualModMap`) stay empty —
///   xkbcommon's per-section validators tolerate that here.
///
/// Reply layout follows xkb.xml's `GetMap` switch order, which is
/// XML order, *not* bit-position order:
///   KeyTypes → KeySyms → KeyActions → KeyBehaviors → VirtualMods
///   → ExplicitComponents → ModifierMap → VirtualModMap.
pub(super) fn reply_get_map(keymap: &Keymap) -> Vec<u8> {
    reply_get_map_for_parts(keymap, XKB_MAP_PARTS_EMITTED)
}

fn reply_get_map_for_parts(keymap: &Keymap, requested_parts: u16) -> Vec<u8> {
    let present = requested_parts & XKB_MAP_PARTS_EMITTED;
    let include_key_types = present & XKB_MAP_PART_KEY_TYPES != 0;
    let include_key_syms = present & XKB_MAP_PART_KEY_SYMS != 0;
    let include_key_actions = present & XKB_MAP_PART_KEY_ACTIONS != 0;
    let include_virtual_mods = present & XKB_MAP_PART_VIRTUAL_MODS != 0;
    let include_explicit = present & XKB_MAP_PART_EXPLICIT_COMPONENTS != 0;
    let include_modifier_map = present & XKB_MAP_PART_MODIFIER_MAP != 0;
    let include_virtual_mod_map = present & XKB_MAP_PART_VIRTUAL_MOD_MAP != 0;

    // X11 keycodes are CARD8 — xkbcommon's keymap can carry a wider
    // range (min=9, max=709 for the default us layout). Clamp into
    // [8, 255] so wire counts fit in u8 and ensure min <= max.
    let (min_kc, max_kc) = clamped_keycode_bounds(keymap);
    let n_keys: u8 = max_kc - min_kc + 1;

    // Derive the real key-type table once. Its `types` (≥ 4: the seeded
    // ONE_LEVEL/TWO_LEVEL/ALPHABETIC/KEYPAD plus any derived higher-level
    // types — notably the AltGr FOUR_LEVEL letters) are serialised into
    // the KeyTypes section below, and each key's `ktIndex[group]` is
    // taken from `type_index_for`. This is what lets a client reach
    // level ≥ 2 (AltGr+e → €): the published type's map says
    // LevelThree(Mod5 0x80) → level 2.
    let key_types = key_types_from_keymap(keymap);

    // Walk every keyboard group for each keycode in the published
    // range and snapshot the data we'll serialise. A multi-group
    // keymap (e.g. `us,de`) exposes each group's keysyms in a single
    // group-major KeySymMap entry so clients can resolve the active
    // group's level-0 keysym (AltGr / layout switch).
    let mut keys: Vec<KeyData> = Vec::with_capacity(usize::from(n_keys));
    let mut modmap: Vec<ModMapEntry> = Vec::new();
    let mut total_syms: u32 = 0;
    for kc_raw in min_kc..=max_kc {
        let kc = Keycode::new(u32::from(kc_raw));
        // Clamp the group count to the XKB ceiling (4). A key with no
        // layouts yields num_groups == 0 and the 8-byte header only.
        let layouts = keymap.num_layouts_for_key(kc);
        let mut num_groups: u8 =
            u8::try_from(layouts.min(u32::from(XKB_NUM_KBD_GROUPS))).unwrap_or(XKB_NUM_KBD_GROUPS);

        // width = max level count across the published groups. xkb.xml
        // uses a single width for all groups in the entry; narrower
        // groups get NoSymbol-padded up to it below.
        let mut width: u8 = 0;
        let mut kt_index = [0u8; 4];
        for group in 0..num_groups {
            let levels = keymap.num_levels_for_key(kc, u32::from(group));
            // u8::MAX is more than enough — XKB caps each key at 8
            // levels in practice; clamp defensively.
            let g_width: u8 = u8::try_from(levels).unwrap_or(u8::MAX);
            width = width.max(g_width);
            // Per-group type index from the derived table (C1). This is
            // the real type whose modifier map lets the client reach the
            // AltGr layer (level ≥ 2); the old `g_width >= 2 ? 1 : 0`
            // heuristic could only ever point at TWO_LEVEL.
            kt_index[usize::from(group)] = key_types.type_index_for(kc_raw, group);
        }
        // A key with no levels at all carries no syms; publish the
        // 8-byte header (num_groups == 0), matching prior behaviour.
        if width == 0 {
            num_groups = 0;
            kt_index = [0u8; 4];
        }

        // Group-major sym table: [g0_l0, g0_l1, …, g1_l0, …]. Levels
        // beyond a group's own level count are NoSymbol (0).
        let mut syms: Vec<u32> = Vec::with_capacity(usize::from(width) * usize::from(num_groups));
        for group in 0..u32::from(num_groups) {
            for level in 0..u32::from(width) {
                let level_syms = keymap.key_get_syms_by_level(kc, group, level);
                syms.push(level_syms.first().map(|s| s.raw()).unwrap_or(0));
            }
        }
        let nsyms_this = u32::from(width) * u32::from(num_groups);
        total_syms = total_syms.saturating_add(nsyms_this);
        // Capture modmap entry from the real-modifier mask this key
        // actually activates in the live keymap (probed via a scratch
        // xkb state). A key is a modifier key iff that mask is non-zero.
        // This is option-agnostic and correct where the keysym table was
        // not (e.g. ISO_Level3_Shift→Mod5 under lv3:ralt_switch, not Mod1).
        let mut mod_bit = 0u8;
        let mut mod_lock = false;
        if num_groups != 0 && !syms.is_empty() {
            let bit = real_mod_mask_for_keycode(keymap, u32::from(kc_raw));
            if bit != 0 {
                modmap.push(ModMapEntry {
                    keycode: kc_raw,
                    mods: bit,
                });
                mod_bit = bit;
                // Caps_Lock (0xFFE5) / Num_Lock (0xFF7F) are LOCK
                // modifiers (LockMods); everything else is SetMods.
                mod_lock = matches!(syms.first().copied(), Some(0xFFE5 | 0xFF7F));
            }
        }
        keys.push(KeyData {
            width,
            num_groups,
            kt_index,
            syms,
            mod_bit,
            mod_lock,
        });
    }

    let total_modmap = u8::try_from(modmap.len()).unwrap_or(u8::MAX);

    // Virtual-modifier section, derived from the keymap.
    let vmod = virtual_mods_from_keymap(keymap);

    // -- Section sizes --------------------------------------------
    // KeyTypes: the derived table (`key_types.types`), serialised in
    // index order. Each `xkbKeyTypeWireDesc` is an 8-byte header plus
    // `nMapEntries` × 8-byte `xkbKTMapEntryWireDesc`. The table always
    // carries ≥ 4 types (the XKB required seed: ONE_LEVEL / TWO_LEVEL /
    // ALPHABETIC / KEYPAD) — Xlib's `XkbAllocClientMap` rejects
    // `nTypes < XkbNumRequiredTypes` (= 4) with BadValue — but now also
    // carries the derived higher-level types (FOUR_LEVEL AltGr letters,
    // KEYPAD/NumLock, …) so clients can resolve level ≥ 2.
    let n_types = u8::try_from(key_types.types.len()).unwrap_or(u8::MAX);
    let key_types_bytes: usize = key_types
        .types
        .iter()
        .map(|t| 8 + 8 * t.map_entries.len())
        .sum();
    let key_types_bytes = if include_key_types {
        key_types_bytes
    } else {
        0
    };

    // KeySyms: 8-byte header + nSyms * 4 per key.
    let key_syms_bytes: usize = keys
        .iter()
        .map(|k| 8 + 4 * usize::from(k.width) * usize::from(k.num_groups))
        .sum();
    let key_syms_bytes = if include_key_syms { key_syms_bytes } else { 0 };

    // KeyActions: nKeyActions CARD8 counts + pad to 4-byte align +
    // `totalActs` × 8-byte action structs. A modifier key carries one
    // action per sym slot (SetMods/LockMods); every other key carries
    // none. Emitting these lets xkbcommon track modifier state from key
    // events (GH #59) — previously totalActs was 0 and a client that
    // started with a modifier latched could never clear it.
    let nk = usize::from(n_keys);
    let actions_count_pad = (4 - nk % 4) % 4;
    let total_acts: u32 = keys
        .iter()
        .filter(|k| k.mod_bit != 0)
        .map(|k| u32::from(k.width) * u32::from(k.num_groups))
        .sum();
    let key_actions_bytes: usize = nk + actions_count_pad + (total_acts as usize) * 8;
    let key_actions_bytes = if include_key_actions {
        key_actions_bytes
    } else {
        0
    };

    // ModifierMap: 2 bytes per entry + pad to 4-byte align.
    let modmap_raw_bytes: usize = usize::from(total_modmap) * 2;
    let modmap_pad = (4 - modmap_raw_bytes % 4) % 4;
    let modmap_bytes: usize = modmap_raw_bytes + modmap_pad;
    let modmap_bytes = if include_modifier_map {
        modmap_bytes
    } else {
        0
    };

    // VirtualMods: one CARD8 binding per present vmod, padded to a
    // 4-byte boundary (Xorg `XkbPaddedSize`).
    let vmod_count: usize = vmod.present_mask.count_ones() as usize;
    let vmod_pad = (4 - vmod_count % 4) % 4;
    let vmod_bytes: usize = vmod_count + vmod_pad;
    let vmod_bytes = if include_virtual_mods { vmod_bytes } else { 0 };
    // VirtualModMap: `xkbVModMapWireDesc` { key(1) pad(1) vmods(2) } per
    // entry — already 4-byte sized, no extra pad.
    let vmodmap_bytes: usize = vmod.vmodmap.len() * 4;
    let vmodmap_bytes = if include_virtual_mod_map {
        vmodmap_bytes
    } else {
        0
    };
    // ExplicitComponents stays empty (0 bytes).

    let extra = key_types_bytes
        + key_syms_bytes
        + key_actions_bytes
        + vmod_bytes
        + modmap_bytes
        + vmodmap_bytes;
    let total = 40 + extra;
    let length_words = u32::try_from((total - 32) / 4).unwrap_or(u32::MAX);

    // -- Fixed 40-byte reply header ------------------------------
    let mut r = vec![0u8; total];
    r[0] = 1; // reply type
    r[1] = 1; // deviceID = 1
    r[4..8].copy_from_slice(&length_words.to_le_bytes());
    r[10] = min_kc;
    r[11] = max_kc;
    r[12..14].copy_from_slice(&present.to_le_bytes());
    if include_key_types {
        // [14] firstType=0
        r[15] = n_types; // nTypes — derived table size (≥ 4)
        r[16] = n_types; // totalTypes
    }
    if include_key_syms {
        r[17] = min_kc; // firstKeySym
        r[18..20].copy_from_slice(&u16::try_from(total_syms).unwrap_or(u16::MAX).to_le_bytes());
        r[20] = n_keys; // nKeySyms — covers full range
    }
    if include_key_actions {
        r[21] = min_kc; // firstKeyAction
        r[22..24].copy_from_slice(&u16::try_from(total_acts).unwrap_or(u16::MAX).to_le_bytes());
        r[24] = n_keys; // nKeyActions — covers full range
    }
    // KeyBehaviors are not implemented and bit 5 is never set in `present`.
    if include_explicit {
        r[28] = min_kc; // firstKeyExplicit; empty section
    }
    if include_modifier_map {
        r[31] = min_kc; // firstModMapKey
        r[32] = n_keys; // nModMapKeys (range; totalModMapKeys is actual list length)
        r[33] = total_modmap;
    }
    if include_virtual_mod_map {
        r[34] = min_kc; // firstVModMapKey
        r[35] = n_keys; // nVModMapKeys — covers full range
        r[36] = u8::try_from(vmod.vmodmap.len()).unwrap_or(u8::MAX); // totalVModMapKeys
    }
    if include_virtual_mods {
        r[38..40].copy_from_slice(&vmod.present_mask.to_le_bytes()); // virtualMods
    }

    // -- Section bodies ------------------------------------------
    let mut off = 40;

    // KeyTypes: serialise the derived table in index order. We use
    // REAL modifiers only (no virtual mods in our types): the type
    // header `mask`/`realMods` is the OR of all entry real-mod masks,
    // `vmods=0`. Each `xkbKTMapEntryWireDesc` carries
    // { active=1, mask=real_mods, level, realMods=real_mods, vmods=0,
    // pad=0 }. The client matches the key event's effective real-mod
    // state against entry `mask` to pick the level (e.g. 0x80 matches
    // the `{mask:0x80 → 2}` entry → € at level 2). ONE_LEVEL is an
    // 8-byte header with no entries.
    if include_key_types {
        for t in &key_types.types {
            let or_mods: u8 = t.map_entries.iter().fold(0u8, |acc, e| acc | e.real_mods);
            r[off] = or_mods; // mods_mask
            r[off + 1] = or_mods; // mods_mods (realMods)
            // [off+2..off+4] mods_vmods = 0
            r[off + 4] = t.num_levels; // numLevels
            r[off + 5] = u8::try_from(t.map_entries.len()).unwrap_or(u8::MAX); // nMapEntries
            // [off+6] hasPreserve=0, [off+7] pad
            let mut eoff = off + 8;
            for e in &t.map_entries {
                r[eoff] = 1; // active = true
                r[eoff + 1] = e.real_mods; // mask
                r[eoff + 2] = e.level; // level
                r[eoff + 3] = e.real_mods; // realMods
                // [eoff+4..+6] virtualMods=0, [eoff+6..+8] pad
                eoff += 8;
            }
            off = eoff;
        }
    }

    // KeySyms: per-key KeySymMap.
    if include_key_syms {
        for k in &keys {
            // [off..off+4] kt_index[4] — one KeyTypes index per group;
            // groups beyond num_groups stay 0 (ONE_LEVEL), within nTypes.
            r[off..off + 4].copy_from_slice(&k.kt_index);
            // [off+4] groupInfo: low 4 bits = num_groups
            r[off + 4] = k.num_groups & 0x0F;
            r[off + 5] = k.width;
            let nsyms = u16::try_from(k.syms.len()).unwrap_or(u16::MAX);
            r[off + 6..off + 8].copy_from_slice(&nsyms.to_le_bytes());
            let mut sym_off = off + 8;
            for sym in &k.syms {
                r[sym_off..sym_off + 4].copy_from_slice(&sym.to_le_bytes());
                sym_off += 4;
            }
            off = sym_off;
        }
    }

    // KeyActions: per-key action count (modifier keys = nSyms, else 0),
    // padded to a 4-byte boundary, then the action structs in key order.
    if include_key_actions {
        for k in &keys {
            r[off] = if k.mod_bit != 0 {
                u8::try_from(usize::from(k.width) * usize::from(k.num_groups)).unwrap_or(u8::MAX)
            } else {
                0
            };
            off += 1;
        }
        off += actions_count_pad;
        // One XkbModAction per sym slot of each modifier key: SetMods
        // (type 1) for plain modifiers, LockMods (type 3) for Caps/Num lock.
        // mask = realMods = the key's real-mod bit; flags = 0; vmods = 0.
        // (8-byte action: type, flags, mask, realMods, vmods1, vmods2, pad,
        // pad.) This is what tells xkbcommon "this key sets/clears Mod4".
        for k in &keys {
            if k.mod_bit == 0 {
                continue;
            }
            let act_type: u8 = if k.mod_lock { 3 } else { 1 };
            let n_acts = usize::from(k.width) * usize::from(k.num_groups);
            for _ in 0..n_acts {
                r[off] = act_type; // type
                // [off + 1] flags = 0
                r[off + 2] = k.mod_bit; // mask
                r[off + 3] = k.mod_bit; // realMods
                // [off + 4 ..= off + 7] vmods (2) + pad (2) = 0
                off += 8;
            }
        }
    }

    // VirtualMods: one CARD8 real-mod binding per present vmod, in
    // ascending bit order, then pad to a 4-byte boundary. Matches
    // Xorg `XkbSendMap` (xkb.c:1430-1438).
    if include_virtual_mods {
        for i in 0..XKB_NUM_VIRTUAL_MODS {
            if vmod.present_mask & (1 << i) != 0 {
                r[off] = vmod.bindings[i];
                off += 1;
            }
        }
        off += vmod_pad;
    }
    // ExplicitComponents empty (0 entries, 0 pad).

    // ModifierMap: 2 bytes per entry, then pad.
    if include_modifier_map {
        for entry in &modmap {
            r[off] = entry.keycode;
            r[off + 1] = entry.mods;
            off += 2;
        }
        off += modmap_pad;
    }

    // VirtualModMap: `xkbVModMapWireDesc` { key, pad, vmods(CARD16) }
    // per entry. Combined with the ModifierMap, lets clients resolve
    // each vmod to its real modifier (`XkbVirtualModsToReal`).
    if include_virtual_mod_map {
        for (key, vmods) in &vmod.vmodmap {
            r[off] = *key;
            // r[off + 1] pad = 0
            r[off + 2..off + 4].copy_from_slice(&vmods.to_le_bytes());
            off += 4;
        }
    }

    debug_assert_eq!(off, total, "GetMap reply body length matches total");
    r
}

/// XKB GetNames reply (minor=17). The `which` mask advertises
/// `KeyTypeNames|KTLevelNames|KeyNames|VirtualModNames`
/// (bits 6,7,9,11 = 0x40|0x80|0x200|0x800 = `0xAC0`), the bitset
/// xkbcommon's `get_names_required` validates. (Verified against
/// `objdump` of `libxkbcommon-x11.so.0.13.1`'s `get_names`: the
/// AND-mask in the `FAIL_UNLESS` is `0xac0`.)
///
/// The reply must agree with `reply_get_map` on `[min_kc, max_kc]`
/// and the type count: xkbcommon's `get_key_names` asserts
/// `firstKey == min_key_code`, `firstKey + nKeys - 1 == max_key_code`,
/// and `reply->{min,max}KeyCode == keymap->{min,max}_key_code`;
/// `get_type_names` asserts `reply->nTypes == keymap->num_types`.
///
/// All name slots carry REAL interned atoms (component names from
/// the compiled RMLVO, canonical type/level names, per-key names
/// from `xkb_keymap_key_get_name`). Plain libX11 clients
/// (xdotool, e16) `XGetAtomName` every atom in this reply — the
/// previous zero-atom stub made them exit on BadAtom. The `nTypes`,
/// `nLevelsPerType` list, and `nKTLevels` are all sourced from the
/// SAME `key_types_from_keymap` derivation `reply_get_map` uses, so
/// the two replies agree by construction.
///
/// Canonical type name by table index / shape ([`type_name_for`]):
/// seeded indices 0..=3 are ONE_LEVEL / TWO_LEVEL / ALPHABETIC /
/// KEYPAD; derived types take ONE_LEVEL / TWO_LEVEL / FOUR_LEVEL by
/// `num_levels`, falling back to a synthesized `type<N>`. Level
/// names by index ([`level_name_for`]): Base / Shift / Alt Base /
/// Shift Alt, then `Level<n+1>`.
fn type_name_for(index: usize, t: &KeyTypeDesc) -> String {
    // Seeded indices 0..=3 carry the canonical XKB required-type names
    // (matches the C1 seed order in `key_types_from_keymap`).
    match index {
        0 => return "ONE_LEVEL".to_owned(),
        1 => return "TWO_LEVEL".to_owned(),
        2 => return "ALPHABETIC".to_owned(),
        3 => return "KEYPAD".to_owned(),
        _ => {}
    }
    // Derived types: name by level count; unknown shapes fall back to a
    // synthesized non-empty name so every slot interns a real atom.
    match t.num_levels {
        1 => "ONE_LEVEL".to_owned(),
        2 => "TWO_LEVEL".to_owned(),
        4 => "FOUR_LEVEL".to_owned(),
        _ => format!("type{index}"),
    }
}

/// Canonical XKB shift-level name for a 0-based level index.
fn level_name_for(level: u8) -> String {
    match level {
        0 => "Base".to_owned(),
        1 => "Shift".to_owned(),
        2 => "Alt Base".to_owned(),
        3 => "Shift Alt".to_owned(),
        n => format!("Level{}", u16::from(n) + 1),
    }
}

pub(super) fn reply_get_names(
    keymap: &Keymap,
    rmlvo: &crate::kms::core::XkbRmlvo,
    intern_atom: &mut dyn FnMut(&str) -> u32,
) -> Vec<u8> {
    let (min_kc, max_kc) = clamped_keycode_bounds(keymap);
    let n_keys: u8 = max_kc - min_kc + 1;

    // Virtual modifiers (same derivation as GetMap). VirtualModNames
    // must carry one atom per present vmod, in ascending bit order, so
    // a client can match "Super"/"Alt"/… to the binding GetMap sent.
    let vmod = virtual_mods_from_keymap(keymap);
    let vmod_count: usize = vmod.present_mask.count_ones() as usize;

    // -- which mask -----------------------------------------------
    // KeyTypeNames|KTLevelNames|VirtualModNames|KeyNames is what
    // xkbcommon's `get_names_required` enforces (0xAC0). But
    // `get_names()` in libxkbcommon also unconditionally reads
    // `list.keycodesName`, `list.symbolsName`, `list.typesName`,
    // and `list.compatName` from a *stack-uninitialized*
    // `xcb_xkb_get_names_value_list_t list;` (keymap.c:1139-1146).
    // xcb-generated `value_list_unpack` only writes fields whose
    // bit is set in `which`; an absent bit leaves stack garbage
    // there, which xkbcommon then dispatches as GetAtomName
    // requests. We saw the resulting bogus atoms (0xAE4BAA70,
    // 22057, …) in the wire log. Set Keycodes|Symbols|Types|Compat
    // (0x35) on top of 0xAC0 so xcb actually writes zeros into
    // those fields.
    const REQUIRED: u32 = 0x0000_0AC0; // KeyTypeNames|KTLevelNames|VirtualModNames|KeyNames
    const UNCONDITIONALLY_READ: u32 = 0x0000_0035; // Keycodes|Symbols|Types|Compat
    let which: u32 = REQUIRED | UNCONDITIONALLY_READ;

    // Derive the SAME key-type table reply_get_map serialises (C1).
    // `key_types_from_keymap` is deterministic, so the table here is
    // byte-for-byte the one GetMap published: identical `nTypes` and
    // identical per-type `num_levels` ordering. That is the load-bearing
    // GetMap/GetNames consistency invariant — xkbcommon-x11 reads both
    // replies and rejects a keymap whose nTypes / level counts disagree.
    let key_types = key_types_from_keymap(keymap);
    let n_types: usize = key_types.types.len();
    // nKTLevels = Σ over the derived types of `num_levels` — the total
    // level-name ATOM count.
    let kt_level_names_count: usize = key_types
        .types
        .iter()
        .map(|t| usize::from(t.num_levels))
        .sum();

    // -- Section sizes (in xkb.xml switch order, which is bit
    // order — Keycodes(0)→Geometry(1)→Symbols(2)→PhysSymbols(3)→
    // Types(4)→Compat(5)→KeyTypeNames(6)→KTLevelNames(7)→
    // IndicatorNames(8)→VirtualModNames(9)→GroupNames(10)→
    // KeyNames(11)→KeyAliases(12)→RGNames(13)). For us:
    //
    // * keycodesName   ATOM = 4 bytes  (Keycodes)
    // * symbolsName    ATOM = 4 bytes  (Symbols)
    // * typesName      ATOM = 4 bytes  (Types)
    // * compatName     ATOM = 4 bytes  (Compat)
    // * typeNames[nTypes] ATOM = nTypes*4 bytes (KeyTypeNames) — must
    //                                  match GetMap's nTypes
    // * nLevelsPerType[nTypes] + pad-to-4 + ktLevelNames[ΣnumLevels]*4
    //                                  bytes (KTLevelNames)
    // * virtualModNames: one ATOM per present vmod (VirtualModNames)
    // * keyNames[nKeys] KeyName(4) = nKeys * 4 bytes (KeyNames)
    let unconditional_names_bytes = 4 * 4;
    let key_type_names_bytes = n_types * 4;
    let kt_levels_count = n_types;
    let kt_levels_count_pad = (4 - kt_levels_count % 4) % 4;
    let kt_level_names_bytes = kt_levels_count + kt_levels_count_pad + kt_level_names_count * 4;
    let nk = usize::from(n_keys);
    let key_names_bytes = nk * 4;
    // VirtualModNames: one ATOM per present vmod (4 bytes each).
    let vmod_names_bytes = vmod_count * 4;
    let extra = unconditional_names_bytes
        + key_type_names_bytes
        + kt_level_names_bytes
        + vmod_names_bytes
        + key_names_bytes;
    let total = 32 + extra;

    let mut r = vec![0u8; total];
    r[0] = 1; // reply type
    r[1] = 1; // deviceID
    let length_words = u32::try_from(extra / 4).unwrap_or(u32::MAX);
    r[4..8].copy_from_slice(&length_words.to_le_bytes());
    r[8..12].copy_from_slice(&which.to_le_bytes());
    r[12] = min_kc;
    r[13] = max_kc;
    r[14] = u8::try_from(n_types).unwrap_or(u8::MAX); // nTypes — matches GetMap
    // [15] groupNames = 0
    r[16..18].copy_from_slice(&vmod.present_mask.to_le_bytes()); // virtualMods
    r[18] = min_kc; // firstKey
    r[19] = n_keys; // nKeys — full range
    // [20..24] indicators = 0
    // [24] nRadioGroups = 0
    // [25] nKeyAliases = 0
    // [26..28] nKTLevels = Σ num_levels over the derived types.
    r[26..28].copy_from_slice(
        &u16::try_from(kt_level_names_count)
            .unwrap_or(u16::MAX)
            .to_le_bytes(),
    );
    // [28..32] pad

    // -- Body ----------------------------------------------------
    // Every ATOM slot carries a REAL interned atom. The previous
    // zero-atom stub was tuned for xkbcommon (whose
    // `get_escaped_atom_name` short-circuits atom == 0) but plain
    // libX11 clients (xdotool, e16) XGetAtomName every name in the
    // reply — atom 0 → BadAtom → the default error handler exits
    // the client (the e16-in-vng blocker).
    let mut off = 32;
    // keycodesName + symbolsName + typesName + compatName, in bit
    // order (Keycodes=0, Symbols=2, Types=4, Compat=5).
    //
    // `symbolsName` is derived from the active RMLVO so it reflects
    // the layout the server actually compiled. This is a best-effort
    // approximation of the KcCGST `symbols` string — NOT what
    // `xkbcomp` would resolve: it omits `options` and uses a
    // simplified per-group join for a multi-layout RMLVO (`us,ru`).
    // The actual keysyms reach clients via GetMap; symbolsName is
    // informational metadata only. keycodesName/typesName/compatName
    // stay at the canonical values for the evdev rules the server uses.
    //
    // Per-layout segment: "<layout>" or "<layout>(<variant>)".
    let layouts: Vec<&str> = rmlvo.layout.split(',').collect();
    let variants: Vec<&str> = rmlvo.variant.split(',').collect();
    let mut segs = Vec::with_capacity(layouts.len());
    for (i, l) in layouts.iter().enumerate() {
        match variants.get(i) {
            Some(v) if !v.is_empty() => segs.push(format!("{l}({v})")),
            _ => segs.push((*l).to_string()),
        }
    }
    // Approximate KcCGST symbols string; informational only.
    let symbols_name = format!("pc+{}+inet(evdev)", segs.join("+"));
    let names: [&str; 4] = [
        "evdev+aliases(qwerty)", // keycodesName
        &symbols_name,           // symbolsName — derived from active RMLVO
        "complete",              // typesName
        "complete",              // compatName
    ];
    for name in names {
        let atom = intern_atom(name);
        r[off..off + 4].copy_from_slice(&atom.to_le_bytes());
        off += 4;
    }
    // typeNames: one ATOM per derived type, in the SAME index order
    // GetMap serialised. Every slot interns a REAL non-zero atom —
    // plain libX11 clients XGetAtomName every name and atom 0 → BadAtom
    // → client exit. Canonical name by index/shape (see
    // `type_name_for`); unknown shapes fall back to a synthesized
    // "type<N>" so no slot is ever empty.
    for (i, t) in key_types.types.iter().enumerate() {
        let name = type_name_for(i, t);
        let atom = intern_atom(&name);
        r[off..off + 4].copy_from_slice(&atom.to_le_bytes());
        off += 4;
    }
    // KTLevelNames: nLevelsPerType[nTypes] = each type's `num_levels`,
    // padded to a 4-byte boundary, then Σ num_levels level-name ATOMs
    // emitted type-by-type (the canonical XKB shift-level names).
    for (i, t) in key_types.types.iter().enumerate() {
        r[off + i] = t.num_levels;
    }
    off += kt_levels_count + kt_levels_count_pad;
    for t in &key_types.types {
        for level in 0..t.num_levels {
            let name = level_name_for(level);
            let atom = intern_atom(&name);
            r[off..off + 4].copy_from_slice(&atom.to_le_bytes());
            off += 4;
        }
    }
    // VirtualModNames: one ATOM per present vmod, ascending bit order.
    // `vmod.names` is already (index, name) in ascending index order.
    for (_idx, name) in &vmod.names {
        let atom = intern_atom(name);
        r[off..off + 4].copy_from_slice(&atom.to_le_bytes());
        off += 4;
    }
    // KeyNames: char[4] per key (NOT atoms) — the keymap's canonical
    // key names ("ESC", "AE01", …) zero-padded/truncated to 4 bytes.
    // Keys the keymap doesn't name stay all-zero (anonymous).
    for i in 0..nk {
        let kc = u32::from(min_kc) + u32::try_from(i).unwrap_or(u32::MAX);
        if let Some(name) = keymap.key_get_name(Keycode::new(kc)) {
            for (j, b) in name.bytes().take(4).enumerate() {
                r[off + i * 4 + j] = b;
            }
        }
    }
    off += key_names_bytes;
    debug_assert_eq!(off, total, "GetNames reply body length matches total");
    r
}

/// XKB GetCompatMap reply (minor=10). 32-byte `sz_xkbGetCompatMapReply`
/// header + the per-group `xkbModsWireDesc` compat records.
///
/// MUST NOT be empty: libX11's `_XkbReadGetCompatMapReply` (XKBCompat.c)
/// unconditionally calls `_XkbInitReadBuffer(dpy, &buf, rep->length * 4)`
/// with no `if (rep->length)` guard (unlike the Map/Indicator/Names/Geometry
/// readers), and `_XkbInitReadBuffer` returns FALSE for `size <= 0`
/// (XKBRdBuf.c:40) → the reader returns `BadAlloc` → `XkbGetKeyboardByName`
/// BAILOUTs to NULL → every libX11 client (e.g. `setxkbmap`) fails with
/// "Error loading new keyboard description". Confirmed by gdb on `setxkbmap`.
/// So we always emit a non-empty body: the group-compat block.
///
/// `groupsRtrn=0x0f` (all four keyboard groups, as Xorg reports) + four
/// `xkbModsWireDesc` (mask, realMods, vmods:u16) records. The values are the
/// `complete`-ruleset constant (golden-vector-verified against the captured
/// Xorg reply, see docs/superpowers/findings/2026-06-25-altgr-4level-golden-vector.md
/// and the request-coverage audit): group 1 → none, groups 2-4 → Mod5 (0x80).
/// `nSIRtrn=0`: the sym-interpretation list stays deferred — it's informational
/// (libX11 accepts `nSI=0`, skipping the SI loop; clients re-derive interps),
/// and a faithful 124-entry encoder is tracked separately.
pub(super) fn reply_get_compat_map() -> Vec<u8> {
    // group-compat: 4 × xkbModsWireDesc (4 bytes each) = 16-byte body.
    const GROUP_COMPAT: [u8; 16] = [
        0x00, 0x00, 0x00, 0x00, // group 1: mask=0 realMods=0 vmods=0
        0x80, 0x80, 0x00, 0x00, // group 2: mask=Mod5 realMods=Mod5 vmods=0
        0x80, 0x80, 0x00, 0x00, // group 3
        0x80, 0x80, 0x00, 0x00, // group 4
    ];
    let mut r = vec![0u8; 32 + GROUP_COMPAT.len()];
    r[0] = 1; // reply type
    r[1] = 1; // deviceID = 1
    // [4..8] length = body words = 16 / 4 = 4 (MUST be > 0; see doc-comment).
    r[4..8].copy_from_slice(&4u32.to_le_bytes());
    r[8] = 0x0f; // groupsRtrn = all four keyboard groups
    // [9] pad1
    // [10..12] firstSIRtrn = 0
    // [12..14] nSIRtrn = 0 (sym-interpret list deferred)
    // [14..16] nTotalSI = 0
    // [16..32] pad2[16] = 0
    r[32..32 + GROUP_COMPAT.len()].copy_from_slice(&GROUP_COMPAT);
    r
}

/// XKB GetDeviceInfo reply (minor=24). The wire-correct *empty*
/// reply is 36 bytes, not 32 — `sizeof(xcb_xkb_get_device_info_reply_t)`
/// (verified via gcc on `xcb/xkb.h`) is 36 because the C struct
/// places `nameLen: CARD16` at offset 32 with 2 bytes of trailing
/// pad. xcb-based clients (xkbcommon-x11 inside `vkgears`,
/// `wezterm`, …) cast the libxcb reply pointer straight to that
/// struct and access `reply->nameLen` plus
/// `xcb_xkb_get_device_info_name(reply) = (char*)(reply + 1)`
/// — both **read past a 32-byte allocation**, producing garbage
/// atoms that the client then fans out as GetAtomName requests
/// (we saw 0xAE4BAA70, 0xAE4B5808, 22057 in the log). xkbcommon-x11
/// then errors out and returns NULL, so `vkgears` segfaults on the
/// resulting `xkb_keymap_ref(NULL)`.
///
/// We publish an empty keyboard: no LED feedbacks, no buttons, no
/// name string, no actions. That's still a 36-byte body — fixed
/// header + `nameLen=0` (2B) + pad-to-4 (2B) — with `length = 1`.
pub(super) fn reply_get_device_info() -> Vec<u8> {
    let mut r = vec![0u8; 36];
    r[0] = 1; // reply
    r[1] = 1; // deviceID = 1
    // [4..8] extra length = (36 - 32) / 4 = 1
    r[4..8].copy_from_slice(&1u32.to_le_bytes());
    // [8..10] present, [10..12] supported, [12..14] unsupported = 0
    // [14..16] nDeviceLedFBs = 0
    // [16] firstBtnWanted, [17] nBtnsWanted
    // [18] firstBtnRtrn, [19] nBtnsRtrn
    // [20..22] totalBtns = 0
    // [22] hasOwnState
    // [23] (padding/alignment)
    // [24..26] dfltKbdFB, [26..28] dfltLedFB
    // [28..32] devType atom = 0
    // [32..34] nameLen = 0
    // [34..36] pad align(4)
    r
}

/// XKB PerClientFlags reply (minor=21). Fixed 32 bytes.
/// Mirrors Xorg's reply shape: advertise the standard per-client flag
/// mask and report the requested value for changed bits. This keeps
/// clients that enable detectable auto-repeat from seeing an all-zero
/// capability/value pair.
pub(super) fn reply_per_client_flags(body: &[u8]) -> Vec<u8> {
    const XKB_PCF_ALL_FLAGS_MASK: u32 = 0x1f;

    let mut r = vec![0u8; 32];
    r[0] = 1; // reply type
    r[1] = 1; // deviceID
    // [2..4] sequence: rewritten by caller
    // [4..8] extra length in 4-byte units = 0
    r[8..12].copy_from_slice(&XKB_PCF_ALL_FLAGS_MASK.to_le_bytes());

    if body.len() >= 12 {
        let change = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
        let value = u32::from_le_bytes([body[8], body[9], body[10], body[11]]);
        let effective = value & change & XKB_PCF_ALL_FLAGS_MASK;
        r[12..16].copy_from_slice(&effective.to_le_bytes());
    }

    r
}

/// Minimal all-zero 32-byte reply for XKB minors that clients tolerate silently.
/// Only use for minors with no required reply content (e.g. SetControls has none).
pub(super) fn reply_minimal(minor: u8) -> Vec<u8> {
    log::debug!("xkb: unimplemented minor {minor}, returning minimal reply");
    let mut r = vec![0u8; 32];
    r[0] = 1; // reply type
    r[1] = 1; // deviceID — must match the value returned by reply_get_map
    // and reply_get_controls etc.; xkbcommon-x11 cross-validates the
    // deviceID across replies and tears down the keymap when it
    // doesn't agree. GTK3's startup path probes minors 4 (GetState)
    // and 21 (PerClientFlags) through here.
    r
}

/// Per-group layout list extracted from an XKB `symbols` KcCGST
/// component string by [`parse_symbols_layouts`].
///
/// Both fields are comma-joined, one entry per keyboard group, in
/// group-slot order (group 1 first). They feed straight into
/// xkbcommon `new_from_names(layout=…, variant=…)` (Task 1b-2's
/// `recompile_keymap`), so the shapes match RMLVO's `layout`/`variant`
/// convention: `layouts = "us,de,us"`, `variants = ",,"`.
// Consumed by the XkbGetKbdByName layout-switch path
// (`KmsBackendV2::load_keymap_by_components`).
pub(super) struct SymbolsLayouts {
    /// Comma-joined layout codes in group order (e.g. `"us,de,us"`).
    pub layouts: String,
    /// Comma-joined variants, one slot per layout (e.g. `",polytonic"`);
    /// empty string for a group with no variant.
    pub variants: String,
    /// Comma-joined RMLVO option group:variant entries extracted from the
    /// behaviour partials in the symbols string (e.g.
    /// `"caps:none,lv3:ralt_switch"`). Empty string if none. These map
    /// straight onto xkbcommon `new_from_names(options=…)`. The
    /// `level3(ralt_switch)` chooser is the load-bearing one: it binds
    /// RAlt→ISO_Level3_Shift→Mod5, which is what makes AltGr (level 2/3)
    /// reachable. Dropping it leaves RAlt as Mod1 and breaks `€`/Belgian.
    pub options: String,
}

/// Maps a behaviour-partial bare name (the head of a `name(variant)`
/// symbols segment) to its RMLVO option *group* prefix, or `None` if the
/// name is not a recognised option partial. A recognised partial with a
/// variant becomes `group:variant` (e.g. `level3(ralt_switch)` →
/// `lv3:ralt_switch`, `capslock(none)` → `caps:none`).
///
/// NB: `pc*` models and the `inet*` family are NOT options — they carry
/// no chooser and are skipped entirely (see [`is_extra`]). This table is
/// the option subset of `SYMBOLS_EXTRAS`.
fn option_group_for(name: &str) -> Option<&'static str> {
    match name {
        "level3" | "lv3" => Some("lv3"),
        "level5" | "lv5" => Some("lv5"),
        "capslock" | "caps" => Some("caps"),
        "group" | "grp" => Some("grp"),
        "compose" => Some("compose"),
        "ctrl" => Some("ctrl"),
        "eurosign" => Some("eurosign"),
        "nbsp" => Some("nbsp"),
        "kpdl" => Some("kpdl"),
        "keypad" => Some("keypad"),
        _ => None,
    }
}

/// Non-layout tokens an XKB `symbols` string carries alongside the
/// real layouts — keyboard model (`pc`/`pc104`/…) and the various
/// behaviour partials (`inet(evdev)`, `group(…)`, `compose(…)`, …).
/// Matched against the bare token before any `(`/`:` suffix; the
/// `inet`/`group`/… entries are prefix-matched (see [`is_extra`]).
const SYMBOLS_EXTRAS: &[&str] = &[
    "pc",
    "pc104",
    "pc105",
    "pc101",
    "pc102",
    "inet",
    "group",
    "grp",
    "compose",
    // NB: no `"lv"` — `lv` is the real Latvian layout
    // (/usr/share/X11/xkb/symbols/lv, in evdev rules), not a level
    // partial. The level partials are `level2`/`level3`/`level5`, all
    // covered by the `"level"` prefix entry above; a `"lv"` prefix
    // would silently swallow Latvian and break fail-closed.
    "level",
    "terminate",
    "capslock",
    "ctrl",
    "keypad",
    "kpdl",
    "eurosign",
    "srvr_ctrl",
    "nbsp",
];

/// True if `token` is a recognised non-layout extra (model / behaviour
/// partial) that should be skipped, not treated as a layout. The
/// `inet`/`group`/`grp`/`compose`/`level` families are matched by
/// prefix because they appear as `inet(evdev)`, `group(alts_toggle)`,
/// … and the parenthesised part is already stripped before this call,
/// but the bare head (`inet`) is what we compare.
fn is_extra(token: &str) -> bool {
    SYMBOLS_EXTRAS.iter().any(|&e| {
        // Prefix-matched families vs. exact model strings. `inet`,
        // `group`, `grp`, `compose`, `level` are the partial
        // namespaces; the rest (`pc*`, `capslock`, …) match exactly.
        matches!(e, "inet" | "group" | "grp" | "compose" | "level")
            .then(|| token.starts_with(e))
            .unwrap_or(token == e)
    })
}

/// True if `token` looks like a layout code: lowercase ASCII letters,
/// length 2..=8, optionally followed by ASCII digits (e.g. `us`, `de`,
/// `gr`, `latam`, `dvorak`). Deliberately conservative — anything that
/// doesn't fit this shape is treated as ambiguous and rejected by the
/// caller (fail-closed).
fn looks_like_layout(token: &str) -> bool {
    let len = token.len();
    if !(2..=8).contains(&len) {
        return false;
    }
    let mut chars = token.chars();
    // First char must be a lowercase ASCII letter.
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    // Remaining: lowercase letters, then optionally digits — but never
    // a letter after a digit (keeps it to `<alpha><digits>`).
    let mut seen_digit = false;
    for c in chars {
        if c.is_ascii_lowercase() {
            if seen_digit {
                return false;
            }
        } else if c.is_ascii_digit() {
            seen_digit = true;
        } else {
            return false;
        }
    }
    true
}

/// Parse an XKB `symbols` KcCGST component string into a per-group
/// layout list, or `None` if any segment is ambiguous.
///
/// When a client (Cinnamon) sends `XkbGetKbdByName`, the `symbols`
/// component encodes a multi-group layout as `+`-joined segments, e.g.
/// the captured `pc+us+de:2+us:3+inet(evdev)` (us=group 1, de=group 2,
/// us=group 3; the `:N` suffix is the 1-based group slot). This
/// extracts `layouts = "us,de,us"` / `variants = ",,"` so a later task
/// can recompile a multi-group keymap via xkbcommon
/// `new_from_names(layout=…, variant=…)`.
///
/// This is a NARROW heuristic for the desktop layout-switch path, NOT
/// general KcCGST→RMLVO inversion, and it MUST FAIL CLOSED: any segment
/// that is neither a recognised layout ([`looks_like_layout`]) nor a
/// known non-layout extra ([`is_extra`]) returns `None`, so the caller
/// keeps the current keymap rather than guessing wrong (a silently
/// wrong layout is worse than no switch).
///
/// Each segment is `<layout>[(<variant>)][:<N>]`. The `:N` places the
/// layout at group slot `N-1`; segments without `:N` fill sequentially
/// from slot 0. Sparse slots (gaps) are malformed → `None`. Zero
/// layouts found → `None`.
pub(super) fn parse_symbols_layouts(symbols: &str) -> Option<SymbolsLayouts> {
    // (slot_index, layout, variant) for each layout segment.
    let mut placed: Vec<(usize, String, String)> = Vec::new();
    // Next sequential slot for a segment without an explicit `:N`.
    let mut next_seq = 0usize;
    // RMLVO option `group:variant` entries from the behaviour partials.
    let mut options: Vec<String> = Vec::new();

    for segment in symbols.split('+') {
        if segment.is_empty() {
            continue;
        }
        // Strip a trailing `:N` group-slot suffix.
        let (head, explicit_slot) = match segment.rsplit_once(':') {
            Some((before, n)) => {
                let slot = n.parse::<usize>().ok()?;
                if slot == 0 {
                    return None; // 1-based; :0 is malformed
                }
                (before, Some(slot - 1))
            }
            None => (segment, None),
        };
        // Strip an optional `(variant)` suffix.
        let (token, variant) = match head.split_once('(') {
            Some((tok, rest)) => {
                let variant = rest.strip_suffix(')')?; // unbalanced paren → malformed
                (tok, variant)
            }
            None => (head, ""),
        };

        // A recognised behaviour partial with a `(variant)` is an RMLVO
        // option (`level3(ralt_switch)` → `lv3:ralt_switch`), not a
        // layout. Capture it and move on; a variant-less option partial
        // (e.g. bare `compose`) carries no chooser, so just skip it.
        if let Some(group) = option_group_for(token) {
            if !variant.is_empty() {
                options.push(format!("{group}:{variant}"));
            }
            continue;
        }
        if is_extra(token) {
            // Pure model / `inet` extra (no option) — skip. A `:N` on an
            // extra is unexpected but harmless; it doesn't claim a slot.
            continue;
        }
        if !looks_like_layout(token) {
            // Ambiguous (neither layout nor known extra) → fail closed.
            return None;
        }

        let slot = match explicit_slot {
            Some(s) => s,
            None => {
                let s = next_seq;
                next_seq += 1;
                s
            }
        };
        placed.push((slot, token.to_string(), variant.to_string()));
    }

    if placed.is_empty() {
        return None;
    }

    // Order by slot and require a dense 0..n range (no gaps, no dupes).
    placed.sort_by_key(|(slot, _, _)| *slot);
    for (expected, (slot, _, _)) in placed.iter().enumerate() {
        if *slot != expected {
            return None; // sparse / duplicate slot → malformed
        }
    }

    let layouts = placed
        .iter()
        .map(|(_, l, _)| l.as_str())
        .collect::<Vec<_>>()
        .join(",");
    let variants = placed
        .iter()
        .map(|(_, _, v)| v.as_str())
        .collect::<Vec<_>>()
        .join(",");
    Some(SymbolsLayouts {
        layouts,
        variants,
        options: options.join(","),
    })
}

// ── XkbGetKbdByName (minor 23) ───────────────────────────────────────
//
// XkbGBN_* component masks (XKB.h:723-732). These name the components a
// GetKbdByName reply can carry; the reply's `found`/`reported` fields are
// bitsets over them, and each set bit (in `reported`) MUST have a matching
// full nested component-reply block concatenated after the header.
const GBN_TYPES: u16 = 1 << 0;
const GBN_COMPAT_MAP: u16 = 1 << 1;
const GBN_CLIENT_SYMBOLS: u16 = 1 << 2;
const GBN_SERVER_SYMBOLS: u16 = 1 << 3;
const GBN_INDICATOR_MAP: u16 = 1 << 4;
const GBN_KEY_NAMES: u16 = 1 << 5;
const GBN_GEOMETRY: u16 = 1 << 6;
const GBN_OTHER_NAMES: u16 = 1 << 7;

/// Number of indicator slots in a full `xkbGetIndicatorMapReply`: XKB
/// fixes the indicator count at 32 (`XkbNumIndicators`), so the populated
/// reply always carries 32 `xkbIndicatorMapWireDesc` records.
const XKB_NUM_INDICATORS: usize = 32;

/// Decoded `xkbIndicatorMapWireDesc` (XKBproto.h:481, 12 bytes):
/// `flags:u8, whichGroups:u8, groups:u8, whichMods:u8, mods:u8,
/// realMods:u8, vmods:u16-le, ctrls:u32-le`.
#[derive(Default, Clone, Copy)]
struct IndicatorMapWire {
    flags: u8,
    which_groups: u8,
    groups: u8,
    which_mods: u8,
    mods: u8,
    real_mods: u8,
    vmods: u16,
    ctrls: u32,
}

impl IndicatorMapWire {
    fn write_into(&self, dst: &mut [u8]) {
        dst[0] = self.flags;
        dst[1] = self.which_groups;
        dst[2] = self.groups;
        dst[3] = self.which_mods;
        dst[4] = self.mods;
        dst[5] = self.real_mods;
        dst[6..8].copy_from_slice(&self.vmods.to_le_bytes());
        dst[8..12].copy_from_slice(&self.ctrls.to_le_bytes());
    }
}

/// `XkbIM_*` whichMods/whichGroups state-component bits (XKB.h).
const XKB_IM_USE_LOCKED: u8 = 4; // whichModState= locked
const XKB_IM_USE_EFFECTIVE: u8 = 8; // default whichGroups for a `groups=` def

/// `XkbIM_*` per-indicator behaviour flags (XKB.h). These are NOT serialised
/// by libxkbcommon — Xorg derives them per standard indicator NAME — so we
/// supply them from the XKB standard (golden-vector-verified).
const XKB_IM_NO_EXPLICIT: u8 = 0x80; // Caps/Num/Shift Lock, Group 2
const XKB_IM_LED_DRIVES_KB: u8 = 0x20; // Mouse Keys

/// `XkbMouseKeysMask` control bit (`controls= MouseKeys`).
const XKB_CTRL_MOUSE_KEYS: u32 = 0x10;

/// Per-indicator `flags` by standard indicator name (the XKB standard
/// numbering libxkbcommon omits from the text dump). Names not listed get
/// 0. Golden-vector source: cinnamon-xorg.xtrace:6202 (see
/// docs/superpowers/findings/2026-06-25-xkb-indicator-compat-golden-vector.md).
fn indicator_flags_by_name(name: &str) -> u8 {
    match name {
        "Caps Lock" | "Num Lock" | "Shift Lock" | "Group 2" => XKB_IM_NO_EXPLICIT,
        "Mouse Keys" => XKB_IM_LED_DRIVES_KB,
        _ => 0, // Scroll Lock and the named-but-mapless slots
    }
}

/// Resolve a modifier name appearing in an `indicator { modifiers= … }`
/// def to its `(mods, realMods, vmods)` contribution.
///
/// * A REAL-mod name (`Shift`=0x01, `Lock`=0x02, `Control`=0x04,
///   `Mod1`=0x08…`Mod5`=0x80) sets both `mods` and `realMods`.
/// * A VIRTUAL-mod name (`NumLock`, `ScrollLock`, …) sets the `vmods`
///   bit for that vmod AND ORs the real mask the vmod is bound to into
///   `mods` (e.g. NumLock→Mod2 0x10; ScrollLock bound to nothing real → 0).
///   The vmod bit index and binding come from `virtual_mods_from_keymap`
///   — the SAME assignment GetMap/GetNames publish, so the reply is
///   internally consistent (see the divergence note on the test).
fn resolve_indicator_modifier(name: &str, vmod: &VirtualModData) -> (u8, u8, u16) {
    const REAL_MODS: [(&str, u8); 8] = [
        ("Shift", 0x01),
        ("Lock", 0x02),
        ("Control", 0x04),
        ("Mod1", 0x08),
        ("Mod2", 0x10),
        ("Mod3", 0x20),
        ("Mod4", 0x40),
        ("Mod5", 0x80),
    ];
    if let Some((_, bit)) = REAL_MODS.iter().find(|(n, _)| *n == name) {
        return (*bit, *bit, 0);
    }
    // Virtual modifier: find its assigned vmod index + real binding.
    if let Some((idx, _)) = vmod.names.iter().find(|(_, n)| *n == name) {
        let vbit = 1u16 << idx;
        let real = vmod.bindings[usize::from(*idx)];
        return (real, 0, vbit);
    }
    (0, 0, 0)
}

/// Parse the `xkb_keycodes` `indicator N = "Name";` declarations into
/// `(slot, name)` pairs (slot = N − 1). Slots ≥ 32 (none in practice) are
/// dropped.
fn parse_indicator_names(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut in_keycodes = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("xkb_keycodes") {
            in_keycodes = true;
            continue;
        }
        if in_keycodes && t == "};" {
            break;
        }
        if !in_keycodes {
            continue;
        }
        // `indicator N = "Name";`
        let Some(rest) = t.strip_prefix("indicator") else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(eq) = rest.find('=') else { continue };
        let Ok(n) = rest[..eq].trim().parse::<usize>() else {
            continue;
        };
        let Some(q1) = rest[eq..].find('"') else {
            continue;
        };
        let after = &rest[eq + q1 + 1..];
        let Some(q2) = after.find('"') else { continue };
        let name = after[..q2].to_owned();
        if n >= 1 && (n - 1) < XKB_NUM_INDICATORS {
            out.push((n - 1, name));
        }
    }
    out
}

/// Parse the `xkb_compatibility` `indicator "Name" { … };` blocks into a
/// name→`IndicatorMapWire` body map, deriving `whichMods`/`mods`/`realMods`/
/// `vmods`/`groups`/`whichGroups`/`ctrls` from the def's statements. The
/// per-indicator `flags` are filled separately (by name) — libxkbcommon
/// does not serialise them.
fn parse_compat_indicator_maps(
    text: &str,
    vmod: &VirtualModData,
) -> Vec<(String, IndicatorMapWire)> {
    let mut out: Vec<(String, IndicatorMapWire)> = Vec::new();
    let mut in_compat = false;
    let mut cur_name: Option<String> = None;
    let mut cur = IndicatorMapWire::default();
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("xkb_compatibility") {
            in_compat = true;
            continue;
        }
        if !in_compat {
            continue;
        }
        if cur_name.is_none() {
            // Look for `indicator "Name" {` — but NOT inside an interpret.
            if let Some(rest) = t.strip_prefix("indicator ")
                && let Some(q1) = rest.find('"')
                && let Some(q2) = rest[q1 + 1..].find('"')
            {
                cur_name = Some(rest[q1 + 1..q1 + 1 + q2].to_owned());
                cur = IndicatorMapWire::default();
            }
            continue;
        }
        // Inside an indicator block: collect statements until `};`.
        if t == "};" {
            if let Some(name) = cur_name.take() {
                out.push((name, cur));
            }
            continue;
        }
        if let Some(v) = t.strip_prefix("whichModState=") {
            if v.trim().trim_end_matches(';').trim() == "locked" {
                cur.which_mods = XKB_IM_USE_LOCKED;
            }
        } else if let Some(v) = t.strip_prefix("modifiers=") {
            for part in v.trim().trim_end_matches(';').split('+') {
                let nm = part.trim();
                if nm.is_empty() {
                    continue;
                }
                let (m, rm, vm) = resolve_indicator_modifier(nm, vmod);
                cur.mods |= m;
                cur.real_mods |= rm;
                cur.vmods |= vm;
            }
        } else if let Some(v) = t.strip_prefix("groups=") {
            let raw = v.trim().trim_end_matches(';').trim();
            // `groups= 0xfffffffe;` (a group mask) — low byte → `groups`,
            // whichGroups defaults to UseEffective.
            let parsed = if let Some(hex) = raw.strip_prefix("0x") {
                u32::from_str_radix(hex, 16).ok()
            } else {
                raw.parse::<u32>().ok()
            };
            if let Some(g) = parsed {
                cur.groups = (g & 0xff) as u8;
                cur.which_groups = XKB_IM_USE_EFFECTIVE;
            }
        } else if let Some(v) = t.strip_prefix("controls=") {
            for part in v.trim().trim_end_matches(';').split('+') {
                if part.trim() == "MouseKeys" {
                    cur.ctrls |= XKB_CTRL_MOUSE_KEYS;
                }
            }
        }
    }
    out
}

/// XKB GetIndicatorMap reply (XKBproto.h:481-495,
/// `xkbGetIndicatorMapReply`): a 32-byte header plus
/// `nIndicators` (= 32) × 12-byte `xkbIndicatorMapWireDesc` records,
/// derived from the live keymap. Total = 32 + 32*12 = 416 bytes.
///
/// Derivation (libxkbcommon text dump via `get_as_string`):
/// * `xkb_keycodes` `indicator N = "Name";` → slot = N − 1 + the slot's name.
/// * `xkb_compatibility` `indicator "Name" { … };` → that slot's wire map
///   (whichMods/mods/realMods/vmods/groups/whichGroups/ctrls).
///
/// `flags` and `realIndicators` are NOT in the text (libxkbcommon omits the
/// per-indicator flags and the real/virtual indicator split). They are
/// supplied from the XKB standard, golden-vector-verified against a real Xorg
/// capture — see
/// docs/superpowers/findings/2026-06-25-xkb-indicator-compat-golden-vector.md.
/// Derive the 32 indicator slots from the live keymap, plus the
/// `(slot, name)` declarations. Shared by [`reply_get_indicator_map`]
/// and [`reply_get_named_indicator`] so the two replies agree on every
/// slot's map body, flags, and the slot→name pairing by construction.
///
/// Returns `(slots, names)` where `slots[i]` is the assembled
/// `IndicatorMapWire` for indicator index `i` (default/empty for slots
/// with no compat def) and `names` is the `(slot, name)` list from the
/// `xkb_keycodes` `indicator N = "Name";` declarations.
fn indicator_slots(
    keymap: &Keymap,
) -> ([IndicatorMapWire; XKB_NUM_INDICATORS], Vec<(usize, String)>) {
    let text = keymap.get_as_string(xkbcommon::xkb::KEYMAP_FORMAT_TEXT_V1);
    let vmod = virtual_mods_from_keymap(keymap);

    let names = parse_indicator_names(&text);
    let compat_maps = parse_compat_indicator_maps(&text, &vmod);

    let mut slots = [IndicatorMapWire::default(); XKB_NUM_INDICATORS];
    for (slot, name) in &names {
        let mut map = compat_maps
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, m)| *m)
            .unwrap_or_default();
        // Per-indicator flags are by NAME (not serialised by libxkbcommon).
        map.flags = indicator_flags_by_name(name);
        slots[*slot] = map;
    }
    (slots, names)
}

pub(super) fn reply_get_indicator_map(keymap: &Keymap) -> Vec<u8> {
    let (slots, _names) = indicator_slots(keymap);

    // -- Header (32 bytes) --------------------------------------------
    // `which`/`realIndicators`/`nIndicators` are fixed per the XKB standard
    // (libxkbcommon does not expose the real/virtual indicator split):
    //   which          = XkbAllIndicatorsMask (0xffffffff)
    //   realIndicators = 0x000007ff (evdev real-indicator block, 1–11)
    //   nIndicators    = 32 (XkbNumIndicators)
    // Golden-vector-verified — see the findings doc cited above.
    const WHICH: u32 = 0xffff_ffff;
    const REAL_INDICATORS: u32 = 0x0000_07ff;

    let body_len = XKB_NUM_INDICATORS * 12;
    let mut r = vec![0u8; 32 + body_len];
    r[0] = 1; // reply type
    r[1] = 1; // deviceID = 1
    // [2..4] sequence: rewritten by the caller
    let length_words = u32::try_from(body_len / 4).unwrap_or(0);
    r[4..8].copy_from_slice(&length_words.to_le_bytes());
    r[8..12].copy_from_slice(&WHICH.to_le_bytes());
    r[12..16].copy_from_slice(&REAL_INDICATORS.to_le_bytes());
    r[16] = u8::try_from(XKB_NUM_INDICATORS).unwrap_or(u8::MAX); // nIndicators (CARD8)
    // [17] pad1, [18..20] pad2, [20..32] pad3/4/5

    // -- Body: 32 × 12-byte indicator maps ---------------------------
    for (i, slot) in slots.iter().enumerate() {
        let off = 32 + i * 12;
        slot.write_into(&mut r[off..off + 12]);
    }
    r
}

/// XKB GetState reply (minor 4) — `xkbGetStateReply` (sz=32, length=0).
/// Layout (XKBproto.h `_xkbGetStateReply`):
///   `mods@8 baseMods@9 latchedMods@10 lockedMods@11 group@12
///    lockedGroup@13 baseGroup:INT16@14 latchedGroup:INT16@16
///    compatState@18 grabMods@19 compatGrabMods@20 lookupMods@21
///    compatLookupMods@22 pad1@23 ptrBtnState:CARD16@24 pad2@26 pad3@28`.
///
/// Derived from the live `xkb_state` plus the authoritative
/// `locked_group` yserver stamps into events. yserver has no base/latched
/// group, so effective group == locked group == `locked_group`. The
/// modifier masks come straight from xkbcommon (`serialize_mods`); only
/// the low 8 bits (real mods Shift/Lock/Control/Mod1..Mod5) are wire
/// CARD8s. compat/grab/lookup/ptrBtn state are all zero (yserver tracks
/// no passive grabs or compat-state). A steady group-0/no-lock state
/// therefore byte-matches the all-zero Xorg capture (trace 4271/10952).
pub(super) fn reply_get_state(state: &xkbcommon::xkb::State, locked_group: u8) -> Vec<u8> {
    // Real-mod masks are the low 8 bits of the serialized mask.
    let effective = state.serialize_mods(xkbcommon::xkb::STATE_MODS_EFFECTIVE) as u8;
    let base = state.serialize_mods(xkbcommon::xkb::STATE_MODS_DEPRESSED) as u8;
    let latched = state.serialize_mods(xkbcommon::xkb::STATE_MODS_LATCHED) as u8;
    let locked = state.serialize_mods(xkbcommon::xkb::STATE_MODS_LOCKED) as u8;

    let mut r = vec![0u8; 32];
    r[0] = 1; // reply type
    r[1] = 1; // deviceID = 1
    // [2..4] sequence: rewritten by the caller
    // [4..8] length = 0
    r[8] = effective; // mods (effective real-mod mask)
    r[9] = base; // baseMods
    r[10] = latched; // latchedMods
    r[11] = locked; // lockedMods
    r[12] = locked_group; // group (effective == locked; no base/latched group)
    r[13] = locked_group; // lockedGroup
    // [14..16] baseGroup:INT16 = 0, [16..18] latchedGroup:INT16 = 0
    // [18] compatState, [19] grabMods, [20] compatGrabMods,
    // [21] lookupMods, [22] compatLookupMods, [23] pad1,
    // [24..26] ptrBtnState:CARD16 = 0, [26..28] pad2, [28..32] pad3 — all 0.
    r
}

/// XKB GetNamedIndicator reply (minor 15) — `xkbGetNamedIndicatorReply`
/// (sz=32, length=0). Layout (XKBproto.h `_xkbGetNamedIndicatorReply`):
///   `indicator:Atom@8 found@12 on@13 realIndicator@14 ndx@15 flags@16
///    whichGroups@17 groups@18 whichMods@19 mods@20 realMods@21
///    virtualMods:CARD16@22 ctrls:CARD32@24 supported@28`.
///
/// Request body (after the 4-byte XKB header the core loop strips):
/// `deviceSpec(2) ledClass(2) ledID(2) pad1(2) indicator:Atom(4)` — the
/// requested indicator atom is at `body[8..12]`
/// (`sz_xkbGetNamedIndicatorReq` = 16).
///
/// Reuses the SAME indicator derivation as [`reply_get_indicator_map`]
/// ([`indicator_slots`]) so the map fields agree by construction. The
/// requested atom is resolved by interning each indicator's name and
/// comparing: a match fills the slot's map body, `found=1`,
/// `realIndicator = (slot < 11)` (the evdev real block 0x7ff), `on =
/// state.led_name_is_active(name)`, `supported=1`. No match → `found=0`,
/// `supported=1`, rest 0. Golden: cinnamon-xorg.xtrace 87810/87812.
pub(super) fn reply_get_named_indicator(
    keymap: &Keymap,
    state: &xkbcommon::xkb::State,
    body: &[u8],
    intern_atom: &mut dyn FnMut(&str) -> u32,
) -> Vec<u8> {
    let requested = if body.len() >= 12 {
        u32::from_le_bytes([body[8], body[9], body[10], body[11]])
    } else {
        0
    };

    let (slots, names) = indicator_slots(keymap);

    let mut r = vec![0u8; 32];
    r[0] = 1; // reply type
    r[1] = 1; // deviceID = 1
    // [2..4] sequence: rewritten by the caller; [4..8] length = 0
    r[8..12].copy_from_slice(&requested.to_le_bytes()); // indicator
    r[28] = 1; // supported = TRUE

    // Resolve the requested atom to one of the named indicator slots.
    for (slot, name) in &names {
        if intern_atom(name) != requested {
            continue;
        }
        let map = &slots[*slot];
        r[12] = 1; // found = TRUE
        r[13] = u8::from(state.led_name_is_active(name)); // on
        r[14] = u8::from(*slot < 11); // realIndicator (real block 0x7ff = idx 0..=10)
        r[15] = u8::try_from(*slot).unwrap_or(u8::MAX); // ndx
        r[16] = map.flags;
        r[17] = map.which_groups;
        r[18] = map.groups;
        r[19] = map.which_mods;
        r[20] = map.mods;
        r[21] = map.real_mods;
        r[22..24].copy_from_slice(&map.vmods.to_le_bytes()); // virtualMods
        r[24..28].copy_from_slice(&map.ctrls.to_le_bytes()); // ctrls
        break;
    }

    r
}

/// Minimal empty `xkbGetGeometryReply` (XKBproto.h:796-815,
/// `sz_xkbGetGeometryReply` = 32). `name`=0, all section counts 0 — a
/// structurally-valid empty geometry, the embedded block for
/// `XkbGBN_GeometryMask`.
///
/// `found`=FALSE: libxkbcommon dropped XKB geometry entirely (it never
/// compiles a `xkb_geometry` section), so we have no geometry to report.
/// Xorg only has geometry via xkbcomp, and no xkbcommon-x11/Wayland client
/// consumes XKB geometry — faithful geometry would need an xkbcomp-style
/// parser, which is deferred. Reporting found=FALSE (rather than TRUE over
/// an all-zero body) tells a client truthfully that no geometry is present.
fn reply_get_geometry() -> Vec<u8> {
    let mut r = vec![0u8; 32];
    r[0] = 1; // reply type
    r[1] = 1; // deviceID = 1
    // [4..8] length = 0 (no trailing geometry sections)
    // [8..12] name atom = 0
    r[12] = 0; // found = FALSE (no geometry; see doc-comment above)
    // [13] pad, [14..] widthMM/heightMM/nProperties/.../labelColorNdx = 0
    r
}

/// Convert the X-protocol XkbGBN_* component mask the client sends in
/// `want`/`need` into the set of components actually located/embeddable,
/// mirroring Xorg's `XkbConvertGetByNameComponents` round-trip
/// (xkbfmisc.c:397). Maps to XKM-space and back: `XkbGBN_SymbolsMask` (both
/// Client+Server symbol bits) collapses to one XKM symbols bit and expands
/// back to BOTH; any nonzero result implies `XkbGBN_OtherNamesMask`. The
/// `OtherNames` pseudo-component has no XKM bit, so it never survives the
/// round-trip unless re-added by the `orig != 0` clause.
fn convert_gbn_components(orig: u16) -> u16 {
    // toXkm: GBN bits -> XKM bits.
    let mut xkm: u16 = 0;
    if orig & GBN_TYPES != 0 {
        xkm |= 1 << 0; // XkmTypesMask
    }
    if orig & GBN_COMPAT_MAP != 0 {
        xkm |= 1 << 1; // XkmCompatMapMask
    }
    if orig & (GBN_CLIENT_SYMBOLS | GBN_SERVER_SYMBOLS) != 0 {
        xkm |= 1 << 2; // XkmSymbolsMask
    }
    if orig & GBN_INDICATOR_MAP != 0 {
        xkm |= 1 << 3; // XkmIndicatorsMask
    }
    if orig & GBN_KEY_NAMES != 0 {
        xkm |= 1 << 4; // XkmKeyNamesMask
    }
    if orig & GBN_GEOMETRY != 0 {
        xkm |= 1 << 5; // XkmGeometryMask
    }
    // fromXkm: XKM bits -> GBN bits.
    let mut gbn: u16 = 0;
    if xkm & (1 << 0) != 0 {
        gbn |= GBN_TYPES;
    }
    if xkm & (1 << 1) != 0 {
        gbn |= GBN_COMPAT_MAP;
    }
    if xkm & (1 << 2) != 0 {
        gbn |= GBN_CLIENT_SYMBOLS | GBN_SERVER_SYMBOLS;
    }
    if xkm & (1 << 3) != 0 {
        gbn |= GBN_INDICATOR_MAP;
    }
    if xkm & (1 << 4) != 0 {
        gbn |= GBN_KEY_NAMES;
    }
    if xkm & (1 << 5) != 0 {
        gbn |= GBN_GEOMETRY;
    }
    if xkm != 0 {
        gbn |= GBN_OTHER_NAMES;
    }
    gbn
}

/// Build the `XkbGetKbdByName` (minor 23) reply: a fixed 32-byte
/// `xkbGetKbdByNameReply` header (XKBproto.h:904-920) followed by full nested
/// component-reply blocks, one per bit set in `reported`, in XkbGBN bit order:
/// GetMap (Types|Symbols), CompatMap, IndicatorMap, GetNames
/// (KeyNames|OtherNames), Geometry. Each nested block is a COMPLETE reply
/// (its own header included) — the GetMap block carries the 40-byte
/// `xkbGetMapReply` header, the rest 32-byte headers — exactly as Xorg's
/// `XkbSendMap`/`XkbSendCompatMap`/… concatenate them (xkb.c:6258-6280).
///
/// `reported` = `convert_gbn_components(want | need)` (mirrors Xorg's
/// `rep.reported = XkbConvertGetByNameComponents(FALSE, fwant|fneed)`), and we
/// embed a block for every reported bit so the body matches the field.
/// `loaded` is the BOOL from the `KeymapLoad` (1 on a successful load). `found`
/// is the component MASK actually located: `reported & ~OtherNames` on success
/// (matching the captured Xorg reply's found=0x7f vs reported=0xff), 0 on
/// failure. `min`/`max` come from the (now-current) keymap.
///
/// Ground truth: cinnamon-xorg.xtrace:6202 (header found=0x7f reported=0xff
/// loaded=1, first embedded block = 40-byte-header GetMap). The embedded
/// CompatMap/IndicatorMap/Geometry blocks are STRUCTURALLY-VALID but EMPTY
/// (Xorg embeds populated ones); the client (Cinnamon/muffin) frees the
/// returned keymap and re-reads via GetMap after the NewKeyboardNotify, so the
/// reply is a success gate that must only parse cleanly in libX11's
/// XkbGetKeyboardByName, which it does for an empty-but-framed block.
pub(super) fn reply_get_kbd_by_name(
    keymap: &Keymap,
    rmlvo: &crate::kms::core::XkbRmlvo,
    want: u16,
    need: u16,
    loaded: bool,
    intern_atom: &mut dyn FnMut(&str) -> u32,
) -> Vec<u8> {
    let (min_kc, max_kc) = clamped_keycode_bounds(keymap);

    // reported = round-trip of (want|need) — the components we will embed.
    let reported = convert_gbn_components(want | need);
    // found = located components. On a successful load we have every real
    // component; mirror Xorg's capture where found omits the OtherNames
    // pseudo-bit (found=0x7f, reported=0xff). On failure nothing was located.
    let found: u16 = if loaded {
        reported & !GBN_OTHER_NAMES
    } else {
        0
    };

    // Assemble the nested blocks in XkbGBN bit order, each a FULL reply.
    let mut body: Vec<u8> = Vec::new();
    if reported & (GBN_TYPES | GBN_CLIENT_SYMBOLS | GBN_SERVER_SYMBOLS) != 0 {
        body.extend_from_slice(&reply_get_map(keymap));
    }
    if reported & GBN_COMPAT_MAP != 0 {
        body.extend_from_slice(&reply_get_compat_map());
    }
    if reported & GBN_INDICATOR_MAP != 0 {
        body.extend_from_slice(&reply_get_indicator_map(keymap));
    }
    if reported & (GBN_KEY_NAMES | GBN_OTHER_NAMES) != 0 {
        body.extend_from_slice(&reply_get_names(keymap, rmlvo, intern_atom));
    }
    if reported & GBN_GEOMETRY != 0 {
        body.extend_from_slice(&reply_get_geometry());
    }

    // The nested blocks are each 4-byte aligned (every reply_* helper returns
    // a multiple-of-4 length), so the concatenation is too; length is in
    // 4-byte units of the trailing body.
    debug_assert_eq!(body.len() % 4, 0, "nested blocks must be 4-byte aligned");
    let length_words = u32::try_from(body.len() / 4).unwrap_or(u32::MAX);

    let mut r = vec![0u8; 32 + body.len()];
    r[0] = 1; // type = Reply
    r[1] = 1; // deviceID = 1 — must match the embedded blocks' deviceID
    // [2..4] sequenceNumber: rewritten by the caller
    r[4..8].copy_from_slice(&length_words.to_le_bytes());
    r[8] = min_kc; // minKeyCode
    r[9] = max_kc; // maxKeyCode
    r[10] = u8::from(loaded); // loaded (BOOL)
    r[11] = 0; // newKeyboard (BOOL) — the NKN is a separate broadcast event
    r[12..14].copy_from_slice(&found.to_le_bytes()); // found (XkbGBN_* mask)
    r[14..16].copy_from_slice(&reported.to_le_bytes()); // reported (XkbGBN_* mask)
    // [16..32] pad1..pad4 = 0
    r[32..].copy_from_slice(&body);
    r
}

/// One `{ realMods → level }` row of an XKB key-type's map. The wire
/// `xkbKTMapEntryWireDesc` carries `mask`/`realMods`/`vmods` plus the
/// resolved `level`; C1 captures the *real-modifier* mask that selects
/// the level (`key_get_mods_for_level` returns real-mod masks, with
/// vmod-bound bits already devirtualised — e.g. LevelThree shows up as
/// real Mod5 = 0x80, per the committed AltGr golden vector). C2 maps
/// `real_mods` back through the published VModMap to fill the wire
/// `vmods` field where appropriate.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct KeyTypeMapEntry {
    pub real_mods: u8,
    pub level: u8,
}

/// A deduplicated XKB key-type descriptor. `num_levels` is the type's
/// shift-level count (1=ONE_LEVEL, 2=TWO_LEVEL/ALPHABETIC/KEYPAD,
/// 4=FOUR_LEVEL, …); `map_entries` are the modifier→level rows for
/// levels ≥ 1 (level 0 is the implicit no-modifier default and carries
/// no entry).
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct KeyTypeDesc {
    pub num_levels: u8,
    pub map_entries: Vec<KeyTypeMapEntry>,
}

/// The derived key-type table plus the per-(keycode, group) index into
/// it. C2 serialises `types` into the GetMap `KeyTypes` section and
/// assigns each key's `ktIndex[group]` from `type_index_for`.
#[derive(Clone, Debug)]
pub(super) struct KeyTypeTable {
    /// Deduplicated descriptors. Indices 0..=3 are the XKB
    /// required-types seed (ONE_LEVEL / TWO_LEVEL / ALPHABETIC /
    /// KEYPAD); derived types follow.
    pub types: Vec<KeyTypeDesc>,
    /// `(keycode, group) → index into `types`. Only populated for
    /// (kc, g) pairs the key actually publishes.
    pub index_for: std::collections::HashMap<(u8, u8), u8>,
}

impl KeyTypeTable {
    /// Type index for a key's group, or 0 (ONE_LEVEL) when the pair
    /// wasn't published (matches the fallback `reply_get_map` uses for
    /// groups beyond a key's count).
    pub fn type_index_for(&self, kc: u8, group: u8) -> u8 {
        self.index_for.get(&(kc, group)).copied().unwrap_or(0)
    }
}

/// Canonical signature for dedup: `(num_levels, sorted entries)`.
fn type_signature(desc: &KeyTypeDesc) -> (u8, Vec<(u8, u8)>) {
    let mut entries: Vec<(u8, u8)> = desc
        .map_entries
        .iter()
        .map(|e| (e.real_mods, e.level))
        .collect();
    entries.sort_unstable();
    entries.dedup();
    (desc.num_levels, entries)
}

/// Derive the X11 XKB key-type table from the live xkbcommon keymap.
///
/// Seeds indices 0..=3 with the XKB required types (ONE_LEVEL,
/// TWO_LEVEL, ALPHABETIC, KEYPAD) — `XkbAllocClientMap` rejects
/// `nTypes < XkbNumRequiredTypes` (= 4), the same invariant
/// [`reply_get_map`] preserves with its `nTypes = 4`. Then, for every
/// published (keycode, group), it asks xkbcommon which real-modifier
/// masks select each level ≥ 1 (`key_get_mods_for_level`) and folds the
/// resulting descriptor into the table, deduplicating by
/// (num_levels, sorted entries). 2-level keys whose map matches the
/// seeded TWO_LEVEL collapse onto index 1; higher-level keys (notably
/// the AltGr FOUR_LEVEL letters/symbols) get fresh indices.
///
/// Golden vector (`docs/.../2026-06-25-altgr-4level-golden-vector.md`):
/// the German `e` key (keycode 26, AD03) must yield a 4-level type whose
/// entries include `{real_mods: 0x80, level: 2}` (LevelThree ⟷ Mod5) —
/// the € path — alongside Shift→1 and Shift+LevelThree→3.
pub(super) fn key_types_from_keymap(keymap: &Keymap) -> KeyTypeTable {
    // -- Seed the four XKB required types (indices 0..=3). ----------
    let mut types: Vec<KeyTypeDesc> = vec![
        // 0: ONE_LEVEL — single level, no modifier map.
        KeyTypeDesc {
            num_levels: 1,
            map_entries: Vec::new(),
        },
        // 1: TWO_LEVEL — Shift (0x01) → level 1.
        KeyTypeDesc {
            num_levels: 2,
            map_entries: vec![KeyTypeMapEntry {
                real_mods: 0x01,
                level: 1,
            }],
        },
        // 2: ALPHABETIC — Shift (0x01) → level 1 (Caps handled by the
        //    server's lock logic; same two-row shape as TWO_LEVEL).
        KeyTypeDesc {
            num_levels: 2,
            map_entries: vec![KeyTypeMapEntry {
                real_mods: 0x01,
                level: 1,
            }],
        },
        // 3: KEYPAD — two-level stub (NumLock-driven; published so the
        //    required-types count holds).
        KeyTypeDesc {
            num_levels: 2,
            map_entries: vec![KeyTypeMapEntry {
                real_mods: 0x01,
                level: 1,
            }],
        },
    ];

    // Signature → index. Seed signatures so matching derived types
    // collapse onto the canonical required indices. ALPHABETIC and
    // KEYPAD share TWO_LEVEL's signature; first-inserted (index 1)
    // wins the lookup, so a derived 2-level Shift→1 key reuses index 1.
    let mut sig_index: std::collections::HashMap<(u8, Vec<(u8, u8)>), u8> =
        std::collections::HashMap::new();
    for (i, desc) in types.iter().enumerate() {
        sig_index
            .entry(type_signature(desc))
            .or_insert_with(|| u8::try_from(i).unwrap_or(0));
    }

    let mut index_for: std::collections::HashMap<(u8, u8), u8> = std::collections::HashMap::new();

    // Same clamp idiom as reply_get_map: X11 keycodes are CARD8.
    let (min_kc, max_kc) = clamped_keycode_bounds(keymap);

    // Buffer for key_get_mods_for_level; 64 covers every mod combo a
    // 4-level type produces (the binding warns only on overflow).
    let mut masks = [0u32; 64];

    for kc_raw in min_kc..=max_kc {
        let kc = Keycode::new(u32::from(kc_raw));
        let layouts = keymap.num_layouts_for_key(kc);
        if layouts == 0 {
            continue;
        }
        let num_groups = layouts.min(u32::from(XKB_NUM_KBD_GROUPS));
        for group in 0..num_groups {
            let levels = keymap.num_levels_for_key(kc, group);
            let n: u8 = u8::try_from(levels).unwrap_or(u8::MAX);
            if n == 0 {
                continue;
            }

            // Level 0 is the implicit no-modifier default — no entry.
            let mut map_entries: Vec<KeyTypeMapEntry> = Vec::new();
            for level in 1..u32::from(n) {
                let count = keymap.key_get_mods_for_level(kc, group, level, &mut masks);
                for &m in &masks[..count] {
                    // key_get_mods_for_level yields real-mod masks
                    // (vmod-bound bits already resolved to their real
                    // modifier — LevelThree → Mod5 0x80). Truncate to
                    // the 8-bit X11 real-mod set. Skip the all-zero
                    // mask (that is the level-0 default leaking in).
                    let real_mods = u8::try_from(m & 0xFF).unwrap_or(0);
                    if real_mods == 0 {
                        continue;
                    }
                    let entry = KeyTypeMapEntry {
                        real_mods,
                        level: u8::try_from(level).unwrap_or(u8::MAX),
                    };
                    if !map_entries.contains(&entry) {
                        map_entries.push(entry);
                    }
                }
            }

            let desc = KeyTypeDesc {
                num_levels: n,
                map_entries,
            };
            let sig = type_signature(&desc);
            let idx = if let Some(&i) = sig_index.get(&sig) {
                i
            } else {
                let i = u8::try_from(types.len()).unwrap_or(u8::MAX);
                types.push(desc);
                sig_index.insert(sig, i);
                i
            };
            index_for.insert((kc_raw, u8::try_from(group).unwrap_or(0)), idx);
        }
    }

    KeyTypeTable { types, index_for }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build the `us,de` keymap matching the capture's RMLVO
    /// (`pc+us+de:2+us:3+inet(evdev)+pc(pc105)` → rules=evdev, model=pc105,
    /// layout=us,de). Source of the IndicatorMap golden vector.
    fn us_de_keymap() -> xkbcommon::xkb::Keymap {
        let ctx = xkbcommon::xkb::Context::new(xkbcommon::xkb::CONTEXT_NO_FLAGS);
        xkbcommon::xkb::Keymap::new_from_names(
            &ctx,
            "evdev",
            "pc105",
            "us,de",
            "",
            None,
            xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .expect("us,de keymap")
    }

    /// External golden-vector test for the populated IndicatorMap block.
    ///
    /// Ground truth: cinnamon-xorg.xtrace:6202 (`us,de`), decoded in
    /// docs/superpowers/findings/2026-06-25-xkb-indicator-compat-golden-vector.md.
    /// The full 416-byte reply (32-byte header + 32 × 12-byte
    /// `xkbIndicatorMapWireDesc`) is asserted byte-for-byte.
    ///
    /// DIVERGENCE from the Xorg capture — the `vmods` field of the Num Lock
    /// and Scroll Lock slots:
    ///   Xorg capture:  Num Lock vmods=0x0001, Scroll Lock vmods=0x0080
    ///   yserver:       Num Lock vmods=0x0002, Scroll Lock vmods=0x0004
    /// This is a PRINCIPLED divergence, not a bug. The `vmods` field is an
    /// index into the keyboard's virtual-modifier table, and Xorg's xkbcomp
    /// and libxkbcommon assign those indices in different orders (Xorg uses a
    /// near-fixed canonical numbering; libxkbcommon allocates over the
    /// keymap's declaration). yserver runs libxkbcommon and publishes the
    /// SAME vmod numbering in GetMap/GetNames via `virtual_mods_from_keymap`
    /// (here: NumLock→idx 1 → bit 0x0002, ScrollLock→idx 2 → bit 0x0004). A
    /// client resolves `vmods` against THIS keyboard's table, so the reply
    /// must use yserver's own indices to stay internally consistent — forcing
    /// Xorg's bytes would point at the wrong vmod. The `mods` field (the
    /// resolved REAL mask: NumLock→Mod2 0x10, ScrollLock→0) is canonical and
    /// DOES match the golden vector, as do flags/realIndicators/which.
    #[test]
    fn indicator_map_matches_us_de_golden_vector() {
        let km = us_de_keymap();
        let got = reply_get_indicator_map(&km);

        // -- Expected (golden) ----------------------------------------
        let mut want = vec![0u8; 32 + 32 * 12];
        want[0] = 1; // type
        want[1] = 1; // deviceID
        // [2..4] sequence: not rewritten in the standalone helper output (0)
        want[4..8].copy_from_slice(&96u32.to_le_bytes()); // length = (416-32)/4
        want[8..12].copy_from_slice(&0xffff_ffffu32.to_le_bytes()); // which
        want[12..16].copy_from_slice(&0x0000_07ffu32.to_le_bytes()); // realIndicators
        want[16] = 32; // nIndicators

        // Non-empty 12-byte slot bodies. The `vmods` (bytes [6..8]) for Num
        // Lock / Scroll Lock are yserver's principled indices (see the
        // divergence note above); every other byte matches the Xorg capture.
        let slot = |idx: usize, bytes: [u8; 12]| (idx, bytes);
        for (idx, bytes) in [
            // flags wG g  wM mods real vmods(le) ctrls(le)
            slot(0, [0x80, 0, 0, 4, 0x02, 0x02, 0x00, 0x00, 0, 0, 0, 0]), // Caps Lock: Lock
            slot(1, [0x80, 0, 0, 4, 0x10, 0x00, 0x02, 0x00, 0, 0, 0, 0]), // Num Lock: NumLock→Mod2, vmod idx1
            slot(2, [0x00, 0, 0, 4, 0x00, 0x00, 0x04, 0x00, 0, 0, 0, 0]), // Scroll Lock: ScrollLock vmod idx2
            slot(11, [0x80, 0, 0, 4, 0x01, 0x01, 0x00, 0x00, 0, 0, 0, 0]), // Shift Lock: Shift
            slot(12, [0x80, 8, 0xfe, 0, 0x00, 0x00, 0x00, 0x00, 0, 0, 0, 0]), // Group 2
            slot(13, [0x20, 0, 0, 0, 0x00, 0x00, 0x00, 0x00, 0x10, 0, 0, 0]), // Mouse Keys: ctrls=0x10
        ] {
            let off = 32 + idx * 12;
            want[off..off + 12].copy_from_slice(&bytes);
        }

        assert_eq!(got.len(), 416, "reply must be 32 + 32*12 bytes");
        assert_eq!(
            got, want,
            "IndicatorMap diverged from golden vector\n got={:02x?}\nwant={:02x?}",
            got, want
        );
    }

    #[test]
    fn key_types_include_four_level_for_altgr() {
        let mut core = crate::kms::core::KmsCore::for_tests();
        core.recompile_keymap(&crate::kms::core::XkbRmlvo {
            rules: "evdev".into(),
            model: "pc105".into(),
            layout: "de".into(),
            variant: String::new(),
            options: None,
        });
        let types = key_types_from_keymap(&core.xkb_keymap.0);
        // A FOUR_LEVEL type must exist, and (golden vector) its map must
        // select level 2 via the LevelThree real-mod (Mod5 = 0x80) — the
        // € path.
        assert!(
            types.types.iter().any(|t| t.num_levels == 4
                && t.map_entries
                    .iter()
                    .any(|e| e.real_mods == 0x80 && e.level == 2)),
            "FOUR_LEVEL type with LevelThree(0x80)->level2 must be derived, got {:?}",
            types.types
        );

        // The de `e` key — X11 keycode 26 (AD03). In a single-layout
        // `de` keymap the AltGr layer rides in group 0 (the multi-group
        // `us,de` capture the golden vector was decoded from put it in
        // group 1; same physical key, same FOUR_LEVEL structure). € is
        // at group 0, level 2 here, proving this is the right key.
        const E_KEY_X11: u8 = 26;
        let g0_syms =
            core.xkb_keymap
                .0
                .key_get_syms_by_level(Keycode::new(u32::from(E_KEY_X11)), 0, 2);
        assert_eq!(
            g0_syms.first().map(|s| s.raw()),
            Some(0x20ac),
            "sanity: de kc26 group0 level2 must be € (0x20ac)"
        );
        let e_g0 = types.type_index_for(E_KEY_X11, 0);
        assert_eq!(
            types.types[usize::from(e_g0)].num_levels,
            4,
            "de e-key (kc26) group 0 must map to a FOUR_LEVEL type, idx {e_g0} of {:?}",
            types.types
        );
        // And that same type carries the full golden-vector real-mod map:
        // Shift→1, LevelThree(Mod5 0x80)→2, Shift+LevelThree(0x81)→3.
        let e_type = &types.types[usize::from(e_g0)];
        for (rm, lvl) in [(0x01u8, 1u8), (0x80, 2), (0x81, 3)] {
            assert!(
                e_type
                    .map_entries
                    .iter()
                    .any(|e| e.real_mods == rm && e.level == lvl),
                "de e-key FOUR_LEVEL must map real_mods {rm:#04x}->level {lvl}, got {e_type:?}"
            );
        }
    }

    #[test]
    fn parse_symbols_basic_multigroup() {
        // The exact strings Cinnamon sent in the capture.
        let r = parse_symbols_layouts("pc+us+de:2+us:3+inet(evdev)").expect("parses");
        assert_eq!(r.layouts, "us,de,us");
        assert_eq!(r.variants, ",,"); // no variants -> empty per group

        let r2 = parse_symbols_layouts("pc+us+us:2+inet(evdev)").expect("parses");
        assert_eq!(r2.layouts, "us,us");
    }

    #[test]
    fn parse_symbols_with_variant() {
        // layout(variant):N
        let r = parse_symbols_layouts("pc+us+gr(polytonic):2+inet(evdev)").expect("parses");
        assert_eq!(r.layouts, "us,gr");
        assert_eq!(r.variants, ",polytonic");
    }

    #[test]
    fn parse_symbols_single_layout() {
        let r = parse_symbols_layouts("pc+de+inet(evdev)").expect("parses");
        assert_eq!(r.layouts, "de");
    }

    #[test]
    fn parse_symbols_latvian_not_treated_as_extra() {
        // `lv` is the Latvian layout, not a level partial — it must not
        // be swallowed by the extras set (regression: a `"lv"` prefix
        // entry silently dropped the Latvian group).
        let r = parse_symbols_layouts("pc+us+lv:2+inet(evdev)").expect("parses");
        assert_eq!(r.layouts, "us,lv");
        let r2 = parse_symbols_layouts("pc+lv+inet(evdev)").expect("parses");
        assert_eq!(r2.layouts, "lv");
    }

    #[test]
    fn parse_symbols_extracts_level3_chooser_option() {
        let r =
            parse_symbols_layouts("pc+us+be:2+us:3+inet(evdev)+capslock(none)+level3(ralt_switch)")
                .expect("parses");
        assert_eq!(r.layouts, "us,be,us");
        // options preserved + mapped: lv3:ralt_switch and caps:none
        assert!(
            r.options.split(',').any(|o| o == "lv3:ralt_switch"),
            "level3(ralt_switch) -> lv3:ralt_switch, got {:?}",
            r.options
        );
        assert!(r.options.split(',').any(|o| o == "caps:none"));
    }

    #[test]
    fn parse_symbols_fail_closed_on_unknown() {
        // An unrecognized, non-extra segment -> None (don't guess).
        assert!(parse_symbols_layouts("pc+us+wat_is_this_xyz:2+inet(evdev)").is_none());
    }

    #[test]
    fn parse_symbols_fail_closed_on_sparse_or_duplicate_slots() {
        // Sparse: us at slot 0, de at slot 2, slot 1 empty -> not a dense
        // 0..n range -> None (don't silently drop a group).
        assert!(parse_symbols_layouts("pc+us+de:3+inet(evdev)").is_none());
        // Duplicate: two layouts claim slot 1 -> None.
        assert!(parse_symbols_layouts("pc+us:1+de:1+inet(evdev)").is_none());
        // Explicit/sequential collision: us:1 (slot 0) + de (sequential slot 0) -> None.
        assert!(parse_symbols_layouts("pc+us:1+de+inet(evdev)").is_none());
    }

    /// RMLVO matching `test_keymap()` (evdev/pc105/us) — yields the
    /// historical `symbolsName = "pc+us+inet(evdev)"`.
    fn test_rmlvo() -> crate::kms::core::XkbRmlvo {
        crate::kms::core::XkbRmlvo {
            rules: "evdev".into(),
            model: "pc105".into(),
            layout: "us".into(),
            variant: String::new(),
            options: None,
        }
    }

    fn test_keymap() -> xkbcommon::xkb::Keymap {
        let ctx = xkbcommon::xkb::Context::new(xkbcommon::xkb::CONTEXT_NO_FLAGS);
        xkbcommon::xkb::Keymap::new_from_names(
            &ctx,
            "evdev",
            "pc105",
            "us",
            "",
            None,
            xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .or_else(|| {
            xkbcommon::xkb::Keymap::new_from_names(
                &ctx,
                "",
                "",
                "",
                "",
                None,
                xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
        })
        .expect("test xkb keymap")
    }

    fn get_map_request_body(full: u16, partial: u16) -> [u8; 20] {
        let mut body = [0u8; 20];
        body[0..2].copy_from_slice(&0x0100_u16.to_le_bytes()); // UseCoreKbd
        body[2..4].copy_from_slice(&full.to_le_bytes());
        body[4..6].copy_from_slice(&partial.to_le_bytes());
        body
    }

    /// Parsed `KeySymMap` entry for one keycode, pulled out of a
    /// `reply_get_map` byte stream by walking the KeySyms section.
    struct ParsedKeySymMap {
        num_groups: u8,
        width: u8,
        n_syms: u16,
        kt_index: [u8; 4],
        /// `group_syms[g][l]` = the keysym at group `g`, level `l`
        /// (group-major wire order). Only `num_groups` groups and
        /// `width` levels are populated.
        group_syms: Vec<Vec<u32>>,
    }

    /// One parsed `xkbKeyTypeWireDesc` from the KeyTypes section.
    struct ParsedKeyType {
        mask: u8,
        num_levels: u8,
        /// `(mask, level, real_mods)` per `xkbKTMapEntryWireDesc`.
        entries: Vec<(u8, u8, u8)>,
    }

    /// Walk the KeyTypes section (variable: each type is an 8-byte
    /// header + `nMapEntries` × 8-byte entries) starting at byte 40.
    /// Returns the parsed types and the byte offset where KeySyms
    /// begins. `nTypes` is read from the reply header (`r[15]`) —
    /// never assumed to be 4.
    fn parse_key_types(reply: &[u8]) -> (Vec<ParsedKeyType>, usize) {
        let n_types = reply[15];
        let mut off = 40;
        let mut types = Vec::with_capacity(usize::from(n_types));
        for _ in 0..n_types {
            let mask = reply[off];
            let num_levels = reply[off + 4];
            let n_entries = reply[off + 5];
            let mut entries = Vec::with_capacity(usize::from(n_entries));
            let mut eoff = off + 8;
            for _ in 0..n_entries {
                // xkbKTMapEntryWireDesc: active(1) mask(1) level(1)
                // realMods(1) virtualMods(2) pad(2).
                assert_eq!(reply[eoff], 1, "map entry active flag must be 1");
                entries.push((reply[eoff + 1], reply[eoff + 2], reply[eoff + 3]));
                eoff += 8;
            }
            types.push(ParsedKeyType {
                mask,
                num_levels,
                entries,
            });
            off = eoff;
        }
        (types, off)
    }

    /// Walk a `reply_get_map` reply to the `KeySymMap` entry for the
    /// given keycode and decode it. The KeyTypes section is
    /// variable-size (the published type table is derived from the
    /// keymap, not a fixed 4-type block), so locate the KeySyms start
    /// by walking the types rather than assuming a fixed offset.
    fn parse_keysym_map_for_keycode(reply: &[u8], keycode: u8) -> ParsedKeySymMap {
        let min_kc = reply[10];
        let max_kc = reply[11];
        assert!(
            keycode >= min_kc && keycode <= max_kc,
            "keycode {keycode} outside published range {min_kc}..={max_kc}"
        );
        let (_types, mut off) = parse_key_types(reply); // KeySyms start
        let mut kc = min_kc;
        loop {
            let mut kt_index = [0u8; 4];
            kt_index.copy_from_slice(&reply[off..off + 4]);
            let num_groups = reply[off + 4] & 0x0F;
            let width = reply[off + 5];
            let n_syms = u16::from_le_bytes([reply[off + 6], reply[off + 7]]);
            let sym_base = off + 8;
            if kc == keycode {
                let w = width as usize;
                let g = num_groups as usize;
                let mut group_syms = vec![vec![0u32; w]; g];
                for (grp, row) in group_syms.iter_mut().enumerate() {
                    for (lvl, cell) in row.iter_mut().enumerate() {
                        let s = sym_base + (grp * w + lvl) * 4;
                        *cell = u32::from_le_bytes([
                            reply[s],
                            reply[s + 1],
                            reply[s + 2],
                            reply[s + 3],
                        ]);
                    }
                }
                return ParsedKeySymMap {
                    num_groups,
                    width,
                    n_syms,
                    kt_index,
                    group_syms,
                };
            }
            off = sym_base + n_syms as usize * 4;
            assert!(
                kc < max_kc,
                "keycode {keycode} not found in KeySyms section"
            );
            kc += 1;
        }
    }

    #[test]
    fn get_map_multigroup_serializes_all_groups() {
        // us,de multi-group: keycode 29 (AD06) is `y` (0x79) in group 0 (us),
        // `z` (0x7a) in group 1 (de). Ground truth: cinnamon-xorg.xtrace shows
        // the de `z` keysym once per group in the loaded multi-group map.
        let mut core = crate::kms::core::KmsCore::for_tests();
        core.recompile_keymap(&crate::kms::core::XkbRmlvo {
            rules: "evdev".into(),
            model: "pc105".into(),
            layout: "us,de".into(),
            variant: String::new(),
            options: None,
        });
        let reply = reply_get_map(&core.xkb_keymap.0);
        let n_types = reply[15];
        let entry = parse_keysym_map_for_keycode(&reply, 29);
        assert_eq!(entry.num_groups, 2, "two groups serialized");
        assert_eq!(
            entry.n_syms as usize,
            entry.width as usize * entry.num_groups as usize
        );
        assert!(
            entry
                .kt_index
                .iter()
                .take(entry.num_groups as usize)
                .all(|&t| t < n_types),
            "every ktIndex must be < nTypes ({n_types})"
        );
        assert_eq!(entry.group_syms[0][0], 0x79, "group0 level0 = y");
        assert_eq!(entry.group_syms[1][0], 0x7a, "group1 level0 = z");
    }

    #[test]
    fn get_map_four_level_resolves_altgr_eurosign() {
        // Functional: with the REAL derived key-type table published in
        // GetMap, a `de` client applying the LevelThree real-mod
        // (Mod5 = 0x80) to the `e` key (kc26) must reach level 2 = €
        // (0x20ac). Golden vector: de e-key FOUR_LEVEL maps Shift→1,
        // LevelThree(0x80)→2, Shift+LevelThree(0x81)→3.
        let mut core = crate::kms::core::KmsCore::for_tests();
        core.recompile_keymap(&crate::kms::core::XkbRmlvo {
            rules: "evdev".into(),
            model: "pc105".into(),
            layout: "de".into(),
            variant: String::new(),
            options: None,
        });
        let reply = reply_get_map(&core.xkb_keymap.0);

        // Structural: nTypes (header) == the emitted KeyTypes count, and
        // every per-key ktIndex[group] < nTypes.
        let n_types = reply[15];
        let (types, _) = parse_key_types(&reply);
        assert_eq!(
            types.len(),
            usize::from(n_types),
            "parsed KeyTypes count must equal header nTypes"
        );

        const E_KEY_X11: u8 = 26;
        let e = parse_keysym_map_for_keycode(&reply, E_KEY_X11);
        for g in 0..usize::from(e.num_groups) {
            assert!(
                e.kt_index[g] < n_types,
                "kc26 ktIndex[{g}]={} must be < nTypes={n_types}",
                e.kt_index[g]
            );
        }

        // In single-layout `de` the AltGr layer rides in group 0.
        // Resolve through the PUBLISHED type: apply effective real-mods
        // 0x80 against group-0's type's map entries → matching level,
        // then read that level's keysym for kc26 group 0 → must be €.
        let g0_type = &types[usize::from(e.kt_index[0])];
        let effective_mods: u8 = 0x80; // LevelThree (Mod5)
        // X11 level resolution: pick the entry whose mask is satisfied by
        // the effective mods (mask & effective == mask); highest such
        // wins. Level 0 is the implicit default.
        let mut level = 0u8;
        for (mask, lvl, _real) in &g0_type.entries {
            if *mask != 0 && (effective_mods & *mask) == *mask && *lvl > level {
                level = *lvl;
            }
        }
        assert_eq!(
            level, 2,
            "LevelThree(0x80) must select level 2 via kc26 group0's published type {:?}",
            g0_type.entries
        );
        assert!(
            (level as usize) < e.group_syms[0].len(),
            "resolved level {level} must be within kc26 group0 width {}",
            e.width
        );
        assert_eq!(
            e.group_syms[0][level as usize], 0x20ac,
            "AltGr+e (kc26, group0, level2) must resolve to € (0x20ac)"
        );
    }

    #[test]
    fn use_extension_reply_length() {
        assert_eq!(reply_use_extension().len(), 32);
    }

    #[test]
    fn use_extension_success_flag() {
        let r = reply_use_extension();
        assert_eq!(r[1], 1, "success must be 1");
    }

    #[test]
    fn get_controls_reply_length() {
        let km = test_keymap();
        assert_eq!(reply_get_controls(&km).len(), 92);
    }

    #[test]
    fn modifier_mapping_derived_from_keymap_places_super_on_mod4() {
        // Ground-truth against evdev/pc105/us: Super_L lives on Mod4,
        // not Mod5; Alt on Mod1; Control_L on Control. The bug this
        // guards against is a hand-written table putting Super on the
        // wrong modifier (or omitting it), which makes WM clients
        // resolve Super-shortcuts to a 0 modifier and grab plain keys.
        let km = test_keymap();
        let (kpm, data) = modifier_mapping_from_keymap(&km);
        assert!(kpm >= 1, "at least one keycode per modifier");
        let kpm = usize::from(kpm);
        let row = |idx: usize| &data[idx * kpm..(idx + 1) * kpm];
        let shift = row(0);
        let control = row(2);
        let mod1 = row(3);
        let mod4 = row(6);
        let mod5 = row(7);
        assert!(shift.contains(&50), "Shift_L (50) on Shift row");
        assert!(control.contains(&37), "Control_L (37) on Control row");
        assert!(mod1.contains(&64), "Alt_L (64) on Mod1 row");
        assert!(
            mod4.contains(&133),
            "Super_L (133) must be on Mod4, got mod4={mod4:?} mod5={mod5:?}"
        );
        assert!(
            !mod5.contains(&133),
            "Super_L must NOT be on Mod5 (the old hardcoded-fallback bug)"
        );
    }

    #[test]
    fn get_controls_field_offsets_match_xkbproto() {
        // Field offsets ground-truthed against
        // /usr/include/X11/extensions/XKBproto.h's
        // xkbGetControlsReply struct. xkbcommon's get_controls
        // asserts `numGroups > 0 && numGroups <= 4` (objdump on
        // libxkbcommon-x11.so.0.13.1 shows the test mask 0x05 →
        // unsigned-greater-than-4 reject after the >0 reject).
        let km = test_keymap();
        let r = reply_get_controls(&km);
        assert_eq!(r[0], 1, "reply type");
        assert_eq!(r[1], 1, "deviceID");
        assert_eq!(
            u32::from_le_bytes([r[4], r[5], r[6], r[7]]),
            15,
            "length = (92 - 32) / 4 = 15"
        );
        assert!(r[9] >= 1 && r[9] <= 4, "numGroups in 1..=4");
        // repeatDelay at offset 20..22, repeatInterval at 22..24.
        assert_eq!(u16::from_le_bytes([r[20], r[21]]), 500);
        assert_eq!(u16::from_le_bytes([r[22], r[23]]), 33);
        // enabledCtrls at offset 56..60, with at least RepeatKeys.
        let enabled = u32::from_le_bytes([r[56], r[57], r[58], r[59]]);
        assert_eq!(enabled & 0x01, 0x01, "RepeatKeys bit set");
    }

    #[test]
    fn get_map_reply_invariants() {
        let km = test_keymap();
        let r = reply_get_map(&km);
        // 40-byte fixed reply + body, 4-byte aligned, length matches.
        assert!(r.len() >= 40);
        assert!(r.len().is_multiple_of(4));
        let length_words = u32::from_le_bytes([r[4], r[5], r[6], r[7]]) as usize;
        assert_eq!(length_words * 4 + 32, r.len());

        let min_kc = r[10];
        let max_kc = r[11];
        assert!(min_kc <= max_kc);
        assert!(min_kc >= 8);
        let n_keys = max_kc - min_kc + 1;

        // present advertises every required map part — xkbcommon's
        // get_map_required_components is a subset of 0xDF.
        let present = u16::from_le_bytes([r[12], r[13]]);
        assert_eq!(present & 0xDF, 0xDF);

        // KeyTypes: the derived table — Xlib's XkbAllocClientMap rejects
        // nTypes < XkbNumRequiredTypes (= 4) with BadValue, so the table
        // always carries at least the four seeded required types and may
        // carry more (derived FOUR_LEVEL / KEYPAD types). totalTypes
        // mirrors nTypes, and the parsed KeyTypes count must agree.
        assert!(r[15] >= 4, "nTypes >= XkbNumRequiredTypes");
        assert_eq!(r[16], r[15], "totalTypes == nTypes");
        let (types, _) = parse_key_types(&r);
        assert_eq!(
            types.len(),
            usize::from(r[15]),
            "parsed KeyTypes count == header nTypes"
        );
        // Sanity on the seeded required types' shapes.
        assert_eq!(types[0].num_levels, 1, "type 0 = ONE_LEVEL");
        assert_eq!(types[1].num_levels, 2, "type 1 = TWO_LEVEL");
        assert_eq!(types[1].mask, SHIFT_MASK, "TWO_LEVEL keyed on Shift");

        // KeySyms covers full range, firstKeySym >= minKeyCode and
        // firstKeySym + nKeySyms <= maxKeyCode + 1.
        assert_eq!(r[17], min_kc, "firstKeySym = min_kc");
        assert_eq!(r[20], n_keys, "nKeySyms = full range");

        // KeyActions covers full range, firstKeyAction == min_key_code
        // and firstKeyAction + nKeyActions == max_key_code + 1.
        assert_eq!(r[21], min_kc, "firstKeyAction = min_kc");
        assert_eq!(r[24], n_keys, "nKeyActions = full range");

        // ModifierMap covers full range; totalModMapKeys must be ≤ nModMapKeys.
        assert_eq!(r[31], min_kc, "firstModMapKey = min_kc");
        assert_eq!(r[32], n_keys, "nModMapKeys = full range");
        assert!(r[33] <= r[32], "totalModMapKeys ≤ nModMapKeys");
    }

    #[test]
    fn get_map_request_only_advertises_requested_parts() {
        let km = test_keymap();
        let body = get_map_request_body(XKB_MAP_PART_KEY_TYPES | XKB_MAP_PART_KEY_SYMS, 0);
        let r = reply_get_map_for_request(&km, &body);

        let present = u16::from_le_bytes([r[12], r[13]]);
        assert_eq!(
            present,
            XKB_MAP_PART_KEY_TYPES | XKB_MAP_PART_KEY_SYMS,
            "GetMap present must not include sections absent from full|partial"
        );

        let length_words = u32::from_le_bytes([r[4], r[5], r[6], r[7]]) as usize;
        assert_eq!(length_words * 4 + 32, r.len());
        assert!(r.len().is_multiple_of(4));

        assert!(r[15] >= 4, "requested KeyTypes are present");
        assert_eq!(r[17], r[10], "requested KeySyms firstKeySym = minKeyCode");
        assert_eq!(r[20], r[11] - r[10] + 1, "requested KeySyms cover range");

        assert_eq!(r[21], 0, "unrequested KeyActions first key is zero");
        assert_eq!(
            u16::from_le_bytes([r[22], r[23]]),
            0,
            "unrequested KeyActions totalActions is zero"
        );
        assert_eq!(r[24], 0, "unrequested KeyActions nKeyActions is zero");
        assert_eq!(r[31], 0, "unrequested ModifierMap first key is zero");
        assert_eq!(r[32], 0, "unrequested ModifierMap n keys is zero");
        assert_eq!(r[33], 0, "unrequested ModifierMap total keys is zero");
        assert_eq!(r[34], 0, "unrequested VirtualModMap first key is zero");
        assert_eq!(r[35], 0, "unrequested VirtualModMap n keys is zero");
        assert_eq!(r[36], 0, "unrequested VirtualModMap total keys is zero");
        assert_eq!(
            u16::from_le_bytes([r[38], r[39]]),
            0,
            "unrequested VirtualMods mask is zero"
        );

        let (_types, mut off) = parse_key_types(&r);
        for _ in 0..r[20] {
            let nsyms = u16::from_le_bytes([r[off + 6], r[off + 7]]) as usize;
            off += 8 + nsyms * 4;
        }
        assert_eq!(
            off,
            r.len(),
            "reply body must stop after requested KeyTypes and KeySyms"
        );
    }

    #[test]
    fn get_map_publishes_real_keysyms_for_letter_keys() {
        // The level-0 keysym for a letter key under the default us
        // layout should be the lowercase ASCII codepoint. Walk the
        // KeySyms section looking for the 'a' keysym (0x61) — at
        // least one key should publish it. Pre-fix the section was
        // empty, so this is the regression guard.
        let km = test_keymap();
        let r = reply_get_map(&km);
        let n_keys = r[11] - r[10] + 1;
        // KeyTypes is variable-size (derived table) — walk it to find
        // where KeySyms begins rather than assuming a fixed offset.
        let (_types, mut off) = parse_key_types(&r);
        let mut found_a = false;
        for _ in 0..n_keys {
            // KeySymMap: kt_index[4], groupInfo, width, nSyms, syms[nSyms]
            let width = r[off + 5] as usize;
            let num_groups = (r[off + 4] & 0x0F) as usize;
            let nsyms = u16::from_le_bytes([r[off + 6], r[off + 7]]) as usize;
            assert_eq!(nsyms, width * num_groups, "nSyms = width * num_groups");
            for s in 0..nsyms {
                let sym_off = off + 8 + s * 4;
                let sym = u32::from_le_bytes([
                    r[sym_off],
                    r[sym_off + 1],
                    r[sym_off + 2],
                    r[sym_off + 3],
                ]);
                if sym == b'a' as u32 {
                    found_a = true;
                }
            }
            off += 8 + nsyms * 4;
        }
        assert!(
            found_a,
            "expected level-0 'a' keysym somewhere in the KeySyms section"
        );
    }

    #[test]
    fn modmap_binds_iso_level3_to_mod5_not_mod1() {
        let mut core = crate::kms::core::KmsCore::for_tests();
        core.recompile_keymap(&crate::kms::core::XkbRmlvo {
            rules: "evdev".into(),
            model: "pc105".into(),
            layout: "us,be,us".into(),
            variant: ",,".into(),
            options: Some("lv3:ralt_switch".into()),
        });
        let km = &core.xkb_keymap.0;
        // Find the keycode whose level-0 keysym is ISO_Level3_Shift (0xfe03),
        // assert its real-mod mask is Mod5 (0x80), NOT Mod1 (0x08).
        let mut found = false;
        for kc in 8u32..=255 {
            let k = xkbcommon::xkb::Keycode::new(kc);
            if km.key_get_syms_by_level(k, 0, 0).first().map(|s| s.raw()) == Some(0xfe03) {
                let m = real_mod_mask_for_keycode(km, kc);
                assert_eq!(
                    m & 0x80,
                    0x80,
                    "ISO_Level3_Shift kc{kc} must bind Mod5, got {m:#04x}"
                );
                assert_eq!(m & 0x08, 0x00, "...and NOT Mod1");
                found = true;
            }
        }
        assert!(found, "be keymap must have an ISO_Level3_Shift key");
        // Sanity: Shift still binds Mod1-bit 0x01
        for kc in 8u32..=255 {
            let k = xkbcommon::xkb::Keycode::new(kc);
            if km.key_get_syms_by_level(k, 0, 0).first().map(|s| s.raw()) == Some(0xffe1) {
                assert_eq!(
                    real_mod_mask_for_keycode(km, kc) & 0x01,
                    0x01,
                    "Shift_L -> 0x01"
                );
            }
        }
    }

    #[test]
    fn get_map_modifier_map_includes_shift() {
        // Shift_L / Shift_R are mod_map'd to bit 0 — find at least
        // one entry with mods == 0x01 to prove the modmap walk
        // worked.
        let km = test_keymap();
        let r = reply_get_map(&km);
        let n_keys = r[11] - r[10] + 1;
        let total_modmap = r[33] as usize;

        // Compute the offset to the ModifierMap section.
        // KeyTypes is variable-size (derived table) — walk it.
        let (_types, mut off) = parse_key_types(&r);
        // Walk KeySyms to advance.
        for _ in 0..n_keys {
            let nsyms = u16::from_le_bytes([r[off + 6], r[off + 7]]) as usize;
            off += 8 + nsyms * 4;
        }
        // KeyActions: nk count bytes + pad + totalActs × 8-byte structs.
        let nk = usize::from(n_keys);
        let total_acts = u16::from_le_bytes([r[22], r[23]]) as usize;
        off += nk + ((4 - nk % 4) % 4) + total_acts * 8;
        // VirtualMods: one CARD8 per present vmod, padded to 4 bytes.
        // ExplicitComponents is empty.
        let vmod_count = virtual_mods_from_keymap(&km).present_mask.count_ones() as usize;
        off += vmod_count + ((4 - vmod_count % 4) % 4);

        let mut found_shift = false;
        for i in 0..total_modmap {
            let mods = r[off + 2 * i + 1];
            if mods == 0x01 {
                found_shift = true;
            }
        }
        assert!(
            found_shift,
            "expected at least one Shift modifier-map entry"
        );
    }

    /// GH #59: GetMap must emit real key ACTIONS (SetMods/LockMods on
    /// the modifier keys), not totalActs=0. Without them an
    /// xkbcommon-x11 client (kitty) that starts with a modifier latched
    /// — e.g. launched from a `super + Return` sxhkd chord, so its
    /// XkbGetState seeds Mod4 active — can never clear Mod4 from the
    /// Super KeyRelease, and every key resolves to NoSymbol (no text).
    /// Assert totalActs > 0 and that the Super key carries a SetMods
    /// action for Mod4 (0x40).
    #[test]
    fn get_map_emits_setmods_action_for_super_mod4() {
        let km = test_keymap();
        let r = reply_get_map(&km);
        let n_keys = usize::from(r[11] - r[10] + 1);
        let total_acts = u16::from_le_bytes([r[22], r[23]]) as usize;
        assert!(
            total_acts > 0,
            "GetMap must emit key actions (totalActs=0 → GH #59 dead keyboard)"
        );

        // Navigate to the KeyActions section: header + KeyTypes + KeySyms.
        let (_types, mut off) = parse_key_types(&r);
        for _ in 0..n_keys {
            let nsyms = u16::from_le_bytes([r[off + 6], r[off + 7]]) as usize;
            off += 8 + nsyms * 4;
        }
        // Per-key count bytes + pad → the action structs.
        let acts_off = off + n_keys + ((4 - n_keys % 4) % 4);
        let mut found_super = false;
        for i in 0..total_acts {
            let a = acts_off + i * 8;
            // SetMods (type 1) with mask = Mod4 (0x40).
            if r[a] == 1 && r[a + 2] == 0x40 {
                found_super = true;
            }
        }
        assert!(
            found_super,
            "Super key must carry a SetMods(Mod4=0x40) action so xkbcommon \
             tracks/clears Mod4"
        );
    }

    #[test]
    fn virtual_mods_bind_super_to_mod4_alt_to_mod1() {
        // Ground-truth against evdev/pc105/us: the "Super" virtual
        // modifier must bind to Mod4 (0x40) and "Alt" to Mod1 (0x08),
        // with Super_L (keycode 133) carrying the Super vmod bit in the
        // VirtualModMap. This is the dead-`p` fix: an empty vmod section
        // made mutter resolve <Super> to 0 and grab bare keys.
        let km = test_keymap();
        let vmod = virtual_mods_from_keymap(&km);
        assert!(vmod.present_mask != 0, "at least one vmod present");

        let super_idx = vmod
            .names
            .iter()
            .find(|(_, n)| *n == "Super")
            .map(|(i, _)| *i)
            .expect("Super vmod present");
        assert_eq!(
            vmod.bindings[usize::from(super_idx)],
            0x40,
            "Super must bind to Mod4"
        );

        if let Some((alt_idx, _)) = vmod.names.iter().find(|(_, n)| *n == "Alt") {
            assert_eq!(
                vmod.bindings[usize::from(*alt_idx)],
                0x08,
                "Alt must bind to Mod1"
            );
        }

        // Super_L (keycode 133) maps to the Super vmod bit.
        let super_bit = 1u16 << super_idx;
        assert!(
            vmod.vmodmap
                .iter()
                .any(|(kc, bits)| *kc == 133 && bits & super_bit != 0),
            "Super_L (133) must carry the Super vmod bit; vmodmap={:?}",
            vmod.vmodmap
        );
    }

    #[test]
    fn get_names_emits_super_vmod_name_atom() {
        // VirtualModNames must carry a non-zero atom for each present
        // vmod so a client can match "Super" by name. Verify the
        // interner is invoked with "Super".
        let km = test_keymap();
        let mut seen: Vec<String> = Vec::new();
        let _ = reply_get_names(&km, &test_rmlvo(), &mut |name| {
            seen.push(name.to_owned());
            0x77
        });
        assert!(
            seen.iter().any(|n| n == "Super"),
            "GetNames must intern the Super vmod name; interned={seen:?}"
        );
    }

    #[test]
    fn get_names_advertises_required_bits_with_real_data() {
        let km = test_keymap();
        let r = reply_get_names(&km, &test_rmlvo(), &mut |_| 0xFFu32);
        let which = u32::from_le_bytes([r[8], r[9], r[10], r[11]]);
        // Bits 6 (KeyTypeNames=0x40) | 7 (KTLevelNames=0x80) |
        // 9 (VirtualModNames=0x200) | 11 (KeyNames=0x800) = 0xAC0
        // is what xkbcommon-x11's `get_names_required` enforces
        // (verified via objdump on libxkbcommon-x11.so.0.13.1).
        let required = 0x0000_0AC0_u32;
        assert_eq!(
            which & required,
            required,
            "GetNames which (0x{which:08x}) must contain all required name detail bits"
        );
        // Reply must agree with reply_get_map on the keycode range.
        let map = reply_get_map(&km);
        assert_eq!(r[12], map[10], "GetNames minKeyCode == GetMap minKeyCode");
        assert_eq!(r[13], map[11], "GetNames maxKeyCode == GetMap maxKeyCode");
        let n_keys = map[11] - map[10] + 1;
        assert_eq!(r[18], map[10], "firstKey == min_kc");
        assert_eq!(r[19], n_keys, "nKeys covers full range");
        assert_eq!(r[14], map[15], "GetNames nTypes == GetMap's nTypes");
        assert!(
            r[14] >= 4,
            "nTypes must be at least XkbNumRequiredTypes (4), got {}",
            r[14]
        );
    }

    /// GetNames must agree with GetMap on the type count, and its
    /// nKTLevels field must equal Σ over the derived types of
    /// `num_levels`. Both replies call `key_types_from_keymap` on the
    /// same keymap, so the counts are equal by construction — assert it
    /// explicitly for `us` AND a richer multi-level layout (`de`, whose
    /// AltGr letters add FOUR_LEVEL types) so a divergence in either
    /// reply's size math is caught.
    #[test]
    fn get_names_counts_track_get_map_and_derived_table() {
        for layout in ["us", "de"] {
            let mut core = crate::kms::core::KmsCore::for_tests();
            core.recompile_keymap(&crate::kms::core::XkbRmlvo {
                rules: "evdev".into(),
                model: "pc105".into(),
                layout: layout.into(),
                variant: String::new(),
                options: None,
            });
            let km = &core.xkb_keymap.0;

            let names = reply_get_names(km, &core.xkb_rmlvo, &mut |_| 0xFFu32);
            let map = reply_get_map(km);

            // nTypes byte: GetNames[14] must equal GetMap[15].
            assert_eq!(
                names[14], map[15],
                "{layout}: GetNames nTypes ({}) == GetMap nTypes ({})",
                names[14], map[15]
            );

            // nKTLevels (GetNames[26..28]) == Σ num_levels of the
            // derived type table.
            let n_kt_levels = u16::from_le_bytes([names[26], names[27]]);
            let table = key_types_from_keymap(km);
            let expected_levels: u16 = table.types.iter().map(|t| u16::from(t.num_levels)).sum();
            assert_eq!(
                n_kt_levels, expected_levels,
                "{layout}: nKTLevels ({n_kt_levels}) == Σ num_levels ({expected_levels})"
            );
            assert_eq!(
                usize::from(names[14]),
                table.types.len(),
                "{layout}: nTypes byte == derived table length"
            );
        }
    }

    /// Interner fixture: hands out sequential ids from 100 and
    /// records the order names were first seen.
    fn recording_interner(seen: &mut Vec<String>) -> impl FnMut(&str) -> u32 + '_ {
        move |name: &str| {
            if let Some(pos) = seen.iter().position(|n| n == name) {
                100 + u32::try_from(pos).unwrap()
            } else {
                seen.push(name.to_owned());
                100 + u32::try_from(seen.len() - 1).unwrap()
            }
        }
    }

    #[test]
    fn get_names_advertises_xkbcommon_unconditional_read_bits() {
        // xkbcommon-x11's `get_names()` (keymap.c:1139-1146) reads
        // `list.{keycodesName,symbolsName,typesName,compatName}`
        // unconditionally from a stack-uninitialized struct. The
        // xcb-generated `value_list_unpack` only writes those
        // fields when their bit is set in `which`; absent bits
        // leave stack garbage there, which the client then
        // dispatches as `GetAtomName(garbage)` requests. Advertise
        // bits 0|2|4|5 (= 0x35 = Keycodes|Symbols|Types|Compat)
        // so xcb writes real atoms into the fields.
        let km = test_keymap();
        let mut seen = Vec::new();
        let r = reply_get_names(&km, &test_rmlvo(), &mut recording_interner(&mut seen));
        let which = u32::from_le_bytes([r[8], r[9], r[10], r[11]]);
        let unconditionally_read = 0x0000_0035_u32;
        assert_eq!(
            which & unconditionally_read,
            unconditionally_read,
            "GetNames which (0x{which:08x}) must include Keycodes|Symbols|Types|Compat so \
             xcb unpacks real atoms into the struct fields xkbcommon-x11 reads unconditionally"
        );
        // The four unconditional ATOMs sit at offsets 32..48 in
        // bit order (Keycodes, Symbols, Types, Compat — Geometry
        // and PhysSymbols bits are not advertised, so xcb skips
        // those slots). Each must be the interned atom of the
        // resolved KcCGST component for the RMLVO the server
        // compiles (evdev/pc105/us — core.rs `KmsCore::new`):
        // `setxkbmap -print -rules evdev -model pc105 -layout us`.
        // Plain-libX11 clients (xdotool, e16) XGetAtomName every
        // one of these — atom 0 → BadAtom → exit (the vng guest
        // blocker), so they must be real interned atoms.
        let expected = [
            "evdev+aliases(qwerty)", // keycodesName
            "pc+us+inet(evdev)",     // symbolsName
            "complete",              // typesName
            "complete",              // compatName
        ];
        let mut check = recording_interner(&mut seen);
        for (i, name) in expected.iter().enumerate() {
            let off = 32 + i * 4;
            let atom = u32::from_le_bytes([r[off], r[off + 1], r[off + 2], r[off + 3]]);
            assert_ne!(
                atom, 0,
                "unconditional name atom at offset {off} must not be 0"
            );
            assert_eq!(
                atom,
                check(name),
                "atom at offset {off} must be the interned id of {name:?}"
            );
        }
    }

    #[test]
    fn get_names_interns_type_and_level_name_atoms() {
        // typeNames[nTypes] start at offset 48 (after the 32-byte
        // header + 16 bytes of unconditional names). The seeded
        // indices 0..=3 carry the canonical XKB names ONE_LEVEL /
        // TWO_LEVEL / ALPHABETIC / KEYPAD. The KTLevelNames section
        // follows the (padded) nLevelsPerType[nTypes] array; its level
        // ATOMs are the canonical shift-level names per derived type.
        // None may be 0 — libX11 XGetAtomName(0)s otherwise.
        let km = test_keymap();
        let mut seen = Vec::new();
        let r = reply_get_names(&km, &test_rmlvo(), &mut recording_interner(&mut seen));
        let mut check = recording_interner(&mut seen);

        let table = key_types_from_keymap(&km);
        let n_types = table.types.len();
        assert_eq!(usize::from(r[14]), n_types, "nTypes byte == table length");

        // Seeded type names at the canonical indices.
        let seeded = ["ONE_LEVEL", "TWO_LEVEL", "ALPHABETIC", "KEYPAD"];
        for (i, name) in seeded.iter().enumerate() {
            let off = 48 + i * 4;
            let atom = u32::from_le_bytes([r[off], r[off + 1], r[off + 2], r[off + 3]]);
            assert_eq!(
                atom,
                check(name),
                "typeNames[{i}] must be the interned id of {name:?}"
            );
        }
        // EVERY type slot must intern a real, non-zero atom.
        for i in 0..n_types {
            let off = 48 + i * 4;
            let atom = u32::from_le_bytes([r[off], r[off + 1], r[off + 2], r[off + 3]]);
            assert_ne!(atom, 0, "typeNames[{i}] must be a non-zero atom");
        }

        // nLevelsPerType[nTypes] sits right after the type names; its
        // bytes equal each derived type's num_levels.
        let nlpt_off = 48 + n_types * 4;
        for (i, t) in table.types.iter().enumerate() {
            assert_eq!(
                r[nlpt_off + i],
                t.num_levels,
                "nLevelsPerType[{i}] == derived num_levels"
            );
        }
        // ktLevelNames follow the (padded) nLevelsPerType array.
        let pad = (4 - n_types % 4) % 4;
        let lvl_off = nlpt_off + n_types + pad;
        let mut k = 0usize;
        for t in &table.types {
            for level in 0..t.num_levels {
                let off = lvl_off + k * 4;
                let atom = u32::from_le_bytes([r[off], r[off + 1], r[off + 2], r[off + 3]]);
                let expected = match level {
                    0 => "Base".to_owned(),
                    1 => "Shift".to_owned(),
                    2 => "Alt Base".to_owned(),
                    3 => "Shift Alt".to_owned(),
                    n => format!("Level{}", u16::from(n) + 1),
                };
                assert_eq!(
                    atom,
                    check(&expected),
                    "ktLevelNames[{k}] (type level {level}) must be {expected:?}"
                );
                k += 1;
            }
        }
    }

    #[test]
    fn get_names_emits_real_key_names_from_keymap() {
        // KeyNames are char[4] slots (not atoms). They must carry
        // the keymap's canonical key names (xkb_keymap_key_get_name)
        // zero-padded/truncated to 4 bytes — real state-derived
        // data, not anonymous zeros (no-protocol-stubs rule).
        let km = test_keymap();
        let mut seen = Vec::new();
        let r = reply_get_names(&km, &test_rmlvo(), &mut recording_interner(&mut seen));
        let min_kc = usize::from(r[12]);
        let n_keys = usize::from(r[19]);
        let vmod_count = virtual_mods_from_keymap(&km).present_mask.count_ones() as usize;
        // 32 header + 16 unconditional + typeNames[nTypes]*4 +
        // (nLevelsPerType[nTypes] padded to 4 + ktLevelNames[ΣnumLevels]*4)
        // + vmodNames. Sizes track the derived type table (C2b).
        let table = key_types_from_keymap(&km);
        let n_types = table.types.len();
        let sum_levels: usize = table.types.iter().map(|t| usize::from(t.num_levels)).sum();
        let pad = (4 - n_types % 4) % 4;
        let kt_levels_bytes = n_types + pad + sum_levels * 4;
        let key_names_off = 32 + 16 + n_types * 4 + kt_levels_bytes + vmod_count * 4;

        // Spot-check a stable anchor: X keycode 9 is ESC in the
        // evdev keycode set.
        let esc = key_names_off + (9 - min_kc) * 4;
        assert_eq!(
            &r[esc..esc + 4],
            b"ESC\0",
            "X keycode 9 must be named ESC (evdev keycodes)"
        );

        // Every key's wire name must match the keymap's own name.
        for i in 0..n_keys {
            let kc = u32::try_from(min_kc + i).unwrap();
            let name = km.key_get_name(Keycode::new(kc)).unwrap_or("");
            let mut expected = [0u8; 4];
            for (j, b) in name.bytes().take(4).enumerate() {
                expected[j] = b;
            }
            let off = key_names_off + i * 4;
            assert_eq!(
                &r[off..off + 4],
                &expected,
                "key name for X keycode {kc} (keymap says {name:?})"
            );
        }
    }

    #[test]
    fn get_names_symbols_reflects_layout() {
        let mut core = crate::kms::core::KmsCore::for_tests();
        core.recompile_keymap(&crate::kms::core::XkbRmlvo {
            rules: "evdev".into(),
            model: "pc105".into(),
            layout: "de".into(),
            variant: String::new(),
            options: None,
        });
        let mut interned: Vec<String> = Vec::new();
        let _ = reply_get_names(&core.xkb_keymap.0, &core.xkb_rmlvo, &mut |s| {
            interned.push(s.to_string());
            1
        });
        assert!(
            interned.iter().any(|s| s == "pc+de+inet(evdev)"),
            "symbolsName must reflect the active layout, got {interned:?}"
        );
    }

    #[test]
    fn get_compat_map_is_non_empty_with_group_compat() {
        // Regression: an EMPTY CompatMap (length=0) makes libX11's
        // _XkbReadGetCompatMapReply call _XkbInitReadBuffer(0), which returns
        // FALSE for size<=0 → BadAlloc → XkbGetKeyboardByName NULL → setxkbmap
        // "Error loading new keyboard description" (confirmed by gdb backtrace
        // through applyComponentNames). The reply MUST carry a non-zero body.
        let r = reply_get_compat_map();
        // 32-byte header + 4 group-compat xkbModsWireDesc (4 bytes each).
        assert_eq!(r.len(), 48, "header + 16-byte group-compat body");
        let length = u32::from_le_bytes([r[4], r[5], r[6], r[7]]);
        assert_eq!(length, 4, "length MUST be > 0 (libX11 BadAlloc otherwise)");
        assert_eq!(r[8], 0x0f, "groupsRtrn = all 4 keyboard groups");
        assert_eq!(
            u16::from_le_bytes([r[12], r[13]]),
            0,
            "nSIRtrn = 0 (deferred)"
        );
        // group-compat body matches the captured Xorg reply: group 1 none,
        // groups 2-4 → Mod5 (0x80).
        assert_eq!(
            &r[32..48],
            &[
                0x00, 0x00, 0x00, 0x00, 0x80, 0x80, 0x00, 0x00, 0x80, 0x80, 0x00, 0x00, 0x80, 0x80,
                0x00, 0x00,
            ],
            "group-compat ModsRec (golden vector)"
        );
    }

    #[test]
    fn convert_gbn_matches_captured_request() {
        // The captured Cinnamon request: need=0x00bf, want=0x00ff.
        // Xorg computes reported = XkbConvertGetByNameComponents(FALSE,
        // fwant|fneed) = 0x00ff (all 8 GBN bits incl. OtherNames). The reply
        // header in cinnamon-xorg.xtrace:6202 carries reported=0x00ff.
        assert_eq!(convert_gbn_components(0x00ff | 0x00bf), 0x00ff);
        // Symbols alone expands to BOTH client+server symbol bits plus
        // OtherNames (orig != 0), and nothing else.
        let only_symbols = GBN_CLIENT_SYMBOLS;
        assert_eq!(
            convert_gbn_components(only_symbols),
            GBN_CLIENT_SYMBOLS | GBN_SERVER_SYMBOLS | GBN_OTHER_NAMES
        );
        // Empty request -> empty (no OtherNames either).
        assert_eq!(convert_gbn_components(0), 0);
    }

    #[test]
    fn get_kbd_by_name_reply_header_grounded_in_capture() {
        // Header fields must match the captured working Xorg reply
        // (cinnamon-xorg.xtrace:6202): loaded=1 (BOOL), found=0x7f (MASK,
        // not a bool), reported=0xff (MASK), min/max from the keymap. found
        // and reported are DIFFERENT masks (found omits the OtherNames
        // pseudo-bit), and both differ from the loaded BOOL.
        let km = test_keymap();
        let r = reply_get_kbd_by_name(&km, &test_rmlvo(), 0x00ff, 0x00bf, true, &mut |_| 1u32);

        assert_eq!(r[0], 1, "type = Reply");
        assert_eq!(r[1], 1, "deviceID");
        // min/max from the keymap, clamped into [8,255].
        let min_kc = u8::try_from(km.min_keycode().raw()).unwrap_or(8).max(8);
        let max_kc = u8::try_from(km.max_keycode().raw().min(255))
            .unwrap_or(255)
            .max(min_kc);
        assert_eq!(r[8], min_kc, "minKeyCode @8");
        assert_eq!(r[9], max_kc, "maxKeyCode @9");
        assert_eq!(r[10], 1, "loaded = TRUE (BOOL) @10");
        assert_eq!(r[11], 0, "newKeyboard = FALSE @11");
        assert_eq!(
            u16::from_le_bytes([r[12], r[13]]),
            0x007f,
            "found = 0x7f (located components MASK, captured value) @12"
        );
        assert_eq!(
            u16::from_le_bytes([r[14], r[15]]),
            0x00ff,
            "reported = 0xff (embedded components MASK, captured value) @14"
        );
        // length is in 4-byte units of the trailing body; header is 32 bytes.
        let length_words = u32::from_le_bytes([r[4], r[5], r[6], r[7]]);
        assert_eq!(
            length_words as usize * 4,
            r.len() - 32,
            "length field counts the trailing nested-block bytes"
        );
    }

    #[test]
    fn get_kbd_by_name_first_block_is_40_byte_header_get_map() {
        // The first embedded block (reported includes Types|Symbols) is a
        // FULL xkbGetMapReply: a 40-byte header (sz_xkbGetMapReply), NOT a
        // 32-byte header and NOT a bare section body. Cross-check: in the
        // captured reply the first embedded block at body offset 0 starts
        // with type=0x01 deviceID then a present mask at offset +12 — same
        // shape reply_get_map produces standalone.
        let km = test_keymap();
        let r = reply_get_kbd_by_name(&km, &test_rmlvo(), 0x00ff, 0x00bf, true, &mut |_| 1u32);
        let block = &r[32..];
        assert_eq!(block[0], 1, "embedded GetMap block type = Reply");
        assert_eq!(block[1], 1, "embedded GetMap block deviceID");
        // The embedded GetMap reply equals reply_get_map verbatim (full
        // header+body), proving the 40-byte-header framing.
        let standalone = reply_get_map(&km);
        assert_eq!(
            &block[..standalone.len()],
            &standalone[..],
            "first embedded block is the full xkbGetMapReply (40-byte header)"
        );
        // The standalone GetMap reply's length field implies a >= 40-byte
        // block (header 32 + body, with at least the 8 extra header bytes
        // that make sz_xkbGetMapReply 40), confirming it is not a 32-byte
        // header reply.
        let map_len_words =
            u32::from_le_bytes([standalone[4], standalone[5], standalone[6], standalone[7]]);
        assert!(
            standalone.len() == 32 + map_len_words as usize * 4,
            "GetMap block length self-consistent"
        );
        assert!(
            standalone.len() >= 40,
            "GetMap block carries the 40-byte sz_xkbGetMapReply header"
        );
    }

    #[test]
    fn get_kbd_by_name_load_failed_clears_found() {
        // A failed load -> loaded=0, found=0 (nothing located), but the
        // reply is still well-formed. reported still reflects the request
        // (we always frame the blocks we embed).
        let km = test_keymap();
        let r = reply_get_kbd_by_name(&km, &test_rmlvo(), 0x00ff, 0x00bf, false, &mut |_| 1u32);
        assert_eq!(r[10], 0, "loaded = FALSE");
        assert_eq!(
            u16::from_le_bytes([r[12], r[13]]),
            0,
            "found = 0 on failed load"
        );
    }

    #[test]
    fn get_device_info_reply_matches_xcb_struct_size() {
        // `sizeof(xcb_xkb_get_device_info_reply_t)` is 36 — verified
        // via gcc on `xcb/xkb.h`. A 32-byte reply makes xcb-based
        // clients (xkbcommon-x11) read past the allocation and pick
        // up uninit heap as `nameLen`/atoms; the fix is to publish
        // the full 36 bytes with `length = 1` and `nameLen = 0`.
        let r = reply_get_device_info();
        assert_eq!(
            r.len(),
            36,
            "matches sizeof(xcb_xkb_get_device_info_reply_t)"
        );
        assert_eq!(r[0], 1, "reply type");
        assert_eq!(r[1], 1, "deviceID");
        assert_eq!(
            u32::from_le_bytes([r[4], r[5], r[6], r[7]]),
            1,
            "length = (36 - 32) / 4 = 1"
        );
        assert_eq!(
            u16::from_le_bytes([r[32], r[33]]),
            0,
            "nameLen = 0 (no name follows)"
        );
    }

    #[test]
    fn per_client_flags_reports_supported_and_requested_flags() {
        let mut body = vec![0u8; 24];
        body[4..8].copy_from_slice(&1u32.to_le_bytes()); // change DetectableAutoRepeat
        body[8..12].copy_from_slice(&1u32.to_le_bytes()); // value DetectableAutoRepeat

        let r = reply_per_client_flags(&body);
        assert_eq!(r.len(), 32);
        assert_eq!(r[0], 1, "reply type");
        assert_eq!(r[1], 1, "deviceID");
        assert_eq!(u32::from_le_bytes(r[4..8].try_into().unwrap()), 0);
        assert_eq!(
            u32::from_le_bytes(r[8..12].try_into().unwrap()),
            0x1f,
            "supported = XkbPCF_AllFlagsMask"
        );
        assert_eq!(
            u32::from_le_bytes(r[12..16].try_into().unwrap()),
            1,
            "value reflects changed supported flags"
        );
        assert_eq!(u32::from_le_bytes(r[16..20].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(r[20..24].try_into().unwrap()), 0);
    }

    /// A plain `us` keymap (single layout) for state tests.
    fn us_keymap() -> xkbcommon::xkb::Keymap {
        let ctx = xkbcommon::xkb::Context::new(xkbcommon::xkb::CONTEXT_NO_FLAGS);
        xkbcommon::xkb::Keymap::new_from_names(
            &ctx,
            "evdev",
            "pc105",
            "us",
            "",
            None,
            xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .expect("us keymap")
    }

    /// GetState golden (trace 4271/10952): a steady group-0/no-lock state
    /// is an all-zero reply — type/deviceID at [0]/[1], length 0, every
    /// data byte ([8..32]) zero.
    #[test]
    fn get_state_steady_group0_matches_all_zero_golden() {
        let km = us_keymap();
        let state = xkbcommon::xkb::State::new(&km);
        let r = reply_get_state(&state, 0);

        assert_eq!(r.len(), 32, "xkbGetStateReply is 32 bytes");
        let mut want = vec![0u8; 32];
        want[0] = 1; // reply type
        want[1] = 1; // deviceID
        assert_eq!(r, want, "steady group-0/no-lock state must be all-zero");
        // length word and all data bytes zero (redundant with the byte-match
        // above, but states the invariant the golden encodes).
        assert_eq!(u32::from_le_bytes(r[4..8].try_into().unwrap()), 0);
        assert!(r[8..32].iter().all(|&b| b == 0));
    }

    /// A locked-group-1 state must report `group@12 == 1` and
    /// `lockedGroup@13 == 1` (effective == locked; yserver has no
    /// base/latched group).
    #[test]
    fn get_state_group1_reports_group_and_locked_group() {
        let km = us_de_keymap();
        let mut state = xkbcommon::xkb::State::new(&km);
        // Lock layout (group) 1 — second group of the us,de keymap.
        state.update_mask(0, 0, 0, 0, 0, 1);
        let r = reply_get_state(&state, 1);
        assert_eq!(r[12], 1, "group (effective) == locked_group");
        assert_eq!(r[13], 1, "lockedGroup == locked_group");
    }

    /// A Caps-locked state must set the Lock bit (0x02) in `lockedMods@11`.
    #[test]
    fn get_state_caps_locked_sets_locked_lock_bit() {
        let km = us_keymap();
        let mut state = xkbcommon::xkb::State::new(&km);
        // Lock the Lock (Caps) real modifier — bit 0x02.
        state.update_mask(0, 0, 0x02, 0, 0, 0);
        let r = reply_get_state(&state, 0);
        assert_ne!(r[11] & 0x02, 0, "lockedMods carries the Lock bit");
    }

    /// Build a `GetNamedIndicator` request body for `requested_atom`.
    /// Body layout (after the 4-byte XKB header the core loop strips):
    /// `deviceSpec(2) ledClass(2) ledID(2) pad1(2) indicator:Atom(4)`.
    fn named_indicator_body(requested_atom: u32) -> Vec<u8> {
        let mut body = vec![0u8; 12];
        body[8..12].copy_from_slice(&requested_atom.to_le_bytes());
        body
    }

    /// A deterministic atom interner: stable name→id mapping (ids start at
    /// 1 so 0 stays "no atom"). Mirrors the server's intern_atom contract.
    fn make_interner() -> impl FnMut(&str) -> u32 {
        let mut map: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
        let mut next = 1u32;
        move |name: &str| {
            if let Some(id) = map.get(name) {
                *id
            } else {
                let id = next;
                next += 1;
                map.insert(name.to_owned(), id);
                id
            }
        }
    }

    /// GetNamedIndicator golden (trace 87810/87812): the map fields match
    /// the IndicatorMap golden's Num Lock (slot 1) / Caps Lock (slot 0).
    /// `on` is asserted against the State we build (no locks → on=0).
    #[test]
    fn get_named_indicator_matches_golden() {
        let km = us_de_keymap();
        let state = xkbcommon::xkb::State::new(&km); // no locks
        let mut intern = make_interner();

        // Resolve the atoms the way the server will (same interner).
        let num_atom = intern("Num Lock");
        let caps_atom = intern("Caps Lock");

        // -- Num Lock ------------------------------------------------
        let r =
            reply_get_named_indicator(&km, &state, &named_indicator_body(num_atom), &mut intern);
        assert_eq!(r.len(), 32, "xkbGetNamedIndicatorReply is 32 bytes");
        assert_eq!(r[0], 1, "reply type");
        assert_eq!(r[1], 1, "deviceID");
        assert_eq!(
            u32::from_le_bytes(r[8..12].try_into().unwrap()),
            num_atom,
            "indicator atom echoed"
        );
        assert_eq!(r[12], 1, "found");
        assert_eq!(r[13], 0, "on (no locks at capture)");
        assert_eq!(r[14], 1, "realIndicator (slot 1 < 11)");
        assert_eq!(r[15], 1, "ndx == slot 1");
        assert_eq!(r[16], 0x80, "flags = NoExplicit");
        assert_eq!(r[17], 0, "whichGroups");
        assert_eq!(r[18], 0, "groups");
        assert_eq!(r[19], 0x04, "whichMods = UseLocked");
        assert_eq!(r[20], 0x10, "mods = Mod2 (NumLock binding)");
        assert_eq!(r[21], 0x00, "realMods");
        // virtualMods is yserver's own vmod index (principled divergence,
        // same as the IndicatorMap golden test): NumLock→idx 1 → 0x0002.
        assert_eq!(
            u16::from_le_bytes(r[22..24].try_into().unwrap()),
            0x0002,
            "virtualMods (yserver vmod index for NumLock)"
        );
        assert_eq!(
            u32::from_le_bytes(r[24..28].try_into().unwrap()),
            0,
            "ctrls"
        );
        assert_eq!(r[28], 1, "supported");

        // -- Caps Lock -----------------------------------------------
        let r =
            reply_get_named_indicator(&km, &state, &named_indicator_body(caps_atom), &mut intern);
        assert_eq!(r[12], 1, "found");
        assert_eq!(r[13], 0, "on (no locks)");
        assert_eq!(r[14], 1, "realIndicator (slot 0 < 11)");
        assert_eq!(r[15], 0, "ndx == slot 0");
        assert_eq!(r[16], 0x80, "flags = NoExplicit");
        assert_eq!(r[19], 0x04, "whichMods = UseLocked");
        assert_eq!(r[20], 0x02, "mods = Lock");
        assert_eq!(r[21], 0x02, "realMods = Lock");
        assert_eq!(
            u16::from_le_bytes(r[22..24].try_into().unwrap()),
            0x0000,
            "virtualMods (Caps Lock is a real mod, no vmod)"
        );
        assert_eq!(r[28], 1, "supported");
    }

    /// `on` reflects the live State: with Caps locked, Caps Lock's `on`=1.
    #[test]
    fn get_named_indicator_on_reflects_state() {
        let km = us_de_keymap();
        let mut state = xkbcommon::xkb::State::new(&km);
        state.update_mask(0, 0, 0x02, 0, 0, 0); // lock Caps
        let mut intern = make_interner();
        let caps_atom = intern("Caps Lock");
        let r =
            reply_get_named_indicator(&km, &state, &named_indicator_body(caps_atom), &mut intern);
        assert_eq!(r[12], 1, "found");
        assert_eq!(r[13], 1, "on = Caps Lock active");
    }

    /// An unknown atom → found=0, supported=1, every map field 0.
    #[test]
    fn get_named_indicator_unknown_atom_not_found() {
        let km = us_de_keymap();
        let state = xkbcommon::xkb::State::new(&km);
        let mut intern = make_interner();
        // An atom that no indicator name will ever intern to.
        let bogus = 0xDEAD_BEEF;
        let r = reply_get_named_indicator(&km, &state, &named_indicator_body(bogus), &mut intern);
        assert_eq!(
            u32::from_le_bytes(r[8..12].try_into().unwrap()),
            bogus,
            "atom echoed"
        );
        assert_eq!(r[12], 0, "found = 0");
        assert_eq!(r[28], 1, "supported = 1");
        // ndx/flags/mods etc. all zero.
        assert!(r[13..28].iter().all(|&b| b == 0), "map fields zero");
    }
}
