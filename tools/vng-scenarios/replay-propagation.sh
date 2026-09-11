# Sourced by tools/vng-shot.sh INSIDE the guest, with DISPLAY=:7 exported and
# the artifact directory as cwd. Issue #141 — after AllowEvents(ReplayPointer),
# does a core press propagate past a child that selected XI2 but not core?
#
# Xorg stops the propagation walk as soon as ANY flavour delivers (XI2, then
# XI1, then core, per window — dix/events.c:2895-2916), so an XI2-only child
# absorbs the press and the WM's frame never sees it. yserver's
# pointer_propagation_target_by_id consults core masks only, so it keeps
# walking and hands the frame a press Xorg never sends — which arms OpenBox's
# drag state and produces the spontaneous Move/Resize of #141.
#
# Deliberately runs NO window manager: the probe plays both roles itself (one
# connection owns the parent + passive grab, another owns the XI2-only child)
# and a WM would reparent the parent out from under the synthetic click. The
# parent is override-redirect for the same reason.
#
# Pure protocol — no rendering involved — so `--dump none` is right and the
# load-bearing artifact is client.log, which ends in a table to diff against
# the same run with `--server xorg`.
#
#   tools/vng-shot.sh --server xorg --dump none --name rp-xorg \
#       --scenario tools/vng-scenarios/replay-propagation.sh
#   tools/vng-shot.sh --dump none --name rp-ys \
#       --scenario tools/vng-scenarios/replay-propagation.sh
set -u
src=$(dirname "$0")/../replay-propagation-probe.c
[ -r "$src" ] || src=/home/jos/Projects/yserver/tools/replay-propagation-probe.c

cc -O1 -o replay-propagation-probe "$src" -lX11 -lXi -lXtst > cc.log 2>&1 || {
    echo "replay-propagation: compile failed" >&2
    cat cc.log >&2
}
if [ -x ./replay-propagation-probe ]; then
    ./replay-propagation-probe > client.log 2>&1
    echo "--- client.log ---"
    cat client.log
fi

xwininfo -root -tree > tree.txt 2>&1 || true
