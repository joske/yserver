# Source layout cleanup: split the oversized files, then the crates

**Branch:** `plan/source-layout-cleanup` (plan only). Execution branches are per
phase, listed below.
**Goal, in priority order:**
1. Maintainability: no hand-edited source file over ~5k lines.
2. Compile parallelism: a shorter crate critical path on slow laptops.
3. A crate seam that lets a future nested backend for macOS/Windows reuse the
   Vulkan renderer.

**Start condition:** jos settles the open branches first (see "Branches"). No
logic changes are allowed in any move commit.

## Measured baseline (master `4db91343`, 2026-10-08)

Taken on silence (i9-13900K). sccache was bypassed with a passthrough
`RUSTC_WRAPPER` and `CARGO_INCREMENTAL=0`. Only the three workspace crates were
rebuilt; dependencies were warm. The "4 cpu" runs use `taskset -c 0-3`, which
only gives a lower bound for fuji (i5-7200U, 2c/4t, roughly 2–2.5× slower per
core). Format is start+duration in seconds, from `cargo build --timings`.

| profile | cpus | total | protocol | core | yserver | core lib(test) | yserver lib(test) |
|---|---|---|---|---|---|---|---|
| release | 32 | 18.9 | 0.0+1.0 | 0.5+11.1 | 3.6+15.0 | – | – |
| release | 4 | 35.0 | 0.0+1.7 | 0.5+20.1 | 4.0+30.8 | – | – |
| dev | 32 | 8.4 | – | 0.4+3.5 | 2.7+5.4 | – | – |
| dev | 4 | 9.5 | 0.0+0.6 | 0.4+4.5 | 2.8+6.5 | – | – |
| test --no-run | 32 | 14.6 | – | 0.5+3.7 | 2.9+5.9 | 0.6+8.6 | 4.2+10.4 |
| test --no-run | 4 | 20.5 | 0.0+0.7 | 0.4+6.1 | 3.5+10.2 | 0.7+15.1 | 6.5+13.9 |

A clean full release build including dependencies took 28.9 s at 32 cpus.

**What the numbers say:**
- **The critical path is the `yserver` crate, not `yserver-core`.** Cargo
  pipelines on metadata, so `yserver` starts about 3–4 s in while core is still
  in codegen. Core adds only its metadata time (about 3 s) to the path.
  `yserver` itself takes 15 s release at 32 cpus and 31 s at 4 cpus, and that is
  the long single-crate tail jos sees.
- **Splitting `yserver-core` buys almost nothing.** Splitting `yserver` into a
  part that does not depend on core plus a thinner top crate is the only cut
  that shortens the path.
- **Inline tests dominate test builds.** core lib(test) takes 15 s on 4 cpus;
  `process_request.rs` alone is 54k lines of tests.
- **File splits do not change compile time.** The crate is the compilation
  unit, so phases 1–2 are pure maintainability work. Only phase 3 can move the
  numbers.

## Inventory (from code, 2026-10-08)

Test/code ratios: `yserver` is 76k test of 195k lines, `yserver-core` 99k of
175k, `yserver-protocol` 11k of 28k.

| file | code | inline tests | structure |
|---|---|---|---|
| core `core_loop/process_request.rs` | 35.8k | 54.4k (816 tests) | **free fns** `(state: &mut ServerState, backend: &mut dyn Backend, ..)`; one `match header.opcode` (297–~500) → `handle_*` / `handle_<ext>_request` |
| yserver `kms/render/backend.rs` | 31.6k | 37.4k (565 tests) | `KmsBackend` struct (1401–1873); inherent impl 16.2k; **`impl Backend for KmsBackend` 10.3k (230 of 247 trait methods)** |
| `kms/render/engine.rs` | 12.9k | 6.4k | `impl RenderEngine` 7.1k, recording `emit_*` free fns 4k |
| `kms/render/scene.rs` | 9.6k | 7.6k | `SceneCompositor`, walk free fns, `tick_one_output` (1,052-line single fn), `ComposeRenderTarget` |
| `tests/render_acceptance.rs` | – | 16.9k (186 tests, all `#[ignore]`, lavapipe) | topic blocks, helpers kept next to their tests, no shared harness |
| `core_loop/pointer_fanout.rs` | 4.4k | 6.0k | free fns; `pointer_event_fanout_to_state_inner` is a 2,009-line single fn |
| `core_loop/run.rs` | 4.8k | 4.7k | `run_core_with_inventory` (~990 lines), telemetry, queues, XI config lane |
| `kms/render/platform.rs` | 7.0k | 2.2k | `PlatformBackend`: DRM device, outputs, libinput, fences, scanout pools, cursor plane |
| `kms/vk/scanout.rs` | 6.6k | 2.0k | GBM/dma-buf scanout BOs, copied PRIME pool, probes; fully Linux |
| protocol `x11/mod.rs` | 4.9k | 2.8k | flat `pub fn` encoders and parsers, no impls |
| core `server.rs` | 4.4k | 3.1k | ~25 state types, `ServerState` with 2 impl blocks, fanout fns |
| core `resources.rs` | 3.9k | 3.2k | `impl ResourceTable` 2.9k contiguous, plus types |

**Core is `dyn Backend` everywhere.** There are 225 `dyn Backend` uses and no
`<B: Backend>`, so nothing in core is monomorphised into `yserver`. The generic
fns in `kms` are few and stay crate-internal (`assemble_root_scanout<F>`, the
probe helpers). A split does not risk a compile-time regression from generics.

**Pure-move blockers** (single bodies that only shrink if they are edited):

| fn | lines |
|---|---|
| `handle_xi2_request` | 5,140 |
| `handle_randr_request` | 2,277 |
| `pointer_event_fanout_to_state_inner` | 2,009 |
| `tick_one_output` | 1,052 |
| `run_core_with_inventory` | ~990 |

These move whole in phase 2. Phase 2c is optional.

## Rules for every commit

**1. One file, one kind of change.** Each commit is exactly one of the
following, and the subject line says which:
- **move:** cut/paste of whole items, keeping their order within each new file.
- **visibility:** only `pub(super)` / `pub(in path)` additions and `use` lines.
- **delegate:** trait method bodies → `self.<subsystem>_<name>(..)`.
- **seam:** a real but tiny logic change, needing a HW smoke.

**2. No new `pub` or `pub(crate)` in phases 1–2.** Rust lets child modules see
their ancestors' private items. So `foo.rs` becomes `foo/mod.rs` with children,
and children need no visibility change to reach mod.rs privates. Only calls
between sibling children, and from mod.rs into a child, need `pub(super)`. mod.rs
re-exports each child with a private `use child::*;`, so existing `use super::*`
test modules and outside `crate::…::fn` paths compile unchanged. Check with
`git diff -U0 | grep -E '^\+.*pub(\(crate\))? (fn|struct|enum|mod)'`, which
must be empty.

**3. Mechanical move proof.** Add `tools/split/verify-move.sh <rev>`. For the
commit, it takes the multiset of removed lines minus the multiset of added
lines, and the reverse. It ignores blank lines, `use`/`mod` lines and
`pub(super)` tokens, and must print nothing. Reviewers use
`git show --color-moved=dimmed-zebra --color-moved-ws=allow-indentation-change`
(and `--no-ext-diff`, because difftastic is the configured differ).

**4. Gate per commit:**
- `cargo +nightly fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test --workspace`
- lavapipe `cargo test -p yserver --features xdmcp -- --ignored`
- `cargo build` in the three feature configs (none, `tcp-transport`, `xdmcp`)

Plus a **test-count invariant**: `cargo test --workspace -- --list` (and the
`--ignored` list) must be identical before and after, sorted. A lost
`#[cfg(test)]` or `mod` line otherwise looks green.

**5. Blame.** Every move commit's SHA is appended to `.git-blame-ignore-revs` in
a follow-up commit; GitHub honours that file. Locally, run
`git config blame.ignoreRevsFile .git-blame-ignore-revs`. Moves across files
still need `git blame -C -C`, so one line saying so goes into the file header.

**6. Scripted, re-runnable splits.** Each split is generated by
`tools/split/split.py <manifest>`. A manifest is a TOML list of
`item-name → target file` per source file, and items are found by top-level
`fn`/`impl`/`struct` start lines with their attributes and doc comments
attached. The move commit is the script's output plus `cargo +nightly fmt`.

Two reasons for this:
- Review becomes "read the manifest", not a 30k-line diff.
- An open branch can port itself across the split by running the same
  manifest on its own tip (see "Branches").

The tools stay until the last open branch has crossed, then are deleted.

## Phase 1: extract inline tests (about 125k lines, zero visibility change)

This is the cheapest and largest win. 50–60% of every giant file is
`#[cfg(test)] mod tests { use super::*; … }`. It runs in two commits per file:

**1a (move).** `mod tests { … }` becomes `#[cfg(test)] mod tests;` plus
`foo/tests/mod.rs` (or `foo_tests/` next to it while `foo.rs` is still a single
file, via `#[path]`). The body is byte-identical, and `use super::*` still sees
everything.

**1b (move).** Split `tests/mod.rs` into topical files:
- `tests/mod.rs` keeps the fixtures and declares `mod <topic>;`.
- Each topic file starts with `use super::*;`, and through the glob re-export it
  sees both the fixtures and the parent's items.

Targets (test lines, file sizes ≤ ~6k):

| file | tests/ topics |
|---|---|
| process_request (54.4k) | `xi2_config`, `xi2_dynamic`, `xi2_grabs_allow`, `xi1` (≈20k across 4), `randr` 6k, `present` 8k (split `present_supersede`), `composite_redirect` 5k, `sync` 3k, `window` 4k, `copy_area` 3k, `glx`, `saver_dpms`, `shm_cow`, `misc` |
| backend.rs (37.4k) | `fixtures` (31654–37164, 5.5k), `xi_input` (~14k, split into `xi_source`/`xi_dynamic`/`keyboard`), `randr_provider_gamma` 4k, `render_composite_clip` 6k, `present_scanout_direct` 8k, `dri3_resolve_paint` 5k; `crtc_transform_tests.rs` is already separate |
| engine (6.4k) | the existing nested mods: `clamp_copy`, `uniform_glyph_source`, `premul_from_wire`, `coalescing`, `session` |
| scene (7.6k) | `walk`, `damage_audit`, `cursor`, `compose`, plus `test_helpers.rs` (the existing 9360–9631 cfg(test) helpers) |
| pointer_fanout 6k, run 4.7k (+ `server_reset` 0.7k), server 3.1k, resources 3.2k, platform 2.2k, vk/scanout 2.0k, x11/mod 2.8k | 2–4 files each |

The process_request tests are interleaved by topic, so 1b is driven by test name
prefix, not line range. The manifest lists names.

**render_acceptance.rs (16.9k)** becomes
`tests/render_acceptance/{main.rs, common.rs, put_copy.rs, glyphs.rs, copy_plane_fill_traps.rs, redirect_backing.rs, depth_pool.rs, present_submit.rs, frame_builder.rs, view_cache_depth1.rs, masked_copy.rs, cursor_border.rs, xts_wz_windows.rs, compositor_exports.rs, trap_stress.rs}`,
following the topic map at lines 39–16916.

It stays **one test binary with modules, not 14 binaries.** Every integration
binary links all of `yserver` plus its dependencies with debuginfo. The
`render_acceptance` unit alone costs about 1 s per test build today, and 14
binaries would multiply that on every `cargo test`. The CI lavapipe step (`cargo test -p yserver -- --ignored`)
needs no change.

## Phase 2: split production code (moves), conflict-light files first

The order runs from least- to most-touched by open branches, so the
contentious files go last, after their branches land.

| step | file → dir | target modules (≈ code lines) |
|---|---|---|
| 2.1 | protocol `x11/mod.rs` | `types` 0.55k, `setup` 0.35k, `request_parsers` 1.0k, `events_core` 0.6k, `events_input` 0.5k, `replies_core` 1.2k, `xi` 0.9k, `xkb_events` 0.9k, `render` 0.5k; `mod.rs` ≈ 0.3k of `pub use` (no downstream path change) |
| 2.2 | core `resources.rs` | `mod.rs` 0.5k (table, ctors), `types` 0.7k, `window` 1.3k, `tree` 0.5k, `gc` 0.6k, `pixmap_props` 0.35k, `picture_font_cursor` 0.45k, `cleanup` 0.25k: `impl ResourceTable` spread over the files |
| 2.3 | core `server.rs` | `mod.rs` 0.7k (`ServerState`, `new`), `types_grabs` 0.7k, `types_ext` 0.9k, `xi_registry` 0.4k, `idle` 0.25k, `hit_test` 0.65k, `fanout` 0.75k |
| 2.4 | `core_loop/run.rs` | `mod.rs` 1.1k (`run_core*`), `telemetry` 0.45k, `queues` 0.6k, `xi_config` 0.65k, `requests` 0.35k, `present_tail` 0.4k, `randr_notify` 0.7k, `accept` 0.45k, `repeat` 0.2k |
| 2.5 | `core_loop/pointer_fanout.rs` | `mod.rs` 0.7k, `fanout_inner` 2.1k (the 2k fn), `xi1` 0.65k, `grabs` 0.65k, `xi2_targets` 0.7k |
| 2.6 | `core_loop/process_request.rs` → `core_loop/request/` (keep `process_request` as the re-exported entry) | see tree below |
| 2.7 | `kms/vk/scanout.rs` | `bo` 1.4k, `bo_pool` 0.5k, `copied` 2.0k, `dmabuf_metadata` 0.5k, `probe` 1.5k, `probe_verify` 0.4k, `alloc_plan` 1.3k, `image_alloc` 0.75k |
| 2.8 | `kms/render/platform.rs` | `fence` 0.75k, `cursor_plane` 1.8k, `qualify` 1.5k, `device` 1.3k, `init` 0.9k, `scanout` 1.2k, `connectors` 1.4k, `submit` 0.7k, `page_flip` 0.6k |
| 2.9 | `kms/render/scene.rs` | `mod.rs` 1.4k, `cursor` 0.95k, `damage_audit` 2.0k, `tick_output` 1.8k, `walk` 2.6k, `fan_out` 0.2k, `targets` 1.1k (`ComposeRenderTarget` + impls), `root_readback` 0.35k |
| 2.10 | `kms/render/engine.rs` | `mod.rs` 1.3k, `types` 0.8k, `staging` 0.5k, `frame` 1.3k, `clip_snapshot` 0.6k, `gradient_assets` 0.7k, `fill_copy` 1.5k, `put_get` 0.9k, `text` 0.5k, `glyphs` 1.4k, `composite` 1.2k, `traps` 0.5k, `batch` 0.6k, `rollback` 0.6k, `record/{copy,render,traps,text,fill}` ≈3k |
| 2.11 | `kms/render/backend.rs` | along the portable/KMS seam, tree below |

**2.6 `core_loop/request/`:**
- `mod.rs` 0.7k: consts, `RequestOutcome`, `process_request` + the opcode match.
- Core requests:
  - `common` 0.7k: `emit_x11_error*`, `drawable_lookup`, the `validate_*` fns, `write_to_client`.
  - `window` 1.6k, `redirect` 1.6k, `property` 1.0k, `selection` 0.55k.
  - `drawing` 2.4k, `gc_pixmap_font` 1.2k, `input_focus_grab` 2.3k, `core_misc` 2.4k.
- Extensions:
  - `render` 0.95k, `randr` 2.7k, `sync` 1.0k (+ xinerama 0.15k), `present` 2.95k, `dri3` 0.8k, `glx` 1.5k (two chunks today).
  - `xfixes` 1.3k, `shape` 0.4k, `composite` 0.45k, `damage` 0.25k, `shm` 0.8k, `xtest` 0.6k, `xres` 0.25k, `xcmisc` 0.13k, `vidmode` 0.5k.
  - `dpms_saver` 1.0k, `xkb` 0.4k (+ keymap fns 0.25k), `xinput/{xi1 0.7k, xi2 5.3k, property 0.8k}`.
- Visibility: about 40 handlers are `pub(crate)` today and called from `run.rs`/`pointer_fanout`. They stay `pub(crate)` and are re-exported from `mod.rs`.
- The cross-group helpers (`emit_x11_error`, `write_to_client`, `emit_property_change`, …) become `pub(super)` in `common`.
- Interleaved regions get reordered in the move: DPMS/saver, XI1/XI2 AllowEvents, the two GLX chunks, XTEST/cursor.

**2.11 `kms/render/backend/`.** Structure the split by the seam, so the crate
work in phase 4 is a directory move:
```
backend/
  mod.rs            ~0.8k  header types, KmsBackend struct, open()
  trait_impl.rs     ~1.4k  impl Backend for KmsBackend: 230 3–6-line delegators (step 2.11b)
  portable/         ~19k   renders X semantics into the GPU crate; no drm/gbm/fd/libc imports
    windows.rs 2.2k  redirect.rs 0.7k  pixmaps.rs 0.5k  cursors.rs 1.3k (records only)
    gc_clip.rs 1.7k  draw.rs 2.7k  render_ops.rs 3.5k  get_image.rs 1.5k  fonts_text.rs 0.5k
    input_logic.rs 1.6k (xkb/keymap/cook_host_key: no thread/libinput types)
  kms/              ~12k   Linux display + device side
    scanout.rs 3.0k (direct scanout, unflip, M0/M1/M2, crtc probes)  randr_hw.rs 2.6k
    present.rs 1.5k  dri3_glx.rs 0.9k  vt.rs 0.7k  input_thread.rs 1.0k  cursor_plane.rs 0.3k
  for_tests.rs      ~2.9k  the pub *_for_tests helpers (integration tests use them)
  telemetry.rs      ~0.4k
  tests/            phase 1
```
- **2.11a (move):** the inherent impl and the free fns go into the tree. Child
  modules see the private `KmsBackend` fields, so no field visibility changes.
- **2.11b (delegate):** each trait method body moves to an inherent
  `<subsystem>_<name>` method in its group file, and the trait method becomes a
  one-line call.
  - **Not a pure move.** `verify-move.sh` gets a delegate mode that checks every
    trait method body is a single forwarding call with identical arguments.
  - **Name collisions to rename on the inherent side:** `fb_dimensions`,
    `randr_outputs_and_modes`, `randr_providers`,
    `acquire_glx_pixmap_export`/`release_glx_pixmap_export`,
    `vt_switching_armed`, `promote_pixmap_exportable`, `present_get_ust_msc`.
  - **Alternative if jos prefers:** keep a single 10.3k `trait_impl.rs` and skip
    2.11b. It is the one file left over the limit.
- **Seam check after 2.11, as a CI grep:** `backend/portable/**` contains no
  `crate::drm`, `gbm`, `OwnedFd|RawFd`, `libc::`, `nix::`, `platform::` or
  `backend::kms::`. Each violation found during the split is listed and moved,
  not waived.

**Phase 2c (optional, edits, not moves).** Split the arms of
`handle_xi2_request` and `handle_randr_request` into `handle_xi2_<minor>` /
`handle_randr_<minor>` fns. Then `xi2.rs` goes from 5.3k to `xi2/{events,
devices, grabs, props, focus}.rs`. Each arm moves as its own body, so this is
checkable but not byte-pure: review it as a refactor, with HW XI smoke on
eiger. Do it only on jos's go.

## Phase 3: crate split (the only phase that changes build time)

### Target graph
```
                yserver-protocol
               /        |        \
     yserver-core   yserver-render   (render: no core, no drm/gbm/udev/libinput)
               \        |
                \       |      future: yserver-nested (WSI present + host input)
                 \      |     /
               yserver-kms   (bin "yserver": drm, gbm, libinput, udev, VT, DRI3,
                              scanout, KmsBackend incl. backend/portable)
   future (when nested starts): backend/portable → yserver-xrender (core + render),
                                shared by yserver-kms and yserver-nested
```
- **`yserver-render`** (~55k code + ~20k tests) contains:
  - `kms/vk` minus `scanout.rs` and `dri3.rs`; `ops/`, pipelines, `pixmap_pool`,
    `vram`, `mem_accounting`, `device`, `instance`, `target`.
  - `kms/render/{engine, scene, store, frame_builder, upload_arena, glyph_atlas,
    glyph_pixels, region, stroke, scene_diff, descriptor_pool_ring, batch_resource,
    transform_intermediate, root_overlay, cursor, cursor_save, submit_*, telemetry,
    scanout_damage}`.
- **The parallelism gain:** render does not depend on `yserver-core`. It
  compiles alongside core, starting right after protocol's metadata, so roughly
  half of today's `yserver` unit leaves the critical path.
- **Honest estimate:** release at 32 cpus goes from 18.9 s to about 12–14 s.
  On 4 cpus the two crates compete for the same cores; expect 10–25% (35 s to
  about 27–31 s). That is unverified: phase 0 and the end of phase 3 measure it
  on fuji.
- **Why not split `yserver-core`:** one strongly connected component spans
  `{core_loop, server, resources, backend, xinput, host_x11, nested, crossings,
  composite_redirects}`. `server` and `xinput` call back into `core_loop`. Only
  leaves totalling about 4.4k lines (`randr`, `properties`, `transport`,
  `xauth`, `unix_fd`, `present_scheduler`) sit outside it. And per the timings,
  core already sits off the critical path.

### Blockers to clear before `git mv` (each its own small commit)

| blocker | fix |
|---|---|
| `yserver_core::backend::GcFunction` (engine 4, scene 3, vk `logic_fill_pipeline` 1) and `backend::params` (stroke) | move the plain data types to `yserver-protocol` (or a `types` module there); core re-exports them, so its paths are unchanged |
| `yserver_core::randr::CrtcTransform` (scene 2) | same: plain data, move down |
| `BatchResource` (render) is implemented by vk; `DescriptorPoolRing` is taken by `vk/render_pipeline.rs:516`; `decode_x11_pixel_for_storage` (engine.rs:12864) is called from `vk/ops/scanout_logic_fill.rs` | they move with render anyway; this only matters if vk is extracted alone |
| `crate::vk_count!` (render 49, vk 98 uses) | `#[macro_export]` from render |
| `crate::kms::backend` helpers (engine 14, stroke 11: `ClipMaskCache`, `scanline_fill_polygon`, `bresenham_segment`, `pixman_transform_to_affine`) | move the helpers from `kms/backend.rs` (1.5k) into render |
| scene → `super::backend::{WindowsMap, WindowGeometry}` (16 refs) | move these types from `KmsBackend`'s header into `render::scene::types` |
| `store.rs` → `platform::{FenceTicket, PlatformBackend}` (only `platform.vk`) | `FencePool`/`FenceTicket` move to render; `destroy` takes `&VkContext` |
| scene.rs:8597–8644 `crate::drm::page_flip::submit_flip_with_fences`, the only hard DRM call in scene | **seam commit:** generalise the private `ComposeRenderTarget` into a pub `PresentTarget` trait in render; KMS implements its submit, and later a swapchain image implements it for nested. HW smoke: flip, unflip, direct scanout on bee and eiger |
| `vk/device.rs` `DrmDeviceKey`, `VulkanDrmIdentity`, `PhysicalDeviceSelection::{RenderEndpoint, Exact}`, and the extension tail at 446–450 (`external_memory_fd`, `dma_buf`, `external_semaphore_fd`, `image_drm_format_modifier`, `queue_family_foreign`) | `DrmDeviceKey` is a plain `(major, minor)`, so it moves to render; the extension tail goes behind `#[cfg(target_os = "linux")]` (a cfg, not a feature — no kill switch) |
| `pub(crate)` used outside the module, needing `pub` (vk ≈74 fns/39 fields/12 structs; render ≈87 fns/12 structs/140 field names, by-name over-counts) | make them `pub` but keep modules private where possible, and expose a facade `pub use` list in `render/lib.rs`. Count before and after; review any increase over the inventory |

**The `git mv` commit.** It moves `kms/vk` and the files listed above into
`crates/yserver-render/src/`. `yserver-kms` keeps
`pub use yserver_render::{vk, …}` aliases under `kms::`, so the
`crate::kms::vk::…` paths in kms code keep working. `render_acceptance` stays
in `yserver-kms` because it needs `KmsBackend`. Renaming the `crates/yserver`
directory to `yserver-kms` is optional, and so is the package name; the binary
stays `yserver`.

### Linux-only surface to fence (inventory for nested; not all needed in phase 3)

| where | what | in phase 3 |
|---|---|---|
| `vk/dri3.rs` 766 | dma-buf import/export, `DMA_BUF_IOCTL_*_SYNC_FILE`, `libc::poll` | → kms |
| `vk/scanout.rs` 8.6k | GBM BOs, scanout images, PRIME | → kms |
| `vk/sync.rs` 100 | sync-fd semaphore wrapper (`OwnedFd`) | render, `cfg(linux)` |
| `vk/target.rs` 1.9k | dma-buf-backed `DrawableImage` (fd 15, modifier 118) | render; external-memory constructors in `cfg(linux)` submodule |
| `vk/mem_accounting.rs` | export buckets | render (data only) |
| `vk/instance.rs`, `device.rs` | `external_*_capabilities`, DRM physical-device identity | `cfg(linux)` |
| `render/store.rs` 2.3k | import/export hooks (fd 8) | render; hooks behind `cfg(linux)` |
| `render/{platform, imported_syncobj, completion_poller (epoll), present_completion, present_source_wait, export_holders, probe_executor}` | DRM, syncobj, epoll | → kms |
| totals in vk+render today | `std::os::fd` 63, `std::os::unix` 27, `libc::` 91, `nix::` 59, epoll/eventfd/timerfd 85, `ash::khr::external_*_fd` 56 | mostly in files that go to kms |

On the core side (not blocking render, but blocking nested on Windows):
- The `Backend` trait leaks `RawFd` (`poll_fds`), `on_page_flip_ready(drm_fd)`
  and `OwnedFd` in the `dri3_*` methods.
- `unix_fd.rs` (SCM_RIGHTS), `server.rs:2863` `shmat` and the Unix-socket
  transport.

macOS is close: BSD sockets, `shmat` and SCM_RIGHTS all exist. Windows needs a
transport and MIT-SHM rework. Out of scope here, just recorded.

## Phase 4 (deferred until nested work starts): `yserver-xrender`

Extract `backend/portable/` together with the portable `KmsBackend` fields:
- `store`, `engine`, `scene`, `windows`, the caches, cursor records and
  `core: KmsCore` (XKB/fonts, 15.8k with `xkb`/`xkb_desc`).

They form a `RenderBackend` that holds a `Box<dyn DisplayPlatform>`. Use dyn,
not generic, so it compiles in its own crate and not in the binaries. The
roughly 45 KMS fields stay in `KmsPlatform`, in their four clusters: scanout,
vblank/CRTC/RANDR-hw, DRI3/Present/dma-buf, and VT/input threads.

What blocks phase 4 today:
- `KmsBackend` calls core glue for input: `process_request`,
  `run::handle_host_input`, `fire_pending_repeats`, `InputOrigin`, and
  `xinput::{hotplug, libinput_props}`. The input paths belong on the platform
  side.
- RANDR output code mixes the X11 view with DRM state.

Doing this now would be speculative. Phase 2.11's directory seam plus the CI
grep keeps it cheap later.

## Branches (state 2026-10-08) and landing order

**Unmerged branches, by big files touched:**

| branch | tip | touches |
|---|---|---|
| `fix/100-implicit-sync-exports` (16 commits) | 10-07 | process_request, backend, engine, scene, pointer_fanout, run, platform, server |
| `diag/214-slow-submits` (6) | 10-08 | process_request, backend, engine, scene, run, platform, scanout, render_acceptance |
| `fix/glyph-atlas-reclaim` (2) | 10-07 | backend, engine |
| `fix/214-trap-per-instance-bbox` (2) | 10-07 | backend, render_acceptance |
| `fix/direct-scanout-scene-revocation` (3) | 09-21 | backend |
| PR #112 (erpalma) / `test/pr112-rebased` | 09-11 | scene (+1215/−146), backend |
| `fix/shape-canonicalise-rects` (3) | 09-11 | scene, backend |
| `fix/213-expose-on-restack`, `pr-198` | – | stale pre-squash copies (#213 and #198 are on master), deletable |

**Landing order:**
1. Land or park all of the above (jos decides which).
2. For PR #112, tell erpalma before phase 1 touches `scene.rs`/`backend.rs`
   (jos writes that message).

**Porting a branch that missed the window:**
1. `git rebase <last master commit before the split>`.
2. Run `tools/split/split.py` with the same manifests on the branch tip.
3. Commit the result.
4. `git rebase --onto master <that-split-commit>`. The diff now applies to the
   same layout.

Phase-1 test moves only conflict with test edits, so phase 1 can start once the
branches touching `process_request`/`backend` tests have landed.

## Risks

| risk | mitigation |
|---|---|
| Merge conflicts with in-flight work | start only after branches settle; scripted manifests for porting; one file per commit, so a conflict is local |
| Visibility creep (`pub(crate)` everywhere) | rule 2 grep; in phase 3, a counted `pub` delta in the PR body |
| Accidental behaviour change | moves only; `verify-move.sh`; test-list invariant; seam commits isolated and HW-smoked |
| Lost tests (dropped `mod` line) | test-list invariant per commit |
| `cfg`/`#[path]` modules (xdmcp, crtc_transform_tests) | three feature configs in the gate; `#[path]` children are kept and moved explicitly |
| Compile-time regression | measure at the end of phases 1, 2 and 3 with the same recipe; core is `dyn`, so generics are no risk |
| Blame churn | `.git-blame-ignore-revs` plus `-C -C` |
| Seam commits (PresentTarget, delegators) hide real changes | each is its own commit with its own review; PresentTarget needs HW smoke on bee (amdgpu) and eiger (Asahi) |

## Acceptance

- **File size:**
  - No non-test `.rs` over 5,000 lines, except `request/xinput/xi2.rs` (~5.3k)
    unless 2c is done, and `backend/trait_impl.rs` if 2.11b is skipped.
  - No test `.rs` over 6,000 lines.
  - Checked by a CI step: `find crates -name '*.rs' | xargs wc -l` against a
    list of exceptions.
- **Tests:** test lists (normal and `--ignored`) identical to the phase-0
  snapshot, plus any tests added meanwhile.
- **Seam:** the `backend/portable/**` grep check passes. `yserver-render`
  builds with no `drm`, `gbm`, `input`, `udev` or `yserver-core` in
  `cargo tree -p yserver-render`.
- **Timings:** the phase-0 recipe (`--timings`, passthrough wrapper,
  `CARGO_INCREMENTAL=0`, workspace crates only) is run on fuji and silence
  before phase 1 and after phase 3. The numbers are recorded in the phase-3 PR
  and in `docs/status.md`. The phase-3 target is a release critical path on
  fuji at least 20% shorter; if it is not, report it and do not tune further
  without jos.

## Phase-0 checklist

1. Branches settled (above).
2. Baseline timings on fuji.
3. Snapshot the test list.
4. `tools/split/{split.py, verify-move.sh}` on a branch, proven on one small
   file (`resources.rs`) end to end.

## Open questions (jos / codex)

1. Trait impl: 230 delegators (2.11b), or one 10.3k `trait_impl.rs` left as a
   known exception?
2. Phase 2c: split the arms of the XI2/RANDR matches, or keep the 5.1k/2.3k
   bodies?
3. Crate names: `yserver-render` (vk + engine + scene), keep `crates/yserver`
   as the KMS binary crate, or rename it to `yserver-kms`?
4. Should phase 3 also cover the ~12k `backend/kms/` vs `portable/` crate cut
   now (phase 4), or wait for real nested work as proposed?
5. `cfg(target_os = "linux")` for external memory/sync-fd in render, or an
   `ExternalMemory` trait now? The cfg is cheaper; the trait is needed only once
   MoltenVK/KosmicKrisp interop (Metal shared events) is real.
6. The size limits (5k code / 6k tests): right numbers?
