use crate::kms::render::{
    backend::{
        DirectFrameSlot, DirectPresentFrame, DirectScope, KmsBackend, ScanoutM0Coverage,
        ScanoutM0Shape, ScanoutM0Target, ScanoutM1DescriptorError, ScanoutM1OutputGeometry,
        ScanoutM1ProbeEntry, ScanoutM1Reject, ScanoutSourceDescriptor, ScanoutSourceLayout,
        WindowsMap, kms::randr::effective_refresh_matches,
    },
    scene::CursorUploadOutcome,
    store::DrawableId,
    target::Src,
};
use ash::vk;
use std::{
    collections::{HashMap, HashSet},
    io,
    rc::Rc,
};
use yserver_core::{
    backend::{Backend, PresentScanoutCandidate},
    server::ServerState,
};
use yserver_protocol::x11::xfixes;

const DIRECT_CURSOR_RETRY_AFTER_SUCCESSOR_EVENTS: u8 = 2;
const DIRECT_CURSOR_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(50);

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

/// Returns the leaf-to-top-level chain with absolute content origins, or `None` if unresolved.
fn direct_shape_chain_root_offsets(
    windows: &WindowsMap,
    root_window_id: u32,
    leaf_xid: u32,
) -> Option<Vec<(u32, i32, i32)>> {
    let mut chain = Vec::new();
    let mut xid = leaf_xid;
    // Resource validation prevents cycles in production; stay bounded anyway.
    for _ in 0..=windows.len() {
        let geometry = windows.get(&xid)?;
        let bw = i32::from(geometry.border_width);
        chain.push((xid, i32::from(geometry.x) + bw, i32::from(geometry.y) + bw));
        match geometry.parent {
            None => break,
            Some(parent) if parent == root_window_id => break,
            Some(parent) => xid = parent,
        }
    }

    let (mut abs_x, mut abs_y) = (0, 0);
    for (_, x, y) in chain.iter_mut().rev() {
        abs_x += *x;
        abs_y += *y;
        *x = abs_x;
        *y = abs_y;
    }
    Some(chain)
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
    root_rect: vk::Rect2D,
) -> bool {
    use crate::kms::render::region::Region;

    let Some(chain) = direct_shape_chain_root_offsets(windows, root_window_id, leaf_xid) else {
        return false;
    };
    for &(xid, abs_x, abs_y) in chain.iter().rev() {
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

fn direct_shape_chain_hides_root(
    windows: &WindowsMap,
    root_window_id: u32,
    shape_bounding: &HashMap<u32, Vec<xfixes::RegionRect>>,
    shape_clip: &HashMap<u32, Vec<xfixes::RegionRect>>,
    leaf_xid: u32,
    root_rect: vk::Rect2D,
) -> bool {
    use crate::kms::render::region::Region;

    let Some(chain) = direct_shape_chain_root_offsets(windows, root_window_id, leaf_xid) else {
        return false;
    };
    let root_region = Region::from_rect(root_rect);
    for &(xid, abs_x, abs_y) in chain.iter().rev() {
        for shapes in [shape_bounding, shape_clip] {
            let Some(rects) = shapes.get(&xid) else {
                continue;
            };
            if rects.iter().all(|rect| {
                !root_region.intersects_rect(vk::Rect2D {
                    offset: vk::Offset2D {
                        x: abs_x + i32::from(rect.x),
                        y: abs_y + i32::from(rect.y),
                    },
                    extent: vk::Extent2D {
                        width: u32::from(rect.width),
                        height: u32::from(rect.height),
                    },
                })
            }) {
                return true;
            }
        }
    }
    false
}

/// Decide, per output, whether a CRTC is displaying a directly-flipped client
/// buffer and which retained frame it is — or the compositor's own scanout BO.
///
/// A root-scoped direct transaction is submitted to every CRTC at once, while
/// an output-scoped transaction touches one CRTC. Retirement is per-CRTC in
/// both cases, so a grouped root transaction can temporarily leave outputs in
/// different states:
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
        && matches!(
            coverage,
            ScanoutM0Coverage::Root | ScanoutM0Coverage::Output(_)
        )
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
            || !scanout_m1_output_is_unoverlapped(index, outputs)
        {
            return false;
        }
        let x = u32::try_from(output.x).expect("non-negative checked above");
        let y = u32::try_from(output.y).expect("non-negative checked above");
        if x.saturating_add(output.width) > root.0 || y.saturating_add(output.height) > root.1 {
            return false;
        }
        area = area.saturating_add(u64::from(output.width) * u64::from(output.height));
    }
    area == u64::from(root.0) * u64::from(root.1)
}

/// Per-output geometry shared by Root tiling and Output admission. Output
/// scope uses the selected mode-size and separation checks without requiring
/// the other monitors to tile the virtual root.
pub(in crate::kms::render::backend) fn scanout_m1_output_is_unoverlapped(
    output_idx: usize,
    outputs: &[ScanoutM1OutputGeometry],
) -> bool {
    let Some(output) = outputs.get(output_idx) else {
        return false;
    };
    if output.width == 0
        || output.height == 0
        || output.width != output.mode_width
        || output.height != output.mode_height
    {
        return false;
    }
    let x0 = i64::from(output.x);
    let y0 = i64::from(output.y);
    let x1 = x0 + i64::from(output.width);
    let y1 = y0 + i64::from(output.height);
    outputs.iter().enumerate().all(|(other_idx, other)| {
        if other_idx == output_idx {
            return true;
        }
        if other.width == 0 || other.height == 0 {
            return false;
        }
        let other_x0 = i64::from(other.x);
        let other_y0 = i64::from(other.y);
        let other_x1 = other_x0 + i64::from(other.width);
        let other_y1 = other_y0 + i64::from(other.height);
        x1 <= other_x0 || other_x1 <= x0 || y1 <= other_y0 || other_y1 <= y0
    })
}

pub(in crate::kms::render::backend) const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258;

/// One full-output primary-plane state per output of `output_indices`. Root
/// sources each output's rectangle of a root-sized buffer, Output the whole
/// output-sized buffer.
pub(in crate::kms::render::backend) fn scanout_m1_plane_states<'a>(
    outputs: &'a [crate::kms::backend::ActiveOutput],
    output_indices: &[usize],
    scope: DirectScope,
) -> Vec<crate::drm::modeset::DirectScanoutPlaneState<'a>> {
    output_indices
        .iter()
        .map(|&output_idx| {
            let layout = &outputs[output_idx];
            let (src_x, src_y) = match scope {
                DirectScope::Root => (
                    u32::try_from(layout.x).expect("root geometry validated non-negative"),
                    u32::try_from(layout.y).expect("root geometry validated non-negative"),
                ),
                DirectScope::Output(_) => (0, 0),
            };
            crate::drm::modeset::DirectScanoutPlaneState {
                output: &layout.output,
                src_x,
                src_y,
                src_w: u32::from(layout.width),
                src_h: u32::from(layout.height),
                in_fence_fd: None,
            }
        })
        .collect()
}

/// M1's format and size checks for one source, before any `AddFB2`.
/// `expected` is the scope's size (root or output); `output_modifiers`
/// holds each scanned-out output's modifiers, where empty means unknown and
/// leaves the decision to `TEST_ONLY`.
pub(in crate::kms::render::backend) fn scanout_m1_validate(
    layout: &ScanoutSourceLayout,
    expected: (u32, u32),
    output_modifiers: &[&[u64]],
) -> Result<(), ScanoutM1Reject> {
    if layout.fourcc != DRM_FORMAT_XRGB8888 {
        return Err(ScanoutM1Reject::Fourcc);
    }
    if layout.vk_format != vk::Format::B8G8R8A8_UNORM {
        return Err(ScanoutM1Reject::VkFormat);
    }
    if layout.plane_count != 1 {
        return Err(ScanoutM1Reject::PlaneCount);
    }
    if layout.depth != 24 {
        return Err(ScanoutM1Reject::Depth);
    }
    if layout.bpp != 32 {
        return Err(ScanoutM1Reject::Bpp);
    }
    if layout.pitch == 0 {
        return Err(ScanoutM1Reject::ZeroPitch);
    }
    if (u32::from(layout.width), u32::from(layout.height)) != expected {
        return Err(ScanoutM1Reject::ScopeSizeMismatch);
    }
    if output_modifiers
        .iter()
        .any(|modifiers| !modifiers.is_empty() && !modifiers.contains(&layout.modifier))
    {
        return Err(ScanoutM1Reject::Modifier);
    }
    Ok(())
}

impl KmsBackend {
    pub(in crate::kms::render::backend) fn direct_scope_outputs(
        &self,
        scope: DirectScope,
    ) -> Vec<usize> {
        match scope {
            DirectScope::Root => (0..self.platform.outputs.len()).collect(),
            DirectScope::Output(output_idx) => vec![output_idx],
        }
    }

    pub(in crate::kms::render::backend) fn direct_root_overlay_empty_for_outputs(
        &self,
        output_indices: &[usize],
    ) -> bool {
        output_indices.iter().all(|&output_idx| {
            self.platform.outputs.get(output_idx).is_some_and(|output| {
                self.scene
                    .root_overlay
                    .apply_list_for_output((
                        output.x,
                        output.y,
                        u32::from(output.width),
                        u32::from(output.height),
                    ))
                    .is_empty()
            })
        })
    }

    pub(in crate::kms::render::backend) fn output_is_direct(&self, output_idx: usize) -> bool {
        if output_idx >= self.platform.outputs.len() {
            return false;
        }
        self.pstate
            .direct
            .scope_for_output(output_idx)
            .and_then(|scope| self.pstate.direct.get(scope))
            .is_some_and(|owner| {
                owner.active()
                    || owner.unflip_outputs_to_replace.contains(&output_idx)
                    || owner.unflip_awaiting_outputs.contains_key(&output_idx)
            })
    }

    fn output_has_degraded_unflip(&self, output_idx: usize) -> bool {
        self.pstate
            .direct
            .scope_for_output(output_idx)
            .and_then(|scope| self.pstate.direct.get(scope))
            .is_some_and(|owner| {
                owner.degraded_composed_unflip
                    && owner.unflip_outputs_to_replace.contains(&output_idx)
            })
    }

    fn scene_skip_outputs(&self) -> Vec<bool> {
        (0..self.platform.outputs.len())
            .map(|output_idx| !self.scene_output_ready_for_scheduled_compose(output_idx))
            .collect()
    }

    fn scene_output_ready_for_scheduled_compose(&self, output_idx: usize) -> bool {
        !self.output_is_direct(output_idx) || self.output_has_degraded_unflip(output_idx)
    }

    fn scene_output_wants_scheduled_compose(&self, output_idx: usize) -> bool {
        self.output_has_degraded_unflip(output_idx)
            || self.scene_wants_scene() && self.scene.output_wants_compose(output_idx, &self.store)
    }

    fn scene_output_can_accept_scheduled_frame(&self, output_idx: usize) -> bool {
        if self.scene.is_live() {
            self.scene
                .output_ready_for_submit(output_idx, &self.platform)
        } else {
            self.scene.has_output_ready_for_submit(&self.platform)
        }
    }

    fn scene_has_ready_scheduled_output(&self) -> bool {
        (0..self.platform.outputs.len()).any(|output_idx| {
            self.scene_output_ready_for_scheduled_compose(output_idx)
                && self.scene_output_wants_scheduled_compose(output_idx)
                && self.scene_output_can_accept_scheduled_frame(output_idx)
        })
    }

    fn scheduled_scene_retry_deadline(&self) -> Option<std::time::Instant> {
        self.scene
            .earliest_retry_deadline_for_outputs(&self.scene_skip_outputs())
    }

    fn scene_cursor_eligible_outputs(&self) -> Vec<bool> {
        (0..self.platform.outputs.len())
            .map(|output_idx| {
                !self
                    .pstate
                    .direct
                    .scope_for_output(output_idx)
                    .and_then(|scope| self.pstate.direct.get(scope))
                    .is_some_and(|owner| {
                        owner.pending.is_some()
                            || owner.queued_successor.is_some()
                            || owner.unflip_requested
                    })
            })
            .collect()
    }

    pub(in crate::kms::render::backend) fn monotonic_now(&self) -> std::time::Instant {
        (self.pstate.monotonic_clock)()
    }

    /// Keep generic atomic cursor failures pending, but pace retries through
    /// the backend deadline and cap them with the existing per-CRTC recovery.
    pub(super) fn schedule_direct_cursor_retry(
        &mut self,
        scope: DirectScope,
        output_idx: usize,
        operation: &str,
        error: &io::Error,
        unflip_reason: &'static str,
    ) -> bool {
        if let Some(owner) = self.pstate.direct.get_mut(scope) {
            owner.mark_direct_cursor_refresh_pending(output_idx);
        }
        let deadline = self.monotonic_now() + DIRECT_CURSOR_RETRY_DELAY;
        let attempts = self
            .platform
            .note_direct_cursor_retry_failure(output_idx, deadline)
            .unwrap_or(crate::kms::render::platform::CURSOR_HIDE_RECOVERY_MAX_ATTEMPTS);
        let max_attempts = crate::kms::render::platform::CURSOR_HIDE_RECOVERY_MAX_ATTEMPTS;
        if attempts >= max_attempts {
            log::warn!(
                "scanout_m2: direct cursor {operation} failed {attempts}/{max_attempts} times ({error}); unflipping"
            );
            self.request_direct_unflip_for(scope, unflip_reason);
            false
        } else {
            log::warn!(
                "scanout_m2: direct cursor {operation} failed ({error}); retry {attempts}/{max_attempts} at {deadline:?}"
            );
            true
        }
    }

    /// The direct scanout cursor gate depends on KMS plane visibility, so it
    /// stays with the KMS implementation while cursor image policy remains
    /// portable.
    pub(in crate::kms::render::backend) fn direct_cursor_eligible_for(
        &self,
        output_indices: &[usize],
    ) -> bool {
        if self.effective_cursor_hidden() {
            return output_indices.iter().all(|&output_idx| {
                !self.platform.cursor_plane_visible_for_output(output_idx)
                    || self
                        .pstate
                        .direct
                        .scope_for_output(output_idx)
                        .and_then(|scope| self.pstate.direct.get(scope))
                        .is_some_and(|owner| {
                            owner.direct_cursor_refresh_pending.contains(&output_idx)
                        })
            });
        }
        matches!(
            self.scene.cursor_mode_for_outputs(output_indices),
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
            let affected_outputs: Vec<_> = self
                .platform
                .outputs
                .iter()
                .enumerate()
                .filter_map(|(output_idx, layout)| {
                    outcome
                        .affected_crtcs
                        .contains(&crate::kms::render::platform::CrtcKey::for_output(layout))
                        .then_some(output_idx)
                })
                .collect();
            self.request_direct_unflip_for_outputs(
                &affected_outputs,
                "cursor_move_fallback_or_retry",
            );
        }
    }

    /// Whether every frame direct scanout holds (on screen, submitted, or
    /// queued) is the composite overlay window or a descendant of it.
    /// `false` when there are none, or when any is an ordinary window.
    pub(in crate::kms::render::backend) fn direct_frames_are_under_cow(&self) -> bool {
        let mut any = false;
        for scope in self.pstate.direct.active_scopes() {
            let Some(owner) = self.pstate.direct.get(scope) else {
                continue;
            };
            for frame in [
                owner.current.as_ref(),
                owner.pending.as_ref(),
                owner.queued_successor.as_ref(),
            ]
            .into_iter()
            .flatten()
            {
                any = true;
                if !matches!(
                    self.scanout_m0_target(frame.candidate.paint_dst_host_xid, None, None),
                    ScanoutM0Target::Cow | ScanoutM0Target::CowDescendant
                ) {
                    return false;
                }
            }
        }
        any
    }

    pub(in crate::kms::render::backend) fn request_direct_unflip(&mut self, reason: &'static str) {
        self.pstate.request_direct_unflip(reason);
    }

    pub(in crate::kms::render::backend) fn request_direct_unflip_for_outputs(
        &mut self,
        output_indices: &[usize],
        reason: &'static str,
    ) {
        let mut scopes = Vec::new();
        for &output_idx in output_indices {
            if let Some(scope) = self.pstate.direct.scope_for_output(output_idx)
                && !scopes.contains(&scope)
            {
                scopes.push(scope);
            }
        }
        for scope in scopes {
            if self
                .pstate
                .direct
                .get(scope)
                .is_some_and(|owner| owner.active())
            {
                self.request_direct_unflip_for(scope, reason);
            }
        }
    }

    pub(in crate::kms::render::backend) fn request_direct_unflip_for(
        &mut self,
        scope: DirectScope,
        reason: &'static str,
    ) {
        self.pstate.request_direct_unflip_for(scope, reason);
    }

    pub(in crate::kms::render::backend) fn direct_scopes_referencing_host_drawable(
        &self,
        host_xid: u32,
    ) -> Vec<DirectScope> {
        let drawable_id = self.store.lookup(host_xid);
        let references_drawable = |frame: &DirectPresentFrame| {
            frame.candidate.paint_dst_host_xid == host_xid
                || drawable_id.is_some_and(|id| {
                    frame.source_id == id || frame.fallback_target.backing_id() == id
                })
        };
        self.pstate
            .direct
            .active_scopes()
            .into_iter()
            .filter(|&scope| {
                let Some(owner) = self.pstate.direct.get(scope) else {
                    return false;
                };
                [
                    owner.pending.as_ref(),
                    owner.queued_successor.as_ref(),
                    owner.current.as_ref(),
                ]
                .into_iter()
                .flatten()
                .any(|frame| {
                    if references_drawable(frame) {
                        return true;
                    }
                    if !matches!(scope, DirectScope::Output(_)) {
                        return false;
                    }
                    direct_shape_chain_root_offsets(
                        &self.windows,
                        self.core.window_id,
                        frame.candidate.paint_dst_host_xid,
                    )
                    .is_none_or(|chain| chain.iter().any(|(xid, _, _)| *xid == host_xid))
                })
            })
            .collect()
    }

    pub(in crate::kms::render::backend) fn request_direct_unflip_for_host_drawable(
        &mut self,
        host_xid: u32,
        reason: &'static str,
    ) {
        for scope in self.direct_scopes_referencing_host_drawable(host_xid) {
            self.request_direct_unflip_for(scope, reason);
        }
    }

    /// Keep the scene's per-output cursor mode aligned with cursor planes
    /// changed outside a composed scene retirement.
    pub(super) fn sync_direct_cursor_scene_state(&mut self, output_indices: &[usize]) -> bool {
        let visible_by_output: Vec<_> = (0..self.platform.outputs.len())
            .map(|output_idx| self.platform.cursor_plane_visible_for_output(output_idx))
            .collect();
        self.scene
            .record_external_cursor_plane_bindings(&visible_by_output, output_indices)
    }

    fn bind_direct_cursor_on_outputs(&mut self, scope: DirectScope) {
        if self
            .pstate
            .direct
            .get(scope)
            .is_some_and(|owner| owner.cursor_bound_all)
        {
            return;
        }
        let output_indices = self.direct_scope_outputs(scope);
        let tearing = scope == DirectScope::Root && self.pstate.tearing.is_on();
        if tearing || self.effective_cursor_hidden() {
            // Tearing keeps every cursor plane detached. A hidden effective
            // cursor also must not leave a stale sprite bound over direct scanout.
            let mut failed = false;
            for &output_idx in &output_indices {
                if !self.platform.cursor_plane_visible_for_output(output_idx) {
                    self.platform.clear_direct_cursor_retry(output_idx);
                    if let Some(owner) = self.pstate.direct.get_mut(scope) {
                        owner.clear_direct_cursor_refresh_pending(output_idx);
                    }
                    continue;
                }
                let atomic_path = self
                    .platform
                    .device_for_output(&self.platform.outputs[output_idx].key)
                    .is_some_and(|device| {
                        device.cursor.state_path()
                            == crate::kms::cursor_plane::topology::CursorStatePath::Atomic
                    });
                match self.platform.cursor_plane_hide_on_crtc(output_idx) {
                    Ok(()) => {
                        self.platform.clear_direct_cursor_retry(output_idx);
                        if let Some(owner) = self.pstate.direct.get_mut(scope) {
                            owner.clear_direct_cursor_refresh_pending(output_idx);
                        }
                    }
                    Err(error)
                        if atomic_path
                            && (error.kind() == io::ErrorKind::WouldBlock
                                || error.raw_os_error() == Some(libc::EBUSY)) =>
                    {
                        self.platform.clear_direct_cursor_retry(output_idx);
                        if let Some(owner) = self.pstate.direct.get_mut(scope) {
                            owner.mark_direct_cursor_refresh_pending(output_idx);
                        }
                        log::debug!(
                            "scanout_m2: cursor hide deferred on output {output_idx}; retrying on CRTC event"
                        );
                    }
                    Err(error) if error.raw_os_error() == Some(libc::EINVAL) => {
                        failed = true;
                        log::warn!(
                            "scanout_m2: cursor hide failed on output {output_idx}: {error}; unflipping"
                        );
                    }
                    Err(error) => {
                        if !self.schedule_direct_cursor_retry(
                            scope,
                            output_idx,
                            "hide",
                            &error,
                            "cursor_hide_failed",
                        ) {
                            failed = true;
                        }
                    }
                }
            }
            let scene_synced = self.sync_direct_cursor_scene_state(&output_indices);
            if failed || !scene_synced {
                self.request_direct_unflip_for(scope, "cursor_hide_failed");
            } else if self
                .pstate
                .direct
                .get(scope)
                .expect("direct cursor owner exists")
                .direct_cursor_refresh_pending
                .is_empty()
                && let Some(owner) = self.pstate.direct.get_mut(scope)
            {
                owner.clear_all_direct_cursor_refresh_pending();
                owner.cursor_bound_all = true;
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
        for &output_idx in &output_indices {
            match self
                .platform
                .cursor_plane_show_on_crtc(output_idx, hot_x, hot_y, x, y)
            {
                Ok(()) => {
                    self.platform.clear_direct_cursor_retry(output_idx);
                    if let Some(owner) = self.pstate.direct.get_mut(scope) {
                        owner.clear_direct_cursor_refresh_pending(output_idx);
                    }
                }
                Err(error)
                    if error.is_deferred()
                        || error.operation_error().raw_os_error() == Some(libc::EBUSY) =>
                {
                    self.platform.clear_direct_cursor_retry(output_idx);
                    if let Some(owner) = self.pstate.direct.get_mut(scope) {
                        owner.mark_direct_cursor_refresh_pending(output_idx);
                    }
                    log::debug!(
                        "scanout_m2: direct cursor bind deferred on output {output_idx}; retrying on CRTC event"
                    );
                }
                Err(error) => {
                    if error.operation_error().raw_os_error() == Some(libc::EINVAL) {
                        failed = true;
                        log::warn!(
                            "scanout_m2: cursor bind failed on output {output_idx}: {error}; unflipping"
                        );
                    } else {
                        if !self.schedule_direct_cursor_retry(
                            scope,
                            output_idx,
                            "show",
                            error.operation_error(),
                            "cursor_bind_failed",
                        ) {
                            failed = true;
                        }
                    }
                }
            }
        }
        let scene_synced = self.sync_direct_cursor_scene_state(&output_indices);
        if failed || !scene_synced {
            self.request_direct_unflip_for(scope, "cursor_bind_failed");
        } else if let Some(owner) = self.pstate.direct.get_mut(scope) {
            owner.cursor_bound_all = true;
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
    ) -> Vec<usize> {
        if self.pstate.direct.active_scopes().is_empty() || self.platform.outputs.is_empty() {
            return Vec::new();
        }
        #[allow(clippy::cast_possible_truncation)]
        let x = self.core.cursor_x as i32;
        #[allow(clippy::cast_possible_truncation)]
        let y = self.core.cursor_y as i32;
        let mut refreshed_outputs = Vec::new();
        for scope in self.pstate.direct.active_scopes() {
            let scope_outputs = self.direct_scope_outputs(scope);
            if scope == DirectScope::Root && self.pstate.tearing.is_on() {
                if let Some(owner) = self.pstate.direct.get_mut(scope) {
                    owner.cursor_bound_all = false;
                }
                continue;
            }
            let mut succeeded = true;
            for &output_idx in &scope_outputs {
                if !self.refresh_direct_cursor_on_output(scope, output_idx, record, x, y) {
                    succeeded = false;
                    break;
                }
            }
            if succeeded && !self.sync_direct_cursor_scene_state(&scope_outputs) {
                succeeded = false;
            }
            if !succeeded {
                self.request_direct_unflip_for(scope, "direct_cursor_refresh_failed");
            }
            if let Some(owner) = self.pstate.direct.get_mut(scope) {
                owner.cursor_bound_all = succeeded;
            }
            if succeeded {
                refreshed_outputs.extend(scope_outputs);
            }
        }
        refreshed_outputs.sort_unstable();
        refreshed_outputs.dedup();
        refreshed_outputs
    }

    fn refresh_direct_cursor_on_output(
        &mut self,
        scope: DirectScope,
        output_idx: usize,
        record: &crate::kms::render::cursor::CursorRecord,
        x: i32,
        y: i32,
    ) -> bool {
        match self.platform.cursor_plane_upload_image_for_output(
            output_idx,
            record.version,
            record.hot_x,
            record.hot_y,
            u32::from(record.width),
            u32::from(record.height),
            &record.bgra_bytes,
        ) {
            Ok(CursorUploadOutcome::Ready) => {}
            Ok(CursorUploadOutcome::Deferred) => {
                self.platform.clear_direct_cursor_retry(output_idx);
                if let Some(owner) = self.pstate.direct.get_mut(scope) {
                    owner.mark_direct_cursor_refresh_pending(output_idx);
                }
                log::debug!(
                    "scanout_m2: direct cursor upload deferred on output {output_idx}; retrying on CRTC event"
                );
                return true;
            }
            Err(error) => {
                log::warn!(
                    "scanout_m2: direct cursor upload failed on output {output_idx}: {error}; unflipping"
                );
                return false;
            }
        }
        match self
            .platform
            .cursor_plane_show_on_crtc(output_idx, record.hot_x, record.hot_y, x, y)
        {
            Ok(()) => {
                self.platform.clear_direct_cursor_retry(output_idx);
                if let Some(owner) = self.pstate.direct.get_mut(scope) {
                    owner.clear_direct_cursor_refresh_pending(output_idx);
                }
            }
            Err(error)
                if error.is_deferred()
                    || error.operation_error().raw_os_error() == Some(libc::EBUSY) =>
            {
                self.platform.clear_direct_cursor_retry(output_idx);
                if let Some(owner) = self.pstate.direct.get_mut(scope) {
                    owner.mark_direct_cursor_refresh_pending(output_idx);
                }
                log::debug!(
                    "scanout_m2: direct cursor show deferred on output {output_idx}; retrying on CRTC event"
                );
            }
            Err(error) => {
                if error.operation_error().raw_os_error() == Some(libc::EINVAL) {
                    log::warn!(
                        "scanout_m2: direct cursor show failed on output {output_idx}: {error}; unflipping"
                    );
                    return false;
                }
                return self.schedule_direct_cursor_retry(
                    scope,
                    output_idx,
                    "show",
                    error.operation_error(),
                    "cursor_show_failed",
                );
            }
        }
        true
    }

    fn retry_due_direct_cursor_refreshes_before_successor(&mut self, scope: DirectScope) {
        let Some(owner) = self.pstate.direct.get(scope) else {
            return;
        };
        let due_outputs: Vec<_> = owner
            .direct_cursor_refresh_pending
            .iter()
            .copied()
            .filter(|output_idx| {
                owner
                    .direct_cursor_refresh_successor_deferrals
                    .get(output_idx)
                    .is_some_and(|count| *count >= DIRECT_CURSOR_RETRY_AFTER_SUCCESSOR_EVENTS)
            })
            .collect();
        for output_idx in due_outputs {
            self.retry_pending_direct_cursor_refresh(scope, output_idx, true);
        }
    }

    fn retry_due_direct_cursor_refreshes_on_timer(&mut self) {
        let now = self.monotonic_now();
        for scope in self.pstate.direct.active_scopes() {
            let Some(owner) = self.pstate.direct.get(scope) else {
                continue;
            };
            if owner.pending.is_some() || owner.queued_successor.is_some() || owner.unflip_requested
            {
                continue;
            }
            let due_outputs: Vec<_> = owner
                .direct_cursor_refresh_pending
                .iter()
                .copied()
                .filter(|output_idx| {
                    self.platform
                        .direct_cursor_retry_deadline(*output_idx)
                        .is_some_and(|deadline| deadline <= now)
                })
                .collect();
            for output_idx in due_outputs {
                self.retry_pending_direct_cursor_refresh(scope, output_idx, false);
            }
        }
    }

    fn retry_pending_direct_cursor_refresh(
        &mut self,
        scope: DirectScope,
        output_idx: usize,
        direct_frame_retired: bool,
    ) {
        let Some(owner) = self.pstate.direct.get(scope) else {
            return;
        };
        if !owner.direct_cursor_refresh_pending.contains(&output_idx)
            || !owner.active()
            || owner.unflip_requested
        {
            return;
        }
        if self
            .platform
            .direct_cursor_retry_deadline(output_idx)
            .is_some_and(|deadline| self.monotonic_now() < deadline)
        {
            return;
        }
        if scope == DirectScope::Root && self.pstate.tearing.is_on() {
            for output_idx in 0..self.platform.outputs.len() {
                self.platform.clear_direct_cursor_retry(output_idx);
            }
            if let Some(owner) = self.pstate.direct.get_mut(scope) {
                owner.clear_all_direct_cursor_refresh_pending();
            }
            return;
        }
        // Keep direct frames first for the first two successor events. Once
        // the per-output bound is reached, retry before submitting that
        // successor so the cursor commit gets one CRTC event to retire.
        let successor_deferrals = self
            .pstate
            .direct
            .get(scope)
            .expect("direct owner exists")
            .direct_cursor_refresh_successor_deferrals
            .get(&output_idx)
            .copied()
            .unwrap_or(0);
        let owner = self.pstate.direct.get(scope).expect("direct owner exists");
        if (owner.queued_successor.is_some()
            && successor_deferrals < DIRECT_CURSOR_RETRY_AFTER_SUCCESSOR_EVENTS)
            || (owner.pending.is_some() && !direct_frame_retired)
        {
            return;
        }
        if self.effective_cursor_hidden() {
            match self.platform.cursor_plane_hide_on_crtc(output_idx) {
                Ok(()) => {
                    self.platform.clear_direct_cursor_retry(output_idx);
                    if let Some(owner) = self.pstate.direct.get_mut(scope) {
                        owner.clear_direct_cursor_refresh_pending(output_idx);
                    }
                }
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock
                        || error.raw_os_error() == Some(libc::EBUSY) =>
                {
                    self.platform.clear_direct_cursor_retry(output_idx);
                    log::debug!(
                        "scanout_m2: direct cursor hide deferred on output {output_idx}; retrying on CRTC event"
                    );
                    return;
                }
                Err(error) => {
                    if error.raw_os_error() == Some(libc::EINVAL)
                        || error.raw_os_error() == Some(libc::EBUSY)
                    {
                        log::warn!(
                            "scanout_m2: direct cursor hide failed on output {output_idx}: {error}; unflipping"
                        );
                        self.request_direct_unflip_for(scope, "cursor_hide_failed");
                    } else {
                        self.schedule_direct_cursor_retry(
                            scope,
                            output_idx,
                            "hide",
                            &error,
                            "cursor_hide_failed",
                        );
                    }
                    return;
                }
            }
            let output_indices = self.direct_scope_outputs(scope);
            if !self.sync_direct_cursor_scene_state(&output_indices) {
                self.request_direct_unflip_for(scope, "cursor_scene_sync_failed");
            }
            return;
        }
        let Some(xid) = self.effective_cursor_xid else {
            self.platform.clear_direct_cursor_retry(output_idx);
            if let Some(owner) = self.pstate.direct.get_mut(scope) {
                owner.clear_direct_cursor_refresh_pending(output_idx);
            }
            return;
        };
        let Some(record) = self.cursor_records.get(&xid).cloned() else {
            self.request_direct_unflip_for(scope, "cursor_show_failed");
            return;
        };
        #[allow(clippy::cast_possible_truncation)]
        let x = self.core.cursor_x as i32;
        #[allow(clippy::cast_possible_truncation)]
        let y = self.core.cursor_y as i32;
        if !self.refresh_direct_cursor_on_output(scope, output_idx, &record, x, y) {
            self.request_direct_unflip_for(scope, "cursor_show_failed");
            return;
        }
        let output_indices = self.direct_scope_outputs(scope);
        if !self.sync_direct_cursor_scene_state(&output_indices) {
            self.request_direct_unflip_for(scope, "cursor_scene_sync_failed");
        }
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
        scope: DirectScope,
        timing: crate::drm::async_flip::FlipTiming,
    ) -> io::Result<crate::drm::async_flip::AsyncOutcome> {
        if matches!(
            self.pstate.server_scanout.sources.get(&frame.source_id),
            Some(crate::kms::render::backend::ServerScanoutState::Disqualified(_))
        ) {
            return Err(io::Error::other("direct source was exported"));
        }
        let submitter = self.pstate.direct_frame_submitter;
        let outcome = submitter(self, frame, scope, timing)?;
        if let Some(tally) = frame.fence_tally.take() {
            self.count_server_fence(tally);
        }
        Ok(outcome)
    }

    pub(in crate::kms::render::backend) fn submit_direct_frame_live(
        &mut self,
        frame: &mut DirectPresentFrame,
        scope: DirectScope,
        timing: crate::drm::async_flip::FlipTiming,
    ) -> io::Result<crate::drm::async_flip::AsyncOutcome> {
        let dest_id = self.store.lookup(frame.candidate.paint_dst_host_xid);
        if !self.direct_source_ready(frame.source_id, dest_id, scope) {
            return Err(io::Error::other("direct source is not ready for scanout"));
        }
        let fb = self
            .pstate
            .scanout_m1
            .entries
            .get(&(frame.source_id, scope))
            .and_then(ScanoutM1ProbeEntry::framebuffer)
            .map(crate::drm::modeset::DirectScanoutProbeFramebuffer::handle)
            .ok_or_else(|| io::Error::other("direct successor framebuffer disappeared"))?;
        let output_indices = self.direct_scope_outputs(scope);
        let mut plane_states =
            scanout_m1_plane_states(&self.platform.outputs, &output_indices, scope);
        // A server-owned source's write fence gates every plane of the commit (D8).
        let in_fence_fd = frame.in_fence.as_ref().map(std::os::fd::AsFd::as_fd);
        for state in &mut plane_states {
            state.in_fence_fd = in_fence_fd;
        }
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
        let cursor_positions = self
            .platform
            .cursor_plane_atomic_positions_for_outputs(&output_indices);
        let vrr_wanted = self.direct_frame_wants_vrr(frame);
        let vrr_before = self.vrr_committed_snapshot();
        let async_shape = if timing == crate::drm::async_flip::FlipTiming::Async {
            Some(
                self.pstate
                    .scanout_m1
                    .entries
                    .get(&(frame.source_id, scope))
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
            vrr_wanted,
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
                        vrr_wanted,
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
        let vrr_off_reason = if frame.target == ScanoutM0Target::Unredirected
            && self.pstate.vrr_mode == crate::launch::VrrMode::Property
            && !frame.candidate.variable_refresh
        {
            "property"
        } else {
            "inherited"
        };
        self.log_vrr_committed_changes(
            &vrr_before,
            (frame.target == ScanoutM0Target::Unredirected)
                .then_some(frame.candidate.dst_window_xid),
            vrr_off_reason,
        );
        if attempt_timing == crate::drm::async_flip::FlipTiming::Vsync
            || matches!(
                async_outcome,
                crate::drm::async_flip::AsyncOutcome::FellBack { .. }
            )
        {
            self.platform
                .cursor_plane_scene_commit_submitted(&cursor_positions);
        }
        frame.awaiting_outputs = output_indices.into_iter().collect();
        frame.accepted_event_id = Some(accepted_event_id);
        frame.async_outcome = async_outcome;
        Ok(async_outcome)
    }

    pub(in crate::kms::render::backend) fn vrr_committed_snapshot(&self) -> Vec<bool> {
        self.platform
            .outputs
            .iter()
            .map(|output| output.output.vrr_committed.get())
            .collect()
    }

    fn direct_frame_wants_vrr(&self, frame: &DirectPresentFrame) -> bool {
        if frame.target != ScanoutM0Target::Unredirected {
            return false;
        }
        match self.pstate.vrr_mode {
            crate::launch::VrrMode::Off => false,
            crate::launch::VrrMode::Property => frame.candidate.variable_refresh,
            crate::launch::VrrMode::All => !self.vrr_client_excluded(&frame.candidate),
        }
    }

    fn vrr_client_excluded(
        &self,
        candidate: &yserver_core::backend::PresentScanoutCandidate,
    ) -> bool {
        if let Some(source) = self
            .pstate
            .vrr_exclusion_cache
            .borrow()
            .get(&candidate.client_id)
        {
            return source.is_some();
        }

        let decision = match (self.pstate.vrr_exclusions.as_ref(), candidate.client_pid) {
            (Some(exclusions), Some(pid)) => exclusions.evaluate(pid, &self.pstate.vrr_proc_root),
            (_, None) => {
                log::debug!(
                    "kms: vrr exclusion skipped client={} reason=no-pid",
                    candidate.client_id
                );
                crate::vrr_exclusions::VrrExclusionDecision {
                    process_name: None,
                    source: None,
                }
            }
            _ => crate::vrr_exclusions::VrrExclusionDecision {
                process_name: None,
                source: None,
            },
        };
        if let (Some(pid), Some(source)) = (candidate.client_pid, decision.source) {
            let source_name = match source {
                crate::vrr_exclusions::VrrExclusionSource::Environment => "environment",
                crate::vrr_exclusions::VrrExclusionSource::NvidiaProfiles => "nvidia-profiles",
                crate::vrr_exclusions::VrrExclusionSource::MesaDriconf => "mesa-driconf",
            };
            log::info!(
                "kms: vrr excluded client={} pid={} process={} source={source_name}",
                candidate.client_id,
                pid,
                decision.process_name.as_deref().unwrap_or("unknown")
            );
        }
        self.pstate
            .vrr_exclusion_cache
            .borrow_mut()
            .insert(candidate.client_id, decision.source);
        decision.source.is_some()
    }

    pub(in crate::kms::render::backend) fn live_direct_vrr_wanted_by_output(&self) -> Vec<bool> {
        (0..self.platform.outputs.len())
            .map(|output_idx| {
                let Some(output_frame) = self.direct_scanout_frame_for_output(output_idx) else {
                    return false;
                };
                let Some(scope) = self.pstate.direct.scope_for_output(output_idx) else {
                    return false;
                };
                let Some(owner) = self.pstate.direct.get(scope) else {
                    return false;
                };
                let frame = owner
                    .queued_successor
                    .as_ref()
                    .or(owner.pending.as_ref())
                    .unwrap_or(output_frame);
                self.direct_frame_wants_vrr(frame)
            })
            .collect()
    }

    pub(in crate::kms::render::backend) fn backend_scanout_on_window_property_changed(
        &mut self,
        state: &yserver_core::server::ServerState,
        host_xid: u32,
        property: yserver_protocol::x11::AtomId,
    ) {
        if self.pstate.vrr_mode != crate::launch::VrrMode::Property || host_xid == 0 {
            return;
        }
        let Some(vrr_atom) = state.atoms.id_for("_VARIABLE_REFRESH") else {
            return;
        };
        if property != vrr_atom {
            return;
        }
        let matches_host = |candidate: &yserver_core::backend::PresentScanoutCandidate| {
            state
                .resources
                .window(yserver_protocol::x11::ResourceId(candidate.dst_window_xid))
                .and_then(|window| window.host_xid)
                .is_some_and(|window| window.as_raw() == host_xid)
        };
        let changed_owners: Vec<_> = self
            .pstate
            .direct
            .active_scopes()
            .into_iter()
            .filter_map(|scope| {
                self.pstate.direct.get(scope).and_then(|owner| {
                    owner
                        .current
                        .iter()
                        .chain(owner.pending.iter())
                        .chain(owner.queued_successor.iter())
                        .find(|frame| matches_host(&frame.candidate))
                        .map(|frame| {
                            (
                                scope,
                                frame.candidate.dst_window_xid,
                                frame.candidate.crtc_id,
                            )
                        })
                })
            })
            .collect();
        let Some((_, window, _)) = changed_owners.first().copied() else {
            return;
        };
        let allowed = {
            state
                .resources
                .window_property(yserver_protocol::x11::ResourceId(window), vrr_atom)
        };
        let allowed = yserver_core::properties::variable_refresh_property_enabled(allowed);
        for (scope, _, _) in &changed_owners {
            let scanout = self
                .pstate
                .direct
                .get_mut(*scope)
                .expect("matched direct owner remains present");
            let (current, pending, queued_successor) = (
                &mut scanout.current,
                &mut scanout.pending,
                &mut scanout.queued_successor,
            );
            for frame in current
                .iter_mut()
                .chain(pending.iter_mut())
                .chain(queued_successor.iter_mut())
            {
                if matches_host(&frame.candidate) {
                    frame.candidate.variable_refresh = allowed;
                }
            }
        }
        for (scope, _, direct_crtc_id) in changed_owners {
            let Some((direct_output_idx, _)) = self.present_crtc_output(direct_crtc_id) else {
                continue;
            };

            if allowed {
                let output = &self.platform.outputs[direct_output_idx].output;
                output.vrr_clear_retry_deadline.set(None);
                output.vrr_clear_waiting_for_flip.set(false);
                continue;
            }
            let Some(owner) = self.pstate.direct.get(scope) else {
                continue;
            };
            if owner.queued_successor.is_some()
                || owner
                    .queued_successor
                    .as_ref()
                    .or(owner.pending.as_ref())
                    .or(owner.current.as_ref())
                    .is_some_and(|frame| self.direct_frame_wants_vrr(frame))
                || !self.scanout_allowed()
                || !self.kms_outputs_active
            {
                continue;
            }

            let now = self.monotonic_now();
            let output = &self.platform.outputs[direct_output_idx].output;
            if !output.vrr_committed.get() {
                continue;
            }
            if self
                .pstate
                .direct
                .get(scope)
                .is_some_and(|owner| owner.pending.is_some())
                || self
                    .platform
                    .scanout_commit_in_flight_for_output(direct_output_idx)
            {
                output.vrr_clear_retry_deadline.set(None);
                output.vrr_clear_waiting_for_flip.set(true);
                continue;
            }
            if let Err(error) =
                self.submit_pending_crtc_property_for_output(direct_output_idx, false, now)
            {
                log::warn!(
                    "kms: standalone VRR clear for CRTC {:?} failed: {error}",
                    output.crtc
                );
            }
        }
    }

    pub(in crate::kms::render::backend) fn log_vrr_committed_changes(
        &self,
        before: &[bool],
        owner_window: Option<u32>,
        fallback_off_reason: &'static str,
    ) {
        let off_reason = if fallback_off_reason == "property" {
            fallback_off_reason
        } else {
            self.pstate
                .direct
                .root()
                .unflip_last_reason
                .unwrap_or(fallback_off_reason)
        };
        for (index, layout) in self.platform.outputs.iter().enumerate() {
            let was_enabled = before.get(index).copied().unwrap_or(false);
            let enabled = layout.output.vrr_committed.get();
            if was_enabled == enabled {
                continue;
            }
            if enabled {
                if let Some(window) = owner_window {
                    log::info!(
                        "kms: vrr on output={} crtc={} window=0x{window:x}",
                        layout.key.connector_name,
                        u32::from(layout.output.crtc)
                    );
                }
            } else {
                log::info!(
                    "kms: vrr off output={} crtc={} reason={off_reason}",
                    layout.key.connector_name,
                    u32::from(layout.output.crtc)
                );
            }
        }
    }

    /// Submit a standalone optional CRTC property and report any accepted VRR
    /// transition through the same logger used by frame commits.
    pub(in crate::kms::render::backend) fn submit_pending_crtc_property_for_output(
        &self,
        output_idx: usize,
        vrr_wanted: bool,
        now: std::time::Instant,
    ) -> io::Result<()> {
        let before = self.vrr_committed_snapshot();
        let current_primary_framebuffer = self.current_primary_framebuffer_for_output(output_idx);
        let result = self.platform.submit_pending_crtc_property_for_output(
            output_idx,
            vrr_wanted,
            now,
            current_primary_framebuffer,
        );
        self.log_vrr_committed_changes(&before, None, "property");
        result
    }

    fn retry_pending_crtc_property_after_flip(
        &mut self,
        output_idx: usize,
        now: std::time::Instant,
    ) {
        let before = self.vrr_committed_snapshot();
        let vrr_wanted = self
            .live_direct_vrr_wanted_by_output()
            .get(output_idx)
            .copied()
            .unwrap_or(false);
        let current_primary_framebuffer = self.current_primary_framebuffer_for_output(output_idx);
        self.platform.retry_pending_crtc_property_after_flip(
            output_idx,
            now,
            vrr_wanted,
            current_primary_framebuffer,
        );
        self.log_vrr_committed_changes(&before, None, "property");
    }

    fn current_primary_framebuffer_for_output(
        &self,
        output_idx: usize,
    ) -> Option<::drm::control::framebuffer::Handle> {
        if let Some(frame) = self.direct_scanout_frame_for_output(output_idx) {
            let scope = self.pstate.direct.scope_for_output(output_idx)?;
            return self
                .pstate
                .scanout_m1
                .entries
                .get(&(frame.source_id, scope))
                .and_then(ScanoutM1ProbeEntry::framebuffer)
                .map(crate::drm::modeset::DirectScanoutProbeFramebuffer::handle);
        }
        None
    }

    #[cfg(test)]
    pub(in crate::kms::render::backend) fn submit_direct_frame_for_tests(
        backend: &mut KmsBackend,
        frame: &mut DirectPresentFrame,
        scope: DirectScope,
        _timing: crate::drm::async_flip::FlipTiming,
    ) -> io::Result<crate::drm::async_flip::AsyncOutcome> {
        frame.awaiting_outputs = backend.direct_scope_outputs(scope).into_iter().collect();
        frame.async_outcome = crate::drm::async_flip::AsyncOutcome::NotRequested;
        Ok(crate::drm::async_flip::AsyncOutcome::NotRequested)
    }

    pub(super) fn direct_frame_flip_timing(
        &self,
        frame: &DirectPresentFrame,
        scope: DirectScope,
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
            .get(&(frame.source_id, scope))
            .and_then(|entry| entry.async_shape.as_ref());
        let direct_outputs = self.direct_scope_outputs(scope);
        if owns_stream
            && frame.candidate.options & yserver_core::present_scheduler::PRESENT_OPTION_ASYNC != 0
            && !direct_outputs
                .iter()
                .any(|&output_idx| self.platform.cursor_plane_visible_for_output(output_idx))
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

    fn submit_queued_direct_successor(&mut self, scope: DirectScope) {
        // A cursor/overlay/topology invalidation which arrived while the
        // predecessor was in flight wins over the queued Present. Leave the
        // successor retained for the composed-unflip teardown to Skip.
        let Some(owner) = self.pstate.direct.get(scope) else {
            return;
        };
        if owner.unflip_requested || owner.pending.is_some() {
            return;
        }
        if let DirectScope::Output(output_idx) = scope
            && owner.queued_successor.is_some()
            && !self.direct_output_frames_scene_eligible(output_idx)
        {
            self.request_direct_unflip_for(scope, "queued_direct_successor_scene_changed");
            return;
        }
        let direct_outputs = self.direct_scope_outputs(scope);
        if self
            .platform
            .scanout_commit_in_flight_for_outputs(&direct_outputs)
        {
            return;
        }
        let Some(mut successor) = self
            .pstate
            .direct
            .get_mut(scope)
            .and_then(|owner| owner.queued_successor.take())
        else {
            return;
        };
        let timing = self.direct_frame_flip_timing(&successor, scope);
        match self.submit_direct_frame(&mut successor, scope, timing) {
            Ok(_) => {
                log::debug!(
                    "scanout_m2: submitted queued direct successor source_id={} present_id={} outputs={}",
                    successor.source_id.as_u64(),
                    successor.candidate.present_id,
                    direct_outputs.len()
                );
                if let Some(owner) = self.pstate.direct.get_mut(scope) {
                    owner.pending = Some(successor);
                    owner.hold_direct = true;
                }
            }
            Err(error) => {
                log::warn!(
                    "scanout_m2: queued direct successor submit failed after predecessor retirement: {error}"
                );
                self.defer_direct_successor_skip(scope, successor);
                if let Some(direct) = self.pstate.direct.get_mut(scope) {
                    direct
                        .completed
                        .append(&mut direct.deferred_successor_skips);
                }
                self.request_direct_unflip_for(scope, "queued_direct_successor_submit_failed");
            }
        }
    }

    fn release_direct_frame(&mut self, scope: DirectScope, frame: DirectPresentFrame) {
        if let Some(owner) = self.pstate.direct.get_mut(scope) {
            owner.idled.push(frame.event);
        }
        <Self as Backend>::release_present_source(self, frame.source_pin);
        <Self as Backend>::release_present_source(self, frame.fallback_target_pin);
        self.release_unreferenced_server_scanout();
    }

    fn defer_direct_successor_skip(&mut self, scope: DirectScope, mut frame: DirectPresentFrame) {
        frame.event.completion_mode = yserver_protocol::x11::present::COMPLETE_MODE_SKIP;
        // Match Present supersession: the discarded buffer becomes idle as
        // soon as it leaves the bounded successor slot, while its Skip
        // CompleteNotify remains ordered behind the in-flight predecessor.
        // `emit_idle = false` prevents the later completion from idling it a
        // second time.
        frame.event.emit_idle = false;
        if let Some(owner) = self.pstate.direct.get_mut(scope) {
            owner.idled.push(frame.event.clone());
            owner.deferred_successor_skips.push(frame.event);
        }
        <Self as Backend>::release_present_source(self, frame.source_pin);
        <Self as Backend>::release_present_source(self, frame.fallback_target_pin);
        self.note_present_skip();
    }

    pub(in crate::kms::render::backend) fn queue_direct_successor(
        &mut self,
        scope: DirectScope,
        frame: DirectPresentFrame,
    ) {
        let superseded = self
            .pstate
            .direct
            .get_mut(scope)
            .and_then(|owner| owner.queued_successor.replace(frame));
        if let Some(superseded) = superseded {
            self.defer_direct_successor_skip(scope, superseded);
        }
        if let Some(owner) = self.pstate.direct.get_mut(scope) {
            owner.hold_direct = true;
        }
        log::debug!(
            "scanout_m2: queued latest direct successor present_id={}",
            self.pstate
                .direct
                .get(scope)
                .expect("queued successor owner exists")
                .queued_successor
                .as_ref()
                .map_or(0, |frame| frame.candidate.present_id)
        );
    }

    /// Release direct records only after the caller has disabled/replaced the
    /// primary planes. A not-yet-retired submission falls back to Copy mode.
    pub(in crate::kms::render::backend) fn stop_direct_after_scanout_replaced(
        &mut self,
        scope: DirectScope,
        reason: &'static str,
    ) {
        let outputs = self.direct_scope_outputs(scope);
        let vrr_committed = self
            .platform
            .outputs
            .iter()
            .enumerate()
            .any(|(output_idx, output)| {
                outputs.contains(&output_idx) && output.output.vrr_committed.get()
            });
        if vrr_committed {
            if let Some(owner) = self.pstate.direct.get_mut(scope) {
                owner.unflip_last_reason.get_or_insert(reason);
            }
        } else if let Some(owner) = self.pstate.direct.get_mut(scope) {
            owner.unflip_last_reason = None;
        }
        if let Some(mut pending) = self
            .pstate
            .direct
            .get_mut(scope)
            .and_then(|owner| owner.pending.take())
        {
            pending.event.completion_mode = yserver_protocol::x11::present::COMPLETE_MODE_COPY;
            pending.event.emit_idle = true;
            if let Some(owner) = self.pstate.direct.get_mut(scope) {
                owner.completed.push(pending.event);
            }
            <Self as Backend>::release_present_source(self, pending.source_pin);
            <Self as Backend>::release_present_source(self, pending.fallback_target_pin);
        }
        if let Some(queued) = self
            .pstate
            .direct
            .get_mut(scope)
            .and_then(|owner| owner.queued_successor.take())
        {
            self.defer_direct_successor_skip(scope, queued);
        }
        if let Some(owner) = self.pstate.direct.get_mut(scope) {
            owner.completed.append(&mut owner.deferred_successor_skips);
        }
        if let Some(current) = self
            .pstate
            .direct
            .get_mut(scope)
            .and_then(|owner| owner.current.take())
        {
            self.release_direct_frame(scope, current);
        }
        if let Some(owner) = self.pstate.direct.get_mut(scope) {
            owner.hold_direct = false;
            owner.cursor_bound_all = false;
            owner.unflip_requested = false;
            owner.unflip_reason = None;
            owner.unflip_outputs_to_replace.clear();
            owner.unflip_awaiting_outputs.clear();
            owner.reentry_blocked_until_composed = false;
            owner.reset_eligible_direct_probation();
            owner.unflip_fallback_source = None;
            owner.unflip_shadow_ready = false;
            owner.degraded_composed_unflip = false;
            owner.fresh_composed_unflip = false;
            owner.clear_all_direct_cursor_refresh_pending();
        }
        for output_idx in outputs {
            self.platform.clear_direct_cursor_retry(output_idx);
        }
        if self.pstate.direct.active_scopes().is_empty() {
            self.finish_deferred_cow_release();
        }
        self.release_unreferenced_server_scanout();
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
        let scopes = self.pstate.direct.active_scopes();
        if scopes.is_empty() {
            return Ok(false);
        }
        let relight = self.kms_outputs_active;
        for &scope in &scopes {
            self.materialize_direct_shadow_for_unflip(scope)?;
        }
        if let Err(error) = self.platform.dpms_set_outputs_active(false) {
            log::error!(
                "scanout_m2: {context}: could not disable the old grouped direct topology: {error}; exiting"
            );
            // Keep direct source pins/framebuffers alive: the helper attempts
            // every output and failure may mean one CRTC still scans them.
            self.request_exit();
            return Err(error);
        }
        for scope in scopes {
            self.stop_direct_after_scanout_replaced(scope, context);
        }
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
        let scope = self.pstate.direct.scope_for_output(output_idx)?;
        let owner = self.pstate.direct.get(scope)?;
        let slot = direct_frame_slot_on_output(
            output_idx,
            owner.pending.as_ref().map(|frame| &frame.awaiting_outputs),
            owner.current.is_some(),
            &owner.unflip_outputs_to_replace,
        )?;
        match slot {
            DirectFrameSlot::Pending => owner.pending.as_ref(),
            DirectFrameSlot::Current => owner.current.as_ref(),
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
        scope: DirectScope,
    ) -> io::Result<()> {
        let owner = self
            .pstate
            .direct
            .get(scope)
            .ok_or_else(|| io::Error::other("scanout M2: no direct owner for fallback"))?;
        if owner.unflip_shadow_ready {
            return Ok(());
        }
        let current = owner
            .pending
            .as_ref()
            .or(owner.current.as_ref())
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
        if let Some(owner) = self.pstate.direct.get_mut(scope) {
            owner.unflip_shadow_ready = true;
        }
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
    fn submit_composed_unflip(&mut self, scope: DirectScope) -> io::Result<bool> {
        let output_indices = self.direct_scope_outputs(scope);
        if self
            .platform
            .scanout_commit_in_flight_for_outputs(&output_indices)
        {
            return Ok(false);
        }
        let output_scope_is_primary = match scope {
            DirectScope::Root => self.direct_scanout_topology_eligible(DirectScope::Root),
            DirectScope::Output(output_idx) => {
                self.platform.outputs.get(output_idx).is_some_and(|layout| {
                    self.platform
                        .primary_device()
                        .is_some_and(|primary| layout.key.device_key == primary.key)
                })
            }
        };
        if !output_scope_is_primary {
            return Err(io::Error::other(
                "scanout M2: direct unflip scope is not on the primary DRM device",
            ));
        }
        self.materialize_direct_shadow_for_unflip(scope)?;
        let planes: Vec<crate::drm::modeset::ComposedScanoutPlaneState<'_>> = output_indices
            .iter()
            .map(|&output_idx| {
                let layout = &self.platform.outputs[output_idx];
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
        let cursor_positions = self
            .platform
            .cursor_plane_atomic_positions_for_outputs(&output_indices);
        let vrr_before = self.vrr_committed_snapshot();
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
        self.log_vrr_committed_changes(&vrr_before, None, "inherited");
        self.platform
            .cursor_plane_scene_commit_submitted(&cursor_positions);
        if let Some(owner) = self.pstate.direct.get_mut(scope) {
            owner.unflip_outputs_to_replace = output_indices.iter().copied().collect();
            owner.unflip_awaiting_outputs = output_indices
                .iter()
                .map(|&output_idx| (output_idx, accepted_event_id))
                .collect();
        }
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
            self.pstate
                .direct
                .get(scope)
                .and_then(|owner| owner.unflip_reason)
                .unwrap_or("unknown"),
            self.pstate
                .direct
                .get(scope)
                .expect("unflip owner exists")
                .unflip_last_reason
                .unwrap_or("unknown"),
            self.pstate
                .direct
                .get(scope)
                .expect("unflip owner exists")
                .pending
                .as_ref()
                .map(describe_frame),
            self.pstate
                .direct
                .get(scope)
                .expect("unflip owner exists")
                .current
                .as_ref()
                .map(describe_frame)
        );
        Ok(true)
    }

    /// Whether the unflip can replace every direct plane with a freshly
    /// composed scene frame: shared scene outputs that can all accept a frame
    /// now, explicit in-fences, and the grouped unflip's own topology.
    fn fresh_composed_unflip_eligible(&self, scope: DirectScope) -> bool {
        use crate::kms::render::scene::{OutputRoute, OutputSink};

        let outputs = self.direct_scope_outputs(scope);
        let scope_is_on_primary = match scope {
            DirectScope::Root => self.direct_scanout_topology_eligible(DirectScope::Root),
            DirectScope::Output(output_idx) => {
                self.platform.outputs.get(output_idx).is_some_and(|layout| {
                    self.platform
                        .primary_device()
                        .is_some_and(|primary| layout.key.device_key == primary.key)
                })
            }
        };
        let tearing_cursor_handoff = self
            .pstate
            .direct
            .get(scope)
            .and_then(|owner| owner.unflip_reason)
            == Some("tearing_cursor_handoff");
        !outputs.is_empty()
            // This handoff needs a fresh scene frame so the newest Present
            // Copy replaces the retained direct frame in the same commit.
            && (!self.pstate.tearing.is_on() || tearing_cursor_handoff)
            && scope_is_on_primary
            && !self.platform.scanout_commit_in_flight_for_outputs(&outputs)
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
    fn submit_fresh_composed_unflip(&mut self, scope: DirectScope) -> bool {
        if !self.fresh_composed_unflip_eligible(scope) {
            return false;
        }
        if let Err(error) = self.materialize_direct_shadow_for_unflip(scope) {
            log::debug!("scanout_m2: fresh composed unflip skipped: {error}");
            return false;
        }
        let preserve_global_scene_wake =
            matches!(scope, DirectScope::Output(_)) && self.scene.scene_structure_dirty;
        match scope {
            DirectScope::Root => {
                self.scene.invalidate_all_scanout_damage();
                self.scene.mark_scene_structure_dirty();
            }
            DirectScope::Output(output_idx) => {
                self.scene.invalidate_scanout_damage_for_output(output_idx);
                self.scene.mark_scene_structure_dirty_for_output(output_idx);
            }
        }
        self.platform.defer_scene_flips = true;
        let mut skip_outputs = vec![true; self.platform.outputs.len()];
        for output_idx in self.direct_scope_outputs(scope) {
            if let Some(skip) = skip_outputs.get_mut(output_idx) {
                *skip = false;
            }
        }
        self.compose_tick(&skip_outputs);
        if preserve_global_scene_wake {
            self.scene.scene_structure_dirty = true;
        }
        self.platform.defer_scene_flips = false;
        let deferred = std::mem::take(&mut self.platform.deferred_scene_flips);
        let output_indices = self.direct_scope_outputs(scope);
        let output_count = output_indices.len();
        let incomplete: Vec<_> = deferred
            .iter()
            .filter(|&&(_, _, complete)| !complete)
            .map(|&(output_idx, _, _)| output_idx)
            .collect();
        let targets: Option<Vec<_>> = if deferred.len() == output_count {
            output_indices
                .iter()
                .copied()
                .map(|output_idx| {
                    let mut frames = deferred.iter().filter(|&&(out, _, _)| out == output_idx);
                    match (frames.next(), frames.next()) {
                        (Some(&(_, bo_idx, _)), None) => {
                            self.platform.deferred_scene_flip_target(output_idx, bo_idx)
                        }
                        _ => None,
                    }
                })
                .collect()
        } else {
            None
        };
        let result = match targets {
            // A truncated compose painted only a prefix of the scene: never
            // flip it, the retained frames are whole.
            Some(_) if !incomplete.is_empty() => Err(io::Error::other(format!(
                "incomplete compose on outputs {incomplete:?}"
            ))),
            Some(targets) => self.commit_fresh_composed_unflip(scope, &targets),
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
        if let Some(owner) = self.pstate.direct.get_mut(scope) {
            owner.unflip_outputs_to_replace = output_indices.iter().copied().collect();
            owner.unflip_awaiting_outputs = output_indices
                .iter()
                .map(|&output_idx| (output_idx, accepted_event_id))
                .collect();
            owner.fresh_composed_unflip = true;
        }
        log::debug!(
            "scanout_m2: submitted fresh atomic composed unflip outputs={output_count} reason={}",
            self.pstate
                .direct
                .get(scope)
                .and_then(|owner| owner.unflip_reason)
                .unwrap_or("unknown")
        );
        true
    }

    /// The grouped commit of [`Self::submit_fresh_composed_unflip`]: one
    /// `(framebuffer, IN_FENCE_FD)` per output, in output order.
    fn commit_fresh_composed_unflip(
        &mut self,
        scope: DirectScope,
        targets: &[(::drm::control::framebuffer::Handle, i32)],
    ) -> io::Result<u64> {
        let output_indices = self.direct_scope_outputs(scope);
        let planes: Vec<_> = self
            .platform
            .outputs
            .iter()
            .enumerate()
            .filter(|(output_idx, _)| output_indices.contains(output_idx))
            .zip(targets)
            .map(|((_, layout), &(fb, in_fence_fd))| {
                crate::drm::modeset::ComposedScanoutPlaneState {
                    output: &layout.output,
                    fb,
                    in_fence_fd: Some(in_fence_fd),
                }
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
            .ok_or_else(|| io::Error::other("scanout M2: fresh unflip without a KMS device"))?;
        let cursor_positions = self
            .platform
            .cursor_plane_atomic_positions_for_outputs(&output_indices);
        let vrr_before = self.vrr_committed_snapshot();
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
        self.log_vrr_committed_changes(&vrr_before, None, "inherited");
        self.platform
            .cursor_plane_scene_commit_submitted(&cursor_positions);
        Ok(accepted_event_id)
    }

    /// Install only accepted per-output scene commits as degraded-unflip
    /// retirement obligations. Copied render readiness alone has no KMS ID.
    pub(in crate::kms::render::backend) fn record_accepted_degraded_unflip_replacements(&mut self) {
        for scope in self.pstate.direct.active_scopes() {
            let Some(owner) = self.pstate.direct.get(scope) else {
                continue;
            };
            if !owner.degraded_composed_unflip {
                continue;
            }
            let outputs = owner
                .unflip_outputs_to_replace
                .iter()
                .copied()
                .collect::<Vec<_>>();
            for output_idx in outputs {
                if let Some(event_id) = self.scene.pending_page_flip_event_id(output_idx)
                    && let Some(owner) = self.pstate.direct.get_mut(scope)
                {
                    owner.unflip_awaiting_outputs.insert(output_idx, event_id);
                }
            }
        }
    }

    pub(in crate::kms::render::backend) fn retire_direct_output(
        &mut self,
        output_idx: usize,
        event_id: Option<u64>,
        clock: yserver_core::backend::PresentClockSample,
    ) -> bool {
        let Some(scope) = self.pstate.direct.scope_for_output(output_idx) else {
            return false;
        };
        let Some(owner) = self.pstate.direct.get(scope) else {
            return false;
        };
        if let Some(&accepted_replacement_id) = owner.unflip_awaiting_outputs.get(&output_idx) {
            if event_id != Some(accepted_replacement_id) {
                return false;
            }
            let (degraded, scene_owned, cursor_handoff, owner_is_done) = {
                let owner = self
                    .pstate
                    .direct
                    .get_mut(scope)
                    .expect("direct owner remains present");
                owner.unflip_awaiting_outputs.remove(&output_idx);
                owner.unflip_outputs_to_replace.remove(&output_idx);
                let degraded = owner.degraded_composed_unflip;
                let scene_owned = degraded || owner.fresh_composed_unflip;
                let cursor_handoff =
                    !scene_owned && owner.unflip_reason == Some("tearing_cursor_handoff");
                (
                    degraded,
                    scene_owned,
                    cursor_handoff,
                    owner.unflip_outputs_to_replace.is_empty(),
                )
            };
            if cursor_handoff {
                self.scene
                    .retire_external_cursor_handoff(output_idx, &mut self.platform);
            }
            if owner_is_done {
                self.stop_direct_after_scanout_replaced(
                    scope,
                    if degraded {
                        "degraded composed unflip"
                    } else {
                        "atomic composed unflip"
                    },
                );
                if let Some(owner) = self.pstate.direct.get_mut(scope) {
                    owner.reentry_blocked_until_composed = true;
                }
                if scope == DirectScope::Root {
                    for owner in &mut self.pstate.direct.outputs {
                        owner.reentry_blocked_until_composed = true;
                    }
                }
                // Step 3 — while a CRTC scanned out a client buffer directly,
                // the composed BOs were not painted and the scene was not
                // tracking them; a direct retirement also bypasses
                // `handle_page_flip_complete` entirely. Treat every composed BO
                // as wholly stale on the way back.
                match scope {
                    DirectScope::Root => {
                        self.scene.invalidate_all_scanout_damage();
                        self.scene.mark_scene_structure_dirty();
                    }
                    DirectScope::Output(output_idx) => {
                        self.scene.invalidate_scanout_damage_for_output(output_idx);
                        self.scene.mark_scene_structure_dirty_for_output(output_idx);
                    }
                }
                log::debug!("scanout_m2: composed unflip retired for {scope:?}");
            }
            if scene_owned {
                // The planes were replaced by the scene's own composed
                // frames, which must still unwind their pending-acks and BO
                // state through the scene; hand the retire back.
                return false;
            }
            return true;
        }
        let Some(owner) = self.pstate.direct.get(scope) else {
            return false;
        };
        if owner.unflip_outputs_to_replace.contains(&output_idx) {
            return false;
        }
        let Some(pending) = self
            .pstate
            .direct
            .get_mut(scope)
            .and_then(|owner| owner.pending.as_mut())
        else {
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
                .direct
                .get_mut(scope)
                .and_then(|owner| owner.pending.take())
                .expect("pending direct frame disappeared");
            let completion_clock = presented
                .completion_clock
                .expect("selected direct CRTC retired with the grouped flip");
            presented.event.completion_mode = yserver_protocol::x11::present::COMPLETE_MODE_FLIP;
            presented.event.emit_idle = false;
            presented.event.completion_clock = Some(completion_clock);
            if let Some(owner) = self.pstate.direct.get_mut(scope) {
                owner.completed.push(presented.event.clone());
            }
            // Xorg publishes the retiring flip before re-executing any
            // flip-ready successor. Coalesced successors therefore become
            // ordered Skip completions only at this retirement boundary.
            if let Some(owner) = self.pstate.direct.get_mut(scope) {
                owner.completed.append(&mut owner.deferred_successor_skips);
            }
            if let Some(previous) = self
                .pstate
                .direct
                .get_mut(scope)
                .and_then(|owner| owner.current.replace(presented))
            {
                self.release_direct_frame(scope, previous);
            }
            log::debug!(
                "scanout_m2: direct frame retired scope={scope:?} source_id={}",
                self.pstate
                    .direct
                    .get(scope)
                    .and_then(|owner| owner.current.as_ref())
                    .map_or(0, |frame| frame.source_id.as_u64())
            );
            self.retry_due_direct_cursor_refreshes_before_successor(scope);
            // Like Xorg's present_flip_try_ready and wlroots' frame_pending
            // gate, submit a queued successor as soon as the prior grouped
            // transaction retires unless a bounded cursor retry just won a
            // CRTC event.
            self.submit_queued_direct_successor(scope);
            if self
                .pstate
                .direct
                .get(scope)
                .is_some_and(|owner| owner.queued_successor.is_none() && owner.pending.is_none())
            {
                self.bind_direct_cursor_on_outputs(scope);
            }
        }
        true
    }

    /// Whether the current whole-root Present may use upstream's grouped
    /// all-output direct transaction.
    ///
    /// One atomic request cannot cross DRM devices. Even on one card, grouping
    /// CRTCs of clearly different refresh makes replacement retirement wait for
    /// the slowest CRTC; keep those layouts on the normal per-output composed
    /// path so each output retains its native cadence. Rates within 0.5% (two
    /// "60 Hz" monitors) are grouped, like Xorg's multi-CRTC flip.
    pub(in crate::kms::render::backend) fn direct_scanout_topology_eligible(
        &self,
        scope: DirectScope,
    ) -> bool {
        let Some(primary) = self.platform.primary_device() else {
            return false;
        };
        match scope {
            DirectScope::Root => {
                let Some(first) = self.platform.outputs.first() else {
                    return false;
                };
                first.key.device_key == primary.key
                    && self.platform.outputs.iter().all(|layout| {
                        layout.key.device_key == primary.key
                            && effective_refresh_matches(
                                &first.output.picked,
                                &layout.output.picked,
                            )
                    })
            }
            DirectScope::Output(output_idx) => {
                let Some(layout) = self.platform.outputs.get(output_idx) else {
                    return false;
                };
                if layout.key.device_key != primary.key {
                    return false;
                }
                let geometries: Vec<_> = self
                    .platform
                    .outputs
                    .iter()
                    .map(|layout| ScanoutM1OutputGeometry {
                        x: layout.x,
                        y: layout.y,
                        width: u32::from(layout.width),
                        height: u32::from(layout.height),
                        mode_width: u32::from(layout.output.picked.width),
                        mode_height: u32::from(layout.output.picked.height),
                    })
                    .collect();
                scanout_m1_output_is_unoverlapped(output_idx, &geometries)
            }
        }
    }

    /// A whole-root direct Present is paced in the selected RANDR CRTC's
    /// domain even though the grouped framebuffer is installed on every
    /// output. The selected CRTC therefore has to be live, owned by the
    /// primary DRM device that imported the framebuffer, and a member of the
    /// homogeneous single-device topology accepted by the grouped path.
    pub(in crate::kms::render::backend) fn direct_present_crtc_eligible(
        &self,
        scope: DirectScope,
        crtc_id: u32,
        crtc_epoch: u64,
    ) -> bool {
        if crtc_epoch == 0 || self.present_crtc_clock_epoch(crtc_id) != crtc_epoch {
            return false;
        }
        let Some((selected_output_idx, crtc_key)) = self.present_crtc_output(crtc_id) else {
            return false;
        };
        let on_primary = self
            .platform
            .primary_device()
            .is_some_and(|primary| crtc_key.device_key == primary.key);
        on_primary
            && match scope {
                DirectScope::Root => self.direct_scanout_topology_eligible(DirectScope::Root),
                DirectScope::Output(output_idx) => {
                    selected_output_idx == output_idx
                        && self.direct_scanout_topology_eligible(scope)
                }
            }
    }

    /// Bring the M1 cache to the current output topology, dropping its
    /// entries on a change. `false` while a direct owner is active across a
    /// change (#311): the cache must not be touched then.
    pub(in crate::kms::render::backend) fn sync_scanout_m1_topology(&mut self) -> bool {
        let topology_signature = self.scanout_m1_topology_signature();
        if self.pstate.scanout_m1.topology_signature != topology_signature {
            if self.pstate.direct.root().active()
                || self
                    .pstate
                    .direct
                    .outputs
                    .iter()
                    .any(|owner| owner.active())
            {
                return false;
            }
            self.pstate.scanout_m1.clear("output topology changed");
            self.pstate
                .direct
                .sync_output_count(self.platform.outputs.len());
            self.pstate.scanout_m1.topology_signature = topology_signature;
        }
        true
    }

    pub(in crate::kms::render::backend) fn maybe_probe_scanout_m1(
        &mut self,
        source_id: Option<DrawableId>,
        target: ScanoutM0Target,
        coverage: ScanoutM0Coverage,
        scope: Option<DirectScope>,
        candidate: PresentScanoutCandidate,
    ) {
        let Some(scope) = scope else {
            return;
        };
        let output_indices = self.direct_scope_outputs(scope);
        let root_overlay_empty = self.direct_root_overlay_empty_for_outputs(&output_indices);
        if self.platform.any_output_transformed()
            || !self.direct_present_crtc_eligible(scope, candidate.crtc_id, candidate.crtc_epoch)
            || !scanout_m1_probe_eligible(
                self.scanout_allowed(),
                self.kms_outputs_active,
                self.direct_cursor_eligible_for(&output_indices),
                root_overlay_empty,
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

        if !self.sync_scanout_m1_topology() {
            return;
        }
        if self
            .pstate
            .scanout_m1
            .entries
            .contains_key(&(source_id, scope))
        {
            return;
        }

        let root = (u32::from(self.platform.fb_w), u32::from(self.platform.fb_h));
        let output_geometry = self.scanout_m1_output_geometry(scope);
        let geometry_eligible = match scope {
            DirectScope::Root => scanout_m1_outputs_cover_root(root, &output_geometry),
            DirectScope::Output(output_idx) => {
                scanout_m1_output_is_unoverlapped(output_idx, &output_geometry)
            }
        };
        if !geometry_eligible {
            log::debug!(
                "scanout_m1: source_id={} skipped: invalid {scope:?} output geometry {:?}",
                source_id.as_u64(),
                output_geometry,
            );
            self.pstate
                .scanout_m1
                .entries
                .insert((source_id, scope), ScanoutM1ProbeEntry::rejected());
            self.pstate.scanout_m0.m1_probe_reject =
                self.pstate.scanout_m0.m1_probe_reject.saturating_add(1);
            return;
        }

        let plane_states = scanout_m1_plane_states(&self.platform.outputs, &output_indices, scope);
        let prober = self.pstate.scanout_m1_prober;
        match prober(self, source_id, scope, &plane_states) {
            Ok(entry) => {
                if entry.framebuffer().is_some() {
                    self.pstate.scanout_m0.m1_probe_pass =
                        self.pstate.scanout_m0.m1_probe_pass.saturating_add(1);
                } else {
                    self.pstate.scanout_m0.m1_probe_reject =
                        self.pstate.scanout_m0.m1_probe_reject.saturating_add(1);
                }
                self.pstate
                    .scanout_m1
                    .entries
                    .insert((source_id, scope), entry);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.pstate.scanout_m0.m1_gate_reject_import = self
                    .pstate
                    .scanout_m0
                    .m1_gate_reject_import
                    .saturating_add(1);
            }
            Err(error) => {
                log::warn!(
                    "scanout_m1: import/probe failed source_id={} scope={scope:?}: {error}",
                    source_id.as_u64(),
                );
                self.pstate
                    .scanout_m1
                    .entries
                    .insert((source_id, scope), ScanoutM1ProbeEntry::rejected());
                self.pstate.scanout_m0.m1_probe_error =
                    self.pstate.scanout_m0.m1_probe_error.saturating_add(1);
            }
        }
    }

    /// Every active output's placement within the root, as M1 validates it
    /// for `scope`: Root compares the CRTC's current mode, Output the
    /// selected one.
    pub(in crate::kms::render::backend) fn scanout_m1_output_geometry(
        &self,
        scope: DirectScope,
    ) -> Vec<ScanoutM1OutputGeometry> {
        self.platform
            .outputs
            .iter()
            .map(|layout| {
                let (mode_width, mode_height) = match scope {
                    DirectScope::Root => layout.output.mode.size(),
                    DirectScope::Output(_) => {
                        (layout.output.picked.width, layout.output.picked.height)
                    }
                };
                ScanoutM1OutputGeometry {
                    x: layout.x,
                    y: layout.y,
                    width: u32::from(layout.width),
                    height: u32::from(layout.height),
                    mode_width: u32::from(mode_width),
                    mode_height: u32::from(mode_height),
                }
            })
            .collect()
    }

    /// The scanout descriptor of `source_id`: a promoted server-owned
    /// source's, or its imported dma-buf layout, with a dup of the fd.
    pub(in crate::kms::render::backend) fn scanout_m1_descriptor(
        &self,
        source_id: DrawableId,
    ) -> Result<ScanoutSourceDescriptor, ScanoutM1DescriptorError> {
        if let Some(descriptor) = self.pstate.server_scanout.descriptor(source_id) {
            return match descriptor.fd.try_clone() {
                Ok(fd) => Ok(ScanoutSourceDescriptor {
                    fd,
                    layout: descriptor.layout,
                }),
                Err(error) => Err(ScanoutM1DescriptorError::FdDup {
                    layout: descriptor.layout,
                    error,
                }),
            };
        }
        let drawable = self
            .store
            .get(source_id)
            .ok_or(ScanoutM1DescriptorError::Unavailable)?;
        let metadata = drawable
            .storage
            .imported_dmabuf
            .as_ref()
            .ok_or(ScanoutM1DescriptorError::Unavailable)?;
        // A layout guessed by the legacy import path (`PixmapFromBuffer`)
        // cannot safely reach KMS: scanning a tiled buffer out as linear
        // puts garbage on the display, and `AddFB2` may well accept it.
        if metadata.implicit_layout {
            return Err(ScanoutM1DescriptorError::Unavailable);
        }
        let plane = metadata
            .planes
            .first()
            .ok_or(ScanoutM1DescriptorError::Unavailable)?;
        let fd = drawable
            .storage
            .imported_drawable
            .as_ref()
            .and_then(|imported| imported.imported_dma_buf_fd())
            .ok_or(ScanoutM1DescriptorError::Unavailable)?;
        let layout = ScanoutSourceLayout {
            fourcc: metadata.fourcc,
            vk_format: metadata.vk_format,
            depth: metadata.depth,
            bpp: metadata.bpp,
            width: metadata.width,
            height: metadata.height,
            plane_count: metadata.planes.len(),
            offset: plane.offset,
            pitch: plane.pitch,
            modifier: metadata.modifier,
        };
        match fd.try_clone_to_owned() {
            Ok(fd) => Ok(ScanoutSourceDescriptor { fd, layout }),
            Err(error) => Err(ScanoutM1DescriptorError::FdDup { layout, error }),
        }
    }

    /// The size a source must have to be scanned out by `scope`: the root
    /// for Root, the output for Output. `None` for a missing output.
    pub(in crate::kms::render::backend) fn scanout_m1_scope_size(
        &self,
        scope: DirectScope,
    ) -> Option<(u32, u32)> {
        match scope {
            DirectScope::Root => {
                Some((u32::from(self.platform.fb_w), u32::from(self.platform.fb_h)))
            }
            DirectScope::Output(output_idx) => self
                .platform
                .outputs
                .get(output_idx)
                .map(|output| (u32::from(output.width), u32::from(output.height))),
        }
    }

    /// M1's exact `AddFB2` + primary-plane atomic `TEST_ONLY` of one dma-buf
    /// on `planes` (the planes of `output_indices`). `None` without a
    /// primary DRM device.
    pub(in crate::kms::render::backend) fn scanout_m1_test_only(
        &self,
        fd: std::os::fd::BorrowedFd<'_>,
        layout: &ScanoutSourceLayout,
        planes: &[crate::drm::modeset::DirectScanoutPlaneState<'_>],
        output_indices: &[usize],
    ) -> Option<io::Result<crate::drm::modeset::DirectScanoutTestResult>> {
        let primary = self.platform.primary_device()?;
        let cursor_positions = self
            .platform
            .cursor_plane_atomic_positions_for_outputs(output_indices);
        let import = self.pstate.scanout_m1_importer;
        Some(
            import(Rc::clone(&primary.device), fd, layout).and_then(|framebuffer| {
                crate::drm::modeset::test_direct_scanout_framebuffer(
                    framebuffer,
                    &primary.device,
                    primary.atomic_commit_sink.as_ref(),
                    planes,
                    &cursor_positions,
                )
            }),
        )
    }

    /// Production [`ScanoutM1Importer`](crate::kms::render::backend::ScanoutM1Importer).
    pub(in crate::kms::render::backend) fn import_scanout_m1_framebuffer_live(
        device: Rc<crate::drm::Device>,
        fd: std::os::fd::BorrowedFd<'_>,
        layout: &ScanoutSourceLayout,
    ) -> io::Result<crate::drm::modeset::DirectScanoutProbeFramebuffer> {
        crate::drm::modeset::import_direct_scanout_framebuffer(
            device,
            fd,
            u32::from(layout.width),
            u32::from(layout.height),
            layout.fourcc,
            layout.modifier,
            layout.offset,
            layout.pitch,
        )
    }

    /// An accepted M1 entry for `layout`, with the async-flip shape over the
    /// CRTCs of `planes`.
    pub(in crate::kms::render::backend) fn scanout_m1_accepted_entry(
        framebuffer: crate::drm::modeset::DirectScanoutProbeFramebuffer,
        layout: &ScanoutSourceLayout,
        planes: &[crate::drm::modeset::DirectScanoutPlaneState<'_>],
    ) -> ScanoutM1ProbeEntry {
        let mut crtcs: Vec<_> = planes.iter().map(|state| state.output.crtc).collect();
        crtcs.sort_by_key(|crtc| u32::from(*crtc));
        ScanoutM1ProbeEntry::accepted_with_shape(
            framebuffer,
            crate::drm::async_flip::AsyncShape {
                fourcc: layout.fourcc,
                modifier: layout.modifier,
                pitch: layout.pitch,
                width: u32::from(layout.width),
                height: u32::from(layout.height),
                crtcs,
                direct: true,
            },
        )
    }

    pub(in crate::kms::render::backend) fn probe_direct_scanout_m1_live(
        &self,
        source_id: DrawableId,
        scope: DirectScope,
        planes: &[crate::drm::modeset::DirectScanoutPlaneState<'_>],
    ) -> io::Result<ScanoutM1ProbeEntry> {
        use std::os::fd::AsFd as _;

        let descriptor = match self.scanout_m1_descriptor(source_id) {
            Ok(descriptor) => descriptor,
            Err(ScanoutM1DescriptorError::Unavailable) => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "M1 source has no scanout descriptor",
                ));
            }
            Err(ScanoutM1DescriptorError::FdDup { layout, error }) => {
                return Err(io::Error::other(format!(
                    "M1 dma-buf dup failed ({layout:?}): {error}"
                )));
            }
        };
        let layout = descriptor.layout;
        let Some(expected) = self.scanout_m1_scope_size(scope) else {
            return Ok(ScanoutM1ProbeEntry::rejected());
        };
        let output_modifiers: Vec<&[u64]> = planes
            .iter()
            .map(|state| state.output.scanout_modifiers.as_slice())
            .collect();
        if let Err(reason) = scanout_m1_validate(&layout, expected, &output_modifiers) {
            log::debug!(
                "scanout_m1: source_id={} {scope:?} skipped ({reason:?}): {layout:?}",
                source_id.as_u64(),
            );
            return Ok(ScanoutM1ProbeEntry::rejected());
        }
        let output_indices = self.direct_scope_outputs(scope);
        let Some(result) =
            self.scanout_m1_test_only(descriptor.fd.as_fd(), &layout, planes, &output_indices)
        else {
            return Ok(ScanoutM1ProbeEntry::rejected());
        };
        match result? {
            crate::drm::modeset::DirectScanoutTestResult::Accepted(framebuffer) => Ok(
                Self::scanout_m1_accepted_entry(framebuffer, &layout, planes),
            ),
            crate::drm::modeset::DirectScanoutTestResult::Rejected(error) => {
                log::debug!("scanout_m1: TEST_ONLY rejected {scope:?}: {error}");
                Ok(ScanoutM1ProbeEntry::rejected())
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
        root_rect: vk::Rect2D,
    ) -> bool {
        direct_shape_chain_covers_root(
            &self.windows,
            self.core.window_id,
            &self.core.shape_bounding,
            &self.core.shape_clip,
            leaf_xid,
            root_rect,
        )
    }

    pub(in crate::kms::render::backend) fn direct_shape_chain_hides_root(
        &self,
        leaf_xid: u32,
        root_rect: vk::Rect2D,
    ) -> bool {
        direct_shape_chain_hides_root(
            &self.windows,
            self.core.window_id,
            &self.core.shape_bounding,
            &self.core.shape_clip,
            leaf_xid,
            root_rect,
        )
    }

    /// Whether a retained direct frame now presents through a Bounding or
    /// Clip shape that no longer covers the root. A compositor may shape the
    /// COW without presenting again, so the shape change itself must unflip.
    pub(in crate::kms::render::backend) fn direct_frames_shaped_off_root(&self) -> bool {
        let scope = DirectScope::Root;
        let Some(owner) = self.pstate.direct.get(scope) else {
            return false;
        };
        let rect = self.direct_scope_rect(scope);
        [
            owner.pending.as_ref(),
            owner.queued_successor.as_ref(),
            owner.current.as_ref(),
        ]
        .into_iter()
        .flatten()
        .any(|frame| !self.direct_shape_chain_covers_root(frame.candidate.paint_dst_host_xid, rect))
    }

    /// Whether an unredirected Present target is still the window which the
    /// scene would show for the whole root. Direct scanout bypasses that
    /// scene, so a mapped top-level raised above the candidate (or a workspace
    /// switch which unmaps it) must keep later Presents composed.
    pub(in crate::kms::render::backend) fn unredirected_direct_scene_eligible(
        &self,
        leaf_xid: u32,
        direct_rect: vk::Rect2D,
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
        let rect_x = direct_rect.offset.x;
        let rect_y = direct_rect.offset.y;
        let rect_right =
            rect_x.saturating_add(i32::try_from(direct_rect.extent.width).unwrap_or(i32::MAX));
        let rect_bottom =
            rect_y.saturating_add(i32::try_from(direct_rect.extent.height).unwrap_or(i32::MAX));
        let candidate_right = i32::from(candidate.x) + i32::from(candidate.width);
        let candidate_bottom = i32::from(candidate.y) + i32::from(candidate.height);
        let covers_rect = i32::from(candidate.x) <= rect_x
            && i32::from(candidate.y) <= rect_y
            && candidate_right >= rect_right
            && candidate_bottom >= rect_bottom;
        if !candidate.mapped || !covers_rect {
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
                        && i32::from(geometry.x) < rect_right
                        && i32::from(geometry.y) < rect_bottom
                        && i32::from(geometry.x) + i32::from(geometry.width) > rect_x
                        && i32::from(geometry.y) + i32::from(geometry.height) > rect_y
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
        let direct_scope = self.direct_candidate_scope(candidate, target);
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
        let crtc_eligible = direct_scope.is_some_and(|scope| {
            self.direct_present_crtc_eligible(scope, candidate.crtc_id, candidate.crtc_epoch)
        });
        let scanout_allowed = self.scanout_allowed();
        let kms_outputs_active = self.kms_outputs_active;
        let scope_outputs = direct_scope
            .map(|scope| self.direct_scope_outputs(scope))
            .unwrap_or_default();
        let cursor_mode = self.scene.cursor_mode_for_outputs(&scope_outputs);
        let cursor_hw = self.direct_cursor_eligible_for(&scope_outputs);
        let root_overlay_empty = self.direct_root_overlay_empty_for_outputs(&scope_outputs);
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
        self.observe_server_scanout_candidate(candidate, source_id, leaf_id, paint_target, target);
        self.maybe_probe_scanout_m1(source_id, target, coverage, direct_scope, candidate);
        let promoted =
            source_id.is_some_and(|id| self.pstate.server_scanout.descriptor(id).is_some());
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
        if !imported && !promoted {
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
            let server = std::mem::take(&mut diag.server);
            let mut unflip_reasons: Vec<_> = server.unflip_reasons.iter().collect();
            unflip_reasons.sort_unstable();
            log::debug!(
                "scanout_server_summary candidates={} promote[attempts={} ok={} fail={} \
                 us_max={} present_us_max={}] reject[exported={} cap_fail={} cap_success={} \
                 cap_live={}] export[deferred={} defer_us_max={} deadline_teardown={} \
                 deadline_fail={}] probe[accept={} reject={}] presents[direct_imported={} \
                 direct_server={} copy={}] fence[explicit={} none={} cpu_wait={}] \
                 direct_flush[added={} reused={}] unflip_reasons={unflip_reasons:?}",
                server.candidates,
                server.promote_attempts,
                server.promote_ok,
                server.promote_fail,
                server.promote_us_max,
                server.promote_present_us_max,
                server.reject_exported,
                server.reject_cap_fail,
                server.reject_cap_success,
                server.reject_cap_live,
                server.export_deferred,
                server.export_defer_us_max,
                server.export_deadline_teardown,
                server.export_deadline_fail,
                server.probe_accept,
                server.probe_reject,
                server.presents_direct_imported,
                server.presents_direct_server,
                server.presents_copy,
                server.fence_explicit,
                server.fence_none,
                server.fence_cpu_wait,
                server.direct_flush_added,
                server.direct_flush_reused,
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

    /// Count one Present's outcome: a direct flip (by source origin) or Copy.
    pub(in crate::kms::render::backend) fn note_present_outcome(
        &mut self,
        src_host_xid: u32,
        direct: bool,
    ) {
        let imported = self
            .store
            .lookup(src_host_xid)
            .and_then(|id| self.store.get(id))
            .is_some_and(|drawable| drawable.storage.imported_dmabuf.is_some());
        let server = &mut self.pstate.scanout_m0.server;
        let counter = match (direct, imported) {
            (true, true) => &mut server.presents_direct_imported,
            (true, false) => &mut server.presents_direct_server,
            (false, _) => &mut server.presents_copy,
        };
        *counter = counter.saturating_add(1);
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
        ) || (self.pstate.direct.root().unflip_reason == Some("tearing_cursor_handoff")
            && !self
                .pstate
                .direct
                .root()
                .unflip_outputs_to_replace
                .is_empty())
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
        let cursor_compose_blocked = self
            .tearing_cursor_compose_deadline(self.monotonic_now())
            .is_some();
        let now = self.monotonic_now();
        let live_direct_vrr_wanted = self.live_direct_vrr_wanted_by_output();
        let cursor_eligible_outputs = self.scene_cursor_eligible_outputs();
        (self.scene_has_ready_scheduled_output() && !cursor_compose_blocked)
            || (!cursor_compose_blocked
                && self.platform.has_eligible_atomic_cursor_move_for_outputs(
                    now,
                    &live_direct_vrr_wanted,
                    &cursor_eligible_outputs,
                ))
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
        self.dispatch_page_flip_events(drm_fd);
    }

    /// The normal page-flip/sequence event handler for one DRM fd. Also
    /// driven by a per-CRTC RANDR change while it waits for that CRTC's
    /// events, so every other CRTC's events retire exactly as they would
    /// from the core loop.
    pub(super) fn dispatch_page_flip_events(&mut self, drm_fd: std::os::fd::RawFd) {
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
            let scope = self.pstate.direct.scope_for_output(output_idx);
            let queued_successor_before_event = scope
                .and_then(|scope| self.pstate.direct.get(scope))
                .is_some_and(|owner| owner.queued_successor.is_some());
            let matched_event_id = self
                .platform
                .note_tearing_async_flip_event(output_idx, timestamps);
            if timestamps.commit_id.is_some() && matched_event_id.is_none() {
                continue;
            }
            if queued_successor_before_event
                && let Some(scope) = scope
                && let Some(owner) = self.pstate.direct.get_mut(scope)
                && owner.direct_cursor_refresh_pending.contains(&output_idx)
            {
                let count = owner
                    .direct_cursor_refresh_successor_deferrals
                    .entry(output_idx)
                    .or_default();
                *count = count
                    .saturating_add(1)
                    .min(DIRECT_CURSOR_RETRY_AFTER_SUCCESSOR_EVENTS);
            }
            let cursor_commit_retired =
                self.record_atomic_cursor_commit_retirement(output_idx, matched_event_id);
            if cursor_commit_retired && let Some(scope) = scope {
                let owner_ready = self.pstate.direct.get(scope).is_some_and(|owner| {
                    owner.pending.is_none() && owner.queued_successor.is_some()
                });
                if owner_ready {
                    self.retry_due_direct_cursor_refreshes_before_successor(scope);
                    self.submit_queued_direct_successor(scope);
                }
                if self
                    .pstate
                    .direct
                    .get(scope)
                    .is_some_and(|owner| owner.direct_cursor_refresh_pending.contains(&output_idx))
                {
                    self.retry_pending_direct_cursor_refresh(scope, output_idx, false);
                }
            }
            let direct_retired = self.retire_direct_output(output_idx, matched_event_id, clock);
            if direct_retired
                && !queued_successor_before_event
                && let Some(scope) = scope
            {
                self.retry_pending_direct_cursor_refresh(scope, output_idx, true);
            }
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
            // A CRTC waiting for a per-CRTC modeset takes no new gamma-only
            // commit; the pending LUT is applied after the modeset.
            if self.pstate.crtc_config_gated_crtc.is_some_and(|gated| {
                self.platform.outputs.get(output_idx).is_some_and(|output| {
                    crate::kms::render::platform::CrtcKey::for_output(output) == gated
                })
            }) {
                continue;
            }
            let vrr_clear_waiting_for_flip = self
                .platform
                .outputs
                .get(output_idx)
                .is_some_and(|layout| layout.output.vrr_clear_waiting_for_flip.get());
            let direct_successor_owns_next_frame = scope
                .and_then(|scope| self.pstate.direct.get(scope))
                .is_some_and(|owner| owner.pending.is_some() || owner.queued_successor.is_some());
            if !self.gamma_output_has_queued_scanout(output_idx)
                || (vrr_clear_waiting_for_flip && !direct_successor_owns_next_frame)
            {
                self.retry_pending_crtc_property_after_flip(output_idx, self.monotonic_now());
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
    ) -> bool {
        let retired = self
            .platform
            .retire_atomic_cursor_commit_for_output(output_idx, matched_event_id);
        let Some(layout) = self.platform.outputs.get(output_idx) else {
            return retired;
        };
        if let Some(device) = self.platform.device_for_output(&layout.key) {
            device
                .atomic_commit_sink
                .note_cursor_commit_retired(layout.output.crtc, std::time::Instant::now());
        }
        retired
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
        self.request_tearing_cursor_handoff();
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
        let live_direct_vrr_wanted = self.live_direct_vrr_wanted_by_output();
        let allow_kms_timers = self.scanout_allowed() && self.kms_outputs_active;
        let direct_cursor_retry_deadline = if allow_kms_timers {
            self.pstate
                .direct
                .active_scopes()
                .into_iter()
                .filter_map(|scope| self.pstate.direct.get(scope))
                .filter(|owner| {
                    owner.pending.is_none()
                        && owner.queued_successor.is_none()
                        && !owner.unflip_requested
                })
                .flat_map(|owner| owner.direct_cursor_refresh_pending.iter().copied())
                .filter_map(|output_idx| self.platform.direct_cursor_retry_deadline(output_idx))
                .min()
        } else {
            None
        };
        let scene_deadline = if allow_kms_timers {
            if self.scene_wants_compose() {
                let cursor_eligible_outputs = self.scene_cursor_eligible_outputs();
                if self.scene_has_ready_scheduled_output()
                    || self.platform.has_eligible_atomic_cursor_move_for_outputs(
                        now,
                        &live_direct_vrr_wanted,
                        &cursor_eligible_outputs,
                    )
                {
                    Some(now)
                } else {
                    self.scheduled_scene_retry_deadline()
                }
            } else {
                self.tearing_cursor_compose_deadline(now)
                    .or_else(|| self.scheduled_scene_retry_deadline())
            }
        } else {
            None
        };
        let gamma_deadline = allow_kms_timers
            .then(|| {
                self.platform
                    .outputs
                    .iter()
                    .filter_map(|layout| {
                        (layout.output.pending_gamma_lut.borrow().is_some())
                            .then_some(layout.output.pending_gamma_deadline.get())
                            .flatten()
                    })
                    .min()
            })
            .flatten();
        let vrr_clear_deadline = allow_kms_timers
            .then(|| {
                self.platform
                    .outputs
                    .iter()
                    .filter_map(|layout| {
                        (!layout.output.vrr_clear_waiting_for_flip.get())
                            .then_some(layout.output.vrr_clear_retry_deadline.get())
                            .flatten()
                    })
                    .min()
            })
            .flatten();
        let vrr_cursor_deadline = allow_kms_timers
            .then(|| self.platform.vrr_cursor_deadline(&live_direct_vrr_wanted))
            .flatten();
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
            .chain(direct_cursor_retry_deadline)
            .chain(gamma_deadline)
            .chain(vrr_clear_deadline)
            .chain(vrr_cursor_deadline)
            .chain(source_wait_deadline)
            .chain(rescan_deadline)
            .chain(self.server_scanout_export_deadline())
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
        let now = self.monotonic_now();
        for output_idx in 0..self.platform.outputs.len() {
            let property_retry_due = {
                let output = &self.platform.outputs[output_idx].output;
                (output.pending_gamma_lut.borrow().is_some()
                    && output
                        .pending_gamma_deadline
                        .get()
                        .is_some_and(|deadline| now >= deadline))
                    || (!output.vrr_clear_waiting_for_flip.get()
                        && output
                            .vrr_clear_retry_deadline
                            .get()
                            .is_some_and(|deadline| now >= deadline))
            };
            if property_retry_due {
                self.retry_pending_crtc_property_after_flip(output_idx, now);
            }
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
        self.retry_due_direct_cursor_refreshes_on_timer();
        // Check visibility and current scene ownership before the held-direct
        // return below. Pointer motion from a dead zone wakes this pass.
        self.prepare_tearing_scene_pass();
        let active_scopes = self.pstate.direct.active_scopes();
        for &scope in &active_scopes {
            if let DirectScope::Output(output_idx) = scope
                && !self.direct_output_frames_scene_eligible(output_idx)
            {
                self.request_direct_unflip_for(scope, "composite_tick_output_scene_changed");
            }
        }
        // Preserve Root's existing successor priority; Output owners have
        // already passed the per-output scene check above.
        for &scope in &active_scopes {
            self.submit_queued_direct_successor(scope);
        }
        if !active_scopes.is_empty() {
            use crate::kms::render::scene::CursorPlaneMode;
            if self.platform.any_output_transformed() {
                self.request_direct_unflip("composite_tick_crtc_transform");
            } else {
                for &scope in &active_scopes {
                    let outputs = self.direct_scope_outputs(scope);
                    let cursor_mode = self.scene.cursor_mode_for_outputs(&outputs);
                    let root_overlay_empty = self.direct_root_overlay_empty_for_outputs(&outputs);
                    if !matches!(cursor_mode, CursorPlaneMode::Hw | CursorPlaneMode::Hidden)
                        || !root_overlay_empty
                    {
                        let reason = match cursor_mode {
                            CursorPlaneMode::Mixed => "composite_tick_mixed_cursor",
                            CursorPlaneMode::Sw => "composite_tick_software_or_hidden_cursor",
                            CursorPlaneMode::Hw | CursorPlaneMode::Hidden => {
                                "composite_tick_root_overlay"
                            }
                        };
                        self.request_direct_unflip_for(scope, reason);
                    }
                }
            }
            for scope in active_scopes {
                let Some(owner) = self.pstate.direct.get(scope) else {
                    continue;
                };
                if owner.pending.is_some() || !owner.unflip_awaiting_outputs.is_empty() {
                    continue;
                }
                if owner.hold_direct && !owner.unflip_requested {
                    if owner.current.is_some() && owner.queued_successor.is_none() {
                        let live_direct_vrr_wanted = self.live_direct_vrr_wanted_by_output();
                        for output_idx in self.direct_scope_outputs(scope) {
                            match self.platform.submit_atomic_cursor_move_for_output(
                                output_idx,
                                self.monotonic_now(),
                                live_direct_vrr_wanted
                                    .get(output_idx)
                                    .copied()
                                    .unwrap_or(false),
                            ) {
                                Ok(outcome) => self.handle_cursor_move_outcome(outcome),
                                Err(error) => log::warn!(
                                    "render maybe_composite: cursor-only atomic commit failed for output {output_idx}: {error}"
                                ),
                            }
                        }
                    }
                    continue;
                }
                if owner.current.is_some()
                    && owner.unflip_requested
                    && !owner.degraded_composed_unflip
                {
                    let submitted = if self.submit_fresh_composed_unflip(scope) {
                        Ok(true)
                    } else {
                        self.submit_composed_unflip(scope)
                    };
                    match submitted {
                        Ok(true) => {
                            self.pstate
                                .tearing
                                .note_composed_submit(self.monotonic_now());
                        }
                        Ok(false) => {}
                        Err(error) => {
                            log::error!(
                                "scanout_m2: composed unflip for {scope:?} failed: {error}; degrading to per-output composed flips"
                            );
                            let replacement_outputs =
                                self.direct_scope_outputs(scope).into_iter().collect();
                            if let Some(owner) = self.pstate.direct.get_mut(scope) {
                                owner.unflip_outputs_to_replace = replacement_outputs;
                                owner.unflip_awaiting_outputs.clear();
                                owner.degraded_composed_unflip = true;
                            }
                            match scope {
                                DirectScope::Root => self.scene.mark_scene_structure_dirty(),
                                DirectScope::Output(output_idx) => {
                                    self.scene.mark_scene_structure_dirty_for_output(output_idx)
                                }
                            }
                        }
                    }
                } else if self.pstate.direct.get(scope).is_some_and(|owner| {
                    owner.degraded_composed_unflip && !owner.unflip_outputs_to_replace.is_empty()
                }) {
                    match scope {
                        DirectScope::Root => self.scene.mark_scene_structure_dirty(),
                        DirectScope::Output(output_idx) => {
                            self.scene.mark_scene_structure_dirty_for_output(output_idx)
                        }
                    }
                }
            }
        }
        let skip_outputs = self.scene_skip_outputs();
        if skip_outputs.iter().all(|skip| *skip) {
            // Root direct ownership still holds the complete output set, while
            // per-output owners leave unrelated CRTCs available above.
            self.drain_render_telemetry();
            self.telemetry.maybe_emit(self.engine.pending_count());
            self.drain_paint_submit_telemetry();
            return Ok(());
        }
        let vrr_before = self.vrr_committed_snapshot();
        let composed_outputs = if self
            .tearing_cursor_compose_deadline(self.monotonic_now())
            .is_some()
        {
            None
        } else {
            self.compose_tick(&skip_outputs)
        };
        self.log_vrr_committed_changes(&vrr_before, None, "inherited");
        // `scene.tick` drains already-signalled copied completions itself,
        // without entering backend_scanout_on_scanout_render_completion.
        self.record_accepted_degraded_unflip_replacements();
        if let Some(composed) = &composed_outputs {
            if self.pstate.direct.root().reentry_blocked_until_composed
                && composed.len() == self.platform.outputs.len()
            {
                self.pstate.direct.root_mut().reentry_blocked_until_composed = false;
                log::debug!(
                    "scanout_m2: composed fallback submitted; root re-entry barrier cleared"
                );
            }
            for (output_idx, owner) in self.pstate.direct.outputs.iter_mut().enumerate() {
                if owner.reentry_blocked_until_composed && composed.contains(&output_idx) {
                    owner.reentry_blocked_until_composed = false;
                    log::debug!(
                        "scanout_m2: output {output_idx} composed fallback submitted; re-entry barrier cleared"
                    );
                }
            }
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
        let live_direct_vrr_wanted = self.live_direct_vrr_wanted_by_output();
        for output_idx in 0..self.platform.outputs.len() {
            if self.output_is_direct(output_idx)
                || scene_submitted_outputs.contains(&output_idx)
                || self.scene.has_pending_page_flip(output_idx)
                || self.scene.output_submit_retry_pending(output_idx)
                || (self.scene_output_wants_scheduled_compose(output_idx)
                    && self
                        .scene
                        .output_ready_for_submit(output_idx, &self.platform))
            {
                continue;
            }
            match self.platform.submit_atomic_cursor_move_for_output(
                output_idx,
                self.monotonic_now(),
                live_direct_vrr_wanted
                    .get(output_idx)
                    .copied()
                    .unwrap_or(false),
            ) {
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
