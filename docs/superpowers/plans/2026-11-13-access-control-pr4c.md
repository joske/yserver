# Access control layer, PR 4c — input and keyboard notifications Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Check Receive on every existing input, XI and XKB delivery below, including grabbed and deferred input,
without changing any client-observable byte when no policy is installed.

**Architecture:** Consume 4a's hooks/descriptions and 4b's checked egress and scoped request metadata. Migrate
notification leaves to that egress; where input routing needs a delivery result, observe the checked singleton
emission rather than interpreting its disconnected-client list as a delivery count. Keep physical input state and
grab lifetime separate from whether a recipient gets bytes; use Xorg's denial-dependent fallback rules.

**Tech Stack:** Rust, crate `yserver-core`; existing protocol encoders, recording backend and recording policy; std only.

**Spec:** `docs/superpowers/specs/2026-10-30-access-control-layer-design.md`, “What a policy can see”, “A denial is
interpreted by its site”, “Who is checked”, “Where the checks go” → Receive / XTEST.

**Reference:** Xorg (MIT), **xorg-server 21.1.24**, `/data/Projects/xorg-21.1.24`. All Xorg paths/lines below were
rechecked in that tree. The spec binds where it decides; otherwise follow Xorg. No XLibre reference is needed.

**Source inventory:** `/home/ariel_santangelo/Projects/claude-scratch/access-control/pr4/inventory.md`, taken at
`d14eaecb`, §3 Pointer/Keyboard fanout, XI focus/state/mapping/change, inline XI, hotplug/hierarchy and XKB; also the
two XI device-property sites and two XI helpers under common fanout. Relocated on `feat/access-control-pr4b`, HEAD
`6c651290` (only the approved 4b plan differs from upstream `c3d43e3a`). Anchors below describe current source;
4b implementation will move them. Read the converged `2026-11-13-access-control-pr4b.md` as a dependency, together
with the 4a and 4a-follow-up plans. Implement 4c against the interfaces 4b actually produces, without redefining them.

## Global Constraints

- No policy installed → no client-observable byte changes: replies, errors/precedence, event order, padding,
  send bit, sequences, byte order and buffered output remain identical. The existing suite passes unmodified.
- No new validity errors, validation, event production or independent routing fixes. Retain current empty-target,
  missing/stale-source, mask, wildcard, repeat, deduplication and transport-failure behavior. Denial-dependent
  routing changes specified below apply only when Receive denies an actual candidate.
- Helpers follow `check_resource`: empty policy list → Allow without context construction, owner resolution or
  dynamic call. Reuse `check_receive` and first-denial order; Receive does not exempt internal/SERVER_OWNER recipients.
  No cached authorization, environment variable, new access mode or `cfg(test)` in production bodies.
- Reuse first: 4b's checked fanout, existing window/device/selection resolvers, XI form selection and ancestor walks,
  `RecordingExtensionPolicy` in `process_request/tests/extensions.rs`, 4b's `NotifyFixture` and `access_send.rs`
  decision/drain patterns. No second recipient resolver or writer. Every new interface below states its requirement.
- Server-generated events have no Send check, even if a request caused them. Checks use 4b's `receive_request`:
  actual `Some(RequestOpcodes)` during dispatch, `None` for independent input/hotplug/repeat work. Wire XKB cause
  opcodes are not the access context; retain their existing values even when they differ from the triggering request.
- Check before per-recipient encoding, telemetry or buffering. Snapshot facts out of mutable client/device/resource
  borrows, decide using read-only state, then borrow the client for writing. Build descriptions from event identity
  and batch shape, never by running a full encoder for a recipient who may be denied.
- Tests use `process_request`, or the real event-generating entry point for server events, with RecordingBackend.
  Setup may seed devices, resources, masks and backend answers. Helper-only tests do not cover a delivery site.
  Policies allow unrelated hooks; ordinal denial rows count matching Receive calls, not dispatch/resource records.
- Every DENY row asserts its named suppression/routing effect and isolation: no bytes for the denied event in socket
  **or pending output**, permitted deliveries retain exact bytes, no new error/disconnect, and no unrelated state or
  backend effect changes. Compare input/device/focus/keymap state against an Allow control. An implicit grab created
  only by successful delivery is the explicit Xorg-dependent exception: assert its correct absence/allowed owner;
  do not undo physical input state, an existing grab or its sync/release transitions.
- Every new test names a compiling mutation in a comment; run it, observe that test fail, restore it, then pass the
  task gate. Tests added to sibling modules must be registered in `tests/mod.rs`; every test file stays <6000 lines.
  Use nonblocking bounded drains, explicit input timestamps and existing repeat deadlines, never sleeps or narrow
  elapsed-time assertions. Run suites under CPU load as in Task 9.
- Implementation stays on the feature branch, formats with `cargo +nightly fmt`, and runs exactly
  `cargo clippy --all-targets -- -D warnings` before each task commit. No merge authorization. No ioctl/platform
  change, hardware gate or new dependency. Xorg may be read, not copied.

## Review Focus

1. **Grab owner allowed at one window but denied at another:** owner-events natural denial must try the grab
   fallback; fallback denial still captures/refreezes input and never leaks to another client. Tasks 1/3/4:
   `receive_pointer_owner_events_denial_tries_grab_window`, `receive_key_grab_denial_keeps_capture`,
   `receive_xi1_grab_denial_keeps_sync_and_replay`.
2. **Queued input outlives a grant or source:** authorize at actual replay, not enqueue; reject stale origins and
   retain slave/master raw ordering. Task 9: `receive_frozen_input_rechecks_on_thaw` and
   `receive_replay_revalidates_source_and_request_context`.
3. **One logical delivery occupies several wire events:** XI1 motion/state continuations and two-axis scroll-stop
   batches are described completely and denied together; detectable repeat keeps its existing omissions. Tasks 2/4/3:
   `receive_scroll_stop_describes_both_axes`, `receive_xi1_chains_are_atomic`, `receive_repeat_keeps_protocol_forms`.
4. **A windowless wire event still has a selection window:** two hotplug selections by one client remain two
   independently checked deliveries; raw root delivery uses the root, not a wildcard/source device. Task 6:
   `receive_hotplug_uses_each_selection_window`; Tasks 2/3: `receive_raw_pointer_uses_root_and_each_form`,
   `receive_raw_key_distinguishes_grab_and_root`.
5. **Mixed XI reply/event and extension identity:** drop only inline XI bytes, preserve exempt MappingNotify, and
   distinguish XI2 evtypes from XI1 offsets and XKB's single ordinary event code. Tasks 7/8:
   `receive_inline_xi_denial_keeps_reply_and_mapping`, `receive_xkb_metadata_uses_event_base_not_subtype`.

## Scope decisions

- **Count:** 46 current semantic emission expressions, I01–I46, including shared forwarding leaves and three checked
  inline XI constructions. This counts source sites, not recipients or hooks. Six XKB-group MappingNotify expressions
  and the inline/core XI MappingNotify expressions are exempt, not extra sites. 4a's XI SendExtensionEvent is already
  covered and must not be checked twice. Core FocusIn/Out remains 4b; its XI2 forwarding is I20 here.
- **Subjects:** use W(actual delivery/transition window), including XI1 state chains and per-window
  DevicePresence/DeviceChanged/HierarchyChanged even though their wire formats lack a window. Their existing loops
  retain the selected window: do not discard it or collapse all a client's windows into one hook. Bootstrap and
  virtual-pointer DeviceChanged use their existing root selection context. Root-selected raw events use W(ROOT_WINDOW),
  as Xorg `DeliverRawEvent` passes the root to the recipient helper (`dix/events.c:2494-2519`); the raw-key grabbed
  leg uses W(natural root) or W(grab window) according to its route. Do not add a missing raw-pointer grab route.
  D(device) is for aggregate device-only mapping/change/property notifications and native XKB. XKB uses its **wire
  device ID 1**, not XI master keyboard 3; other device notifications use the concrete device, not selector 0/1.
  W takes a ResourceId; widen u8 wire IDs with `u16::from` for D, without remapping them to XI IDs.
  An event with none of window/drawable/device uses `EventSubject::Global`; no existing 4c site needs it. In particular
  hierarchy has a selection-window subject, not a fabricated Device(0), changed device or Global.
- **Descriptions:** core codes 2–8 have no extension metadata. XI1 uses `XI_FIRST_EVENT + offset`, XI major from
  the existing extension table, and `event_type = offset`. XI2 uses type 35, its XI major and actual u16 evtype via
  `describe_wire_event`, including for events longer than 32 bytes. Only the fixed metadata header is necessary.
  XKB is an ordinary extension event: type = runtime event base, major = yserver's advertised XKEYBOARD opcode,
  **event_type = 0**, not wire byte 1's xkbType. Preserve subtype in the wire event; do not extend the policy ABI.
  One batch describes exactly the ordered events an invocation emits. XI1 MORE_EVENTS chains are atomic; a scroll
  stop describes two XI_Motion events. Focus and its following state snapshot are independent batches.
- **Metadata without a Backend:** key, hotplug and several shared emitters have no backend. Task 1 extracts 4a's
  wire decoder into a pure `access::describe_wire_event` taking a lazy ordinary-extension lookup, retaining the old
  `process_request::selection::describe_wire_event` signature as a wrapper. This is required to share the GenericEvent
  decoder without fabricating a backend, threading one through all input APIs, or encoding a denied event. 4a tests
  and callers remain unchanged. Generated ordinary XI/XKB descriptions use typed constructors and existing table
  identities; neither a second registry nor a new metadata parser is introduced.
- **Ordinary input:** denied selected recipients count as nondelivery, never as disconnect. Continue the existing
  candidate walk when every eligible recipient is denied, preserving focus bounds, existing do-not-propagate rules
  and coordinates/child. A denied XI2 candidate cannot keep absorbing a permitted lower protocol form solely because
  it selected XI2. Xorg tries XI2/XI1/core then ancestors until actual delivery, rejecting unrelated clients under a
  grab (`dix/events.c:2071-2075, 2291-2295, 2897-2932, 4258-4324`). Foreign-client grab rejection still stops the
  natural walk; it is not Receive nondelivery. Transport failure also keeps the existing stop/disconnect behavior:
  do not retry ancestors or grab fallback just because the adapter excluded a failed write from its delivery bool.
  Continue only for Receive nondelivery (false bool with no failed recipient), after existing no-event filters.
  Preserve existing no-policy form/dedup ordering;
  this PR does not globally replace yserver's routing with Xorg's. Continue candidates inside the current delivery
  operation, never by reentering the top-level input router and repeating physical transitions/grab matching.
  Crossings/focus/state/device notifications do not
  gain input propagation or fallback. Never ask Receive in a diagnostic/probe that will not attempt delivery.
- **Active/previously activated grabs:** a denied owner-events **natural** attempt is nondelivery and can continue
  that bounded natural walk, then fall back to the grab window. Each attempted window is checked separately, even
  for the same owner. A denial on the grabbed fallback counts as handled, preserves capture, active grab, motion
  bookkeeping and sync re-freeze; never route it to another client (`DeliverGrabbedEvent`/`DeliverOneGrabbedEvent`,
  `dix/events.c:4388-4397, 4431-4473`). If the natural route delivered, do not also try the grab window. Retain the
  current mask and protocol restrictions before delivery; no denied bytes or policy-generated X error.
- **Passive and implicit grabs:** passive activation precedes delivery; denying its activating event does not undo
  activation, confinement, focus/crossings, freeze or stored replay event. Xorg's activating write bypasses Receive
  (`dix/events.c:3886-3919`); the spec's recipient rule deliberately checks it in yserver. Existing grab release and
  replay-floor rules remain. A new implicit pointer grab can use only a recipient actually delivered a permitted
  press, not merely one absent from the overflow list (`dix/events.c:2417-2431`). This is why Task 1 needs a local
  delivery-result adapter atop 4b's checked helper; its return type and existing helpers are not redefined.
- **Frozen/repeat input:** canonical queued facts are not output; check when delivered after thaw/replay, with
  current policy and route. Already-buffered bytes are not recalled. Preserve today's separate synthetic release/
  press emissions and detectable-repeat filters; describe each emitted batch, with no checks for omitted releases.
  Xorg synthesizes legacy repeat releases inside `TryClientEvents` (`dix/events.c:2100-2128`); do not reorganize
  yserver's repeat producer to imitate that internal representation or add a release to XI2.
- **Inline XI:** DeviceMappingNotify is checked even when appended to a reply. Deny only that event, at its current
  position: modifier event before reply; pointer reply then event; ChangeKeyboardDevice's ChangeDeviceNotify then
  exempt MappingNotify then reply; ChangeDeviceKeyMapping is void. Xorg has no matching ChangeKeyboardDevice producer
  (its handler returns BadDevice, `Xi/chgkbd.c:92-98`); keep yserver's current successful behavior and apply the spec's
  Receive rule. Do not introduce Xorg's rejection or parse the mixed buffer in `xi_write_reply`.
- **Inherited 4b rulings remain binding:** denied MapRequest/ConfigureRequest/ResizeRequest/CirculateRequest counts as
  delivered and keeps redirection; denied SelectionRequest answers the ConvertSelection issuer with the exempt
  SelectionNotify(None); all other selection delivery checks remain 4a/4b. MappingNotify and direct GraphicsExpose/
  NoExpose results bypass Receive. 4c neither reimplements nor weakens these rules.
- **Exclusions/findings:** SHAPE/XFIXES/SYNC/Present/saver/SHM/DAMAGE are 4d; RANDR/RECORD (including input recording)
  are 4e. XTEST is admission-only; do not add a Send check or retain its issuer as sender through input queues.
  Input it generates naturally reaches these shared Receive sites. No KeymapNotify encoder/producer exists in the
  current core tree (only wire-swap support); record this existing gap instead of adding code-11 delivery. Keep
  4b's absent ResizeRequest finding. Do not retire the unchecked fanout APIs while exempt/later callers still use them.

## Site table

`C/ = crates/yserver-core/src/core_loop/`, `P/ = C/process_request/`, `Y/ = crates/yserver-core/src/`.
W(x) = `EventSubject::Window(x)`, D(x) = `EventSubject::Device(x)`. **Drop** suppresses only the denied delivery and
leaves the producing operation/state unchanged; **Input** also applies ordinary nondelivery/fallback and delivered
press accounting above; **Grab** preserves capture/sync/lifetime after denied fallback. All rows inherit DENY isolation.
Xorg event producers cited below feed its generic Receive path (`dix/events.c:2213-2224, 2291-2295`) unless explicitly
marked “direct/bypass”; the spec supplies Receive for those bypasses.

| ID / task | Event(s) | Current file:function (emission line) | Recipient | EventSubject | Denial effect | Xorg 21.1.24 reference |
|---|---|---|---|---|---|---|
| I01 / 1 | Core button/motion, owner-events natural | `C/pointer_fanout.rs:pointer_event_fanout_to_state_inner` (1222) | Grab owner at natural candidate | W(natural_window) | Input; try fallback if no natural delivery | `dix/events.c:4431-4462` |
| I02 / 1 | Core button/motion, grabbed fallback | Same (1290) | Grab owner | W(grab_window) | Grab | `dix/events.c:4388-4397, 4467-4473` |
| I03 / 1 | Activating core passive ButtonPress | Same (1450) | Matching passive-grab owner | W(grab.grab_window) | Drop; keep activation/freeze/store | `dix/events.c:3886-3919` (bypass) |
| I04 / 1 | Core ButtonPress/Release/Motion/Enter/Leave | Same (1643) | Existing propagated/crossing targets | W(nested_id) | Input for data, Drop for crossing | `dix/events.c:2897-2932, 4777-4783` |
| I05 / 2 | XI2 raw button/motion, slave | Same (1996) | Root slave/wildcard selectors | W(ROOT_WINDOW) | Drop; other form retained | `dix/events.c:2477-2519` |
| I06 / 2 | XI2 raw button/motion, master | Same (2013) | Root master/wildcard selectors | W(ROOT_WINDOW) | Drop; physical input retained | Same as I05 |
| I07 / 2 | XI_Motion smooth-scroll before wheel button | Same (2219) | Each existing motion target/form | W(event_window) | Drop; wheel event/counter retained | `dix/events.c:2816-2829, 2897-2932` |
| I08 / 2 | XI2 button/motion/Enter/Leave | Same (2401) | Routed slave/master selector or grab owner | W(ev_win) | Input / Grab / crossing Drop | `dix/events.c:2897-2932, 4388-4473, 4868-4879` |
| I09 / 2 | Two XI_Motion scroll-stop events | `C/pointer_fanout.rs:emit_scroll_stop_to_state` (2671) | Each existing target/form | W(target) | Drop whole two-event batch | `dix/events.c:2816-2829` (delivery analogue) |
| I10 / 3 | Core KeyPress/Release | `C/key_fanout.rs:deliver_key_to_window` (761) | Existing core selectors after form/repeat filtering | W(target_window) | Input | `dix/events.c:2100-2115, 4258-4324` |
| I11 / 3 | XI_KeyPress/Release | Same (785) | Each selected XI2 form | W(target_window) | Input | `dix/events.c:4283-4292` |
| I12 / 3 | Grabbed XI_KeyPress/Release | `C/key_fanout.rs:deliver_key_to_grab_owner` (841) | XI2 grab owner | W(natural/grab window passed) | Input natural; Grab fallback/activation | `dix/events.c:3886-3919, 4388-4473` |
| I13 / 3 | Grabbed core KeyPress/Release | Same (863) | Core grab owner | W(natural/grab window passed) | Same as I12 | Same as I12; `dix/events.c:2100-2115` |
| I14 / 3 | XI_RawKeyPress/Release | `C/key_fanout.rs:send_raw_key` (1202) | Existing slave/master roots or grabbed-key leg | W(root / natural root / grab window) | Drop roots; Input natural; Grab fallback | `dix/events.c:2477-2519, 4431-4473` |
| I15 / 4 | XI1 device key/button/motion + valuator | `C/pointer_fanout.rs:xi1_fan_device_event` (3031) | Existing XI1 routed/grabbed recipients | W(event_window) | Input / Grab; whole chain omitted | `dix/events.c:2846-2848, 2897-2932, 4388-4473` |
| I16 / 4 | DeviceFocusIn/Out | `C/xi1_focus.rs:emit_device_focus` (567) | Exact per-window XI1 focus class | W(window) | Drop; following state independent | `dix/enterleave.c:822-836` |
| I17 / 4 | DeviceMappingNotify | `C/xi1_focus.rs:emit_device_mapping_notify` (626) | Aggregate exact class, optional originator excluded | D(deviceid) | Drop; mapping and other notifications kept | `xkb/xkbEvents.c:129-155`; `Xi/exevents.c:3300-3312` |
| I18 / 4 | ChangeDeviceNotify | `C/xi1_focus.rs:emit_change_device_notify` (696) | Aggregate exact class, originator excluded | D(deviceid) | Drop | `Xi/chgkbd.c:92-98` (no producer); generic Receive above |
| I19 / 4 | DeviceStateNotify + key/valuator continuations | `C/xi1_state_notify.rs:deliver_state_notify` (82) | Exact state class on focus window | W(window) | Drop whole chain; focus event independent | `dix/enterleave.c:698-763, 835-836` |
| I20 / 5 | XI_FocusIn/Out | `C/fanout.rs:emit_xi2_focus_event_to_state` (371) | Existing transition-window XI2 master/wildcard selectors | W(window) | Drop; core focus retained | `dix/enterleave.c:767-818` |
| I21 / 5 | XI Enter/Leave/FocusIn/Out NotifyGrab | `P/xi/dispatch/grabs.rs:handle_xi_grab_device` (328) | Grabbing client | W(target_window) | Drop; successful grab/reply retained | `dix/events.c:4868-4879` (crossing grab bypass); `dix/enterleave.c:817-833` |
| I22 / 5 | Same, NotifyUngrab | `P/xi/dispatch/grabs.rs:deactivate_xi2_device_grab` (534) | Former grab owner | W(target_window) | Drop; ungrab/thaw retained | Same as I21 |
| I23 / 5 | XI_BarrierHit/Leave | `C/pointer_fanout.rs:emit_barrier_event` (4199) | Matching owner/window grab, else barrier selectors | W(barrier_window) | Drop; barrier constraint/release retained | `Xi/exevents.c:1709-1747` |
| I24 / 6 | Virtual-pointer XI_DeviceChanged | `C/fanout.rs:emit_xi2_device_changed_slave_pointer` (440) | Existing exact-device/AllDevices root selectors | W(ROOT_WINDOW) | Drop | `Xi/exevents.c:747-761, 3300-3312` |
| I25 / 6 | Bootstrap XI_DeviceChanged SlaveSwitch, per selected master | `P/xi/mod.rs:emit_xi2_device_changed_bootstrap` (630 encode, 650 write) | XISelectEvents issuer selecting root with retained slave classes | W(ROOT_WINDOW) | Drop only denied master event | `Xi/exevents.c:747-761`; bootstrap itself is yserver-specific |
| I26 / 6 | XI_PropertyEvent | `P/props.rs:emit_property_change` (778) | Existing deduped exact/AllDevices selectors | D(deviceid) | Drop; property/reply retained | `Xi/xiproperty.c:185-209` |
| I27 / 6 | DevicePropertyNotify | Same (821) | Existing aggregate exact XI1 class | D(deviceid) | Drop; XI2 sibling independent | Same as I26 |
| I28 / 6 | DevicePresenceNotify Added/Removed/Enabled/Disabled | `Y/xinput/hotplug.rs:emit_xi1_device_presence` (68) | Each selected client/window tuple | W(selected_window) | Drop this window's copy only | `dix/devices.c:333-345`; `Xi/exevents.c:3280-3312` |
| I29 / 6 | XI_DeviceChanged DeviceChange/SlaveSwitch | `Y/xinput/hotplug.rs:emit_xi2_device_changed` (263) | Each selected window, existing exact/wildcard match | W(selected_window) | Drop; class-source/last-slave update retained | `Xi/exevents.c:747-761, 3280-3312` |
| I30 / 6 | XI_HierarchyChanged | `Y/xinput/hotplug.rs:emit_xi_hierarchy_changed` (417) | Each AllDevices hierarchy selection window | W(selected_window) | Drop full hierarchy event, keep registry transition | `Xi/xichangehierarchy.c:62-121`; `Xi/exevents.c:3280-3312` |
| I31 / 7 | Originator DeviceMappingNotify Modifier, before reply | `P/xi/dispatch/control.rs:handle_xi1_set_device_modifier_mapping` (640) | Selected originator | D(dev) | Drop event only | `xkb/xkbEvents.c:146-155`; generic Receive above |
| I32 / 7 | Inline DeviceMappingNotify Keyboard (void) | `P/xi/dispatch/control.rs:handle_xi1_change_device_key_mapping` (483) | Selected originator | D(dev) | Drop event only, mapping/followups retained | `Xi/chgkmap.c:111`; `xkb/xkbEvents.c:129-143` |
| I33 / 7 | Inline DeviceMappingNotify Pointer, after reply | `P/xi/dispatch/control.rs:handle_xi1_set_device_button_mapping` (841) | Selected originator | D(dev) | Drop event, keep entire reply | `dix/inpututils.c:71-103`; generic Receive above |
| I34 / 7 | Inline ChangeDeviceNotify, before MappingNotify/reply | `P/xi/dispatch/devices.rs:handle_xi1_change_keyboard_device` (663) | Selected originator | D(dev) | Drop event, keep exempt MappingNotify and reply | `Xi/chgkbd.c:92-98` (no producer); spec inline ruling |
| I35 / 8 | XkbNewKeyboardNotify | `C/xkb_layout.rs:apply_rules_names_change` (145) | Existing NKN subscribers | D(1) | Drop; rules/keymap load retained | `xkb/xkbEvents.c:162-195` (direct) |
| I36 / 8 | XkbMapNotify | `C/xkb_layout.rs:send_xkb_map_notify` (185) | Changed-detail map recipients | D(notify.device_id) | Drop; return original recipient list | `xkb/xkbEvents.c:266-300` (direct) |
| I37 / 8 | XkbCompatMapNotify | `C/xkb_layout.rs:send_xkb_compat_map_notify` (257) | Initialized compat interest | D(notify.device_id) | Drop | `xkb/xkbEvents.c:654-694` (direct) |
| I38 / 8 | XkbNamesNotify | `C/xkb_layout.rs:send_xkb_names_notify` (271) | Matching names details | D(notify.device_id) | Drop | `xkb/xkbEvents.c:607-649` (direct) |
| I39 / 8 | XkbExtensionDeviceNotify IndicatorNames | `C/xkb_layout.rs:send_indicator_names_change` (301) | Matching extension-device interest | D(1) | Drop; names/LED state retained | `xkb/xkbEvents.c:742-789` (direct) |
| I40 / 8 | XkbExtensionDeviceNotify LED maps/state | `C/xkb_layout.rs:send_led_extension_device_notify` (377) | Matching reason details | D(1) | Drop; remaining indicator events retained | Same as I39 |
| I41 / 8 | XkbControlsNotify PerKeyRepeat | `C/xkb_layout.rs:apply_keyboard_mapping_repeats` (413) | Matching controls interest | D(1) | Drop; repeat derivation retained | `xkb/xkbEvents.c:383-427` (direct) |
| I42 / 8 | XkbIndicatorMapNotify/StateNotify | `C/xkb_layout.rs:send_indicator_notify` (462) | Matching kind/detail interest | D(1) | Drop; LED state/other kind retained | `xkb/xkbEvents.c:432-474` (direct) |
| I43 / 8 | XkbStateNotify | `C/xkb_layout.rs:announce_xkb_state` (605) | Changed-state-detail recipients | D(1) | Drop; announced snapshot and indicators advance | `xkb/xkbEvents.c:203-255` (direct) |
| I44 / 8 | XkbControlsNotify | `C/xkb_controls.rs:send_controls_notify` (184) | Initialized changed-controls recipients | D(1) | Drop; controls retained | `xkb/xkbEvents.c:383-427` (direct) |
| I45 / 8 | XkbNewKeyboardNotify on GetKeyboardByName load | `P/xkb.rs:handle_xkb_request` (314) | Existing all-client list in this branch | D(1) | Drop; reply/load/exempt mappings retained | `xkb/xkbEvents.c:162-195` (direct; preserve current recipients) |
| I46 / 8 | XkbNewKeyboardNotify from Set* backend events | `P/xkb.rs:send_xkb_set_events` (615) | Existing new-keyboard recipients | D(1) | Drop; remaining Set* events retained | `xkb/xkbEvents.c:162-195` (direct) |

No Xorg citation above claims its native XKB writes or passive/grab crossing bypasses invoke Receive. The spec
requires those checks here. Xorg's GenericEvent identity helpers are `dix/events.c:183-191, 419-435, 459-468`;
the pure decoder refactor preserves 4a's existing ordinary-extension lookup and GenericEvent field interpretation.

---

Task paths beginning `core_loop/`, `xinput/` or `access.rs` are relative to `crates/yserver-core/src/`.
`tests/` means `core_loop/process_request/tests/`; bare test filenames are siblings there.

## Interfaces consumed from 4a/4b (unchanged)

- `ServerState::check_receive(&self, recipient: ClientId, subject: EventSubject, events: &EventBatch<'_>,
  request: Option<RequestOpcodes>) -> Decision`; `pub(crate) receive_request: Option<RequestOpcodes>`.
- `pub(crate) fn fanout_event_to_clients_with_receive<F>(state: &mut ServerState, client_ids: &[ClientId],
  subject: EventSubject, events: &EventBatch<'_>, encode: F) -> Vec<ClientId>`;
  `pub(crate) fn emit_window_event_to_state_with_receive<F>(state: &mut ServerState, window: ResourceId,
  mask_bits: u32, events: &EventBatch<'_>, encode: F) -> Vec<ClientId>`, both with
  `F: Fn(&mut Vec<u8>, SequenceNumber, ClientByteOrder)`. Returned IDs mean transport failure only.
- `pub(crate) fn fanout_raw_event_to_clients_with_receive(state: &mut ServerState, client_ids: &[ClientId],
  subject: EventSubject, events: &EventBatch<'_>, event: &[u8; 32], template_byte_order: ClientByteOrder)
  -> Vec<ClientId>`; no new raw-template path is needed in 4c.
- `EventBatch::new(events: &[EventDescription]) -> EventBatch<'_>`;
  `EventDescription::core(type_code: u8) -> EventDescription`;
  `EventDescription::extension(type_code: u8, major_opcode: u8, event_type: u16) -> EventDescription`.
- Existing 4a wrapper: `pub(crate) fn describe_wire_event(event: &[u8; 32], byte_order: ClientByteOrder,
  backend: &mut dyn Backend) -> EventDescription` in `process_request/selection.rs`; its signature stays intact.
- 4b `NotifyFixture::new() -> Self`; `request(&mut self, client: ClientId, sequence: SequenceNumber,
  header: RequestHeader, body: &[u8]) -> io::Result<RequestOutcome>`; `drain(&mut self, client: ClientId) -> Vec<u8>`;
  `install_policy(&mut self, decide: impl Fn(usize, &RecordedExtensionCheck) -> Decision + Send + Sync + 'static)
  -> Arc<Mutex<Vec<RecordedExtensionCheck>>>`. Use its public `state`, `backend`, `peers`; no new fixture/policy type.

### Task 1: Shared description/delivery adapters and core pointer input (I01–I04)

**Files:** Modify `access.rs`, `core_loop/process_request/selection.rs`, `core_loop/fanout.rs`,
`core_loop/pointer_fanout.rs`; create `tests/access_input_pointer.rs`, register in `tests/mod.rs`.

**Interfaces:**
- Consumes the unchanged 4a/4b interfaces above and existing pointer selection/coordinate resolvers.
- Produces `pub(crate) fn describe_wire_event(event: &[u8; 32], byte_order: ClientByteOrder,
  extension_lookup: impl FnOnce(u8) -> Option<ExtensionEventDescription>) -> EventDescription` in `access.rs`.
  The old wrapper supplies its existing live backend lookup, lazily only for ordinary codes >=64.
- Produces in `fanout.rs`: `pub(crate) fn describe_xi2_event(major_opcode: u8, evtype: u16) -> EventDescription`
  (a canonical little-endian metadata header through the pure decoder); and
  `pub(crate) fn fanout_input_event_to_client_with_receive<F>(state: &mut ServerState, recipient: ClientId,
  subject: EventSubject, events: &EventBatch<'_>, encode: F) -> (bool, Vec<ClientId>)`, with the same encoder bound
  as 4b. This adapter calls 4b's checked singleton helper **once**, observes nonempty encoding and excludes failed
  writes from the bool. No second policy call, transport loop, global state or new outcome enum. Bool means actual
  permitted delivery, not grab capture; WouldBlock with queued bytes is delivery.
- Preserves `pub fn pointer_event_fanout_to_state(state: &mut ServerState, backend: &mut dyn Backend,
  xid_map: &HostXidMap, event: HostPointerEvent, handle_grabs: bool, is_replay: bool) -> Vec<ClientId>`.

- [ ] **Step 1 — failing tests:** `receive_core_pointer_checks_delivery_windows`: drive the real pointer entry
  with press/release/motion and Enter/Leave chains, two LE/BE subscribers and distinct sequences; record recipient,
  W(propagated/transition window), exact code 4/5/6/7/8, request None, no Send. Deny one, retain permitted events,
  physical button/pointer state and crossing state. Mutation: use hit window for propagated event; bypass one code.
  `receive_core_pointer_all_denied_continues_walk`: child selectors denied, parent allowed; assert parent coordinates/
  child and no child bytes, no walk beyond a delivered candidate; current propagation bounds unchanged. Mutation:
  stop on nonempty selection instead of delivery.
  `receive_pointer_owner_events_denial_tries_grab_window`: install GrabPointer through dispatch, deny the owner at
  its selected natural window, allow at a different grab window; one fallback with grab-relative coordinates. Deny
  both: zero bytes, no other client delivery, grab and sync state equal control; granting natural yields no fallback.
  Mutations: treat natural denial as handled; release grab on fallback denial; retry to nonowner.
  `receive_pointer_passive_denial_keeps_activation`: GrabButton then activating press denied; active grab,
  confinement, cross/focus generation, freeze and stored event equal Allow; release/ungrab still works. Mutation:
  condition activation/store on the adapter bool.
  `receive_pointer_transport_failure_does_not_retry`: failed/overflowing selected writer with a permitted parent
  and grab fallback; retain the old disconnected IDs and route stop, no new ancestor/fallback bytes, no implicit
  grab for failed delivery. Mutation: use false adapter bool alone to retry. Run with no policy and recording Allow.
  `receive_denied_press_cannot_create_implicit_grab`: deny all natural press recipients, then permit one of two
  eligible recipients in a separate row; implicit grab absent / belongs only to delivered recipient, physical state
  unchanged by policy, later release still clears existing implicit grab. Mutation: infer delivered from !dropped.
- [ ] **Step 2 — red gate:** `cargo test -p yserver-core access_input_pointer`; expect missing Receive/routing
  assertions to fail. Save independent no-policy wire expectations before migration.
- [ ] **Step 3 — implement:** extract, do not duplicate, the decoder; preserve wrapper lookup/fallback and all 4a
  callers/tests. Migrate I01–I04 through checked egress, using singleton delivery results where routing/implicit-grab
  accounting requires them. Continue existing walks only after nondelivery; do not put policy calls in the debug
  `pointer_propagation_target_by_id` probe. A denied grab fallback remains handled; a passive activation stays active.
  Keep core/XI selection facts separate from actual delivered-press records for Task 2.
- [ ] **Step 4 — green/mutation gate:** `cargo test -p yserver-core access_input_pointer`;
  `cargo test -p yserver-core access_send`; `cargo test -p yserver-core core_loop::pointer_fanout::tests`;
  `cargo test -p yserver-core core_loop::fanout::tests`; `cargo +nightly fmt -- --check`;
  `cargo clippy --all-targets -- -D warnings`. Restore each named mutation and pass the gate.
- [ ] **Step 5 — commit:** `feat(access): check core pointer delivery and preserve grab denial semantics`.

### Task 2: XI2 pointer, raw input and scroll batches (I05–I09)

**Files:** Modify `core_loop/pointer_fanout.rs`; create `tests/access_input_xi2_pointer.rs`, register in `tests/mod.rs`.

**Interfaces:**
- Consumes Task 1's exact `describe_xi2_event` and singleton adapter and 4b's checked fanout. Preserves the pointer
  entry above and `pub fn emit_scroll_stop_to_state(state: &mut ServerState, xid_map: &HostXidMap, origin: InputOrigin,
  pointer_host_xid: u32, root_x: i16, root_y: i16, state_mask: u16, time: u32)`.
- Produces no public API; existing slave/master selection and route helpers retain their current parameter/return
  types. Delivery control uses actual results, not extra policy calls inside form-selection probes.

- [ ] **Step 1 — failing tests:** `receive_xi2_pointer_checks_each_resolved_form`: physical attached and floating
  sources, exact/AllDevices/AllMasterDevices selections, different selected ancestors, dual core/XI2 selection;
  record W(ev_win), type35/XI major/evtype 4/5/6/7/8, no Send, root/child/relative geometry. Deny one form/window,
  retain other source/form recipients and physical state; denied XI2 absorption permits an eligible core fallback
  at the same candidate, then ancestor when all forms are denied. Mutations: gate core by selected XI2 despite
  denial; use source/hit window for every form; count denied XI2 press for implicit grab. Include owner-events and
  fallback-denied XIGrabDevice rows using the Task 1 capture rule, with no core leak or activation rollback.
  Add a dispatched XIPassiveGrabDevice button row denying the activating XI2 press: active grab, sync stored event
  and physical button state remain, no nonowner/core leak. Mutation: roll back passive activation on Receive denial.
  `receive_raw_pointer_uses_root_and_each_form`: record slave then master raw evtypes 15/16/17, W(root), independently
  deny each and keep other form/order/valuators and normal input. Floating source emits only slave; warp emits no raw.
  Mutations: D(source) instead of W(root); skip master after denied slave; add raw on warp.
  `receive_scroll_denial_keeps_wheel_and_counters`: wheel press via the real pointer entry, deny preceding XI_Motion
  but permit button, then reverse; compare counters, flags, other recipients and source-switch behavior with Allow.
  Mutations: gate the entire wheel operation; describe scroll motion as ButtonPress.
  `receive_scroll_stop_describes_both_axes`: real scroll-stop entry, batch [XI_Motion, XI_Motion] for axes 2/3,
  W(target), unchanged cumulative values; deny on batch length/second description and receive neither, other
  recipients retain both. Mutation: describe only first event or check each axis separately.
  `receive_xi2_metadata_matches_wire`: for all these rows compare recorded descriptions with 4a's unchanged
  `describe_wire_event` applied to the first 32 bytes of each actual permitted LE/BE event; include longer crossing/
  valuator output. Mutation: read evtype in host/recipient order rather than the header's stated order.
- [ ] **Step 2 — red gate:** `cargo test -p yserver-core access_input_xi2_pointer`; fail for missing sites/descriptions.
- [ ] **Step 3 — implement:** I05–I09; metadata precedes recipient encoding. Hoist the current scroll-grab empty
  encoder suppression before the Receive attempt. Preserve per-form geometry, scroll flags, order and raw shortcuts;
  denied candidates do not create implicit grabs or suppress permitted core fallbacks. Reuse existing bounded walks
  for denial continuation; do not replace the general routing model or authorize diagnostics twice.
- [ ] **Step 4 — green/mutation gate:** `cargo test -p yserver-core access_input_xi2_pointer`;
  `cargo test -p yserver-core access_input_pointer`; `cargo test -p yserver-core core_loop::pointer_fanout::tests`;
  `cargo test -p yserver-core core_loop::process_request::tests::xi2_allow`;
  `cargo +nightly fmt -- --check`; `cargo clippy --all-targets -- -D warnings`; named mutations restored.
- [ ] **Step 5 — commit:** `feat(access): check XI2 pointer raw and scroll deliveries`.

### Task 3: Keyboard forms, grabs, raw keys and repeat (I10–I14)

**Files:** Modify `core_loop/key_fanout.rs`; create `tests/access_input_keys.rs`, register in `tests/mod.rs`.

**Interfaces:**
- Consumes Task 1 adapters and 4b checked egress. Preserves `pub fn key_event_fanout_to_state(state: &mut ServerState,
  backend: &mut dyn Backend, event: HostKeyEvent) -> Vec<ClientId>` and
  `pub fn raw_key_event_to_state(state: &mut ServerState, event: RawKeyEvent, key_was_down: bool,
  is_modifier: bool) -> Vec<ClientId>`.
- Changes private `fn send_raw_key(state: &mut ServerState, targets: &[ClientId], event: RawKeyEvent,
  deviceid: u16, sourceid: u16, delivery_window: ResourceId) -> (bool, Vec<ClientId>)`; root callers pass ROOT_WINDOW
  and use the failed-client list, the grabbed caller passes its actual natural/fallback window and uses the bool
  for Receive nondelivery, while preserving transport-failure handling. Also changes private
  `fn deliver_key_to_window(state: &mut ServerState, event: &HostKeyEvent, target_window: ResourceId,
  include_master_form: bool, include_core_form: bool, synthetic_repeat: bool) -> (bool, Vec<ClientId>)` and
  `fn deliver_key_to_grab_owner(state: &mut ServerState, event: &HostKeyEvent, owner: ClientId,
  grab_window: ResourceId, via_xi2: bool, device_id: u16, synthetic_repeat: bool) -> (bool, Vec<ClientId>)`.
  Their bool reports any actual permitted output using Task 1's adapter; routed public entry points still return
  only failed-client IDs. Existing focused/natural probe signatures stay unchanged;
  actual delivery results decide denial-dependent continuation. No authorization is stored in the queue.
- Test repeat entry consumes unchanged `pub(crate) fn fire_pending_repeats_at(state: &mut ServerState,
  backend: &mut dyn Backend, now: Instant) -> bool` in `core_loop/run.rs`; no production change there.

- [ ] **Step 1 — failing tests:** `receive_keys_check_focus_and_form_windows`: key entry with real focus set through
  dispatch, pointer descendant/focus-only/PointerRoot/None and attached/floating source; codes2/3 or XI evtype2/3,
  W(target), source/master bytes, selected-recipient isolation and physical key/modifier state. Deny all at child,
  allow bounded ancestor; deny XI2 and permit eligible core fallback without duplicate allowed XI2/core. Mutations:
  use final focus for every subject; terminate at denied selectors; undo key state on denial.
  `receive_key_grab_denial_keeps_capture`: core GrabKeyboard/GrabKey and XI2 XIGrabDevice/passive key grab via
  dispatch, both owner_events values; natural denied/grab allowed, both denied, natural allowed. Assert separate
  subjects, no nonowner leak, passive activation/store and active sync re-freeze/release unchanged. Mutations:
  no grab-window retry after natural denial; ungrab on denial; match passive grab again during replay.
  `receive_raw_key_distinguishes_grab_and_root`: slave root, master root, XI2 grabbed natural-root/fallback at another
  window; assert W(root)/W(grab), separate checks for the same owner when both existing paths deliver, XI2.0/2.1
  FilterRawEvents behavior unchanged, other roots retain bytes. Mutations: use D(device) or root for all checks;
  skip root selectors when grabbed delivery is denied; invent a core-grab raw form.
  `receive_repeat_keeps_protocol_forms`: arm a held repeating key and fire the real repeat entry at its recorded
  deadline. Core/XI1 classic clients keep release+press, detectable clients omit synthetic release, XI2 only a
  repeat-flagged press; check only events actually produced. Deny each produced form in rows and keep other clients,
  repeat scheduling and held-key state. Mutations: check/emit omitted release; omit repeat flag; stop timer on denial.
- [ ] **Step 2 — red gate:** `cargo test -p yserver-core access_input_keys`; expect missing checks/window fallback.
- [ ] **Step 3 — implement:** I10–I14, preserving detectable-repeat omission before checks. Use actual singleton
  delivery for natural/focused nondelivery and fallback, keeping current no-denial protocol selection. Pass the raw
  delivery window explicitly, preserve separate grabbed/root legs, raw key freeze and source guards, and don't
  change key transition/XKB state updates, repeat producer or admission.
- [ ] **Step 4 — green/mutation gate:** `cargo test -p yserver-core access_input_keys`;
  `cargo test -p yserver-core core_loop::key_fanout::tests`; `cargo test -p yserver-core fire_pending_repeats`;
  `cargo test -p yserver-core core_loop::process_request::tests::xi2_allow`;
  `cargo +nightly fmt -- --check`; `cargo clippy --all-targets -- -D warnings`; named mutations restored.
- [ ] **Step 5 — commit:** `feat(access): check keyboard and raw-key recipients through grabs and repeat`.

### Task 4: XI1 routed chains, focus/state and aggregate device notifications (I15–I19)

**Files:** Modify `core_loop/pointer_fanout.rs`, `core_loop/xi1_focus.rs`, `core_loop/xi1_state_notify.rs`;
create `tests/access_input_xi1.rs`, register in `tests/mod.rs`.

**Interfaces:**
- Consumes 4b/Task 1 egress, `EventDescription::extension`, unchanged
  `pub(crate) fn xi1_route_device_event(state: &mut ServerState, q: Xi1QueuedEvent, allow_freeze: bool) -> Vec<ClientId>`;
  `pub fn deliver_state_notify(state: &mut ServerState, deviceid: u16, window: ResourceId)`;
  `pub(crate) fn emit_device_focus(state: &mut ServerState, deviceid: u16, focus_in: bool, mode: u8,
  detail: u8, window: ResourceId)`.
- Preserves `pub(crate) fn emit_device_mapping_notify(state: &mut ServerState, originator: Option<ClientId>,
  deviceid: u16, request_kind: u8, first_keycode: u8, count: u8)` and
  `pub(crate) fn emit_change_device_notify(state: &mut ServerState, originator: ClientId, deviceid: u16,
  request_kind: u8)`. Changes private `fn xi1_fan_device_event(state: &mut ServerState, targets: &[ClientId],
  event_window: ResourceId, q: &Xi1QueuedEvent) -> (bool, Vec<ClientId>)` so its router can distinguish actual output
  from Receive nondelivery and failed writes. Produces no public API; batch shape uses the encoder's class facts.

- [ ] **Step 1 — failing tests:** `receive_xi1_chains_are_atomic`: real pointer/key entries and dispatched XI focus
  changes, with XI1 motion+DeviceValuator and state shapes: pointer without continuation, keyboard with
  DeviceKeyStateNotify, four-axis pointer with DeviceValuator. Assert ordered description offsets match every wire
  event, MORE_EVENTS bits and count unchanged. A policy denying when the continuation appears gets zero chain bytes;
  another subscriber gets the entire chain, input state unchanged. Mutations: describe lead only; gate continuation
  separately; D(device) for a window-delivered snapshot.
  `receive_xi1_focus_and_state_are_independent`: SetDeviceFocus/SetInputFocus and destruction reversion; deny focus
  while state allowed, then reverse, including state-only subscriber. Assert W(each transition window), mode/detail,
  request opcode or None for server reversion and unchanged focus/reversion. Mutation: return before state on denied
  focus; stop ancestor focus chain; reuse focus batch for state.
  `receive_xi1_grab_denial_keeps_sync_and_replay`: dispatched GrabDevice/GrabDeviceButton/GrabDeviceKey and real
  input; owner-events natural denied then fallback allowed, denied fallback still freezes/stores/releases normally;
  AllowDeviceEvents replays once to permitted current target. Mutations: treat denial as a dead owner; skip stored
  activating event; use failed natural route instead of grabbed fallback. A non-grab row denies all selectors at
  child and delivers to allowed ancestor within Xi1FocusRoute bounds; mutation: stop at selected denied child.
  `receive_xi1_device_notifies_are_device_scoped`: dispatched XI mapping/change requests with aggregate class
  subscriptions, originator handled by Task 7, others through I17/I18. Assert D(concrete device), offsets11/12, exact
  request major/minor, and no extra per-window copies; denial keeps maps/reply/core MappingNotify and other clients.
  Mutations: use Window(root), exempt XI DeviceMappingNotify with core type34, deny the whole operation.
- [ ] **Step 2 — red gate:** `cargo test -p yserver-core access_input_xi1`; expect missing checks/complete batches.
- [ ] **Step 3 — implement:** I15–I19; one check per whole chain. Shape descriptions before encoding from existing
  has-keys/axes facts, including the same optional key and valuator continuations. Adapt actual XI1 delivery walks
  for all-denied and owner-events fallback using the Task 1 result; preserve freeze, replay floor, state transitions
  and transport failures. Keep focus/state independent and originator exclusion in aggregate notifications.
- [ ] **Step 4 — green/mutation gate:** `cargo test -p yserver-core access_input_xi1`;
  `cargo test -p yserver-core core_loop::xi1_focus::tests`; `cargo test -p yserver-core core_loop::xi1_state_notify::tests`;
  `cargo test -p yserver-core core_loop::process_request::tests::xi1`;
  `cargo +nightly fmt -- --check`; `cargo clippy --all-targets -- -D warnings`; named mutations restored.
- [ ] **Step 5 — commit:** `feat(access): check XI1 input chains focus state and device notifications`.

### Task 5: XI2 focus, grab transitions and barriers (I20–I23)

**Files:** Modify `core_loop/fanout.rs`, `core_loop/process_request/xi/dispatch/grabs.rs`,
`core_loop/pointer_fanout.rs`; create `tests/access_input_transitions.rs`, register in `tests/mod.rs`.

**Interfaces:**
- Consumes Task 1 `describe_xi2_event` and 4b checked fanout. Preserves
  `pub fn emit_xi2_focus_event_to_state(state: &mut ServerState, window: ResourceId, evtype: u16,
  xi2_major_opcode: u8, mode: u8, detail: u8, root_x: i16, root_y: i16) -> Vec<ClientId>`;
  `pub(in crate::core_loop::process_request) fn deactivate_xi2_device_grab(state: &mut ServerState,
  backend: &mut dyn Backend, origin: Option<OriginContext>, owner: ClientId, deviceid: u16,
  is_keyboard: bool, is_master: bool)`.
- Preserves `pub(crate) fn emit_barrier_event(state: &mut ServerState, barrier_xid: u32, barrier_owner: ClientId,
  barrier_window: ResourceId, evtype: u16, time: u32, eventid: u32, dtime: u32, flags: u32, sourceid: u16,
  root_x: i32, root_y: i32, dx: f64, dy: f64) -> Vec<ClientId>`. No new API or grab admission checks.

- [ ] **Step 1 — failing tests:** `receive_xi2_focus_denial_keeps_core_transition`: SetInputFocus/XISetFocus and
  real host/input transition with ancestor/nonlinear windows; record XI evtypes9/10 and each W(window), with actual
  request or None. Deny only XI2 at one transition window; its core Focus event (4b) and other allowed windows/clients
  keep exact bytes, focus state unchanged. Mutations: use final focus as every subject; gate core and XI2 together.
  `receive_xi_grab_transition_denial_keeps_reply_and_lifetime`: XIGrabDevice/XIUngrabDevice for keyboard/pointer,
  replacement and same-window no-op crossing; deny each mode1/2 event separately. Grab reply/status and active grab,
  focus, confinement, thaw and release match Allow; allowed chain entries retained, no extra notification for no-op.
  Mutation: abort grab/ungrab on denied crossing or reuse grab window for all chain subjects.
  `receive_barrier_denial_keeps_constraint_and_release`: real pointer crossing against a barrier created through
  XFIXES request, then XIBarrierReleasePointer; check W(barrier window), XI evtype25/26, owner/window-grab override
  and normal selection. Denied hit/leave suppress only that recipient, motion remains constrained/released as control,
  eventid/dtime and other recipients unchanged. Mutation: return before barrier physics/state; D(master) as subject;
  fall through to selectors after denied matching grab owner.
- [ ] **Step 2 — red gate:** `cargo test -p yserver-core access_input_transitions`; fail on unchecked XI2 leaves.
- [ ] **Step 3 — implement:** migrate only I20–I23 to checked fanout, leaving 4b core focus gates independent.
  Describe each transition invocation, not all events from a grab as one batch; denial cannot stop chain generation,
  reply construction, ungrab or barrier updates. Keep current owner/window override and selector behavior.
- [ ] **Step 4 — green/mutation gate:** `cargo test -p yserver-core access_input_transitions`;
  `cargo test -p yserver-core core_loop::process_request::tests::xi2_grabs`;
  `cargo test -p yserver-core core_loop::pointer_fanout::tests`;
  `cargo +nightly fmt -- --check`; `cargo clippy --all-targets -- -D warnings`; named mutations restored.
- [ ] **Step 5 — commit:** `feat(access): check XI2 focus grab transitions and barrier recipients`.

### Task 6: Device properties, bootstrap and per-window hotplug delivery (I24–I30)

**Files:** Modify `core_loop/fanout.rs`, `core_loop/process_request/props.rs`, `core_loop/process_request/xi/mod.rs`,
`xinput/hotplug.rs`; create `tests/access_input_devices.rs`, register in `tests/mod.rs`.

**Interfaces:**
- Consumes Task 1 metadata and 4b fanout/manual `check_receive` with `receive_request`.
- Preserves `pub(crate) fn emit_property_change(state: &mut ServerState, deviceid: u16, property: AtomId,
  what: PropWhat) -> Vec<ClientId>`;
  `pub fn emit_xi2_device_changed_slave_pointer(state: &mut ServerState, xi2_major_opcode: u8) -> Vec<ClientId>`;
  private `fn emit_xi2_device_changed_bootstrap(state: &mut ServerState, backend: &mut dyn Backend,
  origin: Option<OriginContext>, client_id: ClientId, sequence: SequenceNumber, major_opcode: u8) -> io::Result<()>`
  (existing underscored parameter spellings may stay).
- Preserves `pub fn emit_xi1_device_presence(state: &mut ServerState, id: u16, change: DevicePresenceChange)
  -> Vec<ClientId>`; `pub fn emit_xi2_device_changed(state: &mut ServerState, id: u16,
  reason: XiDeviceChangeReason, sourceid: u16) -> Vec<ClientId>`; `pub fn emit_xi_hierarchy_changed(state:
  &mut ServerState, change: XiHierarchyStep, changed_id: u16, removed: Option<&XiDevice>,
  descriptor_snapshot: Option<XiHierarchyInfo>) -> Vec<ClientId>`. Produces no new signature.

- [ ] **Step 1 — failing tests:** `receive_device_properties_keep_changes_and_reply`: XI1/XI2 Change/Delete/GetProperty
  delete via dispatch, both forms selected, deny one form/recipient. D(concrete device), XI1 offset16 or XI2 evtype12,
  actual request opcodes; property content/deletion/backend call and complete GetProperty reply equal Allow,
  independent sibling form retained. Mutation: W(root) or deny whole output/delete.
  `receive_hotplug_uses_each_selection_window`: same client selects root and child plus another subscriber; drive
  Device Enabled changes through dispatch and real hotplug emitters for Added/Removed, DeviceChange/SlaveSwitch and
  hierarchy add/attach/detach/remove snapshots. Deny only one selected window per event kind; exactly one permitted
  copy remains for that same client, another client's copies unchanged, W(each selected window) and complete
  hierarchy/classes correct. Registry/property/enabled/attachment/class-source/last-slave state equal Allow, no
  retry for a denied window and no duplicate wildcard deliveries. Mutations: use changed device/Global as subject;
  dedup across windows; insert denied client into disconnected set; skip registry/source update after denial.
  `receive_bootstrap_checks_each_actual_master_event`: XISelectEvents through dispatch after both masters retained
  real source classes; policy denies only its first call. One Receive per existing master event, W(root), evtype1,
  `{137,46}`, first event absent, second complete with proper classes and issuer sequence; masks retained. With no
  retained source, no event/check. Mutation: one check for combined output; break after first deny; fabricate source.
  `receive_virtual_device_changed_is_root_scoped`: real virtual-pointer DeviceChanged entry with root/child and
  exact/wildcard selection; check W(root), only existing root recipients, deny one and retain others/class state.
  Mutation: admit child selection or D(XTEST device) instead of root.
- [ ] **Step 2 — red gate:** `cargo test -p yserver-core access_input_devices`; expect unchecked deliveries/buffer.
- [ ] **Step 3 — implement:** I24/I26–I30 use checked fanout with available selected window or concrete device.
  Restore `_selected_window` as the subject, preserving iteration and dedup granularity. I25 checks each retained
  master event before class/wire encoding, appending only allowed events to its existing combined buffer; no check
  at `write_to_client` and no loss of subsequent masters. Snapshot client sequence/order before read-only checks.
  Device-property forms remain independent; no resource/property-access hook is added for server work.
- [ ] **Step 4 — green/mutation gate:** `cargo test -p yserver-core access_input_devices`;
  `cargo test -p yserver-core core_loop::process_request::tests::xi_hotplug`;
  `cargo test -p yserver-core core_loop::process_request::tests::xi2`;
  `cargo test -p yserver-core xinput::hotplug::tests`;
  `cargo +nightly fmt -- --check`; `cargo clippy --all-targets -- -D warnings`; named mutations restored.
- [ ] **Step 5 — commit:** `feat(access): check XI device properties bootstrap and hotplug notifications`.

### Task 7: Inline and originator XI notifications (I31–I34)

**Files:** Modify `core_loop/process_request/xi/dispatch/control.rs`, `core_loop/process_request/xi/dispatch/devices.rs`;
extend test support `backend/recording.rs`; create `tests/access_input_inline.rs`, register in `tests/mod.rs`.

**Interfaces:**
- Consumes 4a `check_receive`, 4b scoped metadata/checked fanout and unchanged
  `xi1_client_wants_device_mapping_notify(state: &ServerState, client_id: ClientId, deviceid: u16) -> bool` and
  `xi1_client_wants_change_device_notify(state: &ServerState, client_id: ClientId, deviceid: u16) -> bool`.
- Preserves `fn xi_write_reply(state: &mut ServerState, client_id: ClientId, buf: Vec<u8>) -> io::Result<RequestOutcome>`
  (existing visibility unchanged). Produces no writer, buffer filter, signature change or new reply type.
- Test support produces RecordingBackend fields `pub keyboard_mapping_change_result: Option<KeyboardMappingChange>`
  and `pub modifier_mapping_change_result: Option<KeyboardMappingChange>`, both default None, and implements existing
  Backend signatures `fn change_keyboard_mapping(&mut self, first_keycode: u8, keysyms_per_keycode: u8,
  keysyms: &[u32]) -> Option<KeyboardMappingChange>` and
  `fn set_modifier_mapping(&mut self, modmap: &[u8; 256]) -> Option<KeyboardMappingChange>` by returning the seeded
  result. These methods currently inherit None; this extension is needed to generate real mapping followups through
  dispatch. Add RecordedCall variants `ChangeKeyboardMapping { first_keycode: u8, keysyms_per_keycode: u8,
  keysyms: Vec<u32> }` and `SetModifierMapping { modmap: Vec<u8> }` only on configured-result paths. Default None
  retains the old call log, so existing tests remain unmodified. This file is already a test-only module.

- [ ] **Step 1 — failing tests:** `receive_inline_xi_denial_keeps_reply_and_mapping`: table-driven requests
  ChangeDeviceKeyMapping(minor25), SetDeviceModifierMapping(27), SetDeviceButtonMapping(29), ChangeKeyboardDevice(11),
  selected issuer plus two other clients. Record D(dev), offsets11/12, exact request major/minor once per actual
  event; issuer sequence/byte order, each existing relative event/reply order fixed independently. Deny selected
  issuer: modifier reply survives with no pre-reply event; pointer reply survives with no appended event; keyboard
  change keeps exempt MappingNotify before reply; void keymapping keeps all independent XKB/core followups. Deny an
  other recipient instead and keep issuer bytes. Maps, selection masks, success/busy status and backend calls equal
  Allow control. Mutations: drop mixed buffer; exempt appended notification; send issuer through aggregate fanout
  a second time; Receive-check core MappingNotify. Unselected issuer/busy mapping rows ask nothing for absent event.
  `receive_inline_denial_is_independent_of_wire_cause`: XI key/modifier mapping triggers XKB with its existing core
  cause (100/118), using the seeded mapping result; the XI Receive context must still report `{137,25/27}` while
  adjacent XKB wire cause stays 100/118. Mutation: use XKB cause as receive_request. Task 8 adds the XKB Receive
  assertions after its leaves are checked; this task does not depend on future XKB checks.
- [ ] **Step 2 — red gate:** `cargo test -p yserver-core access_input_inline`; fail for unchecked issuer events.
- [ ] **Step 3 — implement:** I31 uses checked singleton fanout at its existing pre-reply position. I32–I34 ask
  Receive after the selection predicate, before just that encoder appends to buf; borrow state read-only and use
  original sequence/order. Preserve core MappingNotify unconditionally where currently unconditional, the entire
  reply and nonoriginator fanout through Task 4. Do not change `xi_write_reply` or redispatch accepted notifications.
- [ ] **Step 4 — green/mutation gate:** `cargo test -p yserver-core access_input_inline`;
  `cargo test -p yserver-core core_loop::process_request::tests::xi1`;
  `cargo test -p yserver-core access_send`; `cargo +nightly fmt -- --check`;
  `cargo clippy --all-targets -- -D warnings`; named mutations restored.
- [ ] **Step 5 — commit:** `feat(access): check inline XI notifications without dropping replies`.

### Task 8: Native XKB notifications and exempt mapping companions (I35–I46)

**Files:** Modify `core_loop/fanout.rs`, `core_loop/xkb_layout.rs`, `core_loop/xkb_controls.rs`,
`core_loop/process_request/xkb.rs`; extend test support `backend/recording.rs`; create `tests/access_input_xkb.rs`,
register in `tests/mod.rs`.

**Interfaces:**
- Consumes 4b checked egress and existing XKB recipient helpers. Produces in `fanout.rs`
  `pub(crate) fn describe_xkb_event(event_base: u8) -> EventDescription`: use the advertised XKEYBOARD major from
  `nested::EXTENSIONS`, ordinary event offset0; use core(0) for the existing unavailable-backend base0 fallback.
  Advertised XKB bases are >=64; do not add validation or event production for an unavailable extension.
  This shares the runtime-base rule at twelve sites without adding a second extension table or passing a Backend
  through leaves that already receive the base.
- Preserves `pub(crate) fn send_xkb_map_notify(state: &mut ServerState, xkb_event_base: u8,
  notify: x11::XkbMapNotify) -> Vec<ClientId>`; it still returns selected recipients, not only allowed ones.
  Preserves `pub fn announce_xkb_state(state: &mut ServerState, backend: &mut dyn Backend, cause: XkbStateCause)`;
  `pub(crate) fn send_controls_notify(state: &mut ServerState, backend: &dyn Backend, old: RepeatControls,
  cause: (u8, u8))`; `fn send_xkb_set_events(state: &mut ServerState, backend: &dyn Backend, major: u8,
  minor: u8, events: &[XkbSetEvent])`. Remaining XKB emission signatures stay unchanged. No policy-ABI extension.
- Test support produces RecordingBackend fields `pub xkb_set_outcomes: HashMap<u8, XkbSetOutcome>` (empty default)
  and `pub xkb_indicators_result: (u32, u32)` (default `(0, 0)`), implementing the existing Backend signatures
  `fn xkb_set(&mut self, minor: u8, body: &[u8], client_is_ancient: bool,
  atom_name: &dyn Fn(u32) -> Option<String>) -> Option<XkbSetOutcome>` and
  `fn xkb_indicators(&mut self) -> (u32, u32)`. Return the seeded outcome for that minor / seeded indicators;
  record `RecordedCall::XkbSet { minor: u8, body: Vec<u8>, client_is_ancient: bool }` only when an outcome was seeded.
  These currently inherit None/(0,0); defaults and existing logs stay unchanged. This narrow extension is required
  to exercise I37–I42/I46 through the dispatcher rather than calling notification helpers as test substitutes.

- [ ] **Step 1 — failing tests:** `receive_xkb_notifications_cover_every_emitter`: table rows drive ChangeProperty
  `_XKB_RULES_NAMES` (I35), ChangeKeyboardMapping (I36/I41), XKB SetCompatMap (I37), SetNames (I38/I39),
  SetIndicatorMap (I40/I42), LatchLockState and real input (I43 plus indicator state/extension-device followups),
  core ChangeKeyboardControl and XKB SetControls (I44), GetKeyboardByName(load) (I45), backend Set* NewKeyboard
  result through real XKB dispatch (I46). Seed RecordingBackend's existing and above XKB result fields; select details through
  dispatch. Each row records exact existing recipients, D(wire device, 1 for current producers), request or None,
  one event description and its real subtype/payload on allowed wire. Deny one client, retain another, state/keymap/
  repeat/LED changes and reply identical to Allow. I42 has separate Map/State rows, I40 separate LED reason rows.
  Mutations: bypass each site's gate; D(3); abort later XKB notifications on first denial; return allowed-only I36 list.
  `receive_xkb_metadata_uses_event_base_not_subtype`: use two advertised bases and different host-vs-local major
  fixture values; NKN/Map/State/etc all ordinary `(type=base, local XKEYBOARD major, event_type=0)` as the unchanged
  4a wrapper decodes their actual wire headers. Mutation: use xkbType as event_type; hardcode event base; use host major.
  `receive_xkb_denial_advances_announced_snapshot`: deny state/indicator notifications, then repeat identical
  no-change input/lock; no repeated notification/check, snapshots and LED state equal control. Mutation: update
  xkb_announced or xkb_announced_indicators only after allowed delivery.
  `receive_xkb_denial_keeps_later_backend_error`: seed a Set* outcome with two notifications and a trailing existing
  error; deny the first, permit the second. The second event and original error code/value/opcodes/order remain,
  backend call and core partial effects equal Allow. Mutation: return early on denied first notification or clear
  outcome.error after filtering. Also apply Task 7's XI-vs-wire-cause row to the now-checked XKB notifications.
  `receive_mapping_companions_are_exempt`: deny every Receive, drive all pointer/modifier/keyboard MappingNotify
  sources, including legacy XKB helper and all inline XI forms; type34 has zero Receive calls and original bytes/
  ordering while adjacent native XKB and XI DeviceMappingNotify are denied. Mutation: migrate the old MappingNotify
  writer to checked egress. This exemption test passes before implementation; do not invent a red failure for it.
  Existing BE-XKB request refusal stays unchanged; use BE receiving clients for server broadcasts where currently
  supported, not a new BE dispatch capability.
- [ ] **Step 2 — red gate:** `cargo test -p yserver-core access_input_xkb`; notification rows fail for absent checks;
  capture literal/structural wire expectations and existing exemption results before changes.
- [ ] **Step 3 — implement:** I35–I46 through checked fanout, dynamic base descriptions and wire device subjects.
  Preserve detail masks, initialization, original recipient return values, followup order, legacy mappings, keymap
  load success and announced snapshots even if everybody is denied. Do not modify `xkb_select.rs`'s core MappingNotify
  emitters or gate them on an XKB delivery result. No XKB subtype is added to EventDescription.
- [ ] **Step 4 — green/mutation gate:** `cargo test -p yserver-core access_input_xkb`;
  `cargo test -p yserver-core core_loop::process_request::tests::xkb`;
  `cargo test -p yserver-core core_loop::xkb_`; `cargo test -p yserver-core access_input_inline`;
  `cargo +nightly fmt -- --check`; `cargo clippy --all-targets -- -D warnings`; named mutations restored.
- [ ] **Step 5 — commit:** `feat(access): check native XKB notifications and preserve legacy mappings`.

### Task 9: Deferred revocation, no-policy parity, coverage/status and final gate

**Files:** Create `tests/access_input_replay.rs`, register in `tests/mod.rs`; extend the new test files as needed;
modify `docs/status.md`. No additional production changes planned unless these named tests expose an omission.

**Interfaces:**
- Consumes Tasks 1–8 and 4b's fixture/context. Existing canonical queues and AllowEvents/AllowDeviceEvents/XIAllowEvents
  dispatch are the replay entry points; no new queue representation or test-only production entry point.
- Consumes unchanged `pub(crate) fn xi1_compute_freezes(state: &mut ServerState, backend: &mut dyn Backend,
  xid_map: &HostXidMap)` only where its production server-thaw entry is required. Produces no API.

- [ ] **Step 1 — tests:** `receive_frozen_input_rechecks_on_thaw`: table covers canonical HostPointer, HostKey,
  HostKeyTransition, Xi1Routed and RawKey queues, with core and XI2/XI1 sync grabs installed by requests. Enqueue
  while Allow, revoke before dispatched thaw/replay; no premature Receive for withheld batches, fresh Receive at
  delivery, no denied event bytes or unrelated loss, correct capture/refreeze/store/release and current focus/window.
  Reverse Deny→Allow before delivery and receive normally; repeat once with policy changed between immediate events.
  Slave raw delivery that precedes master freeze remains independently checked once. Mutations: cache enqueue-time
  Allow; skip replay checks; gate both raw forms with master freeze; count denial as disconnected.
  `receive_replay_revalidates_source_and_request_context`: change focus, detach/remove source while pending, then
  replay; stale origin emits/asks nothing; live origin uses current window and current grant. Dispatch thaw records
  its real major/minor (core and XI forms); after successful/invalid requests independent server thaw/input records
  None. State/queue cleanup and allowed recipients match control. Mutations: use queued source without validation;
  retain original request opcodes; leak thaw opcodes to subsequent input. Xorg reprocesses queued facts in
  `dix/events.c:1232-1294, 1318-1372`; don't authorize previously encoded bytes again.
  `receive_input_no_policy_bytes_match_baseline`: reuse I01–I46 fixtures with no policy and recording Allow policy;
  compare both with independent expectations captured at each red step, not merely each other. Cover LE/BE supported
  recipients, distinct sequences, socket and existing buffered output, no selector, no-op/busy, stale/floating source,
  crossings, all grab routes, chain lengths, two-axis scroll, inline reply/event order, XKB runtime base/legacy forms.
  Time-bearing fields use explicit input times or recorded/state timestamps/before-after server bounds, not blanket
  masking. A deny-all row dispatches the existing 4b CopyArea/CopyPlane fixtures: GraphicsExpose/NoExpose bytes and
  replies survive, with zero Receive for types13/14, as do MappingNotify and an independent XFIXES/RANDR sibling
  generated by an existing operation. No reimplementation of 4b's exemption sites. Mutations:
  normalize all sequences/order; drop a later extension sibling; clear already-allowed pending output on drain
  after revocation. Assert buffered bytes from before revocation still drain and trigger no new Receive. This parity row
  should pass before implementation; its named mutations, not missing hooks, establish sensitivity.
  `receive_xtest_input_has_no_send_hook`: dispatch FakeInput with selected input recipients, allowing admission and
  denying one Receive. Input state, success and other recipient bytes equal Allow; the denied event is absent and
  no Send hook is recorded. Mutation: add Send to the generated-input path or skip its shared Receive site. Do not
  change xtest.rs, admission policy or retain the issuer in queued input.
- [ ] **Step 2 — audit:** map each I01–I46 to its ASK, DENY/isolation and baseline row. Re-search all input/XI/XKB
  encoders, direct writes, closure/inline egress and thaw routes; no unchecked in-scope expression, double-check of
  4a SendExtensionEvent, or core-focus duplication. Check chain descriptions against actual wire event counts and
  all no-encoder suppression before Receive. Run/restore named mutations. Confirm no-policy shortcuts and <6000 lines
  in every new test file. No new code-11/ResizeRequest producer is smuggled into this change.
- [ ] **Step 3 — status:** update `docs/status.md` for completed 4c coverage, input denial/grab behavior, remaining
  KeymapNotify/ResizeRequest gaps and 4d/4e/PR5 work. Do not claim device authorization, RECORD coverage, clipboard
  isolation or completion of all PR4. Leave binding spec and approved plans unchanged.
- [ ] **Step 4 — final gate (implementation time only):** `cargo +nightly fmt -- --check`;
  `cargo clippy --all-targets -- -D warnings`;
  `cargo clippy --all-targets --features tcp-transport -- -D warnings`;
  `cargo clippy --all-targets --features xdmcp -- -D warnings`; in a subshell source the existing
  `/home/ariel_santangelo/Projects/claude-scratch/freebsd-sysroot/env.sh`, then
  `cargo clippy --target x86_64-unknown-freebsd --all-targets -- -D warnings` and
  `cargo clippy --target x86_64-unknown-freebsd --all-targets --features xdmcp -- -D warnings`;
  `cargo check -p yserver-core --target x86_64-pc-windows-gnu`; `cargo test --workspace`;
  `cargo test --workspace --features xdmcp`; CI's `cargo test --all-targets --locked` in default/tcp-transport/xdmcp
  configurations; `cargo test --workspace --tests --locked` (all existing integration targets);
  established CI software-Vulkan/lavapipe `cargo test --workspace --locked --features xdmcp --no-fail-fast -- --ignored`;
  `sh tools/check-layout.sh`; `just man`. Report unavailable prerequisites as unrun, never passing. No hardware.
  Run `cargo test -p yserver-core access_` five times with `nproc` background CPU workers; a shell trap kills only
  their recorded PIDs on every exit, including test failure. All new modules have the access_ prefix. Record all
  five successful runs and confirm the original suites pass unmodified.
- [ ] **Step 5 — commit:** `test(access): verify input receive revocation parity and coverage`.

## Estimated implementation size and self-review

Changed-line estimates are additions plus edits, not whole-file lengths, unchanged moved bodies or this plan.
They include denial-dependent input routing and tests, not just mechanical call replacements. Paths relative to
`crates/yserver-core/src/`; estimates are for deciding PR size, not implementation budgets.

| Production file | Estimated changed lines |
|---|---:|
| `access.rs` | 35 |
| `core_loop/process_request/selection.rs` | 25 |
| `core_loop/fanout.rs` | 80 |
| `core_loop/pointer_fanout.rs` | 420 |
| `core_loop/key_fanout.rs` | 240 |
| `core_loop/xi1_focus.rs` | 40 |
| `core_loop/xi1_state_notify.rs` | 35 |
| `core_loop/process_request/props.rs` | 25 |
| `core_loop/process_request/xi/mod.rs` | 25 |
| `core_loop/process_request/xi/dispatch/grabs.rs` | 30 |
| `core_loop/process_request/xi/dispatch/control.rs` | 40 |
| `core_loop/process_request/xi/dispatch/devices.rs` | 15 |
| `xinput/hotplug.rs` | 40 |
| `core_loop/xkb_layout.rs` | 100 |
| `core_loop/xkb_controls.rs` | 12 |
| `core_loop/process_request/xkb.rs` | 20 |
| **Production total** | **1182** |

| Test file (`core_loop/process_request/tests/`) | Estimated changed lines |
|---|---:|
| `access_input_pointer.rs` | 550 |
| `access_input_xi2_pointer.rs` | 500 |
| `access_input_keys.rs` | 550 |
| `access_input_xi1.rs` | 500 |
| `access_input_transitions.rs` | 350 |
| `access_input_devices.rs` | 550 |
| `access_input_inline.rs` | 350 |
| `access_input_xkb.rs` | 550 |
| `access_input_replay.rs` | 450 |
| `mod.rs` | 9 |
| `backend/recording.rs` (relative to `src/`, test-only support) | 110 |
| **Test total** | **4469** |

`docs/status.md`: about 12 lines. **Total: 5663 lines (1182 production, 4469 tests, 12 docs).**
Shared production files with 4b: `core_loop/fanout.rs`, `core_loop/process_request/props.rs`,
`core_loop/process_request/selection.rs`. Both also edit `tests/mod.rs` and `docs/status.md`; 4c consumes 4b's
`access_notify_support.rs` unchanged. The input routing and deferred-grant tests account for most of 4c's size.

Self-review completed against the binding sections: all 46 current emission expressions have an owning task;
GenericEvent/ordinary-XI/XKB identities, real request context, selected-window versus device subjects, server work,
atomic chains, mixed replies, active/passive/implicit grabs and deferred revocation are covered. Inherited redirect,
selection-refusal and direct-result exemptions remain explicit; no user ruling is needed for a new exception.
Xorg raw-root context, normal nondelivery and grabbed pretend-delivery were checked in 21.1.24, as were the native
XKB and passive activation bypasses. The absence of KeymapNotify is a finding, not a fabricated check site. All five
Review Focus classes have named task tests. 4b's signatures remain unchanged; the pure decoder and singleton result
adapter have stated requirements, exact signatures and production-path tests; no test depends on an undefined
fixture or grant mechanism. Nine tasks, no function bodies, no new dependencies or expanded authorization scope.
No open question requires a user decision: the spec fixes policy coverage and Xorg supplies the remaining routing
semantics. Authoring was read-only apart from this file: no implementation, git writes, build, test or hardware action.
