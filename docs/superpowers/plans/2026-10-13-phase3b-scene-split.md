# Phase 3b: scene compose into `yserver-render`, output driving behind a trait

**Branch:** `plan/phase3b-scene-split` (plan only). Parent:
`2026-10-13-phase3-render-crate.md` ("3b outline", "Execution (3a)"), itself
under `2026-10-08-source-layout-cleanup.md`. Needs a codex review before code
moves.

**Goal:** the scene compositor (window walk, damage, repaint planning,
compositing into an image) moves into `yserver-render`; everything that drives
KMS outputs (scanout BOs, page flips, the copied two-GPU route, the cursor
plane) stays in `yserver` behind a narrow trait. Maintainability first (jos,
2026-10-13); the future second implementor (a macOS swapchain) is not built
here and shapes nothing beyond "no KMS type crosses the trait".

## Inventory (master `7b5fff83`, scripts in `target/phase3b-inventory/`)

Scene is 9,574 non-test lines plus 7,665 test lines (143 tests).

| class | fns | lines |
|---|---|---|
| compose | 176 | 5,468 |
| output driving | 28 | 216 |
| mixed | 8 | 1,693 |
| types, imports, docs | – | 2,197 |

- **Compose already has its seam.** All compositing goes through the private
  trait `ComposeRenderTarget` (`scene.rs:1126`). Its two output
  implementations, on `ScanoutBo` and `CopiedRenderSource` (114 lines), stay in
  `yserver`. Root readback, transform priming and the damage audit render into
  offscreen targets and never touch scanout or flip state.
- **The 8 mixed functions** carry the work: `tick_one_output` (950 lines,
  ~200 of them output), `tick` (181), `handle_page_flip_complete` (175),
  `drain_all` (117), `handle_scanout_render_completion_inner` (108),
  `submit_shared_scanout_frame` (72), `submit_copied_scanout_render` (48),
  `retire_failed_submit_bos` (42).
- **Scene → KMS:** 86 `platform.*` accesses: GPU 23 (via `platform.gpu`),
  output geometry 33, frame slots and flip 19, cursor plane 11. `KmsCore`:
  6 fields, 14 reads, all in `build_scene_with` / `root_node`. `WindowsMap`:
  two lookups and one iteration; elsewhere passed through. The direct DRM flip
  at `scene/compose.rs:213, 214, 260`.
- **Nothing calls back into scene from flip events:** the backend drains them,
  then calls scene.
- **Tests:** none of the 143 uses live Vulkan, scanout types or DRM (one
  `#[ignore]` is a benchmark). `PlatformBackend` appears in 60, 59 of them
  only to set output geometry (`platform_with_layout`); `KmsCore` in 61, only
  the six scene fields. All can move into render; one needs a trivial sink.
  Live-Vulkan scene coverage lives elsewhere (`crtc_transform_tests` 14,
  `root_scanout` 9, `render_acceptance` 17).

### `tick_one_output`, in order (`scene/tick.rs`)

1. Output: recycle failed BOs (562), GPU pool drain (569); compose gates.
2. `build_scene_with` (638): reads root rect and the cursor-plane `fits`
   query (`build.rs:558`).
3. Cursor-plane `visible` (664); compose 668–890: cursor transition, damage,
   empty-damage skip, audit.
4. Output: route check via `scanout_pools` (916), `acquire_scanout_bo` (938).
   Must stay here: repaint planning needs the BO's age.
5. Compose 958–1199: repaint plan, cull, audit, descriptor slot, fence
   ticket, scale pass.
6. Output 1142–1187: compose image, DRM device and pool borrowed from
   `platform` at once.
7. 1209–1275: BO phase change, **record** (compose), export fd, DRM flip
   (`compose.rs:260`). The record sits inside the output sequence.
8. Cursor-save finish; output `register_scanout_render_completion` (1294).
9. Push the in-flight entry, stage damage; on failure `invalidate_bo` (1410)
   or `cancel_scanout_bo_recording` (1453), chosen by `gpu_submitted` set in
   step 7.

## Decisions proposed for review

### 1. `SceneInput`: geometry and core state as data, not calls

Per tick, the backend builds a borrowed
`SceneInput { core: the 6 KmsCore fields, windows: &WindowsMap, outputs: &[OutputLayout], fb_size }`
(`OutputLayout` = root rect, transform, CRTC/output key as a plain index).
The 33 geometry accesses and the 14 `KmsCore` reads become reads of this
struct. No trait methods for geometry. `WindowsMap`/`WindowGeometry` move into
render; `backend` re-exports them.

### 2. `OutputSink`: frame slots and flips, one `&mut dyn` borrow

Scene takes `&mut dyn OutputSink` instead of `&mut PlatformBackend`.
`PlatformBackend` implements it. First cut, from the actual uses:

| method | replaces |
|---|---|
| `gpu(&mut self) -> &mut GpuCore` | the 23 GPU accesses (decision 3) |
| `acquire_slot(output) -> Option<(SlotToken, age)>` | route check + `acquire_scanout_bo` (step 4) |
| `begin_frame(output, slot) -> &mut dyn ComposeRenderTarget` | step 6's simultaneous borrows |
| `finish_frame(output, slot, …) -> FinishOutcome` | step 7's phase change, export, flip / copied registration; `FinishOutcome` carries `gpu_submitted` |
| `invalidate_slot`, `cancel_slot`, `recycle_failed_slots` | step 9 error paths, step 1 |
| `commit_slot_present`, `on_page_flip_complete` | flip retire bookkeeping |
| `submit_copied`, `register_render_completion`, `drain_render_completions`, `clear_render_completions` | the copied two-GPU route |

- **Record inside the output sequence (step 7):** `finish_frame` takes the
  recording as a closure (`&mut dyn FnMut(&mut dyn ComposeRenderTarget) -> Result<…>`)
  so the order phase change → record → export → flip is kept inside one
  call, exactly as today. Alternative for review: split into
  `prepare_flip` / `submit_flip` around the record.
- **Types crossing the trait** are plain data defined in render: `SlotToken`
  (today's `ScanoutBoToken`, an index), `OutputLayout`, `InFlight`,
  `FlipRetirement`, `FinishOutcome`, `ComposeRenderTarget` (made `pub` in
  render). `ScanoutBo`, `OutputScanout`, `CopiedRenderSource`, `BoState`,
  `OutputKey`, DRM `Device`/`Output` and GBM stay behind the trait.
- **Prerequisite:** `ComposeRenderTarget::post_compose_preparation` returns
  `PostComposePreparation`, which names the KMS type
  `CopiedTransportPreparation` (`scene.rs:1155`). It becomes private to the
  KMS implementations first: the trait method returns an opaque value the
  implementation itself consumes in `record_post_compose`.
- **`dyn`, not generics**, so scene is not monomorphised inside `yserver`
  (same reasoning as 3a's present edge).
- Exact signatures are settled in the steps; the review checks the shape and
  the ordering guarantees, not the parameter lists.

### 3. The GPU latch borrow

Two KMS methods set `gpu.renderer_failed` themselves
(`scanout_bos.rs:172`, `flip_events.rs:470`), so passing `&mut platform.gpu`
next to `&mut platform` as the sink is a double borrow. Proposal: scene gets
the GPU only through `sink.gpu()`, at the points it uses it today (no
long-lived `&mut GpuCore` across sink calls). The alternative, splitting
`PlatformBackend` into `GpuCore` + `KmsOutputs` siblings, is larger and not
needed for this step.

### 4. Cursor plane: its own small trait, a pass-through

The cursor-plane calls scene makes (`fits`, `visible`, `uploaded_version`,
`upload`, `show`, `hide`, `hide_all`, `note_composed_retirement`, and the
`CursorShowError::remains_visible()` contract at `cursor.rs:520`) go behind a
separate `CursorPlaneSink`, a one-to-one pass-through of today's methods.
The NVIDIA cursor investigation (issue #241, from 2026-10-13 evening) is
expected to change the cursor path. The legacy `MoveCursor` calls are outside
scene (`pointer.rs:647, 673`, `scanout.rs:1789`) and untouched by 3b; if the
NVIDIA work changes the retire-time show/hide/upload in `scene/cursor.rs`, the
3b step that wraps it lands after that.

### 5. Order of work

1. **Prep commits in `yserver`** (scene still in place, each gated):
   `CopiedTransportPreparation` made private to the KMS targets; the unused
   `&mut PlatformBackend` and parameters of `queue_steady_state_cursor_upload`
   (`cursor.rs:150`) removed; `SceneInput` introduced and the geometry/core
   reads switched to it; tests switched from `platform_with_layout` to a
   `SceneInput` fixture.
2. **The seam** (behaviour-adjacent): `OutputSink` + `CursorPlaneSink`
   defined, implemented by `PlatformBackend`, the 8 mixed functions split
   along them. One subsystem per commit where it compiles (slots/flip,
   copied route, cursor). Lavapipe and HW smoke after the series.
3. **The move:** scene (minus the two KMS `ComposeRenderTarget` impls, which
   move to the KMS side next to `ScanoutBo`) into `yserver-render`, mirrored
   paths as in 3a, `WindowsMap`/`WindowGeometry` with it; tests move; the 9
   test-only `pub` widenings that only scene tests needed go back to
   `pub(crate)`.

### 6. Tests

All 143 scene tests move into render; 142 need no sink, one uses a minimal
test sink. Same rule-4 mapping and "no `pub` for a test" rule as 3a.
`compose_for_tests` (`for_tests.rs:135`) already composes into an offscreen
target: an offscreen test sink that lets lavapipe tests drive whole ticks
without KMS is the natural follow-up, not part of 3b.

## Acceptance

- `cargo tree -p yserver-render` still free of `drm`, `gbm`, `input`, `udev`,
  `yserver-core`; render code names no `PlatformBackend`, `ScanoutBo` or
  `crate::drm`.
- Gates per commit, three feature configs, FreeBSD CI, test mapping, size
  ceilings.
- Seam series: lavapipe suite; HW smoke on silence (RADV) and eiger (Asahi):
  desktop, video, a fullscreen game or `glmark2 --fullscreen` for direct
  scanout, hotplug/output off-on, logout. The copied two-GPU route is gated
  on behaving as master does, known bugs included (on silence, i915
  rendering to the RX DP sink hangs on master too); fixing pre-existing bugs
  is out of scope.
- Of 3a's 23 test-only widenings, the 9 scene-only ones are reverted.

## Questions for the review

1. `SceneInput` as data plus `OutputSink` for slots/flips, rather than one
   trait carrying geometry too?
2. `finish_frame` taking the record as a closure, or a `prepare_flip` /
   `submit_flip` pair?
3. `sink.gpu()` for the GPU latch, rather than splitting `PlatformBackend`?
4. Cursor plane as a separate pass-through trait, sequenced after the NVIDIA
   cursor work if that touches `scene/cursor.rs`?
5. Anything in the ordering (acquire → plan → fence → record → export → flip;
   retire: BO phases, ack, pool-slot release, buffer age, cursor) the trait
   shape could break?
