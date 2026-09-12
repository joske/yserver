# Test status — latest numbers

Snapshot of the current xts5 (X Test Suite) and rendercheck (RENDER
smoke) pass rates. This file is the headline only; run-by-run history
and debugging notes live in [`xts-baseline.md`](xts-baseline.md) and
`status.md`.

## xts5 — full run #7, yserver/KMS bare-metal (bee/x86_64, 2026-09-12)

**4011 / 5987 test purposes PASS (67.0%)** — +18 vs run #6 (3993),
FAIL 813 → 802, UNRES 67 → 61. Results in
`xts/results/2026-09-12-09:40:39/` on bee.

Run #6 was on eiger, this one on bee. That is not a caveat: xts5 tests
protocol conformance, so the same server build should produce the same
verdicts on any machine. A number that moves between boxes without a
code change is our own non-determinism and is itself worth chasing —
not a reason to discount the delta.

Movers: Xlib11 +13, Xlib4 +11, Xlib9 / Xt11 +2, six suites +1.
Down: Xproto −5, XI −5, Xlib13 / Xt13 −2, XIproto −1 — all unexplained
and worth a look.

For context, Xorg itself only passes 77% of the test suite.

| scenario  | cases | tests | PASS | FAIL | UNRES | UNTST | UNSUP | NOTIU | Δ PASS |
|-----------|------:|------:|-----:|-----:|------:|------:|------:|------:|-------:|
| Xproto    |   122 |   389 |  356 |    9 |     3 |    19 |     2 |     0 |     −5 |
| Xlib3     |   109 |   162 |  108 |   18 |     2 |    21 |     6 |     1 |     +1 |
| Xlib4     |    29 |   324 |  182 |  103 |     3 |    20 |    11 |     5 |    +11 |
| Xlib5     |    15 |    84 |   60 |   17 |     0 |     5 |     2 |     0 |     +1 |
| Xlib6     |     8 |    50 |    7 |   14 |     0 |    29 |     0 |     0 |     +1 |
| Xlib7     |    58 |   172 |   87 |   27 |     0 |    13 |    45 |     0 |     +1 |
| Xlib8     |    29 |   165 |   92 |   37 |     4 |    22 |    10 |     0 |      0 |
| Xlib9     |    46 |  1472 |  835 |  374 |     0 |    36 |    23 |   201 |     +2 |
| Xlib10    |    23 |    95 |   25 |   37 |     4 |    28 |     1 |     0 |      0 |
| Xlib11    |    33 |   195 |   87 |   36 |     3 |     4 |    22 |    43 |    +13 |
| Xlib12    |    27 |   138 |   97 |   11 |     1 |    15 |     2 |    12 |     +1 |
| Xlib13    |    32 |   269 |  205 |   30 |    18 |    10 |     3 |     3 |     −2 |
| Xlib14    |    45 |    58 |   46 |    7 |     0 |     5 |     0 |     0 |      0 |
| Xlib15    |    45 |   159 |  125 |    1 |     0 |    33 |     0 |     0 |      0 |
| Xlib16    |    30 |   105 |   82 |    0 |     0 |    22 |     1 |     0 |      0 |
| Xlib17    |    55 |   131 |  102 |    8 |     0 |    21 |     0 |     0 |      0 |
| Xopen     |     8 |   127 |  122 |    3 |     0 |     0 |     2 |     0 |      0 |
| Xt3       |    21 |    73 |   73 |    0 |     0 |     0 |     0 |     0 |      0 |
| Xt4       |    33 |   192 |   94 |    0 |     0 |    98 |     0 |     0 |      0 |
| Xt5       |    10 |    69 |   26 |    0 |     0 |    41 |     0 |     0 |      0 |
| Xt6       |     7 |    71 |   67 |    4 |     0 |     0 |     0 |     0 |      0 |
| Xt7       |    11 |   106 |   96 |    1 |     0 |     6 |     0 |     3 |      0 |
| Xt8       |     7 |    43 |   35 |    4 |     0 |     4 |     0 |     0 |      0 |
| Xt9       |    33 |   189 |  122 |    2 |     8 |    55 |     2 |     0 |      0 |
| Xt10      |     8 |    17 |   16 |    0 |     0 |     1 |     0 |     0 |      0 |
| Xt11      |    58 |   285 |  248 |    1 |     0 |    34 |     0 |     0 |     +2 |
| Xt12      |    22 |    67 |   55 |    0 |     1 |    11 |     0 |     0 |      0 |
| Xt13      |    39 |   178 |  124 |    5 |     2 |    47 |     0 |     0 |     −2 |
| Xt14      |     2 |    18 |   18 |    0 |     0 |     0 |     0 |     0 |      0 |
| Xt15      |     1 |     2 |    0 |    0 |     0 |     0 |     2 |     0 |      0 |
| XtC       |    29 |   147 |   88 |    0 |     2 |    56 |     1 |     0 |      0 |
| XtE       |     1 |     1 |    1 |    0 |     0 |     0 |     0 |     0 |      0 |
| ShapeExt  |    11 |    11 |   11 |    0 |     0 |     0 |     0 |     0 |      0 |
| XI        |    36 |   316 |  217 |   51 |    10 |    31 |     2 |     5 |     −5 |
| XIproto   |    35 |   107 |  102 |    2 |     0 |     3 |     0 |     0 |     −1 |
| **total** | **1078** | **5987** | **4011** | **802** | **61** | **690** | **137** | **273** | **+18** |

ShapeExt, Xlib16 and Xt3/4/5/10/14/XtE are fully clean (zero
FAIL/UNRES). 2 NORESULTs, unchanged.

Note on what xts5 can and cannot show: the #141 fix (an XI2 selection
absorbs the core press, so core propagation stops there) cannot move a
single number here. xts5 has **no XI2 at all** — its `XI` and `XIproto`
suites are XInput 1.x (`XOpenDevice`, `AllowDeviceEvents`,
`ChangeDeviceKeyMapping`). The same was true of the earlier e27 work.
Both were caught instead by `tools/replay-propagation-probe.c` under
`tools/vng-scenarios/replay-propagation.sh`, which diffs the same probe
binary against Xorg and yserver in one harness.

The XI bucket is spread thin rather than concentrated: 51 FAIL across
~20 files, led by `XSelectExtensionEvent` 7, `ChangeKeyboardDevice` 6,
`ChangePointerDevice` 5, `AllowDeviceEvents` 5, `GrabDeviceKey` 4.
Beware of ranking these by report-line volume —
`ChangeDeviceKeyMapping` emits 612 keysym lines from just 2 FAILs.
Delivery-shaped reports are a small minority: "not delivered" 15, "too
many events sent" 2, "incorrectly delivered" 2.

Largest FAIL buckets / next targets:
1. **Xlib9 (374)** — remaining drawing/GetImage content semantics.
   Biggest single bucket by a wide margin.
2. **Xlib4 (103)** — depth-mismatch BadMatch (CWBorderPixmap parser
   needed), colormap visual-type checks, bit-gravity pixel cluster,
   stacking-order pixel checks, BadAccess event-mask conflicts.
3. **XI (51)** — XInput-1.x device functions, now the third bucket.
4. **Xlib8 (37)** / **Xlib10 (37)** — events / colormap sections.
5. **Xlib11 (36)** — residual grab semantics, down from 49.

Previous full runs:
- #6 — 2026-06-25 (eiger, aarch64): 3993/5987 PASS (66.7%); results on
  that box.
- #5 — 2026-06-07 22:03:01 (bee, HW): 3961/5987 PASS (66.2%) —
  `xts/results/2026-06-07-22:03:01/`.
- #4 — 2026-06-07 17:14:17 (bee, HW): 3747/5987 PASS (62.6%) —
  `xts/results/2026-06-07-17:14:17/`. Last run before the Xlib4
  BadX work and the desktop-input-fixes branch.
- #3 — 2026-06-06 (air, M1): 3419/5987 PASS (57.1%) —
  `xts/results/2026-06-06-20:26:54/`.
- #2 — 2026-06-05 (M2) + 2026-06-06 air XI row: 3370/5987 PASS (56.3%)
  — `xts/results/2026-06-05-13:20:07/` (+ `2026-06-06-00:58:03` for XI).
- #1 — 2026-06-04 (first ever to complete): 2784/5987 PASS (46.5%) —
  `xts/results/2026-06-04-15:48:44/`.

Aborted run between #3 and #4 (`xts/results/2026-06-07-14:01:34/`):
2999/5987 PASS, 1290 UNRES — the GetImage BadMatch cascade caused
by an unguarded `XConfigureWindow` on the root window, fixed by
`77f785b` before run #4.

## rendercheck — bare-metal 2026-06-04, rendercheck 1.6, 900 s/test

| category    |  PASS | TOTAL |
|-------------|------:|------:|
| fill        |    64 |    64 |
| dcoords     |     2 |     2 |
| scoords     |     1 |     1 |
| mcoords     |     1 |     1 |
| tscoords    |     2 |     2 |
| tmcoords    |     2 |     2 |
| blend       |     5 |     5 |
| composite   |     5 |     5 |
| cacomposite |     5 |     5 |
| gradients   |  6081 |  6081 |
| repeat      |   380 |   380 |
| triangles   |   570 |   570 |
| bug7366     |     1 |     1 |
| **total**   | **7119** | **7119** |

**100% pass.**

> Use rendercheck ≥ 1.6. Version 1.5 has a bug in
> `gradients::render_to_gradient_test` that trips even against the
> host X server.
