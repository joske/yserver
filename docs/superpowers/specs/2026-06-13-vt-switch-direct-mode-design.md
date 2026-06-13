# Direct-mode VT switching — design

**Date:** 2026-06-13
**Status:** draft
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
- `libinput.suspend()` / `libinput.resume()` (close/reopen input device fds);
- re-query connectors + re-modeset + rearm HW cursor + full-damage repaint on resume;
- the no-blink coalescing of a fast flip (`pending_enable`/`pending_disable`).

Direct mode drives this **same** machinery; it only changes *what triggers it* and *who moves DRM master*.

## Design (approach A)

### Mode gating

All new behaviour is gated on `matches!(self.seat, Seat::Direct)`. In libseat mode nothing changes.

### VT acquire/release signal

Per Xorg and the 2026-05-27 reference: arm `VT_SETMODE { mode: VT_PROCESS, relsig: SIGUSR1, acqsig: SIGUSR1 }` on the controlling VT (the existing `ConsoleGuard` fd in `kms/console.rs`). A single `SIGUSR1` carries both events; we disambiguate by current `seat_state`:
- `SIGUSR1` while **Active** → kernel is asking us to **release** (user switched away).
- `SIGUSR1` while **Suspended** → kernel is telling us the VT was **acquired** (switched back).

`SIGUSR1` is freed from its diagnostic-dump duty (the scanout dump stays available via its `Ctrl+Alt+Enter` hotkey). The **outbound** `SIGUSR1` DM-readiness handshake (yserver→parent, once at startup before VT mode is armed) is unaffected. `SIGUSR2` (drawable dump) is untouched.

The signalfd thread (`lib.rs`) already consumes `SIGUSR1`; its handler routes to the VT path **only in direct mode with VT_PROCESS armed**, otherwise the legacy dump behaviour (kept for non-direct/dev runs).

### Release path (switch away), direct mode

On `SIGUSR1` while Active:
1. `drive_seat_event(Disable)` → `run_suspend` (stop scanout, drain flips, reset BOs, suspend libinput). Reused unchanged.
2. **`drmDropMaster`** on the DRM fd — the piece logind does in libseat mode. After this, another KMS client can `drmSetMaster`.
3. **`ioctl(VT_RELDISP, 1)`** — acknowledge the release so the kernel completes the switch. Without this the kernel blocks the VT switch (the Risk-#1 wedge).

### Acquire path (switch back), direct mode

On `SIGUSR1` while Suspended:
1. **`drmSetMaster`** on the DRM fd. May fail `EBUSY` if the outgoing server hasn't dropped master yet.
   - **Contention handling (the two-server case):** if `SetMaster` returns `EBUSY`, do **not** modeset; log and retry on a short bounded timer (a few attempts over ~250 ms) before proceeding. If it never succeeds, stay suspended (screen stays with the other server) rather than half-resume. This is the only genuinely new robustness concern vs the text-console case.
2. **`ioctl(VT_RELDISP, VT_ACKACQ)`** — acknowledge the acquire.
3. `drive_seat_event(Enable)` → `run_resume` (re-modeset on existing device, rearm cursor, resume libinput, deferred full repaint). Reused unchanged.

### Teardown

On exit/`Drop`, restore `VT_SETMODE { mode: VT_AUTO }` so the kernel resumes automatic VT switching, alongside the existing `ConsoleGuard` keyboard/screen-mode restore.

### Input in direct mode

`run_suspend`/`run_resume` already call `libinput.suspend()`/`resume()`, which in direct mode open/close evdev fds directly (no libseat `open_restricted`). Verify the direct-mode reopen path works after a switch (devices not exclusively grabbed by the foreground VT's server). No new input logic expected; called out as a verification point.

### Hotkey collision (note, minor)

`Ctrl+Alt+F12` currently triggers the drawable-storage dump hotkey (`input/hotkey.rs`). Once VT switching is live, the kernel consumes `Ctrl+Alt+F<n>` for VT switches, so `F12` → VT12 and the dump hotkey is shadowed. Relocate that hotkey to a non-F-key combo (e.g. keep `Ctrl+Alt+Enter` for scanout; pick a non-F combo for the drawable dump) as part of this work.

## Data flow

```
Ctrl-Alt-F2 (kernel) ── SIGUSR1 ──▶ signalfd ──▶ direct VT handler
  Active?  yes → run_suspend → drmDropMaster → VT_RELDISP(1)     [screen → console/other server]
  Active?  no  → drmSetMaster (retry EBUSY) → VT_RELDISP(ACKACQ) → run_resume  [screen restored]
```

## Error handling

- **`drmDropMaster` fails:** log; still `VT_RELDISP(1)` (don't wedge the kernel) — worst case the next server's `SetMaster` fails and it handles its own retry.
- **`drmSetMaster` `EBUSY`:** bounded retry; if exhausted, remain Suspended (don't modeset without master). Acquire is re-attempted on the next switch-back.
- **`run_resume` modeset fails (card gone):** existing behaviour (log + stay; Risk #4 in 2026-05-27).
- **Not on a real VT** (e.g. dev run, no console): `ConsoleGuard` already returns `None`; VT_PROCESS is simply not armed and the handler keeps the legacy dump behaviour.

## Testing

- **Unit:** the release/acquire decision (`SIGUSR1` + `seat_state` → suspend-vs-resume routing); `SetMaster` `EBUSY` retry-then-give-up logic (mock the ioctl result).
- **Reused coverage:** the suspend/resume state-machine tests from 2026-05-27 still apply.
- **HW smoke (the real gate — user-driven):** from a lightdm/direct `yserver :0`:
  - `Ctrl-Alt-F<n>` to a text console and back → screen restores, clients alive, no stuck keys.
  - Switch to **another graphical server** (a second yserver / Xorg on another VT) and back → master ping-pongs cleanly, both restore.
  - Rapid switch-away/switch-back (exercises no-blink coalescing + flip-drain).
- vng note: VT switching needs a real seat/VT; vng can't meaningfully exercise it — this is an HW-gated feature ([[feedback_vng_pass_not_hw_pass]]).

## Risks

- **DRM-master contention (two servers):** the `SetMaster` `EBUSY` window is the main new hazard; handled by bounded retry + stay-suspended-on-failure rather than half-resuming.
- **Single-signal ambiguity:** relying on `seat_state` to classify `SIGUSR1` as rel-vs-acq. The state machine is authoritative (we only arm VT_PROCESS in direct mode and we drive the state ourselves), so a stray/coalesced signal resolves to the current state's action — matches Xorg's single-signal model.
- **Reopen-after-switch (direct input):** evdev reopen on resume must not be blocked by the foreground server; verified in HW smoke.
