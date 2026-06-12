# lightdm Launch (argv + readiness) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make yserver launchable by the lightdm display manager — parse X-server-style argv, hold the `/tmp/.X<N>-lock` display lock, and perform the SIGUSR1 / `-displayfd` readiness handshake so lightdm starts the greeter.

**Architecture:** A new pure-where-possible `launch` module owns argv parsing, the display-resolution table, the X lockfile protocol, socket binding (explicit + auto-pick), and readiness signaling. `yserver::run()` changes from `run(display: u16)` to `run(opts: LaunchOptions)` and calls the module's helpers; the binary becomes a thin shim. Filesystem-touching helpers take directory paths so they unit-test against a tempdir.

**Tech Stack:** Rust, `nix` (signal/signalfd, already used), `libc` (raw `sigaction`/`kill`/`getppid`/`write`/`pipe`), std `UnixListener`/`UnixStream`.

**Spec:** `docs/superpowers/specs/2026-06-12-lightdm-launch-design.md`

**Toolchain (per AGENTS.md):** format with `cargo +nightly fmt`; lint with regular `cargo clippy` (NOT pedantic); test with `cargo test`.

---

## File Structure

- **Create** `crates/yserver/src/launch.rs` — the whole launch subsystem:
  - `LaunchOptions` + `parse_args` (pure)
  - `Resolution` + `resolve` (pure — the display-resolution table)
  - `DEFAULT_DISPLAY` const
  - `LockGuard` + `acquire_lock` + `lock_path` + `inspect_lock` (lockfile protocol)
  - `bind_explicit` + `autopick` (socket binding)
  - `write_displayfd` + `sigusr1_is_ignored` + `signal_ready_to_parent` + `signal_ready` (readiness)
- **Modify** `crates/yserver/src/lib.rs` — add `pub mod launch;`; change `run` signature; query SIGUSR1 disposition early; replace inline socket bind (228-260) with resolve + lock + bind; signal readiness before `run_core`; remove lock after socket on shutdown / error paths.
- **Modify** `crates/yserver/src/bin/yserver.rs` — replace `parse_display` with `launch::parse_args`; build `LaunchOptions`; call `run(opts)`.

Each task below produces a self-contained, compiling, tested change.

---

### Task 1: argv parsing + display resolution (pure)

**Files:**
- Create: `crates/yserver/src/launch.rs`
- Modify: `crates/yserver/src/lib.rs` (add `pub mod launch;`)

- [ ] **Step 1: Create the module with types, parser, resolver, and failing tests**

Create `crates/yserver/src/launch.rs`:

```rust
//! X-server-style launch handling: argv parsing, display resolution,
//! the `/tmp/.X<N>-lock` protocol, socket binding, and the lightdm
//! readiness handshake. See
//! `docs/superpowers/specs/2026-06-12-lightdm-launch-design.md`.

use std::os::fd::RawFd;
use std::path::PathBuf;

/// Display yserver uses when neither an explicit display nor `-displayfd`
/// is given. 7 avoids clashing with a real Xorg on `:0` (existing
/// convention).
pub const DEFAULT_DISPLAY: u16 = 7;

/// Parsed X-server-style command line. Fields the issue's items 1-2 act
/// on; `vt`/`seat` are parsed + logged but otherwise ignored (logind owns
/// the seat/VT), `auth_file` is stashed for the deferred item 4.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LaunchOptions {
    /// `:N` or bare `N` → explicit display; `None` → resolved in `run()`.
    pub display: Option<u16>,
    /// `-displayfd N`.
    pub displayfd: Option<RawFd>,
    /// `vtN` — logged, otherwise ignored.
    pub vt: Option<u32>,
    /// `-seat NAME` — logged, otherwise ignored.
    pub seat: Option<String>,
    /// `-auth FILE` — stashed for item 4, unused now.
    pub auth_file: Option<PathBuf>,
}

fn next_value(
    it: &mut impl Iterator<Item = String>,
    flag: &str,
) -> Result<String, String> {
    it.next().ok_or_else(|| format!("{flag} requires a value"))
}

/// Parse X-server-style argv. Tolerates unknown flags (warn + skip);
/// hard-errors only on malformed *explicit* requests and missing values
/// for known value-taking flags.
pub fn parse_args(
    args: impl IntoIterator<Item = String>,
) -> Result<LaunchOptions, String> {
    let mut o = LaunchOptions::default();
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        if let Some(rest) = arg.strip_prefix(':') {
            o.display = Some(
                rest.parse::<u16>()
                    .map_err(|_| format!("invalid display argument: {arg}"))?,
            );
        } else if let Some(rest) = arg.strip_prefix("vt") {
            o.vt = Some(
                rest.parse::<u32>()
                    .map_err(|_| format!("invalid vt argument: {arg}"))?,
            );
        } else if arg == "-seat" {
            o.seat = Some(next_value(&mut it, "-seat")?);
        } else if arg == "-auth" {
            o.auth_file = Some(PathBuf::from(next_value(&mut it, "-auth")?));
        } else if arg == "-displayfd" {
            let v = next_value(&mut it, "-displayfd")?;
            o.displayfd = Some(
                v.parse::<RawFd>()
                    .map_err(|_| format!("invalid -displayfd argument: {v}"))?,
            );
        } else if matches!(
            arg.as_str(),
            "-nolisten" | "-config" | "-layout" | "-background"
        ) {
            // Known value-taking no-ops. Consume + ignore the value; a
            // missing value is tolerated (these don't affect us).
            if it.next().is_none() {
                log::warn!("yserver: {arg} given without a value; ignoring");
            }
        } else if arg == "-novtswitch" {
            // Known no-arg no-op (lightdm passes it).
        } else if let Ok(n) = arg.parse::<u16>() {
            // Bare number → display. Keeps `yserver 7` (Justfile) working.
            o.display = Some(n);
        } else {
            log::warn!("yserver: ignoring unrecognized argument: {arg}");
        }
    }
    Ok(o)
}

/// How `run()` should obtain the display + whether to take the lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// Use this exact display. `lock` is true only when `-displayfd` is
    /// absent (Xorg sets `nolock = TRUE` whenever `-displayfd` is parsed).
    Explicit { display: u16, lock: bool },
    /// Scan for the lowest free display (gdm-style `-displayfd`); no lock.
    AutoPick,
}

/// The display-resolution table from the spec. Lock iff `-displayfd` is
/// absent.
#[must_use]
pub fn resolve(opts: &LaunchOptions) -> Resolution {
    match (opts.display, opts.displayfd) {
        (Some(display), None) => Resolution::Explicit { display, lock: true },
        (Some(display), Some(_)) => Resolution::Explicit { display, lock: false },
        (None, Some(_)) => Resolution::AutoPick,
        (None, None) => Resolution::Explicit {
            display: DEFAULT_DISPLAY,
            lock: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<LaunchOptions, String> {
        parse_args(args.iter().map(|s| (*s).to_string()))
    }

    #[test]
    fn lightdm_default_argv_parses_clean() {
        let o = parse(&[
            ":0",
            "-seat",
            "seat0",
            "-auth",
            "/var/run/lightdm/root/:0",
            "-nolisten",
            "tcp",
            "vt7",
            "-novtswitch",
        ])
        .unwrap();
        assert_eq!(o.display, Some(0));
        assert_eq!(o.displayfd, None);
        assert_eq!(o.vt, Some(7));
        assert_eq!(o.seat.as_deref(), Some("seat0"));
        assert_eq!(o.auth_file, Some(PathBuf::from("/var/run/lightdm/root/:0")));
    }

    #[test]
    fn gdm_style_displayfd_without_explicit_display() {
        let o = parse(&["-displayfd", "12"]).unwrap();
        assert_eq!(o.displayfd, Some(12));
        assert_eq!(o.display, None);
    }

    #[test]
    fn bare_number_is_back_compat_display() {
        assert_eq!(parse(&["7"]).unwrap().display, Some(7));
        assert_eq!(parse(&[]).unwrap().display, None);
    }

    #[test]
    fn explicit_colon_display() {
        assert_eq!(parse(&[":42"]).unwrap().display, Some(42));
    }

    #[test]
    fn unknown_flags_are_tolerated() {
        let o = parse(&["-bogus", "--whatever", ":1"]).unwrap();
        assert_eq!(o.display, Some(1));
    }

    #[test]
    fn malformed_explicit_requests_error() {
        assert!(parse(&[":foo"]).is_err());
        assert!(parse(&["vtbad"]).is_err());
        assert!(parse(&["-displayfd", "notanumber"]).is_err());
    }

    #[test]
    fn missing_required_value_errors() {
        assert!(parse(&["-seat"]).is_err());
        assert!(parse(&["-auth"]).is_err());
        assert!(parse(&["-displayfd"]).is_err());
    }

    #[test]
    fn resolution_table() {
        let mk = |d: Option<u16>, fd: Option<RawFd>| LaunchOptions {
            display: d,
            displayfd: fd,
            ..Default::default()
        };
        assert_eq!(
            resolve(&mk(Some(0), None)),
            Resolution::Explicit { display: 0, lock: true }
        );
        assert_eq!(
            resolve(&mk(Some(0), Some(9))),
            Resolution::Explicit { display: 0, lock: false }
        );
        assert_eq!(resolve(&mk(None, Some(9))), Resolution::AutoPick);
        assert_eq!(
            resolve(&mk(None, None)),
            Resolution::Explicit { display: DEFAULT_DISPLAY, lock: true }
        );
    }
}
```

Add the module declaration to `crates/yserver/src/lib.rs` — in the `pub mod` block near the top (after `pub mod kms;`):

```rust
pub mod kms;
pub mod launch;
pub mod present;
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p yserver --lib launch::tests`
Expected: compile succeeds, all `launch::tests::*` PASS immediately (this task is pure logic with no separate "stub" phase). If anything FAILS, fix the parser until green.

> Note: because `parse_args`/`resolve` are written together with their tests in one step, "red" here is a compile/logic check rather than a missing-symbol failure. Confirm the suite is green before committing.

- [ ] **Step 3: Format, lint, commit**

Run:
```bash
cargo +nightly fmt
cargo clippy -p yserver --lib
git add crates/yserver/src/launch.rs crates/yserver/src/lib.rs
git commit -m "feat(launch): X-style argv parser + display-resolution table"
```
Expected: fmt clean, clippy no warnings, commit succeeds.

---

### Task 2: display lockfile protocol

**Files:**
- Modify: `crates/yserver/src/launch.rs`

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module in `crates/yserver/src/launch.rs`:

```rust
    use std::io::Write as _;

    fn unique_tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("yserver-launch-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn lock_fresh_acquire_creates_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = unique_tmp_dir("lock-fresh");
        let guard = acquire_lock(&dir, 5).unwrap();
        let p = lock_path(&dir, 5);
        assert!(p.exists());
        // Xorg publishes the lock read-only (0444).
        let mode = std::fs::metadata(&p).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o444);
        drop(guard);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn lock_guard_drop_removes_file() {
        let dir = unique_tmp_dir("lock-drop");
        let guard = acquire_lock(&dir, 6).unwrap();
        let p = lock_path(&dir, 6);
        assert!(p.exists());
        drop(guard);
        assert!(!p.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn lock_live_pid_is_rejected() {
        let dir = unique_tmp_dir("lock-live");
        // First acquire writes OUR pid into the lock; the second sees a
        // live owner (us) and must refuse.
        let _guard = acquire_lock(&dir, 7).unwrap();
        let err = acquire_lock(&dir, 7).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn lock_stale_pid_is_reclaimed() {
        let dir = unique_tmp_dir("lock-stale");
        // A pid far above any real one → kill(pid,0) == ESRCH → stale.
        std::fs::write(lock_path(&dir, 8), format!("{:>10}\n", 2_147_483_646i32))
            .unwrap();
        let guard = acquire_lock(&dir, 8).unwrap();
        assert!(lock_path(&dir, 8).exists());
        drop(guard);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn lock_bogus_contents_are_reclaimed() {
        let dir = unique_tmp_dir("lock-bogus");
        std::fs::write(lock_path(&dir, 9), b"not a pid").unwrap();
        let guard = acquire_lock(&dir, 9).unwrap();
        assert!(lock_path(&dir, 9).exists());
        drop(guard);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn lock_short_numeric_contents_are_bogus() {
        // Xorg reads exactly 11 bytes ("%10d\n"); a short numeric file is
        // not a valid lock and must be reclaimed, not trusted.
        let dir = unique_tmp_dir("lock-short");
        std::fs::write(lock_path(&dir, 10), b"123\n").unwrap();
        let guard = acquire_lock(&dir, 10).unwrap();
        assert!(lock_path(&dir, 10).exists());
        drop(guard);
        std::fs::remove_dir_all(&dir).ok();
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p yserver --lib launch::tests::lock`
Expected: FAIL to compile — `acquire_lock` / `lock_path` not defined.

- [ ] **Step 3: Implement the lockfile protocol**

Add to the top-of-file `use` block in `crates/yserver/src/launch.rs`:

```rust
use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, Read, Write};
use std::os::fd::RawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
```

(Replace the existing two `use std::os::fd::RawFd;` / `use std::path::PathBuf;` lines with the block above.)

Add the implementation (after `resolve`):

```rust
/// RAII handle for an acquired display lock. Dropping removes the lock
/// file. `run()` holds this for the server's lifetime and lets it drop
/// *after* the socket is removed at shutdown (the lock is the
/// authoritative occupancy marker, so it must outlive the socket).
pub struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// `/tmp/.X<N>-lock` path for a display.
#[must_use]
pub fn lock_path(lock_dir: &Path, display: u16) -> PathBuf {
    lock_dir.join(format!(".X{display}-lock"))
}

enum LockState {
    Alive,
    Stale,
    Bogus,
}

fn inspect_lock(path: &Path) -> io::Result<LockState> {
    let f = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(f) => f,
        // Vanished between link-failure and open → treat as stale (retry).
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(LockState::Stale),
        Err(e) => return Err(e),
    };
    // Xorg's lock format is exactly "%10d\n" = 11 bytes. read_to_end
    // (capped at 12 via `take`) loops over short reads — a single
    // read() may legally return fewer bytes; this is lock-DELETION
    // logic, so don't risk it. The 12th byte makes an over-long file
    // detectable (len == 12 ⇒ bogus). A genuine I/O error propagates —
    // never classify an unreadable lock as bogus (that would delete a
    // lock we couldn't actually inspect).
    let mut buf = Vec::with_capacity(12);
    f.take(12).read_to_end(&mut buf)?;
    if buf.len() != 11 || buf[10] != b'\n' {
        return Ok(LockState::Bogus);
    }
    let text = std::str::from_utf8(&buf[..11]).unwrap_or("").trim();
    let pid: i32 = match text.parse() {
        Ok(p) if p > 0 => p,
        _ => return Ok(LockState::Bogus),
    };
    // SAFETY: kill with signal 0 only probes existence/permissions.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return Ok(LockState::Alive);
    }
    match io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => Ok(LockState::Stale),
        // EPERM ⇒ a live process we may not signal (e.g. another user).
        // Anything else ⇒ be conservative and treat as occupied.
        _ => Ok(LockState::Alive),
    }
}

/// Acquire the `/tmp/.X<N>-lock` display lock using Xorg's temp-file +
/// atomic `link()` protocol (`os/utils.c`), reclaiming stale/bogus locks.
/// Errors with `AddrInUse` if a live server owns the display.
pub fn acquire_lock(lock_dir: &Path, display: u16) -> io::Result<LockGuard> {
    let final_path = lock_path(lock_dir, display);
    // PID-suffixed temp name: each starter owns a unique temp file, so two
    // concurrent starters can never clobber each other's temp before the
    // atomic link() (a fixed name would let the winner link a file holding
    // the LOSER's pid, corrupting later stale/live detection). Deviation
    // from Xorg's fixed ".tX<N>-lock": race-free without Xorg's
    // O_EXCL+retry dance; a crash can orphan one 11-byte temp file, which
    // is harmless (nothing inspects temp names).
    let tmp_path = lock_dir.join(format!(".tX{display}-lock.{}", std::process::id()));
    let pid_line = format!("{:>10}\n", std::process::id());

    // Create the temp ONCE, outside the retry loop: its content never
    // changes between attempts, and a 0444 file cannot be re-opened for
    // write on a second iteration (mode applies at create time; reopening
    // a read-only file for write is EACCES). The pre-unlink handles a
    // stale 0444 temp left by a crashed earlier server that had our
    // (reused) PID.
    let _ = fs::remove_file(&tmp_path);
    {
        let mut tmp = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o444)
            .open(&tmp_path)?;
        tmp.write_all(pid_line.as_bytes())?;
    }
    // Create-time mode is masked by the process umask (a DM/systemd unit
    // may set e.g. UMask=0077 → 0400, unreadable to foreign launchers —
    // Xorg's LockServer READS existing locks). Force 0444 umask-immune,
    // like Xorg's fchmod after create (os/utils.c:312).
    fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o444))?;

    // At most two attempts: acquire, or reclaim-once-then-acquire.
    for _ in 0..2 {
        match fs::hard_link(&tmp_path, &final_path) {
            Ok(()) => {
                let _ = fs::remove_file(&tmp_path);
                return Ok(LockGuard { path: final_path });
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                // Identity-bracketed reclaim: note the lock's (dev, ino)
                // before inspecting, and only unlink if the path still
                // names that same inode afterwards. Narrows the race
                // where a concurrent starter reclaims the stale lock and
                // links its own fresh one between our inspect and our
                // unlink. Xorg has the full-width version of this race
                // (LockServer: read → kill(pid,0) → unlink, no identity
                // check); Linux has no unlink-by-fd, so a tiny
                // lstat→unlink window remains — and same-display
                // concurrent starts are DM-serialized in practice.
                let seen = fs::symlink_metadata(&final_path)
                    .ok()
                    .map(|m| (m.dev(), m.ino()));
                match inspect_lock(&final_path)? {
                    LockState::Stale | LockState::Bogus => {
                        let now = fs::symlink_metadata(&final_path)
                            .ok()
                            .map(|m| (m.dev(), m.ino()));
                        if seen.is_some() && now == seen {
                            let _ = fs::remove_file(&final_path);
                        }
                        // else: replaced under us — the retry link will
                        // hit EEXIST again and inspect the NEW lock
                        // (typically Alive → AddrInUse).
                    }
                    LockState::Alive => {
                        let _ = fs::remove_file(&tmp_path);
                        return Err(io::Error::new(
                            ErrorKind::AddrInUse,
                            format!(
                                "display :{display} already in use (lock {})",
                                final_path.display()
                            ),
                        ));
                    }
                }
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp_path);
                return Err(e);
            }
        }
    }
    let _ = fs::remove_file(&tmp_path);
    Err(io::Error::new(
        ErrorKind::AddrInUse,
        format!("could not acquire display lock {}", final_path.display()),
    ))
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p yserver --lib launch::tests::lock`
Expected: PASS (6 lock tests: fresh, drop, live, stale, bogus, short-numeric).

- [ ] **Step 5: Format, lint, commit**

Run:
```bash
cargo +nightly fmt
cargo clippy -p yserver --lib
git add crates/yserver/src/launch.rs
git commit -m "feat(launch): /tmp/.X<N>-lock protocol (temp+link, stale reclaim)"
```
Expected: clean, commit succeeds.

---

### Task 3: socket binding (explicit + auto-pick)

**Files:**
- Modify: `crates/yserver/src/launch.rs`

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module:

```rust
    use std::os::unix::net::UnixListener as TestListener;

    #[test]
    fn bind_explicit_binds_and_chmods() {
        let dir = unique_tmp_dir("bind-explicit");
        let (listener, path) = bind_explicit(&dir, 3).unwrap();
        assert_eq!(path, dir.join("X3"));
        assert!(path.exists());
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o777);
        drop(listener);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn autopick_empty_dir_picks_zero() {
        let dir = unique_tmp_dir("autopick-empty");
        let (n, listener, path) = autopick(&dir).unwrap();
        assert_eq!(n, 0);
        assert_eq!(path, dir.join("X0"));
        drop(listener);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn autopick_skips_live_socket() {
        let dir = unique_tmp_dir("autopick-live");
        // A live listener on X0 — keep it bound for the duration.
        let live = TestListener::bind(dir.join("X0")).unwrap();
        let (n, listener, _path) = autopick(&dir).unwrap();
        assert_eq!(n, 1);
        drop(listener);
        drop(live);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn autopick_reclaims_stale_socket() {
        let dir = unique_tmp_dir("autopick-stale");
        // Bind then drop: the socket node remains on disk with no
        // listener (Rust does not unlink on drop) → a faithful stale
        // socket. connect() → ECONNREFUSED → reclaim.
        let stale = TestListener::bind(dir.join("X0")).unwrap();
        drop(stale);
        assert!(dir.join("X0").exists());
        let (n, listener, _path) = autopick(&dir).unwrap();
        assert_eq!(n, 0);
        drop(listener);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn autopick_skips_non_socket_file() {
        // A regular file named X0: the file-type check skips it without
        // deleting. (connect() to a non-socket gives ECONNREFUSED on
        // Linux — same errno as a stale socket — so the type check, not
        // errno, is what protects the file.)
        let dir = unique_tmp_dir("autopick-notsock");
        std::fs::write(dir.join("X0"), b"junk").unwrap();
        let (n, listener, _path) = autopick(&dir).unwrap();
        assert_eq!(n, 1);
        assert!(dir.join("X0").exists()); // untouched
        drop(listener);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bind_explicit_refuses_non_socket_file() {
        let dir = unique_tmp_dir("bind-notsock");
        std::fs::write(dir.join("X6"), b"junk").unwrap();
        let err = bind_explicit(&dir, 6).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
        assert!(dir.join("X6").exists()); // untouched
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bind_explicit_refuses_live_socket() {
        // Explicit bind must never steal a live server's socket — probe
        // first, error AddrInUse if something is listening.
        let dir = unique_tmp_dir("bind-live");
        let live = TestListener::bind(dir.join("X4")).unwrap();
        let err = bind_explicit(&dir, 4).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
        drop(live);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bind_explicit_reclaims_stale_socket() {
        let dir = unique_tmp_dir("bind-stale");
        let stale = TestListener::bind(dir.join("X5")).unwrap();
        drop(stale);
        assert!(dir.join("X5").exists());
        let (listener, path) = bind_explicit(&dir, 5).unwrap();
        assert_eq!(path, dir.join("X5"));
        drop(listener);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn lock_released_when_bind_fails() {
        // Mimics run()'s error path: `?` after acquire_lock drops the
        // guard, which must remove the lock — never leave a lock we
        // don't back with a live socket.
        let dir = unique_tmp_dir("lock-bind-fail");
        let res: std::io::Result<()> = (|| {
            let _guard = acquire_lock(&dir, 11)?;
            let missing = dir.join("no-such-subdir");
            let _ = bind_explicit(&missing, 11)?; // fails: dir doesn't exist
            Ok(())
        })();
        assert!(res.is_err());
        assert!(!lock_path(&dir, 11).exists());
        std::fs::remove_dir_all(&dir).ok();
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p yserver --lib launch::tests`
Expected: FAIL to compile — `bind_explicit` / `autopick` not defined.

- [ ] **Step 3: Implement the binding helpers**

Extend the `use` block at the top of `launch.rs` to add the net + permissions imports (merge into the existing `use std::os::unix::...` lines):

```rust
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
```

Add the implementation (after `acquire_lock`):

```rust
fn chmod_socket(path: &Path) -> io::Result<()> {
    // X clients connect as the invoking user; the socket needs world
    // write (connect() on AF_UNIX requires `w`). Xorg sets 0777.
    fs::set_permissions(path, fs::Permissions::from_mode(0o777))
}

/// Bind `<socket_dir>/X<display>` for the explicit-`:N` and
/// back-compat-default cases. Probes an existing socket before removing
/// it: a live server's socket is NEVER stolen (old yservers take no
/// lock, so holding the lock alone doesn't prove the display is free).
pub fn bind_explicit(
    socket_dir: &Path,
    display: u16,
) -> io::Result<(UnixListener, PathBuf)> {
    let path = socket_dir.join(format!("X{display}"));
    // File-type check FIRST: connect(AF_UNIX) to a non-socket inode
    // returns ECONNREFUSED on Linux (same errno as a stale socket!), so
    // errno alone cannot distinguish "dead server's socket" from "some
    // file that happens to be named X<n>". Never delete a non-socket.
    // symlink_metadata also refuses symlinks (is_socket() is false).
    if let Ok(meta) = fs::symlink_metadata(&path) {
        if !meta.file_type().is_socket() {
            return Err(io::Error::new(
                ErrorKind::AddrInUse,
                format!(
                    "{} exists and is not a socket — refusing to replace it",
                    path.display()
                ),
            ));
        }
        match UnixStream::connect(&path) {
            Ok(_) => {
                return Err(io::Error::new(
                    ErrorKind::AddrInUse,
                    format!("display :{display} in use (live socket {})", path.display()),
                ));
            }
            Err(e) if e.kind() == ErrorKind::ConnectionRefused => {
                // Stale socket from a dead server — reclaim it.
                let _ = fs::remove_file(&path);
            }
            Err(_) => {
                // Occupied/unknown (e.g. EACCES). Be conservative:
                // refuse rather than delete what we can't identify.
                return Err(io::Error::new(
                    ErrorKind::AddrInUse,
                    format!("cannot probe {} — refusing to replace it", path.display()),
                ));
            }
        }
    }
    let listener = UnixListener::bind(&path)?;
    chmod_socket(&path)?;
    Ok((listener, path))
}

/// Scan `0..256` for the lowest free display and bind it. Disambiguates an
/// existing socket file via `connect()`: refused ⇒ stale ⇒ reclaim;
/// connected ⇒ live ⇒ skip; other error ⇒ occupied ⇒ skip. Retries on a
/// `bind()` `EADDRINUSE` race. Takes no lock (matches Xorg `nolock`).
pub fn autopick(socket_dir: &Path) -> io::Result<(u16, UnixListener, PathBuf)> {
    for n in 0u16..256 {
        let path = socket_dir.join(format!("X{n}"));
        // Same file-type-first rule as bind_explicit: ECONNREFUSED can't
        // distinguish a stale socket from a non-socket file, and we must
        // never delete the latter. Non-socket / unreadable ⇒ skip.
        match fs::symlink_metadata(&path) {
            Ok(meta) if !meta.file_type().is_socket() => continue,
            Ok(_) => match UnixStream::connect(&path) {
                Ok(_) => continue, // live server
                Err(e) if e.kind() == ErrorKind::ConnectionRefused => {
                    // Stale socket node — reclaim it.
                    let _ = fs::remove_file(&path);
                }
                Err(_) => continue, // occupied/unknown — leave it alone
            },
            Err(e) if e.kind() == ErrorKind::NotFound => {} // free — bind below
            Err(_) => continue, // unreadable — not ours to touch
        }
        match UnixListener::bind(&path) {
            Ok(listener) => {
                chmod_socket(&path)?;
                return Ok((n, listener, path));
            }
            Err(e) if e.kind() == ErrorKind::AddrInUse => continue, // lost a race
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        ErrorKind::AddrInUse,
        "no free X display in 0..256",
    ))
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p yserver --lib launch::tests`
Expected: PASS — all `launch::tests` green, including the 9 added in this task (explicit bind ×4, autopick ×4, lock-release-on-bind-failure).

- [ ] **Step 5: Format, lint, commit**

Run:
```bash
cargo +nightly fmt
cargo clippy -p yserver --lib
git add crates/yserver/src/launch.rs
git commit -m "feat(launch): explicit + auto-pick socket binding"
```
Expected: clean, commit succeeds.

---

### Task 4: readiness signaling (`-displayfd` + SIGUSR1-to-parent)

**Files:**
- Modify: `crates/yserver/src/launch.rs`

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module:

```rust
    #[test]
    fn write_displayfd_writes_ascii_and_closes() {
        // libc pipe: write end gets the display number, read end verifies.
        let mut fds = [0 as RawFd; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let (read_fd, write_fd) = (fds[0], fds[1]);

        write_displayfd(write_fd, 12).unwrap();

        let mut buf = [0u8; 8];
        let n = unsafe {
            libc::read(read_fd, buf.as_mut_ptr().cast(), buf.len())
        };
        assert!(n > 0);
        assert_eq!(&buf[..n as usize], b"12\n");
        unsafe { libc::close(read_fd) };
    }

    #[test]
    fn sigusr1_disposition_roundtrip() {
        // Process-global: save the prior disposition and restore it at
        // the end (SIG_DFL is not necessarily what we started with). No
        // other unit test in this crate touches SIGUSR1 disposition —
        // keep it that way (tests share one process).
        let prev = unsafe { libc::signal(libc::SIGUSR1, libc::SIG_IGN) };
        assert!(sigusr1_is_ignored());
        unsafe { libc::signal(libc::SIGUSR1, libc::SIG_DFL) };
        assert!(!sigusr1_is_ignored());
        unsafe { libc::signal(libc::SIGUSR1, prev) };
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p yserver --lib launch::tests`
Expected: FAIL to compile — `write_displayfd` / `sigusr1_is_ignored` not defined.

- [ ] **Step 3: Implement the readiness helpers**

Add (after `autopick`):

```rust
/// Write `"<display>\n"` (Xorg's `-displayfd` format) to `fd`, then close
/// it. Used only when `-displayfd N` was given. Close errors are ignored:
/// the fd is single-use and there is nothing actionable after the payload
/// was written.
pub fn write_displayfd(fd: RawFd, display: u16) -> io::Result<()> {
    let s = format!("{display}\n");
    let bytes = s.as_bytes();
    let mut written = 0usize;
    while written < bytes.len() {
        // SAFETY: writing `bytes[written..]` to a caller-owned fd.
        let r = unsafe {
            libc::write(
                fd,
                bytes[written..].as_ptr().cast(),
                bytes.len() - written,
            )
        };
        if r < 0 {
            let err = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(err);
        }
        if r == 0 {
            // A zero-length write would loop forever — fail instead.
            unsafe { libc::close(fd) };
            return Err(io::Error::new(
                ErrorKind::WriteZero,
                "displayfd write returned 0",
            ));
        }
        written += r as usize;
    }
    unsafe { libc::close(fd) };
    Ok(())
}

/// True if SIGUSR1's *inherited disposition* is `SIG_IGN` — the signal
/// the DM (lightdm/Xorg convention) uses to request "signal me when
/// ready". Querying via `sigaction(…, NULL, &old)` does not mutate the
/// disposition; signalfd masking (done later) only blocks delivery, so
/// the two are independent — call this any time before installing a
/// `sigaction` handler (yserver never does; it uses signalfd).
#[must_use]
pub fn sigusr1_is_ignored() -> bool {
    // SAFETY: zeroed sigaction is a valid "read current" target.
    let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::sigaction(libc::SIGUSR1, std::ptr::null(), &mut old) };
    rc == 0 && old.sa_sigaction == libc::SIG_IGN
}

/// Send SIGUSR1 to the parent (the DM), matching Xorg's `NotifyParentProcess`
/// (`ParentProcess = getppid()`). Skips PID 1 — if we were reparented to
/// init there is no DM to notify.
pub fn signal_ready_to_parent() {
    let ppid = unsafe { libc::getppid() };
    if ppid > 1 {
        unsafe { libc::kill(ppid, libc::SIGUSR1) };
    }
}

/// Perform the readiness handshake: report the chosen display on
/// `-displayfd` (if given) and signal the parent (if SIGUSR1 was inherited
/// ignored). Call once, just before entering the core loop.
pub fn signal_ready(opts: &LaunchOptions, display: u16, sigusr1_was_ignored: bool) {
    if let Some(fd) = opts.displayfd {
        if let Err(e) = write_displayfd(fd, display) {
            log::warn!("yserver: failed to write -displayfd {fd}: {e}");
        }
    }
    if sigusr1_was_ignored {
        log::info!("yserver: signaling readiness to parent (SIGUSR1)");
        signal_ready_to_parent();
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p yserver --lib launch::tests`
Expected: PASS — all `launch::tests` green, including the 2 added in this task (displayfd pipe, SIGUSR1 disposition).

- [ ] **Step 5: Run the full launch suite, format, lint, commit**

Run:
```bash
cargo test -p yserver --lib launch::tests
cargo +nightly fmt
cargo clippy -p yserver --lib
git add crates/yserver/src/launch.rs
git commit -m "feat(launch): -displayfd + SIGUSR1-to-parent readiness"
```
Expected: all `launch::` tests pass, clean, commit succeeds.

---

### Task 5: wire `launch` into `run()` and the binary

**Files:**
- Modify: `crates/yserver/src/lib.rs` (signature + bootstrap)
- Modify: `crates/yserver/src/bin/yserver.rs` (thin shim)

- [ ] **Step 1: Change the `run()` signature and query SIGUSR1 early**

In `crates/yserver/src/lib.rs`, change the signature at line 48:

```rust
pub fn run(opts: launch::LaunchOptions) -> io::Result<()> {
```

Immediately after the `log::info!("yserver: Phase 6.4 ...")` line (≈ line 52), capture the inherited SIGUSR1 disposition **before** anything blocks it:

```rust
    // Capture the inherited SIGUSR1 disposition before signalfd masking.
    // If the DM started us with SIGUSR1 ignored, we signal it when ready.
    let sigusr1_was_ignored = launch::sigusr1_is_ignored();
```

- [ ] **Step 2: Replace the inline socket bind (lib.rs ~228-260) with resolve + lock + bind**

Replace this existing block:

```rust
    let socket_dir = PathBuf::from("/tmp/.X11-unix");
    fs::create_dir_all(&socket_dir).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("create_dir_all({}): {e}", socket_dir.display()),
        )
    })?;
    let socket_path = socket_dir.join(format!("X{display}"));
    match fs::remove_file(&socket_path) {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => {
            return Err(io::Error::new(
                err.kind(),
                format!("remove_file({}): {err}", socket_path.display()),
            ));
        }
    }
    let listener = UnixListener::bind(&socket_path).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("UnixListener::bind({}): {e}", socket_path.display()),
        )
    })?;
    // X clients connect as the invoking user; the socket needs world write
    // (connect() on AF_UNIX requires `w`). Xorg sets 0777 on /tmp/.X11-unix/X*.
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o777)).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("set_permissions({}, 0o777): {e}", socket_path.display()),
        )
    })?;
    log::info!("yserver: listening on unix socket DISPLAY=:{display}");
```

with:

```rust
    let socket_dir = PathBuf::from("/tmp/.X11-unix");
    fs::create_dir_all(&socket_dir).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("create_dir_all({}): {e}", socket_dir.display()),
        )
    })?;
    let lock_dir = PathBuf::from("/tmp");

    // Resolve the effective display, acquire the lock (when -displayfd is
    // absent), and bind the socket. `_lock_guard` is held for the server's
    // lifetime; it drops at the end of `run()` — after the socket file is
    // removed at shutdown — so the lock (the authoritative occupancy
    // marker) outlives the socket. On any error after lock acquisition the
    // `?` unwinds and drops the guard, releasing the lock.
    let (display, listener, _lock_guard, socket_path) = match launch::resolve(&opts) {
        launch::Resolution::Explicit { display, lock } => {
            let guard = if lock {
                Some(launch::acquire_lock(&lock_dir, display)?)
            } else {
                None
            };
            let (listener, socket_path) = launch::bind_explicit(&socket_dir, display)?;
            (display, listener, guard, socket_path)
        }
        launch::Resolution::AutoPick => {
            let (display, listener, socket_path) = launch::autopick(&socket_dir)?;
            (display, listener, None, socket_path)
        }
    };
    log::info!("yserver: listening on unix socket DISPLAY=:{display}");
```

> Note: this removes the only uses of the bare `UnixListener` import in `lib.rs`. After editing, if `cargo clippy` flags `unused_import` for `std::os::unix::net::UnixListener` or `std::os::unix::fs::PermissionsExt`, remove them from the top-of-file `use` block (Step 6 lint will catch this).

> Behavior change vs. today: the old inline code unconditionally removed
> `X{display}` before binding; `bind_explicit` probes first and refuses a
> live socket with `AddrInUse`. Intentional — never steal a running
> server's display. A genuinely stale socket (crashed server) still
> reclaims exactly as before (`ECONNREFUSED` → remove).

- [ ] **Step 3: Signal readiness just before entering the core loop**

In `lib.rs`, immediately before the `let result = core_loop::run_core(` line (≈ 352), insert:

```rust
    // Readiness handshake: ServerState is fully constructed, the socket is
    // bound + chmod'd, and the lock is held — we can complete an initial X
    // connection setup now. This is the analog of Xorg signaling after
    // CreateConnectionBlock() and before Dispatch().
    launch::signal_ready(&opts, display, sigusr1_was_ignored);

```

- [ ] **Step 4: Update the binary shim**

Replace the entire contents of `crates/yserver/src/bin/yserver.rs` with:

```rust
use std::{env, process::ExitCode};

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let opts = match yserver::launch::parse_args(env::args().skip(1)) {
        Ok(o) => o,
        Err(err) => {
            eprintln!("yserver: {err}");
            eprintln!(
                "usage: yserver [:N | N] [vtN] [-seat NAME] [-auth FILE] \
                 [-displayfd N] [-nolisten PROTO] [-novtswitch]"
            );
            return ExitCode::FAILURE;
        }
    };

    match yserver::run(opts) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            log::error!("yserver: {err}");
            ExitCode::FAILURE
        }
    }
}
```

> The old `parse_display` + its `#[cfg(test)] mod tests` are deleted — that logic and its coverage now live in `launch.rs`.

- [ ] **Step 5: Build and run the whole crate test suite**

Run: `cargo test -p yserver --lib`
Expected: PASS, including all `launch::tests::*`. Build must succeed with the new `run(LaunchOptions)` signature and the binary shim.

- [ ] **Step 6: Format, lint, commit**

Run:
```bash
cargo +nightly fmt
cargo clippy -p yserver
```
Expected: no warnings. If clippy reports unused `UnixListener` / `PermissionsExt` imports in `lib.rs`, remove them, then re-run clippy.

```bash
git add crates/yserver/src/lib.rs crates/yserver/src/bin/yserver.rs
git commit -m "feat(launch): drive run() from LaunchOptions (lock, bind, readiness)"
```
Expected: commit succeeds.

- [ ] **Step 7: Full workspace verification**

Run:
```bash
cargo build --release --bin yserver
cargo test
cargo clippy
```
Expected: release binary builds; full test suite green; clippy clean. Fix anything red before proceeding.

---

### Task 6: hardware smoke under lightdm (manual gate — required before merge)

Per repo practice, startup/KMS-touching changes are not committed-as-done until observed on hardware. The unit tests cannot exercise SIGUSR1-to-parent, the real lockfile interop, or first light. This task is a manual checklist run on a HW machine with a GPU (e.g. silence or bee), from a TTY — **not** automated.

**Files:**
- Create: `/etc/lightdm/lightdm.conf.d/99-yserver.conf` (on the test machine; not committed)
- Create: a logging wrapper at `tools/yserver-lightdm.sh` (committed)

- [ ] **Step 1: Add a logging wrapper so lightdm's server has capturable logs**

Create `tools/yserver-lightdm.sh`:

```bash
#!/usr/bin/env bash
# lightdm launches this as its X server. lightdm appends X-style argv
# (:N -seat … -auth … -nolisten tcp vtN -novtswitch); yserver parses it
# natively. We only add logging here.
exec env RUST_LOG="${YSERVER_LOG:-info}" RUST_BACKTRACE=1 \
    /usr/local/bin/yserver "$@" 2>>/var/log/yserver-lightdm.log
```

Make it executable:
```bash
chmod +x tools/yserver-lightdm.sh
```

- [ ] **Step 2: Install the binary and point lightdm at it (on the HW machine)**

```bash
sudo install -m755 target/release/yserver /usr/local/bin/yserver
sudo install -m755 tools/yserver-lightdm.sh /usr/local/bin/yserver-lightdm
sudo mkdir -p /etc/lightdm/lightdm.conf.d
printf '[Seat:*]\nxserver-command=/usr/local/bin/yserver-lightdm\n' \
    | sudo tee /etc/lightdm/lightdm.conf.d/99-yserver.conf
```

- [ ] **Step 3: Restart lightdm from a TTY and observe**

From a text VT (Ctrl-Alt-F3), with no X running:
```bash
sudo systemctl restart lightdm
```
Expected, in order:
- `journalctl -u lightdm -b` shows lightdm starting the server and **not** timing out waiting for the ready signal ("Waiting for ready signal from X server" is followed by progress, not a timeout/respawn loop).
- `/var/log/yserver-lightdm.log` shows yserver parsing the argv, "listening on unix socket DISPLAY=:0", and "signaling readiness to parent (SIGUSR1)".
- `/tmp/.X0-lock` exists and contains yserver's PID while running.
- The **lightdm GTK greeter appears** on screen (this is the acceptance gate).

- [ ] **Step 4: Verify the restart loop leaves no leaked lock/socket**

Log in (or cancel) so lightdm cycles the server, then:
```bash
ls -l /tmp/.X0-lock /tmp/.X11-unix/X0   # after a clean server exit: both gone
```
Expected: on clean shutdown the socket is removed first, then the lock — neither lingers to block the next generation. (Leaked DRM/input state across generations is out of scope — item 5.)

- [ ] **Step 5: Commit the wrapper (only the committed file)**

```bash
git add tools/yserver-lightdm.sh
git commit -m "chore: lightdm logging wrapper for yserver xserver-command"
```

- [ ] **Step 6: Record the HW result**

Note the machine, date, and outcome (greeter shown? lock created? clean cycle?) in the PR description. If first light fails, capture `/var/log/yserver-lightdm.log` + `journalctl -u lightdm -b` and debug before merge — do not mark the chunk done on green unit tests alone.

---

## Self-Review

**Spec coverage:**
- Item 1 (argv) → Task 1 (`parse_args`, all documented tokens + tolerance/error rules).
- Display resolution table → Task 1 (`resolve`, all four rows tested).
- Display auto-selection + stale/live/non-socket handling → Task 3 (`autopick`); the `EADDRINUSE` bind-race `continue` branch is straight-line code that cannot be hit deterministically in a unit test — verified by inspection.
- Lockfile protocol (PID-suffixed temp + atomic link, O_NOFOLLOW, strict 11-byte format, stale/bogus reclaim, EPERM=alive, error-path release tested in `lock_released_when_bind_fails`, socket-then-lock ordering by code structure + HW smoke Task 6 Step 4) → Task 2 (`acquire_lock`/`inspect_lock`/`LockGuard`) + Task 3 + Task 5 Step 2 (guard lifetime/ordering).
- Item 2 readiness (`-displayfd` format, SIGUSR1 disposition query, kill(getppid)) → Task 4; timing (before core loop) → Task 5 Step 3.
- `run()` signature change + thin shim → Task 5 Steps 1/4.
- First light (unauthenticated; tolerate presented cookie) → already true in existing code (spec Key facts); verified by Task 6 Step 3.
- HW smoke gate → Task 6.

**Placeholder scan:** none — every code step shows complete code; every command shows expected output.

**Type consistency:** `LaunchOptions`, `Resolution`, `LockGuard`, `acquire_lock`, `lock_path`, `bind_explicit`, `autopick`, `write_displayfd`, `sigusr1_is_ignored`, `signal_ready_to_parent`, `signal_ready`, `DEFAULT_DISPLAY`, `resolve` are used with identical names/signatures across Tasks 1-5. `run(opts: launch::LaunchOptions)` is consistent between `lib.rs` (Task 5 Step 1) and the binary (Task 5 Step 4).
