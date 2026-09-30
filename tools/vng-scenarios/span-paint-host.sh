#!/usr/bin/env bash
# Host half of span-paint.sh: dumps the scanout at the end of every round and
# checks each output shows only that round's colour (the pointer, at 5,5 on
# the first output, is masked).
#   tools/vng-scenarios/span-paint-host.sh [name] [vng-shot args...]
set -euo pipefail
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
name=${1:-span-paint}
shift || true
out=$repo/target/vng/$name
rounds=${SPAN_ROUNDS:-10}
rm -rf "$out"
"$repo/tools/vng-shot.sh" --outputs 2 --dump none --name "$name" --settle 0 --timeout 900 \
    --env SPAN_ROUNDS="$rounds" "$@" \
    --scenario "$repo/tools/vng-scenarios/span-paint.sh" > /dev/null &
shot=$!
mon=$out/monitor.sock
for r in $(seq 0 $((rounds - 1))); do
    for _ in $(seq 1 600); do
        [ -e "$out/READY-$r" ] && break
        kill -0 "$shot" 2>/dev/null || { wait "$shot"; exit 1; }
        sleep 0.5
    done
    "$repo/tools/qemu-monitor.py" "$mon" "sendkey ctrl-alt-ret"
    for _ in $(seq 1 40); do
        [ "$(compgen -G "$out/yserver-scanout-*.ppm" | wc -l || true)" -ge 2 ] && break
        sleep 0.25
    done
    sleep 0.5
    for o in 0 1; do
        mv "$(ls -t "$out"/yserver-scanout-*-out$o-*.ppm | head -1)" "$out/scanout-$r-out$o.ppm"
    done
    touch "$out/DONE-$r"
done
wait "$shot"
python3 - "$out" "$rounds" <<'PY'
import sys
from PIL import Image
out, rounds = sys.argv[1], int(sys.argv[2])
stale = 0
for r in range(rounds):
    c = int(open(f"{out}/expect-{r}").read(), 16)
    want = (c >> 16, (c >> 8) & 255, c & 255)
    line = []
    for o in (0, 1):
        img = Image.open(f"{out}/scanout-{r}-out{o}.ppm").convert("RGB")
        p = img.load()
        w, h = img.size
        bad = sum(p[x, y] != want for y in range(h) for x in range(w)
                  if not (o == 0 and x < 64 and y < 64))
        line.append(f"out{o} {bad}")
        stale += bad > 0
    print(f"round {r}: wrong pixels " + ", ".join(line))
print(f"{stale} stale output(s) in {rounds} rounds")
sys.exit(1 if stale else 0)
PY
