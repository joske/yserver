> **DROPPED 2026-10-02 (user decision):** direct-touch devices (touchscreens, touch tablets) are out of scope; the intended scope is mice, keyboards and laptop touchpads, all covered by the keyboard/pointer plan. Kept for history only; do not implement.

# Dynamic XInput Touch Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Complete the approved dynamic XInput spec by registering libinput touch sources and delivering XI2 touch and raw touch events with stable contact IDs and Xorg-compatible pointer emulation.

**Architecture:** The registry from the keyboard/pointer plan gives a touch-only source one pointer/touch facet, or adds touch classes to its existing pointer facet. A touch tracker maps libinput slots to contact IDs per `InputSourceId`; the KMS backend sends those contacts through XI2 selection, grab, ownership, and pointer-emulation paths. No touch event may borrow the identity of another source or of virtual device 4.

**Tech Stack:** Rust workspace (`yserver`, `yserver-core`, `yserver-protocol`), libinput touch events via the `input` crate, XI2.2+ touch protocol, Xorg `dix/getevents.c` and `Xi/exevents.c` as behavior references.

**Spec:** `docs/superpowers/specs/2026-09-29-dynamic-xinput-device-registry-design.md`. Depends on completion of `2026-09-29-dynamic-xinput-keyboard-pointer.md`.

**Executor:** One implementation subagent per numbered task, using `gpt-6-luna` with reasoning effort `xhigh`. The parent supplies this plan, the approved spec, the task number, and the completed keyboard/pointer handoff. Do not use the conversation as the only task brief. Each task ends with a commit and a handoff naming changed interfaces, exact verification output, and unresolved issues. A fresh task reviewer checks spec compliance and code quality before the parent dispatches the next task; a separate whole-branch review follows Task 10.

**Dispatch parameters:** `spawn_agent(fork_turns="none", model="gpt-6-luna", reasoning_effort="xhigh", task_name="xi_touch_<task_number>", message="Read AGENTS.md, the approved spec, both implementation plans, and implement only touch Task <task_number>. Prior handoff: <summary>. Report changed interfaces and exact verification results.")`. Supply the real task number and prior handoff; use the skill's `task-brief` extractor and review loop.

## Subagent execution contract

- Implementation is authorized by the user on 2026-09-30. Further Opus reviews remain canceled. The companion plan closes local reset/VT findings in Tasks 3 and 18; this plan retains its task-worker and task-review method.
- Start only after the keyboard/pointer plan's Task 18 has landed. Execute touch Tasks 1–10 in order; each numbered heading is a separate dispatch and review gate.
- Read `AGENTS.md`, the approved spec, this plan's Global Constraints, the assigned task, and the listed code. Treat line numbers as search hints.
- Use the approved feature branch or its isolated `feat/xi-dynamic-registry-implementation` execution worktree, preserve the registry and source contracts from the first plan, and leave unrelated untracked files untouched.
- Write the assigned task's focused checks, confirm they fail, and implement. Then run focused checks plus `cargo test -p yserver-core --lib`, `cargo test -p yserver --lib`, `cargo test -p yserver-protocol --lib`, `cargo +nightly fmt`, and `cargo clippy --all-targets -- -D warnings` before committing. At Tasks 6, 8, and 10 also run `cargo test -p yserver --tests --locked`; the final task runs the full workspace suite. Stop and report any Xorg wire/behavior mismatch rather than guessing.

## Global Constraints

- Physical touch facets use IDs 6..=127 and attach to master pointer 2; 4 remains virtual XTEST.
- A source with both pointer and touch capability has one XI pointer/touch facet; keyboard capability adds a separate keyboard facet.
- Touchpad gesture input does not imply `XITouchClass`; only libinput `Touch` capability does.
- XI1 `TOUCHSCREEN` and XI2 `TouchClass` describe the same live source; XI1 and XI2 still enumerate identical IDs/names.
- Contact identity is per source, survives Down/Motion/Up, and ends on Cancel, device removal, suspend, or reset.
- Existing pointer, scroll, grab, and touchpad behavior remains usable.
- Before each implementation commit: `cargo +nightly fmt` and `cargo clippy --all-targets -- -D warnings`, as required by `AGENTS.md`.

## Review Focus

1. Touchscreen with keyboard and pointer capabilities: one pointer/touch facet plus one keyboard facet; Task 1 checks identity and class count.
2. Two screens reusing the same libinput slot number: their contacts remain distinct; Task 3 checks source-scoped IDs.
3. Cancel, unplug, or VT suspend during an active contact: no stuck touch or emulated button; Tasks 3 and 10 check cleanup.
4. XI2 touch selector and ordinary pointer-only client on the same window: touch and emulated pointer delivery follow Xorg rules without duplicate clicks; Tasks 5, 6, and 9 check this.
5. Passive grab owner rejects a touch: ownership transfers or ends according to `XIAllowEvents`, rather than hanging the contact; Tasks 7 and 8 check this.

---

## File map and order

| Responsibility | Files |
| --- | --- |
| Touch capability, classes and XI1 type | `crates/yserver/src/input/context.rs`, `crates/yserver-core/src/core_loop/message.rs`, `crates/yserver-core/src/xinput/registry.rs`, `crates/yserver-core/src/xinput/query.rs`, `crates/yserver-core/src/core_loop/process_request.rs`, `crates/yserver-protocol/src/x11/mod.rs` |
| Libinput contacts and source-safe tracker | `crates/yserver/src/input/context.rs`, `crates/yserver/src/input/event.rs`, `crates/yserver/src/input_thread.rs`, new `crates/yserver-core/src/xinput/touch.rs` |
| XI2 touch wire and fanout | `crates/yserver-protocol/src/x11/mod.rs`, new `crates/yserver-core/src/core_loop/touch_fanout.rs`, `crates/yserver/src/kms/render/backend.rs` |
| Touch selection, ownership and emulation | `crates/yserver-core/src/core_loop/process_request.rs`, `crates/yserver-core/src/core_loop/pointer_fanout.rs`, `crates/yserver-core/src/core_loop/touch_fanout.rs` |
| Cleanup and status | `crates/yserver-core/src/core_loop/reset.rs`, `crates/yserver/src/input/context.rs`, `docs/status.md` |

### Task 1: Publish touch-capable facets and one consistent axis model

**Files:** Modify `crates/yserver/src/input/context.rs`, `crates/yserver-core/src/core_loop/message.rs` and all `DeviceInfo` constructors, `crates/yserver-core/src/xinput/registry.rs`, `crates/yserver-core/src/xinput/query.rs`, `crates/yserver-core/src/core_loop/process_request.rs`, `crates/yserver-core/src/core_loop/pointer_fanout.rs`, `crates/yserver/src/kms/render/backend.rs`, `crates/yserver-protocol/src/x11/mod.rs`.

**Interfaces:** `DeviceInfo` gains `max_touches: Option<u32>` from `Device::touch_count()`; `None` or zero means unknown. `XiRegistry::register` creates `PointerTouch` for `capabilities.touch` even without `pointer`. `XiDevice` stores direct-touch metadata and the resolved advertised contact count, clamped to 255 for the `u8` wire field. Unknown counts use 15, matching xf86-input-libinput 1.5.0's `TOUCH_MAX_SLOTS`, rather than confusing Xorg's generic allocation fallback of five with this driver's policy.

A touch-capable facet advertises axes 0/1 as `Abs MT Position X`/`Abs MT Position Y`, Absolute, minimum 0, maximum 65535, resolution 0. It uses these same axes for touch and pointer events; never add another relative X/Y pair with the same axis numbers. Retain both horizontal/vertical ScrollClasses at axes 2/3 from pointer initialization on a mixed pointer+touch source, as xf86-input-libinput sets both unconditionally; libinput has no per-axis support query. For mixed relative pointer input, follow Xorg's absolute-axis movement conversion: deltas are driver-native device units, integrated/rescaled through GetPointerEvents/moveRelative/positionSprite; do not preserve pointer-only pixel speed on a touch-axis device. Encode ordinary Motion valuators as the resulting device-range position. The master-form event retains the identical valuator values and classes, as CopyGetMasterEvent copies the slave payload; only root/event coordinates use root pixels. Touch updates carry the contact's own absolute position, separately from cursor position. Query classes, master SlaveSwitch classes, and event values must agree on these modes and units. A touch-only facet has ButtonClass with 7 buttons, four valuators (0/1 Abs MT; 2/3 Relative with Rel Horiz Scroll/Rel Vert Scroll labels and default min/max/resolution), no ScrollClass, and TouchClass, matching the driver's initialization. A combined pointer/touch facet retains both pointer-initialized ScrollClasses on 2/3.

**Grounding:** Official [xf86-input-libinput 1.5.0 source archive](https://www.x.org/releases/individual/driver/xf86-input-libinput-1.5.0.tar.xz), `src/xf86libinput.c`: `xf86libinput_init_touch`, `xf86libinput_init`, `xf86libinput_handle_touch`, `xf86libinput_handle_motion`. The driver initializes touch after pointer on a combined device, so touch installs the absolute X/Y axes. Local `../xserver/dix/getevents.c` defines the corresponding raw/slave/master coordinate conversion. Use the archive or upstream checkout as a reference, not a new runtime dependency.

- [ ] **Step 1:** Add `xi_touch_query` assertions: touch-only gives one slave attached to 2 with XI1 `TOUCHSCREEN`, XI2 TouchClass type 8, mode XIDirectTouch, matching source ID and length; pointer+touch gives one facet; keyboard+touch gives two; touchpad without Touch gives no TouchClass. `xi_touch_axis_model` asserts absolute X/Y labels/range, contact count 15 for unknown and 255 for a count above 255, unchanged pointer-only axes, the exact touch-only class list, master/slave identical native valuators, and mixed pointer/touch values and cursor movement using Xorg's absolute-axis conversion.
- [ ] **Step 2:** Run `cargo test -p yserver-core xi_touch_query --lib`, `cargo test -p yserver-core xi_touch_axis_model --lib`, and `cargo test -p yserver-protocol xi_touch_class --lib`; confirm failure before implementation.
- [ ] **Step 3:** Encode `xXITouchInfo` and valuator classes using `XI2proto.h`/Xorg `Xi/xiquerydevice.c`; intern `TOUCHSCREEN` and the axis labels at server initialization. Extend the registry/query/class-copy paths and mixed Motion conversion using the fixed model above.
- [ ] **Step 4:** Run focused and contract regression checks, format, CI clippy; commit `feat(xinput): publish touch facets with consistent absolute axes`.

### Task 2: Translate libinput touch into source-tagged core messages

**Files:** Modify `crates/yserver/src/input/context.rs:530`, `crates/yserver/src/input/event.rs`, `crates/yserver/src/input_thread.rs:436`, `crates/yserver-core/src/core_loop/message.rs:230`, `crates/yserver-core/src/core_loop/mod.rs`, `crates/yserver/src/kms/render/backend.rs:19046`.

**Interfaces:** Define `TouchPhase = Down|Motion|Up|Cancel` and `TouchPosition { x_norm: f64, y_norm: f64 }` in `core_loop/message.rs`, re-exported by `core_loop/mod.rs`. Add `InputEvent::Touch { source_id: InputSourceId, phase: TouchPhase, slot: Option<u32>, position: Option<TouchPosition>, time: u32 }` and equivalent `HostInputEvent::Touch`. Only Down/Motion have `Some(position)`. Use `x_transformed(1)`/`y_transformed(1)`; native XI touch values are these fractions multiplied by 65535. Libinput `x()`/`y()` millimeters are not XI axis units. KMS later derives root coordinates using native × state.randr.screen_width/height ÷ 65536, with native=norm × 65535, matching Xorg's inclusive declared axis range; this task forwards contacts without converting them into pointer motion.

- [ ] **Step 1:** Add `touch_message_mapping` for Down/Motion fractional coordinates, Up/Cancel None, source, slot, timestamp, preserved input ordering, and no pointer/scroll side effect.
- [ ] **Step 2:** Run `cargo test -p yserver touch_message_mapping --lib`; confirm failure.
- [ ] **Step 3:** Match `TouchEvent::{Down,Motion,Up,Cancel,Frame}`; resolve the event's source through its device handle. Frame flushes preceding contacts in order and creates no XI contact event. Preserve source-safe cancellation and unknown-handle drop behavior.
- [ ] **Step 4:** Run focused and contract regression checks, format, CI clippy; commit `feat(input): forward source-tagged touch messages`.

### Task 3: Track physical contacts per source and slot

**Files:** Create `crates/yserver-core/src/xinput/touch.rs`; modify `crates/yserver-core/src/xinput/mod.rs`, `crates/yserver-core/src/server.rs`.

**Interfaces:** `TouchTracker::apply(&mut self, source_id: InputSourceId, phase: TouchPhase, slot: Option<u32>, position: Option<TouchPosition>, time: u32) -> Vec<TouchContactEvent>` allocates a server contact ID at Down and retains last position. `TouchContactEvent { source_id: InputSourceId, contact_id: u32, phase: TouchPhase, position: TouchPosition, has_new_position: bool, emulates_pointer: bool, synthetic: bool, time: u32 }` carries last coordinates for Up/Cancel, with `has_new_position=false`. `cancel_source(&mut self, source_id: InputSourceId, time: u32) -> Vec<TouchContactEvent>` and `cancel_all(&mut self, time: u32) -> Vec<TouchContactEvent>` return terminal events. `ServerState` owns the tracker. The later delivery state may retain a physically ended contact pending ownership decisions; slot tracking is already retired at physical Up. At Down set `emulates_pointer` only if no contact on that source is physically active; retain it through End and never promote a later contact. `synthetic=true` marks forced cleanup/repeated-Down terminal events; ordinary libinput phases use false.

- [ ] **Step 1:** Add `xinput::touch::tests` for stable ID across Down/Motion/Up, the same slot on two sources, single-touch None slot, unknown Motion/Up ignored, repeated Down returning old End before new Begin, slot-scoped Cancel, cancel_source/all, stored terminal coordinates, first-contact emulation without later promotion, synthetic cleanup flags, timestamps, and checked ID exhaustion without aliasing.
- [ ] **Step 2:** Run `cargo test -p yserver-core xinput::touch::tests --lib`; confirm failure.
- [ ] **Step 3:** Implement `(InputSourceId, Option<u32>)` keys and checked monotonic contact-ID allocation. Cancel on a libinput slot ends that contact; source removal uses `cancel_source`. Never reuse an active or pending-delivery ID.
- [ ] **Step 4:** Run focused and contract regression checks, format, CI clippy; commit `feat(xinput): track touch contacts by physical source`.

### Task 4: Encode fractional XI2 touch and raw touch payloads

**Files:** Modify `crates/yserver-protocol/src/x11/mod.rs:1962,2231`.

**Interfaces:** Add `XiTouchWireEvent { evtype: u16, deviceid: u16, sourceid: u16, time: u32, contact_id: u32, root: ResourceId, event: ResourceId, child: ResourceId, root_xy: [f64; 2], event_xy: [f64; 2], state: u16, flags: u32, axes: [f64; 2] }` and `encode_xi2_touch_event(out: &mut Vec<u8>, order: ClientByteOrder, sequence: SequenceNumber, major_opcode: u8, ev: &XiTouchWireEvent)`. Add `XiRawTouchWireEvent { evtype: u16, deviceid: u16, sourceid: u16, time: u32, contact_id: u32, flags: u32, axes: Option<[f64; 2]>, raw_axes: Option<[f64; 2]> }` and `encode_xi2_raw_touch_event(out: &mut Vec<u8>, order: ClientByteOrder, sequence: SequenceNumber, major_opcode: u8, ev: &XiRawTouchWireEvent)`. Use common internal mask/fixed-point packing helpers with existing encoders; preserve existing non-touch outputs and signatures. Device coordinates use FP16.16 and axis values FP32.32, retaining fractions. Task 6 supplies device-appropriate axes from Task 1's model.

Down/Motion raw values are libinput's transformed 0..65535 coordinates, the same driver-native units Xorg receives. Ordinary slave touch carries the last absolute position on End; RawTouchEnd has mask bits 0/1, axes=Some(last native processed position), and raw_axes=Some([0.0, 0.0]): GetTouchEvents backfills processed X/Y after initially recording an empty raw driver mask. Device removal/disable uses the same physical-End path and also emits RawTouchEnd. Ownership-only artificial End does not emit raw input. Distinguish normal/raw arrays in the encoder, even where the current calibration/transform makes them equal.

- [ ] **Step 1:** Add `xi2_touch_wire` byte assertions for types 18/19/20 and 22/23/24, XGE lengths, masks, source/contact IDs, flags, root/event coordinates, fractional native axes and RawTouchEnd with last processed axes plus zero raw axes, including forced physical End. Repeat in both byte orders; include a negative fractional event coordinate to catch incorrect signed packing. Existing key/button/motion/raw tests must keep their byte output.
- [ ] **Step 2:** Run `cargo test -p yserver-protocol xi2_touch_wire --lib`; confirm failure.
- [ ] **Step 3:** Match `xXIDeviceEvent`/`xXIRawEvent` and FP16.16/FP32.32 packing in `XI2proto.h`, Xorg `dix/eventconvert.c`, and `dix/getevents.c:GetTouchEvents`. Build lengths from actual mask/value counts using the existing Xorg mask widths.
- [ ] **Step 4:** Run focused and contract regression checks, format, CI clippy; commit `feat(xinput): encode fractional XI2 touch and raw touch`.

### Task 5: Validate XI2 touch selections

**Files:** Modify `crates/yserver-core/src/core_loop/process_request.rs` at XISelectEvents and XIGetSelectedEvents, `crates/yserver-core/src/server.rs`, `crates/yserver-protocol/src/x11/request_swap.rs`.

**Interfaces:** Extend the existing XISelectEvents validator before mutating stored selections. If any Begin/Update/End/Ownership bit is present, all of Begin/Update/End are required; otherwise return BadValue with errorValue XI_TouchBegin. Another client's touch selection on the same window and same requested device selector returns BadAccess. Implement the exact selector comparison from Xorg `Xi/xiselectev.c:check_for_touch_selection_conflicts`: equal exact IDs conflict, two XIAllDevices selectors conflict, two XIAllMasterDevices selectors conflict; wildcard versus exact selections may coexist. Do not broaden this into an overlap-of-physical-devices conflict. Preserve same-client replacement/clear and XI2 version checks; raw touch is not subject to regular-touch triplet/exclusivity rules.

- [ ] **Step 1:** Add `touch_selection_validation`: partial triplet and Ownership-only return BadValue without mutation; complete triplet succeeds; same window/device second client returns BadAccess; different window/device succeeds; wildcard/exact coexist per Xorg; same client can replace or clear; raw-only masks succeed; big-endian XISelectEvents variable mask headers decode correctly and malformed tails fail without partial mutation; XIGetSelectedEvents reports accepted state in each byte order.
- [ ] **Step 2:** Run `cargo test -p yserver-core touch_selection_validation --lib`; confirm failure.
- [ ] **Step 3:** Implement dynamic request_swap decoding of deviceid/mask_len in each XISelectEvents mask record (mask bytes stay opaque), reusing the first plan's request swap helper. Validate the whole request before applying selection changes, matching Xorg `Xi/xiselectev.c` error precedence and client-version behavior.
- [ ] **Step 4:** Run focused and contract regression checks, format, CI clippy; commit `feat(xinput): validate XI2 touch selections like Xorg`.

### Task 6: Build touch listeners and deliver regular/raw events

**Files:** Create `crates/yserver-core/src/core_loop/touch_fanout.rs`; modify `crates/yserver-core/src/core_loop/mod.rs`, `crates/yserver-core/src/server.rs`, `crates/yserver/src/kms/render/backend.rs:19046`.

**Interfaces:** `touch_event_fanout_to_state(state: &mut ServerState, source_xi_id: u16, contact: TouchContactEvent) -> Vec<ClientId>` resolves coordinates and target windows from the source and contact. Use native × state.randr.screen_width/height ÷ 65536 for the full root (native=norm × 65535), then root_pointer_target_at for hit testing; no individual output-mode, rotation, or CRTC-transform mapping is applied to the input surface (Xorg desktop-wide default without a Coordinate Transformation Matrix). Retain native pre-confinement coordinates separately from reported coordinates. Store `TouchDeliveryState` per `(deviceid, contact_id)`, with source, target trace, ordered listeners, owner, physical-ended state and replay history. For attached sources, process slave `(deviceid=sourceid=facet)` and master `(deviceid=2, sourceid=facet)` forms as Xorg does; their delivery state is distinct. Build listeners at Begin using `TouchSetupListeners`: active grab first; matching passive grabs from root to deepest child (Task 7 adds TouchBegin grabs); then at most one regular listener, searching deepest window toward root. At each window prefer applicable touch selection, then pointer selection for a pointer-emulating contact, and take the first deliverable client in Xorg order. Retain the selected target/listener through Update/End. Task 9 implements pointer-listener delivery; do not fan regular touch to every matching client.

A forced physical End on suspend/removal/reset emits a raw End with the Task 4 payload; ownership-generated End alone does not. This distinction is independent of TouchContactEvent.synthetic, which can describe forced physical cleanup. Raw touch uses the existing raw-event selection/grab/version policy from the first plan and may reach multiple raw subscribers; it is independent of regular listener exclusivity. Slave/master forms retain source attribution and masks without delivering the same form twice for overlapping selectors. Use the per-master SlaveSwitch hook before touch events. Unknown sources are dropped; allocation-exhausted known touch sources retain only core pointer emulation when applicable.

- [ ] **Step 1:** Add `touch_fanout_selection`: stable ID across phases; deepest eligible regular listener; touch preference over pointer selection on the same window; wildcard/exact selectors coexist but do not fan the same regular form to both clients; multiple raw subscribers; exact-slave isolation for two screens; separate slave/master identities; no duplicate cookie from overlapping selectors; retained Begin target after crossing a window boundary. Include norm=0.5 on a 1920-wide root yielding native=32767.5 and root_x=959.9853515625 before confinement, two-output root extent, a scaled/rotated CRTC footprint, and non-emulating contacts in an intentional root gap without pointer confinement.
- [ ] **Step 2:** Run `cargo test -p yserver-core touch_fanout_selection --lib`; confirm failure.
- [ ] **Step 3:** Connect Task 2 messages, Task 3 tracker, Task 4 encoders and Task 5 selection state. Use Xorg `dix/touch.c:TouchSetupListeners` and `Xi/exevents.c:ProcessTouchEvent/DeliverTouchEvents`; keep regular touch listener delivery separate from raw fanout.
- [ ] **Step 4:** Run focused and contract regression/integration checks, format, CI clippy; commit `feat(xinput): deliver touch through Xorg listener selection`.

### Task 7: Implement device-scoped passive TouchBegin grabs

**Files:** Modify `crates/yserver-core/src/core_loop/process_request.rs:18432`, `crates/yserver-core/src/server.rs`, `crates/yserver-core/src/core_loop/touch_fanout.rs`, `crates/yserver-protocol/src/x11/request_swap.rs`.

**Interfaces:** Add `PassiveTouchGrab { device_id: u16, client: ClientId, window: ResourceId, modifiers: u32, grab_mode: u8, paired_device_mode: u8, owner_events: bool, cursor: ResourceId, mask: Vec<u8> }`, one record per requested modifier. Implement `XIPassiveGrabDevice`/`XIPassiveUngrabDevice` for `XIGrabtypeTouchBegin=4`, requiring detail 0, grab_mode XIGrabModeTouch (2), paired_device_mode GrabModeAsync (1), valid mask/modifiers/cursor/window, and conflict/status behavior from Xorg `Xi/xipassivegrab.c`. Match the exact device or wildcard selector at Begin using the first plan's role/attachment machinery. Insert matching touch and pointer grabs into Task 6's ordered listener list following `TouchAddPassiveGrabListener`. Ungrab/client disconnect/window destruction must retire matching listener resources safely.

- [ ] **Step 1:** Add `touch_passive_grabs`: valid grab activates only on the requested device/window/modifiers; invalid detail/mode/mask returns Xorg's error/status; grab conflict per modifier; exact-slave grab excludes another screen; wildcard matching; ungrab and disconnect remove listeners without stale resources; pointer/keyboard grabs still work; big-endian passive grab/ungrab modifiers decode before validation.
- [ ] **Step 2:** Run `cargo test -p yserver-core touch_passive_grabs --lib`; confirm failure.
- [ ] **Step 3:** Reuse/finalize the dynamic passive-grab/ungrab modifier-array request swaps from the first plan. Replace the existing logged-and-ignored TouchBegin branch and implement its matching ungrab path. Use Xorg `Xi/xipassivegrab.c`, `dix/passivegrab.c`, and `dix/touch.c` rather than treating a touch grab as an ordinary active button grab.
- [ ] **Step 4:** Run focused and contract regression checks, format, CI clippy; commit `feat(xinput): implement passive XI2 TouchBegin grabs`.

### Task 8: Implement ownership, accept/reject and replay

**Files:** Modify `crates/yserver-core/src/core_loop/touch_fanout.rs`, `crates/yserver-core/src/core_loop/process_request.rs:18277`, `crates/yserver-protocol/src/x11/mod.rs`.

**Interfaces:** Extend XIAllowEvents (minor 53) modes `XIAcceptTouch=6`/`XIRejectTouch=7`, validating device, contact, client and grab window as Xorg `TouchAcceptReject` does. A qualifying listener may accept early; do not reject it merely because it is not yet current owner. Invalid device returns BadDevice, unknown contact BadValue, and no matching client/window listener BadAccess. Preserve ordered ownership transitions, `XI_TouchOwnership=21`, early acceptance, replay history for listeners without ownership selection, and `XITouchPendingEnd` when physical End precedes resolution. Encode ownership cookies using `xXITouchOwnershipEvent` in both byte orders. A physical slot may be reused while an older contact awaits resolution; their contact IDs remain distinct.

- [ ] **Step 1:** Add `touch_ownership` for passive grab accept, reject/transfer, early accept by a later listener, wrong client/window/contact, Begin/history replay where required, physical Up while ownership remains pending, pending-end flags, ungrab/disconnect while owned, and exactly one terminal End per listener.
- [ ] **Step 2:** Run `cargo test -p yserver-core touch_ownership --lib` and `cargo test -p yserver-protocol xi2_touch_ownership_wire --lib`; confirm failure.
- [ ] **Step 3:** Implement `TouchListenerAcceptReject`/`ProcessTouchOwnershipEvent` behavior from Xorg `dix/touch.c` and `Xi/exevents.c`; preserve existing non-touch XIAllowEvents modes. Keep history until the delivery sequence resolves, rather than deleting all delivery state at physical Up.
- [ ] **Step 4:** Run focused and contract regression/integration checks, format, CI clippy; commit `feat(xinput): honor touch ownership and pending-end replay`.

### Task 9: Deliver pointer emulation through the selected listener

**Files:** Modify `crates/yserver-core/src/core_loop/touch_fanout.rs`, `crates/yserver-core/src/core_loop/pointer_fanout.rs`, `crates/yserver/src/kms/render/backend.rs`.

**Interfaces:** Move the emulating contact through KmsBackend::process_pointer_absolute(relative=false), including root clamping and constrain_to_crtcs. Ordinary touch/root/native reported coordinates use the post-confinement position; raw processed values retain pre-confinement native values from GetTouchEvents. Non-emulating contacts keep their own unconstrained root positions. The first plan's physical relative integrator starts from the current KMS cursor, preventing touch-to-mouse snapback. Keep Task 3's first-contact emulates_pointer rule; concurrent later contacts never acquire it by promotion.

Add `PointerDelivery = Normal | TouchListener { client: ClientId, window: ResourceId, deviceid: u16 } | TouchStateOnly { deviceid: u16 }` as the last pointer_event_fanout_to_state argument and update ordinary callers with Normal. KMS pending entries become `QueuedPointerEvent { event: HostPointerEvent, delivery: PointerDelivery }`; builders and the drain retain the tag. Add HostPointerEvent.pointer_emulated: bool (ordinary producers false); emulated Motion/Button set it and encode XIPointerEmulated. They never produce an additional XI raw-pointer event: raw touch was already delivered. Extend process_pointer_absolute with delivery/origin, but apply the restriction only to emulated Motion/Button. emit_crossing always queues Normal with the real source; Enter/Leave, cursor updates and XFIXES cursor notifications use their ordinary delivery. Never generate an unrestricted duplicate of the emulated Motion/Button.

For a pointer listener, use TouchListener to deliver Motion/Button only to the retained owner/listener chosen by Tasks 6–8, with the existing grab/freeze/replay behavior. A regular touch owner moves the sprite through TouchStateOnly without emitting pointer Motion/Button to other clients. For zero listeners mirror Xorg DeliverEmulatedMotionEvent's ordinary Motion fallback where it is invoked (Begin; End only when the ProcessTouchEvent call condition applies), while normal sprite/crossing processing remains; do not synthesize an unrestricted Button. Preserve the master/slave form distinction without moving the master twice, including a floating touch facet's independent position.

Physical buttons keep KP Task 10's down-state aggregation. Touch emulation has separate per-device TouchClass button count/state, including a separate master count. Mirror Xi/exevents.c:UpdateDeviceState ET_TouchBegin/End: count once for an emulating non-replayed Begin and decrement on its effective End after ownership/pending-End handling; replay must not add another hold. Do not call process_pointer_button's physical master guard for emulated cookies. Deliver the chosen pointer listener its emulated Press/Release even when a physical mouse or another touch holds button 1. Build core state/buttons by OR-ing physical button state with touch state, matching event_get_corestate/event_set_state; one stream's Release does not erase another stream's reported hold. Emit XITouchEmulatingPointer on corresponding touch events and carry the physical source facet, never 4. Terminal/replay bookkeeping prevents duplicate releases for a listener.

- [ ] **Step 1:** Add `touch_pointer_emulation` for one contact, overlapping contacts on one source, no promotion of the second finger, independent first contacts on two screens, touch and pointer-only clients on one window with no duplicate click, passive pointer grab, accept/reject transitions, no-listener cursor movement, touch at (1500,800) followed by mouse +1 ending at (1501,800), post-confinement reported touch coordinates in a transformed two-output layout, one final release on End/Cancel/removal, mouse button-1 drag overlapping a touch tap with a complete Press/Release at its pointer listener and the mouse still reported held, two screens' touch button counts, pending-End/replay without double counting, and normal Enter/Leave to an independent WM during emulated motion across windows.
- [ ] **Step 2:** Run `cargo test -p yserver-core touch_pointer_emulation --lib`; confirm failure.
- [ ] **Step 3:** Implement Xorg `dix/touch.c` listener types and `Xi/exevents.c:DeliverEmulatedMotionEvent/DeliverTouchEmulatedEvent`; reuse source-aware pointer state while honoring contact ownership. Keep distinct pointer-cookie views according to master/slave selection without creating a second master button stream.
- [ ] **Step 4:** Run focused and contract regression checks, format, CI clippy; commit `feat(xinput): emulate pointer through touch listener ownership`.

### Task 10: Clean up contacts on cancel, removal, suspend and reset

**Files:** Modify `crates/yserver-core/src/xinput/touch.rs`, `crates/yserver-core/src/core_loop/touch_fanout.rs`, `crates/yserver/src/kms/render/backend.rs`, `crates/yserver-core/src/core_loop/reset.rs`, `crates/yserver-core/src/core_loop/run.rs`, `docs/status.md`.

**Interfaces:** Use Task 3 `cancel_source` before the first plan's release/disable/remove sequence; VT suspend ends contacts once through the first plan's synchronous release owner and subsequent DeviceSuspended is idempotent. Proven DeviceResumed keeps source/XI identity but begins fresh contacts; a physically replugged endpoint receives a new source. Reset wires `cancel_all` and listener/history cleanup into the first plan's Backend::reset_input_session hook while old clients/windows/facets still exist, then clears pending delivery/listener/history state before rebuilding the registry. Release emulated pointer state once and finish both physically active and pending-owned contacts without retaining dead resource IDs. Window destruction and client disconnect retire delivery listeners and resolve or terminate ownership; late old-source Up is ignored.

- [ ] **Step 1:** Add `touch_lifecycle` for slot Cancel versus source removal, pending-owned contact during unplug, VT pause/resume ordering, reset, late old-source Up, replug with new contact/source IDs, window destruction/disconnect, and no remaining emulated button or stale listener.
- [ ] **Step 2:** Run `cargo test -p yserver-core touch_lifecycle --lib` and `cargo test -p yserver-core core_loop::reset::tests --lib`; confirm failure.
- [ ] **Step 3:** Wire cleanup before disabling/removing facets or dropping client resources. Update status with the complete touch behavior and evidence only after it lands.
- [ ] **Step 4:** Run focused and contract suites, `cargo test --workspace --all-targets --locked`, format, CI clippy. With authorized hardware verification compare XI1/XI2 lists, query axes, touch/raw coordinate values, listener/grab/ownership behavior, pointer emulation, unplug/replug and VT switching against Xorg traces; record missing hardware checks explicitly. Commit `feat(xinput): clean up touch across device and client lifecycle`.

## Completion boundary

Both this plan and the keyboard/pointer plan must land before the dynamic XInput spec is complete. The final review checks protocol bytes and real-client behavior for MATE, i3, GTK, and XInput tools, especially listener selection, coordinate units, add/remove order and source attribution. No client compatibility claim is established by this plan alone.
