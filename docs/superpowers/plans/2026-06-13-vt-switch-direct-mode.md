# Direct-mode VT switching Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a lightdm-launched (direct/no-libseat) yserver `Ctrl-Alt-F<n>` away to a text console or another graphical server and back, with correct DRM-master handoff.

**Architecture:** A delta on the existing libseat VT-switch path. Arm `VT_PROCESS` on the controlling VT (direct mode only); on the kernel's release/acquire signals, drive the *existing* `drive_seat_event`/`run_suspend`/`run_resume` machinery, adding the two pieces logind does for the libseat path: explicit `drmDropMaster`/`drmSetMaster`, and pausing/resuming the separate direct-mode input thread.

**Tech Stack:** Rust, Linux KMS/DRM (`drm-rs`), VT ioctls (`VT_SETMODE`/`VT_RELDISP`), signalfd, mio/epoll core loop.

**Spec:** `docs/superpowers/specs/2026-06-13-vt-switch-direct-mode-design.md`

**Branch:** `feat/vt-switch-direct`

---

### Task 1: VT_PROCESS arming + teardown in ConsoleGuard

**Files:**
- Modify: `crates/yserver/src/kms/console.rs` (`ConsoleGuard`)

- [ ] **Step 1: Add VT-mode constants + an `arm_vt_process` / `disarm_vt_process` pair.** Constants from `linux/vt.h`: `VT_SETMODE=0x5602`, `VT_PROCESS=1`, `VT_AUTO=0`, `VT_ACKACQ=2`, `VT_RELDISP=0x5605`. `struct vt_mode { mode: c_char, waitv: c_char, relsig: c_short, acqsig: c_short, frsig: c_short }`. `arm_vt_process(relsig, acqsig)` issues `ioctl(fd, VT_SETMODE, &vt_mode{ mode: VT_PROCESS, relsig, acqsig, .. })`; `disarm_vt_process()` sets `mode: VT_AUTO`. Both are methods on `ConsoleGuard` operating on its existing `/dev/tty` fd. Add a `vt_reldisp(arg: i64)` helper too (`ioctl(fd, VT_RELDISP, arg)`).

- [ ] **Step 2: Restore VT_AUTO on Drop.** In `ConsoleGuard::drop`, call `disarm_vt_process()` (best-effort, log on error) before the existing keyboard/screen-mode restore, so the kernel resumes automatic switching on exit/panic.

- [ ] **Step 3: Unit test the vt_mode struct layout** (size + field offsets match the kernel ABI; the ioctl itself isn't callable in a unit test, so assert `size_of::<vt_mode>()` and field packing against the known layout).

- [ ] **Step 4: Build + commit.** `cargo build --locked`; `feat(vt): VT_PROCESS arm/disarm + VT_RELDISP helpers on ConsoleGuard`.

> Note: arming is *invoked* in Task 4 (gated on direct mode); this task only adds the mechanism.

---

### Task 2: Route VT signals to a backend handler (direct mode only)

**Files:**
- Modify: `crates/yserver/src/lib.rs` (signalfd thread, ~lines 311–343, 529–534)
- Modify: `crates/yserver/src/kms/v2/backend.rs` (add `on_vt_release` / `on_vt_acquire` entry points)

- [ ] **Step 1:** Add `pub fn on_vt_release(&mut self, state: &mut ServerState)` and `on_vt_acquire(...)` stubs on `KmsBackendV2` that (for now) just `log::info!`. These are the entry points the signalfd path calls.

- [ ] **Step 2:** In the signalfd handler, when `SIGUSR1`/`SIGUSR2` arrive: if the backend reports VT_PROCESS is armed (a `bool` the backend sets when it arms in Task 4 — expose `vt_switching_armed()`), route `SIGUSR1 → on_vt_release`, `SIGUSR2 → on_vt_acquire`. Otherwise keep the existing behaviour (`SIGUSR1` → scanout dump, `SIGUSR2` → drawable dump). Keep `SIGUSR1`/`SIGUSR2` in the signalfd mask (already there).

- [ ] **Step 3:** Confirm the outbound DM-readiness `SIGUSR1` (sent to parent at startup, `launch.rs:477`) is unaffected — it fires before VT mode is armed and is a `kill(parent, …)`, not an inbound handler.

- [ ] **Step 4: Build + commit.** `feat(vt): route SIGUSR1/USR2 to VT release/acquire when armed (direct mode)`.

---

### Task 3: Release path (switch away)

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs` (`on_vt_release`)

- [ ] **Step 1: Write the failing test.** `on_vt_release` in direct mode must: (a) send the input-thread Pause control message, (b) drive `drive_seat_event(Disable)`, (c) call `release_master_lock()`, (d) `vt_reldisp(1)` — in that order. Test with a fake/recording seat + a flag-capturing master/reldisp shim; assert the call order. (Reuse the seat-state test fixtures from the 2026-05-27 work.)

- [ ] **Step 2: Run, verify fail** (handler is a stub from Task 2).

- [ ] **Step 3: Implement** `on_vt_release`:
  1. `self.pause_input_thread()` (Task 5).
  2. `self.drive_seat_event(state, SeatEventKind::Disable)` — reuses `run_suspend` (stop scanout, drain flips, reset BOs, synthesize held releases).
  3. `self.platform.drm_release_master()` → `release_master_lock()` on the DRM device.
  4. `self.console_guard.vt_reldisp(1)` to ack the release.
  Each step error-tolerant (log, continue) so the `vt_reldisp` ack always runs — a missing ack wedges the kernel.

- [ ] **Step 4: Run, verify pass.**

- [ ] **Step 5: Commit.** `feat(vt): direct-mode VT release — suspend, drop master, ack`.

---

### Task 4: Acquire path (switch back) + arm on startup

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs` (`on_vt_acquire`, plus arm VT_PROCESS during direct-mode init)

- [ ] **Step 1: Write the failing test.** `on_vt_acquire` must: (a) `vt_reldisp(VT_ACKACQ)` first, (b) `set_master` with bounded inline retry on `EBUSY` (assert it retries up to N then proceeds best-effort), (c) `drive_seat_event(Enable)`, (d) resume input thread — in that order. Mock `set_master` to return `EBUSY` k times then `Ok`; assert exactly that order and that resume runs only after master is held (or after the retry budget).

- [ ] **Step 2: Run, verify fail.**

- [ ] **Step 3: Implement** `on_vt_acquire`:
  1. `self.console_guard.vt_reldisp(VT_ACKACQ)` — ack first (kernel already gave us the VT; Xorg `xf86VTEnter` order).
  2. `acquire_master_lock()` with bounded inline retry: loop up to ~10 times, on `EBUSY` `nanosleep(~5ms)` and retry; on success break; on exhaustion `log::error!` and proceed.
  3. `self.drive_seat_event(state, SeatEventKind::Enable)` — reuses `run_resume` (re-modeset, rearm cursor, deferred full repaint).
  4. `self.resume_input_thread()` (Task 5).

- [ ] **Step 4: Arm VT_PROCESS in direct-mode init.** Where the backend finishes init in direct mode (`matches!(self.seat, Seat::Direct)` and a `ConsoleGuard` is present), call `console_guard.arm_vt_process(SIGUSR1, SIGUSR2)` and set `vt_switching_armed = true`. Do NOT arm in libseat mode (logind owns VT switching there).

- [ ] **Step 5: Run, verify pass.**

- [ ] **Step 6: Commit.** `feat(vt): direct-mode VT acquire — ack, reacquire master (retry), resume; arm VT_PROCESS`.

---

### Task 5: Pause/resume the direct-mode input thread

**Files:**
- Modify: `crates/yserver/src/input_thread.rs` (handle Pause/Resume on the existing control eventfd)
- Modify: `crates/yserver/src/kms/v2/backend.rs` (`pause_input_thread`/`resume_input_thread` via `input_sender`)
- Modify: wherever the input control-message enum is defined (`yserver_core::core_loop` control messages used by `input_sender`)

- [ ] **Step 1: Write the failing test.** The input thread, on receiving `Pause`, stops dispatching libinput events (drops/ignores them) until `Resume`; on `Resume` it `libinput_resume`s and dispatches again. Unit-test the thread's pause-state toggle in isolation (factor the "dispatch enabled" decision into a testable function if needed).

- [ ] **Step 2: Run, verify fail.**

- [ ] **Step 3: Implement.** Add `Pause`/`Resume` variants to the input control-message enum already carried over `input_sender` (the thread already has a control eventfd in its epoll set). On `Pause`: call `SendContext::suspend()` (closes device fds) and set a `paused` flag so any in-flight batch is dropped. On `Resume`: `SendContext::resume()` (reopens fds) and clear the flag. `pause_input_thread()`/`resume_input_thread()` on the backend send these via `input_sender` (mirroring the existing control-message senders at `backend.rs:8514`).

- [ ] **Step 4: Run, verify pass.**

- [ ] **Step 5: Commit.** `feat(vt): pause/resume direct-mode input thread across VT switch`.

---

### Task 6: Relocate the F12 drawable-dump hotkey

**Files:**
- Modify: `crates/yserver/src/input/hotkey.rs`

- [ ] **Step 1:** The `Ctrl+Alt+F12` drawable-dump hotkey collides with VT12 once VT switching is live (kernel consumes `Ctrl+Alt+F<n>`). Move the drawable-dump trigger to a non-F-key combo (e.g. `Ctrl+Alt+D`); keep `Ctrl+Alt+Enter` for the scanout dump (Enter isn't a VT key). Update the hotkey detector + its doc comment.

- [ ] **Step 2:** Update/extend the hotkey unit test for the new combo.

- [ ] **Step 3: Commit.** `feat(vt): move drawable-dump hotkey off F12 (VT-switch collision)`.

---

### Task 7: Build, lint, and HW smoke

- [ ] **Step 1:** `cargo build --locked`, `cargo +nightly fmt`, `cargo clippy` (plain) — fix warnings in touched code. `cargo test -p yserver -p yserver-core`.
- [ ] **Step 2: HW smoke (user-driven — the real gate).** From a lightdm/direct `yserver :0`:
  - `Ctrl-Alt-F<n>` to a text console and back → screen restores, clients still alive, no stuck keys/buttons, input works after return.
  - Switch to another graphical server (second yserver / Xorg on another VT) and back → master ping-pongs, both restore.
  - Rapid away/back (exercises no-blink coalescing + flip-drain + the SetMaster retry).
  - Confirm libseat-mode VT switching is unchanged (no regression): run under logind and Ctrl-Alt-F-switch.
- [ ] **Step 3: Commit** any fixups; open the PR.

---

## Notes

- **Libseat mode is untouched.** Everything new is gated on `Seat::Direct` + `vt_switching_armed`. The libseat path keeps delegating master handoff to logind.
- **Reuse, not reinvention:** `drive_seat_event`/`run_suspend`/`run_resume`, `acquire_master_lock`/`release_master_lock`, and the input-thread control eventfd all already exist — this plan wires them to VT signals and adds the master ioctls + input pause/resume the direct path needs.
- **vng can't test this** — VT switching needs a real seat/VT; HW smoke is the gate.
