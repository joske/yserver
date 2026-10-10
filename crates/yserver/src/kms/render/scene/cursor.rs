use super::*;

/// Pure classifier driving the steady-state cursor-mode decision —
/// extracted from `SceneCompositor::cursor_mode` so the dual-output
/// case (and any future N-output topology) can be unit-tested with
/// synthetic mode arrays. Callers MUST short-circuit Mixed for
/// pending transitions BEFORE invoking this helper; this fn only
/// looks at last-frame modes.
///
/// Classification rules (load-bearing — see scene.rs:686 docstring):
/// - `Hidden` outputs are NEUTRAL (cursor isn't on them, the
///   per-CRTC visible check in `cursor_plane_move` skips them).
/// - `Hw` outputs vote for the fast path.
/// - `Sw` / `SwPending` outputs need scene-compose updates for cursor position
///   (the sprite is part of the compose draw list).
/// - Any mix of `Hw` and `Sw` is `Mixed` so the SW cursor doesn't
///   desync from the eventual plane bind during a transition.
pub(super) fn classify_cursor_mode_from_per_output(
    modes: impl IntoIterator<Item = OutputCursorMode>,
) -> CursorPlaneMode {
    let mut any_hw = false;
    let mut any_sw_like = false;
    for m in modes {
        match m {
            OutputCursorMode::Hw => any_hw = true,
            OutputCursorMode::Sw { .. } | OutputCursorMode::SwPending => any_sw_like = true,
            OutputCursorMode::Hidden => {}
        }
    }
    match (any_hw, any_sw_like) {
        (true, false) => CursorPlaneMode::Hw,
        (false, _) => CursorPlaneMode::Sw,
        (true, true) => CursorPlaneMode::Mixed,
    }
}

pub(super) fn cursor_output_needs_sprite_retry(
    mode: OutputCursorMode,
    pending: impl IntoIterator<Item = Option<CursorTransition>>,
) -> bool {
    matches!(mode, OutputCursorMode::Hw)
        || pending
            .into_iter()
            .any(|transition| matches!(transition, Some(CursorTransition::ShowOnRetire { .. })))
}

impl SceneBuild {
    pub(super) fn omit_software_cursor_for_hide(&mut self) {
        if let Some((draw_index, sampled_index)) = self.software_cursor_tail.take() {
            debug_assert_eq!(self.scene.draws.len(), draw_index + 1);
            debug_assert_eq!(self.sampled_ids.len(), sampled_index + 1);
            let cursor_id = self.sampled_ids[sampled_index];
            self.presented_ids.retain(|id| *id != cursor_id);
            self.pieces_ids.retain(|id| *id != cursor_id);
            self.scene.draws.truncate(draw_index);
            self.sampled_ids.truncate(sampled_index);
        }
        self.new_cursor_rect = None;
        self.cursor_record_version = None;
    }
}

impl SceneCompositor {
    /// Put the pixels a software cursor covers back into `bytes`, a tight
    /// BGRA8 read of `read` from compose image `image` (`cursor_save`).
    pub(crate) fn restore_under_cursor(
        &self,
        image: vk::Image,
        read: vk::Rect2D,
        bytes: &mut [u8],
    ) {
        if let Some(inner) = self.inner.as_ref() {
            for o in &inner.outputs {
                o.cursor_saves.restore(image, read, bytes);
            }
        }
    }

    /// Stage 3f.8: register the software cursor sprite after the
    /// backend has uploaded its pixel data. Idempotent — a later
    /// `define_cursor` flow (Stage 4) can swap the entry. Drops to
    /// a no-op on the stub fixture.
    pub(crate) fn register_cursor(&mut self, entry: CursorEntry) {
        if let Some(inner) = self.inner.as_mut() {
            inner.cursor = Some(entry);
            self.note_structure_change();
        }
    }

    /// Drop the cursor entry (XFIXES `HideCursor`). `build_scene` then
    /// assigns `CursorAssignment::Hidden` on every output, so the next
    /// tick erases a SW sprite and detaches a bound HW plane on retire.
    /// `register_cursor` restores it.
    pub(crate) fn clear_cursor(&mut self) {
        if let Some(inner) = self.inner.as_mut()
            && inner.cursor.take().is_some()
        {
            self.note_structure_change();
        }
    }

    /// Stage 5 Phase D — cursor-plane mode aggregate query for the
    /// pointer fast path. Returns `Hw` ONLY when every active
    /// output has retired its Sw→Hw transition AND no PendingAck
    /// carries an in-flight cursor transition. Mixed-state
    /// (transition pending on any output, or a heterogeneous
    /// mix) returns `Mixed`; the fast path falls back to scene
    /// wake until the plane is fully consistent.
    pub(crate) fn cursor_mode(&self) -> CursorPlaneMode {
        let Some(inner) = self.inner.as_ref() else {
            return CursorPlaneMode::Sw;
        };
        for output in &inner.outputs {
            // Any pending transition on any output forces Mixed —
            // the fast path must not move the plane until every
            // ShowOnRetire / HideOnRetire has applied.
            if output
                .pending_acks
                .iter()
                .any(|a| a.cursor_transition.is_some())
            {
                return CursorPlaneMode::Mixed;
            }
            if output.force_show_retry_version.is_some() {
                return CursorPlaneMode::Mixed;
            }
        }
        classify_cursor_mode_from_per_output(inner.outputs.iter().map(|o| o.last_frame_cursor_mode))
    }

    /// Stage 5 Phase D — steady-state HW sprite-change path. Called
    /// synchronously from the backend's `refresh_effective_cursor`.
    /// Marks each output that already has (or is retiring into) a HW
    /// binding for an output-local upload + full ShowOnRetire retry.
    ///
    /// `bytes` MUST be `width * height * 4` (BGRA8). `Arc` so the
    /// deferred slot can hold the bytes without re-cloning the
    /// `Vec<u8>` from `CursorRecord` per upload.
    pub(crate) fn queue_steady_state_cursor_upload(
        &mut self,
        _platform: &mut PlatformBackend,
        version: u64,
        _width: u16,
        _height: u16,
        _bgra_bytes: std::sync::Arc<Vec<u8>>,
        _hot_x: u16,
        _hot_y: u16,
        _cursor_x: i32,
        _cursor_y: i32,
    ) -> bool {
        let Some(inner) = self.inner.as_mut() else {
            return false;
        };
        // Do not mutate/rebind several cards synchronously as one pseudo-
        // transaction. Each output's next composed retirement performs its
        // own upload+ShowOnRetire on the owning device. That keeps per-device
        // capacities and failures independent and gives the cursor state
        // machine an exact success/failure point for metadata commitment.
        let mut refreshes_hw_binding = false;
        for output in &mut inner.outputs {
            if cursor_output_needs_sprite_retry(
                output.last_frame_cursor_mode,
                output.pending_acks.iter().map(|ack| ack.cursor_transition),
            ) {
                output.force_show_retry_version = Some(version);
                force_cursor_retry_repaint(output);
                refreshes_hw_binding = true;
            }
        }
        self.note_structure_change();
        refreshes_hw_binding
    }
}

// ────────────────────────────────────────────────────────────────
// Per-output compose tick body
// ────────────────────────────────────────────────────────────────

/// Stage 5 Phase D — pure derivation: combine the previous frame's
/// per-output cursor mode with this frame's `CursorAssignment` to
/// produce the transition to queue and the new prev_pos to write on
/// successful retirement.
///
/// Returns `(transition_to_queue, prev_pos_after_retire)`.
///
/// - `transition_to_queue` is `Some(ShowOnRetire)` only on actual
///   mode transitions into HW (`Hidden→Hw` / `Sw→Hw`);
///   `Some(HideOnRetire)` on transitions out (`Hw→Sw` / `Hw→Hidden`).
///   Steady-state same-mode frames produce `None`.
/// - `prev_pos_after_retire` is `Some(Some(pos))` to set, or
///   `Some(None)` to clear, on successful retire. `None` means
///   "leave `OutputSceneState.cursor_prev_pos` as-is". Hw mode
///   doesn't carry an SW prev_pos so the field always clears on
///   `→ Hw`; `Sw` / `Hidden` carry it.
pub(super) fn cursorless_hide_frame_required(
    prev: OutputCursorMode,
    assignment: CursorAssignment,
) -> bool {
    matches!(prev, OutputCursorMode::Hw)
        && matches!(
            assignment,
            CursorAssignment::Sw { .. } | CursorAssignment::Hidden
        )
}

/// Reconcile transactional scene bookkeeping with the owning device's actual
/// kernel-side cursor binding. Eligibility can change while a failed hide or
/// rollback leaves HW visible. In that case a scene mode of Sw/Hidden must not
/// authorize another SW draw: derive an Hw→Sw/Hidden cursorless hide first.
///
/// An observed live binding only upgrades a non-HW predecessor for a desired
/// SW/Hidden assignment. Desired HW still follows scene state so lifecycle or
/// version changes queue a full Show. Conversely, scene HW with no recorded
/// live binding becomes Hidden so desired HW rebinds.
pub(super) fn effective_cursor_prev_mode(
    scene_prev: OutputCursorMode,
    platform_visible: bool,
    assignment: CursorAssignment,
) -> OutputCursorMode {
    if !platform_visible && matches!(scene_prev, OutputCursorMode::Hw) {
        // A successful fast-path rollback may hide the plane before the scene
        // retires another frame. Do not issue a second hide against an
        // already-unbound CRTC; Hidden→Hw still derives a fresh full Show.
        OutputCursorMode::Hidden
    } else if platform_visible
        && !matches!(scene_prev, OutputCursorMode::Hw)
        && matches!(
            assignment,
            CursorAssignment::Sw { .. } | CursorAssignment::Hidden
        )
    {
        OutputCursorMode::Hw
    } else {
        scene_prev
    }
}

#[allow(clippy::type_complexity)]
pub(super) fn derive_cursor_transition(
    prev: OutputCursorMode,
    assignment: CursorAssignment,
) -> (
    Option<CursorTransition>,
    Option<Option<(i32, i32)>>,
    OutputCursorMode,
) {
    match (prev, assignment) {
        (
            OutputCursorMode::Sw { .. } | OutputCursorMode::SwPending | OutputCursorMode::Hidden,
            CursorAssignment::Hw {
                x,
                y,
                record_version,
                hot_x,
                hot_y,
            },
        ) => (
            Some(CursorTransition::ShowOnRetire {
                upload_version: record_version,
                hot_x,
                hot_y,
                x,
                y,
            }),
            Some(None),
            // Mode advances to Hw only AFTER the retire applies
            // the show; the post-retire mode reflects what's on
            // the screen.
            OutputCursorMode::Hw,
        ),
        (OutputCursorMode::Hw, CursorAssignment::Sw { .. }) => (
            Some(CursorTransition::HideOnRetire {
                reveal_sw_after: true,
            }),
            // Phase one is cursorless. Only after hide retires successfully
            // may a Hidden→Sw frame install the SW position/metadata.
            Some(None),
            OutputCursorMode::SwPending,
        ),
        (OutputCursorMode::Hw, CursorAssignment::Hidden) => (
            Some(CursorTransition::HideOnRetire {
                reveal_sw_after: false,
            }),
            Some(None),
            OutputCursorMode::Hidden,
        ),
        (_, CursorAssignment::Sw { pos }) => {
            // Sw → Sw or Hidden → Sw: no transition; advance the
            // per-output `cursor_prev_pos` to where the SW sprite
            // landed this frame so the NEXT frame damages this
            // rect. The transactional rule (codex v4-pass) means
            // failed submits do NOT advance — the OLD prev rect
            // survives and is re-damaged.
            (
                None,
                Some(Some(pos)),
                OutputCursorMode::Sw { prev: Some(pos) },
            )
        }
        (_, CursorAssignment::Hidden) => (None, Some(None), OutputCursorMode::Hidden),
        (OutputCursorMode::Hw, CursorAssignment::Hw { .. }) => (None, None, OutputCursorMode::Hw),
    }
}

pub(super) fn resolve_retired_cursor_state(
    result: CursorTransitionResult,
    desired_mode: OutputCursorMode,
) -> CursorRetireResolution {
    match result {
        CursorTransitionResult::Applied => CursorRetireResolution {
            actual_mode: desired_mode,
            commit_desired_metadata: true,
            clear_presented_metadata: false,
            force_repaint: false,
        },
        CursorTransitionResult::Hidden => CursorRetireResolution {
            actual_mode: OutputCursorMode::Hidden,
            commit_desired_metadata: false,
            clear_presented_metadata: true,
            force_repaint: true,
        },
        CursorTransitionResult::HiddenNeedsRepaint => CursorRetireResolution {
            actual_mode: desired_mode,
            commit_desired_metadata: true,
            clear_presented_metadata: false,
            force_repaint: true,
        },
        CursorTransitionResult::Visible => CursorRetireResolution {
            actual_mode: OutputCursorMode::Hw,
            commit_desired_metadata: false,
            clear_presented_metadata: false,
            force_repaint: true,
        },
        CursorTransitionResult::VisibleNeedsShowRetry => CursorRetireResolution {
            actual_mode: OutputCursorMode::Hw,
            commit_desired_metadata: false,
            clear_presented_metadata: false,
            force_repaint: true,
        },
    }
}

pub(super) fn update_force_show_retry_version(
    current: Option<u64>,
    transition: Option<CursorTransition>,
    result: CursorTransitionResult,
    desired_mode: OutputCursorMode,
) -> Option<u64> {
    let attempted = match transition {
        Some(CursorTransition::ShowOnRetire { upload_version, .. }) => Some(upload_version),
        _ => None,
    };
    match (attempted, result) {
        (Some(version), CursorTransitionResult::VisibleNeedsShowRetry) => {
            Some(current.map_or(version, |pending| pending.max(version)))
        }
        (Some(version), CursorTransitionResult::Applied | CursorTransitionResult::Hidden)
            if current == Some(version) =>
        {
            None
        }
        (None, CursorTransitionResult::Applied)
            if !matches!(desired_mode, OutputCursorMode::Hw) =>
        {
            None
        }
        _ => current,
    }
}

pub(super) fn force_cursor_retry_repaint(state: &mut OutputSceneState) {
    state.scene_structure_damage.add(vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent: state.output_extent,
    });
}

pub(super) fn reset_cursor_retry_for_lifecycle(retry: &mut Option<u64>) {
    // drain_all also resets the actual mode to Hidden and invalidates every
    // uploaded plane version. The first post-resume/DPMS frame therefore
    // derives a fresh Hidden→Hw Show using the current cursor record; an old
    // pre-suspend generation must not keep the aggregate mode Mixed forever.
    *retry = None;
}

pub(super) fn reset_cursor_mode_for_lifecycle(mode: &mut OutputCursorMode) {
    *mode = OutputCursorMode::Hidden;
}

pub(super) fn resolve_failed_cursor_upload(
    output_idx: usize,
    prior_visible: bool,
    hide_live_binding: impl FnOnce() -> io::Result<()>,
) -> CursorTransitionResult {
    if !prior_visible {
        // Hidden/Sw→Hw can fail before the plane has ever been bound. There
        // is no rollback to perform; issuing set_cursor2(None) here can itself
        // return EINVAL and falsely manufacture HW ownership.
        return CursorTransitionResult::Hidden;
    }
    match hide_live_binding() {
        Ok(()) => CursorTransitionResult::Hidden,
        Err(error) => {
            log::warn!(
                "render cursor: upload failed and hide_on_crtc({output_idx}) rollback failed: {error}"
            );
            CursorTransitionResult::VisibleNeedsShowRetry
        }
    }
}

pub(super) fn resolve_cursor_hide_on_retire(
    output_idx: usize,
    reveal_sw_after: bool,
    currently_visible: bool,
    hide_live_binding: impl FnOnce() -> io::Result<()>,
) -> CursorTransitionResult {
    if !currently_visible {
        return if reveal_sw_after {
            // The submitted phase-one frame was cursorless. Preserve the
            // one-frame reveal gap and force phase two even though a fast-path
            // rollback already performed the hide.
            CursorTransitionResult::HiddenNeedsRepaint
        } else {
            CursorTransitionResult::Applied
        };
    }
    match hide_live_binding() {
        Ok(()) if reveal_sw_after => CursorTransitionResult::HiddenNeedsRepaint,
        Ok(()) => CursorTransitionResult::Applied,
        Err(error) => {
            log::warn!("render cursor: hide_on_crtc({output_idx}) failed at retire: {error}");
            CursorTransitionResult::Visible
        }
    }
}

pub(super) fn apply_cursor_transition_on_retire(
    inner: &mut SceneCompositorInner,
    output_idx: usize,
    platform: &mut PlatformBackend,
    transition: Option<CursorTransition>,
) -> CursorTransitionResult {
    let Some(t) = transition else {
        return CursorTransitionResult::Applied;
    };
    match t {
        CursorTransition::ShowOnRetire {
            upload_version,
            hot_x,
            hot_y,
            x,
            y,
        } => {
            let prior_visible = platform.cursor_plane_visible_for_output(output_idx);
            // Upload if version doesn't match. `upload_image` is
            // already idempotent-deduplicated by value inside
            // `CursorPlane`, but skipping the FFI when we know the
            // version matches is cheaper. Bytes come from the
            // scene's current `CursorEntry` (cloned-Arc, no copy)
            // when its version matches the transition's. A mismatch never
            // binds stale pixels: the old binding must be hidden successfully
            // or remain authoritative with a version-qualified Show retry.
            let upload_ready = if platform.cursor_plane_uploaded_version_for_output(output_idx)
                != Some(upload_version)
            {
                if let Some(entry) = inner.cursor.as_ref()
                    && entry.record_version == upload_version
                    && let Some(bytes) = entry.bgra_bytes.as_ref()
                {
                    match platform.cursor_plane_upload_image_for_output(
                        output_idx,
                        upload_version,
                        entry.extent.width,
                        entry.extent.height,
                        bytes.as_ref(),
                    ) {
                        Ok(()) => true,
                        Err(e) => {
                            log::warn!(
                                "render cursor: retire-time upload (v{upload_version}) failed: {e}"
                            );
                            false
                        }
                    }
                } else {
                    log::debug!(
                        "render cursor: retire-time upload (v{upload_version}) — \
                         no matching entry bytes; binding with current buffer"
                    );
                    false
                }
            } else {
                true
            };
            if !upload_ready {
                // A steady-HW rebind can fail its per-device upload while an
                // old binding is live. Switch to SW only if detaching that old
                // binding succeeds; otherwise retain actual HW ownership. A
                // never-bound output has nothing to detach and resolves
                // directly to Hidden without probing hide.
                resolve_failed_cursor_upload(output_idx, prior_visible, || {
                    platform.cursor_plane_hide_on_crtc(output_idx)
                })
            } else {
                match platform.cursor_plane_show_on_crtc(output_idx, hot_x, hot_y, x, y) {
                    Ok(()) => CursorTransitionResult::Applied,
                    Err(error) => {
                        let actual = if error.remains_visible() {
                            CursorTransitionResult::VisibleNeedsShowRetry
                        } else {
                            CursorTransitionResult::Hidden
                        };
                        log::warn!(
                            "render cursor: show_on_crtc({output_idx}) failed at retire: {error}"
                        );
                        actual
                    }
                }
            }
        }
        CursorTransition::HideOnRetire { reveal_sw_after } => {
            let currently_visible = platform.cursor_plane_visible_for_output(output_idx);
            resolve_cursor_hide_on_retire(output_idx, reveal_sw_after, currently_visible, || {
                platform.cursor_plane_hide_on_crtc(output_idx)
            })
        }
    }
}

/// Opt-in gate for the per-tick skip/unblock diagnostic. Default OFF:
/// the logging fires on every skip-state transition, which during
/// healthy vsync operation is one line per frame per output — pure
/// noise + CPU unless you're actively chasing a freeze. Set
/// `YSERVER_TICK_SKIP_LOG=1` (or true/yes) to enable it; when unset,
/// `record_tick_skip` / `record_tick_success` are no-ops (no logging,
/// no `last_skip_reason` book-keeping).
/// Xorg refuses the HW cursor while any CRTC is transformed
/// (xf86Cursors.c:569): software on every output, so it scales with the
/// content and never vanishes crossing to an identity output (spec D5).
pub(super) fn hw_cursor_allowed(platform: &PlatformBackend) -> bool {
    !platform.any_output_transformed()
}

pub(super) fn cursor_damage_for_frame(
    last_present_cursor_rect: Option<vk::Rect2D>,
    last_present_cursor_version: Option<u64>,
    new_cursor_rect: Option<vk::Rect2D>,
    new_cursor_version: Option<u64>,
    cursor_transition: Option<CursorTransition>,
) -> RegionSet {
    let mut damage = RegionSet::new();
    let cursor_changed = new_cursor_rect != last_present_cursor_rect
        || cursor_transition.is_some()
        || new_cursor_version != last_present_cursor_version;
    if !cursor_changed {
        return damage;
    }
    if let Some(rect) = last_present_cursor_rect {
        damage.add(rect);
    }
    if let Some(rect) = new_cursor_rect
        && Some(rect) != last_present_cursor_rect
    {
        damage.add(rect);
    }
    damage
}

pub(super) fn cursor_footprint_rect(
    dx: i32,
    dy: i32,
    cursor_w: i32,
    cursor_h: i32,
    layout_w: i32,
    layout_h: i32,
) -> Option<vk::Rect2D> {
    let x0 = dx.max(0);
    let y0 = dy.max(0);
    let x1 = (dx + cursor_w).min(layout_w);
    let y1 = (dy + cursor_h).min(layout_h);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some(vk::Rect2D {
        offset: vk::Offset2D { x: x0, y: y0 },
        extent: vk::Extent2D {
            width: u32::try_from(x1 - x0).unwrap_or(0),
            height: u32::try_from(y1 - y0).unwrap_or(0),
        },
    })
}

/// Copy the rect under the software cursor out of the just-composed
/// `image` into `save.buffer` for root reads (`cursor_save`), leaving `image`
/// back in `COLOR_ATTACHMENT_OPTIMAL` for the cursor draw.
///
/// # Safety
///
/// `cb` is recording, outside a rendering scope.
pub(super) unsafe fn record_cursor_save(
    vk: &crate::kms::vk::device::VkContext,
    cb: vk::CommandBuffer,
    image: vk::Image,
    save: CursorSaveTarget,
) {
    let device = &vk.device;
    let color = vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1);
    let to_src = [vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .src_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::COPY)
        .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
        .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
        .image(image)
        .subresource_range(color)];
    // The previous frame's copy into the same buffer.
    let buffer_waw = [vk::BufferMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COPY)
        .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::COPY)
        .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
        .buffer(save.buffer)
        .size(vk::WHOLE_SIZE)];
    let region = [vk::BufferImageCopy::default()
        .image_subresource(
            vk::ImageSubresourceLayers::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .layer_count(1),
        )
        .image_offset(vk::Offset3D {
            x: save.rect.offset.x,
            y: save.rect.offset.y,
            z: 0,
        })
        .image_extent(vk::Extent3D {
            width: save.rect.extent.width,
            height: save.rect.extent.height,
            depth: 1,
        })];
    let to_color = [vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COPY)
        .src_access_mask(vk::AccessFlags2::TRANSFER_READ)
        .dst_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .dst_access_mask(
            vk::AccessFlags2::COLOR_ATTACHMENT_WRITE | vk::AccessFlags2::COLOR_ATTACHMENT_READ,
        )
        .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
        .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .image(image)
        .subresource_range(color)];
    let to_host = [vk::BufferMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COPY)
        .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::HOST)
        .dst_access_mask(vk::AccessFlags2::HOST_READ)
        .buffer(save.buffer)
        .size(vk::WHOLE_SIZE)];
    unsafe {
        crate::vk_count!(cmd_pipeline_barrier2);
        device.cmd_pipeline_barrier2(
            cb,
            &vk::DependencyInfo::default()
                .image_memory_barriers(&to_src)
                .buffer_memory_barriers(&buffer_waw),
        );
        crate::vk_count!(cmd_copy_image_to_buffer);
        device.cmd_copy_image_to_buffer(
            cb,
            image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            save.buffer,
            &region,
        );
        crate::vk_count!(cmd_pipeline_barrier2);
        device.cmd_pipeline_barrier2(
            cb,
            &vk::DependencyInfo::default()
                .image_memory_barriers(&to_color)
                .buffer_memory_barriers(&to_host),
        );
    }
}
