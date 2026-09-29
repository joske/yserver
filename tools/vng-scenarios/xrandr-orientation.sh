# Sourced by tools/vng-shot.sh INSIDE the guest (DISPLAY=:7, cwd = artifacts).
# RANDR 1.0 SetScreenConfig (`xrandr -o`): statuses, errors, what it does to
# screen/CRTC geometry, timestamps and events.
#   tools/vng-shot.sh --server xorg --dump none --name orient-xorg \
#       --scenario tools/vng-scenarios/xrandr-orientation.sh
set -u
set +e
xprop -root -spy > /dev/null 2>&1 &
hold=$!
sleep 1
cat > orient.py <<'PY'
import sys
from Xlib import display, error
from Xlib.ext import randr
d = display.Display()
for code in (0, 1, 2):
    d.display.error_classes[code] = error.xerror_class.get(code, error.XError)
root = d.screen().root
# python-xlib's RANDR init already sent QueryVersion 1.5: a 1.1+ client.
def screen_info():
    i = root.xrandr_get_screen_info()
    g = root.get_geometry()
    print("  info rotation", hex(i.rotation), "rotations", hex(i.set_of_rotations), "size_id", i.size_id,
          "sizes", [(s.width_in_pixels, s.height_in_pixels, s.width_in_millimeters, s.height_in_millimeters)
                    for s in i.sizes], "rate", i.rate, "ts", i.timestamp, "cts", i.config_timestamp)
    r = randr.get_screen_resources(root)
    for c in r.crtcs:
        ci = randr.get_crtc_info(d, c, r.config_timestamp)
        if ci.mode:
            print("  crtc pos", ci.x, ci.y, "size", ci.width, ci.height, "rotation", hex(ci.rotation))
    print("  screen", g.width, g.height)
    return i
def call(label, **kw):
    i = root.xrandr_get_screen_info()
    args = dict(size_id=0, rotation=1, config_timestamp=i.config_timestamp, rate=0, timestamp=0)
    args.update(kw)
    try:
        rep = root.xrandr_set_screen_config(**args)
        print(f"  {label}: status {rep.status} new_ts {'moved' if rep.new_timestamp != i.timestamp else 'same'}"
              f" cts {'same' if rep.new_config_timestamp == i.config_timestamp else 'moved'}"
              f" root {'ok' if rep.root.id == root.id else hex(rep.root.id)}")
    except error.XError as e:
        print(f"  {label}: error {type(e).__name__} code {e.code} value {getattr(e, 'resource_id', '?')}")
cmd = sys.argv[1]
if cmd == "info":
    screen_info()
elif cmd == "probe":
    i = screen_info()
    call("stale config timestamp", config_timestamp=i.config_timestamp + 1)
    call("old timestamp", timestamp=1)
    call("size_id 1", size_id=1)
    call("rotation 3", rotation=3)
    call("rotation 0x41", rotation=0x41)
    call("rate 1", rate=1, rotation=i.rotation)
    call(f"rate {i.rate}", rate=i.rate, rotation=i.rotation)
    call("same config", rotation=i.rotation)
    call("same config again", rotation=i.rotation)
    try:
        rep = randr._1_0SetScreenConfig(display=d.display, opcode=d.display.get_extension_major(randr.extname),
                                        drawable=root, timestamp=0, config_timestamp=i.config_timestamp,
                                        size_id=0, rotation=i.rotation)
        print("  1.0-sized request: status", rep.status)
    except error.XError as e:
        print(f"  1.0-sized request: error {type(e).__name__} code {e.code}")
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
    elif n == "ConfigureNotify":
        print(n, e.width, e.height, flush=True)
PY
python3 events.py >> events.log 2>&1 &
events=$!
{
    echo "===== probe"
    python3 orient.py probe
    for o in left right inverted normal 1 left; do
        echo "===== xrandr -o $o"
        timeout 30 xrandr -o "$o" 2>&1; echo "rc $?"
        sleep 1
        python3 orient.py info
        xrandr | grep -E ' connected'
        xrandr --listmonitors | tail -n +2
    done
    echo "===== probe while left"
    python3 orient.py probe
} > orient.log 2>&1
xrandr -o normal > /dev/null 2>&1
kill $events $hold 2>/dev/null || true
