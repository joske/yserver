# Passive grab activation and replay (C19)

Status: revision 3, sol review r2 APPROVED; awaiting the user's approval (sol review r1: F1 P4b diagnosis corrected and P4c added, F2 master-state invariant 3a, F3 C15 exception). Stacked on PR #374 (`feat/per-device-implicit-grabs`, `af690cdf`, rebased on master with #376; the measurements were taken on its pre-rebase tree `d479e443`, and #376 changed only GLX).
Measurements: `docs/superpowers/findings/2026-11-21-passive-grab-activation-measurements.md` (cases P1-P5, P4b and P4c; one fresh server per case).

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
6. **Unchanged:** the crossing chain on activation (NotifyGrab), sync freeze and keyboard hold (`xi1_check_grab_for_syncs`), confine-to, detach and reattach of slave grabs, AllowEvents modes other than replay, and release-time deactivation.

## Goals

- P1, P1m, P2r, P2m, P3, P4, P4c and P5 match Xvfb in recipient, event type, device/source and event window. P2 and P4b still match. Every case runs from a known initial state (no button held) on a fresh server.
- Rust regression tests drive real requests (GrabButton, XIPassiveGrabDevice, AllowEvents, XIAllowEvents) and the production input entry (`pointer_event_fanout_to_state`) for every P case, P2 included.
  - P4b is a passing regression test asserting the core grab is the active one; P4c is the failing test for coexistence and newest-wins.
  - Each test also checks the end state: no grab left, no device frozen, and no button held on the master or the slave (invariant 3a).
  - Each test is seen failing on `d479e443`, except P2 and P4b, which guard existing behaviour.
  - Each fix has a mutation that turns its test red.
- No regression in C1-C20 (#373) or I1-I8 (#374).

## Non-goals

- XI1 passive grabs (GrabDeviceButton): their activation, and their ordering against core and XI2 grabs on one window. Not measured. No client we run uses them.
- Key grabs. `CheckDeviceGrabs` also handles keys; this PR touches button grabs only.
- The XI2 passive grab's event mask during the grab. The XIPassiveGrabDevice handler stores the mask as `0xFFFF_FFFF` (`process_request/xi/dispatch/grabs.rs` ~849), so events outside the client's mask are not filtered while the grab is active. Not measured here; recorded as a follow-up. Activation delivery (invariant 2) does not depend on the mask, so this PR is correct with either stored value.
- DontPropagate, the XI1 attempt for smooth-scroll Motion, XI1/core order: the walk follow-ups from #373.
- XI1 implicit grabs (I5), crossings, C18, touch.

## Anti-goals

- A second activation path, or a per-protocol flag machine, where the existing activation block can carry the rules.
- Any new `YSERVER_*` environment variable.
- Changing what a client receives in cases the measurements already show matching (P2, C1-C20, I1-I8).

## Verification

- `cargo +nightly fmt`, `cargo clippy --workspace --all-targets -- -D warnings`.
- `cargo test --workspace --features xdmcp`, and the lavapipe `--ignored` suite.
- `tools/check-layout.sh`, `just man`.
- `pointer_fanout`, `grab` and `xi2_grabs` run 5 times under 16 busy loops.
- The oracle client runs P1-P5, P4b, P4c, I1-I8 and C1-C20 against Xvfb and the headless branch build, one fresh server per P case. C15 stays excluded on headless yserver, which has no second pointer slave (the oracle prints BLOCKED); `slave_grab_leaves_master_form_from_other_source` (`pointer_fanout/tests/owner_events.rs` ~674) remains its evidence, as in #374.
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
- Test design for joske's owner-events point: an async slave grab with owner_events where the natural walk would deliver to a different window of another client (P1), so a walk-routed activation is distinguishable from the grab-window delivery.
