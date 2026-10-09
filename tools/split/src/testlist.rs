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

fn git_in(repo: &Path, args: &[&str], index: Option<&Path>) -> Res<String> {
    let mut c = Command::new("git");
    c.args(args).current_dir(repo);
    if let Some(i) = index {
        c.env("GIT_INDEX_FILE", i);
    }
    let out = c.output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Tree id of the working tree (tracked and untracked, `.gitignore` honoured).
pub fn worktree_tree(repo: &Path) -> Res<String> {
    let idx = repo.join(git_in(
        repo,
        &["rev-parse", "--git-path", "split-index"],
        None,
    )?);
    let _ = std::fs::remove_file(&idx);
    let r = git_in(repo, &["add", "-A"], Some(&idx))
        .and_then(|_| git_in(repo, &["write-tree"], Some(&idx)));
    let _ = std::fs::remove_file(&idx);
    r
}

pub fn rev_tree(repo: &Path, rev: &str) -> Res<String> {
    git_in(repo, &["rev-parse", &format!("{rev}^{{tree}}")], None)
}

fn host() -> Res<String> {
    let out = Command::new("rustc")
        .arg("-vV")
        .output()
        .map_err(|e| e.to_string())?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("host: "))
        .map(str::to_string)
        .ok_or_else(|| "rustc -vV: no host".into())
}

pub fn snapshot(repo: &Path, out: &Path) -> Res<()> {
    std::fs::create_dir_all(out).map_err(|e| e.to_string())?;
    let (tree, rev, host) = (
        worktree_tree(repo)?,
        git_in(repo, &["rev-parse", "HEAD"], None)?,
        host()?,
    );
    for (name, flags) in CONFIGS {
        let all = list(repo, flags, false)?;
        let ign = list(repo, flags, true)?;
        if worktree_tree(repo)? != tree {
            return Err("the working tree changed during the snapshot".into());
        }
        let mut text = format!("#\ttree={tree}\trev={rev}\tconfig={name}\thost={host}\n");
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

type Row = (String, String, String);

/// Snapshot rows of one config; the header must name that config.
pub fn read(p: &Path, config: &str) -> Res<(BTreeMap<String, String>, Vec<Row>)> {
    let text = std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
    let mut lines = text.lines();
    let head: BTreeMap<String, String> = lines
        .next()
        .and_then(|l| l.strip_prefix("#\t"))
        .ok_or_else(|| {
            format!(
                "{}: no provenance header (re-run split test-list)",
                p.display()
            )
        })?
        .split('\t')
        .filter_map(|f| f.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    for k in ["tree", "rev", "config", "host"] {
        if !head.contains_key(k) {
            return Err(format!("{}: header lacks {k}", p.display()));
        }
    }
    if head["config"] != config {
        return Err(format!(
            "{}: snapshot of config {}",
            p.display(),
            head["config"]
        ));
    }
    let rows = lines
        .enumerate()
        .map(
            |(i, l)| match l.split('\t').collect::<Vec<_>>().as_slice() {
                [bin, name, st @ ("run" | "ignored")] if !bin.is_empty() && !name.is_empty() => {
                    Ok(((*bin).to_string(), (*name).to_string(), (*st).to_string()))
                }
                _ => Err(format!("{}:{}: malformed row {l:?}", p.display(), i + 2)),
            },
        )
        .collect::<Res<Vec<_>>>()?;
    Ok((head, rows))
}

/// The snapshots in `dir` were taken on this host from a tree equal to
/// `expected` in everything cargo builds.
pub fn fresh(repo: &Path, dir: &Path, expected: &str) -> Res<()> {
    let host = host()?;
    for (cfg, _) in CONFIGS {
        let (head, _) = read(&dir.join(format!("{cfg}.tsv")), cfg)?;
        if head["host"] != host {
            return Err(format!("{}: taken on {}", dir.display(), head["host"]));
        }
        let diff = git_in(
            repo,
            &[
                "diff",
                "--name-only",
                &head["tree"],
                expected,
                "--",
                ".",
                ":(exclude)tools/split",
                ":(exclude)docs",
            ],
            None,
        )?;
        if !diff.is_empty() {
            let files: Vec<&str> = diff.lines().take(5).collect();
            return Err(format!(
                "{}/{cfg}.tsv is stale (rev {}): differs in {}",
                dir.display(),
                head["rev"],
                files.join(", ")
            ));
        }
    }
    Ok(())
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
        let suffix = format!("{} {}", l.kind, l.name);
        let Some(new_mod) = new.strip_suffix(&suffix) else {
            continue;
        };
        let new_mod = new_mod.strip_suffix("::").unwrap_or(new_mod);
        let names = match l.kind {
            "fn" => vec![l.name.clone()],
            "macro_call" => macro_fns(&l.tokens),
            _ => continue,
        };
        for n in names {
            map.insert(tree::child(&l.module, &n), tree::child(new_mod, &n));
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
        let (_, b) = read(&before.join(format!("{cfg}.tsv")), cfg)?;
        let (_, a) = read(&after.join(format!("{cfg}.tsv")), cfg)?;
        let mut into: BTreeMap<(String, String), Vec<(String, String)>> = BTreeMap::new();
        for (bin, name, st) in b {
            let new = if bin == crate_bin {
                map.get(&name).cloned().unwrap_or_else(|| name.clone())
            } else {
                name.clone()
            };
            into.entry((bin, new)).or_default().push((name, st));
        }
        for ((bin, new), olds) in into.iter().filter(|(_, v)| v.len() > 1) {
            let olds: Vec<&str> = olds.iter().map(|(n, _)| n.as_str()).collect();
            println!("FAIL {cfg}: {bin} {} all map to {new}", olds.join(", "));
            ok = false;
        }
        let mapped: BTreeSet<Row> = into
            .into_iter()
            .map(|((bin, new), v)| (bin, new, v[0].1.clone()))
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
