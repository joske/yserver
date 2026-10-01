# Sourced by tools/vng-shot.sh INSIDE the guest (DISPLAY=:7, cwd = artifacts).
# Crossings from window-tree changes under a still pointer: map, unmap,
# configure, restack, shape, reparent and destroy, plain and under a grab.
# Xorg sends them within the request; crossing-tree-probe.c logs core and XI2.
# shellcheck shell=sh
# golden: probe.log
set -u
set +e
src=${YSERVER_REPO:?}/tools/vng-scenarios/crossing-tree-probe.c
cc -O1 -o probe "$src" -lxcb -lxcb-shape -lxcb-xfixes -lxcb-xinput > cc.log 2>&1 || cat cc.log >&2
size=$(xdpyinfo | awk '/dimensions:/ { sub("x", " ", $2); print $2; exit }')
# shellcheck disable=SC2086
./probe $size > probe.log 2>&1
cat probe.log
if [ ! -x probe ]; then echo "fail: probe did not build (cc.log)" > RESULT
elif [ ! -e PROBE-DONE ]; then echo "fail: the probe stopped early (probe.log)" > RESULT
else echo pass > RESULT; fi
