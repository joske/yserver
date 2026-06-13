# Direct-mode VT switching — design

**Date:** 2026-06-13
**Status:** draft (rev 2 — codex review folded in)
**Branch:** `feat/vt-switch-direct`
**Builds on:** `docs/superpowers/specs/2026-05-27-vt-switching-design.md` (the libseat/wlroots-model VT switching that is already implemented). This spec is a focused **delta**: it adds the **direct (no-libseat) mode** path.
**Reference:** Xorg `xserver/hw/xfree86/os-support/linux/lnx_init.c` + `common/xf86Events.c` (classic direct `VT_SETMODE`/`drmSetMaster` path).

## Problem

When yserver runs under lightdm in **direct mode** (`yserver :0 … vt7 -novtswitch`, no libseat/logind session management), VT switching is disabled — `seat/mod.rs`: *"Direct mode is a marker — no libseat, VT switching off."* The user cannot `Ctrl-Alt-F<n>` away to a text console or another graphical server and back; the session is inescapable and a second server can't take the GPU. This blocked testing repeatedly (2026-06-13: "can't switch to a VT… as we can't switch VT yet").

The libseat path (2026-05-27) handles this by delegating DRM-master handoff to logind. Direct mode has no logind, so **nobody drops/restores DRM master** — that is the gap this spec fills.

## Goal

In direct mode, `Ctrl-Alt-F<n>` switches away from yserver to a text console **or another graphical server (a second yserver / Xorg)** and back, with correct DRM-master handoff, no wedged screen, no lost clients, no stuck keys/buttons — matching Xorg's direct-mode behaviour.

## Non-goals

Inherits all non-goals from 2026-05-27 (XF86Switch_VT_N keysym, DPMS preservation across switch, KP mode hotkeys, return-to-boot-VT on exit, hot-plug-while-suspended, Vulkan device-loss recovery, multi-seat). Additionally:

- **Changing the libseat path.** When libseat is in use, master handoff stays delegated to logind exactly as today; this spec only adds behaviour gated on `Seat::Direct`.
- **Forcing direct mode.** Mode selection (libseat-with-fallback-to-direct) is unchanged.

## What is reused (already built by 2026-05-27)

The `drive_seat_event` state machine (`kms/v2/backend.rs:4700`) and `run_suspend`/`run_resume` already do, on `Disable`/`Enable`:
- stop the scanout gate, synthesize held key/button releases, `wait_idle_bounded`;
- drain in-flight page-flip acks + reset scanout BOs (the load-bearing fixes for "output frozen after switch" / "BO starvation");
- `core_libinput.suspend()` / `.resume()` (close/reopen input device fds) — **libseat path only**; in direct mode input is a separate thread that needs a new control path (see "Input in direct mode");
- re-query connectors + re-modeset + rearm HW cursor + full-damage repaint on resume;
- the no-blink coalescing of a fast flip (`pending_enable`/`pending_disable`).

Direct mode drives this **same** machinery; it only changes *what triggers it* and *who moves DRM master*.

## Design (approach A)

### Mode gating

All new behaviour is gated on `matches!(self.seat, Seat::Direct)`. In libseat mode nothing changes.

### VT acquire/release signals (two distinct signals)

Arm `VT_SETMODE { mode: VT_PROCESS, relsig: SIGUSR1, acqsig: SIGUSR2 }` on the controlling VT (the existing `ConsoleGuard` fd in `kms/console.rs`). Using **two distinct signals** removes all rel-vs-acq ambiguity — the signal *is* the answer — which matters because the reused `seat_state` machine has four states (`Active`, `Suspended`, **`Suspending`**, **`Resuming`**) plus `pending_enable`/`pending_disable` coalescing, so classifying by stable state alone would be wrong mid-transition. (A single shared signal à la Xorg would need `VT_GETSTATE` or state-belief disambiguation; two signals is simpler and robust.)

- `SIGUSR1` → kernel is asking us to **release** (switched away) → synthesize `Disable`.
- `SIGUSR2` → kernel granted us the VT (**acquired**) → synthesize `Enable`.

Both signals are **fed into the existing `drive_seat_event`** rather than acted on directly, so a signal arriving mid-transition is absorbed by the existing pending-flag coalescing (e.g. a release during `Resuming` sets `pending_disable` and re-suspends at the no-blink boundary).

Both signals are freed from diagnostic-dump duty; the scanout dump stays on `Ctrl+Alt+Enter` and the drawable dump moves to a non-F-key hotkey (see "Hotkey collision"). The **outbound** `SIGUSR1` DM-readiness handshake (yserver→parent, once at startup before VT mode is armed) is unaffected. The signalfd thread (`lib.rs`) routes `SIGUSR1`/`SIGUSR2` to the VT path **only in direct mode with VT_PROCESS armed**, otherwise the legacy dump behaviour (kept for non-direct/dev runs).

### Release path (switch away), direct mode

On `SIGUSR1` (release):
1. **Pause the direct-mode input thread** (see "Input in direct mode") so it stops reading evdev before we leave the foreground.
2. `drive_seat_event(Disable)` → `run_suspend` (stop scanout, drain flips, reset BOs, synthesize held-key/button releases). Reused unchanged — note its `core_libinput.suspend()` is a no-op in direct mode, hence step 1.
3. **`drmDropMaster`** on the DRM fd — the piece logind does in libseat mode. After this, another KMS client can `drmSetMaster`.
4. **`ioctl(VT_RELDISP, 1)`** — acknowledge the release so the kernel completes the switch. Without this the kernel blocks the VT switch (the Risk-#1 wedge).

### Acquire path (switch back), direct mode

On `SIGUSR2` (acquire). **Order matches Xorg `xf86VTEnter` — ACK first, then reacquire master**, because by the time the kernel sends acqsig it has *already* switched the VT to us; we cannot decline it.
1. **`ioctl(VT_RELDISP, VT_ACKACQ)`** — acknowledge the acquire first.
2. **`drmSetMaster`** on the DRM fd. The kernel serializes VT switches, so the outgoing server already received its release signal and dropped master before our acqsig — this normally succeeds on the first try. If it returns `EBUSY` (outgoing server slow to drop), do a **bounded inline retry**: a small fixed number of attempts (e.g. ~10) spaced a few ms apart, *synchronously* in the acquire handler. The acquire is a rare, isolated event (we're resuming — no clients being serviced mid-switch), so a brief synchronous wait is acceptable and keeps the handler self-contained — no `next_wakeup`/`SeatState` "waiting-for-master" substate, and no lingering window for a second VT event to land in. If all retries fail (a genuinely misbehaving other server holding master), log an error and let `run_resume` proceed best-effort (its modeset will likely fail too, leaving us suspended-blank until the next switch) — a degenerate case, not our bug.
3. `drive_seat_event(Enable)` → `run_resume` (re-modeset on existing device, rearm cursor, resume input, deferred full repaint) — run once `drmSetMaster` succeeds.

### Teardown

On exit/`Drop`, restore `VT_SETMODE { mode: VT_AUTO }` so the kernel resumes automatic VT switching, alongside the existing `ConsoleGuard` keyboard/screen-mode restore.

### Input in direct mode (NEW logic — not just reuse)

This is the part `run_suspend`/`run_resume` do **not** cover in direct mode. Those call `core_libinput.suspend()`/`resume()`, but `core_libinput` is `Some` only on the **libseat** path. In **direct** mode, input runs in a separate `yserver-libinput` thread (`input_thread.rs`, spawned at `lib.rs:282`) on an infinite epoll/`dispatch` loop with **no suspend/resume control**. evdev devices are not VT-bound, and there is no logind to `EVIOCREVOKE` the fds — so without action that thread keeps reading the keyboard/mouse after we switch away, delivering input to background clients and double-driving the foreground console/server.

**Add a control path** from the core loop to the direct-mode input thread:
- A control channel (or `AtomicBool` + eventfd to break the epoll wait) telling the thread to **pause** (stop dispatching libinput events; optionally `libinput_suspend` the context to release fds) on VT release, and **resume** (re-enable / `libinput_resume`, reopening fds) on VT acquire.
- Wired so the release path pauses input *before* `VT_RELDISP(1)`, and the acquire path resumes input as part of `run_resume`.
- Held key/button release synthesis on suspend already exists in `run_suspend` (step 3) and still applies.

This is the one genuinely new subsystem in this spec; everything else is wiring + the master ioctls.

### Hotkey collision (note, minor)

`Ctrl+Alt+F12` currently triggers the drawable-storage dump hotkey (`input/hotkey.rs`). Once VT switching is live, the kernel consumes `Ctrl+Alt+F<n>` for VT switches, so `F12` → VT12 and the dump hotkey is shadowed. Relocate that hotkey to a non-F-key combo (e.g. keep `Ctrl+Alt+Enter` for scanout; pick a non-F combo for the drawable dump) as part of this work.

## Data flow

```
release: Ctrl-Alt-F2 (kernel) ── SIGUSR1 ─▶ signalfd ─▶ direct VT handler
   pause input thread → drive_seat_event(Disable)=run_suspend → drmDropMaster → VT_RELDISP(1)
   [screen → console / other graphical server]

acquire: switch back (kernel) ── SIGUSR2 ─▶ signalfd ─▶ direct VT handler
   VT_RELDISP(VT_ACKACQ) → drmSetMaster (bounded inline retry on EBUSY)
   → drive_seat_event(Enable)=run_resume + resume input thread   [screen restored]
```

## Error handling

- **`drmDropMaster` fails:** log; still `VT_RELDISP(1)` (don't wedge the kernel) — worst case the next server's `SetMaster` fails and it handles its own retry.
- **`drmSetMaster` `EBUSY`:** bounded inline retry (small fixed count, few-ms spacing) in the acquire handler; on exhaustion, log + best-effort proceed. No `next_wakeup`/seat-substate dependency.
- **`run_resume` modeset fails (card gone):** existing behaviour (log + stay; Risk #4 in 2026-05-27).
- **Not on a real VT** (e.g. dev run, no console): `ConsoleGuard` already returns `None`; VT_PROCESS is simply not armed and the handler keeps the legacy dump behaviour.

## Testing

- **Unit:** signal→event mapping (`SIGUSR1`→`Disable`, `SIGUSR2`→`Enable`, fed to `drive_seat_event`); `drmSetMaster` `EBUSY`→bounded-inline-retry logic (mock the ioctl result; assert N attempts then best-effort proceed); input-thread pause/resume control toggling the thread's dispatch state.
- **Reused coverage:** the suspend/resume state-machine tests from 2026-05-27 still apply.
- **HW smoke (the real gate — user-driven):** from a lightdm/direct `yserver :0`:
  - `Ctrl-Alt-F<n>` to a text console and back → screen restores, clients alive, no stuck keys.
  - Switch to **another graphical server** (a second yserver / Xorg on another VT) and back → master ping-pongs cleanly, both restore.
  - Rapid switch-away/switch-back (exercises no-blink coalescing + flip-drain).
- vng note: VT switching needs a real seat/VT; vng can't meaningfully exercise it — this is an HW-gated feature ([[feedback_vng_pass_not_hw_pass]]).

## Risks

- **DRM-master contention (two servers):** the `SetMaster` `EBUSY` window is the main new hazard; handled by bounded retry + stay-suspended-on-failure rather than half-resuming.
- **Direct-mode input thread:** the new control path must reliably break the thread's epoll wait to pause (an eventfd, not just a flag the loop checks after the next event — otherwise a quiet keyboard never re-checks). evdev reopen on resume must not be blocked by the foreground server; verified in HW smoke.
- **Mid-transition signals:** handled by feeding `drive_seat_event` and reusing the `pending_enable/disable` coalescing rather than acting on signals directly; two distinct signals mean no rel-vs-acq misclassification.
