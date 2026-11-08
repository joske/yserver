# VRR: lift the cursor hold while the direct-scanout client is idle — design

**Date:** 2026-11-08 · **Status:** revision 1.1 (sol review: deterministic fixtures, three more mutation tests, both VRR callers exercised, explicit reset) · **Base:** `docs/superpowers/specs/2026-10-23-vrr-design.md`
(implemented by #294 and #298, per-output since #307–#314) · **Replaces:** #351 (draft), at the maintainer's request
· **Scope:** KMS backend, the VRR cursor hold only.

## Why

With `--vrr`, a direct-scanout client that stops drawing leaves the VRR cursor hold waiting on frames that never come.
Cursor moves then go out only at the hold's 33 ms deadline (`VRR_CURSOR_DEADLINE`), so the cursor moves at about
30 Hz. The user saw it over a still Steam screenshot and a paused fullscreen Discord video.

On #351 the maintainer asked for a narrower fix first: suspend the hold while the client is idle, and leave VRR as it
is. No entry rate, no application list, no `VRR_ENABLED` toggling. Turning VRR off while idle would be the next step
only on a driver where a cursor commit does not start a refresh.

A throwaway build of exactly that (branch `spike/vrr-cursor-idle`, 87af70d6) was measured on 2026-11-08, panel
MSI MAG 245F X24 (48–240 Hz):

- **NVIDIA RTX 5060 Ti, `--vrr all`:** over a still Steam image and a paused Discord video the cursor is smooth. The
  panel sits at 48 Hz while nothing moves and rises to 240 Hz while the mouse moves. The user saw no brightness change.
- **AMD Raphael, `--vrr`:** a fullscreen mpv, paused after 2.5 s of playback, with VRR on and no Presents. While the
  mouse moved there were 238–240 cursor-only commits/s, with motion-to-visible p99 of 4.5–5.4 ms. The OSD read 240 Hz
  while the mouse moved and 48 Hz when it stopped.

On both drivers a cursor commit starts a refresh with VRR on, so lifting the hold is enough.

## Decision

The VRR cursor hold applies only while the direct-scanout owner on that output has presented within the last
**100 ms**. The owner counts as idle when `now >= last + 100 ms`. Everything else is unchanged:

- When VRR is on and VRR is wanted;
- the 33 ms hold deadline;
- what a held move waits for.

While the client presents, the cursor rides its frames as today. Once it is idle, a cursor move is committed on its
own. The next Present brings the hold back.

## Design

- **The last Present.** The direct owner's slot (`ScanoutM2State`, one per direct scope) records when its last direct
  Present was accepted in `backend_present_try_present_direct`:
  - each branch that keeps it as the queued successor, including the replacement of an occupied successor;
  - an immediate submission, only once it succeeds.

  A Present rejected on the way, or one whose submission fails, leaves the time unchanged. The spike (87af70d6) stamped
  too early and used a direct `Instant::now()`, so it is not the production implementation. The time comes from the backend's clock
  (`monotonic_now()`, the paired clock provider), never from a direct `Instant::now()`. Without that, controlled-clock
  tests and the cursor's own `now` would disagree.
- **Not a Present.** Dispatching a queued successor, a retirement, and the cursor's own commits and their retirement do
  not renew the time.
- **Reset.** Slots survive teardown, so the time is cleared explicitly wherever the slot's other fields are cleared:
  - when the direct owner stops after its scanout is replaced;
  - on the queued first-entry fallback.

  An owner that has not presented yet counts as idle.
- **Where it is read.** The hold decision gets one more condition. Only the cursor callers use it:
  - `scene_wants_compose`;
  - `backend_scanout_next_wakeup`;
  - the held-direct cursor submit in the scheduling pass;
  - the composed-output cursor loop in `maybe_composite`.

  The VRR callers keep the live *wanted* exactly as today: the property retry after a flip, and the gamma path in
  `randr.rs`. So VRR and `VRR_ENABLED` commits do not change.
- **Per output.** Each output reads its own scope's owner. An idle client on one monitor does not lift the hold on
  another. A Root owner spans every output in its grouped scope, so they share one time, whichever output its Present
  completes on.
- **No new deadline.** A move already held when the boundary passes goes out by its existing 33 ms deadline. If
  anything re-evaluates the scheduler after the boundary, it becomes eligible at once. This bounds the VRR hold only.
  The existing waits stay in force: pending frames, queued successors, unflip, cursor commits in flight, EBUSY, cursor
  recovery, VT and DPMS. No new timer, wakeup or commit kind.
- **No change without VRR.** The hold only exists while VRR is wanted and committed on that output. With `--vrr`
  absent, or on an output that is not capable, holds and commits are exactly as today.

## Non-goals

- An entry rate, application lists, or turning VRR off while idle (#351). Those change who gets VRR at all, and the
  maintainer asked to discuss them separately if still needed.
- The Steam UI's bursts and the `reason=tearing_cursor_handoff` VRR toggles: both are pre-existing on master and are
  tracked separately.
- The panel's floor while everything is still. It stays at the VRR minimum by design, and no brightness change was
  seen on the test panel.

## Testing

Backend tests through the Backend/Present request paths, using the recording fixtures with the controlled paired clock
installed before the first Present. No sleeps. Each test names the mutation that fails it. Report which tests ran and
which skipped, since the cursor fixtures need the Vulkan test environment.

- `vrr_cursor_hold_lifts_after_idle`: VRR on, owner presents, then 100 ms with no Present; a cursor move is committed
  at once as a cursor-only commit, with no wait for the 33 ms deadline. Mutation: ignore idle in the hold.
- `vrr_cursor_hold_applies_while_presenting`: owner presents every 16 ms; a cursor move waits for the next direct frame
  as today. Mutation: lift the hold always.
- `vrr_cursor_hold_idle_boundary`: at `last + 99.999 ms` the move is held; at exactly `last + 100 ms` it is not.
  Mutation: `>` instead of `>=`.
- `vrr_cursor_hold_returns_on_next_present`: after idle, one Present restores the hold for the next move. Mutation:
  record the time only once per owner.
- `vrr_cursor_hold_queued_dispatch_does_not_renew`: accept a queued successor, advance past that acceptance's own
  100 ms boundary, dispatch and retire it, then move. The move goes out at once. Mutation: stamp the time at dispatch.
- `vrr_cursor_hold_queued_acceptance_renews`: accept A at 0, accept B as the successor at 90 ms, dispatch and retire B
  at 120 ms. A move at 125 ms is held. Mutation: do not stamp queued acceptances.
- `vrr_cursor_hold_cursor_commits_do_not_renew`: at 100 ms an idle move is committed; its event retires at 101 ms, and
  a move at 102 ms is committed at once as well. Mutation: stamp on a cursor commit or its retirement.
- `vrr_cursor_hold_pending_move_crosses_boundary`: last Present at 0, move at 90 ms (deadline 123 ms). The real
  `Backend::next_wakeup` is honoured with no further motion or Present, and exactly one cursor-only commit carries the
  latest position. Re-evaluating at 105 ms makes the wake immediate, not absent. Runs for a Root and an Output owner.
  Mutation: lose the cursor wake when idle removes the held deadline.
- `vrr_cursor_hold_idle_leaves_vrr_alone`: past 100 ms with VRR committed on, both VRR callers run with the idle state:
  - a gamma ramp through Backend/RandR with no frame in flight gives a gamma-only request with no VRR clear;
  - gamma queued behind an idle cursor-only commit, with its event retired, is submitted by the retry path with no VRR
    clear.

  Mutation, one per caller: feed the idle-aware value to the gamma path, or to the property retry.
- `vrr_cursor_hold_idle_is_per_output`: two outputs with their own Output owners, both VRR-on; one stays fresh while
  the other ages past 100 ms. The idle output commits and the fresh one holds. Mutation: read one scope for every
  output.
- `vrr_cursor_hold_reset_on_teardown`: after a Backend-driven unflip, and after the queued first-entry fallback, the
  slot's time is `None`. Mutation: skip the reset on either path.
- The existing VRR, cursor and tearing backend tests keep their behavioural assertions. Fixtures that enter direct
  scanout before installing the controlled clock (the `vrr.rs` cursor harness and its callers, and the two-output
  cursor fixture in `vrr_per_output.rs`) install the clock before direct entry instead, so wall time spent between
  setup and the first move cannot make the owner look idle.

**Hardware (ask the user first):** repeat the spike checks on the final build. On NVIDIA DP-3 (`--vrr all`) and AMD
DP-1 (`--vrr`): a still Steam image, a paused Discord video and a paused fullscreen mpv. Check that the cursor is
smooth, that the OSD reads 48 Hz while still and rises while the mouse moves, and that there is no brightness change.
Also check that mpv at 100 fps still holds the cursor to its frames.

## Provenance

- yserver master `68fa4be2`: the VRR cursor hold (`kms/render/platform/cursor.rs`, `VRR_CURSOR_DEADLINE`) and its
  callers (`kms/render/backend/kms/scanout.rs`).
- The maintainer's comment on #351, for the proposal itself.
- Hardware logs under `~/Projects/claude-scratch/vrr/active/hw/run3-nv-spike/` and `run4-amd-spike/`.

This document describes behaviour in original prose; no third-party code was used.
