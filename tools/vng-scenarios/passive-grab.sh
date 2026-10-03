# Sourced by tools/vng-shot.sh INSIDE the guest (DISPLAY=:7, cwd = artifacts).
# A synchronous passive button grab whose event mask lacks ButtonPress, as
# CDE's dtwm puts on its front panel: the activating press, the freeze and
# the thaw by AllowEvents (passive-grab-probe.c); a plain click on an
# ungrabbed window beside it for contrast.
# shellcheck shell=sh
# golden: probe.log
# mask: \sMappingNotify\(request \d\) =>  -- Xorg's master keyboard switches to its XTEST slave on the first faked key and announces it with MappingNotify; yserver has one keyboard
set -u
set +e
cc -O1 -o probe "${YSERVER_REPO:?}/tools/vng-scenarios/passive-grab-probe.c" -lxcb -lxcb-xtest > cc.log 2>&1 || cat cc.log >&2
./probe > probe.log 2>&1
cat probe.log
if [ ! -x probe ]; then echo "fail: probe did not build (cc.log)" > RESULT
elif [ ! -e PROBE-DONE ]; then echo "fail: the probe stopped early (probe.log)" > RESULT
else echo pass > RESULT; fi
