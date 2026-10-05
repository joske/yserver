/* A client that stops reading while events flood in must be disconnected,
 * not kept alive and silently skipped. yserver caps a client's unread
 * output (OUTBOUND_CAP); Xorg grows it instead (os/io.c FlushClient), so
 * this checks yserver's policy, not an Xorg golden.
 *
 *   ./probe [family...]     # default: every family below
 *
 * Per family: connection V selects the events and never reads again, H
 * selects the same events and reads everything, S makes FLOOD events. Then
 * V's marker window must be gone (V disconnected), H must hold every event,
 * and the server must answer S. Families: property (PropertyNotify),
 * configure (ConfigureNotify), motion (core MotionNotify via XTEST), key
 * (KeyPress/KeyRelease via XTEST), xi2motion (XI_Motion via XTEST), damage
 * (DamageNotify, raw rectangles).
 *
 *   cc -O1 -o probe slow-client-probe.c -lxcb -lxcb-xtest -lxcb-xinput -lxcb-damage
 */
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <xcb/damage.h>
#include <xcb/xcb.h>
#include <xcb/xinput.h>
#include <xcb/xtest.h>

#define FLOOD 200000
#define BATCH 1000

static xcb_connection_t *s;
static xcb_window_t root, target;
static int16_t tx = 20, ty = 20;
static int failures;

static void sync_conn(xcb_connection_t *c)
{
    free(xcb_get_input_focus_reply(c, xcb_get_input_focus(c), NULL));
}

static xcb_connection_t *connect_or_die(void)
{
    xcb_connection_t *c = xcb_connect(NULL, NULL);
    if (xcb_connection_has_error(c)) {
        printf("cannot connect\n");
        exit(2);
    }
    return c;
}

static uint8_t ext_event_base(xcb_connection_t *c, const char *name)
{
    const xcb_query_extension_reply_t *r =
        xcb_get_extension_data(c, strcmp(name, "DAMAGE") == 0 ? &xcb_damage_id : &xcb_input_id);
    return r && r->present ? r->first_event : 0;
}

/* Subscribe `c` to `family`'s events on the target window. */
static void subscribe(xcb_connection_t *c, const char *family)
{
    uint32_t mask = 0;
    if (strcmp(family, "property") == 0)
        mask = XCB_EVENT_MASK_PROPERTY_CHANGE;
    else if (strcmp(family, "configure") == 0)
        mask = XCB_EVENT_MASK_STRUCTURE_NOTIFY;
    else if (strcmp(family, "motion") == 0)
        mask = XCB_EVENT_MASK_POINTER_MOTION;
    else if (strcmp(family, "key") == 0)
        mask = XCB_EVENT_MASK_KEY_PRESS | XCB_EVENT_MASK_KEY_RELEASE;
    if (mask)
        xcb_change_window_attributes(c, target, XCB_CW_EVENT_MASK, &mask);
    if (strcmp(family, "xi2motion") == 0) {
        free(xcb_input_xi_query_version_reply(c, xcb_input_xi_query_version(c, 2, 2), NULL));
        struct {
            xcb_input_event_mask_t head;
            uint32_t bits;
        } em = {{XCB_INPUT_DEVICE_ALL_MASTER, 1}, XCB_INPUT_XI_EVENT_MASK_MOTION};
        xcb_input_xi_select_events(c, target, 1, &em.head);
    }
    if (strcmp(family, "damage") == 0) {
        free(xcb_damage_query_version_reply(c, xcb_damage_query_version(c, 1, 1), NULL));
        xcb_damage_create(c, xcb_generate_id(c), target, XCB_DAMAGE_REPORT_LEVEL_RAW_RECTANGLES);
    }
    sync_conn(c);
}

/* One stimulus request (two events for key: press then release). */
static void stimulate(const char *family, int i, xcb_gcontext_t gc)
{
    if (strcmp(family, "property") == 0) {
        uint32_t v = (uint32_t)i;
        xcb_change_property(s, XCB_PROP_MODE_REPLACE, target, XCB_ATOM_WM_NAME,
                            XCB_ATOM_CARDINAL, 32, 1, &v);
    } else if (strcmp(family, "configure") == 0) {
        uint32_t x = (uint32_t)(1 + (i & 1));
        xcb_configure_window(s, target, XCB_CONFIG_WINDOW_X, &x);
    } else if (strcmp(family, "motion") == 0 || strcmp(family, "xi2motion") == 0) {
        xcb_test_fake_input(s, XCB_MOTION_NOTIFY, 0, XCB_CURRENT_TIME, root,
                            (int16_t)(tx + (i & 1)), ty, 0);
    } else if (strcmp(family, "key") == 0) {
        xcb_test_fake_input(s, XCB_KEY_PRESS, 38, XCB_CURRENT_TIME, root, 0, 0, 0);
        xcb_test_fake_input(s, XCB_KEY_RELEASE, 38, XCB_CURRENT_TIME, root, 0, 0, 0);
    } else if (strcmp(family, "damage") == 0) {
        xcb_rectangle_t r = {(int16_t)(i & 7), 0, 4, 4};
        xcb_poly_fill_rectangle(s, target, gc, 1, &r);
    }
}

static int is_family_event(const char *family, xcb_generic_event_t *ev, uint8_t damage_base)
{
    uint8_t t = ev->response_type & 0x7f;
    if (strcmp(family, "property") == 0)
        return t == XCB_PROPERTY_NOTIFY;
    if (strcmp(family, "configure") == 0)
        return t == XCB_CONFIGURE_NOTIFY;
    if (strcmp(family, "motion") == 0)
        return t == XCB_MOTION_NOTIFY;
    if (strcmp(family, "key") == 0)
        return t == XCB_KEY_PRESS || t == XCB_KEY_RELEASE;
    if (strcmp(family, "xi2motion") == 0)
        return t == XCB_GE_GENERIC &&
               ((xcb_ge_generic_event_t *)ev)->event_type == XCB_INPUT_MOTION;
    if (strcmp(family, "damage") == 0)
        return damage_base && t == damage_base + XCB_DAMAGE_NOTIFY;
    return 0;
}

static long drain(xcb_connection_t *c, const char *family, uint8_t damage_base)
{
    long n = 0;
    xcb_generic_event_t *ev;
    while ((ev = xcb_poll_for_event(c))) {
        n += is_family_event(family, ev, damage_base);
        free(ev);
    }
    return n;
}

/* Has the server closed `c`? Reads what it holds for up to 3 s of silence. */
static int closed_by_server(xcb_connection_t *c)
{
    struct pollfd p = {xcb_get_file_descriptor(c), POLLIN, 0};
    for (;;) {
        xcb_generic_event_t *ev;
        while ((ev = xcb_poll_for_event(c)))
            free(ev);
        if (xcb_connection_has_error(c))
            return 1;
        if (poll(&p, 1, 3000) <= 0)
            return 0;
    }
}

static void run_family(const char *family)
{
    xcb_connection_t *v = connect_or_die(), *h = connect_or_die();
    const xcb_setup_t *setup = xcb_get_setup(s);
    xcb_screen_t *scr = xcb_setup_roots_iterator(setup).data;
    root = scr->root;

    uint32_t vals[2] = {scr->black_pixel, 1};
    target = xcb_generate_id(s);
    xcb_create_window(s, XCB_COPY_FROM_PARENT, target, root, 0, 0, 200, 200, 0,
                      XCB_WINDOW_CLASS_INPUT_OUTPUT, XCB_COPY_FROM_PARENT,
                      XCB_CW_BACK_PIXEL | XCB_CW_OVERRIDE_REDIRECT, vals);
    xcb_map_window(s, target);
    xcb_gcontext_t gc = xcb_generate_id(s);
    xcb_create_gc(s, gc, target, XCB_GC_FOREGROUND, &scr->white_pixel);
    xcb_test_fake_input(s, XCB_MOTION_NOTIFY, 0, XCB_CURRENT_TIME, root, tx, ty, 0);
    if (strcmp(family, "key") == 0)
        xcb_set_input_focus(s, XCB_INPUT_FOCUS_POINTER_ROOT, target, XCB_CURRENT_TIME);
    sync_conn(s);

    /* V's own marker window: it goes away with V. */
    xcb_window_t marker = xcb_generate_id(v);
    xcb_create_window(v, XCB_COPY_FROM_PARENT, marker, root, 0, 0, 1, 1, 0,
                      XCB_WINDOW_CLASS_INPUT_OUTPUT, XCB_COPY_FROM_PARENT, 0, NULL);
    subscribe(v, family);
    subscribe(h, family);
    uint8_t damage_base = ext_event_base(h, "DAMAGE");

    long expected = 0, got = 0;
    for (int i = 0; i < FLOOD; i++) {
        stimulate(family, i, gc);
        expected += strcmp(family, "key") == 0 ? 2 : 1;
        if ((i + 1) % BATCH == 0) {
            sync_conn(s);
            got += drain(h, family, damage_base);
        }
    }
    sync_conn(s);
    sync_conn(h);
    got += drain(h, family, damage_base);

    xcb_generic_error_t *err = NULL;
    free(xcb_get_window_attributes_reply(s, xcb_get_window_attributes(s, marker), &err));
    int marker_gone = err && err->error_code == XCB_WINDOW;
    free(err);
    int v_closed = closed_by_server(v);
    int server_ok = !xcb_connection_has_error(s) && !xcb_connection_has_error(h);
    /* Damage and motion may legitimately coalesce: H must see at least one per batch. */
    int exact = strcmp(family, "damage") != 0;
    int h_ok = exact ? got == expected : got >= FLOOD / BATCH;
    int ok = marker_gone && v_closed && h_ok && server_ok;
    printf("%-10s %s: slow client %s, its window %s; healthy client %ld/%ld events; server %s\n",
           family, ok ? "PASS" : "FAIL", v_closed ? "disconnected" : "STILL CONNECTED",
           marker_gone ? "freed" : "STILL EXISTS", got, expected, server_ok ? "alive" : "LOST");
    fflush(stdout);
    failures += !ok;

    xcb_disconnect(v);
    xcb_disconnect(h);
    xcb_set_input_focus(s, XCB_INPUT_FOCUS_POINTER_ROOT, XCB_INPUT_FOCUS_POINTER_ROOT,
                        XCB_CURRENT_TIME);
    xcb_free_gc(s, gc);
    xcb_destroy_window(s, target);
    sync_conn(s);
}

int main(int argc, char **argv)
{
    static const char *all[] = {"property", "configure", "motion", "key", "xi2motion", "damage"};
    s = connect_or_die();
    if (argc > 1)
        for (int i = 1; i < argc; i++)
            run_family(argv[i]);
    else
        for (size_t i = 0; i < sizeof all / sizeof *all; i++)
            run_family(all[i]);
    if (xcb_connection_has_error(s)) {
        printf("stimulus connection lost\n");
        return 1;
    }
    FILE *done = fopen("PROBE-DONE", "w");
    if (done)
        fclose(done);
    return failures ? 1 : 0;
}
