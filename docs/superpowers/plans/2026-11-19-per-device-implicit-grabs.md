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
- Every test is seen failing on `48f69aab` for its stated reason. Observation-only test support (the cursor recorder of Task 3) is added before the red run so the failure is an assertion, not a compile error. Every production change has a named mutation; each task's mutation table says which tests each mutation must turn red. Mutations are verified one at a time and reverted.
- Post-press check: every scenario that relies on a slave implicit grab asserts, right after the press and before any terminating operation, the expected `xi2_pointer_grabs` entry (owner, grab window, `implicit`), the master slot and `xi2_detached_masters`. This is what makes the scenario red on the base, where no slave entry exists.
- Commits: signed (`-S`), explicit paths, trailers `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>` and `Implemented-By: codex (gpt-6-luna xhigh)`. No hardware, no yserver binary runs by implementers.
- Per-task gate: `cargo +nightly fmt`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test -p yserver-core pointer_fanout`, `cargo test -p yserver-core xi2`, `cargo test -p yserver-core grab`, `cargo test -p yserver-core xi2_grabs`, `tools/check-layout.sh`.

## Fixture (shared by every task)

`implicit_grabs.rs` builds the measurement geometry: client A = `0xA753`, client B = `0xA754`; W (A) at root (100,100) 300x300; D (A) child of W at (20,20) 100x100; OUT (B) at root (550,100) 300x300; host xids mapped as in `owner_events.rs`. Helpers: `press_over_d(origin, button)` (Motion to (150,150) then the press), `move_to_out(origin)` (Motion to (650,150)), `release(origin, button)`, `final_motion(origin)` (Motion to (660,160)), and `assert_slave_implicit(f, device, owner, window)` for the post-press check. Task 1 creates the fixture; later tasks extend it.

## Review Focus

1. Two attached slaves (XTest 4 and a physical source) pressing at once: each slave's implicit grab ends on its own last release while the master grab ends on the aggregate. Test in Task 1.
2. The same client holding both the slave and the master implicit grab, then XIUngrabDevice on the slave only: the master grab and its release on W survive. Test in Task 3.
3. A slave implicit grab whose owner disconnects mid-drag while another client holds a live sync master grab: that grab's paired hold survives. Test in Task 3.
4. Destroying the grab window (not only unmapping it) mid-drag. Test in Task 3.
5. An explicit slave XIGrabDevice on W, then UnmapWindow of W: the explicit grab ends with its full teardown (reattach, freeze cleared). Test in Task 4.

---

### Task 1: Per-device activation, mask snapshot and release

Invariants 1-6 of the spec.

**Files:**
- Modify: `crates/yserver-core/src/core_loop/pointer_fanout.rs` (`ImplicitGrabFanoutInfo` ~465-560, XI2 press capture ~2537-2575, `implicit_pointer_grab_lifecycle` ~2594-2660, the button-transition store ~648, `release_passive_grab_on_button_release` ~3771-3800)
- Modify: `crates/yserver-core/src/server.rs` (one helper, below)
- Create: `crates/yserver-core/src/core_loop/pointer_fanout/tests/implicit_grabs.rs`; modify `pointer_fanout/tests.rs` (module declaration)

**Interfaces:**
- Produces:
  - `ServerState::remove_implicit_xi2_pointer_grab(&mut self, device_id: u16) -> Option<ActivePointerGrab>` in `server.rs`: removes and returns the entry only when it exists and `implicit` is true; otherwise leaves the table unchanged and returns `None`. Never touches `xi1_frozen`, cursors or attachment. It does not check the owner: callers that act for a client call it only after their own owner check. Tasks 3 and 4 call it.
  - `ImplicitGrabFanoutInfo::source_button_transition: bool`, set from `button_transition.source_accepted` next to `master_button_transition`.
  - `fn xi2_effective_mask(state: &ServerState, window: ResourceId, device_id: u16, is_master: bool) -> u64` in `pointer_fanout.rs`: the union over all clients of the window's selections for `device_id` and 0, plus 1 when `is_master` (`../xserver/dix/inpututils.c:1153`).

- [ ] **Step 1: Write the failing tests** in `implicit_grabs.rs` (origin XTest 4 unless stated):
  - `i1_slave_and_master_sequences_stay_on_press_window` (I1): A selects XI2 Motion|Press|Release for devices 4 and 2 on W; B the same on OUT. Press over D; post-press check: slave entry `{A, W, implicit}`, `active_pointer_grab` is A's implicit grab on W. Move to OUT, release, final motion. Assert A receives press d4 then d2, the drag Motion d4 then d2, release d4 then d2, all on W (slave before master, `../xserver/mi/mieq.c:502-508`); B receives only the final Motion d4 and d2 on OUT. End state: both slots empty, `xi2_detached_masters` empty.
  - `i1a_xiall_devices_selection_matches_i1` (I1a): as I1 with device 0 for both clients.
  - `i2_slave_only_selector_gets_no_master_events` (I2): A selects device 4 on W; B device 2 on OUT. Post-press check: slave entry `{A, W, implicit, via_xi2, owner_events: false}`, `active_pointer_grab` is `None`, `xi2_detached_masters` empty. Then drag. Assert A gets press, Motion and release d4 on W and no d2 at all; B gets drag Motion d2 and release d2 on OUT and no press.
  - `i3_core_selector_gets_release_during_slave_grab` (I3): A device 4 on W; B core PointerMotion|ButtonPress|ButtonRelease on OUT. Post-press check as I2. Assert A gets only d4 on W; B gets core MotionNotify and ButtonRelease on OUT.
  - `i7_slave_grab_holds_until_last_slave_button` (I7): A device 4 on W, B device 4 on OUT. Press 1 over D; post-press check; move to OUT; press 3; release 1; Motion; release 3. Assert every d4 event goes to A on W including press 3; B gets no d4 before release 3. End state empty.
  - `slave_and_master_masks_snapshot_per_device` (invariant 3): A selects device 4 Press|Motion and device 2 Press|Release on W; B selects devices 4 and 2 Motion|Release on OUT. Press over D; post-press check: slave `xi2_mask` == Press|Motion bits, master `xi2_mask` == Press|Release bits. Move to OUT, release, final motion. Assert: A gets drag Motion d4 on W and no release d4; A gets release d2 on W and no Motion d2; B gets nothing until the final Motion d4 and d2 on OUT (the grabbed device's unselected events are discarded, not delivered naturally).
  - `two_slaves_end_their_grabs_separately` (Review Focus 1): register a physical source; A selects device 0 on W. Press 1 from XTest 4, press 1 from the physical slave: both slave entries exist. Release from XTest 4: only the physical slave's entry remains and `active_pointer_grab` is `Some`. Release from the physical slave: both slots empty.
- [ ] **Step 2: Run** `cargo test -p yserver-core implicit_grabs`. Expected: every test FAILS on an assertion (the post-press slave entry is missing on the base), not on compilation.
- [ ] **Step 3: Implement.**
  - Capture: replace the four-device union at ~2549 with `xi2_effective_mask(state, ev_win, device, form == Master)`, `device` = the slave id for the slave form, 2 for the master form.
  - Master activation: from `core_press` and `xi2_master_press` only; the slave candidate leaves `delivered_press` (the core-vs-XI2 rules at ~524-560 stay). Gate on `master_button_transition`. Keep the early return for an existing master grab and for a NON-implicit source entry in `xi2_pointer_grabs`; an implicit source entry does not block it (invariant 4).
  - Slave activation: from `xi2_slave_press`, gated on `source_button_transition`, only when the source slave has no entry in `xi2_pointer_grabs` or `xi1_active_grabs`; it does not depend on the master slot. Insert `ActivePointerGrab { owner, grab_window: window, event_mask: 0, cursor: 0, time: event.time, owner_events: false, via_xi2: true, implicit: true, passive: false, xi2_mask }`; never call `detach_xi2_slave`.
  - Slave release: reuse `release_passive_grab_on_button_release`, which already resolves the source and its own `buttons_down`. When that set is empty and the entry is implicit, call `remove_implicit_xi2_pointer_grab` and nothing else; the passive branch and the master aggregate rule stay as they are. No second release check in the lifecycle.
- [ ] **Step 4: Run** `cargo test -p yserver-core implicit_grabs`. Expected: PASS. Then the per-task gate. Existing tests that asserted the old merged master grab change only with an I-row named in a comment.
- [ ] **Step 5: Mutations** (each alone; listed tests must fail; revert):

  | Mutation | Must fail |
  |---|---|
  | (a) put the slave candidate back into the master choice | `i2_...`, `i3_...` |
  | (b) restore the four-device union for both forms | `slave_and_master_masks_snapshot_per_device` |
  | (c) skip slave activation (no insert) | `i1_...`, `i1a_...`, `i7_...` |
  | (d) end the slave grab on master `buttons_down == 0` instead of the slave's set | `two_slaves_end_their_grabs_separately`, `i7_...` |
  | (e) call `detach_xi2_slave` on install | `i2_...` |
- [ ] **Step 6: Commit** `fix(input): per-device implicit pointer grabs` with the test module.

### Task 2: Wheel encoding uses the form's own grab

Invariant 10.

**Files:**
- Modify: `crates/yserver-core/src/core_loop/pointer_fanout.rs` (wheel suppression ~2507-2512)
- Test: `implicit_grabs.rs`

**Interfaces:**
- Consumes: Task 1's slave implicit grab.

- [ ] **Step 1: Failing test** `motion_only_master_grab_gets_no_wheel_press_during_slave_grab`: register a physical scroll-capable source (as the `owner_events.rs` scroll tests do); A selects that slave's id on W with Press|Release|Motion. Press button 1 from the physical source over W; post-press check. B issues XIGrabDevice on master 2, `owner_events=false`, mask XI_Motion only, on OUT (status 0). Inject ButtonPress 5 then ButtonRelease 5 from the physical source. Assert B receives exactly one XI_Motion d2 carrying the scroll valuator and no XI_ButtonPress d2. Release button 1. End state: `xi2_pointer_grabs` empty, B's master grab still active.
- [ ] **Step 2: Run** it. Expected: FAIL. On the base the post-press check fails; on Task 1's head, B receives an XI_ButtonPress d2 (spec R7). Record both.
- [ ] **Step 3: Implement**: the suppression reads the grab of the form being encoded (`grab_for_form(state, xi_source, form)`) instead of `active_source_grab`.
- [ ] **Step 4: Run** the test and the per-task gate. Expected: PASS.
- [ ] **Step 5: Mutation**: put `active_source_grab` back → the test fails.
- [ ] **Step 6: Commit** `fix(input): wheel button suppression follows the form's grab`.

### Task 3: Implicit-aware removal in every remover

Invariant 7. I8.

**Files:**
- Modify: `crates/yserver-core/src/backend/recording.rs` (observation only, first)
- Modify: `crates/yserver-core/src/core_loop/process_request/xi/dispatch/grabs.rs` (`deactivate_xi2_device_grab` ~460-500)
- Modify: `crates/yserver-core/src/core_loop/pointer_fanout.rs` (`xi_cleanup_source` ~3240-3285)
- Modify: `crates/yserver-core/src/core_loop/process_disconnect.rs` (force-thaw ~195-210)
- Test: `implicit_grabs.rs`

**Interfaces:**
- Consumes: `ServerState::remove_implicit_xi2_pointer_grab` (Task 1).
- Produces: `RecordingBackend::grab_cursor_calls: Vec<Option<u32>>`, appended by an override of `Backend::set_grab_cursor` in `recording.rs` (a separate field, like `get_image_calls`, so existing call-log snapshots do not change).

Rules:
- **XIUngrabDevice / destroy / disconnect** reach `deactivate_xi2_device_grab`. Inside its existing owner-validated slave-pointer branch (after the `grab.owner == owner` filter, ~489), when the entry is implicit: keep the captured grab window for the unchanged crossing code, remove the entry with `remove_implicit_xi2_pointer_grab`, and skip `xi1_frozen` writes, `xi1_core_grab_bridge_release`, `reattach_xi2_slave` and `set_grab_cursor`. Another client's XIUngrabDevice still removes nothing.
- **Device removal**: production runs `xi_cleanup_source` before `xi_unregister_source` / `xi_unregister_facet` (`crates/yserver/src/kms/render/backend/portable/devices.rs:760-777`), and the cleanup removes every `xi2_pointer_grabs` entry of the source. The rule goes in `xi_cleanup_source`: an implicit entry is removed bare, with no freeze write, no bridge release and no reattach. `xi_unregister_source` and `xi_unregister_facet` stay unchanged because no entry is left when they run; this is how the spec's source and facet paths are met.
- **Disconnect force-thaw** (pre-existing defect, own commit): `process_disconnect` force-thaws every frozen device when the departing client held no XI1 grab and `xi1_active_grabs` is empty (~195), even while another client's core or XI2 grab is live. `xi1_thaw_device` clears `freeze.other`. Gate the force-thaw on no active grab in any slot (`active_pointer_grab`, `active_keyboard_grab`, `xi2_pointer_grabs`, `xi2_keyboard_grabs`, `xi1_active_grabs`), which is what its comment says it means.

- [ ] **Step 1: Add the recorder** (`grab_cursor_calls` and the override). Run `cargo test -p yserver-core` for `recording`: no existing test changes.
- [ ] **Step 2: Write the failing tests:**
  - `i8_owner_ungrab_ends_slave_implicit_grab` (I8): A device 4 on W, B device 4 on OUT. Press over D; post-press check; move to OUT: A receives drag Motion d4 on W, B nothing. A XIUngrabDevice on device 4: the slave entry is gone, `active_pointer_grab` unchanged. Release: B receives release d4 on OUT, A no release d4. End state empty.
  - `foreign_ungrab_leaves_slave_implicit_grab`: as I8, but B issues XIUngrabDevice on device 4: the entry stays; release d4 goes to A on W.
  - `slave_implicit_removal_keeps_master_grab_state` (invariant 7, spec Goal), three fresh-fixture sub-cases. Setup: B issues core GrabPointer on OUT with pointer_mode Async, keyboard_mode Sync and a cursor; assert `xi1_frozen[DEVICEID_MASTER_KEYBOARD].other == Some(B)`. A selects the pressing slave on W. Press over W; post-press check: A's slave entry exists while B's explicit master grab stays in `active_pointer_grab`. Then end the slave grab by (i) the release, (ii) A's XIUngrabDevice, (iii) removal of a physical slave through `xi_cleanup_source` followed by `xi_unregister_facet`, in the production order. Afterwards in each: `active_pointer_grab` is B's grab, `xi1_frozen[DEVICEID_MASTER_KEYBOARD].other == Some(B)`, `backend.grab_cursor_calls` gained no `None`, `xi2_detached_masters` unchanged.
  - `same_client_holds_both_grabs_ungrab_slave_keeps_master` (Review Focus 2): A selects devices 4 and 2 on W. Press; post-press check: slave entry and master implicit grab both A's. A XIUngrabDevice on device 4: slave entry gone, master grab unchanged. Release: A receives release d2 on W and no release d4.
  - `owner_disconnect_keeps_other_clients_hold` (Review Focus 3): setup of `slave_implicit_removal_keeps_master_grab_state` with B also selecting device 4 on OUT. Press; post-press check; `process_disconnect(A)`. Assert the slave entry is gone, B's grab, hold and cursor remain. Move to OUT, release: B receives Motion d4 and release d4 on OUT.
  - `destroy_grab_window_ends_slave_implicit_grab` (Review Focus 4): A device 4 on W, B device 4 on OUT. Press; post-press check; A DestroyWindow W. Move to OUT, release: B receives Motion d4 and release d4 on OUT. End state empty.
- [ ] **Step 3: Run** them on Task 2's head. Expected: `slave_implicit_removal_keeps_master_grab_state` (ii), (iii) and `owner_disconnect_keeps_other_clients_hold` FAIL on the hold or cursor assertions; record the others. On `48f69aab` every test fails at the post-press check.
- [ ] **Step 4: Implement** the rules, the force-thaw gate in its own commit first.
- [ ] **Step 5: Run** the tests and the per-task gate. Expected: PASS.
- [ ] **Step 6: Mutations:**

  | Mutation | Must fail |
  |---|---|
  | (a) drop the implicit branch in `deactivate_xi2_device_grab` | sub-case (ii), `same_client_holds_both_grabs_ungrab_slave_keeps_master` |
  | (b) move the implicit removal before the owner filter | `foreign_ungrab_leaves_slave_implicit_grab` |
  | (c) drop the implicit branch in `xi_cleanup_source` | sub-case (iii) |
  | (d) restore the old force-thaw condition | `owner_disconnect_keeps_other_clients_hold` |
  | (e) skip removal for implicit entries in `deactivate_xi2_device_grab` (return early) | `i8_...`, `destroy_grab_window_ends_slave_implicit_grab` |
- [ ] **Step 7: Commits** `fix(input): disconnect thaws only when no grab is live` (force-thaw) and `fix(input): end slave implicit grabs without explicit-grab teardown`.

### Task 4: Unviewable grab windows end slave grabs

Invariant 8. I6.

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request/grabs.rs` (`release_core_grabs_for_unviewable` ~1600-1625)
- Test: `implicit_grabs.rs`

**Interfaces:**
- Consumes: `remove_implicit_xi2_pointer_grab` (Task 1); `deactivate_xi2_device_grab` (with Task 3's rule).

- [ ] **Step 1: Failing tests:**
  - `i6_unmap_grab_window_ends_slave_implicit_grab` (I6): A device 4 on W, B device 4 on OUT. Press over D; post-press check; move to OUT: A receives drag Motion d4 on W. A UnmapWindow W: the slave entry is gone. Motion, release: B receives Motion d4 and release d4 on OUT; A receives no d2 at all. End state empty.
  - `unmap_subwindows_of_ancestor_ends_slave_implicit_grab`: W is created as a child of a top-level P (A) at (100,100). A device 4 on W, B device 4 on OUT. Press; post-press check; A UnmapSubwindows on P: the slave entry is gone; release d4 goes to B on OUT.
  - `unmap_ends_explicit_slave_grab_with_full_teardown` (Review Focus 5): A XIGrabDevice on device 4 on W, `owner_events=false`, this-device mode Sync; assert `xi2_detached_masters` holds 4. A UnmapWindow W. Assert the entry is gone, 4 is no longer in `xi2_detached_masters`, `xi1_frozen[4]` is thawed with `stored == None`.
- [ ] **Step 2: Run** them. Expected: FAIL; on Task 3's head the entry survives the unmap.
- [ ] **Step 3: Implement**: in `release_core_grabs_for_unviewable`, after the master slots, collect `xi2_pointer_grabs` entries whose `grab_window` is not viewable, sorted by device id as window destroy does; implicit entries go through `remove_implicit_xi2_pointer_grab`, others through `deactivate_xi2_device_grab(state, backend, None, owner, dev, false, false)`.
- [ ] **Step 4: Run** the tests and the per-task gate. Expected: PASS.
- [ ] **Step 5: Mutations:**

  | Mutation | Must fail |
  |---|---|
  | (a) skip non-implicit entries in the new scan | `unmap_ends_explicit_slave_grab_with_full_teardown` |
  | (b) skip implicit entries in the new scan | `i6_...`, `unmap_subwindows_of_ancestor_...` |
- [ ] **Step 6: Commit** `fix(input): unviewable grab windows end slave pointer grabs`.

### Task 5: Grab requests over a slave implicit grab

Invariant 9. I4.

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request/xi/dispatch/grabs.rs` (XI1 GrabDevice status ~1140)
- Test: `implicit_grabs.rs`

- [ ] **Step 1: Failing tests:**
  - `i4_grab_statuses_during_slave_implicit_grab` (I4): A device 4 on W; B device 4 on OUT. Press over D; post-press check; move to OUT. B XIGrabDevice on OUT for device 4 → status 1; B XIGrabDevice on OUT for device 2 → status 0. Release. Assert A receives release d4 on W and B receives release d2 on OUT.
  - `same_client_xigrabdevice_replaces_implicit_with_explicit`: A device 4 on W. Press; post-press check. A XIGrabDevice on device 4 → status 0; the entry has `implicit == false` and `xi2_detached_masters` holds 4. Release: the entry stays (explicit grabs do not auto-release). A XIUngrabDevice: end state empty, 4 reattached.
  - `xi1_grab_device_over_xi2_implicit_is_already_grabbed`: A device 4 on W. Press; post-press check. A XI1 GrabDevice on device 4 → status 1 (AlreadyGrabbed, `../xserver/dix/events.c:5288-5291`); `xi1_active_grabs` stays empty.
- [ ] **Step 2: Run** them. Expected: all fail on `48f69aab` at the post-press check; on Task 4's head the XI1 test fails with status 0.
- [ ] **Step 3: Implement**: XI1 GrabDevice answers AlreadyGrabbed when `xi2_pointer_grabs` holds an entry for the device, whatever its owner (the protocols differ).
- [ ] **Step 4: Run** the tests and the per-task gate. Expected: PASS.
- [ ] **Step 5: Mutation**: restrict the new check to `owner != client_id` → `xi1_grab_device_over_xi2_implicit_is_already_grabbed` fails. `i4_...` and `same_client_...` need no production change after Task 1; mutation (c) of Task 1 must also turn them red (record it).
- [ ] **Step 6: Commit** `fix(input): XI1 GrabDevice respects XI2 slave grabs`.

### Task 6: Whole gate and oracle

Run by the coordinator, except the cargo gate, which the implementer of Task 5 also runs.

- [ ] **Step 1:** `cargo +nightly fmt -- --check`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo test --workspace --features xdmcp`; the lavapipe `--ignored` suite; `tools/check-layout.sh`; `just man`. Expected: all green.
- [ ] **Step 2:** `pointer_fanout`, `grab`, `xi2_grabs` 5 times under 16 busy loops. Expected: 0 failures.
- [ ] **Step 3:** Oracle: build the branch release binary; run I1-I8 (I5 expected unchanged) and C1-C20 of the parent measurements against Xvfb and the branch headless (`target/release/yserver :96 --headless`, lavapipe). Expected: I1-I4, I6-I8 match Xvfb under the doc's exclusions; C1-C20 as on `48f69aab`.
- [ ] **Step 4:** Add the branch result to the measurements doc; commit `docs(findings): per-device implicit grabs on the branch`.
- [ ] **Step 5:** Hardware, only with the user's permission: Enlightenment and qBittorrent drags ending over another client; one XFCE drag as master-only control.
