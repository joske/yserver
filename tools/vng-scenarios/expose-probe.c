/* Which Expose and GraphicsExpose events a server sends, and where it
 * paints the background for them.
 *
 *   ./expose-probe redirect | direct
 *
 * CopyArea: the windows of child-clip-probe.c — a frame F (200x150) with
 * a title bar T, a client C (5,20 190x100), a button bar B and above
 * them H (130,15 30x20) over C's top edge; C has a child V (100,10
 * 80x120). Each copy, with graphics-exposures on, first fills C orange
 * (ClipByChildren), then logs the GraphicsExpose / NoExpose events and
 * how many of C's own pixels are its gray background again: Xorg paints
 * it over the exposed part of a window destination.
 *
 * Map: P (300x200) sits partly off the left edge of the screen, with
 * children P1 (10,10 100x50), P2 (50,40 100x80) above it, an InputOnly
 * P3 over all of P, P4 (200,150 150x100) reaching past P's corner, and
 * an unmapped P5 (20,40 60x60). The children are mapped first, then P;
 * then P5 is mapped, P2 unmapped, P1 raised over P5, and all of P's
 * children unmapped and mapped again. Each step logs the Expose events
 * in the order they arrive.
 *
 * In `redirect` the probe is its own compositor (RedirectSubwindows(root,
 * Manual)) and the windows have depth 32; a redirected window is not
 * clipped by the screen. In `direct` they have the root's depth.
 *
 *   cc -O1 -o expose-probe expose-probe.c -lxcb -lxcb-composite
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <xcb/composite.h>
#include <xcb/xcb.h>

#define FRAME 0x202020u
#define GRAY 0x3b3b3eu
#define BLUE 0x0000ffu
#define GREEN 0x00ff00u
#define MAGENTA 0xff00ffu
#define ORANGE 0xff8000u
#define CYAN 0x00ffffu

#define FW 200
#define FH 150
#define CX 5
#define CY 20
#define CW 190
#define CH 100

static xcb_connection_t *c;
static xcb_screen_t *s;
static xcb_visualid_t vis;
static uint8_t depth;
/* An opaque pixel of the windows' depth. */
#define PIX (depth == 32 ? 0xff000000u : 0) |
static xcb_colormap_t cmap;
static xcb_window_t f, t, cw, b, h, v;
static int redirect;

static struct {
    xcb_window_t id;
    const char *name;
} names[16];
static int n_names;

static void sync_server(void)
{
    free(xcb_get_input_focus_reply(c, xcb_get_input_focus(c), NULL));
}

static const char *name_of(xcb_window_t w)
{
    static char hex[16];
    for (int i = 0; i < n_names; i++)
        if (names[i].id == w)
            return names[i].name;
    snprintf(hex, sizeof hex, "%#x", w);
    return hex;
}

/* Log every event that arrived up to a round trip from now. */
static void events(void)
{
    sync_server();
    xcb_generic_event_t *e;
    while ((e = xcb_poll_for_event(c))) {
        switch (e->response_type & 0x7f) {
        case XCB_GRAPHICS_EXPOSURE: {
            xcb_graphics_exposure_event_t *g = (xcb_graphics_exposure_event_t *)e;
            printf("  GraphicsExpose %d,%d %ux%u count %u\n", g->x, g->y, g->width, g->height,
                   g->count);
            break;
        }
        case XCB_NO_EXPOSURE:
            printf("  NoExpose\n");
            break;
        case XCB_EXPOSE: {
            xcb_expose_event_t *x = (xcb_expose_event_t *)e;
            printf("  Expose %s %d,%d %ux%u count %u\n", name_of(x->window), x->x, x->y,
                   x->width, x->height, x->count);
            break;
        }
        case 0:
            printf("  error %u\n", ((xcb_generic_error_t *)e)->error_code);
            break;
        }
        free(e);
    }
    fflush(stdout);
}

/* Depth 32, as a compositor's frames are, under the probe's compositor;
 * else the root's depth: Composite redirects a depth-32 child of a
 * depth-24 parent by itself (compImplicitRedirect). */
static void find_visual(void)
{
    depth = s->root_depth;
    vis = s->root_visual;
    for (xcb_depth_iterator_t d = xcb_screen_allowed_depths_iterator(s); d.rem && redirect;
         xcb_depth_next(&d))
        if (d.data->depth == 32) {
            depth = 32;
            vis = xcb_depth_visuals_iterator(d.data).data->visual_id;
            break;
        }
    cmap = xcb_generate_id(c);
    xcb_create_colormap(c, XCB_COLORMAP_ALLOC_NONE, cmap, s->root, vis);
}

static xcb_window_t window(xcb_window_t parent, const char *name, int x, int y, int w, int hh,
                           uint32_t bg, uint32_t mask, int map)
{
    xcb_window_t id = xcb_generate_id(c);
    uint32_t vals[4] = {PIX bg, 0, mask, cmap};
    xcb_create_window(c, depth, id, parent, x, y, w, hh, 0, XCB_WINDOW_CLASS_INPUT_OUTPUT, vis,
                      XCB_CW_BACK_PIXEL | XCB_CW_BORDER_PIXEL | XCB_CW_EVENT_MASK |
                          XCB_CW_COLORMAP,
                      vals);
    if (map)
        xcb_map_window(c, id);
    names[n_names].id = id;
    names[n_names++].name = name;
    return id;
}

static xcb_gcontext_t gc_exposures(xcb_drawable_t d, uint32_t mode)
{
    xcb_gcontext_t id = xcb_generate_id(c);
    uint32_t vals[3] = {PIX ORANGE, mode, 1};
    xcb_create_gc(c, id, d,
                  XCB_GC_FOREGROUND | XCB_GC_SUBWINDOW_MODE | XCB_GC_GRAPHICS_EXPOSURES, vals);
    return id;
}

static void fill_c(void)
{
    xcb_gcontext_t g = gc_exposures(cw, XCB_SUBWINDOW_MODE_CLIP_BY_CHILDREN);
    uint32_t off = 0;
    xcb_change_gc(c, g, XCB_GC_GRAPHICS_EXPOSURES, &off);
    xcb_rectangle_t r = {0, 0, CW, CH};
    xcb_poly_fill_rectangle(c, cw, g, 1, &r);
    xcb_free_gc(c, g);
    sync_server();
}

/* How many of C's own pixels (not under V or H) are its background. */
static void gray_in_c(void)
{
    sync_server();
    xcb_drawable_t d = s->root;
    xcb_pixmap_t p = XCB_NONE;
    if (!redirect)
        usleep(150000); /* the root reads the last composited frame */
    if (redirect) {
        p = xcb_generate_id(c);
        xcb_composite_name_window_pixmap(c, f, p);
        d = p;
    }
    xcb_get_image_reply_t *r = xcb_get_image_reply(
        c, xcb_get_image(c, XCB_IMAGE_FORMAT_Z_PIXMAP, d, 0, 0, FW, FH, ~0u), NULL);
    if (p)
        xcb_free_pixmap(c, p);
    if (!r) {
        printf("  GetImage failed\n");
        return;
    }
    const uint32_t *img = (const uint32_t *)xcb_get_image_data(r);
    int n = 0, x0 = CW, y0 = CH, x1 = -1, y1 = -1;
    for (int y = 0; y < CH; y++)
        for (int x = 0; x < CW; x++) {
            int fx = x + CX, fy = y + CY;
            if ((x >= 100 && y >= 10) || (fx >= 130 && fx < 160 && fy < 35))
                continue;
            if ((img[fy * FW + fx] & 0xffffff) != GRAY)
                continue;
            n++;
            x0 = x < x0 ? x : x0;
            y0 = y < y0 ? y : y0;
            x1 = x > x1 ? x : x1;
            y1 = y > y1 ? y : y1;
        }
    if (n)
        printf("  C background %d: x %d-%d y %d-%d\n", n, x0, x1, y0, y1);
    else
        printf("  C background 0\n");
    free(r);
    fflush(stdout);
}

static void copy(const char *when, xcb_drawable_t src, xcb_drawable_t dst, uint32_t mode,
                 int sx, int sy, int dx, int dy, int w, int hh)
{
    fill_c();
    xcb_gcontext_t g = gc_exposures(dst, mode);
    xcb_copy_area(c, src, dst, g, sx, sy, dx, dy, w, hh);
    xcb_free_gc(c, g);
    printf("%s\n", when);
    events();
    gray_in_c();
}

static void copies(void)
{
    f = window(s->root, "F", 0, 0, FW, FH, FRAME, 0, 1);
    t = window(f, "T", 0, 0, 200, 20, MAGENTA, 0, 1);
    cw = window(f, "C", CX, CY, CW, CH, GRAY, 0, 1);
    b = window(f, "B", 0, 125, 200, 25, GREEN, 0, 1);
    h = window(f, "H", 130, 15, 30, 20, CYAN, 0, 1);
    v = window(cw, "V", 100, 10, 80, 120, BLUE, 0, 1);
    sync_server();
    usleep(300000);

    const uint32_t cbc = XCB_SUBWINDOW_MODE_CLIP_BY_CHILDREN;
    const uint32_t inf = XCB_SUBWINDOW_MODE_INCLUDE_INFERIORS;
    copy("C: CopyArea 0,0 -> 0,40 100x200 (source past C's bottom)", cw, cw, cbc, 0, 0, 0, 40,
         100, 200);
    copy("C: CopyArea 0,50 -> 0,0 100x200 (scroll up)", cw, cw, cbc, 0, 50, 0, 0, 100, 200);
    copy("C: CopyArea 0,0 -> -20,-40 100x140", cw, cw, cbc, 0, 0, -20, -40, 100, 140);
    copy("C: CopyArea 90,20 -> 10,20 40x40 (source under V)", cw, cw, cbc, 90, 20, 10, 20, 40,
         40);
    copy("C: CopyArea 90,20 -> 10,20 40x40 IncludeInferiors", cw, cw, inf, 90, 20, 10, 20, 40,
         40);
    copy("C: CopyArea 120,0 -> 10,50 40x30 (source under H)", cw, cw, cbc, 120, 0, 10, 50, 40,
         30);
    copy("C: CopyArea 120,0 -> 10,50 40x30 IncludeInferiors", cw, cw, inf, 120, 0, 10, 50, 40,
         30);
    copy("C: CopyArea 0,0 -> 100,0 80x60 (onto V)", cw, cw, cbc, 0, 0, 100, 0, 80, 60);

    xcb_pixmap_t pm = xcb_generate_id(c);
    xcb_create_pixmap(c, depth, pm, f, CW, CH);
    copy("C: CopyArea to a pixmap 0,0 190x100", cw, pm, cbc, 0, 0, 0, 0, CW, CH);
    copy("C: CopyArea to a pixmap 0,0 190x100 IncludeInferiors", cw, pm, inf, 0, 0, 0, 0, CW,
         CH);
    xcb_free_pixmap(c, pm);

    pm = xcb_generate_id(c);
    xcb_create_pixmap(c, depth, pm, f, 50, 50);
    copy("pixmap 50x50: CopyArea 0,0 -> C 10,10 80x80", pm, cw, cbc, 0, 0, 10, 10, 80, 80);
    copy("pixmap 50x50: CopyArea 0,0 -> C 150,50 100x100", pm, cw, cbc, 0, 0, 150, 50, 100, 100);
    copy("pixmap 50x50: CopyArea 0,0 -> C 70,0 80x80 (over V)", pm, cw, cbc, 0, 0, 70, 0, 80, 80);
    copy("pixmap 50x50: CopyArea 0,0 -> C 70,0 80x80 IncludeInferiors", pm, cw, inf, 0, 0, 70, 0,
         80, 80);
    {
        fill_c();
        xcb_gcontext_t g = gc_exposures(cw, cbc);
        xcb_rectangle_t clip = {0, 0, 70, 70};
        xcb_set_clip_rectangles(c, XCB_CLIP_ORDERING_UNSORTED, g, 0, 0, 1, &clip);
        xcb_copy_area(c, pm, cw, g, 0, 0, 10, 10, 80, 80);
        xcb_free_gc(c, g);
        printf("pixmap 50x50: CopyArea 0,0 -> C 10,10 80x80, clip 0,0 70x70\n");
        events();
        gray_in_c();
    }
    xcb_free_pixmap(c, pm);
    xcb_destroy_window(c, f);
    sync_server();
}

static void maps(void)
{
    const uint32_t ex = XCB_EVENT_MASK_EXPOSURE;
    xcb_window_t p = xcb_generate_id(c);
    {
        uint32_t vals[4] = {PIX GRAY, 0, ex, cmap};
        xcb_create_window(c, depth, p, s->root, -40, 200, 300, 200, 0,
                          XCB_WINDOW_CLASS_INPUT_OUTPUT, vis,
                          XCB_CW_BACK_PIXEL | XCB_CW_BORDER_PIXEL | XCB_CW_EVENT_MASK |
                              XCB_CW_COLORMAP,
                          vals);
        names[n_names].id = p;
        names[n_names++].name = "P";
    }
    xcb_window_t p1 = window(p, "P1", 10, 10, 100, 50, BLUE, ex, 1);
    window(p, "P2", 50, 40, 100, 80, GREEN, ex, 1);
    xcb_window_t p2 = names[n_names - 1].id;
    {
        xcb_window_t p3 = xcb_generate_id(c);
        xcb_create_window(c, 0, p3, p, 0, 0, 300, 200, 0, XCB_WINDOW_CLASS_INPUT_ONLY,
                          XCB_COPY_FROM_PARENT, XCB_CW_EVENT_MASK, &ex);
        xcb_map_window(c, p3);
        names[n_names].id = p3;
        names[n_names++].name = "P3";
    }
    window(p, "P4", 200, 150, 150, 100, MAGENTA, ex, 1);
    xcb_window_t p5 = window(p, "P5", 20, 40, 60, 60, CYAN, ex, 0);
    events();
    printf("P: children mapped while P is not\n");
    events();
    xcb_map_window(c, p);
    printf("P: MapWindow\n");
    events();
    xcb_map_window(c, p5);
    printf("P5: MapWindow\n");
    events();
    xcb_unmap_window(c, p2);
    printf("P2: UnmapWindow\n");
    events();
    uint32_t above = XCB_STACK_MODE_ABOVE;
    xcb_configure_window(c, p1, XCB_CONFIG_WINDOW_STACK_MODE, &above);
    printf("P1: raised over P5\n");
    events();
    xcb_unmap_subwindows(c, p);
    printf("P: UnmapSubwindows\n");
    events();
    xcb_map_subwindows(c, p);
    printf("P: MapSubwindows\n");
    events();
    xcb_destroy_window(c, p);
    sync_server();
}

int main(int argc, char **argv)
{
    redirect = argc > 1 && !strcmp(argv[1], "redirect");
    /* Xorg resets when the previous run's last client leaves. */
    for (int tries = 0; (c = xcb_connect(NULL, NULL)) && xcb_connection_has_error(c) && tries < 50;
         tries++) {
        xcb_disconnect(c);
        usleep(100000);
    }
    if (xcb_connection_has_error(c)) {
        printf("cannot connect\n");
        return 1;
    }
    s = xcb_setup_roots_iterator(xcb_get_setup(c)).data;
    free(xcb_composite_query_version_reply(c, xcb_composite_query_version(c, 0, 4), NULL));
    if (redirect)
        xcb_composite_redirect_subwindows(c, s->root, XCB_COMPOSITE_REDIRECT_MANUAL);
    find_visual();
    copies();
    maps();

    FILE *done = fopen("EXPOSE-DONE", "w");
    if (done)
        fclose(done);
    xcb_disconnect(c);
    return 0;
}
