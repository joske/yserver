#!/usr/bin/env bash
# Host half of the issue #173 vng A/B (see xi2-raw-keys.sh): boots the guest
# on yserver or Xorg, waits for the in-guest raw-key listener, then presses
# real keys on the guest's emulated PS/2 keyboard through the QEMU monitor.
#
#   KERNEL=/usr/lib/modules/$(uname -r)/vmlinuz \
#       tools/vng-scenarios/xi2-raw-keys-host.sh yserver|xorg
#
# Artifacts: target/vng/xi2-raw-keys-<server>/{xtest,physical,devices}.log
set -euo pipefail
server=${1:?usage: xi2-raw-keys-host.sh yserver|xorg}
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
name=xi2-raw-keys-$server
out=$repo/target/vng/$name

# vng-shot recreates $out, but only once it gets going: a previous run's
# LISTENING would otherwise send the keys before this guest is listening.
rm -rf "$out"
"$repo/tools/vng-shot.sh" --server "$server" --dump none --hold 30 --name "$name" \
    --scenario "$repo/tools/vng-scenarios/xi2-raw-keys.sh" &
shot=$!

for _ in $(seq 1 600); do
    [ -e "$out/LISTENING" ] && break
    kill -0 "$shot" 2>/dev/null || { wait "$shot"; exit 1; }
    sleep 0.5
done
[ -e "$out/LISTENING" ] || { echo "xi2-raw-keys-host: guest listener never started" >&2; exit 1; }

mon=$out/monitor.sock
send() { "$repo/tools/qemu-monitor.py" "$mon" "$@"; sleep 1; }
send "sendkey a"
send "sendkey f5"
send "sendkey shift-a"
# Held past the auto-repeat delay: repeats must not produce raw events.
send "sendkey b 1500"
sleep 1
wait "$shot"
echo "== $out/physical.log"
cat "$out/physical.log"
