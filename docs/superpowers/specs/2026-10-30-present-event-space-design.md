# Present vblank arming and frozen CRTC clocks

## Why

With two NVIDIA heads, running `fastfetch` in a new Alacritty made the cursor
stutter on both monitors, and it stayed that way until the session ended. The
log filled with `ENOMEM` from vblank arms, atomic cursor commits and flips.

Measured with `present_pace` logging, on master `824cc763`:

1. A client's `xrandr` changes the mode of HDMI-2 after DP-3 was enabled. DP-3
   keeps flipping, but its MSC and UST stop: 41,396 flip events reported
   `msc=213` with one timestamp, until the session ended.
2. Alacritty's first Present asks for `target_msc=1` on DP-3. Its target is 214,
   which never arrives. Clients asking for `target_msc=0` complete at once on the
   same clock, which hides the freeze.
3. The window's later Presents run on the live CRTC, but completion is ordered
   per window, so they wait behind the first one with targets already reached.
4. `arm_present_idle_vblanks` passes those targets to the absolute arm. Each
   event retires them from `absolute_vblank_targets`, so they are armed again and
   fire immediately. Sequence events per MSC follow the number of held Presents:
   31, 32, 33, then 66, 100, 127.
5. `drain_events` reads 1024 bytes per iteration, 32 events. Above that, unread
   events accumulate up to the kernel's limit of 4096 bytes per file, and every
   event-carrying ioctl on that fd returns `ENOMEM`, including flips and cursor
   commits on both heads.

Two defects are ours and independent of each other. A client can exhaust the
event space on any driver, and a Present on a frozen clock never completes. The
freeze itself is in the NVIDIA driver: a modeset leaves the vblank interrupt off
on heads it did not change, and it only recovers when DRM vblank is disabled and
re-enabled.

## Goals

- The number of outstanding Present vblank arms per CRTC is bounded by a small
  constant, whatever clients queue.
- A Present on a frozen CRTC completes, paced at the mode's refresh rate.
- No change that a client can observe on a CRTC whose clock advances.

## Non-goals

- Avoiding the freeze: forcing a modeset on the frozen head causes a visible
  glitch, and the driver is not ours.
- Changing how RANDR applies modes. Whether yserver can avoid a real modeset in
  the `xrandr` sequence is a separate measurement.
- Retrying commits that fail with `ENOMEM`.
- Reading more than one buffer per iteration. With arms bounded, a backlog above
  32 events cannot build up from this path, so this adds nothing.

## Design

### One shared Present wake per CRTC

Arming per target is what lets clients decide how many kernel events exist. A
single wake per CRTC is enough: while any target on the CRTC lies ahead of the
clock, keep one next-vblank arm outstanding. When it fires, the loop drains what
is due and arms again if something still lies ahead.

This replaces `absolute_vblank_targets` with one armed slot per CRTC. The arm is
relative (next vblank) and keeps the existing tag, so it stays completion-eligible
while a flip is active, which the untagged idle slot is not. Both former absolute
call sites in `arm_present_idle_vblanks` share the slot. For the exec site, an
armed slot covers all its targets, so its "not covered, execute immediately"
branch keeps its meaning: it runs only when no wake could be armed.

A target already reached arms nothing: if it is not blocked the sweep fires it,
and if it is blocked, it waits for the completion that unblocks it. "Ahead of the
clock" uses the clock the completion sweep decides with: the selection in
`fire_present_completions_sweep`, moved to a shared helper so the two cannot
disagree. With no usable clock (`msc == 0`), a target counts as ahead.

Outstanding Present arms per CRTC are then at most two, the idle slot and this
one, across iterations, new arrivals and cancelled windows. A destroyed window
leaves at most one wake that finds nothing to do. The cost is a wake every vblank
while a distant target is parked, on top of the iterations the loop already runs.

### Software clock for a frozen CRTC

The existing fallback for devices with no kernel vblank source already has the
timer, dispatch and lifecycle a frozen CRTC needs: `arm_software_vblank`,
`service_software_vblanks`, and the state pruned with the CRTC. This extends its
condition from per device to also per CRTC, adding a small `frozen` map in
`VblankFallback`. Only how a frozen CRTC computes MSC differs, as described below.

**Detection.** A next-vblank arm is itself the observation: the kernel accepted
it, it holds a vblank reference, and on a running head it must fire within one
frame. A CRTC is frozen when a next-vblank arm on it, of either slot, has not
fired one second after it was armed.

- Each armed slot records its deadline, and `next_wakeup` includes it, so the check
  runs when no other event arrives. This wake does not advance any clock.
- The check runs after the iteration's event drain, so an event that is merely
  unread never counts.
- The code that already clears the armed slots, on DPMS off, VT away, a CRTC's own
  modeset and output removal, also ends the observation. A CRTC with no Present
  work ahead has no arm and is never judged. A device already on the fallback has
  no kernel arm.

The one-second window is a constant, not an option. It is the longest a Present
can stall on a freeze, and it is far longer than any frame yserver drives,
including VRR's lowest rate.

**While frozen.** MSC comes from time alone, as in Xorg's fake vblank: the last MSC
at the moment of marking, plus the whole frame periods of the current mode elapsed
since then, with UST the time of that frame. So `frozen` maps each `CrtcKey` to that
anchor, and a mode change on the same route re-anchors at the current value so the
clock never goes back. Flip count has no effect: a head flipping asynchronously at
600 Hz in a 60 Hz mode still advances 60 MSC per second.

The CRTC arms software vblanks instead of kernel ones, each deadline being the next
frame boundary, and a software tick records this clock. A flip that completes keeps
retiring buffers as it does now, but its stale frame and timestamp are discarded: the
sample it reports is this clock's current value. Kernel sequence events still clear
their slot but no longer touch the clock.

**Leaving.** The mark lasts as long as the `CrtcKey` is live, and goes with the
CRTC's other clock state in `prune_present_clocks_to_live_outputs` when the CRTC is
disabled or removed. A modeset on the same route keeps it, since neither the epoch
nor the clock samples change there, and a kernel counter below the software one
would be rejected as going backwards. A head that recovers stays on the software
clock until then. This is a cost we accept: the rate stays correct, and a handoff
between counters would be a second transition with its own bugs.

## Testing

Tests go through the core loop, with a stubbed event source for what only a
driver can produce.

- **Bound across iterations:** more than 128 Presents behind an unfinished older
  one, and distinct future targets that arrive one per iteration in descending
  order, and windows destroyed with targets parked. The stub counts unretired arms
  and never sees more than the bound, and flips keep succeeding. Mutation: restore
  per-target arming.
- **Every target still on time:** more than 128 distinct future targets complete
  in order, each at its target, including across MSC wrap and several windows on
  one CRTC. Mutation: stop re-arming after the first wake.
- **Reached targets:** a held Present arms nothing and fires when unblocked.
  Mutation: treat reached targets as ahead.
- **Exec coverage:** a failed arm still executes the parked entries immediately.
  Mutation: report covered on failure.
- **Frozen clock, no driver events:** the stub never fires one CRTC's arm and
  delivers no flips. A `target_msc=1` Present completes within the window plus a
  frame, and later Presents of that window follow in order. Mutation: remove the
  deadline from `next_wakeup`.
- **Frozen clock, flips running:** flips keep completing with a stale frame and
  timestamp, faster than the refresh rate. MSC follows elapsed frames of the mode, not
  flip count, and buffers retire. Mutations: record the flip's frame; advance per flip.
- **Stalled loop and mode change:** after a stall of several frames MSC catches up in
  one step, and a same-route mode change neither rewinds it nor jumps it. Mutation:
  drop the re-anchor.
- **No false positive:** an advancing clock, a slow but firing arm, a held Present
  with nothing ahead, an event still unread at the deadline, DPMS off, VT away, the
  CRTC's own modeset. Mutation: drop each guard.
- **Leaving:** disabling and re-enabling the CRTC returns it to the kernel clock; a
  same-route modeset keeps the software clock and MSC keeps rising. Mutation:
  clear the mark on any modeset.
- **Other CRTC:** freezing one CRTC leaves the other on its kernel clock.

The suites run under CPU load.

**Hardware, with the user watching:** on NVIDIA, a user's `xrandr` sequence that sets
DP-3 and then changes HDMI-2, then a new Alacritty and `fastfetch`, with `present_pace` on. Expected: no
`ENOMEM`, the frozen CRTC logged once, the cursor smooth on both monitors. Repeat
on AMD as a regression run.

## Known costs

- On a frozen CRTC, MSC and UST come from a timer, not from the display, so a
  client pacing on them is only approximately in phase.
- A Present waits up to one second when a freeze begins.
- A head that recovers keeps the software clock until it is disabled or removed.
- A wake every vblank while a future target is parked.

## Provenance

The driver's behaviour was read in NVIDIA's open-gpu-kernel-modules and DRM's
event limit in the Linux kernel; both were read for behaviour only, with no code
copied. The software vblank fallback already in yserver follows the idea of Xorg's
Present fake vblank (MIT). No code was taken from any of these for this design.
