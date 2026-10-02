# Sourced by tools/vng-shot.sh INSIDE the guest (DISPLAY=:7, cwd = artifacts).
# #196: steady pixmap churn, a burst of distinct sizes, then idle; the pool's
# bounds are checked on the host from the server's telemetry
# (pixmap-pool-host.sh / pixmap-pool-check.py).
# shellcheck shell=sh
set -u
set +e
cc -O1 -o probe "${YSERVER_REPO:?}/tools/vng-scenarios/pixmap-pool-probe.c" -lxcb > cc.log 2>&1 \
    || cat cc.log >&2
./probe > probe.log 2>&1
if [ ! -x probe ]; then echo "fail: probe did not build (cc.log)" > RESULT
elif ! grep -q '^PHASE idle end' probe.log; then echo "fail: the probe stopped early (probe.log)" > RESULT
else echo pass > RESULT; fi
