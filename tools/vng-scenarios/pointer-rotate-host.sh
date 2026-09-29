#!/usr/bin/env bash
# Host half of pointer-rotate.sh: per phase, moves the guest's PS/2 mouse by a
# fixed relative delta through the QEMU monitor.
#   tools/vng-scenarios/pointer-rotate-host.sh yserver|xorg
set -euo pipefail
server=${1:?usage: pointer-rotate-host.sh yserver|xorg}
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
name=pointer-rotate-$server
out=$repo/target/vng/$name
rm -rf "$out"
"$repo/tools/vng-shot.sh" --server "$server" --dump none --hold 30 --name "$name" \
    --scenario "$repo/tools/vng-scenarios/pointer-rotate.sh" &
shot=$!
mon=$out/monitor.sock
for p in normal left right inverted reflectx; do
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
