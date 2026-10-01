/* Moving an unredirected window inside a redirected top-level: the MATE
 * panel under a compositor (picom, marco), whose applets are subwindows
 * that all draw into the panel's one redirect backing.
 *
 *   ./probe
 *
 * The probe is its own compositor (RedirectSubwindows(root, Manual)) and
 * builds the panel's notification area: a depth-32 panel T, an applet
 * socket C (background None), the tray's plug G (created as a top-level
 * and reparented into C, as XEmbed does), the tray's Manual-redirected
 * icon socket S inside G and the icon window I inside S. The tray draws
 * its icon into G once, from a temporary pixmap, and then only on Expose.
 *
 * Each stage logs the colour of T's backing (NameWindowPixmap) along one
 * row, so Xorg and yserver runs diff directly:
 *
 *  1. the applet is dragged (pure moves of C, the panel repainting its
 *     strip ClipByChildren after each): the icon moves with C — Xorg's
 *     CopyWindow — and a DAMAGE object on T reports both ends;
 *  2. a higher sibling (the workspace switcher) slides across C and off:
 *     what it uncovers is exposed, and G repaints;
 *  3. the switcher slides under another, higher window in one jump: the
 *     part of its new position that was hidden before the move is
 *     exposed to the switcher itself, which repaints.
 *
 *   cc -O1 -o probe subwindow-move-probe.c -lxcb -lxcb-composite -lxcb-damage -lxcb-xfixes
 */
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <xcb/composite.h>
#include <xcb/damage.h>
#include <xcb/xcb.h>
#include <xcb/xfixes.h>

#define GRAY 0x3b3b3eu
#define BLUE 0x0000ffu
#define GREEN 0x00ff00u
#define MAGENTA 0xff00ffu
#define RED 0xff0000u

static xcb_connection_t *c;
static xcb_screen_t *s;
static xcb_visualid_t vis32;
static xcb_colormap_t cmap32;
static xcb_window_t t;
static xcb_damage_damage_t damage;
static xcb_xfixes_region_t parts;

static void sync_server(void)
{
    free(xcb_get_input_focus_reply(c, xcb_get_input_focus(c), NULL));
}

static const char *colour(uint32_t v)
{
    static char hex[16];
    switch (v) {
    case GRAY: return "gray";
    case BLUE: return "blue";
    case GREEN: return "green";
    case MAGENTA: return "magenta";
    case RED: return "red";
    }
    snprintf(hex, sizeof hex, "%06x", v);
    return hex;
}

static uint32_t pixel(xcb_drawable_t d, int x, int y)
{
    xcb_get_image_reply_t *r = xcb_get_image_reply(
        c, xcb_get_image(c, XCB_IMAGE_FORMAT_Z_PIXMAP, d, x, y, 1, 1, ~0u), NULL);
    if (!r)
        return 0xdeadbeef;
    uint32_t v = *(uint32_t *)xcb_get_image_data(r) & 0xffffff;
    free(r);
    return v;
}

/* T's backing along y=12, every 5 pixels over the area the windows use,
 * run-length encoded: "140-145 gray, 150-180 blue, ...". */
static void report(const char *when)
{
    xcb_pixmap_t p = xcb_generate_id(c);
    xcb_composite_name_window_pixmap(c, t, p);
    sync_server();
    printf("%s:", when);
    uint32_t run = 0;
    int from = -1;
    for (int x = 140; x <= 350; x += 5) {
        uint32_t v = x < 350 ? pixel(p, x, 12) : ~0u;
        if (from >= 0 && v != run) {
            printf(" %d-%d %s", from, x - 5, colour(run));
            from = -1;
        }
        if (from < 0) {
            from = x;
            run = v;
        }
    }
    printf("\n");
    fflush(stdout);
    xcb_free_pixmap(c, p);
}

/* Whether T's accumulated damage covers every pixel of the rect, then
 * reset it. */
static int damage_covers(int x, int y, int w, int h)
{
    xcb_damage_subtract(c, damage, XCB_NONE, parts);
    xcb_xfixes_fetch_region_reply_t *r =
        xcb_xfixes_fetch_region_reply(c, xcb_xfixes_fetch_region(c, parts), NULL);
    if (!r)
        return 0;
    xcb_rectangle_t *rects = xcb_xfixes_fetch_region_rectangles(r);
    int n = xcb_xfixes_fetch_region_rectangles_length(r);
    int covered = 1;
    for (int py = y; py < y + h && covered; py++)
        for (int px = x; px < x + w && covered; px++) {
            int in = 0;
            for (int i = 0; i < n && !in; i++)
                in = px >= rects[i].x && px < rects[i].x + rects[i].width &&
                     py >= rects[i].y && py < rects[i].y + rects[i].height;
            covered = in;
        }
    free(r);
    return covered;
}

static void find_argb(void)
{
    for (xcb_depth_iterator_t d = xcb_screen_allowed_depths_iterator(s); d.rem;
         xcb_depth_next(&d))
        if (d.data->depth == 32) {
            vis32 = xcb_depth_visuals_iterator(d.data).data->visual_id;
            break;
        }
    cmap32 = xcb_generate_id(c);
    xcb_create_colormap(c, XCB_COLORMAP_ALLOC_NONE, cmap32, s->root, vis32);
}

/* A depth-32 window, as GTK's RGBA widgets are; bg < 0 is None. */
static xcb_window_t window(xcb_window_t parent, int x, int y, int w, int h, int bg)
{
    xcb_window_t id = xcb_generate_id(c);
    uint32_t v[3] = {bg >= 0 ? 0xff000000u | (uint32_t)bg : XCB_BACK_PIXMAP_NONE, 0, cmap32};
    uint32_t mask = (bg >= 0 ? XCB_CW_BACK_PIXEL : XCB_CW_BACK_PIXMAP) | XCB_CW_BORDER_PIXEL |
                    XCB_CW_COLORMAP;
    xcb_create_window(c, 32, id, parent, x, y, w, h, 0, XCB_WINDOW_CLASS_INPUT_OUTPUT, vis32,
                      mask, v);
    return id;
}

static void move_x(xcb_window_t w, uint32_t x)
{
    xcb_configure_window(c, w, XCB_CONFIG_WINDOW_X, &x);
}

int main(void)
{
    c = xcb_connect(NULL, NULL);
    if (xcb_connection_has_error(c)) {
        printf("cannot connect\n");
        return 1;
    }
    s = xcb_setup_roots_iterator(xcb_get_setup(c)).data;
    free(xcb_composite_query_version_reply(c, xcb_composite_query_version(c, 0, 4), NULL));
    free(xcb_damage_query_version_reply(c, xcb_damage_query_version(c, 1, 1), NULL));
    free(xcb_xfixes_query_version_reply(c, xcb_xfixes_query_version(c, 5, 0), NULL));
    xcb_composite_redirect_subwindows(c, s->root, XCB_COMPOSITE_REDIRECT_MANUAL);
    find_argb();

    t = window(s->root, 0, 0, 400, 25, GRAY);
    xcb_window_t cw = window(t, 300, 0, 32, 25, -1);
    xcb_window_t g = window(s->root, 0, 100, 32, 25, -1);
    xcb_map_window(c, g);
    sync_server();
    xcb_unmap_window(c, g);
    xcb_reparent_window(c, g, cw, 0, 0);
    xcb_window_t icon_socket = window(g, 0, 0, 32, 25, -1);
    xcb_composite_redirect_window(c, icon_socket, XCB_COMPOSITE_REDIRECT_MANUAL);
    xcb_window_t icon = window(icon_socket, 0, 0, 32, 25, RED);
    xcb_map_window(c, icon);
    xcb_map_window(c, icon_socket);
    xcb_map_window(c, g);
    xcb_map_window(c, cw);
    xcb_map_window(c, t);
    sync_server();
    usleep(300000);

    /* The tray draws its icon into G from a temporary pixmap. */
    xcb_pixmap_t tray = xcb_generate_id(c);
    xcb_create_pixmap(c, 32, tray, g, 32, 25);
    xcb_gcontext_t blue = xcb_generate_id(c);
    uint32_t blue_px = 0xff000000u | BLUE;
    xcb_create_gc(c, blue, tray, XCB_GC_FOREGROUND, &blue_px);
    xcb_rectangle_t all_icon = {0, 0, 32, 25};
    xcb_poly_fill_rectangle(c, tray, blue, 1, &all_icon);
    xcb_copy_area(c, tray, g, blue, 0, 0, 0, 0, 32, 25);

    /* The panel repaints its whole strip from a pixmap, ClipByChildren. */
    xcb_pixmap_t strip = xcb_generate_id(c);
    xcb_create_pixmap(c, 32, strip, t, 400, 25);
    xcb_gcontext_t gray = xcb_generate_id(c);
    uint32_t gray_px = 0xff000000u | GRAY;
    xcb_create_gc(c, gray, strip, XCB_GC_FOREGROUND, &gray_px);
    xcb_rectangle_t all_strip = {0, 0, 400, 25};
    xcb_poly_fill_rectangle(c, strip, gray, 1, &all_strip);

    damage = xcb_generate_id(c);
    xcb_damage_create(c, damage, t, XCB_DAMAGE_REPORT_LEVEL_NON_EMPTY);
    parts = xcb_generate_id(c);
    xcb_xfixes_create_region(c, parts, 0, NULL);
    report("icon drawn, C at 300");
    (void)damage_covers(0, 0, 1, 1);

    /* 1. Drag the applet. */
    move_x(cw, 200);
    report("C moved to 200");
    printf("damage covers C at 200: %s\n", damage_covers(200, 0, 32, 25) ? "yes" : "no");
    move_x(cw, 190);
    printf("damage covers what C left at 200: %s\n",
           damage_covers(222, 0, 10, 25) ? "yes" : "no");
    xcb_copy_area(c, strip, t, gray, 0, 0, 0, 0, 400, 25);
    report("C at 190, strip repainted");
    for (uint32_t x = 180; x >= 150; x -= 10) {
        move_x(cw, x);
        xcb_copy_area(c, strip, t, gray, 0, 0, 0, 0, 400, 25);
    }
    report("C dragged to 150, strip repainted after each move");

    /* 2. The switcher, a higher sibling, slides across C and off. */
    uint32_t exposure = XCB_EVENT_MASK_EXPOSURE;
    xcb_change_window_attributes(c, g, XCB_CW_EVENT_MASK, &exposure);
    xcb_change_window_attributes(c, t, XCB_CW_EVENT_MASK, &exposure);
    xcb_window_t switcher = window(t, 20, 0, 60, 25, GREEN);
    xcb_map_window(c, switcher);
    sync_server();
    for (uint32_t x = 40; x <= 260; x += 20) {
        move_x(switcher, x);
        xcb_copy_area(c, strip, t, gray, 0, 0, 0, 0, 400, 25);
    }
    sync_server();
    usleep(200000);
    int plug_exposed = 0;
    xcb_generic_event_t *e;
    while ((e = xcb_poll_for_event(c))) {
        if ((e->response_type & 0x7f) == XCB_EXPOSE) {
            xcb_expose_event_t *x = (xcb_expose_event_t *)e;
            if (x->window == g) {
                plug_exposed = 1;
                xcb_copy_area(c, tray, g, blue, 0, 0, 0, 0, 32, 25);
            }
            if (x->window == t)
                xcb_copy_area(c, strip, t, gray, x->x, x->y, x->x, x->y, x->width, x->height);
        }
        free(e);
    }
    printf("plug got Expose: %s\n", plug_exposed ? "yes" : "no");
    report("switcher passed over C to 260, Expose handled");

    /* 3. The switcher jumps under a higher window H. */
    xcb_change_window_attributes(c, switcher, XCB_CW_EVENT_MASK, &exposure);
    xcb_window_t h = window(t, 290, 0, 20, 25, MAGENTA);
    xcb_map_window(c, h);
    xcb_gcontext_t green = xcb_generate_id(c);
    uint32_t green_px = 0xff000000u | GREEN;
    xcb_create_gc(c, green, switcher, XCB_GC_FOREGROUND, &green_px);
    move_x(switcher, 270);
    move_x(switcher, 200);
    sync_server();
    usleep(200000);
    int switcher_exposed = 0;
    while ((e = xcb_poll_for_event(c))) {
        if ((e->response_type & 0x7f) == XCB_EXPOSE) {
            xcb_expose_event_t *x = (xcb_expose_event_t *)e;
            if (x->window == switcher) {
                switcher_exposed = 1;
                xcb_rectangle_t r = {x->x, x->y, x->width, x->height};
                xcb_poly_fill_rectangle(c, switcher, green, 1, &r);
            }
        }
        free(e);
    }
    printf("switcher got Expose: %s\n", switcher_exposed ? "yes" : "no");
    report("switcher jumped from under H to 200, Expose handled");

    FILE *done = fopen("PROBE-DONE", "w");
    if (done)
        fclose(done);
    xcb_disconnect(c);
    return 0;
}
