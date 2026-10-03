# Sourced by tools/vng-shot.sh INSIDE the guest (DISPLAY=:7, cwd = artifacts).
# MIT-SCREEN-SAVER SetAttributes the way CDE's dtsession uses it: one client
# owns the saver window's attributes, another is refused, and the screen
# saver shows and removes that window (screensaver-probe.c).
# shellcheck shell=sh
# golden: probe.log
set -u
set +e
cc -O1 -o probe "${YSERVER_REPO:?}/tools/vng-scenarios/screensaver-probe.c" -lxcb -lxcb-screensaver > cc.log 2>&1 || cat cc.log >&2
./probe > probe.log 2>&1
cat probe.log
if [ ! -x probe ]; then echo "fail: probe did not build (cc.log)" > RESULT
elif [ ! -e PROBE-DONE ]; then echo "fail: the probe stopped early (probe.log)" > RESULT
else echo pass > RESULT; fi
