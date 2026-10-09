//! Test-list snapshots (`split test-list`) and `split verify --tests`.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    process::Command,
};

use proc_macro2::{TokenStream, TokenTree};

use crate::{
    tree::{self, Res, Source},
    verify::{self, Spec},
};

pub const CONFIGS: [(&str, &str); 3] = [
    ("default", ""),
    ("tcp-transport", "--features tcp-transport"),
    ("xdmcp", "--features xdmcp"),
];

/// `binary<TAB>name` → ignored, from `cargo test -- --list` output.
fn list(repo: &Path, flags: &str, ignored: bool) -> Res<BTreeSet<(String, String)>> {
    let cmd = format!(
        "cargo test --all-targets --locked {flags} -- --list{} 2>&1",
        if ignored { " --ignored" } else { "" }
    );
    let out = Command::new("sh")
        .args(["-c", &cmd])
        .current_dir(repo)
        .output()
        .map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() {
        return Err(format!("`{cmd}` failed:\n{text}"));
    }
    let mut bin = String::new();
    let mut set = BTreeSet::new();
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("Running ") {
            // "unittests src/lib.rs (target/debug/deps/yserver_core-hash)"
            let (what, deps) = rest
                .rsplit_once(" (")
                .ok_or_else(|| format!("odd line: {line}"))?;
            let stem = deps.trim_end_matches(')').rsplit('/').next().unwrap_or("");
            let stem = stem.rsplit_once('-').map_or(stem, |(s, _)| s);
            let kind = match what.strip_prefix("unittests ") {
                Some(p) if p.ends_with("lib.rs") => "lib",
                Some(_) => "bin",
                None => "test",
            };
            bin = format!("{stem}:{kind}");
        } else if let Some(name) = line.strip_suffix(": test") {
            set.insert((bin.clone(), name.to_string()));
        }
    }
    Ok(set)
}

pub fn snapshot(repo: &Path, out: &Path) -> Res<()> {
    std::fs::create_dir_all(out).map_err(|e| e.to_string())?;
    for (name, flags) in CONFIGS {
        let all = list(repo, flags, false)?;
        let ign = list(repo, flags, true)?;
        let mut text = String::new();
        for t in &all {
            let st = if ign.contains(t) { "ignored" } else { "run" };
            text.push_str(&format!("{}\t{}\t{st}\n", t.0, t.1));
        }
        let p = out.join(format!("{name}.tsv"));
        std::fs::write(&p, text).map_err(|e| e.to_string())?;
        println!(
            "{}: {} tests, {} ignored",
            p.display(),
            all.len(),
            ign.len()
        );
    }
    Ok(())
}

fn read(p: &Path) -> Res<Vec<(String, String, String)>> {
    let text = std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
    Ok(text
        .lines()
        .filter_map(|l| {
            let mut it = l.split('\t');
            Some((it.next()?.into(), it.next()?.into(), it.next()?.into()))
        })
        .collect())
}

/// Names after `fn` anywhere in a macro call (`proptest! { fn … }`).
fn macro_fns(tokens: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack: Vec<TokenStream> = vec![tokens.parse().unwrap_or_default()];
    while let Some(ts) = stack.pop() {
        let mut prev_fn = false;
        for tt in ts {
            match &tt {
                TokenTree::Ident(id) => {
                    if prev_fn {
                        out.push(id.to_string());
                    }
                    prev_fn = id == "fn";
                }
                TokenTree::Group(g) => {
                    stack.push(g.stream());
                    prev_fn = false;
                }
                _ => prev_fn = false,
            }
        }
    }
    out
}

/// Old test name → new test name, from the path table.
fn test_map(spec: &Spec, base: &dyn Source) -> Res<BTreeMap<String, String>> {
    let before = tree::load(base, &spec.old_root, &spec.module)?;
    let table = verify::table(spec, &before)?;
    let mut map = BTreeMap::new();
    for l in before.leaves.iter().filter(|l| l.owner.is_none()) {
        let new = &table[&l.okey()];
        let suffix = format!("::{} {}", l.kind, l.name);
        let Some(new_mod) = new.strip_suffix(&suffix) else {
            continue;
        };
        let names = match l.kind {
            "fn" => vec![l.name.clone()],
            "macro_call" => macro_fns(&l.tokens),
            _ => continue,
        };
        for n in names {
            map.insert(format!("{}::{n}", l.module), format!("{new_mod}::{n}"));
        }
    }
    Ok(map)
}

pub fn verify(
    spec: &Spec,
    base: &dyn Source,
    crate_bin: &str,
    before: &Path,
    after: &Path,
) -> Res<bool> {
    let map = test_map(spec, base)?;
    let mut ok = true;
    for (cfg, _) in CONFIGS {
        let b = read(&before.join(format!("{cfg}.tsv")))?;
        let a = read(&after.join(format!("{cfg}.tsv")))?;
        let mapped: BTreeSet<(String, String, String)> = b
            .into_iter()
            .map(|(bin, name, st)| {
                let name = if bin == crate_bin {
                    map.get(&name).cloned().unwrap_or(name)
                } else {
                    name
                };
                (bin, name, st)
            })
            .collect();
        let a: BTreeSet<_> = a.into_iter().collect();
        let mut per_bin: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
        for t in &mapped {
            per_bin.entry(&t.0).or_default().0 += 1;
        }
        for t in &a {
            per_bin.entry(&t.0).or_default().1 += 1;
        }
        let missing: Vec<_> = mapped.difference(&a).collect();
        let extra: Vec<_> = a.difference(&mapped).collect();
        let bins: Vec<String> = per_bin
            .iter()
            .map(|(b, (x, y))| format!("{b} {x}→{y}"))
            .collect();
        println!("{cfg}: {} tests mapped; {}", mapped.len(), bins.join(", "));
        for t in &missing {
            println!("FAIL {cfg}: missing after: {}\t{}\t{}", t.0, t.1, t.2);
        }
        for t in &extra {
            println!("FAIL {cfg}: unexpected after: {}\t{}\t{}", t.0, t.1, t.2);
        }
        ok &= missing.is_empty() && extra.is_empty();
    }
    println!("verify --tests: {}", if ok { "OK" } else { "FAILED" });
    Ok(ok)
}
