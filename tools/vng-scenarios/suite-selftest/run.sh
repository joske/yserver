#!/usr/bin/env bash
# Self-test of tools/vng-suite.sh: a failing assertion, a crashed server, a
# hang past its timeout and a SIGINT to the runner must each be reported as a
# failure and leave no process behind; a guest skip is a skip and exits 0.
#   tools/vng-scenarios/suite-selftest/run.sh [gpu] [vng-suite args...]
set -euo pipefail
here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd -- "$here/../../.." && pwd)
gpu=${1:-none}
shift || true
root=$repo/target/vng-suite/selftest-$(date +%Y%m%d-%H%M%S)
suite=("$repo/tools/vng-suite.sh" --registry "$here/suite.list" --gpu "$gpu" "$@")
bad=0

# Anything still running whose command line names this case's run directory.
leftovers() { pgrep -f -- "$1" || true; }

check() {
    local name=$1 rc=$2 want=$3 out=$4 verdict
    verdict=$(cat "$out/$name/verdict.txt" 2> /dev/null || echo "no verdict")
    if [ "$rc" -ne 0 ] && [[ $verdict == *"$want"* ]] && [ -z "$(leftovers "$out")" ]; then
        echo "selftest: ok   $name (exit $rc): $verdict"
    else
        echo "selftest: FAIL $name (exit $rc, want '$want'): $verdict; leftovers: $(leftovers "$out" | paste -sd' ' -)"
        bad=1
    fi
}

for c in "selftest-fail|guest: deliberate" "selftest-crash|server died" "selftest-hang|timeout after"; do
    name=${c%%|*}
    rc=0
    "${suite[@]}" --out "$root/$name" "^$name\$" > "$root-$name.log" 2>&1 || rc=$?
    check "$name" "$rc" "${c#*|}" "$root/$name"
done

name=selftest-skip
rc=0
"${suite[@]}" --out "$root/$name" "^$name\$" > "$root-$name.log" 2>&1 || rc=$?
verdict=$(cat "$root/$name/$name/verdict.txt" 2> /dev/null || echo "no verdict")
if [ "$rc" -eq 0 ] && [[ $verdict == skip:* ]]; then
    echo "selftest: ok   $name (exit 0): $verdict"
else
    echo "selftest: FAIL $name (exit $rc, want a skip): $verdict"
    bad=1
fi

# Job control gives the runner a default SIGINT instead of an ignored one.
name=selftest-interrupt
set -m
"${suite[@]}" --out "$root/$name" "^$name\$" > "$root-$name.log" 2>&1 &
runner=$!
set +m
for _ in $(seq 1 240); do
    [ -e "$root/$name/$name/STARTED" ] && break
    kill -0 "$runner" 2> /dev/null || break
    sleep 0.5
done
kill -INT "$runner" 2> /dev/null || true
rc=0
wait "$runner" || rc=$?
check "$name" "$rc" "interrupted" "$root/$name"

echo "selftest: qemu-system processes now: $(pgrep -c qemu-system || true)"
echo "selftest: logs in $root-*.log, artifacts in $root/"
exit "$bad"
