use super::*;

impl SubmittedOp {
    /// Phase B.2 Mechanism 3 helper: attach a retired
    /// `BatchResource` to this op. Called via
    /// `RenderEngineInner::adopt_retired_resource_for_gpu_retirement`
    /// case (b) when `submitted.back` is the newest fence owner.
    #[allow(
        dead_code,
        reason = "B.2 Task 1: case (b) of adopt_retired_resource_for_gpu_retirement \
                  feeds this. The helper is wired in this commit; the first call \
                  site from a real grow event lands once the _legacy paths route \
                  their ensure_returning_old returns through the engine helper."
    )]
    fn append_retired_scratch(
        &mut self,
        boxed: Box<dyn crate::kms::render::batch_resource::BatchResource>,
    ) {
        self.retired_resources.push(boxed);
    }

    /// Phase B.2 Mechanism 3 helper: drain the per-op retired
    /// `BatchResource`s for release at retirement. Caller calls
    /// `release(&vk)` per Box.
    fn drain_retired_scratch(
        &mut self,
    ) -> std::vec::Drain<'_, Box<dyn crate::kms::render::batch_resource::BatchResource>> {
        self.retired_resources.drain(..)
    }
}

impl RenderEngineInner {
    /// Phase B.2 Mechanism 3: route a retired scratch
    /// `BatchResource` (returned by
    /// [`crate::kms::vk::dst_readback::DstReadback::ensure_returning_old`]
    /// or
    /// [`crate::kms::vk::mask_scratch::MaskScratch::ensure_image_size_returning_old`]
    /// on a grow) to the right fence-gated owner.
    ///
    /// **Crucially:** `BatchResource::release(self: Box<Self>, &VkContext)`
    /// is explicit (see
    /// `crates/yserver/src/kms/scheduler/paint_batch.rs:146-147`).
    /// The trait does NOT implement `Drop` for Vk-handle teardown;
    /// dropping a `Box<dyn BatchResource>` without calling
    /// [`release`](crate::kms::render::batch_resource::BatchResource::release)
    /// would LEAK the underlying Vk handles. Every retirement path
    /// MUST call `boxed.release(&inner.vk)` explicitly.
    ///
    /// Ownership cases (in order of precedence):
    /// - (a) Open frame's [`FramePinSet::retired_resources`]
    ///   (`self.frame_builder.open.as_mut().unwrap().pins`). The pin
    ///   set rides the frame's `FenceTicket`; the
    ///   `pending_frames` retire walk in
    ///   [`RenderEngine::poll_retired`] /
    ///   [`RenderEngine::drain_all`] releases each entry once the
    ///   ticket signals. Under B.2's grow-before-open rule (Phase
    ///   9A — to land in a later Task), this case is rarely hit
    ///   because every grow forces a close-reopen before any
    ///   in-frame op runs. Wiring it now keeps the helper complete
    ///   for B.3+ when mid-frame retire becomes possible.
    /// - (b) Newest [`SubmittedOp`] on `self.submitted`. After
    ///   `close_open_frame` succeeds the just-closed frame's CB has
    ///   appended one `SubmittedOp` carrying the frame's ticket;
    ///   attaching the retired Box here rides that fence. For
    ///   legacy callers (per-op submits), `submitted.back` is
    ///   likewise the newest fence owner. Using `submitted.back`
    ///   instead of `pending_frames.back` guarantees we pick the
    ///   NEWEST in-flight ticket (legacy SubmittedOps queued AFTER
    ///   a frame close are newer than the frame's record).
    /// - (c) Explicit release if both `frame_builder.open` is None
    ///   AND `submitted` is empty. Safe because no in-flight CB
    ///   can still be sampling the retired backing.
    ///
    ///   **M1 invariant assumption:** case (c) additionally
    ///   requires that `pending_group_ops` is empty at call time.
    ///   Under the Phase B M1 invariant (`submit_group_max_size = 1`
    ///   in production → auto-flush per op via
    ///   [`Self::maybe_auto_flush_submit_group`]), the parked
    ///   `Vec<SubmittedOp>` is drained at every op boundary, so a
    ///   grow that fires inside a paint op finds it empty by the
    ///   time the engine returns to a quiescent state. Tests that
    ///   raise the cap (e.g. `submit_group_max_size_for_tests(16)`)
    ///   can violate this invariant by leaving a previous paint
    ///   op's CB parked in `pending_group_ops` with a reference to
    ///   the OLD scratch's `vk::Image` handle; case (c) would then
    ///   destroy that handle before the parked CB submits. If a
    ///   later sub-phase (B.3+) relaxes M1 to allow cap>1 in
    ///   production, this helper must grow a fourth tier that
    ///   routes the retired Box onto
    ///   `pending_group_ops.back_mut()`'s retired-resources slot
    ///   (the type would need a `SubmittedOp`-style extension).
    ///   The `debug_assert!` below catches the regression in
    ///   debug builds.
    ///
    /// `None` input is a no-op (the common case: no grow fired).
    #[allow(
        dead_code,
        reason = "B.2 Task 1: helper lands now so the SubmittedOp + FramePinSet \
                  extensions compile. The _legacy ensure_returning_old call sites \
                  will be re-wired to call this helper in this same commit; the \
                  open-frame case (a) is exercised once Phase 9A's grow-before-open \
                  path lands in a later Task."
    )]
    pub(crate) fn adopt_retired_resource_for_gpu_retirement(
        &mut self,
        retired: Option<Box<dyn crate::kms::render::batch_resource::BatchResource>>,
    ) {
        let Some(boxed) = retired else { return };
        // (a) Open frame — adopt into its pin set.
        if let Some(open) = self.frame_builder.open.as_mut() {
            open.pins.adopt_retired(boxed);
            return;
        }
        // (b) Newest in-flight SubmittedOp — append to its
        //     retired_resources; the op's fence retires it.
        if let Some(submitted) = self.submitted.back_mut() {
            submitted.append_retired_scratch(boxed);
            return;
        }
        // (c) Nothing in flight — safe to release immediately.
        //     M1 invariant: pending_group_ops MUST be empty here.
        //     If a future sub-phase relaxes cap=1 auto-flush and a
        //     parked op's CB still references the retired backing,
        //     this release would destroy a live Vk handle. See the
        //     docstring above for the fix shape (fourth tier onto
        //     pending_group_ops.back_mut()).
        debug_assert!(
            self.pending_group_ops.is_empty(),
            "adopt_retired_resource_for_gpu_retirement case (c): \
             pending_group_ops must be empty under M1 (cap=1 \
             auto-flush per op). If B.3+ relaxes M1, add a fourth \
             tier that routes onto pending_group_ops.back_mut()."
        );
        boxed.release(&self.vk);
    }
}

impl RenderEngine {
    /// Production constructor. Borrows the platform's `VkContext`
    /// (cloned `Arc`); CB allocation goes through the platform's
    /// shared `OpsCommandPool` on each op.
    ///
    /// # Errors
    ///
    /// Returns `NoVk` if `platform` was built via `for_tests`
    /// (no Vk). Production paths always have Vk.
    pub(crate) fn new(platform: &PlatformBackend) -> Result<Self, RenderError> {
        let vk = platform.vk().ok_or(RenderError::NoVk)?.clone();
        // SAFETY: `physical_device` belongs to `instance`.
        let limits = unsafe {
            vk.instance
                .get_physical_device_properties(vk.physical_device)
                .limits
        };
        let upload_copy_align = upload_copy_align(limits.optimal_buffer_copy_offset_alignment);
        let descriptor_pool_ring =
            crate::kms::render::descriptor_pool_ring::DescriptorPoolRing::new(Arc::clone(&vk));
        Ok(Self {
            inner: Some(RenderEngineInner {
                vk,
                submitted: VecDeque::new(),
                staging_pool: StagingPool::default(),
                upload_arena: crate::kms::render::upload_arena::UploadArena::default(),
                upload_copy_align,
                picture_paint: HashMap::new(),
                glyph_atlas: None,
                text_pipelines: HashMap::new(),
                atlas_last_upload_ticket: None,
                render_pipelines: None,
                masked_blit: None,
                solid_src_image: None,
                solid_mask_image: None,
                white_mask_image: None,
                dst_readback: None,
                src_alias_readback: None,
                trap_pipeline: None,
                mask_scratch: None,
                drawable_view_cache: HashMap::new(),
                logic_fill_caches: HashMap::new(),
                descriptor_pool_ring,
                acquire_generation: 0,
                pending_render_batch: None,
                render_flush_records: Vec::new(),
                get_image_phase_totals: GetImagePhases::default(),
                pending_present_batches: Vec::new(),
                pending_group_ops: Vec::new(),
                pending_flush_outcomes: Vec::new(),
                pending_frames: std::collections::VecDeque::new(),
                pending_frame_close_events: Vec::new(),
                frame_seq: 0,
                frame_builder: crate::kms::render::frame_builder::FrameBuilder::new(),
                frame_builder_timeout:
                    crate::kms::render::frame_builder::FrameBuilder::timeout_from_env_default_16ms(),
                retired_promoted_images: Vec::new(),
                clip_snapshots: HashMap::new(),
                next_snapshot_id: 1,
                retired_snapshots: Vec::new(),
            }),
        })
    }

    /// Vk-less constructor — used by `KmsBackend::for_tests` and
    /// Stage 1b-era callers that haven't migrated yet. Every paint
    /// op on a stubbed engine returns `NoVk`.
    pub(crate) fn stub() -> Self {
        Self { inner: None }
    }

    /// Whether the engine has a live Vk inner. Tests use this to
    /// skip Vk-backed assertions on the stub fixture.
    pub(crate) fn is_live(&self) -> bool {
        self.inner.is_some()
    }

    /// Walk `submitted`, dropping entries whose [`FenceTicket`]
    /// has signaled. Their CB is freed and any staging buffer
    /// destroyed.
    pub(crate) fn poll_retired(&mut self, platform: &PlatformBackend) {
        let Some(inner) = self.inner.as_mut() else {
            return;
        };
        let Some(pool) = platform.ops_command_pool_handle() else {
            return;
        };
        let device = &inner.vk.device;
        // Walk front-to-back, removing prefixes that have signaled.
        // Same-queue submission order guarantees prefix-signal
        // monotonicity; if entry N's ticket is signaled, entry
        // N-1's also is. We could short-circuit on first
        // unsignaled but the loop is small enough to walk all.
        while let Some(front) = inner.submitted.front() {
            if !front.ticket.poll_signaled(&inner.vk) {
                break;
            }
            let mut op = inner.submitted.pop_front().expect("non-empty");
            unsafe {
                device.free_command_buffers(pool, &[op.cb]);
            }
            // staging drops at end of scope → destroys Vk handles. (Frame-
            // builder put_image staging lives in the frame pin-set, not here;
            // it's pooled at the pending_frames retire below.)
            drop(op.staging.take());
            // Phase B.2 Mechanism 3: release retired BatchResources
            // attached via adopt_retired_resource_for_gpu_retirement
            // case (b). BatchResource::release is explicit (no Drop);
            // dropping the Box without this call would LEAK Vk handles
            // (see paint_batch.rs:147).
            for r in op.drain_retired_scratch() {
                r.release(&inner.vk);
            }
            // Stage 5 Task 4 layer 1: signal the descriptor pool
            // ring that everything up to and including this op's
            // generation has retired. Pools whose high_water_
            // generation <= op.generation transition InFlight → Free
            // via vkResetDescriptorPool.
            inner.descriptor_pool_ring.release_up_to(op.generation);
        }
        // Phase B.1: walk pending_frames. Same ticket-signaled monotonicity
        // argument as the `submitted` loop above (same-queue submission
        // order signals tickets in order).
        while let Some(front) = inner.pending_frames.front() {
            if !front.ticket.poll_signaled(&inner.vk) {
                break;
            }
            let mut record = inner.pending_frames.pop_front().expect("non-empty");
            // Phase B.2 Mechanism 3 (defensive): release retired
            // BatchResources attached via case (a) of
            // adopt_retired_resource_for_gpu_retirement. Under B.2
            // this Vec is structurally empty — the grow-before-open
            // rule routes all retires through submitted.back — but
            // explicit release here keeps the invariant honest for
            // B.3+ when mid-frame retire becomes possible. Without
            // it, Vk handles inside the Boxes would leak (no Drop on
            // BatchResource; see paint_batch.rs:147).
            for r in record.pins.retired_resources.drain(..) {
                r.release(&inner.vk);
            }
            // #177: hand the frame's upload arena blocks back for the next
            // frames. This is the ONLY place the arena gets a block back, and
            // it runs only after this frame's ticket has signalled, so a
            // block handed out again is never read by submitted work.
            inner.upload_arena.retire(
                std::mem::take(&mut record.pins.uploads),
                std::time::Instant::now(),
            );
            // #nvidia perf: reclaim pooled put_image staging for reuse
            // instead of destroying it (avoids per-upload vkCreateBuffer/
            // vkAllocateMemory churn, costly on NVIDIA). Same fence argument.
            // The pin holds the sole staging Arc at retire (put_image's local
            // dropped after recording; RecordedPutImage keeps only an index),
            // so try_unwrap succeeds; from_pool buffers go back to the pool,
            // others drop.
            for arc in record.pins.staging_buffers.drain(..) {
                if let Ok(buf) = Arc::try_unwrap(arc)
                    && buf.from_pool
                {
                    inner.staging_pool.release(buf);
                }
            }
            // The Arcs inside the record drop here, releasing pinned resources.
            drop(record);
        }
        // #177: destroy upload blocks idle past the eviction age, so a burst
        // doesn't leave live VA ranges parked (each one makes every other
        // allocation and free on amdgpu dearer). Bounded by the idle cap even
        // when this doesn't run.
        inner.upload_arena.trim(std::time::Instant::now());
        crate::kms::vk::mem_accounting::set_upload_arena_idle_blocks(inner.upload_arena.idle_len());
        // GLX-TFP (Task 1.2): free old promotion-displaced images whose
        // guarding fence has signaled. No ordering relationship to the
        // queues above (each rides its own ticket), so retain-filter
        // rather than pop-prefix.
        if !inner.retired_promoted_images.is_empty() {
            let vk = Arc::clone(&inner.vk);
            inner.retired_promoted_images.retain(|(retired, guard)| {
                let signaled = guard.as_ref().is_none_or(|t| t.poll_signaled(&vk));
                if signaled {
                    Self::destroy_retired_image(&vk, retired);
                }
                !signaled
            });
        }
        // Task 11: drain retired clip snapshots whose guarding fence has
        // signaled. No ordering relationship to the queues above (each rides
        // its own ticket), so retain-filter rather than pop-prefix. The
        // retained tuple's `ClipSnapshot::drop` frees its Vk handles when the
        // entry is dropped by `retain`.
        if !inner.retired_snapshots.is_empty() {
            let vk = Arc::clone(&inner.vk);
            inner
                .retired_snapshots
                .retain(|(_snap, guard)| !guard.as_ref().is_none_or(|t| t.poll_signaled(&vk)));
        }
    }

    /// Destroy the Vk handles of a promotion-displaced [`RetiredImage`].
    /// Order: sample_view, image_view, image, memory (views before the
    /// image they reference; image before its backing memory). Null
    /// handles are no-ops per the Vulkan spec.
    fn destroy_retired_image(vk: &VkContext, retired: &RetiredImage) {
        unsafe {
            if retired.sample_view != vk::ImageView::null() {
                vk.device.destroy_image_view(retired.sample_view, None);
            }
            if retired.image_view != vk::ImageView::null() {
                vk.device.destroy_image_view(retired.image_view, None);
            }
            if retired.image != vk::Image::null() {
                vk.device.destroy_image(retired.image, None);
            }
            if retired.memory != vk::DeviceMemory::null() {
                crate::kms::vk::mem_accounting::free_memory(&vk.device, retired.memory);
            }
        }
    }

    /// Push old promotion-displaced handles onto the deferred-destroy
    /// list. If `guard` is `None` or already signaled, the handles are
    /// destroyed immediately; otherwise they're parked until
    /// [`Self::poll_retired`] observes the fence signal.
    pub(super) fn retire_image_after(&mut self, retired: RetiredImage, guard: Option<FenceTicket>) {
        let Some(inner) = self.inner.as_mut() else {
            // No Vk inner: nothing to destroy against. (The handles are
            // necessarily null in the stub case.)
            return;
        };
        let ready = guard.as_ref().is_none_or(|t| t.poll_signaled(&inner.vk));
        if ready {
            Self::destroy_retired_image(&inner.vk, &retired);
        } else {
            inner.retired_promoted_images.push((retired, guard));
        }
    }

    /// Phase B.1: production-side shutdown. Closes any open frame
    /// first, then defers to `drain_all` for the existing
    /// `SubmitGroup` + submitted-queue + `pending_frames` drain.
    ///
    /// Test call sites that construct a fresh engine/platform/store
    /// and never open a frame can keep using `drain_all` directly.
    pub(crate) fn shutdown(&mut self, store: &mut DrawableStore, platform: &mut PlatformBackend) {
        if let Err(e) = self.close_open_frame(
            store,
            platform,
            crate::kms::render::frame_builder::CloseReason::Shutdown,
        ) {
            log::warn!("render shutdown: close_open_frame failed: {e:?}");
        }
        self.drain_all(platform);
    }

    /// Drain every in-flight submit, waiting on the deepest
    /// ticket. Called at shutdown to ensure all CB / staging
    /// resources are reclaimed before pool destruction.
    ///
    /// PRECONDITION: callers must close any open render batches
    /// (via `flush_render_batch`) BEFORE calling `drain_all`,
    /// because that method needs `&mut DrawableStore` which is not
    /// available here. The production call site (`disable_output`)
    /// already satisfies this. Any open batch that reaches here is
    /// dropped with a warning to avoid a non-empty/no-ticket panic
    /// on `flush_submit_group(Shutdown)`.
    pub(crate) fn drain_all(&mut self, platform: &mut PlatformBackend) {
        // Drop any open render batch that reached us without being
        // flushed. This should never happen when called from the
        // production code path (disable_output closes batches first),
        // but guards against shutdown-time panics if the invariant is
        // violated (e.g. a future call site that forgets to close
        // batches first).
        if self
            .inner
            .as_mut()
            .is_some_and(|i| i.pending_render_batch.take().is_some())
        {
            log::warn!(
                "render drain_all: open render_batch dropped without flush \
                 (caller must close batches before drain_all)"
            );
        }
        // Flush any open SubmitGroup first; this commits parked ops
        // into `submitted` so the loop below sees the right set.
        //
        // GLX-TFP: this shutdown flush drives `platform.flush_submit_group`
        // directly (no exported-write publish): at drain_all the engine
        // has no `store` borrow, and the GL consumer is going away, so
        // re-publishing write fences here is pointless. The engine's
        // commit-of-parked-ops bookkeeping below is replicated from the
        // `flush_submit_group` wrapper.
        let result =
            platform.flush_submit_group(crate::kms::render::submit_group::FlushReason::Shutdown);
        if let Some(outcome) = platform.take_last_flush_outcome()
            && let Some(inner) = self.inner.as_mut()
        {
            inner.pending_flush_outcomes.push(outcome);
        }
        match (result, self.inner.as_mut()) {
            (Ok(_), Some(inner)) => {
                for op in inner.pending_group_ops.drain(..) {
                    inner.submitted.push_back(op);
                }
            }
            (Err(e), inner_opt) => {
                if let Some(inner) = inner_opt {
                    inner.pending_group_ops.clear();
                }
                log::warn!("render drain_all: flush_submit_group failed: {e:?}");
            }
            (Ok(_), None) => {}
        }
        let Some(inner) = self.inner.as_mut() else {
            return;
        };
        let Some(pool) = platform.ops_command_pool_handle() else {
            return;
        };
        let device = &inner.vk.device;
        // Wait on each ticket in order. Off-hot-path; one wait
        // per pending op is fine at shutdown.
        while let Some(mut op) = inner.submitted.pop_front() {
            // A failed wait (lost device) leaves the CB pending: leak it
            // (VUID-vkFreeCommandBuffers-pCommandBuffers-00047).
            if op.ticket.wait(&inner.vk).is_ok() {
                unsafe {
                    device.free_command_buffers(pool, &[op.cb]);
                }
            }
            drop(op.staging.take());
            // Phase B.2 Mechanism 3: explicit release of retired
            // BatchResources attached via case (b). See
            // poll_retired for the rationale (BatchResource has no
            // Drop — paint_batch.rs:147).
            for r in op.drain_retired_scratch() {
                r.release(&inner.vk);
            }
            inner.descriptor_pool_ring.release_up_to(op.generation);
        }
        // #nvidia perf: destroy pooled upload staging buffers (all submitted
        // work above is waited out, so none is in flight).
        inner.staging_pool.drain();
        let arena = inner.upload_arena.stats();
        log::info!(
            "upload arena: suballocs={} suballoc_bytes={} dedicated={} block_allocs={} \
             block_reuses={} returned={} rejected={} evicted={} idle={}",
            arena.suballocs,
            arena.suballoc_bytes,
            arena.dedicated,
            arena.block_allocs,
            arena.block_reuses,
            arena.returned,
            arena.rejected,
            arena.evicted,
            inner.upload_arena.idle_len(),
        );
        inner.upload_arena.drain();
        crate::kms::vk::mem_accounting::set_upload_arena_idle_blocks(0);
        // Phase B.1: drain in-flight frame pins. wait() ensures Vk-side
        // completion before the Arc<StagingBuffer> drops would otherwise
        // race with GPU reads. Off-hot-path; one wait per pending frame
        // is fine at shutdown.
        while let Some(mut record) = inner.pending_frames.pop_front() {
            let _ = record.ticket.wait(&inner.vk);
            // Phase B.2 Mechanism 3 (defensive): release retired
            // BatchResources attached via case (a). See poll_retired
            // for the rationale.
            for r in record.pins.retired_resources.drain(..) {
                r.release(&inner.vk);
            }
            // Record drops; pins drop; Arcs decrement.
            drop(record);
        }
        // GLX-TFP (Task 1.2): wait out + free any promotion-displaced
        // images still parked. The earlier `submitted` / `pending_frames`
        // waits don't necessarily cover their guarding tickets (a guard
        // can be a foreign ticket), so wait each explicitly.
        let vk = Arc::clone(&inner.vk);
        for (retired, guard) in inner.retired_promoted_images.drain(..) {
            if let Some(t) = guard.as_ref() {
                let _ = t.wait(&vk);
            }
            Self::destroy_retired_image(&vk, &retired);
        }
        // Task 11 (codex round-5 finding 8): release any clip snapshots parked
        // in `retired_snapshots`; covering only `poll_retired` leaks the last
        // batch at teardown. Wait out each guard (foreign tickets may not be
        // covered by the waits above), then drop — `ClipSnapshot::drop` frees
        // the Vk objects.
        for (snap, guard) in inner.retired_snapshots.drain(..) {
            if let Some(t) = guard.as_ref() {
                let _ = t.wait(&vk);
            }
            drop(snap);
        }
    }

    /// Phase A Task 3.5: total active staging + scratch bytes
    /// across both submitted (in-flight) and parked (pending_group)
    /// ops. Used for per-tick high-water sampling. Returns
    /// `(staging_bytes, scratch_bytes)`.
    pub(crate) fn active_resource_bytes(&self) -> (u64, u64) {
        let Some(inner) = self.inner.as_ref() else {
            return (0, 0);
        };
        let staging_submitted: u64 = inner
            .submitted
            .iter()
            .map(|op| op.staging.as_ref().map_or(0, |s| s.size))
            .sum();
        let staging_parked: u64 = inner
            .pending_group_ops
            .iter()
            .map(|op| op.staging.as_ref().map_or(0, |s| s.size))
            .sum();
        let scratch_submitted: u64 = inner
            .submitted
            .iter()
            .map(|op| {
                op.scratch.iter().map(|s| s.size_bytes()).sum::<u64>()
                    + op.sampled_scratch.iter().map(|s| s.size_bytes).sum::<u64>()
            })
            .sum();
        let scratch_parked: u64 = inner
            .pending_group_ops
            .iter()
            .map(|op| {
                op.scratch.iter().map(|s| s.size_bytes()).sum::<u64>()
                    + op.sampled_scratch.iter().map(|s| s.size_bytes).sum::<u64>()
            })
            .sum();
        (
            staging_submitted + staging_parked,
            scratch_submitted + scratch_parked,
        )
    }

    /// Task 3 test helper: allocate a pixmap drawable in `store` backed
    /// by a real Vk storage. Returns the `DrawableId`.
    #[cfg(test)]
    pub(crate) fn create_pixmap(
        &self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        xid: u32,
        w: u16,
        h: u16,
        depth: u8,
    ) -> Result<DrawableId, RenderError> {
        let storage = platform
            .allocate_drawable_storage(w, h, depth)
            .map_err(RenderError::Vk)?;
        store
            .allocate(
                xid,
                crate::kms::render::store::DrawableKind::Pixmap,
                depth,
                false,
                storage,
            )
            .map_err(|_| RenderError::NoVk)
    }

    /// Stage 3c: invalidate any cached drawable views referencing
    /// `id`. Called by `KmsBackend` after a drawable has actually
    /// retired (storage destroyed); evicting earlier would leave
    /// dangling Vk handles since `vk::ImageView`'s underlying image
    /// is gone.
    pub(crate) fn notify_drawable_retired(&mut self, id: DrawableId) {
        self.invalidate_drawable_views(id);
    }
}

impl Drop for RenderEngine {
    fn drop(&mut self) {
        // Best-effort drain — any submitted ops that didn't go
        // through `drain_all` would leak CBs. The `Drop` here
        // can't access the platform's pool any more, but it can
        // wait on each fence so `StagingBuffer`'s drop is safe.
        if let Some(inner) = self.inner.as_mut() {
            // Collect VkContext clone up front so we can release
            // BatchResources without borrow conflicts against the
            // submitted/pending_frames iteration below.
            let vk = Arc::clone(&inner.vk);
            // Drain cached drawable views. `notify_drawable_retired`
            // is the runtime per-drawable-destroy invalidation hook
            // but currently nobody calls it (filed as a separate
            // known-issue), so at shutdown the entire cache is
            // resident and every cached `VkImageView` would leak.
            // VkImageView is destroyable independently of its image
            // (Vulkan spec), so even cache entries whose underlying
            // image has already been destroyed via the runtime path
            // are safe to destroy here.
            for (_, cached) in inner.drawable_view_cache.drain() {
                unsafe { vk.device.destroy_image_view(cached.view, None) };
            }
            for mut op in inner.submitted.drain(..) {
                let _ = op.ticket.wait(&vk);
                // Phase B.2 Mechanism 3: explicit release of any
                // retired BatchResources attached to this op. Drop
                // would LEAK the underlying Vk handles
                // (BatchResource::release is `self: Box<Self>` —
                // paint_batch.rs:147). Must run BEFORE moving
                // `op.staging` out (the iterator hands us a `mut op`
                // and `drain_retired_scratch` requires `&mut op`).
                for r in op.drain_retired_scratch() {
                    r.release(&vk);
                }
                // staging drops here.
                drop(op.staging);
                // CB handles leak — caller should have invoked
                // `drain_all` against a live platform pool. The
                // pool's own Drop destroys the pool, which
                // implicitly frees all its CBs (Vulkan spec).
                let _ = op.cb;
            }
            for mut record in inner.pending_frames.drain(..) {
                let _ = record.ticket.wait(&vk);
                // Phase B.2 Mechanism 3 (defensive): release any
                // retired BatchResources attached to the frame's
                // pin set. See submitted loop above for rationale.
                for r in record.pins.retired_resources.drain(..) {
                    r.release(&vk);
                }
                drop(record); // pins (Arcs) decrement here
            }
        }
    }
}
