//! Seeding the model from xkbcommon's compile of a keymap loaded by name
//! (startup, `setxkbmap`/GetKbdByName, `_XKB_RULES_NAMES`).
//!
//! What Xorg holds after such a load is xkbcomp's compile of the same
//! components plus the server's `XkbUpdateDescActions` over every key
//! (`ProcXkbGetKbdByName`, `XkbInitDevice`). xkbcommon compiles the same
//! xkeyboard-config sources but exposes little of the result, so the model
//! is read from its V1 dump ([`super::text`]) and its API:
//!
//! - types in Xorg order (the four required first, then dump order), with
//!   xkbcomp's own normalisation of their map entries and preserve values
//!   ([`xkbcomp_type`]);
//! - keysyms, groups and levels from the xkbcommon API; each group's type
//!   from the dump's `type=`, else xkbcomp's automatic type rule;
//! - explicit bits as xkbcomp sets them (named types, >2-level and
//!   alphabetic automatic types, `actions[]`, `repeat=`, `virtualMods=`);
//! - compat interprets and indicator maps from the dump, in dump order;
//! - key actions, vmodmap, behaviors and per-key repeat from Xorg's
//!   `XkbUpdateDescActions` over the whole keycode range, run on the model.

use std::collections::HashMap;

use xkbcommon::xkb::{self, Keycode, Keymap};

use super::{
    Action, CLAMP_INTO_RANGE, EXPLICIT_AUTO_REPEAT, EXPLICIT_INTERPRET, EXPLICIT_VMODMAP,
    IndicatorMap, KeySyms, KeyType, KtEntry, Mods, NO_MODIFIER, NUM_GROUPS, NUM_INDICATORS,
    NUM_VMODS, REDIRECT_INTO_RANGE, REQUIRED_TYPE_NAMES, SI_ALL_OF, SI_ANY_OF, SI_ANY_OF_OR_NONE,
    SI_AUTO_REPEAT, SI_EXACTLY, SI_LEVEL_ONE_ONLY, SI_LOCKING_KEY, SI_NONE_OF, SymInterpret,
    XkbChanges, XkbDesc, action,
    text::{self, DumpError},
};

/// `XkbIM_*` state-component bits of an indicator map.
const IM_USE_BASE: u8 = 1;
const IM_USE_LATCHED: u8 = 2;
const IM_USE_LOCKED: u8 = 4;
const IM_USE_EFFECTIVE: u8 = 8;
const IM_USE_COMPAT: u8 = 16;
/// `XkbIM_NoExplicit` / `XkbIM_LEDDrivesKB`.
const IM_NO_EXPLICIT: u8 = 0x80;
const IM_LED_DRIVES_KB: u8 = 0x20;

/// The evdev keycodes' physical indicators (1–11; 12+ are `virtual`), which
/// xkbcommon's dump doesn't distinguish (golden-verified `realIndicators`).
const EVDEV_PHYS_INDICATORS: u32 = 0x0000_07ff;

/// The group compat maps of xkeyboard-config's `complete` compat (group 1
/// none, groups 2–4 Mod5); xkbcommon ignores `group N=` and doesn't dump it.
const GROUP_COMPAT: [(u8, u8); 4] = [(0x00, 0x00), (0x80, 0x80), (0x80, 0x80), (0x80, 0x80)];

/// The virtual modifiers of `keymap` (Xorg index order: xkbcommon's
/// declaration order, `mod_get_index(name) - 8`) and the real mapping
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

/// One `map[]` entry of a dump type: `(real, vmods, level, preserve)`.
type DumpEntry = (u8, u16, u8, Option<(u8, u16)>);

/// xkbcomp's view of a type: its entries `(real, vmods, level)`, the
/// preserve per entry (if any) and the level count.
type XkbcompType = (Vec<(u8, u16, u8)>, Option<Vec<(u8, u16)>>, u8);

/// A key type as the dump writes it.
#[derive(Debug, Default)]
struct DumpType {
    name: String,
    real: u8,
    vmods: u16,
    /// `(real, vmods, level, preserve)` in dump order, level 0-based.
    entries: Vec<DumpEntry>,
    /// `(level, name)`, level 0-based.
    level_names: Vec<(usize, String)>,
}

/// What a key's dump entry says explicitly.
#[derive(Debug, Default)]
struct DumpKey {
    types: [Option<String>; NUM_GROUPS],
    actions: [Option<Vec<String>>; NUM_GROUPS],
    vmods: Option<String>,
    repeat: Option<bool>,
    /// `group_info` out-of-range bits.
    range: u8,
}

fn is_true(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "true" | "yes" | "on"
    )
}

/// A keysym as the dump writes it (name or hex); `None` for `Any`.
fn dump_keysym(s: &str) -> Option<u32> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("Any") || s.eq_ignore_ascii_case("NoSymbol") {
        return None;
    }
    Some(xkb::keysym_from_name(s, xkb::KEYSYM_NO_FLAGS).raw())
}

/// A key name as four wire bytes.
fn key_name_bytes(name: &str) -> [u8; 4] {
    let mut out = [0u8; 4];
    for (i, b) in name.bytes().take(4).enumerate() {
        out[i] = b;
    }
    out
}

/// xkbcommon escapes a component name for its section header, `+` and `:`
/// becoming `_` (`pc_us_ru_2_inet(evdev)_group(alt_shift_toggle)`). Xorg
/// reports the component strings themselves (`pc+us+ru:2+inet(evdev)+…`).
/// Undo it outside parentheses: a digits-only piece is a `:N` group index,
/// every other `_` a `+`. (Layout and file names in xkeyboard-config carry
/// no underscores outside a variant's parentheses.)
pub(crate) fn unescape_component(name: &str) -> String {
    let mut pieces: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut depth = 0usize;
    for c in name.chars() {
        match c {
            '(' => {
                depth += 1;
                cur.push(c);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                cur.push(c);
            }
            '_' if depth == 0 => pieces.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    pieces.push(cur);
    let mut out = String::new();
    for p in pieces {
        if !out.is_empty() && !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) {
            out.push(':');
            out.push_str(&p);
        } else {
            if !out.is_empty() {
                out.push('+');
            }
            out.push_str(&p);
        }
    }
    out
}

/// The name in `xkb_<keyword> "name" {`; none when xkbcommon didn't keep
/// one (libxkbcommon 1.6 writes `(unnamed)` for a keymap compiled from
/// RMLVO).
fn section_name(dump: &str, keyword: &str) -> Option<String> {
    dump.lines()
        .find(|l| {
            l.strip_prefix(keyword)
                .is_some_and(|r| r.starts_with(|c: char| c.is_whitespace() || c == '"'))
        })
        .and_then(text::quoted)
        .filter(|n| !n.is_empty() && n != "(unnamed)")
}

/// The component names rules resolve an RMLVO to, as far as the RMLVO
/// says: what GetNames reports when xkbcommon kept no section names. The
/// symbols name leaves out the options' partials.
pub(crate) fn component_names_from_rmlvo(rmlvo: &crate::kms::core::XkbRmlvo) -> [String; 4] {
    let layouts: Vec<&str> = rmlvo.layout.split(',').collect();
    let variants: Vec<&str> = rmlvo.variant.split(',').collect();
    let segs: Vec<String> = layouts
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let v = match variants.get(i) {
                Some(v) if !v.is_empty() => format!("{l}({v})"),
                _ => (*l).to_owned(),
            };
            if i == 0 { v } else { format!("{v}:{}", i + 1) }
        })
        .collect();
    [
        "evdev+aliases(qwerty)".to_owned(),
        format!("pc+{}+inet(evdev)", segs.join("+")),
        "complete".to_owned(),
        "complete".to_owned(),
    ]
}

/// xkbcomp's type normalisation (keytypes.c) applied to a dump type:
///
/// - `DeleteLevel1MapEntries` drops the entries that map to level 1 — with
///   its loop bug: after removing an entry it skips the one that moved into
///   its place, so of two level-1 entries in a row the second survives.
///   xkbcommon's dump leaves out the sources' leading `map[None]= Level1`,
///   which is what makes the first real entry the skipped one; it's put
///   back here.
/// - `CopyDefToKeyType` then re-adds, in `preserve[]` definition order, a
///   level-1 entry for each preserve whose entry is gone (at the end), and
///   fills the preserve array.
/// - `SetPreserve` takes the preserve value's virtual modifiers from the
///   wrong bits (`uval >> 16` instead of `>> 8`), so they're always lost;
///   only the real modifiers survive.
///
/// The dump keeps preserves in entry order, not definition order: two
/// re-added level-1 entries (CTRL+ALT's Control and Alt) can come out in
/// the other order (the listed seed tolerance of the pristine test).
fn xkbcomp_type(dt: &DumpType) -> XkbcompType {
    // The sources' leading `map[None]= Level1`, unless the dump kept it.
    let mut entries: Vec<(u8, u16, u8)> = Vec::new();
    if !dt.entries.iter().any(|e| e.0 == 0 && e.1 == 0) {
        entries.push((0, 0, 0));
    }
    // A trailing run of level-1 entries that only carry a preserve are the
    // ones xkbcommon's `AddPreserve` created for a `preserve[]` without a
    // map entry: not map entries to xkbcomp, which adds them back itself.
    let map_len = dt.entries.len()
        - dt.entries
            .iter()
            .rev()
            .take_while(|e| e.2 == 0 && e.3.is_some())
            .count();
    entries.extend(dt.entries[..map_len].iter().map(|&(r, v, l, _)| (r, v, l)));
    let num_levels = dt
        .entries
        .iter()
        .map(|e| e.2 + 1)
        .chain(
            dt.level_names
                .iter()
                .map(|(l, _)| u8::try_from(l + 1).unwrap_or(u8::MAX)),
        )
        .max()
        .unwrap_or(1)
        .max(1);
    let mut i = 0;
    while i < entries.len() {
        if entries[i].2 == 0 {
            entries.remove(i);
        }
        i += 1;
    }
    let preserves: Vec<(u8, u16, u8, u16)> = dt
        .entries
        .iter()
        .filter_map(|&(r, v, _, p)| p.map(|(pr, pv)| (r, v, pr, pv)))
        .collect();
    if preserves.is_empty() {
        return (entries, None, num_levels);
    }
    let mut pre_at: Vec<(usize, u8, u16)> = Vec::new();
    for &(ir, iv, pr, pv) in &preserves {
        let idx = match entries.iter().position(|e| e.0 == ir && e.1 == iv) {
            Some(i) => i,
            None => {
                entries.push((ir, iv, 0));
                entries.len() - 1
            }
        };
        pre_at.push((idx, pr & ir, (pv >> 8) & iv));
    }
    let mut preserve = vec![(0u8, 0u16); entries.len()];
    for (idx, r, v) in pre_at {
        preserve[idx] = (r, v);
    }
    (entries, Some(preserve), num_levels)
}

/// The canonical key type for a required slot the dump lacks (XKB
/// protocol §15.2): ONE_LEVEL, TWO_LEVEL (Shift), ALPHABETIC (Shift, Lock),
/// KEYPAD (Shift, NumLock).
fn canonical_type(index: usize, numlock_vmod: u16) -> DumpType {
    let mut t = DumpType {
        name: REQUIRED_TYPE_NAMES[index].to_owned(),
        ..DumpType::default()
    };
    match index {
        0 => {}
        1 => {
            t.real = 0x01;
            t.entries.push((0x01, 0, 1, None));
        }
        2 => {
            t.real = 0x03;
            t.entries.push((0x01, 0, 1, None));
            t.entries.push((0x02, 0, 1, None));
        }
        _ => {
            t.real = 0x01;
            t.vmods = numlock_vmod;
            t.entries.push((0x01, 0, 1, None));
            t.entries.push((0, numlock_vmod, 1, None));
        }
    }
    t
}

/// xkbcomp's `FindAutomaticType` (symbols.c) over a group's keysyms, the
/// case tests with libX11's `XConvertCase`: the type name, and whether it's
/// one of the "automatic" ones that don't mark the group explicit.
fn automatic_type(syms: &[u32]) -> (&'static str, bool) {
    let lower = |s: u32| {
        let (l, u) = super::case::xconvert_case(s);
        l == s && u != s
    };
    let upper = |s: u32| {
        let (l, u) = super::case::xconvert_case(s);
        u == s && l != s
    };
    let sym = |i: usize| syms.get(i).copied().unwrap_or(0);
    match syms.len() {
        0 | 1 => ("ONE_LEVEL", true),
        2 => {
            if lower(sym(0)) && upper(sym(1)) {
                ("ALPHABETIC", false)
            } else if super::is_keypad(sym(0)) || super::is_keypad(sym(1)) {
                ("KEYPAD", true)
            } else {
                ("TWO_LEVEL", true)
            }
        }
        3 | 4 => {
            if lower(sym(0)) && upper(sym(1)) {
                if lower(sym(2)) && upper(sym(3)) {
                    ("FOUR_LEVEL_ALPHABETIC", false)
                } else {
                    ("FOUR_LEVEL_SEMIALPHABETIC", false)
                }
            } else if super::is_keypad(sym(0)) || super::is_keypad(sym(1)) {
                ("FOUR_LEVEL_KEYPAD", false)
            } else {
                ("FOUR_LEVEL", false)
            }
        }
        _ => ("TWO_LEVEL", false),
    }
}

/// `groups= 0xfe` / `Group2+Group3` → the group mask.
fn parse_groups(v: &str) -> u8 {
    let v = v.trim();
    if let Some(h) = v.strip_prefix("0x") {
        return u8::try_from(u32::from_str_radix(h, 16).unwrap_or(0) & 0xff).unwrap_or(0);
    }
    if v.eq_ignore_ascii_case("all") {
        return 0xff;
    }
    v.split('+')
        .filter_map(text::index_number)
        .filter(|&g| (1..=8).contains(&g))
        .fold(0, |m, g| m | (1 << (g - 1)))
}

/// `locked+effective` → the `XkbIM_Use*` bits.
fn parse_state_mask(v: &str) -> u8 {
    v.split('+')
        .map(|p| match p.trim().to_ascii_lowercase().as_str() {
            "base" | "depressed" => IM_USE_BASE,
            "latched" => IM_USE_LATCHED,
            "locked" => IM_USE_LOCKED,
            "effective" => IM_USE_EFFECTIVE,
            "compat" => IM_USE_COMPAT,
            "any" | "all" => 0x1f,
            _ => 0,
        })
        .fold(0, |m, b| m | b)
}

/// The per-indicator flags xkbcommon doesn't dump, by standard indicator
/// name (xkeyboard-config's `!allowExplicit` / `drivesKeyboard`),
/// golden-verified against Xorg's GetIndicatorMap.
fn indicator_flags_by_name(name: &str) -> u8 {
    match name {
        "Caps Lock" | "Num Lock" | "Shift Lock" | "Group 2" => IM_NO_EXPLICIT,
        "Mouse Keys" => IM_LED_DRIVES_KB,
        _ => 0,
    }
}

impl XkbDesc {
    /// Seed the model from `keymap` (see the module doc).
    pub(crate) fn from_keymap(keymap: &Keymap) -> Result<Self, DumpError> {
        let dump = text::keymap_text(keymap);
        let (vmod_names, vmod_real) = vmod_mappings(keymap);
        let vmod_bit = |name: &str| {
            vmod_names
                .iter()
                .position(|n| n.eq_ignore_ascii_case(name))
                .map(|i| 1u16 << i)
        };
        let mut desc = Self::empty();
        desc.vmods = vmod_real;
        for (i, n) in vmod_names.iter().enumerate().take(NUM_VMODS) {
            desc.names.vmods[i] = Some(n.clone());
        }

        // xkb_keycodes: indicator names, aliases.
        let keycodes = text::section_statements(&dump, "xkb_keycodes")?
            .ok_or(DumpError::MissingSection("xkb_keycodes"))?;
        for stmt in &keycodes {
            if let Some(rest) = stmt.strip_prefix("indicator") {
                let Some((n, name)) = rest.split_once('=') else {
                    continue;
                };
                if let (Some(n), Some(name)) = (text::index_number(n), text::quoted(name))
                    && (1..=NUM_INDICATORS).contains(&n)
                {
                    desc.names.indicators[n - 1] = Some(name);
                }
            } else if let Some(rest) = stmt.strip_prefix("alias") {
                let names: Vec<&str> = rest
                    .split(['<', '>'])
                    .map(str::trim)
                    .filter(|s| !s.is_empty() && *s != "=" && *s != ";")
                    .collect();
                if let [alias, real] = names[..] {
                    desc.names
                        .key_aliases
                        .push((key_name_bytes(real), key_name_bytes(alias)));
                }
            }
        }
        for kc in 8..=255u8 {
            if let Some(name) = keymap.key_get_name(Keycode::new(u32::from(kc))) {
                desc.names.keys[usize::from(kc)] = key_name_bytes(name);
            }
        }

        // xkb_types.
        let mut dump_types: Vec<DumpType> = Vec::new();
        for stmt in text::section_statements(&dump, "xkb_types")?.unwrap_or_default() {
            let Some(rest) = stmt.strip_prefix("type") else {
                continue;
            };
            if !rest.starts_with(|c: char| c.is_whitespace() || c == '"') {
                continue;
            }
            let Some(name) = text::quoted(rest) else {
                continue;
            };
            let mut t = DumpType {
                name,
                ..DumpType::default()
            };
            for f in text::body_statements(stmt)? {
                let (key, idx, value) = text::field(f);
                let value = value.unwrap_or("");
                match (key.to_ascii_lowercase().as_str(), idx) {
                    ("modifiers", None) => {
                        (t.real, t.vmods) = action::parse_mods(value, &vmod_bit);
                    }
                    ("map", Some(idx)) => {
                        let (r, v) = action::parse_mods(idx, &vmod_bit);
                        let level = text::index_number(value).unwrap_or(1).max(1) - 1;
                        let level = u8::try_from(level).unwrap_or(0);
                        match t.entries.iter_mut().find(|e| e.0 == r && e.1 == v) {
                            Some(e) => e.2 = level,
                            None => t.entries.push((r, v, level, None)),
                        }
                    }
                    ("preserve", Some(idx)) => {
                        let (r, v) = action::parse_mods(idx, &vmod_bit);
                        let p = action::parse_mods(value, &vmod_bit);
                        match t.entries.iter_mut().find(|e| e.0 == r && e.1 == v) {
                            Some(e) => e.3 = Some(p),
                            None => t.entries.push((r, v, 0, Some(p))),
                        }
                    }
                    ("level_name", Some(idx)) => {
                        if let (Some(l), Some(n)) = (text::index_number(idx), text::quoted(value))
                            && l >= 1
                        {
                            t.level_names.push((l - 1, n));
                        }
                    }
                    _ => {}
                }
            }
            dump_types.push(t);
        }
        let numlock = vmod_bit("NumLock").unwrap_or(0);
        let mut ordered: Vec<DumpType> = Vec::new();
        for (i, req) in REQUIRED_TYPE_NAMES.iter().enumerate() {
            match dump_types.iter().position(|t| t.name == *req) {
                Some(p) => ordered.push(dump_types.remove(p)),
                None => {
                    log::warn!("xkb: keymap has no {req} type; using the canonical one");
                    ordered.push(canonical_type(i, numlock));
                }
            }
        }
        ordered.extend(dump_types);
        let type_index: HashMap<String, usize> = ordered
            .iter()
            .enumerate()
            .map(|(i, t)| (t.name.clone(), i))
            .collect();
        for dt in &ordered {
            let (entries, preserve, num_levels) = xkbcomp_type(dt);
            let tmask = dt.real | desc.vmods_to_real(dt.vmods);
            let map = entries
                .iter()
                .map(|&(r, v, level)| {
                    let vr = desc.vmods_to_real(v);
                    KtEntry {
                        active: v == 0 || vr != 0,
                        mods: Mods {
                            mask: (r | vr) & tmask,
                            real: r,
                            vmods: v,
                        },
                        level,
                    }
                })
                .collect();
            let preserve = preserve.map(|p| {
                p.iter()
                    .map(|&(r, v)| Mods {
                        mask: r | desc.vmods_to_real(v),
                        real: r,
                        vmods: v,
                    })
                    .collect()
            });
            let level_names = (!dt.level_names.is_empty()).then(|| {
                let mut names = vec![None; usize::from(num_levels)];
                for (l, n) in &dt.level_names {
                    if let Some(slot) = names.get_mut(*l) {
                        *slot = Some(n.clone());
                    }
                }
                names
            });
            desc.types.push(KeyType {
                mods: Mods {
                    mask: tmask,
                    real: dt.real,
                    vmods: dt.vmods,
                },
                num_levels,
                map,
                preserve,
                name: Some(dt.name.clone()),
                level_names,
            });
        }

        // xkb_compatibility: interprets, indicator maps.
        let mut default_level_one = false;
        let mut default_repeat = false;
        let mut default_locking = false;
        let mut led_maps: Vec<(String, IndicatorMap)> = Vec::new();
        for stmt in text::section_statements(&dump, "xkb_compatibility")?.unwrap_or_default() {
            if let Some(default) = stmt.strip_prefix("interpret.") {
                let (k, _, v) = text::field(default.trim_end_matches(';'));
                let v = v.unwrap_or("");
                match k.to_ascii_lowercase().as_str() {
                    "usemodmapmods" => {
                        default_level_one =
                            v.eq_ignore_ascii_case("level1") || v.eq_ignore_ascii_case("levelone");
                    }
                    "repeat" => default_repeat = is_true(v),
                    "locking" => default_locking = is_true(v),
                    _ => {}
                }
            } else if let Some(rest) = stmt.strip_prefix("interpret") {
                if !rest.starts_with(|c: char| c.is_whitespace()) {
                    continue;
                }
                let head = rest.split('{').next().unwrap_or_default().trim();
                if let Some(si) = parse_interpret(
                    head,
                    &text::body_statements(stmt)?,
                    (default_level_one, default_repeat, default_locking),
                    &vmod_bit,
                    &vmod_names,
                ) {
                    desc.compat.push(si);
                }
            } else if let Some(rest) = stmt.strip_prefix("indicator") {
                let Some(name) = text::quoted(rest) else {
                    continue;
                };
                let mut map = IndicatorMap::default();
                let mut real = 0u8;
                let mut vmods = 0u16;
                for f in text::body_statements(stmt)? {
                    let (k, _, v) = text::field(f);
                    let v = v.unwrap_or("");
                    match k.to_ascii_lowercase().as_str() {
                        "whichmodstate" | "whichmodifierstate" => {
                            map.which_mods = parse_state_mask(v);
                        }
                        "modifiers" | "mods" => (real, vmods) = action::parse_mods(v, &vmod_bit),
                        "whichgroupstate" => map.which_groups = parse_state_mask(v),
                        "groups" => map.groups = parse_groups(v),
                        "controls" | "ctrls" => {
                            for c in v.split('+') {
                                if let Some((_, b)) = action::CONTROL_NAMES
                                    .iter()
                                    .find(|(n, _)| n.eq_ignore_ascii_case(c.trim()))
                                {
                                    map.ctrls |= b;
                                }
                            }
                        }
                        _ => {}
                    }
                }
                if map.which_mods == 0 && (real != 0 || vmods != 0) {
                    map.which_mods = IM_USE_EFFECTIVE;
                }
                if map.which_groups == 0 && map.groups != 0 {
                    map.which_groups = IM_USE_EFFECTIVE;
                }
                map.mods = Mods {
                    mask: real | desc.vmods_to_real(vmods),
                    real,
                    vmods,
                };
                map.flags = indicator_flags_by_name(&name);
                led_maps.push((name, map));
            }
        }
        for (name, map) in led_maps {
            let slot = desc
                .names
                .indicators
                .iter()
                .position(|n| n.as_deref() == Some(name.as_str()))
                .or_else(|| desc.names.indicators.iter().position(Option::is_none));
            if let Some(i) = slot {
                desc.names.indicators[i] = Some(name);
                desc.indicators[i] = map;
            }
        }
        desc.phys_indicators = EVDEV_PHYS_INDICATORS;
        for (g, &(mask, real)) in GROUP_COMPAT.iter().enumerate() {
            desc.group_compat[g] = Mods {
                mask,
                real,
                vmods: 0,
            };
        }

        // xkb_symbols: group names, per-key explicit properties, modmap.
        let mut dump_keys: HashMap<u8, DumpKey> = HashMap::new();
        let key_code = |name: &str| {
            keymap
                .key_by_name(name)
                .and_then(|k| u8::try_from(k.raw()).ok())
        };
        for stmt in text::section_statements(&dump, "xkb_symbols")?.unwrap_or_default() {
            if let Some(rest) = stmt.strip_prefix("name") {
                let (_, idx, v) = text::field(rest);
                if let (Some(g), Some(n)) =
                    (idx.and_then(text::index_number), v.and_then(text::quoted))
                    && (1..=NUM_GROUPS).contains(&g)
                {
                    desc.names.groups[g - 1] = Some(n);
                }
            } else if let Some(rest) = stmt.strip_prefix("key") {
                if !rest.starts_with(|c: char| c.is_whitespace()) {
                    continue;
                }
                let Some(name) = rest
                    .trim_start()
                    .strip_prefix('<')
                    .and_then(|r| r.split_once('>'))
                    .map(|(n, _)| n)
                else {
                    continue;
                };
                let Some(kc) = key_code(name) else {
                    continue;
                };
                let mut dk = DumpKey::default();
                for f in text::body_fields(stmt)? {
                    let (k, idx, v) = text::field(f);
                    let group = idx.and_then(text::index_number).map(|g| g.max(1) - 1);
                    match k.to_ascii_lowercase().as_str() {
                        "type" => {
                            let Some(t) = v.and_then(text::quoted) else {
                                continue;
                            };
                            match group {
                                Some(g) if g < NUM_GROUPS => dk.types[g] = Some(t),
                                Some(_) => {}
                                None => dk.types = std::array::from_fn(|_| Some(t.clone())),
                            }
                        }
                        "actions" => {
                            let g = group.unwrap_or(0);
                            if g < NUM_GROUPS
                                && let Some(list) = v
                                    .and_then(|v| v.trim().strip_prefix('['))
                                    .and_then(|v| v.strip_suffix(']'))
                            {
                                dk.actions[g] = Some(
                                    text::split_top_level(list, b',')
                                        .into_iter()
                                        .map(str::to_owned)
                                        .collect(),
                                );
                            }
                        }
                        "virtualmods" | "vmods" | "virtualmodifiers" => {
                            dk.vmods = v.map(str::to_owned);
                        }
                        "repeat" | "repeats" => dk.repeat = v.map(is_true),
                        "groupsclamp" => dk.range = CLAMP_INTO_RANGE,
                        "groupsredirect" => {
                            let g = v.and_then(text::index_number).unwrap_or(1).max(1) - 1;
                            dk.range =
                                REDIRECT_INTO_RANGE | (u8::try_from(g & 0x03).unwrap_or(0) << 4);
                        }
                        _ => {}
                    }
                }
                dump_keys.insert(kc, dk);
            } else if let Some(rest) = stmt.strip_prefix("modifier_map") {
                let Some((m, keys)) = rest.split_once('{') else {
                    continue;
                };
                let Some(bit) = crate::kms::xkb::REAL_MOD_NAMES
                    .iter()
                    .position(|n| n.eq_ignore_ascii_case(m.trim()))
                else {
                    continue;
                };
                for k in keys.split(['}', ',']) {
                    if let Some(kc) = k
                        .trim()
                        .strip_prefix('<')
                        .and_then(|k| k.strip_suffix('>'))
                        .and_then(key_code)
                    {
                        desc.modmap[usize::from(kc)] |= 1 << bit;
                    }
                }
            }
        }

        // Keys.
        let mut max_groups = 1u8;
        for kc in 8..=255u8 {
            let k = usize::from(kc);
            let key = Keycode::new(u32::from(kc));
            let n_groups = keymap.num_layouts_for_key(key).min(4) as usize;
            let dk = dump_keys.remove(&kc).unwrap_or_default();
            let groups: Vec<Vec<u32>> = (0..n_groups)
                .map(|g| {
                    let g32 = u32::try_from(g).unwrap_or(0);
                    (0..keymap.num_levels_for_key(key, g32))
                        .map(|l| {
                            keymap
                                .key_get_syms_by_level(key, g32, l)
                                .first()
                                .map_or(0, |s| s.raw())
                        })
                        .collect()
                })
                .collect();
            // xkbcomp's PrepareKeyDef: groups identical in type, keysyms and
            // actions collapse into the first (xkbcommon keeps them all).
            let identical = groups.len() > 1
                && (1..groups.len()).all(|g| {
                    groups[g] == groups[0]
                        && dk.types[g] == dk.types[0]
                        && dk.actions[g] == dk.actions[0]
                });
            let (groups, n_groups) = if identical {
                (groups[..1].to_vec(), 1)
            } else {
                (groups, n_groups)
            };
            let width = groups.iter().map(Vec::len).max().unwrap_or(0);
            let mut ks = KeySyms {
                width: u8::try_from(width).unwrap_or(u8::MAX),
                group_info: u8::try_from(n_groups).unwrap_or(0) | dk.range,
                ..KeySyms::default()
            };
            let mut explicit = 0u8;
            for (g, syms) in groups.iter().enumerate() {
                let (name, auto) = match &dk.types[g] {
                    Some(t) => (t.as_str(), false),
                    None => automatic_type(syms),
                };
                let idx = type_index.get(name).copied().unwrap_or_else(|| {
                    log::warn!("xkb: keycode {kc} group {g}: no type {name:?}; TWO_LEVEL");
                    1
                });
                let levels = usize::from(ordered_levels(&desc, idx));
                if levels != syms.len() {
                    log::warn!(
                        "xkb: keycode {kc} group {g}: type {name} has {levels} levels, \
                         xkbcommon {}",
                        syms.len()
                    );
                }
                ks.kt_index[g] = u8::try_from(idx).unwrap_or(1);
                if !auto || syms.len() > 2 {
                    explicit |= 1 << g;
                }
            }
            ks.syms = vec![0; width * n_groups];
            for (g, syms) in groups.iter().enumerate() {
                for (l, &s) in syms.iter().enumerate() {
                    ks.syms[g * width + l] = s;
                }
            }
            if dk.actions.iter().any(Option::is_some) {
                explicit |= EXPLICIT_INTERPRET;
                let mut acts = vec![[0u8; 8]; width * n_groups];
                for (g, list) in dk.actions.iter().enumerate().take(n_groups) {
                    for (l, a) in list.iter().flatten().enumerate().take(width) {
                        match action::parse_action(a, &vmod_bit) {
                            Ok(act) => acts[g * width + l] = act,
                            Err(e) => log::warn!("xkb: keycode {kc}: {e}; NoAction"),
                        }
                    }
                }
                let modmap = desc.modmap[k];
                for act in &mut acts {
                    desc.set_action_key_mods(act, modmap);
                }
                desc.acts[k] = Some(acts);
            }
            if let Some(v) = &dk.vmods {
                explicit |= EXPLICIT_VMODMAP;
                desc.vmodmap[k] = action::parse_mods(v, &vmod_bit).1;
            }
            if let Some(r) = dk.repeat {
                explicit |= EXPLICIT_AUTO_REPEAT;
                desc.per_key_repeat[k >> 3] = if r {
                    desc.per_key_repeat[k >> 3] | (1 << (kc & 7))
                } else {
                    desc.per_key_repeat[k >> 3] & !(1 << (kc & 7))
                };
            }
            desc.explicit[k] = explicit;
            max_groups = max_groups.max(u8::try_from(n_groups).unwrap_or(1));
            desc.keys[k] = ks;
        }
        desc.num_groups = max_groups;

        // Component names, as Xorg reports the ones the rules resolved to.
        desc.names.keycodes = section_name(&dump, "xkb_keycodes").map(|n| unescape_component(&n));
        desc.names.types = section_name(&dump, "xkb_types").map(|n| unescape_component(&n));
        desc.names.compat =
            section_name(&dump, "xkb_compatibility").map(|n| unescape_component(&n));
        desc.names.symbols = section_name(&dump, "xkb_symbols").map(|n| unescape_component(&n));
        desc.names.phys_symbols = desc.names.symbols.clone();

        // The server's XkbUpdateDescActions over every key.
        let mut changes = XkbChanges::default();
        let (min, num) = (desc.min_key_code, desc.num_keys());
        desc.update_desc_actions(min, num, &mut changes);
        Ok(desc)
    }
}

fn ordered_levels(desc: &XkbDesc, idx: usize) -> u8 {
    desc.types.get(idx).map_or(0, |t| t.num_levels)
}

/// One `interpret` as Xorg's `XkbSymInterpretRec`.
fn parse_interpret(
    head: &str,
    fields: &[&str],
    (default_level_one, default_repeat, default_locking): (bool, bool, bool),
    vmod_bit: &dyn Fn(&str) -> Option<u16>,
    vmod_names: &[String],
) -> Option<SymInterpret> {
    let (sym_text, pred) = match head.split_once('+') {
        Some((s, p)) => (s.trim(), p.trim()),
        None => (head.trim(), "AnyOfOrNone(all)"),
    };
    let sym = dump_keysym(sym_text).unwrap_or(0);
    if sym == 0
        && !sym_text.eq_ignore_ascii_case("Any")
        && !sym_text.eq_ignore_ascii_case("NoSymbol")
    {
        log::debug!("xkb: interpret for unknown keysym {sym_text:?} ignored");
        return None;
    }
    let (op_text, mods_text) = pred.split_once('(').unwrap_or((pred, "all)"));
    let op = match op_text.trim().to_ascii_lowercase().as_str() {
        "noneof" => SI_NONE_OF,
        "anyofornone" => SI_ANY_OF_OR_NONE,
        "anyof" => SI_ANY_OF,
        "allof" => SI_ALL_OF,
        "exactly" => SI_EXACTLY,
        other => {
            log::debug!("xkb: interpret match {other:?} ignored");
            return None;
        }
    };
    let (mods, _) = action::parse_mods(mods_text.trim_end_matches(')'), vmod_bit);
    let mut level_one = default_level_one;
    let mut repeat = default_repeat;
    let mut locking = default_locking;
    let mut virtual_mod = NO_MODIFIER;
    let mut act: Action = [0; 8];
    for f in fields {
        let (k, _, v) = text::field(f);
        let v = v.unwrap_or("");
        match k.to_ascii_lowercase().as_str() {
            "virtualmodifier" | "virtualmod" => {
                virtual_mod = vmod_names
                    .iter()
                    .position(|n| n.eq_ignore_ascii_case(v))
                    .and_then(|i| u8::try_from(i).ok())
                    .unwrap_or(NO_MODIFIER);
            }
            "usemodmapmods" | "usemodmap" => {
                level_one = v.eq_ignore_ascii_case("level1") || v.eq_ignore_ascii_case("levelone");
            }
            "repeat" => repeat = is_true(v),
            "locking" => locking = is_true(v),
            "action" => match action::parse_action(v, vmod_bit) {
                Ok(a) => act = a,
                Err(e) => log::warn!("xkb: interpret {head}: {e}; NoAction"),
            },
            _ => {}
        }
    }
    Some(SymInterpret {
        sym,
        mods,
        match_: op | if level_one { SI_LEVEL_ONE_ONLY } else { 0 },
        virtual_mod,
        flags: if repeat { SI_AUTO_REPEAT } else { 0 } | if locking { SI_LOCKING_KEY } else { 0 },
        act,
    })
}
