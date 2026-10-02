#!/usr/bin/env python3
"""Host check for pixmap-pool.sh (#196): the pixmap pool's residency over
the probe's phases, from yserver's 1 Hz resource telemetry. Prints per-phase
pool hits/misses and pool-sized allocations, then exits 1 unless
  - pool_idle never exceeds the 64 MiB budget,
  - nothing is evicted during the steady phase (the working set survives),
  - the pool is empty by the end of the idle phase.

  pixmap-pool-check.py <artifact-dir>
"""
import datetime
import re
import sys

BUDGET_MIB = 64.0

out = sys.argv[1]
phases = {}
for line in open(f"{out}/probe.log"):
    m = re.match(r"PHASE (\w+) (start|end) ([\d.]+)", line)
    if m:
        phases.setdefault(m[1], {})[m[2]] = float(m[3])

def stamp(line):
    m = re.match(r"\[(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d)Z ", line)
    if not m:
        return None
    t = datetime.datetime.fromisoformat(m[1]).replace(tzinfo=datetime.timezone.utc)
    return t.timestamp()

idle, churn, live = [], [], []
for line in open(f"{out}/yserver.log", errors="replace"):
    t = stamp(line)
    if t is None:
        continue
    if m := re.search(r"pool_idle=([\d.]+)MiB/(\d+)", line):
        idle.append((t, float(m[1]), int(m[2])))
    elif "vram churn [1s]" in line:
        a = re.search(r"pixmap_small\[alloc=(\d+)/s", line)
        p = re.search(r"pixmap\[hit=(\d+)/s miss=(\d+)/s", line)
        if a and p:
            churn.append((t, int(a[1]), int(p[1]), int(p[2])))
    elif m := re.search(r"evicted_budget_total=(\d+) evicted_idle_total=(\d+)", line):
        live.append((t, int(m[1]), int(m[2])))

# A row stamped t (truncated) was written in [t, t + 1) and covers the second
# before that. The probe sleeps 3 s between steady and burst and the idle
# phase is quiet, so splitting 1.5 s after each phase's end is unambiguous.
def window(name):
    ends = {"steady": phases.get("steady", {}).get("start", 0),
            "burst": phases.get("steady", {}).get("end", 0) + 1.5,
            "idle": phases.get("burst", {}).get("end", 0) + 1.5}
    hi = {"steady": ends["burst"], "burst": ends["idle"],
          "idle": phases.get("idle", {}).get("end", 0) + 1.5}
    return ends[name], hi[name]


def within(rows, name, skip=0.0):
    lo, hi = window(name)
    return [r for r in rows if lo + skip <= r[0] < hi]


fail = []
if set(phases) != {"steady", "burst", "idle"}:
    fail.append(f"probe phases {sorted(phases)}")
# The first 5 s fill a cold pool; after that misses are what eviction costs.
for name, skip in (("steady", 5.0), ("burst", 0.0), ("idle", 0.0)):
    rows = within(churn, name, skip)
    allocs, hits, misses = (sum(r[i] for r in rows) for i in (1, 2, 3))
    peak = max((r[1] for r in within(idle, name)), default=0.0)
    rate = 100.0 * hits / (hits + misses) if hits + misses else 0.0
    print(f"{name:7} {len(rows):3}s: pool hit={hits} miss={misses} ({rate:.2f}% hits) "
          f"pixmap_small allocs={allocs} | peak pool_idle={peak:.1f}MiB")
end = within(idle, "idle")
if end:
    print(f"pool_idle at idle end: {end[-1][1]:.1f}MiB/{end[-1][2]} entries")
peak = max((r[1] for r in idle), default=0.0)
if peak > BUDGET_MIB:
    fail.append(f"pool_idle peaked at {peak:.1f} MiB > {BUDGET_MIB:.0f} MiB budget")
steady = within(live, "steady")
if not steady:
    fail.append("no eviction counters in the log (master build?)")
elif steady[-1][1:] != steady[0][1:]:
    fail.append(f"evictions during the steady phase: {steady[0][1:]} -> {steady[-1][1:]}")
if not end or end[-1][2] != 0:
    fail.append("the pool did not drain by the end of the idle phase")
for f in fail:
    print(f"FAIL: {f}")
sys.exit(1 if fail else 0)
