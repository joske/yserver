# Passive grab activation and replay: measurements against Xvfb 21.1.24

Oracle: `/data/Projects/xorg-21.1.24` (tag `xorg-server-21.1.24`), cited as `../xserver`. Xvfb ran on `:97` (master pointer 2, XTest pointer 4). yserver ran on `:96` with `--headless` and lavapipe, built from `68447c8f` (the tree of PR #374, head `d479e443` after its rebase).

**Setup.** As in `2026-11-19-per-device-implicit-grabs-measurements.md`: connections A, B and an injector; W at root `(100,100)` 300x300 owned by A; child D at W-relative `(20,20)`; OUT at root `(550,100)`. In every P case D and OUT are owned by B. The injector moves the XTest pointer to `(145,145)` first; each case moves it over D at `(150,150)` before pressing button 1.

**Comparison.** Recipient, event type, device/source and event window, plus grab statuses. Crossings and coordinates are excluded, as in the earlier measurement docs.

| case | setup | Xvfb 21.1.24 | yserver (#374) |
|---|---|---|---|
| P1 | A: async XI2 passive grab (XIGrabButton, button 1, XIAnyModifier) on W for slave 4, owner_events=1, Motion/Press/Release. B selects slave 4 and master 2 on D. Press, move, release over D. | A gets press d4, motion d4 and release d4 on W. B gets no press or release: the activation detached slave 4, so no master event exists. | Same, plus B gets **XI_ButtonPress d2** on D. |
| P1m | As P1, grab on master 2. | A gets XI_ButtonPress d2 on W, then motion/release d2 on W. B gets the slave forms d4 on D. | A gets a **core ButtonPress** on W instead of XI_ButtonPress; the rest matches. |
| P2 | A: sync XI2 passive grab on W for slave 4, owner_events=0, mask **ButtonRelease only**. B selects slave 4 and master 2 on D. Press; XIAllowEvents AsyncDevice; release. | A gets press d4 (outside its mask) and release d4 on W. | Same. |
| P2r | As P2, then ReplayDevice instead of AsyncDevice. | A gets press d4 on W; the replay gives B press d4 and release d4 on D. | Same, plus B gets **press d2 and release d2** on D. |
| P2m | As P2 with the grab on master 2. | A gets XI_ButtonPress d2 and XI_ButtonRelease d2 on W; B gets d4 press/release on D. | A gets **core ButtonPress and XI_ButtonPress d2** for the one activation. |
| P3 (C19 gate) | A: core sync GrabButton (AnyButton, AnyModifier) on W. B selects slave 4 only on D. Press; XAllowEvents ReplayPointer; move to OUT; release. | A gets core ButtonPress on W. B gets press d4 on D once, **before** the replay (the slave is processed first and the core grab lives on the master); the replay replays only the master event. B gets motion d4 and release d4 on D. | B gets **press d4 twice**: at activation and again from the replay. |
| P4 (xfwm4) | A: core sync GrabButton (AnyButton, AnyModifier) on D, **then** XI2 sync XIGrabButton (master 2, XIAnyButton, XIAnyModifier, Press/Release) on D; B selects master 2 Press/Release on D. Press; one XAllowEvents ReplayPointer; release. | A gets only **XI_ButtonPress d2** (the newer grab wins). B gets press d2 and release d2 on D. | A gets **core ButtonPress and XI_ButtonPress d2**. |
| P4b | As P4 with the XI2 grab created first. | A gets only a core ButtonPress (the newer, core grab wins). | Same output, but by two compensating errors: yserver picks the XI2 grab (device specificity), then delivers it in core form (as P1m). |
| P5 | A: core async GrabButton (AnyButton, AnyModifier) on W. B: async XI2 XIGrabButton for slave 4, button 1, on D (a descendant of W); B also selects master 2 Press/Release on D. Press and release over D. | B's slave grab wins: B gets press d4 and release d4 on D. A gets nothing: the slave pass runs over the whole tree before the master pass, and the detached slave produces no master event. | A's core grab on the ancestor wins: A gets core press and release on W; B gets nothing. |

**P4 is xfwm4 4.18 click-to-focus.** Traced with x11trace on Xvfb and on yserver (real KMS, NVIDIA and AMD, 2026-11-21): on every client window xfwm4 issues a core sync `GrabButton` (AnyButton, AnyModifier) and then an XI2 sync `XIPassiveGrabDevice` (master 2, XIAnyButton, XIAnyModifier, mask Press/Release). On Xvfb xfwm4 receives one XI2 press and sends one ReplayPointer; on yserver it receives the press in core and XI2 form and sends two ReplayPointer, on #373 and on #374 alike.

**Source.**
- `CheckDeviceGrabs` (`../xserver/dix/events.c:4187`) runs for each device event, slave first, then master (`../xserver/mi/mieq.c:502-508`). Core grabs match only on the master pass (`checkCore`, `CheckPassiveGrab`, `../xserver/dix/events.c` `MatchForType(..., CORE, ...)` under `checkCore`).
- On one window the first grab in `wPassiveGrabs` that matches wins (`CheckPassiveGrabsOnWindow`). `AddPassiveGrabToList` prepends (`../xserver/dix/grabs.c:564-565`), so the newest matching grab wins.
- `ActivatePassiveGrab` (`../xserver/dix/events.c:3833`) converts the event to the grab's type only (CORE, XI or XI2), activates the grab, and delivers once to the grab client on `grab->window` with the event's own filter as mask, whatever the grab's event mask or owner_events.
- A passive grab on a slave detaches it (`../xserver/dix/events.c:1622-1624`); `mieq` then skips the master pass for that event (`../xserver/mi/mieq.c:505-508`).
- ReplayPointer/ReplayDevice reprocess only the frozen device's stored event (`ComputeFreezes`, `../xserver/dix/events.c:1318`, replay at `:1331-1363`): `CheckDeviceGrabs(replayDev, ...)` below the grab window, else `DeliverDeviceEvents(..., replayDev)`.

## Reproduction

The client is `implicit-oracle.c` from `2026-11-19-per-device-implicit-grabs-measurements.md` with the patch below applied (saved as `passive-oracle.c`).

```sh
cc -std=c11 -O2 -Wall -Wextra -Werror passive-oracle.c -o passive-oracle $(pkg-config --cflags --libs x11 xi xtst)
for c in P1 P1m P2 P2r P2m P3 P4 P4b P5; do ./passive-oracle :97 $c; ./passive-oracle :96 $c; done
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
@@ -868,6 +893,61 @@
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
