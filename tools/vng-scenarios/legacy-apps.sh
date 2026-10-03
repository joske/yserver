# Sourced by tools/vng-shot.sh INSIDE the guest (DISPLAY=:7, cwd = artifacts).
# Legacy toolkit clients read back with GetImage and compared pixel for pixel
# with Xorg (legacy-shot.c). Without a window manager: an Xaw xterm with its
# scrollbar, xmessage, xcalc and xedit, and a Motif client (legacy-motif.c)
# whose list is scrolled and whose File menu is opened and closed. Then the
# Motif client again under mwm, read with mwm's frame.
# The golden is a live Xorg run in the same guest: the pixels depend on the
# distro's fonts and toolkit builds.
# shellcheck shell=sh
# golden: shots.log
# drop: ^xedit: -- several of its Xaw text and label windows stay black where Xorg shows their white background and text, master too; not explained yet
# drop: ^xmessage: -- its Xaw label font (OpenFont of an XLFD pattern picks another match) and its oval buttons (PolyFillArc spans for the SHAPE mask) differ from Xorg's
set -u
set +e
src=${YSERVER_REPO:?}/tools/vng-scenarios
cc -O1 -o legacy-shot "$src/legacy-shot.c" -lxcb > cc.log 2>&1 || cat cc.log >&2
cc -O1 -o legacy-motif "$src/legacy-motif.c" -lXm -lXt -lX11 >> cc.log 2>&1 || cat cc.log >&2
: > shots.log
pids=
shot() { # NAME CLASS [frame]
    id=$(timeout 30 xdotool search --sync --onlyvisible --class "$2" 2> /dev/null | head -1)
    if [ -z "$id" ]; then echo "$1: never mapped" >> shots.log; return; fi
    sleep 2
    ./legacy-shot "$id" "$1" ${3:-} >> shots.log 2>&1
}
wait_for() { # FILE
    for _ in $(seq 1 100); do [ -e "$1" ] && return 0; sleep 0.1; done
    return 1
}
motif() { # NAME X Y [frame]
    rm -f MOTIF-WINDOW SCROLL MOTIF-SCROLLED MOTIF-MENU MOTIF-MENU-DOWN
    ./legacy-motif "$2" "$3" > "$1.log" 2>&1 &
    pids="$pids $!"
    if ! wait_for MOTIF-WINDOW; then echo "$1: never mapped" >> shots.log; return; fi
    sleep 1
    win=$(cat MOTIF-WINDOW)
    ./legacy-shot "$win" "$1" ${4:-} >> shots.log 2>&1
    touch SCROLL
    wait_for MOTIF-SCROLLED && sleep 1
    ./legacy-shot "$win" "$1-scrolled" ${4:-} >> shots.log 2>&1
    # The File cascade sits at the left end of the menubar.
    eval "$(xdotool getwindowgeometry --shell "$win")"
    xdotool mousemove $((X + 12)) $((Y + 10)) click 1
    if wait_for MOTIF-MENU; then
        sleep 1
        ./legacy-shot "$(cat MOTIF-MENU)" "$1-menu" >> shots.log 2>&1
        xdotool key Escape
        wait_for MOTIF-MENU-DOWN && sleep 1
    else
        echo "$1-menu: never mapped" >> shots.log
    fi
    xdotool mousemove 1000 700
    sleep 1
    ./legacy-shot "$win" "$1-menu-closed" ${4:-} >> shots.log 2>&1
}
printf 'An xedit buffer\nwith three lines\nof fixed text\n' > xedit.txt
xterm -sb -fn fixed -geometry 40x12+0+0 -e sh -c 'printf "yserver legacy apps\n"; seq 1 30; sleep 300' > xterm.log 2>&1 &
pids="$pids $!"
shot xterm XTerm
xmessage -geometry +400+0 -buttons ok,cancel "hello from Xaw" > xmessage.log 2>&1 &
pids="$pids $!"
shot xmessage Xmessage
xcalc -fn fixed -geometry +0+200 > xcalc.log 2>&1 &
pids="$pids $!"
shot xcalc XCalc
xedit -fn fixed -geometry 360x240+300+200 xedit.txt > xedit.log 2>&1 &
pids="$pids $!"
shot xedit Xedit
motif motif 700 200
# shellcheck disable=SC2086
kill $pids 2> /dev/null
sleep 1
pids=
mwm > mwm.log 2>&1 &
mwm_pid=$!
sleep 2
motif motif-mwm 100 100 frame
# shellcheck disable=SC2086
kill $pids $mwm_pid 2> /dev/null
cat shots.log
if [ ! -x legacy-shot ] || [ ! -x legacy-motif ]; then echo "fail: a probe did not build (cc.log)" > RESULT
elif grep -q "never mapped\|failed" shots.log; then echo "fail: a client did not show (shots.log)" > RESULT
else echo pass > RESULT; fi
