# Phase 2.9: split `kms/render/scene.rs` (production code)

Step 2.9 of `2026-10-08-source-layout-cleanup.md`. Manifest:
`tools/split/manifests/scene_2.toml` (+ `.paths`). Accepted by jos
2026-10-10 and executed (decisions at the end); sizes below are the final
tree (after `cargo +nightly fmt`).

## Structure before (9,646 lines, tests already in `scene/tests/`)

- **Types (lines 1–1334, ~350 later):** stage-2d `//!` doc, imports,
  `PendingAck`, cursor enums, `BufferAgeRing`, `OutputSceneState`,
  damage-audit types, `TickOutcome`, `SceneCompositor(Inner)`, walk types,
  `RepaintPlan`, `NodeDecision`, `ComposeRenderTarget`. Private fields are
  read across every responsibility.
- **`impl SceneCompositor`** in three blocks (1,147 / 307 / 174 lines):
  lifecycle, damage marking, cursor, tick, flip completion, readback,
  transform priming, `*_for_tests`.
- **Free fns (~150):** `tick_one_output` 952 (indivisible this step),
  `visit_window_subtree` 605, `build_scene_with` 397, `decide_node` 371,
  `record_command_buffer` 327, `run_damage_audit` 231, `emit_node` 221.
- **Location-sensitive:** four `#[track_caller]` damage helpers
  (`wake_for_damage`, `mark_scene_structure_dirty`,
  `mark_scene_structure_damage_rect(s)`) record `Location::caller()`; their
  only callers in this file are the three `root_overlay_*` methods.
- **Macros/logs:** none defined, no `module_path!`/`line!`/`file!`; 72 log
  calls, none with `target:`; children are descendants, filters still match.
- **Relative paths:** 60 `super::` paths outside the root `use super::{…}`
  (36 `store`, 16 `backend`, 5 `root_overlay`, 2 `transform_intermediate`,
  1 `engine`; one is a doc link).

## Final tree (lines after fmt)

All under `kms/render/scene/`, depth 1 (`pub(super)` = old private reach).
Names avoid every first path segment used in the file (`vk`, `store`,
`core`, `render`, `compositor`, `pipeline`, `scanout`, `platform`, …).

```
scene.rs         1195  //! doc, imports, mod decls, re-exports, every type,
                       ComposeRenderTarget trait, SceneError From impls,
                       mod tests
lifecycle.rs      430  new/stub, output state rebuild/sync, drain_all,
                       deferred scene resources, BufferAgeRing
damage.rs         340  note_structure_change, wake_for_damage,
                       mark_scene_structure_*, root_overlay_*, projection
                       and fan-out onto outputs (fan_out_*, dispatch/clip/
                       project/add_projected_damage)
cursor.rs         687  cursor mode, transitions, retire resolution, retry
                       resets, cursor damage/footprint, record_cursor_save
tick.rs          1412  tick, retry deadline, owes_repaint, TickOutcome,
                       tick-skip/success records, walk_needed, dormancy,
                       pending presentation, tick_one_output (952)
flip.rs           436  page-flip and render-completion retirement,
                       InFlightStage matching, failed-submit BO retire,
                       pool release drain, has_pending_page_flip(s)
damage_audit.rs  1197  DamageAuditTarget (+Drop, ComposeRenderTarget),
                       knobs, ledger, reference compose, compare, attribution
repaint.rs        241  partial-compose planning: RepaintPlan, plan_repaint,
                       opaque cover, cull_scene_to_region, scissor consts
build.rs          985  scene list assembly: build_scene(_with),
                       scene_draw_rects, scene_participant_places, root_node,
                       piece_draw, emit_node, WalkSink, walk debug filters
walk.rs          1104  per-window decisions: visit_window_subtree,
                       decide_node, inner_place_rects, children_index,
                       WalkStats
compose.rs        788  ComposeRenderTarget for ScanoutBo/CopiedRenderSource,
                       record_(and_submit_render|command_buffer), scanout
                       submits, device-lost classification
transform.rs      441  transform intermediates: ensure/release/prime,
                       IntermediatePrimeTarget, ScalePass, record_scale_pass
root_readback.rs  210  root_readback(+_is_current), extent/covers
for_tests.rs      251  the 6 test-only SceneCompositor methods,
                       read_general_image_for_tests
```

Total 9,717 (9,646 before). All ≤ 5k. Types stay in the root (as engine);
inherent impls are spread per member.

Root `//!` doc: the stale Stage-2d MVP text became a short ownership
description plus this module map (its own commit, right after the move).

Deviations from the 2.9 row (`cursor`, `damage_audit`, `tick_output`,
`walk` 2.6k, `fan_out`, `targets`, `root_readback`): `tick` holds the whole
tick, not just the per-output fn; `fan_out` merged into `damage` with the
marking it was extracted from; `targets` dissolved: each
`ComposeRenderTarget` impl sits with its owner (scanout ones in `compose`,
the audit one in `damage_audit`, the prime one in `transform`); `walk` split
into `build` + `walk` (matches `tests/build_scene.rs` / `tests/walk.rs`).

## Results

Prep commit: the 60 `super::` paths → `crate::kms::render::…` outside the
root import (sed + fmt: +66/−60; scene tests 162 passed, 1 ignored). Then
`split apply`, fmt, `split verify --manifest` → **OK**: 447/447 leaves
identical, 89 manifest visibility changes, 0 audited exceptions, 3 audited
locations, 27 leaves with log calls moved to descendants. `verify --tests`
against fresh pre-move snapshots: 4008 / 4013 / 4121 tests mapped (default
/ tcp-transport / xdmcp). Rule-5 gate green: fmt, clippy ×3, tests ×3
(3693 / 3698 / 3806 passed), lavapipe 317 passed. Logs:
`target/gate-scene-*.log`.

**Visibility delta (89, all private → `pub(super)` = the old reach):**
66 needed by the lib (53 free fns, 13 methods, e.g. `note_structure_change`,
`record_damage_audit_event`, `RepaintPlan::full`); 23 only by `scene/tests`
(18 fns, e.g. `walk_needed`, `decide_node`; 1 const; 4 methods of
`BufferAgeRing`/`TickOutcome`). Existing `pub(crate)` items unchanged.

**Re-exports (root `lines`):** `use` globs for damage, cursor, tick, flip,
damage_audit, repaint, walk, compose, transform; `pub(crate) use build::*`
(backend's `for_tests` calls `scene::scene_participant_places` /
`scene_draw_rects`); `#[cfg(test)] use
lifecycle::drain_deferred_scene_resources;` (test-only free fn of a
method-only child). No glob for lifecycle, root_readback, for_tests.

**Refusals and handling:**
1. Relative paths: prep commit, 60 sites.
2. Locations (3): `root_overlay_toggle`, `root_overlay_clear`,
   `root_overlay_on_disconnect` call the `#[track_caller]` helpers; the
   recorded site moves from `scene.rs:…` to `scene/damage.rs:…`. Audited
   reason: diagnostic only, read as log text in
   `bound_damage_audit_ledger` (overflow warn) and
   `process_damage_audit_summary` (audit attribution), never in damage
   computation (same reasoning codex verified for phase 1a).
3. Unused glob (compiler): lifecycle and root_readback globs warn (their
   free fns are file-local in the lib) → dropped; one cfg(test) import.
4. Log targets, macros, includes, delegations, exceptions: none.

## Decisions (jos, 2026-10-10)

1. `build` + `walk` as two files (separate responsibilities, mirrors the
   test topics), not one 2.1k `walk`.
2. The renamed/regrouped modules above are accepted; the 2.9 row is updated.
3. All types stay in `scene.rs` (as engine decision 3).
4. The 23 test-only visibility changes (same reach) are accepted.
5. The stale Stage-2d root `//!` doc is replaced by an ownership line and
   module map, in its own commit.
6. `tick_one_output` (952) stays one fn this step; its split belongs with
   phase 3's output-seam decision (option a/b).
7. The 3 `root_overlay_*` `[locations]` entries are audited: diagnostic-only
   damage-audit attribution, the recorded site moves scene.rs →
   scene/damage.rs.

## Commit sequence

1. `refactor(kms): qualify scene super:: paths (prep)`: 60 sites + fmt.
2. `refactor(kms): split scene into lifecycle/tick/compose/walk modules
   (move)`: `split apply` + fmt, `split verify --manifest …` and `--tests`,
   then the full gate (rule 5).
3. `docs(kms): scene root doc is an ownership line and module map`.
4. `chore: blame-ignore` for 1 and 2.
5. This doc and the plan updated to the final tree.
