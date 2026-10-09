# Source layout cleanup: split the oversized files, then the crates

**Branch:** `plan/source-layout-cleanup` (plan only). Execution branches are per
phase. **Revised twice 2026-10-08** after codex reviews (inventory, store,
test identity, item-level move verification, GpuCore lifetimes, FreeBSD gates;
branch porting dropped by jos). Decisions at the end of the file.

**Goal, in priority order:**
1. Maintainability: no hand-edited source file over the size ceilings.
2. Compile parallelism: a shorter crate critical path on slow laptops.
3. A crate seam that lets a future nested backend for macOS/Windows reuse the
   Vulkan renderer.

**Phase order:** 1 test extraction → 2 production modules → 3 render crate
(separately reviewed, only after 1–2 and the inventory below is re-checked) →
4 deferred. No logic changes in any move commit.

## Measured baseline (master `4db91343`, 2026-10-08)

silence (i9-13900K), sccache bypassed, `CARGO_INCREMENTAL=0`, only the three
workspace crates rebuilt. "4 cpu" = `taskset -c 0-3`, a lower bound for fuji
(i5-7200U). start+duration in s, from `cargo build --timings`.

| profile | cpus | total | core | yserver | core lib(test) | yserver lib(test) |
|---|---|---|---|---|---|---|
| release | 32 | 18.9 | 0.5+11.1 | 3.6+15.0 | – | – |
| release | 4 | 35.0 | 0.5+20.1 | 4.0+30.8 | – | – |
| dev | 4 | 9.5 | 0.4+4.5 | 2.8+6.5 | – | – |
| test --no-run | 4 | 20.5 | 0.4+6.1 | 3.5+10.2 | 0.7+15.1 | 6.5+13.9 |

- The critical path is the `yserver` crate (starts ~3–4 s in, then runs alone
  15 s / 31 s). File splits change nothing (the crate is the unit); splitting
  `yserver-core` buys almost nothing. Only phase 3 can move the numbers.
- Inline tests dominate test builds (`process_request.rs` has 54k test lines).

## Inventory (from code, 2026-10-08)

| file | code | inline tests | structure |
|---|---|---|---|
| core `core_loop/process_request.rs` | 35.8k | 54.4k (816 tests) | free `handle_*` fns behind one `match header.opcode` |
| `kms/render/backend.rs` | 31.6k | 37.4k (565) | `KmsBackend`; inherent impl 16.2k; `impl Backend` 10.3k (230 of 247 methods) |
| `kms/render/engine.rs` | 12.9k | 6.4k | `impl RenderEngine` 7.1k, recording `emit_*` 4k |
| `kms/render/scene.rs` | 9.6k | 7.6k | `SceneCompositor`, walk, `tick_one_output` (1,052-line fn), `ComposeRenderTarget` |
| `tests/render_acceptance.rs` | – | 16.9k (186, all `#[ignore]`) | topic blocks, helpers next to their tests |
| `core_loop/pointer_fanout.rs` | 4.4k | 6.0k | `pointer_event_fanout_to_state_inner` is 2,009 lines |
| `core_loop/run.rs` | 4.8k | 4.7k | `run_core_with_inventory` ~990 lines |
| `kms/render/platform.rs` | 7.0k | 2.2k | `PlatformBackend` (platform.rs:2219): DRM, outputs, fences, pools, scanout, cursor plane |
| `kms/vk/scanout.rs` | 6.6k | 2.0k | GBM/dma-buf scanout BOs, copied PRIME pool, probes |
| protocol `x11/mod.rs`, core `server.rs`, core `resources.rs` | 4.9k / 4.4k / 3.9k | 2.8k / 3.1k / 3.2k | flat encoders; ~25 state types; `impl ResourceTable` |

Core is `dyn Backend` everywhere (225 uses, no `<B: Backend>`), so no split
risks monomorphisation cost moving across crates.

**Bodies that only shrink if edited:** `handle_xi2_request` 5,140,
`handle_randr_request` 2,277, `pointer_event_fanout_to_state_inner` 2,009,
`tick_one_output` 1,052, `run_core_with_inventory` ~990. They move whole in
phase 2; XI2/RANDR are split later (phase 2c, decided).

## Rules for every commit

**1. One file, one kind of change.** Subject says which: **move** (whole items
only — fn, impl, struct/enum, mod, const, macro, with attributes and doc
comments), **visibility** (`pub(super)`/`pub(in path)` and `use` lines only),
**delegate** (trait body → `Self::backend_<subsystem>_<name>(self, ..)`), or **seam** (a real
small logic change, own review, HW smoke).

**2. No widened visibility in phases 1–2.** `foo.rs` becomes `foo/mod.rs` +
children; children see ancestors' privates. Only sibling→sibling and
mod.rs→child calls need `pub(super)`. mod.rs does `use child::*;` so existing
paths and `use super::*` test modules compile unchanged. A line grep for added
`pub` is wrong (every relocated public item is an added line). Check by item
identity instead: `split verify` (rule 3) compares each mapped leaf's old
visibility with its new one; a change is allowed only to `pub(super)` /
`pub(in <path>)` and only if the manifest lists it. New `pub use` /
`pub(crate) use` re-exports are allowed only from the commit's manifest
allowlist (`reexports = [...]`); any other visibility change fails.

**3. Move verification (item level, not line level).** A line multiset diff is
kept only as a cheap smoke check: it cannot see reordered statements inside a
body or a line that ended up in the wrong fn. The proof is item-based:
- `tools/split/` is a small Rust tool using `syn` (`span-locations`). It parses
  the source file, extracts each item by span (attributes + doc comments
  included), and writes items to target files per the manifest. It never edits
  item text; the only text it emits itself is `mod`/`use` lines and
  `pub(super)` insertions listed in the manifest.
- `split verify <rev>` parses every `.rs` the commit touches (following
  `mod x;` to its file), before and after, and compares **leaf items**.
  Normalization contract:
  - *Leaves:* `fn`, `const`, `static`, `type`, `struct`, `enum`, `union`,
    `trait`, `macro_rules!`/macro defs, and each impl-associated item (fn,
    const, type) individually. `mod` and `impl` are *wrappers*, not leaves, so
    inline `mod x { }` → `mod x;` + file and one `impl` → several `impl`s with
    the same header are both no-ops for the leaf set. `use` items are not
    leaves (generated imports may differ); see residual risk below.
  - *Key:* new path (via the move script's explicit **old path → new path
    table**, e.g. `backend::KmsBackend::foo` → `backend::portable::draw::
    KmsBackend::foo`), kind, name, and for associated items the impl header
    (generics, trait, self type, where clause) as tokens.
  - *Hash:* the leaf's tokens **including its own attributes, derives and doc
    comments**, visibility removed (checked separately, rule 2). Whitespace is
    not significant (token stream). **Plain comments are part of the item and
    must be preserved:** the hash also covers the leaf's source text with
    whitespace runs collapsed, so a dropped `//` comment fails.
  - *Effective cfg:* each leaf's cfg set = its own `#[cfg]`s ∪ every enclosing
    `mod`/`impl` `#[cfg]` (incl. `#[cfg(test)]` on `mod tests` and on the
    `mod tests;` declaration). Before == after per leaf, so a `#[cfg(test)]`
    or feature gate lost when an inline mod or impl wrapper moves fails.
    Other wrapper attributes (`#[allow]`, `#[path]`, `#[expect]`) are compared
    per wrapper: each new wrapper must carry its source wrapper's set.
  - The mapped keyed multisets must be equal: nothing lost, duplicated, or
    changed.
- Delegate commits (2.11b) use `split verify --delegate`: every changed trait
  method must be one forwarding call with the same arguments in the same order,
  and the moved body must hash-equal the old trait body. The helper must be
  named `<prefix><subsystem>_<method>` (`--delegate backend_`) and must not
  occur as an identifier anywhere in the pre-change repo: a new inherent fn
  wins method resolution over a same-named trait method.
- Then the gate (rule 5).

**Residual risk:** name resolution can change without a token change (a glob
`use child::*` shadowed by a same-named local item; a `macro_rules!` now after
its use). Ambiguities are compile errors, silent shadowing is not, so `split
verify` lists every name defined in more than one module of the moved tree for
review. `include_*!`/`#[path]` breakage is loud.

**4. Test identity: an explicit old→new name mapping.** Test names change when
`mod tests` gains topic submodules (`…::tests::foo` → `…::tests::xi2::foo`).
Per commit and per feature config (default, `tcp-transport`, `xdmcp`):
`cargo test --all-targets $F -- --list` and `-- --ignored --list`, normalised
to `binary::path::name`, before and after; `split verify --tests` applies the
manifest's path table to the before-lists, which must then equal the
after-lists exactly (count per binary, names, ignored status). Integration
tests (`render_acceptance` → modules) use the same mapping.

**5. Gate per commit** (copied from `.github/workflows/ci.yml:54–135`; `F` =
`""`, `--features tcp-transport`, `--features xdmcp`):
`cargo +nightly fmt -- --check`; per `F`: `cargo clippy --all-targets $F --
-D warnings` and `cargo test --all-targets $F --locked`; lavapipe, exactly
CI's step plus the local ICD pin:
`VK_DRIVER_FILES=/usr/share/vulkan/icd.d/lvp_icd.json
YSERVER_ALLOW_SOFTWARE_VULKAN=1 cargo test -p yserver --locked --features
xdmcp --no-fail-fast -- --ignored`; rules 2–4. "lavapipe" below means this
command.
Gate runs keep their complete output (`… 2>&1 | tee target/gate-<step>.log`)
so a failure's test name is never lost to a truncated tail.

**6. Blame.** Move SHAs go into `.git-blame-ignore-revs` in a follow-up commit;
cross-file moves still need `git blame -C -C` (one line in the file header).

**7. Scripted, deterministic splits.** Manifest = TOML `item key → target file`
per source file. Unassigned items make the tool fail with the list, never
default somewhere. Review reads the manifest; the commit is tool output plus
`cargo +nightly fmt`.

## Phase 1: extract inline tests (~125k lines, zero visibility change)

Two commits per file:
- **1a (move):** `mod tests { … }` → `#[cfg(test)] mod tests;` + `foo/tests/mod.rs`
  (or `#[path]` to `foo_tests/` while `foo.rs` is still one file). Body
  byte-identical; test names unchanged (rule 4 map is the identity).
- **1b (move):** `tests/mod.rs` keeps fixtures and `mod <topic>;`; each topic
  file starts `use super::*;`. Names gain the topic segment (rule 4 map).

| file | tests/ topics (≤ ~6k each) |
|---|---|
| process_request (54.4k) | `xi2_config`, `xi2_dynamic`, `xi2_grabs_allow`, `xi1`, `randr`, `present`, `present_supersede`, `composite_redirect`, `sync`, `window`, `copy_area`, `glx`, `saver_dpms`, `shm_cow`, `misc` — by test-name prefix, not line range |
| backend.rs (37.4k) | `fixtures`, `xi_source`, `xi_dynamic`, `keyboard`, `randr_provider_gamma`, `render_composite_clip`, `present_scanout_direct`, `dri3_resolve_paint` |
| engine (6.4k) / scene (7.6k) | engine's existing nested mods; scene `walk`, `damage_audit`, `cursor`, `compose`, `test_helpers` |
| pointer_fanout, run, server, resources, platform, vk/scanout, x11/mod | 2–4 files each |

`render_acceptance.rs` becomes `tests/render_acceptance/{main.rs, common.rs, <14
topic files>}`: **one binary with modules**, not 14 binaries (each binary links
all of `yserver` with debuginfo). CI's lavapipe step needs no change.

## Phase 2: split production code (moves)

Least-contended files first.

| step | file → dir | target modules (≈ code lines) |
|---|---|---|
| 2.1 | protocol `x11/mod.rs` | `types`, `setup`, `request_parsers` 1.0k, `events_core`, `events_input`, `replies_core` 1.2k, `xi`, `xkb_events`, `render`; mod.rs `pub use` |
| 2.2 | core `resources.rs` | `types`, `window` 1.3k, `tree`, `gc`, `pixmap_props`, `picture_font_cursor`, `cleanup` (`impl ResourceTable` spread) |
| 2.3 | core `server.rs` | `types_grabs`, `types_ext`, `xi_registry`, `idle`, `hit_test`, `fanout` |
| 2.4 | `core_loop/run.rs` | `telemetry`, `queues`, `xi_config`, `requests`, `present_tail`, `randr_notify`, `accept`, `repeat` |
| 2.5 | `core_loop/pointer_fanout.rs` | `fanout_inner` (the 2k fn), `xi1`, `grabs`, `xi2_targets` |
| 2.6 | `process_request.rs` → `core_loop/process_request/` | tree below |
| 2.7 | `kms/vk/scanout.rs` | `bo`, `bo_pool`, `copied` 2.0k, `dmabuf_metadata`, `probe` 1.5k, `alloc_plan` 1.3k, `image_alloc` |
| 2.8 | `kms/render/platform.rs` | `fence`, `cursor_plane` 1.8k, `qualify` 1.5k, `device`, `init`, `scanout`, `connectors`, `submit`, `page_flip` |
| 2.9 | `kms/render/scene.rs` | `cursor`, `damage_audit` 2.0k, `tick_output` 1.8k, `walk` 2.6k, `fan_out`, `targets`, `root_readback` |
| 2.10 | `kms/render/engine.rs` | `types`, `staging`, `frame`, `clip_snapshot`, `gradient_assets`, `fill_copy`, `put_get`, `text`, `glyphs`, `composite`, `traps`, `batch`, `rollback`, `record/*` |
| 2.11 | `kms/render/backend.rs` | tree below |

Split by responsibility; the line figures are ceilings to respect, not targets.

**2.6 `core_loop/process_request/`** (a descendant of the old module, so
`core_loop::process_request` log-target filters still match; final tree in
`2026-10-09-phase2-process-request.md`): `process_request.rs` keeps the
consts, `RequestOutcome`, `process_request` + opcode match and the shared
helpers (`emit_x11_error*`, lookups, `validate_*`, `write_to_client`); core
`windows`, `redirect`, `props`, `selection`, `drawing`, `gc_pixmap_cursor`,
`fonts`, `colormaps`, `grabs`, `focus_pointer`, `input_ctl`, `misc`;
extensions `render`, `randr_ext`, `sync_ext`, `present_ext`, `dri3`, `glx`,
`shape_xfixes`, `composite_damage`, `xshm`, `xtest`, `vidmode`, `saver_dpms`,
`xkb`, `xi/{mod, dispatch}` (`_ext` only where the bare name is a protocol
module the code imports). The ~40 handlers that are `pub(crate)` today stay
so and are re-exported. Interleaved regions (DPMS/saver, XI1/XI2
AllowEvents, two GLX chunks, XTEST/cursor) are regrouped by the move.

**2.11 `kms/render/backend/`** (descendants of the old module, so
`kms::render::backend` log-target filters still match; final tree in
`2026-10-09-phase2-backend.md`):
```
backend.rs        imports, all types, helper-type impls, test mod decls
backend/
  trait_impl.rs   impl Backend for KmsBackend (9.9k after 2.11a; 2.0k delegators after 2.11b)
  portable/       windows, redirect, paint_target, render_ops, text, inferiors,
                  clip, draw, keyboard, devices, pointer, stats, dump
                  (no drm/gbm/fd/libc imports; pointer's cursor-plane calls: phase 3)
  kms/            scanout, readback, crtc_config, randr, present, export, session,
                  cursor
  for_tests.rs    *_for_tests helpers;  tests/ (phase 1)
```
- **2.11a (move):** inherent impl and free fns into the tree. The 9.9k
  `trait_impl.rs` is accepted temporarily (decision 1).
- **2.11b (delegate), its own commits grouped by subsystem:** each trait body
  moves to an inherent `backend_<subsystem>_<name>` in its group file;
  inherent-side renames for collisions (`fb_dimensions`, `randr_outputs_and_modes`,
  `randr_providers`, `acquire/release_glx_pixmap_export`,
  `vt_switching_armed`, `promote_pixmap_exportable`, `present_get_ust_msc`).
  Verified by `split verify --delegate backend_`.
- CI grep after 2.11: `backend/portable/**` has no `crate::drm`, `gbm`,
  `OwnedFd|RawFd`, `libc::`, `nix::`, `platform::`, `backend::kms::`.

**Phase 2c (decided: yes, later, separate refactor):** split the arms of
`handle_xi2_request` / `handle_randr_request` into per-minor fns, `xi2/` into
`{events, devices, grabs, props, focus}`. Reviewed as a refactor (bodies move
per arm, not byte-pure), HW XI smoke on eiger. Not part of the move series.

## Phase 3: `yserver-render` crate (separate review, after phases 1–2)

The first draft treated this as "one `PresentTarget` seam". It is not. Counts
below are from non-test code of the would-be render set — `kms/vk/*` minus
`scanout.rs`/`dri3.rs`, `vk/ops/*`, and `kms/render/{engine, scene, store,
frame_builder, upload_arena, glyph_atlas, glyph_pixels, region, stroke,
scene_diff, descriptor_pool_ring, batch_resource, transform_intermediate,
root_overlay, cursor, cursor_save, submit_group, submit_trace, telemetry,
scanout_damage, composite_pool_ring, target, owned_semaphore}` — by resolving
`use` trees and counting each imported name's uses (token counts; re-run the
script before phase 3 starts).

### Dependency inventory (what render would import from outside itself)

| from | names | uses | where | boundary needed |
|---|---|---|---|---|
| `platform::{PlatformBackend, FenceTicket, FlushOutcome, PresentCompletionSignal, ReadyScanoutRenderCompletion}` | 5 | 152 | engine 78, scene 37, store 15, frame_builder 11, submit_group 6, glyph_atlas 3, cursor_save 2 | `GpuCore` (below) + scene output seam |
| `platform.<member>` calls/fields | 30 distinct | 161 | engine 59, scene 96, store 5, frame_builder 1 | same |
| `vk::scanout::{OutputScanout, ScanoutBo, BoPhase, BoState, CopiedRenderSource, CopiedTransportPreparation}` | 6 | 16 | scene 14 (scene.rs:96–99, 4896–4982, 8371, 8447, 8599–8689), `vk/compositor.rs:13`, `vk/target.rs` 1 | scene output seam; `BoPhase` used by compositor's error type |
| `kms::core::KmsCore` | 1 type, 6 fields | 12 type refs, 15 field reads | scene.rs:89, 2103–9277 (`top_level_order`, `shape_bounding`, `shape_clip`, `window_id`, `cursor_x/y`) | a read-only scene-input view |
| `render::backend::{WindowsMap, WindowGeometry}` | 2 | 12 | scene | move the types into render |
| `render::present_completion::{PendingPresentBatch, PendingPresentEntry, PresentBatchWait}` | 3 | 19 | engine 18 (engine.rs:51, 1334, 2763–3048, 5070), frame_builder 1 | present-completion seam; the module itself imports `yserver_core::backend::{CompletedPresentEvent, SyncobjHandle, XshmfenceHandle}` and `OwnedFd` |
| `vk::dri3` | 4 | 13 | `vk/target.rs` (414, 1261, 1408–1516) | split dri3.rs (below) |
| `crate::drm` | 2 | 2 | scene.rs:8597, 8644 (`submit_flip_with_fences`) | scene output seam |
| `crate::platform::drm::{DrmDeviceKey, Output}` | 2 | 10 | `vk/device.rs:17` 9, scene 1 | `DrmDeviceKey` is a plain `(major, minor)`: move down |
| `kms::backend` helpers (`scanline_fill_polygon`, `repeat_to_shader_const`, `pixman_transform_to_affine`, `bresenham_segment`, `compose_affines`) | 5 | 25 | engine 14, stroke 11 | move into render |
| `kms::cpu_types::{Repeat, Rectangle16, PictTransform}` | 3 | 70 | engine 55, stroke 9, target 5, frame_builder 1 | move into render (missed by the first draft) |
| `yserver_core::backend::{GcFunction, params::{ArcMode, CapStyle, JoinStyle, LineStyle}}`, `yserver_core::randr::{CrtcTransform, Filter}` | 7 | ~100 | stroke 42, `vk/logic_fill_pipeline` 38, engine 8, scene 5, root_overlay 4, transform_intermediate 2 | plain data: move to `yserver-protocol`, core re-exports |
| `crate::vk_count!` | 1 macro | 132 | 16 files | `#[macro_export]` from render |

### Boundaries, per group

**A. `GpuCore` — the pooling and resource-lifetime boundary (engine, store,
frame_builder, submit_group).** Today `PlatformBackend` owns the GPU-only state
next to the DRM state (platform.rs:2294–2316, 2351–2362): `vk`,
`ops_command_pool`, `fence_pool`, `pixmap_pool`, `submit_group`,
`last_flush_outcome`, `renderer_failed`, `force_next_submit_failure`. Engine's
59 platform calls are all on this subset (`renderer_failed` 24,
`submit_group_ticket_or_open` 13, `ops_command_pool_handle` 8,
flush/submit-group 10, `vk` 2, `allocate_drawable_storage` 1,
`acquire_present_completion_signal` 1, …). Store needs more than a
`&VkContext`: `Storage::destroy` (store.rs:391) returns images to
`platform.pixmap_pool` (store.rs:446), and `decref` (1101),
`destroy_now` (1172), `poll_pending_retire` (1453) and `shutdown_destroy_all`
(1038) take `PlatformBackend` to poll `FenceTicket`s and destroy.
- **Ownership audit (2026-10-08).** Declared order (platform.rs:2294–2351):
  `vk` → `scanout_readback_op` → `ops_command_pool` → `fence_pool` →
  `scanout_readback` → `pixmap_pool` → `copy_vk_contexts` → `scanout_pools` →
  `submit_group`. `vk` drops *first*: the comment at platform.rs:2298–2301
  ("ops pool BEFORE fence pool BEFORE vk, handled by field order") is
  **stale** (separate one-line comment fix, not part of this plan). It is
  harmless because every Vulkan owner holds its own `Arc<VkContext>`
  (`OpsCommandPool`, `ReusableOneShot`, `FencePoolInner`, `PixmapPool`,
  `StagingBuffer`, `FenceTicketInner`, engine `inner.vk`, scanout and copy
  pools), and `VkContext::drop` (vk/device.rs:897) idles and destroys the
  device at the last `Arc`. The real dependencies:
  1. `scanout_readback_op` before `ops_command_pool`: its CB comes from that
     pool (backend.rs:19299, 19494) and is freed into it (vk/ops/mod.rs:353).
  2. `FenceTicket`s outlive `FencePool`: `KmsBackend` drops `platform` before
     `store`/`engine`/`scene` (backend.rs:1401); a ticket keeps a `Weak` pool
     plus a strong `vk` and destroys its own fence (platform.rs:289).
  3. `OpsCommandPool::drop` idles the queue, then destroys the pool (freeing
     engine CBs); `RenderEngine::drop` (engine.rs:9566) never touches it.
  4. Shutdown (lib.rs:981 → backend.rs `disable_output`): `flush_render_batch`
     → `engine.shutdown` (close frame, `drain_all`) → `scene.drain_all` →
     present drains → `platform.disable_output` (`wait_idle_bounded`,
     `pixmap_pool.drain()`, modeset off) → `store.shutdown_destroy_all`
     (lib.rs:1014). Device loss latches `renderer_failed`; `FencePool` keeps
     unsignalled fences in `leaked_fences` until its drop.
  5. KMS-side Vulkan objects (`scanout_pools`, `copy_vk_contexts`, cursor
     save, `scanout_readback`) stay in `PlatformBackend` with their own `Arc`s.
- **Boundary = lifetime rules, not field order.** `GpuCore` holds `vk`,
  `ops_command_pool`, `fence_pool`, `pixmap_pool`, `submit_group`,
  `last_flush_outcome`, `renderer_failed`, `force_next_submit_failure`.
  `scanout_readback_op` is reset by an explicit `PlatformBackend::drop` step
  before `gpu` drops. Anything allocated from a `GpuCore` pool is released
  before that pool, or holds an `Arc` keeping its parent alive. Order (4)
  becomes one `KmsBackend::teardown` ending in `GpuCore::drain_and_idle`. A
  drop-spy unit test (like platform.rs:7148) pins (1)–(4).
  `PlatformBackend` owns `gpu: GpuCore`.
- `FencePool`/`FenceTicket`, `PresentCompletionSignal`, the submit group and
  `allocate_drawable_storage*` (platform.rs:4617–4750, which takes from the pool)
  move with it. Allocation and return of pooled images then live in one place.
- Store/engine/frame_builder signatures change `&mut PlatformBackend` →
  `&mut GpuCore`; KMS callers pass `&mut platform.gpu`. Disjoint field borrows
  replace the whole-platform borrow, which is the one behaviour-adjacent risk:
  a **seam** commit, lavapipe + HW smoke.
- `flush_submit_group_with_exports` (platform.rs:4959) runs dma-buf sync-file
  ioctls (via `dri3::export_dmabuf_write_access_sync_file`). Its ioctl half
  stays KMS-side; `GpuCore::flush` takes already-imported wait semaphores and
  an optional export signal.

**B. Scene output seam (scene ↔ KMS output driving).** Of scene's 96
`platform.*` uses only 25 are `GpuCore` (`renderer_failed` 16,
`acquire_fence_ticket` 6, …). The rest drive outputs: geometry 39 (`outputs`,
`output_root_rect`, `output_transform`, `fb_w/h`, …), scanout-BO lifecycle 21
(`scanout_pools`, `acquire_scanout_bo`, `invalidate_bo`, `commit_bo_present`,
`on_page_flip_complete`, render-completion register/drain, …), cursor plane 11.
Plus the direct DRM flip and the scanout types. `tick_one_output` is a KMS
output driver, not a renderer. Phase-3 options, to be decided in the phase-3
review with the re-run inventory:
- **(a, default)** scene stays in `yserver`; render gets vk + engine + store +
  frame_builder + the small modules. Smaller gain, no new trait.
- **(b)** split scene into compose (walk, damage, `ComposeRenderTarget`
  impls for plain images) in render and output driving in KMS, behind a narrow
  `OutputSink` trait sized to the calls listed above. Only if (a)'s measured
  gain is too small; it is an edit-heavy refactor with HW smoke on bee and
  eiger.
`KmsCore` reads (6 fields) become a borrowed `SceneInput` struct either way.

**C. Present-completion batches in engine.** Engine stores
`PendingPresentBatch` (engine.rs:1334) whose entries carry core's
`CompletedPresentEvent` and sync-fd waits. Boundary: engine keeps the batch
container and wait kind over an opaque entry supplied by the KMS side;
`present_completion.rs` stays in `yserver`. Opaque is not enough: when the
batch wait is a sync fd, engine publishes it into each entry
(`completion.publish_release_fence(fd)`, engine.rs:2957;
present_completion.rs:42). So the entry needs a release-fence publication
interface. Prefer `Box<dyn PresentEntry>` (or a stored fn pointer) at this
edge: making engine generic over a present trait would monomorphise engine
code inside `yserver` and give back the compile-time win. **Resolved in the
phase-3 review.**

**D. `vk/dri3.rs` split.** Lines 15–520 are Vulkan external-memory work on
dma-buf fds (modifier queries, export/import); `vk/target.rs` needs these.
Lines 521–749 are `DMA_BUF_IOCTL_{EXPORT,IMPORT}_SYNC_FILE` ioctls plus
`poll`. The first part moves to render; the ioctl part stays in KMS.

**E. Visibility.** `pub(crate)` items used across the new crate edge become
`pub` in private modules, exposed via a facade `pub use` list in
`render/lib.rs`. Count before/after in the phase-3 PR.

### Platform gates (FreeBSD is supported — no blanket `cfg(linux)`)

`kms/mod.rs:2,9` gates on `any(linux, freebsd)`; `completion_poller.rs:23–133`
has epoll/kqueue arms; `imported_syncobj.rs:31–32` includes freebsd;
`vk/dri3.rs:550–553,701–704` already defines the ioctl numbers for non-Linux
targets. Rules:
- **Linux-only ioctls/syscalls** (epoll/eventfd/timerfd, VT, udev) keep their
  existing narrow gates. Do not add new gates around code that compiles on
  FreeBSD today.
- **fd-based Vulkan** (`external_memory_fd`, `dma_buf`, `external_semaphore_fd`,
  `image_drm_format_modifier`, `queue_family_foreign`, `vk/device.rs`
  446–450; `vk/sync.rs`; dri3 part D) is **not** Linux-only. It stays
  unconditional on `unix` targets, guarded at runtime by the existing capability
  checks (e.g. 17d29c5b: no `VK_KHR_external_semaphore_fd` ⇒ no DRI3 fences).
- No `ExternalMemory` trait until a nested backend demonstrates the need.

### Expected gain (hypothesis until measured)

Render does not depend on `yserver-core`, so it compiles alongside core. With
option (a) roughly a third of today's `yserver` unit leaves the critical path;
with (b) about half. **Hypothesis:** 32-cpu release 18.9 s → ~13–15 s; 4-cpu
10–25%. Measured on fuji and silence before phase 1 and after phase 3 with the
baseline recipe; recorded in the phase-3 PR and `docs/status.md`. Target ≥ 20%
on fuji's release critical path; if missed, report, do not tune without jos.

**Why not split `yserver-core`:** `{core_loop, server, resources, backend,
xinput, host_x11, nested, crossings, composite_redirects}` is one strongly
connected component, and core is already off the critical path.

**The `git mv` commit** (after A–E): moves the files into
`crates/yserver-render/src/`; `yserver` keeps `pub use yserver_render::{vk, …}`
aliases under `kms::` so kms paths compile. Package and directory stay
`yserver` (decision 3). `render_acceptance` stays in `yserver` (needs
`KmsBackend`).

**Core-side Unix leaks (recorded for nested, not phase 3):** `Backend` exposes
`RawFd` (`poll_fds`), `on_page_flip_ready(drm_fd)`, `OwnedFd` in `dri3_*`;
`unix_fd.rs` SCM_RIGHTS; `server.rs` `shmat`; Unix-socket transport.

## Phase 4 (deferred until nested work starts)

`yserver-xrender` (`backend/portable` + store/engine/scene/caches + `KmsCore`)
behind a `dyn DisplayPlatform`. Blocked today by KMS input glue
(`process_request`, `run::handle_host_input`, `fire_pending_repeats`,
`xinput::{hotplug, libinput_props}`) and RANDR mixing X11 view with DRM state.
No further crates until then (decision 5).

## Branches

The cleanup starts only after the two open branches, `fix/glyph-atlas-reclaim`
and `diag/214-slow-submits`, are merged or closed. Every other branch is kept
for reference, not direct reuse: anything reused later is re-applied by hand
onto the new layout. No porting tooling. For PR #112, erpalma is told before
phase 1 starts (jos writes that message).

## Risks

| risk | mitigation |
|---|---|
| Merge conflicts with in-flight work | start after the two open branches land; others re-applied by hand |
| Visibility creep | rule 2 per-item visibility compare + re-export allowlist; counted `pub` delta in phase 3 |
| Accidental behaviour change | whole-item moves; per-item hash verify; residual shadowing list reviewed; seam commits isolated and HW-smoked |
| Lost / renamed tests | rule 4 mapping per feature config, counts + ignored status |
| `cfg`/`#[path]` modules (xdmcp, crtc_transform_tests) | three feature configs in the gate; `#[path]` children moved explicitly |
| FreeBSD regression from new gates | no blanket `cfg(linux)`; narrow gates only where the code is Linux-only today |
| GpuCore borrow split changes ordering / teardown | lifetime rules + drop-spy test (boundary A); seam commit, lavapipe + HW smoke (bee amdgpu, eiger Asahi) |
| Compile-time claims wrong | all estimates are hypotheses until the fuji before/after measurement |

## Acceptance

- **File size:** no non-test `.rs` over 5,000 lines and no test `.rs` over 6,000
  (ceilings; split by responsibility, not to hit a number). Exceptions until
  their own steps land: `process_request/xi/dispatch.rs` (until 2c) and
  `backend/trait_impl.rs` (until 2.11b). CI step with an exception list.
- **Tests:** rule 4 mapping holds against the phase-0 snapshot, plus tests
  added meanwhile.
- **Seam:** the `backend/portable/**` grep passes; `cargo tree -p
  yserver-render` has no `drm`, `gbm`, `input`, `udev`, `yserver-core`.
- **Timings:** fuji + silence measured before phase 1 and after phase 3.

## Phase-0 checklist

1. `fix/glyph-atlas-reclaim` and `diag/214-slow-submits` merged or closed.
2. Baseline timings on fuji.
3. Test-list snapshot for all three feature configs (normal and `--ignored`).
4. `tools/split` (`apply`, `verify`, `verify --tests`, `verify --delegate`)
   proven end to end on `resources.rs`.

## Decisions (codex review, 2026-10-08; replaces the open questions)

1. **Trait impl:** explicit delegators grouped by subsystem (2.11b). The 10.3k
   `trait_impl.rs` is acceptable temporarily during the mechanical phase.
2. **XI2/RANDR handler extraction:** yes, as a separate later refactor (2c).
3. **Names:** crate `yserver-render`; package and directory stay `yserver` for
   now.
4. **Render extraction:** only after phases 1–2 and a re-run of the dependency
   inventory above, as its own separately reviewed phase.
5. **Further crates** (e.g. `yserver-xrender`): deferred until nested work.
6. **Size limits:** 5k code / 6k tests are ceilings; split by responsibility.
7. **Branches (jos, 2026-10-08):** start after the two open branches land;
   other branches are reference only, re-applied by hand; no porting tool.
8. **Platform gates:** narrow gates plus existing runtime capability checks;
   FreeBSD behaviour preserved; abstractions only for demonstrated needs.

## Resolve while proving the splitter on a small file (codex, third review)

- **Hash tokens, not text.** Compare normalized code tokens (visibility
  excluded, checked by the identity-mapped rule) and verify comments
  separately; collapsed whitespace is not invariant under rustfmt.
- **Path literals.** Moving tests changes `include_str!("../testdata/…")`
  and `#[path]` targets. Allow manifest-listed path edits (or relocate the
  fixtures) and check each still resolves to byte-identical content.
