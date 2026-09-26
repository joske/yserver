//! The cooking gate (#171 phase 4 review outcome): the compiled cooking
//! keymap must cook every key and level the way the model's action says.
//!
//! For every key, every group of the keymap and every modifier mask the
//! key's type distinguishes, a fresh `xkb_state` is put in that group and
//! those depressed modifiers, the key is pressed and released, and the
//! keysym and the modifier/group state change are compared with what the
//! model prescribes: its level lookup (Xorg's first-match rule), its keysym
//! and its action (SetMods/LatchMods/LockMods, SetGroup/LatchGroup/
//! LockGroup, no state change for NoAction and the actions that don't touch
//! modifiers or groups). The per-key repeat, the modmap and the lock LEDs
//! are compared too.
//!
//! What xkbcommon can't cook (§4.6) is excluded explicitly and reported:
//! the actions the writer turns into `NoAction()`, keys with a non-default
//! behavior, and keysyms keymap text can't spell.

use xkbcommon::xkb::{self, KeyDirection, Keycode, Keymap};

use super::{
    Action, SA_LATCH_GROUP, SA_LATCH_MODS, SA_LOCK_GROUP, SA_LOCK_MODS, SA_SET_GROUP, SA_SET_MODS,
    XkbDesc, action, writer,
};

/// `XkbSA_ClearLocks`, `XkbSA_LatchToLock`, `XkbSA_GroupAbsolute`,
/// `XkbSA_LockNoLock`.
const CLEAR_LOCKS: u8 = 0x01;
const GROUP_ABSOLUTE: u8 = 0x04;
const LOCK_NO_LOCK: u8 = 0x01;

/// What the gate left out, by reason.
#[derive(Debug, Default)]
pub(crate) struct Excluded {
    pub uncooked_actions: Vec<(u8, usize, Action)>,
    pub behaviors: Vec<u8>,
    pub keysyms: Vec<(u8, usize, u32)>,
    /// LatchGroup slots, when this libxkbcommon doesn't latch groups.
    pub group_latches: Vec<(u8, usize)>,
    /// Keys without symbols, whose repeat flag isn't compared.
    pub repeat_without_symbols: Vec<u8>,
}

/// Compile keymap text as production does.
pub(crate) fn compile(text: &str) -> Keymap {
    let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    Keymap::new_from_string(
        &ctx,
        text.to_owned(),
        xkb::KEYMAP_FORMAT_TEXT_V1,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .expect("the cooking keymap compiles")
}

/// Whether this libxkbcommon implements group latches (1.6 has no filter
/// for them).
fn latches_groups() -> bool {
    let text = "xkb_keymap { xkb_keycodes { <A> = 10; <B> = 11; }; \
        xkb_types { type \"ONE_LEVEL\" { modifiers= none; level_name[1]= \"L1\"; }; }; \
        xkb_compat { }; xkb_symbols { \
        key <A> { type[Group1]= \"ONE_LEVEL\", symbols[Group1]= [ 0x61 ], \
                  actions[Group1]= [ LatchGroup(group=+1) ] }; \
        key <B> { symbols[Group1]= [ 0x62 ], symbols[Group2]= [ 0x63 ] }; }; };";
    let km = compile(text);
    let mut st = xkb::State::new(&km);
    st.update_key(Keycode::new(10), KeyDirection::Down);
    st.update_key(Keycode::new(10), KeyDirection::Up);
    st.serialize_layout(xkb::STATE_LAYOUT_LATCHED) != 0
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Snapshot {
    depressed: u32,
    latched: u32,
    locked: u32,
    base_group: i32,
    latched_group: i32,
    locked_group: u32,
}

fn snapshot(st: &xkb::State) -> Snapshot {
    Snapshot {
        depressed: st.serialize_mods(xkb::STATE_MODS_DEPRESSED) & 0xff,
        latched: st.serialize_mods(xkb::STATE_MODS_LATCHED) & 0xff,
        locked: st.serialize_mods(xkb::STATE_MODS_LOCKED) & 0xff,
        base_group: st.serialize_layout(xkb::STATE_LAYOUT_DEPRESSED) as i32,
        latched_group: st.serialize_layout(xkb::STATE_LAYOUT_LATCHED) as i32,
        locked_group: st.serialize_layout(xkb::STATE_LAYOUT_LOCKED),
    }
}

/// Xorg's (and xkbcommon's) state group wrap over `n` groups.
fn wrap(g: i32, n: u32) -> u32 {
    let n = i32::try_from(n.max(1)).unwrap_or(1);
    u32::try_from(g.rem_euclid(n)).unwrap_or(0)
}

/// The mismatches between the compiled keymap's cooking and the model's.
pub(crate) fn check(desc: &XkbDesc, keymap: &Keymap) -> (Vec<String>, Excluded) {
    let (_, uncooked) = desc.to_v1_text();
    let mut excluded = Excluded {
        uncooked_actions: uncooked.actions.clone(),
        behaviors: uncooked.behaviors.iter().map(|b| b.0).collect(),
        keysyms: uncooked.keysyms.clone(),
        group_latches: Vec::new(),
        repeat_without_symbols: Vec::new(),
    };
    let group_latch = latches_groups();
    let mut out = Vec::new();
    let layouts = keymap.num_layouts();
    if layouts != u32::from(desc.num_groups).max(1) {
        out.push(format!(
            "keymap has {layouts} layouts, the model {} groups",
            desc.num_groups
        ));
    }
    for kc in desc.min_key_code..=desc.max_key_code {
        let k = usize::from(kc);
        let key = Keycode::new(u32::from(kc));
        // A key without symbols: libxkbcommon 1.6 drops it, repeat flag and
        // all (1.13 keeps it). Nothing reads xkbcommon's repeat flag there
        // (the core per-key repeat drives autorepeat), so it's excluded.
        if desc.keys[k].num_groups() == 0 {
            excluded.repeat_without_symbols.push(kc);
        } else if keymap.key_repeats(key) != desc.repeats(kc) {
            out.push(format!(
                "keycode {kc}: repeats {} in the keymap, {} in the model",
                keymap.key_repeats(key),
                desc.repeats(kc)
            ));
        }
        if excluded.behaviors.contains(&kc) {
            continue;
        }
        let width = usize::from(desc.keys[k].width);
        for g in 0..layouts {
            let Some(eg) = desc.effective_group(kc, g as usize) else {
                if keymap.num_layouts_for_key(key) != 0 {
                    out.push(format!(
                        "keycode {kc}: no groups in the model, some in the keymap"
                    ));
                }
                continue;
            };
            let t = usize::from(desc.keys[k].kt_index[eg]);
            // Every real-modifier combination, not just the type's own map
            // entries: a chord of several entries' modifiers must cook the
            // way the model's type resolves it too.
            for m in 0..=u8::MAX {
                let (level, _) = desc.type_level(t, m);
                let slot = eg * width + usize::from(level);
                let sym = desc.keys[k].syms.get(slot).copied().unwrap_or(0);
                let act = desc.key_action(kc, slot);
                let what = format!("keycode {kc} group {g} mods {m:#04x} (level {level})");
                let mut st = xkb::State::new(keymap);
                st.update_mask(u32::from(m), 0, 0, 0, 0, g);
                // The level and group lookup, not `key_get_one_sym`: that
                // adds xkbcommon's client-side Caps Lock capitalization,
                // which isn't cooking (the server sends keycodes).
                let layout = st.key_get_layout(key);
                let got_level = st.key_get_level(key, layout);
                if layout as usize != eg || got_level != u32::from(level) {
                    out.push(format!(
                        "{what}: cooks group {layout} level {got_level}, model group {eg} \
                         level {level}"
                    ));
                }
                if !excluded
                    .keysyms
                    .iter()
                    .any(|&(c, s, _)| c == kc && s == slot)
                {
                    let got = keymap
                        .key_get_syms_by_level(key, layout, got_level)
                        .first()
                        .map_or(0, |s| s.raw());
                    if got != sym {
                        out.push(format!("{what}: cooks {got:#x}, model {sym:#x}"));
                    }
                }
                if excluded
                    .uncooked_actions
                    .iter()
                    .any(|&(c, s, _)| c == kc && s == slot)
                {
                    continue;
                }
                if act[0] == SA_LATCH_GROUP && !group_latch {
                    if !excluded.group_latches.contains(&(kc, slot)) {
                        excluded.group_latches.push((kc, slot));
                    }
                    continue;
                }
                let before = snapshot(&st);
                st.update_key(key, KeyDirection::Down);
                let pressed = snapshot(&st);
                st.update_key(key, KeyDirection::Up);
                let released = snapshot(&st);
                let (want_pressed, want_released) =
                    prescribed(&act, before, u32::from(m), g, layouts);
                if pressed != want_pressed || released != want_released {
                    out.push(format!(
                        "{what}: {} → pressed {pressed:?} released {released:?}, \
                         prescribed {want_pressed:?} / {want_released:?}",
                        action::action_text(&act)
                    ));
                }
            }
        }
    }
    check_leds(desc, keymap, &mut out);
    let reseeded = XkbDesc::from_keymap(keymap).expect("reseed the cooking keymap");
    for kc in desc.min_key_code..=desc.max_key_code {
        let m = desc.modmap[usize::from(kc)];
        let lowest = if m == 0 { 0 } else { 1 << m.trailing_zeros() };
        if reseeded.modmap[usize::from(kc)] != lowest {
            out.push(format!(
                "keycode {kc}: modmap {:#04x} in the keymap, {m:#04x} in the model",
                reseeded.modmap[usize::from(kc)]
            ));
        }
    }
    (out, excluded)
}

/// The state after press and after release that the action prescribes,
/// from `before` (depressed `m`, locked group `g`, nothing else).
fn prescribed(
    act: &Action,
    before: Snapshot,
    m: u32,
    g: u32,
    layouts: u32,
) -> (Snapshot, Snapshot) {
    let mask = u32::from(act[2]);
    let flags = act[1];
    let group = i32::from(act[2] as i8);
    let mut pressed = before;
    let mut released = before;
    match act[0] {
        SA_SET_MODS => {
            pressed.depressed = m | mask;
            released.depressed = m & !mask;
        }
        SA_LATCH_MODS => {
            pressed.depressed = m | mask;
            released.depressed = m & !mask;
            released.latched = mask;
        }
        SA_LOCK_MODS => {
            pressed.depressed = m | mask;
            released.depressed = m & !mask;
            if flags & LOCK_NO_LOCK == 0 {
                pressed.locked = mask;
                released.locked = mask;
            }
        }
        SA_SET_GROUP => {
            pressed.base_group = group;
            released.base_group = 0;
            if flags & CLEAR_LOCKS != 0 {
                released.locked_group = 0;
            }
        }
        SA_LATCH_GROUP => {
            pressed.base_group = group;
            released.base_group = 0;
            released.latched_group = if flags & GROUP_ABSOLUTE != 0 {
                group - i32::try_from(g).unwrap_or(0)
            } else {
                group
            };
        }
        SA_LOCK_GROUP => {
            let locked = if flags & GROUP_ABSOLUTE != 0 {
                wrap(group, layouts)
            } else {
                wrap(i32::try_from(g).unwrap_or(0) + group, layouts)
            };
            pressed.locked_group = locked;
            released.locked_group = locked;
        }
        _ => {}
    }
    (pressed, released)
}

/// Each indicator driven by locked or effective modifiers only lights for
/// those modifiers and not without them.
fn check_leds(desc: &XkbDesc, keymap: &Keymap, out: &mut Vec<String>) {
    let names = writer::indicator_names(desc);
    for (i, (&map, name)) in desc.indicators.iter().zip(&names).enumerate() {
        if map.which_mods == 0 || map.mods.mask == 0 || map.which_groups != 0 || map.ctrls != 0 {
            continue;
        }
        let Some(name) = name.clone() else {
            continue;
        };
        let mut st = xkb::State::new(keymap);
        let off = st.led_name_is_active(&name);
        st.update_mask(0, 0, u32::from(map.mods.mask), 0, 0, 0);
        let on = st.led_name_is_active(&name);
        if off || (map.which_mods & 0x0c != 0 && !on) {
            out.push(format!(
                "indicator {i} {name:?}: off={off} on={on} for mods {:#04x}",
                map.mods.mask
            ));
        }
    }
}

/// A modifier or group action as it cooks: the flags that matter and the
/// resolved mask / group; other actions as they are; what xkbcommon can't
/// carry as `NoAction`.
fn cooked(act: &Action) -> Action {
    if !action::cookable(act) {
        return [0; 8];
    }
    match act[0] {
        SA_SET_MODS => [act[0], act[1] & 0x01, act[2], 0, 0, 0, 0, 0],
        SA_LATCH_MODS => [act[0], act[1] & 0x03, act[2], 0, 0, 0, 0, 0],
        SA_LOCK_MODS => [act[0], act[1] & 0x03, act[2], 0, 0, 0, 0, 0],
        SA_SET_GROUP | SA_LATCH_GROUP => [act[0], act[1] & 0x07, act[2], 0, 0, 0, 0, 0],
        SA_LOCK_GROUP => [act[0], act[1] & 0x04, act[2], 0, 0, 0, 0, 0],
        _ => *act,
    }
}

/// Everything about the model that affects cooking, one line per item: per
/// key and group the level of every real-modifier mask with its keysym and
/// cooked action, the out-of-range rule, the repeat and the lowest modmap
/// bit; per indicator its resolved map.
pub(crate) fn cooking_view(desc: &XkbDesc) -> Vec<String> {
    // (The group count isn't here: a re-seed counts the keys' groups, as
    // xkbcomp does; `check` compares the keymap's layouts with it.)
    let mut out = Vec::new();
    for kc in desc.min_key_code..=desc.max_key_code {
        let k = usize::from(kc);
        let key = &desc.keys[k];
        let width = usize::from(key.width);
        let m = desc.modmap[k];
        out.push(format!(
            "key {kc} groups={} range={:#04x} repeat={} modmap={:#04x}",
            key.num_groups(),
            if key.num_groups() > 0 {
                key.group_info & 0xf0
            } else {
                0
            },
            desc.repeats(kc),
            if m == 0 { 0 } else { 1u8 << m.trailing_zeros() }
        ));
        for g in 0..key.num_groups() {
            let t = usize::from(key.kt_index[g]);
            let levels = desc.group_width(kc, g);
            let mut line = format!("key {kc} group {g} levels={levels}");
            for mods in 0..=255u8 {
                let (level, pre) = desc.type_level(t, mods);
                let slot = g * width + usize::from(level);
                let act = cooked(&desc.key_action(kc, slot));
                line.push_str(&format!(
                    " {mods:02x}:{level}/{pre:02x}/{:x}/{}",
                    key.syms.get(slot).copied().unwrap_or(0),
                    act.iter().map(|b| format!("{b:02x}")).collect::<String>()
                ));
            }
            out.push(line);
        }
    }
    let names = writer::indicator_names(desc);
    for (map, name) in desc.indicators.iter().zip(&names) {
        if let Some(name) = name
            && (map.which_mods != 0 || map.which_groups != 0 || map.ctrls != 0)
        {
            out.push(format!(
                "indicator {name:?} whichMods={:02x} mods={:02x} whichGroups={:02x} \
                 groups={:02x} ctrls={:08x}",
                map.which_mods, map.mods.mask, map.which_groups, map.groups, map.ctrls
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_is_modular() {
        assert_eq!(wrap(-1, 2), 1);
        assert_eq!(wrap(2, 2), 0);
        assert_eq!(wrap(1, 1), 0);
        let _ = SA_SET_MODS;
    }
}
