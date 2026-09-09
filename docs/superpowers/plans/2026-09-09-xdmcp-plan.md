# XDMCP — implementation plan

Implements `../specs/2026-09-09-xdmcp-design.md`. Read that first; this plan
does not restate its reasoning, only what to build, in what order, and what
proves each part.

**Branch:** `feat/121-xdmcp`, already created off `feat/121-server-reset`,
carrying the spec.

**Deliverable:** `yserver :7 -query <host> -listen tcp` obtains a session from
a LightDM XDMCP daemon, runs it, and when the session ends resets and queries
again. `-once` exits instead.

## Ordering principle

**Codec, state machine, config, then integrations, then the loop.** The codec
is pure and testable against protocol byte vectors with nothing else in place.
The state machine is a pure transition function — over packets, timers *and*
the session-client lifecycle events — testable with no socket. `XdmcpOptions`
comes third rather than last because the reset and socket wiring both need a
production configuration source to read. Only then do the two integrations
that carry the real risk — auth and reset — get wired, and only after that
does any of it touch the core loop.

Nothing sends a packet until step 6. Steps 1-2 are pure functions.

## Prerequisites

- `cargo +nightly fmt`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test` clean before each commit.
- **No xts A/B.** Nothing here draws.
- **Read the handlers line by line, and quote them in the code comments.** Six
  review rounds on the spec each turned on a detail of `os/xdmcp.c` that had
  been summarised rather than read: `XdmcpCheckAuthentication`'s short-circuit,
  the `AddLocalHosts` fallback, the state-preserving fall-through on a
  malformed `Accept`, the `OneSession` check inside the timeout handler. Treat
  a paraphrase of that file as unreliable, including the paraphrases in this
  plan.

---

## Step 1 — the wire codec

Pure encode/decode for the thirteen message types: `Query`, `BroadcastQuery`,
`IndirectQuery`, `Willing`, `Unwilling`, `Request`, `Accept`, `Decline`,
`Manage`, `Refuse`, `Failed`, `KeepAlive`, `Alive`. `ARRAY8`, `ARRAY16`,
`ARRAY32` and `ARRAYofARRAY8` primitives underneath.

**Proof.** Vectors for **every one of the thirteen**, not just the ones the
happy path uses. Round-trips are necessary but **not sufficient** — a self-consistent
codec that is wrong on the wire passes every round-trip test. So the vectors
must come from the protocol specification and from real captured packets, not
from our own encoder. Include the length-field arithmetic explicitly:
`recv_accept_msg` validates `length == 12 + sum of the four ARRAY8 lengths`
(`xdmcp.c:1185`), and a decoder that ignores the declared length will accept
truncated packets.

## Step 2 — the state machine as a pure function

States per `xdmcp.c:80` and its option assignments: `Off`, `Query`,
`Broadcast`, `IndirectQuery`, `CollectQuery`, `StartConnection`,
`AwaitRequestResponse`, `Manage`, `AwaitManageResponse`, `RunSession`,
`KeepAlive`, `AwaitAliveResponse`.

Model it as `(state, event) -> (state, actions)` with no I/O. Actions are
"send packet X", "install cookie", "clear cookie", "reset generation",
"terminate".

**Events are not only packets and timers.** The session's actual lifecycle is
driven from outside the protocol, and leaving those out would make the two
hardest behaviours integration accidents rather than tested transitions:

| Event | Why it belongs here |
|---|---|
| decoded packet | the obvious half |
| timer expiry | retransmission and keepalive |
| `SessionClientEstablished(client)` | **`RunSession` is entered by an authenticated TCP setup**, not by any packet we receive. This is also what makes the `Refuse`-versus-setup serialisation testable as a transition rather than hoped for at integration time. |
| `SessionClientDisconnected(client)` | what ends the session and triggers the reset |

And the rule that makes the second one correct: **only the recorded session
client ends the session.** Xorg records `sessionSocket` and
`XdmcpCloseDisplay` returns immediately unless `sessionSocket == sock` *and*
the state is `RUN_SESSION` or `AWAIT_ALIVE_RESPONSE` (`xdmcp.c:642`). So the
state machine records which client is the session's, and any *other* client
disconnecting is not a session end — on a display serving several clients,
treating any disconnect as the end would reset the session under the user
whenever a transient client exits.

Encode these exactly, each of which a review round had to correct:

- **Malformed or unusable `Accept` leaves the state unchanged** —
  `AwaitRequestResponse` is retained and the retry timer drives. Not a jump to
  `StartConnection`.
- **`Refuse` acts only in `AwaitManageResponse`** (`xdmcp.c:1264`); in
  `RunSession` it is ignored, so a late refusal cannot disturb a live session.
- **A non-empty authentication *name* is fatal**; its *data* is ignored when
  the name is empty.
- **`-once` turns every renew condition into termination**, including
  retransmission exhaustion with no session ever established.

**Proof.** The transition table driven directly, no socket: every event in
every state, including the four above. This is where the protocol correctness
lives, and it costs nothing to test exhaustively.

## Step 3 — `XdmcpOptions`: the configuration source

Small and pure, but it has to come **before** the reset and socket wiring,
which both need to know whether XDMCP is enabled, in which mode, against which
manager, on which port, from which address, and whether `-once` is set.
Deferring it to the end would leave steps 4-6 with no production configuration
to read.

`-query <host>`, `-indirect <host>`, `-broadcast`, `-port <n>`, `-from <addr>`,
`-class <str>`, `-displayID <str>`, `-once`. Ordered, last-wins, mirroring
`xdmcp.c:252-312`. `-cookie` is parsed and **rejected with a clear message**,
since accepting it implies XDM-AUTHENTICATION-1.

**Proof.** A parse table: each option, the ordering/last-wins cases, mode
conflicts (`-query` then `-broadcast`), `-cookie` rejected rather than ignored,
and no XDMCP option leaving XDMCP disabled.

## Step 4 — auth integration ⚠ the security-critical step

`AuthState` gains a generation-bound session credential:
`{ generation, cookie }`, per the spec.

- Setup authentication compares against **the setup thread's own bound producer
  generation** (the `BoundSender` binding from the reset work), not the global
  current generation. A reset then invalidates by mismatch, with no clear call
  to miss.
- An explicit clear/replace on same-generation offer abandonment, which
  generation binding cannot cover.
- In XDMCP mode, TCP setup accepts the session cookie **only** — a file cookie
  must not authorize a TCP client.
- XDMCP counts as a dynamic auth source for stage 1's `-listen tcp` startup
  check, and before the first `Accept` TCP setup fails closed.

**Proof.** The two tests this whole stage exists for. First: a client
presenting the previous session's cookie after a reset is refused **by
generation mismatch** — assert the mechanism, not just the refusal, or it
passes for the wrong reason. Second, with no reset in it: `Accept`, `Refuse`,
second `Accept` with a different cookie, and a client presenting the *first*
cookie is refused. Plus: TCP before any `Accept` is refused; a file cookie does
not authorize a TCP client while XDMCP is active; an empty cookie is never
installed (`ct_eq(&[], &[])` is `true`, so an empty credential matches any
empty presentation).

## Step 5 — reset integration

- An XDMCP option implies `-reset`; `-once` implies `-terminate`.
- On a new generation the state machine returns to its init state and
  re-queries, running **after** the new generation is installed.
- Session end and `XdmcpDeadSession` raise the reset.

**Proof.** Session end produces exactly one new generation; a second session
starts on the same server. Failed negotiation also resets — the invariant is
"every established session ends at a boundary; a failed negotiation may also
restart one", so a generation with zero sessions is correct, not a bug.

## Step 6 — the socket and the timer

The first step that sends anything.

- UDP socket in the existing mio poll set with its own token, alongside the
  stage-1 listeners. Port **177** to the manager (`XDM_UDP_PORT`,
  `X11/Xdmcp.h:26`).
- Retransmission with exponential backoff, taken from the protocol header
  rather than invented: `rtx = XDM_MIN_RTX << timeOutRtx` capped at
  `XDM_MAX_RTX` — **2 s doubling to 32 s** (`Xdmcp.h:39-40`), giving up at
  `XDM_RTX_LIMIT` **7** retransmissions, or `XDM_KA_RTX_LIMIT` **4** while
  awaiting `Alive` (`:41-42`).
- The deadline joins the loop's existing per-iteration poll-timeout
  computation. **No new thread** — the state machine belongs on the core loop
  where it can see the generation boundary directly.

**Proof.** A fake manager over loopback UDP: full happy path; a manager that
never answers backs off and gives up at the limit rather than spinning or
wedging; `-once` exits on that path instead.

## Step 7 — documentation

Man page and `docs/setup.md`: the options, and plainly that XDMCP without
XDM-AUTHENTICATION-1 is unauthenticated and unencrypted, so anything that can
spoof a `Willing` can offer a session. True of Xorg in the same configuration,
which is why XDMCP deployments assume a trusted network.

## Step 8 — hardware

- `-query` against a local LightDM with XDMCP enabled: session starts, ends, a
  second session starts on the same server.
- Then across the LAN from another machine.
- `-once` exits after one session.

## Hazards

- **Step 4 is where a mistake is not recoverable.** A stale session cookie is
  cross-user access on a shared login node, which is the deployment #121 is
  for. Everything else here is a hang or a refusal.
- **`Accept` is the first untrusted input.** Anything that can answer our
  `Query` can send one, so the codec's length arithmetic and the state
  machine's rejection paths are attack surface, not just correctness.
- **Round-trip tests will pass on a wrong codec.** Vectors must come from the
  spec or from captures.
- **`-broadcast` and `-indirect` are hard to test** without a second machine
  running a manager. `-query` against a local LightDM is the tractable case;
  do not let the other two ship untested-and-unmentioned.
