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

### Runtime request orchestration

A runtime `RRSetCrtcConfig` that requires a different-device or
relationship-unknown allocation is deferred before compatibility work begins.
The core assigns a monotonic token and parks only that client's request FIFO,
preserving same-client X11 ordering while continuing to service input, VT
events, rendering, and other clients. Same-device requests and requests that
can retain their existing allocation stay on the synchronous path.

The backend snapshots the global topology epoch and the exact mode, source and
sink route identities, connector, encoder, CRTC, primary plane, and per-fence
timeout into a resource-free request. Requests enter one deterministic global
FIFO with at most one helper active. This is deliberately not parallel display
probing: one global topology epoch cannot yet admit concurrent disjoint results
deterministically. Parallel qualification requires resource-scoped epochs and
ordered admission and remains future work.

For each request, the child reexecutes the exact running yserver image and owns
a fresh disposable Vulkan/GBM graph. It receives a duplicate of the KMS DRM fd
only so it can run the exact atomic `TEST_ONLY` checks. The duplicate shares
the parent's DRM open file description, so process exit alone cannot be
treated as KMS cleanup and the child must never perform a live commit. Before
returning an ordinary `Compatible` plan or `Rejected` verdict, it strictly
removes every mode blob, framebuffer, and GEM handle that it created. Failure
to prove that cleanup, an uncertain submitted fence, an IPC/launch state in
which child ownership is uncertain, or whole-helper timeout produces
`Indeterminate`, never `Rejected`, and poisons the involved resource set.

Every submitted fence keeps the fresh 200 ms completion window defined below.
Those waits do not bound synchronous Vulkan driver entry points, allocation,
or `TEST_ONLY`, so the parent also enforces a separate 30 s whole-helper
watchdog. A parent-death signal prevents a helper from surviving yserver. On an
uncertain GPU path the child terminates without running normal Rust/Vulkan Drop
or `vkDeviceWaitIdle`; the parent does not reuse poisoned route resources.

The helper returns only a serialized exact plan, never live Vulkan or GBM
objects. While it searches, the parent leaves the old KMS topology installed.
For a `Compatible` result, the parent replays that exact plan into a full live
pool and runs every live `TEST_ONLY`, also without disturbing the old
topology. It then revalidates the topology epoch and request resources,
quiesces immediately before the real commit, and installs the prepared plan.
Thus the only intentional runtime dark interval is the final short
quiesce/install handoff; failure during helper search or live preparation has
no old framebuffer state to restore.

Connector/topology mutation, VT transition, provider-output-source change,
effective DPMS transition, and logical-screen-size change immediately retire
every affected parked request as `Interrupted` and wake its client FIFO. A
late helper result cannot replace that synthetic completion. The executor
still harvests the late result so uncertainty can poison resources internally.

This isolation currently covers runtime cross-device route qualification.
Startup qualification remains synchronous in the server process. The parent's
exact live allocation and `TEST_ONLY`, final quiesce, and real modeset replay
also remain synchronous host-call boundaries, although the old topology stays
lit during the live preparation phase.

Candidate order has three global layers: all copy-free plans first; then the
copied native tier; then the copied explicit-LINEAR tier. Within the native
tier, destinations retain the established GBM-first then renderer-owned
modifier/linear order and each destination is paired with native source
modifiers in A's stable advertised order. The LINEAR tier pairs modifier 0
with those destinations in the same established order. The qualification
worker creates fresh exact disposable logical devices for both A and B,
allocates a complete three-slot pool with one plan pair, and first validates
every destination framebuffer with a full connector/CRTC/primary-plane atomic
`TEST_ONLY`. Allocation remains at the requested full output extent, and both
allocation and `TEST_ONLY` are outside the timed region. GPU liveness timing is
attached to submitted work rather than the complete cold probe. Every
copy-free BO fence and every copied A or B fence receives its own fresh 200 ms
monotonic completion window. The budget resets for each submitted fence;
context and pipeline creation, full-pool allocation, `TEST_ONLY`, and compact
CPU verdict parsing are outside those windows. The per-device block reduction
and compact result write are submitted GPU work and therefore complete under
the corresponding A or B fence. The exact CPU fallback remains outside the
fence window after its image-copy fence succeeds and can therefore run into the
outer helper watchdog, which makes the result `Indeterminate`. Vulkan provides
no way to preempt a driver host call that itself returns late, so a successful
fence result proves completion regardless of total elapsed wall time. The
first copied cycle performs:

1. a real color-attachment render into A's optimal local target of a
   full-extent radial hue-and-luminance pattern. It desaturates smoothly toward
   every rectangular edge and includes coordinate-coded low bits, edge rails,
   and asymmetric exact-color corner fiducials;
2. an optional FOREIGN-to-A acquire of the external transport, separate local
   target/transport transitions, a full optimal-to-transport copy, local
   returns to `GENERAL`, then a separate transport A-to-FOREIGN release and
   export of A's completion as a `sync_file`. Without reacquiring or reading
   the external transport, A makes every pixel of its still-local optimal
   target contribute to a positional, multi-lane digest for its exact block
   and writes the compact digest table plus exact tokenized corner words to a
   small host-visible result buffer, preferring a `HOST_CACHED` memory type when
   available;
3. B import/wait, a matching FOREIGN-to-B acquire, local transfer layouts, a
   full-image GPU copy, local return to `GENERAL`, and separate B-to-FOREIGN
   releases for source and destination. Before the destination release, B
   reduces every pixel of that final Vulkan destination into the same
   positional, multi-lane per-block representation and writes its compact
   digest table and corner words to its own small host-visible result buffer,
   again preferring `HOST_CACHED` memory when available;
4. A and B probe-fence completion, each with its own fresh 200 ms window,
   followed after both report success by CPU validation of the exact expected
   tokenized corner words, equality of every corresponding positional digest
   block and lane, and the freshness record. Completion remains authoritative
   if a successful host wait returns after the nominal window. Because compact
   digests necessarily admit collisions, this is a probabilistic content
   guarantee rather than the previous collision-free byte equality; multiple
   independent lanes plus block position make accidental admission negligible
   while retaining block-level diagnostics.

Digest eligibility is decided independently for A and B. Each selected queue
must support compute, and the corresponding input must fit that device's
`maxStorageBufferRange`. If either check fails, qualification falls back to the
previous full, tightly packed A-target/B-destination readbacks and exact CPU
byte comparison. A reducer infrastructure failure that is safely known before
submission takes the same correctness-preserving fallback. Neither condition
is route incompatibility, and the fallback is not selected merely for
performance or after a digest mismatch. A reducer failure after submission
that leaves completion or resource state uncertain is `Indeterminate`; it
cannot reject the route. The exact CPU fallback remains subject to the outer
helper watchdog and therefore may itself end as `Indeterminate`.

Cycle two changes the pattern token, imports B's retained return completion
into a fresh temporary A semaphore, waits it, and executes FOREIGN-to-A on the
external transport before another target-to-transport copy/release/reduction.
Repeating the first renderer digest set or freshness record is rejected, and
both devices' exact corner words must encode the new token, so matching stale
frames cannot pass merely by matching each other. The optimal target stays
A-local in both cycles. `TEST_ONLY` does not make KMS an ownership participant,
so the disposable destination is treated as atomic-rejected/abandoned after
cycle one and cycle two full-discards it from `UNDEFINED`; the real `GENERAL`
KMS-to-B return leg is reserved for a live two-flip hardware smoke test and is
never fabricated by the probe.

A safe route-specific pre-submit failure other than reducer preparation, or a
validation failure after both fences have completed, such as a digest,
fiducial, or freshness mismatch, rejects only that plan and continues the exact
candidate order. Once both fences complete, CPU
corner/digest/freshness validation runs to its authoritative verdict even if
the cycle or complete probe has already taken more than 200 ms. After all
submitted work on the disposable A/B contexts is fence-proven complete, those
contexts are marked quiescent and their pool, pipeline, and final context Drop paths skip the
otherwise defensive device-wide idle. Live contexts and uncertain disposable
contexts never receive that exemption. Classification follows submission state
rather than the error kind alone: only a timeout or other post-submit failure
that leaves at least one submitted fence incomplete or uncertain is terminal
for exact-plan search. For a runtime request, the helper owns that entire
disposable attempt, including its pool, pipeline, uncertain fences, and owning
contexts. It terminates without normal Drop or `vkDeviceWaitIdle`, because
Vulkan has no safe cancellation primitive; the parent records
`Indeterminate`, poisons the involved resources, and attempts no later exact
plan. Startup still runs this work in the server process, so it retains an
uncertain attempt and any already-built live GPU owners and aborts later
outputs rather than letting constructor unwind enter an idle wait. Runtime
qualification and live preparation leave the old topology untouched, so their
failure returns the request error without a restore commit.

The compact digest readbacks, and the exact full-image CPU readbacks on the
eligibility/setup fallback only, are setup-time validation rather than a CPU
transport fallback in the live frame path. They probabilistically prove the
Vulkan-visible render, transport copy, DMA-BUF import, and B copy chain. They
cannot prove that the display engine will interpret GBM/KMS pitch, offset, and
modifier metadata identically; atomic `TEST_ONLY` plus a live two-flip visual,
writeback, or CRTC-CRC smoke remains the end-to-end display gate.

Only the exact winning pair is replayed on the live A/B contexts, and every
live destination framebuffer is tested again. The runtime helper returns the
resource-free pair and never commits it; the parent performs live allocation
and `TEST_ONLY` while the old topology remains installed, revalidates stale
state, then quiesces and commits. The install step marks the exact destination
front before ownership can leave it. Startup probing runs synchronously while
the initial dumb-framebuffer rollback guard remains armed.

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
  or uncertain. At runtime the isolated helper abandons that graph and exits
  without normal Drop or `vkDeviceWaitIdle`; synchronous startup retains and
  quarantines it in the server process.
- A runtime `Compatible` or ordinary `Rejected` result is accepted only after
  strict cleanup of all helper-created KMS mode blobs, framebuffer IDs, and GEM
  handles. Cleanup uncertainty is `Indeterminate` and poisons the request's
  resources because the child and parent share a DRM open file description.
- The 30 s process watchdog bounds the entire helper, independently of the
  fresh 200 ms window for each submitted fence. Watchdog, IPC, uncertain launch,
  and parent-lifetime failures never become compatibility rejections.
- Live failure to prove quiescence quarantines and disarms both sides; Drop
  leaks the B alias, A transport backing, A optimal target, and uncertain
  scanout resources rather than freeing GPU-referenced memory.
- Live A or B device loss is fatal.
- A sink without explicit modifier/layout import support rejects copied
  scanout before any foreign-memory allocation or submission.
- Every full-extent content-probe pixel contributes to one position-bound block
  and multiple independent digest lanes on both A and B. Reducer writes and
  copies to compact host-visible result buffers occur before the respective A
  and B fences, with `HOST_CACHED` memory preferred but not required;
  successful waits precede every CPU read. Each fence has its own
  fresh 200 ms outstanding-completion window, but successful fences always
  proceed to exact tokenized-corner, corresponding block-digest, and per-cycle
  freshness validation regardless of cumulative elapsed time. Digest equality
  is probabilistic, not collision-free. The digest path requires each selected
  A/B queue independently to support compute and each input to fit that
  endpoint's `maxStorageBufferRange`. Full tightly packed readback and exact
  byte comparison preserve correctness when those checks fail or reducer
  infrastructure fails safely before submission. Post-submit reducer
  uncertainty, or an exact fallback that reaches the whole-helper watchdog, is
  `Indeterminate` rather than route incompatibility.
- Connector/topology, VT, provider, effective DPMS, and logical-size changes
  retire a parked qualification immediately with `Interrupted` and suppress
  its late result. Installed copied-scanout teardown separately unregisters
  live frame jobs, retires scene pins, quiesces both devices, and only then
  resets or drops pools.
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
dependency. Candidate setup keeps all three slots, both cycles, full-size
allocations, and both full-image GPU copies, but normally reads only compact
per-block digest results on the CPU. The full exact CPU comparison is retained
when either selected queue lacks compute, either input exceeds the relevant
`maxStorageBufferRange`, or reducer infrastructure cannot safely be prepared;
it is never a live-frame transport and remains subject to the outer helper
watchdog. On the deployed AMD Radeon 780M plus NVIDIA RTX
4070 route at 1600x1200, the allocator preference selects uncached
`DEVICE_LOCAL|HOST_VISIBLE|HOST_COHERENT` staging on both devices. The first
cycle matched with 0 ms and 2 ms A/B fence waits, yet its whole post-fence
validation phase took 15,149 ms. That interval also includes semaphore
bookkeeping and cycle recovery, but its scale is consistent with the full
mapped image scan; the second cycle then ran into the 30 s helper watchdog.
That was a false route timeout caused by validation overhead, not evidence of
GPU incompatibility. Damage-limited copies and asynchronous live-frame failure
recovery are later optimizations. Runtime route qualification is asynchronous
with respect to the core, but its initial scheduler is a global single-flight
FIFO; safe parallel display probing awaits
resource-scoped topology epochs and deterministic ordered admission.
