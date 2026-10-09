/* RENDER glyph churn: every round creates an A8 and an ARGB32 glyphset of
 * 128 distinct 64x64 glyphs, draws them all with CompositeGlyphs32 (solid
 * red, Over) into a white 1024x512 pixmap, reads it back and counts the
 * pixels that differ from the glyphs' own bitmaps, then frees the set.
 * Coverage is only ever 0 or full, so the expected pixel is exact. The
 * rounds together need several times a 4096^2 glyph atlas; a server that
 * never reclaims glyph space drops the later rounds' glyphs. One A8 set
 * lives through the churn and is redrawn every round. Before and
 * after the churn a fresh glyphset redefines one glyph id (FreeGlyphs +
 * AddGlyphs, then AddGlyphs over a live id) and each draw must show the
 * image that id was last given.
 *
 *   cc -O1 -o probe glyph-atlas-probe.c -lxcb -lxcb-render
 *
 * Exits 1 if any pixel differs; per-round timings go to stderr.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <xcb/render.h>
#include <xcb/xcb.h>

#define G 64
#define COLS 16
#define ROWS 8
#define N (COLS * ROWS)
#define PW (COLS * G)
#define PH (ROWS * G)
#define ROUNDS 16
#define RED 0xff0000u
#define WHITE 0xffffffu

static xcb_connection_t *c;
static xcb_screen_t *s;
static xcb_render_pictformat_t fmt_a8, fmt_argb, fmt_rgb24;
static xcb_pixmap_t pix;
static xcb_render_picture_t dst, red;
static long total_bad;

static void sync_server(void)
{
    free(xcb_get_input_focus_reply(c, xcb_get_input_focus(c), NULL));
}

/* Whether texel (x, y) of glyph `id` in pattern `seed` is covered: 8x8
 * blocks from a hash, so no two glyphs or rounds share a bitmap. */
static int on(uint32_t seed, uint32_t id, int x, int y)
{
    uint32_t h = seed * 0x9e3779b1u ^ id * 0x85ebca6bu ^ (uint32_t)(y / 8 * 8 + x / 8) * 0xc2b2ae35u;
    h ^= h >> 15;
    h *= 0x2c1b3c6du;
    h ^= h >> 12;
    return (h & 3) != 0;
}

static void find_formats(void)
{
    xcb_render_query_pict_formats_reply_t *pf =
        xcb_render_query_pict_formats_reply(c, xcb_render_query_pict_formats(c), NULL);
    for (xcb_render_pictforminfo_iterator_t i = xcb_render_query_pict_formats_formats_iterator(pf);
         i.rem; xcb_render_pictforminfo_next(&i)) {
        xcb_render_directformat_t d = i.data->direct;
        if (i.data->type != XCB_RENDER_PICT_TYPE_DIRECT)
            continue;
        if (i.data->depth == 8 && d.alpha_mask == 0xff && !d.red_mask)
            fmt_a8 = i.data->id;
        if (i.data->depth == 32 && d.alpha_mask == 0xff && d.alpha_shift == 24 &&
            d.red_shift == 16 && d.red_mask == 0xff)
            fmt_argb = i.data->id;
        if (i.data->depth == 24 && !d.alpha_mask && d.red_shift == 16 && d.red_mask == 0xff)
            fmt_rgb24 = i.data->id;
    }
    free(pf);
}

/* Add glyphs ids[0..n) of pattern `seed` to `gs`, eight per request. */
static void add(xcb_render_glyphset_t gs, int argb, uint32_t seed, const uint32_t *ids, int n)
{
    int bpp = argb ? 4 : 1;
    for (int at = 0; at < n; at += 8) {
        int k = n - at < 8 ? n - at : 8;
        xcb_render_glyphinfo_t info[8];
        static uint8_t data[8 * G * G * 4];
        for (int j = 0; j < k; j++) {
            info[j] = (xcb_render_glyphinfo_t){G, G, 0, 0, 0, 0};
            uint8_t *p = data + (size_t)j * G * G * bpp;
            for (int y = 0; y < G; y++)
                for (int x = 0; x < G; x++)
                    memset(p + ((size_t)y * G + x) * bpp, on(seed, ids[at + j], x, y) ? 0xff : 0,
                           bpp);
        }
        xcb_render_add_glyphs(c, gs, k, ids + at, info, (uint32_t)k * G * G * bpp, data);
    }
}

/* Draw glyph ids[i] at cell i of the white pixmap. */
static void draw(xcb_render_glyphset_t gs, xcb_render_pictformat_t mask, const uint32_t *ids,
                 int n)
{
    xcb_rectangle_t all = {0, 0, PW, PH};
    xcb_render_fill_rectangles(c, XCB_RENDER_PICT_OP_SRC, dst,
                               (xcb_render_color_t){0xffff, 0xffff, 0xffff, 0xffff}, 1, &all);
    /* One element per glyph: count, pad[3], dx, dy, then the CARD32 id. */
    static uint8_t items[N * 12];
    int px = 0, py = 0;
    for (int i = 0; i < n; i++) {
        int x = i % COLS * G, y = i / COLS * G;
        uint8_t *e = items + i * 12;
        memset(e, 0, 12);
        e[0] = 1;
        int16_t dx = (int16_t)(x - px), dy = (int16_t)(y - py);
        memcpy(e + 4, &dx, 2);
        memcpy(e + 6, &dy, 2);
        memcpy(e + 8, &ids[i], 4);
        px = x;
        py = y;
    }
    xcb_render_composite_glyphs_32(c, XCB_RENDER_PICT_OP_OVER, red, dst, mask, gs, 0, 0,
                                   (uint32_t)n * 12, items);
}

/* Read the pixmap back and count cells whose pixels are not glyph
 * ids[i] of pattern seeds[i] in red on white. */
static long check(const char *what, const uint32_t *ids, const uint32_t *seeds, int n)
{
    xcb_get_image_reply_t *r = xcb_get_image_reply(
        c, xcb_get_image(c, XCB_IMAGE_FORMAT_Z_PIXMAP, pix, 0, 0, PW, PH, ~0u), NULL);
    if (!r || xcb_get_image_data_length(r) < PW * PH * 4) {
        printf("%s: GetImage failed\n", what);
        free(r);
        return PW * PH;
    }
    const uint32_t *img = (const uint32_t *)xcb_get_image_data(r);
    long bad = 0;
    int bad_cells = 0;
    for (int i = 0; i < N; i++) {
        long cb = 0;
        for (int y = 0; y < G; y++)
            for (int x = 0; x < G; x++) {
                uint32_t want = i < n && on(seeds[i], ids[i], x, y) ? RED : WHITE;
                uint32_t got = img[(i / COLS * G + y) * PW + i % COLS * G + x] & 0xffffff;
                cb += got != want;
            }
        bad += cb;
        bad_cells += cb != 0;
    }
    printf("%s: bad_pixels=%ld bad_glyphs=%d\n", what, bad, bad_cells);
    free(r);
    total_bad += bad;
    return bad;
}

/* A fresh A8 glyphset's glyph id 7 drawn, then redefined by FreeGlyphs +
 * AddGlyphs, then by AddGlyphs over the live id: each draw must show the
 * image it was last given. */
static void redefine(const char *when, uint32_t seed)
{
    xcb_render_glyphset_t gs = xcb_generate_id(c);
    xcb_render_create_glyph_set(c, gs, fmt_a8);
    uint32_t id = 7;
    char what[96];
    add(gs, 0, seed, &id, 1);
    draw(gs, fmt_a8, &id, 1);
    snprintf(what, sizeof what, "redefine %s: new glyphset", when);
    check(what, &id, &seed, 1);
    xcb_render_free_glyphs(c, gs, 1, &id);
    seed++;
    add(gs, 0, seed, &id, 1);
    draw(gs, fmt_a8, &id, 1);
    snprintf(what, sizeof what, "redefine %s: after FreeGlyphs + AddGlyphs", when);
    check(what, &id, &seed, 1);
    seed++;
    add(gs, 0, seed, &id, 1);
    draw(gs, fmt_a8, &id, 1);
    snprintf(what, sizeof what, "redefine %s: AddGlyphs over a live id", when);
    check(what, &id, &seed, 1);
    xcb_render_free_glyph_set(c, gs);
}

static double now(void)
{
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

int main(void)
{
    c = xcb_connect(NULL, NULL);
    if (xcb_connection_has_error(c)) {
        printf("cannot connect\n");
        return 2;
    }
    s = xcb_setup_roots_iterator(xcb_get_setup(c)).data;
    find_formats();
    if (!fmt_a8 || !fmt_argb || !fmt_rgb24) {
        printf("missing a picture format\n");
        return 2;
    }
    pix = xcb_generate_id(c);
    xcb_create_pixmap(c, 24, pix, s->root, PW, PH);
    dst = xcb_generate_id(c);
    xcb_render_create_picture(c, dst, pix, fmt_rgb24, 0, NULL);
    red = xcb_generate_id(c);
    xcb_render_create_solid_fill(c, red, (xcb_render_color_t){0xffff, 0, 0, 0xffff});

    redefine("before churn", 1000);

    uint32_t ids[N], seeds[N], keep_seeds[N];
    for (int i = 0; i < N; i++) {
        ids[i] = 1000 + (uint32_t)i * 7;
        keep_seeds[i] = 500;
    }
    /* A set that lives through the churn, like an open terminal's. */
    xcb_render_glyphset_t keep = xcb_generate_id(c);
    xcb_render_create_glyph_set(c, keep, fmt_a8);
    add(keep, 0, 500, ids, N);
    for (int round = 0; round < ROUNDS; round++) {
        for (int argb = 0; argb < 2; argb++) {
            uint32_t seed = (uint32_t)round * 2 + (uint32_t)argb + 1;
            for (int i = 0; i < N; i++)
                seeds[i] = seed;
            xcb_render_glyphset_t gs = xcb_generate_id(c);
            xcb_render_create_glyph_set(c, gs, argb ? fmt_argb : fmt_a8);
            add(gs, argb, seed, ids, N);
            double t0 = now();
            draw(gs, argb ? fmt_argb : fmt_a8, ids, N);
            sync_server();
            fprintf(stderr, "round %d %s: draw+sync %.2f ms\n", round, argb ? "argb32" : "a8",
                    (now() - t0) * 1e3);
            char what[64];
            snprintf(what, sizeof what, "round %d %s", round, argb ? "argb32" : "a8");
            check(what, ids, seeds, N);
            xcb_render_free_glyph_set(c, gs);
        }
        double t0 = now();
        draw(keep, fmt_a8, ids, N);
        sync_server();
        fprintf(stderr, "round %d kept a8: draw+sync %.2f ms\n", round, (now() - t0) * 1e3);
        char what[64];
        snprintf(what, sizeof what, "round %d kept a8", round);
        check(what, ids, keep_seeds, N);
    }
    xcb_render_free_glyph_set(c, keep);

    redefine("after churn", 2000);
    sync_server();

    printf("total bad_pixels=%ld\n", total_bad);
    fclose(fopen("PROBE-DONE", "w"));
    xcb_disconnect(c);
    return total_bad ? 1 : 0;
}
