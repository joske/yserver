# Sourced by tools/vng-shot.sh INSIDE the guest (DISPLAY=:7, cwd = artifacts).
# Issue #173: XI2 raw key events (XI_RawKeyPress / XI_RawKeyRelease). No WM,
# no windows: focus stays PointerRoot, so root selectors also see the device
# events.
#
# Two phases, both logged with the XI opcode, sequence numbers and timestamps
# blanked (xtest.log also drops the inter-event time deltas) so the Xorg and
# yserver runs diff directly:
#
# xtest.log     XTEST-driven cases: every selection form, duplicate presses,
#               a stray release, core/XI2 grabs (sync, async, passive,
#               owner_events, XI 2.0 vs 2.2 clients), XI version storage.
# physical.log  real keys on the guest's PS/2 keyboard, pressed by the host
#               through the QEMU monitor while the guest holds (a long press
#               covers auto-repeat). Needs tools/vng-scenarios/xi2-raw-keys-host.sh,
#               which runs vng-shot and presses the keys:
#
#   tools/vng-scenarios/xi2-raw-keys-host.sh yserver
#   tools/vng-scenarios/xi2-raw-keys-host.sh xorg
#   diff target/vng/xi2-raw-keys-xorg/xtest.log target/vng/xi2-raw-keys-yserver/xtest.log
#
# Expected differences outside the raw events (2026-09-26 run): xtest.log —
# XIGrabDevice(keyboard) sends the grabber XI_FocusIn/Out it never selected,
# XI2 device events carry a different pointer position, and XIQueryVersion
# answers a changed request with the new version instead of the stored one /
# BadValue; physical.log — the physical keyboard's slave id (Xorg: its own
# device, 7 here; yserver: slave 5) and auto-repeat (Xorg: XI2 KeyPress with
# XIKeyRepeat and no release; yserver: release+press pairs). Raw events agree:
# none for repeats on either server.
set -u
src=/home/jos/Projects/yserver/tools/vng-scenarios/xi2-raw-keys-probe.c
cc -O1 -o probe "$src" -lxcb -lxcb-xinput -lxcb-xtest > cc.log 2>&1 || cat cc.log >&2

./probe list > devices.log 2>&1 || true

M="mon:m22:2:1 mon:a22:2:0 mon:vck:2:3 mon:xtk:2:5 mon:m20:0:1 mon:a20:0:0"
run() {
    # MappingNotify (core event 34) is Xorg switching the master keyboard's
    # classes to the XTEST slave; yserver has one slave, so none. Not part of
    # the raw-event comparison.
    ./probe $M "$@" 2>&1 | grep -v 'core-event type=34' | sed -E 's/ dt=-?[0-9]+//'
    echo "======"
}
{
    run '# basic' p38 r38
    run '# duplicate press, non-modifier' p38 p38 r38
    run '# duplicate press, modifier (Shift_L)' p50 p50 r50
    run '# release of a key that is not down' r38
    run '# G1 core GrabKeyboard async' grabkbd:async p38 r38 ungrabkbd p38 r38
    run '# C1 core GrabKeyboard, grabber selects raw on root' drvsel:1 grabkbd:async p38 r38 ungrabkbd
    run '# G2 XIGrabDevice(3) root, raw in grab mask, grabber selects' drvsel:1 xigrab:3 p38 r38 xiungrab:3
    run '# G3 XIGrabDevice(3) child, raw in grab mask, grabber selects' drvsel:1 xigrab:3:win p38 r38 xiungrab:3
    run '# G3b XIGrabDevice(3) root, raw in grab mask' xigrab:3 p38 r38 xiungrab:3
    run '# O1 XIGrabDevice(3) child owner_events, no raw in mask, grabber selects' drvsel:1 xigrab:3:win:oe:noraw p38 r38 xiungrab:3
    run '# O2 XIGrabDevice(3) child, no raw in mask, grabber selects' drvsel:1 xigrab:3:win:noraw p38 r38 xiungrab:3
    run '# O3 XIGrabDevice(3) root owner_events, no raw in mask, grabber selects' drvsel:1 xigrab:3:oe:noraw p38 r38 xiungrab:3
    run '# O4 XIGrabDevice(3) root, no raw in mask, grabber selects' drvsel:1 xigrab:3:noraw p38 r38 xiungrab:3
    run '# G4 core GrabKeyboard sync, AllowEvents(AsyncKeyboard)' grabkbd:sync p38 r38 allow:async p39 r39 ungrabkbd
    run '# G5 passive GrabKey sync, AllowEvents(ReplayKeyboard)' grabkey:38:sync p38 r38 allow:replay p39 r39
    run '# G6 passive GrabKey async' grabkey:38:async p38 r38 p39 r39
    ./probe mon:none:-:1 mon:v20then22:0,2:1 mon:v22then20:2,0:1 mon:v23then22:3,2:1 \
        '# V1 stored XI version under a core grab' grabkbd:async p38 r38 ungrabkbd 2>&1 \
        | grep -v 'core-event type=34' | sed -E 's/ dt=-?[0-9]+//'
} > xtest.log 2>&1
cat xtest.log

# Physical phase: listen while the host presses keys during the hold.
./probe mon:mk:2:1k mon:ak:2:0 mon:m20:0:1 listen:45 > physical.log 2>&1 &
sleep 2
touch LISTENING
