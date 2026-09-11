# Sourced by tools/vng-shot.sh INSIDE the guest, with DISPLAY=:7 exported and
# the artifact directory as cwd. Xorg's depth-32 background-alpha rule
# (mi/miexpose.c:487-511) — does bg_pixel's alpha survive into storage, and
# does that depend on the ancestor chain?
#
# Deliberately runs NO window manager: a reparenting WM would move the
# "direct child of root" windows under a frame and collapse the two chains
# the probe is built to tell apart. Works unchanged on yserver and on Xorg
# (`--server xorg`), which makes it a parity check.
#
# The load-bearing artifact is client.log (the per-window readback table),
# not the scanout — nothing composites here, so the scanout only shows the
# unblended case.
set -u
src=$(dirname "$0")/../depth32-bg-probe.c
[ -r "$src" ] || src=/home/jos/Projects/yserver/tools/depth32-bg-probe.c

cc -O1 -o depth32-bg-probe "$src" -lX11 -lXcomposite > cc.log 2>&1 || {
    echo "depth32-bg: compile failed" >&2
    cat cc.log >&2
}
if [ -x ./depth32-bg-probe ]; then
    ./depth32-bg-probe --no-redirect --hold 0 > client.log 2>&1 &
    sleep 4
fi

xwininfo -root -tree > tree.txt 2>&1 || true
