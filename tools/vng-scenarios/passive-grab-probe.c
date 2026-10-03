/* A passive button grab whose event mask holds ButtonRelease only, in
 * synchronous pointer and keyboard mode, as CDE's dtwm puts on its front
 * panel controls: clicking one must hand the grabbing client the
 * activating ButtonPress, freeze both devices until its AllowEvents, and
 * then deliver what was queued.
 *
 *   ./passive-grab-probe
 *
 * Client A owns window W (200x200 at the root's origin) with the grab,
 * and window V beside it that selects ButtonPress and ButtonRelease and has
 * no grab, so its clicks are delivered plainly; client B drives the pointer and keyboard through XTEST. Each step logs
 * the events A received, in order.
 *
 *   cc -O1 -o passive-grab-probe passive-grab-probe.c -lxcb -lxcb-xtest
 */
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <xcb/xcb.h>
#include <xcb/xtest.h>

static xcb_connection_t *connect_retry(void)
{
    xcb_connection_t *c;
    /* Xorg resets when the previous run's last client leaves. */
    for (int tries = 0; (c = xcb_connect(NULL, NULL)) && xcb_connection_has_error(c) && tries < 50;
         tries++) {
        xcb_disconnect(c);
        usleep(100000);
    }
    return c;
}

static void sync_server(xcb_connection_t *c)
{
    free(xcb_get_input_focus_reply(c, xcb_get_input_focus(c), NULL));
}

static void events(xcb_connection_t *a, const char *when)
{
    usleep(300000);
    sync_server(a);
    printf("%s:", when);
    int any = 0;
    xcb_generic_event_t *e;
    while ((e = xcb_poll_for_event(a))) {
        any = 1;
        switch (e->response_type & 0x7f) {
        case XCB_BUTTON_PRESS:
        case XCB_BUTTON_RELEASE: {
            xcb_button_press_event_t *b = (xcb_button_press_event_t *)e;
            printf(" %s(button %u at %d,%d state 0x%x)",
                   (e->response_type & 0x7f) == XCB_BUTTON_PRESS ? "ButtonPress" : "ButtonRelease",
                   b->detail, b->event_x, b->event_y, b->state);
            break;
        }
        case XCB_KEY_PRESS:
        case XCB_KEY_RELEASE:
            printf(" %s", (e->response_type & 0x7f) == XCB_KEY_PRESS ? "KeyPress" : "KeyRelease");
            break;
        case XCB_MOTION_NOTIFY:
            printf(" MotionNotify");
            break;
        case XCB_ENTER_NOTIFY:
        case XCB_LEAVE_NOTIFY: {
            xcb_enter_notify_event_t *n = (xcb_enter_notify_event_t *)e;
            printf(" %s(mode %u)", (e->response_type & 0x7f) == XCB_ENTER_NOTIFY ? "Enter" : "Leave",
                   n->mode);
            break;
        }
        case XCB_FOCUS_IN:
        case XCB_FOCUS_OUT:
            printf(" %s", (e->response_type & 0x7f) == XCB_FOCUS_IN ? "FocusIn" : "FocusOut");
            break;
        case XCB_MAPPING_NOTIFY:
            printf(" MappingNotify(request %u)", ((xcb_mapping_notify_event_t *)e)->request);
            break;
        case 0:
            printf(" error %u", ((xcb_generic_error_t *)e)->error_code);
            break;
        default:
            printf(" event %u", e->response_type & 0x7f);
        }
        free(e);
    }
    printf("%s\n", any ? "" : " none");
    fflush(stdout);
}

static void fake(xcb_connection_t *b, uint8_t type, uint8_t detail)
{
    xcb_test_fake_input(b, type, detail, XCB_CURRENT_TIME, XCB_NONE, 0, 0, 0);
    sync_server(b);
}

int main(void)
{
    xcb_connection_t *a = connect_retry(), *b = connect_retry();
    if (xcb_connection_has_error(a) || xcb_connection_has_error(b)) {
        printf("cannot connect\n");
        return 1;
    }
    xcb_screen_t *s = xcb_setup_roots_iterator(xcb_get_setup(a)).data;
    free(xcb_test_get_version_reply(b, xcb_test_get_version(b, 2, 2), NULL));
    xcb_test_fake_input(b, XCB_MOTION_NOTIFY, 0, XCB_CURRENT_TIME, s->root, 400, 400, 0);
    sync_server(b);

    xcb_window_t w = xcb_generate_id(a);
    uint32_t vals[2] = {s->white_pixel,
                        XCB_EVENT_MASK_KEY_PRESS | XCB_EVENT_MASK_KEY_RELEASE |
                            XCB_EVENT_MASK_FOCUS_CHANGE};
    xcb_create_window(a, XCB_COPY_FROM_PARENT, w, s->root, 0, 0, 200, 200, 0,
                      XCB_WINDOW_CLASS_INPUT_OUTPUT, s->root_visual,
                      XCB_CW_BACK_PIXEL | XCB_CW_EVENT_MASK, vals);
    xcb_map_window(a, w);
    xcb_window_t v = xcb_generate_id(a);
    uint32_t vvals[2] = {s->white_pixel,
                         XCB_EVENT_MASK_BUTTON_PRESS | XCB_EVENT_MASK_BUTTON_RELEASE};
    xcb_create_window(a, XCB_COPY_FROM_PARENT, v, s->root, 300, 0, 200, 200, 0,
                      XCB_WINDOW_CLASS_INPUT_OUTPUT, s->root_visual,
                      XCB_CW_BACK_PIXEL | XCB_CW_EVENT_MASK, vvals);
    xcb_map_window(a, v);
    xcb_grab_button(a, 0, w, XCB_EVENT_MASK_BUTTON_RELEASE, XCB_GRAB_MODE_SYNC,
                    XCB_GRAB_MODE_SYNC, XCB_NONE, XCB_NONE, XCB_BUTTON_INDEX_ANY,
                    XCB_MOD_MASK_ANY);
    xcb_set_input_focus(a, XCB_INPUT_FOCUS_POINTER_ROOT, w, XCB_CURRENT_TIME);
    sync_server(a);
    usleep(300000);
    events(a, "mapped, focused");

    xcb_test_fake_input(b, XCB_MOTION_NOTIFY, 0, XCB_CURRENT_TIME, s->root, 350, 50, 0);
    sync_server(b);
    fake(b, XCB_BUTTON_PRESS, 1);
    fake(b, XCB_BUTTON_RELEASE, 1);
    events(a, "V (no grab) clicked");

    xcb_test_fake_input(b, XCB_MOTION_NOTIFY, 0, XCB_CURRENT_TIME, s->root, 50, 50, 0);
    sync_server(b);
    events(a, "pointer moved into W");
    fake(b, XCB_BUTTON_PRESS, 1);
    events(a, "button 1 pressed");
    fake(b, XCB_BUTTON_RELEASE, 1);
    fake(b, XCB_KEY_PRESS, 38);
    fake(b, XCB_KEY_RELEASE, 38);
    events(a, "released, key a typed, still frozen");
    xcb_allow_events(a, XCB_ALLOW_ASYNC_BOTH, XCB_CURRENT_TIME);
    sync_server(a);
    events(a, "AllowEvents AsyncBoth");
    fake(b, XCB_KEY_PRESS, 38);
    fake(b, XCB_KEY_RELEASE, 38);
    events(a, "key a typed");

    /* Again, thawed by ReplayPointer: the press goes on past the grab. */
    fake(b, XCB_BUTTON_PRESS, 1);
    events(a, "button 1 pressed again");
    xcb_allow_events(a, XCB_ALLOW_REPLAY_POINTER, XCB_CURRENT_TIME);
    sync_server(a);
    events(a, "AllowEvents ReplayPointer");
    fake(b, XCB_BUTTON_RELEASE, 1);
    events(a, "released");
    xcb_allow_events(a, XCB_ALLOW_ASYNC_BOTH, XCB_CURRENT_TIME);
    fake(b, XCB_KEY_PRESS, 38);
    fake(b, XCB_KEY_RELEASE, 38);
    events(a, "AllowEvents AsyncBoth, key a typed");

    FILE *done = fopen("PROBE-DONE", "w");
    if (done)
        fclose(done);
    xcb_disconnect(b);
    xcb_disconnect(a);
    return 0;
}
