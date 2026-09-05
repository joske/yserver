# amdgpu_top on yserver, composited Cinnamon — and why it does not size the prize

**Status: the absolute size of the prize is unresolved. This file corrects an
earlier version of itself that claimed the 13-28% figure was refuted; it is not
refuted, but it is also not confirmed, and the two candidate explanations for
the discrepancy differ by 25×.**

Data: `data/2026-09-02-yserver-cinnamon-mpv-rx460.json` (48 samples, yserver +
Cinnamon, windowed mpv, z400 + RX 460, 2560×1440) against
`data/2026-09-01-labwc-baseline-rx460.json` (20 samples, labwc, same box, same
clip).

## What the capture shows

`amdgpu_top --json` per-process `fdinfo` GFX share — the kernel's accounting of
engine-busy time per context.

| run | process | mean GFX | mean CPU |
|---|---|---|---|
| labwc | labwc | **2.80%** | 3.7% |
| labwc | mpv | 3.94% | 39.8% |
| yserver + Cinnamon | cinnamon (muffin) | **6.54%** | 6.0% |
| yserver + Cinnamon | **yserver** | **1.88%** | 7.5% |
| yserver + Cinnamon | mpv | 3.36% | 42.6% |

Per-process shares sum consistently with device-wide GRBM Graphics Pipe in both
runs (11.8 vs 14.5; 6.7 vs 9.6), so the attribution is credible. Neither run was
clock-limited (`GFX_SCLK` 1099 and 1129 MHz against a 1180 MHz cap). yserver's
share is flat at 2% across 42 consecutive samples.

## Why 1.88% is not a compose measurement

**yserver was barely compositing.** The matching Cinnamon session log
(`yserver-hw-cinnamon.log`, same box and resolution) records **2246 `scanout_m2:
live direct submit`** events and `avg_gpu_render_ns=0` in 64 of 67 telemetry
buckets. Direct scanout is flipping muffin's buffer to the CRTC; yserver's
compose path is idle. So 1.88% is the cost of *presenting*, not of compositing,
and it says nothing about the non-composited compose the project targets.

That is good news on its own terms: direct compositor scanout is landed and
working on the target hardware.

## The real discrepancy, which remains open

The three buckets in that same log where yserver *did* compose report
`avg_gpu_render_ns` = **148, 173, 223 µs**. The non-composited MATE run on the
same box at the same resolution reports a median of **4.69 ms** (n=106, range
3.76-6.33 ms).

Same box, same resolution, same destination buffer, same card. They differ by
**25×**.

Everything about the *destination* is held constant between those two numbers, so
the difference is in what the composes do, not what they write into. Two
candidate explanations remain, and they imply very different projects:

1. **Overdraw and draw count.** Composited emits one COW quad; non-composited
   MATE emits the root plus every window and panel. If the true cost scales with
   *covered* pixels, tight damage attacks it directly and the prize is real.
   But plausible overdraw on a MATE desktop is 2-3×, not 25×, so this does not
   obviously account for the gap on its own.
2. **Engine contention inside the timestamp bracket.**
   `cmd_write_timestamp(TOP_OF_PIPE)` → `BOTTOM_OF_PIPE` measures elapsed time
   on the engine, and mpv is decoding and rendering on that same engine. The
   three composited samples may simply have been taken while mpv was idle. If
   this is the dominant term, `avg_gpu_render_ns` is a latency figure being read
   as utilisation, the 13-28% is inflated, and the prize is much smaller.

Nothing in the data on hand separates these.

## What was wrong in the first version of this file

- It claimed amdgpu_top and our telemetry had been pointed at the same workload
  and disagreed by 10×. They had not: the amdgpu_top run had no composes.
- It argued from total memory bandwidth that 4.7 ms per compose is physically
  implausible. Byte count does not bound a compose, so that argument establishes
  nothing and has been dropped. The 25× comparison above replaces it and is
  better evidence anyway, because it holds the destination constant instead of
  reasoning about it.

What survives from it: **the instrument mismatch is real.** The 13-28% came from
`avg_gpu_render_ns`; labwc's 2.80% came from amdgpu_top. Those were compared as
though they were one instrument, and that comparison is not evidence either way.

## What is still solidly measured

- **The relative saving.** The audit A/Bs a clipped compose against a full one in
  the same run, on the same instrument, under the same contention: 521 µs → 250 µs
  on MATE, 443 µs → 233 µs on Window Maker (see
  `2026-09-01-damage-completeness-audit.md`). Ratios transfer even when absolutes
  do not, so "clipping roughly halves the compose, and tight damage should do far
  better than half" stands.
- **Damage is already tight**: `mean_damage = 0.078` over 8999 clean partial
  comparisons.
- **The composited X11 stack costs 3.0× labwc** — muffin 6.54 + yserver 1.88 =
  8.4% against 2.80% — of which muffin is 78%. Composited X11 pays for two
  composites and yserver's half is already handled by direct scanout. This is why
  the project targets non-composited.

## The contention hypothesis is weak — the MATE log already argues against it

If the timestamp bracket were absorbing other contexts' engine time, reported
per-compose time would rise with engine load. In the 105 telemetry buckets of
`yserver-hw-mate.log` (non-composited, z400) it **falls**:

| engine load (paint+composite submits/s) | n | median per-compose | composes/s | implied GPU |
|---|---|---|---|---|
| < 50 | 8 | 4.78 ms | 4 | 1.9% |
| 50-300 | 68 | 5.56 ms | 27 | **15.0%** |
| 300-1200 | 17 | 4.25 ms | 49 | **20.8%** |
| > 1200 | 12 | 3.64 ms | 54 | **19.8%** |

`corr(per-compose, paint_submits/s) = −0.50`, `corr(per-compose,
composite_submits/s) = −0.60`. Both negative, so heavier concurrent work makes
each compose *cheaper*, not dearer — the opposite of what contention pollution
predicts. The 1.5× spread across the bins is the size and direction expected from
clock ramping (this card runs 214-1180 MHz), not from queue interference.

And across every loaded bin the implied share converges on **15-21%**, against
labwc's 2.80% total. That is the 5-7× the project was premised on, reached by a
route that does not depend on the disputed arithmetic.

So hypothesis 2 is largely out, and hypothesis 1 — cost scaling with what the
composes actually draw — is what remains. Note what that implies: the 25× gap
between one full-screen quad and ~35 draws says the cost lives in per-draw
coverage, which is exactly what step 4 removes, by clipping the rasterised area
*and* culling draws outside it. The gap is an argument for the plan, not against
it.

This is an internal-consistency check on our own instrument, not a cross-check
against an independent one. It shows the bracket behaves sensibly under load; it
does not validate its absolute scale.

## What is worth running, and what is not

**Not worth running: the instrument cross-check on a fast GPU.** On an RX 6800 a
1440p compose costs a couple of hundred microseconds, so yserver's share lands
around 1-2%. Both instruments would report 1-2%, and the question being asked —
do they agree within a factor of a few — is not answerable when the quantity sits
that close to the measurement's granularity. Loading the box up synthetically
(many windows, 5120×1440, uncapped compose rate) would push the share into a
readable band, but that is a fabricated workload standing in for a real one, and
it is a lot of setup for a question the table above has already largely answered.

**Worth running, whenever the z400 is next reachable — it is a two-minute
capture, not a campaign.** `amdgpu_top --json` per-process GFX for
non-composited yserver + windowed mpv. Directly comparable to labwc's 2.80% and
to BergmannAtmet's reports, and at 15-21% the signal is far above granularity.

**Worth knowing: step 4 does not depend on that number.** Its acceptance can be
same-instrument before/after on whatever box is available, plus the audit's
clipped-versus-full A/B, which is already same-run and same-instrument and
therefore valid anywhere (521 → 250 µs on MATE, bee). The absolute share decides
*how much the project is worth*, not *whether the change works*.

So: the prize is probable rather than proven, and proving it waits on hardware
rather than on more analysis. Either way, start with
banded region type — *step 0* of
`../plans/2026-09-01-damage-derived-scene-repaint-plan.md`: a y-x banded region
with real union / subtract / intersect, because today's `RegionSet` subtracts by
exact rect match and cannot express the per-BO bookkeeping. It is required under
every outcome above, it is testable against a brute-force oracle with no
hardware, and it risks nothing if the sizing comes back small.

---

**Raw captures are not tracked** (2026-09-04): the `amdgpu_top --json` files
named above were committed in `02bafec3` and are recoverable from that commit,
but `docs/superpowers/findings/data/*.json` is now gitignored — a finding quotes
the numbers, the dump stays on the box that produced it. They cannot be taken
out of history: rewriting master would break every fork and contributor branch
built on it.
