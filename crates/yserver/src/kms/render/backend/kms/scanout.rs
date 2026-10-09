use super::*;

pub(in crate::kms::render::backend) fn scanout_m2_is_authoritative_root(
    target: ScanoutM0Target,
    root_coverage: bool,
) -> bool {
    matches!(
        target,
        ScanoutM0Target::Cow | ScanoutM0Target::CowDescendant | ScanoutM0Target::Unredirected
    ) && root_coverage
}

#[allow(clippy::too_many_arguments)]
pub(in crate::kms::render::backend) fn scanout_direct_eligible(
    scanout_allowed: bool,
    kms_outputs_active: bool,
    cursor_hw: bool,
    root_overlay_empty: bool,
    authoritative_root: bool,
    unbordered: bool,
    x_off: i16,
    y_off: i16,
    valid_region_xid: u32,
) -> bool {
    scanout_allowed
        && kms_outputs_active
        && cursor_hw
        && root_overlay_empty
        && authoritative_root
        && unbordered
        && x_off == 0
        && y_off == 0
        && valid_region_xid == 0
    // explicit_sync and update_region/update_is_full are intentionally NOT
    // consulted: an authoritative-root (fullscreen) present replaces the
    // whole scanout buffer, and the acquire fence is already awaited
    // (source_ready) before try_present_direct runs.
    //
    // #133 step 3 (3.5) — `unbordered` is a HARD phase-1 rule: the flip
    // path assumes the presented content starts at storage `(0, 0)`, and
    // a bordered window's storage starts `bw` earlier (its OUTER origin,
    // per `compAllocPixmap`, `composite/compalloc.c:610`). It must land
    // with the storage-layout change rather than after it, or a bordered
    // window can be flipped with a `bw`-shifted source. It costs nothing
    // on real desktops (every WM in the current smoke set uses
    // `border_width = 0`). Lifting it later needs the bordered storage +
    // source crop proven valid.
}

/// Whether no Bounding or Clip shape on `leaf_xid` or any ancestor up to the
/// root (the COW included) removes part of the `root` rect.
///
/// Xorg flips a Present only when the window's `clipList` equals the root's
/// `winSize` (`present/present_scmd.c:102`), and every shape in the chain
/// narrows that clip list (`SetWinSize`, `dix/window.c:1713`, propagated to
/// descendants by `miComputeClips`). Muffin's lock screen is the load-bearing
/// case: it shapes the COW to an EMPTY region and unredirects the locker, so
/// its stage's Presents are clipped to nothing and the locker below shows.
/// Shape rects are relative to each window's content origin.
fn direct_shape_chain_covers_root(
    windows: &WindowsMap,
    root_window_id: u32,
    shape_bounding: &HashMap<u32, Vec<xfixes::RegionRect>>,
    shape_clip: &HashMap<u32, Vec<xfixes::RegionRect>>,
    leaf_xid: u32,
    root: (u32, u32),
) -> bool {
    use crate::kms::render::region::Region;

    let mut chain = Vec::new();
    let mut xid = leaf_xid;
    // Resource validation prevents cycles in production; stay bounded anyway.
    for _ in 0..=windows.len() {
        let Some(geometry) = windows.get(&xid) else {
            return false;
        };
        let bw = i32::from(geometry.border_width);
        chain.push((xid, i32::from(geometry.x) + bw, i32::from(geometry.y) + bw));
        match geometry.parent {
            None => break,
            Some(parent) if parent == root_window_id => break,
            Some(parent) => xid = parent,
        }
    }
    let root_rect = vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent: vk::Extent2D {
            width: root.0,
            height: root.1,
        },
    };
    let (mut abs_x, mut abs_y) = (0, 0);
    for &(xid, x, y) in chain.iter().rev() {
        abs_x += x;
        abs_y += y;
        for shapes in [shape_bounding, shape_clip] {
            let Some(rects) = shapes.get(&xid) else {
                continue;
            };
            // Subtract rect by rect: a capped remainder only grows, so the
            // answer can err towards "not covered", never towards a flip.
            let mut uncovered = Region::from_rect(root_rect);
            for rect in rects {
                uncovered.subtract(&Region::from_rect(vk::Rect2D {
                    offset: vk::Offset2D {
                        x: abs_x + i32::from(rect.x),
                        y: abs_y + i32::from(rect.y),
                    },
                    extent: vk::Extent2D {
                        width: u32::from(rect.width),
                        height: u32::from(rect.height),
                    },
                }));
            }
            if !uncovered.is_empty() {
                return false;
            }
        }
    }
    true
}

/// Decide, per output, whether a CRTC is displaying a directly-flipped client
/// buffer and which retained frame it is — or the compositor's own scanout BO.
///
/// A direct transaction is submitted to every CRTC at once
/// (`submit_direct_frame` builds one plane state per `platform.outputs`
/// entry), but retirement and the composed unflip are per-CRTC, so the two
/// transitions leave a window where outputs disagree:
///
/// * `pending_awaiting` holds the outputs whose flip has NOT retired yet.
///   Those still show the predecessor (`current`, or the composed pool if this
///   is the first direct frame); the rest already show `pending`.
/// * `unflip_awaiting` holds the outputs whose composed replacement has NOT
///   retired yet. Those are still direct; an output missing from a non-empty
///   set has already been handed back to its pool BO. The set is emptied when
///   the unflip fully retires, which also drops both frames.
pub(in crate::kms::render::backend) fn direct_frame_slot_on_output(
    output_idx: usize,
    pending_awaiting: Option<&HashSet<usize>>,
    current_present: bool,
    unflip_awaiting: &HashSet<usize>,
) -> Option<DirectFrameSlot> {
    if !unflip_awaiting.is_empty() && !unflip_awaiting.contains(&output_idx) {
        return None;
    }
    if pending_awaiting.is_some_and(|awaiting| !awaiting.contains(&output_idx)) {
        return Some(DirectFrameSlot::Pending);
    }
    current_present.then_some(DirectFrameSlot::Current)
}

pub(in crate::kms::render::backend) fn phase_b_flip_in_flight_for_scheduler(
    scene_flip_pending: bool,
    direct_flip_pending: bool,
    unflip_pending: bool,
) -> bool {
    scene_flip_pending || direct_flip_pending || unflip_pending
}

pub(in crate::kms::render::backend) fn scanout_m1_probe_eligible(
    scanout_allowed: bool,
    kms_outputs_active: bool,
    cursor_hw: bool,
    root_overlay_empty: bool,
    target: ScanoutM0Target,
    coverage: ScanoutM0Coverage,
    x_off: i16,
    y_off: i16,
    valid_region_xid: u32,
) -> bool {
    scanout_allowed
        && kms_outputs_active
        && cursor_hw
        && root_overlay_empty
        && matches!(
            target,
            ScanoutM0Target::Cow | ScanoutM0Target::CowDescendant | ScanoutM0Target::Unredirected
        )
        && matches!(coverage, ScanoutM0Coverage::Root)
        && x_off == 0
        && y_off == 0
        && valid_region_xid == 0
}

#[cfg(test)]
pub(in crate::kms::render::backend) fn classify_scanout_m0_coverage(
    rect: Option<(i32, i32, u32, u32)>,
    source_extent: (u32, u32),
    root_extent: (u32, u32),
    outputs: &[(i32, i32, u32, u32)],
) -> ScanoutM0Coverage {
    let Some(rect) = rect else {
        return ScanoutM0Coverage::None;
    };
    if rect == (0, 0, root_extent.0, root_extent.1) && source_extent == root_extent {
        return ScanoutM0Coverage::Root;
    }
    outputs
        .iter()
        .position(|output| rect == *output && source_extent == (output.2, output.3))
        .map_or(ScanoutM0Coverage::None, ScanoutM0Coverage::Output)
}

pub(in crate::kms::render::backend) fn scanout_m1_outputs_cover_root(
    root: (u32, u32),
    outputs: &[ScanoutM1OutputGeometry],
) -> bool {
    if outputs.is_empty() {
        return false;
    }
    let mut area = 0u64;
    for (index, output) in outputs.iter().enumerate() {
        if output.x < 0
            || output.y < 0
            || output.width == 0
            || output.height == 0
            || output.width != output.mode_width
            || output.height != output.mode_height
        {
            return false;
        }
        let x = u32::try_from(output.x).expect("non-negative checked above");
        let y = u32::try_from(output.y).expect("non-negative checked above");
        if x.saturating_add(output.width) > root.0 || y.saturating_add(output.height) > root.1 {
            return false;
        }
        for other in &outputs[..index] {
            let separated = output.x + i32::try_from(output.width).unwrap_or(i32::MAX) <= other.x
                || other.x + i32::try_from(other.width).unwrap_or(i32::MAX) <= output.x
                || output.y + i32::try_from(output.height).unwrap_or(i32::MAX) <= other.y
                || other.y + i32::try_from(other.height).unwrap_or(i32::MAX) <= output.y;
            if !separated {
                return false;
            }
        }
        area = area.saturating_add(u64::from(output.width) * u64::from(output.height));
    }
    area == u64::from(root.0) * u64::from(root.1)
}

impl KmsBackend {
    pub(in crate::kms::render::backend) fn handle_cursor_move_outcome(
        &mut self,
        outcome: crate::kms::render::platform::CursorMoveOutcome,
    ) {
        self.telemetry
            .record_cursor_move_ebusy(u64::from(outcome.ebusy_count));
        if outcome.fallback_changed || outcome.retry_required {
            self.scene.wake_for_damage();
            if self.scanout_m2.active() {
                self.request_direct_unflip("cursor_move_fallback_or_retry");
            }
        }
    }

    /// Whether every frame direct scanout holds (on screen, submitted, or
    /// queued) is the composite overlay window or a descendant of it.
    /// `false` when there are none, or when any is an ordinary window.
    pub(in crate::kms::render::backend) fn direct_frames_are_under_cow(&self) -> bool {
        let frames = [
            self.scanout_m2.current.as_ref(),
            self.scanout_m2.pending.as_ref(),
            self.scanout_m2.queued_successor.as_ref(),
        ];
        let mut any = false;
        for frame in frames.into_iter().flatten() {
            any = true;
            if !matches!(
                self.scanout_m0_target(frame.candidate.paint_dst_host_xid, None, None),
                ScanoutM0Target::Cow | ScanoutM0Target::CowDescendant
            ) {
                return false;
            }
        }
        any
    }

    pub(in crate::kms::render::backend) fn request_direct_unflip(&mut self, reason: &'static str) {
        if !self.scanout_m2.active() {
            return;
        }
        if !self.scanout_m2.unflip_requested {
            self.scanout_m2.unflip_reason = Some(reason);
        }
        self.scanout_m2.unflip_requested = true;
        self.scanout_m2.unflip_last_reason = Some(reason);
        self.scanout_m2.hold_direct = false;
    }

    pub(in crate::kms::render::backend) fn direct_frame_references_host_drawable(
        &self,
        host_xid: u32,
    ) -> bool {
        let drawable_id = self.store.lookup(host_xid);
        let references = |frame: &DirectPresentFrame| {
            frame.candidate.paint_dst_host_xid == host_xid
                || drawable_id.is_some_and(|id| {
                    frame.source_id == id || frame.fallback_target.backing_id() == id
                })
        };
        self.scanout_m2.pending.as_ref().is_some_and(references)
            || self
                .scanout_m2
                .queued_successor
                .as_ref()
                .is_some_and(references)
            || self.scanout_m2.current.as_ref().is_some_and(references)
    }

    pub(in crate::kms::render::backend) fn finish_cow_release(&mut self) {
        self.deferred_cow_release = false;

        // Keep the backend COW projection and storage together until the
        // last owner is safe to drop. For a direct frame, this helper runs
        // only after the composed replacement retired and its pins released.
        let cow_host_xid = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
        self.windows.remove(&cow_host_xid);
        self.scene.mark_scene_structure_dirty();

        self.drain_engine_present_batches();
        if let Err(e) = self.engine.flush_render_batch(
            &mut self.store,
            &mut self.platform,
            crate::kms::render::engine::RenderFlushReason::Other,
        ) {
            log::warn!("render release_overlay_window: flush_render_batch failed: {e:?}");
        }
        self.drain_render_telemetry();
        if let Some(id) = self.cow_id.take() {
            self.store_decref_with_invalidate(id);
        }
    }

    fn finish_deferred_cow_release(&mut self) {
        if self.deferred_cow_release {
            self.finish_cow_release();
        }
    }

    fn bind_direct_cursor_on_all_outputs(&mut self) {
        if self.scanout_m2.cursor_bound_all {
            return;
        }
        if self.cursor_hidden {
            // XFIXES HideCursor: the direct frame owns the planes now, so
            // make sure no stale sprite stays bound on any of them.
            let mut failed = false;
            for output_idx in 0..self.platform.outputs.len() {
                if let Err(error) = self.platform.cursor_plane_hide_on_crtc(output_idx) {
                    failed = true;
                    log::warn!(
                        "scanout_m2: cursor hide failed on output {output_idx}: {error}; unflipping"
                    );
                }
            }
            if failed {
                self.request_direct_unflip("cursor_hide_failed");
            } else {
                self.scanout_m2.cursor_bound_all = true;
            }
            return;
        }
        let (hot_x, hot_y) = self
            .effective_cursor_xid
            .and_then(|xid| self.cursor_records.get(&xid))
            .map_or((0, 0), |record| (record.hot_x, record.hot_y));
        #[allow(clippy::cast_possible_truncation)]
        let x = self.core.cursor_x as i32;
        #[allow(clippy::cast_possible_truncation)]
        let y = self.core.cursor_y as i32;
        let mut failed = false;
        for output_idx in 0..self.platform.outputs.len() {
            if let Err(error) = self
                .platform
                .cursor_plane_show_on_crtc(output_idx, hot_x, hot_y, x, y)
            {
                failed = true;
                log::warn!(
                    "scanout_m2: cursor bind failed on output {output_idx}: {error}; unflipping"
                );
            }
        }
        if failed {
            self.request_direct_unflip("cursor_bind_failed");
        } else {
            self.scanout_m2.cursor_bound_all = true;
        }
    }

    /// Replace the live hardware sprite without disturbing an authoritative
    /// root scanout. M2 is restricted to one DRM device, and its existing
    /// direct-frame retirement already binds the cursor synchronously on each
    /// participating CRTC. A sprite-only update can use the same ownership
    /// model: upload once per output route (deduplicated by version inside the
    /// cursor plane), then rebind at the current position.
    pub(in crate::kms::render::backend) fn refresh_direct_cursor_on_all_outputs(
        &mut self,
        record: &crate::kms::render::cursor::CursorRecord,
    ) -> bool {
        if !self.scanout_m2.active() || self.platform.outputs.is_empty() {
            return false;
        }
        #[allow(clippy::cast_possible_truncation)]
        let x = self.core.cursor_x as i32;
        #[allow(clippy::cast_possible_truncation)]
        let y = self.core.cursor_y as i32;
        for output_idx in 0..self.platform.outputs.len() {
            if let Err(error) = self.platform.cursor_plane_upload_image_for_output(
                output_idx,
                record.version,
                u32::from(record.width),
                u32::from(record.height),
                &record.bgra_bytes,
            ) {
                log::warn!(
                    "scanout_m2: direct cursor upload failed on output {output_idx}: {error}; unflipping"
                );
                return false;
            }
            if let Err(error) = self.platform.cursor_plane_show_on_crtc(
                output_idx,
                record.hot_x,
                record.hot_y,
                x,
                y,
            ) {
                log::warn!(
                    "scanout_m2: direct cursor show failed on output {output_idx}: {error}; unflipping"
                );
                return false;
            }
        }
        self.scanout_m2.cursor_bound_all = true;
        true
    }

    fn pin_present_wake_for_direct(
        &self,
        event: &yserver_core::backend::CompletedPresentEvent,
    ) -> crate::kms::render::present_completion::PinnedWake {
        use crate::kms::render::present_completion::PinnedWake;
        use yserver_core::backend::PresentWake;

        match &event.wake {
            PresentWake::Pixmap { idle_fence_xid } if *idle_fence_xid != 0 => self
                .dri3_xshmfence_handle(*idle_fence_xid)
                .map_or(PinnedWake::None, PinnedWake::Pixmap),
            PresentWake::PixmapSynced {
                release,
                release_syncobj,
                release_value,
            } if *release_syncobj != 0 => PinnedWake::PixmapSynced {
                handle: release.clone(),
                value: *release_value,
            },
            _ => PinnedWake::None,
        }
    }

    pub(in crate::kms::render::backend) fn pin_direct_source(&mut self, id: DrawableId) -> u64 {
        self.store.incref(id);
        let pin_id = self.next_present_source_pin_id;
        self.next_present_source_pin_id = self.next_present_source_pin_id.wrapping_add(1).max(1);
        self.present_source_pins.insert(pin_id, id);
        pin_id
    }

    pub(in crate::kms::render::backend) fn retain_direct_present_wake(
        &mut self,
        event: &yserver_core::backend::CompletedPresentEvent,
    ) {
        use crate::kms::render::present_completion::PinnedWake;

        let wake_pin = self.pin_present_wake_for_direct(event);
        if !matches!(wake_pin, PinnedWake::None) {
            self.retained_present_wakes
                .insert(event.present_id, wake_pin);
        }
    }

    pub(in crate::kms::render::backend) fn submit_direct_frame(
        &mut self,
        frame: &mut DirectPresentFrame,
    ) -> io::Result<()> {
        #[cfg(test)]
        if self.scanout_m2.test_submit_direct_without_drm {
            frame.awaiting_outputs = (0..self.platform.outputs.len()).collect();
            return Ok(());
        }
        let fb = self
            .scanout_m1
            .entries
            .get(&frame.source_id)
            .and_then(ScanoutM1ProbeEntry::framebuffer)
            .map(crate::drm::modeset::DirectScanoutProbeFramebuffer::handle)
            .ok_or_else(|| io::Error::other("direct successor framebuffer disappeared"))?;
        let plane_states: Vec<crate::drm::modeset::DirectScanoutPlaneState<'_>> = self
            .platform
            .outputs
            .iter()
            .map(|layout| crate::drm::modeset::DirectScanoutPlaneState {
                output: &layout.output,
                src_x: u32::try_from(layout.x).expect("M1 validated non-negative x"),
                src_y: u32::try_from(layout.y).expect("M1 validated non-negative y"),
                src_w: u32::from(layout.width),
                src_h: u32::from(layout.height),
            })
            .collect();
        let primary = self.platform.primary_device().ok_or_else(|| {
            io::Error::other("direct scanout submitted without an opened KMS device")
        })?;
        crate::drm::modeset::submit_direct_scanout(&primary.device, fb, &plane_states)?;
        frame.awaiting_outputs = (0..self.platform.outputs.len()).collect();
        Ok(())
    }

    fn submit_queued_direct_successor(&mut self) {
        // A cursor/overlay/topology invalidation which arrived while the
        // predecessor was in flight wins over the queued Present. Leave the
        // successor retained for the composed-unflip teardown to Skip.
        if self.scanout_m2.unflip_requested {
            return;
        }
        let Some(mut successor) = self.scanout_m2.queued_successor.take() else {
            return;
        };
        match self.submit_direct_frame(&mut successor) {
            Ok(()) => {
                log::debug!(
                    "scanout_m2: submitted queued direct successor source_id={} present_id={} outputs={}",
                    successor.source_id.as_u64(),
                    successor.candidate.present_id,
                    self.platform.outputs.len()
                );
                self.scanout_m2.pending = Some(successor);
                self.scanout_m2.hold_direct = true;
            }
            Err(error) => {
                log::warn!(
                    "scanout_m2: queued direct successor submit failed after predecessor retirement: {error}"
                );
                self.defer_direct_successor_skip(successor);
                self.scanout_m2
                    .completed
                    .append(&mut self.scanout_m2.deferred_successor_skips);
                self.request_direct_unflip("queued_direct_successor_submit_failed");
            }
        }
    }

    fn release_direct_frame(&mut self, frame: DirectPresentFrame) {
        self.scanout_m2.idled.push(frame.event);
        <Self as Backend>::release_present_source(self, frame.source_pin);
        <Self as Backend>::release_present_source(self, frame.fallback_target_pin);
    }

    fn defer_direct_successor_skip(&mut self, mut frame: DirectPresentFrame) {
        frame.event.completion_mode = yserver_protocol::x11::present::COMPLETE_MODE_SKIP;
        // Match Present supersession: the discarded buffer becomes idle as
        // soon as it leaves the bounded successor slot, while its Skip
        // CompleteNotify remains ordered behind the in-flight predecessor.
        // `emit_idle = false` prevents the later completion from idling it a
        // second time.
        frame.event.emit_idle = false;
        self.scanout_m2.idled.push(frame.event.clone());
        self.scanout_m2.deferred_successor_skips.push(frame.event);
        <Self as Backend>::release_present_source(self, frame.source_pin);
        <Self as Backend>::release_present_source(self, frame.fallback_target_pin);
        self.note_present_skip();
    }

    pub(in crate::kms::render::backend) fn queue_direct_successor(
        &mut self,
        frame: DirectPresentFrame,
    ) {
        if let Some(superseded) = self.scanout_m2.queued_successor.replace(frame) {
            self.defer_direct_successor_skip(superseded);
        }
        self.scanout_m2.hold_direct = true;
        log::debug!(
            "scanout_m2: queued latest direct successor present_id={}",
            self.scanout_m2
                .queued_successor
                .as_ref()
                .map_or(0, |frame| frame.candidate.present_id)
        );
    }

    /// Release direct records only after the caller has disabled/replaced the
    /// primary planes. A not-yet-retired submission falls back to Copy mode.
    pub(in crate::kms::render::backend) fn stop_direct_after_scanout_replaced(
        &mut self,
        reason: &'static str,
    ) {
        if let Some(mut pending) = self.scanout_m2.pending.take() {
            pending.event.completion_mode = yserver_protocol::x11::present::COMPLETE_MODE_COPY;
            pending.event.emit_idle = true;
            self.scanout_m2.completed.push(pending.event);
            <Self as Backend>::release_present_source(self, pending.source_pin);
            <Self as Backend>::release_present_source(self, pending.fallback_target_pin);
        }
        if let Some(queued) = self.scanout_m2.queued_successor.take() {
            self.defer_direct_successor_skip(queued);
        }
        self.scanout_m2
            .completed
            .append(&mut self.scanout_m2.deferred_successor_skips);
        if let Some(current) = self.scanout_m2.current.take() {
            self.release_direct_frame(current);
        }
        self.scanout_m2.hold_direct = false;
        self.scanout_m2.cursor_bound_all = false;
        self.scanout_m2.unflip_requested = false;
        self.scanout_m2.unflip_reason = None;
        self.scanout_m2.unflip_last_reason = None;
        self.scanout_m2.unflip_awaiting_outputs.clear();
        self.scanout_m2.reentry_blocked_until_composed = false;
        self.scanout_m2.reset_eligible_root_probation();
        self.scanout_m2.unflip_fallback_source = None;
        self.scanout_m2.unflip_shadow_ready = false;
        self.scanout_m2.degraded_composed_unflip = false;
        self.finish_deferred_cow_release();
        log::debug!("scanout_m2: stopped after scanout replacement: {reason}");
    }

    /// Restore the current composed scanout after a topology operation had to
    /// disable every CRTC in order to release a grouped direct framebuffer.
    /// Failure is fatal: the direct source has already been released and the
    /// cached DPMS state cannot truthfully claim that scanout remains active.
    pub(in crate::kms::render::backend) fn relight_after_direct_teardown(
        &mut self,
        required: bool,
        context: &'static str,
    ) -> io::Result<()> {
        if !required {
            return Ok(());
        }
        if let Err(error) = self.platform.dpms_set_outputs_active(true) {
            self.kms_outputs_active = false;
            log::error!("scanout_m2: {context}: composed re-light failed: {error}; exiting");
            self.request_exit();
            return Err(io::Error::new(
                error.kind(),
                format!("scanout M2 {context}: composed re-light failed: {error}"),
            ));
        }
        // A full modeset may reset the hardware LUT. Reapply only after every
        // composed framebuffer has been restored, matching DPMS-on/resume.
        self.reapply_gamma_for_live_outputs();
        Ok(())
    }

    /// Stop grouped direct scanout while the old output/CRTC routing is still
    /// authoritative. A subsequent topology query is allowed to remove or
    /// reassign those objects, so releasing the client framebuffer afterward
    /// would be too late.
    pub(in crate::kms::render::backend) fn teardown_direct_before_topology_requery(
        &mut self,
        context: &'static str,
    ) -> io::Result<bool> {
        if !self.scanout_m2.active() {
            return Ok(false);
        }
        let relight = self.kms_outputs_active;
        self.materialize_direct_shadow_for_unflip()?;
        if let Err(error) = self.platform.dpms_set_outputs_active(false) {
            log::error!(
                "scanout_m2: {context}: could not disable the old grouped direct topology: {error}; exiting"
            );
            // Keep direct source pins/framebuffers alive: the helper attempts
            // every output and failure may mean one CRTC still scans them.
            self.request_exit();
            return Err(error);
        }
        self.stop_direct_after_scanout_replaced(context);
        self.scanout_m1.clear(context);
        // Disabling every CRTC cancels queued sequence events. Retaining an
        // arm entry would make the next composed frame believe the cancelled
        // request was still pending forever.
        self.clear_all_armed_vblank_targets();
        Ok(relight)
    }

    /// The direct frame output `output_idx`'s CRTC is scanning out right now,
    /// or `None` when that CRTC is showing its own composited scanout BO.
    ///
    /// Every on-screen READ has to go through this: while a CRTC scans out a
    /// client buffer directly, its pool BOs are not painted at all (see
    /// `retire_direct_output`'s `invalidate_all_scanout_damage`), so reading
    /// the pool returns arbitrarily old content that is not on screen.
    pub(in crate::kms::render::backend) fn direct_scanout_frame_for_output(
        &self,
        output_idx: usize,
    ) -> Option<&DirectPresentFrame> {
        let slot = direct_frame_slot_on_output(
            output_idx,
            self.scanout_m2
                .pending
                .as_ref()
                .map(|frame| &frame.awaiting_outputs),
            self.scanout_m2.current.is_some(),
            &self.scanout_m2.unflip_awaiting_outputs,
        )?;
        match slot {
            DirectFrameSlot::Pending => self.scanout_m2.pending.as_ref(),
            DirectFrameSlot::Current => self.scanout_m2.current.as_ref(),
        }
    }

    /// M2b lazy fallback: steady direct Presents skip their source-to-backing
    /// Copy. Before a non-Present-triggered unflip, materialize the currently
    /// scanned root source into the exact redirected paint target captured and
    /// pinned when that direct frame was submitted. This is the COW or nearest
    /// redirected backing for compositor frames, and the window's own backing
    /// for an Unredirected fullscreen frame.
    pub(in crate::kms::render::backend) fn materialize_direct_shadow_for_unflip(
        &mut self,
    ) -> io::Result<()> {
        if self.scanout_m2.unflip_shadow_ready {
            return Ok(());
        }
        let current = self
            .scanout_m2
            .pending
            .as_ref()
            .or(self.scanout_m2.current.as_ref())
            .ok_or_else(|| io::Error::other("scanout M2: no direct frame for fallback"))?;
        let source_id = current.source_id;
        let candidate = current.candidate;
        let target = current.fallback_target;
        // `CowDescendant` describes scene participation, not identity with
        // the Composite Overlay Window. In particular, Cinnamon can Present
        // a root descendant whose nearest redirected ancestor has its own
        // backing while the COW is a different drawable. `fallback_target`
        // is the paint-routing result for this Present and its pin keeps that
        // exact storage alive across an asynchronous unflip; comparing it to
        // the current `cow_id` rejects a valid and common startup state.
        if self.store.get(target.backing_id()).is_none() {
            return Err(io::Error::other(format!(
                "scanout M2: direct fallback target {} not in drawable store",
                target.backing_id().as_u64()
            )));
        }
        self.engine
            .cow_copy_area(
                &mut self.store,
                &mut self.platform,
                target.server_backing_dst(),
                Src::server_internal(source_id),
                ash::vk::Rect2D {
                    offset: ash::vk::Offset2D::default(),
                    extent: ash::vk::Extent2D {
                        width: u32::from(candidate.src_width),
                        height: u32::from(candidate.src_height),
                    },
                },
                ash::vk::Offset2D {
                    x: target.offset().0 + i32::from(candidate.x_off),
                    y: target.offset().1 + i32::from(candidate.y_off),
                },
            )
            .map_err(|error| {
                io::Error::other(format!("scanout M2 lazy fallback Copy: {error:?}"))
            })?;
        self.engine
            .flush_render_batch(
                &mut self.store,
                &mut self.platform,
                crate::kms::render::engine::RenderFlushReason::Present,
            )
            .map_err(|error| {
                io::Error::other(format!("scanout M2 lazy render flush: {error:?}"))
            })?;
        self.engine
            .close_open_frame(
                &mut self.store,
                &mut self.platform,
                crate::kms::render::frame_builder::CloseReason::PresentCompletionSignal,
            )
            .map_err(|error| {
                io::Error::other(format!("scanout M2 lazy fallback frame close: {error:?}"))
            })?;
        self.engine
            .flush_submit_group(
                &mut self.store,
                &mut self.platform,
                crate::kms::render::submit_group::FlushReason::PresentCompletionSignal,
            )
            .map_err(|error| io::Error::other(format!("scanout M2 lazy COW submit: {error:?}")))?;
        self.scanout_m2.unflip_shadow_ready = true;
        log::debug!(
            "scanout_m2: lazily materialized direct source_id={} into fallback_target={} for unflip",
            source_id.as_u64(),
            target.backing_id().as_u64()
        );
        Ok(())
    }

    /// Replace the shared direct framebuffer with every output's retained
    /// compositor framebuffer in one non-modesetting atomic transaction.
    /// AMD rejects independent per-CRTC replacement with `ENOSPC`; replacing
    /// the complete set together avoids both that intermediate state and the
    /// visible blackout of a disable/re-enable cycle.
    pub(in crate::kms::render::backend) fn submit_composed_unflip(&mut self) -> io::Result<()> {
        if !self.direct_scanout_topology_eligible() {
            return Err(io::Error::other(
                "scanout M2: grouped unflip requires one DRM device and homogeneous refresh",
            ));
        }
        self.materialize_direct_shadow_for_unflip()?;
        let planes: Vec<crate::drm::modeset::ComposedScanoutPlaneState<'_>> = self
            .platform
            .outputs
            .iter()
            .enumerate()
            .map(|(output_idx, layout)| {
                let fb = self
                    .platform
                    .retained_composed_framebuffer(output_idx)
                    .ok_or_else(|| {
                        io::Error::other(format!(
                            "scanout M2: output {output_idx} has no retained composed framebuffer"
                        ))
                    })?;
                Ok(crate::drm::modeset::ComposedScanoutPlaneState {
                    output: &layout.output,
                    fb,
                })
            })
            .collect::<io::Result<_>>()?;
        let primary = self.platform.primary_device().ok_or_else(|| {
            io::Error::other("scanout M2: composed unflip requested without a KMS device")
        })?;
        crate::drm::modeset::submit_composed_scanout(&primary.device, &planes)?;
        self.scanout_m2.unflip_awaiting_outputs = (0..planes.len()).collect();
        let describe_frame = |frame: &DirectPresentFrame| {
            (
                frame.source_id.as_u64(),
                frame.candidate.present_id,
                frame.candidate.paint_dst_host_xid,
            )
        };
        log::debug!(
            "scanout_m2: submitted atomic composed unflip outputs={} reason={} last_reason={} pending={:?} current={:?}",
            planes.len(),
            self.scanout_m2.unflip_reason.unwrap_or("unknown"),
            self.scanout_m2.unflip_last_reason.unwrap_or("unknown"),
            self.scanout_m2.pending.as_ref().map(describe_frame),
            self.scanout_m2.current.as_ref().map(describe_frame)
        );
        Ok(())
    }

    pub(in crate::kms::render::backend) fn retire_direct_output(
        &mut self,
        output_idx: usize,
        clock: yserver_core::backend::PresentClockSample,
    ) -> bool {
        if self.scanout_m2.unflip_awaiting_outputs.remove(&output_idx) {
            let degraded = self.scanout_m2.degraded_composed_unflip;
            if self.scanout_m2.unflip_awaiting_outputs.is_empty() {
                self.stop_direct_after_scanout_replaced(if degraded {
                    "degraded composed unflip"
                } else {
                    "atomic composed unflip"
                });
                self.scanout_m2.reentry_blocked_until_composed = true;
                // Step 3 — while a CRTC scanned out a client buffer directly,
                // the composed BOs were not painted and the scene was not
                // tracking them; a direct retirement also bypasses
                // `handle_page_flip_complete` entirely. Treat every composed BO
                // as wholly stale on the way back.
                self.scene.invalidate_all_scanout_damage();
                self.scene.mark_scene_structure_dirty();
                self.scanout_m2.degraded_composed_unflip = false;
                log::debug!("scanout_m2: composed unflip retired on all outputs");
            }
            if degraded {
                // The planes were replaced by the scene's own per-output
                // composed flips, which must still unwind their pending-acks
                // and BO state through the scene; hand the retire back.
                return false;
            }
            return true;
        }
        let Some(pending) = self.scanout_m2.pending.as_mut() else {
            return false;
        };
        if !pending.awaiting_outputs.remove(&output_idx) {
            return false;
        }
        if output_idx == pending.completion_output_idx {
            pending.completion_clock = Some(clock);
        }
        // Deliberately keep the platform pool's prior `OnScreen` BO reserved.
        // KMS no longer reads it after this external flip retires, but it is
        // the known-good framebuffer restored by the synchronized M2a unflip;
        // reserving it also prevents scene acquisition from repainting it
        // while the direct transaction is authoritative.
        if pending.awaiting_outputs.is_empty() {
            let mut presented = self
                .scanout_m2
                .pending
                .take()
                .expect("pending direct frame disappeared");
            let completion_clock = presented
                .completion_clock
                .expect("selected direct CRTC retired with the grouped flip");
            presented.event.completion_mode = yserver_protocol::x11::present::COMPLETE_MODE_FLIP;
            presented.event.emit_idle = false;
            presented.event.completion_clock = Some(completion_clock);
            self.scanout_m2.completed.push(presented.event.clone());
            // Xorg publishes the retiring flip before re-executing any
            // flip-ready successor. Coalesced successors therefore become
            // ordered Skip completions only at this retirement boundary.
            self.scanout_m2
                .completed
                .append(&mut self.scanout_m2.deferred_successor_skips);
            if let Some(previous) = self.scanout_m2.current.replace(presented) {
                self.release_direct_frame(previous);
            }
            log::debug!(
                "scanout_m2: direct frame retired on all outputs source_id={}",
                self.scanout_m2
                    .current
                    .as_ref()
                    .map_or(0, |frame| frame.source_id.as_u64())
            );
            self.bind_direct_cursor_on_all_outputs();
            // Like Xorg's present_flip_try_ready and wlroots' frame_pending
            // gate, submit at most one successor only after the kernel has
            // retired the preceding transaction.
            self.submit_queued_direct_successor();
        }
        true
    }

    /// Whether the current whole-root Present may use upstream's grouped
    /// all-output direct transaction.
    ///
    /// One atomic request cannot cross DRM devices. Even on one card, grouping
    /// heterogeneous-refresh CRTCs makes replacement retirement wait for the
    /// slowest CRTC; keep those layouts on the normal per-output composed path
    /// so each output retains its native cadence. A future direct path can
    /// relax this only with per-output authoritative sources and lifetimes.
    pub(in crate::kms::render::backend) fn direct_scanout_topology_eligible(&self) -> bool {
        let Some(primary) = self.platform.primary_device() else {
            return false;
        };
        let Some(first) = self.platform.outputs.first() else {
            return false;
        };
        if first.key.device_key != primary.key {
            return false;
        }
        self.platform.outputs.iter().all(|layout| {
            layout.key.device_key == primary.key
                && effective_refresh_matches(&first.output.picked, &layout.output.picked)
        })
    }

    /// A whole-root direct Present is paced in the selected RANDR CRTC's
    /// domain even though the grouped framebuffer is installed on every
    /// output. The selected CRTC therefore has to be live, owned by the
    /// primary DRM device that imported the framebuffer, and a member of the
    /// homogeneous single-device topology accepted by the grouped path.
    pub(in crate::kms::render::backend) fn direct_present_crtc_eligible(
        &self,
        crtc_id: u32,
        crtc_epoch: u64,
    ) -> bool {
        if crtc_epoch == 0 || self.present_crtc_clock_epoch(crtc_id) != crtc_epoch {
            return false;
        }
        let Some(crtc_key) = self.present_crtc_key(crtc_id) else {
            return false;
        };
        self.platform
            .primary_device()
            .is_some_and(|primary| crtc_key.device_key == primary.key)
            && self.direct_scanout_topology_eligible()
    }

    pub(in crate::kms::render::backend) fn maybe_probe_scanout_m1(
        &mut self,
        source_id: Option<DrawableId>,
        target: ScanoutM0Target,
        coverage: ScanoutM0Coverage,
        candidate: PresentScanoutCandidate,
    ) {
        if self.platform.any_output_transformed()
            || !self.direct_present_crtc_eligible(candidate.crtc_id, candidate.crtc_epoch)
            || !scanout_m1_probe_eligible(
                self.scanout_allowed(),
                self.kms_outputs_active,
                matches!(
                    self.scene.cursor_mode(),
                    crate::kms::render::scene::CursorPlaneMode::Hw
                ),
                self.scene.root_overlay.is_empty(),
                target,
                coverage,
                candidate.x_off,
                candidate.y_off,
                candidate.valid_region_xid,
            )
        {
            return;
        }
        let Some(source_id) = source_id else {
            return;
        };

        let topology_signature = self.scanout_m1_topology_signature();
        if self.scanout_m1.topology_signature != topology_signature {
            self.scanout_m1.clear("output topology changed");
            self.scanout_m1.topology_signature = topology_signature;
        }
        if self.scanout_m1.entries.contains_key(&source_id) {
            return;
        }

        let root = (u32::from(self.platform.fb_w), u32::from(self.platform.fb_h));
        let output_geometry: Vec<ScanoutM1OutputGeometry> = self
            .platform
            .outputs
            .iter()
            .map(|layout| {
                let (mode_width, mode_height) = layout.output.mode.size();
                ScanoutM1OutputGeometry {
                    x: layout.x,
                    y: layout.y,
                    width: u32::from(layout.width),
                    height: u32::from(layout.height),
                    mode_width: u32::from(mode_width),
                    mode_height: u32::from(mode_height),
                }
            })
            .collect();
        if !scanout_m1_outputs_cover_root(root, &output_geometry) {
            log::debug!(
                "scanout_m1: source_id={} skipped: active outputs do not exactly tile root {:?}: {:?}",
                source_id.as_u64(),
                root,
                output_geometry,
            );
            self.scanout_m1
                .entries
                .insert(source_id, ScanoutM1ProbeEntry::rejected());
            self.scanout_m0.m1_probe_reject = self.scanout_m0.m1_probe_reject.saturating_add(1);
            return;
        }

        let import = self.store.get(source_id).and_then(|drawable| {
            let metadata = drawable.storage.imported_dmabuf.as_ref()?;
            // An unresolved layout must never reach KMS. The client never
            // named it (legacy `PixmapFromBuffer`), so `metadata.modifier`
            // is our linear guess; scanning a tiled buffer out as linear
            // puts garbage on the display, and `add_fb2` may well accept
            // it. Refuse the direct path and let the ordinary composite
            // handle the pixmap instead.
            if metadata.implicit_layout {
                return None;
            }
            let plane = metadata.planes.first()?;
            let fd = drawable
                .storage
                .imported_drawable
                .as_ref()?
                .imported_dma_buf_fd()?;
            Some((
                metadata.fourcc,
                metadata.vk_format,
                metadata.modifier,
                metadata.planes.len(),
                plane.offset,
                plane.pitch,
                metadata.width,
                metadata.height,
                metadata.depth,
                metadata.bpp,
                fd.try_clone_to_owned(),
            ))
        });
        let Some((
            fourcc,
            vk_format,
            modifier,
            plane_count,
            offset,
            pitch,
            width,
            height,
            depth,
            bpp,
            fd_result,
        )) = import
        else {
            self.scanout_m0.m1_gate_reject_import =
                self.scanout_m0.m1_gate_reject_import.saturating_add(1);
            return;
        };
        const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258;
        if fourcc != DRM_FORMAT_XRGB8888
            || vk_format != vk::Format::B8G8R8A8_UNORM
            || plane_count != 1
            || depth != 24
            || bpp != 32
            || pitch == 0
            || (u32::from(width), u32::from(height)) != root
            || (candidate.src_width, candidate.src_height) != (width, height)
            || self.platform.outputs.iter().any(|layout| {
                !layout.output.scanout_modifiers.is_empty()
                    && !layout.output.scanout_modifiers.contains(&modifier)
            })
        {
            log::debug!(
                "scanout_m1: source_id={} skipped: incompatible metadata fourcc={fourcc:#010x} \
                 vk_format={vk_format:?} modifier={modifier:#x} planes={plane_count} \
                 size={}x{} depth={depth} bpp={bpp} pitch={pitch}",
                source_id.as_u64(),
                width,
                height,
            );
            self.scanout_m1
                .entries
                .insert(source_id, ScanoutM1ProbeEntry::rejected());
            self.scanout_m0.m1_probe_reject = self.scanout_m0.m1_probe_reject.saturating_add(1);
            return;
        }
        let fd = match fd_result {
            Ok(fd) => fd,
            Err(error) => {
                log::warn!(
                    "scanout_m1: source_id={} dma-buf dup failed: {error}",
                    source_id.as_u64()
                );
                self.scanout_m1
                    .entries
                    .insert(source_id, ScanoutM1ProbeEntry::rejected());
                self.scanout_m0.m1_probe_error = self.scanout_m0.m1_probe_error.saturating_add(1);
                return;
            }
        };
        let plane_states: Vec<crate::drm::modeset::DirectScanoutPlaneState<'_>> = self
            .platform
            .outputs
            .iter()
            .map(|layout| crate::drm::modeset::DirectScanoutPlaneState {
                output: &layout.output,
                src_x: u32::try_from(layout.x).expect("geometry validated non-negative"),
                src_y: u32::try_from(layout.y).expect("geometry validated non-negative"),
                src_w: u32::from(layout.width),
                src_h: u32::from(layout.height),
            })
            .collect();
        let Some(primary) = self.platform.primary_device() else {
            self.scanout_m1
                .entries
                .insert(source_id, ScanoutM1ProbeEntry::rejected());
            self.scanout_m0.m1_probe_reject = self.scanout_m0.m1_probe_reject.saturating_add(1);
            return;
        };
        let result = crate::drm::modeset::probe_direct_scanout_test_only(
            Rc::clone(&primary.device),
            fd.as_fd(),
            u32::from(width),
            u32::from(height),
            fourcc,
            modifier,
            offset,
            pitch,
            &plane_states,
        );
        match result {
            Ok(crate::drm::modeset::DirectScanoutTestResult::Accepted(framebuffer)) => {
                log::debug!(
                    "scanout_m1: TEST_ONLY passed source_id={} drawable_host={:#x} \
                     root={}x{} modifier={modifier:#x} pitch={pitch} outputs={:?}; \
                     live scanout unchanged",
                    source_id.as_u64(),
                    candidate.src_host_xid,
                    width,
                    height,
                    output_geometry,
                );
                self.scanout_m1
                    .entries
                    .insert(source_id, ScanoutM1ProbeEntry::accepted(framebuffer));
                self.scanout_m0.m1_probe_pass = self.scanout_m0.m1_probe_pass.saturating_add(1);
            }
            Ok(crate::drm::modeset::DirectScanoutTestResult::Rejected(error)) => {
                log::debug!(
                    "scanout_m1: TEST_ONLY rejected source_id={} drawable_host={:#x}: {error}",
                    source_id.as_u64(),
                    candidate.src_host_xid,
                );
                self.scanout_m1
                    .entries
                    .insert(source_id, ScanoutM1ProbeEntry::rejected());
                self.scanout_m0.m1_probe_reject = self.scanout_m0.m1_probe_reject.saturating_add(1);
            }
            Err(error) => {
                log::warn!(
                    "scanout_m1: import/probe failed source_id={} drawable_host={:#x}: {error}",
                    source_id.as_u64(),
                    candidate.src_host_xid,
                );
                self.scanout_m1
                    .entries
                    .insert(source_id, ScanoutM1ProbeEntry::rejected());
                self.scanout_m0.m1_probe_error = self.scanout_m0.m1_probe_error.saturating_add(1);
            }
        }
    }

    pub(in crate::kms::render::backend) fn scanout_m0_target(
        &self,
        dst_host_xid: u32,
        leaf_id: Option<DrawableId>,
        paint_id: Option<DrawableId>,
    ) -> ScanoutM0Target {
        let cow_xid = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
        if paint_id.is_some() && paint_id == self.cow_id {
            return ScanoutM0Target::Cow;
        }
        let mut current = Some(dst_host_xid);
        while let Some(xid) = current {
            if xid == cow_xid {
                return ScanoutM0Target::CowDescendant;
            }
            current = self.windows.get(&xid).and_then(|geometry| geometry.parent);
        }
        if leaf_id.is_some()
            && leaf_id == paint_id
            && leaf_id
                .and_then(|id| self.store.get(id))
                .is_some_and(|drawable| drawable.scene_participating)
        {
            ScanoutM0Target::Unredirected
        } else {
            ScanoutM0Target::Other
        }
    }

    pub(in crate::kms::render::backend) fn direct_shape_chain_covers_root(
        &self,
        leaf_xid: u32,
        root: (u32, u32),
    ) -> bool {
        direct_shape_chain_covers_root(
            &self.windows,
            self.core.window_id,
            &self.core.shape_bounding,
            &self.core.shape_clip,
            leaf_xid,
            root,
        )
    }

    /// Whether a retained direct frame now presents through a Bounding or
    /// Clip shape that no longer covers the root. A compositor may shape the
    /// COW without presenting again, so the shape change itself must unflip.
    pub(in crate::kms::render::backend) fn direct_frames_shaped_off_root(&self) -> bool {
        let root = (u32::from(self.platform.fb_w), u32::from(self.platform.fb_h));
        let shaped_off = |frame: &DirectPresentFrame| {
            !self.direct_shape_chain_covers_root(frame.candidate.paint_dst_host_xid, root)
        };
        self.scanout_m2.pending.as_ref().is_some_and(shaped_off)
            || self
                .scanout_m2
                .queued_successor
                .as_ref()
                .is_some_and(shaped_off)
            || self.scanout_m2.current.as_ref().is_some_and(shaped_off)
    }

    /// Whether an unredirected Present target is still the window which the
    /// scene would show for the whole root. Direct scanout bypasses that
    /// scene, so a mapped top-level raised above the candidate (or a workspace
    /// switch which unmaps it) must keep later Presents composed.
    pub(in crate::kms::render::backend) fn unredirected_direct_scene_eligible(
        &self,
        leaf_xid: u32,
        root: (u32, u32),
    ) -> bool {
        let mut top_xid = leaf_xid;
        let mut reached_top_level = false;
        // Resource validation prevents cycles in production, but keep this
        // conservative for transient/backend-test state.
        for _ in 0..=self.windows.len() {
            let Some(geometry) = self.windows.get(&top_xid) else {
                return false;
            };
            match geometry.parent {
                None => {
                    reached_top_level = true;
                    break;
                }
                Some(parent) if parent == self.core.window_id => {
                    reached_top_level = true;
                    break;
                }
                Some(parent) => top_xid = parent,
            }
        }
        if !reached_top_level {
            return false;
        }
        let Some(candidate) = self.windows.get(&top_xid) else {
            return false;
        };
        let root_w = i32::try_from(root.0).unwrap_or(i32::MAX);
        let root_h = i32::try_from(root.1).unwrap_or(i32::MAX);
        let covers_root = i32::from(candidate.x) <= 0
            && i32::from(candidate.y) <= 0
            && i32::from(candidate.x) + i32::from(candidate.width) >= root_w
            && i32::from(candidate.y) + i32::from(candidate.height) >= root_h;
        if !candidate.mapped || !covers_root {
            return false;
        }

        let cow_xid = self.cow_host_xid();
        let topmost_on_root = self
            .core
            .top_level_order
            .iter()
            .rev()
            .filter(|&&xid| Some(xid) != cow_xid)
            .find(|&&xid| {
                self.windows.get(&xid).is_some_and(|geometry| {
                    geometry.mapped
                        && i32::from(geometry.x) < root_w
                        && i32::from(geometry.y) < root_h
                        && i32::from(geometry.x) + i32::from(geometry.width) > 0
                        && i32::from(geometry.y) + i32::from(geometry.height) > 0
                })
            });
        topmost_on_root.is_some_and(|&xid| xid == top_xid)
    }

    pub(in crate::kms::render::backend) fn observe_scanout_m0(
        &mut self,
        candidate: PresentScanoutCandidate,
    ) {
        let source = self
            .store
            .lookup(candidate.src_host_xid)
            .and_then(|id| self.store.get(id).map(|drawable| (id, drawable)));
        let source_id = source.map(|(id, _)| id);
        let source_extent = source.map_or(
            (
                u32::from(candidate.src_width),
                u32::from(candidate.src_height),
            ),
            |(_, drawable)| {
                (
                    drawable.storage.extent.width,
                    drawable.storage.extent.height,
                )
            },
        );
        let depth = source.map_or(0, |(_, drawable)| drawable.depth);
        let (imported, fourcc, vk_format, modifier, plane_offset, plane_pitch, bpp) = source
            .map_or(
                (false, 0, vk::Format::UNDEFINED, 0, 0, 0, 0),
                |(_, drawable)| {
                    drawable.storage.imported_dmabuf.as_ref().map_or(
                        (false, 0, vk::Format::UNDEFINED, 0, 0, 0, 0),
                        |metadata| {
                            let plane = metadata.planes.first();
                            (
                                true,
                                metadata.fourcc,
                                metadata.vk_format,
                                metadata.modifier,
                                plane.map_or(0, |plane| plane.offset),
                                plane.map_or(0, |plane| plane.pitch),
                                metadata.bpp,
                            )
                        },
                    )
                },
            );
        let leaf_id = self.store.lookup(candidate.paint_dst_host_xid);
        let paint_target = self.resolve_paint_target(candidate.paint_dst_host_xid);
        let paint_id = paint_target.map(|target| target.backing_id());
        let target = self.scanout_m0_target(candidate.paint_dst_host_xid, leaf_id, paint_id);
        let rect = leaf_id
            .and_then(|id| self.window_absolute_rect(id))
            .map(|rect| {
                (
                    rect.offset.x,
                    rect.offset.y,
                    rect.extent.width,
                    rect.extent.height,
                )
            });
        let root_extent = (u32::from(self.platform.fb_w), u32::from(self.platform.fb_h));
        let coverage =
            if rect == Some((0, 0, root_extent.0, root_extent.1)) && source_extent == root_extent {
                ScanoutM0Coverage::Root
            } else {
                self.platform
                    .outputs
                    .iter()
                    .position(|output| {
                        rect == Some((
                            output.x,
                            output.y,
                            u32::from(output.width),
                            u32::from(output.height),
                        )) && source_extent == (u32::from(output.width), u32::from(output.height))
                    })
                    .map_or(ScanoutM0Coverage::None, ScanoutM0Coverage::Output)
            };
        let shape = ScanoutM0Shape {
            target,
            coverage,
            rect,
            source_extent,
            depth,
            bpp,
            imported,
            fourcc,
            vk_format,
            modifier,
            plane_offset,
            plane_pitch,
            offsets: (candidate.x_off, candidate.y_off),
            valid_region_present: candidate.valid_region_xid != 0,
            update_region_present: candidate.update_region_xid != 0,
            update_is_full: candidate.update_is_full,
        };
        let authoritative = !matches!(target, ScanoutM0Target::Other);
        let geometry_ok = !matches!(coverage, ScanoutM0Coverage::None);
        let offsets_ok = candidate.x_off == 0 && candidate.y_off == 0;
        let regions_ok = candidate.valid_region_xid == 0
            && candidate.update_region_xid == 0
            && candidate.update_is_full;
        // M1's shape gate intentionally ignores update-region metadata: a
        // root-covering authoritative Present replaces the whole scanout
        // buffer. Record every independent environment gate so a hardware
        // capture says exactly why an otherwise viable stream never probed.
        let m1_shape_candidate = scanout_m1_probe_eligible(
            true,
            true,
            true,
            true,
            target,
            coverage,
            candidate.x_off,
            candidate.y_off,
            candidate.valid_region_xid,
        );
        let crtc_eligible =
            self.direct_present_crtc_eligible(candidate.crtc_id, candidate.crtc_epoch);
        let scanout_allowed = self.scanout_allowed();
        let kms_outputs_active = self.kms_outputs_active;
        let cursor_mode = self.scene.cursor_mode();
        let cursor_hw = matches!(cursor_mode, crate::kms::render::scene::CursorPlaneMode::Hw);
        let root_overlay_empty = self.scene.root_overlay.is_empty();
        // Page flipping is off while any CRTC is transformed
        // (modesetting/present.c:266).
        let untransformed = !self.platform.any_output_transformed();
        let m1_gate_open = m1_shape_candidate
            && crtc_eligible
            && scanout_allowed
            && kms_outputs_active
            && cursor_hw
            && root_overlay_empty
            && untransformed
            && source_id.is_some();
        self.maybe_probe_scanout_m1(source_id, target, coverage, candidate);
        let diag = &mut self.scanout_m0;
        diag.presents = diag.presents.saturating_add(1);
        if authoritative {
            diag.authoritative = diag.authoritative.saturating_add(1);
        } else {
            diag.reject_target = diag.reject_target.saturating_add(1);
        }
        match coverage {
            ScanoutM0Coverage::Root => diag.root_coverage = diag.root_coverage.saturating_add(1),
            ScanoutM0Coverage::Output(_) => {
                diag.output_coverage = diag.output_coverage.saturating_add(1);
            }
            ScanoutM0Coverage::None => {
                diag.reject_geometry = diag.reject_geometry.saturating_add(1);
            }
        }
        if !imported {
            diag.reject_server_owned = diag.reject_server_owned.saturating_add(1);
        }
        if !offsets_ok {
            diag.reject_offsets = diag.reject_offsets.saturating_add(1);
        }
        if !regions_ok {
            diag.reject_regions = diag.reject_regions.saturating_add(1);
        }
        if m1_shape_candidate {
            diag.m1_gate_candidates = diag.m1_gate_candidates.saturating_add(1);
            if m1_gate_open {
                diag.m1_gate_open = diag.m1_gate_open.saturating_add(1);
            }
            if !crtc_eligible {
                diag.m1_gate_reject_crtc = diag.m1_gate_reject_crtc.saturating_add(1);
            }
            if !scanout_allowed {
                diag.m1_gate_reject_vt = diag.m1_gate_reject_vt.saturating_add(1);
            }
            if !kms_outputs_active {
                diag.m1_gate_reject_outputs = diag.m1_gate_reject_outputs.saturating_add(1);
            }
            if !cursor_hw {
                diag.m1_gate_reject_cursor = diag.m1_gate_reject_cursor.saturating_add(1);
            }
            if !root_overlay_empty {
                diag.m1_gate_reject_overlay = diag.m1_gate_reject_overlay.saturating_add(1);
            }
            if source_id.is_none() {
                diag.m1_gate_reject_source = diag.m1_gate_reject_source.saturating_add(1);
            }
        }
        if let Some(source_id) = source_id
            && diag.interval_sources.insert(source_id)
        {
            let recent = diag
                .recent_sources_by_dst
                .entry(candidate.paint_dst_host_xid)
                .or_default();
            if !recent.contains(&source_id) {
                const ROTATION_CAP: usize = 16;
                if recent.len() == ROTATION_CAP {
                    recent.pop_front();
                }
                recent.push_back(source_id);
                log::debug!(
                    "scanout_m0 new_buffer dst_host={:#x} source_id={} rotation_depth={}",
                    candidate.paint_dst_host_xid,
                    source_id.as_u64(),
                    recent.len(),
                );
            }
        }
        let shape_changed =
            diag.last_shape_by_dst.get(&candidate.paint_dst_host_xid) != Some(&shape);
        if shape_changed {
            let crops = if matches!(coverage, ScanoutM0Coverage::Root) {
                self.platform
                    .outputs
                    .iter()
                    .enumerate()
                    .map(|(index, output)| {
                        format!(
                            "{index}:{}x{}+{}+{}",
                            output.width, output.height, output.x, output.y
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(",")
            } else {
                String::new()
            };
            log::debug!(
                "scanout_m0 shape client={} present={} src_client={:#x} src_host={:#x} \
                 dst_client={:#x} dst_host={:#x} completion_host={:#x} source_id={:?} \
                 target={target:?} coverage={coverage:?} rect={rect:?} root={root_extent:?} \
                 source_extent={source_extent:?} imported={} fourcc={fourcc:#010x} \
                 vk_format={vk_format:?} \
                 modifier={modifier:#x} plane_offset={plane_offset} plane_pitch={plane_pitch} \
                 depth={depth} bpp={bpp} offsets=({},{}) valid={:#x} update={:#x} \
                 update_full={} crops=[{}] eligible={} options={:#x} \
                 m1_gates[shape={} crtc={} vt={} outputs={} cursor={cursor_mode:?} \
                 overlay_empty={} source={}]",
                candidate.client_id,
                candidate.present_id,
                candidate.src_pixmap_xid,
                candidate.src_host_xid,
                candidate.dst_window_xid,
                candidate.paint_dst_host_xid,
                candidate.completion_dst_host_xid,
                source_id.map(DrawableId::as_u64),
                imported,
                candidate.x_off,
                candidate.y_off,
                candidate.valid_region_xid,
                candidate.update_region_xid,
                candidate.update_is_full,
                crops,
                authoritative && geometry_ok && imported && offsets_ok && regions_ok,
                candidate.options,
                m1_shape_candidate,
                crtc_eligible,
                scanout_allowed,
                kms_outputs_active,
                root_overlay_empty,
                source_id.is_some(),
            );
            diag.last_shape_by_dst
                .insert(candidate.paint_dst_host_xid, shape);
        }
        if diag.interval_start.elapsed() >= std::time::Duration::from_secs(1) {
            log::debug!(
                "scanout_m0_summary presents={} authoritative={} root={} output={} \
                 distinct_sources={} reject_server_owned={} reject_target={} \
                 reject_geometry={} reject_offsets={} reject_regions={} \
                 m1_gate_candidates={} m1_gate_open={} m1_gate_reject[crtc={} vt={} \
                 outputs={} cursor={} overlay={} source={} import={}] \
                 m1_probe_pass={} m1_probe_reject={} m1_probe_error={}",
                diag.presents,
                diag.authoritative,
                diag.root_coverage,
                diag.output_coverage,
                diag.interval_sources.len(),
                diag.reject_server_owned,
                diag.reject_target,
                diag.reject_geometry,
                diag.reject_offsets,
                diag.reject_regions,
                diag.m1_gate_candidates,
                diag.m1_gate_open,
                diag.m1_gate_reject_crtc,
                diag.m1_gate_reject_vt,
                diag.m1_gate_reject_outputs,
                diag.m1_gate_reject_cursor,
                diag.m1_gate_reject_overlay,
                diag.m1_gate_reject_source,
                diag.m1_gate_reject_import,
                diag.m1_probe_pass,
                diag.m1_probe_reject,
                diag.m1_probe_error,
            );
            diag.interval_start = std::time::Instant::now();
            diag.interval_sources.clear();
            diag.presents = 0;
            diag.authoritative = 0;
            diag.root_coverage = 0;
            diag.output_coverage = 0;
            diag.reject_server_owned = 0;
            diag.reject_target = 0;
            diag.reject_geometry = 0;
            diag.reject_offsets = 0;
            diag.reject_regions = 0;
            diag.m1_gate_candidates = 0;
            diag.m1_gate_open = 0;
            diag.m1_gate_reject_crtc = 0;
            diag.m1_gate_reject_vt = 0;
            diag.m1_gate_reject_outputs = 0;
            diag.m1_gate_reject_cursor = 0;
            diag.m1_gate_reject_overlay = 0;
            diag.m1_gate_reject_source = 0;
            diag.m1_gate_reject_import = 0;
            diag.m1_probe_pass = 0;
            diag.m1_probe_reject = 0;
            diag.m1_probe_error = 0;
        }
    }

    /// True only when `vt_state` is `Active` — i.e. we hold DRM master
    /// and are allowed to submit page-flips, modesets, and GPU work.
    /// Gate every master-requiring operation on this. In Direct mode
    /// `vt_state` is always `Active`, so this is always `true` there.
    pub(in crate::kms::render::backend) fn scanout_allowed(&self) -> bool {
        self.vt_state.allows_scanout()
    }

    /// The scene needs a (re)compose when its structure changed
    /// (map/unmap/restack/redirect — `scene_structure_dirty`) OR a
    /// client painted a scene-participating window since the last
    /// compose (undrained presentation damage). `next_wakeup` and
    /// `maybe_composite` MUST use the same predicate: if `next_wakeup`
    /// armed a wake that `maybe_composite` then declined, the loop
    /// would busy-spin; if `maybe_composite` composed state that
    /// `next_wakeup` never wakes for, the paint would be stranded (the
    /// xfce submenu bug — a menu painted after its map-compose sat
    /// off-screen until an unrelated event poked the loop).
    pub(in crate::kms::render::backend) fn scene_wants_compose(&self) -> bool {
        self.scene.scene_structure_dirty
            || self.store.has_pending_presentation_damage()
            // An invalidated or never-presented frame owes a repaint no producer
            // reports; without this the tick is never driven to paint it.
            || self.scene.owes_repaint()
    }
}
