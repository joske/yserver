# Sourced by tools/vng-shot.sh INSIDE the guest, with DISPLAY=:7 exported and
# the artifact directory as cwd. Regression cover for the redirect-backing
# subtree reconstruction (`overlay_backing_inferiors`): overlap, nesting and
# alpha in one redirected frame. See tools/redirect-subtree-probe.c.
#
# No window manager, every window override-redirect: the probe depends on its
# own stacking order and on nothing reparenting the tree.
set -u
src=$(dirname "$0")/../redirect-subtree-probe.c
[ -r "$src" ] || src=/home/jos/Projects/yserver/tools/redirect-subtree-probe.c

cc -O1 -o redirect-subtree-probe "$src" -lX11 -lXcomposite > cc.log 2>&1 || {
    echo "redirect-subtree: compile failed" >&2
    cat cc.log >&2
}
if [ -x ./redirect-subtree-probe ]; then
    ./redirect-subtree-probe --hold 0 > client.log 2>&1 &
    sleep 6
fi

xwininfo -root -tree > tree.txt 2>&1 || true
