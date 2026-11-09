# VRR cursor hold lifts while the direct client is idle — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The VRR cursor hold applies only while the direct-scanout owner on that output presented within the last 100 ms; VRR itself is unchanged.

**Architecture:** One optional "last accepted direct Present" time per direct scope (the existing `ScanoutM2State` slot), stamped through the backend clock where a Present is accepted as direct, cleared where the slot is cleared. One cursor-only wrapper around `live_direct_vrr_wanted_by_output` drops the hold for idle owners, used by the four cursor callers only.

**Tech Stack:** Rust; `yserver` KMS backend (`kms/render/backend/kms/{present,scanout,state}.rs`); backend tests in `kms/render/backend/tests/commit_identity/` and `tests/cursor_sprites/` (lavapipe).

**Spec:** `docs/superpowers/specs/2026-11-08-vrr-cursor-idle-design.md` (revision 1.1, approved by the user 2026-11-08). Base: `docs/superpowers/specs/2026-10-23-vrr-design.md`.

## Global Constraints

- Idle at `now >= last + 100 ms`; an owner with no recorded Present is idle. `VRR_CURSOR_DEADLINE` (33 ms, `kms/render/platform/cursor.rs:12`) is unchanged.
- Stamp only accepted Presents: each queued-successor branch in `backend_present_try_present_direct` (including replacement of an occupied successor) and a successful immediate submission. Rejection or submission failure leaves the time unchanged. Time from `monotonic_now()` (the paired clock provider), never a direct `Instant::now()`.
- Queued-successor dispatch, retirement, and cursor commits or their retirement never renew the time.
- Clear the time explicitly where the slot's other fields are cleared: `stop_direct_after_scanout_replaced` (~`scanout.rs:2156`) and the queued first-entry fallback (~`scanout.rs:1941`).
- Idle-aware hold only in the cursor callers: `scene_wants_compose` (~`scanout.rs:3941`), `backend_scanout_next_wakeup` (~`:4241`, for both eligibility and the cursor deadline), the held-direct cursor submit (~`:4527`), the composed-output cursor loop (~`:4647`). Unchanged: `retry_pending_crtc_property_after_flip` (~`:1725`), the RandR gamma path (`randr.rs` ~1659), and direct-frame VRR selection.
- No new timer, wakeup, commit kind, `YSERVER_*` knob or config. Reuse first: new code only where the goal is impossible otherwise, each piece justified in the report.
- Tests go through Backend/Present request paths with the controlled paired clock installed before the first Present; no sleeps; each test names the mutation that fails it, and every mutation is run. Report executed vs skipped. Test files stay under 6000 lines (`tools/check-layout.sh`): `vrr.rs` is 5911 lines, so new tests go in a new child module (pattern: `vrr_per_output.rs`), and `vrr.rs` gains only the module line and minimal visibility changes. No copyleft code.

## Review Focus

1. Continuous mouse motion over an idle client must never re-arm the hold (the 30 Hz cursor must not come back).
2. A move held when the 100 ms boundary passes must still go out with no further input or Present.
3. A client presenting steadily must keep exactly today's cursor behaviour.
4. VRR and `VRR_ENABLED` commits must be identical to master in every case.
5. One output's idle client must not lift the hold on another output.

---

### Task 1: Idle-aware cursor hold (one functional commit)

**Files:**
- Modify: `crates/yserver/src/kms/render/backend/kms/state.rs` (the time on `ScanoutM2State`), `kms/present.rs` (stamp on acceptance), `kms/scanout.rs` (resets, the cursor wrapper, the four cursor callers).
- Test: a new `crates/yserver/src/kms/render/backend/tests/commit_identity/vrr_cursor_idle.rs`, a child of `vrr.rs`; fixture changes so the controlled clock is installed before direct entry, with behavioural assertions unchanged: in `vrr.rs`, `vrr_cursor_harness` (~3413) and every caller that installs the clock after it (~3512, ~3568, ~3616, ~3944, ~4012, ~4085, ~4142, ~4335). Install it once before `enter_held_direct`, return its `(base, elapsed_ns)` handle and reuse it, never reinstall later. In `vrr_per_output.rs`, move the install at ~573 before the direct setup at ~523 and keep one handle across both owners. Reuse the existing helper (`tests/commit_identity/support.rs` ~29, setter `backend/for_tests.rs` ~45); no second clock provider. The composed/no-owner installs at ~3846 and ~3899 need no change.

**Interfaces:**
- Produces: an optional last-accepted-Present time per direct scope; a cursor-hold vector per output, equal to live *wanted* except false where the output's owner is idle at `now`.

- [ ] **Step 0: Scaffolding.** Add the optional time field to `ScanoutM2State` (initialised to `None`, nothing else) and move the existing fixtures' clock installation before direct entry. Run the existing VRR and cursor backend tests: they pass unchanged.
- [ ] **Step 1: Failing tests**, the spec's Testing list with names, assertions and mutations verbatim: `vrr_cursor_hold_lifts_after_idle`, `vrr_cursor_hold_applies_while_presenting`, `vrr_cursor_hold_idle_boundary`, `vrr_cursor_hold_returns_on_next_present`, `vrr_cursor_hold_queued_dispatch_does_not_renew`, `vrr_cursor_hold_queued_acceptance_renews`, `vrr_cursor_hold_cursor_commits_do_not_renew`, `vrr_cursor_hold_pending_move_crosses_boundary` (Root and Output owners, real `Backend::next_wakeup`), `vrr_cursor_hold_idle_leaves_vrr_alone` (both VRR callers), `vrr_cursor_hold_idle_is_per_output`, `vrr_cursor_hold_reset_on_teardown`.
- [ ] **Step 2:** Run them and record each row's result. The idle rows fail (the hold never lifts). Preservation rows (`applies_while_presenting`, `leaves_vrr_alone`, the fresh output in `per_output`) may already pass and must not be altered to go red. Rows that need stamping (`reset_on_teardown`, `queued_acceptance_renews`) fail on their `Some(time)` precondition until Step 3.
- [ ] **Step 3:** Implement. The three successful queue branches converge on `queue_direct_successor` (~`scanout.rs:2064`), a possible single stamping point for queue acceptance; successful immediate submission stays separate.
- [ ] **Step 4:** All VRR, cursor, tearing, gamma and direct-scanout backend tests pass; run every named mutation (cursor-submit and cursor-retirement stamping separately; each VRR caller separately).
- [ ] **Step 5:** Task 1 gate (Verification). Commit.

### Task 2: Docs

**Files:** `docs/status.md` (one new entry at the top; one sentence in the 2026-10-25 VRR entry, whose "cursor moves otherwise wait for a direct frame or the 33 ms deadline" now applies only while the client draws).

- [ ] **Step 1:** Write the entry: the rule (hold only while the direct client presented within 100 ms), the reason (cursor at ~30 Hz over a still Steam image or a paused Discord video), the hardware results from the spec attributed to the spike build `87af70d6` (the final build's results come from Task 3), and that VRR is unchanged.
- [ ] **Step 2:** Check the wording against this step in the diff (`just man` does not read `docs/status.md`), `git diff --check`. Commit.

### Task 3: Hardware validation (coordinator; ask the user first)

Spec *Hardware*: NVIDIA DP-3 (`--vrr all`) and AMD DP-1 (`--vrr`) with `YSERVER_LOOP_TELEMETRY=1`. Check a still Steam image, a paused Discord video, a fullscreen mpv paused after playback, and mpv at 100 fps. Record logs under `~/Projects/claude-scratch/vrr/cursor-idle/hw/`.

## Verification

All `cargo test` runs source the lavapipe environment given in the dispatch and report executed vs skipped tests.

- **Task 1 gate:** `cargo +nightly fmt --all`; `cargo build --bin yserver`; `cargo test --workspace --locked --features xdmcp` and the whole ignored suite `--workspace --locked --features xdmcp -- --ignored` (it includes `render_acceptance`); the VRR, cursor and tearing backend modules with `--include-ignored` 5× under 16 busy loops, reaping every loop on success and failure; `cargo clippy --all-targets -- -D warnings` and `--workspace --all-targets --locked --features xdmcp -- -D warnings`; FreeBSD clippy cross-check (local sysroot; the real FreeBSD gate is CI's VM job, reported as pending until it runs); `tools/check-layout.sh`.
- **Task 2 gate:** the wording check and `git diff --check`.
- **Branch gate (coordinator, before the PR):** the Task 1 gate once more on the final HEAD, plus `just man`.
