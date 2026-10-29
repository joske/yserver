# Direct scanout of server-created compositor Present pixmaps — plan

Date: 2026-10-29 (revised after two codex reviews, same day)
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
- `C/` = `crates/yserver-core/src/core_loop/`
- `modeset.rs` = `crates/yserver/src/drm/modeset.rs`

Line numbers are on `52ea8822` (the branch tip only adds docs).

## What exists, what is new

Already there (imported sources):

- M0 classification, `observe_scanout_m0` (`K/scanout.rs:2767`), runs from
  `note_present_scanout_candidate` (`C/process_request/present_ext.rs:1004`),
  before `try_present_direct` (`present_ext.rs:1024`).
- The M1 probe, `maybe_probe_scanout_m1` (`K/scanout.rs:2330`), reads only
  `imported_dmabuf`. A server-owned source counts `m1_gate_reject_import`
  (`:2443-2450`, no cache insert) and `reject_server_owned` (`:2917`).
- The M2 flip, `backend_present_try_present_direct` (`K/present.rs:800`).
  It gates through `direct_candidate_gate` (`K/present.rs:58`) and probation
  (`SCANOUT_M2_ELIGIBLE_ROOT_PROBATION = 8`, `K/state.rs:259`; one global
  streak, not per buffer, `:372-373`), takes the cached probe framebuffer
  (`K/present.rs:898-908`), and pins source and fallback target
  (`:910-911`, `pin_direct_source` `K/scanout.rs:926`). It submits one
  framebuffer to every output in one commit (`submit_direct_frame_live`,
  `K/scanout.rs:956`, framebuffer read from the M1 entry at `:961-968`) and
  retires it in `retire_direct_output` (`K/scanout.rs:2140`). Retirement
  emits Complete `FLIP` and idles the previous frame
  (`release_direct_frame`, `:1500`).
- Unflip: `request_direct_unflip` (`K/scanout.rs:429`) only sets
  `unflip_requested`; the source is released by
  `stop_direct_after_scanout_replaced` (`:1556`, `current.take()` at `:1588`)
  once the replacing commit retires (`:2181`). The lazy COW shadow is
  `materialize_direct_shadow_for_unflip` (`K/scanout.rs:1717`).
- `direct_frame_references_host_drawable` (`K/scanout.rs:441`) is true while
  `pending`, `queued_successor` or `current` names the drawable.
- #311: the probe cache is not cleared on a topology change while a direct
  owner is active (`K/scanout.rs:2359-2368`). The live framebuffer *is* the
  M1 cache entry's framebuffer, so an entry must not be dropped while a CRTC
  scans it (`:1656-1675`).
- Parked backend requests: `RequestOutcome::PendingCrtcConfig`
  (`C/process_request.rs:202`) withholds a client's credit and parks its
  later requests; `PendingBackendRequests` (`C/run.rs:600`) indexes them.

New:

1. A scanout descriptor for server-owned storage.
2. A candidate scanout allocation, independent of the GLX TFP strategy,
   that is exported and `TEST_ONLY`-probed before it replaces any storage.
3. Candidate tracking, once-per-drawable promotion, failure and success caps.
4. A deferred DRI3 export reply while a promoted source is on a CRTC.
5. Explicit write tracking and `IN_FENCE_FD` for server writes on direct
   plane commits. Today `DirectScanoutPlaneState` (`modeset.rs:2003`)
   carries no fence, and the source wait (`K/present.rs:1002`) only covers
   imported fds.
6. Telemetry.

## Decisions

Each is also marked inline in the task that implements it. Codex reviewed
two drafts; its answers are folded in below.

- **D1 Promotion trigger: per rotation, not per drawable.** A *rotation* is
  keyed by the Present destination (the COW or COW descendant) and holds the
  distinct eligible source ids seen on it, in order, at most two (A, or A
  and B). It is *identified* once every member has had a second eligible
  Present with no third source in between; marco's double buffer is
  identified at its fourth Present (A, B, A, B). Identification reserves
  one token per member (D3) up front, then promotes every not-yet-promoted
  member in that same call, so A is never promoted without B being
  attempted. Server-owned sources on that destination may flip (M2) only
  while the rotation is *complete*: every member `Promoted` and probed. If a
  member fails after another succeeded, the successful swap cannot be
  undone: that member stays `Promoted`, the rotation is `Failed`, every
  Present on the destination stays Copy, and the unused reservation is
  returned. A third source on the destination resets the rotation (unflip if
  direct; members that are already `Promoted` need no new token). It is not
  a general amortization guarantee: M2 probation is one global streak, so it
  does not prove per-buffer reuse. The caps in D3 bound what a client can
  make us pay. Cost: three to four composed frames at startup.
- **D2 Synchronous promotion.** Keep promotion synchronous, inside
  `observe_scanout_m0` and before `maybe_probe_scanout_m1`. The same Present
  then flips eligibility, and its Copy reads the new storage (probe remark
  (a)). One-time per buffer (measured 6–8 ms fuji, 0.5–0.8 ms silence).
  Task 12 measures the startup hitch; revisit if it is visible.
- **D3 Caps.** A failed allocation, export, `AddFB2`, `TEST_ONLY` or copy
  marks that drawable `Failed`; it is never retried. Three caps, each with a
  reject counter:
  - failures: at most 4 per topology epoch, then no attempts until the next
    topology change;
  - successes: a rate limit, not a per-epoch count. A token bucket with a
    burst of 8 (marco's two buffers, with room for compositor restarts)
    refills one token per 30 s (`SERVER_SCANOUT_PROMOTE_REFILL`). A per-epoch
    count would end a healthy long session on the composed path; the bucket
    still bounds a restart loop to one 6–8 ms promotion per 30 s. Tokens
    are reserved per rotation at identification (D1), one per member not yet
    promoted: a rotation whose reservation cannot be met stays on Copy and
    spends nothing. One token for a two-buffer rotation would leave it half
    promoted, so A flips while B's Present requests an unflip;
  - live: at most 4 promoted drawables alive at once (bounds the memory of
    non-pooled linear root-sized storage).

  That bounds repeated synchronous stalls on a driver that cannot do it and
  on a client that keeps cycling root pixmaps.
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
- **D6 External export disqualifies for good.** The server cannot know when
  a client closes an fd, so any handoff permanently disqualifies the
  drawable (state `Disqualified`, never cleared while the drawable lives):
  - candidates are admitted only when `!storage.is_exportable()`,
    `!store.is_exported(id)` (`R/render/store.rs:953`) and there is no
    `pstate.exported_dmabufs` entry. That excludes GLX-bound
    (`promote_pixmap_exportable`, `K/export.rs:240`) and GLX-acquired
    (`acquire_glx_pixmap_export`, `:179`) pixmaps too;
  - on a promoted drawable, GLX bind/acquire and DRI3 `BufferFromPixmap` /
    `BuffersFromPixmap` (`C/process_request/dri3.rs:330`, `:407`) mark it
    `Disqualified` and `request_direct_unflip("server_source_exported")`;
  - from then on `framebuffer_ready` (`K/present.rs:898-908`) is false for
    it, so no later flip, successor or async flip can use the cached
    framebuffer;
  - **only the DRI3 replies hand out an fd, so only they wait.** While
    `direct_frame_references_host_drawable` is true, the reply is deferred:
    the client parks as for `PendingCrtcConfig`. It is sent once the
    replacing commit has retired on every CRTC and the frame is released
    (`stop_direct_after_scanout_replaced`). Then the M1 entry (framebuffer)
    and the descriptor are dropped, framebuffer first (D9). GLX bind and
    acquire hand out no fd and do not wait;
  - **the parked export pins its `DrawableId` and storage.** Same-client
    FIFO does not stop another client from freeing the pixmap while the
    reply waits. The pin is taken at park time and released after the
    reply, the error or the cancellation; completion never resolves the
    XID again;
  - **the wait has a deadline,** `PER_CRTC_EVENT_WAIT` (250 ms,
    `K/crtc_config.rs:1261`, the per-CRTC RANDR event wait) as the starting
    threshold. On expiry the backend runs the established safe teardown,
    `teardown_direct_before_topology_requery` (`K/scanout.rs:1656`: COW
    shadow, every CRTC off, `stop_direct_after_scanout_replaced`, M1 clear)
    and `relight_after_direct_teardown` (`:1629`). The fd is sent only after
    every plane has left the source. If the teardown fails, the reply is
    the existing export-failure error, `BadPixmap`
    (`C/process_request/dri3.rs:349-367` for op 3, `:435-449` for op 8,
    after Xorg `dri3_request.c:277`), the client's credit is released, and
    the live framebuffer and source pins are kept for recovery (that
    function already keeps them on failure; both helpers also
    `request_exit()` on failure today, which stays). A timeout alone never
    makes the fd safe to hand out.
- **D7 Separate write registry.** Promoted scanout storage joins the flush
  chokepoint's signal-semaphore path through its own set, not through
  `exported_sync`. That keeps `is_exported` meaning "a client holds an fd"
  (D6), and avoids publishing implicit WRITE fences nobody reads. Writes are
  tracked explicitly: `touch_render_fence` (`R/render/store.rs:1499`) also
  stamps read sources (e.g. `R/render/engine/fill_copy.rs:556`,
  `batch.rs:411`, `scene/transform.rs:293`), so it cannot say "written".
- **D8 Explicit fence.** Every direct plane commit of a server-owned source
  carries `IN_FENCE_FD` from the sync_file of the last submit that wrote it.
  KMS defines `IN_FENCE_FD` for exactly this dependency, and Vulkan external
  memory carries no synchronization, so implicit sync is not relied on. One
  submit group's sync_file may serve every scanout source that group wrote.
  The fence state is tri-state per drawable:
  - `NoPendingWrite`: no write since promotion (its copy waited on the CPU);
  - `Fence(Arc<OwnedFd>)`: the last writing submit's sync_file;
  - `ExportFailed`: the last writing submit produced no sync_file.

  Fallbacks:
  - `ExportFailed`, or no `plane_in_fence_fd_prop` on an output: wait on the
    CPU for `last_render_ticket` (conservative: it may name a later read).
    Counted, `warn` once;
  - CPU wait error: unflip (`server_source_fence_failed`), Copy path.
- **D9 Descriptor owns the fd.** `ServerScanoutDescriptor` owns the exported
  dma-buf fd of the promoted storage. The fd is closed after the probe
  framebuffer that imported it is dropped, so field order and `Drop` must
  guarantee it.
- **D10 No storage generation; one-swap invariant instead.** The probe cache
  stays keyed on `DrawableId`. Codex's premises hold in the code:
  - M1 caches no accepted entry for a server-owned source: without
    `imported_dmabuf` it returns at `K/scanout.rs:2443-2450` without an
    insert. The only reachable insert for such a source is a *rejected*
    entry when outputs do not tile root (`:2397-2410`), which also blocks
    admission;
  - storage swaps happen only in `Storage::adopt_exportable`
    (`R/render/store.rs:353`), whose only caller is
    `promote_drawable_exportable` (`R/render/engine/export.rs:112`), which
    returns early on `is_exportable()` (`:37-42`); `adopt_exportable` sets
    `promoted_exportable` (`store.rs:375`), so `is_exportable()` stays true.

  **Invariant:** a drawable's storage is swapped at most once, from
  non-exportable to exportable. Scanout promotion admits only
  `!is_exportable()` sources and goes through `adopt_exportable`, so once a
  drawable is promoted (and therefore while it can own a CRTC) no swap is
  possible: GLX bind and DRI3 export hit the early return. Enforcement:
  the `is_exportable()` admission check (Task 6), the early return above,
  and a `debug_assert!(!self.promoted_exportable)` in `adopt_exportable`
  (Task 2). Promotion explicitly removes any (rejected) entry for the id
  and inserts its own accepted one. A future second swap (e.g. tiled
  re-promotion) must reintroduce a generation key, and its old framebuffer
  and storage must survive until every CRTC has left them.
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
  `offset` (the field exists, `:1292`).
- `Storage` (`R/render/store.rs:123`) gains `export_offset: u64`, set by
  `adopt_exportable` (`:353`).
- `promote_drawable_exportable` (`R/render/engine/export.rs:27`, adopt at
  `:112`) passes it through.
- `export_backing` (`R/vk/dri3.rs:412`, `:429`) and `export_promoted`
  (`:443`, `:463`) report the real offset instead of `0`.
- Callers in `K/export.rs:677-695` pass it.

Tests:

- Lavapipe: promote a pixmap. Assert that `export_promoted(..).offset`
  equals the offset from `vkGetImageSubresourceLayout` on the same image,
  queried in the test (oracle: the driver). Use the same aspect as the
  allocator.
- Existing DRI3 export tests stay green.

No HW check. The fuji probe saw offset 0 both ways, so this is a
correctness fix for drivers that report a nonzero offset.

### Task 2 — one-swap invariant

D10.

Changes:

- `debug_assert!(!self.promoted_exportable)` at the top of
  `Storage::adopt_exportable` (`R/render/store.rs:353`), with a comment
  naming the M1 cache as the dependent.
- `ScanoutM1ProbeCache` (`K/state.rs:192`) doc comment states the invariant.

Tests:

- Unit (`Storage::for_tests_null`): a second `adopt_exportable` panics in
  debug builds.
- Lavapipe: `promote_drawable_exportable` on an already-promoted drawable
  returns `Ok` and leaves `image`/`memory` unchanged (oracle: the handles
  before the call).

### Task 3 — scanout descriptor and shared M1 validation

Pure refactor of the imported path.

Changes:

- Add `ScanoutSourceDescriptor` in `K/state.rs`, with fields:
  - `fd: BorrowedFd`-producing owner (an imported dup, or the server-owned
    fd from D9);
  - `fourcc`, `depth`, `bpp`, `width`, `height`, `plane_count`, `offset`,
    `pitch`, `modifier`;
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
    diff hunk at `:2327`). Task 6 calls (b) and (c) on a candidate before
    any storage swap.

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
- `server_promote_us_max`, and `server_promote_present_us_max` (receipt to
  Copy submit of the Present that promoted; the startup-hitch number);
- `server_reject_exported` (D6), `server_reject_cap_fail`,
  `server_reject_cap_success`, `server_reject_cap_live` (D3);
- `server_export_deferred`, `server_export_defer_us_max`,
  `server_export_deadline_teardown`, `server_export_deadline_fail` (D6);
- `server_probe_accept`, `server_probe_reject`;
- `presents_direct_imported`, `presents_direct_server`, `presents_copy`;
- `server_fence_explicit`, `server_fence_none`, `server_fence_cpu_wait` (D8);
- `server_direct_flush_added`, `server_direct_flush_reused` (Task 8).

Log the chosen modifier, pitch and offset once per promotion. Unflip reasons
already log (`unflip_reason` / `unflip_last_reason`). Add a per-reason count
map flushed with the periodic log, so the HW run can report reasons per
interval.

Test: recording backend, counters increment for one imported direct Present
and one Copy Present.

### Task 5 — candidate allocation and guarded promotion in the engine

D4, D10; probe remark (b). The swap happens only after the candidate has
passed export, `AddFB2` and `TEST_ONLY` (codex review point 2).

Changes:

- Add `allocate_scanout_pixmap(vk, w, h, format, plane_modifiers)` in
  `R/vk/target.rs`, next to `allocate_exportable` (`:1393`). It reuses
  `allocate_exportable_linear` / `allocate_exportable_modifier` with
  `EXPORT_IMAGE_USAGE`, does not touch `tfp_tiling_strategy`, and returns
  `ExportableImage` (with its offset, Task 1).
- Factor the post-allocation tail of `promote_drawable_exportable`
  (`R/render/engine/export.rs:56-130`: frame close, flush, blocking copy,
  views, `adopt_exportable`, view invalidation, retire) into
  `adopt_candidate(gpu, store, id, ExportableImage)`.
  `promote_drawable_exportable` becomes allocate + `adopt_candidate`.
- New `promote_drawable_for_scanout(gpu, store, id, plane_modifiers, probe)`,
  in order:
  1. refuse (`Err`, nothing allocated) if `is_exportable()`;
  2. `allocate_scanout_pixmap`;
  3. `export_backing` on the candidate (`R/vk/dri3.rs:412`), giving the fd;
  4. `probe(&CandidateLayout { fd, offset, pitch, modifier, .. })`, a
     backend closure that runs validation, `AddFB2` and `TEST_ONLY`
     (Task 6) and returns the accepted framebuffer;
  5. only on `Ok`: `adopt_candidate`. Return the fd and the probe result.

  An error in 2–4 drops the candidate (`ExportableImage::Drop`) and never
  touches the drawable: no frame close, no flush, no generation of any
  kind. An error inside `adopt_candidate` before `adopt_exportable` drops
  the framebuffer, then the fd, then the image, and the storage is
  likewise untouched.
- Promoted storage is never pooled (`!self.promoted_exportable` guard in
  `Storage::destroy`, `R/render/store.rs:444`); assert it in the test.

Tests (lavapipe):

- Pixels written before promotion read back identically after it. The
  oracle is the bytes written.
- `vk.tfp_tiling_strategy.get()` is `None` both before and after a scanout
  promotion on a fresh context.
- A RENDER Composite and a CopyArea into the promoted pixmap work afterwards,
  and read back as expected (oracle: the same ops on an unpromoted pixmap).
- A failing `probe` closure: `Err`, `image`/`memory`/`current_layout`
  unchanged, `is_exportable()` false, the submit-group flush count
  unchanged (no boundary taken), and pixels unchanged.
- A `plane_modifiers` list that excludes `LINEAR`: `Err` before the probe
  closure runs; storage unchanged.
- An already-exportable drawable: `Err`, nothing allocated.

### Task 6 — candidate tracking, admission and promotion in the backend

D1, D2, D3, D5, D6 (admission), D9.

Changes:

- `KmsBackendState` gets `server_scanout: ServerScanoutSources`. Per
  `DrawableId` it records `eligible_presents: u8` and a state:
  `Candidate | Promoted(ServerScanoutDescriptor) | Failed(reason) |
  Disqualified`. Per Present destination (window id) it records a
  `Rotation { members: ArrayVec<DrawableId, 2>, state: Observing |
  Complete | Failed }` (D1). It also holds the D3 state: failures per
  topology epoch, the success token bucket (its methods take `now:
  Instant`, so tests drive time, and it has `reserve(n)` / `refund(n)`) and
  the live count.
- In `observe_scanout_m0` (`K/scanout.rs:2767`), before
  `maybe_probe_scanout_m1` (call at `:2899`), add
  `observe_server_scanout_candidate`. It admits a source only when:
  - it is server-owned (`imported_dmabuf` and `imported_drawable` are both
    `None`), `!is_exportable()`, not exported and without an
    `exported_dmabufs` entry (D6, D10);
  - depth is 24 and the format is `B8G8R8A8_UNORM`;
  - the extent equals root, and `src_width/height` equal root;
  - the target is `Cow` or `CowDescendant` (D5);
  - `direct_candidate_gate(candidate, target, unbordered, true)` passes
    (`K/present.rs:58`; make it `pub(in backend)` as the probe did). That
    covers the authoritative root, the shape chain covering and not hiding
    root, the unbordered target, zero offsets, no valid region, no
    transform, the CRTC epoch, VT and outputs active, the hardware cursor
    and an empty root overlay;
  - `scanout_m1_outputs_cover_root` (`K/scanout.rs:289`) holds.
- Each admitted Present updates the destination's rotation (D1). When the
  rotation is identified and the failure and live caps are open (D3), it
  reserves one token per member not yet `Promoted`; if the bucket cannot
  cover them all, nothing is promoted and the Present stays Copy. Otherwise
  it calls `promote_drawable_for_scanout` for each such member in turn, with
  a probe closure that builds the descriptor and runs
  `scanout_m1_validate` + `scanout_m1_test_only` (Task 3). On `Ok` it
  removes any M1 entry for the id, inserts the accepted entry with the
  returned framebuffer and the current topology signature, and stores
  `Promoted(desc)`. On `Err` it records `Failed`, marks the rotation
  `Failed` and refunds the reservations not yet spent. The rotation is
  `Complete` only when every member is `Promoted`.
- Task 9's admission of a server-owned source requires its destination's
  rotation to be `Complete`; a `Promoted` member of an `Observing` or
  `Failed` rotation takes the Copy path.
- Invariant: when a member is exported (Task 7) or retired, its rotation
  leaves `Complete` immediately and requests an unflip, so a surviving
  member never alternates between direct flips and Copy. When the
  destination window is retired, its rotation is removed.
- `promote_drawable_for_scanout` is reached through a function pointer in
  `pstate`, like `direct_frame_submitter` (`K/state.rs:248`). Recording
  tests install a fake that returns a descriptor with a memfd, or an error.
  Decision: this fake seam is test-only wiring and not a feature switch.
- `note_drawable_retired` (`K/platform_hooks.rs:44`) drops the entry and
  decrements the live count.
- Feed (a) of Task 3 from `Promoted(desc)` only; any other state yields no
  descriptor.

Tests (recording backend, fake promoter):

- A single-buffer rotation (A, A): the first eligible Present does not
  promote; the second promotes, and that Present's M1 entry is the
  accepted one from promotion. A third Present does not promote again.
- An A/B rotation (A, B, A, B): nothing promotes before the fourth
  Present, which promotes both A and B and spends two tokens; from then on
  A and B alternate direct flips with no unflip.
- Second-buffer failure: the fake promoter succeeds for A and fails for B.
  A stays `Promoted`, B is `Failed`, the rotation is `Failed`, one token is
  spent and one refunded, every later Present on the destination takes
  Copy, and no flip or unflip is ever requested.
- A third source C on the destination resets the rotation and, if direct,
  requests an unflip; a new rotation including an already `Promoted`
  member reserves tokens only for the others.
- A depth-32 source, an `Unredirected` target, a non-root extent, a nonzero
  `x_off`, a bordered target, a non-empty root overlay, a GLX-bound and a
  DRI3-exported pixmap never promote.
- Four failing promotions exhaust the failure cap: a fifth candidate is not
  attempted until a topology-epoch change.
- Eight successes in a burst exhaust the success bucket; the ninth is
  refused until 30 s of test time refill one token, and a topology change
  does not refill it. With one token available, an alternating A/B
  rotation promotes neither buffer and every Present stays on Copy; after a
  second token refills, both promote and A/B alternate direct flips with no
  unflip. Five live promoted drawables are refused by the live
  cap, and freeing one reopens it.
- A promoter error leaves storage untouched (the fake asserts no flush and
  no swap) and the Present takes the Copy path.

### Task 7 — export disqualification and deferred DRI3 reply

D6 (codex review point 1).

Changes:

- `KmsBackend::disqualify_server_scanout(id, reason)`: sets
  `Disqualified`, requests unflip if the source is referenced by a direct
  frame, and counts `server_reject_exported`. Called from GLX bind
  (`promote_pixmap_exportable`, `K/export.rs:240`), GLX acquire
  (`acquire_glx_pixmap_export`, `:179`) and the DRI3 export path
  (`backend_export_dri3_export_pixmap_buffers`, `:618`, which op 3 shares).
  Each call happens before the handler's own idempotent promotion.
- `framebuffer_ready` (`K/present.rs:898-908`) and `submit_direct_frame_live`
  (`K/scanout.rs:961-968`) treat a server-owned source as ready only in
  `Promoted`.
- Deferred reply: the DRI3 export of a drawable for which
  `direct_frame_references_host_drawable` is true returns a new
  `RequestOutcome::PendingDri3Export(token)`, parked in
  `PendingBackendRequests` (`C/run.rs:600`) like `PendingCrtcConfig`
  (same-client FIFO, credit withheld, cancelled on disconnect). The backend
  completes the token after `stop_direct_after_scanout_replaced` releases
  the frame; completion builds the export (fd, offset, modifier) and the
  core sends the op 3 or op 8 reply. Then the M1 entry is removed,
  framebuffer before descriptor fd (D9).
- Pin (D6): at park time the backend resolves the host XID once
  (`store.lookup`) and takes a source pin as `pin_direct_source` does
  (`K/scanout.rs:926`: `store.incref` + `present_source_pins`). The token
  carries the `DrawableId` and the pin id; completion exports from that id
  and never calls `lookup` again. The pin is released through
  `backend_present_release_present_source`
  (`B/portable/present.rs:190`, `store_decref_with_invalidate`) after the
  reply or error is sent, or on cancellation.
- Deadline (D6): the token records `parked_at + PER_CRTC_EVENT_WAIT`, and
  the earliest token deadline joins `next_wakeup`
  (`K/platform_hooks.rs:424`). On expiry, still referenced: count
  `server_export_deadline_teardown`, run
  `teardown_direct_before_topology_requery("server_source_export_deadline")`
  then `relight_after_direct_teardown`, and complete the token with the fd
  once `stop_direct_after_scanout_replaced` has run. If the teardown fails:
  complete with `BadPixmap` (`C/process_request/dri3.rs:349-367`,
  `:435-449`), count `server_export_deadline_fail`, release the credit and
  the token's pin, and keep the frame's framebuffer and source pins. If the
  teardown succeeds but relight fails, the teardown has already released
  the direct frame and its pins, so the source is off every plane: complete
  the token with the fd.
- An unreferenced drawable is answered immediately, and its M1 entry and
  descriptor are dropped in the same call.

Tests:

- Recording backend:
  - a DRI3 export of the live direct source: no reply while `current` holds
    it; unflip requested with `server_source_exported`; after the recording
    sink retires the composed replacement on every output, exactly one
    reply, and the M1 entry is gone;
  - two outputs: the reply waits for both CRTCs;
  - while parked, a later request from the same client is not dispatched;
  - a Present of the disqualified drawable after the reply takes the Copy
    path, and no successor queued before the export flips it;
  - GLX bind and acquire on a promoted drawable disqualify without parking;
  - the client disconnecting while parked cancels the token and releases
    its pin;
  - another client frees the pixmap while parked: the reply still exports
    the pinned storage, and the store entry is freed only after the pin is
    released (oracle: store refcount);
  - deadline: a recording sink that never retires the replacement; at
    `parked_at + PER_CRTC_EVENT_WAIT` the teardown runs, every plane is
    off before the reply, and exactly one reply carries the fd;
  - deadline with a failing all-off commit: one `BadPixmap` error, the
    credit released (the next request dispatches), the frame's framebuffer
    and source pins still held.
- The existing DRI3 export tests stay green.

### Task 8 — server-write tracking and fence

D7, D8; spec synchronization point 4.

Changes:

- `DrawableStore::touch_render_write(id, ticket)`: calls
  `touch_render_fence` and, for ids in `scanout_fenced`, sets
  `unsubmitted_write` and queues the id in `scanout_writes`. Destination
  stamps switch to it: `fill_copy.rs:138`, `:388`, `:554`, `:776`;
  `batch.rs:410`, `:557`; `traps.rs:474`; `glyphs.rs:363`, `:494`;
  `text.rs:250`; `composite.rs:623`; `put_get.rs:171` (all under
  `R/render/engine/`). Source, mask and sampled stamps stay
  `touch_render_fence`. Each site is checked against its own op before it
  switches; the list is the audit start, not the result. The scene stamps
  are reads (sampled sources) and stay: `scene/tick.rs:1333`, `:1418`;
  `scene/transform.rs:293`; `scene/root_readback.rs:171`;
  `composite.rs:789`, `:874` (codex found no missed write site).
- Write classifier for recorded ops:
  `RecordedOp::written_drawable(&self) -> Option<DrawableId>`
  (`R/render/frame_builder.rs`, next to `dst_id`, `:1116`), an exhaustive
  `match` with no wildcard arm, so a new variant does not compile until it
  is classified. `dst_id()` is not a write classifier:
  `ClipSnapshotRefresh` returns `live_mask_id` (`:1129`), which it reads
  (it writes the snapshot image, not a drawable). Classification:
  `ClipSnapshotRefresh` and `GlyphUpload` (atlas) write no drawable;
  `LayoutTransition` writes its `drawable_id` (a layout transition is a
  write access, which Vulkan allows to read and write bound memory; the
  variant is only constructed in a test today, so this is a conservative
  rule for the future); every other variant writes its `dst_id`. The check
  runs in `close_open_frame` (`R/render/engine/frame.rs:525`), which still
  owns `open_frame.ops`, before the flush: a `debug_assert!` that every
  op's written id that is in `scanout_fenced` was stamped by
  `touch_render_write` (is in `scanout_writes`). `flush_submit_group` cannot
  see the ops.
- `scanout_fenced: HashSet<DrawableId>`, and
  `scanout_write_fence: HashMap<DrawableId, ScanoutWriteFence>` with the D8
  tri-state. Promotion (Task 6) inserts `NoPendingWrite`; retirement and
  D6 remove the id.
- `engine/frame.rs:443-455`: when the group is non-empty, take
  `scanout_writes` as well, and pass a "signal wanted" flag so that
  `flush_submit_group_with_exports` (`R/render/gpu/submit.rs:207`) creates
  its SYNC_FD signal semaphore (`:289-299`) even with no `exported_sync`
  writes. After submit, export it once
  (`PresentCompletionSignal::export_sync_file_fd`, `R/render/gpu/fence.rs:316`)
  and set every written id to `Fence(arc)`, or `ExportFailed` if the
  semaphore or the export failed. Clear `unsubmitted_write`. Do not import
  it as a dma-buf WRITE fence; that is `exported_sync` only.
- Ordering: in `backend_present_try_present_direct`, for a `ServerOwned`
  source, before any direct submit or successor queueing:
  1. if `unsubmitted_write`, flush (render batch, open frame, submit
     group — the sequence in `materialize_direct_shadow_for_unflip`,
     `K/scanout.rs:1788-1812`) and count `server_direct_flush_added`;
     otherwise count `server_direct_flush_reused`. This counter answers
     whether marco's render boundary already submitted the writes;
  2. map the state into `DirectPresentFrame` (`K/state.rs:220`) as
     `in_fence: Option<OwnedFd>`: `Fence` dups the fd; `NoPendingWrite`
     is `None` (counted `server_fence_none`); `ExportFailed` takes the CPU
     wait.

  A queued successor (`queue_direct_successor`, `K/scanout.rs:1529`)
  captures its fence at queue time.
- `DirectScanoutPlaneState` (`modeset.rs:2003`) gets
  `in_fence_fd: Option<BorrowedFd>`. `submit_direct_scanout`
  (`modeset.rs:2208`) sets `IN_FENCE_FD` on every plane, mirroring the
  composed path (`:2407-2412`). Ownership follows the composed path's
  convention. `submit_direct_frame_live` (`K/scanout.rs:969-980`) fills it
  for every output.
- D8 fallbacks: `ExportFailed` or a missing `plane_in_fence_fd_prop`: CPU
  wait on `last_render_ticket`, count `server_fence_cpu_wait`, `warn` once;
  a wait error unflips (`server_source_fence_failed`) and takes the Copy
  path.
- The imported path is unchanged: no fence field set.

Tests:

- Unit (store): a drawable only read (CopyArea source, Composite source and
  mask) gets no write; the destination does.
- Unit (`frame_builder.rs`), mirroring
  `b3_recorded_op_dst_id_covers_every_variant` (`:1439`): one case per
  variant for `written_drawable`; for `ClipSnapshotRefresh` it is `None`
  while `dst_id()` is `Some(live_mask_id)`.
- Recording atomic sink:
  - a server-owned direct commit sets `IN_FENCE_FD` on every plane, for two
    outputs;
  - `NoPendingWrite` sets none; `ExportFailed` CPU-waits and counts;
  - the flush in step 1 is recorded before the atomic commit, and only when
    a write was unsubmitted (`flush_added` vs `flush_reused`);
  - an imported direct commit sets none (unchanged);
  - a queued successor carries the fence of its own Present, not a later
    one.
- Unit: with the property absent, the CPU-wait path runs and the counter
  increments.
- Lavapipe: one submit group writing two `scanout_fenced` drawables gives
  both the same `Fence`, which polls readable once the submit signals.

### Task 9 — direct flips of server-owned sources

Changes:

- `maybe_probe_scanout_m1`: a `ServerOwned` descriptor no longer counts
  `m1_gate_reject_import`. `reject_server_owned` (`K/scanout.rs:2917`) counts
  only server-owned sources without a descriptor. A promoted drawable whose
  entry was cleared (topology, modeset) re-probes from its descriptor.
- Pins, probation, retirement, Complete `FLIP` and idle-on-successor-retire
  (`K/scanout.rs:2242-2249`, `:1500-1504`) are unchanged.
- Present source wait (`K/present.rs:1002`): a server-owned source yields
  `Ready` as today. The fence is Task 8.

Tests (recording backend, mirroring `present_scanout.rs:1212` and the
tearing helpers `B/tests/tearing.rs:104`, `:477`, `:501`):

- Marco double buffer A/B, both promoted, after 8 probation Presents:
  - each Present returns `Ok(true)` and records no CopyArea and no scene
    compose;
  - A's PresentIdle is emitted only after B's flip retires;
  - Complete mode is `FLIP`.
- A Present while a flip is pending queues as successor (latest wins), as
  imported.
- Imported-source M2 tests (`present_scanout.rs:1212`, `:1892`, `:1991`,
  `:3188`; `atomic_cursor.rs:1230`) are unchanged.

### Task 10 — invalidation and framebuffer lifetime

D9, D10; the #311 rule.

Audit and test every invalidation for server-owned entries. Each drops both
accepted and rejected probe entries; the descriptor fd outlives the
framebuffer; the descriptor itself survives everything except retirement
and D6.

| Event | Site | Expected |
| --- | --- | --- |
| promotion | Task 6 | prior (rejected) entry replaced by the accepted one |
| drawable retired | `K/platform_hooks.rs:44` | entry, descriptor, fence state gone |
| resize / topology | signature, `K/scanout.rs:2358-2373` | cleared, but not while a direct owner is active (#311) |
| modeset / CRTC config | `K/crtc_config.rs:137`, `:1537`, `:1599`; `K/randr.rs:1174` | cleared |
| VT suspend | `K/session.rs:622` | cleared |
| DPMS off | `K/session.rs:1133` | cleared |
| shutdown | `K/session.rs:499` | cleared, fds closed after framebuffers |
| client export | Task 7 (D6) | flips refused at once; entry and descriptor dropped after the CRTCs leave |

Decision: there is no clear on VT resume. No probe or flip runs while the VT
is suspended, the suspend already clears, and a topology change on regain
changes the signature.

Tests:

- Recording backend: one per row.
- The #311 case, mirroring `B/tests/commit_identity/direct_unflip.rs`:
  resize during a live server-owned direct frame. The framebuffer and the
  descriptor fd stay valid until the CRTC is replaced, then the entry goes
  and the descriptor stays.
- After a topology change with no direct owner, the next Present re-probes
  from the descriptor without promoting again.

### Task 11 — revalidation, fallbacks and the COW shadow

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
- A forced `TEST_ONLY` reject on re-probe via the recording sink: a
  negative entry, Copy path, no re-probe until an invalidation.
- Lavapipe: unflip shadow pixels equal the source pixels. The oracle is the
  bytes written to the source.

### Task 12 — hardware acceptance

No commit before this passes (HW smoke rule).

Setup:

- MATE + marco compositing, fullscreen YouTube in Firefox, same video and
  position.
- 10 s windows, captured on Xorg, master (`52ea8822`) and the branch:
  - Present flip/copy/skip counts on the COW and root with
    `tools/present-watch.c` (counts `PresentCompleteNotify` modes for N
    seconds), the same capture as the 301/317 baseline;
  - GPU busy at frequency: `intel_gpu_top` on fuji, `amdgpu_top` on silence;
  - yserver CPU (`pidstat`) and marco CPU;
  - dropped frames (Firefox `about:support` or video stats);
  - periodic telemetry (Task 4).
- Startup hitch (D2): with marco already composing, start the video and
  also restart marco (`marco --replace`). Record `server_promote_us_max`,
  `server_promote_present_us_max`, and the Present-to-Complete gap of the
  promoting Present from `tools/present-watch.c`, against the median
  interval. Dropped frames in the first seconds are recorded separately.

Boxes:

- **fuji** (i915/anv, 1920x1080). Acceptance:
  - at most two promotions per marco start;
  - after probation, about one direct flip per video frame;
  - `presents_copy` about 0 and no scene compose per frame;
  - `server_fence_explicit` + `server_fence_none` equal to the direct flips
    and `server_fence_cpu_wait` = 0;
  - `server_direct_flush_added` vs `_reused` recorded (no threshold; it
    tells whether direct Presents add a submit);
  - no new dropped frames, and no visible startup hitch;
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
  HW and rely on the Task 11 recording test.

NVIDIA (oldnas GTX 1050), if available. Linear allocation and `TEST_ONLY`
may succeed, so rejection is not assumed:

- if promotion fails or `TEST_ONLY` rejects: the composed path, no crash,
  no corruption, attempts within the D3 failure cap;
- if both succeed: verify pixels and performance exactly as on fuji. If
  either is wrong, exclude the driver explicitly (a named driver check in
  admission, with a counter and a `docs/status.md` entry), not by relying
  on a probe failure.

### Task 13 — docs

`docs/status.md`: what shipped, the measured numbers from Task 12, the
non-goals below. No PR body detail beyond symptom/cause/verification.

## Non-goals for v1 and when to revisit

- **NVIDIA / GBM-backed allocation.** Revisit when an NVIDIA user wants
  compositor direct scanout, or Task 12 excludes or rejects NVIDIA. It needs
  a GBM-first allocation imported into Vulkan with the full pixmap usage
  (spec, Recommendation).
- **Tiled modifiers.** Revisit if a GPU measurably loses on linear render
  cost. The trigger is compositor render GPU time on promoted storage
  clearly above unpromoted on the same box. The fuji probe showed about 9%
  busy×clock, at noise level, while still composing. A tiled re-promotion
  is a second swap and needs D10's generation key back.
- **Depth-32 sources.** They need a proof that scanout equals scene blending
  for alpha-bearing pixels (spec point 5).
- **Unredirected server-owned sources** (D5), and per-output owners (PR
  #310).
- **Asynchronous promotion** (D2).
- **Re-admitting a drawable after export.** The server cannot see an fd
  close (D6).

## Resolved review questions

1. Reuse proof: keep the second-use trigger (D1); it fits marco but is not
   general, so successful promotions are capped as well as failures (D3).
2. One submit group's sync_file serving both buffers it wrote is sound
   (D8). Writes are tracked explicitly, and the fence is tri-state.
3. Direct Presents flush pending source writes before the commit, and a
   counter measures whether that adds a submit (Task 8).
4. A DRI3 export of a live promoted source waits until every CRTC has left
   it, and export disqualifies for good (D6, Task 7). The parked export
   pins its `DrawableId` and storage, because same-client FIFO does not
   stop another client freeing the pixmap (D6, Task 7).
5. Deferred export deadline (former open question 1): `PER_CRTC_EVENT_WAIT`
   (250 ms), then the safe all-off teardown; the fd goes out only after
   every plane has left the source, else `BadPixmap` with the pins kept
   (D6, Task 7).
6. Success cap (former open question 2): a token bucket, burst 8, one
   token per 30 s; the four-live memory cap stays (D3, Task 6).
7. Write sites (former open question 3): no missed scene or composite
   write site; the scene stamps are reads. `RecordedOp::written_drawable`
   is an exhaustive classifier, because `dst_id()` reports a read for
   `ClipSnapshotRefresh` (Task 8).
