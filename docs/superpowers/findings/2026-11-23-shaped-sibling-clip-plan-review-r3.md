## Verdict

**0 blocking, 0 major, 0 minor**

**Coverage: INCOMPLETE**

No additional design defect was demonstrated. The plan addresses R2’s coordinate-range finding. Missing historical documentation leaves a provenance question unresolved; this is not a clean review or implementation approval.

## Incorporation audit

References below use [the target plan](/data/Projects/yserver-shaped-clip/docs/superpowers/plans/2026-11-23-shaped-sibling-clip.md).

| Prior finding | Disposition | Assessment |
|---|---|---|
| R2 B-1: signed-16-bit clipping discards visible pieces | **APPLIED** | Lines 289–302 explicitly retain `Rect2D` through subtraction, intersection and tiling, bypassing the narrowing adapter. T8, lines 266–284, adds the requested visible piece beyond `i16::MAX` and names the adapter mutation. |
| R1 B-1, carried through R2: incorrect sibling-origin translation | **APPLIED** | Lines 189–196 subtract the sibling’s parent-space content origin before adding the resolved target-offset difference. |
| R1 M-1, carried through R2: tile fixture cannot distinguish restarted sampling | **APPLIED** | Lines 227–241 use a second island at local x=10, different expected colors, and a restart-at-zero mutation. |
| R1 M-2, carried through R2: uniform pixels cannot establish move carry | **APPLIED** | Lines 154–181 use alternating columns, displaced-color assertions, source/destination protection, and a move-only mutation. |
| R1 m-1, carried through R2: missing depth-32 fixture | **APPLIED** | Lines 224–230 specify depth-32 windows and pixmap, compatible visual setup, and opaque alpha. |

R2’s provenance limitation was not a formal finding. Its missing investigation document remains unresolved below.

## Findings

### Blocking

None demonstrated.

### Major

None demonstrated.

### Minor

None demonstrated.

## Coverage and implementation checks

1. **Incorporation audit.** Assessed R2’s finding and its four carried R1 corrections against the actual task text. None is merely deferred to compilation. The R2 correction preserves the existing pixmap branch’s coordinate range: the narrowing occurs specifically in [the adapter, lines 675–686](/data/Projects/yserver-shaped-clip/crates/yserver/src/kms/render/backend/portable/clip.rs:675).

2. **Architecture and cross-task contracts.** Checked G1’s shape production, content-origin translation, same-backing restriction, ancestor traversal, border-ring consumer, and both move-carry masks. Existing consumers subtract a rectangle union; the proposed flattened output preserves that contract. See [occluder ownership and traversal, lines 947–999](/data/Projects/yserver-shaped-clip/crates/yserver/src/kms/render/backend/portable/paint_target.rs:947), [move consumption, lines 892–909](/data/Projects/yserver-shaped-clip/crates/yserver/src/kms/render/backend/portable/paint_target.rs:892), and [border-ring consumption, lines 1232–1249](/data/Projects/yserver-shaped-clip/crates/yserver/src/kms/render/backend/portable/windows.rs:1232). G2 consumes the existing clip producer without introducing another owner or cache.

3. **Safety, ownership and failure semantics.** Checked absent versus explicitly empty shapes, cached clip invalidation, old/new move-mask ordering, fully clipped returns, and background realization after optional leaf allocation failure. Scene-participation changes increment the topology generation used by the clip cache: [store lines 1371–1384](/data/Projects/yserver-shaped-clip/crates/yserver-render/src/kms/render/store.rs:1371), [clip lines 519–535](/data/Projects/yserver-shaped-clip/crates/yserver/src/kms/render/backend/portable/clip.rs:519). No new asynchronous handoff is proposed. GPU submission, fencing and unrelated lifecycle mechanisms were not audited.

4. **Specification and verification strategy.** Checked the normative shaped outer-region rule, all listed G1 consumers, G2 cut/keep semantics, tile alignment and fully clipped behavior against [spec lines 86–106](/data/Projects/yserver-shaped-clip/docs/superpowers/specs/2026-11-22-shaped-sibling-clip-design.md:86). T1–T6 cover the [specified minimum scenarios, lines 166–202](/data/Projects/yserver-shaped-clip/docs/superpowers/specs/2026-11-22-shaped-sibling-clip-design.md:166); T7 preserves the unclipped leaf case, and T8 guards the reviewed range defect. The supplied Xvfb T4 measurement was accepted as oracle evidence, not independently reproduced. Request-driven pixel checks and mutations can distinguish the intended behaviors without injecting clip state.

   Formatting, all-target clippy, workspace/lavapipe tests, layout checks, documentation generation and load repetitions are assigned to implementation in plan lines 61–73. No builds, tests, installs, benchmarks or compilation experiments were run. Fixture execution, mutation results and supported-platform buildability remain unverified. Explicit non-goals and coordinator-owned hardware work were excluded.

5. **Provenance of changed behavior.** Definitions and producer searches covered shape storage, paint-target construction, clip/cache construction, mapped-state transitions, scene-participation allocation and setter paths, and redirect/disconnect restoration.

| Changed or newly consumed contract | Historical source and assessment |
|---|---|
| Same-backing sibling protection and ancestor-level occlusion | `2300c647`, [status lines 2546–2555](/data/Projects/yserver-shaped-clip/docs/status.md:2546); `cdb6d7cf`, [status lines 7287–7304](/data/Projects/yserver-shaped-clip/docs/status.md:7287). G1 preserves backing identity and ancestor traversal while refining shaped coverage. |
| Content-origin offsets and privileged border painting | `9a0de0aa`, [border design lines 444–459](/data/Projects/yserver-shaped-clip/docs/superpowers/specs/2026-09-07-window-borders-design.md:444). The corrected translation preserves the content-offset formula. Ring protection is also recorded by `ae64dbc6`, [status lines 244–247](/data/Projects/yserver-shaped-clip/docs/status.md:244). |
| Empty versus absent bounding shape | `81c9dc28`, [Phase 6.7 plan line 1375](/data/Projects/yserver-shaped-clip/docs/superpowers/plans/2026-05-05-phase67-full-x11-pass.md:1375), with the later sentinel regression described by `c8f7aa88`. The plan preserves the distinction. |
| Clip generation validity | `b4a405f2`; later documented by `4f7fd8d4`, [cache design lines 233–241](/data/Projects/yserver-shaped-clip/docs/superpowers/specs/2026-10-28-cached-clip-lists-design.md:233). G2 retains geometry, topology and shape generation ownership. |
| Scene participation and child exclusion | `f1b4940b`, [RENDER plan lines 115–136](/data/Projects/yserver-shaped-clip/docs/superpowers/plans/2026-07-02-render-clipbychildren-damage.md:115), explicitly preserves drawing beneath manually redirected children. The [rendering model, lines 229–238](/data/Projects/yserver-shaped-clip/docs/superpowers/specs/2026-05-15-rendering-model-v2.md:229) distinguishes map/unmap and redirect transitions. G2 retains this existing gate rather than reinterpreting the flag. |
| Viewability-driven storage | `69766606`, [lifecycle design lines 48–79](/data/Projects/yserver-shaped-clip/docs/superpowers/specs/2026-09-23-window-storage-lifecycle-design.md:48). The plan leaves allocation/release authority with core-driven viewability transitions. |
| Pixmap-clear sibling protection | `cd8e57ca` and current source identify decoration buttons overwritten by later titlebar clears. G2 retains that protection through `shared_backing_draw_clip`. The referenced `docs/handoff-xfce-buttons.md` is absent from the checkout and available history; additional requirements in that document remain **unverified**. |

**Reading budget:** **24/24** bounded spec/source excerpts and **12/12** provenance excerpts, with bounded locator/history searches separately. Investigation stopped at the limit.

The specific unresolved provenance question is whether `docs/handoff-xfce-buttons.md` imposed any pixmap-clear requirement beyond the higher-sibling protection stated in `cd8e57ca`. That missing ground is not established as sound.