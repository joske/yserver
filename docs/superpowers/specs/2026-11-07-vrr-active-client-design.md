# VRR only while the client is drawing — design

**Date:** 2026-11-07 · **Status:** revision 2 (sol review 1: observation point, clear obligation, expiry order, episode identity, tests) · **Base:** `docs/superpowers/specs/2026-10-23-vrr-design.md`
(revision 5.1, implemented by #294 and #298) · **Scope:** KMS backend, the *wanted* decision for a direct-scanout
owner and what follows from it (cursor hold, standalone clear, logging).

## Why

Measured on the user's session (NVIDIA, MSI 48–240 Hz on DP-3, `--vrr all`, 2026-11-07):

- Steam: in the store, clicking a game's screenshot opens a fullscreen viewer (`steamwebhelper`, window type
  `_NET_WM_WINDOW_TYPE_NOTIFICATION`). It enters direct scanout and turns VRR on. It draws in short bursts and then
  stops: the per-second summary shows `flips/s=0` with `cursor_deadline_commits=21..29` for several seconds. The
  monitor drops to its 48 Hz floor (OSD) and the cursor moves at about 30 Hz, because while VRR is on each position
  waits for the client's next frame or the 33 ms deadline (base spec, The cursor while VRR is on). The user sees the
  cursor and the screen stutter.
- Discord: a shared YouTube video in fullscreen also enters direct scanout and gets VRR.

Neither process is in the NVIDIA or Mesa lists (`steam` is listed, `steamwebhelper` is not). The base spec accepted
that a browser or player the lists do not name may get VRR "until a user reports a problem"; this is that report.

The cause is general, not specific to these applications: the base design turns VRR on for any eligible window in
direct scanout, whether it is drawing or not. A client that stops presenting (a static image, a paused video, a game
menu that does not redraw) leaves the panel at its floor and the cursor at the 33 ms cadence. The 33 ms hold exists
so the cursor does not drive the refresh rate while a client does (base anti-goal 4); with no client frames there is
nothing to protect.

## Goals

1. **VRR needs a drawing client.** An eligible direct-scanout owner (as today: `Unredirected`, not excluded, and in
   `--vrr` mode with `_VARIABLE_REFRESH`) wants VRR only while it is *active*:
   - it becomes active after **three consecutive intervals of at most 1000/30 ms** between its Presents (four
     Presents; at least 30 fps);
   - it stops being active after **100 ms** without a Present;
   - after that it must qualify again with three new intervals.
2. **No VRR on a static or bursty window.** A window that presents once, or in bursts slower than 30 fps, never sends
   `VRR_ENABLED` = 1.
3. **The cursor follows the monitor's fixed refresh as soon as the client stops.** When an owner stops being active,
   the cursor hold ends at once (it already requires a live direct frame that wants VRR), and the VRR clear goes out
   without waiting for a client frame.
4. **A game that slows down keeps VRR.** Once active, an owner stays active at any frame rate, as long as no gap
   reaches 100 ms. The 30 fps rule only gates entry. A game that drops to 25 fps keeps VRR; a client that never
   reaches 30 fps never gets it.

## Non-goals

- Application lists (option B in the conversation: adding `steamwebhelper`, Electron, Chromium). Decided after the
  hardware run of this change, only if it is not enough. A video that keeps playing at 30 fps or more still qualifies
  (expected; the user decides on B with that observed).
- Reading the panel's real range (EDID monitor range descriptor). The thresholds are fixed values.
- Changing what makes a window eligible, the exclusions, the cursor deadline, tearing, or the transport of
  `VRR_ENABLED` (pending CRTC property, one feature class per request, EINVAL latch).
- Image batching for animated cursors (base spec's known limitation).

## Anti-goals

1. **No change without VRR.** With `--vrr` absent, or on an output that is not capable, commits and cursor commits are
   exactly as today. Tracking Present intervals must not add commits, wakeups or log lines in that case.
2. **No toggling while a game runs.** A client presenting steadily at 30 fps or more never sees VRR switch off and on.
3. **No extra commits while active.** While the owner is active, VRR adds no commit that the base design does not
   already send.
4. **No new timer.** The 100 ms idle deadline uses the existing `next_wakeup` path, like the cursor deadline and
   gamma's pending deadline.

## Decisions

- **Thresholds (user, 2026-11-07):** entry at 30 fps, as an interval of at most **1000/30 ms, represented as
  33 333 333 ns** (inclusive); idle at **100 ms** without a Present (three missed frames at 30 fps). The user raised
  the entry floor from 20 fps, the first proposal. The cursor deadline stays 33 ms (`VRR_CURSOR_DEADLINE`); the two
  are different constants.
- **Entry counts intervals, not frames:** three consecutive intervals of at most 33 333 333 ns. An interval above that
  resets the count while the owner is not yet active; while it is active only the 100 ms gap matters.
- **What is measured (the observation point):** a Present counts when the core hands it to the backend's direct path
  (`backend_present_try_present_direct`, `kms/render/backend/kms/present.rs`), timestamped through the backend's
  paired clock provider (`kms/render/backend/kms/state.rs`, the one tests already control), not with a direct
  `Instant::now()`. Consequences, all intended:
  - a Present counts whether it is submitted at once, queued, or later replaced in the latest-wins successor slot;
  - a Present with a future target MSC counts when it executes, not when its request arrived, so a client that sends
    a batch of scheduled frames and then waits stays active while the frames execute on schedule;
  - a parked request completed as Skip before it executes does not count; the request that replaced it counts when
    it executes;
  - an async (tearing) Present counts like any other.
- **Whose cadence:** one ownership episode of one direct-scanout scope (Root or an output): the window that owns it,
  from direct entry until ownership teardown completes. A Present for another window on that scope, or the same
  window entering again after a teardown, starts a new episode: inactive, empty count. Frames do not carry activity;
  current, pending and queued frames are judged by their scope's episode at the moment *wanted* is evaluated.
- **The rule applies in both modes** (`--vrr` and `--vrr all`) and on every driver.
- **Turning off reuses the base design's clear** (pending CRTC property, the standalone VRR-only commit with the live
  direct frame's primary framebuffer — on NVIDIA also the plane flip `VRR_ENABLED` = 0 needs —, the existing
  wait/retry fields, clear-before-gamma selection). Turning on rides the next direct frame after the owner becomes
  active, as today.

## Design

- **State:** per scope, with the existing scope slot (`ScanoutM2State`): the episode's window, the time of its last
  counted Present, and a qualifying-interval count that saturates at three (three means active; no separate flag
  needed). The idle deadline is derived (last + 100 ms), not stored. Ownership teardown and a window change reset it.
- **Expiry first.** Every use of the state first applies expiry: at `now >= last + 100 ms` the episode is inactive
  (count 0). This happens before a new Present is counted (a Present after a long gap starts a fresh sequence; it
  cannot erase the gap) and before any *wanted* result is used.
- ***wanted*:** today's predicate (`direct_frame_wants_vrr`, `kms/render/backend/kms/scanout.rs`) gains one
  condition: the scope's episode is active at the current time. Every caller (immediate submit, queued dispatch on
  retirement, `live_direct_vrr_wanted_by_output`, the cursor hold, the property-change clear) sees the same answer for
  the same time. Dispatching a queued successor is not a Present: it neither counts nor renews activity, and its
  *wanted* is recomputed at submission, so a frame queued while active cannot re-enable VRR after an idle clear
  without new qualification. A request the kernel already accepted with `VRR_ENABLED` = 1 cannot be rewritten; the
  clear follows its retirement.
- **Idle deadline:** while a capable output's episode is active, `backend_scanout_next_wakeup` includes last + 100 ms
  in its minimum, as one more `Option<Instant>`, separate from the cursor and gamma deadlines (renewing activity
  never postpones a pending cursor position or gamma update). No new timer.
- **Clear obligation.** When an episode expires on an output whose committed value is 1, or whose pending or queued
  work may set it to 1, the output owes a clear until one of: a clear is accepted; *wanted* becomes true again
  (requalification); or the existing terminal handling (rejection latch, output teardown, device release) ends it.
  The obligation uses the existing wait/retry fields (`vrr_clear_retry_deadline`, `vrr_clear_waiting_for_flip`) and
  the existing assembler:
  - no pending work and a retained current frame: standalone VRR-only commit at once;
  - flip in flight: wait for its event, then the standalone commit (existing event retry);
  - a queued **synchronous** successor that will carry the clear may stand in for the standalone commit; a queued
    **async** successor cannot (async requests carry no CRTC property), so the obligation stays and is serviced after
    it retires;
  - gamma pending: clear first (existing one-class selection), gamma in the next request;
  - transient error (EBUSY, EIO): retry through the existing retry deadline.
  Expiry is serviced in the scheduling pass before the property-retry loop and before the held-direct early returns,
  so neither skips it. A `_VARIABLE_REFRESH` notification may cancel the retry fields only when the full *wanted*
  (including activity) is true.
- **No change without VRR:** with VRR off on the command line, or on an output that is not capable (missing or zero
  `vrr_capable`, no `VRR_ENABLED`, or latched off after a rejection), no idle deadline is reported and nothing new is
  logged; commits are as today.

## Logging

- An accepted clear caused by idle logs `kms: vrr off ... reason=client_idle`, also when it is accepted later through
  the event retry or a queued synchronous frame. Other causes keep their reasons. Only accepted changes log at info.
- Expiry itself logs one debug line (scope, window); it is not an info "off" line, since the hardware may never have
  had VRR on (gamma or async transport can delay an enable past the episode).
- One debug line when an episode becomes active: scope, window and the last interval.
- The per-second summary keeps its fields.

## Testing

Backend tests through the Backend/Present request paths (the recording scene/direct fixtures and request helper in
`kms/render/backend/tests/commit_identity/vrr.rs`, lavapipe), with the paired controlled clock installed before the
first Present and no sleeps. Each test checks the fake device's attempts and accepted events, drains events as the
path needs, and names the mutation that fails it. Existing fixtures that assume one Present enables VRR (the cursor
fixtures) are adapted to qualify first.

Entry and idle:
- `vrr_static_window_never_enables`: an eligible, capable synchronous owner presents once (a direct commit is
  observed), then 200 ms pass: no commit carries `VRR_ENABLED` = 1. Mutation: drop the activity condition.
- `vrr_slow_intervals_never_enable`: four or more Presents 40 ms apart, each observed as a direct commit: none carries
  1. Mutation: entry interval 1000/20 ms.
- `vrr_enables_after_three_fast_intervals`: four Presents 16 ms apart: the fourth commit carries 1, none before.
  Mutation: count frames instead of intervals.
- `vrr_entry_interval_boundary`: intervals of exactly 33 333 333 ns qualify; 33 333 334 ns do not. Mutation: compare
  with the 33 ms cursor constant, or use `<`.
- `vrr_slow_interval_breaks_entry_streak`: 16, 16, 40, 16 ms: not active after the fourth interval; active after
  three consecutive fast ones. Mutation: count non-consecutive intervals.
- `vrr_active_owner_keeps_vrr_below_entry_rate`: active, then several Presents 40 ms apart with scheduling between
  them: no clear. Mutation: apply the entry rule while active.
- `vrr_active_presents_renew_without_commits`: active, Presents every 16 ms for 300 ms: no clear and no commit beyond
  the frames themselves; the reported idle wakeup moves with each Present. Mutation: deadline not renewed.
- `vrr_idle_wakeup_and_clear`: active, last Present at T: `Backend::next_wakeup` reports T + 100 ms once other work
  is drained; at T + 99 ms no clear; serviced at T + 100 ms: accepted VRR-only commit with the live FB_ID and the
  `client_idle` line. Mutation: no idle wakeup; `>` instead of `>=`.
- `vrr_late_present_after_gap_requalifies`: active, then one Present at T + 120 ms handled before the idle wakeup is
  serviced: it does not carry 1 and VRR is cleared; it needs three new fast intervals. Mutation: test only a stored
  active state before counting.
- `vrr_idle_releases_cursor_hold`: active, a scripted transient clear failure keeps committed at 1; a move pending
  since T + 90 ms goes out at T + 100 ms, before its 33 ms deadline at T + 123 ms, with no further motion. Mutation:
  the hold ignores activity.

Ownership and queued frames:
- `vrr_queued_frame_cannot_reenable_after_idle`: a successor queued while active dispatches after an idle clear: it
  does not carry 1. Mutation: cache activity on the frame, or renew activity at dispatch.
- `vrr_continuous_with_replaced_successors_stays_active`: at 60 fps with successors replaced in the latest-wins slot,
  and with skipped parked requests: no clear. Mutation: count only submitted or retired frames.
- `vrr_scheduled_msc_batch_stays_active`: frames sent in batches of distinct future MSCs every 200 ms, executing at
  60 fps: no clear. Mutation: timestamp at request arrival.
- `vrr_tearing_owner_counts_async_presents`: an async-flipping owner presenting at 60 fps stays active. Mutation:
  count only vsync submits.
- `vrr_new_window_starts_new_episode` and `vrr_same_window_reentry_starts_new_episode`: after a window change, or
  teardown and re-entry of the same window, the first new Present does not carry 1. Mutation: keep the state across
  episodes.
- `vrr_idle_is_per_output`: two outputs with independent owners; one goes idle: only its CRTC is cleared. Mutation:
  share cadence across scopes.

The clear obligation (reuse the scripted recorder and event machinery):
- `vrr_idle_clear_matrix`: retained current frame, no pending work; pending flip; queued synchronous successor; queued
  async successor; pending gamma; `_VARIABLE_REFRESH` reasserted while the idle clear waits; transient EBUSY/EIO. In
  each case a clear is eventually accepted without another client Present, gamma goes in a separate later request,
  and the wakeup is not left repeatedly overdue. Mutations: let a queued async successor discharge the obligation;
  cancel retries on property value 1 without activity.

No change without VRR:
- `vrr_off_or_incapable_reports_nothing_new`: `--vrr` absent; capability missing or 0; no `VRR_ENABLED`; latched
  off: Presents fast enough to qualify produce no idle wakeup, no new log line and the same commit signatures as
  today. The claim is about these observable effects only. Mutation: report the deadline in every mode.

**Hardware (ask the user first; NVIDIA DP-3 and AMD DP-1):**
- Steam screenshot viewer: OSD stays at 240 Hz once the image is static; the cursor is smooth.
- mpv at 100 fps: VRR on. mpv at 25 fps: VRR stays off, because entry needs 30 fps.
- A game: VRR on, with no `client_idle` while playing. A pause menu that stops drawing turns it off, and it comes
  back on resume.
- Discord fullscreen video: observe and report. It is expected to get VRR while the video plays at 30 fps or more;
  this is the input for the option-B decision.

## Provenance

- yserver master `0eceeaa3`: the VRR *wanted* predicate, the standalone clear and the cursor hold
  (`kms/render/backend/kms/scanout.rs`, `kms/render/platform/cursor.rs`).
- The yserver log of the user's session, 2026-11-07, for the measurements in Why.
- `/usr/share/nvidia/nvidia-application-profiles-620.70.06-rc`, read for which process names it excludes. Nothing
  is copied.

This document describes behaviour in original prose; no third-party code was used.
