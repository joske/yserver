/* Exposures of tree changes other than a map: what a restack, a move, a
 * resize, an unmap, a destroy, a reparent, a shape change or a
 * CirculateWindow uncovers is exposed to the windows it uncovers, as Xorg's
 * ValidateTree + HandleExposures does (#213: awesome maps mpv's frame under
 * the terminal and raises it; mpv draws its cover art only on Expose).
 *
 *   ./probe direct      # no compositor
 *   ./probe automatic   # RedirectSubwindows(root, Automatic)
 *   ./probe manual      # RedirectSubwindows(root, Manual)
 *
 * Every Expose the probe gets is logged in arrival order, with the window's
 * name. A window whose background is None repaints its fill colour over each
 * exposed rect, as a client does; a window with a background pixel repaints
 * only on its first exposure, so a later exposure shows the background the
 * server painted. Pixels are read from the windows at points that are
 * visible at that moment.
 *
 *   cc -O1 -o probe restack-expose-probe.c -lxcb -lxcb-composite -lxcb-shape
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <xcb/composite.h>
#include <xcb/shape.h>
#include <xcb/xcb.h>

#define RED 0xff0000u
#define YELLOW 0xffff00u
#define BLUE 0x0000ffu
#define GREEN 0x00ff00u
#define GRAY 0x808080u
#define MAGENTA 0xff00ffu
#define CYAN 0x00ffffu
#define ORANGE 0xff8000u
#define WHITE 0xffffffu

static xcb_connection_t *c;
static xcb_screen_t *s;

struct win {
    xcb_window_t id;
    const char *name;
    uint32_t fill;  /* what the client draws on Expose */
    int redraw;     /* 1: every Expose; 0: the first batch only */
    int drawn;
    xcb_gcontext_t gc;
};
static struct win wins[32];
static int nwins;

static void sync_server(void)
{
    free(xcb_get_input_focus_reply(c, xcb_get_input_focus(c), NULL));
}

static struct win *find(xcb_window_t id)
{
    for (int i = 0; i < nwins; i++)
        if (wins[i].id == id)
            return &wins[i];
    return NULL;
}

static const char *colour(uint32_t v)
{
    static char hex[16];
    switch (v) {
    case RED: return "red";
    case YELLOW: return "yellow";
    case BLUE: return "blue";
    case GREEN: return "green";
    case GRAY: return "gray";
    case MAGENTA: return "magenta";
    case CYAN: return "cyan";
    case ORANGE: return "orange";
    case WHITE: return "white";
    }
    snprintf(hex, sizeof hex, "%06x", v);
    return hex;
}

/* bg < 0: background None. */
static xcb_window_t window(const char *name, xcb_window_t parent, int x, int y, int w, int h,
                           int bg, uint32_t fill, int redraw)
{
    xcb_window_t id = xcb_generate_id(c);
    uint32_t v[2] = {bg >= 0 ? (uint32_t)bg : XCB_BACK_PIXMAP_NONE, XCB_EVENT_MASK_EXPOSURE};
    uint32_t mask = (bg >= 0 ? XCB_CW_BACK_PIXEL : XCB_CW_BACK_PIXMAP) | XCB_CW_EVENT_MASK;
    xcb_create_window(c, XCB_COPY_FROM_PARENT, id, parent, x, y, w, h, 0,
                      XCB_WINDOW_CLASS_INPUT_OUTPUT, XCB_COPY_FROM_PARENT, mask, v);
    struct win *e = &wins[nwins++];
    e->id = id;
    e->name = name;
    e->fill = fill;
    e->redraw = redraw;
    e->drawn = 0;
    e->gc = xcb_generate_id(c);
    xcb_create_gc(c, e->gc, id, XCB_GC_FOREGROUND, &fill);
    return id;
}

/* Log every Expose queued so far and let the windows repaint. */
static void drain(const char *when)
{
    sync_server();
    printf("%s:\n", when);
    int any = 0;
    xcb_generic_event_t *ev;
    while ((ev = xcb_poll_for_event(c))) {
        if ((ev->response_type & 0x7f) == XCB_EXPOSE) {
            xcb_expose_event_t *x = (xcb_expose_event_t *)ev;
            struct win *w = find(x->window);
            printf("  Expose %s %d,%d %ux%u count=%u\n", w ? w->name : "?", x->x, x->y,
                   x->width, x->height, x->count);
            any = 1;
            if (w && (w->redraw || !w->drawn)) {
                xcb_rectangle_t r = {(int16_t)x->x, (int16_t)x->y, x->width, x->height};
                xcb_poly_fill_rectangle(c, w->id, w->gc, 1, &r);
                if (x->count == 0)
                    w->drawn = 1;
            }
        } else if (ev->response_type == 0) {
            xcb_generic_error_t *e = (xcb_generic_error_t *)ev;
            printf("  error %u major %u\n", e->error_code, e->major_code);
        }
        free(ev);
    }
    if (!any)
        printf("  no Expose\n");
    sync_server();
    fflush(stdout);
}

static void pix(xcb_window_t w, int x, int y)
{
    xcb_get_image_reply_t *r = xcb_get_image_reply(
        c, xcb_get_image(c, XCB_IMAGE_FORMAT_Z_PIXMAP, w, x, y, 1, 1, ~0u), NULL);
    struct win *n = find(w);
    if (!r) {
        printf("  %s@%d,%d: no image\n", n ? n->name : "?", x, y);
        return;
    }
    printf("  %s@%d,%d: %s\n", n ? n->name : "?", x, y,
           colour(*(uint32_t *)xcb_get_image_data(r) & 0xffffff));
    free(r);
    fflush(stdout);
}

static void stack(xcb_window_t w, uint32_t mode)
{
    xcb_configure_window(c, w, XCB_CONFIG_WINDOW_STACK_MODE, &mode);
}

static void stack_sibling(xcb_window_t w, xcb_window_t sib, uint32_t mode)
{
    uint32_t v[2] = {sib, mode};
    xcb_configure_window(c, w, XCB_CONFIG_WINDOW_SIBLING | XCB_CONFIG_WINDOW_STACK_MODE, v);
}

static void move(xcb_window_t w, int x, int y)
{
    uint32_t v[2] = {(uint32_t)x, (uint32_t)y};
    xcb_configure_window(c, w, XCB_CONFIG_WINDOW_X | XCB_CONFIG_WINDOW_Y, v);
}

static void resize(xcb_window_t w, int width, int height)
{
    uint32_t v[2] = {(uint32_t)width, (uint32_t)height};
    xcb_configure_window(c, w, XCB_CONFIG_WINDOW_WIDTH | XCB_CONFIG_WINDOW_HEIGHT, v);
}

/* The #213 sequence and its variations, on top-levels. */
static void top_levels(void)
{
    xcb_window_t b = window("B", s->root, 40, 40, 200, 150, RED, YELLOW, 0);
    xcb_map_window(c, b);
    drain("B mapped");
    pix(b, 100, 75);

    /* mpv's frame: background None, mapped under the terminal. */
    xcb_window_t a = window("A", s->root, 40, 40, 200, 150, -1, BLUE, 1);
    stack(a, XCB_STACK_MODE_BELOW);
    xcb_map_window(c, a);
    drain("A mapped under B");

    stack(a, XCB_STACK_MODE_ABOVE);
    drain("A raised");
    pix(a, 100, 75);

    stack(a, XCB_STACK_MODE_BELOW);
    drain("A lowered");
    pix(b, 100, 75);

    xcb_window_t cw = window("C", s->root, 180, 120, 120, 100, GREEN, ORANGE, 0);
    xcb_map_window(c, cw);
    drain("C mapped over A and B");
    stack_sibling(a, cw, XCB_STACK_MODE_BELOW);
    drain("A raised to just below C");
    pix(a, 20, 20);

    stack(a, XCB_STACK_MODE_ABOVE);
    drain("A raised over C");
    pix(a, 160, 100);

    stack(a, XCB_STACK_MODE_BELOW);
    drain("A lowered under B and C");
    pix(b, 20, 20);
    pix(cw, 20, 20);

    xcb_unmap_window(c, b);
    drain("B unmapped");
    pix(a, 20, 20);

    xcb_map_window(c, b);
    drain("B mapped again");
    move(b, 70, 60);
    drain("B moved by 30,20");
    pix(a, 10, 10);

    resize(b, 230, 170);
    drain("B grown to 230x170");
    resize(b, 150, 100);
    drain("B shrunk to 150x100");
    pix(a, 190, 40);

    /* An occluder that leaves without an unmap first. */
    xcb_destroy_window(c, cw);
    drain("C destroyed");

    xcb_unmap_window(c, a);
    xcb_unmap_window(c, b);
    drain("A and B unmapped");
}

/* The same rules inside a top-level, and CirculateWindow, a shape change
 * and a reparent, which need a parent of their own. */
static void subwindows(void)
{
    xcb_window_t p = window("P", s->root, 360, 20, 260, 200, GRAY, GRAY, 0);
    xcb_window_t s1 = window("S1", p, 10, 10, 120, 90, -1, BLUE, 1);
    xcb_window_t s2 = window("S2", p, 60, 50, 120, 90, MAGENTA, WHITE, 0);
    xcb_window_t s3 = window("S3", p, 150, 100, 100, 90, CYAN, ORANGE, 0);
    xcb_map_subwindows(c, p);
    xcb_map_window(c, p);
    drain("P mapped");

    stack(s1, XCB_STACK_MODE_ABOVE);
    drain("S1 raised");
    pix(s1, 100, 70);

    xcb_circulate_window(c, XCB_CIRCULATE_RAISE_LOWEST, p);
    drain("circulate P RaiseLowest");
    pix(s2, 10, 10);

    xcb_circulate_window(c, XCB_CIRCULATE_LOWER_HIGHEST, p);
    drain("circulate P LowerHighest");
    pix(s3, 20, 10);

    move(s3, 120, 110);
    drain("S3 moved by -30,10");
    pix(s2, 115, 55);

    xcb_rectangle_t half = {0, 0, 60, 90};
    xcb_shape_rectangles(c, XCB_SHAPE_SO_SET, XCB_SHAPE_SK_BOUNDING, XCB_CLIP_ORDERING_UNSORTED,
                         s2, 0, 0, 1, &half);
    drain("S2 shaped to its left half");
    xcb_shape_mask(c, XCB_SHAPE_SO_SET, XCB_SHAPE_SK_BOUNDING, s2, 0, 0, XCB_NONE);
    drain("S2 shape removed");

    xcb_unmap_window(c, s1);
    drain("S1 unmapped");

    xcb_reparent_window(c, s3, s->root, 360, 260);
    drain("S3 reparented to the root");
    pix(s3, 50, 45);

    xcb_destroy_window(c, s3);
    xcb_unmap_window(c, p);
    drain("S3 destroyed, P unmapped");
}

int main(int argc, char **argv)
{
    const char *mode = argc > 1 ? argv[1] : "direct";
    c = xcb_connect(NULL, NULL);
    if (xcb_connection_has_error(c)) {
        printf("cannot connect\n");
        return 1;
    }
    s = xcb_setup_roots_iterator(xcb_get_setup(c)).data;
    free(xcb_shape_query_version_reply(c, xcb_shape_query_version(c), NULL));
    if (strcmp(mode, "direct") != 0) {
        free(xcb_composite_query_version_reply(c, xcb_composite_query_version(c, 0, 4), NULL));
        xcb_composite_redirect_subwindows(c, s->root,
                                          strcmp(mode, "manual") == 0
                                              ? XCB_COMPOSITE_REDIRECT_MANUAL
                                              : XCB_COMPOSITE_REDIRECT_AUTOMATIC);
    }
    printf("mode %s\n", mode);
    top_levels();
    subwindows();

    FILE *done = fopen("PROBE-DONE", "w");
    if (done)
        fclose(done);
    xcb_disconnect(c);
    return 0;
}
