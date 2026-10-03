**Invocation verified:** Claude Code `claude-opus-5-5`, `--effort high`, read-only tools; successful completion, 20 turns, zero permission denials. **Code base:** `dbeb5a49`, incorporating `joske/master` at `736a8036`.

I've finished verifying the revised documents against the Xorg, driver and yserver sources; the full round-3 report follows.

# Round 3 adversarial review: dynamic XInput registry spec and plans

**Verdict: NOT CONVERGED.** Eight MEDIUM findings (R3-1 to R3-8) block convergence. None is HIGH. Most of the round-2 repairs are correct, but three of the new decisions carry verified defects:
- **VT continuation:** R3-1 through R3-4.
- **Property deletion:** R3-5.
- **Touch and keyboard master aggregation:** R3-6 through R3-8.

**Scope and method:**
- I re-read the four live documents, `AGENTS.md`, and the yserver code at `dbeb5a49`.
- I checked claims against `../xserver`, `../xorgproto`, `xf86libinput.c` 1.5.0, `input` 0.10.0 and `libinput.h`.
- I used read-only tools only. **No implementation tests were run and no files were written.**

**Abbreviations:**
- **SPEC** = design spec, **KP** = keyboard/pointer plan, **TP** = touch plan, cited by current line number.
- `backend.rs` = `crates/yserver/src/kms/render/backend.rs`.
- `xserver/…` = local Xorg checkout.
- **Verified** = read in source; **Inference** = reasoned; **Unverifiable** = source not available.

---

## MEDIUM (blocking)

### R3-1. A Pause immediately followed by Resume can leave every physical device suspended permanently
- **Affected:** KP Task 14 (KP:224, "marks all input inventory sources suspended at VtRelease in the same dispatch"), KP Task 3 (KP:94-96), SPEC:147-159.
- **Verified:**
  - `InputThreadControl.command` is a single `AtomicU8` latch: `pause()` stores 1, `resume()` overwrites it with 2 (`crates/yserver/src/input_thread.rs:306-316`), and `drain()` swaps it to 0 (`:333-336`).
  - The thread ignores `Resume` when it isn't paused (`:819-853`).
  - Before this design, losing that pair was harmless. Now the core suspends sources on its own at `VtRelease`, and only a `DeviceResumed` message re-enables them.
- **Failure sequence:**
  1. `on_vt_release` calls `pause()`; the core marks every source suspended and disables its facets.
  2. `on_vt_acquire` calls `resume()` before the input thread drains the latch.
  3. The thread sees only `Resume` while unpaused and does nothing, so no `DeviceResumed` is ever sent.
  4. The core rejects all physical input from then on (SPEC:121). **Inference:** the window is small and needs a busy or late input thread, but the outcome is total, permanent input loss.
- **Smallest correction (KP3):** make pause/resume an ordered FIFO in `InputThreadControl`, so every Pause gets its Resume and emits suspend/resume batches. Add a check for pause+resume arriving within one drain.

### R3-2. Configuration writes during VT suspension block the global lane; Xorg returns BadMatch
- **Affected:** SPEC:158 ("paused configuration commands wait for rebind"); KP Task 8 (KP:155, "retain queued backend commands until resume rebinds"); KP Task 3 (KP:96).
- **Verified:**
  - At VT leave, the driver drops the libinput device (`xf86libinput.c:1005-1026`, `416-432`), so `shared_device->device == NULL`.
  - Every setter's check pass then fails `xf86libinput_check_device` and returns BadMatch immediately (`:4385-4401`; accel profile example at `:4636-4637`).
  - Under the plan, one pending write keeps the single FIFO lane occupied for the whole time the VT is away (KP:153, 155).
- **Failure sequence:** A client runs `xinput set-prop` (or a settings daemon writes a property) while the user is on another VT. That client blocks; every other client's recognized write queues behind it and those clients block too, until the user returns. Xorg would have answered BadMatch at once.
- **Smallest correction:**
  - KP7/KP8: validation of a request targeting a suspended source returns BadMatch after value validation, preserving Xorg's error precedence. It is never submitted.
  - Only commands already submitted before the pause resolve after rebind or as SourceGone.
  - Fix SPEC:158 to match.

### R3-3. Continuation proof is limited to the initial resume drain, but device opens can arrive later
- **Affected:** KP Task 3 (KP:94, "drains its initial device enumeration … Emit DeviceRemoved for unmatched old endpoints first"), SPEC:149-157.
- **Verified in-tree evidence:** `input_thread.rs:705-711` and `882-885` document that a device open "can be DEFERRED by a lagging udev uaccess ACL" and completes on a later dispatch; the thread keeps a 2.5 s retry window for this.
- **Inference:** with seat ACLs (the alternative to the `input` group that `context.rs:176-179` names), switching back to the VT re-grants the ACL asynchronously. `libinput_resume` can therefore miss some devices in the first drain. How libinput defers the open internally is **unverifiable** here (no libinput source available).
- **Failure sequence:** the Razer's reopen is deferred. At resume the old source is declared unmatched, so Disabled then Removed is published. Later the Razer arrives as `DeviceAdded` with a new source, new XI ID and the global default config. That is exactly the ID/config loss R2-12 was meant to fix, on every VT switch in that configuration.
- **Smallest correction (KP3):**
  - Keep unmatched suspended sources pending until the existing hotplug retry window closes.
  - Match late adds by `resume_key` within that window; only then emit DeviceRemoved.
  - The kernel `inputN` key already proves identity, so time only bounds when removal is published.

### R3-4. VT suspend does more than Xorg's physical disable: XTEST holds are released and the XKB reset is kept
- **Affected:** KP Task 14 (KP:224, drain "all active physical and XTEST streams"; "retain the KMS XKB/hotkey reset on acquire"), KP Task 3 (KP:96), SPEC:128, 140-142.
- **Verified:**
  - Xorg disables only driver devices (`xf86InputDevs`) at VT leave and enter (`xserver/hw/xfree86/common/xf86Events.c:378-379`, `474-477`). XTEST devices and their held state are untouched.
  - Xorg keeps the master's locked XKB state and pushes it to re-enabled slaves (`xserver/dix/devices.c:434-435`).
  - The retained KMS reset replaces XKB state with `State::new` and clears `down_keys` (`backend.rs:20643-20653`), which drops Caps/Num Lock.
  - The reset was justified only by releases lost while away. The guarded drain the plan now adds already releases held keys through XKB, so the reset is no longer needed.
- **Inference (LED desync):** a re-added keyboard receives `last_leds` (`crates/yserver/src/input/context.rs:284-294`) while XKB state has been reset. The Caps LED can stay lit with Caps Lock logically off.
- **Smallest correction:**
  - Drain physical sources only; leave XTEST (4/5 and XTEST-targeted) held state alone.
  - Replace the XKB reset on acquire with the guarded drain alone, and push the current lock/LED state on resume.
  - Update SPEC:140-142.

### R3-5. Deletion via GetProperty is not blocked in Xorg, and the Task 7 map-entry requirement contradicts Xorg
- **Affected:** SPEC:198-201; KP Task 6 (KP:129, "protected deletion returns BadAccess" for GetProperty-delete); KP Task 7 (KP:140, "verifies the matching seeded map entry, which cannot have been client-deleted").
- **Verified:**
  - Direct delete is correct: `XIDeleteDeviceProperty` returns BadAccess for non-deletable properties (`xserver/Xi/xiproperty.c:657`).
  - `ProcXIGetProperty` and XI1 `ProcXGetDeviceProperty` send the normal reply and then unlink the property whenever `delete && bytes_after == 0`, without checking `deletable` (`xiproperty.c:1238-1250`, `1000-1012`). The deleted notification is sent from `:1218-1219` (XI2, only if `length`) and `:980-981` (XI1).
  - After that, a write recreates the property: `XIChangeDeviceProperty` creates it (`xiproperty.c:699-706`), and the libinput handler validates and applies it (`xf86libinput.c:5446-5560`).
- **Failure:** the plan returns an error where Xorg returns data (the reply is lost). Afterwards, Task 7 rejects valid writes forever because the seeded map entry is gone.
- **Smallest correction:**
  - GetProperty(delete) follows Xorg exactly: reply, delete, and the notification rules above.
  - Recognized-write support comes from the source's config snapshot only. A supported write to a deleted driver property recreates it after backend success, with a Created notification.
  - Direct DeleteProperty keeps BadAccess.

### R3-6. It is undefined which transitions update the master's XKB state
- **Affected:** KP Task 13 (KP:213, "Apply duplicate guards to the generating slave before XKB/device delivery").
- **Verified:**
  - Xorg runs XKB per device; the master's XKB ignores a press of an already-down key and a release of an up key, keyed on the master's own state (`xserver/xkb/xkbPrKeyEv.c:73-81`).
  - Master transitions follow `exevents.c:922-943`.
  - yserver has one XKB state per attached keyboard stream (`backend.rs:19331-19349`).
  - In-tree comment: xkbcommon counts every key-down, so a double press leaves a modifier set (`backend.rs:19320-19330`).
- **Failure sequence (inference from the wording):**
  1. Keyboards A and B both hold Shift. B's press passes its own slave guard and is fed to XKB.
  2. A releases Shift. The master release is delivered, but XKB still counts one Shift press.
  3. Keys typed on B afterwards are shifted, whereas Xorg releases master Shift at A's first release.
- **Smallest correction:** state that master XKB (and core modifier state) changes only on master transitions that are not suppressed. Slave guards gate slave-form XI events only; floating slaves use their own XKB state.

### R3-7. Touch-emulated buttons must not use master button aggregation
- **Affected:** KP Task 10 (KP:177, "Removal, suspend and emulated-touch releases use this same aggregation rule"); TP Task 9 (TP:154, "without releasing a button still held by another source").
- **Verified:**
  - Xorg tracks emulated buttons in the touch class (`t->buttonsDown`, `t->state`; `exevents.c:997-1025`).
  - Core state is `button->state | touch->state` (`xserver/dix/inpututils.c:773-774`, `788-789`).
  - Emulated Press/Release are delivered straight to the chosen listener (`exevents.c:1464-1522`), outside the `ET_ButtonRelease` master suppression rule (`:960-992`).
- **Failure sequence:** a mouse holds button 1 for a drag while the user taps a touchscreen. Under the plan, the emulated Release to the touch listener is suppressed and that listener sees Press without Release. Xorg delivers both.
- **Smallest correction:** keep a separate per-master touch-emulation button count (Xorg `TouchBegin`/`TouchEnd` accounting). Deliver emulated Release to its listener unconditionally. Physical and emulated state are combined only in the reported core state and button mask.

### R3-8. Tagging the whole restricted pointer queue also restricts Enter/Leave events that Xorg delivers normally
- **Affected:** TP Task 9 (TP:154, "pointer builders receive origin/delivery and the drain honors each entry's tag … Extend process_pointer_absolute with the delivery argument"); SPEC:283-284.
- **Verified:**
  - Xorg moves the sprite for the emulating touch via `CheckMotion` (`exevents.c:1681-1682`).
  - `CheckMotion` produces ordinary `DoEnterLeaveEvents(NotifyNormal)` crossings (`xserver/dix/events.c:3247-3250`) to every selecting client.
  - In yserver, crossings are built by `emit_crossing` inside `process_pointer_absolute` → `dispatch_motion_event` (`backend.rs:13906-13918`, `13780-13806`), so they would inherit the TouchListener tag.
- **Failure sequence:** a touch drags the sprite across windows. A focus-follows-mouse window manager (i3 defaults to focus-follows-mouse) gets no EnterNotify, so focus doesn't follow.
- **Smallest correction (TP9):** only the emulated Motion and Button events carry the listener restriction. Crossing events, cursor updates and XFIXES cursor notifications use Normal delivery.

---

## LOW (non-blocking)

- **R3-9. Last-slave record not cleared on suspend.** Xorg clears `lastSlave` and `last.slave` on disable (`devices.c:504-507`, `525-528`), so the first event after resume emits a SlaveSwitch. KP14/15 clear it only on removal (KP:230, 239). This is a small verified divergence; since per-source scroll values persist, no client-visible state differs apart from one redundant event. Add "and on suspend".
- **R3-10. Resync removal is only implied.** KP11 (KP:191) says it avoids "repeated resync" but does not say to remove `push_position` calls (`backend.rs:14085-14091`, `27980-27984`) or the pending-motion drop at `input_thread.rs:810-818`. If they stay, confined relative motion still loses coalesced deltas. State the removal.
- **R3-11. TP1 scope and wording:**
  - TP1 asserts cursor movement for mixed pointer+touch facets (TP:60, 64), but the KMS integrator lives in `backend.rs`, which is not in TP1's Files (TP:56).
  - "Retains only its actually supported ScrollClasses" (TP:60; SPEC:261) differs from the driver, which sets both scroll valuators unconditionally (`xf86libinput.c:1114-1115`). libinput has no per-axis support query, so say "both".
- **R3-12. Minor precision points:**
  - The resume restoration must run after `configure_touchpad` (`context.rs:221-224`), or tap is forced back on.
  - Xorg maps root = native·W/65536 with native = transformed(65535) (`getevents.c:299-321`, `xf86libinput.c:2003`); norm·W differs by less than 1/65536 of W.

---

## Audit of round-2 findings

| ID | Status | Evidence / residual |
| --- | --- | --- |
| R2-1 | Resolved in design | One guarded synchronous owner; eight releases deleted (KP:96, 224). Residual: R3-1, R3-4 |
| R2-2 | Resolved (different repair, sound) | KMS integrates `motion_delta`; per-slave XKB state (KP:90, 191). Residual: R3-8, R3-10, R3-11 |
| R2-3 | Resolved for physical keys and buttons | Matches `exevents.c:922-992` (KP:177, 213). Residual: R3-6 (XKB), R3-7 (touch) |
| R2-4 | Resolved | Node carried through Task 5, dropped in Task 6 (KP:77, 88, 133) |
| R2-5 | Resolved | run_core owns the lane; `pub(super)` helper; `generation.rs`/`reset.rs` in Files (KP:140, 151-155). Residual: R3-2 |
| R2-6 | Resolved | TP:97, 121; SPEC:268-272 match `getevents.c:2026-2048` |
| R2-7 | Resolved | Master copies native values; touch-only class list pinned (TP:60). Wording nit: R3-11 |
| R2-8 | Resolved | Full RandR root; only the emulating contact is confined; raw values pre-confinement (TP:73, 119, 154) match `getevents.c:2047-2060` |
| R2-9 | Resolved | Swap work in KP:186, 191; TP:106, 112, 130, 136 |
| R2-10 | Resolved | SPEC:46-50, 117-120; KP:166, 202 |
| R2-11 | Partial | Direct delete and read-only rules correct; GetProperty-delete refuted: R3-5 |
| R2-12 | Resolved in principle | Continuation keyed on the kernel `inputN/eventM` sysfs path, with no heuristics. Uniqueness of `inputN` is standard kernel behaviour, **unverifiable** here (kernel source not in references). Residual: R3-1 to R3-4 |
| R2-13 | Resolved | KP:261 matches the verified constructor chain |
| R2-14 | Resolved | KP:140; SPEC:217-220 match `xtest.c:589-597`; keyboard metadata in KP:129 |
| R2-15 | Resolved | Xorg absolute-axis conversion adopted (TP:60). Scope nit: R3-11 |

**New decisions checked and confirmed:**
- A proven continuation emits Disabled/Enabled only (KP:237) and XTEST devices are not disabled, matching Xorg's `DisableDevice`/`EnableDevice` at `xf86Events.c:304-323`.
- Paused physical input is rejected before any state changes (SPEC:121).
- Stray removal events while paused are handled by retiring handle bindings.
- The runner-owned lane with generation stamping and `protocol=None` across reset is well defined.
- Big-endian decode now covers the variable XISelectEvents headers and the passive grab/ungrab modifier arrays.

**Task sufficiency:** the corrections above land in KP 3, 7, 8, 10, 13, 14 and TP 1, 9. Every other task now has concrete interfaces, and its Files list covers its staging boundary.

## Verdict

**NOT CONVERGED.** Blocking findings: **R3-1 to R3-8** (MEDIUM). R3-9 to R3-12 do not block. No implementation tests were run. Hardware and client behaviour remain future acceptance checks.

## Reviewed document fingerprints

```json
{
  "docs/superpowers/specs/2026-09-29-dynamic-xinput-device-registry-design.md": "cdf836d6246ded7d0bd9b1295704c64acfdb51cefae07f151c640eee3f3b1e11",
  "docs/superpowers/plans/2026-09-29-dynamic-xinput-keyboard-pointer.md": "9d763755309830c703279ed0577f71817748ad8b4a738cd4d271dd7617f7a27f",
  "docs/superpowers/plans/2026-09-29-dynamic-xinput-touch.md": "edea6b68ae9e63f1304bb4d325992d554146bc55ae178368fef9a665468b3157"
}
```

## Author disposition after round 3

These corrections apply to live documents after the reviewed fingerprints
above. They were checked against the cited source; no Rust implementation
has started. This disposition is not an independent convergence verdict.

| Finding | Disposition | Owning tasks |
| --- | --- | --- |
| R3-1 | Accepted. Replace the overwritten AtomicU8 latch with ordered pause/resume FIFO commands and forward each batch even when both commands are drained together. | KP 3 |
| R3-2 | Accepted. New suspended-source writes fail in Xorg validation order at runner admission, without parking behind the lane. Recheck/cancel unsubmitted entries on suspension. Only already-submitted operations may resolve after rebind. | KP 3, 7, 8; spec |
| R3-3 | Accepted. Keep unmatched provable sources disabled during a fixed 2500 ms resume window, retry every 250 ms even after empty initial enumeration, match late adds, and retire unmatched sources at deadline. Unrelated hotplug cannot extend the proof window. | KP 3; spec |
| R3-4 | Partly accepted and corrected against source. Preserve virtual XTEST holds and master locks/LEDs; remove fresh-XKB reset. The suggestion to preserve *all XTEST-targeted* holds is too broad: DisableDevice calls ReleaseButtonsAndKeys(dev) (dix/devices.c:483, 2626-2668), including holds injected on a physical target. Drain by disabled target device, not origin; enabled masters and virtual XTEST remain untouched. | KP 3, 14; spec |
| R3-5 | Accepted. GetProperty-delete returns/unlinks as Xorg does, with its XI1/XI2 notification differences. Supported writes recreate deleted properties after backend acknowledgment. XICreateDeviceProperty's deletable=true default is retained on recreation, while descriptor identity still enforces read-only/support. Resume refreshes only present properties; a successful completion updates only its affected property. | KP 6, 7, 8; spec |
| R3-6 | Accepted. Slave XI guards and master XKB guards are explicit and separate; only accepted master transitions change master XKB/core state. Add the two-keyboard Shift check. | KP 13; spec |
| R3-7 | Accepted. Emulated touch count/state is separate from physical buttons and mirrors TouchBegin/End after ownership handling. Listener Release is never suppressed by a mouse hold; combine states only for reported masks. | KP 10; TP 9; spec |
| R3-8 | Accepted. Only emulated Motion/Button receive listener restrictions; crossings, cursor updates and XFIXES delivery remain normal. Add explicit TouchStateOnly delivery for sprite movement under a touch owner, retaining Xorg's no-listener Motion fallback and excluding duplicate raw pointer events. | TP 9; spec |
| R3-9 | Accepted. Clear master last-slave records on disable/suspend as well as removal so resumed input produces SlaveSwitch. | KP 14, 15 |
| R3-10 | Accepted. Explicitly remove producer pending_position, push_position/take_position and motion-discard resync; migrate KMS trait call sites while keeping other backends' hook. | KP 11 |
| R3-11 | Accepted. TP1 now includes the KMS integrator file and both unconditional pointer scroll classes, matching driver initialization. | TP 1; spec |
| R3-12 | Accepted. Restore saved settings after configure_touchpad, and map native=norm×65535 to root=native×extent/65536 with a fractional assertion. | KP 3; TP 2, 6; spec |

The R2 disposition's GetProperty-delete protection and its "all active
physical and XTEST streams" VT cleanup are superseded by this round.
The review snapshot remains NOT CONVERGED pending independent verification
of these corrected live documents. No implementation tests were run.
