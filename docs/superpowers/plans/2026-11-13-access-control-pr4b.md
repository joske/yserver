# Access control layer, PR 4b — core notifications Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Check Receive for every existing core notification delivery in scope, including server-originated and
rootless deliveries, without changing any client-observable byte when no policy is installed.

**Architecture:** Add checked entry points sharing the existing closure/raw fanout loops; migrate only the core
notification sites below. Carry request opcodes through a scoped field around the production dispatcher, so shared
emitters see the actual request, including extension minor opcodes, and subsequent server work sees `None`. Keep
redirect selection independent of receive success, and use the existing conversion-failure answer after a denied
SelectionRequest. No new policy interface or authorization mode.

**Tech Stack:** Rust, crate `yserver-core`; existing protocol encoders, recording backend and access policy; std only.

**Spec:** `docs/superpowers/specs/2026-10-30-access-control-layer-design.md`, “What a policy can see”, “A denial is
interpreted by its site”, “Who is checked”, “Where the checks go” → Receive, including the merged 4a/follow-up rulings.

**Reference:** Xorg (MIT), **xorg-server 21.1.24**, `/data/Projects/xorg-21.1.24`. Every Xorg path and line below refers
to that tree, rechecked for this plan. The spec binds where it decides; otherwise follow Xorg. No XLibre reference.

**Source inventory:** `/home/ariel_santangelo/Projects/claude-scratch/access-control/pr4/inventory.md`, taken at
`d14eaecb`; §3 common egress, generic fanout/exposure, core windows/structure, destroy/overlay/focus/mapping/properties/
selections and GraphicsExpose/NoExpose; §5–7. Sites below were relocated at **`c3d43e3a`**, branch
`feat/access-control-pr4b`, after #363 and #367. Source line numbers are navigation anchors, not proposed edit ranges.

## Global Constraints

- With no policy installed, every byte a client observes stays the same, including errors and their precedence,
  replies, event order, padding, send-event bit, sequence numbers and byte order. The existing suite passes unmodified.
- No new validity errors, stricter request validation, event production, recipient selection or propagation rules.
  Preserve existing no-op, absent-client, mask, duplicate-recipient and failed-write handling.
- Helpers follow `check_resource`'s shortcuts: empty policy list → `Allow`, no context construction, owner resolution
  or dynamic policy call. Reuse `ServerState::check_receive` and its first-denial order. Unlike `check_resource`, Receive
  does **not** exempt `SERVER_OWNER` or internal recipients; 4a already made that distinction.
- Reuse first: fanout loops, encoders, `subscribers_by_id`, `client_target_id`, `selection_owner_target_id`, existing
  selection refusal logic, `RecordingExtensionPolicy` in `process_request/tests/extensions.rs`, and `access_send.rs`
  fixture/decision patterns. No second recipient resolver, no decision cache, no new mechanism without a requirement
  stated below. No environment variable and no `cfg(test)` in production bodies.
- Server-generated notifications have no Send check. Receive still runs for their recipients, including on
  disconnect, destruction, focus reversion, host exposure and rootless work. Core notifications caused while a
  request executes carry that request's `Some(RequestOpcodes)`; independent server work carries `None`.
- Check before per-recipient encoding, telemetry and output buffering, with only a read-only state borrow. Snapshot
  subjects/targets out of resource or rootless borrows first; obtain `clients.get_mut` only after the decision.
  Revocation takes effect at the next delivery; already-buffered output is not recalled.
- Tests drive `process_request`, or the actual event-generating entry point for server events, using the recording
  backend. Setup may seed resources, masks and backend facts; do not make helper-only tests stand in for a delivery
  site. No hardware is needed for the site tests.
- Every DENY row asserts the decision's effect **and** isolation: no bytes for that event to the denied recipient,
  byte-identical delivery to allowed recipients, no new error/disconnect, and no state change caused by the denial.
  For notifications, compare underlying state/backend effects with an Allow control: the successful operation still
  happens (a denied PropertyNotify does not undo ChangeProperty). For redirects compare with pre-request state too;
  for intercepted events only the denied queue insertion is absent. Do not mistake expected operation effects for
  authorization effects, or compare only socket bytes while overlooking buffered output.
- Every new test names its compiling mutation in a comment. Run each mutation against its named test, observe failure,
  restore it, then pass the task gate; no mutation is committed. Suites run under CPU load. Test files stay below
  6000 lines. Use bounded nonblocking output drains, not sleeps or short timing thresholds.
- Work on this feature branch. Each task has a reviewable commit; run `cargo clippy --all-targets -- -D warnings`
  before every commit. Format with `cargo +nightly fmt`. No merge is authorized by this plan; follow AGENTS.md when
  integration is requested. Xorg is MIT; read it, do not copy code.

## Review Focus

1. **One recipient selected at two windows:** parent and child subscriptions must ask with the delivery window, even
   when both events name the same affected child. Denying one must keep the other. Task 3:
   `receive_structure_subject_is_delivery_window`.
2. **Every redirect recipient denied:** hiding the WM notification must never execute the requested map/configure/
   circulate. Task 4: `receive_redirect_denial_preserves_redirection` (all three existing emitters).
3. **Selection setter/requester differs from window creator, or owner is the bridge:** address the setter/requesting
   client, check the bridge as `SERVER_OWNER`, and answer a denied owner without waiting. Task 7:
   `receive_selection_request_denied_answers_actual_requester` and `receive_bridge_selection_denial_answers_requester`.
4. **Request followed by asynchronous server work:** no stale major/minor opcode, including after early validation
   returns; actual extension minor retained for core COW events. Tasks 1/5:
   `receive_request_context_does_not_leak_to_host_expose`, `receive_cow_notifications_keep_extension_request`.
5. **Mixed output and dependent cleanup:** denial must preserve GetProperty's reply/deletion, destruction/focus
   cleanup and unrelated extension events; exempt direct results and MappingNotify must survive deny-all Receive.
   Tasks 5/6/8: `receive_destroy_denial_keeps_cleanup`, `receive_get_property_delete_keeps_reply`,
   `receive_exempt_results_and_mapping_bypass_policy`.

## Scope decisions

- **Coverage unit:** 47 semantic delivery expressions/bridge interception sites, IDs N01–N47 below, plus three common
  egress rows E1–E3. This is a source-site count, not 47 independent hooks or runtime recipients. One static core-focus
  site emits both FocusIn and FocusOut. Exempt/absent paths are listed separately and excluded from the count.
- **Typed metadata before encoding:** use the existing `EventBatch::new(events: &[EventDescription]) -> EventBatch<'_>`
  and `EventDescription::core(type_code: u8) -> EventDescription`. Codes here: FocusIn/Out 9/10, Expose 12, Visibility
  15, Create 16, Destroy 17, Unmap 18, Map 19, MapRequest 20, Reparent 21, Configure 22, ConfigureRequest 23, Gravity
  24, Circulate 26, CirculateRequest 27, Property 28, SelectionClear 29, SelectionRequest 30, SelectionNotify 31,
  Colormap 32, ClientMessage 33. The batch describes exactly the event(s) that invocation encodes, in order, with
  `extension: None` and the send bit removed. Do not describe all events of a request as one batch. Existing Expose
  loops check each emitted rectangle separately; keep their count fields and extents collapse.
- **Subject:** `Window(delivery_window)`, not the affected child, property atom, selection atom or colormap XID.
  StructureNotify uses the changed window; SubstructureNotify/Redirect uses the parent/container; each focus-chain
  event uses its transition window. SelectionClear uses the old owner window; SelectionRequest the owner's window;
  rootless SelectionNotify the requestor window. Subjectless notifications use `EventSubject::Global`, never a fake
  root or XID 0. No newly checked subjectless core notification exists here: MappingNotify is exempt. `Drawable`
  belongs to later checked paths; GraphicsExpose/NoExpose do not ask Receive at all.
- **Shared egress without moving 4c–4e:** keep the existing signatures and behavior of `fanout_event_to_clients`,
  `emit_window_event_to_state`, `fanout_raw_event_to_clients` for callers not migrated here. Add the three required-
  metadata checked entry points in Tasks 1/7, backed by the **same** loops. The old entry points pass no receive
  metadata internally; only N01–N47 opt in. This transitional boundary is required by the split scope: input, XI,
  XKB, extension, RANDR and RECORD callers must not start receiving partial or wrongly described checks in 4b.
  In particular, 4a core/XI explicit sends retain their own checks and use the old writers: no double check. Later
  plans migrate their own callers and can retire the unchecked entry points when all remaining uses are exempt.
- **Request context requirement:** shared emitters are also called by server work and by extensions and generally
  have no request header; `OriginContext` has no minor opcode. Add only
  `ServerState::receive_request: Option<RequestOpcodes>`, initialized to `None`, and a wrapper around the existing
  `process_request` body. Save the previous value, install `Some(RequestOpcodes::for_request(header))`, call the body,
  restore the previous value for every returned `io::Result`, including early validation/error returns. The inner
  function retains all validation order, sequence stamping and backend synchronization. No policy decision/context is
  stored. This scoped metadata is the only new state mechanism, needed to meet the spec's actual-opcode contract
  without threading request parameters through unrelated extension/input APIs. Checked egress and manual core
  notification gates read it. No external event loop runs inside the synchronous dispatch; reentrant requests must
  restore the outer value. Do not infer opcodes from recipients' sequence counters or from `OriginContext`.
- **Denial:** ordinary notification → omit only that recipient's event, no X error; keep the operation and other
  deliveries. Denial is not a transport failure and never enters the disconnected-client return vector. Redirect
  MapRequest/ConfigureRequest/CirculateRequest → also keep the existing redirected branch/early return even if nobody
  got bytes (Xorg `dix/events.c:2557-2587`; caller examples `dix/window.c:2307-2309, 2470-2474, 2621-2631`). Do not
  refactor branch choice to depend on successful receive counts.
- **ResizeRequest finding:** no production emitter or encoder exists in the current tree. The ResizeRedirect mask
  is selectable/exclusive (`windows.rs:1727-1746`), but ConfigureWindow does not emit ResizeRequest or perform that
  redirection. Do not add it in this receive-only PR: it would change no-policy behavior. Record the gap in status.
  Whenever that separate fix adds it, its subject is the resized window and denied Receive counts as delivered;
  Xorg `dix/window.c:2311-2330` fixes that behavior. There is no fabricated check/test site here.
- **Selections:** receive-check SelectionClear and SelectionRequest without adding selection access/mediation.
  Setter identity, not window creator, identifies the owner recipient. A denied SelectionRequest follows the existing
  no-owner refusal path, with `SelectionNotify(property=None)` to the **ConvertSelection issuing client**, even when
  another client created the requestor window; the bridge requestor receives its existing `note_refusal` answer.
  Both refusals are exempt direct results. No owner/selection/property mutation, no BadAccess. Xorg emits directly
  without Receive (`dix/selection.c:183-190, 285-311`); the spec deliberately adds it. An owner's explicit SendEvent
  SelectionNotify stays under 4a. Host-generated `send_selection_notify`, even with property None, remains a checked
  notification to the requestor-window creator; it is not the server's ConvertSelection failure result.
- **Rootless:** check bridge SelectionClear/Request and watched PropertyNotify before queue insertion as
  `SERVER_OWNER`. Denial must not affect normal socket recipients or undo ownership/property changes. 4a's
  SendEvent interceptions remain untouched. `SelectionEvent::Owner` is XFIXES and deferred to 4d;
  `SelectionEvent::Destroyed`/watch removal is lifetime cleanup and must still run. Xorg has no matching host-bridge
  queue; apply the spec's recipient contract, with the corresponding Xorg core delivery as reference below.
- **Focus boundary:** all core FocusIn/Out emitted by `emit_core_focus_transition`, including grab/ungrab, reversion,
  host activation and input-caused transitions, are in 4b. Its XI2 forwarding remains unchanged. Pointer/keyboard
  data, Enter/Leave, KeymapNotify, XI focus/state and grab fanout belong to 4c. This is a core-focus leaf change,
  not authorization of the input operation.
- **RANDR boundary:** its four core ConfigureNotify expressions also stay with 4e, as part of the explicitly
  excluded RANDR group: `P/randr_ext.rs:send_root_config_notify` (128),
  `C/run.rs:emit_screen_resize_window_notifications` (3981, 4022, 4039). They are not mistaken for extension event
  codes or counted among N01–N47. No screen-resize producer is changed here.
- **Exemptions and transport:** MappingNotify (all pointer/modifier/keyboard and inline XI-origin core forms) and
  CopyArea/CopyPlane GraphicsExpose/NoExpose stay on existing writers, without a Receive call. The only other core
  event-capable `write_to_client` writes are those direct results. Do not parse arbitrary buffers in
  `write_to_client` or `client_io::write_or_buffer`; replies, errors, extension mixed buffers and buffer drain keep
  their semantics. `write_to_client`'s signature stays unchanged. Present/SHM/SYNC exemptions are for 4d, not changes
  here. Dormant legacy `server.rs` event writers with no production callers are out of scope.

## Site table

Paths use `C/ = crates/yserver-core/src/core_loop/`, `P/ = C/process_request/`,
`R/ = crates/yserver-core/src/rootless/`. Xorg paths are relative to the Reference above.
`W(x)` means `EventSubject::Window(x)`. **Drop** means suppress only this event for the denied recipient, operation
unchanged; **Redirect** means Drop and keep redirection; **Refuse** means the exempt SelectionNotify(None) answer.
All task tests also apply the Global Constraints' DENY isolation assertions.

| ID / task | Event(s) | Current file:function (line) | Recipient | EventSubject | Denial effect | Xorg 21.1.24 reference |
|---|---|---|---|---|---|---|
| E1 / 1 | Described core event(s) | `C/fanout.rs:fanout_event_to_clients` (269) | Deduplicated existing client IDs | Required from migrated caller | Drop before encode; no disconnect | `dix/events.c:2202-2224, 2270-2295` |
| E2 / 1 | Described core event(s) | `C/fanout.rs:emit_window_event_to_state` (307) | Existing core-mask subscribers | W(window) | Delegate to E1 once per recipient | `dix/events.c:2951-3013` |
| E3 / 7 | Described raw core event | `C/fanout.rs:fanout_raw_event_to_clients` (636) | Deduplicated existing client IDs | Required from migrated caller | Drop before copy/swap/sequence/telemetry | `dix/events.c:2202-2224, 2270-2295` |
| N01 / 2 | Expose | `C/fanout.rs:expose_event_fanout_to_state` (538) | Host-resolved window Exposure subscribers | W(window) | Drop | `mi/miexpose.c:304-323, 359`; E1 |
| N02 / 2 | Descendant Expose | Same (555) | Each affected descendant's Exposure subscribers | W(target_window) | Drop; continue descendants | Same as N01 |
| N03 / 2 | VisibilityNotify | `C/fanout.rs:emit_visibility_unobscured_subtree_to_state` (600) | Each viewable descendant's Visibility subscribers | W(child) | Drop; continue recursion | `dix/window.c:3084-3089`; E1 |
| N04 / 2 | Expose | `P/windows.rs:send_window_exposures` (112) | Window Exposure subscribers, per rectangle | W(window) | Drop; paint/remaining rectangles unchanged | `mi/miexpose.c:304-323, 359, 365-382` |
| N05 / 3 | UnmapNotify | `P/windows.rs:handle_reparent_window` (563) | Changed-window Structure subscribers | W(window) | Drop | `dix/window.c:2518-2519, 2826-2833`; E2 |
| N06 / 3 | UnmapNotify | Same (567) | Old-parent Substructure subscribers | W(old_parent) | Drop | Same as N05; E2 |
| N07 / 3 | ReparentNotify | Same (744) | Changed-window snapshot | W(window) | Drop | `dix/window.c:2521-2535`; `dix/events.c:2999-3011` |
| N08 / 3 | ReparentNotify | Same (757) | Old-parent snapshot | W(old_parent) | Drop | Same as N07 |
| N09 / 3 | ReparentNotify | Same (770) | New-parent snapshot (empty for same parent) | W(new_parent) | Drop | Same as N07 |
| N10 / 3 | Remap MapNotify | Same (784) | Changed-window Structure subscribers | W(window) | Drop | `dix/window.c:2641-2642`; E2 |
| N11 / 3 | Remap MapNotify | Same (788) | New-parent Substructure subscribers | W(new_parent) | Drop | Same as N10 |
| N12 / 3 | CreateNotify | `P/windows.rs:handle_create_window` (1258) | Parent Substructure snapshot | W(parent) | Drop; creation succeeds | `dix/window.c:952-953`; E1 |
| N13 / 4 | ConfigureRequest | `P/windows.rs:configure_window_apply` (1985) | Existing parent redirect targets | W(parent) | Redirect; geometry/stack unchanged | `dix/window.c:2299-2309`; `dix/events.c:2560-2585` |
| N14 / 3 | ConfigureNotify | Same (2093) | Changed-window Structure subscribers | W(window_id) | Drop | `dix/window.c:2383-2390`; E2 |
| N15 / 3 | ConfigureNotify | Same (2107) | Parent Substructure subscribers | W(parent) | Drop | Same as N14 |
| N16 / 3 | GravityNotify | Same (2199) | Moved-child Structure subscribers | W(child) | Drop; child moves | `dix/window.c:1867-1868`; E2 |
| N17 / 3 | GravityNotify | Same (2203) | Resized-parent Substructure subscribers | W(window_id) | Drop | Same as N16 |
| N18 / 4 | MapRequest | `P/windows.rs:handle_map_window` (2741) | Existing parent redirect targets | W(parent) | Redirect; stays unmapped | `dix/window.c:2621-2631, 2674-2675`; `dix/events.c:2560-2585` |
| N19 / 3 | MapNotify | Same (2774) | Changed-window Structure subscribers | W(window) | Drop | `dix/window.c:2635-2642`; E2 |
| N20 / 3 | MapNotify | Same (2777) | Parent Substructure subscribers | W(parent) | Drop | Same as N19 |
| N21 / 2 | VisibilityNotify | Same (2800) | Changed-window Visibility subscribers | W(window) | Drop; viewability unchanged | `dix/window.c:3084-3089`; E1 |
| N22 / 3 | MapNotify | `P/windows.rs:map_subwindows_with_delta` (2941) | Each mapped-child Structure subscribers | W(child) | Drop; continue children | `dix/window.c:2742-2747`; E2 |
| N23 / 3 | MapNotify | Same (2945) | Parent Substructure subscribers | W(parent) | Drop; continue children | Same as N22 |
| N24 / 3 | UnmapNotify | `P/windows.rs:unmap_window` (3076) | Changed-window Structure subscribers | W(window) | Drop | `dix/window.c:2826-2833, 2855`; E2 |
| N25 / 3 | UnmapNotify | Same (3079) | Parent Substructure subscribers | W(parent) | Drop | Same as N24 |
| N26 / 3 | UnmapNotify | `P/windows.rs:unmap_subwindows_with_delta` (3253) | Each child Structure subscribers | W(child) | Drop; continue children | `dix/window.c:2905`; E2 |
| N27 / 3 | UnmapNotify | Same (3256) | Parent Substructure subscribers | W(parent) | Drop; continue children | Same as N26 |
| N28 / 4 | CirculateRequest | `P/windows.rs:handle_circulate_window` (3447) | Existing first redirect selector other than requester | W(container) | Redirect; stack unchanged | `dix/window.c:2470-2474`; `dix/events.c:2560-2585` |
| N29 / 3 | CirculateNotify | Same (3473) | Child Structure subscribers | W(child) | Drop | `dix/window.c:2477-2478`; E2 |
| N30 / 3 | CirculateNotify | Same (3477) | Container Substructure subscribers | W(container) | Drop | Same as N29 |
| N31 / 5 | UnmapNotify | `P/window_destroy.rs:execute_destroy` (90) | Captured top-window subscribers | W(top) | Drop; teardown continues | `dix/window.c:1064-1086, 2826-2833`; E2 |
| N32 / 5 | UnmapNotify | Same (93) | Captured top-parent subscribers | W(parent) | Drop | Same as N31 |
| N33 / 5 | DestroyNotify | Same (128) | Each captured destroyed-window subscriber list | W(window) | Drop; all cleanup continues | `dix/window.c:1035-1044, 1080-1086`; E2 |
| N34 / 5 | DestroyNotify | Same (131) | Each captured parent subscriber list | W(parent) | Drop | Same as N33 |
| N35 / 5 | CreateNotify | `C/composite_overlay.rs:materialize_overlay` (82) | Root Substructure subscribers | W(ROOT_WINDOW) | Drop; COW/reply/claim retained | `composite/compoverlay.c:147-158`; `dix/window.c:952-953` |
| N36 / 5 | MapNotify | Same (94) | Root Substructure subscribers | W(ROOT_WINDOW) | Drop; COW/reply/claim retained | `composite/compoverlay.c:158`; `dix/window.c:2641-2642` |
| N37 / 6 | FocusIn, FocusOut | `P/focus_pointer.rs:emit_core_focus_transition` (399) | Each transition-window FocusChange subscriber | W(e.window) | Drop; focus/grab/reversion and XI2 unchanged | `dix/events.c:4887-4897`; E1 |
| N38 / 6 | ColormapNotify | `P/colormaps.rs:emit_colormap_notify_for` (264) | Each window-using-cmap's ColormapChange subscribers | W(w) | Drop; installed list/attributes unchanged by denial | `dix/colormap.c:477-523`; E1 |
| N39 / 1,6 | PropertyNotify | `P/props.rs:emit_property_notify` (1201) | PropertyChange subscribers | W(window) | Drop; property operation/reply retained | `dix/property.c:111-130`; E1 |
| N40 / 7 | SelectionClear | `P/selection.rs:apply_set_selection_owner` (184) | Connected previous setter client | W(old_window) | Drop; new ownership retained | `dix/selection.c:183-190` (direct; spec adds Receive) |
| N41 / 7 | SelectionRequest | `P/selection.rs:apply_convert_selection` (449) | Connected selection setter from existing resolver | W(owner_window) | Refuse; no change to selection/properties | `dix/selection.c:285-311` (direct; spec binds denial) |
| N42 / 7 | Synthetic ClientMessage | `R/host.rs:send_client_message_as_server` (69) | Existing zero-mask destination creator | W(window) | Drop; no Send check | `dix/events.c:5638-5640, 2202-2224` (delivery analogue; spec skips server Send) |
| N43 / 7 | Synthetic ConfigureNotify | `R/host.rs:synthetic_configure_notify` (187) | Changed-window Structure subscribers | W(window) | Drop; host move retained | `dix/window.c:2383-2390`; E2 (no host-queue equivalent) |
| N44 / 7 | Synthetic SelectionNotify | `R/selection.rs:send_selection_notify` (431) | Existing zero-mask requestor-window creator | W(requestor) | Drop; success return retained | `dix/events.c:5638-5640, 2202-2224` (delivery analogue) |
| N45 / 7 | Bridge SelectionClear | `R/selection.rs:note_selection_clear` (249, push at 258) | SERVER_OWNER | W(old.window) | Drop queue insertion; ownership change retained | `dix/selection.c:183-190`; spec bridge ruling |
| N46 / 7 | Bridge SelectionRequest | `P/selection.rs:apply_convert_selection` (429 → `R/selection.rs:note_request`, 235) | SERVER_OWNER | W(owner.window) | Refuse; no bridge request queued | `dix/selection.c:298-311`; spec deliberately checks bridge |
| N47 / 6 | Bridge PropertyNotify | `R/selection.rs:note_property_notify` (308, heard at 323, push at 324) | SERVER_OWNER, only if existing bridge/watch predicate matches | W(window) | Drop queue insertion; client events/property change retained | `dix/property.c:111-130`; spec bridge recipient contract |

E1/E2's Xorg references establish ordinary receive/delivery and the delivery-window split; event-specific Xorg
producers in each N row lead to that machinery. They do not imply Xorg checks its direct selection writes or has
yserver's host bridge. No Receive check is added to an unselected/nonexistent target just to match Xorg's broader
pre-mask hook attempts.

| Exempt / absent path | Current location | Disposition / Xorg 21.1.24 reference |
|---|---|---|
| MappingNotify Pointer/Modifier | `P/input_ctl.rs:handle_set_pointer_mapping` (488), `change_modifier_mapping` (629) | Exempt; preserve event/reply order. `dix/inpututils.c:71-103`; `xkb/xkbEvents.c:101-126` |
| MappingNotify Keyboard/Modifier, core forms produced by XI/XKB | `C/xkb_layout.rs:apply_rules_names_change` (136, 139); `C/xkb_select.rs:send_legacy_core_map_notify` (408, 413); `P/xkb.rs:handle_xkb_request` (301, 311); `P/xi/dispatch/devices.rs:handle_xi1_change_keyboard_device` (677, 685); `P/xi/dispatch/control.rs` via `legacy_keyboard_mapping_notify` (472) | Core MappingNotify exempt; do not migrate associated XI/XKB notifications. `xkb/xkbEvents.c:101-126` |
| SelectionNotify(None), client or bridge refusal | `P/selection.rs:apply_convert_selection` (466–486); `R/selection.rs:note_refusal` (264) | Exempt direct result, for absent/gone owner **and denied owner**. `dix/selection.c:305-311` |
| GraphicsExpose, NoExpose | `P/drawing.rs:emit_copy_exposures` (1495, 1514), via `C/process_request.rs:write_to_client` (1545) | Exempt direct CopyArea/CopyPlane result; GC graphics_exposures gate unchanged. `dix/dispatch.c:1718-1759` |
| ResizeRequest | No production emitter/encoder (mask support only) | Separate existing gap; eventual denial preserves resize redirect. `dix/window.c:2311-2330` |
| Later/dormant paths | Inventory §3 input/XI/XKB/extensions/RANDR/RECORD; legacy `server.rs` writers | 4c–4e; retain current entry points, bytes and hook counts |

---

Task file paths starting `core_loop/`, `rootless/` or `server.rs` are relative to
`crates/yserver-core/src/`; `tests/` is relative to `crates/yserver-core/src/core_loop/process_request/`.
Bare test filenames following a fully qualified test path are its siblings.

### Task 1: Shared checked egress, scoped request facts and first production site

**Files:**
- Modify: `crates/yserver-core/src/server.rs` (ServerState field/constructor only),
  `core_loop/process_request.rs` (dispatcher wrapper), `core_loop/fanout.rs` (shared loops/checked entry points),
  `core_loop/process_request/props.rs` (N39 only), `core_loop/process_request/tests/extensions.rs` (recorder facts).
- Create tests: `crates/yserver-core/src/core_loop/process_request/tests/access_notify_support.rs`,
  `access_notify_egress.rs`; register these and subsequent test modules in `tests/mod.rs` as each is added.

**Interfaces:**
- Consumes unchanged: `ServerState::check_receive(&self, recipient: ClientId, subject: EventSubject,
  events: &EventBatch<'_>, request: Option<RequestOpcodes>) -> Decision`;
  `subscribers_by_id(state: &ServerState, window: ResourceId, mask_bits: u32) -> Vec<ClientId>`;
  `EventBatch::new` / `EventDescription::core` above.
- Produces field: `pub(crate) receive_request: Option<RequestOpcodes>` on `ServerState`, default `None`.
- Produces wrapper with unchanged public signature: `pub fn process_request(state: &mut ServerState,
  backend: &mut dyn Backend, client_id: ClientId, sequence: SequenceNumber, header: RequestHeader, body: &[u8],
  attached_fd: Option<OwnedFd>) -> io::Result<RequestOutcome>`; existing body becomes private
  `fn process_request_inner(state: &mut ServerState, backend: &mut dyn Backend, client_id: ClientId,
  sequence: SequenceNumber, header: RequestHeader, body: &[u8], attached_fd: Option<OwnedFd>) -> io::Result<RequestOutcome>`.
- Produces `pub(crate) fn fanout_event_to_clients_with_receive<F>(state: &mut ServerState, client_ids: &[ClientId],
  subject: EventSubject, events: &EventBatch<'_>, encode: F) -> Vec<ClientId>` and
  `pub(crate) fn emit_window_event_to_state_with_receive<F>(state: &mut ServerState, window: ResourceId,
  mask_bits: u32, events: &EventBatch<'_>, encode: F) -> Vec<ClientId>`, both with
  `F: Fn(&mut Vec<u8>, SequenceNumber, ClientByteOrder)`.
- Shared private implementations: `fn fanout_event_to_clients_inner<F>(state: &mut ServerState,
  client_ids: &[ClientId], receive: Option<(EventSubject, &EventBatch<'_>)>, encode: F) -> Vec<ClientId>` with the
  same bound. Old closure entry point delegates with `None`; checked one with `Some`. Raw E3 is added with its
  first callers in Task 7 so no unused API is introduced at this commit. No duplicate transport/encoding loops.
- Test support: `pub(super) struct NotifyFixture { pub state: ServerState, pub backend: RecordingBackend,
  pub peers: HashMap<u32, UnixStream> }`; `NotifyFixture::new() -> Self` installs socket clients 1, 2 and 3;
  `request(&mut self, client: ClientId, sequence: SequenceNumber, header: RequestHeader, body: &[u8]) ->
  io::Result<RequestOutcome>` forwards to production dispatch;
  `drain(&mut self, client: ClientId) -> Vec<u8>` includes socket and `pending_output` via existing
  `read_all_or_buffered`; `install_policy(&mut self, decide: impl Fn(usize, &RecordedExtensionCheck) -> Decision
  + Send + Sync + 'static) -> Arc<Mutex<Vec<RecordedExtensionCheck>>>` uses `RecordingExtensionPolicy::custom`.
  Reuse parent test resource/client helpers; no general new test framework or second policy.
- Recorder uses its existing `RecordedExtensionCheck::owner: Option<ClientId>` for a Receive `Window` subject too:
  snapshot `ctx.owner(window, ResourceKind::Window)` there, leaving resource-check semantics and all other fields
  unchanged. This small test-support extension is required to prove teardown checks see the live subject before
  removal; no new policy class, record fields or changes to existing test expectations.

- [ ] **Step 1 — failing tests:** in `access_notify_egress.rs`, add `receive_property_fanout_is_per_recipient`:
  client 1 changes client 2's window property through dispatch, clients 2/3 selected with different sequences/orders,
  client 1 unselected; record
  Receive recipient, W(window), `[core(28)]`, request `{major:18, minor:0}`, and no Send. Deny one; its socket and
  buffer stay empty, other receives exact PropertyNotify, property bytes/backend effects equal Allow control,
  subscriptions unchanged; recorder owner equals the subject window's creator. Repeat with policy decision flipped
  between requests to prove fresh delivery decisions
  (`receive_notification_revocation_is_immediate`). Mutations respectively: bypass Receive/deny whole fanout or
  record requestor ownership instead of subject ownership; cache first Allow.
  `receive_request_scope_restores_after_dispatch`: successful ChangeProperty records opcode 18;
  invalid-length request still yields the existing BadLength; `receive_request` is `None` after both returns.
  Mutations: set no active opcode; skip restoration for invalid-length requests. Task 2 adds the independent
  server-delivery proof.
- [ ] **Step 2 — red gate:** `cargo test -p yserver-core access_notify_egress::receive_property`; also run the
  revocation test by name. Expect assertions to fail because Receive is never called. First save literal wire
  expectations for the no-policy ChangeProperty case; do not rewrite existing tests.
- [ ] **Step 3 — implement:** E1/E2 and N39. Deduplicate and skip absent clients before asking; deny before encoder,
  copy/swap, telemetry or mutable client borrow. Read `receive_request` into a local before mutably borrowing state.
  An empty policy list reaches the existing helper shortcut without building an AccessContext. Preserve the old
  wrappers for later sites and 4a. Add the scoped dispatcher wrapper specified above; restore before returning its
  result, not only on the successful arm. Do not change `write_to_client`, `check_receive`, or `AccessPolicy`.
- [ ] **Step 4 — green/mutation gate:** `cargo +nightly fmt -- --check`;
  `cargo test -p yserver-core access_notify_egress`; `cargo test -p yserver-core access_send`;
  `cargo test -p yserver-core core_loop::fanout::tests`; `cargo clippy --all-targets -- -D warnings`.
  Run the named mutations, restore them and rerun these gates; all must pass.
- [ ] **Step 5 — commit:** `feat(access): check core notification fanout with scoped request metadata`.

### Task 2: Expose and VisibilityNotify, including server-generated descendants

**Files:** Modify `crates/yserver-core/src/core_loop/fanout.rs`, `core_loop/process_request/windows.rs` (N01–N04,
N21). Create `core_loop/process_request/tests/access_notify_windows.rs`; extend `access_notify_egress.rs` context test.

**Interfaces:**
- Consumes Task 1's checked window emitter; unchanged `pub fn expose_event_fanout_to_state(state: &mut ServerState,
  xid_map: &HostXidMap, ev: HostExposeEvent) -> Vec<ClientId>` and
  `pub fn emit_visibility_unobscured_subtree_to_state(state: &mut ServerState, root: ResourceId) -> Vec<ClientId>`.
- Produces no new signature: preserve `pub(crate) fn send_window_exposures(state: &mut ServerState,
  backend: &mut dyn Backend, origin: Option<OriginContext>, window: ResourceId,
  region: &[x11::xfixes::RegionRect], paint: bool)`; migrate its existing deliveries with `[core(12)]`.

- [ ] **Step 1 — failing tests:** `receive_host_expose_checks_each_delivery_window`: invoke the real host exposure
  entry point with a mapped parent/descendant, two subscribers each; assert W(parent)/W(child), `[core(12)]`, request
  None, one denial leaves the other delivery/rectangles/counts and state unchanged. Unknown host XID and no selectors
  ask nobody. Mutations: use ancestor subject for descendants; return on first denial; ask absent selectors.
  `receive_clear_area_expose_denial_keeps_paint`: ClearArea(exposures=true) through dispatch, assert zero denied bytes
  and original painting/backend calls and permitted Expose counts; exposures=false asks/delivers nothing. Mutation:
  skip painting/operation on Receive denial. `receive_map_visibility_denial_keeps_descendant_walk`: MapWindow through
  dispatch with previously mapped Unviewable descendants; deny root of visibility subtree, permit its child and a
  second subscriber; assert N21/N03 subjects, `[core(15)]`, visibility state 0, request `{8,0}`, all map states equal
  Allow control. Mutation: prune recursion after denial. Include disjoint exposure regions and >RECTLIMIT collapse
  from a real configure/unmap operation in `receive_expose_rectangles_keep_counts_and_collapse`; assert every Expose
  description matches its actual call and counts/allowed byte stream unchanged. Mutation: describe Visibility as
  Expose, or drop remaining rectangles after the first denied one.
  Add `receive_request_context_does_not_leak_to_host_expose` in `access_notify_egress.rs`: successful ChangeProperty,
  then invalid-length request (unchanged BadLength), then real host Expose on a selected mapped window. The first
  receive has `{18,0}`, the host receive has `None`, and the scope is `None` after each request. Mutation: leave the
  most recent request active for later host events.
- [ ] **Step 2 — red gate:** `cargo test -p yserver-core access_notify_windows::receive_` and
  `cargo test -p yserver-core receive_request_context_does_not_leak_to_host_expose`; expect absent Expose/Visibility
  Receive calls.
- [ ] **Step 3 — implement:** N01–N04/N21 use checked window fanout, one description per currently encoded event.
  Preserve paint-before-notify, exposure collapse/count, host-XID resolution, visibility mask and recursion order.
- [ ] **Step 4 — green/mutation gate:** `cargo test -p yserver-core access_notify_`;
  `cargo test -p yserver-core expose_probe`; `cargo test -p yserver-core core_loop::fanout::tests`;
  `cargo +nightly fmt -- --check`; `cargo clippy --all-targets -- -D warnings`. All pass after restoring mutations.
- [ ] **Step 5 — commit:** `feat(access): receive checks for core exposure and visibility notifications`.

### Task 3: Window and structure notifications use each delivery window

**Files:** Modify `crates/yserver-core/src/core_loop/process_request/windows.rs` (N05–N12, N14–N17, N19–N20,
N22–N27, N29–N30). Extend `tests/access_notify_windows.rs`.

**Interfaces:**
- Consumes Task 1's checked snapshot and window emitters, using existing subscriber snapshots and EventBatch API.
- Produces no new public/private signature; handler/apply APIs, recipient resolvers and resource transitions stay
  unchanged. At snapshot sites supply W(window/old_parent/new_parent) explicitly; window-emitter sites derive it.

- [ ] **Step 1 — failing tests:** add table-driven `receive_structure_subject_is_delivery_window`: drive CreateWindow,
  MapWindow, UnmapWindow, ConfigureWindow, ReparentWindow, MapSubwindows, UnmapSubwindows and nonredirected
  CirculateWindow through dispatch. Cover each ID owned by this task, named by ID in the case table. A single client
  selecting both Structure on the child and Substructure on the parent is denied only at one subject; it must still
  get the other event. A second allowed recipient keeps the full original stream. Assert exact type, affected child,
  event/delivery window, request major/minor, sequence/order, no Send; resource geometry/map/stack/parent and backend
  calls equal Allow control. Mutations: W(affected child) for all deliveries; suppress all events for a denied client
  across windows; use wrong type code at each migrated emitter. Add `receive_reparent_preserves_three_targets_and_order`:
  mapped cross-parent reparent observes Unmap, Reparent (window/old/new), remap in existing order; deny new-parent
  delivery only. Same-parent case preserves today's dedup and remap behavior. Mutation: drop old-parent delivery or
  duplicate new-parent delivery for the same parent. `receive_gravity_denial_keeps_child_move`: resize South-gravity
  child fixture through ConfigureWindow; denied child stream empty for GravityNotify, parent allowed gets type 24,
  child coordinates/backend movement equal control. Mutation: skip gravity application on receive denial.
  `receive_structure_noop_does_not_ask`: same-value configure, already-mapped map and already-unmapped unmap have
  no fabricated Receive call/event. Mutation: move the receive gate ahead of the existing no-change exit.
- [ ] **Step 2 — red gate:** `cargo test -p yserver-core access_notify_windows`; new structure cases must fail on
  missing hooks/subjects before migration. Freeze existing no-policy per-recipient wire expectations for these cases.
- [ ] **Step 3 — implement:** migrate exactly the IDs above to checked fanout with the code/subject from the table.
  Keep Present ConfigureNotify before core ConfigureNotify; do not check or remove Present output in 4b. Do not
  reorder snapshots, mutate mask selections, or share a denial across different delivery windows.
- [ ] **Step 4 — green/mutation gate:** `cargo test -p yserver-core access_notify_windows`;
  `cargo test -p yserver-core core_loop::process_request::tests::window`;
  `cargo test -p yserver-core core_loop::process_request::tests::composite`;
  `cargo +nightly fmt -- --check`; `cargo clippy --all-targets -- -D warnings`. Restore mutations; all pass.
- [ ] **Step 5 — commit:** `feat(access): receive checks for core window structure notifications`.

### Task 4: Redirect denial preserves the redirected operation

**Files:** Modify `crates/yserver-core/src/core_loop/process_request/windows.rs` (N13, N18, N28).
Extend `tests/access_notify_windows.rs`.

**Interfaces:**
- Consumes `fanout_event_to_clients_with_receive` with W(parent/container) and codes 23/20/27.
- Produces no new signature or delivery-count API. The existing nonempty redirect-selection branch continues to own
  the successful early return/else choice; its choice is not derived from E1's transport-disconnect vector.

- [ ] **Step 1 — failing tests:** `receive_redirect_denial_preserves_redirection`, rows MapWindow/ConfigureWindow/
  CirculateWindow: legal redirect selector is a different client. Deny Receive to it; record W(parent/container),
  exact code and request opcode; no denied event, no request error, no MapNotify/ConfigureNotify/CirculateNotify to
  ordinary listeners, geometry/map/stack and backend effects exactly as the redirected Allow control and pre-request
  state. Mutation per row: continue applying the operation if the checked recipient receives no bytes.
  `receive_redirect_does_not_check_unselected_clients`: requester selects redirect itself, override-redirect window,
  and no redirect selector cases preserve today's operation and ordinary notifications; no redirect Receive occurs.
  Mutation: manufacture a redirect check from the mask bit without the existing requester/override filters.
  `receive_redirect_revocation_still_redirects`: repeat an unchanged redirect request before/after revoking the
  selector's Receive grant; first notification arrives, second does not, both operations stay redirected and no
  ordinary listener acquires an event. Mutation: cache the first Allow or fall through to operation on denial.
- [ ] **Step 2 — red gate:** `cargo test -p yserver-core access_notify_windows::receive_redirect`; expect missing
  Receive calls before migration. Do not simulate impossible multiple legal redirect selectors to test fanout.
- [ ] **Step 3 — implement:** gate only notification output inside existing redirect branches. Keep operation stays
  redirected even on deny-all, as Xorg's “pretend delivered” path does. Document the absent ResizeRequest emitter;
  add neither a resize redirect implementation nor a speculative unused check.
- [ ] **Step 4 — green/mutation gate:** `cargo test -p yserver-core access_notify_windows`;
  `cargo test -p yserver-core core_loop::process_request::tests::window`;
  `cargo +nightly fmt -- --check`; `cargo clippy --all-targets -- -D warnings`. Restored mutations; all pass.
- [ ] **Step 5 — commit:** `feat(access): preserve core redirection when receive is denied`.

### Task 5: Destruction, disconnect and core overlay notifications

**Files:** Modify `crates/yserver-core/src/core_loop/process_request/window_destroy.rs` and
`core_loop/composite_overlay.rs` (N31–N36). Create `tests/access_notify_lifecycle.rs`.

**Interfaces:**
- Consumes Task 1 checked fanout and scoped opcodes; `subscribers_by_id` and captured lists remain unchanged.
- Produces unchanged entry points: `pub(crate) fn execute_destroy(state: &mut ServerState, backend: &mut dyn Backend,
  origin: Option<OriginContext>, prepared: PreparedDestroy) -> DestroyOutcome`;
  `pub(crate) fn materialize_overlay(state: &mut ServerState, backend: &mut dyn Backend,
  origin: Option<OriginContext>) -> io::Result<()>`. No new teardown or disconnect API.

- [ ] **Step 1 — failing tests:** `receive_destroy_denial_keeps_cleanup`: DestroyWindow and DestroySubwindows through
  dispatch with mapped nested tree; deny one window/parent subject for one recipient, allow another. Cover N31–N34;
  record `[core(18)]`/`[core(17)]` at their delivery windows and assert the recorder's owner still resolves before
  resource removal (for child and parent separately), existing Unmap/Destroy order,
  request `{4,0}`/`{5,0}`. Assert remaining recipients' whole stream, destroyed resources gone, subscriptions/selection
  cleanup, focus/grab release and backend calls equal Allow control. Mutations: return from destroy loop on denial;
  perform check after deleting subscriptions/resources; use top subject for every descendant.
  `receive_disconnect_destroy_notifies_survivors`: call real `process_disconnect(state, backend, departing_client)`;
  departing owner is gone, selected survivors are still checked with request None, denied recipient gets no core
  notification, allowed one keeps Unmap/Destroy, full cleanup equals Allow control. Mutation: skip receive for server
  teardown. `receive_cow_notifications_keep_extension_request`: COMPOSITE GetOverlayWindow through dispatch (actual
  registered major 144, minor 7); core CreateNotify/MapNotify are checked as W(ROOT_WINDOW), with `[core(16)]` then
  `[core(19)]`, request `{144,7}`. Deny one subscriber; allowed one gets both, requestor's reply and COW claim/resource
  are unchanged. ReleaseOverlayWindow (minor 8) and last-claim disconnect exercise COW teardown through N31–N34;
  assert Some({144,8}) versus None respectively. Mutations: lose the extension minor; gate reply/claim with Receive;
  exempt core events solely because the producer is COMPOSITE.
- [ ] **Step 2 — red gate:** `cargo test -p yserver-core access_notify_lifecycle`; fail on missing hooks or wrong
  arguments; use existing composite recording-backend COW setup, not KMS/hardware.
- [ ] **Step 3 — implement:** check captured lists while windows still exist; do not move delivery past free/apply
  stages. Migrate COW's core Create/Map only; keep all extension notification paths and teardown ordering intact.
- [ ] **Step 4 — green/mutation gate:** `cargo test -p yserver-core access_notify_lifecycle`;
  `cargo test -p yserver-core core_loop::process_request::tests::composite`;
  `cargo test -p yserver-core process_disconnect`; `cargo +nightly fmt -- --check`;
  `cargo clippy --all-targets -- -D warnings`. Restore mutations and pass all.
- [ ] **Step 5 — commit:** `feat(access): receive checks for destroy and overlay core notifications`.

### Task 6: Core focus, colormaps and property/bridge isolation

**Files:** Modify `crates/yserver-core/src/core_loop/process_request/focus_pointer.rs`, `colormaps.rs`,
`props.rs` only if N39 needs adjustment, and `crates/yserver-core/src/rootless/selection.rs` (N47).
Create `core_loop/process_request/tests/access_notify_objects.rs` (N37–N39, N47).

**Interfaces:**
- Consumes checked window emitter and `ServerState::check_receive`; manual bridge gate reads `receive_request`.
- Preserves `pub(crate) fn emit_core_focus_transition(state: &mut ServerState, from_raw: u32, to_raw: u32, mode: u8)`;
  private `fn emit_colormap_notify_for(state: &mut ServerState, cmap: ResourceId, installed: bool)`;
  private `fn emit_property_notify(state: &mut ServerState, window: ResourceId, property: AtomId, deleted: bool)`;
  `pub(crate) fn note_property_notify(state: &mut ServerState, window: ResourceId, property: AtomId,
  time: u32, deleted: bool)`. No new signatures.

- [ ] **Step 1 — failing tests:** `receive_focus_checks_transition_windows_and_keeps_xi2`: SetInputFocus through
  dispatch across ancestor/nonlinear windows, then core GrabKeyboard/UngrabKeyboard and destruction reversion;
  assert codes 9/10 at every selected transition window, mode/detail/order unchanged. Deny one transition-window
  event to a dual core/XI2 subscriber; its XI2 events and another client's core events remain unchanged, focus/grab
  state equals Allow control. Also drive `rootless::host_request` with `HostRequest::Activated` on a live top-level
  for request None (the private seat helper is not exposed for testing). Mutations:
  use final focus for every subject; suppress XI2 with core denial; skip checks for NotifyGrab/Ungrab/server mode.
  `receive_colormap_uses_each_subscribing_window`: InstallColormap/UninstallColormap with two windows sharing a map,
  one client denied only on one window; `[core(32)]`, W(window), correct request; installed list, fallback reinstall
  and other deliveries equal control. Mutation: W(colormap) or skip installed-list update on denial.
  `receive_get_property_delete_keeps_reply`: ChangeProperty, DeleteProperty, RotateProperties and deleting GetProperty
  through dispatch; record code 28, W(window), appropriate request opcode, including originator subscribed to its own
  property. Deny its event; full GetProperty reply remains, deletion/rotation still applied, another subscriber keeps
  exact events. Mutation: drop mixed request output/reply or skip deletion on denial.
  `receive_bridge_property_denial_keeps_clients_and_property`: enable rootless, watch a client window (and test the
  bridge-owned window); ChangeProperty through dispatch. SERVER_OWNER denial records W(window), `[core(28)]` before
  queue mutation; no Property queue event, real client subscriber still receives its event, data/watch list unchanged
  by denial. Unwatched client window produces no bridge receive. Mutation: check after push; deny entire emitter;
  ask bridge on an unwatched window. Server `change_property_as_server` variant asserts request None and no Send.
- [ ] **Step 2 — red gate:** `cargo test -p yserver-core access_notify_objects`; fail for missing core/bridge checks
  or wrong reply isolation; reuse production property requests and rootless setup, not a direct checked-writer test.
- [ ] **Step 3 — implement:** migrate N37/N38 and add N47 gate after the existing heard predicate, before `push`,
  without keeping a rootless/client mutable borrow across the check. N39 already uses checked fanout from Task 1.
  Keep XI/XKB counterparts, timestamps, focus transition generation and all state updates unchanged.
- [ ] **Step 4 — green/mutation gate:** `cargo test -p yserver-core access_notify_objects`;
  `cargo test -p yserver-core core_loop::process_request::tests::props`;
  `cargo test -p yserver-core core_loop::process_request::tests::core_misc`;
  `cargo test -p yserver-core rootless`; `cargo +nightly fmt -- --check`;
  `cargo clippy --all-targets -- -D warnings`. Restore mutations and pass all.
- [ ] **Step 5 — commit:** `feat(access): receive checks for core focus property and colormap events`.

### Task 7: Selection recipients, denial answer and rootless outgoing core events

**Files:** Modify `crates/yserver-core/src/core_loop/process_request/selection.rs`,
`crates/yserver-core/src/rootless/selection.rs`, `rootless/host.rs`, `core_loop/fanout.rs` (E3, N40–N46). Create
`core_loop/process_request/tests/access_notify_selection.rs`.

**Interfaces:**
- Consumes unchanged `selection_owner_target_id(state: &ServerState, selection: AtomId) ->
  Option<(ResourceId, ClientId)>`, `client_target_id(state: &ServerState, client_id: ClientId) -> Option<ClientId>`,
  `is_selection_bridge_window(state: &ServerState, window: ResourceId) -> bool`, and Task 1's checked snapshot
  emitters and manual Receive helper. Uses the existing lazy `send_event_recipients` for rootless zero-mask routing.
- Produces `pub(crate) fn fanout_raw_event_to_clients_with_receive(state: &mut ServerState,
  client_ids: &[ClientId], subject: EventSubject, events: &EventBatch<'_>, event: &[u8; 32],
  template_byte_order: ClientByteOrder) -> Vec<ClientId>` and its shared private loop
  `fn fanout_raw_event_to_clients_inner(state: &mut ServerState, client_ids: &[ClientId], event: &[u8; 32],
  template_byte_order: ClientByteOrder, receive: Option<(EventSubject, &EventBatch<'_>)>) -> Vec<ClientId>`.
  The unchanged raw entry point delegates with `None`, the checked one with `Some`, like Task 1's closure path.
- Preserves `pub(crate) fn apply_set_selection_owner(state: &mut ServerState, client_id: ClientId,
  sequence: SequenceNumber, window: ResourceId, selection: AtomId, time_val: u32)`;
  `pub(crate) fn apply_convert_selection(state: &mut ServerState, client_id: ClientId, sequence: SequenceNumber,
  requestor: ResourceId, atoms: [AtomId; 3], time_val: u32)` (existing destructuring unchanged);
  `pub(crate) fn note_selection_clear(state: &mut ServerState, old: &SelectionOwner, selection: AtomId, time: u32)
  -> bool`; `pub(crate) fn note_request(state: &mut ServerState, owner: &SelectionOwner, event: SelectionEvent) -> bool`;
  `pub fn send_selection_notify(state: &mut ServerState, time: u32, requestor: ResourceId, atoms: [AtomId; 3]) ->
  Result<(), SelectionError>`; `pub(crate) fn send_client_message_as_server(state: &mut ServerState,
  window: ResourceId, r#type: &str, data: [u32; 5])`; private `fn synthetic_configure_notify(state: &mut ServerState,
  window: ResourceId)`. No new recipient/outcome type.

- [ ] **Step 1 — failing tests:** `receive_selection_clear_targets_previous_setter`: client A sets ownership to B's
  window, C replaces/releases; deny A's clear at the old window; record code 29, request `{22,0}`, no bytes to A/B,
  new ownership/time retained, XFIXES selection notifications untouched. Same-setter nonrelease has no fabricated
  clear. Mutations: ask window creator; undo ownership; gate XFIXES with core denial.
  `receive_selection_request_denied_answers_actual_requester`: A is setter of B's owner window, C issues
  ConvertSelection with B's requestor window. Allow row sends code 30 only to A, W(owner), request `{24,0}`. Deny A:
  no owner event/bridge request, exactly one code-31 failure to C (property 0, request time/atoms/requestor, C's
  sequence and byte order), none to B. Policy also denies all code 31; the direct failure still arrives and no
  Receive for it is recorded. Selection and properties unchanged; no error/backend mutation. Repeat absent/gone owner
  for identical failure bytes. Mutations: silently drop denied request; answer window creator; check failure Receive;
  clear owner on denial. LE/BE recipients and nonzero explicit time prevent accidental identity/byte-order passes.
  `receive_bridge_selection_denial_answers_requester`: server bridge owns selection; deny SERVER_OWNER's code-30
  W(owner) before `note_request`; no queued Request, client requestor gets the same exempt failure. Allow row queues
  one Request. `receive_denied_owner_with_bridge_requestor_answers_issuing_client` (sol r1 B-1): rootless bridge
  window W exists, ordinary client A owns S, socket client C issues ConvertSelection naming W as requestor; deny A's
  code 30 and also every code 31: C receives exactly one exempt code-31 failure (C's sequence and byte order), the
  bridge queue is unchanged (no `SelectionEvent::Notify`). Mutation: route the denied-owner outcome through
  `note_refusal` by requestor window. `receive_denied_owner_for_bridge_conversion_queues_refusal` (sol r2 B-1): the
  bridge's typed `convert_selection` with ordinary owner A denied code 30: exactly one queued
  `SelectionEvent::Notify(property=None)`, no owner event, no Receive recorded for the refusal. Mutation: answer every
  denied owner through the socket writer. `receive_bridge_clear_denial_does_not_queue`: deny SERVER_OWNER code 29 before
  insertion; no Clear queued, new ownership/XFIXES behavior retained. Mutations: exempt server recipient; queue before
  check; treat “not queued” as conversion success; let bridge clear denial fall through to another recipient.
  `receive_rootless_outgoing_core_events_are_checked`: use real `rootless::host_request` move/close paths and
  `rootless::selection::send_selection_notify` API to generate synthetic ConfigureNotify, WM_PROTOCOLS ClientMessage,
  and SelectionNotify (both nonzero/None property). Assert codes 22/33/31 without send bit, W(target/requestor), None
  request, no Send; denied recipient gets no event, permitted subscribers keep bytes, host geometry/WM state equals
  Allow control. Synthetic bit remains set in allowed bytes. Mutation: bypass raw checks; describe sent type with
  bit 7; exempt host SelectionNotify(None) by type/property alone.
- [ ] **Step 2 — red gate:** `cargo test -p yserver-core access_notify_selection`; fail on missing checks/refusal
  before implementation. Existing `access_send` interception tests remain unchanged.
- [ ] **Step 3 — implement:** N40 uses checked snapshot fanout. N45 gates inside `note_selection_clear` after bridge
  recognition, before push; a recognized denied clear still returns `true` (handled), preventing fallback.
  In `apply_convert_selection`, resolve an existing bridge owner (`owner.client == Some(SERVER_OWNER)` and existing
  bridge-window predicate) or ordinary owner with the existing resolver; ask Receive **once** for that recipient
  and `[core(30)]`. Allow uses the existing `note_request` or unchecked writer (already authorized); deny answers
  by the ISSUING identity (sol r1 B-1, r2 B-1): when `client_id == SERVER_OWNER` (the bridge's typed
  `rootless::selection::convert_selection`), the existing `note_refusal` queues its exempt
  `SelectionEvent::Notify(property=None)`; for a socket issuer, the exempt SelectionNotify(None) socket writer used
  for an absent owner answers `client_id`, even when the requestor window is the bridge's (never `note_refusal`
  there, which recognises the bridge by requestor window only). Never re-queries the owner as another recipient and never calls a checked writer a second
  time. Keep absent/gone-owner and bridge-requestor refusal semantics; do not introduce a selection policy hook.
  N42/N44 use checked raw fanout; N43 checked window fanout, preserving template/source byte order, synthetic bit,
  target resolution and success returns. No Send check and no change to 4a's explicit-send writers/interceptions.
- [ ] **Step 4 — green/mutation gate:** `cargo test -p yserver-core access_notify_selection`;
  `cargo test -p yserver-core core_loop::process_request::tests::selection`;
  `cargo test -p yserver-core access_send`; `cargo test -p yserver-core rootless`;
  `cargo +nightly fmt -- --check`; `cargo clippy --all-targets -- -D warnings`. Restore mutations and pass all.
- [ ] **Step 5 — commit:** `feat(access): receive checks and denial answers for core selection notifications`.

### Task 8: Exemptions, no-policy parity, status and final gate

**Files:** Create `crates/yserver-core/src/core_loop/process_request/tests/access_notify_exemptions.rs`;
extend `access_notify_egress.rs`/existing new fixtures where needed; modify `docs/status.md`.
No further production changes are planned unless a named test finds an omission in Tasks 1–7.

**Interfaces:**
- Consumes the checked interfaces from Tasks 1/7 and unchanged direct-result/MappingNotify paths. In particular,
  `pub(super) fn write_to_client(client: &mut ClientState, client_id: ClientId, bytes: &[u8]) -> RequestOutcome`.
- Produces no API. Tests use the shared NotifyFixture; status records completed core coverage, exclusions and the
  existing ResizeRequest gap without claiming selection mediation or completion of PR 4 as a whole.

- [ ] **Step 1 — tests:** `receive_exempt_results_and_mapping_bypass_policy`: policy denies every Receive, allows
  resource/property/dispatch checks. Through dispatch exercise CopyArea/CopyPlane missing and fully available
  regions (GraphicsExpose 13/NoExpose 14, decreasing counts, destination drawable, originating opcode, GC
  graphics_exposures true/false); SetPointerMapping, SetModifierMapping fallback, and ChangeKeyboardMapping with
  existing recording keymap fixture. Also exercise the core MappingNotify forms produced by XI keyboard-change
  requests, without requiring Receive on adjacent XI/XKB events in 4b. All direct events/replies and mappings equal
  no-policy control; **zero Receive calls for types 13/14/34**, and no code-31 Receive for absent/denied-owner refusal
  (Task 7 fixture). Mutations: add a Receive gate at `emit_copy_exposures` before its mutable client borrow;
  check MappingNotify at its fanout caller; classify failure SelectionNotify
  as a notification. These exemption rows pass before implementation and must fail under those mutations; do not
  pretend they initially fail due to absent feature code.
  `receive_no_policy_core_bytes_match_baseline`: execute the N01–N47 case fixtures with no policy and with recording
  Allow policy, checking the independently fixed wire expectations saved at the red steps, in LE/BE, distinct client
  sequences and both socket/buffered output. Include no selectors/absent owner, no-op requests, multi-event reparent,
  Present-before-core configure, mixed GetProperty reply/event and direct results. A deny-all case also proves later
  XI/XKB/Present/XFIXES output through unchanged writers still arrives with its original order/hook count. Mutations:
  force synthetic bit/sequence to a fixed value; gate all old writers; drop an unrelated extension event.
  Time-bearing events assert actual recorded/state timestamps or a before/after server-time bound; compare only
  those dynamic fields separately, not arbitrary bytes. No narrow wall-clock deadline; no new clock abstraction.
- [ ] **Step 2 — coverage audit:** for each N ID, identify its ASK, DENY/isolation and no-policy case by name/row.
  Search all core encoders/fanout/raw/direct writes again; ensure no unchecked core notification remains except the
  listed exemptions/absent path. In particular verify both duplicate parent/child deliveries, rootless queue gates,
  originator-as-recipient and server/disconnect causes. Run the named mutations and restore all. Existing tests
  remain unmodified; new files all <6000 lines.
- [ ] **Step 3 — status:** record the 4b coverage, explicit direct-result/MappingNotify exemptions, absent
  ResizeRequest finding and remaining 4c–4e work in `docs/status.md`. The binding spec already decides these cases;
  do not rewrite it or broaden this plan to extensions, RECORD or legacy writers.
- [ ] **Step 4 — final gate (implementation time only):** `cargo +nightly fmt -- --check`;
  `cargo clippy --all-targets -- -D warnings`; `cargo clippy --all-targets --features tcp-transport -- -D warnings`;
  `cargo clippy --all-targets --features xdmcp -- -D warnings`; in a subshell source the existing
  `/home/ariel_santangelo/Projects/claude-scratch/freebsd-sysroot/env.sh`, then run
  `cargo clippy --target x86_64-unknown-freebsd --all-targets -- -D warnings` and
  `cargo clippy --target x86_64-unknown-freebsd --all-targets --features xdmcp -- -D warnings`;
  `cargo check -p yserver-core --target x86_64-pc-windows-gnu`; `cargo test --workspace` and
  `cargo test --workspace --features xdmcp`; CI's `cargo test --all-targets --locked` in default, tcp-transport and
  xdmcp configurations; `cargo test --workspace --tests --locked` (every existing `crates/*/tests` integration
  target); the established lavapipe `cargo test --workspace --locked --features xdmcp --no-fail-fast -- --ignored`
  gate (CI software-Vulkan environment, not physical hardware);
  `sh tools/check-layout.sh`; `just man`. Cross-platform/lavapipe prerequisites that are unavailable must be reported
  as unrun, never counted as passing. No ioctl/platform code is changed by 4b.
  Run `cargo test -p yserver-core access_` five times while `nproc` background CPU workers run; use a shell trap to
  kill only those recorded worker PIDs on every exit, including failed tests. Assert each run succeeds, and record
  results. Run the remaining new tests under the same load if their names lack the `access_` module prefix.
- [ ] **Step 5 — commit:** `test(access): pin core receive exemptions and no-policy parity`.

## Estimated implementation size and self-review

Changed-line estimates count additions plus edits (not whole files, moved unchanged dispatcher body or this plan).
Paths below are relative to `crates/yserver-core/src/` except status. They are review-sizing estimates, not budgets.

| Production file | Estimated changed lines |
|---|---:|
| `server.rs` | 15 |
| `core_loop/process_request.rs` | 35 |
| `core_loop/fanout.rs` | 180 |
| `core_loop/process_request/windows.rs` | 210 |
| `core_loop/process_request/window_destroy.rs` | 45 |
| `core_loop/composite_overlay.rs` | 25 |
| `core_loop/process_request/focus_pointer.rs` | 15 |
| `core_loop/process_request/props.rs` | 15 |
| `core_loop/process_request/colormaps.rs` | 15 |
| `core_loop/process_request/selection.rs` | 100 |
| `rootless/host.rs` | 30 |
| `rootless/selection.rs` | 50 |
| **Production total** | **735** |

| Test file (`core_loop/process_request/tests/`) | Estimated changed lines |
|---|---:|
| `access_notify_support.rs` | 180 |
| `access_notify_egress.rs` | 350 |
| `access_notify_windows.rs` | 750 |
| `access_notify_lifecycle.rs` | 450 |
| `access_notify_objects.rs` | 400 |
| `access_notify_selection.rs` | 600 |
| `access_notify_exemptions.rs` | 300 |
| `mod.rs` | 8 |
| `extensions.rs` | 15 |
| **Test total** | **3053** |

`docs/status.md`: about 12 lines. **Total implementation estimate: 3800 lines (735 production, 3053 tests, 12 docs).**

Self-review completed against the binding sections: N01–N47 cover every existing in-scope notification expression
and bridge interception; E1–E3 share encoding/transport loops with untouched later callers. Redirects preserve the
operation boundary; selection failures, MappingNotify and graphics results have explicit exempt tests; server and
internal recipients, actual request metadata, pre-encoding checks and revocation have assigned tasks. The only
absent producer is ResizeRequest, recorded rather than added under a receive change. Interface names/types are
consistent across tasks; the checked helpers consume existing EventBatch/Subject types, and no task requires an
undefined resolver, policy or outcome. All five Review Focus classes have named tests. Eight tasks, no function
bodies, no new dependencies, selection mediation or input/extension authorization. No remaining question requires a
user ruling: the spec decides differences from Xorg; Xorg determines the remaining core delivery semantics.

Plan authoring was read-only apart from this file: no implementation, git writes, build, test or hardware action.
