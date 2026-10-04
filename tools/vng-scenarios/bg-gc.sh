# Sourced by tools/vng-shot.sh INSIDE the guest (DISPLAY=:7, cwd = artifacts).
# Window background paints (ClearArea, a child's map, a child's unmap) after
# another or the same client drew with GXxor, a partial plane mask,
# FillStippled or IncludeInferiors: Xorg paints them with its own GC, and a
# MIT-SHM pixmap holds the segment bytes as they are (bg-gc-probe.c).
# shellcheck shell=sh
# golden: probe.log
set -u
set +e
cc -O1 -o probe "${YSERVER_REPO:?}/tools/vng-scenarios/bg-gc-probe.c" -lxcb -lxcb-shm > cc.log 2>&1 || cat cc.log >&2
./probe > probe.log 2>&1
cat probe.log
if [ ! -x probe ]; then echo "fail: probe did not build (cc.log)" > RESULT
elif [ ! -e PROBE-DONE ]; then echo "fail: the probe stopped early (probe.log)" > RESULT
else echo pass > RESULT; fi
