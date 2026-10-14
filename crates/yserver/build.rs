//! Emit the `kms` cfg alias and stamp the build with the git commit for
//! `--version`. The GLSL shaders are compiled by `yserver-render`'s build
//! script.

use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
};

fn main() {
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));

    // `cfg(kms)`: the DRM/KMS platform (and its libinput/VT stack) builds.
    println!("cargo::rustc-check-cfg=cfg(kms)");
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if matches!(target_os.as_str(), "linux" | "freebsd") {
        println!("cargo::rustc-cfg=kms");
    }
    // The `xkbcommon` crate links `-lxkbcommon` without a search path, and
    // Homebrew's lib dir is not on the macOS linker's default path.
    if target_os == "macos" {
        emit_pkg_config_link_search("xkbcommon");
        emit_homebrew_prefix();
    }

    // Stamp the build with the git commit for `--version`. Best-effort:
    // a tarball / crates.io build (no `.git`, or no `git` on PATH) falls
    // back to "unknown".
    emit_git_commit(&manifest_dir);
}

/// Add `lib`'s pkg-config `libdir` as a native link search path. `libdir`
/// rather than `--libs-only-L`: pkgconf drops `-L` for its system dirs.
fn emit_pkg_config_link_search(lib: &str) {
    println!("cargo::rerun-if-env-changed=PKG_CONFIG_PATH");
    let Ok(output) = Command::new("pkg-config")
        .args(["--variable=libdir", lib])
        .output()
    else {
        println!("cargo::warning=pkg-config not found; cannot locate {lib}");
        return;
    };
    let libdir = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if libdir.is_empty() {
        println!("cargo::warning=pkg-config has no libdir for {lib}");
    } else {
        println!("cargo::rustc-link-search=native={libdir}");
    }
}

/// Emit `YSERVER_HOMEBREW_PREFIX` (`brew --prefix`), where `app_bundle`
/// looks for the Vulkan ICD first. Absent without `brew`.
fn emit_homebrew_prefix() {
    let Ok(output) = Command::new("brew").arg("--prefix").output() else {
        return;
    };
    let prefix = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if output.status.success() && !prefix.is_empty() {
        println!("cargo::rustc-env=YSERVER_HOMEBREW_PREFIX={prefix}");
    }
}

/// Emit `YSERVER_GIT_COMMIT` (consumed by `src/version.rs` via `env!`).
///
/// A pre-set `YSERVER_GIT_COMMIT` in the environment wins — that is how a
/// distro package stamps the release commit when building from a tarball
/// with no `.git`. Otherwise this is the 12-char `HEAD` hash, suffixed
/// `-dirty` when the working tree has uncommitted tracked changes, or
/// `"unknown"` outside a git checkout. Rerun triggers are registered so
/// the stamp tracks HEAD moves (commit / checkout / merge) and changes to
/// the override variable.
fn emit_git_commit(manifest_dir: &Path) {
    // Packagers build from an exported tarball with no `.git`, where the
    // git probe below yields "unknown". Let them stamp the release commit
    // explicitly instead. `rerun-if-env-changed` is required or a rebuild
    // in a warm target directory keeps the previous stamp baked in.
    println!("cargo:rerun-if-env-changed=YSERVER_GIT_COMMIT");
    if let Ok(preset) = std::env::var("YSERVER_GIT_COMMIT") {
        let preset = preset.trim();
        if !preset.is_empty() {
            println!("cargo:rustc-env=YSERVER_GIT_COMMIT={preset}");
            return;
        }
    }

    let commit = match run_git(manifest_dir, &["rev-parse", "--short=12", "HEAD"]) {
        Some(hash) => {
            // `git diff --quiet HEAD` exits non-zero on any tracked
            // change vs HEAD (matches `git describe --dirty` semantics;
            // untracked files don't count).
            let dirty = !git_status(manifest_dir, &["diff", "--quiet", "HEAD"]);
            if dirty { format!("{hash}-dirty") } else { hash }
        }
        None => "unknown".to_string(),
    };
    println!("cargo:rustc-env=YSERVER_GIT_COMMIT={commit}");

    // Rerun when HEAD moves. `logs/HEAD` is appended on every ref update
    // (commit / checkout / reset / merge), so watching it catches commits
    // on the current branch that `HEAD` itself (a symref) wouldn't.
    if let Some(git_dir) = run_git(manifest_dir, &["rev-parse", "--absolute-git-dir"]) {
        let git_dir = PathBuf::from(git_dir);
        for f in ["HEAD", "logs/HEAD"] {
            let p = git_dir.join(f);
            if p.exists() {
                println!("cargo:rerun-if-changed={}", p.display());
            }
        }
    }
}

/// Run `git <args>` in `dir`, returning trimmed stdout on success, or
/// `None` if git is absent / fails / prints nothing.
fn run_git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// Run `git <args>` for its exit status only. Returns `true` on success
/// (exit 0), `false` on non-zero or if git can't be spawned.
fn git_status(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .current_dir(dir)
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
