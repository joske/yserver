# Sourced by tools/vng-shot.sh INSIDE the guest (DISPLAY=:7, cwd = artifacts).
# One window spanning every output, filled whole with a new colour at uneven
# intervals; each round ends on a known colour and waits for
# span-paint-host.sh to dump and check every output. With independent flips,
# a paint lands while one output is flip-pending in most rounds.
set -u
set +e
xdotool mousemove 5 5
python3 - > span.log 2>&1 <<'PY'
import os, random, time
from Xlib import display, X
d = display.Display()
s = d.screen()
root = s.root
g = root.get_geometry()
w = root.create_window(0, 0, g.width, g.height, 0, s.root_depth, X.InputOutput,
                       X.CopyFromParent, background_pixel=0, override_redirect=True)
w.map()
d.sync()
time.sleep(1)
rng = random.Random(7)
gc = w.create_gc()
for r in range(int(os.environ.get("SPAN_ROUNDS", "10"))):
    for _ in range(40):
        gc.change(foreground=rng.randrange(1 << 24))
        w.fill_rectangle(gc, 0, 0, g.width, g.height)
        d.sync()
        time.sleep(rng.random() * 0.03)
    final = rng.randrange(1 << 24)
    gc.change(foreground=final)
    w.fill_rectangle(gc, 0, 0, g.width, g.height)
    d.sync()
    time.sleep(1)
    with open(f"expect-{r}", "w") as f:
        f.write(f"{final:06x}\n")
    open(f"READY-{r}", "w").close()
    while not os.path.exists(f"DONE-{r}"):
        time.sleep(0.2)
open("ROUNDS-DONE", "w").close()
PY
