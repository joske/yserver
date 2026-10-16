/* damage-after-map-probe — does a compositor get a DamageNotify AFTER the
 *                          MapNotify of a re-mapped top-level? (issue #248)
 *
 * picom (v13) clears a window's `ever_damaged` on MapNotify (event.c
 * ev_map_notify) and does not paint it until a DamageNotify arrives after that
 * (picom.c paint_preprocess). Its damage handler (event.c repair_win) does
 * DamageSubtract(d, None, None) for the first notify since map, else
 * DamageSubtract(d, None, region) + FetchRegion. Damage is NonEmpty on each
 * top-level (win.c), root's subwindows are redirected Manual.
 *
 * Two connections: `comp` plays picom, `app` plays WM + client: a top-level
 * frame W with a mapped child C. Every step is followed by a round trip and a
 * 200 ms quiet period, then one line: the compositor's events (MapNotify /
 * UnmapNotify of W from root's SubstructureNotify, DamageNotify on W, `+sub`
 * where it repaired like picom, `(held)` where it did not) and the app's
 * Expose events. Each scenario runs with backgrounds None and pixel.
 *
 *   a  map, repair, draw, repair, unmap, map, draw
 *   b  as a, but the draw's notify is NOT repaired before unmap
 *   c  as a, but the client draws into C while W is unmapped
 *   d  as a, but W is moved + raised while unmapped (WM restack)
 *   e  as a, but the client redraws in the same step as the re-map (its
 *      Expose response landing before the compositor's DamageSubtract)
 *
 * `ever=` at the end of the comp column is picom's ever_damaged after the
 * step: 0 on a mapped window means picom will not paint it.
 *
 * No ids or timings in the output: diff an Xorg run against a yserver run.
 *
 *   cc -O1 -o damage-after-map-probe damage-after-map-probe.c \
 *       -lxcb -lxcb-damage -lxcb-composite -lxcb-xfixes
 */
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <xcb/composite.h>
#include <xcb/damage.h>
#include <xcb/xcb.h>
#include <xcb/xfixes.h>

static xcb_connection_t *comp, *app;
static xcb_window_t root, W, C;
static xcb_damage_damage_t dmg;
static xcb_xfixes_region_t xregion;
static uint8_t damage_ev;
static int ever_damaged, hold;
static xcb_screen_t *scr;

static void roundtrip(xcb_connection_t *conn)
{
    free(xcb_get_input_focus_reply(conn, xcb_get_input_focus(conn), NULL));
}

/* picom repair_win. */
static void repair(void)
{
    if (!ever_damaged) {
        free(xcb_request_check(comp, xcb_damage_subtract_checked(comp, dmg, XCB_NONE, XCB_NONE)));
    } else {
        xcb_damage_subtract(comp, dmg, XCB_NONE, xregion);
        free(xcb_xfixes_fetch_region_reply(comp, xcb_xfixes_fetch_region(comp, xregion), NULL));
    }
    ever_damaged = 1;
}

static const char *wname(xcb_window_t w)
{
    return w == W ? "W" : w == C ? "C" : "?";
}

/* Drain one connection until 200 ms pass without an event. */
static void drain(xcb_connection_t *conn, int is_comp)
{
    struct pollfd p = {xcb_get_file_descriptor(conn), POLLIN, 0};
    for (;;) {
        xcb_generic_event_t *ev = xcb_poll_for_event(conn);
        if (!ev) {
            xcb_flush(conn);
            if (poll(&p, 1, 200) <= 0)
                return;
            ev = xcb_poll_for_event(conn);
            if (!ev)
                return;
        }
        uint8_t t = ev->response_type & 0x7f;
        if (t == 0) {
            printf(" Error(%u)", ((xcb_generic_error_t *)ev)->error_code);
        } else if (is_comp && t == XCB_MAP_NOTIFY) {
            xcb_map_notify_event_t *e = (void *)ev;
            if (e->window == W) {
                printf(" MapNotify");
                ever_damaged = 0;
            }
        } else if (is_comp && t == XCB_UNMAP_NOTIFY) {
            if (((xcb_unmap_notify_event_t *)ev)->window == W)
                printf(" UnmapNotify");
        } else if (is_comp && t == damage_ev + XCB_DAMAGE_NOTIFY) {
            xcb_damage_notify_event_t *e = (void *)ev;
            printf(" Damage(%d,%d %ux%u)", e->area.x, e->area.y, e->area.width, e->area.height);
            if (hold) {
                printf("(held)");
            } else {
                repair();
                printf("+sub");
            }
        } else if (!is_comp && t == XCB_EXPOSE) {
            xcb_expose_event_t *e = (void *)ev;
            printf(" Expose(%s %u,%u %ux%u)", wname(e->window), e->x, e->y, e->width, e->height);
        } else if (is_comp && (t == XCB_CONFIGURE_NOTIFY || t == XCB_CREATE_NOTIFY ||
                               t == XCB_DESTROY_NOTIFY || t == XCB_MAPPING_NOTIFY)) {
            /* not what this probe is about */
        } else {
            printf(" event%u", t);
        }
        free(ev);
    }
}

static void step(const char *label)
{
    roundtrip(app);
    roundtrip(comp);
    printf("  %-14s comp:", label);
    drain(comp, 1);
    printf(" ever=%d | app:", ever_damaged);
    drain(app, 0);
    printf("\n");
    fflush(stdout);
}

static void draw(void)
{
    static uint32_t colour = 0x204080;
    colour ^= 0x00ff00;
    xcb_gcontext_t gc = xcb_generate_id(app);
    xcb_create_gc(app, gc, C, XCB_GC_FOREGROUND, &colour);
    xcb_rectangle_t r = {0, 0, 280, 180};
    xcb_poly_fill_rectangle(app, C, gc, 1, &r);
    xcb_free_gc(app, gc);
}

static void scenario(char which, int bg_pixel)
{
    printf("%c/%s\n", which, bg_pixel ? "pixel" : "none");
    uint32_t mask = XCB_CW_OVERRIDE_REDIRECT | XCB_CW_EVENT_MASK;
    uint32_t v[3];
    int n = 0;
    if (bg_pixel) {
        mask |= XCB_CW_BACK_PIXEL;
        v[n++] = 0x808080;
    } else {
        mask |= XCB_CW_BACK_PIXMAP;
        v[n++] = XCB_BACK_PIXMAP_NONE;
    }
    v[n++] = 1;
    v[n++] = XCB_EVENT_MASK_EXPOSURE;
    W = xcb_generate_id(app);
    C = xcb_generate_id(app);
    xcb_create_window(app, scr->root_depth, W, root, 50, 50, 300, 200, 0,
                      XCB_WINDOW_CLASS_INPUT_OUTPUT, scr->root_visual, mask, v);
    xcb_create_window(app, scr->root_depth, C, W, 10, 10, 280, 180, 0,
                      XCB_WINDOW_CLASS_INPUT_OUTPUT, scr->root_visual, mask, v);
    xcb_map_window(app, C);
    roundtrip(app);
    /* picom creates the Damage on CreateNotify, before the map. */
    dmg = xcb_generate_id(comp);
    xcb_damage_create(comp, dmg, W, XCB_DAMAGE_REPORT_LEVEL_NON_EMPTY);
    ever_damaged = 0;
    hold = 0;
    step("create");

    xcb_map_window(app, W);
    step("map1");
    draw();
    hold = which == 'b';
    step("draw1");
    hold = 0;
    if (which == 'c') {
        xcb_unmap_window(app, W);
        step("unmap");
        draw();
        step("draw-unmapped");
    } else if (which == 'd') {
        xcb_unmap_window(app, W);
        step("unmap");
        uint32_t cfg[] = {60, 40, XCB_STACK_MODE_ABOVE};
        xcb_configure_window(app, W,
                             XCB_CONFIG_WINDOW_X | XCB_CONFIG_WINDOW_Y |
                                 XCB_CONFIG_WINDOW_STACK_MODE,
                             cfg);
        step("move+raise");
    } else {
        xcb_unmap_window(app, W);
        step("unmap");
    }
    xcb_map_window(app, W);
    if (which == 'e') {
        draw();
        step("map2+draw");
    } else {
        step("map2");
        draw();
        step("draw2");
    }

    xcb_damage_destroy(comp, dmg);
    xcb_destroy_window(app, W);
    roundtrip(comp);
    step("destroy");
}

int main(void)
{
    comp = xcb_connect(NULL, NULL);
    app = xcb_connect(NULL, NULL);
    if (xcb_connection_has_error(comp) || xcb_connection_has_error(app))
        return 1;
    scr = xcb_setup_roots_iterator(xcb_get_setup(comp)).data;
    root = scr->root;
    const xcb_query_extension_reply_t *d = xcb_get_extension_data(comp, &xcb_damage_id);
    if (!d || !d->present)
        return 1;
    damage_ev = d->first_event;
    free(xcb_damage_query_version_reply(comp, xcb_damage_query_version(comp, 1, 1), NULL));
    free(xcb_xfixes_query_version_reply(comp, xcb_xfixes_query_version(comp, 5, 0), NULL));
    free(xcb_composite_query_version_reply(comp, xcb_composite_query_version(comp, 0, 4), NULL));
    xcb_generic_error_t *e = xcb_request_check(
        comp, xcb_composite_redirect_subwindows_checked(comp, root, XCB_COMPOSITE_REDIRECT_MANUAL));
    if (e) {
        printf("RedirectSubwindows(root, Manual): error %u (another compositor?)\n",
               e->error_code);
        return 1;
    }
    uint32_t sub = XCB_EVENT_MASK_SUBSTRUCTURE_NOTIFY | XCB_EVENT_MASK_STRUCTURE_NOTIFY;
    xcb_change_window_attributes(comp, root, XCB_CW_EVENT_MASK, &sub);
    xregion = xcb_generate_id(comp);
    xcb_xfixes_create_region(comp, xregion, 0, NULL);

    const char *which = "abcde";
    for (const char *s = which; *s; s++)
        for (int bg = 0; bg < 2; bg++)
            scenario(*s, bg);
    printf("done\n");
    xcb_disconnect(app);
    xcb_disconnect(comp);
    return 0;
}
