/* #196 follow-up: pixmaps a GC references, then freed by the client, must
 * not outlive the GC's last reference; nor may a cursor's sprite outlive the
 * cursor's last reference (XID, window, grab, animated cursor; Xorg
 * dix/cursor.c refcnt). Each variant runs CYCLES times and is
 * bracketed on stdout by "PHASE <name> <start|end> <unix seconds>", with a
 * quiet gap after it so the 1 Hz telemetry samples a settled counter.
 * Xorg: a clip mask is converted to a region at once (mi/migc.c:68); a tile
 * or stipple is held until replaced, CopyGC'd over or FreeGC (dix/gc.c). */
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>
#include <xcb/render.h>
#include <xcb/xcb.h>

#define CYCLES 200
#define DISCONNECTS 50
#define SIZE 64

static xcb_connection_t *c;
static xcb_screen_t *s;

static double now(void) {
    struct timespec t;
    clock_gettime(CLOCK_REALTIME, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

static void phase(const char *name, const char *edge) {
    printf("PHASE %s %s %.3f\n", name, edge, now());
    fflush(stdout);
}

static void sync_server(xcb_connection_t *conn) {
    free(xcb_get_input_focus_reply(conn, xcb_get_input_focus(conn), NULL));
}

static xcb_pixmap_t pixmap(xcb_connection_t *conn, uint8_t depth) {
    xcb_pixmap_t p = xcb_generate_id(conn);
    xcb_create_pixmap(conn, depth, p, s->root, SIZE, SIZE);
    return p;
}

static xcb_gcontext_t gc_on(xcb_connection_t *conn, xcb_drawable_t d) {
    xcb_gcontext_t g = xcb_generate_id(conn);
    xcb_create_gc(conn, g, d, 0, NULL);
    return g;
}

/* Run one variant, then let the server settle and telemetry sample it. */
static void run(const char *name, void (*body)(void)) {
    phase(name, "start");
    body();
    sync_server(c);
    phase(name, "end");
    sleep(4);
}

static xcb_gcontext_t gc_deep, gc_deep2;
static xcb_pixmap_t tile0, stip0;

static void clip(void) {
    for (int i = 0; i < CYCLES; i++) {
        xcb_pixmap_t p = pixmap(c, 1);
        xcb_change_gc(c, gc_deep, XCB_GC_CLIP_MASK, &p);
        xcb_free_pixmap(c, p);
        uint32_t none = XCB_NONE;
        xcb_change_gc(c, gc_deep, XCB_GC_CLIP_MASK, &none);
    }
}

static void clip_rects(void) {
    for (int i = 0; i < CYCLES; i++) {
        xcb_pixmap_t p = pixmap(c, 1);
        xcb_change_gc(c, gc_deep, XCB_GC_CLIP_MASK, &p);
        xcb_free_pixmap(c, p);
        xcb_rectangle_t r = {0, 0, 8, 8};
        xcb_set_clip_rectangles(c, XCB_CLIP_ORDERING_UNSORTED, gc_deep, 0, 0, 1, &r);
    }
}

static void tile(void) {
    for (int i = 0; i < CYCLES; i++) {
        xcb_pixmap_t p = pixmap(c, s->root_depth);
        xcb_change_gc(c, gc_deep, XCB_GC_TILE, &p);
        xcb_free_pixmap(c, p);
        xcb_change_gc(c, gc_deep, XCB_GC_TILE, &tile0);
    }
}

static void stipple(void) {
    for (int i = 0; i < CYCLES; i++) {
        xcb_pixmap_t p = pixmap(c, 1);
        xcb_change_gc(c, gc_deep, XCB_GC_STIPPLE, &p);
        xcb_free_pixmap(c, p);
        xcb_change_gc(c, gc_deep, XCB_GC_STIPPLE, &stip0);
    }
}

static void copy_gc(void) {
    /* gc_deep holds tile0 / stip0; copying them over gc_deep2 drops its own. */
    for (int i = 0; i < CYCLES; i++) {
        xcb_pixmap_t t = pixmap(c, s->root_depth), st = pixmap(c, 1);
        uint32_t v[2] = {t, st};
        xcb_change_gc(c, gc_deep2, XCB_GC_TILE | XCB_GC_STIPPLE, v);
        xcb_free_pixmap(c, t);
        xcb_free_pixmap(c, st);
        xcb_copy_gc(c, gc_deep, gc_deep2, XCB_GC_TILE | XCB_GC_STIPPLE);
    }
}

static void free_gc(void) {
    for (int i = 0; i < CYCLES; i++) {
        xcb_gcontext_t g = gc_on(c, s->root);
        xcb_pixmap_t t = pixmap(c, s->root_depth), st = pixmap(c, 1), cm = pixmap(c, 1);
        uint32_t v[3] = {t, st, cm};
        xcb_change_gc(c, g, XCB_GC_TILE | XCB_GC_STIPPLE | XCB_GC_CLIP_MASK, v);
        xcb_free_pixmap(c, t);
        xcb_free_pixmap(c, st);
        xcb_free_pixmap(c, cm);
        xcb_free_gc(c, g);
    }
}

static void disconnect(void) {
    for (int i = 0; i < DISCONNECTS; i++) {
        xcb_connection_t *k = xcb_connect(NULL, NULL);
        if (xcb_connection_has_error(k)) { fprintf(stderr, "connect %d failed\n", i); return; }
        xcb_gcontext_t g = gc_on(k, s->root);
        xcb_pixmap_t t = pixmap(k, s->root_depth), st = pixmap(k, 1), cm = pixmap(k, 1);
        uint32_t v[3] = {t, st, cm};
        xcb_change_gc(k, g, XCB_GC_TILE | XCB_GC_STIPPLE | XCB_GC_CLIP_MASK, v);
        xcb_free_pixmap(k, t);
        xcb_free_pixmap(k, st);
        xcb_free_pixmap(k, cm);
        sync_server(k);
        xcb_disconnect(k); /* the GC still holds all three */
    }
}

static xcb_cursor_t pixmap_cursor(xcb_connection_t *conn) {
    xcb_pixmap_t p = pixmap(conn, 1);
    xcb_cursor_t cu = xcb_generate_id(conn);
    xcb_create_cursor(conn, cu, p, p, 0, 0, 0, 0xffff, 0xffff, 0xffff, 0, 0);
    xcb_free_pixmap(conn, p);
    return cu;
}

static xcb_font_t cursor_font;
static xcb_window_t win;

static xcb_window_t window(xcb_connection_t *conn) {
    xcb_window_t w = xcb_generate_id(conn);
    xcb_create_window(conn, XCB_COPY_FROM_PARENT, w, s->root, 0, 0, 32, 32, 0,
                      XCB_WINDOW_CLASS_INPUT_OUTPUT, XCB_COPY_FROM_PARENT, 0, NULL);
    return w;
}

static void cursor_free(void) {
    for (int i = 0; i < CYCLES; i++) {
        xcb_free_cursor(c, pixmap_cursor(c));
        xcb_cursor_t g = xcb_generate_id(c);
        xcb_create_glyph_cursor(c, g, cursor_font, cursor_font, 68, 69, 0, 0, 0, 0xffff, 0xffff, 0xffff);
        xcb_free_cursor(c, g);
    }
}

static void cursor_window(void) {
    for (int i = 0; i < CYCLES; i++) {
        xcb_cursor_t cu = pixmap_cursor(c);
        xcb_change_window_attributes(c, win, XCB_CW_CURSOR, &cu);
        xcb_free_cursor(c, cu);
        uint32_t none = XCB_NONE;
        xcb_change_window_attributes(c, win, XCB_CW_CURSOR, &none);
    }
}

static void cursor_destroy(void) {
    for (int i = 0; i < CYCLES; i++) {
        xcb_window_t w = window(c);
        xcb_cursor_t cu = pixmap_cursor(c);
        xcb_change_window_attributes(c, w, XCB_CW_CURSOR, &cu);
        xcb_free_cursor(c, cu);
        xcb_destroy_window(c, w);
    }
}

static void cursor_grab(void) {
    for (int i = 0; i < CYCLES; i++) {
        xcb_cursor_t cu = pixmap_cursor(c);
        free(xcb_grab_pointer_reply(c, xcb_grab_pointer(c, 0, s->root, 0, XCB_GRAB_MODE_ASYNC,
                                                        XCB_GRAB_MODE_ASYNC, XCB_NONE, cu,
                                                        XCB_CURRENT_TIME), NULL));
        xcb_free_cursor(c, cu);
        xcb_ungrab_pointer(c, XCB_CURRENT_TIME);
    }
}

static void cursor_anim(void) {
    for (int i = 0; i < CYCLES; i++) {
        xcb_render_animcursorelt_t f[2] = {{pixmap_cursor(c), 50}, {pixmap_cursor(c), 50}};
        xcb_cursor_t a = xcb_generate_id(c);
        xcb_render_create_anim_cursor(c, a, 2, f);
        xcb_free_cursor(c, f[0].cursor);
        xcb_free_cursor(c, f[1].cursor);
        xcb_free_cursor(c, a);
    }
}

static void cursor_disconnect(void) {
    for (int i = 0; i < DISCONNECTS; i++) {
        xcb_connection_t *k = xcb_connect(NULL, NULL);
        if (xcb_connection_has_error(k)) { fprintf(stderr, "connect %d failed\n", i); return; }
        xcb_window_t w = window(k);
        xcb_cursor_t cu = pixmap_cursor(k);
        xcb_change_window_attributes(k, w, XCB_CW_CURSOR, &cu);
        sync_server(k);
        xcb_disconnect(k); /* window and cursor both still live */
    }
}

int main(void) {
    c = xcb_connect(NULL, NULL);
    if (xcb_connection_has_error(c)) { fprintf(stderr, "no display\n"); return 1; }
    s = xcb_setup_roots_iterator(xcb_get_setup(c)).data;
    gc_deep = gc_on(c, s->root);
    gc_deep2 = gc_on(c, s->root);
    tile0 = pixmap(c, s->root_depth);
    stip0 = pixmap(c, 1);
    uint32_t v[2] = {tile0, stip0};
    xcb_change_gc(c, gc_deep, XCB_GC_TILE | XCB_GC_STIPPLE, v);
    cursor_font = xcb_generate_id(c);
    xcb_open_font(c, cursor_font, 6, "cursor");
    win = window(c);
    xcb_render_query_version_reply_t *rv =
        xcb_render_query_version_reply(c, xcb_render_query_version(c, 0, 11), NULL);
    free(rv);
    sync_server(c);
    sleep(2);
    phase("baseline", "start");
    phase("baseline", "end");
    sleep(4);
    run("clip", clip);
    run("clip_rects", clip_rects);
    run("tile", tile);
    run("stipple", stipple);
    run("copy_gc", copy_gc);
    run("free_gc", free_gc);
    run("disconnect", disconnect);
    run("cursor_free", cursor_free);
    run("cursor_window", cursor_window);
    run("cursor_destroy", cursor_destroy);
    run("cursor_grab", cursor_grab);
    run("cursor_anim", cursor_anim);
    run("cursor_disconnect", cursor_disconnect);
    xcb_disconnect(c);
    return 0;
}
