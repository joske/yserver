# Phase 2.10 proposal: split `kms/render/engine.rs` (production code)

Step 2.10 of `2026-10-08-source-layout-cleanup.md`. Manifest:
`tools/split/manifests/engine_2.toml` (+ `.paths`). Accepted by jos
2026-10-09 and executed (decisions at the end); sizes below are the final
tree (after `cargo +nightly fmt`).

## Structure today (13,034 lines, tests already in `engine/tests/`)

- **Types (lines 1–1812):** `RenderError`, batch/flush keys and records
  (`RenderBatchKey`, `PendingRenderBatch`, `RenderFlushReason`), coalescing
  (`CoalesceClass`/`Counts`, `DstPassSession`, `SessionStep`),
  `SubmittedOp`, scratch (`ScratchImage`, `SampledScratchImage`,
  `ClipSnapshot`, `MaskedCopyMask`), `StagingBuffer` + `StagingPool`,
  sampler/swizzle enums, `RenderEngine` (6 lines) wrapping
  `RenderEngineInner` (203-line struct, private fields used everywhere).
  Later: `SourceDrawable`, `ResolvedSource`, glyph/text inputs, the three
  `CompositeTarget`/`TextRunTarget` adapters.
- **`impl RenderEngineInner`** (392 lines, 6 methods): upload into the open
  frame, text pipeline, retired-resource adoption, descriptor sets, layouts.
- **`impl RenderEngine`** (7,154 lines, 120 methods). Biggest:
  `render_composite_via_frame_builder` 708, `composite_glyphs_via_frame_builder`
  664, `close_open_frame` 511, `render_traps_or_tris` 496, `image_text` 457,
  `try_append_render_batch` 421. Indivisible for this step (codex: accept).
- **Free fns (~95):** close-time replay `emit_*_into_cb` (~2.1k, the traps
  one 440), commit/rollback, scratch allocation, clamps, pixel pack/unpack and
  wire decode (`decode_x11_pixel_for_storage` and co., used by backend).
- **Macros:** none defined; no `module_path!`/`line!`/`file!`/
  `#[track_caller]`, no calls of scene's damage-audit helpers.
- **Logs:** 34 `warn!` (9 with `target:`), 16 `error!`, 11 `debug!`,
  6 `info!`, 0 `trace!`. All children are descendants: filters still match.
- **Relative paths:** 155 `super::` paths outside the root `use super::{…}`
  (129 `frame_builder`, 7 `submit_group`, 3 each `upload_arena`/`glyph_atlas`/
  `glyph_pixels`/`platform`, 2 `descriptor_pool_ring`/`present_completion`,
  1 `store`/`telemetry`; 2 of them in doc links).

## Final tree (lines after fmt)

All under `kms/render/engine/`, depth 1 (so `pub(super)` = old private
reach). Names avoid every first path segment used in the file (`vk`,
`vk_render`, `render_pipeline`, `ops`, `trap_pipeline`, `frame_builder`,
`store`, `platform`, `telemetry`, …) and `tests`.

```
engine.rs     1104  //! doc, imports, mod decls, root re-exports, every type,
                    From/Debug/Send/Sync impls, mod tests
lifecycle.rs   666  new/stub/is_live, poll_retired, retire/destroy image,
                    shutdown, drain_all, Drop for RenderEngine, SubmittedOp,
                    adopt_retired_resource, create_pixmap, notify_retired,
                    active_resource_bytes
export.rs      225  promote_drawable_exportable, copy_image_blocking (GLX-TFP
                    promotion onto dma-buf-exportable storage)
frame.rs      1498  frame builder open/close/submit: trace filter + trace_ops,
                    flush_submit_group, close_open_frame(+_if_timed_out,
                    _for_non_ported_op), frame/descriptor-pool counters,
                    present completion attach/drain, descriptor sets,
                    coalescing stats, begin_op_cb/end_and_submit_op*,
                    commit_close_success, rollback_*
staging.rs     395  upload arena consts/align, StagingBuffer, StagingPool,
                    upload_to_frame
scratch.rs     502  ScratchImage/SampledScratchImage/ClipSnapshot drops, clip
                    snapshot create/query/retire/refresh, scratch allocators
fill_copy.rs  1054  fill_rect(_batch), logic fill, stamp alpha, copy_area,
                    masked_copy_area, cow_copy_area, clamp_rect*/clamp_copy_*
put_get.rs     535  put_image, get_image, GetImage phase telemetry, clamp_put_*
pixels.rs      334  row stride, unpack_to_staging, pack_from_storage, decode_*,
                    premul_from_wire_pixel
text.rs        572  image_text, ensure_text_pipeline, StorageTextTarget impls
glyphs.rs     1159  atlas fit/reset/forget, composite_glyphs(+via frame
                    builder), glyph layout/runs, uniform_pixel_glyph_source
composite.rs  1534  render_composite(+via frame builder), fill_rectangles,
                    render assets, drawable views/sampler/swizzle,
                    SourceDrawable, force-opaque, composite attrs
batch.rs       570  try_append/flush render batch, flush records, RenderFlushReason
gradients.rs   166  linear/radial gradient build+insert, picture paint, gradient_error
traps.rs       547  render_traps_or_tris, ensure_trap_assets
emit.rs       2155  close-time replay: emit_recorded_*_into_cb, dst colour
                    pass/session draws, RecordedCompositeTarget, clip
                    scissors, color_layers, barrier_to_layout
for_tests.rs   181  the 11 *_for_tests methods
```

Total 13,197 (13,034 before; `mod`/`use` lines and fmt). All ≤ 5k; largest
`emit.rs`. Types stay in the root: `RenderEngineInner`/`StagingBuffer`/
scratch fields are read across files, and a child cannot see a sibling's
private fields. Inherent impls are spread per member (`impl X::fn *`), so
`apply` re-opens one `impl X {}` per file.

Root `//!` doc: the Stage-2c history became a short ownership description
plus this module map (its own commit, right after the move).

## Results

Prep commit: the 155 `super::` paths → `crate::kms::render::…` (sed
outside the root import, + fmt: +287/−227; engine tests green). Then
`split apply`, fmt, `split verify --manifest` → **OK**: 426/426 leaves
identical, 46 manifest visibility changes, 0 audited exceptions, 0 location
audits, 18 leaves with log targets moved to descendants, no shadowed names
outside the existing test helpers. `verify --tests` against fresh pre-move
snapshots: 4008 / 4013 / 4121 tests mapped (default / tcp-transport /
xdmcp). Rule-5 gate green: fmt, clippy ×3, tests ×3 (3693 / 3698 / 3806
passed), lavapipe 317 passed. Logs: `target/gate-engine-*.log`.

**Visibility delta (46, all private → `pub(super)` = the old reach):**
14 methods, 30 free fns, 2 consts. 30 are needed by the lib (cross-file
callers; `retire_image_after` was added for `export.rs`), 16 only by `engine/tests` (`clamp_copy_rects(_to)`,
`clamp_put_rect(_to)`, `coalescing_counts`, `session_step`,
`x11_src_row_stride`, `UPLOAD_COPY_ALIGN_MAX`, `resolve_force_opaque`,
`StagingBuffer::{new_with_usage, pick_memory_type}`,
`{split,record}_glyph_runs`, `effective_glyph_layout`,
`build_render_clip_scissors`, `trap_composite_src_origin_axis`). No
existing `pub(super)`/`pub(in …)` items moved; `pub(crate)` items unchanged.

**Re-exports (root `lines`):** `use` globs for frame, staging, scratch,
composite, emit; `pub(crate) use` for fill_copy, pixels, glyphs (outside
users, e.g. `decode_x11_pixel_for_storage`). No glob for the
method-only children (lifecycle, export, text, batch, traps, for_tests).

**Refusals and handling:**
1. Relative paths (sound): prep commit, 155 sites (decision 1).
2. Unused glob (compiler, not the tool): `put_get` and `gradients` free fns
   are file-local in the lib, so their globs warn. `gradients` gets none;
   `put_get`'s two test-only clamps get an explicit
   `#[cfg(test)] use put_get::{clamp_put_rect, clamp_put_rect_to};`
   instead of moving them to the root by usage.
3. Tool: whole inherent impls (`impl StagingPool`, …) cannot take member
   visibility entries ("matches no item") → spread per member instead.
   Wildcard `impl std :: fmt :: Debug for *` also matches members → listed
   explicitly.
4. Log targets, locations, macros, imports/traits in scope, includes: none.

## Decisions (jos, 2026-10-09)

1. Prep uses full `crate::kms::render::…` paths (as backend).
2. `emit.rs` stays one file (2.2k).
3. All types stay in `engine.rs`.
4. The 16 test-only visibility changes (same reach) are accepted.
5. `promote_drawable_exportable` + `copy_image_blocking` go to their own
   `export.rs` (no other dma-buf export helper lives in engine.rs).

## Commit sequence

1. `refactor(kms): qualify engine super:: paths (prep)`: 155 sites + fmt.
2. `refactor(kms): split engine into lifecycle/frame/op modules (move)`:
   `split apply` + fmt, `split verify --manifest …` and `--tests`, then the
   full gate (rule 5).
3. `docs(kms): engine root doc is an ownership line and module map`.
4. `chore: blame-ignore` for 1 and 2.
5. This doc and the plan updated to the final tree.
