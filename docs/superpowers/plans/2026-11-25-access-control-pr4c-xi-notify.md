# Access control layer, PR 4c plan 1 — XI device notifications Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Check Receive on every existing XI device notification (presence, hierarchy, DeviceChanged, mapping,
change-device, device property), without changing any client-observable byte when no policy is installed.

**Architecture:** Each site keeps its current recipients, multiplicity, encoder and order. Fanouts move to the existing
`fanout_event_to_clients_with_receive`. The inline originator copies and the XISelectEvents bootstrap get one local
`check_receive` before the copy is appended. Two small description helpers give the XI event identity. No routing
change, and no new mechanism.

**Tech Stack:** Rust, crate `yserver-core`; existing XI encoders, recording backend and access policy; std only.

**Spec:** `docs/superpowers/specs/2026-10-30-access-control-layer-design.md`, "Input delivery under denial" →
**Notifications** (commit `57a2f411`), plus "What a policy can see", "Who is checked" and "Where the checks go".

**Reference:** Xorg (MIT) **xorg-server 21.1.24**, `/data/Projects/xorg-21.1.24`. The spec binds where it decides;
otherwise follow Xorg. Rules the spec names as yserver rules must never be described as Xorg behavior.

**Source survey:** `/home/ariel_santangelo/Projects/claude-scratch/access-control/pr4c-v2/survey.md` §2 "XI crossing,
focus, and device notifications", taken at `8247002b`. Line numbers below are navigation anchors at `8247002b`.

## Global Constraints

- With no policy installed, or with every check allowed, every byte a client observes stays the same: events,
  replies, errors, their order, sequence numbers, byte order and multiplicity. The existing suite passes unmodified,
  except `access_notify_exemptions.rs::receive_deny_all_keeps_later_extension_writers`, whose XI1 part encoded 4b's
  transitional boundary (XI writers unchecked); Task 3 updates only that part.
- No new recipient selection, event production or propagation rule. Preserve the existing absent-client, duplicate,
  disconnect and failed-write handling. A policy denial is not a transport failure and never enters a returned
  disconnected-client vector.
- Reuse first: `fanout_event_to_clients_with_receive`, `ServerState::check_receive`, the existing encoders, the
  `xi1_client_wants_*` predicates, `NotifyFixture`/`RecordingExtensionPolicy` test support and the `xi_hotplug.rs`
  drivers. New code only where a stated requirement cannot be met otherwise.
- Empty policy list → no policy is called. The existing shortcut inside `fanout_event_to_clients_with_receive` and
  `check_receive` provides this; building the constant `EventBatch` beforehand is allowed. Local checks add no guard
  of their own.
- Server-generated notifications have no Send check. The request opcode passed to Receive is `state.receive_request`,
  which `process_request` already scopes; independent server work passes `None`.
- Check before per-recipient encoding, telemetry and buffering. A denial drops only that recipient's copy; the device,
  hierarchy, mapping or property change, the reply and every other copy are unchanged.
- Core `MappingNotify` stays exempt and on its existing writer, also where an XI request emits it inline.
- No environment variable, no `cfg(test)` in production bodies. Xorg is MIT: read it, do not copy code.
- Tests drive `process_request` or the real entry point (hotplug publishers). Every DENY test asserts: no bytes of that
  event to the denied recipient, byte-identical output to the allowed recipient compared with an all-Allow run, no
  error or disconnect, and the request's effect unchanged: the state change where the request makes one, otherwise
  the existing state preserved (`ChangeKeyboardDevice` changes nothing; a direct `emit_property_change` call only
  publishes an already-applied change). Every test names its compiling mutation in a
  comment; run it, observe the failure, restore. Suites run under CPU load.
- XI frames carry a server timestamp. Every test that compares bytes across runs pins it with the existing technique
  (`fixture.state.start_instant = Instant::now() + Duration::from_secs(30)`, as in `access_notify_support.rs`), so
  the timestamp is zero in both runs.
- Byte parity is pinned against base behavior, not against another run of the new code: for every event a task
  touches, one no-policy test asserts the complete literal frames (all 32 bytes, sequence and byte order included,
  timestamp pinned to zero). The Allow and denial comparisons use those literal frames. Such baseline tests pass
  before and after the implementation; their failure is shown by their named mutation.
- Expected event descriptions in tests are written out with `EventDescription::extension(..)` from the XI constants,
  not through the production helpers, so a wrong helper fails a test and every red step compiles.
- To deny one Receive call, a policy matches the hook (`Receive`), the subject and the description. Never select a
  call by its global index: dispatch and resource checks come first. Request admission is always allowed. Test files stay below 6000 lines.
- Each task is one reviewable commit, signed (`-S`), with `cargo +nightly fmt` and
  `cargo clippy --all-targets -- -D warnings` before it. No push or merge is authorized by this plan.

## Review Focus

1. **A client selecting the same device event on two windows** (hotplug, DeviceChanged): one copy per window today;
   denying one window's subject must keep the other copy. Task 1 `receive_xi_hierarchy_subject_is_selected_window`,
   Task 2 `receive_xi_device_changed_denial_per_window`.
2. **The originator of a mapping/change request**: its selected copy is appended inline before or after the reply;
   denial must keep the reply, the core `MappingNotify` and their order. Task 3
   `receive_xi_mapping_originator_denial_keeps_reply_order` (all three mapping requests) and
   `receive_change_device_notify_originator_denial_keeps_core_mapping`.
3. **A client selecting both XI2 and XI1 property events**: two independent copies; denying one form keeps the other.
   Task 4 `receive_xi_property_forms_checked_independently`.
4. **XISelectEvents bootstrap for both masters** with one master's event denied: the other master's event and the
   request's own effect stay. Task 2 `receive_xi_bootstrap_denies_one_master_only`.
5. **Server-driven hotplug with no request in flight**: the check carries `request: None`. Task 1
   `receive_xi_presence_from_server_work_has_no_request`.

## Site table

`C/` = `crates/yserver-core/src/core_loop/`, `P/` = `C/process_request/`, `X/` = `crates/yserver-core/src/xinput/`.
`W(x)` = `EventSubject::Window(x)`, `D(id)` = `EventSubject::Device(id)`. **Drop** = suppress only this copy for the
denied recipient.

| ID / task | Event | Site (line at `8247002b`) | Recipients (unchanged) | Subject | Rule | Xorg 21.1.24 |
|---|---|---|---|---|---|---|
| X1 / 1 | XI1 DevicePresenceNotify | `X/hotplug.rs:emit_xi1_device_presence` (68) | each selected (client, window) pair | W(selected window) | Drop; continue pairs | `dix/devices.c:344`, `Xi/exevents.c:3300-3312` |
| X2 / 1 | XI2 HierarchyChanged | `X/hotplug.rs:emit_xi_hierarchy_changed` (417) | each (client, window) with `XIAllDevices` | W(selected window) | Drop; continue pairs | `Xi/xichangehierarchy.c:62-119` |
| X3 / 2 | XI2 DeviceChanged | `X/hotplug.rs:emit_xi2_device_changed` (263) | each (client, window) pair | W(selected window) | Drop; continue pairs | `Xi/exevents.c:747-760` |
| X4 / 2 | XI2 DeviceChanged bootstrap | `P/xi/mod.rs:emit_xi2_device_changed_bootstrap` (629 → 650) | the issuing client, one event per selected master | W(ROOT_WINDOW) | Drop that master's event before appending | none: yserver producer, spec subject ruling |
| X5 / 3 | XI1 DeviceMappingNotify | `C/xi1_focus.rs:emit_device_mapping_notify` (626) | aggregate class selectors, minus originator | D(deviceid) | Drop | `dix/inpututils.c:103` (per-window broadcast; device subject is a yserver rule) |
| X5b / 3 | XI1 DeviceMappingNotify after an XKB Set request | `C/xkb_select.rs:send_legacy_map_notify` (430) → X5 with `originator: None` | every matching selector, the XKB requester included | D(deviceid) | Drop; XKB and core notifications keep their existing delivery | `xkb/xkbEvents.c:117-133` (Xorg writes the core notification before its per-window XI1 broadcast) |
| X6 / 3 | XI1 ChangeDeviceNotify | `C/xi1_focus.rs:emit_change_device_notify` (696) | aggregate class selectors, minus originator | D(deviceid) | Drop | none: Xorg returns `BadDevice` (`Xi/chgkbd.c:92`) |
| X7a / 3 | Originator DeviceMappingNotify, inline | `P/xi/dispatch/control.rs` key map (483), button map (841) | the originator, if selected | D(deviceid) | Drop the inline copy; keep reply and order | key map: `xkb/xkbEvents.c:133`; button map: `dix/inpututils.c:103`. The separate originator copy, its order and the device subject are yserver behavior |
| X7b / 3 | Originator DeviceMappingNotify, singleton fanout | `P/xi/dispatch/control.rs` modifier map, `notify_device` closure (640) | the originator, if selected | D(deviceid) | Drop; the closure keeps running before the core `MappingNotify` | `xkb/xkbEvents.c:133`; same yserver note |
| X8 / 3 | Originator ChangeDeviceNotify | `P/xi/dispatch/devices.rs` ChangeKeyboardDevice (663) | the originator, if selected | D(deviceid) | Drop the inline copy; keep core MappingNotify and reply | none (as X6) |
| X9 / 4 | XI2 PropertyEvent | `P/props.rs:emit_property_change` (778) | aggregate exact/`XIAllDevices` selectors | D(deviceid) | Drop | `Xi/xiproperty.c:185-208` (per-window broadcast; device subject is a yserver rule) |
| X10 / 4 | XI1 DevicePropertyNotify | `P/props.rs:emit_property_change` (821) | aggregate exact-class selectors | D(deviceid) | Drop; independent of X9 | same |

Out of scope: `C/fanout.rs:emit_xi2_device_changed_slave_pointer` (471) has only test callers; XI focus, state,
crossing and barrier events belong to the crossings/focus plan; native XKB to its own plan.

Task paths: production paths relative to `crates/yserver-core/src/`; `tests/` = `core_loop/process_request/tests/`.

### Task 1: XI descriptions, presence and hierarchy

**Files:**
- Modify: `xinput/mod.rs` (two helpers), `xinput/hotplug.rs` (X1, X2).
- Create: `tests/access_xi_notify.rs`; register it in `tests/mod.rs`.

**Interfaces:**
- Consumes: `fanout_event_to_clients_with_receive(state, client_ids, subject, &EventBatch, encode) -> Vec<ClientId>`;
  `EventDescription::extension(type_code: u8, major_opcode: u8, event_type: u16)`; `NotifyFixture`;
  the `xi_hotplug.rs` selection and enable drivers (make the ones you need `pub(super)`).
- Produces in `xinput/mod.rs`:
  `pub(crate) fn xi1_event_description(offset: u8) -> EventDescription` →
  `extension(XI_FIRST_EVENT + offset, 137, u16::from(offset))`;
  `pub(crate) fn xi2_event_description(evtype: u16) -> EventDescription` → `extension(35, 137, evtype)`.
  137 is the existing `XI2_MAJOR_OPCODE` constant value; reuse a constant, do not add a new literal elsewhere.

- [ ] **Step 1: Write the failing tests** in `tests/access_xi_notify.rs` (red = assertion failure, not compile error):
  - `receive_xi_hierarchy_subject_is_selected_window`: the receiver selects HierarchyChanged on ROOT and on a child;
    disable a device via the enable driver; the policy denies `W(child)` only. Assert: exactly one HierarchyChanged
    reaches the receiver, byte-identical to the ROOT copy of an all-Allow run; recorded Receive checks name
    `W(ROOT)` and `W(child)`, events `[EventDescription::extension(35, 137, 11)]`; device disabled state unchanged.
    `// mutation: pass W(ROOT_WINDOW) instead of the selected window in X2`
  - `receive_xi_presence_denial_keeps_other_client`: client A selects DevicePresenceNotify on ROOT, client B on a
    child; deny A. B's bytes equal the all-Allow run; A gets none; the recorded (recipient, subject) pairs are
    exactly (A, `W(ROOT)`) and (B, `W(child)`); events
    `[EventDescription::extension(XI_FIRST_EVENT + 15, 137, 15)]`.
    `// mutation: skip the receive check in X1 (use fanout_event_to_clients)`
  - `receive_xi_presence_from_server_work_has_no_request`: call the publisher directly (`publish_facet_disabled`)
    outside `process_request`; assert the recorded check has `request: None`; when driven by the enable request,
    `request` equals `Some` of that request's opcodes.
    `// mutation: hard-code Some(RequestOpcodes) in the check`
- [ ] **Step 2: Run** `cargo test -p yserver-core access_xi_notify` — Expected: the tests compile and FAIL on their assertions (no Receive calls recorded).
- [ ] **Step 3: Implement** the two helpers and switch X1, X2 to the checked fanout with `W(selected_window)`
  (rename `_selected_window`). Build the `EventBatch` once outside the loop. The helpers are added in this step and
  used only by production code.
- [ ] **Step 4: Run** the three tests, `cargo test -p yserver-core xi_hotplug` and the whole crate suite under load.
  Expected: PASS, existing hotplug tests unmodified.
- [ ] **Step 5: Commit** `feat(access): Receive on XI presence and hierarchy notifications`.

### Task 2: DeviceChanged and the XISelectEvents bootstrap

**Files:** Modify `xinput/hotplug.rs` (X3), `core_loop/process_request/xi/mod.rs` (X4); test `tests/access_xi_notify.rs`.

**Interfaces:** Consumes Task 1's `xi2_event_description`. For X4, a local
`state.check_receive(client_id, EventSubject::Window(ROOT_WINDOW), &batch, state.receive_request)` per master,
placed right after the retained-source eligibility check and before `device_changed_class_block`, which already
encodes in the client's byte order; `Decision::Deny(_)` → `continue`. The empty-buffer early return stays, so
with both masters denied nothing is written and the request still succeeds.

- [ ] **Step 1: Write the failing tests:**
  - `receive_xi_device_changed_denial_per_window`: receiver selects DeviceChanged for the master on ROOT and a
    child; trigger a slave switch with the existing `announce_xi2_slave_switch`; deny `W(child)`. One copy arrives,
    equal to the ROOT copy of the all-Allow run; the master's recorded last slave is updated.
    `// mutation: drop the subject loop variable and check W(ROOT_WINDOW)`
  - `receive_xi_bootstrap_denies_one_master_only`: both masters carry a slave's classes; the issuer's XISelectEvents
    selects DeviceChanged on ROOT for `XIAllDevices`. The policy counts only Receive calls whose description is
    DeviceChanged and denies the first of them. Assert two such calls, both `W(ROOT_WINDOW)`; only the second
    master's event arrives, byte-identical to its all-Allow bytes; the selection is stored. Variant denying every
    DeviceChanged Receive call (admission allowed): no event bytes, no error, selection stored.
    `// mutation: check once for the whole buffer instead of per master`
- [ ] **Step 2: Run** `cargo test -p yserver-core access_xi_notify` — Expected: the two new tests FAIL.
- [ ] **Step 3: Implement** X3 as in Task 1 and X4 as stated in Interfaces.
- [ ] **Step 4: Run** the tests, `cargo test -p yserver-core xi` and the crate suite under load — Expected: PASS.
- [ ] **Step 5: Commit** `feat(access): Receive on XI DeviceChanged and its bootstrap`.

### Task 3: Mapping and change-device notifications

**Files:** Modify `core_loop/xi1_focus.rs` (X5, X6), `core_loop/process_request/xi/dispatch/control.rs` (X7, three
sites), `core_loop/process_request/xi/dispatch/devices.rs` (X8); test `tests/access_xi_notify.rs`.

**Interfaces:** Consumes `xi1_event_description` with the existing `XI_DEVICE_MAPPING_NOTIFY_OFFSET` and
`XI_CHANGE_DEVICE_NOTIFY_OFFSET`. X5, X6 and X7b switch their existing fanout in place to the checked fanout with
`D(deviceid)`; X7b's write stays inside `notify_device`, before the core `MappingNotify`. X7a and X8 keep their
`xi1_client_wants_*` predicate and add one local `check_receive(client_id, D(deviceid), &batch,
state.receive_request)`; only `Allow` appends the copy. Core `MappingNotify` writes stay unchecked.

- [ ] **Step 1: Write the failing tests:**
  - `receive_xi_mapping_fanout_device_subject`: two other clients select DeviceMappingNotify for the device; the
    originator changes the device key mapping; deny one. Recorded subject is `D(deviceid)`; the allowed client's
    bytes equal the all-Allow run. `// mutation: use EventSubject::Global in X5`
  - `receive_xi_mapping_originator_denial_keeps_reply_order`: for ChangeDeviceKeyMapping, SetDeviceModifierMapping
    and SetDeviceButtonMapping, the originator selected DeviceMappingNotify and is denied. Its output equals the
    all-Allow output with exactly the DeviceMappingNotify removed: reply, core `MappingNotify` (where emitted) and
    their order unchanged; the mapping is applied. The all-Allow run for SetDeviceModifierMapping on the master
    keyboard is pinned literally to the order DeviceMappingNotify → core `MappingNotify` → reply.
    `// mutation: skip the check at the button-mapping site only`; second mutation for the pinned order: move X7b's
    write into the reply buffer.
  - `receive_change_device_notify_originator_denial_keeps_core_mapping`: ChangeKeyboardDevice with the originator
    and two other clients selecting ChangeDeviceNotify; deny the originator and one other client. Each denied
    client's output equals its all-Allow output minus ChangeDeviceNotify; the allowed client's bytes are unchanged;
    every client still gets the core `MappingNotify`, and the originator its reply; recorded ChangeDeviceNotify
    checks use `D(deviceid)`; no Receive check is recorded for the core `MappingNotify`.
    `// mutation: leave X6 on the unchecked fanout`; second: route the core MappingNotify through the checked fanout
  - Update `receive_deny_all_keeps_later_extension_writers`: keep its literal no-policy baseline unchanged; under
    deny-all, the originator's XI1 `DeviceMappingNotify` is now absent and one Receive call with `D(deviceid)` is
    recorded for it, while the core `MappingNotify` still arrives with no Receive call. Leave its XKB, Present and
    XFIXES assertions unchanged.
  - `receive_xi_mapping_change_baseline_order`: no policy installed. Pin complete literal frames:
    SetDeviceButtonMapping → reply then DeviceMappingNotify; ChangeKeyboardDevice → ChangeDeviceNotify, core
    `MappingNotify`, reply; SetDeviceModifierMapping on the master keyboard → DeviceMappingNotify, core
    `MappingNotify`, reply. Passes before and after the implementation.
    `// mutation: append X7a's button-map copy before the reply`
  - `receive_xi_mapping_after_xkb_set_request`: the XKB requester and another client select the master keyboard's
    DeviceMappingNotify; an XKB SetMap request changes the key map; deny the requester's XI1 copy. The requester
    gets no XI1 copy but keeps its XKB and core notifications and the reply, literal and in order; the other
    client's XI1 frame equals its literal baseline; the recorded check uses `D(DEVICEID_MASTER_KEYBOARD)`; no Receive
    call is recorded for the XKB or core notifications. `// mutation: leave X5 on the unchecked fanout`
- [ ] **Step 2: Run** `cargo test -p yserver-core access_xi_notify`, then
  `cargo test -p yserver-core access_notify_exemptions` — Expected: the new denial tests and the updated exemption
  test FAIL; the baseline-order test passes.
- [ ] **Step 3: Implement** X5–X8. Correct two doc comments to name yserver behavior: `emit_device_mapping_notify`
  (aggregate per-client fanout; Xorg broadcasts per window) and `change_modifier_mapping`'s `on_applied` note (the
  XI-first order is yserver's; Xorg writes the core notification first, `xkb/xkbEvents.c:117-133`). No order change.
- [ ] **Step 4: Run** the tests, `cargo test -p yserver-core xi` and the crate suite under load — Expected: PASS.
- [ ] **Step 5: Commit** `feat(access): Receive on XI mapping and change-device notifications`.

### Task 4: Device property notifications

**Files:** Modify `core_loop/process_request/props.rs` (X9, X10); test `tests/access_xi_notify.rs`.

**Interfaces:** X9 uses `xi2_event_description(12)`, X10 `xi1_event_description(XI_DEVICE_PROPERTY_NOTIFY_OFFSET)`,
both with `D(deviceid)` through the checked fanout. The returned vector keeps carrying only transport disconnects.

- [ ] **Step 1: Write the failing tests:**
  - `receive_xi_property_forms_checked_independently`: one client selects XI2 PropertyEvent (`XIAllDevices`) and
    XI1 DevicePropertyNotify for the device; XIChangeProperty on the device. Run 1 denies only the XI2 description,
    run 2 only the XI1 description. In each run the client receives exactly the other form's copy, equal to its
    all-Allow bytes, and exactly one Receive call per form is recorded with `D(deviceid)` and the exact description;
    the property value is stored; replies and errors are unchanged.
    `// mutation: skip the check on the XI1 path (X10)`
  - `receive_xi_property_baseline_order`: no policy installed, the dual-selecting client receives XI2
    PropertyEvent then XI1 DevicePropertyNotify, as complete literal frames. Passes before and after. `// mutation: swap the X9 and X10 blocks`
  - `receive_xi_property_denial_is_not_disconnect`: a client with both selections established; deny every Receive
    call (admission allowed); call `emit_property_change` directly. Assert no bytes reach the client, the returned
    vector is empty and the client stays connected. `// mutation: push denied clients into the returned vector`
- [ ] **Step 2: Run** `cargo test -p yserver-core access_xi_notify` — Expected: the new denial tests FAIL; the
  baseline test passes.
- [ ] **Step 3: Implement** X9 and X10. Correct the `emit_property_change` doc comment: Xorg broadcasts per selecting
  window (`Xi/xiproperty.c:206-208`, `Xi/exevents.c:3300-3312`); yserver sends one aggregate copy per client and
  form, and its device subject is a yserver rule.
- [ ] **Step 4: Run** the tests, the full crate suite under load, every `tests/*.rs` integration target, and
  `cargo clippy --all-targets -- -D warnings` — Expected: PASS.
- [ ] **Step 5: Commit** `feat(access): Receive on XI device property notifications`.
