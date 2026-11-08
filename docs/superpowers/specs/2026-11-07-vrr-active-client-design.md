# VRR only while the client is drawing — design

**Date:** 2026-11-07 · **Status:** revision 4.1 (sol review of revision 4: property-mode route, process-name contract, no-read gating, test matrix); revision 4 (option B after hardware run 1: yserver's own application exclusions, section *Application exclusions*); revision 2.1 (sol re-review: queued-frame test, EBUSY keeps its event wait); revision 2 (sol review 1: observation point, clear obligation, expiry order, episode identity, tests) · **Base:** `docs/superpowers/specs/2026-10-23-vrr-design.md`
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
- AMD (Raphael, same monitor on DP-1, `--vrr`, 2026-11-08): Mesa sets `_VARIABLE_REFRESH` = 1 on `steamwebhelper`
  and Discord windows (neither is in Mesa's `adaptive_sync` list) and both get VRR; Discord's fullscreen video shows
  the same stall. The problem is not specific to NVIDIA.

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

- Application lists beyond the four process names of *Application exclusions* (revision 4): no Electron or Chromium
  in general, no user-editable yserver list.
- Entry on a sustained rate instead of three intervals, for bursty UIs not on the list. Not decided here.
- Reading the panel's real range (EDID monitor range descriptor). The thresholds are fixed values.
- Changing what makes a window eligible, the NVIDIA/Mesa exclusions, the cursor deadline, tearing, or the transport of
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
  - transient error: EBUSY waits for the flip event (the existing path); other errors except EINVAL retry through
    the existing retry deadline.
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

## Application exclusions (option B, revision 4)

### Why, measured

Hardware run 1 of the activity rule (2026-11-08; NVIDIA RTX 5060 Ti, DP-3, 48–240 Hz panel, `--vrr all`; logs in
`~/Projects/claude-scratch/vrr/active/hw/run1-nv/`):

- mpv at 100 fps entered VRR and left with `reason=client_idle` when it closed; mpv at 25 fps never entered. The
  activity rule works as designed.
- The Steam UI (`steamwebhelper`) draws in bursts of 3–5 frames 3–5 ms apart. Three fast intervals qualify and the
  episode expires 100 ms later: VRR went on and off 16 times in five minutes, and the user saw one or two frames at
  48 Hz each time. In the screenshot viewer it also presented at a steady 240/s with a static image.
- A Steam trailer presents at a steady 30 fps and a Discord video at 34–42 fps. Both qualify; no rate rule tells them
  from a game.

Neither list yserver honors names these processes: Mesa's `00-mesa-defaults.conf` excludes browsers, compositors and
video players but not `steamwebhelper` or Discord; NVIDIA's "No VRR/OSD" profile (620.70.06) has `steam` and
`steamcompmgr`, not `steamwebhelper`, and Discord only under a CUDA profile. The user decided against waiting for a
Mesa merge request, and to keep option B in this spec and this PR: A and B fix one problem.

Prior art, behaviour only. gamescope (Valve, BSD-2), commit `8e0a5bee` (2026-09-24): with adaptive sync the Steam UI
makes the panel swing between its minimum and maximum refresh, which shows as brightness flicker on OLED and VA
panels; while the Steam UI is focused gamescope repeats the last frame every vblank, holding the panel at its maximum.
It recognises the Steam UI by process name (`steamwebhelper`) and the `STEAM_BIGPICTURE` property. Its commit
`c8fecc6c` (2026-08-16) is the same 30 fps cursor rule as here. KWin gives VRR to any active fullscreen window and
excludes only through per-window rules the user writes, so it behaves as yserver did before this change. Sway and
wlroots have a per-output switch and no policy. No dedicated Discord exclusion was found in the inspected sources
(versions in Provenance); the user found it here.

### Decision

yserver carries its own exclusion list, applied in both `--vrr` modes on every driver, before the activity rule: an
excluded client never wants VRR. Names are matched exactly against the process name yserver already derives for the
NVIDIA/Mesa lists, unchanged: `read_process_name` in `vrr_exclusions.rs` needs a readable UTF-8 `cmdline`; it takes
the `exe` link's file name only when argv[0] starts with the full `exe` path, otherwise argv[0]'s file name; it does
not read `comm`, look at ancestors or fold case. The pid is the peer credential of the connection that sends the
Present, not the window's creator, `_NET_WM_PID` or a launcher:

| Process name | Application |
|---|---|
| `steamwebhelper` | Steam UI: library, store, screenshot viewer, trailers, Big Picture |
| `Discord` | Discord (official tarball binary `/opt/discord/Discord`; the Flathub launcher runs `/app/discord/Discord`) |
| `DiscordPTB` | Discord PTB (coordinator's call: same client, another channel) |
| `DiscordCanary` | Discord Canary (coordinator's call, as above) |

Exclusion means no VRR, not a held refresh: yserver changes `VRR_ENABLED` without a modeset, so turning it off keeps
the panel at its fixed refresh. gamescope repeats frames only because toggling VRR takes a modeset on its path.
Coverage is conditional on the Present connection's peer resolving to one of these names. Chromium and Electron hold
X connections in both the browser and the GPU process; which one presents, and its name under the Steam runtime,
Flatpak Steam, Flatpak Discord or a distro launcher, was not captured. A launcher named `discord` that execs the
binary with argv[0] `/usr/bin/discord` would not match. The hardware run records the `source=yserver` line per
application as evidence; no aliases, case folding or ancestor walk are added without such evidence.

Games launched by Steam are separate processes and are not affected; Steam's in-game overlay draws inside the game's
process and is not affected either.

### Design

- **One question, both modes.** One shared evaluator (environment, process name, then lists) answers through the
  existing per-client cache in both modes. The NVIDIA/Mesa list data stay optional and are loaded and consulted only
  in effective `All` (NVIDIA), as today; their absence (property mode, or `--vrr all` falling back on another driver)
  must not skip the environment and built-in checks. In `--vrr` the built-in list is the only list consulted: Mesa has
  already applied its own by not setting `_VARIABLE_REFRESH`. The Present activity gate and the frame *wanted*
  predicate use the same policy, so neither can enable VRR for an excluded client. No second `/proc` reader or cache.
- **Mode and capability first.** The lookup runs only after the mode check and the output capability check (missing
  or zero `vrr_capable`, no `VRR_ENABLED`, rejection latch). An incapable output reads nothing and logs nothing new.
  The existing `All` ordering, which evaluates before capability, is not changed by this revision.
- **Opt back in.** The existing environment rule keeps precedence, unchanged: if either `adaptive_sync` or
  `__GL_VRR_ALLOWED` is false the client is excluded (source `environment`); otherwise, if either is true, no list
  applies, the built-in one included. Opting in does not set `_VARIABLE_REFRESH` or waive the activity rule or
  capability. No new knob, no config file.
- **Cached for the client's life.** The decision is per client and dropped on disconnect, so a client cannot become
  excluded mid-episode; no new clear path.
- **Downstream.** *wanted* is false for an excluded client; the activity episode, idle clear and cursor hold already
  follow *wanted*.
- **Unknown pid.** A client without a pid (remote, no peer credentials) is not excluded, as today.
- **Logging.** The existing once-per-client line: `kms: vrr excluded client=… pid=… process=… source=yserver`.
- **No change without VRR.** With VRR off or an incapable output, nothing is read, logged or committed differently.
  A client not on the list, and a client without a pid, behave exactly as with the activity rule alone.

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
- `vrr_queued_frame_cannot_reenable_after_idle`: through the existing event path — active with a flip pending and a
  synchronous successor queued; no Present for 100 ms; the flip event dispatches the successor, which carries
  `VRR_ENABLED` = 0 (it discharges the obligation). Then three genuine Presents 16 ms apart do not re-enable VRR;
  the fourth does. Mutations: cache activity on the frame (the successor carries 1); renew `last` at dispatch (the
  third genuine Present re-enables).
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

Application exclusions (revision 4), with a scripted `/proc` tree as the existing exclusion tests use:
- `vrr_builtin_exclusion_steamwebhelper_property_mode`: `--vrr` on a non-NVIDIA driver and `--vrr all` falling back
  to property mode; capable output, process `steamwebhelper`, `_VARIABLE_REFRESH` = 1, Presents fast enough that it
  would qualify without the list: no `VRR_ENABLED` = 1, one `source=yserver` line. Mutation: bypass the shared policy
  in property mode (at both the activity gate and *wanted*).
- `vrr_builtin_exclusion_discord_all_mode`: `--vrr all` on nvidia-drm with empty NVIDIA/Mesa lists, rows `Discord`,
  `DiscordPTB`, `DiscordCanary`: same result. Mutation: drop any one name.
- `vrr_builtin_exclusion_environment_opt_in`: property mode, `steamwebhelper` with property 1; rows `adaptive_sync=true`
  and `__GL_VRR_ALLOWED=1` qualify and enable VRR; `__GL_VRR_ALLOWED=1` with `adaptive_sync=0` stays excluded
  (source `environment`); an opted-in client with property 0 stays off. Mutation: check the list before the
  environment, or let true beat false.
- `vrr_builtin_exclusion_exact_name`: property mode, processes `steam` and `steamwebhelper2` qualify and enable VRR.
  Mutation: substring match.
- `vrr_builtin_exclusion_no_pid_allowed`: property mode, property 1, no peer pid: qualifies and enables VRR, no
  `source=yserver` line. Mutation: a missing pid denies in the new path.
- `vrr_builtin_exclusion_off_or_incapable_reads_nothing`: rows VRR off, capability missing or 0, no `VRR_ENABLED`,
  rejection latch; `steamwebhelper` with property 1: no process read (observed through a read counter or hook, not
  through absent files, which the reader tolerates silently), no log, commits as today. Mutation: run the lookup
  before the mode or capability check.
- The existing exclusion tests pass unchanged.

**Hardware (ask the user first; NVIDIA DP-3 and AMD DP-1):**
- Steam screenshot viewer: OSD stays at 240 Hz once the image is static; the cursor is smooth.
- mpv at 100 fps: VRR on. mpv at 25 fps: VRR stays off, because entry needs 30 fps.
- A game: VRR on, with no `client_idle` while playing. A pause menu that stops drawing turns it off, and it comes
  back on resume.
- Discord fullscreen video: observed in run 1 (VRR while playing at 34–42 fps); input for option B.
- After option B (NVIDIA DP-3 `--vrr all`, AMD DP-1 `--vrr`): Steam screenshot viewer, trailer and library stay at the
  fixed refresh with no 48 Hz flashes; Discord video stays at the fixed refresh; mpv at 100 fps and a game launched
  from Steam still enter VRR. Each excluded application's `source=yserver` line (client, pid, process) is recorded
  next to the OSD result.

## Provenance

- yserver master `0eceeaa3`: the VRR *wanted* predicate, the standalone clear and the cursor hold
  (`kms/render/backend/kms/scanout.rs`, `kms/render/platform/cursor.rs`).
- The yserver log of the user's session, 2026-11-07, for the measurements in Why.
- `/usr/share/nvidia/nvidia-application-profiles-620.70.06-rc`, read for which process names it excludes. Nothing
  is copied.
- Revision 4: gamescope (BSD-2) `e0ad8bed`, commits `8e0a5bee` and `c8fecc6c`; KWin (GPL) `0406762`,
  `src/compositor.cpp`, `src/window.cpp`, `src/core/renderloop.cpp`, `src/scene/surfaceitem.cpp`; Sway (MIT)
  `1652c54`, `sway/sway-output.5.scd`; Mesa `e2ab6687d5a`, `src/util/00-mesa-defaults.conf` and its commit
  `a9c36dbf9c5`; Flathub `com.discordapp.Discord` `discord.sh`. Read for behaviour only; nothing is copied.

This document describes behaviour in original prose; no third-party code was used.
