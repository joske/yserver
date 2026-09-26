/* xkb-mutation-probe — what does a runtime core keymap mutation do to the
 *                      server's XKB keymap, and which events does it send?
 *
 * Issue #171 (runtime keymap mutation must reach XKB). Captures Xorg ground
 * truth for ChangeKeyboardMapping / SetModifierMapping against Xvfb.
 *
 * Three connections to the same display:
 *
 *   XKB LISTENER  XkbUseExtension + XkbSelectEvents(all events, all details)
 *                 on the core keyboard. Records every event it receives
 *                 (XKB events and any core MappingNotify), in arrival order.
 *                 Takes the XkbGetMap snapshots (full=XkbAllMapComponentsMask,
 *                 all keys) and the core GetModifierMapping readback.
 *   CORE LISTENER a plain core client (never calls XkbUseExtension). Xorg
 *                 filters core MappingNotify for XKB-initialised clients, so
 *                 this is where the core MappingNotify is observed.
 *   ACTOR         issues the mutation requests (raw ChangeKeyboardMapping,
 *                 SetModifierMapping, XTEST key down/up) and nothing else.
 *
 * After each step: round trip on the actor, then on both listeners, then
 * drain their event queues (arrival order preserved per connection), then a
 * new GetMap snapshot diffed against the one taken before the step.
 *
 * Usage:
 *   xkb-mutation-probe [-d DISPLAY] [-x] STEP [STEP ...]
 * -x (extended) also snapshots GetCompatMap / GetIndicatorMap / GetNames /
 * the GetGeometry header after every step and prints their delta (see
 * "Extended output" below).
 * steps (one argv word each, fields ':'-separated, keysyms hex):
 *   ckm:FIRST:KPK:COUNT:SYM,SYM,...   raw ChangeKeyboardMapping (nsyms =
 *                                     number of syms listed, may be empty)
 *   smm:KPM:KC,KC,...                 SetModifierMapping, 8*KPM keycodes
 *                                     (decimal); reply status recorded
 *   smmx:EDIT,EDIT,...                SetModifierMapping built from the current
 *                                     GetModifierMapping with edits: same |
 *                                     -KC@MOD | +KC@MOD | kpm=N (see smmx());
 *                                     prints "sent kpm=K keys=..." first
 *   down:KC / up:KC                   XTEST FakeInput KeyPress / KeyRelease
 *   run:SHELL COMMAND                 run an external client (e.g. xkbcomp
 *                                     upload) as the actor; exit status recorded
 *   atoms:FILE                        the actor interns every "0xATOM NAME"
 *                                     line of FILE in order and dies unless
 *                                     each gets the recorded value (so a
 *                                     replayed request's atoms mean the same)
 *   xreq:FILE                         the actor (after its XkbUseExtension)
 *                                     sends the one request in binary FILE,
 *                                     byte-exact but for the XKB major opcode
 *   full                              (-x) print the whole current state as
 *                                     '= ' lines, no step taken
 *   total                             diff the current map against the map
 *                                     taken before the first step
 *
 * Output (line oriented):
 *   > STEP                      the step as given
 *   = ok | = error=E value=V | = status=S   request result
 *   e xkb NAME k=v ... raw=HEX  XKB event on the XKB listener (32 bytes)
 *   e xkbl core ... raw=HEX     core event on the XKB listener
 *   e core NAME k=v ... raw=HEX core event on the core listener
 *   - KC <key>                  key KC before the step (only keys that differ)
 *   + KC <key>                  key KC after the step
 *       <key> = kt=T,T.. gi=0xGG w=W syms=S,S.. acts=HEX16,.. beh=TT:DD
 *               expl=0xEE mm=0xMM vmm=0xVVVV   (syms hex, acts 8 raw bytes)
 *   -type N <type> / +type N <type>   key type N before/after (if changed)
 *       <type> = mods=M/V lv=L map=[act:mask/vmods->level ...] pre=[...]
 *   -vmods / +vmods M0,M1..M15        virtual mod real-mod table if changed
 *   ntypes A->B / keys A..B->C..D     header fields if changed
 *   enabledControls A->B              XkbGetControls enabled mask if changed
 *   repeat KC A->B                    XkbGetControls per-key repeat bit changed
 *   coremodmap kpm=K 0:k,k.. 1:.. ... 7:..   GetModifierMapping after step
 *
 * Extended output (-x): the rest of the description as keyed lines
 * "KIND ID VALUE..." (key = first two words), atoms by name ('None' = 0),
 * printed as "-LINE" / "+LINE" when a key's line changed, appeared or went:
 *   controls - numGroups= groupsWrap=             GetControls (the rest is in the base lines)
 *   compat - groups= firstSI= nSI= nTotalSI=      GetCompatMap(getAllSI, groups 0x0f)
 *   si N sym= mods= match= vmod= flags= act=16hex one sym interpret
 *   groupcompat G mask= real= vmods=
 *   indicators - which= realIndicators= nIndicators=   GetIndicatorMap(all)
 *   indmap N flags= whichGroups= groups= whichMods= mods= realMods= vmods= ctrls=
 *   names - which= min= max= nTypes= ...           GetNames(all) header
 *   name keycodes|geometry|symbols|phys_symbols|types|compat NAME
 *   typename N NAME / levelnames N n=L [NAME ...]  ('?' = a slot Xorg has
 *                                                  not initialised, see take_xsnap)
 *   indname N / vmodname N / groupname G NAME, keyname KC 'NAME',
 *   alias N 'ALIAS'->'REAL', rgname N NAME
 *   geometry - name= found= widthMM= heightMM= nProperties= ... length=
 *
 * Build:
 *   cc -O1 -Wall -o xkb-mutation-probe tools/xkb-mutation-probe.c \
 *      $(pkg-config --cflags --libs xcb xcb-xkb xcb-xtest)
 *
 * Rebuilding the #171 goldens (crates/yserver/src/kms/testdata/
 * xorg-xkb-change-keyboard-mapping.txt, xorg-xkb-set-modifier-mapping.txt,
 * xorg-xkbcomp-upload-trace.txt):
 *
 *   tools/xkb-mutation-goldens.sh            # all of them
 *   tools/xkb-mutation-goldens.sh smm        # or ckm / smm / xkbcomp / repeat /
 *                                            # pristine / steps (phase 4: -x,
 *                                            # atoms:, xreq:, full)
 *
 * It builds this probe, and for each case starts a fresh `Xvfb :92 -noreset`,
 * runs `setxkbmap -rules evdev -model pc105 -layout L [-option O]`, then one
 * probe invocation with the case's steps (e.g. `-d :92 ckm:10:1:1:78`).
 * The file headers record the case set and the xorg/xkeyboard-config versions.
 * Never hand-edit the goldens; rerun the script. In raw event hex the
 * sequence number (bytes 2-3) and timestamp (bytes 4-7) differ between runs.
 */
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <xcb/xcb.h>
#include <xcb/xcbext.h>
#include <xcb/xkb.h>
#include <xcb/xtest.h>

#define DEV_CORE XCB_XKB_ID_USE_CORE_KBD
#define ALL_MAP 0xff
#define ALL_EVENTS 0x0fff

static xcb_connection_t *xl, *cl, *ac;
static uint8_t xkb_event_base;

static void die(const char *m)
{
    fprintf(stderr, "xkb-mutation-probe: %s\n", m);
    exit(1);
}

/* ---- snapshot ---------------------------------------------------------- */

struct snap {
    int min, max, ntypes;
    char *types[256];
    char *keys[256];
    uint8_t vmods[16];
    uint32_t enabled;
    uint8_t repeat[32];
};

static char *xstrdup(const char *s)
{
    char *d = strdup(s);
    if (!d)
        die("oom");
    return d;
}

static void snap_free(struct snap *s)
{
    for (int i = 0; i < 256; i++) {
        free(s->types[i]);
        free(s->keys[i]);
    }
    memset(s, 0, sizeof *s);
}

static void take_snap(struct snap *s)
{
    memset(s, 0, sizeof *s);
    xcb_xkb_get_map_cookie_t ck = xcb_xkb_get_map(
        xl, DEV_CORE, ALL_MAP, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0);
    xcb_generic_error_t *err = NULL;
    xcb_xkb_get_map_reply_t *r = xcb_xkb_get_map_reply(xl, ck, &err);
    if (!r)
        die("GetMap failed");
    const uint8_t *b = (const uint8_t *)r;
    s->min = b[10];
    s->max = b[11];
    unsigned present = b[12] | b[13] << 8;
    int nTypes = b[15];
    int firstKeySym = b[17], nKeySyms = b[20];
    int firstKeyAct = b[21], nKeyActs = b[24];
    int totalBeh = b[27];
    int totalExpl = b[30];
    int totalMM = b[33];
    int totalVMM = b[36];
    unsigned vmodsMask = b[38] | b[39] << 8;
    const uint8_t *p = b + 40;
    char buf[8192], *q;

    s->ntypes = nTypes;
    if (present & 0x01) {
        for (int t = 0; t < nTypes; t++) {
            int nent = p[5], pres = p[6];
            q = buf;
            q += sprintf(q, "mods=%02x/%04x lv=%d map=[", p[1], p[2] | p[3] << 8, p[4]);
            const uint8_t *e = p + 8;
            for (int i = 0; i < nent; i++, e += 8)
                q += sprintf(q, "%s%d:%02x/%04x->%d", i ? " " : "", e[0], e[3],
                             e[4] | e[5] << 8, e[2]);
            q += sprintf(q, "]");
            if (pres) {
                q += sprintf(q, " pre=[");
                for (int i = 0; i < nent; i++, e += 4)
                    q += sprintf(q, "%s%02x/%04x", i ? " " : "", e[1], e[2] | e[3] << 8);
                q += sprintf(q, "]");
            }
            s->types[t] = xstrdup(buf);
            p = e;
        }
    }
    /* per-key accumulators */
    static char syms[256][2048];
    static char acts[256][2048];
    static int beh[256], expl[256], mm[256], vmm[256];
    memset(syms, 0, sizeof syms);
    memset(acts, 0, sizeof acts);
    memset(beh, 0, sizeof beh);
    memset(expl, 0, sizeof expl);
    memset(mm, 0, sizeof mm);
    memset(vmm, 0, sizeof vmm);
    if (present & 0x02) {
        for (int k = firstKeySym; k < firstKeySym + nKeySyms; k++) {
            int n = p[6] | p[7] << 8;
            q = syms[k];
            q += sprintf(q, "kt=%d,%d,%d,%d gi=0x%02x w=%d syms=", p[0], p[1], p[2], p[3],
                         p[4], p[5]);
            const uint8_t *sy = p + 8;
            for (int i = 0; i < n; i++, sy += 4) {
                uint32_t v = sy[0] | sy[1] << 8 | sy[2] << 16 | (uint32_t)sy[3] << 24;
                q += sprintf(q, "%s%x", i ? "," : "", v);
            }
            if (!n)
                q += sprintf(q, "-");
            p = sy;
        }
    }
    if (present & 0x10) {
        const uint8_t *cnt = p;
        p += (nKeyActs + 3) & ~3;
        for (int k = firstKeyAct; k < firstKeyAct + nKeyActs; k++) {
            int n = cnt[k - firstKeyAct];
            q = acts[k];
            if (!n)
                q += sprintf(q, "-");
            for (int i = 0; i < n; i++, p += 8)
                q += sprintf(q, "%s%02x%02x%02x%02x%02x%02x%02x%02x", i ? "," : "", p[0], p[1],
                             p[2], p[3], p[4], p[5], p[6], p[7]);
        }
    }
    if (present & 0x20) {
        for (int i = 0; i < totalBeh; i++, p += 4)
            beh[p[0]] = p[1] << 8 | p[2];
    }
    if (present & 0x40) {
        int j = 0;
        for (int i = 0; i < 16; i++)
            if (vmodsMask & (1u << i))
                s->vmods[i] = p[j++];
        p += (j + 3) & ~3;
    }
    if (present & 0x08) {
        for (int i = 0; i < totalExpl; i++)
            expl[p[2 * i]] = p[2 * i + 1];
        p += (2 * totalExpl + 3) & ~3;
    }
    if (present & 0x04) {
        for (int i = 0; i < totalMM; i++)
            mm[p[2 * i]] = p[2 * i + 1];
        p += (2 * totalMM + 3) & ~3;
    }
    if (present & 0x80) {
        for (int i = 0; i < totalVMM; i++, p += 4)
            vmm[p[0]] = p[2] | p[3] << 8;
    }
    /* the parse must consume the reply exactly, or a layout assumption is wrong */
    if (p - b != 32 + 4 * (long)r->length)
        die("GetMap reply parse did not end at the reply length");
    for (int k = s->min; k <= s->max; k++) {
        snprintf(buf, sizeof buf, "%s acts=%s beh=%02x:%02x expl=0x%02x mm=0x%02x vmm=0x%04x",
                 syms[k][0] ? syms[k] : "nosyms", acts[k][0] ? acts[k] : "-", beh[k] >> 8,
                 beh[k] & 0xff, expl[k], mm[k], vmm[k]);
        s->keys[k] = xstrdup(buf);
    }
    free(r);

    xcb_xkb_get_controls_reply_t *c =
        xcb_xkb_get_controls_reply(xl, xcb_xkb_get_controls(xl, DEV_CORE), NULL);
    if (!c)
        die("GetControls failed");
    s->enabled = c->enabledControls;
    memcpy(s->repeat, c->perKeyRepeat, 32);
    free(c);
}

static void diff_snap(const struct snap *a, const struct snap *b)
{
    if (a->ntypes != b->ntypes)
        printf("ntypes %d->%d\n", a->ntypes, b->ntypes);
    if (a->min != b->min || a->max != b->max)
        printf("keys %d..%d->%d..%d\n", a->min, a->max, b->min, b->max);
    int nt = a->ntypes > b->ntypes ? a->ntypes : b->ntypes;
    for (int t = 0; t < nt; t++) {
        const char *x = a->types[t], *y = b->types[t];
        if (x && y && !strcmp(x, y))
            continue;
        if (x)
            printf("-type %d %s\n", t, x);
        if (y)
            printf("+type %d %s\n", t, y);
    }
    if (memcmp(a->vmods, b->vmods, 16)) {
        for (int pass = 0; pass < 2; pass++) {
            const uint8_t *v = pass ? b->vmods : a->vmods;
            printf("%cvmods ", pass ? '+' : '-');
            for (int i = 0; i < 16; i++)
                printf("%s%02x", i ? "," : "", v[i]);
            printf("\n");
        }
    }
    if (a->enabled != b->enabled)
        printf("enabledControls 0x%08x->0x%08x\n", a->enabled, b->enabled);
    for (int k = 0; k < 256; k++) {
        int x = a->repeat[k >> 3] >> (k & 7) & 1, y = b->repeat[k >> 3] >> (k & 7) & 1;
        if (x != y)
            printf("repeat %d %d->%d\n", k, x, y);
    }
    for (int k = 0; k < 256; k++) {
        const char *x = a->keys[k], *y = b->keys[k];
        if ((x && y && !strcmp(x, y)) || (!x && !y))
            continue;
        if (x)
            printf("- %d %s\n", k, x);
        if (y)
            printf("+ %d %s\n", k, y);
    }
}

static void core_modmap(void)
{
    xcb_get_modifier_mapping_reply_t *r =
        xcb_get_modifier_mapping_reply(xl, xcb_get_modifier_mapping(xl), NULL);
    if (!r)
        die("GetModifierMapping failed");
    int kpm = r->keycodes_per_modifier;
    xcb_keycode_t *k = xcb_get_modifier_mapping_keycodes(r);
    printf("coremodmap kpm=%d", kpm);
    for (int m = 0; m < 8; m++) {
        printf(" %d:", m);
        for (int i = 0; i < kpm; i++)
            printf("%s%d", i ? "," : "", k[m * kpm + i]);
    }
    printf("\n");
    free(r);
}

/* ---- extended state (-x): compat map, names, indicator maps, geometry --- */

/* The rest of the XKB description, as keyed lines "KIND ID VALUE..." (the
 * key is the first two words). Atoms are printed by name, so the lines
 * compare across servers. */
struct xsnap {
    int n;
    char **line;
    int valid;          /* taken (nlevels meaningful) */
    int ntypes;
    int nlevels[256];   /* GetNames level count per type */
};

static int extended; /* -x: also snapshot/diff the xsnap parts */
static uint8_t xkb_major;

static unsigned u16c(const uint8_t *e, int o) { return e[o] | e[o + 1] << 8; }
static unsigned u32c(const uint8_t *e, int o)
{
    return e[o] | e[o + 1] << 8 | e[o + 2] << 16 | (unsigned)e[o + 3] << 24;
}
static void report_error(xcb_generic_error_t *err);

static void xs_add(struct xsnap *s, const char *fmt, ...)
    __attribute__((format(printf, 2, 3)));
static void xs_add(struct xsnap *s, const char *fmt, ...)
{
    char buf[8192];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(buf, sizeof buf, fmt, ap);
    va_end(ap);
    s->line = realloc(s->line, (s->n + 1) * sizeof *s->line);
    if (!s->line)
        die("oom");
    s->line[s->n++] = xstrdup(buf);
}

static void xs_free(struct xsnap *s)
{
    for (int i = 0; i < s->n; i++)
        free(s->line[i]);
    free(s->line);
    memset(s, 0, sizeof *s);
}

/* atom -> name, cached per run (atoms are never freed while the server runs) */
static struct {
    uint32_t atom;
    char *name;
} atom_cache[4096];
static int n_atom_cache;

static const char *atom_name(uint32_t a)
{
    if (!a)
        return "None";
    for (int i = 0; i < n_atom_cache; i++)
        if (atom_cache[i].atom == a)
            return atom_cache[i].name;
    xcb_get_atom_name_reply_t *r = xcb_get_atom_name_reply(xl, xcb_get_atom_name(xl, a), NULL);
    char buf[512];
    if (r) {
        int len = xcb_get_atom_name_name_length(r);
        snprintf(buf, sizeof buf, "'%.*s'", len, xcb_get_atom_name_name(r));
        free(r);
    } else
        snprintf(buf, sizeof buf, "BadAtom(0x%x)", a);
    if (n_atom_cache == 4096)
        die("atom cache full");
    atom_cache[n_atom_cache].atom = a;
    atom_cache[n_atom_cache].name = xstrdup(buf);
    return atom_cache[n_atom_cache++].name;
}

/* Level names Xorg has not initialised, printed as '?': XkbResizeKeyType
 * reallocarray()s a type's level_names when SetMap gives it more levels
 * than it had (a type index SetMap adds starts with none) and never fills
 * the new slots, so until SetNames writes that type's level names they are
 * uninitialised memory that differs run to run (seen: None, 'PRIMARY',
 * BadAtom). Tracked for the xreq: steps (the probe knows their requests);
 * a run: step is assumed to finish with SetNames, as xkbcomp does. */
static unsigned char level_uninit[256][256];

static void take_xsnap(struct xsnap *s)
{
    memset(s, 0, sizeof *s);
    s->valid = 1;
    const uint8_t *b, *p;

    /* GetControls fields the base snapshot does not print */
    xcb_xkb_get_controls_reply_t *ct =
        xcb_xkb_get_controls_reply(xl, xcb_xkb_get_controls(xl, DEV_CORE), NULL);
    if (!ct)
        die("GetControls failed");
    xs_add(s, "controls - numGroups=%d groupsWrap=0x%02x", ct->numGroups, ct->groupsWrap);
    free(ct);

    /* GetCompatMap: all interprets, all four groups */
    xcb_xkb_get_compat_map_reply_t *c = xcb_xkb_get_compat_map_reply(
        xl, xcb_xkb_get_compat_map(xl, DEV_CORE, 0x0f, 1, 0, 0), NULL);
    if (!c)
        die("GetCompatMap failed");
    b = (const uint8_t *)c;
    int groups = b[8], nsi = u16c(b, 12);
    xs_add(s, "compat - groups=0x%02x firstSI=%d nSI=%d nTotalSI=%d", groups, u16c(b, 10), nsi,
           u16c(b, 14));
    p = b + 32;
    for (int i = 0; i < nsi; i++, p += 16)
        xs_add(s, "si %d sym=%x mods=%02x match=%02x vmod=%d flags=%02x act=%02x%02x%02x%02x%02x%02x%02x%02x",
               i, u32c(p, 0), p[4], p[5], p[6], p[7], p[8], p[9], p[10], p[11], p[12], p[13],
               p[14], p[15]);
    for (int g = 0; g < 4; g++)
        if (groups & (1 << g)) {
            xs_add(s, "groupcompat %d mask=%02x real=%02x vmods=%04x", g + 1, p[0], p[1],
                   u16c(p, 2));
            p += 4;
        }
    if (p - b != 32 + 4 * (long)c->length)
        die("GetCompatMap reply parse did not end at the reply length");
    free(c);

    /* GetIndicatorMap: all 32 */
    xcb_xkb_get_indicator_map_reply_t *im = xcb_xkb_get_indicator_map_reply(
        xl, xcb_xkb_get_indicator_map(xl, DEV_CORE, 0xffffffff), NULL);
    if (!im)
        die("GetIndicatorMap failed");
    b = (const uint8_t *)im;
    uint32_t which = u32c(b, 8);
    xs_add(s, "indicators - which=0x%08x realIndicators=0x%08x nIndicators=%d", which, u32c(b, 12),
           b[16]);
    p = b + 32;
    for (int i = 0; i < 32; i++)
        if (which & (1u << i)) {
            xs_add(s, "indmap %d flags=%02x whichGroups=%02x groups=%02x whichMods=%02x mods=%02x "
                      "realMods=%02x vmods=%04x ctrls=%08x",
                   i, p[0], p[1], p[2], p[3], p[4], p[5], u16c(p, 6), u32c(p, 8));
            p += 12;
        }
    if (p - b != 32 + 4 * (long)im->length)
        die("GetIndicatorMap reply parse did not end at the reply length");
    free(im);

    /* GetNames: everything */
    xcb_xkb_get_names_reply_t *nm =
        xcb_xkb_get_names_reply(xl, xcb_xkb_get_names(xl, DEV_CORE, 0x3fff), NULL);
    if (!nm)
        die("GetNames failed");
    b = (const uint8_t *)nm;
    uint32_t nw = u32c(b, 8);
    int nTypes = b[14], groupNames = b[15], vmods = u16c(b, 16), firstKey = b[18], nKeys = b[19];
    uint32_t inds = u32c(b, 20);
    int nRG = b[24], nAliases = b[25], nKTLevels = u16c(b, 26);
    xs_add(s, "names - which=0x%04x min=%d max=%d nTypes=%d groupNames=0x%02x virtualMods=0x%04x "
              "keys=%d+%d indicators=0x%08x nRadioGroups=%d nKeyAliases=%d nKTLevels=%d",
           nw, b[12], b[13], nTypes, groupNames, vmods, firstKey, nKeys, inds, nRG, nAliases,
           nKTLevels);
    p = b + 32;
    static const char *const comp[6] = {"keycodes", "geometry", "symbols",
                                        "phys_symbols", "types", "compat"};
    for (int i = 0; i < 6; i++)
        if (nw & (1u << i)) {
            xs_add(s, "name %s %s", comp[i], atom_name(u32c(p, 0)));
            p += 4;
        }
    if (nw & 0x40)
        for (int t = 0; t < nTypes; t++, p += 4)
            xs_add(s, "typename %d %s", t, atom_name(u32c(p, 0)));
    if (nw & 0x80) {
        const uint8_t *nl = p;
        p += (nTypes + 3) & ~3;
        s->ntypes = nTypes;
        for (int t = 0; t < nTypes; t++) {
            char buf[4096], *q = buf;
            s->nlevels[t] = nl[t];
            q += sprintf(q, "n=%d [", nl[t]);
            for (int l = 0; l < nl[t]; l++, p += 4)
                q += snprintf(q, sizeof buf - (q - buf), "%s%s", l ? " " : "",
                              level_uninit[t][l] ? "?" : atom_name(u32c(p, 0)));
            sprintf(q, "]");
            xs_add(s, "levelnames %d %s", t, buf);
        }
    }
    if (nw & 0x100)
        for (int i = 0; i < 32; i++)
            if (inds & (1u << i)) {
                xs_add(s, "indname %d %s", i, atom_name(u32c(p, 0)));
                p += 4;
            }
    if (nw & 0x200)
        for (int i = 0; i < 16; i++)
            if (vmods & (1 << i)) {
                xs_add(s, "vmodname %d %s", i, atom_name(u32c(p, 0)));
                p += 4;
            }
    if (nw & 0x400)
        for (int i = 0; i < 4; i++)
            if (groupNames & (1 << i)) {
                xs_add(s, "groupname %d %s", i + 1, atom_name(u32c(p, 0)));
                p += 4;
            }
    if (nw & 0x800)
        for (int k = 0; k < nKeys; k++, p += 4)
            xs_add(s, "keyname %d '%.4s'", firstKey + k, (const char *)p);
    if (nw & 0x1000)
        for (int a = 0; a < nAliases; a++, p += 8)
            xs_add(s, "alias %d '%.4s'->'%.4s'", a, (const char *)p + 4, (const char *)p);
    if (nw & 0x2000)
        for (int r = 0; r < nRG; r++, p += 4)
            xs_add(s, "rgname %d %s", r, atom_name(u32c(p, 0)));
    if (p - b != 32 + 4 * (long)nm->length)
        die("GetNames reply parse did not end at the reply length");
    free(nm);

    /* GetGeometry: header only (what SetGeometry replaced). xcb-xkb has no
     * GetGeometry, so it goes out raw: deviceSpec, pad, name=None */
    uint8_t greq[12] = {xkb_major, 19, 3, 0, DEV_CORE & 0xff, DEV_CORE >> 8, 0, 0, 0, 0, 0, 0};
    struct iovec gv[3];
    gv[2].iov_base = greq;
    gv[2].iov_len = sizeof greq;
    xcb_protocol_request_t greqd = {.count = 1, .ext = NULL, .opcode = 19, .isvoid = 0};
    unsigned gseq = xcb_send_request(xl, XCB_REQUEST_CHECKED | XCB_REQUEST_RAW, gv + 2, &greqd);
    xcb_generic_error_t *gerr = NULL;
    uint8_t *g = xcb_wait_for_reply(xl, gseq, &gerr);
    if (!g)
        die("GetGeometry failed");
    b = g;
    xs_add(s, "geometry - name=%s found=%d widthMM=%d heightMM=%d nProperties=%d nColors=%d "
              "nShapes=%d nSections=%d nDoodads=%d nKeyAliases=%d length=%u",
           atom_name(u32c(b, 8)), b[12], u16c(b, 14), u16c(b, 16), u16c(b, 18), u16c(b, 20),
           u16c(b, 22), u16c(b, 24), u16c(b, 26), u16c(b, 28), u32c(b, 4));
    free(g);
}

/* key of a line = its first two words */
static int xs_keylen(const char *l)
{
    const char *sp = strchr(l, ' ');
    if (!sp)
        return strlen(l);
    sp = strchr(sp + 1, ' ');
    return sp ? sp - l : (int)strlen(l);
}

static int xs_find(const struct xsnap *s, const char *l)
{
    int k = xs_keylen(l);
    for (int i = 0; i < s->n; i++)
        if (xs_keylen(s->line[i]) == k && !strncmp(s->line[i], l, k))
            return i;
    return -1;
}

static void diff_xsnap(const struct xsnap *a, const struct xsnap *b)
{
    for (int i = 0; i < a->n; i++) {
        int j = xs_find(b, a->line[i]);
        if (j < 0)
            printf("-%s\n", a->line[i]);
        else if (strcmp(a->line[i], b->line[j])) {
            printf("-%s\n", a->line[i]);
            printf("+%s\n", b->line[j]);
        }
    }
    for (int j = 0; j < b->n; j++)
        if (xs_find(a, b->line[j]) < 0)
            printf("+%s\n", b->line[j]);
}

/* the whole current state: GetMap, GetControls, core modmap, and the xsnap */
static void print_full(const struct snap *s, const struct xsnap *x)
{
    printf("= keys %d..%d ntypes %d enabledControls 0x%08x\n", s->min, s->max, s->ntypes,
           s->enabled);
    for (int t = 0; t < s->ntypes; t++)
        printf("= type %d %s\n", t, s->types[t] ? s->types[t] : "-");
    printf("= vmods ");
    for (int i = 0; i < 16; i++)
        printf("%s%02x", i ? "," : "", s->vmods[i]);
    printf("\n= repeat ");
    for (int i = 0; i < 32; i++)
        printf("%02x", s->repeat[i]);
    printf("\n");
    for (int k = s->min; k <= s->max; k++)
        printf("= key %d %s\n", k, s->keys[k]);
    for (int i = 0; i < x->n; i++)
        printf("= %s\n", x->line[i]);
}

/* ---- raw XKB requests (xkbcomp replay) --------------------------------- */

static int actor_xkb_ready;

/* intern every "0xVALUE NAME" line of PATH on the actor, in order, and die
 * unless each atom gets the value recorded: a replayed request carries the
 * recording server's atoms, so they must mean the same names here */
static void intern_atoms(const char *path)
{
    FILE *f = fopen(path, "r");
    if (!f)
        die("atoms: cannot open file");
    char line[1024];
    int n = 0;
    while (fgets(line, sizeof line, f)) {
        char *nl = strchr(line, '\n');
        if (nl)
            *nl = 0;
        char *sp = strchr(line, ' ');
        if (!sp)
            continue;
        uint32_t want = strtoul(line, NULL, 16);
        const char *name = sp + 1;
        xcb_intern_atom_reply_t *r = xcb_intern_atom_reply(
            ac, xcb_intern_atom(ac, 0, strlen(name), name), NULL);
        if (!r)
            die("atoms: InternAtom failed");
        if (r->atom != want) {
            fprintf(stderr, "xkb-mutation-probe: atom '%s' is 0x%x here, 0x%x recorded\n", name,
                    r->atom, want);
            exit(1);
        }
        free(r);
        n++;
    }
    fclose(f);
    printf("= ok atoms=%d\n", n);
}

/* send the one request in binary file PATH (header included) on the actor,
 * with the XKB major opcode of this server */
static void send_xkb_request(const char *path)
{
    if (!actor_xkb_ready) {
        xcb_xkb_use_extension_reply_t *ue =
            xcb_xkb_use_extension_reply(ac, xcb_xkb_use_extension(ac, 1, 0), NULL);
        if (!ue || !ue->supported)
            die("actor UseExtension failed");
        free(ue);
        actor_xkb_ready = 1;
    }
    FILE *f = fopen(path, "rb");
    if (!f)
        die("xreq: cannot open file");
    static uint8_t buf[1 << 18];
    size_t n = fread(buf, 1, sizeof buf, f);
    fclose(f);
    if (n < 4 || n % 4 || (size_t)(buf[2] | buf[3] << 8) * 4 != n)
        die("xreq: file is not one request");
    buf[0] = xkb_major;
    struct iovec v[3];
    v[2].iov_base = buf;
    v[2].iov_len = n;
    xcb_protocol_request_t req = {.count = 1, .ext = NULL, .opcode = buf[1], .isvoid = 1};
    unsigned seq = xcb_send_request(ac, XCB_REQUEST_CHECKED | XCB_REQUEST_RAW, v + 2, &req);
    xcb_void_cookie_t ck = {seq};
    report_error(xcb_request_check(ac, ck));
}

/* update level_uninit for step ARG, BEFORE = the state before it: a SetMap
 * with key types marks the slots past each type's old level count, a SetNames
 * with level names clears the types it writes, a run: step clears all */
static void track_level_names(const char *arg, const struct xsnap *before)
{
    if (!strncmp(arg, "run:", 4)) {
        memset(level_uninit, 0, sizeof level_uninit);
        return;
    }
    if (strncmp(arg, "xreq:", 5))
        return;
    FILE *f = fopen(arg + 5, "rb");
    if (!f)
        die("xreq: cannot open file");
    static uint8_t b[1 << 18];
    size_t n = fread(b, 1, sizeof b, f);
    fclose(f);
    if (n >= 36 && b[1] == 9 && (u16c(b, 6) & 0x01)) {
        /* SetMap: firstType@12 nTypes@13; each xkbKeyTypeWireDesc is 8 bytes
         * (numLevels@4 nMapEntries@5 preserve@6), then nMapEntries 4-byte
         * xkbKTSetMapEntryWireDesc, then as many xkbModsWireDesc if preserve */
        int first = b[12], nt = b[13];
        const uint8_t *p = b + 36;
        for (int i = 0; i < nt && p + 8 <= b + n; i++) {
            int ty = first + i, lv = p[4], ne = p[5], pre = p[6];
            int old = ty < before->ntypes ? before->nlevels[ty] : 0;
            for (int l = old; l < lv; l++)
                level_uninit[ty][l] = 1;
            p += 8 + ne * 4 + (pre ? ne * 4 : 0);
        }
    } else if (n >= 28 && b[1] == 18 && (u32c(b, 8) & 0x80)) {
        /* SetNames with KTLevelNames: widths after the component atoms and
         * the type names; a type with width 0 keeps its level names */
        uint32_t which = u32c(b, 8);
        int ntn = b[13], firstKT = b[14], nKT = b[15], off = 28;
        for (int i = 0; i < 6; i++)
            if (which & (1u << i))
                off += 4;
        if (which & 0x40)
            off += 4 * ntn;
        for (int i = 0; i < nKT && off + i < (int)n; i++)
            if (b[off + i])
                memset(level_uninit[firstKT + i], 0, sizeof level_uninit[0]);
    }
}

/* ---- events ------------------------------------------------------------ */

static void hex32(const uint8_t *e)
{
    printf(" raw=");
    for (int i = 0; i < 32; i++)
        printf("%02x", e[i]);
    printf("\n");
}

static unsigned u16(const uint8_t *e, int o) { return e[o] | e[o + 1] << 8; }
static unsigned u32(const uint8_t *e, int o)
{
    return e[o] | e[o + 1] << 8 | e[o + 2] << 16 | (unsigned)e[o + 3] << 24;
}

static void print_xkb_event(const uint8_t *e)
{
    switch (e[1]) {
    case 0:
        printf("e xkb NewKeyboardNotify dev=%d oldDev=%d min=%d max=%d oldMin=%d oldMax=%d "
               "req=%d.%d changed=0x%04x",
               e[8], e[9], e[10], e[11], e[12], e[13], e[14], e[15], u16(e, 16));
        break;
    case 1:
        printf("e xkb MapNotify dev=%d ptrBtnActions=%d changed=0x%04x min=%d max=%d "
               "types=%d+%d syms=%d+%d acts=%d+%d beh=%d+%d expl=%d+%d modmap=%d+%d "
               "vmodmap=%d+%d vmods=0x%04x",
               e[8], e[9], u16(e, 10), e[12], e[13], e[14], e[15], e[16], e[17], e[18], e[19],
               e[20], e[21], e[22], e[23], e[24], e[25], e[26], e[27], u16(e, 28));
        break;
    case 2:
        printf("e xkb StateNotify dev=%d mods=0x%02x base=0x%02x latched=0x%02x locked=0x%02x "
               "group=%d baseGroup=%d latchedGroup=%d lockedGroup=%d compat=0x%02x "
               "grab=0x%02x compatGrab=0x%02x lookup=0x%02x compatLookup=0x%02x "
               "ptrBtn=0x%04x changed=0x%04x keycode=%d eventType=%d req=%d.%d",
               e[8], e[9], e[10], e[11], e[12], e[13], (int16_t)u16(e, 14),
               (int16_t)u16(e, 16), e[18], e[19], e[20], e[21], e[22], e[23], u16(e, 24),
               u16(e, 26), e[28], e[29], e[30], e[31]);
        break;
    case 3:
        printf("e xkb ControlsNotify dev=%d numGroups=%d changed=0x%08x enabled=0x%08x "
               "enabledChanges=0x%08x keycode=%d eventType=%d req=%d.%d",
               e[8], e[9], u32(e, 12), u32(e, 16), u32(e, 20), e[24], e[25], e[26], e[27]);
        break;
    case 4:
    case 5:
        printf("e xkb %s dev=%d state=0x%08x changed=0x%08x",
               e[1] == 4 ? "IndicatorStateNotify" : "IndicatorMapNotify", e[8], u32(e, 12),
               u32(e, 16));
        break;
    case 6:
        printf("e xkb NamesNotify dev=%d changed=0x%04x types=%d+%d levelNames=%d+%d "
               "nRadioGroups=%d nKeyAliases=%d changedGroupNames=0x%02x changedVMods=0x%04x "
               "keys=%d+%d changedIndicators=0x%08x",
               e[8], u16(e, 10), e[12], e[13], e[14], e[15], e[17], e[18], e[19], u16(e, 20),
               e[22], e[23], u32(e, 24));
        break;
    case 7:
        printf("e xkb CompatMapNotify dev=%d changedGroups=0x%02x si=%d+%d totalSI=%d", e[8],
               e[9], u16(e, 10), u16(e, 12), u16(e, 14));
        break;
    default:
        printf("e xkb xkbType=%d dev=%d", e[1], e[8]);
        break;
    }
    hex32(e);
}

static void print_core_event(const char *who, const uint8_t *e)
{
    int type = e[0] & 0x7f;
    if (type == XCB_MAPPING_NOTIFY)
        printf("e %s MappingNotify request=%d first=%d count=%d", who, e[4], e[5], e[6]);
    else
        printf("e %s type=%d", who, type);
    hex32(e);
}

static void drain(void)
{
    xcb_generic_event_t *ev;
    free(xcb_get_input_focus_reply(ac, xcb_get_input_focus(ac), NULL));
    free(xcb_get_input_focus_reply(xl, xcb_get_input_focus(xl), NULL));
    free(xcb_get_input_focus_reply(cl, xcb_get_input_focus(cl), NULL));
    while ((ev = xcb_poll_for_queued_event(xl))) {
        const uint8_t *e = (const uint8_t *)ev;
        if ((e[0] & 0x7f) == xkb_event_base)
            print_xkb_event(e);
        else
            print_core_event("xkbl", e);
        free(ev);
    }
    while ((ev = xcb_poll_for_queued_event(cl))) {
        print_core_event("core", (const uint8_t *)ev);
        free(ev);
    }
    /* actor events are not interesting, but discard them */
    while ((ev = xcb_poll_for_queued_event(ac)))
        free(ev);
}

/* ---- steps ------------------------------------------------------------- */

static int split(char *s, char sep, char **out, int max)
{
    int n = 0;
    if (!*s)
        return 0;
    out[n++] = s;
    for (; *s && n < max; s++)
        if (*s == sep) {
            *s = 0;
            out[n++] = s + 1;
        }
    return n;
}

static void report_error(xcb_generic_error_t *err)
{
    if (err) {
        printf("= error=%d value=%u\n", err->error_code, err->resource_id);
        free(err);
    } else
        printf("= ok\n");
}

static void send_smm(int kpm, const xcb_keycode_t *kc)
{
    xcb_generic_error_t *err = NULL;
    xcb_set_modifier_mapping_reply_t *r =
        xcb_set_modifier_mapping_reply(ac, xcb_set_modifier_mapping(ac, kpm, kc), &err);
    if (r) {
        printf("= status=%d\n", r->status);
        free(r);
    } else
        report_error(err);
}

/* SetModifierMapping built from the current GetModifierMapping (read on the
 * actor) with edits applied: "same" (no edit), "-KC@M" (remove KC from
 * modifier M, row compacted), "+KC@M" (append KC to row M; the width grows
 * when the row is full), "kpm=N" (send with width N, rows zero padded; dies
 * if a row does not fit). The request actually sent is printed first. */
static void smmx(char *edits)
{
    xcb_get_modifier_mapping_reply_t *r =
        xcb_get_modifier_mapping_reply(ac, xcb_get_modifier_mapping(ac), NULL);
    if (!r)
        die("GetModifierMapping failed");
    int cur = r->keycodes_per_modifier, force = 0;
    int row[8][64], len[8];
    xcb_keycode_t *k = xcb_get_modifier_mapping_keycodes(r);
    for (int m = 0; m < 8; m++) {
        len[m] = 0;
        for (int i = 0; i < cur; i++)
            if (k[m * cur + i])
                row[m][len[m]++] = k[m * cur + i];
    }
    free(r);
    char *e[32];
    int ne = split(edits, ',', e, 32);
    for (int i = 0; i < ne; i++) {
        int kc, m;
        if (!strcmp(e[i], "same"))
            continue;
        if (sscanf(e[i], "kpm=%d", &force) == 1)
            continue;
        if (sscanf(e[i] + 1, "%d@%d", &kc, &m) != 2 || m < 0 || m > 7)
            die("smmx: bad edit");
        if (e[i][0] == '+')
            row[m][len[m]++] = kc;
        else if (e[i][0] == '-') {
            int j = 0;
            for (int t = 0; t < len[m]; t++)
                if (row[m][t] != kc)
                    row[m][j++] = row[m][t];
            len[m] = j;
        } else
            die("smmx: bad edit");
    }
    /* keep the current width unless a row needs more (xmodmap does the same) */
    int kpm = cur;
    for (int m = 0; m < 8; m++)
        if (len[m] > kpm)
            kpm = len[m];
    if (force) {
        for (int m = 0; m < 8; m++)
            if (len[m] > force)
                die("smmx: row does not fit kpm");
        kpm = force;
    }
    xcb_keycode_t out[8 * 64];
    memset(out, 0, sizeof out);
    for (int m = 0; m < 8; m++)
        for (int i = 0; i < len[m]; i++)
            out[m * kpm + i] = row[m][i];
    printf("sent kpm=%d keys=", kpm);
    for (int i = 0; i < 8 * kpm; i++)
        printf("%s%d", i ? "," : "", out[i]);
    printf("\n");
    send_smm(kpm, out);
}

static void step(char *arg)
{
    printf("> %s\n", arg);
    fflush(stdout);
    if (!strncmp(arg, "run:", 4)) {
        /* an external client acts instead of the actor connection */
        int rc = system(arg + 4);
        printf("= exit=%d\n", WIFEXITED(rc) ? WEXITSTATUS(rc) : -1);
        return;
    }
    if (!strncmp(arg, "atoms:", 6)) {
        intern_atoms(arg + 6);
        return;
    }
    if (!strncmp(arg, "xreq:", 5)) {
        send_xkb_request(arg + 5);
        return;
    }
    char *f[8];
    char *copy = xstrdup(arg);
    int n = split(copy, ':', f, 8);
    if (!strcmp(f[0], "ckm") && n == 5) {
        int first = atoi(f[1]), kpk = atoi(f[2]), count = atoi(f[3]);
        char *sv[2048];
        int ns = split(f[4], ',', sv, 2048);
        uint32_t syms[2048];
        for (int i = 0; i < ns; i++)
            syms[i] = strtoul(sv[i], NULL, 16);
        /* raw request so nsyms can disagree with count*kpk */
        uint8_t hdr[8] = {100, (uint8_t)count, 0, 0, (uint8_t)first, (uint8_t)kpk, 0, 0};
        unsigned len = 2 + ns;
        hdr[2] = len & 0xff;
        hdr[3] = len >> 8;
        /* xcb_send_request wants two writable iovecs in front of the request */
        struct iovec v[4];
        v[2].iov_base = hdr;
        v[2].iov_len = 8;
        v[3].iov_base = syms;
        v[3].iov_len = 4 * ns;
        xcb_protocol_request_t req = {.count = 2, .ext = NULL, .opcode = 100, .isvoid = 1};
        unsigned seq = xcb_send_request(ac, XCB_REQUEST_CHECKED | XCB_REQUEST_RAW, v + 2, &req);
        xcb_void_cookie_t ck = {seq};
        report_error(xcb_request_check(ac, ck));
    } else if (!strcmp(f[0], "smm") && n == 3) {
        int kpm = atoi(f[1]);
        char *kv[256];
        int nk = split(f[2], ',', kv, 256);
        xcb_keycode_t kc[256];
        if (nk != 8 * kpm)
            die("smm: need 8*kpm keycodes");
        for (int i = 0; i < nk; i++)
            kc[i] = atoi(kv[i]);
        send_smm(kpm, kc);
    } else if (!strcmp(f[0], "smmx") && n == 2) {
        smmx(f[1]);
    } else if ((!strcmp(f[0], "down") || !strcmp(f[0], "up")) && n == 2) {
        uint8_t type = !strcmp(f[0], "down") ? XCB_KEY_PRESS : XCB_KEY_RELEASE;
        report_error(xcb_request_check(
            ac, xcb_test_fake_input_checked(ac, type, atoi(f[1]), XCB_CURRENT_TIME,
                                            XCB_NONE, 0, 0, 0)));
    } else
        die("bad step");
    free(copy);
}

static xcb_connection_t *open_conn(const char *d)
{
    xcb_connection_t *c = xcb_connect(d, NULL);
    if (xcb_connection_has_error(c))
        die("cannot connect");
    return c;
}

int main(int argc, char **argv)
{
    const char *disp = NULL;
    int i = 1;
    if (argc > 2 && !strcmp(argv[1], "-d")) {
        disp = argv[2];
        i = 3;
    }
    if (i < argc && !strcmp(argv[i], "-x")) {
        extended = 1;
        i++;
    }
    if (i >= argc)
        die("usage: xkb-mutation-probe [-d DISPLAY] [-x] STEP...");
    xl = open_conn(disp);
    cl = open_conn(disp);
    ac = open_conn(disp);

    const xcb_query_extension_reply_t *qe = xcb_get_extension_data(xl, &xcb_xkb_id);
    if (!qe || !qe->present)
        die("no XKB");
    xkb_event_base = qe->first_event;
    xkb_major = qe->major_opcode;
    xcb_xkb_use_extension_reply_t *ue =
        xcb_xkb_use_extension_reply(xl, xcb_xkb_use_extension(xl, 1, 0), NULL);
    if (!ue || !ue->supported)
        die("UseExtension failed");
    free(ue);
    xcb_generic_error_t *err = xcb_request_check(
        xl, xcb_xkb_select_events_checked(xl, DEV_CORE, ALL_EVENTS, 0, ALL_EVENTS, ALL_MAP,
                                          ALL_MAP, NULL));
    if (err)
        die("SelectEvents failed");
    /* make sure the core listener is known to the server as a client with the
     * root window's default event delivery (MappingNotify goes to all) */
    free(xcb_get_input_focus_reply(cl, xcb_get_input_focus(cl), NULL));
    free(xcb_query_extension_reply(ac, xcb_query_extension(ac, 5, "XTEST"), NULL));
    drain();

    struct snap first, before, after;
    struct xsnap xfirst = {0}, xbefore = {0}, xafter = {0};
    take_snap(&first);
    take_snap(&before);
    if (extended) {
        take_xsnap(&xfirst);
        take_xsnap(&xbefore);
    }
    for (; i < argc; i++) {
        if (!strcmp(argv[i], "total")) {
            printf("> total\n");
            diff_snap(&first, &before);
            if (extended)
                diff_xsnap(&xfirst, &xbefore);
            continue;
        }
        if (!strcmp(argv[i], "full")) {
            /* the whole current state (needs -x) */
            if (!extended)
                die("full needs -x");
            printf("> full\n");
            print_full(&before, &xbefore);
            core_modmap();
            continue;
        }
        step(argv[i]);
        drain();
        take_snap(&after);
        diff_snap(&before, &after);
        if (extended) {
            track_level_names(argv[i], &xbefore);
            take_xsnap(&xafter);
            diff_xsnap(&xbefore, &xafter);
            xs_free(&xbefore);
            xbefore = xafter;
            memset(&xafter, 0, sizeof xafter);
        }
        core_modmap();
        snap_free(&before);
        before = after;
    }
    fflush(stdout);
    xcb_disconnect(ac);
    xcb_disconnect(cl);
    xcb_disconnect(xl);
    return 0;
}
