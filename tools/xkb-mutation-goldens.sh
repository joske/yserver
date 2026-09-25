#!/usr/bin/env bash
# xkb-mutation-goldens.sh — regenerate the issue #171 Xorg goldens
#
#   crates/yserver/src/kms/testdata/xorg-xkb-change-keyboard-mapping.txt
#   crates/yserver/src/kms/testdata/xorg-xkb-set-modifier-mapping.txt
#   crates/yserver/src/kms/testdata/xorg-xkbcomp-upload-trace.txt
#
# Every value in those files is Xvfb output recorded by tools/xkb-mutation-probe.c
# (or x11trace). Never hand-edit them; rerun this script.
#
# Needs: Xvfb, setxkbmap, xmodmap, xkbcomp, x11trace, xdpyinfo, python3, cc with
# xcb/xcb-xkb/xcb-xtest headers. Uses displays :$DISP (Xvfb) and :$PROXY
# (x11trace's fake display); both must be free. Only the Xvfb this script
# starts is ever killed (by pid).
#
# usage: tools/xkb-mutation-goldens.sh [ckm|smm|xkbcomp ...]   (default: all)
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
TD=$ROOT/crates/yserver/src/kms/testdata
DISP=${DISP:-92}
PROXY=${PROXY:-97}
WORK=$(mktemp -d "${TMPDIR:-/tmp}/xkb-goldens.XXXXXX")
PROBE=$WORK/probe
XPID=

cleanup() {
    if [ -n "$XPID" ]; then kill "$XPID" 2>/dev/null || true; fi
    rm -rf "$WORK"
}
trap cleanup EXIT

cc -O1 -Wall -o "$PROBE" "$ROOT/tools/xkb-mutation-probe.c" \
    $(pkg-config --cflags --libs xcb xcb-xkb xcb-xtest)

start_x() {
    Xvfb ":$DISP" -noreset >"$WORK/xvfb.log" 2>&1 &
    XPID=$!
    for _ in $(seq 200); do
        xdpyinfo -display ":$DISP" >/dev/null 2>&1 && return 0
        sleep 0.05
    done
    echo "Xvfb :$DISP did not start" >&2
    exit 1
}

stop_x() {
    kill "$XPID"
    wait "$XPID" 2>/dev/null || true
    XPID=
}

# fresh server with LAYOUT [OPTION]
fresh() {
    start_x
    if [ "${2:--}" = "-" ]; then
        setxkbmap -display ":$DISP" -rules evdev -model pc105 -layout "$1"
    else
        setxkbmap -display ":$DISP" -rules evdev -model pc105 -layout "$1" -option "$2"
    fi
}

versions() {
    start_x
    local xorg vopt
    xorg=$(xdpyinfo -display ":$DISP" | sed -n 's/^X.Org version: //p')
    stop_x
    vopt=$(Xvfb -version 2>&1 | grep -m1 -o 'Unrecognized option: -version' || true)
    echo "# server: X.Org version $xorg (xdpyinfo)${vopt:+; \`Xvfb -version\` -> \"$vopt\" (not supported by this build)}"
    echo "# pacman -Q: $(pacman -Q xkeyboard-config xorg-server-xvfb | tr '\n' ' ' | sed 's/ $//')"
}

# decode an x11trace log's client requests; XKB Set* headers decoded from
# the unparsed bytes (x11trace -m 48 shows the first 48 bytes after the
# 4-byte request header)
decode_trace() {
    python3 - "$1" "${2:-all}" <<'EOF'
import re, sys
path, mode = sys.argv[1], sys.argv[2]
def u16(b, o): return b[o] | b[o+1] << 8
def u32(b, o): return b[o] | b[o+1] << 8 | b[o+2] << 16 | b[o+3] << 24
def setmap(b):
    f = ['deviceSpec=0x%x' % u16(b,0), 'present=0x%04x' % u16(b,2), 'flags=0x%x' % u16(b,4),
         'minKeyCode=%d' % b[6], 'maxKeyCode=%d' % b[7], 'types=%d+%d' % (b[8], b[9]),
         'syms=%d+%d total=%d' % (b[10], b[11], u16(b,12)),
         'acts=%d+%d total=%d' % (b[14], b[15], u16(b,16)),
         'behaviors=%d+%d total=%d' % (b[18], b[19], b[20]),
         'explicit=%d+%d total=%d' % (b[21], b[22], b[23]),
         'modmap=%d+%d total=%d' % (b[24], b[25], b[26]),
         'vmodmap=%d+%d total=%d' % (b[27], b[28], b[29]), 'virtualMods=0x%04x' % u16(b,30)]
    return ' '.join(f)
def setindmap(b): return 'deviceSpec=0x%x which=0x%08x' % (u16(b,0), u32(b,4))
def setcompat(b):
    return ('deviceSpec=0x%x recomputeActions=%d truncateSI=%d groups=0x%02x firstSI=%d nSI=%d'
            % (u16(b,0), b[3], b[4], b[5], u16(b,6), u16(b,8)))
def setnames(b):
    return ('deviceSpec=0x%x virtualMods=0x%04x which=0x%08x types=%d+%d ktLevels=%d+%d '
            'indicators=0x%08x groupNames=0x%02x nRadioGroups=%d keys=%d+%d nKeyAliases=%d '
            'totalKTLevelNames=%d' % (u16(b,0), u16(b,2), u32(b,4), b[8], b[9], b[10], b[11],
            u32(b,12), b[16], b[17], b[18], b[19], b[20], u16(b,22)))
def setgeom(b):
    return ('deviceSpec=0x%x nShapes=%d nSections=%d widthMM=%d heightMM=%d nProperties=%d '
            'nColors=%d nDoodads=%d nKeyAliases=%d' % (u16(b,0), b[2], b[3], u16(b,8), u16(b,10),
            u16(b,12), u16(b,14), u16(b,16), u16(b,18)))
dec = {'SetMap': setmap, 'SetIndicatorMap': setindmap, 'SetCompatMap': setcompat,
       'SetNames': setnames, 'SetGeometry': setgeom}
other = {}
for line in open(path):
    m = re.match(r'^(\d+):<:([0-9a-f]+):\s*(\d+): (.*)$', line.rstrip('\n'))
    if not m:
        continue
    seq, size, text = int(m.group(2), 16), int(m.group(3)), m.group(4)
    if mode == 'xkb' and 'XKEYBOARD' not in text:
        name = re.match(r'(?:[\w-]+-)?Request\(\d+(?:,\d+)?\): (\w+)', text)
        key = name.group(1) if name else text
        other[key] = other.get(key, 0) + 1
        continue
    r = re.match(r'XKEYBOARD-Request\(\d+,(\d+)\): (\w+) .*unparsed-data=([0-9a-fx,]+)', text)
    if r and r.group(2) in dec:
        b = [int(x, 16) for x in r.group(3).strip(',').split(',')]
        text = 'XKEYBOARD-Request(%s): %s %s' % (r.group(1), r.group(2), dec[r.group(2)](b))
    print('%04x %5d %s' % (seq, size, text))
if other:
    print('non-XKB requests: ' + ', '.join('%s x%d' % kv for kv in other.items()))
EOF
}

# ---------------------------------------------------------------------------
gen_ckm() {
    local out=$TD/xorg-xkb-change-keyboard-mapping.txt
    python3 - "$TD/xorg-change-keyboard-mapping.txt" >"$WORK/ckm_cases.tsv" <<'EOF'
import re, sys
cases = []
for l in open(sys.argv[1]):
    l = l.rstrip('\n')
    m = re.match(r'## \+ first=(\d+) kpk=(\d+) count=(\d+) syms=(\S*)', l)
    if m:
        cases[-1]['steps'].append(m.groups()); cases[-1]['hdr'].append(l); continue
    m = re.match(r'## (\S+) layout=(\S+) options=(\S+) first=(\d+) kpk=(\d+) count=(\d+) syms=(\S*)', l)
    if m:
        cases.append({'layout': m.group(2), 'opt': m.group(3), 'steps': [m.groups()[3:]], 'hdr': [l]})
        continue
    m = re.match(r'! first=(\d+) kpk=(\d+) count=(\d+) nsyms=(\d+) ->', l)
    if m:
        # the core golden does not record the keysyms of its edge requests;
        # these are chosen here and recorded in the '!' line
        f, k, c, n = map(int, m.groups())
        syms = ['61', '41'] if k == 2 else ['61', '62', '63'] if c == 3 else ['61']
        syms = (syms * n)[:n]
        cases.append({'layout': 'gb', 'opt': '-', 'steps': [(str(f), str(k), str(c), ','.join(syms))],
                      'hdr': ['! first=%d kpk=%d count=%d nsyms=%d syms=%s' % (f, k, c, n, ','.join(syms) or '-')]})
for c in cases:
    steps = ' '.join('ckm:%s:%s:%s:%s' % s for s in c['steps'])
    if len(c['steps']) > 1:
        steps += ' total'
    print('\t'.join([c['layout'], c['opt'], '|'.join(c['hdr']), steps]))
EOF
    {
        echo "# Xorg XKB view of ChangeKeyboardMapping (issue #171): Xvfb -noreset, fresh server per case"
        versions
        echo "# each case: setxkbmap -rules evdev -model pc105 -layout L [-option O]; then tools/xkb-mutation-probe.c"
        echo "# case set = every case of xorg-change-keyboard-mapping.txt (same names, layouts, options, requests,"
        echo "# '## +' follow-ups sent in order); its '!' edge requests on layout=gb follow, with the keysyms used"
        echo "# probe output lines (see the probe header for the full grammar):"
        echo "#   > ckm:FIRST:KPK:COUNT:SYMS   raw ChangeKeyboardMapping sent by the actor (syms hex; nsyms = syms listed)"
        echo "#   = ok | = error=CODE value=V request result"
        echo "#   e xkb NAME k=v... raw=HEX    XKB event on the XKB listener (XkbSelectEvents all, core kbd), arrival order;"
        echo "#                                MapNotify fields: types/syms/acts/beh/expl/modmap/vmodmap = first+count"
        echo "#                                raw = the 32 wire bytes; seq (bytes 2-3) and time (4-7) differ run to run"
        echo "#   e xkbl ...                   core event on the XKB listener, interleaved in arrival order"
        echo "#   e core ...                   core event on a separate non-XKB connection"
        echo "#   - KC <key> / + KC <key>      XkbGetMap(full=all, keys 8..255) before/after the request, differing keys only;"
        echo "#       <key> = kt=<type index per group 1..4> gi=<groupInfo> w=<width> syms=<width*groups hex>"
        echo "#               acts=<8 raw bytes per action, '-' none> beh=<type:data> expl=<explicit> mm=<modmap> vmm=<vmodmap>"
        echo "#   -type/+type N, -vmods/+vmods, ntypes, repeat KC A->B, enabledControls   other XkbGetMap/GetControls changes"
        echo "#   coremodmap kpm=K 0:... 7:...  core GetModifierMapping after the request"
        echo "#   > total                      XkbGetMap delta from before the first request to after the last"
        while IFS=$'\t' read -r L O H STEPS; do
            fresh "$L" "$O"
            echo "$H" | tr '|' '\n'
            # shellcheck disable=SC2086
            "$PROBE" -d ":$DISP" $STEPS
            stop_x
        done <"$WORK/ckm_cases.tsv"
    } >"$out"
}

# ---------------------------------------------------------------------------
XMODMAP_ARGS=(-e 'remove Lock = Caps_Lock' -e 'keycode 66 = Control_L' -e 'add Control = Control_L')
XMODMAP_QUOTED="-e 'remove Lock = Caps_Lock' -e 'keycode 66 = Control_L' -e 'add Control = Control_L'"

smm_case() { # LAYOUT NAME STEP...
    local layout=$1 name=$2
    shift 2
    fresh "$layout"
    echo "## $name layout=$layout"
    "$PROBE" -d ":$DISP" "$@"
    stop_x
}

xmodmap_steps() { # trace -> probe steps for its ChangeKeyboardMapping / SetModifierMapping
    python3 - "$1" <<'EOF'
import re, sys
steps = []
for line in open(sys.argv[1]):
    m = re.search(r'Request\(100\): ChangeKeyboardMapping keycode-count=\S+ first-keycode=(0x[0-9a-f]+) '
                  r'keysyms-per-keycode=(0x[0-9a-f]+) keysyms=([0-9a-fx,]+);', line)
    if m:
        first, kpk = int(m.group(1), 16), int(m.group(2), 16)
        syms = [int(s, 16) for s in m.group(3).split(',')]
        steps.append('ckm:%d:%d:%d:%s' % (first, kpk, len(syms) // kpk, ','.join('%x' % s for s in syms)))
    m = re.search(r'Request\(118\): SetModifierMapping keycodes-per-modifier=(0x[0-9a-f]+) keycodes=([0-9a-fx,]+);', line)
    if m:
        steps.append('smm:%d:%s' % (int(m.group(1), 16), ','.join(str(int(k, 16)) for k in m.group(2).split(','))))
print(' '.join(steps))
EOF
}

gen_smm() {
    local out=$TD/xorg-xkb-set-modifier-mapping.txt
    local warm="down:24 up:24"
    {
        echo "# Xorg XKB view of SetModifierMapping (issue #171): Xvfb -noreset, fresh server per case"
        versions
        echo "# each case: setxkbmap -rules evdev -model pc105 -layout L; then tools/xkb-mutation-probe.c"
        echo "# output grammar as in xorg-xkb-change-keyboard-mapping.txt, plus:"
        echo "#   > smm:KPM:KC,...     SetModifierMapping with exactly these keycodes"
        echo "#   > smmx:EDITS         SetModifierMapping built from the current GetModifierMapping with EDITS"
        echo "#                        (same | -KC@MOD | +KC@MOD | kpm=N); 'sent kpm=K keys=...' is the request sent"
        echo "#   = status=S           reply: 0 Success, 1 Busy, 2 Failed; '= error=...' for protocol errors"
        echo "#   > down:KC / up:KC    XTEST FakeInput key press/release on the actor"
        echo "#   > run:CMD            an external client acts (exit status recorded)"
        echo "# '$warm' warms XTEST up first where keys are held: the first XTEST event makes the XTEST"
        echo "# keyboard the core keyboard's last slave, which itself sends NewKeyboardNotify/MappingNotify."
        echo "# The xmodmap-replay case replays, one by one, the mutating requests of"
        echo "#   xmodmap $XMODMAP_QUOTED"
        echo "# as captured with x11trace -n (request list per layout below). NB x11trace prints the request"
        echo "# length in words as ChangeKeyboardMapping 'keycode-count'; the real count is nsyms/kpk."
        echo "# xmodmap-direct runs that xmodmap command itself against the server."
        for L in gb us; do
            fresh "$L"
            x11trace -n -d ":$DISP" -D ":$PROXY" -m 1000 -o "$WORK/xmodmap-$L.trace" -- \
                xmodmap "${XMODMAP_ARGS[@]}" >/dev/null 2>&1
            stop_x
            echo "#"
            echo "# xmodmap requests, layout=$L (seq bytes request):"
            decode_trace "$WORK/xmodmap-$L.trace" | sed 's/^/#   /'
        done
        for L in gb us; do
            local replay kpm2
            replay=$(xmodmap_steps "$WORK/xmodmap-$L.trace")
            case $L in
                gb) kpm2="smmx:-205@3,-206@6,kpm=2" ;;
                us) kpm2="smmx:-204@3,-205@3,-206@6,kpm=2" ;;
            esac
            # shellcheck disable=SC2086
            smm_case "$L" xmodmap-replay $replay total
            smm_case "$L" xmodmap-direct "run:xmodmap -display :$DISP $XMODMAP_QUOTED"
            smm_case "$L" same-map smmx:same
            smm_case "$L" busy-held-key-changes $warm down:50 smmx:-50@0 up:50 smmx:-50@0 total
            smm_case "$L" busy-held-modifier-unchanged $warm down:50 smmx:-66@1,+66@2 up:50 total
            smm_case "$L" held-nonmodifier-unaffected $warm down:38 smmx:-66@1,+66@2 up:38 total
            smm_case "$L" busy-held-key-becomes-modifier $warm down:38 smmx:+38@5 up:38 total
            smm_case "$L" kpm-wider-same-content smmx:kpm=5
            smm_case "$L" kpm-narrower "$kpm2"
            smm_case "$L" kpm-grow smmx:+38@3
            smm_case "$L" duplicate-keycode smmx:+50@2
            smm_case "$L" keycode-out-of-range smmx:+7@3
        done
    } >"$out"
}

# ---------------------------------------------------------------------------
gen_xkbcomp() {
    local out=$TD/xorg-xkbcomp-upload-trace.txt
    {
        echo "# xkbcomp keymap dump + upload against Xorg (issue #171): Xvfb -noreset, layout=gb"
        versions
        echo "# setxkbmap -rules evdev -model pc105 -layout gb; then, both under x11trace -n:"
        echo "#   xkbcomp -xkb \$DISPLAY dump.xkb                (dump)"
        echo "#   xkbcomp dump-edited.xkb \$DISPLAY              (upload; <AC01>/<AC02> symbols swapped)"
        echo "# request lines: seq(hex) bytes decoded-request; XKB Set* headers decoded from the raw bytes"
        echo "# (field names as in XKBproto.h; a+b = first+count). The upload runs as a probe 'run:' step,"
        echo "# so the listener events and the XkbGetMap delta it caused follow (grammar as in"
        echo "# xorg-xkb-change-keyboard-mapping.txt)."
        fresh gb
        x11trace -n -d ":$DISP" -D ":$PROXY" -m 48 -o "$WORK/dump.trace" -- \
            xkbcomp -xkb ":$PROXY" "$WORK/dump.xkb" >/dev/null 2>&1
        python3 - "$WORK/dump.xkb" "$WORK/dump-edited.xkb" <<'EOF'
import re, sys
t = open(sys.argv[1]).read()
a = re.search(r'key <AC01> \{(.*?)\};', t, re.S)
b = re.search(r'key <AC02> \{(.*?)\};', t, re.S)
assert a and b and a.start() < b.start()
t = t[:a.start(1)] + b.group(1) + t[a.end(1):b.start(1)] + a.group(1) + t[b.end(1):]
open(sys.argv[2], 'w').write(t)
EOF
        echo "## dump requests"
        decode_trace "$WORK/dump.trace" xkb
        echo "## edit"
        diff "$WORK/dump.xkb" "$WORK/dump-edited.xkb" | sed 's/^/# /' || true
        "$PROBE" -d ":$DISP" \
            "run:x11trace -n -d :$DISP -D :$PROXY -m 48 -o $WORK/upload.trace -- xkbcomp $WORK/dump-edited.xkb :$PROXY >/dev/null 2>&1" \
            | sed "s|$WORK/||g" >"$WORK/upload.out"
        echo "## upload requests"
        decode_trace "$WORK/upload.trace" xkb
        echo "## upload listener"
        cat "$WORK/upload.out"
        stop_x
    } >"$out"
}

targets=("$@")
[ ${#targets[@]} -eq 0 ] && targets=(ckm smm xkbcomp)
for t in "${targets[@]}"; do
    "gen_$t"
done
