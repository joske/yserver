# Sourced by tools/vng-shot.sh INSIDE the guest (DISPLAY=:7, cwd = artifacts).
# Issue #248: does a compositor (picom's DamageSubtract pattern) get a
# DamageNotify after the MapNotify of a re-mapped top-level? Pure protocol, no
# WM; diff probe.log against the same run with `--server xorg`.
#
#   tools/vng-shot.sh --server xorg --dump none --name dam-xorg \
#       --scenario tools/vng-scenarios/damage-after-map.sh
#   tools/vng-shot.sh --dump none --name dam-ys \
#       --scenario tools/vng-scenarios/damage-after-map.sh
# shellcheck shell=sh
# golden: probe.log
set -u
set +e
src=${YSERVER_REPO:?}/tools/damage-after-map-probe.c
cc -O1 -o probe "$src" -lxcb -lxcb-damage -lxcb-composite -lxcb-xfixes > cc.log 2>&1 || cat cc.log >&2
timeout 120 ./probe > probe.log 2>&1
cat probe.log
if [ ! -x probe ]; then echo "fail: probe did not build (cc.log)" > RESULT
elif ! grep -qx done probe.log; then echo "fail: the probe stopped early (probe.log)" > RESULT
else echo pass > RESULT; fi
