/*
 * replay-propagation-probe — where does AllowEvents(ReplayPointer) put the
 * press, when the window under the pointer selected XI2 but not core?
 *
 * Issue #141: under OpenBox, a window spontaneously starts following the
 * pointer (sometimes resizing instead), a click stops it, and it never
 * happens on Xorg. The xtrace capture (2026-09-11, air) showed OpenBox
 * re-entering interactive moveresize with NO triggering X event at all —
 * no ButtonPress, no _NET_WM_MOVERESIZE, no key event, no modifier. The
 * only mechanism that fits is OpenBox's mouse.c firing its Drag binding
 * from a bare MotionNotify while its internal `button` is still set,
 * using the press position it remembers — which is also why the window
 * warps ~1000px before it starts tracking.
 *
 * What could double-arm that state: in the capture OpenBox saw the SAME
 * press twice — once on the client window (its own passive grab
 * activating) and once on the frame (the replay propagating up, because
 * the GTK client selected XI2 and not core). So this probe asks the one
 * question that decides whether that second delivery is ours to fix:
 *
 *   after AllowEvents(ReplayPointer), who receives the replayed press
 *   for each combination of core/XI2 selection on the child?
 *
 * Layout, mirroring OpenBox: a "wm" connection owns parent P (selects
 * core ButtonPress, like a frame) and holds a passive button grab on the
 * child with pointer_mode=Sync — Sync is what makes ReplayPointer mean
 * anything. A separate "client" connection owns child C inside P and
 * selects core and/or XI2 per case.
 *
 * Run the SAME binary against Xorg and against yserver and diff the
 * table. Any row where they differ is the bug; a fully matching table
 * refutes this hypothesis and the hunt moves elsewhere.
 *
 *   cc -O1 -o /tmp/rpp tools/replay-propagation-probe.c -lX11 -lXi -lXtst
 *   DISPLAY=:19 /tmp/rpp
 *
 * Uses XTEST to place the pointer and click, so it needs no human. It
 * warps the pointer, so prefer a nested server (Xephyr) over a live
 * desktop.
 */
#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <X11/extensions/XInput2.h>
#include <X11/extensions/XTest.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define P_W 400
#define P_H 300
#define C_X 40
#define C_Y 40
#define C_W 200
#define C_H 150

enum { SEL_XI2_ONLY, SEL_CORE_ONLY, SEL_BOTH, SEL_NEITHER, N_CASES };

static const char *case_name(int c)
{
    switch (c) {
    case SEL_XI2_ONLY:  return "child selects XI2 only  ";
    case SEL_CORE_ONLY: return "child selects core only ";
    case SEL_BOTH:      return "child selects both      ";
    default:            return "child selects neither   ";
    }
}

struct result {
    int wm_presses;        /* core presses the "wm" connection saw */
    Window wm_first_win;   /* event window of the 1st (grab activation) */
    Window wm_second_win;  /* event window of the 2nd (the replay), if any */
    int client_core;       /* core presses the client connection saw */
    int client_xi2;        /* XI2 presses the client connection saw */
};

/* Drain both connections for `ms`, tallying presses. */
static void collect(Display *wm, Display *cl, int xi_opcode, int ms,
                    struct result *r)
{
    for (int i = 0; i < ms; i++) {
        while (XPending(wm)) {
            XEvent e;
            XNextEvent(wm, &e);
            if (e.type == ButtonPress) {
                if (r->wm_presses == 0)
                    r->wm_first_win = e.xbutton.window;
                else if (r->wm_presses == 1)
                    r->wm_second_win = e.xbutton.window;
                r->wm_presses++;
            }
        }
        while (XPending(cl)) {
            XEvent e;
            XNextEvent(cl, &e);
            if (e.type == ButtonPress) {
                r->client_core++;
            } else if (e.type == GenericEvent) {
                XGenericEventCookie *ck = &e.xcookie;
                if (ck->extension == xi_opcode && XGetEventData(cl, ck)) {
                    if (ck->evtype == XI_ButtonPress)
                        r->client_xi2++;
                    XFreeEventData(cl, ck);
                }
            }
        }
        usleep(1000);
    }
}

/* Non-fatal: a diagnostic must print its table even if cleanup errors,
 * and the two servers may well disagree about which requests error —
 * that is data, not a reason to abort. */
static int x_errors;
static int swallow_error(Display *d, XErrorEvent *e)
{
    char buf[128];
    XGetErrorText(d, e->error_code, buf, sizeof buf);
    fprintf(stderr, "  [x-error] %s on major %d (resource 0x%lx)\n", buf,
            e->request_code, e->resourceid);
    x_errors++;
    return 0;
}

int main(void)
{
    XSetErrorHandler(swallow_error);
    Display *wm = XOpenDisplay(NULL);
    if (!wm) {
        fprintf(stderr, "cannot open display\n");
        return 2;
    }
    Display *cl = XOpenDisplay(NULL);
    if (!cl) {
        fprintf(stderr, "cannot open second connection\n");
        return 2;
    }

    int xi_opcode, ev, err;
    if (!XQueryExtension(cl, "XInputExtension", &xi_opcode, &ev, &err)) {
        fprintf(stderr, "no XInputExtension\n");
        return 2;
    }
    int xi_major = 2, xi_minor = 0;
    if (XIQueryVersion(cl, &xi_major, &xi_minor) != Success) {
        fprintf(stderr, "no XI2\n");
        return 2;
    }
    int t_ev, t_err, t_major, t_minor;
    if (!XTestQueryExtension(wm, &t_ev, &t_err, &t_major, &t_minor)) {
        fprintf(stderr, "no XTEST — cannot drive input\n");
        return 2;
    }

    printf("server vendor: %s (release %d)\n", ServerVendor(wm),
           VendorRelease(wm));

    struct result results[N_CASES];
    memset(results, 0, sizeof results);

    for (int c = 0; c < N_CASES; c++) {
        /* Parent P, owned by the "wm" connection: selects core
         * ButtonPress exactly as a reparenting WM's frame does. */
        Window p = XCreateSimpleWindow(wm, DefaultRootWindow(wm), 0, 0,
                                       P_W, P_H, 0, 0,
                                       BlackPixel(wm, 0));
        /* override-redirect so no WM reparents or moves P: the synthetic
         * click is at fixed ROOT coordinates and has to land inside C.
         * Also keeps the probe safe to run on a live desktop — the click
         * cannot reach whatever was underneath. */
        XSetWindowAttributes swa;
        swa.override_redirect = True;
        XChangeWindowAttributes(wm, p, CWOverrideRedirect, &swa);
        XSelectInput(wm, p, ButtonPressMask | ButtonReleaseMask |
                                ExposureMask | StructureNotifyMask);
        XMapRaised(wm, p);
        XSync(wm, False);

        /* Child C, owned by the OTHER connection, created under P — the
         * reparented-client shape. */
        Window ch = XCreateSimpleWindow(cl, p, C_X, C_Y, C_W, C_H, 0, 0,
                                        WhitePixel(cl, 0));
        if (c == SEL_CORE_ONLY || c == SEL_BOTH)
            XSelectInput(cl, ch, ButtonPressMask);
        else
            XSelectInput(cl, ch, 0);
        if (c == SEL_XI2_ONLY || c == SEL_BOTH) {
            unsigned char mask[XIMaskLen(XI_LASTEVENT)];
            memset(mask, 0, sizeof mask);
            XISetMask(mask, XI_ButtonPress);
            XIEventMask em = { .deviceid = XIAllMasterDevices,
                               .mask_len = sizeof mask,
                               .mask = mask };
            XISelectEvents(cl, ch, &em, 1);
        }
        XMapWindow(cl, ch);
        XSync(cl, False);
        XSync(wm, False);

        /* The WM's passive grab on the CHILD, owner_events=False and
         * pointer_mode=Sync — openbox's click-to-focus grab, and Sync is
         * what ReplayPointer needs to have anything to replay. */
        XGrabButton(wm, Button1, AnyModifier, ch, False, ButtonPressMask,
                    GrabModeSync, GrabModeAsync, None, None);
        XSync(wm, False);

        /* Put the pointer inside C and click. */
        int px = C_X + C_W / 2, py = C_Y + C_H / 2;
        XTestFakeMotionEvent(wm, -1, px, py, 0);
        XSync(wm, False);
        usleep(20000);
        XTestFakeButtonEvent(wm, Button1, True, 0);
        XSync(wm, False);

        /* Let the activation arrive, THEN replay. */
        collect(wm, cl, xi_opcode, 120, &results[c]);
        XAllowEvents(wm, ReplayPointer, CurrentTime);
        XSync(wm, False);
        collect(wm, cl, xi_opcode, 200, &results[c]);

        XTestFakeButtonEvent(wm, Button1, False, 0);
        XSync(wm, False);
        collect(wm, cl, xi_opcode, 80, &results[c]);

        XUngrabButton(wm, Button1, AnyModifier, ch);
        XDestroyWindow(cl, ch);
        XSync(cl, False);
        XDestroyWindow(wm, p);
        XSync(wm, False);
        usleep(50000);

        /* Record P/C identity for the report. */
        printf("case %d: P=0x%lx C=0x%lx\n", c, p, ch);
    }

    printf("\n%-26s | wm core presses | 2nd to | client core | client XI2\n",
           "case");
    printf("---------------------------+-----------------+--------+-------------+-----------\n");
    for (int c = 0; c < N_CASES; c++) {
        struct result *r = &results[c];
        const char *second = r->wm_presses < 2 ? "-"
                             : (r->wm_second_win == r->wm_first_win ? "same win"
                                                                    : "PARENT");
        printf("%-26s | %15d | %-6s | %11d | %9d\n", case_name(c),
               r->wm_presses, second, r->client_core, r->client_xi2);
    }

    /* The row that matters for #141: XI2-only child. A second core press
     * to the WM there is the double-arming candidate. */
    struct result *x = &results[SEL_XI2_ONLY];
    printf("\nXI2-ONLY ROW: wm saw %d core press(es); client saw %d XI2, %d core.\n",
           x->wm_presses, x->client_xi2, x->client_core);
    printf("VERDICT: replayed press %s to the WM's parent window.\n",
           x->wm_presses >= 2 ? "IS propagated" : "is NOT propagated");
    printf("Compare this table between Xorg and yserver; a differing row is the bug.\n");
    printf("x-errors during the run: %d\n", x_errors);

    XCloseDisplay(cl);
    XCloseDisplay(wm);
    return 0;
}
