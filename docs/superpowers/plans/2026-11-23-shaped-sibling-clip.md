# Shaped windows in a shared redirect backing — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task by task. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Goal:** A shaped window that shares a redirect backing occludes, and
paints its pixmap background, only inside its bounding shape, as in Xorg.

**Architecture:**
- **G1.** `copy_area_shared_backing_occluders` contributes each higher
  sibling's shape ∩ outer rect, reusing `child_clip_region`.
- **G2.** The background-pixmap branch of `clear_window_area_with_background`
  takes the same `subwindow_mode_clip(ClipByChildren)` clip as the solid
  branch.
- No new types, no new caches.

**Tech Stack:** Rust; the yserver KMS backend
(`crates/yserver/src/kms/render/backend/portable/`); Vulkan through lavapipe
for the tests.

**Spec:** `docs/superpowers/specs/2026-11-22-shaped-sibling-clip-design.md`
(rev 3, converged). Read it first. Its Goals, Non-goals and Tests sections
bind this plan.

## Global Constraints

- **Reuse first.** G1 is built on `child_clip_region` / `bounding_shape_local`
  (`paint_target.rs`); G2 on `subwindow_mode_clip` / `apply_subwindow_mode_clip`
  (`clip.rs`). A new helper must say in its doc comment why an existing one
  cannot serve.
- **Non-goals (spec):**
  - no change to `subwindow_mode_clip`'s content-box child subtraction;
  - no change to the core `ClearArea` path, input or clip shape, ShapeNotify,
    or `scene/walk.rs` / `scanout.rs`;
  - nothing about the GTK gradient;
  - no `YSERVER_*` env vars.
- **Real paths (rules A, B, D).**
  - Tests build their windows and shapes and change state only through
    `yserver_core::core_loop::process_request::process_request` on a
    `ServerState` with a `KmsBackend::for_tests_with_vk()`. This is the
    pattern of `paint_target.rs`'s `dispatch_for_cow_test`,
    `tests/mod.rs`'s `create_live_window`, `dispatch_configure_window` and
    `install_client_for_render`.
  - Requests used: `CreateWindow`, `ChangeWindowAttributes` (CWBackPixmap /
    CWBackPixel / CWBorderPixel), `MapWindow`, `UnmapWindow`, `ConfigureWindow`,
    `CreatePixmap`, `PolyFillRectangle`, `CopyArea`, SHAPE `Rectangles` /
    `Mask`, and Composite `RedirectWindow` (manual). Never insert into
    `b.windows` or `b.store` by hand.
  - Pixels are read through the `Backend::get_image` trait entry, the same one
    `GetImage` uses.
  - Each test ends with an end-state pixel check.
  - Each test is `#[ignore = "needs live Vulkan ICD"]` like its neighbours.
- **Base and mutation.**
  - Every new-behaviour test fails on base `3ef6570b`.
  - Every test names a mutation that makes it fail. Check the mutation by
    editing the line and running, and record the result in the commit
    message.
  - Preservation and oracle tests (T2, T4) need not fail on the base.
- **Per-task gate** (from `/data/Projects/yserver-shaped-clip`):
  ```
  cargo +nightly fmt
  cargo clippy --workspace --all-targets --locked -- -D warnings
  tools/check-layout.sh
  VK_DRIVER_FILES=/home/ariel_santangelo/Projects/claude-scratch/yserver-pr1/lvp/lvp_icd.json YSERVER_ALLOW_SOFTWARE_VULKAN=1 cargo test -p yserver --locked kms::render::backend::tests::paint_target -- --ignored
  cargo test -p yserver --locked kms::render::backend::tests::paint_target
  ```
- **Final gate** (Task 3):
  - workspace `cargo test --workspace --locked --features xdmcp`;
  - the whole lavapipe `-- --ignored` suite with the env above;
  - `just man`;
  - rule F: the `paint_target` `--ignored` tests 5× under 16 busy loops.
- **Commits.**
  - Signed (`git commit -S`), one per task.
  - Each ends with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`
    and `Implemented-By: codex (gpt-6-luna xhigh)`.
  - Never a session URL.

## Review Focus

Most likely to bite first:

1. **Bordered shaped sibling.** The shape is in content space, while the
   occluder must cover the border too. `child_clip_region` offsets by
   `x + bw`; the occluder offset uses the sibling's paint-target offset. Mixing
   the two spaces shifts the hole by `bw`. Pinned by T6.
2. **Root and own-leaf windows with a pixmap background.**
   `subwindow_mode_clip` returns `None` for the root and owner/leaf targets.
   The pixmap branch must then clear the whole rect, as today. Pinned by T7.
3. **Tile alignment.** After G2 cuts the rect into pieces, each piece must
   still sample at `piece + tile_origin`, not at the piece's own `0,0`.
   Pinned by T3's second island at a nonzero local origin.
4. **A sibling shaped to empty** (an unmapped-like zero-rect shape). It must
   occlude nothing and must not panic on an empty `Vec`. Covered in T1 as a
   second assertion.
5. **ParentRelative / ancestor shapes.** The pixmap branch now clips to
   ancestors' shapes. A child of a shaped parent loses pixels outside the
   parent's shape, as the solid branch already does. Covered by the
   reused `shared_backing_draw_clip`; no new test.

---

### Task 1: G1 — a shaped sibling occludes only its shape

**Files:**
- Modify: `crates/yserver/src/kms/render/backend/portable/paint_target.rs`
  (`copy_area_shared_backing_occluders`, ~950-1000).
- Test: `crates/yserver/src/kms/render/backend/tests/paint_target.rs`.

**Interfaces:**
- `copy_area_shared_backing_occluders(&self, dst_host_xid: u32, dst_target:
  &PaintTarget) -> Vec<ash::vk::Rect2D>` keeps its signature. A shaped sibling
  may now contribute several rects. The order stays by stack rank; the
  consumers only take the union (spec, "Left to the builder").
- Produces for Task 2/3: nothing new. Every consumer listed in spec G1 gets the
  change.

- [ ] **Step 1: Write the failing tests** (lavapipe, `#[ignore]`).
  - **`shaped_higher_sibling_occludes_only_its_shape_in_shared_backing` (T1).**
    - Setup:
      - redirected (manual) parent P, 64×64, depth 24;
      - lower child L at 0,0, 64×64, background pixel `0x000000`;
      - higher child U at 0,0, 16×16, background pixel `0x00ff00`, mapped and
        then SHAPE-Rectangles bounding `0,0 5x16` + `0,11 16x5` (an L);
      - fill a pixmap with `0xff0000` and `CopyArea` it over all of L.
    - Assert, via `get_image` on P:
      - `(10,5)` (in the hole) == `0xff0000`;
      - `(2,2)` (in U's shape) == `0x00ff00`;
      - `(30,30)` == `0xff0000`.
    - Second assertion: reshape U to an empty rectangle list, `CopyArea`
      again, and `(2,2)` == `0xff0000`.
    - Mutation: replace the shaped contribution with the full sibling rect →
      `(10,5)` fails.
  - **`unshaped_higher_sibling_still_occludes_its_rect` (T2, preservation).**
    - Setup: as T1 without the SHAPE request.
    - Assert `(10,5)` == `0x00ff00` and `(30,30)` == `0xff0000`.
    - Mutation: skip unshaped siblings in the occluder list → fails.
  - **`bordered_shaped_sibling_occludes_shape_within_outer_rect` (T6).**
    - Setup:
      - redirected parent P, 64×64;
      - lower child L at 0,0, 40×40, `border_width` 4, border pixel
        `0x808080`, background `0x000000`. L's content starts at P `(4,4)`;
      - higher child U at 0,0, 16×16, `border_width` 2, border pixel
        `0x0000ff`, background `0x00ff00`. U's outer rect is P `(0,0)-(20,20)`;
      - U's bounding shape, in content space: `-2,-2 6x20` + `-2,12 20x6`.
        In P that is `(0,0) 6x20` + `(0,14) 20x6`, so it covers part of U's
        border.
    - Pixels checked in P:
      - `(10,8)`: L content, inside U's outer rect, outside the shape;
      - `(10,2)`: L's border ring, inside U's outer rect, outside the shape;
      - `(1,1)`: U's border, inside the shape;
      - `(3,8)`: U's content, inside the shape.
    - **(a) draw:** `CopyArea` a 40×40 depth-24 pixmap of one-pixel columns
      over all of L. Even columns are `0xff0000`, odd columns `0x00ffff`, by P
      x coordinate once drawn. Then:
      - `(10,8)` == `0xff0000`;
      - `(1,1)` == `0x0000ff`;
      - `(3,8)` == `0x00ff00`.
    - **(b) border ring:** `ChangeWindowAttributes` L's border pixel to
      `0xffff00`. This repaints L's ring through
      `copy_area_shared_backing_occluders`.
      - `(10,2)` == `0xffff00`;
      - `(1,1)` == `0x0000ff`.
    - **(c) move carry:** `ConfigureWindow` L to `x=1`, with no client
      redraw.
      - `(10,8)` == `0x00ffff`: carried through the hole from P `(9,8)`.
      - `(3,8)` == `0x00ff00`: destination excluded, U's shape untouched.
      - `(6,8)` != `0x00ff00`: its source P `(5,8)` lies in U's shape, so U's
        pixels must not be carried into L. Whether the spot then holds L's
        background or its earlier pixel is the exposure path's business, not
        this test's.
      Then `ConfigureWindow` L back to `x=0`:
      - `(10,8)` == `0xff0000`;
      - `(3,8)` == `0x00ff00`.
    - Mutations:
      - intersect U's shape with its content rect instead of its outer rect →
        (b) `(1,1)` fails;
      - full-rect occluder for every consumer → (a) `(10,8)` fails;
      - full-rect occluder only for the two move-carry calls
        (`paint_target.rs` ~807, ~892) → (c) `(10,8)` fails.

- [ ] **Step 2: Run them on the base.** Expected: T1 and T6 FAIL on
  `(10,5)` / `(10,8)`; T2 PASS.
- [ ] **Step 3: Implement.** In `copy_area_shared_backing_occluders`, for a
  sibling with a bounding shape, contribute `child_clip_region(sibling,
  geom, outer)` translated into `dst` content space. For an unshaped sibling,
  contribute the existing rect, unchanged.
  - Derive the translation from the same paint-target offsets the current
    code uses. `child_clip_region` places the shape at the sibling's CONTENT
    origin `(x + bw, y + bw)` in parent content space, and `PaintTarget::offset`
    is a content origin (`yserver-render/src/kms/render/target.rs:134-155`).
    So subtract `(geom.x + bw, geom.y + bw)` and add `sibling_target.offset()
    - dst_target.offset()`. This is the border design's
    `W.border_width + C.x + C.border_width` rule
    (`docs/superpowers/specs/2026-09-07-window-borders-design.md:444-451`).
  - Keep the outer rect border-inclusive (Review Focus 1).
- [ ] **Step 4: Run** the three tests plus the existing `paint_target` tests
  (both commands of the per-task gate). Expected: all PASS.
- [ ] **Step 5: Check each mutation** and record which assertion failed.
- [ ] **Step 6: Commit** `fix(render): a shaped sibling occludes only its shape in a shared backing`.

### Task 2: G2 — the pixmap-background clear uses the solid branch's clip

**Files:**
- Modify: `crates/yserver/src/kms/render/backend/portable/windows.rs`
  (`clear_window_area_with_background`, pixmap branch ~847-900). Use
  `subwindow_mode_clip` (`clip.rs`) and the `Rect2D` helpers
  `compute_copy_area_dst_rects` / `intersect_rect_with_clip`.
- Test: `crates/yserver/src/kms/render/backend/tests/paint_target.rs`.

**Interfaces:**
- Consumes, all existing:
  - `subwindow_mode_clip(&self, host_xid: u32, mode: SubwindowMode) ->
    Option<SubwindowModeClip>`, whose fields are `cut: Vec<Rect2D>` and
    `keep: Option<Vec<Rect2D>>` (`backend.rs:664`);
  - `compute_copy_area_dst_rects` (`draw.rs`);
  - `intersect_rect_with_clip` (`clip.rs`).
- Produces: none.

- [ ] **Step 1: Write the failing tests** (lavapipe, `#[ignore]`).
  - **`pixmap_background_realize_paints_only_inside_own_shape` (T3).**
    - Setup:
      - redirected parent P, depth 32, 64×64, background pixel `0xff000000`
        (opaque black). Depth-32 windows need a 32-bit visual, `CWColormap`
        and `CWBorderPixel`, as xfwm4 frames do;
      - child C of P, depth 32, at 8,8, 16×16, with a depth-32 16×16
        background pixmap of four 4-pixel column bands, all alpha `0xff`:
        cols 0-3 `0xffff0000`, 4-7 `0xff00ff00`, 8-11 `0xff0000ff`,
        12-15 `0xffffff00`;
      - before `MapWindow`, C is SHAPE-Rectangles bounded to two islands,
        `0,0 4x16` and `10,0 4x16`.
    - Assert RGB on P after `MapWindow` C:
      - `(9,10)`, local col 1, == `0xff0000`;
      - `(18,10)`, local col 10, == `0x0000ff` (second island, tile phase kept);
      - `(20,10)`, local col 12, == `0xffff00`;
      - `(14,10)`, local col 6, outside the shape, == `0x000000` (untouched).
    - Mutations:
      - restore the occluder-only clip in the pixmap branch → `(14,10)` fails;
      - sample each surviving piece from tile `(0,0)` instead of
        `piece + tile_origin` → `(18,10)` fails.
  - **`pixmap_background_grow_does_not_paint_over_mapped_child` (T4, oracle).**
    The case measured on 2026-11-23 against Xvfb 21.1.24
    (`~/Projects/claude-scratch/xfwm-corners/oracle/t4.c`).
    - Setup:
      - grandparent G redirected (manual), 200×200, background `0x00ff00`;
      - parent P inside G, not redirected, 60×60, background pixmap 8×8 of
        `0xff0000`;
      - child C inside P at 10,10, 30×30, background pixel `0x0000ff`;
      - all mapped. Then `ConfigureWindow` P to 120×120.
    - Assert via `get_image`:
      - C `(5,5)` == `0x0000ff` (Xvfb: `0000ff`; master: `ff0000`);
      - P `(100,100)` == `0xff0000`.
    - Mutation: pass `SubwindowMode::IncludeInferiors` (no children cut) →
      C fails.
  - **`pixmap_background_clear_on_own_leaf_is_unclipped` (T7, Review Focus 2).**
    - Setup: a top-level unredirected window W with no children, 32×32, with a
      uniform `0xff0000` 8×8 background pixmap. W draws into its own leaf, so
      `subwindow_mode_clip` returns `None`.
    - `ConfigureWindow` W to 48×48.
    - Assert: `get_image` on W at `(40,40)` == `0xff0000`. This is today's
      behaviour, kept.
    - Mutation: treat `None` from `subwindow_mode_clip` as "clip to nothing"
      → fails.

  - **`pixmap_background_piece_beyond_i16_is_painted` (T8, review r2 B-1).**
    - Setup:
      - redirected parent P, 64×64, background pixel `0x000000`;
      - child C of P at x = −32768, y = 0, 40000×16, with an 8×8 `0xff0000`
        background pixmap, not yet mapped;
      - child D of C at local x = 32760, 9×16, background pixel `0x0000ff`,
        mapped;
      - then map C.
    - C's background clear minus D leaves `[0,32760)` and `[32769,40000)`
      locally. The second piece covers P x = 1…63, and starts beyond
      `i16::MAX` in C's local space. C's own leaf allocation may fail at this
      width; realize still paints into P's backing.
    - Assert on P:
      - `(10,8)` == `0xff0000`;
      - `(0,8)` == `0x0000ff` (D: local 32768 lies in `[32760,32769)`).
    - Mutation: route the pixmap branch through `apply_subwindow_mode_clip` →
      `(10,8)` fails.
    - If setup cannot reach the realize paint, for example because core
      rejects the geometry, stop and report; do not reshape the test.

- [ ] **Step 2: Run on the base (with Task 1).** Expected: T3 and T4 FAIL as
  stated; T7 PASS; T8 follows the base's occluder-only branch, so record
  its base result without requiring failure.
- [ ] **Step 3: Implement.**
  - In the pixmap branch, replace the occluder subtraction with the
    `SubwindowModeClip` from `subwindow_mode_clip(host_xid,
    SubwindowMode::ClipByChildren)`. `None` means the whole rect.
  - Apply its `cut` and `keep` (both `Vec<ash::vk::Rect2D>`) to the
    `Rect2D` clear rect with the existing 32-bit rect algebra:
    - subtract `cut` with `compute_copy_area_dst_rects`;
    - intersect with `keep` (when `Some`) with `intersect_rect_with_clip`.
  - Do NOT go through `apply_subwindow_mode_clip`. That wrapper converts to
    `Rectangle16` and drops pieces whose origin exceeds `i16`
    (`clip.rs` ~675-686), while this branch tiles in `Rect2D`. The rule is the
    same; only the 16-bit adapter is skipped. Say so in a one-line comment.
  - Keep the "nothing survives → return Ok" early return.
  - Keep each piece's `src_x/src_y = piece + tile_origin`.
  - Remove imports that become unused.
- [ ] **Step 4: Run** the per-task gate. Expected: all PASS.
- [ ] **Step 5: Check each mutation** and record the results.
- [ ] **Step 6: Commit** `fix(render): a pixmap background clears through the window's clip`.

### Task 3: T5 end to end, and the final gate

**Files:**
- Test: `crates/yserver/src/kms/render/backend/tests/paint_target.rs`.
- No production change expected. If T5 fails with Tasks 1 and 2 in place,
  stop and report; do not patch.

- [ ] **Step 1: Write `xfwm_corner_piece_shows_client_through_its_hole` (T5).**
  - Setup, as xfwm4-gl's sequence reduced to requests:
    - frame F, redirected (manual), depth 24, 64×64, background none;
    - client K at 0,0, 64×64, background pixel `0x000000`;
    - corner piece Q at 0,48, 16×16, stacked above K, with a 16×16 depth-24
      background pixmap of `0x101010`;
    - map Q, then SHAPE-Rectangles Q to the L `0,0 5x11` + `0,11 16x5`;
    - `CopyArea` a `0xff0000` pixmap over all of K;
    - then `UnmapWindow` Q and `MapWindow` Q (an unpreclipped realize paint).
  - Assert on F after the client draw and again after the remap:
    - `(10,52)` (in the hole) == `0xff0000`;
    - `(2,60)` (in the L) == `0x101010`.
  - Mutations, both must fail it:
    - revert Task 1 only (the client draw cannot reach the hole);
    - revert Task 2 only (the remap paints `0x101010` into the hole).
- [ ] **Step 2: Run** it with Tasks 1 and 2. Expected: PASS. Run both
  mutations and record which assertion failed.
- [ ] **Step 3: Final gate** (Global Constraints). Record the pass/fail counts
  in the commit message.
- [ ] **Step 4: Commit** `test(render): xfwm4-gl corner piece shows the client through its hole`.

Outside codex, done by the coordinator:
- the xfwm4-gl reproduction headless and against Xvfb, with
  `~/Projects/claude-scratch/xfwm-corners/r2/run-diag.sh` on a release build;
- the hardware run, only with the user's OK.
