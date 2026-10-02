#!/usr/bin/env python3
"""Host check for pixmap-refs.sh: the live `pixmap=` count in yserver's
`vram by use` telemetry, sampled in the quiet gap after each probe variant.
Exit 1 if any variant leaves more live pixmaps than it found.

  pixmap-refs-check.py <artifact-dir>
"""
import datetime
import re
import sys

SLACK = 2  # server-internal pixmaps may come and go

out = sys.argv[1]
phases = []
for line in open(f"{out}/probe.log"):
    m = re.match(r"PHASE (\w+) (start|end) ([\d.]+)", line)
    if m and m[2] == "end":
        phases.append((m[1], float(m[3])))

samples = []
for line in open(f"{out}/yserver.log", errors="replace"):
    m = re.match(r"\[(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d)Z .*vram by use.* pixmap=[\d.]+MiB/(\d+)", line)
    if m:
        t = datetime.datetime.fromisoformat(m[1]).replace(tzinfo=datetime.timezone.utc)
        samples.append((t.timestamp(), int(m[2])))


def settled(end):
    # Rows are stamped truncated and written within the next second; the
    # probe sleeps 4 s after each phase, so these were written in its gap.
    rows = [n for t, n in samples if end + 1.5 <= t <= end + 2.5]
    return rows[-1] if rows else None


fail = []
prev = None
for name, end in phases:
    n = settled(end)
    if n is None:
        fail.append(f"{name}: no telemetry sample after it")
        continue
    delta = "" if prev is None else f" ({n - prev:+d})"
    print(f"{name:11} live pixmaps after: {n}{delta}")
    if prev is not None and n - prev > SLACK:
        fail.append(f"{name} left {n - prev} pixmaps behind")
    prev = n
if [p for p, _ in phases] != ["baseline", "clip", "clip_rects", "tile", "stipple",
                              "copy_gc", "free_gc", "disconnect", "cursor_free",
                              "cursor_window", "cursor_destroy", "cursor_grab",
                              "cursor_anim", "cursor_disconnect"]:
    fail.append(f"probe phases {[p for p, _ in phases]}")
for f in fail:
    print(f"FAIL: {f}")
sys.exit(1 if fail else 0)
