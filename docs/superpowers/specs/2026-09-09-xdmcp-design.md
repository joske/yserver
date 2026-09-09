# XDMCP — design

## Status

Draft, unimplemented. Written 2026-09-09 against master `64d4b6e4`.

Stage 4 of four for [#121](https://github.com/joske/yserver/issues/121), and
the one the issue is actually about. The other three exist to make it possible:

1. ~~TCP transport~~ — implemented, verified loopback and remote (`bee:2`).
2. ACL — specified, deferred; **XDMCP does not need it**
   (`AccessUsingXdmcp` only *tightens*, and the DM authorizes by cookie).
3. ~~Server reset~~ — implemented; gated on the composite-overlay claim fix.
4. **XDMCP** ← this spec.

## Goal

Run as an XDMCP display: query a display manager, obtain a session, and when
that session ends, reset and query again. Target deployment is the one in the
issue — LightDM's XDMCP daemon on an HPC login node, with the X server on a
thin client.

**Non-goals:**

- **XDM-AUTHENTICATION-1.** The DES-based mutual authentication is optional in
  the protocol and off by default in LightDM and GDM. We send an empty
  authentication name and accept `Willing` offering none. If a deployment
  demands it, that is a separate piece of work.
- **Multicast** (`-multicast`, XDMCP 1.1). `-query`, `-indirect` and
  `-broadcast` cover the issue's use case.
- **The chooser.** `-indirect` gets us a `Willing` from whichever manager the
  indirect query resolves to; we do not implement the chooser UI protocol.
- IPv6, consistent with the IPv4-only listener from stage 1.

## Current behaviour on master

Nothing. No UDP socket, no packet types, none of the options parsed.

## Reference: how Xorg does it

`../xserver/os/xdmcp.c`, ~1550 lines, of which the protocol is a small part.

**The state machine** starts at `XDM_INIT_STATE` (`:80`), which the options set
to `XDM_QUERY`, `XDM_BROADCAST`, `XDM_MULTICAST` or `XDM_INDIRECT`
(`:254-276`). The exchange is:

| We send | Manager replies |
|---|---|
| `Query` / `BroadcastQuery` / `IndirectQuery` | `Willing` (or `Unwilling`) |
| `Request` | `Accept` or `Decline` |
| `Manage` | *(session starts)* or `Refuse` / `Failed` |
| `KeepAlive` | `Alive` |

Handlers at `:954-1317`. Retransmission is timer-driven with backoff.

**The authorization arrives in the `Accept` packet** and is installed **in
memory**: `recv_accept_msg` (`:1168`) calls `XdmcpAddAuthorization` (`:899`),
which is `AddAuthorization` from `os/auth.c`. There is no file involved.

**Session end couples reset and re-query.** On the session socket closing
(`:645`) and on "declaring session dead" (`:805`):

```c
state = XDM_INIT_STATE;                              /* back to Query */
dispatchException |= (OneSession ? DE_TERMINATE : DE_RESET);
```

So an established session always ends at a generation boundary, and `-once`
means terminate instead of looping. Note the converse does **not** hold: the
generic retransmission limit (`:826`) calls `XdmcpDeadSession` regardless of
state, so a negotiation that never reached a session can reset a generation
too. See invariant 2 — "one session = one generation" is too strong.

## Design

### Two things that are not protocol work

The packet handling is the easy half. Two integrations are where the work is,
and both were found by reading rather than assumed:

#### 1. `AuthState` cannot accept a cookie at runtime

`core_loop/auth.rs` exposes exactly `new(file)`, `require_tcp_auth_at_startup`
and `check` — the cookie list is loaded from `-auth`'s file and nothing else.
XDMCP delivers the session cookie in an `Accept` packet, so **there is
currently no way to install it.**

Required: a second, in-memory cookie source alongside the file one, which
`check` consults identically. Two properties matter, and they pull in opposite
directions from the file source:

- **It is per-offer, not merely per-process.** The manager issues a new cookie
  with each `Accept`. A cookie from the previous user's session authorizing a
  client in the next one is the same class of leak stage 3 exists to prevent.
- **`AuthState` deliberately survives a reset.** It sits outside `ServerState`
  as an `Arc`, and the reset spec keeps it that way because the *file* cookie
  is process-lifetime. So the XDMCP cookie cannot be forgotten along with the
  state — its invalidation has to come from somewhere else. That is what the
  generation binding below provides, *without* an explicit clear at the reset
  boundary.

**Decided: generation-bound, and cleared on abandonment.** Not an explicit
reset-time clear alone — that is a call which can be missed or raced.

- `AuthState` stores `{ generation, cookie }`.
- Setup authentication compares it against **the setup thread's own bound
  producer generation** — the binding introduced by the reset work's
  `BoundSender`, captured at accept — *not* the global current generation. A
  reset then invalidates the cookie by mismatch, immediately and without
  anyone remembering to clear anything.

Generation binding alone is **not sufficient**, because an Accept can be
abandoned with no generation change. `recv_refuse_msg` (`xdmcp.c:1264`) takes
`XDM_AWAIT_MANAGE_RESPONSE` back to `XDM_START_CONNECTION` and resends
`Request`; the cookie from the refused Accept is still installed and the next
Accept brings a different one. So the cookie belongs to **the accepted offer**,
not merely to the generation: clear or replace it on every path that leaves
`AWAIT_MANAGE_RESPONSE` without a running session.

To be exact about the division of labour, because it is the whole point of
this design:

| How an offer dies | What invalidates the cookie |
|---|---|
| reset / new generation | the bound-generation **mismatch** — no clear call, nothing to miss or race |
| abandoned in the same generation (`Refuse`, and any other exit from `AWAIT_MANAGE_RESPONSE` without a session) | an **explicit** clear/replace, because no generation changed |

**Once the session is running, a late `Refuse` must not clear anything.**
Xorg's `recv_refuse_msg` returns immediately unless
`state == XDM_AWAIT_MANAGE_RESPONSE` (`xdmcp.c:1264`), so a stray or delayed
refusal cannot disturb a live session. Our equivalent must be serialised
against the TCP setup that takes the state to `RUN_SESSION`: an authenticated
setup arriving concurrently with a `Refuse` must not end with a running
session whose cookie has just been cleared.

### What counts as an acceptable `Accept`

`Accept` carries **two** credential pairs, and they are checked separately
(`recv_accept_msg`, `xdmcp.c:1168`):

**1. `AcceptAuthenticationName` must be EMPTY; its `Data` is ignored.** We
advertise no authentication, because XDM-AUTHENTICATION-1 is a non-goal. A
non-empty *name* means the manager selected a mode we do not implement, and
accepting it would silently bypass the authentication the manager chose. Xorg
validates through `XdmcpCheckAuthentication(..., ACCEPT)` and, on failure,
calls `XdmcpFatal("Authentication Failure", ...)` — **fatal, not a retry**.
Match that: refuse to run rather than proceed under a mode we cannot honour.

Be precise about the *data*, though, because the obvious stricter reading is
wrong:

```c
return (XdmcpARRAY8Equal(Name, AuthenticationName) &&
        (AuthenticationName->length == 0 ||
         (*AuthenticationFuncs->Validator)(AuthenticationData, Data, packet_type)));
```

With our configured name empty, the `length == 0` short-circuit means the
accompanying data is **never examined** — so Xorg accepts an empty name with
non-empty data. We do the same, and log the stray data at debug rather than
refusing it. An earlier draft of this spec required both to be empty; that is
a divergence with nothing forcing it, and unlike the authorization case below
(where Xorg's fallback needs a host ACL we do not have) there is no gain to
weigh against the risk: with no authentication mode selected the data is inert
and we would never interpret it, while refusing it could break a manager that
sends stray bytes — in the exact deployment this stage targets.

**2. `AcceptAuthorizationName` must be `MIT-MAGIC-COOKIE-1`**, with well-formed
data. That is the only name our runtime auth layer recognises
(`auth.rs:150`).

**Here we diverge from Xorg deliberately.** When `XdmcpAddAuthorization` fails,
Xorg does *not* abandon the offer: it calls `AddLocalHosts()` and proceeds to
`XDM_MANAGE` anyway — "if the authorization specified in the packet fails to be
acceptable, enable the local addresses". That fallback depends on the
host-based access control we have deliberately not implemented (stage 2,
deferred). Without it, proceeding would create a session no TCP client can ever
authenticate to: an obscure hang instead of a diagnosable refusal. So an
unusable authorization **fails the offer** for us.

**The transition on a failed offer**, which must not be left to inference — and
note it differs from `Refuse`, which arrives in `AWAIT_MANAGE_RESPONSE` whereas
this arrives in `AWAIT_REQUEST_RESPONSE`:

**Stay in `AWAIT_REQUEST_RESPONSE` and let the retry timer drive.** Do not jump
back to `START_CONNECTION` and immediately resend `Request`. This is
Xorg-faithful — a malformed or short `Accept` falls through `recv_accept_msg`
without touching `state`, so retransmission handles it — and it is the bounded
choice: an immediate resend against a manager that keeps answering badly is a
tight loop, whereas the retry path backs off and terminates at the
retransmission limit via `XdmcpDeadSession` (or exits, under `-once`).

Any provisional cookie from the rejected offer is cleared regardless.

This is the first untrusted input in the whole flow: anything that can answer
our `Query` can send an `Accept`.

### The stage-1 contradiction, and how it resolves

Stage 1 made `-listen tcp` a **startup error** unless `-auth` yields a usable
cookie. XDMCP has no cookie at startup — it arrives in the `Accept`, and TCP
must already be listening for the manager's session to connect. As written the
two rules cannot both hold.

Resolution: **XDMCP is an approved dynamic authorization source** for the
purposes of that startup check, so `-query` and friends satisfy it in place of
`-auth`. Before an `Accept` has been received, TCP setup **fails closed** —
there is no cookie, so nothing authorizes.

And the decision that follows, which must be explicit: **in XDMCP mode, TCP
setup accepts the generation-bound XDMCP cookie only.** File cookies do not
authorize a TCP client while XDMCP is driving the session. Honouring both
would let a cookie in a local file authorize a client in a session it has
nothing to do with, which defeats the per-session model this whole stage is
built on. Unix clients are unaffected.

#### 2. XDMCP owns the reset policy

Stage 3 defaults to `-noreset`, deliberately opposite to Xorg. XDMCP inverts
that: an established session ends its generation, and the loop *is* the
feature. So an XDMCP option implies `-reset`.

**`-once` is stronger than "terminate at session end".** It turns *every*
XDMCP-driven reset-or-renew condition into termination: session end, keepalive
failure, and — the one easy to miss — generic retransmission exhaustion during
a negotiation that never established a session. Xorg's timeout handler checks
`OneSession` before `XdmcpDeadSession` is ever reached (`xdmcp.c:826-834`), so
a `-once` server that cannot reach its manager exits rather than looping
forever. That case needs its own test, because it is the one with no session
in it.

The reset boundary gains an XDMCP hook: on a new generation, the state machine
returns to its init state and re-queries. It must run **after** the new
generation is installed — the cookie it is about to receive belongs to the new
one.

### The socket

A UDP socket in the existing mio poll set with its own token, alongside the
listeners from stage 1. Port 177 to the manager; the reply port is ours.
Retransmission needs a timer; the loop already has a poll timeout computed per
iteration, so the XDMCP retransmit deadline joins that computation rather than
introducing a thread.

**No new thread.** The state machine is small, event-driven and belongs on the
core loop, where it can see the generation boundary directly.

### Options

`-query <host>`, `-indirect <host>`, `-broadcast`, `-port <n>`, `-from <addr>`,
`-class <str>`, `-displayID <str>`, `-once`. Ordered, last-wins, mirroring
Xorg's `:252-312`. `-cookie` (the XDM-AUTHENTICATION-1 key) is parsed and
rejected with a clear message rather than silently ignored, since accepting it
would imply an authentication mode we do not implement.

## Invariants

1. A session's cookie authorizes only that session — enforced by generation
   binding, and by clearing the provisional cookie when an accepted offer is
   abandoned. A new generation never inherits the previous one's cookie, and
   nor does a retried negotiation within one.
2. **Every established XDMCP session ends at a generation boundary; a failed
   negotiation may also restart a generation.** The weaker second clause is
   Xorg-faithful and load-bearing: the generic retransmission limit
   (`xdmcp.c:826`) calls `XdmcpDeadSession` regardless of state, so exhausting
   retries during Query or Request resets with no session having existed.
   Generations can therefore exist with zero sessions, and an invariant saying
   "one session = one generation" would be false.
3. `-once` turns **every** XDMCP-driven reset-or-renew condition into
   termination — session end, keepalive failure, and retransmission exhaustion
   during a negotiation that never established a session — not only session
   end.
4. With no XDMCP option, nothing changes: no UDP socket, no state machine, and
   the reset policy stays `-noreset`.
5. A manager that never answers leaves the server retrying, not wedged or
   spinning — retransmission is bounded and backs off.

## Risks

- **The cookie lifetime is the security-critical part**, not the packet
  parsing. Everything else here is recoverable; a stale session cookie is
  cross-user access on a shared login node, which is exactly what #121's
  deployment is.
- **XDMCP is unauthenticated and unencrypted** without XDM-AUTHENTICATION-1,
  which we are not implementing. Anyone who can spoof a `Willing` can offer a
  session. That is true of Xorg in the same configuration and is why XDMCP
  deployments assume a trusted network — the man page must say so plainly.
- **`-broadcast` and `-indirect` are hard to test** without a second machine
  running a manager; `-query` against a local LightDM is the tractable case.
- Retransmission timers interacting with the loop's existing poll-timeout
  computation is the fiddliest non-protocol part.

## Verification

- Unit: packet encode/decode round-trips for all nine message types against
  byte vectors taken from the protocol spec, **not** from our own encoder —
  a self-consistent codec that is wrong on the wire passes every round-trip
  test. State-machine transitions including `Unwilling`, `Decline`, `Refuse`
  and `Failed`.
- Cookie lifetime, the tests this spec exists for: an `Accept` installs a
  cookie that authorizes a client; after a reset, a client presenting the
  previous session's cookie is refused **by generation mismatch** — assert the
  mechanism, not just the refusal, or a test passes for the wrong reason. And
  the abandoned-offer case with no reset in it: `Accept`, then `Refuse`, then a
  second `Accept` with a different cookie — a client presenting the *first*
  cookie is refused.
- The stage-1 interaction: `-listen tcp` with an XDMCP option and no `-auth`
  starts; a TCP client connecting **before** any `Accept` is refused; a file
  cookie does not authorize a TCP client while XDMCP is driving.
- Integration: `-query` against LightDM with XDMCP enabled, on one machine
  first, then across the LAN. A session starts, ends, and a second session
  starts on the same server.
- `-once`, both cases: the server exits at session end, **and** exits on
  retransmission exhaustion during a negotiation that never established a
  session — the second is the one a happy-path suite skips.
- `Accept` rejection, as protocol vectors: an authorization name other than
  `MIT-MAGIC-COOKIE-1`, and a malformed/empty cookie, each leave the state
  machine in `AWAIT_REQUEST_RESPONSE` with no credential installed. A
  **non-empty authentication name** is fatal. And the fidelity case that is
  easy to get backwards: an **empty authentication name with non-empty data is
  ACCEPTED**, matching `XdmcpCheckAuthentication`'s short-circuit.
- No XDMCP option ⇒ byte-identical behaviour to today.

## Adjacent gaps, not in scope

- The chooser protocol for `-indirect` (`XdmcpAddHost`, `xdmcp.c:698`, is the
  manager list for indirect queries — unrelated to the X access list despite
  the name).
- XDM-AUTHENTICATION-1, per the non-goals.
