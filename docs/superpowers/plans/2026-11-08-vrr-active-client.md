# VRR only while the client is drawing — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A direct-scanout owner gets VRR only after it qualifies at 30 fps or more (three consecutive intervals), keeps it at any rate while it keeps presenting, and loses it after 100 ms without a Present, which also releases the cursor hold at once.

**Architecture:** One activity record per direct-scanout scope (the existing `ScanoutM2State` slot), fed at the backend's direct execution point through the paired clock, read by the existing *wanted* predicate. The idle boundary is one more `Option<Instant>` in the existing wakeup minimum; turning off reuses the existing VRR clear (pending CRTC property, standalone VRR-only commit, wait/retry fields).

**Tech Stack:** Rust; `yserver` KMS backend (`kms/render/backend/kms/{present,scanout,state}.rs`, `kms/render/platform/{cursor,gamma}.rs`); backend tests in `kms/render/backend/tests/commit_identity/` (lavapipe).

**Spec:** `docs/superpowers/specs/2026-11-07-vrr-active-client-design.md` (revision 2.1, approved by the user 2026-11-08; revision 4.1 with the application exclusions, approved 2026-11-08, for Tasks 4–6). Base: `docs/superpowers/specs/2026-10-23-vrr-design.md`.

## Global Constraints

- Thresholds, verbatim from the spec: entry = three consecutive intervals of at most **33 333 333 ns** (1000/30 ms, inclusive); idle at **100 ms** (`now >= last + 100 ms`) without a Present. The cursor deadline `VRR_CURSOR_DEADLINE` (33 ms, `kms/render/platform/cursor.rs:12`) is unchanged and is not reused for entry.
- Observation point: `backend_present_try_present_direct` (`kms/render/backend/kms/present.rs`), timestamped through the paired clock provider (`MonotonicClockSample`, `kms/render/backend/kms/state.rs`), never a direct `Instant::now()`.
- With VRR off on the command line, or on an output that is not capable (missing or zero `vrr_capable`, no `VRR_ENABLED`, latched off), no idle deadline is reported, nothing new is logged, and commits are exactly as today.
- No new timer, no new commit kind, no new clear mechanism: reuse `backend_scanout_next_wakeup`, `vrr_clear_retry_deadline`, `vrr_clear_waiting_for_flip`, `submit_pending_crtc_property_for_output`, the one-feature-class selection.
- Reuse first (binding, from the user): new code only where the goal is impossible otherwise, each piece justified in the report.
- Tests go through the Backend/Present request paths with the controlled paired clock installed before the first Present; no sleeps; each test names in a comment the mutation that makes it fail. Scheduled-MSC tests use the full-core request-driving pattern (`kms/render/backend/tests/tearing/round2.rs` ~690), because the backend helper alone does not run the core's parked-request pass.
- No new `YSERVER_*` knobs. The existing CI test environment (`YSERVER_ALLOW_SOFTWARE_VULKAN=1` with a lavapipe that has `VK_KHR_external_semaphore_fd` and `VK_EXT_image_drm_format_modifier`, `.github/workflows/ci.yml` ~160) is the test environment; locally it is `lvp-env.sh` as given in the dispatch. The new tests must execute, not return early from an unsupported recording fixture: the report lists them as run. No copyleft code. Test files stay under 6000 lines (`tools/check-layout.sh`).

## Review Focus

1. A client that stops presenting while a flip is pending and an async successor is queued must still get VRR cleared, with no further Present.
2. A steady 30+ fps client (vsync, async, scheduled-MSC batches, latest-wins replacements) must never see VRR toggle.
3. A Present after a gap of 100 ms or more must never keep or restore VRR by itself.
4. With VRR off or an incapable output, nothing observable changes (commits, wakeups, logs).
5. Two outputs with independent owners: one going idle never touches the other's CRTC or cadence.

---

### Task 1: Activity record, *wanted*, idle wakeup and clear (one functional commit)

The policy, its idle wakeup and its clear land together: enabling the activity condition without the clear service
could leave `VRR_ENABLED` = 1 on an owner that stopped presenting (plan review B1).

**Files:**
- Modify: `crates/yserver/src/kms/render/backend/kms/state.rs` (activity on `ScanoutM2State`, reset with ownership), `kms/present.rs` (record at the execution point through the paired clock), `kms/scanout.rs` (`direct_frame_wants_vrr` gains the activity condition; episode reset on teardown/window change; `backend_scanout_next_wakeup` minimum; expiry serviced in the scheduling pass before the property-retry loop and before the held-direct early returns; the property-change cancel uses full *wanted*; `client_idle` reason through `log_vrr_committed_changes` and the delayed routes). `kms/render/platform/gamma.rs` already handles EBUSY (event wait) and other transient errors (retry deadline); change it only if the obligation cannot reuse it, with the reason in the report.
- Test: a new `crates/yserver/src/kms/render/backend/tests/commit_identity/vrr_active.rs`, registered as a child of `vrr.rs` like `vrr_per_output.rs` (`vrr.rs` is 5911 of 6000 lines: it may only gain the module line and minimal visibility changes for shared helpers; do not duplicate the fixture stack). Migrate the existing tests that expect one Present to enable VRR so each owner qualifies first, with the controlled clock installed before qualification: the cursor harness in `vrr.rs` (~3372) and `vrr_per_output.rs` (~189, ~266, ~309, ~387), plus any other the suite reveals.

**Interfaces:**
- Produces: per-scope activity — the episode's window, the time of its last counted Present, and a qualifying-interval count saturating at 3 (3 = active); one query "is this scope's episode active at `now`" that applies expiry first (`now >= last + 100 ms` → count 0) and is the only thing *wanted* reads; the derived idle boundary `last + 100 ms`.

What must hold (spec: Decisions; Design / State, Expiry first, *wanted*, Idle deadline, Clear obligation, No change without VRR; Logging):
- Counting happens only at the observation point, after expiry is applied; a Present after a gap ≥ 100 ms starts a fresh sequence. Queued-successor dispatch is not a Present.
- An interval > 33 333 333 ns resets the count while not active; while active only the 100 ms gap matters.
- A Present for another window on the scope, or the same window after ownership teardown, starts a new episode.
- Every *wanted* caller (immediate submit, queued dispatch, `live_direct_vrr_wanted_by_output`, cursor hold, property-change clear) gets the same answer for the same time; frames carry no activity.
- While a capable output's episode is active, `next_wakeup` includes its idle boundary, separate from cursor and gamma deadlines; renewing activity never postpones those.
- On expiry with committed 1, or with pending/queued work that may set 1, the output owes a clear until a clear is accepted, *wanted* is true again, or the existing terminal handling ends it — through the existing fields: no pending work → standalone VRR-only commit with the live frame's FB at once; flip in flight → existing event retry; queued synchronous successor may carry the clear; queued async successor cannot (obligation stays, serviced after it retires); gamma pending → clear first; EBUSY waits for the flip event; other errors except EINVAL retry through the retry deadline.
- A `_VARIABLE_REFRESH` notification cancels the retry fields only when full *wanted* is true.
- The cursor hold ends at expiry; a position pending before expiry goes out at expiry.
- An accepted idle clear logs `reason=client_idle`, also via the event retry or a queued synchronous frame; expiry is one debug line; activation one debug line (scope, window, last interval).
- VRR off on the command line or an incapable output: nothing recorded, no idle deadline, no new log line.

- [ ] **Step 1: Failing tests**, all 19 of the spec's Testing list with names, assertions and mutations verbatim: `vrr_static_window_never_enables`, `vrr_slow_intervals_never_enable`, `vrr_enables_after_three_fast_intervals`, `vrr_entry_interval_boundary`, `vrr_slow_interval_breaks_entry_streak`, `vrr_active_owner_keeps_vrr_below_entry_rate`, `vrr_active_presents_renew_without_commits`, `vrr_idle_wakeup_and_clear`, `vrr_late_present_after_gap_requalifies`, `vrr_idle_releases_cursor_hold`, `vrr_queued_frame_cannot_reenable_after_idle`, `vrr_continuous_with_replaced_successors_stays_active`, `vrr_scheduled_msc_batch_stays_active` (full-core request driving), `vrr_tearing_owner_counts_async_presents` (observed through live *wanted* and the idle wakeup, since async commits carry no CRTC property), `vrr_new_window_starts_new_episode`, `vrr_same_window_reentry_starts_new_episode`, `vrr_idle_is_per_output`, `vrr_idle_clear_matrix` (all seven rows), `vrr_off_or_incapable_reports_nothing_new`.
- [ ] **Step 2:** Run them; they fail on today's code (one Present enables VRR; no idle wakeup).
- [ ] **Step 3:** Implement, replacing the two direct clock reads at the start of `backend_present_try_present_direct` with the paired provider's sample.
- [ ] **Step 4:** Migrate the existing VRR tests named under Files; all VRR, tearing, gamma, cursor and direct-scanout backend tests pass; run each named mutation.
- [ ] **Step 5:** Gate (Verification). Commit.

### Task 2: Docs

**Files:**
- Modify: `docs/status.md` (one new entry at the top; one sentence in the 2026-10-25 VRR entry pointing to it), `docs/high-level-design.md` VRR section if it states when VRR turns on, `docs/man/yserver.1.scd` `--vrr` text if it states that a fullscreen window alone enables VRR.

- [ ] **Step 1:** Write the status entry: the rule (30 fps entry, 100 ms idle, cursor released at idle), the reason (Steam screenshot viewer at the 48 Hz floor with a 30 Hz cursor), that application lists are unchanged.
- [ ] **Step 2:** `just man` passes. Commit.

### Task 3: Hardware validation (coordinator; ask the user first)

Per the spec's Hardware list, on NVIDIA DP-3 and AMD DP-1: Steam screenshot viewer (OSD back to 240 Hz when static, smooth cursor), mpv 100 fps (VRR on) and 25 fps (VRR stays off), a game (no `client_idle` while playing; pause menu off, resume on), Discord fullscreen video (observe; input for the option-B decision). Record logs under `~/Projects/claude-scratch/vrr/active/hw/`.

### Task 4: yserver's own application exclusions (spec revision 4.1, section *Application exclusions*)

Tasks 1–3 are done (Task 1 `6fb2cad7`, Task 2 `76a41331`, hardware run 1 recorded in the spec). This task implements
option B; the spec section *Application exclusions* is binding, including its Design bullets and its revision-4
tests.

**Files:**
- Modify: `crates/yserver/src/vrr_exclusions.rs` (the four names, a `VrrExclusionSource` variant logged as `yserver`;
  the evaluator works with the NVIDIA/Mesa list data absent), `kms/render/backend/kms/scanout.rs`
  (`vrr_client_excluded`, the log's source name), `kms/render/backend/kms/scanout/active_vrr.rs` (the Property arms of
  the activity gate and of `direct_frame_wants_vrr_at`), `kms/render/backend/kms/session.rs` and `state.rs` only if
  the evaluator's ownership needs it.
- Test: a new `crates/yserver/src/kms/render/backend/tests/commit_identity/vrr_exclusions.rs`, a child of `vrr.rs` like
  `vrr_active.rs` (`vrr.rs` is 5642 lines: it gains only the module line); unit rows in `vrr_exclusions.rs`'s own test
  module where the spec's case is about the evaluator alone.

**Interfaces:**
- Consumes: `read_process_name`, `read_environment` and the environment rule in `VrrExclusions::evaluate`; the
  per-client `vrr_exclusion_cache` and `vrr_proc_root`; `direct_scope_vrr_capable`; `Output::vrr_capable()`.
- Produces: one evaluator answering "is this pid excluded, and by which source" in both modes, with optional list
  data; `vrr_client_excluded` callable in Property mode; source name `yserver` in
  `kms: vrr excluded client=… pid=… process=… source=yserver`.

What must hold (spec *Application exclusions* / Decision and Design):
- Exact, case-sensitive names: `steamwebhelper`, `Discord`, `DiscordPTB`, `DiscordCanary`. The process-name reader is
  unchanged.
- Environment first, unchanged: either variable false → excluded (`environment`); else either true → no list applies.
- Effective `All` (NVIDIA): environment, then NVIDIA/Mesa lists and the built-in list. Property mode (including
  `--vrr all` falling back on another driver): environment, then the built-in list only; the NVIDIA/Mesa data are
  neither loaded nor consulted there (the existing tests at `tests/commit_identity/vrr.rs` ~5215 and ~5529, which
  expect the list object to stay absent outside effective `All`, keep passing).
- In Property mode the lookup runs only after the mode and output-capability checks, in both the activity gate and
  *wanted*; the existing `All` ordering is unchanged.
- A client without a pid is not excluded; the decision is cached per client and dropped on disconnect, as today.
- With VRR off or an incapable output: no process read, no log, commits as today.

- [ ] **Step 1: Failing tests**, the spec's revision-4 list with names, rows, assertions and mutations verbatim:
  `vrr_builtin_exclusion_steamwebhelper_property_mode`, `vrr_builtin_exclusion_discord_all_mode`,
  `vrr_builtin_exclusion_environment_opt_in`, `vrr_builtin_exclusion_exact_name`, `vrr_builtin_exclusion_no_pid_allowed`,
  `vrr_builtin_exclusion_off_or_incapable_reads_nothing` (process reads observed through a counter or hook; reuse an
  existing seam if one exists, otherwise the smallest test-only seam, justified in the report). Each owner presents
  fast enough to qualify under the activity rule, with the controlled clock installed first.
- [ ] **Step 2:** Run them. The rows that need the new exclusion (the four names in both modes, the environment
  conflict row) fail on today's code; preservation rows (`steam`, `steamwebhelper2`, no pid, off/incapable, opt-in
  enables) may already pass and must not be altered to go red. Every named mutation is run after Step 3 and must fail
  its test.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** All VRR, exclusion, tearing, gamma, cursor and direct-scanout backend tests pass, the existing
  exclusion tests unchanged; run each named mutation.
- [ ] **Step 5:** Gate (Verification). Commit.

### Task 5: Docs for the exclusions

**Files:** `docs/status.md` (amend the 2026-11-08 entry: application lists are no longer unchanged; the four names,
both modes, the environment opt-in), `docs/setup.md` (VRR section), `docs/man/yserver.1.scd` (`--vrr` text),
`docs/high-level-design.md` VRR paragraph (one sentence).

- [ ] **Step 1:** Write the text: which applications never get VRR and why (bursty UI and video), how to opt back in
  (`adaptive_sync=true` or `__GL_VRR_ALLOWED=1` in the application's environment), and that games launched from Steam
  are not affected.
- [ ] **Step 2:** `just man` passes. Commit.

### Task 6: Hardware validation (coordinator; ask the user first)

Spec *Hardware*, the lines after option B, on NVIDIA DP-3 (`--vrr all`) and AMD DP-1 (`--vrr`), recording each
excluded application's `source=yserver` line next to the OSD result. Logs under
`~/Projects/claude-scratch/vrr/active/hw/`.

## Verification (each task, then the whole branch)

`cargo +nightly fmt --all`; `cargo build --bin yserver`; `cargo test --workspace --locked --features xdmcp` and the whole ignored suite `-- --ignored` in the CI lavapipe environment (Global Constraints), with the new tests reported as executed; `cargo test -p yserver --test render_acceptance -- --ignored`; the VRR/tearing/direct backend modules 5× under `nproc` busy loops; `cargo clippy --all-targets -- -D warnings` and `--workspace --all-targets --locked --features xdmcp`; FreeBSD clippy; `tools/check-layout.sh`; `just man`.
