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
  replies, errors, their order, sequence numbers, byte order and multiplicity. The existing suite passes unmodified.
- No new recipient selection, event production or propagation rule. Preserve the existing absent-client, duplicate,
  disconnect and failed-write handling. A policy denial is not a transport failure and never enters a returned
  disconnected-client vector.
- Reuse first: `fanout_event_to_clients_with_receive`, `ServerState::check_receive`, the existing encoders, the
  `xi1_client_wants_*` predicates, `NotifyFixture`/`RecordingExtensionPolicy` test support and the `xi_hotplug.rs`
  drivers. New code only where a stated requirement cannot be met otherwise.
- Empty policy list → no description is built and no policy is called (the existing `policies_installed()` shortcut).
- Server-generated notifications have no Send check. The request opcode passed to Receive is `state.receive_request`,
  which `process_request` already scopes; independent server work passes `None`.
- Check before per-recipient encoding, telemetry and buffering. A denial drops only that recipient's copy; the device,
  hierarchy, mapping or property change, the reply and every other copy are unchanged.
- Core `MappingNotify` stays exempt and on its existing writer, also where an XI request emits it inline.
- No environment variable, no `cfg(test)` in production bodies. Xorg is MIT: read it, do not copy code.
- Tests drive `process_request` or the real entry point (hotplug publishers). Every DENY test asserts: no bytes of that
  event to the denied recipient, byte-identical output to the allowed recipient compared with an all-Allow run, no
  error or disconnect, and the underlying state change still present. Every test names its compiling mutation in a
  comment; run it, observe the failure, restore. Suites run under CPU load. Test files stay below 6000 lines.
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
| X6 / 3 | XI1 ChangeDeviceNotify | `C/xi1_focus.rs:emit_change_device_notify` (696) | aggregate class selectors, minus originator | D(deviceid) | Drop | none: Xorg returns `BadDevice` (`Xi/chgkbd.c:92`) |
| X7 / 3 | Originator DeviceMappingNotify | `P/xi/dispatch/control.rs` key map (483), modifier map (641), button map (841) | the originator, if selected | D(deviceid) | Drop the inline copy; keep reply, core MappingNotify, order | `dix/inpututils.c:103` |
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

- [ ] **Step 1: Write the failing tests** in `tests/access_xi_notify.rs`:
  - `receive_xi_hierarchy_subject_is_selected_window`: the receiver selects HierarchyChanged on ROOT and on a child;
    disable a device via the enable driver; the policy denies `W(child)` only. Assert: exactly one HierarchyChanged
    reaches the receiver, byte-identical to the ROOT copy of an all-Allow run; recorded Receive checks name
    `W(ROOT)` and `W(child)`, events `[xi2_event_description(11)]`; device disabled state unchanged.
    `// mutation: pass W(ROOT_WINDOW) instead of the selected window in X2`
  - `receive_xi_presence_denial_keeps_other_client`: two clients select DevicePresenceNotify; deny one. The other's
    bytes equal the all-Allow run; denied client gets none; events `[xi1_event_description(15)]`.
    `// mutation: skip the receive check in X1 (use fanout_event_to_clients)`
  - `receive_xi_presence_from_server_work_has_no_request`: call the publisher directly (`publish_facet_disabled`)
    outside `process_request`; assert the recorded check has `request: None`; when driven by the enable request,
    `request` equals `Some` of that request's opcodes.
    `// mutation: hard-code Some(RequestOpcodes) in the check`
- [ ] **Step 2: Run** `cargo test -p yserver-core access_xi_notify` — Expected: FAIL (helpers missing / no checks).
- [ ] **Step 3: Implement** the two helpers and switch X1, X2 to the checked fanout with `W(selected_window)`
  (rename `_selected_window`). Build the `EventBatch` once outside the loop.
- [ ] **Step 4: Run** the three tests, `cargo test -p yserver-core xi_hotplug` and the whole crate suite under load.
  Expected: PASS, existing hotplug tests unmodified.
- [ ] **Step 5: Commit** `feat(access): Receive on XI presence and hierarchy notifications`.

### Task 2: DeviceChanged and the XISelectEvents bootstrap

**Files:** Modify `xinput/hotplug.rs` (X3), `core_loop/process_request/xi/mod.rs` (X4); test `tests/access_xi_notify.rs`.

**Interfaces:** Consumes Task 1's `xi2_event_description`. For X4, a local
`state.check_receive(client_id, EventSubject::Window(ROOT_WINDOW), &batch, state.receive_request)` per master,
before `encode_xi2_device_changed_event`; `Decision::Deny(_)` → `continue`. The empty-buffer early return stays, so
with both masters denied nothing is written and the request still succeeds.

- [ ] **Step 1: Write the failing tests:**
  - `receive_xi_device_changed_denial_per_window`: receiver selects DeviceChanged for the master on ROOT and a
    child; trigger a slave switch with the existing `announce_xi2_slave_switch`; deny `W(child)`. One copy arrives,
    equal to the ROOT copy of the all-Allow run; the master's recorded last slave is updated.
    `// mutation: drop the subject loop variable and check W(ROOT_WINDOW)`
  - `receive_xi_bootstrap_denies_one_master_only`: both masters carry a slave's classes; the issuer's XISelectEvents
    selects DeviceChanged on ROOT for `XIAllDevices`; deny the check whose events are DeviceChanged only for the
    first call (index 0). Assert only the second master's event arrives, byte-identical to its all-Allow bytes, and
    the selection is stored. With every check denied: no event bytes, no error, selection stored.
    `// mutation: check once for the whole buffer instead of per master`
- [ ] **Step 2: Run** `cargo test -p yserver-core access_xi_notify` — Expected: the two new tests FAIL.
- [ ] **Step 3: Implement** X3 as in Task 1 and X4 as stated in Interfaces.
- [ ] **Step 4: Run** the tests, `cargo test -p yserver-core xi` and the crate suite under load — Expected: PASS.
- [ ] **Step 5: Commit** `feat(access): Receive on XI DeviceChanged and its bootstrap`.

### Task 3: Mapping and change-device notifications

**Files:** Modify `core_loop/xi1_focus.rs` (X5, X6), `core_loop/process_request/xi/dispatch/control.rs` (X7, three
sites), `core_loop/process_request/xi/dispatch/devices.rs` (X8); test `tests/access_xi_notify.rs`.

**Interfaces:** Consumes `xi1_event_description` with the existing `XI_DEVICE_MAPPING_NOTIFY_OFFSET` and
`XI_CHANGE_DEVICE_NOTIFY_OFFSET`. X5/X6 use the checked fanout with `D(deviceid)`. X7/X8 keep their
`xi1_client_wants_*` predicate and add one local `check_receive(client_id, D(deviceid), &batch,
state.receive_request)`; only `Allow` appends the copy. Core `MappingNotify` writes stay unchecked.

- [ ] **Step 1: Write the failing tests:**
  - `receive_xi_mapping_fanout_device_subject`: two other clients select DeviceMappingNotify for the device; the
    originator changes the device key mapping; deny one. Recorded subject is `D(deviceid)`; the allowed client's
    bytes equal the all-Allow run. `// mutation: use EventSubject::Global in X5`
  - `receive_xi_mapping_originator_denial_keeps_reply_order`: for ChangeDeviceKeyMapping, SetDeviceModifierMapping
    and SetDeviceButtonMapping, the originator selected DeviceMappingNotify and is denied. Its output equals the
    all-Allow output with exactly the DeviceMappingNotify removed: reply, core `MappingNotify` (where emitted) and
    their order unchanged; the mapping is applied. `// mutation: skip the check at the button-mapping site only`
  - `receive_change_device_notify_originator_denial_keeps_core_mapping`: ChangeKeyboardDevice with the originator
    selecting ChangeDeviceNotify and denied: output equals the all-Allow output minus ChangeDeviceNotify; core
    `MappingNotify` and the reply remain, and no Receive check is recorded for the core `MappingNotify`.
    `// mutation: route the core MappingNotify through the checked fanout`
- [ ] **Step 2: Run** `cargo test -p yserver-core access_xi_notify` — Expected: new tests FAIL.
- [ ] **Step 3: Implement** X5–X8.
- [ ] **Step 4: Run** the tests, `cargo test -p yserver-core xi` and the crate suite under load — Expected: PASS.
- [ ] **Step 5: Commit** `feat(access): Receive on XI mapping and change-device notifications`.

### Task 4: Device property notifications

**Files:** Modify `core_loop/process_request/props.rs` (X9, X10); test `tests/access_xi_notify.rs`.

**Interfaces:** X9 uses `xi2_event_description(12)`, X10 `xi1_event_description(XI_DEVICE_PROPERTY_NOTIFY_OFFSET)`,
both with `D(deviceid)` through the checked fanout. The returned vector keeps carrying only transport disconnects.

- [ ] **Step 1: Write the failing tests:**
  - `receive_xi_property_forms_checked_independently`: one client selects XI2 PropertyEvent (`XIAllDevices`) and
    XI1 DevicePropertyNotify for the device; XIChangeProperty on the device; deny only the XI2 description. The
    client receives exactly the XI1 copy, equal to its all-Allow bytes; the property value is stored; the request
    reply/errors are unchanged. `// mutation: skip the check on the XI1 path (X10)`
  - `receive_xi_property_denial_is_not_disconnect`: deny everything; `emit_property_change` returns an empty vector
    and the client stays connected. `// mutation: push denied clients into the returned vector`
- [ ] **Step 2: Run** `cargo test -p yserver-core access_xi_notify` — Expected: new tests FAIL.
- [ ] **Step 3: Implement** X9 and X10.
- [ ] **Step 4: Run** the tests, the full crate suite under load, every `tests/*.rs` integration target, and
  `cargo clippy --all-targets -- -D warnings` — Expected: PASS.
- [ ] **Step 5: Commit** `feat(access): Receive on XI device property notifications`.
