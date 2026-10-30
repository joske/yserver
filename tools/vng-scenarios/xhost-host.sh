#!/usr/bin/env bash
# Host half of xhost.sh: boots the server with -auth and a cookie that only
# the scenario's user holds, plus the root hook that adds a second user.
#   tools/vng-scenarios/xhost-host.sh [name] [vng-shot args...]
# XHOST_LISTEN_TCP=1 adds -listen tcp (yserver needs the tcp-transport build).
set -euo pipefail
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
name=${1:-xhost}
shift || true
# Outside the artifacts (vng-shot recreates them); the guest sees it read-only.
auth_dir=$(mktemp -d "${TMPDIR:-/tmp}/vng-xauth.XXXXXX")
trap 'rm -rf "$auth_dir"' EXIT
auth=$auth_dir/xauth
# One FamilyWild record (no address, display 7) so :7 and 127.0.0.1:7 both match.
{ printf '\xff\xff\x00\x00\x00\x017\x00\x12MIT-MAGIC-COOKIE-1\x00\x10'; head -c 16 /dev/urandom; } > "$auth"
declare -a tcp=()
[ "${XHOST_LISTEN_TCP:-}" != 1 ] || tcp=(--server-arg -listen --server-arg tcp)
"$repo/tools/vng-shot.sh" --dump none --name "$name" "$@" \
    --server-arg -auth --server-arg "$auth" ${tcp[@]+"${tcp[@]}"} \
    --env XAUTHORITY="$auth" \
    --root-hook "$repo/tools/vng-scenarios/xhost-root.sh" \
    --scenario "$repo/tools/vng-scenarios/xhost.sh"
