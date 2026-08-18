# Reverse-PRIME copy scanout design

## Goal

Add a compatibility transport for a renderer A and a display/KMS device B
when none of yserver's copy-free DMA-BUF allocation plans can satisfy both
endpoints:

- output-owned shared: B allocates, A renders, B scans out;
- renderer-owned shared: A allocates, A renders, B scans out;
- copied: A renders an exportable source, then renderer B copies into an
  independent B-local scanout destination.

Copy removes the requirement that one allocation be both renderable by A and
scannable by B. It still requires a Vulkan renderer associated unambiguously
with B and capable of importing A's DMA-BUF as a transfer source with its exact
modifier, offset, and pitch. CPU readback/upload is outside this design.

## Endpoint and selection model

Copy is transport, not a third `ScanoutOwnership`. One output owns:

```text
OutputScanout
  Shared(ScanoutBoPool)          owner = Output | Renderer
  Copied(CopiedScanoutPool)      source A -> destination B -> KMS B
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
Copied allocations are exclusive external-memory images shared by distinct
devices/drivers, so their ownership transfers use
`VK_QUEUE_FAMILY_FOREIGN_EXT`; `VK_QUEUE_FAMILY_EXTERNAL` is not a substitute
because it is limited to queues from the same physical device and driver.
Missing foreign-family support removes copied candidates without making the
selected live renderer globally fatal.

The sink context must also expose `VK_EXT_image_drm_format_modifier`. A driver
without explicit DMA-BUF modifier/layout import cannot safely express A's
foreign pitch; copied scanout rejects that sink before allocating or importing
foreign memory. This gate still permits explicit `DRM_FORMAT_MOD_LINEAR`; it
does not add the renderer-local optimal-to-linear transport reserved for the
later linearization revision.

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
  source: A exportable linear or exact DRM modifier,
  destination: one existing exact B-local ScanoutAllocationPlan,
}
```

Destination order remains the established GBM-first then renderer-owned
modifier/linear order; source order is stable within each destination. The
probe creates fresh exact disposable logical devices for both A and B,
allocates a complete three-slot pool with one plan pair, and first validates
every destination framebuffer with a full connector/CRTC/primary-plane atomic
`TEST_ONLY`. It then validates every slot with two A/B cycles. The first cycle
performs:

1. a real color-attachment render on A;
2. A's local transition to `GENERAL`, then a separate A-to-FOREIGN ownership
   release and export of A's completion as a `sync_file`;
3. B import/wait, a matching FOREIGN-to-B acquire, local transfer layouts, a
   full-image GPU copy, local return to `GENERAL`, and separate B-to-FOREIGN
   releases for source and destination;
4. bounded A and B probe-fence completion.

Cycle two imports B's retained return completion into a fresh temporary A
semaphore, waits it, and executes FOREIGN-to-A before another render/release.
`TEST_ONLY` does not make KMS an ownership participant, so the disposable
destination is treated as atomic-rejected/abandoned after cycle one and cycle
two full-discards it from `UNDEFINED`; the real `GENERAL` KMS-to-B return leg
is reserved for a live two-flip hardware smoke test and is never fabricated by
the probe.

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
the matching KMS
page-flip retirement acknowledges damage/generation/cursor state, releases
descriptor and drawable pins, and makes the paired A source reusable. KMS
framebuffer selection, retained-front state, and direct-scanout bookkeeping
always use B's destination; composited readback uses A's source.

Destination ownership also preserves layout provenance. A destination
actually produced by B is released in `GENERAL`; after KMS later replaces it,
the next B write uses a matching `GENERAL` FOREIGN acquire. A fresh destination
installed directly by synchronous modeset was never given a Vulkan layout, so
its eventual retirement remains uninitialized and the next full copy performs
a FOREIGN acquire/discard from `UNDEFINED`. Atomic rejection similarly uses a
local full-discard path rather than inventing a KMS return release. Readback of
a foreign-owned A source consumes the retained B completion and performs its
own FOREIGN-to-A acquire before copying pixels.

## Failure and lifetime invariants

- At most one frame per output waits for A or a KMS flip.
- A is never rendered again while B may still read it.
- B is never written while pending or on screen.
- Completion registration failure keeps the destination reserved and defers
  recycling until A's compose fence signals.
- A post-submit completion-export failure leaves that binary semaphore dirty.
  A's semaphore is recreated only after its compose fence succeeds; B's is
  recreated only after a successful sink `device_wait_idle`. Exporting either
  a real fd or `fd = -1` makes the semaphore reusable normally.
- A stale live-output completion fails closed rather than guessing a ledger
  entry.
- A B-copy or atomic failure synchronously quiesces B before reuse and folds
  damage forward with retry backoff. This base recovery is safe but may block
  on a wedged driver.
- Failure to prove quiescence quarantines and disarms both sides; Drop leaks
  uncertain Vulkan/scanout resources rather than freeing GPU-referenced
  memory.
- Live A or B device loss is fatal. Disposable probe failure advances to the
  next exact pair with fresh contexts.
- A sink without explicit modifier/layout import support rejects copied
  scanout before any foreign-memory allocation or submission.
- Connector removal, topology rebuild, VT release, DPMS off, and shutdown
  unregister waiting jobs, retire scene pins, quiesce both devices, and only
  then reset or drop pools.
- A failed fence wait or recovery retains the exact BO/descriptor ledger and
  latches fatal instead of releasing anything still queue-referenced. After
  KMS is off and both devices are proven idle, lifecycle reset destroys both
  temporary wait semaphores, drops retained payloads, rearms dirty export
  semaphores, and normalizes source/destination ownership to full-overwrite
  discard states. Failure to prove that boundary leaves the pool quarantined.
- Failed final KMS disable disarms the B-visible destination backing.

## Initial performance and follow-up boundary

Copied scanout renders and copies the full output. The scene already uses full
repaint, so this adds no buffer-age dependency. There is no copied CPU
fallback. Damage-limited copies and asynchronous failure recovery are later
optimizations. Renderer-local optimal composition followed by an explicit
A-side linearization copy is a separate transport revision and is not part of
this first copied path.
