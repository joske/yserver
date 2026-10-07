/* SendEvent delivery (#212): a synthetic core event reaches the clients
 * whose CORE event mask matches, whether or not they also selected the XI2
 * form of the event; then propagation, do-not-propagate, the empty mask,
 * the PointerWindow and InputFocus destinations, the request's errors and,
 * for contrast, a real XTEST click.
 *
 *   ./probe
 *
 * Client R owns W (200x200 at 100,100, core Button/Motion/Key masks) and
 * its child C (100x100 at 50,50, no mask); client O selects PointerMotion
 * on W; client S sends. Each step logs what R and O received, in order.
 *
 *   cc -O1 -o probe sendevent-xi2-probe.c -lxcb -lxcb-xinput -lxcb-xtest
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <xcb/xcb.h>
#include <xcb/xinput.h>
#include <xcb/xtest.h>

static xcb_connection_t *r, *o, *s;
static xcb_window_t root, w, c;

static xcb_connection_t *connect_retry(void)
{
    xcb_connection_t *x;
    /* Xorg resets when the previous run's last client leaves. */
    for (int tries = 0; (x = xcb_connect(NULL, NULL)) && xcb_connection_has_error(x) && tries < 50;
         tries++) {
        xcb_disconnect(x);
        usleep(100000);
    }
    return x;
}

static void sync_server(xcb_connection_t *x)
{
    free(xcb_get_input_focus_reply(x, xcb_get_input_focus(x), NULL));
}

static const char *wname(xcb_window_t x)
{
    static char buf[16];
    if (x == w)
        return "W";
    if (x == c)
        return "C";
    if (x == root)
        return "root";
    snprintf(buf, sizeof buf, "0x%x", x);
    return buf;
}

static void drain(xcb_connection_t *x, const char *who)
{
    sync_server(x);
    printf("  %s:", who);
    int any = 0;
    xcb_generic_event_t *e;
    while ((e = xcb_poll_for_event(x))) {
        any = 1;
        uint8_t type = e->response_type & 0x7f;
        const char *sent = (e->response_type & 0x80) ? "sent " : "";
        switch (type) {
        case 0:
            printf(" error %u", ((xcb_generic_error_t *)e)->error_code);
            break;
        case XCB_KEY_PRESS:
        case XCB_BUTTON_PRESS:
        case XCB_BUTTON_RELEASE:
        case XCB_MOTION_NOTIFY: {
            static const char *n[] = {[2] = "KeyPress", [4] = "ButtonPress",
                                      [5] = "ButtonRelease", [6] = "MotionNotify"};
            xcb_button_press_event_t *b = (xcb_button_press_event_t *)e;
            printf(" %s%s(event %s)", sent, n[type], wname(b->event));
            break;
        }
        case XCB_GE_GENERIC: {
            xcb_input_button_press_event_t *g = (xcb_input_button_press_event_t *)e;
            printf(" %sXI2(evtype %u event %s)", sent, g->event_type, wname(g->event));
            break;
        }
        default:
            printf(" %sevent %u", sent, type);
        }
        free(e);
    }
    printf("%s\n", any ? "" : " none");
}

static void step(const char *what)
{
    sync_server(s);
    usleep(100000);
    printf("%s\n", what);
    drain(r, "R");
    drain(o, "O");
    fflush(stdout);
}

/* S sends a core event of `type` naming `event` in the template. */
static void send(uint8_t type, xcb_window_t dest, uint32_t mask, uint8_t propagate,
                 xcb_window_t event)
{
    xcb_button_press_event_t ev;
    memset(&ev, 0, sizeof ev);
    ev.response_type = type;
    ev.detail = type == XCB_KEY_PRESS ? 38 : type == XCB_MOTION_NOTIFY ? 0 : 1;
    ev.root = root;
    ev.event = event;
    ev.event_x = 10;
    ev.event_y = 10;
    ev.same_screen = 1;
    xcb_send_event(s, propagate, dest, mask, (const char *)&ev);
}

static void click(xcb_window_t dest, uint8_t propagate)
{
    send(XCB_MOTION_NOTIFY, dest, XCB_EVENT_MASK_POINTER_MOTION, propagate, dest);
    send(XCB_BUTTON_PRESS, dest, XCB_EVENT_MASK_BUTTON_PRESS | XCB_EVENT_MASK_BUTTON_RELEASE,
         propagate, dest);
    send(XCB_BUTTON_RELEASE, dest, XCB_EVENT_MASK_BUTTON_PRESS | XCB_EVENT_MASK_BUTTON_RELEASE,
         propagate, dest);
}

#define XI2_BUTTONS_MOTION                                                                         \
    (XCB_INPUT_XI_EVENT_MASK_BUTTON_PRESS | XCB_INPUT_XI_EVENT_MASK_BUTTON_RELEASE |               \
     XCB_INPUT_XI_EVENT_MASK_MOTION)

static void select_xi2(xcb_window_t win, uint16_t device, uint32_t m)
{
    struct {
        xcb_input_event_mask_t h;
        uint32_t m;
    } mask = {{device, 1}, m};
    xcb_input_xi_select_events(r, win, 1, &mask.h);
    sync_server(r);
}

static void dnp(uint32_t mask)
{
    xcb_change_window_attributes(r, c, XCB_CW_DONT_PROPAGATE, &mask);
    sync_server(r);
}

static void focus(xcb_window_t win)
{
    xcb_set_input_focus(r, XCB_INPUT_FOCUS_NONE, win, XCB_CURRENT_TIME);
    sync_server(r);
    drain(r, "(focus change) R");
}

static void error_of(const char *what, uint8_t type, uint8_t format, xcb_window_t dest,
                     uint32_t mask, uint8_t propagate)
{
    xcb_button_press_event_t ev;
    memset(&ev, 0, sizeof ev);
    ev.response_type = type;
    ev.detail = format;
    xcb_generic_error_t *err = xcb_request_check(
        s, xcb_send_event_checked(s, propagate, dest, mask, (const char *)&ev));
    if (err)
        printf("%s: error %u value 0x%x\n", what, err->error_code, err->resource_id);
    else
        printf("%s: no error\n", what);
    free(err);
}

int main(void)
{
    r = connect_retry();
    o = connect_retry();
    s = connect_retry();
    if (xcb_connection_has_error(r) || xcb_connection_has_error(o) || xcb_connection_has_error(s)) {
        printf("cannot connect\n");
        return 1;
    }
    xcb_screen_t *scr = xcb_setup_roots_iterator(xcb_get_setup(r)).data;
    root = scr->root;
    const xcb_query_extension_reply_t *q = xcb_get_extension_data(r, &xcb_input_id);
    if (!q || !q->present) {
        printf("no XInput\n");
        return 1;
    }
    free(xcb_input_xi_query_version_reply(r, xcb_input_xi_query_version(r, 2, 2), NULL));
    free(xcb_test_get_version_reply(s, xcb_test_get_version(s, 2, 2), NULL));
    xcb_warp_pointer(s, XCB_NONE, root, 0, 0, 0, 0, 20, 20);
    sync_server(s);

    w = xcb_generate_id(r);
    uint32_t wv[2] = {scr->white_pixel, XCB_EVENT_MASK_KEY_PRESS | XCB_EVENT_MASK_BUTTON_PRESS |
                                             XCB_EVENT_MASK_BUTTON_RELEASE |
                                             XCB_EVENT_MASK_POINTER_MOTION};
    xcb_create_window(r, XCB_COPY_FROM_PARENT, w, root, 100, 100, 200, 200, 0,
                      XCB_WINDOW_CLASS_INPUT_OUTPUT, scr->root_visual,
                      XCB_CW_BACK_PIXEL | XCB_CW_EVENT_MASK, wv);
    c = xcb_generate_id(r);
    uint32_t cv = scr->black_pixel;
    xcb_create_window(r, XCB_COPY_FROM_PARENT, c, w, 50, 50, 100, 100, 0,
                      XCB_WINDOW_CLASS_INPUT_OUTPUT, scr->root_visual, XCB_CW_BACK_PIXEL, &cv);
    xcb_map_window(r, c);
    xcb_map_window(r, w);
    sync_server(r);
    step("mapped");

    click(w, 0);
    step("core only: motion, press, release to W");
    select_xi2(w, XCB_INPUT_DEVICE_ALL_MASTER, XI2_BUTTONS_MOTION);
    click(w, 0);
    step("XI2 AllMasterDevices on W: motion, press, release to W");
    select_xi2(root, XCB_INPUT_DEVICE_ALL, XI2_BUTTONS_MOTION);
    click(w, 0);
    step("XI2 AllDevices on the root too: motion, press, release to W");
    select_xi2(root, XCB_INPUT_DEVICE_ALL, 0);

    uint32_t om = XCB_EVENT_MASK_POINTER_MOTION;
    xcb_change_window_attributes(o, w, XCB_CW_EVENT_MASK, &om);
    sync_server(o);
    send(XCB_MOTION_NOTIFY, w, XCB_EVENT_MASK_POINTER_MOTION, 0, w);
    step("O selects PointerMotion on W: motion to W");
    send(XCB_MOTION_NOTIFY, w, XCB_EVENT_MASK_BUTTON_PRESS, 0, w);
    step("motion to W with mask ButtonPress");

    click(c, 0);
    step("to C, propagate False");
    click(c, 1);
    step("to C, propagate True");
    dnp(XCB_EVENT_MASK_BUTTON_PRESS);
    click(c, 1);
    step("C do-not-propagate ButtonPress: to C, propagate True");
    send(XCB_BUTTON_PRESS, c, XCB_EVENT_MASK_BUTTON_PRESS | XCB_EVENT_MASK_POINTER_MOTION, 1, c);
    step("press to C with mask ButtonPress|PointerMotion, propagate True");
    dnp(0);
    send(XCB_BUTTON_PRESS, c, 0, 0, c);
    send(XCB_BUTTON_PRESS, c, 0, 1, c);
    step("press to C with an empty mask, propagate False then True");
    send(XCB_BUTTON_PRESS, root, 0, 1, root);
    step("press to the root with an empty mask");

    xcb_warp_pointer(s, XCB_NONE, root, 0, 0, 0, 0, 200, 200);
    sync_server(s);
    step("pointer warped into C");
    send(XCB_BUTTON_PRESS, XCB_SEND_EVENT_DEST_POINTER_WINDOW, XCB_EVENT_MASK_BUTTON_PRESS, 0, c);
    step("press to PointerWindow, propagate False");
    send(XCB_BUTTON_PRESS, XCB_SEND_EVENT_DEST_POINTER_WINDOW, XCB_EVENT_MASK_BUTTON_PRESS, 1, c);
    step("press to PointerWindow, propagate True");

    focus(w);
    send(XCB_KEY_PRESS, XCB_SEND_EVENT_DEST_ITEM_FOCUS, XCB_EVENT_MASK_KEY_PRESS, 0, c);
    step("focus W, pointer in C: key to InputFocus, propagate False");
    send(XCB_KEY_PRESS, XCB_SEND_EVENT_DEST_ITEM_FOCUS, XCB_EVENT_MASK_KEY_PRESS, 1, c);
    step("focus W, pointer in C: key to InputFocus, propagate True");
    focus(c);
    send(XCB_KEY_PRESS, XCB_SEND_EVENT_DEST_ITEM_FOCUS, XCB_EVENT_MASK_KEY_PRESS, 1, c);
    step("focus C, pointer in C: key to InputFocus, propagate True");
    focus(XCB_NONE);
    send(XCB_KEY_PRESS, XCB_SEND_EVENT_DEST_ITEM_FOCUS, XCB_EVENT_MASK_KEY_PRESS, 1, c);
    step("focus None: key to InputFocus, propagate True");
    xcb_set_input_focus(r, XCB_INPUT_FOCUS_POINTER_ROOT, XCB_INPUT_FOCUS_POINTER_ROOT,
                        XCB_CURRENT_TIME);
    sync_server(r);
    drain(r, "(focus PointerRoot) R");

    error_of("BadWindow destination", XCB_BUTTON_PRESS, 0, 0x7ffffff0, 0, 0);
    error_of("propagate 2", XCB_BUTTON_PRESS, 0, w, 0, 2);
    error_of("mask bit 25", XCB_BUTTON_PRESS, 0, w, 0x02000000, 0);
    error_of("event type 1", 1, 0, w, 0, 0);
    error_of("event type 35", 35, 0, w, 0, 0);
    error_of("event type 40", 40, 0, w, 0, 0);
    error_of("ClientMessage format 7", XCB_CLIENT_MESSAGE, 7, w, 0, 0);
    error_of("ClientMessage format 32", XCB_CLIENT_MESSAGE, 32, w, 0, 0);
    step("after the errors");

    xcb_test_fake_input(s, XCB_BUTTON_PRESS, 1, XCB_CURRENT_TIME, XCB_NONE, 0, 0, 0);
    xcb_test_fake_input(s, XCB_BUTTON_RELEASE, 1, XCB_CURRENT_TIME, XCB_NONE, 0, 0, 0);
    step("real XTEST click in C (XI2 + core on W)");

    FILE *f = fopen("PROBE-DONE", "w");
    if (f)
        fclose(f);
    xcb_disconnect(s);
    xcb_disconnect(o);
    xcb_disconnect(r);
    return 0;
}
