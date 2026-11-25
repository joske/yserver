## Verdict

**1 blocking, 0 major, 0 minor**

**Coverage: INCOMPLETE**

The revised plan applies the four prior findings, but G2 introduces a coordinate-range contract defect. Historical provenance remains partly unresolved. This result establishes neither compilation nor test success nor implementation approval.

## Incorporation audit

| Prior finding | Disposition | Evidence in revised plan |
|---|---|---|
| R1 B-1: incorrect sibling-origin translation | **APPLIED** | Lines 189–196 subtract `(geom.x + bw, geom.y + bw)` before adding the paint-target offset difference. This matches the content-origin contract. |
| R1 M-1: tile-alignment fixture cannot distinguish restarted sampling | **APPLIED** | Lines 223–237 introduce a second island starting at local x=10, distinguish its expected colors, and name the restart-at-zero mutation. |
| R1 M-2: uniform pixels cannot establish move carry | **APPLIED** | Lines 154–181 use alternating columns, assert displaced colors after both moves, check source/destination exclusion, and name a move-only rectangular-occluder mutation. |
| R1 m-1: missing depth-32 fixture | **APPLIED** | Lines 220–226 restore depth-32 windows and pixmap, compatible visual setup, and opaque alpha. |

References: [revised plan](/data/Projects/yserver-shaped-clip/docs/superpowers/plans/2026-11-23-shaped-sibling-clip.md:139).

## Findings

### Blocking

**B-1 — G2’s helper reuse silently discards visible pieces outside signed-16-bit local coordinates**

[Plan lines 264–270](/data/Projects/yserver-shaped-clip/docs/superpowers/plans/2026-11-23-shaped-sibling-clip.md:264) route pixmap-background clears through `apply_subwindow_mode_clip`. Its output conversion silently drops any surviving rectangle whose origin cannot fit `i16`: [`clip.rs` lines 675–686](/data/Projects/yserver-shaped-clip/crates/yserver/src/kms/render/backend/portable/clip.rs:675). The existing pixmap branch instead retains `Rect2D` coordinates through tiling: [`windows.rs` lines 871–907](/data/Projects/yserver-shaped-clip/crates/yserver/src/kms/render/backend/portable/windows.rs:871).

A concrete sequence:

1. Create a black, redirected parent P, width 64.
2. Create its child C at x=−32768, width 40000, with a red tiled background.
3. Create C’s child D at local x=32760, width 9, with a blue background. Map D while C remains unviewable, then map C.
4. C’s background clear, minus D, leaves horizontal pieces `[0,32760)` and `[32769,40000)`.
5. The first piece lies outside P’s backing. The helper drops the second because 32769 exceeds `i16::MAX`. P pixels x=1…63 consequently miss C’s red background.

This does not require a giant backing allocation. Creation accepts the wide geometry; realization continues to background painting when C’s optional leaf allocation fails; paint-target resolution can still reach P’s small backing. See [creation lines 1932–1971](/data/Projects/yserver-shaped-clip/crates/yserver/src/kms/render/backend/portable/windows.rs:1932), [realization lines 2042–2059](/data/Projects/yserver-shaped-clip/crates/yserver/src/kms/render/backend/portable/windows.rs:2042), and [ancestor resolution lines 517–574](/data/Projects/yserver-shaped-clip/crates/yserver/src/kms/render/backend/portable/paint_target.rs:517).

The discarded piece is allowed background area, violating [spec G2, lines 95–106](/data/Projects/yserver-shaped-clip/docs/superpowers/specs/2026-11-22-shaped-sibling-clip-design.md:95). The proposed small fixtures cannot detect this.

**Smallest correction:** preserve 32-bit coordinates through internal clipping and tiling while sharing the existing cut/keep rule—for example, factor the rectangle algebra beneath the `Rectangle16` wrapper. Explain why the current wrapper cannot serve this caller, as required by the reuse constraint, and add a request-driven regression with a visible surviving piece starting beyond 32767.

### Major

None.

### Minor

None.

## Coverage and implementation checks

1. **Incorporation:** assessed all four prior findings against the actual revised tasks. All are applied; the prior historical-document limitation remains unresolved.

2. **Architecture and cross-task contracts:** inspected G1’s shape producer and coordinate translation, shared drawing clip, both move-carry masks, and G2’s clipping producer/consumer boundary. The flattened occluder union reaches the existing consumers. B-1 identifies a representation boundary that loses required information. Request-driven tests exercise the actual backend rather than an injected clip fake.

3. **Safety, ownership and failure semantics:** assessed empty versus absent shapes, cached clip results, old/new move masks, fully clipped returns, and failed-leaf-allocation continuation. No new asynchronous ownership handoff is proposed. GPU internals and unrelated cancellation, fencing, and lifecycle mechanisms were not audited.

4. **Specification and verification:** checked normative G1/G2 behavior, minimum T1–T6 coverage, T7 preservation, oracle provenance supplied by the user, and named mutations. The revised tile and move fixtures distinguish the prior faulty behaviors. B-1 needs additional boundary evidence. Formatting, regular clippy with all targets, workspace tests, lavapipe tests, layout checks, documentation generation, and load repetitions are assigned to implementation. No builds, tests, installs, benchmarks, or compilation experiments were run. Explicit non-goals and coordinator-owned hardware work were excluded.

5. **Provenance:** traced these changed or newly consumed contracts:

   | Code item | Historical source and assessment |
   |---|---|
   | Shared-backing sibling protection and ancestor traversal | `2300c647`, [status lines 2546–2555](/data/Projects/yserver-shaped-clip/docs/status.md:2546); later ancestor behavior associated with `cdb6d7cf`, [status lines 7287–7304](/data/Projects/yserver-shaped-clip/docs/status.md:7287). G1 retains same-backing protection and ancestor traversal while refining shaped coverage. |
   | Content-space paint-target offsets | `9a0de0aa`, [border design lines 444–451](/data/Projects/yserver-shaped-clip/docs/superpowers/specs/2026-09-07-window-borders-design.md:444). The corrected translation preserves the documented formula. Paint-target producers were searched. |
   | Absent versus explicitly empty bounding shapes | Definitions and production mutation path inspected; producers searched. `81c9dc28` introduced the [shape-plan requirement at line 1375](/data/Projects/yserver-shaped-clip/docs/superpowers/plans/2026-05-05-phase67-full-x11-pass.md:1375); `c8f7aa88` documents the later sentinel regression. The plan preserves this distinction. |
   | Clip result and cache validity | `b4a405f2`; [later cache design lines 233–241](/data/Projects/yserver-shaped-clip/docs/superpowers/specs/2026-10-28-cached-clip-lists-design.md:233). Clip/cache producers were searched. The plan retains geometry, topology, and shape generations. |
   | Pixmap-clear sibling protection | `cd8e57ca` and current source explain protecting decoration buttons from later titlebar clears. The replacement retains sibling subtraction. Its referenced investigation document, `docs/handoff-xfce-buttons.md`, was unavailable, including in searched history. Additional historical guarantees remain unverified. |

**Reading budget:** **24/24** source/spec excerpts, plus **12/12** provenance excerpts; locator searches were bounded separately. Investigation stopped at that limit.

**Unassessed provenance:** the original rationale for the helper’s narrowing conversion, all producers behind the newly consumed `scene_participating` child gate, and the missing pixmap-clear investigation document. A follow-up, if authorized, should address those specific contracts. They are unverified, not sound. Compiler-level API, borrow, fixture, and portability checks remain for implementation.