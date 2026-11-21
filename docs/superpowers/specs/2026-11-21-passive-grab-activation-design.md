# Passive grab activation and replay (C19), XI1 implicit grabs, XTest device motion

Status: revision 5 (sol r3: F1 XI1 mask snapshot, F2 one grab per device / master independence). Revision 4: the user folded the XI1 implicit grabs (I5) and the XTest device-motion sprite (C18) into this PR; revision 3 (C19/passive part) was APPROVED by sol r2 (sol review r1: F1 P4b diagnosis corrected and P4c added, F2 master-state invariant 3a, F3 C15 exception). Stacked on PR #374 (`feat/per-device-implicit-grabs`, `af690cdf`, rebased on master with #376; the measurements were taken on its pre-rebase tree `d479e443`, and #376 changed only GLX).
Measurements: `docs/superpowers/findings/2026-11-21-passive-grab-activation-measurements.md` (cases P1-P5, P4b and P4c, then I5 and X1-X3; one fresh server per case).

## Intent

joske's review of #373 (2026-11-19) left two follow-ups for this work:
- **XI2 passive-grab activation vs `ActivatePassiveGrab`.** An async slave grab with owner_events routes its activating press differently from Xorg, and a sync XI2 grab whose mask lacks ButtonPress mishandles the activating press.
- **The C19 replay gate** also fires when a core sync passive grab replays to a client with only slave-form XI2 selections. This is the click-to-focus pattern of xfwm4, marco and openbox.

The measurements show one family of defects behind both, plus a real-client case:
- **xfwm4 receives one press twice.** It gets the activating press in core and XI2 form and answers with two ReplayPointer (P4). This was seen on hardware, on NVIDIA and AMD, under #373 and #374 alike. It is in scope because the duplicate comes from the same activation joske named: `ActivatePassiveGrab` delivers once, in the winning grab's form. It is also the only defect here that a shipping client shows today.
- **Wrong form at activation.** An XI2 grab on the master is delivered as core plus XI2 (P1m, P2m).
- **Master events from a detached slave.** A slave passive grab still lets the master form of the activating event, or of its replay, reach other clients (P1, P2r). The master's button state is also updated for that event and is left held after the release (a first, discarded run of the cases on one server started P1m with the master button down).
- **Replay re-delivers forms that were already delivered.** A ReplayPointer of a master grab gives the slave form a second time (P3, the C19 case).
- **Grab selection differs from Xorg.** Across windows a master grab on an ancestor beats a slave grab on a descendant, where Xorg checks the slave first (P5). On one window yserver ranks grabs by device "specificity", where Xorg takes the newest matching grab.
- **A core GrabButton deletes an XI2 grab.** yserver's GrabButton handler replaces any grab of the same client, window, button and modifiers, whatever its protocol (`process_request/grabs.rs` ~1186). Xorg replaces only a grab of the same type, so the two coexist (P4c: after XIGrabButton, GrabButton and UngrabButton, Xorg still activates the XI2 grab; yserver activates nothing). P4b gives the same output as Xorg only because the XI2 grab is already gone.

The user added the two remaining grab-series follow-ups to this PR (2026-11-21):
- **XI1 implicit grabs (I5),** left out of #374. A client that receives an XI1 DeviceButtonPress gets no implicit grab, so the drag and the release go to whichever window is under the pointer: the client that got the press never gets its release, and another client gets a release without a press. #374 made core and XI2 implicit grabs per device; XI1 is the one form left without one.
- **XTest device motion does not move the sprite (C18),** left out of #373. An XTestFakeInput DeviceMotionNotify reaches only XI1 selectors of the XTest device; the sprite stays where it was and no master, core or XI2 event is produced (X1-X3).

## Xorg behaviour to match (21.1.24)

- **Two passes.** A slave event is processed for the slave first and then for its master (`../xserver/mi/mieq.c:502-508`). Each pass runs `CheckDeviceGrabs` (`../xserver/dix/events.c:4187`) over the sprite trace from the root down:
  - The slave pass matches grabs whose device is that slave or XIAllDevices.
  - The master pass matches grabs whose device is the master, XIAllMasterDevices or XIAllDevices, plus core grabs, because `checkCore` is only true for a master.
  - If the slave pass activates a grab, the slave is detached (`../xserver/dix/events.c:1622-1624`), and `mieq` skips the master pass for that event (`mieq.c:505-508`). So a slave grab anywhere in the trace beats every master or core grab (P5).
  - If the slave pass activates nothing, the slave event is delivered normally before the master pass runs (P3: B receives the slave press before A replays).
- **On one window, the newest matching grab wins.** `AddPassiveGrabToList` prepends (`../xserver/dix/grabs.c:564-565`), and `CheckPassiveGrabsOnWindow` takes the first grab that matches (P4, P4b). The first-button rule (`../xserver/dix/events.c:4200`) is unchanged.
- **Replacement only within one grab type.** A new passive grab replaces an existing one only when `GrabMatchesSecond` holds, and that first requires the same grab type (`../xserver/dix/grabs.c`, `GrabMatchesSecond`; `AddPassiveGrabToList` `:551-565`). Core and XI2 grabs with the same window, button and modifiers coexist (P4c).
- **A skipped master pass is skipped entirely.** When the slave pass detaches the slave, the master's whole processing procedure is skipped for that event, including `UpdateDeviceState` (`../xserver/Xi/exevents.c:1848-1850`, `:1922-1925`). The master's button state does not change.
- **Activation delivers once, in the grab's form.** `ActivatePassiveGrab` (`../xserver/dix/events.c:3833`) converts the event to the grab's type only (CORE, XI or XI2) and activates the grab. It then delivers to the grab client on `grab->window`, using the event's own filter as the mask. The grab's event mask and owner_events therefore do not decide whether the activating press arrives (P2, P2m), and no second form goes to the grab client (P1m, P2m, P4).
- **Replay reprocesses only the frozen device's event.** For ReplayPointer/ReplayDevice, `ComputeFreezes` (`../xserver/dix/events.c:1318`, replay at `:1331-1363`) reruns `CheckDeviceGrabs(replayDev, ...)` below the grab window. If nothing activates, it runs `DeliverDeviceEvents(..., replayDev)`. Only that device's forms are delivered:
  - a master grab's replay gives no slave form (P3);
  - a slave grab's replay gives no master or core form (P2r).
  The implicit grab that this delivery creates belongs to `replayDev`.
- **XI1 deliveries create implicit grabs too.** `ActivateImplicitGrab` (`../xserver/dix/events.c:2152-2196`, called at `:2423`) types a delivered DeviceButtonPress as an `XI` grab on the delivering device when no grab is active, with `ImplicitGrabMask`, so a slave is not detached. The grab ends with the release that leaves the device's buttons up (`../xserver/Xi/exevents.c:1928-1955`). Xorg has done this since MPX (`a511c445d`, 2007); R6.6 grabbed only for the DeviceButtonPressGrab class, and XTS `XSelectExtensionEvent` assertion 5 still checks that older rule (I5).
- **XTest device motion is pointer motion.** `ProcXTestFakeInput` treats detail 0 as absolute and detail 1 as relative (`../xserver/Xext/xtest.c:225-241`), copies the valuators with `valuator_mask_set_range` (`:248-284`, `:417`), and calls `GetPointerEvents` on the XTest slave; the sprite moves (`../xserver/dix/getevents.c:1399-1440`) and the events are processed slave then master like any motion (`xtest.c:434`). With one axis at `first_valuator=1` the copy reads the wrong index, so the stored Y is 0 and the sprite goes to y=0 (X3, and C18's payload).

## Design

Approach A, chosen by the user: targeted fixes in the existing activation and replay paths (`pointer_fanout.rs` ~1410-1545, `replay_frozen_pointer_event_to_state` ~417, `ServerState::find_passive_grab` `server.rs` ~4074), reusing the existing conversion and delivery helpers.

Invariants:

1. **Selection.**
   - Passive button grab selection runs a slave pass over the whole window chain, root first, before the master pass. Each pass matches the device sets listed above, and core grabs match only the master pass.
   - Within one window the most recently established matching grab wins. The insertion order of `button_grabs` is that order, and a replaced grab counts as newly established, as in `AddPassiveGrabToList`.
   - Device "specificity" no longer ranks grabs on one window.
   - The first-button rule and the `confine_to` viewability check stay.
   - A GrabButton or XIPassiveGrabDevice request replaces only an existing grab of the same protocol (core with core, XI2 with XI2, with Xorg's device matching for XI2). A grab of the other protocol on the same window, button and modifiers stays (P4c).
2. **Activation delivery.** The activating press goes exactly once to the grab owner, on the grab window, in the winning grab's protocol: core for GrabButton, XI2 for XIPassiveGrabDevice. This holds whatever the grab's event mask and owner_events.
   - It is never delivered a second time in another form, and it never goes through the natural walk.
   - Child and coordinates follow `FixUpEventFromWindow` toward the sprite, as the core path already does.
3. **Detached slave.** When the activated grab is on a slave, the activating event produces no master XI2 form and no core form for any client. The same holds for a replay of that grab (invariant 4).

   3a. **Master state.** That event also changes no master state: no master button transition and no master acceptance. A replay processes state only for the frozen device. After any P case's press and release, every device's held-button state is back to empty, and unrelated devices' held state is preserved. Today the master bit is set before the activation detaches the slave (`pointer_button_transition`, `pointer_fanout.rs` ~277-300, ~615-647), and a release while detached does not clear it. Filtering only the delivered master forms would leave the master logically pressed.
4. **Replay.** Replaying a frozen grab reprocesses only the frozen device's event:
   - Master or core grab: master and core forms only, plus the master pass of passive grab checks below the grab window.
   - Slave grab: slave forms only, plus the slave pass below the grab window.
   - Forms already delivered when the grab activated are not delivered again.
   - The implicit grab that the replay delivery creates belongs to the replayed device. This uses #374's per-device lifecycle.
5. **C19 guard.** The `slave_only_replay_press` special case (`pointer_fanout.rs` ~431) exists to stop a slave-only replay from creating a master implicit grab. With invariants 3 and 4 and #374's per-device grabs, that case cannot arise. Remove the guard unless a measured case still needs it. If it stays, the plan must name that case.
6. **XI1 implicit grab (I5).** When an XI1 DeviceButtonPress is delivered to a client by selection and the device has no active grab, that device gets an implicit grab for that client on the delivery window, as for #374's XI2 slave grabs:
   - **One grab per device.** It lives on the delivering device only; the slave stays attached. A device has at most one active grab whatever protocol created it: routing and occupancy checks for that device, including the natural slave XI2 delivery, see the XI1 implicit grab (sol r3 F2).
   - **Which form wins on one device.** When one press could be delivered to the same slave in XI2 form to one client and in XI1 form to another, the device's single delivery walk decides once, as in Xorg's `DeliverDeviceEvents`: the deepest window with a selection takes it, and on one window XI2 before XI1. The implicit grab follows that delivery, in that protocol.
   - **The master stays independent.** The XI1 grab on the slave does not stop the master from getting its own implicit grab when the same press delivers a core or XI2 master form to another client (#374). Today master activation refuses when `xi1_active_grabs` holds an entry (`pointer_fanout.rs` ~2596-2601); that check must not treat a slave's XI1 implicit grab as the master's.
   - **Delivery while active.** The device's events go to the grab client in XI1 form only, with owner_events false: `ActivateImplicitGrab` tests the core `OwnerGrabButtonMask` (`../xserver/dix/events.c:2176`), which an XI1 selection cannot set.
   - **Mask snapshot.** At activation the grab records the delivery window's merged XI1 selection for that device (`deviceMask`, `../xserver/dix/events.c:2185-2187`). Grabbed events are filtered against it (`:2064-2068`, `:4368-4375`). A client that selected Motion and Press only does not get a release (C10's selection). The release that leaves the buttons up still ends the grab when it is not delivered. #374's spec deferred exactly this (`2026-11-19-per-device-implicit-grabs-design.md:110`).
   - **End.** It ends with the release that leaves the device's buttons up, and on unmap of the grab window, as #374's slave implicit grabs do.
   - The doc comment on `xi1_route_device_event` that cites XTS for the opposite is replaced by the Xorg citation.
7. **XTest device motion (C18).** An XTestFakeInput DeviceMotionNotify for a pointer device moves the sprite and produces the same events as any motion of that device: absolute for detail 0, relative for detail 1.
   - It reuses the path the core FakeInput MotionNotify already takes (`process_request/xtest.rs` ~600-635, `HostInputEvent::PointerMotion`), not a new one.
   - An axis the request leaves out keeps its current value. The axes it carries take the values Xorg's valuator copy produces, so X3 moves the sprite to y=0 (the user chose Xorg here).
   - The XI1 DeviceMotionNotify and XI2 valuator payload stay as #373 measured them (C16-C18 match).
8. **Unchanged:** the crossing chain on activation (NotifyGrab), sync freeze and keyboard hold (`xi1_check_grab_for_syncs`), confine-to, detach and reattach of slave grabs, AllowEvents modes other than replay, and release-time deactivation.

## Goals

- I5 and X1-X3 match Xvfb in recipient, event type, device/source and event window, and X1-X3 also in the sprite position and child.
- P1, P1m, P2r, P2m, P3, P4, P4c and P5 match Xvfb in recipient, event type, device/source and event window. P2 and P4b still match. Every case runs from a known initial state (no button held) on a fresh server.
- Rust regression tests drive real requests (GrabButton, XIPassiveGrabDevice, AllowEvents, XIAllowEvents) and the production input entry (`pointer_event_fanout_to_state`) for every P case, P2 included.
  - P4b is a passing regression test asserting the core grab is the active one; P4c is the failing test for coexistence and newest-wins.
  - Each test also checks the end state: no grab left, no device frozen, and no button held on the master or the slave (invariant 3a).
  - I5 and X1-X3 get the same kind of test: real XSelectExtensionEvent or XTestFakeInput requests, the production input entry, and an end-state check (no grab, sprite where Xvfb put it).
  - Each test is seen failing on `d479e443`, except P2 and P4b, which guard existing behaviour.
  - Each fix has a mutation that turns its test red.
- No regression in C1-C20 (#373) or I1-I8 (#374), I5 aside.
- XTS: `XSelectExtensionEvent` assertion 5 is expected to fail its checks 5 and 6, as on Xorg. The PR says so and cites `a511c445d`. No other XTS change is intended.

## Non-goals

- XI1 passive grabs (GrabDeviceButton): their activation, and their ordering against core and XI2 grabs on one window. Not measured. No client we run uses them.
- Key grabs. `CheckDeviceGrabs` also handles keys; this PR touches button grabs only.
- The XI2 passive grab's event mask during the grab. The XIPassiveGrabDevice handler stores the mask as `0xFFFF_FFFF` (`process_request/xi/dispatch/grabs.rs` ~849), so events outside the client's mask are not filtered while the grab is active. Not measured here; recorded as a follow-up. Activation delivery (invariant 2) does not depend on the mask, so this PR is correct with either stored value.
- DontPropagate, the XI1 attempt for smooth-scroll Motion, XI1/core order: the walk follow-ups from #373.
- Crossings, touch.
- The DeviceButtonPressGrab and DeviceOwnerGrabButton classes: no measured case; they keep their current behaviour.

## Anti-goals

- A second activation path, or a per-protocol flag machine, where the existing activation block can carry the rules.
- Any new `YSERVER_*` environment variable.
- Changing what a client receives in cases the measurements already show matching (P2, C1-C20, I1-I8).
- A separate XTest sprite-motion path next to the core FakeInput one.

## Verification

- `cargo +nightly fmt`, `cargo clippy --workspace --all-targets -- -D warnings`.
- `cargo test --workspace --features xdmcp`, and the lavapipe `--ignored` suite.
- `tools/check-layout.sh`, `just man`.
- `pointer_fanout`, `grab` and `xi2_grabs` run 5 times under 16 busy loops.
- The oracle client runs P1-P5, P4b, P4c, I1-I8, X1-X3 and C1-C20 against Xvfb and the headless branch build, one fresh server per P case. C15 stays excluded on headless yserver, which has no second pointer slave (the oracle prints BLOCKED); `slave_grab_leaves_master_form_from_other_source` (`pointer_fanout/tests/owner_events.rs` ~674) remains its evidence, as in #374.
- Hardware, with permission first, real KMS on NVIDIA and AMD:
  - xfwm4: x11trace shows one XI2 press and one ReplayPointer per click-to-focus, and the dialog still gets press and release;
  - marco click-to-focus;
  - the #374 qBittorrent, Enlightenment and XFCE scenes as a regression check.

## Provenance

Behaviour is taken from Xorg server 21.1.24 (MIT/X11 licence) source citations, Xvfb measurements, and x11trace captures of xfwm4. No code is copied.

## Left to the plan

- How the two passes are expressed: one lookup that takes the pass, or two calls from the activation site.
- How the activation block suppresses the master and core forms after a slave activation, and how the replay limits itself to the frozen device's forms. The existing per-form routing is the reuse candidate.
- Request paths (sol r2): Xorg replaces with `GrabsAreIdentical` (`../xserver/dix/grabs.c:495-501`, `:551-555`), which also requires identical devices, and deletes with `GrabMatchesSecond` (`:615-619`). Both reject a different grab type. yserver's core `GrabButton` replacement (`process_request/grabs.rs` ~1186) and core `UngrabButton` (~1260) both omit the protocol; both must keep the other protocol's grab for P4c to pass. Keep matching and replacement distinct for XI2 wildcard devices.
- Test fixture reuse from the #373 and #374 test modules.
- I5: the representation (reusing `xi1_active_grabs`, which already has release-time deactivation, or #374's per-device slot, with a device-scoped grab view so no new grab table appears), and where the mask snapshot lives.
- C18: how the DeviceMotion branch hands the pointer path its absolute or relative position while keeping the measured XI1/XI2 valuator payload.
- Test design for joske's owner-events point: an async slave grab with owner_events where the natural walk would deliver to a different window of another client (P1), so a walk-routed activation is distinguishable from the grab-window delivery.
