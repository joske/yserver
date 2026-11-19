# Per-device implicit pointer grabs

Status: draft for review. Stacked on PR #373 (`feat/pointer-routing-xorg`, `48f69aab`).
Measurements: `docs/superpowers/findings/2026-11-19-per-device-implicit-grabs-measurements.md` (cases I1-I8).

## Intent

Suppose a user presses in a window, drags over another client's window, and releases there. Clients that select slave-form XI2 events (Qt6 with XIAllDevices, Enlightenment 0.27 with explicit slave ids) or XI1 device events must see the whole press/motion/release sequence on the window where the press happened. Xorg 21.1.24 does this, and yserver does not.
- **Lost slave release.** yserver sends the slave release and the slave drag motion to whatever window is under the pointer (I1, I1a, I5, I7).
- **Leaked master events.** yserver hands master-form (d2) events to a client that never selected the master (I2, I3, I6, I8).
- **Wrong grab status.** It answers XIGrabDevice with the wrong status during a drag (I4).

PR #373 named this as follow-up "finding 3". joske's #373 review placed the C19 replay gate with it; that and XI2 passive activation go to the next stacked PR (see Non-goals).

## Xorg behaviour to match (21.1.24)

- **One grab per device.** Each device has its own `deviceGrab`. The master pointer and each attached slave pointer hold independent implicit grabs.
- **Activation.** `ActivateImplicitGrab` (`dix/events.c:2152-2196`) runs for a device when that device's ButtonPress was delivered and the device had no grab.
  - The grab window is the window the press was delivered on. The owner is the receiving client.
  - The grab type is the delivered protocol: CORE for core ButtonPress, XI for DeviceButtonPress, XI2 for XI_ButtonPress.
  - `xi2mask` is the merged XI2 selection of that window.
- **Which deliveries count.**
  - Master: the core ButtonPress and the XI2 master form.
  - Slave: the XI2 slave form and XI1 DeviceButtonPress.
  - A delivery of one device's form never activates the other device's grab (I2, I3).
- **No detach.** An implicit grab does not detach the slave from its master (`dix/events.c:1622-1624`). Master events keep flowing and are routed by the master's own grab state (I2: B receives d2 on OUT while A holds the slave grab).
- **Under the grab.** The grabbed device's events go only to the grab owner, on the grab window, filtered by the grab's protocol and mask. The implicit grab's owner_events is set only by the core OwnerGrabButton bit, so XI/XI2 implicit grabs are not owner-events.
- **Release.** The grab ends after delivering the release that leaves THAT device's buttons-down set empty (`Xi/exevents.c:1928-1955`). Buttons pressed later, anywhere, stay with the grab (I7).
- **Termination by requests.** An implicit grab is a real active grab:
  - XIGrabDevice by another client on the grabbed device returns AlreadyGrabbed (I4 slave).
  - The master is unaffected by a slave's grab (I4 master: Success).
  - XIUngrabDevice by the owner ends it (I8).
  - Unmapping or destroying the grab window ends it (I6).
  - Disconnecting the owner ends it.

## Design

**Reuse first:**
- **Slave grab:** stored in the existing per-slave table `ServerState::xi2_pointer_grabs`, as an `ActivePointerGrab` with `implicit: true` (field exists).
- **XI1 implicit grab:** stored in the existing `xi1_active_grabs`, with an implicit marker on `Xi1ActiveGrab`.
- **Master grab:** stays in `active_pointer_grab`.
- **Consequence:** existing request and cleanup paths (XIGrabDevice AlreadyGrabbed, XIUngrabDevice, unmap, window destroy, disconnect, device removal) and the existing per-form routing (`grab_for_form`, the owner-events walk of #373, `xi1_selection_filter_for_slave_grab`) apply to implicit slave grabs without new mechanisms. A new table or a parallel lifecycle is an anti-goal.

**Invariants:**
1. **Master activation.** The master implicit grab is built only from master-side deliveries (core press, XI2 master form). A press whose only delivery was a slave form or XI1 leaves `active_pointer_grab` empty.
2. **Slave activation.** A slave implicit grab is built only from that slave's deliveries (XI2 slave form, XI1), and only when that slave has no grab.
   - If both an XI2 slave delivery and an XI1 delivery happened, the deeper window wins. On the same window, XI2 wins (Xorg tries XI2 before XI1 per window).
   - The XI2 case lives in `xi2_pointer_grabs`; the XI1 case lives in `xi1_active_grabs`.
   - At most one implicit grab exists per slave, in exactly one of the two tables.
3. **Independence.** Master activation does not depend on whether the source slave holds an implicit grab, and vice versa.
   - The existing early return for a source slave with an XI2 grab must keep holding for explicit and passive slave grabs, which detach the slave.
   - It must not suppress the master's implicit grab when the slave's grab is implicit.
4. **No detach.** Installing or removing an implicit slave grab never calls `detach_xi2_slave`. `xi2_detached_masters` is unchanged across its lifetime.
5. **Release.** A slave implicit grab is removed after the release that leaves that slave's per-device buttons-down set empty. The master implicit grab keeps its current rule: the master aggregate `buttons_down` reaches zero.
6. **Routing.** While a slave implicit grab exists, that slave's XI2 slave-form events and XI1 events go only to its owner on its window (I1, I2, I5, I7). This is the same routing an exclusive (non-owner-events) explicit grab of that protocol gets today. Master-form and core events follow the master's grab state only.
7. **Teardown.** Ending a slave implicit grab by request, unmap, destroy, disconnect or device removal leaves no `xi2_pointer_grabs` or `xi1_active_grabs` entry.
   - The next slave event routes naturally (I6, I8).
   - Teardown thaws no freeze, since an implicit grab is always async. It also reattaches nothing, since the slave never detached.
8. **Requests.**
   - XIGrabDevice on a slave held implicitly by another client: AlreadyGrabbed.
   - By the same client: it replaces the grab and becomes explicit, with the detach that explicit slave grabs already do.
   - Core GrabPointer and XIGrabDevice on the master ignore slave implicit grabs (I4).

## Goals

- I1-I8 match Xvfb in recipient, event type, device/source and event window, under the exclusions of the measurement doc.
- Rust regression tests drive real requests and the production input entry points (`pointer_event_fanout_to_state`) for: I1/I1a, I2, I3, I4, I5, I6, I7, I8. Each test must be seen failing on `48f69aab`.
- No regression in the #373 matrix C1-C20.

## Non-goals

- The C19 replay gate with core sync passive grabs, and XI2 passive-grab activation vs `ActivatePassiveGrab` (joske's #373 follow-ups): the next PR, stacked on this one.
- DontPropagate, the XI1 attempt for smooth-scroll Motion, XI1/core order: the walk PR.
- Which windows a press is delivered to: this PR takes the existing deliveries as given and only changes which grab they activate.
- Enter/Leave crossings for implicit grabs; attached-slave coordinate timing; C18.
- Touch, keyboard.

## Anti-goals

- A new grab table, flag-per-protocol state machine, or second release path where the existing ones can carry the implicit case.
- Any new `YSERVER_*` environment variable.
- Changing explicit or passive slave grab behaviour (detach, freeze, replay).

## Verification

- `cargo +nightly fmt`, `cargo clippy --workspace --all-targets -- -D warnings`.
- `cargo test --workspace --features xdmcp`, and the lavapipe `--ignored` suite.
- `tools/check-layout.sh`, `just man`.
- `pointer_fanout`, `grab` and `xi2_grabs` run 5 times under 16 busy loops.
- The oracle client (I1-I8 and C1-C20) runs against Xvfb and the headless branch build.
- Hardware, with permission first:
  - Enlightenment and qBittorrent (Qt6) drags that end over another client;
  - one XFCE drag, as a master-only control.

## Provenance

Behaviour is taken from Xorg server 21.1.24 (MIT/X11 licence) source citations and Xvfb measurements. No code is copied.

## Left to the plan

- The exact shape of the implicit marker on `Xi1ActiveGrab`.
- How the XI1 press delivery is reported to the lifecycle, in the same way `ImplicitGrabFanoutInfo` reports XI2 deliveries.
- Where the per-slave release check lives. The existing passive per-device release at `pointer_fanout.rs` ~3245 is the reuse candidate.
- Test fixture reuse.
