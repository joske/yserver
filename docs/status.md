# Status — Rendering model v2

Working doc for the rendering-model-v2 program. The spec is at
`docs/superpowers/specs/2026-05-15-rendering-model-v2.md`; this
file tracks execution against it.

Earlier program docs are archived:

- `status-archive-2026-05-21.md` — Stage 4 close diagnosis chain
  on `cow-authoritative-mode` (Phase 1+2 plan + correctness
  fix-chain narrative + 4d.8 reverted pragmatic-floor attempt +
  4d open-investigation items that closed by the cow-authoritative
  branch).
- `status-archive-2026-05-15.md` — the v1 rendering re-architecture
  (Phases 3A–3F-2, sync rework, pixmap pool, GPU traps, the paused
  timeline-semaphore migration, the abandoned convolution filter +
  Manual-redirect work). Re-read it for context on what's already
  in tree, what was tried and reverted, and what was deliberately
  paused.
- `status-archive-2026-05-13.md` — pre-rework history (Phases 1–6
  + host-X11 era).

Cross-cutting bugs and followups that don't fit a stage live in
[`known-issues.md`](known-issues.md).

---

## Where we are

- Baseline branch: `graphics-followups` (working — xfce + mate
  verified 2026-05-15). HEAD includes five cherry-picks from the
  abandoned `render-convolution-filter` branch: QueryFilters
  standard list, NameWindowPixmap diagnosis docs, Justfile xtrace
  `rm`, picom validation harness.
- Active dev branch: `master`. All v2 work (Stages 1–4 + Stage 5
  Task 3 + Task 4 layer 1) has been fast-forwarded from
  `rendering-model-v2` → `cow-authoritative-mode` → `perf` into
  `master`. **v1 retired 2026-05-26** after Phase B.3 closed
  across the hardware matrix (bee/yoga/silence/air/M2-Asahi/nvidia). The
  v1 `KmsBackend` struct + all its impl blocks, the `KmsBackendKind`
  dispatcher, the `YSERVER_RENDER_MODEL` env knob, and the
  4 stub modules (compositor / render / event / fonts) were
  deleted: 14,596 lines removed across 14 files. What remains in
  `kms/backend.rs` is ~650 lines of shared helpers (rasterisers,
  wire-byte readers, `ClipMaskCache`, `OutputLayout`, `platform_init`,
  `parse_add_glyphs`) still consumed by v2. The `kms/scheduler/`
  subdir survives the deletion because v2 still uses
  `damage::OutputDamageState` (as a dead-stored field on
  `OutputLayout`) and `paint_batch::BatchResource` (the v2 frame
  builder's retire-pin trait); a follow-up cleanup can fold those
  into v2 proper.
- Abandoned branch: `render-convolution-filter`. Left untouched
  as historical reference for T1-T4 of the Manual-redirect work,
  convolution Phase 1+2, the rotate fix, and the
  parallel-implementation lessons. Don't ship anything from there.
- **Stage 4 closed 2026-05-21** on `cow-authoritative-mode`
  (since merged into `master` via the perf-branch FF).
  Two-phase plan
  (`docs/superpowers/plans/2026-05-20-cow-authoritative-mode.md`)
  + a long correctness fix chain. Phase 1 (`19ed354`) gates
  `build_scene` on COW registration: when a compositor has
  registered to paint COW, scene emit = `root + COW + cursor` only.
  Phase 2 (`1065c50` + supporting commits) mirrors Xorg's
  `compRedirectOneSubwindow` / `compUnredirectOneSubwindow` to
  reconcile redirect status on `ReparentWindow`. The fix chain
  (~35 commits) closed the compositor-update bug class: Present
  Copy `x_off/y_off` direction matching Xorg
  `present_copy_region()`, DAMAGE Subtract canonicalization +
  remaining-region re-report (matching `ProcDamageSubtract`),
  `ConfigureNotify.above_sibling` direction (lower-neighbor
  `pSib`), `XDamageCreate(viewable_window)` immediate seed (matching
  `DamageExtRegister`), ConfigureWindow stack-only no-damage,
  `MapWindow` idempotency on already-mapped (Xorg
  `dix/window.c:2661`), `MapWindow` damage seed for descendants
  transitioning Unviewable→Viewable, ClipByChildren exemption for
  manually-redirected children, reparent-time redirect
  reconciliation, rotate-redirected-backing release-then-allocate
  OLD-storage retention across release→copy, MapNotify-before-
  DamageNotify ordering on the wire, Marco compositor visibility
  restoration under MATE. Plus an input-stack chain that finally
  made GTK3 popup menus work end-to-end across MATE+marco /
  XFCE+xfwm4: XI2 grab opcodes 51-55 wired into core grab state
  (were no-op stubs), synthesized grab-activation crossings
  (NotifyGrab + FocusIn/Out NotifyNonlinear pairs), natural
  Enter/Leave under active grabs no longer suppressed,
  `owner_events=true` honoured. Cursor stack: XIChangeCursor
  routed to backend `define_cursor` (GTK4 I-beam), CWA cursor=None
  propagates clear (marco resize-cursor reset), v2
  `window_under_cursor` descends into sub-window tree (xfwm4
  resize-edge sub-windows), XFixes SetCursorName/GetCursorName
  round-trip, hardware/software cursor split with
  `YSERVER_V2_HW_CURSOR=1` opt-in. RANDR `Set{Screen,Crtc}Config`
  now validates against `state.randr.modes`.
- Full narrative of the diagnosis chain that drove the Stage 4
  close is archived at
  [`status-archive-2026-05-21.md`](status-archive-2026-05-21.md).
  Stage 4 sub-stage history (4a through 4d.7) still lives below
  under the now-`[x]` Stage 4 sections.
- **Next live work**: Stage 5 — make v2 fast. Plan at
  `docs/superpowers/plans/2026-05-20-stage-5-make-v2-fast.md`.
  Phase A closed 2026-05-24; Phase B sub-phases B.1, B.2, B.3
  all CLOSED by 2026-05-26 — bee MATE drag is lag-free across
  the chain, silence dual-output passes, yoga and air (Apple M1
  / Asahi) green, M2 Asahi (ALARM, first M2 capture) green,
  nvidia bootstraps. Three correctness fixes
  during B.3 closure: trap-emit α-loss on depth-32 backings
  (`b0b57f8`), SHAPE Input region not mirrored to backend
  (`ae480a2` — fixed adapta-nokto menu hover + click), graceful
  drop of X11 requests from already-disconnected clients
  (`932c34e`). `feature/frame-builder-submit-rate` HEAD is at
  `0c08568`; merged into master 2026-05-26. Next phase is B.4
  / B.5 work to retire SubmitGroup entirely and fold scene
  compose into the frame builder.
- **2026-06-08 COW architectural reset**: the active Cinnamon blocker
  is now treated as a structural COW/model bug, not another
  scene-assembly edge case. Replacement design doc:
  `docs/superpowers/specs/2026-06-08-cow-structural-design.md`.
  Direction: make the Composite Overlay Window a real backend/window
  node, remove the missing-parent reparent fallback, and make Manual
  redirects suppress direct scanout unconditionally. Further
  `cow_authoritative` surgery is no longer the primary path.
- **2026-06-12 XINERAMA landed in-tree**: yserver now advertises the
  `XINERAMA` extension and serves all six PanoramiX/Xinerama requests
  from the same shared monitor list as RANDR `GetMonitors`. That keeps
  the Xinerama screen count and the RANDR monitor count identical by
  construction, which is the compatibility invariant needed for
  dual-head mutter/muffin. Wire encoder unit tests are in
  `yserver-protocol`; core wiring/build validation is green. Hardware
  Cinnamon smoke on real dual-head is still the remaining proof gate.
- **2026-06-13 root-window GetImage screenshot support**: the root
  `GetImage` hang is fixed end-to-end for the generic screenshot path.
  The outbound cap now allows a single large reply to buffer when the
  queue is empty, disconnect teardown actively `shutdown(Both)`s the
  socket so peers see EOF, `GetImage` reply headers are patched in the
  client byte order, and the v2 backend routes root-window `GetImage`
  through on-screen scanout readback instead of root storage. The new
  root screenshot path is covered by unit tests and a backend
  compile-time smoke; live `xwd -root` desktop smoke still needs a real
  KMS session.
- **2026-06-12 HW text-cursor offset diagnosed/fixed**: `silence` was
  selecting text above the visible I-beam only with
  `YSERVER_V2_HW_CURSOR=1`; `eiger`/Asahi looked correct because the
  driver disables the HW cursor path and falls back to SW composition.
  Root cause: the steady-state `cursor_plane_move` fast path advanced
  the DRM cursor plane in root/CRTC coordinates without subtracting the
  active cursor hotspot, while the SW path and HW bind/rearm paths did.
  Fixed by threading hotspot-aware root→CRTC translation through show,
  move, rebind, and pending-move retry so the logical pointer and
  visible cursor stay aligned.
- **Bugfixing kicked off 2026-05-26 PM** with Cinnamon validation.
  First live issue: clicks inside `cinnamon-settings` were not
  taking effect even though XI2 ButtonPress/Release reached the
  app. Final root cause was the XI2 passive-grab path as a whole:
  `XIPassiveGrabDevice` requests were being recorded as async
  regardless of the requested `grab_mode`, so muffin's sync
  click-to-focus grabs never froze the ButtonPress. yserver then
  delivered the XI2 press to `cinnamon-settings` before focus
  activation, `XIAllowEvents(mode=ReplayDevice)` had nothing
  meaningful to replay, and GTK never treated the click as an
  activation inside the now-focused window. Fixed 2026-05-26 PM by
  (1) preserving the requested XI2 passive-grab modes, (2)
  withholding the initial XI2 ButtonPress from non-grab-owner
  clients when the passive grab is synchronous, and (3) replaying
  the XI2 press to the natural target on `XIAllowEvents(mode=2)`.
  Added a regression test that models the Cinnamon sequence
  directly. The earlier XI2 device topology cleanup (attached slave
  ids / non-XTEST naming / slave-first XI2 mask fallback) remains
  in-tree as Cinnamon/GTK compatibility hardening, but it was not
  the blocker for `cinnamon-settings` activation clicks. Rust
  validation: `cargo check` green, `cargo test -p yserver-core
  --lib` 392/392 green. A second live trace immediately after that
  showed why Cinnamon still looked broken: muffin focused the
  toplevel, then `cinnamon-settings` moved focus into its internal
  child window, and yserver encoded that transition as a generic
  top-level `FocusOut(detail=NotifyAncestor)` plus child
  `FocusIn(detail=NotifyAncestor)`. Cinnamon treated the top-level
  `FocusOut` as a real deactivation and cleared
  `_NET_ACTIVE_WINDOW`, making the window appear to lose focus under
  the pointer while being clicked. Fixed 2026-05-26 PM by switching
  `SetInputFocus` fanout to the real ancestor-chain crossing
  semantics for both core and XI2 focus events, so parent-to-child
  transitions now emit `NotifyInferior` on the parent and preserve
  the child as the actual keyboard focus. Added a regression test
  that models the Cinnamon top-level -> child focus move directly.
  Rust validation after the focus fix: `cargo check` green, `cargo
  test -p yserver-core --lib` 393/393 green. `cargo clippy
  --all-targets --all-features -- -D warnings` is still blocked by
  pre-existing `doc_lazy_continuation` warnings in
  `crates/yserver/src/kms/v2/engine.rs:118`. A working Xorg
  `x11trace` for the same `cinnamon-settings applets` repro then
  showed the remaining live click path was much closer than
  expected: ButtonPress / replayed ButtonPress / ButtonRelease hit
  the same target window on both servers, muffin sends the same
  `SetInputFocus` + `XIAllowEvents(mode=ReplayDevice)` sequence,
  and the app performs the same immediate `_NET_WM_USER_TIME`,
  `XIQueryPointer`, and `XIChangeCursor` follow-up. The clearest
  remaining protocol mismatch in the traces was `XIQueryVersion`:
  yserver was hardcoded to advertise XI 2.2 while Xorg negotiated
  up to XI 2.4 for these clients. Fixed 2026-05-26 PM by making
  `XIQueryVersion` negotiate like Xorg for a 2.4-capable server
  (return `min(requested, 2.4)`, keep `BadValue` for major < 2, and
  encode the reply in the client's byte order). Added a focused
  regression test covering 2.4 and 2.3 negotiation. Rust validation
  after the protocol fix: `cargo +nightly fmt` green, `cargo check`
  green, `cargo test -p yserver-core --lib` 394/394 green.
  Another live Cinnamon retest still failed, and the next Xorg delta
  turned out to be device-metadata bootstrap rather than click
  routing: GTK selects `XI_DeviceChangedMask` on the root window and
  Xorg immediately supplies a master-pointer `XI_DeviceChanged`
  description with labeled button/valuator classes. yserver had been
  sending no `XI_DeviceChanged` events at all and its `XIQueryDevice`
  pointer classes were still unlabeled. Fixed 2026-05-26 PM by
  enriching the virtual pointer classes with Xorg-style label atoms
  (`Button Left`, `Rel X`, `Rel Vert Scroll`, etc.) and by emitting a
  bootstrap `XI_DeviceChanged` for the master pointer when a client
  selects that mask on the root window. Added a regression test for
  the bootstrap event; Rust validation after this step: `cargo
  +nightly fmt` green, `cargo check` green, `cargo test -p
  yserver-core --lib` 395/395 green.
  `cargo clippy --all-targets --all-features -- -D warnings`
  remains blocked only by the same unrelated `engine.rs`
  doc-comment warnings. Runtime Cinnamon re-validation still
  pending against the XI 2.4 advertisement. 2026-05-27 PM
  follow-up: drawable-dump instrumentation was widened for the
  live Cinnamon menu regression. The previous dump only tracked
  recent `PresentPixmap` sources when the destination was the
  COW/overlay path, which meant a Muffin menu visible on screen
  via a fullscreen stage pixmap could be absent from the dump.
  `KmsBackendV2::note_present_pixmap` now records a general
  recent-Present ring for any destination window and resolves
  COW-targeted presents through the backend drawable map instead of
  assuming the protocol overlay XID survives unchanged on KMS.
  2026-05-27 follow-up: the overlay-registration race was closed
  by retroactively arming COW when the first `PresentPixmap → COW`
  lands before `GetOverlayWindow` finishes wiring the backend
  storage. This keeps the scene authoritative once the compositor
  is actually presenting, even if the allocation/compositor startup
  ordering flips under Cinnamon's startup timing. 2026-05-27 PM
  follow-up: the remaining keyring-dialog click regression was in
  the legacy `server.rs` passive-grab path, which was still
  redirecting owned-window clicks to the grab owner even when
  `owner_events=true`. That wrapper now mirrors the newer
  state-borrowing fanout logic, preserves passive-grab
  `owner_events` from both core `GrabButton` and XI2
  `XIPassiveGrabDevice`, and lets owned-child clicks continue
  through normal propagation while still activating the grab.
  2026-05-27 later follow-up: Cinnamon's keyring trace also issues
  `XIGrabDevice` followed by `XIAllowEvents(ReplayDevice)` on the
  pointer side. `XIAllowEvents` now releases active pointer grabs as
  well as passive ones, so the server no longer leaves the dialog in
  a stuck grabbed state after the client believes it has thawed the
  device. Added a regression test for the `owner_events=true`
  passive-grab release path on `ButtonRelease`. Follow-up: the
  `owner_events` check now keys off the grab window's descendant
  chain, not just client ownership, so descendant dialog widgets
  still receive clicks even when another client owns them. The
  legacy `server.rs` fanout now matches that descendant rule too.
2026-05-27 final follow-up: v2 now re-evaluates
`_NET_WM_WINDOW_TYPE`, `_NET_WM_STATE`, and `WM_TRANSIENT_FOR`
when window properties change and when a window re-enters
top-level status. Desktop windows are forced to the bottom of
`core.top_level_order`, while dialog/menu-like toplevels are
raised back to the top. This is the stacking fix exposed by the
sync-timing change.
2026-05-27 focus follow-up: viewable windows now also promote
keyboard focus when `WM_HINTS.input` is set, even if they do not
select key events yet. That matches the Cinnamon keyring dialog
case where the window is mapped and then advertises input later.
2026-05-27 stack-hint follow-up: the v2 property / top-level hooks
were being called with nested protocol XIDs in production, while the
stack-hint logic expects the resolved host XID. That mismatch meant
desktop/dialog restacks could silently no-op until the caller was
fixed to translate through `state.resources.window(...).host_xid`.
2026-05-27 drag follow-up: window moves under Cinnamon were broken —
dragging cinnamon-settings by its titlebar lagged, stuck after a little
movement, and on release the button read as still-held (window glued to
the cursor, move-cursor stuck). Root-caused from the bee hw trace
(`yserver-hw-cinnamon.log`): muffin drives the move via an active
`XIGrabDevice` on the master pointer, but the XI2 event fanout routed
purely by window mask-selection and ignored the active grab. Once the
drag pulled the pointer off muffin's grab window, `XI_Motion` /
`XI_ButtonRelease` went to whatever client selected XI2 masks on the
window under the cursor (nemo-desktop) instead of the grab owner — so
muffin never saw the release and never ended its move loop. The log shows
the release delivered to `xi2_targets=[37]` (nemo) while client 27
(muffin) held the grab. Fixed by mirroring the core Step-2 active-grab
redirect into the XI2 device-event path (`pointer_fanout.rs`): grabbed
device events funnel to the grab owner, reported against the grab window,
honoring `owner_events` (so GTK menu hover-tracking still flows naturally
inside the grab-window subtree); raw events and crossings untouched. Same
redirect covers motion, so the lag/stuck and the stuck-button are one fix.
Added regression test
`xi_active_device_grab_funnels_button_release_to_grab_owner_not_window_under_cursor`.
Validated on bee hardware (drag now works). `cargo test -p yserver-core
--lib` 410/410 green; nightly fmt + plain clippy clean.
2026-05-27 first Cinnamon telemetry run (bee, release build,
`RUST_LOG=warn YSERVER_LOOP_TELEMETRY=1`, ~30 s while dragging a window
fast across the whole screen): clean across every correctness gate.
`vk_queue_wait_idle/s=0`, `missed_pageflips/s=0`,
`full_redraw_fallback/s=0` (steady state; 2 at cold boot),
`submit_group_aborts/s=0`, frame-builder `aborts=0` every window — no
EBUSY / atomic-commit-failed / BO-invalidated / panic (only WARNs are
the Ctrl-Alt-Backspace zap shutdown). Throughput: `composite_submits/s`
≈ 55–61 with `frame_present_count/s` matching 1:1 → vsync-locked
~58–60 fps held under a full Cinnamon desktop. Cost: avg compose-CB
record ≈ 200–250 µs steady (1.29 ms cold-start spike), ~1.5% of a
16.6 ms frame budget. `damage_fraction` rode at ~1.000 for the back half
— honest full-screen damage from the fast drag (worst-case compositing
workload), not a fallback, and compose cost stayed flat through it.
Frame-builder close reasons still split `legacy_sc` ≈ `present_completion`
(~half each), i.e. the legacy scene-compose path is still in use as
expected pre-B.4. Net: Cinnamon runs vsync-locked at 60 fps with zero
stalls/misses/aborts on bee, even under a screen-spanning drag.

---

### What runs on v2 today (after 3f.15 + hardware-smoke fixes)

- xeyes pupils render correctly (post-`dae5769` MaskScratch
  IDENTITY attachment view).
- xeyes eye whites render without horizontal stripes (post-
  `e76a6f6` trap-shader AA off-by-0.5).
- Cairo / GTK gradient widget backgrounds render with actual
  ramps, not first-stop flat colour (3f.13).
- Fresh pixmaps read back as zero (3f.14 + `fcd2521` —
  Vk DEVICE_LOCAL no longer surfaces recycled GPU memory
  through SHAPE-clipped client drawing).
- xeyes resize-UP renders cleanly (post-`3afa5bd` xid-detach
  + `fcd2521` pixmap clear).
- xeyes resize-DOWN still shows artefacts — see
  [`known-issues.md`](known-issues.md) under "Drawing /
  rendering artifacts".

### What runs on v2 today (after 3b)

- Core paint ops via Stage 2: `xsetroot -solid <color>` cycles
  colours on bee (verified 2026-05-16).
- Core text via Stage 3a: `image_text8`/`image_text16`/
  `poly_text8`/`poly_text16` against any drawable; glyph atlas
  + FenceTicket discipline live; back-to-back upload race test
  passes under lavapipe. **No real app exercises this in
  isolation** — Core-text-only clients (twm window labels,
  pre-Xrender `xsetroot -name`) are the only path; xclock /
  xeyes / xterm / Cairo / Pango all reach for RENDER first.
- Picture-record protocol via Stage 3b: every
  `RenderCreatePicture` / `RenderChangePicture` /
  `RenderCreateSolidFill` / gradient lifecycle / clip / filter /
  transform op stores its record correctly. **No paint side
  effect** — Stage 3c is the substage where these records start
  driving draws.

**RENDER-painting apps stay black on v2 until 3c–3e land.** A
`just yserver-xclock-only-hw` smoke on 2026-05-16 confirmed this
shape: clean v2 boot + RADV ready + first pageflip + xclock
connects + `render_create_picture` / `render_create_solid_fill`
/ `render_trapezoids` gap-logs fire on first paint — i.e.
expected. No crash, no atomic-commit failure.

## VT switching (branch `vt-switch`, 2026-05-28)

Ctrl-Alt-F<N> switching away from a running yserver and back, ported
from the wlroots model. Spec:
`docs/superpowers/specs/2026-05-27-vt-switching-design.md`; plan:
`docs/superpowers/plans/2026-05-28-vt-switching.md` (converged after 3
codex rounds).

**Two modes**, chosen at startup by whether `libseat_open_seat`
succeeds:

- **Libseat mode** (logind/seatd present): `libseat::Seat` lives in
  `KmsBackendV2` on the core-loop thread as a new poll source
  (`BackendFdKind::Seat`). libinput moves onto the core loop (the
  reserved `LIBINPUT_TOKEN` hook) and opens its evdev fds through
  `seat.open_device`; VT switching is enabled.
- **Direct mode** (no seat manager — e.g. the bwrap sandbox): today's
  behaviour exactly — separate libinput thread, direct device opens,
  VT switching disabled. The fallback is silent.

**What's implemented + unit-tested (no hardware):** the `SeatState`
machine (`seat/state.rs`, exhaustive); Ctrl-Alt-F<N> detection via the
shared `HotkeyDetector`; held-key/button release synthesis on suspend;
`seat_state` gating of modeset/pageflip/submit; the suspend sequence
(deterministic libinput drain → synth releases → GPU wait → libinput
suspend → libseat `disable()` ack) and the resume driver + no-blink
double-switch boundary (stub-driven via `inject_seat_event_for_test`).

**wlroots-aligned deviations from the spec** (documented in the plan's
"Deviations" section): libinput on the core thread (no input-quiesce
barrier); the DRM fd is opened once and kept stable across switches
(NOT reopened, NO `drmSetMaster`/`drmDropMaster` — libseat/logind owns
master); on resume we only re-scan connectors + re-modeset the existing
device.

**MVP non-goals / known follow-ups:**
- Hardware validation pending (Task 14): bee / yoga / silence / iMac
  19,2 — Ctrl-Alt-F2 → getty → Ctrl-Alt-F1 → desktop restored, zero
  `vk_device_lost` / `missed_pageflips` once Active; plus bee
  rapid-double-switch. The sandbox can't acquire DRM master/seat0, so
  these are user-run.
- `wait_idle_bounded` is not truly timeout-bounded (relies on the
  driver returning `DEVICE_LOST`); fence-timeout bounding deferred.
- Resume repaint currently uses `scene.wake_for_damage()` rather than
  explicit full-output damage — verify on hardware that switch-back
  fully repaints; post explicit full damage if not.
- Dynamic RandR change-events on hot-unplug-while-suspended are a stub
  (logs dropped connectors) — that hot-plug path is an explicit
  non-goal.
- `YSERVER_SIMULATE_VT_SWITCH` live knob deferred (stub test covers the
  logic deterministically).
- No real-libseat container CI yet (CI is unit + lint).

## DPMS + MIT-SCREEN-SAVER (branch `feat/mit-screen-saver`, 2026-05-30)

DPMS (major opcode 134) shipped earlier on `feat/dpms`; MIT-SCREEN-SAVER
(major opcode 150, first_event 162) layers on top. Both extensions are
implemented end-to-end on yserver-hw — protocol, state machines, idle
timer, input-driven wake, and Xorg-faithful coupling.

Specs: [`2026-05-30-dpms-design.md`](superpowers/specs/2026-05-30-dpms-design.md) +
[`2026-05-30-mit-screen-saver-design.md`](superpowers/specs/2026-05-30-mit-screen-saver-design.md).
Plans: [`2026-05-30-dpms.md`](superpowers/plans/2026-05-30-dpms.md) +
[`2026-05-30-mit-screen-saver.md`](superpowers/plans/2026-05-30-mit-screen-saver.md)
(both converged with codex review).

**What DPMS ships:** 8 requests (GetVersion / Capable / GetTimeouts /
SetTimeouts / Enable / Disable / ForceLevel / Info — opcodes 0–7),
DPMS power levels On/Standby/Suspend/Off (0–3), per-output
`set_dpms_power` KMS hook, page-flip gating on `kms_outputs_active`
so suspended outputs don't get blits, input-driven wake to On,
idle-cascade evaluation in the poll loop, VT-leave reset + VT-return
cache restore, and `dpms_capable` snapshotted onto `ServerState` on
both KMS and ynest paths. Wake/notify hot path is allocation-free.
Error replies emit real `bad_value` (matching Xorg) rather than
generic zeros. **Post-landing EINVAL fix chain (2026-05-30,
`feat/dpms` `0b9221d`/`797e32a`/`bf4f0d8`):** the DPMS path missed
VT-switching's drain+rearm sequence AND `maybe_composite` had no
`kms_outputs_active` gate, which produced an atomic-commit EINVAL
storm that wedged bee/MATE on the wake path. Fix mirrors
`run_suspend`/`run_resume` drain+rearm around `set_dpms_power`,
always rearms the cursor on wake (restoring the retry-via-reissue
path), and gates `maybe_composite` on `kms_outputs_active` so a
suspended cycle no longer races a queued atomic. 8 verification
cycles on bee/MATE post-fix, zero EINVAL.

**What MIT-SCREEN-SAVER ships:** 6 requests (QueryVersion/QueryInfo/
SelectInput/SetAttributes/UnsetAttributes/Suspend), one sequential
event `ScreenSaverNotify` at first_event=162, per-client subscriber
masks (NOTIFY_MASK + CYCLE_MASK), per-client `Suspend` refcount,
idle activation, periodic Cycle re-fire, real handlers for the core
opcodes 107/108/115 (replacing the prior `log_void` stubs), Xorg-
faithful DPMS↔SS coupling (DPMS-non-On→SS-On forced=true; DPMS-On→
SS-Off forced=false; SS notify fires BEFORE DPMS notify on the wire),
and Xorg's unified-timer rule (`XScreenSaverSuspend` inhibits both
SS and DPMS firing — the Firefox/mpv/vlc fullscreen-video-inhibit
path). `SetAttributes` returns BadAccess unconditionally (documented
deviation — xscreensaver falls back to its own override-redirect
saver window).

**Bug this fixes:** mate-screensaver's lockscreen never activated on
yserver because there was no MIT-SCREEN-SAVER extension. With this
landing, the default-config mate-screensaver "blank screen after N"
fires its lockscreen as expected.

**Visible smoke matrix is user-driven** (per the spec; `xset s 60`
+ `xev -event screensaver`, mate-screensaver default config, mpv
fullscreen + `xset dpms 60 60 60` no-blank, etc.). Test coverage is
40 new unit tests across protocol/server/process_request/run/
disconnect modules — invariants only; the smoke is in the spec.

**IDLETIME counter + SYNC alarm firing (2026-05-31):** smoke testing
the MIT-SCREEN-SAVER landing revealed that MATE doesn't actually use
the X SS/DPMS timers — it uses XSync IDLETIME counter alarms for
idle detection, then calls `DPMSForceLevel` from its own logic. The
xtrace showed `mate-power-manager` calling `SetTimeouts(0,0,0)`
(explicit disable) and creating IDLETIME alarms at value=59999.
yserver's IDLETIME counter was stubbed to return SERVERTIME (server
uptime), AND `evaluate_alarms_for_counter` had a pre-existing bug
where `delta=0` alarms went Inactive regardless of test type
(Xorg `sync.c:548-555` only does this for Comparison types).

The IDLETIME fix landed across 5 implementation commits (plus 1
follow-up review-fix) on `feat/mit-screen-saver`:

- Real `IDLETIME` counter values (`now - last_activity` instead of
  server uptime); per-master-device IDLETIME counters (VCP=0x108,
  VCK=0x109) advertised in `ListSystemCounters`.
- Fixed `delta=0+Transition→Inactive` bug; switched re-arm to
  `checked_add` with overflow→Inactive.
- `ServerState::idletime_alarm_deadline()` chained into poll-min,
  with quiescent-state skip and unified-timer suspend gate.
- `evaluate_idletime_alarms_post_poll()` evaluator + create-time fire
  + Relative-value resolution for IDLETIME counters.
- Fanout-prologue `evaluate_idletime_negative_alarms_on_input_wake`
  for input-driven Negative-* alarm firing; per-device prior idle
  falls back to global when no per-device entry exists.
- `reset_idletime_state_after_suspend_release` at both SS suspend-
  drain sites to prevent stale-high cache values from missing
  post-resume crossings.

Spec: [`2026-05-31-idletime-sync-alarms-design.md`](superpowers/specs/2026-05-31-idletime-sync-alarms-design.md).
Plan: [`2026-05-31-idletime-sync-alarms.md`](superpowers/plans/2026-05-31-idletime-sync-alarms.md)
(converged after 4 codex rounds). +20 new unit tests. After this
the visible-smoke matrix that DPMS + MIT-SCREEN-SAVER alone could
not deliver (`mate-screensaver` lockscreen at N min; `mate-power-
manager` blanks display via `DPMSForceLevel` at N min) actually
works.

**MapWindow(root) phantom MapRequest fix (2026-05-31):** first hardware
smoke of the IDLETIME branch on bee/MATE blanked the screen but left it
with a moving cursor and no visible saver — the dialog never appeared
and the session was unrecoverable. Root cause was a latent v1 bug
unmasked by IDLETIME finally making mate-screensaver activate for the
first time: yserver's `handle_map_window` had the spec-required
"already mapped → no-op" guard positioned AFTER the SubstructureRedirect-
on-parent MapRequest dispatch, and yserver's root window has
`parent == self` (`resources.rs:194`). When mate-screensaver activates
it calls `MapWindow(root)`; that combination fanned a phantom
`MapRequest(parent=root, window=root)` to marco. Marco panic-
responded by GrabServer + ChangeWindowAttributes(root) DROPPING
SubstructureNotify+SubstructureRedirect from its event mask (visible
on the wire at xtrace seq `0x35e5a`), then UngrabServer. After that
marco no longer received MapNotify for new top-levels → no
`COMPOSITE::NameWindowPixmap` → the saver overlay and dialog were
invisible. Fix matches Xorg `dix/window.c:2661` exactly: hoist
`if (pWin->mapped) return Success;` above the MapRequest emission;
root (permanently `Viewable`) is naturally covered. Subsumes the
prior brisk-menu redundant-map guard. Regression test
`map_window_on_root_does_not_emit_phantom_map_request` asserts zero
MapRequest events from a non-WM client's `MapWindow(root)` against
a marco-like WM peer (pre-fix: 1; post-fix: 0). Commit `e4ef3f4`
on `feat/mit-screen-saver`. End-to-end visible smoke verified on
bee/MATE: saver appears → screen blanks → key press → DPMS wakes →
unlock dialog appears → password unlock → desktop restored. Three
loose ends surfaced during the smoke run filed in
[`known-issues.md`](known-issues.md): Vulkan
`shaderDemoteToHelperInvocation` validation error (latent, user
WIP), BadWindow teardown storm on saver/dialog destroy
(cosmetic), and 1471 leaked Vulkan objects at yserver shutdown
(pre-existing cleanup gap).

## HW cursor drag-lag fix (master, 2026-05-29)

After the VT-switch series landed, `55924b1 feat(kms/v2): flip HW cursor
strategy to default ON` (2026-05-28) made the HW cursor plane the default.
Symptom on bee/MATE drag: cursor smooth, window content trails badly;
`yserver-hw-mate.log` showed ~280 EBUSY errors in 4.7s, `frame_present_count/s`
collapsing to 0 during sustained drag, and `iter_wall_max` 80-93ms (5-6 vsync
periods per blocked iter).

**Diagnosis** (verified across two machines): cursor-plane atomic commits
and scanout-plane atomic pageflips on the same CRTC serialize in the
kernel atomic state machine, and during drag they collide repeatedly.
The kernel rejects the second with `EBUSY`; the second commit retries on
the next iter; meanwhile damage accumulates so `damage_fraction → 1.0`
and compose work grows, widening the contention window — feedback spiral
that ends with `frame_present_count/s → 0`.

**Failed architectural fix** (`bundle-cursor-atomic` branch, not merged):
bundle cursor properties into the scanout pageflip atomic so they ride
the same commit. Worked for drag (verified on yoga: drag → smooth, zero
EBUSY) but left idle cursor stranded — `scene.tick` is gated on
`scene_structure_dirty`, so on idle desktop the bundle absorb path
never fires, cursor only updates at ~5-9 Hz. A per-iter cursor flush in
`maybe_composite` was tried; produced a fresh EBUSY storm
(~200 cursor-EBUSYs/sec) because kernel cursor commits are vblank-paced
and we were spamming them.

The plan went through 4 codex review rounds (`docs/superpowers/plans/2026-05-28-cursor-bundled-atomic.md`)
and converged on a clean atomic-bundle design — but the design was wrong
for an X server, regardless of correctness of the implementation.

**The fix that landed** (`0b3fd0c fix(kms/cursor): use legacy ioctls for HW cursor`):
match Xorg's modesetting driver exactly
(`hw/xfree86/drivers/modesetting/drmmode_display.c:1797` →
`drmModeMoveCursor`; `:1812` → `drmModeSetCursor2`). The legacy cursor
ioctls don't EBUSY-collide with atomic scanout commits on the same CRTC
because the kernel routes them through a separate path from the atomic
state machine. Legacy is also **immediate** (not vblank-paced) — perfect
for cursor responsiveness. ~283 LOC deleted from `cursor_plane.rs`
(`PerCrtcAtomicState`, `discover_cursor_planes`, `fb` framebuffer,
imports); `show`/`hide`/`move_to` now have only the legacy path.

Verified on yoga (2026-05-29):

| Metric                       | Original master | bundle (drag fix) | cursor-legacy-ioctl |
|------------------------------|---|---|---|
| EBUSY (over ~30s)            | 284 | 362 | **0** |
| frame_present_count/s        | 0–37 (collapses) | 7–26 | **43–60 sustained** |
| missed_pageflips/s           | 5–10 | 4–9 | **0** |
| iter_wall_max                | 80–93ms | 26–99ms | 15–22ms |
| Log WARN lines               | 280+ | 90+ | **3** |

Bundle work preserved as the `bundle-cursor-atomic` branch on origin —
documents the architectural exploration we abandoned, retained as
reference for any future pure-atomic context (e.g. kernel/driver that
deprecates legacy cursor).

**Process lesson — when designing a new KMS architecture change,
check Xorg's modesetting driver first.** Memory rule "Xorg is the de
facto spec" applies at the *design* stage, not just at the diagnosis
stage. The codex plan-review loop verified correctness against the spec
but never asked "is the whole architectural approach right for an X
server?" — that question belongs upstream of plan-writing.

**Open follow-up — MATE drag still slightly choppy on yoga.** MATE/Marco
fires ~7000 RENDER (op133) calls/sec totaling ~18.8s of CPU over a 40s
run, with at least one 27.86ms single-call outlier (a dropped frame).
This is a separate problem from cursor — Marco's compositor + Caja
RENDER traffic. Not addressed by this work. A diagnostic perf-log
attribution inside the RENDER dispatcher (mirroring `c4093fa`'s
MIT-SHM perf log) would identify the heavy subop before any
optimization attempt.

## Dogfood bring-up: Firefox + Chrome + YouTube on bee (master, 2026-05-29)

A single-day investigation that took yserver from "Firefox profile chooser
empty-shadow" to "playing YouTube in Firefox" to "Chrome scroll works
everywhere" — five concrete fixes, all driven by visible-smoke evidence
and Xorg-vs-yserver xtrace diffs.

### Fixes that landed (all on master)

1. **`85ff1e7 diag(present): PRESENT-INSTR per-Pixmap info log`** — INFO-level
   log line per `Present::Pixmap` request (src kind+size, target window,
   wait_fence triggered bit, idle_fence). Cherry-picked from the now-
   discarded dma-buf-sync branch because it was the diagnostic that
   cracked the Firefox bug below ("`src=pixmap 1x1` over an 800×800 window"
   was the smoking gun).

2. **`25af7fa fix(x11): gate ConfigureWindow grow-Expose on Viewable`** —
   `handle_configure_window` unconditionally emitted `Expose` on every
   grow-configure, including on Unmapped windows. Under marco-style
   reparenting WMs that puts a phantom Expose ~50 requests before
   MapNotify; FF would service it, paint background to transparent
   black, mark the window clean, and the real content never reached
   the compositor. Spec-correctness gate mirroring MapWindow's existing
   Viewable filter. Not the FF root cause but a real protocol fix.

3. **`df500df fix(x11): emit VisibilityNotify(Unobscured) to viewable
   descendants on Map`** — MapWindow only emitted `VisibilityNotify` for
   the directly-mapped window. Xorg emits it for every previously-
   mapped descendant that transitions Unviewable→Viewable when an
   ancestor maps (FF profile-chooser child `0x02400010`). GTK3's
   frame clock keys content paints off `VisibilityNotify`; missing
   notification left the child stuck non-paintable. New
   `emit_visibility_unobscured_subtree_to_state` helper in `fanout.rs`.

4. **`e9b7802 fix(present): emit Present::ConfigureNotify on window resize`** —
   *The actual Firefox empty-shadow root cause.* Mesa's `loader_dri3_helper`
   keys swap-buffer reallocation on `Present::ConfigureNotify`. yserver
   never emitted it. Race-determined: when Mesa's first
   `DRI3::PixmapFromBuffers` happened to land AFTER the GTK
   ConfigureWindow to 800×800, FF rendered; when it landed before
   (window still 1×1), Mesa stayed at 1×1 forever — every Present
   landed a 1×1 pixmap over an 800×800 window → blank.
   `mate.xtrace` fail-vs-pass on the same binary showed
   `src=pixmap 1x1` vs `src=pixmap 800x800` PRESENT-INSTRs — pure
   timing race that the missing event closed.

5. **`53542c9 fix(xi2): match Xorg byte-for-byte wire format on pointer events`** —
   *Chrome scroll-crash, then Caja scroll-stuck.* Release Chrome (Ozone X11)
   stack-smashed on every wheel-scroll into a yserver window. Three earlier
   hypotheses (implicit dma-buf sync; XI_POINTER_EMULATED flag alone;
   NoEmulation-on-scroll-class alone) didn't move the symptom. The
   `mate-xorg.xtrace` vs `mate.xtrace` byte diff on the same scroll event
   revealed four cumulative wire-format gaps + one latent valuator-encoding bug:
   - `flags=0` vs Xorg's `0x00010000` (XIPointerEmulated)
   - `buttons_len=1` vs Xorg's 8 (32-byte button mask)
   - `valuators_len=0` (on button events) vs Xorg's 2
   - scroll-class declared `NoEmulation` but still emitted buttons (contradiction)
   - scroll axisvalue was `delta` (±1) instead of cumulative

   Width + flag fixes were needed for Chrome's pre-sized event-struct
   reader. The cumulative-vs-delta fix was a pre-existing GDK bug
   masked by the missing XIPointerEmulated flag — once GDK correctly
   skipped the emulated buttons, the broken Motion-axis path became
   the only scroll source and Caja got "1 tick then stuck." All five
   fixed together; verified on Chrome, Caja, Firefox, video playback.

### What didn't land

- `fix/dri3-implicit-dmabuf-sync` branch (commits `4011764`, `d965c89`,
  `39bf239` of the original plan docs) — the implicit dma-buf
  sync read-side fence bridge based on a refuted hypothesis. Branch
  preserved as historical record; PRESENT-INSTR diagnostic salvaged
  into the standalone `85ff1e7` commit above. Plan docs
  (`docs/plans/2026-05-29-dri3-implicit-dmabuf-sync.md` and
  `docs/plans/2026-05-29-present-wait-fence.md`) dropped — both
  proposed fixes for what turned out to be a different bug.
- `fix/xi2` first attempt (`7b5461d`, reset from history) — XI_POINTER_EMULATED
  flag alone with a false-verification commit message. Re-landed combined
  with the width and cumulative fixes in `53542c9`.

### Methodology takeaway

For race-y intermittent bugs, **same-binary fail-vs-pass xtrace diff** is
far more informative than Xorg-vs-yserver. The cross-server diff
conflates "different code path" with "different timing"; same-server
isolates timing alone. Saved to memory
([`feedback_same_binary_fail_vs_pass_diff`](../../../.claude/projects/-home-jos-Projects-yserver/memory/feedback_same_binary_fail_vs_pass_diff.md)).

When that's not enough — and for client-specific crashes — **byte-for-byte
Xorg vs yserver event diff** is the second line of attack. Each spec-
allowed-but-different field is a candidate; cumulative pressure on a
strictly-parsing client (Chrome's Ozone X11 layer) needs all of them
matched, not just the ones spec-language calls "MUST."

## Systray applet loop — RESOLVED (2026-06-01, branch `clip-by-children`)

**Status: FIXED and HW-verified on silence.** Both symptoms — the
per-vsync mate-panel notification-area damage storm AND invisible tray
icons — were a single root cause: **yserver never applied
`ClipByChildren` to RENDER paint or the damage it emits.** Tray icons
now render (first time ever) and the loop is gone.

The fix is at the RENDER FillRectangles site
(`process_request.rs`, opcode 26) and clips BOTH halves to the
window's effective region (geometry MINUS mapped `InputOutput`
children), matching Xorg:

- **paint** — `clip_fill_rects_by_children` subtracts mapped child
  rects from the requested fill rects (`nested::subtract_regions`),
  re-encodes wire bytes; an empty result skips the whole op (no
  backend fill, no damage). This is what stops the Clear from wiping
  the embedded icon's backing → icons stay visible.
- **damage** — `accumulate_damage_clip_by_children_to_state`.

Shared child-region helper: `mapped_child_clip_rects` in
`damage_fanout.rs`. Pixmaps and childless windows pass through
unchanged.

**Measured on silence (`yserver-mate-hw-trace`), yserver before → after:**

| Metric | Before | After | Xorg |
|---|---|---|---|
| Total DAMAGE-Notify | 1486 | 146 | (29 to panel) |
| DAMAGE-Notify → panel applet | 1486 | 21 | 29 |
| FillRectangles per tray socket | ~485 | 5–8 | <14 |
| Total FillRectangles | 3114 | 521 | — |
| Trace lines (session traffic) | 91 983 | 43 294 | — |

Roughly half the session's protocol traffic was this one dead loop;
killing it also recovers idle CPU and lowers latency (the
single-threaded core was being woken ~per vsync to service a no-op).

**Open follow-ups (not blocking; the storm is fixed):**

0. **Tray icons intermittently blank / bottom-sliver** — a SECOND,
   distinct tray bug surfaced after the storm fix: COMPOSITE
   manual-redirect paint-routing race (routing set lazily in the scene
   walk, not at `RedirectWindow` time, so a plug that paints before the
   scene walk loses its content). Code-grounded; confirming probe +
   fix direction in `known-issues.md` ("Tray icons intermittently
   blank / bottom-sliver") and the `project_systray_icon_redirect_race`
   memory.


1. The other RENDER paint sites — Composite (~`1592`), CompositeGlyphs
   (~`1414`/`1488`), Trapezoids/Triangles (~`1764`) — still
   `accumulate_damage_full_to_state` with no ClipByChildren on paint.
   Same latent bug class; just not exercised by the tray. Generalize
   the shared helper across them.
2. The CopyArea "skip ClipByChildren subtraction for manually-
   redirected children" workaround at `process_request.rs:~13462` is a
   tray-specific patch reflecting the same gap — review for
   consistency now that the proper machinery exists.
3. Remove the `YSERVER_DAMAGE_BACKTRACE=1` diagnostic in
   `accumulate_damage_to_state` (its emitter-hunt is complete).

The parked `fix/applet` source-picture-redirect branch (10 commits,
tip `b76a537`) is a real Xorg-spec asymmetry but was NOT load-bearing
for either symptom; it stays parked for a future workload that
surfaces it.

---

### Investigation history (preserved)

Long-standing mate-panel notification-area-applet bug:
mate `mate-bee.xtrace` baseline (2026-05-31) — applet receives ~61
DAMAGE-Notify/s vs Xorg's 0.76/s on bee, plus tray icons render
invisible. The original codex-assisted diagnosis identified an
asymmetry: v2's `resolve_dst_picture_for_render` walks through
`KmsBackendV2::resolve_paint_target` for redirect routing, but the
SOURCE counterpart `resolve_picture_for_render` did `store.lookup`
directly and never walked the redirect chain. Hypothesis: source
pictures wrapping a redirected window sample the leaf storage
(empty under Manual redirect) instead of the backing → composite
produces empty output (icons invisible) → applet's FillRect Clear
on the same picture damages the backing → DAMAGE-Notify → applet
wakes → 60 Hz loop. One defect, two symptoms.

Plan executed (`docs/superpowers/plans/2026-05-31-render-source-picture-redirect.md`
on the parked branch): 10 commits on `fix/applet` landing
`KmsBackendV2::resolve_source_picture` + Phase 2 wire-up in
`render_composite` + Phase 3 in `render_trapezoids` /
`render_triangles_op` (engine signature gained `src_offset: (i32, i32)`)
+ Phase 4 tray-shape integration test. All 13 added tests pass.
Each commit is atomic + bisectable.

**hw smoke outcome (yoga, X1E / Turnip, 2026-06-01):**

- Tray icons remained invisible.
- Total systray request rate: pre-fix 1338 req/s → post-fix
  1270 req/s. ~5% delta, well within trace-duration noise.
- Xorg-on-yoga reference (same workload, same 4 tray icons):
  7.5 req/s. **yserver is ~170× the Xorg rate** for the same
  systray workload — yoga is NOT an intrinsically hot workload;
  it's a real yserver bug.

**Subsequent code-backed diagnosis (codex, 2026-06-01):**

The actual loop driver is `crates/yserver-core/src/core_loop/process_request.rs:1622`
— RENDER FillRectangles' unconditional
`accumulate_damage_full_to_state(dst_drawable)`. When the applet's
`FillRectangles op=Clear` on `picture(proxy)` lands, yserver damages
the full proxy drawable. The applet has `DamageCreate` on those
proxy/socket drawables (the XEMBED-host pattern for noticing icon
redraws). The applet receives its own DamageNotify → Subtract +
Compose + Clear → repeat at vsync.

In Xorg, the same `Clear` on `picture(proxy)` is a true no-op:
default `subwindow-mode = ClipByChildren`
(`xserver/render/picture.c:719`) plus `miValidatePicture`'s
window-clip-list intersection (`xserver/render/mipict.c:112-118`)
make `pCompositeClip` empty when children fully cover the proxy.
`damagePolyFillRect`'s `TRIM_BOX` reduces the damage box to empty,
and the actual write is also gated by the GC clip — no damage
emitted, icon pixels in the backing preserved across cycles.

The source-picture redirect fix that landed on `fix/applet` IS a
real asymmetry per the Xorg spec, but it's NOT the load-bearing
driver of either symptom on yoga. The Risk Register on the
original plan named this exact outcome as "outcome 2" and called
out ClipByChildren as the likely follow-up. We landed there.

**Process notes worth carrying forward:**

- Multi-symptom diagnoses that elegantly explain everything with
  one defect deserve MORE skepticism, not less. The
  source-picture-fixes-both shape was clean but wrong about
  load-bearing-ness.
- yoga-Claude's mid-investigation pivot ("damage_fanout descends
  into children") was wrong about mechanism — codex's code read
  confirmed `damage_fanout` only ascends; the
  `level_drawable=0x2600011 match_ids=1` lines come from a
  separate `accumulate_damage` call at `process_request.rs:1622`,
  not from descent. The symptom observation was right, the
  mechanism interpretation was wrong.
- Branch parked at `origin/fix/applet` (tip `b76a537`), not
  merged. Source-picture fix is preserved for future workloads
  that surface it; the loop fix is a follow-up plan.
- yserver-core working-tree diagnostic block
  (`YSERVER_DAMAGE_BACKTRACE=1` gate in `damage_fanout.rs`)
  preserved on the WIP commit (`b76a537`). Re-add to working
  tree on the next branch — it's load-bearing for verifying
  the ClipByChildren fix.

**Findings doc:** `docs/superpowers/findings/2026-05-31-render-source-picture-redirect.md`
(post-mortem on the parked branch).

**Next direction (to be planned next session, probably on
silence):** ClipByChildren-aware FillRectangles in v2's
RENDER paint paths. Specifically:

1. At `process_request.rs:1622` (and the matching sites for
   `render_composite`, `render_trapezoids`, etc.): the actual
   paint AND the damage emit need to be gated by the picture's
   effective clip-by-children region. Damage-only gating stops
   the loop but leaves icons flickering (Clear still wipes the
   backing every cycle). Paint-gating preserves the icon
   pixels — both halves are needed.
2. The existing CopyArea "skip ClipByChildren subtraction for
   manually-redirected children" workaround at
   `process_request.rs:13462` is a tray-specific patch that
   probably reflects the same underlying gap. Worth reviewing
   for consistency once the proper ClipByChildren machinery
   lands.
3. Re-test on bee (the platform where the original ~80×
   divergence was measured) as the load-bearing validation.
   yoga remains the rate-canary post-fix.

## GLX texture-from-pixmap (branch `feat/glx-texture-from-pixmap`, 2026-06-10)

**Status: TFP IN USE end-to-end, HW-verified on yoga (X1E/Turnip) and bee
(6900HX/RADV).** `GLX_EXT_texture_from_pixmap` — the live-texture compositing
path Compiz / picom-glx / muffin need (no copy-upload fallback). Unblocks
`project_compiz_is_required`.

Spec/plan: `docs/superpowers/{specs,plans}/2026-06-09-glx-texture-from-pixmap.md`
(post-implementation findings appended to the plan). Root-cause findings:
`docs/superpowers/findings/2026-06-09-glx-tfp-radv-export-rootcause.md`.

**What landed (four components + the unlock):**
- **Phase 0 — exportable image.** `allocate_exportable` (`kms/vk/target.rs`)
  allocates a dma-buf-exportable VkImage. Tiling is **LINEAR-preferred,
  modifier-fallback**, cached per `VkContext::tfp_tiling_strategy`
  (`0eb7432`): Turnip needs LINEAR (UBWC keeps compression metadata in driver
  caches that don't reach the dma-buf on same-GPU share → frozen snapshot);
  RADV rejects LINEAR+COLOR_ATTACHMENT+dma-buf so falls through to modifier.
  (The original modifier-first default was wrong on Turnip.)
- **Phase 1 — DRI3 `BuffersFromPixmap` (op 8) export.** Un-deferred; emits
  per-plane fd/stride/offset + the driver-chosen DRM modifier, threaded
  `ExportableImage → Storage::export_modifier → reply`. op-3 delegates to it.
  Also fixed scanout's modifier-BO layout query to `MEMORY_PLANE_0` aspect.
- **Phase 2 — dma-buf sync.** `IMPORT/EXPORT_SYNC_FILE` ioctls wired + firing
  (GL-reads-while-we-write hazard); correct, but were NOT the Turnip unblocker
  (LINEAR was).
- **Phase 3/4 — GLX surface + DRI3 plumbing** (pre-existing on the branch).
- **THE UNLOCK — GLX fbconfig/visual matchability (`061aa131`).** mesa's GLX
  dri3 *screen* creation failed `No matching fbConfigs or visuals found`
  (upstream of any export — the plan's "GLX surface is correct" premise was
  wrong). `driConfigEqual` (mesa `src/glx/dri_common.c` attribMap)
  scalar-compares the server's config against the client driver's
  `__DRIconfig`; three of yserver's synthesized attrs didn't match radeonsi:
  depth-24 `alpha 0→8` / `buffer_size 24→32`, depth-24 `BIND_TO_TEXTURE_RGBA
  0→1`, and `BIND_TO_TEXTURE_TARGETS 2D|RECT → GLX_DONT_CARE` (DONT_CARE is
  the matcher's wildcard; the only portable value). Diagnosed by dumping
  radeonsi's live configs via `tools/glx-cfgdump.c`.

**Wire-level proof TFP is live (not copy-upload fallback), bee xtrace:**
6× `glXCreatePixmap` + **5× DRI3 `BuffersFromPixmap` (op 8)** + 5×
`GetSupportedModifiers` + 5× `PixmapFromBuffers`. Note `glXBindTexImageEXT`
does NOT appear in the X trace — under DRI3 it's handled client-side by mesa
(re-sampling the already-imported dma-buf), so the wire-level TFP-in-use
signal is the op-8 export count; the per-frame Bind shows only in a
client-side apitrace (yoga session: 86× Bind / 82× Release).

**Stale-frame fix was SEPARATE and upstream (master `8b93ce6`).** The
cinnamon-settings stale-pane repro was a DAMAGE-emission bug, not TFP:
`damage_fanout.rs` applied `NonEmpty`'s one-shot gate to all levels, but
muffin uses `BoundingBox` (level 2) which per the X11 DAMAGE spec + Xorg
`damageext.c:136-153` must emit on EVERY paint. TFP alone would not have
cured it.

Validation: `cargo test -p yserver-core --lib` 721/721, `-p yserver --lib`
441/441, clippy + fmt clean. **Open follow-ups (non-blocking):**
`shaderDemoteToHelperInvocation` VVL error (latent, also flagged under
MIT-SCREEN-SAVER above), and `vkDestroySemaphore`-in-use VVL errors during
the live GL-client load.

## Animated cursors (branch `feat/anim-cursor`, 2026-06-10)

**Status: RENDER `CreateAnimCursor` animates on KMS v2, HW-verified on bee
2026-06-10.** Previously the cursor was frozen on the first frame of an
animated cursor (the `XFixes::GetCursorImage` static-capture path was the
only consumer). ynest is unchanged — animated cursors degenerate correctly
to the static first frame (host X11 does not support animated cursors via
the same protocol path). Full test suite (`cargo test --locked`) and
`cargo clippy` + `cargo +nightly fmt --check` clean.

## v1 → v2 transition

The v1 model (per-window mirrors + scanout-walk) hit a structural
wall: compositing WMs (xfwm4 with compositing, picom, xcompmgr)
paint to root or COW, but yserver's scanout never displays root's
mirror. T1-T4 of the Manual-redirect plan made the
`NameWindowPixmap → BadAlloc` symptom go away but couldn't make
the compositor's paint visible — the model itself was the
blocker.

v2 replaces the rendering layer (not the protocol / input / WM /
KMS-setup layers) with a four-component core:

- `PlatformBackend` (HW + OS surface)
- `DrawableStore` (storage + lifetime + damage)
- `RenderEngine` (paint ops into storage)
- `SceneCompositor` (composed output pass with damage-clipped
  redraw + buffer-age correctness)

Ships as a **parallel implementation** `KmsBackendV2`, alongside
the existing `KmsBackend` (v1). `YSERVER_RENDER_MODEL` env var
picks at startup. Both embed a shared `KmsCore` for protocol
bookkeeping (XID maps, window/pixmap/font/cursor metadata,
COMPOSITE redirect records, SHAPE regions, picture records — no
Vk / GPU types).

End-goal: yserver flies on non-ancient hardware. v2's job is to
be the substrate that makes reaching that goal possible.

## Stages

Per the spec (`docs/superpowers/specs/2026-05-15-rendering-model-v2.md`).

### Done

- [x] **Stage 1a — `KmsCore` extraction.** Landed 2026-05-16
  (`56ad631`). ~30 protocol-bookkeeping fields moved from
  `KmsBackend` into a new `KmsCore` struct. Pure refactor; v1
  behaviour bit-identical. Hardware smoke green
  (`just yserver-xfce-hw` confirms no regression).
- [x] **Stage 1b — `KmsBackendV2` skeleton + startup selector.**
  Landed 2026-05-16 (`982f1eb`). Sibling backend embedding the
  same `KmsCore`; all 118 `Backend` trait methods stubbed with
  once-per-method `v2: <method> not yet implemented` warn.
  `YSERVER_RENDER_MODEL=v1` (default) → `KmsBackend`;
  `=v2` → `KmsBackendV2`. Wires `kms::dispatch::KmsBackendKind`
  selector + shared `platform_init` helper. Hardware smoke
  green: v1 unchanged, v2 boots through capability queries +
  logs gaps on first paint.

### Done (continued)

- [x] **Stage 2 — minimal-Vk correct baseline.** Plan landed
  2026-05-16 (`88d3f3d`) after three codex review rounds; six
  substages 2a–2f. Cross-cutting concepts: two-sync-object model
  (FenceTicket for CPU lifetime + per-ScanoutBo export semaphore
  for KMS IN_FENCE_FD), image-layout state machine per drawable,
  compose-after-paint via same-queue submit order + in-CB barrier
  (zero `vkQueueWaitIdle` on hot path), v2-native RenderBatch
  (not v1's PaintBatch), compose-read consumer tracking on
  FenceTicket Rc.
  - [x] **2a — PlatformBackend real.** Landed 2026-05-16
    (`6f3f423`). Real DRM/Vk/libinput owner replaces the flat
    field set from Stage 1b. FenceTicket + FencePool (recyclable
    VkFence allocator), per-BO BoGenerationEntry tracking
    (last_present_generation + content_invalidated) parallel to
    ScanoutBoPool's BoState, ScanoutBoToken + PageFlipRetirement
    + invalidate_bo + record_present + commit_bo_present hooks.
    KmsBackendV2 delegates fb_dimensions/randr_outputs/take_input_ctx/
    disable_output/poll_fds through it. Paint paths still log
    gaps; v1 untouched. 11 v2 unit tests + 297 yserver tests green;
    clippy clean.
  - [x] **2b — DrawableStore real.** Landed 2026-05-16
    (`4bda93d`). Real storage + lifetime + damage bookkeeping:
    DrawableId/DrawableKind, Storage (Vk image + layout tracker),
    RegionSet, Drawable with refcount + scene_participating + the
    two damage lists per I5 + presentation_damage_epoch for
    snapshot/ack + last_render_ticket for I6a retirement. allocate
    / lookup / incref / decref → RetireDecision,
    set_scene_participating (clears unpresented presentation
    damage + bumps epoch per codex round 1 point 5), damage,
    peek/ack_presentation_damage with the epoch-survival rule,
    touch_render_fence, poll_pending_retire. Storage allocation
    split from metadata so test fixtures (no VkContext) and
    production paths flow uniform. 14 new unit tests; backend
    wiring of allocation methods (create_pixmap etc.) lands
    incrementally across Stages 2c–2d.
  - [x] **2c — RenderEngine minimal (fill / put_image / get_image).**
    Landed 2026-05-16. Three Vk paint ops on the v2 path:
    `vkCmdClearAttachments`-driven fill_rect; staging-buffer
    `vkCmdCopyBufferToImage` put_image (depths 1/8/24/32 with
    MSB-first depth-1 unpack); synchronous `vkCmdCopyImageToBuffer`
    get_image. Each op self-contained — one CB + one
    FenceTicket per op (per-batch coalescing deferred to Stage 5);
    `submitted` deque retires on signal; `get_image` is the only
    `wait` path. `create_pixmap`/`free_pixmap`/`fill_rectangle`/
    `poly_fill_rectangle`/`put_image`/`get_image` wired through
    KmsBackendV2 → engine → store. Offscreen acceptance: 3
    Vk-backed roundtrip tests (depth-32 PutImage→GetImage,
    fill→GetImage, PutImage-then-fill) passing under lavapipe.
    11 logic-only unit tests for byte-stride math + clipping.
    v1 path unchanged.
  - [x] **2d — copy_area + scene graph + blit pipeline.**
    **Part 1** (`6151e34`): RenderEngine `copy_area` with
    disjoint + same-image-scratch paths. 2 Vk-backed acceptance
    tests under lavapipe.
    **Part 2 landed 2026-05-16**: SceneCompositor real with
    v1's `CompositorPipeline` reuse, per-output
    `CompositePoolRing`. Window lifecycle wired on KmsBackendV2
    — `register_top_level` / `register_subwindow` /
    `create_subwindow` / `configure_subwindow` /
    `map_subwindow` / `unmap_subwindow` / `destroy_subwindow` /
    `reparent_subwindow`, all maintaining `windows_v2` map +
    `DrawableStore` allocation/decref + `scene_participating`
    flip + scene-structure-dirty bumps. `composite_and_flip` /
    `maybe_composite` / `mark_dirty` / `on_page_flip_ready`
    wired through `scene.tick` / `scene.handle_page_flip_complete`
    + engine retirement + store retirement. Full-redraw every
    tick; bg_pixel is the compose clear color; cursor + bg_pixmap
    deferred (Stage 3/4). 36 unit tests + 5 Vk-backed
    integration tests passing under lavapipe.
    Hardware smoke for first-visible-scanout pending — Stage 2f
    user-run smoke covers it.
  - [x] **2e — Buffer-age clipping + I6b retirement + failed-flip
    recovery.** Landed 2026-05-16. `BufferAgeRing` per output,
    `pick_repaint_region` algorithm (full-redraw fallback on
    invalidated BO / fresh BO / history loss; clipped repaint
    otherwise). `OutputSceneState` tracks
    `scene_structure_damage` + `pending_repaint_after_failed_submit`
    as `RegionSet`s with the transactional snapshot/ack rule
    (codex round 2 point 2). v2-specific `record_compose_v2`
    function forks v1's `record_and_present_composite` to add
    `loadOp=LOAD` + clip-region scissor for buffer-age paths.
    9a (queue-submit failed) and 9b (atomic-commit failed)
    recovery paths share descriptor-pool slot release + repaint
    deferral; only 9b invalidates the BO's content tracking.
    `handle_page_flip_complete` subtracts submitted snapshots
    from live state (post-submit damage survives) and pushes
    output damage onto the history ring.
    6 new unit tests for ring trim / history-window math /
    repaint picker fallback cases. Stage 2f's synthetic harness
    is the load-bearing buffer-age oracle test (deferred).
  - [x] **2f — Telemetry + acceptance harness + hardware smoke.**
    **Telemetry + acceptance landed 2026-05-16**:
    `kms::v2::telemetry::Telemetry` owns per-second counter
    bucket + lifetime aggregates per spec §"Required counters".
    Counter sites wired (paint_submit on fill/put/copy success,
    one_shot_submit + fence_wait on get_image, composite_submit
    on scene tick, frame_present on page-flip retirement,
    storage_allocation + image_view_create on pixmap/window
    alloc). `YSERVER_LOOP_TELEMETRY=1` enables the per-second
    summary line via `maybe_emit` from `maybe_composite`.
    `crates/yserver/tests/v2_acceptance.rs` ships 3 Vk-backed
    acceptance tests (PutImage→GetImage byte-identical;
    fill+gradient compose oracle; CopyArea disjoint oracle;
    telemetry-lifetime assertion confirming `vk_queue_wait_idle
    == 0`), driving `KmsBackendV2` through the `Backend` trait
    surface. Functionally equivalent to the plan's "synthetic
    harness binary" minus the X11 protocol encoding layer —
    correctness is at the Backend-trait boundary, not protocol
    bytes. `KmsBackendV2::for_tests_with_vk` constructs a live-Vk
    fixture for these. 4 lib telemetry unit tests + 3 acceptance
    tests all green under lavapipe.

    **vng smoke under Venus passthrough (2026-05-16)** —
    `just yserver-v2` / `just yserver-v2-fvwm3-xterm` recipes
    added, run via `-display egl-headless,gl=on` (headless) or
    `gtk,gl=on` (when host has DISPLAY / Xwayland). Verified
    end-to-end in vng:
      - `driver_id=MESA_VENUS`, `1 scanout pools live`,
        `first pageflip complete on output 0 (bo 0)` — Stage 2d
        atomic-flip path lives on Venus-exported dma-buf scanout.
      - `xdpyinfo` (post `xkb_proxy` fix, see below) returns a
        full reply — 16 advertised extensions, depths, screen
        info.
      - `xsetroot -solid red`/`green` drives **7 composite_submits
        + 7 frame_present_count in a 1s telemetry window**,
        `vk_queue_wait_idle/s=0` holds throughout.
      - **Stage 2e 9b atomic-commit-failed recovery fires live**
        under back-to-back composes racing the vblank
        (`atomic commit failed ... Device or resource busy; BO
        invalidated`); the scene loop recovers cleanly and
        keeps going. The recovery path is verified outside the
        unit-test scaffolding.

    **`xkb_proxy` wired in v2** (`b001911`, 2026-05-16). The
    Stage-1b stub returned `Ok(None)` for every XKB minor
    opcode, which kills any Xlib-based client at the XKEYBOARD
    UseExtension handshake — verified: xterm, xeyes, xsetroot,
    xdpyinfo all disconnected silently under the old stub.
    Since `KmsCore.xkb_keymap` is shared with v1 and the
    `kms::xkb::reply_*` helpers are `pub(super)`, v2 mirrors
    v1's body verbatim. This was the hidden blocker for any
    real-client smoke: without it, hardware smoke on bee + fuji
    would have produced "boot + first pageflip" and nothing
    more, identical to the pre-fix vng result.

    **Known polish item (Stage 5 territory)**: under rapid
    back-to-back composites the scene tick can pick a Free BO
    and submit before the prior flip retires, causing KMS to
    return EBUSY on the second `present_scanout` (9b path).
    Recovery handles it correctly (BO invalidated + repaint
    deferred) but the warning is noisy. Fix is to gate
    `tick_one_output` on per-output flip-pending state so we
    only submit when we know KMS will accept; tracked for the
    perf-plans cycle on top of v2.

    **Hardware smoke on bee — green 2026-05-16.**
    `YSERVER_RENDER_MODEL=v2 just yserver-v2-xsetroot-hw` on
    bee (RDNA2 / RADV, 2560x1440) cycles through 11 solid
    colors with `xsetroot -solid <color>`. Two hardware-only
    bugs surfaced + fixed during the run:
      - `bc6718a` — **per-output flip-pending gate.** On RADV
        at 2560x1440, mate-session's connect-flurry fires
        composite-after-composite faster than vblank; KMS only
        allows one pending atomic commit per CRTC and returns
        EBUSY on each subsequent submit. The 9b recovery path
        invalidated every BO faster than they could retire, so
        no BO ever landed → frozen at the boot frame. Fix
        skips `tick_one_output` when `state.pending_acks` is
        non-empty; scene_structure_dirty stays set so the next
        tick (post page-flip-complete) picks up deferred
        damage. KMS submit rate is now structurally bounded to
        one flip per vblank per output.
      - `3d90a0c` — **empty-scene Clipped/LOAD bug.** With the
        flip-pending gate in place, compose ran cleanly (9
        composite_submits/s + 9 frame_present/s) but the
        screen stayed black after xsetroot. Buffer-age picked
        `Clipped` (loadOp=LOAD) once the history ring had
        enough generations; `scene.draws` was empty (xsetroot
        only mutates root.background_pixel — no top-level
        windows), so LOAD preserved each BO's pre-update black
        and we drew nothing on top of it. Force `Repaint::Full`
        whenever the scene has zero draw entries so loadOp=CLEAR
        paints the current `bg_color`. Stage 4's root storage
        makes the empty-scene case rare again.

    **Real client smoke** (xterm/xeyes/xdpyinfo/xsetroot)
    requires the `xkb_proxy` fix from `b001911`. Without it,
    Xlib clients abort at the XKEYBOARD UseExtension
    handshake; with it, xdpyinfo returns a full reply and
    xsetroot drives `bg_color` flips per the bee result above.

    **Stage 2 effectively closed.** mate/xfce/xfwm4 will not
    paint until Stage 3 (RENDER + glyphs + fonts) lands —
    they abort during their first open_font /
    render_create_picture calls and never reach
    set_container_background_pixel-equivalent paint paths.
    That's expected per spec; Stage 2's job was the substrate.

### In progress

- [x] **Stage 3 — RENDER + glyphs coverage.** Closed
  2026-05-17. Plan landed 2026-05-16 (`142cda8`) after four
  codex review rounds; six substages 3a–3f.

  Plan: `docs/superpowers/plans/2026-05-16-stage-3.md`.

  Cross-cutting concepts: atlas-fence ownership (drops v1's
  per-glyph `queue_wait_idle` by routing uploads through
  per-call `BatchUploadArena` slices owned by `SubmittedOp`,
  with same-graphics-queue submission ordering as the GPU
  dependency and `atlas_last_upload_ticket` cloned onto every
  consume for CPU lifetime); picture-record split between
  `KmsCore` and `RenderEngine`; clipping-domain separation
  (RENDER ops consult picture clip only; Core ops consult GC
  clip only; per-rect scissoring for multi-rect clips
  replaces v1's union-bbox); damage op-aware override for
  destructive PictOps + `GXclear`-class GC.functions;
  `drawable_view_cache` keyed on
  `(DrawableId, SamplerConfig, SwizzleClass)`.

  Acceptance gates: rendercheck parity, real-app smoke matrix
  (xterm/xclock/xeyes/gedit/MATE/xfce4/xfd + xterm-tail
  terminal-text load), bee 30-min stability run, fuji perf
  gates split into correctness-hard-fail (no
  `vk_queue_wait_idle/s` outside `get_image`,
  `composite_glyphs_dropped_unsupported == 0` across smoke
  matrix unless whitelisted, etc.) and headroom-observed-and-
  recorded (≤ 2× v1 envelope per workload).

  - [x] **3a-telemetry primer** — `atlas_intern/s`,
    `glyph_uploads/s`, `glyphs_dropped_atlas_full` (lifetime),
    `composite_glyphs_dropped_unsupported` (lifetime + per-s,
    sites wired in 3d), `disjoint_readback_count` (per-s,
    sites in 3c). Landed 2026-05-16 (`5d88295`). Counter
    storage + `record_*` hooks + emitter line additions; no
    call sites yet. Pure additions; no v1 path touched.
  - [x] **3a-atlas+text** — landed 2026-05-16 (`459da11`). New
    `kms::v2::glyph_atlas::V2GlyphAtlas` forks v1's atlas to
    drop the persistent staging buffer; each upload owns its
    own `StagingBuffer` slice for the lifetime of its
    `SubmittedOp` (`FenceTicket`-gated). `RenderEngine` grows
    `glyph_atlas` + `text_pipeline` + `atlas_last_upload_ticket`;
    every atlas-sampling op clones the latest upload ticket
    onto its own `SubmittedOp.atlas_ticket` (CPU-side lifetime
    gate; same-graphics-queue submission ordering is the GPU
    dependency). `record_text_run` generalized to a
    `TextRunTarget` trait so both v1 `DrawableImage` and v2's
    `StorageTextTarget` (a borrow over v2's `Drawable` storage
    fields) flow through the same recorder. `KmsBackendV2`
    `open_font` / `close_font` mirror v1's body (KmsCore owns
    FontLoader + fonts already); `image_text8`/`image_text16`
    parse + lower a background fill via `engine.fill_rect`
    then call `engine.image_text`; `poly_text8`/`poly_text16`
    iterate TextItem8/16 with inline `0xFF` font-change items
    rotating `core.current_font`. Telemetry sites:
    `record_atlas_intern` / `record_glyph_upload` /
    `record_glyph_dropped_atlas_full` driven from the engine's
    returned `ImageTextStats`. 5 new tests (1 logic + 4
    Vk-backed under lavapipe): back-to-back-upload-no-
    corruption (load-bearing per codex round 1),
    image_text_run_records_damage, atlas_intern_uses_fence_ticket,
    atlas_full_drops_glyph_and_increments_counter,
    v2_poly_text8_font_change_advances_current_font. All
    211 yserver lib tests + 9 ignored Vk tests pass under
    lavapipe; clippy clean. **Plan claim that 3a unlocks
    xclock/xeyes was wrong** — both of those use Xrender for
    their analog/eye geometry and won't render until 3c–3e;
    Core-text-only apps (twm labels, fvwm decorations) are the
    actual 3a smoke surface. FreeType-path real-app text
    correctness gates land at 3f via xterm + gedit.
  - [x] **3b — picture records + pipelines.** Landed
    2026-05-16 (`4a01e68`).
    `KmsCore.pictures: HashMap<u32, PictureRecord>`
    added with v2-side `PictureRecord` enum (`Drawable` +
    `SolidFill` + `LinearGradient` + `RadialGradient` variants,
    plus `PictureFilter` for nearest/bilinear/convolution) and
    `GradientStop` helper. `KmsBackendV2` promotes every
    `render_create_picture` / `render_change_picture` /
    `render_free_picture` / `render_create_solid_fill` /
    `render_create_{linear,radial}_gradient` /
    `render_create_glyphset` / `render_free_glyphset` /
    `render_add_glyphs` / `render_free_glyphs` /
    `render_set_picture_clip_rectangles` / `_filter` /
    `_transform` stub to a real record on `KmsCore.pictures`
    (or `core.glyphsets`). 13 RENDER value-mask bits (CPRepeat
    through CPComponentAlpha) flow through a shared
    `change_picture_apply_mask` helper; gradient bodies parse
    endpoints + stops; clip-rectangles pre-shift by the
    clip-origin; filter names map by ASCII (`nearest`/`fast` →
    Nearest, `bilinear`/`good`/`best` → Bilinear,
    `convolution` → Convolution). DrawableStore refcount
    raised on `render_create_picture` against a wrapped
    Drawable, dropped on `render_free_picture` — `free_pixmap`
    now survives so long as a picture still references the
    pixmap (verified). `RenderEngine` grows a
    `picture_paint: HashMap<u32, PicturePaintState>` slot with
    `Empty` placeholder; `render_free_picture` calls
    `engine.picture_paint_remove` so the teardown hook is in
    place for 3c's gradient LUT lazy-build. `parse_add_glyphs`
    promoted from v1's `pub(super)` to `pub(crate)` so v2
    reuses it verbatim. No paint side effects yet — Stage 3c
    lights up `render_composite`. 5 new unit tests: lifecycle
    (every value-mask bit), drawable-refcount blocks
    free_pixmap, solid-fill stores wire color as-is (v1-parity
    against rendercheck premul convention), linear gradient
    parsing, clip-rectangles pre-shift. 216 yserver lib tests
    + 9 Vk-backed ignored tests pass under lavapipe; clippy
    clean. Application smoke unchanged (3b doesn't draw); RENDER
    paint apps stay broken until 3c.
  - [x] **3c — `render_composite` + `render_fill_rectangles`.**
    Standard PictOps + Saturate + Disjoint/Conjoint shader
    blend; per-rect picture-clip scissoring; self-composite
    aliasing routed through alias-readback scratch.
    - **3c.1 landed 2026-05-16 (`bcca8a3`)** — foundation only.
      `RenderEngineInner` grows `render_pipelines`,
      `solid_src_image`, `solid_mask_image`, `white_mask_image`,
      `dst_readback`, `drawable_view_cache` slots (lazy);
      `ensure_render_assets` builds them on first paint;
      `notify_drawable_retired` evicts cached views;
      `SubmittedOp.descriptor_arena` retires per-op pools.
      `record_render_composite` is now generic over a new
      `CompositeTarget` trait (impl'd for v1's `DrawableImage`
      and v2's `StorageTextTarget`). No new dispatch. v1 paths
      bit-identical.
    - **3c.2 landed 2026-05-16 (`ccac8c9`)** — paint bodies +
      wiring + telemetry. `RenderEngine::render_composite` /
      `render_fill_rectangles` are real: lazy ensure-assets,
      pipeline cache lookup, view cache for src/mask, synthetic
      1×1 scratch clears, dst_readback for Disjoint/Conjoint,
      per-rect picture-clip scissor (plan §4 deviation from v1's
      union-bbox shortcut — `record_render_composite` now takes
      a scissor slice). `KmsBackendV2::render_composite` /
      `render_fill_rectangles` resolve picture records from
      `KmsCore.pictures` and dispatch; `disjoint_readback_count`
      telemetry primer wired live. Three `kms::backend` helpers
      (`repeat_to_shader_const` / `compose_affines` /
      `pixman_transform_to_affine`) promoted to `pub(crate)` so
      v2 reuses verbatim.
    - **3c.3 landed 2026-05-16** — self-alias scratch routing
      + 7 Vk-backed acceptance tests under lavapipe. New
      `RenderEngineInner.src_alias_readback: DstReadback`
      slot (sibling to `dst_readback`, same growable per-format
      scratch shape). `render_composite` detects
      `src.drawable_id() == dst_id` (or same for mask), pre-allocates
      the alias scratch + extracts its sampled view, then in the
      per-op CB issues `record_copy_from` (dst → alias scratch →
      SHADER_READ_ONLY) before `record_render_composite`. The
      composite descriptor binds the alias view as `src_tex` /
      `mask_tex` in place of dst's own drawable view; Vulkan
      can't sample an image while it's bound as a color
      attachment in the same draw, the scratch breaks the alias.
      `CompositeStats.used_src_alias_scratch` is the observable
      signal. 7 acceptance tests added — 6 in
      `kms::v2::engine::tests::` (`render_composite_over_renders
      _alpha_blended`, `_picture_clip_per_rect`,
      `_solid_fill_source_path`,
      `_disjoint_clear_uses_readback`, `_self_alias`,
      `render_fill_rectangles_src_clears_to_color`); the
      seventh, `v2_render_composite_no_gc_clip_leak`, lives in
      `tests/v2_acceptance.rs` because the "GC clip must not
      leak into RENDER paint" property is a Backend-trait
      invariant (engine has no GC-clip notion). Gradient source
      still bails to 3e. 216 lib tests + 15 ignored v2-engine Vk
      tests + 4 v2_acceptance tests all green under lavapipe;
      clippy pedantic clean for the touched lines.
  - [x] **3d — `render_composite_glyphs` + glyphsets.** Landed
    2026-05-16. v1-parity scope: PictOp Over + SolidFill source
    + A8/A1/ARGB32-as-A8 glyphsets. Fixes v1's latent
    `_clip unused` bug (`kms::backend.rs:5313`) — the dst picture
    clip is now honoured via per-rect scissoring, plan §4.

    `record_text_run` factored into a single-scissor convenience
    wrapper + `record_text_run_scissored` that takes a
    `&[vk::Rect2D]` slice. Same `cmd_begin_rendering` →
    loop `set_scissor` + glyph-draw batches → `cmd_end_rendering`
    shape; v1's existing `record_text_run` site stays at
    full-extent scissor via the wrapper. New
    `RenderEngine::composite_glyphs` mirrors `image_text`'s
    atlas-intern path (same key namespace, atlas_last_upload_ticket
    discipline, FenceTicket-gated per-glyph staging buffers); the
    new `CompositeGlyphInput` borrows pre-A8 glyph pixels from
    `KmsCore.glyphsets` or a per-call A1→A8 scratch.
    `KmsBackendV2::render_composite_glyphs` parses the items
    stream (8-byte elements + inline `0xFF 0 0 0 new_gs_xid`
    glyphset-change form, ids 1/2/4 bytes per CompositeGlyphs8/16/32
    minor), looks up each glyph from `core.glyphsets`, A1→A8
    expands per v1's MSB-first bit order, then calls into the
    engine. Two-pass parse avoids a borrow conflict on the
    A1-expansion Vec (the inner Vec<u8>'s heap buffers are stable
    through outer-Vec pushes, but the borrow checker doesn't
    track that — pass 1 fills the scratch + records indices, pass
    2 resolves slices). Gate: op != Over OR source not SolidFill
    bumps `composite_glyphs_dropped_unsupported`; stale handles
    (picture / glyphset / dst drawable) log gap + return Ok
    without bumping (protocol errors, not unsupported features).
    Telemetry sites: atlas_intern, glyph_uploads,
    glyphs_dropped_atlas_full, paint_submits — same shape as
    image_text.

    3 unit tests: `v2_composite_glyphs_unsupported_op_drops`,
    `v2_composite_glyphs_non_solidfill_source_drops` (uses a
    LinearGradient source so the gate fires at the source-type
    check, not a stale-handle path),
    `v2_composite_glyphs_inline_glyphset_change_parsed` (asserts
    Over+SolidFill stays in-envelope when the items stream
    rotates the active glyphset mid-run). 1 Vk-backed
    acceptance: `v2_composite_glyphs_clip_intersects_picture` —
    paints two 4×4 white glyphs across an 8×4 blue dst with a
    4×4 top-left clip; asserts left half white, right half blue.
    This is the v1-bug-fix gate — v1 paints both glyphs.
    219 lib tests + 15 ignored v2-engine Vk tests + 5
    v2_acceptance tests all green under lavapipe.
  - [x] **3e — trapezoids + triangles + `copy_plane`.** Landed
    2026-05-16 in two substages.

    **3e.1 — copy_plane (`dc3853d`).** GXcopy scope: pull src wire
    bytes via `engine.get_image`, classify each pixel by
    `(pixel & plane) != 0` into fg/bg rect lists, drive
    `poly_fill_rectangle` bg-first then fg. Depths 1/8/24/32
    supported via per-depth wire row-stride + pixel extraction
    (MSB-first bit unpacker for depth-1). Non-GXcopy logs a gap +
    skips (Stage 3f LogicFillPipeline). xfd/xfontsel are the
    canonical callers and both use GXcopy.

    **3e.2 — trapezoids + triangles (TrapPipeline port).** New
    `trap_pipeline: Option<TrapPipeline>` + `mask_scratch:
    Option<MaskScratch>` slots on `RenderEngineInner`, lazy-init
    via `ensure_trap_assets`. New engine method
    `render_traps_or_tris(prim_kind, instance_data, instance_count,
    bbox, ...)` ports v1's `try_vk_render_traps_or_tris`
    (kms/backend.rs:4500) into v2's per-op CB shape:
      1. Allocate per-call `StagingBuffer::new_with_usage(...,
         VERTEX_BUFFER)` (sibling to the existing `TRANSFER_*`
         constructor) sized for `instance_count × stride`; memcpy
         the wrapper-cooked instance bytes in.
      2. `mask_scratch.ensure_image_size_returning_old(bbox_w,
         bbox_h)` — retired old image currently dropped on the
         floor (same shape as `dst_readback` grow-leak; flagged
         in the `mask_scratch` doc note for Stage 5 polish).
      3. Trap rasterize phase inside the CB:
         mask → COLOR_ATTACHMENT, `begin_rendering(mask_view,
         LOAD_OP_CLEAR)` at `(0,0)..(bbox_w, bbox_h)`, bind
         trap-or-tri sibling pipeline + vertex buffer, push
         `TrapDrawPushConsts`, set viewport + scissor, draw
         `(4, instance_count)`, end_rendering, mask
         → SHADER_READ_ONLY.
      4. Composite phase: `needs_full_dst` op set (Clear/Src/etc.
         and every Disjoint/Conjoint variant) drives a full-dst
         draw with `mask_off = -bbox`; other ops draw only the
         bbox. dst_readback snapshot fires for Disjoint/Conjoint.
         Goes through the existing `record_render_composite` with
         the scratch view bound as `mask_tex`, `REPEAT_NONE` so
         out-of-bbox samples yield mask=0. Per-rect picture-clip
         scissoring (plan §4) honoured at the composite stage.
      5. Push the `SubmittedOp` with the instance buffer in
         `staging` so its retirement releases the upload buffer.

    Out-of-scope at 3e.2: gradient sources (Stage 3e gradient
    work is risk-listed for follow-up), src self-alias scratch
    (rare in trap workloads), all the broader op coverage
    Disjoint/Conjoint already accepts via the existing pipeline.

    Backend wiring: `KmsBackendV2::render_trapezoids` /
    `render_triangles_op` decode wire bytes (40 B traps; 24 B
    triangles via minor 11/12/13 dispatch), apply `(x_off, y_off)`
    in 16.16 fixed-point, compute bbox via the existing
    `trapezoid_bbox` / `triangle_bbox` helpers, pre-pack instance
    data, and call into the engine.

    Tests: 2 unit (`trapezoid_decoder_x11_wire_layout`,
    `triangle_to_trap_degenerate`) + 1 Vk-backed acceptance
    (`v2_render_trapezoids_renders_filled_rect` — axis-aligned
    4×4 trap with `Over` + SolidFill source, interior red over a
    blue dst). 221 lib + 15 ignored v2-engine Vk + 7
    v2_acceptance tests all green under lavapipe.
  - [~] **3f — Core remainder + GC.function + planemask +
    acceptance.** Substaged for incremental landing.
    - [x] **3f.1 — poly_* + fill_poly + poly_fill_arc landed
      2026-05-16.** `poly_line` / `poly_segment` /
      `poly_rectangle` / `poly_arc` / `poly_point` /
      `poly_fill_arc` / `fill_poly` all real on KmsBackendV2.
      `bresenham_segment` / `scanline_fill_polygon` /
      `clip_rects_to_image` / `read_i16_pair` / `read_rect`
      promoted from v1's free fns to `pub(crate)` and reused
      verbatim — wire-byte parsing + rasterisation is
      v1-identical, so rendercheck results carry across. v2
      grows `current_clip_rects_in_dst_space` +
      `intersect_with_current_clip` (mirrors v1) +
      `drawable_dims_v2` + `fill_solid_rects`; the latter is
      the shared "lower a list of solid rects to
      `engine.fill_rect`" lowering, with non-GXcopy
      `GcFunction` logged as a gap until 3f.2 lands
      `LogicFillPipeline`. **v1 latent bug fixed in v2 (codex
      plan §4):** v2's `fill_rectangle` + `poly_fill_rectangle`
      now intersect with `KmsCore.current_clip` before
      submitting; v1 has the same call path but bypasses the
      clip helper, so Core paint silently overflows the GC
      clip there. 3 new unit tests:
      `poly_line_origin_mode_offsets_correctly`,
      `fill_poly_scanline_correctness`,
      `poly_fill_rectangle_honours_gc_clip`. 224 lib tests +
      17 ignored v2 Vk tests + 7 v2_acceptance tests all green
      under lavapipe.
    - [x] **3f.2 — LogicFillPipeline landed 2026-05-16.**
      v1's `LogicFillPipelineCache` reused verbatim (no fork —
      the `vk::Format`-bound cache + `(GcFunction, opaque_alpha)`
      sub-key are already format-agnostic). `RenderEngineInner`
      grows `logic_fill_caches:
      HashMap<vk::Format, LogicFillPipelineCache>` (lazy,
      sharded by dst format — BGRA8 is the typical entry; R8
      enters on the rare depth-1/8 logic-fill path). New
      engine method `RenderEngine::logic_fill(target, function,
      opaque_alpha, fg, rects)` ports v1's
      `try_vk_fill_with_function` body into v2's per-op CB:
      ensure_cache → `begin_op_cb` → drawable layout
      transition COLOR_ATTACHMENT_OPTIMAL → `cmd_begin_rendering(LOAD)`
      → `bind_pipeline` → per-rect (`set_scissor` +
      `cmd_push_constants` + `cmd_draw(4, 1)`) →
      `end_rendering` → transition back to SHADER_READ_ONLY →
      `end_and_submit_op` + ticket-clone-onto-Drawable +
      damage. `opaque_alpha` derived from `Drawable.depth !=
      32` (L1 server-α invariant). `KmsBackendV2::fill_solid_rects`
      now diverts to `engine.logic_fill` when `function !=
      Copy`; `copy_plane`'s pre-3f.2 non-`GXcopy` gate dropped
      (decomposes into `poly_fill_rectangle` calls which now
      honour function). 2 new tests:
      `gxcopy_planemask_diverts_to_logic_fill` (logic, asserts
      no gap fires) + `logic_fill_xor_applies_per_pixel`
      (Vk-backed, GXxor on a 4×4 BGRA8 dst — XOR per byte +
      alpha preserved via opaque_alpha pipeline). 225 lib +
      18 ignored v2 Vk + 7 v2_acceptance tests green under
      lavapipe. **Out of 3f.2 scope (v1-parity gap, tracked
      separate):** `put_image_non_gxcopy` and `copy_area_non_gxcopy`
      stay logged-gap — v1 doesn't honour function on these
      either (put_image is a byte copy, copy_area is a Vk
      image-to-image blit; neither passes through the
      fragment-shader stage that LogicOp acts on). Real apps
      use logic-op only on fill paths; rendercheck covers it
      there.
    - [x] **3f.3 — set_clip_pixmap + set_gc_fill_tiled landed
      2026-05-16.** `set_clip_pixmap` stores `ClipState::Pixmap
      { origin, pixmap }` instead of logging a gap +
      clearing the clip (v1-parity bookkeeping; mask sampling
      itself is deferred — no real-app smoke matrix client
      drives clip-pixmap, v1 stores-but-doesn't-enforce too).
      `set_gc_fill_tiled` stores `FillState::Tiled { pixmap,
      origin }` instead of logging a gap (the dispatcher also
      pushes via `apply_fill_state`; keeping the dedicated
      entrypoint correct keeps the Backend trait uniform).
      `fill_solid_rects` split: stroke ops (`poly_line` /
      `poly_segment` / `poly_rectangle` / `poly_arc` /
      `poly_point`) still call it directly (X11 strokes are
      always solid foreground); fill ops (`fill_rectangle` /
      `poly_fill_rectangle` / `poly_fill_arc` / `fill_poly`)
      now go through new `fill_rects_honoring_fill_state`,
      which dispatches `FillState::Tiled` + GcFunction::Copy
      to `try_tiled_fill` (engine.render_composite with
      `OP_SRC` + `Repeat::Normal` + `(src_x, src_y) = (dst -
      tile_origin)`). Non-Copy + Tiled and `Stippled` /
      `OpaqueStippled` degenerate to solid (v1-parity).
      Tile aliasing (`tile_id == dst_id`) and non-BGRA8 tile
      formats fall back to solid. 2 new unit tests
      (`set_clip_pixmap_stores_pixmap_clip`,
      `set_gc_fill_tiled_stores_fill_state`) + 1 Vk-backed
      acceptance (`v2_tiled_fill_replicates_tile_pixmap` —
      4×4 dst pre-filled blue, 2×2 red tile, `apply_fill_state(Tiled)`
      + `poly_fill_rectangle` over whole dst, asserts every
      pixel reads red). 227 lib + 18 ignored v2 Vk + 8
      v2_acceptance tests green under lavapipe.
    - [x] **3f.4 — cursor + `xfixes_change_cursor_by_name`
      landed 2026-05-16.** Closes the pre-Stage-4 cursor
      stubs: `create_cursor` / `create_glyph_cursor` /
      `render_create_cursor` / `define_cursor` /
      `xfixes_change_cursor_by_name` all return without
      logging `v2:` gaps. Handles are well-formed (xid minted
      via `core.next_host_xid()` for the three create paths),
      so Cairo / GTK / Qt theme-cursor clients see a clean
      reply and don't trip on a zero handle. Pixel
      rasterisation + scene blit stays Stage 4 territory (no
      cursor scene-layer yet; cursor defaults to bare KMS HW
      cursor — see spec § scene layering "Cursor — always on
      top"). `xfixes_change_cursor_by_name` is a v1-parity
      no-op (yserver has no cursor-theme registry; the name
      hint is silently dropped). 1 unit test
      (`cursor_paths_do_not_log_gaps`) asserts the closure.
      228 lib + 18 ignored v2 Vk + 8 v2_acceptance tests green
      under lavapipe.
    - [x] **3f.6 — subwindow scene composition landed
      2026-05-16.** `WindowGeometryV2` grows `parent:
      Option<u32>` + `bg_pixel` + `bg_pixmap`.
      `create_subwindow` records the passed-in parent xid +
      bg-pixel; `reparent_subwindow` updates parent on tree
      moves. `change_subwindow_attributes` ports v1's
      `CWBackPixmap` (0x01) + `CWBackPixel` (0x02) parse —
      pre-3f.6 stub logged a `v2:` gap, now stores the values
      into `windows_v2[xid]`. `allocate_window_storage`
      (called from `create_subwindow` / `register_top_level`
      / `register_subwindow`) clears fresh storage to
      `bg_pixel` via `engine.fill_rect` so freshly-mapped
      windows have a defined initial colour. `configure_subwindow`
      re-fills bg_pixel on resize.

      `build_scene` factored: top-level iteration calls a new
      `emit_window_subtree` recurse that walks top-level →
      mapped descendants, projecting through accumulated
      parent offsets into output coords. Unmapped parents
      short-circuit the entire subtree per X11 MapWindow
      cascade semantics. Sibling z-order between children
      of the same parent is HashMap-iteration-order — proper
      stack tracking is post-3f.6 polish (most real apps have
      one child per parent so the gap rarely matters at
      Stage 3).

      4 new tests:
      `build_scene_recurses_into_mapped_children` (parent +
      child draw entries with absolute coords),
      `build_scene_unmapped_parent_hides_subtree` (cascade),
      `change_subwindow_attributes_stores_bg_state`
      (value-mask 0x03 lands both fields; bookkeeping no
      longer logs a gap),
      `create_subwindow_records_parent_and_bg_pixel` (parent
      xid + bg-pixel hint stored on the geometry record).

      232 lib + 18 ignored v2 Vk + 8 v2_acceptance tests
      green under lavapipe. Hardware smoke (xterm under no-WM,
      xclock) pending — runs at 3f.5.
    - [x] **3f.7 — input dispatch landed 2026-05-16.**
      `on_host_input` + `cook_host_key` +
      `process_pointer_absolute` + `process_pointer_button` +
      the 7-helper cluster (`serialize_modifiers`,
      `window_under_cursor`, `event_relative_coords`,
      `emit_pointer` / `emit_crossing` / `emit_motion_only`,
      `update_pointer_window`, `dispatch_motion_event`) all
      ported on `KmsBackendV2`. v1's body went over almost
      verbatim — KmsCore-only helpers (xkb_state,
      cursor_x/y, button_mask, pending_pointer_events,
      xid_map) port byte-for-byte; the three v1-specific
      touches got v2-shape substitutions:
      - `self.windows` → `self.windows_v2` (uses the parent-
        aware geometry record landed in 3f.6 for hit-testing).
      - `self.fb_w` / `self.fb_h` → `self.platform.outputs[0]`
        extent (single-output is the only path exercised;
        multi-output input mapping is risk-listed for later).
      - HW cursor calls (`hw_cursor_active` / `hw_cursor_move`
        / `hw_cursor_refresh`) → no-op + `scene.mark_scene_structure_dirty`
        per then-current spec § I7. This was later superseded by
        the v2 HW cursor implementation; Stage 5 now treats HW
        cursor as prerequisite work and focuses on perf closure.

      6 new unit tests: `serialize_modifiers_zero_on_fresh_state`,
      `cook_host_key_fills_coords_and_modifier_state`,
      `process_pointer_button_state_field_is_pre_press`
      (X11 spec pre-press `state` discipline + post-event
      `button_mask` update),
      `process_pointer_absolute_clamps_to_output`,
      `window_under_cursor_finds_topmost_mapped`
      (back-to-front walk + unmapped-skip),
      `on_host_input_does_not_log_gap`.

      238 lib + 18 ignored v2 Vk + 8 v2_acceptance tests
      green under lavapipe; clippy clean.

      Software cursor lands as 3f.8 below so hardware smoke has
      visible pointer feedback; the full theme/`define_cursor`
      pipeline stays Stage 4.
    - [x] **3f.8 — software cursor sprite landed 2026-05-16.**
      Stage-4 preview so the user has visible pointer feedback
      during 3f.5 smoke. `SceneCompositorInner` grows
      `cursor: Option<CursorEntry>`; `SceneCompositor::register_cursor`
      records a (DrawableId, extent, hotspot) tuple. `build_scene`
      grows a `cursor: Option<CursorEntry>` parameter; when
      `Some`, appends a top-of-z `CompositeDraw` at
      `(cursor_x - hot_x, cursor_y - hot_y)` with
      `alpha_passthrough=true` so the sprite's transparent
      border actually blends.

      `KmsBackendV2::init_cursor_sprite` (called from `open`)
      allocates a 16×16 BGRA8 Pixmap-kind Drawable, uploads a
      baked default-arrow sprite (12-row right-triangle, black
      fill, 1-px white diagonal outline) via `engine.put_image`,
      and registers it on the scene. Best-effort — failure
      logs + leaves the cursor invisible (no regression).

      Full theme support + per-window `define_cursor` swap-in +
      `xfixes_change_cursor_by_name` integration stays Stage 4.
      `process_pointer_absolute` already calls
      `scene.mark_scene_structure_dirty` on every motion so the
      cursor draw refreshes per tick.

      1 new unit test
      (`build_scene_appends_cursor_draw_at_top_of_z` —
      cursor draw is the last `scene.draws` entry at the
      expected position with `alpha_passthrough=true`).

      239 lib + 18 ignored v2 Vk + 8 v2_acceptance tests green
      under lavapipe; clippy clean.

      Bonus fix in the same range: v2's `get_keyboard_mapping`
      and `get_modifier_mapping` stubs were returning empty
      tables. Hardware smoke after 3f.7 showed xterm taking
      pointer events but dropping every keypress because xlib
      saw zero keysyms per code; v1's body ported verbatim
      (commit `6b0ffb6`).

      *Post-3f.8 hardware smoke surfaced two follow-up bugs
      that 3f.9 (below) fixes — the cursor trail and the
      ENOMEM → DEVICE_LOST cascade. The bare-3f.8 cursor draw
      is correct in isolation but unsound at scene level
      without root storage; the buffer-age clipped/LOAD path
      left prior cursor pixels in inter-window gaps, and the
      failed-commit path was leaking pinned resources.*
    - [x] **3f.9 — scene + commit recovery + root storage
      landed 2026-05-16 (codex review/fix).** Three root-cause
      fixes for the post-3f.8 hardware-smoke regressions, all
      under the v2 spec's invariants:

      - **Root storage promoted forward.** Spec §scene-
        layering item 1 ("Root storage — always") was deferred
        to Stage 4 in our stage plans but the cursor-trail
        diagnosis proved the deferral was unsound under
        buffer-age clipping. `KmsBackendV2::init_root_storage`
        allocates a virtual-screen-extent BGRA8 drawable as
        `DrawableKind::Root`, fills it with `bg_pixel`, and
        marks it scene-participating. `build_scene` now
        prepends a root-layer `CompositeDraw` at the bottom
        of z before walking top-levels. `set_container_background_pixel`
        / `set_container_background_pixmap` repaint root
        storage via `engine.fill_rect` /
        `engine.copy_area` instead of just mutating
        `core.bg_pixel`. The cursor trail is gone because
        there are no longer gap regions between scene draws —
        root covers everything, so the buffer-age repaint
        re-blits the area behind the prior cursor every tick.
        Means 3f.6's "trail fix" via prior-rect damage was
        the wrong layer; the right layer was the missing
        scene-layering bottom entry.

      - **`FenceTicket` false-positive leak detection.**
        `FenceTicketInner::drop` previously checked only the
        `signaled_cache` bool, which is set explicitly via
        `poll_signaled`/`update_signaled` calls. Tickets that
        dropped signaled-but-stale-cache logged
        `FenceTicket: leaked unsignaled fence ... renderer_failed
        will be set` and flipped `platform.renderer_failed =
        true` — once that happens, every engine + scene op
        returns `RendererFailed` and the screen freezes.
        Fix: drop falls back to
        `vk.device.get_fence_status(self.fence)` when the
        cache says false, and updates the cache. This was the
        proximate cause of the "screen freezes after a couple
        minutes" symptom in the post-3f.8 logs.

      - **Failed-atomic-commit BO recovery.** The pre-3f.9
        recovery path did `bo.state = BoState::default()`
        immediately after a failed flip — but the GPU had
        already executed `queue_submit2` writing into that
        BO. Next `acquire_scanout_bo` could hand back the
        same BO for recording while GPU writes were still in
        flight, and the OUT_FENCE_PTR (defensively set to
        `-1`, but some drivers/kernels do write it alongside
        an error) was never closed. Under RADV/bee this
        accumulated sync_file fds in the kernel until
        atomic-commit started returning ENOMEM, which the
        prior recovery path then busy-looped retrying, which
        eventually triggered the GPU context-lost
        (`VK_ERROR_DEVICE_LOST`).
        Fix:
        - `record_compose_v2` takes a `signal_fence` (the
          compose fence) + signals `gpu_submitted: &mut bool`
          to the caller. Stops resetting BoState on failure.
        - New `failed_submit_bos: VecDeque<FailedSubmitBo>`
          per `OutputSceneState` parks each
          (bo_idx, pool_slot, compose_ticket) so the BO + the
          descriptor-pool slot survive the failed flip.
        - New `retire_failed_submit_bos` polls each ticket
          every tick; once signaled, calls the new
          `platform.recycle_failed_submit_bo(output_idx, bo_idx)`
          helper (which resets `BoState` properly) and
          releases the pool slot.
        - `next_submit_retry_at` adds a 100 ms commit-retry
          back-off so we don't spin re-submitting in the
          failure window (with `TODO(stage-5 perf)` note for
          per-driver tuning).
        - Defensive: closes `out_fence` if the kernel returns
          one alongside the error.

      Additional polish landed in the same diff:
      - **`do_dump_scanout_v2`** (~150 LoC): real PPM
        readback via Vk staging buffer + image-to-buffer
        copy; was a `log_v2_gap` before. `yserver-v2-scanout-<n>-out<i>.ppm`
        per dump.
      - **`drain_page_flip_events`**: drains DRM events and
        returns per-output indices; `on_page_flip_ready` only
        ticks the outputs that actually flipped (was looping
        over all outputs blindly).
      - **`SceneCompositor::wake_for_damage`** (cheap dirty
        bit) vs **`mark_scene_structure_dirty`** (full-output
        rect) split — cursor motion + `mark_dirty` use the
        cheap path; map/unmap/configure/restack/redirect/
        root-background changes use the coarse path.
      - **`add_projected_damage`** helper clips to output
        extent before adding to `RegionSet` (was missing —
        could push out-of-bounds rects into the scissor list).
      - **Compose fence threading**: `PendingAck.ticket` was
        `None` before. Now a real `FenceTicket` is stored and
        `touch_render_fence` runs against every sampled
        drawable so retire can't free them mid-compose. This
        closes a latent I6a hole.
      - **Telemetry additions**: `compose_cb_record_ns`,
        `damage_pixels`, `scene_entries`,
        `full_redraw_fallback`, `descriptor_allocations`,
        `missed_pageflip`. Most listed as "Required counters"
        in the spec's perf gates section but never wired.

      239 lib + 18 ignored v2 Vk + 8 v2_acceptance tests
      green; clippy clean. Hardware smoke green per
      user-report (cursor tracks, server no longer locks up).

      Open follow-ups:
      - **No unit tests for the failed-submit recovery
        path** — `retire_failed_submit_bos`,
        `recycle_failed_submit_bo`, the back-off gate all
        landed without coverage. Hard to test without a
        fault-injection harness, but a synthetic "atomic
        commit fails N times" test would catch regressions.
      - **100 ms back-off is hardcoded** with `TODO(stage-5
        perf)` — should become a tunable + observable via
        `commit_retry_backoff_ms` telemetry counter for
        per-driver tuning.
      - **`set_container_background_pixmap`** uses
        `copy_area` which doesn't honour X11 bg_pixmap
        tiling. v1-parity for now; proper tiling is a
        follow-up.
    - [x] **3f.10 — port v1's PixmapPool to v2 landed
      2026-05-16.** MATE hardware-smoke (post-3f.9) showed
      ~90 pixmap allocate/free cycles per second (3447
      CreatePixmap + 3422 FreePixmap over 38 s). Without a
      pool, every CreatePixmap pays a full
      `vkCreateImage` + `vkAllocateMemory` + `vkBindImageMemory`
      + `vkCreateImageView` cycle, freed symmetrically per
      `FreePixmap`. v1's `kms::vk::pixmap_pool::PixmapPool`
      already has the right shape — bucket-cap=32,
      max_pooled_dim=128, keyed by (w, h, format), thread-safe
      Mutex internals — and ported verbatim:
      - `PlatformBackend` grows
        `pixmap_pool: Option<Arc<PixmapPool>>` (None on the
        test fixture; Some on `open_with_commit`).
        `register_for_telemetry` runs so the existing
        `GLOBAL_LATEST_POOL` telemetry hook also surfaces v2
        pool stats.
      - `Storage` grows a `from_pooled` constructor that
        adopts a recycled (image, view, memory) triple +
        inherits the pool entry's tracked `current_layout`
        so the next op's barrier issues the correct
        `old_layout`.
      - `PlatformBackend::allocate_drawable_storage` tries
        `pool.try_take(key)` before falling through to fresh
        Vk allocate.
      - `Storage::destroy` tries `pool.try_return(key, entry)`
        before destroying handles; bucket-full / ineligible
        falls through to synchronous destroy.
      - `PlatformBackend::disable_output` calls `pool.drain()`
        after `device_wait_idle` so recycled triples don't
        leak across VkContext teardown.

      1 new unit test
      (`storage_from_pooled_inherits_layout_and_dims` —
      pool-take inherits SHADER_READ_ONLY_OPTIMAL not the
      fresh-alloc UNDEFINED). Pool-internal logic was already
      tested in `kms::vk::pixmap_pool::tests` from v1.

      240 lib + 18 ignored v2 Vk + 8 v2_acceptance tests
      green under lavapipe; clippy clean. Hardware smoke
      gate: pool telemetry counters (`total_takes_hit`,
      `total_takes_miss`) under MATE should show high hit
      rate after 30 s of GTK churn — Stage 3f.5 will capture.

      Out-of-scope: pool stats are not yet emitted on v2's
      per-second telemetry line. `GLOBAL_LATEST_POOL` hook is
      registered, so the existing v1 emitter sees them too;
      a v2-side per-second emitter line is post-3f.5 polish.
    - [x] **3f.11 — reparent removes from top_level_order +
      ConfigureWindow stack_mode for top-levels landed
      2026-05-16.** Two related fixes after the MATE clock-
      applet PPM dump showed a left-edge ghost of the right-
      edge clock:
      - `reparent_subwindow` now reconciles
        `core.top_level_order` with the resolved parent. Pre-
        fix, an xid registered as a top-level (parent=root)
        stayed in the order even after being reparented INTO
        another container. `build_scene` then emitted the
        same window twice — once at child-relative coords via
        the top-level walk (treated as absolute → (0,0)) and
        once at its real position via the parent's recurse.
        MATE clock-applet was the reproducer: created with
        parent=root, reparented into mate-panel's container,
        rendered at both edges. Click on the LEFT ghost hit-
        tested against the RIGHT clock's geom (since geom.x
        was correctly updated by reparent) — visual at left
        + input-route at right confirmed the same-window-
        emitted-twice diagnosis.
        Belt-and-suspenders fix: `destroy_subwindow` also
        drops from top_level_order so a destroyed top-level
        doesn't ghost-render until the next register_top_level
        fills the slot.
      - `configure_subwindow` honours `stack_mode`: caja-
        desktop occluded mate-panel because v2 dropped the
        `stack_mode` field from `HostSubwindowConfig`. marco's
        `ConfigureWindow stack_mode=Below` was a no-op in v2;
        caja-desktop (last-registered top-level) drew on top
        of everything. Ported v1's `restack_window` for the
        top-level case — Above/Below/TopIf/BottomIf/Opposite
        all route through `core.top_level_order`. Subwindow
        sibling stack order is still HashMap-iteration-order
        (post-3f.11 polish; doesn't affect caja-on-top).
      Tests: 5 new unit tests covering both bugs +
      restack-corner cases. 245 lib tests green; clippy
      clean. Hardware smoke confirmed by user:
      cursor tracks, MATE panels render correctly, caja-
      desktop stays beneath.
    - [x] **3f.12 — gradient src/mask collapses to first-stop
      SolidFill landed 2026-05-16.** MATE caja PPM dump
      revealed caja's offscreen render buffer was painted
      black-with-isolated-widget-rects. Cause: every Cairo
      widget background uses an XRender Composite op with a
      gradient source; v2's `render_composite` logged a gap
      and bailed; caja's pixmap stayed mostly UNDEFINED Vk
      content; CopyArea propagated that to the on-screen
      window.

      Stage planning miss (same shape as 3f.6 / 3f.7): a 3c.2
      comment promised "gradient src bails to 3e", but 3e's
      actual plan was trapezoids + triangles + copy_plane and
      never picked up gradients. Gradients are mentioned only
      in 3e's Risks list as "risk-listed for follow-up." Fell
      through the gap.

      Pragmatic fallback (not the real fix): in
      `resolve_picture_for_render`, collapse a LinearGradient
      / RadialGradient picture to a SolidFill of its first
      stop's premultiplied colour. The existing SolidFill
      path in `engine.render_composite` already works end-to-
      end. Most GTK gradients are mild light→lighter so flat
      first-stop colour is visually approximate. Same
      collapse benefits `render_composite_glyphs` — used to
      drop with `composite_glyphs_dropped_unsupported++`,
      now gradient glyph paints flow through.

      v1's `kms::vk::gradient::GradientPicture` (256×1 LUT
      for linear, 256×256 for radial) is fully built and in
      tree — just not wired into v2. **Real fix tracked as
      3f.13 below.**

      245 lib tests pass; clippy clean. Test renamed:
      `v2_composite_glyphs_non_solidfill_source_drops` →
      `v2_composite_glyphs_gradient_source_collapses_to_solidfill`
      (counter stays at 0 since gradient now flows through).
    - [x] **3f.13 — full gradient LUT sampling landed
      2026-05-16 (`5031e39`).** v1's `GradientPicture` (linear
      256×1, radial 256×256) wired into v2's
      `engine.render_composite` + `render_traps_or_tris`.
      `render_create_linear_gradient` /
      `render_create_radial_gradient` eagerly build a
      `GradientPicture` and store on
      `engine.picture_paint[xid]`; `ResolvedSource::Gradient`
      arms bind the gradient image_view + extent and compose
      the `axis_projection` affine with the user transform.
      `render_free_picture` drops the entry via the existing
      `picture_paint_remove` hook. `composite_glyphs` path
      keeps the 3f.12 first-stop SolidFill collapse — glyph
      pipeline is SolidFill-only — but no longer bumps
      `composite_glyphs_dropped_unsupported` (factored into
      `first_stop_premul_of_gradient` helper). 5 new tests (3
      Vk-backed: linear ramp pixel-correctness, radial centre+
      rim, missing-picture gap; 2 logic: resolve-as-gradient,
      free-drops-record). Fuji hw-smoke confirmed gradient
      rendering by user 2026-05-16.
    - [x] **3f.14 — bg_pixmap tiling + window-storage init
      landed 2026-05-16 (`408e197`).** Two fixes from post-
      3f.10 smoke:
      - `set_container_background_pixmap` routes through
        `engine.render_composite` with `OP_SRC + Repeat::Normal`
        across full root extent (single submit), not a single
        copy_area at (0,0). fvwm3 wallpaper now tiles edge-to-
        edge.
      - `default_window_init_color(depth)` paints fresh window
        storage when `bg_pixel == None` (caja drag artefact
        from 3f.10 pool-take). Depth-32 → transparent black
        (premul no-op for compositing); other depths → opaque
        black. Applied in both `allocate_window_storage` and
        the `configure_subwindow` resize path.
      3 new tests (2 Vk-backed, 1 logic).
    - [x] **3f.15 — submit aggregation for stroke ops landed
      2026-05-17.** PolySegment / PolyLine / PolyRectangle /
      PolyArc / PolyPoint fan-outs no longer pay O(N) submits.
      New `RenderEngine::fill_rect_batch(target, color, &[Rect2D])`
      records every rect into a single `cmd_clear_attachments`
      call (Vulkan natively accepts a `ClearRect` slice) inside
      one CB + one queue submit + one `SubmittedOp`. Single
      layout-transition pair per batch instead of per rect.
      `RenderEngine::fill_rect` keeps its one-rect signature for
      the create_pixmap / bg_pixel / image_text-bg / root-init
      / window-init call sites by delegating to `fill_rect_batch`
      with a 1-slice. `KmsBackendV2::fill_solid_rects` (the
      shared lowering for every solid stroke op) drops its
      per-rect for-loop in the `GcFunction::Copy` arm and calls
      `fill_rect_batch` with the full slice; `record_paint_submit`
      fires once per call, matching the non-Copy `logic_fill`
      path's shape. Zero-sized rects are filtered up-front so an
      all-empty batch never burns a CB / fence ticket. Non-Copy
      logic-op stroke ops were already coalesced by
      `logic_fill` (3f.2); no change there.

      Effect on the worst-case workload: an 8-segment
      `PolySegment` request that pre-3f.15 drove ~50 paint
      submits now drives 1. fvwm3 drag stutter + caja "hang"
      should both ease; Stage 3f.5 hardware smoke is the
      load-bearing gate.

      2 new tests: `fill_rect_batch_one_submit_for_n_rects`
      (engine-level Vk-backed: 3 disjoint rects on a 16×4 BGRA8
      pixmap pre-cleared blue, asserts `inner.submitted` grew
      by exactly 1 across the call + pixel-correct per-rect red
      + blue background); `v2_poly_segment_coalesces_to_one_submit`
      (acceptance Vk-backed: drives 8 segments via
      `Backend::poly_segment`, asserts `telemetry.lifetime
      .paint_submits` delta is 1 and `queue_submit2` delta is
      1). 249 lib + 23 ignored v2 Vk + 16 v2_acceptance tests
      green under lavapipe; clippy clean. (The 3f.15 close run
      surfaced 3 pre-existing Vk-test flakes — 2 gradient
      pixel mismatches + 1 SIGSEGV in
      `set_container_background_pixmap` — all triaged + fixed
      in the same window; see entries below.) Hardware smoke
      (fvwm3 drag,
      caja-on-mate) deferred to 3f.5.

  ### Shared-Vk and v2-storage fixes landed 2026-05-16/17
  ### (from xeyes-on-mate-marco hardware smoke)

  All Vk path; v1 and v2 share the underlying code (shaders +
  `MaskScratch` + `DrawableStore`). xeyes was the load-bearing
  reproducer — it exercises every weak point: SolidFill traps,
  shape-clipped offscreen pixmaps, Present-Pixmap, and rapid
  resize via the WM frame.

    - [x] **MaskScratch IDENTITY attachment view (`dae5769`).**
      `MaskScratch` viewed its R8 image with `a=R` swizzle for
      the composite-side mask sample. The SAME view was bound as
      a color attachment for the trap rasterize phase. Vulkan
      VUID-VkFramebufferCreateInfo-pAttachments-00891 requires
      IDENTITY swizzle on attachment views; lavapipe is lenient,
      Intel + RADV strict — the rasterize writes were undefined
      (typically zero) → mask sampled 0 → trap composite added
      nothing to dst. xeyes' pupils never appeared on hardware;
      eye whites (different geometry / coverage) sometimes did.
      Fix: two views on the same image — `view` (a=R swizzle)
      for sampling, `attachment_view` (IDENTITY) for the
      attachment binding. Wired in both v2 (`engine.rs`) and v1
      (`kms/backend.rs`). 3 new acceptance tests
      (back-to-back-trap-different-SolidFill, large-bbox, single
      trap).
    - [x] **Synthetic-1×1 `REPEAT_PAD` override in
      `render_traps_or_tris` (same commit).** Mirrors
      `render_composite`'s existing override. SolidFill sources
      with `Repeat::None` were sampling a 1×1 src image with
      shader-side `REPEAT_NONE` — UV outside `[0, 1]` returns 0
      from `apply_repeat`. Fragments at `dst_offset > 0` zeroed
      the source; the composite added nothing. Latent — only
      surfaced once the MaskScratch swizzle fix made the
      rasterized mask non-zero.
    - [x] **`trap.frag.glsl` horizontal-edge AA off-by-0.5
      (`e76a6f6`).** `c_top = clamp(p.y - top, 0, 1)` was the
      formula; should be `clamp(0.5 + (p.y - top), 0, 1)` to
      match the slanted-edge convention (pixel center on edge =
      0.5 coverage). Adjacent stacked traps sharing a non-
      integer Y boundary (xeyes' eye whites are 16 such
      trapezoids) under-covered the shared row by ~0.7. Visible
      as horizontal stripes inside the eye whites. Same shader
      shipped with v1; v1 had the bug too. Regression test
      `v2_adjacent_trapezoids_share_horizontal_boundary_cleanly`.
    - [x] **`decref → PendingFence` detaches `by_xid` (`5027cc2`).**
      When the parked drawable's xid mapping stayed alive,
      `configure_subwindow`'s `decref → alloc(same_xid)` got
      `XidInUse` → silently kept old storage. xeyes resize
      visibly broken.
    - [x] **`destroy_now` only removes `by_xid` if still mapped
      to this id (`4115fc8`).** Follow-on to 5027cc2. When the
      parked old drawable's fence finally signaled,
      `destroy_now`'s blanket `by_xid.remove(xid)` nuked the NEW
      drawable's xid mapping. Scene then couldn't find the
      resized window storage; stale prior content surfaced.
    - [x] **`detach_xid` runs unconditionally on resize
      (`3afa5bd`).** A Picture wrapping a window holds an extra
      refcount on the backing drawable; `decref` returned
      `StillReferenced` and left `by_xid` intact. Re-alloc
      failed with `XidInUse`. New `DrawableStore::detach_xid`
      removes the mapping without touching refcount;
      `configure_subwindow` calls it before the
      decref + allocate sequence. Picture's next
      `store.lookup(xid)` returns the NEW DrawableId (matches
      X11 RENDER semantics).
    - [x] **`create_pixmap` zero-fills new storage (`fcd2521`).**
      X11 leaves pixmap content "undefined" but real X servers
      get away with it because system allocators zero pages.
      Vk DEVICE_LOCAL memory is fully undefined (random
      recycled GPU bytes). xeyes uses SHAPE-clipped drawing
      into an offscreen pixmap; the non-eye-shape pixels of the
      pixmap held garbage; Present-Pixmap copied that to the
      window storage; massive visible noise around the eyes.
      Fix: `engine.fill_rect` on every `create_pixmap` with
      `default_window_init_color(depth)`. Regression test
      `v2_fresh_pixmap_reads_back_zero`.
    - [x] **Dispatch default flipped to v2 (`3afa5bd`).** Status
      doc claimed v2 was the boot default but `dispatch.rs`
      had v1 as the fallthrough. Three consecutive hardware-
      smoke sessions silently tested v1 because the `yserver-*-hw`
      Justfile recipes don't set `YSERVER_RENDER_MODEL`. Now:
      unset / empty → v2; `=v1` is the explicit fallback.

  ### Open follow-up from 2026-05-17 smoke (not yet diagnosed)

    - [ ] **xeyes resize-DOWN artefact on mate + marco.**
      Resize-UP works clean after the above fixes. Resize-DOWN
      shows xeyes with eyes drawn for a *wider* geometry than
      the current window — eye 2 visibly cut off at the right
      edge. No new v2 warnings during the shrink other than
      marco's existing COMPOSITE-related gaps (`name_window_
      pixmap` stubbed `Err`, Stage 4 territory). Two
      hypotheses, neither confirmed:
      1. xeyes internal state stale — its eye geometry trails
         the pixmap dims on rapid drag-shrink. Would be an
         xeyes-side race; verify by stopping the drag and
         waiting 2-3 seconds before dumping the scanout (if the
         eyes resolve to the correct smaller shape, it's this).
      2. v2 scene compositor blits stale storage / mismatched
         storage extent vs window geom. The `decref → detach →
         alloc` chain seems correct but might still have a
         pending-ack path that captures the old DrawableId.
      Also visible during the shrink: many `render_composite
      gap: host_src 0x40xxxx not resolvable` lines from marco's
      decoration compositing — depends on `name_window_pixmap`
      (stubbed `Err` on v2). Real fix is Stage 4; the noise
      isn't a v2 regression.
    - [ ] **MATE panel flicker on v2.** Reported during the
      same session, not yet diagnosed. Could share a root cause
      with the xeyes-shrink bug (rapid configure_subwindow on
      panel applet activity) or be its own scene-damage issue.
      Worth capturing a focused trace+log when picked up.
    - [x] **Three Vk-test flakes triaged + fixed 2026-05-17**
      (surfaced during the 3f.15 close run; all were pre-
      existing on the 3f.14 baseline, not 3f.15 regressions):
      1. `render_composite_linear_gradient_horizontal_two_stop`
         + `render_composite_radial_gradient_centred` — both
         tests passed `pos: i32::MAX` for the second stop.
         `Stop::pos` is documented as X11 16.16 fixed-point in
         [0, 1], so 1.0 = `0x10000`. `sample_stops` builds
         `target = t * 65536` and lerps in `[lo.pos, hi.pos]`;
         with `hi.pos = i32::MAX` the lerp's `local =
         (target - lo.pos) / (hi.pos - lo.pos)` ≈ 0 at every
         LUT index, so every output pixel read the first stop
         (black). Hardware-smoke worked because real clients
         send proper 16.16 positions. Fix: change `i32::MAX`
         → `0x10000` in both tests.
      2. `v2_set_container_background_pixmap_tiles_across_root`
         — SIGSEGV. `for_tests_with_vk` called `for_tests`
         first, which ran `init_root_storage` with no Vk
         attached and stamped a `for_tests_null` stub
         (`vk::ImageView::null()`) into the store. The
         second `init_root_storage` after Vk attach
         short-circuited on the existing xid mapping, leaving
         the null-view root in place. `render_composite`
         then bound the null view as a color attachment →
         segfault inside the descriptor-set bind. Fix:
         extract `for_tests_seed` (no root init), have
         `for_tests_with_vk` call seed-only, attach Vk +
         engine, then `init_root_storage`. `for_tests` still
         does the immediate init for the no-Vk path.

    - [x] **3f.5 — acceptance + Stage 3 close 2026-05-17.**

      **xts5 `Xproto`** (`just xts-yserver`, vng + virtio-gpu
      KMS): **358 PASS / 6 FAIL / 4 UNRES / 19 UNTST** out of
      389 test purposes (92%). Same v1 (forced via
      `YSERVER_RENDER_MODEL=v1`) run: **bit-identical
      358/6/4/19** — confirms the Stage 1a `KmsCore` extraction
      is faithful end-to-end on the protocol surface. Remaining
      FAILs cluster on font metadata (`pListFonts*`,
      `pSetFontPath`, `pPolyText8/16`) + `pGetImage` +
      `pBadRequest`; UNRES on `pPutImage` (×3). None touch the
      v1/v2 divergence point.

      **rendercheck**: not a Stage 3 gate. The suite enumerates
      RENDER ops through COMPOSITE-shaped flows that hit the
      still-stubbed `name_window_pixmap` (Err) on v2 — that's
      explicitly Stage 4 territory. rendercheck against v2 today
      aborts 12 of 13 categories; re-runs after Stage 4 lands
      `set_redirected_target` + real `NameWindowPixmap`. v1's
      historical 100% on rendercheck reflects v1's per-window-
      mirror path that exposes storage directly without
      compositor-style aliasing — exactly the model mismatch
      v2 rewrites.

      **Real-app hardware-smoke matrix** (bee + fuji, v2, KMS):
      xterm-under-fvwm3, xclock, xeyes (post-shader-fix), gedit,
      MATE-cc, xfce4-no-compositor, xfd all PASS. marco / xfwm4
      with compositor hit `name_window_pixmap` gaps (Stage 4).
      Later Stage 4d follow-up smokes reached MATE + XFCE
      desktop/window rendering. Cinnamon/muffin then failed early
      with `Could not initialize XSync counter`; the captured trace
      showed SYNC `Initialize` itself shaped like Xorg, but every
      affected client received an all-zero minimal XKB
      `PerClientFlags` reply immediately before SYNC setup.
      2026-05-19 fix: XKB minor 21 now returns a structured
      `xkbPerClientFlagsReply` advertising `XkbPCF_AllFlagsMask`
      and echoing changed supported flags, on both v1 and v2 KMS
      paths. A follow-up smoke still logged the same muffin XSync
      complaint, but the session advanced far enough to map
      nemo-desktop. The new trace showed nemo-desktop creating a
      full-screen depth-32 ARGB top-level, while v2 registered the
      host storage as depth 24, so its transparent desktop surface
      became an opaque black cover over the root background. KMS now
      carries a depth-only visual selector when there is no upstream
      host X server visual / colormap to translate, and v1/v2 KMS
      consume that selector directly so ARGB top-levels allocate
      depth-32 storage.
      Next Cinnamon smoke: background stayed visible and
      cinnamon-settings rendered, but without decorations. Logs show
      muffin starts as the WM, sends `SYNC::ListSystemCounters`, gets
      an empty system-counter list, prints `Could not initialize
      XSync counter`, and exits before managing the settings window.
      2026-05-19 follow-up: SYNC now advertises a Xorg-compatible
      `SERVERTIME` system counter (resolution 4 ms) and
      `QueryCounter(SERVERTIME)` returns the server timestamp. The
      follow-up trace showed muffin receiving `SERVERTIME` but still
      exiting immediately because mutter's idle monitor searches for
      the `IDLETIME` system counter during XSync startup. SYNC now
      advertises both `SERVERTIME` and `IDLETIME` with 4 ms
      resolution; `QueryCounter(IDLETIME)` currently returns the
      server timestamp as a minimal monotonic stand-in until full idle
      accounting / alarm delivery is needed.
      Next Cinnamon smoke reached Muffin's RANDR monitor setup, then
      crashed with `meta_settings_get_ui_scaling_factor:
      settings->ui_scaling_factor != 0`. The trace showed the first
      `RANDR::GetScreenResourcesCurrent` reply carrying
      `timestamp=0` / `config-timestamp=0`; Muffin's XRandR backend
      compares the resource timestamp with its own initial
      last-config timestamp (also zero) and takes a fast path that
      reads UI scaling before settings post-init has populated it.
      RANDR initialization now clamps zero resource timestamps to 1 so
      the initial server layout is not mistaken for a completed
      client-side reconfiguration. Follow-up xserver comparison also
      showed `GetMonitors` should mark active server monitors as
      `automatic=TRUE`; yserver was hardcoding that flag false, which
      could make Muffin treat the layout as manual/fallback instead of
      the live server geometry. That flag now matches Xorg. Muffin
      also probes `RANDR::QueryOutputProperty` for connector atoms
      such as `EDID`, `ConnectorType`, and `Backlight`; yserver was
      leaving that minor opcode unhandled, so it now returns a proper
      `BadName` error for unsupported output properties instead of
      silently eating the request.
      Next Cinnamon smoke uncovered muffin exiting early with
      `Window manager error: Mutter requires XFixes 5.0` in
      `cinnamon.log`. The xtrace showed muffin sending
      `XFIXES::QueryVersion major=6 minor=0` and yserver replying
      `major=2 minor=0`, which is below muffin's hardcoded
      `xfixes_major < 5` bail. XFIXES `MAJOR_VERSION` was bumped from
      2 to 5; QueryVersion already returns `min(client, server)` so
      older clients keep negotiating down. The XFIXES 5.0 opcodes
      muffin will subsequently issue (`CreatePointerBarrier` minor 31,
      `DestroyPointerBarrier` minor 32, `SetClientDisconnectMode`
      minor 33) are reply-less and fall through
      `handle_xfixes_request`'s `other` arm without blocking the
      client. A `major_version_meets_mutter_floor` const-assertion
      test was added so the floor cannot silently regress. Follow-up
      smoke advanced further but the screen turned fully white.
      Subsequent Cinnamon/Muffin fixes in progress: PRESENT
      `NotifyMSC` now emits an immediate `CompleteNotify` when the
      current MSC already satisfies the request (fixing the white
      screen), root/input hit-testing now reaches the compositor's
      overlay children (menus and desktop icons become clickable),
      and XI2 `QueryDevice` now advertises paired master/slave
      pointer+keyboard devices. Remaining Cinnamon rendering issue:
      Muffin's full-screen compositor stage surfaces are visible, but
      managed client frames were also being emitted directly through
      their Manual-redirected backings, which double-presented clients
      outside Muffin's own compositor output. 2026-05-19 correction:
      Manual redirect again removes the window subtree from normal
      scene traversal for descendants, while the redirected parent
      still emits its own backing directly into the scene. Automatic
      redirect samples the backing through W's scene entry.
      Resize-time backing rotation now
      reapplies the effective redirect mode so Manual frames do not
      become scene-visible after ConfigureWindow, and resize-time
      storage reallocation preserves the existing scene bit instead
      of blindly re-deriving it from `geom.mapped`. XFCE's previous
      "emit Manual backing directly" workaround is considered wrong
      and needs a separate compositor-output fix.

      **Stability + perf** observed positive through 3f.10 +
      3f.15 (flip-pending gate + failed-submit recovery + stroke
      aggregation). Formal bee 30-min capture + fuji v1/v2 perf
      diff deferred to Stage 4 close — until COMPOSITE flows
      land, v2's compose-pass + scene-walk is doing more
      work-per-frame than v1's per-window-mirror walk, which
      would skew an apples-to-apples comparison. Capture is
      meaningful once v2 reaches its target shape (compositor
      WMs working, damage-clipping engaged).

      Stage 3 is closed: v2 is the substrate Stage 4 builds on.

  ### Stage 3f planning-gap retrospective

  Substages landed during 3f close that were NOT in the
  original Stage 3 plan:
  - **3f.6 subwindow scene composition** — spec
    §scene-layering item 2 ("top-level + descendants") was
    deferred to Stage 4 in our stage plan; the cursor-trail
    diagnosis proved the deferral was unsound. Codex picked
    this up in 3f.9 with root storage + descendant recurse.
  - **3f.7 input dispatch** — no substage owned it; the spec
    only listed input as a `PlatformBackend` primitive.
  - **3f.13 full gradient LUT** — Stage 3c.2 comment
    promised 3e, but 3e's plan didn't include it.
  - **MaskScratch + trap-shader fixes (2026-05-17)** — the
    Vk-spec attachment-swizzle violation + AA off-by-0.5
    were latent v1+v2 bugs in shared shader/Vk code that
    lavapipe accepted but Intel/RADV rejected. Stage 3a
    landed `MaskScratch` shape verbatim from v1; no
    cross-check against the Vk spec or against multi-driver
    hardware caught the swizzle issue. xeyes was the first
    real-app that exercised the trap path enough to surface
    it.
  - **DrawableStore xid-detach semantics on PendingFence /
    refcount > 1 (2026-05-17)** — Stage 2b's `decref` was
    designed for the simple "refcount → 0 → destroy" case.
    Resize-with-Picture-refcount and resize-with-in-flight-
    fence weren't planned; both surfaced via xeyes on mate
    + marco. The fix split (5027cc2 + 4115fc8 + 3afa5bd) is
    iterative because each layer of the bug only became
    visible after the prior was addressed.

  Common pattern: spec-correct invariant got deferred / lost
  in stage planning; or Vk-spec-correct code wasn't verified
  against strict drivers because lavapipe was lenient. Future
  stages: an explicit "spec invariant coverage" checklist
  per stage (X11 + Vk both) would catch these. Lavapipe
  smoke is *necessary but not sufficient*; real-GPU smoke
  (Intel KBL or fuji minimum, RDNA2 / bee for the strictest
  driver coverage) needs to gate stage close, not be
  reserved for the final acceptance pass.
- [x] **Stage 4 — re-enable COMPOSITE + COW. Closed 2026-05-21
  on `cow-authoritative-mode`, now in `master`.** Manual-redirect
  backing routing, NameWindowPixmap, scene treats COW as
  always-on-top entry. xfce drop-shadow renders correctly. picom
  composites and updates per Damage event. The actual closure
  came from a different shape than the original plan envisioned
  (cow-authoritative scene gating + reparent reconciliation +
  long correctness fix chain — see the "Where we are" preamble
  and
  [`status-archive-2026-05-21.md`](status-archive-2026-05-21.md)).

  Plan: `docs/superpowers/plans/2026-05-17-stage-4.md` (8 codex
  review rounds; ready for impl 2026-05-17). Four substages:
  4a (`set_redirected_target` + paint resolver) → 4b
  (real `allocate_redirected_backing` / `name_window_pixmap` +
  protocol activation) → 4c (`SceneCompositor` mode-aware
  participation + Automatic-mode storage routing) → 4d (COW as
  first-class scene entry).

  - [x] **Stage 4a — `set_redirected_target` + paint resolver
    landed 2026-05-17 (`e2df0dd`).** Storage-side
    `DrawableStore::set_redirected_target(window_id,
    Some(backing_id))` plus `KmsBackendV2::resolve_paint_target(
    host_xid) -> Option<PaintTarget { id, offset }>` walking
    `windows_v2.parent` upward, accumulating descendant (x, y)
    offsets, stopping at the nearest redirected ancestor.
    Pre-loop short-circuit for non-`windows_v2` xids (pixmaps +
    root) checks the leaf's own `redirected_target` so
    `RedirectWindow(root, …)` routes correctly. `parent == None`
    arm (the production representation `create_subwindow`
    produces when host_parent is root_xid) steps up to root
    explicitly before the fall-through — codex round-7 finding
    caught pre-commit via TDD: the original test used the non-
    production `parent = Some(root_xid)` shape and missed the
    bug.

    ~22 paint sites swapped to `resolve_paint_target`:
    `fill_solid_rects`, `fill_rects_honoring_fill_state`,
    `try_tiled_fill` (all refactored to take `PaintTarget`);
    `fill_rectangle`, `poly_fill_*`, `poly_line/segment/
    rectangle/arc/point`; `copy_area` (dst only — src stays
    unresolved per spec); `put_image`, `get_image` (per plan
    Risk 1 — GetImage(W) under redirect reads B);
    `image_text*`, `poly_text*` (via `render_text_chars_v2` +
    `fill_text_background`); `render_composite`,
    `render_fill_rectangles`, `render_composite_glyphs`,
    `render_trapezoids`, `render_triangles_op` (dst-via-picture
    through refactored `resolve_dst_picture_for_render` —
    signature now `(&core, host_pic) -> Option<(u32, clip)>`,
    each caller resolves through `resolve_paint_target`
    afterwards); `set_container_background_pixel` / `_pixmap`.

    Trap / triangle redirect offset folded into the 16.16
    fixed-point `x_off` / `y_off` shift BEFORE bbox computation
    so the bbox is in backing coords. Per-rect picture clips
    shifted via `shift_dst_picture_clip`. `try_tiled_fill`
    shifts dst_x/dst_y but NOT src_x/src_y (which is
    `r.x - tile_ox`, a difference invariant under translation).

    Deliberately NOT swapped:
    `change_subwindow_attributes` (just stores into
    `windows_v2`); `allocate_window_storage` initial fill
    (happens before any redirect could be settable);
    `configure_subwindow` resize bg fill (resize allocates fresh
    storage; redirect can't be on a fresh drawable per the 4b
    lifecycle).

    Tests: 4 store unit tests + 9 resolver unit tests + 2
    Vk-backed acceptance tests (`v2_set_redirected_target_
    routes_fill_to_backing`, `v2_set_redirected_target_
    descendant_fill_lands_at_offset`) driving through the
    Backend trait via a new doc-hidden `test_set_redirected_
    target` helper. 262 lib + 22 ignored v2 Vk + 18 v2_
    acceptance tests green under lavapipe; clippy clean.

    No protocol-visible change yet — 4b lights up the protocol
    surface.
  - [x] **Stage 4b — real `allocate_redirected_backing` /
    `release_redirected_backing` / `name_window_pixmap` +
    protocol activation, landed 2026-05-17.**
    - **4b.1** (`3873a8f`): `Backend::supports_redirect_activation`
      capability gate (default `false`; v2 overrides to `true`);
      `set_window_scene_participation` + `set_backing_scene_
      participation` trait methods with default-`Ok(())`
      no-ops (v2 impls are 4c scope — the trait surface is here
      so 4b.6 can call them).
    - **4b.2-4b.4** (same commit): v2 real bodies for
      `name_window_pixmap` (`io::ErrorKind::NotFound` on the
      unredirected path per v1), `release_redirected_backing`
      (clears `host_window_to_backing` + `store.set_redirected_
      target(None)` for every routed window + `alias_registry.
      decref` → `free_pixmap` on refcount=0), real
      `allocate_redirected_backing` (idempotent on same-W
      re-call; seeds B from W via `engine.copy_area` BEFORE
      `set_redirected_target` flips routing per the plan's
      Cross-cutting §"Initial backing content"), alias-
      registry-aware `free_pixmap` (decref-then-decref-store
      gate so `FreePixmap(alias_xid)` doesn't drop the storage
      while a Reason-1 redirect hold remains).
    - **4b.5** (`ce64766`): descendant seed-copy. `seed_backing_
      from_window` DFS-walks `windows_v2` for descendants of W
      and `engine.copy_area`s each at its absolute position
      relative to W. Per Xorg `composite/compalloc.c:556` and
      the plan's Cross-cutting §"Per-hierarchy redirect".
    - **4b.6** (`8abb03c`): wire `activate_redirect_backing_for`
      into the COMPOSITE handlers in `process_request.rs`
      (gated on `supports_redirect_activation()`).
      `RedirectSubwindows(W, mode)` snapshots
      `state.resources.children(W)` and activates each child;
      `UnredirectSubwindows(W)` symmetric. Window-side
      `set_window_scene_participation` flip drives Manual=false
      / Automatic=true per the plan's `activate_one` helper;
      backing-side flip fires only on Automatic. Disconnect
      path mirrors the subtree-vs-single-window walk.
    - **4b.7** (`79f44ba`): MapWindow / MapSubwindows post-hook
      `maybe_activate_child_under_redirected_parent`. A child
      mapped under a `RedirectSubwindows`-redirected parent
      inherits the parent's mode; activation fires AFTER
      `backend.map_subwindow` per the plan's codex-round-6
      ordering fix so Manual `set_window_scene_participation
      (W, false)` lands last over `map_subwindow`'s blind
      `true` flip.
    - **4b.8** (same commit): same-owner mode-flip handler
      `flip_redirect_target_mode`. Diffs `prev.mode != new_mode`
      in the dispatcher; routes mode-flip to a path that
      preserves backing + aliases per Xorg's
      `compCheckRedirect` (`compwindow.c:172`) and Composite
      spec line 80, only firing the new mode's participation
      pair. No re-seed per codex-round-6 explicit decision.

    Tests landed: v2_allocate_redirected_backing_seeds_refcount_
    and_map, _is_idempotent, _survives_named_alias,
    v2_name_window_pixmap_returns_existing_backing,
    _without_redirect_errors_not_found,
    v2_release_redirected_backing_drops_storage_when_no_aliases,
    _survives_named_alias, v2_redirect_seed_copies_window_
    content, _copies_descendants. 262 yserver lib + 22 ignored
    v2 Vk + 26 v2_acceptance + 297 yserver-core lib all green;
    clippy clean.

    **Tests deferred to 4b.9 / 4c batch**: yserver-core has no
    Composite-handler test scaffolding today; the plan's
    remaining Vk-backed tests (`v2_mode_flip_preserves_backing_
    and_aliases`, `_map_window_after_redirect_subwindows_keeps_
    manual_participation`, `_map_subwindows_redirects_each_
    child`, `_name_window_pixmap_on_unviewable_returns_bad_match`,
    `_existing_alias_survives_window_unmap`,
    `_automatic_redirect_backing_is_scene_participating`)
    need either (a) a `RecordingBackend`-driven
    `handle_composite_request` test harness, or (b) v2's
    real impls of `set_window_scene_participation` +
    `set_backing_scene_participation` (4c scope). Both share
    the same scaffolding, so the batch lands at 4c open.

    No hardware smoke yet for Stage 4b in isolation — the
    real-app gate (mate + marco-with-compositing rendering
    correctly on bee + fuji) lands at 4c close, once
    SceneCompositor mode-aware participation actually drives
    the scene walk through the backing.
  - [x] **Stage 4c — SceneCompositor I4 + Automatic-mode
    storage routing, code landed 2026-05-17.** Hardware-smoke
    gate (4c.6) was run 2026-05-17 — see the 4c.6 sub-bullet
    below; the 4c implementation is correct but the gate moved
    to 4d because real compositors paint through COW, not
    classic RedirectSubwindows. Closed as part of Stage 4
    close (2026-05-21).
    - **4c.1** (`b7bfc02` + `70f2bf1`): `SceneCompositor::
      mark_scene_structure_damage_rects(&[vk::Rect2D])` —
      per-output dispatch via factored `dispatch_clip_rects_to_
      outputs` helper with per-output extent clipping. Real-
      inner test covers the dispatch path; 4c.1 follow-up added
      after code-quality review flagged stub-only coverage.
    - **4c.2** (`ea5cd8a`): `KmsBackendV2::window_absolute_rect(
      W_id) -> Option<vk::Rect2D>` — walks `windows_v2.parent`
      chain accumulating (x, y), terminates at root or returns
      None on dangling parent. Five unit tests.
    - **4c.3** (`b9a9f3a`): `build_scene` source_id indirection.
      Both root + window emission paths insert `source_id =
      store.redirected_target(id).unwrap_or(id)`; `image_view`
      sampled from `source_id`'s storage, `sampled_ids` push
      `source_id`. Manual-W filtered upstream by
      `scene_participating`; Automatic-W keeps in scene but
      blits from B. W's geometry (`dst_*`) untouched.
    - **4c.4** (`e7168bf`): v2 impls of
      `set_window_scene_participation` (captures pre-flip rect
      via 4c.2 helper, calls `store.set_scene_participating`,
      fires `scene.mark_scene_structure_damage_rects(&[rect])`
      always per the Cross-cutting transition table; coarse
      `mark_scene_structure_dirty` fallback on `None` rect) +
      `set_backing_scene_participation` (flag flip only, no
      damage — W-side carries the geometric damage). Also
      added `store.set_scene_participating(b_id, false)` to
      `release_redirected_backing` per round-3 finding.
    - **4c.5** (`e3326f4`): 4 no-Vk + 2 Vk-backed acceptance
      tests for redirect routing + participation. Test rename:
      `build_scene_uses_redirected_target_storage_when_set` →
      `_automatic_redirect_keeps_window_via_backing_storage`
      to align with Automatic-mode invariant framing. Four
      protocol-level tests TODO'd at v2_acceptance.rs:2154-2178
      (`v2_map_window_after_redirect_subwindows_keeps_manual_
      participation`, `_map_subwindows_redirects_each_child`,
      `_name_window_pixmap_on_unviewable_returns_bad_match`,
      `_existing_alias_survives_window_unmap`) — need
      `handle_composite_request` test scaffolding which doesn't
      exist in yserver-core today.
    - **4c.6 (HARDWARE SMOKE)** — ran 2026-05-17 on bee
      (RDNA2/RADV). Result: **black screen + partial mate-panel
      gray bar at top + repeating `render_composite gap:
      host_src 0x40005f not resolvable`**. Diagnosis from
      `yserver-hw.log` (12K lines) + `mate.log` cross-reference:

      1. Marco issued **zero** `RedirectWindow` /
         `RedirectSubwindows` calls during the entire ~20s
         session. Only `COMPOSITE::QueryVersion` and
         `COMPOSITE::GetOverlayWindow -> 0x103`. The
         classic Redirect-based path that Stages 4a/4b/4c
         built is **never exercised** by modern marco.
      2. Marco's compositor uses pure COW (XComposite
         Overlay Window). Marco called `GetOverlayWindow`,
         got the sentinel xid `0x103`, then built a
         Picture wrapping COW (xid `0x40005f` in the log).
         The Picture record was correctly stored in
         `core.pictures`, but the underlying drawable
         `host_xid = 0x103` has no entry in the v2 store —
         `get_overlay_window` returns the sentinel without
         allocating storage. `resolve_picture_for_render`
         → `store.lookup(0x103)` → `None` → render_composite
         drops every paint marco issues. Repeats every tick
         while marco's compositor is alive.
      3. The plan's §4c gate text *"mate + marco-with-
         compositing"* was a planning miss. The plan's own
         §4c hedge for xfwm4 (*"renders if it doesn't use
         COW (it likely does, so see 4d)"*) applies
         identically to marco: marco-with-compositing
         requires COW. The 4c implementation IS correct —
         it's just unused by modern compositors. The
         meaningful smoke that exercises 4c without 4d
         would need a non-COW compositor (e.g. `xcompmgr`
         classic, or marco-compositing-disabled with
         non-COW XRender clients).

      **4c implementation status: correct, unblocked by
      this smoke result.** The black-screen failure mode
      is the 4d-COW gap, not a 4c bug. mate-panel's gray
      bar rendered correctly through the non-compositor
      v2 paint pipeline (top-level windows compose via
      `build_scene` per Stage 3f.6); caja-desktop hadn't
      finished its wallpaper render by the time the session
      was killed at ~20s.

      Plan revision: §4c "Hardware smoke gate" amended to
      reflect that marco-with-compositing is a 4d gate, not
      a 4c gate. The 4c-only gate (if needed) is a
      non-COW-using compositor or marco-without-compositing
      exercising classic RedirectSubwindows from a synthetic
      test. Deferred until 4d lands.

    Final reviewer assessment: READY_FOR_HARDWARE_SMOKE. 277
    yserver lib + 22 ignored v2 Vk + 28 v2_acceptance tests
    green under lavapipe. Two known follow-ups tracked in
    `known-issues.md`:
    - `src_size = [1, 1]` after redirected W resizes past B's
      extent — could stretch B onto larger W rect (4c.3 limitation).
    - Multi-output coord-space for scene-structure damage rects —
      `mark_scene_structure_damage_rects` clips by extent
      assuming origin (0, 0); under multi-output layouts the
      clip would be in the wrong frame (not a regression vs the
      4c.1 stub but now flagged).
  - [x] **Stage 4d — Composite Overlay Window as first-class
    scene entry, code landed 2026-05-17 (`8a0456f` + `dac676d`
    polish), closed 2026-05-21 by the cow-authoritative-mode
    branch.** Hardware-smoke gate met by the mate + marco-with-
    compositing run on `cow-authoritative-mode` after the
    correctness fix chain. See the "Where we are" preamble for
    the close summary;
    [`status-archive-2026-05-21.md`](status-archive-2026-05-21.md)
    has the full per-iteration narrative.

    `cow_refcount: u32` added to `KmsCore` (protocol
    bookkeeping only). `cow_id: Option<DrawableId>` on
    `KmsBackendV2`. Two new `Backend` trait methods —
    `get_overlay_window` + `release_overlay_window` (default
    no-op for v1 / ynest / RecordingBackend; v2 overrides).
    Backend impl: first `get_overlay_window` allocates a
    screen-extent depth-24 BGRA8 drawable as
    `DrawableKind::Window`, zero-fills it via
    `engine.fill_rect`, sets `cow_id` + refcount=1, and keeps
    it off the normal scene path. xfwm4 presents into its own
    child compositor window, so a topmost scene-layer COW can
    cover the real output with a stale black surface.
    Subsequent calls bump refcount only.
    `release_overlay_window` decrements; on zero it clears
    `cow_id` and decrefs storage. Defensive guard against
    unmatched release (no underflow). `process_request.rs` GET
    / RELEASE arms now call the backend hooks; log + continue
    on Err so the protocol reply still goes out.

    Early smoke showed xfwm4's compositor paints into a child
    window under the overlay, so the first pass of the COW was
    not kept as a visible scene entry. The overlay drawable now
    stays backend-owned storage only; there is no top-level COW
    draw in `build_scene`, which avoids covering xfwm4's real
    output with the zero-filled overlay surface.

    2026-05-19 correction after Cinnamon/Muffin smoke: Manual-
    redirected parents still emit their own redirected backing
    directly, but their descendants are pruned so the subtree is
    owned by the compositor. Automatic redirects continue to sample
    through W's `redirected_target`. The earlier "emit all manual
    backings" path was too broad; the real rule is parent backing
    yes, child leakage no.

    7 tests: `cow_get_overlay_first_call_allocates_storage`,
    `_second_call_refcounts`, `cow_release_decrements_refcount`,
    `_zero_drops_storage`, `cow_release_without_prior_get_is_noop`
    (defensive-branch coverage), `build_scene_appends_cow_above_top_levels`,
    `build_scene_cow_is_below_cursor` (spec layering item 4),
    plus 1 Vk-backed acceptance `v2_cow_paint_appears_on_scanout`
    (paint via `put_image` on the COW xid, assert
    presentation-damage non-empty + `get_image` roundtrip
    confirms paint landed on COW storage). 285 yserver lib +
    23 ignored v2 Vk + 29 v2_acceptance tests green under
    lavapipe; clippy pedantic clean for touched lines.

    **Promoted from "secondary gate" to "load-bearing":** per
    the 2026-05-17 4c.6 smoke result, both
    marco-with-compositing AND xfwm4-with-compositing use COW.
    4d is the **actual** gate for "compositing WMs render
    correctly". mate + marco-compositing failed at 4c without
    4d (`render_composite gap: host_src 0x40005f not
    resolvable` log shape with `0x40005f` being a Picture
    wrapping COW 0x103).

    Hardware-smoke gate: **(a)** mate-session +
    marco-with-compositing on bee + fuji; and **(b)** full
    xfce4 session with xfwm4-with-compositing on bee + fuji.

    ### Stage 4d post-smoke iteration (2026-05-17)

    Eight rounds of hardware smoke on bee uncovered a cascade
    of issues. Fixes that landed:

    - [x] **Stage 4d.1 — COW host_xid wiring on resource
      record (`3ed630c`).** First smoke run after the 4d
      commits (`8a0456f` + `dac676d`) showed marco's
      PresentPixmap onto COW silently dropping at
      `process_request.rs:4124` because
      `state.resources.host_drawable_target(0x103)` returned
      None. COW window was registered in resources.rs:230 with
      `host_xid: None` and 4d never wired it. Fix: handler-side
      mutation on first `GetOverlayWindow` to set host_xid +
      root extent; symmetric clear when refcount hits 0 (new
      `release_overlay_window -> io::Result<bool>` trait
      return). Marco's PresentPixmap into COW now lands.

    - [x] **Stage 4d.2 — COMPOSITE handler logging (`589aa87`).**
      Multiple diagnosis rounds wasted because
      `REDIRECT_WINDOW` / `REDIRECT_SUBWINDOWS` /
      `UNREDIRECT_*` / `NAME_WINDOW_PIXMAP` arms had no
      `debug!` lines. Silenced the misleading
      `update_host_event_mask not yet implemented` warn at the
      same time (v1's body is a pure no-op on KMS).

    - [x] **Stage 4d.3 — DRI3 backfill (`9414096`).** v2's
      `dri3_capabilities()` hard-returned `unsupported()` per
      a "Stage 1b — no Vulkan" comment that never got
      backfilled when 2a landed Vk. Ported v1's ~260 LoC DRI3
      impl (`dri3_capabilities`, `dri3_open`,
      `dri3_import_pixmap`, `dri3_fence_from_fd`,
      `dri3_trigger_fence`, `dri3_fd_from_fence`,
      `dri3_import_syncobj`, `dri3_free_syncobj`,
      `dri3_signal_syncobj`, `dri3_export_pixmap`,
      `dri3_supported_modifiers`). v2's `Storage` gained a
      `from_imported_drawable_image` constructor (DrawableImage
      stays owner of Vk handles + dma-buf fd; v2 Storage
      aliases image/view/memory; Storage::destroy drops the
      inner DrawableImage). Added `dri3_xshmfences` +
      `dri3_sync_resources` state fields on `KmsBackendV2`.
      Without this, mate-session-check-accelerated fails its
      GLES probe (`libEGL warning: DRI3 error: Could not get
      DRI3 device`) and downstream compositors can't import
      backings as GPU textures.

    - [x] **Stage 4d.4 — disconnect-recovery participation
      restore (`6b63173`).** `teardown_redirect_for_window`
      in `process_disconnect.rs` called
      `release_redirected_backing` but didn't restore
      `set_window_scene_participation(W, true)`. Manual-mode
      compositor crash would leave every redirected window
      invisible until session end. `RecordingBackend`
      extended with `ReleaseRedirectedBacking` +
      `SetWindowSceneParticipation` recorded calls.

    - [x] **Stage 4d.5 — rotate_redirected_backing_on_resize
      release-then-allocate order (`cd22f47`).** First
      with-comp smoke showed the top panel invisible —
      marco's `NameWindowPixmap(top_panel)` returned
      `not redirected` even though `RedirectSubwindows(root,
      Manual)` was active and the panel was a top-level. Root
      cause: panel got resized 25→28 after map (marco's
      animation). `rotate_redirected_backing_on_resize` did
      `allocate(new_w, new_h) → release(old)` but v2's
      `allocate_redirected_backing` is idempotent on
      `host_window_to_backing[W]` (matches v1) and returns the
      existing backing **ignoring the new dimensions**. The
      release then destroyed that same backing → `W` lost its
      route. Fix: swap order to release-then-allocate.
      Backing survives if `NameWindowPixmap` aliases hold
      (per Composite spec); allocate creates a fresh one at
      new size since host_window_to_backing is now empty.

    - [x] **Stage 4d.6 — depth-24 source pictures force
      alpha=1.0 (`d20f279`).** Per X11 Render PictFormat:
      Pictures wrapping depth-24 drawables have alpha_mask=0
      and samples must return alpha=1.0. v2's render_composite
      was sampling raw alpha bits from depth-24 storage
      (where the "X" padding byte is undefined / often 0).
      Plumbed a `force_opaque_src/mask` bit into push-constants
      `repeat_modes[]` upper bits (bit 8); shader applies
      `src.a = 1.0` for that case. Implementer noted the
      existing `BgraNoAlpha` swizzle on depth-24 image views
      already pinned `a=ONE` so the shader-side force is
      belt-and-suspenders; left in place for paths that bypass
      the swizzle (self-alias scratch). Scoped to `depth == 24`
      (not `depth < 32`) so depth-8 (A8) and depth-1 (bitmap)
      mask coverages aren't broken.

    - [x] **Stage 4d.7 — emit_window_subtree alpha_passthrough
      = true (`f3e9276`).** Non-compositing v2 smoke showed
      mate panel-right area (clock applet / system tray) as
      opaque black instead of v1's visible widgets. Root
      cause: v2's scene draws windows with
      `alpha_passthrough=false` which force-opaques alpha=1.0
      in the shader. depth-32 panel storage initialized to
      `(0,0,0,0)` per `default_window_init_color(32)` →
      panel-right unpainted areas had alpha=0 → force-opaque
      → opaque black covering wallpaper + masking applet
      windows that should sit there. v1's per-window-mirror
      scanout walk alpha-blends → matches X11 Composite
      semantics. Flipped to `alpha_passthrough=true`. Root
      storage draw at scene.rs:986 left `false` (it IS the
      opaque bottom layer). Brought mate-no-comp visual to
      ~95% v1 parity.

    ### Stage 4 close (2026-05-21)

    Stage 4 closed on `cow-authoritative-mode`, since merged into
    `master`. The pragmatic-floor and PictFormat options framed
    in the pre-close retrospective (now archived to
    [`status-archive-2026-05-21.md`](status-archive-2026-05-21.md))
    were not what landed — instead, a two-phase plan
    (`docs/superpowers/plans/2026-05-20-cow-authoritative-mode.md`)
    plus a ~35-commit correctness fix chain. Phase 1
    (`19ed354`) gates `build_scene` on COW registration; Phase 2
    (`1065c50` + supporting commits) reconciles redirect status
    on `ReparentWindow` per Xorg
    `compRedirectOneSubwindow` / `compUnredirectOneSubwindow`.
    The fix chain closed the compositor-update bug class
    (DAMAGE Subtract canonicalization + remaining-region
    re-report, Present Copy `x_off/y_off` direction matching
    Xorg `present_copy_region()`, `ConfigureNotify.above_sibling`
    direction, viewable `XDamageCreate` immediate seed,
    `MapWindow` idempotency, etc.) plus an input-stack chain
    (XI2 grab opcodes 51-55 wired into core grab state,
    synthesized NotifyGrab crossings, `owner_events=true`,
    XIChangeCursor → backend `define_cursor`, CWA cursor=None
    clear propagate, sub-window descent in v2
    `window_under_cursor`, XFixes SetCursorName/GetCursorName).

    See the "Where we are" preamble at the top of this doc and
    [`status-archive-2026-05-21.md`](status-archive-2026-05-21.md)
    for the full diagnosis narrative.

    Residual items moved to `known-issues.md` (PictFormat / ARGB
    intent tracking, KmsCore.pictures disconnect cleanup, MATE
    Control Center yellow group-header glyphs).

- [ ] **Stage 5 — make v2 fast.**
  Active plan:
  `docs/superpowers/plans/2026-05-20-stage-5-make-v2-fast.md`.
  HW cursor is now treated as implemented prerequisite work, not the
  Stage 5 scope. Stage 5 is the measured perf-closure pass: telemetry
  first, bounded frame production, COW-authoritative compositor mode,
  paint-submit aggregation, cheaper compose, allocation-churn removal,
  then syncobj/direct-scanout/plane strategies only if profiling still
  justifies them.

  - [x] **Task 4 layer 1 — DescriptorPoolRing.** Landed
    2026-05-22 (commits `fb058a6` through `e12a559`, 14 commits on
    `perf` branch). Spec:
    `docs/superpowers/specs/2026-05-21-descriptor-pool-ring-design.md`;
    plan: `docs/superpowers/plans/2026-05-21-descriptor-pool-ring.md`.
    Per-call `BatchDescriptorArena` instantiation in
    `try_vk_render_composite` + `try_vk_render_traps_or_tris`
    replaced with a long-lived `DescriptorPoolRing` on `EngineInner`
    cycling Free/Active/InFlight pools by `acquire_generation`
    watermark. `vkResetDescriptorPool` failure poisons the ring
    (hard-error propagation: subsequent `acquire_set` returns
    `ERROR_UNKNOWN`). v1's `BatchDescriptorArena` stays in tree —
    `paint_batch.descriptor_arena_mut()` still drives it.

    **In-tree gates** (lavapipe ICD, 2026-05-22):
    11 ring unit tests pass
    (`crates/yserver/src/kms/v2/descriptor_pool_ring.rs`);
    3 acceptance gates pass
    (`v2_render_composite_bumps_pool_create_telemetry`,
    `v2_render_composite_pool_creates_bounded_after_warmup`,
    `v2_render_traps_pool_creates_bounded_after_warmup`).
    Pool ring stats from the 2000-op acceptance runs: `creates=2`,
    `resets=7`, `residency=2` on both Composite and Trapezoid paths
    — pools recycle, residency stays bounded.

    **Bee hardware capture 2026-05-22** (Ryzen 9 6900HX / RDNA2 /
    RADV, MATE drag workload; perf data `yserver-mate.perf.data`,
    telemetry `yserver-hw-mate.log`, full analysis in the Stage 5
    plan §"Bee 2026-05-22 perf-branch findings"):
      - Ring fix delivered as designed: `descriptor_pool_creates/s = 0`
        through the drag; `descriptor_pool_resets/s = 5-6` (recycle
        path runs).
      - The yoga/Turnip pathology (per-call `vkCreateDescriptorPool`
        → `msm_ioctl_vm_bind` shmem pin) was never material on bee,
        so the bee drag-lag user-perceived state did NOT improve.
      - Bee's drag-lag hot path is `ioctl → libvulkan_radeon →
        amdgpu` at `queue_submit2/s = 2119` (~35 submits/frame, one
        kernel round-trip every ~470 µs). Next perf-branch layer:
        Stage 5 Task 3 (paint-submit aggregation), diagnostic-first
        per the timeline-semaphore lesson in
        `feedback_perf_branch_2026_05_10`.

    **Yoga hardware capture 2026-05-22** (Snapdragon X1 / Adreno X1
    / Turnip, same MATE drag workload; full analysis in the Stage 5
    plan §"Yoga 2026-05-22 perf-branch findings"). This is the
    capture the design was authored against — the 2026-05-21
    baseline showed `vkCreateDescriptorPool → msm_ioctl_vm_bind`
    shmem-pin path at ~36% of yserver's own CPU.
      - `descriptor_pool_creates/s = 0` in 50 of 52 one-second
        buckets; **2 total creates** across the entire 52-second
        drag (was implicit ~4700/s on the pre-ring baseline — four
        orders of magnitude reduction).
      - `descriptor_pool_resets/s = 0–26` (avg ~6–10 during drag);
        recycle path runs as designed.
      - `descriptor_allocations/s = 180–183` (unchanged from
        baseline — same allocations, recycled pools).
      - Peak `paint_submits/s = 8117` (drag avg 3807; baseline was
        700–4700, so we're at parity or higher).
      - yserver total CPU **0.32%** of system (`perf report` with
        system-wide capture); `libvulkan_freedreno.so` inside
        yserver another 0.44%. No Rust symbol above 0.05%.
      - The `create_descriptor_pool → msm_ioctl_vm_bind` path that
        hit ~1.63% of total system CPU on the baseline is no longer
        measurable at the 0.05% threshold.
      - User subjective: **no CPU spikes during drag** — matches
        the data.

    **Silence hardware capture 2026-05-22** (i9 13900k / rx580
    Polaris/GCN4 / RADV / dual 2560x1440, same MATE drag workload;
    full analysis in the Stage 5 plan §"Silence 2026-05-22
    perf-branch findings"). The perf recipe defaults to
    `RUST_LOG=warn` which suppresses `INFO`-level v2_telemetry, so
    telemetry and perf cover two consecutive drag runs.
      - System fully responsive; drag silky-smooth; CPU spikes
        peak ~30% user-subjective, confirmed by perf at ~1.1% of
        total system CPU = ~35% of one logical core averaged
        across the 13900k's 32 logical cores.
      - **Paint volume 3-9× bee** because silence's CPU isn't the
        limiter: `paint_submits/s` avg 6852 peak 18910 (bee peak
        2048); `queue_submit2/s` avg 7069 peak 19379 (bee peak
        2119). Same X11 client traffic; bee was rate-limited by
        single-thread cost so it never measured what MATE actually
        wanted to push.
      - **`composite_submits/s` avg 98 peak 121** matches the
        dual-output prediction (2 × 60 Hz; bee single-output was
        59). `frame_present_count/s` tracks 1:1 — KMS keeps up on
        both outputs.
      - **`storage_allocations/s` avg 1605 peak 6073 — 13× bee.**
        Dual output compounds with bigger surfaces; every
        full-output redirected backing misses the `PixmapPool`
        (≤128px bucket cap). Task 5 territory; pool needs a
        separate bucket regime for compositor-sized surfaces.
      - **DescriptorPoolRing working as designed:**
        `descriptor_pool_creates/s` ≈ 0 (2 across the whole run,
        both in warmup); `descriptor_pool_resets/s` avg 24 peak
        65; `descriptor_allocations/s` avg 255 peak 304 (close to
        yoga's 180). Ring scales with submit rate without
        exploding creates.
      - **`cpu_fence_wait_ns/s` avg 76 ms peak 206 ms** (bee 12
        ms). 7-20% of one core in fence waits — Task 6 territory
        but doesn't bind on silence either.
      - **Perf hot-path shape identical to bee, diluted.**
        `libvulkan_radeon.so` 0.25% / 0.08%, `libc` `__ioctl`
        0.42% / 0.06%; no Rust symbol above the 0.05% threshold.
        Same `main → run_core → … → __ioctl → libvulkan_radeon`
        chain. Confirms the bottleneck is universal across
        AMD/RADV; it only *binds* where single-core budget runs
        out.
      - **`VK_EXT_image_drm_format_modifier` missing on rx580
        under RADV** — `Vulkan-fed scanout will not work` warning
        logged at startup, yet the desktop displays correctly,
        so a fallback scanout path is in use. Worth a follow-up
        to confirm which path and whether it accounts for any of
        the silence-specific allocation behaviour. Tracked as a
        Stage 5 Task 5 sub-question, not a perf gate.

    **Smearing artifact diagnosis (silence-specific surfacing;
    underlying bug is general).** Drag showed occasional
    smearing / damage trails on silence that bee/yoga didn't
    expose. Telemetry pins it: `damage_fraction` hits 1.00 in
    peak buckets while `full_redraw_fallback/s` stays ~0 across
    the run. `pick_repaint_region` keeps choosing `Clipped` with
    `loadOp=LOAD` even when ~100% of the output is damaged;
    Clipped at 99% damage paints 99% of pixels and leaves the
    residual ~1% as prior buffer-age content — that residual is
    the smearing. Bug is the strategy selection (no
    `damage_fraction > threshold → Full` arm); silence surfaces
    it because silence is the first hardware with enough headroom
    to push damage_fraction to saturation under MATE drag. Stage
    5 Task 4 already calls out "full redraw when clipping is more
    expensive than redraw" — this is its correctness corollary.

    The per-hardware-class bottleneck split is now empirically
    established on three axes:
      - **yoga (Snapdragon X1 / Turnip)** — `vkCreateDescriptorPool
        → msm_ioctl_vm_bind` pin path. Fixed by Task 4 layer 1.
      - **bee (Ryzen 9 6900HX / RADV)** — `vkQueueSubmit2` ioctl
        rate (~2k/s) bound by single-thread budget. Task 3.
      - **silence (i9 13900k / RADV)** — same ioctl-rate cost as
        bee but ~3-9× higher absolute volume, absorbed by
        single-core headroom. Perf does not bind; the higher
        damage saturation exposes the `pick_repaint_region`
        correctness gap (smearing).

    Task 4 layer 1 + Task 3 POC + render_composite generalization
    fast-forwarded to `master` together (HEAD `4ecb271`). Remaining
    Stage 5 work (Task 3 `render_fill` extension, Task 4 damage
    strategy, Task 5 follow-ups) continues directly on `master`.

  - [~] **Task 3 prep — submit-trace instrumentation +
    diagnostic capture.** Landed on `perf` 2026-05-22.
    `YSERVER_SUBMIT_TRACE=<path>` (off by default, zero hot-path
    cost) writes one TSV row per `vkQueueSubmit2` with kind +
    target + op + src/mask class + flags. Wired into all 21 v2
    submit sites + the per-output scene-compose loop. The two
    `-telemetry` Justfile recipes enable it automatically and
    write to `yserver-{mate,xfce}.submit.tsv`. New module
    `crates/yserver/src/kms/v2/submit_trace.rs` (630 LoC, 6
    unit tests including a kind-name prefix-collision property
    check).

    **Silence drag capture 2026-05-22** (45.5 s, 380,917
    submits, 8,376/s avg). Full analysis in the Stage 5 plan
    §"Silence 2026-05-22 submit-trace findings (Task 3 design
    data)". Headline:
      - **37.7 % of submits sit in trivially coalesce-able runs**
        (consecutive same-target same-kind same-(op, src_class,
        mask_class)). Aggregating each run to one CB collapses
        143,560 of 380,917 submits → ~5,200/s post-aggregation.
        On bee that lands well below the ~2,000/s steady-state
        where the lag bound.
      - Three kinds carry 96 % of the savings: `render_composite`
        (88k savings), `copy_area` (40k), `render_fill` (8k).
      - Biggest single hotspot: **marco compositor `copy_area`
        onto COW — 46,920 of 62,255 (75 %) target drawable
        id=35**. Runs of length 12-50. Single-target coalescing
        here alone captures ~40k of the 143k savings.
      - `render_composite` keys concentrate on 4 dominant
        tuples (`over | direct | no_mask` 35 %, `src | direct
        | no_mask` 25 %, `over | direct | direct` 18 %, `src |
        gradient_linear | no_mask` 6 %). The aggregation key
        `(target_id, kind, op, src_class, mask_class)` matches
        the runs naturally.
      - **Aggregation boundary is the main-loop tick** (end of
        `maybe_composite`, before `scene.tick`). Compose reads
        from every touched target, so flushing the pending-op
        queue immediately before compose runs is correctness-
        for-free; no cross-tick ordering work needed.

    Bee re-capture pending hardware access. Next concrete step
    is Task 3 brainstorm → spec → plan (same shape as
    DescriptorPoolRing / Task 4 layer 1), with the COW
    `copy_area` slice as the smallest valuable proof-of-concept.

  - [~] **Task 3 POC — COW `copy_area` coalescing landed
    2026-05-22** on `perf` at `0bec1b3`. Full design + numbers
    in the Stage 5 plan §"Task 3 POC 2026-05-22 — COW
    `copy_area` coalescing".

    `RenderEngine` grows `PendingCowBatch` (one CB + ticket
    across N appends) plus `cow_copy_area` /
    `flush_cow_batch` / `drain_cow_flush_records`. Auto-flush
    hooks at the top of every other engine entry point keep
    same-queue submission order correct. Backend routes
    `copy_area` to the batched path when
    `dst_target.id == self.cow_id`; per-call telemetry
    suppressed and re-emitted at flush time with
    `batch_size = coalesced_count`. New `cow_batches_flushed`
    + `cow_copies_coalesced` counters on `Telemetry`.

    **Silence verification — same 45 s MATE drag:**

    | metric                    | pre-POC | post-POC | Δ        |
    | ------------------------- | ------: | -------: | -------: |
    | `paint_submits/s` avg     |   6,852 |    5,653 | **−18 %** |
    | `paint_submits/s` peak    |  18,910 |   14,040 | **−26 %** |
    | `queue_submit2/s` avg     |   7,069 |    5,850 |  −17 %   |
    | `queue_submit2/s` peak    |  19,379 |   14,438 |  −25 %   |
    | `cpu_fence_wait_ns/s` avg |   76 ms |    45 ms | **−40 %** |
    | `composite_submits/s` avg |      98 |       98 | unchanged ✓ |

    Cow batch shape: 10,111 flushes; **avg batch size 5.41,
    peak 46**. Cow-path submit collapse: 46,920 → 10,111 =
    **78 % fewer cow submits**. Non-cow `copy_area` path
    untouched (`avg_batch_size = 1.00` as designed).

    **Bee projection:** pre-POC bee bound at ~2 k submits/s;
    pre-POC silence ran 8.4 k/s; post-POC silence 5.7 k/s →
    projected bee ~1.4 k/s, comfortably below the
    user-perceived lag floor. Bee re-capture pending hardware
    access.

    **Tests:** 3 Vk-backed lavapipe tests cover the marco
    pattern (4 distinct srcs → 1 cow dst), same-src repeat
    dedupe, and the per-method auto-flush hook. 368 lib + 40
    lavapipe + 35 acceptance tests pass; default clippy
    clean. (One pedantic over-100-lines warning on
    `cow_copy_area`; deferred.)

    **End-of-session damage artifacts** observed on the
    post-POC drag are the pre-existing silence
    `pick_repaint_region` damage-saturation bug
    (`damage_fraction → 1.0` while `full_redraw_fallback`
    stays ~0) reproducing unchanged — not POC-caused. Scanout
    dumps held off-tree for the Task 4 correctness fix later.

    **Remaining Task 3 work**: extend the aggregation pattern
    to `render_composite` (88 k savings) + `render_fill`
    (8 k savings). Same `begin → append → flush` shape but
    aggregation key needs `(target_id, op, src_class,
    mask_class, pipeline_id)` instead of just `target_id`,
    and per-call descriptor sets diverge per key.

  - [~] **Task 3 generalization — `render_composite` batching
    landed 2026-05-22** on `perf` at `68af625`. Full design in
    the Stage 5 plan §"Task 3 generalization 2026-05-22 —
    render_composite". Took two iterations:

    **Iteration 1 (over-strict key)** — failed by measurement.
    Predicate keyed on the full per-call signature including
    `src_id`, `mask_id`, src/mask `repeat`, src/mask
    `pict_format`. Silence verification: **1.005 calls/batch**
    (essentially no coalescing); `paint_submits/s` *regressed*
    from 5,653 to 6,158 (+9 %). Diagnosis: marco's compositor
    pump is "N different srcs → one stage texture" (same shape
    as cow `copy_area`); the over-strict predicate rejected
    every same-target run because `src_id` varied per call.
    The submit-trace's `src_class` column conflated distinct
    `src_id`s into "direct", which mis-led the original
    Iter-1 analysis — trace schema lesson for future Task 3
    work.

    **Iteration 2 (relaxed key)** — measured success. Predicate
    cut to four fields that drive pipeline + render-pass:
    `dst`, `op`, `dst_pict_format`, `mask_component_alpha`.
    Everything else (`src_id` / `mask_id`, src/mask `repeat`,
    src/mask `pict_format`, transforms, `clip_rects`) is
    re-encoded per append: each append allocates its own
    descriptor set, `cmd_bind_descriptor_sets` inside the
    open render pass, scissor + push consts per draw.
    `record_render_composite_open/draws/close` split so the
    pipeline binds once at open and the per-append descriptor
    binding happens inside `_draws`.

    **Silence verification — same 45.5 s MATE drag:**

    | metric                            | pre-POC | cow-only | render-relaxed |
    | --------------------------------- | ------: | -------: | -------------: |
    | `paint_submits/s` avg             |   6,852 |    5,653 |   **4,180**    |
    | `paint_submits/s` peak            |  18,910 |   14,040 |   14,814       |
    | `queue_submit2/s` avg             |   7,069 |    5,850 |   **4,377**    |
    | `composite_submits/s` avg         |      98 |       98 |       98 ✓     |
    | `render_batches_flushed/s` avg    |   n/a   |   n/a    |   1,294        |
    | `render_composites_coalesced/s` avg | n/a   |   n/a    |   2,018        |

    Cumulative reduction in `paint_submits/s` avg: **−39 %
    vs pre-POC, −26 % on top of cow alone.** Render batch
    shape: 122,103 flushes containing 174,953 underlying
    composites = **avg 1.43 calls/batch, peak 8**.
    `composite_submits/s` unchanged at 98 — scene compose
    path untouched as designed.

    **Tests:** four new Vk-backed lavapipe tests covering
    same-key coalesce (with **different srcs** to verify
    per-append descriptor rebinding), key mismatch flush,
    Solid src skipping the batched path, and the per-method
    auto-flush hook. 368 lib + 44 lavapipe tests pass;
    default clippy clean.

    **Out of scope for the POC:**
    - `render_fill` (Solid src, ~8 k of the 143 k total
      coalesce savings) — would need `record_solid_color_clear`
      lifted out of the render pass; deferred until bee
      re-capture justifies it.
    - Bee + yoga re-capture pending hardware access.

    **One transient "eog window stayed at origin" observed
    during silence verification.** Could not repro on
    master, on perf HEAD, or with the relaxed POC reverted.
    Filed as a non-repro flake. Scanouts saved off-tree.

    **Bee re-validation 2026-05-22 surfaced UAF + PRESENT
    deadlock + drag latency.** Plain `yserver-mate-hw` on
    bee (Rembrandt RDNA2 iGPU) wedged with `ERROR_DEVICE_LOST`
    flooding paint paths. RADV's `addr_binding_report` named
    it: a 256×256 d32 mate-panel icon pixmap was destroyed
    while a coalesced `render_composite` CB still held a
    descriptor for its VkImageView; gpu-allocator recycled
    the slab in 115 µs into a smaller image at the same VA;
    the next pageflip's CB sampled the recycled page; TCP
    permission fault. Bee-only because Rembrandt's GTT
    fast-recycle path + RDNA2's strict TCP boundary check
    expose the dangling-descriptor window that silence
    (rx580/GCN4), yoga (Adreno/turnip), and fuji (Intel/ANV)
    silently survived. **Fix landed:** engine eagerly stamps
    the batch ticket onto src/mask/dst at append time (closes
    the destroy_now path on `decref` while the batch CB is
    pending), and `wait_for_drawable_idle` now flushes
    pending batches before sampling `last_render_ticket`
    (the round-2 deadlock the eager touch alone introduced
    — `wait for drawable 0x103: TIMEOUT`, screen black, kernel
    alive via SAK + SysRq). Four regression tests gate it
    (three engine-level + one backend-level), all verified
    red→green. bee CC drag visibly lags post-fix — not a
    *measured* regression though (pre-fix bee crashed before
    steady state, silence pre-fix was qualitatively fast but
    silence is a much faster machine). yserver-side CPU
    shape is unchanged from the perf-branch baseline (4.47 %
    of 16 cores vs 4.26 %, flat user-space). The lag is the
    PRESENT path correctly serializing on the GPU finishing
    its frame's cow_batch/render_batch — pre-fix raced
    (`last_render_ticket == None` → immediate return → Mesa
    WSI client woke before COW actually had the content),
    independent of whether that race happened to feel fast
    on any given machine. Followup filed as Stage 5 Task 6.1
    (PRESENT IN_FENCE_FD — move the wait off the CPU into
    the KMS atomic-commit path via `ScanoutBo::export_
    semaphore`). Full chain in the Stage 5 plan §"Bee
    2026-05-22 render-batch UAF + PRESENT wait — fix landed".
    Diagnostic recipe `just yserver-mate-hw-vkdebug` added
    (with a warning that it's not survivable on Renoir —
    `RADV_DEBUG=syncshaders` stalls the display controller
    hard enough to need SysRq recovery).

    **Task 6.1 deferred PRESENT completion attempt — branch
    `feature/deferred-present-completion`, shelved 2026-05-23.**
    Spec
    `docs/superpowers/specs/2026-05-23-deferred-present-completion-design.md`;
    plan `docs/superpowers/plans/2026-05-23-deferred-present-completion.md`.
    Implemented the deferred-completion design end-to-end (18
    commits on the branch, lib tests 384 / 49 ignored green,
    clippy clean, vng smoke recipe added as
    `just yserver-defpresent-vng-smoke`). Goals: replace `8ca552a`
    (sync wait + bee fix) with an async mechanism that closes the
    bee UAF without the yoga deadlock and without per-PRESENT
    CPU fence waits.

    Two hardware findings:
    1. **Yoga / Turnip hard hang**, fixed by `4f566e6`. Initial
       implementation captured `engine.current_cow_batch_ticket()`
       at PRESENT::Pixmap time and called
       `vkGetFenceFdKHR(SYNC_FD)` on it. That fence is recorded
       but not submitted at PRESENT time (the cow_batch CB is
       still open; submission lands later at `flush_cow_batch`).
       Per `VUID-VkFenceGetFdInfoKHR-handleType-01457` ("the
       fence must have an associated fence signal operation that
       has been submitted for execution"), calling export on an
       unsubmitted fence is UB — Turnip on yoga hung the entire
       process; lavapipe likewise hung in a unit test attempt
       (Task 4 verbatim test was dropped).
       Fix: capture `dst.last_render_ticket` (set by
       `engine.copy_area`'s `store.touch_render_fence` after
       `end_and_submit_op` — fence has a queued signal op).
       Safety net for the rare `dst == cow_id` corner: force-
       flush the cow_batch first.
    2. **Bee / RADV cursor-only regression, blocking ship.**
       With the `4f566e6` fix in place: no deadlock, no UAF,
       sustained 60 FPS render activity (`cpu_fence_wait_ns/s
       = 0`). Clients (`mate-panel`, `caja`, `marco`, …) connect
       and paint their backing pixmaps successfully (panel
       backing 254KB / 256KB non-zero — fully drawn; caja-
       desktop full-screen backing 100% non-zero). COW has ~25%
       non-zero pixels (some cow_copy_areas did flush — `cow_
       batches_flushed/s = 1` peak). **But the scanout BO is
       essentially empty (541 non-zero bytes out of 11MB).**
       KMS pageflips a black framebuffer; only the cursor
       plane (which is independent of the scene-compose CB) is
       visible. Marco bails out of compositor mode with
       `Window manager error: Unable to open X display :7`
       after only 11 X requests — its compositor-helper
       subprocess is failing to open a secondary connection
       that 8ca552a accepts cleanly. Cause unknown; v2
       `scene.rs` is byte-identical to 8ca552a and the engine
       eager-touch is also identical (Task 15 restored it
       verbatim from 8ca552a).

    Hypothesis being tested: MATE's `mate-session-check-accelerated`
    GL probe routes through Mesa WSI which uses PRESENT, so the
    deferred-PRESENT path IS exercised even though MATE itself
    doesn't visibly use GL. A wrong IdleNotify timing or stale
    `state.sync_fences` bookkeeping mid-probe might leave the GL
    helper in a bad state → marco compositor child fails to
    initialise → COW pump never runs → black scanout. Removing
    `VK_KHR_external_fence_fd` from the wanted device-extension
    list as a long-shot diagnostic crashed yserver immediately
    (ash's loader-stub panics when the function is called without
    the extension enabled, even though the call site is only
    reached during PRESENT — confirms MATE startup DOES generate
    PRESENT traffic). Reverted that diagnostic.

    Cross-machine hardware coverage on the branch:
      - **silence (rx580 / RADV)**: not tested (lower priority —
        bee + yoga drive the design).
      - **bee (Rembrandt RDNA2 / RADV)**: branch + fix runs,
        cursor only. Master alone hits the original
        `ERROR_DEVICE_LOST` UAF (matches `8ca552a` chain).
        `8ca552a` itself works fully.
      - **yoga (Snapdragon X1E / Turnip)**: branch without fix
        deadlocks within 2 s of marco starting (Turnip's
        VUID-01457 enforcement). Branch + fix: not re-tested
        after the fix landed.
      - **vng smoke (Venus → host GPU)**: branch + fix runs
        glxgears at 210 FPS, `cpu_fence_wait_ns/s = 0`, vs
        master at 78–93 ms/s in synchronous waits. Useful as a
        regression gate but doesn't exercise marco's COW
        pump (glxgears targets its own window, never the COW).

    Branch is preserved on `origin/feature/deferred-present-completion`
    (commits `faa7fca`…`4f566e6`). vng smoke recipe lives in the
    Justfile. Not ready to land; the cursor-only regression
    blocks bee usability, and reverting to master re-opens the
    bee UAF.

    **2026-05-23 Codex follow-up (working tree, hardware smoke
    pending):** replaced the temporary `FenceTicket` polling
    workaround with the intended semaphore-batch design. COW
    `PRESENT::Pixmap` now attaches completion payloads to the open
    COW copy batch; flush submits the batch with one dedicated
    export-only `VkSemaphore`, exports a `SYNC_FD`, registers that fd
    with the backend's PresentCompletion epoll, and drains all events
    attached to that batch together when the fd signals. The
    `FenceTicket` stays purely internal for I6a lifetime/retirement.
    Non-COW PRESENT uses a signal-only same-queue submit as a
    correctness fallback, and the old `VK_KHR_external_fence_fd` /
    fence sync-file export path was removed. The degraded 1 ms
    polling path remains only for semaphore setup/export failure or
    Vk-less tests.

    **2026-05-23 perf follow-up:** first semaphore smoke rendered
    correctly but showed higher `cow_batches_flushed/s` under drag.
    Root cause in code review: `maybe_composite` flushed open COW /
    render batches before checking whether any output could submit;
    if a pageflip was still pending, `scene.tick` skipped after the
    batch had already become a GPU submit. Working tree now gates
    the load-bearing `maybe_composite` flush on
    `scene.has_output_ready_for_submit()` and `next_wakeup` no longer
    busy-wakes for a dirty scene that is blocked solely on pageflip /
    retry. This should let COW copies coalesce until the vblank-limited
    scanout path can consume them.

    **2026-05-23 bee hardware close — Task 6.1 functionally fixed,
    drag lag delegated to next perf phase.** Bee MATE telemetry on
    the optimised working tree (mate-session, ~30 s session, no
    instrumentation overhead): `cow_batches_flushed/s` peak 152
    (down from 218 pre-pacing-fix), `cow_copies/batch` ratio 8.09
    (up from 5.75 — aggregation now ahead of the 8ca552a baseline
    of ~5.5), `cpu_fence_wait_count/s` peak 24 (down from 28),
    `cpu_fence_wait_ns/s` peak 26 ms (down from 31 ms). PRESENT
    completion is no longer the structural bottleneck. The
    `paint_submits/s` increase (2255 → 3240, +45 %) and
    `queue_submit2/s` increase (2306 → 3304, +43 %) are back-
    pressure removal: clients + compositor are no longer artificially
    stalled by synchronous PRESENT completion, so MATE produces
    more frames worth of activity. **Subjectively bee drag still
    feels laggy under heavy load** — but that matches the previously-
    measured bee bottleneck: per-`queue_submit2` `ioctl →
    libvulkan_radeon → amdgpu` kernel round-trip overhead. We were
    at 2119/s on the perf-branch capture; we're now at 3304/s. Lag
    is structurally pre-existing and unaddressed by Task 6.1.

    **Followup filed: submit-rate reduction on bee.** Hot path is
    raw queue_submit2 frequency. Next perf phase targets bigger
    paint/render batches, fewer per-op submits, and identifying the
    top non-COW submit sources in `yserver-mate.submit.tsv` (the
    top three by row count from the 2026-05-23 bee capture were
    `render_composite=20171`, `render_fill=17973`, `composite_glyphs=8993`).
    Task 6.1 lands as-is.

    **nvidia hardware capture 2026-05-23** (NVIDIA proprietary
    driver, single output DP-2 @ 2560x1440, MATE session ~39 s).
    **Captured pre-Phase-A** (Stage 5 baseline; no `submit_group_*`
    counters in this run). First yserver-v2 run on NVIDIA proprietary;
    bootstraps clean (`driver_id=NVIDIA_PROPRIETARY`, scanout
    modifier=0x0 linear fallback — proprietary userspace doesn't
    expose modifier-friendly tilings, same shape as the rx580 RADV
    fallback noted in the silence 2026-05-22 capture). HW cursor
    plane up (64x64 ARGB8888), first pageflip immediate, no v2 gaps
    logged, no `not yet implemented` warns. 38 telemetry buckets:
      - `paint_submits/s` avg 1,641 peak 3,156;
        `queue_submit2/s` avg 1,702 peak 3,216. Same submit-rate
        ceiling as bee's 2026-05-23 post-Task-6.1 capture (peak
        3,304) on a similar single-output mate-session workload —
        confirms the bee finding that the ceiling is universal
        once coalescing is on, not RADV-specific.
      - `composite_submits/s` avg 55 peak 61, `frame_present_count/s`
        tracks 1:1; `missed_pageflips/s = 0` across all buckets.
        Display path solid.
      - `cpu_fence_wait_ns/s = 0` everywhere — Task 6.1 deferred-
        PRESENT semaphore-batch design holds on NVIDIA. No
        synchronous waits introduced by the export-fd path.
      - `descriptor_pool_creates/s` 2 across the whole run (warmup
        only); `descriptor_pool_resets/s` avg 4.7 peak 8. Ring
        recycles as designed.
      - Cow coalescing: `cow_batches_flushed/s` avg 70 peak 111;
        `cow_copies_coalesced/s` avg 536 peak 937 (avg
        `copies/batch ≈ 7.7`, peak 8.4). Render coalescing:
        `render_batches_flushed/s` avg 410 peak 749;
        `render_composites_coalesced/s` avg 543 peak 889 (avg
        `composites/batch ≈ 1.3`).
      - `storage_allocations/s` avg 495 peak 881 (single output;
        bigger than bee, smaller than dual-output silence — same
        pattern, full-output redirected backings missing the
        ≤128px PixmapPool bucket cap). Task 5 territory.
      - From the submit trace (`yserver-mate.submit.tsv`, 64 k
        rows): copy_area avg_batch 4.3 peak 26 — half of all
        copy_areas target id=39 (the COW); render_traps avg_batch
        19.6 peak 60; composite_glyphs avg_batch 12.0 peak 27;
        **`render_fill` avg_batch 1.27** — largest unexploited
        coalesce surface (22 k rows, batch ≈ 1), confirms the bee
        Task 3 followup target.
      - **Damage-saturation bug reproduces:** `damage_fraction =
        1.000` in 3 of 38 buckets, ≥0.98 in ~10 more,
        `full_redraw_fallback/s = 0` in 37/38. Same
        `pick_repaint_region` correctness gap silence first
        surfaced 2026-05-22 — now seen on a third hardware class.
      - Client-side: nvidia-libGL DRI3 doesn't bind (`glx: failed
        to create dri3 screen / failed to load driver: nvidia-drm`
        in `mate.log`). yserver's Stage 4d.3 DRI3 backfill exports
        fences/syncobjs correctly but the NVIDIA proprietary
        userspace GLX wants its own kernel hooks. mate-session-
        check-accelerated falls back to llvmpipe / skips GL —
        does not block the session.

    **air hardware capture 2026-05-25** (Apple M1, Asahi ALARM
    kernel 6.19.14-2-1-ARCH, single output eDP-1 @ 2560x1600, MATE
    session ~35 s). First yserver-v2 run on aarch64 / Apple Silicon.
    Bootstrap required one fix on the `asahi` branch: Asahi exposes
    a split-driver layout — `/dev/dri/card0` is the `asahi` GPU
    (render-only, `MODE_GETRESOURCES → EOPNOTSUPP`) and
    `/dev/dri/card2` is `apple-drm` (KMS). `resolve_drm_device` in
    `lib.rs` only probed card0/card1 and didn't distinguish render-
    only nodes; now it scans `/dev/dri/card*` and keeps the first
    whose `resource_handles()` succeeds (commit `90c5c57`). The
    Stage 4d.3 DRI3 dev-walk fallback already handled the
    parent-mismatch case so `/dev/dri/renderD128` (asahi GPU) gets
    picked up cleanly for Vulkan + DRI3 from apple-drm's card2.
    After the resolver fix: clean bootstrap
    (`driver_id=MESA_AGXV`, scanout modifier=0x0 linear fallback —
    apple-drm doesn't advertise modifier-friendly tilings, same
    shape as the rx580 RADV and NVIDIA proprietary fallbacks), no
    v2 gaps logged, no `not yet implemented` warns. HW cursor
    plane found via legacy `set_cursor2` fallback: `cursor: found
    0/1 atomic cursor planes; remaining CRTCs will use legacy
    ioctls` — apple-drm doesn't expose an atomic cursor plane;
    the 64×64 ARGB8888 cursor still works via the legacy path.
    Atomic-commit despite `DRM_CLIENT_CAP_ATOMIC` rejection works
    end-to-end — confirms the hopeful comment in `drm/device.rs`'s
    `enable_atomic_capabilities`. 35 telemetry buckets:
      - `paint_submits/s` avg 1,552 peak 9,496;
        `queue_submit2/s` avg 1,618 peak 9,554. Steady-state
        below bee's post-Task-6.1 ceiling (3,304) and nvidia's
        ceiling (3,216); the peak (9,554) shows the workload
        bursts harder than nvidia/bee but the bursts don't
        accumulate — frame production tracks composite at 60 Hz
        without backpressure.
      - `composite_submits/s` avg 53 peak 61,
        `frame_present_count/s` tracks 1:1;
        `missed_pageflips/s = 0` across all buckets. Display
        path solid.
      - `cpu_fence_wait_ns/s` avg 1.44 ms peak 31.7 ms; non-zero
        but well below bee/silence pre-Task-6.1. Task 6.1
        deferred-PRESENT semaphore-batch design holds on AGX.
      - `descriptor_pool_creates/s` 2 across the whole run
        (warmup only); `descriptor_pool_resets/s` avg 4.6 peak
        9. Ring recycles as designed on a 4th driver
        (Mesa AGX-V joins RADV, NVIDIA, Turnip).
      - Cow coalescing: `cow_batches_flushed/s` avg 77 peak 160;
        `cow_copies_coalesced/s` avg 591 peak 1,102 (avg
        `copies/batch ≈ 7.7`, matching nvidia). Render
        coalescing: `render_batches_flushed/s` avg 368 peak
        1,951; `render_composites_coalesced/s` avg 523 peak
        2,069 (avg `composites/batch ≈ 1.34`).
      - `storage_allocations/s` avg 443 peak 1,887 (single
        output @ 2560x1600; matches nvidia's 495/881 shape on
        the same workload — full-output redirected backings
        missing the ≤128px PixmapPool bucket cap). Task 5
        territory.
      - From the submit trace (`yserver-mate.submit.tsv`, 57 k
        rows): `composite_glyphs` avg_batch 10.30 peak 62;
        `render_traps` avg_batch 17.06 peak 39; `copy_area`
        avg_batch 3.97 peak 26 (cow target dominates as
        elsewhere); `render_composite` avg_batch 1.34 peak 6;
        **`render_fill` avg_batch 1.24** (17,897 rows) — same
        largest unexploited coalesce surface as nvidia and bee.
      - **Damage-saturation bug reproduces** on a 4th hardware
        class: `damage_fraction = 1.000` in several buckets,
        `full_redraw_fallback/s ≈ 0`. Same `pick_repaint_region`
        correctness gap silence first surfaced 2026-05-22 —
        AGX joins RADV/silence and NVIDIA/proprietary. Did not
        visibly smear on this run.
      - User subjective: **completely lag-free** under the same
        MATE drag workload that lags on bee. Bee remains the
        sole hardware where steady-state submit rate binds the
        user-perceived envelope; the M1's aarch64 ioctl path
        and/or asahi GPU submit path absorb the load that
        Rembrandt's amdgpu round-trip cannot.

    **2026-05-23 bee wrapper-overhead baseline (Phase A T3.5 + T3.6
    landed):** `queue_submit2/s` peak **2457** (pre-Phase-A peak was
    3304; ~25 % lower already, likely workload variance + the
    deferred-PRESENT-completion fix from 41f6fbb in-tree). Wrapper
    is bit-identical to today's per-op cadence at `max_size=1`:
    `submit_group_size_avg` 1.00-1.04, histogram dominated by the
    size-1 bucket, `submit_group_flush_reason_max_size/s` accounts
    for ≥ 95 % of flushes. `submit_group_aborts/s` = 0 throughout.
    `active_descriptor_pool_count_high_water` = 2 (no ring pressure).
    `active_staging_bytes_high_water` 14.7 MB. Histogram occasionally
    spikes to size=13 from `image_text`/`composite_glyphs` glyph-
    upload loops (documented borrow-factoring exception where the
    inner-loop appends defer their flush to the outer paint-op's
    tail). All T3.6 stop-and-investigate conditions clear — proceeding
    to T4 (cap=16 + scene-compose flush).

    **2026-05-23 yoga Phase A full capture** (Snapdragon X1E / Adreno
    X1 / Turnip, MATE drag, full Phase A in tree at HEAD `189e8dd` —
    cap=16, all four flush triggers (sync_boundary, scene_compose,
    pageflip_retire, present_completion_signal) wired, plus the T6/T7
    close-batches-before-flush fix (189e8dd) and the empty-flush
    open-batch-ticket fix (b1cbbe9) that bee MATE drag at cap=16
    surfaced as latent invariant violations):
      - `queue_submit2/s` steady 1500-2100, peak 5752 (vs pre-Phase-A
        yoga peak ~8200; ~30 % collapse on peaks, similar on steady).
      - `submit_group_size_avg` steady 6.0-7.5, peak 12.19;
        `submit_group_size_max_in_window` mostly cap-bound at 16.
      - `submit_group_flushes/s` steady 250-350, peak 610.
      - Flush-reason split: `present_completion_signal/s` 114-180
        (dominant — every COW PRESENT), `scene_compose/s` ~60,
        `pageflip_retire/s` ~60, `max_size/s` 60-142, `sync_boundary/s`
        6-92 (get_image bursts), `shutdown/s` 0.
      - `submit_group_aborts/s` = 0 (no failed submits, no UAFs).
      - `active_staging_bytes_high_water` = 21.7 MB (matches spec's
        "16 × worst-op footprint" envelope).
      - `active_descriptor_pool_count_high_water` = 2 (no ring
        pressure on this hardware).
      - `cpu_fence_wait_ns/s` steady 0; occasional 12-88 ms bursts
        on `get_image` paths.
      - **Anomaly:** `submit_group_size_max_in_window` shows values
        > cap (18, 24, 25, 26) in some peak rows. Reproduces on iMac
        too — real telemetry-or-cap-check bug, not yoga-specific.
        Filed but not Phase-A blocking.
      - User subjective: lag-free, but yoga was already lag-free
        pre-Phase-A (Task 4 layer 1 fixed yoga's pathology). So no
        perceptible win, no regression — what we expected.

    **2026-05-23 iMac 19,2 Phase A capture** (NEW analogue platform,
    Intel i5-8500 + Radeon Pro Polaris Baffin / GCN4 / RADV,
    `connector=eDP-1` at 3840x2160, Ubuntu, same Phase A branch
    `189e8dd`). Same GCN4 generation as `silence`'s rx580. See
    [[reference-imac-19-2-bee-analogue]]:
      - `queue_submit2/s` steady 2400-3700, peak 3671.
      - `submit_group_size_avg` steady **9-10.6** (notably higher
        than yoga's 6-7.5) — same code + cap, AMD just produces
        more consecutive paint CBs between flush triggers because
        the 4K display drives `render_batches_flushed/s` 700-950
        (vs yoga's 350-500).
      - `max_size` flush-reason fires **140-250/s** = 50-55 % of all
        flushes (vs yoga's ~20 %). **Spec's "cap=16 is a guess,
        retune from telemetry" open question now has a concrete
        answer for AMD: cap is too low.** Filed as Phase A T15
        tuning input.
      - `submit_group_aborts/s` = 0, no panics, no errors. Only
        warning is the same `VK_EXT_image_drm_format_modifier`
        fallback `silence` (rx580) hit — modifier path missing on
        Polaris under RADV but the desktop renders correctly via a
        fallback scanout path. Not Phase-A-induced.
      - Clean shutdown (`shutting down` → `master released` →
        `console state restored`). An earlier pre-Phase-A run on
        this iMac hung on zap; the Phase A run shuts down cleanly.
        Single observation each, but it's the inverse of "Phase A
        introduced a shutdown bug."
      - Same `submit_group_size_max_in_window > cap` anomaly as
        yoga (sgm=18, 20, 25, 26 in peak rows) — confirmed
        platform-independent.

    **2026-05-23 fuji Phase A capture** (Intel ANV / Mesa
    `driver_id=INTEL_OPEN_SOURCE_MESA`, `connector=eDP-1` at
    1920x1080, MATE drag, same Phase A branch `189e8dd`; 34
    one-second buckets; clean Ctrl-Alt-Bsp zap shutdown). Third
    green analogue platform alongside yoga + iMac:
      - `queue_submit2/s` avg 1350 peak 1802 (well below bee
        post-T6.1 peak 3304; squarely in the spec's Phase A
        target band of 900-1500 on average, slightly above on
        peaks).
      - `paint_submits/s` avg 1284 peak 1719.
      - `submit_group_size_avg` avg 5.61 peak 6.75 (between
        yoga's 6.0-7.5 and iMac's 9-10.6 — Intel sits in the
        middle).
      - `submit_group_flushes/s` avg 276 peak 391.
      - Flush-reason split: `present_completion_signal/s` avg
        107 peak 149 (dominant, COW PRESENTs), `pageflip_retire/s`
        58, `scene_compose/s` 58, `max_size/s` avg 46 peak 65
        (**17% of all flushes** — between yoga's ~20% and iMac's
        50-55%, so cap=16 is fine on Intel), `sync_boundary/s`
        avg 7 peak 120 (bursty get_image), `shutdown/s` 0.
      - `submit_group_aborts/s` = 0 throughout; no panics, no
        `renderer_failed`.
      - `active_descriptor_pool_count_high_water` = 2 (no ring
        pressure). `active_staging_bytes_high_water` avg 7.6 MB
        peak 8.3 MB (smaller envelope than yoga's 21.7 MB —
        Intel is the lightest of the three on retention).
      - `composite_submits/s` = `frame_present_count/s` = 58,
        `missed_pageflips/s` = 0 across all buckets — display
        path solid at 60 Hz.
      - `cpu_fence_wait_ns/s` avg 96 ms with a single 3.2 s peak
        bucket (`cpu_fence_wait_count/s` peak = 2 so it's one
        get_image burst spanning a bucket boundary, not a hot-
        loop wait).
      - Histogram healthy across all six buckets; size-1 still
        ~31% (cap=1 forced flushes — sync_boundary + the per-op
        get_image / cow with attached completion paths), size-
        13+ consistently populated → cap actually doing work.
      - Same `submit_group_size_max_in_window > cap` anomaly
        (sgm=17, 21, 24, 28, 36 in peak rows). Now reproduced
        on yoga + iMac + fuji — fully platform-independent
        telemetry-or-cap-check bug; worth filing as a real
        defect rather than rounding.
      - **Triangulation update:** bee freeze now has *three*
        green analogues on the same `189e8dd` commit — yoga
        (Adreno/Turnip), iMac (Polaris/GCN4/RADV), fuji
        (Intel/ANV). The remaining shared variable between green
        and frozen is RDNA2 RADV path × Arch Mesa-current.

    **2026-05-23 bee MATE-load freeze (KNOWN, deferred to Phase B):**
    with full Phase A in tree at `189e8dd`, yserver
    loads MATE and then freezes on bee (Ryzen 9 6900HX / RDNA2,
    Arch Linux). Follow-up logs captured on 2026-05-24 show this is
    not an event-loop deadlock: RADV reports a GPUVM TCP protection
    fault, then yserver sees `ERROR_DEVICE_LOST`; Ctrl-Alt-Bsp still
    reaches the input thread and shutdown path.
    Cross-platform triangulation tonight makes the hypothesis space
    much smaller:
      - **Not generic Phase A bug** — yoga (Adreno/Turnip) and iMac
        (Polaris/RADV) both run Phase A clean on the same commit.
      - **Not generic RADV/amdgpu bug** — iMac is RADV/amdgpu and
        works.
      - **Not generic kernel-7.0 bug** — both bee and iMac run
        Linux 7.x kernels.
      - **Not CPU-budget** — bee has 8c/16t vs iMac's 6c/6t; bee
        has *more* single-thread headroom.
      - **Narrowed to:** RDNA2-specific RADV/kernel code paths
        AND/OR Arch's Mesa-current vs Ubuntu's older Mesa. Bee is
        `AMD Radeon 680M (RADV REMBRANDT)`, `driverName=radv`,
        `driverInfo=Mesa 26.1.0-arch2.1`, kernel
        `7.0.9-1-cachyos`. iMac is Ubuntu 26.04 + kernel 7.0.x,
        so "kernel 7.0" alone is no longer a useful separator.
      - **SubmitGroup cap sweep:** a diagnostic
        `YSERVER_SUBMIT_GROUP_MAX_SIZE` knob was added for hardware
        isolation. `cap=1` boots MATE and survives (`submit_group_size_max`
        mostly 1, with known glyph-loop exceptions still able to
        exceed the nominal cap). `cap=2` reproduces the same RADV
        GPUVM fault within MATE load:
        `GCVM_L2_PROTECTION_FAULT_STATUS=0x701031`,
        `ERROR_DEVICE_LOST`, longest request `op133` (RENDER) at
        ~1.98 s. Stop sweeping higher caps; the failure is the first
        regular multi-CB submit-group shape, not a high-cap tuning
        cliff.
      - **Abandoned submit-shape experiment:** changing
        `flush_submit_group` from one `VkSubmitInfo2` containing N
        command buffers to one `vkQueueSubmit2` call containing N
        one-command-buffer submits moved `cap=16` farther (MATE
        desktop appeared), but bee still froze with the same RADV
        GPUVM fault during a `composite_glyphs` burst. This makes the
        env knob useful as a hardware diagnostic, but the fix path is
        the later real frame-builder phase, not more SubmitGroup
        tuning.

    **Phase A — CLOSED 2026-05-24.** Plan
    `docs/superpowers/plans/2026-05-23-frame-builder-submit-rate-phase-a.md`
    landed in 27 commits on `feature/frame-builder-submit-rate`
    (`d1cd19d` … `394bf7b`). All 15 plan tasks implemented; the only
    open items are deferred to Phase B by design, not by execution gap:
      - **Hardware validation:** three of four hardware classes green
        on `189e8dd` (yoga / Adreno / Turnip; iMac 19,2 / Polaris /
        GCN4 / RADV; fuji / Intel / ANV). `queue_submit2/s` peak
        collapses match the spec target band (900-1500/s) on Intel
        and approach it on the AMD/Adreno analogues; `cpu_fence_wait`
        steady-state ~0 except `get_image` bursts; `submit_group_aborts`
        = 0; no panics, no `renderer_failed`.
      - **bee (Ryzen 9 6900HX / RDNA2 / RADV / Arch Mesa-current):**
        MATE-load RADV GPUVM TCP protection fault → `ERROR_DEVICE_LOST`.
        Reproduces at `cap=2` (first multi-CB submit shape); not a
        high-cap tuning cliff. Three green analogues triangulated the
        hypothesis space to RDNA2 RADV path × Arch Mesa-current.
        **Accepted as a known issue; Phase B is the fix path** —
        per-frame deferred op-list recording removes the cap=N-CB
        submit shape that bee's RADV/RDNA2 stack faults on.
      - **Carried forward to Phase B telemetry/tuning** (not Phase A
        blockers):
          - AMD `max_size`-flush share 50-55 % on iMac vs 17-20 % on
            Intel/Adreno — cap=16 is too low for AMD's batch shape;
            Phase B's frame-CB makes per-frame the natural unit.
          - `submit_group_size_max_in_window > cap` telemetry anomaly
            reproduces on yoga + iMac + fuji — real telemetry-or-cap-
            check defect; survives Phase A but is non-blocking.
      - **Followups intentionally punted to Phase B** (per spec
        § Out of scope): op-list frame builder, multi-output frame
        CB, glyph-upload deferred recording, transactional layout
        state, frame-wide resource pinning, idle/no-pageflip trigger
        model. See `2026-05-23-frame-builder-submit-rate-design.md`
        § "Phase B" for the design sketch.

  - **Phase B sub-phase B.1 — IMPLEMENTED 2026-05-24.** Plan
    `docs/superpowers/plans/2026-05-24-frame-builder-phase-b1.md`
    landed in 25 commits on `feature/frame-builder-submit-rate`
    (`eb399c2` … `ea24914` + lint cleanup `f6dd508`). Spec
    `docs/superpowers/specs/2026-05-24-frame-builder-phase-b-design.md`.

    **Structural changes:**
      - New `FrameBuilder` on `RenderEngine` (`Closed ↔ OpenForPaint`
        lifecycle), with `RecordedOp` enum (`CompositeGlyphs`,
        `GlyphUpload`, `LayoutTransition`), `FramePinSet` (Arc-pinned
        `StagingBuffer`), `FrameLayoutTable` (pre-frame/in-frame
        snapshot for rollback), `TouchedDrawables`,
        `PendingGlyphInserts`, `FrameSubmittedRecord`, `CloseReason`
        (8 variants), `CloseOutcome`, `FrameCloseEvent`.
      - **Invariant M1**: `SubmitGroup::new()` default cap 16 → 1.
        Every queue submission carries exactly one CB for the
        duration of B.1–B.4. Bee MATE survives this trivially
        (the `cap=1` row from the 2026-05-23 capture). Other
        platforms see a temporary submit-rate regression that
        recovers at B.5 when SubmitGroup retires entirely. The
        legacy `YSERVER_SUBMIT_GROUP_MAX_SIZE` env override was
        removed (it could re-enable the multi-CB shape that bee
        faults on).
      - **Invariant M2**: `RenderEngine::close_open_frame_for_non_ported_op`
        helper wired at the top of every non-ported paint op
        (`fill_rect`, `fill_rect_batch`, `logic_fill`, `copy_area`,
        `cow_copy_area`, `put_image`, `image_text`, `render_composite`,
        `render_fill_rectangles`, `render_traps_or_tris`). No-op when
        no frame is open (preserves existing batch coalescing); when
        a frame is open, flushes cow/render batches first then
        closes.
      - **Invariant M3**: `maybe_composite` closes the open frame
        before flushing the legacy compose path.
      - **Close triggers wired** (8): M2, M3, PRESENT-completion at
        `enqueue_present_completion`, get_image SyncWait, 16 ms
        timeout (env knob `YSERVER_FRAME_BUILDER_TIMEOUT_MS`),
        shutdown via new `RenderEngine::shutdown`, pin-set ceiling
        (1024 default, log-once latch).
      - **`composite_glyphs` ported** through the FrameBuilder.
        Glyph upload commands + draw commands record into one CB
        per frame, submitted via the SubmitGroup's cap=1 auto-flush
        — one `vkQueueSubmit2` per frame's composite_glyphs burst,
        regardless of glyph miss count.
      - **3-pass close walk** in `RenderEngine::close_open_frame`:
        resource pass (no-op in B.1) → record pass → finalise pass.
        Commit-after-`flush_submit_group`-Ok ordering. All 4 error
        paths roll back drawable layouts, drawable `last_render_ticket`,
        atlas layout, and atlas `last_render_ticket`; pending glyph
        inserts drop on failure.
      - **`pending_frames` retirement queue** parallel to
        `submitted`. `poll_retired` drains both. The frame's
        `Arc<StagingBuffer>` pins drop only after the frame's
        FenceTicket signals.
      - **Telemetry**: `frame_builder_opens` / `closes` / `aborts` /
        `ops_per_frame_total|max|hist[8]` / `glyph_uploads_per_frame_*`
        / `active_pins_high_water` / 8 per-CloseReason counters.
        `drain_frame_builder_telemetry()` helper invoked at every
        close-driving backend site (`maybe_composite`,
        `enqueue_present_completion`, `get_image`, `disable_output`,
        `render_composite_glyphs`).
      - **Gate**: `YSERVER_FRAME_BUILDER` defaults ON (Task 24's
        `ea24914` flips the bee fix into production); set
        `YSERVER_FRAME_BUILDER=off` (or `0`/`false`/`no`) as the
        kill-switch.

    **Spec-side closure (5 codex review rounds applied to the plan
    before any implementation landed):**
      - Commit-after-flush-Ok ordering (R1).
      - M2 batch ordering relative to existing cow/render batches (R1).
      - Layout overlay first-touch at append time (R1).
      - Gate-flip isolated to a single bisect-clean commit (R1).
      - PRESENT-completion close trigger at the real semaphore submit
        site (`backend.rs::enqueue_present_completion`), not the
        unrelated `attach_cow_present_completion` (R1).
      - Atlas rollback invoked from every error path (R2).
      - Pin-ceiling enforcement with pre-pass HashSet dedupe before
        any pack/allocate (R2/R3/R4).
      - M2 helper conditional on frame-open to preserve batch
        coalescing (R3).
      - Telemetry drain coverage at every close-driving call site
        (R3).
      - Pixel-length validation before atlas pack (R5).

    **Acceptance:**
      - **Implementation gates** (already validated): all 25 commits
        green at HEAD; `cargo build -p yserver` clean; `cargo test
        --workspace` shows 1038 tests passing (the existing 242
        `kms::v2` lib tests, plus 23 new `frame_builder` unit tests
        across state/op/pin/layout/touched/glyph_insert/open_frame/
        lifecycle modules); `cargo +nightly fmt` clean;
        `cargo clippy -- -W clippy::pedantic` clean on the B.1
        surface (pre-existing warnings outside B.1 left alone for
        independent cleanup).
      - **Hardware gates:**
        - **bee MATE-load survival — PASS 2026-05-24** (capture
          `yserver-hw-mate.log` + `yserver-mate.submit.tsv` at
          22:25). Boot MATE with default `YSERVER_FRAME_BUILDER=on`,
          drag for ~43 s, clean Ctrl-Alt-Backspace zap. Zero
          `ERROR_DEVICE_LOST`, zero RADV GPUVM faults, zero panics.
          Compare with the May 23 master-bee capture
          (`yserver-hw-mate-master-bee.log`) on the same hardware:
          RADV GPUVM fault at `0x8001040a0000` /
          `GCVM_L2_PROTECTION_FAULT_STATUS=0x601031` ~5 s into MATE
          load (the documented "2026-05-23 bee MATE-load freeze").
          B.1 telemetry confirms the structural fix: frame builder
          opens/closes matched (`aborts=0` throughout),
          `submit_group_size_max_in_window=1` every sample (M1
          enforced), composite_glyphs bursts collapse into single
          frames (peak `glyph_uploads/frame_max=27` in one frame —
          the same `composite_glyphs` shape that faulted master-bee
          now records as one frame CB and one submit). The bee
          freeze is closed.
        - **bee MATE-load smoothness — NOT YET** (same capture).
          Peak `queue_submit2/s=2341` during MATE drag, above the
          pre-Phase-A 8ca552a baseline (1880/s) that was already
          laggy. Submit-source ranking from the May 24 submit-trace:
          render_fill 22885 + render_composite 21854 (~60 %) +
          put_image 8082 + copy_area 7591 are all unported, every
          one of them its own `vkQueueSubmit2` ioctl under cap=1.
          composite_glyphs (8606, 11.6 %) IS collapsing through the
          frame builder. User-side feel: ~1 s initial-drag hitch
          (cold atlas intern + descriptor-pool grow + 14.7 MB
          staging high-water in the first burst), then runs but
          never smooth. Matches the spec's explicit design: B.1 is
          the freeze fix; B.2 (porting render_composite +
          render_fill) is the smoothness fix.
        - **yoga / iMac / fuji regression check — pending.** Same
          MATE drag, expect no new `ERROR_DEVICE_LOST` and
          `queue_submit2/s` ≤ Phase A peak. Non-glyph paint ops on
          these platforms regress temporarily (cap=1) and recover
          at B.5.
        - **silence dual-output regression check — pending.**
          Confirm both outputs present correctly via the existing
          per-output compose path (B.1 doesn't fold compose;
          single-output is the only flavor exercised).

    **Open follow-ups:**
      - Two integration tests (`v2_frame_builder_renderer_failed_on_submit_failure`,
        `v2_frame_builder_mixed_sequence_smoke`) are SCAFFOLDED as
        `#[ignore]` placeholders pending end-to-end test-side glyph
        fabrication helpers. Structural correctness is verified by
        spec review of Task 12's 4 error-path rollbacks + Task 14's
        M2 wiring + Task 15's first-touch overlays.
      - Q1 (op variant sizing — 256 B test gate enforced).
      - Q3 (gate retirement timing) — env var stays as kill-switch
        through B.4; final removal at B.5 alongside SubmitGroup
        deletion.
      - Q5 (semaphore export ordering) — the implementation preserves
        the submit-then-export shape Phase A established; the Q5
        SYNC_FD pass-through test is deferred to the scaffolded
        renderer-failed integration test bucket.

  - **Phase B sub-phase B.2 — IMPLEMENTED 2026-05-25.** Plan
    `docs/superpowers/plans/2026-05-24-frame-builder-phase-b2.md`
    landed in 19 commits on `feature/frame-builder-submit-rate`
    (`70512d0` … `dbb540d`). Spec
    `docs/superpowers/specs/2026-05-24-frame-builder-phase-b-design.md`.

    Commit chain in order (Task 1 → Task 20):
    `70512d0`, `b6b3d4b`, `5d4266c`, `f13baac`, `7526716`,
    `21a3f52`, `6994658`, `61eb2ad`, `157fa4e`, `fb57a83`,
    `f9852e4`, `d31b5d1`, `a9813d8`, `cff6e62`, `9e6ecca`,
    `a3dd080`, `e80adfb`, `7125002`, `dbb540d` (default-ON flip).

    **Structural changes:**
      - Mechanism 3 (retired-scratch pinning via existing BatchResource
        trait): `EngineInner` scratch slots stay as `Option<DstReadback>` /
        `Option<MaskScratch>` / `Option<SolidColorImage>` (no Arc-wrap;
        the existing `&mut`-mutating record APIs stay unchanged). The
        existing `ensure_returning_old` already returns
        `Option<Box<dyn BatchResource>>` on growth; B.2 routes the Box
        into the open frame's `retired_resources: Vec<Box<dyn BatchResource>>`
        pin slot instead of dropping it on the floor. The Box's Drop
        destroys the Vk handles after the frame ticket signals.
        Closes the existing retired-scratch leak documented at
        `engine.rs:529-535`.
      - Mechanism 2 (descriptor pool ring watermark): `OpenFrame::frame_generation`
        captured at `open_for_paint` from a bumped `acquire_generation`;
        all per-op descriptor acquisitions during the frame use that
        generation; `release_up_to(frame_generation)` at retire releases
        only the frame's pools.
      - Layout overlay flips to source-of-truth: open-frame paint ops
        read `current_in_frame_for_drawable` via the new
        `RenderEngineInner::current_layout_for_drawable` accessor.
        Second `render_composite` op-in-frame sees the prior op's
        post-transition layout, not stale `storage.current_layout`.
      - `RecordedOp::RenderComposite` + `RecordedRenderComposite`
        payload — all resolved view handles, descriptor set, rects,
        clip.
      - `render_composite_via_frame_builder` + `render_composite_legacy`
        dispatch behind `YSERVER_FRAME_BUILDER_RENDER_COMPOSITE`
        sub-gate. Default ON after Task 20.
      - `emit_recorded_render_composite_into_cb` close-time replay.
      - M2 `close_open_frame_for_non_ported_op` removed from
        `render_composite` and `render_fill_rectangles` entry points
        (they ARE the frame builder now). M2 still wraps the remaining
        8 non-ported entry points until B.3.
      - Telemetry: `frame_builder_renders_per_frame_total` /
        `_max_in_window`, drained at every close-driving backend site.

    **Acceptance:**
      - **Implementation gates** (already validated): `cargo build` clean;
        `cargo test --workspace` green; `cargo +nightly fmt` + plain
        `cargo clippy --workspace --all-targets` clean on the B.2
        surface (AGENTS.md: no pedantic by default).
      - **Hardware gates:**
        - **bee MATE-load correctness / survival — PASS 2026-05-25**
          (captures `yserver-hw-mate.log`,
          `yserver-hw-mate-vkdebug.log`, `yserver-mate.submit.tsv`).
          Post-fix bee boot reaches MATE, renders correctly, and
          survives drag with no `ERROR_DEVICE_LOST`, no panics, and
          `frame_builder_aborts=0` throughout. The last load-bearing
          replay bug was B.2's treatment of
          `SetPictureClipRectangles(n=0)`: backend storage correctly
          distinguished `clip=None` ("paint everywhere") from
          `clip=Some([])` ("empty clip, paint nothing"), but
          `emit_recorded_render_composite_into_cb` collapsed both to
          the same full-extent scissor fallback. On bee this replayed
          empty-clipped composites as whole-target redraws, producing
          the "icons appear briefly, then window goes shadow-only"
          failure. Fix: preserve the distinction at replay time —
          `Some(empty)` now closes without drawing, `None` still uses
          full-extent scissor. After that fix the B.2 path is
          functionally correct on bee.
        - **bee MATE-load smoothness — NOT YET (2026-05-25 telemetry).**
          The new run is slightly better subjectively than B.1 but
          still laggy, and the telemetry says why. Over the captured
          drag window, `queue_submit2/s` averaged **2266** and peaked
          at **2959**; `paint_submits/s` averaged **2190** and peaked
          at **2881**. CPU fence waits are no longer the problem
          (`cpu_fence_wait_ns/s` avg **15 ms**, peak **44 ms**), so
          the remaining lag is still submit-rate pressure on bee. The
          frame builder itself is healthy but not yet dominant:
          `frame_builder_opens/s` averages **897** (peak **1137**),
          `renders/frame_avg` averages **1.67** (peak **4**), while
          `close_reasons[non_ported]` still averages **873/s** (peak
          **1115/s**). Combined with Phase-B invariant M1
          (`submit_group_size_max_in_window=1` every sample;
          `submit_group_size_avg≈0.93-0.95`), this means B.2 is
          constantly forced to close the frame for still-unported ops,
          and each close becomes its own `vkQueueSubmit2`. Net:
          correctness is fixed, but smoothness still waits on later
          B-phases that reduce `non_ported` close pressure; this is no
          longer a fence-wait bug.
        - **yoga / iMac / fuji regression check — PENDING.** Same drag,
          no new `ERROR_DEVICE_LOST`, no fault chains. Expected to
          IMPROVE on these platforms vs B.1 (cap=1 reverts to per-op
          submit; B.2 collapses RENDER → one submit per frame).
        - **silence dual-output regression check — PENDING.** Same
          drag, confirm both outputs paint correctly under
          render_composite → frame builder.

    **Open follow-ups:**
      - 2026-05-25 bee vkdebug follow-up: latest post-B.2 run no
        longer reports descriptor-pool reset, semaphore-destroy, or
        shader-demote VUIDs. Remaining hard VUID was sampled-image
        layout on a newly allocated `dst_readback` scratch during
        `op=13` (`PictOpSaturate`) composites: append-time wrote the
        descriptor for binding 2, but deferred replay only copied the
        self-alias scratch and skipped the normal `dst -> dst_readback`
        copy, leaving the scratch globally `UNDEFINED` at draw submit.
        Local follow-up replays `DstReadback::record_copy_from` for
        every recorded composite with `needs_dst_readback`, before the
        draw. The earlier FreePixmap close-boundary guard remains as a
        conservative lifetime fence for already-recorded descriptor
        views. Hardware re-test pending.
      - 2026-05-25 bee post-fix telemetry follow-up: B.2 now renders
        correctly on bee, but submit pressure remains fundamentally a
        `non_ported` close problem, not a sync problem. The captured
        run (`yserver-hw-mate.log` + `yserver-mate.submit.tsv`) shows
        `queue_submit2/s` avg **2266** peak **2959** with
        `cpu_fence_wait_ns/s` avg **15 ms** peak **44 ms**. Frame
        builder close telemetry points at the real limiter:
        `close_reasons[non_ported]` avg **873/s** peak **1115/s**,
        while `submit_group_size_max_in_window=1` confirms M1 is still
        forcing one-submit-per-close. So B.2 has cleared the replay
        correctness work; smoothness now depends on later B-phase
        porting that keeps more paint inside the frame instead of
        closing for legacy ops.
      - Q1 (op variant sizing) — measured at B.2 close;
        `RecordedRenderComposite` is 264 B, well under the 512 B
        budget. No Box-`rects` follow-up needed.
      - Q3 (gate retirement) — env knob stays as kill-switch through
        B.5.
      - DescriptorPoolRing Mechanism 2 watermark wire-through —
        validated via the existing `release_up_to(op.generation)` call
        site (engine.rs:744 in `poll_retired`); B.2 doesn't add new
        retire sites.

  - **Phase B sub-phase B.3 — CLOSED 2026-05-26.** Plan
    `docs/superpowers/plans/2026-05-25-frame-builder-phase-b3.md`
    landed in commits `67ff198` … `2300db1` on
    `feature/frame-builder-submit-rate`. Spec
    `docs/superpowers/specs/2026-05-25-frame-builder-phase-b3-design.md`.

    All 8 remaining non-ported paint ops (`copy_area`, `cow_copy_area`,
    `put_image`, `fill_rect`, `fill_rect_batch`, `logic_fill`,
    `image_text`, `render_traps_or_tris`) are now FrameBuilder-resident.
    M2's only remaining call site is `render_composite_legacy`
    (engine.rs:~5877) behind the
    `YSERVER_FRAME_BUILDER_RENDER_COMPOSITE` kill-switch — with the
    switch ON (default), zero non_ported close events fire in steady
    state.

    **Mechanism changes:**
      - `SubmittedOp::scratch` is `Vec<ScratchImage>` (was
        `Option<ScratchImage>`). Single self-overlap scratch lives on
        `RecordedCopyArea` from append until the close-path walk
        migrates it into the vec (N8).
      - `pending_cow_batch` and `flush_cow_batch` infrastructure
        DELETED. X PRESENT completions for COW frames now migrate to
        `OpenFrame::pending_present_completions`, acquired via
        `PresentCompletionSignal` BEFORE the submit (N10 — three-branch
        close-path: acquire signal pre-submit, drain post-flush-success,
        force-enqueue degraded `PendingPresentBatch` on flush-failure;
        never silent-drop).

    **Bug found in user testing (yoga):**
      - Task 12 hotfix at `2300db1`: `frame_generation` was re-fetched
        from the already-`take()`n `OpenFrame` option inside the emit
        dispatch for `RecordedOp::RenderTrapsOrTris`, causing a panic.
        Fix: thread `frame_generation` through the emit dispatch instead
        of re-fetching.

    **Yoga hardware-smoke capture (2026-05-26) — PASS:**
      - `close_reasons[non_ported]/s` = **0** across the entire run
        (target ≤ 10). Every paint op routes through the frame builder;
        the rip-and-replace closed the non_ported escape hatch
        end-to-end. ✓ exceeded.
      - `submit_group_flushes/s` = **137–524, mostly 180–400**
        (target ~200–400). ✓ in range.
      - `frame_builder_aborts/s` = **0** (target 0). ✓
      - Clean shutdown, zero regressions on the yoga workload.
      - `ops/frame_avg` = **18–115** (target ~4–8). Much higher
        collapse than predicted; not a regression — the spec author
        derived 4–8 from a per-op-family multiplier that undercounts
        what a single open frame absorbs between close triggers.
        Real math: at ~100 closes/s and 1800–8000 underlying paint
        ops/s on a desktop session, 18–80 ops/frame is the correct
        steady state. Peak single frame absorbed 355 ops (still under
        the 1024 pin ceiling, so `pin_ceiling` close trigger never
        fired). Worth watching: per-frame close-time latency
        (`avg_compose_cb_record_ns` / `iter_wall_max`) on fat frames
        — if record stays bounded under a few ms, the high collapse
        ratio is pure win.
      - Other surviving close reasons (informational): `sync_wait/s`
        0–116 (get_image bursts), `present_completion/s` 0–15 (COW
        PRESENT), structural `frame_builder/s` 60–164 (60 Hz scene
        compose + sundry).

    **Bee hardware-smoke capture (2026-05-26) — PASS (after the
    78f71fe layout-overlay fix):**
      - `close_reasons[non_ported]/s` = **0** across the entire run
        (target ≤ 10 vs ~900–1100 pre-B.3). ✓ exceeded.
      - `submit_group_flushes/s` = **113–557, mostly 180–510 under
        drag** (target ~200–400). ✓ in range. ~4× reduction vs the
        spec's ~2300/s pre-B.3 estimate; combined with B.2's ~75 %
        absorption this matches the spec's 30–50 % additional drop
        target.
      - `frame_builder_aborts/s` = **0** (target 0). ✓
      - **Lag-free**: bee MATE-drag is smooth for the first time
        across Phase A / B.1 / B.2 / B.3 — the smoothness fix the
        whole phase was chasing. The dominant remaining CPU cost is
        `cpu_fence_wait_ns/s` 60M–220M (60–220 ms/s) tied to
        `sync_wait/s` 56–124 — bee MATE applets doing get_image
        readbacks. Orthogonal to frame-builder rate; would be the
        next target.
      - `ops/frame_avg` = **19–47, peaks 47 under drag** (target
        ~4–8). Same spec undercount as yoga. Peak single frame
        absorbed 252 ops; still well under the 1024 pin ceiling.
        `avg_compose_cb_record_ns` stays 100K–300K (0.1–0.3 ms per
        close) — record-time bounded.
      - Other surviving close reasons (informational): `legacy_sc/s`
        ~50 (kill-switch-off scene compose), `sync_wait/s` 56–124,
        `present_completion/s` 0–25, structural `frame_builder/s`
        110–178.
      - Zero new warnings / errors / VUIDs (only Ctrl-Alt-Backspace
        zap on shutdown).
      - **Pre-fix history:** first bee run after B.3 implementation
        landed showed half-rendered desktop. Two hotfixes resolved
        it:
          1. `449e732` — Arc-wrap `GradientPicture` so client
             `FreePicture` between trap-op append and frame close
             doesn't destroy the GPU resources mid-record. Caught
             the "missing at emit — was present at append" warns.
          2. `78f71fe` — every frame-builder body (B.1's
             `composite_glyphs`, B.2's src/mask Drawable arms, B.3
             Tasks 2/6/8/10/14) was reading `storage.current_layout`
             instead of `inner.current_layout_for_drawable`. The
             overlay is the source of truth between op-appends; the
             body rewrites missed it from copying the wrong B.1
             template. RDNA2 surfaced this as visible corruption;
             yoga's GPU was lenient. Codex (out-of-session) found
             and fixed all five B.3 sites + the B.1/B.2 sites in
             one diff.

    **Post-PASS Task 12 hotfix 3 — `b0b57f8` 2026-05-26.** Bee +
    silence both rendered the mate-control-center appearance dialog
    with α=0 across marco's SSD title bar / side borders / bottom
    action row — visible as "partially transparent" CSD chrome over
    whatever was behind. Drawable-dump PAM RGB+α extraction confirmed
    RGB correct everywhere; α bimodal (1 over the inner client area
    + the Help/Close button islands, 0 elsewhere) — exactly matching
    marco's SSD frame regions. Bisect inside the B.3 chain:
    `c4083fc` (Task 10) clean, `2300db1` (Task 12 + frame_generation
    hotfix) bad → regression in `369c8c2` (Task 12 base, trap-emit
    body). Root cause: `emit_recorded_render_traps_or_tris_into_cb`
    used a `StorageCompositeTarget` built from
    `storage.current_layout` and called `record_render_composite`,
    which reads `dst.current_layout()` for the `to_color` barrier.
    Under deferred recording, storage is NOT committed mid-frame —
    `commit_close_success` writes the overlay back only on submit
    success — so the barrier declared `old_layout = storage` ≠ the
    GPU's actual layout (e.g. SHADER_READ_ONLY_OPTIMAL after a
    prior B.2 `render_composite` in the same frame). Driver-undefined
    dst contents; α channel discard on RADV (both RDNA2 and GCN4),
    RGB survives via LOAD_OP=LOAD + tile-cache. Fix swaps to
    `RecordedCompositeTarget` + `record_render_composite_open_with_old_layout(rt.dst_old_layout)`,
    matching the B.2 `render_composite` emit path's deferred-recording
    contract verbatim. Regression test
    `v2_frame_builder_render_traps_or_tris_after_prior_dst_paint_uses_recorded_old_layout`
    drives the exact two-op same-dst scenario.

    **Silence (dual-output, RX580 / GCN4 / RADV) — PASS 2026-05-26**
    (capture `yserver-hw-mate.log` + `yserver-mate.submit.tsv` at
    10:13, with the hotfix 3 fix applied). User reports fully
    responsive; appearance dialog renders correctly.
      - `close_reasons[non_ported]/s` = **0** across all samples
        (target ≤ 10). ✓
      - `frame_builder_aborts/s` = **0**. ✓
      - `submit_group_aborts/s` = **0**, `submit_group_size_max_in_window`
        = **1** (M1 enforced). ✓
      - `pin_ceiling=0`, `scratch_grow=0` — no pin pressure / no
        scratch grow churn. ✓
      - No new validation VUIDs, no `ERROR_DEVICE_LOST`, no fault
        chains.
      - **Followup observation (NOT a B.3 regression — workload-side):**
        absolute submit volume on silence is ~3-4× bee's at peak
        — `submit_group_flushes/s` 1408–2088 (vs bee's 180–510),
        `queue_submit2/s` peaking at 25024, `paint_submits/s` peaking
        at 27938. Dominant feeder is **`storage_allocations/s` = 8478**
        (matched by `image_view_creates/s` = 8478) — ~8K fresh
        Vk image+view pairs per second. v2 doesn't self-allocate
        at that rate, so this is client-side pixmap churn (marco's
        compositor pipeline + mate-panel applets are the usual
        suspects). `cpu_fence_wait_ns/s` peaks at 307 ms/s tracked
        1:1 by `cpu_fence_wait_count/s = 409` and
        `close_reasons[sync_wait] = 397` — clients doing
        `get_image` / xshmfence-style readbacks at ~400 Hz.
        Followup item logged: identify the pixmap-churn culprit via
        per-client `CreatePixmap` pivot on the captured log; the
        likely fix lives at the pixmap-pool / recycling layer, not
        Phase B.

    **Silence pixmap-churn followup — RESOLVED 2026-05-26
    (`71e7542` + `faee4b8`).** Two-step diagnostic: (1) added
    `returns_rejected_oversize_by_bin[<=256,<=512,<=1024,>1024]`
    to the 1-s `pixmap pool` telemetry line — purely an observation
    surface, no behaviour change; (2) captured a fresh MATE drag on
    silence, observed **99.3 % of all oversize rejects landing in
    the `<=256` bin at peak burst** (8026 / 8080 per second). Other
    bins were noise (`<=512` peaked at 282/s, `<=1024` at 66/s,
    `>1024` startup-only). Cause: icon-theme thumbnails
    (16/24/32/…/256), Cairo subsurface tiles, and panel-applet
    backings — all in the 128 < max(W, H) ≤ 256 band that the
    old `MAX_POOLED_DIM=128` cap excluded. The cap predated this
    workload; it was sized for adapta-nokto/mate-cc 32×32 widgets.

    Raised `MAX_POOLED_DIM` to 256. Worst-case extra pool footprint
    32–64 MB across the new bucket range, comparable to Mesa's
    userspace BO cache budget. Bucket cap kept at 32 (data showed
    demand was high in the hottest bucket; tightening would push
    rejects into the `bucket_full` path).

    Post-bump big-drag capture results — measured against the
    pre-bump same-workload run:
      - `<=256` oversize rejects: **8026/s peak → 0/s** ✓.
        Bin completely captured.
      - Total oversize rejects: **8080/s peak → 86/s steady**
        (one 340/s startup blip with `<=512=277, >1024=54`).
        ~99 % reduction.
      - `takes_hit/s` (pool): **1573 peak → 4730 peak**. Pool
        roughly tripled in throughput; ~98 % of takes are hits.
      - Real `vkCreateImage + vkAllocateMemory + vkBindImageMemory`
        rate (≡ `takes_miss/s`): **~8000/s → ~100/s** — **~80×
        reduction** in the expensive Vk allocation path.
      - `storage_allocations/s`: 9875 peak → 5195 peak (~50 %).
        Note the metric counts every CreatePixmap regardless of
        pool path — see footnote below.
      - `paint_submits/s`: 27938 peak → 16735 peak (~40 %).
      - `cpu_fence_wait_ns/s`: 307 ms/s peak → 235 ms/s peak
        (~23 %); the remaining lag is `sync_wait`-driven
        client readbacks, orthogonal to allocation.

    **Telemetry footnote — `image_view_creates/s` is a
    CreatePixmap-frequency counter, not a `vkCreateImageView`
    counter.** The pool keys on `(W, H, format)` but the
    sample-side view is depth-specific (a depth-32 BGRA8 image
    can serve a depth-24 request and vice versa per
    `Storage::from_pooled` at `store.rs:201`), so
    `allocate_drawable_storage` builds a fresh sample_view on
    every CreatePixmap — pool hit or miss
    (`platform.rs:1186-1207`). Code comment is explicit: "View
    creation is cheap; pooling the image + memory is where the
    win is." If a future debugging session needs the *real* view
    creation rate, derive it from `storage_allocations - takes_hit`
    or split the counter at the v2 backend's `record_image_view_create`
    call site.

    **Other silence followups (separate from the pool bump):**
      - `legacy_sc` close reason ~150-330/s — kill-switch-off
        scene compose. Expected through B.5 when SubmitGroup
        retires entirely and scene compose folds into the frame
        builder.
      - `cpu_fence_wait_ns/s` 50-235 ms/s tied to
        `close_reasons[sync_wait]` 80-175/s — MATE applet
        `get_image` readbacks. Same shape as bee post-B.3 (an
        orthogonal next target).

    **B.3 closure summary (2026-05-26).** Closing on the strength
    of:
      - bee (RDNA2 / RADV) PASS lag-free — the original B.3 target,
        first time the MATE drag workload smooths out across the
        Phase A→B.3 chain.
      - yoga (Snapdragon X1 / Adreno / Turnip) PASS — frame-builder
        flush rate in band, no aborts, no `non_ported` close
        pressure.
      - silence (RX580 / GCN4 / RADV, dual-output 5120×1440) PASS
        plus pixmap-churn followup resolved via the
        MAX_POOLED_DIM 128 → 256 bump (`faee4b8`).
      - air (Apple M1 / Asahi / Mesa AGX-V, aarch64) — bootstrap
        clean (cherry-picked DRM-resolver fix on master); user
        subjective lag-free under the same MATE drag.
      - M2 Asahi (ALARM, aarch64) — first M2 capture
        (2026-05-26 user-reported); full MATE session runs on
        the same `0c08568` master HEAD as the rest of the
        matrix. Extends AGX-V coverage from M1 to M2 with no
        platform-specific fix needed.
      - nvidia (proprietary) — MATE bootstraps and runs.
      - Three correctness fixes landed during the closure window:
        trap-emit α-loss on depth-32 backings (`b0b57f8`),
        SHAPE Input region not mirrored to backend (`ae480a2`,
        broke adapta-nokto menu hover + click), graceful drop
        of X11 requests from already-disconnected clients
        (`932c34e`, crash on mate-appearance teardown).
      - Diagnostic surfaces added: pixmap-pool oversize-reject
        bin telemetry (`71e7542`), pointer-dispatch trace target
        `yserver::kms::v2::pointer` (`0c08568`).

    **Followups carried out of B.3 (not blockers):**
      - iMac / fuji regression checks — pending fresh capture on
        post-2026-05-26 master. Phase A had each green; B.3 is
        structurally compatible (no driver-class divergence so
        far across RADV variants + ANV + Adreno + AGX-V + NVIDIA).
      - Cross-vendor validation-layer sanity (lavapipe with VUID
        watch) — pending; surface is unchanged since B.2 which
        was vkdebug-clean.
      - silence `cpu_fence_wait_ns/s` 50-235 ms/s from MATE
        applet `get_image` readbacks — orthogonal to frame
        builder, candidate for a separate sync-wait close
        reduction.
      - bee `cpu_fence_wait_ns/s` 60-220 ms/s same shape — same
        target.
      - Pre-trap-α-fix nvidia capture: visual transparency
        artifacts on depth-32 CSD chrome were present at capture
        time; resolved by `b0b57f8`. A fresh nvidia run after the
        fix will likely show clean rendering on the appearance
        dialog and similar GTK CSD windows.

---

## e16 regressions — bg-pixmap lifetime fix (master, 2026-06-04)

**Symptoms (silence HW, e16):** desktop-menu items blanked on
hover/un-hover; bottom-left pager misrendered.

**Root cause:** X11 lifetime rule — installing a pixmap as a window
background does NOT transfer ownership; the client's pixmap must stay
usable until the client frees it. The host-side pixmap may only be
released when BOTH hold: the client resource is gone AND no window
background references it. Two sites freed on the second condition
alone, destroying still-client-owned pixmaps:

- `handle_change_window_attributes` — bg replacement freed the
  previous bg pixmap. e16 swaps item-window backgrounds between a
  kept "normal" pixmap and a transient hilite pixmap on every
  hover/un-hover; the swap host-freed the kept pixmap, so the
  restore pointed at a dead host xid (window fell back to bg_pixel,
  GetImage read zeros). Same churn drives the pager thumbnails.
- DestroyWindow teardown — freed every retained bg host xid in the
  subtree unconditionally (client ownership + outside-subtree bg
  references unchecked).

Fix: `Resources::host_xid_owned_by_pixmap` + orphan-rule guards at
both sites (`handle_free_pixmap` already had the converse check).

**Method:** request-log forensics (hover idiom: CopyArea slice +
tiled fill + CompositeGlyphs → CWA bg swap → ClearArea) → extended
SIGUSR2 drawables dump (per-window `win-` leaves + env-gated
`xid-` full-store sweep, `YSERVER_DUMP_ALL_DRAWABLES=1`) showed
storage-vs-scanout agreement (scene exonerated) and the kept item
pixmaps missing from the store → standalone probe
(`tools/e16-glyph-probe.c`) reproduced 12/12 in vng → guards
eliminated by HW A/B (frame-builder off @ `ea24914~1` still bad;
pool-ineligible dims @ MAX_POOLED_DIM=128 still bad) → fix verified
probe-green in vng + e16 menu/pager HW-verified on silence.

**Still open (characterized, separate fixes):**
- ~~SHAPE depth-1 masks~~ — RESOLVED (`a69c873`, 2026-06-04):
  `KmsBackendV2::read_depth1_pixmap` override (engine.get_image
  depth-1 readback + LSBFirst bit-unpack to the byte-per-pixel
  triple `bitmap_to_yx_banded_rects` consumes). Acceptance tests
  Vk-gated; e16 shaped popups HW-verified on silence. Same squash
  repaired the 9 stale/born-broken v2_acceptance tests (76/76 on
  RADV; 5 broke at `926a01a` when fill ops moved into the frame
  builder, 2 were born failing, 1 test-helper never drained FB
  close events, 1 pinned the pre-`b5c5f16` COW-registration
  design).
- ~~`YSERVER_FRAME_BUILDER=off` kill-switch is bit-rotted~~ —
  RESOLVED by removal (`04d90c4`): both kill-switches + the legacy
  per-op-submit bodies they gated are deleted (-1170 LOC); the
  frame builder is the only composite/glyph path.
- ~~XkbGetNames zero atoms~~ — RESOLVED (`0f9de9c`, 2026-06-04):
  real interned atoms (resolved KcCGST for the compiled RMLVO,
  canonical type/level names) + per-key names via
  `xkb_keymap_key_get_name`. Negative control reproduced the
  libX11 `BadAtom, X_GetAtomName, 0x0` exit; xdotool now works in
  vng (`tools/xkb-names-vng-test.sh`).
- ~~Composite GetOverlayWindow returns 0~~ — NOT A BUG: x11trace
  mis-decodes this reply (Xephyr traces `overlayWin=0` too while
  e16 parents onto the real overlay xid on both servers).
- ~~e16 silent vng exit / XCreateFontSet NULL~~ — RESOLVED
  (`28f42d1`, 2026-06-04): alias catalog entries now go out under a
  real-metrics XLFD (`FontLoader::alias_to_xlfd`) AND every
  ListFontsWithInfo entry carries the `FONT` property (XA_FONT=18 →
  interned XLFD atom, interner threaded like `xkb_proxy`). Key
  libX11 fact (omGeneric.c `get_prop_name`): for non-XLFD base
  names XLC resolves the charset EXCLUSIVELY via the FONT property
  of the first reply — the reply name is not consulted on that
  path. vng-verified (probe conversation matches Xephyr shape; e16
  ALIVE headless; `tools/e16-hover-repro.sh` runs end-to-end) +
  silence HW (xlsfonts/xfd/xterm/wmaker/e16 normal). **Headless
  e16-in-vng repros are unblocked.**
- Connection setup advertises `max-keycode=0` (min=0x08); derive
  from the real keymap.

## GetImage format/plane_mask — XTS hang fix (branch `fix/get-image-format-plane-mask`, 2026-06-04)

**Symptom (eiger HW, full xts5 run):** every full run died at test
400 `Xlib9/XGetImage` scenario 810-0 — TP2 reported "unexpected
signal 11", TP4 then hung ~5 min until the harness SIGTERM
("sigtrap.c" in the journal is TET's handler file, not a SIGTRAP).
Orphaned TCM processes survived forever, deadlocked in their own
SIGTERM handler.

**Root cause (gdb core of the crashing TCM):** v2 `get_image`
ignored both `format` and `plane_mask` — XYPixmap requests got
ZPixmap-layout bytes. For TP2's first call (`plane_mask=0`,
XYPixmap) the non-empty reply walks into a latent libX11 NULL deref:
`_XGetImage` runs `planes = image->depth` *before* its NULL check,
and `XCreateImage(depth=Ones(0)=0)` returns NULL. Xorg never trips
it (it replies length-0 there). The deref fires with the display
mutex held; TET catches the SIGSEGV and longjmps, so every later
Xlib call in that TCM blocks on the poisoned mutex — that's the
5-minute "hang". Upstream libX11 bug worth filing.

**Fix (`8fb4ff2`):** backend truncates `plane_mask` to drawable
depth; XYPixmap + empty mask → length-0 reply (Xorg behavior);
XYPixmap repacks the engine's Z bytes into bitmap planes (MSB plane
first, pad-32, LSBFirst); partial-mask ZPixmap masks pixels in
place. Protocol fallback (unknown drawable) sizes XYPixmap zeros as
`popcount(mask)` planes instead of 0 bytes.

**Verified on eiger HW:** scenario completes in 5 s (was: SIGSEGV +
300 s timeout). 17 TPs: 1 PASS / 10 FAIL / 3 NOTINUSE / 3 UNTESTED,
no signals. Residual FAILs are content-level (depth-4/15/16 pixmap
formats advertised but unreadable by the engine → zeroed fallback
data; no BadValue for invalid format) — tracked as follow-ups, they
don't block full runs.

## Grab-protocol XI2 gate — second XTS hang fix (branch `fix/grab-xi2-protocol-gate`, 2026-06-04)

**Symptom (eiger HW, full xts5 run):** after the GetImage fix the run
advanced to `Xlib11/ButtonPress` scenario 890-0 and wedged the same
way — TP10 "unexpected signal 11", TP11 hung until the harness kill.
gdb-shim backtrace: SIGSEGV inside libXi's XGE wire handler, reached
from `XSync` ← `buttonpress` (devcntl.c:159).

**Root cause:** grab redirection delivered XI2 events to the grab
owner unconditionally (pointer_fanout XI2 redirect pushed the owner;
key-grab delivery always sent core + XI2). A core `XGrabPointer` /
`GrabKey` owner never asked for XI2; the xts5 TCMs link libXi without
calling XIQueryVersion, and libXi NULL-derefs on the unsolicited XGE
event. TET longjmps out of the SIGSEGV with the display mutex held →
the rest of the TCM deadlocks (same poisoned-mutex mechanism as the
GetImage crash).

**Fix (`9177ec3`):** `via_xi2` flag on ActivePointerGrab /
PassiveButtonGrab / KeyGrab / ActiveKeyboardGrab — core grab requests
set false, XIGrabDevice / XIPassiveGrabDevice set true. Redirection
still makes the grab exclusive, but the owner only receives the XI2
form when the grab was established via XI2 (Xorg DeliverGrabbedEvent
consults the grab's own xi2mask, empty for core grabs). GTK popup /
window-move behavior (XI2 grabs) unchanged.

**Verified on eiger HW:** ButtonPress scenario completes in 3 s, no
signals (was: SIGSEGV + ~3 min hang). 1 PASS / 8 FAIL / 1 UNTESTED /
2 UNSUPPORTED.

**~~Still open~~ — WarpPointer fixed (`37d1aa1`, same branch):**
the "events don't reach test windows" follow-up was
`KmsBackendV2::warp_pointer` being a log_v2_gap stub — the pointer
stayed at the screen-center start position (1280,800 on the
2560×1600 panel) and every synthesized press missed its test window.
New `Backend::warp_pointer_root(state, x, y)` (default no-op): the
handler resolves the destination to root coords; KMS routes through
`on_host_input(PointerMotion)` so the warp updates the tracked
cursor and generates the spec'd motion/crossing events; ynest keeps
the host forward. Relative warps (dst=None) now work on KMS too.
ButtonPress on eiger HW: 7 PASS / 2 FAIL (was 1/8). Remaining FAILs
are finer grab semantics, filed under known-issues territory:
- TP2: passive-grab activation must search ancestors ROOT-DOWN
  (oldest ancestor's grab wins) + synthesize
  EnterNotify(mode=NotifyGrab) crossings on activation.
- TP7: an over-delivery during an active grab ("Unexpected event
  (ButtonPress) received").

## First full XTS run on HW — baseline (master, 2026-06-04)

`just xts-yserver-hw all 21600` on eiger completed for the first time:
**1078 test cases / 5987 test purposes in 53 min, zero crashes, zero
hangs.** Result: **2784 PASS (46.5%)** / 1871 FAIL / 730 UNTESTED /
273 NOTINUSE / 168 UNRES / 139 UNSUP / 15 ABORT. Journal + summary:
`xts/results/2026-06-04-15:48:44/`.

Per-section: Xproto 358/389 PASS; Xt sections strong (Xt3 73/73,
Xt14 18/18, Xt11 245/285). Largest FAIL buckets: Xlib9 974 (drawing /
GetImage content — depth-4/15/16 pixmap formats advertised but
unreadable, plane semantics), XI 200, Xlib4 176, Xlib13 140 (WM /
visibility), Xlib11 73 (residual grab semantics).

**New bug found by the run's tail (15 XIproto ABORTs):** client
resource-ID bases are allocated monotonically (`client# × 0x100000`)
and never recycled — the 32-bit base overflows at client 4096 and
connection setup fails ("Could not open display"). This run alone
consumed 4102 client slots. Strictly, bases past client ~512 already
exceed the X11 29-bit resource-ID space. Fix: recycle bases of
disconnected clients (Xorg model).

Unblocked by this session's chain: stale-tetexec.cfg aborts →
xts-run.sh regeneration; Xlib9/XGetImage libX11 NULL-deref →
GetImage format/plane_mask; Xlib11/ButtonPress libXi NULL-deref →
grab-protocol XI2 gate; mass event-delivery FAILs → WarpPointer on
KMS. The earlier "20-min timeout" mystery at Xlib12/XMaskEvent was
the recipe's default `timeout=1200` budget for the WHOLE run, not a
yserver bug.

## Xlib9 bulk-drawing — second-fill drops between scene composes (resolved 2026-06-05, see follow-up section below)

**Symptom (vng, all ICDs).** `just xts-yserver Xlib9` on silence
reproduces eiger's 970/1472 FAIL (180 PASS — within noise of the
2026-06-04 baseline). Per-TCM the fails concentrate in stroke/fill
ops: XDrawLines 73, XDrawArcs 68, XDrawSegments 65, XDrawLine 65,
XDrawArc 61, XDrawRectangles 56, … — every drawing TCM fails most
of its assertions. Even **XFillRectangle TP1** (the simplest "draw
rect, GetImage, compare" — no GC iteration, no plane-mask) FAILs
with an all-zero `bad` image (`Err000?.err`: `2328,0` = 9000 pixels
of 0 across depth-1/8/24/32). The drawing didn't survive to the
readback.

**Cluster analysis (per-FAIL REPORT line):** 273 gx-raster-op
("GX* fail expected V, got V" — every drawing TCM × ~15 GX ops),
114 missing-error ("Got Success, Expecting BadDrawable/BadGC/
BadMatch/BadValue"), 536 "other" (pixel-mismatch from line/arc
geometry). Cluster tool: `awk '$1=="220"&&$3=="FAIL"' …/journal`
+ classify by REPORT regex (full pipeline in this session's log).

**What I traced in vng** (`yserver-fb*.log` + per-rect `EMIT
FillRect` log added during diagnosis, removed before commit):

For XFillRectangle TP1, depth-24 window xid 0x400004, DrawableId(4):

1. `allocate_window_storage` enqueues fill #1 (init, color
   `[0,0,0,1]`, rect `(0,0 100×90)`) into frame `gen=3`.
2. `maybe_composite` fires → closes frame `gen=3`
   (`reason=LegacyScCompose`) and submits its CB.
3. Scene compose's `record_compose_v2` samples DrawableId(4)
   (SHADER_READ_ONLY_OPTIMAL) into the scanout BO.
4. Client's XFillRectangle → backend.poly_fill_rectangle →
   fill_solid_rects → engine.fill_rect_batch records fill #2
   (color `[0,0,1/255,1]`, rect `(20,30 70×30)`) into a new
   frame `gen=4`. `EMIT FillRect` log confirms **same image
   handle, same image_view, old_layout=SHADER_READ_ONLY_OPTIMAL,
   correct color, correct rect, correct extent (100×90)**.
5. `maybe_composite` fires AGAIN → closes frame `gen=4`
   (`reason=LegacyScCompose`) and submits its CB.
6. Scene compose samples DrawableId(4) again.
7. GetImage → engine.get_image → readback CB → `ticket.wait()`.
8. Staging buffer reads as **`00 00 00 ff` everywhere** —
   fill #2's pixels nowhere in storage; only fill #1's effect
   survived.

So at the yserver API surface, fill #2 is correctly recorded and
its CB is submitted before the readback. The pixels just don't
land. Confirmed both with Venus (`venus=true` virtio-vga-gl) and
with `VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json`
forcing lavapipe → identical FAIL set, so it's **not** a Venus /
virgl_renderer / amdgpu issue.

**Distillation attempt** (`v2_two_fills_then_get_image_returns_
second_fill` in `tests/v2_acceptance.rs`): backend-level repro —
`KmsBackendV2::for_tests_with_vk()` + `create_subwindow` (depth-24,
100×90, bg_pixel=0) + `map_subwindow` + `poly_fill_rectangle` at
(20,30 70×30) with fg=1 + `get_image_pixels_for_tests`. The init
fill from `allocate_window_storage` plus the XCALL PFR gives the
same 2-fill shape as XTS. **Test PASSES on host AND inside vng
(Venus).** So the bug is **not reachable** from a backend-level
two-fills-then-readback sequence — the difference is that scene
compose actually fires between fills in the live XTS run (in tests
`kms_outputs_active=false` and `maybe_composite` short-circuits).

**Working hypothesis.** Something at the
**scene-compose ↔ next-paint-frame boundary** drops fill #2's
writes. Candidates:
- A barrier / access-mask hazard: compose's sample completes
  later than expected, and fill #2's pre-barrier
  (`src_stage=ALL_COMMANDS`, `src_access=SHADER_SAMPLED_READ |
  TRANSFER_WRITE | COLOR_ATTACHMENT_WRITE`) doesn't drain it
  before the layout transition. The barrier masks LOOK right but
  may be wrong against compose's actual queue submit.
- A submit-ordering bug: `close_open_frame`'s
  `flush_submit_group(FrameBuilder)` parks fill #2's CB into
  `pending_group_ops` and then submits — but maybe the SceneCompose
  flush that fires next clobbers or reorders something.
- Storage aliasing: the `dst_image_view` recorded into fill #2's
  `RecordedFillRect` becomes stale by submit time (recycled by
  the pixmap pool, etc.).
- A subtle render-pass issue with cmd_clear_attachments on a
  partial rect when LOAD_OP=LOAD pulls in contents that compose
  already sampled.

**What the next session needs to do.** Either:
- Build a unit-test repro that DRIVES scene compose between the
  two fills (needs a way to force `kms_outputs_active=true` or
  call `scene.tick`/`record_compose_v2` directly from the test);
  or
- Add a structured submit-event log (every `vkQueueSubmit2` with
  CB handle, attached barriers, image handles touched) and rerun
  XFillRectangle TCM in vng, then diff against a known-good Xorg
  sequence under `xtrace`.

Paused this session because each cycle costs ~30 s vng boot +
TCM, and the next step is sync-layer work that wants careful
diff-by-diff iteration. The investigation tool added is
permanent: the failing-second-fill negative-result test plus the
cluster table above. Unbreaking this likely unlocks ~800 of the
Xlib9 970 FAILs in one shot (273 gx-raster-op + ~500 of "other"
which are also two-fill verifications).

Pivoting to the smaller `missing-error` cluster next (114 fails,
deterministic, no Vulkan involvement).

## Xlib9 `XFillRectangle` — resolved in KMS worktree (2026-06-05)

The earlier "second fill drops after scene compose" hypothesis turned
out to be incomplete. The reduced compose harness stayed green; the
live XTS failures instead broke down into a stack of independent
protocol and storage bugs in the KMS/v2 path. The narrow diag is now
green:

- `xts5/Xlib9/XFillRectangle/XFillRectangle`: `42 PASS / 1 NOTINUSE / 1 UNSUPPORTED`
- final live run: `/home/jos/Projects/xts/results/2026-06-05-11:24:32`

Main fixes that moved the case:

- v2 color-attachment `LOAD` passes now open with
  `COLOR_ATTACHMENT_READ | COLOR_ATTACHMENT_WRITE`, fixing stale
  render-pass semantics after prior sampling.
- depth 4 now uses the same `R8_UNORM` storage / row-pack path as
  depth 8 instead of falling through `UnsupportedDepth(4)`.
- solid fills now honor plane mask and sub-byte depth (`1`/`4`)
  semantics via a CPU read/modify/write fallback for the NOT-family
  logic ops and masked writes.
- request-path error validation for `PolyFillRectangle` now returns
  the expected `BadDrawable`, `BadGC`, and `BadMatch` cases.
- patterned fills (`Tiled`, `Stippled`, `OpaqueStippled`) now go
  through a CPU fallback that honors GC function, plane mask,
  TS-origin, stipple background semantics, and `ClipByChildren`.
- GC-retained clip/tile/stipple pixmaps now survive `FreePixmap` on
  the host side, matching X11 lifetime semantics.
- `PutImage` bitmap ingress fixes:
  - `XYBitmap` uploads to depth-N drawables now expand bits with the
    GC foreground/background and upload at the target depth.
  - depth-1 `XYPixmap` uploads are normalized into the existing depth-1
    ZPixmap path.
- KMS startup now installs backend root bindings into `ServerState`,
  fixing request-path root draws such as the `IncludeInferiors` case.

The remaining `UNSUPPORTED` TP is the existing non-actionable XTS
bucket rather than a live KMS regression in `XFillRectangle`.

## XC-MISC 1.1 (branch `feat/xcmisc`, 2026-06-12)

**Status: IMPLEMENTED and probe-verified.** XC-MISC (major opcode 152,
minor version 1.1) is fully wired: `GetVersion` (opcode 0), `GetXIDRange`
(opcode 1), and `GetXIDList` (opcode 2). This fixes the silent WM/client
death caused by XID-range exhaustion — libxcb calls `xcb_generate_id`
which, with no XC-MISC extension present, wraps at 2^20 (1 048 576) ids
and returns `0xffffffff` for every subsequent allocation. Clients hit
`BadIDChoice` on those ids; GDK-based clients (muffin, marco, most GTK
apps) treat the first `BadIDChoice` as fatal and exit, silently killing
the window manager and eventually the entire session after roughly two
hours of desktop use. With XC-MISC, the server hands the client a fresh
XID range on demand via `GetXIDRange` and individual ids via
`GetXIDList`; libxcb's exhaustion path calls these automatically and
recycling proceeds transparently.

Verified by `tools/xid-exhaust-probe`: **RED pre-fix** — exhausted at
exactly 1 048 576 ids with no recycling; **GREEN post-fix** — 1 310 718
ids allocated across the range wrap under ynest, server log confirms
`GetXIDRange` firing at the wrap boundary. Wire encoder unit tests are
in `yserver-protocol`; core wiring is in `yserver-core`. `cargo test
--locked` workspace-green; plain clippy + nightly fmt clean.
