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

impl KmsBackend {
    pub(in crate::kms::render::backend) fn backend_scanout_on_scanout_render_completion(
        &mut self,
        _state: &mut ServerState,
    ) {
        let completions = self.platform.drain_scanout_render_completions();
        if !self.scanout_allowed() {
            // Draining avoids a readable aggregator spinning the core loop.
            // The suspend/topology lifecycle subsequently waits A, cancels
            // the ledger, and drains both pool devices before any allocation
            // can be reset or dropped.
            log::debug!(
                "render copied scanout: discarded {} completion(s) while scanout is inactive",
                completions.len(),
            );
            return;
        }
        for completion in completions {
            if self.platform.renderer_failed {
                break;
            }
            if !self
                .scene
                .handle_scanout_render_completion(completion, &mut self.platform)
            {
                self.telemetry.record_missed_pageflip();
            }
            if self.platform.renderer_failed {
                break;
            }
        }
    }

    pub(in crate::kms::render::backend) fn backend_scanout_on_page_flip_ready(
        &mut self,
        _state: &mut ServerState,
        drm_fd: std::os::fd::RawFd,
    ) {
        // Gate: when not Active we have no DRM master; page-flip events
        // are drained (so the fd doesn't stay readable) but no resubmit
        // or flush_submit_group runs. In Direct mode this is always false
        // → no behaviour change.
        if !self.scanout_allowed() {
            // Discard page-flip retires (no DRM master → don't touch scanout
            // state) but STILL run the sequence handler so the armed-target
            // map clears — leaving a stuck entry across suspend is exactly
            // the permanent-stall failure mode this guards against.
            if let Ok((_flips, sequences)) = self.platform.drain_page_flip_events(drm_fd) {
                for seq in sequences {
                    self.on_crtc_sequence_event(
                        seq.device_key,
                        seq.user_data,
                        seq.time_ns,
                        seq.sequence,
                    );
                }
            }
            log::debug!("render on_page_flip_ready: skipped (seat not Active)");
            return;
        }
        let (flipped, sequences) = match self.platform.drain_page_flip_events(drm_fd) {
            Ok(pair) => pair,
            Err(e) => {
                log::warn!("render: drain_page_flip_events failed: {e}");
                return;
            }
        };
        for (output_idx, clock) in flipped {
            let direct_retired = self.retire_direct_output(output_idx, clock);
            let scene_retired = !direct_retired
                && self.scene.handle_page_flip_complete(
                    output_idx,
                    &mut self.store,
                    &mut self.platform,
                );
            if direct_retired || scene_retired {
                self.telemetry.record_frame_present();
            }
            // Retry only the cursor state owned by the card whose output just
            // retired. A page flip on card A must not consume card B's EBUSY
            // slot or EINVAL backoff.
            match self
                .platform
                .cursor_plane_drain_pending_move_for_output(output_idx)
            {
                Ok(outcome) => {
                    self.handle_cursor_move_outcome(outcome);
                }
                Err(e) => log::debug!("render cursor drain on page-flip retire: {e}"),
            }
        }
        // Idle vblank arming: clear the arm + advance the Present clock for
        // each CRTC sequence the kernel delivered. The run loop reads the
        // updated `(msc, ust)` via `present_get_ust_msc` and fires parked
        // NotifyMSC, then re-arms if any remain.
        for seq in sequences {
            self.on_crtc_sequence_event(seq.device_key, seq.user_data, seq.time_ns, seq.sequence);
        }
        // Sweep retired engine submits + retired drawables now
        // that their fences may have signaled.
        self.engine.poll_retired(&self.platform);
        self.poll_pending_retire_with_invalidate();
        self.sync_descriptor_pool_telemetry();
        // Phase A T7: pageflip retire is a frame boundary — close
        // any open render batch FIRST so its CBs land in the group
        // under the same ticket that the subsequent flush will
        // consume. Then flush the SubmitGroup so an idle next tick
        // (no scene_structure_dirty) does not leave paint CBs
        // buffered until the next compose. Drive through the engine
        // wrapper so parked pending_group_ops commit to `submitted`
        // atomically.
        if let Err(e) = self.engine.flush_render_batch(
            &mut self.store,
            &mut self.platform,
            crate::kms::render::engine::RenderFlushReason::Present,
        ) {
            log::warn!("render on_page_flip_ready: flush_render_batch failed: {e:?}");
        }
        if let Err(e) = self.engine.flush_submit_group(
            &mut self.store,
            &mut self.platform,
            crate::kms::render::submit_group::FlushReason::PageflipRetire,
        ) {
            log::warn!("render on_page_flip_ready: flush_submit_group failed: {e:?}");
        }
    }

    pub(in crate::kms::render::backend) fn backend_scanout_before_block(&mut self) {
        // BlockHandler analog (cf. Xorg glamor_block_handler → glamor_flush):
        // every dispatch-loop iteration, just before the core loop blocks,
        // reap render-op resources whose fences have signaled. This is the
        // reclaim half of `on_page_flip_ready` (the scanout / compose / flush
        // half stays page-flip-driven), lifted onto the dispatch loop so it
        // runs even when no page-flip occurs.
        //
        // Without this, the engine `submitted` queue (one per-op command
        // buffer + any displaced images each) is drained ONLY on page-flip.
        // While the display is dark (DPMS-off / monitor standby / VT-away)
        // page-flips stop, but clients keep submitting render ops, so the
        // queue grows without bound until amdgpu can't allocate command-
        // submission memory and the device is lost
        // (project_reclamation_starvation_leak). poll_retired only frees
        // ops whose fence has signaled and is a cheap no-op on an empty
        // queue, so running it every iteration costs nothing at idle.
        self.engine.poll_retired(&self.platform);
        self.poll_pending_retire_with_invalidate();
        // #196: destroy pixmap-pool entries idle past the eviction age, so
        // a burst drains back down; `next_wakeup` wakes us for it.
        if let Some(pool) = self.platform.pixmap_pool.as_ref() {
            pool.trim_idle(std::time::Instant::now());
        }
        // Diagnostic: drive the 1Hz telemetry emit from here too,
        // publishing the live `submitted`-queue depth. maybe_emit()
        // self-gates to 1Hz and is a no-op below threshold, but running
        // it every dispatch iteration means the `render_telemetry:` line (and
        // the submit-trace flush) keep ticking even while the display is
        // dark — exactly when `submitted_queue_depth` is the number worth
        // watching (project_reclamation_starvation_leak). Without this,
        // the only other maybe_emit caller is on the compose path, which
        // is gated off when dark, so telemetry went silent in the window.
        self.telemetry.maybe_emit(self.engine.pending_count());
    }

    pub(in crate::kms::render::backend) fn backend_scanout_flush_before_damage_notify(&mut self) {
        // A compositor may sample a redirected backing after DamageNotify
        // or after retrieving coalesced damage via Subtract/FetchRegion.
        // Close all recording paths before publishing their producer fences.
        if let Err(error) = self.engine.flush_render_batch(
            &mut self.store,
            &mut self.platform,
            crate::kms::render::engine::RenderFlushReason::Other,
        ) {
            log::warn!("render damage boundary batch flush failed: {error:?}");
        }
        if let Err(error) = self.engine.close_open_frame(
            &mut self.store,
            &mut self.platform,
            crate::kms::render::frame_builder::CloseReason::DamageBoundary,
        ) {
            log::warn!("render DamageNotify submission boundary failed: {error:?}");
        }
        // Closing an already-closed frame does not drain command buffers
        // parked in the submit group (including the render batch above).
        if let Err(error) = self.engine.flush_submit_group(
            &mut self.store,
            &mut self.platform,
            crate::kms::render::submit_group::FlushReason::SyncBoundary,
        ) {
            log::warn!("render damage boundary submit flush failed: {error:?}");
        }
    }

    pub(in crate::kms::render::backend) fn backend_scanout_next_wakeup(
        &self,
    ) -> Option<std::time::Instant> {
        let now = std::time::Instant::now();
        let allow_kms_timers = self.scanout_allowed() && self.kms_outputs_active;
        let scene_deadline = if allow_kms_timers {
            if self.scene_wants_compose() {
                if self.scene.has_output_ready_for_submit() {
                    Some(now)
                } else {
                    self.scene.earliest_retry_deadline()
                }
            } else {
                self.scene.earliest_retry_deadline()
            }
        } else {
            None
        };
        let needs_present_poll = self.pending_present_batches.iter().any(|batch| {
            matches!(
                batch.wait,
                crate::kms::render::present_completion::PresentBatchWait::Poll
            )
        });
        let needs_source_wait_poll = self.pending_present_source_waits.values().any(|wait| {
            !wait.ready_reported
                && (wait.poll_timeline || wait.fds.iter().any(|fd| !fd.registered && !fd.ready))
        });
        let present_deadline = if needs_present_poll || needs_source_wait_poll {
            Some(now + std::time::Duration::from_millis(1))
        } else {
            None
        };
        let rescan_deadline = self
            .hotplug_rescan_deadline
            .map(|until| if now >= until { now } else { until });
        // A drawable freed while its GPU work was in flight waits in
        // `pending_retire` for `before_block` to see its fence signal; with
        // no other activity nothing would wake us to release it.
        let retire_deadline =
            (self.store.pending_retire_count() > 0).then(|| now + PENDING_RETIRE_POLL_INTERVAL);
        scene_deadline
            .into_iter()
            .chain(present_deadline)
            .chain(rescan_deadline)
            .chain(retire_deadline)
            // Not gated on `allow_kms_timers`: `maybe_composite` closes the
            // paint frame on its timeout while dark too (#177).
            .chain(self.engine.open_frame_timeout_deadline())
            .chain(
                self.platform
                    .pixmap_pool
                    .as_ref()
                    .and_then(|pool| pool.next_trim_deadline()),
            )
            .chain(
                allow_kms_timers
                    .then(|| self.cursor_anim_deadline())
                    .flatten(),
            )
            .min()
    }

    pub(in crate::kms::render::backend) fn backend_scanout_maybe_composite(
        &mut self,
    ) -> io::Result<()> {
        // GPU-reset recovery: once the renderer has observed a lost
        // device (a submit returned `ERROR_DEVICE_LOST` → `abort_flush`
        // latched `renderer_failed`), every subsequent tick used to
        // `return Ok(0)` forever — an infinite fence-poll spin that
        // leaves the screen corrupt until a hard reboot (never exits, so
        // the display manager can't respawn either). A card reset is
        // unrecoverable in-process here, so instead request a clean
        // shutdown: the RAII console/DRM-master guards restore a usable
        // TTY on the way out (no GPU access, safe on a dead device), and
        // lightdm respawns us on a fresh device. Checked before the
        // gates below so we exit even while VT-away / DPMS-off. Sends
        // `Message::Shutdown`, which the core loop drains next iteration,
        // so this fires ~once rather than per-frame.
        if self.platform.renderer_failed {
            log::error!(
                "kms: renderer device lost (GPU reset) — requesting clean shutdown \
                 so the display manager can respawn on a fresh device"
            );
            self.request_exit();
            return Ok(());
        }
        // Service the paint-batch deadline ahead of every scanout gate below
        // (VT, DPMS, direct-scanout hold). Closing the frame records its CB
        // and submits it on the render queue — Vulkan only, no KMS commit and
        // no DRM master — and clients keep drawing while the display is dark
        // or held by direct scanout. Behind the VT/DPMS gates the frame stayed
        // open until the 1024-pin ceiling forced it shut, then freed
        // everything it pinned in one burst: a live-allocation/VRAM sawtooth
        // with the monitor off (#177). Behind the direct-scanout gate the
        // final gkrellm update stayed open until unrelated screen activity
        // closed it.
        if let Err(e) = self
            .engine
            .close_open_frame_if_timed_out(&mut self.store, &mut self.platform)
        {
            log::warn!("render maybe_composite: timeout close failed: {e:?}");
        }
        // VT-master gate: while a VT switch is in progress or the GPU is
        // handed to another session, every
        // atomic_commit returns `EACCES`. `composite_and_flip` has
        // the same gate at :3263; `maybe_composite` was missing it
        // and emitted a burst of "atomic commit failed for output …
        // Permission denied" WARNs across the VT-suspend window
        // (observed 2026-05-31 — 77 WARNs in 3 seconds on
        // `just startx` + VT switch under MATE).
        if !self.scanout_allowed() {
            self.drain_paint_submit_telemetry();
            return Ok(());
        }
        // DPMS gate: outputs are inactive (every CRTC has ACTIVE=0 +
        // MODE_ID=0 from disable_output). Submitting an atomic page-flip
        // commit against a disabled CRTC returns EINVAL. Without this
        // gate the core loop's per-iteration `backend.maybe_composite()`
        // call would loop a tight EINVAL storm while DPMS is Off (and
        // the `composite_and_flip` gate at :3196 wouldn't catch it —
        // maybe_composite is a separate scene.tick caller).
        // See project_einval_atomic_commit_storm_wedge memory entry.
        if !self.kms_outputs_active {
            self.drain_paint_submit_telemetry();
            return Ok(());
        }
        // Animated-cursor frame advance — after both gates above so
        // DPMS-off / VT-away never uploads (spec "DPMS / VT gating").
        self.tick_cursor_animation();
        if self.scanout_m2.active() {
            use crate::kms::render::scene::CursorPlaneMode;
            let cursor_mode = self.scene.cursor_mode();
            if self.platform.any_output_transformed() {
                self.request_direct_unflip("composite_tick_crtc_transform");
            } else if !matches!(cursor_mode, CursorPlaneMode::Hw)
                || !self.scene.root_overlay.is_empty()
            {
                let reason = match cursor_mode {
                    CursorPlaneMode::Mixed => "composite_tick_mixed_cursor",
                    CursorPlaneMode::Sw => "composite_tick_software_or_hidden_cursor",
                    CursorPlaneMode::Hw => "composite_tick_root_overlay",
                };
                self.request_direct_unflip(reason);
            }
            // Never race a composed commit against the all-output direct
            // transaction. A requested unflip starts on the first tick after
            // the direct transaction itself has fully retired.
            if self.scanout_m2.pending.is_some()
                || !self.scanout_m2.unflip_awaiting_outputs.is_empty()
                || (self.scanout_m2.hold_direct && !self.scanout_m2.unflip_requested)
            {
                self.drain_render_telemetry();
                self.telemetry.maybe_emit(self.engine.pending_count());
                return Ok(());
            }
            if self.scanout_m2.current.is_some() && self.scanout_m2.unflip_requested {
                if let Err(error) = self.submit_composed_unflip() {
                    log::error!(
                        "scanout_m2: synchronized composed unflip failed: {error}; degrading to per-output composed flips"
                    );
                    // The atomic transaction never replaced the planes: the
                    // kernel is still scanning the direct dma-buf, so the
                    // direct frame's pins must stay held. Arm the composed-flip
                    // retirement machinery and fall through into the per-output
                    // scene compose path below — the scene flips replace the
                    // planes this tick, and `retire_direct_output` releases the
                    // direct frame only after every output has retired on the
                    // composed framebuffer.
                    self.scanout_m2.unflip_awaiting_outputs =
                        (0..self.platform.outputs.len()).collect();
                    self.scanout_m2.degraded_composed_unflip = true;
                    self.scene.mark_scene_structure_dirty();
                } else {
                    // The atomic replacement itself is now pending on every
                    // CRTC. Do not fall through into per-output scene flips in
                    // this same tick: KMS correctly rejects those with EBUSY.
                    self.drain_render_telemetry();
                    self.telemetry.maybe_emit(self.engine.pending_count());
                    return Ok(());
                }
            }
        }
        // One main-loop tick = one frame_id. Submit events
        // recorded between calls share the surrounding tick's
        // id; the scene_compose event of this tick (if it
        // submits) also carries this id.
        self.telemetry.advance_frame();
        let can_submit_scene =
            self.scene_wants_compose() && self.scene.has_output_ready_for_submit();
        // #214 telemetry: did this tick's compose close submit paint, and
        // did the tick then compose anything?
        let mut legacy_close_submitted = false;
        if can_submit_scene {
            // Stage 5 Task 3 (render-composite generalization): flush
            // the render batch — scene.tick samples dst.
            self.drain_engine_present_batches();
            if let Err(e) = self.engine.flush_render_batch(
                &mut self.store,
                &mut self.platform,
                crate::kms::render::engine::RenderFlushReason::Present,
            ) {
                log::warn!("render maybe_composite: flush_render_batch failed: {e:?}");
            }
            // Phase B Invariant M3: close any open frame BEFORE legacy compose
            // records. compose samples drawable storage at record time
            // (scene.rs:1307), so the open frame's layout + ticket-touch overlays
            // must be committed before the compose CB lands. Retires at sub-phase
            // B.4 when compose itself ports into the frame builder.
            // NOTE: integration test for M3 lives in Task 23's mixed-sequence
            // smoke (frame_builder_mixed_sequence_smoke); Task 13 only adds
            // the wiring. Until Task 15 ports composite_glyphs into the frame
            // builder, no frame can be open, so this call is a no-op.
            match self.engine.close_open_frame(
                &mut self.store,
                &mut self.platform,
                crate::kms::render::frame_builder::CloseReason::LegacyScCompose,
            ) {
                Ok(crate::kms::render::frame_builder::CloseOutcome::Submitted { .. }) => {
                    legacy_close_submitted = true;
                }
                Ok(crate::kms::render::frame_builder::CloseOutcome::AlreadyClosed) => {}
                Err(e) => log::warn!("render maybe_composite: close_open_frame failed: {e:?}"),
            }
            // Phase A Task 4: flush the SubmitGroup so scene.tick
            // observes all paint CBs already submitted to the queue.
            // Compose stays on its own dedicated `vkQueueSubmit2`
            // (record_compose) — only the buffered paint group is
            // flushed here. Drive through the engine wrapper so
            // parked `pending_group_ops` commit too.
            if let Err(e) = self.engine.flush_submit_group(
                &mut self.store,
                &mut self.platform,
                crate::kms::render::submit_group::FlushReason::SceneCompose,
            ) {
                log::warn!("render maybe_composite: flush_submit_group failed: {e:?}");
            }
        }
        let result = if !can_submit_scene {
            Ok(())
        } else {
            let cow_host_xid = self.cow_host_xid();
            match self.scene.tick(
                &self.core,
                &mut self.store,
                &mut self.platform,
                &self.windows,
                &mut self.telemetry,
                cow_host_xid,
            ) {
                Ok(composed_outputs) => {
                    if legacy_close_submitted {
                        crate::kms::vk::submit_stats::SUBMITS
                            .record_legacy_sc_tick(!composed_outputs.is_empty());
                    }
                    if self.scanout_m2.reentry_blocked_until_composed
                        && composed_outputs.len() == self.platform.outputs.len()
                    {
                        self.scanout_m2.reentry_blocked_until_composed = false;
                        log::debug!(
                            "scanout_m2: composed fallback submitted; re-entry barrier cleared"
                        );
                    }
                    for output_idx in composed_outputs {
                        self.telemetry.record_composite_submit();
                        // One scene_compose event per output that presented
                        // this tick, keyed by the exact output index.
                        self.telemetry.record_submit_event(SubmitEvent {
                            frame_id: 0,
                            kind: SubmitKind::SceneCompose,
                            target_kind: TargetKind::Output,
                            target_id: u64::try_from(output_idx).unwrap_or(0),
                            batch_size: 1,
                            op: SubmitOp::None,
                            src_class: SrcClass::None,
                            mask_class: SrcClass::None,
                            pipeline_id: None,
                            flags: SubmitFlags::NONE,
                        });
                    }
                    Ok(())
                }
                Err(e) => {
                    if legacy_close_submitted {
                        crate::kms::vk::submit_stats::SUBMITS.record_legacy_sc_tick(false);
                    }
                    log::warn!("render maybe_composite: scene.tick failed: {e:?}");
                    Ok(())
                }
            }
        };
        self.drain_paint_submit_telemetry();
        result
    }

    /// Stage 4d — Composite Overlay Window allocation.
    ///
    /// The **0 → 1 claim edge only**: core owns the claim list and this
    /// backend counts nothing, so every call here is a first claim.
    /// Allocates screen-extent depth-24 storage at xid
    /// `COMPOSITE_OVERLAY_WINDOW` (0x103) and stores the resulting
    /// `DrawableId` on `self.cow_id`. The drawable stays off the normal
    /// scene path; xfwm4 paints its composited desktop into its own child
    /// window, so adding the COW as a topmost scene layer would cover the
    /// real output with a stale black surface.
    ///
    /// Initial fill: storage from `allocate_drawable_storage`
    /// is uninitialised Vk-DEVICE_LOCAL memory (same problem
    /// Stage 3f.14 fixed for `create_pixmap`). We do an explicit
    /// OPAQUE-black fill via `engine.fill_rect` so the
    /// compositor's first paint composites over a known value
    /// rather than recycled GPU garbage. Opaque, not transparent:
    /// the COW is depth-24, and a depth-24 drawable has no alpha
    /// channel on X11 — see `default_window_init_color`. The fill is
    /// best-effort — on the stub fixture (no Vk) `engine.fill_rect`
    /// errors; log + continue (storage already exists at xid level).
    pub(in crate::kms::render::backend) fn backend_scanout_get_overlay_window(
        &mut self,
        _origin: Option<OriginContext>,
    ) -> io::Result<bool> {
        if self.cow_id.is_some() {
            // Core only calls this on the 0 → 1 claim edge, so a live
            // `cow_id` here can only be a physical teardown the previous
            // final release deferred behind direct scanout: the protocol
            // resource was logically destroyed but the backend identity
            // and storage stayed alive for their safe replacement. Reuse
            // that identity and ask core to materialize the protocol
            // resource again.
            debug_assert!(
                self.deferred_cow_release,
                "get_overlay_window is the 0 → 1 edge; a live cow_id here \
                 without a deferred release means core and the backend have \
                 drifted",
            );
            self.deferred_cow_release = false;
            return Ok(true);
        }
        let fb_w = self.platform.fb_w.max(1);
        let fb_h = self.platform.fb_h.max(1);
        let storage = match self.platform.allocate_drawable_storage_as(
            fb_w,
            fb_h,
            24,
            crate::kms::vk::mem_accounting::MemCategory::WindowStorage,
        ) {
            Ok(storage) => {
                self.telemetry.record_storage_allocation();
                self.telemetry.record_image_view_create();
                storage
            }
            Err(e) => {
                // Test-fixture / no-Vk path: same shape as
                // `init_root_storage` — fall back to a null-view
                // stub so unit tests can exercise refcount /
                // scene-registration without a live Vk ICD.
                log::debug!("render get_overlay_window: no Vk, using stub COW storage: {e:?}");
                crate::kms::render::store::Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: u32::from(fb_w),
                        height: u32::from(fb_h),
                    },
                    crate::kms::render::platform::PlatformBackend::format_for_depth(24),
                )
            }
        };
        let xid = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
        // Defensive: if a stale mapping somehow survives a prior
        // teardown (decref's PendingFence path detaches xid for us,
        // but a synchronous-destroy path could race), detach first
        // so the allocate doesn't trip XidInUse.
        self.store.detach_xid(xid);
        let id = self
            .store_alloc(xid, DrawableKind::Window, 24, true, storage)
            .map_err(|e| {
                io::Error::other(format!("render get_overlay_window: store alloc: {e:?}"))
            })?;
        // Stage 3f.14 follow-on — zero-fill the fresh storage so
        // the compositor doesn't composite over recycled GPU
        // garbage on its first paint. Best-effort on stub paths.
        let rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D::default(),
            extent: ash::vk::Extent2D {
                width: u32::from(fb_w),
                height: u32::from(fb_h),
            },
        };
        if let Err(e) = self.engine.fill_rect(
            &mut self.store,
            &mut self.platform,
            Dst::server_internal(id),
            rect,
            default_window_init_color(24),
        ) && self.platform.vk.is_some()
        {
            log::warn!("render get_overlay_window: initial fill failed: {e:?}");
        }
        self.cow_id = Some(id);

        // Phase 2 Task 2.2 — also materialize the backend's window-
        // tree projection so the COW participates in build_scene /
        // hit-testing / paint resolution the same way any top-level
        // window does. The xid is the well-known protocol xid; v2
        // keys windows on host xid directly.
        let cow_host_xid = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
        let rank = self.alloc_window_stack_rank();
        let geom = WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 0,
            y: 0,
            width: fb_w,
            height: fb_h,
            depth: 24,
            mapped: true,
            viewable: true,
            // `parent: None` matches windows's convention for a
            // direct child of the root (root is not itself tracked
            // in windows — see register_top_level).
            parent: None,
            stack_rank: rank,
            bg_pixel: None,
            bg_pixmap: None,
            cursor: None,
        };
        self.windows.insert(cow_host_xid, geom);
        // The COW takes the pointer until its input region is emptied, so
        // crossings resolve it like any window (Nonlinear to a sibling).
        self.core.xid_map.insert(
            cow_host_xid,
            yserver_core::resources::COMPOSITE_OVERLAY_WINDOW,
        );
        self.deferred_cow_release = false;
        // Step 2 (DRIFT 2): the COW's place in top_level_order is no longer
        // set here — the GetOverlayWindow core handler reprojects from core
        // children via `sync_top_level_order` AFTER materialize_cow_resource
        // (which inserts the COW as a root child capped on top).
        self.scene.mark_scene_structure_dirty();
        Ok(true)
    }

    /// Stage 4d — Composite Overlay Window release.
    ///
    /// The **1 → 0 claim edge only**: core owns the claim list, so every
    /// call here is the final release. Decrefs the store storage and
    /// clears `self.cow_id`. If direct scanout is active, the logical
    /// release succeeds immediately but physical teardown is deferred
    /// until the composed replacement retires.
    /// `DrawableStore::decref` removes the xid mapping (immediately on
    /// synchronous-destroy, deferred on `PendingFence`) so the next
    /// `GetOverlayWindow` reallocates fresh storage at the same xid.
    ///
    /// Returns `Ok(false)` when nothing was materialized — defensive
    /// only; core does not call this without a claim.
    pub(in crate::kms::render::backend) fn backend_scanout_release_overlay_window(
        &mut self,
        _origin: Option<OriginContext>,
    ) -> io::Result<bool> {
        if self.cow_id.is_none() {
            return Ok(false);
        }
        if self.scanout_m2.active() {
            // Do this while cow_id and its storage owner are authoritative.
            // Failure leaves the COW and the direct pins untouched, so core
            // can keep the caller's claim and let the compositor retry, or
            // fail safely without freeing a scanned buffer.
            self.materialize_direct_shadow_for_unflip()?;
            self.request_direct_unflip("release_last_overlay_window");
            self.deferred_cow_release = true;
            return Ok(true);
        }
        self.finish_cow_release();
        Ok(true)
    }

    pub(in crate::kms::render::backend) fn backend_scanout_cow_host_xid(&self) -> Option<u32> {
        // The COW's host xid is the well-known protocol xid once
        // get_overlay_window has materialized; None otherwise.
        if self.cow_id.is_some() {
            Some(yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0)
        } else {
            None
        }
    }
}
