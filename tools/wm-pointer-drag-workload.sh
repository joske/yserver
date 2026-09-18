#!/usr/bin/env bash
# Deterministic POINTER-driven drag/resize workload, for comparing what a
# window manager asks of yserver against what it asks of real Xorg.
#
# WHY THIS EXISTS, AND WHY NOT damage-workload.sh
#
# `tools/damage-workload.sh` moves its window with `xdotool windowmove`, which
# is a ConfigureRequest from a client. That never enters a reparenting WM's
# pointer-drag loop at all. The interesting path for a stutter report is the
# other one: press a button inside the frame and move the pointer, so the WM
# runs its own motion loop and emits geometry traffic per motion event. evilwm's
# `client_move_drag` does exactly one `XMoveWindow` on the frame plus one
# `send_config` (a synthetic ConfigureNotify) per MotionNotify, with no motion
# compression — it consumes events one at a time through `XMaskEvent` — so the
# request rate it generates is set by how fast the server delivers motion.
# Issue #155 reported stutter while dragging under evilwm, and op12
# ConfigureWindow was measured at 0.42ms average / 47ms worst in that telemetry,
# so "how many of these per drag, on each server" is the question.
#
# WHY A FIXED STEP COUNT AND NOT A DURATION
#
# x11trace has no timestamp option, so absolute counts from two traces are
# duration-confounded and cannot be compared unless the workload emits the same
# number of events both times. Every phase here is therefore driven by a FIXED
# step count, not by a wall-clock deadline, and `xdotool --sync` is used so each
# step is known to have landed before the next is sent. Two runs on two servers
# then produce request counts that differ only in what the server and WM did.
# This is the opposite choice from damage-workload.sh, which is time-bounded
# because it measures per-second telemetry rollups rather than trace totals.
#
# The pointer, not the keyboard, is the clock here: it is the only input that
# both servers deliver on their own schedule, which is exactly the variable
# under test.
#
# USAGE
#
#   DISPLAY=:7 tools/wm-pointer-drag-workload.sh <phases-log> [steps] [step-px]
#
# Phase boundaries are written to <phases-log> in the same ISO-8601 UTC format
# damage-workload.sh uses, so `tools/damage-phases.py` can join them against
# `render_telemetry:` / `loop telemetry` lines from the same run.
#
# evilwm binds button1=move and button2=resize, triggered either inside the
# frame or anywhere in the window with mask2 held; mask2 defaults to Alt. Alt is
# held here so the grab does not depend on hitting a border, whose width varies
# per WM. Other WMs that use Alt+Button1 to move (most of them) work unchanged.

set -u

phases=${1:?usage: wm-pointer-drag-workload.sh <phases-log> [steps] [step-px]}
steps=${2:-60}
step_px=${3:-6}

readonly TERM_TITLE=yserver-drag-workload
# Fixed start geometry. Away from the top-left so the drag cannot walk the
# window off-screen and have the WM clamp it, which would make the motion
# count and the geometry traffic disagree.
readonly TERM_GEOM=+400+300
readonly TERM_SIZE=(700 500)

: >"$phases"

mark() {
    printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$1" >>"$phases"
}

cleanup() {
    # Always let go of the pointer and the modifier. A workload that dies
    # mid-drag with Alt+button1 still held leaves the session unusable and the
    # next run measuring a stuck grab.
    xdotool mouseup 1 2>/dev/null
    xdotool mouseup 2 2>/dev/null
    xdotool keyup alt 2>/dev/null
    [ -n "${term_pid:-}" ] && kill -TERM "$term_pid" 2>/dev/null
    mark shutdown
}
trap cleanup EXIT

for tool in xdotool xterm; do
    command -v "$tool" >/dev/null || {
        echo "wm-pointer-drag-workload: $tool not found" >&2
        exit 1
    }
done

# `-e` execs its argument directly with no shell, so a shell one-liner would be
# taken as a program name and xterm would exit at once.
xterm -title "$TERM_TITLE" -geometry "$TERM_GEOM" \
    -e sleep infinity >/dev/null 2>&1 &
term_pid=$!

# Poll rather than sleep-and-hope: a slow first map must not read as a missing
# WM, and a missing WM must not read as a slow map.
win=
for _ in $(seq 20); do
    win=$(xdotool search --name "$TERM_TITLE" 2>/dev/null | head -1)
    [ -n "$win" ] && break
    sleep 0.5
done
if [ -z "$win" ]; then
    echo "wm-pointer-drag-workload: xterm window never appeared." >&2
    echo "  xterm alive? $(kill -0 "$term_pid" 2>/dev/null && echo yes || echo NO)" >&2
    echo "  windows seen: $(xdotool search --name . 2>/dev/null | wc -l)" >&2
    exit 1
fi
mark found-window

xdotool windowsize --sync "$win" "${TERM_SIZE[0]}" "${TERM_SIZE[1]}"
xdotool windowactivate "$win" 2>/dev/null

# Startup is whole-output damage by construction, so keep it out of a measured
# phase.
mark settle
sleep 4

mark idle
sleep 5

# ── drag ─────────────────────────────────────────────────────────────
# Out `steps` and back `steps`, so the window ends where it began and the
# resize phase starts from the same geometry every run.
#
# The pointer is parked well inside the window rather than on the titlebar:
# with Alt held, evilwm grabs button1 anywhere in the frame, and depending on
# a decoration that other WMs size differently would make this workload
# WM-specific for no gain.
eval "$(xdotool getwindowgeometry --shell "$win")"
centre_x=$(( X + WIDTH / 2 ))
centre_y=$(( Y + HEIGHT / 2 ))

mark drag
xdotool mousemove --sync "$centre_x" "$centre_y"
xdotool keydown alt
xdotool mousedown 1
for _ in $(seq "$steps"); do
    xdotool mousemove_relative --sync -- "$step_px" "$step_px"
done
for _ in $(seq "$steps"); do
    xdotool mousemove_relative --sync -- "-$step_px" "-$step_px"
done
xdotool mouseup 1
xdotool keyup alt
mark drag-done

mark idle2
sleep 5

# ── resize ───────────────────────────────────────────────────────────
# Same shape with button2, which evilwm binds to resize. A resize reallocates
# window storage as well as moving an edge, so it is a different server cost
# from the move even though the WM-side traffic per motion event is similar.
eval "$(xdotool getwindowgeometry --shell "$win")"
centre_x=$(( X + WIDTH / 2 ))
centre_y=$(( Y + HEIGHT / 2 ))

mark resize
xdotool mousemove --sync "$centre_x" "$centre_y"
xdotool keydown alt
xdotool mousedown 2
for _ in $(seq "$steps"); do
    xdotool mousemove_relative --sync -- "$step_px" "$step_px"
done
for _ in $(seq "$steps"); do
    xdotool mousemove_relative --sync -- "-$step_px" "-$step_px"
done
xdotool mouseup 2
xdotool keyup alt
mark resize-done

mark idle3
sleep 5

mark done

# Emitted so a trace can be checked against what was actually driven, rather
# than against what the defaults say should have been driven.
echo "wm-pointer-drag-workload: drove $(( steps * 2 )) motion steps per phase" \
     "(${step_px}px each), 2 phases"
