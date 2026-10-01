/* The window tree changes under a pointer that never moves: map, unmap,
 * configure, restack, circulate, shape, reparent and destroy, with and
 * without a pointer grab, under PointerRoot and an explicit focus. Xorg
 * re-evaluates the sprite after each of these (WindowsRestructured), so the
 * crossings arrive within the request; a sloppy-focus WM (muffin) focuses on
 * that EnterNotify.
 *
 *   ./probe <width> <height>
 *
 * Connection L does every request and selects core crossings, focus and
 * StructureNotify; connection X selects the XI2 Enter/Leave/FocusIn/FocusOut
 * on the same windows. Each step prints its name, then both round-trip and
 * print what they got, L first. probe.log carries no ids or times, so Xorg
 * and yserver runs diff directly.
 *
 *   cc -O1 -o probe crossing-tree-probe.c -lxcb -lxcb-shape -lxcb-xfixes -lxcb-xinput
 */
#include <stdio.h>
#include <stdlib.h>
#include <xcb/shape.h>
#include <xcb/xcb.h>
#include <xcb/xfixes.h>
#include <xcb/xinput.h>

#define NWIN 6

static xcb_connection_t *l, *x;
static xcb_window_t root, win[NWIN];
static const char *names[NWIN] = {"A", "B", "C", "D", "E", "F"};
static uint8_t xi_opcode;
static int16_t px, py;
static int grab_status = -2;

static const char *name(xcb_window_t w)
{
    if (w == XCB_NONE)
        return "None";
    if (w == root)
        return "root";
    for (int i = 0; i < NWIN; i++)
        if (win[i] && w == win[i])
            return names[i];
    return "other";
}

static const char *detail_name(uint8_t d)
{
    static const char *n[] = {"Ancestor", "Virtual", "Inferior", "Nonlinear",
                              "NonlinearVirtual", "Pointer", "PointerRoot", "None"};
    return d < 8 ? n[d] : "?";
}

static const char *mode_name(uint8_t m)
{
    static const char *n[] = {"Normal", "Grab", "Ungrab", "WhileGrabbed",
                              "PassiveGrab", "PassiveUngrab"};
    return m < 6 ? n[m] : "?";
}

static void sync_conn(xcb_connection_t *conn)
{
    free(xcb_get_input_focus_reply(conn, xcb_get_input_focus(conn), NULL));
}

static void print_core(xcb_generic_event_t *ev)
{
    switch (ev->response_type & 0x7f) {
    case XCB_ENTER_NOTIFY:
    case XCB_LEAVE_NOTIFY: {
        xcb_enter_notify_event_t *e = (xcb_enter_notify_event_t *)ev;
        printf("  core %s event=%s child=%s detail=%s mode=%s focus=%d at=%d,%d\n",
               (ev->response_type & 0x7f) == XCB_ENTER_NOTIFY ? "Enter" : "Leave",
               name(e->event), name(e->child), detail_name(e->detail), mode_name(e->mode),
               e->same_screen_focus & 1, e->event_x, e->event_y);
        break;
    }
    case XCB_FOCUS_IN:
    case XCB_FOCUS_OUT: {
        xcb_focus_in_event_t *e = (xcb_focus_in_event_t *)ev;
        printf("  core %s event=%s detail=%s mode=%s\n",
               (ev->response_type & 0x7f) == XCB_FOCUS_IN ? "FocusIn" : "FocusOut",
               name(e->event), detail_name(e->detail), mode_name(e->mode));
        break;
    }
    case XCB_MAP_NOTIFY: {
        xcb_map_notify_event_t *e = (xcb_map_notify_event_t *)ev;
        printf("  core MapNotify event=%s window=%s\n", name(e->event), name(e->window));
        break;
    }
    case XCB_UNMAP_NOTIFY: {
        xcb_unmap_notify_event_t *e = (xcb_unmap_notify_event_t *)ev;
        printf("  core UnmapNotify event=%s window=%s\n", name(e->event), name(e->window));
        break;
    }
    case XCB_DESTROY_NOTIFY: {
        xcb_destroy_notify_event_t *e = (xcb_destroy_notify_event_t *)ev;
        printf("  core DestroyNotify event=%s window=%s\n", name(e->event), name(e->window));
        break;
    }
    case XCB_REPARENT_NOTIFY: {
        xcb_reparent_notify_event_t *e = (xcb_reparent_notify_event_t *)ev;
        printf("  core ReparentNotify event=%s window=%s parent=%s\n", name(e->event),
               name(e->window), name(e->parent));
        break;
    }
    case XCB_CONFIGURE_NOTIFY: {
        xcb_configure_notify_event_t *e = (xcb_configure_notify_event_t *)ev;
        printf("  core ConfigureNotify event=%s window=%s\n", name(e->event), name(e->window));
        break;
    }
    case XCB_CIRCULATE_NOTIFY: {
        xcb_circulate_notify_event_t *e = (xcb_circulate_notify_event_t *)ev;
        printf("  core CirculateNotify event=%s window=%s\n", name(e->event), name(e->window));
        break;
    }
    case 0:
        printf("  core error code=%u\n", ((xcb_generic_error_t *)ev)->error_code);
        break;
    default:
        break;
    }
}

static void print_xi2(xcb_generic_event_t *ev)
{
    if ((ev->response_type & 0x7f) != XCB_GE_GENERIC)
        return;
    xcb_ge_generic_event_t *g = (xcb_ge_generic_event_t *)ev;
    if (g->extension != xi_opcode)
        return;
    static const char *n[] = {[XCB_INPUT_ENTER] = "Enter", [XCB_INPUT_LEAVE] = "Leave",
                              [XCB_INPUT_FOCUS_IN] = "FocusIn",
                              [XCB_INPUT_FOCUS_OUT] = "FocusOut"};
    if (g->event_type < XCB_INPUT_ENTER || g->event_type > XCB_INPUT_FOCUS_OUT)
        return;
    xcb_input_enter_event_t *e = (xcb_input_enter_event_t *)ev;
    printf("  xi2 %s dev=%s src=%s event=%s child=%s detail=%s mode=%s focus=%d at=%d,%d\n",
           n[g->event_type], e->deviceid == 2 || e->deviceid == 3 ? "master" : "slave",
           e->sourceid == e->deviceid ? "master" : "slave", name(e->event), name(e->child),
           detail_name(e->detail), mode_name(e->mode), e->focus, e->event_x >> 16,
           e->event_y >> 16);
}

static void step(const char *what)
{
    if (what)
        printf("%s\n", what);
    if (grab_status != -2)
        printf("  GrabPointer status=%d\n", grab_status);
    grab_status = -2;
    sync_conn(l);
    sync_conn(x);
    xcb_generic_event_t *ev;
    while ((ev = xcb_poll_for_event(l))) {
        if (what)
            print_core(ev);
        free(ev);
    }
    while ((ev = xcb_poll_for_event(x))) {
        if (what)
            print_xi2(ev);
        free(ev);
    }
    fflush(stdout);
}

static void select_xi2(xcb_window_t w)
{
    struct {
        xcb_input_event_mask_t h;
        uint32_t m;
    } mask = {{XCB_INPUT_DEVICE_ALL_MASTER, 1},
              XCB_INPUT_XI_EVENT_MASK_ENTER | XCB_INPUT_XI_EVENT_MASK_LEAVE |
                  XCB_INPUT_XI_EVENT_MASK_FOCUS_IN | XCB_INPUT_XI_EVENT_MASK_FOCUS_OUT};
    xcb_input_xi_select_events(x, w, 1, &mask.h);
}

/* Window i, a child of parent at x,y (relative), size w x h, both
 * connections listening on it. Not mapped. */
static void make(int i, xcb_window_t parent, int16_t wx, int16_t wy, uint16_t w, uint16_t h)
{
    uint32_t v[] = {0x808080, 1,
                    XCB_EVENT_MASK_ENTER_WINDOW | XCB_EVENT_MASK_LEAVE_WINDOW |
                        XCB_EVENT_MASK_FOCUS_CHANGE | XCB_EVENT_MASK_STRUCTURE_NOTIFY};
    win[i] = xcb_generate_id(l);
    xcb_create_window(l, XCB_COPY_FROM_PARENT, win[i], parent, wx, wy, w, h, 0,
                      XCB_WINDOW_CLASS_INPUT_OUTPUT, XCB_COPY_FROM_PARENT,
                      XCB_CW_BACK_PIXEL | XCB_CW_OVERRIDE_REDIRECT | XCB_CW_EVENT_MASK, v);
    sync_conn(l);
    select_xi2(win[i]);
    sync_conn(x);
}

static void move(int i, int16_t wx, int16_t wy)
{
    uint32_t v[] = {(uint32_t)wx, (uint32_t)wy};
    xcb_configure_window(l, win[i], XCB_CONFIG_WINDOW_X | XCB_CONFIG_WINDOW_Y, v);
}

static void resize(int i, uint16_t w, uint16_t h)
{
    uint32_t v[] = {w, h};
    xcb_configure_window(l, win[i], XCB_CONFIG_WINDOW_WIDTH | XCB_CONFIG_WINDOW_HEIGHT, v);
}

static void stack(int i, uint32_t mode)
{
    xcb_configure_window(l, win[i], XCB_CONFIG_WINDOW_STACK_MODE, &mode);
}

static void grab(int i, int owner_events)
{
    xcb_grab_pointer_reply_t *r = xcb_grab_pointer_reply(
        l,
        xcb_grab_pointer(l, owner_events, win[i],
                         XCB_EVENT_MASK_ENTER_WINDOW | XCB_EVENT_MASK_LEAVE_WINDOW,
                         XCB_GRAB_MODE_ASYNC, XCB_GRAB_MODE_ASYNC, XCB_NONE, XCB_NONE,
                         XCB_CURRENT_TIME),
        NULL);
    grab_status = r ? r->status : -1;
    free(r);
}

int main(int argc, char **argv)
{
    if (argc < 3)
        return 2;
    uint16_t sw = (uint16_t)atoi(argv[1]), sh = (uint16_t)atoi(argv[2]);
    px = (int16_t)(sw / 2);
    py = (int16_t)(sh / 2);
    l = xcb_connect(NULL, NULL);
    x = xcb_connect(NULL, NULL);
    if (xcb_connection_has_error(l) || xcb_connection_has_error(x))
        return 1;
    root = xcb_setup_roots_iterator(xcb_get_setup(l)).data->root;
    const xcb_query_extension_reply_t *q = xcb_get_extension_data(x, &xcb_input_id);
    if (!q || !q->present)
        return 1;
    xi_opcode = q->major_opcode;
    free(xcb_input_xi_query_version_reply(x, xcb_input_xi_query_version(x, 2, 2), NULL));
    free(xcb_xfixes_query_version_reply(l, xcb_xfixes_query_version(l, 5, 0), NULL));

    /* The pointer moves once, to the centre; it stays there. */
    xcb_set_input_focus(l, XCB_INPUT_FOCUS_POINTER_ROOT, XCB_INPUT_FOCUS_POINTER_ROOT,
                        XCB_CURRENT_TIME);
    xcb_warp_pointer(l, XCB_NONE, root, 0, 0, 0, 0, px, py);
    uint32_t rootmask = XCB_EVENT_MASK_ENTER_WINDOW | XCB_EVENT_MASK_LEAVE_WINDOW |
                        XCB_EVENT_MASK_FOCUS_CHANGE;
    xcb_change_window_attributes(l, root, XCB_CW_EVENT_MASK, &rootmask);
    select_xi2(root);
    make(0, root, px - 100, py - 100, 200, 200);
    make(1, root, px - 50, py - 50, 100, 100);
    make(2, win[0], 50, 50, 100, 100);
    make(3, root, px + 200, py + 200, 100, 100);
    step(NULL);
    printf("pointer at the centre, focus PointerRoot\n");

    xcb_map_window(l, win[0]);
    step("map A over the pointer");
    xcb_map_window(l, win[1]);
    step("map B over A");
    xcb_unmap_window(l, win[1]);
    step("unmap B, revealing A");
    xcb_map_subwindows(l, win[0]);
    step("MapSubwindows(A) maps its child C under the pointer");
    move(0, px + 10, py + 10);
    step("move A off the pointer");
    move(0, px - 100, py - 100);
    step("move A back");
    resize(0, 40, 40);
    step("shrink A off the pointer");
    resize(0, 200, 200);
    step("grow A back");
    move(0, px - 100, py - 100);
    step("move A to where it is");
    xcb_map_window(l, win[1]);
    step("map B over C");
    stack(1, XCB_STACK_MODE_BELOW);
    step("lower B under A");
    stack(1, XCB_STACK_MODE_ABOVE);
    step("raise B");
    xcb_circulate_window(l, XCB_CIRCULATE_LOWER_HIGHEST, root);
    step("CirculateWindow(root, LowerHighest)");
    xcb_circulate_window(l, XCB_CIRCULATE_RAISE_LOWEST, root);
    step("CirculateWindow(root, RaiseLowest)");

    xcb_rectangle_t corner = {0, 0, 10, 10};
    xcb_shape_rectangles(l, XCB_SHAPE_SO_SET, XCB_SHAPE_SK_BOUNDING, 0, win[1], 0, 0, 1,
                         &corner);
    step("bounding shape of B misses the pointer");
    xcb_shape_mask(l, XCB_SHAPE_SO_SET, XCB_SHAPE_SK_BOUNDING, win[1], 0, 0, XCB_NONE);
    step("bounding shape of B reset");
    xcb_shape_rectangles(l, XCB_SHAPE_SO_SET, XCB_SHAPE_SK_INPUT, 0, win[1], 0, 0, 0, NULL);
    step("input shape of B emptied");
    xcb_shape_offset(l, XCB_SHAPE_SK_INPUT, win[1], 5, 5);
    step("input shape of B offset (still empty)");
    xcb_shape_mask(l, XCB_SHAPE_SO_SET, XCB_SHAPE_SK_INPUT, win[1], 0, 0, XCB_NONE);
    step("input shape of B reset");
    xcb_xfixes_region_t empty = xcb_generate_id(l);
    xcb_xfixes_create_region(l, empty, 0, NULL);
    xcb_xfixes_set_window_shape_region(l, win[1], XCB_SHAPE_SK_INPUT, 0, 0, empty);
    step("XFixes input region of B emptied");
    xcb_xfixes_set_window_shape_region(l, win[1], XCB_SHAPE_SK_INPUT, 0, 0, XCB_NONE);
    step("XFixes input region of B reset");
    xcb_xfixes_destroy_region(l, empty);

    xcb_reparent_window(l, win[1], win[3], 0, 0);
    step("reparent B into D, off the pointer");
    xcb_reparent_window(l, win[1], win[0], 60, 60);
    step("reparent B into A, under the pointer");
    xcb_destroy_window(l, win[1]);
    step("destroy B under the pointer");
    win[1] = 0;
    xcb_destroy_subwindows(l, win[0]);
    step("DestroySubwindows(A) with the pointer in C");
    win[2] = 0;
    make(2, win[0], 50, 50, 100, 100);
    xcb_map_window(l, win[2]);
    step("map a new C in A");
    xcb_destroy_window(l, win[0]);
    step("destroy A with the pointer in its child C");
    win[0] = win[2] = 0;

    make(4, root, px - 100, py - 100, 200, 200);
    make(5, root, px - 50, py - 50, 100, 100);
    xcb_map_window(l, win[4]);
    step("map E over the pointer");
    xcb_set_input_focus(l, XCB_INPUT_FOCUS_NONE, win[4], XCB_CURRENT_TIME);
    step("focus E");
    xcb_map_window(l, win[5]);
    step("map F over E, focus on E");
    xcb_unmap_window(l, win[5]);
    step("unmap F, focus on E");
    xcb_set_input_focus(l, XCB_INPUT_FOCUS_POINTER_ROOT, XCB_INPUT_FOCUS_POINTER_ROOT,
                        XCB_CURRENT_TIME);
    step("focus PointerRoot");
    grab(4, 0);
    step("GrabPointer(E), owner_events false");
    xcb_map_window(l, win[5]);
    step("grabbed: map F over E");
    xcb_unmap_window(l, win[5]);
    step("grabbed: unmap F");
    xcb_map_window(l, win[5]);
    step("grabbed: map F again");
    xcb_ungrab_pointer(l, XCB_CURRENT_TIME);
    step("UngrabPointer");
    grab(4, 1);
    step("GrabPointer(E), owner_events true");
    xcb_unmap_window(l, win[5]);
    step("grabbed with owner_events: unmap F");
    xcb_map_window(l, win[5]);
    step("grabbed with owner_events: map F");
    xcb_ungrab_pointer(l, XCB_CURRENT_TIME);
    step("UngrabPointer");
    xcb_destroy_window(l, win[5]);
    xcb_destroy_window(l, win[4]);
    step("destroy F and E");
    win[4] = win[5] = 0;

    printf("done\n");
    fflush(stdout);
    FILE *f = fopen("PROBE-DONE", "w");
    if (f)
        fclose(f);
    xcb_disconnect(x);
    xcb_disconnect(l);
    return 0;
}
