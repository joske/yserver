# Passive grab activation and replay: measurements against Xvfb 21.1.24

Oracle: `/data/Projects/xorg-21.1.24` (tag `xorg-server-21.1.24`), cited as `../xserver`. Xvfb ran on `:97` (master pointer 2, XTest pointer 4). yserver ran on `:96` with `--headless` and lavapipe, built from `68447c8f` (the tree of PR #374, head `d479e443` after its rebase).

**Setup.** As in `2026-11-19-per-device-implicit-grabs-measurements.md`: connections A, B and an injector; W at root `(100,100)` 300x300 owned by A; child D at W-relative `(20,20)`; OUT at root `(550,100)`. In every P case D and OUT are owned by B. The injector moves the XTest pointer to `(145,145)` first; each case moves it over D at `(150,150)` before pressing button 1.

**Comparison.** Recipient, event type, device/source and event window, plus grab statuses. Crossings and coordinates are excluded, as in the earlier measurement docs.

**Fresh server per case.** Each case runs on its own freshly started Xvfb and yserver, so every case starts with no button held (`POINTER_INITIAL ... mask=0x0` on both). A first run of all cases on one yserver instance left the master button logically held after P1, and P1m then started with `mask=0x100`; that run is discarded.

| case | setup | Xvfb 21.1.24 | yserver (#374) |
|---|---|---|---|
| P1 | A: async XI2 passive grab (XIGrabButton, button 1, XIAnyModifier) on W for slave 4, owner_events=1, Motion/Press/Release. B selects slave 4 and master 2 on D. Press, move, release over D. | A gets press d4, motion d4 and release d4 on W. B gets no press or release: the activation detached slave 4, so no master event exists. | Same, plus B gets **XI_ButtonPress d2** on D. |
| P1m | As P1, grab on master 2. | A gets XI_ButtonPress d2 on W, then motion/release d2 on W. B gets the slave forms d4 on D. | A gets **core ButtonPress and XI_ButtonPress d2** for the one activation; the rest matches. |
| P2 | A: sync XI2 passive grab on W for slave 4, owner_events=0, mask **ButtonRelease only**. B selects slave 4 and master 2 on D. Press; XIAllowEvents AsyncDevice; release. | A gets press d4 (outside its mask) and release d4 on W. | Same. |
| P2r | As P2, then ReplayDevice instead of AsyncDevice. | A gets press d4 on W; the replay gives B press d4 and release d4 on D. | Same, plus B gets **press d2 and release d2** on D. |
| P2m | As P2 with the grab on master 2. | A gets XI_ButtonPress d2 and XI_ButtonRelease d2 on W; B gets d4 press/release on D. | A gets **core ButtonPress and XI_ButtonPress d2** for the one activation. |
| P3 (C19 gate) | A: core sync GrabButton (AnyButton, AnyModifier) on W. B selects slave 4 only on D. Press; XAllowEvents ReplayPointer; move to OUT; release. | A gets core ButtonPress on W. B gets press d4 on D once, **before** the replay (the slave is processed first and the core grab lives on the master); the replay replays only the master event. B gets motion d4 and release d4 on D. | B gets **press d4 twice**: at activation and again from the replay. |
| P4 (xfwm4) | A: core sync GrabButton (AnyButton, AnyModifier) on D, **then** XI2 sync XIGrabButton (master 2, XIAnyButton, XIAnyModifier, Press/Release) on D; B selects master 2 Press/Release on D. Press; one XAllowEvents ReplayPointer; release. | A gets only **XI_ButtonPress d2** (the newer grab wins). B gets press d2 and release d2 on D. | A gets **core ButtonPress and XI_ButtonPress d2**. |
| P4b | As P4 with the XI2 grab created first. | A gets only a core ButtonPress. | Same. yserver reaches it differently: its core GrabButton request deleted A's XI2 grab on the same window, button and modifiers, so only the core grab exists at the press (see P4c). |
| P4c | A: XI2 sync XIGrabButton (master 2, XIAnyButton, XIAnyModifier, Press/Release) on D, then core sync GrabButton (AnyButton, AnyModifier) on D, then XUngrabButton (AnyButton, AnyModifier) on D. B selects master 2 Press/Release on D. Press; XIAllowEvents ReplayDevice; release. | The XI2 grab survives both core requests: A gets XI_ButtonPress d2 on D; B gets press d2 and release d2. | A gets nothing: the core GrabButton removed the XI2 grab, so no grab activates; B gets press d2 and release d2. |
| P5 | A: core async GrabButton (AnyButton, AnyModifier) on W. B: async XI2 XIGrabButton for slave 4, button 1, on D (a descendant of W); B also selects master 2 Press/Release on D. Press and release over D. | B's slave grab wins: B gets press d4 and release d4 on D. A gets nothing: the slave pass runs over the whole tree before the master pass, and the detached slave produces no master event. | A's core grab on the ancestor wins: A gets core press and release on W; B gets nothing. |

**P4 is xfwm4 4.18 click-to-focus.** Traced with x11trace on Xvfb and on yserver (real KMS, NVIDIA and AMD, 2026-11-21): on every client window xfwm4 issues a core sync `GrabButton` (AnyButton, AnyModifier) and then an XI2 sync `XIPassiveGrabDevice` (master 2, XIAnyButton, XIAnyModifier, mask Press/Release). On Xvfb xfwm4 receives one XI2 press and sends one ReplayPointer; on yserver it receives the press in core and XI2 form and sends two ReplayPointer, on #373 and on #374 alike.

**Source.**
- A passive grab request replaces an existing grab only when `GrabMatchesSecond` holds, which first requires the same grab type (`../xserver/dix/grabs.c`, `GrabMatchesSecond`: `pFirstGrab->grabtype != pSecondGrab->grabtype` returns FALSE; used by `AddPassiveGrabToList`, `:551-565`). A core GrabButton and an XI2 XIPassiveGrabDevice on the same window, button and modifiers therefore coexist (P4b, P4c).
- `CheckDeviceGrabs` (`../xserver/dix/events.c:4187`) runs for each device event, slave first, then master (`../xserver/mi/mieq.c:502-508`). Core grabs match only on the master pass (`checkCore`, `CheckPassiveGrab`, `../xserver/dix/events.c` `MatchForType(..., CORE, ...)` under `checkCore`).
- On one window the first grab in `wPassiveGrabs` that matches wins (`CheckPassiveGrabsOnWindow`). `AddPassiveGrabToList` prepends (`../xserver/dix/grabs.c:564-565`), so the newest matching grab wins.
- `ActivatePassiveGrab` (`../xserver/dix/events.c:3833`) converts the event to the grab's type only (CORE, XI or XI2), activates the grab, and delivers once to the grab client on `grab->window` with the event's own filter as mask, whatever the grab's event mask or owner_events.
- A passive grab on a slave detaches it (`../xserver/dix/events.c:1622-1624`); `mieq` then skips the master pass for that event (`../xserver/mi/mieq.c:505-508`).
- ReplayPointer/ReplayDevice reprocess only the frozen device's stored event (`ComputeFreezes`, `../xserver/dix/events.c:1318`, replay at `:1331-1363`): `CheckDeviceGrabs(replayDev, ...)` below the grab window, else `DeliverDeviceEvents(..., replayDev)`.

## Reproduction

The client is `implicit-oracle.c` from `2026-11-19-per-device-implicit-grabs-measurements.md` with the patch below applied (saved as `passive-oracle.c`).

```sh
cc -std=c11 -O2 -Wall -Wextra -Werror passive-oracle.c -o passive-oracle $(pkg-config --cflags --libs x11 xi xtst)
for c in P1 P1m P2 P2r P2m P3 P4 P4b P4c P5; do   # one fresh server per case ./passive-oracle :97 $c; ./passive-oracle :96 $c; done
```

```diff
--- implicit-oracle.c
+++ passive-oracle.c
@@ -162,8 +162,8 @@
     }
     s->grab = XCreateSimpleWindow(s->a, s->root, 100, 100, 300, 300, 0,
                                   BlackPixel(s->a, s->screen), WhitePixel(s->a, s->screen));
-    s->child_owned_by_b = !strcmp(case_id, "C8");
-    s->outside_owned_by_b = !strcmp(case_id, "C20") || case_id[0] == 'I';
+    s->child_owned_by_b = !strcmp(case_id, "C8") || case_id[0] == 'P';
+    s->outside_owned_by_b = !strcmp(case_id, "C20") || case_id[0] == 'I' || case_id[0] == 'P';
     if (s->child_owned_by_b) XSync(s->a, False);
     Display *child_owner = s->child_owned_by_b ? s->b : s->a;
     s->child = XCreateSimpleWindow(child_owner, s->grab, 20, 20, 180, 180, 0,
@@ -556,6 +556,31 @@
     move_to(s, 660, 160);
 }
 
+static void pgrab_xi2_button(struct Setup *s, int dev, int button, Window window, int mode, Bool owner,
+                             unsigned int bits, XIGrabModifiers *mods) {
+    unsigned char mask[XIMaskLen(XI_LASTEVENT)];
+    memset(mask, 0, sizeof(mask));
+    if (bits & (1u << XI_Motion)) XISetMask(mask, XI_Motion);
+    if (bits & (1u << XI_ButtonPress)) XISetMask(mask, XI_ButtonPress);
+    if (bits & (1u << XI_ButtonRelease)) XISetMask(mask, XI_ButtonRelease);
+    XIEventMask em = { .deviceid = dev, .mask_len = sizeof(mask), .mask = mask };
+    XIGrabModifiers m = *mods;
+    int status = XIGrabButton(s->a, dev, button, window, None, mode, GrabModeAsync, owner, &em, 1, &m);
+    XSync(s->a, False);
+    printf("GRAB XIGrabButton client=A window=%s device=%d button=%d mode=%s owner_events=%d mask=0x%x status=%d modifier_status=%d\n",
+           window_name(window), dev, button, mode == GrabModeSync ? "sync" : "async", owner, bits, status, m.status);
+}
+static void pgrab_xi2(struct Setup *s, int dev, Window window, int mode, Bool owner, unsigned int bits, XIGrabModifiers *mods) {
+    pgrab_xi2_button(s, dev, 1, window, mode, owner, bits, mods);
+}
+static void pgrab_core(struct Setup *s, Window window, unsigned int button) {
+    XGrabButton(s->a, button, AnyModifier, window, False, ButtonPressMask | ButtonReleaseMask,
+                GrabModeSync, GrabModeAsync, None, None);
+    XSync(s->a, False);
+    printf("GRAB XGrabButton client=A window=%s button=%u modifiers=AnyModifier pointer_mode=sync mask=Press|Release\n",
+           window_name(window), button);
+}
+
 static void run_case(struct Setup *s, const char *case_id) {
     const unsigned int motion = 1u << XI_Motion;
     const unsigned int buttons = (1u << XI_ButtonPress) | (1u << XI_ButtonRelease);
@@ -868,6 +893,72 @@
         drain_all_for(s, 75);
         release_b(s, 1);
         move_to(s, 660, 160);
+    } else if (case_id[0] == 'P') {
+        if (s->xtest_pointer < 0) { puts("BLOCKED no XTest pointer device"); return; }
+        XIGrabModifiers anymod = { .modifiers = XIAnyModifier };
+        if (!strcmp(case_id, "P1") || !strcmp(case_id, "P1m")) {
+            int dev = case_id[2] == 'm' ? s->master : s->xtest_pointer;
+            printf("SCENARIO %s A async XI2 passive grab dev=%d on W owner_events=1 Press|Release|Motion; B slave+master XI2 on D (B-owned)\n", case_id, dev);
+            select_xi2(s->b, s->child, s->xtest_pointer, XI_GRAB_BITS);
+            select_xi2(s->b, s->child, s->master, XI_GRAB_BITS);
+            pgrab_xi2(s, dev, s->grab, GrabModeAsync, True, XI_GRAB_BITS, &anymod);
+            move_to(s, 150, 150); press_at(s, 1); move_to(s, 160, 160); release_b(s, 1); move_to(s, 165, 165);
+        } else if (!strcmp(case_id, "P2") || !strcmp(case_id, "P2r") || !strcmp(case_id, "P2m")) {
+            int dev = case_id[2] == 'm' ? s->master : s->xtest_pointer;
+            printf("SCENARIO %s A sync XI2 passive grab dev=%d on W owner_events=0 mask=Release only; B slave+master XI2 on D; then %s\n",
+                   case_id, dev, case_id[2] == 'r' ? "ReplayDevice" : "AsyncDevice");
+            select_xi2(s->b, s->child, s->xtest_pointer, XI_GRAB_BITS);
+            select_xi2(s->b, s->child, s->master, XI_GRAB_BITS);
+            pgrab_xi2(s, dev, s->grab, GrabModeSync, False, 1u << XI_ButtonRelease, &anymod);
+            move_to(s, 150, 150); press_at(s, 1);
+            XIAllowEvents(s->a, dev, case_id[2] == 'r' ? XIReplayDevice : XIAsyncDevice, CurrentTime); XSync(s->a, False);
+            printf("ALLOW XIAllowEvents client=A device=%d mode=%s\n", dev, case_id[2] == 'r' ? "ReplayDevice" : "AsyncDevice");
+            drain_all_for(s, 100); release_b(s, 1); move_to(s, 165, 165);
+        } else if (!strcmp(case_id, "P3")) {
+            puts("SCENARIO P3 A core sync GrabButton(any button, AnyModifier) on W; B slave-only XI2 on D; ReplayPointer; drag to OUT and release");
+            select_xi2(s->b, s->child, s->xtest_pointer, XI_GRAB_BITS);
+            pgrab_core(s, s->grab, AnyButton);
+            move_to(s, 150, 150); press_at(s, 1);
+            XAllowEvents(s->a, ReplayPointer, CurrentTime); XSync(s->a, False);
+            puts("ALLOW XAllowEvents client=A mode=ReplayPointer");
+            drain_all_for(s, 100); move_to(s, 650, 150); release_b(s, 1); move_to(s, 660, 160);
+        } else if (!strcmp(case_id, "P4") || !strcmp(case_id, "P4b")) {
+            int core_first = !strcmp(case_id, "P4");
+            printf("SCENARIO %s xfwm4: A core sync GrabButton(any,AnyModifier) and XI2 sync XIGrabButton(master,any,XIAnyModifier,Press|Release) on D (B-owned), %s created first; B master XI2 Press|Release on D; one XAllowEvents ReplayPointer\n",
+                   case_id, core_first ? "core" : "XI2");
+            select_xi2(s->b, s->child, s->master, (1u << XI_ButtonPress) | (1u << XI_ButtonRelease));
+            if (core_first) pgrab_core(s, s->child, AnyButton);
+            pgrab_xi2_button(s, s->master, XIAnyButton, s->child, GrabModeSync, False, (1u << XI_ButtonPress) | (1u << XI_ButtonRelease), &anymod);
+            if (!core_first) pgrab_core(s, s->child, AnyButton);
+            move_to(s, 150, 150); press_at(s, 1);
+            XAllowEvents(s->a, ReplayPointer, CurrentTime); XSync(s->a, False);
+            puts("ALLOW XAllowEvents client=A mode=ReplayPointer");
+            drain_all_for(s, 100); release_b(s, 1); move_to(s, 165, 165);
+        } else if (!strcmp(case_id, "P4c")) {
+            puts("SCENARIO P4c A XI2 sync XIGrabButton(master,any,XIAnyModifier,Press|Release) on D, then core sync GrabButton(any,AnyModifier) on D, then XUngrabButton(core); B master XI2 Press|Release on D; press; XIAllowEvents ReplayDevice; release");
+            select_xi2(s->b, s->child, s->master, (1u << XI_ButtonPress) | (1u << XI_ButtonRelease));
+            pgrab_xi2_button(s, s->master, XIAnyButton, s->child, GrabModeSync, False, (1u << XI_ButtonPress) | (1u << XI_ButtonRelease), &anymod);
+            pgrab_core(s, s->child, AnyButton);
+            XUngrabButton(s->a, AnyButton, AnyModifier, s->child); XSync(s->a, False);
+            puts("REQUEST XUngrabButton client=A window=D button=any modifiers=AnyModifier");
+            move_to(s, 150, 150); press_at(s, 1);
+            XIAllowEvents(s->a, s->master, XIReplayDevice, CurrentTime); XSync(s->a, False);
+            puts("ALLOW XIAllowEvents client=A device=master mode=ReplayDevice");
+            drain_all_for(s, 100); release_b(s, 1); move_to(s, 165, 165);
+        } else if (!strcmp(case_id, "P5")) {
+            puts("SCENARIO P5 A core async GrabButton(any,AnyModifier) on W (ancestor); B async XI2 XIGrabButton slave 4 button 1 on D (descendant); press, release over D");
+            XGrabButton(s->a, AnyButton, AnyModifier, s->grab, False, ButtonPressMask | ButtonReleaseMask, GrabModeAsync, GrabModeAsync, None, None);
+            XSync(s->a, False); puts("GRAB XGrabButton client=A window=W button=0 modifiers=AnyModifier pointer_mode=async mask=Press|Release");
+            unsigned char m5[XIMaskLen(XI_LASTEVENT)]; memset(m5, 0, sizeof(m5)); XISetMask(m5, XI_ButtonPress); XISetMask(m5, XI_ButtonRelease);
+            XIEventMask em5 = { .deviceid = s->xtest_pointer, .mask_len = sizeof(m5), .mask = m5 };
+            XIGrabModifiers mm = { .modifiers = XIAnyModifier };
+            int st = XIGrabButton(s->b, s->xtest_pointer, 1, s->child, None, GrabModeAsync, GrabModeAsync, False, &em5, 1, &mm);
+            XSync(s->b, False); printf("GRAB XIGrabButton client=B window=D device=%d button=1 mode=async status=%d modifier_status=%d\n", s->xtest_pointer, st, mm.status);
+            select_xi2(s->b, s->child, s->master, (1u << XI_ButtonPress) | (1u << XI_ButtonRelease));
+            move_to(s, 150, 150); press_at(s, 1); release_b(s, 1); move_to(s, 165, 165);
+        } else {
+            fprintf(stderr, "unknown case id: %s\n", case_id); exit(2);
+        }
     } else {
         fprintf(stderr, "unknown case id: %s\n", case_id);
         exit(2);
```

## XI1 implicit grabs (I5) and direct XTest device motion (C18)

Added 2026-11-21 when the user folded both follow-ups into this PR. Same Xvfb and yserver build as above, one fresh server per case; every case starts with `mask=0x0`.

**I5** is the case of the same name in `2026-11-19-per-device-implicit-grabs-measurements.md`, re-run unchanged with `implicit-oracle` on the #374 tree. **X1-X3** extend C18 of `2026-11-16-pointer-routing-xorg-measurements.md`: that case excluded the sprite because yserver does not move it. Here the sprite is the measured quantity. A selects XI2 Motion for master 2 and core PointerMotion on W; B selects the same on OUT. The pointer is at `(150,150)` over D. The injector sends one XTestFakeInput DeviceMotionNotify for XTest pointer 4, then `XQueryPointer` reports the sprite.

| case | input | Xvfb 21.1.24 | yserver (#374) |
|---|---|---|---|
| I5 | XI1: A selects DeviceMotionNotify/DeviceButtonPress/DeviceButtonRelease for device 4 on W, B the same on OUT. Press over D, drag to OUT, release, move. | A gets DeviceButtonPress, the drag DeviceMotionNotify and DeviceButtonRelease on W (XI1 implicit grab on device 4). B gets only the motion after the release, on OUT. | A gets the press only. The drag motion and the **release go to B** on OUT. |
| X1 | absolute (detail 0), axes 0-1 = `(650,150)` | Sprite moves to `(650,150)`, child OUT. B gets XI_Motion d2/s4 on OUT. | Sprite stays at `(150,150)`, child W. **No event to anyone.** |
| X2 | relative (detail 1), axes 0-1 = `(+500,0)` | Same as X1. | Same as X1: no movement, no event. |
| X3 | absolute, one axis: `first_valuator=1`, value 175 | Sprite moves to `(150,0)`, child None: Xorg's FakeInput parser stores the value at index 1 and `valuator_mask_set_range` reads index 0 (C18). No client selected the root, so no event. | Sprite stays at `(150,150)`. |

Core MotionNotify is absent on Xvfb in X1/X2 because the same client's XI2 selection on the same window takes the event first; this is #373's walk, not part of the change.

**Source.**
- **XI1 implicit grab.** `ActivateImplicitGrab` (`../xserver/dix/events.c:2152-2196`) types a DeviceButtonPress delivery as an `XI` grab and activates it on the delivering device with `ImplicitGrabMask`, so a slave is not detached (`:1622-1624`). It is called from `DeliverEventsToWindow` (`:2423`) for any delivered press when no grab is active. It ends with the release that leaves the device's `buttonsDown` at zero (`../xserver/Xi/exevents.c:1928-1955`), as for #374's XI2 slave grabs.
- **History.** R6.6 (`ded6147bf`) grabbed on DeviceButtonPress only for the DeviceButtonPressGrab class (`CheckDeviceGrabAndHintWindow`). MPX commit `a511c445d` (2007-09-28, "Remove generation of core events, long live XI!") made the implicit grab apply to `DeviceButtonPress` as to `ButtonPress`. XTS `xts5/XI/XSelectExtensionEvent.m` assertion 5 ("automatic passive grabs are not done") checks the pre-MPX rule; by the source above and I5, Xorg fails its checks 5 and 6. yserver follows XTS today on purpose (`pointer_fanout.rs` doc of `xi1_route_device_event`).
- **XTest device motion.** `ProcXTestFakeInput` (`../xserver/Xext/xtest.c:225-241`) sets `POINTER_ABSOLUTE` when detail is 0 and leaves the motion relative otherwise, copies the valuators (`:248-284`, `valuator_mask_set_range` at `:417`), and calls `GetPointerEvents` on the XTest slave. `fill_pointer_events` (`../xserver/dix/getevents.c:1399-1440`) moves the sprite through `positionSprite` for both modes, and the events go through `mieqProcessDeviceEvent` (`xtest.c:434`), slave then master, like any other motion.
- **yserver.** The DeviceMotionNotify branch of `dispatch_fake_input_with_body` (`process_request/xtest.rs` ~505) only calls `xi1_route_device_event`. Its comment claims Xorg does not move the sprite, which X1-X3 contradict. The core FakeInput MotionNotify branch (~600-635) already moves the sprite through `HostInputEvent::PointerMotion` for absolute and relative fakes.

**Reproduction.** `xtest-oracle.c` is `pointer-oracle.c` from the 2026-11-16 measurements with the patch below. I5 uses `implicit-oracle` unchanged.

```sh
cc -std=c11 -O2 -Wall -Wextra -Werror xtest-oracle.c -o xtest-oracle $(pkg-config --cflags --libs x11 xi xtst)
for c in X1 X2 X3; do   # one fresh server per case
  ./xtest-oracle :97 $c; ./xtest-oracle :96 $c; done
./implicit-oracle :97 I5; ./implicit-oracle :96 I5
```

```diff
--- pointer-oracle.c
+++ xtest-oracle.c
@@ -252,6 +252,7 @@
     printf("INJECT XTestFakeMotionEvent source=Virtual-core-XTEST-pointer root=%d,%d\n", x, y);
 }
 
+static int g_relative;
 static void fake_device_motion(struct Setup *s, int first_axis, int *axes, int count) {
     int xtest_event_base, xtest_error_base, xtest_opcode;
     int xi_opcode, xi_event_base, xi_error_base;
@@ -264,6 +265,7 @@
     }
     unsigned char events[64] = {0};
     events[0] = (unsigned char)(xi_event_base + XI_DeviceMotionNotify);
+    events[1] = (unsigned char)g_relative;
     events[31] = (unsigned char)s->xtest_pointer | 0x80;
     events[32] = (unsigned char)(xi_event_base + XI_DeviceValuator);
     events[33] = (unsigned char)s->xtest_pointer;
@@ -278,7 +280,7 @@
     memcpy(request + 4, events, sizeof(events));
     UnlockDisplay(s->inj);
     XSync(s->inj, False);
-    printf("INJECT XTestFakeDeviceMotionEvent device=%d first_axis=%d count=%d", s->xtest_pointer, first_axis, count);
+    printf("INJECT XTestFakeDeviceMotionEvent device=%d relative=%d first_axis=%d count=%d", s->xtest_pointer, g_relative, first_axis, count);
     for (int i = 0; i < count; i++) printf(" axis%d=%d", first_axis + i, axes[i]);
     printf("\n");
 }
@@ -736,6 +738,23 @@
         drain_all_for(s, 75);
         fake_button(s, False, 1);
         drain_all_for(s, 100);
+    } else if (!strcmp(case_id, "X1") || !strcmp(case_id, "X2") || !strcmp(case_id, "X3")) {
+        /* direct XTest DeviceMotion: does it move the sprite, and which forms reach whom */
+        if (s->xtest_pointer < 0) { puts("BLOCKED no XTest pointer device"); return; }
+        fake_motion(s, 150, 150);
+        drain_all_for(s, 50);
+        unsigned int mo = 1u << XI_Motion;
+        select_xi2(s->a, s->grab, s->master, mo);
+        select_core(s->a, s->grab, PointerMotionMask);
+        select_xi2(s->b, s->outside, s->master, mo);
+        select_core(s->b, s->outside, PointerMotionMask);
+        if (!strcmp(case_id, "X1")) { int ax[2] = {650, 150}; fake_device_motion(s, 0, ax, 2); }
+        else if (!strcmp(case_id, "X2")) { g_relative = 1; int ax[2] = {500, 0}; fake_device_motion(s, 0, ax, 2); }
+        else { int v = 175; fake_device_motion(s, 1, &v, 1); }
+        drain_all_for(s, 100);
+        Window r, c; int rx, ry, wx, wy; unsigned int m;
+        XQueryPointer(s->a, DefaultRootWindow(s->a), &r, &c, &rx, &ry, &wx, &wy, &m);
+        printf("QUERY sprite root=%d,%d child=%s\n", rx, ry, window_name(c));
     } else {
         fprintf(stderr, "unknown case id: %s\n", case_id);
         exit(2);
```

## Branch results (2026-11-22, branch head 30b2db33)

Headless yserver with lavapipe, the same clients and comparison rules as above; one fresh server per P, I5 and X case.

- P1, P1m, P2, P2r, P2m, P3, P4, P4b, P4c and P5: all match Xvfb.
- I1-I8 (implicit-oracle, shared server): all match Xvfb, including I5 (XI1 implicit grab), which differed on #374.
- C1-C20: unchanged from #374 and equal to Xvfb, except C10, which differs only in raw timestamps, and C15, which cannot run headless (its Rust evidence is unchanged).
- X1-X3: the sprite position and the delivered forms match Xvfb: (650,150) for X1 and X2, (150,0) for X3.
- Slave motion forms route from the pre-move sprite window. Measured on Xvfb with core `XTestFakeMotionEvent` and XTest device motion alike: leaving W gives W the slave XI_Motion d4 only, entering W gives it the master d2 only (probes `passive-grabs/probe/m.c`, `q.c`).

XTS (xts5 upstream `6cf9440`, scenarios XI, Xlib13, XIproto), master (`3ef6570b`) vs branch vs Xvfb 21.1.24:
- No purpose regresses against master, and every changed purpose now matches Xvfb.
- XSelectExtensionEvent 5: PASS on master, FAIL on the branch, FAIL on Xvfb (I5, pre-MPX assertion).
- Now passing, as on Xvfb: XSelectExtensionEvent 7, XAllowEvents 8, XGrabKeyboard 12 and 15, XGrabPointer 10.
- XAllowDeviceEvents is back to master/Xvfb after the per-device freeze. XIproto is identical.

Gate: workspace 5150/0, lavapipe `--ignored` 774/0, `just man`, `pointer_fanout` 5× under 16 busy loops.
