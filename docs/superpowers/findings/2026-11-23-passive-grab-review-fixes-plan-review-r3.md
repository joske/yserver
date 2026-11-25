## Verdict

1 blocking, 0 major, 0 minor.

Coverage: INCOMPLETE

The remaining defect concerns Task 5’s XI2 entry-creation contract. Historical rationale for the original XTest clamps remains unresolved. This is a design-review result, not implementation approval or evidence that code compiles or tests pass.

## Incorporation audit

The table includes R2’s finding and the earlier findings explicitly carried in R2’s audit.

| Prior finding | Result | Assessment |
|---|---|---|
| R2 B-1 — Incorrect whole-request atomicity | APPLIED | Plan lines 202, 226 and 243 preserve successful lower-ID device updates, reject the conflicting device, and skip higher IDs. The measured `twodev` row and M14 distinguish this behavior. |
| Carried B-1 — Shared extension-entry lifetime | PARTIAL | Shared XI1/XI2 order, retained entries and teardown are specified. Creation still conflates two observably different XI2 requests; see B-1 below. |
| Carried B-2 — DeviceButtonGrab exclusivity | APPLIED | Task 5 accepts the class, checks same-window/device conflicts, preserves the conflicting device’s existing selection, and specifies Xorg’s partial application order. |
| Carried M-1 — Contradictory replay state | APPLIED | Task 1 distinguishes explicit regrab’s `FrozenNoEvent` from a separate genuine `FrozenWithEvent` scenario. M1 and M2b separately exercise state and ownership admission. |

## Findings

### Blocking

**B-1 — Task 5 conflates zero-length XI2 clears with requests that create an entry.**

The [Task 5 invariant, lines 220–221](/data/Projects/yserver-passive-grabs/docs/superpowers/plans/2026-11-23-passive-grab-review-fixes.md:220) says the first XI2 selection creates the client’s shared extension entry.

Xorg’s condition is more specific: `XISetEventMask` creates an entry only when the supplied **byte length is nonzero** and no entry exists. A positive-length mask containing only zero bits creates an entry; a zero-length mask does not. Existing entries remain in place. See [exevents.c:3333–3348](/home/ariel_santangelo/Projects/xserver/Xi/exevents.c:3333) and [3351–3362](/home/ariel_santangelo/Projects/xserver/Xi/exevents.c:3351). The caller passes the request’s actual mask length at [xiselectev.c:314–315](/home/ariel_santangelo/Projects/xserver/Xi/xiselectev.c:314).

Concrete sequence on a fresh window W:

1. A sends XISelectEvents with one device mask whose `mask_len` is zero.
2. B selects XI1 DeviceButtonPress/Release on W.
3. A selects the same XI1 classes.
4. A press and release occur over W.

Xorg creates B’s entry first, then A’s. Delivery order is A, B, so B becomes the implicit-grab owner and receives the release. Following the plan’s unconditional first-XI2-selection rule records A first, making A the owner instead.

This changes the owner and subsequent release recipient under the behavior Task 5 must implement. The authoritative [spec:66–72](/data/Projects/yserver-passive-grabs/docs/superpowers/specs/2026-11-21-passive-grab-activation-design.md:66) ties the implicit grab to selection delivery and requires subsequent events to follow that owner.

There is also a concrete information handoff to specify: the current XI2 parser normalizes each request mask into `(deviceid, u64)`, discarding its length, and treats both forms as numeric zero ([events.rs:102–107, 130–133](/data/Projects/yserver-passive-grabs/crates/yserver-core/src/core_loop/process_request/xi/dispatch/events.rs:102)). The normalized selection map cannot distinguish them.

**Smallest correction:** preserve whether each applied XI2 mask had positive wire length until entry creation is recorded. Zero-length masks must not create an absent entry; positive-length all-zero masks must create it; neither moves an existing entry. Add paired request-driven rows differing only in mask length, followed by the B-then-A XI1 selections above. Existing `xi2first` and `xi2clear` rows do not distinguish this contract.

### Major

None.

### Minor

None.

## Coverage and implementation checks

**1. Incorporation.** Compared R2’s finding and its carried corrections against actual task requirements. The ascending-device failure correction is incorporated, including the measured row and mutation. Shared-entry creation remains incomplete as described above.

**2. Architecture and cross-task contracts.** Inspected replay admission/context capture, passive lookup, source/master acceptance, slave Motion geometry, XI1 owner selection, and XI1/XI2 selection writes. The plan reuses the existing activation, replay and delivery paths. Creation-order metadata is justified because canonical mask sets cannot reconstruct entry history. B-1 requires carrying one existing wire distinction to that metadata.

**3. Safety, ownership and failure semantics.** Checked ownership before replay mutation, unified-state eligibility, deferred master processing, independent slave acceptance, selection retention, and per-device conflict application. No additional memory/ABI defect was demonstrated in the inspected excerpts. That is limited evidence, not certification of uninspected paths.

**4. Specification and verification.** Tasks 1–4 preserve the inspected requirements for device-restricted replay, independent master state, XTest motion and pre-move slave routing ([spec:59–84](/data/Projects/yserver-passive-grabs/docs/superpowers/specs/2026-11-21-passive-grab-activation-design.md:59)). Task 5 explicitly authorizes the DeviceButtonGrab scope expansion while preserving mask snapshots. Its tests need B-1’s paired length cases.

Formatting, all-target Clippy, request-driven packet tests, KMS backend tests, workspace/lavapipe tests and layout checks are assigned to implementation. No new ioctl or FFI layout is proposed. Actual test outcomes and platform buildability remain unverified.

**5. Changed-behavior provenance.** Read relevant state/storage definitions and comments; searched producers, including direct freeze writers, replay/deferred acceptance overrides, registry shapes, and selection mutations.

| Changed code | Provenance and assessment |
|---|---|
| Replay admission and freeze eligibility | `f6c45fd6`; design commit `a1907fd1`, [freeze-state plan:7](/data/Projects/yserver-passive-grabs/docs/superpowers/plans/2026-07-15-freeze-state-unification.md:7). Task 1 preserves unified-state authority and separation of stored replay material from pending input. `d4d1e586`’s device-restricted replay remains intact. |
| Slave child and master-pass admission | `30b2db33`, authoritative spec:59–83; original state split `77ce93c8`, [registry spec:144–153](/data/Projects/yserver-passive-grabs/docs/superpowers/specs/2026-09-29-dynamic-xinput-device-registry-design.md:144). Tasks 2/4 preserve independent source/master processing, including deferred master acceptance. |
| XTest validation and clamps | Clamp origin `8677f535`; last motion-path shaping `678e4136`, authoritative spec:74–77. Current registry shapes have at most four axes, within the six-value payload; validation against those shapes can replace the capacity clamps. The [registry addendum:138–158](/data/Projects/yserver-passive-grabs/docs/superpowers/specs/2026-10-04-xi-registry-maintainer-review-addendum.md:138) requires device-shape validation in Xorg’s order. **Original clamp rationale remains unverified:** the introducing commit’s `docs/test-status.md` contains results, not that rationale. |
| XI1 owner and canonical selections | `5533f5ab`, authoritative spec:66–72; storage shaping `6ce488c5`, [July 26 plan:191–197](/data/Projects/yserver-passive-grabs/docs/superpowers/plans/2026-07-26-protocol-silent-success-audit.md:191). Canonical storage, notification aggregation and mask snapshots are preserved as contracts. Entry creation needs B-1’s correction. |

**Reading budget:** 24/24 general excerpts and 12/12 provenance excerpts; plan and prior review read once. Investigation stopped at the limit. One producer locator exceeded the requested 40-line output cap.

**Unassessed question:** did the original XTest clamps serve any compatibility requirement beyond payload bounds? A narrowly scoped provenance follow-up should recover that reason or explicitly document its replacement by validated registry bounds. No builds, tests, benchmarks, installs, compilation experiments or delegated reviews were run.