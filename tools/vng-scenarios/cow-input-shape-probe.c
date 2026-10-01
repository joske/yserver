/* The COW's input region across a compositor's start-up: Xorg creates the
 * overlay with no input shape (compCreateOverlayWindow), so it takes the
 * pointer over the whole screen until the compositor gives it an input region
 * that misses the pointer (a panel strip, or empty), and again once that
 * region is reset to None.
 *
 *   ./probe <width> <height>
 *
 * An app client maps window A under the centre of the screen; the compositor
 * client takes the COW and selects pointer events on it. In each phase the
 * probe reads the COW's Input rectangles, TranslateCoordinates and QueryPointer
 * at a point inside A, then moves the pointer there, clicks and moves back
 * out to the strip through XTest, and logs which client got which event. probe.log carries no ids or screen
 * coordinates, so Xorg and yserver runs diff directly.
 *
 *   cc -O1 -o probe cow-input-shape-probe.c -lxcb -lxcb-composite \
 *       -lxcb-xfixes -lxcb-shape -lxcb-xtest
 */
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>
#include <xcb/composite.h>
#include <xcb/shape.h>
#include <xcb/xcb.h>
#include <xcb/xfixes.h>
#include <xcb/xtest.h>

static xcb_connection_t *c, *a;
static xcb_window_t root, cow, app;
static uint16_t sw, sh;
static int16_t ax, ay;

static const char *name(xcb_window_t w)
{
    if (w == XCB_NONE)
        return "none";
    if (w == root)
        return "root";
    if (cow && w == cow)
        return "COW";
    if (w == app)
        return "A";
    return "other";
}

static const char *detail(uint8_t d)
{
    static const char *const names[] = {"Ancestor", "Virtual", "Inferior", "Nonlinear",
                                        "NonlinearVirtual"};
    return d < 5 ? names[d] : "?";
}

static void pause_ms(int ms)
{
    struct timespec ts = {ms / 1000, (ms % 1000) * 1000000L};
    nanosleep(&ts, NULL);
}

static void sync_both(void)
{
    free(xcb_get_input_focus_reply(c, xcb_get_input_focus(c), NULL));
    free(xcb_get_input_focus_reply(a, xcb_get_input_focus(a), NULL));
}

static void touch(const char *path)
{
    FILE *f = fopen(path, "w");
    if (f)
        fclose(f);
}

/* Event coordinates relative to the point the pointer was sent to. */
static void at(int16_t ex, int16_t ey, xcb_window_t w, int16_t px, int16_t py)
{
    int16_t ox = w == app ? ax : 0, oy = w == app ? ay : 0;
    if (ex == px - ox && ey == py - oy)
        printf(" at=pointer");
    else
        printf(" at=%+d,%+d", ex - (px - ox), ey - (py - oy));
}

static void log_events(xcb_connection_t *conn, const char *who, int16_t px, int16_t py)
{
    xcb_generic_event_t *ev;
    while ((ev = xcb_poll_for_event(conn))) {
        switch (ev->response_type & 0x7f) {
        case XCB_ENTER_NOTIFY:
        case XCB_LEAVE_NOTIFY: {
            xcb_enter_notify_event_t *e = (void *)ev;
            printf("  %s: %s event=%s child=%s detail=%s mode=%u\n", who,
                   (ev->response_type & 0x7f) == XCB_ENTER_NOTIFY ? "EnterNotify" : "LeaveNotify",
                   name(e->event), name(e->child), detail(e->detail), e->mode);
            break;
        }
        case XCB_MOTION_NOTIFY:
        case XCB_BUTTON_PRESS:
        case XCB_BUTTON_RELEASE: {
            xcb_button_press_event_t *e = (void *)ev;
            const char *t = (ev->response_type & 0x7f) == XCB_MOTION_NOTIFY ? "MotionNotify"
                            : (ev->response_type & 0x7f) == XCB_BUTTON_PRESS ? "ButtonPress"
                                                                             : "ButtonRelease";
            printf("  %s: %s event=%s child=%s", who, t, name(e->event), name(e->child));
            at(e->event_x, e->event_y, e->event, px, py);
            printf("\n");
            break;
        }
        case XCB_MAPPING_NOTIFY:
        case XCB_EXPOSE:
            break;
        case 0:
            printf("  %s: error code=%u\n", who, ((xcb_generic_error_t *)ev)->error_code);
            break;
        default:
            printf("  %s: event type=%u\n", who, ev->response_type & 0x7f);
        }
        free(ev);
    }
}

static void input_rects(void)
{
    xcb_shape_get_rectangles_reply_t *r = xcb_shape_get_rectangles_reply(
        c, xcb_shape_get_rectangles(c, cow, XCB_SHAPE_SK_INPUT), NULL);
    if (!r) {
        printf("  GetRectangles(COW, Input): error\n");
        return;
    }
    xcb_rectangle_t *rs = xcb_shape_get_rectangles_rectangles(r);
    int n = xcb_shape_get_rectangles_rectangles_length(r);
    printf("  GetRectangles(COW, Input): %d", n);
    for (int i = 0; i < n; i++) {
        if (rs[i].x == 0 && rs[i].y == 0 && rs[i].width == sw && rs[i].height == sh)
            printf(" screen");
        else
            printf(" %ux%u%+d%+d", rs[i].width, rs[i].height, rs[i].x, rs[i].y);
    }
    printf("\n");
    free(r);
}

/* Settle the pointer at (4, 4) and log as "settle" what crossed since the
 * last phase, the tree change under the still pointer included. Then probe
 * the point (px, py), inside A:
 * tree answers first, then real input, then back to (5, 5). */
static void phase(const char *what, int shaped, int16_t px, int16_t py)
{
    printf("%s\n", what);
    xcb_test_fake_input(c, XCB_MOTION_NOTIFY, 0, XCB_CURRENT_TIME, root, 4, 4, 0);
    sync_both();
    pause_ms(200);
    sync_both();
    log_events(a, "settle app", 4, 4);
    log_events(c, "settle compositor", 4, 4);
    if (shaped)
        input_rects();
    xcb_translate_coordinates_reply_t *t =
        xcb_translate_coordinates_reply(a, xcb_translate_coordinates(a, root, root, px, py), NULL);
    printf("  TranslateCoordinates(root, root) child=%s\n", t ? name(t->child) : "error");
    free(t);

    xcb_test_fake_input(c, XCB_MOTION_NOTIFY, 0, XCB_CURRENT_TIME, root, px, py, 0);
    sync_both();
    pause_ms(200);
    sync_both();
    xcb_query_pointer_reply_t *q = xcb_query_pointer_reply(a, xcb_query_pointer(a, root), NULL);
    printf("  QueryPointer(root) child=%s\n", q ? name(q->child) : "error");
    free(q);
    xcb_test_fake_input(c, XCB_BUTTON_PRESS, 1, XCB_CURRENT_TIME, root, 0, 0, 0);
    xcb_test_fake_input(c, XCB_BUTTON_RELEASE, 1, XCB_CURRENT_TIME, root, 0, 0, 0);
    sync_both();
    pause_ms(200);
    sync_both();
    log_events(a, "app", px, py);
    log_events(c, "compositor", px, py);
    xcb_test_fake_input(c, XCB_MOTION_NOTIFY, 0, XCB_CURRENT_TIME, root, 5, 5, 0);
    sync_both();
    pause_ms(200);
    sync_both();
    log_events(a, "app", 5, 5);
    log_events(c, "compositor", 5, 5);
    fflush(stdout);
}

static void set_input_region(xcb_rectangle_t *r, uint32_t n)
{
    xcb_xfixes_region_t region = xcb_generate_id(c);
    xcb_xfixes_create_region(c, region, n, r);
    xcb_xfixes_set_window_shape_region(c, cow, XCB_SHAPE_SK_INPUT, 0, 0, region);
    xcb_xfixes_destroy_region(c, region);
    sync_both();
}

int main(int argc, char **argv)
{
    if (argc < 3)
        return 2;
    sw = (uint16_t)atoi(argv[1]);
    sh = (uint16_t)atoi(argv[2]);
    c = xcb_connect(NULL, NULL);
    a = xcb_connect(NULL, NULL);
    if (xcb_connection_has_error(c) || xcb_connection_has_error(a))
        return 1;
    xcb_screen_t *s = xcb_setup_roots_iterator(xcb_get_setup(c)).data;
    root = s->root;
    free(xcb_composite_query_version_reply(c, xcb_composite_query_version(c, 0, 4), NULL));
    free(xcb_xfixes_query_version_reply(c, xcb_xfixes_query_version(c, 5, 0), NULL));
    free(xcb_shape_query_version_reply(c, xcb_shape_query_version(c), NULL));

    ax = (int16_t)(sw / 4);
    ay = (int16_t)(sh / 4);
    app = xcb_generate_id(a);
    uint32_t av[] = {0x3060c0u, 1,
                     XCB_EVENT_MASK_BUTTON_PRESS | XCB_EVENT_MASK_BUTTON_RELEASE |
                         XCB_EVENT_MASK_ENTER_WINDOW | XCB_EVENT_MASK_LEAVE_WINDOW |
                         XCB_EVENT_MASK_POINTER_MOTION};
    xcb_create_window(a, s->root_depth, app, root, ax, ay, sw / 2, sh / 2, 0,
                      XCB_WINDOW_CLASS_INPUT_OUTPUT, s->root_visual,
                      XCB_CW_BACK_PIXEL | XCB_CW_OVERRIDE_REDIRECT | XCB_CW_EVENT_MASK, av);
    xcb_map_window(a, app);
    sync_both();
    phase("without the COW", 0, (int16_t)(sw / 2), (int16_t)(sh / 2));

    xcb_composite_get_overlay_window_reply_t *ow =
        xcb_composite_get_overlay_window_reply(c, xcb_composite_get_overlay_window(c, root), NULL);
    if (!ow)
        return 1;
    cow = ow->overlay_win;
    free(ow);
    uint32_t cm = XCB_EVENT_MASK_BUTTON_PRESS | XCB_EVENT_MASK_BUTTON_RELEASE |
                  XCB_EVENT_MASK_ENTER_WINDOW | XCB_EVENT_MASK_LEAVE_WINDOW |
                  XCB_EVENT_MASK_POINTER_MOTION;
    xcb_change_window_attributes(c, cow, XCB_CW_EVENT_MASK, &cm);
    sync_both();
    phase("COW taken, no input shape set", 1, (int16_t)(sw / 2 + 10), (int16_t)(sh / 2 + 10));

    /* A panel strip above A, as a compositor's chrome. */
    xcb_rectangle_t strip = {0, 0, sw, (uint16_t)(sh / 8)};
    set_input_region(&strip, 1);
    phase("compositor set a strip COW input region", 1, (int16_t)(sw / 2 + 20),
          (int16_t)(sh / 2 + 20));

    set_input_region(NULL, 0);
    phase("compositor set an empty COW input region", 1, (int16_t)(sw / 2 + 30),
          (int16_t)(sh / 2 + 30));

    xcb_xfixes_set_window_shape_region(c, cow, XCB_SHAPE_SK_INPUT, 0, 0, XCB_NONE);
    sync_both();
    phase("COW input region reset to None", 1, (int16_t)(sw / 2 + 40), (int16_t)(sh / 2 + 40));

    xcb_composite_release_overlay_window(c, root);
    sync_both();
    phase("COW released", 0, (int16_t)(sw / 2 + 50), (int16_t)(sh / 2 + 50));
    printf("done\n");
    fflush(stdout);
    touch("PROBE-DONE");
    xcb_disconnect(a);
    xcb_disconnect(c);
    return 0;
}
