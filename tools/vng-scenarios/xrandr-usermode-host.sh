#!/usr/bin/env bash
# Host half of xrandr-usermode.sh: while the user mode is current, presses
# yserver's Ctrl+Alt+Enter scanout dump and checks it is the mode's 1000x700.
# Under Xorg (no dump) it only releases the guest.
#   tools/vng-scenarios/xrandr-usermode-host.sh [name] [vng-shot args...]   # e.g. --server xorg
set -euo pipefail
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
name=${1:-xrandr-usermode}
shift || true
out=${VNG_OUT:-$repo/target/vng}/$name
server=yserver
prev=
for arg in "$@"; do
    [ "$prev" != --server ] || server=$arg
    prev=$arg
done
rm -rf "$out"
"$repo/tools/vng-shot.sh" --dump none --name "$name" "$@" \
    --scenario "$repo/tools/vng-scenarios/xrandr-usermode.sh" &
shot=$!
mon=$out/monitor.sock
for _ in $(seq 1 600); do
    [ -e "$out/READY-usermode" ] && break
    kill -0 "$shot" 2>/dev/null || { wait "$shot"; exit 1; }
    sleep 0.5
done
if [ "$server" = yserver ]; then
    "$repo/tools/qemu-monitor.py" "$mon" "sendkey ctrl-alt-ret"
    for _ in $(seq 1 40); do
        compgen -G "$out/yserver-scanout-*.ppm" > /dev/null && break
        sleep 0.25
    done
fi
touch "$out/DONE-usermode"
wait "$shot"
[ "$server" = yserver ] || exit 0
python3 - "$out" <<'PY'
import glob, sys
from PIL import Image
files = glob.glob(f"{sys.argv[1]}/yserver-scanout-*.ppm")
if not files:
    sys.exit("no scanout dump")
size = Image.open(files[0]).size
print(f"scanout {size[0]}x{size[1]} while the user mode is current")
sys.exit(0 if size == (1000, 700) else 1)
PY
