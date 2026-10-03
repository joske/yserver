/* Every core drawing request, and the reads, against a window's clip:
 * the windows of child-clip-probe.c — a frame F (200x150) with a title
 * bar T (0,0 200x20), a client C (5,20 190x100), a button bar B (0,125
 * 200x25) and above them H (130,15 30x20) over C's top edge; C has a
 * child V (100,10 80x120) reaching past C's bottom.
 *
 *   ./probe redirect | direct
 *
 * In `redirect` the probe is its own compositor (RedirectSubwindows(root,
 * Manual)) and reads F's backing, which C, V and H share, all of depth
 * 32; in `direct` they have the root's depth and it reads F through the
 * root window (F sits at the root's origin).
 *
 * Each draw stage resets every window to its background, draws into C
 * (or V) reaching past C's bounds, and counts the pixels that changed
 * per window: T, H, B and F's own must never change, V only for
 * IncludeInferiors or a draw into V. Clip stages log the bounding box
 * of what changed in C's coordinates. Read stages log GetImage of a
 * window, and RENDER reads through a window picture, run-length encoded;
 * in `direct` also the root right after a draw.
 *
 *   cc -O1 -o probe draw-clip-probe.c -lxcb -lxcb-composite -lxcb-render \
 *      -lxcb-xfixes
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <xcb/composite.h>
#include <xcb/render.h>
#include <xcb/xcb.h>
#include <xcb/xfixes.h>

#define FRAME 0x202020u
#define GRAY 0x3b3b3eu
#define BLUE 0x0000ffu
#define GREEN 0x00ff00u
#define MAGENTA 0xff00ffu
#define ORANGE 0xff8000u
#define CYAN 0x00ffffu
#define WHITE 0xffffffu

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
static xcb_render_pictformat_t fmt;
static xcb_window_t f, t, cw, b, h, v;
static xcb_font_t font;
static uint32_t base[FW * FH];
static int redirect;

static void sync_server(void)
{
    free(xcb_get_input_focus_reply(c, xcb_get_input_focus(c), NULL));
}

static const char *colour(uint32_t px)
{
    static char hex[16];
    switch (px) {
    case FRAME: return "frame";
    case GRAY: return "gray";
    case BLUE: return "blue";
    case GREEN: return "green";
    case MAGENTA: return "magenta";
    case ORANGE: return "orange";
    case CYAN: return "cyan";
    case WHITE: return "white";
    }
    snprintf(hex, sizeof hex, "%06x", px);
    return hex;
}

/* Run-length encode `n` samples of `img` (`stride` wide) from (x0,y0). */
static void runs(const uint32_t *img, int stride, int x0, int y0, int dx, int dy, int n)
{
    uint32_t run = 0;
    int from = -1, last = 0;
    for (int i = 0; i <= n; i++) {
        int at = dx ? x0 + i * dx : y0 + i * dy;
        uint32_t px = i < n ? img[(y0 + i * dy) * stride + x0 + i * dx] & 0xffffff : ~0u;
        if (from >= 0 && px != run) {
            printf(" %d-%d %s", from, last, colour(run));
            from = -1;
        }
        if (from < 0) {
            from = at;
            run = px;
        }
        last = at;
    }
}

/* F as the probe sees it: its backing, or the screen under it. */
static xcb_get_image_reply_t *read_f(int wait)
{
    sync_server();
    xcb_drawable_t d = s->root;
    xcb_pixmap_t p = XCB_NONE;
    if (!redirect && wait)
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
    return r;
}

static int in(int x, int y, int rx, int ry, int rw, int rh)
{
    return x >= rx && x < rx + rw && y >= ry && y < ry + rh;
}

/* Which window owns F's pixel (x,y) on screen. */
static int owner(int x, int y)
{
    if (in(x, y, 130, 15, 30, 20))
        return 1; /* H */
    if (in(x, y, 0, 0, 200, 20))
        return 0; /* T */
    if (in(x, y, 0, 125, 200, 25))
        return 2; /* B */
    if (in(x, y, CX + 100, CY + 10, 80, 120) && in(x, y, CX, CY, CW, CH))
        return 5; /* V */
    if (in(x, y, CX, CY, CW, CH))
        return 4; /* C */
    return 3; /* F */
}

static void events(void)
{
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
        case 0:
            printf("  error %u\n", ((xcb_generic_error_t *)e)->error_code);
            break;
        }
        free(e);
    }
}

/* The pixels of each window that changed since the reset; inside C and
 * V only whether any did, since rasterisation is not the point. */
static void report(const char *when)
{
    xcb_get_image_reply_t *r = read_f(1);
    printf("%s\n", when);
    if (!r) {
        printf("  GetImage failed\n");
        return;
    }
    const uint32_t *img = (const uint32_t *)xcb_get_image_data(r);
    int n[6] = {0};
    for (int y = 0; y < FH; y++)
        for (int x = 0; x < FW; x++)
            n[owner(x, y)] += (img[y * FW + x] & 0xffffff) != (base[y * FW + x] & 0xffffff);
    printf("  T %d  H %d  B %d  F %d  C %s  V %s\n", n[0], n[1], n[2], n[3],
           n[4] ? "changed" : "same", n[5] ? "changed" : "same");
    free(r);
    events();
    fflush(stdout);
}

/* The bounding box, in C's coordinates, of the pixels that changed,
 * and how many did. */
static void report_box(const char *when)
{
    xcb_get_image_reply_t *r = read_f(1);
    printf("%s\n", when);
    if (!r) {
        printf("  GetImage failed\n");
        return;
    }
    const uint32_t *img = (const uint32_t *)xcb_get_image_data(r);
    int x0 = FW, y0 = FH, x1 = -1, y1 = -1, n = 0;
    for (int y = 0; y < FH; y++)
        for (int x = 0; x < FW; x++)
            if ((img[y * FW + x] & 0xffffff) != (base[y * FW + x] & 0xffffff)) {
                n++;
                x0 = x < x0 ? x : x0;
                y0 = y < y0 ? y : y0;
                x1 = x > x1 ? x : x1;
                y1 = y > y1 ? y : y1;
            }
    if (n)
        printf("  changed %d: x %d-%d y %d-%d\n", n, x0 - CX, x1 - CX, y0 - CY, y1 - CY);
    else
        printf("  nothing changed\n");
    free(r);
    events();
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
    xcb_render_query_pict_formats_reply_t *pf =
        xcb_render_query_pict_formats_reply(c, xcb_render_query_pict_formats(c), NULL);
    for (xcb_render_pictscreen_iterator_t ps = xcb_render_query_pict_formats_screens_iterator(pf);
         ps.rem; xcb_render_pictscreen_next(&ps))
        for (xcb_render_pictdepth_iterator_t pd = xcb_render_pictscreen_depths_iterator(ps.data);
             pd.rem; xcb_render_pictdepth_next(&pd))
            for (xcb_render_pictvisual_iterator_t pv =
                     xcb_render_pictdepth_visuals_iterator(pd.data);
                 pv.rem; xcb_render_pictvisual_next(&pv))
                if (pv.data->visual == vis)
                    fmt = pv.data->format;
    free(pf);
}

static xcb_window_t window(xcb_window_t parent, int x, int y, int w, int hh, uint32_t bg)
{
    xcb_window_t id = xcb_generate_id(c);
    uint32_t vals[3] = {PIX bg, 0, cmap};
    xcb_create_window(c, depth, id, parent, x, y, w, hh, 0, XCB_WINDOW_CLASS_INPUT_OUTPUT, vis,
                      XCB_CW_BACK_PIXEL | XCB_CW_BORDER_PIXEL | XCB_CW_COLORMAP, vals);
    xcb_map_window(c, id);
    return id;
}

static void reset(void)
{
    xcb_window_t all[] = {f, t, cw, b, h, v};
    for (unsigned i = 0; i < sizeof all / sizeof all[0]; i++)
        xcb_clear_area(c, 0, all[i], 0, 0, 0, 0);
    sync_server();
    events();
}

static xcb_gcontext_t gc_mode(xcb_drawable_t d, uint32_t mode)
{
    xcb_gcontext_t id = xcb_generate_id(c);
    uint32_t vals[5] = {PIX ORANGE, PIX WHITE, font, mode, 0};
    xcb_create_gc(c, id, d,
                  XCB_GC_FOREGROUND | XCB_GC_BACKGROUND | XCB_GC_SUBWINDOW_MODE | XCB_GC_FONT |
                      XCB_GC_GRAPHICS_EXPOSURES,
                  vals);
    return id;
}

static void line_width(xcb_gcontext_t g, uint32_t w)
{
    xcb_change_gc(c, g, XCB_GC_LINE_WIDTH, &w);
}

/* A field of points every 4 pixels over C and well past it. */
static void points(xcb_drawable_t d, xcb_gcontext_t g)
{
    static xcb_point_t pts[90 * 60];
    int n = 0;
    for (int y = -40; y < 200; y += 4)
        for (int x = -40; x < 320; x += 4)
            pts[n++] = (xcb_point_t){x, y};
    xcb_poly_point(c, XCB_COORD_MODE_ORIGIN, d, g, n, pts);
}

static void polyline(xcb_drawable_t d, xcb_gcontext_t g)
{
    xcb_point_t zig[] = {{-40, -30}, {60, 140}, {120, -20}, {200, 130}, {260, -10}, {-40, 60}};
    xcb_poly_line(c, XCB_COORD_MODE_ORIGIN, d, g, 6, zig);
}

static void segments(xcb_drawable_t d, xcb_gcontext_t g)
{
    xcb_segment_t seg[16];
    for (int i = 0; i < 12; i++)
        seg[i] = (xcb_segment_t){-50, (int16_t)(-20 + i * 12), 300, (int16_t)(-20 + i * 12)};
    seg[12] = (xcb_segment_t){20, -50, 20, 200};
    seg[13] = (xcb_segment_t){140, -50, 140, 200};
    seg[14] = (xcb_segment_t){150, -50, 150, 200};
    seg[15] = (xcb_segment_t){-50, -50, 300, 200};
    xcb_poly_segment(c, d, g, 16, seg);
}

static void rectangles(xcb_drawable_t d, xcb_gcontext_t g)
{
    xcb_rectangle_t r[] = {{-10, -10, 210, 120}, {10, 5, 170, 90}, {120, -8, 30, 30}};
    xcb_poly_rectangle(c, d, g, 3, r);
}

static void arcs(xcb_drawable_t d, xcb_gcontext_t g)
{
    xcb_arc_t a[] = {{-40, -40, 270, 180, 0, 360 * 64}, {20, 10, 150, 80, 0, 360 * 64},
                     {110, -20, 60, 50, 0, 360 * 64}};
    xcb_poly_arc(c, d, g, 3, a);
}

static void fill_poly(xcb_drawable_t d, xcb_gcontext_t g)
{
    xcb_point_t tri[] = {{-60, -40}, {320, 20}, {40, 220}};
    xcb_fill_poly(c, d, g, XCB_POLY_SHAPE_COMPLEX, XCB_COORD_MODE_ORIGIN, 3, tri);
}

static void fill_arcs(xcb_drawable_t d, xcb_gcontext_t g)
{
    xcb_arc_t a[] = {{-40, -40, 270, 180, 0, 360 * 64}};
    xcb_poly_fill_arc(c, d, g, 1, a);
}

static const char text[] = "The quick brown fox jumps over the lazy dog";

/* PolyText8: one item per line, lines from above C to below it. */
static void poly_text8(xcb_drawable_t d, xcb_gcontext_t g)
{
    uint8_t buf[2 + sizeof text];
    int len = (int)strlen(text);
    buf[0] = (uint8_t)len;
    buf[1] = 0;
    memcpy(buf + 2, text, len);
    for (int y = -4; y < 130; y += 11)
        xcb_poly_text_8(c, d, g, -30, y, 2 + len, buf);
}

static void poly_text16(xcb_drawable_t d, xcb_gcontext_t g)
{
    int len = (int)strlen(text);
    uint8_t buf[2 + 2 * sizeof text];
    buf[0] = (uint8_t)len;
    buf[1] = 0;
    for (int i = 0; i < len; i++) {
        buf[2 + 2 * i] = 0;
        buf[3 + 2 * i] = (uint8_t)text[i];
    }
    for (int y = -4; y < 130; y += 11)
        xcb_poly_text_16(c, d, g, -30, y, 2 + 2 * len, buf);
}

static void image_text8(xcb_drawable_t d, xcb_gcontext_t g)
{
    for (int y = -4; y < 130; y += 11)
        xcb_image_text_8(c, (uint8_t)strlen(text), d, g, -30, y, text);
}

static void image_text16(xcb_drawable_t d, xcb_gcontext_t g)
{
    int len = (int)strlen(text);
    xcb_char2b_t chars[sizeof text];
    for (int i = 0; i < len; i++)
        chars[i] = (xcb_char2b_t){0, (uint8_t)text[i]};
    for (int y = -4; y < 130; y += 11)
        xcb_image_text_16(c, (uint8_t)len, d, g, -30, y, chars);
}

static void fill_all(xcb_drawable_t d, xcb_gcontext_t g)
{
    xcb_rectangle_t r = {-50, -50, 400, 400};
    xcb_poly_fill_rectangle(c, d, g, 1, &r);
}

struct op {
    const char *name;
    void (*draw)(xcb_drawable_t, xcb_gcontext_t);
    uint32_t width;
};

static const struct op ops[] = {
    {"PolyPoint", points, 0},
    {"PolyLine", polyline, 0},
    {"PolyLine width 7", polyline, 7},
    {"PolySegment", segments, 0},
    {"PolySegment width 5", segments, 5},
    {"PolyRectangle", rectangles, 0},
    {"PolyRectangle width 6", rectangles, 6},
    {"PolyArc", arcs, 0},
    {"PolyArc width 5", arcs, 5},
    {"FillPoly", fill_poly, 0},
    {"PolyFillArc", fill_arcs, 0},
    {"PolyText8", poly_text8, 0},
    {"PolyText16", poly_text16, 0},
    {"ImageText8", image_text8, 0},
    {"ImageText16", image_text16, 0},
};

static void draw_ops(void)
{
    char line[96];
    for (unsigned i = 0; i < sizeof ops / sizeof ops[0]; i++) {
        static const uint32_t modes[] = {XCB_SUBWINDOW_MODE_CLIP_BY_CHILDREN,
                                         XCB_SUBWINDOW_MODE_INCLUDE_INFERIORS};
        for (int m = 0; m < 2; m++) {
            xcb_gcontext_t g = gc_mode(cw, modes[m]);
            if (ops[i].width)
                line_width(g, ops[i].width);
            ops[i].draw(cw, g);
            xcb_free_gc(c, g);
            snprintf(line, sizeof line, "C: %s %s", ops[i].name,
                     m ? "IncludeInferiors" : "ClipByChildren");
            report(line);
            reset();
        }
        xcb_gcontext_t g = gc_mode(v, XCB_SUBWINDOW_MODE_CLIP_BY_CHILDREN);
        if (ops[i].width)
            line_width(g, ops[i].width);
        ops[i].draw(v, g);
        xcb_free_gc(c, g);
        snprintf(line, sizeof line, "V: %s", ops[i].name);
        report(line);
        reset();
    }

    /* A tiled fill, a CopyPlane from a bitmap and a ClearArea. */
    xcb_pixmap_t tile = xcb_generate_id(c);
    xcb_create_pixmap(c, depth, tile, f, 8, 8);
    xcb_gcontext_t tg = gc_mode(tile, XCB_SUBWINDOW_MODE_CLIP_BY_CHILDREN);
    xcb_rectangle_t all = {0, 0, 8, 8};
    xcb_poly_fill_rectangle(c, tile, tg, 1, &all);
    xcb_free_gc(c, tg);
    xcb_gcontext_t g = gc_mode(cw, XCB_SUBWINDOW_MODE_CLIP_BY_CHILDREN);
    uint32_t tiled[2] = {XCB_FILL_STYLE_TILED, tile};
    xcb_change_gc(c, g, XCB_GC_FILL_STYLE | XCB_GC_TILE, tiled);
    fill_all(cw, g);
    xcb_free_gc(c, g);
    report("C: PolyFillRectangle tiled -50,-50 400x400");
    reset();
    xcb_free_pixmap(c, tile);

    xcb_pixmap_t bits = xcb_generate_id(c);
    xcb_create_pixmap(c, 1, bits, f, 300, 200);
    xcb_gcontext_t one = xcb_generate_id(c);
    uint32_t on = 1;
    xcb_create_gc(c, one, bits, XCB_GC_FOREGROUND, &on);
    xcb_rectangle_t whole = {0, 0, 300, 200};
    xcb_poly_fill_rectangle(c, bits, one, 1, &whole);
    xcb_free_gc(c, one);
    g = gc_mode(cw, XCB_SUBWINDOW_MODE_CLIP_BY_CHILDREN);
    xcb_copy_plane(c, bits, cw, g, 0, 0, -40, -40, 300, 200, 1);
    xcb_free_gc(c, g);
    report("C: CopyPlane -40,-40 300x200");
    reset();
    xcb_free_pixmap(c, bits);

    g = gc_mode(cw, XCB_SUBWINDOW_MODE_INCLUDE_INFERIORS);
    fill_all(cw, g);
    xcb_free_gc(c, g);
    xcb_clear_area(c, 0, cw, 0, 0, 0, 0);
    report("C: PolyFillRectangle IncludeInferiors, then ClearArea C");
    reset();
}

/* GC clip origins: SetClipRectangles' own, ChangeGC's after it, a clip
 * mask's, CopyGC's and XFixesSetGCClipRegion's. */
static void clip_origins(void)
{
    xcb_rectangle_t r = {0, 0, 40, 40};
    xcb_gcontext_t g = gc_mode(cw, XCB_SUBWINDOW_MODE_CLIP_BY_CHILDREN);
    xcb_set_clip_rectangles(c, XCB_CLIP_ORDERING_UNSORTED, g, 20, 50, 1, &r);
    fill_all(cw, g);
    report_box("C: fill, clip 0,0 40x40 at origin 20,50");
    reset();

    uint32_t org[2] = {30, 10};
    xcb_change_gc(c, g, XCB_GC_CLIP_ORIGIN_X | XCB_GC_CLIP_ORIGIN_Y, org);
    fill_all(cw, g);
    report_box("C: fill, then ChangeGC clip origin 30,10");
    reset();

    xcb_set_clip_rectangles(c, XCB_CLIP_ORDERING_UNSORTED, g, 20, 50, 1, &r);
    xcb_segment_t seg[] = {{-50, 60, 300, 60}, {40, -50, 40, 200}};
    xcb_poly_segment(c, cw, g, 2, seg);
    report_box("C: PolySegment, clip 0,0 40x40 at origin 20,50");
    reset();
    poly_text8(cw, g);
    report_box("C: PolyText8, clip 0,0 40x40 at origin 20,50");
    reset();

    xcb_pixmap_t pm = xcb_generate_id(c);
    xcb_create_pixmap(c, depth, pm, f, 300, 300);
    xcb_gcontext_t pg = gc_mode(pm, XCB_SUBWINDOW_MODE_CLIP_BY_CHILDREN);
    fill_all(pm, pg);
    xcb_free_gc(c, pg);
    xcb_copy_area(c, pm, cw, g, 0, 0, -20, -20, 300, 300);
    report_box("C: CopyArea, clip 0,0 40x40 at origin 20,50");
    reset();
    xcb_free_pixmap(c, pm);

    xcb_gcontext_t copy = gc_mode(cw, XCB_SUBWINDOW_MODE_CLIP_BY_CHILDREN);
    xcb_copy_gc(c, g, copy, XCB_GC_CLIP_ORIGIN_X | XCB_GC_CLIP_ORIGIN_Y | XCB_GC_CLIP_MASK);
    fill_all(cw, copy);
    report_box("C: fill, CopyGC of the clip at origin 20,50");
    reset();
    xcb_free_gc(c, copy);
    xcb_free_gc(c, g);

    xcb_pixmap_t mask = xcb_generate_id(c);
    xcb_create_pixmap(c, 1, mask, f, 40, 40);
    xcb_gcontext_t one = xcb_generate_id(c);
    uint32_t on = 1;
    xcb_create_gc(c, one, mask, XCB_GC_FOREGROUND, &on);
    xcb_poly_fill_rectangle(c, mask, one, 1, &r);
    xcb_free_gc(c, one);
    g = gc_mode(cw, XCB_SUBWINDOW_MODE_CLIP_BY_CHILDREN);
    uint32_t mvals[3] = {20, 50, mask};
    xcb_change_gc(c, g, XCB_GC_CLIP_ORIGIN_X | XCB_GC_CLIP_ORIGIN_Y | XCB_GC_CLIP_MASK, mvals);
    fill_all(cw, g);
    report_box("C: fill, 40x40 clip mask at origin 20,50");
    reset();
    xcb_free_gc(c, g);
    xcb_free_pixmap(c, mask);

    g = gc_mode(cw, XCB_SUBWINDOW_MODE_CLIP_BY_CHILDREN);
    xcb_xfixes_region_t rg = xcb_generate_id(c);
    xcb_xfixes_create_region(c, rg, 1, &r);
    xcb_xfixes_set_gc_clip_region(c, g, rg, 20, 50);
    fill_all(cw, g);
    report_box("C: fill, XFixesSetGCClipRegion 0,0 40x40 at 20,50");
    reset();
    xcb_xfixes_destroy_region(c, rg);
    xcb_free_gc(c, g);
}

/* GetImage of a window: what is on screen within it, its inferiors too. */
static void get_image(const char *name, xcb_window_t w, int ww, int hh, int wait)
{
    sync_server();
    if (wait && !redirect)
        usleep(150000);
    xcb_get_image_reply_t *r = xcb_get_image_reply(
        c, xcb_get_image(c, XCB_IMAGE_FORMAT_Z_PIXMAP, w, 0, 0, ww, hh, ~0u), NULL);
    printf("%s\n", name);
    if (!r) {
        printf("  GetImage failed\n");
        return;
    }
    const uint32_t *img = (const uint32_t *)xcb_get_image_data(r);
    if (ww == CW) {
        /* Clear of H, which covers C's rows 0..14 from x=125. */
        printf("  x=40: ");
        runs(img, ww, 40, 0, 0, 5, hh / 5);
        printf("\n  x=140:");
        runs(img, ww, 140, 15, 0, 5, (hh - 15) / 5);
        printf("\n  y=50: ");
        runs(img, ww, 0, 50, 5, 0, ww / 5);
        printf("\n  y=95: ");
        runs(img, ww, 0, 95, 5, 0, ww / 5);
    } else {
        printf("  x=50: ");
        runs(img, ww, 50, 0, 0, 5, hh / 5);
        printf("\n  x=145:");
        runs(img, ww, 145, 0, 0, 5, hh / 5);
        printf("\n  y=60: ");
        runs(img, ww, 0, 60, 5, 0, ww / 5);
        printf("\n  y=137:");
        runs(img, ww, 0, 137, 5, 0, ww / 5);
    }
    printf("\n");
    free(r);
    events();
    fflush(stdout);
}

static xcb_render_picture_t picture(xcb_drawable_t d, uint32_t mode)
{
    xcb_render_picture_t p = xcb_generate_id(c);
    xcb_render_create_picture(c, p, d, fmt, XCB_RENDER_CP_SUBWINDOW_MODE, &mode);
    return p;
}

static const xcb_render_color_t orange_rc = {0xffff, 0x8080, 0, 0xffff};

/* RENDER through a window picture: reading C into a pixmap, drawing
 * onto C, with and without its inferiors. */
static void render_inferiors(void)
{
    static const uint32_t modes[] = {XCB_SUBWINDOW_MODE_CLIP_BY_CHILDREN,
                                     XCB_SUBWINDOW_MODE_INCLUDE_INFERIORS};
    static const char *names[] = {"ClipByChildren", "IncludeInferiors"};
    char line[96];
    for (int m = 0; m < 2; m++) {
        xcb_pixmap_t pm = xcb_generate_id(c);
        xcb_create_pixmap(c, depth, pm, f, CW, CH);
        xcb_gcontext_t g = gc_mode(pm, XCB_SUBWINDOW_MODE_CLIP_BY_CHILDREN);
        uint32_t white = PIX WHITE;
        xcb_change_gc(c, g, XCB_GC_FOREGROUND, &white);
        fill_all(pm, g);
        xcb_free_gc(c, g);
        xcb_render_picture_t src = picture(cw, modes[m]);
        xcb_render_picture_t dst = picture(pm, XCB_SUBWINDOW_MODE_CLIP_BY_CHILDREN);
        sync_server();
        if (!redirect)
            usleep(150000);
        xcb_render_composite(c, XCB_RENDER_PICT_OP_SRC, src, XCB_NONE, dst, 0, 0, 0, 0, 0, 0, CW,
                             CH);
        snprintf(line, sizeof line, "RENDER Composite from C %s into a pixmap", names[m]);
        get_image(line, pm, CW, CH, 0);
        xcb_render_free_picture(c, src);
        xcb_render_free_picture(c, dst);
        xcb_free_pixmap(c, pm);
    }
    for (int m = 0; m < 2 && !redirect; m++) {
        xcb_pixmap_t pm = xcb_generate_id(c);
        xcb_create_pixmap(c, depth, pm, f, FW, FH);
        xcb_render_picture_t src = picture(s->root, modes[m]);
        xcb_render_picture_t dst = picture(pm, XCB_SUBWINDOW_MODE_CLIP_BY_CHILDREN);
        sync_server();
        usleep(150000);
        xcb_render_composite(c, XCB_RENDER_PICT_OP_SRC, src, XCB_NONE, dst, 0, 0, 0, 0, 0, 0, FW,
                             FH);
        snprintf(line, sizeof line, "RENDER Composite from the root %s into a pixmap", names[m]);
        get_image(line, pm, FW, FH, 0);
        xcb_render_free_picture(c, src);
        xcb_render_free_picture(c, dst);
        xcb_free_pixmap(c, pm);
    }
    for (int m = 0; m < 2; m++) {
        xcb_render_picture_t dst = picture(cw, modes[m]);
        xcb_rectangle_t r = {-50, -50, 400, 400};
        xcb_render_fill_rectangles(c, XCB_RENDER_PICT_OP_SRC, dst, orange_rc, 1, &r);
        xcb_render_free_picture(c, dst);
        snprintf(line, sizeof line, "C: RENDER FillRectangles %s", names[m]);
        report(line);
        reset();

        xcb_pixmap_t pm = xcb_generate_id(c);
        xcb_create_pixmap(c, depth, pm, f, 300, 300);
        xcb_render_picture_t src = picture(pm, XCB_SUBWINDOW_MODE_CLIP_BY_CHILDREN);
        xcb_rectangle_t all = {0, 0, 300, 300};
        xcb_render_fill_rectangles(c, XCB_RENDER_PICT_OP_SRC, src, orange_rc, 1, &all);
        dst = picture(cw, modes[m]);
        xcb_render_composite(c, XCB_RENDER_PICT_OP_SRC, src, XCB_NONE, dst, 0, 0, 0, 0, -30, -30,
                             300, 300);
        xcb_render_free_picture(c, dst);
        xcb_render_free_picture(c, src);
        xcb_free_pixmap(c, pm);
        snprintf(line, sizeof line, "C: RENDER Composite %s", names[m]);
        report(line);
        reset();
    }
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
    free(xcb_render_query_version_reply(c, xcb_render_query_version(c, 0, 11), NULL));
    free(xcb_xfixes_query_version_reply(c, xcb_xfixes_query_version(c, 5, 0), NULL));
    if (redirect)
        xcb_composite_redirect_subwindows(c, s->root, XCB_COMPOSITE_REDIRECT_MANUAL);
    find_visual();
    font = xcb_generate_id(c);
    xcb_open_font(c, font, 5, "fixed");

    f = window(s->root, 0, 0, FW, FH, FRAME);
    t = window(f, 0, 0, 200, 20, MAGENTA);
    cw = window(f, CX, CY, CW, CH, GRAY);
    b = window(f, 0, 125, 200, 25, GREEN);
    h = window(f, 130, 15, 30, 20, CYAN);
    v = window(cw, 100, 10, 80, 120, BLUE);
    sync_server();
    usleep(300000);
    reset();
    xcb_get_image_reply_t *r = read_f(1);
    if (!r) {
        printf("cannot read F\n");
        return 1;
    }
    memcpy(base, xcb_get_image_data(r), sizeof base);
    free(r);

    draw_ops();
    clip_origins();

    get_image("GetImage C", cw, CW, CH, 1);
    get_image("GetImage F", f, FW, FH, 1);
    /* Partly off the screen: a redirected window reads its backing. */
    int32_t off = -50;
    xcb_configure_window(c, f, XCB_CONFIG_WINDOW_X, &off);
    get_image("F at x=-50: GetImage F", f, FW, FH, 1);
    off = 0;
    xcb_configure_window(c, f, XCB_CONFIG_WINDOW_X, &off);
    render_inferiors();

    /* What the screen shows right after a draw, without waiting. */
    if (!redirect) {
        xcb_gcontext_t g = gc_mode(cw, XCB_SUBWINDOW_MODE_INCLUDE_INFERIORS);
        fill_all(cw, g);
        xcb_free_gc(c, g);
        get_image("C: fill IncludeInferiors, GetImage root at once", s->root, FW, FH, 0);
        reset();
        get_image("reset, GetImage root at once", s->root, FW, FH, 0);
    }

    FILE *done = fopen("PROBE-DONE", "w");
    if (done)
        fclose(done);
    xcb_disconnect(c);
    return 0;
}
