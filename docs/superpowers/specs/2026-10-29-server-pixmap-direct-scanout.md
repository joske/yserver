# Direct scanout of server-created compositor Present pixmaps

**Status:** design for review; no implementation. **Scope:** KMS, root-sized
depth-24 compositor-final Present sources, initially Marco's COW on one output.
Keep the existing imported dma-buf path and composed fallback.

## Evidence and goal

On fuji during the same fullscreen YouTube video, Xorg flipped Marco's
Presented COW pixmap 301 times in 10 seconds. Yserver copied it 317 times and
then composed the full screen at about 6.5 ms GPU per compose, with measured
overdraw 2.0. This explains a large part of the GPU gap and some of the CPU
gap. The source is a plain `XCreatePixmap` buffer, so yserver's imported-only
M1 probe never admits it. A successful steady-state path should flip Marco's
*final* pixmap and skip both the Present Copy and the extra scene compose, as
the existing M2 path already does for imported compositor buffers. The
full-screen damage itself is expected and is not the optimization target.

This is **not** an application-window unredirect. Marco remains responsible
for the final desktop image, including video, controls, effects, and panels.
The redirect-boundary and PutImage CPU investigations remain separate.

## Current seams

- `backend_redirect_create_pixmap` allocates generic Vulkan storage through
  `GpuCore::allocate_drawable_storage`; the small-pixmap recycle pool is part
  of that path. A root-sized pixmap is not scanout-qualified by creation.
- `RenderEngine::promote_drawable_exportable` already preserves `DrawableId`,
  copies old pixels, flushes outstanding work, replaces image/views, and
  retires old handles. It was built for GLX/DRI3 exports. Its allocator
  prefers linear memory and does not select against KMS plane modifiers or
  perform an atomic `TEST_ONLY`. `Storage` keeps stride and modifier, but the
  existing export path reports offset zero even though the allocator queries
  a real plane offset. Neither `is_exportable` nor the scanout-BO allocator
  proves that an image is a renderable, sampleable *pixmap* and scannable.
- `maybe_probe_scanout_m1` only reads `imported_dmabuf` metadata and its fd.
  It validates XRGB8888, BGRA8, one explicit plane, depth 24/bpp 32, full-root
  geometry, supported modifiers and atomic `TEST_ONLY`. Its cache currently
  keys on `DrawableId` plus a separately tracked topology signature, so a
  storage replacement needs invalidation or a storage generation in the key.
- M2 already pins the Presented source and fallback paint target, transfers
  ownership through flip retirement, and materializes a COW shadow on unflip.
  Its scene, cursor, border, CRTC, shape and root-overlay gates should remain
  the common admission rules for server-owned sources.

## Options

| Approach | Steady-state result | Cost and difficulty |
| --- | --- | --- |
| **A. Promote after an eligible Present** | Direct flip from the same pixmap on later Presents | Two one-time allocations/copies for Marco's double buffer; needs a scanout-qualified allocation and synchronization. First frame(s) still compose. Only candidate pixmaps pay. |
| **B. Allocate scanout-capable at CreatePixmap** | Can flip the first eligible Present | Size/depth alone cannot establish that a pixmap will be the compositor's final buffer. Every matching allocation pays the larger allocation/GBM/import cost, including unrelated fullscreen intermediates. Display topology may change later. |
| **C. Copy into a dedicated scanout BO** | Skips the scene compose but retains one full-screen copy per frame | Reuses proven scanout BO allocation, but adds a copy, a submit/fence, and a separate Present retirement/ownership path. Useful as a fallback experiment if no directly renderable and scannable pixmap layout exists. |

**Recommendation:** A, with a small hardware proof before committing to a new
allocator. Promote one Marco buffer using the existing export path and run the
exact M1 `AddFB2`/atomic `TEST_ONLY` against its actual fd, offset, pitch and
modifier. Measure its render throughput as well as flip acceptance: a linear
pixmap that flips but makes Marco's SRC/OVER rendering expensive is not a win.
If the existing allocation fails scanout or is slow, allocate a **separate
scanout-qualified pixmap storage class** on first eligible Present. Select a
modifier supported by both Vulkan with the full pixmap usage
(`COLOR_ATTACHMENT|SAMPLED|TRANSFER_SRC|TRANSFER_DST`) and the KMS primary
plane. Prefer the existing GBM-first allocation direction where necessary
(notably NVIDIA); import into Vulkan with that full usage. Validate the exact
buffer with `TEST_ONLY` before replacing live storage. Do not reuse the
compose-only `ScanoutBo` or copied-transport allocations unchanged: their
image-usage contracts lack `SAMPLED` or attachment support, respectively.

## Admission, synchronization, and lifetime

1. Identify an authoritative root Present to the COW or an eligible COW
   descendant. Require zero offsets, an unbordered target, whole-root shape
   and coverage, depth 24/XRGB8888, one explicit plane, no transformed CRTC,
   eligible hardware cursor, no root overlay, and the existing M2 scene
   ownership checks. Do not promote merely because a pixmap is screen-sized.
   The first eligible Present may take the normal Copy path while promotion
   happens; later Presents can probe and flip. Keep PR #310's per-output owner
   work compatible, but prove the root case first.
2. Promote at most once per stable source storage. Before migration, close
   pending frame/submit work and preserve the source's contents and XID,
   picture aliases, render views and readback behavior. Never replace storage
   that has already been exported to an external client unless all external
   references are known to be gone: an old fd would continue pointing at the
   old image. Keep the generic image on failed allocation, export, Vulkan
   import or DRM probe; compose normally. If render and display devices
   cannot share the chosen layout, leave this source on the composed route.
3. Store a complete server-owned scanout descriptor: fd ownership, fourcc,
   depth/bpp, exact width/height, plane count, offset, pitch and modifier.
   Share M1's format/geometry/`TEST_ONLY` validation for imported and
   server-owned descriptors. Cache probe results by storage generation and
   topology; invalidate both accepted and rejected entries on storage swap,
   resize, modeset, VT loss and teardown. An active framebuffer must outlive
   every CRTC still scanning it out, as in #311.
4. Publish the *server's Vulkan writes* before the atomic flip. The current
   Present source wait handles imported dma-bufs; it does not by itself wait
   for server-owned Vulkan render commands. Flush the source's pending work,
   then either attach its completion sync_file as `IN_FENCE_FD` for every
   flipped plane or establish an equivalent verified dma-buf implicit fence.
   A CPU fence wait is a correctness fallback, not the intended per-frame
   path. Keep source storage pinned until display retirement, and preserve
   Present completion/idle timing and the M2 lazy COW shadow on unflip.
5. Revalidate live COW shape, stacking, cursor/overlay and target changes
   before each flip and while a direct frame is held. An empty or partial COW
   shape, an alpha-bearing source, other visible content, or a failing
   modifier/atomic check must use the composed path. Marco's observed
   depth-24 final pixmap is opaque; depth-32 sources need separate proof that
   replacing scene blending with scanout has the same pixels. Existing
   unflip-on-shape and scene-change behavior must continue to work.

## Proof and success criteria

Start with telemetry for candidate count, promotion attempts, accepted and
rejected `TEST_ONLY` probes, chosen modifier/pitch/offset, direct versus Copy
Presents, and reasons for unflip. On fuji, repeat the same fullscreen video on
Xorg and yserver; verify that after at most the two Marco buffer promotions,
   the steady state (after existing M2 admission probation) has about one
   direct flip per video frame, no COW Copy or scene compose per frame, and
   correct pixels with zero dropped frames. Compare
GPU busy *at frequency*, server CPU and Marco render cost; compare against the
current yserver capture and Xorg, not just flip counts. Exercise COW shape
changes, cursor fallback, panels/overlays, resize/modeset, VT switch and a
failed `TEST_ONLY` to prove a visible composed fallback. Then test a second
GPU/driver; NVIDIA particularly needs GBM-backed tiled allocation validation.

## Probe results

Measured 2026-10-29 with `diag/server-pixmap-scanout-probe` (`df3c284b`,
not for merge): MATE with marco compositing, fullscreen video. The probe
promoted each eligible source once through `promote_drawable_exportable`,
exported it and ran M1's exact `AddFB2` + atomic `TEST_ONLY`; nothing flipped.

| Box | Source | Layout | Result |
| --- | --- | --- | --- |
| fuji (i5-7200U HD 620, i915/anv, 1920x1080) | marco's two root-sized depth-24 pixmaps | tiling Linear, modifier 0x0, pitch 7680, offset 0 (reported and queried agree) | AddFB2 ok, `TEST_ONLY` accepted; promote 6–8 ms each, one-time |
| silence (RX 6800 Navi21, RADV, two outputs, root 5120x1440) | same | LinearModifier 0x0, pitch 20480 | AddFB2 ok, `TEST_ONLY` accepted across both CRTCs; promote 0.5–0.8 ms |

On fuji, GPU busy×clock during playback was about 9% higher with linear
marco buffers (noise level, still composing). NVIDIA is untested; it needs
the GBM-backed allocation above.

Design remarks from the probe, addressed in the plan:

- Promotion is synchronous inside the first eligible Present (closes the
  frame, flushes, blocking copy, fence wait); that Present's Copy reads the
  new storage.
- `allocate_exportable` stores its tiling strategy globally in
  `VkContext::tfp_tiling_strategy`, shared with GLX TFP. Scanout promotion
  must neither depend on nor set it.
- `export_promoted`/`export_backing` report offset 0 and
  `ExportableImage::into_raw_parts` drops the queried offset.
- The M1 probe cache keys on `DrawableId`; promotion swaps storage under the
  same id, so the key needs a storage generation.
- Plan review: the probe cache keeps its `DrawableId` key; a one-swap invariant replaces the storage generation of point 3 (plan D10).
