/* destroy-order-probe — the client-visible event stream of DestroyWindow on
 *                       a mapped subtree (window-destroy plan, phase 1).
 *
 *   ./destroy-order-probe c | t [destroy | disconnect][-appsel][-kbd][-ptr][-present]
 *
 * `app` owns mapped T (child of root) and its mapped child C; the pointer is
 * warped into C (c) or into T outside C (t). `grb` owns a selection on C, holds an active pointer grab on
 * C and sets the keyboard focus to C (RevertToParent). `obs` selects
 * StructureNotify/FocusChange/EnterLeave on T and C, SubstructureNotify/
 * Exposure/FocusChange/EnterLeave on root, and XFixesSelectSelectionInput on
 * the selection. Then `app` destroys T; every event `obs` and `grb` receive is
 * printed, one per line, in arrival order.
 *
 * `disconnect`: `app` closes its connection instead of destroying T.
 * Modifiers, joined to `destroy`/`disconnect` with `-` (e.g. `disconnect-kbd-appsel`):
 *   `appsel`  `app`, not `grb`, sets the selection owner on C;
 *   `kbd`     `app` also holds a keyboard grab on T;
 *   `ptr`     the active pointer grab is `app`'s, on T, instead of `grb`'s on C;
 *   `present` C gets a 3px border and `obs` selects PresentConfigureNotify on T and C.
 *
 *   cc -O1 -o destroy-order-probe destroy-order-probe.c -lxcb -lxcb-xfixes -lxcb-present
 */
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <xcb/xcb.h>
#include <xcb/present.h>
#include <xcb/xfixes.h>

static xcb_connection_t *app, *grb, *obs;
static xcb_window_t root, T, C;
static uint8_t xfixes_ev, present_op;
static uint32_t eT, eC;

static const char *nm(uint32_t id)
{
    static char buf[4][16];
    static int k;
    if (id == root)
        return "root";
    if (id == T)
        return "T";
    if (id == C)
        return "C";
    if (id == 0)
        return "None";
    if (id == 1)
        return "PointerRoot";
    k = (k + 1) & 3;
    snprintf(buf[k], 16, "0x%x", id);
    return buf[k];
}

static const char *detail(uint8_t d)
{
    static const char *n[] = {"Ancestor", "Virtual", "Inferior", "Nonlinear", "NonlinearVirtual",
                              "Pointer", "PointerRoot", "DetailNone"};
    return d < 8 ? n[d] : "?";
}

static const char *mode(uint8_t m)
{
    static const char *n[] = {"Normal", "Grab", "Ungrab", "WhileGrabbed"};
    return m < 4 ? n[m] : "?";
}

static void rt(xcb_connection_t *c) { free(xcb_get_input_focus_reply(c, xcb_get_input_focus(c), NULL)); }

static void drain(xcb_connection_t *c, const char *who)
{
    struct pollfd p = {xcb_get_file_descriptor(c), POLLIN, 0};
    for (;;) {
        xcb_generic_event_t *ev = xcb_poll_for_event(c);
        if (!ev) {
            xcb_flush(c);
            if (poll(&p, 1, 200) <= 0)
                return;
            ev = xcb_poll_for_event(c);
            if (!ev)
                return;
        }
        uint8_t t = ev->response_type & 0x7f;
        printf("  %s: ", who);
        if (t == 0) {
            xcb_generic_error_t *e = (void *)ev;
            printf("Error(%u op%u.%u)", e->error_code, e->major_code, e->minor_code);
        } else if (t == XCB_EXPOSE) {
            xcb_expose_event_t *e = (void *)ev;
            printf("Expose(%s %u,%u %ux%u count=%u)", nm(e->window), e->x, e->y, e->width, e->height,
                   e->count);
        } else if (t == XCB_UNMAP_NOTIFY) {
            xcb_unmap_notify_event_t *e = (void *)ev;
            printf("UnmapNotify(event=%s window=%s fromConfigure=%u)", nm(e->event), nm(e->window),
                   e->from_configure);
        } else if (t == XCB_DESTROY_NOTIFY) {
            xcb_destroy_notify_event_t *e = (void *)ev;
            printf("DestroyNotify(event=%s window=%s)", nm(e->event), nm(e->window));
        } else if (t == XCB_ENTER_NOTIFY || t == XCB_LEAVE_NOTIFY) {
            xcb_enter_notify_event_t *e = (void *)ev;
            printf("%s(event=%s child=%s detail=%s mode=%s root=%d,%d event=%d,%d sf=0x%x)",
                   t == XCB_ENTER_NOTIFY ? "EnterNotify" : "LeaveNotify", nm(e->event), nm(e->child),
                   detail(e->detail), mode(e->mode), e->root_x, e->root_y, e->event_x, e->event_y,
                   e->same_screen_focus);
        } else if (t == XCB_FOCUS_IN || t == XCB_FOCUS_OUT) {
            xcb_focus_in_event_t *e = (void *)ev;
            printf("%s(event=%s detail=%s mode=%s)", t == XCB_FOCUS_IN ? "FocusIn" : "FocusOut",
                   nm(e->event), detail(e->detail), mode(e->mode));
        } else if (t == xfixes_ev + XCB_XFIXES_SELECTION_NOTIFY) {
            xcb_xfixes_selection_notify_event_t *e = (void *)ev;
            printf("XFixesSelectionNotify(subtype=%u window=%s owner=%s)", e->subtype, nm(e->window),
                   nm(e->owner));
        } else if (t == XCB_GE_GENERIC && ((xcb_ge_generic_event_t *)ev)->extension == present_op &&
                   ((xcb_ge_generic_event_t *)ev)->event_type == XCB_PRESENT_CONFIGURE_NOTIFY) {
            xcb_present_configure_notify_event_t *e = (void *)ev;
            printf("PresentConfigureNotify(eid=%s window=%s x=%d y=%d w=%u h=%u off=%d,%d pixmap=%ux%u "
                   "flags=0x%x)",
                   e->event == eT ? "eT" : e->event == eC ? "eC" : "?", nm(e->window), e->x, e->y, e->width,
                   e->height, e->off_x, e->off_y, e->pixmap_width, e->pixmap_height, e->pixmap_flags);
        } else {
            printf("event%u", t);
        }
        printf("\n");
        free(ev);
    }
}

static xcb_connection_t *conn(void)
{
    xcb_connection_t *c = xcb_connect(NULL, NULL);
    if (xcb_connection_has_error(c)) {
        fprintf(stderr, "cannot connect\n");
        exit(1);
    }
    return c;
}

int main(int argc, char **argv)
{
    int in_t = argc > 1 && strcmp(argv[1], "t") == 0;
    const char *how = argc > 2 ? argv[2] : "destroy";
    int disconnect = strncmp(how, "disconnect", 10) == 0;
    int app_sel = strstr(how, "appsel") != NULL;
    int app_kbd = strstr(how, "kbd") != NULL;
    int app_ptr = strstr(how, "ptr") != NULL;
    int present = strstr(how, "present") != NULL;
    app = conn();
    grb = conn();
    obs = conn();
    root = xcb_setup_roots_iterator(xcb_get_setup(app)).data->root;

    const xcb_query_extension_reply_t *x = xcb_get_extension_data(obs, &xcb_xfixes_id);
    if (!x || !x->present) {
        fprintf(stderr, "no XFIXES\n");
        return 1;
    }
    xfixes_ev = x->first_event;
    free(xcb_xfixes_query_version_reply(obs, xcb_xfixes_query_version(obs, 5, 0), NULL));

    T = xcb_generate_id(app);
    C = xcb_generate_id(app);
    uint32_t v[2] = {0x808080, 1};
    xcb_create_window(app, XCB_COPY_FROM_PARENT, T, root, 100, 100, 200, 200, 0,
                      XCB_WINDOW_CLASS_INPUT_OUTPUT, XCB_COPY_FROM_PARENT,
                      XCB_CW_BACK_PIXEL | XCB_CW_OVERRIDE_REDIRECT, v);
    xcb_create_window(app, XCB_COPY_FROM_PARENT, C, T, 50, 50, 100, 100, present ? 3 : 0,
                      XCB_WINDOW_CLASS_INPUT_OUTPUT,
                      XCB_COPY_FROM_PARENT, XCB_CW_BACK_PIXEL, v);
    xcb_map_window(app, C);
    xcb_map_window(app, T);
    rt(app);

    xcb_intern_atom_reply_t *a =
        xcb_intern_atom_reply(obs, xcb_intern_atom(obs, 0, 18, "DESTROY_ORDER_PROBE"), NULL);
    xcb_atom_t sel = a->atom;
    free(a);

    uint32_t m_root = XCB_EVENT_MASK_SUBSTRUCTURE_NOTIFY | XCB_EVENT_MASK_EXPOSURE |
                      XCB_EVENT_MASK_FOCUS_CHANGE | XCB_EVENT_MASK_ENTER_WINDOW | XCB_EVENT_MASK_LEAVE_WINDOW;
    uint32_t m_win = XCB_EVENT_MASK_STRUCTURE_NOTIFY | XCB_EVENT_MASK_FOCUS_CHANGE |
                     XCB_EVENT_MASK_ENTER_WINDOW | XCB_EVENT_MASK_LEAVE_WINDOW;
    xcb_change_window_attributes(obs, root, XCB_CW_EVENT_MASK, &m_root);
    xcb_change_window_attributes(obs, T, XCB_CW_EVENT_MASK, &m_win);
    xcb_change_window_attributes(obs, C, XCB_CW_EVENT_MASK, &m_win);
    xcb_xfixes_select_selection_input(obs, root, sel,
                                      XCB_XFIXES_SELECTION_EVENT_MASK_SET_SELECTION_OWNER |
                                          XCB_XFIXES_SELECTION_EVENT_MASK_SELECTION_WINDOW_DESTROY |
                                          XCB_XFIXES_SELECTION_EVENT_MASK_SELECTION_CLIENT_CLOSE);
    if (present) {
        const xcb_query_extension_reply_t *p = xcb_get_extension_data(obs, &xcb_present_id);
        if (!p || !p->present) {
            fprintf(stderr, "no Present\n");
            return 1;
        }
        present_op = p->major_opcode;
        free(xcb_present_query_version_reply(obs, xcb_present_query_version(obs, 1, 2), NULL));
        eT = xcb_generate_id(obs);
        eC = xcb_generate_id(obs);
        xcb_present_select_input(obs, eT, T, XCB_PRESENT_EVENT_MASK_CONFIGURE_NOTIFY);
        xcb_present_select_input(obs, eC, C, XCB_PRESENT_EVENT_MASK_CONFIGURE_NOTIFY);
    }
    rt(obs);

    xcb_warp_pointer(app, XCB_NONE, root, 0, 0, 0, 0, in_t ? 110 : 200, in_t ? 110 : 200);
    rt(app);

    xcb_set_selection_owner(app_sel ? app : grb, C, sel, XCB_CURRENT_TIME);
    rt(app);
    xcb_grab_pointer_reply_t *g = xcb_grab_pointer_reply(
        app_ptr ? app : grb,
        xcb_grab_pointer(app_ptr ? app : grb, 0, app_ptr ? T : C,
                         XCB_EVENT_MASK_ENTER_WINDOW | XCB_EVENT_MASK_LEAVE_WINDOW, XCB_GRAB_MODE_ASYNC,
                         XCB_GRAB_MODE_ASYNC, XCB_NONE, XCB_NONE, XCB_CURRENT_TIME),
        NULL);
    if (!g || g->status != XCB_GRAB_STATUS_SUCCESS) {
        fprintf(stderr, "grab failed\n");
        return 1;
    }
    free(g);
    xcb_set_input_focus(grb, XCB_INPUT_FOCUS_PARENT, C, XCB_CURRENT_TIME);
    rt(grb);
    if (app_kbd) {
        xcb_grab_keyboard_reply_t *k = xcb_grab_keyboard_reply(
            app, xcb_grab_keyboard(app, 0, T, XCB_CURRENT_TIME, XCB_GRAB_MODE_ASYNC, XCB_GRAB_MODE_ASYNC),
            NULL);
        if (!k || k->status != XCB_GRAB_STATUS_SUCCESS) {
            fprintf(stderr, "keyboard grab failed\n");
            return 1;
        }
        free(k);
    }
    rt(obs);

    printf("setup\n");
    drain(obs, "obs");
    drain(grb, "grb");

    if (disconnect) {
        xcb_disconnect(app);
        usleep(200000);
        app = conn();
        rt(app);
    } else {
        xcb_destroy_window(app, T);
        rt(app);
    }
    rt(grb);
    rt(obs);
    printf("%s\n", disconnect ? "disconnect app" : "destroy T");
    drain(obs, "obs");
    drain(grb, "grb");

    xcb_get_input_focus_reply_t *f = xcb_get_input_focus_reply(obs, xcb_get_input_focus(obs), NULL);
    printf("focus after: %s revert=%u\n", nm(f->focus), f->revert_to);
    free(f);
    xcb_grab_status_t st = 0;
    xcb_grab_pointer_reply_t *g2 = xcb_grab_pointer_reply(
        app, xcb_grab_pointer(app, 0, root, 0, XCB_GRAB_MODE_ASYNC, XCB_GRAB_MODE_ASYNC, XCB_NONE, XCB_NONE,
                              XCB_CURRENT_TIME),
        NULL);
    if (g2)
        st = g2->status;
    free(g2);
    printf("grab by another client after: %s\n", st == XCB_GRAB_STATUS_SUCCESS ? "Success" : "AlreadyGrabbed");

    xcb_disconnect(app);
    xcb_disconnect(grb);
    xcb_disconnect(obs);
    return 0;
}
