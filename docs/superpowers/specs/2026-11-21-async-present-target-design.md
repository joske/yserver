# Async Present with a future target waits for its vblank (#23)

Status: draft for sol review, then the user's approval. Base: master `3ef6570b`.

## Intent

Issue #23 (craftfever, Ivy Bridge HD 4000, SuperTuxKart 1.5, OpenGL, fullscreen): ~30 FPS on yserver against 60 on Xorg, unless the game runs under MangoHud or with `vblank_mode=3`. He narrowed it to Mesa's `vblank_mode`: `3` is fine, `0` is better, `1`/`2` (the application chooses the swap interval) show the drop. The user reproduced the async path on AMD (`~/Games/stk-tests`, 2026-11-21): without MangoHud the game is presented with async flips at ~83/s on a 59.9 Hz output, and the user perceives lag; with MangoHud it goes through synced composition at ~59/s.

## What the client sends

SuperTuxKart under `vblank_mode=1/2` uses adaptive vsync (swap interval −1, GLX_EXT_swap_control_tear). Mesa's DRI3 loader (`src/gallium/frontends/dri/loader_dri3_helper.c:1118-1159`) then waits for the previous swap's completion, sends `target_msc = last completed msc + 1`, and sets `PresentOptionAsync`: "show at the next vblank; tear only if that vblank has already passed". Measured here: 72 % of SuperTuxKart's presents have `target_msc = kernel_msc + 1`, the rest target the current msc.

## Xorg behaviour (21.1.24)

- `present_scmd_pixmap` (`../xserver/present/present_scmd.c:770-783`) pulls the execution one frame early (`exec_msc--`) **only for synced flips** (`vblank->flip && vblank->sync_flip`). Any other request with a future target, async flips and copies alike, is queued for the vblank of its target and executed there (`present_queue_vblank`, `:775-776`).
- The completion of a request reports the msc at which it took effect, so it is never earlier than a future target.

## What yserver does today

- `classify_msc_due` (`crates/yserver-core/src/present_scheduler.rs:93-111`) executes a request whose effective target is the next vblank (`eff == clock_msc + 1`) immediately when no flip is in flight, for every request. For synced requests this mirrors `exec_msc--` (the compose carrying the copy lands on the target vblank). For `PresentOptionAsync` it has no Xorg counterpart.
- craftfever's log (`yserver_nomangohud.log`, presents 2001-2008, msc 1228098-1228104) shows the effect: every other present targets `msc + 1`, is executed early at a flip retirement, and is completed with `msc` = the retirement's msc, **one below its target** (`stage=fired msc=1228098 eff=1228099`). Mesa records that msc, so the next swap targets the same msc again; two presents share each vblank and the game runs at half rate. With MangoHud (interval 1, no Async) every present completes at its target and no present parks.
- On AMD with direct scanout the async flip is accepted and executed at arrival, so frames are shown up to a refresh early and unpaced (83 flips/s on 60 Hz).

## Design

Approach: the smallest change in the existing classifier; no new scheduling path.

1. **Async future target waits.** A request with `PresentOptionAsync` in its masked options and `eff == clock_msc + 1` is parked (`MscDue::Park`) instead of executed. It becomes due through the existing due pass when the clock reaches `eff` (`eff <= clock_msc` → ExecuteNow), and is then executed with its async semantics unchanged. Requests with `eff <= clock_msc` (late, or targeting the current msc as with `vblank_mode=0`) still execute immediately. Synced requests keep today's rule.
2. **No completion earlier than the target.** A request's completion never reports an msc below its effective target when that target was in the future at arrival. With invariant 1 this holds by construction for async requests; the plan must check the stamping path (`present_ext.rs` ~3360-3390, `completion_clock` vs the due clock) and state whether a synced request can still be stamped below its target.
3. **Every classifier call site gets the option.** `classify_msc_due` gains the async flag; all callers (`present_ext.rs` arrival ~1398, drain ~845 and ~1472, supersession ~325) pass it from `pending.masked_options`, so arrival, drain and supersession cannot disagree.

Invariants beyond these are unchanged: fence waits, supersession, the fallback ladder (absolute vblank arm, idle display, blackout), async suspension after kernel rejection, VRR.

## Goals

- SuperTuxKart under `vblank_mode=1/2` on yserver presents at the refresh rate, not half of it, on the composed path (craftfever's case) and on direct scanout (AMD).
- For async presents with a future target, the completion msc equals the target (or later when late), never the msc before it.
- Tests through production entries (PresentPixmap dispatch with the Async option, the backend's flip retirement path), each failing on master; a mutation per change.

## Non-goals

- Async flips of a composed scene being rejected by the kernel (handled by the existing suspension).
- Synced request timing, supersession policy, VRR, the idle/blackout ladder.
- Vulkan WSI (`wsi_common_x11.c`) behaviour beyond what the shared classifier changes; to be checked, not redesigned.

## Verification

- Gate: fmt, clippy, workspace tests, lavapipe `--ignored`, check-layout, rule F under load for the present suites.
- AMD, after asking the user: `~/Games/stk-tests/stk-normal.sh` before and after; present rate ≤ refresh rate, completion msc == target for the async presents, and the user's judgment of the lag.
- craftfever tests a build on the HD 4000 (the user posts the request).

## Provenance

Xorg 21.1.24 (MIT) and Mesa (MIT) source citations; yserver logs from the issue and from the user's AMD runs. No code copied.
