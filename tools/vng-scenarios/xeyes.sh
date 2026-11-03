# Sourced by tools/vng-shot.sh INSIDE the guest (DISPLAY=:7, cwd = artifacts).
# xeyes without a WM: its shell carries a SHAPE Mask of two ovals over an
# unshaped child; xeyes-host.sh checks the scanout shows the root around the
# ovals, as Xorg does. This size puts the scene past 1024 draws.
# shellcheck shell=sh
set -u
set +e
xsetroot -solid "#4f4f4f"
xeyes -geometry 1000x600+100+100 > xeyes.log 2>&1 &
if timeout 30 xdotool search --sync --onlyvisible --class xeyes > /dev/null 2>&1; then
    echo pass > RESULT
else
    echo "fail: xeyes window never mapped" > RESULT
fi
