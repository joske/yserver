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
#   tools/vng-shot.sh --gpu none                  # 2D virtio-gpu + lavapipe, no host GPU
#   CPUS=8 tools/vng-shot.sh                      # wider guest (default 4)
#   LVP_ICD=/path/lvp_icd.json tools/vng-shot.sh --gpu none   # other lavapipe ICD
#   VNG_OUT=target/elsewhere tools/vng-shot.sh    # artifacts in $VNG_OUT/<name>/
#   tools/vng-shot.sh --root-hook hook.sh         # sourced as guest root once :7 listens
#   tools/vng-shot.sh --server-arg --headless     # extra server argument (Xorg too), repeatable
#
# The host root is read-only in the guest behind discarded overlays; only the
# artifact directory is shared writable, at the same path inside and out, so
# the handshake is plain files: the guest touches READY when the scenario has
# settled, the host presses Ctrl+Alt+Enter through the emulated PS/2 keyboard
# (a real evdev device to the guest, so the dump travels yserver's actual
# input path), then touches GO to release the guest. The server runs as guest
# root; the scenario runs as a throwaway user `ysuite` with a tmpfs home.
# A guest that did not run under KVM fails the run. Before stopping the server
# the guest records SERVER-ALIVE, or SERVER-DEAD with its exit status.
#
# QEMU's own `screendump` does NOT work here: `-display egl-headless` has
# no surface, so it answers "Error: no surface". yserver's dump is the
# better artifact anyway — it is the same PPM the Ctrl+Alt+Enter capture
# produces on real hardware, so captures are comparable across both.
set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
kernel=${KERNEL:-/boot/vmlinuz-linux-zen}
cpus=${CPUS:-4}
gpu=${GPU:-venus}
name=shot
scenario=
root_hook=
log=info
settle=5
hold=0
timeout_s=300
binary=
outputs=1
server=yserver
dump=scanout
declare -a extra_env=()
declare -a server_args=()

usage() {
    sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed 's/^# \?//;$d'
    exit "${1:-0}"
}

while [ $# -gt 0 ]; do
    case $1 in
        --name) name=$2; shift 2;;
        --scenario) scenario=$2; shift 2;;
        --root-hook) root_hook=$2; shift 2;;
        --log) log=$2; shift 2;;
        --settle) settle=$2; shift 2;;
        --hold) hold=$2; shift 2;;
        --timeout) timeout_s=$2; shift 2;;
        --binary) binary=$2; shift 2;;
        --dump) dump=$2; shift 2;;
        --server) server=$2; shift 2;;
        --outputs) outputs=$2; shift 2;;
        --env) extra_env+=("$2"); shift 2;;
        --server-arg) server_args+=("$2"); shift 2;;
        --gpu) gpu=$2; shift 2;;
        -h|--help) usage 0;;
        *) echo "vng-shot: unknown argument $1" >&2; usage 1;;
    esac
done

command -v vng >/dev/null || { echo "vng-shot: virtme-ng (vng) not on PATH" >&2; exit 1; }
[ -e "$kernel" ] || { echo "vng-shot: kernel $kernel not found (set KERNEL=)" >&2; exit 1; }
case $gpu in
    # Without vulkan-virtio the guest picks RADV and fails amdgpu init.
    venus) icd=/usr/share/vulkan/icd.d/virtio_icd.json; icd_pkg=vulkan-virtio;;
    none)
        icd=/usr/share/vulkan/icd.d/lvp_icd.json
        [ -e "$icd" ] || icd=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json
        icd=${LVP_ICD:-$icd}; icd_pkg="vulkan-swrast or mesa-vulkan-drivers, or set LVP_ICD";;
    *) echo "vng-shot: --gpu must be venus or none" >&2; exit 1;;
esac
[ -e "$icd" ] || { echo "vng-shot: $icd missing (pacman -S $icd_pkg)" >&2; exit 1; }
[ "$gpu" = venus ] || [ -w /dev/udmabuf ] || {
    echo "vng-shot: --gpu none needs a writable /dev/udmabuf" >&2; exit 1; }
if [ -n "$scenario" ]; then
    [ -r "$scenario" ] || { echo "vng-shot: scenario $scenario not readable" >&2; exit 1; }
    scenario=$(cd -- "$(dirname -- "$scenario")" && pwd)/$(basename -- "$scenario")
fi
if [ -n "$root_hook" ]; then
    [ -r "$root_hook" ] || { echo "vng-shot: root hook $root_hook not readable" >&2; exit 1; }
    root_hook=$(cd -- "$(dirname -- "$root_hook")" && pwd)/$(basename -- "$root_hook")
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
    xorg_bin=/usr/lib/Xorg                                  # Arch
    [ -x "$xorg_bin" ] || xorg_bin=/usr/lib/xorg/Xorg       # Debian/Ubuntu
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

out_root=${VNG_OUT:-$repo/target/vng}
mkdir -p "$out_root"
out=$(cd -- "$out_root" && pwd)/$name
rm -rf "$out"
mkdir -p "$out"
# sun_path holds 107 bytes: the socket lives in a short directory, linked
# from the artifacts as monitor.sock.
mon_dir=$(mktemp -d "${TMPDIR:-/tmp}/vng-mon.XXXXXX")
mon=$mon_dir/monitor.sock
ln -s "$mon" "$out/monitor.sock"

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

# The guest runs exactly one command, so materialise the guest side as
# scripts next to their own artifacts. `env` cannot carry the handshake, and a
# here-doc through `vng --` would be re-split by the guest shell. guest.sh runs
# as guest root (DRM master, input); client.sh runs the scenario as ysuite.
guest=$out/guest.sh
client=$out/client.sh
{
    echo '#!/bin/sh'
    echo 'set -eu'
    echo "cd '$out'"
    echo "export VK_DRIVER_FILES=$icd"
    echo "export LANG='${LANG:-C.UTF-8}'"
    echo "export HOME=/run/ysuite USER=ysuite LOGNAME=ysuite SHELL=/bin/sh"
    echo 'export XDG_CONFIG_HOME=$HOME/.config XDG_DATA_HOME=$HOME/.local/share'
    echo 'export XDG_CACHE_HOME=$HOME/.cache XDG_STATE_HOME=$HOME/.local/state'
    echo 'export XDG_RUNTIME_DIR=/run/ysuite-runtime DISPLAY=:7'
    echo "export YSERVER_REPO='$repo'"
    for kv in ${extra_env+"${extra_env[@]}"}; do
        echo "export ${kv%%=*}='${kv#*=}'"
    done
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
} > "$client"
{
    echo '#!/bin/sh'
    echo 'set -eu'
    echo "cd '$out'"
    # virtme-ng asks for accel=kvm:tcg, and the TCG fallback is silent.
    echo 'dmesg | grep -q "Hypervisor detected: KVM" || {'
    echo '    echo "vng-shot: guest is not running under KVM" > NOT-KVM; exit 1; }'
    # ysuite takes the uid that owns the share: 9p checks modes guest-side
    # against the host owner, so any other uid could not chmod its own files
    # (cc's output would not be executable). /etc and /run are guest-only
    # (overlay, tmpfs), so the host user's entry is only hidden in the guest.
    echo 'uid=$(stat -c %u .) gid=$(stat -c %g .)'
    echo '[ "$uid" != 0 ] || { uid=4242 gid=4242; chown $uid:$gid .; }'
    echo 'sed -i "/^[^:]*:[^:]*:$uid:/d" /etc/passwd; sed -i "/^[^:]*:[^:]*:$gid:/d" /etc/group'
    echo 'echo "ysuite:x:$uid:$gid:vng suite:/run/ysuite:/bin/sh" >> /etc/passwd'
    echo 'echo "ysuite:x:$gid:" >> /etc/group'
    echo 'for d in /run/ysuite /run/ysuite/.config /run/ysuite/.cache /run/ysuite/.local \'
    echo '    /run/ysuite/.local/share /run/ysuite/.local/state /run/ysuite-runtime; do'
    echo '    install -d -m 0700 -o $uid -g $gid "$d"'
    echo 'done'
    echo "export VK_DRIVER_FILES=$icd"
    # yserver refuses KMS scanout off a CPU Vulkan device unless told.
    if [ "$gpu" = none ]; then echo 'export YSERVER_ALLOW_SOFTWARE_VULKAN=1'; fi
    echo "export RUST_LOG='$log'"
    echo 'export RUST_BACKTRACE=1'
    # XDG_RUNTIME_DIR is Xorg's documented fallback; /var/lib/xkb (Xorg's
    # XKM_OUTPUT_DIR) is writable through vng's /var overlay.
    echo 'install -d -m 0700 /run/user/0'
    echo 'export XDG_RUNTIME_DIR=/run/user/0'
    for kv in ${extra_env+"${extra_env[@]}"}; do
        echo "export ${kv%%=*}='${kv#*=}'"
    done
    # The guest /tmp overlays the host's, so a host :7 would show through.
    echo 'rm -f /tmp/.X11-unix/X7 /tmp/.X7-lock'
    # Without vmport the PS/2 mouse probes as ImExPS/2 ~1.5 s into boot.
    echo 'i=0'
    echo 'while ! grep -q "Mouse" /proc/bus/input/devices && [ $i -lt 50 ]; do i=$((i+1)); sleep 0.1; done'
    if [ "$server" = xorg ]; then
        # /usr/bin/Xorg is a shim onto the setuid Xorg.wrap, which drops root
        # when the caller is not sitting on a console — and then the real
        # server cannot open the VT ("Cannot open virtual console 1
        # (Permission denied)"). We are already root in the guest, so run the
        # real binary and skip the wrapper.
        echo "${xorg_bin} :7 vt1 -keeptty -novtswitch \\"
        printf "    -config '%s' -logfile '%s'" "$out/xorg.conf" "$out/xorg-server.log"
        for a in ${server_args+"${server_args[@]}"}; do printf " '%s'" "$a"; done
        echo " > xorg-stdio.log 2>&1 &"
    else
        printf "'%s' 7" "$binary"
        for a in ${server_args+"${server_args[@]}"}; do printf " '%s'" "$a"; done
        echo " > yserver.log 2>&1 &"
    fi
    echo 'server=$!'
    echo 'i=0'
    # Xorg on a software stack needs far longer than yserver to come up.
    echo "while [ ! -S /tmp/.X11-unix/X7 ] && [ \$i -lt $listen_tries ]; do i=\$((i+1)); sleep 0.2; done"
    echo '[ -S /tmp/.X11-unix/X7 ] || { echo "server never listened on :7" >&2; touch FAILED; }'
    if [ -n "$root_hook" ]; then echo ". '$root_hook'"; fi
    echo 'set -- sh ./client.sh'
    echo 'command -v dbus-run-session > /dev/null && set -- dbus-run-session -- "$@"'
    echo "setpriv --reuid=\$uid --regid=\$gid --clear-groups env -i PATH=\"\$PATH\" \"\$@\" \\"
    echo '    || echo "vng-shot: client.sh exited $?" >&2'
    echo 'if kill -0 $server 2>/dev/null; then'
    echo '    touch SERVER-ALIVE'
    echo '    kill -TERM $server 2>/dev/null || true'
    # Xorg can hang in its shutdown; the results are already written.
    echo '    ( sleep 15; kill -KILL $server 2>/dev/null ) & reaper=$!'
    echo '    wait $server 2>/dev/null || true'
    echo '    kill $reaper 2>/dev/null || true'
    echo 'else'
    echo '    st=0; wait $server 2>/dev/null || st=$?'
    echo '    echo "exit status $st" > SERVER-DEAD'
    echo 'fi'
    echo 'dmesg > guest-dmesg.log'
    echo 'touch DONE'
} > "$guest"
chmod +x "$guest"

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

if [ "$gpu" = none ]; then
    # 2D virtio-gpu, nothing from the host GPU. Without blob=on the guest
    # cannot PRIME-import lavapipe's dma-bufs (ENODEV); QEMU backs blobs
    # with the host's /dev/udmabuf.
    qemu_opts="-device virtio-gpu-pci,blob=on,max_outputs=$outputs"
else
    qemu_opts="-display egl-headless"
    qemu_opts="$qemu_opts -device virtio-gpu-gl-pci,venus=on,blob=on,hostmem=4G,max_hostmem=4G,max_outputs=$outputs"
fi
# vmport=off: the VMware VMMouse ignores relative mouse_move; plain PS/2 doesn't.
qemu_opts="$qemu_opts -machine vmport=off -monitor unix:$mon,server=on,wait=off"

echo "vng-shot: booting guest ($name) with ${binary:-Xorg}; artifacts in $out"
timeout "$timeout_s" vng -r "$kernel" --cpus "$cpus" --disable-microvm --rwdir "$out" \
    ${appends+"${appends[@]}"} \
    --qemu-opts="$qemu_opts" -- "$guest" > "$out/vng.log" 2>&1 < /dev/null &
vm=$!

release() { touch "$out/GO" 2>/dev/null || true; }
drop_monitor() { rm -rf "$mon_dir"; rm -f "$out/monitor.sock"; }
trap 'release; drop_monitor' EXIT

waited=0
while [ ! -e "$out/READY" ]; do
    kill -0 "$vm" 2>/dev/null || { echo "vng-shot: guest died before READY" >&2; break; }
    sleep 0.5
    waited=$((waited + 1))
    [ "$waited" -lt $((timeout_s * 2)) ] || { echo "vng-shot: timed out waiting for READY" >&2; break; }
done

if [ -e "$out/NOT-KVM" ]; then
    wait "$vm" 2>/dev/null || true
    cat "$out/NOT-KVM" >&2
    exit 1
fi

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
drop_monitor
trap - EXIT

echo "vng-shot: artifacts:"
ls -1 "$out" | sed 's/^/  /'
if [ "$dump" != none ]; then
    compgen -G "$out/yserver-scanout-*.ppm" >/dev/null || {
        echo "vng-shot: NO scanout dump captured — see $out/yserver.log" >&2
        exit 1; }
fi
