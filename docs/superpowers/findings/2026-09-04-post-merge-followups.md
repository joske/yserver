# Post-merge follow-ups to the damage-clipped repaint: four fixes, one spin reproduced

**Date:** 2026-09-04. **Branch:** `fix/damage-repaint-followups` off master
`02bafec3`. Codex reviewed the squash and reported four regressions; all four
were verified against the code and fixed test-first, with the expected
behaviour known in each case.

## 1. Hidden damage busy-looped the render thread — reproduced, fixed

`scene_wants_compose()` is "structure dirty, or any drawable with pending
presentation damage that is not dormant", and `next_wakeup()` then fires
immediately. Stage C stopped projecting and acking damage classified `Hidden`,
and the walk kept reporting hidden nodes as sampled, so the dormancy
reconciliation never flagged them: tick → walk → empty damage → wake, forever.

Repro on silence, MATE, mpv playing fully under a maximised terminal
(`data/2026-09-04-hidden-damage-spin-mate-silence-telemetry.log`, 09:38-09:39):

| covered mpv, 26 paints/s | before | after |
|---|---|---|
| `build_scene_calls/s` | 1650-1900 | 61-100 |
| composes/s | 2 | 2 |
| `content_damage_hidden/s` | ~900 | 28-33 |

The bee Warframe log's 800-1700 walks/s spikes were the same loop.

Fix: the set handed to `reconcile_offscreen_no_draw` now means "this
drawable's pending damage was *presented* on some output this tick" rather than
"this drawable was sampled". Dormancy has **two reasons**, because a paint's
only wake path is `has_pending_presentation_damage()`:

- `NoPieces` — the node emitted nothing (fully covered, off-output, empty
  shape). No paint can become visible without a structural change, and every
  structural change wakes the loop. Stays dormant across paints — the
  pre-existing cut-2b behaviour, preserved.
- `HiddenDamage` — the node emitted pieces but its damage lay under a cover.
  Its next paint may be visible, so `DrawableStore::damage` re-arms it: one
  walk per paint, which is the correct floor. The first version of the fix made
  such a window dormant for good, which would have frozen the visible half of a
  half-covered mpv; caught in review before hardware.

## 2. Batched restacks could miss damage — fixed

The overlap rule compared only pairs where *both* participants changed rank.
`A B C → C B A` leaves B at rank 1 while it flips against both; if A and C are
disjoint the damage was empty. Now each rank-changed participant is compared
against every other common participant.

## 3. Shapes past the region cap compared by bounding box — fixed

`ScenePresence.region` is a capped `Region`; two shapes over 32 fragments with
the same bounding box compared equal, and shape changes no longer post
whole-output damage. The presence now carries the exact place rects (moved, not
cloned; emission order, not sorted — a re-specified shape in a different order
reads as `moved`, the safe direction) and the `moved` test uses them.

## 4. A truncated submit was not repaired next frame — fixed

`invalidate()` set the owed region to full, but the empty-damage skip ran before
the model was consulted and `scene_wants_compose()` never looked at it. An owed
repaint now counts as wanting a compose and is checked before the skip. Covers
the three pre-existing `invalidate` callers too.

## Acceptance

- silence/MATE, hand-driven (`data/2026-09-04-hidden-damage-fix-mate-silence-telemetry.log`):
  table above; full cover, half cover, raise/lower all visually right (jos).
- silence/e16 phased workload
  (`data/2026-09-04-followups-e16-silence-workload-*.log`) vs the 2026-09-02
  pre-step-1 baseline: painted identical at idle (0.141), drag 0.174 → 0.172,
  resize 0.167 → 0.163, **restack 0.151 → 0.142 with structural 0.013 →
  0.000**, zero Full frames in every phase, `overdraw` 1.01, GPU per compose
  41-50 → 13-17 µs on the RX 6800.
- 1204 tests, clippy `-D warnings` clean. Bench: `Off` at baseline, `On` +7%
  (two id pushes per node).

## What the e16 run also says: the wake multiplier is now the cost

| e16 phased workload, silence | |
|---|---|
| walks/s | 769 median, 1198 max |
| composes/s | 57 |
| **walks per compose** | **13.8** |
| walk per call | 436 µs (85 nodes) |
| **`build_scene` ms per second** | **346 median, 461 max** |
| `content_damage_hidden/s` | 0 median (no spin) |

With the spin gone, every remaining walk is a legitimate wake — e16's CopyArea
storm paints 1000-2700 times a second and each paint wakes the tick, which
walks to find out whether anything is visible. On the z400 the same ratio was
3.2 (different timing), already 12-20% of a core. The lever is a pre-walk
predicate in the tick driver: a wake that carries no new presentation damage
on an armed drawable, no structure change and no cursor move must not walk.
That is the next item, not optional.

## The walk-skip, measured (e16 phased workload, silence, 2 outputs)

Three runs of `just yserver-e16-hw-workload`, same box and workload, medians
over the run. "four fixes" is `b41825bb`; "v1" added a pre-walk predicate whose
pending-damage input was global; "v2" is that predicate per output plus the
ack-race fix (`36e357af`).

| | four fixes | walk-skip v1 | **v2** |
|---|---|---|---|
| `build_scene_calls/s` | 771 | 771 | **354** |
| `tick_skips_nothing_pending/s` | — | 0 | **227** |
| `build_scene` ms per second | 357 | 357 | **98** |
| composes/s | 57 | 57 | **34** |
| `avg_build_scene_ns` | 463 µs | 463 µs | 225 µs |
| painted, idle / drag / resize | 0.141 / 0.172 / 0.167 | 0.141 / 0.173 / 0.164 | 0.141 / 0.174 / 0.170 |
| Full frames | 0 | 0 | 0 |
| `overdraw` | 1.01 | 1.01 | 1.01 |

**Why v1 did nothing:** its pending-damage question was "does any armed
drawable have damage", which e16's pager answers yes ~1020 times a second. The
output that shows the pager is flip-pending and returns before the walk anyway;
the *other* output was the one walking pointlessly, and a global question
cannot see that. Asked per output — "is an armed damaged drawable in this
output's last visible set, or unknown to every output" — the redundant walks
disappear.

**Composes fell from 57/s to 34/s at identical painted fractions.** That is the
ack-race fix showing up as work removed: output 1 was force-composing damage
that projects entirely outside it, once per hover or pager tick, and those
composes displayed nothing. The per-phase area counters are deterministic and
unchanged, so the visible work is the same.

Per-walk time also halved (463 → 225 µs) at the same node count (84 per walk).
Not attributed — the aggregate is what the change targets, and 98 ms/s against
357 is consistent across the run.

**What that number is and is not (codex, 2026-09-04):** it is `build_scene`
time only, the interval `avg_build_scene_ns` measures. The predicate's own cost
— collecting the armed damaged ids and building each output's `elsewhere` set
per wake — falls *outside* that timer, and **total process CPU was not
measured**. So "walk CPU fell 3.6×" is supported; "the server uses a third less
CPU" is not. Measuring the process would need `yserver-mate-hw-perf` or
equivalent, and the pre-walk allocations are the first thing to look at there
(an armed-damaged counter in the store would make the store side O(1)).

**Still open:** damage straddling two outputs is presented by whichever output
composes first; no repro yet. And the wake rate is still ~12 walks per compose,
now for legitimate reasons (e16 paints 1000-2700 times a second, each paint a
wake); further reduction would mean coalescing wakes, not skipping walks.

## Single-output coverage on the z400 (build `8da43094`), and the one path still uncovered

MATE and XFCE, hand-driven, one output (DP-2 2560x1440 — the z400 has no second
monitor). Raw: `data/2026-09-04-followups-{mate,xfce}-z400-singleoutput-telemetry.log`.

| | MATE (184 buckets) | XFCE (138 buckets) |
|---|---|---|
| `overdraw` | 1.00 | 1.00 (max 3) |
| `damage_fraction` | 0.304 | 0.244 |
| `avg_gpu_render_ns` | 590 µs | 698 µs |
| composes / Full | 7434 / 1549 (threshold) | 5036 / 802 (threshold) |
| `build_scene` ms/s | 11.6 | ~12 |
| `tick_skips_nothing_pending/s` | 0 | 0 |
| `content_damage_off_output/s` | **0 in every bucket** | **0 in every bucket** |

No warnings, no panics, nothing visually wrong. Two negatives worth stating
plainly rather than reading as passes:

- **The walk-skip is inert on one output, correctly.** Walks ran 50/s against
  41 composes (MATE) and 52 against 35 (XFCE) — there are no redundant wakes to
  skip, because the savings on silence came from the *second* output walking for
  damage it cannot show.
- **The off-output branch never fired**, on either desktop. That is the branch
  this campaign changed last: it still forces a compose (so a paint whose
  projection is empty is not stranded — the xfce-submenu bug) but no longer
  acks, since acking pixels an output does not display is what caused the
  multi-output race. XFCE menus and submenus were exercised specifically to
  reach it and the counter stayed at zero, so the empty-projection condition
  does not arise on either desktop today — plausibly because `e2fe3a03` fixed
  the structure-damage coordinate translation that produced it. **The risk of
  the change is therefore low and its correctness remains unexercised;** if the
  xfce symptom ever returns (a submenu painted but not shown until something
  else moves), this is the first place to look. The recipe now sets
  `YSERVER_TICK_SKIP_LOG` so a future run has the per-drawable lines, not just
  the counter.

## The same e16 workload on the z400 (one output): restack fixed, walk-skip inert

Build `8da43094` against the 2026-09-03 stage-C run on the same box and
workload. Raw: `data/2026-09-04-followups-e16-z400-workload-*.log`.

| phase | painted, stage C → now | structural | Full/s |
|---|---|---|---|
| idle | 0.141 → 0.141 | 0 → 0 | 0 |
| drag | 0.179 → 0.178 | 0.022 → 0.023 | 0 |
| resize | 0.167 → 0.168 | 0.016 → 0.013 | 0 |
| **restack** | **0.226 → 0.142** | **0.052 → 0.000** | **3.0 → 0** |
| idle3 | 0.141 → 0.141 | 0 → 0 | 0 |

The restack row is the rank-rule fix landing on the target box exactly as it did
on silence: a raise or lower now costs what idle costs, and the phase's Full
frames are gone.

| | stage C | now |
|---|---|---|
| `build_scene_calls/s` | 113 | 109 |
| `avg_build_scene_ns` | 1.06 ms | 1.08 ms |
| `build_scene` ms/s | 120 | 118 |
| walks per compose | 3.5 | 3.3 |
| `tick_skips_nothing_pending/s` | — | **0** |

**The walk-skip does nothing here, and that is the honest reading.** With one
output every wake carries real work: e16's pager paints ~1014 times a second on
the only output there is, so the predicate says "walk" every time and the 3.3
walks per compose are all legitimate. The 771 → 354 saving on silence came from
a *second* output walking for damage it cannot show. So the walk CPU on the
target box is unchanged at ~118 ms/s (12% of one core), and reducing it needs
wake coalescing — several paints in a frame should wake once — not walk
skipping. That is the next lever if e16-class paint rates ever matter.
