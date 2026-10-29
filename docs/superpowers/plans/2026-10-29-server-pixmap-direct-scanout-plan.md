# Direct scanout of server-created compositor Present pixmaps — plan

Date: 2026-10-29
Base: `origin/master` `52ea8822` (#311). Branch: `feat/server-pixmap-scanout`.
Spec: `docs/superpowers/specs/2026-10-29-server-pixmap-direct-scanout.md`
(option A, with the probe results appended there).
Probe reference (not merged): `diag/server-pixmap-scanout-probe` `df3c284b`.

## Goal

A root-sized depth-24 pixmap that a compositor creates with `XCreatePixmap`
and Presents to the COW (or an eligible COW descendant) is promoted once to
scanout-capable storage, after which M2 flips it directly, as it already does
for imported dma-buf sources. Every rejection keeps today's Copy + compose
path.

Path prefixes used below:

- `K/` = `crates/yserver/src/kms/render/backend/kms/`
- `B/` = `crates/yserver/src/kms/render/backend/`
- `R/` = `crates/yserver-render/src/kms/`
- `modeset.rs` = `crates/yserver/src/drm/modeset.rs`

Line numbers are on `52ea8822`.

## What exists, what is new

Already there (imported sources):

- M0 classification, `observe_scanout_m0` (`K/scanout.rs:2767`), runs from
  `note_present_scanout_candidate` (`crates/yserver-core/src/core_loop/process_request/present_ext.rs:1004`), before
  `try_present_direct` (`present_ext.rs:1024`).
- The M1 probe, `maybe_probe_scanout_m1` (`K/scanout.rs:2330`), reads only
  `imported_dmabuf`. A server-owned source counts `m1_gate_reject_import`
  (`:2457`) and `reject_server_owned` (`:2917`).
- The M2 flip, `backend_present_try_present_direct` (`K/present.rs:800`).
  It gates through `direct_candidate_gate` (`K/present.rs:58`) and probation
  (`SCANOUT_M2_ELIGIBLE_ROOT_PROBATION = 8`, `K/state.rs:259`), takes the
  cached probe framebuffer (`:898-908`), and pins source and fallback target
  (`:910-911`, `pin_direct_source` `K/scanout.rs:926`). It submits one
  framebuffer to every output in one commit (`submit_direct_frame_live`,
  `K/scanout.rs:956`, planes at `:969-980`) and retires it in
  `retire_direct_output` (`K/scanout.rs:2140`). Retirement emits Complete
  `FLIP` and idles the previous frame (`release_direct_frame`, `:1500`).
- Unflip: `request_direct_unflip` (`K/scanout.rs:429`), plus the lazy COW
  shadow `materialize_direct_shadow_for_unflip` (`K/scanout.rs:1717`).
- #311: the probe cache is not cleared on a topology change while a direct
  owner is active (`K/scanout.rs:2359-2368`). A live framebuffer is dropped
  only after its CRTCs are replaced (`:1656-1675`).

New:

1. A scanout descriptor for server-owned storage.
2. A scanout allocation that is independent of the GLX TFP strategy.
3. Candidate tracking and once-per-storage promotion.
4. A storage generation in the probe-cache key.
5. Explicit `IN_FENCE_FD` for server writes on direct plane commits. Today
   `DirectScanoutPlaneState` (`modeset.rs:2003`) carries no fence, and the
   source wait (`K/present.rs:1002`) only covers imported fds.
6. Telemetry.

## Decisions (for review)

Each is also marked inline in the task that implements it.

- **D1 Promotion trigger.** Promote on the **second** eligible Present of
  the same storage, not the first. That proves the compositor reuses the
  buffer: marco's two buffers each promote on their second Present. A client
  that allocates a fresh root pixmap per frame never pays 6–8 ms per frame.
  Cost: two extra composed frames at startup.
- **D2 Synchronous promotion.** Keep promotion synchronous, inside
  `observe_scanout_m0` and before `maybe_probe_scanout_m1`. The same Present
  then probes, and its Copy reads the new storage (probe remark (a)). It is
  one-time per buffer (measured 6–8 ms fuji, 0.5–0.8 ms silence). Revisit
  only if the HW check shows a visible hitch at video start.
- **D3 Failure budget.** A failed promotion, export or `TEST_ONLY` marks that
  storage generation failed, and it is never retried. The backend allows at
  most 4 failed promotions per topology epoch; then it stops promoting until
  the next topology change. That bounds repeated synchronous stalls on a
  driver that cannot do it.
- **D4 Own allocator, linear only in v1.** Add `allocate_scanout_pixmap`.
  It never reads or writes `VkContext::tfp_tiling_strategy` (remark (b)) and
  allocates only:
  - `TILING_LINEAR` when linear export is advertised;
  - otherwise DRM modifier `LINEAR` via the modifier extension.

  Both pixmap usages (`EXPORT_IMAGE_USAGE`, `R/vk/target.rs:1232`) are
  required, as today. The `LINEAR` modifier must appear in every output's
  `scanout_modifiers` (an empty list means unknown, and `TEST_ONLY` decides).
  These are the two layouts the probe measured as accepted. Tiled modifiers
  are a non-goal.
- **D5 Targets.** v1 admits `Cow` and `CowDescendant` only. `Unredirected`
  server-owned sources (an application's plain-pixmap Present) keep the Copy
  path. That needs its own scene proof.
- **D6 Exported means ineligible.** A pixmap is never promoted, and a
  promoted one loses its descriptor, when either holds:
  - `store.is_exported(id)` (`R/render/store.rs:953`) is true;
  - `pstate.exported_dmabufs` has an entry (DRI3/GLX export,
    `K/export.rs:618`).

  An external writer would defeat the server-write fence. If it is the live
  direct source, it unflips with reason `server_source_exported`.
- **D7 Separate write-fence registry.** Promoted scanout storage joins the
  flush chokepoint's signal-semaphore path through its own set, not through
  `exported_sync`. That keeps `is_exported` meaning "a client holds an fd"
  (D6), and it avoids publishing implicit WRITE fences nobody reads.
- **D8 Explicit fence, no implicit-sync reliance.** Every direct plane commit
  of a server-owned source carries `IN_FENCE_FD` from the sync_file of the
  last submit that wrote the source.
  - Fallback 1: when the plane lacks the property or the export fails, wait
    on the CPU for `last_render_ticket`. That path has a counter and a
    `warn` once.
  - Fallback 2: if the CPU wait fails, take the Copy path.

  Implicit fences on Vulkan-exported memory are not attached by anv/RADV for
  non-WSI images, so implicit sync is not assumed.
- **D9 Descriptor owns the fd.** `ServerScanoutDescriptor` owns the exported
  dma-buf fd for the life of that storage generation. The fd is closed after
  the probe framebuffer that imported it is dropped, so field order and
  `Drop` must guarantee it.
- **D10 Generation in the cache key.** Key `ScanoutM1ProbeCache` on
  `(DrawableId, StorageGeneration)` for **all** sources. Imported sources
  stay at generation 0. An entry whose generation does not match the
  drawable's current storage is dead, and it is purged on the next probe for
  that id.
- **D11 Multi-output unchanged.** Keep the one-framebuffer, all-outputs
  commit. The only multi-output change is the fence on every plane. PR #310
  per-output owners stay compatible because the descriptor is per source,
  not per output.

## Tasks

Each task has its own commit. Run `cargo +nightly fmt`,
`cargo clippy --all-targets -- -D warnings` and `cargo test` before each
commit. Lavapipe tests follow the existing `#[ignore]` + live-ICD pattern in
`R/render/engine/tests/mod.rs:709-730` and `B/for_tests.rs:527`
(`promote_and_export_pixmap_for_tests`). A test asserts a red-first
expectation only where the oracle is named.

### Task 1 — carry the real plane offset through export

Probe remark (c).

Changes:

- `ExportableImage::into_raw_parts` (`R/vk/target.rs:1315`) returns
  `offset` too.
- `Storage` (`R/render/store.rs:123`) gains `export_offset: u64`, set by
  `adopt_exportable` (`:350`).
- `promote_drawable_exportable` (`R/render/engine/export.rs:27`, adopt at
  `:112`) passes it through.
- `export_backing` (`R/vk/dri3.rs:412`, `:429`) and `export_promoted`
  (`:443`, `:463`) report the stored offset instead of `0`.
- Callers in `K/export.rs:677-695` pass it.

Tests:

- Lavapipe: promote a pixmap. Assert that `export_promoted(..).offset`
  equals the offset from `vkGetImageSubresourceLayout` on the same image,
  queried in the test (oracle: the driver). Use the same aspect as the
  allocator.
- Existing DRI3 export tests stay green.

No HW check. The fuji probe saw offset 0 both ways, so this is a
correctness fix for drivers that report a nonzero offset.

### Task 2 — storage generation

D10.

Changes:

- `Storage` gets `generation: u64`, taken from a store-wide counter. It is
  bumped by `adopt_exportable` and by any future storage swap; grep for
  writes to `storage.image` in `R/render/store.rs` and `R/render/engine/`.
- `DrawableStore::storage_generation(id)`.
- Key `ScanoutM1ProbeCache.entries` (`K/state.rs:192`) on
  `(DrawableId, u64)`.
- Update every `entries.get/insert/remove`:
  - `K/scanout.rs:2330-2600`;
  - `K/present.rs:898-908`;
  - `note_drawable_retired` (`K/platform_hooks.rs:44`) removes all
    generations of the id.

Tests (recording backend, `B/tests/present_scanout.rs`):

- An entry probed at generation N is not used after the generation is bumped
  (call the store's bump directly; no Vulkan): `framebuffer_ready` is false,
  and the next probe replaces the entry.
- Existing M1/M2 tests (`present_scanout.rs:1212`, `:1892`, `:1991`,
  `:3188`; `atomic_cursor.rs:1230`) stay green.

### Task 3 — scanout descriptor and shared M1 validation

Pure refactor of the imported path.

Changes:

- Add `ScanoutSourceDescriptor` in `K/state.rs`, with fields:
  - `fd: BorrowedFd`-producing owner (an imported dup, or the server-owned
    fd from D9);
  - `fourcc`, `depth`, `bpp`, `width`, `height`, `plane_count`, `offset`,
    `pitch`, `modifier`, `generation`;
  - `origin: Imported | ServerOwned`.
- Split `maybe_probe_scanout_m1` into three parts:
  - **(a)** `scanout_m1_descriptor(source_id) -> Result<Descriptor, M1Reject>`.
    It reads `imported_dmabuf` (today's `:2412-2442`) or the server-owned
    descriptor (Task 6).
  - **(b)** `scanout_m1_validate(&Descriptor, root, &[OutputGeometry], &[modifiers]) -> Result<(), M1Reject>`.
    This is a pure function holding today's checks at `:2464-2477`:
    XRGB8888, one plane, depth 24/bpp 32, size equal to root, outputs tile
    root, and the modifier allowed.
  - **(c)** The probe's `scanout_m1_output_geometry` and
    `scanout_m1_test_only` helpers, lifted from `df3c284b` (`K/scanout.rs`
    diff hunk at `:2327`).

Tests:

- Unit tests of `scanout_m1_validate`, one per reject reason, plus accept.
  The oracle is today's inline checks; the test cases are derived from the
  code being moved, not from new behaviour.
- All existing M1 tests are unchanged.

### Task 4 — telemetry scaffolding

The spec asks for telemetry first, so the counters exist before behaviour.

Add to `ScanoutM0Telemetry` (`K/state.rs:88`), and to the periodic log
(`K/scanout.rs:3030-3079`) and its reset (`:3064-3077`):

- `server_candidates`: eligible-shaped server-owned Presents;
- `server_promote_attempts`, `server_promote_ok`, `server_promote_fail`;
- `server_promote_us_max`;
- `server_reject_exported` (D6), `server_reject_budget` (D3);
- `server_probe_accept`, `server_probe_reject`;
- `presents_direct_imported`, `presents_direct_server`, `presents_copy`;
- `server_fence_explicit`, `server_fence_cpu_wait` (D8).

Log the chosen modifier, pitch and offset once per promotion. Unflip reasons
already log (`unflip_reason` / `unflip_last_reason`). Add a per-reason count
map flushed with the periodic log, so the HW run can report reasons per
interval.

Test: recording backend, counters increment for one imported direct Present
and one Copy Present.

### Task 5 — scanout allocation and promotion in the engine

D4; probe remark (b).

Changes:

- Add `allocate_scanout_pixmap(vk, w, h, format, plane_modifiers)` in
  `R/vk/target.rs`, next to `allocate_exportable` (`:1393`). It reuses
  `allocate_exportable_linear` / `allocate_exportable_modifier` with
  `EXPORT_IMAGE_USAGE`. It does not touch `tfp_tiling_strategy` and returns
  `ExportableImage` (with its offset, Task 1).
- Factor the body of `promote_drawable_exportable`
  (`R/render/engine/export.rs:27-131`) over an allocator closure. Then
  `promote_drawable_exportable` becomes a wrapper over `allocate_exportable`,
  and a new `promote_drawable_for_scanout(gpu, store, id, plane_modifiers)`
  wraps `allocate_scanout_pixmap`.
- The new function marks `Storage.scanout_promoted = true`, bumps the
  generation (Task 2), and keeps the pool exclusion. Promoted storage is
  already never pooled (`R/render/store.rs:~436`); assert that.

Tests (lavapipe):

- Pixels written before promotion read back identically after it. The
  oracle is the bytes written.
- `vk.tfp_tiling_strategy.get()` is `None` both before and after a scanout
  promotion on a fresh context.
- The generation is bumped.
- A RENDER Composite and a CopyArea into the promoted pixmap work afterwards,
  and read back as expected (oracle: the same ops on an unpromoted pixmap).
- With a `plane_modifiers` list that excludes `LINEAR`, promotion returns an
  error, and storage and generation are unchanged.

### Task 6 — candidate tracking, admission and promotion in the backend

D1, D2, D3, D5, D6, D9.

Changes:

- `KmsBackendState` gets `server_scanout: ServerScanoutSources`. Per
  `DrawableId` it records:
  - the generation seen;
  - `eligible_presents: u8`;
  - a state: `Candidate | Promoted(ServerScanoutDescriptor) | Failed(reason)`.

  It also holds the per-epoch failure budget.
- In `observe_scanout_m0` (`K/scanout.rs:2767`), before
  `maybe_probe_scanout_m1` (call at `:2899`), add
  `observe_server_scanout_candidate`. It admits a source only when:
  - it is server-owned (`imported_dmabuf` and `imported_drawable` are both
    `None`) and not exported (D6);
  - depth is 24 and the format is `B8G8R8A8_UNORM`;
  - the extent equals root, and `src_width/height` equal root;
  - the target is `Cow` or `CowDescendant` (D5);
  - `direct_candidate_gate(candidate, target, unbordered, true)` passes
    (`K/present.rs:58`; make it `pub(in backend)` as the probe did). That
    covers the authoritative root, the shape chain covering and not hiding
    root, the unbordered target, zero offsets, no valid region, no
    transform, the CRTC epoch, VT and outputs active, the hardware cursor
    and an empty root overlay;
  - `scanout_m1_outputs_cover_root` holds.
- On the second admitted Present of the same generation (D1), with budget
  left (D3), it calls `promote_drawable_for_scanout`. It then exports
  (`export_promoted` with the offset), builds the descriptor and stores it.
  On any error it records `Failed` for that generation.
- `promote_drawable_for_scanout` is reached through a function pointer in
  `pstate`, like `direct_frame_submitter` (`K/state.rs:248`). Recording
  tests install a fake that bumps the generation and returns a descriptor
  with a memfd. Decision: this fake seam is test-only wiring and not a
  feature switch.
- `note_drawable_retired` drops the entry.
- The DRI3/GLX export handler (`K/export.rs:629-643`) drops the descriptor
  and marks the generation `Failed(exported)` before its own promotion. Its
  `is_exportable()` early return then keeps the scanout storage, which is
  exportable, so no second re-storage happens (spec admission rule 2).
- Feed (a) of Task 3 from `Promoted(desc)`.

Tests (recording backend, fake promoter):

- The first eligible Present does not promote. The second promotes, and the
  same Present's M1 probe runs against the descriptor.
- A third Present does not promote again.
- A depth-32 source, an `Unredirected` target, a non-root extent, a nonzero
  `x_off`, a bordered target and a non-empty root overlay never promote.
- A pixmap exported via DRI3 before or after promotion never flips:
  `server_reject_exported` increments, and the live direct frame unflips with
  `server_source_exported`.
- Four failing promotions exhaust the budget. A fifth candidate is not
  attempted until a topology-epoch change.
- A promoter error leaves the generation and storage untouched (fake asserts
  that), and the Present takes the Copy path.

### Task 7 — server-write fence

D7, D8; spec synchronization point 4.

Changes:

- `DrawableStore` gets a `scanout_fenced: HashSet<DrawableId>`, plus
  `last_write_sync_file: HashMap<DrawableId, Arc<OwnedFd>>`.
  - `touch_render_fence` (`R/render/store.rs:1499`) also pushes ids that are
    in `scanout_fenced`.
  - `engine/frame.rs:444-455` passes them so that
    `flush_submit_group_with_exports` (`R/render/gpu/submit.rs:207`) attaches
    its SYNC_FD signal semaphore (`:289-299`).
  - After submit, export once via `PresentCompletionSignal::export_sync_file_fd`
    (`R/render/gpu/fence.rs:316`) and store it for each such id. Do not
    import it as a dma-buf WRITE fence; that is `exported_sync` only.
- Promotion (Task 6) inserts the id. Retirement and D6 remove it.
- In `backend_present_try_present_direct`, for a `ServerOwned` source:
  1. if the source has queued writes, flush (render batch, open frame,
     submit group — the same sequence as
     `materialize_direct_shadow_for_unflip` `K/scanout.rs:1788-1812`);
  2. dup `last_write_sync_file` into `DirectPresentFrame` (`K/state.rs:220`)
     as `in_fence: Option<OwnedFd>`.

  A queued successor (`queue_direct_successor`, `K/scanout.rs:1529`)
  captures its fence at queue time, not at submit. `None` means signalled.
- `DirectScanoutPlaneState` (`modeset.rs:2003`) gets
  `in_fence_fd: Option<BorrowedFd>`. `submit_direct_scanout`
  (`modeset.rs:2208`) sets `IN_FENCE_FD` on every plane, mirroring the
  composed path (`:2407`). Ownership follows the composed path's
  convention. `submit_direct_frame_live` (`K/scanout.rs:969-980`) fills it
  for every output.
- D8 fallbacks:
  - no `plane_in_fence_fd_prop` on any output, or a failed export: CPU-wait
    `last_render_ticket`, count `server_fence_cpu_wait`, `warn` once;
  - wait error: unflip (`server_source_fence_failed`), Copy path.
- The imported path is unchanged: no fence field set.

Tests:

- Recording atomic sink:
  - a server-owned direct commit sets `IN_FENCE_FD` on every plane, for two
    outputs;
  - an imported direct commit sets none (unchanged);
  - a queued successor carries the fence of its own Present, not a later
    one.
- Unit: with the property absent, the CPU-wait path runs and the counter
  increments.
- Lavapipe: after a write to a `scanout_fenced` drawable and a flush,
  `last_write_sync_file` is `Some`, and it polls readable once the submit's
  fence signals.

### Task 8 — direct flips of server-owned sources

Changes:

- `maybe_probe_scanout_m1`: a `ServerOwned` descriptor no longer counts
  `m1_gate_reject_import`. `reject_server_owned` (`K/scanout.rs:2917`) counts
  only server-owned sources without a descriptor.
- The `framebuffer_ready` lookup (`K/present.rs:898-908`) uses
  `(source_id, generation)`.
- Pins, probation, retirement, Complete `FLIP` and idle-on-successor-retire
  (`K/scanout.rs:2242-2249`, `:1500-1504`) are unchanged.
- Present source wait (`K/present.rs:1002`): a server-owned source yields
  `Ready` as today. The fence is Task 7.

Tests (recording backend, mirroring `present_scanout.rs:1212` and the
tearing helpers `B/tests/tearing.rs:104`, `:477`, `:501`):

- Marco double buffer A/B, both promoted, after 8 probation Presents:
  - each Present returns `Ok(true)` and records no CopyArea and no scene
    compose;
  - A's PresentIdle is emitted only after B's flip retires;
  - Complete mode is `FLIP`.
- A Present while a flip is pending queues as successor (latest wins), as
  imported.
- Imported-source M2 tests are unchanged.

### Task 9 — invalidation and framebuffer lifetime

D9, D10; the #311 rule.

Audit and test every invalidation for server-owned entries. Each drops both
accepted and rejected probe entries; the descriptor fd outlives the
framebuffer.

| Event | Site | Expected |
| --- | --- | --- |
| storage swap | generation bump (Task 2) | old-generation entry dead and purged |
| drawable retired | `K/platform_hooks.rs:44` | entry, descriptor, fence state gone |
| resize / topology | signature, `K/scanout.rs:2358-2373` | cleared, but not while a direct owner is active (#311) |
| modeset / CRTC config | `K/crtc_config.rs:137`, `:1537`, `:1599`; `K/randr.rs:1174` | cleared |
| VT suspend | `K/session.rs:622` | cleared |
| DPMS off | `K/session.rs:1133` | cleared |
| shutdown | `K/session.rs:499` | cleared, fds closed after framebuffers |
| client export | Task 6 (D6) | descriptor dropped, unflip if live |

Decision: there is no clear on VT resume. No probe or flip runs while the VT
is suspended, the suspend already clears, and a topology change on regain
changes the signature.

Tests:

- Recording backend: one per row.
- The #311 case, mirroring `B/tests/commit_identity/direct_unflip.rs`:
  resize during a live server-owned direct frame. The framebuffer and the
  descriptor fd stay valid until the CRTC is replaced, then go.
- The descriptor survives a topology change with no direct owner, but its
  probe entry is cleared and re-probed on the next Present.

### Task 10 — revalidation, fallbacks and the COW shadow

These are existing unflip triggers; prove they hold for server-owned
sources:

- shape (`B/portable/windows.rs:2698`);
- stacking (`:1926`);
- map/unmap/configure (`:2018`, `:2117`, `:2203`);
- cursor (`K/scanout.rs:861-899`, `B/portable/cursor.rs:532`, `:696`, `:736`);
- composite tick: transform, cursor mode, root overlay (`K/scanout.rs:3677-3686`);
- overlay release (`K/platform_hooks.rs:340`).

`materialize_direct_shadow_for_unflip` (`K/scanout.rs:1717`) copies from the
source image; for promoted storage that is a plain Vulkan image.

Tests:

- Recording backend, each with a live server-owned direct frame:
  - COW shape change: unflip with `shape_clips_direct_frame`;
  - cursor leaving hardware: cursor reason;
  - root overlay mapped;
  - panel/menu mapped above the COW (stacking);
  - each test then has the next Present take the Copy path.
- A forced `TEST_ONLY` reject via the recording sink: a negative entry for
  that generation, Copy path, no re-probe until an invalidation.
- Lavapipe: unflip shadow pixels equal the source pixels. The oracle is the
  bytes written to the source.

### Task 11 — hardware acceptance

No commit before this passes (HW smoke rule).

Setup:

- MATE + marco compositing, fullscreen YouTube in Firefox, same video and
  position.
- 10 s windows, captured on Xorg, master (`52ea8822`) and the branch:
  - Present flip/copy counts, with the same capture as the 301/317 baseline;
  - GPU busy at frequency: `intel_gpu_top` on fuji, `amdgpu_top` on silence;
  - yserver CPU (`pidstat`) and marco CPU;
  - dropped frames (Firefox `about:support` or video stats);
  - periodic telemetry (Task 4).

Boxes:

- **fuji** (i915/anv, 1920x1080). Acceptance:
  - at most two promotions;
  - after probation, about one direct flip per video frame;
  - `presents_copy` about 0 and no scene compose per frame;
  - `server_fence_explicit` equal to the direct flips and
    `server_fence_cpu_wait` = 0;
  - no new dropped frames;
  - correct pixels (screenshot through a scanout-correct path, and by eye);
  - GPU busy@freq and server CPU compared with master and Xorg, numbers
    recorded in `docs/status.md`.
- **silence** (RADV, two outputs, root 5120x1440): the same. Both CRTCs flip
  in one commit with a fence on each plane.

Fallback exercises on fuji, each showing a correct composed frame and then a
return to direct:

- COW shape change: marco compositing off/on, and `--replace`;
- cursor fallback: a cursor too big for the hardware plane, or a theme
  change;
- panel, menu and notification over the video;
- resize/modeset (`xrandr --mode` …);
- VT switch out and back;
- forced `TEST_ONLY` failure. Decision: no runtime switch (no kill-switches
  rule). Exercise it on an output whose plane rejects LINEAR, or skip it on
  HW and rely on the Task 10 recording test.

NVIDIA (oldnas GTX 1050), if available: the composed path only, with no
crash, no corruption, and promotion failing or `TEST_ONLY` rejecting within
the D3 budget.

### Task 12 — docs

`docs/status.md`: what shipped, the measured numbers from Task 11, the
non-goals below. No PR body detail beyond symptom/cause/verification.

## Non-goals for v1 and when to revisit

- **NVIDIA / GBM-backed allocation.** Revisit when an NVIDIA user wants
  compositor direct scanout, or Task 11 shows NVIDIA rejecting the linear
  layout. It needs a GBM-first allocation imported into Vulkan with the full
  pixmap usage (spec, Recommendation).
- **Tiled modifiers.** Revisit if a GPU measurably loses on linear render
  cost. The trigger is compositor render GPU time on promoted storage
  clearly above unpromoted on the same box. The fuji probe showed about 9%
  busy×clock, at noise level, while still composing.
- **Depth-32 sources.** They need a proof that scanout equals scene blending
  for alpha-bearing pixels (spec point 5).
- **Unredirected server-owned sources** (D5), and per-output owners (PR
  #310).
- **Asynchronous promotion** (D2).

## Open questions for codex

1. D1: is "second eligible Present" the right reuse proof? Or should
   promotion wait for full M2 probation (8), at the cost of later direct
   entry?
2. D7/D8: is a per-drawable sync_file stored at flush time sound when one
   submit group contains writes to both marco buffers? Both get the same,
   later fence, which is conservative. Or should the fence be per-drawable
   ticket?
3. Task 7 flush in `try_present_direct`: the Copy path already submits. Is
   an extra flush point per direct Present acceptable, or should the direct
   submit move to the frame's flush?
4. D6: on DRI3 export of a live promoted source, is unflip-and-disqualify
   enough? Or must the export also wait for the displayed frame to retire
   before the client can write?
