# Sourced by tools/vng-shot.sh INSIDE the guest (DISPLAY=:7, cwd = artifacts).
# What a restack, move, resize, unmap, destroy, reparent, shape change or
# CirculateWindow uncovers is exposed, as Xorg's ValidateTree does (#213: a
# frame mapped under another window and then raised). restack-expose-probe.c
# has the stages; it runs without a compositor and under an Automatic and a
# Manual RedirectSubwindows of the root.
# shellcheck shell=sh
# golden: direct.log automatic.log manual.log
set -u
set +e
src=${YSERVER_REPO:?}/tools/vng-scenarios/restack-expose-probe.c
cc -O1 -o probe "$src" -lxcb -lxcb-composite -lxcb-shape > cc.log 2>&1 || cat cc.log >&2
ok=1
for m in direct automatic manual; do
    rm -f PROBE-DONE
    ./probe $m > $m.log 2>&1
    [ -e PROBE-DONE ] || ok=0
done
cat direct.log automatic.log manual.log
if [ ! -x probe ]; then echo "fail: probe did not build (cc.log)" > RESULT
elif [ $ok = 0 ]; then echo "fail: the probe stopped early (direct.log, automatic.log, manual.log)" > RESULT
else echo pass > RESULT; fi
