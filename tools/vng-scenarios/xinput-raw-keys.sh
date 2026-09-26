# Sourced by tools/vng-shot.sh INSIDE the guest (DISPLAY=:7, cwd = artifacts).
# Issue #173 with the reporter's own client: `xinput test-xi2 --root` while
# XTEST presses F5 and a. Raw events decode through libXi, which trusts a
# raw event's sourceid only if XI GetExtensionVersion says 2.2+.
#
#   tools/vng-shot.sh --dump none --name xinput-raw-ys \
#       --scenario tools/vng-scenarios/xinput-raw-keys.sh
#   tools/vng-shot.sh --server xorg --dump none --name xinput-raw-xorg \
#       --scenario tools/vng-scenarios/xinput-raw-keys.sh
# Compare the `EVENT type 13/14` blocks in xinput.log (Xorg: device: 3 (5)).
set -u
xinput test-xi2 --root > xinput.log 2>&1 &
x=$!
sleep 2
xdotool key F5; sleep 0.3; xdotool key a; sleep 1
kill $x 2>/dev/null || true
