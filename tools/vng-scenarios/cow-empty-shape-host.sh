#!/usr/bin/env bash
# Host half of cow-empty-shape.sh: dumps every output after each probe phase
# and checks it shows only that phase's colour (the pointer, at 5,5 on the
# first output, is masked). The first phase must be on the direct-scanout
# path, or the scenario proves nothing about it.
#   tools/vng-scenarios/cow-empty-shape-host.sh [name] [vng-shot args...]
set -euo pipefail
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
name=${1:-cow-empty-shape}
shift || true
out=${VNG_OUT:-$repo/target/vng}/$name
rm -rf "$out"
"$repo/tools/vng-shot.sh" --outputs 2 --dump none --name "$name" --settle 0 --timeout 400 "$@" \
    --scenario "$repo/tools/vng-scenarios/cow-empty-shape.sh" > /dev/null &
shot=$!
mon=$out/monitor.sock
n=0
while :; do
    for _ in $(seq 1 600); do
        [ -e "$out/READY-$n" ] && break
        kill -0 "$shot" 2>/dev/null || break
        sleep 0.5
    done
    [ -e "$out/READY-$n" ] || break
    "$repo/tools/qemu-monitor.py" "$mon" "sendkey ctrl-alt-ret"
    for _ in $(seq 1 40); do
        [ "$(compgen -G "$out/yserver-scanout-*.ppm" | wc -l || true)" -ge 2 ] && break
        sleep 0.25
    done
    sleep 0.5
    for o in 0 1; do
        f=$(ls -t "$out"/yserver-scanout-*-out$o-*.ppm 2>/dev/null | head -1 || true)
        [ -z "$f" ] || mv "$f" "$out/scanout-$n-out$o-${f##*-out$o-}"
    done
    touch "$out/DONE-$n"
    n=$((n + 1))
done
wait "$shot"
python3 - "$out" "$n" <<'PY'
import glob, sys
from PIL import Image
out, phases = sys.argv[1], int(sys.argv[2])
bad_phases = 0
if phases != 3:
    print(f"{phases} of 3 phases dumped")
    bad_phases += 1
for n in range(phases):
    c = int(open(f"{out}/expect-{n}").read(), 16)
    want = (c >> 16, (c >> 8) & 255, c & 255)
    line = []
    for o in (0, 1):
        files = glob.glob(f"{out}/scanout-{n}-out{o}-*.ppm")
        if not files:
            line.append(f"out{o} no dump")
            bad_phases += 1
            continue
        origin = files[0].split(f"-out{o}-", 1)[1][:-4]
        img = Image.open(files[0]).convert("RGB")
        p = img.load()
        w, h = img.size
        bad = sum(p[x, y] != want for y in range(h) for x in range(w)
                  if not (o == 0 and x < 64 and y < 64))
        if n == 0 and not origin.startswith("direct-src"):
            line.append(f"out{o} {bad} ({origin}: NOT direct scanout)")
            bad_phases += 1
            continue
        line.append(f"out{o} {bad} ({origin})")
        bad_phases += bad > 0
    print(f"phase {n} expect {c:06x}: wrong pixels " + ", ".join(line))
sys.exit(1 if bad_phases else 0)
PY
