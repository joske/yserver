# Sourced by tools/vng-shot.sh INSIDE the guest (DISPLAY=:7, cwd = artifacts).
# SendEvent delivers a synthetic core event by core event masks alone, with or
# without an XI2 selection for the same event (#212); plus propagation, the
# empty mask, PointerWindow/InputFocus and the request's errors
# (sendevent-xi2-probe.c).
# shellcheck shell=sh
# golden: probe.log
set -u
set +e
cc -O1 -o probe "${YSERVER_REPO:?}/tools/vng-scenarios/sendevent-xi2-probe.c" -lxcb -lxcb-xinput -lxcb-xtest > cc.log 2>&1 || cat cc.log >&2
./probe > probe.log 2>&1
cat probe.log
if [ ! -x probe ]; then echo "fail: probe did not build (cc.log)" > RESULT
elif [ ! -e PROBE-DONE ]; then echo "fail: the probe stopped early (probe.log)" > RESULT
else echo pass > RESULT; fi
