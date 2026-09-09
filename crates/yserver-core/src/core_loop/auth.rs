//! Server-side X11 connection authorization (`-auth` / MIT-MAGIC-COOKIE-1).
//!
//! Faithful to X.Org's `os/auth.c` + `os/mitauth.c`: honor an auth file,
//! validate each client's SetupRequest cookie, reject mismatches. No auth
//! file (or zero cookies loaded) keeps local access open. See
//! docs/superpowers/specs/2026-06-25-xauth-server-auth-design.md.
//!
//! Beside the file cookies sits **one** in-memory *session credential*: the
//! cookie an XDMCP `Accept` carries (`../xserver/os/xdmcp.c:1168`). It is not
//! another entry in the same list. It is bound to the server generation the
//! offer belongs to, and a setup thread authenticates against it only if the
//! thread's own producer binding — [`BoundSender`], captured at accept —
//! names that same generation. A reset then invalidates it by *mismatch*,
//! with no clear call at the boundary that could be missed or raced;
//! `AuthState` deliberately outlives a reset, so it cannot rely on being torn
//! down. Generation binding alone is not enough, because an offer can be
//! abandoned inside one generation (`recv_refuse_msg`, `xdmcp.c:1264`), so
//! there is also an explicit clear. See
//! docs/superpowers/specs/2026-09-09-xdmcp-design.md, "`AuthState` cannot
//! accept a cookie at runtime".
//!
//! [`BoundSender`]: crate::core_loop::sender::BoundSender

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::SystemTime,
};

use super::generation::Generation;
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
    /// XDMCP is driving this display. Two consequences, both from the design
    /// doc's "stage-1 contradiction": `-listen tcp` no longer needs a usable
    /// `-auth` file at startup (the cookie arrives later, in an `Accept`),
    /// and a TCP client is then authorized by the session credential
    /// **alone** — a cookie sitting in a local file must not admit a client
    /// to a session it has nothing to do with. Unix clients are unaffected.
    xdmcp: bool,
    inner: Mutex<Inner>,
}

/// The cookie from one accepted XDMCP offer, bound to the generation that
/// offer belongs to.
struct SessionCredential {
    generation: Generation,
    /// MIT-MAGIC-COOKIE-1 data. Never empty — see
    /// [`AuthState::install_session_cookie`].
    cookie: Vec<u8>,
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
    /// The XDMCP session credential. ONE slot: a new `Accept` replaces it,
    /// an abandoned offer clears it. Deliberately not merged into `cookies`,
    /// which is additive and process-lifetime — the opposite lifetime.
    session: Option<SessionCredential>,
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
    /// Whether the installed session credential authorizes a setup thread
    /// bound to `setup_generation`.
    ///
    /// The generation compared here is the **setup thread's own** binding,
    /// not the counter's current value. That is the whole mechanism: a
    /// producer is bound at accept and never re-reads the counter, so after
    /// a reset the previous session's client carries the previous
    /// generation and fails this comparison — while a client accepted in the
    /// new generation fails it too, until a fresh `Accept` installs a
    /// credential bound to that new generation.
    fn session_authorizes(&self, setup_generation: Generation, name: &[u8], data: &[u8]) -> bool {
        let Some(session) = self.session.as_ref() else {
            return false;
        };
        session.generation == setup_generation
            && name == MIT_MAGIC_COOKIE.as_bytes()
            && ct_eq(&session.cookie, data)
    }

    fn reject(name: &[u8]) -> AuthVerdict {
        if name.is_empty() {
            return AuthVerdict::Reject(REASON_NO_PROTO);
        }
        if name == MIT_MAGIC_COOKIE.as_bytes() {
            return AuthVerdict::Reject(REASON_BAD_COOKIE);
        }
        AuthVerdict::Reject(REASON_BAD_PROTO)
    }

    fn verdict(
        &self,
        transport: AuthTransport,
        xdmcp: bool,
        setup_generation: Generation,
        name: &[u8],
        data: &[u8],
    ) -> AuthVerdict {
        if self.session_authorizes(setup_generation, name, data) {
            return AuthVerdict::Allow;
        }
        // In XDMCP mode the session credential is the ONLY thing that
        // authorizes TCP: stop before the file cookies and before the
        // local-open fallback. Before the first `Accept` there is no
        // credential at all, so this is also where TCP fails closed.
        if xdmcp && transport == AuthTransport::Tcp {
            return Self::reject(name);
        }
        // Cookie match OR local-open admits (mirrors CheckAuthorization +
        // host-ACL fallthrough, os/connection.c:536-560).
        if name == MIT_MAGIC_COOKIE.as_bytes() && self.cookies.iter().any(|c| ct_eq(c, data)) {
            return AuthVerdict::Allow;
        }
        if transport == AuthTransport::Unix && self.local_open {
            return AuthVerdict::Allow;
        }
        Self::reject(name)
    }
}

impl AuthState {
    /// Build from `LaunchOptions.auth_file`. `None` ⇒ permanently open.
    pub fn new(file: Option<PathBuf>) -> Arc<Self> {
        Self::new_with_xdmcp(file, false)
    }

    /// Build from `LaunchOptions.auth_file` and whether `LaunchOptions.xdmcp`
    /// is set. See [`AuthState::xdmcp`] for what the flag changes.
    pub fn new_with_xdmcp(file: Option<PathBuf>, xdmcp: bool) -> Arc<Self> {
        // Start open; the first successful load flips this to false (Xorg ShouldLoadAuth=TRUE → never-loaded+unopenable stays open).
        let local_open = true;
        Arc::new(Self {
            file,
            xdmcp,
            inner: Mutex::new(Inner {
                last_mtime: None,
                ever_loaded: false,
                local_open,
                cookies: Vec::new(),
                session: None,
            }),
        })
    }

    /// Install the credential from an accepted XDMCP offer, bound to
    /// `generation` — the generation running when the `Accept` was processed,
    /// which is the one whose clients the cookie may authorize. Replaces any
    /// credential already installed: there is one slot, and a new offer
    /// supersedes the old one.
    ///
    /// Returns whether anything was installed. An unusable credential
    /// installs **nothing** and clears the slot, failing closed:
    ///
    /// * a name other than MIT-MAGIC-COOKIE-1 is the only authorization
    ///   protocol this layer knows how to check, and
    /// * empty data is load-bearing rather than tidiness — [`ct_eq`] returns
    ///   true for two empty slices, so an empty credential would match every
    ///   client that presents an empty cookie: an open display dressed as an
    ///   authenticated one.
    ///
    /// The state machine's `recv_accept` rejects both cases before emitting
    /// `InstallCookie`; this is the second line, and where the property is
    /// actually enforced against whatever ends up calling in.
    pub fn install_session_cookie(&self, generation: Generation, name: &[u8], data: &[u8]) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if name != MIT_MAGIC_COOKIE.as_bytes() || data.is_empty() {
            inner.session = None;
            return false;
        }
        inner.session = Some(SessionCredential {
            generation,
            cookie: data.to_vec(),
        });
        true
    }

    /// Drop the session credential. Services the state machine's
    /// `ClearCookie`, which is emitted when an accepted offer is abandoned
    /// *within* a generation — `Refuse` takes `AwaitManageResponse` back to
    /// `StartConnection` with no generation change (`xdmcp.c:1264`), so the
    /// generation binding cannot invalidate the refused offer's cookie and
    /// this must.
    pub fn clear_session_cookie(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.session = None;
    }

    /// Load the configured authorization file into this state and require at
    /// least one MIT cookie before a TCP listener may be enabled.
    pub fn require_tcp_auth_at_startup(&self) -> Result<(), String> {
        if self.xdmcp {
            // XDMCP is an approved dynamic authorization source, so it
            // satisfies this check in place of `-auth`: the session cookie
            // arrives in an `Accept`, but TCP has to be listening already for
            // the manager's session to reach us. Nothing is authorized in the
            // meantime — `verdict` refuses every TCP setup until a credential
            // is installed.
            return Ok(());
        }
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
    ///
    /// `setup_generation` is the generation the *calling setup thread* was
    /// bound to at accept (`BoundSender::generation`), never the counter's
    /// current value: reading the counter here would re-admit a client of the
    /// destroyed session the instant a reset installed a new credential.
    pub fn check(
        &self,
        transport: AuthTransport,
        setup_generation: Generation,
        proto_name: &[u8],
        proto_data: &[u8],
    ) -> AuthVerdict {
        // Poisoning recovery — same pattern as setup_thread.rs:66.
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        // With no `-auth` file there is nothing to reload; the initial state
        // (`local_open`, no file cookies) is already what such a server has,
        // and is never mutated. The session credential still applies.
        if let Some(path) = self.file.as_deref() {
            Self::reload_if_needed(&mut inner, path, false);
        }
        inner.verdict(
            transport,
            self.xdmcp,
            setup_generation,
            proto_name,
            proto_data,
        )
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
            session: None,
        }
    }
    fn fresh_open() -> Inner {
        Inner {
            last_mtime: None,
            ever_loaded: false,
            local_open: true,
            cookies: Vec::new(),
            session: None,
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
            session: None,
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
            i.verdict(
                AuthTransport::Unix,
                false,
                Generation::default(),
                MIT_MAGIC_COOKIE.as_bytes(),
                &[9u8; 16]
            ),
            AuthVerdict::Allow
        );
    }

    #[test]
    fn verdict_enforcing_rejects() {
        let i = enforcing_with(vec![vec![9u8; 16]]);
        assert_eq!(
            i.verdict(
                AuthTransport::Unix,
                false,
                Generation::default(),
                MIT_MAGIC_COOKIE.as_bytes(),
                &[0u8; 16]
            ),
            AuthVerdict::Reject(REASON_BAD_COOKIE)
        );
        assert_eq!(
            i.verdict(AuthTransport::Unix, false, Generation::default(), b"", b""),
            AuthVerdict::Reject(REASON_NO_PROTO)
        );
        assert_eq!(
            i.verdict(
                AuthTransport::Unix,
                false,
                Generation::default(),
                b"XDM-AUTHORIZATION-1",
                &[0u8; 8]
            ),
            AuthVerdict::Reject(REASON_BAD_PROTO)
        );
    }

    #[test]
    fn verdict_local_open_allows_anything() {
        let mut i = fresh_open();
        i.local_open = true;
        assert_eq!(
            i.verdict(AuthTransport::Unix, false, Generation::default(), b"", b""),
            AuthVerdict::Allow
        );
        assert_eq!(
            i.verdict(
                AuthTransport::Unix,
                false,
                Generation::default(),
                MIT_MAGIC_COOKIE.as_bytes(),
                &[0u8; 16]
            ),
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
            auth.check(
                AuthTransport::Unix,
                Generation::default(),
                MIT_MAGIC_COOKIE.as_bytes(),
                &cookie
            ),
            AuthVerdict::Allow
        );
        assert_eq!(
            auth.check(
                AuthTransport::Unix,
                Generation::default(),
                MIT_MAGIC_COOKIE.as_bytes(),
                &[0u8; 16]
            ),
            AuthVerdict::Reject(REASON_BAD_COOKIE)
        );
        assert_eq!(
            auth.check(AuthTransport::Unix, Generation::default(), b"", b""),
            AuthVerdict::Reject(REASON_NO_PROTO)
        );

        // File vanishes after a successful load → stays enforcing; the
        // already-loaded cookie still validates (additive list).
        fs::remove_file(&path).unwrap();
        assert_eq!(
            auth.check(
                AuthTransport::Unix,
                Generation::default(),
                MIT_MAGIC_COOKIE.as_bytes(),
                &cookie
            ),
            AuthVerdict::Allow
        );
        assert_eq!(
            auth.check(
                AuthTransport::Unix,
                Generation::default(),
                MIT_MAGIC_COOKIE.as_bytes(),
                &[0u8; 16]
            ),
            AuthVerdict::Reject(REASON_BAD_COOKIE),
            "missing-after-load does not reopen"
        );
    }

    #[test]
    fn check_no_auth_file_is_open() {
        let auth = AuthState::new(None);
        assert_eq!(
            auth.check(AuthTransport::Unix, Generation::default(), b"", b""),
            AuthVerdict::Allow
        );
        assert_eq!(
            auth.check(AuthTransport::Tcp, Generation::default(), b"", b""),
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
            auth.check(AuthTransport::Unix, Generation::default(), b"", b""),
            AuthVerdict::Allow
        );
        assert_eq!(
            auth.check(AuthTransport::Tcp, Generation::default(), b"", b""),
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
            auth.check(AuthTransport::Unix, Generation::default(), b"", b""),
            AuthVerdict::Allow
        );
        assert_eq!(
            auth.check(
                AuthTransport::Tcp,
                Generation::default(),
                MIT_MAGIC_COOKIE.as_bytes(),
                &[0u8; 16]
            ),
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
            auth.check(
                AuthTransport::Tcp,
                Generation::default(),
                MIT_MAGIC_COOKIE.as_bytes(),
                &cookie
            ),
            AuthVerdict::Allow
        );
        assert_eq!(
            auth.check(
                AuthTransport::Tcp,
                Generation::default(),
                MIT_MAGIC_COOKIE.as_bytes(),
                &[0u8; 16]
            ),
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
            auth.check(
                AuthTransport::Tcp,
                Generation::default(),
                MIT_MAGIC_COOKIE.as_bytes(),
                &cookie
            ),
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

    // ---- regression fence: the four pre-XDMCP local-auth cases ----
    //
    // Pinned as one test BEFORE the session credential was added to
    // `check`, so a change in any of them is visible as this test going
    // red rather than as a subtle shift spread over the suite.

    #[test]
    fn local_file_auth_behaviour_is_pinned() {
        // 1. No -auth file at all: Unix open, TCP closed.
        let none = AuthState::new(None);
        assert_eq!(
            none.check(AuthTransport::Unix, Generation::default(), b"", b""),
            AuthVerdict::Allow
        );
        assert_eq!(
            none.check(AuthTransport::Tcp, Generation::default(), b"", b""),
            AuthVerdict::Reject(REASON_NO_PROTO)
        );

        // 2. -auth names an unreadable (absent) file: never loaded, so
        //    Unix stays open and TCP still cannot be authorized.
        let missing = temp_path("fence-missing");
        let _ = fs::remove_file(&missing);
        let unreadable = AuthState::new(Some(missing));
        assert_eq!(
            unreadable.check(AuthTransport::Unix, Generation::default(), b"", b""),
            AuthVerdict::Allow
        );
        assert_eq!(
            unreadable.check(
                AuthTransport::Tcp,
                Generation::default(),
                MIT_MAGIC_COOKIE.as_bytes(),
                &[7u8; 16]
            ),
            AuthVerdict::Reject(REASON_BAD_COOKIE)
        );

        // 3 + 4. A loaded file: the right cookie is admitted on both
        //        transports, the wrong one is refused on both.
        let cookie = [0x2Fu8; 16];
        let path = temp_path("fence-loaded");
        fs::write(&path, xauth_bytes(b"7", &cookie)).unwrap();
        let loaded = AuthState::new(Some(path.clone()));
        for transport in [AuthTransport::Unix, AuthTransport::Tcp] {
            assert_eq!(
                loaded.check(
                    transport,
                    Generation::default(),
                    MIT_MAGIC_COOKIE.as_bytes(),
                    &cookie
                ),
                AuthVerdict::Allow,
                "{transport:?}"
            );
            assert_eq!(
                loaded.check(
                    transport,
                    Generation::default(),
                    MIT_MAGIC_COOKIE.as_bytes(),
                    &[0u8; 16]
                ),
                AuthVerdict::Reject(REASON_BAD_COOKIE),
                "{transport:?}"
            );
        }
        let _ = fs::remove_file(path);
    }

    // ---- the XDMCP session credential ----

    const MIT: &[u8] = MIT_MAGIC_COOKIE.as_bytes();

    #[test]
    fn a_reset_invalidates_the_session_cookie_by_generation_mismatch() {
        use crate::core_loop::sender::channel;

        let (_poll, sender, rx) = channel().unwrap();
        let auth = AuthState::new_with_xdmcp(None, true);
        let first = [0xC1u8; 16];

        // Session 1: a client accepted now, and the `Accept` that arrives for
        // the generation it was accepted in.
        let old_client = sender.bind();
        assert!(auth.install_session_cookie(rx.current_generation(), MIT, &first));
        assert_eq!(
            auth.check(AuthTransport::Tcp, old_client.generation(), MIT, &first),
            AuthVerdict::Allow
        );

        // The reset. Nothing calls into `AuthState` here — that is the point.
        let new_generation = rx.generation_counter().bump();
        let new_client = sender.bind();
        assert_eq!(new_client.generation(), new_generation);
        assert_ne!(new_client.generation(), old_client.generation());

        // A client of the new session presenting the PREVIOUS session's
        // cookie is refused.
        assert_eq!(
            auth.check(AuthTransport::Tcp, new_client.generation(), MIT, &first),
            AuthVerdict::Reject(REASON_BAD_COOKIE)
        );

        // ...and refused by MISMATCH, not because something cleared the slot:
        // the identical cookie still satisfies a producer bound to the old
        // generation, so the credential is demonstrably still installed. (Such
        // a producer is quarantined elsewhere, by
        // `generation::should_dispatch`; all this asserts is which comparison
        // produced the refusal above.)
        assert_eq!(
            auth.check(AuthTransport::Tcp, old_client.generation(), MIT, &first),
            AuthVerdict::Allow,
            "the refusal must come from the generation comparison, not from an \
             empty slot — otherwise this test passes for the wrong reason"
        );

        // The new session is authorized only once its own `Accept` lands, and
        // that install retires the old credential for everyone.
        let second = [0xC2u8; 16];
        assert!(auth.install_session_cookie(new_generation, MIT, &second));
        assert_eq!(
            auth.check(AuthTransport::Tcp, new_client.generation(), MIT, &second),
            AuthVerdict::Allow
        );
        assert_eq!(
            auth.check(AuthTransport::Tcp, new_client.generation(), MIT, &first),
            AuthVerdict::Reject(REASON_BAD_COOKIE)
        );
        assert_eq!(
            auth.check(AuthTransport::Tcp, old_client.generation(), MIT, &first),
            AuthVerdict::Reject(REASON_BAD_COOKIE),
            "one slot: the previous session's cookie is gone once replaced"
        );
        assert_eq!(
            auth.check(AuthTransport::Tcp, old_client.generation(), MIT, &second),
            AuthVerdict::Reject(REASON_BAD_COOKIE),
            "nor may an old client borrow the new session's cookie"
        );
    }

    #[test]
    fn an_abandoned_offer_clears_the_cookie_inside_one_generation() {
        // No reset anywhere in this test: `Refuse` sends the machine from
        // AwaitManageResponse back to StartConnection to resend `Request`
        // (xdmcp.c:1264) without changing the generation, so the binding
        // cannot invalidate the refused offer's cookie.
        let auth = AuthState::new_with_xdmcp(None, true);
        let generation = Generation::default();
        let refused = [0xA1u8; 16];
        let accepted = [0xB2u8; 16];

        assert!(auth.install_session_cookie(generation, MIT, &refused));
        assert_eq!(
            auth.check(AuthTransport::Tcp, generation, MIT, &refused),
            AuthVerdict::Allow
        );

        // The abandonment itself — and the assertion that carries this test.
        // Checking only after the replacement lands would pass against an
        // implementation with no clear at all, because the second install
        // overwrites the slot: the window that matters is the one BETWEEN the
        // `Refuse` and the next `Accept`, which can be several retransmits
        // long.
        auth.clear_session_cookie();
        assert_eq!(
            auth.check(AuthTransport::Tcp, generation, MIT, &refused),
            AuthVerdict::Reject(REASON_BAD_COOKIE),
            "the refused offer's cookie must stop authorizing immediately, not \
             merely once a replacement arrives"
        );

        // The replacement offer, in the same generation.
        assert!(auth.install_session_cookie(generation, MIT, &accepted));
        assert_eq!(
            auth.check(AuthTransport::Tcp, generation, MIT, &accepted),
            AuthVerdict::Allow
        );
        assert_eq!(
            auth.check(AuthTransport::Tcp, generation, MIT, &refused),
            AuthVerdict::Reject(REASON_BAD_COOKIE)
        );
    }

    #[test]
    fn tcp_setup_before_any_accept_is_refused() {
        // `-query host -listen tcp`: the listener is up, the manager has not
        // answered yet. Nothing authorizes a TCP client in that window.
        let auth = AuthState::new_with_xdmcp(None, true);
        let generation = Generation::default();

        assert_eq!(
            auth.check(AuthTransport::Tcp, generation, MIT, &[0x11u8; 16]),
            AuthVerdict::Reject(REASON_BAD_COOKIE)
        );
        assert_eq!(
            auth.check(AuthTransport::Tcp, generation, b"", b""),
            AuthVerdict::Reject(REASON_NO_PROTO)
        );
        assert_eq!(
            auth.check(AuthTransport::Tcp, generation, MIT, b""),
            AuthVerdict::Reject(REASON_BAD_COOKIE),
            "an empty presentation must not match an empty slot"
        );
        // Unix is unaffected: no -auth file, so local access is open as ever.
        assert_eq!(
            auth.check(AuthTransport::Unix, generation, b"", b""),
            AuthVerdict::Allow
        );
    }

    #[test]
    fn in_xdmcp_mode_a_file_cookie_authorizes_unix_but_never_tcp() {
        let file_cookie = [0x5Au8; 16];
        let path = temp_path("xdmcp-file-cookie");
        fs::write(&path, xauth_bytes(b"7", &file_cookie)).unwrap();
        let generation = Generation::default();

        let auth = AuthState::new_with_xdmcp(Some(path.clone()), true);
        assert_eq!(
            auth.check(AuthTransport::Unix, generation, MIT, &file_cookie),
            AuthVerdict::Allow,
            "Unix clients are unaffected by XDMCP mode"
        );
        assert_eq!(
            auth.check(AuthTransport::Tcp, generation, MIT, &file_cookie),
            AuthVerdict::Reject(REASON_BAD_COOKIE),
            "a cookie in a local file must not authorize a client into an \
             XDMCP session it has nothing to do with"
        );

        // The session credential is the only thing that admits TCP here.
        let session_cookie = [0x6Bu8; 16];
        assert!(auth.install_session_cookie(generation, MIT, &session_cookie));
        assert_eq!(
            auth.check(AuthTransport::Tcp, generation, MIT, &session_cookie),
            AuthVerdict::Allow
        );
        assert_eq!(
            auth.check(AuthTransport::Tcp, generation, MIT, &file_cookie),
            AuthVerdict::Reject(REASON_BAD_COOKIE),
            "installing a session cookie does not un-shadow the file ones"
        );

        // Same file, no XDMCP: the gate is the mode, not anything about the
        // file or the cookie.
        let plain = AuthState::new(Some(path.clone()));
        assert_eq!(
            plain.check(AuthTransport::Tcp, generation, MIT, &file_cookie),
            AuthVerdict::Allow
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn an_empty_cookie_is_never_installed() {
        let auth = AuthState::new_with_xdmcp(None, true);
        let generation = Generation::default();

        assert!(!auth.install_session_cookie(generation, MIT, b""));
        // `ct_eq(&[], &[])` is true, so an installed empty credential would
        // admit precisely this client — an open display dressed as an
        // authenticated one.
        assert_eq!(
            auth.check(AuthTransport::Tcp, generation, MIT, b""),
            AuthVerdict::Reject(REASON_BAD_COOKIE)
        );

        // An authorization name this layer cannot check is refused too.
        assert!(!auth.install_session_cookie(generation, b"XDM-AUTHORIZATION-1", &[1u8; 8]));
        assert_eq!(
            auth.check(
                AuthTransport::Tcp,
                generation,
                b"XDM-AUTHORIZATION-1",
                &[1u8; 8]
            ),
            AuthVerdict::Reject(REASON_BAD_PROTO)
        );

        // And a refused install fails closed: it drops whatever was in the
        // slot rather than leaving the previous offer's cookie live.
        let good = [0x77u8; 16];
        assert!(auth.install_session_cookie(generation, MIT, &good));
        assert!(!auth.install_session_cookie(generation, MIT, b""));
        assert_eq!(
            auth.check(AuthTransport::Tcp, generation, MIT, &good),
            AuthVerdict::Reject(REASON_BAD_COOKIE)
        );
    }

    #[test]
    fn xdmcp_mode_satisfies_the_tcp_startup_check_without_an_auth_file() {
        // Stage 1 hard-errors here; XDMCP is an approved dynamic source, so
        // the listener may come up before any cookie exists.
        assert!(
            AuthState::new_with_xdmcp(None, true)
                .require_tcp_auth_at_startup()
                .is_ok()
        );
        // Unchanged without it.
        assert!(
            AuthState::new_with_xdmcp(None, false)
                .require_tcp_auth_at_startup()
                .is_err()
        );
    }
}
