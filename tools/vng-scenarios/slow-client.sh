# Sourced by tools/vng-shot.sh INSIDE the guest (DISPLAY=:7, cwd = artifacts).
# A client that stops reading while events flood in stays connected (Xorg's
# INT_MAX output cap) and gets every event once it reads; so does a reading
# client, and the server lives. slow-client-probe.c checks each event family.
# shellcheck shell=sh
set -u
set +e
src=${YSERVER_REPO:?}/tools/vng-scenarios/slow-client-probe.c
cc -O1 -o probe "$src" -lxcb -lxcb-xtest -lxcb-xinput -lxcb-damage > cc.log 2>&1 || cat cc.log >&2
./probe > probe.log 2>&1; rc=$?
cat probe.log
if [ ! -x probe ]; then echo "fail: probe did not build (cc.log)" > RESULT
elif [ ! -e PROBE-DONE ]; then echo "fail: the probe stopped early (probe.log)" > RESULT
elif [ "$rc" -ne 0 ]; then echo "fail: $(grep -c FAIL probe.log) event families lost or dropped a slow client's events (probe.log)" > RESULT
else echo pass > RESULT; fi
