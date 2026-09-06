KERNEL := "/boot/vmlinuz-linux-cachyos"

# --- Install contract configuration --------------------------------------
# These are top-level variables, NOT recipe parameters: recipe parameters
# in just are positional, so `just install PREFIX=/usr` would bind the
# literal string "PREFIX=/usr" to the first parameter and silently install
# to the default prefix. As variables, all of these work, and the command
# line beats the environment:
#
#   DESTDIR=$pkgdir PREFIX=/usr just install     (make-like; preferred)
#   just PREFIX=/usr DESTDIR=$pkgdir install
#   just --set PREFIX /usr install
PREFIX := env_var_or_default("PREFIX", "/usr/local")
DESTDIR := env_var_or_default("DESTDIR", "")
TARGETDIR := env_var_or_default("TARGETDIR", env_var_or_default("CARGO_TARGET_DIR", "target") / "release")
# Where the /tmp/.X11-unix tmpfiles.d snippet goes. Set empty to skip it —
# correct for non-systemd Linux, FreeBSD, and prefixes systemd does not
# scan. Not decided by sniffing `uname` on the build host, which would be
# wrong when cross-staging a Linux package elsewhere.
TMPFILESDIR := env_var_or_default("TMPFILESDIR", PREFIX / "lib/tmpfiles.d")

# ============================== SETUP & ENVIRONMENT CHECKS ==============================

# Render the scdoc man page sources to roff in target/man/. Separate from
# `install` on purpose: running scdoc is a build transformation, so a
# packager runs this in %build alongside `cargo build`, and `install` only
# copies. DOCDIR is baked in so the FILES section names real paths.
man:
    #!/usr/bin/env sh
    set -eu
    command -v scdoc >/dev/null 2>&1 || {
        echo "just man: scdoc not found on PATH" >&2
        echo "  Arch:   pacman -S scdoc" >&2
        echo "  Debian: apt install scdoc" >&2
        echo "  Alpine: apk add scdoc" >&2
        exit 1; }
    docdir='{{ PREFIX }}/share/doc/yserver'
    case "$docdir" in
        *'|'*|*'&'*|*'\'*) echo "just man: PREFIX may not contain | & or \\" >&2; exit 1;;
    esac
    mkdir -p target/man
    for page in yserver starty; do
        # Deliberately not `sed ... | scdoc > out`: POSIX sh has no
        # pipefail, so a scdoc syntax error would leave a truncated .1
        # behind and still look successful.
        sed "s|@DOCDIR@|$docdir|g" "docs/man/$page.1.scd" > "target/man/$page.1.in"
        scdoc < "target/man/$page.1.in" > "target/man/$page.1"
        rm -f "target/man/$page.1.in"
        echo "just man: target/man/$page.1"
    done

# Stage an install into $DESTDIR$PREFIX. Copies only — compiles nothing, so
# a packager can call it from %install after their own build step:
#
#   cargo build --locked --release --bin yserver
#   PREFIX=/usr just man
#   DESTDIR=$pkgdir PREFIX=/usr just install
#
# Every input is checked before the first write, so a failure leaves no
# partially populated stage. Uses `install -d` + `install -m` rather than
# GNU-only `install -D`, so this works on FreeBSD.
install:
    #!/usr/bin/env sh
    set -eu
    targetdir='{{ TARGETDIR }}'
    dest='{{ DESTDIR }}{{ PREFIX }}'
    prefix='{{ PREFIX }}'
    tmpfilesdir='{{ TMPFILESDIR }}'
    case "$prefix" in
        *'|'*|*'&'*|*'\'*) echo "just install: PREFIX may not contain | & or \\" >&2; exit 1;;
    esac

    # --- Preflight: verify every input before writing anything. ---------
    # Two flags rather than pattern-matching the accumulated list:
    # "yserver" appears in both a binary path and target/man/yserver.1, so
    # a glob over the list would print the wrong hint.
    missing=''
    need_build=0
    need_man=0
    for f in "$targetdir/yserver" starty; do
        [ -f "$f" ] || { missing="$missing $f"; need_build=1; }
    done
    for f in target/man/yserver.1 target/man/starty.1; do
        [ -f "$f" ] || { missing="$missing $f"; need_man=1; }
    done
    for f in LICENSE docs/setup.md \
             examples/lightdm-99-yserver.conf.in examples/yserver.tmpfiles; do
        [ -f "$f" ] || missing="$missing $f"
    done
    if [ -n "$missing" ]; then
        echo "just install: missing input(s):" >&2
        for f in $missing; do echo "  $f" >&2; done
        [ "$need_man" -eq 0 ] || echo "run: PREFIX=$prefix just man" >&2
        [ "$need_build" -eq 0 ] || {
            echo "run: cargo build --locked --release --bin yserver" >&2
            echo "(binaries looked for in $targetdir; override with TARGETDIR=)" >&2; }
        exit 1
    fi

    # --- Binaries. Only yserver and starty; there is no ynest. ----------
    install -d "$dest/bin"
    install -m755 "$targetdir/yserver" "$dest/bin/yserver"
    install -m755 starty "$dest/bin/starty"

    # --- Man pages, uncompressed. Distro tooling owns compression. ------
    install -d "$dest/share/man/man1"
    install -m644 target/man/yserver.1 "$dest/share/man/man1/yserver.1"
    install -m644 target/man/starty.1  "$dest/share/man/man1/starty.1"

    # --- Documentation. Downstream may relocate or drop any of this to --
    # match distro policy; only bin/ and share/man/man1/ are stable.
    install -d "$dest/share/doc/yserver/examples"
    install -m644 docs/setup.md "$dest/share/doc/yserver/setup.md"
    install -m644 LICENSE       "$dest/share/doc/yserver/LICENSE"
    sed "s|@PREFIX@|$prefix|g" examples/lightdm-99-yserver.conf.in \
        > "$dest/share/doc/yserver/examples/lightdm-99-yserver.conf"
    chmod 644 "$dest/share/doc/yserver/examples/lightdm-99-yserver.conf"

    # --- tmpfiles.d, unless TMPFILESDIR is empty. -----------------------
    if [ -n "$tmpfilesdir" ]; then
        install -d "{{ DESTDIR }}$tmpfilesdir"
        install -m644 examples/yserver.tmpfiles "{{ DESTDIR }}$tmpfilesdir/yserver.conf"
    fi

    echo "just install: staged into $dest"

# Build a release yserver, render the man pages, and install both plus
# starty to /usr/local (needs sudo). Developer convenience wrapper.
install-local:
    cargo build --locked --release --bin yserver
    PREFIX=/usr/local just man
    sudo PREFIX=/usr/local just install
    @echo "installed to /usr/local — see 'man yserver' and 'man starty'"

# Verify the install contract. Builds and renders first so the smoke script
# has its inputs.
install-smoke:
    cargo build --locked --release --bin yserver
    PREFIX=/usr just man
    sh tools/install-smoke.sh

# ============================== CORE — RUN / HEADLESS / SSH / DEBUG / ENTRY ==============================

# Run yserver in virtme-ng with virtio-gpu DRM device and a QEMU window.
yserver:
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-display gtk -vga none -device virtio-gpu-pci -device virtio-tablet-pci -device virtio-keyboard-pci" \
        -- target/debug/yserver

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
# TEMPORARY — instrumented for the damage-clipped-repaint branch test
# (2026-09-02, `fix/noncomposited-damage-repaint`). Two deltas from the plain
# recipe, all to be reverted once external testing is done:
#   * `-C debug-assertions=yes` on a release build, so the four damage-model
#     invariants stay live at release speed. A violation then panics with a
#     readable message instead of showing up as a visual glitch. NOTE: changing
#     RUSTFLAGS invalidates the build cache, so the first run rebuilds fully.
#   * `YSERVER_LOOP_TELEMETRY=1` plus INFO for that one module. BOTH are
#     required: `Telemetry::maybe_emit` returns early on `!self.enabled`
#     (telemetry.rs:451), which that env var sets, so the log level alone
#     produces a completely empty log — verified the hard way.
# Deliberately prints no summary: this runs from a bare TTY, where console
# output cannot be copied. Ask for `yserver-hw-startx.log` and grep it here.
startx log="info":
    RUSTFLAGS="-C debug-assertions=yes" cargo build --release --bin yserver
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
        YSERVER_LOOP_TELEMETRY=1 RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/release/yserver "$display" -auth "$authfile" > yserver-hw-startx.log 2>&1 &\
        yserver_pid=$!;\
        for i in $(seq 30); do [ -S /tmp/.X11-unix/X$display ] && break; sleep 1; done;\
        xinitrc=~/.xinitrc;\
        [ -f "$xinitrc" ] || xinitrc=/etc/X11/xinit/xinitrc;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET XDG_SESSION_TYPE=x11 XAUTHORITY="$userauth" DISPLAY=":$display" sh "$xinitrc" > startx.log 2>&1;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;\
        xauth -f "$userauth" remove ":$display" 2>/dev/null;\
        rm -f "$authfile";\
        echo "";\
        echo "startx: done. Please send yserver-hw-startx.log from this directory."'

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
    cargo build --release --bin yserver
    rm -f yserver-cinnamon.submit.tsv
    bash -c '\
        YSERVER_LOOP_TELEMETRY=1 YSERVER_SUBMIT_TRACE=yserver-cinnamon.submit.tsv \
            RUST_LOG="{{log}}" RUST_BACKTRACE=1 \
            target/release/yserver > yserver-hw-cinnamon.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:7 GDK_BACKEND=x11 \
            XDG_SESSION_TYPE=x11 \
            dbus-run-session cinnamon-session > cinnamon.log 2>&1;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;'

yserver-cinnamon-hw-trace log="trace":
    cargo build --bin yserver
    rm -f cinnamon.xtrace
    bash -c '\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/debug/yserver > yserver-hw-cinnamon.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        x11trace -d :7 -D :8 -n -o cinnamon.xtrace &\
        xtrace_pid=$!;\
        sleep 1;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:8 GDK_BACKEND=x11 \
            XDG_SESSION_TYPE=x11  \
            dbus-run-session cinnamon-session > cinnamon.log 2>&1;\
        kill -TERM $xtrace_pid $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;'

# ============================== MATE ==============================

yserver-mate-hw log="warn":
    cargo build --release --bin yserver
    bash -c '\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/release/yserver > yserver-hw-mate.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:7 GDK_BACKEND=x11 XDG_SESSION_TYPE=x11 \
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
    cargo build --release --bin yserver
    rm -f mate.xtrace
    bash -c '\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/release/yserver > yserver-hw-mate.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        x11trace -d :7 -D :8 -k -n -o mate.xtrace &\
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
# crates/yserver/src/kms/render/submit_trace.rs). One row per submit:
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
    cargo build --release --bin yserver
    rm -f yserver-mate.submit.tsv
    bash -c '\
        YSERVER_TICK_SKIP_LOG=1 YSERVER_LOOP_TELEMETRY=1 YSERVER_SUBMIT_TRACE=yserver-mate.submit.tsv \
            RUST_LOG="{{log}}" RUST_BACKTRACE=1 \
            target/release/yserver > yserver-hw-mate.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:7 GDK_BACKEND=x11 \
            XDG_SESSION_TYPE=x11 \
            dbus-run-session mate-session --display :7 > mate.log 2>&1;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;'

# MATE on yserver/KMS with x11trace recording the full X11 wire
# protocol between clients and yserver. Follows the server default
# cursor strategy, currently SW cursor.
yserver-mate-hw-trace log="warn":
    cargo build --bin yserver
    rm -f mate.xtrace
    bash -c '\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 \
            YSERVER_SCENE_WALK_ALL=1 \
            target/debug/yserver > yserver-hw-mate.log 2>&1 &\
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
        YSERVER_SCENE_WALK_ALL=1 \
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/release/yserver > yserver-hw-xfce.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:7 GDK_BACKEND=x11 \
            XDG_SESSION_TYPE=x11 \
            dbus-run-session xfce4-session --display :7 > xfce.log 2>&1;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null'

yserver-xfce-hw-perf log="warn" freq="999":
    RUST_LOG={{log}} PERF_FREQ={{freq}} \
        SESSION_NAME=xfce SESSION_COMMAND='xfce4-session --display :7' \
        tools/profile-mate.sh

yserver-xfce-hw-telemetry log="info":
    cargo build --release --bin yserver
    rm -f yserver-xfce.submit.tsv
    bash -c '\
        YSERVER_LOOP_TELEMETRY=1 YSERVER_SUBMIT_TRACE=yserver-xfce.submit.tsv \
            YSERVER_TICK_SKIP_LOG=1 \
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
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/debug/yserver > yserver-hw-xfce.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        x11trace -d :7 -D :8 -n -o xfce.xtrace &\
        xtrace_pid=$!;\
        sleep 1;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:8 GDK_BACKEND=x11 \
            XDG_SESSION_TYPE=x11 \
            dbus-run-session xfce4-session --display :8 > xfce.log 2>&1;\
        kill -TERM $xtrace_pid $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null'

# ============================== PLASMA ==============================
yserver-plasma-hw log="info":
    cargo build --release --bin yserver
    bash -c '\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/release/yserver > yserver-hw-plasma.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:7 GDK_BACKEND=x11 \
            XDG_SESSION_TYPE=x11 \
            dbus-run-session startplasma-x11 > plasma.log 2>&1;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null'

yserver-plasma-hw-trace log="debug":
    cargo build --bin yserver
    rm -f plasma.xtrace
    bash -c '\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/debug/yserver > yserver-hw-plasma.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        x11trace -d :7 -D :8 -n -o plasma.xtrace &\
        xtrace_pid=$!;\
        sleep 1;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:8 GDK_BACKEND=x11 \
            XDG_SESSION_TYPE=x11 \
            dbus-run-session startplasma-x11 > plasma.log 2>&1;\
        kill -TERM $xtrace_pid $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null'

yserver-plasma-hw-telemetry log="info":
    cargo build --release --bin yserver
    rm -f yserver-xfce.submit.tsv
    bash -c '\
        YSERVER_LOOP_TELEMETRY=1 YSERVER_SUBMIT_TRACE=yserver-plasma.submit.tsv \
            RUST_LOG="{{log}}" RUST_BACKTRACE=1 \
            target/release/yserver > yserver-hw-plasma.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:7 GDK_BACKEND=x11 \
            XDG_SESSION_TYPE=x11 \
            dbus-run-session startplasma-x11 > plasma.log 2>&1;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null'

# ============================== ENLIGHTENMENT ==============================

yserver-e16-xterm-hw log="debug":
    cargo build --bin yserver
    bash -c '\
        unset WAYLAND_DISPLAY WAYLAND_SOCKET;\
        export GDK_BACKEND=x11;\
        export XDG_SESSION_TYPE=x11;\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/debug/yserver > yserver-hw-e16.log 2>&1 &\
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
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/debug/yserver > yserver-hw-e16.log 2>&1 &\
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

# Release-mode e16 with render telemetry, to check the damage-clipped repaint
# on a THIRD non-composited WM. e16 is a stacking, reparenting WM that runs no
# compositor, which is the regime step 4 actually changes — and it stacks, so
# unlike awesome (which tiles) windows really do overlap and the opaque-cover
# gate has something to find.
#
# Verified non-composited on 2026-09-02: a debug run of `yserver-e16-xterm-hw`
# names every request, and e16 issues only `QueryExtension "Composite"` — no
# RedirectSubwindows, no NameWindowPixmap, no DamageCreate. e16 does have a
# built-in compositor (Settings -> Composite), so if a config ever turns it on
# the clipped path stops being exercised; the tell is
# `full_reason/s[no_opaque_cover=...]` dominating.
#
# Built with `-C debug-assertions=yes` on release, so the damage-model
# invariants panic with a readable message instead of appearing as a glitch —
# which is the main thing a new-WM smoke is for. NOTE: this RUSTFLAGS value
# differs from the plain e16 recipes, so switching between them rebuilds.
#
# `YSERVER_LOOP_TELEMETRY=1` AND INFO on the telemetry module are BOTH
# required: `Telemetry::maybe_emit` returns early on `!self.enabled`
# (telemetry.rs:451), which only that env var sets, so the log level alone
# yields an empty log.
#
# Drive it by hand in the wezterm that opens — overlap some windows, drag,
# resize, restack — then close the wezterm to end the run and read:
#   grep "render_telemetry:" yserver-hw-e16.log
# The damage-repaint counters in that line:
#   clipped_repaint/s vs full_reason/s[...]  -- is the clipped path being taken
#   damage_fraction / damage_region_fraction -- painted area, and bbox waste
#   overdraw                                 -- how much is hidden behind what
#   avg_gpu_render_ns                        -- the cost being cut (+/-25% noise)
# For numbers comparable to the awesome/MATE runs, use the phased
# `yserver-e16-hw-workload` below instead — a hand-driven session's content
# load dominates and is not reproducible.
yserver-e16-hw-telemetry log="warn,yserver::startup=info,yserver::kms::render::telemetry=info":
    RUSTFLAGS="-C debug-assertions=yes" cargo build --release --bin yserver
    bash -c '\
        unset WAYLAND_DISPLAY WAYLAND_SOCKET;\
        export GDK_BACKEND=x11;\
        export XDG_SESSION_TYPE=x11;\
        YSERVER_LOOP_TELEMETRY=1 RUST_LOG="{{log}}" RUST_BACKTRACE=1 \
            target/release/yserver > yserver-hw-e16.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        DISPLAY=:7 e16 > e16-hw.log 2>&1 &\
        e16_pid=$!;\
        sleep 2;\
        DISPLAY=:7 wezterm;\
        kill -TERM $e16_pid 2>/dev/null;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;\
        echo "telemetry: grep \"render_telemetry:\" yserver-hw-e16.log"'

# The same deterministic phased workload as `yserver-{awesome,mate}-hw-workload`,
# under e16. This is the recipe whose numbers are comparable to the ones already
# recorded for those two WMs, because the events and their timings are identical
# across runs and across branches (settle, idle, drag, idle2, resize, restack,
# idle3) — see the awesome recipe above for why whole-session A/B does not work.
#
# e16 adds a third stacking model to the set: awesome tiles (nothing overlaps,
# `overdraw` pinned near 1.0), MATE stacks with panels, e16 stacks with its own
# reparenting frames and no panel. Worth a look mostly for whether the per-rect
# path holds up against a different frame/border geometry.
#
# Arguments are POSITIONAL, as everywhere in this file:
#     just yserver-e16-hw-workload ~/clip.mp4          # ~90s, measure
#     just yserver-e16-hw-workload ~/clip.mp4 0.2      # ~20s, smoke the recipe
# Keep `scale` identical between any two runs being compared.
yserver-e16-hw-workload clip scale="1" log="warn,yserver::startup=info,yserver::kms::render::telemetry=info":
    RUSTFLAGS="-C debug-assertions=yes" cargo build --release --bin yserver
    bash -c '\
        unset WAYLAND_DISPLAY WAYLAND_SOCKET;\
        export GDK_BACKEND=x11 XDG_SESSION_TYPE=x11;\
        YSERVER_LOOP_TELEMETRY=1 RUST_LOG="{{log}}" RUST_BACKTRACE=1 \
            target/release/yserver > yserver-hw-e16.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        DISPLAY=:7 e16 > e16-hw.log 2>&1 &\
        e16_pid=$!;\
        sleep 3;\
        DISPLAY=:7 tools/damage-workload.sh "{{clip}}" damage-phases.log "{{scale}}" \
            > damage-workload.log 2>&1;\
        kill -TERM $e16_pid 2>/dev/null;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;\
        echo "workload done: yserver-hw-e16.log + damage-phases.log";\
        tail -5 damage-workload.log;\
        echo "read it with: tools/damage-phases.py yserver-hw-e16.log damage-phases.log"'

yserver-e27-xterm-hw log="debug":
    cargo build --release --bin yserver
    bash -c '\
        unset WAYLAND_DISPLAY WAYLAND_SOCKET;\
        export GDK_BACKEND=x11;\
        export XDG_SESSION_TYPE=x11;\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/release/yserver > yserver-hw-e27.log 2>&1 &\
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
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/debug/yserver > yserver-hw-e27.log 2>&1 &\
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

# Release-mode e27 (enlightenment) with core-loop + submit telemetry — a WM
# telemetry harness for perf triage on a compositing WM (enlightenment composites
# via its own GL path, like cinnamon). RUST_LOG defaults to `info` so the rollups
# come through; drop to `warn` for a quieter log (you lose the rollups).
#   grep "render_telemetry"   yserver-hw-e27.log   # per-second render rollup
#   grep "loop telemetry" yserver-hw-e27.log   # iter/s + host_input gap
yserver-e27-hw-telemetry log="info":
    cargo build --release --bin yserver
    rm -f yserver-e27.submit.tsv
    bash -c '\
        xdg_rd=$(mktemp -d -t yserver-run.XXXXXX); chmod 700 "$xdg_rd";\
        unset WAYLAND_DISPLAY WAYLAND_SOCKET;\
        export GDK_BACKEND=x11;\
        export XDG_SESSION_TYPE=x11;\
        YSERVER_LOOP_TELEMETRY=1 YSERVER_SUBMIT_TRACE=yserver-e27.submit.tsv \
            RUST_LOG="{{log}}" RUST_BACKTRACE=1 \
            target/release/yserver > yserver-hw-e27.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET GDK_BACKEND=x11 \
            XDG_SESSION_TYPE=x11 XDG_RUNTIME_DIR="$xdg_rd" \
            DISPLAY=:7 enlightenment_start > e27-hw.log 2>&1 &\
        sleep 2;\
        DISPLAY=:7 wezterm ;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;\
        rm -rf "$xdg_rd" 2>/dev/null;'

# ============================== openbox ==============================

yserver-openbox-hw log="info":
    cargo build --release --bin yserver
    bash -c '\
        unset WAYLAND_DISPLAY WAYLAND_SOCKET;\
        export GDK_BACKEND=x11;\
        export XDG_SESSION_TYPE=x11;\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/release/yserver > yserver-hw-openbox.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        DISPLAY=:7 openbox > openbox.log 2>&1 ;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;'

yserver-openbox-picom-hw log="info":
    cargo build ---release -bin yserver
    bash -c '\
        unset WAYLAND_DISPLAY WAYLAND_SOCKET;\
        export GDK_BACKEND=x11;\
        export XDG_SESSION_TYPE=x11;\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/release/yserver > yserver-hw-openbox-picom.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        DISPLAY=:7 openbox > openbox-picom.log 2>&1 &\
        sleep 2;\
        DISPLAY=:7 picom --backend glx --log-level debug --log-file picom.log > picom.out 2>&1;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;'

# Release-mode openbox + picom in the XRENDER backend, with core-loop + submit
# telemetry — a WM telemetry harness with a real XRender compositor in the mix
# (the --backend glx variant above composites via GL instead). Measured clean on
# bee 2026-07-09: picom composites steadily (~40-60 composite_submits/s, 0 yserver
# faults). RUST_LOG defaults to `info` for the rollups.
#   grep "render_telemetry" yserver-hw-openbox-picom.log   # copy_area_calls/s etc
yserver-openbox-picom-xrender-hw-telemetry log="info":
    cargo build --release --bin yserver
    rm -f yserver-openbox-picom.submit.tsv
    bash -c '\
        xdg_rd=$(mktemp -d -t yserver-run.XXXXXX); chmod 700 "$xdg_rd";\
        unset WAYLAND_DISPLAY WAYLAND_SOCKET;\
        export GDK_BACKEND=x11;\
        export XDG_SESSION_TYPE=x11;\
        YSERVER_LOOP_TELEMETRY=1 YSERVER_SUBMIT_TRACE=yserver-openbox-picom.submit.tsv \
            RUST_LOG="{{log}}" RUST_BACKTRACE=1 \
            target/release/yserver > yserver-hw-openbox-picom.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET GDK_BACKEND=x11 \
            XDG_SESSION_TYPE=x11 XDG_RUNTIME_DIR="$xdg_rd" \
            DISPLAY=:7 openbox > openbox-picom.log 2>&1 &\
        sleep 2;\
        DISPLAY=:7 picom --backend xrender --log-level warn --log-file picom.log > picom.out 2>&1 &\
        sleep 1;\
        DISPLAY=:7 wezterm ;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;\
        rm -rf "$xdg_rd" 2>/dev/null;'

# ============================== awesome ==============================

yserver-awesome-hw log="info":
    cargo build --release --bin yserver
    bash -c '\
        unset WAYLAND_DISPLAY WAYLAND_SOCKET;\
        export GDK_BACKEND=x11;\
        export XDG_SESSION_TYPE=x11;\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/release/yserver > yserver-hw-awesome.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET GDK_BACKEND=x11 XDG_SESSION_TYPE=x11 \
            DISPLAY=:7 awesome > awesome.log 2>&1;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;'

# Release-mode awesome with core-loop telemetry enabled (see `LoopTelemetry`
# in `crates/yserver-core/src/core_loop/run.rs`). Emits one info!-level
# "loop telemetry" line/sec to yserver-hw-awesome.log (iter/s, req/s,
# drain_max, top opcodes, host_input gap, ...) plus a per-vkQueueSubmit2 TSV
# to yserver-awesome.submit.tsv (schema + awk analyses documented on
# `yserver-mate-hw-telemetry`). Release build + frame pointers so `perf`
# folds cleanly on top.
#
# Use to chase the general responsiveness lag (e.g. `xclock` takes seconds
# to start under awesome): run this, then in the wezterm that opens launch
# `xclock` / flameshot and reproduce, then
# `grep "loop telemetry" yserver-hw-awesome.log` for the per-second rollups.
# RUST_LOG defaults to `info` so the rollup lines come through; pass
# `log=warn` for quieter output (but you lose the rollups — they're info!).
yserver-awesome-hw-telemetry log="info":
    cargo build --release --bin yserver
    rm -f yserver-awesome.submit.tsv
    bash -c '\
        xdg_rd=$(mktemp -d -t yserver-run.XXXXXX); chmod 700 "$xdg_rd";\
        unset WAYLAND_DISPLAY WAYLAND_SOCKET;\
        export GDK_BACKEND=x11;\
        export XDG_SESSION_TYPE=x11;\
        YSERVER_LOOP_TELEMETRY=1 YSERVER_SUBMIT_TRACE=yserver-awesome.submit.tsv \
            RUST_LOG="{{log}}" RUST_BACKTRACE=1 \
            target/release/yserver > yserver-hw-awesome.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET GDK_BACKEND=x11 \
            XDG_SESSION_TYPE=x11 XDG_RUNTIME_DIR="$xdg_rd" \
            DISPLAY=:7 awesome > awesome.log 2>&1 ;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;\
        rm -rf "$xdg_rd" 2>/dev/null;'

# Deterministic phased workload for damage-repaint before/after measurement.
#
# awesome because it never composites, so the clipped-repaint path is always
# under test with no gsettings archaeology; and a fixed clip in a fixed-geometry
# mpv holds content damage constant while `tools/damage-workload.sh` toggles
# structural damage by phase (settle, idle, drag, idle2, resize, restack, idle3).
#
# Then, for one run:
#     tools/damage-phases.py yserver-hw-awesome.log damage-phases.log
# or, comparing two branches:
#     tools/damage-phases.py before.log before-phases.log after.log after-phases.log
#
# This exists because whole-session A/B does not work: content load dominates
# `damage_fraction` and differs between any two hand-driven sessions, which is
# how a step-2 comparison on 2026-09-02 produced −63%, −17%, +14% and +34%
# across four paint-load bins and settled nothing. Same events at the same
# times, compared phase by phase, is what removes that.
#
# Arguments are POSITIONAL, as everywhere in this file — `just <recipe> a b`,
# not `k=v`:
#     just yserver-awesome-hw-workload ~/clip.mp4          # ~90s, measure
#     just yserver-awesome-hw-workload ~/clip.mp4 0.2      # ~20s, smoke the recipe
# `clip` must be a video file. `scale` multiplies every phase duration; keep it
# identical between the two branches being compared.
yserver-awesome-hw-workload clip scale="1" log="warn,yserver::startup=info,yserver::kms::render::telemetry=info":
    RUSTFLAGS="-C debug-assertions=yes" cargo build --release --bin yserver
    bash -c '\
        xdg_rd=$(mktemp -d -t yserver-run.XXXXXX); chmod 700 "$xdg_rd";\
        unset WAYLAND_DISPLAY WAYLAND_SOCKET;\
        export GDK_BACKEND=x11 XDG_SESSION_TYPE=x11;\
        YSERVER_LOOP_TELEMETRY=1 RUST_LOG="{{log}}" RUST_BACKTRACE=1 \
            target/release/yserver > yserver-hw-awesome.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET GDK_BACKEND=x11 \
            XDG_SESSION_TYPE=x11 XDG_RUNTIME_DIR="$xdg_rd" \
            DISPLAY=:7 awesome > awesome.log 2>&1 &\
        awesome_pid=$!;\
        sleep 2;\
        DISPLAY=:7 XDG_RUNTIME_DIR="$xdg_rd" \
            tools/damage-workload.sh "{{clip}}" damage-phases.log "{{scale}}" \
                > damage-workload.log 2>&1;\
        kill -TERM $awesome_pid 2>/dev/null;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;\
        rm -rf "$xdg_rd" 2>/dev/null;\
        echo "workload done: yserver-hw-awesome.log + damage-phases.log";\
        tail -5 damage-workload.log;\
        echo "read it with: tools/damage-phases.py yserver-hw-awesome.log damage-phases.log"'

# The same phased workload under MATE instead of awesome.
#
# awesome tiles, so windows never overlap and the `overdraw` counter reads ~1.0
# there whatever the scene contains — which makes it useless for sizing step 1's
# occlusion culling. MATE stacks windows, so this is the recipe that can answer
# "is anything actually hidden behind anything".
#
# REQUIRES marco compositing OFF, or the opaque-cover gate correctly declines
# every frame and the clipped path is never exercised:
#     gsettings set org.mate.Marco.general compositing-manager false
# Check it afterwards: `full_reason/s[no_opaque_cover=...]` dominating means it
# was on.
#
# Arguments are positional, as everywhere here:
#     just yserver-mate-hw-workload ~/clip.mp4          # ~90s, measure
#     just yserver-mate-hw-workload ~/clip.mp4 0.2      # ~20s, smoke
yserver-mate-hw-workload clip scale="1" log="warn,yserver::startup=info,yserver::kms::render::telemetry=info":
    RUSTFLAGS="-C debug-assertions=yes" cargo build --release --bin yserver
    bash -c '\
        unset WAYLAND_DISPLAY WAYLAND_SOCKET;\
        export GDK_BACKEND=x11 XDG_SESSION_TYPE=x11;\
        YSERVER_LOOP_TELEMETRY=1 RUST_LOG="{{log}}" RUST_BACKTRACE=1 \
            target/release/yserver > yserver-hw-mate.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:7 GDK_BACKEND=x11 \
            XDG_SESSION_TYPE=x11 \
            dbus-run-session mate-session --display :7 > mate.log 2>&1 &\
        session_pid=$!;\
        sleep 8;\
        DISPLAY=:7 tools/damage-workload.sh "{{clip}}" damage-phases.log "{{scale}}" \
            > damage-workload.log 2>&1;\
        kill -TERM $session_pid 2>/dev/null;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;\
        echo "workload done: yserver-hw-mate.log + damage-phases.log";\
        tail -5 damage-workload.log;\
        echo "read it with: tools/damage-phases.py yserver-hw-mate.log damage-phases.log"'

yserver-awesome-picom-hw log="warn":
    cargo build --release --bin yserver
    bash -c '\
        unset WAYLAND_DISPLAY WAYLAND_SOCKET;\
        export GDK_BACKEND=x11;\
        export XDG_SESSION_TYPE=x11;\
        stdbuf -oL -eL env RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/release/yserver > yserver-hw-awesome.log 2>&1 &\
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
        stdbuf -oL -eL env RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/debug/yserver > yserver-hw-awesome.log 2>&1 &\
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

# ============================== icewm ==============================

yserver-icewm-hw log="info":
    cargo build --release --bin yserver
    bash -c '\
        unset WAYLAND_DISPLAY WAYLAND_SOCKET;\
        export GDK_BACKEND=x11;\
        export XDG_SESSION_TYPE=x11;\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/release/yserver > yserver-hw-icewm.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        DISPLAY=:7 icewm > icewm.log 2>&1 ;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;'

yserver-icewm-hw-trace log="yserver::kms::render::pointer=trace":
    cargo build --bin yserver
    bash -c '\
        unset WAYLAND_DISPLAY WAYLAND_SOCKET;\
        export GDK_BACKEND=x11;\
        export XDG_SESSION_TYPE=x11;\
        stdbuf -oL -eL env RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/debug/yserver > yserver-hw-icewm.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        x11trace -k -d :7 -D :8 -n -o icewm.xtrace &\
        DISPLAY=:8 icewm > icewm.log 2>&1 ;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;'

# ============================== i3 ==============================

yserver-i3-hw log="info":
    cargo build --release --bin yserver
    bash -c '\
        unset WAYLAND_DISPLAY WAYLAND_SOCKET;\
        export GDK_BACKEND=x11;\
        export XDG_SESSION_TYPE=x11;\
        stdbuf -oL -eL env RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/release/yserver > yserver-hw-i3.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        DISPLAY=:7 i3 > i3.log 2>&1 &\
        DISPLAY=:7 feh --bg-fill /home/jos/Pictures/catbackground.jpg ;\
        sleep 1;\
        DISPLAY=:7 fastcompmgr -o 0.4 -r 12 -c -C ;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;'

yserver-i3-hw-trace log="debug":
    cargo build --bin yserver
    rm -f i3.xtrace
    bash -c '\
        unset WAYLAND_DISPLAY WAYLAND_SOCKET;\
        export GDK_BACKEND=x11;\
        export XDG_SESSION_TYPE=x11;\
        stdbuf -oL -eL env RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/debug/yserver > yserver-hw-i3.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        x11trace -k -d :7 -D :8 -n -o i3.xtrace &\
        sleep 1;\
        DISPLAY=:8 i3 > i3.log 2>&1 &\
        DISPLAY=:7 feh --bg-fill /home/jos/Pictures/catbackground.jpg > feh.log 2>&1;\
        DISPLAY=:8 fastcompmgr -o 0.4 -r 12 -c -C -i 0.5 ;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;'

# ============================== dwm ==============================

yserver-dwm-hw log="info":
    cargo build --release --bin yserver
    bash -c '\
        unset WAYLAND_DISPLAY WAYLAND_SOCKET;\
        export GDK_BACKEND=x11;\
        export XDG_SESSION_TYPE=x11;\
        stdbuf -oL -eL env RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/release/yserver > yserver-hw-dwm.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        DISPLAY=:7 dwm > dwm.log 2>&1 &\
        DISPLAY=:7 feh --bg-fill /home/jos/Pictures/catbackground.jpg ;\
        sleep 1;\
        DISPLAY=:7 fastcompmgr -o 0.4 -r 12 -c -C ;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;'

yserver-dwm-hw-trace log="debug":
    cargo build --bin yserver
    rm -f dwm.xtrace
    bash -c '\
        unset WAYLAND_DISPLAY WAYLAND_SOCKET;\
        export GDK_BACKEND=x11;\
        export XDG_SESSION_TYPE=x11;\
        stdbuf -oL -eL env RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/debug/yserver > yserver-hw-dwm.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        x11trace -k -d :7 -D :8 -n -o dwm.xtrace &\
        sleep 1;\
        DISPLAY=:8 dwm > dwm.log 2>&1 &\
        DISPLAY=:7 feh --bg-fill /home/jos/Pictures/catbackground.jpg > feh.log 2>&1;\
        DISPLAY=:8 fastcompmgr -o 0.4 -r 12 -c -C -i 0.5 ;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;'

# ============================== FVWM3 ==============================

yserver-fvwm3-xterm-hw log="info":
    cargo build --release --bin yserver
    bash -c '\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/release/yserver > yserver-hw-fvwm3.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        DISPLAY=:7 fvwm3 > fvwm3-hw.log 2>&1 &\
        sleep 8;\
        DISPLAY=:7 wezterm;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;'

yserver-fvwm3-hw-trace log="debug":
    cargo build --bin yserver
    rm -f fvwm3.xtrace
    bash -c '\
        unset WAYLAND_DISPLAY WAYLAND_SOCKET;\
        export GDK_BACKEND=x11;\
        export XDG_SESSION_TYPE=x11;\
        stdbuf -oL -eL env RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/debug/yserver > yserver-hw-fvwm3.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        x11trace -k -d :7 -D :8 -n -o fvwm3.xtrace &\
        sleep 1;\
        DISPLAY=:8 fvwm3 > fvwm3.log 2>&1;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;'

# Release-mode NON-COMPOSITED fvwm3 with core-loop + scene telemetry, to
# diagnose the "choppy cursor while a fullscreen video plays" symptom.
# The present/compose/flip path is already known SOUND (cinnamon runs dual
# fullscreen video 100-119Hz zero drops) — this measures WHERE the loop
# time goes under fvwm, the one WM only ever run non-composited + never
# telemetered (docs/superpowers/findings/2026-07-08-perf-thread-wm-redirect-model.md).
# fvwm3 runs no compositor, so the fullscreen window stays UNREDIRECTED —
# the regime where a participating top-level should actually exist.
#
# Run it, then in the wezterm that opens start a FULLSCREEN video and wiggle
# the mouse to reproduce the choppy cursor:
#   - real symptom:  DISPLAY=:7 chromium --start-fullscreen <youtube-url>
#   - deterministic: pass video=/path/to/file.mp4 to auto-launch `mpv --fs`
# Close the wezterm to end, then read the per-second rollups:
#   grep "loop telemetry" yserver-hw-fvwm3.log   # host_input/s + gap_max=..ms
#   grep "render_telemetry"   yserver-hw-fvwm3.log   # cursor_move_ebusy/s, full_redraw_fallback/s,
#                                                # frame_present_count/s, missed_pageflips/s, damage_fraction
# Reading the result — the three candidate mechanisms for the choppy cursor:
#   gap_max spikes (tens of ms) while moving   -> INPUT path starved (#1)
#   cursor_move_ebusy/s high                    -> HW cursor deferred to pageflip (#3)
#   cursor_move_ebusy/s ~0 while moving         -> cursor is SW, tied to compose cadence (#2)
#   full_redraw_fallback/s == frame_present_count/s (damage_fraction~1.0) confirms Repaint::Full/frame
yserver-fvwm3-hw-telemetry log="info":
    cargo build --release --bin yserver
    rm -f yserver-fvwm3.submit.tsv
    bash -c '\
        xdg_rd=$(mktemp -d -t yserver-run.XXXXXX); chmod 700 "$xdg_rd";\
        unset WAYLAND_DISPLAY WAYLAND_SOCKET;\
        export GDK_BACKEND=x11;\
        export XDG_SESSION_TYPE=x11;\
        YSERVER_LOOP_TELEMETRY=1 YSERVER_SUBMIT_TRACE=yserver-fvwm3.submit.tsv \
            RUST_LOG="{{log}}" RUST_BACKTRACE=1 \
            target/release/yserver > yserver-hw-fvwm3.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET GDK_BACKEND=x11 \
            XDG_SESSION_TYPE=x11 XDG_RUNTIME_DIR="$xdg_rd" \
            DISPLAY=:7 fvwm3 > fvwm3-hw.log 2>&1 &\
        sleep 1;\
        DISPLAY=:7 wezterm ;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;\
        rm -rf "$xdg_rd" 2>/dev/null;'

# ============================== WINDOW MAKER ==============================

yserver-wmaker-xterm-hw log="info":
    cargo build --release --bin yserver
    bash -c '\
        xdg_rd=$(mktemp -d -t yserver-run.XXXXXX); chmod 700 "$xdg_rd";\
        YSERVER_SCENE_WALK_ALL=1 \
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/release/yserver > yserver-hw-wmaker.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:7 GDK_BACKEND=x11 \
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
        YSERVER_SCENE_WALK_ALL=1 \
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/debug/yserver > yserver-hw-wmaker.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        x11trace -d :7 -D :8 -n -o wmaker.xtrace &\
        xtrace_pid=$!;\
        sleep 1;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:8 GDK_BACKEND=x11 \
            XDG_SESSION_TYPE=x11 XDG_RUNTIME_DIR="$xdg_rd" \
            wmaker > wmaker-hw.log 2>&1 &\
        sleep 2;\
        DISPLAY=:8 wezterm;\
        kill -TERM $xtrace_pid $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;\
        rm -rf "$xdg_rd" 2>/dev/null;'

# ============================== RENDERCHECK ==============================

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

# Run an xts5 scenario against yserver (KMS) inside virtme-ng.
# Boots vng once with yserver in the background (headless QEMU,
# virtio-gpu KMS), polls for the X socket on :7, then runs
# tools/xts-run.sh. Result tree lands in xts/results/ on the host
# because vng mounts the host rootfs --rw.
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

xts-yserver-hw scenario="all" timeout="20000":
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
