# TCP transport — design

## Status

Draft, unimplemented. Written 2026-09-09 against master `64d4b6e4`.

**Revised after codex review (2026-09-09), which withheld approval on four
blocking points — all verified against the source and all accepted:**

1. Authorization is **fail-open**, not "transport-agnostic and already
   correct" as the first draft claimed. This is now the gating requirement.
2. `unix_fd::FdReader` is a sixth production seam the first draft missed, and
   extension policy must be **dispatch gating, not hiding** — Xorg registers
   these extensions globally and rejects non-local clients per request. The
   first draft would also have hidden Present, which Xorg does not gate at
   all: a functional regression.
3. `Vec<Listener>` alone cannot deliver the non-starvation invariant.
4. The `Transport` API was missing methods live sites already call.

Plus, found while verifying: **`Xext/vidmode.c` gates on `client->local` too**,
so vidmode belongs in the gated set.

Stage 1 of four for [#121](https://github.com/joske/yserver/issues/121) (XDMCP
for HPC shared desktops). The chain, as scoped in the issue thread:

1. **TCP transport** ← this spec
2. ACL / host access control (required once TCP is exposed)
3. Server reset (LightDM resets the X server after logout)
4. XDMCP itself (`-query`, `-indirect`, `-broadcast`; UDP to the DM)

The protocol part of XDMCP is small; this stage is the one that touches the
core, so it is specified alone. Stages 2-4 get their own specs.

## Goal

Accept X11 client connections over TCP on port `6000 + display`, off by
default, opt-in with `-listen tcp`, with cookie authorization made **fail-closed
for TCP** as a precondition (see the warning below — this is not a follow-up,
it is the gate on the whole stage).

**Non-goals for this stage**, each deferred deliberately:

- **Host-based access control** (`xhost`, Xorg's `os/access.c`). Until stage 2
  lands, a TCP listener is protected by a mandatory MIT-MAGIC-COOKIE-1 and
  nothing else. This is why the listener is opt-in, why the fail-closed auth
  work below is in *this* stage, and why the man page must say so.
- **IPv6.** `FamilyInternet6` is a listener-address question, not a seam
  question; adding it later touches only the bind site.
- **Remote GL.** See "Adjacent gaps" — this is much larger than the rest of
  #121 combined and must not be implied to be near.
- **XDMCP.** Nothing in this stage sends or parses an XDMCP packet.

## Current behaviour on master (`64d4b6e4`)

yserver is AF_UNIX only. The production seam is **six sites** — the ~133
`UnixStream` references in the tree are dominated by test helpers
(`UnixStream::pair()`), which is what `process_request.rs:13232` already
predicts ("`ClientState` is built at 46 sites (mostly tests)").

| Site | What it holds |
|---|---|
| `core_loop/run.rs:1024`, accept at `:2962` | `listener: Option<UnixListener>` |
| `server.rs:2200` | `ClientState.writer: Arc<Mutex<UnixStream>>` |
| `core_loop/setup_thread.rs:50,61,127` | `SetupRegistry = Arc<Mutex<HashMap<ClientId, UnixStream>>>` |
| `core_loop/client_reader.rs:89,114` | the reader threads' `stream` |
| `core_loop/process_request.rs:7211` | the only fd **send** (`send_with_fd`) |
| `unix_fd.rs:109` | `FdReader` — the fd **receive** side |

`FdReader` is the seam that does not merely change type. It holds a
`UnixStream` and reads via `recvmsg(2)` with `SCM_RIGHTS` (`unix_fd.rs:22`),
so it cannot simply accept a `Transport`: the TCP path needs a reader that
performs an ordinary `read` and yields **no** attached descriptors.

### ⚠ Authorization is FAIL-OPEN. This is the gating problem.

The first draft of this spec claimed a TCP listener would be "protected by the
existing cookie authorization". **That is false**, in two independent ways:

- `AuthState::check` (`auth.rs:173`) begins
  `let Some(path) = self.file.as_deref() else { return AuthVerdict::Allow; };`
  — with **no `-auth`, every client is allowed unconditionally**.
- `AuthState::new` (`auth.rs:151`) sets `local_open = true`, and only a
  *successful* load clears it. So an **unreadable, empty or malformed** auth
  file also leaves the server open.

Both are correct today precisely because every connection is AF_UNIX and
therefore already trusted by filesystem permissions. Exposing TCP on top of
this would expose an unauthenticated X server to the network — the worst
possible outcome of this stage, and the reason it must be fixed *before* a
listener can be bound, not after.

Requirement: authorization becomes transport-aware and fails closed.

- A `Tcp` client is authorized **only** by a successfully loaded
  MIT-MAGIC-COOKIE-1 that it presents correctly. No cookie file, an unloadable
  file, or an empty cookie list ⇒ refuse the connection.
- `local_open` keeps its present meaning for `Unix` clients only, so local
  behaviour is unchanged and Xorg's `ShouldLoadAuth` semantics are preserved
  where they apply.
- Structural, not conditional: `-listen tcp` must be **rejected at startup**
  when the auth configuration cannot authorize a TCP client, so that "TCP
  bound but open" is unrepresentable rather than merely unlikely.

Two things do already work and need no change:

- **Write backpressure is real.** `core_loop/client_io.rs` buffers into
  `ClientState.outbound`, drains on WRITABLE via `drain_outbound`, and
  disconnects past `OUTBOUND_CAP` (4 MiB, `:30`). Partial writes are a normal
  path, not an edge case, so TCP inherits working flow control.
- **`LocalClientPID` already degrades.** `process_request.rs:13228` documents
  that an OS which cannot supply a peer pid means the X-Resource identity is
  omitted, matching `Xext/xres.c`. A TCP client takes that existing path.

`-nolisten` is currently parsed and **discarded** (`launch.rs:79`, grouped with
`-config`/`-background`), so display managers passing `-nolisten tcp` are
already tolerated — by accident rather than by contract.

`accept_pending` (`run.rs:2954`) drains its single listener until `WouldBlock`,
under one `LISTENER_TOKEN` in the poll loop (`run.rs:1261`).

## Reference: how Xorg does it

- **Default is off.** `os/utils.c:644` `defaultNoListenList[]` contains
  `"tcp"` unless the build defines `LISTEN_TCP`, and `:677` applies that list
  *before* argv is parsed. `-nolisten` (`:876`) and `-listen` (`:885`) then
  add to or remove from it. So on a modern distro build, `-nolisten tcp` is a
  no-op restating the default — which is why every DM passes it blindly.
- **`NoListenAll`** (`os/connection.c:122`) suppresses every listener;
  `:281` errors out when no transport is left to listen on.
- Transport abstraction lives in libxtrans (not vendored in `../xserver`), and
  `os/connection.c` drives it. We do not need that generality: two transports,
  known at compile time.
- ACL for stage 2 is `os/access.c`.

## Design

### The seam: a `Transport` enum, not a trait object

Introduce in `yserver-core`:

```rust
pub enum Transport {
    Unix(UnixStream),
    Tcp(TcpStream),
}
```

delegating per variant, and providing **every method the live sites already
call** — not a guessed minimum:

| Method | Called by |
|---|---|
| `Read`, `Write` | `client_io.rs`, `client_reader.rs` |
| `AsRawFd` | poll registration, `client_peer_pid` |
| `set_nonblocking` | `run.rs:1030` |
| `try_clone` | `setup_thread.rs:66` |
| `shutdown(Shutdown::Both)` | `setup_thread.rs:108` (`shutdown_all`) |
| `set_read_timeout` / `set_write_timeout` | `setup_thread.rs:131,132` |

All six exist on both `UnixStream` and `TcpStream` with identical signatures,
so the enum delegates cleanly; the point of the table is that omitting any one
of them is a compile error discovered late, in a mechanical 46-site change.

Similarly `Listener { Unix(UnixListener), Tcp(TcpListener) }` — but see
"Listener fairness" below: a `Vec<Listener>` is necessary and not sufficient.

An enum rather than `Box<dyn ReadWrite>` for three reasons:

1. `send_with_fd` must be able to **match exhaustively** and refuse the TCP
   variant. With a trait object the fd path would be a runtime downcast or an
   `Option<RawFd>` that silently returns `None` — the failure mode being a
   client that hangs waiting for a reply carrying an fd that never comes.
2. The write path is hot (`client_io.rs` drains per WRITABLE wakeup); no
   vtable indirection and no allocation per client.
3. `Arc<Mutex<Transport>>` stays a fixed-size field, so `ClientState` does not
   grow an indirection and the 46 construction sites change by type only.

### Two separate capabilities, and dispatch gating rather than hiding

`SCM_RIGHTS` has no TCP equivalent, but "cannot pass an fd" and "is a remote
client" are **different properties** and must not be one flag. `ClientState`
gains both, set at accept:

- `fd_passing: bool` — can this connection carry descriptors. Governs
  `send_with_fd` and the `FdReader` variant.
- `is_local: bool` — the transport-locality property Xorg calls
  `client->local`. Governs extension policy.

Xorg does **not** hide locality-restricted extensions from remote clients. It
registers them globally and rejects per request, at dispatch:

| Extension | Xorg behaviour | Reference |
|---|---|---|
| DRI3 | whole extension rejected | `dri3/dri3_request.c:662` — `if (!client->local) return BadMatch;` |
| MIT-SHM | `QueryVersion` allowed, everything else rejected | `Xext/shm.c:1346` — `if (!client->local) return BadRequest;` |
| XF86-VidMode | gates on locality | `Xext/vidmode.c` |
| **Present** | **not gated at all** | no `client->local` anywhere in `present/` |

So we mirror that:

- Keep all extensions in `QueryExtension`/`ListExtensions` for every client.
  A client that queries DRI3 and then gets `BadMatch` is the behaviour it
  already handles against a remote Xorg; a client that cannot see DRI3 at all
  is a divergence we would be inventing.
- Gate **at dispatch, before the handler**, with Xorg's exact errors: DRI3 ⇒
  `BadMatch` for every request; MIT-SHM ⇒ `BadRequest` for everything except
  `ShmQueryVersion`; vidmode ⇒ per `Xext/vidmode.c`.
- **Do not gate Present.** `PresentPixmap` against a server-side pixmap is
  useful to a remote client and works without fd passing; only the buffer-import
  paths need descriptors, and those are DRI3's. Hiding or refusing Present would
  be a functional regression from Xorg.
- `send_with_fd` (`process_request.rs:7211`) still refuses a non-fd_passing
  transport. That is defence in depth *behind* the dispatch gate, not the gate
  itself — the first draft had only this, which left TCP clients reaching the
  DRI3 and MIT-SHM handlers.

The dispatch gate is also what makes remote GL additive later: GLX is not
locality-gated in Xorg either, so when there is an indirect path to offer, it
needs no change here.

### Listener fairness

`Vec<Listener>` alone cannot deliver non-starvation. Today `accept_pending`
(`run.rs:2954`) drains one listener until `WouldBlock` beneath a single
`LISTENER_TOKEN` (`run.rs:1261`), so a TCP connection flood would hold that
loop indefinitely and never reach the unix listener — the exact opposite of
what this spec must promise, and a trivially remote-triggerable local DoS.

Required:

- A **distinct poll token per listener**, so readiness is attributed.
- A **bounded accept budget** per listener per wakeup (drain at most N, then
  yield), rather than draining to `WouldBlock`.
- Round-robin across ready listeners, so neither transport can monopolise.

### Listen options

Match Xorg's semantics exactly, since DM argv is the compatibility surface:

- Default: no TCP listener.
- `-listen tcp` — bind `0.0.0.0:6000 + display`. `display` is `u16`, so the
  port is **checked arithmetic**: a display above 59535 must produce a defined
  startup error, not a wrap.
- `-nolisten tcp` — disables TCP. It is a no-op **only** when no earlier
  `-listen tcp` appeared: Xorg processes argv in order (`os/utils.c:876,885`
  mutate one list), so the flags are **ordered and reversible** —
  `-listen tcp -nolisten tcp` ends disabled, the reverse ends enabled.
  Last-wins per transport, with a test table covering both orders.
- `-listen`/`-nolisten` with any other value: accepted and ignored with a
  warning, rather than a hard error — a DM passing `unix` must not fail to
  start the server.

### What does not change

Backpressure and `LocalClientPID`, per "Current behaviour" above — **not**
auth, which this stage must change. The
setup handshake, byte-order negotiation and BIG-REQUESTS barrier are all
transport-independent already.

## Invariants

1. With no `-listen tcp`, the process has **no** TCP socket bound. Assertable
   from the test suite by inspecting the listener set.
2. **A TCP client is never authorized without presenting a correct cookie from
   a successfully loaded auth file.** `-listen tcp` with an auth configuration
   that cannot satisfy that is a startup error, so the fail-open state is
   unrepresentable rather than merely avoided.
3. A `Tcp` client never receives a file descriptor, and its `FdReader`
   equivalent never reports one.
4. A `Tcp` client reaches no DRI3 or MIT-SHM handler (beyond
   `ShmQueryVersion`), and receives Xorg's error for each: `BadMatch` and
   `BadRequest` respectively. It *does* see both in `QueryExtension`, as
   against Xorg.
5. Present is reachable and functional for a `Tcp` client on the paths that do
   not require an imported buffer.
6. A `Unix` client's behaviour is byte-identical to master — the enum and both
   new capability flags are transparent on that path.
7. Neither listener can starve the other: bounded accept budget, distinct
   tokens, round-robin.

## Risks

- **`OUTBOUND_CAP` is sized for a local socket.** 4 MiB against a LAN peer is
  ample; against a slow or distant peer a burst of events could exceed it and
  we would *disconnect* a client where Xorg would keep buffering. Needs a
  decision: raise the cap for TCP, or make it transport-dependent. Do not
  discover this from a bug report.
- **No ACL in this stage.** Mandatory-cookie protection on an exposed port,
  and nothing at the network layer. The man page and `docs/setup.md` must say
  so plainly until stage 2. This is Xorg's position too, but Xorg has had
  decades of deployment scrutiny that our first TCP listener has not.
- **The auth change touches the local path.** Making `check` transport-aware
  edits the one function every existing client already goes through, so the
  regression risk lands on Unix clients rather than on the new code. It wants
  its own tests for the four existing local cases (no file, unreadable file,
  loaded file + right cookie, loaded file + wrong cookie) before the TCP arm
  is added.
- **`SO_PEERCRED` on a TCP socket.** `client_peer_pid`
  (`process_request.rs:13240`) must return `None` rather than garbage; it is
  `#[cfg(target_os = "linux")]` and reads a socket option that does not apply
  to AF_INET. Gate it on `is_local` rather than relying on the syscall to fail.
- **Test surface.** 46 `ClientState` construction sites and ~120 test
  `UnixStream::pair()` uses change type. Mechanical, but it is where a
  careless `unwrap` on the wrong variant would hide.

## Verification

- **Auth, first and separately.** Unit tests pinning all four existing local
  outcomes unchanged, then the TCP arm: no file ⇒ refuse; unreadable ⇒ refuse;
  empty cookie list ⇒ refuse; correct cookie ⇒ allow; wrong cookie ⇒ refuse
  with the existing reason. Plus a startup test that `-listen tcp` without a
  usable auth file is a hard error.
- Unit: enum delegation for all six methods; `send_with_fd` refuses `Tcp`;
  dispatch gate returns `BadMatch` for DRI3 and `BadRequest` for MIT-SHM
  (and allows `ShmQueryVersion`) for a `Tcp` client and neither for `Unix`;
  Present is **not** gated; `-listen`/`-nolisten` ordering table both ways
  including the DM argv forms; port arithmetic at display 59535 and 59536.
- Fairness: a synthetic accept flood on one listener must not delay an accept
  on the other beyond the budget.
- Integration: `xdpyinfo -display 127.0.0.1:N`, `xeyes`, `xterm` against
  `-listen tcp` with `XAUTHORITY` set. Confirm `xdpyinfo` **does** list DRI3
  and MIT-SHM (matching Xorg) while a DRI3 request returns `BadMatch`, and
  that a GL client fails at DRI3 rather than hanging.
- Negative: no `-listen tcp` ⇒ connection refused on 6000+N; wrong cookie over
  TCP ⇒ refused; no `-auth` + `-listen tcp` ⇒ server refuses to start.
- Regression: full unit suite, plus the xts gate per
  `feedback_xts_ab_gates_pixel_changes` — Xlib4 + Xlib9 A/B, zero PASS→FAIL.
  xts runs over the unix socket, so that is the "did the seam or the auth
  change break the local path" check, which is the main regression risk.

## Adjacent gaps found while reading, not in scope

- **No indirect GLX at all.** Zero `GLXRender`/`GLXRenderLarge` sites; GLX is
  direct-only on DRI3/dma-buf. So GL over a TCP connection is not a follow-up
  patch, it is the GL wire protocol from scratch — larger than stages 1-4
  combined. Xorg has the same constraint (remote GL is indirect there too), so
  this is not a yserver regression, but #121's "drop-in replacement for Xorg"
  holds for 2D and not for GPU visualisation. Worth asking the reporter which
  their sessions actually are.
- **Cookie matching ignores family/address/display.** `auth.rs:137` compares
  `(name, data)` only, where Xorg selects by family and address. Even with the
  fail-closed rule above, one cookie in the file therefore authorizes any
  transport, so a local cookie is a remote cookie. Tightening this to
  family-aware matching belongs with stage 2's ACL work; noted here because it
  is the difference between "TCP requires a cookie" and "TCP requires a cookie
  issued for TCP".
- **`-nolisten` is discarded rather than understood** (`launch.rs:79`). Fixed
  by this stage as a side effect; noting it because the same line also
  swallows `-config` and `-background`.
