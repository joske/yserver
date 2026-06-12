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
| `Some(n)` | `None` | `n` (explicit `:N`/bare `N` — **this is the lightdm path**) | **yes** |
| `Some(n)` | `Some(_)` | `n` (explicit display wins; `-displayfd` still reported) | no |
| `None` | `Some(_)` | **auto-pick** lowest free in `0..256` (gdm-style `-displayfd`, *not* stock lightdm) | no |
| `None` | `None` | `DEFAULT_DISPLAY` (7) — back-compat for bare/legacy invocation | **yes** |

The lockfile rule is exactly Xorg's: **lock the display iff `-displayfd`
is absent.** Xorg sets `nolock = TRUE` whenever it parses `-displayfd`
(`os/utils.c:764`), *before* `LockServer()` runs (`dix/main.c:138`) —
i.e. presence of `-displayfd` suppresses the lock even when the display is
explicit. We match that intentionally rather than diverging. Stock lightdm
passes `:N` with no `-displayfd`, so it lands in the locked row, which is
what matters: lightdm checks `/tmp/.X<N>-lock`, not the socket.

`DEFAULT_DISPLAY` stays at 7 (the existing convention that avoids clashing
with a real Xorg on `:0`); the bare-invocation behavior is unchanged. The
lockfile column is implemented by component 2b below.

Binding per case:

A shared probe rule for both cases below — **file type first**: on Linux,
`connect(AF_UNIX)` to a *non-socket* inode fails with `ECONNREFUSED`, the
**same errno as a stale socket**, so errno alone cannot tell "dead
server's socket" from "some file named `X<n>`". Check
`symlink_metadata(..).file_type().is_socket()` before any connect-probe;
a non-socket path is **never deleted** (explicit ⇒ refuse with
`AddrInUse`; auto-pick ⇒ skip to the next `k`). This also refuses
symlinks.

The connect-probe itself must be **nonblocking**: a blocking
`connect(AF_UNIX)` to a live listener whose accept backlog is full waits
for an `accept()` — potentially forever. A wedged X server (precisely the
case where the DM restarts us), or a hostile local user's never-accepting
listener in the world-writable socket dir, must not hang the launch.
Nonblocking errno mapping: success/`EINPROGRESS` ⇒ live; **`EAGAIN` ⇒
backlog full ⇒ live** (the display *is* occupied); `ECONNREFUSED` ⇒
stale; anything else ⇒ opaque/refuse-to-touch. Implemented as one shared
`probe()` classifier returning {Free, Live, Stale, NonSocket, Opaque} so
the explicit (refuse) and auto-pick (skip) policies stay visibly separate
while the errno subtleties exist once.

- **Explicit display** and **back-compat default**: acquire the lockfile
  (component 2b, when the table says so), then probe any existing path:
  non-socket ⇒ refuse; `connect()` succeeds ⇒ a live server owns the
  display ⇒ hard error (`AddrInUse`) — never steal it (old yservers take
  no lock, so holding the lock alone doesn't prove the display free);
  `ECONNREFUSED` ⇒ stale ⇒ remove + bind; any other probe error ⇒ refuse
  conservatively rather than delete something unidentified. (Behavior
  change vs. the old inline code, which removed the socket
  unconditionally.)
- **Auto-pick**: scan `/tmp/.X11-unix/Xk` for `k` in `0..256` and bind the
  lowest free one. For an existing path:
  - not a socket (or unreadable metadata) ⇒ skip to the next `k`, do
    **not** delete;
  - connect refused (`ECONNREFUSED`) ⇒ stale ⇒ remove + bind it;
  - connect succeeds ⇒ a live server ⇒ try the next `k`;
  - **any other `connect()` error** (e.g. `EACCES`) ⇒ treat as
    occupied/unknown ⇒ try the next `k`, do **not** delete the socket;
  - **`bind()` returns `EADDRINUSE`** (lost a race to a concurrent
    starter between the scan and the bind) ⇒ try the next `k`.
  - Exhausting `0..256` is a hard error.
  Auto-pick deliberately does **not** create a lockfile, matching Xorg's
  `nolock = TRUE` for the `-displayfd` path.

Factored as a function taking the socket-directory path so it can be unit
tested against a tempdir. The `0..256` cap is an **intentional bound** —
narrower than Xorg's dynamic `-displayfd` scan (`os/connection.c:249`),
which ranges much higher — chosen because 256 covers any realistic seat
count; documented here so it isn't mistaken for parity with Xorg.

### 2b. Display lockfile (`/tmp/.X<N>-lock`)

**Required for interop** — lightdm (and other launchers) test
`/tmp/.X<N>-lock`, not the socket, to decide whether a display is free.
For the locked cases (`-displayfd` absent — see the table above), we
implement Xorg's lock protocol (`os/utils.c:258-380`), with minor
cleanups, **before** binding the socket. Create is done via a temp file + atomic `link()`, not
a direct `O_EXCL`, so a partially-written or empty final lock is never
observable by a concurrent launcher:

1. Write PID `"%10d\n"` (11 bytes) into a **PID-suffixed** temp file
   `/tmp/.tX<N>-lock.<pid>`, `chmod 0444`, then `link()` it to
   `/tmp/.X<N>-lock` and `unlink` the temp file. A successful `link()` is
   the atomic "we won" signal. (Deviation from Xorg's fixed `.tX<N>-lock`
   temp name: with a fixed name two concurrent starters can clobber each
   other's temp before the `link()`, letting the winner publish a lock
   holding the *loser's* PID; the PID suffix makes each starter's temp
   unique. A crash can orphan one 11-byte temp file — harmless.)
2. If `link()` fails with `EEXIST`, inspect the existing lock — opened
   `O_RDONLY | O_NOFOLLOW` (refuse to follow a symlink):
   - **Malformed contents** (not exactly 11 bytes ending in `\n`, or a
     non-positive/non-numeric PID field) ⇒ treat as bogus ⇒ remove and
     retry the link once. A genuine *read error* propagates instead —
     never delete a lock that couldn't actually be inspected.
   - Parse the PID and `kill(pid, 0)`:
     - success **or `EPERM`** ⇒ process alive (a live server owned by
       another user still counts) ⇒ display in use ⇒ for explicit `:N`,
       hard error.
     - `ESRCH` ⇒ stale lock from a dead server ⇒ remove and retry once.
   Stale/bogus removal is **identity-bracketed**: record the lock's
   `(dev, ino)` before inspecting and unlink only if the path still names
   that inode — this narrows the race where a concurrent starter reclaims
   the same stale lock and publishes its own fresh one between our inspect
   and our unlink. Xorg has the full-width version of this race
   (`LockServer`: read → `kill(pid,0)` → `unlink`, no identity check);
   Linux has no unlink-by-fd, so a tiny lstat→unlink window remains —
   acceptable, since same-display concurrent starts are DM-serialized in
   practice.
3. **Error-path release:** if anything after lock acquisition fails
   (socket bind/listen/chmod), remove our lockfile before returning the
   error — never leave a lock we don't back with a live socket.
4. **Clean shutdown ordering:** remove the **socket first, then the lock
   last** (extend the cleanup at `lib.rs:387`). The lock is the
   authoritative occupancy marker, so dropping it before the socket would
   open a brief false-"free" window for a racing launcher.

Auto-pick (and any `-displayfd` case) takes **no** lock, matching Xorg's
`nolock = TRUE`.

Factored to take the lock-directory path (`/tmp` by default) so the
create / temp+link / stale-detection / malformed / collision logic is
unit-testable against a tempdir. This is the fix for codex's interop
**blocker**: a socket-only server gets misclassified as "free" by
lightdm's `display_number_in_use()`.

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
  (the DM started us that way — the classic X convention), record a flag,
  and **also capture the parent PID once at startup** (`getppid()`, stored
  as `Option<i32>` — `None` if already orphaned). Once ready,
  `kill(parent_pid, SIGUSR1)`. Capturing the PID at startup rather than at
  readiness is deliberate and matches Xorg: it stores
  `ParentProcess = getppid()` in `InitParentProcess()` (`os/connection.c`)
  and signals that captured PID in `NotifyParentProcess()` — reading
  `getppid()` at readiness instead would, if the DM died during yserver's
  init and we were reparented, point at a subreaper or PID 1 and either
  misfire or silently skip the signal.

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
  live-socket logic and the "non-socket ⇒ skip, don't delete" branch (a
  regular file named `Xk` exercises the file-type check). The `EADDRINUSE`
  bind-race `continue` branch cannot be hit deterministically in a unit
  test (it needs a socket to appear between the existence check and the
  `bind()`) — verified by inspection.
- **Explicit bind:** live socket refused (`AddrInUse`), stale socket
  reclaimed, non-socket path refused without deletion, chmod 0777 applied.
- **Lockfile protocol:** tempdir-based test of the temp-file + atomic
  `link()` create; the stale-lock (`kill(pid,0)` → `ESRCH`)
  remove-and-retry path; the live-lock collision (hard error for explicit
  `:N`); malformed (non-11-byte / short-numeric / non-numeric) lock
  contents treated as bogus + reclaimed; and the error-path release (lock
  removed when a bind failure follows acquisition — `?` drops the guard).
  Cleanup ordering (socket-then-lock) follows from code structure (the
  guard is dropped at end-of-`run()`, after the socket `remove_file`) and
  is observed in the HW smoke's restart-cycle check.
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
