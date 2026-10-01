# Sourced by tools/vng-shot.sh INSIDE the guest (DISPLAY=:7, cwd = artifacts).
# Cinnamon's lock screen: muffin shapes the COW to an empty Bounding region
# and unredirects the locker while still Presenting its stage into the COW.
# The probe (cow-empty-shape-probe.c) runs the three phases and
# cow-empty-shape-host.sh checks every output's scanout after each one.
# shellcheck shell=sh
# golden: probe.log
set -u
set +e
src=${YSERVER_REPO:?}/tools/vng-scenarios/cow-empty-shape-probe.c
cc -O1 -o probe "$src" -lxcb -lxcb-composite -lxcb-xfixes -lxcb-shape -lxcb-present \
    -lxcb-dri3 > cc.log 2>&1 || cat cc.log >&2
# Both heads on Virtual-2's mode (Virtual-1 lists it too): a grouped direct
# flip needs one refresh rate on every CRTC.
mode=$(xrandr --verbose | awk '/^Virtual-2 /{v=1} v && /\*current/ { gsub(/[()]/, "", $2); print $2; exit }')
xrandr --output Virtual-1 --mode "$mode" --pos 0x0 --output Virtual-2 --right-of Virtual-1 \
    > xrandr.log 2>&1
sleep 1
xdotool mousemove 5 5
size=$(xdpyinfo | awk '/dimensions:/ { sub("x", " ", $2); print $2; exit }')
# shellcheck disable=SC2086
./probe $size > probe.log 2>&1
cat probe.log
if [ ! -x probe ]; then echo "fail: probe did not build (cc.log)" > RESULT
elif [ ! -e PHASES-DONE-2 ]; then echo "fail: the probe stopped early (probe.log)" > RESULT
else echo pass > RESULT; fi
