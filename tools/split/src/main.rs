//! Item-level file splitter and move verifier; see
//! docs/superpowers/plans/2026-10-08-source-layout-cleanup.md (rules 2–4, 7).
//! Paths are repo-relative; run from anywhere inside the repo.

mod apply;
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
               [--delegate] [--tests <before-dir> <after-dir>] [<rev>]
verify compares <rev>^ with <rev>, or HEAD with the working tree.";

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

fn crate_bin(src: &dyn Source, file: &str) -> Res<String> {
    let dir = file.split_once("/src/").map_or("", |(d, _)| d);
    let toml_path = format!("{dir}/Cargo.toml");
    let text = String::from_utf8(
        src.read(&toml_path)
            .ok_or(format!("{toml_path}: not found"))?,
    )
    .map_err(|e| e.to_string())?;
    let v: toml::Value = toml::from_str(&text).map_err(|e| e.to_string())?;
    let name = v
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .ok_or("package name")?;
    Ok(format!("{}:lib", name.replace('-', "_")))
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
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--manifest" => (manifest, i) = (Some(arg(i + 1)?), i + 1),
                    "--root" => (root, i) = (Some(arg(i + 1)?), i + 1),
                    "--module" => (module, i) = (Some(arg(i + 1)?), i + 1),
                    "--delegate" => delegate = true,
                    "--tests" => (tests, i) = (Some((arg(i + 1)?, arg(i + 2)?)), i + 2),
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
            let (base, head, touched): (Box<dyn Source>, Box<dyn Source>, String) = match &rev {
                Some(r) => (
                    Box::new(Git(format!("{r}^"))),
                    Box::new(Git(r.clone())),
                    git(&["diff", "--name-only", "--no-renames", &format!("{r}^"), r])?,
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
                    )
                }
            };
            if let Some((b, a)) = tests {
                let bin = crate_bin(head.as_ref(), &spec.new_root)?;
                return testlist::verify(&spec, base.as_ref(), &bin, b.as_ref(), a.as_ref());
            }
            let touched: Vec<String> = touched.lines().map(str::to_string).collect();
            verify::run(&spec, base.as_ref(), head.as_ref(), &touched)
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
