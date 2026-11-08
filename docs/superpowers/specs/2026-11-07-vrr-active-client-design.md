# VRR only while the client is drawing — design

**Date:** 2026-11-07 · **Status:** draft for review · **Base:** `docs/superpowers/specs/2026-10-23-vrr-design.md`
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

- **Thresholds (user, 2026-11-07):** entry at 30 fps, measured as 1000/30 ms per interval (the same constant as the
  cursor deadline); idle after 100 ms (three missed frames at 30 fps). The user raised the entry floor from 20 fps,
  which was the first proposal.
- **Entry counts intervals, not frames:** three consecutive intervals of at most 1000/30 ms each. An interval above
  that resets the count while the owner is not yet active; while it is active only the 100 ms gap matters.
- **What is measured:** the time Presents reach the server for the owning window (the arrival time a direct frame
  already records), not flip completions. A Present that is skipped or replaced before it flips still counts as
  the client drawing.
- **Whose cadence:** per direct-scanout owner window. A new owner (another window, or the same window entering direct
  scanout again) starts inactive with an empty count.
- **The rule applies in both modes** (`--vrr` and `--vrr all`) and on every driver: the stall happens wherever VRR
  is on.
- **Turning off reuses the base design's paths.** Inactive makes *wanted* false. The clear rides a frame if one is
  due, or goes out as the standalone VRR clear with the live direct frame's primary framebuffer (base spec revision
  2.1) — on NVIDIA this is also the plane flip that `VRR_ENABLED` = 0 needs. Turning on rides the next direct frame
  after the owner becomes active, as today.

## Design

- **State:** per direct-scanout owner, the arrival time of its last Present, the count of consecutive qualifying
  intervals, and whether it is active. It lives with the direct-scanout owner record, so it ends when ownership
  ends. Reuse the arrival time the direct frame already carries; no second clock.
- ***wanted*:** today's predicate (`direct_frame_wants_vrr` in `kms/render/backend/kms/scanout.rs`) gains one
  condition: the owner is active. Every caller of *wanted* (frame submit, `live_direct_vrr_wanted_by_output`, the
  cursor hold, the standalone clear decision) sees the same answer.
- **Idle deadline:** while an owner is active, its last Present time + 100 ms is a wakeup reported through
  `backend_scanout_next_wakeup`. When it passes without a Present, the owner becomes inactive and the pass that
  handles the standalone VRR clear (today run on a `_VARIABLE_REFRESH` change) runs for that output. Prefer one
  deadline shape shared with the existing cursor and gamma deadlines over a new copy; the builder justifies the
  choice.
- **No change without VRR:** with VRR off on the command line, or on an output that is not capable, no state is
  updated and no deadline is reported.

## Logging

- `kms: vrr off ... reason=client_idle` when an owner stops being active.
- `kms: vrr on` keeps its line.
- The per-second summary keeps its fields.
- One debug line when an owner becomes active, with the three interval values.

## Testing

Backend tests through the Present/direct-scanout paths with a controlled monotonic clock. Each test checks the commits
the fake device received, and names the mutation that fails it.

- `vrr_static_window_never_enables`: one Present, then 200 ms without one: no commit carries `VRR_ENABLED` = 1.
  Mutation: drop the active condition from *wanted*.
- `vrr_slow_bursts_never_enable`: Presents at 40 ms intervals: no `VRR_ENABLED` = 1. Mutation: entry interval
  1000/20 ms.
- `vrr_enables_after_three_fast_intervals`: four Presents 16 ms apart: the frame of the fourth carries
  `VRR_ENABLED` = 1 and no earlier one does. Mutation: count frames instead of intervals (enables on the third).
- `vrr_idle_clears_without_client_frame`: active owner, then 100 ms without a Present: a standalone commit carries
  `VRR_ENABLED` = 0 with the live frame's framebuffer, and the off line says `client_idle`. Mutation: no idle
  deadline.
- `vrr_idle_releases_cursor_hold`: active owner gone idle, then a cursor move: the position goes out in a
  `cursor-only` commit at once, not at 33 ms. Mutation: the hold ignores the active condition.
- `vrr_active_owner_keeps_vrr_below_entry_rate`: active owner, then Presents at 40 ms: no clear. Mutation: apply the
  entry interval while active.
- `vrr_reentry_needs_three_new_intervals`: idle owner presents again at 16 ms: VRR comes back on the fourth new
  Present, not the first. Mutation: keep the count across idle.
- `vrr_off_mode_tracks_nothing`: `--vrr` absent: Presents at 16 ms report no idle deadline and commits match today.
  Mutation: track and report the deadline in every mode.

**Hardware (ask the user first; NVIDIA DP-3 and AMD DP-1):**
- Steam screenshot viewer: OSD stays at 240 Hz once the image is static; the cursor is smooth.
- mpv at 100 fps and at 25 fps: VRR on at 100 fps; at 25 fps it stays off, because entry needs 30 fps.
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
