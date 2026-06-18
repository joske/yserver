/*
 * barrier-smoke.c — deterministic XFIXES/XInput2 pointer-barrier test client.
 *
 * Creates a pointer barrier (default: a vertical line down the middle of the
 * root window — on a symmetric dual-head setup that is the monitor seam),
 * selects XI2 BarrierHit/BarrierLeave on the root, and prints every event.
 * Accumulates pressure (sum of |dx| or |dy|) and, once it crosses a
 * threshold, calls XIBarrierReleasePointer so a *firm* push gets through
 * while a gentle one is held — exactly the Mutter/muffin pressure-barrier
 * contract.
 *
 * This is the HW-smoke harness for the XFIXES pointer-barriers feature
 * (audit T13). Run it as the SOLE client under yserver from a TTY, then run
 * the identical binary under Xorg and diff the stdout / xtrace — Xorg is the
 * de-facto spec.
 *
 * Build:  gcc tools/barrier-smoke.c -lX11 -lXfixes -lXi -o ./barrier-smoke
 * Run:    DISPLAY=:7 ./barrier-smoke                 # vertical seam, both dirs blocked
 *         DISPLAY=:7 ./barrier-smoke 1920 0 1920 2160 # explicit x1 y1 x2 y2
 *         DISPLAY=:7 ./barrier-smoke --horizontal      # horizontal line at mid-height
 *         RELEASE=0 DISPLAY=:7 ./barrier-smoke         # never release -> pointer trapped
 *
 * Env:  RELEASE=<n>  pressure (px) before auto-release; 0 = never release
 *                    (trap test). Default 600.
 */
#include <X11/Xlib.h>
#include <X11/extensions/Xfixes.h>
#include <X11/extensions/XInput2.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

int main(int argc, char **argv) {
    Display *dpy = XOpenDisplay(NULL);
    if (!dpy) { fprintf(stderr, "barrier-smoke: cannot open display\n"); return 1; }
    Window root = DefaultRootWindow(dpy);
    int sw = DisplayWidth(dpy, DefaultScreen(dpy));
    int sh = DisplayHeight(dpy, DefaultScreen(dpy));

    /* XFIXES >= 5.0 is required for pointer barriers. */
    int xf_ev, xf_err, xf_major = 0, xf_minor = 0;
    if (!XFixesQueryExtension(dpy, &xf_ev, &xf_err) ||
        !XFixesQueryVersion(dpy, &xf_major, &xf_minor) ||
        xf_major < 5) {
        fprintf(stderr, "barrier-smoke: XFIXES >= 5.0 unavailable (got %d.%d)\n",
                xf_major, xf_minor);
        return 1;
    }

    /* XInput2 >= 2.3 for BarrierHit/BarrierLeave + XIBarrierReleasePointer. */
    int xi_opcode, xi_ev, xi_err;
    if (!XQueryExtension(dpy, "XInputExtension", &xi_opcode, &xi_ev, &xi_err)) {
        fprintf(stderr, "barrier-smoke: XInputExtension unavailable\n");
        return 1;
    }
    int xi_major = 2, xi_minor = 3;
    if (XIQueryVersion(dpy, &xi_major, &xi_minor) != Success ||
        xi_major * 10 + xi_minor < 23) {
        fprintf(stderr, "barrier-smoke: XI2 >= 2.3 unavailable (got %d.%d)\n",
                xi_major, xi_minor);
        return 1;
    }
    printf("barrier-smoke: XFIXES %d.%d, XI2 %d.%d, root=%dx%d\n",
           xf_major, xf_minor, xi_major, xi_minor, sw, sh);

    /* Decide geometry. */
    int x1, y1, x2, y2;
    int horizontal = 0;
    int argi = 1;
    if (argi < argc && strcmp(argv[argi], "--horizontal") == 0) { horizontal = 1; argi++; }
    if (argc - argi >= 4) {
        x1 = atoi(argv[argi]); y1 = atoi(argv[argi+1]);
        x2 = atoi(argv[argi+2]); y2 = atoi(argv[argi+3]);
    } else if (horizontal) {
        x1 = 0; y1 = sh / 2; x2 = sw; y2 = sh / 2;     /* horizontal line, mid-height */
    } else {
        x1 = sw / 2; y1 = 0; x2 = sw / 2; y2 = sh;     /* vertical line, mid-width (the seam) */
    }

    /* directions=0 blocks motion through the barrier in BOTH directions. */
    PointerBarrier b = XFixesCreatePointerBarrier(dpy, root, x1, y1, x2, y2,
                                                  0, 0, NULL);
    printf("barrier-smoke: created barrier 0x%lx at (%d,%d)-(%d,%d), block both dirs\n",
           (unsigned long)b, x1, y1, x2, y2);

    /* Select BarrierHit/BarrierLeave on the root. */
    XIEventMask em;
    unsigned char mask[XIMaskLen(XI_LASTEVENT)] = {0};
    XISetMask(mask, XI_BarrierHit);
    XISetMask(mask, XI_BarrierLeave);
    em.deviceid = XIAllMasterDevices;
    em.mask_len = sizeof(mask);
    em.mask = mask;
    XISelectEvents(dpy, root, &em, 1);
    XFlush(dpy);

    long release_at = 600;
    const char *rel_env = getenv("RELEASE");
    if (rel_env) release_at = atol(rel_env);
    if (release_at == 0)
        printf("barrier-smoke: RELEASE=0 -> never releasing (pointer-trap test). Ctrl-C to exit.\n");
    else
        printf("barrier-smoke: will release after %ld px of accumulated pressure\n", release_at);
    printf("barrier-smoke: push the pointer against the line now...\n");
    fflush(stdout);

    double pressure = 0;
    for (;;) {
        XEvent ev;
        XNextEvent(dpy, &ev);
        if (ev.xcookie.type != GenericEvent || ev.xcookie.extension != xi_opcode)
            continue;
        if (!XGetEventData(dpy, &ev.xcookie)) continue;

        if (ev.xcookie.evtype == XI_BarrierHit) {
            XIBarrierEvent *be = (XIBarrierEvent *)ev.xcookie.data;
            pressure += (be->dx < 0 ? -be->dx : be->dx) +
                        (be->dy < 0 ? -be->dy : be->dy);
            printf("HIT    eventid=%u barrier=0x%lx root=(%.1f,%.1f) d=(%.1f,%.1f) pressure=%.0f\n",
                   be->eventid, (unsigned long)be->barrier,
                   be->root_x, be->root_y, be->dx, be->dy, pressure);
            fflush(stdout);
            if (release_at != 0 && pressure >= release_at) {
                printf("RELEASE eventid=%u (pressure %.0f >= %ld)\n",
                       be->eventid, pressure, release_at);
                XIBarrierReleasePointer(dpy, be->deviceid, be->barrier, be->eventid);
                XFlush(dpy);
                pressure = 0;
            }
        } else if (ev.xcookie.evtype == XI_BarrierLeave) {
            XIBarrierEvent *be = (XIBarrierEvent *)ev.xcookie.data;
            printf("LEAVE  eventid=%u barrier=0x%lx root=(%.1f,%.1f)\n",
                   be->eventid, (unsigned long)be->barrier, be->root_x, be->root_y);
            fflush(stdout);
            pressure = 0;
        }
        XFreeEventData(dpy, &ev.xcookie);
    }
    return 0;
}
