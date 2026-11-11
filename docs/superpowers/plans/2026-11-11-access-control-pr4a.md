# Access control layer, PR 4a — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The send and receive hooks exist, with `EventSubject` and an event batch a policy can read, and the two
explicit client sends (core `SendEvent`, XI `SendExtensionEvent`) ask the send hook at every candidate window and the
receive hook for every recipient, with no change any client can observe while no policy is installed.

**Architecture:** Two new `AccessPolicy` methods with default `Allow`, two `ServerState` helpers that follow
`check_resource`'s shortcuts and first-denial order, and check sites in the two send handlers and their recipient
resolvers. The resolvers keep the candidate window instead of losing it. Receive checks on every other delivery path are
plans 4b–4e (core notifications; input and keyboard; extensions; RANDR, RECORD and closure).

**Tech Stack:** Rust, crate `yserver-core`; std only.

**Spec:** `docs/superpowers/specs/2026-10-30-access-control-layer-design.md`, series row 4 ("What a policy can see",
"A denial is interpreted by its site", "Who is checked", "Where the checks go" → Receive / Send / XTEST).

**Reference:** Xorg (MIT) at `/data/Projects/xorg-drivers/xserver`; paths below are relative to it. Where the spec
decides, the spec binds; where it is silent, Xorg's behaviour is followed (user ruling 2026-11-11).

**Source inventory:** `~/Projects/claude-scratch/access-control/pr4/inventory.md` (taken at `d14eaecb`), §1, §2, §4,
§5, §7 and "Contradictions".

## Global Constraints

- With no policy installed, every byte a client observes stays the same, including error precedence, replies, events
  and their order. The existing suite passes unmodified.
- No new validity errors. The existing differences from Xorg in `SendEvent` / `SendExtensionEvent` validation
  (inventory "Contradictions" 10: XI accepts any nonzero propagate, lacks the event-range check and the zero-event
  early return; the core propagate check runs in the dispatcher) are not changed; they are reported as findings.
- The new helpers follow `check_resource`: an empty policy list answers `Allow` without building a context; a request
  run as `SERVER_OWNER` is not checked; policies are asked in order and the first denial wins.
- Reuse first: the existing resolvers, fanout writers and macros; no second resolver; no new `AccessMode` bit for
  send/receive (the hooks are their own methods). No environment variable, no `cfg(test)` in production bodies.
- Licensing: Xorg is MIT and may be read; no code copied.
- Tests exercise production paths through `process_request`, with the shared recording/decision policy of
  `process_request/tests/extensions.rs`. Every DENY row asserts the policy's effect and that nothing else changed (no
  bytes to any client, state unchanged). Every new test names the mutation that makes it fail, and every mutation is
  run and compiles. Suites run under CPU load. Test files stay under 6000 lines.

## Scope decisions

- **The hooks** (`access.rs`), both defaulting to `Decision::Allow`:
  - `send(&self, ctx, sender: ClientId, window: ResourceId, device: Option<u16>, events: &EventBatch<'_>) -> Decision`
    — the spec's "sender, target window at this step of propagation, optional device, the event batch".
  - `receive(&self, ctx, recipient: ClientId, subject: EventSubject, events: &EventBatch<'_>) -> Decision` — the spec's
    "recipient, delivery window if there is one, the event batch"; `subject` carries the window (or drawable, device,
    or `Global`).
  - `pub enum EventSubject { Window(ResourceId), Drawable(ResourceId), Device(u16), Global }`, exactly the spec's.
- **The event batch is a description, not encoded bytes,** so a receive check never needs an encoding for a client
  that will be denied (spec "Receive"): `EventBatch<'a>` gives, per event, its core type code without the send-event
  bit and, for an extension event, the extension's major opcode and its event or XI2 `evtype`. That is what both
  reference consumers read (Xorg `Xext/security.c:869-899` checks each event's type). A `SendEvent` batch is built from
  the request's events; the same type serves 4b–4e.
- **ServerState helpers** `check_send(sender, window, device, &EventBatch, Option<RequestOpcodes>)` and
  `check_receive(recipient, EventSubject, &EventBatch, Option<RequestOpcodes>)`, next to `check_resource`. A receive
  check during a request carries the request's opcodes (it runs while handling it); one from server work carries `None`.
  A recipient that is `SERVER_OWNER` or an internal client is asked like any other client (the internal clipboard bridge
  is a real client, spec "Who is checked").
- **Core `SendEvent`** (handler `process_request/selection.rs`, resolver `send_event_recipients`), Xorg
  `dix/events.c:5571-5590`:
  - After today's validation and destination resolution (no change in order or errors), the resolver returns the
    candidate windows it walks, not only client ids.
  - Propagating: at each candidate window, the send check first; a denial ends the request with `Success` and no
    delivery (Xorg `:5574-5575`). Then the window's selected recipients are each asked `receive` with
    `EventSubject::Window(candidate)`; denied ones are skipped. If at least one recipient received the event, the walk
    stops, as today. If every selected recipient was denied, the walk continues to the parent as if the window had no
    selector (Xorg `DeliverEventsToWindow` returns 0 when receive skips every input client, `:2289-2290`, `:2415-2431`),
    applying the do-not-propagate mask and the effective-focus stop as today.
  - Zero mask (only the window's creator, Xorg `DeliverToWindowOwner`): a receive denial of the creator counts as a
    non-delivery and stops the walk (Xorg `:2211`, returns `EVENT_NOT_DELIVERED`, counted as a nondelivery that ends
    `ProcSendEvent`'s loop).
  - Not propagating: one send check at the destination; denial → `Success`, no delivery (Xorg `:5587`); then receive per
    recipient.
  - `InputFocus` with no focus (`None`) asks nothing and answers as today.
  - The rootless `SelectionNotify` interception and the root `ClientMessage` handling are deliveries of this request:
    the send check at the destination runs before the interception; a denial skips the interception.
- **XI `SendExtensionEvent`** (`xi/dispatch/events.rs`, resolver `xi1_send_extension_event_resolve_targets`): the same
  rules, with `device = Some(request device id)` and the batch built from all supplied events. The spec asks the send
  check at every propagated candidate (Xorg checks only the non-propagated case, `Xi/exevents.c:2962-2974`); the spec
  binds. The existing XI resolver has no effective-focus stop; none is added (no behaviour change).
- **Server-generated events have no sender:** no send check anywhere else in this plan (spec "Who is checked"). XTEST
  stays as is (spec "XTEST").
- **Revocation:** checks are made at each delivery; bytes already written to a client's output buffer are not recalled
  (spec "Scoped, revocable grants").

## Review Focus

1. No-policy parity: `SendEvent` and `SendExtensionEvent` produce today's bytes and errors for PointerWindow,
   InputFocus (incl. `None` and PointerRoot), explicit windows, propagate 0/1, zero mask, do-not-propagate and
   effective focus.
2. Propagation under denial: send denied at the first, a middle and the last candidate; receive denying all selectors of
   a window (walk continues) vs denying the creator under a zero mask (walk stops).
3. A recipient denied at one window does not receive the event from a later window of the same walk unless it is
   selected there and allowed there.
4. Order: send check before any receive check at the same window; receive checks only for recipients the mask selects.
5. The batch a policy sees: type code without the send-event bit, XI events with the XI major opcode.

---

### Task 1: Hooks, EventSubject, EventBatch and helpers

**Files:** `crates/yserver-core/src/access.rs`, `server.rs` (helpers next to `check_resource`),
`process_request/tests/extensions.rs` (recording policy records send/receive calls).

- [ ] **Step 1: Failing tests:** `send_and_receive_default_allow` (a policy overriding nothing allows);
  `check_send_and_receive_follow_first_denial` (two policies, allow-then-deny and deny-then-allow: denied both ways,
  the first denier's decision, the policy after a denier not asked; mutation: ask every policy);
  `empty_policy_list_builds_no_context` for both helpers; `server_owner_sender_is_not_checked`;
  `event_batch_describes_core_and_extension_events` (send bit stripped, XI major and evtype). Unit tests of the
  helpers are allowed here (they are the interface, plan 3a precedent); site tests go through `process_request`.
- [ ] **Steps 2–4:** run, fail; implement; run, pass, whole suite unmodified.
- [ ] **Step 5: Commit** `feat(core): access control PR 4a, Task 1: send and receive hooks`.

### Task 2: Core SendEvent

**Files:** `process_request/selection.rs` (handler and `send_event_recipients`); tests in a new
`process_request/tests/access_send.rs`.

- [ ] **Step 1: Failing tests:** ASK rows (sender, candidate window, `device: None`, batch; then one receive per
  selected recipient with `EventSubject::Window(candidate)` and the request's opcodes) for PointerWindow, InputFocus and
  an explicit window, propagate 0 and 1; DENY rows: send denied at the destination (no bytes to anyone, `Success`), send
  denied at a middle candidate while propagating (no delivery from that window or above);
  `receive_denied_for_all_selectors_continues_to_parent` (mutation: stop when the window has selectors);
  `receive_denied_creator_under_zero_mask_stops` (mutation: continue); `receive_denied_one_of_two_selectors` (the other
  gets the event, the walk stops); `rootless_selection_notify_send_denied_skips_interception`; no-policy rows equal to
  today's bytes for every resolver branch.
- [ ] **Steps 2–5.** Commit `… Task 2: core SendEvent`.

### Task 3: XI SendExtensionEvent

**Files:** `process_request/xi/dispatch/events.rs`, `process_request/xi/mod.rs`; tests in `access_send.rs`.

- [ ] **Step 1: Failing tests:** as Task 2, with `device: Some(id)` and a multi-event batch (every event described);
  send checked at every propagated candidate (mutation: check only the destination); the class do-not-propagate walk
  unchanged with no policy.
- [ ] **Steps 2–5.** Commit `… Task 3: XI SendExtensionEvent`.

### Task 4: Spec rows, status and gate

- [ ] **Step 1:** spec: the hook signatures and `EventBatch` under "What a policy can see"; under "A denial is
  interpreted by its site", the propagation rules of Task 2 (all selectors denied → walk continues; creator denied →
  stops), citing Xorg; under "Where the checks go" → Receive, the rulings for later plans: redirect notifications
  (MapRequest, ConfigureRequest, ResizeRequest, CirculateRequest) denied by receive count as delivered, so the operation
  stays redirected (Xorg `dix/events.c:2568`, `:2582`); selection events go through receive; SYNC, DPMS, MappingNotify
  and other subjectless notifications use `Global`; RECORD checks only records that carry X events; SHM `Completion`
  and an `Await`-triggered `CounterNotify` are direct results (exempt, like `GraphicsExpose`); Present notifications are
  checked; the legacy `server.rs` writers without production callers are out of scope.
- [ ] **Step 2:** `docs/status.md`: one entry at the top for PR 4a; the SendEvent validation differences listed as
  open findings.
- [ ] **Step 3: Gate:** as plan 3c-ii Task 7 (fmt, clippy ×3 incl. `tcp-transport` and `xdmcp`, FreeBSD clippy,
  windows-gnu check, workspace tests with and without `xdmcp`, each `crates/*/tests` file, lavapipe `--ignored`, the
  access modules 5 runs under `nproc` busy loops, `tools/check-layout.sh`, `just man`).
- [ ] **Step 4: Commit** `docs: access control PR 4a spec rows and status`.
