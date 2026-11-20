# Per-device Implicit Pointer Grabs Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The master pointer and each attached slave pointer hold independent implicit grabs, as in Xorg 21.1.24, so a drag that ends over another client's window keeps its slave and master sequences on the press window (measured cases I1-I4, I6-I8).

**Architecture:** The slave implicit grab is an `ActivePointerGrab { implicit: true }` in the existing `ServerState::xi2_pointer_grabs`; the master implicit grab stays in `active_pointer_grab`. `implicit_pointer_grab_lifecycle` (`pointer_fanout.rs` ~2594) becomes per device, the XI2 capture snapshots each device's own mask, and every remover of a `xi2_pointer_grabs` entry goes through one implicit-aware helper.

**Tech Stack:** Rust (yserver-core). Oracle: the C client and Xvfb 21.1.24 of the measurements doc.

**Spec:** `docs/superpowers/specs/2026-11-19-per-device-implicit-grabs-design.md` (approved by the user 2026-11-19). Measurements: `docs/superpowers/findings/2026-11-19-per-device-implicit-grabs-measurements.md`.

## Global Constraints

- Base: `48f69aab` (head of PR #373), branch `feat/per-device-implicit-grabs`.
- Cite Xorg as `../xserver` (tag xorg-server-21.1.24). No copyleft code read or used.
- Reuse first: new code only where an existing helper cannot do it; justify each new function in the commit body. No new grab table, no per-protocol state machine, no second release path.
- No new `YSERVER_*` environment variables.
- Out of scope (spec Non-goals): XI1 implicit grabs (I5), the C19 replay gate, XI2 passive activation, DontPropagate, crossings, coordinates, C18, touch, keyboard. The replay path's `slave_only_replay_press` guard (`pointer_fanout.rs` ~430) stays as it is.
- Existing tests change expected values only where an I-row of the measurements doc or a cited `../xserver` line gives a different answer; each changed expectation names its row in a comment and in the commit body.
- Tests (rules A/B):
  - New tests live in a new module `crates/yserver-core/src/core_loop/pointer_fanout/tests/implicit_grabs.rs`, declared next to `mod owner_events;` in `pointer_fanout/tests.rs` (~5677).
  - Windows are created and mapped through production request dispatch (`process_request` with CreateWindow opcode 1 and MapWindow opcode 8), never `state.resources.create_window`. Selections and grabs also go through dispatch (XISelectEvents, XIGrabDevice, XIUngrabDevice, GrabPointer, GrabDevice, UnmapWindow, UnmapSubwindows, DestroyWindow).
  - Input goes through `pointer_event_fanout_to_state` only. XTest cases use origin `InputOrigin::XTest(4)`; physical cases register a source with `state.xi_register_source` as `owner_events.rs` tests do.
  - Assertions read the encoded packets from the captured peers (decode helpers may be copied from `owner_events.rs`).
  - Every scenario ends with an end-state check: `xi2_pointer_grabs`, `active_pointer_grab`, `xi2_detached_masters` and the relevant `xi1_frozen` entries equal the expected live set.
- Every test is seen failing on `48f69aab` (or on the previous task's head for tests of code that does not exist at the base). Each has one named mutation that makes it fail again, verified and reverted.
- Commits: signed (`-S`), explicit paths, trailers `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>` and `Implemented-By: codex (gpt-6-luna xhigh)`. No hardware, no yserver binary runs by implementers.
- Per-task gate: `cargo +nightly fmt`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test -p yserver-core pointer_fanout`, `cargo test -p yserver-core xi2`, `cargo test -p yserver-core grab`, `cargo test -p yserver-core xi2_grabs`, `tools/check-layout.sh`.

## Fixture (shared by every task)

`implicit_grabs.rs` builds the measurement geometry: client A = `0xA753`, client B = `0xA754`; W (A) at root (100,100) 300x300; D (A) child of W at (20,20) 100x100; OUT (B) at root (550,100) 300x300; host xids mapped as in `owner_events.rs`. The drag helper injects, from a given origin: Motion to (150,150) over D, press button 1, Motion to (650,150) over OUT, release button 1, Motion to (660,160). Task 1 creates the fixture; later tasks extend it.

## Review Focus

1. Two attached slaves (XTest 4 and a physical source) pressing at once: each slave's implicit grab ends on its own last release while the master grab ends on the aggregate. Test in Task 1.
2. The same client holding both the slave implicit grab and the master implicit grab, then XIUngrabDevice on the slave only: the master grab and its release on W survive. Test in Task 3.
3. A slave implicit grab whose owner disconnects mid-drag: the next events of that slave go to natural targets, no stale entry is left. Test in Task 3.
4. Destroying the grab window (not only unmapping it) mid-drag. Test in Task 3.
5. An explicit slave XIGrabDevice on W, then UnmapWindow of W: the explicit grab ends, the slave is reattached, and the freeze bridge is released (explicit teardown kept). Test in Task 4.

---

### Task 1: Per-device activation, mask snapshot and release

Invariants 1-6 of the spec.

**Files:**
- Modify: `crates/yserver-core/src/core_loop/pointer_fanout.rs` (`ImplicitGrabFanoutInfo` ~465-560, XI2 press capture ~2537-2575, `implicit_pointer_grab_lifecycle` ~2594-2660, the button-transition store ~648)
- Modify: `crates/yserver-core/src/server.rs` (one helper, below)
- Create: `crates/yserver-core/src/core_loop/pointer_fanout/tests/implicit_grabs.rs`; modify `pointer_fanout/tests.rs` (module declaration)

**Interfaces:**
- Produces:
  - `ServerState::remove_implicit_xi2_pointer_grab(&mut self, device_id: u16) -> Option<ActivePointerGrab>` in `server.rs`: removes and returns the entry only when it exists and `implicit` is true; otherwise leaves the table unchanged and returns `None`. Never touches `xi1_frozen`, cursors or attachment. Tasks 3 and 4 call it.
  - `ImplicitGrabFanoutInfo::source_button_transition: bool`, set from `button_transition.source_accepted` next to `master_button_transition`.
  - `fn xi2_effective_mask(state: &ServerState, window: ResourceId, device_id: u16, is_master: bool) -> u64` in `pointer_fanout.rs`: the union over all clients of the window's selections for `device_id` and 0, plus 1 when `is_master` (`../xserver/dix/inpututils.c:1153`).

- [ ] **Step 1: Write the failing tests** in `implicit_grabs.rs`:
  - `i1_slave_and_master_sequences_stay_on_press_window` (I1): A selects XI2 Motion|Press|Release for devices 4 and 2 on W; B the same on OUT; drag from XTest 4. Assert that from the press on, A receives press d4 then press d2, the drag Motion d4 then d2, and release d4 then d2, all with event window W (slave form before master form, `../xserver/mi/mieq.c:502-508`); B receives only the final Motion d4 and d2 on OUT. End state: both tables empty, `xi2_detached_masters` empty.
  - `i1a_xiall_devices_selection_matches_i1` (I1a): as I1 with device 0 for both clients; same assertions.
  - `i2_slave_only_selector_gets_no_master_events` (I2): A selects device 4 only on W; B selects device 2 on OUT. Assert A gets press, Motion and release d4 on W and no d2 at all; B gets drag Motion d2 and release d2 on OUT and no press. During the drag (after the press): `xi2_pointer_grabs[4]` is `{owner: A, grab_window: W, implicit: true, via_xi2: true, owner_events: false}`, `active_pointer_grab` is `None`, `xi2_detached_masters` is empty.
  - `i3_core_selector_gets_release_during_slave_grab` (I3): A selects device 4 only on W; B selects core PointerMotion|ButtonPress|ButtonRelease on OUT. Assert A gets only d4 events on W; B gets core MotionNotify and ButtonRelease on OUT.
  - `i7_slave_grab_holds_until_last_slave_button` (I7): A device 4 on W, B device 4 on OUT; press 1 over D, Motion to OUT, press 3, release 1, Motion, release 3. Assert every d4 event goes to A on W, including press 3; B gets no d4 until after release 3. End state empty.
  - `slave_and_master_masks_snapshot_per_device` (invariant 3): A selects device 4 Press only and device 2 Press|Motion|Release on W; B selects device 4 Motion|Release on OUT. Drag. Assert the slave grab's `xi2_mask` has only the Press bit, A receives no d4 Motion and no d4 release, B receives the drag Motion d4 and release d4 on OUT, and A receives Motion d2 and release d2 on W.
  - `two_slaves_end_their_grabs_separately` (Review Focus 1): register a physical source; A selects device 0 on W. Press 1 from XTest 4, press 1 from the physical slave, release from XTest 4: `xi2_pointer_grabs` still holds the physical slave's entry and no XTest entry; `active_pointer_grab` is `Some` (master button still down); release from the physical slave: both empty.
- [ ] **Step 2: Run** `cargo test -p yserver-core implicit_grabs`. Expected: every test FAILS on an assertion (A receives d2 in I2, B receives d4 in I1, ...), not on compilation of production code.
- [ ] **Step 3: Implement.**
  - Capture: replace the four-device union at ~2549 with `xi2_effective_mask(state, ev_win, device, form == Master)`, where `device` is the slave id for the slave form and 2 for the master form.
  - Master activation: built from `core_press` and `xi2_master_press` only (the merge of the slave candidate in `delivered_press` goes away; the core-vs-XI2 rules at ~524-560 stay). Gate on `master_button_transition`. Keep the early return for an existing master grab and for a non-implicit source entry in `xi2_pointer_grabs`; an implicit source entry does not block it (invariant 4).
  - Slave activation: from `xi2_slave_press`, gated on `source_button_transition`, only when the source slave has no entry in `xi2_pointer_grabs` or `xi1_active_grabs`. Insert `ActivePointerGrab { owner, grab_window: window, event_mask: 0, cursor: 0, time: event.time, owner_events: false, via_xi2: true, implicit: true, passive: false, xi2_mask }` into `xi2_pointer_grabs`; never call `detach_xi2_slave`.
  - Slave release: after a ButtonRelease whose source slave's `buttons_down` (`state.xi_devices.device(id).buttons_down`) is 0 and whose entry is implicit, call `remove_implicit_xi2_pointer_grab`. The master release rule (aggregate `buttons_down == 0`) is unchanged.
- [ ] **Step 4: Run** `cargo test -p yserver-core implicit_grabs`. Expected: PASS. Then the per-task gate. Existing tests that assert the old merged master grab change only with an I-row named in a comment.
- [ ] **Step 5: Mutations** (each alone, test must fail, then revert): (a) restore the slave candidate in the master choice → `i2_...` fails; (b) restore the four-device union → `slave_and_master_masks_snapshot_per_device` fails; (c) release the slave grab on master `buttons_down == 0` instead of the slave's → `two_slaves_end_their_grabs_separately` fails; (d) call `detach_xi2_slave` on install → `i2_...` fails on `xi2_detached_masters`.
- [ ] **Step 6: Commit** `fix(input): per-device implicit pointer grabs` with the test module.

### Task 2: Wheel encoding uses the form's own grab

Invariant 10.

**Files:**
- Modify: `crates/yserver-core/src/core_loop/pointer_fanout.rs` (wheel suppression ~2507-2512)
- Test: `implicit_grabs.rs`

**Interfaces:**
- Consumes: Task 1's slave implicit grab.

- [ ] **Step 1: Failing test** `motion_only_master_grab_gets_no_wheel_press_during_slave_grab`: register a physical scroll-capable source (as `owner_events.rs` scroll tests do); A selects that slave's id on W with Press|Release|Motion; press button 1 from the physical source over W (A gets the slave implicit grab); B issues XIGrabDevice on master 2, `owner_events=false`, mask XI_Motion only, on OUT. Inject one wheel step (button 5) from the physical source. Assert B receives exactly one XI_Motion d2 carrying the scroll valuator and no XI_ButtonPress d2. Release button 1; end state: `xi2_pointer_grabs` empty, B's master grab still active.
- [ ] **Step 2: Run** it. Expected: FAIL, B receives an XI_ButtonPress d2 (spec R7).
- [ ] **Step 3: Implement**: the suppression reads the grab of the form being encoded (`grab_for_form(state, xi_source, form)`) instead of `active_source_grab`.
- [ ] **Step 4: Run** the test and the per-task gate. Expected: PASS.
- [ ] **Step 5: Mutation**: put `active_source_grab` back → the test fails.
- [ ] **Step 6: Commit** `fix(input): wheel button suppression follows the form's grab`.

### Task 3: Implicit-aware removal in every remover

Invariant 7. I8.

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request/xi/dispatch/grabs.rs` (`deactivate_xi2_device_grab` ~460-500)
- Modify: `crates/yserver-core/src/core_loop/pointer_fanout.rs` (`xi_cleanup_source` ~3240-3285)
- Modify: `crates/yserver-core/src/server.rs` (`xi_unregister_facet` ~2475, `xi_unregister_source` ~2550)
- Modify: `crates/yserver-core/src/backend/recording.rs` (one field, below)
- Test: `implicit_grabs.rs`

**Interfaces:**
- Consumes: `ServerState::remove_implicit_xi2_pointer_grab` (Task 1).
- Produces: `RecordingBackend::grab_cursor_calls: Vec<Option<u32>>`, appended by an override of `Backend::set_grab_cursor` in `recording.rs` (a separate field, like `get_image_calls`, so existing call-log snapshots do not change).

Rule: each remover first calls `remove_implicit_xi2_pointer_grab(device_id)`. When it returns `Some`, the remover does no freeze-bridge release (`xi1_core_grab_bridge_release`), no `xi1_frozen` write, no `set_grab_cursor`, no `reattach_xi2_slave`, and no paired `freeze.other` clear for that entry. Crossing emission in `deactivate_xi2_device_grab` is unchanged (crossings are a Non-goal). Window destroy and disconnect (`release_active_grabs`) reach `deactivate_xi2_device_grab` and need no own branch.

- [ ] **Step 1: Failing tests:**
  - `i8_owner_ungrab_ends_slave_implicit_grab` (I8): A device 4 on W, B device 4 on OUT; press over D, Motion to OUT, A XIUngrabDevice on device 4, release. Assert XIUngrabDevice leaves `active_pointer_grab` unchanged, B receives release d4 on OUT, A receives no release d4. End state empty.
  - `slave_implicit_removal_keeps_master_grab_state` (spec Goal, invariant 7): A issues core GrabPointer on OUT with pointer_mode Async, keyboard_mode Sync and a cursor; assert `xi1_frozen[DEVICEID_MASTER_KEYBOARD].other == Some(A)`. A selects device 4 on W. Press over W (slave implicit grab for A), then end the slave grab, in three separate sub-cases built from fresh fixtures: (i) the release, (ii) A's XIUngrabDevice on device 4, (iii) `xi_cleanup_source` / `xi_unregister_source` for a physical slave used instead of XTest. In each: afterwards `active_pointer_grab` is A's explicit grab, `xi1_frozen[DEVICEID_MASTER_KEYBOARD].other == Some(A)`, `backend.grab_cursor_calls` has no `None` added by the removal, and `xi2_detached_masters` is unchanged.
  - `same_client_holds_both_grabs_ungrab_slave_keeps_master` (Review Focus 2): A selects devices 4 and 2 on W; press; A XIUngrabDevice on device 4; release. Assert A still receives release d2 on W; `active_pointer_grab` ends after the release.
  - `owner_disconnect_ends_slave_implicit_grab` (Review Focus 3): A device 4 on W, B device 4 on OUT; press; disconnect A with `crate::core_loop::process_disconnect::process_disconnect`; Motion to OUT, release. Assert B receives drag Motion d4 and release d4 on OUT; tables empty.
  - `destroy_grab_window_ends_slave_implicit_grab` (Review Focus 4): as above with DestroyWindow W by A instead of disconnect.
- [ ] **Step 2: Run** them. Expected on Task 2's head: `slave_implicit_removal_keeps_master_grab_state` fails (hold cleared or cursor `None`), `i8_...` passes or fails as the code stands; record which in the report. Every test must have failed on `48f69aab`.
- [ ] **Step 3: Implement** the rule in the four removers.
- [ ] **Step 4: Run** the tests and the per-task gate. Expected: PASS.
- [ ] **Step 5: Mutations**: (a) drop the implicit check in `deactivate_xi2_device_grab` → sub-case (ii) fails; (b) drop it in `xi_cleanup_source` → sub-case (iii) fails.
- [ ] **Step 6: Commit** `fix(input): end slave implicit grabs without explicit-grab teardown`.

### Task 4: Unviewable grab windows end slave grabs

Invariant 8. I6.

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request/grabs.rs` (`release_core_grabs_for_unviewable` ~1600-1625)
- Test: `implicit_grabs.rs`

**Interfaces:**
- Consumes: `remove_implicit_xi2_pointer_grab` (Task 1); `deactivate_xi2_device_grab` (existing, with Task 3's rule).

- [ ] **Step 1: Failing tests:**
  - `i6_unmap_grab_window_ends_slave_implicit_grab` (I6): A device 4 on W, B device 4 on OUT; press over D; A UnmapWindow W; Motion to OUT, release. Assert B receives drag Motion d4 and release d4 on OUT; A receives no d2 at all. End state empty.
  - `unmap_subwindows_of_ancestor_ends_slave_implicit_grab`: W is created as a child of a top-level P (A); A device 4 on W; press; A UnmapSubwindows on P; assert `xi2_pointer_grabs` is empty and the next release d4 goes to B on OUT.
  - `unmap_ends_explicit_slave_grab_with_full_teardown` (Review Focus 5): A XIGrabDevice on device 4 on W, `owner_events=false`, with a Sync this-device mode via the request; A UnmapWindow W. Assert the entry is gone, `xi2_detached_masters` no longer holds 4 (reattached), and `xi1_frozen[4]` is thawed with `stored == None`.
- [ ] **Step 2: Run** them. Expected: FAIL, the entry survives the unmap.
- [ ] **Step 3: Implement**: in `release_core_grabs_for_unviewable`, after the master slots, collect `xi2_pointer_grabs` entries whose `grab_window` is not viewable (sorted by device id, as window destroy does); implicit entries go through `remove_implicit_xi2_pointer_grab`, others through `deactivate_xi2_device_grab(state, backend, None, owner, dev, false, false)`.
- [ ] **Step 4: Run** the tests and the per-task gate. Expected: PASS.
- [ ] **Step 5: Mutation**: skip non-implicit entries in the new scan → `unmap_ends_explicit_slave_grab_with_full_teardown` fails.
- [ ] **Step 6: Commit** `fix(input): unviewable grab windows end slave pointer grabs`.

### Task 5: Grab requests over a slave implicit grab

Invariant 9. I4.

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request/xi/dispatch/grabs.rs` (XI1 GrabDevice status ~1140)
- Test: `implicit_grabs.rs`

- [ ] **Step 1: Failing tests:**
  - `i4_grab_statuses_during_slave_implicit_grab` (I4): A device 4 on W; B device 4 on OUT; press over D; Motion to OUT; B XIGrabDevice on OUT for device 4 → reply status 1; B XIGrabDevice on OUT for device 2 → status 0; release. Assert A receives release d4 on W and B receives release d2 on OUT.
  - `same_client_xigrabdevice_replaces_implicit_with_explicit`: A device 4 on W; press; A XIGrabDevice on device 4 → status 0; assert the entry has `implicit == false` and `xi2_detached_masters` holds 4; release; assert the entry is still there (explicit grabs do not auto-release); A XIUngrabDevice; end state empty and reattached.
  - `xi1_grab_device_over_xi2_implicit_is_already_grabbed`: A device 4 on W; press; A XI1 GrabDevice on device 4 → status 1 (AlreadyGrabbed), `../xserver/dix/events.c:5288-5291`; `xi1_active_grabs` stays empty.
- [ ] **Step 2: Run** them. Expected: the XI1 test FAILS (status 0); record the others' result.
- [ ] **Step 3: Implement**: XI1 GrabDevice answers AlreadyGrabbed when `xi2_pointer_grabs` holds an entry for the device, whatever its owner (the protocols differ).
- [ ] **Step 4: Run** the tests and the per-task gate. Expected: PASS.
- [ ] **Step 5: Mutation**: restrict the new check to `owner != client_id` → the XI1 test fails.
- [ ] **Step 6: Commit** `fix(input): XI1 GrabDevice respects XI2 slave grabs`.

### Task 6: Whole gate and oracle

Run by the coordinator, except the cargo gate, which the implementer of Task 5 also runs.

- [ ] **Step 1:** `cargo +nightly fmt -- --check`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo test --workspace --features xdmcp`; the lavapipe `--ignored` suite; `tools/check-layout.sh`; `just man`. Expected: all green.
- [ ] **Step 2:** `pointer_fanout`, `grab`, `xi2_grabs` 5 times under 16 busy loops. Expected: 0 failures.
- [ ] **Step 3:** Oracle: build the branch release binary; run I1-I8 (I5 expected unchanged) and C1-C20 of the parent measurements against Xvfb and the branch headless (`target/release/yserver :96 --headless`, lavapipe). Expected: I1-I4, I6-I8 match Xvfb under the doc's exclusions; C1-C20 as on `48f69aab`.
- [ ] **Step 4:** Add a `branch` column (or paragraph) to the measurements doc with the result; commit `docs(findings): per-device implicit grabs on the branch`.
- [ ] **Step 5:** Hardware, only with the user's permission: Enlightenment and qBittorrent drags ending over another client; one XFCE drag as master-only control.
