# Phase 2.9 proposal: split `kms/render/scene.rs` (production code)

Step 2.9 of `2026-10-08-source-layout-cleanup.md`. Manifest:
`tools/split/manifests/scene_2.toml` (+ `.paths`). Status: **proposal, dry
run only**; nothing moved yet. Sizes below are the dry-run tree after
`cargo +nightly fmt`.

## Structure today (9,646 lines, tests already in `scene/tests/`)

- **Types (lines 1–1334, plus ~350 later):** stage-2d `//!` history doc,
  imports, `InFlightStage`, `PendingAck`, cursor enums
  (`CursorAssignment`/`Transition`/`OutputCursorMode`/`CursorPlaneMode`),
  `BufferAgeRing`, `OutputSceneState` (126-line struct), damage-audit types,
  `TickSkipReason`/`TickOutcome`, `SceneCompositor` + `SceneCompositorInner`,
  walk types (`WalkStats`, `WalkSink`, `SceneBuild`, `ContentDamage`), later
  `RepaintPlan`, `RootNode`, `NodeDecision`, `IntermediatePrimeTarget`,
  `ScalePass`, the `ComposeRenderTarget` trait. Private fields read across
  every responsibility.
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
- **Macros:** none defined, no `module_path!`/`line!`/`file!`.
- **Logs:** 72 log macros, none with `target:`; all children are
  descendants, so `kms::render::scene` filters still match.
- **Relative paths:** 60 `super::` paths outside the root `use super::{…}`
  (36 `store`, 16 `backend`, 5 `root_overlay`, 2 `transform_intermediate`,
  1 `engine`; one is a doc link).

## Target tree (dry run, lines after fmt)

All under `kms/render/scene/`, depth 1 (`pub(super)` = old private reach).
Names avoid every first path segment used in the file (`vk`, `store`,
`core`, `render`, `compositor`, `pipeline`, `scanout`, `platform`, …).

```
scene.rs         1195  //! doc, imports, mod decls, re-exports, every type,
                       ComposeRenderTarget trait, SceneError From impls,
                       mod tests
lifecycle.rs      430  new/stub/is_live, build_output_state, rebuild_outputs,
                       sync_output_layouts, invalidate_all_scanout_damage,
                       drain_all, drain_deferred_scene_resources, BufferAgeRing
damage.rs         340  note_structure_change, wake_for_damage,
                       mark_scene_structure_*, root_overlay_*, projection
                       and fan-out onto outputs (fan_out_*, dispatch/clip/
                       project/add_projected_damage)
cursor.rs         687  cursor mode classification, transitions, retire
                       resolution, retry/lifecycle resets, hw_cursor_allowed,
                       cursor damage/footprint, record_cursor_save,
                       register/clear/restore, steady-state upload
tick.rs          1412  tick, retry deadline, owes_repaint, TickOutcome,
                       tick-skip/success records, walk_needed, dormancy,
                       pending presentation, tick_one_output (952)
flip.rs           436  page-flip and render-completion retirement,
                       InFlightStage matching, failed-submit BO retire,
                       pool release drain, has_pending_page_flip(s)
damage_audit.rs  1197  DamageAuditTarget (+Drop, ComposeRenderTarget), audit
                       state/env knobs, heartbeat, ledger, reference compose,
                       compare submit, summary + tile attribution
repaint.rs        241  partial-compose planning: RepaintPlan, plan_repaint,
                       opaque cover, cull_scene_to_region, scissor consts
build.rs          985  scene list assembly: build_scene(_with),
                       scene_draw_rects, scene_participant_places, root_node,
                       piece_draw, emit_node, WalkSink, walk debug filters
walk.rs          1104  per-window decisions: visit_window_subtree,
                       decide_node, inner_place_rects, children_index,
                       WalkStats
compose.rs        788  ComposeRenderTarget for ScanoutBo/CopiedRenderSource,
                       record_and_submit_render, record_command_buffer,
                       shared/copied scanout submit, stage_submitted_frame,
                       device-lost classification, CopiedRenderSubmitError
transform.rs      441  transform intermediates: ensure/release/prime,
                       IntermediatePrimeTarget, ScalePass, record_scale_pass
root_readback.rs  210  root_readback(+_is_current), extent/covers
for_tests.rs      251  the 6 test-only SceneCompositor methods,
                       read_general_image_for_tests
```

Total 9,717 (9,646 before). All ≤ 5k; largest `tick.rs`. Types stay in
the root (as engine): `SceneCompositorInner`/`OutputSceneState` fields are
read from nearly every child. Inherent impls are spread per member.

Deviations from the 2.9 row (`cursor`, `damage_audit`, `tick_output`,
`walk` 2.6k, `fan_out`, `targets`, `root_readback`): `tick` holds the whole
tick, not just the per-output fn; `fan_out` merged into `damage` with the
marking it was extracted from; `targets` dissolved: each
`ComposeRenderTarget` impl sits with its owner (scanout ones in `compose`,
the audit one in `damage_audit`, the prime one in `transform`); `walk` split
into `build` + `walk` (matches `tests/build_scene.rs` / `tests/walk.rs`).

## Dry-run results

Prep (temporary commit, dropped): the 60 `super::` paths →
`crate::kms::render::…` outside the root import (sed + fmt: +66/−60).
Then `split apply`, fmt, `cargo build -p yserver` and `cargo clippy -p
yserver --all-targets -- -D warnings` (default features) green;
`split verify --manifest` → **OK**: 447/447 leaves identical, 89 manifest
visibility changes, 0 audited exceptions, 3 location audits, 27 leaves with
log calls moved to descendants. Scene unit tests: 162 passed, 1 ignored.
Not run in the dry run: `verify --tests`, feature variants, lavapipe.
Logs: `target/gate-scene-dry-*.log`.

**Visibility delta (89, all private → `pub(super)` = the old reach):**
66 needed by the lib (53 free fns, 13 methods: `note_structure_change`,
`record_damage_audit_event`, `damage_audit_active`,
`full_output_audit_area`, `DamageAuditTarget::new`, `ScalePass::new`,
`RepaintPlan::full`, `WalkStats::{collapses,
off_output_damage_forces_compose}`, `BufferAgeRing::push`,
`CopiedRenderSubmitError::{into_present, requires_fail_stop}`,
`SceneBuild::omit_software_cursor_for_hide`); 23 only by `scene/tests`
(18 fns, e.g. `walk_needed`, `decide_node`, `fan_out_to_output`,
`classify_cursor_mode_from_per_output`; `CLIPPED_REPAINT_MAX_FRACTION`;
`BufferAgeRing::{new, contains_all}`,
`TickOutcome::{clears_scene_structure_dirty, walked}`). Existing
`pub(crate)` items unchanged.

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

## Open questions

1. `build` + `walk` as two files, or one `walk` (2.1k) as the plan row says?
   Recommend two: separate responsibilities, mirrors the test topics.
2. Accept the renamed/regrouped modules above and update the 2.9 row?
   Recommend yes.
3. All types stay in `scene.rs` (engine decision 3)? Recommend yes; moving
   e.g. `NodeDecision` to `walk` needs field visibility for little gain.
4. 23 test-only visibility changes (same reach)? Recommend accept, as engine.
5. Root `//!` doc: the Stage-2d MVP text is stale (full redraw, no HW
   cursor). Recommend replacing it with an ownership line + module map in
   its own commit, as engine.
6. `tick_one_output` (952) stays one fn this step (codex: accept); its
   split belongs with phase 3's output-seam decision (option a/b).

## Commit sequence (on acceptance)

1. `refactor(kms): qualify scene super:: paths (prep)`.
2. `refactor(kms): split scene into lifecycle/tick/compose/walk modules
   (move)`: apply + fmt, `verify --manifest` and `--tests`, full gate.
3. `docs(kms): scene root doc is an ownership line and module map`.
4. `chore: blame-ignore` for 1 and 2.
5. This doc and the plan updated to the final tree.
