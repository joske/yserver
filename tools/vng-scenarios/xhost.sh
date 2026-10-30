# Sourced by tools/vng-shot.sh INSIDE the guest (DISPLAY=:7, cwd = artifacts).
# Issue #305: xhost under -auth. xhost-host.sh gives the server a cookie only
# ysuite holds; xhost-root.sh runs ysuite2's commands (u2 below) without one.
# RESULT is the verdict, then one line per check (also in checks.log).
# The expected text is Xorg's; both runs must pass:
#   tools/vng-suite.sh xhost
#   tools/vng-scenarios/xhost-host.sh xhost-xorg --server xorg --gpu none
# shellcheck shell=sh
set -u
set +e
ok=1
: > checks.log; : > commands.log
u2() { printf '%s\n' "$*" > /run/xhost/req; read -r rc < /run/xhost/rep; return "$rc"; }
connects() { xdpyinfo > /dev/null; }
u2_connects() { u2 'xdpyinfo > /dev/null'; }
refused() { ! "$@"; }
# Command output goes to commands.log under the check's label.
is() {
    label=$1; shift
    echo "== $label" >> commands.log
    if "$@" >> commands.log 2>&1; then r=ok; else r=FAIL; ok=; fi
    printf '%-5s %s\n' "$r" "$label" >> checks.log
}
list_is() { xhost > xhost-list.txt 2>&1; cat xhost-list.txt; printf '%s\n' "$@" | cmp -s - xhost-list.txt; }
# Xorg resets, dropping the host list, when its last client leaves.
xprop -root -spy > /dev/null 2>&1 &
hold=$!
sleep 1
xprop -root -f XHOST_PING 8s -set XHOST_PING before
is "0 ysuite connects with the cookie" connects
# shellcheck disable=SC2016
is "0 commands run as ysuite2" u2 'test "$(id -un)" = ysuite2'

is "1 ysuite2 without a cookie is refused" refused u2_connects

is "2 xhost +si:localuser:ysuite2" xhost +si:localuser:ysuite2
is "2 ysuite2 connects" u2_connects
u2 'stdbuf -oL xprop -root -spy XHOST_PING > /run/xhost/spy.log 2>&1 &'
i=0; while ! grep -q '"before"' /run/xhost/spy.log 2> /dev/null && [ $i -lt 50 ]; do i=$((i+1)); sleep 0.1; done
is "2 ysuite2 keeps a client open" grep -q '"before"' /run/xhost/spy.log

is "3 xhost lists it" list_is "access control enabled, only authorized clients can connect" \
    "SI:localuser:ysuite2"

is "4 xhost -si:localuser:ysuite2" xhost -si:localuser:ysuite2
is "4 a new ysuite2 client is refused" refused u2_connects
xprop -root -f XHOST_PING 8s -set XHOST_PING after
i=0; while ! grep -q '"after"' /run/xhost/spy.log && [ $i -lt 50 ]; do i=$((i+1)); sleep 0.1; done
is "4 the open ysuite2 client still answers" grep -q '"after"' /run/xhost/spy.log

is "5 xhost +" xhost +
is "5 ysuite2 connects" u2_connects
if DISPLAY=127.0.0.1:7 xdpyinfo > /dev/null 2>&1; then
    # Deliberate difference (spec): Xorg admits any TCP client while access
    # is disabled (os/access.c:1535); the list never admits TCP on yserver.
    if xdpyinfo | grep -q '^vendor string: *The X.Org Foundation'; then
        is "5 TCP without a cookie is admitted (Xorg)" u2 xdpyinfo -display 127.0.0.1:7
    else
        is "5 TCP without a cookie is refused" refused u2 xdpyinfo -display 127.0.0.1:7
    fi
else
    echo "skip  5 TCP without a cookie (no TCP listener)" >> checks.log
fi
is "5 xhost -" xhost -
is "5 ysuite2 is refused again" refused u2_connects

is "6 xhost +local:" xhost +local:
is "6 xhost lists LOCAL:" list_is "access control enabled, only authorized clients can connect" \
    "LOCAL:"
is "6 ysuite2 connects" u2_connects
is "6 xhost -local:" xhost -local:
is "6 ysuite2 is refused again" refused u2_connects

echo exit > /run/xhost/req
cp /run/xhost/spy.log spy.log 2> /dev/null
kill $hold 2> /dev/null
if [ -n "$ok" ]; then echo pass > RESULT; else echo "fail: $(grep -c ^FAIL checks.log) check(s) (checks.log)" > RESULT; fi
cat checks.log >> RESULT
