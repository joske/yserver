# Sourced by tools/vng-shot.sh INSIDE the guest (DISPLAY=:7, cwd = artifacts).
# Moving unredirected windows inside a redirected top-level — the MATE panel's
# applets under a compositor: the moved window's pixels follow it, the
# compositor's damage covers both ends, and what a move uncovers (beneath it,
# and in the moved window itself) is exposed. subwindow-move-probe.c has the
# stages.
# shellcheck shell=sh
# golden: probe.log
set -u
set +e
src=${YSERVER_REPO:?}/tools/vng-scenarios/subwindow-move-probe.c
cc -O1 -o probe "$src" -lxcb -lxcb-composite -lxcb-damage -lxcb-xfixes > cc.log 2>&1 || cat cc.log >&2
./probe > probe.log 2>&1
cat probe.log
if [ ! -x probe ]; then echo "fail: probe did not build (cc.log)" > RESULT
elif [ ! -e PROBE-DONE ]; then echo "fail: the probe stopped early (probe.log)" > RESULT
else echo pass > RESULT; fi
