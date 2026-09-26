/* Issue #173: XI2 raw key events (XI_RawKeyPress 13 / XI_RawKeyRelease 14).
 *
 * One process, several X connections, so the log is one deterministic
 * interleaving: after every command the driver connection round-trips,
 * then every monitor round-trips and prints what it received. Commands
 * come from argv, in order:
 *
 *   list                      XIQueryDevice(AllDevices): id/use/attach/name
 *   mon:TAG:MINORS:DEVS       new client; one XIQueryVersion(2, m) per m in
 *                             MINORS (comma separated, "-" for none), then
 *                             selects RawKeyPress|RawKeyRelease on the root
 *                             for each deviceid in DEVS (comma separated; a
 *                             "k" suffix, e.g. 1k, adds KeyPress|KeyRelease)
 *   drvsel:DEVS               the driver client selects the same on the root
 *   p<kc> / r<kc>             XTEST FakeInput KeyPress / KeyRelease (driver)
 *   grabkbd:sync|async        core GrabKeyboard(root) by the driver
 *   ungrabkbd                 core UngrabKeyboard
 *   grabkey:<kc>:sync|async   core GrabKey(kc, AnyModifier, root)
 *   allow:async|sync|replay   core AllowEvents Async/Sync/ReplayKeyboard
 *   xigrab:DEV[:win][:oe][:noraw]
 *                             XIGrabDevice(DEV), async, on the root (or on a
 *                             mapped child window), owner_events if :oe,
 *                             grab mask KeyPress|KeyRelease plus
 *                             RawKeyPress|RawKeyRelease unless :noraw
 *   xiungrab:DEV              XIUngrabDevice
 *   listen:SECONDS            print events as they arrive (physical keys)
 *   # text                    echo a marker line
 *
 * The driver announces XI 2.2. Every XGE event is printed as the parsed
 * raw fields plus its wire bytes with the XI opcode (byte 1), sequence
 * (bytes 2-3) and time (bytes 12-15) blanked, so an Xorg run and a yserver
 * run diff directly;
 * `dt` is the time delta to the previous event on the same connection
 * (dt=0 between a raw event and its device event means one timestamp).
 *
 *   cc -O1 -o probe xi2-raw-keys-probe.c -lxcb -lxcb-xinput -lxcb-xtest
 */
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <xcb/xcb.h>
#include <xcb/xinput.h>
#include <xcb/xtest.h>

#define MAX_CONN 16

struct conn {
    xcb_connection_t *c;
    char tag[32];
    uint8_t xi_opcode;
    uint32_t last_time;
    int have_time;
};

static struct conn conns[MAX_CONN];
static int nconns;
static xcb_window_t root, child;

static const char *evname(int t)
{
    switch (t) {
    case 2: return "KeyPress";
    case 3: return "KeyRelease";
    case 13: return "RawKeyPress";
    case 14: return "RawKeyRelease";
    default: return "?";
    }
}

static uint16_t rd16(const uint8_t *b) { return (uint16_t)(b[0] | b[1] << 8); }
static uint32_t rd32(const uint8_t *b)
{
    return (uint32_t)b[0] | (uint32_t)b[1] << 8 | (uint32_t)b[2] << 16 | (uint32_t)b[3] << 24;
}

static void print_event(struct conn *k, xcb_generic_event_t *ev)
{
    uint8_t type = ev->response_type & 0x7f;
    if (type == 0) {
        xcb_generic_error_t *e = (xcb_generic_error_t *)ev;
        printf("%s ERROR code=%u major=%u minor=%u\n", k->tag, e->error_code,
               e->major_code, e->minor_code);
        return;
    }
    if (type != XCB_GE_GENERIC) {
        printf("%s core-event type=%u\n", k->tag, type);
        return;
    }
    /* XCB stores the 32-byte header, then full_sequence (4 bytes), then
     * the `length` extra words. Rebuild the wire image. */
    const uint8_t *b = (const uint8_t *)ev;
    uint32_t len = rd32(b + 4);
    size_t n = 32 + (size_t)len * 4;
    uint8_t *w = malloc(n);
    memcpy(w, b, 32);
    memcpy(w + 32, b + 36, n - 32);
    uint16_t evtype = rd16(w + 8);
    uint32_t t = rd32(w + 12);
    long dt = k->have_time ? (long)(t - k->last_time) : -1;
    k->last_time = t;
    k->have_time = 1;
    if (w[1] != k->xi_opcode)
        printf("%s GE ext=%u evtype=%u\n", k->tag, w[1], evtype);
    else if (evtype == 13 || evtype == 14 || evtype == 15 || evtype == 16 || evtype == 17)
        printf("%s %-13s dev=%u src=%u detail=%u flags=0x%x vlen=%u len=%u dt=%ld\n", k->tag,
               evname(evtype), rd16(w + 10), rd16(w + 20), rd32(w + 16), rd32(w + 24),
               rd16(w + 22), len, dt);
    else
        printf("%s %-13s dev=%u src=%u detail=%u event=%s flags=0x%x len=%u dt=%ld\n", k->tag,
               evname(evtype), rd16(w + 10), rd16(w + 52), rd32(w + 16),
               rd32(w + 24) == root ? "root" : (rd32(w + 24) == child ? "child" : "other"),
               rd32(w + 56), len, dt);
    /* Blank the extension opcode, sequence and time so runs diff. */
    printf("%s   wire", k->tag);
    for (size_t i = 0; i < n; i++) {
        if (i % 4 == 0) printf(" ");
        if (i == 1 || (i >= 2 && i < 4) || (i >= 12 && i < 16))
            printf("..");
        else
            printf("%02x", w[i]);
    }
    printf("\n");
    free(w);
}

static void sync_conn(struct conn *k)
{
    free(xcb_get_input_focus_reply(k->c, xcb_get_input_focus(k->c), NULL));
}

static void drain_conn(struct conn *k)
{
    xcb_generic_event_t *ev;
    while ((ev = xcb_poll_for_event(k->c))) {
        print_event(k, ev);
        free(ev);
    }
}

static void settle(void)
{
    for (int i = 0; i < nconns; i++) sync_conn(&conns[i]);
    for (int i = 0; i < nconns; i++) {
        sync_conn(&conns[i]);
        drain_conn(&conns[i]);
    }
    fflush(stdout);
}

/* `minors`: comma-separated XI 2.x minor versions to announce, one
 * XIQueryVersion each, in order; "-" announces nothing. */
static struct conn *open_conn(const char *tag, const char *minors)
{
    struct conn *k = &conns[nconns++];
    k->c = xcb_connect(NULL, NULL);
    if (xcb_connection_has_error(k->c)) {
        fprintf(stderr, "cannot connect\n");
        exit(1);
    }
    snprintf(k->tag, sizeof k->tag, "%s", tag);
    const xcb_query_extension_reply_t *q = xcb_get_extension_data(k->c, &xcb_input_id);
    if (!q || !q->present) {
        fprintf(stderr, "no XInputExtension\n");
        exit(1);
    }
    k->xi_opcode = q->major_opcode;
    char buf[32];
    snprintf(buf, sizeof buf, "%s", minors);
    for (char *m = strtok(buf, ","); m && strcmp(m, "-") != 0; m = strtok(NULL, ",")) {
        int minor = atoi(m);
        xcb_generic_error_t *e = NULL;
        xcb_input_xi_query_version_reply_t *v = xcb_input_xi_query_version_reply(
            k->c, xcb_input_xi_query_version(k->c, 2, (uint16_t)minor), &e);
        if (v)
            printf("%s xi-version asked=2.%d got=%u.%u\n", tag, minor, v->major_version,
                   v->minor_version);
        else
            printf("%s xi-version asked=2.%d error=%u\n", tag, minor, e ? e->error_code : 0);
        free(v);
        free(e);
    }
    root = xcb_setup_roots_iterator(xcb_get_setup(k->c)).data->root;
    return k;
}

static void select_raw(struct conn *k, const char *devs)
{
    struct {
        xcb_input_event_mask_t h;
        uint32_t m;
    } masks[8];
    int n = 0;
    char buf[64];
    snprintf(buf, sizeof buf, "%s", devs);
    for (char *s = strtok(buf, ","); s && n < 8; s = strtok(NULL, ",")) {
        masks[n].h.deviceid = (xcb_input_device_id_t)atoi(s);
        masks[n].h.mask_len = 1;
        masks[n].m = XCB_INPUT_XI_EVENT_MASK_RAW_KEY_PRESS | XCB_INPUT_XI_EVENT_MASK_RAW_KEY_RELEASE;
        if (strchr(s, 'k'))
            masks[n].m |= XCB_INPUT_XI_EVENT_MASK_KEY_PRESS | XCB_INPUT_XI_EVENT_MASK_KEY_RELEASE;
        n++;
    }
    xcb_generic_error_t *e = xcb_request_check(
        k->c, xcb_input_xi_select_events_checked(k->c, root, (uint16_t)n,
                                                 (const xcb_input_event_mask_t *)masks));
    printf("%s select on root devs=%s -> %s\n", k->tag, devs, e ? "ERROR" : "ok");
    free(e);
}

static void fake_key(struct conn *d, uint8_t type, uint8_t kc)
{
    xcb_test_fake_input(d->c, type, kc, XCB_CURRENT_TIME, root, 0, 0, 0);
}

static uint8_t mode_of(const char *s) { return strcmp(s, "sync") == 0 ? 0 : 1; }

static void make_child(struct conn *d)
{
    if (child) return;
    child = xcb_generate_id(d->c);
    uint32_t v = 1;
    xcb_create_window(d->c, XCB_COPY_FROM_PARENT, child, root, 10, 10, 50, 50, 0,
                      XCB_WINDOW_CLASS_INPUT_OUTPUT, XCB_COPY_FROM_PARENT,
                      XCB_CW_OVERRIDE_REDIRECT, &v);
    xcb_map_window(d->c, child);
}

static void listen_for(int seconds)
{
    struct pollfd p[MAX_CONN];
    for (int i = 0; i < nconns; i++) {
        p[i].fd = xcb_get_file_descriptor(conns[i].c);
        p[i].events = POLLIN;
    }
    time_t end = time(NULL) + seconds;
    while (time(NULL) < end) {
        if (poll(p, (nfds_t)nconns, 250) <= 0) continue;
        for (int i = 0; i < nconns; i++) drain_conn(&conns[i]);
        fflush(stdout);
    }
}

int main(int argc, char **argv)
{
    setvbuf(stdout, NULL, _IOLBF, 0);
    struct conn *d = open_conn("drv", "2");
    for (int i = 1; i < argc; i++) {
        const char *a = argv[i];
        char tag[32];
        char minors[32];
        char devs[64];
        int kc;
        char mode[16];
        if (a[0] == '#') {
            printf("%s\n", a);
            continue;
        }
        printf("> %s\n", a);
        if (strcmp(a, "list") == 0) {
            xcb_input_xi_query_device_reply_t *r = xcb_input_xi_query_device_reply(
                d->c, xcb_input_xi_query_device(d->c, 0), NULL);
            xcb_input_xi_device_info_iterator_t it =
                xcb_input_xi_query_device_infos_iterator(r);
            for (; it.rem; xcb_input_xi_device_info_next(&it)) {
                int nl = xcb_input_xi_device_info_name_length(it.data);
                printf("  device id=%u use=%u attachment=%u name=%.*s\n", it.data->deviceid,
                       it.data->type, it.data->attachment, nl,
                       xcb_input_xi_device_info_name(it.data));
            }
            free(r);
        } else if (sscanf(a, "mon:%31[^:]:%31[^:]:%63s", tag, minors, devs) == 3) {
            struct conn *k = open_conn(tag, minors);
            select_raw(k, devs);
        } else if (sscanf(a, "drvsel:%63s", devs) == 1) {
            select_raw(d, devs);
        } else if (sscanf(a, "p%d", &kc) == 1 && a[0] == 'p') {
            fake_key(d, XCB_KEY_PRESS, (uint8_t)kc);
        } else if (sscanf(a, "r%d", &kc) == 1 && a[0] == 'r') {
            fake_key(d, XCB_KEY_RELEASE, (uint8_t)kc);
        } else if (sscanf(a, "grabkbd:%15s", mode) == 1) {
            xcb_grab_keyboard_reply_t *r = xcb_grab_keyboard_reply(
                d->c,
                xcb_grab_keyboard(d->c, 0, root, XCB_CURRENT_TIME, XCB_GRAB_MODE_ASYNC,
                                  mode_of(mode)),
                NULL);
            printf("drv GrabKeyboard status=%u\n", r ? r->status : 255);
            free(r);
        } else if (strcmp(a, "ungrabkbd") == 0) {
            xcb_ungrab_keyboard(d->c, XCB_CURRENT_TIME);
        } else if (sscanf(a, "grabkey:%d:%15s", &kc, mode) == 2) {
            xcb_generic_error_t *e = xcb_request_check(
                d->c, xcb_grab_key_checked(d->c, 0, root, XCB_MOD_MASK_ANY, (uint8_t)kc,
                                           XCB_GRAB_MODE_ASYNC, mode_of(mode)));
            printf("drv GrabKey -> %s\n", e ? "ERROR" : "ok");
            free(e);
        } else if (sscanf(a, "allow:%15s", mode) == 1) {
            uint8_t m = strcmp(mode, "async") == 0  ? XCB_ALLOW_ASYNC_KEYBOARD
                        : strcmp(mode, "sync") == 0 ? XCB_ALLOW_SYNC_KEYBOARD
                                                    : XCB_ALLOW_REPLAY_KEYBOARD;
            xcb_allow_events(d->c, m, XCB_CURRENT_TIME);
        } else if (strncmp(a, "xigrab:", 7) == 0) {
            int dev = atoi(a + 7);
            int on_child = strstr(a + 7, ":win") != NULL;
            int owner_events = strstr(a + 7, ":oe") != NULL;
            if (on_child) make_child(d);
            uint32_t m = XCB_INPUT_XI_EVENT_MASK_KEY_PRESS | XCB_INPUT_XI_EVENT_MASK_KEY_RELEASE;
            if (!strstr(a + 7, ":noraw"))
                m |= XCB_INPUT_XI_EVENT_MASK_RAW_KEY_PRESS | XCB_INPUT_XI_EVENT_MASK_RAW_KEY_RELEASE;
            xcb_input_xi_grab_device_reply_t *r = xcb_input_xi_grab_device_reply(
                d->c,
                xcb_input_xi_grab_device(d->c, on_child ? child : root, XCB_CURRENT_TIME,
                                         XCB_NONE, (xcb_input_device_id_t)dev,
                                         XCB_INPUT_GRAB_MODE_22_ASYNC,
                                         XCB_INPUT_GRAB_MODE_22_ASYNC, (uint8_t)owner_events,
                                         1, &m),
                NULL);
            printf("drv XIGrabDevice dev=%d window=%s status=%u\n", dev,
                   on_child ? "child" : "root", r ? r->status : 255);
            free(r);
        } else if (sscanf(a, "xiungrab:%d", &kc) == 1) {
            xcb_input_xi_ungrab_device(d->c, XCB_CURRENT_TIME, (xcb_input_device_id_t)kc);
        } else if (sscanf(a, "listen:%d", &kc) == 1) {
            settle();
            listen_for(kc);
        } else {
            fprintf(stderr, "unknown command %s\n", a);
            return 2;
        }
        xcb_flush(d->c);
        settle();
    }
    return 0;
}
