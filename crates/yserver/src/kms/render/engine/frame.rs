use super::*;

/// Pure fold over the per-op classification. `prev_pass_dst` drives the
/// all-kinds `coalescable` count; `open_composite_dst` drives the
/// composite-only `mergeable` count (reset by ANY non-composite op);
/// `prev_pass_was_composite` lets a non-mergeable coalescable pass be
/// attributed to the dirty-clear vs cross-kind bucket.
pub(super) fn coalescing_counts(
    classes: impl IntoIterator<Item = CoalesceClass>,
) -> CoalesceCounts {
    let mut prev_pass_dst: Option<DrawableId> = None;
    let mut prev_pass_was_composite = false;
    let mut open_composite_dst: Option<DrawableId> = None;
    let mut c = CoalesceCounts::default();
    for class in classes {
        match class {
            CoalesceClass::NonPass => {
                prev_pass_dst = None;
                prev_pass_was_composite = false;
                open_composite_dst = None;
            }
            CoalesceClass::PassNonComposite { dst, .. } => {
                c.pass_ops += 1;
                if dst.is_some() && dst == prev_pass_dst {
                    // A non-composite repeat is never reachable by the
                    // composite-only slices — pure cross-kind work.
                    c.coalescable += 1;
                    c.coalescable_cross_kind += 1;
                }
                prev_pass_dst = dst;
                prev_pass_was_composite = false;
                // A non-composite pass op breaks the consecutive-composite
                // run that Slice 1 keys on.
                open_composite_dst = None;
            }
            CoalesceClass::Composite {
                dst,
                self_samples,
                folder_clean,
                dirty_clear_only,
            } => {
                c.pass_ops += 1;
                let dst = Some(dst);
                let coalescable_hit = dst == prev_pass_dst;
                if coalescable_hit {
                    c.coalescable += 1;
                }
                if self_samples {
                    c.self_sample += 1;
                }
                if folder_clean && open_composite_dst == dst {
                    c.mergeable += 1; // Slice 1 removes this pass.
                // session stays open on the same dst
                } else {
                    // Not foldable today. If it's a coalescable pass,
                    // attribute it: a clear-only block on a consecutive
                    // same-dst composite is the Slice-1.5 prize; anything
                    // else needs the cross-kind session.
                    if coalescable_hit {
                        if dirty_clear_only && prev_pass_was_composite {
                            c.coalescable_dirty_clear += 1;
                        } else {
                            c.coalescable_cross_kind += 1;
                        }
                    }
                    if self_samples {
                        // Direct src/mask==dst aliasing is a feedback loop —
                        // a hard render-pass boundary, opens nothing foldable.
                        open_composite_dst = None;
                    } else {
                        // Opens (or re-anchors) a foldable session on this
                        // dst. Solid-clear and dst-readback composites still
                        // open one: their pre-pass transfer work runs, then
                        // they leave dst in COLOR for clean followers.
                        open_composite_dst = dst;
                    }
                }
                prev_pass_dst = dst;
                prev_pass_was_composite = true;
            }
        }
    }
    c
}

fn classify_recorded_op(op: &crate::kms::render::frame_builder::RecordedOp) -> CoalesceClass {
    use crate::kms::render::frame_builder::RecordedOp;
    match op {
        RecordedOp::RenderComposite(rc) => {
            let self_samples = rc.src_view == rc.dst_view || rc.mask_view == rc.dst_view;
            let has_clear = rc.src_clear_color.is_some() || rc.mask_clear_color.is_some();
            let reads_dst = rc.src_alias_view.is_some() || rc.needs_dst_readback || self_samples;
            let folder_clean = !has_clear && !reads_dst;
            // Blocked only by a solid clear (no dst self-read): a per-op
            // solid scratch would lift the block.
            let dirty_clear_only = has_clear && !reads_dst;
            CoalesceClass::Composite {
                dst: rc.dst_id,
                self_samples,
                folder_clean,
                dirty_clear_only,
            }
        }
        // Slice-2 phase 3: fold-clean composite is now session-eligible too
        // (handled via the `CoalesceClass::Composite { folder_clean: true }`
        // arm in `session_eligible`). Among the non-composite pass kinds,
        // fill + logic_fill are still the ONLY session-eligible ones; glyph /
        // image_text / traps stay standalone (text.rs owns its pass; traps
        // target a different attachment).
        RecordedOp::FillRect(_) | RecordedOp::LogicFill(_) => CoalesceClass::PassNonComposite {
            dst: op.dst_id(),
            is_fill_or_logic: true,
        },
        RecordedOp::CompositeGlyphs(_)
        | RecordedOp::ImageText(_)
        | RecordedOp::RenderTrapsOrTris(_) => CoalesceClass::PassNonComposite {
            dst: op.dst_id(),
            is_fill_or_logic: false,
        },
        _ => CoalesceClass::NonPass,
    }
}

/// Slice-2 phase-3 session eligibility = fill / logic_fill + FOLD-CLEAN
/// composite. A `folder_clean` composite has NO pre-pass transfer (no solid
/// clear, no src-alias/dst-readback copy) and does NOT self-sample, so it is
/// safe to draw mid-session (clears/readback are illegal inside an open
/// `begin_rendering`). A composite with `folder_clean == false` (solid clear,
/// dst readback, or self-sample) stays INELIGIBLE → flush + standalone.
/// Glyph, image_text, traps, and every non-pass op remain INELIGIBLE.
fn session_eligible(class: &CoalesceClass) -> Option<DrawableId> {
    match class {
        CoalesceClass::PassNonComposite {
            dst: Some(dst),
            is_fill_or_logic: true,
        } => Some(*dst),
        CoalesceClass::Composite {
            dst,
            folder_clean: true,
            ..
        } => Some(*dst),
        _ => None,
    }
}

/// Pure decision: given the currently-open session's dst (if any) and the
/// next op's classification, what does the replay loop do? No GPU state,
/// fully unit-testable. Self-sample does not apply to fill/logic (they
/// never read dst), so there is no read-dst arm this phase.
pub(super) fn session_step(open_dst: Option<DrawableId>, class: &CoalesceClass) -> SessionStep {
    match (open_dst, session_eligible(class)) {
        // Ineligible op.
        (Some(_), None) => SessionStep::FlushThenStandalone,
        (None, None) => SessionStep::Standalone,
        // Eligible op.
        (None, Some(_)) => SessionStep::OpenNew,
        (Some(open), Some(dst)) if open == dst => SessionStep::Continue,
        (Some(_), Some(_)) => SessionStep::FlushThenOpenNew,
    }
}

fn record_frame_coalescing_stats(ops: &[crate::kms::render::frame_builder::RecordedOp]) {
    let c = coalescing_counts(ops.iter().map(classify_recorded_op));
    if c.pass_ops > 0 {
        use std::sync::atomic::Ordering::Relaxed;
        let s = &crate::kms::vk::call_stats::VK_CALLS;
        s.fb_pass_ops.fetch_add(c.pass_ops, Relaxed);
        s.fb_pass_coalescable.fetch_add(c.coalescable, Relaxed);
        s.fb_self_sample.fetch_add(c.self_sample, Relaxed);
        s.fb_pass_mergeable.fetch_add(c.mergeable, Relaxed);
        s.fb_coalescable_dirty_clear
            .fetch_add(c.coalescable_dirty_clear, Relaxed);
        s.fb_coalescable_cross_kind
            .fetch_add(c.coalescable_cross_kind, Relaxed);
    }
}

impl RenderEngineInner {
    /// Phase B.2 Mechanism 2: acquire a descriptor set tagged with
    /// the right generation watermark. When a frame is open, every
    /// acquire shares the frame's captured `frame_generation`; the
    /// SubmittedOp pushed at close carries the same value, so the
    /// retire walk's `release_up_to(op.generation)` retires exactly
    /// the frame's pools. When no frame is open (legacy per-op
    /// fallback path), bump `acquire_generation` and use the new
    /// value — same shape as the pre-B.2 code.
    ///
    /// **Load-bearing safety invariant** (codex round 3 finding 3):
    /// `DescriptorPoolRing::acquire_set(layout, generation)` only
    /// allocates from pools whose state is `Active` (currently
    /// growing — never seen `vkResetDescriptorPool`) OR was just
    /// transitioned `Free → Active` via `ensure_active_with_capacity`
    /// after the ring's `release_up_to` reset it. The ring's
    /// `release_up_to(retired_watermark)` only resets pools whose
    /// `high_water_generation <= retired_watermark` (via
    /// `vkResetDescriptorPool`), and Vulkan
    /// VUID-vkResetDescriptorPool-descriptorPool-00313 mandates that
    /// all CBs referencing the pool's sets must have completed
    /// execution before reset. Therefore:
    ///
    /// - **Active pool case:** allocating from a still-growing pool
    ///   produces a handle to backing storage that has NEVER been
    ///   written to by `vkAllocateDescriptorSets` before; no prior
    ///   CB can possibly reference it.
    /// - **Just-reset pool case:** the reset guarantees no in-flight
    ///   CB depends on any of the pool's prior sets; the new
    ///   `vkAllocateDescriptorSets` call produces fresh handles
    ///   whose backing storage is also CB-independent.
    ///
    /// Either way, the descriptor set returned by `acquire_set` has
    /// zero in-flight CB dependencies. `vkUpdateDescriptorSets`
    /// against it at op-append time is safe per Vulkan host-mutation
    /// rules (VUID-vkUpdateDescriptorSets-pDescriptorWrites-06493):
    /// the targeted set must not be used by any pending command
    /// buffer.
    ///
    /// **This invariant is load-bearing for B.2.** If a future
    /// refactor changes the ring to recycle descriptor sets without
    /// going through reset (e.g. a hypothetical "fast-reuse" path),
    /// `vkUpdateDescriptorSets`-at-append would become unsafe. The
    /// audit at
    /// `crates/yserver/src/kms/render/descriptor_pool_ring.rs` (Task 3
    /// audit gate) confirms the current ring matches this invariant.
    ///
    /// # Errors
    ///
    /// Propagates `vkAllocateDescriptorSets` / `vkResetDescriptorPool`
    /// errors verbatim. Callers convert to `RenderError::Vk`.
    #[allow(
        dead_code,
        reason = "B.2 Task 3: helper lands now for B.3+ render-composite porting. \
                  Task 11 in B.2 routes render_composite through \
                  RenderPipeline::allocate_descriptor_for_views_into_ring, \
                  not this helper. The frame-open branch becomes hot in B.3."
    )]
    pub(crate) fn acquire_descriptor_set_for_frame_or_op(
        &mut self,
        layout: vk::DescriptorSetLayout,
    ) -> Result<vk::DescriptorSet, vk::Result> {
        let generation = if let Some(open) = self.frame_builder.open.as_ref() {
            open.frame_generation
        } else {
            self.acquire_generation = self.acquire_generation.saturating_add(1);
            self.acquire_generation
        };
        self.descriptor_pool_ring.acquire_set(layout, generation)
    }
}

impl RenderEngine {
    fn frame_builder_trace_filter() -> Option<FrameBuilderTraceFilter> {
        let raw = std::env::var("YSERVER_FB_TRACE_DRAWABLE_ID").ok()?;
        let s = raw.trim();
        if s.eq_ignore_ascii_case("redirected-argb-backings") {
            return Some(FrameBuilderTraceFilter::RedirectedArgbBackings);
        }
        if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
            return u64::from_str_radix(hex, 16)
                .ok()
                .map(FrameBuilderTraceFilter::DrawableId);
        }
        s.parse::<u64>()
            .ok()
            .map(FrameBuilderTraceFilter::DrawableId)
    }

    fn frame_builder_trace_matches_dst(
        store: &DrawableStore,
        filter: FrameBuilderTraceFilter,
        dst_id: DrawableId,
    ) -> bool {
        match filter {
            FrameBuilderTraceFilter::DrawableId(raw) => dst_id.as_u64() == raw,
            FrameBuilderTraceFilter::RedirectedArgbBackings => store
                .get(dst_id)
                .is_some_and(|d| d.depth == 32 && store.is_active_redirect_target(dst_id)),
        }
    }

    fn trace_frame_ops(
        store: &DrawableStore,
        frame_seq: u64,
        ops: &[crate::kms::render::frame_builder::RecordedOp],
        filter: FrameBuilderTraceFilter,
    ) {
        use crate::kms::render::frame_builder::{RecordedOp, RecordedTrapSrcKind};

        let hits = ops
            .iter()
            .filter(|op| {
                op.dst_id().is_some_and(|dst_id| {
                    Self::frame_builder_trace_matches_dst(store, filter, dst_id)
                })
            })
            .count();
        if hits == 0 {
            return;
        }
        log::warn!(
            target: "yserver::kms::render::fbtrace",
            "fbtrace frame_seq={} filter={:?} matched_ops={} total_ops={}",
            frame_seq,
            filter,
            hits,
            ops.len(),
        );
        for (idx, op) in ops.iter().enumerate() {
            match op {
                RecordedOp::RenderComposite(rc)
                    if Self::frame_builder_trace_matches_dst(store, filter, rc.dst_id) =>
                {
                    log::warn!(
                        target: "yserver::kms::render::fbtrace",
                        "fbtrace frame_seq={} op#{} RenderComposite dst={} op={} dst_has_alpha={} mask_ca={} rects={} clips={} src_clear={} mask_clear={} old_layout={:?}",
                        frame_seq, idx, rc.dst_id.as_u64(), rc.op, rc.dst_has_alpha,
                        rc.mask_component_alpha, rc.rects.len(),
                        rc.clip_rects.as_deref().map_or(0, |r| r.len()),
                        rc.src_clear_color.is_some(), rc.mask_clear_color.is_some(),
                        rc.dst_old_layout,
                    )
                }
                RecordedOp::CopyArea(ca)
                    if Self::frame_builder_trace_matches_dst(store, filter, ca.dst_id) =>
                {
                    log::warn!(
                        target: "yserver::kms::render::fbtrace",
                        "fbtrace frame_seq={} op#{} CopyArea dst={} src={} src_off=({}, {}) dst_off=({}, {}) extent={}x{} self_overlap={} old_layouts=({:?}->{:?})",
                        frame_seq, idx, ca.dst_id.as_u64(), ca.src_id.as_u64(),
                        ca.src_rect.offset.x, ca.src_rect.offset.y, ca.dst_rect.offset.x,
                        ca.dst_rect.offset.y, ca.dst_rect.extent.width, ca.dst_rect.extent.height,
                        ca.self_overlap_scratch.is_some(), ca.src_old_layout, ca.dst_old_layout,
                    )
                }
                RecordedOp::PutImage(pi)
                    if Self::frame_builder_trace_matches_dst(store, filter, pi.dst_id) =>
                {
                    log::warn!(
                        target: "yserver::kms::render::fbtrace",
                        "fbtrace frame_seq={} op#{} PutImage dst={} off=({}, {}) extent={}x{} old_layout={:?}",
                        frame_seq, idx, pi.dst_id.as_u64(), pi.dst_rect.offset.x,
                        pi.dst_rect.offset.y, pi.dst_rect.extent.width, pi.dst_rect.extent.height,
                        pi.dst_old_layout,
                    )
                }
                RecordedOp::FillRect(fr)
                    if Self::frame_builder_trace_matches_dst(store, filter, fr.dst_id) =>
                {
                    log::warn!(
                        target: "yserver::kms::render::fbtrace",
                        "fbtrace frame_seq={} op#{} FillRect dst={} rects={} color={:?} old_layout={:?}",
                        frame_seq, idx, fr.dst_id.as_u64(), fr.rects.len(), fr.color, fr.dst_old_layout,
                    )
                }
                RecordedOp::LogicFill(lf)
                    if Self::frame_builder_trace_matches_dst(store, filter, lf.dst_id) =>
                {
                    log::warn!(
                        target: "yserver::kms::render::fbtrace",
                        "fbtrace frame_seq={} op#{} LogicFill dst={} mode={:?} channels={:?} rects={} color={:?} old_layout={:?}",
                        frame_seq, idx, lf.dst_id.as_u64(), lf.logic_mode, lf.channels,
                        lf.rects.len(), lf.color, lf.dst_old_layout,
                    )
                }
                RecordedOp::ImageText(it)
                    if Self::frame_builder_trace_matches_dst(store, filter, it.dst_id) =>
                {
                    log::warn!(
                        target: "yserver::kms::render::fbtrace",
                        "fbtrace frame_seq={} op#{} ImageText dst={} instances={} fg={:?} old_layout={:?}",
                        frame_seq, idx, it.dst_id.as_u64(), it.instance_count, it.foreground_rgba,
                        it.dst_old_layout,
                    )
                }
                RecordedOp::CompositeGlyphs(cg)
                    if Self::frame_builder_trace_matches_dst(store, filter, cg.dst_id) =>
                {
                    log::warn!(
                        target: "yserver::kms::render::fbtrace",
                        "fbtrace frame_seq={} op#{} CompositeGlyphs dst={} instances={} clips={} fg={:?} old_layout={:?}",
                        frame_seq, idx, cg.dst_id.as_u64(), cg.instance_count, cg.clip_scissors.len(),
                        cg.foreground_rgba, cg.dst_old_layout,
                    )
                }
                RecordedOp::RenderTrapsOrTris(rt)
                    if Self::frame_builder_trace_matches_dst(store, filter, rt.dst_id) =>
                {
                    let src_kind = match &rt.src_kind {
                        RecordedTrapSrcKind::Drawable { .. } => "drawable",
                        RecordedTrapSrcKind::Solid(_) => "solid",
                        RecordedTrapSrcKind::Gradient { .. } => "gradient",
                    };
                    log::warn!(
                        target: "yserver::kms::render::fbtrace",
                        "fbtrace frame_seq={} op#{} RenderTrapsOrTris dst={} op_byte={} dst_has_alpha={} src_kind={} clips={} bbox=({},{} {}x{}) old_layout={:?}",
                        frame_seq, idx, rt.dst_id.as_u64(), rt.op_byte, rt.dst_has_alpha,
                        src_kind, rt.clip_scissors.len(), rt.bbox_x, rt.bbox_y,
                        rt.bbox_w, rt.bbox_h, rt.dst_old_layout,
                    );
                }
                _ => {}
            }
        }
    }

    /// Phase A: flush the platform's SubmitGroup and commit/drop the
    /// engine's parked per-op state atomically. THIS is the API every
    /// flush-trigger site calls (scene compose, get_image, PRESENT
    /// signal, pageflip retire, shutdown, MaxSize auto-flush) —
    /// NEVER call `platform.flush_submit_group` directly from outside
    /// the engine.
    pub(crate) fn flush_submit_group(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        reason: crate::kms::render::submit_group::FlushReason,
    ) -> Result<crate::kms::render::platform::FlushOutcome, vk::Result> {
        // GLX-TFP (Task 2.3): drain the exported drawables written since
        // the last flush; the platform waits on / publishes their dma-buf
        // implicit-sync fences around this submit. `Arc<OwnedFd>` clones
        // keep the fds alive across the call without re-borrowing `store`.
        //
        // Only drain when a real `vkQueueSubmit2` will occur (group
        // non-empty). The platform short-circuits an empty group without
        // submitting (an open render-batch/COW may be mid-recording); if
        // we drained here on that no-submit path we'd silently drop the
        // recorded exported writes and the eventual real submit would skip
        // their wait/publish. Leaving them queued lets the next non-empty
        // flush pick them up.
        let exported = if platform.submit_group_size() > 0 {
            store.take_exported_writes()
        } else {
            Vec::new()
        };
        let exported_borrows: Vec<(std::os::fd::BorrowedFd<'_>, bool)> = {
            use std::os::fd::AsFd as _;
            exported
                .iter()
                .map(|(f, prewaited)| (f.as_fd(), *prewaited))
                .collect()
        };
        let result = platform.flush_submit_group_with_exports(reason, &exported_borrows);
        // Drain the platform's last_flush_outcome regardless of Ok/Err
        // — both branches in platform's flush_submit_group populate it
        // before returning. The engine queues it for backend telemetry
        // drain (Task 3.5 wires that side).
        if let Some(outcome) = platform.take_last_flush_outcome()
            && let Some(inner) = self.inner.as_mut()
        {
            inner.pending_flush_outcomes.push(outcome);
        }
        let Some(inner) = self.inner.as_mut() else {
            return result;
        };
        match result {
            Ok(outcome) => {
                // Commit: parked ops graduate to `submitted`.
                for op in inner.pending_group_ops.drain(..) {
                    inner.submitted.push_back(op);
                }
                Ok(outcome)
            }
            Err(e) => {
                // Rollback. CBs were already freed by platform's Err
                // branch. Engine just clears the parked SubmittedOps so
                // their staging / scratch / atlas_ticket / shared-fence-
                // Arc clones drop together.
                inner.pending_group_ops.clear();
                Err(e)
            }
        }
    }

    /// Phase A: check whether the platform's SubmitGroup has hit its
    /// cap; if so, drive a `MaxSize` flush.
    pub(crate) fn maybe_auto_flush_submit_group(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
    ) -> Result<(), RenderError> {
        if platform.submit_group_size() >= platform.submit_group_max_size() {
            self.flush_submit_group(
                store,
                platform,
                crate::kms::render::submit_group::FlushReason::MaxSize,
            )
            .map_err(RenderError::Vk)?;
        }
        Ok(())
    }

    /// Phase B.1 Task 21: drain queued `FrameCloseEvent`s for telemetry.
    /// Returns empty when no events queued (or when engine is stubbed).
    pub(crate) fn drain_frame_close_events(
        &mut self,
    ) -> Vec<crate::kms::render::frame_builder::FrameCloseEvent> {
        self.inner
            .as_mut()
            .map(|i| std::mem::take(&mut i.pending_frame_close_events))
            .unwrap_or_default()
    }

    /// Phase B.1 Task 12: close the open frame (if any) for `reason`,
    /// replay its op list into ONE primary CB, submit through the
    /// `SubmitGroup` (cap=1 → one vkQueueSubmit2), and ONLY THEN park
    /// the pin set onto `pending_frames` + commit overlays. On any
    /// failure before submit-success, the local `OpenFrame` drops
    /// (pins evaporate, overlays evaporate); rollback writes
    /// `pre_frame_layout` values back to storage where the recorder
    /// already mutated them.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn close_open_frame(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        reason: crate::kms::render::frame_builder::CloseReason,
    ) -> Result<crate::kms::render::frame_builder::CloseOutcome, RenderError> {
        // Take the open frame from the FrameBuilder.
        let (mut open_frame, frame_seq) = {
            let Some(inner) = self.inner.as_mut() else {
                return Ok(crate::kms::render::frame_builder::CloseOutcome::AlreadyClosed);
            };
            let Some(open_frame_box) = inner.frame_builder.take_open_for_close(reason) else {
                return Ok(crate::kms::render::frame_builder::CloseOutcome::AlreadyClosed);
            };
            inner.frame_seq = inner.frame_seq.wrapping_add(1);
            (*open_frame_box, inner.frame_seq)
        };
        let frame_ticket = open_frame.ticket.clone();
        if let Some(filter) = Self::frame_builder_trace_filter() {
            Self::trace_frame_ops(store, frame_seq, &open_frame.ops, filter);
        }
        // Coalescing telemetry: this replay emits one render pass +
        // barrier pair per pass-op. Measure the same-dst headroom on
        // the live frame-builder path (the rpflush_* counters cover the
        // unused PendingRenderBatch path). Cheap O(ops) walk per close.
        record_frame_coalescing_stats(&open_frame.ops);

        // Phase B.2 Task 14: count RenderComposite ops once for the
        // telemetry close-event. `open_frame.ops` is not mutated
        // between this point and any of the 5 close-event push sites
        // below (success + 4 error paths: begin_op_cb err, record err,
        // end_and_submit err, flush_submit_group err), so a single
        // tally is safe and avoids re-walking the op list on each path.
        let renders_in_frame: u32 = u32::try_from(
            open_frame
                .ops
                .iter()
                .filter(|op| {
                    matches!(
                        op,
                        crate::kms::render::frame_builder::RecordedOp::RenderComposite(_)
                    )
                })
                .count(),
        )
        .unwrap_or(u32::MAX);

        // Allocate the primary CB.
        let cb = {
            let inner = self.inner.as_mut().expect("inner");
            match begin_op_cb(inner, platform) {
                Ok((cb, op_ticket)) => {
                    // Phase B.1 invariant: the begin_op_cb ticket MUST be the
                    // same fence the frame opened against. If they diverge,
                    // the SubmitGroup was flushed mid-frame (no current
                    // code path does that, but a future regression would
                    // park pins on the wrong fence).
                    debug_assert_eq!(
                        op_ticket.fence(),
                        frame_ticket.fence(),
                        "begin_op_cb returned a different fence than the open frame's — \
                         SubmitGroup was flushed mid-frame?"
                    );
                    cb
                }
                Err(e) => {
                    rollback_pre_submit(store, &mut open_frame);
                    let inner_post = self.inner.as_mut().expect("inner");
                    rollback_atlas(
                        inner_post,
                        open_frame.layouts.atlas,
                        open_frame.atlas_prev_ticket_snapshot.clone(),
                    );
                    rollback_snapshots(inner_post, &mut open_frame.snapshot_touch);
                    rescue_present_completions(inner_post, &mut open_frame);
                    // Phase B.2 Mechanism 3 (defensive): release any
                    // retired BatchResources attached to the open
                    // frame's pin set. Structurally empty under B.2's
                    // grow-before-open rule, but BatchResource has no
                    // Drop (paint_batch.rs:147); preserved for B.3+.
                    for r in open_frame.pins.retired_resources.drain(..) {
                        r.release(&inner_post.vk);
                    }
                    if inner_post.pending_frame_close_events.len() < 1024 {
                        inner_post.pending_frame_close_events.push(
                            crate::kms::render::frame_builder::FrameCloseEvent {
                                reason,
                                ops_in_frame: open_frame.ops.len(),
                                glyph_uploads_in_frame: open_frame.glyph_uploads_in_frame,
                                renders_in_frame,
                                pin_count: open_frame.pins.len(),
                                aborted: true,
                            },
                        );
                    }
                    inner_post.frame_builder.complete_close_failure();
                    return Err(e);
                }
            }
        };

        // Pass 1 (resource) — no-op in B.1.
        // Pass 2 (record) — record each op into cb.
        //
        // Slice-2 phase 3: hold ONE begin_rendering open across consecutive
        // same-dst FILL / LOGIC_FILL / FOLD-CLEAN COMPOSITE ops (the session).
        // DIRTY composites (solid clear / dst-readback / self-sample), glyph,
        // image_text, and traps run through the unchanged standalone
        // `emit_recorded_op_into_cb` after
        // the session is flushed. The session emits exactly one pre-barrier
        // (the opening op's `dst_old_layout` → COLOR) and one post-barrier
        // (→ SHADER_READ) at close; continued ops emit draws only.
        let record_result: Result<(), RenderError> = {
            let inner = self.inner.as_mut().expect("inner");
            let mut acc: Result<(), RenderError> = Ok(());
            let frame_generation = open_frame.frame_generation;
            // Cloned Vk handle for the session's open/close helpers, so they
            // don't alias `&inner.vk` against `&mut inner` (Phase-1 pattern).
            let vk = inner.vk.clone();
            let mut session: Option<DstPassSession> = None;
            // #214: gradient initial uploads go first — nothing earlier in
            // the frame can sample a picture created while it was open.
            for gi in &open_frame.gradient_inits {
                let src = open_frame.pins.upload_slices[gi.upload_pin.0 as usize];
                gi.picture
                    .record_initial_upload(&vk.device, cb, src.buffer, src.offset);
            }
            for op in &open_frame.ops {
                let class = classify_recorded_op(op);
                let step = session_step(session.as_ref().map(|s| s.dst_id), &class);
                // Flush the open session first if the step demands it.
                let must_flush = matches!(
                    step,
                    SessionStep::FlushThenStandalone | SessionStep::FlushThenOpenNew
                );
                if let Some(s) = session.take_if(|_| must_flush) {
                    close_dst_color_pass(&vk, cb, s.dst_image);
                }
                let step_result: Result<(), RenderError> = match step {
                    SessionStep::Standalone | SessionStep::FlushThenStandalone => {
                        emit_recorded_op_into_cb(
                            inner,
                            store,
                            cb,
                            &open_frame.pins,
                            frame_generation,
                            op,
                        )
                    }
                    SessionStep::OpenNew | SessionStep::FlushThenOpenNew => {
                        emit_session_open_and_draws(inner, store, &vk, cb, op, &mut session)
                    }
                    SessionStep::Continue => emit_session_continue_draws(inner, cb, op),
                };
                if let Err(e) = step_result {
                    acc = Err(e);
                    break;
                }
            }
            // End-of-frame flush rule 4: close any still-open session.
            if let Some(s) = session.take_if(|_| acc.is_ok()) {
                close_dst_color_pass(&vk, cb, s.dst_image);
            }
            acc
        };

        let record_result = if platform.take_forced_frame_record_failure() {
            Err(RenderError::RendererFailed)
        } else {
            record_result
        };
        if let Err(e) = record_result {
            // CB never appended to SubmitGroup. Free it ourselves.
            {
                let inner = self.inner.as_mut().expect("inner");
                let device = &inner.vk.device;
                if let Some(pool) = platform.ops_command_pool_handle() {
                    // SAFETY: cb was allocated from `pool` and never
                    // submitted; safe to free in Recording state.
                    unsafe { device.free_command_buffers(pool, &[cb]) };
                }
            }
            rollback_pre_submit(store, &mut open_frame);
            platform.renderer_failed = true;
            let inner_post = self.inner.as_mut().expect("inner");
            rollback_atlas(
                inner_post,
                open_frame.layouts.atlas,
                open_frame.atlas_prev_ticket_snapshot.clone(),
            );
            rollback_snapshots(inner_post, &mut open_frame.snapshot_touch);
            rescue_present_completions(inner_post, &mut open_frame);
            // Phase B.2 Mechanism 3 (defensive): release any retired
            // BatchResources attached to the open frame's pin set.
            // See path 1 above for rationale.
            for r in open_frame.pins.retired_resources.drain(..) {
                r.release(&inner_post.vk);
            }
            if inner_post.pending_frame_close_events.len() < 1024 {
                inner_post.pending_frame_close_events.push(
                    crate::kms::render::frame_builder::FrameCloseEvent {
                        reason,
                        ops_in_frame: open_frame.ops.len(),
                        glyph_uploads_in_frame: open_frame.glyph_uploads_in_frame,
                        renders_in_frame,
                        pin_count: open_frame.pins.len(),
                        aborted: true,
                    },
                );
            }
            inner_post.frame_builder.complete_close_failure();
            return Err(e);
        }

        // Phase B.3 (N10) — branch (a) PRE-SUBMIT: acquire PresentCompletionSignal
        // BEFORE end_and_submit_op_with_signal so the semaphore is queued on the
        // submit's signal list. Acquiring after submit means the semaphore is
        // never queued; the exported sync_file fd would never fire (Pitfall 8).
        let completion_signal: Option<PresentCompletionSignal> = {
            let pending_count = open_frame.pending_present_completions.len();
            if pending_count == 0 {
                None
            } else {
                match platform.acquire_present_completion_signal() {
                    Ok(s) => Some(s),
                    Err(e) => {
                        // The frame still submits; its completions fall
                        // back to polling the frame's fence (post-flush
                        // `None` arm) instead of a sync_file.
                        log::warn!(
                            "close_open_frame: Present completion semaphore allocation \
                             failed: {e:?}; falling back to FenceTicket polling"
                        );
                        None
                    }
                }
            }
        };
        let completion_semaphore = completion_signal
            .as_ref()
            .map(PresentCompletionSignal::semaphore);

        // End CB + append to SubmitGroup. Does NOT vkQueueSubmit2 yet.
        // Uses end_and_submit_op_with_signal so the completion semaphore
        // (if any) is queued on the submit's signal list.
        let append_result = {
            let inner = self.inner.as_mut().expect("inner");
            end_and_submit_op_with_signal(inner, platform, cb, &frame_ticket, completion_semaphore)
        };
        if let Err(e) = append_result {
            {
                let inner = self.inner.as_mut().expect("inner");
                let device = &inner.vk.device;
                if let Some(pool) = platform.ops_command_pool_handle() {
                    unsafe { device.free_command_buffers(pool, &[cb]) };
                }
            }
            rollback_pre_submit(store, &mut open_frame);
            platform.renderer_failed = true;
            let inner_post = self.inner.as_mut().expect("inner");
            rollback_atlas(
                inner_post,
                open_frame.layouts.atlas,
                open_frame.atlas_prev_ticket_snapshot.clone(),
            );
            rollback_snapshots(inner_post, &mut open_frame.snapshot_touch);
            rescue_present_completions(inner_post, &mut open_frame);
            // Phase B.2 Mechanism 3 (defensive): release any retired
            // BatchResources attached to the open frame's pin set.
            // See path 1 above for rationale.
            for r in open_frame.pins.retired_resources.drain(..) {
                r.release(&inner_post.vk);
            }
            if inner_post.pending_frame_close_events.len() < 1024 {
                inner_post.pending_frame_close_events.push(
                    crate::kms::render::frame_builder::FrameCloseEvent {
                        reason,
                        ops_in_frame: open_frame.ops.len(),
                        glyph_uploads_in_frame: open_frame.glyph_uploads_in_frame,
                        renders_in_frame,
                        pin_count: open_frame.pins.len(),
                        aborted: true,
                    },
                );
            }
            inner_post.frame_builder.complete_close_failure();
            // completion_signal drops with the local — submit never
            // queued the signal-op so the fd would never fire.
            return Err(e);
        }

        // Phase B.3 (N8): collect every self-overlap scratch from the recorded
        // ops into a local Vec — the SubmittedOp will own them through fence
        // retire. std::mem::take leaves the ops in place with `None` for the
        // scratch slot (idempotent if the op never carried one). Done BEFORE
        // flush_submit_group so close-failure drops the local on the stack
        // (ScratchImage::Drop destroys Vk handles cleanly — no fence ticket
        // exists yet at this point).
        let frame_scratches: Vec<ScratchImage> = open_frame
            .ops
            .iter_mut()
            .filter_map(|op| match op {
                crate::kms::render::frame_builder::RecordedOp::CopyArea(ca) => {
                    ca.self_overlap_scratch.take()
                }
                _ => None,
            })
            .collect();
        // Phase B.3 clip: same single-source-of-truth take for the masked
        // copy_area's SampledScratchImage (codex round-4 finding 4).
        let frame_sampled_scratches: Vec<SampledScratchImage> = open_frame
            .ops
            .iter_mut()
            .filter_map(|op| match op {
                crate::kms::render::frame_builder::RecordedOp::MaskedCopyArea(m) => {
                    m.self_overlap_scratch.take()
                }
                _ => None,
            })
            .collect();

        // Park a SubmittedOp into pending_group_ops.
        //
        // Phase B.2 Mechanism 2: consume the frame's captured-at-open
        // `frame_generation` instead of bumping at close. Every
        // descriptor acquisition that ran during the open frame
        // tagged the descriptor pool with this same value, so the
        // retire walk's `release_up_to(op.generation)` retires
        // exactly the frame's pools (and no others).
        {
            let inner = self.inner.as_mut().expect("inner");
            let generation = open_frame.frame_generation;
            inner.pending_group_ops.push(SubmittedOp {
                cb,
                ticket: frame_ticket.clone(),
                staging: None,
                scratch: frame_scratches,                 // NEW (B.3 N8)
                sampled_scratch: frame_sampled_scratches, // NEW (B.3 clip)
                atlas_ticket: None,
                generation,
                retired_resources: Vec::new(),
            });
        }

        // Drive the actual vkQueueSubmit2 via engine's flush_submit_group wrapper.
        platform.set_next_submit_cause(reason.submit_cause());
        let flush_outcome = self.flush_submit_group(
            store,
            platform,
            crate::kms::render::submit_group::FlushReason::FrameBuilder,
        );

        match flush_outcome {
            Ok(_) => {
                // Commit-after-Ok.
                let op_count = open_frame.ops.len();
                let glyph_uploads = open_frame.glyph_uploads_in_frame;
                let pin_count = open_frame.pins.len();
                {
                    let inner = self.inner.as_mut().expect("inner");
                    // Phase B.3 (N10) branch (b) POST-FLUSH SUCCESS: drain
                    // pending_present_completions into a PendingPresentBatch
                    // alongside the exported sync_file fd from the signal
                    // we queued on the submit. The batch keeps the signal
                    // alive until the fd fires.
                    let mut drained_completions: Vec<
                        crate::kms::render::present_completion::PendingPresentEntry,
                    > = std::mem::take(&mut open_frame.pending_present_completions);
                    if !drained_completions.is_empty() {
                        let (wait, signal) = match completion_signal {
                            Some(signal) => match signal.export_sync_file_fd() {
                                Ok(Some(fd)) => (PresentBatchWait::Fd(fd), Some(signal)),
                                Ok(None) => (PresentBatchWait::Ready, Some(signal)),
                                Err(e) => {
                                    log::warn!(
                                        "B.3 close_open_frame: vkGetSemaphoreFdKHR(SYNC_FD) \
                                         failed: {e:?}; falling back to FenceTicket polling"
                                    );
                                    (PresentBatchWait::Poll, Some(signal))
                                }
                            },
                            // No signal: its allocation failed. Poll the
                            // frame's fence (the batch ticket below).
                            None => (PresentBatchWait::Poll, None),
                        };
                        if let PresentBatchWait::Fd(fd) = &wait {
                            for completion in &mut drained_completions {
                                if let Err(e) = completion.publish_release_fence(fd) {
                                    log::warn!(
                                        "B.3 close_open_frame: publish Present release fence \
                                         failed: {e}; falling back to host signal"
                                    );
                                }
                            }
                        }
                        let ticket = inner.submitted.back().map(|op| op.ticket.clone());
                        inner.pending_present_batches.push(PendingPresentBatch {
                            wait,
                            ticket,
                            signal,
                            events: drained_completions,
                        });
                    }
                    // B.3 hotfix 2: adopt gradient Arc clones from every
                    // RenderTrapsOrTris op into pins.retired_resources
                    // BEFORE taking the pins. This keeps the GradientPicture
                    // Arc alive in FrameSubmittedRecord until the GPU fence
                    // fires — otherwise the recorded-op clone would drop with
                    // open_frame at the end of this function while the GPU CB
                    // is still in flight.
                    for op in &open_frame.ops {
                        if let crate::kms::render::frame_builder::RecordedOp::RenderTrapsOrTris(rt) = op
                            && let crate::kms::render::frame_builder::RecordedTrapSrcKind::Gradient {
                                ref picture,
                                ..
                            } = rt.src_kind
                        {
                            open_frame.pins.adopt_retired(Box::new(picture.clone())
                                as Box<dyn crate::kms::render::batch_resource::BatchResource>);
                        }
                    }
                    inner.pending_frames.push_back(
                        crate::kms::render::frame_builder::FrameSubmittedRecord {
                            ticket: frame_ticket.clone(),
                            pins: std::mem::take(&mut open_frame.pins),
                            frame_seq,
                        },
                    );
                    commit_close_success(
                        inner,
                        store,
                        std::mem::take(&mut open_frame.layouts),
                        std::mem::take(&mut open_frame.touched),
                        std::mem::take(&mut open_frame.pending_glyph_inserts),
                        &frame_ticket,
                    );
                    if inner.pending_frame_close_events.len() < 1024 {
                        inner.pending_frame_close_events.push(
                            crate::kms::render::frame_builder::FrameCloseEvent {
                                reason,
                                ops_in_frame: op_count,
                                glyph_uploads_in_frame: glyph_uploads,
                                renders_in_frame,
                                pin_count,
                                aborted: false,
                            },
                        );
                    }
                    inner.frame_builder.complete_close_success();
                }
                Ok(crate::kms::render::frame_builder::CloseOutcome::Submitted {
                    frame_seq,
                    op_count,
                    pin_count,
                    ticket: frame_ticket,
                    reason,
                })
            }
            Err(e) => {
                // Platform's abort_flush already freed (or, after a device loss,
                // leaked) the CBs + set renderer_failed.
                rollback_pre_submit(store, &mut open_frame);
                let atlas_overlay = open_frame.layouts.atlas;
                let atlas_prev = open_frame.atlas_prev_ticket_snapshot.clone();
                let ops_in_frame = open_frame.ops.len();
                let glyph_uploads_in_frame = open_frame.glyph_uploads_in_frame;
                let pin_count = open_frame.pins.len();
                let inner = self.inner.as_mut().expect("inner");
                // Phase B.3 (N10) branch (c) POST-FLUSH FAILURE: force-enqueue
                // a degraded PendingPresentBatch BEFORE returning Err.
                // Never silent-drop — X PRESENT protocol observes events regardless
                // of submit success (Pitfall 8). The completion_signal drops with
                // the local; the failed submit never queued a signal-op so the fd
                // would never fire anyway.
                let drained_completions: Vec<
                    crate::kms::render::present_completion::PendingPresentEntry,
                > = std::mem::take(&mut open_frame.pending_present_completions);
                if !drained_completions.is_empty() {
                    inner.pending_present_batches.push(PendingPresentBatch {
                        wait: PresentBatchWait::Ready,
                        ticket: None,
                        signal: None,
                        events: drained_completions,
                    });
                }
                rollback_atlas(inner, atlas_overlay, atlas_prev);
                rollback_snapshots(inner, &mut open_frame.snapshot_touch);
                // Phase B.2 Mechanism 3 (defensive): release any
                // retired BatchResources attached to the open frame's
                // pin set. See path 1 above for rationale.
                for r in open_frame.pins.retired_resources.drain(..) {
                    r.release(&inner.vk);
                }
                if inner.pending_frame_close_events.len() < 1024 {
                    inner.pending_frame_close_events.push(
                        crate::kms::render::frame_builder::FrameCloseEvent {
                            reason,
                            ops_in_frame,
                            glyph_uploads_in_frame,
                            renders_in_frame,
                            pin_count,
                            aborted: true,
                        },
                    );
                }
                inner.frame_builder.complete_close_failure();
                Err(RenderError::Vk(e))
            }
        }
    }

    /// Phase B Invariant M2: close the open frame (if any) BEFORE a
    /// non-ported paint op records its own CB. The non-ported op
    /// samples committed `Drawable::storage.current_layout` and
    /// `last_render_ticket`; without the close, it would race against
    /// the deferred frame on the GPU. Retires when every paint op is
    /// ported (end of sub-phase B.3 at the latest).
    ///
    /// Fast path: no frame open → no-op. Preserves existing
    /// batch-coalescing discipline in `render_composite`,
    /// `cow_copy_area`, etc.
    ///
    /// Slow path: frame open → flush pre-existing batches first
    /// (chronological ordering: pre-frame batches must submit before
    /// the frame's CB), then close the frame. Each non-ported op's
    /// own batch prelude runs afterward against an empty batch state.
    pub(crate) fn close_open_frame_for_non_ported_op(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
    ) -> Result<(), RenderError> {
        let frame_open = self
            .inner
            .as_ref()
            .is_some_and(|i| i.frame_builder.is_open());
        if !frame_open {
            return Ok(());
        }
        self.flush_render_batch(store, platform, RenderFlushReason::Other)?;
        match self.close_open_frame(
            store,
            platform,
            crate::kms::render::frame_builder::CloseReason::NonPortedPaintOp,
        )? {
            crate::kms::render::frame_builder::CloseOutcome::Submitted { .. }
            | crate::kms::render::frame_builder::CloseOutcome::AlreadyClosed => Ok(()),
        }
    }

    /// Phase B.1 close trigger 4: close the open frame if its open
    /// duration has exceeded the cached timeout. No-op if no frame
    /// open or below threshold. Called by `maybe_composite` at the
    /// top of every tick.
    pub(crate) fn close_open_frame_if_timed_out(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
    ) -> Result<(), RenderError> {
        let timed_out = self
            .inner
            .as_ref()
            .is_some_and(|i| i.frame_builder.open_for_at_least(i.frame_builder_timeout));
        if !timed_out {
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

    /// Poll deadline for the frame-builder timeout close. A caller must wake
    /// at this instant and call [`Self::close_open_frame_if_timed_out`].
    pub(crate) fn open_frame_timeout_deadline(&self) -> Option<std::time::Instant> {
        self.inner.as_ref().and_then(|inner| {
            inner
                .frame_builder
                .open_deadline(inner.frame_builder_timeout)
        })
    }

    /// Count of in-flight submits awaiting retirement. Tests use
    /// this to assert the lifecycle book-keeping.
    pub(crate) fn pending_count(&self) -> usize {
        self.inner.as_ref().map(|i| i.submitted.len()).unwrap_or(0)
    }

    /// Phase B.1 Task 15: test introspection — is the frame builder
    /// currently open?
    pub(crate) fn frame_builder_is_open(&self) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|i| i.frame_builder.is_open())
    }

    /// Phase B.1 Task 15: test introspection — lifetime count of
    /// `FrameBuilder` closes.
    pub(crate) fn frame_builder_lifetime_closes(&self) -> u64 {
        self.inner
            .as_ref()
            .map_or(0, |i| i.frame_builder.lifetime_closes())
    }

    /// Phase B.2 Task 11: test introspection — walk the open frame's
    /// recorded op list and return each
    /// `RecordedOp::RenderComposite`'s `dst_old_layout` in append
    /// order. Returns an empty vec if no frame is open. Used by the
    /// second-op-in-frame overlay test (see
    /// `KmsBackend::frame_builder_peek_render_composite_dst_old_layouts_for_tests`).
    /// The `RecordedRenderComposite` payload is `pub(crate)` so the
    /// integration crate cannot match on it directly — this returns
    /// the minimum scalar needed for the assertion.
    pub(crate) fn frame_builder_peek_render_composite_dst_old_layouts(
        &self,
    ) -> Vec<vk::ImageLayout> {
        let Some(inner) = self.inner.as_ref() else {
            return Vec::new();
        };
        let Some(open) = inner.frame_builder.open.as_ref() else {
            return Vec::new();
        };
        open.ops
            .iter()
            .filter_map(|op| match op {
                crate::kms::render::frame_builder::RecordedOp::RenderComposite(rc) => {
                    Some(rc.dst_old_layout)
                }
                _ => None,
            })
            .collect()
    }

    /// Phase B.1 Task 21: monotonic count of all `FrameBuilder` opens
    /// since init. Delta-tracked by `KmsBackend::drain_frame_builder_telemetry`
    /// to emit one `record_frame_builder_open` per new open.
    pub(crate) fn frame_builder_lifetime_opens(&self) -> u64 {
        self.inner
            .as_ref()
            .map_or(0, |i| i.frame_builder.lifetime_opens())
    }

    /// Phase B.1 Task 15: test introspection — monotonic `frame_seq`
    /// counter. Bumped by `close_open_frame` on every successful close.
    pub(crate) fn engine_frame_seq(&self) -> u64 {
        self.inner.as_ref().map_or(0, |i| i.frame_seq)
    }

    /// Stage 5 Task 4 layer 1: lifetime count of `vkCreateDescriptorPool`
    /// calls inside the ring. Backend polls this and bumps telemetry.
    pub(crate) fn descriptor_pool_creates_lifetime(&self) -> u64 {
        self.inner
            .as_ref()
            .map_or(0, |i| i.descriptor_pool_ring.lifetime_creates())
    }

    /// Stage 5 Task 4 layer 1: lifetime count of successful
    /// `vkResetDescriptorPool` calls inside the ring.
    pub(crate) fn descriptor_pool_resets_lifetime(&self) -> u64 {
        self.inner
            .as_ref()
            .map_or(0, |i| i.descriptor_pool_ring.lifetime_resets())
    }

    /// Stage 5 Task 4 layer 1: ring residency for the acceptance
    /// gate (`render_composite_pool_creates_bounded_after_warmup`).
    pub(crate) fn descriptor_pool_ring_pool_count(&self) -> usize {
        self.inner
            .as_ref()
            .map_or(0, |i| i.descriptor_pool_ring.pool_count())
    }

    /// Phase B.2 Task 3 test introspection: maximum `high_water_generation`
    /// across the descriptor pool ring's resident pools. The Mechanism 2
    /// integration test reads this to assert that every `acquire_set`
    /// during an open frame tags the active pool with the frame's
    /// captured `frame_generation`.
    pub(crate) fn descriptor_pool_ring_high_water_generation(&self) -> u64 {
        self.inner
            .as_ref()
            .map_or(0, |i| i.descriptor_pool_ring.max_high_water_generation())
    }

    /// Phase B.2 Task 3 test introspection: read the open frame's
    /// captured `frame_generation`. Returns `None` if no frame is
    /// open. Used by the Mechanism 2 watermark integration test to
    /// confirm the open-time bump landed.
    pub(crate) fn open_frame_generation(&self) -> Option<u64> {
        self.inner
            .as_ref()
            .and_then(|i| i.frame_builder.open.as_ref().map(|o| o.frame_generation))
    }

    /// Phase A Task 3.5: drain all queued `FlushOutcome` records
    /// accumulated since the last drain. Backend calls this once
    /// per `maybe_composite` tick to route outcomes to telemetry.
    pub(crate) fn drain_flush_outcomes(
        &mut self,
    ) -> Vec<crate::kms::render::platform::FlushOutcome> {
        self.inner
            .as_mut()
            .map(|i| std::mem::take(&mut i.pending_flush_outcomes))
            .unwrap_or_default()
    }

    /// Phase B.3 (N10): attach a PRESENT-completion entry to the open frame
    /// if and only if the frame has an op that WRITES to `dst` (the COW, or
    /// a non-COW Present's destination storage). Its completion signal then
    /// rides the frame's own submit. Returns `Err(entry)` if no open frame
    /// exists or the frame doesn't write to `dst` (predicate is
    /// `RecordedOp::dst_id() == Some(dst)`, NOT `touched` — touched includes
    /// sampled-only references that would attach completions to frames that
    /// never wrote it).
    // This Result is an ownership hand-back, not a conventional error path;
    // boxing the entry would add an allocation to every COW Present.
    #[allow(clippy::result_large_err)]
    pub(crate) fn attach_present_completion(
        &mut self,
        dst: DrawableId,
        entry: PendingPresentEntry,
    ) -> Result<(), PendingPresentEntry> {
        let Some(inner) = self.inner.as_mut() else {
            return Err(entry);
        };
        let Some(open) = inner.frame_builder.open.as_mut() else {
            return Err(entry);
        };
        // N10 predicate: writes, NOT just touched.
        let writes_to_dst = open.ops.iter().any(|op| op.dst_id() == Some(dst));
        if !writes_to_dst {
            return Err(entry);
        }
        open.pending_present_completions.push(entry);
        Ok(())
    }

    pub(crate) fn drain_present_batches(&mut self) -> Vec<PendingPresentBatch> {
        let Some(inner) = self.inner.as_mut() else {
            return Vec::new();
        };
        std::mem::take(&mut inner.pending_present_batches)
    }
}

// ────────────────────────────────────────────────────────────────
// Helpers: CB lifecycle, byte conversion, rect clipping.
// ────────────────────────────────────────────────────────────────

/// Allocate a fresh primary CB from the platform's
/// `OpsCommandPool`, begin recording, and acquire a
/// `FenceTicket` from the platform's fence pool. Returns
/// `(cb, ticket)` ready to record into.
pub(super) fn begin_op_cb(
    inner: &mut RenderEngineInner,
    platform: &mut PlatformBackend,
) -> Result<(vk::CommandBuffer, FenceTicket), RenderError> {
    let pool = platform
        .ops_command_pool_handle()
        .ok_or(RenderError::NoVk)?;
    let device = &inner.vk.device;
    let alloc_info = vk::CommandBufferAllocateInfo::default()
        .command_pool(pool)
        .level(vk::CommandBufferLevel::PRIMARY)
        .command_buffer_count(1);
    let cb = unsafe { device.allocate_command_buffers(&alloc_info)? }[0];
    let begin =
        vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
    if let Err(e) = unsafe { device.begin_command_buffer(cb, &begin) } {
        // SAFETY: cb was just allocated from `pool`, never submitted;
        // safe to free in Initial state.
        unsafe { device.free_command_buffers(pool, &[cb]) };
        return Err(e.into());
    }
    // Phase A: shared ticket comes from the open submit group. With
    // max_size = 1, the group auto-closes after one append; with
    // max_size > 1 (post-Task 4), N appends share the same ticket.
    let ticket = match platform.submit_group_ticket_or_open() {
        Ok(t) => t,
        Err(e) => {
            // SAFETY: cb was begun but never submitted; safe to
            // free in Recording state.
            unsafe { device.free_command_buffers(pool, &[cb]) };
            return Err(RenderError::Vk(e));
        }
    };
    Ok((cb, ticket))
}

/// End CB recording, submit on the graphics queue with the
/// ticket's fence, return `Ok` on accept. Same-queue submission
/// order with the I6a fence is what Stage 2 plan cross-cutting
/// §3 banks on for paint→compose ordering without
/// `vkQueueWaitIdle`.
pub(super) fn end_and_submit_op(
    inner: &mut RenderEngineInner,
    platform: &mut PlatformBackend,
    cb: vk::CommandBuffer,
    ticket: &FenceTicket,
) -> Result<(), RenderError> {
    end_and_submit_op_with_signal(inner, platform, cb, ticket, None)
}

fn end_and_submit_op_with_signal(
    inner: &mut RenderEngineInner,
    platform: &mut PlatformBackend,
    cb: vk::CommandBuffer,
    ticket: &FenceTicket,
    completion_signal: Option<vk::Semaphore>,
) -> Result<(), RenderError> {
    let device = &inner.vk.device;
    unsafe { device.end_command_buffer(cb)? };
    platform.submit_paint_cb_with_semaphore(cb, ticket.fence(), completion_signal)?;
    let _ = device;
    Ok(())
}

/// Phase B.1 Task 12 / Phase B.2 Task 4: commit recorder-side state
/// to engine + atlas + drawable store after `flush_submit_group`
/// returned Ok.
///
/// - **Drawable layout commit** (B.2 LOAD-BEARING, USER-codex U-R6.F1):
///   walk the `FrameLayoutTable::drawables` overlay and write each
///   entry's `current_in_frame_layout` back to
///   `Drawable::storage.current_layout`. The recorded ops' barriers
///   transitioned the GPU image to this layout; storage was
///   deliberately NOT mutated during recording so failed frames can
///   drop the overlay without rolling back. On success, storage MUST
///   catch up — otherwise subsequent ops (legacy or post-B.4 ported)
///   emit barriers from stale `old_layout` values, producing a
///   Vulkan validation hazard / corrupt sampled image / device loss.
///   Under B.1's recorder (which mutates storage in-place during
///   emit) the overlay is structurally empty for the porting paths,
///   so this loop is a no-op on B.1 frames — the harm shape only
///   shows up once a B.2 port (Task 11+) routes its layout updates
///   exclusively through the overlay.
/// - **Touched-drawable `last_render_ticket` commit:** no-op — the
///   recorder already called `store.touch_render_fence` at append.
/// - **Atlas layout commit:** the B.1 recorder mutates
///   `GlyphAtlas::current_layout` in place during composite_glyphs,
///   so the atlas overlay is structurally empty on B.1 frames.
///   Reserved as a no-op-friendly write for Task 11 (when ported ops
///   read the atlas layout via overlay too). Idempotent against the
///   B.1 path because B.1's recorder leaves the overlay entry's
///   `current_in_frame_layout` equal to the atlas's actual
///   post-frame layout in the rare case it touches both.
/// - **Glyph cache inserts:** drained here onto the atlas.
/// - **Atlas last_render_ticket:** stamped with the closed frame's
///   ticket.
fn commit_close_success(
    inner: &mut RenderEngineInner,
    store: &mut DrawableStore,
    layouts: crate::kms::render::frame_builder::FrameLayoutTable,
    touched: crate::kms::render::frame_builder::TouchedDrawables,
    pending: crate::kms::render::frame_builder::PendingGlyphInserts,
    frame_ticket: &FenceTicket,
) {
    let _ = touched;
    // Drawables: commit overlay → storage. Empty on B.1 frames; the
    // load-bearing write is reserved for B.2 Task 11+ ports that
    // route their layout updates exclusively through the overlay.
    for (id, entry) in layouts.drawables {
        if let Some(d) = store.get_mut(id) {
            d.storage.current_layout = entry.current_in_frame_layout;
        }
    }
    if let Some(atlas) = inner.glyph_atlas.as_mut() {
        // Atlas: commit overlay → atlas.current_layout. Structurally
        // empty under B.1's recorder (which mutates the atlas
        // layout in place during emit). Reserved for Task 11+ when
        // the ported path consults the overlay-resolved layout.
        if let Some(entry) = layouts.atlas {
            atlas.set_current_layout(entry.current_in_frame_layout);
        }
        for (key, entry) in pending.entries {
            atlas.insert_entry(key, entry);
        }
        atlas.set_last_render_ticket(frame_ticket.clone());
    }
}

/// Phase B.1 Task 12: rollback drawable-side state to pre-frame on
/// any close-time failure. Walks the layout overlay + touched-set
/// to undo any in-frame mutations the recorder already wrote into
/// the store. Atlas-side rollback is handled by `rollback_atlas`.
fn rollback_pre_submit(
    store: &mut DrawableStore,
    open_frame: &mut crate::kms::render::frame_builder::OpenFrame,
) {
    for (id, entry) in open_frame.layouts.drawables.drain() {
        if let Some(d) = store.get_mut(id) {
            d.storage.current_layout = entry.pre_frame_layout;
        }
    }
    for (id, prior) in open_frame.touched.snapshots.drain() {
        if let Some(d) = store.get_mut(id) {
            d.last_render_ticket = prior;
        }
    }
}

/// Phase B.1 Task 12: rollback atlas-side state to pre-frame on any
/// close-time failure. Restores the pre-frame layout (if the frame
/// touched the atlas) and the pre-frame `last_render_ticket`
/// snapshot (if the frame snapshotted it).
fn rollback_atlas(
    inner: &mut RenderEngineInner,
    layouts_atlas: Option<crate::kms::render::frame_builder::LayoutOverlayEntry>,
    atlas_prev_ticket_snapshot: Option<Option<FenceTicket>>,
) {
    if let Some(atlas) = inner.glyph_atlas.as_mut() {
        if let Some(entry) = layouts_atlas {
            atlas.set_current_layout(entry.pre_frame_layout);
        }
        if let Some(prior) = atlas_prev_ticket_snapshot {
            match prior {
                Some(t) => atlas.set_last_render_ticket(t),
                None => atlas.clear_last_render_ticket(),
            }
        }
    }
}

/// Phase 2 clip Task 12: record the pre-frame snapshot state (layout, ticket,
/// version) into the open frame's `snapshot_touch` overlay, once per snapshot
/// per frame. Called by the snapshot-touching ops (`masked_copy_area` SAMPLE
/// path here; `refresh_clip_snapshot` WRITE path in Task 13).
///
/// Borrow-split: the snapshot locals are read out of `inner.clip_snapshots`
/// before the mutable `inner.frame_builder.open` borrow (sibling fields of
/// `inner`).
pub(super) fn snapshot_first_touch(inner: &mut RenderEngineInner, sid: SnapshotId) {
    let (layout, ticket, ver) = {
        let snap = inner.clip_snapshots.get(&sid).expect("snapshot");
        (
            snap.current_layout,
            snap.last_render_ticket.clone(),
            snap.snapshotted_version,
        )
    };
    let open = inner.frame_builder.open.as_mut().expect("open");
    open.snapshot_touch
        .entry(sid)
        .or_insert((layout, ticket, ver));
}

/// Phase 2 clip Task 12: rollback snapshot-side state to pre-frame on any
/// close-time failure. Restores each touched snapshot's pre-frame layout,
/// `last_render_ticket`, and `snapshotted_version`. The version restore is
/// mandatory: a failed close where append already advanced the version (WRITE
/// path, Task 13) must restore the OLD version or the next frame skips a needed
/// re-refresh and samples stale bytes. Mirrors `rollback_atlas`.
/// A frame close that fails before its submit drops the frame: hand its
/// Present completions to the completion scheduler as an immediately ready
/// batch instead, like the post-flush failure path. X PRESENT must deliver
/// the events whether or not the copy ran.
fn rescue_present_completions(
    inner: &mut RenderEngineInner,
    open_frame: &mut crate::kms::render::frame_builder::OpenFrame,
) {
    let events = std::mem::take(&mut open_frame.pending_present_completions);
    if !events.is_empty() {
        inner.pending_present_batches.push(PendingPresentBatch {
            wait: PresentBatchWait::Ready,
            ticket: None,
            signal: None,
            events,
        });
    }
}

fn rollback_snapshots(
    inner: &mut RenderEngineInner,
    snapshot_touch: &mut std::collections::HashMap<
        SnapshotId,
        (vk::ImageLayout, Option<FenceTicket>, u64),
    >,
) {
    for (id, (pre_layout, prior_ticket, prev_version)) in snapshot_touch.drain() {
        if let Some(snap) = inner.clip_snapshots.get_mut(&id) {
            snap.current_layout = pre_layout;
            snap.last_render_ticket = prior_ticket;
            snap.snapshotted_version = prev_version;
        }
    }
}
