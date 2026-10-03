/* MIT-SCREEN-SAVER SetAttributes, as dtsession uses it: client A sets
 * the attributes of the saver window, client B is refused until A unsets
 * them, and ForceScreenSaver shows and removes the window A asked for.
 *
 *   cc -O1 -o screensaver-probe screensaver-probe.c -lxcb -lxcb-screensaver
 */
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <xcb/screensaver.h>
#include <xcb/xcb.h>

static xcb_connection_t *connect_retry(void)
{
    xcb_connection_t *c;
    /* Xorg resets when the previous run's last client leaves. */
    for (int tries = 0; (c = xcb_connect(NULL, NULL)) && xcb_connection_has_error(c) && tries < 50;
         tries++) {
        xcb_disconnect(c);
        usleep(100000);
    }
    return c;
}

static const char *result(xcb_connection_t *c, xcb_void_cookie_t ck)
{
    static char buf[32];
    xcb_generic_error_t *e = xcb_request_check(c, ck);
    if (!e)
        return "ok";
    snprintf(buf, sizeof buf, "error %u", e->error_code);
    free(e);
    return buf;
}

static xcb_window_t saver_window;

static void info(xcb_connection_t *c, xcb_window_t root, const char *when)
{
    xcb_screensaver_query_info_reply_t *r = xcb_screensaver_query_info_reply(
        c, xcb_screensaver_query_info(c, root), NULL);
    if (!r) {
        printf("%s: QueryInfo failed\n", when);
        return;
    }
    if (!saver_window)
        saver_window = r->saver_window;
    printf("%s: state %u kind %u, window %s\n", when, r->state, r->kind,
           r->saver_window == 0 ? "none" : r->saver_window == saver_window ? "the saver's" : "other");
    free(r);
}

static void window(xcb_connection_t *c, const char *when)
{
    xcb_get_window_attributes_reply_t *a = xcb_get_window_attributes_reply(
        c, xcb_get_window_attributes(c, saver_window), NULL);
    xcb_get_geometry_reply_t *g = xcb_get_geometry_reply(c, xcb_get_geometry(c, saver_window), NULL);
    if (!a || !g)
        printf("%s: no saver window\n", when);
    else
        printf("%s: saver window %ux%u+%d+%d map state %u override %u\n", when, g->width,
               g->height, g->x, g->y, a->map_state, a->override_redirect);
    free(a);
    free(g);
}

static void events(xcb_connection_t *c, const char *who)
{
    xcb_flush(c);
    usleep(200000);
    const xcb_query_extension_reply_t *ext = xcb_get_extension_data(c, &xcb_screensaver_id);
    xcb_generic_event_t *e;
    while ((e = xcb_poll_for_event(c))) {
        if ((e->response_type & 0x7f) == ext->first_event) {
            xcb_screensaver_notify_event_t *n = (xcb_screensaver_notify_event_t *)e;
            printf("  %s: ScreenSaverNotify state %u kind %u forced %u window %s\n", who, n->state,
                   n->kind, n->forced, n->window == saver_window ? "the saver's" : "other");
        }
        free(e);
    }
}

int main(void)
{
    xcb_connection_t *a = connect_retry(), *b = connect_retry();
    if (xcb_connection_has_error(a) || xcb_connection_has_error(b)) {
        printf("cannot connect\n");
        return 1;
    }
    xcb_screen_t *s = xcb_setup_roots_iterator(xcb_get_setup(a)).data;
    xcb_window_t root = s->root;
    free(xcb_screensaver_query_version_reply(a, xcb_screensaver_query_version(a, 1, 1), NULL));
    free(xcb_screensaver_query_version_reply(b, xcb_screensaver_query_version(b, 1, 1), NULL));
    info(a, root, "before");
    xcb_screensaver_select_input(a, root, XCB_SCREENSAVER_EVENT_NOTIFY_MASK);
    uint32_t red = 0xff0000;
    printf("A SetAttributes 200x100+10+20: %s\n",
           result(a, xcb_screensaver_set_attributes_checked(a, root, 10, 20, 200, 100, 0,
                                                            XCB_WINDOW_CLASS_COPY_FROM_PARENT, 0,
                                                            0, XCB_CW_BACK_PIXEL, &red)));
    printf("B SetAttributes 1x1: %s\n",
           result(b, xcb_screensaver_set_attributes_checked(b, root, 0, 0, 1, 1, 0,
                                                            XCB_WINDOW_CLASS_COPY_FROM_PARENT, 0,
                                                            0, 0, NULL)));
    printf("A SetAttributes 0x10: %s\n",
           result(a, xcb_screensaver_set_attributes_checked(a, root, 0, 0, 0, 10, 0,
                                                            XCB_WINDOW_CLASS_COPY_FROM_PARENT, 0,
                                                            0, 0, NULL)));
    info(a, root, "attributes set");
    window(a, "before activation");
    xcb_force_screen_saver(a, XCB_SCREEN_SAVER_ACTIVE);
    events(a, "A");
    info(a, root, "activated");
    window(a, "activated");
    xcb_force_screen_saver(a, XCB_SCREEN_SAVER_RESET);
    events(a, "A");
    info(a, root, "reset");
    window(a, "reset");
    printf("A UnsetAttributes: %s\n", result(a, xcb_screensaver_unset_attributes_checked(a, root)));
    printf("B SetAttributes 1x1: %s\n",
           result(b, xcb_screensaver_set_attributes_checked(b, root, 0, 0, 1, 1, 0,
                                                            XCB_WINDOW_CLASS_COPY_FROM_PARENT, 0,
                                                            0, 0, NULL)));
    xcb_force_screen_saver(a, XCB_SCREEN_SAVER_ACTIVE);
    events(a, "A");
    window(a, "B's activated");
    xcb_disconnect(b);
    usleep(300000);
    info(a, root, "B gone");
    window(a, "B gone");
    xcb_force_screen_saver(a, XCB_SCREEN_SAVER_RESET);
    events(a, "A");
    FILE *done = fopen("PROBE-DONE", "w");
    if (done)
        fclose(done);
    xcb_disconnect(a);
    return 0;
}
