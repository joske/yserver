# Sourced by tools/vng-shot.sh INSIDE the guest (DISPLAY=:7, cwd = artifacts).
# Drawing into a client window inside its redirected frame — a GTK window
# under xfwm4's compositor: core, SHM and RENDER drawing, CopyArea scrolls and
# a subwindow move in the client stay inside it and leave the frame's title
# bar and button bar alone. child-clip-probe.c has the stages; `direct` is the
# same probe without a compositor. xembed-scroll-probe.c scrolls a second
# client's plug inside the first's socket, as xfce4-settings-manager does.
# shellcheck shell=sh
# golden: direct.log probe.log xembed-direct.log xembed.log
set -u
set +e
src=${YSERVER_REPO:?}/tools/vng-scenarios/child-clip-probe.c
cc -O1 -o probe "$src" -lxcb -lxcb-composite -lxcb-damage -lxcb-render -lxcb-shape -lxcb-shm -lxcb-xfixes > cc.log 2>&1 || cat cc.log >&2
./probe direct > direct.log 2>&1 && mv PROBE-DONE DIRECT-DONE
./probe redirect > probe.log 2>&1
xsrc=${YSERVER_REPO:?}/tools/vng-scenarios/xembed-scroll-probe.c
cc -O1 -o xembed-probe "$xsrc" -lxcb -lxcb-composite -lxcb-damage -lxcb-shape -lxcb-xfixes >> cc.log 2>&1 || cat cc.log >&2
./xembed-probe direct > xembed-direct.log 2>&1 && mv XEMBED-DONE XEMBED-DIRECT-DONE
./xembed-probe redirect > xembed.log 2>&1
cat direct.log probe.log xembed-direct.log xembed.log
if [ ! -x probe ] || [ ! -x xembed-probe ]; then echo "fail: a probe did not build (cc.log)" > RESULT
elif [ ! -e DIRECT-DONE ] || [ ! -e PROBE-DONE ] || [ ! -e XEMBED-DIRECT-DONE ] || [ ! -e XEMBED-DONE ]; then
    echo "fail: a probe stopped early (direct.log, probe.log, xembed-direct.log, xembed.log)" > RESULT
else echo pass > RESULT; fi
