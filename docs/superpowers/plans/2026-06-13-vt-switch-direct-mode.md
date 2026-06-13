# Direct-mode VT switching Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a lightdm-launched (direct/no-libseat) yserver `Ctrl-Alt-F<n>` away to a text console or another graphical server and back, with correct DRM-master handoff.

**Architecture:** A delta on the existing libseat VT-switch path. Arm `VT_PROCESS` on the controlling VT (direct mode only). The kernel's release/acquire signals are *marshaled through the existing core-loop `Message` channel* (exactly as the diagnostic dumps already are) and handled on the core thread, where they drive the existing `drive_seat_event`/`run_suspend`/`run_resume` machinery plus the two pieces logind does for the libseat path: explicit `drmDropMaster`/`drmSetMaster` and pausing/resuming the separate direct-mode input thread.

**Tech Stack:** Rust, Linux KMS/DRM (`drm-rs`), VT ioctls (`VT_SETMODE`/`VT_RELDISP`), signalfd, mio/epoll core loop, eventfd.

**Spec:** `docs/superpowers/specs/2026-06-13-vt-switch-direct-mode-design.md`

**Branch:** `feat/vt-switch-direct` · **Plan rev 2** (codex review folded in: signal marshaling, ConsoleGuard ownership, input control path built not assumed, `repr(C)`, test approach).

## Key plumbing facts (from review)

- Signals are **not** handled in the signalfd thread; it forwards `Message::DumpScanout`/`DumpDrawables`/`Shutdown` into the core loop (`lib.rs:321`), consumed on the core thread (`run.rs:640`). VT signals follow the same path via new `Message` variants — no cross-thread backend mutation.
- `_console_guard` is a local in `lib.rs:199`; `KmsBackendV2` has no console field. It must move into the backend so the VT ioctls are reachable from the handlers.
- `acquire_master_lock()`/`release_master_lock()` exist on `drm::Device` (`drm/device.rs:54,118`); `PlatformBackend` owns `Arc<drm::Device>` (`platform.rs:503`), reachable from the backend.
- `SendContext::suspend()`/`resume()` exist (`input/context.rs:403,414`), but there is **no** control path to the direct-mode input thread — its epoll has only the libinput fd + LED-relay eventfd (`input_thread.rs:321,348`). The control eventfd must be built (LED relay is the model).

---

### Task 1: VT_PROCESS arming + teardown in ConsoleGuard

**Files:** Modify `crates/yserver/src/kms/console.rs`.

- [ ] **Step 1: Add the VT ioctl mechanism.** Constants from `linux/vt.h`: `VT_SETMODE=0x5602`, `VT_RELDISP=0x5605`, `VT_PROCESS=1`, `VT_AUTO=0`, `VT_ACKACQ=2`. Define `#[repr(C)] struct VtMode { mode: c_char, waitv: c_char, relsig: c_short, acqsig: c_short, frsig: c_short }` — **`#[repr(C)]` is required** (the ioctl reads a fixed kernel layout). Add `ConsoleGuard` methods: `arm_vt_process(relsig: c_int, acqsig: c_int)` (ioctl `VT_SETMODE` with `mode: VT_PROCESS`), `disarm_vt_process()` (`mode: VT_AUTO`), `vt_reldisp(arg: c_long)` (ioctl `VT_RELDISP`). All operate on the existing `/dev/tty` fd.

- [ ] **Step 2: Restore VT_AUTO on Drop.** In `ConsoleGuard::drop`, call `disarm_vt_process()` (best-effort, log on error) before the existing keyboard/screen-mode restore.

- [ ] **Step 3: Unit test the ABI.** Assert `size_of::<VtMode>()` and field offsets match the kernel layout (the ioctls themselves aren't unit-testable).

- [ ] **Step 4:** `cargo build --locked`; commit `feat(vt): VtMode + VT_PROCESS arm/disarm + VT_RELDISP on ConsoleGuard`.

---

### Task 2: Move ConsoleGuard into the backend

**Files:** Modify `crates/yserver/src/lib.rs` (~199, `build_kms_backend_v2` ~421), `crates/yserver/src/kms/v2/backend.rs` (struct ~332 + constructor).

- [ ] **Step 1:** Add `console_guard: Option<ConsoleGuard>` to `KmsBackendV2` and a `vt_switching_armed: bool` (default false). Thread the guard through `build_kms_backend_v2` (new param) instead of binding it to the `_console_guard` local in `lib.rs`. Add `pub fn vt_switching_armed(&self) -> bool`.

- [ ] **Step 2:** In direct-mode init only (`matches!(self.seat, Seat::Direct)` **and** `console_guard.is_some()`), call `console_guard.arm_vt_process(SIGUSR1, SIGUSR2)` and set `vt_switching_armed = true`. Never arm in libseat mode.

- [ ] **Step 3:** `cargo build --locked` (no behaviour change yet); commit `feat(vt): backend owns ConsoleGuard; arm VT_PROCESS in direct mode`.

---

### Task 3: Marshal VT signals through the core loop

**Files:** Modify `crates/yserver-core/src/core_loop/message.rs` (~124), `crates/yserver/src/lib.rs` (signalfd ~311–343), `crates/yserver-core/src/core_loop/run.rs` (message dispatch ~640).

- [ ] **Step 1:** Add `VtRelease` and `VtAcquire` variants to `Message`.

- [ ] **Step 2:** In the signalfd thread, forward `SIGUSR1 → Message::VtRelease` and `SIGUSR2 → Message::VtAcquire` **unconditionally** (keep the variants distinct from the dump messages). Leave the outbound startup readiness `SIGUSR1` (`launch.rs:477`) untouched.

- [ ] **Step 3:** In `run_core`'s message dispatch, handle the two new variants **on the core thread**: if `backend.vt_switching_armed()` → call `backend.on_vt_release(state)` / `on_vt_acquire(state)`; else → fall back to the legacy dump (`DumpScanout` for release/USR1, `DumpDrawables` for acquire/USR2) so non-direct/dev runs keep today's diagnostic behaviour. (This keeps the armed-vs-dump decision on the core thread where backend state lives — no shared atomics, no cross-thread mutation.)

- [ ] **Step 4:** Add `on_vt_release`/`on_vt_acquire` stubs on `KmsBackendV2` (log only for now).

- [ ] **Step 5:** `cargo build --locked`; commit `feat(vt): marshal SIGUSR1/2 as VtRelease/VtAcquire to the core thread`.

---

### Task 4: Release path

**Files:** Modify `crates/yserver/src/kms/v2/backend.rs` (`on_vt_release`).

- [ ] **Step 1: Implement** `on_vt_release(&mut self, state)`:
  1. `self.pause_input_thread()` (Task 6).
  2. `self.drive_seat_event(state, SeatEventKind::Disable)` — reuses `run_suspend` (stop scanout, drain flips, reset BOs, synthesize held releases). Note its `core_libinput.suspend()` is a no-op in direct mode, hence step 1.
  3. `self.platform.device.release_master_lock()` (log on error).
  4. `self.console_guard.as_ref().unwrap().vt_reldisp(1)` to ack the release.
  Steps 2–4 error-tolerant so the ack always runs (a missing ack wedges the kernel).

- [ ] **Step 2: Unit test the testable logic.** The concrete `Arc<drm::Device>`/`ConsoleGuard` aren't mockable, so don't assert ioctl order via shims. Instead unit-test the input-pause flag toggle (Task 6) and rely on HW smoke (Task 7) for the release ordering. (Optionally add a `#[cfg(test)]` recording wrapper later if call-order coverage is wanted — not required now.)

- [ ] **Step 3:** `cargo build --locked`; commit `feat(vt): direct-mode VT release — pause input, suspend, drop master, ack`.

---

### Task 5: Acquire path

**Files:** Modify `crates/yserver/src/kms/v2/backend.rs` (`on_vt_acquire`).

- [ ] **Step 1: Implement** `on_vt_acquire(&mut self, state)`:
  1. `self.console_guard.as_ref().unwrap().vt_reldisp(VT_ACKACQ)` — **ack first** (kernel already gave us the VT; matches Xorg `xf86VTEnter`).
  2. `acquire_master_lock()` with **bounded inline retry**: loop up to 10 times; on `EBUSY`, `nix::unistd::nanosleep`/sleep ~5 ms and retry; on success break; on exhaustion `log::error!` and proceed best-effort.
  3. `self.drive_seat_event(state, SeatEventKind::Enable)` — reuses `run_resume` (re-modeset, rearm cursor, deferred full repaint).
  4. `self.resume_input_thread()` (Task 6).

- [ ] **Step 2: Unit test** the EBUSY retry as a pure helper — extract `fn try_acquire_master_bounded(dev, attempts, delay) -> Result<(), ()>` and test with a mock returning `EBUSY` k times then `Ok` (assert it retries then succeeds; and gives up after the budget). The synchronous sleep is acceptable: acquire is a rare, isolated event handled on the core thread with nothing else in flight.

- [ ] **Step 3:** `cargo build --locked`; commit `feat(vt): direct-mode VT acquire — ack, reacquire master (bounded retry), resume`.

---

### Task 6: Build the input-thread pause/resume control path

**Files:** Modify `crates/yserver/src/input_thread.rs` (add a control eventfd to the epoll set + command handling), `crates/yserver/src/kms/v2/backend.rs` (own the control handle; `pause_input_thread`/`resume_input_thread`).

- [ ] **Step 1: Build the control channel.** Model it on the existing LED-relay eventfd (`input_thread.rs:348`): create a control `eventfd` + a shared command slot (`Arc<AtomicU8>` with `None=0/Pause=1/Resume=2`, or an `Arc<Mutex<VecDeque<InputControl>>>`). Register the eventfd in the input thread's epoll set. Hand the write side to the backend at spawn time.

- [ ] **Step 2: Handle it in the thread loop.** When the control eventfd fires, read the command: `Pause` → `ctx.suspend()` (closes device fds) + set a `paused` flag so any in-flight batch is dropped; `Resume` → `ctx.resume()` (reopens) + clear `paused`. The eventfd write is what breaks `epoll_wait`, so a quiet keyboard still gets paused promptly.

- [ ] **Step 3:** On the backend, store the control write handle and implement `pause_input_thread()` / `resume_input_thread()` (write the command + bump the eventfd). These are no-ops if the handle is absent (libseat mode, where `core_libinput` handles it instead).

- [ ] **Step 4: Unit test** the `paused`-flag gate (a pure function deciding whether to dispatch a batch) and the command decode.

- [ ] **Step 5:** `cargo build --locked`; commit `feat(vt): control eventfd to pause/resume direct-mode input thread`.

---

### Task 7: F12 hotkey relocation

**Files:** Modify `crates/yserver/src/input/hotkey.rs` (~20, ~52).

- [ ] **Step 1:** `Ctrl+Alt+F12` maps to `DumpDrawables` (`hotkey.rs:20,52`) and collides with VT12 once VT switching is live. Move the drawable-dump trigger to a non-F-key combo (e.g. `Ctrl+Alt+D`); keep `Ctrl+Alt+Enter` for the scanout dump (Enter isn't a VT key). Update the detector + doc comment + its unit test.

- [ ] **Step 2:** `cargo build --locked`; commit `feat(vt): move drawable-dump hotkey off F12 (VT12 collision)`.

---

### Task 8: Build, lint, HW smoke

- [ ] **Step 1:** `cargo +nightly fmt`, `cargo clippy` (plain) — fix warnings in touched code; `cargo test -p yserver -p yserver-core`.
- [ ] **Step 2: HW smoke (user-driven — the real gate).** From a lightdm/direct `yserver :0`:
  - `Ctrl-Alt-F<n>` to a text console and back → screen restores, clients alive, no stuck keys/buttons, input works after return.
  - Switch to another graphical server (second yserver / Xorg on another VT) and back → master ping-pongs, both restore.
  - Rapid away/back (no-blink coalescing + flip-drain + SetMaster retry).
  - Libseat mode regression: under logind, Ctrl-Alt-F-switch still works (nothing changed there).
- [ ] **Step 3:** commit fixups; open the PR.

---

## Notes

- **Libseat mode untouched** — everything new is gated on `Seat::Direct` + `vt_switching_armed`; libseat keeps delegating master handoff to logind.
- **Reuse** — `drive_seat_event`/`run_suspend`/`run_resume`, `acquire_master_lock`/`release_master_lock` are reused; the genuinely new code is the VT ioctls, the `Message` marshaling, and the input control eventfd.
- **vng can't test this** — VT switching needs a real seat/VT; HW smoke is the gate.
