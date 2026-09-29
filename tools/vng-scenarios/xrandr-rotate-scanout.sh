# Sourced by tools/vng-shot.sh INSIDE the guest (DISPLAY=:7, cwd = artifacts).
# yserver only (Xorg has no scanout dump): per rotation/reflection, a root
# capture and, via xrandr-rotate-scanout-host.sh, a scanout dump of the same
# frame, over a known pattern with an invisible cursor.
set -u
set +e
xprop -root -spy > /dev/null 2>&1 &
hold=$!
sleep 1
python3 - > pattern.log 2>&1 <<'PY'
from Xlib import display, X
d = display.Display()
s = d.screen()
root = s.root
w = h = 1024
pm = root.create_pixmap(w, h, s.root_depth)
gc = pm.create_gc()
for y0 in range(0, h, 16):
    rows = bytearray()
    for y in range(y0, y0 + 16):
        for x in range(w):
            rows += bytes(((x * 7 + y) & 255, y & 255, x & 255, 0))
    pm.put_image(gc, 0, y0, w, 16, X.ZPixmap, s.root_depth, 0, bytes(rows))
root.change_attributes(background_pixmap=pm)
blank = root.create_pixmap(1, 1, 1)
cursor = blank.create_cursor(blank, (0, 0, 0), (0, 0, 0), 0, 0)
root.change_attributes(cursor=cursor)
root.clear_area(0, 0, 0, 0)
d.sync()
PY
out=$(xrandr | awk '/ connected/{print $1; exit}')
shot() {
    tag=$1; shift
    xrandr --output "$out" "$@" >> xrandr.log 2>&1
    sleep 2
    import -window root "root-$tag.png" > /dev/null 2>&1 || true
    touch "READY-$tag"
    for _ in $(seq 1 120); do [ -e "DONE-$tag" ] && break; sleep 0.5; done
}
shot normal --rotate normal
shot left --rotate left
shot right --rotate right
shot inverted --rotate inverted
shot reflect-x --rotate normal --reflect x
shot reflect-y --reflect y
shot left-scale2 --reflect normal --rotate left --scale 2x2 --filter nearest
xrandr --output "$out" --rotate normal --scale 1x1 > /dev/null 2>&1 || true
kill $hold 2>/dev/null || true
