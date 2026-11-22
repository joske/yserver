# Async Present Future Target Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A Present with `PresentOptionAsync` whose target is in the future when it arrives is handed to scanout as a synced request, as Xorg's `sync_flip` (#23).

**Architecture:**
- **Decide at arrival.** The decision is taken where both PresentPixmap variants build their `PendingPresentPixmap`, with the same general-clock msc that computes the effective target.
- **Keep it.** It is stored on the pending request and survives every deferral.
- **Apply it at execution.** `execute_present_pixmap_copy` applies it to the `PresentScanoutCandidate` options it hands to scanout. Execution timing, parking, wakeups, completion stamping and the KMS backend stay unchanged.

**Tech Stack:** Rust (yserver-core); `RecordingBackend` candidate observation.

**Spec:** `docs/superpowers/specs/2026-11-21-async-present-target-design.md` (revision 3, approved by the user 2026-11-21).

## Global Constraints

- **Base:** branch `fix/async-present-target` on master `3ef6570b` plus the docs commits.
- **Citations:** cite Xorg as `../xserver` (tag xorg-server-21.1.24): `present/present_vblank.c:110-122`, `present/present_scmd.c:232,579,770-771`. Cite Mesa `loader_dri3_helper.c:1118-1159` and `wsi_common_x11.c:1827,2632`. No copyleft code.
- **Reuse first:** no new scheduling, parking, wake or completion path. `classify_msc_due` stays unchanged. The KMS backend stays unchanged: it already treats a candidate without Async as synced (`kms/present.rs` ~340-400, `kms/scanout.rs` ~1812).
- **No new `YSERVER_*` environment variables.**
- **Out of scope (spec Non-goals):**
  - composed-scene async rejection;
  - the completion sweep's stamping policy;
  - synced timing, supersession policy, VRR, and the idle/blackout ladder.
- **Tests (rules A/B/C):**
  - **Location:** in `crates/yserver-core/src/core_loop/process_request/tests/present_pixmap.rs`.
  - **Inputs:** requests go through `process_request` dispatch (PresentPixmap / PresentPixmapSynced bodies via the existing `pixmap_body`, `pixmap_synced_body`, `dispatch_present_body` and `future_present_pixmap_request` helpers). Deferral and execution go through the core loop's existing drivers (`run_core_present_wait_drain`, `run_core_present_request_and_wait`).
  - **Observation:** `RecordingBackend::present_direct_candidates`, plus the pending entry's stored decision.
  - **End state:** every scenario ends with the end-state check: `present_pending_exec` and `present_pending_complete` empty (or the expected live set), and no pins left.
  - **No injected state:** no hand-built pending entries, and no direct calls to `execute_present_pixmap_copy`.
- **Red runs and mutations:** every new test is seen failing on the base for its stated reason. Tests marked GUARD must pass on the base. Every production change has a mutation, listed with the tests it must turn red.
- **Commits:** signed (`-S`), explicit paths, trailers `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>` and `Implemented-By: codex (gpt-6-luna xhigh)`.
- **No hardware or server runs by the implementer.** No hardware and no yserver binary.
- **Per-task gate:**
  - `cargo +nightly fmt`;
  - `cargo clippy --workspace --all-targets -- -D warnings`;
  - `cargo test -p yserver-core present`;
  - `cargo test -p yserver kms::render::backend::tests::present_scanout`;
  - `tools/check-layout.sh`;
  - rule F: `cargo test -p yserver-core present` five times under `nproc` busy loops.

## Review Focus

1. **A late Async request after an on-time one.** The late request (target ≤ msc at arrival) keeps Async; the on-time request before it does not. Test in Task 1 (`async_late_after_on_time_keeps_async`).
2. **Same-target supersession.** An Async future-target request superseded by a successor with the same target reports the skipped completion exactly as today, and the successor's candidate is synced. Test in Task 1.
3. **PresentPixmapSynced deferred by its acquire point past the target.** It is still synced when it executes. Test in Task 1.
4. **VRR window.** The `variable_refresh` of the candidate is unchanged by the decision. Test in Task 1.
5. **Client disconnect with a parked Async future-target request.** The existing teardown (`process_disconnect.rs` ~913) still works with the new field. Test in Task 1.

---

### Task 1: Arrival-time sync decision for Async future targets

Spec Design rules 1-4.

**Files:**
- Modify: `crates/yserver-core/src/server.rs`: `PendingPresentPixmap` (~3017) gains `pub future_target_sync: bool`.
- Modify: `crates/yserver-core/src/core_loop/process_request/present_ext.rs`:
  - the two arrival constructors (~2151 PresentPixmap, ~2596 PresentPixmapSynced) set the field from `domain.raw_msc` and the computed `effective_target_msc`;
  - `execute_present_pixmap_copy` (~948-1015) applies it to the candidate options.
- Modify: every other `PendingPresentPixmap` constructor (14 sites in total, including `process_disconnect.rs` ~913 and tests). Each passes the value it should; any non-arrival constructor carries over the original request's value.
- Test: `crates/yserver-core/src/core_loop/process_request/tests/present_pixmap.rs`.

**Interfaces:**
- Produces: `PendingPresentPixmap::future_target_sync: bool`. It is `true` exactly when `masked_options & PRESENT_OPTION_ASYNC != 0` and the effective target is `Some(eff)` with `msc_is_after(eff, domain.raw_msc)` at arrival.
- `execute_present_pixmap_copy`: the candidate's `options` are `masked_options & !PRESENT_OPTION_ASYNC` when the flag is set, otherwise `masked_options`. The `CompletedPresentEvent` options and every other use of `masked_options` are unchanged.

- [ ] **Step 1: Write the tests.** Each dispatches real requests and drives the core loop with the existing drivers.
  - `async_next_target_present_is_synced`: Async PresentPixmap, target = current msc + 1, source ready, no flip in flight. It executes at arrival, and the recorded candidate's options lack Async. End state.
  - `async_current_target_keeps_async`: Async with target = current msc. The candidate keeps Async. GUARD.
  - `async_late_after_on_time_keeps_async` (Review Focus 1): first an Async request at msc + 1, then an Async request whose target ≤ msc at its arrival. The first candidate is synced and the second keeps Async.
  - `async_future_target_fence_deferred_past_target_stays_synced`: Async target msc + 1, with a producer fence that signals only after the clock reached the target (advance the clock through the backend's existing test sequence path). The executed candidate lacks Async.
  - `async_synced_variant_acquire_deferred_stays_synced` (Review Focus 3): the same through PresentPixmapSynced with an acquire syncobj.
  - `async_distant_target_idle_fallback_is_synced`: Async with target msc + 20 on an idle display (the `present_ext.rs` ~867 fallback executes it now). The candidate lacks Async. The completion stamp is whatever today's sweep gives, as in the spec's exemption; assert only the candidate.
  - `async_distant_target_failed_arm_is_synced`: the same with a failed shared-wake arm (`run.rs` ~3373), using the `RecordingBackend` arm-failure control if one exists. If none exists, report it and cover it with the idle case only.
  - `synced_present_unchanged` (GUARD): non-Async at msc + 1. The candidate options equal `masked_options`.
  - `async_same_target_supersession_unchanged` (Review Focus 2): two Async presents with the same future target while the first waits. The first completes as skipped exactly as today, and the second's candidate lacks Async.
  - `async_future_target_keeps_variable_refresh` (Review Focus 4): with `_VARIABLE_REFRESH` set on the window, the synced candidate's `variable_refresh` is true.
  - `async_future_target_pending_survives_disconnect` (Review Focus 5): a parked Async future-target request, then client disconnect. The entry and pin are gone. GUARD.
  - `fifo_relaxed_shaped_sequence_never_repeats_target`: three Async presents, each targeting the msc the previous completion reported + 1, as Vulkan FIFO_RELAXED and Mesa adaptive do. All candidates are synced, and the targets strictly increase. Note in the report which completion msc the `RecordingBackend` reports; equality with the target is validated on hardware (Task 2).
- [ ] **Step 2: Run** `cargo test -p yserver-core present`. Expected: every non-GUARD test FAILs on an assertion (Async present in the candidate), and the GUARDs pass.
- [ ] **Step 3: Implement** as in Files and Interfaces.
- [ ] **Step 4: Run** the task's tests, then the gate. Expected: PASS.
- [ ] **Step 5: Mutations:**

  | Mutation | Must fail |
  |---|---|
  | (a) decide at execution instead of arrival (`msc_is_after(eff, clock_at_execution)`) | `async_future_target_fence_deferred_past_target_stays_synced`, `async_synced_variant_acquire_deferred_stays_synced` |
  | (b) ignore the flag in `execute_present_pixmap_copy` | `async_next_target_present_is_synced` |
  | (c) set the flag for every Async request | `async_current_target_keeps_async`, `async_late_after_on_time_keeps_async` |
  | (d) strip Async from `masked_options` itself (completion options change) | a completion-options assertion in `async_next_target_present_is_synced` |

- [ ] **Step 6: Commit** `fix(present): an Async present with a future target flips in sync, as Xorg's sync_flip (#23)`.

### Task 2: Hardware check and docs (coordinator)

The coordinator does this task, not codex.

- [ ] **Gate:**
  - workspace `cargo test --features xdmcp`;
  - the lavapipe `--ignored` suite;
  - `just man`;
  - check-layout;
  - rule F, five runs under load for `present`.
- [ ] **AMD, after asking the user.** Install the branch build as the tty1 session's yserver per the user's procedure, then run `~/Games/stk-tests/stk-normal.sh` against the base build and the branch build. For the on-time adaptive presents, from the run's `yserver.log`:
  - throughput near 60/s;
  - completion msc equal to target;
  - no repeated target;
  - no `kind=async` commit for future-target presents.

  Late frames may still be async. The user also gives his judgment of the lag.
- [ ] **Docs:** add a `docs/status.md` entry. Draft the reply to craftfever with a request to test the branch on the HD 4000; the user posts it.
- [ ] **Commit** `docs: async present future target status (#23)`.
