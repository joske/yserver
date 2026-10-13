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
`PlatformBackend` implements it. Every method borrows the sink only for the
duration of the call; scene never holds a reference obtained from the sink
across another sink call. First cut, from the actual uses:

| method | replaces |
|---|---|
| `gpu(&mut self) -> &mut GpuCore` | the 23 GPU accesses (decision 3) |
| `cursor_plane(&mut self) -> &mut dyn CursorPlaneSink` | the 11 cursor-plane accesses (decision 4) |
| `route(output) -> OutputRoute` (`Shared` / `Copied` / `None`) | the route check at `tick.rs:916`; called **before** acquisition, as today, because damage is recorded for shared outputs only (step 3b) |
| `acquire_slot(output) -> Option<SlotToken>` | `acquire_scanout_bo` (`tick.rs:938`) |
| `slot_image(output, slot) -> Option<vk::Image>` | step 6's compose-image lookup (shared BO image or copied source image), for `cursor_saves.prepare`; returns a handle, no borrow |
| `finish_frame(output, slot, record) -> FinishOutcome` | step 7 (decision 2a) |
| `invalidate_slot`, `cancel_slot`, `recycle_failed_slots` | step 9 error paths, step 1 |
| `commit_slot_present`, `on_page_flip_complete` | flip retire bookkeeping |
| `submit_copied`, `register_render_completion`, `drain_render_completions`, `clear_render_completions` | the copied two-GPU route |

- **`SlotToken`** is today's `ScanoutBoToken` (`platform.rs:99`) moved into
  render unchanged: `output_idx`, `bo_idx`, `extent`,
  `last_present_generation`, `content_invalidated`. All plain data; repaint
  planning keeps reading exactly these fields (`tick.rs:960–964`, and `extent` at 1254, 1354).
- **Types crossing the trait** are plain data defined in render: `SlotToken`,
  `OutputRoute`, `OutputLayout`, `InFlight`, `FlipRetirement`,
  `FinishOutcome`, `ComposeRenderTarget` (made `pub` in render). `ScanoutBo`,
  `OutputScanout`, `CopiedRenderSource`, `BoState`, `OutputKey`, DRM
  `Device`/`Output` and GBM stay behind the trait.
- **`dyn`, not generics**, so scene is not monomorphised inside `yserver`
  (same reasoning as 3a's present edge).
- Exact signatures are settled in the steps; the review checks the shape and
  the ordering guarantees, not the parameter lists.

#### 2a. `finish_frame`: the record runs inside the output sequence

Today (`compose.rs:211–286`, shared; 288–336, copied) the order is: phase
check → (copied: renderer acquire) → `transition_to_recording` → record and
submit → export the completion fd → flip (shared) or export the render
completion (copied) → phase transitions, with the fd ownership dance on
reject. `finish_frame` keeps that order inside one call and takes the record
step as `&mut dyn FnMut(&mut dyn ComposeRenderTarget) -> Result<ComposeSubmit, PresentError>`.

Borrow sequence, explicitly:
1. Scene asks `route`, `acquire_slot`, `slot_image` (each a short borrow;
   only plain values come back).
2. Scene prepares everything the record needs from its own state (pipeline,
   descriptor pool, fence ticket, scale pass, overlay ops, cursor save) and
   builds the closure over those, plus `&mut gpu_submitted` and
   `&mut compose_complete`. The closure does not capture the sink.
3. `sink.finish_frame(output, slot, &mut record)` borrows the sink once; the
   sink looks up the BO / source and DRM handles itself (no more simultaneous
   field borrows in scene), runs the sequence, and passes its own target to
   the closure. The target borrow ends when the closure returns, inside
   `finish_frame`.
4. `FinishOutcome` returns what scene needs afterwards: `Ok(KmsFlipPending)`
   / `Ok(RenderCompletion(fd))` / `Err`, the previous GPU render ns
   (`bo.last_gpu_render_ns.take()`), and the copied fail-stop flag
   (`copied_prepare_failed`). `gpu_submitted` stays scene-owned, set by the
   closure as today.

There is no separate `begin_frame`: nothing between acquisition and the
record needs the target itself, only its image handle (`slot_image`).

#### 2b. Post-compose preparation lives inside the target

`ComposeRenderTarget::post_compose_preparation` returns
`PostComposePreparation`, which names the KMS type
`CopiedTransportPreparation` (`scene.rs:1155`). Today it is validated before
the command buffer begins recording (`compose.rs:510` → `begin_command_buffer`
at 566), so a failed preparation never leaves a command buffer in the
recording state. The trait becomes:
- `fn prepare_post_compose(&mut self) -> Result<(), PresentError>`, called
  where `post_compose_preparation` is called today (before
  `begin_command_buffer`); the copied target stores its
  `CopiedTransportPreparation` in itself, the shared and damage-audit targets
  do nothing;
- `fn record_post_compose(&mut self, vk, command_buffer)`, which consumes the
  stored preparation (copied) or records the shared barrier, as today.

`PostComposePreparation` disappears; the validation-before-recording order is
unchanged.

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
Scene reaches it only through `sink.cursor_plane()`, so there is one sink
borrow, never two trait objects over the same `PlatformBackend`.

Of those methods only `show` returns a KMS type today:
`cursor_plane_show_on_crtc` (`platform/cursor.rs:461`) returns
`kms::cursor_plane::CursorShowError` (`Unbound` / `StillVisible`); the others
return `io::Result`, `bool` or `Option<u64>`, which cross as they are. Scene
uses exactly two things from the show error (`scene/cursor.rs:517–527`):
`remains_visible()` to choose `VisibleNeedsShowRetry` over `Hidden`, and its
`Display` for the warning. So render defines
`CursorShowFailure { remains_visible: bool, error: Box<dyn std::error::Error + Send + Sync> }`
with `Display` delegating to `error`, and the sink converts
(`remains_visible: e.remains_visible(), error: Box::new(e)`). The log text is
unchanged; `CursorShowError` stays in `yserver`.
The NVIDIA cursor investigation (issue #241, from 2026-10-13 evening) is
expected to change the cursor path. The legacy `MoveCursor` calls are outside
scene (`pointer.rs:647, 673`, `scanout.rs:1789`) and untouched by 3b; if the
NVIDIA work changes the retire-time show/hide/upload in `scene/cursor.rs`, the
3b step that wraps it lands after that.

### 5. Order of work

1. **Prep commits in `yserver`** (scene still in place, each gated):
   the post-compose preparation moved into the targets (decision 2b); the unused
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

## Execution

Squash-merged (planned, as 3a): the hashes below are branch commits and will
not exist on master; no blame-ignore entries.

Branch `refactor/phase3b-scene-split`, 2026-10-13.

### Commits by stage

- Stage 1, prep:
  - `372de98d` copied post-compose preparation kept in the target (2b).
  - `67b4a8f2` unused parameters of `queue_steady_state_cursor_upload` dropped.
  - `96ec1e7b` core state and output geometry read from a `SceneInput`.
- Stage 2, the seam:
  - `a6b84904` `OutputSink` / `CursorPlaneSink` defined, `PlatformBackend` implements both.
  - `343c7733` GPU, frame slots and flip retirement through the sink.
  - `da18be67` copied-route render completions through the sink.
  - `1a9e279e` cursor plane through the sink (`CursorShowFailure`).
  - `eaf47374` frame sequence in the sink's `finish_frame`.
  - `181a989a` scene takes `&mut dyn OutputSink`.
  - `9cac04bb` scene tests on a test sink.
  - `1ea91930` scene comments name the sink, not the platform.
- Stage 3, the move:
  - `c8701fb8` protocol paths instead of `yserver-core` re-exports.
  - `72bae306` `WindowGeometry` / `WindowsMap` → `kms::render::window_geometry`.
  - `41e5ee52` the move.
  - `3934db42` nine scene-test-only widenings narrowed back.
  - `9092ee16` log-target hints in scene comments.
  - this docs commit.

### Deviations from the plan

- Stage 1:
  - `IntermediatePrimeTarget` (transform priming) is a fourth
    `ComposeRenderTarget` implementor; it gets the 2b split too.
  - `queue_steady_state_cursor_upload` had 8 unused parameters (the platform
    borrow plus 7), not 7.
  - `SceneInput` geometry is copied per call
    (`PlatformBackend::scene_output_layouts()` → `Vec<OutputLayout>`): the
    platform is borrowed `&mut` as the sink during the same call, so the
    geometry can't be borrowed from it.
  - `build_scene` keeps the cursor-plane `fits` query as a sink call, so the
    59 `build_scene` tests use a test sink. A snapshot of `fits` would change
    the semantics mid-tick.
- Stage 2, the sink as implemented vs the plan table:
  - Accessors come in pairs: `gpu`/`gpu_mut`, `cursor_plane`/`cursor_plane_mut`.
  - Added `slot_count`.
  - Added `check_frame_target`, so the device and pool errors keep their
    place before acquisition.
  - `recycle_failed_slots` is per slot (`recycle_failed_slot`).
  - `compose_complete` is derived from the `ComposeSubmit` in `FinishOutcome`
    (`FrameHandoff::FlipPending`), not set by the closure.
  - The sink resolves render completions to an output index at drain time.
  - `CopiedRenderSubmitError` stays KMS-side in `platform/scene_sink.rs`.
  - `TestSink`'s answers were checked against a fresh
    `PlatformBackend::for_tests()`.
- Stage 3:
  - Scene reached `yserver-core`, which render must not depend on.
    `CrtcTransform` and `GcFunction::Xor` now use their `yserver-protocol`
    paths. The three COW tests use a test-local `COMPOSITE_OVERLAY_WINDOW = 0x103`
    (the value of the core constant; scene sees it only as `cow_host_xid`).
  - The `test-support` cfg also covers the readers of the scene's two test
    fields (`flip.rs`, `transform.rs`, `root_readback.rs`), so those branches
    are compiled into `yserver`'s dev builds. They are inert unless a test sets
    the field (3a's `THREAD_ALLOC_CALLS` precedent).
  - 142 scene tests moved, not 143: `eaf47374` moved
    `copied_renderer_acquire_failure_is_fail_stop_before_submit` to
    `platform/scene_sink.rs` along with `CopiedRenderSubmitError`.

### Numbers

- Visibility (compiler-driven, to `pub`): 22 types/traits, 2 free fns,
  36 methods/assoc fns, 50 fields, 2 glob re-exports (`build::*`, `sink::*`).
  `test-support`: the 5 `for_tests.rs` hooks `yserver`'s tests call (with
  helpers and imports), the `test_flip_in_flight_override` /
  `test_prime_descriptor_sets` fields, their initialisers and readers.
- Move diff: 12 of 24 files are byte-identical. Changed lines: visibility 109,
  test-support cfg 35, rustfmt reflow of widened signatures 14 → 3, other 0.
- Tests: 142 (1 ignored, `walk_bench`) moved from the `yserver` lib to
  `yserver_render`. For each config (default, xdmcp, tcp-transport), normal
  and `--ignored`, the union of test names equals the pre-move list.
- 3a's test-only widenings:
  - 9 reverted to `pub(crate)`: `Drawable::dormant`,
    `Region::{contains, contains_rect, rect_count, union_with}`,
    `ScenePresence::{region, signature}`,
    `ScanoutDamage::{has_staged_frame, missing_area}`.
  - 14 kept, follow-up. All are under `crates/yserver/src/kms/`:
    - `Drawable::{presentation_damage, presentation_damage_epoch}`:
      `render/backend/tests/paint_target.rs`.
    - `DrawableStore::damage`: `render/backend/tests/{deadlines,paint_target}.rs`.
    - `DrawableStore::mark_contents_modified`: `render/backend/tests/gc_draw.rs`.
    - `Bucket::{frame_builder_close_reason_legacy_sc_compose,
      frame_builder_close_reason_timeout}`: `render/backend/tests/deadlines.rs`.
    - `Bucket::frame_builder_renders_per_frame_max_in_window`:
      `render/backend/tests/misc.rs`.
    - `Telemetry::frame_id`: `render/backend/tests/deadlines.rs`.
    - `RenderEngine::try_append_render_batch`:
      `render/backend/tests/render_picture.rs`.
    - `bresenham_segment`: `render/backend/tests/gc_draw.rs`.
    - `color_roles_to_bgra`: `render/backend/tests/cursor.rs`.
    - `reduce_argb32_glyph_to_a8_coverage`: `backend.rs` (cfg(test)).
    - `vk::target::{pick_export_memory_type, ExportMemoryPlacement}`:
      `vk/scanout/copied/two_gpu_tests.rs`.
- Lavapipe (`--workspace --features xdmcp --ignored`): 321. `yserver` lib 58,
  `render_acceptance` 191, `glx_tfp_export` 10, `dri3_fd_leak` 1,
  `yserver_render` lib 59, doctests 2 (`walk_bench` moved crates).

### Acceptance (3b)

- [x] `cargo tree -p yserver-render` (also `-e normal`): no `drm`, `gbm`,
  `input`, `udev`, `yserver-core`. Render code names no `PlatformBackend`,
  `ScanoutBo` or `crate::drm`; comments only.
- [x] Per-commit gates: fmt, clippy ×3 configs, workspace tests default +
  xdmcp; render alone (clippy `--all-targets`, `cargo test -p yserver-render`).
- [x] Test mapping (stage 1 and the move).
- [x] File-size ceilings: no non-test `.rs` over 5,000 lines, no test `.rs`
  over 6,000. No new `target_os` gates.
- [x] Lavapipe after the seam and after the move: 321.
- [x] HW smoke of the seam (`1ea91930`), jos 2026-10-13: silence (XFCE with
  Warframe and mpv; CDE) and eiger (Plasma).
- [ ] FreeBSD CI: runs on the PR (not pushed yet).
- [x] The 9 scene-only widenings reverted.

## Review log

**Round 1 (codex, 2026-10-13):** direction accepted; 3a re-checked (clippy and
workspace tests pass, no regression found). Three interface gaps fixed in
decision 2: the route query comes before acquisition and `SlotToken` carries
all of today's token fields; the copied post-compose preparation is stored in
the target with validation still before command-buffer recording (2b); the
borrow sequence is spelled out, `begin_frame` is dropped, and the cursor
trait is reached through the single sink borrow (2a, 4).

**Round 2 (codex, 2026-10-13):** round-1 fixes confirmed. One gap fixed in
decision 4: the cursor show error crosses as a render-owned
`CursorShowFailure` (`remains_visible` + boxed error), converted in the sink,
so scene never names `CursorShowError`. No other blocker.

## Questions for the review

1. `SceneInput` as data plus `OutputSink` for slots/flips, rather than one
   trait carrying geometry too?
2. `finish_frame` taking the record as a closure (2a), or a `prepare_flip` /
   `submit_flip` pair?
3. `sink.gpu()` for the GPU latch, rather than splitting `PlatformBackend`?
4. Cursor plane as a separate pass-through trait, sequenced after the NVIDIA
   cursor work if that touches `scene/cursor.rs`?
5. Anything in the ordering (acquire → plan → fence → record → export → flip;
   retire: BO phases, ack, pool-slot release, buffer age, cursor) the trait
   shape could break?
