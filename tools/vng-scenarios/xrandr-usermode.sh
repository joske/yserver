# Sourced by tools/vng-shot.sh INSIDE the guest (DISPLAY=:7, cwd = artifacts).
# Issue #360: RANDR user modes through xrandr --newmode/--addmode/--output
# --mode/--delmode/--rmmode, with Xorg's errors, the RANDR 1.0 view of the
# user mode and, via xrandr-usermode-host.sh, the scanout size while it is
# current. The mode (cvt 1000 700 50) is off the virtio-gpu output's list.
# shellcheck shell=sh
# golden: usermode.log
# golden-include: masks/randr.txt
# mask: ^([ *])\d+(\s+1000 x 700) => \1<sz>\2 -- RANDR 1.0 size index: the probed list before the user mode is the driver's
set -u
set +e
# Hold a connection: Xorg resets (freeing user modes) when its last client leaves.
xprop -root -spy > /dev/null 2>&1 &
hold=$!
sleep 1
out=$(xrandr | awk '/ connected/{print $1; exit}')
orig=$(xrandr | awk '/\*/{print $1; exit}')
failed=
step() {
    echo "===== xrandr $*"
    xrandr "$@" 2>&1
    echo "exit $?"
}
expect() {
    want=$1; shift
    step "$@" > step.log
    cat step.log
    tail -1 step.log | grep -qx "exit $want" || failed="$failed [$*]"
}
show() {
    xrandr --verbose | awk -v out="$out" '
        /^Screen/ || $1 == out { print; next }
        /\*current/ { print; next }
        $1 == "um" { print; n = 2; next }
        n > 0 { print; n-- }'
    xrandr --q1 | grep -E '1000 x 700|^\*'
    xwininfo -root | grep -E 'Width|Height'
}
{
    expect 0 --newmode um 45.75 1000 1040 1136 1272 700 703 713 723 -hsync +vsync
    expect 1 --newmode um 45.75 1000 1040 1136 1272 700 703 713 723 -hsync +vsync
    expect 0 --addmode "$out" um
    expect 0 --addmode "$out" um
    show
    expect 0 --output "$out" --mode um
    sleep 1
    show
    touch READY-usermode
    for _ in $(seq 1 120); do [ -e DONE-usermode ] && break; sleep 0.5; done
    expect 1 --rmmode um
    expect 1 --delmode "$out" um
    expect 0 --output "$out" --mode "$orig"
    sleep 1
    show
    expect 0 --delmode "$out" um
    expect 0 --rmmode um
    show
} > usermode.log 2>&1
rm -f step.log
kill $hold 2>/dev/null || true
if [ -z "$out" ]; then echo "fail: no connected output" > RESULT
elif [ -n "$failed" ]; then echo "fail: unexpected xrandr exit:$failed" > RESULT
else echo pass > RESULT; fi
