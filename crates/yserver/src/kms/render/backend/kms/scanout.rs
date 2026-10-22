use crate::kms::render::{
    backend::{
        DirectFrameSlot, DirectPresentFrame, KmsBackend, ScanoutM0Coverage, ScanoutM0Shape,
        ScanoutM0Target, ScanoutM1OutputGeometry, ScanoutM1ProbeEntry, WindowsMap,
        kms::randr::effective_refresh_matches,
    },
    scene::CursorUploadOutcome,
    store::DrawableId,
    target::Src,
};
use ash::vk;
use std::{
    collections::{HashMap, HashSet},
    io,
    os::fd::AsFd as _,
    rc::Rc,
};
use yserver_core::{
    backend::{Backend, PresentScanoutCandidate},
    server::ServerState,
};
use yserver_protocol::x11::xfixes;

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
    pub(in crate::kms::render::backend) fn monotonic_now(&self) -> std::time::Instant {
        (self.pstate.monotonic_clock)()
    }

    /// The direct scanout cursor gate depends on KMS plane visibility, so it
    /// stays with the KMS implementation while cursor image policy remains
    /// portable.
    pub(in crate::kms::render::backend) fn direct_cursor_eligible(&self) -> bool {
        if self.effective_cursor_hidden() {
            return !self.platform.any_cursor_plane_visible();
        }
        matches!(
            self.scene.cursor_mode(),
            crate::kms::render::scene::CursorPlaneMode::Hw
                | crate::kms::render::scene::CursorPlaneMode::Hidden
        )
    }

    pub(in crate::kms::render::backend) fn handle_cursor_move_outcome(
        &mut self,
        outcome: crate::kms::render::platform::CursorMoveOutcome,
    ) {
        self.telemetry
            .record_cursor_move_ebusy(u64::from(outcome.ebusy_count));
        if outcome.fallback_changed || outcome.retry_required {
            self.scene.wake_for_damage();
            if self.pstate.scanout_m2.active() {
                self.request_direct_unflip("cursor_move_fallback_or_retry");
            }
        }
    }

    /// Whether every frame direct scanout holds (on screen, submitted, or
    /// queued) is the composite overlay window or a descendant of it.
    /// `false` when there are none, or when any is an ordinary window.
    pub(in crate::kms::render::backend) fn direct_frames_are_under_cow(&self) -> bool {
        let frames = [
            self.pstate.scanout_m2.current.as_ref(),
            self.pstate.scanout_m2.pending.as_ref(),
            self.pstate.scanout_m2.queued_successor.as_ref(),
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
        if !self.pstate.scanout_m2.active() {
            return;
        }
        if !self.pstate.scanout_m2.unflip_requested {
            self.pstate.scanout_m2.unflip_reason = Some(reason);
        }
        self.pstate.scanout_m2.unflip_requested = true;
        self.pstate.scanout_m2.unflip_last_reason = Some(reason);
        self.pstate.scanout_m2.hold_direct = false;
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
        self.pstate
            .scanout_m2
            .pending
            .as_ref()
            .is_some_and(references)
            || self
                .pstate
                .scanout_m2
                .queued_successor
                .as_ref()
                .is_some_and(references)
            || self
                .pstate
                .scanout_m2
                .current
                .as_ref()
                .is_some_and(references)
    }

    /// Keep the scene's per-output cursor mode aligned with cursor planes
    /// changed outside a composed scene retirement.
    pub(super) fn sync_direct_cursor_scene_state(&mut self) -> bool {
        let visible_by_output: Vec<_> = (0..self.platform.outputs.len())
            .map(|output_idx| self.platform.cursor_plane_visible_for_output(output_idx))
            .collect();
        self.scene
            .record_external_cursor_plane_bindings(&visible_by_output)
    }

    fn bind_direct_cursor_on_all_outputs(&mut self) {
        if self.pstate.scanout_m2.cursor_bound_all {
            return;
        }
        let tearing = self.pstate.tearing.is_on();
        if tearing || self.effective_cursor_hidden() {
            // Tearing keeps every cursor plane detached. A hidden effective
            // cursor also must not leave a stale sprite bound over direct scanout.
            let mut failed = false;
            for output_idx in 0..self.platform.outputs.len() {
                if !self.platform.cursor_plane_visible_for_output(output_idx) {
                    continue;
                }
                if let Err(error) = self.platform.cursor_plane_hide_on_crtc(output_idx) {
                    failed = true;
                    log::warn!(
                        "scanout_m2: cursor hide failed on output {output_idx}: {error}; unflipping"
                    );
                }
            }
            let scene_synced = self.sync_direct_cursor_scene_state();
            if failed || !scene_synced {
                self.request_direct_unflip("cursor_hide_failed");
            } else {
                self.pstate.scanout_m2.cursor_bound_all = true;
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
        let scene_synced = self.sync_direct_cursor_scene_state();
        if failed || !scene_synced {
            self.request_direct_unflip("cursor_bind_failed");
        } else {
            self.pstate.scanout_m2.cursor_bound_all = true;
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
        if self.pstate.tearing.is_on()
            || !self.pstate.scanout_m2.active()
            || self.platform.outputs.is_empty()
        {
            return false;
        }
        #[allow(clippy::cast_possible_truncation)]
        let x = self.core.cursor_x as i32;
        #[allow(clippy::cast_possible_truncation)]
        let y = self.core.cursor_y as i32;
        let mut failed = false;
        for output_idx in 0..self.platform.outputs.len() {
            let upload_failure = match self.platform.cursor_plane_upload_image_for_output(
                output_idx,
                record.version,
                record.hot_x,
                record.hot_y,
                u32::from(record.width),
                u32::from(record.height),
                &record.bgra_bytes,
            ) {
                Ok(CursorUploadOutcome::Ready) => None,
                Ok(CursorUploadOutcome::Deferred) => Some(String::from("cursor upload deferred")),
                Err(error) => Some(error.to_string()),
            };
            if let Some(error) = upload_failure {
                log::warn!(
                    "scanout_m2: direct cursor upload failed on output {output_idx}: {error}; unflipping"
                );
                failed = true;
                break;
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
                failed = true;
                break;
            }
        }
        let scene_synced = self.sync_direct_cursor_scene_state();
        self.pstate.scanout_m2.cursor_bound_all = !failed && scene_synced;
        !failed && scene_synced
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

    pub(super) fn retain_direct_present_wake(
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

    pub(super) fn submit_direct_frame(
        &mut self,
        frame: &mut DirectPresentFrame,
        timing: crate::drm::async_flip::FlipTiming,
    ) -> io::Result<crate::drm::async_flip::AsyncOutcome> {
        let submitter = self.pstate.direct_frame_submitter;
        submitter(self, frame, timing)
    }

    pub(in crate::kms::render::backend) fn submit_direct_frame_live(
        &mut self,
        frame: &mut DirectPresentFrame,
        timing: crate::drm::async_flip::FlipTiming,
    ) -> io::Result<crate::drm::async_flip::AsyncOutcome> {
        let fb = self
            .pstate
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
        let (device, atomic_commit_sink) = self
            .platform
            .primary_device()
            .map(|primary| {
                (
                    Rc::clone(&primary.device),
                    Rc::clone(&primary.atomic_commit_sink),
                )
            })
            .ok_or_else(|| {
                io::Error::other("direct scanout submitted without an opened KMS device")
            })?;
        let output_indices: Vec<_> = (0..self.platform.outputs.len()).collect();
        let cursor_positions = self
            .platform
            .cursor_plane_atomic_positions_for_outputs(&output_indices);
        let async_shape = if timing == crate::drm::async_flip::FlipTiming::Async {
            Some(
                self.pstate
                    .scanout_m1
                    .entries
                    .get(&frame.source_id)
                    .and_then(|entry| entry.async_shape.clone())
                    .ok_or_else(|| {
                        io::Error::other("direct async submit has no probed buffer shape")
                    })?,
            )
        } else {
            None
        };
        let stream = crate::drm::async_flip::AsyncStream::Direct;
        if let Some(shape) = &async_shape {
            self.platform.async_ledger.observe_shape(stream, shape);
        }
        let async_allowed = async_shape
            .as_ref()
            .is_some_and(|shape| self.platform.async_ledger.allows(stream, shape));
        let attempt_timing = if async_allowed {
            crate::drm::async_flip::FlipTiming::Async
        } else {
            crate::drm::async_flip::FlipTiming::Vsync
        };
        let mut result = crate::drm::modeset::submit_direct_scanout(
            &device,
            atomic_commit_sink.as_ref(),
            fb,
            &plane_states,
            &cursor_positions,
            attempt_timing,
            crate::drm::atomic::TearingCommitContext::direct(
                Some(frame.present_received_at),
                frame.present_received_monotonic,
                false,
            ),
        );
        let async_outcome = if attempt_timing == crate::drm::async_flip::FlipTiming::Async {
            match result {
                Ok(_) => {
                    let shape = async_shape
                        .as_ref()
                        .expect("async submit carries its shape");
                    self.platform.async_ledger.note_accepted(stream, shape);
                    crate::drm::async_flip::AsyncOutcome::Accepted
                }
                Err(error) if error.raw_os_error() == Some(libc::EINVAL) => {
                    let shape = async_shape
                        .as_ref()
                        .expect("async submit carries its shape");
                    let suspended = self.platform.async_ledger.note_einval(stream, shape);
                    result = crate::drm::modeset::submit_direct_scanout(
                        &device,
                        atomic_commit_sink.as_ref(),
                        fb,
                        &plane_states,
                        &cursor_positions,
                        crate::drm::async_flip::FlipTiming::Vsync,
                        crate::drm::atomic::TearingCommitContext::direct(
                            Some(frame.present_received_at),
                            frame.present_received_monotonic,
                            true,
                        ),
                    );
                    crate::drm::async_flip::AsyncOutcome::FellBack { suspended }
                }
                Err(error) => {
                    result = Err(error);
                    crate::drm::async_flip::AsyncOutcome::NotRequested
                }
            }
        } else {
            crate::drm::async_flip::AsyncOutcome::NotRequested
        };
        if let Err(error) = &result
            && atomic_commit_sink.take_last_cursor_rejection()
        {
            let outcome = self
                .platform
                .reject_atomic_cursor_commit(&cursor_positions, &error.to_string());
            self.handle_cursor_move_outcome(outcome);
        }
        let accepted_event_id = result?;
        if attempt_timing == crate::drm::async_flip::FlipTiming::Vsync
            || matches!(
                async_outcome,
                crate::drm::async_flip::AsyncOutcome::FellBack { .. }
            )
        {
            self.platform
                .cursor_plane_scene_commit_submitted(&cursor_positions);
        }
        frame.awaiting_outputs = (0..self.platform.outputs.len()).collect();
        frame.accepted_event_id = Some(accepted_event_id);
        frame.async_outcome = async_outcome;
        Ok(async_outcome)
    }

    #[cfg(test)]
    pub(in crate::kms::render::backend) fn submit_direct_frame_for_tests(
        backend: &mut KmsBackend,
        frame: &mut DirectPresentFrame,
        _timing: crate::drm::async_flip::FlipTiming,
    ) -> io::Result<crate::drm::async_flip::AsyncOutcome> {
        frame.awaiting_outputs = (0..backend.platform.outputs.len()).collect();
        frame.async_outcome = crate::drm::async_flip::AsyncOutcome::NotRequested;
        Ok(crate::drm::async_flip::AsyncOutcome::NotRequested)
    }

    pub(super) fn direct_frame_flip_timing(
        &self,
        frame: &DirectPresentFrame,
    ) -> crate::drm::async_flip::FlipTiming {
        let owns_stream = matches!(
            self.pstate.tearing.state,
            crate::kms::render::backend::TearingState::On {
                window,
                crtc_epoch,
                ..
            } if window == frame.candidate.dst_window_xid
                && crtc_epoch == frame.candidate.crtc_epoch
        );
        let async_shape = self
            .pstate
            .scanout_m1
            .entries
            .get(&frame.source_id)
            .and_then(|entry| entry.async_shape.as_ref());
        if owns_stream
            && frame.candidate.options & yserver_core::present_scheduler::PRESENT_OPTION_ASYNC != 0
            && !self.platform.any_cursor_plane_visible()
            && async_shape.is_some_and(|shape| {
                self.platform
                    .async_ledger
                    .allows(crate::drm::async_flip::AsyncStream::Direct, shape)
            })
        {
            crate::drm::async_flip::FlipTiming::Async
        } else {
            crate::drm::async_flip::FlipTiming::Vsync
        }
    }

    fn submit_queued_direct_successor(&mut self) {
        // A cursor/overlay/topology invalidation which arrived while the
        // predecessor was in flight wins over the queued Present. Leave the
        // successor retained for the composed-unflip teardown to Skip.
        if self.pstate.scanout_m2.unflip_requested || self.pstate.scanout_m2.pending.is_some() {
            return;
        }
        let direct_outputs: Vec<_> = (0..self.platform.outputs.len()).collect();
        if self
            .platform
            .cursor_commit_in_flight_for_outputs(&direct_outputs)
        {
            return;
        }
        let Some(mut successor) = self.pstate.scanout_m2.queued_successor.take() else {
            return;
        };
        let timing = self.direct_frame_flip_timing(&successor);
        match self.submit_direct_frame(&mut successor, timing) {
            Ok(_) => {
                log::debug!(
                    "scanout_m2: submitted queued direct successor source_id={} present_id={} outputs={}",
                    successor.source_id.as_u64(),
                    successor.candidate.present_id,
                    self.platform.outputs.len()
                );
                self.pstate.scanout_m2.pending = Some(successor);
                self.pstate.scanout_m2.hold_direct = true;
            }
            Err(error) => {
                log::warn!(
                    "scanout_m2: queued direct successor submit failed after predecessor retirement: {error}"
                );
                self.defer_direct_successor_skip(successor);
                self.pstate
                    .scanout_m2
                    .completed
                    .append(&mut self.pstate.scanout_m2.deferred_successor_skips);
                self.request_direct_unflip("queued_direct_successor_submit_failed");
            }
        }
    }

    fn release_direct_frame(&mut self, frame: DirectPresentFrame) {
        self.pstate.scanout_m2.idled.push(frame.event);
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
        self.pstate.scanout_m2.idled.push(frame.event.clone());
        self.pstate
            .scanout_m2
            .deferred_successor_skips
            .push(frame.event);
        <Self as Backend>::release_present_source(self, frame.source_pin);
        <Self as Backend>::release_present_source(self, frame.fallback_target_pin);
        self.note_present_skip();
    }

    pub(in crate::kms::render::backend) fn queue_direct_successor(
        &mut self,
        frame: DirectPresentFrame,
    ) {
        if let Some(superseded) = self.pstate.scanout_m2.queued_successor.replace(frame) {
            self.defer_direct_successor_skip(superseded);
        }
        self.pstate.scanout_m2.hold_direct = true;
        log::debug!(
            "scanout_m2: queued latest direct successor present_id={}",
            self.pstate
                .scanout_m2
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
        if let Some(mut pending) = self.pstate.scanout_m2.pending.take() {
            pending.event.completion_mode = yserver_protocol::x11::present::COMPLETE_MODE_COPY;
            pending.event.emit_idle = true;
            self.pstate.scanout_m2.completed.push(pending.event);
            <Self as Backend>::release_present_source(self, pending.source_pin);
            <Self as Backend>::release_present_source(self, pending.fallback_target_pin);
        }
        if let Some(queued) = self.pstate.scanout_m2.queued_successor.take() {
            self.defer_direct_successor_skip(queued);
        }
        self.pstate
            .scanout_m2
            .completed
            .append(&mut self.pstate.scanout_m2.deferred_successor_skips);
        if let Some(current) = self.pstate.scanout_m2.current.take() {
            self.release_direct_frame(current);
        }
        self.pstate.scanout_m2.hold_direct = false;
        self.pstate.scanout_m2.cursor_bound_all = false;
        self.pstate.scanout_m2.unflip_requested = false;
        self.pstate.scanout_m2.unflip_reason = None;
        self.pstate.scanout_m2.unflip_last_reason = None;
        self.pstate.scanout_m2.unflip_outputs_to_replace.clear();
        self.pstate.scanout_m2.unflip_awaiting_outputs.clear();
        self.pstate.scanout_m2.reentry_blocked_until_composed = false;
        self.pstate.scanout_m2.reset_eligible_root_probation();
        self.pstate.scanout_m2.unflip_fallback_source = None;
        self.pstate.scanout_m2.unflip_shadow_ready = false;
        self.pstate.scanout_m2.degraded_composed_unflip = false;
        self.pstate.scanout_m2.fresh_composed_unflip = false;
        self.finish_deferred_cow_release();
        log::debug!("scanout_m2: stopped after scanout replacement: {reason}");
    }

    /// Restore the current composed scanout after a topology operation had to
    /// disable every CRTC in order to release a grouped direct framebuffer.
    /// Failure is fatal: the direct source has already been released and the
    /// cached DPMS state cannot truthfully claim that scanout remains active.
    pub(super) fn relight_after_direct_teardown(
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
    pub(super) fn teardown_direct_before_topology_requery(
        &mut self,
        context: &'static str,
    ) -> io::Result<bool> {
        if !self.pstate.scanout_m2.active() {
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
        self.pstate.scanout_m1.clear(context);
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
            self.pstate
                .scanout_m2
                .pending
                .as_ref()
                .map(|frame| &frame.awaiting_outputs),
            self.pstate.scanout_m2.current.is_some(),
            &self.pstate.scanout_m2.unflip_outputs_to_replace,
        )?;
        match slot {
            DirectFrameSlot::Pending => self.pstate.scanout_m2.pending.as_ref(),
            DirectFrameSlot::Current => self.pstate.scanout_m2.current.as_ref(),
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
        if self.pstate.scanout_m2.unflip_shadow_ready {
            return Ok(());
        }
        let current = self
            .pstate
            .scanout_m2
            .pending
            .as_ref()
            .or(self.pstate.scanout_m2.current.as_ref())
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
        if self.platform.gpu.vk().is_none() {
            return Err(io::Error::other(
                "NoVk: direct fallback materialization needs Vulkan storage",
            ));
        }
        let has_live_source_image = self
            .store
            .get(source_id)
            .is_some_and(|drawable| drawable.storage.image != ash::vk::Image::null());
        let has_live_target_image = self
            .store
            .get(target.backing_id())
            .is_some_and(|drawable| drawable.storage.image != ash::vk::Image::null());
        if !has_live_source_image || !has_live_target_image {
            return Err(io::Error::other(format!(
                "scanout M2: direct fallback source {} or target {} has no Vulkan image",
                source_id.as_u64(),
                target.backing_id().as_u64(),
            )));
        }
        self.engine
            .cow_copy_area(
                &mut self.store,
                &mut self.platform.gpu,
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
                &mut self.platform.gpu,
                crate::kms::render::engine::RenderFlushReason::Present,
            )
            .map_err(|error| {
                io::Error::other(format!("scanout M2 lazy render flush: {error:?}"))
            })?;
        self.engine
            .close_open_frame(
                &mut self.store,
                &mut self.platform.gpu,
                crate::kms::render::frame_builder::CloseReason::PresentCompletionSignal,
            )
            .map_err(|error| {
                io::Error::other(format!("scanout M2 lazy fallback frame close: {error:?}"))
            })?;
        self.engine
            .flush_submit_group(
                &mut self.store,
                &mut self.platform.gpu,
                crate::kms::render::submit_group::FlushReason::PresentCompletionSignal,
            )
            .map_err(|error| io::Error::other(format!("scanout M2 lazy COW submit: {error:?}")))?;
        self.pstate.scanout_m2.unflip_shadow_ready = true;
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
    fn submit_composed_unflip(&mut self) -> io::Result<bool> {
        let output_indices: Vec<_> = (0..self.platform.outputs.len()).collect();
        if self
            .platform
            .cursor_commit_in_flight_for_outputs(&output_indices)
        {
            return Ok(false);
        }
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
                    in_fence_fd: None,
                })
            })
            .collect::<io::Result<_>>()?;
        let output_count = planes.len();
        let (device, atomic_commit_sink) = self
            .platform
            .primary_device()
            .map(|primary| {
                (
                    Rc::clone(&primary.device),
                    Rc::clone(&primary.atomic_commit_sink),
                )
            })
            .ok_or_else(|| {
                io::Error::other("scanout M2: composed unflip requested without a KMS device")
            })?;
        let output_indices: Vec<_> = (0..planes.len()).collect();
        let cursor_positions = self
            .platform
            .cursor_plane_atomic_positions_for_outputs(&output_indices);
        let result = crate::drm::modeset::submit_composed_scanout(
            &device,
            atomic_commit_sink.as_ref(),
            &planes,
            &cursor_positions,
        );
        if let Err(error) = &result
            && atomic_commit_sink.take_last_cursor_rejection()
        {
            let outcome = self
                .platform
                .reject_atomic_cursor_commit(&cursor_positions, &error.to_string());
            self.handle_cursor_move_outcome(outcome);
        }
        let accepted_event_id = result?;
        self.platform
            .cursor_plane_scene_commit_submitted(&cursor_positions);
        self.pstate.scanout_m2.unflip_outputs_to_replace = (0..output_count).collect();
        self.pstate.scanout_m2.unflip_awaiting_outputs = (0..output_count)
            .map(|output_idx| (output_idx, accepted_event_id))
            .collect();
        let describe_frame = |frame: &DirectPresentFrame| {
            (
                frame.source_id.as_u64(),
                frame.candidate.present_id,
                frame.candidate.paint_dst_host_xid,
            )
        };
        log::debug!(
            "scanout_m2: submitted atomic composed unflip outputs={} reason={} last_reason={} pending={:?} current={:?}",
            output_count,
            self.pstate.scanout_m2.unflip_reason.unwrap_or("unknown"),
            self.pstate
                .scanout_m2
                .unflip_last_reason
                .unwrap_or("unknown"),
            self.pstate.scanout_m2.pending.as_ref().map(describe_frame),
            self.pstate.scanout_m2.current.as_ref().map(describe_frame)
        );
        Ok(true)
    }

    /// Whether the unflip can replace every direct plane with a freshly
    /// composed scene frame: shared scene outputs that can all accept a frame
    /// now, explicit in-fences, and the grouped unflip's own topology.
    fn fresh_composed_unflip_eligible(&self) -> bool {
        use crate::kms::render::scene::{OutputRoute, OutputSink};

        let outputs: Vec<_> = (0..self.platform.outputs.len()).collect();
        !outputs.is_empty()
            && !self.pstate.tearing.is_on()
            && self.pstate.scanout_m2.unflip_reason != Some("tearing_cursor_handoff")
            && self.direct_scanout_topology_eligible()
            && !self.platform.cursor_commit_in_flight_for_outputs(&outputs)
            && outputs.iter().all(|&output_idx| {
                self.platform.route(output_idx) == OutputRoute::Shared
                    && self.platform.outputs[output_idx]
                        .output
                        .plane_in_fence_fd_prop
                        .is_some()
                    && self
                        .scene
                        .output_ready_for_submit(output_idx, &self.platform)
            })
    }

    /// #281: compose a fresh frame from the materialized direct source on
    /// every output and flip all of them in one grouped atomic commit, each
    /// plane waiting on its compose through `IN_FENCE_FD`. Returns false
    /// when nothing was committed; the retained-framebuffer unflip then
    /// runs as before, and any frame already rendered here is recycled only
    /// after its compose fence signals.
    fn submit_fresh_composed_unflip(&mut self) -> bool {
        if !self.fresh_composed_unflip_eligible() {
            return false;
        }
        if let Err(error) = self.materialize_direct_shadow_for_unflip() {
            log::debug!("scanout_m2: fresh composed unflip skipped: {error}");
            return false;
        }
        self.scene.invalidate_all_scanout_damage();
        self.scene.mark_scene_structure_dirty();
        self.platform.defer_scene_flips = true;
        self.compose_tick();
        self.platform.defer_scene_flips = false;
        let deferred = std::mem::take(&mut self.platform.deferred_scene_flips);
        let output_count = self.platform.outputs.len();
        let incomplete: Vec<_> = deferred
            .iter()
            .filter(|&&(_, _, complete)| !complete)
            .map(|&(output_idx, _, _)| output_idx)
            .collect();
        let targets: Option<Vec<_>> = (0..output_count)
            .map(|output_idx| {
                let mut frames = deferred.iter().filter(|&&(out, _, _)| out == output_idx);
                match (frames.next(), frames.next()) {
                    (Some(&(_, bo_idx, _)), None) => {
                        self.platform.deferred_scene_flip_target(output_idx, bo_idx)
                    }
                    _ => None,
                }
            })
            .collect();
        let result = match targets {
            // A truncated compose painted only a prefix of the scene: never
            // flip it, the retained frames are whole.
            Some(_) if !incomplete.is_empty() => Err(io::Error::other(format!(
                "incomplete compose on outputs {incomplete:?}"
            ))),
            Some(targets) => self.commit_fresh_composed_unflip(&targets),
            None => Err(io::Error::other(format!(
                "composed {} of {output_count} outputs",
                deferred.len()
            ))),
        };
        let accepted_event_id = result.as_ref().ok().copied();
        for &(output_idx, bo_idx, _) in &deferred {
            self.platform.settle_deferred_scene_flip(
                output_idx,
                bo_idx,
                accepted_event_id.is_some(),
            );
            match accepted_event_id {
                Some(event_id) => {
                    if !self.scene.accept_deferred_flip(output_idx, event_id) {
                        log::error!(
                            "scanout_m2: fresh composed unflip committed output {output_idx} \
                             but the scene has no deferred frame to accept event {event_id}"
                        );
                    }
                }
                None => {
                    if !self
                        .scene
                        .abandon_deferred_flip(output_idx, &mut self.platform)
                    {
                        log::error!(
                            "scanout_m2: fresh composed unflip found no deferred scene frame \
                             to abandon on output {output_idx}"
                        );
                    }
                }
            }
        }
        let Some(accepted_event_id) = accepted_event_id else {
            log::warn!(
                "scanout_m2: fresh composed unflip failed: {}; using retained framebuffers",
                result
                    .err()
                    .map_or_else(String::new, |error| error.to_string())
            );
            return false;
        };
        self.pstate.scanout_m2.unflip_outputs_to_replace = (0..output_count).collect();
        self.pstate.scanout_m2.unflip_awaiting_outputs = (0..output_count)
            .map(|output_idx| (output_idx, accepted_event_id))
            .collect();
        self.pstate.scanout_m2.fresh_composed_unflip = true;
        log::debug!(
            "scanout_m2: submitted fresh atomic composed unflip outputs={output_count} reason={}",
            self.pstate.scanout_m2.unflip_reason.unwrap_or("unknown")
        );
        true
    }

    /// The grouped commit of [`Self::submit_fresh_composed_unflip`]: one
    /// `(framebuffer, IN_FENCE_FD)` per output, in output order.
    fn commit_fresh_composed_unflip(
        &mut self,
        targets: &[(::drm::control::framebuffer::Handle, i32)],
    ) -> io::Result<u64> {
        let planes: Vec<_> = self
            .platform
            .outputs
            .iter()
            .zip(targets)
            .map(
                |(layout, &(fb, in_fence_fd))| crate::drm::modeset::ComposedScanoutPlaneState {
                    output: &layout.output,
                    fb,
                    in_fence_fd: Some(in_fence_fd),
                },
            )
            .collect();
        let (device, atomic_commit_sink) = self
            .platform
            .primary_device()
            .map(|primary| {
                (
                    Rc::clone(&primary.device),
                    Rc::clone(&primary.atomic_commit_sink),
                )
            })
            .ok_or_else(|| io::Error::other("scanout M2: fresh unflip without a KMS device"))?;
        let output_indices: Vec<_> = (0..planes.len()).collect();
        let cursor_positions = self
            .platform
            .cursor_plane_atomic_positions_for_outputs(&output_indices);
        let result = crate::drm::modeset::submit_composed_scanout(
            &device,
            atomic_commit_sink.as_ref(),
            &planes,
            &cursor_positions,
        );
        if let Err(error) = &result
            && atomic_commit_sink.take_last_cursor_rejection()
        {
            let outcome = self
                .platform
                .reject_atomic_cursor_commit(&cursor_positions, &error.to_string());
            self.handle_cursor_move_outcome(outcome);
        }
        let accepted_event_id = result?;
        self.platform
            .cursor_plane_scene_commit_submitted(&cursor_positions);
        Ok(accepted_event_id)
    }

    /// Install only accepted per-output scene commits as degraded-unflip
    /// retirement obligations. Copied render readiness alone has no KMS ID.
    pub(in crate::kms::render::backend) fn record_accepted_degraded_unflip_replacements(&mut self) {
        if !self.pstate.scanout_m2.degraded_composed_unflip {
            return;
        }
        let outputs = self
            .pstate
            .scanout_m2
            .unflip_outputs_to_replace
            .iter()
            .copied()
            .collect::<Vec<_>>();
        for output_idx in outputs {
            if let Some(event_id) = self.scene.pending_page_flip_event_id(output_idx) {
                self.pstate
                    .scanout_m2
                    .unflip_awaiting_outputs
                    .insert(output_idx, event_id);
            }
        }
    }

    pub(in crate::kms::render::backend) fn retire_direct_output(
        &mut self,
        output_idx: usize,
        event_id: Option<u64>,
        clock: yserver_core::backend::PresentClockSample,
    ) -> bool {
        if let Some(&accepted_replacement_id) = self
            .pstate
            .scanout_m2
            .unflip_awaiting_outputs
            .get(&output_idx)
        {
            if event_id != Some(accepted_replacement_id) {
                return false;
            }
            self.pstate
                .scanout_m2
                .unflip_awaiting_outputs
                .remove(&output_idx);
            self.pstate
                .scanout_m2
                .unflip_outputs_to_replace
                .remove(&output_idx);
            let degraded = self.pstate.scanout_m2.degraded_composed_unflip;
            let scene_owned = degraded || self.pstate.scanout_m2.fresh_composed_unflip;
            let cursor_handoff = !scene_owned
                && self.pstate.scanout_m2.unflip_reason == Some("tearing_cursor_handoff");
            if cursor_handoff {
                self.scene
                    .retire_external_cursor_handoff(output_idx, &mut self.platform);
            }
            if self.pstate.scanout_m2.unflip_outputs_to_replace.is_empty() {
                self.stop_direct_after_scanout_replaced(if degraded {
                    "degraded composed unflip"
                } else {
                    "atomic composed unflip"
                });
                self.pstate.scanout_m2.reentry_blocked_until_composed = true;
                // Step 3 — while a CRTC scanned out a client buffer directly,
                // the composed BOs were not painted and the scene was not
                // tracking them; a direct retirement also bypasses
                // `handle_page_flip_complete` entirely. Treat every composed BO
                // as wholly stale on the way back.
                self.scene.invalidate_all_scanout_damage();
                self.scene.mark_scene_structure_dirty();
                self.pstate.scanout_m2.degraded_composed_unflip = false;
                log::debug!("scanout_m2: composed unflip retired on all outputs");
            }
            if scene_owned {
                // The planes were replaced by the scene's own composed
                // frames, which must still unwind their pending-acks and BO
                // state through the scene; hand the retire back.
                return false;
            }
            return true;
        }
        if self
            .pstate
            .scanout_m2
            .unflip_outputs_to_replace
            .contains(&output_idx)
        {
            return false;
        }
        let Some(pending) = self.pstate.scanout_m2.pending.as_mut() else {
            return false;
        };
        if pending.accepted_event_id != event_id {
            return false;
        }
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
                .pstate
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
            self.pstate
                .scanout_m2
                .completed
                .push(presented.event.clone());
            // Xorg publishes the retiring flip before re-executing any
            // flip-ready successor. Coalesced successors therefore become
            // ordered Skip completions only at this retirement boundary.
            self.pstate
                .scanout_m2
                .completed
                .append(&mut self.pstate.scanout_m2.deferred_successor_skips);
            if let Some(previous) = self.pstate.scanout_m2.current.replace(presented) {
                self.release_direct_frame(previous);
            }
            log::debug!(
                "scanout_m2: direct frame retired on all outputs source_id={}",
                self.pstate
                    .scanout_m2
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
                self.direct_cursor_eligible(),
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
        if self.pstate.scanout_m1.topology_signature != topology_signature {
            self.pstate.scanout_m1.clear("output topology changed");
            self.pstate.scanout_m1.topology_signature = topology_signature;
        }
        if self.pstate.scanout_m1.entries.contains_key(&source_id) {
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
            self.pstate
                .scanout_m1
                .entries
                .insert(source_id, ScanoutM1ProbeEntry::rejected());
            self.pstate.scanout_m0.m1_probe_reject =
                self.pstate.scanout_m0.m1_probe_reject.saturating_add(1);
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
            self.pstate.scanout_m0.m1_gate_reject_import = self
                .pstate
                .scanout_m0
                .m1_gate_reject_import
                .saturating_add(1);
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
            self.pstate
                .scanout_m1
                .entries
                .insert(source_id, ScanoutM1ProbeEntry::rejected());
            self.pstate.scanout_m0.m1_probe_reject =
                self.pstate.scanout_m0.m1_probe_reject.saturating_add(1);
            return;
        }
        let fd = match fd_result {
            Ok(fd) => fd,
            Err(error) => {
                log::warn!(
                    "scanout_m1: source_id={} dma-buf dup failed: {error}",
                    source_id.as_u64()
                );
                self.pstate
                    .scanout_m1
                    .entries
                    .insert(source_id, ScanoutM1ProbeEntry::rejected());
                self.pstate.scanout_m0.m1_probe_error =
                    self.pstate.scanout_m0.m1_probe_error.saturating_add(1);
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
            self.pstate
                .scanout_m1
                .entries
                .insert(source_id, ScanoutM1ProbeEntry::rejected());
            self.pstate.scanout_m0.m1_probe_reject =
                self.pstate.scanout_m0.m1_probe_reject.saturating_add(1);
            return;
        };
        let output_indices: Vec<_> = (0..self.platform.outputs.len()).collect();
        let cursor_positions = self
            .platform
            .cursor_plane_atomic_positions_for_outputs(&output_indices);
        let result = crate::drm::modeset::probe_direct_scanout_test_only(
            Rc::clone(&primary.device),
            fd.as_fd(),
            u32::from(width),
            u32::from(height),
            fourcc,
            modifier,
            offset,
            pitch,
            primary.atomic_commit_sink.as_ref(),
            &plane_states,
            &cursor_positions,
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
                let mut crtcs: Vec<_> = self
                    .platform
                    .outputs
                    .iter()
                    .map(|layout| layout.output.crtc)
                    .collect();
                crtcs.sort_by_key(|crtc| u32::from(*crtc));
                self.pstate.scanout_m1.entries.insert(
                    source_id,
                    ScanoutM1ProbeEntry::accepted_with_shape(
                        framebuffer,
                        crate::drm::async_flip::AsyncShape {
                            fourcc,
                            modifier,
                            pitch,
                            width: u32::from(width),
                            height: u32::from(height),
                            crtcs,
                            direct: true,
                        },
                    ),
                );
                self.pstate.scanout_m0.m1_probe_pass =
                    self.pstate.scanout_m0.m1_probe_pass.saturating_add(1);
            }
            Ok(crate::drm::modeset::DirectScanoutTestResult::Rejected(error)) => {
                log::debug!(
                    "scanout_m1: TEST_ONLY rejected source_id={} drawable_host={:#x}: {error}",
                    source_id.as_u64(),
                    candidate.src_host_xid,
                );
                self.pstate
                    .scanout_m1
                    .entries
                    .insert(source_id, ScanoutM1ProbeEntry::rejected());
                self.pstate.scanout_m0.m1_probe_reject =
                    self.pstate.scanout_m0.m1_probe_reject.saturating_add(1);
            }
            Err(error) => {
                log::warn!(
                    "scanout_m1: import/probe failed source_id={} drawable_host={:#x}: {error}",
                    source_id.as_u64(),
                    candidate.src_host_xid,
                );
                self.pstate
                    .scanout_m1
                    .entries
                    .insert(source_id, ScanoutM1ProbeEntry::rejected());
                self.pstate.scanout_m0.m1_probe_error =
                    self.pstate.scanout_m0.m1_probe_error.saturating_add(1);
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
        self.pstate
            .scanout_m2
            .pending
            .as_ref()
            .is_some_and(shaped_off)
            || self
                .pstate
                .scanout_m2
                .queued_successor
                .as_ref()
                .is_some_and(shaped_off)
            || self
                .pstate
                .scanout_m2
                .current
                .as_ref()
                .is_some_and(shaped_off)
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
        let cursor_hw = self.direct_cursor_eligible();
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
        let diag = &mut self.pstate.scanout_m0;
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
        self.pstate.vt_state.allows_scanout()
    }

    /// Whether cursor-only work is rate-limited for the currently visible
    /// software cursor, including the interval after a grouped cursor handoff
    /// was submitted but before its retirement reveals the software cursor.
    pub(in crate::kms::render::backend) fn tearing_cursor_compose_cap_applies(&self) -> bool {
        if !self.pstate.tearing.is_on() || self.effective_cursor_hidden() {
            return false;
        }

        matches!(
            self.scene.cursor_mode(),
            crate::kms::render::scene::CursorPlaneMode::Sw
        ) || (self.pstate.scanout_m2.unflip_reason == Some("tearing_cursor_handoff")
            && !self.pstate.scanout_m2.unflip_outputs_to_replace.is_empty())
    }

    /// Earliest time a cursor-only software-scene wake may be composed while
    /// tearing. Other scene damage is deliberately excluded from this gate.
    fn tearing_cursor_compose_deadline(
        &self,
        now: std::time::Instant,
    ) -> Option<std::time::Instant> {
        if !self.tearing_cursor_compose_cap_applies()
            || !self.scene.cursor_only_wake_pending()
            || self.store.has_pending_presentation_damage()
            || self.scene.owes_repaint()
        {
            return None;
        }

        let last_compose = self.pstate.tearing.last_cursor_compose?;
        let deadline = last_compose + self.pstate.tearing.cursor_compose_interval(&self.platform);
        (now < deadline).then_some(deadline)
    }

    pub(super) fn scene_wants_compose(&self) -> bool {
        if self.pstate.scanout_m2.pending.is_some()
            || !self.pstate.scanout_m2.unflip_awaiting_outputs.is_empty()
        {
            // The grouped direct or composed transaction's flip events are
            // the next progress source; its state will be re-arbitrated there.
            return false;
        }
        let outputs: Vec<_> = (0..self.platform.outputs.len()).collect();
        let direct_transaction_waiting = self.pstate.scanout_m2.pending.is_some()
            || self.pstate.scanout_m2.queued_successor.is_some()
            || self.pstate.scanout_m2.unflip_requested;
        let degraded_replacement_needs_submit = self.pstate.scanout_m2.degraded_composed_unflip
            && !self.pstate.scanout_m2.unflip_outputs_to_replace.is_empty();
        let unflip_waiting_for_cursor = self.pstate.scanout_m2.current.is_some()
            && self.pstate.scanout_m2.unflip_requested
            && self.platform.cursor_commit_in_flight_for_outputs(&outputs);
        if unflip_waiting_for_cursor {
            // The cursor flip event is the next progress source. Re-arming an
            // immediate scene wake here would spin while the grouped unflip is
            // correctly waiting for that event.
            return false;
        }
        let cursor_compose_blocked = self
            .tearing_cursor_compose_deadline(self.monotonic_now())
            .is_some();
        ((self.scene_wants_scene() || degraded_replacement_needs_submit) && !cursor_compose_blocked)
            || (!cursor_compose_blocked
                && !direct_transaction_waiting
                && self.platform.has_eligible_atomic_cursor_move())
    }

    #[cfg(test)]
    pub(crate) fn for_tests_scene_wants_compose(&self) -> bool {
        self.scene_wants_compose()
    }

    #[cfg(test)]
    pub(crate) fn for_tests_scene_wants_scene(&self) -> bool {
        self.scene_wants_scene()
    }

    #[cfg(test)]
    pub(crate) fn for_tests_tearing_cursor_compose_deadline_at(
        &self,
        now: std::time::Instant,
    ) -> Option<std::time::Instant> {
        self.tearing_cursor_compose_deadline(now)
    }
}

impl KmsBackend {
    pub(in crate::kms::render::backend) fn backend_scanout_on_scanout_render_completion(
        &mut self,
        _state: &mut ServerState,
    ) {
        let completions =
            crate::kms::render::scene::OutputSink::drain_render_completions(&mut self.platform);
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
            if self.platform.gpu.renderer_failed {
                break;
            }
            if !self
                .scene
                .handle_scanout_render_completion(completion, &mut self.platform)
            {
                self.telemetry.record_missed_pageflip();
            }
            self.record_accepted_degraded_unflip_replacements();
            if self.platform.gpu.renderer_failed {
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
            if let Ok((flips, sequences)) = self.platform.drain_page_flip_events(drm_fd) {
                for (output_idx, _, timestamps) in flips {
                    let matched_event_id = self
                        .platform
                        .note_tearing_async_flip_event(output_idx, timestamps);
                    if timestamps.commit_id.is_some() && matched_event_id.is_none() {
                        continue;
                    }
                    self.record_atomic_cursor_commit_retirement(output_idx, matched_event_id);
                }
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
        for (output_idx, clock, timestamps) in flipped {
            let matched_event_id = self
                .platform
                .note_tearing_async_flip_event(output_idx, timestamps);
            if timestamps.commit_id.is_some() && matched_event_id.is_none() {
                continue;
            }
            self.record_atomic_cursor_commit_retirement(output_idx, matched_event_id);
            let direct_retired = self.retire_direct_output(output_idx, matched_event_id, clock);
            let scene_retired = !direct_retired
                && self.scene.handle_page_flip_complete(
                    output_idx,
                    matched_event_id,
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
        self.engine.poll_retired(&self.platform.gpu);
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
            &mut self.platform.gpu,
            crate::kms::render::engine::RenderFlushReason::Present,
        ) {
            log::warn!("render on_page_flip_ready: flush_render_batch failed: {e:?}");
        }
        if let Err(e) = self.engine.flush_submit_group(
            &mut self.store,
            &mut self.platform.gpu,
            crate::kms::render::submit_group::FlushReason::PageflipRetire,
        ) {
            log::warn!("render on_page_flip_ready: flush_submit_group failed: {e:?}");
        }
    }

    fn record_atomic_cursor_commit_retirement(
        &mut self,
        output_idx: usize,
        matched_event_id: Option<u64>,
    ) {
        self.platform
            .retire_atomic_cursor_commit_for_output(output_idx, matched_event_id);
        let Some(layout) = self.platform.outputs.get(output_idx) else {
            return;
        };
        if let Some(device) = self.platform.device_for_output(&layout.key) {
            device
                .atomic_commit_sink
                .note_cursor_commit_retired(layout.output.crtc, std::time::Instant::now());
        }
    }

    fn prepare_tearing_scene_pass(&mut self) {
        if std::mem::take(&mut self.platform.scene_cursor_hide_failed)
            && self.pstate.tearing.is_on()
            && !self.pstate.tearing.hide_failed_logged
            && self.platform.any_cursor_plane_visible()
        {
            log::warn!("kms: tearing cursor-plane hide failed; keeping composed flips on vsync");
            self.pstate.tearing.hide_failed_logged = true;
        }

        if !self.pstate.tearing.is_on() {
            return;
        }
        if !self.tearing_scene_still_owned() {
            self.exit_tearing("eligibility_lost");
            return;
        }
        if self.tearing_cursor_wants_visible() && self.pstate.scanout_m2.active() {
            self.request_direct_unflip("tearing_cursor_handoff");
        }
        if self.platform.any_cursor_plane_visible() {
            return;
        }
        for output_idx in 0..self.platform.outputs.len() {
            let Some(shape) = self.platform.scene_async_shape(output_idx) else {
                continue;
            };
            let stream = crate::drm::async_flip::AsyncStream::Composed(
                self.platform.outputs[output_idx].output.crtc,
            );
            if self.platform.async_ledger.allows(stream, &shape) {
                self.platform
                    .set_scene_flip_timing(output_idx, crate::drm::async_flip::FlipTiming::Async);
            }
        }
    }

    pub(in crate::kms::render::backend) fn backend_scanout_next_wakeup(
        &self,
    ) -> Option<std::time::Instant> {
        let now = self.monotonic_now();
        let allow_kms_timers = self.scanout_allowed() && self.kms_outputs_active;
        let scene_deadline = if allow_kms_timers {
            if self.scene_wants_compose() {
                if self.scene.has_output_ready_for_submit(&self.platform) {
                    Some(now)
                } else {
                    self.scene.earliest_retry_deadline()
                }
            } else {
                self.tearing_cursor_compose_deadline(now)
                    .or_else(|| self.scene.earliest_retry_deadline())
            }
        } else {
            None
        };
        let needs_source_wait_poll =
            self.pstate
                .pending_present_source_waits
                .values()
                .any(|wait| {
                    !wait.ready_reported
                        && (wait.poll_timeline
                            || wait.fds.iter().any(|fd| !fd.registered && !fd.ready))
                });
        let source_wait_deadline =
            needs_source_wait_poll.then(|| now + std::time::Duration::from_millis(1));
        let rescan_deadline = self
            .pstate
            .hotplug_rescan_deadline
            .map(|until| if now >= until { now } else { until });
        scene_deadline
            .into_iter()
            .chain(source_wait_deadline)
            .chain(rescan_deadline)
            // Not gated on `allow_kms_timers`: `maybe_composite` closes the
            // paint frame on its timeout while dark too (#177).
            .chain(self.gpu_work_deadline(now))
            .chain(
                allow_kms_timers
                    .then(|| self.cursor_anim_deadline())
                    .flatten(),
            )
            .chain(
                allow_kms_timers
                    .then_some(self.pstate.tearing.deferred_cursor_wake)
                    .flatten(),
            )
            .min()
    }

    pub(in crate::kms::render::backend) fn backend_scanout_maybe_composite(
        &mut self,
    ) -> io::Result<()> {
        for output_idx in 0..self.platform.outputs.len() {
            self.platform
                .set_scene_flip_timing(output_idx, crate::drm::async_flip::FlipTiming::Vsync);
        }
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
        if self.platform.gpu.renderer_failed {
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
            .close_open_frame_if_timed_out(&mut self.store, &mut self.platform.gpu)
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
        if self.pstate.tearing.is_on()
            && self
                .pstate
                .tearing
                .deferred_cursor_wake
                .is_some_and(|deadline| self.monotonic_now() >= deadline)
        {
            self.pstate.tearing.deferred_cursor_wake = None;
            if !self.scene.scene_structure_dirty {
                self.scene.wake_for_cursor_damage();
            }
        }
        // Animated-cursor frame advance — after both gates above so
        // DPMS-off / VT-away never uploads (spec "DPMS / VT gating").
        self.tick_cursor_animation();
        // Check visibility and current scene ownership before the held-direct
        // return below. Pointer motion from a dead zone wakes this pass.
        self.prepare_tearing_scene_pass();
        // Arbitration step 1: a direct successor retained behind a cursor-only
        // commit gets the first opportunity once all of its CRTCs are clear.
        // The helper leaves it in the slot while any CRTC is still blocked.
        self.submit_queued_direct_successor();
        if self.pstate.scanout_m2.active() {
            use crate::kms::render::scene::CursorPlaneMode;
            let cursor_mode = self.scene.cursor_mode();
            if self.platform.any_output_transformed() {
                self.request_direct_unflip("composite_tick_crtc_transform");
            } else if !matches!(cursor_mode, CursorPlaneMode::Hw | CursorPlaneMode::Hidden)
                || !self.scene.root_overlay.is_empty()
            {
                let reason = match cursor_mode {
                    CursorPlaneMode::Mixed => "composite_tick_mixed_cursor",
                    CursorPlaneMode::Sw => "composite_tick_software_or_hidden_cursor",
                    CursorPlaneMode::Hw | CursorPlaneMode::Hidden => "composite_tick_root_overlay",
                };
                self.request_direct_unflip(reason);
            }
            // Never race a composed commit against the all-output direct
            // transaction. A requested unflip starts on the first tick after
            // the direct transaction itself has fully retired.
            if self.pstate.scanout_m2.pending.is_some()
                || !self.pstate.scanout_m2.unflip_awaiting_outputs.is_empty()
            {
                self.drain_render_telemetry();
                self.telemetry.maybe_emit(self.engine.pending_count());
                return Ok(());
            }
            if self.pstate.scanout_m2.hold_direct && !self.pstate.scanout_m2.unflip_requested {
                // A fully-retired direct frame may continue receiving cursor
                // motion. Let the scheduling pass send cursor-only commits
                // while there is no direct successor waiting; once a
                // successor is retained, its grouped direct transaction gets
                // every later position and no CRTC is extended by another
                // cursor-only commit.
                if self.pstate.scanout_m2.current.is_some()
                    && self.pstate.scanout_m2.queued_successor.is_none()
                {
                    for output_idx in 0..self.platform.outputs.len() {
                        match self
                            .platform
                            .submit_atomic_cursor_move_for_output(output_idx)
                        {
                            Ok(outcome) => self.handle_cursor_move_outcome(outcome),
                            Err(error) => log::warn!(
                                "render maybe_composite: cursor-only atomic commit failed for output {output_idx}: {error}"
                            ),
                        }
                    }
                }
                self.drain_render_telemetry();
                self.telemetry.maybe_emit(self.engine.pending_count());
                return Ok(());
            }
            if self.pstate.scanout_m2.current.is_some()
                && self.pstate.scanout_m2.unflip_requested
                && !self.pstate.scanout_m2.degraded_composed_unflip
            {
                let submitted = if self.submit_fresh_composed_unflip() {
                    Ok(true)
                } else {
                    self.submit_composed_unflip()
                };
                match submitted {
                    Ok(true) => {
                        self.pstate
                            .tearing
                            .note_composed_submit(self.monotonic_now());
                        // The atomic replacement itself is now pending on every
                        // CRTC. Do not fall through into per-output scene flips in
                        // this same tick: KMS correctly rejects those with EBUSY.
                        self.drain_render_telemetry();
                        self.telemetry.maybe_emit(self.engine.pending_count());
                        return Ok(());
                    }
                    Ok(false) => {
                        // A cursor-only commit still owns one of these CRTCs.
                        // Keep direct scanout active and retry after retirement.
                        self.drain_render_telemetry();
                        self.telemetry.maybe_emit(self.engine.pending_count());
                        return Ok(());
                    }
                    Err(error) => {
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
                        self.pstate.scanout_m2.unflip_outputs_to_replace =
                            (0..self.platform.outputs.len()).collect();
                        self.pstate.scanout_m2.unflip_awaiting_outputs.clear();
                        self.pstate.scanout_m2.degraded_composed_unflip = true;
                        self.scene.mark_scene_structure_dirty();
                    }
                }
            }
        }
        if self.pstate.scanout_m2.degraded_composed_unflip
            && !self.pstate.scanout_m2.unflip_outputs_to_replace.is_empty()
        {
            // A failed per-output replacement can leave the scene clean after
            // another output's accepted frame retires. Keep the scene tick
            // eligible until every direct output has an accepted replacement;
            // its normal readiness and retry gates still control submission.
            self.scene.mark_scene_structure_dirty();
        }
        let composed_outputs = if self
            .tearing_cursor_compose_deadline(self.monotonic_now())
            .is_some()
        {
            None
        } else {
            self.compose_tick()
        };
        // `scene.tick` drains already-signalled copied completions itself,
        // without entering backend_scanout_on_scanout_render_completion.
        self.record_accepted_degraded_unflip_replacements();
        if let Some(composed) = &composed_outputs
            && self.pstate.scanout_m2.reentry_blocked_until_composed
            && composed.len() == self.platform.outputs.len()
        {
            self.pstate.scanout_m2.reentry_blocked_until_composed = false;
            log::debug!("scanout_m2: composed fallback submitted; re-entry barrier cleared");
        }
        if composed_outputs
            .as_ref()
            .is_some_and(|outputs| !outputs.is_empty())
        {
            self.pstate
                .tearing
                .note_composed_submit(self.monotonic_now());
        }
        let scene_submitted_outputs: HashSet<usize> =
            composed_outputs.into_iter().flatten().collect();
        let cursor_outcome = self.platform.take_deferred_cursor_move_outcome();
        self.handle_cursor_move_outcome(cursor_outcome);
        for output_idx in 0..self.platform.outputs.len() {
            if scene_submitted_outputs.contains(&output_idx)
                || self.scene.has_pending_page_flip(output_idx)
                || self.scene.output_submit_retry_pending(output_idx)
                || (self.scene_wants_scene()
                    && self
                        .scene
                        .output_ready_for_submit(output_idx, &self.platform))
            {
                continue;
            }
            match self
                .platform
                .submit_atomic_cursor_move_for_output(output_idx)
            {
                Ok(outcome) => self.handle_cursor_move_outcome(outcome),
                Err(error) => log::warn!(
                    "render maybe_composite: cursor-only atomic commit failed for output {output_idx}: {error}"
                ),
            }
        }
        self.drain_paint_submit_telemetry();
        Ok(())
    }
}
