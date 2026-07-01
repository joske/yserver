KERNEL := "/boot/vmlinuz-linux-cachyos"

# ============================== SETUP & ENVIRONMENT CHECKS ==============================

# Build a release yserver and install it to /usr/local/bin (needs sudo).
install:
    cargo build --release --bin yserver
    sudo install -m755 target/release/yserver /usr/local/bin/yserver
    @echo "installed /usr/local/bin/yserver — see README 'Use with a display manager' to enable it"

# ============================== CORE — RUN / HEADLESS / SSH / DEBUG / ENTRY ==============================

# Run yserver in virtme-ng with virtio-gpu DRM device and a QEMU window.
yserver:
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-display gtk -vga none -device virtio-gpu-pci -device virtio-tablet-pci -device virtio-keyboard-pci" \
        -- target/debug/yserver

# Run yserver inside the guest for `seconds`, then send SIGTERM
# from inside the guest. Exercises the signalfd shutdown path.
yserver-headless-shutdown seconds="3":
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-device virtio-gpu-pci" \
        -- bash -c 'target/debug/yserver & pid=$!; sleep {{seconds}}; kill -TERM $pid; wait $pid'

yserver-hw log="warn":
    cargo build --release --bin yserver
    bash -c '\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/release/yserver 7 > yserver-hw.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        env DISPLAY=":7" xterm -geometry 100x80-100+0;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null'

# Picks the lowest free X display by scanning /tmp/.X11-unix/, brings
# yserver up there, then runs ~/.xinitrc (or /etc/X11/xinit/xinitrc
# fallback) with the matching DISPLAY. When xinitrc exits, yserver is
# torn down. WAYLAND_* are unset belt-and-braces; a real VT wouldn't
# have them set anyway. Rejects pty / SSH / graphical-terminal callers
# via a /dev/ttyN check on stdin — mirrors real `startx`.
#
# Like real startx/xinit, it mints a per-session MIT-MAGIC-COOKIE-1 and
# hands it to yserver via -auth (an unguessable mktemp /tmp file, the
# SERVER's copy — used only to validate incoming clients). The same cookie
# is also added to the user's ~/.Xauthority keyed to :$display, and the
# session runs with XAUTHORITY pointed at ~/.Xauthority (NOT the /tmp file).
# So, exactly like real startx: the session's own clients authenticate; a
# second terminal in the same login connects with a bare DISPLAY=:$display
# (no hunting for the /tmp file); the session can also reach other X
# displays whose cookies live in ~/.Xauthority; and other local UIDs (and
# cookieless TTYs) are still refused. On teardown the :$display entry is
# removed from ~/.Xauthority and the temp server file is deleted. Needs
# xauth + mcookie on PATH.
#
# Runs STANDALONE from a bare TTY, so (unlike the `-hw` desktop
# recipes) it does NOT override XDG_RUNTIME_DIR — it inherits the TTY
# login's real /run/user/UID + systemd --user instance. That is what
# makes gcr-ssh-agent (and the keyring-unlocked SSH keys) reachable in
# the session with no extra wiring here: ~/.xinitrc must NOT repoint
# XDG_RUNTIME_DIR at a temp dir (an x11trace setup once did, which
# pointed SSH_AUTH_SOCK at a dead /tmp/.../gcr/ssh).
startx log="warn":
    cargo build --release --bin yserver
    bash -c '\
        case "$(tty)" in /dev/tty[0-9]*) ;; *) echo "startx: must be run from a TTY (got: $(tty))" >&2; exit 1;; esac;\
        display=0;\
        while [ -e /tmp/.X11-unix/X$display ]; do display=$((display+1)); done;\
        authfile=$(mktemp /tmp/yserver-startx-auth.XXXXXX);\
        userauth="${XAUTHORITY:-$HOME/.Xauthority}";\
        cookie=$(mcookie);\
        xauth -f "$authfile" add ":$display" . "$cookie";\
        xauth -f "$userauth" add ":$display" . "$cookie";\
        echo "startx: using DISPLAY=:$display (server auth $authfile; cookie also added to $userauth)";\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/release/yserver "$display" -auth "$authfile" > yserver-hw-startx.log 2>&1 &\
        yserver_pid=$!;\
        for i in $(seq 30); do [ -S /tmp/.X11-unix/X$display ] && break; sleep 1; done;\
        xinitrc=~/.xinitrc;\
        [ -f "$xinitrc" ] || xinitrc=/etc/X11/xinit/xinitrc;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET XDG_SESSION_TYPE=x11 XAUTHORITY="$userauth" DISPLAY=":$display" sh "$xinitrc" > startx.log 2>&1;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;\
        xauth -f "$userauth" remove ":$display" 2>/dev/null;\
        rm -f "$authfile"'

# Run yserver headless + wait 8 s + start xterm inside the guest.
# Use to smoke-test the xterm path without needing two terminals.
yserver-xterm:
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-device virtio-gpu-pci" \
        -- bash -c 'RUST_LOG=info RUST_BACKTRACE=1 target/debug/yserver &\
            yserver_pid=$!;\
            sleep 8;\
            DISPLAY=:7 xterm -e "echo xterm connected; sleep 10" &\
            wait $yserver_pid'

# ============================== GPU / APP SMOKE ==============================

# Phase 4.1: yserver under virtio-gpu Venus passthrough.
# Exposes a real Vulkan device inside the guest. Requires
# `vulkan-virtio` on the host (Venus ICD).
yserver-venus mode="1024x768" log="info":
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-display gtk,gl=on -vga none -device virtio-vga-gl,hostmem=4G,blob=true,venus=true -device virtio-tablet-pci -device virtio-keyboard-pci" \
        -- bash -c 'RUST_LOG="{{log}}" RUST_BACKTRACE=1 YSERVER_MODE={{mode}} target/debug/yserver'

# ============================== CINNAMON ==============================

yserver-cinnamon-hw log="warn":
    cargo build --release --bin yserver
    bash -c '\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/release/yserver > yserver-hw-cinnamon.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:7 GDK_BACKEND=x11 \
            XDG_SESSION_TYPE=x11 \
            dbus-run-session cinnamon-session > cinnamon.log 2>&1;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null'

# Release-mode cinnamon wrapped in system-wide `perf record` (see
# tools/profile-mate.sh). For triaging cinnamon choppiness — note that on
# drivers where GLX_EXT_texture_from_pixmap isn't advertised (NVIDIA
# proprietary), muffin composites via the read-pixmap -> glTexImage2D
# fallback, so yserver's GetImage / GPU-readback path is the prime hot-spot
# to look for in the flamegraph. cinnamon-session takes DISPLAY from the
# env (no --display flag), matching `yserver-cinnamon-hw`.
yserver-cinnamon-hw-perf log="warn" freq="999":
    RUST_LOG={{log}} PERF_FREQ={{freq}} \
        SESSION_NAME=cinnamon SESSION_COMMAND=cinnamon-session \
        tools/profile-mate.sh

yserver-cinnamon-hw-telemetry log="info":
    RUSTFLAGS="-C force-frame-pointers=yes" cargo build --release --bin yserver
    rm -f yserver-cinnamon.submit.tsv
    bash -c '\
        xdg_rd=$(mktemp -d -t yserver-run.XXXXXX); chmod 700 "$xdg_rd";\
        YSERVER_LOOP_TELEMETRY=1 YSERVER_SUBMIT_TRACE=yserver-cinnamon.submit.tsv \
            RUST_LOG="{{log}}" RUST_BACKTRACE=1 \
            target/release/yserver > yserver-hw-cinnamon.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:7 GDK_BACKEND=x11 \
            XDG_SESSION_TYPE=x11 XDG_RUNTIME_DIR="$xdg_rd" \
            dbus-run-session cinnamon-session > cinnamon.log 2>&1;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;\
        rm -rf "$xdg_rd" 2>/dev/null;'

yserver-cinnamon-hw-trace log="trace":
    cargo build --bin yserver
    rm -f cinnamon.xtrace
    bash -c '\
        xdg_rd=$(mktemp -d -t yserver-run.XXXXXX); chmod 700 "$xdg_rd";\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/debug/yserver > yserver-hw-cinnamon.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        x11trace -d :7 -D :8 -n -o cinnamon.xtrace &\
        xtrace_pid=$!;\
        sleep 1;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:8 GDK_BACKEND=x11 \
            XDG_SESSION_TYPE=x11 XDG_RUNTIME_DIR="$xdg_rd" \
            dbus-run-session cinnamon-session > cinnamon.log 2>&1;\
        kill -TERM $xtrace_pid $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;\
        rm -rf "$xdg_rd" 2>/dev/null;'

# ============================== MATE ==============================

yserver-mate mode="1024x768" log="trace":
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-display gtk,gl=on -vga none -device virtio-vga-gl,hostmem=4G,blob=true,venus=true,xres=1024,yres=768 -device virtio-tablet-pci -device virtio-keyboard-pci" \
        -- bash -c '\
            export MESA_LOADER_DRIVER_OVERRIDE=zink;\
            RUST_LOG="{{log}}" RUST_BACKTRACE=1 YSERVER_MODE={{mode}} target/debug/yserver > yserver.log 2>&1 &\
            yserver_pid=$!;\
            for i in $(seq 30); do if [ -e /tmp/.X11-unix/X7 ]; then break; fi; sleep 1; done;\
            env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:7 GDK_BACKEND=x11 \
                XDG_SESSION_TYPE=x11 \
                dbus-run-session mate-session --display :7 > mate.log 2>&1 &\
            wait $yserver_pid'

yserver-mate-hw log="warn":
    cargo build --release --bin yserver
    bash -c '\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/release/yserver > yserver-hw-mate.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:7 GDK_BACKEND=x11 \
            XDG_SESSION_TYPE=x11 \
            dbus-run-session mate-session --display :7 > mate.log 2>&1;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null'

# Release-mode mate wrapped in system-wide `perf record`. See
# tools/profile-mate.sh for what it captures and how to read the trace.
# Set `STRACE=1` in the env to also attach strace to caja the moment it
# spawns (writes caja.strace; useful for "what is caja sitting in poll()
# on for 25s").
yserver-mate-hw-perf log="warn" freq="999":
    RUST_LOG={{log}} PERF_FREQ={{freq}} tools/profile-mate.sh

# Release-mode mate with logging turned down to `warn`. Use this to
# test whether pointer lag under hover is dominated by env_logger /
# stderr formatting cost (observed at ~5% of CPU under debug+debug
# build) or by the underlying paint pipeline. If hover responds
# noticeably faster than `yserver-mate-hw`, logging was the bottleneck.
#
# Build is forced with `-C force-frame-pointers=yes` so that
# `perf record --call-graph fp` can walk the stack reliably for
# flamegraphs. Without this, optimized Rust release builds produce
# ~66% [unknown] frames in the flamegraph (DWARF unwinding fails
# partway through inlined call chains). ~1-2% runtime cost; harmless
# for general release use, essential for profiling.
yserver-mate-hw-release log="warn":
    RUSTFLAGS="-C force-frame-pointers=yes" cargo build --release --bin yserver
    bash -c '\
        xdg_rd=$(mktemp -d -t yserver-run.XXXXXX); chmod 700 "$xdg_rd";\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/release/yserver > yserver-hw-mate.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:7 GDK_BACKEND=x11 \
            XDG_SESSION_TYPE=x11 XDG_RUNTIME_DIR="$xdg_rd" \
            dbus-run-session mate-session --display :7 > mate.log 2>&1;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;\
        rm -rf "$xdg_rd" 2>/dev/null;'

# Release-build counterpart to `yserver-mate-hw-trace`: builds with
# `--release` (so perf characteristics match real-world) but still
# wires `x11trace` between mate-session and yserver, dumping the
# protocol stream to `mate.xtrace`. Use when comparing wire-level
# behaviour to `mate-xephyr-trace`'s `mate-xorg.xtrace` — the trace
# recipe above produces a debug-built log that is ~3-5× slower per
# request, which can mask or distort timing-related symptoms.
#
# Defaults `RUST_LOG=warn` so yserver-hw-mate.log stays compact; pass
# `log=...` to crank specific targets for a cross-reference run.
yserver-mate-hw-release-trace log="warn":
    RUSTFLAGS="-C force-frame-pointers=yes" cargo build --release --bin yserver
    rm -f mate.xtrace
    bash -c '\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/release/yserver > yserver-hw-mate.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        x11trace -d :7 -D :8 -n -o mate.xtrace &\
        xtrace_pid=$!;\
        sleep 1;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:8 GDK_BACKEND=x11 \
            XDG_SESSION_TYPE=x11 \
            dbus-run-session mate-session --display :8 > mate.log 2>&1;\
        kill -TERM $xtrace_pid $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;'

# Release-mode mate with the core-loop telemetry enabled (see
# `LoopTelemetry` in `crates/yserver-core/src/core_loop/run.rs`).
# Emits one info!-level line per second to yserver-hw.log with
# iter/s, req/s, drain_max, top opcodes, host_input gap, etc.
#
# Also writes a per-vkQueueSubmit2 TSV to `yserver-${session}.submit.tsv`
# (Stage 5 Task 3 paint-aggregation diagnostic, see
# crates/yserver/src/kms/v2/submit_trace.rs). One row per submit:
#   frame_id ns_mono kind target_kind target_id batch_size op \
#   src_class mask_class pipeline_id readback alias zero_draws upload
# Quick analyses:
#   awk -F'\t' 'NR>1{c[$3]++} END{for(k in c) print c[k],k}' \
#       yserver-mate.submit.tsv | sort -rn
#   awk -F'\t' 'NR>1 && $3==pk && $5==pt {run++; next} \
#       {if(run>1) print run,pk,pt; run=1; pk=$3; pt=$5}' \
#       yserver-mate.submit.tsv | sort -rn | head
#
# Use to diagnose input-loop starvation on bee/adapta — reproduce
# the lag, then `grep "loop telemetry" yserver-hw.log` for the
# rollups. RUST_LOG defaults to `info` so the telemetry lines come
# through; pass `log=warn` if you need quieter output, but you'll
# lose the rollup lines (they're info!-level).
yserver-mate-hw-telemetry log="info":
    RUSTFLAGS="-C force-frame-pointers=yes" cargo build --release --bin yserver
    rm -f yserver-mate.submit.tsv
    bash -c '\
        xdg_rd=$(mktemp -d -t yserver-run.XXXXXX); chmod 700 "$xdg_rd";\
        YSERVER_LOOP_TELEMETRY=1 YSERVER_SUBMIT_TRACE=yserver-mate.submit.tsv \
            RUST_LOG="{{log}}" RUST_BACKTRACE=1 \
            target/release/yserver > yserver-hw-mate.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:7 GDK_BACKEND=x11 \
            XDG_SESSION_TYPE=x11 XDG_RUNTIME_DIR="$xdg_rd" \
            dbus-run-session mate-session --display :7 > mate.log 2>&1;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;\
        rm -rf "$xdg_rd" 2>/dev/null;'

# MATE on yserver/KMS with x11trace recording the full X11 wire
# protocol between clients and yserver. Follows the server default
# cursor strategy, currently SW cursor.
yserver-mate-hw-trace log="trace":
    cargo build --bin yserver
    rm -f mate.xtrace
    bash -c '\
        xdg_rd=$(mktemp -d -t yserver-run.XXXXXX); chmod 700 "$xdg_rd";\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 \
            YSERVER_V2_SCENE_WALK_ALL=1 \
            target/debug/yserver > yserver-hw-mate.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        x11trace -d :7 -D :8 -n -o mate.xtrace &\
        xtrace_pid=$!;\
        sleep 1;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:8 GDK_BACKEND=x11 \
            XDG_SESSION_TYPE=x11 XDG_RUNTIME_DIR="$xdg_rd" \
            dbus-run-session mate-session --display :8 > mate.log 2>&1;\
        kill -TERM $xtrace_pid $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;\
        rm -rf "$xdg_rd" 2>/dev/null;'

# Counterpart to `yserver-mate-hw` with Vulkan validation + RADV
# hang reporting wired in for tracking down GPU VM faults / device
# losses. Use when yserver wedges with `ERROR_DEVICE_LOST` on a
# RADV-driven AMD card/APU:
#   - YSERVER_VK_VALIDATION=1 + VK_INSTANCE_LAYERS turns on the
#     Khronos validation layer (needs `vulkan-validation-layers`
#     installed; the loader will warn-and-continue if absent).
#   - VK_LAYER_ENABLES=...SYNCHRONIZATION_VALIDATION_EXT pinpoints
#     missing layout/cache barriers (the most likely class of bug
#     for a TCP texture-read VM fault).
#   - RADV_DEBUG=hang,syncshaders makes RADV insert wait-idle
#     around every shader stage and dump GPU state to
#     ~/radv_dumps/ when a hang/fault fires. syncshaders is slow
#     by design — that's the point; it makes the offending submit
#     localizable.
#   - MESA_VK_ABORT_ON_DEVICE_LOSS=1 aborts the process on the
#     first device-lost rather than letting hundreds of downstream
#     RendererFailed warnings drown the actual cause.
# Writes logs to `yserver-hw-mate-vkdebug.log` so the baseline
# `yserver-hw-mate.log` is preserved for diffing.
yserver-mate-hw-vkdebug log="trace":
    cargo build --bin yserver
    bash -c '\
        xdg_rd=$(mktemp -d -t yserver-run.XXXXXX); chmod 700 "$xdg_rd";\
        RUST_LOG="{{log}}" RUST_BACKTRACE=full \
            YSERVER_VK_VALIDATION=1 \
            VK_INSTANCE_LAYERS=VK_LAYER_KHRONOS_validation \
            VK_LAYER_ENABLES=VK_VALIDATION_FEATURE_ENABLE_SYNCHRONIZATION_VALIDATION_EXT \
            RADV_DEBUG=hang,syncshaders \
            MESA_VK_ABORT_ON_DEVICE_LOSS=1 \
            target/debug/yserver > yserver-hw-mate-vkdebug.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:7 GDK_BACKEND=x11 \
            XDG_SESSION_TYPE=x11 XDG_RUNTIME_DIR="$xdg_rd" \
            dbus-run-session mate-session --display :7 > mate-vkdebug.log 2>&1;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;\
        rm -rf "$xdg_rd" 2>/dev/null;\
        echo "yserver log: yserver-hw-mate-vkdebug.log";\
        echo "mate log:    mate-vkdebug.log";\
        echo "radv dumps:  ~/radv_dumps/ (if any)";'

# ============================== XFCE ==============================

yserver-xfce-hw log="warn":
    cargo build --release --bin yserver
    bash -c '\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/release/yserver > yserver-hw-xfce.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:7 GDK_BACKEND=x11 YSERVER_V2_SCENE_WALK_ALL=1\
            XDG_SESSION_TYPE=x11 \
            dbus-run-session xfce4-session --display :7 > xfce.log 2>&1;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null'

yserver-xfce-hw-perf log="warn" freq="999":
    RUST_LOG={{log}} PERF_FREQ={{freq}} \
        SESSION_NAME=xfce SESSION_COMMAND='xfce4-session --display :7' \
        tools/profile-mate.sh

yserver-xfce-hw-telemetry log="info":
    RUSTFLAGS="-C force-frame-pointers=yes" cargo build --release --bin yserver
    rm -f yserver-xfce.submit.tsv
    bash -c '\
        YSERVER_LOOP_TELEMETRY=1 YSERVER_SUBMIT_TRACE=yserver-xfce.submit.tsv \
            RUST_LOG="{{log}}" RUST_BACKTRACE=1 \
            target/release/yserver > yserver-hw-xfce.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:7 GDK_BACKEND=x11 \
            XDG_SESSION_TYPE=x11 \
            dbus-run-session xfce4-session --display :7 > xfce.log 2>&1;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;'

# xfce on yserver with x11trace recording the full X11 wire
# protocol between clients and yserver. xfce-session connects to
# the fake display `:8`; x11trace tunnels everything to yserver
# on `:7` and dumps a human-readable per-request/per-event trace
# to `xfce.xtrace`. Use to diff against an Xorg-side capture
# (see `xfce-xorg-trace`) when debugging GTK popup placement,
# rubber-band selection, or any "works on Xorg, broken on
# yserver" client-side bug.
yserver-xfce-hw-trace log="debug":
    cargo build --bin yserver
    rm -f xfce.xtrace
    bash -c '\
        xdg_rd=$(mktemp -d -t yserver-run.XXXXXX); chmod 700 "$xdg_rd";\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/debug/yserver > yserver-hw-xfce.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        x11trace -d :7 -D :8 -n -o xfce.xtrace &\
        xtrace_pid=$!;\
        sleep 1;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:8 GDK_BACKEND=x11 \
            XDG_SESSION_TYPE=x11 XDG_RUNTIME_DIR="$xdg_rd" \
            dbus-run-session xfce4-session --display :8 > xfce.log 2>&1;\
        kill -TERM $xtrace_pid $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;\
        rm -rf "$xdg_rd" 2>/dev/null;'

# ============================== ENLIGHTENMENT (e16) ==============================

yserver-e16-xterm mode="1024x768" log="trace":
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-display gtk,gl=on -vga none -device virtio-vga-gl,hostmem=4G,blob=true,venus=true,xres=1024,yres=768 -device virtio-tablet-pci -device virtio-keyboard-pci" \
        -- bash -c '\
            export MESA_LOADER_DRIVER_OVERRIDE=zink;\
            RUST_LOG="{{log}}" RUST_BACKTRACE=1 YSERVER_MODE={{mode}} target/debug/yserver > yserver.log 2>&1 &\
            yserver_pid=$!;\
            sleep 3;\
            DISPLAY=:7 e16 > e16.log 2>&1 &\
            sleep 3;\
            DISPLAY=:7 xterm &\
            wait $yserver_pid'

yserver-e16-xterm-hw log="debug":
    cargo build --bin yserver
    bash -c '\
        unset WAYLAND_DISPLAY WAYLAND_SOCKET;\
        export GDK_BACKEND=x11;\
        export XDG_SESSION_TYPE=x11;\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 YSERVER_OPS_SAFE=1 target/debug/yserver > yserver-hw-e16.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        DISPLAY=:7 e16 > e16-hw.log 2>&1 &\
        sleep 2;\
        DISPLAY=:7 wezterm;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;'

# e16 + wezterm on yserver with x11trace recording the X11 wire
# protocol between clients and yserver. e16 connects to the fake
# display `:8`; x11trace tunnels everything to yserver on `:7` and
# dumps a human-readable per-request/per-event trace to `e16.xtrace`.
# Use to diff against an Xorg-side capture when debugging e16
# hover-popup gating or other event-flow oddities.
yserver-e16-xterm-hw-trace log="debug":
    cargo build --bin yserver
    rm -f e16.xtrace
    bash -c '\
        unset WAYLAND_DISPLAY WAYLAND_SOCKET;\
        export GDK_BACKEND=x11;\
        export XDG_SESSION_TYPE=x11;\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 YSERVER_OPS_SAFE=1 target/debug/yserver > yserver-hw-e16.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        x11trace -d :7 -D :8 -n -o e16.xtrace &\
        xtrace_pid=$!;\
        sleep 1;\
        DISPLAY=:8 e16 > e16-hw.log 2>&1 &\
        sleep 2;\
        DISPLAY=:8 wezterm;\
        kill -TERM $xtrace_pid $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;'

yserver-e27-xterm-hw log="debug":
    cargo build --release --bin yserver
    bash -c '\
        unset WAYLAND_DISPLAY WAYLAND_SOCKET;\
        export GDK_BACKEND=x11;\
        export XDG_SESSION_TYPE=x11;\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 YSERVER_OPS_SAFE=1 target/release/yserver > yserver-hw-e27.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        DISPLAY=:7 enlightenment_start > e27-hw.log 2>&1 &\
        sleep 2;\
        DISPLAY=:7 xterm;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;'

yserver-e27-xterm-hw-trace log="debug":
    cargo build --bin yserver
    rm -f e27.xtrace
    bash -c '\
        unset WAYLAND_DISPLAY WAYLAND_SOCKET;\
        export GDK_BACKEND=x11;\
        export XDG_SESSION_TYPE=x11;\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 YSERVER_OPS_SAFE=1 target/debug/yserver > yserver-hw-e27.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        x11trace -d :7 -D :8 -n -o e27.xtrace &\
        xtrace_pid=$!;\
        sleep 1;\
        DISPLAY=:8 EINA_LOG_LEVELS="ecore_x:4,ecore_input:4,ecore_evas:4,ecore:3,e:4" E_DEBUG=1 enlightenment_start > e27-hw.log 2>&1 &\
        sleep 2;\
        DISPLAY=:8 xterm;\
        kill -TERM $xtrace_pid $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;'

# ============================== openbox ==============================

yserver-openbox-hw log="info":
    cargo build --bin yserver
    bash -c '\
        unset WAYLAND_DISPLAY WAYLAND_SOCKET;\
        export GDK_BACKEND=x11;\
        export XDG_SESSION_TYPE=x11;\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 YSERVER_OPS_SAFE=1 target/debug/yserver > yserver-hw-openbox.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        DISPLAY=:7 openbox > openbox.log 2>&1 ;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;'

yserver-openbox-picom-hw log="info":
    cargo build --bin yserver
    bash -c '\
        unset WAYLAND_DISPLAY WAYLAND_SOCKET;\
        export GDK_BACKEND=x11;\
        export XDG_SESSION_TYPE=x11;\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 YSERVER_OPS_SAFE=1 target/debug/yserver > yserver-hw-openbox-picom.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        DISPLAY=:7 openbox > openbox-picom.log 2>&1 &\
        sleep 2;\
        DISPLAY=:7 picom --backend glx --log-level debug --log-file picom.log > picom.out 2>&1;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;'

# ============================== awesome ==============================

yserver-awesome-hw log="info":
    cargo build --bin yserver
    bash -c '\
        unset WAYLAND_DISPLAY WAYLAND_SOCKET;\
        export GDK_BACKEND=x11;\
        export XDG_SESSION_TYPE=x11;\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 YSERVER_OPS_SAFE=1 target/debug/yserver > yserver-hw-awesome.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        DISPLAY=:7 awesome > awesome.log 2>&1 &\
        sleep 2;\
        DISPLAY=:7 wezterm ;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;'

yserver-awesome-picom-hw log="yserver_core::core_loop::process_request=debug":
    cargo build --bin yserver
    bash -c '\
        unset WAYLAND_DISPLAY WAYLAND_SOCKET;\
        export GDK_BACKEND=x11;\
        export XDG_SESSION_TYPE=x11;\
        stdbuf -oL -eL env RUST_LOG="{{log}}" RUST_BACKTRACE=1 YSERVER_OPS_SAFE=1 target/debug/yserver > yserver-hw-awesome.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        DISPLAY=:7 awesome > awesome.log 2>&1 &\
        sleep 2;\
        DISPLAY=:7 picom --backend glx --log-level debug --log-file picom.log > picom.out 2>&1 &\
        picom_pid=$!;\
        sleep 1;\
        DISPLAY=:7 xterm;\
        kill -TERM $picom_pid 2>/dev/null;\
        wait $picom_pid 2>/dev/null;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;'

yserver-awesome-picom-hw-trace log="debug":
    cargo build --bin yserver
    bash -c '\
        unset WAYLAND_DISPLAY WAYLAND_SOCKET;\
        export GDK_BACKEND=x11;\
        export XDG_SESSION_TYPE=x11;\
        stdbuf -oL -eL env RUST_LOG="{{log}}" RUST_BACKTRACE=1 YSERVER_OPS_SAFE=1 target/debug/yserver > yserver-hw-awesome.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        x11trace -k -d :7 -D :8 -n -o awesome-picom-xorg.xtrace &\
        xtrace_pid=$!;\
        DISPLAY=:7 awesome > awesome.log 2>&1 &\
        sleep 2;\
        DISPLAY=:8 picom --backend glx --log-level debug --log-file picom.log > picom.out 2>&1 &\
        picom_pid=$!;\
        sleep 1;\
        DISPLAY=:8 xterm;\
        kill -TERM $picom_pid $xtrace_pid 2>/dev/null;\
        wait $picom_pid 2>/dev/null;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;'

# ============================== FVWM3 ==============================

# Idle-rate check under fvwm3 (a quiet, non-polling WM — unlike e16's pager).
# Brings up yserver + fvwm3 with YSERVER_LOOP_TELEMETRY=1, telemetry -> the same
# target/yserver-telemetry.log we watch. Leave it idle (don't touch input) and
# the per-second "vk call rate" / "loop telemetry" lines should show
# compose=0 / req/s=0 once settled — the decisive "yserver reaches 0/s" gate
# (the cursor-damage idle fix). Zap or Ctrl-C to stop.
yserver-fvwm3-idle log="info":
    cargo build --bin yserver
    bash -c '\
        unset WAYLAND_DISPLAY WAYLAND_SOCKET;\
        export GDK_BACKEND=x11 XDG_SESSION_TYPE=x11;\
        RUST_LOG="{{log}}" YSERVER_LOOP_TELEMETRY=1 RUST_BACKTRACE=1 target/debug/yserver > target/yserver-telemetry.log 2>&1 &\
        yserver_pid=$!;\
        for i in $(seq 100); do [ -S /tmp/.X11-unix/X7 ] && break; sleep 0.1; done;\
        DISPLAY=:7 fvwm3 > target/fvwm3-idle.log 2>&1 &\
        echo "fvwm3 up on :7; telemetry -> target/yserver-telemetry.log. Leave idle; watch compose rate. Zap/Ctrl-C to stop.";\
        wait $yserver_pid 2>/dev/null;'

# Bring up yserver + fvwm3 + xterm in one QEMU window. The WM starts
# before xterm so the terminal gets framed. Logs to yserver.log on the
# host side via the shared cwd. Override resolution with `mode=WxH`.
#
# yserver runs in the background; xterm is the foreground process so
# closing it terminates the recipe (yserver dies with the guest).
yserver-fvwm3-xterm mode="1024x768" log="trace":
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-display gtk,gl=on -vga none -device virtio-vga-gl,hostmem=4G,blob=true,venus=true,xres=1024,yres=768 -device virtio-tablet-pci -device virtio-keyboard-pci" \
        -- bash -c '\
            export MESA_LOADER_DRIVER_OVERRIDE=zink;\
            RUST_LOG="{{log}}" RUST_BACKTRACE=1 YSERVER_MODE={{mode}} target/debug/yserver > yserver.log 2>&1 &\
            yserver_pid=$!;\
            sleep 2;\
            DISPLAY=:7 fvwm3 > fvwm3.log 2>&1 &\
            sleep 2;\
            DISPLAY=:7 xterm &\
            wait $yserver_pid'

yserver-fvwm3-xterm-hw log="debug":
    cargo build --bin yserver
    bash -c '\
        RUST_LOG="{{log}},yserver::kms::v2::scene=debug" RUST_BACKTRACE=1 target/debug/yserver > yserver-hw-fvwm3.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        DISPLAY=:7 fvwm3 > fvwm3-hw.log 2>&1 &\
        sleep 8;\
        DISPLAY=:7 wezterm;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;'

# ============================== WINDOW MAKER ==============================

yserver-wmaker-xterm mode="1024x768" log="trace":
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-display gtk,gl=on -vga none -device virtio-vga-gl,hostmem=4G,blob=true,venus=true,xres=1024,yres=768 -device virtio-tablet-pci -device virtio-keyboard-pci" \
        -- bash -c '\
            export MESA_LOADER_DRIVER_OVERRIDE=zink;\
            RUST_LOG="{{log}}" RUST_BACKTRACE=1 YSERVER_MODE={{mode}} target/debug/yserver > yserver.log 2>&1 &\
            yserver_pid=$!;\
            sleep 2;\
            DISPLAY=:7 wmaker > wmaker.log 2>&1 &\
            sleep 2;\
            DISPLAY=:7 xterm &\
            wait $yserver_pid'

yserver-wmaker-xterm-hw log="debug":
    cargo build --bin yserver
    bash -c '\
        xdg_rd=$(mktemp -d -t yserver-run.XXXXXX); chmod 700 "$xdg_rd";\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/debug/yserver > yserver-hw-wmaker.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:7 GDK_BACKEND=x11 YSERVER_V2_SCENE_WALK_ALL=1\
            XDG_SESSION_TYPE=x11 XDG_RUNTIME_DIR="$xdg_rd" \
            wmaker > wmaker-hw.log 2>&1 &\
        sleep 2;\
        DISPLAY=:7 wezterm;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;\
        rm -rf "$xdg_rd" 2>/dev/null;'

# wmaker + wezterm on yserver with x11trace tunnelling. wmaker connects
# to the fake display `:8`; x11trace forwards every request/event to
# yserver on `:7` and writes a per-request/per-event trace to
# `wmaker.xtrace`. Use when debugging which window/drawable wmaker is
# painting (the yserver debug log omits drawable xids on PolySegment /
# PolyFillRectangle / ClearArea); compare against an Xorg capture or
# read alongside `yserver-hw-wmaker.log`.
yserver-wmaker-xterm-hw-trace log="debug":
    cargo build --bin yserver
    rm -f wmaker.xtrace
    bash -c '\
        xdg_rd=$(mktemp -d -t yserver-run.XXXXXX); chmod 700 "$xdg_rd";\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/debug/yserver > yserver-hw-wmaker.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        x11trace -d :7 -D :8 -n -o wmaker.xtrace &\
        xtrace_pid=$!;\
        sleep 1;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:8 GDK_BACKEND=x11 YSERVER_V2_SCENE_WALK_ALL=1\
            XDG_SESSION_TYPE=x11 XDG_RUNTIME_DIR="$xdg_rd" \
            wmaker > wmaker-hw.log 2>&1 &\
        sleep 2;\
        DISPLAY=:8 wezterm;\
        kill -TERM $xtrace_pid $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;\
        rm -rf "$xdg_rd" 2>/dev/null;'

# ============================== PROBES & MISC HW ==============================

# Input-device hotplug probe for the "mouse doesn't return after monitor
# off->on" issue (2026-06-14, vs GNOME-Wayland which recovers it). Runs yserver
# + fvwm3 AND a UTC-timestamped `udevadm monitor` of the input subsystem, so we
# can correlate WHEN the kernel re-creates the mouse node vs WHEN yserver picks
# it up. Procedure: run from a VT, then physically power the monitor OFF, wait
# ~5s, power ON, move the mouse; then zap/Ctrl-C. Compare:
#   target/yserver-hotplug.log  — yserver `xi-device`/`libinput` add/remove
#   target/udev-input.log       — kernel/udev input device add/remove
# kernel-add at screen-on but yserver-add only later => yserver hotplug bug.
yserver-input-hotplug-probe log="info":
    cargo build --bin yserver
    rm -f target/udev-input.log target/yserver-hotplug.log
    bash -c '\
        unset WAYLAND_DISPLAY WAYLAND_SOCKET;\
        export GDK_BACKEND=x11 XDG_SESSION_TYPE=x11;\
        ( stdbuf -oL udevadm monitor --udev --kernel --subsystem-match=input 2>&1 | while IFS= read -r l; do printf "%s %s\n" "$(date -u +%H:%M:%S.%3N)" "$l"; done > target/udev-input.log ) &\
        udev_pid=$!;\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/debug/yserver > target/yserver-hotplug.log 2>&1 &\
        yserver_pid=$!;\
        for i in $(seq 100); do [ -S /tmp/.X11-unix/X7 ] && break; sleep 0.1; done;\
        DISPLAY=:7 fvwm3 > target/fvwm3-hotplug.log 2>&1 &\
        echo "up. NOW: power monitor OFF, wait ~5s, power ON, move mouse. Then zap/Ctrl-C.";\
        echo "logs: target/yserver-hotplug.log + target/udev-input.log";\
        wait $yserver_pid 2>/dev/null;\
        kill $udev_pid 2>/dev/null;'

# Bring up yserver ALONE on :7 (no compositor / no GL client), then run
# the GLX TFP probe as the FIRST and only client — so its dri3-screen /
# texture_from_pixmap result is representative of muffin's first-client
# position. Run from a free VT/tty on the HW box (needs DRM master).
# Output: probe result on stdout + /tmp/tfp-probe.out, yserver log in
# yserver-hw-bare.log. yserver is killed when the probe finishes.
yserver-tfp-probe-hw log="warn":
    cargo build --bin yserver
    gcc tools/glx-tfp-probe.c -lGL -lX11 -o ./tfp-probe
    bash -c '\
        xdg_rd=$(mktemp -d -t yserver-run.XXXXXX); chmod 700 "$xdg_rd";\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/debug/yserver > yserver-hw-bare.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        echo "=== GLX TFP probe (sole client) ===";\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:7 \
            XDG_RUNTIME_DIR="$xdg_rd" LIBGL_DEBUG=verbose \
            ./tfp-probe 2>&1 | tee tfp-probe.out \
            | grep -iE "screen|texture_from_pixmap|USABLE|matching|radeonsi|cfg [0-9]|returned";\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;\
        rm -rf "$xdg_rd" 2>/dev/null'

# ============================== RENDERCHECK ==============================

# Run rendercheck (X RENDER smoke suite) against ynest on `display`.
# `tests` is a comma-separated list. Default budget is 600s/test —
# `composite` / `cacomposite` / `repeat` are intrinsically slow
# (massive operator × format × source enumeration). Set timeout=N to
# override.
rendercheck-ynest display="99" geometry="1024x768" timeout="600" tests="fill,dcoords,scoords,mcoords,tscoords,tmcoords,blend,composite,cacomposite,gradients,repeat,triangles,bug7366":
    cargo build --release --bin ynest
    DISPLAY=:0 RUST_LOG=warn target/release/ynest {{display}} --geometry {{geometry}} > ynest-rc.log 2>&1 & \
        pid=$!; \
        trap "kill $pid 2>/dev/null; wait" INT TERM EXIT; \
        sleep 1; \
        tools/rendercheck.sh :{{display}} {{timeout}} {{tests}}

# Run rendercheck against yserver (KMS) inside virtme-ng.
rendercheck-yserver timeout="600" tests="fill,dcoords,scoords,mcoords,tscoords,tmcoords,blend,composite,cacomposite,gradients,repeat,triangles,bug7366":
    cargo build --release --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-display egl-headless,gl=on -vga none -device virtio-vga-gl,hostmem=4G,blob=true,venus=true -device virtio-tablet-pci -device virtio-keyboard-pci" \
        -- tools/yserver-vng-run.sh rendercheck {{timeout}} {{tests}}

# Run rendercheck on host
rendercheck-yserver-hw timeout="60" tests="fill,dcoords,scoords,mcoords,tscoords,tmcoords,blend,composite,cacomposite,gradients,repeat,triangles,bug7366":
    tools/yserver-vng-run.sh rendercheck {{timeout}} {{tests}}

# ============================== XTS ==============================

# Boot ynest on `display` and run an xts5 scenario against it.
# `scenario` matches an entry in xts5/tet_scen (Xproto, Xlib3, …, all).
# Tally lands in xts/results/<timestamp>/summary.
xts-ynest scenario="Xproto" display="99" geometry="1024x768" timeout="600":
    cargo build --release --bin ynest
    DISPLAY=:0 RUST_LOG=warn target/release/ynest {{display}} --geometry {{geometry}} > ynest-xts.log 2>&1 & \
        pid=$!; \
        trap "kill $pid 2>/dev/null; wait" INT TERM EXIT; \
        sleep 1; \
        tools/xts-run.sh :{{display}} {{scenario}} {{timeout}}

# Run an xts5 scenario against yserver (KMS) inside virtme-ng.
# Boots vng once with yserver in the background (headless QEMU,
# virtio-gpu KMS), polls for the X socket on :7, then runs the same
# xts harness ynest uses. Result tree lands in xts/results/ on the
# host because vng mounts the host rootfs --rw.
# NOTE: uses the Venus GPU-passthrough display config (same as
# rendercheck), NOT `-device virtio-gpu-pci -display none`. The
# headless-no-display config leaves yserver's KMS pageflips with no
# completion event, which stalls the compose path and wedges clients
# drawing to windows — the draw-heavy scenarios (Xlib9) hung there.
# egl-headless gives a working display+flip path so the tests
# complete. Timeout is generous because GetImage-heavy verification
# runs slow under the guest's software/Venus Vulkan.
xts-yserver scenario="Xproto" timeout="1200":
    cargo build --release --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-display egl-headless,gl=on -vga none -device virtio-vga-gl,hostmem=4G,blob=true,venus=true -device virtio-tablet-pci -device virtio-keyboard-pci" \
        -- tools/yserver-vng-run.sh xts {{scenario}} {{timeout}}

xts-yserver-hw scenario="Xproto" timeout="1200":
    cargo build --release --bin yserver
    bash -c '\
        case "$(tty)" in /dev/tty[0-9]*) ;; *) echo "startx: must be run from a TTY (got: $(tty))" >&2; exit 1;; esac;\
        display=0;\
        while [ -e /tmp/.X11-unix/X$display ]; do display=$((display+1)); done;\
        echo "xts: using DISPLAY=:$display";\
        target/release/yserver "$display" > yserver-hw-xts.log 2>&1 &\
        yserver_pid=$!;\
        for i in $(seq 30); do [ -S /tmp/.X11-unix/X$display ] && break; sleep 1; done;\
        env DISPLAY=":$display" xset s off -dpms; \
        env DISPLAY=":$display" xterm -geometry 100x80-100+0 -e "tools/xts-run.sh :$display {{scenario}} {{timeout}}";\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null'
