#!/usr/bin/env bash
# Count X11 requests, replies, events and errors per kind in an x11trace dump,
# and optionally diff two dumps side by side.
#
# WHY THIS EXISTS
#
# x11trace has no timestamp option, so two dumps cannot be compared per second
# and absolute totals are duration-confounded: a longer capture "shows more
# traffic" whatever the server did. The only sound comparison is between two
# captures of a workload that emitted the SAME number of input events, which is
# what `tools/wm-pointer-drag-workload.sh` exists to guarantee. Given that, a
# per-kind count answers the question a stutter report actually raises — is the
# client asking for more, or is the server costing more per ask? — because the
# counts isolate the first and leave the second to the loop telemetry.
#
# For issue #155 the specific question was whether evilwm does something silly
# per motion event. Its `client_move_drag` should emit exactly one
# ConfigureWindow on the frame plus one SendEvent per MotionNotify, so
# ConfigureWindow ~= SendEvent ~= the driven step count means the WM is
# blameless and the cost is ours.
#
# USAGE
#
#   tools/xtrace-opcount.sh <trace>              # one trace, counts per kind
#   tools/xtrace-opcount.sh <trace-a> <trace-b>  # side-by-side with deltas
#
# Unclassifiable lines are REPORTED, not dropped: a silent drop would turn a
# format change in x11trace into a fake "traffic went away" result.

set -u

usage() {
    echo "usage: xtrace-opcount.sh <trace> [<trace-b>]" >&2
    exit 1
}

[ $# -ge 1 ] || usage
[ $# -le 2 ] || usage

for f in "$@"; do
    [ -r "$f" ] || { echo "xtrace-opcount: cannot read $f" >&2; exit 1; }
done

# Classify one trace into "<kind>\t<name>\t<count>" on stdout.
#
# Request lines look like:
#   000:<:0003: 20: Request(55): CreateGC cid=...
#   000:<:0002:  4: BIG-REQUESTS-Request(133,0): Enable
# Reply lines:
#   000:>:0004:160: Reply to GetProperty: type=...
# Events and errors are whatever else travels server->client.
classify() {
    awk '
        # Requests: client -> server.
        /:<:/ {
            if (match($0, /Request\([0-9]+(,[0-9]+)?\): [A-Za-z0-9_]+/)) {
                s = substr($0, RSTART, RLENGTH)
                sub(/^Request\([0-9]+(,[0-9]+)?\): /, "", s)
                req[s]++; nreq++
                next
            }
            # The connection-setup line travels client->server too.
            if ($0 ~ /am lsb-first/) { next }
            other["request-unparsed"]++; nother++
            next
        }
        /:>:/ {
            if (match($0, /Reply to [A-Za-z0-9_]+/)) {
                s = substr($0, RSTART + 9, RLENGTH - 9)
                rep[s]++; nrep++
                next
            }
            # `Event CreateNotify(16) ...`, for an extension
            # `Event XKEYBOARD-XkbEvent(85) ...`, and for one delivered by a
            # client SendEvent rather than the server
            # `Event (generated) ConfigureNotify(22) ...`. The NAME precedes
            # the number in parens; the optional "(generated) " sits between
            # "Event " and the name, and is counted separately because a
            # synthetic event is a different fact from a real one — a WM
            # send_config() lands here, so folding the two hides who moved
            # the window.
            if (match($0, /Event \(generated\) [A-Za-z0-9_-]+\(/)) {
                s = substr($0, RSTART + 18, RLENGTH - 19)
                gen[s]++; ngen++
                next
            }
            if (match($0, /Event [A-Za-z0-9_-]+\(/)) {
                s = substr($0, RSTART + 6, RLENGTH - 7)
                ev[s]++; nev++
                next
            }
            # `Error 15=Name: major=45, ...` — the code is before the "=",
            # the name after it. Keep the name; the code alone is unreadable
            # and the numbering here belongs to x11trace.
            if (match($0, /Error [0-9]+=[A-Za-z0-9_]+/)) {
                s = substr($0, RSTART, RLENGTH)
                sub(/^Error [0-9]+=/, "", s)
                err[s]++; nerr++
                next
            }
            # The greeting and the "am lsb-first" handshake land here.
            if ($0 ~ /Success, version is/ || $0 ~ /am lsb-first/) { next }
            other["server-unparsed"]++; nother++
            next
        }
        { other["no-direction"]++; nother++ }
        END {
            for (k in req) printf "request\t%s\t%d\n", k, req[k]
            for (k in rep) printf "reply\t%s\t%d\n", k, rep[k]
            for (k in ev)  printf "event\t%s\t%d\n", k, ev[k]
            for (k in gen) printf "sendevent\t%s\t%d\n", k, gen[k]
            for (k in err) printf "error\t%s\t%d\n", k, err[k]
            for (k in other) printf "unclassified\t%s\t%d\n", k, other[k]
            printf "TOTAL\trequests\t%d\n", nreq + 0
            printf "TOTAL\treplies\t%d\n",  nrep + 0
            printf "TOTAL\tevents\t%d\n",   nev + 0
            printf "TOTAL\tsendevents\t%d\n", ngen + 0
            printf "TOTAL\terrors\t%d\n",   nerr + 0
        }
    ' "$1"
}

if [ $# -eq 1 ]; then
    printf '%-14s %-26s %8s\n' KIND NAME COUNT
    printf '%-14s %-26s %8s\n' -------------- -------------------------- --------
    classify "$1" | sort -k1,1 -k3,3nr | while IFS=$'\t' read -r kind name count; do
        printf '%-14s %-26s %8d\n' "$kind" "$name" "$count"
    done
    exit 0
fi

a=$1
b=$2
ta=$(mktemp); tb=$(mktemp)
trap 'rm -f "$ta" "$tb"' EXIT
classify "$a" | sort >"$ta"
classify "$b" | sort >"$tb"

echo "A = $a"
echo "B = $b"
echo
printf '%-14s %-26s %9s %9s %9s\n' KIND NAME A B "B-A"
printf '%-14s %-26s %9s %9s %9s\n' -------------- -------------------------- --------- --------- ---------
join -t$'\t' -j 1 -a 1 -a 2 -o 0,1.2,2.2 -e 0 \
    <(awk -F'\t' '{printf "%s\t%s\t%s\n", $1"|"$2, $3, ""}' "$ta" | cut -f1,2) \
    <(awk -F'\t' '{printf "%s\t%s\t%s\n", $1"|"$2, $3, ""}' "$tb" | cut -f1,2) \
  | while IFS=$'\t' read -r key ca cb; do
        kind=${key%%|*}
        name=${key#*|}
        printf '%-14s %-26s %9d %9d %+9d\n' "$kind" "$name" "$ca" "$cb" "$((cb - ca))"
    done | sort -k1,1 -k3,3nr
