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
  issue asks for. No new code required to reach first light once items
  1–2 land.
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

## Components

### 1. New `launch` module — `crates/yserver/src/launch.rs`

A pure, unit-testable argv parser:

```rust
pub struct LaunchOptions {
    pub display: Option<u16>,       // `:N` or bare `N` → explicit; None → auto-pick
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
| unknown `-flag` / stray token | **warn + skip, not fatal** |

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

| `display` | `displayfd` | Effective display |
|-----------|-------------|-------------------|
| `Some(n)` | any | `n` (explicit `:N`/bare `N` always wins) |
| `None` | `Some(_)` | **auto-pick** lowest free in `0..256` (the DM path) |
| `None` | `None` | `DEFAULT_DISPLAY` (7) — back-compat for bare/legacy invocation |

`DEFAULT_DISPLAY` stays at 7 (the existing convention that avoids clashing
with a real Xorg on `:0`); the bare-invocation behavior is unchanged.

Binding per case:

- **Explicit display** and **back-compat default**: keep the current
  "remove stale socket, then bind" behavior.
- **Auto-pick**: scan `/tmp/.X11-unix/Xk` for `k` in `0..256` and bind the
  lowest free one. For an existing socket file, `connect()` first to
  disambiguate:
  - connect refused (`ECONNREFUSED`) ⇒ stale ⇒ remove + bind it;
  - connect succeeds ⇒ a live server ⇒ try the next `k`.
  - Exhausting `0..256` is a hard error.

Factored as a function taking the socket-directory path so it can be unit
tested against a tempdir. Cap of 256 chosen as "plenty" (real X allows
far more; 256 covers any realistic seat count).

### 3. Readiness signaling (in `run()`)

Fired immediately after the socket is bound and chmod'd
(around `lib.rs:260`) — the earliest point at which connections are
accepted (the listen backlog queues lightdm's `connect()` until the core
loop accepts). Both mechanisms run if configured:

- **`-displayfd`**: write `"<N>\n"` (ASCII, Xorg format) to the fd, then
  close it, where `N` is the **effective** display resolved by component 2
  (the auto-picked number in the DM path). Factored into
  `write_displayfd(fd, display)` so it can be unit-tested through a pipe.
- **SIGUSR1-to-parent**: **very early in startup, *before* SIGUSR1 is
  masked for the signalfd**, query SIGUSR1's inherited disposition. If it
  is `SIG_IGN` (the DM started us that way — the classic X convention),
  record a flag and `kill(getppid(), SIGUSR1)` once the socket is ready.
  Order is load-bearing: the existing signalfd setup masks SIGUSR1, which
  would overwrite the inherited disposition if read too late. Inbound
  SIGUSR1=dump-scanout is unaffected — that is the receive side; this is
  the send side, fired exactly once at readiness.

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

## Testing

- **`parse_args` unit tests:** each token; lightdm's real argv string
  (e.g. `:0 vt1 -seat seat0 -auth /var/run/lightdm/root/:0 -nolisten tcp
  -displayfd 12`); bare-number back-compat (`7`); unknown-flag tolerance;
  arg-consuming flags; malformed-explicit errors; missing-value errors.
- **Display auto-pick:** tempdir-based test of the scan / stale-socket /
  live-socket logic (no real server needed — create dummy socket files and
  a live listener).
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

- Session cycling (item 5) is out of scope, but the stale-socket handling
  in auto-pick partly helps lightdm's kill-and-restart loop (a leftover
  socket from a dead generation is reclaimed). Leaked DRM/input state
  across generations remains a known follow-up.
- `-auth` is parsed and stashed but unused in this chunk; item 4 will read
  the Xauthority file from `LaunchOptions::auth_file`.
