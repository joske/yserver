#!/usr/bin/env bash
# Host half of pointer-scale.sh: per phase, moves the guest's PS/2 mouse by a
# fixed relative delta through the QEMU monitor.
#   tools/vng-scenarios/pointer-scale-host.sh yserver|xorg
set -euo pipefail
server=${1:?usage: pointer-scale-host.sh yserver|xorg}
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
name=pointer-scale-$server
out=$repo/target/vng/$name
rm -rf "$out"
"$repo/tools/vng-shot.sh" --server "$server" --dump none --hold 20 --name "$name" \
    --scenario "$repo/tools/vng-scenarios/pointer-scale.sh" &
shot=$!
mon=$out/monitor.sock
for p in identity scale2; do
    for _ in $(seq 1 600); do
        [ -e "$out/READY-$p" ] && break
        kill -0 "$shot" 2>/dev/null || { wait "$shot"; exit 1; }
        sleep 0.5
    done
    for _ in 1 2 3 4; do "$repo/tools/qemu-monitor.py" "$mon" "mouse_move 25 10"; sleep 0.2; done
    touch "$out/DONE-$p"
done
wait "$shot"
cat "$out/pointer.log"
