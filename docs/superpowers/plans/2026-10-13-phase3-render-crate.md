# Phase 3: the `yserver-render` crate

**Branch:** `plan/phase3-render-crate` (plan only). Parent plan:
`2026-10-08-source-layout-cleanup.md`, section "Phase 3". This file replaces
that section's inventory and settles its open choices; it needs a codex review
before any code moves (parent decision 4).

## What changed since the parent plan

1. **A concrete second consumer.** jos (2026-10-13): the future nested backend
   is macOS on **KosmicKrisp** (Mesa's Vulkan-on-Metal driver), not MoltenVK.
   Measured on an M2 (Homebrew Mesa): Vulkan 1.4.354, `logicOp = true`, no
   `VK_KHR_portability_subset`, `VK_EXT_metal_surface` present. Our device
   requirements already fit; no logic-op fallback is needed.
2. **The inventory was re-run** on master `7ffee2f7` (phase-2 module trees,
   explicit imports). Script and full tables: `target/phase3-inventory/`
   (gitignored); the numbers below are quoted from it.
3. **A fresh compile-time baseline** on silence with the parent's recipe.

## Measured baseline (master `7ffee2f7`, silence, 2026-10-13)

Same recipe as the parent (sccache bypassed, `CARGO_INCREMENTAL=0`, only the
three workspace crates rebuilt, start+duration in s from `--timings`, two runs
within 2%).

| profile | cpus | total | core | yserver | core lib(test) | yserver lib(test) |
|---|---|---|---|---|---|---|
| release | 32 | 19.5 | 0.6+8.5 | 3.9+15.2 | – | – |
| release | 4 | 36.2 | 0.6+21.1 | 4.2+31.7 | – | – |
| dev | 4 | 10.0 | 0.5+4.7 | 3.0+6.9 | – | – |
| test --no-run | 4 | 22.0 | 0.6+6.3 | 3.7+11.9 | 0.9+15.1 | 6.9+15.1 |

Unchanged from the pre-phase-1 baseline (18.9 s release/32): the `yserver`
unit is still the critical path. fuji is still to be measured (jos).

## Re-run inventory, summary

Two variants: **(a)** render set without scene, **(b)** with scene.

| | files | non-test lines | share of `yserver` | test lines moving |
|---|---|---|---|---|
| (a) | 81 | 38,820 | 31.5% | 14,906 |
| (b) | 103 | 48,396 | 39.3% | 22,914 |

The parent's "(b) about half" was wrong: (b) adds 8 points, not 18.

**Outward dependencies of (a)** (names / uses): `vk_count!` 106;
`platform::{PlatformBackend, FenceTicket, FlushOutcome,
PresentCompletionSignal}` 4 / 93; `yserver_core::backend::params::{GcFunction,
CapStyle, JoinStyle, LineStyle, ArcMode}` 5 / 79; `kms::cpu_types::{Rectangle16,
Repeat, PictTransform}` 3 / 56; `kms::backend` helpers 5 / 22;
`present_completion::{PresentBatchWait, PendingPresentBatch,
PendingPresentEntry}` 3 / 17; `yserver_protocol::x11` 4 / 12 (allowed);
`platform::drm::DrmDeviceKey` 1 / 9; `vk::dri3` 3 / 8;
`yserver_core::randr::transform::{CrtcTransform, Filter}` 2 / 2;
`vk::scanout::BoPhase` 1 / 1. Nothing else crosses.

(b) adds: scene's 86 `PlatformBackend` accesses (GPU 23, output geometry 34,
scanout-BO lifecycle 18, cursor plane 11), the direct DRM flip
(`scene/compose.rs:213, 214, 260`), five more `vk::scanout` types, `KmsCore`
(6 fields, 14 reads), `WindowsMap`/`WindowGeometry` 12, `RegionRect` 6.

**Reverse direction (facade size):** (a) 163 items used from outside non-test
code (89 `pub(crate)`, 73 `pub`, 1 private), plus ~290 render-only methods
called from outside (name heuristic, ~242 `pub(crate)`). Biggest outside users:
`backend` 765 refs, `scene` 366, `vk/scanout` 121, `platform` 104.

**macOS:** no `cfg(target_os)` gate and no Linux-only syscall in (a)'s own
code; `std::os::fd` is `cfg(unix)`; libc has `clock_gettime` for Apple. The
fd/dma-buf extensions are already capability-checked at runtime. (b) is
blocked by the KMS flip and the scanout/cursor-plane types until scene is split.

## Decisions proposed for review

### 1. Two steps: 3a = variant (a), 3b = scene split

**3a** extracts the renderer without scene: the crate seam, `GpuCore`, the
present-completion edge, plain-data moves, facade. Mostly mechanical plus one
seam commit. **3b** splits scene into compose (render) and output driving
(KMS) behind an `OutputSink`, designed in its own document after 3a is
measured.

Why both: the macOS target needs the compositor, so (a) alone does not meet
goal 3. Why not (b) in one go: 3b is an edit-heavy refactor of
`tick_one_output` and friends, and doing it on top of a stable crate seam keeps
each review reviewable and each HW smoke attributable. 3a also gives the
compile-time number the parent asked for before deciding on (b).

### 2. `GpuCore` (boundary A, revised)

`PlatformBackend` gets `gpu: GpuCore`, holding the GPU-only fields
(`platform.rs:530–684`): `vk`, `ops_command_pool`, `fence_pool`,
`pixmap_pool`, `submit_group`, `last_flush_outcome`, `renderer_failed`,
`force_next_submit_failure`, and three the parent missed: `next_submit_cause`
(676), `force_next_frame_record_failure` (679), `queue_submits` (683). The
methods engine/store/frame_builder use move with it (all 14 engine members are
GPU-side, store uses only `vk` and `pixmap_pool`): the submit-group and flush
methods, `submit_paint_cb_with_semaphore` (`platform/submit.rs:57`),
`acquire_present_completion_signal` (`platform/fence.rs:358`),
`acquire_fence_ticket`, `refresh_fence_pool_failure`,
`allocate_drawable_storage*` (`platform/storage_alloc.rs:139, 154`), plus the
types `FencePool`, `FenceTicket`, `FlushOutcome`, `PresentCompletionSignal`.

Lifetime rules (the parent's audit still holds, lines re-checked):
- `scanout_readback_op` (a `ReusableOneShot` whose CB comes from
  `ops_command_pool`, freed into it at `vk/ops/mod.rs:363–369`) is reset
  explicitly before `gpu` drops. `PlatformBackend` now has a `Drop`
  (`platform/init.rs:292`) whose first statement is an early return for a
  fully constructed backend (`!initial_scanout_rollback_armed`), so the reset
  is the **first statement of `drop`, before that guard**, and runs on normal
  shutdown and on construction failure alike. The rollback follows unchanged.
  The deliberate `mem::forget` of the op on a failed readback
  (`backend/kms/readback.rs:795`) stays as is. `GpuCore` itself has no
  knowledge of KMS.
- `FenceTicket`s may outlive `FencePool` (Weak pool + strong `vk`,
  `platform/fence.rs:168`); `KmsBackend` still drops `platform` before
  `store`/`engine`/`scene` (`backend.rs:965–986`). Unchanged.
- Shutdown order (`lib.rs:991` → `session.rs:364/408` → `present.rs:45`) is
  unchanged; this step does not introduce `KmsBackend::teardown`.
- The drop-spy test (`platform/tests.rs:161`) gains a case pinning
  `scanout_readback_op` before the ops pool.
- Signatures change `&mut PlatformBackend` → `&mut GpuCore` in engine, store,
  frame_builder, submit_group, glyph_atlas, cursor_save. KMS callers pass
  `&mut platform.gpu`. Disjoint field borrows replace whole-platform borrows:
  this is the one behaviour-adjacent commit (lavapipe + HW smoke).
- The stale drop-order comment (`platform.rs:609–612`) is replaced by these
  rules in `GpuCore`'s doc.

### 3. `vk/dri3.rs` moves whole (boundary D, changed)

The parent split it (Vulkan half to render, ioctl half to KMS). Re-checked:
the ioctl half (521–749) is `libc::ioctl`/`libc::poll` with the request
numbers already defined for non-Linux targets (550–553, 701–704), so it
compiles on macOS and returns `Unsupported` at runtime (ENOTTY). The engine's
flush calls it through `flush_submit_group_with_exports`
(`platform/submit.rs:207`, ioctl at 270), which moves into `GpuCore`. Splitting
would need a new callback trait for those two calls only, against parent
decision 8. Proposal: move the file whole; revisit only if a target cannot
compile it. `owned_fd_from_vk` (private, `vk/mod.rs:45`, also used by
`vk/scanout/image_alloc.rs`) becomes `pub`.

### 4. Present-completion edge (boundary C)

Engine owns `pending_present_batches` (`engine.rs:771`), pushes at
`engine/frame.rs:921, 1003, 1489`, drains at 1264, and publishes a release
fence into each entry (`frame.rs:912` → `present_completion.rs:42`). The entry
carries core's `CompletedPresentEvent`, syncobj and xshmfence handles, which
must not enter render.

The entry also travels back. `attach_present_completion` hands it back on
`Err`, and the KMS side then tries the next destination with the same entry
(`backend/kms/present.rs:1345–1395`: COW batch, then the paint target, then a
signal-only submit). The drain consumes the concrete `wake_pin` and `event`.
So the payload must be recoverable as the concrete type without render
knowing it.

Proposal:
- Render defines
  `pub trait PresentCompletionEntry: Any + Debug { fn publish_release_fence(&mut self, fence: &OwnedFd) -> io::Result<bool>; }`,
  the only call engine makes on an entry (`engine/frame.rs:912`).
- Engine stores and returns `Box<dyn PresentCompletionEntry>`:
  `attach_present_completion(id, entry) -> Result<(), Box<dyn PresentCompletionEntry>>`,
  and the drained batches carry `Vec<Box<dyn PresentCompletionEntry>>`.
- KMS recovers the concrete payload by trait upcasting to `Box<dyn Any>`
  (stable since Rust 1.86) and `downcast::<PendingPresentEntry>()`, in one
  helper used at both return points (failed attach, drain). There is one
  implementor, so a failed downcast is a bug: `expect` with a message, not a
  silent drop. A unit test pins the round trip: attach to a missing
  destination, get the same entry back, downcast, fields intact.
- Render owns `PresentBatchWait` and the batch container;
  `present_completion.rs` (with `PinnedWake` and core's types) stays in
  `yserver`.
- `Box<dyn>` over generics so engine is not monomorphised inside `yserver`
  (parent's reasoning).

### 5. Plain data moves down

- `GcFunction`, `CapStyle`, `JoinStyle`, `LineStyle`, `ArcMode`
  (`yserver-core/src/backend/params.rs`) and `CrtcTransform`, `Filter`
  (`randr/transform.rs`) → `yserver-protocol`; core re-exports them at the old
  paths. If an impl on them needs core types, the type moves and that impl
  stays in core as a free function (checked in the step, listed in the PR).
- `DrmDeviceKey` → `vk::device`; `platform::drm` re-exports it. Its current
  module wraps the `drm` crate (drm-sys has prebuilt bindings for Linux and
  Android only), which would otherwise pull `drm` into render.
- `kms/cpu_types.rs` and the five `kms/backend.rs` helpers
  (`bresenham_segment`, `scanline_fill_polygon`, `repeat_to_shader_const`,
  `compose_affines`, `pixman_transform_to_affine`) → render; `kms::` keeps
  re-exports.
- `vk::scanout::BoPhase` (used only in compositor's `PresentError::WrongPhase`):
  moves to render next to `PresentError`; `vk::scanout` re-exports it.

### 6. The crate, facade and visibility (boundary E)

- `crates/yserver-render/` with the shader half of `build.rs` (glslc →
  `OUT_DIR/*.spv`) and `src/kms/vk/shaders/`; the git-hash half stays in
  `yserver`. Dependencies: `ash`, `log`, `thiserror`, `libc`,
  `yserver-protocol`; `nix` as a dev-dependency (one Linux-gated test,
  `store.rs:1606`).
- `vk_count!` is `#[macro_export]`ed from render; the 41 calls outside the set
  switch to `yserver_render::vk_count!`.
- Items crossing the edge go from `pub(crate)` to `pub` in private modules,
  with a facade `pub use` list in `lib.rs`. `yserver` keeps
  `pub use yserver_render::…` aliases under `kms::` so its paths compile.
  Counted before/after in the PR; the inventory gives the starting point.
- `render_acceptance` and the other integration tests stay in `yserver`.

### 7. Tests

Rule: no item becomes `pub` only for a test. Tests that span the boundary
are split or re-fixtured in **test-only prep commits before the crate move**,
while everything is still one crate, so the move commit itself carries no
test edits beyond paths.

- **`PlatformBackend` fixtures** (41 uses in (a)): switch to a `GpuCore`
  fixture where the test only uses GPU state; tests that need KMS state stay
  in `yserver`.
- **`engine/tests/glyph_runs.rs:749–1050`** chains the KMS glyph parser
  (`parse_composite_glyph_items`, `ParsedGlyph`, `kms::core::{GlyphSetState,
  GlyphSetFormat, StoredGlyph}`) into private engine functions
  (`RenderEngine::split_glyph_runs`, `effective_glyph_layout`,
  `frame_builder::RecordedTextGlyph`). Split at the seam:
  - a parser test in `yserver` asserting, per parsed glyph, the full tuple
    the renderer consumes today (`recorded_from_parsed`,
    `glyph_runs.rs:827`): source format, `w`, `h`, `dst_x`, `dst_y`. Source
    formats are `A8, A8, Argb32, A8, Argb32, Argb32`; the other values are
    read from today's fixture output when the test is written, then checked
    by hand against the fixture's glyph sizes and one-pixel pen advance;
  - render tests that start from those same literal tuples and assert the
    splitter's ordered runs and the layout answers, as today.
  The two halves pin the same tuples, so a parser regression in format or
  positioning fails the KMS half, and the chain stays covered without making
  the splitter public.
- **`engine/tests/memory_frame.rs:192`** builds a whole `KmsBackend` only to
  read two engine counters; it builds `RenderEngine` on the `GpuCore` fixture
  instead.
- The step starts by re-running the inventory's §1g scan (render-set test
  code naming outside items) and lists every remaining case with its
  resolution; the plan's three are the known ones.

The parent's rule-4 mapping (every test before = one test after, same ignored
status, per feature config) applies; splits and crate-path changes are listed
explicitly in the mapping.

### 8. Findings deferred to the macOS/Windows scoping

Found by the inventory; both only matter on a driver without the Linux
extensions, so they wait for the macOS/Windows work (jos, 2026-10-13):
- `allocate_exportable`'s last fallback (`vk/target.rs:1449` → 1669–1677) and
  `bind_exportable_memory` (1774–1800) request dma-buf export without checking
  `VK_EXT_external_memory_dma_buf` (reached from GLX-TFP promotion,
  `engine/export.rs:79`). Every Linux/FreeBSD driver we run on has the
  extension.
- Required instance extensions are not filtered (`vk/instance.rs:8–14`). The
  loader implements `debug_utils` itself, so it is present wherever a loader is.

`gpu-allocator` (a `yserver` dependency used nowhere) is removed in 3a's
first prep commit.

## Acceptance (3a)

- `cargo tree -p yserver-render` has no `drm`, `gbm`, `input`, `udev`,
  `yserver-core`.
- The parent's gates per commit, all three feature configs, FreeBSD CI, test
  mapping, file-size ceilings.
- Seam commit: lavapipe suite, HW smoke on bee (amdgpu) and eiger (Asahi).
- **Timings:** this file's recipe on silence and fuji after 3a. Target
  (parent): ≥ 20% on fuji's release critical path. If missed, report, do not
  tune without jos. The 3a number decides how 3b is weighed.

## 3b outline (designed separately after 3a)

Scene's 86 platform accesses after 3a: the 23 GPU ones go through `GpuCore`;
the rest (geometry 34, scanout-BO lifecycle 18, cursor plane 11, the flip)
become an `OutputSink` trait sized to those calls. `KmsCore`'s six fields
become a borrowed `SceneInput`; `WindowsMap`/`WindowGeometry` move into
render. Scanout BO types stay KMS-side, behind the sink. A second sink is
**not** built in 3b; an offscreen test sink that lets lavapipe tests compose
without KMS would be the first real second implementor. HW smoke on bee and
eiger.

## Review log

**Round 1 (codex, 2026-10-13):** 3a-then-3b and the whole-file `dri3.rs` move
accepted. Four gaps fixed above: the present entry's return path (decision 4:
`Any` upcast + downcast, round-trip test), the mixed-boundary tests (decision
7: split in prep commits, no test-only `pub`), the `scanout_readback_op` reset
placed before `Drop`'s early return (decision 2), and the cross target install
for the macOS CI check (acceptance).

**Round 2 (codex, 2026-10-13):** round-1 fixes confirmed (codex compiled a
probe of the `Box<dyn PresentCompletionEntry>` → `Box<dyn Any>` downcast on
the current toolchain). One fix: the glyph parser half asserts the full
per-glyph tuple (format, size, position), not only the format sequence
(decision 7). No remaining plan blocker.

**jos, 2026-10-13: no macOS work in this pass.** The macOS acceptance check
(cross `cargo check`, a macOS runner, a native build) is dropped from 3a: the
crate would compile there but nothing would run, so it proves little. The
`cargo tree` check guards the dependency side. macOS stays recorded as the
second target (3b's `OutputSink` is designed with a swapchain in mind);
macOS/Windows scoping, including its CI job, starts after the cleanup.

## Questions for the review

1. Two steps (3a then 3b) rather than (a) or (b) alone?
2. Moving `dri3.rs` whole instead of the parent's split?
3. `Box<dyn PresentCompletionEntry>` for the present edge?
4. `GpuCore` field list and the `scanout_readback_op` reset in
   `PlatformBackend::drop`: complete and correctly ordered?
5. Moving the GC/line/arc enums and `CrtcTransform`/`Filter` into
   `yserver-protocol`: right home, or a small `yserver-types` crate?
6. ~~Cross-`cargo check` for `aarch64-apple-darwin` in CI as the macOS gate.~~ Dropped (jos).
