# Host access control (`xhost`) — design

**Date:** 2026-10-30
**Issue:** #305
**Status:** Draft, for review. Supersedes
`2026-09-09-host-access-control-design.md` (never implemented). It keeps that
spec's TCP decision and reverses its non-goal on `si:` matching, which #305
needs.

## Problem

`xhost +SI:localuser:bob`, `xhost +` and `xhost -` fail on yserver, and plain
`xhost` prints "access control disabled" even under `-auth`.

- `110 ListHosts` is a stub (`process_request.rs:364` → `misc.rs:335` →
  `x11::write_list_hosts_reply`, `yserver-protocol/src/x11/mod.rs:3225`).
  It always replies `enabled = 0` with zero hosts.
- `109 ChangeHosts` and `111 SetAccessControl` have no handler.
- Admission (`core_loop/auth.rs` `Inner::verdict`) uses the XDMCP session
  credential, then the file cookies, then `local_open` for Unix. It has no
  host list and no peer identity.

## Goal

1. Implement Xorg's three requests over real state, matching Xorg's wire
   bytes and error codes.
2. Admit local (Unix-socket) clients from the host list, including
   `si:localuser` and `si:localgroup` matched against the peer's credentials.
   This is the #305 use case: let a second local user into a session that
   runs under `-auth`.
3. Capture each connection's peer identity once, at setup, in a form that
   SECURITY (the next layer) can consume. SECURITY is not designed here.

## Non-goals

- **The host list never admits a TCP client.** This is decision B from the
  2026-09-09 spec, kept as is. A TCP client needs a valid cookie whatever the
  list or `AccessEnabled` say. `xhost +` therefore opens the display to local
  users only.
- SECURITY / XACE trust levels. One hook point is reserved (see "Who may
  change the list").
- `/etc/X<n>.hosts` (`ResetHosts`, `os/access.c:977-1083`). It stays out, as
  decided in the 2026-09-09 spec.
- `-ac`. Open question 1.
- `FamilyNetname` / SECURE_RPC, Kerberos, and the `ssh` command-name locality
  heuristic (`ComputeLocalClient`, `os/access.c:1126`).
- Disconnecting clients when access is narrowed. Xorg does not do this (see
  below).

## Reference: Xorg

All line numbers are in `../xserver` @ `65d790bd2`.

### State (`os/access.c:232-237`)

| Variable | Default | Meaning |
|---|---|---|
| `validhosts` | empty | the list ListHosts returns; newest entry first (`NewHost` prepends, `:1393-1396`); duplicates are a no-op (`:1375-1378`) |
| `AccessEnabled` | TRUE | `xhost -`/`+` |
| `LocalHostEnabled` | FALSE | admits every local-socket client |
| `UsingXdmcp` | FALSE | set by `-query`/`-broadcast`/`-indirect` (`xdmcp.c:252`) |
| `selfhosts` | — | the server's own interface addresses, loopback included (`DefineSelf`, getifaddrs path `:793-818`). Filled only for non-local listeners (`connection.c:287-288`), so it is empty with `-nolisten tcp`. |

`LocalHostEnabled` has three writers:

- the auth-file load outcome, through `CheckAuthorization`
  (`os/auth.c:196-208`): ≥1 cookie → `DisableLocalAccess`; zero cookies, or
  never loaded → `EnableLocalAccess`;
- `AddHost`/`RemoveHost` of `FamilyLocalHost` (252), i.e. `xhost +local:` /
  `-local:` (`:1322-1325`, `:1410-1413`);
- `ResetHosts`.

`EnableLocalHost` does nothing while `UsingXdmcp` is set (`:275`). Because
`AccessUsingXdmcp` also clears the flag (`:387-391`), an XDMCP Xorg never
opens local access from the auth-file path.

`LocalAccessScopeUser` (`:375`) has no caller anywhere in the tree, so the
`localuser` local-access scope is dead code. The scope is always "host".

**Reset.** `ResetHosts` (`:956-962`) runs at startup and at every reset
(`connection.c:301`, `:349`). It sets `AccessEnabled = !defeatAccessControl`,
`LocalHostEnabled = FALSE` and clears `validhosts`. `ResetAuthorization`
(`os/auth.c:226-233`) forces an auth reload on the next connection, and that
reload derives `LocalHostEnabled` again. So the access list does not survive
a reset.

**Connected clients are never touched.** `ChangeAccessControl`
(`:1616-1623`) assigns a flag. `RemoveHost` (`:1439-1446`) unlinks an entry.
Neither walks `clients[]`. Access is checked only at setup.

### Admission: `ClientAuthorized` (`os/connection.c:511-560`)

The cookie is checked first (`CheckAuthorization`, `:533`). The host list is
consulted only when the cookie fails (`:538-539`). So the list can only add
admissions; it never removes one. A rejected client gets the cookie layer's
reason string (`:553-556`).

`InvalidHost` (`os/access.c:1529-1570`) returns "allowed" for the first of
these that holds:

1. `!AccessEnabled` → everyone (`:1535`).
2. Peer is `FamilyLocal` (AF_UNIX) and `LocalHostEnabled` (`:1540-1555`).
3. Peer is `FamilyLocal`, and some `selfhosts` address is in `validhosts`.
   This "implicitly enables local connections" (`:1542-1552`). Example:
   `xhost +127.0.0.1` on a TCP-listening Xorg opens the Unix socket.
4. A `FamilyServerInterpreted` entry for which `siAddrMatch` is true
   (`:1557-1561`).
5. A non-SI entry exactly equal to the peer's family+address (`:1563-1565`).
   For AF_UNIX `addr` is NULL, so this never matches a local client.

### SI types (`siTypesInitialize`, `:2160-2171`)

The wire form is `type\0value`, with no trailing NUL. Measured on :0 (see
Verification): GDM's entry is 13 bytes, `localuser\0jos`.

| Type | Validation, at Insert **and** Delete (`CheckAddr`) | Match |
|---|---|---|
| `localuser` | value is `#<num>` (`strtol` base 0) or a name `getpwnam` resolves (`:2050-2093`) | peer **euid** == id |
| `localgroup` | `#<num>` or a name `getgrnam` resolves | peer **egid** == id, or a supplementary gid (`:2120-2139`) |
| `hostname` | RFC 2396 hostname syntax (`:1901-1966`) | `getaddrinfo` at check time; Internet/Internet6 peers only (`:1819-1846`) |
| `ipv6` | `inet_pton` (`:1996-2032`) | Internet6 peers only (`:1970-1994`) |

An unknown type, a missing `\0`, or a failed lookup gives `BadValue`
(`siCheckAddr`, `:1745-1777`). The id is resolved again on every match
(`siLocalCredAddrMatch` calls `siLocalCredGetId`, `:2115`). The stored entry
stays the string.

**Supplementary groups: Solaris only.** `GetLocalClientCreds`
(`:1166-1280`) fills `pSuppGids` only on the `getpeerucred` path. On the
`SO_PEERCRED` path (Linux, `:1246-1255`) and the `getpeereid` path (BSD and
macOS, `:1256-1274`), `nSuppGids = 0`. In practice `localgroup` therefore
matches the peer's effective primary gid at `connect()` time and nothing
else. Peer credentials exist only for local transports (`:1196-1199`).

### The requests (`dix/dispatch.c`)

**ChangeHosts** (`:3253-3267`). Encoding: `mode` in the data byte;
`family:CARD8, pad:1, length:CARD16, address, pad4`. Checks, in this order:

1. `REQUEST_FIXED_SIZE` → `BadLength`.
2. Mode is neither Insert (0) nor Delete (1) → `BadValue`, value = mode.
   This check runs before the access check.
3. `AuthorizedClient` (`access.c:1293-1307`): `-ac` → Success; otherwise
   `XaceHook(XACE_SERVER_ACCESS, DixManageAccess)`; otherwise
   `client->local ? Success : BadAccess`.
4. Family (`AddHost`, `:1313-1352`; `RemoveHost`, `:1401-1438`):
   - `FamilyLocalHost` (252) accepts any length and toggles
     `LocalHostEnabled`.
   - Internet (0), Internet6 (6), DECnet (1), Chaos (2) and SI (5) go
     through `CheckAddr` (`:1495-1524`): v4 needs length 4, v6 needs 16, SI
     needs a valid type. DECnet and Chaos always fail. Failure →
     `BadValue`, value = **length**.
   - Any other family, including `FamilyLocal` (256 cannot be encoded in a
     CARD8 anyway) → `BadValue`, value = **family**.
5. Deleting an absent entry → Success. An `Insert` that cannot allocate →
   `BadAlloc`.

**SetAccessControl** (`:3307-3317`): fixed size; mode not 0/1 → `BadValue`
(value = mode), then `AuthorizedClient`.

**ListHosts** (`:3270-3304`): fixed size, then
`XaceHook(XACE_SERVER_ACCESS, DixReadAccess)`. There is no locality check, so
remote clients may list. Reply layout:

- byte 1: `enabled`;
- `length` = entry bytes / 4;
- `nHosts:CARD16` at offset 8;
- then entries (`GetHosts`, `:1451-1491`, capped at 1 MiB):
  `family:CARD8, pad, length:CARD16, addr`, padded to 4.

Byte-swapped clients get `length` swapped per entry (`SLHostsExtend`,
`swaprep.c:648`). Xorg leaves the entry pad bytes **uninitialised**: two
probes on :0 returned `0x22` and `0x0c` for the same entry. We write zeros.

## Design

### Peer identity, captured once

```rust
pub struct PeerIdentity {
    pub transport: AuthTransport,      // Unix | Tcp (exists)
    pub addr: Option<IpAddr>,          // TCP peer
    pub cred: Option<PeerCred>,        // Unix only
}
pub struct PeerCred { pub uid: u32, pub gid: u32, pub pid: Option<u32> }
```

The setup thread reads this from the socket before `auth.check`. It is passed
to `check` and stored in `ClientState`, where it stays immutable for the
connection's lifetime. Once it is stored, `client_peer_pid` (`misc.rs:20`)
reads the cached value instead of calling `getsockopt` per request. Keep this
a capture-only type with no policy in it. SECURITY will classify clients from
it later.

How each platform gets the credentials (all from the libc 0.2.191 we already
depend on):

- **Linux:** `SO_PEERCRED` → `ucred{pid,uid,gid}`. The pattern already exists
  in `misc.rs:20-48`.
- **FreeBSD, OpenBSD, macOS:** `getpeereid` (`libc/src/unix/bsd/mod.rs:627`)
  → `euid, egid`. This is Xorg's `HAVE_GETPEEREID` path. pid is optional:
  `LOCAL_PEERPID` on macOS, `SO_PEERCRED`/`sockpeercred` on OpenBSD. FreeBSD
  stays `None`, as it does today.

Do **not** use `LOCAL_PEERCRED`/`xucred` (which has supplementary groups) on
FreeBSD or macOS. That would make `localgroup` broader than Xorg's on those
platforms only.

If credential lookup fails, `cred = None`, so no SI entry matches. This fails
closed, the same as Xorg's `GetLocalClientCreds == -1` (`:2104`).

### State: inside `AuthState`

The access list goes into `AuthState`'s existing `Inner`, under its existing
mutex. That makes the cookie decision and the host decision one locked step,
with no second lock and no lock ordering to get wrong.

- Setup threads already hold `Arc<AuthState>`.
- The core thread needs a handle for 109/110/111. `run_core` has
  `Arc<AuthState>` (`run.rs`, `accept_pending`). Pass it into
  `process_request` or onto `ServerState`, and make sure a reset's fresh state
  carries it over.

```rust
struct HostAcl {
    generation: Generation,        // the generation this list belongs to
    access_enabled: bool,          // AccessEnabled
    hosts: Vec<HostEntry>,         // newest first; dedup on insert
}
struct HostEntry { family: u8, addr: Vec<u8> }   // exactly what the client sent
```

`Inner.local_open` becomes Xorg's `LocalHostEnabled`: one variable with
Xorg's three writers. A cookie reload writes it (as today). `ChangeHosts` with
`FamilyLocalHost` writes it too. That is new, and it holds even with no
`-auth` file, so the doc comment at `auth.rs:375` saying it is "never mutated"
becomes false and must be rewritten.

**Generation binding.** This copies the session credential's mechanism
(`auth.rs` module doc). `AuthState` outlives a reset, but Xorg's
`ResetHosts` wipes the list. So the list is tagged with the generation it was
built in:

- A `check` from a setup thread bound to G, or a request from the core in G,
  that finds `acl.generation != G` reads the Xorg reset state: enabled, empty
  list.
- The first write in G re-seeds the list (`generation = G`, defaults) and
  then applies the change.
- `local_open` is generation-tagged the same way (`local_open_generation`).
  A forced auth-file reload cannot reset it when there is no `-auth` file, so
  a reader that finds a stale tag takes the Xorg reset value for G instead:
  the value the auth file gives (reloaded, as `ResetAuthorization` does), or,
  with no file, `!xdmcp`. An `xhost +local:` from the old generation
  therefore never survives a reset, with or without `-auth`.

**XDMCP (fixed in #319).** With `xdmcp` set, `local_open` starts `false`
and the reload paths never open it (an empty file, an unreadable
never-loaded file), matching Xorg's `EnableLocalHost` (`:275`). A loaded
cookie still closes it. This design builds on that: under XDMCP only
`FamilyLocalHost` opens local access.

### Admission decision table

`cookie` means the existing order: session credential (generation-bound),
then file cookies (not for TCP in XDMCP mode). Cookie admission and its
reject reasons are unchanged. `H` is evaluated only when the cookie fails.

| Transport | Mode | Valid cookie | Result |
|---|---|---|---|
| Unix | any | yes | admit (unchanged) |
| Unix | any | no | admit iff **H(peer)**, else the cookie layer's reason |
| TCP | `-listen tcp -auth F` | yes (file) | admit (unchanged) |
| TCP | XDMCP | yes (session, same generation) | admit (unchanged) |
| TCP | any | no | **reject. H is not called**, so `xhost +` cannot change this (structural, as in the 2026-09-09 spec) |

`H(peer)` for a Unix peer follows Xorg's `InvalidHost` order, first match
wins:

1. `!access_enabled` (`xhost +`);
2. `local_open`;
3. a `FamilyInternet` entry equal to one of our own addresses
   (`address_is_ours`, `run.rs:4750`), counted only while a TCP listener is
   open. This mirrors `selfhosts` being empty without one. Only IPv4 for
   now, because the listener is IPv4-only (`lib.rs:260`);
4. `si:localuser` id == `cred.uid`;
5. `si:localgroup` id == `cred.gid`.

`hostname` and `ipv6` SI entries never match a Unix peer in Xorg either.

What this means per configuration:

| Configuration | Unix without a cookie, out of the box | Effect of `xhost` |
|---|---|---|
| no `-auth` | admitted (`local_open`) | `xhost -local:` closes it; `xhost -` does not, because `local_open` still admits |
| `-auth`, ≥1 cookie (lightdm, GDM-like) | rejected | `+si:localuser:bob` admits bob; `+` admits every local user; `+local:` admits every local user until the next cookie reload |
| `-auth` + `-listen tcp` | as above | TCP is unaffected by any `xhost` |
| XDMCP | rejected (after the fix) | as with `-auth` for Unix; TCP is session-cookie only |

### Who may change the list

There is one predicate, `may_manage_access(client)`. It returns
`client.is_local`, which is already Xorg's notion: Unix, or TCP from one of
our own addresses. It returns `BadAccess` otherwise. `ListHosts` gets the same
kind of hook (`may_read_access`), which always returns true for now. These
two functions are where SECURITY's `DixManageAccess`/`DixReadAccess` will go.
Nothing else in this design assumes a trust level.

Error order and error values follow the Reference section exactly. `BadValue`
values are mode, length or family as listed there; they were measured on
Xorg. Inserts that cannot grant anything are accepted, stored and listed like
Xorg, and each one logs one `warn!` naming why: an Internet or Internet6 entry
that is not one of our addresses, or an SI `hostname`/`ipv6` entry, because
the list never admits TCP.

SI name resolution uses `getpwnam_r`/`getgrnam_r`, never the non-reentrant
calls, because setup threads run concurrently. `#<num>` copies `strtol` base
0: `#0x3e8`, `#01750` and `#1000x` are all accepted.

- At **insert/delete**, resolution happens on the core thread, as Xorg does
  it on its only thread.
- At **check**, resolution is done again (Xorg semantics: a renamed user
  stops matching). It runs on the setup thread, on a snapshot of the list
  taken under the lock, with the lock **released** during the NSS calls. The
  admission decision is therefore made against the list as it was at that
  moment.

## Invariants

1. No list state, including `xhost +`, admits a TCP client without a valid
   cookie. Carried over from the 2026-09-09 spec.
2. A valid cookie always admits. The list only ever adds admissions.
3. With the default state (enabled, empty list), admission is exactly
   master's. The XDMCP local-access fix is the one intended exception.
4. Only an `is_local` client can change the list or the mode.
5. A reset restores Xorg's `ResetHosts` state through the generation
   mismatch. Nothing granted in one generation admits a client in the next.
6. Changing the list or the mode never touches connected clients.
7. Peer identity is read once, at setup, from the socket. It is never derived
   from anything the client sends.

## Verification

**Unit tests (`auth.rs`).**

- All 20 rows of the H table: each H source on and off × Unix/TCP, plus
  `access_enabled` both ways on TCP, plus XDMCP with and without a session
  credential.
- Generation mismatch reads defaults; a write re-seeds.
- `FamilyLocalHost` interacts with the cookie reload
  (`+local:`, then a cookie reload re-closes it).
- XDMCP `local_open` starts closed and stays closed after a reload of an
  empty or unreadable auth file, at startup and later; a loaded cookie still
  closes a `+local:`.
- `xhost +local:` in G, then a reset to G+1 with no `-auth`: closed under
  XDMCP, open (the default) otherwise, and with `-auth` derived from the file.

**Request tests.** Byte-exact against measured Xorg replies. Each bad-request
case is pinned to its `(error, value)` pair:

| Case | Error | Value |
|---|---|---|
| SI with unknown type | BadValue | length |
| SI with unknown user | BadValue | length |
| family 1 | BadValue | length |
| family 7 | BadValue | 7 |
| v6 with wrong length | BadValue | length |
| mode 2 | BadValue | 2 |
| `SetAccessControl` mode 2 | BadValue | 2 |
| `ListHosts` with extra length | BadLength | — |

Plus: `BadAccess` for a non-local client on 109/111 but not on 110;
insert/delete round-trips, dedup and newest-first order; a big-endian client
gets entry lengths swapped.

**Peer credentials.** A `UnixStream::pair` test asserts our own uid/gid on
every CI target: Linux, FreeBSD, OpenBSD, macOS.

**Measured oracle.** A python probe (kept in the session scratchpad; adding it
under `tools/` is open question 4) ran against Xorg :0 under GDM on
2026-10-30:

- `ListHosts` reply is `01 01 …`, 1 host, length 5, entry `05 ?? 0d 00`
  `localuser\0jos`. `enabled = 1`.
- The error values in the table above were confirmed. All of these cases are
  non-mutating.

**End to end (vng).** The vng suite already has a throwaway user. Under
`-auth`: the second user is refused; `xhost +si:localuser:<user2>` admits
them; `xhost -si:…` refuses them again, and their open connection survives;
`xhost +`/`-`; `xhost` output matches. Also a TCP client without a cookie
after `xhost +`: refused.

There is no xts A/B for this change, since nothing is drawn.

## Risks

- **NSS on the core thread.** ChangeHosts resolves names with LDAP/sssd
  latency, just as Xorg does. `xhost` is rare, but a hung NSS stalls the
  server.
- **Snapshot race.** A connection racing an `xhost -si:` may be admitted
  against the list from just before the change. Xorg serialises this; we
  don't.
- **`localgroup` is narrower than users expect.** It checks the primary
  egid only, as Xorg does on Linux and the BSDs. Being faithful is the safe
  direction here.
- **Silent TCP divergence** (carried over): `xhost +host` succeeds and admits
  nothing. Mitigated by the `warn!` and a line in `docs/setup.md`.

## Decisions (review 2026-10-30)

1. **`-ac`** stays ignored; it keeps warning as unrecognized and fails closed.
2. **XDMCP `local_open`** is fixed first on its own branch, including the
   reload paths; see above.
3. **`localgroup`** matches the primary gid only, as Xorg does.
4. The measurement probe stays out of `tools/` for now.
