# Sourced by tools/vng-shot.sh INSIDE the guest (DISPLAY=:7, cwd = artifacts).
# XTEST motion across two windows: which deviceid/sourceid the XI2 crossings,
# motion, buttons and focus events carry for XIAllDevices, slave-only and
# XIAllMasterDevices selectors. Attached slaves get no crossings on Xorg.
# shellcheck shell=sh
# golden: probe.log
# drop: ^  [XS] Motion dev=xtest-ptr -- open divergence: Xorg stamps an attached slave's motion with the sprite before it moves (Xi/exevents.c:1865, CheckMotion runs only for the master); yserver uses the new position
set -u
set +e
src=${YSERVER_REPO:?}/tools/vng-scenarios/xi2-crossing-devices-probe.c
cc -O1 -o probe "$src" -lxcb -lxcb-xinput -lxcb-xtest > cc.log 2>&1 || cat cc.log >&2
./probe > probe.log 2>&1
cat probe.log
if [ ! -x probe ]; then echo "fail: probe did not build (cc.log)" > RESULT
elif [ ! -e PROBE-DONE ]; then echo "fail: the probe stopped early (probe.log)" > RESULT
else echo pass > RESULT; fi
