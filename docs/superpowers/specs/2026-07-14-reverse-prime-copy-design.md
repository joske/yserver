# Reverse-PRIME copy scanout design

## Goal

Add a compatibility transport for a renderer A and a display/KMS device B
when none of yserver's copy-free DMA-BUF allocation plans can satisfy both
endpoints:

- output-owned shared: B allocates, A renders, B scans out;
- renderer-owned shared: A allocates, A renders, B scans out;
- copied: A renders an optimal local target, copies it into an explicit
  DRM-modifier external transport, then renderer B copies that transport into
  an independent B-local scanout destination.

Copy removes the requirement that one allocation be both renderable by A and
scannable by B. It still requires a Vulkan renderer associated unambiguously
with B and capable of importing A's DMA-BUF transport as a transfer
source with its exact offset and pitch. CPU readback/upload is not a scanout
transport fallback.

## Endpoint and selection model

Copy is transport, not a third `ScanoutOwnership`. One output owns:

```text
OutputScanout
  Shared(ScanoutBoPool)          owner = Output | Renderer
  Copied(CopiedScanoutPool)      optimal A -> external A/B -> destination B -> KMS B
```

The outer `ScanoutRoute` always identifies the selected `RenderDeviceId` A
and the sink KMS `DrmDeviceKey`. A copied destination pool separately records
a truthful B-local route and one of the existing Output/Renderer allocation
plans. Provider XIDs are policy objects, not route identities.

`Same` routes use only the established shared allocator. `Different` and
`Unknown` routes exhaust every exact shared plan before copied candidates are
considered. B is resolved only when exactly one non-selected inventoried
`RenderDevice` advertises a primary node equal to the sink KMS key. Its Vulkan
context is recreated by exact device UUID plus driver UUID. Missing,
ambiguous, or display-only B endpoints make copied scanout unavailable; they
never trigger generic Vulkan scoring or a first-node fallback.

Both logical devices must expose and enable `VK_EXT_queue_family_foreign`.
The A/B transport is an exclusive external-memory allocation shared by
distinct devices/drivers, so its ownership transfers use
`VK_QUEUE_FAMILY_FOREIGN_EXT`; `VK_QUEUE_FAMILY_EXTERNAL` is not a substitute
because it is limited to queues from the same physical device and driver.
Missing foreign-family support removes copied candidates without making the
selected live renderer globally fatal.

Both contexts must also expose `VK_EXT_image_drm_format_modifier`. Renderer A
enumerates its `B8G8R8A8_UNORM` modifier list and retains a nonzero modifier
only when A can export it as a one-plane DMA-BUF with exactly `TRANSFER_DST`
usage and B can import the same one-plane modifier with exactly `TRANSFER_SRC`
usage. Mutually supported native modifiers retain A's advertised order;
explicit `DRM_FORMAT_MOD_LINEAR` is queried independently and appended as the
final transport fallback. Every allocation uses
`VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT` with a singleton modifier list, and B
imports that exact modifier, offset, and row pitch. Copied transport never uses
implicit `VK_IMAGE_TILING_LINEAR`: RADV can reject that external-image query
while supporting the equivalent explicit modifier-0 contract. The transport
is not intersected with KMS plane modifiers because it is never scanned out.
A's optimal color-attachment target is not external and never changes
queue-family ownership.

Legacy DRI3 imports have the same layout-description limit but a narrower
topology gate. If the selected renderer exposes the modifier extension, DRI3
remains available. Without it, DRI3 remains available for one inventoried
Vulkan renderer, including a verified renderer paired with additional
display-only KMS endpoints, and is hidden once another Vulkan renderer may
originate a PRIME buffer whose padded stride cannot be represented. This makes
clients choose their software fallback rather than displaying empty window
interiors. `UnverifiedFallback` cannot prove that several KMS endpoints
coalesce with the selected renderer, so it preserves the historical one-KMS
allowance and fails closed when more than one KMS device is open.

## Exact probing and replay

Each copied candidate is a persisted pair:

```text
CopiedScanoutPlan {
  source: one exact A/B external transport modifier,
  destination: one existing exact B-local ScanoutAllocationPlan,
}
```

Candidate order has three global layers: all copy-free plans first; then the
copied native tier; then the copied explicit-LINEAR tier. Within the native
tier, destinations retain the established GBM-first then renderer-owned
modifier/linear order and each destination is paired with native source
modifiers in A's stable advertised order. The LINEAR tier pairs modifier 0
with those destinations in the same established order. The
probe creates fresh exact disposable logical devices for both A and B,
allocates a complete three-slot pool with one plan pair, and first validates
every destination framebuffer with a full connector/CRTC/primary-plane atomic
`TEST_ONLY`. Allocation remains at the requested full output extent, and both
allocation and `TEST_ONLY` are outside the timed region. GPU liveness timing is
attached to submitted work rather than the complete cold probe. Every
copy-free BO fence and every copied A or B fence receives its own fresh 200 ms
monotonic completion window. The budget resets for each submitted fence;
context and pipeline creation, full-pool allocation, `TEST_ONLY`, and CPU
validation are outside those windows. Vulkan provides no way to preempt a
driver host call that itself returns late, so a successful fence result proves
completion regardless of total elapsed wall time. The first copied cycle
performs:

1. a real color-attachment render into A's optimal local target of a
   full-extent radial hue-and-luminance pattern. It desaturates smoothly toward
   every rectangular edge and includes coordinate-coded low bits, edge rails,
   and asymmetric exact-color corner fiducials;
2. an optional FOREIGN-to-A acquire of the external transport, separate local
   target/transport transitions, a full optimal-to-transport copy, local
   returns to `GENERAL`, then a separate transport A-to-FOREIGN release and
   export of A's completion as a `sync_file`. A also copies its still-local
   optimal target into a tightly packed host-visible BGRA buffer without
   reacquiring or reading the external transport;
3. B import/wait, a matching FOREIGN-to-B acquire, local transfer layouts, a
   full-image GPU copy, local return to `GENERAL`, and separate B-to-FOREIGN
   releases for source and destination. Before the destination release, B
   copies that final Vulkan destination into its own tightly packed
   host-visible BGRA buffer;
4. A and B probe-fence completion, each with its own fresh 200 ms window,
   followed after both report success by stable diagnostic hashes and an exact
   byte-for-byte A-target/B-destination comparison. Completion remains
   authoritative if a successful host wait returns after the nominal window;
   hashes never decide admission alone, and a mismatch reports the first pixel
   and channel.

Cycle two changes the pattern token, imports B's retained return completion
into a fresh temporary A semaphore, waits it, and executes FOREIGN-to-A on the
external transport before another target-to-transport copy/release/readback.
Repeating the first renderer hash is rejected, so matching stale frames on both
devices cannot pass merely by matching each other. The optimal target stays
A-local in both cycles. `TEST_ONLY` does not make KMS an ownership participant,
so the disposable destination is treated as atomic-rejected/abandoned after
cycle one and cycle two full-discards it from `UNDEFINED`; the real `GENERAL`
KMS-to-B return leg is reserved for a live two-flip hardware smoke test and is
never fabricated by the probe.

A safe pre-submit failure or validation failure after both fences have
completed, such as a pixel, fiducial, or freshness mismatch, rejects only that
plan and continues the exact candidate order. Once both fences complete, CPU
validation runs to its authoritative verdict even if the cycle or complete
probe has already taken more than 200 ms. After all submitted work on the
disposable A/B contexts is fence-proven complete, those contexts are marked
quiescent and their pool, pipeline, and final context Drop paths skip the
otherwise defensive device-wide idle. Live contexts and uncertain disposable
contexts never receive that exemption. Classification follows submission state
rather than the error kind alone: only a timeout or other post-submit failure
that leaves at least one submitted fence incomplete or uncertain is terminal
for exact-plan search. That path retains the entire disposable
attempt, including its pool, pipeline, uncertain fences, and owning contexts,
until process exit; it bypasses normal Drop and never calls
`vkDeviceWaitIdle`, because Vulkan has no safe cancellation primitive. No later
copy-free or copied exact plan is attempted after that terminal failure.
Startup additionally retains already-built live GPU owners and aborts later
outputs so constructor unwind cannot enter another idle wait. At runtime, the
RANDR failure path keeps the old scene/Vulkan ownership intact, re-commits only
the unchanged old KMS framebuffers when they were active, and returns the
request failure without normal topology recovery.

The CPU readbacks are setup-time validation only, not a CPU transport fallback
in the live frame path. They prove the Vulkan-visible render, transport copy,
DMA-BUF import, and B copy chain. They cannot prove that the display engine
will interpret GBM/KMS pitch, offset, and modifier metadata identically; atomic
`TEST_ONLY` plus a live two-flip visual, writeback, or CRTC-CRC smoke remains the
end-to-end display gate.

Only the exact winning pair is replayed on the live A/B contexts, and every
live destination framebuffer is tested again. Startup probing runs while the
initial dumb-framebuffer rollback guard remains armed. Runtime enable commits
and marks the exact destination front inside the candidate helper before its
ownership can leave that helper.

DMA-BUF import uses the source's exact modifier, offset, and pitch. Vulkan
image memory requirements are intersected with
`vkGetMemoryFdPropertiesKHR`'s fd memory-type mask. A sink without the explicit
modifier path is rejected categorically: a coincidentally matching implicit
linear pitch is not treated as a safe compatibility probe on affected older
drivers.

## Completion boundary and frame sequence

```text
Free
  -> RenderingOnA
  -> WaitingForRenderCompletion(job, OutputKey, slot)
  -> CopySubmittedOnB + KmsFlipPending
  -> OnScreen
  -> Free
```

A signals an exportable binary semaphore and exports one completion payload.
Vulkan's valid already-signalled `SYNC_FD` result (`fd = -1`) is represented as
`None`: it bypasses readiness polling, but B still imports raw `-1` into a
fresh temporary semaphore and includes the normal `ALL_COMMANDS` wait. A real
fd joins a stable backend-owned epoll/kqueue aggregator because the core loop
samples `Backend::poll_fds()` only at startup. Core dispatch has a dedicated
`ScanoutRenderCompletion` source. Each job has a monotonic id, a
device-qualified `OutputKey`, and a BO index; raw fds and output-vector
indices are never identities.

Readiness schedules the handoff but does not replace synchronization. B
imports and waits on A's payload, copies the image, then exports B's completion
payload. A valid fd is duplicated: one copy goes to the KMS atomic commit as
`IN_FENCE_FD`, while the other is retained with the paired source for its next
FOREIGN-to-A acquire. The `fd = -1` result is retained as an explicit
already-signalled sentinel and is likewise imported, not omitted. Each
temporary imported semaphore lives through the submission that consumes it
and is destroyed only after that submission's fence is proven complete. Only
the matching KMS page-flip retirement acknowledges
damage/generation/cursor state, releases descriptor and drawable pins, and
makes the paired A slot reusable. KMS
framebuffer selection, retained-front state, and direct-scanout bookkeeping
always use B's destination. Composited readback uses A's optimal local target,
requires a previously submitted successful compose, and neither consumes the
retained B completion nor changes transport ownership.

Destination ownership also preserves layout provenance. A destination
actually produced by B is released in `GENERAL`; after KMS later replaces it,
the next B write uses a matching `GENERAL` FOREIGN acquire. A fresh destination
installed directly by synchronous modeset was never given a Vulkan layout, so
its eventual retirement remains uninitialized and the next full copy performs
a FOREIGN acquire/discard from `UNDEFINED`. Atomic rejection similarly uses a
local full-discard path rather than inventing a KMS return release. The next
transport overwrite consumes the retained B completion and performs its own
FOREIGN-to-A acquire. A separate local-pixel-valid bit starts false, becomes
true only after successful A queue
submission, and is cleared by failed-cycle recovery and lifecycle reset.

## Failure and lifetime invariants

- At most one frame per output waits for A or a KMS flip.
- A's external transport is never overwritten while B may still read it; the
  optimal target itself remains renderer-local.
- B is never written while pending or on screen.
- Completion registration failure keeps the destination reserved and defers
  recycling until A's compose fence signals.
- A live post-submit completion-export failure leaves that binary semaphore
  dirty. A's semaphore is recreated only after its compose fence succeeds; B's
  is recreated only after a successful sink `device_wait_idle`. Exporting
  either a real fd or `fd = -1` makes the semaphore reusable normally.
- A stale live-output completion fails closed rather than guessing a ledger
  entry.
- A live B-copy or atomic failure synchronously quiesces B before reuse and
  folds damage forward with retry backoff. This live recovery remains safe but
  may block on a wedged driver.
- A safe disposable validation failure after proven fence completion advances
  to the next exact pair with fresh contexts. A safe failure before submission
  does likewise. Safe teardown marks a disposable context quiescent only after
  all of its submitted fences complete, allowing final Drop to omit a redundant
  `vkDeviceWaitIdle`; this policy is never enabled for live contexts.
- A timeout or error is terminal only when submitted work remains incomplete
  or uncertain. That disposable attempt is retained and quarantined in its
  entirety without normal Drop or `vkDeviceWaitIdle`; GPU-referenced resources
  and both owning contexts remain alive until process exit.
- Live failure to prove quiescence quarantines and disarms both sides; Drop
  leaks the B alias, A transport backing, A optimal target, and uncertain
  scanout resources rather than freeing GPU-referenced memory.
- Live A or B device loss is fatal.
- A sink without explicit modifier/layout import support rejects copied
  scanout before any foreign-memory allocation or submission.
- Content-probe buffers are host-coherent and tightly packed. COPY-to-HOST
  barriers plus successful A and B fences precede every CPU read. Each fence
  has its own fresh 200 ms outstanding-completion window, but successful fences
  always proceed to corner-fiducial, per-cycle freshness, hash, and exact-byte
  validation regardless of cumulative elapsed time.
- Connector removal, topology rebuild, VT release, DPMS off, and shutdown
  unregister waiting jobs, retire scene pins, quiesce both devices, and only
  then reset or drop pools.
- A failed live fence wait or recovery retains the exact BO/descriptor ledger
  and latches fatal instead of releasing anything still queue-referenced. After
  KMS is off and both devices are proven idle, lifecycle reset destroys both
  temporary wait semaphores, drops retained payloads, rearms dirty export
  semaphores, and normalizes source/destination ownership to full-overwrite
  discard states. Failure to prove that boundary leaves the pool quarantined.
- Failed final KMS disable disarms the B-visible destination backing.

## Initial performance and follow-up boundary

Copied scanout renders and copies the full output twice on the GPU: optimal A
target to the selected transport, then B's imported transport alias to its scanout
destination. The scene already uses full repaint, so this adds no buffer-age
dependency. Candidate setup additionally reads and compares both full images
for all three slots and two cycles; per-fence 200 ms completion windows do not
reduce the three-slot pool or full-output image extent. Live frames do not
perform this CPU work. There is no copied CPU fallback. Damage-limited copies
and asynchronous failure recovery are later optimizations.
