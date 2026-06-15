//! X-server-style launch handling: argv parsing, display resolution,
//! the `/tmp/.X<N>-lock` protocol, socket binding, and the lightdm
//! readiness handshake. See
//! `docs/superpowers/specs/2026-06-12-lightdm-launch-design.md`.

use std::{
    fs::{self, OpenOptions},
    io::{self, ErrorKind, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, RawFd},
        unix::{
            fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
            net::UnixListener,
        },
    },
    path::{Path, PathBuf},
};

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
    /// `--version` / `-version` — print version + git commit and exit
    /// (handled by the binary before `run()`).
    pub show_version: bool,
}

fn next_value(it: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    it.next().ok_or_else(|| format!("{flag} requires a value"))
}

/// Parse X-server-style argv. Tolerates unknown flags (warn + skip);
/// hard-errors only on malformed *explicit* requests and missing values
/// for known value-taking flags.
pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<LaunchOptions, String> {
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
        } else if matches!(arg.as_str(), "--version" | "-version") {
            // Print-and-exit; the binary acts on this before `run()`.
            // Keep scanning so it works regardless of position.
            o.show_version = true;
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
        (Some(display), None) => Resolution::Explicit {
            display,
            lock: true,
        },
        (Some(display), Some(_)) => Resolution::Explicit {
            display,
            lock: false,
        },
        (None, Some(_)) => Resolution::AutoPick,
        (None, None) => Resolution::Explicit {
            display: DEFAULT_DISPLAY,
            lock: true,
        },
    }
}

/// RAII handle for an acquired display lock. Dropping removes the lock
/// file. `run()` holds this for the server's lifetime and lets it drop
/// *after* the socket is removed at shutdown (the lock is the
/// authoritative occupancy marker, so it must outlive the socket).
#[derive(Debug)]
pub struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Removes the temp lock file when `acquire_lock` exits by ANY path —
/// including `?` error propagation. The success path's hard link
/// survives the temp's unlink, so unconditional cleanup is safe.
struct TmpGuard {
    path: PathBuf,
}

impl Drop for TmpGuard {
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
    let _tmp_guard = TmpGuard {
        path: tmp_path.clone(),
    };

    // At most two attempts: acquire, or reclaim-once-then-acquire.
    for _ in 0..2 {
        match fs::hard_link(&tmp_path, &final_path) {
            Ok(()) => {
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
                return Err(e);
            }
        }
    }
    Err(io::Error::new(
        ErrorKind::AddrInUse,
        format!("could not acquire display lock {}", final_path.display()),
    ))
}

/// Occupancy classification for a path in the socket directory.
enum Occupancy {
    /// Nothing there — free to bind.
    Free,
    /// A server is listening (or its backlog is full — still occupied).
    Live,
    /// Socket node with no listener (`ECONNREFUSED`) — reclaimable.
    Stale,
    /// Exists but is not a socket — never delete.
    NonSocket,
    /// Unreadable/unidentifiable — never delete.
    Opaque,
}

/// Probe a socket-dir path. The connect is NONBLOCKING: a blocking
/// connect to a live-but-backlogged AF_UNIX listener waits for accept —
/// potentially forever (a wedged server, or a hostile local user's
/// never-accepting listener in the world-writable socket dir, must not
/// hang the launch). EAGAIN ⇒ backlog full ⇒ the display IS occupied.
fn probe(path: &Path) -> Occupancy {
    use nix::sys::socket::{AddressFamily, SockFlag, SockType, UnixAddr, connect, socket};

    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == ErrorKind::NotFound => return Occupancy::Free,
        Err(_) => return Occupancy::Opaque,
    };
    if !meta.file_type().is_socket() {
        return Occupancy::NonSocket;
    }
    let fd = match socket(
        AddressFamily::Unix,
        SockType::Stream,
        SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC,
        None,
    ) {
        Ok(fd) => fd,
        Err(_) => return Occupancy::Opaque,
    };
    let addr = match UnixAddr::new(path) {
        Ok(a) => a,
        Err(_) => return Occupancy::Opaque,
    };
    match connect(fd.as_raw_fd(), &addr) {
        Ok(()) => Occupancy::Live,
        // Nonblocking AF_UNIX connect outcomes:
        Err(nix::errno::Errno::EAGAIN) => Occupancy::Live, // backlog full
        Err(nix::errno::Errno::EINPROGRESS) => Occupancy::Live,
        Err(nix::errno::Errno::ECONNREFUSED) => Occupancy::Stale,
        Err(_) => Occupancy::Opaque,
    }
    // fd is an OwnedFd — closed on drop.
}

fn chmod_socket(path: &Path) -> io::Result<()> {
    // X clients connect as the invoking user; the socket needs world
    // write (connect() on AF_UNIX requires `w`). Xorg sets 0777.
    fs::set_permissions(path, fs::Permissions::from_mode(0o777))
}

/// Bind `<socket_dir>/X<display>` for the explicit-`:N` and
/// back-compat-default cases. Probes an existing socket before removing
/// it: a live server's socket is NEVER stolen (old yservers take no
/// lock, so holding the lock alone doesn't prove the display is free).
pub fn bind_explicit(socket_dir: &Path, display: u16) -> io::Result<(UnixListener, PathBuf)> {
    let path = socket_dir.join(format!("X{display}"));
    match probe(&path) {
        Occupancy::Free => {}
        Occupancy::Stale => {
            // Stale socket from a dead server — reclaim it.
            let _ = fs::remove_file(&path);
        }
        Occupancy::Live => {
            return Err(io::Error::new(
                ErrorKind::AddrInUse,
                format!("display :{display} in use (live socket {})", path.display()),
            ));
        }
        Occupancy::NonSocket => {
            return Err(io::Error::new(
                ErrorKind::AddrInUse,
                format!(
                    "{} exists and is not a socket — refusing to replace it",
                    path.display()
                ),
            ));
        }
        Occupancy::Opaque => {
            return Err(io::Error::new(
                ErrorKind::AddrInUse,
                format!("cannot probe {} — refusing to replace it", path.display()),
            ));
        }
    }
    let listener = UnixListener::bind(&path)
        .map_err(|e| io::Error::new(e.kind(), format!("bind({}): {e}", path.display())))?;
    if let Err(e) = chmod_socket(&path) {
        let _ = fs::remove_file(&path);
        return Err(e);
    }
    Ok((listener, path))
}

/// Scan `0..256` for the lowest free display and bind it. Disambiguates an
/// existing socket file via `connect()`: refused ⇒ stale ⇒ reclaim;
/// connected ⇒ live ⇒ skip; other error ⇒ occupied ⇒ skip. Moves on to the
/// next display on a `bind()` `EADDRINUSE` race. Takes no lock (matches
/// Xorg `nolock`).
pub fn autopick(socket_dir: &Path) -> io::Result<(u16, UnixListener, PathBuf)> {
    for n in 0u16..256 {
        let path = socket_dir.join(format!("X{n}"));
        match probe(&path) {
            Occupancy::Free => {}
            Occupancy::Stale => {
                // Stale socket node — reclaim it.
                let _ = fs::remove_file(&path);
            }
            // Live, non-socket, or unidentifiable: not ours — next k.
            Occupancy::Live | Occupancy::NonSocket | Occupancy::Opaque => continue,
        }
        match UnixListener::bind(&path) {
            Ok(listener) => {
                if let Err(e) = chmod_socket(&path) {
                    let _ = fs::remove_file(&path);
                    return Err(e);
                }
                return Ok((n, listener, path));
            }
            Err(e) if e.kind() == ErrorKind::AddrInUse => continue, // lost a race
            Err(e) => {
                return Err(io::Error::new(
                    e.kind(),
                    format!("bind({}): {e}", path.display()),
                ));
            }
        }
    }
    Err(io::Error::new(
        ErrorKind::AddrInUse,
        "no free X display in 0..256",
    ))
}

/// Write `"<display>\n"` (Xorg's `-displayfd` format) to `fd`, then close
/// it. Takes ownership of the fd (closed on every exit path via drop).
/// Close errors are ignored: the fd is single-use and there is nothing
/// actionable after the payload was written.
pub fn write_displayfd(fd: RawFd, display: u16) -> io::Result<()> {
    // SAFETY: ownership transfer — the caller hands us the `-displayfd`
    // fd precisely so we consume it; no other owner exists. From here,
    // `File` closes it on drop (success and error paths alike).
    let mut f = unsafe { std::fs::File::from_raw_fd(fd) };
    f.write_all(format!("{display}\n").as_bytes())
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

/// Capture the parent PID once at startup — Xorg's `InitParentProcess`
/// (`os/connection.c`). Returns `None` if already orphaned (`ppid <= 1`,
/// no DM to signal). MUST be called early, before long init during which
/// the parent could die and reparent us to a subreaper or PID 1.
#[must_use]
pub fn startup_parent_pid() -> Option<i32> {
    let ppid = unsafe { libc::getppid() };
    if ppid > 1 { Some(ppid) } else { None }
}

/// Send SIGUSR1 to the parent (the DM) captured at startup, matching
/// Xorg's `NotifyParentProcess` (`ParentProcess` set in
/// `InitParentProcess`). `parent_pid` comes from [`startup_parent_pid`].
pub fn signal_ready_to_parent(parent_pid: i32) {
    // SAFETY: kill to a captured PID; harmless if it's since exited.
    unsafe { libc::kill(parent_pid, libc::SIGUSR1) };
}

/// Perform the readiness handshake: report the chosen display on
/// `-displayfd` (if given) and signal the startup-captured parent (if
/// SIGUSR1 was inherited ignored). Call once, just before the core loop.
#[allow(clippy::collapsible_if)]
pub fn signal_ready(
    opts: &LaunchOptions,
    display: u16,
    sigusr1_was_ignored: bool,
    parent_pid: Option<i32>,
) {
    if let Some(fd) = opts.displayfd {
        if let Err(e) = write_displayfd(fd, display) {
            log::warn!("yserver: failed to write -displayfd {fd}: {e}");
        }
    }
    if sigusr1_was_ignored {
        match parent_pid {
            Some(pid) => {
                log::info!("yserver: signaling readiness to parent {pid} (SIGUSR1)");
                signal_ready_to_parent(pid);
            }
            None => log::warn!(
                "yserver: SIGUSR1 inherited-ignored but orphaned at startup — no parent to signal"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::net::UnixListener as TestListener;

    fn parse(args: &[&str]) -> Result<LaunchOptions, String> {
        parse_args(args.iter().map(|s| (*s).to_string()))
    }

    fn unique_tmp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("yserver-launch-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
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
    fn version_flag_sets_show_version() {
        assert!(parse(&["--version"]).unwrap().show_version);
        assert!(parse(&["-version"]).unwrap().show_version);
        // Default is off, and it doesn't disturb normal parsing.
        assert!(!parse(&[":0"]).unwrap().show_version);
        // Works regardless of position alongside other args.
        let o = parse(&[":3", "--version", "-seat", "seat0"]).unwrap();
        assert!(o.show_version);
        assert_eq!(o.display, Some(3));
    }

    #[test]
    fn version_line_contains_crate_version_and_commit() {
        let line = crate::version::line();
        assert!(line.starts_with("yserver "));
        assert!(line.contains(env!("CARGO_PKG_VERSION")));
        // build.rs always sets the commit env (at worst "unknown").
        assert!(line.contains(crate::version::GIT_COMMIT));
        assert!(!crate::version::GIT_COMMIT.is_empty());
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
            Resolution::Explicit {
                display: 0,
                lock: true
            }
        );
        assert_eq!(
            resolve(&mk(Some(0), Some(9))),
            Resolution::Explicit {
                display: 0,
                lock: false
            }
        );
        assert_eq!(resolve(&mk(None, Some(9))), Resolution::AutoPick);
        assert_eq!(
            resolve(&mk(None, None)),
            Resolution::Explicit {
                display: DEFAULT_DISPLAY,
                lock: true
            }
        );
    }

    #[test]
    fn unknown_flag_does_not_consume_next_arg() {
        assert_eq!(parse(&["-bogus", ":1"]).unwrap().display, Some(1));
    }

    #[test]
    fn duplicate_display_args_last_wins() {
        assert_eq!(parse(&[":0", ":1"]).unwrap().display, Some(1));
        assert_eq!(parse(&["7", ":3"]).unwrap().display, Some(3));
    }

    #[test]
    fn lock_fresh_acquire_creates_file() {
        let dir = unique_tmp_dir("lock-fresh");
        let guard = acquire_lock(&dir, 5).unwrap();
        let p = lock_path(&dir, 5);
        assert!(p.exists());
        let tmp = dir.join(format!(".tX5-lock.{}", std::process::id()));
        assert!(!tmp.exists());
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
        let tmp = dir.join(format!(".tX7-lock.{}", std::process::id()));
        assert!(!tmp.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn lock_stale_pid_is_reclaimed() {
        let dir = unique_tmp_dir("lock-stale");
        // A pid far above any real one → kill(pid,0) == ESRCH → stale.
        std::fs::write(lock_path(&dir, 8), format!("{:>10}\n", 2_147_483_646i32)).unwrap();
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

    #[test]
    fn write_displayfd_writes_ascii_and_closes() {
        // libc pipe: write end gets the display number, read end verifies.
        let mut fds = [0 as RawFd; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let (read_fd, write_fd) = (fds[0], fds[1]);

        write_displayfd(write_fd, 12).unwrap();

        let mut buf = [0u8; 8];
        let n = unsafe { libc::read(read_fd, buf.as_mut_ptr().cast(), buf.len()) };
        assert!(n > 0);
        assert_eq!(&buf[..n as usize], b"12\n");
        // EOF proves the write end was closed by write_displayfd.
        let n2 = unsafe { libc::read(read_fd, buf.as_mut_ptr().cast(), buf.len()) };
        assert_eq!(n2, 0);
        unsafe { libc::close(read_fd) };
    }

    #[test]
    fn sigusr1_disposition_roundtrip() {
        // Process-global: restore the prior disposition even on panic.
        // No other unit test in this crate touches SIGUSR1 disposition —
        // keep it that way (tests share one process).
        struct Restore(libc::sighandler_t);
        impl Drop for Restore {
            fn drop(&mut self) {
                unsafe { libc::signal(libc::SIGUSR1, self.0) };
            }
        }
        let prev = unsafe { libc::signal(libc::SIGUSR1, libc::SIG_IGN) };
        let _restore = Restore(prev);
        assert!(sigusr1_is_ignored());
        unsafe { libc::signal(libc::SIGUSR1, libc::SIG_DFL) };
        assert!(!sigusr1_is_ignored());
    }

    #[test]
    fn startup_parent_pid_is_some_under_test_runner() {
        // The test process always has a real parent (the harness), ppid > 1.
        assert!(startup_parent_pid().is_some());
    }
}
