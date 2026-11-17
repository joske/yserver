# Pointer Routing Aligned with Xorg Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Under an owner-events grab, deliver pointer events the way Xorg 21.1.24 does (owner-only natural delivery per device form, per-window XI2→XI1→core order, grab-window fallback), and keep the XI1 `MORE_EVENTS` bit.

**Architecture:** Targeted fixes inside the existing delivery code in `crates/yserver-core/src/core_loop/pointer_fanout.rs` (XI2 owner-events filter, smooth-scroll Motion path, `grabbed_natural_target`) and `crates/yserver-core/src/xinput/mod.rs` (`encode_xi1_device_input_event`). Expected results come from a measured Xvfb 21.1.24 oracle, recorded in a findings document before any routing code changes.

**Tech Stack:** Rust (yserver-core), Xvfb built from `/data/Projects/xorg-21.1.24` with meson, small measurement clients in C against libX11/libXi/libXtst (MIT).

**Spec:** `docs/superpowers/specs/2026-11-16-pointer-routing-xorg-design.md`

## Global Constraints

- Base: joske/master d8d3416b. No access control, no Receive checks, no 4c changes.
- Cite Xorg as `../xserver` (tag xorg-server-21.1.24); quote Xorg freely (MIT). No copyleft code read or used.
- Reuse first: new code only where existing helpers (`subscribers_by_id`, `xi2_form_targets_for_source`, `xi2_route_window_for_source`, `xi2_master_selectors`, `xi2_child_toward`, `active_grab_for_source`, `active_grab_target_for_source`) cannot do it; each new piece justified in the commit body.
- No new `YSERVER_*` environment variables.
- Keep: the core implicit-grab exception (`pointer_fanout.rs` ~1876-1887, XFCE) unless Task 1 measures otherwise; slave-then-master double delivery (E27 tests stay green); every delivered press gets its release.
- Existing tests change expected values only where a Task 1 measured row (or, for scroll, a cited `../xserver` line) gives a different answer; each changed expectation names its row in a comment and in the commit body.
- Tests drive real request and input paths: real grab/selection requests through request dispatch, input injected through the production pointer fanout entry (`xi_register_source` + physical/XTest injection as existing tests in `pointer_fanout/tests.rs` do). No hand-built grab or selection state.
- Every fix: a test that fails on d8d3416b, then passes; one named mutation per test that makes it fail again, verified.
- Commits: signed (`-S`), explicit paths, trailers `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>` and `Implemented-By: codex (gpt-6-luna xhigh)`. No hardware access by implementers.
- Per-task gate: `cargo +nightly fmt` (AGENTS.md), `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test -p yserver-core pointer_fanout`, `cargo test -p yserver-core xi2`, `cargo test -p yserver-core grab`, `cargo test -p yserver-core xi2_grabs`.

## Review Focus

1. A client that selected both core and XI2 on the window under the pointer while owning the owner-events grab: it must still get its own delivery (owner wins inside an attempt), not a fallback to the grab window. Test in Task 3.
2. A slave-device XIGrabDevice with no master grab (the grabbed slave floats): master-form selectors of other clients keep normal delivery for input from another, still attached source. Test in Task 4.
3. Owner-events grab released mid-motion (ungrab between two motions): the second motion uses ungrabbed routing again. Test in Task 3.
4. Grab window that is not viewable / pointer outside the grab window's tree with owner_events: fallback still reports grab-window-relative coordinates. Test in Task 4.
5. Button press delivered by owner-events natural delivery to a foreign-owned window owned by the grab client: its release reaches the same client. Test in Task 3.

---

### Task 1: Xorg oracle and measured cases

**Files:**
- Create: `docs/superpowers/findings/2026-11-16-pointer-routing-xorg-measurements.md`
- Scratch (not committed): `~/Projects/claude-scratch/pointer-routing/oracle/` (Xvfb build dir, clients, run script)

**Interfaces:**
- Produces: the findings table, one row per case id below, columns: case id, setup (grabbing client, grab request, grabbed device, owner_events, grab mask; each client's selections with window, device and event types; which input source injects), injected input, Xvfb 21.1.24 result (per client: event type/evtype, deviceid, sourceid, event window, child, event_x/y, root_x/y), yserver d8d3416b result, match yes/no. Later tasks cite rows by id; a test may only assert a row that exists.

- [ ] **Step 1: Build Xvfb** from `/data/Projects/xorg-21.1.24` with meson (`-Dxvfb=true`, other DDXs off), install prefix inside the scratch dir. Expected: `Xvfb :97 -screen 0 1024x768x24` starts and `xdpyinfo -display :97` answers.
- [ ] **Step 2: Write the clients** (C, libX11/libXi/libXtst): a setup client per role that creates the windows/selections/grabs of a case from arguments and prints every received event as one normalized line (window ids mapped to names); an injector using XTest (the XTest pointer slave) and, where a case needs a second source, `XIGrabDevice` on the vfb mouse slave while injecting through XTest. One run script executes every case on a given display.
- [ ] **Step 3: Cases.** Grabs are on master 2 unless stated; "B XI2" means B selects master XI_Motion|XI_ButtonPress|XI_ButtonRelease unless stated.
  - C1-C3: A XIGrabDevice owner_events=True (mask Motion|Press|Release), A no natural selections; B XI2 on (C1) the grab window, (C2) a descendant, (C3) a top-level owned by A outside the grab window.
  - C4: as C1, A also selects core PointerMotion on B's window (core attempt after a rejected XI2 attempt).
  - C4b: as C2 (B XI2 on descendant D of the grab window), A also selects both core PointerMotion and master XI_Motion on D (owner wins inside the XI2 attempt: natural XI2 on D, not fallback on the grab window; no duplicate core).
  - C5: as C1, B selects slave (XTest pointer) XI_Motion: slave form reaches B.
  - C6: core GrabPointer owner_events=True by A; B XI2 on a window below where A selects core PointerMotion on its parent.
  - C7: core GrabPointer owner_events=True by A; A core PointerMotion on W, B XI2 on W.
  - C7s: as C6 but B selects slave XI_Motion only (slave-only does not block the core walk).
  - C8: reproduce exactly the setup of `implicit_grab_xi2_release_reaches_all_master_selector_xfce` and of the test documented at `pointer_fanout/tests.rs` ~5225 (read both; state in the row which grab, core or XI2 implicit, Xorg activates).
  - C8c: core implicit grab: A selects core ButtonPress|ButtonRelease|PointerMotion on W; B selects master XI_Motion only (no XI2 press) on D, a child of W under the pointer; motion, press, motion, release. Record that Xorg activates a core implicit grab for A (if it does not, stop and report) and whether B receives the Motion during that grab. This is the row for the `core_implicit` exception (`pointer_fanout.rs` ~1887).
  - C9: one client with slave + master XI2 selections (E27 setup of the existing E27 tests): press/release counts per form.
  - C10: XI1 DeviceMotionNotify + DeviceButtonPress selected via XSelectExtensionEvent on the XTest pointer: raw 32-byte events; record byte 31 (deviceid | MORE_EVENTS) and what follows.
  - C11: as C1, then XIUngrabDevice between two motions.
  - C12: XIGrabDevice owner_events=True by A with the pointer outside the grab window's tree: fallback event window, child, event_x/y.
  - C13: as C2 for ButtonPress then ButtonRelease (fallback child and release follow-up).
  - C14: A core GrabPointer owner_events=True; press then release over a top-level owned by A outside the grab window with A's core button selection there.
  - C15: XIGrabDevice on the vfb mouse slave (no master grab); inject through XTest; B XI2 (master) on the window under the pointer receives the master form. Xvfb only: headless yserver has no second pointer source (`xinput/registry.rs:67-82`). The yserver result for C15 (baseline and branch) is produced by Task 4's Rust test, which registers a second source with `xi_register_source`, grabs it with a real XIGrabDevice, injects through the XTest slave, and first asserts the grabbed device id differs from the injecting source id. The row's yserver column names that test.
- [ ] **Step 4: Run all cases on Xvfb and on yserver d8d3416b headless** (`target/release/yserver :98 --headless`, see `crates/yserver/src/launch.rs`). Expected: one result file per case per server.
- [ ] **Step 5: Write the findings doc**: table, build commands, client sources inlined as an appendix. List differing rows. If a row contradicts the spec's routing contract, stop and report.
- [ ] **Step 6: Commit** `docs(findings): pointer routing measured against Xvfb 21.1.24`.

### Task 2: XI1 MORE_EVENTS bit

**Files:**
- Modify: `crates/yserver-core/src/xinput/mod.rs` (`encode_xi1_device_input_event`, ~1361-1392)
- Test: next to the existing XI1 delivery tests

**Interfaces:**
- Consumes: Task 1 row C10.

- [ ] **Step 1: Failing test** `xi1_motion_with_valuator_keeps_more_events_bit`: a client selects XI1 DeviceMotionNotify and DeviceButtonPress on device 4 through the real request path; inject one motion and one press through the production fanout. Assert the motion event's byte 31 equals `4 | 0x80` and the next event is a DeviceValuator for device 4 with the valuators of row C10; assert the press event's byte 31 matches row C10. Mutation: mask byte 31 with `0x7f` again.
- [ ] **Step 2: Run** `cargo test -p yserver-core xi1_motion_with_valuator_keeps_more_events_bit`. Expected: FAIL (byte 31 == 4).
- [ ] **Step 3: Implement**: write the caller's device byte as given (bit 7 kept), `../xserver/dix/eventconvert.c:369-370`.
- [ ] **Step 4: Run** the test and the per-task gate. Expected: PASS.
- [ ] **Step 5: Commit** `fix(xi1): keep MORE_EVENTS on device events followed by valuators`.

### Task 3: One owner-events decision per window and device form (master form)

**Files:**
- Modify: `crates/yserver-core/src/core_loop/pointer_fanout.rs` (core grab branch ~1205-1260, `grabbed_natural_target` ~3560-3597, XI2 owner-events block ~1870-1910, delivery loop ~2300-2370 only where it consumes the decision)
- Test: new `crates/yserver-core/src/core_loop/pointer_fanout/tests/owner_events.rs`, registered in the tests module

**Interfaces:**
- Consumes: Task 1 rows C1-C4b, C6-C7s, C8, C8c, C9, C11, C14.
- Produces, in `pointer_fanout.rs`:
  - `enum OwnerEventsRoute { Natural { window: ResourceId, x: i16, y: i16, child: ResourceId, protocol: OwnerProtocol }, Fallback }`, `enum OwnerProtocol { Xi2, Xi1, Core }`.
  - `fn owner_events_route(state: &ServerState, form: Xi2PointerForm, slave_deviceid: Option<u16>, grab_client: ClientId, start: ResourceId, x: i16, y: i16, core_mask: u32, xi2_evtype: Option<u16>, xi1_mask: Option<u32>) -> OwnerEventsRoute` — walks from `start` toward the root; on each window tries XI2 for this form (`xi2_form_targets_for_source`), then XI1 (slave form only), then core (master form only, `subscribers_by_id`). In an attempt, the grab client present → `Natural` with that protocol; only other clients present → the attempt is rejected and the remaining protocols are still tried on this window; if nothing was delivered on a window where any attempt was rejected → `Fallback`; if no protocol matched at all, continue to the parent. `grabbed_natural_target` is replaced by this function (delete it; its callers use `owner_events_route` with `form = Master`).
  - `fn grab_for_form(state: &ServerState, source: PointerXiSource, form: Xi2PointerForm) -> Option<…>` — the grab held on exactly that device (master id or slave id), with no master→slave inheritance; `active_grab_for_source` stays for its other callers.
- Rule for callers: for the master form, exactly one delivery per window decision: `Natural{Xi2}` sends only the XI2 event to the grab client, `Natural{Core}` only the core event, `Fallback` sends to the grab window per the grab's mask (existing fallback code). The core branch's `if !via_xi2` suppression is replaced by this rule.

- [ ] **Step 1: Failing tests** (assert every client's full received set as the row says): `owner_events_xi2_grab_window_foreign_selector_gets_nothing` (C1), `owner_events_xi2_descendant_foreign_selector_gets_nothing` (C2, recipients only; child is Task 4), `owner_events_xi2_owned_toplevel_foreign_selector_gets_nothing` (C3), `owner_events_core_after_rejected_xi2_on_same_window` (C4), `owner_events_owner_xi2_and_core_on_same_window_delivers_once` (C4b, Review Focus 1; asserts event window D), `core_implicit_grab_foreign_xi2_motion_follows_row` (C8c, asserts what the row measured for B's Motion during A's core implicit grab), `core_owner_events_stops_at_foreign_master_xi2_selection` (C6), `core_owner_events_owner_core_wins_over_foreign_xi2_on_same_window` (C7), `core_owner_events_slave_only_xi2_does_not_block` (C7s), `owner_events_ungrab_restores_natural_xi2_delivery` (C11, Review Focus 3), `core_owner_events_release_follows_press_to_owned_window` (C14, Review Focus 5). Mutations: treat a rejected XI2 attempt as end-of-window (C4 fails); skip the core attempt after XI2 (C7 fails); count slave selections in the master form (C7s fails); drop the owner check (C1-C3 fail); return `Fallback` when the owner is present in the XI2 attempt (C4b fails: event window becomes the grab window); remove the `core_implicit` exception, or apply it where the row says it does not hold (C8c fails).
- [ ] **Step 2: Run** them. Expected: rows marked "differ" in Task 1 fail; the others pass (record which).
- [ ] **Step 3: Implement** `owner_events_route`, `grab_for_form`, and wire the master-form callers per the rule. Leave the `core_implicit` exception untouched unless row C8 says otherwise.
- [ ] **Step 4: Run** new tests, the E27 tests, `implicit_grab_xi2_release_reaches_all_master_selector_xfce`, `implicit_grab_xi2_motion_filtered_by_owner_selection`, `xi_owner_events_fallback_preserves_other_recipients_root_geometry`, `active_grab_owner_events_uses_grab_client_filtered_propagation`, and the gate. Expected: PASS; a pre-existing test changes only with a named row.
- [ ] **Step 5: Commit** `fix(input): owner-events delivery decided per window across XI2, XI1 and core`.

### Task 4: Slave form and fallback geometry

**Files:**
- Modify: `crates/yserver-core/src/core_loop/pointer_fanout.rs` (XI2/XI1 slave-form delivery; XI2 fallback child ~2330-2337)
- Test: `pointer_fanout/tests/owner_events.rs`

**Interfaces:**
- Consumes: `owner_events_route`, `grab_for_form` (Task 3); `xi2_child_toward`; rows C2, C5, C12, C13, C15.

- [ ] **Step 1: Failing tests**: `owner_events_master_grab_leaves_slave_form_to_selectors` (C5), `slave_grab_leaves_master_form_from_other_source` (C15, Review Focus 2), `owner_events_fallback_child_toward_pointer` (C2/C13: fallback XI2 event has the child Xorg reports, not 0), `owner_events_fallback_outside_tree_geometry` (C12, Review Focus 4). Update `pointer_fanout/tests.rs` ~1527-1530 (pinned child 0) to row C2's value, naming the row. Mutations: apply the master grab to the slave form (C5 fails); force child 0 (C2/C13 fail).
- [ ] **Step 2: Run**. Expected: C2/C13 child and C5 (if the row differs) fail.
- [ ] **Step 3: Implement**: decide each form with `grab_for_form` and `owner_events_route` separately; keep per-form fallback flags and geometry (no shared override across forms); compute the fallback child with `xi2_child_toward(grab_window, pointer_window)`.
- [ ] **Step 4: Run** new tests, E27 tests, gate. Expected: PASS.
- [ ] **Step 5: Commit** `fix(input): owner-events per device form, fallback child toward the pointer`.

### Task 5: Smooth-scroll Motion under owner-events grab

**Files:**
- Modify: `crates/yserver-core/src/core_loop/pointer_fanout.rs` (scroll Motion block ~2090-2130)
- Test: `pointer_fanout/tests/owner_events.rs`

**Interfaces:**
- Consumes: `owner_events_route`, `grab_for_form` (Task 3).

- [ ] **Step 1: Failing test** `owner_events_scroll_motion_routed_independently_of_wheel_press`: register a `PhysicalPointer` with a scroll class (as the wheel tests in `pointer_fanout/tests.rs` ~641-681 do). A XIGrabDevice master owner_events=True, grab mask XI_Motion|XI_ButtonPress. The pointer is over D, a descendant of the grab window; A selects master XI_ButtonPress on D; B selects master XI_Motion on D. Inject one wheel step. Expected (source-backed, comment cites `../xserver/dix/getevents.c:1646-1692`, `../xserver/dix/events.c:2071-2075,4431-4462`): the wheel ButtonPress goes naturally to A on D; the smooth-scroll Motion is rejected for B on D and falls back to A on the grab window; B receives nothing. Mutation: route the scroll Motion with the wheel press's decision (A on D) or leave its natural targets unfiltered (B receives Motion) — both fail. Second test `owner_events_scroll_motion_has_no_core_attempt`: same setup, A also selects core PointerMotion on D; expected: A receives no core MotionNotify and still gets the XI2 Motion on the grab window. Mutation: pass the ordinary core motion mask instead of 0 (spurious core Motion, XI2 fallback lost).
- [ ] **Step 2: Run**. Expected: FAIL on d8d3416b-equivalent routing (B receives Motion or A gets it on D).
- [ ] **Step 3: Implement**: the scroll Motion block computes its own `owner_events_route` with `xi2_evtype = XI_Motion`, independent of the press decision, with `core_mask = 0`: a wheel-only Motion has no X/Y and has no core form (`../xserver/dix/eventconvert.c:113-122`), so the core attempt never counts as delivery.
- [ ] **Step 4: Run** test, existing scroll tests, gate. Expected: PASS.
- [ ] **Step 5: Commit** `fix(input): owner-events rule on smooth-scroll motion`.

### Task 6: Re-measure and record

**Files:**
- Modify: `docs/superpowers/findings/2026-11-16-pointer-routing-xorg-measurements.md` (add column "yserver branch")
- Modify: `docs/status.md` (one entry for this PR)

- [ ] **Step 1: Re-run** the Task 1 script against the branch build headless. Expected: every row matches Xvfb.
- [ ] **Step 2: Load runs**: `cargo test -p yserver-core pointer_fanout`, `xi2_grabs` and `grab` 5 times under 16 busy loops. Expected: 5/5 pass.
- [ ] **Step 3: Full gate**: `cargo test --workspace`, the CI `--workspace --features xdmcp -- --ignored` lavapipe suite, FreeBSD cross clippy, `tools/check-layout.sh`, `just man`. Expected: all pass.
- [ ] **Step 4: Update** the findings column and a `docs/status.md` entry (dated, one paragraph, no "as Xorg" phrasing for our own rulings; second master noted as unreachable non-goal). Commit `docs: pointer routing measured on the branch`.

Hardware smoke (Steam/CEF, Cinnamon menus, XFCE dialogs, Enlightenment 0.27, sync grabs and replay) is run by the coordinator with the user after Task 6, not by implementers.
