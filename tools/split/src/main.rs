//! Item-level file splitter and move verifier; see
//! docs/superpowers/plans/2026-10-08-source-layout-cleanup.md (rules 2–4, 7).
//! Paths are repo-relative; run from anywhere inside the repo.

mod apply;
mod scope;
mod testlist;
#[cfg(test)]
mod tests;
mod tree;
mod verify;

use std::{path::PathBuf, process::Command};

use tree::{Disk, Git, Res, Source};

const USAGE: &str = "usage:
  split items <file.rs>
  split apply <manifest.toml>
  split test-list <out-dir>
  split verify [--manifest <m.toml> | --root <file.rs> --module <path>]
               [--delegate] [--tests <before-dir> <after-dir> [--target <bin:kind>]]
               [<rev>]
verify compares <rev>^ with <rev>, or HEAD with the working tree; --tests
also checks the test lists, snapshotted by test-list at those two trees.
--manifest (and its path table) is read from the working tree, not from
<rev>: a manifest amended after its move commit (e.g. a new [locations]
entry) applies when re-verifying that commit.";

fn git(args: &[&str]) -> Res<String> {
    let out = Command::new("git")
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

fn toml_at(src: &dyn Source, path: &str) -> Option<toml::Value> {
    toml::from_str(&String::from_utf8(src.read(path)?).ok()?).ok()
}

/// Test binary id (`split test-list`'s `name:kind`) of the target whose
/// module tree holds `file`: explicit `[lib]`/`[[bin]]`/`[[test]]`/
/// `[[example]]` paths first, then Cargo's auto-discovery layout.
fn crate_bin(src: &dyn Source, file: &str) -> Res<String> {
    let mut dir = tree::dir_of(file);
    let (pkg, cargo) = loop {
        if let Some(v) = toml_at(src, &tree::join(dir, "Cargo.toml")) {
            break (dir, v);
        }
        if dir.is_empty() {
            return Err(format!("{file}: no Cargo.toml above it"));
        }
        dir = tree::dir_of(dir);
    };
    let rel = file
        .strip_prefix(pkg)
        .unwrap_or(file)
        .trim_start_matches('/');
    let us = |s: &str| s.replace('-', "_");
    let package = cargo
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .ok_or("package name")?;
    let stem = |p: &str| {
        p.rsplit('/')
            .next()
            .unwrap_or(p)
            .trim_end_matches(".rs")
            .to_string()
    };
    let mut roots: Vec<(String, String)> = Vec::new();
    for (sect, kind) in [
        ("lib", "lib"),
        ("bin", "bin"),
        ("test", "test"),
        ("example", "bin"),
    ] {
        let tables = match cargo.get(sect) {
            Some(toml::Value::Table(t)) => vec![t.clone()],
            Some(toml::Value::Array(a)) => a.iter().filter_map(|v| v.as_table().cloned()).collect(),
            _ => vec![],
        };
        for t in tables {
            if let Some(p) = t.get("path").and_then(|p| p.as_str()) {
                let name = t
                    .get("name")
                    .and_then(|n| n.as_str())
                    .map_or_else(|| stem(p), str::to_string);
                roots.push((
                    p.trim_start_matches("./").to_string(),
                    format!("{}:{kind}", us(&name)),
                ));
            }
        }
    }
    if let Some((_, id)) = roots
        .iter()
        .find(|(p, _)| p == rel || tree::dir_of(p) == tree::dir_of(rel) && tree::dir_of(p) != "src")
    {
        return Ok(id.clone());
    }
    let parts: Vec<&str> = rel.split('/').collect();
    match parts.as_slice() {
        ["tests" | "examples" | "benches", first, ..] => {
            let kind = if parts[0] == "tests" { "test" } else { "bin" };
            Ok(format!("{}:{kind}", us(first.trim_end_matches(".rs"))))
        }
        ["src", "bin", first, ..] => Ok(format!("{}:bin", us(first.trim_end_matches(".rs")))),
        ["src", ..] => {
            let lib = roots.iter().any(|(_, id)| id.ends_with(":lib"))
                || src.read(&tree::join(pkg, "src/lib.rs")).is_some();
            Ok(format!(
                "{}:{}",
                us(package),
                if lib { "lib" } else { "bin" }
            ))
        }
        _ => Err(format!("{file}: cannot tell its target; pass --target")),
    }
}

fn run() -> Res<bool> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let repo = PathBuf::from(git(&["rev-parse", "--show-toplevel"])?);
    std::env::set_current_dir(&repo).map_err(|e| e.to_string())?;
    let arg = |i: usize| args.get(i).cloned().ok_or_else(|| USAGE.to_string());
    match args.first().map(String::as_str) {
        Some("items") => apply::list(&arg(1)?).map(|()| true),
        Some("apply") => {
            apply::run(&apply::Manifest::load(arg(1)?.as_ref())?, &repo).map(|()| true)
        }
        Some("test-list") => testlist::snapshot(&repo, arg(1)?.as_ref()).map(|()| true),
        Some("verify") => {
            let (mut manifest, mut root, mut module, mut delegate, mut tests, mut rev) =
                (None, None, None, false, None, None);
            let mut target = None;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--manifest" => (manifest, i) = (Some(arg(i + 1)?), i + 1),
                    "--root" => (root, i) = (Some(arg(i + 1)?), i + 1),
                    "--module" => (module, i) = (Some(arg(i + 1)?), i + 1),
                    "--delegate" => delegate = true,
                    "--tests" => (tests, i) = (Some((arg(i + 1)?, arg(i + 2)?)), i + 2),
                    "--target" => (target, i) = (Some(arg(i + 1)?), i + 1),
                    s if !s.starts_with('-') && rev.is_none() => rev = Some(s.to_string()),
                    _ => return Err(USAGE.into()),
                }
                i += 1;
            }
            let m = manifest
                .map(|p| apply::Manifest::load(p.as_ref()))
                .transpose()?;
            let spec = match (&m, root, module) {
                (Some(m), None, None) => verify::Spec {
                    manifest: Some(m),
                    old_root: m.source.clone(),
                    new_root: m.new_root(),
                    module: m.module.clone(),
                    delegate,
                },
                (None, Some(r), Some(module)) => verify::Spec {
                    manifest: None,
                    old_root: r.clone(),
                    new_root: r,
                    module,
                    delegate,
                },
                _ => return Err(USAGE.into()),
            };
            let (base, head, touched, trees): (Box<dyn Source>, Box<dyn Source>, String, _) =
                match &rev {
                    Some(r) => (
                        Box::new(Git(format!("{r}^"))),
                        Box::new(Git(r.clone())),
                        git(&["diff", "--name-only", "--no-renames", &format!("{r}^"), r])?,
                        (format!("{r}^"), Some(r.clone())),
                    ),
                    None => {
                        let st = git(&["status", "--porcelain=v1", "-uall", "--no-renames"])?;
                        let files = st
                            .lines()
                            .filter_map(|l| l.get(3..))
                            .collect::<Vec<_>>()
                            .join("\n");
                        (
                            Box::new(Git("HEAD".into())),
                            Box::new(Disk(repo.clone())),
                            files,
                            ("HEAD".to_string(), None),
                        )
                    }
                };
            let touched: Vec<String> = touched.lines().map(str::to_string).collect();
            let mut ok = verify::run(&spec, base.as_ref(), head.as_ref(), &touched)?;
            if let Some((b, a)) = tests {
                let after_tree = match &trees.1 {
                    Some(r) => testlist::rev_tree(&repo, r)?,
                    None => testlist::worktree_tree(&repo)?,
                };
                testlist::fresh(&repo, b.as_ref(), &testlist::rev_tree(&repo, &trees.0)?)?;
                testlist::fresh(&repo, a.as_ref(), &after_tree)?;
                let bin = match target.or_else(|| m.as_ref().and_then(|m| m.target.clone())) {
                    Some(t) => t,
                    None => crate_bin(base.as_ref(), &spec.old_root)?,
                };
                ok &= testlist::verify(&spec, base.as_ref(), &bin, b.as_ref(), a.as_ref())?;
            }
            Ok(ok)
        }
        _ => Err(USAGE.into()),
    }
}

fn main() {
    match run() {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(e) => {
            eprintln!("split: {e}");
            std::process::exit(2);
        }
    }
}
