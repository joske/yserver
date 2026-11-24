# PR #379 review round 2 — Implementation Plan

> **For agentic workers:** execute task by task; each task ends with its tests green and one signed commit.

**Goal:** close the five before-merge items of joske's second #379 review, plus the two one-line follow-ups, on the rebased branch.

**Architecture:** four independent fixes in existing code paths (pointer_fanout Motion geometry, XI1 SelectExtensionEvent decoding, one AllowSome routine shared by the AllowEvents entry points, XTest valuator copy). No new modules, no new state.

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

- Compute `child` once, before the geometry shortcut. Invariant: for the slave form, `child` is always `xi2_child_toward(state, win, slave_pass_target)`, including when `win == nested_id`. The master form keeps `child = None` in the shortcut (there `nested_id == target`).
- Test `slave_motion_child_into_parent_reports_child`: W with child D, sprite in D, motion into W outside D (stays in W), slave XI2 Motion selected on W. Expect (Xvfb) the slave Motion on W with `child == D`; master form as measured. End state: sprite in W, no grabs.
- Mutation: restoring `ResourceId(0)` in the shortcut fails the test.

### Task 2: XI1 classes 0–6 and 8 register like Xorg

**Files:** `process_request/xi/dispatch/events.rs` (`handle_xi1_select_extension_event`, `handle_xi1_change_device_dont_propagate_list` decoder if it shares the class set), `pointer_fanout/owner_events.rs` (grab-class lookup), tests in `pointer_fanout/tests/implicit_grabs.rs`.

Xorg registers every class with an EventInfo entry (`Xi/extinit.c` `SetEventInfo` block, 21.1.24 lines): `CreateMaskFromList` sets `mask[dev]` and `SelectForWindow` replaces the device's mask.

- Invariant 1: low bytes 0–6 (`_devicePointerMotionHint` … `_deviceButtonMotion`) are recognized: they touch the device (replace semantics), are stored in the entry's `xi1_masks`, and set non-delivering aggregate bits that cannot collide with the XI event-offset bits (so `ShouldFreeInputMasks` and the sticky aggregate see them as Xorg does).
- Invariant 2: class 8 (`_deviceOwnerGrabButton`) shares `DeviceButtonGrabMask` (1<<17) with class 7: the exclusivity check, the aggregate bit and the implicit-owner pick use one shared "class 7 or 8" predicate. A client holding 7 blocks another's 8 and vice versa (BadAccess).
- Invariant 3: no BadClass for DeviceButtonGrab/OwnerGrabButton on a device without buttons (`CreateMaskFromList` only validates the device; remove the `xi1_device_has_buttons` gate at events.rs ~398-409 / ~593).
- Tests (each a measured Xvfb row):
  - `xi1_button_motion_class_replaces_device_selection`: A selects [DeviceButtonPress(d)] on W, then [DeviceButtonMotion(d)]; a press on W delivers no DeviceButtonPress to A. Same for one of classes 0–5 (pick PointerMotionHint).
  - `xi1_owner_grab_button_is_exclusive_with_button_grab`: A selects class 8, B selects class 7 → BadAccess; reverse order too.
  - `xi1_owner_grab_button_picks_implicit_owner`: the newer-B-selector shape (as `implicit_grabs.rs:2445`), with B selecting class 8, so ignoring class 8 changes the owner.
  - The sticky-aggregate retention scenario (`implicit_grabs.rs:3672`) parameterized over classes 0, 6 and 8: select the class alone, narrow to NoExtensionEvent, then check the retained ordering, so omitting the aggregate bits fails.
  - `xi1_button_grab_on_buttonless_device_is_accepted`: class 7 and class 8 on the keyboard device → no error, proven with a following GetInputFocus round trip (no GetSelectedExtensionEvents packet assert: its class aliases are a Non-goal).
- Mutations: dropping any of the three invariants fails its test.

### Task 3: every AllowEvents entry point follows `AllowSome`

**Files:** `process_request/grabs.rs` (`apply_allow_events`, `xi2_allow_mode_to_core`, `apply_xi2_allow_events_for_slave`), tests in the existing AllowEvents / XIAllowEvents test modules.

Xorg has one function for this: `AllowSome(client, time, thisDev, newState)` (21.1.24 `dix/events.c:1825-1926`). Core AllowEvents calls it with the master pointer (modes 0–2) or master keyboard (3–7); `ProcXIAllowEvents` (`Xi/xiallowev.c:95-115`) calls it with the requested device, and the paired modes (AsyncPairedDevice → THAW_OTHERS, AsyncPair → THAWED_BOTH, SyncPair → FREEZE_BOTH_NEXT_EVENT) only when that device is a master; on a slave they do nothing.

- Invariant: one admission and state-transition routine, keyed by (client, device, newState), with Xorg's per-state rules:
  - admission: `(thisGrabbed && state >= FROZEN) || thisSynced`; `othersFrozen` over the client's other grabbed devices;
  - THAWED: owner → Thawed; synced → clear `other`;
  - FREEZE_NEXT_EVENT: only if `thisGrabbed` (then clear own `other` if synced);
  - THAWED_BOTH / FREEZE_BOTH_NEXT_EVENT / THAW_OTHERS: only if `othersFrozen`; THAW_OTHERS skips `thisDev`;
  - NOT_GRABBED: only if `thisGrabbed && FrozenWithEvent`.
  Core AllowEvents, XIAllowEvents on a master and XIAllowEvents on a slave all go through it. The existing stored-event, replay and queued-playback code stays and runs only for the transitions the routine performed (stored event taken only by the device's grab owner).
- Tests (each a measured Xvfb row, packets plus `other`/freeze end state):
  - `sync_pointer_from_synced_only_client_is_a_noop` / `sync_keyboard_…`: a case where only B's `other` hold freezes the device (premature thaw visible in packets on the current head), and a case where A's grab also freezes it, asserting B's hold survives B's Sync and the device stays frozen until both A's Async and B's Async are sent (assert the packet boundary after each).
  - `xi_async_paired_device_without_frozen_others_is_a_noop`: A grabs the pointer with both modes sync, keyboard not grabbed; `XIAsyncPairedDevice(pointer)` and `XIAsyncPair(pointer)` change nothing.
  - `xi_replay_device_on_frozen_no_event_slave_is_a_noop`: explicit sync slave grab (FrozenNoEvent), `XIReplayDevice(slave)` keeps it frozen and grabbed.
  - `xi_paired_modes_on_slave_are_noops`: modes 3–5 on a slave change nothing.
- Mutations: removing the `thisGrabbed` gate for FREEZE_NEXT_EVENT, the `othersFrozen` gate for THAW_OTHERS, the master-only check for paired modes, or the FrozenWithEvent check on the slave replay each fail a test.

### Task 4: small follow-ups and citations

- `process_request/xtest.rs` ~636: restore the `slot >= 6` guard (stop copying, as before the round) so a device with more axes cannot panic.
- `key_fanout.rs` ~598: cite `ActivateGrabNoDelivery`, 21.1.24 `dix/events.c:3917-3919`.
- Re-pin every Xorg `file:line` citation added by commits `up/master..HEAD` (code comments, `docs/status.md`) to the 21.1.24 tree; report the list of changed citations.
- Commit; then the full gate: workspace `--features xdmcp`, lavapipe `--ignored`, fmt, clippy, layout, `just man`, rule F on touched modules.
