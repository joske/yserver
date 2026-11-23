## Verdict

1 blocking, 0 major, 0 minor.

Coverage: INCOMPLETE

This is a design-review result, not implementation approval or evidence that code compiles or tests pass.

## Incorporation audit

| Prior finding | Result | Assessment |
|---|---|---|
| B-1 — Shared extension-entry lifetime | APPLIED | Task 5 now specifies shared XI1/XI2 creation order, retained entries, conditional removal, and measured reselection/XI2 rows (plan:196–223). |
| B-2 — DeviceButtonGrab exclusivity | TRADED | Task 5 adds the required same-window/device conflict rejection, but incorrectly extends rejection to whole-request atomicity. See B-1 below. |
| M-1 — Contradictory replay test state | APPLIED | Task 1 expects FrozenNoEvent after explicit regrab, adds a separate FrozenWithEvent scenario, and distinguishes the ownership and state conjuncts through mutations (plan:78–104). |

## Findings

### Blocking

**B-1 — Task 5 requires request atomicity that Xorg does not provide.**

The plan requires a conflicting DeviceButtonGrab selection to reject **the whole request before applying anything**, including selection order ([plan:225](/data/Projects/yserver-passive-grabs/docs/superpowers/plans/2026-11-23-passive-grab-review-fixes.md:225)). Its explanation makes the same claim at line 202.

Xorg instead applies selections in ascending device-ID order. `ProcXSelectExtensionEvent` calls `SelectForWindow` for each device and returns immediately when one fails; it does not undo earlier devices’ changes ([selectev.c:167–176](/home/ariel_santangelo/Projects/xserver/Xi/selectev.c:167)). The exclusive-mask check rejects the conflicting device before changing **that device’s** selection ([exevents.c:2672–2690](/home/ariel_santangelo/Projects/xserver/Xi/exevents.c:2672)).

Concrete sequence:

1. A holds DeviceButtonGrab on higher-numbered pointer H.
2. B submits one request selecting Press/Release on lower-numbered pointer L and DeviceButtonGrab on H.
3. Xorg installs B’s selection on L, creates B’s shared extension entry if necessary, then returns BadAccess for H.
4. The plan rejects both selections. A subsequent press on L therefore reaches B and creates B’s implicit grab on Xorg, but does neither under the plan.

This changes observable selection, delivery and ownership behavior. The authoritative spec ties XI1 implicit ownership to selection delivery ([spec:66–71](/data/Projects/yserver-passive-grabs/docs/superpowers/specs/2026-11-21-passive-grab-activation-design.md:66)); Task 5’s authorized expansion of the class non-goal ([spec:106](/data/Projects/yserver-passive-grabs/docs/superpowers/specs/2026-11-21-passive-grab-activation-design.md:106)) must carry Xorg’s failure semantics too.

**Smallest correction:** preserve earlier successful device updates when a later device conflicts. Reject the conflicting device without modifying its selection. Add one two-device oracle/regression row asserting BadAccess **and** the surviving lower-device selection and entry order. The existing single-device `grabconflict` row cannot distinguish these contracts.

### Major

None.

### Minor

None.

## Coverage and implementation checks

**1. Incorporation.** Compared all three prior findings against the actual task text. The replay correction and shared-entry model are incorporated. Exclusivity is incorporated with the transaction-semantics defect above.

**2. Architecture and cross-task contracts.** Examined replay admission, source/master acceptance, passive lookup, pre-move slave routing, XI1 owner selection, and XI1/XI2 selection storage. The plan generally reuses the existing request, activation and fanout paths. Additional creation-order metadata is justified because class sets cannot reconstruct shared-entry history. No second routing or activation mechanism is required.

**3. Safety, ownership and failure semantics.** Checked authorization before replay mutation, stored-event eligibility, deferred master processing, independent slave acceptance, selection lifetime and conflict handling. B-1 identifies a concrete failure-ordering violation. No additional memory/ABI defect was demonstrated within the inspected excerpts.

**4. Specification and verification.** Tasks 1–4’s stated contracts preserve the inspected replay, master-state, XTest and pre-move routing requirements ([spec:59–84](/data/Projects/yserver-passive-grabs/docs/superpowers/specs/2026-11-21-passive-grab-activation-design.md:59)). Request-driven packet assertions and the KMS test backend provide relevant evidence. Task 5 needs the mixed-device failure row above. Formatting, all-target clippy, workspace/lavapipe tests and layout checks are assigned to implementation. Their outcomes and platform buildability remain unverified.

**5. Changed-behavior provenance.** Read relevant definitions/comments and searched producers for freeze states, button acceptance, motion axes/class shapes, pre-move targets and selection storage.

| Changed item | Provenance and preservation assessment |
|---|---|
| Replay state/gate | Implementation history `f6c45fd6`; design commit `a1907fd1`, [freeze-state plan:7, 30–44](/data/Projects/yserver-passive-grabs/docs/superpowers/plans/2026-07-15-freeze-state-unification.md:7). Task 1 preserves unified-state authority and stored/pending separation. |
| Slave child and master acceptance | `30b2db33`, authoritative spec:59–83 and [parent-plan Task 5b](/data/Projects/yserver-passive-grabs/docs/superpowers/plans/2026-11-21-passive-grab-activation.md:420); original acceptance split `77ce93c8`, [registry spec:144–153](/data/Projects/yserver-passive-grabs/docs/superpowers/specs/2026-09-29-dynamic-xinput-device-registry-design.md:144). Tasks 2/4 preserve separate device processing. |
| XTest clamps/capacity | Clamp origin `8677f535`; last motion-path change `678e4136`, authoritative spec:74–77. Current registry shapes expose at most four axes, within the six-value payload. The [registry addendum:138–158](/data/Projects/yserver-passive-grabs/docs/superpowers/specs/2026-10-04-xi-registry-maintainer-review-addendum.md:138) preserves device-shape and validation requirements. **Original clamp rationale remains unresolved:** the introducing commit’s referenced status document supplies none. |
| XI1 implicit owner and canonical selections | `5533f5ab`, authoritative spec:66–72; storage shaping `6ce488c5`, [July 26 plan:191–197](/data/Projects/yserver-passive-grabs/docs/superpowers/plans/2026-07-26-protocol-silent-success-audit.md:191). Task 5 explicitly preserves mask snapshots, canonical per-window storage and existing notification behavior. |

**Reading budget:** 24/24 general excerpts; 12/12 provenance excerpts. Investigation stopped at that boundary.

**Unassessed ground:** the original clamp’s historical rationale is not certified. A narrowly scoped follow-up would recover that rationale or explicitly document its replacement by validated device-axis bounds. No builds, tests, benchmarks, installs or compilation experiments were run.