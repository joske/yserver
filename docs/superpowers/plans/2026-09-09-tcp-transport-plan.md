# TCP transport — implementation plan

Implements `../specs/2026-09-09-tcp-transport-design.md`. Read that first; this
plan does not restate its reasoning, only what to build, in what order, and
what proves each part.

**Branch:** `feat/121-tcp-transport`. One branch for the whole of #121 stage 1,
per the one-branch-per-issue rule; stages 2-4 (ACL, server reset, XDMCP) get
their own branches and specs.

**Deliverable:** `yserver :7 -listen tcp -auth <file>` accepts
`xdpyinfo -display 127.0.0.1:7` with `XAUTHORITY` set, and refuses it without.
Without `-listen tcp` no TCP socket exists. Unix clients are unchanged, proven
by the existing suite passing unmodified.

## Ordering principle

**Security before capability.** The auth fix lands and is proven *before any
listener can bind*, because the failure mode of the reverse order is an
unauthenticated X server on a network port. Steps 1-2 have no TCP socket in
them at all.

Then: seam before user, gate before exposure. The `Transport` enum is
mechanical and lands with zero behaviour change (step 3), the per-client
policy lands while still Unix-only and therefore testable in isolation
(step 4), and only step 6 opens a port.

Every step keeps the invariant that a **Unix client is byte-identical to
master**. That is the whole existing user base; the new transport has no users
yet.

## Prerequisites

- `cargo +nightly fmt`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test` clean before each commit, per AGENTS.md.
- **No xts A/B.** That gate is for pixel/rendering changes; nothing here draws,
  moves geometry, or alters protocol semantics for a local client, and
  `is_local` is `true` for every connection until step 6, so the dispatch gate
  is a no-op by construction until then. The unit suite exercises the real
  paths over socket pairs and covers the gate in both directions.
- No hardware needed for any step: everything here is socket and dispatch
  work, testable in the sandbox. The integration check in step 7 wants a real
  `xdpyinfo`/`xterm`, which run locally.

---

## Step 1 — make authorization fail-closed, Unix-only semantics preserved

The spec's gating problem. No TCP anywhere in this step.

- `AuthState::check` (`auth.rs:173`) grows a transport-kind argument. Keep the
  present behaviour exactly for `Unix`: `file == None` ⇒ `Allow`;
  `local_open` (`auth.rs:151`) still true until a successful load.
- Add the `Tcp` arm: authorize **only** on a successfully loaded cookie list
  containing a match. `file == None`, an unreadable or malformed file, or an
  empty cookie list ⇒ `Reject`.
- `local_open` is renamed to say what it now means (it is genuinely
  local-only), so a future reader cannot mistake it for a global.

**Proof.** Unit tests, and the four *existing* local cases pinned **first**, as
a regression fence before the new arm exists: no file ⇒ Allow; unreadable file
⇒ Allow; loaded + correct cookie ⇒ Allow; loaded + wrong cookie ⇒ Reject with
`REASON_BAD_COOKIE`. Then five TCP cases: no file, unreadable, empty list,
correct cookie, wrong cookie. Verify the tests are red without the change —
the local four must stay green, the TCP five must fail to compile or fail
outright, and "fails to compile" is not evidence, so write them against the
new signature and check each individually.

## Step 2 — refuse `-listen tcp` when auth cannot satisfy it

Makes "TCP bound but open" unrepresentable rather than merely avoided.

- `launch.rs` learns `-listen`/`-nolisten` as an **ordered, reversible**
  per-transport list (`os/utils.c:876,885` mutates one list in argv order),
  last-wins. Default list disables `tcp`, mirroring `defaultNoListenList[]`
  (`os/utils.c:644`).
- Startup validation: TCP enabled + no usable auth file ⇒ hard error, with a
  message naming `-auth`. **One shared operation, not a second opinion on
  file validity** — e.g. `AuthState::require_tcp_auth_at_startup()`, which
  performs the initial load into the same `AuthState` the runtime path uses
  and fails unless at least one MIT cookie loaded. Startup and
  `check(Tcp, …)` must not be able to disagree: two interpretations of
  "usable" is how a server starts with a file that every subsequent TCP
  client is then refused against (or worse, the reverse).
- Port is `6000 + display` in **checked** arithmetic; display > 59535 ⇒ defined
  startup error.

**Proof.** Parse table: bare, `-nolisten tcp`, `-listen tcp`,
`-listen tcp -nolisten tcp` (disabled), `-nolisten tcp -listen tcp` (enabled),
plus the two real DM argv forms from `launch.rs:553` and the gdm form, which
must all still parse. Startup test: `-listen tcp` without `-auth` fails.
Overflow test at display 59535 (ok) and 59536 (error). Still no socket bound
anywhere in the suite.

## Step 3 — the `Transport` enum

Mechanical, wide, zero behaviour change. The risk is a silent local
regression, not a TCP bug.

- `Transport { Unix(UnixStream), Tcp(TcpStream) }` delegating `Read`, `Write`,
  `AsRawFd`, `set_nonblocking`, `try_clone`, `shutdown`, `set_read_timeout`,
  `set_write_timeout` — the full set the spec's table takes from live call
  sites. Missing one is a late compile error in a 46-site change.
- Replace the type at five of the six seams: `run.rs:1024`, `server.rs:2200`,
  `setup_thread.rs:50,61,127`, `client_reader.rs:89,114`. `FdReader` is step 5.
- `Listener { Unix(UnixListener), Tcp(TcpListener) }`, `run.rs` holding a
  `Vec<Listener>` — but still only ever containing the Unix one.
- Test helpers move to a `Transport::pair()` returning the Unix variant, so the
  ~120 `UnixStream::pair()` sites change in one shape rather than 120 ad-hoc
  edits.

**Proof.** The whole existing suite, unchanged in meaning, must pass — that is
the point of the step. Delegation unit tests for all eight methods on the Unix
variant. **No** TCP variant is constructed yet.

## Step 4 — `is_local` and `fd_passing`, and the dispatch gate

Lands while every client is still Unix, so the gate is provable in isolation
before anything can reach it remotely.

- `ClientState` gains `is_local: bool` and `fd_passing: bool`, both `true` for
  every connection today. Two flags, not one: the spec's reason.
- Dispatch gate **before** the handlers, mirroring Xorg exactly:
  - **DRI3** — every request ⇒ `BadMatch` for `!is_local`
    (`dri3/dri3_request.c:662`).
  - **MIT-SHM** — `ShmQueryVersion` allowed; everything else ⇒ `BadRequest`
    (`Xext/shm.c:1346`).
  - **XF86-VidMode** — *partially* gated, and this is the detail to get right
    rather than treating the extension as remote-forbidden
    (`Xext/vidmode.c:1655-1706`). Unconditional for any client, 12 requests:
    `QueryVersion`, `GetModeLine`, `GetMonitor`, `GetAllModeLines`,
    `ValidateModeLine`, `GetViewPort`, `GetDotClocks`, `SetClientVersion`,
    `GetGamma`, `GetGammaRamp`, `GetGammaRampSize`, `GetPermissions`. The
    mutating remainder ⇒ **`VidModeErrorBase + XF86VidModeClientNotLocal`**,
    an extension-relative error, *not* `BadAccess`/`BadRequest` — so our
    vidmode error base must be correct or the code is wrong in a way tests
    that only check "it failed" will miss. An unknown minor inside the gated
    branch ⇒ `BadRequest` (`:1702`).
  - `GetPermissions` must report `XF86VM_WRITE_PERMISSION` only when
    `is_local` (`Xext/vidmode.c:1614`), so a remote client is told read-only
    rather than refused.
  - **Present is NOT gated.** No `client->local` exists anywhere in Xorg's
    `present/`. Gating it would be a regression we invented.
- Extensions stay listed in `QueryExtension`/`ListExtensions` for every
  client — Xorg advertises and then refuses, and clients handle that.
- `send_with_fd` (`process_request.rs:7211`) refuses `!fd_passing` as defence
  in depth *behind* the gate.

**Proof.** Unit tests driving each gated opcode with `is_local` false and true,
asserting the **exact** error per the table above, including the vidmode split
across all 12 unconditional requests and at least three mutating ones, and
`GetPermissions`'s two reply forms. A test asserting Present is reachable with
`is_local == false`. A test that all three extensions are still advertised.
A sense inversion would show as the local-direction tests failing.

## Step 5 — `FdReader`, the ingress seam

- `FdReader` (`unix_fd.rs:109`) becomes transport-aware: the Unix variant keeps
  `recvmsg`/`SCM_RIGHTS` (`unix_fd.rs:22`), the TCP variant does an ordinary
  `read` and yields **no** descriptors — not an empty-fd Unix path, a distinct
  variant, so "TCP got an fd" cannot typecheck.

**Proof.** Unit: a Unix pair still passes an fd through; a TCP pair reads the
same bytes and returns an empty fd list. Full suite.

## Step 6 — bind the listener, with fairness

The first step in which a TCP socket exists.

- Bind on `-listen tcp` only, after step 2's validation.
- **Distinct poll token per listener** — today there is one `LISTENER_TOKEN`
  (`run.rs:1261`).
- **Bounded accept budget** per listener per wakeup, replacing
  `accept_pending`'s drain-to-`WouldBlock` (`run.rs:2954`), and round-robin
  across ready listeners.
- `is_local`/`fd_passing` set from the accepting listener's variant.
- `client_peer_pid` (`process_request.rs:13240`) gated on `is_local` rather
  than relying on `SO_PEERCRED` to fail on AF_INET.
- **`OUTBOUND_CAP` decision, made here rather than deferred: keep 4 MiB for
  both transports.** Bounded accumulation matters more than emulating Xorg's
  unbounded remote queue, and a slow TCP client being disconnected is a
  documented outcome (man page, step 7).

  What the cap actually does must be stated correctly, because it is not what
  it looks like: `buffer_or_disconnect` (`client_io.rs:79`) tests
  `!client.outbound.is_empty() && len + bytes.len() > OUTBOUND_CAP`, so when
  the queue is **empty** an arbitrarily large single write is buffered
  unconditionally. The cap bounds *accumulation across writes*, **not peak
  memory**. That is load-bearing rather than pedantic: a `GetImage` of a
  3840x2160 screen is a ~33 MB single reply, and screenshot tools are exactly
  what a remote-desktop user runs. Under the current rule that reply is
  admitted; converting the cap into a hard peak bound would break it. So keep
  the existing semantics deliberately and do not "fix" the empty-queue skip as
  part of this stage.

**Proof.** No `-listen tcp` ⇒ nothing bound (assert the listener set, and
assert connect-refused on 6000+N). Fairness: a synthetic flood on one listener
must not delay an accept on the other beyond the budget — a real test, since
this is a remotely triggerable local DoS if the budget is missing.

And an **automated loopback success test**, not just the negatives. Without it,
listener registration, accept metadata, the setup-thread transport handoff,
`is_local == false` and actual cookie acceptance have no reproducible proof —
only the manual `xdpyinfo` in step 7, which is the wrong place to discover any
of them:

- start with `-listen tcp -auth <tmpfile>`;
- connect to `127.0.0.1:6000+N`, complete setup with the cookie;
- assert the resulting client is `!is_local && !fd_passing`;
- repeat with no cookie and with a wrong cookie, asserting refusal.

On the cookie family, **measured rather than assumed** (2026-09-09): an
earlier draft of this plan asserted that a `FamilyLocal` (256) entry is not
selected for `127.0.0.1:N` and that the fixture therefore needs a
`FamilyInternet` (0) record. **That is false for loopback.** With an authority
file containing only a FamilyLocal record, `xdpyinfo -display 127.0.0.1:77`
against a capturing listener sent `MIT-MAGIC-COOKIE-1` with exactly that
cookie — xtrans converts the loopback address to FamilyLocal before the auth
lookup. So an ordinary `:N` cookie works for loopback TCP and no separate
record is needed.

It still matters for the real target: a **genuinely remote** client resolves to
`FamilyInternet`, so XDMCP deployments in stage 4 do need Internet-family
records. And since our server ignores family entirely (`auth.rs:137`), any of
this is purely about what the *client* chooses to send.

## Step 7 — end-to-end, and the documentation the security posture requires

- Integration by hand: `xdpyinfo -display 127.0.0.1:N`, `xeyes`, `xterm`
  against `-listen tcp` with `XAUTHORITY` set. Confirm `xdpyinfo` **does**
  list DRI3 and MIT-SHM (Xorg parity) while a DRI3 request returns `BadMatch`,
  and that a GL client fails at DRI3 rather than hanging.
- Wrong/absent cookie over TCP ⇒ refused with the existing reason.
- `docs/man/yserver.1.scd` and `docs/setup.md`: `-listen`/`-nolisten`, and
  plainly that `-listen tcp` has **no** network-layer access control until
  stage 2, and requires `-auth`.
- `docs/status.md` entry.

## Hazards

- **Step 1 is the one that can go wrong quietly.** It edits the function every
  existing client already traverses, and the failure direction that matters —
  accidentally making local clients stricter — breaks every desktop we
  support. Hence the local-four regression fence before the TCP arm.
- **A `!is_local` sense inversion** would refuse DRI3 to *local* clients, i.e.
  break all GL everywhere. The explicit both-directions unit tests are what
  catch it; a test that only checks the remote direction would not.
- **`OUTBOUND_CAP` is resolved in step 6** (keep 4 MiB, both transports,
  documented). The residual hazard is the empty-queue skip described there: it
  means one huge reply to a slow remote peer can hold tens of MB queued for
  that client. Acceptable for stage 1 with one exposed, cookie-gated port;
  revisit if stage 4 brings many concurrent remote sessions, which is exactly
  #121's use case.
- **One cookie authorizes any transport.** `auth.rs:137` compares `(name,
  data)` only, ignoring family, so a local cookie is a remote cookie even
  after step 1. Belongs with stage 2's ACL; must be stated in the man page in
  step 7 so the posture is not overstated.
- Do **not** let the test-helper migration in step 3 sprawl into behaviour
  changes; if a test needs different semantics, that is a separate commit.
