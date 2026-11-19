# Per-device implicit grabs: measurements against Xvfb 21.1.24

Oracle: `/data/Projects/xorg-21.1.24` (tag `xorg-server-21.1.24`), cited as `../xserver`. Xvfb ran on `:97` and reported vendor release `12101024`: master pointer 2, XTest pointer 4. yserver `48f69aab` (the head of PR #373) ran on `:96` with `--headless` and lavapipe.

**Setup.** The client opens three connections: A, B and an injector.
- **Windows:** W at root `(100,100)`, 300x300, owned by A. Its child D at W-relative `(20,20)`. OUT at root `(550,100)`, owned by B.
- **Positioning:** the injector moves the XTest pointer to `(145,145)` before the selections are made.
- **Drag:** motion to `(150,150)` over D, button 1 press, motion to `(650,150)` over OUT, release, then motion to `(660,160)`.

**Comparison.** Only recipient, event type, device/source and event window are compared. XTest attached-slave event and root coordinates, and the child, report the sprite before the motion on Xvfb. They are excluded, as in C5 and C7s of `2026-11-16-pointer-routing-xorg-measurements.md`. Enter/Leave crossings are excluded.

| case | setup | Xvfb 21.1.24 | yserver 48f69aab |
|---|---|---|---|
| I1 | A selects XI2 Motion/Press/Release on W for slave 4 and master 2. B selects the same on OUT. | A gets press d4+d2, drag Motion d4+d2 and release d4+d2, all on W. B gets only the final Motion d4+d2 on OUT. | d2 stays on A/W. The drag Motion d4 and release d4 go to B on OUT. |
| I1a | Same as I1 with XIAllDevices (device 0): Qt6's selection, captured from qBittorrent. | Same as I1. | Same as I1. |
| I2 | A selects slave 4 only on W: Enlightenment 0.27's selection. B selects master 2 on OUT. | A gets press d4, drag Motion d4 and release d4 on W. B gets drag Motion d2 and release d2 on OUT, with no press. | A gets **d2** drag Motion and d2 release on W, although it never selected the master. B gets no d2 release. |
| I3 | A selects slave 4 only on W. B selects core PointerMotion/ButtonPress/ButtonRelease on OUT. | A gets d4 on W. B gets core MotionNotify and ButtonRelease on OUT. | A gets d2 Motion/Release on W. B gets no core release. |
| I4 | As I2, but B selects slave 4 on OUT. Mid-drag, B issues XIGrabDevice on OUT, first for slave 4, then for master 2. | Slave 4 status 1 (AlreadyGrabbed). Master 2 status 0 (Success). A gets release d4 on W. B gets release d2 on OUT (its grab). | Slave status 0, master status 1, so the statuses are swapped. The release d4 goes to B. |
| I5 | XI1: A selects DeviceMotionNotify/DeviceButtonPress/DeviceButtonRelease for device 4 on W. B selects the same on OUT. | A gets DeviceMotionNotify and DeviceButtonRelease on W. | Both go to B on OUT. |
| I6 | A selects slave 4 only on W. B selects slave 4 on OUT. A unmaps W mid-drag. | The slave grab ends. B gets release d4 on OUT. | B gets the release, but drag Motion d4 already went to B, and A got a d2 Motion. |
| I7 | As I6, without the unmap: press button 1 over W/D, move to OUT, press button 3, release 1, move, release 3. | Every d4 event goes to A on W, including the button-3 press over OUT, until both buttons are up. | d4 goes to B on OUT; A gets d2. |
| I8 | As I6, but A issues XIUngrabDevice on slave 4 mid-drag instead of unmapping. | The implicit slave grab ends. B gets release d4 on OUT. | A gets a d2 release on W (the merged grab lives on the master). |

**Source.** Xorg 21.1.24 has one grab per device, `deviceGrab`.
- **Activation.** `ActivateImplicitGrab` (`dix/events.c:2152-2196`) runs for the device whose event was delivered. The grab type follows the delivered event: core ButtonPress gives CORE, DeviceButtonPress gives XI, XI_ButtonPress gives XI2.
- **No detach.** `ActivatePointerGrab` detaches a slave from its master only for explicit and passive grabs: `!(autoGrab & ImplicitGrabMask) && !IsMaster(mouse)`, `dix/events.c:1622-1624`.
- **Release.** The implicit or passive grab ends after the release that leaves that device's `buttonsDown` at zero (`Xi/exevents.c:1928-1955`).

**Cause in yserver.** `ImplicitGrabFanoutInfo::delivered_press` merges the slave and master candidates into one grab and installs it with `set_pointer_grab` as the master grab. Two consequences:
- The slave form has no grab.
- The master grab can belong to a client that only selected the slave form.

## Reproduction

```sh
cc -std=c11 -O2 -Wall -Wextra -Werror implicit-oracle.c -o implicit-oracle $(pkg-config --cflags --libs x11 xi xtst)
Xvfb :97 -screen 0 1024x768x24        # oracle install from the 2026-11-16 measurements
target/release/yserver :96 --headless  # lavapipe environment as in 2026-11-16
for c in I1 I1a I2 I3 I4 I5 I6 I7 I8; do ./implicit-oracle :97 $c; ./implicit-oracle :96 $c; done
```

## implicit-oracle.c

```c
#define _DEFAULT_SOURCE
#include <X11/Xlib.h>
#include <X11/Xlibint.h>
#include <X11/Xproto.h>
#include <X11/extensions/XInput.h>
#include <X11/extensions/XI.h>
#include <X11/extensions/XIproto.h>
#include <X11/extensions/XInput2.h>
#include <X11/extensions/XTest.h>
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/time.h>
#include <unistd.h>

#define XI2_MASTER_POINTER 2
#define XI2_ALL_MASTER 1
#define XI2_ALL_DEVICES 0
#define CORE_BUTTONS (ButtonPressMask | ButtonReleaseMask)
#define CORE_POINTER (PointerMotionMask | ButtonPressMask | ButtonReleaseMask)
#define XI_GRAB_BITS ((1u << XI_Motion) | (1u << XI_ButtonPress) | (1u << XI_ButtonRelease))

struct Setup {
    Display *a;
    Display *b;
    Display *inj;
    int screen;
    Window root;
    Window grab;
    Window child;
    Window outside;
    int xi_opcode;
    int xi_event_base;
    int xi_error_base;
    int master;
    int xtest_pointer;
    int xvfb_mouse;
    int child_owned_by_b;
    int outside_owned_by_b;
    int xi1_motion_type;
    int xi1_button_press_type;
    int xi1_button_release_type;
    int xi1_valuator_type;
};

static struct Setup *g_setup;
static int raw_capture_enabled;
static int raw_count;
static struct {
    unsigned char bytes[32];
} raw_events[64];
static Bool (*prev_motion_wire)(Display *, XEvent *, xEvent *);
static Bool (*prev_button_press_wire)(Display *, XEvent *, xEvent *);
static Bool (*prev_valuator_wire)(Display *, XEvent *, xEvent *);

static int x_error_handler(Display *display, XErrorEvent *event) {
    char message[256];
    XGetErrorText(display, event->error_code, message, sizeof(message));
    fprintf(stderr, "XERROR code=%u request=%u minor=%u resource=0x%lx text=%s\n",
            event->error_code, event->request_code, event->minor_code,
            event->resourceid, message);
    return 0;
}

static Bool capture_wire(Display *display, XEvent *re, xEvent *wire,
                         Bool (*previous)(Display *, XEvent *, xEvent *)) {
    if (raw_capture_enabled && raw_count < (int)(sizeof(raw_events) / sizeof(raw_events[0]))) {
        memcpy(raw_events[raw_count++].bytes, wire, 32);
    }
    return previous ? previous(display, re, wire) : False;
}

static Bool motion_wire(Display *display, XEvent *re, xEvent *wire) {
    return capture_wire(display, re, wire, prev_motion_wire);
}

static Bool button_press_wire(Display *display, XEvent *re, xEvent *wire) {
    return capture_wire(display, re, wire, prev_button_press_wire);
}

static Bool valuator_wire(Display *display, XEvent *re, xEvent *wire) {
    return capture_wire(display, re, wire, prev_valuator_wire);
}

static const char *window_name(Window window) {
    if (!g_setup || window == None) return "None";
    if (window == g_setup->root) return "ROOT";
    if (window == g_setup->grab) return "W";
    if (window == g_setup->child) return "D";
    if (window == g_setup->outside) return "OUT";
    return "OTHER";
}

static const char *xi2_name(int evtype) {
    switch (evtype) {
    case XI_Motion: return "XI_Motion";
    case XI_ButtonPress: return "XI_ButtonPress";
    case XI_ButtonRelease: return "XI_ButtonRelease";
    default: return "XI_other";
    }
}

static const char *core_name(int type) {
    switch (type) {
    case MotionNotify: return "MotionNotify";
    case ButtonPress: return "ButtonPress";
    case ButtonRelease: return "ButtonRelease";
    case EnterNotify: return "EnterNotify";
    case LeaveNotify: return "LeaveNotify";
    default: return "CoreOther";
    }
}

static int event_xi2_type(Display *dpy) {
    int opcode, event, error;
    if (!XQueryExtension(dpy, "XInputExtension", &opcode, &event, &error)) return -1;
    return opcode;
}

static void print_device_info(Display *dpy, int *master, int *xtest, int *mouse) {
    int n = 0;
    XIDeviceInfo *devices = XIQueryDevice(dpy, XIAllDevices, &n);
    *master = -1;
    *xtest = -1;
    *mouse = -1;
    for (int i = 0; i < n; i++) {
        XIDeviceInfo *dev = &devices[i];
        if (dev->use == XIMasterPointer && *master < 0) *master = dev->deviceid;
        if (dev->use == XISlavePointer && strstr(dev->name, "XTEST pointer"))
            *xtest = dev->deviceid;
        if (dev->use == XISlavePointer && strstr(dev->name, "mouse"))
            *mouse = dev->deviceid;
        printf("DEVICE id=%d use=%d attachment=%d enabled=%d name=\"%s\"\n",
               dev->deviceid, dev->use, dev->attachment, dev->enabled, dev->name);
    }
    XIFreeDeviceInfo(devices);
}

static void init_setup(struct Setup *s, const char *display_name, const char *case_id) {
    memset(s, 0, sizeof(*s));
    s->a = XOpenDisplay(display_name);
    s->b = XOpenDisplay(display_name);
    s->inj = XOpenDisplay(display_name);
    if (!s->a || !s->b || !s->inj) {
        fprintf(stderr, "cannot open display %s\n", display_name);
        exit(2);
    }
    XSetErrorHandler(x_error_handler);
    s->screen = DefaultScreen(s->a);
    s->root = RootWindow(s->a, s->screen);
    s->xi_opcode = event_xi2_type(s->a);
    if (s->xi_opcode < 0) {
        fprintf(stderr, "XInput extension unavailable on %s\n", display_name);
        exit(2);
    }
    print_device_info(s->a, &s->master, &s->xtest_pointer, &s->xvfb_mouse);
    if (s->master < 0) {
        fprintf(stderr, "no master pointer on %s\n", display_name);
        exit(2);
    }
    s->grab = XCreateSimpleWindow(s->a, s->root, 100, 100, 300, 300, 0,
                                  BlackPixel(s->a, s->screen), WhitePixel(s->a, s->screen));
    s->child_owned_by_b = !strcmp(case_id, "C8");
    s->outside_owned_by_b = !strcmp(case_id, "C20") || case_id[0] == 'I';
    if (s->child_owned_by_b) XSync(s->a, False);
    Display *child_owner = s->child_owned_by_b ? s->b : s->a;
    s->child = XCreateSimpleWindow(child_owner, s->grab, 20, 20, 180, 180, 0,
                                   BlackPixel(s->a, s->screen), WhitePixel(s->a, s->screen));
    Display *outside_owner = s->outside_owned_by_b ? s->b : s->a;
    s->outside = XCreateSimpleWindow(outside_owner, s->root, 550, 100, 300, 300, 0,
                                     BlackPixel(s->a, s->screen), WhitePixel(s->a, s->screen));
    XStoreName(s->a, s->grab, "W");
    XStoreName(child_owner, s->child, "D");
    XStoreName(outside_owner, s->outside, "OUT");
    XMapWindow(child_owner, s->child);
    XMapWindow(s->a, s->grab);
    XMapWindow(outside_owner, s->outside);
    XSync(s->a, False);
    XSync(s->b, False);
    g_setup = s;
    Window pointer_root = None, pointer_child = None;
    int root_x = 0, root_y = 0, window_x = 0, window_y = 0;
    unsigned int pointer_mask = 0;
    Bool same_screen = XQueryPointer(s->a, s->root, &pointer_root, &pointer_child,
                                     &root_x, &root_y, &window_x, &window_y,
                                     &pointer_mask);
    printf("CASE %s display=%s master=%d xtest_pointer=%d xvfb_mouse=%d root=0x%lx W=0x%lx D=0x%lx OUT=0x%lx window_owners=W:A,D:%s,OUT:%s\n",
           case_id, display_name, s->master, s->xtest_pointer, s->xvfb_mouse,
           s->root, s->grab, s->child, s->outside,
           s->child_owned_by_b ? "B" : "A",
           s->outside_owned_by_b ? "B" : "A");
    printf("POINTER_INITIAL root=%d,%d window=%s child=%s same_screen=%d mask=0x%x\n",
           root_x, root_y, window_name(pointer_root), window_name(pointer_child),
           same_screen, pointer_mask);
}

static void close_setup(struct Setup *s) {
    if (s->a) XCloseDisplay(s->a);
    if (s->b) XCloseDisplay(s->b);
    if (s->inj) XCloseDisplay(s->inj);
    memset(s, 0, sizeof(*s));
    g_setup = NULL;
}

static void select_xi2(Display *dpy, Window window, int deviceid, unsigned int bits) {
    unsigned char mask[XIMaskLen(XI_LASTEVENT)];
    memset(mask, 0, sizeof(mask));
    if (bits & (1u << XI_Motion)) XISetMask(mask, XI_Motion);
    if (bits & (1u << XI_ButtonPress)) XISetMask(mask, XI_ButtonPress);
    if (bits & (1u << XI_ButtonRelease)) XISetMask(mask, XI_ButtonRelease);
    XIEventMask event_mask = { .deviceid = deviceid, .mask_len = sizeof(mask), .mask = mask };
    XISelectEvents(dpy, window, &event_mask, 1);
    XSync(dpy, False);
    printf("SELECT XI2 client=%s window=%s device=%d events=0x%x\n",
           dpy == g_setup->a ? "A" : "B", window_name(window), deviceid, bits);
}

static void select_core(Display *dpy, Window window, long mask) {
    XSelectInput(dpy, window, mask);
    XSync(dpy, False);
    printf("SELECT core client=%s window=%s mask=0x%lx\n",
           dpy == g_setup->a ? "A" : "B", window_name(window), mask);
}

static void grab_xi2(struct Setup *s, Window window, int deviceid, Bool owner_events,
                     unsigned int bits) {
    unsigned char mask[XIMaskLen(XI_LASTEVENT)];
    memset(mask, 0, sizeof(mask));
    if (bits & (1u << XI_Motion)) XISetMask(mask, XI_Motion);
    if (bits & (1u << XI_ButtonPress)) XISetMask(mask, XI_ButtonPress);
    if (bits & (1u << XI_ButtonRelease)) XISetMask(mask, XI_ButtonRelease);
    XIEventMask event_mask = { .deviceid = deviceid, .mask_len = sizeof(mask), .mask = mask };
    int status = XIGrabDevice(s->a, deviceid, window, CurrentTime, None,
                              GrabModeAsync, GrabModeAsync, owner_events, &event_mask);
    XSync(s->a, False);
    printf("GRAB XIGrabDevice client=A window=%s device=%d owner_events=%d mask=0x%x status=%d\n",
           window_name(window), deviceid, owner_events, bits, status);
}

static void grab_core(struct Setup *s, Window window, Bool owner_events, unsigned int mask) {
    int status = XGrabPointer(s->a, window, owner_events, mask,
                              GrabModeAsync, GrabModeAsync, None, None, CurrentTime);
    XSync(s->a, False);
    printf("GRAB XGrabPointer client=A window=%s owner_events=%d mask=0x%x status=%d\n",
           window_name(window), owner_events, mask, status);
}

static void fake_motion(struct Setup *s, int x, int y) {
    XTestFakeMotionEvent(s->inj, s->screen, x, y, 0);
    XSync(s->inj, False);
    printf("INJECT XTestFakeMotionEvent source=Virtual-core-XTEST-pointer root=%d,%d\n", x, y);
}

static void fake_device_motion(struct Setup *s, int first_axis, int *axes, int count) {
    int xtest_event_base, xtest_error_base, xtest_opcode;
    int xi_opcode, xi_event_base, xi_error_base;
    if (!XQueryExtension(s->inj, "XTEST", &xtest_opcode,
                         &xtest_event_base, &xtest_error_base) ||
        !XQueryExtension(s->inj, "XInputExtension", &xi_opcode,
                         &xi_event_base, &xi_error_base)) {
        fprintf(stderr, "XTest or XInput extension unavailable\n");
        exit(2);
    }
    unsigned char events[64] = {0};
    events[0] = (unsigned char)(xi_event_base + XI_DeviceMotionNotify);
    events[31] = (unsigned char)s->xtest_pointer | 0x80;
    events[32] = (unsigned char)(xi_event_base + XI_DeviceValuator);
    events[33] = (unsigned char)s->xtest_pointer;
    events[38] = (unsigned char)count;
    events[39] = (unsigned char)first_axis;
    for (int i = 0; i < count; i++)
        memcpy(events + 40 + i * 4, &axes[i], sizeof(axes[i]));

    LockDisplay(s->inj);
    unsigned char *request = _XGetRequest(s->inj, (CARD8)xtest_opcode, 68);
    request[1] = 2; /* XTestFakeInput */
    memcpy(request + 4, events, sizeof(events));
    UnlockDisplay(s->inj);
    XSync(s->inj, False);
    printf("INJECT XTestFakeDeviceMotionEvent device=%d first_axis=%d count=%d", s->xtest_pointer, first_axis, count);
    for (int i = 0; i < count; i++) printf(" axis%d=%d", first_axis + i, axes[i]);
    printf("\n");
}

static void fake_button(struct Setup *s, Bool press, unsigned int button) {
    XTestFakeButtonEvent(s->inj, button, press, 0);
    XSync(s->inj, False);
    printf("INJECT XTestFakeButtonEvent source=Virtual-core-XTEST-pointer type=%s button=%u\n",
           press ? "ButtonPress" : "ButtonRelease", button);
}

static void print_xi2_event(const char *client, XGenericEventCookie *cookie) {
    if (cookie->evtype == XI_Enter || cookie->evtype == XI_Leave) {
        XIEnterEvent *ev = (XIEnterEvent *)cookie->data;
        printf("EV client=%s type=%s evtype=%d deviceid=%d sourceid=%d event=%s child=%s event_x=%.3f event_y=%.3f root_x=%.3f root_y=%.3f detail=%d mode=%d focus=%d same_screen=%d\n",
               client, cookie->evtype == XI_Enter ? "XI_Enter" : "XI_Leave",
               cookie->evtype, ev->deviceid, ev->sourceid,
               window_name(ev->event), window_name(ev->child),
               ev->event_x, ev->event_y, ev->root_x, ev->root_y,
               ev->detail, ev->mode, ev->focus, ev->same_screen);
        return;
    }
    XIDeviceEvent *ev = (XIDeviceEvent *)cookie->data;
    printf("EV client=%s type=%s evtype=%d deviceid=%d sourceid=%d event=%s child=%s event_x=%.3f event_y=%.3f root_x=%.3f root_y=%.3f detail=%d\n",
           client, xi2_name(cookie->evtype), cookie->evtype, ev->deviceid,
           ev->sourceid, window_name(ev->event), window_name(ev->child),
           ev->event_x, ev->event_y, ev->root_x, ev->root_y, ev->detail);
    if (cookie->evtype == XI_Motion) {
        unsigned int value_index = 0;
        uint64_t value_mask = 0;
        for (int axis = 0; axis < ev->valuators.mask_len * 8; axis++) {
            if (XIMaskIsSet(ev->valuators.mask, axis)) {
                if (axis < 64) value_mask |= UINT64_C(1) << axis;
                value_index++;
            }
        }
        printf("VALUATORS client=%s mask=0x%llx values=", client,
               (unsigned long long)value_mask);
        value_index = 0;
        for (int axis = 0; axis < ev->valuators.mask_len * 8; axis++) {
            if (XIMaskIsSet(ev->valuators.mask, axis)) {
                printf("%s%d:%.3f", value_index ? "," : "", axis, ev->valuators.values[value_index]);
                value_index++;
            }
        }
        printf("\n");
    }
}

static void print_core_event(const char *client, XEvent *event) {
    if (event->type == MotionNotify) {
        XMotionEvent *ev = &event->xmotion;
        printf("EV client=%s type=%s event=%s child=%s event_x=%d event_y=%d root_x=%d root_y=%d state=0x%x\n",
               client, core_name(event->type), window_name(ev->window),
               window_name(ev->subwindow), ev->x, ev->y, ev->x_root, ev->y_root, ev->state);
    } else if (event->type == ButtonPress || event->type == ButtonRelease) {
        XButtonEvent *ev = &event->xbutton;
        printf("EV client=%s type=%s event=%s child=%s event_x=%d event_y=%d root_x=%d root_y=%d button=%u state=0x%x\n",
               client, core_name(event->type), window_name(ev->window),
               window_name(ev->subwindow), ev->x, ev->y, ev->x_root, ev->y_root,
               ev->button, ev->state);
    }
}

static void print_xi1_event(struct Setup *s, const char *client, XEvent *event) {
    if (event->type == s->xi1_motion_type) {
        XDeviceMotionEvent *ev = (XDeviceMotionEvent *)event;
        printf("EV client=%s type=DeviceMotionNotify deviceid=%lu event=%s child=%s event_x=%d event_y=%d root_x=%d root_y=%d axes_count=%u first_axis=%u axis0=%d axis1=%d\n",
               client, ev->deviceid, window_name(ev->window), window_name(ev->subwindow),
               ev->x, ev->y, ev->x_root, ev->y_root, ev->axes_count,
               ev->first_axis, ev->axis_data[0], ev->axis_data[1]);
    } else if (event->type == s->xi1_button_press_type) {
        XDeviceButtonEvent *ev = (XDeviceButtonEvent *)event;
        printf("EV client=%s type=DeviceButtonPress deviceid=%lu event=%s child=%s event_x=%d event_y=%d root_x=%d root_y=%d button=%d\n",
               client, ev->deviceid, window_name(ev->window), window_name(ev->subwindow),
               ev->x, ev->y, ev->x_root, ev->y_root, ev->button);
    } else if (s->xi1_button_release_type && event->type == s->xi1_button_release_type) {
        XDeviceButtonEvent *ev = (XDeviceButtonEvent *)event;
        printf("EV client=%s type=DeviceButtonRelease deviceid=%lu event=%s child=%s event_x=%d event_y=%d root_x=%d root_y=%d button=%d\n",
               client, ev->deviceid, window_name(ev->window), window_name(ev->subwindow),
               ev->x, ev->y, ev->x_root, ev->y_root, ev->button);
    } else if (event->type == s->xi1_valuator_type) {
        printf("EV client=%s type=DeviceValuator raw_extension_event\n", client);
    }
}

static int drain_display(struct Setup *s, Display *dpy, const char *client) {
    int count = 0;
    while (XPending(dpy) > 0) {
        XEvent event;
        XNextEvent(dpy, &event);
        count++;
        if (event.type == GenericEvent && event.xcookie.extension == s->xi_opcode) {
            if (XGetEventData(dpy, &event.xcookie)) {
                print_xi2_event(client, &event.xcookie);
                XFreeEventData(dpy, &event.xcookie);
            }
        } else if (event.type == MotionNotify || event.type == ButtonPress || event.type == ButtonRelease) {
            print_core_event(client, &event);
        } else if (event.type == s->xi1_motion_type ||
                   event.type == s->xi1_button_press_type ||
                   (s->xi1_button_release_type && event.type == s->xi1_button_release_type) ||
                   event.type == s->xi1_valuator_type) {
            print_xi1_event(s, client, &event);
        }
    }
    return count;
}

static void drain_all_for(struct Setup *s, int milliseconds) {
    struct timeval start, now;
    gettimeofday(&start, NULL);
    int elapsed;
    do {
        int got = 0;
        got += drain_display(s, s->a, "A");
        got += drain_display(s, s->b, "B");
        gettimeofday(&now, NULL);
        elapsed = (int)((now.tv_sec - start.tv_sec) * 1000 + (now.tv_usec - start.tv_usec) / 1000);
        if (!got) usleep(1000);
    } while (elapsed < milliseconds);
}

static void inject_motion_and_drain(struct Setup *s, int x, int y) {
    fake_motion(s, x, y);
    drain_all_for(s, 100);
}

static void inject_click_and_drain(struct Setup *s, int x, int y) {
    fake_motion(s, x, y);
    drain_all_for(s, 50);
    fake_button(s, True, 1);
    drain_all_for(s, 75);
    fake_button(s, False, 1);
    drain_all_for(s, 100);
}

static void raw_xi1_begin(struct Setup *s, Display *dpy) {
    XDevice *device = XOpenDevice(dpy, (XID)s->xtest_pointer);
    if (!device) {
        fprintf(stderr, "cannot XOpenDevice for XTEST pointer %d\n", s->xtest_pointer);
        exit(2);
    }
    int motion_class, press_class, motion_type, press_type;
    DeviceMotionNotify(device, motion_type, motion_class);
    DeviceButtonPress(device, press_type, press_class);
    XEventClass classes[2] = { motion_class, press_class };
    XSelectExtensionEvent(dpy, s->grab, classes, 2);
    XSync(dpy, False);
    int opcode, error;
    if (!XQueryExtension(dpy, INAME, &opcode, &s->xi_event_base, &error)) {
        fprintf(stderr, "XInputExtension missing for XI1 capture\n");
        exit(2);
    }
    s->xi1_motion_type = motion_type;
    s->xi1_button_press_type = press_type;
    s->xi1_valuator_type = s->xi_event_base + XI_DeviceValuator;
    prev_motion_wire = XESetWireToEvent(dpy, motion_type, motion_wire);
    prev_button_press_wire = XESetWireToEvent(dpy, press_type, button_press_wire);
    prev_valuator_wire = XESetWireToEvent(dpy, s->xi1_valuator_type, valuator_wire);
    raw_count = 0;
    raw_capture_enabled = 1;
    printf("SELECT XI1 client=%s window=W device=%d events=DeviceMotionNotify|DeviceButtonPress types=%d,%d valuator_type=%d wire_hooks=DeviceMotionNotify|DeviceButtonPress|DeviceValuator\n",
           dpy == g_setup->a ? "A" : "B", s->xtest_pointer, motion_type,
           press_type, s->xi1_valuator_type);
}

static const char *raw_xi1_name(struct Setup *s, unsigned int wire_type) {
    if ((int)wire_type == s->xi1_motion_type) return "DeviceMotionNotify";
    if ((int)wire_type == s->xi1_button_press_type) return "DeviceButtonPress";
    if ((int)wire_type == s->xi1_valuator_type) return "DeviceValuator";
    return "other";
}

static void packet_hex(const unsigned char *bytes, char hex[65]) {
    for (int i = 0; i < 32; i++) snprintf(hex + (i * 2), 3, "%02x", bytes[i]);
    hex[64] = '\0';
}

static void raw_xi1_dump(struct Setup *s) {
    raw_capture_enabled = 0;
    for (int i = 0; i < raw_count; i++) {
        unsigned char *b = raw_events[i].bytes;
        unsigned int wire_type = b[0] & 0x7f;
        char hex[65];
        packet_hex(b, hex);
        printf("RAWXI1 index=%d event=%s type=%u hex=%s",
               i, raw_xi1_name(s, wire_type), wire_type, hex);
        if ((int)wire_type == s->xi1_motion_type ||
            (int)wire_type == s->xi1_button_press_type) {
            printf(" byte31=0x%02x deviceid=%u MORE_EVENTS=%u",
                   b[31], b[31] & 0x7f, !!(b[31] & MORE_EVENTS));
            if (i + 1 < raw_count) {
                unsigned char *next = raw_events[i + 1].bytes;
                unsigned int next_type = next[0] & 0x7f;
                char next_hex[65];
                packet_hex(next, next_hex);
                printf(" followup_index=%d followup_event=%s followup_hex=%s",
                       i + 1, raw_xi1_name(s, next_type), next_hex);
                if ((int)next_type == s->xi1_valuator_type) {
                    unsigned int count = next[6] > 6 ? 6 : next[6];
                    printf(" followup_byte1=0x%02x followup_deviceid=%u followup_MORE_EVENTS=%u valuators=",
                           next[1], next[1] & 0x7f, !!(next[1] & MORE_EVENTS));
                    for (unsigned int axis = 0; axis < count; axis++) {
                        int32_t value;
                        memcpy(&value, next + 8 + (axis * sizeof(value)), sizeof(value));
                        printf("%s%d:%d", axis ? "," : "", next[7] + (int)axis, value);
                    }
                }
            } else {
                printf(" followup=none");
            }
        } else if ((int)wire_type == s->xi1_valuator_type) {
            unsigned int count = b[6] > 6 ? 6 : b[6];
            printf(" byte1=0x%02x deviceid=%u MORE_EVENTS=%u valuators=",
                   b[1], b[1] & 0x7f, !!(b[1] & MORE_EVENTS));
            for (unsigned int axis = 0; axis < count; axis++) {
                int32_t value;
                memcpy(&value, b + 8 + (axis * sizeof(value)), sizeof(value));
                printf("%s%d:%d", axis ? "," : "", b[7] + axis, value);
            }
        }
        printf("\n");
    }
    printf("RAWXI1 total_extension_wire_events=%d\n", raw_count);
}


static void select_xi1(struct Setup *s, Display *dpy, Window window) {
    XDevice *device = XOpenDevice(dpy, (XID)s->xtest_pointer);
    if (!device) { fprintf(stderr, "cannot XOpenDevice %d\n", s->xtest_pointer); exit(2); }
    int mc, pc, rc, mt, pt, rt;
    DeviceMotionNotify(device, mt, mc);
    DeviceButtonPress(device, pt, pc);
    DeviceButtonRelease(device, rt, rc);
    XEventClass classes[3] = { mc, pc, rc };
    XSelectExtensionEvent(dpy, window, classes, 3);
    XSync(dpy, False);
    int opcode, error;
    XQueryExtension(dpy, INAME, &opcode, &s->xi_event_base, &error);
    s->xi1_motion_type = mt;
    s->xi1_button_press_type = pt;
    s->xi1_button_release_type = rt;
    s->xi1_valuator_type = s->xi_event_base + XI_DeviceValuator;
    printf("SELECT XI1 client=%s window=%s device=%d events=DeviceMotionNotify|DeviceButtonPress|DeviceButtonRelease\n",
           dpy == s->a ? "A" : "B", window_name(window), s->xtest_pointer);
}

static int grab_xi2_on(struct Setup *s, Display *dpy, Window window, int deviceid, unsigned int bits) {
    unsigned char mask[XIMaskLen(XI_LASTEVENT)];
    memset(mask, 0, sizeof(mask));
    if (bits & (1u << XI_Motion)) XISetMask(mask, XI_Motion);
    if (bits & (1u << XI_ButtonPress)) XISetMask(mask, XI_ButtonPress);
    if (bits & (1u << XI_ButtonRelease)) XISetMask(mask, XI_ButtonRelease);
    XIEventMask event_mask = { .deviceid = deviceid, .mask_len = sizeof(mask), .mask = mask };
    int status = XIGrabDevice(dpy, deviceid, window, CurrentTime, None,
                              GrabModeAsync, GrabModeAsync, False, &event_mask);
    XSync(dpy, False);
    printf("GRAB XIGrabDevice client=%s window=%s device=%d owner_events=0 mask=0x%x status=%d\n",
           dpy == s->a ? "A" : "B", window_name(window), deviceid, bits, status);
    return status;
}

static void press_at(struct Setup *s, unsigned int button) { fake_button(s, True, button); drain_all_for(s, 100); }
static void release_b(struct Setup *s, unsigned int button) { fake_button(s, False, button); drain_all_for(s, 125); }
static void move_to(struct Setup *s, int x, int y) { fake_motion(s, x, y); drain_all_for(s, 100); }

/* Press in W (over D), drag out to OUT (owned by B), release there. */
static void drag_out(struct Setup *s) {
    move_to(s, 150, 150);
    press_at(s, 1);
    move_to(s, 650, 150);
    release_b(s, 1);
    move_to(s, 660, 160);
}

static void run_case(struct Setup *s, const char *case_id) {
    const unsigned int motion = 1u << XI_Motion;
    const unsigned int buttons = (1u << XI_ButtonPress) | (1u << XI_ButtonRelease);
    if (!strcmp(case_id, "C1")) {
        select_xi2(s->b, s->grab, s->master, XI_GRAB_BITS);
        grab_xi2(s, s->grab, s->master, True, XI_GRAB_BITS);
        inject_motion_and_drain(s, 150, 150);
    } else if (!strcmp(case_id, "C2")) {
        select_xi2(s->b, s->child, s->master, XI_GRAB_BITS);
        grab_xi2(s, s->grab, s->master, True, XI_GRAB_BITS);
        inject_motion_and_drain(s, 150, 150);
    } else if (!strcmp(case_id, "C3")) {
        select_xi2(s->b, s->outside, s->master, XI_GRAB_BITS);
        grab_xi2(s, s->grab, s->master, True, XI_GRAB_BITS);
        inject_motion_and_drain(s, 650, 150);
    } else if (!strcmp(case_id, "C4")) {
        select_xi2(s->b, s->grab, s->master, XI_GRAB_BITS);
        select_core(s->a, s->grab, PointerMotionMask);
        grab_xi2(s, s->grab, s->master, True, XI_GRAB_BITS);
        inject_motion_and_drain(s, 150, 150);
    } else if (!strcmp(case_id, "C4b")) {
        select_xi2(s->b, s->child, s->master, XI_GRAB_BITS);
        select_xi2(s->a, s->child, s->master, XI_GRAB_BITS);
        select_core(s->a, s->child, PointerMotionMask);
        grab_xi2(s, s->grab, s->master, True, XI_GRAB_BITS);
        inject_motion_and_drain(s, 150, 150);
    } else if (!strcmp(case_id, "C5")) {
        if (s->xtest_pointer < 0) {
            puts("BLOCKED no XTest pointer device");
            return;
        }
        select_xi2(s->b, s->grab, s->xtest_pointer, motion);
        grab_xi2(s, s->grab, s->master, True, XI_GRAB_BITS);
        inject_motion_and_drain(s, 150, 150);
    } else if (!strcmp(case_id, "C6")) {
        select_xi2(s->b, s->child, s->master, motion);
        select_core(s->a, s->grab, PointerMotionMask);
        grab_core(s, s->outside, True, CORE_POINTER);
        inject_motion_and_drain(s, 150, 150);
    } else if (!strcmp(case_id, "C7")) {
        select_xi2(s->b, s->grab, s->master, XI_GRAB_BITS);
        select_core(s->a, s->grab, PointerMotionMask);
        grab_core(s, s->grab, True, CORE_POINTER);
        inject_motion_and_drain(s, 150, 150);
    } else if (!strcmp(case_id, "C7s")) {
        if (s->xtest_pointer < 0) {
            puts("BLOCKED no XTest pointer device");
            return;
        }
        select_xi2(s->b, s->child, s->xtest_pointer, motion);
        select_core(s->a, s->grab, PointerMotionMask);
        grab_core(s, s->outside, True, CORE_POINTER);
        inject_motion_and_drain(s, 150, 150);
    } else if (!strcmp(case_id, "C8")) {
        puts("SCENARIO C8a all-master XI2 implicit-grab selector");
        select_xi2(s->a, s->grab, XI2_ALL_MASTER, buttons);
        fake_motion(s, 350, 350);
        drain_all_for(s, 50);
        fake_button(s, True, 1);
        drain_all_for(s, 75);
        fake_button(s, False, 1);
        drain_all_for(s, 100);
        puts("SCENARIO C8b XI2 leaf plus core ancestor selection");
        select_xi2(s->a, s->grab, XI2_ALL_MASTER, 0);
        select_core(s->a, s->grab, CORE_BUTTONS);
        select_xi2(s->b, s->child, XI2_ALL_MASTER, buttons);
        fake_motion(s, 150, 150);
        drain_all_for(s, 50);
        fake_button(s, True, 1);
        drain_all_for(s, 75);
        fake_button(s, False, 1);
        drain_all_for(s, 100);
    } else if (!strcmp(case_id, "C8c")) {
        select_core(s->a, s->grab, CORE_BUTTONS | PointerMotionMask);
        select_xi2(s->b, s->child, s->master, motion);
        fake_motion(s, 150, 150);
        drain_all_for(s, 100);
        fake_button(s, True, 1);
        drain_all_for(s, 75);
        fake_motion(s, 160, 160);
        drain_all_for(s, 100);
        fake_button(s, False, 1);
        drain_all_for(s, 100);
    } else if (!strcmp(case_id, "C9")) {
        if (s->xtest_pointer < 0) {
            puts("BLOCKED no XTest pointer device");
            return;
        }
        select_xi2(s->a, s->grab, s->xtest_pointer, buttons);
        select_xi2(s->a, s->grab, s->master, buttons);
        fake_motion(s, 150, 150);
        drain_all_for(s, 50);
        fake_button(s, True, 1);
        drain_all_for(s, 100);
        fake_button(s, False, 1);
        drain_all_for(s, 125);
    } else if (!strcmp(case_id, "C9e")) {
        if (s->xtest_pointer < 0) {
            puts("BLOCKED no XTest pointer device");
            return;
        }
        puts("SCENARIO C9e E27 core plus slave XI2 on one client; no master XI2 selection");
        select_core(s->a, s->grab, CORE_BUTTONS);
        select_xi2(s->a, s->grab, s->xtest_pointer, buttons);
        fake_motion(s, 150, 150);
        drain_all_for(s, 50);
        fake_button(s, True, 1);
        drain_all_for(s, 100);
        fake_button(s, False, 1);
        drain_all_for(s, 125);
    } else if (!strcmp(case_id, "C10")) {
        if (s->xtest_pointer < 0) {
            puts("BLOCKED no XTest pointer device");
            return;
        }
        raw_xi1_begin(s, s->b);
        fake_motion(s, 150, 150);
        drain_all_for(s, 100);
        fake_motion(s, 155, 155);
        drain_all_for(s, 100);
        fake_button(s, True, 1);
        drain_all_for(s, 100);
        fake_button(s, False, 1);
        drain_all_for(s, 100);
        raw_xi1_dump(s);
    } else if (!strcmp(case_id, "C11")) {
        select_xi2(s->b, s->grab, s->master, XI_GRAB_BITS);
        grab_xi2(s, s->grab, s->master, True, XI_GRAB_BITS);
        inject_motion_and_drain(s, 150, 150);
        XIUngrabDevice(s->a, s->master, CurrentTime);
        XSync(s->a, False);
        puts("UNGRAB XIUngrabDevice client=A device=master-pointer");
        inject_motion_and_drain(s, 160, 160);
    } else if (!strcmp(case_id, "C12")) {
        grab_xi2(s, s->grab, s->master, True, XI_GRAB_BITS);
        inject_motion_and_drain(s, 650, 150);
    } else if (!strcmp(case_id, "C13")) {
        select_xi2(s->b, s->child, s->master, buttons);
        grab_xi2(s, s->grab, s->master, True, XI_GRAB_BITS);
        inject_click_and_drain(s, 150, 150);
    } else if (!strcmp(case_id, "C14")) {
        select_core(s->a, s->outside, CORE_BUTTONS);
        grab_core(s, s->grab, True, CORE_POINTER);
        inject_click_and_drain(s, 650, 150);
    } else if (!strcmp(case_id, "C15")) {
        if (s->xvfb_mouse < 0) {
            puts("BLOCKED no Xvfb mouse slave device");
            return;
        }
        select_xi2(s->b, s->child, s->master, motion);
        grab_xi2(s, s->grab, s->xvfb_mouse, True, XI_GRAB_BITS);
        inject_motion_and_drain(s, 150, 150);
    } else if (!strcmp(case_id, "C16")) {
        if (s->xtest_pointer < 0) {
            puts("BLOCKED no XTest pointer device");
            return;
        }
        fake_motion(s, 150, 150);
        drain_all_for(s, 50);
        raw_xi1_begin(s, s->a);
        select_xi2(s->a, s->grab, s->xtest_pointer, motion);
        grab_xi2(s, s->grab, s->xtest_pointer, True, motion);
        int axes[2] = {150, 150};
        fake_device_motion(s, 0, axes, 2);
        drain_all_for(s, 100);
        raw_xi1_dump(s);
    } else if (!strcmp(case_id, "C17")) {
        if (s->xtest_pointer < 0) {
            puts("BLOCKED no XTest pointer device");
            return;
        }
        fake_motion(s, 150, 150);
        drain_all_for(s, 50);
        raw_xi1_begin(s, s->a);
        grab_xi2(s, s->grab, s->xtest_pointer, False, motion);
        int axes[2] = {150, 150};
        fake_device_motion(s, 0, axes, 2);
        drain_all_for(s, 100);
        raw_xi1_dump(s);
    } else if (!strcmp(case_id, "C18")) {
        if (s->xtest_pointer < 0) {
            puts("BLOCKED no XTest pointer device");
            return;
        }
        fake_motion(s, 150, 150);
        drain_all_for(s, 50);
        raw_xi1_begin(s, s->a);
        select_xi2(s->a, s->grab, s->xtest_pointer, motion);
        grab_xi2(s, s->grab, s->xtest_pointer, True, motion);
        int axis_value = 175;
        fake_device_motion(s, 1, &axis_value, 1);
        drain_all_for(s, 100);
        raw_xi1_dump(s);
    } else if (!strcmp(case_id, "C19")) {
        if (s->xtest_pointer < 0) {
            puts("BLOCKED no XTest pointer device");
            return;
        }
        unsigned char mask[XIMaskLen(XI_LASTEVENT)];
        memset(mask, 0, sizeof(mask));
        XISetMask(mask, XI_ButtonPress);
        XISetMask(mask, XI_ButtonRelease);
        XIEventMask event_mask = {
            .deviceid = s->xtest_pointer,
            .mask_len = sizeof(mask),
            .mask = mask,
        };
        XIGrabModifiers modifiers = { .modifiers = XIAnyModifier };
        int status = XIGrabButton(s->a, s->xtest_pointer, 1, s->grab, None,
                                  GrabModeSync, GrabModeAsync, False,
                                  &event_mask, 1, &modifiers);
        XSync(s->a, False);
        printf("GRAB XIGrabButton client=A window=W device=%d button=1 owner_events=0 mode=sync mask=ButtonPress|ButtonRelease status=%d modifier_status=%d\n",
               s->xtest_pointer, status, modifiers.status);
        select_xi2(s->b, s->child, s->xtest_pointer, buttons);
        fake_button(s, True, 1);
        drain_all_for(s, 75);
        fake_button(s, False, 1);
        drain_all_for(s, 75);
        XIAllowEvents(s->a, s->xtest_pointer, XIReplayDevice, CurrentTime);
        XSync(s->a, False);
        puts("ALLOW XIAllowEvents client=A device=XTEST-pointer mode=ReplayDevice");
        drain_all_for(s, 125);
    } else if (!strcmp(case_id, "C20")) {
        if (s->xtest_pointer < 0) {
            puts("BLOCKED no XTest pointer device");
            return;
        }
        fake_motion(s, 650, 150);
        drain_all_for(s, 75);
        select_xi2(s->b, s->outside, s->xtest_pointer, buttons);
        grab_xi2(s, s->grab, s->master, True, buttons);
        fake_button(s, True, 1);
        drain_all_for(s, 75);
        fake_button(s, False, 1);
        drain_all_for(s, 100);
    } else if (!strcmp(case_id, "I1")) {
        puts("SCENARIO I1 A slave+master XI2 on W; B slave+master XI2 on OUT; drag out");
        select_xi2(s->a, s->grab, s->xtest_pointer, XI_GRAB_BITS);
        select_xi2(s->a, s->grab, s->master, XI_GRAB_BITS);
        select_xi2(s->b, s->outside, s->xtest_pointer, XI_GRAB_BITS);
        select_xi2(s->b, s->outside, s->master, XI_GRAB_BITS);
        drag_out(s);
    } else if (!strcmp(case_id, "I1a")) {
        puts("SCENARIO I1a Qt6 shape: A XIAllDevices on W; B XIAllDevices on OUT; drag out");
        select_xi2(s->a, s->grab, XI2_ALL_DEVICES, XI_GRAB_BITS);
        select_xi2(s->b, s->outside, XI2_ALL_DEVICES, XI_GRAB_BITS);
        drag_out(s);
    } else if (!strcmp(case_id, "I2")) {
        puts("SCENARIO I2 E27 shape: A slave-only XI2 on W; B master XI2 on OUT; drag out");
        select_xi2(s->a, s->grab, s->xtest_pointer, XI_GRAB_BITS);
        select_xi2(s->b, s->outside, s->master, XI_GRAB_BITS);
        drag_out(s);
    } else if (!strcmp(case_id, "I3")) {
        puts("SCENARIO I3 A slave-only XI2 on W; B core on OUT; drag out");
        select_xi2(s->a, s->grab, s->xtest_pointer, XI_GRAB_BITS);
        select_core(s->b, s->outside, CORE_POINTER);
        drag_out(s);
    } else if (!strcmp(case_id, "I4")) {
        puts("SCENARIO I4 A slave-only XI2 on W; mid-drag B XIGrabDevice slave then master on OUT");
        select_xi2(s->a, s->grab, s->xtest_pointer, XI_GRAB_BITS);
        select_xi2(s->b, s->outside, s->xtest_pointer, XI_GRAB_BITS);
        move_to(s, 150, 150);
        press_at(s, 1);
        move_to(s, 650, 150);
        grab_xi2_on(s, s->b, s->outside, s->xtest_pointer, XI_GRAB_BITS);
        grab_xi2_on(s, s->b, s->outside, s->master, XI_GRAB_BITS);
        release_b(s, 1);
        move_to(s, 660, 160);
    } else if (!strcmp(case_id, "I5")) {
        puts("SCENARIO I5 XI1: A DeviceMotion/Press/Release dev4 on W; B same on OUT; drag out");
        select_xi1(s, s->a, s->grab);
        select_xi1(s, s->b, s->outside);
        drag_out(s);
    } else if (!strcmp(case_id, "I6")) {
        puts("SCENARIO I6 A slave-only XI2 on W; B slave XI2 on OUT; A unmaps W mid-drag");
        select_xi2(s->a, s->grab, s->xtest_pointer, XI_GRAB_BITS);
        select_xi2(s->b, s->outside, s->xtest_pointer, XI_GRAB_BITS);
        move_to(s, 150, 150);
        press_at(s, 1);
        move_to(s, 650, 150);
        XUnmapWindow(s->a, s->grab);
        XSync(s->a, False);
        puts("REQUEST XUnmapWindow client=A window=W");
        drain_all_for(s, 75);
        release_b(s, 1);
        move_to(s, 660, 160);
    } else if (!strcmp(case_id, "I7")) {
        puts("SCENARIO I7 A slave-only XI2 on W; B slave XI2 on OUT; press 1 in W, press 3 in OUT, release 1, release 3");
        select_xi2(s->a, s->grab, s->xtest_pointer, XI_GRAB_BITS);
        select_xi2(s->b, s->outside, s->xtest_pointer, XI_GRAB_BITS);
        move_to(s, 150, 150);
        press_at(s, 1);
        move_to(s, 650, 150);
        press_at(s, 3);
        release_b(s, 1);
        move_to(s, 655, 155);
        release_b(s, 3);
        move_to(s, 660, 160);
    } else if (!strcmp(case_id, "I8")) {
        puts("SCENARIO I8 A slave-only XI2 on W; A XIUngrabDevice slave mid-drag; B slave XI2 on OUT");
        select_xi2(s->a, s->grab, s->xtest_pointer, XI_GRAB_BITS);
        select_xi2(s->b, s->outside, s->xtest_pointer, XI_GRAB_BITS);
        move_to(s, 150, 150);
        press_at(s, 1);
        move_to(s, 650, 150);
        XIUngrabDevice(s->a, s->xtest_pointer, CurrentTime);
        XSync(s->a, False);
        puts("REQUEST XIUngrabDevice client=A device=XTEST-pointer");
        drain_all_for(s, 75);
        release_b(s, 1);
        move_to(s, 660, 160);
    } else {
        fprintf(stderr, "unknown case id: %s\n", case_id);
        exit(2);
    }
}

int main(int argc, char **argv) {
    if (argc != 3) {
        fprintf(stderr, "usage: %s DISPLAY CASE-ID\n", argv[0]);
        return 2;
    }
    struct Setup setup;
    init_setup(&setup, argv[1], argv[2]);
    puts("PREPOSITION XTestFakeMotionEvent root=145,145 before case selections/grabs");
    fake_motion(&setup, 145, 145);
    drain_all_for(&setup, 50);
    run_case(&setup, argv[2]);
    fflush(stdout);
    close_setup(&setup);
    return 0;
}
```
