# VRR only while the client is drawing — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A direct-scanout owner gets VRR only while it presents at 30 fps or more; 100 ms without a Present turns VRR off and releases the cursor hold at once.

**Architecture:** One activity record per direct-scanout scope (the existing `ScanoutM2State` slot), fed at the backend's direct execution point through the paired clock, read by the existing *wanted* predicate. The idle boundary is one more `Option<Instant>` in the existing wakeup minimum; turning off reuses the existing VRR clear (pending CRTC property, standalone VRR-only commit, wait/retry fields).

**Tech Stack:** Rust; `yserver` KMS backend (`kms/render/backend/kms/{present,scanout,state}.rs`, `kms/render/platform/{cursor,gamma}.rs`); backend tests in `kms/render/backend/tests/commit_identity/` (lavapipe).

**Spec:** `docs/superpowers/specs/2026-11-07-vrr-active-client-design.md` (revision 2.1, approved by the user 2026-11-08). Base: `docs/superpowers/specs/2026-10-23-vrr-design.md`.

## Global Constraints

- Thresholds, verbatim from the spec: entry = three consecutive intervals of at most **33 333 333 ns** (1000/30 ms, inclusive); idle at **100 ms** (`now >= last + 100 ms`) without a Present. The cursor deadline `VRR_CURSOR_DEADLINE` (33 ms, `kms/render/platform/cursor.rs:12`) is unchanged and is not reused for entry.
- Observation point: `backend_present_try_present_direct` (`kms/render/backend/kms/present.rs`), timestamped through the paired clock provider (`MonotonicClockSample`, `kms/render/backend/kms/state.rs`), never a direct `Instant::now()`.
- With VRR off on the command line, or on an output that is not capable (missing or zero `vrr_capable`, no `VRR_ENABLED`, latched off), no idle deadline is reported, nothing new is logged, and commits are exactly as today.
- No new timer, no new commit kind, no new clear mechanism: reuse `backend_scanout_next_wakeup`, `vrr_clear_retry_deadline`, `vrr_clear_waiting_for_flip`, `submit_pending_crtc_property_for_output`, the one-feature-class selection.
- Reuse first (binding, from the user): new code only where the goal is impossible otherwise, each piece justified in the report.
- Tests go through the Backend/Present request paths with the controlled paired clock installed before the first Present; no sleeps; each test names in a comment the mutation that makes it fail. Scheduled-MSC tests use the full-core request-driving pattern (`kms/render/backend/tests/tearing/round2.rs` ~690), because the backend helper alone does not run the core's parked-request pass.
- No `YSERVER_*` environment variables. No copyleft code. Test files stay under 6000 lines (`tools/check-layout.sh`).

## Review Focus

1. A client that stops presenting while a flip is pending and an async successor is queued must still get VRR cleared, with no further Present.
2. A steady 30+ fps client (vsync, async, scheduled-MSC batches, latest-wins replacements) must never see VRR toggle.
3. A Present after a gap of 100 ms or more must never keep or restore VRR by itself.
4. With VRR off or an incapable output, nothing observable changes (commits, wakeups, logs).
5. Two outputs with independent owners: one going idle never touches the other's CRTC or cadence.

---

### Task 1: Activity record and *wanted*

**Files:**
- Modify: `crates/yserver/src/kms/render/backend/kms/state.rs` (activity fields on `ScanoutM2State`, reset with ownership), `kms/present.rs` (record at the execution point via the paired clock), `kms/scanout.rs` (`direct_frame_wants_vrr` gains the activity condition; episode reset on teardown/window change).
- Test: `crates/yserver/src/kms/render/backend/tests/commit_identity/vrr.rs` (or a new sibling `vrr_active.rs` registered in that `mod.rs` if `vrr.rs` would pass 5000 lines); adapt existing VRR fixtures that assume one Present enables VRR so they qualify first.

**Interfaces:**
- Produces: per-scope activity — the episode's window, the time of its last counted Present, and a qualifying-interval count saturating at 3 (3 = active); a query "is this scope's episode active at time `now`" that applies expiry first (`now >= last + 100 ms` → inactive, count 0) and is the only thing *wanted* reads; a derived idle boundary `last + 100 ms` available to Task 2.

What must hold (spec: Decisions, Design / State, Expiry first, *wanted*):
- Counting happens only at the observation point, after expiry is applied; a Present after a gap ≥ 100 ms starts a fresh sequence. Queued-successor dispatch is not a Present.
- An interval > 33 333 333 ns resets the count while not active; while active only the 100 ms gap matters.
- A Present for another window on the scope, or the same window after ownership teardown, starts a new episode.
- Every *wanted* caller (immediate submit, queued dispatch, `live_direct_vrr_wanted_by_output`, cursor hold, property-change clear) gets the same answer for the same time; frames carry no activity.
- VRR off on the command line: nothing is recorded.

- [ ] **Step 1: Failing tests** (spec Testing, names verbatim): `vrr_static_window_never_enables`, `vrr_slow_intervals_never_enable`, `vrr_enables_after_three_fast_intervals`, `vrr_entry_interval_boundary`, `vrr_slow_interval_breaks_entry_streak`, `vrr_active_owner_keeps_vrr_below_entry_rate`, `vrr_new_window_starts_new_episode`, `vrr_same_window_reentry_starts_new_episode`, `vrr_idle_is_per_output`, `vrr_continuous_with_replaced_successors_stays_active`, `vrr_scheduled_msc_batch_stays_active`, `vrr_tearing_owner_counts_async_presents`. Each with the spec's assertion and named mutation. Tests whose observation needs Task 2's wakeup or clear (idle in `vrr_idle_is_per_output`, "stays active" checks) assert here only what Task 1 delivers (*wanted* and the commits' `VRR_ENABLED` value at the next frame) and are completed in Task 2.
- [ ] **Step 2:** Run them; they fail on today's code (one Present enables VRR).
- [ ] **Step 3:** Implement the activity record, the observation-point recording through the paired clock (replacing the two direct clock reads at the start of `backend_present_try_present_direct` with the provider's sample), the episode reset, and the condition in `direct_frame_wants_vrr`.
- [ ] **Step 4:** All VRR, tearing and direct-scanout backend tests pass, including the adapted fixtures; run each named mutation.
- [ ] **Step 5:** Gate (Verification). Commit.

### Task 2: Idle wakeup, clear obligation, cursor release, logging

**Files:**
- Modify: `kms/render/backend/kms/scanout.rs` (`backend_scanout_next_wakeup` minimum; expiry serviced in the scheduling pass before the property-retry loop and before the held-direct early returns; the property-change cancel uses full *wanted*; `client_idle` reason through `log_vrr_committed_changes` and the delayed routes), `kms/render/platform/gamma.rs` only if the obligation's non-EBUSY transient retry needs it.
- Test: same file as Task 1.

**Interfaces:**
- Consumes: Task 1's activity query and derived idle boundary.

What must hold (spec: Idle deadline, Clear obligation, No change without VRR, Logging):
- While a capable output's episode is active, `next_wakeup` includes its idle boundary, separate from cursor and gamma deadlines; renewing activity never postpones those.
- On expiry with committed 1, or with pending/queued work that may set 1, the output owes a clear until a clear is accepted, *wanted* is true again, or the existing terminal handling ends it — through the existing fields: no pending work → standalone VRR-only commit with the live frame's FB at once; flip in flight → existing event retry; queued synchronous successor may carry the clear; queued async successor cannot (obligation stays, serviced after it retires); gamma pending → clear first; EBUSY waits for the flip event as today; other errors except EINVAL retry through the existing retry deadline.
- A `_VARIABLE_REFRESH` notification cancels the retry fields only when full *wanted* (activity included) is true.
- The cursor hold ends at expiry (it reads *wanted*); a position pending before expiry goes out at expiry.
- An accepted idle clear logs `reason=client_idle`, also via the event retry or a queued synchronous frame; expiry itself is one debug line; activation one debug line (scope, window, last interval).
- No idle deadline or new log with VRR off or an incapable output.

- [ ] **Step 1: Failing tests** (spec Testing, names verbatim): `vrr_active_presents_renew_without_commits`, `vrr_idle_wakeup_and_clear`, `vrr_late_present_after_gap_requalifies`, `vrr_idle_releases_cursor_hold`, `vrr_queued_frame_cannot_reenable_after_idle`, `vrr_idle_clear_matrix` (all seven rows), `vrr_off_or_incapable_reports_nothing_new`; complete the Task 1 tests that need the clear (`vrr_idle_is_per_output`, the three "stays active" tests: no clear while presenting). Each with the spec's assertion and named mutation.
- [ ] **Step 2:** Run them; they fail before the change.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** All VRR, tearing, gamma, cursor and direct-scanout backend tests pass; run each named mutation.
- [ ] **Step 5:** Gate. Commit.

### Task 3: Docs

**Files:**
- Modify: `docs/status.md` (one new entry at the top; one sentence in the 2026-10-25 VRR entry pointing to it), `docs/high-level-design.md` VRR section if it states when VRR turns on, `docs/man/yserver.1.scd` `--vrr` text if it states that a fullscreen window alone enables VRR.

- [ ] **Step 1:** Write the status entry: the rule (30 fps entry, 100 ms idle, cursor released at idle), the reason (Steam screenshot viewer at the 48 Hz floor with a 30 Hz cursor), that application lists are unchanged.
- [ ] **Step 2:** `just man` passes. Commit.

### Task 4: Hardware validation (coordinator; ask the user first)

Per the spec's Hardware list, on NVIDIA DP-3 and AMD DP-1: Steam screenshot viewer (OSD back to 240 Hz when static, smooth cursor), mpv 100 fps (VRR on) and 25 fps (VRR stays off), a game (no `client_idle` while playing; pause menu off, resume on), Discord fullscreen video (observe; input for the option-B decision). Record logs under `~/Projects/claude-scratch/vrr/active/hw/`.

## Verification (each task, then the whole branch)

`cargo +nightly fmt --all`; `cargo build --bin yserver`; `cargo test --workspace --locked --features xdmcp` and the whole ignored suite `-- --ignored` under the lavapipe env; `cargo test -p yserver --test render_acceptance -- --ignored`; the VRR/tearing/direct backend modules 5× under `nproc` busy loops; `cargo clippy --all-targets -- -D warnings` and `--workspace --all-targets --locked --features xdmcp`; FreeBSD clippy; `tools/check-layout.sh`; `just man`.
