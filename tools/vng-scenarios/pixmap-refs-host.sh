#!/usr/bin/env bash
# Host half of pixmap-refs.sh: runs it with resource telemetry on, then checks
# the live-pixmap counter in yserver.log with pixmap-refs-check.py.
#   tools/vng-scenarios/pixmap-refs-host.sh [name] [vng-shot args...]
set -euo pipefail
here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd -- "$here/../.." && pwd)
name=${1:-pixmap-refs}
shift || true
out=${VNG_OUT:-$repo/target/vng}/$name
rm -rf "$out"
"$repo/tools/vng-shot.sh" --dump none --name "$name" --settle 0 --timeout 400 \
    --env YSERVER_LOOP_TELEMETRY=1 --log warn,yserver::resources=info "$@" \
    --scenario "$here/pixmap-refs.sh" > /dev/null
python3 "$here/pixmap-refs-check.py" "$out"
