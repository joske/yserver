**Invocation verified:** Claude Code `claude-opus-5-5`, `--effort high`, read-only tools; successful completion, 139 turns, zero permission denials. **Code base:** `dbeb5a49`, incorporating `joske/master` at `736a8036`.

# Round 2 adversarial review: dynamic XInput registry spec and plans

**Verdict up front: NOT CONVERGED.** Two HIGH and ten MEDIUM findings remain. Every one is either a verified correctness defect, a contradiction, or a mandatory decision the plans leave to the worker.

**Scope.** Read-only review of the four live documents against the updated yserver base (includes `736a8036`), `../xserver`, `../xorgproto`, `/tmp/xf86-input-libinput-1.5.0/src/xf86libinput.c`, the `input` 0.10.0 crate and `libinput.h`. **No implementation tests were run and no files were written.** The report is delivered inline as you asked, rather than as a plan file.

Abbreviations: **KP** = keyboard/pointer plan, **TP** = touch plan, **SPEC** = design spec. yserver paths are relative to the repo; `xserver/…` is the local Xorg checkout; `xf86libinput.c` is the 1.5.0 driver.

Each finding marks its basis as **Verified** (read in source), **Inference**, or **Unverifiable**.

---

## HIGH

### R2-1. VT release has two uncoordinated release mechanisms, and the resume-time synthetic key releases have no origin
- **Affected:** KP Task 3 (KP:88), Task 13 (KP:202), Task 14 (KP:213, 217); TP Task 10 (TP:165); SPEC:114-116, 120-121.
- **Verified:**
  - `on_vt_release` pauses the input thread, then `run_suspend` calls `synthesize_held_releases` on global state (`crates/yserver/src/kms/render/backend.rs:20561-20571`, `12476-12483`, `12038-12114`).
  - That function calls `key_event_fanout_to_state` directly, bypassing the duplicate-key guard at `backend.rs:19331`.
  - `process_pointer_button` emits a release even when the bit is already clear; it only logs a warning (`backend.rs:14139-14182`).
  - After resume the input thread sends eight device-less modifier releases (`crates/yserver/src/input_thread.rs:831-844`).
- **Failure sequence:**
  1. User presses Ctrl+Alt+F2. The keyboard source holds Ctrl and Alt; the F-key is eaten by the hotkey.
  2. The core runs `run_suspend` and releases Ctrl/Alt from the global set.
  3. The input thread processes Pause and, per Task 3, emits `DeviceRemoved(source)`.
  4. Task 14 "releases that source's held keys/buttons through existing master/focus paths". Nothing in the plan clears the per-source map in step 2, so Ctrl/Alt are released a second time. A held button gets a second `ButtonRelease`.
  - The exact symptom is **inference**; the existence of two owners with none designated is verified. This happens on every VT switch.
- **Second gap:** Task 3 adds `origin` to `HostKeyEvent`, but none of `Physical | XTest | NestedHost` fits the eight resume releases. `Physical(unknown)` is dropped by SPEC:110.
- **Smallest correction (Task 14, add `backend.rs` `run_suspend`/`synthesize_held_releases` to its scope):**
  - Name one owner. Either `run_suspend` stops synthesizing and the per-source removal path does it, or `synthesize_held_releases` drains the per-source maps.
  - Require removal releases to pass the master down-state guard.
  - Delete the eight synthetic releases (redundant with the reset at `backend.rs:20651-20653`) or give them an explicit origin.
  - Add a KMS-level check: VT release with Ctrl+Alt held produces exactly one release per key.

### R2-2. The input-thread cursor accumulator is not accounted for by floating slaves or touch pointer emulation
- **Affected:** KP Task 11 (KP:180; its Files list omits `input_thread.rs`); TP Task 2 (TP:73), Task 9 (TP:154).
- **Verified:**
  - All relative motion is integrated on the input thread into one cursor (`input_thread.rs:142-150`).
  - `HostInputEvent::PointerMotion` carries that master-absolute `x/y` (`crates/yserver-core/src/core_loop/message.rs:231-244`), and KMS applies it (`backend.rs:19262-19270`, `13997-13999`).
  - The only resync is `push_position` (`backend.rs:27980-27984`), which also drops the pending coalesced motion (`input_thread.rs:810-818`).
- **Failure A (floating slave):**
  1. A client calls `XIGrabDevice` on Razer facet 6; the slave floats.
  2. Razer moves +500 px. The input-thread cursor advances; the core must not move the master.
  3. Ungrab. HyperX moves 1 px. The message carries old+501, so the master cursor jumps 500 px.
  - Task 11's checks run on `RecordingBackend`, which has no accumulator, so they pass. This is the same pattern as round-1 H1.
- **Failure B (touch emulation):**
  1. A touch moves the sprite to (1500,800) via TP9.
  2. The mouse moves 1 px. The input thread sends its stale position +1, and the cursor snaps back.
- **Inference:** a floating keyboard also needs its own XKB state. KMS has one `core.xkb_state`, and KP:180 says master state must not be mutated.
- **Smallest correction:**
  - KP11: for a floating source, KMS integrates the event's `dx/dy` into a per-slave position, leaves `core.cursor_*` alone, and calls `resync_input_position()`. State the keyboard-state rule, or scope floating keyboards out explicitly. Add a KMS-level check.
  - TP9: emulated motion goes through `process_pointer_absolute(relative=false)` followed by `resync_input_position()`. Name the recipient-restriction parameter added to `pointer_event_fanout_to_state`.

---

## MEDIUM

### R2-3. Master aggregation of per-source held keys and buttons is undefined
- **Affected:** KP Task 10 (KP:169 "per physical InputSourceId, not globally"), Task 13 (KP:202, 206 "keep … duplicate-key guard"), Task 14 (KP:213). TP Task 9 (TP:154) presumes a merge rule that KP never states.
- **Verified:**
  - Xorg keeps per-device down state. On the master, a press of an already-down key is dropped and the first release releases it (`xserver/Xi/exevents.c:922-943`).
  - A master button release is suppressed while any attached slave still holds the mapped button (`exevents.c:960-992`).
  - yserver has one `button_mask` and one `down_keys` (`backend.rs:14156-14160`, `19331-19349`).
- **Failure sequence:** Razer holds button 1 in a drag. A second source (HyperX, or XTEST via `xdotool click 1`) releases button 1, or is unplugged under "releases only that source's buttons". A master `ButtonRelease` is delivered while Razer still holds, and the drag's implicit grab ends.
- **Smallest correction:** in KP10/13/14 state the Xorg rule:
  - Down sets per XI device, including 4/5 and XTEST-targeted facets.
  - Master button release only when no attached slave holds it.
  - Master key press ignored when already down; release on first release.
  - Removal releases obey the same rule.

### R2-4. Tasks 2 and 3 remove the node key before its consumers are migrated
- **Affected:** KP Task 2 (KP:77 "`InputEvent::DeviceRemoved { source_id }` replaces node removal"), Task 3 (KP:88, same for `HostInputEvent`), versus Task 6 (KP:121-125).
- **Verified:** node consumers remain until Task 6:
  - `input_thread.rs:502-508` builds `HostInputEvent::DeviceRemoved { device_node }`.
  - `crates/yserver-core/src/core_loop/run.rs:1653-1654` removes from the inventory by node.
  - `backend.rs:19389-19391` calls `xi_clear_touchpad(&device_node)`.
  - `core_loop/generation.rs:101-105` and `host_x11/trait_impl.rs:178` also match the variant.
  - `backend.rs` is in neither task's Files. `XiRegistry::register` is not called in production before Task 6, so the node cannot be recovered from the registry.
- **Failure:** after Task 2 the input thread has no node to send. After Task 3 KMS cannot clear device 4 for three task boundaries.
- **Smallest correction:** carry `{ source_id, device_node }` in both variants through Task 5 and drop `device_node` in Task 6. Alternatively move the KMS add/remove routing into Task 3.

### R2-5. Task 7/8 interfaces cannot be populated where they are defined
- **Affected:** KP Task 7 (KP:132), Task 8 (KP:143-147).
- **Verified:**
  - `XiConfigRequest.generation` has no producer. `process_request` takes no generation (`core_loop/process_request.rs:178-186`), `ServerState` has none, and `Generation`'s field is private and "never inside ServerState" by design (`generation.rs:1-28`).
  - The `Applied` path must update `InputInventory`, which is a `run_core` local (`run.rs:1296`) and unreachable from `process_request`.
  - `Message::DeviceConfigResult` needs an arm in the exhaustive `is_session_scoped` match (`generation.rs:64-81`). That file, and `reset.rs` for the retain-across-reset rule, are in no Files list.
  - "Handle this result before generation quarantine, like inventory lifecycle messages" misdescribes the mechanism. Those messages are classified process-lifetime; they are not handled earlier (`run.rs:1494-1500`). **Refuted as worded.**
  - `XIChangeProperty` has no reply. "Unsubmitted work is canceled on … source removal" must therefore say that a BadDevice error is emitted at the request's sequence.
- **Smallest correction:**
  - Drop the `generation` field, or stamp it in `run.rs` at park time.
  - State that `process_request` returns only `Queued`, and the lane in `run.rs` performs validate → start → commit → inventory update.
  - Add `generation.rs` and `reset.rs` to Task 8 Files.
  - Define cancel-on-removal as BadDevice.

### R2-6. The RawTouchEnd contract is refuted by Xorg
- **Affected:** TP Task 4 (TP:97, Step 1 "empty RawTouchEnd"), Task 6 (TP:121 "Forced cleanup … does not invent a raw event"), SPEC:206-207.
- **Verified:**
  - For a direct-touch End with an empty driver mask, `GetTouchEvents` back-fills X/Y from the touch's stored valuators, then calls `set_raw_valuators(…, raw->valuators.data)` (`xserver/dix/getevents.c:2026-2048`).
  - `TouchEndDDXTouch` only clears `active` (`xserver/dix/touch.c:187-195`), so the stored valuators survive.
  - `eventToRawEvent` emits every set bit (`xserver/dix/eventconvert.c:781-813`).
  - Result on the wire: RawTouchEnd has mask bits 0 and 1; `valuators` hold the last position; `raw` values are 0.0.
  - Device disable and removal end touches through the same function, which produces raw events (`xserver/dix/devices.c:481`, `touch.c:1013-1036`).
  - The in-tree encoder already treats type 24 as carrying valuators (`crates/yserver-protocol/src/x11/mod.rs:2245`).
- **Smallest correction:**
  - TP4: RawTouchEnd uses `axes = Some(last)`, `raw_axes = Some([0,0])`, mask `{0,1}`.
  - TP6: a forced End on removal or disable emits a raw End; only ownership-generated Ends do not.
  - Fix SPEC:206-207.

### R2-7. TP Task 1 contradicts itself on master valuator units and leaves the touch-only class list open
- **Affected:** TP Task 1 (TP:58-60, 64; Review Focus 1 "class count").
- **Verified:**
  - The master-form event is a `memcpy` of the slave event. Only the device ID and button mapping change (`xserver/mi/mieq.c:359-376`, `423-425`).
  - Valuators for an absolute slave are therefore device units (0..65535), not desktop pixels (`getevents.c:976-989`).
  - "The master view uses desktop-space X/Y, following Xorg master scaling" is **refuted** for touch-capable facets. It also conflicts with the next sentence, which requires SlaveSwitch classes and event values to agree.
  - Driver class list for a touch-only device (`xf86libinput.c:1246-1274`):
    - ButtonClass with 7 buttons.
    - Four valuators: 0/1 Abs MT, and 2/3 relative with scroll labels and no ScrollClass.
    - TouchClass.
  - The combined pointer+touch claim in the plan is **confirmed**. The second `InitPointerDeviceStruct` bails (`xserver/dix/devices.c:1654-1657`), so scroll valuators 2/3 from `init_pointer` survive and axes 0/1 become Abs MT.
- **Smallest correction:**
  - State that the master form carries the same valuator values as the slave form; only `root_*`/`event_*` are desktop pixels.
  - Pin the touch-only class list to the driver's.

### R2-8. Touch coordinate conversion is not tied to the RandR root extent or the new transform and confinement paths
- **Affected:** TP Task 2 (TP:73 "using current output geometry"), Task 6 (TP:119), Task 9 (TP:154). Neither plan mentions transforms, rotation or CRTC confinement.
- **Verified on the updated base:**
  - The pointer range is the client root extent `state.randr.screen_width/height`, mirrored into `platform.fb_w/fb_h` and the input thread (`backend.rs:21461-21473`, `12979-12984`).
  - CRTC footprints under rotation or transform come from `output_root_rect` (`crates/yserver/src/kms/render/platform.rs:3878-3886`).
  - Moves are confined by `constrain_to_crtcs` (`backend.rs:13990-13996`; `kms/render/pointer_confine.rs:49-78`).
  - Xorg maps direct touch desktop-wide (`getevents.c:2056-2057`, `884-918`). Only the pointer-emulating contact is constrained, and it then reports the constrained coordinates (`getevents.c:2058-2060`, `963-989`).
- **Failure (inference):** "output geometry" invites mapping to one output's mode size. With two outputs, a scaled CRTC or a rotated panel, that targets the wrong window. `backend.rs:13976-13985` records exactly this bug class for the pointer.
- **Smallest correction:**
  - TP2/TP6: root = norm × root extent, hit-tested with `ServerState::root_pointer_target_at`.
  - No per-output or rotation mapping; this matches Xorg without a Coordinate Transformation Matrix. Say so.
  - TP9: the emulating contact uses the post-`constrain_to_crtcs` position.
  - Add a check with a transformed CRTC and a two-output root.

### R2-9. Big-endian XI2 requests are not decodable for the handlers these plans rewrite
- **Affected:** KP Task 10 (exact-slave selections), Task 11 (passive grabs with `device_id`), TP Task 5, Task 7. Both-order checks exist only for replies and events (KP:99, 112; TP:99, 110).
- **Verified:**
  - The swap table leaves the XISelectEvents mask records and the XIPassiveGrabDevice modifiers unswapped (`crates/yserver-protocol/src/x11/request_swap.rs:925`, `947-956`).
  - The handlers read them little-endian (`process_request.rs:17471-17477`, `18812-18815`).
  - Xorg swaps both (`xserver/Xi/xiselectev.c:116-143`, `xserver/Xi/xipassivegrab.c:50-75`).
- **Failure sequence:** a big-endian client selects on device 6 with `mask_len=1`. The handler reads `mask_len=256`, hits the length check, breaks, and stores nothing, with no error. Passive-grab modifiers are read byte-reversed. This is pre-existing, but TP5 and TP7 build validation on top of it.
- **Smallest correction:** add `request_swap.rs` to TP5 and to KP11/TP7, with dynamic swaps following the pattern at `request_swap.rs:42-46`, and big-endian request-decoding checks.

### R2-10. `sourceid` for NestedHost and for known sources without a facet is unspecified
- **Affected:** KP Task 9 (KP:158), Task 12 (KP:191); SPEC:46-48 versus SPEC:109-113.
- **Verified:**
  - XI2 device and raw events have a mandatory `sourceid`.
  - Xorg uses the generating device's own ID for raw events (`getevents.c:205-206`). Applying the same to device events is **inference**; I did not read `init_device_event`.
  - SPEC:46-48 says nested input "retains existing behavior". Today that is sourceid 4/5, which now means XTEST.
- **Failure:** GTK clients on the master receive only XI2 events. The worker must either suppress master-form events (device dead for GTK) or invent a sourceid.
- **Smallest correction:** state that the master form is delivered with `sourceid = master id` for `NestedHost` and for capacity-exhausted sources. No slave or raw-slave form is emitted. Reconcile the two spec sentences.

### R2-11. Driver properties remain client-deletable, and Task 7's "exists on that facet" is ambiguous
- **Affected:** SPEC:153-158; KP Task 1 (non-deletable only for the XTEST marker), Task 7 (KP:132).
- **Verified:**
  - yserver deliberately allows deletion, on the rationale that the driver re-seeds on every add (`process_request.rs:17976-18010`). That rationale ends with per-source seeding.
  - Xorg makes every libinput property, Device Node and Product ID non-deletable (`xf86libinput.c:5580`, `6723`, `6735`; `xserver/Xi/xiproperty.c:657`).
  - Device Node and Product ID are read-only in the driver (`xf86libinput.c:5536`, `5552`).
- **Failure sequence:** `xinput delete-prop 6 "libinput Accel Speed"`. Task 7 then returns BadMatch for every later write until replug. The KDE KCM crash condition (atom absent, `crates/yserver/src/input/context.rs:247-250`) becomes reachable per device. The KCM behaviour itself is **unverifiable** locally.
- **Smallest correction:**
  - Seeded driver properties return BadAccess on delete, including `XIGetProperty(delete=1)`.
  - Device Node and Product ID are read-only.
  - Task 7 defines support from the source's config snapshot, not from map presence.

### R2-12. VT switch discards client-applied configuration and renumbers devices; Xorg preserves both
- **Affected:** SPEC:118-125; KP Task 3.
- **Verified:**
  - Xorg disables and re-enables the same device across a VT switch (`xserver/hw/xfree86/common/xf86Events.c:304-323`, `379`, `431`).
  - The driver reapplies stored options on enable (`xf86libinput.c:971-1001`).
  - IDs and properties survive; clients see Disabled/Enabled, not Removed/Added.
- **Consequence under the plan:**
  - Every VT switch resets natural-scroll, accel speed, left-handed and similar settings to libinput defaults.
  - Clients that do not listen for presence (one-shot `xinput set-prop` from an i3 config) lose them.
  - Exact-ID selections and grabs go stale.
- This is not a regression against today's behaviour: the handles are already recreated, and XI currently shows stale values.
- **Decision needed.** Either record this as an accepted deviation in the spec, or carry the last confirmed snapshot across one suspend/resume pair (matched within that pair only, not as a selector) before publishing.

---

## LOW (non-blocking)

- **R2-13. Task 17 names an API that does not exist** (KP:248).
  - There is no `KmsCore::open_with_commit`.
  - The real chain is `KmsBackend::open` → `KmsBackend::open_with_commit` (`backend.rs:5198-5221`) → `PlatformBackend::open_with_commit` (`platform.rs:2623-2635`) → `platform_init` (`crates/yserver/src/kms/backend.rs:982`, `1062`) → `SendContext::new`.
  - `input_thread::run` takes an already-built context and needs no profile.
- **R2-14. Property rules on non-libinput devices.**
  - Xorg's XTEST handler rejects only the XTEST marker; other writes succeed as ordinary properties (`xserver/Xext/xtest.c:589-597`). Task 7's BadMatch for libinput-named writes to 2–5 differs. State the choice.
  - Keyboard facets should carry `Device Node` and `Device Product ID`, as the driver's keyboard subdevice does.
- **R2-15. TP:60 attributes "keep cursor movement intact" to Xorg.**
  - Xorg adds relative deltas in device units on an absolute-axis device (`getevents.c:786-818`), so its cursor speed is scaled.
  - The emitted valuator values in the plan are right. Label the cursor behaviour as a deliberate deviation.

---

## Audit of round-1 findings

| ID | Status | Note |
| --- | --- | --- |
| B1 | Partial | Forwarding specified (KP3). Residual: R2-1, R2-12 |
| H1 | Resolved in design | Residual interface gaps: R2-5, R2-11 |
| H2 | Resolved | KP5 |
| H3 | Partial | Device-scoped grabs specified; floating mechanics missing: R2-2 |
| H4 | Resolved | KP10/15. Related new gap: R2-3 |
| H5 | Resolved | Matches `selectev.c:68-112,158` |
| H6 | Resolved | KP:22, TP:22 |
| H7 | Resolved | Matches `touch.c:737-875`, `xiselectev.c:47-89,216-271`. Residual: R2-9 |
| H8 | Resolved | TP7; modes match `xipassivegrab.c:126-136` |
| H9 | Partial | Axis model confirmed. Residual: R2-6, R2-7, R2-8 |
| M1 | Partial | Files and absolute motion added; node removal premature: R2-4 |
| M2 | Resolved | Callers now at `reset.rs:421`, `recording.rs:1338` |
| M3 | Resolved | Matches `devices.c:428-431,547-550,616-620,1256-1258`, `xichangehierarchy.c:61-121` |
| M4 | Resolved | Naming error only: R2-13 |
| M5 | Partial | Names and marker match `xtest.c:615-638`; sourceid open: R2-10 |
| M6 | Resolved | Matches `mieq.c:307-340,484-508`, `events.c:2475-2522` |
| M7 | Resolved in design | Inventory reachability: R2-5 |
| L1–L5 | Resolved | Unknown touch count = 15 confirmed (`xf86libinput.c:52,1271-1274`) |
| L6 | Deferred | Hardware check; still unverifiable |
| L7 | Withdrawn | The authors' rejection is correct: no size threshold exists in AGENTS.md or the spec |

**Other plan claims confirmed against source:**
- `input::Device` equality and hash by pointer (`input-0.10.0/src/lib.rs:188-198`).
- Touch slot and position traits, and `touch_count() -> Option<u32>` (`event/touch.rs:29-53,179-201`; `device.rs:687`).
- XI2 detach-on-grab rule (`events.c:1621-1624,1742-1745,1717,1814`).
- First-contact-only emulation (`touch.c:147-160`).
- Keyboard subdevice split and the TOUCHPAD > TOUCHSCREEN > MOUSE type order (`xf86libinput.c:4135-4141,3810-3815`).
- XI constants, including `XIGrabtypeTouchBegin=4`, `XIAcceptTouch=6`, `_devicePresence=0`.

**Task sufficiency:**
- Adequate as written: KP 1, 4, 5, 6, 9, 12, 15, 16, 18; TP 3, 8.
- Need the corrections above before dispatch: KP 2/3 (R2-4), 7/8 (R2-5), 10/13/14 (R2-1, R2-3), 11 (R2-2, R2-9), 17 (R2-13); TP 1 (R2-7), 2/6 (R2-8), 4 (R2-6), 5/7 (R2-9), 9 (R2-2).

---

## Verdict

**NOT CONVERGED.**

Blocking findings: **R2-1, R2-2** (HIGH) and **R2-3 through R2-12** (MEDIUM). R2-12 needs your explicit decision. R2-13 to R2-15 are editorial or low-risk and do not block. There is no BLOCKER-severity finding this round.

No implementation tests were run; hardware and third-party client behaviour (GTK, MATE, KDE) remain unverified.

## Reviewed document fingerprints

```json
{
  "docs/superpowers/specs/2026-09-29-dynamic-xinput-device-registry-design.md": "d56ece13f61bf444b355580cc8e3cc69ac4babadb7bb040d3cd283aa2099372e",
  "docs/superpowers/plans/2026-09-29-dynamic-xinput-keyboard-pointer.md": "ebb5757c1628c7ced8f137f77d15a8fad3f458a904f5ba089cafd82e7e88eb0b",
  "docs/superpowers/plans/2026-09-29-dynamic-xinput-touch.md": "d9f60a17011a908d034b5773b05310a5d6942c605e79f87fae27a02a9f37b774"
}
```

## Author disposition after round 2

The report above describes the reviewed snapshots, not the subsequently
corrected live documents. All findings were checked against the cited
source before editing documentation. No implementation has started.

| Finding | Disposition in the corrected design | Owning tasks |
| --- | --- | --- |
| R2-1 | Accepted. One guarded release owner drains source/XI down sets synchronously before VT yield; delayed suspension is idempotent. Delete the eight origin-less resume releases. | KP 3, 10, 13, 14; TP 10 |
| R2-2 | Accepted, with a different repair. KMS owns physical relative integration from fractional accelerated deltas; remove the input-thread accumulator rather than repeated resync that discards coalesced motion. Floating keyboards have independent XKB state. Restricted touch delivery travels through the existing tagged pointer queue. | KP 3, 11; TP 9 |
| R2-3 | Accepted. Explicit Xorg aggregation: buttons remain down while another attached slave holds the mapped button; keys release at the first valid slave release. Include XTEST and unpublished sources, and separate slave/master duplicate guards. | KP 10, 13, 14; TP 9 |
| R2-4 | Accepted. Keep source ID plus node in removal/suspend messages through Task 5; Task 6 converts all legacy consumers before dropping the node field. Enumerate generation, host and KMS matches. | KP 2, 3, 6 |
| R2-5 | Accepted. Parsing produces an owned request only; run_core owns generation, serialized validation/submission, completion and inventory update for both Applied and Pending. Retain submitted token/source/change across reset and classify completion as process-lifetime. Capture the original source at receipt and report canceled queued writes with their original sequence. | KP 7, 8 |
| R2-6 | Accepted. RawTouchEnd includes processed X/Y and zero raw X/Y, including forced physical cleanup. Ownership-only End does not emit raw input. | TP 4, 6, 10; spec |
| R2-7 | Accepted. Master copies retain native valuators. A touch-only facet has seven buttons, four valuators and TouchClass without ScrollClass; a mixed facet retains supported scroll classes. | TP 1, 4, 6; spec |
| R2-8 | Accepted. Use the full RandR client-root extent and common hit testing. Default input mapping has no additional per-output transform; confinement applies only to the emulating contact, with reported coordinates adjusted and raw processed coordinates retained. | TP 2, 6, 9; spec |
| R2-9 | Accepted. Decode variable XISelectEvents headers and passive grab/ungrab modifier arrays in request_swap, with both request byte orders and malformed tails covered. | KP 11; TP 5, 7 |
| R2-10 | Accepted. NestedHost and known unpublished sources use master-only XI forms with deviceid=sourceid=2/3 and no XTEST attribution. Unknown/removed/suspended physical events cannot mutate KMS state. | KP 9, 12, 14; spec |
| R2-11 | Accepted. Driver properties are non-deletable and metadata/default/availability properties are read-only; protect GetProperty(delete) as well as direct deletion. Support comes from source config, never a forged client map entry. | KP 6, 7 |
| R2-12 | Resolved by the standing AGENTS.md requirement to follow Xorg. Preserve XI/source identity and current configuration across a proven VT continuation, matched only within one explicit pause/resume pair using the canonical kernel endpoint-instance path. Physical unplug/replug or unavailable proof follows safe remove/add with a diagnostic. This is internal continuation proof, not a user selector. | KP 2, 3, 6, 8, 14, 15, 17, 18; TP 10; spec |
| R2-13 | Accepted. Corrected the actual startup constructor chain and removed input_thread::run as a profile recipient. | KP 17 |
| R2-14 | Accepted. Keyboard facets carry node/product metadata; masters/XTEST permit ordinary application properties with libinput names without a backend call. Only seeded metadata/driver properties and the XTEST marker have their stated protection. | KP 1, 6, 7; spec |
| R2-15 | Evidence accepted; the proposed deviation is not adopted. Follow Xorg's relative motion in native units on a mixed absolute-axis facet and its resulting cursor scaling. | TP 1; spec |

Earlier round-1 explanations of VT remove/add and RawTouchEnd's empty mask
are superseded by these corrections. The snapshot verdict remains NOT
CONVERGED; another independent review must verify the revised live design.
Hardware/client checks remain future acceptance evidence, not proof from
this document review. No implementation tests were run.

Further verification in [round 3](2026-09-30-dynamic-xinput-adversarial-review-round-3.md)
supersedes the GetProperty-delete protection and VT XTEST cleanup stated
in this disposition. Use the live spec/plans and the latest correction record.
