# Passive Grab Activation, XI1 Implicit Grabs and XTest Device Motion Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:**
- Passive button grabs select, activate and replay as in Xorg 21.1.24: P1-P5, P4b, P4c, the C19 gate and xfwm4's double press.
- XI1 deliveries create per-device implicit grabs (I5).
- XTest DeviceMotionNotify moves the sprite (X1-X3).

**Architecture:**
- **Activation (Task 1).** The activation block in `pointer_fanout.rs` delivers one form. A slave activation skips master processing, including the master button transition.
- **Selection (Task 2).** `ServerState::find_passive_grab` becomes a slave pass over the whole chain, then a master pass. It picks the newest grab per window and returns the pass that matched.
- **Replay (Task 3).** Replay takes the frozen device and grab window from the AllowEvents request paths, so it reprocesses only that device's forms, with passive checks below the grab window. The C19 guard goes.
- **XI1 (Task 4).** XI1 implicit grabs reuse `xi1_active_grabs` with a mask snapshot.
- **XTest (Task 5).** XTest device motion reuses the core FakeInput motion path through the backend. The request's axes ride on the motion event across every bridge.

**Tech Stack:** Rust (yserver-core, yserver KMS backend tests). Oracles: `passive-oracle`, `implicit-oracle` and `xtest-oracle` against Xvfb 21.1.24.

**Spec:** `docs/superpowers/specs/2026-11-21-passive-grab-activation-design.md` (revision 5, approved by the user 2026-11-21). Measurements: `docs/superpowers/findings/2026-11-21-passive-grab-activation-measurements.md`.

## Global Constraints

- **Base:** branch `feat/passive-grab-activation`, on PR #374 `af690cdf` plus the docs commits. Each task's red run is against that task's base, the commit before its first commit. The pointer code there equals the spec's `d479e443`.
- **Xorg citations:** cite Xorg as `../xserver` (tag xorg-server-21.1.24). Read or use no copyleft code.
- **Reuse first:** new code only where an existing helper cannot do it, and justify each new function in the commit body. No new grab table, no per-protocol flag machine, no second activation, release or XTest-motion path. The spec's anti-goals bind.
- **Environment variables:** no new `YSERVER_*` environment variables.
- **Out of scope (spec Non-goals):**
  - XI1 passive grabs (GrabDeviceButton) and key grabs.
  - The XI2 passive grab's stored `0xFFFF_FFFF` mask (`process_request/xi/dispatch/grabs.rs` ~849).
  - DontPropagate, crossings and touch.
  - The DeviceButtonPressGrab and DeviceOwnerGrabButton classes.
  - Coordinate fields are compared only where the spec requires them: activation child/coordinates per `FixUpEventFromWindow` (spec invariant 2), and sprite position and child for X1-X3.
- **Existing tests:** they change expected values only where a P, I5 or X row of the measurements doc, or a cited `../xserver` line, gives another answer. Each change names its row in a comment and in the commit body.
- **Tests, rules A/B/C:**
  - Windows are created and mapped through `process_request` dispatch, never `state.resources.create_window`. Selections (XISelectEvents, XSelectExtensionEvent, ChangeWindowAttributes), grabs, ungrabs, AllowEvents/XIAllowEvents, XTestFakeInput, UnmapWindow and client disconnect also go through their production entries.
  - Pointer input enters through `pointer_event_fanout_to_state` with origin `InputOrigin::XTest(4)`, or through a registered physical source as in `implicit_grabs.rs`.
  - Task 5's input enters through `process_request` XTestFakeInput on `KmsBackend::for_tests()`, so the production backend moves the sprite and runs the fanout.
  - Assertions read the encoded packets the clients receive.
  - **End state:** every scenario ends with an end-state check. `active_pointer_grab`, `xi2_pointer_grabs`, `xi1_active_grabs`, `xi1_frozen` and `xi2_detached_masters` must equal the expected live set. `buttons_down` and every per-source held-button set must be empty, unless the test deliberately holds an unrelated button, which must then still be held (spec invariant 3a).
  - **Post-press check:** right after the press, before any AllowEvents or release, each scenario asserts the active grab: owner, window, device slot, `via_xi2`, and `passive` or `implicit`. It also asserts `buttons_down`.
- **Red runs and mutations:**
  - Every new test is seen failing on its task's base for its stated reason, on an assertion and not a compile error.
  - Observation-only test support goes in before the red run.
  - Tests marked GUARD must pass on the base.
  - Every production change has a named mutation, listed with the tests it must turn red. Mutations are verified one at a time, by line, and reverted.
- **Shared fixture:** in `pointer_fanout/tests/implicit_grabs.rs`, `Fixture` and `Packet` become `pub(super)` with `pub(super)` fields. The helpers `dispatch`, `create_window`, `map_window`, `select_xi2`, `select_core`, `send`, `press_over_d`, `move_to_out`, `release`, `final_motion`, `read_packets`, `xi`, `core`, `assert_end_state` and `assert_end_state_with_xi1_entries` also become `pub(super)`. Other test modules import them with `use super::implicit_grabs::{...}`. Nothing is copied.
- **Commits:** signed (`-S`), with explicit paths, and the trailers `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>` and `Implemented-By: codex (gpt-6-luna xhigh)`.
- **No server runs:** implementers run no hardware and no yserver binary. The coordinator runs the oracles (Task 6).
- **Per-task gate:**
  - `cargo +nightly fmt` and `cargo clippy --workspace --all-targets -- -D warnings`.
  - `cargo test -p yserver-core pointer_fanout`, `... grab`, `... xi2_grabs`, `... xi1` and `... xtest`.
  - `cargo test -p yserver kms::render::backend::tests`.
  - `tools/check-layout.sh`.
  - Rule F: `pointer_fanout` five consecutive times under `nproc` busy loops.

## Fixture

- **`p_fixture()`** lives in `pointer_fanout/tests/passive_grab_activation.rs`. It builds the measurement geometry with the shared helpers:
  - client A `0xA753`, client B `0xA754`;
  - W, owned by A, at root (100,100), 300x300;
  - D, **owned by B**, a child of W at (20,20), 100x100;
  - OUT, owned by B, at root (550,100), 300x300.
- **Request helpers** in the same module, each through dispatch:
  - `grab_button_core(f, client, window, button, modifiers, pointer_mode, owner_events)`;
  - `ungrab_button_core(f, client, window, button, modifiers)`;
  - `xi_grab_button(f, client, device, window, button, mode, owner_events, mask)` (XIPassiveGrabDevice);
  - `xi_ungrab_button(f, client, device, window, button)`;
  - `allow_events_core(f, client, mode)`;
  - `xi_allow_events(f, client, device, mode)`.

## Review Focus

1. **XIAllDevices grab on the slave pass.** An XIPassiveGrabDevice for device 0 that matches on the slave pass activates on the slave: the slave is detached and no master form is produced. Test in Task 2.
2. **Ungrab protocol boundaries.** A client holds a core grab and an XI2 grab on the same window, button and modifiers. XIPassiveUngrabDevice removes only the XI2 grab, and UngrabButton only the core grab. Test in Task 2.
3. **Another source's held master button.** A physical slave holds button 3 before XTest slave 4 activates a slave passive grab. Its master bit survives the activation and the slave release, and clears only on its own release. Test in Task 1.
4. **XI1 implicit grab owner disconnect.** The owner of an XI1 implicit grab disconnects mid-drag. The grab is removed, and the release goes to its natural target. Test in Task 4.
5. **Relative XTest device motion past the screen edge.** It clamps like a relative core FakeInput motion and keeps Y. Test in Task 5.

---

### Task 1: Activation delivery and the detached slave

Spec invariants 2, 3 and 3a. Cases P1, P1m, P2, P2m and P4. Each case's winning grab is already the one the current selection picks; the only selection change here is the first-button rule becoming per device (sol plan r2 F1).

**Files:**
- Modify: `crates/yserver-core/src/core_loop/pointer_fanout.rs`:
  - the activation block (~1410-1545);
  - the XI2 form emission for `via_xi2` grabs;
  - `pointer_button_transition` and its call site (~277-300, ~615-647).
- Modify: `crates/yserver-core/src/server.rs` `find_passive_grab` (~4083-4087): the first-button rule follows the device of the pass that a candidate belongs to (`../xserver/dix/events.c:4200`, `../xserver/mi/mieq.c:502-508`): slave candidates (XI2 grabs on the slave or XIAllDevices matched for it) check that source's own held buttons (the set `pointer_button_transition` maintains); master and core candidates keep checking the master's aggregate `buttons_down`. Task 2 keeps the two predicates, one per pass (sol plan r3).
- Modify: `crates/yserver-core/src/core_loop/pointer_fanout/tests/implicit_grabs.rs` (visibility only).
- Test: `crates/yserver-core/src/core_loop/pointer_fanout/tests/passive_grab_activation.rs` (the fixture and the request helpers above).

**Interfaces:**
- **Produces:** the activation rules. Task 2 reuses them.
  - A `via_xi2` grab's activating press is emitted once, in XI2 form, on the grab window. Its device is the grabbed slave when the slave is detached by the activation, otherwise the master. It never goes out as core or through the natural walk.
  - A core grab's activating press is emitted once, as core.
  - Child and coordinates follow `FixUpEventFromWindow` (event window = the grab window, child toward the sprite), as the core branch already does.
  - A slave activation emits no master XI2 form and no core form for that event to any client.
  - A slave activation makes no master button transition and no master acceptance. The release of that button while detached leaves master state untouched, including other sources' held bits.
  - The implementer chooses between deferring the transition until after the lookup and rolling it back, and names the choice in the commit body.

- [ ] **Step 1: Write the tests.** Each starts from a fresh `p_fixture()` with `buttons_down == 0` asserted first.
  - `p1_async_slave_grab_owner_events_no_master_press`:
    - A async XIGrabButton (slave 4, button 1, AnyModifier, owner_events 1, Motion|Press|Release) on W. B selects slave 4 and master 2 Motion|Press|Release on D.
    - Press over D. Post-press: `xi2_pointer_grabs[4]` = `{A, W, passive}`, slave 4 detached, `buttons_down == 0`.
    - Motion to (155,155), then release.
    - A gets press d4, Motion d4 and release d4, all on W. B gets nothing. End state.
  - `p1m_master_xi2_grab_delivers_only_xi2`: P1 with device 2.
    - A's first packet is exactly one XI_ButtonPress d2/s4 on W with child D and event coordinates (50,50). No core ButtonPress.
    - Then Motion and release d2 on W. B gets the d4 forms on D. End state.
  - `p2_sync_slave_grab_release_only_mask_gets_press`:
    - A sync XIGrabButton (slave 4, mask Release only, owner_events 0) on W. B selects slave 4 and master 2 Press|Release on D.
    - Press, XIAllowEvents AsyncDevice (device 4), release.
    - A gets press d4 and release d4 on W. B gets nothing. End state.
  - `p2m_master_sync_release_only_mask_single_press`: P2 with device 2. A gets exactly one XI_ButtonPress d2 (no core) and XI_ButtonRelease d2 on W. B gets press and release d4 on D. End state.
  - `p4_xfwm4_core_then_xi2_grab_single_xi2_press`:
    - A core sync GrabButton (AnyButton, AnyModifier) on D, then A XIGrabButton (master 2, AnyButton, AnyModifier, sync, Press|Release) on D. B selects master 2 Press|Release on D.
    - Press. Post-press: `active_pointer_grab.via_xi2` is true, and A has exactly one packet, XI_ButtonPress d2.
    - XAllowEvents ReplayPointer once, release. B gets press d2 and release d2 on D. End state.
  - `held_button_blocks_core_grab_on_master_pass`: register a physical source; A core async GrabButton (button 1, AnyModifier) on W; nobody selects button 3. Physical press of button 3; XTest press of button 1 over D: `active_pointer_grab` is `None` (the master already has button 3 down). Releases; end state. GUARD on the base.
  - `held_master_button_survives_slave_grab` (Review Focus 3):
    - Register a physical source as in `implicit_grabs.rs::two_slaves_end_their_grabs_separately`. A async XIGrabButton (slave 4, button 1) on W.
    - Physical press of button 3: master bit 3 is set.
    - XTest press of button 1 over D: `buttons_down` == button-3 bit only.
    - XTest release: still button-3 bit only.
    - Physical release of button 3. End state.
- [ ] **Step 2: Run** `cargo test -p yserver-core passive_grab_activation`. Expected FAIL:
  - `p1_...`: the extra d2 press to B, and the master bit.
  - `p1m_...`, `p2m_...` and `p4_...`: core plus XI2.
  - `p2_...`: the packets match the measurement, but the end state fails with the master bit left held.
  - `held_master_...`: the master-wide first-button rule refuses the activation (and the button-1 bit).
- [ ] **Step 3: Implement** the rules.
  - Reuse `fanout_core_pointer_event_ordered` and the existing XI2 form emitter.
  - Delete the `grab.via_xi2 && dynamic_device` special case (~1503), which the single-form rule replaces.
- [ ] **Step 4: Run** the task's tests, then the per-task gate. Expected: PASS.
- [ ] **Step 5: Mutations:**

  | Mutation | Must fail |
  |---|---|
  | (a) emit the core form again for a `via_xi2` master grab | `p1m_...`, `p2m_...`, `p4_...` |
  | (b) emit the master XI2 form after a slave activation | `p1_...` |
  | (c) apply the master button transition before a slave activation | `p1_...`, `p2_...`, `held_master_...` |
  | (d) clear all of `buttons_down` instead of the slave's bit on rollback/deferral | `held_master_...` |
  | (e) the first-button rule reads the master's `buttons_down` for slave candidates again | `held_master_...` |
  | (f) master/core candidates read the source's held set | `held_button_blocks_core_grab_...` |

  The grab-mask independence of activation (P2, P2m) cannot be mutated through requests, because XIPassiveGrabDevice stores `0xFFFF_FFFF` (out of scope). The P2 and P2m tests pin the delivered press. No mutation is claimed for it.
- [ ] **Step 6: Commit** `fix(grabs): passive activation delivers one form; a slave activation leaves the master untouched (P1, P1m, P2, P2m, P4)`.

### Task 2: Grab selection and request replacement

Spec invariant 1. Cases P4b, P4c and P5; P4 (Task 1) guards newest-wins.

**Files:**
- Modify: `crates/yserver-core/src/server.rs`: `find_passive_grab` (~4075-4140) and its caller (~4506).
- Modify: `crates/yserver-core/src/core_loop/process_request/grabs.rs`: the `handle_grab_button` replacement (~1186) and `handle_ungrab_button` (~1260).
- Modify: `crates/yserver-core/src/core_loop/pointer_fanout.rs`: `try_match_passive_grab` (~3925) and the activation block's `dynamic_device`/`sync_device` (~1440-1460).
- Test: `passive_grab_activation.rs`.

**Interfaces:**
- **Consumes:** Task 1's activation rules.
- **Produces:**
  - `pub enum PassiveGrabPass { Slave(u16), Master }` in `server.rs`.
  - `ServerState::find_passive_grab(&self, window, button, state_mask, device_id: u16, master_device_id: Option<u16>) -> Option<(PassiveButtonGrab, PassiveGrabPass)>`.
- **Activation by pass.** The activation block uses the pass instead of `xi_devices.role(grab.device_id)`:
  - `Slave(id)` stores the grab in `xi2_pointer_grabs[id]` and detaches `id`.
  - `Master` uses `set_pointer_grab`.
  - `sync_device` is `id`, or `DEVICEID_MASTER_POINTER` for the master pass. Task 3 consumes it.
- **Matching** (`../xserver/dix/grabs.c:439-458`, `checkCore`):
  - Slave pass: XI2 grabs (`via_xi2`) whose `device_id` is the slave or 0 (XIAllDevices).
  - Master pass: XI2 grabs whose `device_id` is the master, 1 (XIAllMasterDevices) or 0, plus core grabs (`!via_xi2`).
  - The slave pass runs over the whole chain, root first, before the master pass.
  - On one window, the last matching `button_grabs` entry wins: requests append, so the last entry is the newest. The "specificity" ranking is removed.
  - The per-device first-button rule (Task 1) and the `confine_to` check stay.
- **Requests.**
  - The core GrabButton replacement and the UngrabButton removal add `!g.via_xi2`.
  - The XIPassiveGrabDevice replacement already matches `via_xi2` and `device_id`, and stays.

- [ ] **Step 1: Write the tests**:
  - `p4c_core_requests_keep_xi2_grab`:
    - A XIGrabButton (master 2, AnyButton, AnyModifier, sync, Press|Release) on D. Then A core GrabButton (AnyButton, AnyModifier, sync) on D.
    - Assert `button_grabs` has both entries.
    - A UngrabButton (AnyButton, AnyModifier) on D. Assert exactly the XI2 entry remains.
    - B selects master 2 Press|Release on D. Press over D. Post-press: `active_pointer_grab` = `{A, D, via_xi2, passive}`, and A got one XI_ButtonPress d2 on D.
    - XIAllowEvents ReplayDevice, release. B gets press d2 and release d2 on D. End state.
  - `p4b_older_xi2_newer_core_core_wins`:
    - P4 with the XI2 grab created first. Both registrations exist before the press.
    - Post-press: `active_pointer_grab` = `{A, D, via_xi2: false, passive}`, and A has exactly one core ButtonPress.
    - ReplayPointer, release, end state.
  - `p5_slave_grab_on_descendant_beats_core_grab_on_ancestor`:
    - A core async GrabButton (AnyButton, AnyModifier) on W. B async XIGrabButton (slave 4, button 1, AnyModifier, Press|Release) on D. B selects master 2 Press|Release on D.
    - Press. Post-press: `xi2_pointer_grabs[4]` = `{B, D, passive}`, `active_pointer_grab` is `None`, `buttons_down == 0`.
    - Release. B gets press d4 and release d4 on D and no d2. A gets nothing. End state.
  - `xiall_devices_grab_activates_on_slave_pass` (Review Focus 1):
    - A async XIGrabButton device 0, button 1, on W.
    - Press. Post-press: `xi2_pointer_grabs[4]` = `{A, W, passive}`, slave 4 detached, `active_pointer_grab` is `None`.
    - A gets one XI_ButtonPress d4/s4 on W. Release, end state.
  - `each_ungrab_removes_only_its_protocol` (Review Focus 2):
    - A core GrabButton and then XIGrabButton (master 2) on W, same button and modifiers.
    - XIPassiveUngrabDevice: only the core entry remains.
    - Recreate the XI2 grab. UngrabButton: only the XI2 entry remains.
- [ ] **Step 2: Run.** Expected FAIL on assertions:
  - `p4c_...`, `p4b_...`: the coexistence assertion;
  - `p5_...`;
  - `each_ungrab_...`.
  - `xiall_...` FAILs if the base activates XIAllDevices on the master. If it passes, record that and keep it as GUARD.
- [ ] **Step 3: Implement** the passes, newest-wins, the pass plumbing and the two `!g.via_xi2` checks.
- [ ] **Step 4: Run** the task's tests and Task 1's (P4 included), then the gate. Expected: PASS.
- [ ] **Step 5: Mutations:**

  | Mutation | Must fail |
  |---|---|
  | (a) drop `!g.via_xi2` from the GrabButton replacement | `p4c_...`, `p4b_...` |
  | (b) drop it from UngrabButton | `p4c_...`, `each_ungrab_...` |
  | (c) master pass before slave pass | `p5_...` |
  | (d) first matching entry wins | `p4b_...`, and Task 1's `p4_...` |
  | (e) device 0 only on the master pass | `xiall_...` |

- [ ] **Step 6: Commit** `fix(grabs): passive grab selection by pass and newest-wins; core requests keep XI2 grabs (P4b, P4c, P5)`.

### Task 3: Replay of the frozen device and the C19 guard

Spec invariants 4 and 5. Cases P2r and P3.

**Files:**
- Modify: `crates/yserver-core/src/core_loop/pointer_fanout.rs`: `replay_frozen_pointer_event_to_state` (~417-445) and the inner fanout's replay handling.
- Modify: the replay request paths. Each captures `PointerReplay` from the grab it tears down, **before** teardown:
  - core AllowEvents, `process_request/grabs.rs` ~158-178;
  - the second core replay caller, ~495;
  - XIAllowEvents, `process_request/xi/dispatch/grabs.rs` ~1893-1930.
  - No new storage. `Xi1Freeze::stored` stays `Option<QueuedInputEvent>`.
- Modify: the direct test callers `pointer_fanout/tests.rs` ~1770 and ~2535. Each passes the `PointerReplay` its scenario's grab implies.
- Test: `passive_grab_activation.rs`.

**Interfaces:**
- **Produces:**
  - `pub struct PointerReplay { pub device: u16, pub grab_window: ResourceId }` in `pointer_fanout.rs`.
  - `replay_frozen_pointer_event_to_state(state, backend, xid_map, event: HostPointerEvent, replay: PointerReplay) -> Vec<ClientId>`.
  - In the request paths, `device` is the frozen device (`device_id`/`dev`, the master for core AllowEvents), and `grab_window` is the grab's window read before deactivation.
- **Rules:**
  - When `replay.device` is the master, only master XI2 and core forms are produced. The master pass of passive grabs runs for the chain windows strictly below `grab_window` (`../xserver/dix/events.c:1331-1363`).
  - When `replay.device` is a slave, only that slave's XI2 and XI1 forms are produced, with the slave pass below `grab_window`.
  - The implicit grab created by the replay goes on `replay.device` only, through #374's lifecycle.
  - The `slave_only_replay_press` guard is deleted.

- [ ] **Step 1: Write the tests**:
  - `p2r_slave_replay_gives_only_slave_forms`:
    - P2, with XIAllowEvents ReplayDevice (device 4).
    - After the replay: `xi2_pointer_grabs[4]` = `{B, D, implicit}`, `active_pointer_grab` is `None`, `buttons_down == 0`.
    - Release. A got press d4 on W. B gets press d4 and release d4 on D, and no d2. End state.
  - `p3_core_replay_does_not_repeat_slave_press` (C19):
    - A core sync GrabButton (AnyButton, AnyModifier) on W. B selects slave 4 Press|Motion|Release on D.
    - Press. B already has press d4 on D, and `xi2_pointer_grabs[4]` = `{B, D, implicit}`.
    - XAllowEvents ReplayPointer: B gets no second press.
    - Move to OUT, release. B gets Motion d4 and release d4 on D. End state.
  - `core_replay_activates_grab_below_grab_window`:
    - A core sync GrabButton on W. B core async GrabButton on D.
    - Press: A's grab is active.
    - ReplayPointer: `active_pointer_grab` = `{B, D, passive}`, and B gets one core press on D.
    - Release, end state. The commit body cites `../xserver/dix/events.c:1331-1363`, since this case is unmeasured.
  - `replay_does_not_reactivate_grab_on_grab_window` (GUARD, by P4): after a ReplayPointer of P4's XI2 grab on D, the core grab on D does not activate, and B gets the press.
- [ ] **Step 2: Run.** Expected:
  - `p2r_...` FAILs on the extra d2 forms and the missing slave implicit grab.
  - `p3_...` FAILs on the second press.
  - `core_replay_...` FAILs because the base passes `handle_grabs = false`.
  - `replay_does_not_...` PASSes.
- [ ] **Step 3: Implement** the capture in each request path, the argument, the per-device forms, the below-floor pass and the guard deletion.
- [ ] **Step 4: Run** the task's tests, Tasks 1-2, and the `implicit_grabs` and `owner_events` modules, then the gate. Expected: PASS.
- [ ] **Step 5: Mutations:**

  | Mutation | Must fail |
  |---|---|
  | (a) master forms on a slave replay | `p2r_...` |
  | (b) slave forms on a master replay | `p3_...` |
  | (c) skip passive checks on replay | `core_replay_...` |
  | (d) include `grab_window` itself in the floor | `replay_does_not_...` |
  | (e) restore the C19 guard | `p2r_...`, because the slave implicit grab is missing |

- [ ] **Step 6: Commit** `fix(grabs): replay reprocesses only the frozen device below the grab window; drop the C19 guard (P2r, P3)`.

### Task 4: XI1 implicit grabs

Spec invariant 6. Case I5.

**Files:**
- Modify: `crates/yserver-core/src/server.rs`: `Xi1ActiveGrab` (~410) gains `implicit: bool` and `device_mask: u32`.
- Modify: `crates/yserver-core/src/core_loop/pointer_fanout.rs`:
  - `xi1_route_device_event` (~2771-2910): activation after a selection delivery of a DeviceButtonPress; filtering by `device_mask` in the active branch; release when the device's buttons are up. Its XTS doc comment is replaced by the Xorg citation.
  - The master-activation check (~2596-2601): it no longer treats a slave's XI1 implicit grab as the master's.
- Modify: `crates/yserver-core/src/core_loop/pointer_fanout/owner_events.rs`: `grab_for_form(Slave)` (~280-292) also sees a slave's XI1 implicit grab.
- Modify: `crates/yserver-core/src/core_loop/process_request/xi/dispatch/grabs.rs`: XIGrabDevice's slave occupancy (~181) also checks `xi1_active_grabs`, returning AlreadyGrabbed whatever the owner (`../xserver/dix/events.c:5288`).
- Modify: unmap and disconnect teardown, reusing `xi1_deactivate_device_grab` (~3389) and the existing client-gone cleanup.
- Test: `crates/yserver-core/src/core_loop/pointer_fanout/tests/implicit_grabs.rs`.

**Interfaces:**
- **Produces:** `Xi1ActiveGrab { .., implicit: bool, device_mask: u32 }`.
  - Explicit and passive XI1 grabs set `implicit: false` and `device_mask: 0`, and keep their behaviour.
  - An implicit grab has `owner_events: false` and `passive_detail: None`.
  - `device_mask` is the union over all clients of the delivery window's XI1 classes for that device, taken at activation (`../xserver/dix/events.c:2185-2187`). Later selection changes do not alter it.
- **Rules:**
  - The grab is created only when the device has no entry in `xi1_active_grabs` or `xi2_pointer_grabs`, and the press was delivered by selection.
  - **Arbitration on one slave:** the deepest window with a selection takes the press, and on one window XI2 beats XI1 (`../xserver/dix/events.c:2897-2931`). The XI1 grab is created only when the XI1 form was the one delivered.
  - **During the grab:**
    - The owner receives an event on the grab window only when `device_mask` has its class bit.
    - The release that leaves the device's buttons up ends the grab, whether or not it was delivered.
    - XI2 slave forms of that device reach no other client.

- [ ] **Step 1: Write the tests.** XI1 selections go through XSelectExtensionEvent dispatch.
  - `i5_xi1_drag_and_release_stay_with_press_client`:
    - A selects DeviceMotionNotify, DeviceButtonPress and DeviceButtonRelease for device 4 on W. B selects the same on OUT.
    - Press over D. Post-press: `xi1_active_grabs[4]` = `{A, W, implicit, owner_events: false}`, `active_pointer_grab` is `None`, slave 4 attached.
    - Move to OUT, release, final motion.
    - A gets press, Motion and release on W. B gets only the final motion on OUT. End state.
  - `xi1_implicit_grab_mask_is_merged_snapshot`:
    - A selects Motion|Press for device 4 on W, B selects Release for device 4 on W, and B selects Release on OUT.
    - Press. Post-press: `device_mask` has the Motion, Press and Release class bits.
    - A changes its selection on W to Press only.
    - Move to OUT: A still gets Motion on W.
    - Release: A gets the release on W, through the merged snapshot. B gets nothing on OUT. End state.
  - `xi1_unselected_release_still_ends_grab`: A selects Motion|Press only (C10's selection) on W, and nobody else selects Release. Press, move to OUT, release. A gets no release, and `xi1_active_grabs` is empty afterwards. End state.
  - `xi1_grab_holds_until_last_button`: as I5, with presses of 1 and 3 over D, then move to OUT, release 1, Motion, release 3. Every event goes to A on W until release 3. End state.
  - `xi2_slave_selection_on_deeper_window_beats_xi1`:
    - B selects XI2 slave 4 Press|Motion|Release on D. A selects XI1 device 4 on W.
    - Press. Post-press: `xi2_pointer_grabs[4]` = `{B, D, implicit}`, and no XI1 entry exists. A gets nothing. Release, end state.
  - `xi1_on_deeper_window_beats_xi2_on_ancestor`:
    - B selects XI1 device 4 on D. A selects XI2 slave 4 on W.
    - Press. Post-press: `xi1_active_grabs[4]` = `{B, D, implicit}`.
    - A gets no d4 during the drag. End state.
  - `xi2_beats_xi1_on_same_window`:
    - On D, B selects XI2 slave 4 Press|Release, and B also selects XI1 device 4 Press|Release.
    - Press. Post-press: XI2 implicit grab `{B, D}` and no XI1 entry. B gets XI_ButtonPress d4 and no DeviceButtonPress. End state.
  - `xi1_slave_grab_and_master_grab_coexist`:
    - A selects XI1 device 4 on W. B selects core ButtonPress|ButtonRelease|PointerMotion on W.
    - Press. Post-press: `xi1_active_grabs[4]` = `{A, W, implicit}` **and** `active_pointer_grab` = `{B, W, implicit}`.
    - Move to OUT, release. A gets the XI1 release on W, and B gets the core release on W. End state.
  - `xi_grab_device_refused_during_xi1_implicit_grab`: as I5, after the press, B and then A issue XIGrabDevice for device 4. Both get status AlreadyGrabbed (1), and `xi2_pointer_grabs` stays empty. Release, end state.
  - `xi1_implicit_grab_ends_on_unmap`: as I5, A unmaps W mid-drag. The XI1 entry is removed, and the release goes to B on OUT.
  - `xi1_implicit_grab_owner_disconnect_frees_device` (Review Focus 4): as I5, A disconnects mid-drag through the production client-gone path. The entry is removed, and the release goes to B on OUT.
  - Existing tests that expect "no XI1 implicit grab" change under I5, with a row comment.
- [ ] **Step 2: Run.**
  - Expected FAIL on assertions: `i5_...`, `..._merged_snapshot`, `..._unselected_release_...`, `..._last_button`, `xi1_on_deeper_...`, `..._coexist`, `xi_grab_device_refused_...`, `..._unmap` and `..._disconnect`.
  - `xi2_slave_selection_on_deeper_...` and `xi2_beats_xi1_on_same_window` may already pass; if so, record them as GUARD.
- [ ] **Step 3: Implement** the rules. Reuse `xi1_fan_device_event`, `compute_xi1_route_targets` and the existing release deactivation.
- [ ] **Step 4: Run** the task's tests, `owner_events`, the `xi1` modules and Tasks 1-3, then the gate. Expected: PASS.
- [ ] **Step 5: Mutations:**

  | Mutation | Must fail |
  |---|---|
  | (a) no activation | `i5_...`, `..._coexist`, `..._unmap` |
  | (b) filter by the owner's live selection instead of `device_mask` | `..._merged_snapshot` |
  | (c) end the grab only on a delivered release | `..._unselected_release_...` |
  | (d) end it on the first release | `..._last_button` |
  | (e) the master check counts the slave's XI1 entry again | `..._coexist` |
  | (f) slave XI2 routing ignores `xi1_active_grabs` | `xi1_on_deeper_...` |
  | (g) XIGrabDevice ignores `xi1_active_grabs` | `xi_grab_device_refused_...` |

- [ ] **Step 6: Commit** `fix(xi1): XI1 deliveries create per-device implicit grabs with a mask snapshot (I5)`. The commit body cites `a511c445d` and XTS `XSelectExtensionEvent` assertion 5.

### Task 5: XTest DeviceMotionNotify moves the sprite

Spec invariant 7. Cases X1, X2 and X3. C16, C17 and C18 keep their measured payload.

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request/xtest.rs`: the DeviceMotion branch (~505-560).
  - It builds a `HostInputEvent::PointerMotion` like the core MotionNotify branch (~600-635): absolute for detail 0, relative for detail 1. It carries `device_axes`.
  - It calls `backend.on_host_input`, then `resync_input_position`.
  - It no longer calls `xi1_route_device_event` directly. Its wrong "does NOT move the sprite" comment goes.
- Modify: add `device_axes: Option<crate::server::Xi1MotionAxes>`:
  - to `HostInputEvent::PointerMotion` (`core_loop/message.rs`);
  - to `HostPointerEvent` (`host_x11/pump.rs`).
- Modify: every bridge that rebuilds the motion keeps `device_axes`:
  - KMS `on_host_input` (`crates/yserver/src/kms/render/backend/portable/devices.rs` ~459), into the `HostPointerEvent` built in `portable/pointer.rs` (~318, ~361), including the floating-slave path (`floating_pointer_device_id`, `floating_pointer_positions`);
  - nested `trait_impl.rs` (~106).
  - Constructors of events that originate without explicit axes pass `None`.
- Modify: `crates/yserver-core/src/core_loop/pointer_fanout.rs`. The XI1 event built at ~1863 takes `axes: event.device_axes`. The XI2 slave and master Motion forms use explicit valuators when `device_axes` is `Some`.
- Modify: `crates/yserver-core/src/core_loop/pointer_fanout/owner_events.rs`.
  - The valuator copy of `xi2_direct_xtest_motion` (~202-225) becomes `pub(in crate::core_loop) fn xtest_wire_valuators(axes: crate::server::Xi1MotionAxes) -> (u8, Vec<i32>)`. The XI2 encoder in `pointer_fanout.rs` uses it.
  - `xi2_direct_xtest_motion` and its call site (`pointer_fanout.rs` ~2943-2955) are deleted, so each packet is delivered once.
- Test: new module `crates/yserver/src/kms/render/backend/tests/xtest_device_motion.rs`. It reuses the parent module's `kbd_map_client_id` and `kbd_map_drain` (`tests/mod.rs` ~1332, ~1375). Selections are dispatched through requests.
  - The existing `xtest_device_motion_owner_events_and_exclusive_grabs_route_xi2` and `xtest_device_motion_preserves_partial_valuator_range` (`pointer_fanout/tests/owner_events.rs` ~967, ~1077) move into this module with their assertions unchanged. Only the harness changes: `KmsBackend::for_tests()` and dispatch.

**Interfaces:**
- **Produces:**
  - `HostInputEvent::PointerMotion::device_axes`;
  - `HostPointerEvent::device_axes`;
  - `xtest_wire_valuators`.
- **Position rules:**
  - **Detail 0:** each of axes 0/1 present in the Xorg-copied valuators replaces that coordinate, and a missing axis keeps the current one. For X3 (`first=1`, value 175) the result is (x, 0).
  - **Detail 1:** the current position plus the copied values.
  - The backend clamps to the screen as for core fakes.
  - A floating XTest slave moves its floating position, not the master sprite, through the existing floating path.

- [ ] **Step 1: Write the tests** in `xtest_device_motion.rs`.
  - **Setup:** A selects XI2 master Motion and core PointerMotion on W, and B the same on OUT. A core XTestFakeInput MotionNotify first puts the sprite at (150,150).
  - `x1_absolute_device_motion_moves_sprite`: XTestFakeInput DeviceMotionNotify for device 4, detail 0, axes (650,150).
    - `state.pointer_root == (650,150)`, and the window under the sprite is OUT.
    - B gets XI_Motion d2/s4 on OUT, child None. A gets nothing.
  - `x2_relative_device_motion_moves_sprite`: detail 1, (+500, 0). Same assertions.
  - `x3_one_axis_device_motion_follows_xorg_copy`: detail 0, `first=1`, value 175. `state.pointer_root == (150,0)`, the sprite is over the root (child None), and A and B get no packet.
  - `xtest_relative_device_motion_clamps_to_screen` (Review Focus 5): detail 1, (+5000, 0). `pointer_root == (screen_width − 1, 150)`.
  - The moved C16-C18 tests keep their assertions: mask 0x2, axis value 0, single delivery.
- [ ] **Step 2: Run** `cargo test -p yserver xtest_device_motion`.
  - Expected: the four X tests FAIL, with the sprite unmoved.
  - The moved C16-C18 tests PASS on the base. They still use the direct path, and the KMS harness runs the same request.
- [ ] **Step 3: Implement** as listed in Files.
- [ ] **Step 4: Run** the task's tests, `owner_events`, `xtest` and `implicit_grabs`, then the gate. Expected: PASS.
- [ ] **Step 5: Mutations:**

  | Mutation | Must fail |
  |---|---|
  | (a) ignore detail (always absolute) | `x2_...`, `..._clamps_...` (y becomes 0) |
  | (b) use the request value for X3 instead of Xorg's copy | `x3_...` |
  | (c) drop `device_axes` in the KMS bridge | `xtest_device_motion_preserves_partial_valuator_range` |
  | (d) drop `device_axes` in the XI2 encoder | `xtest_device_motion_preserves_partial_valuator_range` |
  | (e) keep `xi2_direct_xtest_motion` as well | `xtest_device_motion_owner_events_and_exclusive_grabs_route_xi2` (duplicate packet) |

- [ ] **Step 6: Commit** `fix(xtest): DeviceMotionNotify moves the sprite through the pointer motion path (C18, X1-X3)`.

### Task 6: Oracle, docs and status (coordinator)

The coordinator does this task, not codex.

- [ ] **Oracles:** build the branch in release mode. Run `passive-grabs/oracle/run-fresh.sh`, `passive-grabs/xtest/run.sh` and the I1-I8 and C1-C20 runs, with one fresh server per P, I5 and X case.
  - Expected: P1-P5, P4b, P4c, I5 and X1-X3 match Xvfb under the comparison rules of the measurements doc.
  - Expected: I1-I8 and C1-C20 are unchanged, with C15 excluded as in #374.
- [ ] **Docs:** append a "Branch results" section to the measurements doc, and add a `docs/status.md` entry like #374's.
- [ ] **Full gate:**
  - `cargo test --workspace --features xdmcp`;
  - the lavapipe `--ignored` suite;
  - `just man`;
  - rule F, five times under load.
- [ ] **Hardware**, after asking the user, on NVIDIA and AMD:
  - xfwm4 click-to-focus: one XI2 press and one ReplayPointer, by x11trace;
  - marco;
  - the #374 scenes.
- [ ] **PR body,** on the user's word: XTS `XSelectExtensionEvent` assertion 5 now fails its checks 5 and 6 as on Xorg, citing `a511c445d`.
- [ ] **Commit** `docs: passive-grab activation branch results`.
