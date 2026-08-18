# Reverse-PRIME copy implementation plan

1. Keep renderer-to-KMS route identity separate from allocation ownership and
   introduce `OutputScanout::{Shared,Copied}`.
2. Inventory exact Vulkan device/driver UUID selectors and resolve a copied
   sink renderer only through one unambiguous advertised KMS-primary match.
3. Add minimal per-sink Vulkan transfer contexts, exact A-source plans, and
   paired B-local destination pools without touching the TFP strategy cache;
   require `VK_EXT_queue_family_foreign` on both devices.
4. Probe full three-slot source/destination pairs on disposable exact A/B
   contexts by running full-pool atomic `TEST_ONLY` first, then two
   source-handoff cycles with separate local/FOREIGN barriers, retained B-to-A
   completion, and real render/copy/fences; never fabricate the absent KMS
   return leg, and replay only the exact winner live.
5. Add a stable copied-render completion poller and core backend-fd dispatch
   using monotonic job id, `OutputKey`, and BO index.
6. Render into A, submit the B copy after readiness, pass B's completion to
   KMS, retain a duplicate/sentinel for A's next acquire, preserve `fd=-1`
   import semantics, and retire the source plus scene acknowledgements only on
   page flip.
7. Track exclusive FOREIGN ownership and layout provenance for A, B, and KMS;
   route readback through a waited A acquire while all framebuffer/front/
   pageflip/direct state continues to use B's destination.
8. Integrate startup fallback, runtime modesets, completion cancellation,
   topology changes, VT/DPMS, shutdown, dirty semaphore rearm, device loss,
   full-discard lifecycle normalization, and quarantine-safe Drop.
9. Cover exact/none/ambiguous sink selection, shared-before-copied order,
   route pairing, plan order, completion matching, deferred recycle, fd=-1,
   ownership transitions, failed-wait retention, and core fd routing; retain a
   live two-flip GENERAL KMS-to-B ownership smoke as an explicit external gate.
10. Update status, run nightly formatting, workspace tests, exact all-target
    Clippy, and diff checks before committing the semantic successor.
