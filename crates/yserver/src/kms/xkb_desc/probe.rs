//! A Rust port of `tools/xkb-mutation-probe.c`'s printer: our XKB replies
//! decoded into the golden files' line grammar. Every decoder must end at
//! the reply length, which also checks the encoders' wire layout.

use std::collections::HashMap;

use super::{XkbDesc, reply};

/// A test atom table: names in, stable ids out, and back.
#[derive(Default)]
pub(crate) struct Atoms {
    by_name: HashMap<String, u32>,
    by_id: HashMap<u32, String>,
}

impl Atoms {
    pub(crate) fn intern(&mut self, name: &str) -> u32 {
        if let Some(&a) = self.by_name.get(name) {
            return a;
        }
        let a = 0x100 + u32::try_from(self.by_name.len()).unwrap();
        self.by_name.insert(name.to_owned(), a);
        self.by_id.insert(a, name.to_owned());
        a
    }

    fn name(&self, a: u32) -> String {
        if a == 0 {
            return "None".to_owned();
        }
        self.by_id
            .get(&a)
            .map_or_else(|| format!("BadAtom(0x{a:x})"), |n| format!("'{n}'"))
    }
}

fn u16c(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn u32c(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn pad4(n: usize) -> usize {
    n.div_ceil(4) * 4
}

/// `take_snap`'s GetMap part: the type lines, the 16 vmods and the key
/// lines, from a full GetMap reply.
pub(crate) fn map_lines(r: &[u8]) -> (Vec<String>, [u8; 16], Vec<(u8, String)>) {
    let (min, max) = (r[10], r[11]);
    let present = u16c(r, 12);
    let n_types = usize::from(r[15]);
    let (first_sym, n_sym) = (r[17], r[20]);
    let (first_act, n_act) = (r[21], usize::from(r[24]));
    let total_beh = usize::from(r[27]);
    let total_expl = usize::from(r[30]);
    let total_mm = usize::from(r[33]);
    let total_vmm = usize::from(r[36]);
    let vmods_mask = u16c(r, 38);
    let mut p = 40;
    let mut types = Vec::new();
    if present & 0x01 != 0 {
        for _ in 0..n_types {
            let nent = usize::from(r[p + 5]);
            let pres = r[p + 6] != 0;
            let mut s = format!(
                "mods={:02x}/{:04x} lv={} map=[",
                r[p + 1],
                u16c(r, p + 2),
                r[p + 4]
            );
            let mut e = p + 8;
            for i in 0..nent {
                s.push_str(&format!(
                    "{}{}:{:02x}/{:04x}->{}",
                    if i > 0 { " " } else { "" },
                    r[e],
                    r[e + 3],
                    u16c(r, e + 4),
                    r[e + 2]
                ));
                e += 8;
            }
            s.push(']');
            if pres {
                s.push_str(" pre=[");
                for i in 0..nent {
                    s.push_str(&format!(
                        "{}{:02x}/{:04x}",
                        if i > 0 { " " } else { "" },
                        r[e + 1],
                        u16c(r, e + 2)
                    ));
                    e += 4;
                }
                s.push(']');
            }
            types.push(s);
            p = e;
        }
    }
    let mut syms: HashMap<u8, String> = HashMap::new();
    let mut acts: HashMap<u8, String> = HashMap::new();
    let mut beh: HashMap<u8, (u8, u8)> = HashMap::new();
    let mut expl: HashMap<u8, u8> = HashMap::new();
    let mut mm: HashMap<u8, u8> = HashMap::new();
    let mut vmm: HashMap<u8, u16> = HashMap::new();
    if present & 0x02 != 0 {
        for i in 0..n_sym {
            let kc = first_sym + i;
            let n = usize::from(u16c(r, p + 6));
            let mut s = format!(
                "kt={},{},{},{} gi=0x{:02x} w={} syms=",
                r[p],
                r[p + 1],
                r[p + 2],
                r[p + 3],
                r[p + 4],
                r[p + 5]
            );
            let list: Vec<String> = (0..n)
                .map(|j| format!("{:x}", u32c(r, p + 8 + 4 * j)))
                .collect();
            s.push_str(if n == 0 { "-" } else { "" });
            s.push_str(&list.join(","));
            syms.insert(kc, s);
            p += 8 + 4 * n;
        }
    }
    if present & 0x10 != 0 {
        let counts = &r[p..p + n_act];
        p += pad4(n_act);
        for (i, &n) in counts.iter().enumerate() {
            let kc = first_act + u8::try_from(i).unwrap();
            let list: Vec<String> = (0..usize::from(n))
                .map(|j| {
                    r[p + 8 * j..p + 8 * j + 8]
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<String>()
                })
                .collect();
            p += 8 * usize::from(n);
            acts.insert(
                kc,
                if n == 0 {
                    "-".to_owned()
                } else {
                    list.join(",")
                },
            );
        }
    }
    if present & 0x20 != 0 {
        for _ in 0..total_beh {
            beh.insert(r[p], (r[p + 1], r[p + 2]));
            p += 4;
        }
    }
    let mut vmods = [0u8; 16];
    if present & 0x40 != 0 {
        let mut j = 0;
        for (i, v) in vmods.iter_mut().enumerate() {
            if vmods_mask & (1 << i) != 0 {
                *v = r[p + j];
                j += 1;
            }
        }
        p += pad4(j);
    }
    if present & 0x08 != 0 {
        for i in 0..total_expl {
            expl.insert(r[p + 2 * i], r[p + 2 * i + 1]);
        }
        p += pad4(2 * total_expl);
    }
    if present & 0x04 != 0 {
        for i in 0..total_mm {
            mm.insert(r[p + 2 * i], r[p + 2 * i + 1]);
        }
        p += pad4(2 * total_mm);
    }
    if present & 0x80 != 0 {
        for _ in 0..total_vmm {
            vmm.insert(r[p], u16c(r, p + 2));
            p += 4;
        }
    }
    assert_eq!(
        p,
        32 + 4 * u32c(r, 4) as usize,
        "GetMap parse ends at the reply length"
    );
    let keys = (min..=max)
        .map(|kc| {
            let (bt, bd) = beh.get(&kc).copied().unwrap_or((0, 0));
            (
                kc,
                format!(
                    "{} acts={} beh={bt:02x}:{bd:02x} expl=0x{:02x} mm=0x{:02x} vmm=0x{:04x}",
                    syms.get(&kc).map_or("nosyms", String::as_str),
                    acts.get(&kc).map_or("-", String::as_str),
                    expl.get(&kc).copied().unwrap_or(0),
                    mm.get(&kc).copied().unwrap_or(0),
                    vmm.get(&kc).copied().unwrap_or(0)
                ),
            )
        })
        .collect();
    (types, vmods, keys)
}

/// `print_full`: the whole description in the pristine golden's grammar.
pub(crate) fn full_lines(desc: &XkbDesc) -> Vec<String> {
    let mut atoms = Atoms::default();
    let map = reply::encode_map(desc, reply::MapRequest::full(desc));
    let ctl = reply::reply_get_controls(desc);
    let (types, vmods, keys) = map_lines(&map);
    let mut out = vec![format!(
        "keys {}..{} ntypes {} enabledControls 0x{:08x}",
        map[10],
        map[11],
        types.len(),
        u32c(&ctl, 56)
    )];
    for (i, t) in types.iter().enumerate() {
        out.push(format!("type {i} {t}"));
    }
    out.push(format!(
        "vmods {}",
        vmods
            .iter()
            .map(|v| format!("{v:02x}"))
            .collect::<Vec<_>>()
            .join(",")
    ));
    out.push(format!(
        "repeat {}",
        desc.per_key_repeat
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    ));
    for (kc, k) in keys {
        out.push(format!("key {kc} {k}"));
    }
    out.push(format!(
        "controls - numGroups={} groupsWrap=0x{:02x}",
        ctl[9], ctl[10]
    ));
    out.extend(compat_lines(&reply::encode_compat_map(
        desc,
        0x0f,
        0,
        desc.compat.len(),
    )));
    out.extend(indicator_lines(&reply::encode_indicator_map(
        desc,
        u32::MAX,
    )));
    let names = reply::encode_names(desc, 0x3fff, &mut |n| atoms.intern(n));
    out.extend(names_lines(&names, &atoms));
    out.push("geometry - (not modelled)".to_owned());
    let (kpm, data) = desc.modifier_mapping();
    let rows: Vec<String> = (0..8)
        .map(|m| {
            let row: Vec<String> = (0..usize::from(kpm))
                .map(|i| data[m * usize::from(kpm) + i].to_string())
                .collect();
            format!("{m}:{}", row.join(","))
        })
        .collect();
    out.push(format!("coremodmap kpm={kpm} {}", rows.join(" ")));
    out
}

pub(crate) fn compat_lines(b: &[u8]) -> Vec<String> {
    let groups = b[8];
    let nsi = usize::from(u16c(b, 12));
    let mut out = vec![format!(
        "compat - groups=0x{groups:02x} firstSI={} nSI={nsi} nTotalSI={}",
        u16c(b, 10),
        u16c(b, 14)
    )];
    let mut p = 32;
    for i in 0..nsi {
        out.push(format!(
            "si {i} sym={:x} mods={:02x} match={:02x} vmod={} flags={:02x} act={}",
            u32c(b, p),
            b[p + 4],
            b[p + 5],
            b[p + 6],
            b[p + 7],
            b[p + 8..p + 16]
                .iter()
                .map(|x| format!("{x:02x}"))
                .collect::<String>()
        ));
        p += 16;
    }
    for g in 0..4 {
        if groups & (1 << g) != 0 {
            out.push(format!(
                "groupcompat {} mask={:02x} real={:02x} vmods={:04x}",
                g + 1,
                b[p],
                b[p + 1],
                u16c(b, p + 2)
            ));
            p += 4;
        }
    }
    assert_eq!(
        p,
        32 + 4 * u32c(b, 4) as usize,
        "GetCompatMap parse ends at the reply length"
    );
    out
}

pub(crate) fn indicator_lines(b: &[u8]) -> Vec<String> {
    let which = u32c(b, 8);
    let mut out = vec![format!(
        "indicators - which=0x{which:08x} realIndicators=0x{:08x} nIndicators={}",
        u32c(b, 12),
        b[16]
    )];
    let mut p = 32;
    for i in 0..32 {
        if which & (1 << i) != 0 {
            out.push(format!(
                "indmap {i} flags={:02x} whichGroups={:02x} groups={:02x} whichMods={:02x} \
                 mods={:02x} realMods={:02x} vmods={:04x} ctrls={:08x}",
                b[p],
                b[p + 1],
                b[p + 2],
                b[p + 3],
                b[p + 4],
                b[p + 5],
                u16c(b, p + 6),
                u32c(b, p + 8)
            ));
            p += 12;
        }
    }
    assert_eq!(
        p,
        32 + 4 * u32c(b, 4) as usize,
        "GetIndicatorMap parse ends at the reply length"
    );
    out
}

/// The GetNames lines. The sections follow `XkbSendNames`' order, each
/// gated by its XKB.h bit (KeyNames 1<<9, KeyAliases 1<<10,
/// VirtualModNames 1<<11, GroupNames 1<<12). (tools/xkb-mutation-probe.c
/// gates them with shifted bits, which only works because it always asks
/// for all of them.)
pub(crate) fn names_lines(b: &[u8], atoms: &Atoms) -> Vec<String> {
    let nw = u32c(b, 8);
    let n_types = usize::from(b[14]);
    let group_names = b[15];
    let vmods = u16c(b, 16);
    let (first_key, n_keys) = (b[18], usize::from(b[19]));
    let inds = u32c(b, 20);
    let (n_rg, n_aliases, n_kt_levels) = (usize::from(b[24]), usize::from(b[25]), u16c(b, 26));
    let mut out = vec![format!(
        "names - which=0x{nw:04x} min={} max={} nTypes={n_types} groupNames=0x{group_names:02x} \
         virtualMods=0x{vmods:04x} keys={first_key}+{n_keys} indicators=0x{inds:08x} \
         nRadioGroups={n_rg} nKeyAliases={n_aliases} nKTLevels={n_kt_levels}",
        b[12], b[13]
    )];
    let mut p = 32;
    let comp = [
        "keycodes",
        "geometry",
        "symbols",
        "phys_symbols",
        "types",
        "compat",
    ];
    for (i, c) in comp.iter().enumerate() {
        if nw & (1 << i) != 0 {
            out.push(format!("name {c} {}", atoms.name(u32c(b, p))));
            p += 4;
        }
    }
    if nw & 0x40 != 0 {
        for t in 0..n_types {
            out.push(format!("typename {t} {}", atoms.name(u32c(b, p))));
            p += 4;
        }
    }
    if nw & 0x80 != 0 {
        let nl: Vec<usize> = b[p..p + n_types].iter().map(|&n| usize::from(n)).collect();
        p += pad4(n_types);
        for (t, &n) in nl.iter().enumerate() {
            let names: Vec<String> = (0..n).map(|l| atoms.name(u32c(b, p + 4 * l))).collect();
            p += 4 * n;
            out.push(format!("levelnames {t} n={n} [{}]", names.join(" ")));
        }
    }
    if nw & 0x100 != 0 {
        for i in 0..32 {
            if inds & (1 << i) != 0 {
                out.push(format!("indname {i} {}", atoms.name(u32c(b, p))));
                p += 4;
            }
        }
    }
    if nw & 0x800 != 0 {
        for i in 0..16 {
            if vmods & (1 << i) != 0 {
                out.push(format!("vmodname {i} {}", atoms.name(u32c(b, p))));
                p += 4;
            }
        }
    }
    if nw & 0x1000 != 0 {
        for i in 0..4 {
            if group_names & (1 << i) != 0 {
                out.push(format!("groupname {} {}", i + 1, atoms.name(u32c(b, p))));
                p += 4;
            }
        }
    }
    let key4 = |s: &[u8]| {
        let end = s.iter().position(|&c| c == 0).unwrap_or(4);
        String::from_utf8_lossy(&s[..end]).into_owned()
    };
    if nw & 0x200 != 0 {
        for k in 0..n_keys {
            out.push(format!(
                "keyname {} '{}'",
                usize::from(first_key) + k,
                key4(&b[p..p + 4])
            ));
            p += 4;
        }
    }
    if nw & 0x400 != 0 {
        for a in 0..n_aliases {
            out.push(format!(
                "alias {a} '{}'->'{}'",
                key4(&b[p + 4..p + 8]),
                key4(&b[p..p + 4])
            ));
            p += 8;
        }
    }
    if nw & 0x2000 != 0 {
        for r in 0..n_rg {
            out.push(format!("rgname {r} {}", atoms.name(u32c(b, p))));
            p += 4;
        }
    }
    assert_eq!(
        p,
        32 + 4 * u32c(b, 4) as usize,
        "GetNames parse ends at the reply length"
    );
    out
}

/// A `type N …` line's entries, each with its preserve, as a sorted set.
pub(crate) fn type_entry_set(line: &str) -> (String, Vec<String>) {
    let head = line.split(" map=").next().unwrap_or("").to_owned();
    let list = |key: &str| -> Vec<String> {
        line.split(key)
            .nth(1)
            .and_then(|r| r.split(']').next())
            .map(|r| r.split(' ').map(str::to_owned).collect())
            .unwrap_or_default()
    };
    let map = list("map=[");
    let pre = list("pre=[");
    let mut set: Vec<String> = map
        .iter()
        .enumerate()
        .map(|(i, e)| format!("{e}|{}", pre.get(i).map_or("", String::as_str)))
        .collect();
    set.sort();
    (head, set)
}
