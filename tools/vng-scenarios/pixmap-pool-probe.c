/* #196 pixmap-pool workload, driven by pixmap-pool.sh. Three phases, each
 * bracketed on stdout by "PHASE <name> <start|end> <unix seconds>":
 *   steady  STEADY_SECS of churn the pool exists for: 8 widget/menu-row
 *           sizes x4 every 50 ms, plus the reporter's pre-burst set (104
 *           sizes x4) cycled every 2 s;
 *   burst   BURST_SIZES distinct sizes <= 256, two pixmaps each, freed;
 *   idle    IDLE_SECS with no requests at all.
 * Every pixmap gets one fill so its storage is really used before it goes. */
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>
#include <xcb/xcb.h>

#define STEADY_SECS 90
#define BURST_SIZES 3000
#define IDLE_SECS 75

static xcb_connection_t *c;
static xcb_screen_t *s;
static xcb_gcontext_t gc;

static double now(void) {
    struct timespec t;
    clock_gettime(CLOCK_REALTIME, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

static void phase(const char *name, const char *edge) {
    printf("PHASE %s %s %.3f\n", name, edge, now());
    fflush(stdout);
}

static void sync_server(void) {
    free(xcb_get_input_focus_reply(c, xcb_get_input_focus(c), NULL));
}

/* Create n pixmaps of w x h, fill each, free them all, round-trip. */
static void cycle(int w, int h, int n) {
    xcb_pixmap_t p[8];
    for (int i = 0; i < n; i++) {
        p[i] = xcb_generate_id(c);
        xcb_create_pixmap(c, s->root_depth, p[i], s->root, w, h);
        xcb_rectangle_t r = {0, 0, w, h};
        xcb_poly_fill_rectangle(c, p[i], gc, 1, &r);
    }
    for (int i = 0; i < n; i++) xcb_free_pixmap(c, p[i]);
}

int main(void) {
    c = xcb_connect(NULL, NULL);
    if (xcb_connection_has_error(c)) { fprintf(stderr, "no display\n"); return 1; }
    s = xcb_setup_roots_iterator(xcb_get_setup(c)).data;
    gc = xcb_generate_id(c);
    uint32_t fg = 0x336699;
    xcb_create_gc(c, gc, s->root, XCB_GC_FOREGROUND, &fg);

    static const int fast[8][2] = {{16, 16}, {24, 24}, {32, 32}, {48, 48},
                                   {64, 64}, {230, 51}, {230, 57}, {230, 26}};
    phase("steady", "start");
    double t0 = now(), next_slow = t0;
    long fast_cycles = 0, slow_cycles = 0;
    while (now() - t0 < STEADY_SECS) {
        for (int i = 0; i < 8; i++) cycle(fast[i][0], fast[i][1], 4);
        fast_cycles++;
        if (now() >= next_slow) {
            for (int i = 0; i < 104; i++) cycle(16 + i, 16 + i % 7, 4);
            slow_cycles++;
            next_slow += 2.0;
        }
        sync_server();
        usleep(50000);
    }
    phase("steady", "end");
    printf("steady cycles: fast=%ld slow=%ld\n", fast_cycles, slow_cycles);

    sleep(3);
    phase("burst", "start");
    for (int i = 0; i < BURST_SIZES; i++) {
        cycle(17 + i % 240, 17 + 8 * (i / 240), 2); /* all distinct */
        if (i % 50 == 49) sync_server();
    }
    sync_server();
    phase("burst", "end");

    phase("idle", "start");
    sleep(IDLE_SECS);
    phase("idle", "end");
    xcb_disconnect(c);
    return 0;
}
