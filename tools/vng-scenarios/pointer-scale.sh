# Sourced by tools/vng-shot.sh INSIDE the guest (DISPLAY=:7, cwd = artifacts).
# Issue #185 Q6: does relative device motion (QEMU PS/2 mouse, via
# pointer-scale-host.sh) move the root pointer the same distance under a CRTC
# scale transform as without one?
set -u
xprop -root -spy > /dev/null 2>&1 &
hold=$!
sleep 1
out=$(xrandr | awk '/ connected/{print $1; exit}')
if command -v xinput > /dev/null; then
    for id in $(xinput list --id-only); do
        xinput set-prop "$id" 'libinput Accel Profile Enabled' 0 1 2>/dev/null || true
    done
    echo "accel: flat" >> pointer.log
else
    echo "accel: default (no xinput)" >> pointer.log
fi
phase() {
    xdotool mousemove 200 200
    sleep 0.5
    echo "=== $1 start $(xdotool getmouselocation)" >> pointer.log
    touch "READY-$1"
    for _ in $(seq 1 120); do [ -e "DONE-$1" ] && break; sleep 0.5; done
    sleep 0.5
    echo "=== $1 end   $(xdotool getmouselocation)" >> pointer.log
}
phase identity
xrandr --output "$out" --scale 2x2
xrandr | grep -E '^Screen' >> pointer.log
phase scale2
xrandr --output "$out" --scale 1x1 || true
kill $hold 2>/dev/null || true
