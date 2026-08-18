# Reverse-PRIME copy implementation plan

1. Keep renderer-to-KMS route identity separate from allocation ownership and
   introduce `OutputScanout::{Shared,Copied}`.
2. Inventory exact Vulkan device/driver UUID selectors and resolve a copied
   sink renderer only through one unambiguous advertised KMS-primary match.
3. Add minimal per-sink Vulkan transfer contexts, enumerate exact one-plane
   A-to-B DRM-modifier transport plans with native nonzero modifiers before an
   explicit `DRM_FORMAT_MOD_LINEAR` fallback, pair them with every B-local
   destination plan, and keep independent optimal A render targets without
   touching the TFP strategy cache. Require
   `VK_EXT_queue_family_foreign` and
   `VK_EXT_image_drm_format_modifier` on both devices before foreign allocation.
4. Qualify full three-slot target/transport/destination pairs on fresh exact
   disposable A/B contexts. Run full-pool atomic `TEST_ONLY`, then two
   source-handoff cycles per slot with separate local/FOREIGN barriers, retained
   B-to-A completion, and real render/copy/fences. Render a token-distinct
   full-extent radial diagnostic pattern and make every pixel of A's local
   target and B's final destination contribute to compact positional,
   multi-lane per-block digests inside the existing A and B submissions. Copy
   only the digest blocks and exact tokenized corner words into small
   host-visible result buffers, preferring `HOST_CACHED` memory when available.
   After both fences succeed, require the expected corner words, equal
   corresponding digest blocks, and cross-cycle freshness. Digest admission is
   deliberately probabilistic rather than collision-free. Enable it only when
   the selected A and B queues independently support compute and each
   endpoint's input fits its `maxStorageBufferRange`; otherwise retain full
   exact CPU comparison as the correctness-preserving capability fallback.
   Reducer infrastructure that cannot be prepared safely before submission
   also takes that fallback; post-submit uncertainty is `Indeterminate`, never
   route incompatibility. Preserve requested full-output allocation, both
   full-image GPU copies, and the complete three-slot/two-cycle probe.
5. Attach liveness timing to work, not to cold setup. Give every copy-free BO
   fence and every copied A or B fence its own fresh 200 ms monotonic completion
   window, including that device's block-digest reduction and compact result
   write. Keep context/pipeline creation, allocation, atomic `TEST_ONLY`, and
   CPU verdict parsing outside that window; the exact CPU fallback also remains
   outside it and may therefore reach the whole-helper watchdog, yielding
   `Indeterminate`. A successful fence always proceeds to the content verdict
   even when total elapsed time exceeds 200 ms. Other safe, route-specific
   pre-submit failures and mismatches after proven completion continue candidate
   order. Fence-proven disposable teardown skips redundant device-wide idle;
   an incomplete or uncertain submission is terminal and must never enter
   `vkDeviceWaitIdle`.
6. Split qualification from live replay. Persist only the exact resource-free
   winning plan, then prepare a complete live pool and repeat every framebuffer
   `TEST_ONLY` while the old topology remains installed. Revalidate staleness,
   quiesce only immediately before the real commit, install the prepared plan,
   and mark its exact front `OnScreen`. Failure before that final handoff needs
   no old-framebuffer restore. Startup retains its synchronous dumb-scanout
   rollback guard.
7. Make runtime cross-device `RRSetCrtcConfig` asynchronous at the protocol
   boundary. Park only the requesting client's FIFO behind a monotonic token so
   other clients, input, rendering, and VT handling remain responsive. Leave
   same-device and no-reallocation requests synchronous. Immediately complete a
   parked request with `Interrupted` on connector/topology, VT, provider-output-
   source, effective DPMS, or logical-screen-size invalidation; ignore a late
   helper verdict for protocol completion while still harvesting internal
   poison state.
8. Execute runtime qualification in a helper that reexecutes the exact running
   yserver image and owns a fresh Vulkan/GBM graph. Pass a duplicated KMS DRM fd
   only for exact atomic `TEST_ONLY`; never commit from the child. Because that
   fd shares the parent's open file description, require strict removal of all
   child-created mode blobs, framebuffers, and GEM handles before returning
   ordinary `Compatible` or `Rejected`. Map uncertain GPU work, cleanup,
   launch/IPC ownership, or watchdog expiry to `Indeterminate`, poison the
   involved resources, and terminate the child without normal Vulkan Drop. Add
   a 30 s whole-helper watchdog and parent-death kill independently of the
   per-fence 200 ms windows.
9. Schedule helpers through a deterministic global FIFO with one active probe.
   Do not claim per-display parallelism while validity uses a global topology
   epoch. Treat resource-scoped epochs and ordered multi-helper admission as a
   future stage. Keep startup qualification and the parent's final live
   allocation, `TEST_ONLY`, quiesce, and install synchronous for now.
10. Add a stable copied-render completion poller and core backend-fd dispatch
    using monotonic job id, `OutputKey`, and BO index.
11. Render into A's optimal target, copy into the selected external transport,
    submit the B copy after readiness, pass B's completion to KMS, retain a
    duplicate/sentinel for A's next transport acquire, preserve `fd=-1` import
    semantics, and retire the paired slot plus scene acknowledgements only on
    page flip.
12. Track exclusive FOREIGN ownership and layout provenance for the A/B
    transport, B destination, and KMS; keep copied validation anchored to A's
    local optimal target while all framebuffer/front/pageflip/direct state uses
    B's destination. Integrate installed-pool completion cancellation, VT/DPMS,
    shutdown, dirty semaphore rearm, device loss, full-discard lifecycle
    normalization, and quarantine-safe Drop. Keep synchronous sink
    `device_wait_idle` for live B-copy/atomic failure recovery separate from
    disposable helper containment.
13. Hide legacy DRI3 without explicit modifier/layout import only when another
    inventoried Vulkan renderer may supply PRIME buffers; retain verified
    display-only splits, with a conservative multi-KMS rule for an unverified
    selected renderer.
14. Cover exact/none/ambiguous sink selection, shared-before-copied order,
    native-before-LINEAR tiers, stable pairing, exact replay, per-fence timing,
    full-extent positional multi-lane digest coverage, exact corner words,
    cross-cycle freshness, `HOST_CACHED`-preferred compact results, independent
    A/B compute and `maxStorageBufferRange` eligibility, exact CPU fallback,
    reducer setup fallback, safe-failure continuation, uncertain submission
    containment,
    strict shared-DRM cleanup, helper watchdog and parent lifetime, FIFO request
    parking, prompt stale interruption, late-result suppression, and
    deterministic single-flight admission. Retain a live
    two-flip GENERAL KMS-to-B ownership and display-interpretation smoke as an
    explicit external gate. Update status, run nightly formatting, workspace
    tests, exact all-target Clippy, and diff checks before committing the
    semantic successor.
