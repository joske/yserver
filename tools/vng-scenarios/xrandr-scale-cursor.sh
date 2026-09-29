# Sourced by tools/vng-shot.sh INSIDE the guest (DISPLAY=:7, cwd = artifacts).
# Issue #185: root GetImage under `xrandr --scale` (a SW cursor on yserver)
# must not contain the pointer. Captures the root with the pointer at two
# spots over a noise-tiled root; on both servers the two must be identical.
#   tools/vng-shot.sh --name cursor-ys --scenario tools/vng-scenarios/xrandr-scale-cursor.sh
#   tools/vng-shot.sh --server xorg --name cursor-xorg --scenario tools/vng-scenarios/xrandr-scale-cursor.sh
set -u
xprop -root -spy > /dev/null 2>&1 &
hold=$!
sleep 1
out=$(xrandr | awk '/ connected/{print $1; exit}')
xrandr --output "$out" --scale 2x2 --filter nearest > xrandr.log 2>&1
sleep 1
python3 - > tile.log 2>&1 <<'PY'
import random
from Xlib import X, display
d = display.Display()
s = d.screen()
root = s.root
random.seed(185)
tile = root.create_pixmap(64, 64, s.root_depth)
gc = tile.create_gc()
tile.put_image(gc, 0, 0, 64, 64, X.ZPixmap, s.root_depth, 0,
               bytes(random.randrange(256) if i % 4 != 3 else 0 for i in range(64 * 64 * 4)))
root.change_attributes(background_pixmap=tile)
root.clear_area(0, 0, 0, 0)
d.sync()
PY
sleep 1
for at in "301 203" "900 500"; do
    xdotool mousemove $at
    sleep 1
    import -window root "root-${at% *}.png" > import.log 2>&1 || true
done
xwininfo -root | grep -E 'Width|Height' > root.txt
kill $hold 2>/dev/null || true
