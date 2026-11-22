# Async Present with a future target flips in sync (#23)

Status: revision 2, after sol review r1. Fixes: F1, Xorg makes a future-target flip synced even with Async; F2, no new parking; F3, the completion claim is narrowed; F4, proven defect vs open cause; F5, acceptance cases. Next: sol review, then the user's approval. Base: master `3ef6570b`.

## Intent

Issue #23 (craftfever, Ivy Bridge HD 4000, SuperTuxKart 1.5, OpenGL, fullscreen) reports about 30 FPS on yserver against 60 on Xorg. Under MangoHud or with `vblank_mode=3` the drop goes away. The user narrowed it down with `vblank_mode`:
- `3`: fine;
- `0`: better;
- `1`/`2`: the drop. With these values the application chooses its swap interval.

The user reproduced the same path on AMD (`~/Games/stk-tests`, 2026-11-21). Without MangoHud the game gets direct async flips at about 83/s on a 59.9 Hz output, and the user reports lag.

## What the client sends

Under `vblank_mode=1/2`, SuperTuxKart uses adaptive vsync: swap interval −1, GLX_EXT_swap_control_tear. Mesa's DRI3 loader (`src/gallium/frontends/dri/loader_dri3_helper.c:1118-1159`) does three things:
- waits for the previous completion;
- targets `msc + 1`, where `msc` comes from that completion (`:548`);
- sets `PresentOptionAsync`.

Vulkan `FIFO_RELAXED` follows the same pattern (`src/vulkan/wsi/wsi_common_x11.c:1827`, `:2632`). Measured on AMD: 72 % of SuperTuxKart's presents target `kernel_msc + 1` and the rest target the current msc.

## Xorg behaviour (21.1.24)

- **Future target: synced flip, even with Async.** `present_vblank_create` (`../xserver/present/present_vblank.c:110-122`) makes a flip-capable request whose target is in the future a **synced** flip (`sync_flip = TRUE`) whatever its options. Only a request whose target is not in the future becomes an async flip (`PresentCapabilityAsync`).
- **The one-frame-early execution applies to that synced flip.** A synced flip executes one frame early (`exec_msc--`, `../xserver/present/present_scmd.c:770-771`) and lands on its target vblank, so its completion reports the target.
- **Result for adaptive vsync:** on-time frames do not tear and complete at their target. Only late frames tear.

## What yserver does today

- **Execution timing matches Xorg.** `classify_msc_due` (`present_scheduler.rs:93-111`) executes a request with `eff == clock_msc + 1` immediately when no flip is in flight, whatever its options. This corresponds to Xorg's `exec_msc--`.
- **The flip itself is wrong.** The executed request keeps `PresentOptionAsync` in the scanout candidate (`execute_present_pixmap_copy`, `present_ext.rs` ~950-1015). The KMS backend submits it as an async commit (`kms/present.rs` ~340-400, `kms/scanout.rs` ~1812), so it reaches the screen at once, up to a refresh before its target, and tears.
- **The completion then reports the msc before the target.** Its exact retirement sample is the msc before the target. The completion sweep releases the event on the domain clock but stamps it with that earlier sample (`present_ext.rs` ~3360-3390).
- **craftfever's log shows the sequence** (`yserver_nomangohud.log`, presents 2001-2008, direct scanout with successful `kind=async` commits). Every on-time present (2001, 2003, 2005, 2007) completes with `msc = target − 1`. Mesa takes that msc, so the next swap targets the same vblank again (2002, 2004, ...).

**Proven:** on-time async-adaptive frames flip early and tear, and their completions report an msc below their target. **Not proven:** that this alone causes the reported 30 FPS. The client issued about 78 presents/s in that window. The fix is expected to remove the repeated targets and the early, tearing flips; craftfever's test on the HD 4000 decides whether the drop is gone.

## Design

The smallest change matching Xorg: no new scheduling, no new parking.

1. **A future target means a synced flip.** When a Present executes, `PresentOptionAsync` is removed from the options it hands to scanout if its effective target was after the general clock msc used for that execution decision (`msc_is_after(eff, clock_msc)`, the same clock the classifier used). The flip is then submitted like any synced flip and lands on the target vblank. Requests whose target is the current or a past msc keep Async and may tear, as in Xorg.
2. **Execution timing unchanged.** `classify_msc_due` stays as it is. Arrival, drain and supersession keep their decisions, wakeups and fallback ladder, so the failed-arm gap of r1 F2 does not arise.
3. **Tearing state machine.** A synced future-target request does not qualify for tearing. The existing rule (`kms/present.rs`: a direct present without Async exits tearing and resets the probation streak) applies unchanged to the options the candidate now carries. Late frames still build a streak.
4. **VRR.** Under VRR the direct submission stays ready-driven (`2026-10-23-vrr-design.md:27`); dropping Async there only removes the tearing request. The plan must check that the VRR path does not depend on Async for its timing and state the result.

## Completion bound

For a request handled by rule 1, the flip retires on the target vblank, so the exact retirement sample, and therefore the completion msc, is `≥ target`.

The general rule that "no completion is below its target" is **not** claimed. The sweep's stamping policy, the blackout flush, epoch-change fail-open, and the existing test that stamps an older exact sample (`tests/present_pixmap.rs` ~1982) are unchanged. They are outside this change.

## Goals

- **Async future-target presents:**
  - they are submitted as synced commits;
  - their completion msc is `≥ target`, and equals it when on time;
  - no two consecutive on-time presents of one client target the same msc.
- **Async presents with a current or late target** still flip async when the ledger allows.
- **Synced presents, supersession, VRR and the idle, blackout and failed-arm ladder** behave exactly as before.

## Acceptance

- **Tests through production entries** (PresentPixmap dispatch with Async, the backend's flip and retirement path, the KMS test backend for commit kind). Each must fail on master where it covers the fix. Cases:
  - async next-target on direct scanout is synced, with completion = target;
  - async current-target stays async;
  - async late request;
  - async on the composed path;
  - synced request unchanged;
  - same-target supersession;
  - VRR direct submission;
  - Vulkan `FIFO_RELAXED`-shaped requests (Async + completion-derived next target).
- **Mutations:** a mutation for each change.
- **AMD, after asking the user.** Run `~/Games/stk-tests/stk-normal.sh` before and after. For the on-time adaptive workload, require all of the following:
  - throughput near the refresh rate (not half of it);
  - completion msc equal to target for on-time presents;
  - no repeated targets;
  - commit kind = synced for future targets;
  - the user's judgment of the lag.
  Late frames may still exceed their target and tear.
- **craftfever** tests a build on the HD 4000 (the user posts the request).
- **Gate:** fmt, clippy, workspace tests, lavapipe `--ignored`, check-layout, rule F under load for the present suites.

## Non-goals

- Kernel rejection of async flips on a composed scene (the existing suspension).
- The completion sweep's stamping policy in general.
- Synced timing, supersession policy, the VRR design, and the idle and blackout ladder.

## Provenance

Source citations from Xorg 21.1.24 (MIT) and Mesa (MIT); yserver logs from the issue and from the user's AMD runs. No code copied.
