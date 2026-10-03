# Local verification of the two unfinished XI design questions

**Scope:** source audit and reproducible acceptance scenarios, 2026-09-30,
on yserver `dbeb5a49`. Further Opus review is canceled by user instruction.
This document does not claim runtime reproduction or a passing implementation.
No Rust changes, implementation tests or live VT/reset operations were performed.

## 1. Reset and held input state

### Verified source evidence

- `crates/yserver-core/src/core_loop/reset.rs:308`: reset_generation keeps
  its existing `&mut dyn Backend`. It tears down clients, constructs fresh
  ServerState, replays input inventory and replaces `*state` at lines 404–426.
- `crates/yserver-core/src/backend/mod.rs:159`: root binding only assigns
  host resource identifiers; it performs no held-state cleanup.
- `crates/yserver/src/kms/render/backend.rs:19331`: duplicate-key validation
  reads `self.core.down_keys`; lines 19346–19348 mutate that backend set.
  XKB state and the button mask also belong to this persistent backend.
- Existing cleanup is `synthesize_held_releases` at backend.rs:12038,
  called by VT run_suspend (12483), not by reset_generation.
- Xorg's `dix/main.c:306` calls CloseDownDevices during teardown.
  `dix/devices.c:1017` frees device classes/grabs/frozen events;
  CloseDownDevices (1108) closes both enabled/disabled lists and masters.

### Conclusion and required design contract

The current reset path has no explicit input-session cleanup in KMS.
The proposed dynamic plan adds per-device held state but its Task 18
specifies only registry/property replay. This is a verified design gap;
symptoms from reused XI IDs are an inference until implementation exists.

Before the old registry/resources are replaced, reset must retire its
held state, repeats, pending pointer/key events and touch/listener state.
Backend maps keyed by old XI IDs must not survive into the rebuilt registry.
Preserve process-lifetime sources and confirmed libinput config; submitted
configuration acknowledgments retain the separate Task 8 contract. Specify
reset XKB/LED initialization explicitly, separately from VT lock preservation.

### Acceptance sequence

1. Register two keyboards and two mice; change one source's Accel Speed.
2. Hold Shift on a keyboard and button 1 on a mouse, with an active grab;
   include a virtual XTEST hold and a floating keyboard's modifier state.
3. Request a generation reset; rebuild facets so a live source gets an
   XI ID formerly used by a different source.
4. Assert no old down sets, repeat, grab, queued input or touch owner is
   carried by that new ID; source identity and confirmed config survive.
5. Deliver late releases and a fresh press/release: no unrelated source
   transition, no stuck modifier/button and no duplicate terminal event.
6. Repeat reset while VT is suspended: sources remain disabled until their
   real continuation and no old XI ID is used by delayed lifecycle events.

This needs a KMS-backed reset harness as well as core registry tests; a
RecordingBackend-only test cannot prove its persistent KMS state is cleared.

## 2. Back-to-back VT continuation windows

### Verified source evidence

- `input_thread.rs:306–335`: the current single AtomicU8 Pause/Resume latch
  overwrites commands. Task 3 already prescribes FIFO replacement.
- `input_thread.rs:819–852`: Pause/Resume use the paused-state guard;
  retry timing is a thread-local deadline at 711 and 883–885.
- `core_loop/generation.rs:64–81`: HostInput and VT messages are
  process-lifetime, so server-generation filtering cannot identify an old
  VT cycle on their own.
- KP Task 3 specifies a fixed 2500 ms resume proof window and expiry of
  unmatched endpoints, but omits what a new Pause does to an unfinished
  window and its pending source facts.

### Conclusion and required design contract

The overwritten-command defect has a proposed FIFO repair. The remaining
question is a separate, verified omission in the proposed state machine;
there is no new implementation to reproduce yet.

A second Pause must preserve unmatched suspended-source facts/config,
retire the preceding window's timer, and define which new cycle may rebind
or expire them. A stale timer/lifecycle result cannot remove or enable a
source belonging to the later cycle. Reset must preserve this process-level
recovery bookkeeping while discarding its old XI IDs. Failure/retry of
libinput_resume also needs to retain paused facts until a successful resume
starts its actual window.

### Acceptance sequence with an injected clock

1. Queue Pause → Resume → Pause → Resume before one control drain.
2. In the first resume, reopen A immediately and delay B.
3. Pause again at 100 ms while B is unmatched; retain B's source/config.
4. Start the second resume at 200 ms and reopen B within its current window.
5. Fire the first window's old deadline: B must remain enabled with its
   original source/XI identity, and no extra Removed/Added notification.
6. In another run, B stays absent: remove it exactly once at the active
   window's deadline; unrelated hotplug must not postpone that deadline.
7. Repeat with a genuine unplug/replug using the same eventN and a different
   kernel endpoint key: the replacement must receive a fresh source.
8. Repeat with failed libinput_resume and with a server reset during the
   window; no false activation and no loss of recovery facts.

Use a fake endpoint resolver and injected monotonic clock; correctness
must not depend on sleeps or real udev timing. Check source IDs, enabled
state, configuration, lifecycle ordering and release counts at each step.

## Evidence boundary

Source audit identifies the omissions and defines falsifiable acceptance
criteria. Deterministic implementation tests establish those invariants;
real keyboard/mouse VT and reset traces provide the final integration check.
The design questions are not closed by this checklist alone. Their explicit
contracts still need to be added to the implementation plan.

## Implementation authorization and local rulings

The user authorized implementation on 2026-09-30. The design decisions
above now have explicit interfaces and acceptance cases in KP Tasks 3/18:
- Token-checked single recovery window; a new Pause retains unmatched facts
  and invalidates the old deadline; failed Resume keeps facts paused.
- Backend::reset_input_session before old client/facet teardown, retiring
  session-held state while preserving source/config/recovery and submitted
  configuration tokens. Reset initializes fresh XKB/LED session state.
The touch plan hooks contact/ownership cleanup into that same reset path.
These are locally checked design contracts; runtime proof remains part of
implementation verification. No independent Opus verdict is claimed.
