# Findings report: dynamic XInput registry spec and plans

**Reviewer:** Claude Code, `claude-opus-5-5`, effort `high` (confirmed by CLI model usage). **Scope:** read-only plan and source review; no implementation tests run.

I checked the plans against the yserver code at `9b93ff08`, the Xorg source in `../xserver`, `../xorgproto`, the `input` 0.10.0 crate source and `/usr/include/libinput.h`. I did not read xf86-input-libinput, GTK, MATE or KDE sources. Any claim that depends on them is marked **unverifiable**.

Where this report cites plan lines, **KP** means `docs/superpowers/plans/2026-09-29-dynamic-xinput-keyboard-pointer.md` and **TP** means `docs/superpowers/plans/2026-09-29-dynamic-xinput-touch.md`.

---

## BLOCKER

### B1. After a VT switch, every device gets a new source ID the core never learns, so input dies
- **Plan:** KP:71-91 (Tasks 2–3). No task implements spec:118-119 ("Suspend/resume … rebuild the registry"). TP:30 and TP:137 assume a suspend hook exists; it does not.
- **Verified:**
  - While paused, the input thread discards libinput dispatch output: `crates/yserver/src/input_thread.rs:861-864` (`let _ = input_ctx.dispatch(); continue;`).
  - Right after resume it discards it again: `input_thread.rs:830` (`let _ = input_ctx.dispatch();`).
  - `libinput.h:3930-3951`: resume "adds existing devices" and suspend "closes existing devices".
  - `input::Device` equality and hashing are by C pointer (`input-0.10.0/src/lib.rs:188-198`).
- **Failure (inference, strongly supported):**
  1. Task 2's handle-keyed `SourceTracker` inside `Context::dispatch` retires the old sources and creates new ones on resume.
  2. The Removed/Added events are thrown away by the input thread, so the core registry keeps the dead sources and their held keys and buttons.
  3. Every event after resume carries an unknown `InputSourceId`. The spec (line 108) says those are dropped, so keyboard, mouse and property writes all stop after the first VT switch.
- **Smallest fix:**
  - Add a task in Tasks 2–3 so the removal events from suspend and the re-add burst after resume reach the core.
  - Alternatively, on `Pause` emit `DeviceRemoved` for every tracked source and forward the resume dispatch instead of discarding it.
  - Test: pause, resume, then a key event is attributed to a registered source and no old facet remains.

## HIGH

### H1. The KMS backend cannot report whether a property write succeeded, so "commit only after backend success" can't be built as written
- **Plan:** KP:130 and KP:132 (Task 7: `backend_error_does_not_commit`, "returns an XInput device error if the source disappeared"). Spec:149-151.
- **Verified:**
  - `kms/render/backend.rs:21282-21299` only queues the change for the input thread and always returns `Ok(())`. libinput's rejection is just logged (`input_thread.rs:798-805`).
  - `dispatch_change_property` (`process_request.rs:24021-24101`) never checks that the target device actually advertises the libinput property. A write to a keyboard facet or to a pointer without acceleration would commit and create the property.
- **Failure:** Task 7's tests pass only on `RecordingBackend`. In production, an unsupported write, or one to a source that is being removed, commits a false value to the XI property.
- **Smallest fix:**
  - State in Task 7 that KMS success means "queued".
  - Before committing, the core returns BadMatch for any libinput-descriptor property the target facet does not currently have.
  - It validates the value against that facet's config snapshot, for example whether the requested acceleration profile is available.
  - Queue writes keyed by `InputSourceId` and drop them on the input thread if the source is gone.

### H2. XI1 device checks are hardcoded to IDs 2–5, so devices 6+ are listed but can't be opened
- **Plan:** KP:104-113 (Task 5 publishes 6+). Only Tasks 9 and 11 touch XI1 "open/grab", and only through a vague "audit".
- **Verified:**
  - `process_request.rs:16425-16461` hardcodes the helpers: `xi1_device_valid` is `2..=5`, and `has_keys`, `has_buttons` and `has_valuators` are `3|5` / `2|4`. They are used at about 70 call sites plus `xi1_state_notify.rs:65-67`.
  - `XOpenDevice` returns BadDevice for any ID above 5 (`process_request.rs:19005`). It decides "keyboard" only by `id==3||id==5` (`process_request.rs:19061`).
  - The code comment at `process_request.rs:19086-19093` records that an `OpenDevice` reply contradicting `ListInputDevices` caused a Chromium fatal CHECK.
- **Failure:** from Task 5 onward, `XOpenDevice(6)` returns BadDevice, and a keyboard facet would be given pointer classes. The MATE flow (XListInputDevices → XOpenDevice → XGetDeviceProperty) fails. The Chromium crash class could return (inference).
- **Smallest fix:**
  - In Task 5, make these checks take the registry: validity, key/button/valuator classes, `OpenDevice` class set by facet role, and state-notify.
  - Test: every ID returned by XListInputDevices can be opened with matching classes.

### H3. XI2 grab and allow paths decide keyboard vs pointer by `deviceid == 3`, and passive grabs don't store a device
- **Plan:** KP:152-156 (Task 9) and KP:174-178 (Task 11). The audit wording "hardcode 4/5" (KP:156, KP:233, spec:155-161) would not find these.
- **Verified:**
  - `process_request.rs:17915-18248` and `27436-27437` (`let kbd = deviceid == 3`) treat every device other than 3 as a pointer. A physical keyboard facet would therefore get a pointer grab.
  - `XIPassiveGrabDevice` (`process_request.rs:18384-18431`) never stores `deviceid`. Every XI2 passive grab is effectively core-wide.
  - XI1 freeze pairing is hardcoded 4↔5 (`pointer_fanout.rs:2296-2304`).
  - In Xorg, an XI2 grab on a slave detaches that slave from its master for the grab's duration (`dix/events.c:1621-1624`, `1742-1745`).
- **Failure:** Task 9's test ("a grab for Razer never receives HyperX events") needs a device field on `PassiveButtonGrab`/`KeyGrab` and device matching in the core grab-activation path. Neither is in the task's interface. The implementer either gets stuck or tests only the delivery filter.
- **Smallest fix:**
  - Add explicit steps:
    - look up device role in the registry for XI2 grab, ungrab, allow and focus;
    - add `device_id` to passive grab records and match on it;
    - replace the 4↔5 pairing;
    - state whether yserver implements Xorg's detach-during-grab or deliberately deviates.
  - Split Task 9 into grabs and selection routing.

### H4. Switching between mice never produces Xorg's "slave switch" DeviceChanged, and the scroll value is shared across devices
- **Plan:** KP:141-146 and KP:192-201. Task 13's `emit_xi2_device_changed` only covers class changes. libinput capabilities don't change at runtime, so that case rarely occurs.
- **Verified:**
  - Xorg sends `XI_DeviceChanged` with reason SlaveSwitch on the master whenever the source slave changes (`dix/getevents.c:686-709`).
  - yserver already fakes one at XISelectEvents time with the source hardcoded to 4 (`process_request.rs:33220-33257`).
  - The scroll value `state.scroll_axis_value` is global and appears in every class block (`fanout.rs:555`, `process_request.rs:33229`).
- **Inference (GTK not available locally):** GDK tracks scroll-valuator deltas per source device and resets them on DeviceChanged. With per-slave `sourceid`, a global scroll value and no SlaveSwitch, the first wheel tick after switching mice would jump.
- **Smallest fix:**
  - Add a task that records each master's last slave.
  - Before the first event from a different slave, emit DeviceChanged (master ID as deviceid, the new slave as sourceid, reason SlaveSwitch, the slave's classes).
  - Keep scroll valuator values per facet.
  - Make the bootstrap use the real last slave instead of 4.

### H5. SelectExtensionEvent currently rejects MATE's DevicePresence class with BadClass
- **Plan:** KP:207 and KP:211 (Task 14 "stores the special class").
- **Verified:**
  - `xi1_event_class_device` computes `(class>>8)&0xff`, which is `0` for `0x1000F` (`process_request.rs:16471-16473`).
  - The loop at `18623-18637` therefore returns BadClass before any storage code runs.
  - `dev_byte` truncation (`18652`) makes the class collide with device 0.
  - Xorg strips device-256 classes *before* validating them (`Xi/selectev.c:68-112,158`).
- **Failure:** an implementer adds storage in the accept loop, which is never reached. MATE never gets presence events.
- **Smallest fix:** Task 14 must require Xorg's pre-pass: take out `class>>8 == 256` entries, record the presence selection per window and client, then validate the rest.

### H6. Each task runs only its new tests, so existing tests that assume device 4 are never re-run
- **Plan:** KP:22 and TP:22 ("Write only the focused checks … then run those checks"). Every Step 2/4 uses a filtered `--lib` run.
- **Verified:** dozens of existing tests assert device 4/5 behaviour, for example `process_request.rs:40625-44017`, `pointer_fanout.rs:3922`, and the key_fanout tests. `clippy --all-targets` compiles them but does not run them. No task runs `crates/yserver/tests/*.rs`.
- **Failure:** assertion regressions pass every per-task review and only surface at the end.
- **Smallest fix:**
  - Per task: `cargo test -p yserver-core --lib`, `cargo test -p yserver --lib`, and each integration test file.
  - Add your standing A/B/C real-path clauses to both plans' contracts.

### H7 (touch). Touch delivery doesn't follow Xorg's single-listener rule, and touch selection rules are missing
- **Plan:** TP:104 (Task 5 delivers to all of XIAllDevices, master and exact-slave masks), TP:126 (emulation "for a client without XI2 touch selection"), TP:39 (Review Focus 4).
- **Verified:**
  - Xorg chooses exactly one regular listener per touch sequence (`dix/touch.c:737-875`): the first window, deepest first, with a deliverable selection. Touch selections win over pointer selections, and only the first client is taken.
  - Only the first touch emulates the pointer (`touch.c:147-160`).
  - XISelectEvents must require all three touch bits (BadValue) and allow only one touch selector per window/device (BadAccess) (`Xi/xiselectev.c:216-271`). yserver implements neither.
- **Failure:** events go to several clients, a pointer-only client on a touch-selected window gets duplicate clicks, and clients can make selections Xorg would reject.
- **Smallest fix:**
  - Task 5 builds the listener list at TouchBegin following `TouchSetupListeners`.
  - Add the touch XISelectEvents validation to Task 5.
  - In Task 7, emulation is a listener type decided by the window walk, not a per-client choice.

### H8 (touch). Passive touch grabs are not implemented, yet Task 6 tests them
- **Plan:** TP:113-117 (Task 6 touches only XIAllowEvents).
- **Verified:** `XIPassiveGrabDevice` with grab type 4 (TouchBegin) is logged and ignored (`process_request.rs:18432-18441`). `XIGrabtypeTouchBegin=4` is confirmed in `XI2.h:87`.
- **Failure:** the "passive-grab accept/reject/transfer" tests can't be written.
- **Smallest fix:** add TouchBegin passive grab and ungrab (scoped per device, see H3) to Task 6, or put it in a task of its own before Task 6.

### H9 (touch). The touch axis model is undefined
- **Plan:** TP:58 (no valuator classes defined for touch facets), TP:92-94 (keep the `encode_xi2_device_event` signature; raw touch carries libinput `x()/y()` in millimetres).
- **Verified:**
  - The device-event encoder takes `i16` root coordinates and only includes axes for `evtype == 6` (`yserver-protocol/src/x11/mod.rs:1962-1998`).
  - The raw encoder takes `i32` values (`mod.rs:2231-2243`), so fractional millimetres are lost.
  - `XI2proto.txt:1088`: touch events use "the same axes as pointer events". For a pointer+touch source, valuators 0/1 cannot be both relative and absolute.
- **Unverifiable:** which axis range xf86-input-libinput advertises.
- **Smallest fix:**
  - In the spec or TP Task 1, define the touch facet's axes, e.g. "Abs MT Position X/Y", Absolute mode, with a fixed range filled from `x_transformed`.
  - Encode raw events in the same units.
  - Decide how a mixed pointer+touch source is laid out.

## MEDIUM

### M1. Task 2 cannot compile without editing a file it is not allowed to touch
- **Plan:** KP:73-75 (the file list omits `input_thread.rs`).
- **Verified:** `input_thread.rs:495-510` destructures `InputEvent::DeviceRemoved { device_node }`. `map()` matches `InputEvent::KeyPress { keycode }` and similar without `..` (`input_thread.rs:122-197`). Adding fields breaks both.
- **Also:** Task 2's variant list omits `PointerMotionAbsolute`.
- **Smallest fix:** allow Task 2 to edit `input_thread.rs`, carrying `source_id` plus the node until Task 3, or merge Tasks 2 and 3.

### M2. Task 6 removes functions that code owned by Task 16 still calls
- **Plan:** KP:119 (Task 6 removes `xi_seed_touchpad`/`xi_clear_touchpad`), KP:233 (Task 16 replaces the reset replay).
- **Verified:** `reset.rs:420-422` and `backend/recording.rs:1281` call them.
- **Smallest fix:** Task 6 switches the reset loop and the recording probe to `xi_register_source`. Task 16 keeps the reset tests and the audit.

### M3. Hotplug notifications differ from Xorg's sequence
- **Plan:** KP:196 and KP:207 (`Enabled|Removed` only; "one hierarchy event").
- **Verified:**
  - Xorg sends presence DeviceAdded then DeviceEnabled on hotplug, and DeviceDisabled then DeviceRemoved on unplug (`dix/devices.c:616, 428, 547, 1256`).
  - Each step also sends a hierarchy event (XISlaveAdded, XIDeviceEnabled, XIDeviceDisabled, XISlaveRemoved) whose `info[]` lists **all** devices plus the removed ones (`Xi/xichangehierarchy.c:61-121`).
- **Unverifiable:** whether MATE reacts only to DeviceEnabled.
- **Smallest fix:** emit Xorg's four-step sequence and full-list hierarchy events.

### M4. An invalid `YSERVER_MOUSE_ACCEL_PROFILE` produces a misleading "no input" abort
- **Plan:** KP:218 (the parser is called by `Context::new()`).
- **Verified:** a `Context::new()` error becomes a warning plus `None` (`kms/backend.rs:1062-1069`). Startup then aborts through the "no input devices" path (`lib.rs:769-775`). The allowed-values diagnostic required by spec:172-173 is lost.
- **Smallest fix:** parse the variable at launch, before platform init, and fail with its own error.

### M5. Virtual 4/5 are under-specified
- **Plan:** KP:67 ("named XTEST slaves", no names given), KP:33.
- **Verified:**
  - Xorg names them "Virtual core XTEST pointer/keyboard" and sets a non-deletable `XTEST Device` property (INTEGER/8 = 1); writes to it return BadAccess (`Xext/xtest.c:589-639`).
  - A yserver comment says device 4 must "look like a generic attached pointer, not like XTEST" for GTK/GDK (`process_request.rs:17362-17366`). The plan reverses this without addressing it.
  - Nested host input also uses `None` and would therefore be labelled XTEST, which conflicts with "nested retains behaviour" (KP:33).
- **Smallest fix:** fix the names and property in the plan, choose how nested input is attributed, and confirm or delete the GTK claim.

### M6. Task 8 misstates how raw events carry device IDs
- **Plan:** KP:141 and KP:145.
- **Verified:** Xorg delivers both raw and device events in two forms: a slave form (deviceid = sourceid = slave) and a master form (deviceid = 2, sourceid = slave) (`dix/events.c:2475-2522`). The existing test `pointer_fanout.rs:3922` asserts the master form.
- **Smallest fix:** restate Task 8 in those terms.

### M7. After a server reset, XI properties show add-time values instead of client-changed ones
- **Plan:** KP:229 (Task 16).
- **Verified:** the inventory stores the add-time snapshot (`input_inventory.rs:72`) and reset replays it (`reset.rs:420`). The libinput device keeps the values clients wrote later.
- **Smallest fix:** on a successful write, update the source's inventory config, which needs a path from the core to the `run_core` local, or say explicitly that this is accepted.

## LOW

- **L1.** XIQueryDevice ignores the requested ID and writes the device list little-endian regardless of client byte order (`process_request.rs:17124-17402`, `fanout.rs:482-534`). KP:101 "preserve … client byte order" preserves a bug. It should state BadDevice for unknown IDs and byte order per client.
- **L2.** XTEST fake events that name an XI device are stamped as 4 (`process_request.rs:9765-9786`). Xorg posts them from the named device (`xtest.c:208-235`). Neither the audit nor KP:33 covers this.
- **L3.** Test filter mismatch in Task 5: KP:110 names the test `xi1_dynamic_list`, but KP:111 runs `cargo test -p yserver-core xi_dynamic_list`.
- **L4.** Task 1 adds `DeviceInfo` fields, which forces changes to about 25 constructors, including `crates/yserver/src/input/context.rs:305`, which is not in Task 1's file list. The plan should specify the placeholder until Task 2.
- **L5.** `xXITouchInfo.num_touches` is one byte; TP:58 should clamp `touch_count()` to 255.
- **L6.** Pointer facets without acceleration (Consumer Control, System Control) will appear without `libinput Accel Speed`. `server.rs:1740-1752` records an earlier KDE KCM crash tied to that property. Whether the KCM walks all pointers is **unverifiable** locally, so this should get a hardware check.
- **L7.** KP has 16 tasks, which per your measured size-to-defect data is at risk. H3 and H4 add scope. Splitting KP into registry/query (Tasks 1–7) and routing/notifications (Tasks 8–16) is pre-approved under your rule.


---

## Corrections applied to the plans — 2026-09-30

**Author:** Codex. This section records the response to the original review; it is not a second approval by Opus. The original findings and their old line/task references above are retained. The revised plans contain 18 keyboard/pointer tasks (KP) and 10 touch tasks (TP). Only documentation changed; implementation and hardware behavior have not been verified.

| Finding | Disposition and revised task |
| --- | --- |
| B1 | KP 3 forwards VT suspend removals and resume additions before new input; its regression covers continued keyboard/pointer routing. |
| H1 | KP 7 validates facet existence/support; KP 8 adds input-thread acknowledgment before XI commit. The suggested definition of success as merely queued was rejected because it violates the approved apply-before-commit contract. |
| H2 | KP 5 migrates XI1 validity/classes/OpenDevice/state-notify and checks that every listed device opens consistently. |
| H3 | KP 11 adds device-indexed active/passive grabs, dynamic role/pair lookup, floating-slave delivery and reattachment; KP 13 completes keyboard delivery checks. |
| H4 | KP 10 stores per-source scroll values; KP 15 adds SlaveSwitch, dynamic bootstrap and per-source classes. |
| H5 | KP 16 extracts device-256 presence classes before ordinary XI1 validation and truncation. |
| H6 | Both contracts run existing core/server/protocol suites per task and integration suites at explicit milestones; final tasks run the full workspace suite. No undocumented A/B/C clauses were invented. |
| H7 | TP 5 implements selection triplet/conflict validation; TP 6 builds one regular listener per device form; TP 9 restricts pointer emulation to the chosen listener. Raw subscriptions remain separate. |
| H8 | TP 7 implements passive TouchBegin grab/ungrab before TP 8 ownership tests depend on them. |
| H9 | TP 1 fixes the shared absolute axis model using official driver source; TP 2 uses transformed native units; TP 4 preserves FP fractions and raw End's empty mask. Mixed pointer/touch motion is checked against the advertised axes. |
| M1 | KP 2 includes input_thread mapping and absolute pointer events; KP 3 completes core-channel origin propagation. |
| M2 | KP 6 migrates reset and recording callers when removing the old seed/clear helpers; KP 18 audits replay and reset. |
| M3 | KP 14–16 specify Added/Enabled and Disabled/Removed, full hierarchy info, transition visibility, removed snapshots and publication before ID reuse. |
| M4 | KP 17 parses the global default in startup and passes its typed value through platform/input initialization; the file hint is corrected to kms/backend.rs. |
| M5 | KP 1 gives exact XTEST names and immutable marker; KP 3/9/12 distinguish Physical, XTest(target) and NestedHost origins. KP 18 requires the GTK/GDK client check rather than treating the old comment as evidence. |
| M6 | KP 9/12 explicitly deliver slave and master raw/device forms with the physical facet sourceid, subject to attachment and masks. |
| M7 | KP 8 updates registry and atom-free inventory after confirmed writes, including completion after disconnect/reset; KP 18 verifies current values survive reset. |
| L1 | KP 4 fixes exact-ID lookup, BadDevice and both byte orders. |
| L2 | KP 3/9 preserve an explicit valid XTEST target; it is not overwritten with 4. |
| L3 | KP 5 aligns xi1_dynamic_list test names/filters and adds xi1_dynamic_open. |
| L4 | KP 1 and TP 1 include all affected DeviceInfo constructors and explicit placeholder metadata. |
| L5 | TP 1 clamps advertised touch count to 255. Official driver unknown-count fallback is 15; the former generic-Xorg fallback of five was corrected. |
| L6 | KP 18 includes KDE KCM with acceleration-less pointer facets as a real-client verification item. Its behavior is still unverified; no fictitious acceleration property is added. |
| L7 | The claimed numeric size threshold and pre-approval rule are not present in AGENTS.md or the approved spec. The plan keeps two subsystem documents, explicitly separates KP's registry/config and event/integration stages, splits overloaded property/grab/touch steps, and dispatches fresh extracted task briefs with review gates. Task count alone is not accepted as evidence of a defect. |

The spec records the Xorg-grounded lifecycle, XTEST and touch clarifications. Original user decisions remain: masters 2/3, virtual 4/5, physical facets 6..127, keyboard/pointer/touch, and no exact selector. Hardware checks for GTK, KDE, MATE and touch remain implementation acceptance work; this correction does not claim those clients already work.
