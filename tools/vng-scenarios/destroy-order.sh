# Sourced by tools/vng-shot.sh INSIDE the guest (DISPLAY=:7, cwd = artifacts).
# The event stream of DestroyWindow and of the owner's disconnect on a mapped
# subtree (tools/destroy-order-probe.c): PresentConfigureNotify
# (PresentWindowDestroyed) per window, and the Ungrab focus/crossing events
# when the departing client's own grabs are released. Only the stream after
# the destroy/disconnect is kept: the setup's NotifyPointer focus events
# follow Xorg's PointerWindows, a separate divergence.
# shellcheck shell=sh
# golden: probe.log
set -u
set +e
cc -O1 -o probe "${YSERVER_REPO:?}/tools/destroy-order-probe.c" -lxcb -lxcb-xfixes -lxcb-present > cc.log 2>&1 || cat cc.log >&2
: > probe.log
ok=1
for case in "t destroy-present" "c destroy-present" "t disconnect-present-appsel" \
    "c disconnect-kbd-appsel" "t disconnect-ptr-appsel" "c disconnect-ptr-appsel"; do
    echo "== $case" >> probe.log
    # shellcheck disable=SC2086
    if ./probe $case > case.log 2>&1; then
        sed -n '/^destroy T$/,$p; /^disconnect app$/,$p' case.log >> probe.log
    else
        ok=0
        cat case.log >> probe.log
    fi
done
cat probe.log
if [ ! -x probe ]; then echo "fail: probe did not build (cc.log)" > RESULT
elif [ "$ok" != 1 ]; then echo "fail: a probe case failed (probe.log)" > RESULT
else echo pass > RESULT; fi
