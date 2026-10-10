# Phase 2.8 / 2.7: split `kms/render/platform.rs` and `kms/vk/scanout.rs`

Steps 2.8 and 2.7 of `2026-10-08-source-layout-cleanup.md`. Proposals with
dry runs (2026-10-10, on `refactor/phase-2-platform` stacked on 65b924b4);
nothing applied yet. Manifests: `tools/split/manifests/platform_2.toml` and
`scanout_2.toml` (+ `.paths`). Both follow the scene/engine pattern: depth-1
descendants (`pub(super)` = old private reach), all types stay in the root,
inherent impls spread per member. Module names avoid every first path
segment used in the file (`device`, `scanout`, `target`, `sync`, `buffer`, …).

Dry-run procedure per file: prep as a temporary commit, `split apply`,
`cargo +nightly fmt`, `cargo build -p yserver` and `cargo clippy -p yserver
--all-targets -- -D warnings` (default features, `RUSTC_WRAPPER` unset),
`split verify --manifest` (HEAD = prep vs working tree), then reset to
65b924b4 and remove the generated files. Logs:
`target/gate-{platform,scanout}-dry-*.log`. Execution per file (platform
first), as scene: prep commit, move commit (`split verify --manifest` and
`--tests`, full rule-5 gate), root `//!` ownership line + module map (own
commit; platform's map grouped GPU-only / KMS), blame-ignore, this doc.
Open questions carry a recommendation each.

## 1. `kms/render/platform.rs` (7,055 lines, tests already in `platform/tests.rs`)

**Structure.** Types (lines 92–2629, ~740 with the `PlatformBackend` struct
at 2220) interleaved with free fns: fence ticket/pool and present signal
(92–518), cursor helpers and `KmsCursorState` (611–923), scanout-route
qualification (963–2031, run on the startup worker and by `internal_probe`),
init rollback guard (2097–2218), inventory/connector helpers (2378–2627).
Then one `impl PlatformBackend` of 4,365 lines with banner comments
(cursor plane, storage allocation, SubmitGroup, I6a fences, I6b scanout BOs,
disable output, VT resume). Largest fns: `from_platform_init` 420,
`enable_connector_inner` 340, `flush_submit_group_with_exports` 186,
`allocate_drawable_storage_as` 178. No macros, no `module_path!`/`line!`, no
`#[track_caller]`; 3 explicit `target:` logs (literal strings, unchanged).

**Target tree (lines after fmt).** GPU-only first (Boundary A, the future
`GpuCore`), then KMS:

```
platform.rs        741  //! doc, mod decls, re-exports, imports, types, consts
-- GPU-only (Boundary A) --
fence.rs           384  FenceTicket, FencePool, PresentCompletionSignal, acquire
storage_alloc.rs   330  views, format_for_depth, allocate_drawable_storage(_as)
submit.rs          442  vk/ops pool accessors, paint + present submits, submit
                        group, flush(+_with_exports), abort_flush, test knobs
-- KMS --
init.rs            874  open, from_platform_init, for_tests, Drop, inventory,
                        rollback guard
devices.rs         192  device/route/output-geometry accessors, CrtcKey
qualify.rs         925  scanout-route qualification, replay, pool allocation
cursor.rs          995  cursor failure policy, KmsCursorState, cursor_plane_*
flip_events.rs     475  poll_fds, render completions, DRM events, page flip
scanout_bos.rs     377  scanout BO lifecycle, copied submit, record/commit
connectors.rs     1158  probe, enable/disable/remove, snapshot, layout
power.rs           227  wait_idle_bounded, disable_output, DPMS
```

Total 7,120 (7,055 before); all ≤ 5k.

**GPU vs KMS.** The three GPU-only files hold every method on the Boundary A
field subset (`vk`, `ops_command_pool`, `fence_pool`, `pixmap_pool`,
`submit_group`, `last_flush_outcome`, `renderer_failed`,
`force_next_submit_failure`). Mixed items stay KMS-side: `wait_idle_bounded`
(also idles `copy_vk_contexts`) in `power`, `submit_copied_scanout` in
`scanout_bos`. `flush_submit_group_with_exports` sits in `submit` with its
dma-buf sync-file half; the phase-3 seam splits it (plan Boundary A). The
`PlatformBackend` struct keeps its field order (pure move). rustfmt sorts the
`mod` lines, so the grouping is shown in the root `//!` module map (own
commit).

**Dry run.** Prep: 2 `super::submit_group::` paths → `crate::kms::render::…`
(+4/−2 after fmt). `split verify` → **OK**: 350/350 leaves identical, 53
visibility changes, 0 locations, 35 leaves with log calls moved to
descendants (prefix filters still match). fmt, build and clippy green.

**Visibility delta (53, all private → `pub(super)`):** 21 needed by the lib
(e.g. `route_requires_copy_free_probe`, `allocate_copied_scanout_pool`,
`check_scanout_liveness`, `create_present_completion_signal`,
`FenceTicket::retain_{signal,imported_wait}_semaphore(s)`,
`PlatformBackend::drain_scanout_pool_at`); 32 only by `platform/tests`
(cursor policy fns, `KmsCursorState::note_*`, qualification classifiers,
rollback guard `new_with`/`disarm`).

**Re-exports:** `pub(crate) use qualify::*` (`backend.rs` imports
`is_terminal_disposable_probe_error`, `internal_probe.rs`
`qualify_scanout_route_for_worker`); `use` globs for fence, cursor, init,
scanout_bos; `#[cfg(test)] use connectors::*`; none for the method-only
devices, flip_events, power, storage_alloc, submit.

**Refusals:** (1) relative paths: prep, 2 sites. (2) Name resolution: a
named `#[cfg(test)] use connectors::{…}` made `split verify` flag the test
calling `mode_via_connector_handle::<…>` (turbofish) as resolving
differently; the cfg(test) glob resolves like before and passes. (3) Unused
globs (compiler): 6 method-only modules, dropped. (4) Locations, log
targets, macros, includes, exceptions: none.

**Open questions.**
1. Flat vs `platform/gpu/{fence,storage_alloc,submit}`: nesting needs
   `pub(in crate::kms::render::platform)` and a re-export chain. **Flat**;
   phase 3 moves the three files wholesale.
2. Types stay in the root: forced anyway, `platform/tests` reads
   `FenceTicket.inner`. **Accept** (engine/scene decision 3).
3. The 32 test-only visibility changes (same reach). **Accept**.
4. Renames vs the 2.8 row: `cursor_plane`→`cursor`, `device`→`devices`,
   `scanout`→`scanout_bos` (path segments used in the file),
   `page_flip`→`flip_events` (also completions and present clocks); new
   `storage_alloc`, `power`. **Accept**, update the row.
5. `wait_idle_bounded` idles render and copy-sink devices. **Keep in
   `power`**; `GpuCore::drain_and_idle` is the phase-3 seam.

## 2. `kms/vk/scanout.rs` (6,617 lines, tests already in `scanout/tests.rs`)

**Structure.** Long allocation-direction `//!` doc (GBM-first, NVIDIA
findings); types for BO/copied-ownership state machines (97–582);
`impl CopiedRenderSource` 539 and `impl CopiedScanoutPool` 1,023
(`submit_copy_with_fence` 279, `probe_copy_all(_inner)` 390); dma-buf
metadata classification (2365–2843); `impl ScanoutBo` 483 + `Drop` 87;
`impl ScanoutBoPool` 389; disposable-probe error/fence machinery and copied
probe validation (4006–4978); allocation plans and modifier candidates
(4980–5810); Vulkan/GBM image allocation, framebuffer, transfer resources
(5812–6617). 27 relative `super::` paths in bodies (`dri3`, `sync`, `target`,
`optional_sync_fd_from_vk`, `owned_fd_from_vk`, `image_format_properties2`).
Three `#[track_caller]` format-query fns.

**Target tree (lines after fmt):**

```
scanout.rs          809  //! doc, mod decls, re-exports, imports, every type
bo_state.rs         241  BoState transitions, copied ownership/contents state
bo.rs               723  ScanoutBo allocate/probe/export/disarm + Drop,
                         partial-allocation rollback, DRM handle release
bo_pool.rs          498  ScanoutBoPool, OutputScanout, GBM device open
copied.rs          1632  CopiedRenderSource, CopiedScanoutPool (+Drop), plan
dmabuf_metadata.rs  355  PRIME caps, directional modifiers, route verdicts
probe.rs            819  probe errors/attempts/fences, copied probe validation
errors.rs            63  scanout_vk_error, io context, device-lost class
alloc_plan.rs       291  allocation plans, linear preference, pitch
modifiers.rs        498  modifier candidates/override, format-feature queries
image_alloc.rs      740  Vulkan/GBM scanout image, VkScanoutFb, transfer res.
```

Total 6,669 (6,617 before); all ≤ 5k.

**Dry run.** Prep: 27 `super::` → `crate::kms::vk::…` outside the root
imports (+31/−28 after fmt). `split verify` → **OK**: 376/376 leaves
identical, 105 visibility changes, 4 audited locations, 22 leaves with log
calls moved to descendants. fmt, build and clippy green.

**Visibility delta (105, all private → `pub(super)`):** 80 needed by the lib
(state-machine methods called across bo/copied/probe, error helpers,
plan/modifier/image fns, `COPIED_SINK_IMPORT_USAGE`); 25 only by
`scanout/tests` (`parse_scanout_modifier_override`, `padded_linear_pitch`,
`classify_route_from_direction_verdicts`, `SCANOUT_PITCH_ALIGN`,
`DRM_PRIME_CAP_*`, `DisposableProbeError::kind`, …).

**Re-exports:** `pub(crate) use errors::*` (platform calls
`scanout::scanout_error_is_device_lost`); `use` globs for alloc_plan, bo,
copied, dmabuf_metadata, image_alloc, modifiers, probe; none for bo_state,
bo_pool (method-only in the lib).

**Refusals:** (1) relative paths: prep, 27 sites. (2) Locations (4): the
phase-1a callers of the `#[track_caller]` format queries
(`exact_copied_source_plans` → alloc_plan, `probe_directional_modifiers` and
`probe_dmabuf_scanout_metadata` → dmabuf_metadata,
`scanout_modifier_single_plane_supports_feature` → modifiers). Same audited
reason as phase 1a: only the `asked from file:line` of the
`yserver::format_query` debug log (vk/mod.rs `image_format_properties2`)
changes, now file and line, not the query or its result. (3) Unused globs:
bo_state, bo_pool dropped. (4) Log targets, macros, includes, exceptions:
none.

**Open questions.**
1. The 25 test-only visibility changes (same reach). **Accept**.
2. Renames vs the 2.7 row: new `bo_state`, `errors`, `modifiers` (split
   from `alloc_plan`, which shrinks to 291). **Accept**, update the row.
3. `probe_copy_all(_inner)` (390) and the two `probe_renderer_access`
   methods could go to `probe.rs` (copied 1,632 → ~1,240). **Keep with the
   owner type**: the probe drives the pool's own state machine.
4. `errors.rs` is 63 lines. **Keep**: it is the crate-facing re-export.
