/* Issue #171: after `xmodmap` turns Caps Lock into a Control key, do XKB
 * clients (Xlib-with-XKB, xkbcommon-x11 toolkits) and the server's own key
 * cooking agree that keycode 66 is Control_L?
 *
 *   probe listen   — Xlib client with XKB active. Maps a window, takes focus,
 *                    prints every KeyPress/KeyRelease (keycode, state,
 *                    XkbLookupKeySym, XLookupString) and every XkbMapNotify /
 *                    MappingNotify, re-reading the keymap via XkbGetMap on
 *                    each XkbMapNotify. Runs until killed (SIGTERM).
 *   probe xkbmap [TAG [KC..]] — one-shot XkbGetMap dump of keycodes 66 and
 *                    37, or of the keycodes given.
 *   probe xkbcommon — builds a keymap with xkb_x11_keymap_new_from_device (what
 *                    GTK/Qt do) and prints keycode 66's level-1 keysym and the
 *                    modifiers it sets when pressed, whether press+release of
 *                    66 locks anything, and what 'a' produces with 66 held.
 *
 * All output is line-buffered and free of ids/timestamps so Xorg and yserver
 * runs diff cleanly. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <signal.h>
#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <X11/XKBlib.h>
#include <xcb/xcb.h>
#include <xkbcommon/xkbcommon.h>
#include <xkbcommon/xkbcommon-x11.h>

static void dump_xkb_key(Display *d, XkbDescPtr xkb, const char *tag, int kc)
{
    int n = XkbKeyNumSyms(xkb, kc);
    (void)d;
    printf("%s kc=%d type_index=%d nsyms=%d syms=[", tag, kc,
           XkbKeyKeyTypeIndex(xkb, kc, 0), n);
    for (int i = 0; i < n; i++) {
        const char *s = XKeysymToString(XkbKeySymsPtr(xkb, kc)[i]);
        printf(" %s", s ? s : "NoSymbol");
    }
    printf(" ] modmap=0x%02x", xkb->map->modmap ? xkb->map->modmap[kc] : 0);
    if (xkb->server && xkb->server->key_acts && XkbKeyHasActions(xkb, kc)) {
        XkbAction *a = XkbKeyActionsPtr(xkb, kc);
        printf(" act0.type=%d", a[0].type);
        if (a[0].type == XkbSA_SetMods || a[0].type == XkbSA_LatchMods ||
            a[0].type == XkbSA_LockMods)
            printf(" act0.flags=0x%02x act0.mask=0x%02x", a[0].mods.flags,
                   a[0].mods.mask);
    } else {
        printf(" act0=none");
    }
    printf("\n");
}

/* The keycodes dump_xkbmap prints: 66 and 37 unless `probe xkbmap TAG KC..`
 * names others. */
static int dump_kcs[16] = {66, 37};
static int n_dump_kcs = 2;

static void dump_xkbmap(Display *d, const char *tag)
{
    XkbDescPtr xkb = XkbGetMap(d, XkbAllMapComponentsMask, XkbUseCoreKbd);
    if (!xkb) {
        printf("%s XkbGetMap failed\n", tag);
        return;
    }
    for (int i = 0; i < n_dump_kcs; i++)
        dump_xkb_key(d, xkb, tag, dump_kcs[i]);
    XkbFreeKeyboard(xkb, 0, True);
}

static volatile sig_atomic_t stop;
static void on_term(int s) { (void)s; stop = 1; }

static int listen_mode(void)
{
    Display *d = XOpenDisplay(NULL);
    if (!d) return 1;
    int op, ev, er, maj = XkbMajorVersion, min = XkbMinorVersion;
    if (!XkbQueryExtension(d, &op, &ev, &er, &maj, &min) || !XkbUseExtension(d, &maj, &min)) {
        printf("listen: no XKB\n");
        return 1;
    }
    XkbSelectEvents(d, XkbUseCoreKbd, XkbMapNotifyMask | XkbNewKeyboardNotifyMask,
                    XkbMapNotifyMask | XkbNewKeyboardNotifyMask);
    Window w = XCreateSimpleWindow(d, DefaultRootWindow(d), 10, 10, 200, 100, 0, 0, 0xffffff);
    XSelectInput(d, w, KeyPressMask | KeyReleaseMask | StructureNotifyMask | FocusChangeMask);
    XMapWindow(d, w);
    XEvent e;
    do XNextEvent(d, &e); while (e.type != MapNotify);
    XSetInputFocus(d, w, RevertToParent, CurrentTime);
    XSync(d, False);
    Window f; int rev;
    XGetInputFocus(d, &f, &rev);
    printf("listen: ready focus_is_ours=%d\n", f == w);
    dump_xkbmap(d, "listen:initial");
    signal(SIGTERM, on_term);
    int fd = ConnectionNumber(d);
    while (!stop) {
        if (!XPending(d)) {
            fd_set s; FD_ZERO(&s); FD_SET(fd, &s);
            struct timeval tv = { 0, 100000 };
            select(fd + 1, &s, NULL, NULL, &tv);
            continue;
        }
        XNextEvent(d, &e);
        if (e.type == KeyPress || e.type == KeyRelease) {
            XKeyEvent *k = &e.xkey;
            KeySym lk = NoSymbol; unsigned int mods_rtrn = 0;
            XkbLookupKeySym(d, k->keycode, k->state, &mods_rtrn, &lk);
            char buf[16] = {0}; KeySym ls = NoSymbol;
            int n = XLookupString(k, buf, sizeof buf - 1, &ls, NULL);
            printf("listen: %s kc=%u state=0x%04x xkblookup=%s consumed=0x%02x "
                   "xlookupstring=%s nbytes=%d bytes=[",
                   e.type == KeyPress ? "KeyPress  " : "KeyRelease", k->keycode, k->state,
                   XKeysymToString(lk) ? XKeysymToString(lk) : "NoSymbol", mods_rtrn,
                   XKeysymToString(ls) ? XKeysymToString(ls) : "NoSymbol", n);
            for (int i = 0; i < n; i++) printf(" 0x%02x", (unsigned char)buf[i]);
            printf(" ]\n");
        } else if (e.type == MappingNotify) {
            printf("listen: MappingNotify request=%d first=%d count=%d\n",
                   e.xmapping.request, e.xmapping.first_keycode, e.xmapping.count);
            XRefreshKeyboardMapping(&e.xmapping);
        } else if (e.type == ev) {
            XkbEvent *x = (XkbEvent *)&e;
            if (x->any.xkb_type == XkbMapNotify) {
                printf("listen: XkbMapNotify device=%d changed=0x%04x first_key_sym=%d num_key_syms=%d "
                       "first_modmap_key=%d num_modmap_keys=%d\n",
                       x->map.device, x->map.changed, x->map.first_key_sym, x->map.num_key_syms,
                       x->map.first_modmap_key, x->map.num_modmap_keys);
                dump_xkbmap(d, "listen:after-mapnotify");
            } else if (x->any.xkb_type == XkbNewKeyboardNotify) {
                /* Xorg sends this when the master keyboard's source slave
                 * changes (e.g. first XTEST key after a physical one). */
                printf("listen: XkbNewKeyboardNotify device=%d old_device=%d changed=0x%04x "
                       "req=%d.%d\n", x->new_kbd.device, x->new_kbd.old_device,
                       x->new_kbd.changed, x->new_kbd.req_major, x->new_kbd.req_minor);
            }
        }
    }
    printf("listen: done\n");
    XCloseDisplay(d);
    return 0;
}

static int xkbmap_mode(const char *tag)
{
    Display *d = XOpenDisplay(NULL);
    if (!d) return 1;
    dump_xkbmap(d, tag);
    XCloseDisplay(d);
    return 0;
}

static void print_mods(struct xkb_keymap *km, struct xkb_state *st,
                       enum xkb_state_component which)
{
    printf("[");
    for (xkb_mod_index_t i = 0; i < xkb_keymap_num_mods(km); i++)
        if (xkb_state_mod_index_is_active(st, i, which) > 0)
            printf(" %s", xkb_keymap_mod_get_name(km, i));
    printf(" ]");
}

static int xkbcommon_mode(void)
{
    xcb_connection_t *c = xcb_connect(NULL, NULL);
    if (xcb_connection_has_error(c)) return 1;
    if (!xkb_x11_setup_xkb_extension(c, XKB_X11_MIN_MAJOR_XKB_VERSION,
                                     XKB_X11_MIN_MINOR_XKB_VERSION,
                                     XKB_X11_SETUP_XKB_EXTENSION_NO_FLAGS,
                                     NULL, NULL, NULL, NULL)) {
        printf("xkbcommon: setup failed\n");
        return 1;
    }
    struct xkb_context *ctx = xkb_context_new(XKB_CONTEXT_NO_FLAGS);
    int32_t dev = xkb_x11_get_core_keyboard_device_id(c);
    struct xkb_keymap *km = xkb_x11_keymap_new_from_device(ctx, c, dev, XKB_KEYMAP_COMPILE_NO_FLAGS);
    if (!km) {
        printf("xkbcommon: keymap_new_from_device failed\n");
        return 1;
    }
    const xkb_keysym_t *syms;
    int n = xkb_keymap_key_get_syms_by_level(km, 66, 0, 0, &syms);
    char name[64] = "NoSymbol";
    if (n > 0) xkb_keysym_get_name(syms[0], name, sizeof name);
    printf("xkbcommon: kc66 layout0 level1 nsyms=%d sym=%s\n", n, name);

    /* A fresh state from the device's keymap; press/release on it the way a
     * toolkit feeds key events (toolkits actually use update_mask from the
     * server state, but the keymap's actions are what decide these). */
    struct xkb_state *st = xkb_state_new(km);
    xkb_state_update_key(st, 66, XKB_KEY_DOWN);
    printf("xkbcommon: kc66 held depressed=");
    print_mods(km, st, XKB_STATE_MODS_DEPRESSED);
    printf(" locked=");
    print_mods(km, st, XKB_STATE_MODS_LOCKED);
    char u[16] = {0};
    xkb_keysym_t a = xkb_state_key_get_one_sym(st, 38);
    xkb_keysym_get_name(a, name, sizeof name);
    int un = xkb_state_key_get_utf8(st, 38, u, sizeof u);
    printf(" | kc38 sym=%s utf8=[", name);
    for (int i = 0; i < un; i++) printf(" 0x%02x", (unsigned char)u[i]);
    printf(" ]\n");
    xkb_state_update_key(st, 66, XKB_KEY_UP);
    printf("xkbcommon: kc66 released depressed=");
    print_mods(km, st, XKB_STATE_MODS_DEPRESSED);
    printf(" locked=");
    print_mods(km, st, XKB_STATE_MODS_LOCKED);
    a = xkb_state_key_get_one_sym(st, 38);
    xkb_keysym_get_name(a, name, sizeof name);
    un = xkb_state_key_get_utf8(st, 38, u, sizeof u);
    printf(" | kc38 sym=%s utf8=[", name);
    for (int i = 0; i < un; i++) printf(" 0x%02x", (unsigned char)u[i]);
    printf(" ]\n");
    xkb_state_unref(st);

    /* And the live server state as toolkits track it (state_new_from_device). */
    st = xkb_x11_state_new_from_device(km, c, dev);
    printf("xkbcommon: server-state effective=");
    print_mods(km, st, XKB_STATE_MODS_EFFECTIVE);
    printf(" locked=");
    print_mods(km, st, XKB_STATE_MODS_LOCKED);
    printf("\n");
    xkb_state_unref(st);
    xkb_keymap_unref(km);
    xkb_context_unref(ctx);
    xcb_disconnect(c);
    return 0;
}

int main(int argc, char **argv)
{
    setvbuf(stdout, NULL, _IOLBF, 0);
    const char *mode = argc > 1 ? argv[1] : "listen";
    if (!strcmp(mode, "listen")) return listen_mode();
    if (!strcmp(mode, "xkbmap")) {
        if (argc > 3) {
            n_dump_kcs = 0;
            for (int i = 3; i < argc && n_dump_kcs < 16; i++)
                dump_kcs[n_dump_kcs++] = atoi(argv[i]);
        }
        return xkbmap_mode(argc > 2 ? argv[2] : "xkbmap");
    }
    if (!strcmp(mode, "xkbcommon")) return xkbcommon_mode();
    fprintf(stderr, "usage: %s listen|xkbmap [tag [kc..]]|xkbcommon\n", argv[0]);
    return 2;
}
