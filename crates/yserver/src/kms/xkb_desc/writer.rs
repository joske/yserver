//! The cooking keymap: the model written out as complete V1 text for
//! xkbcommon to compile (§4.3).
//!
//! The text is written so that xkbcommon derives nothing: every modifier is
//! already resolved to real modifiers (types, entries, preserve, actions,
//! indicator maps), so no virtual modifier is declared; there are no
//! interprets, so every key carries its own actions; every group names its
//! type and every key its repeat. Names are synthetic (`<K038>`, `"T12"`)
//! except the indicators', which the LEDs are read back by. Only what
//! xkbcommon can't carry (§4.6) differs from the model, and the writer
//! reports it.

use std::fmt::Write as _;

use super::{
    Action, CLAMP_INTO_RANGE, IndicatorMap, NUM_INDICATORS, REDIRECT_INTO_RANGE,
    REQUIRED_TYPE_NAMES, XkbDesc,
    action::{self, real_mods_text},
};

/// `XKB_KEYSYM_MAX`: the largest keysym libxkbcommon accepts.
const KEYSYM_MAX: u32 = 0x1fff_ffff;

/// What the cooking keymap can't carry: stored and read back exactly, not
/// cooked (§4.6).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Uncooked {
    /// `(keycode, slot, action)`: actions written as `NoAction()`.
    pub actions: Vec<(u8, usize, Action)>,
    /// `(keycode, type, data)`: non-default key behaviors.
    pub behaviors: Vec<(u8, u8, u8)>,
    /// `(keycode, slot, keysym)`: keysyms keymap text can't spell.
    pub keysyms: Vec<(u8, usize, u32)>,
}

impl Uncooked {
    pub(crate) fn is_empty(&self) -> bool {
        self.actions.is_empty() && self.behaviors.is_empty() && self.keysyms.is_empty()
    }
}

/// The written name of type `index`: the required slots keep their XKB
/// names (xkbcommon attaches no meaning to them), the rest are synthetic.
pub(crate) fn type_name(index: usize) -> String {
    REQUIRED_TYPE_NAMES
        .get(index)
        .map_or_else(|| format!("T{index}"), |n| (*n).to_owned())
}

/// The name indicator `i` is written with, `None` when it has neither a
/// name nor a map: the model's name when it can be written as a string,
/// else `yserver-led<i>`.
pub(crate) fn indicator_name(desc: &XkbDesc, i: usize) -> Option<String> {
    let map = desc.indicators[i];
    let name = desc.names.indicators[i].as_deref();
    if name.is_none() && map == IndicatorMap::default() {
        return None;
    }
    Some(match name {
        Some(n) if !n.is_empty() && n.chars().all(|c| c != '"' && c != '\\' && !c.is_control()) => {
            n.to_owned()
        }
        _ => format!("yserver-led{i}"),
    })
}

/// A keysym as every libxkbcommon parses it: hex from 10 up. The integers
/// 0–9 would parse as the digit keysyms and values above `XKB_KEYSYM_MAX`
/// are rejected, so those (none of which is an assigned keysym) can't be
/// written.
fn keysym_text(sym: u32) -> Option<String> {
    match sym {
        0 => Some("NoSymbol".to_owned()),
        1..=9 => None,
        s if s > KEYSYM_MAX => None,
        s => Some(format!("{s:#x}")),
    }
}

/// `XkbIM_Use*` bits as xkbcommon names them.
fn state_mask_text(mask: u8) -> String {
    let names: Vec<&str> = [
        (1u8, "base"),
        (2, "latched"),
        (4, "locked"),
        (8, "effective"),
        (16, "compat"),
    ]
    .iter()
    .filter(|(b, _)| mask & b != 0)
    .map(|(_, n)| *n)
    .collect();
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.join("+")
    }
}

impl XkbDesc {
    /// The cooking keymap text, and what it couldn't carry.
    pub(crate) fn to_v1_text(&self) -> (String, Uncooked) {
        let mut out = String::with_capacity(64 * 1024);
        let mut uncooked = Uncooked::default();
        let (min, max) = (self.min_key_code, self.max_key_code);

        // xkbcommon's layout count is the most groups any key has, Xorg's
        // `ctrls->num_groups` only grows: a key beyond X's keycode range
        // (never pressed) carries the difference, so the state wraps groups
        // over the same count as Xorg.
        let key_groups = (min..=max)
            .map(|kc| self.keys[usize::from(kc)].num_groups())
            .max()
            .unwrap_or(0);
        let groups = usize::from(self.num_groups.clamp(1, 4));
        let pad = groups > key_groups.max(1);
        out.push_str("xkb_keymap {\nxkb_keycodes \"yserver\" {\n");
        let top = if pad {
            u32::from(max) + 1
        } else {
            u32::from(max)
        };
        let _ = writeln!(out, "\tminimum = {min};\n\tmaximum = {top};");
        for kc in u32::from(min)..=top {
            let _ = writeln!(out, "\t<K{kc:03}> = {kc};");
        }
        for i in 0..NUM_INDICATORS {
            if let Some(name) = indicator_name(self, i) {
                let _ = writeln!(out, "\tindicator {} = \"{name}\";", i + 1);
            }
        }
        out.push_str("};\n\nxkb_types \"yserver\" {\n");
        for (i, t) in self.types.iter().enumerate() {
            let _ = writeln!(out, "\ttype \"{}\" {{", type_name(i));
            let _ = writeln!(out, "\t\tmodifiers= {};", real_mods_text(t.mods.mask));
            let mut written: Vec<u8> = Vec::new();
            for (n, e) in t.map.iter().enumerate() {
                // An inactive entry never matches; an entry outside the
                // type's modifiers never matches either (xkbcommon would
                // mask it into one that does); a later entry with an
                // already written mask is shadowed by the first.
                if !e.active
                    || e.mods.mask & !t.mods.mask != 0
                    || e.level >= t.num_levels
                    || written.contains(&e.mods.mask)
                {
                    continue;
                }
                written.push(e.mods.mask);
                let mask = real_mods_text(e.mods.mask);
                let _ = writeln!(out, "\t\tmap[{mask}]= {};", e.level + 1);
                let pre = t
                    .preserve
                    .as_ref()
                    .and_then(|p| p.get(n))
                    .map_or(0, |p| p.mask & e.mods.mask);
                if pre != 0 {
                    let _ = writeln!(out, "\t\tpreserve[{mask}]= {};", real_mods_text(pre));
                }
            }
            for l in 1..=t.num_levels {
                let _ = writeln!(out, "\t\tlevel_name[{l}]= \"L{l}\";");
            }
            out.push_str("\t};\n");
        }
        out.push_str("};\n\nxkb_compatibility \"yserver\" {\n");
        for i in 0..NUM_INDICATORS {
            let map = self.indicators[i];
            if map == IndicatorMap::default() {
                continue;
            }
            let Some(name) = indicator_name(self, i) else {
                continue;
            };
            let _ = writeln!(out, "\tindicator \"{name}\" {{");
            if map.which_mods != 0 {
                let _ = writeln!(
                    out,
                    "\t\twhichModState= {};\n\t\tmodifiers= {};",
                    state_mask_text(map.which_mods),
                    real_mods_text(map.mods.mask)
                );
            }
            if map.which_groups != 0 {
                let _ = writeln!(
                    out,
                    "\t\twhichGroupState= {};\n\t\tgroups= {:#04x};",
                    state_mask_text(map.which_groups),
                    map.groups
                );
            }
            if map.ctrls != 0 {
                let names: Vec<&str> = action::CONTROL_NAMES
                    .iter()
                    .filter(|(_, b)| map.ctrls & b != 0)
                    .map(|(n, _)| *n)
                    .collect();
                if !names.is_empty() {
                    let _ = writeln!(out, "\t\tcontrols= {};", names.join("+"));
                }
            }
            out.push_str("\t};\n");
        }
        out.push_str("};\n\nxkb_symbols \"yserver\" {\n");
        for kc in min..=max {
            self.write_key(&mut out, kc, &mut uncooked);
        }
        if pad {
            let syms: Vec<String> = (1..=groups)
                .map(|g| format!("symbols[Group{g}]= [ VoidSymbol ]"))
                .collect();
            let _ = writeln!(out, "\tkey <K{top:03}> {{ {} }};", syms.join(", "));
        }
        for (bit, name) in crate::kms::xkb::REAL_MOD_NAMES.iter().enumerate() {
            let keys: Vec<String> = (min..=max)
                .filter(|&kc| {
                    let m = self.modmap[usize::from(kc)];
                    m != 0 && m.trailing_zeros() as usize == bit
                })
                .map(|kc| format!("<K{kc:03}>"))
                .collect();
            if !keys.is_empty() {
                let _ = writeln!(out, "\tmodifier_map {name} {{ {} }};", keys.join(", "));
            }
        }
        out.push_str("};\n\n};\n");
        (out, uncooked)
    }

    fn write_key(&self, out: &mut String, kc: u8, uncooked: &mut Uncooked) {
        let k = usize::from(kc);
        let key = &self.keys[k];
        let b = self.behaviors[k];
        if b.kind != super::KB_DEFAULT {
            uncooked.behaviors.push((kc, b.kind, b.data));
        }
        let mut fields: Vec<String> = Vec::new();
        let n_groups = key.num_groups();
        let width = usize::from(key.width);
        for g in 0..n_groups {
            fields.push(format!(
                "type[Group{}]= \"{}\"",
                g + 1,
                type_name(usize::from(key.kt_index.get(g).copied().unwrap_or(0)))
            ));
        }
        for g in 0..n_groups {
            let levels = self.group_width(kc, g);
            let syms: Vec<String> = (0..levels)
                .map(|l| {
                    let slot = g * width + l;
                    let sym = key.syms.get(slot).copied().unwrap_or(0);
                    keysym_text(sym).unwrap_or_else(|| {
                        uncooked.keysyms.push((kc, slot, sym));
                        "NoSymbol".to_owned()
                    })
                })
                .collect();
            fields.push(format!("symbols[Group{}]= [ {} ]", g + 1, syms.join(", ")));
            if self.acts[k].is_some() {
                let acts: Vec<String> = (0..levels)
                    .map(|l| {
                        let slot = g * width + l;
                        let act = self.key_action(kc, slot);
                        if action::cookable(&act) {
                            action::action_text(&act)
                        } else {
                            uncooked.actions.push((kc, slot, act));
                            "NoAction()".to_owned()
                        }
                    })
                    .collect();
                fields.push(format!("actions[Group{}]= [ {} ]", g + 1, acts.join(", ")));
            }
        }
        if n_groups > 0 {
            match key.group_info & 0xc0 {
                CLAMP_INTO_RANGE => fields.push("groupsClamp".to_owned()),
                REDIRECT_INTO_RANGE => fields.push(format!(
                    "groupsRedirect= Group{}",
                    ((key.group_info >> 4) & 0x03) + 1
                )),
                _ => {}
            }
        }
        let repeats = self.repeats(kc);
        fields.push(format!("repeat= {}", if repeats { "Yes" } else { "No" }));
        let _ = writeln!(out, "\tkey <K{kc:03}> {{ {} }};", fields.join(", "));
    }
}
