# Present Vblank Arming and Frozen CRTC Clocks Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bound outstanding Present vblank arms per CRTC, and complete Presents on a CRTC whose kernel clock froze.

**Architecture:** One shared tagged next-vblank slot per CRTC replaces per-target absolute arming. An arm that does not fire in one second marks the CRTC frozen, and the existing software vblank fallback then drives it with a clock derived from elapsed time.

**Tech Stack:** Rust, DRM `CRTC_QUEUE_SEQUENCE`, existing `PageFlipEventSource` test stubs.

**Spec:** `docs/superpowers/specs/2026-10-30-present-event-space-design.md`

## Global Constraints

- Detection window: one second, a `const`, not an option.
- Outstanding Present arms per CRTC: at most two (idle slot + tagged slot).
- No `YSERVER_*` environment variables; no `cfg(test)` in production bodies.
- Reuse first: new code only where existing code cannot reach the goal.
- Tests: A end-state check, B driven through the core loop or the backend's production entry points, F run under CPU load (`nproc` busy loops).
- Implementers do no git writes, no hardware, and run no `yserver` binary. The coordinator commits signed, without a session URL.
- Copyleft sources (kernel, NVIDIA) are behaviour only.

## Review Focus

1. A compositor (picom, muffin) waiting on NotifyMSC with a healthy clock: pacing unchanged. Pinned in Task 1, `notify_msc_pacing_unchanged_with_shared_wake`.
2. A second client on a frozen CRTC with `target_msc=0` and a non-zero divisor: completes at a rising MSC. Pinned in Task 2, `divisor_present_completes_on_frozen_crtc`.
3. Direct scanout on a frozen CRTC: buffers retire, and retirement samples are coherent. Pinned in Task 2, `direct_scanout_retires_on_frozen_crtc`.
4. A frozen CRTC whose flip is always in flight: the clock still ticks. Pinned in Task 2, `frozen_tick_runs_with_a_flip_in_flight`.
5. A window moved from a frozen CRTC to a healthy one: older Presents still complete in order. Pinned in Task 2, `window_moved_off_frozen_crtc_completes_in_order`.

---

### Task 1: One shared Present wake per CRTC

**Files:**
- Modify: `crates/yserver-core/src/backend/trait_def.rs` (absolute arm contract, ~1275-1290)
- Modify: `crates/yserver-core/src/backend/recording.rs` (1148, fields 536-547)
- Modify: `crates/yserver-core/src/core_loop/run.rs` (`arm_present_idle_vblanks`, 3214-3354)
- Modify: `crates/yserver-core/src/core_loop/process_request/present_ext.rs` (`fire_present_completions_sweep`, 3095-3106)
- Modify: `crates/yserver/src/kms/render/backend/kms/present.rs` (606-640, 679-690, 802-846, 1895-1918), `kms/state.rs` (1414-1427), `kms/crtc_config.rs` (1547), `kms/session.rs` (235), `seam.rs` (338), `trait_impl.rs` (289), `kms/platform_hooks.rs` (576), `surface/platform.rs` if it implements the arm, `for_tests.rs` (679)
- Test: `crates/yserver-core/src/core_loop/process_request/tests/present_pixmap.rs`, `crates/yserver/src/kms/render/backend/tests/present_scanout.rs`, `tests/vblank_fallback.rs`

**Interfaces:**
- Produces, replacing `arm_present_absolute_vblank(crtc_id, targets) -> io::Result<usize>`:
  `Backend::arm_present_shared_wake(&mut self, crtc_id: u32) -> io::Result<bool>`. `true` means a tagged next-vblank arm is outstanding on that CRTC, newly issued or already there. Default `Ok(false)`.
- Produces in `present_ext.rs`: `pub(crate) fn present_completion_due_clock(state: &ServerState, event: &CompletedPresentEvent) -> PresentClockSample`, the selection now inline in the sweep. Its type names follow what the sweep uses.
- Produces in `KmsState`: `shared_wake_armed: HashSet<CrtcKey>` replacing `absolute_vblank_targets`.
- Produces in `RecordingBackend`: `armed_shared_wake_crtcs: Vec<u32>`, `arm_present_shared_wake_result: Option<Result<bool, io::ErrorKind>>`, replacing the absolute fields.
- Keeps `present_absolute_vblank_arm_supported`, and the `ABSOLUTE_SEQ_TAG` user data, unchanged in name.

**Invariants:** a tagged event on a CRTC clears that CRTC's slot, before any validity check, as the current code does for its set. Every site that clears today's set clears the slot. The arm is `relative = true, target = 1`, with `absolute_seq_user_data`; the legacy path uses the matching relative signal. The tagged event stays completion-eligible.

- [ ] **Step 1: Write the failing tests**

In `present_pixmap.rs`, through the production request and `arm_present_idle_vblanks`:

```rust
#[test] fn shared_wake_held_presents_past_target_arm_nothing()
// 40 Presents behind an unfinished older one on the same window, targets reached.
// assert: armed_shared_wake_crtcs is empty; on unblock all fire in present_id order.

#[test] fn shared_wake_future_targets_make_one_call_per_domain_per_iteration()
// >128 distinct future targets, two windows, one CRTC.
// assert: one call per iteration for that CRTC; every completion fires at its target, in
// per-window order, including targets across u64 MSC wrap, with the clock advanced by hand.

#[test] fn shared_wake_no_usable_clock_counts_as_ahead()
// due clock msc == 0: assert one call.

#[test] fn shared_wake_failure_executes_parked_entries_immediately()
// result Some(Ok(false)) and Some(Err(..)): entries more than one vblank out run in the same pass;
// a next-MSC entry parked behind a flip does not.

#[test] fn shared_wake_next_msc_entry_behind_a_flip_is_armed()
// only a next-MSC exec entry parked with present_flip_in_flight: assert one call.

#[test] fn notify_msc_pacing_unchanged_with_shared_wake()
// a parked NotifyMSC still goes through arm_idle_vblanks and fires at its target.
```

In `present_scanout.rs`, with the recording event source:

```rust
#[test] fn shared_wake_outstanding_arms_stay_bounded()
// Descending targets arrive one per iteration, and a window is destroyed with targets parked.
// With CRTC_QUEUE_SEQUENCE latched unsupported on the device, arms take the legacy route, which the
// recording source sees (fixtures have no ioctl). Issued minus delivered per CRTC never exceeds 2,
// and a page flip submitted meanwhile succeeds. Watch tests in Task 2 establish arms the same way.

#[test] fn shared_wake_rearms_only_after_its_event()
// second call issues no ioctl and returns true; delivering the tagged event clears the slot;
// next call issues one ioctl.

#[test] fn shared_wake_cleared_by_lifecycle_edges()
// DPMS off, VT away, per-CRTC modeset, output removal: slot empty, next arm issues an ioctl.
```

Mutations that must fail these: restore per-target arming; treat reached targets as ahead; report `true` on failure; never clear the slot on the event.

- [ ] **Step 2: Run them and confirm they fail**

Run: `cargo test -p yserver-core --lib shared_wake && cargo test -p yserver --lib shared_wake`
Expected: compile failure on the missing method.

- [ ] **Step 3: Implement**

Move the sweep's selection into `present_completion_due_clock` and call it from the sweep. In `arm_present_idle_vblanks`, the completion block keeps only entries whose target is after that clock (or whose clock has `msc == 0`); with none left it arms nothing. Supported CRTC: `arm_present_shared_wake`. Unsupported: the existing `arm_present_completion_idle_vblanks`. The exec block arms the shared wake for every `source_ready` entry whose target is after the clock, which now includes a next-MSC entry parked behind an in-flight flip, so its CRTC stays observed. `Ok(true)` covers all of them. On anything else, entries more than one vblank out (today's set) keep the existing immediate execution; next-MSC entries keep waiting for the flip as they do today. Delete the per-target set and update the `state.rs` and trait docs.

- [ ] **Step 4: Migrate existing tests**

Every caller and fixture moves to the new contract before the gate: `present_pixmap.rs` (including 2247 and 4205, find all with `grep armed_absolute_vblank\|arm_present_absolute_vblank_result`), `present_scanout.rs` (65-76, 135, 439, 591-640, 728, 770, 1890, 1923, 2170), `vblank_fallback.rs` (160-189). They move to the slot, keeping what each protects. Delete only those whose subject no longer exists, and list them in the report with the reason.

- [ ] **Step 5: Run the gate** (see Gate). Expected: 0 failures.

- [ ] **Step 6: Commit** (coordinator): `fix(present): one shared vblank wake per CRTC`

---

### Task 2: Software clock for a frozen CRTC

**Files:**
- Modify: `crates/yserver/src/kms/render/platform/vblank_fallback.rs`
- Modify: `crates/yserver/src/kms/render/platform/flip_events.rs` (flip path 793-830, `prune_present_clocks_to_live_outputs` via `retain_live`)
- Modify: `crates/yserver/src/kms/render/backend/kms/present.rs` (`on_crtc_sequence_event`, `kernel_vblank_arm_available` callers, `service_software_vblanks`, `KernelVblankArmer` success paths, `clear_all_armed_vblank_targets`, `prune_armed_targets_to_live_outputs`, `backend_present_present_absolute_vblank_arm_supported`)
- Modify: `kms/crtc_config.rs` (1546, re-anchor and end observation), `kms/platform_hooks.rs` (`next_wakeup` 482, `poll_deferred_input` 695)
- Test: `crates/yserver/src/kms/render/backend/tests/vblank_fallback.rs`

**Interfaces:**
- Consumes: the slot from Task 1.
- Produces in `VblankFallback`:
  `kernel_arm_watch: HashMap<(CrtcKey, VblankArmSlot), Instant>` (when each slot's unfired kernel arm was issued), with `pub(crate) enum VblankArmSlot { Idle, SharedWake }`, and
  `frozen: HashMap<CrtcKey, FrozenClock>`.
- Produces: `pub(crate) struct FrozenClock { anchor_msc: u64, anchor_ust: u64, anchor_at: Instant, interval: Duration }`
- Produces on `PlatformBackend`: `frozen_clock_now(&self, key: CrtcKey, now: Instant) -> Option<(u64, u64)>` returning `(msc, ust)`; `next_frozen_watch_deadline(&self) -> Option<Instant>`.
- Produces on `KmsBackend`: `const FROZEN_VBLANK_WINDOW: Duration = Duration::from_secs(1)`; `mark_frozen_crtcs(&mut self)`.

**Invariants:**
- A slot's watch starts on its successful kernel arm. Only that slot's own event removes it: tagged events the `SharedWake` watch, untagged events the `Idle` one. Every path that clears a slot also removes its watch.
- `mark_frozen_crtcs` runs from `poll_deferred_input`, reached through `next_wakeup`, the poll timeout and `run_iteration_tail` (`core_loop/run.rs:3140`), so silence reaches it. It reads no events: if the device's fd is readable (`PageFlipEventSource::events_ready(&self, device: &Device) -> io::Result<bool>`, a zero-timeout poll, added to the trait and its stubs), it defers the verdict to the next iteration, whose normal `on_page_flip_ready` handles them. It does nothing while `!scanout_allowed() || !kms_outputs_active`.
- `frozen_clock_now`: `frames = (now - anchor_at) / interval`; `msc = anchor_msc + frames`; `ust = anchor_ust + frames * interval`. Marking anchors at the larger of the general and completion clocks' last MSC, so nothing goes back.
- `present_absolute_vblank_arm_supported` stays true on a frozen CRTC. There, `arm_present_shared_wake` and `arm_idle_vblanks_with` arm a software vblank instead of the ioctl and return true, so exec and completion work keep a wake during flips and composes, past the idle gate of `arm_present_completion_idle_vblanks`. Deadlines land on the next frame boundary of the frozen clock.
- In `service_software_vblanks`, a frozen CRTC ignores `armed_msc` and flip-in-flight suppression, and records `frozen_clock_now` into both the general and the completion clock, so `present_get_completion_clock` advances. Non-frozen CRTCs keep today's path byte for byte.
- In the flip path, a frozen CRTC discards `flip.frame` and `flip.ust` for clock purposes, and the sample uses `frozen_clock_now`. Buffer retirement and the returned completion are otherwise unchanged. `kernel_ust` in `FlipEventTimestamps` stays the kernel's value only if nothing computes Present timing from it; check its consumers and use the frozen UST otherwise.
- Every read of a frozen CRTC's clock returns the elapsed value, with no flip or tick before it: `present_get_ust_msc` and `present_get_completion_clock` consult `frozen_clock_now` (using the injected clock) for a frozen key. They are read at request time, so a `NotifyMSC(0, 0, 0)` and a new Present's target after inactivity start from the current frame.
- `on_crtc_sequence_event` on a frozen CRTC clears the slot and watch and returns before touching any clock.
- A same-route modeset (`crtc_config.rs:1546`) re-anchors with the new interval at the current value and keeps the mark. `retain_live` drops the mark and watch.
- Marking logs once: `log::warn!("kms: CRTC {crtc:?} on {device} vblank counter frozen; Present uses a software clock")`.

- [ ] **Step 1: Write the failing tests**

In `vblank_fallback.rs`, through `b.poll_deferred_input`, `next_wakeup` and event delivery on the recording source, using `two_output_fixture`:

```rust
#[test] fn frozen_crtc_no_events_completes_after_window()
// Real dispatch through the public `run_core`, as tests/tearing/round2.rs:1655 drives it: PresentPixmap
// with target_msc=1 and then two more. Arm never fires, no flips.
// next_wakeup() <= armed_at + 1 s. At +1 s: marked. The client receives CompleteNotify for all three, in
// order, within one frame after. Mutation: drop the deadline from next_wakeup.

#[test] fn frozen_crtc_flips_do_not_drive_the_clock()
// 600 stale flips delivered over 1 s of a 60 Hz mode: msc rises by 60 (+/-1); ust == anchor + frames*interval;
// buffers retired. Mutations: record flip.frame; advance per flip.

#[test] fn frozen_clock_catches_up_after_a_stall()
// no service for 10 frames: next sample jumps 10.

#[test] fn frozen_tick_runs_with_a_flip_in_flight()
// no manual tick, through `run_core`: a flip stays in flight while dispatch parks a Present; the loop arms
// and services the software wake and the Present completes. Mutation: keep the idle gate; keep the suppression.

#[test] fn frozen_clock_reads_are_current_after_inactivity()
// marked, nothing pending, no flips, 2 s pass: NotifyMSC(0,0,0) completes with msc = anchor + 2 s of frames, and
// a new Present's target is computed from it. Mutation: getters return the stored sample.

#[test] fn frozen_kernel_sequence_does_not_touch_the_clock()

#[test] fn other_slots_event_keeps_the_surviving_watch()
// both slots armed; only the idle event arrives and NotifyMSC completes. next_wakeup() still reports the shared
// wake's deadline, and at +1 s the CRTC is marked. Mutation: remove all watches of the CRTC on any event.

#[test] fn slow_but_firing_arm_is_not_frozen()
// the event arrives at +0.9 s, and a new arm fires at +1.8 s: never marked.

#[test] fn event_unread_at_deadline_is_not_frozen()
// event queued on the source, not drained, at +1 s: not marked, and draining clears the watch.
// Mutation: ignore events_ready.

#[test] fn no_arm_means_no_verdict()
// a CRTC with only reached, held Presents, for 5 s: not marked.

#[test] fn dpms_off_vt_away_and_own_modeset_end_the_observation()
// one case each, then +1 s: not marked. Mutation: drop each clear.

#[test] fn device_on_fallback_has_no_watch()

#[test] fn same_route_modeset_keeps_the_mark_and_never_rewinds()
// interval change from 60 to 240 Hz: msc continues rising at the new rate. Mutation: clear the mark; drop the re-anchor.

#[test] fn disable_and_enable_returns_to_the_kernel_clock()

#[test] fn freezing_one_crtc_leaves_the_other_on_kernel_clock()

#[test] fn divisor_present_completes_on_frozen_crtc()

#[test] fn direct_scanout_retires_on_frozen_crtc()
// reuse the fixtures of tests/present_scanout_a3.rs or tests/tearing; the owner's retirement sees the frozen clock's sample.

#[test] fn window_moved_off_frozen_crtc_completes_in_order()
```

- [ ] **Step 2: Run them and confirm they fail**

Run: `cargo test -p yserver --lib vblank_fallback`
Expected: compile failure on the missing types.

- [ ] **Step 3: Implement** to the invariants above.

- [ ] **Step 4: Run the gate** (see Gate). Expected: 0 failures.

- [ ] **Step 5: Commit** (coordinator): `fix(kms): software Present clock for a CRTC whose vblank froze`

---

### Task 3: Hardware acceptance

The coordinator runs this, with the user's permission and watching, after preflight: GPU clients checked, both connectors connected. The user gets an observation guide first.

- [ ] **Step 1: NVIDIA, the original failure.** Install the branch binary with `present_pace=debug`. In the session, the i3 config's two `xrandr` lines run at startup. Open Firefox and Alacritty on the two monitors, run `fastfetch`, type, and repeat for 5 minutes. Expected: no `os error 12` in the log, one `vblank counter frozen` line for DP-3, cursor smooth on both monitors, Alacritty responsive.
- [ ] **Step 2: NVIDIA, a healthy head.** Start without the `xrandr` lines. Expected: no `vblank counter frozen` line, `msc` advancing on both outputs.
- [ ] **Step 3: NVIDIA, a game or mpv on DP-3 while frozen.** Expected: frame pacing near the mode's rate, no hang.
- [ ] **Step 3b: NVIDIA client frame rate while frozen.** A native NVIDIA client (`glxgears` or vkcube) and CS2 on the frozen head. Expected: near the mode's rate. XLibre's fake clock gave 1 fps on NVIDIA, so measure instead of assuming.
- [ ] **Step 4: AMD regression.** `--vrr` on, mpv, and a compositor session. Expected: no `vblank counter frozen`, no `os error 12`, tearing and direct scanout as on master.
- [ ] **Step 5: Record results** in the PR body. A visual glitch in any step is a finding.

---

## Gate

Format first, then in this order, with `nproc` busy loops running for the tests:

```bash
source ~/Projects/claude-scratch/atomic-cursor-split/sdd-p1/lvp-env.sh   # lavapipe ICD, pins syncobj tests to amdgpu
cargo +nightly fmt --all
cargo build --bin yserver
cargo test --workspace --locked --features xdmcp
cargo test --workspace --locked --features xdmcp --no-fail-fast -- --ignored
cargo test -p yserver --test render_acceptance -- --ignored
cargo clippy --workspace --all-targets --locked --features xdmcp -- -D warnings
tools/check-layout.sh
PREFIX=<scratch>/man-prefix just man
source ~/Projects/claude-scratch/freebsd-sysroot/env.sh && cargo clippy --target x86_64-unknown-freebsd --all-targets --locked --features xdmcp -- -D warnings
```

Expected: 0 failed, in every command. Tests that wait on GPU work get 20 loaded runs.
