/* redirect-exclusive-probe — the "at most one client" event-mask bits
 *                            (SubstructureRedirect, ResizeRedirect, ButtonPress).
 *
 *   ./redirect-exclusive-probe
 *
 * Clients `a`, `b` (and short-lived `c`) select those bits on root and on
 * windows they create; every request result is printed as `ok` or
 * `error=<code> value=0x<v> major=<m>`, followed by the GetWindowAttributes
 * state the cases check. Run against Xephyr for the oracle, then yserver.
 *
 *   cc -O1 -o redirect-exclusive-probe redirect-exclusive-probe.c -lxcb
 */
#include <stdio.h>
#include <stdlib.h>
#include <xcb/xcb.h>

#define SR XCB_EVENT_MASK_SUBSTRUCTURE_REDIRECT
#define SN XCB_EVENT_MASK_SUBSTRUCTURE_NOTIFY
#define RR XCB_EVENT_MASK_RESIZE_REDIRECT
#define BP XCB_EVENT_MASK_BUTTON_PRESS
#define BR XCB_EVENT_MASK_BUTTON_RELEASE

static xcb_window_t root;

static xcb_connection_t *conn(void)
{
    xcb_connection_t *c = xcb_connect(NULL, NULL);
    if (xcb_connection_has_error(c)) {
        fprintf(stderr, "cannot connect\n");
        exit(1);
    }
    return c;
}

static void report(const char *what, xcb_connection_t *c, xcb_void_cookie_t ck)
{
    xcb_generic_error_t *e = xcb_request_check(c, ck);
    if (!e) {
        printf("%-58s ok\n", what);
        return;
    }
    printf("%-58s error=%u value=0x%x major=%u\n", what, e->error_code, e->resource_id,
           e->major_code);
    free(e);
}

static void select_mask(const char *what, xcb_connection_t *c, xcb_window_t w, uint32_t m)
{
    report(what, c, xcb_change_window_attributes_checked(c, w, XCB_CW_EVENT_MASK, &m));
}

static xcb_window_t make_window(xcb_connection_t *c, uint32_t mask, const char *what)
{
    xcb_window_t w = xcb_generate_id(c);
    uint32_t v[] = {mask};
    report(what, c,
           xcb_create_window_checked(c, XCB_COPY_FROM_PARENT, w, root, 10, 10, 50, 50, 0,
                                     XCB_WINDOW_CLASS_INPUT_OUTPUT, XCB_COPY_FROM_PARENT,
                                     mask ? XCB_CW_EVENT_MASK : 0, v));
    return w;
}

static void attrs(const char *what, xcb_connection_t *c, xcb_window_t w)
{
    xcb_get_window_attributes_reply_t *r =
        xcb_get_window_attributes_reply(c, xcb_get_window_attributes(c, w), NULL);
    if (!r) {
        printf("%-58s no reply\n", what);
        return;
    }
    printf("%-58s bit_gravity=%u win_gravity=%u override_redirect=%u your_mask=0x%x "
           "all_masks=0x%x do_not_propagate=0x%x\n",
           what, r->bit_gravity, r->win_gravity, r->override_redirect, r->your_event_mask,
           r->all_event_masks, r->do_not_propagate_mask);
    free(r);
}

int main(void)
{
    xcb_connection_t *a = conn(), *b = conn();
    root = xcb_setup_roots_iterator(xcb_get_setup(a)).data->root;

    /* Root: the creator is serverClient, so this is the otherClients path. */
    select_mask("1 a SR on root", a, root, SR);
    select_mask("2 b SR on root", b, root, SR);
    select_mask("3 a SR|SN on root (re-select)", a, root, SR | SN);
    select_mask("4 b SN on root", b, root, SN);
    select_mask("5 b SR|SN on root", b, root, SR | SN);
    attrs("5' b's view of root", b, root);
    select_mask("6 a SN on root (drops SR)", a, root, SN);
    select_mask("7 b SR on root after a dropped it", b, root, SR);
    select_mask("8 a SR on root (b holds it now)", a, root, SR);

    /* A disconnecting holder releases the bit. */
    select_mask("9 b 0 on root", b, root, 0);
    xcb_connection_t *c = conn();
    select_mask("10 c SR on root", c, root, SR);
    select_mask("11 b SR on root while c holds it", b, root, SR);
    xcb_disconnect(c);
    free(xcb_get_input_focus_reply(a, xcb_get_input_focus(a), NULL)); /* sync */
    select_mask("12 b SR on root after c disconnected", b, root, SR);
    select_mask("13 b 0 on root", b, root, 0);

    /* Creator path: a creates W with ButtonPress selected. */
    xcb_window_t w = make_window(a, BP, "14 a CreateWindow W mask=ButtonPress");
    select_mask("15 b ButtonPress on W", b, w, BP);
    select_mask("16 b ButtonRelease on W", b, w, BR);
    select_mask("17 b SR on W (other bit)", b, w, SR);
    select_mask("18 a ButtonPress|ButtonRelease on W (re-select)", a, w, BP | BR);

    /* ResizeRedirect, held by the non-creator. */
    select_mask("19 b RR on W", b, w, RR);
    select_mask("20 a RR on W (creator, b holds RR)", a, w, RR);
    select_mask("21 a RR|BP on W", a, w, RR | BP);
    attrs("21' a's view of W", a, w);

    /* Value-list order: bits below CWEventMask apply, bits above do not. */
    {
        uint32_t v[] = {XCB_GRAVITY_STATIC, XCB_GRAVITY_SOUTH_EAST, 1, BP, XCB_EVENT_MASK_KEY_PRESS};
        report("22 b CWA W bit/win gravity+OR+mask=BP+dont_propagate", b,
               xcb_change_window_attributes_checked(
                   b, w,
                   XCB_CW_BIT_GRAVITY | XCB_CW_WIN_GRAVITY | XCB_CW_OVERRIDE_REDIRECT |
                       XCB_CW_EVENT_MASK | XCB_CW_DONT_PROPAGATE,
                   v));
        attrs("22' b's view of W", b, w);
    }

    /* Error value after a request that set one (BadValue on mask). */
    {
        uint32_t bad = 0xfe000000u;
        report("23 b CWA W bad event mask", b,
               xcb_change_window_attributes_checked(b, w, XCB_CW_EVENT_MASK, &bad));
        select_mask("24 b ButtonPress on W again", b, w, BP);
    }

    /* A new window created by b with SR: a new window cannot conflict. */
    xcb_window_t w2 = make_window(b, SR | BP, "25 b CreateWindow W2 mask=SR|BP");
    select_mask("26 a SR on W2", a, w2, SR);

    xcb_disconnect(a);
    xcb_disconnect(b);
    return 0;
}
