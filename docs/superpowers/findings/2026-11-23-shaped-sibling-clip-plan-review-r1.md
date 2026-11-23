## Verdict

**1 blocking, 2 major, 1 minor**

**Coverage: INCOMPLETE**

The coordinate contract contains a demonstrated defect. Historical documentation for the replaced pixmap-clear behavior was unavailable, limiting provenance coverage. This review does not establish compilation, test success, or implementation approval.

## Incorporation audit

| Prior findings | Disposition |
|---|---|
| None | First review; check 1 skipped as instructed. |

## Findings

### Blocking

**B-1 — G1 translates the shape using the wrong sibling origin**

[Plan lines 172–179](/data/Projects/yserver-shaped-clip/docs/superpowers/plans/2026-11-23-shaped-sibling-clip.md:172) instruct the builder to subtract the sibling’s `(x, y)` from the parent-space region, then add the sibling-to-destination paint-target offset.

These origins differ by the sibling’s border width. [`child_clip_region` lines 1039–1056](/data/Projects/yserver-shaped-clip/crates/yserver/src/kms/render/backend/portable/paint_target.rs:1039) place the shape at `(x + bw, y + bw)`. [`PaintTarget` lines 134–155](/data/Projects/yserver-shaped-clip/crates/yserver-render/src/kms/render/target.rs:134) define its offset as the **content** origin.

Consequently, T6’s sibling with `bw=2` contributes an occluder shifted two pixels down and right. When the lower window’s border becomes yellow, P `(1,1)` is no longer protected and can overwrite U’s blue border. This violates [spec G1, lines 86–93](/data/Projects/yserver-shaped-clip/docs/superpowers/specs/2026-11-22-shaped-sibling-clip-design.md:86).

This also contradicts the established requirement from commit `9a0de0aa`: the [border design, lines 444–451](/data/Projects/yserver-shaped-clip/docs/superpowers/specs/2026-09-07-window-borders-design.md:444) specifies the translation as “`W.border_width + C.x + C.border_width`”. The plan does not acknowledge an override.

**Correction:** subtract `(geom.x + bw, geom.y + bw)` before adding the paint-target offset difference. Keep T6’s border assertion.

### Major

**M-1 — T3 cannot establish the claimed tile alignment after clipping**

[Plan lines 90–93](/data/Projects/yserver-shaped-clip/docs/superpowers/plans/2026-11-23-shaped-sibling-clip.md:90) claim T3 detects restarting sampling at each clipped piece. However, [T3’s two shapes, lines 203–215](/data/Projects/yserver-shaped-clip/docs/superpowers/plans/2026-11-23-shaped-sibling-clip.md:203), each leave one rectangle starting at local `(0,0)`: first `8×16`, then `24×16`.

Restarting the tile at each surviving rectangle therefore produces the same sampling coordinates in this fixture. The later assertions establish repetition across the tile boundary, but do not establish alignment for pieces whose origins moved because of clipping. A fragmented shape could consequently restart its second island with the wrong color, violating [spec lines 105–106](/data/Projects/yserver-shaped-clip/docs/superpowers/specs/2026-11-22-shaped-sibling-clip-design.md:105), without this evidence detecting it.

**Correction:** include a surviving island with a nonzero local origin and assert its expected tile phase. Add a mutation that resets sampling at each piece.

**M-2 — T6 does not distinguish correct move carry from rectangular exclusion of the hole**

[T6, plan lines 154–168](/data/Projects/yserver-shaped-clip/docs/superpowers/plans/2026-11-23-shaped-sibling-clip.md:154), fills the lower content uniformly, moves it one pixel, moves it back, and checks unchanged colors.

The hole already contains red before either move. If only the move consumers retain full-rectangle sibling exclusion, they can omit carrying pixels through that hole while its existing red pixels remain. The checked locations provide no displaced pattern identifying whether the required pixels were carried.

The existing contract explicitly requires excluding higher siblings at **both** the source and destination positions: [`paint_target.rs` lines 854–860 and 900–908](/data/Projects/yserver-shaped-clip/crates/yserver/src/kms/render/backend/portable/paint_target.rs:854). [Spec lines 194–200](/data/Projects/yserver-shaped-clip/docs/superpowers/specs/2026-11-22-shaped-sibling-clip-design.md:194) require move-carry coverage for the shaped outer region.

**Correction:** seed distinguishable lower-window pixels and check their translated positions through the hole after each move. Name a move-only mutation that substitutes rectangular occluders; exercise source and destination exclusion.

### Minor

**m-1 — T3 drops the required depth-32 fixture**

[Spec lines 178–184](/data/Projects/yserver-shaped-clip/docs/superpowers/specs/2026-11-22-shaped-sibling-clip-design.md:178) require a depth-32 background pixmap. [Plan lines 203–207](/data/Projects/yserver-shaped-clip/docs/superpowers/plans/2026-11-23-shaped-sibling-clip.md:203) substitute depth 24, and the other proposed pixmap fixtures also use depth 24.

The specified depth-32 request/render path therefore receives no planned regression evidence.

**Correction:** restore depth 32 for T3’s window and background pixmap, with compatible parent/visual setup.

## Coverage and implementation checks

1. **Incorporation:** skipped; no prior review.

2. **Architecture and contracts:** inspected G1’s producer, shape-coordinate helper, destination clipping, border-ring consumer, and old/new move-mask delivery. Inspected G2’s existing pixmap branch and shared clipping producer. B-1 identifies the coordinate-contract defect. No new cache or asynchronous delivery mechanism is proposed.

3. **Safety, ownership and failure semantics:** checked the affected coordinate boundaries, empty-versus-absent shape semantics, cached clip results, and fully clipped early return. The proposal introduces no resource-lifetime handoff. GPU engine internals and unrelated lifecycle paths were not audited.

4. **Specification and verification:** checked G1/G2 requirements, minimum tests, preservation behavior, mutations, and assigned gates. M-1, M-2 and m-1 identify evidence gaps. Formatting, clippy, workspace tests, lavapipe tests, layout checks and load repetitions are assigned to implementation. No builds, tests, installs, benchmarks or compilation experiments were run. Excluded topics and coordinator-owned hardware work were not assessed.

5. **Provenance:**
   - **Sibling occlusion:** traced to `2300c647`; [status lines 2546–2555](/data/Projects/yserver-shaped-clip/docs/status.md:2546) require protecting mapped higher siblings sharing the backing. G1 preserves that purpose while refining shaped coverage, subject to B-1.
   - **Content offsets:** traced to `9a0de0aa` and the border design cited in B-1. The plan violates that existing requirement.
   - **Pixmap-clear sibling protection:** traced to `cd8e57ca`. Its commit rationale and current source require preventing titlebar clears from overwriting higher decoration buttons. The shared helper retains sibling subtraction. The referenced `docs/handoff-xfce-buttons.md` was unavailable; additional historical guarantees remain unassessed.
   - **Shape sentinels and clip cache:** inspected definitions and searched production producers for shape-map updates, shape generation, clip results and KMS paint targets. Empty shapes remain distinct from absent shapes. Cache history reaches `b4a405f2`; the [later cache design, lines 233–241](/data/Projects/yserver-shaped-clip/docs/superpowers/specs/2026-10-28-cached-clip-lists-design.md:233) documents the three generation inputs, which the plan preserves. An original shape-sentinel spec/plan was not located.

**Reading:** 22/24 source/spec excerpts; 11/12 additional provenance excerpts, counting commit-message reads. Locator searches were bounded separately.

**Specific unresolved provenance:** whether the unavailable pixmap-clear handoff or original shape-sentinel design imposes further contracts beyond the recovered source comments and commit rationale. Those historical requirements are unverified, not sound.