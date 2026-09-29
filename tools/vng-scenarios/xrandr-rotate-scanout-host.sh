#!/usr/bin/env bash
# Host half of xrandr-rotate-scanout.sh: per step, presses yserver's
# Ctrl+Alt+Enter scanout dump, then checks each dump against the root capture
# pushed through the rotation on the CPU.
#   tools/vng-scenarios/xrandr-rotate-scanout-host.sh
set -euo pipefail
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
name=rotate-scanout
out=$repo/target/vng/$name
rm -rf "$out"
"$repo/tools/vng-shot.sh" --dump none --name "$name" --timeout 900 \
    --scenario "$repo/tools/vng-scenarios/xrandr-rotate-scanout.sh" &
shot=$!
mon=$out/monitor.sock
steps=(normal left right inverted reflect-x reflect-y left-scale2)
for tag in "${steps[@]}"; do
    for _ in $(seq 1 600); do
        [ -e "$out/READY-$tag" ] && break
        kill -0 "$shot" 2>/dev/null || { wait "$shot"; exit 1; }
        sleep 0.5
    done
    before=$(compgen -G "$out/yserver-scanout-*.ppm" | wc -l || true)
    "$repo/tools/qemu-monitor.py" "$mon" "sendkey ctrl-alt-ret"
    for _ in $(seq 1 40); do
        [ "$(compgen -G "$out/yserver-scanout-*.ppm" | wc -l || true)" -gt "$before" ] && break
        sleep 0.25
    done
    ppm=$(ls -t "$out"/yserver-scanout-*.ppm | head -1)
    mv "$ppm" "$out/scanout-$tag.ppm"
    touch "$out/DONE-$tag"
done
wait "$shot"
# Scanout pixel (dx, dy) of a W×H mode shows root pixel M·(d + ½), nearest
# (RRTransformCompute, rrtransform.c:167-253).
python3 - "$out" <<'PY'
import sys
from PIL import Image
out = sys.argv[1]
maps = {
    "normal": lambda x, y, w, h: (x, y),
    "left": lambda x, y, w, h: (h - 1 - y, x),
    "right": lambda x, y, w, h: (y, w - 1 - x),
    "inverted": lambda x, y, w, h: (w - 1 - x, h - 1 - y),
    "reflect-x": lambda x, y, w, h: (w - 1 - x, y),
    "reflect-y": lambda x, y, w, h: (x, h - 1 - y),
    "left-scale2": lambda x, y, w, h: (2 * h - 2 - 2 * y, 2 * x),
}
for tag, m in maps.items():
    scan = Image.open(f"{out}/scanout-{tag}.ppm").convert("RGB")
    root = Image.open(f"{out}/root-{tag}.png").convert("RGB")
    w, h = scan.size
    sp, rp = scan.load(), root.load()
    bad = sum(sp[x, y] != rp[m(x, y, w, h)] for y in range(h) for x in range(w))
    print(f"{tag}: scanout {w}x{h}, root {root.size[0]}x{root.size[1]}, differing pixels: {bad}")
PY
