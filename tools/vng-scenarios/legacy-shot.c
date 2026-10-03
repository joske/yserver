/* GetImage of a top-level window, written as a PPM and summarised for a
 * golden: its size, the number of distinct colours and an FNV-1a hash of
 * the pixels, so two servers that agree pixel for pixel print one line.
 *
 *   ./legacy-shot WINDOW-ID NAME [frame]   # writes NAME.ppm
 *
 * With `frame`, the top-level the window manager put the window in. A
 * window reaching past the screen is read up to its edge.
 *
 *   cc -O1 -o legacy-shot legacy-shot.c -lxcb
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <xcb/xcb.h>

int main(int argc, char **argv)
{
    if (argc < 3)
        return 2;
    xcb_connection_t *c = xcb_connect(NULL, NULL);
    if (xcb_connection_has_error(c))
        return 1;
    xcb_window_t w = (xcb_window_t)strtoul(argv[1], NULL, 0);
    for (int up = argc > 3 && !strcmp(argv[3], "frame"); up;) {
        xcb_query_tree_reply_t *t = xcb_query_tree_reply(c, xcb_query_tree(c, w), NULL);
        if (!t)
            break;
        up = t->parent != t->root;
        if (up)
            w = t->parent;
        free(t);
    }
    xcb_get_geometry_reply_t *g =
        xcb_get_geometry_reply(c, xcb_get_geometry(c, w), NULL);
    if (!g) {
        printf("%s: no such window\n", argv[2]);
        return 1;
    }
    /* A window larger than the screen (dtpad without a window manager)
     * is read as far as the screen reaches. */
    xcb_screen_t *s = xcb_setup_roots_iterator(xcb_get_setup(c)).data;
    xcb_translate_coordinates_reply_t *at =
        xcb_translate_coordinates_reply(c, xcb_translate_coordinates(c, w, s->root, 0, 0), NULL);
    if (at) {
        if (at->dst_x >= 0 && at->dst_x + g->width > s->width_in_pixels)
            g->width = s->width_in_pixels - at->dst_x;
        if (at->dst_y >= 0 && at->dst_y + g->height > s->height_in_pixels)
            g->height = s->height_in_pixels - at->dst_y;
        free(at);
    }
    xcb_get_image_reply_t *r = xcb_get_image_reply(
        c, xcb_get_image(c, XCB_IMAGE_FORMAT_Z_PIXMAP, w, 0, 0, g->width, g->height, ~0u), NULL);
    if (!r) {
        printf("%s: GetImage failed (%ux%u+%d+%d)\n", argv[2], g->width, g->height, g->x, g->y);
        return 1;
    }
    const uint32_t *px = (const uint32_t *)xcb_get_image_data(r);
    int n = g->width * g->height;
    uint64_t hash = 1469598103934665603ull;
    uint32_t seen[64];
    int colours = 0;
    char path[256];
    snprintf(path, sizeof path, "%s.ppm", argv[2]);
    FILE *f = fopen(path, "wb");
    if (f)
        fprintf(f, "P6\n%d %d\n255\n", g->width, g->height);
    for (int i = 0; i < n; i++) {
        uint32_t p = px[i] & 0xffffff;
        for (int k = 0; k < 3; k++) {
            hash ^= (p >> (8 * k)) & 0xff;
            hash *= 1099511628211ull;
        }
        int known = 0;
        for (int k = 0; k < colours && !known; k++)
            known = seen[k] == p;
        if (!known && colours < 64)
            seen[colours++] = p;
        if (f) {
            fputc((int)(p >> 16) & 0xff, f);
            fputc((int)(p >> 8) & 0xff, f);
            fputc((int)p & 0xff, f);
        }
    }
    if (f)
        fclose(f);
    printf("%s: %ux%u, %d colours, pixels %016llx\n", argv[2], g->width, g->height, colours,
           (unsigned long long)hash);
    free(r);
    free(g);
    xcb_disconnect(c);
    return 0;
}
