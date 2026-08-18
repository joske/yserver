# Reverse-PRIME copy implementation plan

1. Keep renderer-to-KMS route identity separate from allocation ownership and
   introduce `OutputScanout::{Shared,Copied}`.
2. Inventory exact Vulkan device/driver UUID selectors and resolve a copied
   sink renderer only through one unambiguous advertised KMS-primary match.
3. Add minimal per-sink Vulkan transfer contexts, enumerate exact one-plane
   A-to-B DRM-modifier transport plans with native nonzero modifiers before an
   explicit `DRM_FORMAT_MOD_LINEAR` fallback, pair them with every B-local
   destination plan, and keep independent optimal A render targets without
   touching the TFP strategy cache;
   require `VK_EXT_queue_family_foreign` on both devices and
   `VK_EXT_image_drm_format_modifier` on both devices before foreign allocation.
4. Probe full three-slot target/transport/destination pairs on disposable exact
   A/B contexts by running full-pool atomic `TEST_ONLY` first, then two
   source-handoff cycles with separate local/FOREIGN barriers, retained B-to-A
   completion, and real render/copy/fences. Render a token-distinct full-extent
   radial diagnostic pattern, read back A's local target and B's final
   destination only after successful A and B fences, and require exact bytes
   plus diagnostic hashes, valid corner fiducials, and cross-cycle freshness.
   Keep full-size allocation and `TEST_ONLY` outside the timed region. Give
   every copy-free BO fence and every copied A or B fence its own fresh 200 ms
   monotonic completion window; do not use a global cold-probe or exact-plan
   deadline, and keep CPU validation outside the liveness window. Completed
   fences always proceed to the pixel verdict even when cumulative elapsed time
   exceeds 200 ms. Safe pre-submit failures and mismatches after proven
   completion continue candidate order. Mark disposable contexts quiescent
   after every submitted fence completes so their ordinary pool, pipeline, and
   context teardown skips the redundant device-wide idle; never grant that
   exemption to live or uncertain contexts. Classify failure by submission
   state: only a timeout or other post-submit failure that leaves submitted
   work incomplete or uncertain terminally stops exact-plan search and retains
   the whole disposable attempt until process exit without normal Drop or
   `vkDeviceWaitIdle`. Never fabricate the absent KMS return leg, and replay
   only the exact winner live. Propagate terminal status through startup and
   runtime: startup retains already-built GPU owners and aborts later outputs,
   while runtime skips scene/Vulkan teardown and restores only the unchanged
   old KMS topology before returning failure.
5. Add a stable copied-render completion poller and core backend-fd dispatch
   using monotonic job id, `OutputKey`, and BO index.
6. Render into A's optimal target, copy into the selected external transport,
   submit the B copy after readiness, pass B's completion to KMS, retain a
   duplicate/sentinel for A's next transport acquire, preserve `fd=-1` import
   semantics, and retire the paired slot plus scene acknowledgements only on
   page flip.
7. Track exclusive FOREIGN ownership and layout provenance for the A/B
   transport, B destination, and KMS; keep copied readback on A's local optimal
   target while all framebuffer/front/pageflip/direct state uses B's
   destination.
8. Integrate startup fallback, runtime modesets, completion cancellation,
   topology changes, VT/DPMS, shutdown, dirty semaphore rearm, device loss,
   full-discard lifecycle normalization, terminal disposable-probe quarantine,
   fence-proven disposable teardown, and quarantine-safe Drop. Keep synchronous
   sink `device_wait_idle` for live B-copy/atomic failure recovery separate from
   the disposable timeout path.
9. Hide legacy DRI3 without explicit modifier/layout import only when another
   inventoried Vulkan renderer may supply PRIME buffers; retain verified
   display-only splits, with a conservative multi-KMS rule for an unverified
   selected renderer.
10. Cover exact/none/ambiguous sink selection, shared-before-copied order,
   native-before-LINEAR transport tiers, stable modifier/destination order,
   modifier-0 deduplication, route pairing, exact plan replay, completion
   matching, deferred recycle, fd=-1,
   ownership transitions, local-pixel validity, A/B readback equality, first
   mismatch diagnostics, stale-cycle rejection, corner fiducials, failed-wait
   retention, per-fence completion-window accounting, completed-fence
   validation beyond nominal elapsed time, submission-state timeout
   classification, safe-failure continuation, uncertain-attempt retention
   without `vkDeviceWaitIdle`, and core fd routing; retain a live two-flip
   GENERAL KMS-to-B ownership and display-interpretation smoke as an explicit
   external gate.
11. Update status, run nightly formatting, workspace tests, exact all-target
    Clippy, and diff checks before committing the semantic successor.
