#!/usr/bin/env bash
# Host half of pixmap-pool.sh: runs it with resource telemetry on, then
# checks the pool's residency in yserver.log with pixmap-pool-check.py.
#   tools/vng-scenarios/pixmap-pool-host.sh [name] [vng-shot args...]
set -euo pipefail
here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd -- "$here/../.." && pwd)
name=${1:-pixmap-pool}
shift || true
out=${VNG_OUT:-$repo/target/vng}/$name
rm -rf "$out"
"$repo/tools/vng-shot.sh" --dump none --name "$name" --settle 0 --timeout 500 \
    --env YSERVER_LOOP_TELEMETRY=1 --log warn,yserver::resources=info "$@" \
    --scenario "$here/pixmap-pool.sh" > /dev/null
python3 "$here/pixmap-pool-check.py" "$out"
