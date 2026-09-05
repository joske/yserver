# Step 1 on the target box: per-compose GPU time 4.7 ms → 0.83 ms, overdraw ≈ 1

**Date:** 2026-09-03. **Box:** z400 (2009 Xeon) + RX 460, MATE non-composited,
hand-driven telemetry run (`just yserver-mate-hw-telemetry`), build `465fb585`
= steps 0/2/3/4/4.5 + step 1 stages A+B. 64 one-second buckets, no warnings,
no panics, `no_opaque_cover = 0` throughout.
Raw: `data/2026-09-03-stageB-mate-z400-handdriven-telemetry.log`.

## Numbers

| | 2026-09-01 z400 baseline (always-Full) | 2026-09-03 stage B |
|---|---|---|
| `avg_gpu_render_ns` per compose, median | 4.69 ms (range 3.76-6.33) | **0.83 ms** (max 2.04) |
| composes/s, median | ~44 | 36 |
| implied GPU share (per-compose × composes/s) | 15-21% | **3.0%** |
| labwc on the same card (2026-09-01) | 2.8% | 2.8% |
| `overdraw`, median | — (counter did not exist) | **1.11** (see the correction below) |
| hidden participants per walk | — | 7.6 |
| Full frames | 100% | 15.5% (hand-driven, `threshold`) |
| walk (`avg_build_scene_ns`) | unmeasured | 131 µs/call, 5.3 ms/s |

Both runs are hand-driven and neither is the phased workload, so the ratio is
indicative rather than exact — but 5.7× is far outside any workload confound
seen on this campaign, and the direction agrees with every same-instrument
before/after so far. **The implied share lands on labwc's number.** The
memory's standing caveat applies: "implied share" is per-compose time ×
compose rate, not a per-process GFX measurement. The one run that closes the
campaign is still `amdgpu_top --json` per-process GFX on this box during this
workload, two minutes, directly comparable to labwc's 2.80%.

The single 100%-Full bucket (startup, 55 composes, damage 1.0) reads
**2.04 ms per Full compose** with `overdraw 2.96` — the reporter's frame shape
(#131, 99% Full). His 6.73 ms at 4K was measured pre-step-1; a rerun on
≥ `a1da01bf` sizes his gain directly.

Per-compose GPU time still falls with load (846 µs at < 100 paints/s → 473 µs
at > 800), the clock-ramping signature noted on 2026-09-02, so compare buckets
at similar load.

## Where the cap collapses

`visibility_collapses/s[mine= claim= taken= taken_skipped=]`: **`mine` never
collapsed** in 64 buckets; only `claim` did (up to 131/s in busy buckets). That
is the universe exceeding 32 boxes while opaque rects are subtracted from it —
the documented safe direction (superset, under-cull). The one union the walk
builds is fine in practice, and the 2-3 collapses per walk seen on silence/e16
are almost certainly the same site.

The walk on the z400 runs ~1.2× per compose (43.5 calls/s vs 36 composes/s),
not the ~11× seen on silence under the xdotool workload — the wake multiplier
is workload-driven, not a property of the box.

## Correction: `overdraw` was inflated by walks per compose

`scene_draw_pixels` was recorded on every **walk** while its denominator,
`output_pixels`, is recorded per **compose**. The walk runs on every wake, so
the counter read `overdraw × (walks / composes)`. The tell on this run: a
startup bucket with 2284 walks and one compose read `overdraw = 2284`, and
early buckets read 29, 11, 8 while composing 5-29 times against 45-840 walks.

Consequences for earlier numbers:

- **"Overdraw is 25×" (2026-09-02, MATE on silence, commit `8bd28ca2`) was
  ~2× real overdraw times ~11.6 walks per compose** (578 walks/s vs 50
  composes/s in the same log). The step-1 sizing argument that rested on 25×
  was wrong in magnitude; the conclusion drawn from it ("the scissor cull
  already took the saving") happened to survive because it was about clipped
  frames. The plan's step 1 history line is corrected.
- The stage-B e16 comparison "15.2 → 7.1" on silence is inflated by the same
  ratio on both sides; the *ratio* between the two runs is roughly right only
  if the walks-per-compose ratio was similar, which it was (436/55 vs 337/56).
- On this z400 run walks ≈ 1.2× composes, so 1.11 is ~0.9-1.1 real: **the
  scene no longer overpaints** — what remains is the software cursor and the
  handful of translucent draws.

Fixed in the same commit: `scene_draw_pixels` is now recorded beside
`record_damage_pixels`, once per compose. Numbers from builds before this fix
must be divided by `build_scene_calls/s ÷ composite_submits/s` for the same
bucket before being quoted.

## e16 on the z400 (same day, build `not logged — log begins after the startup line; same session day as 465fb585`, hand-driven, 117 buckets)

Raw: `data/2026-09-03-stageB-e16-z400-handdriven-telemetry.log`. No warnings,
no panics, `no_opaque_cover = 0`.

| | median | max |
|---|---|---|
| `avg_gpu_render_ns` per compose | 417 µs | 732 µs |
| `damage_fraction` | 0.141 | 1.0 |
| Full frames | 1.5% (63 of 4090) | |
| nodes / draws / hidden per walk | 79 / 213 / 13.5 | |
| `avg_build_scene_ns` per call | **1.10 ms** | 1.59 ms |
| walks/s vs composes/s | 108 vs 34 (3.2×) | 413 |
| **walk ms per second** | **117 (≈12% of a core)** | 203 (≈20%) |
| `overdraw` (pre-fix counter, ÷ 3.2) | 3.85 → ≈1.2 real | |
| collapses/s: mine / claim / taken | 0 / 444 / 0 | 0 / 953 / 0 |

GPU side is where it should be: 417 µs per compose at 14% damage on a 2009
box, 1.5% Full frames. **The CPU side is the open item.** The walk costs 12-20%
of one core on the z400 under e16, on the thread that also serves protocol.
Two facts bound it: the bench (72 nodes, i9, release + debug-assertions)
measures 260 µs, and a 2009 Xeon is 3-4× slower per thread, so 1.1 ms is the
walk performing as the bench predicts on slow hardware — not a regression of
the implementation against its own baseline. But there is no stage-A e16 run on
the z400, so the share of that 1.1 ms attributable to visibility (the bench
says +73%) is unmeasured here.

The lever is not the walk: **walks run 3.2× more often than composes**
(108/s vs 34/s). Every walk that ends in an empty-damage skip is pure cost, and
whether damage is pending could be known before walking (a store-level "any
presentation damage / structure dirty / cursor moved" flag). That is a
tick-driver change — out of step 1's scope — worth roughly a 3× cut in walk
CPU on this workload, more on silence where the ratio was 11×.

Collapses: `mine` never; `claim` ~4 per walk — e16's 79 nodes × several place
rects push the universe past 32 boxes routinely. Safe direction, but each
collapse hands every node below it a bounding-box universe, which is why hidden
participants (13.5/walk) are fewer than the scene would allow. A larger cap
for the walk's universe only (64-128) is the obvious experiment; it trades
region-op cost for culling and the bench can price it.

## Closing measurement: per-process GFX, same instrument as the labwc baseline

`amdgpu_top --json`, 20 one-second samples, z400 + RX 460, **awesome
(non-composited) + windowed mpv + wezterm**, build `465fb585`. This is the run
the campaign has asked for since 2026-09-02: the kernel's own per-process
accounting, directly comparable to the labwc capture on the same box.
Raw: `data/2026-09-03-yserver-awesome-mpv-rx460.json`; telemetry of the same
session `data/2026-09-03-stageB-awesome-z400-handdriven-telemetry.log`.

| run | process | GFX mean | GFX max | CPU mean | device GFX | SCLK |
|---|---|---|---|---|---|---|
| **yserver step 1** (awesome, 2026-09-03) | **yserver** | **4.70%** | 9 | 3.3% | 12.0% | 1180 MHz |
| | mpv | 3.35% | 4 | 24.2% | | |
| | wezterm-gui | 0.35% | 3 | 5.8% | | |
| labwc (2026-09-01) | labwc | 2.80% | 5 | 3.6% | 14.6% | 1129 MHz |
| | mpv | 3.94% | 4 | 39.8% | | |
| yserver + Cinnamon (2026-09-02, direct scanout) | cinnamon | 6.54% | 10 | 6.0% | 16.2% | 1099 MHz |
| | yserver | 1.88% | 4 | 7.5% | | |

**yserver composes this workload at 4.70% GFX against labwc's 2.80% — 1.7×,
down from the 5-10× the campaign opened with.** The stack as a whole is
cheaper than labwc's (12.0% device GFX vs 14.6%) because mpv itself costs
less here (3.35% vs 3.94%; clock 1180 vs 1129 MHz, so per-frame work, not
throttling). yserver's CPU share (3.3%) is below labwc's (3.6%).

**The layout (jos, 2026-09-03): wezterm fullscreen, mpv floating above it — the
reporter's shape from #131.** So the root is entirely hidden under wezterm and
emits nothing; wezterm is emitted as the frame around mpv; mpv on top. That is
why `overdraw` reads exactly 1.00. What the remaining 1.7× is, from the
telemetry of the same session: mpv's floating window is ~0.55-0.6 of the
output, so `damage_fraction` ran at 0.547 median and **23.7% of composes went
Full on `threshold`** — those cost 2.2-2.7 ms each (the five 100%-Full
buckets), against 0.9 ms median overall. On the phased
MATE workload (damage 0.14, zero Full frames) the share would be lower; on
the reporter's terminal-tiled workload (99% Full) higher. Two levers remain and
both are outside step 1: the 0.6 clip threshold was measured on bee, not on
this card (plan 4.2), and a Full compose still pays the fixed per-compose floor
the audit fitted at 40-110 µs plus whatever the clipped path's LOAD saves.

**Calibration note for every "implied share" in this campaign:** compose time ×
compose rate for this same session gives 0.899 ms × 31/s = **2.8%**, while
fdinfo reports **4.70%** — the implied figure under-reads the kernel's
accounting by ~1.7× here. The MATE figure above (3.0% implied) should be read
as roughly 5% fdinfo-equivalent, and the 2026-09-01 baseline's 15-21% implied
as correspondingly higher. The ratio between runs on the same instrument is
what transfers; the absolute number is fdinfo's.

## Stage C on the z400, e16 phased workload — per phase against the silence baseline

Build `92a7e0f1` (stage C), `just yserver-e16-hw-workload` on the z400, against
the 2026-09-02 e16 workload on silence (steps 0-4, pre-step-1). Different box
and GPU, so only the **area** counters compare (they are deterministic to three
decimals); µs do not. Raw: `data/2026-09-03-stageC-e16-z400-workload-*.log`.

| phase | painted before → after | structural before → after | Full/s after |
|---|---|---|---|
| idle | 0.141 → 0.141 | 0 → 0 | 0 |
| drag | 0.174 → 0.179 | 0.016 → 0.022 | 0 |
| idle2 | 0.141 → 0.141 | 0 → 0 | 0 |
| resize | 0.167 → 0.167 | 0.010 → 0.016 | 0 |
| **restack** | **0.151 → 0.226** | **0.013 → 0.052** | **3.0** |
| idle3 | 0.141 → 0.141 | 0 → 0 | 0 |

No warnings, no panics; `content_damage_off_output/s = 0` throughout (the force
never fired); `content_damage_hidden/s` 3-23 during restack, when mpv sits
under the terminal — stage C doing its job. `overdraw` 1.16-1.28 per compose.

**Restack is the one regression, and it is the step-2 rank rule, not stage C.**
Each raise/lower (3.3/s) produces 2-4 `threshold` Full frames per second and
~0.64 of the output in structural damage per event. `structural_damage` marks a
participant `restacked` when its rank among common participants changed and
damages its whole `old ∪ new` region — and moving one window past another
shifts the rank of everything between them, so the window it jumped over is
damaged in full too. mpv (0.14 of the output) raised over a terminal that
covers roughly half the screen is ~0.64 ⇒ over the 0.6 threshold ⇒ Full. The
silence baseline saw 0.013 with the same workload because the terminal there
was smaller relative to the output (two outputs, e16 placement); the rule is
the same in both builds. Hypothesis about the terminal's size is inferred from
the arithmetic, not measured — the recipe logs at `warn`, so geometry is not in
the log.

The tight rule is available now that regions are exact: for a pure restack the
pixels that can change are only where the reordered participants **overlap**,
so damage should be `⋃ (A.region ∩ B.region)` over pairs whose relative order
flipped, not `⋃ region` over everything whose index moved. wlroots gets the
same effect from per-node visible regions. Small, unit-testable
(`a raised window damages only what it overlaps`), and it turns the restack
phase from Full frames back into clipped ones.

## Restack fixed — confirmed on the z400 (`35c8ac8f`)

`just yserver-e16-hw-workload`, same box, build hash now in the log. Restack row,
stage C → restack fix: painted **0.226 → 0.142**, structural **0.052 → 0.000**,
Full/s **3.0 → 0.0**. Every other phase unchanged to three decimals. A restack
phase now costs what idle costs (0.141), which is also below the silence
baseline's 0.151 / 0.013.

`structural = 0.000` says the flipped pair (mpv, terminal) does **not overlap**
on this layout: the old rule damaged both windows in full whether or not they
overlapped (0.14 + 0.5 ≈ 0.64 ⇒ Full), the new rule damages their intersection,
which is empty — and a restack of two disjoint windows changes no pixel, so
that is the right answer. The earlier "mpv raised over a half-screen terminal"
story had the sizes right and the overlap wrong. An intermediate run that
showed no change was a stale build; that is what the `yserver::startup` log
target is for. Raw: `data/2026-09-03-restackfix-e16-z400-workload-*.log`.

## Second per-process capture, on the final build of the day (`35c8ac8f`)

`amdgpu_top --json`, **64 samples**, awesome + windowed mpv + wezterm, same box.
Raw: `data/2026-09-03-yserver-awesome-mpv-rx460-restackfix.json`; telemetry of
the session `data/2026-09-03-restackfix-awesome-z400-handdriven-telemetry.log`
(74 buckets, 22.6% Full frames, `overdraw` 1.00, GPU 1.0 ms median per compose).

| run | process | GFX mean | GFX max | CPU | device GFX | SCLK | samples |
|---|---|---|---|---|---|---|---|
| **`35c8ac8f`** (stage C + restack fix) | **yserver** | **4.27%** | 10 | 3.2% | 7.8% | 1131 MHz | 64 |
| | mpv | 3.27% | 4 | 43.2% | | | 52 |
| `465fb585` (stage B) | yserver | 4.70% | 9 | 3.3% | 12.0% | 1180 MHz | 20 |
| | mpv | 3.35% | 4 | 24.2% | | | 20 |
| labwc (2026-09-01) | labwc | 2.80% | 5 | 3.6% | 14.6% | 1129 MHz | 20 |
| | mpv | 3.94% | 4 | 39.8% | | | 18 |

**yserver at 4.27% against labwc's 2.80% — 1.5×** on a 64-sample capture at
the same clock as the labwc run (1131 vs 1129 MHz), so this pair is the
cleanest like-for-like of the day. The 0.4-point drop from stage B is within
what two hand-driven sessions can differ by and is not claimed as stage C's
effect; on awesome, stage C and the restack fix have little to act on. mpv
appears in 52 of 64 samples (started after the capture began), which slightly
depresses its mean, not yserver's. The stack total is 7.8% device GFX against
labwc's 14.6%.

Same layout as the first capture: **wezterm fullscreen, mpv floating above —
the reporter's shape.** What is left of the 1.5× is the threshold: 22.6% of
composes went Full because the floating mpv covers ~0.6 of the output, and a
Full compose costs 1.8-2.6 ms here against 1.0 ms median. Retuning
`CLIPPED_REPAINT_MAX_FRACTION` (measured on bee) for this card is the next lever
for this workload; the walk CPU item is e16's.

**What this says about #131.** With step 1 a Full compose of this layout draws
one screen of pixels — wezterm minus mpv, plus mpv — where before it drew about
2.6 (root + wezterm + mpv). The reporter's case differs in one way: his
terminal repaints every frame (mpv's status line), so his damage is ~0.97 and
~all his composes are Full; the threshold cannot help him, only the Full-path
cost can — and that is what step 1 cut. The 100%-Full buckets here put a Full
compose at 1.8-2.6 ms at 2560×1440 on the RX 460; his pre-step-1 6.73 ms at 4K
on Polaris should fall by roughly the overdraw factor, to the 2.5-3 ms range.
His rerun on ≥ `a1da01bf` is what settles it.

## The reporter's rerun (#131, Polaris RX 570 @ 4K, build `c633a867`)

Terminal tiled to all but the panel, mpv floating, non-composited. His nvtop
GPU share for the display server, before → after this branch head, with Xorg
for reference:

| | mpv alone | + mpv status in the terminal |
|---|---|---|
| yserver, steps 0/3/4 (2026-09-02) | 12% | 44% |
| **yserver, step 1 + stage C** | **4%** | **22%** |
| Xorg | 3% | 10% |

His telemetry (`data/2026-09-03-issue131-reporter-*-telemetry.log`), same
shape as predicted from the z400:

| | mpv alone | + status |
|---|---|---|
| Full frames | 1.4% | **99.1%** (damage 1.0) |
| `avg_gpu_render_ns` per compose, median | 2.21 ms → **0.52 ms** | 6.73 ms → **3.05 ms** |
| `overdraw` | 1.00 | 1.00 |
| hidden participants per walk | 1.8 | 1.8 |
| walk per call / per second | 50 µs / 1.5 ms | 70 µs / 4.2 ms |

Both halves halved: the clipped case because the compose now draws only what
the damage touches of what is visible, the Full case because the compose
draws exactly one screen (`overdraw = 1.00`) instead of ~2.2 — the 6.73 → 3.05
ms ratio *is* the overdraw factor, as the z400 Full buckets predicted.

**What is left, and the one number that frames it:** 3.05 ms for one 4K
screen of opaque blit is **2.7 Gpix/s** on a card whose ROPs and memory bus
are each an order of magnitude above that. Xorg does the same frame for ~10%
GPU, i.e. roughly 1.7 ms. Two candidates, one free and one that must be
measured before it is argued:

1. **Blending is enabled on the opaque pipeline** (`vk/pipeline.rs:334`,
   `blend_enable(true)` with src-over factors; the shader forces `src.a = 1`
   so the result is an overwrite). The blend unit still reads the destination
   for every pixel. Disabling blending on the force-opaque variant is
   semantically identical and removes a destination read per pixel. Free to
   try; measure on the reporter's Full case.
2. **Render-target and source layout on GFX8.** Polaris lacks
   `VK_EXT_image_drm_format_modifier` (the z400 e16 log says so at startup),
   so yserver's scanout BO and every DRI3 client buffer it imports are
   LINEAR; Xorg/glamor and wlroots on the same card scan out implicitly
   *tiled* GBM buffers. ⚠ **This is the theory the campaign has excluded
   twice** ([[feedback_dont_revive_excluded_hypotheses]]); it was excluded
   for the RX 6800 fullscreen-game case (direct scanout, no compose) and on
   the argument "Xorg and labwc write the same buffers", which does not hold on
   a modifier-less GFX8 part. It is **not** to be re-argued; it is to be
   **measured** in one run: the damage audit composes the same frame into a
   private OPTIMAL-tiled image and times it
   (`YSERVER_DAMAGE_AUDIT=1`, `0a012146`), so audit µs vs production
   `avg_gpu_render_ns` on the same Full frames on his box — or the z400 —
   is a same-instrument A/B of layout cost. If it shows ~1×, the theory is
   dead for good; if ~2×, the remaining gap to Xorg on Polaris is structural
   to Vulkan without modifiers, which is a different conversation.

Everything else in his log is quiet: no warnings under `-C debug-assertions`,
`content_damage_off_output = 0`, the cap never collapsed (6-7 nodes per walk).

---

**Raw captures are not tracked** (2026-09-04): the `amdgpu_top --json` files
named above were committed in `02bafec3` and are recoverable from that commit,
but `docs/superpowers/findings/data/*.json` is now gitignored — a finding quotes
the numbers, the dump stays on the box that produced it. They cannot be taken
out of history: rewriting master would break every fork and contributor branch
built on it.
