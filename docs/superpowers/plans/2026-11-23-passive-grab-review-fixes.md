# Passive Grab Activation — PR #379 Review Fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the five before-merge findings of the maintainer's review of PR #379 (2026-11-23), each pinned by a test that fails on `6de66587`.

**Architecture:** five independent, local fixes in the code #379 already touches:
- AllowEvents' replay precondition (`process_request/grabs.rs`).
- The slave Motion `child` (`pointer_fanout.rs`).
- XTest's DeviceValuator validation (`process_request/xtest.rs`).
- The master pass of passive grab lookup (`pointer_fanout.rs`, `try_match_passive_grab`).
- The XI1 implicit grab owner (`pointer_fanout.rs` `xi1_route_device_event`, plus the selection order it needs).

No new grab table, no new path.

**Tech Stack:** Rust (yserver-core; yserver KMS backend tests for XTest). Oracles: Xvfb 21.1.24 through the existing `passive-oracle`, `xtest-oracle` clients in `~/Projects/claude-scratch/passive-grabs/` and the implicit-grab oracle in `~/Projects/claude-scratch/implicit-grabs/oracle/` (coordinator only).

**Spec:** `docs/superpowers/specs/2026-11-21-passive-grab-activation-design.md` (rev 5, approved), plus the maintainer's review of #379 (quoted per task below). Where the review and the spec's Non-goals disagree, the review wins for the named item only: Task 5 brings the XI1 `DeviceButtonGrab` class into scope. `DeviceOwnerGrabButton` stays out.

## Global Constraints

- **Base:** branch `feat/passive-grab-activation` at `6de66587`. Every red run happens on its task's base. No rebase onto master in this plan.
- **Xorg citations:** cite Xorg as `../xserver` (xorg-server-21.1.24): behaviour and line numbers only, no copied code.
- **Reuse first:** new code only where an existing helper cannot do it, each piece justified in its commit body. No new grab table, no second activation, replay or XTest path. No new `YSERVER_*` environment variables.
- **Tests, rules A/B/C:** as in the parent plan (`2026-11-21-passive-grab-activation.md`, Global Constraints "Tests, rules A/B/C").
  - Setup and actions go through `process_request` (windows, selections, grabs, AllowEvents, XTestFakeInput) and through `pointer_event_fanout_to_state` or a registered physical source.
  - Assertions read the encoded packets the clients receive.
  - Every scenario ends with the shared `assert_end_state*` check.
  - The XTest tests run on `KmsBackend::for_tests()` through `process_request`, as `kms/render/backend/tests/xi_xtest.rs` does.
- **Shared fixture:** reuse `pointer_fanout/tests/implicit_grabs.rs`'s `pub(super)` helpers and `passive_grab_activation.rs`'s `p_fixture()`. Copy nothing.
- **Existing tests:** expected values change only where a cited `../xserver` line or an oracle row gives another answer. Each change names it in a comment and in the commit body.
- **Red runs and mutations:**
  - Every new test is seen failing on its task's base for its stated reason, on an assertion or (Task 1 only) the panic, never a compile error.
  - Every production change has a named mutation, listed with the tests it must turn red. Mutations are verified one at a time, by line, and reverted.
- **Commits:** signed (`-S`), explicit paths, one commit per task. Trailers: `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>` and `Implemented-By: codex (gpt-6-luna xhigh)`.
- **No server runs:** implementers run no hardware and no yserver binary. The coordinator runs the oracles.
- **Per-task gate:**
  - `cargo +nightly fmt` and `cargo clippy --workspace --all-targets -- -D warnings`.
  - `cargo test -p yserver-core pointer_fanout`, `... grab`, `... xi1` and `... xtest`.
  - `cargo test -p yserver kms::render::backend::tests::xi_xtest`.
  - `tools/check-layout.sh`.
  - Rule F: `pointer_fanout` five consecutive times under `nproc` busy loops.

## Review Focus

1. **Task 1 — another client's grab on the other device.**
   - Setup: a ReplayPointer from a client that holds only the keyboard grab, while the pointer is frozen by A's grab.
   - Expected: a no-op for the pointer. A's press stays replayable by A only while the state is `FrozenWithEvent`.
2. **Task 3 — valid requests are not rejected.**
   - `first=0, num=2` on the 2-axis XTest pointer moves the sprite exactly as before.
   - `DeviceButtonPress` with no trailing valuators is accepted.
3. **Task 4 — the slave form still flows.**
   - When the master refuses an already-held button, the XTest slave's XI2/XI1 slave-form press is still delivered.
   - A slave passive grab on that slave still activates.
4. **Task 5 — selection order follows Xorg's shared entry.**
   - A re-selection, an empty XI1 list or an XI2 change keeps the client's place.
   - An XI2 selection made first creates the place.
   - A disconnected client leaves the order.
5. **Task 2 — the `win == nested_id` shortcut.** It keeps returning `(nested_id, None, event_x, event_y)` unchanged.

---

### Task 1: ReplayPointer replays only for the grab holder

**Review item 1.** The gate at `process_request/grabs.rs:320` admits a client that is only synced (`xi1_frozen[2].other`). `replay_context` (`:420-438`) accepts only a passive grab or one owned by the requester, so `replay_context.expect(...)` (`:541`) panics. Xorg's `AllowSome` NOT_GRABBED case (`../xserver/dix/events.c:1899-1906`) replays only when the requester holds this device's grab and the state is `FROZEN_WITH_EVENT`. A client that is only synced changes nothing.

**Invariant:**
- For mode 2 (ReplayPointer) and mode 5 (ReplayKeyboard), the replay branch runs only when `this_grabbed && this_state == FrozenWithEvent`.
- That check happens before any freeze-state change, before `sync.other` is cleared and before the stored event is taken.
- Otherwise the request changes nothing: freeze state, `other`, the stored event, the active grab and any crossing all stay as they were.
- The `expect` stays reachable only under that precondition.

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request/grabs.rs` (AllowEvents, ~300-545).
- Test: `crates/yserver-core/src/core_loop/pointer_fanout/tests/passive_grab_activation.rs`.

- [ ] **Step 1: Write the failing tests**
  - `replay_pointer_from_synced_only_client_is_a_noop`, the review's sequence:
    1. A's sync passive pointer grab activates on a press over W, and the press is stored (`FrozenWithEvent`).
    2. A sends `GrabPointer(pointer_mode=Sync)`. As in Xorg's `CheckGrabForSyncs` (`../xserver/dix/events.c:1429-1430`; yserver `xi1_check_grab_for_syncs`), the state becomes `FrozenNoEvent`. The unified freeze state is the sole authority (plan `2026-07-15-freeze-state-unification.md`), so the stored bytes are not replayable.
    3. B sends `GrabKeyboard(pointer_mode=Sync)`, so `xi1_frozen[DEVICEID_MASTER_POINTER].other == Some(B)`.
    4. B sends `AllowEvents(ReplayPointer)`.
    - Assert: no panic, and no packet to any client from step 4.
    - Assert: A's grab is still active and explicit, the pointer state is still `FrozenNoEvent`, and `other` is still B.
    5. A sends `AllowEvents(ReplayPointer)`.
    - Assert: a no-op (state not `FrozenWithEvent`). No packet; A's grab is still active; state and `other` are unchanged.
    6. A ungrabs the pointer and B ungrabs the keyboard; the button is released.
    - The packets from step 6 on match Xvfb for the same sequence (coordinator Step 0). Then the end state holds.
  - `owner_replay_after_synced_client_noop_replays`: the genuine `FrozenWithEvent` case.
    1. A's sync passive pointer grab activates on a press over W (`FrozenWithEvent`).
    2. B sends `GrabKeyboard(pointer_mode=Sync)`, then `AllowEvents(ReplayPointer)`.
    - Assert: a no-op as above; state still `FrozenWithEvent`.
    3. B sends `AllowEvents(AsyncPointer)`. Xorg's THAWED case for a synced-only client clears `other` (`:1858-1863`).
    - Assert: `other == None`; the state is unchanged.
    4. A sends `AllowEvents(ReplayPointer)`.
    - Assert: the press replays below A's grab window as `p_fixture` P2r expects. Then B ungrabs, the release flows, and the end state holds.
  - `replay_keyboard_from_synced_only_client_is_a_noop`: the same shape as the first test on the keyboard, if the code path is shared. Otherwise a GUARD row stating that.
- [ ] **Step 0 (coordinator, before dispatch):** add both pointer sequences as cases to `passive-oracle`, run them on Xvfb, and put the packet rows in the brief.
- [ ] **Step 2: Red run** on `6de66587`. The first test panics at the `expect`; record the panic line.
- [ ] **Step 3: Implement** the invariant in the AllowEvents handler.
- [ ] **Step 4: Gate.** Mutations:
  - M1: drop the `this_state == FrozenWithEvent` term. The first test must turn red at step 5: A's replay deactivates the grab.
  - M2: move the precondition after `set_state`. A test must turn red: `other` is cleared by B's request.
  - M2b: drop the `this_grabbed` term. `owner_replay_after_synced_client_noop_replays` must panic or turn red at step 2.
- [ ] **Step 5: Commit** `fix(grabs): AllowEvents replays only for the client whose grab froze the event, as Xorg's AllowSome`.

### Task 2: slave Motion `child` from the pre-move target

**Review item 2.** `pointer_fanout.rs:2676` picks the event window from `slave_pass_target`, but computes `child` toward the post-move `target`. With the pointer over child D inside W, slave Motion selected on W, and a move to outside W, Xvfb reports `child=D` and the branch `child=None`.

**Invariant:**
- For the slave form, `child` is `xi2_child_toward(state, win, slave_pass_target)`.
- The `win == nested_id` shortcut stays first and unchanged.
- The master form is unchanged.
- Coordinates are not touched; they remain the listed follow-up.

**Files:**
- Modify: `crates/yserver-core/src/core_loop/pointer_fanout.rs` (~2640-2690).
- Test: `passive_grab_activation.rs` (or `implicit_grabs.rs` next to the #374 pre-move routing cases).

- [ ] **Step 1: Write the failing test** `slave_motion_child_comes_from_pre_move_target`, the review's geometry.
  - Assert the XI2 slave Motion's `event == W` and `child == D` by decoding the `child` field explicitly. The packet comparison ignores geometry.
  - Also assert the master Motion's `child` keeps its current value. That part is a GUARD.
- [ ] **Step 2: Red run** on Task 1's head: `child == None`.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Gate.** Mutation M3: `slave_pass_target` → `target` in the `child` call turns the test red.
- [ ] **Step 5: Commit** `fix(xi2): a slave Motion's child is taken toward the window it was routed from`.

### Task 3: XTest validates DeviceValuator chunks as Xorg

**Review item 3.** `ProcXTestFakeInput` (`../xserver/Xext/xtest.c:189-315, 393-424`) answers BadValue in these cases, in this order, with these `errorValue`s:
- `DeviceMotionNotify` whose first DeviceValuator has `first_valuator > numAxes` (errorValue = event type).
- More than one event on a device without valuators (errorValue = `firstValuator`).
- For each trailing event n ≥ 1:
  - `type != DeviceValuator` (errorValue = that type);
  - `first_valuator != base`, where base starts at `firstValuator` and advances by each `num_valuators` (errorValue = `first_valuator`);
  - `num_valuators` outside 1..=6 (errorValue = `num_valuators`);
  - `firstValuator + total > numAxes` (errorValue = `num_valuators`).
- Device motion `detail` outside {0, 1} (errorValue = detail; `:422`).

Today `xtest.rs:516-536` clamps (`dv[6].min(6)`) and accepts these cases, and `:563` treats any `detail != 1` as absolute. For example, `first=0, num=6` on the 2-axis XTest pointer warps the sprite where Xorg rejects the request.

**Invariant:**
- Every case above answers its error before any `backend.on_host_input`; the sprite and every client's packets are unchanged.
- Valid requests behave exactly as on `6de66587`.
- `numAxes` comes from the same device record `xi1_device_has_valuators` reads.
- The checks apply to every XI device event with trailing chunks, not only DeviceMotion, as Xorg's loop does.

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request/xtest.rs` (~310-565).
- Test: `crates/yserver/src/kms/render/backend/tests/xi_xtest.rs`.

- [ ] **Step 1: Write the failing tests**
  - `xtest_device_valuator_validation_matches_xorg`: one row per case above, plus the review's `first=0, num=6` row.
  - Each row asserts the error code, the `errorValue`, the minor/major opcodes, an unchanged sprite position, and no input packets.
  - GUARD rows (pass on base): `first=0, num=2` absolute and relative motion, and `DeviceButtonPress` without trailing chunks.
- [ ] **Step 2: Red run** on Task 2's head: each non-GUARD row fails, accepted or with the sprite moved.
- [ ] **Step 3: Implement** the checks in Xorg's order, then delete the clamps they make dead.
- [ ] **Step 4: Gate.** Mutations, each turning its row red:
  - M4 drops the contiguity check;
  - M5 restores `.min(6)` without the range check;
  - M6 drops the `numAxes` sum check;
  - M7 drops the detail check.
- [ ] **Step 5: Commit** `fix(xtest): DeviceValuator chunks are validated as Xorg's ProcXTestFakeInput`.
- [ ] **Step 6 (coordinator):** run the rows on `xtest-oracle` against Xvfb and record the error and errorValue per row in the ledger. Any mismatch reopens the task.

### Task 4: a button the master already holds activates no master-pass grab

**Review item 4.** The passive lookup is gated on `source_accepted` for a non-deferred event (`pointer_fanout.rs` ~1023-1036, `passive_transition_accepted`), and its master pass runs on that. Xorg drops a press of a button already held at the master: `UpdateDeviceState` returns `DONT_PROCESS` (`../xserver/Xi/exevents.c:948`), so `CheckDeviceGrabs` never runs for the master.

Scenario: physical button 1 held over root, the pointer over W with a core `GrabButton(1, AnyModifier)` by A, then XTest `ButtonPress 1`. The branch activates the grab and sends a core press; Xorg activates nothing.

**Invariant:**
- Master-pass candidates (core, XI2 master and XIAllMasterDevices forms) are considered only when the master transition is accepted.
- Slave-pass candidates and the slave form's delivery are unchanged.
- Nothing is added for the master pass: no core press and no master XI2 press.

**Files:**
- Modify: `crates/yserver-core/src/core_loop/pointer_fanout.rs` (the gate and/or `try_match_passive_grab`'s master pass).
- Test: `passive_grab_activation.rs`, next to `held_button_blocks_core_grab_on_master_pass` (~643).

- [ ] **Step 1: Write the failing tests**
  - `master_held_button_blocks_core_grab_on_master_pass`, the review's scenario.
    - Assert: no active grab after the XTest press; A gets no core press; the XTest slave's XI2 slave-form press (if selected) is delivered.
    - Release both, then check the end state.
  - GUARD `master_held_button_still_activates_slave_pass_grab`: an XI2 grab on the XTest slave device for button 1 activates.
- [ ] **Step 2: Red run** on Task 3's head: the grab activates and a core press is sent.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Gate.** Mutation M8: gate the master pass on `source_accepted` again; the first test turns red.
- [ ] **Step 5: Commit** `fix(grabs): a press the master already holds activates no master-pass grab, as Xorg's UpdateDeviceState`.

### Task 5: deterministic XI1 implicit grab owner

**Review item 5.** `pointer_fanout.rs:3254` takes `targets.first()`, and `xi1_window_selectors` iterates the clients `HashMap`, so the owner is arbitrary.

In Xorg:
- `DeliverEventToWindowMask` (`../xserver/dix/events.c:2276-2310`) walks the window's `inputClients` list.
- One `InputClients` entry exists per client per window. It is shared by XI1 (`SelectForWindow`, `../xserver/Xi/exevents.c:2660-2700`) and XI2 (`XISetEventMask`, `:3333-3348`) selections.
- `AddExtensionClient` (`:2750-2751`) prepends the entry when a client first selects on the window through either protocol. Later selections update it in place.
- The entry is freed when the client's resource goes (disconnect, window destroy). When a selection empties it, it is freed only if `ShouldFreeInputMasks` (`:3258-3272`) agrees.
- The owner passed to `ActivateImplicitGrab` (`:2421`) is the last client delivered to, unless one with `DeviceButtonGrabMask` (`../xserver/Xi/extinit.c:144, 1105`) was delivered to; the first such client wins.
- `DeviceButtonGrabMask` is exclusive (`../xserver/Xi/selectev.c:173-176`). A second client selecting it on the same window and device gets BadAccess (`../xserver/Xi/exevents.c:2672-2683`). `ProcXSelectExtensionEvent` applies devices in ascending device-ID order and stops at the first failure without undoing earlier devices (`../xserver/Xi/selectev.c:167-176`).

**Measured (coordinator, Xvfb 21.1.24 vs #379 `6de66587`; client `~/Projects/claude-scratch/passive-grabs/xi1owner/xi1owner.c`, results `results-xvfb.txt`, `results-y379.txt` there):**

| Row | Sequence (XI1 = DeviceButtonPress+Release on W, XTest pointer) | Xvfb owner | #379 |
|---|---|---|---|
| ab | A XI1, B XI1 | A | B |
| ba | B XI1, A XI1 | B | A |
| abg | A XI1+DeviceButtonGrab, B XI1 | A | B |
| abgb | A XI1, B XI1+DeviceButtonGrab | B | B |
| resel | A XI1, B XI1, A XI1 again | A | A |
| empty | A XI1, B XI1, A `XSelectExtensionEvent` with 0 classes, A XI1 | A (an empty list changes nothing) | A |
| xi2first | A XI2 (FocusIn, XIAllMasterDevices) on W, B XI1, A XI1 | A | B |
| xi2len0 | A XISelectEvents with `mask_len` 0, B XI1, A XI1 | B (no entry created) | — |
| xi2zerobits | A XISelectEvents with `mask_len` 4, all bits zero, B XI1, A XI1 | A (entry created) | — |
| xi2clear | A XI1, B XI1, A XI2 on then XI2 empty | A | A |
| grabconflict | A XI1+DeviceButtonGrab, B XI1+DeviceButtonGrab | B gets BadAccess (10) and no events; owner A | B accepted, both get the press |
| twodev | A DeviceButtonGrab(+Press) on the mouse (id 6); B one request: Press/Release on the XTest pointer (id 4) + DeviceButtonGrab on the mouse (id 6) | B gets BadAccess (10), yet its id-4 selection stands: B gets press and release, owner B | (not run: device set differs) |

**Invariant:**
- Per window, the selection order is the order in which each client's shared extension-entry was created: newest first. The first XI1 or XI2 selection on the window creates it.
- An XI2 mask creates the entry only when its wire `mask_len` is nonzero (`../xserver/Xi/exevents.c:3333-3362`, `xiselectev.c:314-315`). A positive length with all bits zero creates it; a zero length does not. The XI2 parser keeps that length until the entry is recorded (today `events.rs:102-133` folds both into a numeric zero).
- Later XI1/XI2 selections never move it. An empty XI1 list is a no-op.
- It leaves the order when the client disconnects or the window is destroyed, or when a selection empties all its masks and Xorg's `ShouldFreeInputMasks` would free it.
- `xi1_window_selectors` and delivery follow this order.
- The implicit grab owner is the first delivered client whose classes on that window include the device's `DeviceButtonGrab` class. Otherwise it is the last delivered client.
- `XSelectExtensionEvent` accepts the `DeviceButtonGrab` class for a button device. Its class encoding follows `libXi`'s `DeviceButtonPressGrab` (`_deviceButtonGrab` = 7 on the button class); the oracle's `abg`/`grabconflict` rows confirm it is accepted.
  - When another client already holds that class on the same window and device, that device's selection is not changed and the request answers BadAccess. Devices with a lower ID in the same request keep their applied selection, including the entry they created. Devices with a higher ID are not applied (Xorg's ascending-ID loop).
  - The same client re-selecting it, and other windows or devices, are not conflicts.
- The class delivers no event by itself.
- The XI1 per-device mask snapshot and ownership requirements of spec invariant 6 (`spec:66-72`) are unchanged.

**Files:**
- Modify: `crates/yserver-core/src/core_loop/pointer_fanout.rs` (`xi1_window_selectors`, ~3355; owner pick, ~3251).
- Modify: `crates/yserver-core/src/core_loop/process_request/xi/dispatch/events.rs` (~361-437, XSelectExtensionEvent: conflict check) and the XI2 selection path, only to record the entry's creation.
- Modify: the selection storage. Reuse `xi1_window_event_classes`. Add one per-window creation order only where it cannot be derived, and justify it.
  - Its teardown must ride the existing per-window and per-client teardown.
  - DevicePresence and global-notification selections keep their current behaviour.
- Test: `crates/yserver-core/src/core_loop/process_request/tests/xi1.rs` or `implicit_grabs.rs`.

- [ ] **Step 1: Write the failing tests:** one test per table row, with the same sequence through `process_request`.
  - Assert which client gets the release (the owner), that only the owner gets it, and, for `grabconflict`, BadAccess with B's classes not stored.
  - Each ends with the end-state check.
  - Add one row: a client that disconnects leaves the order (A, B select; B disconnects; C selects → owner A).
  - `twodev` runs on the yserver fixture's own pair of pointer slaves (lower and higher device ID), with the same assertions: BadAccess, plus the lower device's selection and entry surviving.
  - Rows `abgb`, `resel`, `empty` and `xi2clear` are GUARDs (they pass on base). `xi2len0` and `xi2zerobits` are the review round-3 pair; they differ only in `mask_len`.
- [ ] **Step 2: Red run** on Task 4's head. `ab`, `ba`, `abg`, `xi2first`, `grabconflict` and `twodev` fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Gate.** Mutations:
  - M9: `.first()` → `.last()` without the order;
  - M10: ignore the DeviceButtonGrab class;
  - M11: a re-selection moves the client to newest;
  - M12: an XI2 selection does not create the entry;
  - M13: drop the conflict check.
  - M14: make the conflict reject the whole request (`twodev` turns red).
  - M15: a zero-length XI2 mask creates the entry (`xi2len0` turns red).
- [ ] **Step 5: Commit** `fix(xi): the XI1 implicit grab goes to the client Xorg picks, DeviceButtonGrab first`.

### Task 6 (coordinator): finish

- [ ] Run the full gate:
  - workspace `--features xdmcp`, and lavapipe `--ignored` workspace-wide;
  - `just man`;
  - rule F on `pointer_fanout`, 12 runs.
- [ ] Re-run `passive-oracle`, `implicit-oracle` and `xtest-oracle` on the release build and compare with Xvfb (no rows changed except Tasks 3 and 5's new ones).
- [ ] Update `docs/status.md` and the PR body. Draft the reply to the maintainer, mapping each item to its commit, and listing the follow-ups as not done:
  - the `host_x11` `as i16` wrap;
  - `master_sprite_root` on macOS/Windows;
  - UngrabButton against an AnyButton grab;
  - the XIPassiveGrabDevice conflict check;
  - post-move coordinates.
