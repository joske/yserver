#!/usr/bin/env bash
# Boot yserver inside a virtme-ng guest on virtio-gpu Venus, run a scenario
# against it, and capture yserver's own scanout dump — no physical display
# and no human relaying screenshots. Artifacts land in target/vng/<name>/.
#
#   tools/vng-shot.sh                             # xterm, one dump
#   tools/vng-shot.sh --name borders \
#       --scenario tools/vng-scenarios/awesome-wezterm-tile.sh
#   tools/vng-shot.sh --name master --binary ../wt/target/debug/yserver
#   tools/vng-shot.sh --dump drawables       # per-drawable storage too
#   tools/vng-shot.sh --server xorg --dump none    # the Xorg baseline
#   tools/vng-shot.sh --outputs 2                 # dual-head guest
#   CPUS=8 tools/vng-shot.sh                      # wider guest (default 4)
#
# The guest boots the host's rootfs read-write (vng --rw), so the artifact
# directory is the same path inside and out and the handshake is plain
# files: the guest touches READY when the scenario has settled, the host
# presses Ctrl+Alt+Enter through the emulated PS/2 keyboard (a real evdev
# device to the guest, so the dump travels yserver's actual input path),
# then touches GO to release the guest.
#
# QEMU's own `screendump` does NOT work here: `-display egl-headless` has
# no surface, so it answers "Error: no surface". yserver's dump is the
# better artifact anyway — it is the same PPM the Ctrl+Alt+Enter capture
# produces on real hardware, so captures are comparable across both.
set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
kernel=${KERNEL:-/boot/vmlinuz-linux-zen}
cpus=${CPUS:-4}
name=shot
scenario=
log=info
settle=5
hold=0
timeout_s=300
binary=
outputs=1
server=yserver
dump=scanout
declare -a extra_env=()

usage() {
    sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed 's/^# \?//;$d'
    exit "${1:-0}"
}

while [ $# -gt 0 ]; do
    case $1 in
        --name) name=$2; shift 2;;
        --scenario) scenario=$2; shift 2;;
        --log) log=$2; shift 2;;
        --settle) settle=$2; shift 2;;
        --hold) hold=$2; shift 2;;
        --timeout) timeout_s=$2; shift 2;;
        --binary) binary=$2; shift 2;;
        --dump) dump=$2; shift 2;;
        --server) server=$2; shift 2;;
        --outputs) outputs=$2; shift 2;;
        --env) extra_env+=("$2"); shift 2;;
        -h|--help) usage 0;;
        *) echo "vng-shot: unknown argument $1" >&2; usage 1;;
    esac
done

command -v vng >/dev/null || { echo "vng-shot: virtme-ng (vng) not on PATH" >&2; exit 1; }
[ -e "$kernel" ] || { echo "vng-shot: kernel $kernel not found (set KERNEL=)" >&2; exit 1; }
# Without vulkan-virtio the guest picks RADV and fails amdgpu init.
[ -e /usr/share/vulkan/icd.d/virtio_icd.json ] || {
    echo "vng-shot: /usr/share/vulkan/icd.d/virtio_icd.json missing (pacman -S vulkan-virtio)" >&2
    exit 1; }
if [ -n "$scenario" ]; then
    [ -r "$scenario" ] || { echo "vng-shot: scenario $scenario not readable" >&2; exit 1; }
    scenario=$(cd -- "$(dirname -- "$scenario")" && pwd)/$(basename -- "$scenario")
fi

case $server in
    yserver|xorg) ;;
    *) echo "vng-shot: --server must be yserver or xorg" >&2; exit 1;;
esac

cd "$repo"
if [ "$server" = xorg ]; then
    # The Xorg baseline. `vtN` on the command line makes Xorg skip the
    # /dev/tty0 probe, and -keeptty -novtswitch keeps it off the guest's
    # console; without those it dies in parse_vt_settings. No dump hotkey
    # exists, so the scenario's `import -window root` is the capture (on a
    # non-composited X server the root window IS the framebuffer).
    dump=none
    listen_tries=600
    xorg_bin=/usr/lib/Xorg
    [ -x "$xorg_bin" ] || xorg_bin=$(command -v Xorg) || {
        echo "vng-shot: no Xorg on PATH" >&2; exit 1; }
elif [ -n "$binary" ]; then
    # An A/B against another commit: point at a binary built in a separate
    # worktree with its OWN CARGO_TARGET_DIR. Sharing target/ across
    # worktrees leaves stale rlibs and produces bogus link errors.
    binary=$(cd -- "$(dirname -- "$binary")" && pwd)/$(basename -- "$binary")
    [ -x "$binary" ] || { echo "vng-shot: $binary is not executable" >&2; exit 1; }
else
    cargo build --bin yserver
    binary=$repo/target/debug/yserver
fi

listen_tries=${listen_tries:-150}

out=$repo/target/vng/$name
rm -rf "$out"
mkdir -p "$out"
mon=$out/monitor.sock

if [ "$server" = xorg ]; then
    # AccelMethod none: the shadow-fb path. Nothing here depends on glamor,
    # and skipping it removes the slowest, least reliable part of bringing
    # Xorg up on a virtualized GPU. Absolute paths for -config/-logfile are
    # accepted because we run the real binary as real root, so Xorg does not
    # see elevated privileges (which is what rejects them under Xorg.wrap).
    cat > "$out/xorg.conf" <<'CONF'
Section "Device"
    Identifier "virtio"
    Driver     "modesetting"
    Option     "AccelMethod" "none"
EndSection
CONF
fi

# The guest runs exactly one command, so materialise the guest side as a
# script next to its own artifacts. `env` cannot carry the handshake, and a
# here-doc through `vng --` would be re-split by the guest shell.
guest=$out/guest.sh
{
    echo '#!/bin/sh'
    echo 'set -eu'
    echo "cd '$out'"
    echo 'export VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.json'
    echo "export RUST_LOG='$log'"
    echo 'export RUST_BACKTRACE=1'
    # Xorg compiles its keymap into XKM_OUTPUT_DIR (/var/lib/xkb); without a
    # writable one it dies with "Failed to activate virtual core keyboard: 2".
    # An --overlay-rwdir gives the guest a writable /var/lib (see below).
    # XDG_RUNTIME_DIR is Xorg's documented fallback and wezterm wants one too.
    echo "export XDG_RUNTIME_DIR='$out'"
    for kv in ${extra_env+"${extra_env[@]}"}; do
        echo "export ${kv%%=*}='${kv#*=}'"
    done
    # The guest shares the host's /tmp/.X11-unix. A guest killed mid-run
    # leaves X7 and its lock behind, so the listen wait below would pass on
    # a dead socket, and the next server trips over it. The guest runs as
    # root, so it can clear what the host user can't.
    echo 'rm -f /tmp/.X11-unix/X7 /tmp/.X7-lock'
    if [ "$server" = xorg ]; then
        # /usr/bin/Xorg is a shim onto the setuid Xorg.wrap, which drops root
        # when the caller is not sitting on a console — and then the real
        # server cannot open the VT ("Cannot open virtual console 1
        # (Permission denied)"). We are already root in the guest, so run the
        # real binary and skip the wrapper.
        echo "${xorg_bin} :7 vt1 -keeptty -novtswitch \\"
        echo "    -config '$out/xorg.conf' -logfile '$out/xorg-server.log' \\"
        echo "    > xorg-stdio.log 2>&1 &"
    else
        echo "'$binary' 7 > yserver.log 2>&1 &"
    fi
    echo 'server=$!'
    echo 'i=0'
    # Xorg on a software stack needs far longer than yserver to come up.
    echo "while [ ! -S /tmp/.X11-unix/X7 ] && [ \$i -lt $listen_tries ]; do i=\$((i+1)); sleep 0.2; done"
    echo '[ -S /tmp/.X11-unix/X7 ] || { echo "server never listened on :7" >&2; touch FAILED; }'
    echo 'export DISPLAY=:7'
    if [ -n "$scenario" ]; then
        echo ". '$scenario'"
    else
        echo 'xterm -geometry 60x20+80+60 > xterm.log 2>&1 &'
    fi
    echo "sleep $settle"
    echo 'touch READY'
    # Host captures now; GO releases us. Bounded so a dead host cannot
    # strand the VM holding DRM master.
    echo 'i=0'
    echo 'while [ ! -e GO ] && [ $i -lt 600 ]; do i=$((i+1)); sleep 0.5; done'
    echo 'kill -TERM $server 2>/dev/null || true'
    echo 'wait $server 2>/dev/null || true'
    echo 'rm -f /tmp/.X11-unix/X7 /tmp/.X7-lock'
    echo 'touch DONE'
} > "$guest"
chmod +x "$guest"

# Xorg compiles its keymap to XKM_OUTPUT_DIR and reads it straight back, and
# that path is /var/lib/xkb — the ONE directory outside the repo it needs to
# write. It is read-only here, and the guest cannot mount anything itself (no
# privileges in vng's namespace), so hand it an overlay: writable in the guest,
# host filesystem untouched, nothing persisted between runs.
overlay=()
[ "$server" = xorg ] && overlay=(--overlay-rwdir /var/lib/xkb)

# A second head needs BOTH halves: `max_outputs` on the device, and
# `video=Virtual-2:...e` on the kernel cmdline. The `e` suffix forces the
# connector ENABLED — virtio-gpu reports it disconnected under a headless
# display backend, so without the force only Virtual-1 comes up. The forced
# connector picks its own mode (1024x768), which is fine: what matters is that
# the layout is genuinely two outputs over one framebuffer.
declare -a appends=()
if [ "$outputs" -gt 1 ]; then
    for n in $(seq 2 "$outputs"); do
        appends+=(-a "video=Virtual-$n:1280x800e")
    done
fi

qemu_opts="-display egl-headless"
qemu_opts="$qemu_opts -device virtio-gpu-gl-pci,venus=on,blob=on,hostmem=4G,max_hostmem=4G,max_outputs=$outputs"
qemu_opts="$qemu_opts -monitor unix:$mon,server=on,wait=off"

echo "vng-shot: booting guest ($name) with ${binary:-Xorg}; artifacts in $out"
timeout "$timeout_s" vng -r "$kernel" --cpus "$cpus" --disable-microvm --rw \
    ${overlay+"${overlay[@]}"} ${appends+"${appends[@]}"} \
    --qemu-opts="$qemu_opts" -- "$guest" > "$out/vng.log" 2>&1 < /dev/null &
vm=$!

release() { touch "$out/GO" 2>/dev/null || true; }
trap release EXIT

waited=0
while [ ! -e "$out/READY" ]; do
    kill -0 "$vm" 2>/dev/null || { echo "vng-shot: guest died before READY" >&2; break; }
    sleep 0.5
    waited=$((waited + 1))
    [ "$waited" -lt $((timeout_s * 2)) ] || { echo "vng-shot: timed out waiting for READY" >&2; break; }
done

if [ -e "$out/READY" ]; then
    case $dump in
        none) key=;;
        # Ctrl+Alt+F12 dumps every drawable's storage AND the scanout, from
        # one instant — the only way to attribute an on-screen region to the
        # window whose storage holds it.
        drawables) key=ctrl-alt-f12;;
        scanout) key=ctrl-alt-ret;;
        *) echo "vng-shot: --dump must be scanout, drawables or none" >&2; exit 1;;
    esac
    if [ -n "$key" ]; then
        echo "vng-shot: scenario settled; pressing ${key} for a $dump dump"
        "$repo/tools/qemu-monitor.py" "$mon" "sendkey $key"
    fi
    # The dump reads the scanout back over PCI and can take a second.
    for _ in $(seq 1 40); do
        compgen -G "$out/yserver-scanout-*.ppm" >/dev/null && break
        sleep 0.25
    done
    [ "$hold" -gt 0 ] && { echo "vng-shot: holding guest for ${hold}s (monitor $mon)"; sleep "$hold"; }
fi

release
wait "$vm" 2>/dev/null || true
trap - EXIT

echo "vng-shot: artifacts:"
ls -1 "$out" | sed 's/^/  /'
if [ "$dump" != none ]; then
    compgen -G "$out/yserver-scanout-*.ppm" >/dev/null || {
        echo "vng-shot: NO scanout dump captured — see $out/yserver.log" >&2
        exit 1; }
fi
