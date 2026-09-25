# Sourced by tools/vng-shot.sh INSIDE the guest (DISPLAY=:7, cwd = artifacts).
# Issue #171: `xmodmap` remapping Caps Lock to Control must reach XKB clients
# (Xlib with XKB, xkbcommon-x11 toolkits) and the server's own key cooking.
# No WM, default (us) layout on both servers — setxkbmap is not used because
# the guest Xorg was seen ignoring `-layout gb`.
#
#   tools/vng-shot.sh --dump none --name xmodmap-caps-ys \
#       --scenario tools/vng-scenarios/xmodmap-caps-control.sh
#   tools/vng-shot.sh --server xorg --dump none --name xmodmap-caps-xorg \
#       --scenario tools/vng-scenarios/xmodmap-caps-control.sh
#
# probe.log is the load-bearing artifact; it contains no ids or timestamps
# so the Xorg and yserver runs diff directly.
set -u
src=/home/jos/Projects/yserver/tools/vng-scenarios/xmodmap-caps-control-probe.c
cc -O1 -o probe "$src" -lX11 -lxcb -lxkbcommon -lxkbcommon-x11 > cc.log 2>&1 \
    || cat cc.log >&2

{
    echo "== before"
    xmodmap -pm
    xmodmap -pke | grep '^keycode  66' || true
    ./probe xkbmap xkbmap:before || true
    ./probe xkbcommon || true
} > probe.log 2>&1

./probe listen > listen.log 2>&1 &
listener=$!
sleep 2

{
    echo "== xmodmap"
    xmodmap -e 'remove Lock = Caps_Lock' -e 'keycode 66 = Control_L' \
        -e 'add Control = Control_L' && echo "xmodmap rc=0" || echo "xmodmap rc=$?"
    sleep 1
    echo "== after"
    xmodmap -pm
    xmodmap -pke | grep '^keycode  66' || true
    ./probe xkbmap xkbmap:after || true
    ./probe xkbcommon || true
    echo "== inject: keydown 66, key a, keyup 66"
    xdotool keydown 66 key a keyup 66; sleep 1
    echo "== inject: key 66, then key a"
    xdotool key 66; sleep 0.5
    xdotool key a; sleep 1
    echo "== end state"
    ./probe xkbcommon || true
} >> probe.log 2>&1

kill "$listener" 2>/dev/null || true
wait "$listener" 2>/dev/null || true
{
    echo "== listener"
    cat listen.log
} >> probe.log
cat probe.log
