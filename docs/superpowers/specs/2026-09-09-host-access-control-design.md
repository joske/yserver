# Host access control (xhost) — design

## Status

Draft, unimplemented. Written 2026-09-09 against master `64d4b6e4`, with
stage 1 (TCP transport) implemented on `feat/121-tcp-transport` and confirmed
working over both loopback and a real remote client (`bee:2`).

Stage 2 of four for [#121](https://github.com/joske/yserver/issues/121):

1. ~~TCP transport~~ — done, `feat/121-tcp-transport`
2. **ACL / host access control** ← this spec
3. Server reset (LightDM resets the X server after logout)
4. XDMCP itself

**Decided 2026-09-09 (jos): fail-closed for TCP.** Host-list grants authorize
`FamilyLocal` clients only; no host entry and no `xhost +` ever authorizes a
TCP client without a valid cookie. This is a deliberate divergence from Xorg,
taken because the fail-open direction is the one that cannot be undone. See
"The collision" for the alternatives and why they were not taken.

## Goal

**Scoped 2026-09-09 (jos): the minimum that keeps `xhost` working and cannot
weaken the cookie rule. Not full network transparency — `ssh -X` covers that,
and it never touches this code (it proxies through a LOCAL unix socket on the
remote host). TCP in yserver exists solely to serve XDMCP.**

Implement the three access-control requests so `xhost` is not broken:
`ChangeHosts` (109), `ListHosts` (110) and `SetAccessControl` (111), backed by
real state, with the list never authorizing a TCP client.

**XDMCP needs nothing from this stage.** `AccessUsingXdmcp()`
(`os/access.c:379`) is the entire interaction between XDMCP and access
control, and it only *tightens*:

```c
UsingXdmcp = TRUE;
LocalHostEnabled = FALSE;
```

XDMCP authorizes by the cookie the display manager sends in its Accept packet.
(`XdmcpAddHost`, `xdmcp.c:698`, is unrelated — the chooser's list of managers
for indirect queries, not the X access list.) So this stage exists to avoid
leaving `xhost` erroring and `ListHosts` stubbed, not to enable stage 4.

**Non-goals:**

- **`FamilyServerInterpreted` (`si:localuser:`) matching.** Xorg's
  `siAddrMatch` is the modern local idiom, but nothing in the XDMCP workflow
  uses it. Parse and store `si:` entries so `ListHosts` round-trips them;
  `BadValue` nothing, match nothing. Revisit only if asked for.
- `/etc/X<n>.hosts` boot-time host files. Xorg reads them in `ResetHosts`;
  nothing here needs them and they are a surprising source of grants.
- IPv6 host entries. Our listener is IPv4-only (stage 1); revisit with IPv6.
- XSECURITY / `XaceHook` trust levels. Xorg's `AuthorizedClient` consults
  `XaceHookServerAccess`; we have no XACE and do not need one here.
- Any behaviour that makes the host list able to grant network access. That is
  the point of the decision below.

## Current behaviour on master

- **`110 ListHosts` is dispatched** (`process_request.rs:273` →
  `handle_list_hosts:22288`) and writes a reply with an empty list via
  `x11::write_list_hosts_reply`.
- **`109 ChangeHosts` and `111 SetAccessControl` are not dispatched at all**,
  so `xhost +foo` and `xhost +`/`xhost -` fail. This is the "no protocol
  stubs" rule's territory: `xhost` currently gets an error where Xorg gives it
  a working command.
- There is no host list, no `AccessEnabled` flag, and no consultation of
  either at setup. Authorization is cookie-only (`core_loop/auth.rs`), made
  fail-closed for TCP in stage 1.

## Reference: how Xorg does it

All line numbers are `../xserver`.

**The decision at connect** — `ClientAuthorized` (`os/connection.c:510`):

```c
auth_id = CheckAuthorization(...);        /* the cookie */
if (auth_id == ~0L) {                     /* cookie absent or wrong */
    if (!InvalidHost(from, fromlen, client))
        auth_id = 0;                      /* host listed ⇒ ACCEPT anyway */
    if (auth_id == ~0L) return reason;    /* otherwise reject */
}
```

The host list is therefore **not** an additional restriction. It is a
**fallback that overrides a failed cookie check**. This is the single most
important fact in this document.

**The predicate** — `InvalidHost` (`os/access.c:1475`):

- `if (!AccessEnabled) return 0;` — access control off ⇒ *everyone* is
  allowed, with no cookie. This is what `xhost +` does.
- `FamilyLocal`: allowed when `LocalHostEnabled`, else when any of the
  server's own addresses appears in `validhosts` ("implicitly enables local
  connections").
- Otherwise a linear walk of `validhosts`, matching either
  `FamilyServerInterpreted` entries via `siAddrMatch` or an exact
  family+address compare.
- `ConvertAddr` (`:1518`) maps AF_UNIX → `FamilyLocal`, AF_INET →
  `FamilyInternet`, and v4-mapped IPv6 → `FamilyInternet`.

**Who may change it** — `AuthorizedClient` (`os/access.c:1250`) ends:

```c
return client->local ? Success : BadAccess;
```

So **a remote client cannot run `xhost` at all**, in any form. That is a real
mitigation and we should keep it: the blast radius of `xhost +` is limited to
someone who already has local access.

**State**: `AccessEnabled` (`:226`, default TRUE) and `LocalHostEnabled`
(`:227`, default FALSE, set by `EnableLocalHost`/`AddHost(FamilyLocal)`).

## The collision — and the decision taken

Stage 1 established: **a TCP client is authorized only by a correct cookie
from a successfully loaded auth file**, and `-listen tcp` is a startup error
otherwise. That was deliberately stricter than Xorg, which with no `-auth`
accepts remote clients outright.

Xorg's ACL semantics defeat exactly that guarantee:

- `xhost +` sets `AccessEnabled = FALSE`, and `InvalidHost` then returns 0 for
  every client ⇒ **any host on the network gets the display with no cookie.**
- `xhost +somehost` does the same for one host.

Both are documented, expected X11 behaviour that scripts rely on. So
implementing Xorg faithfully means a single local `xhost +` silently undoes
stage 1. Three options were considered; **B was chosen**.

**A. Faithful — NOT taken.** Implement Xorg's semantics exactly. `xhost` behaves as
documented and as every existing script expects. A local user can open the
display to the network with one command — as on Xorg. Argument for: divergence
from Xorg is our bug by default, and users who type `xhost +` on a networked
display are making a choice X11 has always let them make.

**B. Fail-closed for TCP — ✅ CHOSEN.** The host-list fallback authorizes `FamilyLocal`
clients only; a TCP client always needs a valid cookie regardless of the host
list or `AccessEnabled`. `xhost` still works and still lists hosts, but cannot
grant network access. Argument for: it preserves the property stage 1 exists
to provide, and #121's actual mechanism is cookie-based (LightDM's XDMCP hands
the client a session cookie), so nothing in the target workload needs the
fallback. Argument against: `xhost +host` silently not working is a divergence
a user will report as a bug, and it is invisible — the command succeeds and
the connection still fails.

**C. Faithful, behind an opt-in — NOT taken.** Xorg semantics only when a flag such as
`-ac`-alike is passed; otherwise B. Makes the deviation explicit and
recoverable. Cost: a third configuration axis, and a flag Xorg does not have.

**The divergence must be loud, not silent.** `ChangeHosts` still succeeds and
the entry is still stored and listed — refusing it would break `xhost` itself
— but a grant that cannot take effect for TCP logs a warning naming it, and
`docs/setup.md` states plainly that host-based grants do not authorize TCP
clients. A user who types `xhost +bee` and sees the connection refused must be
able to find out why without reading the source.

Reasons B over A: the fail-open direction hurts irrecoverably; #121's own
deployment authenticates by cookie (LightDM's XDMCP hands the session one), so
the target workload never needs the fallback; and unlike Xorg we have no
decades of deployment scrutiny on this listener. Reason B over C: a flag Xorg
does not have is a third configuration axis to document, test and support, for
a capability whose only use is to weaken the server.

Xorg's `client->local` restriction on *modifying* the list is kept regardless.

## Design

### State

`ServerState` gains an access-control block: `access_enabled: bool` (default
true) and `hosts: Vec<HostEntry>`, where `HostEntry` is
`{ family: u16, address: Vec<u8> }`. Families we accept: `FamilyInternet` (0),
`FamilyLocal` (256) and `FamilyServerInterpreted` (5). `FamilyInternet6` (6) is
parsed and stored so `ListHosts` round-trips it, but never matches while the
listener is IPv4-only.

### The three requests

- **`ChangeHosts` (109)** — `mode` in `data` (Insert=0, Delete=1), body
  `family(1) pad(1) length(2) address`. Reject non-local callers with
  `BadAccess` (`os/access.c:1264`). `BadValue` for an unknown family or a
  length that disagrees with the family's fixed size.
- **`ListHosts` (110)** — replace the empty stub with the real list, and set
  the reply's `mode` byte from `access_enabled`. This is a "return real state"
  fix independent of everything else here.
- **`SetAccessControl` (111)** — `mode` in `data` (Disable=0, Enable=1).
  `BadAccess` for non-local callers, `BadValue` for other modes.

### `FamilyServerInterpreted` (si:) — stored, not matched

`xhost +si:localuser:jos` is the modern local idiom and Xorg matches it via
`siAddrMatch`. Out of scope per the goal: accept the entry, store it, list it
back, and never match it. Rationale — matching it correctly means peer-uid
lookup and group resolution, and the only clients it would admit are local
ones that already authorize by cookie. Implementing it is a separate,
self-contained follow-up if a user asks; `SO_PEERCRED`
(`process_request.rs:13240`) already supplies the uid it would need.

### The setup-path hook

One predicate, `host_allows(client_transport, peer_addr) -> bool`, consulted in
the auth path **only when the cookie check fails**, mirroring
`ClientAuthorized`. Per the decision it returns false for any non-local
transport *before* consulting the list or `access_enabled` at all — the
fail-closed behaviour is structural, not a condition inside the walk that a
later edit could invert.

## Invariants

1. A remote client can never modify the host list or toggle access control
   (`BadAccess`), regardless of the option chosen.
2. `ListHosts` reflects real state: every host added is listed, every host
   deleted is not, and the enabled flag tracks `SetAccessControl`.
3. A correct cookie always authorizes, irrespective of the host list — the
   fallback only ever *adds* acceptance, never removes it.
4. **No host-list configuration, including `xhost +`, causes a TCP client
   without a valid cookie to be accepted.** This is the stage-1 guarantee and
   the reason for the divergence; it is the one invariant whose violation is
   not recoverable after the fact.
5. Unix clients behave exactly as on master when the list is empty and access
   control is enabled — the default state must be a no-op.

## Risks

- **Silent divergence is the live risk of the chosen option.** `xhost +host`
  reporting success while not granting anything is the failure mode most
  likely to waste a user's day, and it is invisible from the client side.
  Mitigated by the warning log and documentation, not eliminated. If reports
  come in that this confuses people more than it protects them, revisiting to
  option C is the escape hatch — the predicate is one function.
- **What A would have cost**, recorded so the trade is not re-litigated from
  scratch: `xhost +` would be a one-command exposure of the display to the
  whole network, reachable because stage 1 binds `0.0.0.0`.
- **Address comparison.** Exact byte compare per family; do not normalise or
  resolve names in the server. Xorg stores what the client sent.
- **`ListHosts` reply shape.** The existing stub already writes a reply; the
  encoder must be checked against the real per-entry format
  (`family(1) pad(1) length(2) address` padded to 4) rather than assumed.

## Verification

- Unit: each request's success and error paths; `BadAccess` for a non-local
  caller on 109 and 111; `BadValue` for bad families, modes and lengths;
  `ListHosts` round-trips insert/delete and the enabled flag.
- The full decision table, driven through the setup path: cookie ok/bad ×
  local/remote × host listed/not × access enabled/disabled = 16 cases,
  exhaustive rather than sampled — this is where a fail-open bug hides. Every
  remote row with a bad cookie must reject, including both `access_enabled`
  values.
- Integration: `xhost`, `xhost +`, `xhost -`, `xhost +si:localuser:$USER` on a
  live server, checking `xhost` output matches the state actually enforced.
- No xts A/B: nothing here draws. (`feedback_xts_ab_gates_pixel_changes` is
  scoped to pixel changes.)

## Adjacent gaps, not in scope

- **`ListHosts` is currently a stub returning an empty list** — a pre-existing
  "no protocol stubs" violation that this stage happens to fix.
- Xorg's `/etc/X<n>.hosts` and the `-ac` flag are both unimplemented and stay
  that way; if `-ac` is ever added it should be the option-C switch, not a
  separate mechanism.
