# Shaped windows in a shared redirect backing — design

Status: rev 3, 2026-11-23. The user chose approach B. Spec and plan acceptance
were delegated by the user on 2026-11-22. Reviews:
`docs/superpowers/findings/2026-11-22-shaped-sibling-clip-spec-review-r1.md`,
`docs/superpowers/findings/2026-11-23-shaped-sibling-clip-spec-review-r2.md`.

## Why

An issue #23 reporter (Ivy Bridge, hasvk) sees two black 11×11 squares in the
bottom corners of every window when running xfwm4-gl with its compositor on.
xfwm4-gl ([fulalas/xfwm4-gl](https://github.com/fulalas/xfwm4-gl)) is an
xfwm4 4.20.0 fork with an OpenGL compositor and SVG themes with rounded bottom
corners.

Measured 2026-11-22/23. The scripts are in
`~/Projects/claude-scratch/xfwm-corners/r2/`.

- The fault reproduces with xfwm4-gl on yserver headless (lavapipe) and on real
  KMS (NVIDIA RTX 5060 Ti). It shows on screen and in `GetImage` of both the
  frame and the client.
- Xvfb 21.1.24 running the same session is clean.
- The renderer is not the trigger: it also reproduces with xfwm4-gl's XRender
  fallback. The trigger is the theme.
  - Each corner piece is a 16×16 child window of the depth-32 frame.
  - It is bounding-shaped to a rounded L (`0,0 5x11 · 0,11 16x4 · 1,15 15x1`).
  - Its background pixmap is depth 32 with alpha 0 outside the L.
  - The piece is mapped before it is shaped.
- yserver stores the shape correctly (`XShapeGetRectangles`). The defect is in
  how pixels reach the frame's shared redirect backing.
- Diagnostic spike (`test/shaped-sibling-clip`, `536042ea`, not for merge),
  with each half built separately. Pure-black frame pixels for the same
  session:

| build | bottom squares | top rounded tips | frame black px | client black px |
|---|---|---|---|---|
| master | present | Xvfb's | 252 | 242 |
| G1 only | gone | 8 px `#060606` where Xvfb has `#000000` | 10 | 0 |
| G2 only | present | — | 260 | 242 |
| G1 + G2 | gone | = Xvfb | 18 (= Xvfb) | 0 |

## Root cause

Both defects are in `crates/yserver/src/kms/render/backend/portable/`.

1. **A sibling occludes its whole rect.** `copy_area_shared_backing_occluders`
   (`paint_target.rs`) returns each mapped higher sibling as its full
   `width × height` rect and ignores its bounding shape.
   - The corner piece is stacked above the client, so the client's draws lose
     the whole 16×16, including the 11×11 the shape leaves open.
   - GTK's full-window `CopyArea` never reaches it. Measured: it persists after
     an exposing `ClearArea` and after a resize.
   - This is the visible defect.
2. **Backend-internal background paints ignore the window's own shape.**
   `clear_window_area_with_background` (`windows.rs`) has two branches.
   - The background-pixmap branch clips only against higher siblings.
   - The solid branch (`fill_window_background_solid`) clips through
     `subwindow_mode_clip(ClipByChildren)`.
   - A core `ClearArea` is already intersected with the core clip list before
     it reaches the backend (`core_loop/process_request/drawing.rs:56`,
     `windows.rs:216`, `core_loop/clip_list.rs:158,398`).
   - The backend's own callers are not pre-clipped:
     - realize at map (`windows.rs` ~2051);
     - the newly exposed area on resize (`sync_window_leaf_storage` ~582,
       `relocate_leaf_content_in_place` ~766);
     - `backend_windows_paint_window_background_rect` (~2546).
   - Through those callers a shaped window's pixmap background lands outside
     its shape. This causes the residual tip pixels in the table.

## Reference (Xorg)

- `mi/mivaltree.c`: a child's border universe is the parent universe ∩ its
  `borderSize` (`:426`). The parent universe then loses that `borderSize`, so
  a lower sibling loses only the shaped region (`:437`).
- The child's final `clipList` is that universe ∩ `winSize`, minus its own
  children (`:390`, `:446`).
- `borderSize` is already intersected with the bounding shape (`SetBorderSize`,
  `dix/window.c:1747-1770`). yserver's `child_clip_region` follows this for
  children.
- Background painting (`miPaintWindow`, `mi/miexpose.c:398`) fills a region the
  caller has already intersected with `clipList` (`:385`). This is the same for
  pixel and pixmap backgrounds.

## Goals

- **G1.** A mapped higher sibling sharing the backing occludes only its
  bounding-shaped outer region: shape ∩ outer rect, border included, as
  `child_clip_region` computes it. Every consumer of
  `copy_area_shared_backing_occluders` gets this:
  - client draws (`shared_backing_draw_clip`, `narrow_dst_clip_to_shared_backing`);
  - background clears;
  - the border ring paint (`windows.rs` ~1234);
  - shared-backing move carry (`paint_target.rs` ~807, ~892).
  An unshaped sibling behaves exactly as today.
- **G2.** The background-pixmap branch of `clear_window_area_with_background`
  clips through the same `subwindow_mode_clip(host_xid, ClipByChildren)` as the
  solid branch. The hand-rolled occluder subtraction there is removed, so both
  background kinds share one rule.
  - For a window drawing into an ancestor's shared backing, that rule is: own
    and ancestors' shapes, minus higher siblings, minus mapped children.
  - The existing behaviour for the root, for a window drawing into its own
    leaf, and for the owner of a redirect backing is preserved as it already
    is for solid backgrounds. There `shared_backing_draw_clip` returns `None`,
    and the root bypasses the clip.
  - Tile alignment (`tile_origin`) and the "fully clipped → nothing to do"
    early return stay.
- **G3.** xfwm4-gl's corners match Xvfb on yserver, headless and on real KMS.

## Non-goals

- Do not chase the GTK bottom-bar gradient difference found during
  measurement. yserver draws flat where Xvfb draws a 1–9 level gradient. It is
  a separate finding.
- No border-inclusive child subtraction in `subwindow_mode_clip`. It subtracts
  the child content box rather than the outer box; that is a pre-existing,
  documented difference and stays.
- No change to the core `ClearArea` path, the input shape, the clip shape
  (`kind=1`), ShapeNotify, or the scene/scanout shape handling
  (`scene/walk.rs`, `scanout.rs`).
- No new environment variables, no new clip type, no second shape cache.

## Behaviour to confirm against Xvfb (G2)

Through the backend-internal callers, G2 also stops a pixmap background from
painting over mapped children in a shared backing. This matches Xorg, because
`miPaintWindow` gets a `clipList`-clipped region, but it is unmeasured.

- The implementation adds an oracle case:
  - an unredirected parent with a pixmap background, below a redirected
    grandparent, so the parent paints into the grandparent's backing;
  - a mapped child of the parent, wholly inside both the old and the new
    parent size, that is not repainted afterwards;
  - the parent grown by `ConfigureWindow`, which runs the unpreclipped
    background initialization (`sync_window_leaf_storage`, `windows.rs`
    ~582);
  - `GetImage` of the child region, compared against Xvfb.
- A redirected parent cannot serve here: it skips that initialization
  (`windows.rs` ~2247), and its exposure repaint arrives pre-clipped by the
  core (`core_loop/process_request/windows.rs:2444`).
- If Xvfb differs from this design, stop and bring it back. Do not adjust the
  test.

## Constraints

- **Reuse first.** Build G1 on `child_clip_region` / `bounding_shape_local`
  and G2 on `subwindow_mode_clip` / `apply_subwindow_mode_clip`. Each new
  helper must say why the existing one cannot serve.
- **Tests drive real requests through the core loop.** Use `CreateWindow`,
  `ConfigureWindow`, `MapWindow`/`UnmapWindow`, `SHAPE Mask/Rectangles`,
  `ChangeWindowAttributes` background pixmap, `CopyArea` and `GetImage`, on
  the lavapipe render path. Do not inject clip state.
  - Each test ends with an end-state check of the backing pixels.
  - Each test of a new behaviour fails on the base.
  - Each test has a named mutation that makes it fail.
  - Preservation and oracle tests (T2, T4) need not fail on the base. They
    still carry a mutation.
- **Gate.**
  - workspace tests;
  - the whole lavapipe `--ignored` suite;
  - `just man`;
  - fmt and clippy;
  - render tests 5× under CPU load.
- **Hardware.** Repeat the xfwm4-gl reproduction on real KMS, asking the user
  before any GPU work.

## Tests (minimum)

- **T1, G1.**
  - Two siblings share a redirected parent's backing.
  - The higher one is bounding-shaped with a hole over the lower one.
  - A `CopyArea` into the lower one fills the hole and leaves the shaped part
    untouched.
  - Mutation: drop the shape from the occluder → fails.
- **T2, G1, preservation.**
  - The same setup without a shape: the higher sibling still occludes its
    whole rect.
  - Mutation: drop sibling occlusion → fails.
- **T3, G2, own shape through an internal paint.**
  - A child in a redirected parent's backing, bounding-shaped before it is
    mapped.
  - It has a depth-32 pixmap background whose pixels outside the shape differ
    from what lies beneath.
  - `MapWindow` (realize paint) leaves the pixels outside the shape untouched.
  - Mutation: revert the pixmap branch to the occluder-only clip → fails.
- **T4, G2, children (oracle).** The case from "Behaviour to confirm against
  Xvfb", asserting the Xvfb result. Mutation: drop the children cut → fails.
- **T5, end to end.** The xfwm4-gl corner sequence reduced to requests:
  - frame;
  - client below a corner piece with a pixmap background, mapped then shaped;
  - client draw;
  - then an unpreclipped repaint of the corner (unmap + map).
  After both the client draw and the corner repaint, the hole shows the
  client's pixels. Both the "G1 only" and the "G2 only" mutations fail it.
- **T6, G1, outer region.**
  - The higher sibling has a border, and its bounding shape extends into that
    border.
  - Backing-pixel assertions cover:
    - a lower-sibling draw;
    - the lower sibling's border-ring paint;
    - a shared-backing move carry of the lower sibling.
  - Mutation: intersect with the content rect instead of the outer rect →
    fails.

## Provenance

- Behaviour follows Xorg (MIT/X11). Line references only, no copied code.
- xfwm4 / xfwm4-gl are GPL. They were used only as test clients and their
  requests observed; none of their code enters yserver, this spec, or the plan.

## Left to the builder

- How occluders are represented once a sibling contributes several rects. A
  flat `Vec<Rect2D>` is enough: move carry subtracts the union and orders
  copies by geometry afterwards (`backend.rs:793,805`).
- Where each test lives: the backend `tests/paint_target.rs` module, the
  core-loop drawing tests, or render acceptance.
