# Sourced by tools/vng-shot.sh INSIDE the guest (DISPLAY=:7, cwd = artifacts).
# CDE where it is installed (/usr/dt; not in CI): dtcalc and dtpad, read
# back with GetImage (legacy-shot.c), match a live Xorg run in the same
# guest, and the Xsession comes up and stays up — dtsession sets
# MIT-SCREEN-SAVER attributes and exits on an error — with dtwm managing a
# dtcalc, its front panel switching workspaces and input still flowing
# after (cde-apps-root.sh starts what ToolTalk needs). Skips without /usr/dt.
# shellcheck shell=sh
# golden: shots.log
set -u
set +e
if [ ! -x /usr/dt/bin/dtsession ]; then
    echo "skip: CDE is not installed (/usr/dt)" > RESULT
    return 0 2> /dev/null || exit 0
fi
export PATH=/usr/dt/bin:$PATH
cc -O1 -o legacy-shot "${YSERVER_REPO:?}/tools/vng-scenarios/legacy-shot.c" -lxcb > cc.log 2>&1 || cat cc.log >&2
: > shots.log
shot() { # NAME TITLE-PATTERN
    id=$(timeout 60 xdotool search --sync --onlyvisible --name "$2" 2> /dev/null | head -1)
    if [ -z "$id" ]; then echo "$1: never mapped" >> shots.log; return; fi
    sleep 3
    ./legacy-shot "$id" "$1" >> shots.log 2>&1
}
printf 'A dtpad buffer\nwith three lines\nof text\n' > dtpad.txt
dtcalc -geometry +0+0 > dtcalc.log 2>&1 &
pids=$!
shot dtcalc Calculator
# The pointer between dtcalc and dtpad: no focus, so no text cursor. Moved
# while dtcalc is connected: Xorg resets, recentring it, when the last
# client leaves.
xdotool mousemove 478 790
dtpad -standAlone -xrm '*blinkRate: 0' -geometry 500x300+500+0 dtpad.txt > dtpad.log 2>&1 &
pids="$pids $!"
shot dtpad dtpad.txt
# shellcheck disable=SC2086
kill $pids 2> /dev/null
sleep 1
/usr/dt/bin/Xsession > xsession.log 2>&1 &
session=$!
for _ in $(seq 1 180); do
    xprop -root _MOTIF_WM_INFO 2> /dev/null | grep -q '^_MOTIF_WM_INFO(' && break
    sleep 1
done
echo "Xsession: dtwm $(xprop -root _MOTIF_WM_INFO 2> /dev/null | grep -q '^_MOTIF_WM_INFO(' && echo up || echo missing)" >> shots.log
# The front panel's workspace switch, which dtwm holds with a synchronous
# passive button grab: to workspace Two (the xterm unmaps) and back to One,
# then type into the xterm.
fp=$(timeout 60 sh -c 'until xdotool search --classname FrontPanel; do sleep 1; done' 2> /dev/null | head -1)
echo "Xsession: front panel $([ -n "$fp" ] && echo present || echo missing)" >> shots.log
xterm -geometry 40x5+100+100 -e sh -c 'stty -icanon; head -c 8 > typed.txt' > xterm.log 2>&1 &
xt=$(timeout 60 xdotool search --sync --onlyvisible --classname xterm 2> /dev/null | head -1)
sleep 2
# The switch is the panel's one window 200-300 wide and at least 60 high,
# its four buttons in two rows of two.
sw=$(xwininfo -tree -id "${fp:-0}" 2> /dev/null | awk '{ for (i = 1; i <= NF; i++) if (split($i, g, /[x+]/) == 4 && g[1] >= 200 && g[1] <= 300 && g[2] >= 60) { print $1; exit } }')
wmwin=$(xprop -root _MOTIF_WM_INFO 2> /dev/null | sed 's/.*, //')
workspace() {
    echo "Xsession: after $1, $(xprop -id "$wmwin" _DT_WORKSPACE_CURRENT 2> /dev/null | sed 's/.*= //'), xterm $(xwininfo -id "${xt:-0}" 2> /dev/null | grep -o 'Map State: [A-Za-z]*')" >> shots.log
}
if [ -n "$sw" ]; then
    eval "$(xdotool getwindowgeometry --shell "$sw")"
    xdotool mousemove --window "$sw" $((WIDTH * 3 / 4)) $((HEIGHT / 4)) mousedown 1 sleep 0.3 mouseup 1
    sleep 3
    workspace Two
    xdotool mousemove --window "$sw" $((WIDTH / 4)) $((HEIGHT / 4)) mousedown 1 sleep 0.3 mouseup 1
    sleep 3
    workspace One
else
    echo "Xsession: no workspace switch" >> shots.log
fi
xdotool mousemove --window "${xt:-0}" 20 20 mousedown 1 sleep 0.3 mouseup 1
sleep 2
xdotool type --delay 50 'panelok!'
sleep 2
echo "Xsession: xterm got '$(cat typed.txt 2> /dev/null)'" >> shots.log
dtcalc > dtcalc-dtwm.log 2>&1 &
calc=$!
id=$(timeout 60 xdotool search --sync --onlyvisible --name Calculator 2> /dev/null | head -1)
sleep 2
echo "Xsession: dtcalc $(xprop -id "${id:-0}" WM_STATE 2> /dev/null | grep -o 'window state: [A-Za-z]*' || echo 'not managed')" >> shots.log
echo "Xsession: $(kill -0 $session 2> /dev/null && echo running || echo exited)" >> shots.log
kill $calc $session 2> /dev/null
cat shots.log
if [ ! -x legacy-shot ]; then echo "fail: legacy-shot did not build (cc.log)" > RESULT
elif grep -q "never mapped\|failed" shots.log; then echo "fail: a client did not show (shots.log)" > RESULT
else echo pass > RESULT; fi
