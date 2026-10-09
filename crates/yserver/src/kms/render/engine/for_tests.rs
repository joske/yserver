use super::*;

impl RenderEngine {
    /// Task 12 test-only: current_layout of a registered snapshot, or `None`.
    #[allow(dead_code, reason = "Task 12 rollback test (acceptance)")]
    pub(crate) fn clip_snapshot_layout_for_tests(&self, id: SnapshotId) -> Option<vk::ImageLayout> {
        self.inner
            .as_ref()?
            .clip_snapshots
            .get(&id)
            .map(|s| s.current_layout)
    }

    /// Task 12 test-only: whether a registered snapshot has a `last_render_ticket`.
    #[allow(dead_code, reason = "Task 12 rollback test (acceptance)")]
    pub(crate) fn clip_snapshot_has_ticket_for_tests(&self, id: SnapshotId) -> Option<bool> {
        self.inner
            .as_ref()?
            .clip_snapshots
            .get(&id)
            .map(|s| s.last_render_ticket.is_some())
    }

    /// Task 12 test-only: invoke `masked_copy_area` with the mask sourced from a
    /// registered clip SNAPSHOT (`snapshot_id: Some`), exercising the snapshot
    /// first-touch + terminal-state commit + close-failure rollback path.
    #[allow(dead_code, reason = "Task 12 rollback test (acceptance)")]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn masked_copy_area_with_snapshot_for_tests(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        src: DrawableId,
        dst: DrawableId,
        sid: SnapshotId,
        src_pos: vk::Offset2D,
        dst_pos: vk::Offset2D,
        extent: vk::Extent2D,
        clip_origin: [i32; 2],
        scissors: &[vk::Rect2D],
    ) -> Result<(), RenderError> {
        let mask = {
            let inner = self.inner.as_ref().ok_or(RenderError::NoVk)?;
            let snap = inner.clip_snapshots.get(&sid).expect("snapshot");
            MaskedCopyMask {
                image: snap.image,
                view: snap.view,
                old_layout: snap.current_layout,
                extent: snap.extent,
                clip_origin,
                snapshot_id: Some(sid),
            }
        };
        self.masked_copy_area(
            store,
            platform,
            Src::server_internal(src),
            Dst::server_internal(dst),
            src_pos,
            dst_pos,
            extent,
            mask,
            scissors,
        )
    }

    /// Phase A: count of ops parked in pending_group_ops (not yet
    /// committed to `submitted`). Test helper — also used by the
    /// backend wrapper exposed to acceptance integration tests.
    pub(crate) fn pending_group_ops_count_for_tests(&self) -> usize {
        self.inner.as_ref().map_or(0, |i| i.pending_group_ops.len())
    }

    /// Phase B.3 (N8) test helper: scratch vec length of the most recently
    /// submitted op. Used by `b3_close_path_scratch_walk_yields_empty_for_no_copy_area_frames`
    /// integration test to verify the close-path walk's Vec<ScratchImage>.
    pub(crate) fn most_recent_submitted_op_scratch_len_for_tests(&self) -> usize {
        self.inner
            .as_ref()
            .and_then(|i| i.pending_group_ops.last().or_else(|| i.submitted.back()))
            .map_or(0, |op| op.scratch.len() + op.sampled_scratch.len())
    }

    /// Phase A T9: CB handles of ops parked in `pending_group_ops`
    /// in append order. Used by ordering-invariant tests that need
    /// to match a CB handle observed during recording against the
    /// handle visible in the SubmitGroup's `peek_entries` slice.
    #[cfg(test)]
    pub(crate) fn pending_group_ops_cbs_for_tests(&self) -> Vec<vk::CommandBuffer> {
        self.inner.as_ref().map_or_else(Vec::new, |i| {
            i.pending_group_ops.iter().map(|op| op.cb).collect()
        })
    }

    /// True if either the frame builder has an open frame OR a render-
    /// composite coalescing batch is currently open (CB recorded but
    /// not yet submitted). Used by the eager-touch regression tests and by
    /// `KmsBackend::has_pending_batches_for_tests` (the wrapper
    /// the acceptance test asserts on).
    ///
    /// Phase B.3 (N10): the frame builder's open frame is the
    /// equivalent of "pending COW work" after the cow-batch deletion.
    pub fn has_pending_batches_for_tests(&self) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|i| i.frame_builder.is_open() || i.pending_render_batch.is_some())
    }

    /// Phase B.2 Task 3 test introspection: set `acquire_generation`
    /// directly. The Mechanism 2 integration test uses this to seed
    /// a known baseline before opening a frame so the assertions on
    /// the captured `frame_generation` are deterministic.
    pub(crate) fn set_acquire_generation_for_tests(&mut self, value: u64) {
        if let Some(inner) = self.inner.as_mut() {
            inner.acquire_generation = value;
        }
    }

    /// Phase B.2 Task 3 test introspection: drive the engine's
    /// frame-builder open path. Bumps `acquire_generation` and calls
    /// `FrameBuilder::open_for_paint(ticket, frame_generation)` —
    /// the same shape the production callers use. Used by the
    /// Mechanism 2 integration test to exercise the watermark
    /// without going through a real paint op.
    pub(crate) fn open_frame_for_paint_for_tests(&mut self, ticket: FenceTicket) {
        let Some(inner) = self.inner.as_mut() else {
            return;
        };
        debug_assert!(
            !inner.frame_builder.is_open(),
            "open_frame_for_paint_for_tests while a frame is open"
        );
        inner.acquire_generation = inner.acquire_generation.saturating_add(1);
        let frame_generation = inner.acquire_generation;
        inner.frame_builder.open_for_paint(ticket, frame_generation);
    }

    /// Phase B.2 Task 3 test introspection: invoke
    /// `RenderEngineInner::acquire_descriptor_set_for_frame_or_op`
    /// with a caller-supplied layout. Returns the raw Vk handle.
    /// The Mechanism 2 integration test uses this to assert the
    /// helper's behavior (uses `open.frame_generation` when a frame
    /// is open; bumps `acquire_generation` otherwise).
    pub(crate) fn acquire_descriptor_set_for_frame_or_op_for_tests(
        &mut self,
        layout: vk::DescriptorSetLayout,
    ) -> Result<vk::DescriptorSet, vk::Result> {
        let inner = self
            .inner
            .as_mut()
            .ok_or(vk::Result::ERROR_INITIALIZATION_FAILED)?;
        inner.acquire_descriptor_set_for_frame_or_op(layout)
    }

    /// Phase B.2 Task 3 test introspection: close the open frame
    /// with `CloseReason::Timeout`. Mirrors
    /// `close_open_frame_if_timed_out` but unconditionally closes
    /// (so the test doesn't have to wait for the wall-clock timeout
    /// to elapse).
    pub(crate) fn close_open_frame_for_timeout_for_tests(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
    ) -> Result<(), RenderError> {
        if !self
            .inner
            .as_ref()
            .is_some_and(|i| i.frame_builder.is_open())
        {
            return Ok(());
        }
        match self.close_open_frame(
            store,
            platform,
            crate::kms::render::frame_builder::CloseReason::Timeout,
        )? {
            crate::kms::render::frame_builder::CloseOutcome::Submitted { .. }
            | crate::kms::render::frame_builder::CloseOutcome::AlreadyClosed => Ok(()),
        }
    }
}
