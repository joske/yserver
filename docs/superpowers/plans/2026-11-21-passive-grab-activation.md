# Passive Grab Activation, XI1 Implicit Grabs and XTest Device Motion Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Passive button grabs select, activate and replay as Xorg 21.1.24 does (P1-P5, P4b, P4c, the C19 gate and xfwm4's double press). XI1 deliveries create per-device implicit grabs (I5). XTest DeviceMotionNotify moves the sprite (X1-X3).

**Architecture:**
- `ServerState::find_passive_grab` becomes a slave pass over the whole chain, then a master pass, with newest-wins per window, and returns the pass it matched in.
- The activation block in `pointer_fanout.rs` delivers one form, from that pass. A slave activation skips master processing, including the master button transition.
- Replay carries the frozen device and the grab window, so it reprocesses only that device's forms, with passive checks below the grab window. The C19 guard goes.
- XI1 implicit grabs reuse `xi1_active_grabs` with a mask snapshot.
- XTest device motion reuses the core FakeInput motion path, with the request's axes carried on the event.

**Tech Stack:** Rust (yserver-core). Oracles: `passive-oracle`, `implicit-oracle` and `xtest-oracle` against Xvfb 21.1.24, per the measurements doc.

**Spec:** `docs/superpowers/specs/2026-11-21-passive-grab-activation-design.md` (revision 5, approved by the user 2026-11-21). Measurements: `docs/superpowers/findings/2026-11-21-passive-grab-activation-measurements.md`.

## Global Constraints

- **Base:** branch `feat/passive-grab-activation`, on PR #374 `af690cdf` plus the spec commits. Red runs are against the base binary of each task, i.e. the commit before the task's first commit. The spec's "fails on `d479e443`" means the same pointer code.
- **Citations:** cite Xorg as `../xserver` (tag xorg-server-21.1.24). No copyleft code is read or used.
- **Reuse first:**
  - New code only where an existing helper cannot do the job. Justify each new function in the commit body.
  - No new grab table, no per-protocol flag machine, no second activation or release path, no second XTest motion path. The spec's anti-goals bind.
- **Environment:** no new `YSERVER_*` environment variables.
- **Out of scope (spec Non-goals):**
  - XI1 passive grabs (GrabDeviceButton) and key grabs.
  - The XI2 passive grab's stored `0xFFFF_FFFF` mask.
  - DontPropagate, crossings, coordinates and touch.
  - The DeviceButtonPressGrab and DeviceOwnerGrabButton classes.
- **Changed expectations:** existing tests change expected values only where a P, I5 or X row of the measurements doc, or a cited `../xserver` line, gives another answer. Each changed expectation names its row in a comment and in the commit body. The XTS-style "no XI1 implicit grab" expectation changes under I5.
- **Tests, rules A/B/C:**
  - **Where:** new tests live in the existing modules named per task.
  - **Shared helpers:** the fixture helpers of `pointer_fanout/tests/implicit_grabs.rs` are made `pub(super)` and reused; they are not copied.
  - **Requests:** windows are created and mapped through `process_request` dispatch, never `state.resources.create_window`. Selections, grabs, AllowEvents/XIAllowEvents, XTestFakeInput and UnmapWindow also go through dispatch.
  - **Input:** input goes through `pointer_event_fanout_to_state` only, with origin `InputOrigin::XTest(4)` unless stated. The only exception is XTest device motion, which enters through its XTestFakeInput request.
  - **Assertions:** they read the encoded packets of the captured peers.
  - **End state:** every scenario ends with an end-state check. `active_pointer_grab`, `xi2_pointer_grabs`, `xi1_active_grabs`, `xi1_frozen` and `xi2_detached_masters` must equal the expected live set. `buttons_down` and every per-source held-button set must be empty (spec invariant 3a). This holds also for P2 and P4b.
  - **Post-press check:** right after the press, before any AllowEvents or release, each scenario asserts which grab is active: owner, window, device slot, `via_xi2`, `passive`/`implicit`.
- **Red runs and mutations:**
  - Every new test is seen failing on its task's base for its stated reason, on an assertion and not a compile error. The exceptions are the tests the plan marks GUARD, which must pass on the base.
  - Every production change has a named mutation. The mutation table says which tests it must turn red. Mutations are verified one at a time, by line, and reverted.
- **Commits:**
  - Signed (`-S`), with explicit paths.
  - Trailers `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>` and `Implemented-By: codex (gpt-6-luna xhigh)`.
- **No server runs:** implementers run no hardware and no yserver binary. The coordinator runs the oracles (Task 6).
- **Per-task gate:**
  - `cargo +nightly fmt` and `cargo clippy --workspace --all-targets -- -D warnings`.
  - `cargo test -p yserver-core` for the modules `pointer_fanout`, `grab`, `xi2_grabs`, `xi1`, `xtest`, `implicit_grabs` and `passive_grab_activation`.
  - `tools/check-layout.sh`.
  - Rule F: `pointer_fanout` five consecutive times under `nproc` busy loops.

## Fixture

**P fixture (Tasks 1-3).** In `pointer_fanout/tests/passive_grab_activation.rs`, `p_fixture()` follows the measurements setup:
- client A = `0xA753`, B = `0xA754`;
- W, owned by A, at root (100,100) 300x300;
- D, **owned by B**, child of W at (20,20) 100x100;
- OUT, owned by B, at root (550,100) 300x300.

It is built from the `implicit_grabs.rs` helpers, with an extra `create_window` owner argument where they lack one.

**Request helpers**, each issued through dispatch:
- `grab_button_core(f, client, window, button, modifiers, pointer_mode, owner_events)`;
- `xi_grab_button(f, client, device, window, button, mode, owner_events, mask)`, which is XIPassiveGrabDevice;
- `ungrab_button_core`;
- `allow_events_core(f, client, mode)`;
- `xi_allow_events(f, client, device, mode)`.

**Input helpers:** `press_over_d`, `release` and `move_to_out` as in `implicit_grabs.rs`.

## Review Focus

1. **XIAllDevices grab matched on the slave pass.** An XIPassiveGrabDevice for device 0 (XIAllDevices) matched on the slave pass activates on the slave: the slave is detached and no master form is produced. Test in Task 1 (`xiall_devices_grab_activates_on_slave_pass`).
2. **Ungrab protocol boundaries.** Client A holds a core grab and an XI2 grab on the same window, button and modifiers. XIPassiveUngrabDevice removes only the XI2 grab, and UngrabButton removes only the core grab. Test in Task 1 (`each_ungrab_removes_only_its_protocol`).
3. **A second slave pressing during a slave grab.** While XTest slave 4 holds a detached passive grab, a physical slave's press and release still update the master normally and leave `buttons_down` empty. Test in Task 2 (`other_slave_master_state_survives_slave_grab`).
4. **XI1 implicit grab owner disconnects mid-drag.** The grab is removed, and the release goes to its natural target. Test in Task 4 (`xi1_implicit_grab_owner_disconnect_frees_device`).
5. **Relative XTest device motion past the screen edge.** It clamps the sprite to the screen as a relative core FakeInput motion does. Test in Task 5 (`xtest_relative_device_motion_clamps_to_screen`).

---

### Task 1: Grab selection and request replacement

Spec invariant 1. Cases P4c, P5 and P4b.

**Files:**
- Modify: `crates/yserver-core/src/server.rs`: `find_passive_grab` (~4075-4140).
- Modify: `crates/yserver-core/src/core_loop/process_request/grabs.rs`: `handle_grab_button` replacement (~1186) and `handle_ungrab_button` (~1260).
- Modify: `crates/yserver-core/src/core_loop/pointer_fanout.rs`: `try_match_passive_grab` (~3925) and the activation block's `dynamic_device`/`sync_device` (~1440-1460).
- Test: `crates/yserver-core/src/core_loop/pointer_fanout/tests/passive_grab_activation.rs`.

**Interfaces:**
- Produces:
  - `pub enum PassiveGrabPass { Slave(u16), Master }` in `server.rs`.
  - `ServerState::find_passive_grab(&self, window, button, state_mask, device_id: u16, master_device_id: Option<u16>) -> Option<(PassiveButtonGrab, PassiveGrabPass)>`.
  - The activation block uses `PassiveGrabPass` instead of `state.xi_devices.role(grab.device_id)`: `Slave(id)` stores the grab in `xi2_pointer_grabs[id]` and detaches `id`; `Master` stores it with `set_pointer_grab`. `sync_device` is `id` or `DEVICEID_MASTER_POINTER`. Tasks 2 and 3 consume this.
- Matching, per Xorg `../xserver/dix/grabs.c:439-458` and `checkCore`:
  - **Slave pass:** XI2 grabs whose `device_id` is the slave or 0 (XIAllDevices).
  - **Master pass:** XI2 grabs whose `device_id` is the master, 1 (XIAllMasterDevices) or 0, plus core grabs (`!via_xi2`).
  - The slave pass runs over the whole chain, root first, before the master pass.
  - On one window the last matching entry of `button_grabs` wins. Requests append, so the last entry is the newest. "Specificity" is removed.
  - The first-button rule and the `confine_to` check stay.

- [ ] **Step 1: Write the tests** in `passive_grab_activation.rs`:
  - `p4c_core_requests_keep_xi2_grab`:
    - Setup: A XIGrabButton (master 2, AnyButton, AnyModifier, sync, Press|Release) on D; then A core GrabButton (AnyButton, AnyModifier, sync) on D; then A UngrabButton (AnyButton, AnyModifier) on D. B selects master 2 Press|Release on D.
    - Assert `button_grabs` holds exactly one entry, `{A, D, via_xi2: true, device 2}`.
    - Press over D. Post-press: `active_pointer_grab` = `{A, D, via_xi2, passive}`. A got XI_ButtonPress d2 on D.
    - XIAllowEvents ReplayDevice, then release. B gets press d2 and release d2 on D. End state.
  - `p5_slave_grab_on_descendant_beats_core_grab_on_ancestor`:
    - Setup: A core async GrabButton (AnyButton, AnyModifier) on W. B async XIGrabButton (slave 4, button 1, AnyModifier, Press|Release) on D. B selects master 2 Press|Release on D.
    - Press over D. Post-press: `xi2_pointer_grabs[4]` = `{B, D, passive}`; `active_pointer_grab` is `None`.
    - Release. B gets press d4 and release d4 on D. A gets nothing. B gets no d2. End state.
  - `p4b_older_xi2_newer_core_core_wins` (GUARD): as P4, but the XI2 grab is created first. Post-press: `active_pointer_grab` = `{A, D, via_xi2: false, passive}`, and both registrations exist before the press. Allow ReplayPointer, release, end state.
  - `xiall_devices_grab_activates_on_slave_pass` (Review Focus 1): A async XIGrabButton device 0, button 1, on W. Press over D. Post-press: `xi2_pointer_grabs[4]` = `{A, W, passive}`, slave 4 detached, `active_pointer_grab` is `None`. Release, end state.
  - `each_ungrab_removes_only_its_protocol` (Review Focus 2): A core GrabButton and XIGrabButton (master 2) on W, same button and modifiers. XIPassiveUngrabDevice leaves only the core entry. Recreate the XI2 grab; UngrabButton leaves only the XI2 entry.
- [ ] **Step 2: Run** `cargo test -p yserver-core passive_grab_activation`.
  - Expected: `p4c_...`, `p5_...` and `each_ungrab_...` FAIL on assertions.
  - `xiall_...` FAILs if the base activates XIAllDevices on the master; if it passes on the base, record that in the report and keep it as GUARD.
  - `p4b_...` PASSes.
- [ ] **Step 3: Implement**:
  - the two passes and newest-wins in `find_passive_grab`;
  - the `PassiveGrabPass` plumbing in `try_match_passive_grab` and the activation block;
  - `!g.via_xi2` in the core GrabButton replacement and the UngrabButton removal. The XIPassiveGrabDevice replacement already checks `via_xi2` and `device_id`, so it stays.
  - Callers of `find_passive_grab` (`server.rs` ~4506 and tests) adapt.
- [ ] **Step 4: Run** the task's tests, then the per-task gate. Expected: PASS.
- [ ] **Step 5: Mutations:**

  | Mutation | Must fail |
  |---|---|
  | (a) drop `!g.via_xi2` from the GrabButton replacement | `p4c_...`, `each_ungrab_...` |
  | (b) drop it from UngrabButton | `p4c_...`, `each_ungrab_...` |
  | (c) run the master pass before the slave pass | `p5_...` |
  | (d) first matching entry wins instead of the last | `p4b_...` |
  | (e) match device 0 only on the master pass | `xiall_...` |

- [ ] **Step 6: Commit** `fix(grabs): passive grab selection by pass and newest-wins; core requests keep XI2 grabs (P4c, P5)`.

### Task 2: Activation delivery and the detached slave

Spec invariants 2, 3 and 3a. Cases P1, P1m, P2, P2m and P4.

**Files:**
- Modify: `crates/yserver-core/src/core_loop/pointer_fanout.rs`:
  - the activation block (~1410-1545);
  - the XI2 fanout's activation form for `via_xi2` grabs;
  - `pointer_button_transition` and its call site (~277-300, ~615-647).
- Test: `passive_grab_activation.rs`.

**Interfaces:**
- Consumes: Task 1's `PassiveGrabPass`.
- Produces: no new types. The rules:
  - A `via_xi2` grab's activating press is emitted once as XI2, on the grab window, for the pass's device (d4 for `Slave(4)`, d2 for `Master`). It is never emitted as core and never through the natural walk.
  - A core grab's activating press is emitted once as core.
  - A `Slave(id)` activation emits no master XI2 form and no core form for that event, to any client.
  - A `Slave(id)` activation also makes no change to `buttons_down` or master acceptance. The release of that button, while detached, leaves master state untouched.
  - Whether the master transition is deferred until after the lookup or rolled back is the implementer's choice. Name it in the commit body.

- [ ] **Step 1: Write the tests.** Every test starts from a fresh fixture, with `buttons_down == 0` asserted first.
  - `p1_async_slave_grab_owner_events_no_master_press`:
    - Setup: A async XIGrabButton (slave 4, button 1, AnyModifier, owner_events 1, Motion|Press|Release) on W. B selects slave 4 and master 2 on D.
    - Press over D, Motion to (155,155), release.
    - A gets press d4, Motion d4 and release d4, all on W. B gets nothing.
    - Post-press: `buttons_down == 0`. End state.
  - `p1m_master_xi2_grab_delivers_only_xi2`: as P1 with device 2. A gets exactly one XI_ButtonPress d2 on W and no core ButtonPress, then Motion and release d2 on W. B gets the d4 forms on D. End state.
  - `p2_sync_slave_grab_release_only_mask_gets_press` (GUARD):
    - Setup: A sync XIGrabButton (slave 4, mask Release only, owner_events 0) on W. B selects slave 4 and master 2 on D.
    - Press, XIAllowEvents AsyncDevice (device 4), release.
    - A gets press d4 and release d4 on W. End state.
  - `p2m_master_sync_release_only_mask_single_press`: as P2 with device 2. A gets exactly one XI_ButtonPress d2 (no core) and XI_ButtonRelease d2 on W. B gets d4 press and release on D. End state.
  - `p4_xfwm4_core_then_xi2_grab_single_xi2_press`:
    - Setup: A core sync GrabButton (AnyButton, AnyModifier) on D, then A XIGrabButton (master 2, AnyButton, AnyModifier, sync, Press|Release) on D. B selects master 2 Press|Release on D.
    - Press. Post-press: `active_pointer_grab.via_xi2` is true. A got exactly one packet, XI_ButtonPress d2.
    - XAllowEvents ReplayPointer once, then release. B gets press d2 and release d2 on D. End state.
  - `other_slave_master_state_survives_slave_grab` (Review Focus 3):
    - Setup: register a physical source as in `implicit_grabs.rs::two_slaves_end_their_grabs_separately`. A async XIGrabButton (slave 4) on W.
    - XTest press: no master bit.
    - Physical press and release: `buttons_down` toggles and returns to 0.
    - XTest release. End state.
- [ ] **Step 2: Run.**
  - Expected FAIL: `p1_...` (extra d2 press to B and master bit), `p1m_...`, `p2m_...` and `p4_...` (core plus XI2), `other_slave_...` (master bit).
  - Expected PASS: `p2_...`.
- [ ] **Step 3: Implement** the rules above in the activation block and the transition. Reuse the existing core encoder (`fanout_core_pointer_event_ordered`) and the XI2 form emitter. Delete the `grab.via_xi2 && dynamic_device` special case at ~1503, which the single-form rule replaces.
- [ ] **Step 4: Run** the task's tests and the Task 1 tests, then the gate. Expected: PASS.
- [ ] **Step 5: Mutations:**

  | Mutation | Must fail |
  |---|---|
  | (a) emit the core form for a `via_xi2` master grab again | `p1m_...`, `p2m_...`, `p4_...` |
  | (b) emit the master XI2 form after a `Slave` activation | `p1_...` |
  | (c) apply the master button transition before a `Slave` activation | `p1_...`, `other_slave_...` |
  | (d) gate the activating press on the grab's mask | `p2_...`, `p2m_...` |

- [ ] **Step 6: Commit** `fix(grabs): passive activation delivers one form; a slave activation leaves the master untouched (P1, P1m, P2m, P4)`.

### Task 3: Replay of the frozen device and the C19 guard

Spec invariants 4 and 5. Cases P2r and P3.

**Files:**
- Modify: `crates/yserver-core/src/core_loop/pointer_fanout.rs`: `replay_frozen_pointer_event_to_state` (~417-445) and the inner fanout's replay handling.
- Modify: the replay callers: `process_request/grabs.rs` (~173, ~495) and `process_request/xi/dispatch/grabs.rs` (~1926).
- Modify: the store at activation (`Xi1Freeze::stored`, ~1525). It records the frozen device and the grab window.
- Test: `passive_grab_activation.rs`.

**Interfaces:**
- Produces:
  - `pub struct PointerReplay { pub device: u16, pub grab_window: ResourceId }` in `pointer_fanout.rs`.
  - `replay_frozen_pointer_event_to_state(state, backend, xid_map, event: HostPointerEvent, replay: PointerReplay) -> Vec<ClientId>`.
- Rules:
  - `device == DEVICEID_MASTER_POINTER`: only master XI2 and core forms are produced, and the master pass of passive grabs runs for windows strictly below `grab_window` on the chain (Xorg `ComputeFreezes`, `../xserver/dix/events.c:1331-1363`).
  - Slave `device`: only that slave's XI2 and XI1 forms are produced, with the slave pass below `grab_window`.
  - The implicit grab the replay creates goes on `replay.device` only, through #374's lifecycle.
  - The `slave_only_replay_press` guard is deleted.

- [ ] **Step 1: Write the tests**:
  - `p2r_slave_replay_gives_only_slave_forms`:
    - Setup: as P2, then XIAllowEvents ReplayDevice (device 4) instead of AsyncDevice; release.
    - A gets press d4 on W. B gets press d4 and release d4 on D, and no d2.
    - After the replay: `xi2_pointer_grabs[4]` = `{B, D, implicit}`, `active_pointer_grab` is `None`. End state, with `buttons_down == 0`.
  - `p3_core_replay_does_not_repeat_slave_press` (the C19 gate):
    - Setup: A core sync GrabButton (AnyButton, AnyModifier) on W. B selects slave 4 Press|Motion|Release on D.
    - Press. B already has press d4 on D.
    - XAllowEvents ReplayPointer. B gets no second press d4.
    - Move to OUT, release. B gets Motion d4 and release d4 on D (its slave implicit grab survives). End state.
  - `core_replay_activates_grab_below_grab_window`:
    - Setup: A core sync GrabButton on W. B core async GrabButton on D.
    - Press. A's grab is active. ReplayPointer: B's grab on D activates, and B gets the core press on D.
    - Release. End state.
    - Source: `../xserver/dix/events.c:1331-1363`. This case is not measured; the commit body cites the source.
  - `replay_does_not_reactivate_grab_on_grab_window` (GUARD, by P4): after a ReplayPointer of P4's XI2 grab on D, the core grab on D does not activate.
- [ ] **Step 2: Run.**
  - Expected FAIL: `p2r_...` (extra d2 press and release), `p3_...` (second press d4), `core_replay_activates_...` (`handle_grabs` false on replay).
  - Expected PASS: `replay_does_not_...`.
- [ ] **Step 3: Implement**:
  - store `PointerReplay` at activation;
  - thread it through the callers;
  - limit the inner fanout's forms to `replay.device`;
  - run the pass for that device with the floor `grab_window`;
  - delete the guard.
- [ ] **Step 4: Run** the task's tests, Tasks 1-2, `implicit_grabs` and `owner_events`, then the gate. Expected: PASS.
- [ ] **Step 5: Mutations:**

  | Mutation | Must fail |
  |---|---|
  | (a) produce master forms on a slave replay | `p2r_...` |
  | (b) produce slave forms on a master replay | `p3_...` |
  | (c) skip passive checks on replay | `core_replay_activates_...` |
  | (d) include `grab_window` itself in the floor | `replay_does_not_...` |
  | (e) restore the C19 guard | `p2r_...` (the slave implicit grab is missing) |

- [ ] **Step 6: Commit** `fix(grabs): replay reprocesses only the frozen device below the grab window; drop the C19 guard (P2r, P3)`.

### Task 4: XI1 implicit grabs

Spec invariant 6. Case I5.

**Files:**
- Modify: `crates/yserver-core/src/server.rs`: `Xi1ActiveGrab` (~410). Add `implicit: bool` and `device_mask: u32`. The mask snapshot is the merged XI1 selection of the delivery window for that device.
- Modify: `crates/yserver-core/src/core_loop/pointer_fanout.rs`:
  - `xi1_route_device_event` (~2771-2910): activation after a selection delivery of a DeviceButtonPress, mask filtering in the active branch, and release when the device's buttons are up;
  - the doc comment that cites XTS;
  - the master-activation check at ~2596-2601, which must not treat a slave's XI1 implicit grab as the master's;
  - the slave XI2 routing occupancy (`grab_for_form(Slave)`, `pointer_fanout/owner_events.rs` ~280-292), which must see the XI1 implicit grab.
- Modify: unmap and disconnect teardown of `xi1_active_grabs`, by reusing the existing removers (`xi1_deactivate_device_grab`, ~3389).
- Test: `crates/yserver-core/src/core_loop/pointer_fanout/tests/implicit_grabs.rs`.

**Interfaces:**
- Produces: `Xi1ActiveGrab { .., implicit: bool, device_mask: u32 }`.
  - Explicit and passive XI1 grabs set `implicit: false` and `device_mask: 0`, and keep their behaviour.
  - For an implicit grab, `owner_events` is false and `passive_detail` is `None`.
- Rules:
  - **Activation:** the grab is created only when the device has no entry in `xi1_active_grabs` or `xi2_pointer_grabs` and the press was delivered by selection.
  - **Arbitration with XI2 on the same slave:** the deepest window with a selection takes the press, and on one window XI2 wins over XI1 (`../xserver/dix/events.c:2897-2931`). The XI1 grab is created only when the XI1 form was the one delivered.
  - **Delivery during the grab:**
    - Events are delivered to the owner on the grab window only when `device_mask` has the event's class bit.
    - The release that leaves the device's buttons up ends the grab whether it was delivered or not.
    - XI2 slave forms of that device go to no other client while the grab is active.

- [ ] **Step 1: Write the tests.** Each uses an XI1 selection through XSelectExtensionEvent dispatch.
  - `i5_xi1_drag_and_release_stay_with_press_client`:
    - Setup: A selects DeviceMotionNotify, DeviceButtonPress and DeviceButtonRelease for device 4 on W; B selects the same on OUT.
    - Press over D. Post-press: `xi1_active_grabs[4]` = `{A, W, implicit, owner_events: false}`, `active_pointer_grab` is `None`, the slave is still attached.
    - Move to OUT, release, final motion.
    - A gets press, Motion and release on W. B gets only the final motion on OUT. End state.
  - `xi1_implicit_grab_filters_by_mask_snapshot`: A selects Motion and Press only (C10's selection) on W. Press, move to OUT, release. A gets no release. `xi1_active_grabs` is empty after the release. B (Release on OUT) gets nothing during the grab. End state.
  - `xi2_slave_selection_on_deeper_window_beats_xi1`:
    - Setup: B selects XI2 slave 4 Press|Motion|Release on D; A selects XI1 device 4 on W.
    - Press. Post-press: `xi2_pointer_grabs[4]` = `{B, D, implicit}`, no XI1 entry. A gets nothing.
    - Release, end state.
  - `xi1_on_deeper_window_beats_xi2_on_ancestor`:
    - Setup: B owns a window E, child of W, at D's place in a variant fixture. B selects XI1 device 4 on E; A selects XI2 slave 4 on W.
    - Press. Post-press: `xi1_active_grabs[4]` = `{B, E, implicit}`. A gets no d4 press, and the drag d4 goes to no one but B.
    - End state.
  - `xi1_slave_grab_and_master_grab_coexist`:
    - Setup: A selects XI1 device 4 on W; B selects core ButtonPress|ButtonRelease|PointerMotion on W.
    - Press. Post-press: `xi1_active_grabs[4]` = `{A, W, implicit}` **and** `active_pointer_grab` = `{B, W, implicit}`.
    - Move to OUT, release. A gets the XI1 release on W; B gets the core release on W. End state.
  - `xi1_implicit_grab_ends_on_unmap`: as I5; A unmaps W mid-drag. The XI1 entry is removed, and the release goes to B on OUT.
  - `xi1_implicit_grab_owner_disconnect_frees_device` (Review Focus 4): as I5; A disconnects mid-drag. The entry is removed, and the release goes to B on OUT.
  - Existing expectations of "no XI1 implicit grab" change under I5 with a row comment.
- [ ] **Step 2: Run.**
  - Expected FAIL on assertions: `i5_...`, `..._filters_...`, `xi1_on_deeper_...`, `..._coexist`, `..._unmap`, `..._disconnect`.
  - `xi2_slave_selection_on_deeper_...` may pass on the base; if so, mark it GUARD in the report.
- [ ] **Step 3: Implement** the rules. Reuse `xi1_fan_device_event`, `compute_xi1_route_targets` and the existing release deactivation; no new table.
- [ ] **Step 4: Run** the task's tests, `owner_events`, the `xi1` modules and Tasks 1-3, then the gate. Expected: PASS.
- [ ] **Step 5: Mutations:**

  | Mutation | Must fail |
  |---|---|
  | (a) no activation | `i5_...`, `..._coexist`, `..._unmap` |
  | (b) ignore `device_mask` | `..._filters_...` |
  | (c) end the grab only on a delivered release | `..._filters_...` |
  | (d) master check counts the slave's XI1 entry again | `..._coexist` |
  | (e) slave XI2 occupancy ignores `xi1_active_grabs` | `xi1_on_deeper_...` |

- [ ] **Step 6: Commit** `fix(xi1): XI1 deliveries create per-device implicit grabs with a mask snapshot (I5)`. The body cites `a511c445d` and XTS `XSelectExtensionEvent` assertion 5.

### Task 5: XTest DeviceMotionNotify moves the sprite

Spec invariant 7. Cases X1, X2 and X3; C16, C17 and C18 must still match.

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request/xtest.rs`: the DeviceMotion branch (~505-560). It now builds the motion like the core MotionNotify branch (~600-635) and calls `backend.on_host_input`, then `resync_input_position`. Its wrong "does NOT move the sprite" comment goes.
- Modify: `crates/yserver-core/src/core_loop/message.rs` `HostInputEvent::PointerMotion` and `crates/yserver-core/src/host_x11/pump.rs` `HostPointerEvent`: add `device_axes: Option<crate::server::Xi1MotionAxes>`. Every other constructor passes `None`.
- Modify: `pointer_fanout.rs`, the XI1 event built at ~1863. It passes `event.device_axes`. The XI2 slave and master motion forms use the explicit valuators when `device_axes` is `Some`.
- Modify: `pointer_fanout/owner_events.rs`. The valuator-copy logic of `xi2_direct_xtest_motion` (~202-225) becomes `fn xtest_wire_valuators(axes: Xi1MotionAxes) -> (u8, Vec<i32>)`, shared by the general XI2 encoder. `xi2_direct_xtest_motion` and its call site (~2943-2955) are deleted, so delivery happens once.
- Test: `crates/yserver-core/src/core_loop/pointer_fanout/tests/owner_events.rs`, next to `xtest_device_motion_preserves_partial_valuator_range` (~1077).

**Interfaces:**
- Produces:
  - `HostPointerEvent::device_axes` and `HostInputEvent::PointerMotion::device_axes`;
  - `fn xtest_wire_valuators(axes: crate::server::Xi1MotionAxes) -> (u8, Vec<i32>)`.
- Position rules:
  - detail 0 (absolute): each axis 0/1 present in the Xorg-copied valuators replaces that coordinate, and a missing axis keeps the current one. X3, `first=1` with value 175, gives (x, 0).
  - detail 1 (relative): the current position plus the copied values.
  - The backend clamps to the screen as for core fakes.

- [ ] **Step 1: Write the tests.** Each sends XTestFakeInput through dispatch, with the `fake_device_motion` helper extended with a relative flag.
  - Shared setup for the X tests: A selects XI2 master Motion and core PointerMotion on W; B selects the same on OUT; the pointer is at (150,150).
  - `x1_absolute_device_motion_moves_sprite`: device 4, absolute, axes (650,150). `state.pointer_root == (650,150)`. B gets XI_Motion d2/s4 on OUT; A gets nothing. End state.
  - `x2_relative_device_motion_moves_sprite`: relative, (+500, 0). Same assertions as X1.
  - `x3_one_axis_device_motion_follows_xorg_copy`: absolute, `first=1`, value 175. `state.pointer_root == (150,0)`. No packet to A or B.
  - `xtest_relative_device_motion_clamps_to_screen` (Review Focus 5): relative (+5000, 0). `pointer_root.0` is screen width − 1, as for a relative core FakeInput motion of the same delta in the same test.
  - The existing `xtest_device_motion_owner_events_and_exclusive_grabs_route_xi2` and `xtest_device_motion_preserves_partial_valuator_range` stay unchanged and green. They are C16-C18: mask 0x2 and axis value 0 are preserved, and each packet is delivered once.
- [ ] **Step 2: Run.** Expected: the four new tests FAIL, with the sprite unmoved. The C16-C18 tests PASS.
- [ ] **Step 3: Implement** as listed in Files.
- [ ] **Step 4: Run** the task's tests, `owner_events`, `xtest` and `implicit_grabs`, then the gate. Expected: PASS.
- [ ] **Step 5: Mutations:**

  | Mutation | Must fail |
  |---|---|
  | (a) ignore detail (always absolute) | `x2_...`, `..._clamps_...` |
  | (b) use the request value for X3 instead of Xorg's copy | `x3_...` |
  | (c) drop `device_axes` in the XI2 encoder | `xtest_device_motion_preserves_partial_valuator_range` |
  | (d) keep `xi2_direct_xtest_motion` as well | `xtest_device_motion_owner_events_and_exclusive_grabs_route_xi2` (duplicate packet) |

- [ ] **Step 6: Commit** `fix(xtest): DeviceMotionNotify moves the sprite through the pointer motion path (C18, X1-X3)`.

### Task 6: Oracle, docs and status (coordinator)

The coordinator does this task, not codex.

- [ ] Build the branch in release mode. Run `passive-grabs/oracle/run-fresh.sh`, `passive-grabs/xtest/run.sh`, the I1-I8 and C1-C20 runs, with one fresh server per P, I5 and X case.
  - Expected: P1-P5, P4b, P4c, I5 and X1-X3 match Xvfb, per the comparison rules of the measurements doc.
  - Expected: I1-I8 and C1-C20 are unchanged, with C15 excluded as in #374.
- [ ] Append a "Branch results" section to the measurements doc. Add a `docs/status.md` entry like #374's.
- [ ] Run the full gate:
  - `cargo test --workspace --features xdmcp`;
  - the lavapipe `--ignored` suite;
  - `just man`;
  - rule F, five runs under load.
- [ ] Hardware, after asking the user: xfwm4 click-to-focus (one XI2 press and one ReplayPointer, by x11trace), marco, and the #374 scenes, on NVIDIA and AMD.
- [ ] PR body (on the user's word): states that XTS `XSelectExtensionEvent` assertion 5 checks 5/6 now fail as on Xorg, citing `a511c445d`.
- [ ] Commit `docs: passive-grab activation branch results`.
