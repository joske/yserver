## Verdict

2 blocking, 1 major, 0 minor.

Coverage: INCOMPLETE

This is a design-review result, not implementation approval or evidence that code compiles or tests pass.

## Incorporation audit

| Prior review | Result |
|---|---|
| None | Check 1 skipped as requested. |

## Findings

### Blocking

**B-1 — Task 5 models selection order with the wrong entry lifetime.**

The [plan:187](/data/Projects/yserver-passive-grabs/docs/superpowers/plans/2026-11-23-passive-grab-review-fixes.md:187) removes a client from the order when its XI1 classes become empty; [plan:206](/data/Projects/yserver-passive-grabs/docs/superpowers/plans/2026-11-23-passive-grab-review-fixes.md:206) tests that reselecting makes it newest. Xorg does not consistently remove that entry.

In [Xorg `SelectForWindow`:2691–2698](/home/ariel_santangelo/Projects/xserver/Xi/exevents.c:2691), clearing the client’s masks frees its entry only if `ShouldFreeInputMasks` succeeds. That predicate considers the window’s aggregate selections, including other clients ([`:3258–3272`](/home/ariel_santangelo/Projects/xserver/Xi/exevents.c:3258)).

Concrete sequence: A selects Press/Release, then B selects them, producing list `[B,A]`. A clears its mask while B remains selected. Xorg retains A’s entry; A’s subsequent selection updates it in place. The last delivered client—and implicit owner—remains A ([`events.c:2294–2307`](/home/ariel_santangelo/Projects/xserver/dix/events.c:2294)). The plan removes and reinserts A, producing `[A,B]` and owner B.

The ordering also spans protocols: XI2 selection can create the same `InputClients` entry ([`XISetEventMask`:3333–3348](/home/ariel_santangelo/Projects/xserver/Xi/exevents.c:3333)). Tracking only XI1 selection creation loses that history.

**Correction:** define order by Xorg’s shared extension-client entry lifetime, including retained empty entries and XI2-created entries. Add oracle/test rows for both sequences before fixing expectations. Preserve the per-device ownership and mask-snapshot requirements in [spec:66–72](/data/Projects/yserver-passive-grabs/docs/superpowers/specs/2026-11-21-passive-grab-activation-design.md:66).

**B-2 — DeviceButtonGrab acceptance omits its exclusive-selection contract.**

[Plan:189–191](/data/Projects/yserver-passive-grabs/docs/superpowers/plans/2026-11-23-passive-grab-review-fixes.md:189) introduces acceptance/storage and preferential ownership, but specifies no conflict rejection. The existing selection path filters accepted classes and installs them without this exclusivity check ([yserver `events.rs`:361–437](/data/Projects/yserver-passive-grabs/crates/yserver-core/src/core_loop/process_request/xi/dispatch/events.rs:361)).

Xorg explicitly passes `DeviceButtonGrabMask` as an exclusive mask ([`selectev.c:173–176`](/home/ariel_santangelo/Projects/xserver/Xi/selectev.c:173)); another client selecting it on the same window/device receives `BadAccess` ([`exevents.c:2672–2683`](/home/ariel_santangelo/Projects/xserver/Xi/exevents.c:2672)).

Concrete sequence: A selects Press/Release plus DeviceButtonGrab; B subsequently selects the same classes. Following the plan, both flags are stored and newest-first delivery makes B the preferred owner. Xorg rejects B’s selection, preserving A’s exclusive claim.

**Correction:** add the per-window/device exclusive-selection check before replacing that device’s selection/order state. Test conflicting clients, permitted same-client reselection, and independent windows/devices. Task 5’s explicit override of [spec non-goal:106](/data/Projects/yserver-passive-grabs/docs/superpowers/specs/2026-11-21-passive-grab-activation-design.md:106) authorizes this class; its selection semantics must accompany acceptance.

### Major

**M-1 — Task 1’s required test state contradicts both its gate and the established freeze contract.**

The [test sequence:79–85](/data/Projects/yserver-passive-grabs/docs/superpowers/plans/2026-11-23-passive-grab-review-fixes.md:79) activates a passive sync grab, replaces it with an explicit sync `GrabPointer`, then requires `FrozenWithEvent` and a successful owner replay.

That replacement calls `xi1_check_grab_for_syncs` ([`grabs.rs:790–796`](/data/Projects/yserver-passive-grabs/crates/yserver-core/src/core_loop/process_request/grabs.rs:790)), which sets `FrozenNoEvent` ([`pointer_fanout.rs:3674–3677`](/data/Projects/yserver-passive-grabs/crates/yserver-core/src/core_loop/pointer_fanout.rs:3674)). Xorg makes the same transition ([`events.c:1429–1430`](/home/ariel_santangelo/Projects/xserver/dix/events.c:1429)). Retaining stored bytes does not make that state replayable.

The historical requirement is explicit: **“unified state is the sole authority”**—commit `a1907fd1`, [freeze-state plan:7](/data/Projects/yserver-passive-grabs/docs/superpowers/plans/2026-07-15-freeze-state-unification.md:7). Task 1’s correct `FrozenWithEvent` precondition therefore makes A’s later replay a no-op. Its required assertions cannot establish the intended contract without contradicting this provenance.

Additionally, removing the state conjunct in mutation M1 ([plan:91](/data/Projects/yserver-passive-grabs/docs/superpowers/plans/2026-11-23-passive-grab-review-fixes.md:91)) is not distinguished by B’s request: B fails the ownership conjunct either way.

**Correction:** retain the explicit-regrab panic scenario with `FrozenNoEvent` expectations and owner-replay no-op assertions. Add a separate genuine `FrozenWithEvent` scenario proving successful owner replay. These distinguish ownership from replay-state eligibility while preserving [spec:60–64](/data/Projects/yserver-passive-grabs/docs/superpowers/specs/2026-11-21-passive-grab-activation-design.md:60).

### Minor

None.

## Coverage and implementation checks

- **Check 1:** no prior review; skipped.
- **Check 2:** examined all five task contracts against relevant yserver and Xorg paths. Existing activation, routing and backend paths provide the intended reuse. Task 5’s ordering ownership is incomplete.
- **Check 3:** examined replay authorization before mutation, stored-event eligibility, source/master acceptance, and selection lifetime. The findings above give concrete ordering/state sequences; no additional demonstrated memory/ABI defect emerged from inspected excerpts.
- **Check 4:** Tasks 2–4’s stated invariants agree with inspected Xorg behavior. Packet assertions, production request/input entry points, mutation checks, nightly formatting, all-target clippy and workspace/lavapipe gates are assigned to implementation. Task 1’s evidence is contradictory; Task 5’s oracle rows omit the failing lifecycle/conflict cases.
- **Check 5:** searched state producers and traced freeze admission to `a1907fd1` and its July 15 plan; slave motion/master deferral to `30b2db33`, [spec:78–83](/data/Projects/yserver-passive-grabs/docs/superpowers/specs/2026-11-21-passive-grab-activation-design.md:78), and parent-plan Task 5b; XTest motion to `678e4136` and [spec:74–77](/data/Projects/yserver-passive-grabs/docs/superpowers/specs/2026-11-21-passive-grab-activation-design.md:74); XI1 implicit ownership to `5533f5ab` and spec invariant 6. Task 5 knowingly expands the class scope.

**Excerpts used:** 24/24 general; 12/12 provenance. Investigation stopped at that boundary.

**Unassessed provenance:** canonical XI1 selection storage traces to `6ce488c5` and the July 26 protocol-audit plan, which was located but not read; the older clamp rationale was not recovered beyond move history. These are not marked sound. A bounded follow-up should resolve whether ordering metadata preserves DevicePresence/global-notification selection updates and canonical per-window teardown, and whether removing the clamps preserves their original capacity contract.

No builds, tests, benchmarks, installs or compilation experiments were run. Actual test outcomes and platform buildability remain implementation checks.