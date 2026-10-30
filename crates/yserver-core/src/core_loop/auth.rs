//! Server-side X11 connection authorization (`-auth` / MIT-MAGIC-COOKIE-1).
//!
//! Faithful to X.Org's `os/auth.c` + `os/mitauth.c`: honor an auth file,
//! validate each client's SetupRequest cookie, reject mismatches. No auth
//! file (or zero cookies loaded) keeps local access open, except under XDMCP,
//! where cookie-less local access is never opened (os/access.c:387). See
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

use super::{
    generation::Generation,
    host_access::{
        BadValue, FAMILY_INTERNET, FAMILY_INTERNET6, FAMILY_LOCAL_HOST, FAMILY_SERVER_INTERPRETED,
        HostEntry, HostList, NameResolver, SiEntry, SiType, SystemResolver, check_addr,
        si_matches_local,
    },
};
use crate::{
    peer::{PeerCred, PeerIdentity, own_interface_addrs},
    xauth::{self, MIT_MAGIC_COOKIE},
};

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
    /// to a session it has nothing to do with. Unix clients still take file
    /// cookies, but never the cookie-less local-open fallback (Xorg
    /// `AccessUsingXdmcp`).
    xdmcp: bool,
    /// A TCP listener is configured. Without one Xorg's `selfhosts` is
    /// empty (os/connection.c:287-288), so no list entry stands for this
    /// host and admits a local client.
    tcp_listen: bool,
    /// Our own interface addresses, the interface part of Xorg's
    /// `selfhosts`. Production: [`own_interface_addrs`], queried per use
    /// like `address_is_ours`. A seam so tests do not depend on the host's
    /// interfaces.
    own_addrs: fn() -> Vec<HostEntry>,
    inner: Mutex<Inner>,
}

/// The core thread's handle on the process-lifetime [`AuthState`], for
/// `ChangeHosts`, `ListHosts` and `SetAccessControl`, with the generation the
/// core is running. Every `AuthState` call from the core passes
/// `generation`. `run_core` installs the real one; a reset carries `auth`
/// into the fresh state with the new generation.
#[derive(Clone)]
pub struct HostAccessHandle {
    pub auth: Arc<AuthState>,
    pub generation: Generation,
}

impl Default for HostAccessHandle {
    /// A private `AuthState` with no `-auth` file, for states that never
    /// run under `run_core` (unit tests, before installation).
    fn default() -> Self {
        Self {
            auth: AuthState::new(None),
            generation: Generation::default(),
        }
    }
}

impl std::fmt::Debug for HostAccessHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostAccessHandle")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

/// Why a `ChangeHosts` failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostChangeError {
    /// `BadAccess`: the write came from an older, quarantined generation.
    Access,
    /// `BadValue`, carrying the length or the family as [`BadValue`] says.
    Value(BadValue),
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
    /// Xorg's `LocalHostEnabled`: admits every cookie-less Unix client. Three
    /// writers, as in Xorg: the auth-file load outcome, `ChangeHosts` of
    /// `FamilyLocalHost` (`xhost ±local:`), and the reset (`ResetHosts`, via
    /// [`Inner::enter`]). Only meaningful for `generation`.
    local_open: bool,
    /// Xorg's `validhosts` and `AccessEnabled`. Reset with `local_open`.
    hosts: HostList,
    /// The generation `local_open` and `hosts` belong to. `AuthState`
    /// outlives a reset, so the reset is applied by the first access from a
    /// newer generation ([`Inner::enter`]), never by a call at the boundary.
    generation: Generation,
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

/// How an access relates to the generation `Inner` is tagged with.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Access {
    /// The access belongs to the generation the state is tagged with (after
    /// entering it, if it was newer).
    Current,
    /// A producer of an older, quarantined generation. It writes nothing,
    /// and the local-open fallback does not apply to it; the dispatcher
    /// discards its completion anyway.
    Stale,
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
    /// `xdmcp` mirrors Xorg's `UsingXdmcp`: `EnableLocalHost` is then a no-op
    /// (os/access.c:273-279), so a load may close local access but never open it.
    fn apply_load_outcome(&mut self, outcome: LoadOutcome, xdmcp: bool) {
        match outcome {
            LoadOutcome::Loaded(cookies) => {
                // Additive — os/auth.c never clears. Skip cookies already
                // held, so the forced reload on every reset does not grow
                // the list.
                for cookie in cookies {
                    if !self.cookies.contains(&cookie) {
                        self.cookies.push(cookie);
                    }
                }
                self.local_open = false; // DisableLocalAccess
                self.ever_loaded = true;
            }
            LoadOutcome::OpenedEmpty => {
                // EnableLocalAccess, even if previously loaded (os/auth.c:197 quirk).
                if !xdmcp {
                    self.local_open = true;
                }
            }
            LoadOutcome::OpenFailed => {
                // loadauth == -1: open only if never loaded; else unchanged.
                if !self.ever_loaded && !xdmcp {
                    self.local_open = true;
                }
            }
        }
    }

    /// Bring the state into `generation`. The first access from a newer
    /// generation applies Xorg's reset: `ResetHosts` closes local access
    /// and empties the enabled host list (os/access.c:956-962), and `ResetAuthorization` forces the next auth load
    /// (os/auth.c:225-233), whose outcome may open it again. With no auth
    /// file that load finds zero cookies (`LoadAuthorization` returns 0,
    /// os/auth.c:117-119), so the reset value is `!xdmcp`. An access from an
    /// older generation changes nothing.
    fn enter(&mut self, generation: Generation, file: Option<&Path>, xdmcp: bool) -> Access {
        if generation < self.generation {
            return Access::Stale;
        }
        if generation > self.generation {
            self.local_open = false;
            self.hosts = HostList::default();
            match file {
                Some(path) => AuthState::reload_if_needed(self, path, true, xdmcp),
                None => self.apply_load_outcome(LoadOutcome::OpenedEmpty, xdmcp),
            }
            self.generation = generation;
        }
        Access::Current
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

    /// The cookie verdict, then the parts of Xorg's `InvalidHost`
    /// (os/access.c:1529-1555) that need no lookup: access control disabled
    /// (`xhost +`) or local access open. Both only for Unix: the host list
    /// never admits a TCP client. The selfhost and SI rules run in
    /// [`AuthState::check`] with the lock released.
    fn verdict(
        &self,
        transport: AuthTransport,
        xdmcp: bool,
        setup_generation: Generation,
        name: &[u8],
        data: &[u8],
    ) -> AuthVerdict {
        let verdict = self.cookie_verdict(transport, xdmcp, setup_generation, name, data);
        if verdict != AuthVerdict::Allow
            && transport == AuthTransport::Unix
            && (!self.hosts.access_enabled || self.local_open)
        {
            return AuthVerdict::Allow;
        }
        verdict
    }

    /// [`Inner::verdict`] without the local-open fallback.
    fn cookie_verdict(
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
        // Cookie match; `verdict` adds the local-open fallback (mirrors
        // CheckAuthorization + host-ACL fallthrough,
        // os/connection.c:536-560).
        if name == MIT_MAGIC_COOKIE.as_bytes() && self.cookies.iter().any(|c| ct_eq(c, data)) {
            return AuthVerdict::Allow;
        }
        Self::reject(name)
    }
}

impl AuthState {
    /// Build from `LaunchOptions.auth_file`. `None` ⇒ permanently open.
    pub fn new(file: Option<PathBuf>) -> Arc<Self> {
        Self::new_with(file, false, false)
    }

    /// Build from `LaunchOptions.auth_file` and whether `LaunchOptions.xdmcp`
    /// is set. See [`AuthState::xdmcp`] for what the flag changes.
    pub fn new_with_xdmcp(file: Option<PathBuf>, xdmcp: bool) -> Arc<Self> {
        Self::new_with(file, xdmcp, false)
    }

    /// Build from `LaunchOptions.auth_file`, whether `LaunchOptions.xdmcp`
    /// is set, and whether a TCP listener is configured
    /// (`LaunchOptions.tcp_listen`, see [`AuthState::tcp_listen`]).
    pub fn new_with(file: Option<PathBuf>, xdmcp: bool, tcp_listen: bool) -> Arc<Self> {
        Arc::new(Self::build(file, xdmcp, tcp_listen, own_interface_addrs))
    }

    fn build(
        file: Option<PathBuf>,
        xdmcp: bool,
        tcp_listen: bool,
        own_addrs: fn() -> Vec<HostEntry>,
    ) -> Self {
        // Start open; the first successful load flips this to false (Xorg ShouldLoadAuth=TRUE → never-loaded+unopenable stays open).
        // XDMCP starts closed and stays closed: AccessUsingXdmcp (os/access.c:387-391).
        let local_open = !xdmcp;
        Self {
            file,
            xdmcp,
            tcp_listen,
            own_addrs,
            inner: Mutex::new(Inner {
                last_mtime: None,
                ever_loaded: false,
                local_open,
                hosts: HostList::default(),
                generation: Generation::default(),
                cookies: Vec::new(),
                session: None,
            }),
        }
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
        Self::reload_if_needed(&mut inner, path, true, self.xdmcp);
        if inner.cookies.is_empty() {
            return Err("-listen tcp requires -auth with a MIT-MAGIC-COOKIE-1 cookie".into());
        }
        Ok(())
    }

    fn reload_if_needed(inner: &mut Inner, path: &Path, force: bool, xdmcp: bool) {
        let stat_mtime = fs::metadata(path).ok().and_then(|m| m.modified().ok());
        let changed = should_reload(stat_mtime, &mut inner.last_mtime);
        if force || changed {
            inner.apply_load_outcome(load_outcome(path), xdmcp);
        }
    }

    /// Authorize one client. Reloads lazily on file change, then decides.
    ///
    /// `setup_generation` is the generation the *calling setup thread* was
    /// bound to at accept (`BoundSender::generation`), never the counter's
    /// current value: reading the counter here would re-admit a client of the
    /// destroyed session the instant a reset installed a new credential.
    ///
    /// The cookie decides first; the host list is consulted only when it
    /// fails (os/connection.c:533-560), so the list only adds admissions.
    /// It is consulted for Unix peers only, never for TCP.
    pub fn check(
        &self,
        peer: &PeerIdentity,
        setup_generation: Generation,
        proto_name: &[u8],
        proto_data: &[u8],
    ) -> AuthVerdict {
        self.check_with(
            peer,
            setup_generation,
            proto_name,
            proto_data,
            &SystemResolver,
        )
    }

    fn check_with(
        &self,
        peer: &PeerIdentity,
        setup_generation: Generation,
        proto_name: &[u8],
        proto_data: &[u8],
        resolver: &dyn NameResolver,
    ) -> AuthVerdict {
        let (verdict, hosts) = {
            // Poisoning recovery — same pattern as setup_thread.rs:66.
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            // A newer generation first gets Xorg's reset state; a stale
            // producer changes nothing, and neither the local-open fallback
            // nor the host list applies to it.
            let access = inner.enter(setup_generation, self.file.as_deref(), self.xdmcp);
            if access == Access::Stale {
                return inner.cookie_verdict(
                    peer.transport,
                    self.xdmcp,
                    setup_generation,
                    proto_name,
                    proto_data,
                );
            }
            // With no `-auth` file there is nothing to reload: `local_open`
            // then changes only through `enter` and `FamilyLocalHost`. The
            // session credential still applies.
            if let Some(path) = self.file.as_deref() {
                Self::reload_if_needed(&mut inner, path, false, self.xdmcp);
            }
            let verdict = inner.verdict(
                peer.transport,
                self.xdmcp,
                setup_generation,
                proto_name,
                proto_data,
            );
            if verdict == AuthVerdict::Allow || peer.transport != AuthTransport::Unix {
                return verdict;
            }
            (verdict, inner.hosts.hosts.clone())
        };
        // The lock is released: the interface query and the NSS lookups run
        // on this snapshot of the list.
        if self.list_admits_unix(&hosts, peer.cred.as_ref(), resolver) {
            return AuthVerdict::Allow;
        }
        verdict
    }

    /// The rest of `InvalidHost` for a Unix peer (os/access.c:1540-1566):
    /// one of our `selfhosts` is listed, or an SI entry matches the peer's
    /// credentials. Non-SI entries never match a Unix peer (`addr` is NULL).
    fn list_admits_unix(
        &self,
        hosts: &[HostEntry],
        cred: Option<&PeerCred>,
        resolver: &dyn NameResolver,
    ) -> bool {
        if hosts.is_empty() {
            return false;
        }
        let selfhosts = self.selfhosts();
        hosts.iter().any(|host| selfhosts.contains(host))
            || hosts
                .iter()
                .any(|host| si_matches_local(host, cred, resolver))
    }

    /// Xorg's `selfhosts`: every interface address plus `{FamilyLocalHost,
    /// len 0}` (`DefineSelf`, os/access.c:793-818, :874-886), and nothing
    /// without a TCP listener (os/connection.c:287-288).
    fn selfhosts(&self) -> Vec<HostEntry> {
        if !self.tcp_listen {
            return Vec::new();
        }
        let mut selfhosts = (self.own_addrs)();
        selfhosts.push(HostEntry {
            family: FAMILY_LOCAL_HOST,
            addr: Vec::new(),
        });
        selfhosts
    }

    /// `ChangeHosts` after the mode and access checks: Xorg's `AddHost`
    /// (`insert`) or `RemoveHost` (os/access.c:1313-1446). `FamilyLocalHost`
    /// takes any length and opens or closes local access, then is stored
    /// like any entry. Every other family is validated first; deleting an
    /// absent entry is fine. Names resolve through `resolver` on the calling
    /// thread, as Xorg does on its only one.
    pub fn change_hosts(
        &self,
        generation: Generation,
        insert: bool,
        family: u8,
        addr: &[u8],
        resolver: &dyn NameResolver,
    ) -> Result<(), HostChangeError> {
        if family != FAMILY_LOCAL_HOST {
            check_addr(family, addr, resolver).map_err(HostChangeError::Value)?;
        }
        let entry = HostEntry {
            family,
            addr: addr.to_vec(),
        };
        let inert = if insert {
            self.inert_reason(&entry)
        } else {
            None
        };
        {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            // Cannot happen for a live client; fails closed.
            if inner.enter(generation, self.file.as_deref(), self.xdmcp) == Access::Stale {
                return Err(HostChangeError::Access);
            }
            if family == FAMILY_LOCAL_HOST {
                inner.local_open = insert;
            }
            if insert {
                inner.hosts.insert(entry);
            } else {
                inner.hosts.delete(&entry);
            }
        }
        if let Some(reason) = inert {
            log::warn!("xhost: added a host entry (family {family}) that admits nobody: {reason}");
        }
        Ok(())
    }

    /// Why an inserted entry can never admit anyone here, if it cannot. The
    /// list never admits TCP, so an address entry only counts as one of our
    /// own `selfhosts`, and an SI `hostname`/`ipv6` entry only ever matches
    /// Internet peers (os/access.c:1819-1846, :1970-1994).
    fn inert_reason(&self, entry: &HostEntry) -> Option<&'static str> {
        match entry.family {
            FAMILY_INTERNET | FAMILY_INTERNET6 if !self.tcp_listen => Some(
                "no TCP listener, so it is not one of our addresses, and the list never \
                 admits a TCP client",
            ),
            FAMILY_INTERNET | FAMILY_INTERNET6 if !(self.own_addrs)().contains(entry) => {
                Some("not one of our addresses, and the list never admits a TCP client")
            }
            FAMILY_SERVER_INTERPRETED => SiEntry::parse(&entry.addr)
                .filter(|si| matches!(si.kind, SiType::Hostname | SiType::Ipv6))
                .map(|_| "SI hostname/ipv6 entries only match TCP clients, which the list never admits"),
            _ => None,
        }
    }

    /// `SetAccessControl` after the mode and access checks: Xorg's
    /// `ChangeAccessControl` (os/access.c:1616-1623). Connected clients are
    /// not touched.
    pub fn set_access_enabled(
        &self,
        generation: Generation,
        enabled: bool,
    ) -> Result<(), HostChangeError> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.enter(generation, self.file.as_deref(), self.xdmcp) == Access::Stale {
            return Err(HostChangeError::Access);
        }
        inner.hosts.access_enabled = enabled;
        Ok(())
    }

    /// `ListHosts`' `enabled` flag and entries, newest first. An older
    /// generation reads the reset state.
    pub fn list_hosts(&self, generation: Generation) -> (bool, Vec<HostEntry>) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match inner.enter(generation, self.file.as_deref(), self.xdmcp) {
            Access::Current => (inner.hosts.access_enabled, inner.hosts.hosts.clone()),
            Access::Stale => {
                let reset = HostList::default();
                (reset.access_enabled, reset.hosts)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const UNIX: PeerIdentity = PeerIdentity {
        transport: AuthTransport::Unix,
        addr: None,
        cred: None,
    };
    const TCP: PeerIdentity = PeerIdentity {
        transport: AuthTransport::Tcp,
        addr: None,
        cred: None,
    };

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
            hosts: HostList::default(),
            generation: Generation::default(),
            cookies,
            session: None,
        }
    }
    fn fresh_open() -> Inner {
        Inner {
            last_mtime: None,
            ever_loaded: false,
            local_open: true,
            hosts: HostList::default(),
            generation: Generation::default(),
            cookies: Vec::new(),
            session: None,
        }
    }

    #[test]
    fn loaded_enforces_and_appends() {
        let mut i = fresh_open();
        i.apply_load_outcome(LoadOutcome::Loaded(vec![vec![1u8; 16]]), false);
        assert!(!i.local_open);
        assert!(i.ever_loaded);
        assert_eq!(i.cookies, vec![vec![1u8; 16]]);
        // A second load APPENDS (additive — Xorg never clears on reload).
        i.apply_load_outcome(LoadOutcome::Loaded(vec![vec![2u8; 16]]), false);
        assert_eq!(i.cookies, vec![vec![1u8; 16], vec![2u8; 16]]);
    }

    #[test]
    fn empty_file_reopens_even_after_load() {
        let mut i = enforcing_with(vec![vec![1u8; 16]]);
        i.apply_load_outcome(LoadOutcome::OpenedEmpty, false);
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
        i.apply_load_outcome(LoadOutcome::OpenFailed, false);
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
            hosts: HostList::default(),
            generation: Generation::default(),
            cookies: Vec::new(),
            session: None,
        };
        i.apply_load_outcome(LoadOutcome::OpenFailed, false);
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
                &UNIX,
                Generation::default(),
                MIT_MAGIC_COOKIE.as_bytes(),
                &cookie
            ),
            AuthVerdict::Allow
        );
        assert_eq!(
            auth.check(
                &UNIX,
                Generation::default(),
                MIT_MAGIC_COOKIE.as_bytes(),
                &[0u8; 16]
            ),
            AuthVerdict::Reject(REASON_BAD_COOKIE)
        );
        assert_eq!(
            auth.check(&UNIX, Generation::default(), b"", b""),
            AuthVerdict::Reject(REASON_NO_PROTO)
        );

        // File vanishes after a successful load → stays enforcing; the
        // already-loaded cookie still validates (additive list).
        fs::remove_file(&path).unwrap();
        assert_eq!(
            auth.check(
                &UNIX,
                Generation::default(),
                MIT_MAGIC_COOKIE.as_bytes(),
                &cookie
            ),
            AuthVerdict::Allow
        );
        assert_eq!(
            auth.check(
                &UNIX,
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
            auth.check(&UNIX, Generation::default(), b"", b""),
            AuthVerdict::Allow
        );
        assert_eq!(
            auth.check(&TCP, Generation::default(), b"", b""),
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
            auth.check(&UNIX, Generation::default(), b"", b""),
            AuthVerdict::Allow
        );
        assert_eq!(
            auth.check(&TCP, Generation::default(), b"", b""),
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
            auth.check(&UNIX, Generation::default(), b"", b""),
            AuthVerdict::Allow
        );
        assert_eq!(
            auth.check(
                &TCP,
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
                &TCP,
                Generation::default(),
                MIT_MAGIC_COOKIE.as_bytes(),
                &cookie
            ),
            AuthVerdict::Allow
        );
        assert_eq!(
            auth.check(
                &TCP,
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
                &TCP,
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
            none.check(&UNIX, Generation::default(), b"", b""),
            AuthVerdict::Allow
        );
        assert_eq!(
            none.check(&TCP, Generation::default(), b"", b""),
            AuthVerdict::Reject(REASON_NO_PROTO)
        );

        // 2. -auth names an unreadable (absent) file: never loaded, so
        //    Unix stays open and TCP still cannot be authorized.
        let missing = temp_path("fence-missing");
        let _ = fs::remove_file(&missing);
        let unreadable = AuthState::new(Some(missing));
        assert_eq!(
            unreadable.check(&UNIX, Generation::default(), b"", b""),
            AuthVerdict::Allow
        );
        assert_eq!(
            unreadable.check(
                &TCP,
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
        for transport in [&UNIX, &TCP] {
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
            auth.check(&TCP, old_client.generation(), MIT, &first),
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
            auth.check(&TCP, new_client.generation(), MIT, &first),
            AuthVerdict::Reject(REASON_BAD_COOKIE)
        );

        // ...and refused by MISMATCH, not because something cleared the slot:
        // the identical cookie still satisfies a producer bound to the old
        // generation, so the credential is demonstrably still installed. (Such
        // a producer is quarantined elsewhere, by
        // `generation::should_dispatch`; all this asserts is which comparison
        // produced the refusal above.)
        assert_eq!(
            auth.check(&TCP, old_client.generation(), MIT, &first),
            AuthVerdict::Allow,
            "the refusal must come from the generation comparison, not from an \
             empty slot — otherwise this test passes for the wrong reason"
        );

        // The new session is authorized only once its own `Accept` lands, and
        // that install retires the old credential for everyone.
        let second = [0xC2u8; 16];
        assert!(auth.install_session_cookie(new_generation, MIT, &second));
        assert_eq!(
            auth.check(&TCP, new_client.generation(), MIT, &second),
            AuthVerdict::Allow
        );
        assert_eq!(
            auth.check(&TCP, new_client.generation(), MIT, &first),
            AuthVerdict::Reject(REASON_BAD_COOKIE)
        );
        assert_eq!(
            auth.check(&TCP, old_client.generation(), MIT, &first),
            AuthVerdict::Reject(REASON_BAD_COOKIE),
            "one slot: the previous session's cookie is gone once replaced"
        );
        assert_eq!(
            auth.check(&TCP, old_client.generation(), MIT, &second),
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
            auth.check(&TCP, generation, MIT, &refused),
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
            auth.check(&TCP, generation, MIT, &refused),
            AuthVerdict::Reject(REASON_BAD_COOKIE),
            "the refused offer's cookie must stop authorizing immediately, not \
             merely once a replacement arrives"
        );

        // The replacement offer, in the same generation.
        assert!(auth.install_session_cookie(generation, MIT, &accepted));
        assert_eq!(
            auth.check(&TCP, generation, MIT, &accepted),
            AuthVerdict::Allow
        );
        assert_eq!(
            auth.check(&TCP, generation, MIT, &refused),
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
            auth.check(&TCP, generation, MIT, &[0x11u8; 16]),
            AuthVerdict::Reject(REASON_BAD_COOKIE)
        );
        assert_eq!(
            auth.check(&TCP, generation, b"", b""),
            AuthVerdict::Reject(REASON_NO_PROTO)
        );
        assert_eq!(
            auth.check(&TCP, generation, MIT, b""),
            AuthVerdict::Reject(REASON_BAD_COOKIE),
            "an empty presentation must not match an empty slot"
        );
        // Nor a cookie-less Unix client: XDMCP never opens local access.
        assert_eq!(
            auth.check(&UNIX, generation, b"", b""),
            AuthVerdict::Reject(REASON_NO_PROTO)
        );
    }

    /// Bump `path`'s mtime so the next `check` reloads it.
    /// Push the file's mtime past every earlier `touch_later`: a rewrite in
    /// the same clock tick gets the same mtime, so "mtime + 10 s" alone is
    /// not strictly newer on coarse-clock systems (OpenBSD).
    fn touch_later(path: &Path) {
        static STEP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let step = STEP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let later =
            fs::metadata(path).unwrap().modified().unwrap() + Duration::from_secs(10 * step);
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(later)
            .unwrap();
    }

    #[test]
    fn xdmcp_load_closes_local_access_but_never_opens_it() {
        let mut i = fresh_open();
        i.local_open = false;
        i.apply_load_outcome(LoadOutcome::OpenedEmpty, true);
        assert!(!i.local_open, "EnableLocalHost is a no-op under XDMCP");
        i.apply_load_outcome(LoadOutcome::OpenFailed, true);
        assert!(!i.local_open, "EnableLocalHost is a no-op under XDMCP");
        i.local_open = true;
        i.apply_load_outcome(LoadOutcome::Loaded(vec![vec![1u8; 16]]), true);
        assert!(!i.local_open, "DisableLocalHost still applies");
    }

    #[test]
    fn xdmcp_without_auth_file_refuses_cookieless_unix() {
        let auth = AuthState::new_with_xdmcp(None, true);
        let generation = Generation::default();
        assert_eq!(
            auth.check(&UNIX, generation, b"", b""),
            AuthVerdict::Reject(REASON_NO_PROTO)
        );
        assert_eq!(
            auth.check(&UNIX, generation, MIT, &[0u8; 16]),
            AuthVerdict::Reject(REASON_BAD_COOKIE)
        );
    }

    #[test]
    fn xdmcp_empty_auth_file_refuses_cookieless_unix_across_reloads() {
        let path = temp_path("xdmcp-empty");
        fs::write(&path, b"").unwrap();
        let generation = Generation::default();
        let auth = AuthState::new_with_xdmcp(Some(path.clone()), true);
        assert_eq!(
            auth.check(&UNIX, generation, b"", b""),
            AuthVerdict::Reject(REASON_NO_PROTO),
            "empty file at startup"
        );

        // A later reload of the still-empty file.
        touch_later(&path);
        assert_eq!(
            auth.check(&UNIX, generation, b"", b""),
            AuthVerdict::Reject(REASON_NO_PROTO),
            "reload of the still-empty file"
        );

        // A loaded cookie, then the file emptied: Xorg's reopen quirk must
        // not apply under XDMCP; the loaded cookie keeps working.
        let cookie = [0x3Cu8; 16];
        fs::write(&path, xauth_bytes(b"7", &cookie)).unwrap();
        touch_later(&path);
        assert_eq!(
            auth.check(&UNIX, generation, MIT, &cookie),
            AuthVerdict::Allow
        );
        fs::write(&path, b"").unwrap();
        touch_later(&path);
        assert_eq!(
            auth.check(&UNIX, generation, b"", b""),
            AuthVerdict::Reject(REASON_NO_PROTO),
            "emptied after a load"
        );
        assert_eq!(
            auth.check(&UNIX, generation, MIT, &cookie),
            AuthVerdict::Allow
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn xdmcp_unreadable_auth_file_refuses_cookieless_unix() {
        let generation = Generation::default();

        // stat succeeds, read fails: the never-loaded OpenFailed path.
        let dir = temp_path("xdmcp-unreadable-dir");
        let _ = fs::remove_dir(&dir);
        fs::create_dir(&dir).unwrap();
        let auth = AuthState::new_with_xdmcp(Some(dir.clone()), true);
        assert_eq!(
            auth.check(&UNIX, generation, b"", b""),
            AuthVerdict::Reject(REASON_NO_PROTO)
        );
        let _ = fs::remove_dir(dir);

        // Missing entirely.
        let missing = temp_path("xdmcp-missing");
        let _ = fs::remove_file(&missing);
        let auth = AuthState::new_with_xdmcp(Some(missing), true);
        assert_eq!(
            auth.check(&UNIX, generation, b"", b""),
            AuthVerdict::Reject(REASON_NO_PROTO)
        );
    }

    #[test]
    fn xdmcp_session_cookie_admits_unix() {
        let auth = AuthState::new_with_xdmcp(None, true);
        let generation = Generation::default();
        let session = [0x4Du8; 16];
        assert!(auth.install_session_cookie(generation, MIT, &session));
        assert_eq!(
            auth.check(&UNIX, generation, MIT, &session),
            AuthVerdict::Allow
        );
        assert_eq!(
            auth.check(&UNIX, generation, b"", b""),
            AuthVerdict::Reject(REASON_NO_PROTO)
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
            auth.check(&UNIX, generation, MIT, &file_cookie),
            AuthVerdict::Allow,
            "Unix clients are unaffected by XDMCP mode"
        );
        assert_eq!(
            auth.check(&TCP, generation, MIT, &file_cookie),
            AuthVerdict::Reject(REASON_BAD_COOKIE),
            "a cookie in a local file must not authorize a client into an \
             XDMCP session it has nothing to do with"
        );

        // The session credential is the only thing that admits TCP here.
        let session_cookie = [0x6Bu8; 16];
        assert!(auth.install_session_cookie(generation, MIT, &session_cookie));
        assert_eq!(
            auth.check(&TCP, generation, MIT, &session_cookie),
            AuthVerdict::Allow
        );
        assert_eq!(
            auth.check(&TCP, generation, MIT, &file_cookie),
            AuthVerdict::Reject(REASON_BAD_COOKIE),
            "installing a session cookie does not un-shadow the file ones"
        );

        // Same file, no XDMCP: the gate is the mode, not anything about the
        // file or the cookie.
        let plain = AuthState::new(Some(path.clone()));
        assert_eq!(
            plain.check(&TCP, generation, MIT, &file_cookie),
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
            auth.check(&TCP, generation, MIT, b""),
            AuthVerdict::Reject(REASON_BAD_COOKIE)
        );

        // An authorization name this layer cannot check is refused too.
        assert!(!auth.install_session_cookie(generation, b"XDM-AUTHORIZATION-1", &[1u8; 8]));
        assert_eq!(
            auth.check(&TCP, generation, b"XDM-AUTHORIZATION-1", &[1u8; 8]),
            AuthVerdict::Reject(REASON_BAD_PROTO)
        );

        // And a refused install fails closed: it drops whatever was in the
        // slot rather than leaving the previous offer's cookie live.
        let good = [0x77u8; 16];
        assert!(auth.install_session_cookie(generation, MIT, &good));
        assert!(!auth.install_session_cookie(generation, MIT, b""));
        assert_eq!(
            auth.check(&TCP, generation, MIT, &good),
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

    // ---- the reset: generation-tagged `local_open` ----

    use crate::core_loop::generation::GenerationCounter;

    /// Stand-in for `xhost ±local:` until `ChangeHosts` exists.
    fn set_local_open(auth: &AuthState, open: bool) {
        auth.inner.lock().unwrap().local_open = open;
    }

    fn cookieless_unix(auth: &AuthState, generation: Generation) -> AuthVerdict {
        auth.check(&UNIX, generation, b"", b"")
    }

    /// Rewrite `path` without moving its mtime, so only a forced load can
    /// see the new contents.
    fn rewrite_keeping_mtime(path: &Path, bytes: &[u8]) {
        let mtime = fs::metadata(path).unwrap().modified().unwrap();
        fs::write(path, bytes).unwrap();
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
    }

    #[test]
    fn a_reset_without_auth_file_reopens_local_access() {
        let counter = GenerationCounter::new();
        let auth = AuthState::new(None);
        let g0 = counter.current();
        set_local_open(&auth, false);
        assert_eq!(
            cookieless_unix(&auth, g0),
            AuthVerdict::Reject(REASON_NO_PROTO)
        );
        // ResetHosts closes it, and the forced load of no file reopens it.
        assert_eq!(cookieless_unix(&auth, counter.bump()), AuthVerdict::Allow);
    }

    #[test]
    fn a_reset_under_xdmcp_closes_local_access() {
        let counter = GenerationCounter::new();
        let auth = AuthState::new_with_xdmcp(None, true);
        let g0 = counter.current();
        set_local_open(&auth, true);
        assert_eq!(cookieless_unix(&auth, g0), AuthVerdict::Allow);
        assert_eq!(
            cookieless_unix(&auth, counter.bump()),
            AuthVerdict::Reject(REASON_NO_PROTO)
        );
    }

    #[test]
    fn a_reset_forces_the_auth_reload_even_with_an_unchanged_mtime() {
        let counter = GenerationCounter::new();
        let path = temp_path("reset-forced-reload");
        let (first, second) = ([0x31u8; 16], [0x32u8; 16]);
        fs::write(&path, xauth_bytes(b"0", &first)).unwrap();
        let auth = AuthState::new(Some(path.clone()));
        let g0 = counter.current();
        assert_eq!(auth.check(&UNIX, g0, MIT, &first), AuthVerdict::Allow);
        rewrite_keeping_mtime(&path, &xauth_bytes(b"0", &second));
        assert_eq!(
            auth.check(&UNIX, g0, MIT, &second),
            AuthVerdict::Reject(REASON_BAD_COOKIE),
            "same mtime: no lazy reload within G0"
        );

        // An open local access from G0 (`xhost +local:`) does not survive.
        set_local_open(&auth, true);
        let g1 = counter.bump();
        assert_eq!(
            auth.check(&UNIX, g1, MIT, &second),
            AuthVerdict::Allow,
            "the reset forced the reload"
        );
        assert_eq!(
            cookieless_unix(&auth, g1),
            AuthVerdict::Reject(REASON_NO_PROTO),
            "a loaded cookie leaves local access closed"
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn a_reset_with_an_emptied_auth_file_opens_local_access() {
        let counter = GenerationCounter::new();
        let path = temp_path("reset-emptied");
        fs::write(&path, xauth_bytes(b"0", &[0x33u8; 16])).unwrap();
        let auth = AuthState::new(Some(path.clone()));
        let g0 = counter.current();
        assert_eq!(
            cookieless_unix(&auth, g0),
            AuthVerdict::Reject(REASON_NO_PROTO)
        );
        rewrite_keeping_mtime(&path, b"");
        assert_eq!(
            cookieless_unix(&auth, g0),
            AuthVerdict::Reject(REASON_NO_PROTO)
        );
        assert_eq!(cookieless_unix(&auth, counter.bump()), AuthVerdict::Allow);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn a_reset_with_a_deleted_auth_file_keeps_the_reset_value() {
        // `loaded` stays TRUE and `loadauth == -1` changes nothing, so the
        // value is ResetHosts' closed one.
        let counter = GenerationCounter::new();
        let path = temp_path("reset-deleted");
        fs::write(&path, xauth_bytes(b"0", &[0x34u8; 16])).unwrap();
        let auth = AuthState::new(Some(path.clone()));
        let g0 = counter.current();
        assert_eq!(
            cookieless_unix(&auth, g0),
            AuthVerdict::Reject(REASON_NO_PROTO)
        );
        set_local_open(&auth, true);
        fs::remove_file(&path).unwrap();
        assert_eq!(
            cookieless_unix(&auth, counter.bump()),
            AuthVerdict::Reject(REASON_NO_PROTO)
        );
    }

    #[test]
    fn an_older_generation_writes_nothing_and_gets_no_local_fallback() {
        let counter = GenerationCounter::new();
        let auth = AuthState::new(None);
        let g1 = counter.bump();
        let g2 = counter.bump();
        assert_eq!(cookieless_unix(&auth, g1), AuthVerdict::Allow);
        assert_eq!(cookieless_unix(&auth, g2), AuthVerdict::Allow);
        assert_eq!(
            cookieless_unix(&auth, g1),
            AuthVerdict::Reject(REASON_NO_PROTO)
        );
        let inner = auth.inner.lock().unwrap();
        assert_eq!(inner.generation, g2);
        assert!(inner.local_open);
    }

    #[test]
    fn reset_reloads_do_not_duplicate_cookies() {
        let counter = GenerationCounter::new();
        let path = temp_path("reset-dedup");
        let cookie = [0x35u8; 16];
        fs::write(&path, xauth_bytes(b"0", &cookie)).unwrap();
        let auth = AuthState::new(Some(path.clone()));
        let mut generation = counter.current();
        for _ in 0..4 {
            assert_eq!(
                auth.check(&UNIX, generation, MIT, &cookie),
                AuthVerdict::Allow
            );
            generation = counter.bump();
        }
        assert_eq!(auth.inner.lock().unwrap().cookies.len(), 1);
        let _ = fs::remove_file(path);
    }

    // ---- the host list: admission through `InvalidHost` ----

    use crate::core_loop::host_access::{FAMILY_DECNET, HostEntry};
    use std::{
        ffi::CStr,
        net::{IpAddr, Ipv4Addr, Ipv6Addr},
        sync::mpsc,
        time::Duration as StdDuration,
    };

    const BOB: PeerCred = PeerCred {
        uid: 1000,
        gid: 100,
        pid: None,
    };
    const HOST_COOKIE: [u8; 16] = [0x51; 16];
    const WRONG_COOKIE: [u8; 16] = [0x52; 16];

    /// `bob` is uid 1000, group `staff` gid 100; nothing else resolves.
    struct Names;

    impl NameResolver for Names {
        fn uid(&self, name: &CStr) -> Option<u32> {
            (name.to_bytes() == b"bob").then_some(1000)
        }
        fn gid(&self, name: &CStr) -> Option<u32> {
            (name.to_bytes() == b"staff").then_some(100)
        }
    }

    fn unix_peer(cred: Option<PeerCred>) -> PeerIdentity {
        PeerIdentity {
            transport: AuthTransport::Unix,
            addr: None,
            cred,
        }
    }

    fn tcp_peer() -> PeerIdentity {
        PeerIdentity {
            transport: AuthTransport::Tcp,
            addr: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            cred: None,
        }
    }

    fn v6_own() -> [u8; 16] {
        Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1).octets()
    }

    fn fake_own_addrs() -> Vec<HostEntry> {
        vec![
            HostEntry {
                family: FAMILY_INTERNET,
                addr: vec![127, 0, 0, 1],
            },
            HostEntry {
                family: FAMILY_INTERNET6,
                addr: v6_own().to_vec(),
            },
        ]
    }

    fn si(kind: &str, value: &str) -> Vec<u8> {
        let mut addr = kind.as_bytes().to_vec();
        addr.push(0);
        addr.extend_from_slice(value.as_bytes());
        addr
    }

    /// `-auth` with one cookie, already loaded, so `local_open` is false.
    fn enforcing_state(tag: &str, tcp_listen: bool) -> (AuthState, PathBuf) {
        let path = temp_path(tag);
        fs::write(&path, xauth_bytes(b"0", &HOST_COOKIE)).unwrap();
        let auth = AuthState::build(Some(path.clone()), false, tcp_listen, fake_own_addrs);
        assert_eq!(
            auth.check_with(
                &unix_peer(Some(BOB)),
                Generation::default(),
                MIT,
                &HOST_COOKIE,
                &Names
            ),
            AuthVerdict::Allow
        );
        assert!(!auth.inner.lock().unwrap().local_open);
        (auth, path)
    }

    fn add(auth: &AuthState, family: u8, addr: &[u8]) {
        auth.change_hosts(Generation::default(), true, family, addr, &Names)
            .unwrap();
    }

    fn cookieless(auth: &AuthState, peer: &PeerIdentity) -> AuthVerdict {
        auth.check_with(peer, Generation::default(), b"", b"", &Names)
    }

    #[derive(Clone, Copy, Debug)]
    enum Source {
        AccessDisabled,
        LocalOpen,
        LocalUserId(&'static str),
        LocalUserName(&'static str),
        LocalGroupId(&'static str),
        LocalGroupName(&'static str),
        SelfLocalHost,
    }

    fn apply(auth: &AuthState, source: Source) {
        match source {
            Source::AccessDisabled => auth
                .set_access_enabled(Generation::default(), false)
                .unwrap(),
            Source::LocalOpen => set_local_open(auth, true),
            Source::LocalUserId(v) | Source::LocalUserName(v) => {
                add(auth, FAMILY_SERVER_INTERPRETED, &si("localuser", v));
            }
            Source::LocalGroupId(v) | Source::LocalGroupName(v) => {
                add(auth, FAMILY_SERVER_INTERPRETED, &si("localgroup", v));
            }
            Source::SelfLocalHost => {
                add(auth, FAMILY_LOCAL_HOST, &[]);
                set_local_open(auth, false);
            }
        }
    }

    const ADMITTING: [Source; 7] = [
        Source::AccessDisabled,
        Source::LocalOpen,
        Source::LocalUserId("#1000"),
        Source::LocalUserName("bob"),
        Source::LocalGroupId("#100"),
        Source::LocalGroupName("staff"),
        Source::SelfLocalHost,
    ];

    #[test]
    fn each_host_source_alone_admits_a_cookieless_unix_peer() {
        for (n, source) in ADMITTING.into_iter().enumerate() {
            let (auth, path) = enforcing_state(&format!("h-admit-{n}"), true);
            let bob = unix_peer(Some(BOB));
            assert_eq!(
                cookieless(&auth, &bob),
                AuthVerdict::Reject(REASON_NO_PROTO),
                "{source:?}: refused before"
            );
            apply(&auth, source);
            assert_eq!(cookieless(&auth, &bob), AuthVerdict::Allow, "{source:?}");
            assert_eq!(
                auth.check_with(&bob, Generation::default(), MIT, &WRONG_COOKIE, &Names),
                AuthVerdict::Allow,
                "{source:?}: a wrong cookie falls through to the list too"
            );
            let _ = fs::remove_file(path);
        }
    }

    #[test]
    fn non_matching_host_entries_refuse() {
        let cases: [(Source, Option<PeerCred>); 5] = [
            (Source::LocalUserId("#1001"), Some(BOB)),
            (Source::LocalGroupId("#101"), Some(BOB)),
            // A supplementary group of the peer: credentials carry the
            // primary gid only, as on Xorg's SO_PEERCRED/getpeereid paths.
            (Source::LocalGroupId("#27"), Some(BOB)),
            (Source::LocalUserId("#1000"), None),
            (Source::LocalGroupId("#100"), None),
        ];
        for (n, (source, cred)) in cases.into_iter().enumerate() {
            let (auth, path) = enforcing_state(&format!("h-refuse-{n}"), true);
            apply(&auth, source);
            assert_eq!(
                cookieless(&auth, &unix_peer(cred)),
                AuthVerdict::Reject(REASON_NO_PROTO),
                "{source:?} against {cred:?}"
            );
            let _ = fs::remove_file(path);
        }
    }

    #[test]
    fn no_host_source_admits_tcp() {
        for (n, source) in ADMITTING.into_iter().enumerate() {
            let (auth, path) = enforcing_state(&format!("h-tcp-{n}"), true);
            apply(&auth, source);
            assert_eq!(
                auth.check_with(
                    &tcp_peer(),
                    Generation::default(),
                    MIT,
                    &WRONG_COOKIE,
                    &Names
                ),
                AuthVerdict::Reject(REASON_BAD_COOKIE),
                "{source:?}"
            );
            assert_eq!(
                auth.check_with(&tcp_peer(), Generation::default(), b"", b"", &Names),
                AuthVerdict::Reject(REASON_NO_PROTO),
                "{source:?}"
            );
            assert_eq!(
                auth.check_with(
                    &tcp_peer(),
                    Generation::default(),
                    MIT,
                    &HOST_COOKIE,
                    &Names
                ),
                AuthVerdict::Allow,
                "{source:?}: the file cookie still admits TCP"
            );
            let _ = fs::remove_file(path);
        }
    }

    #[test]
    fn xdmcp_tcp_needs_the_session_credential_whatever_the_list_says() {
        let auth = AuthState::build(None, true, true, fake_own_addrs);
        let generation = Generation::default();
        let session = [0x53u8; 16];
        assert!(auth.install_session_cookie(generation, MIT, &session));
        auth.set_access_enabled(generation, false).unwrap();
        add(&auth, FAMILY_LOCAL_HOST, &[]);
        assert_eq!(
            auth.check_with(&tcp_peer(), generation, MIT, &session, &Names),
            AuthVerdict::Allow
        );
        assert_eq!(
            auth.check_with(&tcp_peer(), generation, MIT, &WRONG_COOKIE, &Names),
            AuthVerdict::Reject(REASON_BAD_COOKIE)
        );
    }

    #[test]
    fn a_valid_cookie_admits_with_an_enabled_empty_list() {
        let (auth, path) = enforcing_state("h-cookie", false);
        assert_eq!(auth.list_hosts(Generation::default()), (true, Vec::new()));
        assert_eq!(
            auth.check_with(
                &unix_peer(None),
                Generation::default(),
                MIT,
                &HOST_COOKIE,
                &Names
            ),
            AuthVerdict::Allow
        );
        let _ = fs::remove_file(path);
    }

    /// `+local:` and then a cookie reload: `DisableLocalHost` closes local
    /// access, the entry stays listed (os/access.c:300-311, :1322-1325).
    fn local_host_then_reload(auth: &AuthState, path: &Path, addr: &[u8]) {
        add(auth, FAMILY_LOCAL_HOST, addr);
        assert!(auth.inner.lock().unwrap().local_open);
        fs::write(path, xauth_bytes(b"0", &[0x54u8; 16])).unwrap();
        touch_later(path);
        // Any connection triggers the lazy reload.
        assert_eq!(
            auth.check_with(
                &unix_peer(None),
                Generation::default(),
                MIT,
                &HOST_COOKIE,
                &Names
            ),
            AuthVerdict::Allow
        );
        assert!(!auth.inner.lock().unwrap().local_open);
    }

    #[test]
    fn a_cookie_reload_closes_local_host_but_keeps_the_entry() {
        let (auth, path) = enforcing_state("h-local-reload", false);
        local_host_then_reload(&auth, &path, &[]);
        assert_eq!(
            auth.list_hosts(Generation::default()),
            (
                true,
                vec![HostEntry {
                    family: FAMILY_LOCAL_HOST,
                    addr: Vec::new(),
                }]
            )
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn a_listed_selfhost_admits_unix_only_with_a_tcp_listener() {
        for tcp_listen in [true, false] {
            let expected = if tcp_listen {
                AuthVerdict::Allow
            } else {
                AuthVerdict::Reject(REASON_NO_PROTO)
            };
            let (auth, path) = enforcing_state(&format!("h-self-local-{tcp_listen}"), tcp_listen);
            local_host_then_reload(&auth, &path, &[]);
            assert_eq!(
                cookieless(&auth, &unix_peer(None)),
                expected,
                "{{252, []}}, tcp_listen {tcp_listen}"
            );
            let _ = fs::remove_file(path);

            let (auth, path) = enforcing_state(&format!("h-self-v4-{tcp_listen}"), tcp_listen);
            add(&auth, FAMILY_INTERNET, &[127, 0, 0, 1]);
            assert_eq!(
                cookieless(&auth, &unix_peer(None)),
                expected,
                "127.0.0.1, tcp_listen {tcp_listen}"
            );
            let _ = fs::remove_file(path);
        }
    }

    #[test]
    fn a_nonzero_length_local_host_entry_is_not_a_selfhost() {
        let (auth, path) = enforcing_state("h-self-local-len", true);
        local_host_then_reload(&auth, &path, &[1]);
        assert_eq!(
            cookieless(&auth, &unix_peer(None)),
            AuthVerdict::Reject(REASON_NO_PROTO)
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn ipv6_selfhosts_come_from_the_own_address_list() {
        let other = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 2).octets();
        for tcp_listen in [true, false] {
            let (auth, path) = enforcing_state(&format!("h-self-v6-{tcp_listen}"), tcp_listen);
            add(&auth, FAMILY_INTERNET6, &other);
            assert_eq!(
                cookieless(&auth, &unix_peer(None)),
                AuthVerdict::Reject(REASON_NO_PROTO),
                "2001:db8::2 is not ours"
            );
            add(&auth, FAMILY_INTERNET6, &v6_own());
            let expected = if tcp_listen {
                AuthVerdict::Allow
            } else {
                AuthVerdict::Reject(REASON_NO_PROTO)
            };
            assert_eq!(
                cookieless(&auth, &unix_peer(None)),
                expected,
                "2001:db8::1, tcp_listen {tcp_listen}"
            );
            let _ = fs::remove_file(path);
        }
    }

    #[test]
    fn the_host_list_does_not_survive_a_reset() {
        let counter = GenerationCounter::new();
        let (auth, path) = enforcing_state("h-reset", false);
        let g0 = counter.current();
        add(&auth, FAMILY_SERVER_INTERPRETED, &si("localuser", "#1000"));
        auth.set_access_enabled(g0, false).unwrap();
        let bob = unix_peer(Some(BOB));
        assert_eq!(cookieless(&auth, &bob), AuthVerdict::Allow);
        let g1 = counter.bump();
        assert_eq!(
            auth.check_with(&bob, g1, b"", b"", &Names),
            AuthVerdict::Reject(REASON_NO_PROTO)
        );
        assert_eq!(auth.list_hosts(g1), (true, Vec::new()));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn a_stale_generation_cannot_write_the_list() {
        let counter = GenerationCounter::new();
        let auth = AuthState::new(None);
        let g0 = counter.current();
        let g1 = counter.bump();
        assert_eq!(auth.list_hosts(g1), (true, Vec::new()));
        assert_eq!(
            auth.change_hosts(g0, true, FAMILY_LOCAL_HOST, &[], &Names),
            Err(HostChangeError::Access)
        );
        assert_eq!(
            auth.set_access_enabled(g0, false),
            Err(HostChangeError::Access)
        );
        assert_eq!(auth.list_hosts(g1), (true, Vec::new()));
    }

    #[test]
    fn change_hosts_validates_before_storing() {
        let auth = AuthState::new(None);
        let g = Generation::default();
        assert_eq!(
            auth.change_hosts(g, true, FAMILY_DECNET, &[1, 2], &Names),
            Err(HostChangeError::Value(BadValue::Length))
        );
        assert_eq!(
            auth.change_hosts(g, true, 7, &[], &Names),
            Err(HostChangeError::Value(BadValue::Family))
        );
        assert_eq!(
            auth.change_hosts(
                g,
                true,
                FAMILY_SERVER_INTERPRETED,
                &si("localuser", "nobody_here"),
                &Names
            ),
            Err(HostChangeError::Value(BadValue::Length))
        );
        assert_eq!(auth.list_hosts(g), (true, Vec::new()));
    }

    /// Blocks inside `uid` until released, after announcing it got there.
    struct BlockingNames {
        entered: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
    }

    impl NameResolver for BlockingNames {
        fn uid(&self, name: &CStr) -> Option<u32> {
            self.entered.send(()).unwrap();
            self.release.lock().unwrap().recv().unwrap();
            Names.uid(name)
        }
        fn gid(&self, name: &CStr) -> Option<u32> {
            Names.gid(name)
        }
    }

    #[test]
    fn name_lookups_at_check_run_with_the_lock_released() {
        let (auth, path) = enforcing_state("h-unlocked", false);
        let auth = Arc::new(auth);
        add(&auth, FAMILY_SERVER_INTERPRETED, &si("localuser", "bob"));
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let checker = {
            let auth = Arc::clone(&auth);
            std::thread::spawn(move || {
                let resolver = BlockingNames {
                    entered: entered_tx,
                    release: Mutex::new(release_rx),
                };
                auth.check_with(
                    &unix_peer(Some(BOB)),
                    Generation::default(),
                    b"",
                    b"",
                    &resolver,
                )
            })
        };
        entered_rx
            .recv_timeout(StdDuration::from_secs(10))
            .expect("check reached the resolver");
        let (done_tx, done_rx) = mpsc::channel();
        let writer = {
            let auth = Arc::clone(&auth);
            std::thread::spawn(move || {
                add(&auth, FAMILY_SERVER_INTERPRETED, &si("localuser", "#1"));
                done_tx.send(()).unwrap();
            })
        };
        let completed = done_rx.recv_timeout(StdDuration::from_secs(10)).is_ok();
        release_tx.send(()).unwrap();
        assert!(completed, "change_hosts blocked behind a check in NSS");
        writer.join().unwrap();
        assert_eq!(checker.join().unwrap(), AuthVerdict::Allow);
        let _ = fs::remove_file(path);
    }
}
