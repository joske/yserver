# Sourced by tools/vng-shot.sh INSIDE the guest (DISPLAY=:7, cwd = artifacts).
# Every core drawing request, GetImage and RENDER read against a window's
# clip — in a client window inside its redirected frame and without a
# compositor (draw-clip-probe.c) — and the Expose and GraphicsExpose
# events of CopyArea, MapWindow and restacking (expose-probe.c).
# shellcheck shell=sh
# golden: direct.log probe.log expose-direct.log expose.log
set -u
set +e
src=${YSERVER_REPO:?}/tools/vng-scenarios/draw-clip-probe.c
cc -O1 -o probe "$src" -lxcb -lxcb-composite -lxcb-render -lxcb-xfixes > cc.log 2>&1 || cat cc.log >&2
./probe direct > direct.log 2>&1 && mv PROBE-DONE DIRECT-DONE
./probe redirect > probe.log 2>&1
esrc=${YSERVER_REPO:?}/tools/vng-scenarios/expose-probe.c
cc -O1 -o expose-probe "$esrc" -lxcb -lxcb-composite >> cc.log 2>&1 || cat cc.log >&2
./expose-probe direct > expose-direct.log 2>&1 && mv EXPOSE-DONE EXPOSE-DIRECT-DONE
./expose-probe redirect > expose.log 2>&1
cat direct.log probe.log expose-direct.log expose.log
if [ ! -x probe ] || [ ! -x expose-probe ]; then echo "fail: a probe did not build (cc.log)" > RESULT
elif [ ! -e DIRECT-DONE ] || [ ! -e PROBE-DONE ] || [ ! -e EXPOSE-DIRECT-DONE ] || [ ! -e EXPOSE-DONE ]; then
    echo "fail: a probe stopped early (direct.log, probe.log, expose-direct.log, expose.log)" > RESULT
else echo pass > RESULT; fi
