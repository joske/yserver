# Sourced by tools/vng-shot.sh INSIDE the guest (DISPLAY=:7, cwd = artifacts).
# RANDR 1.5 SetMonitor/DeleteMonitor: merged monitor list, Xinerama, errors, notifies.
#   tools/vng-shot.sh [--outputs 2] --server xorg --dump none --name monitors-xorg \
#       --scenario tools/vng-scenarios/xrandr-monitors.sh
set -u
# Hold a connection: Xorg resets (dropping client monitors) when its last client leaves.
xprop -root -spy > /dev/null 2>&1 &
hold=$!
sleep 1
cat > mon.py <<'PY'
import sys
from Xlib import display, error, X
from Xlib.ext import randr, xinerama
from Xlib.protocol import rq

class RawSetMonitor(rq.Request):
    _request = rq.Struct(
        rq.Card8('opcode'), rq.Opcode(43), rq.RequestLength(), rq.Window('window'),
        rq.Card32('name'), rq.Bool('primary'), rq.Bool('automatic'), rq.Card16('noutput'),
        rq.Int16('x'), rq.Int16('y'), rq.Card16('width'), rq.Card16('height'),
        rq.Card32('mmw'), rq.Card32('mmh'), rq.List('outputs', rq.Card32Obj))

class RawDeleteMonitor(rq.Request):
    _request = rq.Struct(
        rq.Card8('opcode'), rq.Opcode(44), rq.RequestLength(), rq.Window('window'),
        rq.Card32('name'))

def open_display():
    dpy = display.Display()
    # python-xlib registers RANDR's errors at raw codes 0-2, shadowing core BadValue.
    for code in (0, 1, 2):
        dpy.display.error_classes[code] = error.xerror_class.get(code, error.XError)
    return dpy

d = open_display()
root = d.screen().root
op = d.query_extension(randr.extname).major_opcode
errs = []
d.set_error_handler(lambda e, r=None: errs.append(e))
res = randr.get_screen_resources(root)
oname = {o: randr.get_output_info(d, o, res.config_timestamp).name for o in res.outputs}
oid = {v: k for k, v in oname.items()}

def sync():
    root.get_geometry()
    out = [(e.__class__.__name__, e.code, e.resource_id, e.minor_opcode) for e in errs]
    errs.clear()
    return out

def raw_set(name, x, y, w, h, outs=(), primary=0, noutput=None, window=None, mm=(100, 50)):
    RawSetMonitor(display=d.display, opcode=op, window=window or root, name=name,
                  primary=primary, automatic=0,
                  noutput=len(outs) if noutput is None else noutput,
                  x=x, y=y, width=w, height=h, mmw=mm[0], mmh=mm[1], outputs=list(outs))

def dump(label):
    print("---", label)
    for active in (False, True):
        r = randr.get_monitors(root, active)
        print(f"  get_active={int(active)} n={len(r.monitors)} noutputs={r.outputs}")
        for m in r.monitors:
            print(f"    {d.get_atom_name(m.name)} primary={m.primary} automatic={m.automatic}"
                  f" {m.width_in_pixels}/{m.width_in_millimeters}x{m.height_in_pixels}/"
                  f"{m.height_in_millimeters}+{m.x}+{m.y} outputs={[oname.get(c, hex(c)) for c in m.crtcs]}")
    q = xinerama.query_screens(root)
    print(f"  xinerama count={xinerama.get_screen_count(root).screen_count}"
          f" active={xinerama.is_active(root)}"
          f" screens={[(s.x, s.y, s.width, s.height) for s in q.screens]}")

def atom(n):
    return d.intern_atom(n)

cmd = sys.argv[1]
if cmd == "dump":
    dump(sys.argv[2])
elif cmd == "set":
    # set NAME X Y W H PRIMARY OUT[,OUT]|none
    name, x, y, w, h, p, outs = sys.argv[2:9]
    ids = [] if outs == "none" else [oid[o] for o in outs.split(",")]
    raw_set(atom(name), int(x), int(y), int(w), int(h), ids, int(p))
    print(f"set {name}: {sync()}")
elif cmd == "del":
    RawDeleteMonitor(display=d.display, opcode=op, window=root, name=atom(sys.argv[2]))
    print(f"del {sys.argv[2]}: {sync()}")
elif cmd == "errors":
    first = oname[res.outputs[0]]
    cases = [
        ("set name=output-name", lambda: raw_set(atom(first), 0, 0, 10, 10)),
        ("set name=None", lambda: raw_set(0, 0, 0, 10, 10)),
        ("set name=0x7fffff", lambda: raw_set(0x7fffff, 0, 0, 10, 10)),
        ("set bad window", lambda: raw_set(atom("errmon"), 0, 0, 10, 10, window=0x7fffffe)),
        ("set noutput=1 with 0 outputs", lambda: raw_set(atom("errmon"), 0, 0, 10, 10, noutput=1)),
        ("set noutput=0 with 1 output", lambda: raw_set(atom("errmon"), 0, 0, 10, 10, outs=[res.outputs[0]], noutput=0)),
        ("set bogus output id", lambda: raw_set(atom("bogusout"), 0, 0, 10, 10, outs=[0x7777])),
        ("del never-set atom", lambda: RawDeleteMonitor(display=d.display, opcode=op, window=root, name=atom("nosuchmon"))),
        ("del None", lambda: RawDeleteMonitor(display=d.display, opcode=op, window=root, name=0)),
        ("del 0x7fffff", lambda: RawDeleteMonitor(display=d.display, opcode=op, window=root, name=0x7fffff)),
        ("del bad window", lambda: RawDeleteMonitor(display=d.display, opcode=op, window=0x7fffffe, name=atom("bogusout"))),
        ("del output-name", lambda: RawDeleteMonitor(display=d.display, opcode=op, window=root, name=atom(first))),
        ("del bogusout", lambda: RawDeleteMonitor(display=d.display, opcode=op, window=root, name=atom("bogusout"))),
    ]
    for label, f in cases:
        f()
        # Mask the window/atom ids that differ per server.
        names = {root.id: "root", 0x7fffffe: "badwin", atom(first): "output-atom",
                 atom("nosuchmon"): "name-atom", atom("bogusout"): "name-atom",
                 atom("errmon"): "name-atom"}
        got = [(n, c, names.get(int(getattr(v, 'id', v)), hex(int(getattr(v, 'id', v)))), m) for n, c, v, m in sync()]
        print(f"  {label}: {got}")
elif cmd == "events":
    ev = open_display()
    evroot = ev.screen().root
    evroot.change_attributes(event_mask=X.StructureNotifyMask)
    randr.select_input(evroot, randr.RRScreenChangeNotifyMask | randr.RRCrtcChangeNotifyMask
                       | randr.RROutputChangeNotifyMask | randr.RROutputPropertyNotifyMask)
    evroot.get_geometry()
    def drain(label):
        evroot.get_geometry()
        seen = []
        while ev.pending_events():
            e = ev.next_event()
            desc = e.__class__.__name__
            if e.type == X.ConfigureNotify:
                desc += f"(win={'root' if e.window.id == evroot.id else hex(e.window.id)} {e.width}x{e.height})"
            seen.append(desc)
        print(f"  {label}: {seen}")
    drain("baseline")
    raw_set(atom("evmon"), 0, 0, 100, 100); print("  errs", sync()); drain("after SetMonitor")
    raw_set(atom("evmon"), 0, 0, 200, 100); print("  errs", [e[:2] for e in sync()]); drain("after SetMonitor reusing a monitor name")
    raw_set(atom(oname[res.outputs[0]]), 0, 0, 100, 100); print("  errs", [e[:2] for e in sync()]); drain("after failed SetMonitor")
    RawDeleteMonitor(display=d.display, opcode=op, window=root, name=atom("evmon")); print("  errs", sync()); drain("after DeleteMonitor")
    RawDeleteMonitor(display=d.display, opcode=op, window=root, name=atom("evmon")); print("  errs", [e[:2] for e in sync()]); drain("after failed DeleteMonitor")
PY
out1=$(xrandr | awk '/ connected/{print $1; exit}')
out2=$(xrandr | awk '/ connected/{n++} / connected/ && n==2 {print $1}')
w=$(xwininfo -root | awk '/Width:/{print $2}')
h=$(xwininfo -root | awk '/Height:/{print $2}')
half=$((w / 2))
{
    echo "outputs: out1=$out1 out2=${out2:-none} screen ${w}x${h}"
    python3 mon.py dump initial
    echo "=== xrandr split $out1 into left/right"
    xrandr --setmonitor left "$half/170x$h/211+0+0" "$out1"
    xrandr --setmonitor right "$half/170x$h/211+$half+0" none
    xrandr --listmonitors; xrandr --listactivemonitors
    xdpyinfo -ext XINERAMA | sed -n '/^XINERAMA/,$p'
    python3 mon.py dump split
    echo "=== reuse the name left (new geometry)"
    xrandr --setmonitor left "$((half - 100))/150x$h/211+0+0" "$out1"; xrandr --listmonitors
    python3 mon.py dump reuse-left
    echo "=== '*' prefix through xrandr"
    xrandr --setmonitor '*star' "100/20x100/20+10+10" none
    python3 mon.py dump star
    xrandr --delmonitor star 2>&1; xrandr --delmonitor '*star' 2>&1
    python3 mon.py dump star-deleted
    echo "=== primary: client monitor primary, $out1 is the RANDR primary output"
    xrandr --output "$out1" --primary
    python3 mon.py del right
    python3 mon.py set right "$half" 0 "$half" "$h" 1 none
    python3 mon.py dump right-primary
    echo "=== a second primary client monitor clears the first"
    python3 mon.py set extra 0 0 10 10 1 none
    python3 mon.py dump extra-primary
    python3 mon.py del extra
    python3 mon.py dump extra-deleted
    echo "=== zero-geometry, no outputs"
    python3 mon.py set empty 0 0 0 0 0 none
    python3 mon.py dump empty
    echo "=== automatic geometry from outputs"
    xrandr --delmonitor left; xrandr --delmonitor right; xrandr --delmonitor empty
    xrandr --setmonitor autogeo auto "$out1"
    xrandr --listmonitors
    python3 mon.py dump autogeo
    if [ -n "${out2:-}" ]; then
        echo "=== two outputs: automatic geometry across both, then CRTC changes"
        xrandr --output "$out1" --pos 0x0 --output "$out2" --auto --right-of "$out1"
        xrandr --setmonitor both auto "$out1,$out2"
        python3 mon.py dump both
        xrandr --setmonitor on2 "300/80x200/50+5+5" "$out2"
        python3 mon.py dump on2
        xrandr --output "$out2" --off
        python3 mon.py dump out2-off
        xrandr --output "$out2" --auto --right-of "$out1"
        python3 mon.py dump out2-back
        xrandr --output "$out2" --primary
        python3 mon.py dump out2-primary
        xrandr --delmonitor both; xrandr --delmonitor on2
        echo "=== client primary on $out1 while uncovered $out2 is the primary output"
        python3 mon.py set cprim 0 0 0 0 1 "$out1"
        python3 mon.py dump cprim
        python3 mon.py del cprim
        xrandr --output "$out1" --primary
    fi
    xrandr --delmonitor autogeo
    python3 mon.py dump all-deleted
    echo "=== errors"
    python3 mon.py errors
    python3 mon.py dump after-errors
    echo "=== events"
    python3 mon.py events
    echo "=== persistence after the setting client exits"
    xrandr --setmonitor persist "50/10x50/10+0+0" none
    sleep 1
    python3 mon.py dump persist
    xrandr --delmonitor persist
} > monitors.log 2>&1 || true
kill $hold 2>/dev/null || true
