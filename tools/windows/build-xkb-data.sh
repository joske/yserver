#!/bin/sh
# Build the XKB data for the Windows zip from a pinned xkeyboard-config
# release: OUT/xkb (symlinks resolved) and OUT/COPYING.xkeyboard-config.
# Needs curl, sha256sum, meson, ninja, python3 >= 3.11 and perl.
set -eu

VERSION=2.48
SHA256=b77041324f0109f77161ee43743fe04baa485866af8460d31e476ad3f7648fd5
URL=https://www.x.org/releases/individual/data/xkeyboard-config/xkeyboard-config-$VERSION.tar.xz

[ $# -eq 1 ] || { echo "usage: $0 OUTDIR" >&2; exit 2; }
out=$1
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

curl -fsSL -o "$work/src.tar.xz" "$URL"
echo "$SHA256  $work/src.tar.xz" | sha256sum -c -
tar -xf "$work/src.tar.xz" -C "$work"
src=$work/xkeyboard-config-$VERSION
# The options Arch packages it with; nls only translates the .xml registry.
meson setup "$work/build" "$src" --prefix=/usr -Dnls=false \
    -Dcompat-rules=true -Dxorg-rules-symlinks=true >/dev/null
meson compile -C "$work/build" >/dev/null
DESTDIR="$work/dest" meson install -C "$work/build" --no-rebuild >/dev/null

rm -rf "$out/xkb"
mkdir -p "$out"
# -L: a zip has no symlinks (rules/xorg* point at rules/base*).
cp -RL "$work/dest/usr/share/xkeyboard-config-2" "$out/xkb"
cp "$src/COPYING" "$out/COPYING.xkeyboard-config"
for f in rules/evdev rules/evdev.lst rules/base symbols/us keycodes/evdev; do
    [ -f "$out/xkb/$f" ] || { echo "missing xkb/$f" >&2; exit 1; }
done
echo "xkeyboard-config $VERSION -> $out/xkb"
