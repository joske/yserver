# Sourced by tools/vng-shot.sh INSIDE the guest (DISPLAY=:7, cwd = artifacts).
# RENDER glyphset churn past a full glyph atlas, then a glyph id redefined:
# every glyph drawn must match its own bitmap (glyph-atlas-probe.c).
# shellcheck shell=sh
# golden: probe.log
set -u
set +e
cc -O1 -o probe "${YSERVER_REPO:?}/tools/vng-scenarios/glyph-atlas-probe.c" -lxcb -lxcb-render > cc.log 2>&1 || cat cc.log >&2
./probe > probe.log 2> timing.log
status=$?
cat probe.log timing.log
if [ ! -x probe ]; then echo "fail: probe did not build (cc.log)" > RESULT
elif [ ! -e PROBE-DONE ]; then echo "fail: the probe stopped early (probe.log)" > RESULT
elif [ "$status" -ne 0 ]; then echo "fail: glyphs drawn wrong or missing (probe.log)" > RESULT
else echo pass > RESULT; fi
