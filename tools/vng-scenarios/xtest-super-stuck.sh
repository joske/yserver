# Sourced by tools/vng-shot.sh INSIDE the guest (DISPLAY=:7, cwd = artifacts).
# Issue #168 repro: `xdotool key super+5` under a WM that passively grabs
# Mod4+5 leaves Super stuck on yserver (not on Xorg). awesome's stock rc
# binds Mod4+<n> to view tag n, like dwm.
#
#   tools/vng-shot.sh --dump none --name xtest-super-ys \
#       --scenario tools/vng-scenarios/xtest-super-stuck.sh
#   tools/vng-shot.sh --server xorg --dump none --name xtest-super-xorg \
#       --scenario tools/vng-scenarios/xtest-super-stuck.sh
set -u
src=/home/jos/Projects/yserver/tools/vng-scenarios/xtest-super-probe.c
cc -O1 -o probe "$src" -lX11 > cc.log 2>&1 || cat cc.log >&2
if [ "${YS_NO_WM:-0}" != 1 ]; then
    awesome -c /etc/xdg/awesome/rc.lua > awesome.log 2>&1 &
    sleep 5
fi
xterm -geometry 60x20+80+60 > xterm.log 2>&1 &
sleep 2
{
    ./probe before
    xdotool key super+5; sleep 1
    ./probe after-super+5
    xdotool key super; sleep 1
    ./probe after-super
    xdotool key 5; sleep 1
    ./probe after-5
    xdotool key super+5; sleep 1
    ./probe after-super+5-again
} > probe.log 2>&1 || true
cat probe.log
