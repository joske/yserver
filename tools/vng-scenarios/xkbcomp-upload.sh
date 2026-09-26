# Sourced by tools/vng-shot.sh INSIDE the guest (DISPLAY=:7, cwd = artifacts).
# Issue #171 phase 4: `xkbcomp keymap.xkb $DISPLAY` must change the keymap XKB
# clients (Xlib with XKB, xkbcommon-x11 toolkits) and the server's own key
# cooking use, as on Xorg; and `xkbcomp -xkb` -> upload -> `xkbcomp -xkb` must
# round-trip. No WM, the server's default layout on both servers.
#
#   KERNEL=/usr/lib/modules/$(uname -r)/vmlinuz tools/vng-shot.sh --dump none \
#       --name xkbcomp-upload-ys --scenario tools/vng-scenarios/xkbcomp-upload.sh
#   KERNEL=/usr/lib/modules/$(uname -r)/vmlinuz tools/vng-shot.sh --server xorg \
#       --dump none --name xkbcomp-upload-xorg \
#       --scenario tools/vng-scenarios/xkbcomp-upload.sh
#
# 1. round trip: dump0 = `xkbcomp -xkb`, upload dump0 unchanged, dump1; the
#    diff dump0 -> dump1 (roundtrip.diff) is what an upload changes.
# 2. edit dump1: <AC01>/<AC02> symbols swapped, <CAPS> = Control_L under
#    Control (the xkb way to make Caps a Control); upload it with a listener
#    running, dump2; then inject keys with xdotool (by keycode) and read them
#    through an Xlib XKB client (probe listen) and xkbcommon-x11 (probe
#    xkbcommon).
# probe.log is the load-bearing artifact (no ids or timestamps, so the Xorg
# and yserver runs diff directly); roundtrip.diff and edit.diff are the dump
# diffs.
set -u
src=/home/jos/Projects/yserver/tools/vng-scenarios/xmodmap-caps-control-probe.c
cc -O1 -o probe "$src" -lX11 -lxcb -lxkbcommon -lxkbcommon-x11 > cc.log 2>&1 \
    || cat cc.log >&2

# Xorg (no -noreset) regenerates when its last client leaves, which reloads
# the keymap: keep one connection open for the whole scenario.
xprop -root -spy > /dev/null 2>&1 &
keeper=$!
sleep 1

keys() {
    xmodmap -pm
    xmodmap -pke | grep -E '^keycode  (38|39|66) ' || true
    ./probe xkbmap "xkbmap:$1" 66 37 38 39 || true
    ./probe xkbcommon || true
}

{
    echo "== before"
    keys before
    echo "== round trip"
    xkbcomp -xkb "$DISPLAY" dump0.xkb > xkbcomp-dump0.log 2>&1 && rc=0 || rc=$?; echo "dump0 rc=$rc"
    xkbcomp dump0.xkb "$DISPLAY" > xkbcomp-upload0.log 2>&1 && rc=0 || rc=$?; echo "upload dump0 rc=$rc"
    xkbcomp -xkb "$DISPLAY" dump1.xkb > xkbcomp-dump1.log 2>&1 && rc=0 || rc=$?; echo "dump1 rc=$rc"
    diff dump0.xkb dump1.xkb > roundtrip.diff || true
    echo "roundtrip.diff: $(grep -c '^[<>]' roundtrip.diff || true) changed lines"
    echo "== after the unchanged upload"
    keys roundtrip
} > probe.log 2>&1

python3 - dump1.xkb edited.xkb <<'EOF' >> probe.log 2>&1 || echo "edit: failed" >> probe.log
import re, sys
t = open(sys.argv[1]).read()
def sub1(pat, rep, s, flags=0):
    s, n = re.subn(pat, rep, s, count=1, flags=flags)
    assert n == 1, pat
    return s
a = re.search(r'key <AC01> \{(.*?)\};', t, re.S)
b = re.search(r'key <AC02> \{(.*?)\};', t, re.S)
assert a and b and a.start() < b.start()
t = t[:a.start(1)] + b.group(1) + t[a.end(1):b.start(1)] + a.group(1) + t[b.end(1):]
t = sub1(r'key <CAPS> \{[^\n]*\};', 'key <CAPS> {         [       Control_L ] };', t)
t = sub1(r'modifier_map Lock \{ <CAPS> \};', 'modifier_map Control { <CAPS> };', t)
open(sys.argv[2], 'w').write(t)
print("edit: <AC01>/<AC02> swapped, <CAPS> = Control_L under Control")
EOF

./probe listen > listen.log 2>&1 &
listener=$!
sleep 2

{
    echo "== upload the edit"
    xkbcomp edited.xkb "$DISPLAY" > xkbcomp-upload1.log 2>&1 && rc=0 || rc=$?; echo "upload edited rc=$rc"
    sleep 1
    xkbcomp -xkb "$DISPLAY" dump2.xkb > xkbcomp-dump2.log 2>&1 && rc=0 || rc=$?; echo "dump2 rc=$rc"
    diff dump1.xkb dump2.xkb > edit.diff || true
    echo "edit.diff: $(grep -c '^[<>]' edit.diff || true) changed lines"
    diff edited.xkb dump2.xkb > reread.diff || true
    echo "reread.diff (uploaded vs dumped back): $(grep -c '^[<>]' reread.diff || true) changed lines"
    echo "== after"
    keys after
    echo "== inject: key 38, key 39"
    xdotool key 38; sleep 0.5
    xdotool key 39; sleep 1
    echo "== inject: keydown 66, key 38, keyup 66"
    xdotool keydown 66 key 38 keyup 66; sleep 1
    echo "== inject: key 66, then key 38"
    xdotool key 66; sleep 0.5
    xdotool key 38; sleep 1
    echo "== end state"
    ./probe xkbcommon || true
} >> probe.log 2>&1

kill "$listener" "$keeper" 2>/dev/null || true
wait "$listener" "$keeper" 2>/dev/null || true
{
    echo "== listener"
    cat listen.log
    echo "== roundtrip.diff"
    cat roundtrip.diff
    echo "== edit.diff"
    cat edit.diff
    echo "== reread.diff"
    cat reread.diff
} >> probe.log
cat probe.log
