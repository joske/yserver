# Sourced by tools/vng-shot.sh INSIDE the guest, with DISPLAY=:7 exported and
# the artifact directory as cwd. The composited twin of depth32-bg.sh: same
# probe, same two ancestor chains, but with Composite automatic redirection on,
# so the depth-32 windows are sampled and BLENDED instead of scanned out raw.
#
# This is the half that speaks to the Plasma white/black blocks. Non-composited,
# a depth-32 window is just opaque bytes and every server agrees; the divergence
# can only appear once something actually multiplies by alpha.
#
# The probe redirects itself rather than running picom. An external compositor
# would add its own shadows, fading and opacity rules to every pixel, and then
# nothing in the capture could be attributed to the window's own alpha; with
# automatic redirection the server is the only thing blending.
#
# Still NO window manager: a reparenting WM would move the chain-A windows
# under a frame and collapse the two chains the probe exists to tell apart.
set -u
src=$(dirname "$0")/../depth32-bg-probe.c
[ -r "$src" ] || src=/home/jos/Projects/yserver/tools/depth32-bg-probe.c

cc -O1 -o depth32-bg-probe "$src" -lX11 -lXcomposite > cc.log 2>&1 || {
    echo "depth32-bg-composited: compile failed" >&2
    cat cc.log >&2
}
if [ -x ./depth32-bg-probe ]; then
    ./depth32-bg-probe --redirect --hold 0 > client.log 2>&1 &
    sleep 4
fi

xwininfo -root -tree > tree.txt 2>&1 || true
