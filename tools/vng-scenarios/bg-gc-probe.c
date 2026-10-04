/* Window background paints never take a client's GC state.
 *
 * Client A draws into a scratch pixmap through a GC with an unusual
 * state (GXxor, a partial plane mask, FillStippled, IncludeInferiors);
 * then A or a second client B gives a window W a new background and
 * clears it, maps a child M (its background painted by the server) and
 * unmaps a child K (W's background painted where K was). Xorg paints
 * all of these with its own GC (miPaintWindow, mi/miexpose.c): GXcopy,
 * all planes, solid or tiled, ClipByChildren. Then A or B makes a
 * MIT-SHM pixmap, which holds the segment's bytes whatever A's GC.
 * The probe logs each drawable's pixels as a colour histogram.
 *
 *   cc -O1 -o probe bg-gc-probe.c -lxcb -lxcb-shm
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ipc.h>
#include <sys/shm.h>
#include <xcb/shm.h>
#include <xcb/xcb.h>

#define OLD 0x123456u
#define NEW 0xeda870u
#define KBG 0x0000ffu
#define MBG 0x40c040u
#define WW 200
#define WH 150
#define KX 120
#define KY 90
#define MX 10
#define MY 90
#define CHW 60
#define CHH 40

static xcb_connection_t *a, *b;
static xcb_screen_t *s;
static xcb_pixmap_t scratch, stipple, tile;
static xcb_gcontext_t plain;

static void sync_server(xcb_connection_t *c)
{
    free(xcb_get_input_focus_reply(c, xcb_get_input_focus(c), NULL));
}

/* A draws into the scratch pixmap through `gc`. */
static void draw(xcb_gcontext_t gc)
{
    xcb_rectangle_t r = {0, 0, 32, 32};
    xcb_poly_fill_rectangle(a, scratch, gc, 1, &r);
    sync_server(a);
}

struct bin {
    uint32_t px;
    int n;
};

static int by_count(const void *l, const void *r)
{
    const struct bin *x = l, *y = r;
    if (x->n != y->n)
        return y->n - x->n;
    return x->px < y->px ? -1 : x->px > y->px;
}

/* The colours of `win` (w x h), leaving out the child rect `hole` when
 * it has a width. */
static void histogram(const char *name, xcb_window_t win, int w, int h, xcb_rectangle_t hole)
{
    xcb_get_image_reply_t *r = xcb_get_image_reply(
        a, xcb_get_image(a, XCB_IMAGE_FORMAT_Z_PIXMAP, win, 0, 0, w, h, ~0u), NULL);
    printf("  %s:", name);
    if (!r) {
        printf(" GetImage failed\n");
        return;
    }
    const uint32_t *img = (const uint32_t *)xcb_get_image_data(r);
    struct bin bins[16];
    int nb = 0, other = 0;
    for (int y = 0; y < h; y++)
        for (int x = 0; x < w; x++) {
            if (hole.width && x >= hole.x && x < hole.x + hole.width && y >= hole.y &&
                y < hole.y + hole.height)
                continue;
            uint32_t px = img[y * w + x] & 0xffffff;
            int i = 0;
            while (i < nb && bins[i].px != px)
                i++;
            if (i == nb) {
                if (nb == 16) {
                    other++;
                    continue;
                }
                bins[nb].px = px;
                bins[nb++].n = 0;
            }
            bins[i].n++;
        }
    qsort(bins, nb, sizeof bins[0], by_count);
    for (int i = 0; i < nb; i++)
        printf(" %06x=%d", bins[i].px, bins[i].n);
    if (other)
        printf(" other=%d", other);
    printf("\n");
    free(r);
}

static xcb_window_t window(xcb_connection_t *c, xcb_window_t parent, int x, int y, int w, int h,
                           uint32_t bg)
{
    xcb_window_t win = xcb_generate_id(c);
    uint32_t v[] = {bg, 1};
    xcb_create_window(c, XCB_COPY_FROM_PARENT, win, parent, x, y, w, h, 0,
                      XCB_WINDOW_CLASS_INPUT_OUTPUT, XCB_COPY_FROM_PARENT,
                      XCB_CW_BACK_PIXEL | XCB_CW_OVERRIDE_REDIRECT, v);
    return win;
}

/* One stage: `gc` is A's drawing state, `actor` changes and clears W,
 * maps M and unmaps K; `tiled` gives W a background pixmap instead. */
static void stage(const char *name, xcb_gcontext_t gc, xcb_connection_t *actor, int tiled)
{
    printf("%s, by %s%s\n", name, actor == a ? "A" : "B", tiled ? ", tiled" : "");
    draw(plain);
    xcb_window_t w = window(a, s->root, 20, 20, WW, WH, OLD);
    xcb_window_t k = window(a, w, KX, KY, CHW, CHH, KBG);
    xcb_window_t m = window(a, w, MX, MY, CHW, CHH, MBG);
    xcb_map_window(a, k);
    xcb_map_window(a, w);
    sync_server(a);

    draw(gc);
    if (tiled)
        xcb_change_window_attributes(actor, w, XCB_CW_BACK_PIXMAP, &tile);
    else
        xcb_change_window_attributes(actor, w, XCB_CW_BACK_PIXEL, (uint32_t[]){NEW});
    xcb_clear_area(actor, 0, w, 0, 0, 0, 0);
    sync_server(actor);
    printf(" cleared\n");
    histogram("W", w, WW, WH, (xcb_rectangle_t){KX, KY, CHW, CHH});
    histogram("K", k, CHW, CHH, (xcb_rectangle_t){0});

    draw(gc);
    xcb_map_window(actor, m);
    sync_server(actor);
    draw(gc);
    xcb_unmap_window(actor, k);
    sync_server(actor);
    printf(" M mapped, K unmapped\n");
    histogram("W", w, WW, WH, (xcb_rectangle_t){MX, MY, CHW, CHH});
    histogram("M", m, CHW, CHH, (xcb_rectangle_t){0});

    xcb_destroy_window(a, w);
    sync_server(a);
    fflush(stdout);
}

/* `actor` makes a 32x32 MIT-SHM pixmap of OLD after A drew through `gc`. */
static void shm_stage(const char *name, xcb_gcontext_t gc, xcb_connection_t *actor, uint32_t *mem,
                      xcb_shm_seg_t seg)
{
    printf("%s, by %s, ShmCreatePixmap\n", name, actor == a ? "A" : "B");
    for (int i = 0; i < 32 * 32; i++)
        mem[i] = OLD;
    draw(gc);
    xcb_pixmap_t p = xcb_generate_id(actor);
    xcb_shm_create_pixmap(actor, p, s->root, 32, 32, s->root_depth, seg, 0);
    sync_server(actor);
    histogram("P", p, 32, 32, (xcb_rectangle_t){0});
    xcb_free_pixmap(actor, p);
    sync_server(actor);
    fflush(stdout);
}

static xcb_gcontext_t gc_with(uint32_t mask, const uint32_t *values)
{
    xcb_gcontext_t gc = xcb_generate_id(a);
    xcb_create_gc(a, gc, scratch, mask, values);
    return gc;
}

int main(void)
{
    a = xcb_connect(NULL, NULL);
    b = xcb_connect(NULL, NULL);
    if (xcb_connection_has_error(a) || xcb_connection_has_error(b)) {
        printf("cannot connect\n");
        return 1;
    }
    s = xcb_setup_roots_iterator(xcb_get_setup(a)).data;
    scratch = xcb_generate_id(a);
    xcb_create_pixmap(a, s->root_depth, scratch, s->root, 32, 32);

    /* A 2x2 checker bitmap for FillStippled. */
    stipple = xcb_generate_id(a);
    xcb_create_pixmap(a, 1, stipple, s->root, 2, 2);
    xcb_gcontext_t one = xcb_generate_id(a);
    xcb_create_gc(a, one, stipple, XCB_GC_FOREGROUND, (uint32_t[]){0});
    xcb_rectangle_t all = {0, 0, 2, 2};
    xcb_poly_fill_rectangle(a, stipple, one, 1, &all);
    xcb_change_gc(a, one, XCB_GC_FOREGROUND, (uint32_t[]){1});
    xcb_point_t diag[] = {{0, 0}, {1, 1}};
    xcb_poly_point(a, XCB_COORD_MODE_ORIGIN, stipple, one, 2, diag);

    /* A 4x4 background tile: NEW with a 2x2 KBG corner. */
    tile = xcb_generate_id(a);
    xcb_create_pixmap(a, s->root_depth, tile, s->root, 4, 4);
    xcb_gcontext_t paint = xcb_generate_id(a);
    xcb_create_gc(a, paint, tile, XCB_GC_FOREGROUND, (uint32_t[]){NEW});
    xcb_rectangle_t whole = {0, 0, 4, 4}, corner = {0, 0, 2, 2};
    xcb_poly_fill_rectangle(a, tile, paint, 1, &whole);
    xcb_change_gc(a, paint, XCB_GC_FOREGROUND, (uint32_t[]){KBG});
    xcb_poly_fill_rectangle(a, tile, paint, 1, &corner);

    plain = gc_with(XCB_GC_FOREGROUND, (uint32_t[]){0x808080});
    xcb_gcontext_t xor = gc_with(XCB_GC_FUNCTION | XCB_GC_FOREGROUND,
                                 (uint32_t[]){XCB_GX_XOR, 0x999999});
    xcb_gcontext_t planes = gc_with(XCB_GC_PLANE_MASK | XCB_GC_FOREGROUND,
                                    (uint32_t[]){0x00ff00, 0xffffff});
    xcb_gcontext_t stippled = gc_with(XCB_GC_FOREGROUND | XCB_GC_FILL_STYLE | XCB_GC_STIPPLE,
                                      (uint32_t[]){0xff0000, XCB_FILL_STYLE_STIPPLED, stipple});
    xcb_gcontext_t inferiors = gc_with(XCB_GC_FOREGROUND | XCB_GC_SUBWINDOW_MODE,
                                       (uint32_t[]){0xff00ff, XCB_SUBWINDOW_MODE_INCLUDE_INFERIORS});
    sync_server(a);

    xcb_connection_t *actors[] = {b, a};
    for (int i = 0; i < 2; i++) {
        stage("GXxor", xor, actors[i], 0);
        stage("plane mask 0x00ff00", planes, actors[i], 0);
        stage("FillStippled", stippled, actors[i], 0);
        stage("IncludeInferiors", inferiors, actors[i], 0);
        stage("GXxor", xor, actors[i], 1);
    }

    int id = shmget(IPC_PRIVATE, 32 * 32 * 4, IPC_CREAT | 0600);
    uint32_t *mem = id < 0 ? NULL : shmat(id, NULL, 0);
    if (!mem || mem == (void *)-1) {
        printf("no shared memory\n");
        return 1;
    }
    xcb_shm_seg_t seg_a = xcb_generate_id(a), seg_b = xcb_generate_id(b);
    xcb_shm_attach(a, seg_a, id, 0);
    xcb_shm_attach(b, seg_b, id, 0);
    sync_server(a);
    sync_server(b);
    shmctl(id, IPC_RMID, NULL);
    xcb_shm_seg_t segs[] = {seg_b, seg_a};
    for (int i = 0; i < 2; i++) {
        shm_stage("GXxor", xor, actors[i], mem, segs[i]);
        shm_stage("plane mask 0x00ff00", planes, actors[i], mem, segs[i]);
    }

    FILE *done = fopen("PROBE-DONE", "w");
    if (done)
        fclose(done);
    xcb_disconnect(b);
    xcb_disconnect(a);
    return 0;
}
