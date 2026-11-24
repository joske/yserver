# PR #379 review round 2 — Implementation Plan

> **For agentic workers:** execute task by task; each task ends with its tests green and one signed commit.

**Goal:** close the five before-merge items of joske's second #379 review, plus the two one-line follow-ups, on the rebased branch.

**Architecture:** four independent fixes in existing code paths (pointer_fanout Motion geometry, XI1 SelectExtensionEvent decoding, AllowEvents state machine, XTest valuator copy). No new modules, no new state.

**Reference:** Xorg **21.1.24**, tree `/data/Projects/xorg-21.1.24` (detached `65d790bd2`). `~/Projects/xserver` is upstream main and its line numbers differ; every citation added or touched by this plan uses the 21.1.24 tree.

## Global Constraints

- Rules A/B/C/F: tests drive real requests and the production input entry; packet asserts; every scenario ends with an end-state check; touched test modules pass 5 runs under `nproc` busy loops.
- Every behaviour row is measured on Xvfb 21.1.24 with a raw-request client before its test is written; the test asserts the measured packets. If Xvfb disagrees with the expectation below, stop and report.
- Reuse first: no new modules, structs or env vars; extend the existing match arms and helpers.
- Each regression test fails on the pre-task head (mutation evidence in the report).
- Signed commits, `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`, `Implemented-By: codex (gpt-6-luna xhigh)`.

## Non-goals

- Delivery semantics of the motion qualifier classes 0–6 (PointerMotionHint, Button1–5Motion, ButtonMotion filtering of DeviceMotionNotify). They only register and replace the selection here.
- `ClassFromMask` aliases in GetSelectedExtensionEvents replies (existing follow-up).
- `refresh_xi_client_selection_projection` cost (follow-up, unmeasured).

---

### Task 1: slave Motion `child` in the `win == nested_id` shortcut

**Files:** `crates/yserver-core/src/core_loop/pointer_fanout.rs` (~2684); test next to the existing `child == D` test in `pointer_fanout/tests/owner_events.rs`.

- Invariant: for the slave form, `child` is always `xi2_child_toward(state, win, slave_pass_target)`, including when `win == nested_id`. The master form keeps `child = None` in the shortcut (there `nested_id == target`).
- Test `slave_motion_child_into_parent_reports_child`: W with child D, sprite in D, motion into W outside D (stays in W), slave XI2 Motion selected on W. Expect (Xvfb) the slave Motion on W with `child == D`; master form as measured. End state: sprite in W, no grabs.
- Mutation: restoring `ResourceId(0)` in the shortcut fails the test.

### Task 2: XI1 classes 0–6 and 8 register like Xorg

**Files:** `process_request/xi/dispatch/events.rs` (`handle_xi1_select_extension_event`, `handle_xi1_change_device_dont_propagate_list` decoder if it shares the class set), `pointer_fanout/owner_events.rs` (grab-class lookup), tests in `pointer_fanout/tests/implicit_grabs.rs`.

Xorg registers every class with an EventInfo entry (`Xi/extinit.c` `SetEventInfo` block, 21.1.24 lines): `CreateMaskFromList` sets `mask[dev]` and `SelectForWindow` replaces the device's mask.

- Invariant 1: low bytes 0–6 (`_devicePointerMotionHint` … `_deviceButtonMotion`) are recognized: they touch the device (replace semantics), are stored in the entry's `xi1_masks`, and set non-delivering aggregate bits that cannot collide with the XI event-offset bits (so `ShouldFreeInputMasks` and the sticky aggregate see them as Xorg does).
- Invariant 2: class 8 (`_deviceOwnerGrabButton`) shares `DeviceButtonGrabMask` (1<<17) with class 7: the exclusivity check, the aggregate bit and the implicit-owner pick treat "class 7 or 8" as one mask. A client holding 7 blocks another's 8 and vice versa (BadAccess).
- Invariant 3: no BadClass for DeviceButtonGrab/OwnerGrabButton on a device without buttons (`CreateMaskFromList` only validates the device; remove the `xi1_device_has_buttons` gate at events.rs ~398-409 / ~593).
- Tests (each a measured Xvfb row):
  - `xi1_button_motion_class_replaces_device_selection`: A selects [DeviceButtonPress(d)] on W, then [DeviceButtonMotion(d)]; a press on W delivers no DeviceButtonPress to A. Same for one of classes 0–5 (pick PointerMotionHint).
  - `xi1_owner_grab_button_is_exclusive_with_button_grab`: A selects class 8, B selects class 7 → BadAccess; reverse order too.
  - `xi1_owner_grab_button_picks_implicit_owner`: same shape as the existing DeviceButtonGrab owner row, with class 8.
  - `xi1_button_grab_on_buttonless_device_is_accepted`: class 7 on the keyboard device → no error; GetSelectedExtensionEvents as measured.
- Mutations: dropping any of the three invariants fails its test.

### Task 3: AllowEvents follows `AllowSome` for synced-only clients

**Files:** `process_request/grabs.rs` (`apply_allow_events`, `apply_xi2_allow_events_for_slave`), tests in the existing AllowEvents test modules.

Xorg `AllowSome` (21.1.24 `dix/events.c:1825`): THAWED clears `sync.other` for a synced client; FREEZE_NEXT_EVENT does nothing unless `thisGrabbed`; *Both / AsyncOthers act only with `othersFrozen`; NOT_GRABBED needs `thisGrabbed && FROZEN_WITH_EVENT`.

- Invariant: SyncPointer/SyncKeyboard from a client that only holds `sync.other` is a no-op (hold kept, device stays frozen, no queued event played).
- XIAllowEvents on a slave calls the same `AllowSome` in Xorg. Check whether a synced-only state is reachable on the slave path (Xvfb measurement). If it is, port the same per-mode rules to `apply_xi2_allow_events_for_slave` (state changes only for the grab owner, stored event taken only by the owner, replay only for owner + FrozenWithEvent) with a test per reachable mode. If it is not, record that in the report and change nothing there.
- Tests: `sync_pointer_from_synced_only_client_is_a_noop` and `sync_keyboard_from_synced_only_client_is_a_noop`, built on the item-1 sequences (B grabs the other device with sync mode so it holds `other`); assert no thaw, no packets, and that the owner's later Async thaws as measured.
- Mutation: removing the `thisGrabbed` gate for modes 1/4 fails both.

### Task 4: small follow-ups and citations

- `process_request/xtest.rs` ~636: restore the `slot >= 6` guard (stop copying, as before the round) so a device with more axes cannot panic.
- `key_fanout.rs` ~598: cite `ActivateGrabNoDelivery`, 21.1.24 `dix/events.c:3917-3919`.
- Re-pin every Xorg `file:line` citation added by commits `up/master..HEAD` (code comments, `docs/status.md`) to the 21.1.24 tree; report the list of changed citations.
- Commit; then the full gate: workspace `--features xdmcp`, lavapipe `--ignored`, fmt, clippy, layout, `just man`, rule F on touched modules.
