# Sourced by tools/vng-shot.sh INSIDE the guest (DISPLAY=:7, cwd = artifacts).
# The sprite's cursor over the bare root (the server default) and over
# InputOutput and InputOnly windows with a cursor of their own, as dtwm's
# frame resize handles (cursor-probe.c).
# shellcheck shell=sh
# golden: probe.log
set -u
set +e
cc -O1 -o probe "${YSERVER_REPO:?}/tools/vng-scenarios/cursor-probe.c" -lxcb -lxcb-xfixes -lxcb-xtest -lxcb-shape > cc.log 2>&1 || cat cc.log >&2
./probe > probe.log 2>&1
cat probe.log
if [ ! -x probe ]; then echo "fail: probe did not build (cc.log)" > RESULT
elif [ ! -e PROBE-DONE ]; then echo "fail: the probe stopped early (probe.log)" > RESULT
else echo pass > RESULT; fi
