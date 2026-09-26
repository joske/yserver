#!/usr/bin/env bash
# xkb-mutation-goldens.sh — regenerate the issue #171 Xorg goldens
#
#   crates/yserver/src/kms/testdata/xorg-xkb-change-keyboard-mapping.txt
#   crates/yserver/src/kms/testdata/xorg-xkb-set-modifier-mapping.txt
#   crates/yserver/src/kms/testdata/xorg-xkbcomp-upload-trace.txt
#   crates/yserver/src/kms/testdata/xorg-per-key-repeat.txt
#   crates/yserver/src/kms/testdata/xorg-xkb-pristine.txt
#   crates/yserver/src/kms/testdata/xorg-xkbcomp-steps.txt
#   crates/yserver/src/kms/testdata/xkbcomp-requests/CASE/{N-Request.bin,atoms.txt}
#   crates/yserver/src/kms/testdata/xorg-xkb-setmap-errors.txt
#   crates/yserver/src/kms/testdata/xkb-setmap-errors/NAME.bin
#   crates/yserver/src/kms/testdata/xorg-xkb-setmap-resize.txt
#   crates/yserver/src/kms/testdata/xkb-setmap-resize/NAME.bin
#   crates/yserver/src/kms/testdata/xorg-xkb-setcompat.txt
#   crates/yserver/src/kms/testdata/xkb-setcompat/NAME.bin
#   crates/yserver/src/kms/testdata/xorg-xkb-setnames.txt
#   crates/yserver/src/kms/testdata/xkb-setnames/NAME.bin
#
# Every value in those files is Xvfb output recorded by tools/xkb-mutation-probe.c
# (or x11trace). Never hand-edit them; rerun this script.
#
# Needs: Xvfb, setxkbmap, xmodmap, xkbcomp, x11trace, xdpyinfo, python3, cc with
# xcb/xcb-xkb/xcb-xtest headers. Uses displays :$DISP (Xvfb) and :$PROXY
# (x11trace's fake display); both must be free. Only the Xvfb this script
# starts is ever killed (by pid).
#
# usage: tools/xkb-mutation-goldens.sh [ckm|smm|xkbcomp|repeat|pristine|steps|errors|resize|setcompat|setnames ...]   (default: all)
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
            # a key moves to another modifier while its virtual modifier stays bound
            # (Super, NumLock, LevelThree): the vmod's real mapping changes
            smm_case "$L" vmod-remap-super smmx:-133@6,+133@5
            smm_case "$L" vmod-remap-numlock smmx:-77@4,+77@5
            smm_case "$L" vmod-remap-levelthree smmx:-92@7,+92@5
            # a vmod no key names any more keeps its mapping; named again, it is remapped
            smm_case "$L" vmod-stale-then-rebound "$kpm2" smmx:+205@6
            # every modifier cleared: GetModifierMapping reports kpm=0
            smm_case "$L" clear-all smm:1:0,0,0,0,0,0,0,0
            # ChangeKeyboardMapping, re-derived as XkbUpdateDescActions does: a key with
            # actions then one without (the action range stops at the last such key), and a
            # vmod-carrying key remapped (Num_Lock -> x, then x -> a: NumLock stays bound)
            smm_case "$L" ckm-mixed-actions ckm:37:1:2:ffe3,61
            smm_case "$L" ckm-vmod-key-remapped ckm:76:1:2:ffc9,78 ckm:77:1:1:61
            # a keymap load after a per-key repeat change (Xorg keeps the controls)
            smm_case "$L" reload-keeps-repeat smmx:+38@3 \
                "run:setxkbmap -display :$DISP -rules evdev -model pc105 -layout $L"
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

# ---------------------------------------------------------------------------
gen_repeat() {
    local out=$TD/xorg-per-key-repeat.txt
    {
        echo "# Xorg per-key auto-repeat of the keymap the server starts with (issue #171): fresh Xvfb -noreset"
        versions
        echo "# XkbFinishInit derives the keyboard's per-key repeat from the startup keymap; a later keymap"
        echo "# load keeps it (ProcXkbGetKbdByName: XkbCopyControls from the old keymap), so only the"
        echo "# startup keymap shows it. Lines:"
        echo "#   query: setxkbmap -query of the fresh server (its startup RMLVO)"
        echo "#   repeat: xset q 'auto repeating keys' (autoRepeats = per_key_repeat), 32 bytes in order as"
        echo "#           hex, keycode N = byte N/8 bit N%8"
        start_x
        setxkbmap -display ":$DISP" -query | sed 's/^/query: /'
        bits=$(xset -display ":$DISP" q | sed -n '/auto repeating keys:/,/^[^ ]/p' |
            grep -oE '[0-9a-f]{16}' | tr -d '\n')
        [ ${#bits} -eq 64 ] || { echo "xset q: no 32-byte bitmap" >&2; exit 1; }
        echo "repeat: $bits"
        stop_x
    } >"$out"
}

# ---------------------------------------------------------------------------
# Xorg's whole XKB description (probe -x full) of the fixture layouts, as
# setxkbmap leaves a fresh server: the state every xkbcomp-steps case starts
# from, and the reference for a keymap model seeded from xkbcommon.
gen_pristine() {
    local out=$TD/xorg-xkb-pristine.txt
    {
        echo "# Xorg's whole XKB description after setxkbmap (issue #171 phase 4): Xvfb -noreset, fresh server per layout"
        versions
        echo "# each case: setxkbmap -rules evdev -model pc105 -layout L [-option O]; then"
        echo "# tools/xkb-mutation-probe.c -x full. Lines ('= ' prefixed, see the probe header):"
        echo "#   keys MIN..MAX ntypes N enabledControls C | type N <type> | vmods | repeat <32 bytes hex>"
        echo "#   key KC <key>            XkbGetMap row, grammar as in xorg-xkb-change-keyboard-mapping.txt"
        echo "#   compat/si/groupcompat   XkbGetCompatMap(getAllSI, groups=0x0f); si act = 8 raw bytes"
        echo "#   indicators/indmap       XkbGetIndicatorMap(which=all)"
        echo "#   names/name/typename/levelnames/indname/vmodname/groupname/keyname/alias/rgname"
        echo "#                           XkbGetNames(which=all), atoms printed by name ('None' = 0)"
        echo "#   geometry                XkbGetGeometry(name=None) reply header"
        echo "#   coremodmap              core GetModifierMapping"
        for spec in us:- gb:- de:- us,ru:grp:alt_shift_toggle; do
            local L=${spec%%:*} O=${spec#*:}
            fresh "$L" "$O"
            echo "## pristine layout=$L options=$O"
            "$PROBE" -d ":$DISP" -x full
            stop_x
        done
    } >"$out"
}

# the edits of the xkbcomp-steps cases, applied to an `xkbcomp -xkb` dump
xkbcomp_edit() { # MODE SRC DST
    python3 - "$@" <<'EOF'
import re, sys
mode, src, dst = sys.argv[1], sys.argv[2], sys.argv[3]
t = open(src).read()
def sub1(pat, rep, s, flags=0):
    s, n = re.subn(pat, rep, s, count=1, flags=flags)
    assert n == 1, pat
    return s
if mode in ('identity', 'usru'):
    pass
elif mode == 'swap':
    # <AC01>/<AC02> symbols swapped (as xorg-xkbcomp-upload-trace.txt)
    a = re.search(r'key <AC01> \{(.*?)\};', t, re.S)
    b = re.search(r'key <AC02> \{(.*?)\};', t, re.S)
    assert a and b and a.start() < b.start()
    t = t[:a.start(1)] + b.group(1) + t[a.end(1):b.start(1)] + a.group(1) + t[b.end(1):]
elif mode == 'newtype':
    # a new first non-required type (Xorg index 4: the old types 4.. shift up by one),
    # used by <AC01> with three levels
    t = sub1(r'(xkb_types "[^"]*" \{\n\n    virtual_modifiers[^\n]*\n\n)',
             r'\1    type "YS_SHIFT_CTRL" {\n        modifiers= Shift+Control;\n'
             r'        map[Shift]= Level2;\n        map[Control]= Level3;\n'
             r'        level_name[Level1]= "Base";\n        level_name[Level2]= "Shift";\n'
             r'        level_name[Level3]= "Ctrl";\n    };\n', t)
    t = sub1(r'key <AC01> \{.*?\};',
             'key <AC01> {\n        type= "YS_SHIFT_CTRL",\n'
             '        symbols[Group1]= [ a, A, ae ]\n    };', t, re.S)
elif mode == 'compat':
    # the Caps_Lock interpret sets Control instead of locking Lock; the Caps Lock LED
    # follows Shift
    t = sub1(r'(interpret Caps_Lock\+AnyOfOrNone\(all\) \{\s*action= )LockMods\(modifiers=Lock\);',
             r'\1SetMods(modifiers=Control);', t)
    t = sub1(r'(indicator "Caps Lock" \{\s*!allowExplicit;\s*whichModState= locked;\s*modifiers= )Lock;',
             r'\1Shift;', t)
elif mode == 'capsctrl':
    # caps as control the xkb way: <CAPS> is Control_L and under Control
    t = sub1(r'key <CAPS> \{[^\n]*\};', 'key <CAPS> {         [       Control_L ] };', t)
    t = sub1(r'modifier_map Lock \{ <CAPS> \};', 'modifier_map Control { <CAPS> };', t)
elif mode == 'droptype':
    # an unused type (SHIFT+ALT, the last) removed: xkbcomp sends one type fewer
    t = sub1(r'\n    type "SHIFT\+ALT" \{.*?\};', '', t, re.S)
elif mode == 'explicit':
    # per-key explicit properties: <COMP> gets its own action, <RCTL> its own
    # auto-repeat and virtual modifier map
    t = sub1(r'key <COMP> \{[^\n]*\};',
             'key <COMP> {\n        symbols[Group1]= [ Menu ],\n'
             '        actions[Group1]= [ LockGroup(group=+1) ]\n    };', t)
    t = sub1(r'key <RCTL> \{[^\n]*\};',
             'key <RCTL> {\n        repeat= No,\n        virtualMods= Alt,\n'
             '        symbols[Group1]= [ Control_R ]\n    };', t)
elif mode == 'range':
    # the keycode range shrinks: maximum 255 -> 247, keycodes 248..255 dropped
    t = sub1(r'maximum = 255;', 'maximum = 247;', t)
    t = re.sub(r'\n    <I2(4[89]|5[0-5])> = \d+;', '', t)
    t = re.sub(r'\n    key <I2(4[89]|5[0-5])> \{[^\n]*\};', '', t)
else:
    sys.exit('xkbcomp_edit: bad mode ' + mode)
open(dst, 'w').write(t)
EOF
}

# an upload trace (x11trace -m big) -> DIR/N-Request.bin (each XKB request after
# UseExtension, header included, byte-exact) + DIR/atoms.txt (xkbcomp's
# InternAtom replies "0xVALUE NAME", in order)
xkbcomp_extract() { # TRACE DIR
    python3 - "$@" <<'EOF'
import os, re, sys
trace, out = sys.argv[1], sys.argv[2]
os.makedirs(out, exist_ok=True)
atoms, n = [], 0
for line in open(trace):
    line = line.rstrip('\n')
    m = re.match(r'^\d+:>:[0-9a-f]+:\d+: Reply to InternAtom: atom=(0x[0-9a-f]+)\("(.*)"\)$', line)
    if m:
        atoms.append((m.group(1), m.group(2)))
        continue
    m = re.match(r'^\d+:<:[0-9a-f]+:\s*(\d+): XKEYBOARD-Request\((\d+),(\d+)\): (\w+) '
                 r'.*unparsed-data=([0-9a-fx,]+);$', line)
    if m and int(m.group(3)) != 0:
        size, major, minor = int(m.group(1)), int(m.group(2)), int(m.group(3))
        body = bytes(int(x, 16) for x in m.group(5).strip(',').split(','))
        req = bytes([major, minor, (size // 4) & 0xff, (size // 4) >> 8]) + body
        assert len(req) == size, (m.group(4), len(req), size)
        n += 1
        with open(os.path.join(out, '%d-%s.bin' % (n, m.group(4))), 'wb') as f:
            f.write(req)
with open(os.path.join(out, 'atoms.txt'), 'w') as f:
    for a, name in atoms:
        f.write('%s %s\n' % (a, name))
EOF
}

# probe output -> its '> total' section without the raw event hex
total_of() {
    sed -n '/^> total$/,$p' "$1"
}

gen_steps() {
    local out=$TD/xorg-xkbcomp-steps.txt reqs=$TD/xkbcomp-requests
    rm -rf "$reqs"
    mkdir -p "$reqs"
    fresh gb
    xkbcomp -xkb ":$DISP" "$WORK/gb.xkb" >/dev/null 2>&1
    stop_x
    fresh us,ru grp:alt_shift_toggle
    xkbcomp -xkb ":$DISP" "$WORK/usru.xkb" >/dev/null 2>&1
    stop_x
    {
        echo "# xkbcomp uploads replayed one request at a time against Xorg (issue #171 phase 4):"
        echo "# Xvfb -noreset, fresh server per run, every case on layout=gb"
        versions
        echo "# per case CASE:"
        echo "#  1. record: fresh server, setxkbmap -rules evdev -model pc105 -layout gb, then"
        echo "#     x11trace -n -m 1000000 -- xkbcomp CASE.xkb \$DISPLAY. Every XKB request after"
        echo "#     UseExtension is saved byte-exact as xkbcomp-requests/CASE/N-Request.bin, xkbcomp's"
        echo "#     InternAtom replies as xkbcomp-requests/CASE/atoms.txt ('0xATOM NAME', in order)."
        echo "#  2. replay: fresh server, same setxkbmap, then tools/xkb-mutation-probe.c -x with steps"
        echo "#     atoms:atoms.txt (the actor interns the names in order and checks every atom has the"
        echo "#     recorded value, so the recorded requests mean the same here), then xreq:N-Request.bin"
        echo "#     for each request (sent raw by the actor after its XkbUseExtension), then total"
        echo "#     (used by the check below, not listed)."
        echo "#  3. check: fresh server, same setxkbmap, the probe runs xkbcomp CASE.xkb itself (run:);"
        echo "#     its total delta must equal the replay's (the script fails otherwise)."
        echo "# CASE.xkb = \`xkbcomp -xkb\` dump of that fresh gb server, edited (the diff is listed), or"
        echo "# for usru the unedited dump of a fresh 'us,ru' + grp:alt_shift_toggle server."
        echo "# After each request: its result, the events on the probe's listeners, the XkbGetMap delta"
        echo "# (grammar as in xorg-xkb-change-keyboard-mapping.txt) and, from -x, the delta of"
        echo "# GetCompatMap / GetIndicatorMap / GetNames / GetGeometry lines ('-'/'+' + the line;"
        echo "# grammar as in xorg-xkb-pristine.txt, whose layout=gb case is the state before)."
        echo "# In raw event hex the sequence number (bytes 2-3) and time (4-7) differ between runs, and"
        echo "# NewKeyboardNotify bytes 18-31 are uninitialised stack in Xorg (differ too). A level name"
        echo "# printed '?' is one Xorg has not initialised (a type SetMap gave more levels, until SetNames"
        echo "# writes its level names; see take_xsnap in the probe)."
        for c in identity swap newtype droptype compat capsctrl explicit range usru; do
            local src=$WORK/gb.xkb
            [ "$c" = usru ] && src=$WORK/usru.xkb
            xkbcomp_edit "$c" "$src" "$WORK/$c.xkb"
            fresh gb
            x11trace -n -d ":$DISP" -D ":$PROXY" -m 1000000 -o "$WORK/$c.trace" -- \
                xkbcomp "$WORK/$c.xkb" ":$PROXY" >"$WORK/$c.xkbcomp.log" 2>&1 || true
            stop_x
            xkbcomp_extract "$WORK/$c.trace" "$reqs/$c"
            local steps=("atoms:$reqs/$c/atoms.txt")
            for f in $(ls "$reqs/$c" | grep '\.bin$' | sort -n); do
                steps+=("xreq:$reqs/$c/$f")
            done
            fresh gb
            "$PROBE" -d ":$DISP" -x "${steps[@]}" total >"$WORK/$c.replay"
            stop_x
            fresh gb
            "$PROBE" -d ":$DISP" -x "run:xkbcomp $WORK/$c.xkb :$DISP >/dev/null 2>&1" total \
                >"$WORK/$c.direct"
            stop_x
            if ! diff <(total_of "$WORK/$c.replay") <(total_of "$WORK/$c.direct") >"$WORK/$c.cmp"; then
                echo "xkbcomp-steps: case $c: replay and direct xkbcomp totals differ:" >&2
                cat "$WORK/$c.cmp" >&2
                exit 1
            fi
            echo "## case $c"
            if [ "$c" = usru ]; then
                echo "# uploaded: the us,ru dump unedited"
            else
                echo "# edit of the gb dump:"
                diff "$WORK/gb.xkb" "$WORK/$c.xkb" | sed 's/^/# /' || true
            fi
            echo "# xkbcomp requests (seq bytes decoded-request):"
            decode_trace "$WORK/$c.trace" xkb | sed 's/^/#   /'
            # the total only served the check: it is the sum of the steps
            sed -e "s|$reqs/||g" -e '/^> total$/,$d' "$WORK/$c.replay"
        done
    } >"$out"
}

# ---------------------------------------------------------------------------
# SetMap error vectors (phase 4c): requests built from the recorded identity
# SetMap (gen_steps' xkbcomp-requests/identity/1-SetMap.bin), each aimed at one
# of ProcXkbSetMap / _XkbSetMapCheckLength / _XkbSetMapChecks' rejections, plus
# XKB requests from a client that never called XkbUseExtension (BadAccess).
# One probe run per request, so every request comes from fresh connections
# (Xorg leaves a stale client->errorValue for BadLength/BadAccess; a fresh
# client's is 0).
setmap_error_requests() { # SRC DIR -> DIR/NAME.bin + DIR/cases.txt (STEP\tNAME\tDESCRIPTION)
    python3 - "$@" <<'EOF'
import os, struct, sys
src, out = sys.argv[1], sys.argv[2]
b = open(src, 'rb').read()
def u16(o): return b[o] | b[o+1] << 8
H = {'dev': u16(4), 'present': u16(6), 'flags': u16(8), 'min': b[10], 'max': b[11],
     'ft': b[12], 'nt': b[13], 'fs': b[14], 'ns': b[15], 'fa': b[18], 'na': b[19],
     'fb': b[22], 'nb': b[23], 'fe': b[25], 'ne': b[26], 'fm': b[28], 'nm': b[29],
     'fv': b[31], 'nv': b[32], 'vmods': u16(34)}
p = 36
types = []
for i in range(b[13]):
    ne, pre = b[p+5], b[p+6]
    n = 8 + 4 * ne + (4 * ne if pre else 0)
    types.append(bytearray(b[p:p+n])); p += n
syms = []
for i in range(b[15]):
    n = 8 + 4 * u16(p+6)
    syms.append(bytearray(b[p:p+n])); p += n
counts = list(b[p:p+b[19]]); p += (b[19] + 3) & ~3
acts = []
for c in counts:
    acts.append(bytes(b[p:p+8*c])); p += 8 * c
behs = [bytearray(b[p+4*i:p+4*i+4]) for i in range(b[24])]; p += 4 * b[24]
nv = bin(H['vmods']).count('1'); vmods = bytearray(b[p:p+nv]); p += (nv + 3) & ~3
expl = [bytearray(b[p+2*i:p+2*i+2]) for i in range(b[27])]; p += (2 * b[27] + 3) & ~3
mm = [bytearray(b[p+2*i:p+2*i+2]) for i in range(b[30])]; p += (2 * b[30] + 3) & ~3
vmm = [bytearray(b[p+4*i:p+4*i+4]) for i in range(b[33])]; p += 4 * b[33]
assert p == len(b), (p, len(b))
def pad(x): return x + bytes((-len(x)) % 4)
def build(h, types=None, syms=None, counts=None, acts=None, behs=None, vmods=None,
          expl=None, mm=None, vmm=None, extra=b'', length=None, raw_counts=None):
    """Only the parts given are sent; the header counts follow them unless given in h."""
    present = h.get('present', 0)
    body = b''
    tot = {}
    if types is not None:
        body += b''.join(types)
    if syms is not None:
        body += b''.join(syms)
        tot['syms'] = sum((len(s) - 8) // 4 for s in syms)
    if counts is not None:
        body += pad(bytes(counts)) + b''.join(acts)
        tot['acts'] = sum(counts)
    if behs is not None:
        body += b''.join(behs)
    if vmods is not None:
        body += pad(bytes(vmods))
    if expl is not None:
        body += pad(b''.join(expl))
    if mm is not None:
        body += pad(b''.join(mm))
    if vmm is not None:
        body += b''.join(vmm)
    body += extra
    g = lambda k, d: h.get(k, d)
    hdr = struct.pack('<HHHBBBBBBHBBHBBBBBBBBBBBBH',
        g('dev', 0x100), present, g('flags', 0), g('min', 8), g('max', 255),
        g('ft', 0), g('nt', len(types) if types is not None else 0),
        g('fs', 0), g('ns', len(syms) if syms is not None else 0), g('ts', tot.get('syms', 0)),
        g('fa', 0), g('na', len(counts) if counts is not None else 0), g('ta', tot.get('acts', 0)),
        g('fb', 0), g('nb', 0), g('tb', len(behs) if behs is not None else 0),
        g('fe', 0), g('ne', 0), g('te', len(expl) if expl is not None else 0),
        g('fm', 0), g('nm', 0), g('tm', len(mm) if mm is not None else 0),
        g('fv', 0), g('nv', 0), g('tv', len(vmm) if vmm is not None else 0),
        g('vmods', 0))
    assert len(hdr) == 32
    req = hdr + body
    n = (len(req) + 4) // 4 if length is None else length
    return bytes([b[0], 9, n & 0xff, n >> 8]) + req
cases = []
def case(name, what, data, step='xreq'):
    cases.append((name, what, step))
    open(os.path.join(out, name + '.bin'), 'wb').write(data)
os.makedirs(out, exist_ok=True)
# sanity: the builder rebuilds the recorded request byte for byte
full = dict(H, ts=u16(16), ta=u16(20), tb=b[24], te=b[27], tm=b[30], tv=b[33])
assert build(full, types, syms, counts, acts, behs, vmods, expl, mm, vmm) == b, 'rebuild'
T = lambda h: dict(h, present=0x01)
case('access', 'the recorded identity SetMap, from a client that never called XkbUseExtension', b, 'xreq0')
case('short', 'a 32-byte SetMap (xkbSetMapReq is 36)', bytes([b[0], 9, 8, 0]) + b[4:32])
case('length', 'the identity SetMap with 4 bytes too many (length + 1)', build(full, types, syms, counts, acts, behs, vmods, expl, mm, vmm, extra=bytes(4)))
case('present', 'present = 0x1ff (0x100 is no map component)', build(dict(full, present=0x1ff), types, syms, counts, acts, behs, vmods, expl, mm, vmm))
case('minkeycode', 'minKeyCode = 7', build(dict(full, min=7), types, syms, counts, acts, behs, vmods, expl, mm, vmm))
case('minmax', 'minKeyCode 9 > maxKeyCode 8', build(dict(full, min=9, max=8), types, syms, counts, acts, behs, vmods, expl, mm, vmm))
case('firsttype', 'types only, ResizeTypes, firstType 28 > num_types 27', build(dict(T({}), flags=1, ft=28), types=[types[4]]))
case('requiredtypes', 'types only, ResizeTypes, types 0+3 (< the 4 required)', build(dict(T({}), flags=1, ft=0), types=types[:3]))
case('typesnoresize', 'types only, no ResizeTypes, types 26+2 past num_types 27', build(dict(T({}), flags=0, ft=26), types=types[26:27] + types[4:5]))
t0 = bytearray(types[0]); t0[4] = 2
case('onelevelwidth', 'type 0 (ONE_LEVEL) with 2 levels', build(dict(T({}), ft=0), types=[t0]))
t4 = bytearray(types[4]); t4[4] = 0
case('zerolevels', 'type 4 with 0 levels', build(dict(T({}), ft=4), types=[t4]))
t1 = bytearray(types[1]); assert t1[5] >= 1; t1[8 + 1] = 0x05
case('entrymods', 'type 1 entry 0 realMods 0x05 outside the type mods', build(dict(T({}), ft=1), types=[t1]))
t1 = bytearray(types[1]); t1[8] = 2
case('entrylevel', 'type 1 entry 0 level 2 >= numLevels 2', build(dict(T({}), ft=1), types=[t1]))
pi = next(i for i, t in enumerate(types) if t[6] and t[5] > 0)
tp = bytearray(types[pi]); ne = tp[5]; tp[8 + 4 * ne + 1] = 0x80
case('preserve', 'type %d preserve 0 realMods 0x80 outside its entry' % pi, build(dict(T({}), ft=pi), types=[tp]))
case('existingkt', 'types only, ResizeTypes, types 0+4: existing keys use type >= 4', build(dict(T({}), flags=1, ft=0), types=types[:4]))
S = lambda kc: syms[kc - 8]
s = bytearray(S(38)); s[0] = 40
case('ktindex', 'syms only, key 38 ktIndex[0] 40 >= num_types', build(dict(present=0x02, fs=38), syms=[s]))
s = bytearray(S(38)); s[5] += 1
case('width', 'syms only, key 38 width one more than its type', build(dict(present=0x02, fs=38), syms=[s]))
s = bytearray(S(38)); n = s[6] | s[7] << 8; s[6] += 1; s += bytes(4)
case('nsyms', 'syms only, key 38 one keysym more than width x groups', build(dict(present=0x02, fs=38), syms=[s]))
s = bytearray(S(38)); s[4] = (s[4] & 0xf0) | 5
case('groups', 'syms only, key 38 with 5 groups', build(dict(present=0x02, fs=38), syms=[s]))
case('symrange', 'syms only, keys 250+10 past maxKeyCode', build(dict(present=0x02, fs=250), syms=[S(250)] * 10))
ns38 = S(38)[6] | S(38)[7] << 8
case('actcount', 'actions only, key 38 with %d actions (it has %d keysyms)' % (ns38 - 1, ns38), build(dict(present=0x10, fa=38), counts=[ns38 - 1], acts=[bytes(8 * (ns38 - 1))]))
case('behaviorkey', 'behaviors only, keys 38+1, a behavior for key 39', build(dict(present=0x20, fb=38, nb=1), behs=[bytes([39, 1, 0, 0])]))
case('radiogroup', 'behaviors only, key 38 radio group 33 (> XkbMaxRadioGroups)', build(dict(present=0x20, fb=38, nb=1), behs=[bytes([38, 2, 33, 0])]))
case('permanent', 'behaviors only, key 38 KB_Permanent|KB_Lock (not its current behavior)', build(dict(present=0x20, fb=38, nb=1), behs=[bytes([38, 0x81, 0, 0])]))
case('explicitkey', 'explicit only, keys 38+1, an entry for key 39', build(dict(present=0x08, fe=38, ne=1), expl=[bytes([39, 1])]))
case('explicitmin', 'explicit only, keys 7+1', build(dict(present=0x08, fe=7, ne=1), expl=[bytes([7, 1])]))
case('explicitrange', 'explicit only, keys 250+10 past maxKeyCode', build(dict(present=0x08, fe=250, ne=10), expl=[bytes([250, 1])]))
case('modmapkey', 'modmap only, keys 38+1, an entry for key 39', build(dict(present=0x04, fm=38, nm=1), mm=[bytes([39, 1])]))
case('vmodmapkey', 'vmodmap only, keys 38+1, an entry for key 39', build(dict(present=0x80, fv=38, nv=1), vmm=[bytes([39, 0, 1, 0])]))
gm = bytes([b[0], 8, 7, 0]) + struct.pack('<HHHBBBBBBBBHBBBBBBxx', 0x100, 0xff, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0)
assert len(gm) == 28
case('access-getmap', 'GetMap(full=all) from a client that never called XkbUseExtension', gm, 'xreq0')
se = bytes([b[0], 1, 4, 0]) + struct.pack('<HHHHHH', 0x100, 0x0fff, 0, 0x0fff, 0xff, 0xff)
case('access-selectevents', 'SelectEvents(all) from a client that never called XkbUseExtension', se, 'xreq0')
with open(os.path.join(out, 'cases.txt'), 'w') as f:
    for name, what, step in cases:
        f.write('%s\t%s\t%s\n' % (step, name, what))
EOF
}

# SetCompatMap / SetIndicatorMap error vectors (phase 4d): requests built from
# the recorded identity SetIndicatorMap and SetCompatMap (xkbcomp-requests/
# identity/2-SetIndicatorMap.bin, 3-SetCompatMap.bin), each aimed at one of
# ProcXkbSetCompatMap / _XkbSetCompatMap's dry-run checks or
# ProcXkbSetIndicatorMap's; appended to DIR/cases.txt.
compat_error_requests() { # SRCDIR DIR -> DIR/NAME.bin + DIR/cases.txt (appended)
    python3 - "$@" <<'EOF'
import os, struct, sys
src, out = sys.argv[1], sys.argv[2]
im = open(os.path.join(src, '2-SetIndicatorMap.bin'), 'rb').read()
cm = open(os.path.join(src, '3-SetCompatMap.bin'), 'rb').read()
assert im[1] == 14 and cm[1] == 11
def u16(b, o): return b[o] | b[o+1] << 8
# xkbSetCompatMapReq: deviceSpec@4 pad@6 recomputeActions@7 truncateSI@8 groups@9
# firstSI@10 nSI@12 pad@14, then nSI 16-byte interprets, then a 4-byte
# xkbModsWireDesc per group bit
nsi, groups = u16(cm, 12), cm[9]
sis = [cm[16 + 16 * i:32 + 16 * i] for i in range(nsi)]
gmods = cm[16 + 16 * nsi:]
assert len(gmods) == 4 * bin(groups).count('1'), 'recorded SetCompatMap layout'
def compat(recompute, truncate, groups, first, sis, gmods, extra=b'', length=None):
    body = struct.pack('<HBBBBHHH', 0x100, 0, recompute, truncate, groups, first, len(sis), 0)
    body += b''.join(sis) + gmods + extra
    n = (len(body) + 4) // 4 if length is None else length
    return bytes([cm[0], 11, n & 0xff, n >> 8]) + body
assert compat(cm[7], cm[8], groups, u16(cm, 10), sis, gmods) == cm, 'rebuild SetCompatMap'
# xkbSetIndicatorMapReq: deviceSpec@4 pad@6 which@8, then a 12-byte
# xkbIndicatorMapWireDesc per which bit
which = struct.unpack_from('<I', im, 8)[0]
maps = [bytearray(im[12 + 12 * i:24 + 12 * i]) for i in range(bin(which).count('1'))]
def indmap(which, maps, extra=b'', length=None):
    body = struct.pack('<HHI', 0x100, 0, which) + b''.join(bytes(m) for m in maps) + extra
    n = (len(body) + 4) // 4 if length is None else length
    return bytes([im[0], 14, n & 0xff, n >> 8]) + body
assert indmap(which, maps) == im, 'rebuild SetIndicatorMap'
cases = []
def case(name, what, data, step='xreq'):
    cases.append((name, what, step))
    open(os.path.join(out, name + '.bin'), 'wb').write(data)
case('compat-access', 'the recorded identity SetCompatMap, from a client that never called XkbUseExtension', cm, 'xreq0')
case('compat-short', 'a 12-byte SetCompatMap (xkbSetCompatMapReq is 16)', bytes([cm[0], 11, 3, 0]) + cm[4:12])
case('compat-length', 'the identity SetCompatMap with 4 bytes too many (length + 1)', compat(1, 1, groups, 0, sis, gmods, extra=bytes(4)))
case('compat-groupdata', 'groups 0x0f without their four group maps', compat(1, 1, groups, 0, sis, b''))
case('compat-firstsi', 'firstSI 125 > num_si 124, one interpret', compat(1, 0, 0, 125, sis[:1], b''))
case('compat-firstsi-truncate', 'truncateSI, no interprets, firstSI 125 > num_si 124', compat(0, 1, 0, 125, [], b''))
case('indmap-access', 'the recorded identity SetIndicatorMap, from a client that never called XkbUseExtension', im, 'xreq0')
case('indmap-short', 'an 8-byte SetIndicatorMap (xkbSetIndicatorMapReq is 12)', bytes([im[0], 14, 2, 0]) + im[4:8])
case('indmap-length', 'the identity SetIndicatorMap with 4 bytes too many (length + 1)', indmap(which, maps, extra=bytes(4)))
case('indmap-missing', 'which 0xffffffff with 31 indicator maps', indmap(which, maps[:31]))
m = [bytearray(x) for x in maps]; m[3][1] = 0x10
case('indmap-whichgroups', 'indicator 3 whichGroups 0x10 (not a group component)', indmap(which, m))
m = [bytearray(x) for x in maps]; m[5][3] = 0x20
case('indmap-whichmods', 'indicator 5 whichMods 0x20 (not a modifier component)', indmap(which, m))
m = [bytearray(x) for x in maps]; m[7][3] = 0x60; m[2][1] = 0x30
case('indmap-first-bad', 'indicator 2 whichGroups 0x30 and indicator 7 whichMods 0x60: the first is reported', indmap(which, m))
with open(os.path.join(out, 'cases.txt'), 'a') as f:
    for name, what, step in cases:
        f.write('%s\t%s\t%s\n' % (step, name, what))
EOF
}

# ---------------------------------------------------------------------------
# SetNames / SetGeometry requests (phase 4e), built from the recorded identity
# SetNames and SetGeometry (xkbcomp-requests/identity/4-SetNames.bin,
# 5-SetGeometry.bin), whose atoms mean what they mean after the probe's
# atoms:xkbcomp-requests/identity/atoms.txt step. MODE errors: one request per
# ProcXkbSetNames / _XkbSetNamesCheck / ProcXkbSetGeometry / _CheckSetGeom
# rejection (plus the few requests Xorg's odd checks accept), appended to
# DIR/cases.txt (STEP\tNAME\tDESCRIPTION; xreqa = atoms step, then xreq:).
# MODE semantics: what accepted requests do, DIR/cases.txt =
# NAME\tSTEPS\tDESCRIPTION (REQ in STEPS = the case's request).
names_geometry_requests() { # MODE SRCDIR DIR
    python3 - "$@" <<'EOF'
import os, struct, sys
mode, src, out = sys.argv[1], sys.argv[2], sys.argv[3]
os.makedirs(out, exist_ok=True)
nm = open(os.path.join(src, '4-SetNames.bin'), 'rb').read()
gm = open(os.path.join(src, '5-SetGeometry.bin'), 'rb').read()
assert nm[1] == 18 and gm[1] == 20
def u16(b, o): return b[o] | b[o+1] << 8
def u32(b, o): return struct.unpack_from('<I', b, o)[0]
def words(*atoms): return b''.join(struct.pack('<I', a) for a in atoms)
def pad(x): return x + bytes((-len(x)) % 4)
BAD = 0x7fffffff   # never a valid atom

# --- SetNames: xkbSetNamesReq deviceSpec@4 virtualMods@6 which@8 firstType@12
# nTypes@13 firstKTLevel@14 nKTLevels@15 indicators@16 groupNames@20
# nRadioGroups@21 firstKey@22 nKeys@23 nKeyAliases@24 pad@25
# totalKTLevelNames@26, then the parts in which-bit order.
H = dict(dev=u16(nm, 4), vmods=u16(nm, 6), which=u32(nm, 8), ft=nm[12], nt=nm[13],
         fkl=nm[14], nkl=nm[15], ind=u32(nm, 16), gn=nm[20], nrg=nm[21], fk=nm[22],
         nk=nm[23], nka=nm[24], tkl=u16(nm, 26))
p = 28
P = {}
comps = []
for bit in range(6):
    if H['which'] & (1 << bit):
        comps.append(u32(nm, p)); p += 4
P['comps'] = comps
P['types'] = [u32(nm, p + 4 * i) for i in range(H['nt'])]; p += 4 * H['nt']
widths = list(nm[p:p + H['nkl']]); p += (H['nkl'] + 3) & ~3
levels = []
for w in widths:
    levels.append([u32(nm, p + 4 * i) for i in range(w)]); p += 4 * w
P['widths'], P['levels'] = widths, levels
ni = bin(H['ind']).count('1'); P['ind'] = [u32(nm, p + 4 * i) for i in range(ni)]; p += 4 * ni
nv = bin(H['vmods']).count('1'); P['vmods'] = [u32(nm, p + 4 * i) for i in range(nv)]; p += 4 * nv
ng = bin(H['gn']).count('1'); P['groups'] = [u32(nm, p + 4 * i) for i in range(ng)]; p += 4 * ng
P['keys'] = nm[p:p + 4 * H['nk']]; p += 4 * H['nk']
P['aliases'] = nm[p:p + 8 * H['nka']]; p += 8 * H['nka']
assert p == len(nm), (p, len(nm))
def names(h, parts, extra=b'', length=None):
    """The request: header fields from h, then the given parts, in order."""
    body = b''
    body += words(*parts.get('comps', []))
    body += words(*parts.get('types', []))
    if 'widths' in parts:
        body += pad(bytes(parts['widths']))
        body += b''.join(words(*l) for l in parts['levels'])
    for k in ('ind', 'vmods', 'groups'):
        body += words(*parts.get(k, []))
    body += parts.get('keys', b'') + parts.get('aliases', b'') + words(*parts.get('rg', []))
    body += extra
    g = lambda k: h.get(k, 0)
    hdr = struct.pack('<HHIBBBBIBBBBBxH', g('dev') or 0x100, g('vmods'), g('which'), g('ft'), g('nt'),
                      g('fkl'), g('nkl'), g('ind'), g('gn'), g('nrg'), g('fk'), g('nk'), g('nka'), g('tkl'))
    req = hdr + body
    n = (len(req) + 4) // 4 if length is None else length
    return bytes([nm[0], 18, n & 0xff, n >> 8]) + req
assert names(H, P) == nm, 'rebuild SetNames'

# --- SetGeometry: xkbSetGeometryReq deviceSpec@4 nShapes@6 nSections@7 name@8
# widthMM@12 heightMM@14 nProperties@16 nColors@18 nDoodads@20 nKeyAliases@22
# baseColorNdx@24 labelColorNdx@25 pad@26, then label font, properties,
# colors (counted strings), shapes, sections, doodads, key aliases.
G = dict(dev=u16(gm, 4), nshapes=gm[6], nsections=gm[7], name=u32(gm, 8), w=u16(gm, 12),
         h=u16(gm, 14), nprops=u16(gm, 16), ncolors=u16(gm, 18), ndoodads=u16(gm, 20),
         naliases=u16(gm, 22), base=gm[24], label=gm[25])
q = 28
def cstr(at):
    n = u16(gm, at)
    return gm[at + 2:at + 2 + n], at + ((n + 2 + 3) & ~3)
font, q = cstr(q)
props = []
for _ in range(G['nprops']):
    k, q = cstr(q); v, q = cstr(q); props.append((k, v))
colors = []
for _ in range(G['ncolors']):
    c, q = cstr(q); colors.append(c)
shapes = []
for _ in range(G['nshapes']):
    s0 = q; no = gm[q + 4]; q += 8
    for _ in range(no):
        q += 4 + 4 * gm[q]
    shapes.append(bytearray(gm[s0:q]))
sections = []
for _ in range(G['nsections']):
    s0 = q; nr, nd, no = gm[q + 15], gm[q + 16], gm[q + 17]
    assert nd == 0 and no == 0, 'identity sections carry rows only'
    q += 20
    for _ in range(nr):
        q += 8 + 8 * gm[q + 4]
    sections.append(bytearray(gm[s0:q]))
doodads = []
for _ in range(G['ndoodads']):
    d0 = q; t = gm[q + 4]; q += 20
    if t == 3:
        _, q = cstr(q); _, q = cstr(q)
    if t == 5:
        _, q = cstr(q)
    doodads.append(bytearray(gm[d0:q]))
aliases = [bytearray(gm[q + 8 * i:q + 8 * i + 8]) for i in range(G['naliases'])]; q += 8 * G['naliases']
assert q == len(gm), (q, len(gm))
def counted(s): return pad(struct.pack('<H', len(s)) + s)
def geometry(g, font=font, props=props, colors=colors, shapes=shapes, sections=sections,
             doodads=doodads, aliases=aliases, extra=b'', length=None):
    body = counted(font) + b''.join(counted(k) + counted(v) for k, v in props)
    body += b''.join(counted(c) for c in colors)
    body += b''.join(bytes(s) for s in shapes) + b''.join(bytes(s) for s in sections)
    body += b''.join(bytes(d) for d in doodads) + b''.join(bytes(a) for a in aliases) + extra
    x = lambda k, d: g.get(k, d)
    hdr = struct.pack('<HBBIHHHHHHBBH', x('dev', 0x100), x('nshapes', len(shapes)),
                      x('nsections', len(sections)), x('name', 0), x('w', 0), x('h', 0),
                      x('nprops', len(props)), x('ncolors', len(colors)),
                      x('ndoodads', len(doodads)), x('naliases', len(aliases)),
                      x('base', 0), x('label', 0), 0)
    req = hdr + body
    n = (len(req) + 4) // 4 if length is None else length
    return bytes([gm[0], 20, n & 0xff, n >> 8]) + req
GH = {k: G[k] for k in ('dev', 'name', 'w', 'h', 'base', 'label')}
assert geometry(GH) == gm, 'rebuild SetGeometry'
def with_overlay(sec_ndx, name, rows):
    """Section sec_ndx with one overlay of rows [(rowUnder, [(over, under)...])]."""
    s = bytearray(sections[sec_ndx]); s[17] = 1
    s += struct.pack('<IBxxx', name, len(rows))
    for under, keys in rows:
        s += struct.pack('<BBxx', under, len(keys))
        for over, un in keys:
            s += over.ljust(4, b'\0') + un.ljust(4, b'\0')
    return [bytes(x) if i != sec_ndx else bytes(s) for i, x in enumerate(sections)]
# section 1 ('Alpha') row 0 starts with <TLDE>; it has 5 rows
assert sections[1][20 + 8:20 + 12] == b'TLDE' and sections[1][15] == 5

cases = []
def case(name, what, data, step):
    cases.append((name, what, step))
    open(os.path.join(out, name + '.bin'), 'wb').write(data)

if mode == 'errors':
    A = 'xreqa'
    case('names-access', 'the recorded identity SetNames, from a client that never called XkbUseExtension', nm, 'xreq0')
    case('names-short', 'a 24-byte SetNames (xkbSetNamesReq is 28)', bytes([nm[0], 18, 6, 0]) + nm[4:24], A)
    case('names-which', 'the identity SetNames with which | 0x4000 (not a names component)', names(dict(H, which=H['which'] | 0x4000), P), A)
    case('names-which0', 'which 0, one data word', names({}, {'comps': [0]}), A)
    case('names-which0-nodata', 'which 0, no data', names({}, {}), A)
    case('names-keycodes-only', 'which KeycodesName, just its atom (Xorg wants one more word before each unsent component)', names(dict(which=0x01), {'comps': [0xe9]}), A)
    case('names-keycodes-badatom', 'the identity SetNames with keycodes name atom 0x7fffffff', names(H, dict(P, comps=[BAD] + comps[1:])), A)
    case('names-geometry-badatom', 'the identity SetNames with geometry name atom 0x7fffffff', names(H, dict(P, comps=comps[:1] + [BAD] + comps[2:])), A)
    case('names-types-zero', 'KeyTypeNames with nTypes 0', names(dict(which=0x40, ft=4, nt=0), {'comps': [0]}), A)
    case('names-types-range', 'KeyTypeNames 4+24 past num_types 27', names(dict(which=0x40, ft=4, nt=24), {'types': P['types'] + [0]}), A)
    case('names-types-required', 'KeyTypeNames 3+1 (a required type)', names(dict(which=0x40, ft=3, nt=1), {'types': [0x8f]}), A)
    case('names-types-badatom', 'KeyTypeNames 4+2, the second atom 0x7fffffff', names(dict(which=0x40, ft=4, nt=2), {'types': [0x8f, BAD]}), A)
    case('names-types-stale', 'KeyTypeNames 4+1 named ONE_LEVEL, KTLevelNames 0+27 with no data left: BadLength keeps the errorValue', names(dict(which=0xc0, ft=4, nt=1, nkl=27), {'types': [0x86]}), A)
    case('names-ktlevels-zero', 'KTLevelNames with nKTLevels 0', names(dict(which=0x80, nkl=0), {'comps': [0]}), A)
    case('names-ktlevels-range', 'KTLevelNames 1+27 past num_types 27', names(dict(which=0x80, fkl=1, nkl=27), {'widths': [0] * 27, 'levels': [[]] * 27}), A)
    w = list(widths); assert w[5] == 2; w[5] = 3
    case('names-ktlevels-width', 'the identity SetNames with type 5 level width 3 (it has 2 levels)', names(H, dict(P, widths=w)), A)
    lv = [list(l) for l in levels]; lv[9][1] = BAD
    case('names-ktlevels-badatom', 'the identity SetNames with type 9 level 1 name atom 0x7fffffff', names(H, dict(P, levels=lv)), A)
    case('names-indicators-zero', 'IndicatorNames with indicators 0', names(dict(which=0x100, ind=0), {'comps': [0]}), A)
    case('names-indicators-badatom', 'the identity SetNames with indicator 3 name atom 0x7fffffff', names(H, dict(P, ind=P['ind'][:3] + [BAD] + P['ind'][4:])), A)
    case('names-vmods-zero', 'VirtualModNames with virtualMods 0', names(dict(which=0x800, vmods=0), {'comps': [0]}), A)
    case('names-groups-zero', 'GroupNames with groupNames 0', names(dict(which=0x1000, gn=0), {'comps': [0]}), A)
    case('names-groups-highbit', 'GroupNames 0x11 with two words (bit 4 is past the 4 groups)', names(dict(which=0x1000, gn=0x11), {'groups': [0xe8, 0]}), A)
    case('names-keys-min', 'KeyNames 7+1', names(dict(which=0x200, fk=7, nk=1), {'keys': b'ESC\0'}), A)
    case('names-keys-range', 'KeyNames 250+7 past maxKeyCode', names(dict(which=0x200, fk=250, nk=7), {'keys': bytes(28)}), A)
    case('names-keys-zero', 'KeyNames 8+0', names(dict(which=0x200, fk=8, nk=0), {'comps': [0]}), A)
    case('names-rg-zero', 'RGNames with nRadioGroups 0', names(dict(which=0x2000, nrg=0), {'comps': [0]}), A)
    case('names-rg-badatom', 'RGNames 1, atom 0x7fffffff', names(dict(which=0x2000, nrg=1), {'rg': [BAD]}), A)
    case('names-length', 'the identity SetNames with 4 bytes too many (length + 1)', names(H, P, extra=bytes(4)), A)
    case('names-truncated', 'the identity SetNames without its last key alias (nKeyAliases 73 kept)', names(H, dict(P, aliases=P['aliases'][:-8])), A)
    case('geom-access', 'the recorded identity SetGeometry, from a client that never called XkbUseExtension', gm, 'xreq0')
    case('geom-short', 'a 24-byte SetGeometry (xkbSetGeometryReq is 28)', bytes([gm[0], 20, 6, 0]) + gm[4:24], A)
    case('geom-name-badatom', 'the identity SetGeometry named 0x7fffffff', geometry(dict(GH, name=BAD)), A)
    g0 = geometry(GH)
    case('geom-font-overrun', 'label font length 0x7ff0, past the request', g0[:28] + struct.pack('<H', 0x7ff0) + g0[30:], A)
    case('geom-colors-one', 'nColors 1', geometry(dict(GH, ncolors=1)), A)
    case('geom-basecolor', 'baseColorNdx 7 > nColors 6', geometry(dict(GH, base=7)), A)
    case('geom-labelcolor', 'labelColorNdx 7 > nColors 6', geometry(dict(GH, label=7)), A)
    case('geom-samecolor', 'labelColorNdx = baseColorNdx = 1', geometry(dict(GH, base=1, label=1)), A)
    case('geom-color-dup', 'color 5 spec "black", as color 0', geometry(GH, colors=colors[:5] + [b'black']), A)
    case('geom-shapes-zero', 'nShapes 0', geometry(dict(GH, nshapes=0)), A)
    sh = [bytearray(s) for s in shapes]; sh[1][0:4] = sh[0][0:4]
    case('geom-shape-dup', 'shape 1 named as shape 0', geometry(GH, shapes=sh), A)
    sh = [bytearray(s) for s in shapes]; sh[0][0:4] = bytes(4)
    case('geom-shape-none', 'shape 0 named None', geometry(GH, shapes=sh), A)
    se = [bytearray(s) for s in sections]; se[0][0:4] = struct.pack('<I', BAD)
    case('geom-section-badatom', 'section 0 named 0x7fffffff', geometry(GH, sections=se), A)
    se = [bytearray(s) for s in sections]; se[2][0:4] = bytes(4)
    case('geom-section-none', 'section 2 named None', geometry(GH, sections=se), A)
    se = [bytearray(s) for s in sections]; se[1][20 + 8 + 8 * 3 + 6] = 15
    case('geom-key-shape', 'section 1 row 0 key 3 shapeNdx 15 = nShapes', geometry(GH, sections=se), A)
    se = [bytearray(s) for s in sections]; se[3][20 + 8 + 7] = 6
    case('geom-key-color', 'section 3 row 0 key 0 colorNdx 6 = nColors', geometry(GH, sections=se), A)
    dd = [bytearray(d) for d in doodads]; assert dd[0][4] == 2; dd[0][4] = 9
    case('geom-doodad-type', 'doodad 0 of type 9', geometry(GH, doodads=dd), A)
    dd = [bytearray(d) for d in doodads]; dd[0][0:4] = struct.pack('<I', BAD)
    case('geom-doodad-badatom', 'doodad 0 named 0x7fffffff', geometry(GH, doodads=dd), A)
    dd = [bytearray(d) for d in doodads]; dd[0][13] = 15
    case('geom-doodad-shape', 'doodad 0 (solid) shapeNdx 15', geometry(GH, doodads=dd), A)
    dd = [bytearray(d) for d in doodads]; assert dd[1][4] == 4; dd[1][13] = 6
    case('geom-indicator-color', 'doodad 1 (indicator) onColorNdx 6', geometry(GH, doodads=dd), A)
    dd = [bytearray(d) for d in doodads]; assert dd[4][4] == 3; dd[4][16] = 6
    case('geom-text-color', 'doodad 4 (text) colorNdx 6', geometry(GH, doodads=dd), A)
    case('geom-overlay-rowunder', 'section 1 overlay row 0 over row 6 (the section has 5)', geometry(GH, sections=with_overlay(1, 0xd9, [(6, [(b'FK01', b'TLDE')])])), A)
    case('geom-overlay-pastlast', 'section 1 overlay row 0 over row 5 (past the last), one key', geometry(GH, sections=with_overlay(1, 0xd9, [(5, [(b'FK01', b'TLDE')])])), A)
    case('geom-overlay-nokey', 'section 1 overlay row 0 over row 0, key under <ZZZZ> (not in row 0)', geometry(GH, sections=with_overlay(1, 0xd9, [(0, [(b'FK01', b'ZZZZ')])])), A)
    al = [bytearray(a) for a in aliases]; al[1][4:8] = bytes(4)
    case('geom-alias-empty', 'key alias 1 with an empty alias name', geometry(GH, aliases=al), A)
    case('geom-truncated', 'the identity SetGeometry cut after its first two sections (counts kept)', geometry(dict(GH, nsections=4, ndoodads=7, naliases=2), sections=sections[:2], doodads=[], aliases=[]), A)
    # accepted by Xorg's checks, whatever they look like
    case('names-compat-only', 'which CompatName, just its atom (the last component: accepted)', names(dict(which=0x20), {'comps': [0x85]}), A)
    case('geom-trailing', 'the identity SetGeometry with 8 bytes too many (no length check: accepted)', geometry(GH, extra=bytes(8)), A)
    case('geom-overlay-ok', 'section 1 overlay row 0 over row 0, key <FK01> under <TLDE>', geometry(GH, sections=with_overlay(1, 0xd9, [(0, [(b'FK01', b'TLDE')])])), A)
    with open(os.path.join(out, 'cases.txt'), 'a') as f:
        for name, what, step in cases:
            f.write('%s\t%s\t%s\n' % (step, name, what))
else:
    S = 'REQ'
    case('names-vmods-groups', 'VirtualModNames 0x0003 (Meta, Super) and GroupNames 0x03 (English (UK), Group 2): changedVirtualMods is the group mask',
         names(dict(which=0x1800, vmods=0x0003, gn=0x03), {'vmods': [0x81, 0x7f], 'groups': [0xe8, 0xc6]}), S)
    case('names-vmods', 'VirtualModNames 0x0081 (Hyper, None)',
         names(dict(which=0x0800, vmods=0x0081), {'vmods': [0x82, 0]}), S)
    case('names-levels-only', 'KTLevelNames 4+2 (Shift Level3 / Ctrl, Base / Caps) with the unused nTypes field 9: nLevelNames 9',
         names(dict(which=0x80, nt=9, fkl=4, nkl=2), {'widths': [2, 2], 'levels': [[0xb2, 0xb3], [0x89, 0x8c]]}), S)
    case('names-types', 'KeyTypeNames 4+2 swapped (PC_SUPER_LEVEL2, PC_ALT_LEVEL2)',
         names(dict(which=0x40, ft=4, nt=2), {'types': [0x90, 0x8f]}), S)
    case('names-indicators', 'IndicatorNames 0x8001: indicator 0 (Caps Lock) renamed Function, indicator 15 named Keypad; then XTEST Caps Lock',
         names(dict(which=0x100, ind=0x8001), {'ind': [0xd8, 0xdb]}), 'REQ down:66 up:66')
    case('names-aliases-rg', 'KeyAliases with nKeyAliases 0 (clears them) and RGNames 1 (Function)',
         names(dict(which=0x2400, nka=0, nrg=1), {'rg': [0xd8]}), S)
    case('names-keys', 'KeyNames 38+2 swapped (AC02, AC01)',
         names(dict(which=0x200, fk=38, nk=2), {'keys': b'AC02AC01'}), S)
    case('names-components', 'all six component names (keycodes Base, geometry None, symbols Any, phys_symbols Shift, types Caps, compat complete)',
         names(dict(which=0x3f), {'comps': [0x89, 0, 0x87, 0x8a, 0x8c, 0x85]}), S)
    case('geom-alone', 'the identity SetGeometry on its own: the geometry name None -> pc(pc105)', gm, S)
    case('geom-none', 'the identity SetGeometry named None: the name stays None', geometry(dict(GH, name=0)), S)
    case('geom-renamed', 'the identity SetGeometry after it, named Alpha', geometry(dict(GH, name=0xd9)), 'xreq:%s/geom-alone.bin REQ' % out)
    with open(os.path.join(out, 'cases.txt'), 'w') as f:
        for name, what, steps in cases:
            f.write('%s\t%s\t%s\n' % (name, steps, what))
EOF
}

gen_errors() {
    local out=$TD/xorg-xkb-setmap-errors.txt dir=$TD/xkb-setmap-errors
    rm -rf "$dir"
    setmap_error_requests "$TD/xkbcomp-requests/identity/1-SetMap.bin" "$dir"
    compat_error_requests "$TD/xkbcomp-requests/identity" "$dir"
    names_geometry_requests errors "$TD/xkbcomp-requests/identity" "$dir"
    {
        echo "# Xorg's errors for malformed XKB SetMap requests and for XKB requests without"
        echo "# XkbUseExtension (issue #171 phase 4c), for malformed SetCompatMap /"
        echo "# SetIndicatorMap requests (phase 4d) and for malformed SetNames / SetGeometry"
        echo "# requests (phase 4e): Xvfb -noreset, layout=gb"
        versions
        echo "# setxkbmap -rules evdev -model pc105 -layout gb; then one tools/xkb-mutation-probe.c run per"
        echo "# request (fresh connections each time), sending xkb-setmap-errors/NAME.bin raw:"
        echo "#   xreq:  on the actor after its XkbUseExtension; xreq0: on a connection without it;"
        echo "#   atoms:xkbcomp-requests/identity/atoms.txt first for the SetNames / SetGeometry requests"
        echo "#   (built from the recorded identity upload, whose atoms they use)."
        echo "# Each request is built by this script from xkbcomp-requests/identity/1-SetMap.bin,"
        echo "# 2-SetIndicatorMap.bin, 3-SetCompatMap.bin, 4-SetNames.bin or 5-SetGeometry.bin (the"
        echo "# '## NAME' line says how). Lines: '= error=CODE value=V major=M minor=N' (major is this"
        echo "# server's XKB opcode) or '= ok' for the few requests Xorg accepts, then the (empty)"
        echo "# XkbGetMap delta and coremodmap as usual. The accepted ones come last (they change"
        echo "# only names on this one server)."
        fresh gb
        while IFS=$'\t' read -r step name what; do
            echo "## $name: $what"
            if [ "$step" = xreqa ]; then
                "$PROBE" -d ":$DISP" "atoms:$TD/xkbcomp-requests/identity/atoms.txt" "xreq:$dir/$name.bin"
            else
                "$PROBE" -d ":$DISP" "$step:$dir/$name.bin"
            fi | sed "s|$TD/||g"
        done <"$dir/cases.txt"
        stop_x
    } >"$out"
    rm "$dir/cases.txt"
}

# ---------------------------------------------------------------------------
# SetMap's key-width resizing (phase 4c): XkbResizeKeyType when a SetMap
# changes the level count of a type keys use and doesn't resend those keys.
# Types-only requests (present=KeyTypes, no ResizeTypes/RecomputeActions)
# giving FOUR_LEVEL (index 11 after setxkbmap us,ru) five or three levels,
# its other fields as the server has them (xorg-xkb-pristine.txt): the
# us,ru keys with a FOUR_LEVEL group are two groups wide, so a grow shows
# Xorg's group relayout and a shrink its clearing.
setmap_resize_requests() { # DIR -> DIR/NAME.bin + DIR/cases.txt (NAME\tDESCRIPTION)
    python3 - "$@" <<'EOF'
import os, struct, sys
out = sys.argv[1]
os.makedirs(out, exist_ok=True)
def set_map_types(first, types):
    body = b''
    for real, vmods, levels, entries in types:
        body += struct.pack('<BBHBBBB', 0, real, vmods, levels, len(entries), 0, 0)
        for level, ereal, evmods in entries:
            body += struct.pack('<BBH', level, ereal, evmods)
    hdr = struct.pack('<HHHBBBBBBHBBHBBBBBBBBBBBBH', 0x100, 0x01, 0, 8, 255, first, len(types),
                      0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0)
    req = hdr + body
    n = (len(req) + 4) // 4
    return bytes([0, 9, n & 0xff, n >> 8]) + req
cases = [
    ('grow-four-level', 'FOUR_LEVEL (type 11) with 5 levels, its entries unchanged',
     set_map_types(11, [(0x01, 0x0004, 5, [(1, 0x01, 0), (2, 0, 0x0004), (3, 0x01, 0x0004)])])),
    ('shrink-four-level', 'FOUR_LEVEL (type 11) with 3 levels, its level-4 entry dropped',
     set_map_types(11, [(0x01, 0x0004, 3, [(1, 0x01, 0), (2, 0, 0x0004)])])),
]
with open(os.path.join(out, 'cases.txt'), 'w') as f:
    for name, what, data in cases:
        open(os.path.join(out, name + '.bin'), 'wb').write(data)
        f.write('%s\t%s\n' % (name, what))
EOF
}

gen_resize() {
    local out=$TD/xorg-xkb-setmap-resize.txt dir=$TD/xkb-setmap-resize
    rm -rf "$dir"
    setmap_resize_requests "$dir"
    {
        echo "# Xorg's key-width resizing in XKB SetMap (XkbResizeKeyType, issue #171 phase 4c):"
        echo "# Xvfb -noreset, fresh server per case, layout=us,ru option=grp:alt_shift_toggle"
        versions
        echo "# each case: setxkbmap -rules evdev -model pc105 -layout us,ru -option grp:alt_shift_toggle;"
        echo "# then tools/xkb-mutation-probe.c -x xreq:xkb-setmap-resize/NAME.bin (the actor, after its"
        echo "# XkbUseExtension). The request is built by this script (the '## case' line says what it"
        echo "# changes); output grammar as in xorg-xkbcomp-steps.txt, the state before is"
        echo "# xorg-xkb-pristine.txt's layout=us,ru case."
        while IFS=$'\t' read -r name what; do
            fresh us,ru grp:alt_shift_toggle
            echo "## case $name: $what"
            "$PROBE" -d ":$DISP" -x "xreq:$dir/$name.bin" | sed "s|$TD/||g"
            stop_x
        done <"$dir/cases.txt"
    } >"$out"
    rm "$dir/cases.txt"
}

# ---------------------------------------------------------------------------
# SetCompatMap / SetIndicatorMap semantics beyond the xkbcomp uploads (phase
# 4d): partial interpret replacement, truncation, growth, Xorg's skipping of
# the broken Any+AnyOfOrNone(all)->Private interpret, group compat maps with
# virtual modifiers, indicator maps with virtual modifiers, the wire realMods
# byte Xorg ignores, an indicator the new map lights, which=0, and an
# indicator a new map turns off. Requests built from the recorded identity
# SetCompatMap / SetIndicatorMap interprets and maps.
setcompat_requests() { # SRCDIR DIR -> DIR/NAME.bin + DIR/cases.txt (NAME\tSTEPS\tDESCRIPTION; REQ in STEPS = the case's request)
    python3 - "$@" <<'EOF'
import os, struct, sys
src, out = sys.argv[1], sys.argv[2]
os.makedirs(out, exist_ok=True)
cm = open(os.path.join(src, '3-SetCompatMap.bin'), 'rb').read()
def u16(b, o): return b[o] | b[o+1] << 8
nsi = u16(cm, 12)
sis = [cm[16 + 16 * i:32 + 16 * i] for i in range(nsi)]
def compat(recompute, truncate, groups, first, sis, gmods=b''):
    body = struct.pack('<HBBBBHHH', 0x100, 0, recompute, truncate, groups, first, len(sis), 0)
    body += b''.join(sis) + gmods
    n = (len(body) + 4) // 4
    return bytes([0, 11, n & 0xff, n >> 8]) + body
def si(sym, mods, match, vmod, flags, act):
    return struct.pack('<IBBBB', sym, mods, match, vmod, flags) + bytes(act)
def mods(real, vmods): return struct.pack('<BBH', real, real, vmods)
def indmap(maps):
    which = 0
    body = b''
    for i, (flags, wg, groups, wm, mods, real, vmods, ctrls) in sorted(maps.items()):
        which |= 1 << i
        body += struct.pack('<BBBBBBHI', flags, wg, groups, wm, mods, real, vmods, ctrls)
    body = struct.pack('<HHI', 0x100, 0, which) + body
    n = (len(body) + 4) // 4
    return bytes([0, 14, n & 0xff, n >> 8]) + body
broken = si(0, 0xff, 1, 0xff, 0, [0x86, 0, 0, 0, 0, 0, 0, 0])
cases = [
    ('si-partial', 'interprets 10+2 replaced by the identity upload\'s 20 and 21, no truncation, no recompute',
     compat(0, 0, 0, 10, sis[20:22]), ''),
    ('si-truncate', 'truncateSI at 100, no interprets, recomputeActions',
     compat(1, 1, 0, 100, []), ''),
    ('si-grow', 'one interpret appended at 124 (a: SetMods(Shift)), recomputeActions',
     compat(1, 0, 0, 124, [si(0x61, 0, 1, 0xff, 0, [1, 0, 1, 1, 0, 0, 0, 0])]), ''),
    ('si-skip-broken', 'interprets 0+3, the middle one the broken Any+AnyOfOrNone(all)->Private: skipped',
     compat(0, 0, 0, 0, [sis[0], broken, sis[2]]), ''),
    ('si-skip-truncate', 'truncateSI, interprets 120+2 = the broken one and the identity upload\'s 121',
     compat(0, 1, 0, 120, [broken, sis[121]]), ''),
    ('groups-vmods', 'group compat 1 = LevelThree (vmod 2), 2 = Shift, no interprets',
     compat(0, 0, 0x03, 0, [], mods(0, 0x0004) + mods(0x01, 0)), ''),
    ('indmap-vmods', 'indicator 3 locked Alt (vmod 1) and indicator 1 base Shift+NumLock (vmod 0)',
     indmap({3: (0, 0, 0, 0x04, 0, 0, 0x0002, 0), 1: (0, 0, 0, 0x01, 0x01, 0x01, 0x0001, 0)}), ''),
    ('indmap-realmods', 'indicator 4 whichMods locked, mods 0x01 but realMods 0x04 on the wire',
     indmap({4: (0, 0, 0, 0x04, 0x01, 0x04, 0, 0)}), ''),
    ('indmap-lights', 'indicator 20 (no name) lit by effective group 1: the new map lights it',
     indmap({20: (0, 0x08, 0x01, 0, 0, 0, 0, 0)}), ''),
    ('indmap-which0', 'which 0 with one map and a length that doesn\'t count it: Success, no-op',
     bytes([0, 14, 6, 0]) + struct.pack('<HHI', 0x100, 0, 0) + struct.pack('<BBBBBBHI', 0, 0x08, 1, 0, 0, 0, 0, 0), ''),
    ('indmap-caps-off', 'Caps Lock locked (XTEST 66), then the compat upload\'s SetIndicatorMap (Caps Lock LED = locked Shift)',
     open(os.path.join(src, '../compat/2-SetIndicatorMap.bin'), 'rb').read(), 'down:66 up:66 REQ'),
    ('indmap-none-in-use', 'Caps Lock locked (XTEST 66), then every indicator map cleared: no map in use',
     indmap({i: (0, 0, 0, 0, 0, 0, 0, 0) for i in range(32)}), 'down:66 up:66 REQ'),
    ('groups-vmods-smm', 'group compat 1 = LevelThree (vmod 2), then SetModifierMapping moves <LVL3> (92) from Mod5 to Mod4',
     compat(0, 0, 0x01, 0, [], mods(0, 0x0004)), 'REQ smmx:-92@7,+92@6'),
]
with open(os.path.join(out, 'cases.txt'), 'w') as f:
    for name, what, data, pre in cases:
        open(os.path.join(out, name + '.bin'), 'wb').write(data)
        f.write('%s\t%s\t%s\n' % (name, pre or 'REQ', what))
EOF
}

gen_setcompat() {
    local out=$TD/xorg-xkb-setcompat.txt dir=$TD/xkb-setcompat
    rm -rf "$dir"
    setcompat_requests "$TD/xkbcomp-requests/identity" "$dir"
    {
        echo "# Xorg's XKB SetCompatMap / SetIndicatorMap semantics (issue #171 phase 4d):"
        echo "# Xvfb -noreset, fresh server per case, layout=gb"
        versions
        echo "# each case: setxkbmap -rules evdev -model pc105 -layout gb; then tools/xkb-mutation-probe.c"
        echo "# -x with the case's steps: xreq:xkb-setcompat/NAME.bin (the actor, after its XkbUseExtension),"
        echo "# for some cases after XTEST down:KC up:KC or before an smmx: SetModifierMapping."
        echo "# The request is built by this script from the recorded identity upload's interprets (the"
        echo "# '## case' line says what it does); output grammar as in xorg-xkbcomp-steps.txt, the state"
        echo "# before is xorg-xkb-pristine.txt's layout=gb case. In raw event hex the sequence number"
        echo "# (bytes 2-3) and time (4-7) differ between runs, and CompatMapNotify bytes 16-31 are"
        echo "# uninitialised stack in Xorg (differ too)."
        while IFS=$'\t' read -r name steps what; do
            fresh gb
            echo "## case $name: $what"
            # shellcheck disable=SC2086
            "$PROBE" -d ":$DISP" -x ${steps//REQ/xreq:$dir/$name.bin} | sed "s|$TD/||g"
            stop_x
        done <"$dir/cases.txt"
    } >"$out"
    rm "$dir/cases.txt"
}

gen_setnames() {
    local out=$TD/xorg-xkb-setnames.txt dir=$TD/xkb-setnames
    rm -rf "$dir"
    names_geometry_requests semantics "$TD/xkbcomp-requests/identity" "$dir"
    {
        echo "# Xorg's XKB SetNames / SetGeometry semantics (issue #171 phase 4e):"
        echo "# Xvfb -noreset, fresh server per case, layout=gb"
        versions
        echo "# each case: setxkbmap -rules evdev -model pc105 -layout gb; then tools/xkb-mutation-probe.c"
        echo "# -x atoms:xkbcomp-requests/identity/atoms.txt, then the case's steps: xreq:xkb-setnames/NAME.bin"
        echo "# (the actor, after its XkbUseExtension), for one case followed by XTEST down:KC up:KC."
        echo "# The request is built by this script from the recorded identity upload's SetNames /"
        echo "# SetGeometry and atoms (the '## case' line says what it does); output grammar as in"
        echo "# xorg-xkbcomp-steps.txt, the state before is xorg-xkb-pristine.txt's layout=gb case. In raw"
        echo "# event hex the sequence number (bytes 2-3) and time (4-7) differ between runs, and"
        echo "# NewKeyboardNotify bytes 18-31 are uninitialised stack in Xorg (differ too)."
        while IFS=$'\t' read -r name steps what; do
            fresh gb
            echo "## case $name: $what"
            # shellcheck disable=SC2086
            "$PROBE" -d ":$DISP" -x "atoms:$TD/xkbcomp-requests/identity/atoms.txt" \
                ${steps//REQ/xreq:$dir/$name.bin} | sed "s|$TD/||g"
            stop_x
        done <"$dir/cases.txt"
    } >"$out"
    rm "$dir/cases.txt"
}

targets=("$@")
[ ${#targets[@]} -eq 0 ] && targets=(ckm smm xkbcomp repeat pristine steps errors resize setcompat setnames)
for t in "${targets[@]}"; do
    "gen_$t"
done
