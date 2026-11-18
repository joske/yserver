# Pointer-routing measurements against Xvfb 21.1.24

Status: **Task 6 branch remeasurement complete; C18's direct-XTest sprite geometry remains a documented follow-up, and the layout gate passes after the behavior-preserving extraction in Fix round 2.** The matrix has 22 case ids: 21 Xvfb/branch client comparisons plus C15, measured on Xvfb and covered on yserver by its named Rust regression. All 21 client comparisons match within the scoped exclusions below. C18's child and event/root coordinates are excluded by the coordinator ruling; recipient, event, window, device/source, and valuator payload still match.

Oracle: `/data/Projects/xorg-21.1.24`, cited as `../xserver` (tag `xorg-server-21.1.24`). Xvfb ran on `:97` and reported vendor release `12101024` (21.1.24). Its master pointer was 2, XTest pointer 4, and Xvfb mouse slave 6. The original baseline rows used yserver `d8d3416b` (binary `368d688caac3`). Task 6 rebuilt branch HEAD `ba1f031c6c9b`; yserver ran on `:98`, reported `10900000`, and logged `llvmpipe (LLVM 21.1.8, 256 bits)`. Its device list had master pointer 2 and XTest pointer 4. The original baseline source is unchanged from `d8d3416b`; the current branch includes the routing changes measured below.

The client creates W at root `(100,100)`, D at W-relative `(20,20)`, and OUT at root `(550,100)`, all borderless and mapped. A and B are separate connections; the injector is a third connection. Each case logs the initial pointer position, then moves the XTest pointer to `(145,145)` before installing that case's selections and grabs. This makes the first measured motion begin inside D and removes dependence on the server's prior case state. Xvfb began each case at `(512,384)`; yserver retained its previous case position. The pre-position is recorded in every result file.

Notation: `XI_Motion/6 d2/s4 W/D e=(50,50) r=(150,150)` means event type/evtype, deviceid/sourceid, event window/child, event coordinates, and root coordinates. For core events `d/s=n/a`; XI1 events have a device id but no XI2-style sourceid. Crossings include detail, mode, focus, and same_screen. Table order is each receiving client's queue order; order between separate clients is not a global wire-order claim. Full-match labels compare delivered event streams, excluding setup and injection log lines.

## Findings table

| case id | setup (grabbing client, grab request, grabbed device, owner_events, grab mask; each client's selections with window, device and event types; input source) | injected input | Xvfb 21.1.24 result (per client: event type/evtype, deviceid, sourceid, event window, child, event_x/y, root_x/y) | yserver d8d3416b result | yserver branch | base full match yes/no | base scoped comparison |
|---|---|---|---|---|---|---|---|
| C1 | A: XIGrabDevice master 2 on W, owner_events=true, XI_Motion/ButtonPress/ButtonRelease; no natural selection. B: XI2 W/master 2 Motion/Press/Release. XTest source 4. | Motion to `(150,150)` after common pre-position. | A: XI_Motion/6 d2/s4 W/D e=(50,50) r=(150,150). B: none. | A: XI_Leave/8 d2/s2 D/None e=(25,25) r=(145,145) detail=0 mode=1 focus=1 same=1; XI_Enter/7 d2/s2 W/None e=(45,45) r=(145,145) detail=2 mode=1 focus=1 same=1; XI_Motion/6 d2/s4 W/None e=(50,50) r=(150,150). B: XI_Motion/6 d2/s4 W/D e=(50,50) r=(150,150). | Scoped match; raw stream: `oracle/results/yserver-branch/C1.txt`. | no | no |
| C2 | A: XIGrabDevice master 2 on W, owner_events=true, Motion/Press/Release; no natural selection. B: XI2 D/master 2 Motion/Press/Release. XTest source 4. | Motion to `(150,150)`, over D. | A: XI_Motion/6 d2/s4 W/D e=(50,50) r=(150,150). B: none. | A: XI_Leave/8 d2/s2 D/None e=(25,25) r=(145,145) detail=0 mode=1 focus=1 same=1; XI_Enter/7 d2/s2 W/None e=(45,45) r=(145,145) detail=2 mode=1 focus=1 same=1; XI_Motion/6 d2/s4 W/None e=(50,50) r=(150,150). B: XI_Motion/6 d2/s4 D/None e=(30,30) r=(150,150). | Scoped match; raw stream: `oracle/results/yserver-branch/C2.txt`. | no | no |
| C3 | A: XIGrabDevice master 2 on W, owner_events=true, Motion/Press/Release; no natural selection. B: XI2 OUT/master 2 Motion/Press/Release; OUT is owned by A and outside W's tree. XTest source 4. | Motion to `(650,150)`, over OUT. | A: XI_Motion/6 d2/s4 W/None e=(550,50) r=(650,150). B: none. | A: XI_Leave/8 d2/s2 D/None e=(25,25) r=(145,145) detail=0 mode=1 focus=1 same=1; XI_Enter/7 d2/s2 W/None e=(45,45) r=(145,145) detail=2 mode=1 focus=1 same=1; XI_Motion/6 d2/s4 W/None e=(550,50) r=(650,150). B: XI_Motion/6 d2/s4 OUT/None e=(100,50) r=(650,150). | Scoped match; raw stream: `oracle/results/yserver-branch/C3.txt`. | no | no |
| C4 | A: XIGrabDevice master 2 on W, owner_events=true, Motion/Press/Release; A also selects core PointerMotion on W. B: XI2 W/master 2 Motion/Press/Release. XTest source 4. | Motion to `(150,150)`. | A: MotionNotify d/s=n/a W/D e=(50,50) r=(150,150). B: none. | A: XI_Leave/8 d2/s2 D/None e=(25,25) r=(145,145) detail=0 mode=1 focus=1 same=1; XI_Enter/7 d2/s2 W/None e=(45,45) r=(145,145) detail=2 mode=1 focus=1 same=1; XI_Motion/6 d2/s4 W/None e=(50,50) r=(150,150). B: XI_Motion/6 d2/s4 W/D e=(50,50) r=(150,150). | Scoped match; raw stream: `oracle/results/yserver-branch/C4.txt`. | no | no |
| C4b | A: XIGrabDevice master 2 on W, owner_events=true, Motion/Press/Release; A selects XI2 master Motion/Press/Release and core PointerMotion on D. B: XI2 D/master 2 Motion/Press/Release. XTest source 4. | Motion to `(150,150)`, over D. | A: XI_Motion/6 d2/s4 D/None e=(30,30) r=(150,150); no core duplicate. B: none. | A: XI_Leave/8 d2/s2 D/None e=(25,25) r=(145,145) detail=0 mode=1 focus=1 same=1; XI_Enter/7 d2/s2 W/None e=(45,45) r=(145,145) detail=2 mode=1 focus=1 same=1; XI_Motion/6 d2/s4 D/None e=(30,30) r=(150,150). B: XI_Motion/6 d2/s4 D/None e=(30,30) r=(150,150). | Scoped match; raw stream: `oracle/results/yserver-branch/C4b.txt`. | no | no |
| C5 | A: XIGrabDevice master 2 on W, owner_events=true, Motion/Press/Release. B: XI2 W/slave 4 Motion only. XTest source 4. | Motion to `(150,150)`. | A: XI_Motion/6 d2/s4 W/D e=(50,50) r=(150,150). B: XI_Motion/6 d4/s4 W/D e=(45,45) r=(145,145). | A: XI_Leave/8 d2/s2 D/None e=(25,25) r=(145,145) detail=0 mode=1 focus=1 same=1; XI_Enter/7 d2/s2 W/None e=(45,45) r=(145,145) detail=2 mode=1 focus=1 same=1; XI_Motion/6 d2/s4 W/None e=(50,50) r=(150,150). B: XI_Motion/6 d4/s4 W/D e=(50,50) r=(150,150). | Scoped match; attached-slave coordinates remain excluded; raw stream: `oracle/results/yserver-branch/C5.txt`. | no | no |
| C6 | A: core GrabPointer on OUT, owner_events=true, core PointerMotion/ButtonPress/ButtonRelease; A selects core PointerMotion on W. B: XI2 D/master 2 Motion. XTest source 4. | Motion to `(150,150)`. | A: MotionNotify d/s=n/a OUT/None e=(-400,50) r=(150,150). B: none. | A: MotionNotify d/s=n/a W/D e=(50,50) r=(150,150). B: XI_Motion/6 d2/s4 D/None e=(30,30) r=(150,150). | Scoped match; raw stream: `oracle/results/yserver-branch/C6.txt`. | no | no |
| C7 | A: core GrabPointer on W, owner_events=true, core PointerMotion/ButtonPress/ButtonRelease; A selects core PointerMotion on W. B: XI2 W/master 2 Motion/Press/Release. XTest source 4. | Motion to `(150,150)`. | A: MotionNotify d/s=n/a W/D e=(50,50) r=(150,150). B: none. | A: MotionNotify d/s=n/a W/D e=(50,50) r=(150,150). B: XI_Motion/6 d2/s4 W/D e=(50,50) r=(150,150). | Scoped match; raw stream: `oracle/results/yserver-branch/C7.txt`. | no | no |
| C7s | A: core GrabPointer on OUT, owner_events=true, core PointerMotion/ButtonPress/ButtonRelease; A selects core PointerMotion on W. B: XI2 D/slave 4 Motion only. XTest source 4. | Motion to `(150,150)`. | A: MotionNotify d/s=n/a W/D e=(50,50) r=(150,150). B: XI_Motion/6 d4/s4 D/None e=(25,25) r=(145,145). | A: MotionNotify d/s=n/a W/D e=(50,50) r=(150,150). B: XI_Motion/6 d4/s4 D/None e=(30,30) r=(150,150). | Scoped match; attached-slave coordinates remain excluded; raw stream: `oracle/results/yserver-branch/C7s.txt`. | no | yes‡ |
| C8 | No explicit grab. C8a: A selects XIAllMasterDevices ButtonPress/ButtonRelease on W; delivered XI2 press activates an XI2 implicit grab. C8b: A owns ancestor W and selects core ButtonPress/ButtonRelease there; B owns leaf D and selects XIAllMasterDevices ButtonPress/ButtonRelease on D; B gets the XI2 implicit grab. XTest source 4. | C8a: motion to `(350,350)`, press, release. C8b: motion to `(150,150)`, press, release. | A C8a: XI_ButtonPress/4 d2/s4 W/None e=(250,250) r=(350,350), then XI_ButtonRelease/5 same fields. A C8b: none. B C8a: none. B C8b: XI_ButtonPress/4 d2/s4 D/None e=(30,30) r=(150,150), then XI_ButtonRelease/5 same fields. | Same delivered event streams and fields as Xvfb: A receives C8a only; B receives C8b press/release on D. Result header confirms `window_owners=W:A,D:B,OUT:A`. | Scoped match; raw stream: `oracle/results/yserver-branch/C8.txt`. | yes | yes |
| C8c | No explicit grab. A selects core ButtonPress/ButtonRelease/PointerMotion on W. B selects XI2 master 2 Motion on D. XTest source 4. | Motion to `(150,150)`, press, motion to `(160,160)`, release. | B: XI_Motion/6 d2/s4 D/None e=(30,30) r=(150,150) before press; no in-grab Motion. A: ButtonPress W/D e=(50,50) r=(150,150); MotionNotify W/D e=(60,60) r=(160,160); ButtonRelease W/D e=(60,60) r=(160,160). The core press activates the core implicit grab. | A receives the same core press, motion, and release. B receives the initial XI_Motion and also an in-grab XI_Motion/6 d2/s4 D/None e=(40,40) r=(160,160). | Scoped match; raw stream: `oracle/results/yserver-branch/C8c.txt`. | no | no |
| C9 (dual-XI2) | No explicit grab. A selects XI2 ButtonPress/ButtonRelease on W for slave 4 and master 2; no core selection. XTest source 4. | Motion to `(150,150)`, press, release. | A: XI_ButtonPress/4 d4/s4 W/D e=(50,50) r=(150,150), XI_ButtonPress/4 d2/s4 same fields; then XI_ButtonRelease/5 d4/s4 W/D e=(50,50) r=(150,150), XI_ButtonRelease/5 d2/s4 same fields. B: none. | A: same two press forms and order; release forms are d4/s4 W/None e=(50,50) r=(150,150), then d2/s4 W/None with the same coordinates. B: none. | Scoped match; releases report child D; raw stream: `oracle/results/yserver-branch/C9.txt`. | no | no |
| C9e (E27 core+slave) | No explicit grab. A, on one connection, selects core ButtonPress/ButtonRelease and XI2 ButtonPress/ButtonRelease on W for slave 4; no master XI2 selection. XTest source 4. | Motion to `(150,150)`, press, release. | A queue order: XI_ButtonPress/4 d4/s4 W/D e=(50,50) r=(150,150), core ButtonPress W/D e=(50,50) r=(150,150), XI_ButtonRelease/5 d4/s4 same fields, core ButtonRelease W/D same fields. Core release arrives. B: none. | A queue order: core ButtonPress W/D e=(50,50) r=(150,150), XI_ButtonPress/4 d4/s4 same fields, core ButtonRelease W/D same fields, XI_ButtonRelease/5 d4/s4 same fields. Core release arrives. B: none. | Scoped match; queue order is slave XI2 then core; raw stream: `oracle/results/yserver-branch/C9e.txt`. | no | no |
| C10 | No grab. B selects XI1 DeviceMotionNotify and DeviceButtonPress on W for XTest device 4 via XSelectExtensionEvent; DeviceValuator is also received. XI1 events have device id 4 and no XI2-style sourceid. XTest source 4. | After common pre-position, motions to `(150,150)` and `(155,155)`, then button press and release. | A: none. B: DeviceMotionNotify d4 W/D e=(45,45) r=(145,145), valuators `(150,150)`; then DeviceMotionNotify d4 W/D e=(50,50) r=(150,150), valuators `(155,155)`; DeviceButtonPress d4 W/D e=(55,55) r=(155,155), byte31 `0x04`, follow-up packet none. Both motion byte31 values are `0x84`; each is followed by DeviceValuator with valuators `(150,150)` then `(155,155)`. | A: none. B: DeviceMotionNotify d4 W/D e=(50,50) r=(150,150), valuators `(150,150)`; then DeviceMotionNotify d4 W/D e=(55,55) r=(155,155), valuators `(155,155)`; DeviceButtonPress d4 W/D e=(55,55) r=(155,155), byte31 `0x04`, follow-up packet none. Both motion byte31 values are `0x04`; each DeviceValuator has the matching valuators. | Scoped match; DeviceMotionNotify byte 31 is `0x84`; C10 coordinates remain excluded; raw stream: `oracle/results/yserver-branch/C10.txt`. | no | no |
| C11 | A: XIGrabDevice master 2 on W, owner_events=true, Motion/Press/Release. B: XI2 W/master 2 Motion/Press/Release. XTest source 4. | Motion to `(150,150)`, XIUngrabDevice by A, motion to `(160,160)`. | A: XI_Motion/6 d2/s4 W/D e=(50,50) r=(150,150) before ungrab. B: XI_Motion/6 d2/s4 W/D e=(60,60) r=(160,160) after ungrab. | Before first motion: XI_Leave/8 d2/s2 D/None e=(25,25) r=(145,145) detail=0 mode=1 focus=1 same=1; XI_Enter/7 d2/s2 W/None e=(45,45) r=(145,145) detail=2 mode=1 focus=1 same=1. Then A XI_Motion/6 d2/s4 W/None e=(50,50) r=(150,150), B XI_Motion W/D e=(50,50) r=(150,150). After ungrab: XI_Leave/8 d2/s2 W/None e=(50,50) r=(150,150) detail=2 mode=2 focus=1 same=1; XI_Enter/7 d2/s2 D/None e=(30,30) r=(150,150) detail=0 mode=2 focus=1 same=1; B XI_Motion/6 d2/s4 W/D e=(60,60) r=(160,160). | Scoped match; raw stream: `oracle/results/yserver-branch/C11.txt`. | no | no |
| C12 | A: XIGrabDevice master 2 on W, owner_events=true, Motion/Press/Release; no natural selections. XTest source 4. | Motion to `(650,150)`, outside W's tree. | A: XI_Motion/6 d2/s4 W/None e=(550,50) r=(650,150). B: none. | A: XI_Leave/8 d2/s2 D/None e=(25,25) r=(145,145) detail=0 mode=1 focus=1 same=1; XI_Enter/7 d2/s2 W/None e=(45,45) r=(145,145) detail=2 mode=1 focus=1 same=1; XI_Motion/6 d2/s4 W/None e=(550,50) r=(650,150). B: none. | Scoped match; raw stream: `oracle/results/yserver-branch/C12.txt`. | no | yes |
| C13 | A: XIGrabDevice master 2 on W, owner_events=true, Motion/Press/Release; no natural selections. B: XI2 D/master 2 ButtonPress/ButtonRelease. XTest source 4. | Motion to `(150,150)`, press, release. | A: XI_Motion/6 d2/s4 W/D e=(50,50) r=(150,150), XI_ButtonPress/4 and XI_ButtonRelease/5 W/D with the same fields. B: none. | A first gets XI_Leave/8 d2/s2 D/None e=(25,25) r=(145,145) detail=0 mode=1 focus=1 same=1 and XI_Enter/7 d2/s2 W/None e=(45,45) r=(145,145) detail=2 mode=1 focus=1 same=1; then XI_Motion/6 d2/s4 W/None e=(50,50) r=(150,150), XI_ButtonPress/4 d2/s4 W/None e=(50,50) r=(150,150), XI_ButtonRelease/5 d2/s4 W/None same coordinates. B: XI_ButtonPress/4 d2/s4 D/None e=(30,30) r=(150,150), then XI_ButtonRelease/5 same fields. | Matches the raw Xvfb result: A receives Motion/Press/Release on W; B receives none. Raw stream: `oracle/results/yserver-branch/C13.txt`. | no | no |
| C14 | A: core GrabPointer on W, owner_events=true, core PointerMotion/ButtonPress/ButtonRelease; A selects core ButtonPress/ButtonRelease on OUT. XTest source 4. | Motion to `(650,150)`, press, release. | A: MotionNotify W/None e=(550,50) r=(650,150), ButtonPress OUT/None e=(100,50) r=(650,150), ButtonRelease OUT/None same fields. B: none. | A: same delivered core event stream and fields as Xvfb. B: none. | Scoped match; raw stream: `oracle/results/yserver-branch/C14.txt`. | yes | yes |
| C15 | A: XIGrabDevice on Xvfb mouse slave 6 at W, owner_events=true, Motion/Press/Release. B: XI2 D/master 2 Motion. Injecting source: XTest slave 4, distinct from grabbed device 6. | Motion to `(150,150)`. | A: none. B: XI_Motion/6 d2/s4 D/None e=(30,30) r=(150,150). | N/A in Task 1: `slave_grab_leaves_master_form_from_other_source` (Task 4 Rust test registers and grabs a distinct source). | Rust regression `slave_grab_leaves_master_form_from_other_source` passed; result: `oracle/results/yserver-branch/C15.txt`. | N/A | N/A |


‡ C7s differs only in the XI2 attached-slave `event_x/y` and `root_x/y` fields: Xvfb reports the pre-motion sprite `(145,145)` while the yserver captures report the injected `(150,150)`. The target window, child, event type, device, and recipient match. These fields are excluded from the routing-only scoped comparison as attached-slave coordinate timing; the complete measured values remain in the table and result files. C5 has the same B-side coordinate timing. The d8d3416b base also reported the A-side fallback child as `None` instead of Xvfb `D`; the branch result now reports `D`.

## C3 correction (coordinator)

An earlier revision of this document stated that Xvfb delivered B's XI_Motion on OUT in C3. The raw result `oracle/results/xvfb/C3.txt` contains only A's fallback XI_Motion on W, and three re-runs of `pointer-oracle :97 C3` on Xvfb 21.1.24 gave the same single event. B receives nothing on Xvfb; yserver d8d3416b delivers B's XI_Motion on OUT. C3 is a scoped difference and agrees with the spec's owner-only contract and Task 3's `owner_events_xi2_owned_toplevel_foreign_selector_gets_nothing`.

## C13 correction (Task 6)

The original C13 Xvfb result cell above incorrectly listed ButtonPress and
ButtonRelease for B. Both the original raw capture
`oracle/results/xvfb/C13.txt` and the Task 6 Xvfb rerun
`oracle/results/xvfb-task6/C13.txt` contain only A's Motion, ButtonPress, and
ButtonRelease on W; B receives no events. The Task 6 branch capture has the
same scoped stream. This correction comes from the measured C13 row and its
preserved raw captures; it does not change any Rust test expectation.

## Scoped comparison and exclusions

The full-stream labels above retain all observed XI_Enter/XI_Leave and geometry data. The scoped comparison omits only these fields/events:

- XI_Enter and XI_Leave crossings: Enter/Leave routing is an explicit spec non-goal. yserver emits additional crossing chains on grab/ungrab transitions; they remain visible in the full result cells. Xorg references: `../xserver/dix/events.c:1633-1641,1693-1713`, `../xserver/dix/enterleave.c:596-607`; yserver paths: `crates/yserver-core/src/core_loop/process_request/xi/dispatch/grabs.rs:318-349,374-386,526-554,566-575`.
- C10 XI1 attached-slave coordinates `event_x/y` and `root_x/y`: the review identifies this timing as outside the planned MORE_EVENTS change. Do not alter the measured geometry. The review's source explanation is consistent with the captures: attached slaves share the master sprite (`../xserver/dix/devices.c:2709-2713`), slave processing precedes master processing (`../xserver/mi/mieq.c:502-508`), and XI1 conversion copies sprite coordinates separately from current valuators (`../xserver/dix/eventconvert.c:358-370,451-467`). Device id, event count/type, byte 31, follow-up packet, and valuators remain in scope.
- C5 and C7s XI2 attached-slave `event_x/y` and `root_x/y`: these fields show the same measured prior-sprite/current-valuator timing, while target, child, recipients, and form match. Excluding these coordinate fields keeps the scoped comparison focused on routing. No button event fields, fallback geometry, or recipient differences are excluded.
- C18 direct-XTest sprite geometry: exclude `child`, `event_x/y`, and `root_x/y` because yserver's direct XTest FakeInput device motion does not move the sprite (`crates/yserver-core/src/core_loop/process_request/xtest.rs:505`, pre-existing and outside this PR's routing scope). Xorg moves the sprite to `y=0` here through its valuator range copy, so the child and coordinates follow a different sprite. Recipient, event type, window, device/source, and valuator payload remain in scope and match.

At baseline d8d3416b, C9's yserver release child `None` differed from Xvfb child `D`; Task 4 corrected it on this branch. C9e's baseline same-client event order was core before slave XI2, while Xvfb is slave XI2 before core; the branch now follows the measured order. C10's baseline DeviceMotionNotify byte 31 was `0x04`, while Xvfb is `0x84`; the branch now preserves `MORE_EVENTS` as `0x84`.

Baseline (`yserver d8d3416b`) results: 2 full-stream matches (C8, C14), 16 full-stream differences, and C15 N/A. After the stated exclusions, 4 scoped matches (C7s, C8, C12, C14), 14 scoped differences, and C15 N/A.

## Plan-relevant measured findings

- C8b now matches the referenced test's ownership shape: A owns core ancestor W; B creates and owns XI2 leaf D. Both servers deliver the XI2 press and release to B on D.
- C8c's Xvfb core ButtonPress establishes the core implicit-grab case; A receives its in-grab Motion and release, while B receives no Motion during that grab. The d8d3416b baseline delivered B's in-grab master XI_Motion; the branch capture now suppresses it. The C8c stop condition passed.
- C9 is explicitly the dual-XI2 case. Both servers deliver two presses and two releases to A, in slave-then-master device form order. The d8d3416b baseline's release child was `None` instead of Xvfb's `D`; the branch result now reports `D`.
- C9e is the separate E27 core-plus-slave case. Both servers deliver core and slave XI2 press/release edges; both core releases arrive. Xorg's internal grab type is not queryable by this client; the measured core ButtonPress takes Xorg's CORE implicit-grab path (`../xserver/dix/events.c:2158-2177,2417-2424`), while the slave XI2 edge is separately observed. The same client's queue order differs between servers, as recorded.
- C10 captures five XI1 wire packets per server: two DeviceMotionNotify, their two DeviceValuator follow-ups, and one DeviceButtonPress. Both DeviceButtonPress byte31 values are `0x04` and have no following packet. Xvfb motion byte31 is `0x84`; the d8d3416b baseline used `0x04`, while the branch capture now preserves `0x84`.
- All explicit grabs in the client oracle runs succeeded (status 0); there are no XERROR or BLOCKED records in those cases. C15 branch evidence comes from the named Rust regression; C16 and C17 now have branch client streams.

## Known differences / follow-ups

- **C18 direct XTest FakeInput sprite movement:** yserver's direct XTest FakeInput device motion does not move the sprite (`crates/yserver-core/src/core_loop/process_request/xtest.rs:505`, pre-existing and outside this PR's routing scope). Xorg moves it here to `y=0` through its valuator range copy, so child and coordinates follow a different sprite. C18's `child`, `event_x/y`, and `root_x/y` are excluded from the scoped comparison. Recipient, event type, window, device/source, and valuator payload remain in scope and match. Any change to yserver's XTest sprite behavior is a separate follow-up.

## Round-two oracle additions: direct XI1 FakeInput under a slave grab

These cases isolate the Task 4 review's direct
`XTestFakeDeviceMotionEvent` caller. The client positions the pointer at
`(150,150)`, selects XI1 `DeviceMotionNotify` on W for device 4, then sends a
real device-motion fake with valuators `(150,150)`. C16 also selects XI2
`XI_Motion` on W; C17 relies on the exclusive XI2 grab's Motion mask. The
shared XI1 raw-capture helper also selects `DeviceButtonPress`; neither case
injects a button event.

| case id | grab and selections | Xvfb 21.1.24 result | raw result | yserver branch |
|---|---|---|---|---|
| C16 | A: XIGrabDevice on slave 4 at W, owner_events=true, Motion mask; A selects XI1 DeviceMotionNotify and XI2 Motion on W for device 4. | A: XI_Motion/6 d4/s4 W/D e=(50,50) r=(150,150); no XI1 wire packets. | `oracle/results/xvfb-task6/C16.txt` | A: same XI_Motion fields and no XI1 wire packets; XI_Leave/Enter crossings also appear. Raw: `oracle/results/yserver-branch/C16.txt`. |
| C17 | A: XIGrabDevice on slave 4 at W, owner_events=false, Motion mask; A selects XI1 DeviceMotionNotify on W for device 4. | A: XI_Motion/6 d4/s4 W/D e=(50,50) r=(150,150); no XI1 wire packets. The exclusive fallback still reports child D. | `oracle/results/xvfb-task6/C17.txt` | A: same XI_Motion fields and no XI1 wire packets; XI_Leave/Enter crossings also appear. Raw: `oracle/results/yserver-branch/C17.txt`. |

The direct XTest request goes through `XTestFakeInput` in Xorg and produces the
grabbed XI2 event even though the server-side XTest caller also has the XI1
valuator chain available. Task 4's Rust regression sends the equivalent 64-byte
FakeInput request through production request dispatch. Against Task 4's HEAD
implementation it fails because the direct caller emits no XI2 event; the
expected recipient, child, geometry, and absence of XI1 packets above come only
from C16/C17's Xvfb results.

## Round-three oracle addition: partial-axis direct FakeInput (C18)

C18 uses the C16 setup and the same `pointer-oracle` client and `run-cases.sh`.
After positioning the XTest pointer at `(150,150)`, A selects XI1
`DeviceMotionNotify` and XI2 `XI_Motion` on W for device 4, then installs an
owner-events XIGrabDevice on device 4 with a Motion mask. The client sends a
valid XTestFakeInput request containing one DeviceValuator with
`first_valuator=1`, `num_valuators=1`, and value `175`.

| case id | Xvfb 21.1.24 result | raw result | yserver branch |
|---|---|---|---|
| C18 | A: XI_Motion/6 d4/s4 W/None e=(50,-100) r=(150,0), valuator mask `0x2`, axis 1 value `0`; no XI1 wire packets. B: none. | `oracle/results/xvfb-task6/C18.txt` | A: XI_Motion/6 d4/s4 W/D e=(50,50) r=(150,150), valuator mask `0x2`, axis 1 value `0`; child and geometry are excluded from scoped comparison as documented below. Raw: `oracle/results/yserver-branch/C18.txt`. |

The measured output preserves only the requested Y valuator bit, but Xvfb emits
its value as zero even though the request carried `175`. Xorg's FakeInput parser
writes the request values at the global `base` index (`../xserver/Xext/xtest.c:248-284`),
then `valuator_mask_set_range` reads that range from zero-based array offsets
(`../xserver/dix/inpututils.c:452-462`). For C18, the input lands at index 1,
while the one-axis range copy reads index 0. This measurement means the
compatible XI2 packet has mask bit 1 and value 0. The packet's coordinates and
fallback child also reflect the resulting Xorg pointer position at `(150,0)`.

The oracle was rebuilt with `-Wall -Wextra -Werror` and run against the supplied
Xvfb 21.1.24 installation on display `:124`; `xdpyinfo` reports vendor release
`12101024`. `run-cases.sh :124 xvfb` wrote C18 alongside the existing Xvfb rows.

## Task 6 branch remeasurement (2026-11-18)

The release binary was rebuilt from branch `feat/pointer-routing-xorg` at
`ba1f031c6c9b`. Xvfb 21.1.24 reported vendor release `12101024`; yserver
reported `10900000` and its startup log confirmed llvmpipe. The runner wrote
22 files per server under `oracle/results/xvfb-task6/` and
`oracle/results/yserver-branch/`. C15's yserver placeholder was replaced by
the output of `slave_grab_leaves_master_form_from_other_source`. C16 and C17
are real client captures on both servers; each reports the expected XI_Motion
to A on W with child D, device/source 4/4, coordinates `(50,50)` / `(150,150)`
and valuators `(150,150)`, with no XI1 packets. The branch also records
XI_Leave/Enter crossings, which remain an explicit spec non-goal.

C18 retains its full measured difference in the result files: Xvfb's range
copy moves the sprite to `(150,0)` and reports `W/None` at `(50,-100)`, while
yserver's direct FakeInput device motion leaves the sprite at `(150,150)` and
reports `W/D` at `(50,50)`. Both deliver XI_Motion to A on W from device/source
4/4 and carry valuator mask `0x2`, axis-1 value `0`. The child and coordinate
fields are excluded from the scoped comparison under the coordinator ruling
in “Known differences / follow-ups” below; the remaining measured fields
match.

Load runs passed five times each with 16 busy-loop workers. `cargo test
--workspace`, the lavapipe ignored suite with the listed DRM/KMS cases skipped,
the required targeted tests, workspace Clippy, FreeBSD cross Clippy, and `just
man` passed. Fix round 2 moved the owner-events routing helpers into
`pointer_fanout/owner_events.rs` and moved the owner-events/grab-routing tests
into the existing `pointer_fanout/tests/owner_events.rs`. The resulting line
counts are 4,740/5,000 for `pointer_fanout.rs` and 5,679/6,000 for `tests.rs`,
leaving at least 100 lines of headroom without a layout exception. The final
`cargo +nightly fmt`, workspace/all-target Clippy, focused `pointer_fanout`,
`xi2`, and `grab` tests, and `tools/check-layout.sh` all passed. Mesa printed
nonfatal device-name diagnostics during the lavapipe ignored suite. No
`/dev/dri` device or input device was opened, and no Git write was run.

## C10 raw XI1 wire packets

Each `hex` value is the full 32-byte wire packet captured before libXi conversion. The decoded values are copied from that packet; the client also logs the normalized event.

### Xvfb

| index | wire event | full 32-byte hex | byte 31 / follow-up / valuators |
|---:|---|---|---|
| 0 | DeviceMotionNotify | `4700110098e4aa07420000000100200002002000910091002d002d0000000184` | byte31=`0x84`; follow-up DeviceValuator hex `4204110000000200960000009600000000000000000000000000000000000000`; axes 0,1 = 150,150 |
| 1 | DeviceValuator | `4204110000000200960000009600000000000000000000000000000000000000` | device 4; axes 0,1 = 150,150 |
| 2 | DeviceMotionNotify | `47001100fee4aa07420000000100200002002000960096003200320000000184` | byte31=`0x84`; follow-up DeviceValuator hex `42041100000002009b0000009b00000000000000000000000000000000000000`; axes 0,1 = 155,155 |
| 3 | DeviceValuator | `42041100000002009b0000009b00000000000000000000000000000000000000` | device 4; axes 0,1 = 155,155 |
| 4 | DeviceButtonPress | `4501110064e5aa074200000001002000020020009b009b003700370000000104` | byte31=`0x04`; follow-up packet none |

### yserver d8d3416b

| index | wire event | full 32-byte hex | byte 31 / follow-up / valuators |
|---:|---|---|---|
| 0 | DeviceMotionNotify | `47001100980c0000000100000100100002001000960096003200320000000104` | byte31=`0x04`; follow-up DeviceValuator hex `4204110000000200960000009600000000000000000000000000000000000000`; axes 0,1 = 150,150 |
| 1 | DeviceValuator | `4204110000000200960000009600000000000000000000000000000000000000` | device 4; axes 0,1 = 150,150 |
| 2 | DeviceMotionNotify | `47001100fe0c00000001000001001000020010009b009b003700370000000104` | byte31=`0x04`; follow-up DeviceValuator hex `42041100000002009b0000009b00000000000000000000000000000000000000`; axes 0,1 = 155,155 |
| 3 | DeviceValuator | `42041100000002009b0000009b00000000000000000000000000000000000000` | device 4; axes 0,1 = 155,155 |
| 4 | DeviceButtonPress | `45011100630d00000001000001001000020010009b009b003700370000000104` | byte31=`0x04`; follow-up packet none |

## Build and run commands

The Xvfb installation under `oracle/install` and release yserver binary were reused. The existing Xvfb build was previously produced in the scratch directory with:

```sh
meson setup /home/ariel_santangelo/Projects/claude-scratch/pointer-routing/oracle/xvfb-build /data/Projects/xorg-21.1.24 --prefix=/home/ariel_santangelo/Projects/claude-scratch/pointer-routing/oracle/install -Dxvfb=true -Dxorg=false -Dxephyr=false -Dxnest=false -Dxwin=false -Dxquartz=false -Dglamor=false -Ddri1=false -Ddri2=false -Ddri3=false -Ddrm=false -Dudev=false -Dudev_kms=false -Ddocs=false -Ddevel-docs=false -Dxselinux=false -Dxcsecurity=false -Dxf86bigfont=false -Dglx=false -Dxv=false -Dxvmc=false
ninja -C /home/ariel_santangelo/Projects/claude-scratch/pointer-routing/oracle/xvfb-build install
```

The XI1/C client was rebuilt under the oracle directory:

```sh
cc -std=c11 -O2 -Wall -Wextra -Werror -I/usr/include /home/ariel_santangelo/Projects/claude-scratch/pointer-routing/oracle/clients/pointer-oracle.c -o /home/ariel_santangelo/Projects/claude-scratch/pointer-routing/oracle/clients/pointer-oracle $(pkg-config --cflags --libs x11 xi xtst)
```

`crates/yserver/src/launch.rs` confirms `--headless` is a parsed flag and means no display device, DRM, or input. The only yserver launch arguments were `:98 --headless`. The lavapipe ICD was set through the environment:

```sh
/home/ariel_santangelo/Projects/claude-scratch/pointer-routing/oracle/install/bin/Xvfb :97 -screen 0 1024x768x24
VK_DRIVER_FILES=/home/ariel_santangelo/Projects/claude-scratch/yserver-pr1/lvp/lvp_icd.json YSERVER_ALLOW_SOFTWARE_VULKAN=1 target/release/yserver :98 --headless
target/release/yserver --version
xdpyinfo -display :97 | rg 'name of display|vendor string|vendor release number|version number'
xdpyinfo -display :98 | rg 'name of display|vendor string|vendor release number|version number'
/home/ariel_santangelo/Projects/claude-scratch/pointer-routing/oracle/run-cases.sh :97 xvfb
/home/ariel_santangelo/Projects/claude-scratch/pointer-routing/oracle/run-cases.sh :98 yserver-d8d3416b yes
```

The actual launches ran in the background with SIGINT cleanup traps; the executable arguments were exactly as shown. The runner wrote 19 result files per server. Xvfb reported `12101024`, yserver reported `10900000`, and yserver startup logged llvmpipe. Final process, socket, and lock-file checks found no Xvfb or yserver server left running on `:97` or `:98`. No `/dev/dri` or input device was opened. No git write command was run.

## Result files and source appendix

The complete per-case logs, including initial pointer readings, pre-positioning, grab status, crossings, normalized events, and C10 raw packets, are under `oracle/results/xvfb/` and `oracle/results/yserver-d8d3416b/`. The C15 yserver result names the Task 4 producer test. The client and one run script used for these measurements are inlined below.

### pointer-oracle.c

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
    int xi1_motion_type;
    int xi1_button_press_type;
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
    if (s->child_owned_by_b) XSync(s->a, False);
    Display *child_owner = s->child_owned_by_b ? s->b : s->a;
    s->child = XCreateSimpleWindow(child_owner, s->grab, 20, 20, 180, 180, 0,
                                   BlackPixel(s->a, s->screen), WhitePixel(s->a, s->screen));
    s->outside = XCreateSimpleWindow(s->a, s->root, 550, 100, 300, 300, 0,
                                     BlackPixel(s->a, s->screen), WhitePixel(s->a, s->screen));
    XStoreName(s->a, s->grab, "W");
    XStoreName(child_owner, s->child, "D");
    XStoreName(s->a, s->outside, "OUT");
    XMapWindow(child_owner, s->child);
    XMapWindow(s->a, s->grab);
    XMapWindow(s->a, s->outside);
    XSync(s->a, False);
    XSync(s->b, False);
    g_setup = s;
    Window pointer_root = None, pointer_child = None;
    int root_x = 0, root_y = 0, window_x = 0, window_y = 0;
    unsigned int pointer_mask = 0;
    Bool same_screen = XQueryPointer(s->a, s->root, &pointer_root, &pointer_child,
                                     &root_x, &root_y, &window_x, &window_y,
                                     &pointer_mask);
    printf("CASE %s display=%s master=%d xtest_pointer=%d xvfb_mouse=%d root=0x%lx W=0x%lx D=0x%lx OUT=0x%lx window_owners=W:A,D:%s,OUT:A\n",
           case_id, display_name, s->master, s->xtest_pointer, s->xvfb_mouse,
           s->root, s->grab, s->child, s->outside,
           s->child_owned_by_b ? "B" : "A");
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
    printf("SELECT XI1 client=B window=W device=%d events=DeviceMotionNotify|DeviceButtonPress types=%d,%d valuator_type=%d wire_hooks=DeviceMotionNotify|DeviceButtonPress|DeviceValuator\n",
           s->xtest_pointer, motion_type, press_type, s->xi1_valuator_type);
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

### run-cases.sh

```bash
#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 2 || $# -gt 3 ]]; then
  echo "usage: $0 DISPLAY LABEL [skip-c15]" >&2
  exit 2
fi

DISPLAY_NAME=$1
LABEL=$2
SKIP_C15=${3:-no}
SCRIPT_DIR=$(cd -- "$(dirname -- "$0")" && pwd)
CLIENT="$SCRIPT_DIR/clients/pointer-oracle"
OUTDIR="$SCRIPT_DIR/results/$LABEL"
mkdir -p "$OUTDIR"
CASES=(C1 C2 C3 C4 C4b C5 C6 C7 C7s C8 C8c C9 C9e C10 C11 C12 C13 C14 C15)

for case_id in "${CASES[@]}"; do
  result="$OUTDIR/$case_id.txt"
  if [[ "$case_id" == C15 && "$SKIP_C15" == yes ]]; then
    cat > "$result" <<'RESULT'
NOT RUN ON YSERVER: headless yserver has no second pointer source. Task 4 Rust test producing this row: slave_grab_leaves_master_form_from_other_source.
RESULT
    continue
  fi
  "$CLIENT" "$DISPLAY_NAME" "$case_id" > "$result" 2>&1
  if [[ "$case_id" == C8c && "$LABEL" == xvfb ]]; then
    if ! rg -q 'EV client=A type=ButtonPress' "$result"; then
      echo "C8c stop condition: Xvfb did not deliver the core ButtonPress to A" >&2
      exit 3
    fi
  fi
done
printf 'Wrote %s case results under %s\n' "${#CASES[@]}" "$OUTDIR"
```
