# Async Present Future Target Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A Present with `PresentOptionAsync` whose target is in the future when it arrives is handed to scanout as a synced request, as Xorg's `sync_flip` (#23).

**Architecture:** The decision is taken where both PresentPixmap variants build their `PendingPresentPixmap`, with the general-clock msc that computes the effective target. It is stored on the pending request, survives every deferral, and is applied by `execute_present_pixmap_copy` to the `PresentScanoutCandidate` options only. Execution timing, parking, wakeups, completion stamping and the KMS backend are unchanged; Task 2 proves on the KMS backend that a candidate without Async produces a synced commit even while tearing is admitted, with VRR unchanged.

**Tech Stack:** Rust (yserver-core, yserver KMS backend tests).

**Spec:** `docs/superpowers/specs/2026-11-21-async-present-target-design.md` (revision 3, approved by the user 2026-11-21).

## Global Constraints

- Base: branch `fix/async-present-target` on master `3ef6570b` plus docs commits.
- Cite Xorg as `../xserver` (tag xorg-server-21.1.24): `present/present_vblank.c:110-122`, `present/present_scmd.c:232,579,770-771`; Mesa `loader_dri3_helper.c:1118-1159`, `wsi_common_x11.c:1827,2632`. No copyleft code.
- Reuse first: no new scheduling, parking, wake or completion path; `classify_msc_due` unchanged; no KMS backend production change is expected (it already maps a candidate without Async to Vsync timing and exits tearing, `kms/present.rs` ~340-400, ~1558; `kms/scanout.rs` ~1812). If Task 2 finds otherwise, stop and report.
- No new `YSERVER_*` environment variables.
- Out of scope (spec Non-goals): composed-scene async rejection, the completion sweep's stamping policy, synced timing, supersession policy, VRR design, idle/blackout ladder.
- Tests (rules A/B/C): requests through `process_request` dispatch; core-loop drivers that already exist (`run_core_present_wait_drain`, `run_core_present_request_and_wait`); KMS through the `Backend` trait entries (`note_present_scanout_candidate`, `try_present_direct`, retirement) of the recording-scene fixture. No hand-built pending entries; no direct calls to `execute_present_pixmap_copy`; `_VARIABLE_REFRESH` set by ChangeProperty dispatch.
- End-state check for every core scenario: `present_pending_exec`, `present_pending_complete` and `present_complete_gate` empty (or the expected live set), backend-held completions drained, no pins left.
- Red runs: behavioural assertions (candidate options, completion options, commit kind) are written and seen failing on the base **before** the new field exists; assertions on `future_target_sync` are added after Step 3 (they cannot compile on the base). GUARDs pass on the base. Every production change has a mutation listed with the tests it must turn red.
- Commits: signed (`-S`), explicit paths, trailers `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>` and `Implemented-By: codex (gpt-6-luna xhigh)`.
- Implementers run no hardware and no yserver binary. The completion-derived target sequence (Mesa adaptive / Vulkan FIFO_RELAXED) needs real retirements; it is validated on hardware in Task 3, never by injecting completion samples.
- Per-task gate: `cargo +nightly fmt`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo test -p yserver-core present`; `cargo test -p yserver kms::render::backend::tests::present_scanout` plus `-- --ignored` for the recording-scene tests with the lavapipe environment of the repo's CI; `tools/check-layout.sh`; rule F: `cargo test -p yserver-core present` five times under `nproc` busy loops.

## Review Focus

1. A late Async request right after an on-time one: the late one (target ≤ msc at arrival) keeps Async, the on-time one is synced. Task 1 (`async_late_after_on_time_keeps_async`).
2. Same-target supersession keeps today's Skip behaviour and the successor is synced. Task 1.
3. PresentPixmapSynced deferred by its acquire point past the target stays synced. Task 1.
4. VRR: a synced future-target candidate on a VRR window still gets VRR, with the same submission timing. Task 2.
5. Client disconnect with a parked Async future-target entry: teardown unchanged. Task 1.

---

### Task 1: Arrival-time sync decision (yserver-core)

Spec Design rules 1-2.

**Files:**
- Modify: `crates/yserver-core/src/server.rs` `PendingPresentPixmap` (~3017): `pub future_target_sync: bool`.
- Modify: `crates/yserver-core/src/core_loop/process_request/present_ext.rs`: arrival constructors (~2151 PresentPixmap, ~2596 PresentPixmapSynced) compute it from `domain.raw_msc` and the computed `effective_target_msc`; `execute_present_pixmap_copy` (~948 destructuring — bind the field; ~983 candidate) applies it.
- Modify the other initializers: nine full initializers in total besides the two arrival sites — `process_disconnect.rs` ~913 (test code), `run/tests.rs` ~4648, `tests/present_pixmap.rs` ~2622 and ~2751, `tests/mod.rs` ~903, ~965, ~1136 — pass `false` (their inputs are non-Async); the two struct-update constructions in `tests/present_supersede.rs` ~900, ~927 inherit through `..pred`.
- Test: `crates/yserver-core/src/core_loop/process_request/tests/present_pixmap.rs`.

**Interfaces:**
- Produces: `PendingPresentPixmap::future_target_sync: bool` — true exactly when `masked_options & PRESENT_OPTION_ASYNC != 0` and the effective target is `Some(eff)` with `msc_is_after(eff, domain.raw_msc)` at arrival.
- `execute_present_pixmap_copy`: candidate `options = masked_options & !PRESENT_OPTION_ASYNC` when the flag is set, else `masked_options`. `CompletedPresentEvent.options` and every other use of `masked_options` unchanged.

- [ ] **Step 1: Write the behavioural tests** (base-compilable; candidates from `RecordingBackend::present_direct_candidates`, completion options from the recorded Copy-path completion event where stated):
  - `async_next_target_present_is_synced`: Async PresentPixmap, target = clock + 1, source ready, no flip in flight → executes at arrival; candidate options lack Async; the Copy-path completion event (direct result disabled so the copy path runs and records its completion) keeps Async in its options. End state.
  - `async_current_target_keeps_async` (GUARD): target = clock → candidate keeps Async.
  - `async_late_after_on_time_keeps_async`: two explicit arrivals: Async at clock + 1, then (clock advanced through the recording backend's per-CRTC clock) Async with target ≤ clock → first candidate synced, second keeps Async.
  - `async_future_target_fence_deferred_past_target_stays_synced`: schedule: dispatch Async target = clock + 1 with a producer wait (existing deferred-source path) → assert the pending entry exists and nothing executed → set the backend general clock past the target → publish the ready wait id → run the core drain → candidate lacks Async.
  - `async_synced_variant_acquire_deferred_stays_synced`: same schedule through PresentPixmapSynced with a seeded acquire syncobj.
  - `async_distant_target_idle_fallback_is_synced`: Async target = clock + 20, absolute-vblank arming unsupported, idle display → the `present_ext.rs` ~867 fallback executes it now → candidate lacks Async (completion stamp not asserted — spec exemption).
  - `async_distant_target_failed_arm_is_synced`: Async target = clock + 20, arming supported, `arm_present_shared_wake_result = Some(Ok(false))` and, in a second sub-case, `Some(Err(..))` (as the existing tests at ~4846 do), display not idle so the idle fallback does not take it first → assert the arm was attempted and the `run.rs` ~3373 branch executed it → candidate lacks Async.
  - `synced_present_unchanged` (GUARD): non-Async at clock + 1 → candidate options == masked options.
  - `async_same_target_supersession_unchanged`: schedule: flip in flight (recording backend control) so the first Async target = clock + 1 request classifies `Park`; dispatch the second Async request with the same target; expected (today's behaviour): the first's Skip is **queued** before the successor executes (supersession queues it, `present_ext.rs` ~273; CompleteNotify is sent by the later sweep, after due execution, `run.rs` ~3395/~3575) — assert the queue state and, separately, the wire order as it is today; after the flip retires the second executes → its candidate lacks Async.
  - `async_future_target_pending_survives_disconnect` (GUARD): dispatch creates a parked Async future-target entry with a pin (assert it), then client disconnect through the production path → entry and pin gone.
- [ ] **Step 2: Run** `cargo test -p yserver-core present`. Expected: every non-GUARD test FAILs on its Async assertion; GUARDs PASS.
- [ ] **Step 3: Implement** as in Files/Interfaces.
- [ ] **Step 4: Add the field assertions** only where a pending entry genuinely exists (the deferred cases: fence/acquire-deferred → `future_target_sync == true` right after dispatch; and one dispatch-created producer-wait request with target = clock (current) and one non-Async request → `false`). Immediate scenarios keep only their behavioural assertions. Run the task's tests and the gate. Expected: PASS.
- [ ] **Step 5: Mutations** (each alone, by line, reverted):

  | Mutation | Must fail |
  |---|---|
  | (a) decide at execution (`msc_is_after(eff, clock_at_execution)`) | `..._fence_deferred_past_target_...`, `..._acquire_deferred_...` |
  | (b) ignore the flag in `execute_present_pixmap_copy` | `async_next_target_present_is_synced` |
  | (c) set the flag for every Async request | `async_current_target_keeps_async`, `async_late_after_on_time_keeps_async` |
  | (d) strip Async from `masked_options` itself | `async_next_target_present_is_synced` (Copy-path completion options) |

- [ ] **Step 6: Commit** `fix(present): an Async present with a future target flips in sync, as Xorg's sync_flip (#23)`.

### Task 2: KMS backend evidence — synced commit, tearing exit, VRR unchanged

Spec Design rules 3-4 and the composed-path case. No production change expected.

**Files:**
- Test: `crates/yserver/src/kms/render/backend/tests/present_scanout_a3.rs` for the tearing case (reuse `a3_output_fixture`, `a3_offer_output_candidate_after_probation`, `a3_offer_direct_present`, `a3_records_since`, `a3_retire_records`); `crates/yserver/src/kms/render/backend/tests/commit_identity/vrr.rs` for the VRR and composed cases, reusing its request-state, ChangeProperty and PresentPixmap dispatch setup (~908) so the candidate is built by production (`present_ext.rs` ~976) and Copy/completion happen in the core caller (`present_ext.rs` ~1083). All `#[ignore = "needs live Vulkan ICD (lavapipe)"]` like their neighbours.

**Interfaces:** Consumes Task 1's rule: a future-target Async request reaches the backend as a candidate without Async.

- [ ] **Step 1: Write the tests** (device `async_page_flip = true` as in `unpaced_output_client_enters_direct_on_first_retirement`):
  - `async_candidates_admit_tearing_then_synced_candidate_commits_vsync`: admit tearing through probation with Async candidates (assert the recorded direct commit carries the async flag); then offer the same window's next candidate **without** Async → the recorded commit has no async flag, tearing state is Off, and its retirement stamps the completion with the retirement msc. GUARD on the base (the backend already behaves so) — it pins the backend half of the fix.
  - `future_target_async_present_on_vrr_window_keeps_vrr` (after Task 1, so it exercises the fix end to end): `_VARIABLE_REFRESH` set by ChangeProperty dispatch, then Async PresentPixmap requests dispatched with target = clock + 1 → each direct commit is submitted when ready (no added wait), carries no async flag, VRR permission (`direct_frame_wants_vrr`, `scanout.rs` ~1445, which does not read Async) and committed VRR state are unchanged, and the VRR property is emitted only when due (first enable on a synced commit; not repeated once committed, `modeset.rs` ~446). The existing async-property deferral (`vrr.rs` ~2619: an async commit carries no VRR property; the synced successor carries the pending enable) stays green.
  - `future_target_async_present_not_direct_eligible_composes_vsync` (after Task 1): an overlapped window, Async PresentPixmap dispatched with target = clock + 1 → the core Copy path runs (its completion is enqueued), and the composed scene commit that carries that copy has no async flag; correlate the commit to this Present via the copied region/serial, not just any scene commit on the CRTC.
- [ ] **Step 2: Run** them with `--ignored` under the lavapipe environment on top of Task 1. Expected: PASS. The tearing test is a GUARD of the backend contract (it also passes on the base); the VRR and composed tests fail on the base on their "no async flag" assertion (run them there once and record it). If any other assertion fails, stop and report — the spec's "no backend change" assumption would be wrong.
- [ ] **Step 3: Mutations:** (a) remove the non-Async tearing-exit condition in `note_tearing_present` (`kms/present.rs` ~339-345) → `async_candidates_admit_tearing_...` fails on `TearingState::Off`; (b) revert Task 1's candidate masking → the VRR and composed tests fail on the async flag.
- [ ] **Step 4: Commit** `test(present): KMS commits a synced flip for a candidate without Async, VRR unchanged (#23)`.

### Task 3: Hardware check and docs (coordinator)

- [ ] Gate: workspace `cargo test --features xdmcp`, lavapipe `--ignored`, `just man`, check-layout, rule F ×5 under load for `present`.
- [ ] AMD (ask the user first): install the branch build as the tty1 session's yserver per the user's procedure; run `~/Games/stk-tests/stk-normal.sh` on the base and on the branch build. From each run's `yserver.log`, for the on-time adaptive presents (target = arrival msc + 1): throughput near 60/s; completion msc = target; no client present targets the same msc as its predecessor while on time; no `kind=async` commit for them. Late presents may stay async. The user's judgment of the lag.
- [ ] `docs/status.md` entry; draft the craftfever reply asking him to test the branch on the HD 4000 (the user posts it).
- [ ] Commit `docs: async present future target status (#23)`.
