//! The model against Xorg: the pristine descriptions, the writer invariant
//! and the cooking gate.

use super::*;
use crate::kms::xkb::golden_keymap;

/// The four frozen fixtures, as `golden_keymap` keys them, with their
/// `xorg-xkb-pristine.txt` case names.
pub(crate) const FIXTURES: [(&str, Option<&str>, &str); 4] = [
    ("us", None, "us"),
    ("gb", None, "gb"),
    ("de", None, "de"),
    ("us,ru", Some("grp:alt_shift_toggle"), "us,ru"),
];

pub(crate) fn seeded(layout: &str, options: Option<&str>) -> XkbDesc {
    XkbDesc::from_keymap(&golden_keymap(layout, options)).expect("seed")
}

/// Xorg's whole-description lines for one pristine case (`= ` stripped).
pub(crate) fn pristine_lines(case: &str) -> Vec<String> {
    let text = include_str!("../testdata/xorg-xkb-pristine.txt");
    let header = format!("## pristine layout={case} ");
    let mut lines = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        if line.starts_with("## ") {
            inside = line.starts_with(&header);
            continue;
        }
        if !inside {
            continue;
        }
        if let Some(l) = line.strip_prefix("= ") {
            lines.push(l.to_owned());
        } else if line.starts_with("coremodmap") {
            lines.push(line.to_owned());
        }
    }
    assert!(!lines.is_empty(), "pristine case {case} not found");
    lines
}

/// The listed seed tolerances of the pristine comparison (plan §4.2 /
/// review outcome (4)); everything else must be byte-for-byte Xorg's line.
/// Each is a place where xkbcommon's compile (or its dump) no longer carries
/// what xkbcomp's does, so the seed can't reproduce it:
///
/// - `enabledControls` in the `keys` line: GetControls' enabled controls
///   aren't part of the model (not in 4a's scope; yserver reports
///   RepeatKeys only).
/// - the `geometry` line: geometry is name-only by decision (open question
///   1); GetGeometry keeps answering found=False.
/// - type 9 (`CTRL+ALT`): xkbcomp re-adds the level-1 entries a preserve
///   needs in `preserve[]` *definition* order (Alt before Control in
///   xkeyboard-config's types/pc); xkbcommon's dump keeps preserves in
///   entry order, so those two trailing entries come out swapped. The
///   entries (each with its preserve) are compared as a set; the level
///   selection is the same either way, since their masks differ.
/// - a key whose only symbol is `NoSymbol` (inet(evdev)'s `<I248>`,
///   KEY_UNKNOWN): xkbcomp keeps one ONE_LEVEL group of `NoSymbol`,
///   xkbcommon drops the group. Cooks the same (nothing).
/// - the `Overlay1_Enable` / `Overlay2_Enable` interprets: xkbcommon has no
///   overlay controls and dumps `LockControls(controls=none)`.
/// - key alias order after an alias is overridden (`aliases(qwertz)`'s
///   LatY/LatZ on de): the aliases are compared as a set.
/// - a multi-group key typed explicitly in only some groups, all of the
///   same type (us,ru `<KPDL>`: KEYPAD, explicit in group 2 only):
///   xkbcommon's dump writes one `type=` for all groups, so the seed marks
///   every group explicit.
pub(crate) fn tolerated(xorg: &str, ours: &str) -> bool {
    let key = |l: &str| l.split(' ').take(2).collect::<Vec<_>>().join(" ");
    if key(xorg) != key(ours) {
        return xorg.starts_with("alias ") && ours.starts_with("alias ");
    }
    if xorg.starts_with("keys ") {
        let strip = |l: &str| l.split(" enabledControls").next().unwrap_or("").to_owned();
        return strip(xorg) == strip(ours);
    }
    if xorg.starts_with("geometry ") || xorg.starts_with("alias ") {
        return true;
    }
    if xorg.starts_with("type 9 ") {
        return super::probe::type_entry_set(xorg) == super::probe::type_entry_set(ours);
    }
    if xorg.starts_with("key ") {
        let nosym = xorg.replace(" gi=0x01 w=1 syms=0 ", " gi=0x00 w=0 syms=- ");
        if xorg.contains(" kt=0,0,0,0 gi=0x01 w=1 syms=0 acts=- ") && nosym == ours {
            return true;
        }
        let field = |l: &str, f: &str| {
            l.split(' ')
                .find_map(|t| t.strip_prefix(f))
                .map(str::to_owned)
                .unwrap_or_default()
        };
        let without = |l: &str| {
            l.split(' ')
                .filter(|t| !t.starts_with("expl="))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let kt = field(xorg, "kt=");
        let kts: Vec<&str> = kt.split(',').collect();
        let groups = u8::from_str_radix(field(xorg, "gi=0x").as_str(), 16).unwrap_or(0) & 0x0f;
        let hex = |v: String| u8::from_str_radix(v.trim_start_matches("0x"), 16).unwrap_or(0);
        let (xe, oe) = (hex(field(xorg, "expl=")), hex(field(ours, "expl=")));
        return without(xorg) == without(ours)
            && groups > 1
            && kts[..usize::from(groups)].iter().all(|t| *t == kts[0])
            && xe & !oe == 0
            && (oe & !xe) & !0x0f == 0;
    }
    if xorg.starts_with("si ") {
        let act = |l: &str| l.rsplit("act=").next().unwrap_or("").to_owned();
        let (xa, oa) = (act(xorg), act(ours));
        let head = |l: &str| l.rsplit_once(" act=").map_or("", |(h, _)| h).to_owned();
        return head(xorg) == head(ours)
            && (xa == "0f00000004000000" || xa == "0f00000008000000")
            && oa == "0f00000000000000";
    }
    false
}

fn alias_set(lines: &[String]) -> Vec<String> {
    let mut set: Vec<String> = lines
        .iter()
        .filter(|l| l.starts_with("alias "))
        .map(|l| l.splitn(3, ' ').nth(2).unwrap_or("").to_owned())
        .collect();
    set.sort();
    set
}

/// Golden (`xorg-xkb-pristine.txt`, Xvfb 21.1.24 + xkeyboard-config 2.48):
/// the seeded model, read back through our GetMap / GetControls /
/// GetCompatMap / GetIndicatorMap / GetNames encoders and core
/// GetModifierMapping, is Xorg's whole description after `setxkbmap`, for
/// us, gb, de and us,ru, line for line, but for the listed tolerances.
#[test]
fn seeded_model_matches_xorg_pristine() {
    let mut failures = Vec::new();
    for (layout, options, case) in FIXTURES {
        let desc = seeded(layout, options);
        let ours = super::probe::full_lines(&desc);
        let xorg = pristine_lines(case);
        if alias_set(&xorg) != alias_set(&ours) {
            failures.push(format!("{case}: key aliases differ as a set"));
        }
        let n = xorg.len().max(ours.len());
        for i in 0..n {
            let (x, o) = (xorg.get(i), ours.get(i));
            match (x, o) {
                (Some(x), Some(o)) if x == o || tolerated(x, o) => {}
                _ => failures.push(format!(
                    "{case}: line {i}\n  xorg {}\n  ours {}",
                    x.map_or("-", String::as_str),
                    o.map_or("-", String::as_str)
                )),
            }
        }
    }
    let shown: Vec<&String> = failures.iter().take(2000).collect();
    assert!(
        failures.is_empty(),
        "{} differences:\n{}",
        failures.len(),
        shown
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// The first `n` differences between two line lists, for a failure message.
pub(super) fn line_diff(a: &[String], b: &[String], n: usize) -> Vec<String> {
    let mut out = Vec::new();
    for i in 0..a.len().max(b.len()) {
        if a.get(i) != b.get(i) {
            out.push(format!(
                "line {i}\n  - {}\n  + {}",
                a.get(i).map_or("-", String::as_str),
                b.get(i).map_or("-", String::as_str)
            ));
            if out.len() == n {
                break;
            }
        }
    }
    out
}

/// Writer invariant (§4.11 4b): seed → `to_v1_text` → compile → re-seed
/// gives the same model, in everything that cooks (the cooking keymap has
/// synthetic names and no interprets by design, so names and the compat
/// map can't survive; see `gate::cooking_view`).
#[test]
fn writer_round_trip_keeps_the_cooking_view() {
    for (layout, options, case) in FIXTURES {
        let desc = seeded(layout, options);
        let (text, uncooked) = desc.to_v1_text();
        assert!(
            uncooked.is_empty(),
            "{case}: nothing uncookable in the defaults"
        );
        let reseeded = XkbDesc::from_keymap(&gate::compile(&text)).expect("reseed");
        let (a, b) = (gate::cooking_view(&desc), gate::cooking_view(&reseeded));
        let diff = line_diff(&a, &b, 10);
        assert!(diff.is_empty(), "{case}:\n{}", diff.join("\n"));
    }
}

/// The cooking gate on the seeded fixtures: the compiled cooking keymap
/// cooks every key, group and level as the model prescribes.
#[test]
fn seeded_cooking_keymap_cooks_as_the_model() {
    for (layout, options, case) in FIXTURES {
        let desc = seeded(layout, options);
        let km = gate::compile(&desc.to_v1_text().0);
        let (bad, excluded) = gate::check(&desc, &km);
        assert!(
            bad.is_empty(),
            "{case}: {} mismatches:\n{}",
            bad.len(),
            bad.iter().take(30).cloned().collect::<Vec<_>>().join("\n")
        );
        assert!(
            excluded.uncooked_actions.is_empty()
                && excluded.behaviors.is_empty()
                && excluded.keysyms.is_empty(),
            "{case}: nothing excluded in the defaults: {excluded:?}"
        );
    }
}

/// One captured whole state (Xorg's lines keyed as the probe keys them):
/// the pristine gb description with a case's deltas applied.
pub(crate) struct CapturedState {
    pub(crate) lines: std::collections::BTreeMap<String, String>,
}

impl CapturedState {
    pub(crate) fn key(line: &str) -> String {
        let mut words = line.split(' ');
        let first = words.next().unwrap_or("");
        match first {
            "vmods" | "repeat" | "keys" => first.to_owned(),
            _ => format!("{first} {}", words.next().unwrap_or("")),
        }
    }

    pub(crate) fn new(pristine: &[String]) -> Self {
        Self {
            lines: pristine
                .iter()
                .filter(|l| !l.starts_with("coremodmap"))
                .map(|l| (Self::key(l), l.clone()))
                .collect(),
        }
    }

    /// Apply one delta line of `xorg-xkbcomp-steps.txt` (the probe's `-` /
    /// `+` lines, `repeat KC A->B`, `ntypes A->B`); the rest (events,
    /// results) is ignored.
    pub(crate) fn apply(&mut self, line: &str) {
        if let Some(change) = line.strip_prefix("ntypes ") {
            let (_, to) = change.split_once("->").expect("ntypes line");
            let keys = self.lines.get("keys").expect("keys row").clone();
            let mut words: Vec<String> = keys.split(' ').map(str::to_owned).collect();
            words[3] = to.to_owned();
            self.lines.insert("keys".into(), words.join(" "));
            return;
        }
        if let Some(rest) = line.strip_prefix("+ ") {
            let kc = rest.split(' ').next().unwrap_or("");
            self.lines
                .insert(format!("key {kc}"), format!("key {rest}"));
        } else if line.starts_with("- ") {
            // The old row; its `+` follows.
        } else if let Some(rest) = line.strip_prefix('+') {
            self.lines.insert(Self::key(rest), rest.to_owned());
        } else if let Some(rest) = line.strip_prefix('-') {
            self.lines.remove(&Self::key(rest));
        } else if let Some(rest) = line.strip_prefix("repeat ") {
            let (kc, change) = rest.split_once(' ').expect("repeat line");
            let kc: usize = kc.parse().expect("keycode");
            let on = change.ends_with("->1");
            let bits = self.lines.get("repeat").expect("repeat row").clone();
            let mut bytes: Vec<u8> = (0..32)
                .map(|i| u8::from_str_radix(&bits[7 + 2 * i..9 + 2 * i], 16).unwrap())
                .collect();
            if on {
                bytes[kc >> 3] |= 1 << (kc & 7);
            } else {
                bytes[kc >> 3] &= !(1 << (kc & 7));
            }
            let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            self.lines.insert("repeat".into(), format!("repeat {hex}"));
        }
    }

    /// The model this state is. The captured lines carry everything that
    /// cooks except the resolved masks of the types, their entries and
    /// preserves, which are computed as Xorg's `SetKeyTypes` does
    /// (`real | XkbVirtualModsToReal(vmods)`).
    pub(crate) fn desc(&self) -> XkbDesc {
        let mut d = XkbDesc::empty();
        let hex8 = |v: &str| u8::from_str_radix(v, 16).unwrap();
        let hex16 = |v: &str| u16::from_str_radix(v, 16).unwrap();
        let field = |l: &str, k: &str| -> String {
            l.split(' ')
                .find_map(|t| t.strip_prefix(k))
                .map(str::to_owned)
                .unwrap_or_default()
        };
        if let Some(v) = self.lines.get("vmods") {
            for (i, h) in v[6..].split(',').enumerate() {
                d.vmods[i] = hex8(h);
            }
        }
        if let Some(r) = self.lines.get("repeat") {
            for i in 0..32 {
                d.per_key_repeat[i] = hex8(&r[7 + 2 * i..9 + 2 * i]);
            }
        }
        let mods = |rv: &str, d: &XkbDesc| {
            let (r, v) = rv.split_once('/').unwrap();
            let (r, v) = (hex8(r), hex16(v));
            Mods {
                mask: r | d.vmods_to_real(v),
                real: r,
                vmods: v,
            }
        };
        let mut types: Vec<(usize, KeyType)> = Vec::new();
        for (k, l) in &self.lines {
            if let Some(n) = k.strip_prefix("type ") {
                let tmods = mods(&field(l, "mods="), &d);
                let list = |key: &str| -> Vec<String> {
                    l.split(key)
                        .nth(1)
                        .and_then(|r| r.split(']').next())
                        .map(|r| {
                            r.split(' ')
                                .filter(|s| !s.is_empty())
                                .map(str::to_owned)
                                .collect()
                        })
                        .unwrap_or_default()
                };
                let map = list("map=[")
                    .iter()
                    .map(|e| {
                        let (active, rest) = e.split_once(':').unwrap();
                        let (m, level) = rest.split_once("->").unwrap();
                        KtEntry {
                            active: active == "1",
                            mods: mods(m, &d),
                            level: level.parse().unwrap(),
                        }
                    })
                    .collect();
                let pre = list("pre=[");
                types.push((
                    n.parse().unwrap(),
                    KeyType {
                        mods: tmods,
                        num_levels: field(l, "lv=").parse().unwrap(),
                        map,
                        preserve: (!pre.is_empty())
                            .then(|| pre.iter().map(|p| mods(p, &d)).collect()),
                        name: None,
                        level_names: None,
                    },
                ));
            }
        }
        types.sort_by_key(|(n, _)| *n);
        d.types = types.into_iter().map(|(_, t)| t).collect();
        for (k, l) in &self.lines {
            if let Some(kc) = k.strip_prefix("key ") {
                let kc: usize = kc.parse().unwrap();
                let kt: Vec<u8> = field(l, "kt=")
                    .split(',')
                    .map(|t| t.parse().unwrap())
                    .collect();
                let syms = field(l, "syms=");
                let acts = field(l, "acts=");
                d.keys[kc] = KeySyms {
                    kt_index: [kt[0], kt[1], kt[2], kt[3]],
                    group_info: hex8(field(l, "gi=0x").as_str()),
                    width: field(l, "w=").parse().unwrap(),
                    syms: if syms == "-" {
                        Vec::new()
                    } else {
                        syms.split(',')
                            .map(|s| u32::from_str_radix(s, 16).unwrap())
                            .collect()
                    },
                };
                d.acts[kc] = (acts != "-").then(|| {
                    acts.split(',')
                        .map(|a| std::array::from_fn(|i| hex8(&a[2 * i..2 * i + 2])))
                        .collect()
                });
                let (bt, bd) = field(l, "beh=")
                    .split_once(':')
                    .map(|(t, v)| (hex8(t), hex8(v)))
                    .unwrap();
                d.behaviors[kc] = Behavior { kind: bt, data: bd };
                d.explicit[kc] = hex8(field(l, "expl=0x").as_str());
                d.modmap[kc] = hex8(field(l, "mm=0x").as_str());
                d.vmodmap[kc] = hex16(field(l, "vmm=0x").as_str());
            } else if let Some(i) = k.strip_prefix("indmap ") {
                let i: usize = i.parse().unwrap();
                d.indicators[i] = IndicatorMap {
                    flags: hex8(&field(l, "flags=")),
                    which_groups: hex8(&field(l, "whichGroups=")),
                    groups: hex8(&field(l, "groups=")),
                    which_mods: hex8(&field(l, "whichMods=")),
                    mods: Mods {
                        mask: hex8(&field(l, "mods=")),
                        real: hex8(&field(l, "realMods=")),
                        vmods: hex16(&field(l, "vmods=")),
                    },
                    ctrls: u32::from_str_radix(&field(l, "ctrls="), 16).unwrap(),
                };
            } else if let Some(i) = k.strip_prefix("indname ") {
                let i: usize = i.parse().unwrap();
                d.names.indicators[i] = l
                    .split_once('\'')
                    .and_then(|(_, r)| r.strip_suffix('\''))
                    .map(str::to_owned);
            } else if k == "controls -" {
                d.num_groups = field(l, "numGroups=").parse().unwrap();
            }
        }
        d
    }
}

/// The cooking gate over every state Xorg reached in the nine captured
/// xkbcomp uploads (`xorg-xkbcomp-steps.txt`, 45 requests): each captured
/// post-request state, loaded into the model as it is, written, compiled,
/// cooks every key, group and level as that state prescribes.
#[test]
fn captured_xkbcomp_states_cook_as_captured() {
    let steps = include_str!("../testdata/xorg-xkbcomp-steps.txt");
    let pristine = pristine_lines("gb");
    let mut state: Option<CapturedState> = None;
    let mut pending: Option<String> = None;
    let mut checked = 0;
    let mut failures: Vec<String> = Vec::new();
    let mut finish = |state: &Option<CapturedState>, pending: &mut Option<String>| {
        let (Some(st), Some(what)) = (state, pending.take()) else {
            return;
        };
        let desc = st.desc();
        let (text, _) = desc.to_v1_text();
        let km = gate::compile(&text);
        let (bad, excluded) = gate::check(&desc, &km);
        failures.extend(bad.into_iter().take(10).map(|b| format!("{what}: {b}")));
        assert!(
            excluded.uncooked_actions.is_empty() && excluded.behaviors.is_empty(),
            "{what}: nothing uncookable in the captured uploads: {excluded:?}"
        );
        checked += 1;
    };
    for line in steps.lines() {
        if line.starts_with("## case ") {
            finish(&state, &mut pending);
            state = Some(CapturedState::new(&pristine));
        } else if let Some(req) = line.strip_prefix("> xreq:") {
            finish(&state, &mut pending);
            pending = Some(req.to_owned());
        } else if line.starts_with("> ") {
            finish(&state, &mut pending);
        } else if pending.is_some()
            && let Some(st) = state.as_mut()
        {
            st.apply(line);
        }
    }
    finish(&state, &mut pending);
    assert_eq!(checked, 45, "every captured request");
    assert!(
        failures.is_empty(),
        "{} mismatches:\n{}",
        failures.len(),
        failures
            .iter()
            .take(40)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// What xkbcommon can't carry (§4.6) is stored and read back exactly,
/// written as `NoAction()` / left out of the cooking keymap, reported for
/// the install's warning, and left out of the cooking gate by name.
#[test]
fn uncookable_actions_and_behaviors_are_stored_and_reported() {
    let mut desc = seeded("us", None);
    // <AC01> (38): a RedirectKey on level 1, a radio-group behavior.
    let redirect: Action = [SA_REDIRECT_KEY, 0x26, 0, 0, 0, 0, 0, 0];
    let mut acts = vec![[0u8; 8]; desc.keys[38].num_syms()];
    acts[0] = redirect;
    desc.acts[38] = Some(acts);
    desc.explicit[38] |= EXPLICIT_INTERPRET;
    desc.behaviors[38] = Behavior {
        kind: 0x02,
        data: 1,
    };
    let (text, uncooked) = desc.to_v1_text();
    assert_eq!(uncooked.actions, vec![(38, 0, redirect)]);
    assert_eq!(uncooked.behaviors, vec![(38, 0x02, 1)]);
    let km = gate::compile(&text);
    let (bad, excluded) = gate::check(&desc, &km);
    assert!(bad.is_empty(), "{bad:?}");
    assert_eq!(excluded.uncooked_actions, vec![(38, 0, redirect)]);
    assert_eq!(excluded.behaviors, vec![38]);
    // Read back exactly.
    let map = reply::encode_map(&desc, reply::MapRequest::full(&desc));
    let (_, _, keys) = probe::map_lines(&map);
    let row = &keys.iter().find(|(kc, _)| *kc == 38).expect("38").1;
    assert!(
        row.contains(" acts=1126000000000000,0000000000000000 "),
        "{row}"
    );
    assert!(row.contains(" beh=02:01 "), "{row}");
}

/// Every layout of the host's xkeyboard-config (and a few with options)
/// seeds, writes a cooking keymap xkbcommon compiles, and that keymap cooks
/// as the model: the writer is general, not tuned to the four fixtures.
/// Passes with libxkbcommon 1.13; 1.6 reports `et`'s `<I219>`
/// (`LockGroup(group=-1)` on a one-layout keymap): 1.6 wraps the negative
/// locked group to 1 until the next state update, the RMLVO keymap alike.
#[test]
#[ignore = "exploratory: iterates the host's xkeyboard-config; YSERVER_EXPLORATORY=1"]
fn every_host_layout_cooks_as_the_model() {
    // CI runs every ignored lib test (`--ignored`, for the lavapipe render
    // tests); this one needs host tools/data, so it only runs on request.
    if std::env::var_os("YSERVER_EXPLORATORY").is_none() {
        eprintln!("skipped: set YSERVER_EXPLORATORY=1 to run");
        return;
    }
    let ctx = xkbcommon::xkb::Context::new(xkbcommon::xkb::CONTEXT_NO_FLAGS);
    let layouts: Vec<String> = std::fs::read_dir("/usr/share/X11/xkb/symbols")
        .map(|d| {
            d.filter_map(Result::ok)
                .filter(|e| e.path().is_file())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    let mut failures = Vec::new();
    let mut checked = 0;
    for layout in &layouts {
        for options in [
            None,
            Some("lv3:ralt_switch,grp:alt_shift_toggle,caps:ctrl_modifier"),
        ] {
            let Some(km) = xkbcommon::xkb::Keymap::new_from_names(
                &ctx,
                "evdev",
                "pc105",
                layout,
                "",
                options.map(str::to_owned),
                xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
            ) else {
                continue;
            };
            let desc = XkbDesc::from_keymap(&km).expect("seed");
            let (text, _) = desc.to_v1_text();
            let ctx2 = xkbcommon::xkb::Context::new(xkbcommon::xkb::CONTEXT_NO_FLAGS);
            let Some(cooking) = xkbcommon::xkb::Keymap::new_from_string(
                &ctx2,
                text,
                xkbcommon::xkb::KEYMAP_FORMAT_TEXT_V1,
                xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
            ) else {
                failures.push(format!(
                    "{layout} {options:?}: cooking keymap doesn't compile"
                ));
                continue;
            };
            let (bad, _) = gate::check(&desc, &cooking);
            failures.extend(
                bad.into_iter()
                    .take(3)
                    .map(|b| format!("{layout} {options:?}: {b}")),
            );
            checked += 1;
        }
    }
    assert!(checked > 50, "checked {checked}");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The KeyTypeNames/KTLevelNames/VirtualModNames GetNames reply (what
/// xdotool's libxdo asks for) decoded by libX11's rules: every type name
/// and every level name of a type with level names, as atoms.
fn names_atoms(desc: &XkbDesc, which: u32) -> Vec<String> {
    let mut atoms = super::probe::Atoms::default();
    let mut body = [0u8; 8];
    body[4..8].copy_from_slice(&which.to_le_bytes());
    let r = reply::reply_get_names(desc, &body, &mut |n| atoms.intern(n)).expect("names");
    super::probe::names_lines(&r, &atoms)
}

/// The startup keymap of a real session (this host's
/// /etc/X11/xorg.conf.d/00-keyboard.conf: evdev / pc105+inet / us /
/// terminate:ctrl_alt_bksp) and the pristine fixtures: GetNames carries a
/// real (non-None) atom wherever Xorg's `xorg-xkb-pristine.txt` does —
/// every type name, every level name, every vmod name. libxdo (xdotool)
/// `XGetAtomName`s every type name and dies on BadAtom for a None.
#[test]
fn get_names_carries_real_atoms_where_xorg_does() {
    let ctx = xkbcommon::xkb::Context::new(xkbcommon::xkb::CONTEXT_NO_FLAGS);
    let startup = xkbcommon::xkb::Keymap::new_from_names(
        &ctx,
        "evdev",
        "pc105+inet",
        "us",
        "",
        Some("terminate:ctrl_alt_bksp".to_owned()),
        xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .expect("startup keymap");
    let mut descs = vec![(
        "startup".to_owned(),
        XkbDesc::from_keymap(&startup).expect("seed"),
    )];
    for (layout, options, case) in FIXTURES {
        descs.push((case.to_owned(), seeded(layout, options)));
    }
    // The server's own startup path (KmsCore), whatever it resolved.
    descs.push((
        "KmsCore::for_tests".to_owned(),
        crate::kms::core::KmsCore::for_tests().xkb_desc,
    ));
    let xorg: Vec<String> = pristine_lines("us")
        .into_iter()
        .filter(|l| l.starts_with("typename ") || l.starts_with("levelnames "))
        .collect();
    assert!(
        xorg.iter().all(|l| !l.contains("None")),
        "Xorg names every type and level"
    );
    for (what, desc) in descs {
        let lines = names_atoms(&desc, 0x08c0);
        let typenames = lines.iter().filter(|l| l.starts_with("typename ")).count();
        assert_eq!(typenames, desc.types.len(), "{what}: one name per type");
        for l in &lines {
            if l.starts_with("typename ")
                || l.starts_with("levelnames ")
                || l.starts_with("vmodname ")
            {
                assert!(!l.contains("None"), "{what}: {l}");
            }
            if let Some(rest) = l.strip_prefix("levelnames ") {
                let (n, _) = rest.split_once(' ').unwrap();
                let t = &desc.types[n.parse::<usize>().unwrap()];
                assert!(
                    rest.contains(&format!(" n={} ", t.num_levels)),
                    "{what}: level names for every level of type {n}: {l}"
                );
            }
        }
    }
}

/// libX11's `_XkbReadGetNamesReply` for the KeyTypeNames / KTLevelNames /
/// VirtualModNames parts, over a map read by `_XkbReadKeyTypes` from our
/// GetMap: `(type names, level names per type, vmod names)`, or where it
/// bails out.
#[allow(clippy::type_complexity)]
pub(crate) fn libx11_names(
    map: &[u8],
    names: &[u8],
) -> Result<(Vec<u32>, Vec<Vec<u32>>, [u32; 16]), String> {
    let u32c = |b: &[u8], o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    // _XkbReadKeyTypes: each type's num_levels.
    let mut levels = Vec::new();
    let mut p = 40;
    for _ in 0..map[15] {
        let n = usize::from(map[p + 5]);
        levels.push(map[p + 4]);
        p += 8 + 8 * n + if map[p + 6] != 0 { 4 * n } else { 0 };
    }
    let which = u32c(names, 8);
    if which & !0x08c0 != 0 {
        return Err(format!(
            "reply which {which:#x} carries parts xdotool didn't ask for"
        ));
    }
    let n_types = usize::from(names[14]);
    let vmods = u16::from_le_bytes([names[16], names[17]]);
    let len = 4 * u32c(names, 4) as usize;
    let body = &names[32..];
    if body.len() != len {
        return Err(format!("length {len}, body {}", body.len()));
    }
    let mut p = 0;
    let take = |p: &mut usize, n: usize| -> Result<usize, String> {
        let at = *p;
        if at + n > len {
            return Err(format!("read past the reply at {at}+{n} of {len}"));
        }
        *p += n;
        Ok(at)
    };
    for bit in 0..6 {
        if which & (1 << bit) != 0 {
            take(&mut p, 4)?;
        }
    }
    let mut type_names = Vec::new();
    if which & 0x40 != 0 {
        for _ in 0..n_types {
            let at = take(&mut p, 4)?;
            type_names.push(u32c(body, at));
        }
    }
    let mut level_names = Vec::new();
    if which & 0x80 != 0 {
        let at = take(&mut p, n_types.div_ceil(4) * 4)?;
        let nl: Vec<u8> = body[at..at + n_types].to_vec();
        for (i, &n) in nl.iter().enumerate() {
            if n > 0 && Some(&n) != levels.get(i) {
                return Err(format!(
                    "BAILOUT: type {i} has {n} level names, {:?} levels",
                    levels.get(i)
                ));
            }
            let mut v = Vec::new();
            for _ in 0..n {
                let at = take(&mut p, 4)?;
                v.push(u32c(body, at));
            }
            level_names.push(v);
        }
    }
    // XKB.h: IndicatorNames 1<<8, KeyNames 1<<9, KeyAliases 1<<10,
    // VirtualModNames 1<<11, GroupNames 1<<12; the body carries them in
    // XkbSendNames' order: indicators, vmods, groups, keys, aliases.
    if which & (1 << 8) != 0 {
        let inds = u32c(names, 20);
        take(&mut p, 4 * inds.count_ones() as usize)?;
    }
    let mut vmod_names = [0u32; 16];
    if which & (1 << 11) != 0 {
        for (i, slot) in vmod_names.iter_mut().enumerate() {
            if vmods & (1 << i) != 0 {
                let at = take(&mut p, 4)?;
                *slot = u32c(body, at);
            }
        }
    }
    if p != len {
        return Err(format!("{} bytes left undecoded", len - p));
    }
    Ok((type_names, level_names, vmod_names))
}

/// xdotool (libxdo) reads the key type names, their level names and the
/// virtual modifier names (GetNames `XkbKeyTypeNamesMask |
/// XkbKTLevelNamesMask | XkbVirtualModNamesMask` = 0x8c0) over the type table of its
/// GetMap(XkbAllClientInfoMask), and `XGetAtomName`s the vmod names its
/// types' entries use: libX11 must decode every part, and every name a
/// type entry or vmod needs must be a real atom, as on Xorg (the
/// `xorg-xkb-pristine.txt` names). Covers this host's startup RMLVO
/// (evdev / pc105+inet / us / terminate:ctrl_alt_bksp), the fixtures and
/// the server's own startup keymap.
#[test]
fn libx11_decodes_the_names_xdotool_asks_for() {
    let ctx = xkbcommon::xkb::Context::new(xkbcommon::xkb::CONTEXT_NO_FLAGS);
    let startup = xkbcommon::xkb::Keymap::new_from_names(
        &ctx,
        "evdev",
        "pc105+inet",
        "us",
        "",
        Some("terminate:ctrl_alt_bksp".to_owned()),
        xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .expect("startup keymap");
    let mut descs = vec![(
        "startup".to_owned(),
        XkbDesc::from_keymap(&startup).expect("seed"),
    )];
    for (layout, options, case) in FIXTURES {
        descs.push((case.to_owned(), seeded(layout, options)));
    }
    descs.push((
        "KmsCore::for_tests".to_owned(),
        crate::kms::core::KmsCore::for_tests().xkb_desc,
    ));
    // After the xmodmap caps-as-control script (the vng scenario's order:
    // remove Lock = Caps_Lock, keycode 66 = Control_L, add Control =
    // Control_L), on each of them.
    let mutated: Vec<(String, XkbDesc)> = descs
        .iter()
        .map(|(what, d)| {
            let mut d = d.clone();
            let mut modmap = [0u8; 256];
            modmap.copy_from_slice(&d.modmap);
            modmap[66] = 0;
            let _ = d.apply_modifier_mapping(&modmap);
            let _ = d.apply_keyboard_mapping(66, 1, 1, &[0xffe3]);
            modmap[66] = 0x04;
            let _ = d.apply_modifier_mapping(&modmap);
            (format!("{what} after xmodmap"), d)
        })
        .collect();
    descs.extend(mutated);
    for (what, desc) in descs {
        let mut body = [0u8; 20];
        body[2] = 0x07; // full = XkbAllClientInfoMask
        let map = reply::reply_get_map(&desc, &body).expect("map");
        let mut names_req = [0u8; 8];
        names_req[4..8].copy_from_slice(&0x08c0u32.to_le_bytes());
        let mut atoms = super::probe::Atoms::default();
        let names =
            reply::reply_get_names(&desc, &names_req, &mut |n| atoms.intern(n)).expect("names");
        let (types, levels, vmods) =
            libx11_names(&map, &names).unwrap_or_else(|e| panic!("{what}: {e}"));
        assert!(
            types.iter().all(|&a| a != 0),
            "{what}: type names {types:?}"
        );
        for (i, t) in desc.types.iter().enumerate() {
            assert!(
                levels[i].iter().all(|&a| a != 0),
                "{what}: level names of type {i}"
            );
            let used = t.mods.vmods | t.map.iter().fold(0, |m, e| m | e.mods.vmods);
            for (v, &atom) in vmods.iter().enumerate() {
                if used & (1 << v) != 0 {
                    assert_ne!(atom, 0, "{what}: type {i} uses vmod {v}, which has no name");
                }
            }
        }
    }
}

/// SetNames can give two indicators one name (Xorg keeps its indicators by
/// index; the names are only labels). The cooking keymap still lights each
/// by its own map: the later duplicate is written under a synthetic name,
/// so xkbcommon doesn't merge the two.
#[test]
fn duplicate_indicator_names_keep_their_own_maps() {
    let mut desc = seeded("gb", None);
    assert_eq!(desc.names.indicators[0].as_deref(), Some("Caps Lock"));
    desc.names.indicators[5] = Some("Caps Lock".to_owned());
    desc.indicators[5] = IndicatorMap {
        which_mods: 0x04, // locked
        mods: Mods {
            mask: 0x01,
            real: 0x01,
            vmods: 0,
        },
        ..IndicatorMap::default()
    };
    let keymap = super::gate::compile(&desc.to_v1_text().0);
    let mut state = xkbcommon::xkb::State::new(&keymap);
    state.update_mask(0, 0, 0x02, 0, 0, 0);
    // (Other indicators, e.g. Shift Lock, follow their own maps.)
    let both = |state: &xkbcommon::xkb::State| desc.indicators_lit(state) & 0x21;
    assert_eq!(both(&state), 0x01, "locked Lock: indicator 0");
    state.update_mask(0, 0, 0x01, 0, 0, 0);
    assert_eq!(both(&state), 0x20, "locked Shift: indicator 5");
}
