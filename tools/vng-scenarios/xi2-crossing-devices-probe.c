/* Which devices XI2 crossings carry when the pointer is moved by an attached
 * slave (the XTEST pointer). Xorg computes Enter/Leave only for a device
 * with its own sprite, i.e. a master or a floating slave
 * (Xi/exevents.c:1854 CheckMotion; dix/enterleave.c DeviceEnterLeaveEvents):
 * an XIAllDevices selector gets one crossing, deviceid = master, sourceid =
 * the slave, while motion and buttons arrive twice (slave form, then master
 * form). FocusIn/Out come from the master keyboard with sourceid = itself.
 *
 *   ./probe
 *
 * Connection L makes the windows and does the requests. Connection X selects
 * Enter/Leave/Motion/Button/Focus with XIAllDevices on root, A and B;
 * connection S selects Enter/Leave/Motion on A for the XTEST pointer only;
 * connection M selects Enter/Leave on A with XIAllMasterDevices.
 *
 *   cc -O1 -o probe xi2-crossing-devices-probe.c -lxcb -lxcb-xinput -lxcb-xtest
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <xcb/xcb.h>
#include <xcb/xinput.h>
#include <xcb/xtest.h>

static xcb_connection_t *l, *x, *s, *m;
static xcb_window_t root, a, b;
static uint8_t xi_opcode;
static uint16_t xtest_ptr;

static const char *name(xcb_window_t w)
{
    if (w == XCB_NONE)
        return "None";
    if (w == root)
        return "root";
    if (w == a)
        return "A";
    if (w == b)
        return "B";
    return "other";
}

static const char *dev(uint16_t d)
{
    if (d == 2)
        return "master-ptr";
    if (d == 3)
        return "master-kbd";
    if (d == xtest_ptr)
        return "xtest-ptr";
    return "other";
}

static const char *detail_name(uint32_t d)
{
    static const char *n[] = {"Ancestor", "Virtual", "Inferior", "Nonlinear",
                              "NonlinearVirtual", "Pointer", "PointerRoot", "None"};
    return d < 8 ? n[d] : "?";
}

static void sync_conn(xcb_connection_t *conn)
{
    free(xcb_get_input_focus_reply(conn, xcb_get_input_focus(conn), NULL));
}

static void print_xi2(const char *who, xcb_generic_event_t *ev)
{
    if ((ev->response_type & 0x7f) != XCB_GE_GENERIC)
        return;
    xcb_ge_generic_event_t *g = (xcb_ge_generic_event_t *)ev;
    if (g->extension != xi_opcode)
        return;
    switch (g->event_type) {
    case XCB_INPUT_ENTER:
    case XCB_INPUT_LEAVE:
    case XCB_INPUT_FOCUS_IN:
    case XCB_INPUT_FOCUS_OUT: {
        static const char *n[] = {[XCB_INPUT_ENTER] = "Enter", [XCB_INPUT_LEAVE] = "Leave",
                                  [XCB_INPUT_FOCUS_IN] = "FocusIn",
                                  [XCB_INPUT_FOCUS_OUT] = "FocusOut"};
        xcb_input_enter_event_t *e = (xcb_input_enter_event_t *)ev;
        printf("  %s %s dev=%s src=%s event=%s detail=%s mode=%u\n", who, n[g->event_type],
               dev(e->deviceid), dev(e->sourceid), name(e->event), detail_name(e->detail),
               e->mode);
        break;
    }
    case XCB_INPUT_MOTION:
    case XCB_INPUT_BUTTON_PRESS:
    case XCB_INPUT_BUTTON_RELEASE: {
        static const char *n[] = {[XCB_INPUT_MOTION] = "Motion",
                                  [XCB_INPUT_BUTTON_PRESS] = "ButtonPress",
                                  [XCB_INPUT_BUTTON_RELEASE] = "ButtonRelease"};
        xcb_input_button_press_event_t *e = (xcb_input_button_press_event_t *)ev;
        printf("  %s %s dev=%s src=%s event=%s detail=%u at=%d,%d\n", who, n[g->event_type],
               dev(e->deviceid), dev(e->sourceid), name(e->event),
               g->event_type == XCB_INPUT_MOTION ? 0 : e->detail, e->event_x >> 16,
               e->event_y >> 16);
        break;
    }
    default:
        break;
    }
}

static void drain(const char *who, xcb_connection_t *conn, int print)
{
    xcb_generic_event_t *ev;
    while ((ev = xcb_poll_for_event(conn))) {
        if (print)
            print_xi2(who, ev);
        free(ev);
    }
}

static void step(const char *what)
{
    sync_conn(l);
    usleep(200000);
    sync_conn(l);
    sync_conn(x);
    sync_conn(s);
    sync_conn(m);
    if (what)
        printf("%s\n", what);
    drain("X", x, what != NULL);
    drain("S", s, what != NULL);
    drain("M", m, what != NULL);
    fflush(stdout);
}

static void motion(int16_t px, int16_t py)
{
    xcb_test_fake_input(l, XCB_MOTION_NOTIFY, 0, XCB_CURRENT_TIME, root, px, py, 0);
}

static void button(int press)
{
    xcb_test_fake_input(l, press ? XCB_BUTTON_PRESS : XCB_BUTTON_RELEASE, 1, XCB_CURRENT_TIME,
                        XCB_NONE, 0, 0, 0);
}

static void select_on(xcb_connection_t *conn, xcb_window_t w, uint16_t deviceid, uint32_t bits)
{
    struct {
        xcb_input_event_mask_t h;
        uint32_t m;
    } mask = {{deviceid, 1}, bits};
    xcb_input_xi_select_events(conn, w, 1, &mask.h);
    sync_conn(conn);
}

static xcb_window_t make(int16_t wx, int16_t wy)
{
    uint32_t v[] = {0x808080, 1};
    xcb_window_t w = xcb_generate_id(l);
    xcb_create_window(l, XCB_COPY_FROM_PARENT, w, root, wx, wy, 200, 200, 0,
                      XCB_WINDOW_CLASS_INPUT_OUTPUT, XCB_COPY_FROM_PARENT,
                      XCB_CW_BACK_PIXEL | XCB_CW_OVERRIDE_REDIRECT, v);
    xcb_map_window(l, w);
    sync_conn(l);
    return w;
}

static int find_xtest_pointer(void)
{
    xcb_input_xi_query_device_reply_t *r =
        xcb_input_xi_query_device_reply(l, xcb_input_xi_query_device(l, 0), NULL);
    if (!r)
        return 0;
    xcb_input_xi_device_info_iterator_t it = xcb_input_xi_query_device_infos_iterator(r);
    for (; it.rem; xcb_input_xi_device_info_next(&it)) {
        const char *n = xcb_input_xi_device_info_name(it.data);
        int len = xcb_input_xi_device_info_name_length(it.data);
        if (it.data->type == XCB_INPUT_DEVICE_TYPE_SLAVE_POINTER &&
            len == (int)strlen("Virtual core XTEST pointer") &&
            !memcmp(n, "Virtual core XTEST pointer", (size_t)len))
            xtest_ptr = it.data->deviceid;
    }
    free(r);
    return xtest_ptr != 0;
}

int main(void)
{
    l = xcb_connect(NULL, NULL);
    x = xcb_connect(NULL, NULL);
    s = xcb_connect(NULL, NULL);
    m = xcb_connect(NULL, NULL);
    if (xcb_connection_has_error(l) || xcb_connection_has_error(x) ||
        xcb_connection_has_error(s) || xcb_connection_has_error(m))
        return 1;
    root = xcb_setup_roots_iterator(xcb_get_setup(l)).data->root;
    const xcb_query_extension_reply_t *q = xcb_get_extension_data(x, &xcb_input_id);
    if (!q || !q->present)
        return 1;
    xi_opcode = q->major_opcode;
    xcb_connection_t *all[] = {l, x, s, m};
    for (int i = 0; i < 4; i++)
        free(xcb_input_xi_query_version_reply(all[i], xcb_input_xi_query_version(all[i], 2, 2),
                                              NULL));
    if (!find_xtest_pointer()) {
        printf("no XTEST pointer\n");
        return 1;
    }

    xcb_set_input_focus(l, XCB_INPUT_FOCUS_POINTER_ROOT, XCB_INPUT_FOCUS_POINTER_ROOT,
                        XCB_CURRENT_TIME);
    motion(20, 20);
    a = make(100, 100);
    b = make(300, 100);
    step(NULL);

    const uint32_t crossing = XCB_INPUT_XI_EVENT_MASK_ENTER | XCB_INPUT_XI_EVENT_MASK_LEAVE;
    const uint32_t every = crossing | XCB_INPUT_XI_EVENT_MASK_MOTION |
                           XCB_INPUT_XI_EVENT_MASK_BUTTON_PRESS |
                           XCB_INPUT_XI_EVENT_MASK_BUTTON_RELEASE |
                           XCB_INPUT_XI_EVENT_MASK_FOCUS_IN | XCB_INPUT_XI_EVENT_MASK_FOCUS_OUT;
    select_on(x, root, XCB_INPUT_DEVICE_ALL, every);
    select_on(x, a, XCB_INPUT_DEVICE_ALL, every);
    select_on(x, b, XCB_INPUT_DEVICE_ALL, every);
    select_on(s, a, xtest_ptr, crossing | XCB_INPUT_XI_EVENT_MASK_MOTION);
    select_on(m, a, XCB_INPUT_DEVICE_ALL_MASTER, crossing);
    step(NULL);
    printf("pointer on root at 20,20; A at 100,100 and B at 300,100, both 200x200\n");

    motion(150, 150);
    step("XTEST motion into A");
    motion(160, 160);
    step("XTEST motion inside A");
    motion(350, 150);
    step("XTEST motion from A into B");
    button(1);
    step("XTEST press button 1 in B");
    button(0);
    step("XTEST release button 1 in B");
    motion(150, 150);
    step("XTEST motion from B into A");
    motion(20, 20);
    step("XTEST motion from A onto root");
    xcb_set_input_focus(l, XCB_INPUT_FOCUS_NONE, a, XCB_CURRENT_TIME);
    step("focus A");
    xcb_set_input_focus(l, XCB_INPUT_FOCUS_POINTER_ROOT, XCB_INPUT_FOCUS_POINTER_ROOT,
                        XCB_CURRENT_TIME);
    step("focus PointerRoot");

    printf("done\n");
    fflush(stdout);
    FILE *f = fopen("PROBE-DONE", "w");
    if (f)
        fclose(f);
    for (int i = 0; i < 4; i++)
        xcb_disconnect(all[i]);
    return 0;
}
