/* depth32-bg-probe — a purpose-written client for Xorg's depth-32
 * background-alpha rule. Runs against ANY X server, on a real desktop or in
 * the vng harness, and grades itself, so a session on unfamiliar hardware
 * answers the question without anyone relaying screenshots.
 *
 * THE RULE (mi/miexpose.c:487-511, under #ifdef COMPOSITE, comment verbatim
 * "Make sure alpha will sample as 1.0 for opaque windows"):
 *
 *     if (drawable->depth == 32) {
 *         int effective_depth = orig_pWin->drawable.depth;
 *         if (effective_depth == 32) {
 *             orig_pWin = orig_pWin->parent;
 *             while (orig_pWin && orig_pWin->parent) {
 *                 if (orig_pWin->drawable.depth == 24) {
 *                     effective_depth = 24;
 *                     break;
 *                 }
 *                 orig_pWin = orig_pWin->parent;
 *             }
 *         }
 *         if (effective_depth == 24)
 *             fill.pixel |= 0xff000000;
 *     }
 *
 * The `if (solid)` block this sits in serves BOTH PW_BACKGROUND and PW_BORDER,
 * so bg_pixel and border_pixel take the same treatment. Two details decide the
 * outcome and neither is guessable from spec prose:
 *
 *   1. The gate is `drawable->depth`. For PW_BACKGROUND `drawable` is never
 *      reassigned from `&pWin->drawable`, so the gate is the WINDOW's depth.
 *      (For PW_BORDER it IS reassigned, to GetWindowPixmap's pixmap, so that
 *      one gates on the BACKING depth. Different tests; don't copy one to the
 *      other.)
 *   2. The walk is `while (orig_pWin && orig_pWin->parent)` — it starts at the
 *      parent and stops BEFORE the root, whose depth is therefore never
 *      consulted. So "child of root" and "child of a depth-24 frame" land on
 *      opposite sides of the rule.
 *
 * THE PREDICTION, for a depth-32 window with no client drawing at all, only a
 * bg_pixel:
 *
 *   chain A, direct child of root  -> loop body never runs, effective_depth
 *                                     stays 32, alpha PRESERVED as given
 *   chain B, child of a depth-24
 *            override-redirect frame -> loop finds depth 24, alpha FORCED to
 *                                     0xff regardless of what the client asked
 *
 * so, per bg_pixel:
 *
 *     bg_pixel     chain A stored   chain B stored
 *     0x00ffffff   0x00ffffff       0xffffffff
 *     0x00000000   0x00000000       0xff000000
 *     0xffffffff   0xffffffff       0xffffffff
 *
 * The backdrops are chosen so the SCANOUT reads the same fact independently of
 * the readback: root is red, the depth-24 frames are blue. Anywhere alpha is
 * preserved AND something blends it, the backdrop shows through; anywhere it is
 * forced (or nothing blends), the window's own RGB shows as a solid block.
 * Note the two are only expected to agree when a compositor is redirecting —
 * this scenario deliberately runs none, so the scanout is the unblended case
 * and the readback is the load-bearing half.
 *
 * Readback is taken twice per window, because they can disagree and the
 * disagreement is itself informative: XGetImage on the WINDOW (which may mask
 * to the drawable's depth) and XGetImage on the ROOT at the same absolute
 * coordinates (the raw framebuffer word). Unredirected, a depth-32 window's
 * pixels live in the screen pixmap, so the root read is the one that shows
 * what byte actually sits in the alpha slot.
 *
 * Deliberately runs NO window manager and every window is override-redirect,
 * so nothing reparents them — a reparenting WM would silently move chain A
 * into chain B and destroy the whole experiment. Works unchanged on yserver
 * and on Xorg (`--server xorg`), which is the entire point.
 *
 * WITH `--redirect`, the probe additionally turns on Composite automatic
 * redirection for the root's children and reads the REDIRECTED PIXMAP — which
 * is literally the buffer a compositor samples, so it answers the question the
 * screen capture cannot answer on Xorg (under a compositor `import -window
 * root` returns the root window, not the composited output; that is issue
 * #135's whole symptom). Being pure protocol, it reads the same way on both
 * servers with no capture path in the comparison at all.
 *
 * Note which windows that redirection reaches. XCompositeRedirectSubwindows on
 * the root redirects the root's CHILDREN — chain A's windows and chain B's
 * depth-24 frames — but NOT chain B's depth-32 children, which are one level
 * further down and keep painting into their frame's pixmap. That is not a
 * limitation, it is the real-world structure: a WM frame is what gets
 * redirected and the ARGB client window paints inside it. So for chain B the
 * probe reads the FRAME's pixmap at the child's offset, which asks the
 * load-bearing question directly — does an alpha-zero child punch a
 * transparent hole through its opaque parent's redirected pixmap?
 *
 * THE BASELINE IS MEASURED, NOT PREDICTED. X.Org 1.21.1.24 (Arch's
 * xorg-server 21.1.24), vng harness, 2026-09-11, with `--redirect`:
 *
 *     bg_pixel            chain    win_read   pixmap_read
 *     transparent-white   A-root   00ffffff   00ffffff
 *     transparent-white   B-fr24   00ffffff   00ffffff
 *     transparent-zero    A-root   00000000   00000000
 *     transparent-zero    B-fr24   00000000   00000000
 *     opaque-white        A-root   ffffffff   ffffffff
 *     opaque-white        B-fr24   ffffffff   ffffffff
 *
 * Re-measured 2026-09-11 after `peek` switched from XGetPixel to raw image
 * bytes. XGetPixel masks to the reply depth CLIENT-side, which had been
 * hiding the pad byte on both servers and made B-fr24 opaque-white look like
 * a content divergence when the stored words were in fact identical.
 *
 * That table is the compatibility contract: it is what the server our users
 * actually run does, so it is what we match. It is printed as the `xorg_21.1`
 * column and graded against automatically.
 *
 * Two readings of that baseline are load-bearing elsewhere:
 *
 *   - The alpha fixup never fires anywhere in it. 21.1.x does not carry commit
 *     2de50de56 (2023-07-20), where that rule lives; it is master-only. It is
 *     deliberately NOT part of what this probe grades, and background painting
 *     must not be made to follow it.
 *   - chain B's pixmap_read is the ARGB child's own bits, alpha and all,
 *     sitting in its depth-24 parent's redirected pixmap. That is a
 *     RAW COPY of the child into the parent, not an alpha composite of it —
 *     which is what a redirected backing must be reconstructed with.
 *
 * Build:
 *     cc -O1 -o depth32-bg-probe depth32-bg-probe.c -lX11 -lXcomposite
 *
 * Run (on a real desktop — Plasma, MATE, whatever is already logged in):
 *     ./depth32-bg-probe
 *
 * It prints a table, holds its windows on screen for --hold seconds so they
 * can be photographed or screenshotted, then tidies up and exits. It never
 * takes a grab, never touches an existing window and never installs itself as
 * a compositor, so it is safe to run inside a live session.
 *
 *     --hold N      seconds to stay on screen (default 10; 0 = until killed)
 *     --redirect    force Composite automatic redirection on ourselves
 *     --no-redirect never redirect, even if nothing else is compositing
 *
 * By default the probe REDIRECTS ONLY IF NOTHING ELSE IS. On a composited
 * desktop the session's own compositor has already redirected everything, and
 * a second redirecting client would be both rude and a confound; the probe
 * detects that via the _NET_WM_CM_Sn manager selection and simply reads the
 * pixmaps that are already there. On a bare server with no compositor it turns
 * automatic redirection on itself, because otherwise there is no redirected
 * pixmap to read and the interesting column would be empty.
 */

#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <X11/extensions/Xcomposite.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define ROOT_BG   0x00ff0000u /* red    — root, visible only on a bare server */
#define BACKDROP  0x0000ff00u /* green  — backdrop SIBLING beneath chain A */
#define FRAME_BG  0x000000ffu /* blue   — depth-24 frame, parent of chain B */

#define WIN_W 120
#define WIN_H 120
#define STEP  160
#define ORIGIN_X 100
#define ROW_A_Y  100
#define ROW_B_Y  300
#define INSET 10 /* chain B child is inset inside its frame, so the frame's
                  * blue is visible around it even when the child is opaque */

static const unsigned long BG[3] = { 0x00ffffffu, 0x00000000u, 0xffffffffu };
static const char *BG_NAME[3] = { "transparent-white", "transparent-zero", "opaque-white" };

/* Measured on X.Org 1.21.1.24 with --redirect; see the header. Indexed
 * [bg][chain], chain 0 = A-root, chain 1 = B-fr24. */
static const unsigned long XORG_WIN[3][2] = {
    { 0x00ffffffu, 0x00ffffffu },
    { 0x00000000u, 0x00000000u },
    { 0xffffffffu, 0xffffffffu },
};
static const unsigned long XORG_PIXMAP[3][2] = {
    { 0x00ffffffu, 0x00ffffffu },
    { 0x00000000u, 0x00000000u },
    { 0xffffffffu, 0xffffffffu },
};

static int failures;

/* Grade one reading against the measured Xorg baseline. Only meaningful when
 * the probe ran redirected, since that is the state the baseline was taken
 * in; unredirected runs print the reading and grade nothing. */
static const char *grade(unsigned long got, unsigned long want, int ok, int comparable)
{
    if (!ok)
        return "-";
    if (!comparable)
        return "?";
    if (got == want)
        return "ok";
    failures++;
    return "DIFF";
}

/* Read one pixel out of `d` at drawable-relative (x,y). Returns 0 on failure
 * and sets *ok, so a BadMatch on one read cannot take the whole probe down. */
static unsigned long peek(Display *dpy, Drawable d, int x, int y, int *ok)
{
    XImage *img = XGetImage(dpy, d, x, y, 1, 1, AllPlanes, ZPixmap);
    if (!img) {
        *ok = 0;
        return 0;
    }
    /* Read the RAW bytes, not XGetPixel. XGetPixel masks the value down to
     * the image's depth, so on a depth-24 reply it silently zeroes the pad
     * byte — client-side, on every server equally. That hides exactly the
     * thing this probe exists to compare: what the server stored, and what
     * depth it claimed the reply was. Byte order here is the server's
     * image_byte_order; both ends of this comparison are little-endian
     * LSBFirst, so B,G,R,A packs to 0xAARRGGBB. */
    unsigned long px;
    if (img->bits_per_pixel == 32 && img->byte_order == LSBFirst) {
        unsigned char *b = (unsigned char *) img->data;
        px = ((unsigned long) b[3] << 24) | ((unsigned long) b[2] << 16)
             | ((unsigned long) b[1] << 8) | (unsigned long) b[0];
    } else {
        px = XGetPixel(img, 0, 0);
    }
    XDestroyImage(img);
    *ok = 1;
    return px;
}

/* Read one pixel out of the window's REDIRECTED pixmap — the buffer a
 * compositor would sample. `win` of 0 means redirection is off, which is a
 * skip, not a failure. The pixmap is named fresh each time because its id is
 * invalidated by every resize and by unredirection. */
static unsigned long peek_named_pixmap(Display *dpy, Window win, int x, int y, int *ok)
{
    if (!win) {
        *ok = 0;
        return 0;
    }
    Pixmap pm = XCompositeNameWindowPixmap(dpy, win);
    if (!pm) {
        *ok = 0;
        return 0;
    }
    unsigned long px = peek(dpy, pm, x, y, ok);
    XFreePixmap(dpy, pm);
    return px;
}

int main(int argc, char **argv)
{
    int force_redirect = 0, never_redirect = 0, hold = 10;
    for (int i = 1; i < argc; i++) {
        if (strcmp(argv[i], "--redirect") == 0) {
            force_redirect = 1;
        } else if (strcmp(argv[i], "--no-redirect") == 0) {
            never_redirect = 1;
        } else if (strcmp(argv[i], "--hold") == 0 && i + 1 < argc) {
            hold = atoi(argv[++i]);
        } else {
            fprintf(stderr,
                    "usage: %s [--hold N] [--redirect|--no-redirect]\n", argv[0]);
            return 2;
        }
    }

    Display *dpy = XOpenDisplay(NULL);
    if (!dpy) {
        fprintf(stderr, "depth32-bg-client: cannot open display\n");
        return 1;
    }
    int screen = DefaultScreen(dpy);
    Window root = RootWindow(dpy, screen);

    XVisualInfo vinfo;
    if (!XMatchVisualInfo(dpy, screen, 32, TrueColor, &vinfo)) {
        fprintf(stderr, "depth32-bg-client: no depth-32 TrueColor visual\n");
        return 1;
    }
    Colormap cmap32 = XCreateColormap(dpy, root, vinfo.visual, AllocNone);

    /* Is a compositing manager already running? The ICCCM/EWMH answer is the
     * owner of the _NET_WM_CM_Sn manager selection, which every compositor
     * (KWin, Mutter, picom, xfwm4 ...) claims. */
    char sel[32];
    snprintf(sel, sizeof sel, "_NET_WM_CM_S%d", screen);
    int session_composited =
        XGetSelectionOwner(dpy, XInternAtom(dpy, sel, False)) != None;

    int ev, err;
    int have_composite_ext = XCompositeQueryExtension(dpy, &ev, &err);
    int we_redirect = 0;
    if (!never_redirect && have_composite_ext
        && (force_redirect || !session_composited)) {
        /* Automatic, not manual: the server keeps painting to the screen, so
         * what is on screen stays meaningful and no external compositor has to
         * be trusted to draw. Redirect BEFORE creating the windows so they are
         * born redirected and no unredirected fill can be mistaken for the
         * redirected one. */
        XCompositeRedirectSubwindows(dpy, root, CompositeRedirectAutomatic);
        XSync(dpy, False);
        we_redirect = 1;
    }
    /* Either route leaves the windows redirected, which is all the pixmap
     * read needs. */
    int have_composite = have_composite_ext && (we_redirect || session_composited);

    /* Red root, so a preserved-alpha window over it blends to red. */
    XSetWindowBackground(dpy, root, ROOT_BG);
    XClearWindow(dpy, root);

    Window a[3], b[3], frame[3], back[3];
    XSetWindowAttributes attr;

    for (int i = 0; i < 3; i++) {
        /* A depth-24 backdrop for chain A. It is a SIBLING stacked beneath,
         * never a parent: making it a parent would drag chain A into chain B's
         * shape and there would be only one case left. The root's own red is
         * no use on a real desktop, where the session paints a desktop window
         * over the root and the root is never seen. */
        XSetWindowAttributes battr;
        battr.background_pixel = BACKDROP;
        battr.override_redirect = True;
        battr.event_mask = ExposureMask;
        back[i] = XCreateWindow(dpy, root, ORIGIN_X + i * STEP - INSET, ROW_A_Y - INSET,
                                WIN_W + 2 * INSET, WIN_H + 2 * INSET, 0,
                                (int) DefaultDepth(dpy, screen), InputOutput,
                                DefaultVisual(dpy, screen),
                                CWBackPixel | CWOverrideRedirect | CWEventMask, &battr);

        /* ---- chain A: depth-32 child of the root ---- */
        attr.background_pixel = BG[i];
        attr.border_pixel = 0; /* required: depth differs from the parent's */
        attr.colormap = cmap32;
        attr.override_redirect = True;
        attr.event_mask = ExposureMask;
        a[i] = XCreateWindow(dpy, root, ORIGIN_X + i * STEP, ROW_A_Y, WIN_W, WIN_H, 0,
                             32, InputOutput, vinfo.visual,
                             CWBackPixel | CWBorderPixel | CWColormap
                                 | CWOverrideRedirect | CWEventMask,
                             &attr);

        /* ---- chain B: depth-32 child of a depth-24 frame ---- */
        XSetWindowAttributes fattr;
        fattr.background_pixel = FRAME_BG;
        fattr.override_redirect = True;
        fattr.event_mask = ExposureMask;
        frame[i] = XCreateWindow(dpy, root, ORIGIN_X + i * STEP, ROW_B_Y, WIN_W, WIN_H, 0,
                                 (int) DefaultDepth(dpy, screen), InputOutput,
                                 DefaultVisual(dpy, screen),
                                 CWBackPixel | CWOverrideRedirect | CWEventMask, &fattr);

        attr.background_pixel = BG[i];
        b[i] = XCreateWindow(dpy, frame[i], INSET, INSET,
                             WIN_W - 2 * INSET, WIN_H - 2 * INSET, 0,
                             32, InputOutput, vinfo.visual,
                             CWBackPixel | CWBorderPixel | CWColormap
                                 | CWOverrideRedirect | CWEventMask,
                             &attr);
    }

    /* Backdrops first, so they are below their chain-A window in the stack. */
    for (int i = 0; i < 3; i++)
        XMapWindow(dpy, back[i]);
    XSync(dpy, False);
    for (int i = 0; i < 3; i++) {
        XMapWindow(dpy, a[i]);
        XMapWindow(dpy, frame[i]);
        XMapWindow(dpy, b[i]);
    }
    XSync(dpy, False);

    /* Let the server settle its background paint before reading it back. */
    sleep(1);

    printf("# depth32-bg-probe: root_bg=%08lx backdrop=%08lx frame_bg=%08lx\n",
           (unsigned long) ROOT_BG, (unsigned long) BACKDROP,
           (unsigned long) FRAME_BG);
    printf("# on screen: chain A blended => GREEN shows; chain B blended => BLUE\n");
    printf("# server=\"%s\" composite_ext=%s session_compositor=%s redirected_by=%s\n",
           ServerVendor(dpy), have_composite_ext ? "yes" : "no",
           session_composited ? "yes" : "no",
           we_redirect ? "probe" : (session_composited ? "session" : "nothing"));
    printf("# %-18s %-8s %-10s %-9s %-10s %-11s %-9s %s\n",
           "bg_pixel", "chain", "win_read", "vs_xorg", "root_read", "pixmap_read",
           "vs_xorg", "geometry");

    for (int i = 0; i < 3; i++) {
        int ax = ORIGIN_X + i * STEP, ay = ROW_A_Y;
        int bx = ORIGIN_X + i * STEP + INSET, by = ROW_B_Y + INSET;
        int ok_w, ok_r, ok_p;
        unsigned long wa = peek(dpy, a[i], 2, 2, &ok_w);
        unsigned long ra = peek(dpy, root, ax + 2, ay + 2, &ok_r);
        /* chain A is a direct child of the root, so redirection reached it and
         * its own pixmap is what a compositor samples. */
        unsigned long pa = peek_named_pixmap(dpy, have_composite ? a[i] : 0, 2, 2, &ok_p);
        printf("%-20s %-8s %08lx%s  %-9s %08lx%s  %08lx%s   %-9s %d,%d %dx%d\n",
               BG_NAME[i], "A-root",
               wa, ok_w ? " " : "!", grade(wa, XORG_WIN[i][0], ok_w, have_composite),
               ra, ok_r ? " " : "!",
               pa, ok_p ? " " : "!", grade(pa, XORG_PIXMAP[i][0], ok_p, have_composite),
               ax, ay, WIN_W, WIN_H);

        unsigned long wb = peek(dpy, b[i], 2, 2, &ok_w);
        unsigned long rb = peek(dpy, root, bx + 2, by + 2, &ok_r);
        /* chain B's depth-32 child is NOT redirected — its depth-24 FRAME is.
         * Read the frame's pixmap where the child sits: that is the question. */
        unsigned long pb = peek_named_pixmap(dpy, have_composite ? frame[i] : 0,
                                             INSET + 2, INSET + 2, &ok_p);
        printf("%-20s %-8s %08lx%s  %-9s %08lx%s  %08lx%s   %-9s %d,%d %dx%d\n",
               BG_NAME[i], "B-fr24",
               wb, ok_w ? " " : "!", grade(wb, XORG_WIN[i][1], ok_w, have_composite),
               rb, ok_r ? " " : "!",
               pb, ok_p ? " " : "!", grade(pb, XORG_PIXMAP[i][1], ok_p, have_composite),
               bx, by, WIN_W - 2 * INSET, WIN_H - 2 * INSET);
    }
    /* A depth-24 PIXMAP's pad byte, read raw. The spec leaves the 8 pad bits
     * of a 32-bits-per-pixel depth-24 ZPixmap undefined, so this is not a
     * conformance question but a compatibility one: whatever Xorg puts there
     * is what clients have been written against. Read the image bytes
     * directly rather than through XGetPixel, which normalises. */
    {
        Pixmap p24 = XCreatePixmap(dpy, root, 4, 4, (unsigned) DefaultDepth(dpy, screen));
        GC pgc = XCreateGC(dpy, p24, 0, NULL);
        XSetForeground(dpy, pgc, 0x00ff0000u); /* opaque-ish red, X byte clear */
        XFillRectangle(dpy, p24, pgc, 0, 0, 4, 4);
        XSetForeground(dpy, pgc, 0xffff0000u); /* same red, X byte SET */
        XFillRectangle(dpy, p24, pgc, 2, 0, 2, 4);
        XSync(dpy, False);
        XImage *pi = XGetImage(dpy, p24, 0, 0, 4, 1, AllPlanes, ZPixmap);
        if (pi && pi->bits_per_pixel == 32) {
            unsigned char *d = (unsigned char *) pi->data;
            /* MEASURED on Xorg 21.1.24: 00 then ff — the pad byte is passed
             * through verbatim, NOT canonicalised. `fbGetImage` masks only
             * `if (pm != FB_ALLONES)` (fb/fbimage.c) and AllPlanes on a
             * 32-bpp depth-24 drawable replicates to all-ones, so the mask
             * step is skipped entirely. The 8 pad bits of a depth-24 ZPixmap
             * are undefined by the protocol and Xorg does not define them. */
            /* INFORMATIONAL — deliberately not part of the verdict. These 8
             * bits are undefined by the protocol and Xorg does not define
             * them either, so a difference here is a difference in a field
             * no conforming client may read. Printed because it is cheap and
             * it localises the "which byte moved" question, not because it
             * is a defect. Promote it only if a real client is shown to
             * depend on the byte. */
            printf("# depth-24 pixmap pad byte (informational, protocol-undefined): "
                   "fg=0x00ff0000 -> %02x, fg=0xffff0000 -> %02x"
                   "  (xorg 21.1.24: 00 then ff = pass-through)\n",
                   d[3], d[4 * 2 + 3]);
        } else {
            printf("# depth-24 pixmap pad byte: unavailable (bpp=%d)\n",
                   pi ? pi->bits_per_pixel : -1);
        }
        if (pi)
            XDestroyImage(pi);
        XFreeGC(dpy, pgc);
        XFreePixmap(dpy, p24);
    }

    /* The REPLY depth, not just the pixel. GetImage's reply carries a depth
     * and the client believes it; if it comes from the server's internal
     * storage rather than the drawable's X11 depth, every pixel above is
     * being interpreted against the wrong contract. Xorg 21.1.24 answers 24
     * for the depth-24 root and 32 for a depth-32 window. */
    {
        XImage *ri = XGetImage(dpy, root, ORIGIN_X, ROW_A_Y, 1, 1, AllPlanes, ZPixmap);
        XImage *wi = XGetImage(dpy, a[0], 2, 2, 1, 1, AllPlanes, ZPixmap);
        printf("# reply depth: root=%d (xorg 21.1.24: 24)  depth32 window=%d (xorg: 32)%s\n",
               ri ? ri->depth : -1, wi ? wi->depth : -1,
               (ri && ri->depth != 24) ? "   <-- DIFF" : "");
        if (ri && ri->depth != 24)
            failures++;
        if (ri)
            XDestroyImage(ri);
        if (wi)
            XDestroyImage(wi);
    }
    printf("# a trailing '!' marks a failed XGetImage, not a zero pixel\n");
    printf("# win_read    = XGetImage on the window itself, read as RAW BYTES (not\n"
           "#               XGetPixel, which masks to the reply depth)\n");
    printf("# root_read   = XGetImage on the ROOT at the same absolute point. The\n");
    printf("#               root is depth 24, so its alpha byte carries no\n");
    printf("#               information; the column is here to expose plane-mask\n");
    printf("#               differences, not to be read as stored alpha. Xorg\n");
    printf("#               does NOT mask it: AllPlanes on a 32-bpp depth-24\n");
    printf("#               drawable replicates to all-ones and fbGetImage\n");
    printf("#               skips masking, so the byte is whatever storage\n");
    printf("#               holds. Compare it as content, not as a mask test.\n");
    printf("# pixmap_read = XGetImage on the redirected pixmap a compositor\n");
    printf("#               samples. For chain B that is the depth-24 FRAME's\n");
    printf("#               pixmap at the ARGB child's offset, so alpha 00 there\n");
    printf("#               means the child punched a transparent hole through\n");
    printf("#               its opaque parent.\n");
    printf("# vs_xorg     = against the MEASURED X.Org 1.21.1.24 redirected table\n");
    printf("#               in this file's header, which is the compatibility\n");
    printf("#               contract. '?' = not comparable (ran unredirected),\n");
    printf("#               '-' = the read itself failed.\n");
    printf("# NOT graded  = the master-only alpha fixup (commit 2de50de56); it is\n");
    printf("#               absent from every 21.1 release and out of scope here.\n");
    printf("%s\n", have_composite
           ? (failures ? "VERDICT: DIFFERS from Xorg 21.1" : "VERDICT: matches Xorg 21.1")
           : "VERDICT: ungraded (ran unredirected; baseline is the redirected table)");
    fflush(stdout);

    /* The background is server-painted; the probe only has to stay alive so
     * the connection is not dropped while the screen is being looked at. */
    for (int elapsed = 0; hold <= 0 || elapsed < hold * 10; elapsed++) {
        while (XPending(dpy)) {
            XEvent xev;
            XNextEvent(dpy, &xev);
        }
        usleep(100000);
    }

    /* Tidy up: this may be running inside someone's live session, and leaving
     * six override-redirect windows welded over their desktop is not on. */
    for (int i = 0; i < 3; i++) {
        XDestroyWindow(dpy, a[i]);
        XDestroyWindow(dpy, back[i]);
        XDestroyWindow(dpy, frame[i]); /* takes b[i] with it */
    }
    if (we_redirect)
        XCompositeUnredirectSubwindows(dpy, root, CompositeRedirectAutomatic);
    /* Restore the root background we overwrote. Nothing records what it was,
     * so hand it back to the session rather than guessing a colour: None means
     * "undefined", and the desktop repaints its own wallpaper on the Expose. */
    XSetWindowBackgroundPixmap(dpy, root, None);
    XClearWindow(dpy, root);
    XSync(dpy, False);
    XCloseDisplay(dpy);
    return 0;
}
