# Clip lists: linear region algebra, single-pass sibling clipping, cache

Status: design, for review. Branch `perf/cached-clip-lists`. Refs #155.

## Problem

On the vaio (Haswell Pentium 3556U, 2C/1.7 GHz, MATE, release build,
`tools/x11perf-ratio.sh`), yserver vs Xorg on the same box:

| x11perf test | yserver / Xorg rate | server CPU per op, yserver / Xorg |
|---|---|---|
| `-pointer`, `-prop` (round trips) | 1.05, 0.91 | 1.7 |
| `-getimage10`, `-putimage10`, `-copywinwin10`, `-rect10` | 0.11, 0.16, 0.015, 0.03 | 10, 6.6, 54, 127 |
| `-create`, `-map`, `-popup` (200 kids) | ~1/400 | 430-560 |
| `-move`, `-resize` (200 kids) | 1/390, 1/920 | 300, 790 |

Round trips cost the same; the time is inside request processing. A
frame-pointer profile of `-move` then `-create` (139k samples, stacks
aggregated with inlining off) attributes 88% to request processing:

- ConfigureWindow, 38.6% of samples: 79% of it in `clip_list::clip_lists_of`
  (`TreeChange::begin` 40%, `TreeChange::exposed` 53%), under it
  `normalize_region_rects` 60%, `subtract_regions` 52%, plus allocation.
- MapSubwindows, 26.2%: `send_map_exposures` -> `not_clipped_by_children`
  65%; `realize_window_storage` -> `clear_window_area_with_background` ->
  `subwindow_mode_clip` -> `copy_area_shared_backing_occluders` 34%.

Three costs compound:

1. **Region operations are super-linear.** `nested::subtract_regions`
   splits every rect against every rect, `normalize_region_rects` scans all
   rects once per y band (O(k^2)), and `clip_list::canonical` normalises
   again. pixman, which Xorg uses, combines two y-x banded regions in one
   merge pass (`pixman_op`), linear in their rect counts.
2. **Each sibling's clip is computed from scratch.** `uncovered_in_child_space`
   subtracts every higher sibling for every child it is asked about, so a
   pass over n siblings is O(n^2) region operations. Xorg's
   `miComputeClips` (`mi/mivaltree.c:194-470`) walks the children once, top
   down, carrying the parent's still-uncovered region.
3. **Nothing is cached.** `clip_list`, `not_clipped_by_children` and friends
   recompute from the root on every call, including several times within one
   request (TreeChange before and after, ClearArea, CopyArea source and
   destination). Xorg keeps `clipList` and `borderClip` per window and only
   revalidates the windows a tree change marks (`miValidateTree`).

The KMS backend has its own, independent occlusion computations over its
window mirror (`copy_area_shared_backing_occluders` scans the whole window
map per ancestor level, `compute_copy_area_scissors` re-subtracts children
uncached); `subwindow_mode_clip` already has a single-entry generation cache.

## Goal and invariants

- Same output, faster. Every region a client sees (Expose and
  GraphicsExpose rects, their order and count, RECTLIMIT behaviour) stays
  byte-identical. Those are already pinned to Xorg by the exposure tests in
  `tests/window.rs` and friends, the vng `destroy-order` golden, and the
  `tools/` A/B probes (resize-expose, border-damage, composite-shrink).
- No feature flag. Each phase replaces the old path.
- Each phase is measured before/after on the vaio with
  `tools/x11perf-ratio.sh` (same session, same tests), and keeps the
  profile reproducible (`just yserver-mate-hw-perf`).

## Phase 1: a banded region type with pixman's operations

A `Region` (y-x banded, canonical by construction: bands sorted by y, rects
in a band sorted by x, no empty rects, touching bands with equal spans
coalesced) with `union`, `intersect`, `subtract` implemented as pixman's
band merge (`pixman_op` with the `union_o`/`intersect_o`/`subtract_o`
overlap functions and `pixman_coalesce`), plus `translate`, extents and the
empty/contains tests.

- The output is pixman's canonical form directly, so `canonical()` and the
  `normalize_region_rects` pass disappear from the clip path.
- `clip_list.rs` switches to it first (it is the hot path and already
  speaks canonical form). The SHAPE/XFIXES users of `nested::*_regions`
  follow in the same phase if the swap is mechanical, else stay as they are.

Oracle for the tests (the operations, not the call sites): the current
implementation is slow but its set semantics are right and its canonical
form is tested against Xorg exposures. Differential tests over random small
regions (coordinates in a small grid so that edges coincide often):

- same pixel set as `nested::{union,intersect,subtract}_regions`, checked by
  rasterising both;
- same rect list as `clip_list::canonical` of the old result.

Plus hand-written cases from pixman's own region test expectations where
they pin coalescing (two bands with equal spans merge; a hole splits into
four rects).

## Phase 2: one top-down pass over siblings

Restructure `clip_list.rs` the way `miComputeClips` walks children: for a
parent with universe U, visit its children top-most first, give each child
`U ∩ borderSize(child)` (cut to its winSize for the clip/universe it hands
down), then `U -= borderSize(child)` when the child is viewable and not
transparent. One region operation per sibling instead of one per sibling
pair.

- `walk`, `clip_lists_of`, `subtree_clip_lists`, `clip_lists_under` and
  `clip_list_from` use the running universe.
- `not_clipped_by_children(w)` for a single window still has to account for
  w's higher siblings at each ancestor level; it subtracts them once per
  level (as today) but with Phase 1 operations. Phase 3 makes the repeated
  calls cheap.
- `send_map_exposures` for MapSubwindows computes the parent's universe once
  for the whole batch instead of once per mapped child.

Same tests as above; no new oracle needed, the outputs must not change.

## Phase 3: a per-window cache keyed on a tree generation

`ServerState` gets a `tree_generation: u64` and a cache
`HashMap<ResourceId, CachedClip>` holding `not_clipped_by_children`,
`clip_list` and the border ring, stamped with the generation they were
computed at. A lookup at a stale generation recomputes.

Invalidation is deliberately coarse: any change that can affect any clip
bumps the generation, which drops the whole cache. That trades re-use
across tree changes (Xorg's `MarkOverlappedWindows` precision) for
invalidation that is easy to get right. It still pays off where the
profile and the ratios point:

- within one request (TreeChange after, CopyArea source/destination,
  ClearArea, exposure fan-out all reading the same windows' clips);
- across the drawing requests between two tree changes (the 6-127x drawing
  ratios).

What bumps it (from the mutation inventory):

- `ResourceTable::window_mut` (every geometry, map-state, class, parent and
  child-order write goes through it or through `ResourceTable` methods that
  use it), `create_window`, `apply_destroy`, `restack_window`,
  `reparent_window`, `map_window`/`unmap_window` and the
  promote/demote helpers;
- every write to `shape_windows` bounding or clip;
- every change to `composite_redirects` window modes (Manual makes a window
  transparent) and to `Window.redirected_backing`;
- root and COW resizes on a screen size change.

To make the bump hard to forget, the writers move behind methods that bump
(`window_mut` bumps like the backend's `TrackedWindows::deref_mut`; the
shape and redirect maps get the same wrapper). In debug and test builds a
check recomputes on every cache hit and asserts equality, so a missing bump
fails the existing exposure tests instead of shipping a stale clip.

If the coarse scheme leaves `-move` dominated by recomputation (every move
bumps), measure before refining: the TreeChange before/after pair is then
two Phase 2 passes over the touched layer, which should already be cheap.

## Phase 4: the backend occluders (measure first)

After Phases 1-3, re-profile `-create` and `-copywinwin10`. If
`copy_area_shared_backing_occluders` / `compute_copy_area_scissors` /
`resolve_paint_target` still dominate:

- index the backend mirror's children by parent instead of scanning the
  whole window map per ancestor level;
- cache the occluder set per (window, generation) the way
  `subwindow_mode_clip` already does with `windows.generation()`,
  `topology_generation()` and `shape_generation`;
- share the child subtraction between `compute_copy_area_scissors` and
  `compute_subwindow_mode_clip`.

Not designed further here; the profile after Phase 3 decides.

## Out of scope

- The fixed per-request cost (`-noop` 57x slower, ~3 us server CPU per
  NoOperation). Separate: profile `x11perf -noop` on its own.
- The render scene walk (`scene/walk.rs`): it runs per frame, not per
  request, and has its own capped `Region`.
- Fine-grained invalidation (Xorg's marking) unless Phase 3's measurement
  asks for it.

## Measurement plan

Per phase, on the vaio, same MATE session through `just yserver-mate-hw`:

1. `tools/x11perf-ratio.sh` against yserver; compare to the Xorg baseline
   and to the previous phase.
2. For the phase that targets them, a perf profile of the specific tests
   (`-move`, `-create`, `-copywinwin10`).
3. Gates: unit tests, vng (CI), the `tools/` A/B probes against Xephyr,
   HW smoke on a desktop (MATE, Cinnamon) before merge.
