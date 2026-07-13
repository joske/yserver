#!/usr/bin/env bash
set -Eeuo pipefail

usage() {
    cat <<'EOF'
Usage: tools/run-plasma-yserver.sh [DISPLAY]

Launch yserver KMS and Plasma X11 from a real Linux TTY.

Arguments:
  DISPLAY               X display number to use, default 7. Both "7" and ":7"
                        are accepted.

Environment:
  YSERVER_DISPLAY       Display number when no argument is given.
  YSERVER_BIN           yserver binary path, default target/release/yserver.
  YSERVER_BUILD=0       Do not auto-build target/release/yserver if missing.
  YSERVER_LOG_DIR       Directory for yserver.log and plasma.log.
  RUST_LOG              yserver log filter, default info.
  RUST_BACKTRACE        yserver backtrace setting, default 1.
  YSERVER_DBUS_MODE     "user" uses the login user's DBus/systemd bus when
                        available; "session" starts an isolated
                        dbus-run-session. Default is "user", which is closer
                        to a real TTY login session.
  YSERVER_ALLOW_NON_TTY=1
                        Bypass the TTY guard. Only use for debugging.

Exit:
  Ctrl-Alt-Backspace should zap yserver.
  When Plasma exits, the script terminates yserver and prints the log path.
EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
    usage
    exit 0
fi

tty_name=$(tty)
if [[ "${YSERVER_ALLOW_NON_TTY:-0}" != "1" ]]; then
    case "$tty_name" in
        /dev/tty[0-9]*) ;;
        *)
            echo "run-plasma-yserver: must be run from a real TTY, got: $tty_name" >&2
            echo "Switch with Ctrl-Alt-F3, log in, then run this script." >&2
            exit 1
            ;;
    esac
fi

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

display=${1:-${YSERVER_DISPLAY:-7}}
display=${display#:}
if ! [[ "$display" =~ ^[0-9]+$ ]]; then
    echo "run-plasma-yserver: invalid display '$display'" >&2
    exit 1
fi

yserver_bin=${YSERVER_BIN:-target/release/yserver}
if [[ ! -x "$yserver_bin" ]]; then
    if [[ "${YSERVER_BUILD:-1}" == "0" ]]; then
        echo "run-plasma-yserver: missing executable: $yserver_bin" >&2
        exit 1
    fi
    echo "run-plasma-yserver: building release yserver..."
    cargo build --release --bin yserver
fi

for cmd in dbus-run-session startplasma-x11; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "run-plasma-yserver: missing command: $cmd" >&2
        exit 1
    fi
done

if [[ "${YSERVER_SKIP_SESSION_WARNING:-0}" != "1" ]] && command -v pgrep >/dev/null 2>&1; then
    if pgrep -u "$(id -u)" -x plasmashell >/dev/null 2>&1 \
        || pgrep -u "$(id -u)" -x kwin_wayland >/dev/null 2>&1 \
        || pgrep -u "$(id -u)" -x kwin_x11 >/dev/null 2>&1; then
        cat >&2 <<'EOF'
run-plasma-yserver: existing KDE/Plasma processes are running for this user.
Running a second Plasma session can conflict through DBus, KDE services, and
per-user runtime state. For the cleanest test, log out of the graphical Plasma
session first, or continue for a quick smoke test.
EOF
        read -r -p "Press Enter to continue, or Ctrl-C to abort: " _
    fi
fi

timestamp=$(date +%Y%m%d-%H%M%S)
log_dir=${YSERVER_LOG_DIR:-${TMPDIR:-/tmp}/yserver-plasma-$timestamp}
mkdir -p "$log_dir"
yserver_log=$log_dir/yserver.log
plasma_log=$log_dir/plasma.log

ypid=
cleanup() {
    local status=$?
    trap - EXIT INT TERM
    if [[ -n "${ypid:-}" ]] && kill -0 "$ypid" >/dev/null 2>&1; then
        echo "run-plasma-yserver: stopping yserver pid $ypid"
        kill -TERM "$ypid" >/dev/null 2>&1 || true
        for _ in {1..50}; do
            if ! kill -0 "$ypid" >/dev/null 2>&1; then
                break
            fi
            sleep 0.1
        done
        if kill -0 "$ypid" >/dev/null 2>&1; then
            kill -KILL "$ypid" >/dev/null 2>&1 || true
        fi
        wait "$ypid" >/dev/null 2>&1 || true
    fi
    echo "run-plasma-yserver: logs in $log_dir"
    exit "$status"
}
trap cleanup EXIT INT TERM

echo "run-plasma-yserver: display :$display"
echo "run-plasma-yserver: logs in $log_dir"
echo "run-plasma-yserver: starting $yserver_bin"

RUST_LOG=${RUST_LOG:-info} \
RUST_BACKTRACE=${RUST_BACKTRACE:-1} \
"$yserver_bin" "$display" >"$yserver_log" 2>&1 &
ypid=$!

socket=/tmp/.X11-unix/X$display
for _ in {1..100}; do
    if [[ -S "$socket" ]]; then
        break
    fi
    if ! kill -0 "$ypid" >/dev/null 2>&1; then
        echo "run-plasma-yserver: yserver exited before creating $socket" >&2
        tail -n 80 "$yserver_log" >&2 || true
        exit 1
    fi
    sleep 0.1
done

if [[ ! -S "$socket" ]]; then
    echo "run-plasma-yserver: timed out waiting for $socket" >&2
    tail -n 80 "$yserver_log" >&2 || true
    exit 1
fi

dbus_mode=${YSERVER_DBUS_MODE:-user}
user_bus=${DBUS_SESSION_BUS_ADDRESS:-}
if [[ -z "$user_bus" && -n "${XDG_RUNTIME_DIR:-}" && -S "$XDG_RUNTIME_DIR/bus" ]]; then
    user_bus="unix:path=$XDG_RUNTIME_DIR/bus"
elif [[ -z "$user_bus" && -S "/run/user/$(id -u)/bus" ]]; then
    user_bus="unix:path=/run/user/$(id -u)/bus"
fi

plasma_env=(
    env
    -u WAYLAND_DISPLAY
    -u WAYLAND_SOCKET
    -u SESSION_MANAGER
    "DISPLAY=:$display"
    "XDG_SESSION_TYPE=x11"
    "GDK_BACKEND=x11"
    "QT_QPA_PLATFORM=xcb"
)

if [[ "$dbus_mode" == "user" && -n "$user_bus" ]]; then
    plasma_cmd=("${plasma_env[@]}" "DBUS_SESSION_BUS_ADDRESS=$user_bus" startplasma-x11)
    echo "run-plasma-yserver: starting Plasma X11 on existing user DBus bus"
else
    plasma_cmd=("${plasma_env[@]}" dbus-run-session startplasma-x11)
    echo "run-plasma-yserver: starting Plasma X11 under dbus-run-session"
fi

set +e
"${plasma_cmd[@]}" >"$plasma_log" 2>&1
plasma_status=$?
set -e

echo "run-plasma-yserver: Plasma exited with status $plasma_status"
exit "$plasma_status"
