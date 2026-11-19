# Per-device implicit pointer grabs

Status: draft, revision 2 (sol review r1: R4-R6 and R1's XI2 part folded in; XI1 moved out pending the user's decision). Stacked on PR #373 (`feat/pointer-routing-xorg`, `48f69aab`).
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

**Reuse first.**
- **Slave grab:** stored in the existing per-slave table `ServerState::xi2_pointer_grabs`, as an `ActivePointerGrab` with `implicit: true` (the field exists).
- **Master grab:** stays in `active_pointer_grab`.
- **Routing:** the per-form routing reads the slave's grab through `grab_for_form(Slave)`, so the owner-events walk of #373 applies unchanged. This covers ordinary and smooth-scroll Motion and the `xi1_selection_filter_for_slave_grab` filter.
- **Cleanup:** the existing request and cleanup paths apply too, with the implicit-aware removal of invariant 7.
- **Anti-goal:** a new table or a parallel lifecycle.

Invariants:

1. **Master activation.** The master implicit grab is built only from master-side deliveries: core press and XI2 master form.
   - A press whose only delivery was a slave form leaves `active_pointer_grab` empty.
2. **Slave activation.** A slave implicit grab is built only from that slave's XI2 slave-form press delivery.
   - It activates only when that slave has no grab in either `xi2_pointer_grabs` or `xi1_active_grabs`.
   - The candidate choice among several slave-form deliveries is the existing `consider_xi2_press` rule.
3. **Per-device mask snapshot.** Each implicit grab's `xi2_mask` is the merged selection of its window as seen by THAT device. This follows `xi2mask_isset`, `../xserver/dix/inpututils.c:1153`, as used by grab delivery at `../xserver/dix/events.c:4364`:
   - for the slave: the union over all clients of the window's selections for the slave's id plus XIAllDevices (0);
   - for the master: the master's id (2) plus XIAllDevices (0) plus XIAllMasterDevices (1).
   - Today's capture ORs all four into both grabs; that is a defect for both grabs.
   - The core `event_mask` of the master grab is unchanged.
4. **Independence.** Master activation does not depend on whether the source slave holds an implicit grab, and vice versa.
   - The existing early return when the source slave has an XI2 grab keeps holding for explicit and passive slave grabs, which detach the slave.
   - It must not suppress the master implicit grab when the slave's grab is implicit.
5. **No detach.** Installing or removing an implicit slave grab never calls `detach_xi2_slave` or `reattach_xi2_slave`. `xi2_detached_masters` is unchanged across its lifetime.
6. **Release.** A slave implicit grab is removed after delivering the release that leaves that slave's per-device buttons-down set empty. The master implicit grab keeps its rule: removed once the master aggregate `buttons_down` reaches zero.
7. **Implicit-aware removal.** Every path that removes a `xi2_pointer_grabs` entry removes an implicit one with a bare removal, as the master implicit grab already does today. Xorg's `DeactivatePointerGrab` clears only holds that point at the dying grab (`../xserver/dix/events.c:1698-1701`), and an implicit grab is async and owns none. The bare removal must not:
   - release freeze bridges or other holds (`xi1_core_grab_bridge_release`, `xi1_frozen` owner fields);
   - clear the backend grab cursor;
   - reattach the slave.

   The paths that must use it:
   - XIUngrabDevice (`deactivate_xi2_device_grab`);
   - window destroy;
   - disconnect (`release_active_grabs`);
   - source and facet removal (`xi_cleanup_source`, `xi_unregister_facet`, `xi_unregister_source`);
   - the per-device release of invariant 6.

   One shared helper used by all of them is preferred over per-site branches.
8. **Unviewable windows.** Making a grab window unviewable ends every `xi2_pointer_grabs` entry on it, implicit or not (I6). This holds for UnmapWindow, UnmapSubwindows and the unmap of an ancestor. Xorg reaches it through `DeleteWindowFromAnyEvents` at unrealize (`../xserver/dix/window.c:2806`, `../xserver/dix/events.c:5926`).
   - Today, unmap releases `xi1_active_grabs` and the master slots but not `xi2_pointer_grabs`.
   - The fix extends the existing shared unviewable cleanup (`release_core_grabs_for_unviewable`, `process_request/grabs.rs`), which both unmap requests already call.
   - Explicit slave grabs gain the same Xorg-matching release.
9. **Requests.** As in Xorg `GrabDevice` (`../xserver/dix/events.c:5288-5291`):
   - **XIGrabDevice on a slave held implicitly by another client:** AlreadyGrabbed.
   - **XIGrabDevice by the same client:** it replaces the grab, which becomes explicit and detaches like any explicit slave grab.
   - **XI1 GrabDevice on a slave whose implicit grab is XI2:** AlreadyGrabbed, even for the same client, because the protocols differ. The XI1 GrabDevice occupancy check therefore also consults `xi2_pointer_grabs`.
   - **XIUngrabDevice by the owner:** ends the grab (I8).
   - **Core GrabPointer and XIGrabDevice on the master:** ignore slave implicit grabs (I4).

## Goals

- I1-I8 match Xvfb in recipient, event type, device/source and event window, under the exclusions of the measurement doc.
- Rust regression tests drive real requests and the production input entry points (`pointer_event_fanout_to_state`) for: I1/I1a, I2, I3, I4, I6 (UnmapWindow and UnmapSubwindows), I7, I8. Further tests cover:
  - differing slave and master masks on the press window (invariant 3);
  - removing a slave implicit grab leaves a live master grab's paired freeze hold and cursor intact (invariant 7);
  - XI1 GrabDevice over an XI2 implicit grab (invariant 9).

  Each test must be seen failing on `48f69aab`.
- No regression in the #373 matrix C1-C20.

## Non-goals

- The C19 replay gate with core sync passive grabs, and XI2 passive-grab activation vs `ActivatePassiveGrab` (joske's #373 follow-ups): the next PR, stacked on this one.
- DontPropagate, the XI1 attempt for smooth-scroll Motion, XI1/core order: the walk PR.
- Which windows a press is delivered to: this PR takes the existing deliveries as given and only changes which grab they activate.
- Enter/Leave crossings for implicit grabs; attached-slave coordinate timing; C18.
- Touch, keyboard.
- XI1 implicit grabs (I5), pending the user's decision.

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

- The XI1 section, if the user keeps XI1 in this PR (see review r1, R1-R3).

- Where the per-slave release check lives. The existing passive per-device release at `pointer_fanout.rs` ~3245 is the reuse candidate.
- Test fixture reuse.
