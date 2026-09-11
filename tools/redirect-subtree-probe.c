/* redirect-subtree-probe — does redirecting a window reconstruct its whole
 * subtree correctly into the new backing?
 *
 * When a window W is redirected, Xorg allocates a pixmap and seeds it with
 * `CopyArea(parent, …, IncludeInferiors)` (`composite/compalloc.c:562`). That
 * single copy captures W AND everything inside it, because Xorg has no
 * per-window storage — it is all one screen pixmap. yserver keeps a separate
 * leaf per window, so it has to synthesize the same result by walking the
 * subtree (`overlay_backing_inferiors`) — and a walk can get wrong what a
 * single copy cannot: stacking order, nesting depth, and the compositing
 * operator used to lay each leaf down.
 *
 * That last one was wrong until 2026-09-11: the walk used PictOpOver, so a
 * child with alpha 0 blended as a no-op and left the seed showing through
 * instead of replacing it. CopyArea has no notion of alpha; the walk must
 * REPLACE. This probe is the regression test for that, and it deliberately
 * exercises the three things a raw-copy walk can still get wrong:
 *
 *   overlap   C1 and C2 overlap; C2 is mapped later so it is ABOVE. The
 *             overlap must read as C2. A walk in the wrong order reads C1.
 *   nesting   G is a child of C1, not of F. It must appear at its accumulated
 *             offset. A walk that only visits F's direct children loses it.
 *   alpha     C3 (alpha 0) and C4 (alpha 0x80) must land as their exact
 *             stored words. Under PictOpOver C3 vanished entirely and C4
 *             came out blended with whatever was beneath.
 *
 * The subtree is built and painted FIRST and redirection is turned on only
 * afterwards, so the reconstruction runs over an already-painted tree — the
 * real ordering, and the one where a missed inferior is visible.
 *
 * Every window is override-redirect and no WM runs, so nothing reparents or
 * restacks the tree under us.
 *
 * MEASURED BASELINE — filled in from X.Org 1.21.1.24 below; that table is the
 * contract, exactly as in tools/depth32-bg-probe.c.
 *
 * Build: cc -O1 -o redirect-subtree-probe redirect-subtree-probe.c -lX11 -lXcomposite
 * Run:   ./redirect-subtree-probe [--hold N]
 */

#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <X11/extensions/Xcomposite.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define F_X 100
#define F_Y 100
#define F_W 200
#define F_H 200

#define F_BG  0x00202020u /* dark grey — the frame's own background   */
#define C1_BG 0x00ff0000u /* red       — lower of the overlapping pair */
#define C2_BG 0x000000ffu /* blue      — upper of the overlapping pair */
#define G_BG  0x0000ff00u /* green     — nested one level deeper       */
#define C3_BG 0x00000000u /* alpha 0   — must REPLACE, not blend away  */
#define C4_BG 0x80ff0000u /* alpha 0x80 — must survive byte-exact      */

struct sample {
    const char *name;
    int x, y;            /* frame-local */
    unsigned long xorg;  /* measured on X.Org 1.21.1.24 */
};

/* Filled from the Xorg run; see MEASURED BASELINE above. */
static struct sample SAMPLES[] = {
    { "frame-bg",        5,   5,   0x00202020u },
    { "nested-G",        20,  20,  0x0000ff00u },
    { "C1-only",         50,  20,  0x00ff0000u },
    { "C1-C2-overlap",   70,  70,  0x000000ffu },
    { "C2-only",         110, 100, 0x000000ffu },
    { "C2-C4-overlap",   120, 130, 0x00ff0000u },
    { "C3-alpha0",       30,  150, 0x00000000u },
    { "C4-alpha80",      120, 150, 0x00ff0000u },
};
#define NSAMPLES ((int) (sizeof SAMPLES / sizeof SAMPLES[0]))

static int failures;

static Window mkwin(Display *dpy, Window parent, int depth, Visual *vis, Colormap cmap,
                    int x, int y, int w, int h, unsigned long bg)
{
    XSetWindowAttributes attr;
    unsigned long mask = CWBackPixel | CWOverrideRedirect | CWEventMask;
    attr.background_pixel = bg;
    attr.override_redirect = True;
    attr.event_mask = ExposureMask;
    if (cmap) {
        attr.colormap = cmap;
        attr.border_pixel = 0; /* required when the depth differs from parent */
        mask |= CWColormap | CWBorderPixel;
    }
    return XCreateWindow(dpy, parent, x, y, w, h, 0, depth, InputOutput, vis, mask, &attr);
}

int main(int argc, char **argv)
{
    int hold = 10;
    for (int i = 1; i < argc; i++) {
        if (strcmp(argv[i], "--hold") == 0 && i + 1 < argc) {
            hold = atoi(argv[++i]);
        } else {
            fprintf(stderr, "usage: %s [--hold N]\n", argv[0]);
            return 2;
        }
    }

    Display *dpy = XOpenDisplay(NULL);
    if (!dpy) {
        fprintf(stderr, "redirect-subtree-probe: cannot open display\n");
        return 1;
    }
    int screen = DefaultScreen(dpy);
    Window root = RootWindow(dpy, screen);
    Visual *vis24 = DefaultVisual(dpy, screen);
    int depth24 = DefaultDepth(dpy, screen);

    XVisualInfo vinfo;
    if (!XMatchVisualInfo(dpy, screen, 32, TrueColor, &vinfo)) {
        fprintf(stderr, "redirect-subtree-probe: no depth-32 TrueColor visual\n");
        return 1;
    }
    Colormap cmap32 = XCreateColormap(dpy, root, vinfo.visual, AllocNone);

    int ev, err;
    if (!XCompositeQueryExtension(dpy, &ev, &err)) {
        fprintf(stderr, "redirect-subtree-probe: no Composite extension\n");
        return 1;
    }

    Window f  = mkwin(dpy, root, depth24, vis24, 0, F_X, F_Y, F_W, F_H, F_BG);
    Window c1 = mkwin(dpy, f, depth24, vis24, 0, 10, 10, 80, 80, C1_BG);
    Window g  = mkwin(dpy, c1, depth24, vis24, 0, 5, 5, 30, 30, G_BG);
    Window c2 = mkwin(dpy, f, depth24, vis24, 0, 60, 60, 80, 80, C2_BG);
    Window c3 = mkwin(dpy, f, 32, vinfo.visual, cmap32, 10, 120, 60, 60, C3_BG);
    Window c4 = mkwin(dpy, f, 32, vinfo.visual, cmap32, 100, 120, 60, 60, C4_BG);

    /* Map the frame and C1 (with its nested G) first, then C2 — so C2 is
     * ABOVE C1 in the stack and the overlap has a defined winner. */
    XMapWindow(dpy, f);
    XMapWindow(dpy, c1);
    XMapWindow(dpy, g);
    XSync(dpy, False);
    XMapWindow(dpy, c2);
    XMapWindow(dpy, c3);
    XMapWindow(dpy, c4);
    XSync(dpy, False);
    sleep(1); /* let the tree actually paint before it gets reconstructed */

    /* NOW redirect. The reconstruction walk runs against a painted subtree,
     * which is the case a fresh-window path would never exercise. */
    XCompositeRedirectSubwindows(dpy, root, CompositeRedirectAutomatic);
    XSync(dpy, False);
    sleep(1);

    Pixmap pm = XCompositeNameWindowPixmap(dpy, f);
    if (!pm) {
        fprintf(stderr, "redirect-subtree-probe: NameWindowPixmap failed\n");
        return 1;
    }

    printf("# redirect-subtree-probe: frame=%d,%d %dx%d, read via NameWindowPixmap\n",
           F_X, F_Y, F_W, F_H);
    printf("# comparison is over the 24 defined bits; the frame pixmap's pad\n");
    printf("# byte is protocol-undefined and is shown but not graded.\n");
    printf("# %-16s %-8s %-10s %-10s %s\n", "sample", "at", "got", "xorg_21.1", "verdict");
    for (int i = 0; i < NSAMPLES; i++) {
        XImage *img = XGetImage(dpy, pm, SAMPLES[i].x, SAMPLES[i].y, 1, 1, AllPlanes, ZPixmap);
        if (!img) {
            printf("%-18s %3d,%-4d %-10s %08lx   READ-FAILED\n",
                   SAMPLES[i].name, SAMPLES[i].x, SAMPLES[i].y, "-", SAMPLES[i].xorg);
            failures++;
            continue;
        }
        unsigned long got = XGetPixel(img, 0, 0);
        XDestroyImage(img);
        /* Compare the 24 DEFINED bits only. Every sample is read out of the
         * frame's depth-24 pixmap, whose 8 pad bits are undefined by the
         * protocol; Xorg passes them through rather than normalising them
         * (fb/fbimage.c masks only when the replicated plane mask is not
         * all-ones), so they carry no contract. This still catches the bug
         * the probe exists for: under PictOpOver the alpha-0 child C3 blended
         * as a no-op and the frame's own grey showed through, which differs
         * in the RGB bits, not merely in the pad. */
        int ok = (got & 0x00ffffffu) == (SAMPLES[i].xorg & 0x00ffffffu);
        if (!ok)
            failures++;
        printf("%-18s %3d,%-4d %08lx   %08lx   %s\n",
               SAMPLES[i].name, SAMPLES[i].x, SAMPLES[i].y, got, SAMPLES[i].xorg,
               ok ? "ok" : "DIFF");
    }
    printf("%s\n", failures ? "VERDICT: subtree reconstruction DIFFERS from Xorg 21.1"
                            : "VERDICT: subtree reconstruction matches Xorg 21.1");
    fflush(stdout);

    for (int e = 0; hold <= 0 || e < hold * 10; e++) {
        while (XPending(dpy)) {
            XEvent xev;
            XNextEvent(dpy, &xev);
        }
        usleep(100000);
    }

    XFreePixmap(dpy, pm);
    XDestroyWindow(dpy, f); /* takes the whole subtree */
    XCompositeUnredirectSubwindows(dpy, root, CompositeRedirectAutomatic);
    XSync(dpy, False);
    XCloseDisplay(dpy);
    return failures ? 1 : 0;
}
