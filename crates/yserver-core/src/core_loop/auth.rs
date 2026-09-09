//! Server-side X11 connection authorization (`-auth` / MIT-MAGIC-COOKIE-1).
//!
//! Faithful to X.Org's `os/auth.c` + `os/mitauth.c`: honor an auth file,
//! validate each client's SetupRequest cookie, reject mismatches. No auth
//! file (or zero cookies loaded) keeps local access open. See
//! docs/superpowers/specs/2026-06-25-xauth-server-auth-design.md.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::SystemTime,
};

use crate::xauth::{self, MIT_MAGIC_COOKIE};

// Reject reason strings — byte-for-byte X.Org. Note the trailing newline
// on two of them and its absence on the cookie one.
//   os/auth.c:211, os/auth.c:207, os/mitauth.c:82
const REASON_NO_PROTO: &str = "Authorization required, but no authorization protocol specified\n";
const REASON_BAD_PROTO: &str = "Authorization protocol not supported by server\n";
const REASON_BAD_COOKIE: &str = "Invalid MIT-MAGIC-COOKIE-1 key";

#[derive(Debug, PartialEq, Eq)]
pub enum AuthVerdict {
    Allow,
    Reject(&'static str),
}

/// Connection locality supplied by the accepting transport.
///
/// Unix sockets retain Xorg's existing local-open behaviour. TCP is never
/// admitted by that fallback: it must present a loaded MIT cookie.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthTransport {
    Unix,
    Tcp,
}

pub struct AuthState {
    file: Option<PathBuf>,
    inner: Mutex<Inner>,
}

struct Inner {
    /// Last successful stat mtime; `None` ≡ X.Org `lastmod == 0`.
    last_mtime: Option<SystemTime>,
    /// Latches true once ≥1 cookie has loaded (X.Org `loaded`).
    ever_loaded: bool,
    /// Host-ACL local-access toggle (collapsed to a bool on unix-only).
    local_open: bool,
    /// MIT-MAGIC-COOKIE-1 data blobs. ADDITIVE — never cleared.
    cookies: Vec<Vec<u8>>,
}

/// Reproduce X.Org's reload trigger (os/auth.c:166-175), updating
/// `last_mtime`. `stat_mtime == None` means the stat failed. Returns
/// true iff a load should run now.
fn should_reload(stat_mtime: Option<SystemTime>, last_mtime: &mut Option<SystemTime>) -> bool {
    match stat_mtime {
        Some(m) => match *last_mtime {
            Some(lm) if m > lm => {
                *last_mtime = Some(m);
                true
            }
            Some(_) => false, // not strictly newer (incl. rollback) → no reload
            None => {
                *last_mtime = Some(m);
                true
            }
        },
        None => {
            // stat failed: one-shot transition (lastmod → 0), then quiet.
            if last_mtime.is_some() {
                *last_mtime = None;
                true
            } else {
                false
            }
        }
    }
}

enum LoadOutcome {
    /// File opened, ≥1 MIT cookie decoded.
    Loaded(Vec<Vec<u8>>),
    /// File opened but no MIT cookies (empty or none parseable).
    OpenedEmpty,
    /// File could not be opened/read.
    OpenFailed,
}

impl Inner {
    fn apply_load_outcome(&mut self, outcome: LoadOutcome) {
        match outcome {
            LoadOutcome::Loaded(mut cookies) => {
                self.cookies.append(&mut cookies); // additive — os/auth.c never clears
                self.local_open = false; // DisableLocalAccess
                self.ever_loaded = true;
            }
            LoadOutcome::OpenedEmpty => {
                // EnableLocalAccess, even if previously loaded (os/auth.c:197 quirk).
                self.local_open = true;
            }
            LoadOutcome::OpenFailed => {
                // loadauth == -1: open only if never loaded; else unchanged.
                if !self.ever_loaded {
                    self.local_open = true;
                }
            }
        }
    }
}

/// Constant-time byte equality (mirrors X.Org's timingsafe_memcmp).
/// Length is compared first (X.Org does too); the byte loop is branchless.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn load_outcome(path: &Path) -> LoadOutcome {
    match fs::read(path) {
        Ok(bytes) => {
            let cookies: Vec<Vec<u8>> = xauth::parse_records(&bytes)
                .into_iter()
                .filter(|r| r.name == MIT_MAGIC_COOKIE.as_bytes())
                .map(|r| r.data)
                .collect();
            if cookies.is_empty() {
                LoadOutcome::OpenedEmpty
            } else {
                LoadOutcome::Loaded(cookies)
            }
        }
        Err(_) => LoadOutcome::OpenFailed,
    }
}

impl Inner {
    fn verdict(&self, transport: AuthTransport, name: &[u8], data: &[u8]) -> AuthVerdict {
        // Cookie match OR local-open admits (mirrors CheckAuthorization +
        // host-ACL fallthrough, os/connection.c:536-560).
        if name == MIT_MAGIC_COOKIE.as_bytes() && self.cookies.iter().any(|c| ct_eq(c, data)) {
            return AuthVerdict::Allow;
        }
        if transport == AuthTransport::Unix && self.local_open {
            return AuthVerdict::Allow;
        }
        if name.is_empty() {
            return AuthVerdict::Reject(REASON_NO_PROTO);
        }
        if name == MIT_MAGIC_COOKIE.as_bytes() {
            return AuthVerdict::Reject(REASON_BAD_COOKIE);
        }
        AuthVerdict::Reject(REASON_BAD_PROTO)
    }
}

impl AuthState {
    /// Build from `LaunchOptions.auth_file`. `None` ⇒ permanently open.
    pub fn new(file: Option<PathBuf>) -> Arc<Self> {
        // Start open; the first successful load flips this to false (Xorg ShouldLoadAuth=TRUE → never-loaded+unopenable stays open).
        let local_open = true;
        Arc::new(Self {
            file,
            inner: Mutex::new(Inner {
                last_mtime: None,
                ever_loaded: false,
                local_open,
                cookies: Vec::new(),
            }),
        })
    }

    /// Load the configured authorization file into this state and require at
    /// least one MIT cookie before a TCP listener may be enabled.
    pub fn require_tcp_auth_at_startup(&self) -> Result<(), String> {
        let Some(path) = self.file.as_deref() else {
            return Err("-listen tcp requires -auth with a MIT-MAGIC-COOKIE-1 cookie".into());
        };
        // Use the same state and file decoder as `check`, but force the first
        // load even when `stat` fails so startup and runtime share precisely
        // the same load outcome.
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        Self::reload_if_needed(&mut inner, path, true);
        if inner.cookies.is_empty() {
            return Err("-listen tcp requires -auth with a MIT-MAGIC-COOKIE-1 cookie".into());
        }
        Ok(())
    }

    fn reload_if_needed(inner: &mut Inner, path: &Path, force: bool) {
        let stat_mtime = fs::metadata(path).ok().and_then(|m| m.modified().ok());
        let changed = should_reload(stat_mtime, &mut inner.last_mtime);
        if force || changed {
            inner.apply_load_outcome(load_outcome(path));
        }
    }

    /// Authorize one client. Reloads lazily on file change, then decides.
    pub fn check(
        &self,
        transport: AuthTransport,
        proto_name: &[u8],
        proto_data: &[u8],
    ) -> AuthVerdict {
        let Some(path) = self.file.as_deref() else {
            return Inner {
                last_mtime: None,
                ever_loaded: false,
                local_open: true,
                cookies: Vec::new(),
            }
            .verdict(transport, proto_name, proto_data);
        };
        // Poisoning recovery — same pattern as setup_thread.rs:66.
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        Self::reload_if_needed(&mut inner, path, false);
        inner.verdict(transport, proto_name, proto_data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn t(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn first_successful_stat_loads() {
        let mut last = None;
        assert!(should_reload(Some(t(100)), &mut last));
        assert_eq!(last, Some(t(100)));
    }

    #[test]
    fn newer_mtime_reloads_same_mtime_does_not() {
        let mut last = Some(t(100));
        assert!(!should_reload(Some(t(100)), &mut last)); // equal → no
        assert!(should_reload(Some(t(200)), &mut last)); // newer → yes
        assert_eq!(last, Some(t(200)));
    }

    #[test]
    fn mtime_rollback_does_not_reload() {
        let mut last = Some(t(200));
        assert!(!should_reload(Some(t(100)), &mut last)); // older → no reload
        assert_eq!(last, Some(t(200)), "last_mtime unchanged on rollback");
    }

    #[test]
    fn stat_failure_is_one_shot() {
        let mut last = Some(t(200));
        assert!(
            should_reload(None, &mut last),
            "loss transition reloads once"
        );
        assert_eq!(last, None);
        assert!(!should_reload(None, &mut last), "stays quiet while missing");
        // File reappears with a newer mtime → reload.
        assert!(should_reload(Some(t(300)), &mut last));
        assert_eq!(last, Some(t(300)));
    }

    fn enforcing_with(cookies: Vec<Vec<u8>>) -> Inner {
        Inner {
            last_mtime: Some(t(1)),
            ever_loaded: true,
            local_open: false,
            cookies,
        }
    }
    fn fresh_open() -> Inner {
        Inner {
            last_mtime: None,
            ever_loaded: false,
            local_open: true,
            cookies: Vec::new(),
        }
    }

    #[test]
    fn loaded_enforces_and_appends() {
        let mut i = fresh_open();
        i.apply_load_outcome(LoadOutcome::Loaded(vec![vec![1u8; 16]]));
        assert!(!i.local_open);
        assert!(i.ever_loaded);
        assert_eq!(i.cookies, vec![vec![1u8; 16]]);
        // A second load APPENDS (additive — Xorg never clears on reload).
        i.apply_load_outcome(LoadOutcome::Loaded(vec![vec![2u8; 16]]));
        assert_eq!(i.cookies, vec![vec![1u8; 16], vec![2u8; 16]]);
    }

    #[test]
    fn empty_file_reopens_even_after_load() {
        let mut i = enforcing_with(vec![vec![1u8; 16]]);
        i.apply_load_outcome(LoadOutcome::OpenedEmpty);
        assert!(i.local_open, "empty file reopens local access (Xorg quirk)");
        assert_eq!(
            i.cookies,
            vec![vec![1u8; 16]],
            "cookies retained (additive)"
        );
    }

    #[test]
    fn open_failed_after_load_keeps_enforcing() {
        let mut i = enforcing_with(vec![vec![1u8; 16]]);
        i.apply_load_outcome(LoadOutcome::OpenFailed);
        assert!(!i.local_open, "stays enforcing — does NOT reopen");
        assert_eq!(i.cookies, vec![vec![1u8; 16]]);
    }

    #[test]
    fn open_failed_before_any_load_opens() {
        // ever_loaded=false, but local_open starts false so the branch must
        // actually flip it (deleting the OpenFailed body would now fail this).
        let mut i = Inner {
            last_mtime: None,
            ever_loaded: false,
            local_open: false,
            cookies: Vec::new(),
        };
        i.apply_load_outcome(LoadOutcome::OpenFailed);
        assert!(
            i.local_open,
            "OpenFailed before any load must enable local access"
        );
    }

    #[test]
    fn ct_eq_matches_only_identical_bytes() {
        assert!(ct_eq(&[1, 2, 3], &[1, 2, 3]));
        assert!(!ct_eq(&[1, 2, 3], &[1, 2, 4])); // same len, differ
        assert!(!ct_eq(&[1, 2, 3], &[1, 2])); // differing len
        assert!(ct_eq(&[], &[]));
    }

    #[test]
    fn verdict_cookie_match_allows() {
        let i = enforcing_with(vec![vec![9u8; 16]]);
        assert_eq!(
            i.verdict(AuthTransport::Unix, MIT_MAGIC_COOKIE.as_bytes(), &[9u8; 16]),
            AuthVerdict::Allow
        );
    }

    #[test]
    fn verdict_enforcing_rejects() {
        let i = enforcing_with(vec![vec![9u8; 16]]);
        assert_eq!(
            i.verdict(AuthTransport::Unix, MIT_MAGIC_COOKIE.as_bytes(), &[0u8; 16]),
            AuthVerdict::Reject(REASON_BAD_COOKIE)
        );
        assert_eq!(
            i.verdict(AuthTransport::Unix, b"", b""),
            AuthVerdict::Reject(REASON_NO_PROTO)
        );
        assert_eq!(
            i.verdict(AuthTransport::Unix, b"XDM-AUTHORIZATION-1", &[0u8; 8]),
            AuthVerdict::Reject(REASON_BAD_PROTO)
        );
    }

    #[test]
    fn verdict_local_open_allows_anything() {
        let mut i = fresh_open();
        i.local_open = true;
        assert_eq!(i.verdict(AuthTransport::Unix, b"", b""), AuthVerdict::Allow);
        assert_eq!(
            i.verdict(AuthTransport::Unix, MIT_MAGIC_COOKIE.as_bytes(), &[0u8; 16]),
            AuthVerdict::Allow
        );
    }

    // ---- check() over a real temp file ----

    fn xauth_bytes(number: &[u8], cookie: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&256u16.to_be_bytes()); // FamilyLocal
        for f in [
            b"host".as_slice(),
            number,
            MIT_MAGIC_COOKIE.as_bytes(),
            cookie,
        ] {
            b.extend_from_slice(&(f.len() as u16).to_be_bytes());
            b.extend_from_slice(f);
        }
        b
    }

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("yserver-auth-test-{}-{tag}", std::process::id()))
    }

    #[test]
    fn check_enforces_against_file_cookie() {
        let cookie = [0xABu8; 16];
        let path = temp_path("enforce");
        fs::write(&path, xauth_bytes(b"7", &cookie)).unwrap();
        let auth = AuthState::new(Some(path.clone()));

        assert_eq!(
            auth.check(AuthTransport::Unix, MIT_MAGIC_COOKIE.as_bytes(), &cookie),
            AuthVerdict::Allow
        );
        assert_eq!(
            auth.check(AuthTransport::Unix, MIT_MAGIC_COOKIE.as_bytes(), &[0u8; 16]),
            AuthVerdict::Reject(REASON_BAD_COOKIE)
        );
        assert_eq!(
            auth.check(AuthTransport::Unix, b"", b""),
            AuthVerdict::Reject(REASON_NO_PROTO)
        );

        // File vanishes after a successful load → stays enforcing; the
        // already-loaded cookie still validates (additive list).
        fs::remove_file(&path).unwrap();
        assert_eq!(
            auth.check(AuthTransport::Unix, MIT_MAGIC_COOKIE.as_bytes(), &cookie),
            AuthVerdict::Allow
        );
        assert_eq!(
            auth.check(AuthTransport::Unix, MIT_MAGIC_COOKIE.as_bytes(), &[0u8; 16]),
            AuthVerdict::Reject(REASON_BAD_COOKIE),
            "missing-after-load does not reopen"
        );
    }

    #[test]
    fn check_no_auth_file_is_open() {
        let auth = AuthState::new(None);
        assert_eq!(
            auth.check(AuthTransport::Unix, b"", b""),
            AuthVerdict::Allow
        );
        assert_eq!(
            auth.check(AuthTransport::Tcp, b"", b""),
            AuthVerdict::Reject(REASON_NO_PROTO),
            "TCP must never inherit Unix local-open access"
        );
    }

    #[test]
    fn check_empty_file_is_open() {
        let path = temp_path("empty");
        fs::write(&path, b"").unwrap();
        let auth = AuthState::new(Some(path.clone()));
        assert_eq!(
            auth.check(AuthTransport::Unix, b"", b""),
            AuthVerdict::Allow
        );
        assert_eq!(
            auth.check(AuthTransport::Tcp, b"", b""),
            AuthVerdict::Reject(REASON_NO_PROTO),
            "an empty -auth file cannot authorize TCP"
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn check_missing_file_at_startup_is_open() {
        // -auth names a path that does not exist when the first client
        // connects. Xorg opens local access in this never-loaded case;
        // we must too (fail-open here, matching Xorg — not reject-all).
        let path = temp_path("missing");
        let _ = fs::remove_file(&path); // ensure absent
        let auth = AuthState::new(Some(path));
        assert_eq!(
            auth.check(AuthTransport::Unix, b"", b""),
            AuthVerdict::Allow
        );
        assert_eq!(
            auth.check(AuthTransport::Tcp, MIT_MAGIC_COOKIE.as_bytes(), &[0u8; 16]),
            AuthVerdict::Reject(REASON_BAD_COOKIE),
            "a missing auth file cannot authorize TCP"
        );
    }

    #[test]
    fn tcp_auth_requires_a_loaded_matching_cookie() {
        let cookie = [0x51u8; 16];
        let path = temp_path("tcp-cookie");
        fs::write(&path, xauth_bytes(b"7", &cookie)).unwrap();
        let auth = AuthState::new(Some(path.clone()));

        assert_eq!(
            auth.check(AuthTransport::Tcp, MIT_MAGIC_COOKIE.as_bytes(), &cookie),
            AuthVerdict::Allow
        );
        assert_eq!(
            auth.check(AuthTransport::Tcp, MIT_MAGIC_COOKIE.as_bytes(), &[0u8; 16]),
            AuthVerdict::Reject(REASON_BAD_COOKIE)
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn startup_tcp_auth_loads_the_runtime_cookie_state() {
        let cookie = [0x3Du8; 16];
        let path = temp_path("startup-tcp-cookie");
        fs::write(&path, xauth_bytes(b"7", &cookie)).unwrap();
        let auth = AuthState::new(Some(path.clone()));

        assert!(auth.require_tcp_auth_at_startup().is_ok());
        assert_eq!(
            auth.check(AuthTransport::Tcp, MIT_MAGIC_COOKIE.as_bytes(), &cookie),
            AuthVerdict::Allow,
            "startup validation must load the exact state used at runtime"
        );
        let inner = auth.inner.lock().unwrap();
        assert_eq!(
            inner.cookies,
            vec![cookie.to_vec()],
            "runtime must see the startup load rather than append a second load"
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn startup_tcp_auth_rejects_missing_or_empty_auth_files() {
        assert!(AuthState::new(None).require_tcp_auth_at_startup().is_err());

        let missing = temp_path("startup-tcp-missing");
        let _ = fs::remove_file(&missing);
        assert!(
            AuthState::new(Some(missing))
                .require_tcp_auth_at_startup()
                .is_err()
        );

        let empty = temp_path("startup-tcp-empty");
        fs::write(&empty, b"").unwrap();
        assert!(
            AuthState::new(Some(empty.clone()))
                .require_tcp_auth_at_startup()
                .is_err()
        );
        let _ = fs::remove_file(empty);
    }
}
