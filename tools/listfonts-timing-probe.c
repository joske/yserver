/* listfonts-timing-probe — is ListFonts still exponential in the star count?
 *
 * Issue #155: a single ListFonts froze the server for up to 514ms on the
 * reporter's Ivy Bridge box, because font_pattern_matches (kms/core.rs) was a
 * catastrophic-backtracking glob run once per font-path entry. This probe is
 * the end-to-end check that the fix holds in a live session, since neither
 * the unit tests nor an ordinary desktop session exercise it:
 *
 *   * GIMP, evilwm and xterm issue ZERO ListFonts between them — measured
 *     over a full session. GTK goes through Xft/fontconfig client-side.
 *   * OpenFont for `fixed`, `cursor` or `nil2` short-circuits on
 *     BUILTIN_ALIASES before the matcher is reached.
 *
 * So a session that "feels fine" says nothing at all here, and neither does
 * `xlsfonts` with its default `*`. The cost is in the number of STARS and
 * only shows with a trailing LITERAL to backtrack against, which is what
 * XCreateFontSet, Pango and GTK actually send.
 *
 * A NON-MATCHING pattern is the worst case and the common one: every split of
 * every star must be tried before the answer is known, and most fonts are not
 * in the charset asked for. Hence the iso10646-1/iso8859-1 pair below — on a
 * typical bitmap font dir one of them misses most entries.
 *
 * ORACLE NOTE. This measures the SERVER's matcher, so it needs a font path
 * with real entries to walk. On Arch that means /usr/share/fonts/misc must
 * have a fonts.dir — xorg-fonts-misc does not ship one, the xorg-mkfontscale
 * hook generates it, and without it the directory is not a valid FPE, the
 * path collapses to `built-ins`, and this probe passes VACUOUSLY. The
 * path_entries line below is printed so that cannot be mistaken for a fix:
 * treat any run with a small count as no evidence either way.
 *
 * Build: gcc -O1 -o /tmp/listfonts-timing-probe tools/listfonts-timing-probe.c -lX11
 * Run:   DISPLAY=:7 /tmp/listfonts-timing-probe        (under yserver)
 *        DISPLAY=:0 /tmp/listfonts-timing-probe        (reference)
 *
 * Exit: 0 = every pattern under the budget, 2 = a pattern over it
 *       (the signature of the exponential matcher being back), 1 = no run.
 */

#include <X11/Xlib.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>

/* Generous on purpose. Post-fix this is ~0.1ms of matcher work per call even
 * against a 12k-entry catalog; the exponential form cost 50ms on a 13900k
 * over only 480 names, and 514ms on the reporter's box. Anything between is
 * not a judgement call. */
#define BUDGET_MS 150.0

#define REPEATS 5

static double ms_since(struct timespec t0) {
    struct timespec t1;
    clock_gettime(CLOCK_MONOTONIC, &t1);
    return (double)(t1.tv_sec - t0.tv_sec) * 1e3
         + (double)(t1.tv_nsec - t0.tv_nsec) / 1e6;
}

int main(int argc, char **argv) {
    Display *d = XOpenDisplay(argc > 1 ? argv[1] : NULL);
    if (!d) {
        printf("connect=fail\n");
        return 1;
    }
    printf("connect=ok\n");

    /* How much there is to walk. A tiny number means the font path has no
     * real entries and every timing below is meaningless. */
    int npath = 0;
    char **all = XListFonts(d, "*", 100000, &npath);
    if (all) XFreeFontNames(all);
    printf("path_entries=%d\n", npath);
    printf("vacuous=%s\n", npath < 50 ? "YES-font-path-is-empty" : "no");

    static const char *const PATTERNS[] = {
        /* The shape toolkits send, with a literal tail to backtrack against.
         * Two charsets so at least one misses most entries on any given box. */
        "-*-*-*-*-*-*-*-*-*-*-*-*-iso8859-1",
        "-*-*-*-*-*-*-*-*-*-*-*-*-iso10646-1",
        /* Controls: all-wildcard and bare star are CHEAP even when the
         * matcher is exponential, so a probe that only tried these would
         * report a broken server as healthy. */
        "-*-*-*-*-*-*-*-*-*-*-*-*-*-*",
        "*",
    };
    const int NPAT = (int)(sizeof PATTERNS / sizeof PATTERNS[0]);

    int over_budget = 0;
    for (int p = 0; p < NPAT; p++) {
        double best = -1.0, worst = -1.0;
        int count = -1;
        for (int r = 0; r < REPEATS; r++) {
            struct timespec t0;
            clock_gettime(CLOCK_MONOTONIC, &t0);
            int n = 0;
            char **list = XListFonts(d, PATTERNS[p], 100000, &n);
            /* XListFonts is a round trip, so this includes the reply; that is
             * the number a client actually waits for, which is the point. */
            double el = ms_since(t0);
            if (list) XFreeFontNames(list);
            if (count < 0) count = n;
            if (best < 0 || el < best) best = el;
            if (worst < 0 || el > worst) worst = el;
        }
        printf("pattern[%d]=%s\n", p, PATTERNS[p]);
        printf("pattern[%d].names=%d\n", p, count);
        printf("pattern[%d].best_ms=%.3f\n", p, best);
        printf("pattern[%d].worst_ms=%.3f\n", p, worst);
        /* Judge on the BEST of several runs: a one-off scheduling hiccup must
         * not read as the exponential matcher, and the exponential matcher is
         * slow on every single call, so the best is still far over budget. */
        if (best > BUDGET_MS) over_budget = 1;
    }

    printf("budget_ms=%.1f\n", BUDGET_MS);
    printf("verdict=%s\n", over_budget ? "OVER-BUDGET-matcher-may-be-exponential"
                                       : "within-budget");
    XCloseDisplay(d);
    return over_budget ? 2 : 0;
}
