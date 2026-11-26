# Access control layer, PR 4c plan 3 — native XKB events Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Check Receive on every native XKB event yserver emits. Byte output must not change when no policy is
installed.

**Architecture:** The 12 native XKB emission expressions all have fixed recipient lists. Each one switches from
`fanout_event_to_clients` to `fanout_event_to_clients_with_receive`, which takes:
- the subject `EventSubject::Device(<device id in the event payload>)`;
- a one-event batch described as `EventDescription::extension(xkb_event_base, XKB_MAJOR_OPCODE, 0)`. That is the same
  description `describe_wire_event` gives a `SendEvent` of an XKB event.

A denial drops that recipient's copy only. These stay exactly as they are:
- the keymap, control, indicator and state change;
- the recipient lists, including the list `send_xkb_map_notify` returns;
- the announced-state snapshots;
- the core `MappingNotify` legacy fanout. Its recipients come from selection masks, not from deliveries, so a client
  whose XKB MapNotify is denied still gets exactly the core MappingNotify it gets today.

**Tech Stack:** Rust, crate `yserver-core`; existing encoders, recording backend and access policy; std only.

**Spec:** `docs/superpowers/specs/2026-10-30-access-control-layer-design.md`, "Input delivery under denial" →
**Notifications** (last bullet: "An XKB event has the device as its subject. These two checks are yserver rules."),
"Who is checked", and the event-description rule used by `describe_wire_event`.

**Reference:** Xorg (MIT) **xorg-server 21.1.24**, `/data/Projects/xorg-21.1.24`. Xorg writes native XKB events
with no Receive hook (`xkb/xkbEvents.c:252`). The checks in this plan are a yserver rule, and every comment says so.

**Source survey:** `/data/Projects/claude-scratch/access-control/pr4c-v2/survey.md` §2 "Native XKB and inherited
boundaries". Line numbers below are navigation anchors at `e811c098` (plan 2 head).

## Sites

| Id | Site | Event | Recipients (unchanged) |
|----|------|-------|------------------------|
| K1 | `core_loop/xkb_layout.rs:145` `apply_rules_names_change` | NewKeyboardNotify | `subscribers(state, 0x0001)` |
| K2 | `core_loop/process_request/xkb.rs:314` (XkbGetKbdByName) | NewKeyboardNotify | `all` clients (today's list, XKB-initialised or not) |
| K3 | `core_loop/process_request/xkb.rs:615` `send_xkb_set_events` | NewKeyboardNotify | `new_keyboard_recipients` |
| K4 | `core_loop/xkb_layout.rs:185` `send_xkb_map_notify` | MapNotify | `map_notify_recipients` (still returned) |
| K5 | `core_loop/xkb_layout.rs:257` `send_xkb_compat_map_notify` | CompatMapNotify | compat interest |
| K6 | `core_loop/xkb_layout.rs:271` `send_xkb_names_notify` | NamesNotify | names interest |
| K7 | `core_loop/xkb_layout.rs:301` `send_indicator_names_change` | ExtensionDeviceNotify | extension-device interest |
| K8 | `core_loop/xkb_layout.rs:377` `send_led_extension_device_notify` | ExtensionDeviceNotify | extension-device interest |
| K9 | `core_loop/xkb_layout.rs:413` `apply_keyboard_mapping_repeats` | ControlsNotify (per-key repeat) | controls interest |
| K10 | `core_loop/xkb_layout.rs:462` `send_indicator_notify` | IndicatorMap/StateNotify | indicator interest |
| K11 | `core_loop/xkb_layout.rs:605` `announce_xkb_state` | StateNotify | state interest |
| K12 | `core_loop/xkb_controls.rs:184` `send_controls_notify` | ControlsNotify | controls interest |

The subject is `EventSubject::Device(<the device id the encoder writes>)`:
- **K1–K3:** the literal `1` they pass as the device byte today.
- **K4–K12:** `u16::from(notify.device_id)` of the notify struct they encode. Production callers pass
  `XKB_DEVICE_ID` (1) today; the existing K4 helper test passes `3`, and the subject follows the payload.

Unchanged by this plan:
- The core `MappingNotify` copies, including those sent by `xkb_select::send_legacy_map_notify` and
  `send_legacy_core_map_notify`, stay exempt (4b).
- That helper's XI1 `DeviceMappingNotify` companions already run Receive (plan 1).
- `send_xkb_map_notify`'s return value stays the selected list. No production caller reads it; its unit test at
  `xkb_layout.rs:818-845` stays unmodified.

## Global Constraints

- **No-policy parity.** With no policy installed, or with every check allowed, every byte a client observes stays
  the same: events, replies, errors, their order, sequence numbers and multiplicity. The existing suite passes
  unmodified, except `receive_deny_all_keeps_later_extension_writers` (Task 2 states the one change).
- **No new rules.** No new recipient selection, event production or deduplication rule.
- **Snapshots.** `state.xkb_announced` and `state.xkb_announced_indicators` advance before the fanout, as today.
- **Reuse first:**
  - Use the existing helpers: `fanout_event_to_clients_with_receive`, `EventBatch::new`, `EventSubject::Device`, the
    existing encoders, and the test fixture `NotifyFixture` (`tests/access_notify_support.rs`).
  - Add exactly these things, all in Task 1:
    1. the description helper;
    2. two scripted results on the `RecordingBackend` test double: `xkb_set` (the test backend has no other way to
       reach `send_xkb_set_events` through `process_request`) and `xkb_indicators` (needed to make a StateNotify
       carry pending indicator notifications).
- **Description helper.** `pub(crate) fn xkb_event_description(xkb_event_base: u8) -> EventDescription` lives in
  `core_loop/xkb_layout.rs`.
  - It returns `EventDescription::extension(xkb_event_base, crate::nested::XKB_MAJOR_OPCODE, 0)`.
  - `XKB_MAJOR_OPCODE` becomes `pub(crate)`.
  - Its doc comment says it supplies the native XKB metadata, and that for any wire code the live registry resolves
    to XKB it matches `describe_wire_event` (XKB has one event code; the `xkbType` byte is not part of the
    description). It does not claim equivalence for the base-zero fallback (`xkb_info()` absent), which
    `describe_wire_event` classifies as `core(0)`.
- **No-policy shortcut.** With an empty policy list, no policy is called. That is the shortcut that already exists
  inside the checked fanout.
- **Request context.**
  - These are server-generated notifications, so there is no Send check.
  - The request passed to Receive is `state.receive_request`. Request-driven sites carry the request, and the
    physical-key path of K11 (`key_fanout.rs:473`) carries `None`.
- **Check placement.** Check before encoding each recipient's copy. A denial is not a transport failure and never
  enters a returned disconnected-client vector.
- **Code rules.** No environment variable, and no `cfg(test)` in production bodies. Xorg is MIT: read it, do not
  copy code.
- **Test drivers.** Tests drive `process_request` (through `NotifyFixture::request`) or the real entry point named in
  each task. Never call an emitter directly to prove a state effect.
- **Mutations.** Every new test names its compiling mutation in a comment. Run it, observe the failure, and restore.
- **Test environment.** Suites run under CPU load. The new test file stays below 6000 lines.
- **Test rules learned in plans 1 and 2 (binding):**
  - **Byte parity is pinned against base behavior.** For every event a task touches, one no-policy test asserts the
    complete literal output of each recipient stream. It pins the server timestamp to zero
    (`state.start_instant = Instant::now() + Duration::from_secs(30)`), uses distinct recipient sequence numbers whose
    high and low bytes differ, and includes the surrounding frames in order. It passes before and after the
    implementation.
  - **Byte order.** XKB refuses big-endian clients, so XKB selectors are little-endian. K2 also reaches non-XKB
    clients, and its baseline includes one big-endian non-XKB recipient.
  - **Allow controls.** Allow controls install an explicit allow-all recording policy. Allow and denial runs compare
    complete streams against those literal frames, never against another run of the new code.
  - **Deny predicates.** A policy that denies one call matches hook, subject and description before counting or
    denying. Request admission and other hooks are allowed.
  - **Per-request checks.** After each driven request:
    - `RequestOutcome::Handled`;
    - the requester's stream holds exactly the expected reply or error, if any;
    - `client_io::failed_writers` is empty;
    - the state effect is present, compared with the Allow run: the keymap override cleared, the mods, the repeat
      bits, `xkb_announced`.
  - **Hook log.** The whole-log Receive count and the absence of any Send call are asserted before the log is
    filtered by client.
  - **Red gates.** The module accumulates tests across tasks. At each task's red step, exactly the denial tests that
    task adds fail. Its baseline and every earlier test pass.
  - **Expected descriptions.** Write them as `EventDescription::extension(85, 136, 0)` with
    `backend.xkb_info_result = Some((136, 85, 0))`, from constants, not through the production helper.
  - **Red steps fail on assertions,** not on compile errors.
  - **Every test names a compiling mutation** in its comment, including the byte baselines (mutate one encoded field
    at the site, e.g. the device byte or the request minor) and the Allow controls (drop the site's check, so the
    Receive count falls). The per-task lists below are the minimum, not the whole set.
  - **Builders.** Reuse existing test builders and the `xkb_request`-style drivers. Use one literal builder per frame
    layout and one definition per expected stream.
- **Commits.** Each task is one reviewable commit, signed (`-S`), with `cargo +nightly fmt` and
  `cargo clippy --all-targets -- -D warnings` before it. No push or merge is authorized by this plan.

## Review Focus

1. **A denied MapNotify leaves the core MappingNotify exactly as today.**
   - Setup: a keysym change through a request. Two XKB clients: one selected the changed MapNotify detail (Xorg
     `xkb/xkbEvents.c:84` gives it the core MappingNotify too), one did not select it (no core copy today).
   - Expected: denying the first client's XKB MapNotify removes only that frame. Each client's core MappingNotify
     frames match the no-policy baseline exactly: the first keeps its core copy, the second still gets none.
   - Test: Task 2 `receive_xkb_map_denied_keeps_legacy_filter`.
2. **K2 reaches clients that never initialised XKB.** Its denial and baseline cover such a client, including a
   big-endian one. Test: Task 2 `receive_xkb_kbd_by_name_all_clients`.
3. **A denied StateNotify still advances the snapshots and keeps the indicator follow-ups.**
   - Setup: every selected StateNotify copy is denied, while the same request also changes the lit indicators.
   - Expected: right after that request, `state.xkb_announced` equals the new state and
     `state.xkb_announced_indicators` equals the new lit mask. The IndicatorStateNotify and ExtensionDeviceNotify
     selectors still get their literal frames. A repeated request cannot show this, because the handler resyncs
     `xkb_announced` first (`process_request/xkb.rs:234`).
   - Tests: Task 1 `receive_xkb_state_denied_snapshot_advances` (state snapshot), Task 3
     `receive_xkb_state_indicator_followups` (indicator snapshot and follow-up frames).
4. **One request emits several XKB events.** In the IndicatorMaps change, every XKB event is checked on its own, in
   today's order, and denying one leaves the others: IndicatorMapNotify, then IndicatorStateNotify, then
   ExtensionDeviceNotify. Test: Task 3 `receive_xkb_indicator_maps_denies_one_kind`.
5. **The physical-key path** of K11 carries no request context. Test: Task 1
   `receive_xkb_state_from_key_has_no_request`, driven through `key_event_fanout_to_state` with
   `RecordingBackend::xkb_mods` seeded before the press, as `key_fanout.rs:2403`
   (`modifier_change_emits_xkb_state_notify_with_real_mods`) does.

---

### Task 1: Helper, test backend hook, StateNotify and ControlsNotify (K9, K11, K12)

**Files:**
- Modify: `crates/yserver-core/src/nested.rs:50` — make `XKB_MAJOR_OPCODE` `pub(crate)`.
- Modify: `crates/yserver-core/src/core_loop/xkb_layout.rs` — add `xkb_event_description`, and change K9 and K11.
- Modify: `crates/yserver-core/src/core_loop/xkb_controls.rs:184` — K12.
- Modify: `crates/yserver-core/src/backend/recording.rs` — add the scripted `xkb_set` result.
- Create: `crates/yserver-core/src/core_loop/process_request/tests/access_xkb_notify.rs`, and register it in
  `tests/mod.rs`.

**Interfaces:**
- Produces:
  - `pub(crate) fn xkb_event_description(xkb_event_base: u8) -> EventDescription` in `core_loop/xkb_layout.rs`.
  - `pub xkb_set_outcome: Option<crate::backend::XkbSetOutcome>` on `RecordingBackend`, with
    `#[must_use] pub fn with_xkb_set_outcome(self, outcome: XkbSetOutcome) -> Self`.
  - `RecordingBackend::xkb_set` returns `self.xkb_set_outcome.take()`, and otherwise `None`, as today. If needed,
    `XkbSetOutcome` derives `Clone`/`Debug`.
  - `pub xkb_indicators_result: (u32, u32)` on `RecordingBackend` (default: what `xkb_indicators` returns today),
    returned by `RecordingBackend::xkb_indicators` as `(lit, defined)`.
- Test drivers:
  - **K11:** XkbLatchLockState (minor 5). The `select_state_details` and `lock_mods_body` shapes are in
    `tests/xkb.rs:508-530`; copy them into the new file or move them to a shared helper.
  - **K12:** XkbSetControls (minor 7).
  - **K9:** an XkbSet* request whose scripted outcome is `XkbSetEvent::Notification` with non-empty `repeats` that
    change the core per-key bits.

- [ ] **Step 0: Add the two `RecordingBackend` scripted results** (test double only, no production change). Run
  `cargo test -p yserver-core` → all pass; this keeps the Step 2 red gate about Receive, not about missing fixtures.
- [ ] **Step 1: Write the baseline and denial tests**
  - `receive_xkb_state_no_policy_bytes` covers K11, K12 and K9. Two XKB selectors with distinct sequences each get
    the literal frames in order. The requester gets no reply.
  - `receive_xkb_state_denied_one_recipient` (K11): the denied selector gets nothing, the other gets the literal
    frame, and `backend.xkb_mods` changes. The Receive log holds one call per selector with
    `EventSubject::Device(1)`, `EventDescription::extension(85, 136, 0)` and the request context.
  - `receive_xkb_state_denied_snapshot_advances` (Review Focus 3, StateNotify part): deny every selected StateNotify
    copy of a LatchLockState; right after that request `state.xkb_announced` equals the new state. The indicator
    follow-ups are Task 3's test, because their Receive calls only exist after Task 3.
  - `receive_xkb_state_from_key_has_no_request` (Review Focus 5): a key press through the existing key fanout entry.
    The StateNotify Receive call has request `None`.
  - `receive_xkb_controls_denied_one_recipient` (K12) and `receive_xkb_per_key_repeat_denied` (K9). In each, the
    controls change applies: the repeat bits and `enabled_controls` match the Allow run.
  - `receive_xkb_state_allow_control`: an allow-all policy gives the literal baseline frames, with the expected call
    count.
  - Mutations to name: leave K11 unchecked; check K11 with `EventSubject::Global`; describe with event type 2
    (`xkbType`); move the snapshot update after a denial short-circuit; pass the requester's request to the
    key-path check instead of `state.receive_request`; leave K12 unchecked; leave K9 unchecked.
- [ ] **Step 2: Run** `cargo test -p yserver-core access_xkb_notify`. Expected: the baseline passes. Every other
  Task 1 test fails on assertions: the denial tests, the Allow control (its Receive call count) and
  `receive_xkb_state_from_key_has_no_request` (no Receive call yet).
- [ ] **Step 3: Implement** the helper and K9, K11 and K12 through
  `fanout_event_to_clients_with_receive`. Build the batch once per emission, outside the per-recipient loop.
- [ ] **Step 4: Run** the module, then `cargo test -p yserver-core`. Expected: all pass.
- [ ] **Step 5: Commit** `feat(access): Receive on XKB state and controls notifications`.

### Task 2: NewKeyboardNotify and MapNotify (K1–K4)

**Files:**
- Modify: `core_loop/xkb_layout.rs:145, 185` (K1, K4) and `core_loop/process_request/xkb.rs:314, 615` (K2, K3).
- Modify: `tests/access_notify_exemptions.rs` — `receive_deny_all_keeps_later_extension_writers`, XKB part only.
  Under deny-all, the XKB NewKeyboardNotify is now dropped. The core MappingNotify pair and the XkbGetKbdByName reply
  still arrive, byte for byte, in their order. Assert the remaining stream as literal frames, and the Receive call for
  the NewKeyboardNotify with its subject and description.
- Test: `access_xkb_notify.rs`.

**Interfaces:**
- Consumes: `xkb_event_description` and `with_xkb_set_outcome` (Task 1).
- Test drivers:
  - **K1 and K4 (whole keymap):** ChangeProperty `_XKB_RULES_NAMES` on the root through `process_request`
    (`props.rs:1191`), with `RecordingBackend::with_keymap_rmlvo_result`.
  - **K2:** XkbGetKbdByName (minor 23) with `with_kbd_by_name_result`. `run_xkb_mapping` in
    `access_notify_exemptions.rs:753` shows the setup.
  - **K3 and K4 (partial):** an XkbSet* request with a scripted outcome holding `NewKeyboard` and a `Notification`
    whose `map_notify.changed != 0`.

- [ ] **Step 1: Write the tests**
  - `receive_xkb_new_keyboard_no_policy_bytes`: K1–K4, literal frames for every recipient. The surrounding core
    MappingNotify frames are in order.
  - `receive_xkb_kbd_by_name_all_clients` (Review Focus 2): denial for one non-XKB client. The others, including a
    big-endian one, get the literal NewKeyboardNotify.
  - `receive_xkb_rules_change_denies_new_keyboard_and_map`: the denied client gets neither event, and
    `state.keymap_overrides` is cleared in both runs.
  - `receive_xkb_set_new_keyboard_denied` (K3).
  - `receive_xkb_map_denied_keeps_legacy_filter` (Review Focus 1).
  - `receive_xkb_new_keyboard_allow_control`.
  - `receive_xkb_map_payload_device_subject`: the existing K4 helper-test shape (`xkb_layout.rs:809-845`, notify
    `device_id` 3) with a policy that denies one selector. Asserts the subject `EventSubject::Device(3)` and that the
    returned list still contains the denied selector. This is the helper's own contract, so the test calls the
    helper directly, like the existing unit test.
  - Mutations to name: leave K2 unchecked; leave K3 unchecked; return only allowed recipients from
    `send_xkb_map_notify`; filter the core MappingNotify recipients by the XKB MapNotify Receive
    outcome; check K4 with `EventSubject::Device(1)` instead of `notify.device_id` (caught by a K4 run whose notify
    carries device id 3, through the existing helper test's shape); check K1 with `EventSubject::Global`.
- [ ] **Step 2: Run** the module and `cargo test -p yserver-core access_notify_exemptions`. Expected: exactly the
  Task 2 denial tests and the Allow control fail. The Task 1 tests and the baselines pass.
  `receive_deny_all_keeps_later_extension_writers` fails because XKB output is still delivered; once implemented it
  requires exactly three NewKeyboardNotify Receive calls, one for each client of the existing fixture.
- [ ] **Step 3: Implement** K1–K4.
- [ ] **Step 4: Run** the module, `cargo test -p yserver-core access_notify_exemptions`, then
  `cargo test -p yserver-core`. Expected: all pass.
- [ ] **Step 5: Commit** `feat(access): Receive on XKB NewKeyboard and Map notifications`.

### Task 3: Indicators, ExtensionDevice, Names and CompatMap (K5–K8, K10)

**Files:**
- Modify: `core_loop/xkb_layout.rs:257, 271, 301, 377, 462`.
- Test: `access_xkb_notify.rs`.

**Interfaces:**
- Consumes: Task 1's helper and backend hook.
- Test drivers: XkbSet* requests with scripted outcomes `CompatMap`, `Names`, `IndicatorMaps` (with
  `state_changed != 0`) and `IndicatorNames`.

- [ ] **Step 1: Write the tests**
  - `receive_xkb_indicators_no_policy_bytes`: K5–K8 and K10, literal frames in today's order.
  - `receive_xkb_indicator_maps_denies_one_kind` (Review Focus 4): the target client selects all three
    (IndicatorMap, IndicatorState, ExtensionDevice). The three calls share subject, description and request, so the
    policy denies the target's second matching Receive call (the IndicatorStateNotify). It still gets
    IndicatorMapNotify and ExtensionDeviceNotify, in order. A second client selects ExtensionDevice only; both K8
    calls (target and second client) are asserted.
  - `receive_xkb_state_indicator_followups` (Review Focus 3, indicator part): driven through
    `key_event_fanout_to_state` with `backend.xkb_mods` and `backend.xkb_indicators_result` seeded to a new lit mask
    (the fresh `state.xkb_announced_indicators` differs). Every StateNotify copy is denied. Afterwards
    `state.xkb_announced_indicators` equals the new mask, and the IndicatorStateNotify and ExtensionDeviceNotify
    selectors get their literal frames.
  - `receive_xkb_names_denied`, `receive_xkb_compat_map_denied` and `receive_xkb_indicator_names_denied`, each for one
    recipient.
  - `receive_xkb_indicators_allow_control`.
  - Mutations to name: leave K10 unchecked; check K8 once per emission instead of once per recipient; leave K5,
    K6, K7 unchecked (one each); return early from `announce_xkb_state` when every StateNotify copy is denied.
- [ ] **Step 2: Run** the module. Expected: exactly the Task 3 denial tests and its Allow control fail (its Receive
  count); `receive_xkb_state_indicator_followups` fails on its K8/K10 Receive assertions. Task 1 and 2 tests pass.
- [ ] **Step 3: Implement** K5–K8 and K10.
- [ ] **Step 4: Run** the module, then `cargo test -p yserver-core`. Afterwards run the plan gate: the workspace
  tests, the lavapipe `--ignored` suite, clippy for Linux and FreeBSD, and `tools/check-layout.sh`. Expected: all
  pass.
- [ ] **Step 5: Commit** `feat(access): Receive on XKB indicator, names and compat notifications`.
