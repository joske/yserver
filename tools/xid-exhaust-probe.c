/* xid-exhaust-probe — system test for XC-MISC XID recycling.
 *
 * Allocates 1.25x the client's whole XID range via a create/free
 * pixmap loop. Without XC-MISC, xcb_generate_id() returns
 * 0xFFFFFFFF once the range is exhausted (the 2026-06-12 Cinnamon
 * WM death); with it, libxcb transparently recycles freed IDs and
 * the loop survives a full wrap.
 *
 * Build: cc -o target/xid-exhaust-probe tools/xid-exhaust-probe.c -lxcb
 * Run:   DISPLAY=:97 ./target/xid-exhaust-probe
 * Exit:  0 = PASS (recycling works), 1 = FAIL (exhausted), 2 = no display
 */
#include <stdio.h>
#include <stdlib.h>
#include <xcb/xcb.h>

int main(void) {
    xcb_connection_t *c = xcb_connect(NULL, NULL);
    if (!c || xcb_connection_has_error(c)) {
        fprintf(stderr, "cannot connect to display\n");
        return 2;
    }
    const xcb_setup_t *setup = xcb_get_setup(c);
    xcb_screen_t *screen = xcb_setup_roots_iterator(setup).data;
    uint32_t mask = setup->resource_id_mask;
    unsigned long target = (unsigned long)mask + (mask >> 2); /* 1.25x range */
    printf("resource_id_mask=0x%x; allocating %lu ids...\n", mask, target);

    for (unsigned long i = 0; i < target; i++) {
        uint32_t id = xcb_generate_id(c);
        if (id == 0xFFFFFFFF) {
            printf("FAIL: xcb_generate_id exhausted after %lu ids "
                   "(no XC-MISC recycling)\n", i);
            return 1;
        }
        xcb_create_pixmap(c, screen->root_depth, id, screen->root, 1, 1);
        xcb_free_pixmap(c, id);
        if ((i & 0xFFFF) == 0) {
            xcb_flush(c);
            /* drain any queued errors so the event queue stays flat */
            xcb_generic_event_t *ev;
            while ((ev = xcb_poll_for_event(c)) != NULL) {
                if (((ev->response_type & 0x7f)) == 0)
                    fprintf(stderr, "unexpected X error mid-loop\n");
                free(ev);
            }
        }
    }

    /* survived a full wrap — prove a recycled id really works */
    uint32_t id = xcb_generate_id(c);
    xcb_void_cookie_t ck =
        xcb_create_pixmap_checked(c, screen->root_depth, id, screen->root, 4, 4);
    xcb_generic_error_t *err = xcb_request_check(c, ck);
    if (err) {
        printf("FAIL: post-wrap CreatePixmap error code=%d\n", err->error_code);
        free(err);
        return 1;
    }
    printf("PASS: %lu ids allocated across the range wrap; recycling works\n",
           target);
    xcb_disconnect(c);
    return 0;
}
