# Access control PR 4c — test consolidation (review of #389) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Keep the coverage of PR 4c's three Receive test files and cut their size. The files total 10,317 lines for 55 tests. The cut comes from shared fixtures and one helper that runs a scenario under no policy, allow-all and a targeted deny. The next PR in the series then adds rows instead of copying tests.

**Architecture:** No production code changes. The existing shared module `access_notify_support.rs` gains one three-mode runner and its expectation types. Each of the three files keeps its area and its scenario builders (fixture setup, request bodies, literal byte encoders). Its tests that fit the three-mode shape become `ReceiveCase` rows, checked the same way every time. The scenarios that do not fit stay focused `#[test]` functions built on the same builders and the same shared assertions. Task 1 measures the size, and Tasks 2 and 3 report against that measurement.

**Tech Stack:** Rust, `yserver-core` unit tests (`core_loop/process_request/tests`), `NotifyFixture`, `RecordingExtensionPolicy`; std only.

**Spec:** `docs/superpowers/specs/2026-10-30-access-control-layer-design.md`, "Input delivery under denial" → **Notifications**. It binds what the tests must prove: a denial drops only that recipient's copy, recipients and multiplicity are unchanged, subjects are as listed, and no-policy bytes are unchanged.

**Request:** maintainer review of #389. "Please keep the coverage but share the fixtures and parameterize the repeated allow/deny/no-policy cases (a table of site × expected literal bytes, or a helper that runs the three modes and checks hook, subject and description), so that the next PR in the series can extend it rather than copy it."

## Global Constraints

- The final diff touches only `crates/yserver-core/src/core_loop/process_request/tests/`: no production code, and no `cfg(test)` code in production. Temporary production patches are allowed only while running a mutation probe, one at a time, and are removed before the next.
- **Coverage is preserved by inventory, not by test count.** Before editing a file, the implementer writes the inventory into the report. It is keyed by test × variant × mode × deny alternative; one example is the XI1 focus test, which covers keyboard and pointer and denies either the focus event or the state batch. Each key lists the assertion groups below:
  - no-policy bytes per client;
  - allow-all bytes;
  - the full recorded policy log: Receive checks with client, subject, events and request opcodes, plus any other hook and the assertions that none occur;
  - the deny-mode log;
  - bytes per client after the denial;
  - state and writer assertions in every mode (no-policy and allow-all included), after the denial, and intermediate ones;
  - producer return values: for example the XI property emitter's empty disconnect list (`access_xi_notify.rs:2666`) and the direct XKB MapNotify helper's recipient list, which includes the denied client (`access_xkb_notify.rs:2349`). These stay asserted in the shared action helper or a focused test.

  After the migration each group maps to a row field, a row closure, or a named focused test. The report shows the mapping.
- **Mutation procedure (every task).**
  - Pin the baseline, which is the task's entry commit: Task 1 starts at the commit that lands R1, and Tasks 2 and 3 at the previous task's last commit.
  - Split comments that name several mutations into distinct mutations, and number them in the report. Today's comment markers are 27 crossing, 24 XI and 21 XKB at `532c852f`. Task 2 adds R1's `xkbType` mutations.
  - Each distinct mutation is a probe. **Baseline run, before migrating.** Apply each probe alone at the baseline, run the original owning test, and record the failing assertion with the invariant and mode it checks. A probe counts as stale only if the patched baseline compiles and the owning test actually runs and passes. A stale probe is recorded with the reason and is not counted as lost. **Replay, after migrating.** Apply the same probe alone and run the migrated owning row or focused test. The corresponding assertion, for the same invariant and mode, must fail; a runner guard that fires earlier for a different reason does not count. A probe that edits the test driver, such as issuing GetMap in place of SetMap, is applied to both the original and the migrated driver.
  - A probe that the baseline turned into correct behaviour (R1 makes the old K11 "describe `xkbType`" probe correct) is recorded as retired, with the reason. A probe whose owning test is outside these three files, such as R1's `SendEvent` probe, stays with that test and is not migrated.
  - Remove the patch before the next mutation. The tree must be clean of mutations at report time.
  - A probe that no longer fails its named assertion is a coverage loss and blocks the task.
- Literal expected bytes stay literal: test-side encoders build them from fixed values, never the production encoder under test.
- Each row or focused test drives the same production entry its test uses today: `process_request`, the backend input entries, the server-work paths, and the XTEST producer the XI test calls (`access_xi_notify.rs:1284`). No entry is replaced by a lower-level call.
- **Size.** Aim for half of today's 10,317 lines across the three files plus the added support code. This is best effort, not a gate: about 4,200 of today's lines are scenario helpers, and much of that stays. Task 1 reports its measured reduction and a projection. Readability of the focused tests is not traded for line count.
- Test rules (A/B/C):
  - A: each denial row or focused test checks the end state its scenario names (device, grab, barrier, snapshot, writers);
  - B: state advances only through the production entries above;
  - C: no hardware for this plan.
- Gate per task:
  - `cargo +nightly fmt --all`;
  - under the lavapipe env: `cargo test -p yserver-core --lib access_`, then the whole `cargo test --workspace --locked --features xdmcp` and its `-- --ignored` suite;
  - `cargo clippy --all-targets -- -D warnings`, then the workspace clippy with `--features xdmcp`;
  - `tools/check-layout.sh`.

## Review Focus

- A row whose deny predicate never matches, or denies a different but byte-identical copy (the XI hierarchy root and child copies for one client). The runner checks which calls were actually denied against the row's `denied_calls`, including the subject; a `#[should_panic(expected = ...)]` self-test pins the no-match message.
- Order and multiplicity of checks. Total order, per-client order and multiset comparison are distinct choices. The multiset choice counts duplicates. Each row states its choice, and the choice matches the original test's scope. Examples: COW overlay before parent within one client, XI1 focus before the state batch, and GetKbdByName recipient order derived from each fresh fixture.
- Request context. Request opcodes in the Receive trace are compared in both policy modes, so a regression that passes `None` for the triggering request fails.
- Fixture state shared between modes. Each mode builds a fresh fixture and a fresh policy log from the row's builder.
- Sequence numbers. A denied copy consumes no sequence number; the rows or tests that assert this today (the literal bytes of the next delivered event on the same client) keep that assertion.

---

### Task 1: Three-mode runner and the XI notification file

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request/tests/access_notify_support.rs`
- Modify: `crates/yserver-core/src/core_loop/process_request/tests/access_xi_notify.rs`

**Interfaces:**
- Produces, in `access_notify_support.rs` (all `pub(super)`). Names are fixed; the implementer may adjust borrowing and lifetimes, but not the meaning:
  - `struct ExpectedReceive { client: ClientId, request: Option<RequestOpcodes>, subject: EventSubject, events: Vec<EventDescription> }`, compared against the matching fields of a Receive `RecordedExtensionCheck` (`tests/extensions.rs:19`).
  - `enum CallOrder { Total, PerClient, Multiset }`, where `Multiset` counts duplicates.
  - `struct ReceiveCase<'a, C> { name: &'static str, build: &'a dyn Fn() -> (NotifyFixture, C), act: &'a dyn Fn(&mut NotifyFixture, &C), expect: &'a dyn Fn(&C) -> CaseExpect }`. The callbacks may capture row parameters (selection, minor opcode, request body, literal encoder), so the variants of one scenario share one builder. `C` carries same-run data, such as device ids or a `HostXidMap`, from the builder to the action and the expectations. Fixture-derived data stays inside one run.
  - `struct CaseExpect { streams: Vec<Stream>, receives: Vec<ExpectedReceive>, deny_receives: Option<Vec<ExpectedReceive>>, order: CallOrder, denied_calls: Vec<ExpectedReceive>, deny_first_only: bool, forbidden_hooks: &'static [ExtensionHook], after_deny: Option<Box<dyn Fn(&NotifyFixture, &NotifyFixture)>> }`.
    - `receives` is the Receive-only trace. `deny_receives` is set only where the deny-mode trace differs.
    - `forbidden_hooks` (normally `&[ExtensionHook::Send]`) must not appear in either log. Other hooks, such as Dispatch, are not pinned.
  - `struct Stream { client: ClientId, bytes: Vec<u8>, denied: Option<Vec<u8>> }`: one baseline stream per peer, and `denied` only where the deny mode changes it.
  - `denied_calls` are the deny criteria and the expected result at once. The runner's deny policy considers Receive calls only. It denies each Receive call that equals an entry of `denied_calls` (client, request, subject, events); with `deny_first_only`, it denies only the first such call (XI bootstrap). It records the calls it denied, which must equal `denied_calls` with multiplicity, and there must be at least one. `after_deny` receives the allow-all fixture and the deny fixture after their actions. Intermediate checkpoints do not belong there; see focused tests.
  - `fn check_receive_case<C>(case: &ReceiveCase<C>)`. It runs `build` and `act` three times, each with a fresh fixture and a fresh log:
    - **no policy:** each `streams` client's bytes equal `bytes`.
    - **allow-all recording policy:** the bytes equal `bytes`; the Receive trace equals `receives` under `order`; no `forbidden_hooks` call.
    - **deny policy:** the denied calls equal `denied_calls`; each client's bytes equal `denied`, or else `bytes`; each client named in `denied_calls` gets bytes that differ from its allow-all bytes; the Receive trace equals `deny_receives`, or else `receives`, under `order`; no `forbidden_hooks` call; no writer queued for disconnect; then `after_deny`.
    - **coverage:** every client with a fixture peer appears in `streams`, so no stream can be left out silently.

    Every assertion message carries `case.name` and the mode.
  - `fn receive_checks(calls) -> Vec<RecordedExtensionCheck>` moves here, and the per-file copies go.
- **Focused tests** stay `#[test]` functions that reuse the builders and the shared assertions. Each one carries a comment naming why it is not a row. The known exceptions are:
  - byte-only baseline-frame tests;
  - the deliberately zero-match cooked-pointer test (`access_crossing_notify.rs:3466`);
  - tests with intermediate checkpoints: the barrier test (`access_crossing_notify.rs:4596`), the synthetic XI grab test (`access_crossing_notify.rs:3087`, active grab and success reply after activation and re-grab, delivery before ungrab) and the grabbed-crossing test (`access_crossing_notify.rs:1558`);
  - tests with multi-request originator ordering;
  - `receive_xi_presence_from_server_work_has_no_request` (`access_xi_notify.rs:423`): it checks and clears the log before the second request;
  - the XI2 grab-chain test (`receive_xi2_grab_chain_denial`, `access_crossing_notify.rs:2859`): active grab, success reply and both clients' NotifyGrab bytes before `UngrabPointer`;
  - `receive_xkb_new_keyboard_allow_control` (`access_xkb_notify.rs:2177`): three requests on one fixture, including the GetKbdByName recipient order.

  Tests the implementer adds to this list are named in the report with the reason.

- [ ] **Step 1:** Write the inventory for `access_xi_notify.rs` (16 tests, 24 mutation comments) into the report.
- [ ] **Step 1b:** Baseline run of this file's probes (mutation procedure), before any edit.
- [ ] **Step 2:** Add the runner and types to `access_notify_support.rs`. Add two self-tests, each `#[should_panic(expected = "<the runner's message>")]`: a row whose `deny` never matches, and a row whose `denied_calls` names the root copy while `deny` denies the child copy of the same client.
- [ ] **Step 3:** Migrate the XI tests to rows. Where several tests share a scenario and differ only in what is denied, they become several rows over one builder. The XI bootstrap tests become rows with `deny_first_only` (first of two identical checks) or without it (both). The rest follow the focused-test rule above.
- [ ] **Step 4:** Replay this file's probes on the migrated tests (mutation procedure).
- [ ] **Step 5:** Run the gate. Report the line counts before and after, and a projection for Tasks 2–3.

### Task 2: XKB notification file

**Files:** Modify `crates/yserver-core/src/core_loop/process_request/tests/access_xkb_notify.rs`.

**Interfaces:** Consumes Task 1's `ReceiveCase` and `check_receive_case`, unchanged. If a field is missing for XKB, extend the runner in `access_notify_support.rs` and say so in the report. Do not fork it.

- [ ] **Step 1:** Write the inventory (21 tests, 21 mutation comments).
- [ ] **Step 1b:** Baseline run of this file's probes, before any edit.
- [ ] **Step 2:** Migrate. The K1–K12 sites become rows. The surviving legacy core MappingNotify is a wire assertion: it goes in the `Stream` literal bytes. Denial targets after R1:
  - The indicator-map test denies the selected client's `IndicatorStateNotify` (xkbType 4).
  - The indicator-names test denies the selected client's `ExtensionDeviceNotify` (xkbType 11).
  - The snapshot-advance test denies both subscribers' `StateNotify` (xkbType 2) in one run (two `denied_calls`).
  - The state-follow-up test does the same, and also checks that their indicator follow-ups are delivered. GetKbdByName captures the run's client-map order in its builder's `C`, and its row uses `Total` order, so the recipient order stays asserted in both policy modes.
- [ ] **Step 3:** Replay this file's probes on the migrated tests. The R1 commit is part of the baseline; record retired and out-of-file probes as the procedure says.
- [ ] **Step 4:** Run the gate and report the line counts.

### Task 3: Crossings, focus, barriers, root and COW ConfigureNotify file

**Files:** Modify `crates/yserver-core/src/core_loop/process_request/tests/access_crossing_notify.rs`.

**Interfaces:** As in Task 2.

- [ ] **Step 1:** Write the inventory (18 tests, 27 mutation comments).
- [ ] **Step 1b:** Baseline run of this file's probes, before any edit.
- [ ] **Step 2:** Migrate. Chains that deny one window of several become one row per denied window over one builder. The XI2-over-core dedup row asserts that the shadowed core copy has no Receive call, comparing with `Multiset`, as the original test does not depend on order between clients. COW overlay-before-parent and XI1 focus-before-state-batch use `PerClient`. The synthetic XI grab, grabbed-crossing and XI2 grab-chain tests (`receive_xi2_grab_chain_denial`: reply, active grab and both streams checked before ungrab) stay focused; see Task 1's list.
- [ ] **Step 3:** Replay this file's probes on the migrated tests.
- [ ] **Step 4:** Run the gate and report the total line count of the three files plus the lines added to `access_notify_support.rs` across Tasks 1–3, against the size target.
