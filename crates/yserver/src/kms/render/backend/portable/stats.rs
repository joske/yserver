use super::*;

impl KmsBackend {
    /// Telemetry accessor — used by the acceptance harness to
    /// read lifetime counters after driving a test sequence.
    #[must_use]
    pub fn telemetry(&self) -> &Telemetry {
        &self.telemetry
    }

    // ── Stage 5 Task 3 diagnostic instrumentation helpers ───────
    //
    // These exist purely to wire `YSERVER_SUBMIT_TRACE` event
    // recording at every `vkQueueSubmit2` site with minimal
    // per-site boilerplate. Zero hot-path cost when the env var
    // is unset (`Telemetry::record_submit_event` early-returns on
    // `submit_trace.is_none()`).

    /// Classify a v2 drawable by kind for the trace TSV's
    /// `target_kind` column. COW is held separately on
    /// `KmsBackend`; other kinds come from `DrawableKind`.
    pub(in crate::kms::render::backend) fn submit_target_kind(&self, id: DrawableId) -> TargetKind {
        if self.cow_id == Some(id) {
            return TargetKind::Cow;
        }
        match self.store.get(id).map(|d| d.kind) {
            Some(DrawableKind::Root) => TargetKind::Root,
            Some(DrawableKind::Window) => TargetKind::Window,
            Some(DrawableKind::Pixmap) => TargetKind::Pixmap,
            Some(DrawableKind::Cursor) => TargetKind::Cursor,
            Some(DrawableKind::RedirectedBacking) => TargetKind::Backing,
            None => TargetKind::Unknown,
        }
    }

    /// Emit one trace event for a plain paint submit (no
    /// render-key info). One-liner helper for the ~12 sites
    /// that don't need op/src/mask plumbing.
    pub(in crate::kms::render::backend) fn trace_simple(
        &mut self,
        kind: SubmitKind,
        target: DrawableId,
        batch_size: u32,
    ) {
        let target_kind = self.submit_target_kind(target);
        self.telemetry.record_submit_event(SubmitEvent {
            frame_id: 0,
            kind,
            target_kind,
            target_id: target.as_u64(),
            batch_size,
            op: SubmitOp::None,
            src_class: SrcClass::None,
            mask_class: SrcClass::None,
            pipeline_id: None,
            flags: SubmitFlags::NONE,
        });
    }

    /// Emit one trace event for a RENDER paint submit with full
    /// op + src/mask class info. `mask` of `None` writes
    /// `no_mask`; `Some(class)` writes the class.
    pub(in crate::kms::render::backend) fn trace_render(
        &mut self,
        kind: SubmitKind,
        target: DrawableId,
        batch_size: u32,
        op_byte: u8,
        src: SrcClass,
        mask: Option<SrcClass>,
        flags: SubmitFlags,
    ) {
        let target_kind = self.submit_target_kind(target);
        self.telemetry.record_submit_event(SubmitEvent {
            frame_id: 0,
            kind,
            target_kind,
            target_id: target.as_u64(),
            batch_size,
            op: SubmitOp::from_pict_op_byte(op_byte),
            src_class: src,
            mask_class: mask.unwrap_or(SrcClass::NoMask),
            pipeline_id: None,
            flags,
        });
    }

    /// Classify a `PictureRecord` for the `src_class` /
    /// `mask_class` columns. Used by render-path call sites.
    fn picture_src_class(record: &PictureRecord) -> SrcClass {
        match record {
            PictureRecord::Drawable { .. } => SrcClass::Direct,
            PictureRecord::SolidFill { .. } => SrcClass::Solid,
            PictureRecord::LinearGradient { .. } => SrcClass::GradientLinear,
            PictureRecord::RadialGradient { .. } => SrcClass::GradientRadial,
        }
    }

    /// Lookup a picture xid in `core.pictures` and return its
    /// class, or `SrcClass::Direct` if the xid doesn't resolve
    /// (rare — render sites already guard on `resolve_*`; this
    /// is a defensive default for the diagnostic).
    pub(in crate::kms::render::backend) fn picture_src_class_by_xid(&self, xid: u32) -> SrcClass {
        self.core
            .pictures
            .get(&xid)
            .map_or(SrcClass::Direct, Self::picture_src_class)
    }

    /// Stage 5 Task 3 POC: drain the engine's cow-batch flush
    /// records (since the last drain), bump telemetry counters,
    /// Drain the engine's queued submit/flush/frame-close records into
    /// telemetry. Runs at the end of every `maybe_composite` tick, including
    /// the dark (VT-away / DPMS-off) early returns: paint frames keep
    /// closing while dark, and `pending_flush_outcomes` has no cap, so an
    /// undrained dark period would grow it for as long as the display is off.
    pub(in crate::kms::render::backend) fn drain_paint_submit_telemetry(&mut self) {
        self.drain_render_telemetry();
        // Phase A Task 3.5: drain SubmitGroup flush outcomes and
        // route each to the matching telemetry counter.
        for outcome in self.engine.drain_flush_outcomes() {
            if outcome.aborted {
                self.telemetry.record_submit_group_abort();
            } else {
                self.telemetry
                    .record_submit_group_flush(outcome.flushed_entries, outcome.reason);
            }
        }
        // Phase A telemetry retention gauges. Sample on every tick —
        // the high-water aggregator handles bursts.
        let pool_count =
            u64::try_from(self.engine.descriptor_pool_ring_pool_count()).unwrap_or(u64::MAX);
        self.telemetry
            .record_active_descriptor_pool_high_water(pool_count);
        let (staging_bytes, scratch_bytes) = self.engine.active_resource_bytes();
        self.telemetry
            .record_active_staging_high_water(staging_bytes);
        self.telemetry
            .record_active_scratch_high_water(scratch_bytes);
        // Phase B.1 Task 21: drain frame-builder close events into telemetry.
        self.drain_frame_builder_telemetry();
        // Per-second telemetry summary emission.
        self.telemetry.maybe_emit(self.engine.pending_count());
    }

    /// Stage 5 Task 3 (render-composite generalization): drain
    /// the engine's render-batch flush records, bump telemetry
    /// counters, emit one submit-trace event per flush.
    pub(in crate::kms::render::backend) fn drain_render_telemetry(&mut self) {
        // Ahead of the early return below, and NOT at the GetImage call sites:
        // `engine.get_image` has a dozen callers and only two of them were
        // ever going to drain, so per-site draining both misattributed and
        // dropped readbacks (a session whose only reads are cursor-image ones
        // would report zero). Draining here — `maybe_composite`, ~60/s —
        // captures every site.
        if let Some(p) = self.engine.drain_get_image_phases() {
            self.telemetry
                .record_get_image_engine_phases(p.drain_ns, p.wait_ns, p.copyout_ns);
        }
        let records = self.engine.drain_render_flush_records();
        if records.is_empty() {
            return;
        }
        for rec in records {
            self.telemetry
                .record_render_batch_flushed(rec.coalesced_count);
            let target_kind = self.submit_target_kind(rec.dst);
            self.telemetry.record_submit_event(SubmitEvent {
                frame_id: 0,
                kind: SubmitKind::RenderComposite,
                target_kind,
                target_id: rec.dst.as_u64(),
                batch_size: rec.coalesced_count,
                op: SubmitOp::from_pict_op_byte(rec.op),
                src_class: SrcClass::Direct,
                mask_class: if rec.has_mask {
                    SrcClass::Direct
                } else {
                    SrcClass::NoMask
                },
                pipeline_id: None,
                flags: SubmitFlags::NONE,
            });
        }
    }

    /// Phase B.1 Task 21: drain queued `FrameCloseEvent`s into the
    /// per-second telemetry. Called from every site that drives a
    /// frame close (`maybe_composite`, `enqueue_present_completion`,
    /// `get_image`, `shutdown`/`disable_output`, `render_composite_glyphs`).
    ///
    /// Opens are delta-tracked via `last_drained_fb_opens` — the
    /// drain emits one `record_frame_builder_open` for each new open
    /// since the previous call without requiring a separate event queue.
    ///
    /// The drain is idempotent: calling it multiple times in a row is
    /// harmless (the event queue and delta are both empty after the
    /// first call).
    pub(in crate::kms::render::backend) fn drain_frame_builder_telemetry(&mut self) {
        // Delta-track opens (FrameBuilder doesn't queue open events;
        // we infer them from the monotonic lifetime counter).
        let current_opens = self.engine.frame_builder_lifetime_opens();
        let delta_opens = current_opens.saturating_sub(self.last_drained_fb_opens);
        for _ in 0..delta_opens {
            self.telemetry.record_frame_builder_open();
        }
        self.last_drained_fb_opens = current_opens;

        // Drain the close-event queue.
        for event in self.engine.drain_frame_close_events() {
            if event.aborted {
                self.telemetry.record_frame_builder_abort();
            } else {
                self.telemetry.record_frame_builder_close(
                    event.reason,
                    event.ops_in_frame,
                    event.glyph_uploads_in_frame,
                    event.renders_in_frame,
                );
            }
            // Aborts also record pin_count high water — those pins existed
            // before the failure dropped them.
            self.telemetry.record_frame_builder_active_pins_high_water(
                u64::try_from(event.pin_count).unwrap_or(u64::MAX),
            );
        }
    }

    /// Stage 5 Task 4 layer 1: test-side accessor to the ring's
    /// pool residency. Used by the acceptance harness to assert
    /// steady-state pool count stays small after warm-up.
    #[doc(hidden)]
    #[must_use]
    pub fn descriptor_pool_ring_pool_count(&self) -> usize {
        self.engine.descriptor_pool_ring_pool_count()
    }

    /// Test-only accessor for the engine's per-drawable view cache
    /// size. Used by the live-Vk integration test that gates the
    /// `notify_drawable_retired` runtime-wiring fix: pre-fix a
    /// destroyed drawable's cached views accumulated until engine
    /// `Drop`; post-fix they're invalidated synchronously via
    /// `store_decref_with_invalidate` / `poll_pending_retire_with_invalidate`.
    #[doc(hidden)]
    #[must_use]
    pub fn drawable_view_cache_len(&self) -> usize {
        self.engine.drawable_view_cache_len()
    }

    /// Stage 5 Task 4 layer 1: pull ring lifetime counter deltas
    /// into Telemetry. Called by the backend after every engine
    /// RENDER call site + retirement sweep. The bumps are
    /// independent: ring.lifetime_creates increases inside
    /// acquire_set; ring.lifetime_resets increases inside
    /// release_up_to (which only runs inside engine.poll_retired
    /// and engine.drain_all). Spec
    /// `2026-05-21-descriptor-pool-ring-design.md` § 'Telemetry'.
    pub(in crate::kms::render::backend) fn sync_descriptor_pool_telemetry(&mut self) {
        let creates_now = self.engine.descriptor_pool_creates_lifetime();
        let resets_now = self.engine.descriptor_pool_resets_lifetime();
        let creates_delta = creates_now.saturating_sub(self.last_observed_pool_creates);
        let resets_delta = resets_now.saturating_sub(self.last_observed_pool_resets);
        for _ in 0..creates_delta {
            self.telemetry.record_descriptor_pool_create();
        }
        if resets_delta > 0 {
            self.telemetry.record_descriptor_pool_reset(resets_delta);
        }
        self.last_observed_pool_creates = creates_now;
        self.last_observed_pool_resets = resets_now;
    }

    /// Once-per-method dedup helper. Each `method` name produces
    /// exactly one `warn!` per session, so a busy client doesn't
    /// drown the log.
    pub(in crate::kms::render::backend) fn log_render_gap(&self, method: &'static str) {
        if self.logged_gaps.borrow_mut().insert(method) {
            log::warn!(
                "render: {method} not yet implemented — paint or composite operation skipped"
            );
        }
    }
}
