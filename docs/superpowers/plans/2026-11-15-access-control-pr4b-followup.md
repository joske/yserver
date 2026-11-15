# Access control PR 4b follow-up Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Address the maintainer's follow-up points from the #369 (PR 4b) merge review: one selection routing fix, three test-quality fixes and the docs cleanup.

**Architecture:** One production change in `ConvertSelection`: the no-owner answer is routed by the issuing client, through the helper the denied-owner path already uses. Everything else is tests and docs. With no policy installed, the only client-visible change is the selection fix (Task 1), which is pre-existing behaviour, not access control.

**Tech Stack:** Rust, yserver-core unit tests.

**Spec:** `docs/superpowers/specs/2026-10-30-access-control-layer-design.md` (selection section, "If Receive denies the owner its `SelectionRequest`…"). Base: joske/master `58d7d3fb`.

## Global Constraints

- Reuse first: no new helper where an existing one does the job; new code only where it is needed, each piece justified.
- No new `YSERVER_*` environment variables.
- Tests exercise real request paths (`apply_convert_selection` through the request dispatcher, not a hand-built call of the private helper).
- Each test comment names the mutation that fails it.
- Xorg reference: xorg-server 21.1.24, cited as `../xserver` at tag `xorg-server-21.1.24` (never a local absolute path).
- Commits signed (`-S`), explicit paths, one commit per task, trailer `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>` (plus `Implemented-By: codex (gpt-6-luna xhigh)` for codex work).

## Review Focus

1. A rootless bridge `ConvertSelection` with no owner must still be answered on the host queue (`note_refusal`), never as socket bytes: the fix must not move bridge conversions to `send_selection_failure`.
2. A socket client whose requestor window is not the bridge's: unchanged, `SelectionNotify(None)` to the issuer.
3. Resource tests that legitimately produce events (a request whose handler also emits a notification) must list their expected `Receive` calls explicitly instead of failing spuriously or filtering everything.
4. Removing the aggregate re-run must not drop a no-policy assertion that only the aggregate made (synthetic SelectionNotify, exemptions, deny-all extension writers, buffered property output, Present before ConfigureNotify).
5. Expected bytes in no-policy assertions must be built field by field from protocol constants in the test, not by calling the production encoder.

## Out of scope (tracked elsewhere)

- Core events still unchecked, deferred by plan 4b: root and COW `ConfigureNotify` (`run.rs` RANDR resize paths, `randr_ext.rs`) go to plan 4e; core `Enter`/`Leave` (`grabs.rs`) to plan 4c.
- The PR 4b description's "as Xorg" wording for the denied `SelectionRequest` answer: the PR is merged; the spec already states Xorg's direct writes have no receive hook. Future PR descriptions call it our ruling.

---

### Task 1: No-owner `ConvertSelection` answers the issuer

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request/selection.rs` (the no-owner `else if note_refusal(state, requestor, …)` / `else` chain at the end of `apply_convert_selection`, and `answer_denied_selection_request`)
- Test: `crates/yserver-core/src/core_loop/process_request/tests/access_notify_selection.rs`

**Interfaces:**
- Consumes: `answer_denied_selection_request(state, client_id, requestor, selection, target_atom, time_val)`, which already routes by issuer (`SERVER_OWNER` → `note_refusal`, otherwise `send_selection_failure` to the issuer).
- Produces: the helper renamed `answer_selection_failure` (same signature), used by the bridge-owner denial, the socket-owner denial and the no-owner path.

Xorg answers a conversion with no owner to the issuing client (`dix/selection.c:304-311`, `WriteEventsToClient(client, …)`). Today the no-owner path tries `note_refusal(requestor)` first, so a socket client that names the bridge window as requestor is answered on the host queue and gets nothing.

- [ ] **Step 1: Write the failing tests**, through the real request path, rootless fixture with the bridge window:
  - `no_owner_socket_conversion_with_bridge_requestor_answers_issuer`: a socket client issues `ConvertSelection` for an unowned selection with the bridge window as requestor. Assert the issuer drains exactly one 32-byte `SelectionNotify` (type 31, not synthetic) with requestor = bridge window, the selection, the target and property 0. Assert the host selection queue is empty. Mutation: restore `note_refusal(requestor)` before the issuer check.
  - `no_owner_bridge_conversion_queues_refusal`: the bridge (`SERVER_OWNER`) converts an unowned selection. Assert the host queue holds one `SelectionEvent::Notify { property: AtomId(0), .. }` and no socket client receives bytes. Mutation: always call `send_selection_failure`. (If an existing test already pins this exact case, extend it instead and name it in the commit message.)
- [ ] **Step 2: Run** `cargo test -p yserver-core no_owner_ -- --nocapture`. Expected: the first test FAILS (issuer gets nothing), the second PASSES.
- [ ] **Step 3: Implement.** Rename the helper to `answer_selection_failure`, and replace the no-owner `else if … else` chain with one call to it. Update the comments: the answer goes to the issuer as in Xorg; the bridge's own typed conversion is answered on its host queue.
- [ ] **Step 4: Run** `cargo test -p yserver-core selection` and `cargo test -p yserver-core access_notify_selection`. Expected: all PASS.
- [ ] **Step 5: Commit** `fix(selection): answer a conversion with no owner to the issuer`.

### Task 2: Resource tests catch unexpected Receive calls again

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request/tests/access_resources.rs` (`expected_resource_checks`, around line 1015)
- Modify: `crates/yserver-core/src/core_loop/process_request/tests/extensions.rs` (recording policy `owner` field, around lines 189-194)
- Modify, only where needed: the callers in `access_resources.rs`, `access_gc_font.rs`, `access_sync.rs` (26 call sites)

Invariant: a resource-access test fails if the request makes any policy call that the test does not expect, including `Receive`. Before 4b, a `Receive` call reached `call.target.expect("resource target")` and failed. The 4b filter (`hook == ExtensionHook::Resource`) silently drops it.

- [ ] **Step 1: Write the failing test** `expected_resource_checks_rejects_unexpected_receive`: build a `RecordedExtensionCheck` list with one Resource call and one Receive call, and assert `expected_resource_checks` panics (`#[should_panic(expected = "unexpected Receive")]`). Mutation: restore the filter.
- [ ] **Step 2: Run** it. Expected: FAIL (no panic).
- [ ] **Step 3: Implement.** `expected_resource_checks` asserts every recorded call is `ExtensionHook::Resource` (panic message `unexpected {hook:?}` with the request). Add `expected_resource_checks_and_receives(calls) -> (Vec<RecordedResourceExpectation>, Vec<RecordedExtensionCheck>)` only if some caller legitimately records `Receive`; those callers assert their receive list explicitly. In the recording policy, take the receive-window owner fallback only for `ExtensionHook::Receive`, so a Resource call's `owner` is the target's owner as before 4b.
- [ ] **Step 4: Run** `cargo test -p yserver-core access_`. Expected: all PASS; every caller moved to the second helper names its expected Receive calls in an assertion.
- [ ] **Step 5: Commit** `test(access): resource checks fail on unexpected policy calls`.

### Task 3: No-policy tests assert independent bytes, and nothing runs twice

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request/tests/access_notify_exemptions.rs` (`receive_no_policy_core_bytes_match_baseline`)
- Modify: `run_no_policy_parity_fixtures` in `access_notify_windows.rs`, `access_notify_lifecycle.rs`, `access_notify_objects.rs`, `access_notify_egress.rs`, `access_notify_selection.rs`, `present_pixmap.rs`, and the `#[test]` functions they call

Invariants:
- The no-policy run of every 4b site is compared with expected bytes built in the test, field by field, from protocol constants. A comparison with the recording-Allow run on the same branch is kept as a second assertion but never stands alone. The mutation "change one field of a no-policy event in production" must fail the no-policy assertion even when the Allow run changes the same way.
- No `#[test]` function calls another `#[test]` function.

- [ ] **Step 1: Audit.** For each `#[test]` listed in the six `run_no_policy_parity_fixtures` functions, record whether its no-policy output is already compared with independent expected bytes. Write the table in the commit message body (test name, site ids N01–N47, independent bytes yes/no).
- [ ] **Step 2: Add the missing independent assertions** in each "no" test, then mutate one production field per changed test file and confirm the no-policy assertion fails. Record the mutation in the test comment.
- [ ] **Step 3: Remove the re-runs.** Delete the six `run_no_policy_parity_fixtures` functions and the loop that calls them. `receive_no_policy_core_bytes_match_baseline` keeps only the assertions it makes itself (synthetic `SelectionNotify`, exemptions, deny-all extension writers, buffered property output, Present before `ConfigureNotify`), renamed `receive_no_policy_aggregate_paths_match_expected_bytes`. If `run_denied_owner_refusal_fixture` calls a `#[test]`, apply the same rule: move the body into a plain `fn` that both callers use.
- [ ] **Step 4: Run** `cargo test -p yserver-core access_notify` and `cargo test -p yserver-core present_pixmap`. Expected: all PASS; the test count drops only by the removed aggregate re-runs.
- [ ] **Step 5: Commit** `test(access): no-policy tests compare with independent bytes`.

### Task 4: Docs

**Files:**
- Modify: `docs/status.md`
- Modify: `docs/superpowers/plans/2026-11-10-access-control-pr3c-i-review1.md`, `2026-11-10-access-control-pr3c-ii.md`, `2026-11-11-access-control-pr4a.md`, `2026-11-12-access-control-pr4a-followup.md`, `2026-11-13-access-control-pr4b.md`

- [ ] **Step 1: `docs/status.md`.**
  - Merge the two PR 4b entries (Task 3, Task 8) into one entry: "PR 4b: Receive checks on core notifications (#369)". Keep the facts (47 sites, exemptions, `ResizeRedirect` gap, remaining 4c–4e work) and add one line for this follow-up.
  - Remove the duplicate PR 4a entry (two identical lines).
  - Drop the "Full details … report" sentences from the access-control entries; those files are not in the repo.
- [ ] **Step 2: Citations.** In the five access-control plans, replace the local path `/data/Projects/xorg-21.1.24` with `../xserver` (tag `xorg-server-21.1.24`).
- [ ] **Step 3: Check** `grep -rn "/data/Projects" docs/superpowers/plans/*access-control*` (expected: no output), `grep -n "PR 4b" docs/status.md` (expected: one entry) and `tools/check-layout.sh` (expected: pass).
- [ ] **Step 4: Commit** `docs: one PR 4b status entry, repo-relative Xorg citations`.

### Gate

Run `~/Projects/claude-scratch/access-control/pr4/gate-4b/gate.sh`: fmt, the three clippy variants, Windows check, FreeBSD clippy, `cargo test --workspace --features xdmcp`, the lavapipe `--ignored` suite, `access_*` and `selection` tests 5× under 16 busy loops, `tools/check-layout.sh` and `just man`. No hardware run: the only production change is in selection routing, which the headless tests cover.
