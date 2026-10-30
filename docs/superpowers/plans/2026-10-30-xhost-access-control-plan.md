# Host access control (`xhost`) — plan

Date: 2026-10-30
Base: `master` `824cc763` (#319, the XDMCP local-access fix) plus the two
spec commits. Branch: `feat/xhost-access-control`.
Spec: `docs/superpowers/specs/2026-10-30-xhost-access-control-design.md`,
including "Decisions (review 2026-10-30)". Issue: #305.

## Goal

`xhost` works on yserver the way it does on Xorg for local clients:
`ListHosts`, `ChangeHosts` and `SetAccessControl` operate on real state,
byte-exact with Xorg, and a Unix client without a valid cookie is admitted
when the host list says so (`si:localuser`, `si:localgroup`, `local:`,
`xhost +`). TCP is never admitted by the host list. Each connection's peer
identity is captured once at setup and kept on the client.

Path prefixes used below:

- `C/` = `crates/yserver-core/src/core_loop/`
- `P/` = `crates/yserver-core/src/core_loop/process_request/`
- `S/` = `crates/yserver-core/src/`
- `Y/` = `crates/yserver/src/`
- `W/` = `crates/yserver-protocol/src/x11/`
- `X/` = `../xserver` @ `65d790bd2` (xserver 21.1.24)

Line numbers are on `3e59642d` (the branch tip; it only adds docs to
`824cc763`).

## Oracles

A test in this plan asserts an expected value only where its source is
named. Two kinds:

- **[Xorg]**: derived from the cited `X/` source lines, or from the measured
  `ListHosts` reply recorded in the spec's Verification section.
- **[spec]**: a yserver decision with no Xorg counterpart (TCP never admitted
  by the list, generation binding, peer identity cached at setup, `-ac`
  ignored, resolver on the setup thread with the lock released).

Each test below carries one of these tags. Where a step touches existing
behaviour, the oracle is the existing test that pins it, and it must stay
green unchanged unless the step says otherwise.

## What exists, what is new

Already there:

- **Admission.** `AuthState` (`C/auth.rs:57`) holds `file`, `xdmcp` and
  `Mutex<Inner>`. `Inner` (`C/auth.rs:81-94`) holds `last_mtime`,
  `ever_loaded`, `local_open`, the additive `cookies` and the one XDMCP
  `session` slot. `apply_load_outcome(.., xdmcp)` (`C/auth.rs:136-157`) is
  Xorg's `CheckAuthorization` load outcome (`X/os/auth.c:196-208`) with the
  #319 XDMCP guard. `Inner::verdict` (`C/auth.rs:220-246`) is: session
  credential, then XDMCP-TCP refusal, then file cookies, then
  `transport == Unix && local_open`. `AuthState::check` (`C/auth.rs:358-379`)
  reloads lazily and calls `verdict`; its comment at `:367-369` says
  `local_open` "is never mutated" without `-auth`, which this work makes
  false. `new_with_xdmcp` (`C/auth.rs:258-274`) starts `local_open = !xdmcp`.
- **Setup.** `setup_thread::spawn` (`C/setup_thread.rs:70`) runs
  `run_setup` (`:144`). It derives `AuthTransport` from the stream
  (`:162-167`), calls `auth.check` with the thread's bound generation
  (`:176-192`), and sends `Message::ClientSetupComplete` (`:268-282`,
  variant at `C/message.rs:211`) carrying `is_local`, `fd_passing`,
  `internal`.
- **Accept.** `accept_pending` (`C/run.rs:4837`) spawns setup threads; TCP
  `is_local` is `address_is_ours(peer.ip())` (`C/run.rs:4849-4867`,
  function at `:4750`, IPv4 interface addresses plus any loopback). The
  server's own client is spawned at `C/run.rs:2493`.
  `handle_client_setup_complete` (`C/run.rs:4611`) builds `ClientState`.
- **Client record.** `ClientState` (`S/server.rs:3094`) has `is_local`,
  `fd_passing`, `internal`, and no peer identity. It is built by struct
  literal in about 60 places (production: `C/run.rs:4629`; the rest are
  tests, e.g. `P/tests/mod.rs:108-145`).
- **Peer pid.** `client_peer_pid` (`P/misc.rs:20-55`) calls
  `getsockopt(SO_PEERCRED)` per call on Linux and returns `None` elsewhere.
  Callers: X-Resource `QueryClientIds` (`P/misc.rs:221-226`) and the Present
  scanout candidate (`P/present_ext.rs:948`). Tests that pin it:
  `P/tests/core_misc.rs:364`, `:398` (cfg-split on Linux) and
  `P/tests/present_pixmap.rs:5290-5310`.
- **Requests.** `110 ListHosts` dispatches at `P/process_request.rs:364` to
  `handle_list_hosts` (`P/misc.rs:335-348`), which writes
  `W/mod.rs:3225` `write_list_hosts_reply`: `enabled = 0`, zero hosts.
  `109 ChangeHosts` and `111 SetAccessControl` have no arm and fall to the
  default `BadRequest` arm (`P/process_request.rs:566-581`). Length checks
  already exist and run before dispatch: `W/request_lengths.rs:134-136`
  (`109 AtLeast(2)`, `110`/`111 Fixed(1)`) and the exact ChangeHosts length
  at `:450-454`; byte swapping of the ChangeHosts length field at
  `W/request_swap.rs:813`. Error codes `BAD_ACCESS`/`BAD_VALUE` exist at
  `W/mod.rs:4885-4902`.
- **Reset.** `reset_generation` (`C/reset.rs:318`) bumps the counter
  (`:340`) and replaces `*state` with a fresh one (`:426-456`), carrying
  `start_instant` over (`:444`). It never touches `AuthState`.
  `Generation` (`C/generation.rs:27`) derives `PartialEq, Eq` only.
- **Startup.** `build_auth_state` (`Y/lib.rs:115`) passes the auth file and
  `opts.xdmcp.is_some()`. TCP is bound in `bind_client_listeners`
  (`Y/lib.rs:231`) on `0.0.0.0` only (`Y/lib.rs:258-268`). The nested
  server uses `AuthState::new(None)` (`S/nested.rs:421`).
- **Platforms.** libc 0.2.191 has `getpeereid` for every BSD and macOS
  (`libc/src/unix/bsd/mod.rs:627`), `LOCAL_PEERPID` on macOS
  (`bsd/apple/mod.rs:3177`) and `SO_PEERCRED` + `sockpeercred` on OpenBSD
  (`bsd/netbsdlike/openbsd/mod.rs:368`, `:979`). Existing per-OS code uses
  plain `#[cfg(target_os = ...)]` pairs (`S/unix_fd.rs:21-23`,
  `S/anon_shm.rs:16-24`). CI runs `cargo test` on Linux, FreeBSD, OpenBSD
  and macOS (`.github/workflows/ci.yml:73`, `:190`, `:223`, `:262`).

New:

1. `PeerIdentity`, read once in the setup thread and stored on `ClientState`.
2. A host-access model (`C/host_access.rs`): entries, SI validation and
   matching, the list reply. Pure and unit-tested.
3. `AuthState` owns the list, `access_enabled` and a generation-tagged
   `local_open`. `check` takes the peer and evaluates Xorg's `InvalidHost`
   after a failed cookie, for Unix peers only.
4. The core thread reaches `AuthState` through a handle on `ServerState`.
5. Real handlers for 109, 110 and 111.
6. A vng scenario run on yserver and on Xorg.

## Xorg facts the spec does not state

These came out of reading `X/` for this plan. Steps below follow them.

- **`local:` is stored in the list.** `AddHost` accepts `FamilyLocalHost`
  with any length, sets `LocalHostEnabled` and then falls through to
  `NewHost(family, pAddr, len, FALSE)` like every other family
  (`X/os/access.c:1322-1325`, `:1347-1349`). So `xhost +local:` appears in
  `ListHosts` as a family-252 entry, is deduplicated like any entry, and
  `RemoveHost` unlinks it (`:1410-1413`, `:1439-1444`). A cookie reload
  closes `LocalHostEnabled` (`DisableLocalHost`, `:300-311`) but leaves the
  entry listed.
- **`selfhosts` contains `{FamilyLocalHost, len 0}` whenever a TCP
  listener exists.** `DefineSelf` appends it after the interface addresses
  (`:874-886`). `InvalidHost` admits a local peer when any `selfhosts` entry
  equals a `validhosts` entry (`:1540-1552`). So with a TCP listener, a
  zero-length `+local:` entry keeps local access open after a cookie reload
  has cleared `LocalHostEnabled`. Without a TCP listener `selfhosts` is
  empty (`X/os/connection.c:287-288`) and the reload closes it, as the
  spec's table says.
- **`selfhosts` is every `getifaddrs` address, IPv6 included** (`:793-818`),
  independent of which families the listener uses.
- **`EnableLocalHost` also calls `AddLocalHosts`** (`:273-279`,
  `:919-929`), copying `selfhosts` into `validhosts`, and
  `DisableLocalHost` removes the copies that were not explicitly requested
  (`:300-311`, the `requested` flag set in `NewHost`, `:1379-1386`). Not
  mirrored here (decision 2).
- **Reset clears cookies.** `ResetAuthorization` calls each protocol's
  `Reset` (`X/os/auth.c:225-233`); `MitResetCookie` frees every MIT cookie
  (`X/os/mitauth.c:87-97`). Our `cookies` are process-lifetime and additive.
  This is a pre-existing divergence (decision 3); Step 3 only makes
  the forced reload not duplicate entries.
- **No auth file loads as zero cookies.** `LoadAuthorization` returns 0
  when `authorization_file` is NULL (`X/os/auth.c:117-119`), so the forced
  load after a reset calls `EnableLocalAccess`: `!xdmcp` is the reset value
  without `-auth`, as the spec says.
- **`#<num>` is parsed with `strtol` into an `int`**
  (`X/os/access.c:2051-2072`): base 0, leading whitespace and sign
  accepted, trailing junk accepted if at least one digit parsed, `ERANGE`
  rejected, and the `long` result truncated to `int` (on LP64,
  `#4294967296` resolves to id 0). Names are copied and NUL-terminated
  (`:2059-2060`), so a value with an embedded NUL resolves the part before
  it. This plan keeps Xorg's validation but not the truncation (decision 1).
- **The `hostname` validator accepts `:`**: the digit range is written
  `0x30..=0x3A` (`:1939`). Length must be `1..NI_MAXHOST` (`:1913`).
- **`ipv6` validation** is `3 <= len < INET6_ADDRSTRLEN` plus `inet_pton`
  on the NUL-terminated copy (`:2013-2035`).
- **`BadAccess` carries a stale `errorValue`**: `AuthorizedClient` does not
  set it (`:1293-1307`) and `Dispatch` does not reset it per request. We
  send 0.
- **`GetHosts` cap** (`:1460-1467`): entries are counted until the running
  byte total reaches 1 MiB, and the entry that reaches or crosses the limit
  is still included.

## Decisions

- **D1 Module split.** The list model, SI parsing and matching, and the
  reply entry encoding live in a new `C/host_access.rs`, with no locking
  and no I/O except through a `NameResolver` trait. `C/auth.rs` stays the
  one place that holds state and decides admission. That keeps `auth.rs`
  (1212 lines) well under the layout limit and makes the Xorg rules
  testable without NSS.
- **D2 Name resolution behind a trait.** `trait NameResolver { fn uid(&self,
  name: &CStr) -> Option<u32>; fn gid(&self, name: &CStr) -> Option<u32>; }`.
  `SystemResolver` uses `getpwnam_r`/`getgrnam_r` (spec: setup threads run
  concurrently). Unit tests use a fixed map, so "unknown user" is
  deterministic on every CI host.
- **D3 Peer identity type.** As in the spec: `PeerIdentity { transport:
  AuthTransport, addr: Option<IpAddr>, cred: Option<PeerCred> }`,
  `PeerCred { uid, gid, pid: Option<u32> }`. It lives in `S/peer.rs` next
  to `S/unix_fd.rs`, with a cfg pair per OS family:
  - Linux: `SO_PEERCRED` (pid included);
  - OpenBSD: `SO_PEERCRED` with `sockpeercred` (pid included);
  - macOS: `getpeereid`, pid from `LOCAL_PEERPID`;
  - FreeBSD: `getpeereid`, pid `None`.

  No `LOCAL_PEERCRED`/`xucred` (spec). A failed lookup gives `cred: None`.
- **D4 Generation is ordered.** Add `PartialOrd, Ord` to `Generation`
  (`C/generation.rs:27`). The spec says a reader that finds a stale tag
  takes the reset value. But the reset value comes from a *forced* auth
  reload (`ResetAuthorization` sets `ShouldLoadAuth`), and a reload
  mutates `cookies` and `last_mtime`. A reader that recomputed it without
  storing it would force a reload on every connection. So the first access
  from a **newer** generation enters it: it sets `local_open` to the Xorg
  reset value (`false`, from `ResetHosts`, `X/os/access.c:958`), runs the
  forced reload (which then applies `apply_load_outcome` as today), resets
  the list to `access_enabled = true`, empty, and tags both with the new
  generation. An access from an **older** generation is a quarantined
  producer whose completion the dispatcher will discard anyway. It never
  writes and gets `H = false`. The ordering exposes no number, so the
  module's "numeric identity is not exposed" rule holds.
- **D5 Core handle on `ServerState`.** `ServerState` gains
  `host_access: HostAccessHandle { auth: Arc<AuthState>, generation:
  Generation }`. `ServerState::new()` builds a private `AuthState::new(None)`
  at `Generation::default()`, so unit tests and the nested server need no
  setup. `run_core_with_inventory` installs the real one before the loop,
  and `reset_generation` step 6 carries it into `fresh` with the new
  generation, next to `start_instant` (`C/reset.rs:444`). Handlers pass
  `handle.generation` to every `AuthState` call.
- **D6 TCP listener flag.** `AuthState` learns at construction whether a
  TCP listener is configured (`opts.tcp_listen`). `selfhosts` is empty
  without one (`X/os/connection.c:287-288`), so the selfhost rule is off.
  Constructor: `new_with(file, xdmcp, tcp_listen)`; `new(file)` and
  `new_with_xdmcp` keep their meaning with `tcp_listen = false`.
- **D7 Selfhosts.** With a TCP listener, `selfhosts` is our `getifaddrs`
  addresses, both families, plus `{252, []}`, queried at check time like
  `address_is_ours` (`C/run.rs:4740-4745` explains why per call).
  `address_is_ours` moves next to it in `S/peer.rs` and keeps its own
  behaviour (any loopback counts as local). The spec restricted this rule to
  IPv4; this plan follows Xorg's interface list (decision 4).
- **D8 Mode and family checks in the handler**, not in
  `W/request_lengths.rs` `invalid_value`. The order is length (already
  generic), then mode, then `may_manage_access`, then family and address.
  That is Xorg's order (`X/dix/dispatch.c:3253-3267`,
  `X/os/access.c:1313-1349`), and keeping it in one function keeps it
  readable.

## Steps

Each step is one commit and leaves the tree green. Gate for every step:

```sh
cargo +nightly fmt --check
env -u RUSTC_WRAPPER cargo clippy --all-targets -- -D warnings
env -u RUSTC_WRAPPER cargo clippy --all-targets --features xdmcp -- -D warnings
env -u RUSTC_WRAPPER cargo test -p yserver-core
sh tools/check-layout.sh
```

Steps that touch `crates/yserver` (3, 4, 6) also run
`env -u RUSTC_WRAPPER cargo test -p yserver`. The full workspace test runs
before the PR. The BSD and macOS arms are verified by CI, not locally.

### Step 1 — `Generation: Ord` and the host-access model

Changes:

- `C/generation.rs:27`: derive `PartialOrd, Ord`. Doc comment: ordered so
  that process-lifetime state can tell a newer session from a quarantined
  older one; still no numeric access.
- New `C/host_access.rs` (`pub mod host_access` in `C/mod.rs`), pure:
  - constants: `FAMILY_INTERNET = 0`, `FAMILY_DECNET = 1`,
    `FAMILY_CHAOS = 2`, `FAMILY_SERVER_INTERPRETED = 5`,
    `FAMILY_INTERNET6 = 6`, `FAMILY_LOCAL_HOST = 252`;
  - `HostEntry { family: u8, addr: Vec<u8> }`;
  - `HostList { generation, access_enabled, hosts: Vec<HostEntry> }` with
    `insert` (no-op if an equal entry exists, else prepend) and `delete`
    (unlink the first equal entry, absent is fine). Equality is family,
    length and bytes (`addrEqual`, `X/os/access.c:193-196`);
  - `check_addr(family, addr, &dyn NameResolver) -> Result<(), BadValue>`,
    returning which value Xorg reports: `Length` for families 0, 1, 2, 5,
    6 that fail `CheckAddr`, `Family` for anything else. 252 is handled by
    the caller and never reaches it;
  - `SiEntry::parse(addr)`: type up to the first NUL, value after it, no
    trailing NUL. Types `localuser`, `localgroup`, `hostname`, `ipv6`, each
    with its validator as listed under "Xorg facts";
  - `parse_cred_id(value, kind, resolver) -> Option<i64>`: `#` + `strtol`
    base 0 into the full `long` (no truncation, decision 1), else a name
    lookup of the bytes before the first NUL. Validation (`check_addr`)
    only asks for `Some`, as Xorg does, so ChangeHosts accepts the same
    values;
  - `si_matches_local(entry, cred, resolver) -> bool`: `localuser`
    compares `i64::from(uid)` with the parsed id, `localgroup` the same
    with `gid`, so an id outside `0..=u32::MAX` is stored but never
    matches; the others are false for a local peer (`:1819-1846`,
    `:1970-1994`);
  - `encode_entries(&[HostEntry]) -> (n_hosts, bytes)` applying the 1 MiB
    rule, and the entry layout `family, 0, length:CARD16, addr, zero pad`.
    Byte order is a parameter.
  - `NameResolver` and `SystemResolver` (D2).

Tests (unit, in `host_access.rs`):

- [Xorg `:1371-1396`] insert dedups and prepends: insert A, B, A → list
  `[B, A]`; delete of an absent entry is `Ok` and changes nothing.
- [Xorg `:1495-1524`, `:1337-1344`] `check_addr`: family 0 length 4 ok,
  length 3 → `Length`; family 6 length 16 ok, 15 → `Length`; families 1 and
  2 any length → `Length`; family 7 → `Family`; family 5 with an unknown
  type, no NUL, or an unresolvable user → `Length`.
- [Xorg `:2051-2093`] `parse_cred_id` with a fake resolver: `#1000`,
  `#0x3e8`, `#01750` → 1000; `#1000x` → 1000; `# 1000` and `#+1000` →
  1000; `#` and `#x` → `None`; `#99999999999999999999` (ERANGE) → `None`;
  `#4294967296` → `Some(4294967296)` and `#-1` → `Some(-1)` (no
  truncation, decision 1); a known name → its id, an unknown one → `None`;
  `bob\0junk` → bob's id.
- [Xorg `:2051-2093` for validation, decision 1 for matching] oversized and
  negative ids: `check_addr` accepts `localuser:#4294967296`,
  `localuser:#-1`, `localgroup:#4294967296` and `localgroup:#-1` (Xorg
  accepts them too), and `si_matches_local` is false for all four against uid/gid 0 and against
  uid/gid `u32::MAX`.
- [Xorg `:1901-1966`] hostname validator: `a.b-c` ok, `a:b` ok, `.a`,
  `-a`, `a..b`, `a_b`, empty and 1025 bytes rejected, 1024 ok.
- [Xorg `:2013-2035`] ipv6 validator: `::1` ok, `::` rejected (length 2),
  `::1\0zz` ok, `fe80::1%lo` rejected, 46 bytes rejected.
- [Xorg `:2096-2140`, spec D3] `si_matches_local`: localuser equal uid →
  true, other uid → false; localgroup equal primary gid → true, a
  supplementary group never (cred carries none); hostname and ipv6 → false;
  `cred: None` → false (`:2104`).
- [Xorg, measured in the spec] `encode_entries` of one entry `{5,
  b"localuser\0jos"}` LE → `05 00 0d 00 6c 6f 63 61 6c 75 73 65 72 00 6a 6f
  73 00 00 00` (20 bytes, reply length 5); BE gives `0d` in byte 3 and `00`
  in byte 2 (`X/dix/swaprep.c:648-661`). Pad bytes are zero (spec).
- [Xorg `:1460-1467`] the 1 MiB cap: 16 entries of 65532 bytes (65536 each
  on the wire, 1 MiB in total) → all 16 kept; 17 → 16 (the 16th reaches
  1 MiB and is kept, the oldest is dropped).
- [spec] `Generation` ordering: `bump()` returns a value greater than the
  previous `current()`.

### Step 2 — peer identity, captured but unused

Changes:

- New `S/peer.rs` (`pub mod peer` in `S/lib.rs`): `PeerIdentity`,
  `PeerCred` (D3), `PeerIdentity::of(&Transport) -> PeerIdentity`. TCP
  sets `addr` from `peer_addr()` and `cred: None`; Unix sets `cred` from
  the per-OS call. `Transport::Capture` (tests only) gives
  `PeerIdentity::unknown_unix()`.
- Move `address_is_ours` from `C/run.rs:4750` into `S/peer.rs` as
  `pub(crate)`, and add `own_interface_addrs() -> Vec<HostEntry>` (both
  families, as `{0, 4 bytes}`/`{6, 16 bytes}`) for Step 5, built on a pure
  `host_entry_of(&SocketAddr)` helper. `run.rs` calls the moved function;
  behaviour unchanged.

Tests (unit, in `peer.rs`, run on all four CI OSes):

- [Xorg `:793-818`, decision 4] `host_entry_of` maps a V4 address to
  `{0, 4 bytes}` and a V6 address to `{6, 16 bytes}` in network order.

- [spec] A `UnixListener` bound in a temp dir, one `UnixStream::connect`,
  `accept`: `PeerIdentity::of` the accepted side has `cred.uid ==
  geteuid()` and `cred.gid == getegid()`. This uses the production path,
  listener + accept, rather than `UnixStream::pair` as the spec suggests,
  because `socketpair` credentials are not portable to every BSD. `pid`:
  `Some(std::process::id())` on Linux, OpenBSD and macOS, `None` on
  FreeBSD. That split is the platform API, not a measurement; if CI
  disagrees on a platform, that platform's arm is wrong.
- [spec] A loopback TCP pair: `addr == Some(127.0.0.1)`, `cred == None`.
- [existing] `address_is_ours` keeps its current callers' tests green.

### Step 3 — store the peer on the client

Changes:

- `run_setup` (`C/setup_thread.rs:144`) computes
  `PeerIdentity::of(&stream)` once, before `read_setup_request`, and puts
  it in `Message::ClientSetupComplete` (`C/message.rs:211`) as `peer`.
  `auth.check` is unchanged in this step.
- `ClientState` (`S/server.rs:3094`) gains `pub peer: PeerIdentity`,
  documented as immutable for the connection's life.
  `handle_client_setup_complete` (`C/run.rs:4611`) stores it. All other
  struct literals get `peer: PeerIdentity::unknown_unix()`. This part is
  mechanical. `install_client_with_transport` (`P/tests/mod.rs:108`) instead
  sets `peer` with `cred: Some(PeerCred { uid: geteuid, gid: getegid, pid:
  Some(std::process::id()) })`, so request tests do not depend on per-OS
  socket APIs.
- `client_peer_pid` (`P/misc.rs:20-55`) becomes one function for all OSes:
  `client.peer.cred.and_then(|c| c.pid)`. The `is_local` guard goes,
  because a TCP peer has `cred: None`. Update the X-Resource doc comment
  (`P/misc.rs:56-62`), which says peer credentials are not retained.

Tests:

- [spec] `full_handshake_round_trip` (`C/setup_thread.rs:432`) asserts the
  `ClientSetupComplete` it receives carries `peer.transport == Unix` and
  `cred.uid == geteuid()`.
- [existing, adjusted] `x_resource_query_client_ids_reports_xid_and_pid_identities`
  (`P/tests/core_misc.rs:398`) and the Present `client_pid` assertions
  (`P/tests/present_pixmap.rs:5290-5310`) lose their Linux/non-Linux cfg
  split. They now assert the fixture's pid on every OS: they test the
  cache, and Step 2 tests the OS call.
  `x_resource_query_client_ids_omits_pid_for_nonlocal_client` (`:364`) sets
  `peer.cred = None` instead of `is_local = false`.

Behaviour change: on macOS and OpenBSD, X-Resource and the Present
candidate now report a pid. Xorg does the same there (`LOCAL_PEERPID`,
`X/os/access.c:1265-1272`; `SO_PEERCRED` on OpenBSD).

### Step 4 — generation-tagged `local_open` and the forced reload

Admission is unchanged within one generation. This step adds the reset
semantics before any writer exists.

Changes (`C/auth.rs`):

- `Inner` gains `generation: Generation` (shared by `local_open` and,
  from Step 5, the list; one tag, since both reset together in
  `ResetHosts`) and starts at `Generation::default()`.
- `Inner::enter(gen, file, xdmcp) -> Access` (D4): `gen == tag` →
  `Current`; `gen > tag` → `local_open = false`, forced reload if `file`
  is set, else `apply_load_outcome(OpenedEmpty, xdmcp)` (the
  `LoadAuthorization` → 0 path, `X/os/auth.c:117-119`), tag = gen,
  `Current`; `gen < tag` → `Stale`, nothing written.
- `apply_load_outcome` `Loaded`: append only cookies not already present,
  so the forced reload on each reset does not grow `cookies` (open
  question 3).
- `check` calls `enter` first. A `Stale` access skips the `local_open`
  fallback. The session credential and file cookies still decide as today,
  since the dispatcher discards that completion anyway.
- Rewrite the comment at `C/auth.rs:367-369` and the `local_open` field doc
  (`:86`): `local_open` is Xorg's `LocalHostEnabled`, with three writers
  (load outcome, `FamilyLocalHost`, reset) and a generation tag.
- `new_with(file, xdmcp, tcp_listen)` (D6). `Y/lib.rs:115` passes
  `opts.tcp_listen`.

Tests (unit, `C/auth.rs`):

- [Xorg `X/os/access.c:958`, `X/os/auth.c:117-119`, `:196-208`; spec] No
  `-auth`, not XDMCP: force `local_open = false` in G0 (test hook standing
  in for Step 6's `-local:`), then `check` in G1 → admitted (reset value
  `true`).
- [Xorg `:275`, `:387-391`; spec] Same under XDMCP → refused in G1.
- [Xorg `X/os/auth.c:200-208`] `-auth` with one cookie: G1 entry reloads
  even with an unchanged mtime and leaves `local_open = false`; with an
  emptied file → `true`; with the file deleted after an earlier load →
  `false`. That is `ResetHosts`' value, because `loaded` stays TRUE and
  `loadauth == -1` changes nothing.
- [spec D4] After G2 is entered, a `check` bound to G1 does not change the
  tag or `local_open`, and its cookie-less Unix setup is refused.
- [spec] The cookie count stays the same across three generation entries
  with an unchanged file.
- [existing] Every test in `C/auth.rs:395-1212` stays green unchanged.
  The XDMCP ones (`:1004-1110`) pin #319.

### Step 5 — host list in `AuthState`, admission through `H`

Changes:

- `Inner` gains `hosts: HostList` (Step 1 type), tagged by the same
  generation and reset by `enter`.
- `check(peer: &PeerIdentity, gen, name, data)`. The transport comes from
  `peer.transport`; `run_setup` passes the peer from Step 3. Order, per
  `X/os/connection.c:533-560` and the spec's table:
  1. under the lock: `enter`, then the cookie verdict exactly as today
     (session credential, XDMCP-TCP refusal, file cookies);
  2. if that rejected and `peer.transport == Unix` and `Access::Current`,
     evaluate `H` (Xorg `InvalidHost`, `X/os/access.c:1529-1570`):
     `!access_enabled`; `local_open`; with `tcp_listen`, any
     own-address entry (from the provider, production `own_interface_addrs()`) or `{252, []}` equal to a list entry
     (D7). If none of these holds, snapshot the SI entries and **release
     the lock**;
  3. outside the lock, `si_matches_local` over the snapshot with
     `SystemResolver` and `peer.cred`;
  4. still rejected → the cookie layer's reason, unchanged.

  TCP never reaches step 2 (spec invariant 1).
- Write API, used by Step 7, each taking `gen` and calling `enter` first
  (a `Stale` write is refused with `BadAccess`, which cannot happen for a
  live client but fails closed):
  `change_hosts(gen, insert: bool, family, addr, &dyn NameResolver) ->
  Result<(), HostChangeError>`, `set_access_enabled(gen, bool)`,
  `list_hosts(gen) -> (bool, Vec<HostEntry>)`. `change_hosts` for 252 sets
  `local_open` (insert → true, delete → false), then inserts or deletes the
  entry like any other family (`X/os/access.c:1322-1325`, `:1347-1349`,
  `:1410-1413`). For other families it runs `check_addr` and then
  insert/delete. A successful insert of an entry that cannot admit anyone
  logs one `warn!` naming why (spec): 0 or 6 not among our own addresses
  (or no TCP listener), and SI `hostname`/`ipv6`.

Tests (unit, `C/auth.rs`, fake resolver for SI, `PeerIdentity` built by
hand):

- [spec "Admission decision table" + Xorg `InvalidHost` order] the H table,
  one test per row. Unix without a cookie, `-auth` loaded (so `local_open`
  false), each source alone admits: `access_enabled = false`;
  `local_open`; `si:localuser` = peer uid; `si:localgroup` = peer gid. And
  each one set to the non-matching value refuses: another uid, another gid,
  `si:localgroup` for a gid that is only supplementary, `cred: None`. The
  same five admitting states with `transport == Tcp` and a wrong cookie →
  refused (spec invariant 1). TCP with a valid file cookie → admitted.
  XDMCP TCP with and without a session credential: admitted, refused.
- [Xorg `:1540-1552`, `:874-886`] With `tcp_listen`: a `{252, []}` entry
  admits a cookie-less Unix peer after `local_open` was cleared by a reload,
  and so does a `{0, 127.0.0.1}` entry. Without `tcp_listen`, both → refused.
  A `{252, [1]}` entry (nonzero length) does not count as a selfhost.
- [Xorg `:793-818`, decision 4] IPv6 selfhosts. The own-address list comes
  through a provider (production: `own_interface_addrs`; tests: a fake), so
  the test does not depend on the CI host's interfaces. With `tcp_listen`
  and a fake list holding `{6, 2001:db8::1}`: an `inet6:2001:db8::1` entry
  admits a cookie-less Unix peer after a reload cleared `local_open`;
  `inet6:2001:db8::2` refuses; without `tcp_listen` both refuse.
- [Xorg `:1322-1325`, `:300-311`] `+local:` then a cookie reload (newer
  mtime, one cookie): `local_open` false, the 252 entry still listed.
- [Xorg `:1533-1535`, spec invariant 2] A valid cookie is admitted with
  `access_enabled = true` and an empty list (the list only adds).
- [spec invariant 5] `+si:localuser:#<uid>` in G0, `check` in G1 → refused
  (`-auth` loaded), and `list_hosts(G1)` → `(true, [])`.
- [spec invariant 3] A default state (G0, nothing written) gives the same
  verdict as `master` for every existing `check` test: those tests are
  ported to the new signature with no change in expectation.
- [spec, "lock released"] A resolver that blocks until released: a
  `change_hosts` from another thread completes while a `check` is blocked
  inside the resolver.

### Step 6 — core handle and reset carry-over

Changes:

- `ServerState` (`S/server.rs:1068`) gains `host_access: HostAccessHandle`
  (D5). `ServerState::new()` and `into_server_state`
  (`S/backend/mod.rs:143`) seed the private default.
- `run_core_with_inventory` (`C/run.rs:2112`) sets
  `state.host_access = HostAccessHandle { auth: auth.clone(), generation:
  rx.current_generation() }` before the loop.
- `reset_generation` (`C/reset.rs:426-456`) sets
  `fresh.host_access = HostAccessHandle { auth:
  state.host_access.auth.clone(), generation }` beside `start_instant`.

Tests:

- [spec] Existing reset test pattern (`C/reset.rs:1050` and following):
  after `reset_generation`, `state.host_access.generation` equals the
  returned generation and `Arc::ptr_eq` with the old handle's `auth`.
- [spec] A run-loop test (`C/run/tests.rs`) that drives `run_core` with a
  known `AuthState` asserts the installed handle is that `AuthState`.

### Step 7 — `ListHosts`, `ChangeHosts`, `SetAccessControl`

Changes:

- `W/mod.rs:3225` `write_list_hosts_reply(writer, byte_order, sequence,
  enabled, n_hosts, entry_bytes)`: byte 1 `enabled`, `length = bytes / 4`,
  `nHosts` at offset 8, then the entries from `encode_entries`.
- `P/misc.rs`:
  - `may_manage_access(client) -> bool { client.is_local }` and
    `may_read_access(_) -> bool { true }`, the two SECURITY hook points
    (spec);
  - `handle_list_hosts`: `may_read_access`, then
    `auth.list_hosts(gen)`;
  - new `handle_change_hosts(state, client_id, sequence, header, body)`:
    mode `> 1` → `BadValue(mode)`; `!may_manage_access` → `BadAccess(0)`;
    family `body[0]`, length `body[2..4]` LE (the reader normalises byte
    order, `W/request_swap.rs:813`), address `body[4..4 + len]`;
    `change_hosts` errors map to `BadValue(length)` or `BadValue(family)`;
  - new `handle_set_access_control`: mode `> 1` → `BadValue(mode)`;
    `!may_manage_access` → `BadAccess(0)`; else `set_access_enabled`.
- `P/process_request.rs`: arms `109` and `111` next to `110` (`:364`).
- Resolution on the core thread uses `SystemResolver` (spec, "NSS on the
  core thread" risk). Request tests use the numeric `#<uid>` form so they
  never depend on NSS.

Tests (`P/tests/core_misc.rs`, real `process_request`, reply bytes read
from the peer socket):

- [Xorg, measured in the spec + `X/dix/dispatch.c:3270-3304`] Fresh state:
  `ListHosts` → `01 01 seq 00 00 00 00 00 00 00 …` (enabled, length 0,
  nHosts 0). This replaces `list_hosts_writes_reply` (`:1084`), which only
  checks the opcode.
- [Xorg `:1371-1396`, `X/dix/swaprep.c:648`] Insert `localuser\0#1000` (15
  bytes) then `localgroup\0#1000` (16 bytes): `ListHosts` → nHosts 2,
  newest first, length (4+16 + 4+16)/4 = 10, zero pad. The same list for a
  big-endian client: swapped `sequence`, `length`, `nHosts` and each entry
  length.
- [Xorg `:1322-1325`, `:1347-1349`] Insert family 252 length 0 → listed as
  `fc 00 00 00`. A second insert does not duplicate it. Delete removes it.
- [Xorg `X/dix/dispatch.c:3253-3267`, `X/os/access.c:1313-1349`, the
  spec's error table] Each case pinned to `(error, value)`:
  - SI with unknown type `foo\0bar` (7 bytes) → `BadValue 7`;
  - SI `localuser\0nosuchuser_305` → `BadValue` = its length (resolver
    misses on every CI host; this one uses `SystemResolver` on purpose);
  - family 1, length 4 → `BadValue 4`;
  - family 7 → `BadValue 7`;
  - family 6 length 4 → `BadValue 4`;
  - ChangeHosts mode 2 → `BadValue 2`, also from a non-local client (mode
    is checked before access);
  - SetAccessControl mode 2 → `BadValue 2`;
  - `ListHosts` with length 2 → `BadLength` (already generic; pinned here).
- [Xorg `:1293-1307`] Non-local client (`is_local = false`): 109 and 111 →
  `BadAccess`, and the list is unchanged. 110 → a reply.
- [Xorg `:1616-1623`, `:1451-1458`] `SetAccessControl 0` → `ListHosts`
  byte 1 = 0. `SetAccessControl 1` → 1.
- [Xorg `:1401-1446`] Delete of an absent valid entry → no error, no change.
- [spec invariant 6] Two clients connected. `ChangeHosts` delete and
  `SetAccessControl 1` leave both in `state.clients` and both able to
  `ListHosts`.

### Step 8 — end to end in vng, on yserver and Xorg

Changes:

- `tools/vng-scenarios/xhost.sh` (guest) plus its `suite.list` line. The
  scenario starts the server with `-auth` (a cookie for `ysuite` only).
  A root hook adds a second throwaway user, the same way `vng-shot.sh:199-207`
  adds `ysuite`, and runs clients as that user with `setpriv` (`:251`).
  Checks, in order:
  1. user 2 without a cookie: `xdpyinfo` fails;
  2. `xhost +si:localuser:<user2>` as `ysuite` → user 2 connects; keep one
     user-2 client open;
  3. `xhost` output equals the expected text;
  4. `xhost -si:localuser:<user2>` → a new user-2 connection fails, and the
     open one still answers `xprop -root`;
  5. `xhost +` → user 2 connects; `xhost -` → refused again;
  6. `xhost +local:`, then `xhost` lists `LOCAL:`; `xhost -local:`.

  Each check writes its line to the result. `RESULT` is `pass` only if all
  match.
- The same scenario with `--server xorg` is the oracle for steps 1-6 and
  for the `xhost` text. Both runs go in the scenario's comment header as the
  two commands, not as numbers.
- TCP after `xhost +` (spec): only when the vng build has `tcp-transport`.
  If it does not, the case is covered by the Step 5 unit rows and the
  scenario says `skip` for it.

Gate: `tools/vng-suite.sh xhost` green on yserver; the Xorg run of the same
script green.

### Step 9 — docs

- `docs/status.md`: entry for #305 (what landed, the deviations in
  "Xorg facts" and D4/D7, decisions 1 and 2 as deliberate differences from
  Xorg, and decision 3: cookies still survive a reset, fixed separately).
- Sentences that become false, for jos to rewrite (prose is his):
  `docs/setup.md:337-340` ("There is no host-based access control") and
  `docs/man/yserver.1.scd:292` (`xhost(1)` "is not implemented yet"). The
  spec asks for one line in `docs/setup.md` on `xhost +host` admitting
  nothing over TCP.

## Decisions (review 2026-10-30)

1. **`#<num>` ids are not truncated.** Xorg stores `strtol` into an `int`,
   so `xhost +si:localuser:#4294967296` matches uid 0 on LP64. We keep
   Xorg's validation (ChangeHosts accepts the same values) but match on the
   full value, so oversized and negative ids are stored, listed, and never
   match anyone.
2. **`AddLocalHosts` is not mirrored.** Xorg copies `selfhosts` into the
   list when local access opens; for us that is reachable only with
   `-auth F -listen tcp` and `F` later emptied, and the only effect would be
   extra `ListHosts` entries.
3. **Cookies surviving a reset** (pre-existing; Xorg clears them,
   `X/os/mitauth.c:87-97`) is fixed on its own branch after #305. Step 4
   only deduplicates; the status entry records the difference.
4. **Selfhosts include IPv6 interface addresses** (D7), as Xorg does. Step 5
   uses them to decide whether an `inet6:` entry admits a Unix client, and
   tests that with a fake own-address list.
