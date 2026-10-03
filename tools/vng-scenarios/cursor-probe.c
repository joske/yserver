/* Which cursor the sprite shows, read back with XFixesGetCursorImage: over
 * the bare root (nobody set a root cursor, so the server's default), and
 * over InputOutput and InputOnly children with a cursor of their own, as
 * CDE's dtwm puts on its frame resize handles. Also the crossing events and
 * a click the InputOnly child gets, and what is left once its input shape
 * is emptied.
 *
 *   ./cursor-probe
 *
 *   cc -O1 -o cursor-probe cursor-probe.c -lxcb -lxcb-xfixes -lxcb-xtest -lxcb-shape
 */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <xcb/shape.h>
#include <xcb/xcb.h>
#include <xcb/xfixes.h>
#include <xcb/xtest.h>

static xcb_connection_t *a, *b;
static xcb_window_t root, io_child, io_only;

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

static void sync_server(xcb_connection_t *c)
{
    free(xcb_get_input_focus_reply(c, xcb_get_input_focus(c), NULL));
}

static const char *name_of(xcb_window_t w)
{
    return w == io_child ? "IO" : w == io_only ? "InputOnly" : w == root ? "root" : "other";
}

/* The cursor image: size, hotspot, and an FNV-1a hash of its ARGB pixels. */
static void cursor(const char *when)
{
    usleep(300000);
    xcb_xfixes_get_cursor_image_reply_t *r =
        xcb_xfixes_get_cursor_image_reply(a, xcb_xfixes_get_cursor_image(a), NULL);
    if (!r) {
        printf("%s: cursor: no reply\n", when);
        return;
    }
    const uint32_t *px = xcb_xfixes_get_cursor_image_cursor_image(r);
    uint64_t h = 0xcbf29ce484222325ull;
    for (int i = 0; i < r->width * r->height; i++) {
        h ^= px[i];
        h *= 0x100000001b3ull;
    }
    printf("%s: cursor %ux%u hot %u,%u pixels %016llx\n", when, r->width, r->height, r->xhot,
           r->yhot, (unsigned long long)h);
    free(r);
}

static void events(const char *when)
{
    sync_server(a);
    printf("%s:", when);
    int any = 0;
    xcb_generic_event_t *e;
    while ((e = xcb_poll_for_event(a))) {
        any = 1;
        switch (e->response_type & 0x7f) {
        case XCB_ENTER_NOTIFY:
        case XCB_LEAVE_NOTIFY: {
            xcb_enter_notify_event_t *n = (xcb_enter_notify_event_t *)e;
            printf(" %s(%s detail %u)",
                   (e->response_type & 0x7f) == XCB_ENTER_NOTIFY ? "Enter" : "Leave",
                   name_of(n->event), n->detail);
            break;
        }
        case XCB_BUTTON_PRESS:
        case XCB_BUTTON_RELEASE: {
            xcb_button_press_event_t *p = (xcb_button_press_event_t *)e;
            printf(" %s(%s at %d,%d)",
                   (e->response_type & 0x7f) == XCB_BUTTON_PRESS ? "ButtonPress" : "ButtonRelease",
                   name_of(p->event), p->event_x, p->event_y);
            break;
        }
        case 0:
            printf(" error %u", ((xcb_generic_error_t *)e)->error_code);
            break;
        default:
            printf(" event %u", e->response_type & 0x7f);
        }
        free(e);
    }
    printf("%s\n", any ? "" : " none");
    fflush(stdout);
}

static void move(int x, int y)
{
    xcb_test_fake_input(b, XCB_MOTION_NOTIFY, 0, XCB_CURRENT_TIME, root, x, y, 0);
    sync_server(b);
}

static void click(void)
{
    xcb_test_fake_input(b, XCB_BUTTON_PRESS, 1, XCB_CURRENT_TIME, XCB_NONE, 0, 0, 0);
    xcb_test_fake_input(b, XCB_BUTTON_RELEASE, 1, XCB_CURRENT_TIME, XCB_NONE, 0, 0, 0);
    sync_server(b);
}

static xcb_cursor_t glyph_cursor(xcb_font_t font, uint16_t glyph)
{
    xcb_cursor_t c = xcb_generate_id(a);
    xcb_create_glyph_cursor(a, c, font, font, glyph, glyph + 1, 0, 0, 0, 0xffff, 0xffff, 0xffff);
    return c;
}

int main(void)
{
    a = connect_retry();
    b = connect_retry();
    if (xcb_connection_has_error(a) || xcb_connection_has_error(b)) {
        printf("cannot connect\n");
        return 1;
    }
    xcb_screen_t *s = xcb_setup_roots_iterator(xcb_get_setup(a)).data;
    root = s->root;
    free(xcb_xfixes_query_version_reply(a, xcb_xfixes_query_version(a, 4, 0), NULL));
    free(xcb_test_get_version_reply(b, xcb_test_get_version(b, 2, 2), NULL));

    move(900, 600);
    cursor("bare root");

    xcb_font_t font = xcb_generate_id(a);
    xcb_open_font(a, font, 6, "cursor");
    xcb_cursor_t fleur = glyph_cursor(font, 52), corner = glyph_cursor(font, 134);

    /* W: a plain InputOutput parent with no cursor. */
    xcb_window_t w = xcb_generate_id(a);
    uint32_t wvals[1] = {s->white_pixel};
    xcb_create_window(a, XCB_COPY_FROM_PARENT, w, root, 0, 0, 300, 300, 0,
                      XCB_WINDOW_CLASS_INPUT_OUTPUT, s->root_visual, XCB_CW_BACK_PIXEL, wvals);
    uint32_t mask = XCB_EVENT_MASK_ENTER_WINDOW | XCB_EVENT_MASK_LEAVE_WINDOW |
                    XCB_EVENT_MASK_BUTTON_PRESS | XCB_EVENT_MASK_BUTTON_RELEASE;
    io_child = xcb_generate_id(a);
    uint32_t iovals[3] = {s->black_pixel, mask, fleur};
    xcb_create_window(a, XCB_COPY_FROM_PARENT, io_child, w, 20, 20, 40, 40, 0,
                      XCB_WINDOW_CLASS_INPUT_OUTPUT, s->root_visual,
                      XCB_CW_BACK_PIXEL | XCB_CW_EVENT_MASK | XCB_CW_CURSOR, iovals);
    io_only = xcb_generate_id(a);
    uint32_t onlyvals[2] = {mask, corner};
    xcb_create_window(a, 0, io_only, w, 100, 20, 40, 40, 0, XCB_WINDOW_CLASS_INPUT_ONLY,
                      XCB_COPY_FROM_PARENT, XCB_CW_EVENT_MASK | XCB_CW_CURSOR, onlyvals);
    xcb_map_subwindows(a, w);
    xcb_map_window(a, w);
    sync_server(a);
    usleep(300000);

    move(200, 200);
    events("over W");
    cursor("over W");
    move(40, 40);
    events("over the IO child");
    cursor("over the IO child");
    move(120, 40);
    events("over the InputOnly child");
    cursor("over the InputOnly child");
    click();
    events("clicked the InputOnly child");
    move(200, 200);
    events("back over W");
    cursor("back over W");

    /* An empty input shape takes the InputOnly child out of hit-testing. */
    xcb_shape_rectangles(a, XCB_SHAPE_SO_SET, XCB_SHAPE_SK_INPUT, XCB_CLIP_ORDERING_UNSORTED,
                         io_only, 0, 0, 0, NULL);
    sync_server(a);
    move(120, 40);
    events("over the input-shaped-away InputOnly child");
    cursor("over the input-shaped-away InputOnly child");

    xcb_destroy_window(a, w);
    sync_server(a);
    move(900, 600);
    cursor("bare root again");

    FILE *done = fopen("PROBE-DONE", "w");
    if (done)
        fclose(done);
    xcb_disconnect(b);
    xcb_disconnect(a);
    return 0;
}
