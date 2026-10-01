# Sourced by tools/vng-shot.sh INSIDE the guest (DISPLAY=:7, cwd = artifacts).
# The COW takes the pointer like Xorg's: input-opaque over the whole screen
# until the compositor sets an input region that misses the pointer, and again
# after a reset to None. The probe (cow-input-shape-probe.c) clicks through
# XTest in each phase.
# shellcheck shell=sh
# golden: probe.log
# drop: ^  settle  -- crossings from a tree change under a still pointer: Xorg sends them at once, yserver on the next motion
set -u
set +e
src=${YSERVER_REPO:?}/tools/vng-scenarios/cow-input-shape-probe.c
cc -O1 -o probe "$src" -lxcb -lxcb-composite -lxcb-xfixes -lxcb-shape -lxcb-xtest \
    > cc.log 2>&1 || cat cc.log >&2
xdotool mousemove 5 5
size=$(xdpyinfo | awk '/dimensions:/ { sub("x", " ", $2); print $2; exit }')
# shellcheck disable=SC2086
./probe $size > probe.log 2>&1
cat probe.log
if [ ! -x probe ]; then echo "fail: probe did not build (cc.log)" > RESULT
elif [ ! -e PROBE-DONE ]; then echo "fail: the probe stopped early (probe.log)" > RESULT
else echo pass > RESULT; fi
