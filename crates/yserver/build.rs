//! Compile GLSL shaders under `src/kms/vk/shaders/` into SPIR-V
//! binaries written to `OUT_DIR`. Source files use the conventional
//! suffixes `.vert.glsl` / `.frag.glsl` (and `.comp.glsl` if/when
//! compute shaders show up); the matching `.spv` output preserves
//! the stage suffix so consumers can `include_bytes!()` them by
//! name.
//!
//! Phase 4.1.3.4 introduced the first GLSL shaders (the per-window
//! composite-pass quad shader); 4.1.4.6 will add a much larger set
//! of RENDER pipelines (FixedBlend / ShaderRMW). The build-time
//! glslc invocation scales by adding more files in the shaders
//! directory — no code changes here.
//!
//! Requires `glslc` (from `vulkan-tools` / shaderc) on PATH. If
//! it's missing, the build fails with a clear error rather than
//! producing a half-broken binary.

use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
};

fn main() {
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));

    // Stamp the build with the git commit for `--version`. Best-effort:
    // a tarball / crates.io build (no `.git`, or no `git` on PATH) falls
    // back to "unknown".
    emit_git_commit(&manifest_dir);

    let shaders_src = manifest_dir.join("src/kms/vk/shaders");

    println!("cargo:rerun-if-changed={}", shaders_src.display());
    println!("cargo:rerun-if-env-changed=GLSLC");

    let glslc = env::var_os("GLSLC")
        .map(PathBuf::from)
        .or_else(|| {
            // Try PATH lookup. We avoid the `which` crate to keep
            // build deps zero — this is a one-shot lookup.
            env::var_os("PATH").and_then(|paths| {
                env::split_paths(&paths)
                    .map(|d| d.join("glslc"))
                    .find(|p| p.is_file())
            })
        })
        .unwrap_or_else(|| {
            panic!(
                "glslc not found on PATH. Install vulkan-tools / shaderc, or set the GLSLC env \
                 var to the binary path. (Used to compile yserver's per-window composite \
                 shaders to SPIR-V.)"
            );
        });

    // Iterate top-level .glsl files (no recursion needed yet).
    let read_dir = std::fs::read_dir(&shaders_src).unwrap_or_else(|e| {
        panic!(
            "cannot read shader sources at {}: {e}",
            shaders_src.display()
        )
    });
    let mut compiled = 0usize;
    for entry in read_dir {
        let entry = entry.expect("read shaders dir");
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("glsl") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_else(|| panic!("shader path missing stem: {}", path.display()));
        // The stem is e.g. "composite.vert"; the stage is the inner
        // extension. Pass it explicitly to glslc via -fshader-stage
        // so glslc doesn't need to infer from path.
        let stage = stem
            .rsplit_once('.')
            .map(|(_, s)| s)
            .unwrap_or_else(|| panic!("shader stem missing stage suffix: {stem}"));

        let out_path = out_dir.join(format!("{stem}.spv"));
        compile_shader(&glslc, &path, &out_path, stage);
        compiled += 1;
    }
    if compiled == 0 {
        panic!(
            "no .glsl shader sources found under {} — build.rs would silently produce no \
             SPIR-V outputs. Either add a shader or remove build.rs.",
            shaders_src.display()
        );
    }
}

/// Emit `YSERVER_GIT_COMMIT` (consumed by `src/version.rs` via `env!`)
/// as the 12-char `HEAD` hash, suffixed `-dirty` when the working tree
/// has uncommitted tracked changes. Falls back to `"unknown"` outside a
/// git checkout. Also registers rerun triggers so the stamp tracks HEAD
/// moves (commit / checkout / merge).
fn emit_git_commit(manifest_dir: &Path) {
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

fn compile_shader(glslc: &Path, src: &Path, dst: &Path, stage: &str) {
    let status = Command::new(glslc)
        .arg(format!("-fshader-stage={stage}"))
        .arg("--target-env=vulkan1.3")
        .arg("-O") // size+perf optimisation; debug-friendly enough
        .arg("-o")
        .arg(dst)
        .arg(src)
        .status()
        .unwrap_or_else(|e| panic!("failed to spawn glslc on {}: {e}", src.display()));
    if !status.success() {
        panic!(
            "glslc failed (exit {:?}) on {} — see build log for diagnostics",
            status.code(),
            src.display()
        );
    }
}
