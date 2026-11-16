# Pointer Routing Aligned with Xorg Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Under an owner-events grab, deliver pointer events the way Xorg 21.1.24 does (owner-only natural delivery per device form, per-window XI2→XI1→core order, grab-window fallback), and keep the XI1 `MORE_EVENTS` bit.

**Architecture:** Targeted fixes inside the existing delivery code in `crates/yserver-core/src/core_loop/pointer_fanout.rs` (XI2 owner-events filter, smooth-scroll Motion path, `grabbed_natural_target`) and `crates/yserver-core/src/xinput/mod.rs` (`encode_xi1_device_input_event`). Expected results come from a measured Xvfb 21.1.24 oracle, recorded in a findings document before any routing code changes.

**Tech Stack:** Rust (yserver-core), Xvfb built from `/data/Projects/xorg-21.1.24` with meson, small measurement clients in C against libX11/libXi/libXtst (MIT).

**Spec:** `docs/superpowers/specs/2026-11-16-pointer-routing-xorg-design.md`

## Global Constraints

- Base: joske/master d8d3416b. No access control, no Receive checks, no 4c changes.
- Cite Xorg as `../xserver` (tag xorg-server-21.1.24); quote Xorg freely (MIT). No copyleft code read or used.
- Reuse first: new code only where existing helpers (`subscribers_by_id`, `xi2_pointer_selected_on`, `compute_xi2_targets_for_source`, `active_grab_for_source`, `active_grab_target_for_source`, `grabbed_natural_target`) cannot do it; each new piece justified in the commit body.
- No new `YSERVER_*` environment variables.
- Keep: the core implicit-grab exception (`pointer_fanout.rs` ~1876-1887, XFCE) unless Task 1 measures otherwise; slave-then-master double delivery (E27 tests stay green); every delivered press gets its release.
- Existing tests change expected values only where a Task 1 measured row (or, for scroll, a cited `../xserver` line) gives a different answer; each changed expectation names its row in a comment and in the commit body.
- Tests drive real request and input paths: real grab/selection requests through request dispatch, input injected through the production pointer fanout entry (`xi_register_source` + physical/XTest injection as existing tests in `pointer_fanout/tests.rs` do). No hand-built grab or selection state.
- Every fix: a test that fails on d8d3416b, then passes; one named mutation per test that makes it fail again, verified.
- Commits: signed (`-S`), explicit paths, trailers `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>` and `Implemented-By: codex (gpt-6-luna xhigh)`. No hardware access by implementers.
- Per-task gate: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test -p yserver-core pointer_fanout`, `cargo test -p yserver-core xi2`, `cargo test -p yserver-core grab`.

## Review Focus

1. A client that selected both core and XI2 on the window under the pointer while owning the owner-events grab: it must still get its own delivery (owner wins inside an attempt), not a fallback to the grab window. Test in Task 3.
2. A slave-device XIGrabDevice with no master grab: master-form selectors of other clients keep normal delivery. Test in Task 3.
3. Owner-events grab released mid-motion (ungrab between two motions): the second motion uses ungrabbed routing again. Test in Task 3.
4. Grab window that is not viewable / pointer outside the grab window's tree with owner_events: fallback still reports grab-window-relative coordinates. Test in Task 4.
5. Button press delivered by owner-events natural delivery to a foreign-owned window owned by the grab client: its release reaches the same client. Test in Task 4.

---

### Task 1: Xorg oracle and measured cases

**Files:**
- Create: `docs/superpowers/findings/2026-11-16-pointer-routing-xorg-measurements.md`
- Scratch (not committed): `~/Projects/claude-scratch/pointer-routing/oracle/` (Xvfb build dir, clients, run script)

**Interfaces:**
- Produces: the findings table, one row per case `C1..Cn`, columns: case id, setup (grabbing client, grab request, grabbed device, owner_events, grab mask; each client's selections with window, device and event types), injected input, Xvfb 21.1.24 result (which client receives which event, device id, event window, child, coordinates), yserver d8d3416b result, match yes/no. Later tasks cite rows by id.

- [ ] **Step 1: Build Xvfb** from `/data/Projects/xorg-21.1.24` with meson (`-Dxvfb=true`, other DDXs off), install prefix inside the scratch dir. Expected: `Xvfb :97 -screen 0 1024x768x24` starts and `xdpyinfo -display :97` answers.
- [ ] **Step 2: Write the clients** (C, libX11/libXi/libXtst): a setup client per role (grabber, selector) that creates the windows/selections/grabs of a case from command-line arguments and prints every received event as one normalized line (type, evtype, deviceid, sourceid, event window, child, event_x/y, root_x/y; window ids mapped to names); an injector that moves/clicks with XTest. One run script executes all cases against a display and writes one result file per case.
- [ ] **Step 3: Cases, at least:**
  - C1-C3: XIGrabDevice on master 2, owner_events=True, A selects nothing natural; B selects master XI_Motion|XI_ButtonPress|XI_ButtonRelease on (C1) the grab window, (C2) a descendant, (C3) a top-level owned by A outside the grab window.
  - C4: as C1 but A also selects core PointerMotion on the window B selected (owner delivery on that window).
  - C5: as C1 but B selects slave (XTest pointer) XI_Motion: slave form reaches B.
  - C6: core GrabPointer owner_events=True by A; B selects master XI_Motion on a window below where A selects core PointerMotion on its parent.
  - C7: core GrabPointer owner_events=True by A; A selects core PointerMotion on W, B selects master XI_Motion on W.
  - C8: core implicit grab (press with core ButtonPress selection on parent, XI2 leaf selector by B) — the XFCE case: who gets XI2 press/release.
  - C9: slave + master XI2 selections by one client (E27 case): press/release counts.
  - C10: XI1 DeviceMotionNotify selected through XSelectExtensionEvent on the XTest pointer: raw bytes of the first event's deviceid field (MORE_EVENTS bit 0x80 set when DeviceValuator follows).
- [ ] **Step 4: Run all cases on Xvfb and on yserver d8d3416b headless** (`target/release/yserver :98 --headless`, see `crates/yserver/src/launch.rs`). Expected: one result file per case per server.
- [ ] **Step 5: Write the findings doc** with the table, the build commands, and the client sources inlined as an appendix (they are short; the doc is the reproducible record). State which rows differ; those are the defects Tasks 2-4 fix. If a row contradicts the spec's routing contract, stop and report (do not implement against the spec).
- [ ] **Step 6: Commit** `docs(findings): pointer routing measured against Xvfb 21.1.24`.

### Task 2: XI1 MORE_EVENTS bit

**Files:**
- Modify: `crates/yserver-core/src/xinput/mod.rs` (`encode_xi1_device_input_event`, ~1361-1392)
- Test: next to the existing XI1 encoder/fanout tests (`pointer_fanout/tests.rs` or `xinput` tests, wherever XI1 motion delivery is already tested)

**Interfaces:**
- Consumes: Task 1 row C10.

- [ ] **Step 1: Failing test** `xi1_motion_with_valuator_keeps_more_events_bit`: a client selects XI1 DeviceMotionNotify on device 4 through the real request path, inject one motion through the production fanout; assert the received DeviceMotionNotify byte 1 equals `4 | 0x80` and that a DeviceValuator event follows; also assert a press with no following valuator (if the caller does not set the bit) keeps `deviceid` unchanged. Mutation: mask with `0x7f` again.
- [ ] **Step 2: Run** `cargo test -p yserver-core xi1_motion_with_valuator_keeps_more_events_bit`. Expected: FAIL (byte 1 == 4).
- [ ] **Step 3: Implement**: write the caller's `deviceid` byte as given (bit 7 included), per `../xserver/dix/eventconvert.c:369-370`.
- [ ] **Step 4: Run** the test and the per-task gate. Expected: PASS.
- [ ] **Step 5: Commit** `fix(xi1): keep MORE_EVENTS on device events followed by valuators`.

### Task 3: XI2 owner-events delivery per device form

**Files:**
- Modify: `crates/yserver-core/src/core_loop/pointer_fanout.rs` (owner-events block ~1870-1910; `compute_xi2_targets_for_source` callers ~3957-3985 only as needed)
- Test: `crates/yserver-core/src/core_loop/pointer_fanout/tests.rs` (or a new `pointer_fanout/tests/owner_events.rs` registered in the tests module)

**Interfaces:**
- Consumes: Task 1 rows C1-C5, C8, C9.
- Produces: `fn owner_events_natural_xi2_targets(state: &ServerState, form_deviceid: u16, grab: &ActiveGrab, natural: &[ClientId]) -> Option<Vec<ClientId>>` in `pointer_fanout.rs` — returns `Some(owner-only list)` when the grab owner is a natural selector for this form, `None` when only foreign selectors matched (caller falls back to the grab window if the grab mask selects the event). Use the existing grab record type the module already passes (`ActiveGrab` or its actual name); Task 5 consumes this function.

- [ ] **Step 1: Failing tests**, one per row, each asserting the full received set per client exactly as the Task 1 row says: `owner_events_xi2_grab_window_foreign_selector_gets_nothing` (C1), `..._descendant_...` (C2), `..._owned_toplevel_...` (C3), `owner_events_owner_selection_on_same_window_delivers_to_owner` (C4, Review Focus 1), `owner_events_master_grab_leaves_slave_form_to_selectors` (C5, Review Focus 2), `owner_events_ungrab_restores_natural_xi2_delivery` (Review Focus 3). Mutations: drop the owner filter (C1-C3 fail); apply the master grab to the slave form (C5 fails).
- [ ] **Step 2: Run** them. Expected: C1-C3 fail as measured (B receives events); C4/C5/ungrab may already pass — record which.
- [ ] **Step 3: Implement** `owner_events_natural_xi2_targets` and use it in the owner-events branch, deciding per device form with that form's own grab (`active_grab_for_source` for the form, no master→slave fallback for this decision). Leave the `core_implicit` exception untouched unless C8 measured otherwise.
- [ ] **Step 4: Run** new tests, the E27 tests (`grep -n e27` in `pointer_fanout/tests.rs`), `implicit_grab_xi2_motion_filtered_by_owner_selection`, `xi_owner_events_fallback_preserves_other_recipients_root_geometry`, and the per-task gate. Expected: all PASS; any pre-existing test that now fails is changed only if a Task 1 row says so (name the row).
- [ ] **Step 5: Commit** `fix(input): owner-events XI2 delivery goes to the grab owner only`.

### Task 4: Core owner-events walk stops at foreign XI2 selections

**Files:**
- Modify: `crates/yserver-core/src/core_loop/pointer_fanout.rs` (`grabbed_natural_target` ~3560-3597)
- Test: same test module as Task 3

**Interfaces:**
- Consumes: Task 1 rows C6, C7; `xi2_pointer_selected_on` and `XI2_ABSORBING_POINTER_DEVICES` from `fanout.rs` (master form only).

- [ ] **Step 1: Failing tests**: `core_owner_events_stops_at_foreign_master_xi2_selection` (C6: fallback to grab window, A's parent selection not reached), `core_owner_events_owner_core_selection_wins_over_foreign_xi2_on_same_window` (C7), `core_owner_events_slave_only_xi2_does_not_block` (slave-only selection by B: walk continues), `core_owner_events_fallback_coordinates_relative_to_grab_window` (Review Focus 4), `core_owner_events_release_follows_press_to_owned_window` (Review Focus 5). Mutation: remove the XI2 check (C6 fails); check the slave id too (slave-only test fails).
- [ ] **Step 2: Run**. Expected: C6 fails as measured.
- [ ] **Step 3: Implement**: in the walk, on each window, if the grab client has a matching core selection there, deliver (owner wins); else if another client holds a matching master-form XI2 selection or any core selection, stop and return `None` (caller falls back); else continue up. Pass the XI2 evtype in as a new parameter `xi2_evtype: Option<u16>`.
- [ ] **Step 4: Run** new tests + gate. Expected: PASS.
- [ ] **Step 5: Commit** `fix(input): core owner-events walk stops at foreign XI2 selections`.

### Task 5: Smooth-scroll Motion under owner-events grab

**Files:**
- Modify: `crates/yserver-core/src/core_loop/pointer_fanout.rs` (scroll Motion block ~2090-2130)
- Test: same test module as Task 3

**Interfaces:**
- Consumes: `owner_events_natural_xi2_targets` (Task 3).

- [ ] **Step 1: Failing test** `owner_events_scroll_motion_foreign_selector_gets_nothing`: register a `PhysicalPointer` source with a scroll class (as existing wheel tests in `pointer_fanout/tests.rs` ~641-681 do), A XIGrabDevice master owner_events with XI_Motion in its mask, B selects master XI_Motion on the window under the pointer, inject a wheel step; assert A gets the smooth-scroll Motion on the grab window and B gets none. Comment cites `../xserver/dix/getevents.c:1646-1692` and `../xserver/dix/events.c:2071-2075,4431-4462` (source-backed per spec). Mutation: keep `natural_targets` unfiltered.
- [ ] **Step 2: Run**. Expected: FAIL (B receives Motion).
- [ ] **Step 3: Implement**: replace the scroll path's own owner_events handling with `owner_events_natural_xi2_targets`.
- [ ] **Step 4: Run** test, existing scroll tests (`grep -n scroll` in the test module), gate. Expected: PASS.
- [ ] **Step 5: Commit** `fix(input): owner-events rule on smooth-scroll motion`.

### Task 6: Re-measure and record

**Files:**
- Modify: `docs/superpowers/findings/2026-11-16-pointer-routing-xorg-measurements.md` (add column "yserver branch")
- Modify: `docs/status.md` (one entry for this PR)

- [ ] **Step 1: Re-run** the Task 1 script against the branch build headless. Expected: every row matches Xvfb.
- [ ] **Step 2: Load runs**: `cargo test -p yserver-core pointer_fanout` 5 times under 16 busy loops. Expected: 5/5 pass.
- [ ] **Step 3: Full gate**: `cargo test --workspace`, the CI `--workspace --features xdmcp -- --ignored` lavapipe suite, FreeBSD cross clippy, `tools/check-layout.sh`, `just man`. Expected: all pass.
- [ ] **Step 4: Update** the findings column and a `docs/status.md` entry (dated, one paragraph, no "as Xorg" phrasing for our own rulings; second master noted as unreachable non-goal). Commit `docs: pointer routing measured on the branch`.

Hardware smoke (Steam/CEF, Cinnamon menus, XFCE dialogs, Enlightenment 0.27, sync grabs and replay) is run by the coordinator with the user after Task 6, not by implementers.
