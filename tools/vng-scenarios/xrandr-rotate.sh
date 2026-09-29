# Sourced by tools/vng-shot.sh INSIDE the guest (DISPLAY=:7, cwd = artifacts).
# CRTC rotation/reflection: geometry, GetCrtcInfo, monitors, Xinerama, the
# SetScreenSize crop rule and a root capture over a known pattern, per step.
# With --outputs 2 only the second output, right of the first, is rotated.
#   tools/vng-shot.sh [--outputs 2] --server xorg --dump none --name rotate-xorg \
#       --scenario tools/vng-scenarios/xrandr-rotate.sh
set -u
# The guest runs with -e; a failed probe must not end the run.
set +e
# Hold a connection: Xorg resets (dropping the rotation) when its last client leaves.
xprop -root -spy > /dev/null 2>&1 &
hold=$!
sleep 1
cat > rot.py <<'PY'
import sys
from Xlib import display, X
from Xlib.ext import randr, xinerama
d = display.Display()
d.set_error_handler(lambda *a: None)
root = d.screen().root
s = d.screen()
def pattern():
    g = root.get_geometry()
    w, h = 2048, 2048
    pm = root.create_pixmap(w, h, s.root_depth)
    gc = pm.create_gc()
    for y0 in range(0, h, 16):
        rows = bytearray()
        for y in range(y0, y0 + 16):
            for x in range(w):
                rows += bytes(((x * 7 + y) & 255, y & 255, x & 255, 0))
        pm.put_image(gc, 0, y0, w, 16, X.ZPixmap, s.root_depth, 0, bytes(rows))
    root.change_attributes(background_pixmap=pm)
    root.clear_area(0, 0, 0, 0)
    d.sync()
def info():
    r = randr.get_screen_resources(root)
    g = root.get_geometry()
    print("  screen", g.width, "x", g.height)
    for c in r.crtcs:
        ci = randr.get_crtc_info(d, c, r.config_timestamp)
        if ci.mode == 0:
            continue
        print("  crtc", hex(c), "pos", ci.x, ci.y, "size", ci.width, ci.height, "mode", hex(ci.mode),
              "rotation", hex(ci.rotation), "rotations", hex(ci.possible_rotations))
        t = randr.get_crtc_transform(d, c)
        print("   cur", t.current_transform, "pend", t.pending_transform, "filter", t.current_filter_name)
    print("  timestamp", r.timestamp, "config", r.config_timestamp)
def crop():
    g = root.get_geometry()
    w, h = g.width, g.height
    for (tw, th) in [(w, h), (w - 1, h), (w, h - 1), (h, w), (4000, 4000)]:
        try:
            randr.set_screen_size(root, tw, th, 300, 200)
        except Exception as e:
            print(f"  {tw}x{th}: raised {type(e).__name__}")
        g2 = root.get_geometry()
        verdict = "applied" if (g2.width, g2.height) == (tw, th) else "rejected"
        print(f"  crop {tw}x{th}: {verdict}")
        randr.set_screen_size(root, w, h, 300, 200); root.get_geometry()
cmd = sys.argv[1]
{"pattern": pattern, "info": info, "crop": crop}[cmd]()
PY
cat > events.py <<'PY'
from Xlib import display
from Xlib.ext import randr
d = display.Display()
root = d.screen().root
root.xrandr_select_input(randr.RRScreenChangeNotifyMask | randr.RRCrtcChangeNotifyMask
                         | randr.RROutputChangeNotifyMask)
d.flush()
while True:
    e = d.next_event()
    n = e.__class__.__name__
    if n == "ScreenChangeNotify":
        print(n, "rotation", hex(e.rotation), "size", e.width_in_pixels, e.height_in_pixels,
              "mm", e.width_in_millimeters, e.height_in_millimeters, flush=True)
    elif n == "CrtcChangeNotify":
        print(n, "rotation", hex(e.rotation), "geom", e.x, e.y, e.width, e.height, flush=True)
    elif n == "OutputChangeNotify":
        print(n, "rotation", hex(e.rotation), flush=True)
PY
python3 events.py > events.log 2>&1 &
events=$!
out1=$(xrandr | awk '/ connected/{print $1; exit}')
out2=$(xrandr | awk '/ connected/{n++} / connected/ && n==2 {print $1}')
python3 rot.py pattern > pattern.log 2>&1
step() {
    tag=$1; shift
    echo "===== $tag: xrandr $*"
    echo "===== $tag" >> events.log
    xrandr "$@" 2>&1
    sleep 1
    xrandr --verbose | grep -E '^Screen| connected|Transform|^ {12}[-0-9]|filter:|Rotation|Reflection|^\s+[0-9]+x[0-9]+ .*\*current' 2>&1
    xrandr --listmonitors 2>&1
    xwininfo -root | grep -E 'Width|Height'
    xdpyinfo | grep -E 'dimensions|resolution'
    xdpyinfo -ext XINERAMA | sed -n '/XINERAMA/,$p' | grep -E 'head|XINERAMA'
    python3 rot.py info
    python3 rot.py crop
    root_clear
    for at in "301 203" "700 500"; do
        xdotool mousemove $at
        sleep 0.5
        import -window root "root-$tag-${at% *}.png" > /dev/null 2>&1 || true
    done
}
root_clear() { python3 -c "
from Xlib import display
d = display.Display(); d.screen().root.clear_area(0, 0, 0, 0); d.sync()"; }
{
    if [ -z "$out2" ]; then
        step normal --output "$out1" --rotate normal
        step left --output "$out1" --rotate left
        step right --output "$out1" --rotate right
        step inverted --output "$out1" --rotate inverted
        step reflect-x --output "$out1" --rotate normal --reflect x
        step reflect-y --output "$out1" --reflect y
        step reflect-xy --output "$out1" --reflect xy
        step left-reflect-x --output "$out1" --rotate left --reflect x
        step left-scale2 --output "$out1" --reflect normal --rotate left --scale 2x2
        step left-scale2-pos --output "$out1" --rotate left --scale 2x2 --pos 0x0
        step restore --output "$out1" --rotate normal --scale 1x1
    else
        xrandr --output "$out1" --pos 0x0 --output "$out2" --right-of "$out1" 2>&1
        step dual-normal --output "$out2" --rotate normal
        step dual-right-left --output "$out2" --rotate left
        step dual-right-inverted --output "$out2" --rotate inverted
        step dual-restore --output "$out2" --rotate normal
    fi
} > rotate.log 2>&1
xrandr --output "$out1" --rotate normal --scale 1x1 > /dev/null 2>&1 || true
kill $events $hold 2>/dev/null || true
