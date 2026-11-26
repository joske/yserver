# Access control layer, PR 4c plan 2 — crossings, focus, barriers and screen resize Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Check Receive on every existing crossing, focus, barrier and root/COW `ConfigureNotify` delivery, without
changing any client-observable byte when no policy is installed.

**Architecture:** These are notifications with fixed recipients: each keeps its present recipients, deduplication,
multiplicity, encoder and order, and gets a local Receive check through the existing checked fanouts. A denial drops
that recipient's copy; the transition chain, focus change, grab, barrier state and resize continue. The shared
pointer writers check only their crossing branch; cooked pointer data stays for the pointer plan. No routing change.

**Tech Stack:** Rust, crate `yserver-core`; existing encoders, recording backend and access policy; std only.

**Spec:** `docs/superpowers/specs/2026-10-30-access-control-layer-design.md`, "Input delivery under denial" →
**Notifications** and **Batches** (commit `57a2f411`), plus "Who is checked".

**Reference:** Xorg (MIT) **xorg-server 21.1.24**, `/data/Projects/xorg-21.1.24`. The spec binds where it decides;
otherwise follow Xorg. Rules the spec names as yserver rules are never described as Xorg behavior.

**Source survey:** `/data/Projects/claude-scratch/access-control/pr4c-v2/survey.md` §1 "Raw events, crossings, and
notifications", §2 "Pointer and shared XI1 input" and "XI crossing, focus, and device notifications", §3. Line
numbers below are navigation anchors at `bb729047` (plan 1 head).

## Global Constraints

- With no policy installed, or with every check allowed, every byte a client observes stays the same: events, replies,
  errors, their order, sequence numbers, byte order and multiplicity. The existing suite passes unmodified.
- No new recipient selection, event production, propagation or deduplication rule. Crossings keep the existing
  XI2-over-core deduplication, and the check runs after it: a denied XI2 crossing does not expose a core crossing.
- Reuse first: `fanout_event_to_clients_with_receive`, `emit_window_event_to_state_with_receive`,
  `ServerState::check_receive`, `EventDescription::core`, the plan-1 helpers `xinput::xi1_event_description` /
  `xinput::xi2_event_description`, existing encoders and test drivers. New code only where a stated requirement
  cannot be met otherwise.
- Empty policy list → no policy is called (the existing shortcut inside the checked fanouts and `check_receive`).
- Server-generated notifications have no Send check. The request passed to Receive is `state.receive_request`.
- Check before per-recipient encoding, telemetry and buffering. A denial is not a transport failure and never enters
  a returned disconnected-client vector.
- The cooked pointer and keyboard data paths (button, motion, key, scroll, raw) are out of scope: in a shared writer
  only the crossing branch gets the check.
- No environment variable, no `cfg(test)` in production bodies. Xorg is MIT: read it, do not copy code.
- Tests drive `process_request` or the real entry point. Crossings: feed the explicit Enter/Leave events through
  `pointer_event_fanout_to_state`, as `pointer_fanout/tests.rs` (~3704) does (the KMS backend produces the chain
  before MotionNotify), plus a MotionNotify where the sprite position is asserted. Screen resize: `RRSetScreenSize`
  through `process_request`. Barriers: relative motion through `pointer_event_fanout_to_state`, so the barrier state
  is computed by the real motion path. Never call an emitter directly to prove a state effect. Every new test names its compiling mutation in a comment; run it, observe the failure, restore. Suites
  run under CPU load. Test files stay below 6000 lines.
- Test rules learned in plan 1 (binding):
  - Byte parity is pinned against base behavior: for every event a task touches, one no-policy test asserts the
    complete literal output of each recipient stream, with the server timestamp pinned to zero
    (`state.start_instant = Instant::now() + Duration::from_secs(30)`), at least one big-endian recipient, distinct
    recipient sequence numbers, and the surrounding frames in order. It passes before and after the implementation.
  - Allow controls install an explicit allow-all recording policy; Allow and denial runs compare complete streams
    against those literal frames, never against another run of the new code.
  - A policy that denies one call matches hook, subject and description before counting or denying; request
    admission and other hooks are allowed.
  - After each driven request or entry: `RequestOutcome::Handled` where a request drives it, no unexpected error in
    the requester's stream, `client_io::failed_writers` empty, and the operation's state effect present (focus moved,
    grab active or released, barrier state, screen size), compared with the Allow run.
  - Expected descriptions are written out with `EventDescription::core(..)` / `EventDescription::extension(..)` from
    constants, not through production helpers. Red steps fail on assertions, not compile errors.
  - Reuse existing test builders and drivers; no new frame splitters where whole-stream literal equality suffices.
- Each task is one reviewable commit, signed (`-S`), with `cargo +nightly fmt` and
  `cargo clippy --all-targets -- -D warnings` before it. No push or merge is authorized by this plan.

## Review Focus

1. **A client selecting core and XI2 crossings on the same window**: today it receives only XI2. Denying the XI2
   crossing must leave it with nothing, not a core crossing. Task 3 `receive_xi2_crossing_denial_keeps_core_dedup`.
2. **A crossing chain through several windows** with one window's recipient denied: the other windows' crossings
   arrive in order. Task 2 `receive_core_crossing_chain_denies_one_window`.
3. **Grabbed crossing to the grab owner**: checked against the crossing window (a yserver rule); denial keeps the
   grab. Task 2 `receive_core_grabbed_crossing_owner_denied`.
4. **XI1 FocusIn with a selected DeviceStateNotify**: denying the state batch keeps FocusIn, and denying FocusIn keeps
   the state batch. Task 4 `receive_xi1_focus_and_state_batches_independent`.
5. **Overlay resize**: the overlay's StructureNotify copy and its parent's SubstructureNotify copy are checked with
   different subjects; denying one keeps the other. Task 1 `receive_cow_resize_subjects_independent`.

## Site table

`C/` = `crates/yserver-core/src/core_loop/`, `P/` = `C/process_request/`. `W(x)` = `EventSubject::Window(x)`.
**Drop** = suppress only this copy for the denied recipient; the operation continues unchanged.

| ID / task | Event | Site (line at `bb729047`) | Recipients (unchanged) | Subject | Description | Xorg 21.1.24 |
|---|---|---|---|---|---|---|
| R1 / 1 | core ConfigureNotify | `P/randr_ext.rs:send_root_config_notify` (133) | root StructureNotify subscribers | W(ROOT) | `core(22)` | `randr/rrscreen.c:71-88`; `dix/events.c:2999-3004` |
| R2 / 1 | core ConfigureNotify | `C/run.rs:emit_screen_resize_window_notifications` (3984) | root StructureNotify subscribers | W(ROOT) | `core(22)` | same |
| R3 / 1 | core ConfigureNotify | same (4025) | overlay StructureNotify subscribers | W(overlay) | `core(22)` | `composite/compwindow.c:194-217`; `dix/window.c:2383-2390` |
| R4 / 1 | core ConfigureNotify | same (4042) | overlay parent SubstructureNotify subscribers | W(overlay parent) | `core(22)` | same |
| C1 / 2 | core Enter/Leave | `C/pointer_fanout.rs:fanout_core_pointer_event_ordered` (4097), crossing kinds only; targets from `core_crossing_targets` (3915) | crossing-window selectors, or the grab owner per the existing crossing-mask rule | W(crossing window) | `core(7)` / `core(8)` | ungrabbed `dix/events.c:4730-4805`; Xorg has no hook on the grabbed leg, so checking it is a yserver rule |
| C2 / 2 | core Enter/Leave, NotifyGrab/Ungrab | `P/grabs.rs:emit_core_pointer_grab_chain` (1150) | each chain window's subscribers | W(chain window) | `core(7)` / `core(8)` | chain order `dix/events.c:1615-1647, 1693-1713`; yserver's subscriber fanout differs from Xorg, which can still have the old grab installed and write directly (`:4777-4783`) |
| C3 / 3 | XI2 Enter/Leave | `C/pointer_fanout.rs` XI2 writer (2742), `is_crossing_evt` branch only | existing XI2 crossing recipients | W(ev_win) | `extension(35,137,7/8)` | ungrabbed `dix/events.c:4809-4883`; grabbed leg is a yserver rule |
| C4 / 3 | XI2 Enter/Leave, NotifyGrab/Ungrab | `P/grabs.rs` XI2 chain (1189) | master-pointer selectors per chain window | W(chain window) | `extension(35,137,7/8)` | as C2; Xorg's grabbed XI2 write is direct (`dix/events.c:4868`) |
| C5 / 3 | XI2 Enter/Leave/FocusIn/FocusOut for XIGrabDevice / XIUngrabDevice | `P/xi/dispatch/grabs.rs` (337, 553) | the current / former grab owner | W(target_window) | `extension(35,137,evtype)` | yserver producer; subject is a spec ruling |
| F1 / 4 | XI2 FocusIn/FocusOut | `C/fanout.rs:emit_xi2_focus_event_to_state` (434) | master keyboard and wildcard selectors | W(window) | `extension(35,137,9/10)` | `dix/enterleave.c:767-836` |
| F2 / 4 | XI1 DeviceFocusIn/Out | `C/xi1_focus.rs:emit_device_focus` (568) | exact class selectors on the window | W(window) | `xi1(6/7)` | `dix/enterleave.c:767-836` |
| F3 / 4 | XI1 DeviceStateNotify batch | `C/xi1_state_notify.rs:deliver_state_notify` (82) | state selectors on the focus window | W(window) | one batch: `xi1(10)`, then `xi1(13)` if a key-state continuation is encoded, `xi1(0)` if a valuator continuation is encoded, in encoded order | `dix/enterleave.c:698-763` |
| B1 / 5 | XI2 BarrierHit/Leave | `C/pointer_fanout.rs:emit_barrier_event` (4866) | the matching barrier-owner grab, else barrier-window selectors | W(barrier window) | `extension(35,137,25/26)` | `Xi/exevents.c:1709-1748`; yserver chooses the grabbed owner directly |

`xi1(n)` = `extension(XI_FIRST_EVENT + n, 137, n)`. yserver produces no `KeymapNotify`, so Xorg's KeymapNotify after
Enter/FocusIn has no site here. Out of scope: cooked and raw pointer/keyboard data, smooth scroll, replay (later
plans); native XKB (plan 3).

Task paths: production relative to `crates/yserver-core/src/`; `tests/` = `core_loop/process_request/tests/`.
New tests go in `tests/access_crossing_notify.rs` (register in `tests/mod.rs`).

### Task 1: Root and COW ConfigureNotify

**Files:** Modify `core_loop/process_request/randr_ext.rs` (R1), `core_loop/run.rs` (R2-R4); create
`tests/access_crossing_notify.rs`.

**Interfaces:** Each site switches `emit_window_event_to_state` to `emit_window_event_to_state_with_receive` with
`EventDescription::core(22)`; that helper already uses `W(window)` of the window passed. R4 passes the overlay parent
as before.

- [ ] **Step 1: Write the tests:**
  - `receive_screen_config_baseline_frames` (no policy): `RRSetScreenSize` through `process_request`, with a root
    StructureNotify selector, and one client (big-endian) selecting both overlay StructureNotify and parent
    SubstructureNotify; plus RRSetMonitor driving R1. Pin each stream literally (void requests: no reply). Passes
    before and after. `// mutation: swap R3 and R4` (visible on the dual selector)
  - `receive_cow_resize_subjects_independent`: deny the dual selector's `W(parent)` call only; it gets the R3 copy
    and no R4 copy, literal; screen size and COW geometry changed. Second run denies `W(overlay)`.
    `// mutation: pass the overlay as R4's window`
  - `receive_screen_resize_root_denial`: deny the root selector's `W(ROOT)` call in a resize; it gets no R2 copy;
    the COW copies arrive literally; screen size changed. `// mutation: leave R2 unchecked`
  - `receive_root_config_notify_denial`: RRSetMonitor (void) driving R1 with two root selectors, one denied; the other's
    stream equals its literal baseline; the requester stream is exactly its expected literal output with no reply;
    the monitor is present afterwards. `// mutation: leave R1 unchecked`
- [ ] **Step 2: Run** `cargo test -p yserver-core access_crossing_notify` — Expected: the two denial tests FAIL on
  assertions; the baseline passes.
- [ ] **Step 3: Implement** R1-R4.
- [ ] **Step 4: Run** the tests and the crate suite under load — Expected: PASS.
- [ ] **Step 5: Commit** `feat(access): Receive on root and COW ConfigureNotify`.

### Task 2: Core crossings

**Files:** Modify `core_loop/pointer_fanout.rs` (C1), `core_loop/process_request/grabs.rs` (C2); test
`tests/access_crossing_notify.rs`.

**Interfaces:** In `fanout_core_pointer_event_ordered`, when `event.kind` is EnterNotify/LeaveNotify the immediate
write goes through `fanout_event_to_clients_with_receive` with `W(window)` and `core(7)`/`core(8)`; other kinds keep
the unchecked call unchanged. Crossings are never deferred today, so no deferred path is touched. C2 switches to
`emit_window_event_to_state_with_receive` with the matching description.

- [ ] **Step 1: Write the tests:**
  - `receive_core_crossing_baseline_frames` (no policy): explicit Leave/Enter events, then a MotionNotify, through
    `pointer_event_fanout_to_state` across a parent/child pair with Enter/Leave selectors on both (one big-endian); and a GrabPointer / UngrabPointer
    pair producing the NotifyGrab/NotifyUngrab chain. Pin each stream literally. `// mutation: reverse chain order`
  - `receive_core_crossing_chain_denies_one_window`: deny `W(child)` for one recipient; the parent's crossings and
    the other recipient's stream equal their literal baselines; the sprite window moved.
    `// mutation: leave C1 unchecked`
  - `receive_core_grabbed_crossing_owner_denied`: an active pointer grab whose owner selects crossings; deny the
    owner's crossing check; no crossing reaches it; the grab stays active; a following button press still reaches
    the owner unchanged (cooked data is not checked). `// mutation: check the grab window instead of the crossing window`
  - `receive_core_grab_chain_denial`: GrabPointer with chain-window subscribers; deny one chain window; the grab
    reply is Success and the grab is active; the other windows' NotifyGrab crossings equal their literals.
    `// mutation: leave C2 unchecked`
  - `receive_core_cooked_pointer_unchecked`: under a deny-all Receive policy a ButtonPress still reaches its
    selector and no Receive call is recorded for it (scope guard). `// mutation: check every kind in C1's writer`
- [ ] **Step 2: Run** `cargo test -p yserver-core access_crossing_notify` — Expected: denial tests FAIL; baseline and
  scope-guard tests pass.
- [ ] **Step 3: Implement** C1 and C2. Correct the dedup comment near `pointer_fanout.rs:1817`: the per-client
  XI2-over-core shadowing is yserver's rule (Xorg ends a device's walk at the first delivering form), and the
  grab-chain comment near `grabs.rs:1178` to say the subscriber fanout is yserver's.
- [ ] **Step 4: Run** the tests, `cargo test -p yserver-core pointer_fanout` and the crate suite under load — PASS.
- [ ] **Step 5: Commit** `feat(access): Receive on core crossings`.

### Task 3: XI2 crossings and XI grab notifications

**Files:** Modify `core_loop/pointer_fanout.rs` (C3), `core_loop/process_request/grabs.rs` (C4),
`core_loop/process_request/xi/dispatch/grabs.rs` (C5); test `tests/access_crossing_notify.rs`.

**Interfaces:** C3: in the XI2 writer, the `is_crossing_evt` branch writes through
`fanout_event_to_clients_with_receive` with `W(ev_win)` and `xi2_event_description(xi2_evtype)`; the cooked branch
stays unchecked. C4 and C5 use the checked fanout with `W(window)` / `W(target_window)`.

- [ ] **Step 1: Write the tests:**
  - `receive_xi2_crossing_baseline_frames` (no policy): XI2 crossings from explicit Enter/Leave events, the XI2
    NotifyGrab chain from GrabPointer, and the XIGrabDevice / XIUngrabDevice synthetic crossings and focus events.
    Pin literally. `// mutation: drop the focus flag from the XI2 crossing encoder call`
  - `receive_xi2_crossing_denial_keeps_core_dedup`: one client selects core and XI2 Enter on the child; deny its XI2
    Enter. It receives neither form; a second XI2-only client gets its literal frame. `// mutation: when the dual
    selector's XI2 copy is denied, deliver its core copy`
  - `receive_xi2_grab_chain_denial`: as Task 2's chain test for C4. `// mutation: leave C4 unchecked`
  - `receive_xi_grab_device_synthetic_denial`: XIGrabDevice then XIUngrabDevice with the owner's synthetic crossings
    denied: grab reply Success, grab active then released, no synthetic events, other events unchanged.
    `// mutation: leave the ungrab site (553) unchecked`
- [ ] **Step 2: Run** `cargo test -p yserver-core access_crossing_notify` — denial tests FAIL; baseline passes.
- [ ] **Step 3: Implement** C3-C5.
- [ ] **Step 4: Run** the tests and the crate suite under load — PASS.
- [ ] **Step 5: Commit** `feat(access): Receive on XI2 crossings and XI grab notifications`.

### Task 4: XI2 and XI1 focus, and the XI1 state batch

**Files:** Modify `core_loop/fanout.rs` (F1), `core_loop/xi1_focus.rs` (F2), `core_loop/xi1_state_notify.rs` (F3);
test `tests/access_crossing_notify.rs`.

**Interfaces:** F1, F2 use the checked fanout with `W(window)`. F3 builds its description batch from the same
continuation decisions the encoder uses (lead, key state, valuator, in encoded order) and checks it once per
recipient; denial drops the whole chain for that recipient.

- [ ] **Step 1: Write the tests:**
  - `receive_focus_baseline_frames` (no policy): SetInputFocus producing core and XI2 focus events, and
    SetDeviceFocus on a keyboard and on a pointer device producing XI1 DeviceFocus plus DeviceStateNotify chains
    (key-state continuation; valuator continuation needs a device with more than three valuators: use a physical
    pointer facet with four valuators, not the built-in two-valuator pointer). Pin literally.
    `// mutation: emit the state batch before DeviceFocusIn`
  - `receive_xi2_focus_denial`: deny one XI2 FocusIn recipient; core FocusIn (4b) and other recipients unchanged;
    focus moved. `// mutation: leave F1 unchecked`
  - `receive_xi1_focus_and_state_batches_independent`: run 1 denies the DeviceFocusIn check, run 2 the state batch
    check. In each, the other arrives literally. For both the keyboard and the four-valuator pointer, the recorded
    Receive calls per recipient are exactly [DeviceFocusIn, state batch], the batch call carrying the full
    description list of its chain. `// mutation: describe only the lead DeviceStateNotify in F3`; second: check the
    batch once per event instead of once per recipient
- [ ] **Step 2: Run** `cargo test -p yserver-core access_crossing_notify` — denial tests FAIL; baseline passes.
- [ ] **Step 3: Implement** F1-F3.
- [ ] **Step 4: Run** the tests, `cargo test -p yserver-core xi1` and the crate suite under load — PASS.
- [ ] **Step 5: Commit** `feat(access): Receive on XI focus and device state notifications`.

### Task 5: Barrier notifications

**Files:** Modify `core_loop/pointer_fanout.rs` (B1); test `tests/access_crossing_notify.rs`.

**Interfaces:** `emit_barrier_event` uses the checked fanout with `W(barrier_window)` and
`xi2_event_description(evtype)` for both the grabbed-owner and the selector branch.

- [ ] **Step 1: Write the tests:**
  - `receive_barrier_baseline_frames` (no policy): a pointer barrier hit and leave, ungrabbed and with the owner's
    grab, driven by relative motion. Pin literally. `// mutation: swap BarrierHit and BarrierLeave evtypes`
  - `receive_barrier_denial_keeps_barrier_state`: deny the selector's BarrierHit; it gets nothing, the pointer is
    still constrained and a later BarrierLeave is checked again; under the owner's grab, deny the owner: no event,
    grab and barrier unchanged. `// mutation: leave B1 unchecked`
- [ ] **Step 2: Run** `cargo test -p yserver-core access_crossing_notify` — denial test FAILS; baseline passes.
- [ ] **Step 3: Implement** B1.
- [ ] **Step 4: Run** the tests, the full crate suite under load, every `tests/*.rs` integration target, the
  `--ignored` workspace suite under the lavapipe environment (the CI Vulkan lane, `.github/workflows/ci.yml` ~131;
  Vulkan tests that skip without a device do not count as evidence) and `cargo clippy --all-targets -- -D warnings`
  — PASS.
- [ ] **Step 5: Commit** `feat(access): Receive on XI barrier notifications`.
