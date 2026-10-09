# Phase 2.10 proposal: split `kms/render/engine.rs` (production code)

Step 2.10 of `2026-10-08-source-layout-cleanup.md`. Manifest:
`tools/split/manifests/engine_2.toml` (+ `.paths`). **Proposal + dry run;
not yet approved or executed.** Sizes below are from the dry run (after
`cargo +nightly fmt`).

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
- **Root attribute:** `#![allow(dead_code)]` applies to the children too.

## Target tree (dry-run lines after fmt)

All under `kms/render/engine/`, depth 1 (so `pub(super)` = old private
reach). Names avoid every first path segment used in the file (`vk`,
`vk_render`, `render_pipeline`, `ops`, `trap_pipeline`, `frame_builder`,
`store`, `platform`, `telemetry`, …) and `tests`.

```
engine.rs     1103  //! doc, imports, mod decls, root re-exports, every type,
                    From/Debug/Send/Sync impls, mod tests
lifecycle.rs   888  new/stub/is_live, poll_retired, retire/destroy image,
                    shutdown, drain_all, Drop for RenderEngine, SubmittedOp,
                    adopt_retired_resource, create_pixmap, notify_retired,
                    active_resource_bytes, promote_drawable_exportable,
                    copy_image_blocking
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

Total 13,193 (13,034 before; `mod`/`use` lines and fmt). All ≤ 5k; largest
`emit.rs`. Types stay in the root: `RenderEngineInner`/`StagingBuffer`/
scratch fields are read across files, and a child cannot see a sibling's
private fields. Inherent impls are spread per member (`impl X::fn *`), so
`apply` re-opens one `impl X {}` per file.

Root `//!` doc (in the move commit or right after): replace the Stage-2c
history with "drawing primitives into `DrawableStore` storage, recorded into
the frame builder and replayed at close" + the module map above.

## Dry-run results

Prep (uncommitted, throwaway commit for verify): the 155 `super::` paths →
`crate::kms::render::…` (sed outside the root import, + fmt: +287/−227).
Then `split apply`, fmt, `env -u RUSTC_WRAPPER cargo build -p yserver` and
`cargo clippy -p yserver --all-targets -- -D warnings`: green; fmt
`--check` clean. `split verify --manifest` → **OK**: 426/426 leaves
identical, 45 manifest visibility changes, 0 audited exceptions, 0 location
audits, 18 leaves with log targets moved to descendants, no shadowed names
outside the existing test helpers. Logs: `target/gate-engine-dry-*.log`.
Not run in the dry run: `--features` configs, `verify --tests`, the tests
themselves, lavapipe (rule-5 gate is for the move commit).

**Visibility delta (45, all private → `pub(super)` = the old reach):**
13 methods, 30 free fns, 2 consts. 29 are needed by the lib (cross-file
callers), 16 only by `engine/tests` (`clamp_copy_rects(_to)`,
`clamp_put_rect(_to)`, `coalescing_counts`, `session_step`,
`x11_src_row_stride`, `UPLOAD_COPY_ALIGN_MAX`, `resolve_force_opaque`,
`StagingBuffer::{new_with_usage, pick_memory_type}`,
`{split,record}_glyph_runs`, `effective_glyph_layout`,
`build_render_clip_scissors`, `trap_composite_src_origin_axis`). No
existing `pub(super)`/`pub(in …)` items moved; `pub(crate)` items unchanged.

**Re-exports (root `lines`):** `use` globs for frame, staging, scratch,
composite, emit; `pub(crate) use` for fill_copy, pixels, glyphs (outside
users: `engine::clamp_rect`, `decode_x11_pixel_for_storage`,
`premul_from_wire_pixel`, `uniform_pixel_glyph_source`, …). Method-only
children get none.

**Refusals and handling:**
1. Relative paths (sound): prep commit, 155 sites (alternative: import the
   modules once in the root and drop `super::`, a smaller diff; open q. 1).
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

## Open questions

1. Prep style: full `crate::kms::render::` paths (as backend, done in the dry
   run) or root `use super::{frame_builder, submit_group, …}` + bare paths?
2. `emit.rs` at 2.2k as one file, or `emit/{mod, copy, fill, composite,
   text_traps}` (needs `pub(in crate::kms::render::engine)` at depth 2)?
3. Types all in the root (1.1k) vs a `types.rs` (plan's 2.10 row): a child
   would need `pub(super)` on the private fields it owns; proposal: root.
4. The 16 test-only `pub(super)` changes: accept (as in 2.6/2.11)?
5. `copy_image_blocking` + `promote_drawable_exportable` in `lifecycle` (DRI3
   export realloc) or their own `export.rs` (~220 lines)?

## Commit sequence (after approval)

1. `refactor(kms): qualify engine super:: paths (prep)`: 155 sites + fmt.
2. `refactor(kms): split engine into lifecycle/frame/op modules (move)`:
   `split apply` + fmt, `split verify --manifest …` and `--tests`, then the
   full gate (rule 5).
3. `chore: blame-ignore` for 1 and 2.
4. This doc and the plan updated to the final tree.
