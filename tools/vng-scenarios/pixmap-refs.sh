# Sourced by tools/vng-shot.sh INSIDE the guest (DISPLAY=:7, cwd = artifacts).
# #196 follow-up: pixmaps a GC holds, and cursor sprites, must die with the last
# reference to them; pixmap-refs-check.py reads the live-pixmap counter after each variant.
# shellcheck shell=sh
set -u
set +e
cc -O1 -o probe "${YSERVER_REPO:?}/tools/vng-scenarios/pixmap-refs-probe.c" -lxcb-render -lxcb > cc.log 2>&1 \
    || cat cc.log >&2
./probe > probe.log 2>&1
if [ ! -x probe ]; then echo "fail: probe did not build (cc.log)" > RESULT
elif ! grep -q "^PHASE cursor_disconnect end" probe.log; then echo "fail: the probe stopped early (probe.log)" > RESULT
else echo pass > RESULT; fi
