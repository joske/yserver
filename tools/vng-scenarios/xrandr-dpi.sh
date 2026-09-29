# Sourced by tools/vng-shot.sh INSIDE the guest (DISPLAY=:7, cwd = artifacts).
# Issue #132: `xrandr --dpi` must reach NEW clients' setup reply (Xft's DPI).
#   tools/vng-shot.sh --dump none --name dpi-ys --scenario tools/vng-scenarios/xrandr-dpi.sh
#   tools/vng-shot.sh --server xorg --dump none --name dpi-xorg --scenario tools/vng-scenarios/xrandr-dpi.sh
set -u
# Hold a connection: Xorg resets (reverting the mm) when its last client leaves.
xprop -root -spy > /dev/null 2>&1 &
hold=$!
sleep 1
{
    echo "=== before"; xdpyinfo | grep -E 'dimensions|resolution'
    xrandr --dpi 108
    echo "=== after xrandr --dpi 108"; xdpyinfo | grep -E 'dimensions|resolution'
} > dpi.log 2>&1 || true
kill $hold 2>/dev/null || true
