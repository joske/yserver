# Sourced by tools/vng-shot.sh INSIDE the guest (DISPLAY=:7, cwd = artifacts).
# Issue #185 Q3: SetScreenSize's crop check with a scaled CRTC at a non-zero x.
#   tools/vng-shot.sh --outputs 2 --server xorg --dump none --name crop-xorg \
#       --scenario tools/vng-scenarios/xrandr-scale-crop.sh
set -u
xprop -root -spy > /dev/null 2>&1 &
hold=$!
sleep 1
python3 - > crop.log 2>&1 <<'PY'
from Xlib import display, error
from Xlib.ext import randr
d = display.Display()
root = d.screen().root
res = randr.get_screen_resources(root)
outs = [o for o in res.outputs if randr.get_output_info(d, o, res.config_timestamp).connection == 0]
infos = [randr.get_output_info(d, o, res.config_timestamp) for o in outs]
crtcs = [i.crtc for i in infos]
def geo():
    r = randr.get_screen_resources(root)
    s = d.screen()
    g = d.screen().root.get_geometry()
    print("  screen", g.width, "x", g.height)
    for c in crtcs:
        ci = randr.get_crtc_info(d, c, r.config_timestamp)
        print("  crtc", hex(c), "pos", ci.x, ci.y, "size", ci.width, ci.height, "mode", hex(ci.mode))
print("initial"); geo()
PY
out2=$(xrandr | awk '/ connected/{n++} / connected/ && n==2 {print $1}')
out1=$(xrandr | awk '/ connected/{print $1; exit}')
{
  echo "outputs: $out1 $out2"
  xrandr --output "$out1" --pos 0x0 --output "$out2" --right-of "$out1" 2>&1
  xrandr | grep -E '^Screen| connected'
  echo "=== scale $out2 2x2"
  xrandr --output "$out2" --scale 2x2 2>&1
  xrandr | grep -E '^Screen| connected'
} >> crop.log 2>&1
probe() {
python3 - >> crop.log 2>&1 <<'PY'
from Xlib import display, error
from Xlib.ext import randr
d = display.Display()
d.set_error_handler(lambda *a: None)
root = d.screen().root
g = root.get_geometry()
w, h = g.width, g.height
print("=== SetScreenSize probes from", w, "x", h)
for (tw, th) in [(w, h), (w - 1, h), (3841, h), (3840, h), (3839, h), (w, 1441), (w, 1440), (w, 1439), (3840, 1440), (3840, 1439)]:
    try:
        randr.set_screen_size(root, tw, th, 300, 200)
    except Exception as e:
        print(f"  {tw}x{th}: raised {type(e).__name__}")
    # python-xlib reports errors asynchronously: judge by the readback.
    g2 = root.get_geometry()
    verdict = "applied" if (g2.width, g2.height) == (tw, th) else "rejected"
    print(f"  {tw}x{th}: {verdict} (screen {g2.width}x{g2.height})")
    try:
        randr.set_screen_size(root, w, h, 300, 200); root.get_geometry()
    except Exception as e:
        print("  restore failed", type(e).__name__)
PY
}
probe
{
  echo "=== control: $out2 back to 1x1 (identity), right of $out1"
  xrandr --output "$out2" --scale 1x1 --right-of "$out1" 2>&1
  xrandr | grep -E '^Screen| connected'
} >> crop.log 2>&1
probe
xrandr --output "$out2" --scale 1x1 > /dev/null 2>&1 || true
kill $hold 2>/dev/null || true
