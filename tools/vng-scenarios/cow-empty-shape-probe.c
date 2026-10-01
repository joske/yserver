/* Cinnamon's lock screen, reduced: muffin keeps Presenting its stage (a child
 * of the Composite Overlay Window) while it shapes the COW to an EMPTY
 * Bounding region and unredirects the fullscreen locker. An empty Bounding
 * shape clips the COW and every descendant to nothing (miComputeClips), so
 * the locker below must be on screen, not the stage.
 *
 *   ./probe <width> <height>
 *
 * Phases, each announced as READY-<n> with the expected colour in
 * expect-<n>; the stage keeps Presenting until the host touches DONE-<n>:
 *   0  stage shown (the locker is redirected)              expect STAGE
 *   1  COW Bounding = empty region, locker unredirected     expect LOCKER
 *   2  COW Bounding = None, locker unmapped (the unlock)    expect STAGE
 * The stage Presents a DRI3-imported pixmap when the server exports one
 * (the direct-scanout path), a server pixmap otherwise. probe.log carries
 * no ids, so Xorg and yserver runs diff directly.
 *
 *   cc -O1 -o probe cow-empty-shape-probe.c -lxcb -lxcb-composite \
 *       -lxcb-xfixes -lxcb-shape -lxcb-present -lxcb-dri3
 */
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>
#include <xcb/composite.h>
#include <xcb/dri3.h>
#include <xcb/present.h>
#include <xcb/shape.h>
#include <xcb/xcb.h>
#include <xcb/xfixes.h>

#define STAGE 0x20c040u
#define LOCKER 0xc02060u

static xcb_connection_t *c;
static xcb_window_t stage;
static xcb_pixmap_t frames[2];
static int busy[2];
static uint32_t serial;
static int have_dri3;

static void check(xcb_void_cookie_t ck, const char *what)
{
    xcb_generic_error_t *e = xcb_request_check(c, ck);
    printf("%s: %s", what, e ? "error" : "ok");
    if (e)
        printf(" code=%u", e->error_code);
    printf("\n");
    fflush(stdout);
    free(e);
}

static int exists(const char *fmt, int n)
{
    char path[32];
    snprintf(path, sizeof path, fmt, n);
    return access(path, F_OK) == 0;
}

static void touch(const char *fmt, int n, const char *body)
{
    char path[32];
    snprintf(path, sizeof path, fmt, n);
    FILE *f = fopen(path, "w");
    if (f) {
        fputs(body, f);
        fclose(f);
    }
}

static void drain(void)
{
    xcb_generic_event_t *ev;
    while ((ev = xcb_poll_for_event(c))) {
        if ((ev->response_type & 0x7f) == XCB_GE_GENERIC) {
            xcb_present_generic_event_t *pe = (void *)ev;
            if (pe->evtype == XCB_PRESENT_EVENT_IDLE_NOTIFY) {
                xcb_present_idle_notify_event_t *idle = (void *)ev;
                for (int i = 0; i < 2; i++)
                    if (frames[i] == idle->pixmap)
                        busy[i] = 0;
            }
        }
        free(ev);
    }
}

/* One Present of whichever frame is idle, then ~16 ms of event draining. */
static void present_once(void)
{
    drain();
    for (int i = 0; i < 2; i++) {
        if (busy[i])
            continue;
        busy[i] = 1;
        xcb_present_pixmap(c, stage, frames[i], ++serial, 0, 0, 0, 0, 0, 0, 0,
                           XCB_PRESENT_OPTION_NONE, 0, 0, 0, 0, NULL);
        break;
    }
    xcb_flush(c);
    struct timespec ts = {0, 16 * 1000 * 1000};
    nanosleep(&ts, NULL);
}

static void present_for(int ms)
{
    for (int t = 0; t < ms; t += 16)
        present_once();
}

static void phase(int n, uint32_t colour)
{
    char body[16];
    present_for(1500);
    snprintf(body, sizeof body, "%06x\n", colour);
    touch("expect-%d", n, body);
    touch("READY-%d", n, "");
    while (!exists("DONE-%d", n))
        present_once();
}

/* A frame filled with STAGE; imported through DRI3 when the server can
 * export it, so the Present takes the server's dma-buf path. */
static xcb_pixmap_t make_frame(xcb_window_t root, uint8_t depth, uint16_t w, uint16_t h,
                               int *imported)
{
    xcb_pixmap_t p = xcb_generate_id(c);
    xcb_create_pixmap(c, depth, p, root, w, h);
    xcb_gcontext_t gc = xcb_generate_id(c);
    uint32_t fg = STAGE;
    xcb_create_gc(c, gc, p, XCB_GC_FOREGROUND, &fg);
    xcb_rectangle_t r = {0, 0, w, h};
    xcb_poly_fill_rectangle(c, p, gc, 1, &r);
    xcb_free_gc(c, gc);
    *imported = 0;
    if (!have_dri3)
        return p;
    /* 1.2: the reply names the modifier, so the layout is explicit. */
    xcb_dri3_buffers_from_pixmap_reply_t *b =
        xcb_dri3_buffers_from_pixmap_reply(c, xcb_dri3_buffers_from_pixmap(c, p), NULL);
    if (!b)
        return p;
    if (b->nfd != 1) {
        free(b);
        return p;
    }
    int32_t fd = xcb_dri3_buffers_from_pixmap_reply_fds(c, b)[0];
    xcb_pixmap_t q = xcb_generate_id(c);
    xcb_void_cookie_t ck = xcb_dri3_pixmap_from_buffers_checked(
        c, q, root, 1, b->width, b->height, xcb_dri3_buffers_from_pixmap_strides(b)[0],
        xcb_dri3_buffers_from_pixmap_offsets(b)[0], 0, 0, 0, 0, 0, 0, b->depth, b->bpp,
        b->modifier, &fd);
    xcb_generic_error_t *e = xcb_request_check(c, ck);
    free(b);
    if (e) {
        free(e);
        return p;
    }
    xcb_free_pixmap(c, p);
    *imported = 1;
    return q;
}

int main(int argc, char **argv)
{
    if (argc < 3)
        return 2;
    uint16_t w = (uint16_t)atoi(argv[1]), h = (uint16_t)atoi(argv[2]);
    c = xcb_connect(NULL, NULL);
    if (xcb_connection_has_error(c))
        return 1;
    xcb_screen_t *s = xcb_setup_roots_iterator(xcb_get_setup(c)).data;
    xcb_window_t root = s->root;
    free(xcb_composite_query_version_reply(c, xcb_composite_query_version(c, 0, 4), NULL));
    free(xcb_xfixes_query_version_reply(c, xcb_xfixes_query_version(c, 5, 0), NULL));
    free(xcb_present_query_version_reply(c, xcb_present_query_version(c, 1, 2), NULL));
    /* A request to an absent extension closes the connection. */
    have_dri3 = xcb_get_extension_data(c, &xcb_dri3_id)->present;
    if (have_dri3)
        free(xcb_dri3_query_version_reply(c, xcb_dri3_query_version(c, 1, 2), NULL));

    check(xcb_composite_redirect_subwindows_checked(c, root, XCB_COMPOSITE_REDIRECT_MANUAL),
          "RedirectSubwindows(root, Manual)");
    xcb_composite_get_overlay_window_reply_t *ow =
        xcb_composite_get_overlay_window_reply(c, xcb_composite_get_overlay_window(c, root), NULL);
    if (!ow)
        return 1;
    xcb_window_t cow = ow->overlay_win;
    free(ow);
    printf("GetOverlayWindow: ok\n");

    stage = xcb_generate_id(c);
    uint32_t sv[] = {0};
    xcb_create_window(c, s->root_depth, stage, cow, 0, 0, w, h, 0, XCB_WINDOW_CLASS_INPUT_OUTPUT,
                      s->root_visual, XCB_CW_BACK_PIXEL, sv);
    xcb_present_select_input(c, xcb_generate_id(c), stage,
                             XCB_PRESENT_EVENT_MASK_COMPLETE_NOTIFY |
                                 XCB_PRESENT_EVENT_MASK_IDLE_NOTIFY);
    check(xcb_map_window_checked(c, stage), "map stage (child of the COW)");

    xcb_window_t locker = xcb_generate_id(c);
    uint32_t lv[] = {LOCKER, 1};
    xcb_create_window(c, s->root_depth, locker, root, 0, 0, w, h, 0,
                      XCB_WINDOW_CLASS_INPUT_OUTPUT, s->root_visual,
                      XCB_CW_BACK_PIXEL | XCB_CW_OVERRIDE_REDIRECT, lv);
    check(xcb_map_window_checked(c, locker), "map locker (override-redirect, fullscreen)");
    xcb_gcontext_t lgc = xcb_generate_id(c);
    uint32_t lfg = LOCKER;
    xcb_create_gc(c, lgc, locker, XCB_GC_FOREGROUND, &lfg);
    xcb_rectangle_t lr = {0, 0, w, h};
    xcb_poly_fill_rectangle(c, locker, lgc, 1, &lr);

    int imported = 0;
    for (int i = 0; i < 2; i++)
        frames[i] = make_frame(root, s->root_depth, w, h, &imported);
    /* Not in probe.log: Xorg's shadow-fb guest has no DRI3. */
    touch("IMPORTED-%d", imported, "");
    phase(0, STAGE);

    xcb_rectangle_t full = {0, 0, w, h};
    xcb_xfixes_region_t region = xcb_generate_id(c);
    xcb_xfixes_create_region(c, region, 1, &full);
    check(xcb_xfixes_invert_region_checked(c, region, full, region), "InvertRegion -> empty");
    check(xcb_xfixes_set_window_shape_region_checked(c, cow, XCB_SHAPE_SK_BOUNDING, 0, 0, region),
          "SetWindowShapeRegion(COW, Bounding, empty)");
    check(xcb_composite_unredirect_window_checked(c, locker, XCB_COMPOSITE_REDIRECT_MANUAL),
          "UnredirectWindow(locker)");
    xcb_poly_fill_rectangle(c, locker, lgc, 1, &lr);
    phase(1, LOCKER);

    check(xcb_xfixes_set_window_shape_region_checked(c, cow, XCB_SHAPE_SK_BOUNDING, 0, 0,
                                                     XCB_NONE),
          "SetWindowShapeRegion(COW, Bounding, None)");
    check(xcb_unmap_window_checked(c, locker), "unmap locker");
    phase(2, STAGE);

    printf("done\n");
    touch("PHASES-DONE-%d", 2, "");
    xcb_disconnect(c);
    return 0;
}
