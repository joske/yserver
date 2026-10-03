#!/usr/bin/env bash
# Run the registered vng scenarios (tools/vng-scenarios/suite.list) one at a
# time and decide pass/fail on the host. Artifacts of every run land in
# target/vng-suite/<timestamp>/<scenario>/, with summary.txt next to them.
#
#   tools/vng-suite.sh                       # every scenario, --gpu none
#   tools/vng-suite.sh --gpu venus span      # names matching the regex "span"
#   tools/vng-suite.sh --binary ../wt/target/debug/yserver
#   tools/vng-suite.sh --registry tools/vng-scenarios/suite-selftest/suite.list
#   tools/vng-suite.sh --out target/vng-suite/mine   # fixed run directory
#   tools/vng-suite.sh --regen-goldens xrandr-dpi    # rerun on Xorg, rewrite goldens
#
# A scenario passes only if its guest wrote RESULT "pass", the host driver (or
# vng-shot) exited 0, the guest ran under KVM and finished, the server was
# still alive at the end, and no crash or GPU fault shows in the logs. Each
# scenario runs in its own session; whatever is left of it is killed by
# session id afterwards, and on timeout or Ctrl+C. A guest RESULT "skip:
# <reason>" (a prerequisite the host lacks) is tallied as skip, not a pass
# or a failure.
#
# A guest script with a `# golden:` line (see tools/vng-scenarios/golden.py)
# must also match tools/vng-scenarios/goldens/<name>.txt after normalisation;
# --regen-goldens runs the selected golden scenarios on Xorg 21.1 in the
# guest and rewrites those files from its output. That Xorg run needs only a
# guest RESULT "pass": a timeout or a server dying after the results were
# written is tolerated.
#
# golden=live in the registry (output depends on distro data, e.g. the
# keymap) boots Xorg first on every run, in the same guest and environment,
# and diffs against its normalised output (<scenario>/golden.live.txt, the
# Xorg artifacts in <scenario>/xorg/). --regen-goldens skips these.
set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
registry=$repo/tools/vng-scenarios/suite.list
gpu=none
binary=
filter=
run=
regen=

usage() {
    sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed 's/^# \?//;$d'
    exit "${1:-0}"
}

while [ $# -gt 0 ]; do
    case $1 in
        --gpu) gpu=$2; shift 2;;
        --binary) binary=$2; shift 2;;
        --registry) registry=$2; shift 2;;
        --out) run=$2; shift 2;;
        --regen-goldens) regen=1; shift;;
        -h|--help) usage 0;;
        -*) echo "vng-suite: unknown argument $1" >&2; usage 1;;
        *) [ -z "$filter" ] || { echo "vng-suite: one filter only" >&2; exit 1; }
           filter=$1; shift;;
    esac
done
case $gpu in none|venus) ;; *) echo "vng-suite: --gpu must be none or venus" >&2; exit 1;; esac
[ "$filter" != all ] || filter=
[ -r "$registry" ] || { echo "vng-suite: registry $registry not readable" >&2; exit 1; }
regdir=$(cd -- "$(dirname -- "$registry")" && pwd)
goldens=$regdir/goldens
has_golden() { grep -q '^# *golden:' "$1"; }

declare -a names=() specs=()
while read -r name spec; do
    case $name in ''|'#'*) continue;; esac
    if [ -z "$filter" ] || [[ $name =~ $filter ]]; then
        names+=("$name"); specs+=("$spec")
    fi
done < "$registry"
[ ${#names[@]} -gt 0 ] || { echo "vng-suite: filter '$filter' selects no scenario in $registry" >&2; exit 2; }

cd "$repo"
if [ -n "$binary" ]; then
    binary=$(cd -- "$(dirname -- "$binary")" && pwd)/$(basename -- "$binary")
    [ -x "$binary" ] || { echo "vng-suite: $binary is not executable" >&2; exit 1; }
else
    cargo build --bin yserver
    binary=$repo/target/debug/yserver
fi

run=${run:-$repo/target/vng-suite/$(date +%Y%m%d-%H%M%S)}
mkdir -p "$run"
run=$(cd -- "$run" && pwd)
summary=$run/summary.txt
kernel=${KERNEL:-/boot/vmlinuz-linux-zen}

mesa_versions() {
    if command -v pacman > /dev/null; then
        pacman -Q mesa vulkan-swrast vulkan-virtio 2> /dev/null | paste -sd, - || true
    elif command -v dpkg-query > /dev/null; then
        dpkg-query -W -f '${Package} ${Version}\n' mesa-vulkan-drivers libgl1-mesa-dri 2> /dev/null \
            | paste -sd, - || true
    fi
}
{
    echo "date:          $(date -Is)"
    echo "yserver:       $(git -C "$repo" describe --always --dirty --abbrev=12 2> /dev/null || echo unknown)" \
        "($(git -C "$repo" rev-parse --abbrev-ref HEAD 2> /dev/null || echo '?'))"
    echo "binary:        $binary ($(sha256sum "$binary" | cut -c1-16))"
    echo "gpu mode:      $gpu"
    if [ -n "$regen" ]; then
        echo "server:        $(/usr/lib/Xorg -version 2>&1 | grep -m1 'X.Org X Server' || echo 'Xorg ?'), regenerating goldens"
    fi
    echo "host /dev/kvm: $([ -r /dev/kvm ] && [ -w /dev/kvm ] && echo usable || echo NOT usable)"
    echo "host kernel:   $(uname -r)"
    echo "guest kernel:  $kernel ($(file -bL "$kernel" 2> /dev/null | grep -o 'version [^ ]*' || echo '?'))"
    echo "qemu:          $(qemu-system-x86_64 --version 2> /dev/null | head -1 || echo '?')"
    echo "virtme-ng:     $(vng --version 2> /dev/null | head -1 || echo '?')"
    echo "mesa:          $(mesa_versions)"
    echo "registry:      $registry"
    echo
} > "$summary"
cat "$summary"

# Processes of one session, the scenario's whole tree even after reparenting.
session_pids() { ps -e -o pid=,sid= | awk -v s="$1" '$2 == s { print $1 }'; }
reap() {
    local sid=$1 pids
    pids=$(session_pids "$sid")
    [ -n "$pids" ] || return 0
    # shellcheck disable=SC2086
    kill -TERM $pids 2> /dev/null || true
    for _ in $(seq 1 20); do
        [ -n "$(session_pids "$sid")" ] || return 0
        sleep 0.5
    done
    pids=$(session_pids "$sid")
    # shellcheck disable=SC2086
    [ -z "$pids" ] || kill -KILL $pids 2> /dev/null || true
}

cur_sid=
interrupted=
# shellcheck disable=SC2329
on_signal() {
    interrupted=1
    [ -z "$cur_sid" ] || reap "$cur_sid"
}
trap on_signal INT TERM HUP

declare -a rows=()
failed=0
skips=0
row() { rows+=("$(printf '%-20s %-6s %-8s %-7s %5s  %s' "$@")"); }

# Only the guest's own RESULT and complete artifacts gate an Xorg run; $1 is
# its normalised output. Filters why, keeps the rest in tolerated.
xorg_gate() {
    local w
    local -a kept=()
    for w in ${why[@]+"${why[@]}"}; do
        case $w in guest:*|"no RESULT"|"bad RESULT"*|"golden.py failed"|interrupted) kept+=("$w");; esac
    done
    grep -qx '(missing)' "$1" && kept+=("golden artifact missing")
    tolerated=$(IFS=';'; echo "${why[*]}")
    why=(${kept[@]+"${kept[@]}"})
}

# Diff the yserver output against reference $1 (labelled $2, described as $3).
compare_golden() {
    diff -u --label "$2" --label yserver "$1" "$dir/golden.txt" > "$dir/golden.diff" && return 0
    why+=("differs from $3 ($(($(grep -c '^[-+]' "$dir/golden.diff") - 2)) lines, golden.diff)")
    diff_file=$dir/golden.diff
}

# One boot of the current scenario as $1 (artifacts in $run/$1), extra
# vng-shot args after it. Sets why, rc, secs, kvm and timed_out.
run_leg() {
    local leg=$1 legdir=$run/$1 hostlog=$run/.$1.host.log sidfile=$run/.$1.sid
    local pid start leftover='' hit result
    shift
    local -a args=(--gpu "$gpu" --binary "$binary" --timeout $((timeout_s + 60)) "$@") cmd
    [ -z "$hook" ] || args+=(--root-hook "$hook")
    if [ -n "$host" ]; then
        cmd=("$host" "$leg" "${args[@]}")
    else
        cmd=("$repo/tools/vng-shot.sh" --name "$leg" --scenario "$guest" --outputs "$outputs" "${args[@]}")
    fi
    rm -rf "$legdir"
    echo "vng-suite: $leg ($gpu, timeout ${timeout_s}s)"
    start=$SECONDS
    # shellcheck disable=SC2016
    VNG_OUT=$run setsid -w bash -c 'echo $$ > "$0"; exec "$@"' "$sidfile" "${cmd[@]}" \
        > "$hostlog" 2>&1 < /dev/null &
    pid=$!
    for _ in $(seq 1 50); do [ -s "$sidfile" ] && break; sleep 0.1; done
    cur_sid=$(cat "$sidfile" 2> /dev/null || true)
    timed_out=
    while kill -0 "$pid" 2> /dev/null; do
        [ -z "$interrupted" ] || break
        if [ $((SECONDS - start)) -ge "$timeout_s" ]; then timed_out=1; break; fi
        sleep 1
    done
    rc=0
    if [ -n "$timed_out" ] || [ -n "$interrupted" ]; then
        [ -z "$cur_sid" ] || reap "$cur_sid"
        wait "$pid" 2> /dev/null || true
    else
        wait "$pid" || rc=$?
    fi
    secs=$((SECONDS - start))
    if [ -n "$cur_sid" ]; then
        for _ in $(seq 1 10); do [ -n "$(session_pids "$cur_sid")" ] || break; sleep 0.5; done
        [ -z "$(session_pids "$cur_sid")" ] || { leftover=1; reap "$cur_sid"; }
    fi
    cur_sid=

    mkdir -p "$legdir"
    mv "$hostlog" "$legdir/host.log"
    rm -f "$sidfile"
    find "$legdir" \( -type s -o -name monitor.sock \) -delete 2> /dev/null || true

    why=()
    skipped=
    [ -z "$interrupted" ] || why+=("interrupted")
    [ -z "$timed_out" ] || why+=("timeout after ${timeout_s}s")
    [ ! -e "$legdir/NOT-KVM" ] || why+=("guest not under KVM")
    if [ ! -e "$legdir/RESULT" ]; then
        why+=("no RESULT")
    else
        result=$(head -1 "$legdir/RESULT")
        case $result in
            pass) ;;
            skip:*) skipped=${result#skip: };;
            fail:*) why+=("guest: ${result#fail: }");;
            *) why+=("bad RESULT '$result'");;
        esac
    fi
    [ "$rc" -eq 0 ] || [ -n "$timed_out$interrupted" ] || why+=("host check exited $rc (host.log)")
    if [ -e "$legdir/SERVER-DEAD" ]; then
        why+=("server died, $(head -1 "$legdir/SERVER-DEAD")")
    elif [ ! -e "$legdir/SERVER-ALIVE" ]; then
        why+=("server state unknown")
    fi
    [ -e "$legdir/DONE" ] || why+=("guest did not finish")
    if [ -e "$legdir/yserver.log" ]; then
        hit=$(grep -m1 -E 'panicked at|ERROR_DEVICE_LOST|SIGSEGV|SIGABRT|stack backtrace' "$legdir/yserver.log" || true)
        [ -z "$hit" ] || why+=("yserver.log: ${hit:0:80}")
    fi
    if [ -e "$legdir/guest-dmesg.log" ]; then
        hit=$(grep -m1 -E 'BUG:|Oops|general protection fault|segfault|GPU fault|virtio_gpu.*(error|fault)' \
            "$legdir/guest-dmesg.log" || true)
        [ -z "$hit" ] || why+=("guest dmesg: ${hit:0:80}")
        kvm=$(grep -q 'Hypervisor detected: KVM' "$legdir/guest-dmesg.log" && echo kvm || echo no-kvm)
        grep -m1 -o 'Linux version [^ ]*' "$legdir/guest-dmesg.log" > "$legdir/guest-kernel.txt" || true
    else
        kvm=$([ -e "$legdir/NOT-KVM" ] && echo no-kvm || echo '?')
    fi
    [ -z "$leftover" ] || why+=("processes outlived the run")
}

for i in "${!names[@]}"; do
    name=${names[$i]}
    guest='' host='' outputs=1 timeout_s=300 modes=none,venus hook='' golden=stored
    for kv in ${specs[$i]}; do
        case $kv in
            guest=*) guest=$regdir/${kv#guest=};;
            host=*) host=$regdir/${kv#host=};;
            outputs=*) outputs=${kv#outputs=};;
            timeout=*) timeout_s=${kv#timeout=};;
            gpu=*) modes=${kv#gpu=};;
            root-hook=*) hook=$regdir/${kv#root-hook=};;
            golden=stored|golden=live) golden=${kv#golden=};;
            *) echo "vng-suite: $name: unknown key $kv" >&2; exit 1;;
        esac
    done
    if [[ ,$modes, != *,$gpu,* ]]; then
        row "$name" "$gpu" skip - - "supports $modes"
        continue
    fi
    [ -n "$guest" ] || { echo "vng-suite: $name has no guest=" >&2; exit 1; }
    if [ "$golden" = live ] && ! has_golden "$guest"; then
        echo "vng-suite: $name: golden=live needs a '# golden:' line in $guest" >&2; exit 1
    fi
    if [ -n "$regen" ] && ! has_golden "$guest"; then
        row "$name" "$gpu" skip - - "no golden"
        continue
    fi
    if [ -n "$regen" ] && [ "$golden" = live ]; then
        row "$name" "$gpu" skip - - "golden=live: compared against Xorg on every run"
        continue
    fi
    [ -n "$interrupted" ] && { row "$name" "$gpu" fail - - "not run (interrupted)"; failed=1; continue; }

    dir=$run/$name
    rm -rf "$dir"
    # golden=live: boot Xorg first in this same environment, as the reference.
    xorg_why='' xorg_secs=0 xorg_skipped=
    if [ "$golden" = live ]; then
        xdir=$run/$name.xorg
        run_leg "$name.xorg" --server xorg
        xorg_secs=$secs
        xorg_skipped=$skipped
        "$regdir/golden.py" "$guest" "$xdir" > "$xdir/golden.txt" || why+=("golden.py failed")
        xorg_gate "$xdir/golden.txt"
        [ -z "$tolerated" ] || echo "vng-suite: $name: live Xorg run tolerated: ${tolerated//;/; }"
        [ ${#why[@]} -eq 0 ] || xorg_why=$(IFS=';'; echo "${why[*]}")
    fi
    if [ -n "$interrupted" ]; then
        why=("interrupted") rc=0 secs=0 kvm=- timed_out=
        mkdir -p "$dir"
    else
        run_leg "$name" ${regen:+--server xorg}
    fi
    secs=$((secs + xorg_secs))
    if [ "$golden" = live ]; then
        mv "$xdir" "$dir/xorg"
        cp "$dir/xorg/golden.txt" "$dir/golden.live.txt"
    fi

    diff_file=
    [ -z "$xorg_skipped" ] || skipped=$xorg_skipped
    if [ -n "$skipped" ] && [ ${#why[@]} -eq 0 ]; then
        :
    elif has_golden "$guest"; then
        "$regdir/golden.py" "$guest" "$dir" > "$dir/golden.txt" || why+=("golden.py failed")
        if [ -n "$regen" ]; then
            xorg_gate "$dir/golden.txt"
            if [ ${#why[@]} -eq 0 ]; then
                mkdir -p "$goldens"
                cp "$dir/golden.txt" "$goldens/$name.txt"
                echo "vng-suite: $name: wrote $goldens/$name.txt${tolerated:+ (tolerated: ${tolerated//;/; })}"
            fi
        elif [ "$golden" = live ]; then
            if [ -n "$xorg_why" ]; then
                why+=("live Xorg run failed, nothing to compare: $xorg_why (xorg/)")
            else
                compare_golden "$dir/golden.live.txt" "golden.live.txt" "the live Xorg run"
            fi
        elif [ ! -e "$goldens/$name.txt" ]; then
            why+=("no golden (run --regen-goldens)")
        else
            compare_golden "$goldens/$name.txt" "golden/$name.txt" "the Xorg golden"
        fi
    fi

    if [ -n "$skipped" ] && [ ${#why[@]} -eq 0 ]; then
        status=skip reason=$skipped
        skips=$((skips + 1))
    elif [ ${#why[@]} -eq 0 ]; then
        status=pass reason=
    else
        failed=1
        status=fail
        [ -z "$timed_out" ] || status=timeout
        reason=$(IFS=';'; echo "${why[*]}")
        reason=${reason//;/; }
    fi
    [ -z "$regen" ] || [ "$status" != pass ] || status=golden
    { echo "$status${reason:+: $reason}"; [ -z "$diff_file" ] || cat "$diff_file"; } > "$dir/verdict.txt"
    row "$name" "$gpu" "$status" "$kvm" "$secs" "$reason"
    echo "vng-suite: $name: $status${reason:+ ($reason)}"
done

{
    printf '%-20s %-6s %-8s %-7s %5s  %s\n' scenario mode result kvm secs reason
    printf '%s\n' "${rows[@]}"
} | tee -a "$summary"
[ "$skips" -eq 0 ] || echo "vng-suite: $skips scenario(s) skipped (guest RESULT skip)" | tee -a "$summary"
echo "vng-suite: artifacts in $run"
[ -z "$interrupted" ] || exit 130
exit "$failed"
