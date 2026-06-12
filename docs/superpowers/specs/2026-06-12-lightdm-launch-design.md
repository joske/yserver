# Launch yserver from a display manager (lightdm) — argv + readiness

**Issue:** #6 (Support being launched by a display manager, lightdm first)
**Scope:** Items 1 (X-style argv handling) and 2 (readiness handshake).
**Date:** 2026-06-12
**Branch:** `feat/lightdm-launch`

## Goal

Make lightdm able to start yserver as its X server. lightdm exec's
`xserver-command` and appends X-style argv; yserver must parse that argv
and perform the readiness handshake lightdm waits on before launching the
greeter. The greeter and session are ordinary X clients — nothing new is
needed for them.

## Scope boundary

- **In scope:** X-server-style argv parsing; display auto-selection;
  `-displayfd` and SIGUSR1-to-parent readiness signaling.
- **Unblocked for free:** Item 3 (first light). yserver has no
  authentication today — it already accepts all local clients, which is
  exactly the "initially accept unauthenticated local clients" path the
  issue asks for. Crucially, lightdm *always* connects *with* a cookie, and
  yserver's setup reader already **tolerates** a presented cookie without
  rejecting it (verified — see Key facts). No new code required to reach
  first light once items 1–2 land.
- **Deferred (follow-ups):** Item 4 (MIT-MAGIC-COOKIE-1 from the `-auth`
  Xauthority file) and Item 5 (session-cycling teardown hardening — no
  leaked DRM/input state across server generations).

## Key facts grounding the design

- `crates/yserver/src/bin/yserver.rs` currently parses only a bare
  positional display number (`parse_display`, default 7) and **hard-errors
  on any non-numeric argument**.
- `crates/yserver/src/lib.rs::run(display: u16)` (line 48) binds
  `/tmp/.X11-unix/X{display}` (lines 228–260). Display *selection* lives
  in the `Justfile` `startx`/`xts-yserver-hw` shell loops, not in yserver.
- `run()` has exactly one caller: `yserver.rs:28`. `ynest` uses a separate
  entry point. The signature change is fully contained.
- Inbound signals (`lib.rs:306–348`, `511–518`): SIGUSR1 → scanout dump,
  SIGUSR2 → drawable dump, SIGINT/SIGTERM → shutdown. SIGUSR1/2 are masked
  for a signalfd.
- `libseat::Seat::open<C>(callback)` (libseat 0.2.4) takes **no seat
  name**. The builtin logind backend always opens the seat of the current
  logind session (which lightdm sets up). `Seat::open()`
  (`seat/mod.rs:143`) already blocks waiting for libseat's initial
  `Enable` (= logind activated the session's VT). Therefore yserver
  neither chooses the seat nor switches the VT in libseat mode — `-seat`
  and `vtN` are informational only.
- yserver's connection-setup reader (`read_setup_request`,
  `yserver-protocol/src/x11/mod.rs:585-601`) reads the client-presented
  `auth_protocol_name` + `auth_protocol_data` and **ignores** them; nothing
  downstream rejects on auth (`write_setup_failed` fires only on
  byte-order / version mismatch). So a client connecting *with* a
  `MIT-MAGIC-COOKIE-1` cookie is accepted as-is — see "first light" below.

## Launch-protocol facts (verified against Xorg `../xserver` + lightdm)

These were checked against the upstream X.Org tree and lightdm source, and
they reshape the design — read before the components:

- **lightdm's default local path does NOT use `-displayfd`.** lightdm
  picks the display number itself, appends `:N`, and **waits for the
  SIGUSR1 ready signal** (`src/x-server-local.c`
  `x_server_local_start()` / `got_signal_cb()`, signal routing in
  `src/process.c`). So for lightdm the critical path is **explicit `:N` +
  SIGUSR1-to-parent**, not auto-pick. `-displayfd` is a gdm-style /
  opt-in path we still support, but it is not how stock lightdm drives us.
- **Real default lightdm argv** is roughly:
  `:0 -seat seat0 -auth /var/run/lightdm/root/:0 -nolisten tcp vt7 -novtswitch`
  (plus optional `-config`, `-layout`, `-background`, user extras). Note
  `-novtswitch` and the absence of `-displayfd`.
- **Xorg's SIGUSR1/displayfd mechanism** (`os/connection.c`
  `NotifyParentProcess()`, ~line 190): writes the display number then
  `"\n"`, closes the displayfd, then sends `SIGUSR1` to the **captured
  parent PID** (`ParentProcess = getppid()` from `InitParentProcess()`,
  `os/connection.c:175`). It is invoked from `dix/main.c` *after*
  `CreateConnectionBlock()` and before `Dispatch()`. Dynamic display
  selection runs only when `displayfd >= 0 && !explicit_display`
  (`os/connection.c:249`).
- **Xorg lockfiles.** Xorg locks every *explicit* display with
  `/tmp/.X<N>-lock` before creating the socket (`os/osinit.c:313`,
  `os/utils.c:258`). It sets `nolock = TRUE` **only** for the `-displayfd`
  dynamic-selection case (`os/utils.c:764`). lightdm's own
  `display_number_in_use()` checks `/tmp/.X<N>-lock`, **not** the socket
  path — so a server that creates only the socket can be misclassified as
  "display free" by lightdm and other launchers.
- **lightdm always passes `-auth` and connects *with* auth**
  (`seat-local.c` → `x_server_set_local_authority()`; `x-server.c` uses
  `xcb_connect_to_display_with_auth_info()`). First light works only
  because yserver *tolerates* a presented cookie (verified fact above) —
  enforcing it is the deferred item 4.

## Components

### 1. New `launch` module — `crates/yserver/src/launch.rs`

A pure, unit-testable argv parser:

```rust
pub struct LaunchOptions {
    pub display: Option<u16>,       // `:N` or bare `N` → explicit; None → resolved in run() (component 2)
    pub displayfd: Option<RawFd>,   // `-displayfd N`
    pub vt: Option<u32>,            // `vtN` — parsed, logged, otherwise ignored
    pub seat: Option<String>,       // `-seat NAME` — parsed, logged, ignored
    pub auth_file: Option<PathBuf>, // `-auth FILE` — parsed + stashed for item 4; unused now
}

pub fn parse_args(args: impl IntoIterator<Item = String>)
    -> Result<LaunchOptions, String>;
```

Token handling:

| Token | Action |
|-------|--------|
| `:N` | `display = Some(N)` |
| bare `N` (integer, no colon) | `display = Some(N)` — keeps `Justfile` recipes (`yserver 7`) working |
| `vtN` | `vt = Some(N)` — logged, otherwise ignored (logind owns the VT) |
| `-seat NAME` | `seat = Some(NAME)` — consumes next arg; logged, otherwise ignored |
| `-auth FILE` | `auth_file = Some(FILE)` — consumes next arg; stashed for item 4 |
| `-displayfd N` | `displayfd = Some(N)` — consumes next arg |
| `-nolisten PROTO` | consumes next arg; no-op (yserver never listens on TCP) |
| `-novtswitch` | known no-op (lightdm passes it; no arg) |
| `-background none` | known no-op (consumes `none`/value arg) |
| `-config FILE` / `-layout NAME` | known no-op; consume next arg |
| unknown `-flag` / stray token | **warn + skip, not fatal** |

The known no-op set above exists so the *default lightdm argv*
(`:0 -seat seat0 -auth … -nolisten tcp vt7 -novtswitch`) parses with no
warnings; anything beyond it still falls through to warn + skip.

Behavior change vs. today: unknown arguments are tolerated (warn + skip)
instead of being a hard error — required by the issue's "tolerate/no-op
the rest." Malformed **explicit** requests still error: `:foo`, `vtbad`,
`-displayfd notanumber`, a `-seat`/`-auth`/`-displayfd` with no following
value.

Arity note: only the known arg-consuming flags above consume a following
token. Unknown `-flags` are skipped individually; a stray value left
behind by an unknown flag is itself skipped as an unrecognized token (with
a warning). We do not attempt to infer arity for unknown flags.

### 2. Display selection (in `run()`, factored for tests)

Moves out of the `Justfile` shell loop. `parse_args` leaves `display` as
`Option<u16>`; `run()` resolves the *effective* display from the pair
(`display`, `displayfd`) using three explicit cases, so existing behavior
is preserved:

| `display` | `displayfd` | Effective display | Lockfile? |
|-----------|-------------|-------------------|-----------|
| `Some(n)` | any | `n` (explicit `:N`/bare `N` always wins — **this is the lightdm path**) | yes |
| `None` | `Some(_)` | **auto-pick** lowest free in `0..256` (gdm-style `-displayfd`, *not* stock lightdm) | no (matches Xorg `nolock`) |
| `None` | `None` | `DEFAULT_DISPLAY` (7) — back-compat for bare/legacy invocation | yes |

`DEFAULT_DISPLAY` stays at 7 (the existing convention that avoids clashing
with a real Xorg on `:0`); the bare-invocation behavior is unchanged. The
lockfile column is implemented by component 2b below; it matters because
lightdm hands us an explicit `:N` and checks `/tmp/.X<N>-lock`.

Binding per case:

- **Explicit display** and **back-compat default**: acquire the lockfile
  (component 2b), then keep the current "remove stale socket, then bind"
  behavior.
- **Auto-pick**: scan `/tmp/.X11-unix/Xk` for `k` in `0..256` and bind the
  lowest free one. For an existing socket file, `connect()` first to
  disambiguate:
  - connect refused (`ECONNREFUSED`) ⇒ stale ⇒ remove + bind it;
  - connect succeeds ⇒ a live server ⇒ try the next `k`;
  - **any other `connect()` error** (e.g. `EACCES`, `ETIMEDOUT`) ⇒ treat
    as occupied/unknown ⇒ try the next `k`, do **not** delete the socket;
  - **`bind()` returns `EADDRINUSE`** (lost a race to a concurrent
    starter between the scan and the bind) ⇒ try the next `k`.
  - Exhausting `0..256` is a hard error.
  Auto-pick deliberately does **not** create a lockfile, matching Xorg's
  `nolock = TRUE` for the `-displayfd` path.

Factored as a function taking the socket-directory path so it can be unit
tested against a tempdir. Cap of 256 chosen as "plenty" (real X allows
far more; 256 covers any realistic seat count).

### 2b. Display lockfile (`/tmp/.X<N>-lock`)

**Required for interop** — lightdm (and other launchers) test
`/tmp/.X<N>-lock`, not the socket, to decide whether a display is free.
For the explicit-`:N` and back-compat-default cases (the lightdm path), we
must implement the standard Xorg lock protocol before binding the socket:

1. Create `/tmp/.X<N>-lock` with `O_CREAT | O_EXCL`, mode `0444`, and write
   the owning PID as Xorg does (`"%10d\n"`, 11 bytes).
2. If `O_EXCL` fails with `EEXIST`: read the PID from the existing lock.
   - If `kill(pid, 0)` succeeds (process alive) ⇒ display genuinely in use
     ⇒ for explicit `:N`, hard error; (auto-pick never reaches here — it
     takes no lock).
   - If it fails with `ESRCH` (stale lock from a dead server) ⇒ remove the
     lock and retry the create once.
3. On clean shutdown, remove our own lockfile (alongside the existing
   socket cleanup at `lib.rs:387`).

Factored to take the lock-directory path (`/tmp` by default) so the
create / stale-detection / collision logic is unit-testable against a
tempdir. This is the fix for codex's interop **blocker**: a socket-only
server gets misclassified as "free" by lightdm's `display_number_in_use()`.

### 3. Readiness signaling (in `run()`)

**Timing — when to signal.** Not merely after bind/chmod. lightdm opens
an XCB connection *with auth* the instant it receives SIGUSR1, so we must
not signal before yserver can actually complete the initial X
connection-setup handshake. Xorg signals from `dix/main.c` *after*
`CreateConnectionBlock()` and before `Dispatch()`. yserver's equivalent
"ready to serve setup" point is **just before entering the core loop**
(after `ServerState` is fully constructed, the listener is bound +
chmod'd, and the lockfile is held — around `lib.rs:351`, just before
`run_core`). Signal there. (The listen backlog still queues a `connect()`
that races in slightly early, but the setup *reply* won't be attempted
until the core loop is running and able to produce a valid connection
block.) This is between the user's "at bind time" and "after first
composite/flip" — it does not wait for a painted frame.

Both mechanisms run if configured:

- **`-displayfd`**: write `"<N>\n"` (ASCII, Xorg format) to the fd, then
  close it, where `N` is the **effective** display resolved by component 2
  (the auto-picked number in the DM path). Factored into
  `write_displayfd(fd, display)` so it can be unit-tested through a pipe.
- **SIGUSR1-to-parent**: at startup, query SIGUSR1's inherited disposition
  with `sigaction(SIGUSR1, NULL, &old)`. If `old.sa_handler == SIG_IGN`
  (the DM started us that way — the classic X convention), record a flag
  and `kill(getppid(), SIGUSR1)` once ready. `getppid()` is correct:
  Xorg captures `ParentProcess = getppid()` in `InitParentProcess()` and
  signals that PID (`os/connection.c:175`).

  *Note on ordering:* blocking SIGUSR1 for the signalfd (via
  `sigprocmask`) only suppresses **delivery** — it does **not** change the
  signal's *disposition*. So reading the inherited `SIG_IGN` disposition
  via `sigaction(…, NULL, &old)` is correct regardless of when we later
  add SIGUSR1 to the signalfd mask; there is no real ordering hazard
  (an earlier draft overstated this). We only must avoid installing a real
  `sigaction` *handler* for SIGUSR1 before reading the disposition — and
  yserver never does, it uses signalfd. Inbound SIGUSR1=dump-scanout is
  unaffected — that is the receive side; this is the send side, fired
  exactly once at readiness.

### 4. `run()` signature

`run(display: u16)` → `run(opts: LaunchOptions)`. The single caller
(`yserver.rs:28`) builds `LaunchOptions` from argv via `launch::parse_args`
and passes it through. `yserver.rs` becomes a thin shim: parse argv →
build options → call `run`.

## Error handling

| Condition | Behavior |
|-----------|----------|
| Unknown argv token | warn + ignore (tolerate-the-rest) |
| Malformed explicit `:N` / `vtN` / `-displayfd N` value | hard error, usage message, exit non-zero |
| Missing value after `-seat` / `-auth` / `-displayfd` | hard error |
| `-displayfd` write/close failure | warn, continue (lightdm may time out, but don't crash) |
| Auto-pick exhausts `0..256` | hard error |
| Explicit `:N` lockfile held by a live PID | hard error (display genuinely in use) |
| Explicit `:N` lockfile stale (PID dead) | remove + retry create once |

## Testing

- **`parse_args` unit tests:** each token; **lightdm's real default argv**
  (`:0 -seat seat0 -auth /var/run/lightdm/root/:0 -nolisten tcp vt7
  -novtswitch`) parses with no warnings and yields `display = Some(0)`,
  `displayfd = None`, `auth_file = Some(...)`; a gdm-style `-displayfd`
  variant (`-displayfd 12` with no explicit `:N`) yields
  `displayfd = Some(12)`, `display = None`; bare-number back-compat (`7`);
  unknown-flag tolerance; arg-consuming flags; malformed-explicit errors;
  missing-value errors.
- **Display auto-pick:** tempdir-based test of the scan / stale-socket /
  live-socket logic plus the `EADDRINUSE` bind-race retry and the
  "non-`ECONNREFUSED` ⇒ skip, don't delete" branch (create dummy socket
  files and a live listener).
- **Lockfile protocol:** tempdir-based test of `O_EXCL` create, the
  stale-lock (`kill(pid,0)` → `ESRCH`) remove-and-retry path, and the
  live-lock collision (hard error for explicit `:N`).
- **`write_displayfd`:** unit-test via a `pipe()` — write to the write
  end, assert `"<N>\n"` on the read end.
- **SIGUSR1-disposition path:** not unit-testable (process-global signal
  state) → verified by **HW smoke under real lightdm** on bee/silence.
  Per repo practice, startup/KMS-touching changes require hardware smoke
  before commit anyway.
- **Integration / first light:** point a real `lightdm.conf`
  `[Seat:*] xserver-command` at the built binary on a HW machine; confirm
  the GTK greeter appears. This is the acceptance gate for the chunk.

## Risks / notes

- Session cycling (item 5) is out of scope, but the stale-socket *and*
  stale-lockfile handling help lightdm's kill-and-restart loop (a leftover
  socket or lock from a dead generation is reclaimed via the `ESRCH`
  path). Leaked DRM/input state across generations remains a known
  follow-up.
- The lockfile must be removed on clean shutdown (extend the existing
  socket cleanup at `lib.rs:387`). A crash leaves a stale lock, but the
  `kill(pid,0)`→`ESRCH` reclaim on the next start handles that.
- `-auth` is parsed and stashed but unused in this chunk; item 4 will read
  the Xauthority file from `LaunchOptions::auth_file`.
