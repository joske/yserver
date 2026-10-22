use crate::kms::render::platform::{
    CursorFailureDisposition, CursorHideRecovery, CursorMoveOutcome, KmsCursorState, KmsDevice,
    PlatformBackend, TransientCursorFallback,
};
use std::{
    collections::{HashMap, HashSet},
    io,
    rc::Rc,
    time::Instant,
};

/// True iff a cursor-plane ioctl error means the driver does not
/// implement the (legacy) cursor ioctls at all — a permanent,
/// per-driver condition that warrants latching the HW cursor strategy
/// off and falling back to the SW composite path.
///
/// Apple's DCP display driver (Asahi) returns `ENXIO` from
/// `DRM_IOCTL_MODE_CURSOR2`; other atomic-only drivers may return
/// `ENODEV` / `EOPNOTSUPP`. Recoverable / ambiguous errors (`EBUSY`,
/// `EINVAL`, non-OS errors) must NOT latch — `EBUSY` is transient and
/// latching it would needlessly kill the HW cursor on drivers that do
/// support the ioctl (e.g. amdgpu). This mirrors Xorg's modesetting
/// driver, which clears `use_hw_cursor` when the cursor ioctl fails.
pub(super) fn cursor_err_disables_hw(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::ENXIO | libc::ENODEV | libc::EOPNOTSUPP)
    )
}

fn cursor_error_is_transient_fallback(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::InvalidInput
}

fn classify_cursor_failure_pair(
    operation_error: &io::Error,
    rollback_error: Option<&io::Error>,
) -> CursorFailureDisposition {
    if cursor_err_disables_hw(operation_error) || rollback_error.is_some_and(cursor_err_disables_hw)
    {
        CursorFailureDisposition::Permanent
    } else if cursor_error_is_transient_fallback(operation_error)
        || rollback_error.is_some_and(cursor_error_is_transient_fallback)
    {
        CursorFailureDisposition::Transient
    } else {
        CursorFailureDisposition::Unchanged
    }
}

pub(super) fn cursor_dimensions_fit(
    plane_width: u32,
    plane_height: u32,
    width: u32,
    height: u32,
) -> bool {
    width <= plane_width && height <= plane_height
}

impl KmsCursorState {
    pub(crate) fn new() -> Self {
        Self {
            plane: None,
            pending_move: None,
            pending_move_timestamp: None,
            crtc_states: HashMap::new(),
            atomic_positions: HashMap::new(),
            atomic_pending_moves: HashMap::new(),
            atomic_cursor_commits_in_flight: HashMap::new(),
            cursor_retry_waits_for_flip: HashSet::new(),
            cursor_hide_recovery: HashMap::new(),
            permanently_disabled: false,
            initialization_retryable: false,
            headless_deferred: true,
            topology_blocked: false,
            transient_fallback_crtcs: HashMap::new(),
            atomic_cursor: false,
            sprite_signature: None,
        }
    }

    pub(crate) fn hw_bound_on(&self, crtc: ::drm::control::crtc::Handle) -> bool {
        if self.state_path() == crate::kms::cursor_plane::topology::CursorStatePath::Atomic {
            self.crtc_states
                .get(&crtc)
                .is_some_and(|state| state.applied.is_some())
        } else {
            self.plane
                .as_ref()
                .is_some_and(|plane| plane.is_visible_on(crtc))
        }
    }

    fn remember_atomic_position(
        &mut self,
        device_key: crate::platform::drm::DrmDeviceKey,
        crtc: ::drm::control::crtc::Handle,
        x: i32,
        y: i32,
        motion_timestamp: Option<Instant>,
    ) {
        if !self.atomic_cursor {
            return;
        }
        let Some((plane, crtc_x, crtc_y)) = self
            .plane
            .as_ref()
            .and_then(|plane| plane.atomic_position_properties(crtc))
        else {
            return;
        };
        let position = crate::drm::atomic::CursorPlanePosition {
            device_key,
            crtc,
            plane,
            crtc_x,
            crtc_y,
            x,
            y,
            motion_timestamp,
        };
        self.atomic_positions.insert(crtc, position);
        if !self.cursor_hide_recovery.contains_key(&crtc) {
            self.atomic_pending_moves.insert(crtc, position);
        }
    }

    pub(super) fn clear_atomic_position(&mut self, crtc: ::drm::control::crtc::Handle) {
        self.atomic_positions.remove(&crtc);
        self.atomic_pending_moves.remove(&crtc);
    }

    pub(super) fn available_on(&self, crtc: ::drm::control::crtc::Handle) -> bool {
        self.plane.is_some()
            && !self.permanently_disabled
            && !self.topology_blocked
            && self
                .transient_fallback_crtcs
                .get(&crtc)
                .is_none_or(|retry| retry.remaining_sw_retires == 0)
    }

    pub(super) fn note_einval(&mut self, crtc: ::drm::control::crtc::Handle) {
        let retry = self
            .transient_fallback_crtcs
            .entry(crtc)
            .or_insert(TransientCursorFallback {
                remaining_sw_retires: 0,
                failures: 0,
            });
        retry.failures = retry.failures.saturating_add(1);
        let shift = retry.failures.saturating_sub(1).min(3);
        retry.remaining_sw_retires = 1_u8 << shift;
    }

    pub(super) fn note_cursor_success(&mut self, crtc: ::drm::control::crtc::Handle) {
        self.transient_fallback_crtcs.remove(&crtc);
        self.cursor_hide_recovery.remove(&crtc);
    }

    pub(super) fn note_cursor_failure_pair(
        &mut self,
        crtc: ::drm::control::crtc::Handle,
        operation_error: &io::Error,
        rollback_error: Option<&io::Error>,
    ) -> CursorFailureDisposition {
        let disposition = classify_cursor_failure_pair(operation_error, rollback_error);
        match disposition {
            CursorFailureDisposition::Unchanged => {}
            CursorFailureDisposition::Transient => {
                self.note_einval(crtc);
                // Once eligibility changes, the cursorless scene handoff owns
                // recovery. A position-only retry must not race it or clear a
                // failed bind/hotspot/upload observation.
                self.pending_move = None;
                self.pending_move_timestamp = None;
            }
            CursorFailureDisposition::Permanent => {
                self.permanently_disabled = true;
                self.pending_move = None;
                self.pending_move_timestamp = None;
                self.transient_fallback_crtcs.clear();
            }
        }
        disposition
    }

    pub(super) fn note_initialization_failure(&mut self, error: &io::Error) {
        self.headless_deferred = false;
        let permanent = cursor_err_disables_hw(error);
        self.permanently_disabled |= permanent;
        self.initialization_retryable = !permanent;
    }

    pub(super) fn should_initialize_headless_deferred(&self, has_active_crtcs: bool) -> bool {
        has_active_crtcs
            && self.headless_deferred
            && self.plane.is_none()
            && !self.permanently_disabled
            && !self.initialization_retryable
    }

    pub(super) fn should_retry_initialization(&self, has_active_crtcs: bool) -> bool {
        has_active_crtcs
            && self.plane.is_none()
            && !self.permanently_disabled
            && self.initialization_retryable
    }
}

impl PlatformBackend {
    pub(crate) fn invalidate_cursor_applied(
        &mut self,
        device_key: crate::platform::drm::DrmDeviceKey,
        crtc: ::drm::control::crtc::Handle,
    ) {
        let Some(device) = self
            .devices
            .iter_mut()
            .find(|device| device.key == device_key)
        else {
            return;
        };
        device.cursor.crtc_states.entry(crtc).or_default().valid = false;
    }

    /// Drop the cursor positions cached and queued for one CRTC, and force
    /// its next cursor commit, after that CRTC moved in the root window.
    pub(crate) fn forget_cursor_position_for_crtc(
        &mut self,
        device_key: crate::platform::drm::DrmDeviceKey,
        crtc: ::drm::control::crtc::Handle,
    ) {
        let Some(device) = self
            .devices
            .iter_mut()
            .find(|device| device.key == device_key)
        else {
            return;
        };
        device.cursor.clear_atomic_position(crtc);
        device.cursor.crtc_states.entry(crtc).or_default().valid = false;
    }

    pub(crate) fn invalidate_live_cursor_applied(&mut self) {
        let live_crtcs: Vec<_> = self
            .outputs
            .iter()
            .map(|layout| (layout.key.device_key, layout.output.crtc))
            .collect();
        for (device_key, crtc) in live_crtcs {
            self.invalidate_cursor_applied(device_key, crtc);
        }
    }

    /// Submit the Atomic-path blocking detach before a CRTC is disabled.
    /// Legacy devices keep their existing all-off path and never enter the
    /// full-state cursor transaction.
    pub(crate) fn detach_atomic_cursor_before_disable(
        &mut self,
        device_key: crate::platform::drm::DrmDeviceKey,
        crtc: ::drm::control::crtc::Handle,
    ) -> io::Result<()> {
        let Some(device_idx) = self
            .devices
            .iter()
            .position(|device| device.key == device_key)
        else {
            return Ok(());
        };
        if self.devices[device_idx].cursor.state_path()
            != crate::kms::cursor_plane::topology::CursorStatePath::Atomic
        {
            return Ok(());
        }
        let cursor = &self.devices[device_idx].cursor;
        let owns_binding = cursor
            .crtc_states
            .get(&crtc)
            .is_some_and(|state| state.applied.is_some())
            || cursor
                .atomic_cursor_commits_in_flight
                .get(&crtc)
                .and_then(|commit| commit.prepared.as_ref())
                .is_some_and(|prepared| prepared.prior.is_some());
        if !owns_binding {
            return Ok(());
        }
        self.detach_cursor_blocking(device_idx, crtc)
    }

    fn release_cursor_crtc_owned_leases(
        &mut self,
        device_idx: usize,
        crtc: ::drm::control::crtc::Handle,
    ) {
        let applied = self.devices[device_idx]
            .cursor
            .crtc_states
            .get(&crtc)
            .and_then(|state| state.applied);
        let prior = self.devices[device_idx]
            .cursor
            .atomic_cursor_commits_in_flight
            .get(&crtc)
            .and_then(|commit| commit.prepared.as_ref())
            .and_then(|prepared| prepared.prior);
        self.release_cursor_bindings_once(device_idx, [applied, prior], "all-off CRTC cleanup");
        let cursor = &mut self.devices[device_idx].cursor;
        cursor.crtc_states.remove(&crtc);
        cursor.atomic_cursor_commits_in_flight.remove(&crtc);
        cursor.clear_atomic_position(crtc);
        cursor.cursor_retry_waits_for_flip.remove(&crtc);
        cursor.cursor_hide_recovery.remove(&crtc);
        cursor.transient_fallback_crtcs.remove(&crtc);
    }

    /// Release ownership after a successful all-off topology removal. The
    /// output must already be absent from the live topology; this guard keeps
    /// refresh/removal bookkeeping from releasing a surviving CRTC's lease.
    pub(crate) fn release_cursor_crtc_ownership_after_all_off(
        &mut self,
        device_key: crate::platform::drm::DrmDeviceKey,
        crtc: ::drm::control::crtc::Handle,
    ) {
        if self
            .outputs
            .iter()
            .any(|layout| layout.key.device_key == device_key && layout.output.crtc == crtc)
        {
            log::error!(
                "render cursor: refusing removed-CRTC cleanup for live device {device_key} CRTC {crtc:?}"
            );
            return;
        }
        let Some(device_idx) = self
            .devices
            .iter()
            .position(|device| device.key == device_key)
        else {
            return;
        };
        self.release_cursor_crtc_owned_leases(device_idx, crtc);
    }

    pub(crate) fn settle_disabled_cursor_crtc(
        &mut self,
        device_key: crate::platform::drm::DrmDeviceKey,
        crtc: ::drm::control::crtc::Handle,
    ) {
        let Some(device_idx) = self
            .devices
            .iter()
            .position(|device| device.key == device_key)
        else {
            return;
        };
        self.release_cursor_crtc_owned_leases(device_idx, crtc);
    }
}

impl CursorMoveOutcome {
    pub(super) fn merge(&mut self, other: Self) {
        self.ebusy_count = self.ebusy_count.saturating_add(other.ebusy_count);
        self.fallback_changed |= other.fallback_changed;
        self.retry_required |= other.retry_required;
    }
}

pub(super) fn apply_cursor_move_rollback_result(
    state: &mut KmsCursorState,
    crtc: ::drm::control::crtc::Handle,
    move_error: &io::Error,
    rollback: io::Result<()>,
    outcome: &mut CursorMoveOutcome,
) -> bool {
    let disposition = state.note_cursor_failure_pair(crtc, move_error, rollback.as_ref().err());
    if disposition != CursorFailureDisposition::Unchanged {
        outcome.fallback_changed = true;
    }
    match rollback {
        Ok(()) => false,
        Err(_) => {
            outcome.retry_required = true;
            // A classified eligibility failure is recovered by the scene's
            // visibility-aware cursorless hide transaction, not by a stale
            // position-only retry. Keep pending only for an unclassified
            // ownership uncertainty.
            disposition == CursorFailureDisposition::Unchanged
        }
    }
}

pub(super) fn apply_cursor_show_failure_state(
    state: &mut KmsCursorState,
    crtc: ::drm::control::crtc::Handle,
    error: &crate::kms::cursor_plane::CursorShowError,
    desired_move: (i32, i32, u16, u16),
) -> CursorFailureDisposition {
    let disposition =
        state.note_cursor_failure_pair(crtc, error.operation_error(), error.rollback_error());
    if error.remains_visible()
        && disposition == CursorFailureDisposition::Unchanged
        && !error.needs_full_rebind()
    {
        state.pending_move = Some(desired_move);
        state.pending_move_timestamp = Some(Instant::now());
    }
    disposition
}

pub(super) fn apply_cursor_operation_error(
    state: &mut KmsCursorState,
    crtc: ::drm::control::crtc::Handle,
    error: &io::Error,
) -> CursorFailureDisposition {
    state.note_cursor_failure_pair(crtc, error, None)
}

pub(super) fn apply_cursor_operation_result(
    state: &mut KmsCursorState,
    crtc: ::drm::control::crtc::Handle,
    result: &io::Result<()>,
) -> CursorFailureDisposition {
    result
        .as_ref()
        .err()
        .map_or(CursorFailureDisposition::Unchanged, |error| {
            apply_cursor_operation_error(state, crtc, error)
        })
}

pub(super) fn install_cursor_plane_for_device(
    kms_device: &mut KmsDevice,
    crtcs: &[::drm::control::crtc::Handle],
    boundary: &str,
    plane: crate::kms::cursor_plane::CursorPlane,
) {
    let decision = plane.state_path();
    log::info!(
        "render cursor: device {} cursor_state_path={} reason={}",
        kms_device.key,
        decision.path,
        decision.reason,
    );
    kms_device.cursor.topology_blocked = !plane.supports_crtcs(crtcs);
    kms_device.cursor.atomic_cursor = plane.supports_atomic_positions(crtcs);
    kms_device.cursor.initialization_retryable = false;
    kms_device.cursor.headless_deferred = false;
    log::info!(
        "render cursor: device {} initialized {}x{} ARGB8888 for {} active CRTC(s) at {boundary}; atomic_cursor={} topology_blocked={}",
        kms_device.key,
        plane.width(),
        plane.height(),
        crtcs.len(),
        kms_device.cursor.atomic_cursor,
        kms_device.cursor.topology_blocked,
    );
    kms_device.cursor.plane = Some(plane);
}

pub(super) fn initialize_cursor_plane_for_device(
    kms_device: &mut KmsDevice,
    crtcs: &[::drm::control::crtc::Handle],
    boundary: &str,
) {
    kms_device.cursor.headless_deferred = false;
    match crate::kms::cursor_plane::CursorPlane::new(Rc::clone(&kms_device.device), crtcs) {
        Ok(plane) => install_cursor_plane_for_device(kms_device, crtcs, boundary, plane),
        Err(error) => {
            kms_device.cursor.note_initialization_failure(&error);
            let retry = if kms_device.cursor.initialization_retryable {
                "will retry at an explicit topology/resume boundary"
            } else {
                "cursor support is permanently unavailable on this device"
            };
            log::warn!(
                "render cursor: device {} initialization failed at {boundary} ({error}); using software cursor, {retry}",
                kms_device.key
            );
        }
    }
}

impl PlatformBackend {
    // ── Stage 5 Phase B — hardware cursor-plane hooks ─────────────
    //
    // The plan splits the legacy `set_cursor2`-driven path into
    // narrow per-CRTC primitives so the Phase D `PendingAck`
    // transition state machine can drive the plane without
    // re-introducing the multi-output double-cursor hazard.
    //
    // - `cursor_plane_available_for_output()` is consulted by `build_scene`'s
    //   pure `CursorAssignment` decision. It resolves the output's stable
    //   device key before looking at that KmsDevice's independent plane and
    //   fallback state.
    // - `cursor_plane_upload_image_for_output` memcpys bytes into the owning
    //   device's dumb buffer ONLY. It does NOT call `set_cursor2`.
    //   `set_cursor2(Some, …)` IS the show operation in legacy DRM;
    //   upload-as-show would prematurely bind on CRTCs whose Sw→Hw
    //   transition hasn't retired yet.
    // - `cursor_plane_show_on_crtc` is the sole `set_cursor2(Some,
    //   …)` site, called per-output from `handle_page_flip_complete`
    //   when that CRTC's PendingAck queues a `ShowOnRetire`. The
    //   immediate `move_to` follow-up is required because some
    //   kernels reset the cursor position to (0, 0) on rebind (v1
    //   pattern at `backend.rs:2173`).
    // - A steady-state sprite swap is queued per output and repeats the full
    //   upload+ShowOnRetire transaction. It never treats several cards as one
    //   atomic cursor resource.
    // - `cursor_plane_move` is the pointer-fast-path entry point;
    //   one ioctl per visible CRTC, no GPU work.
    // - `cursor_plane_hide_on_crtc` and `cursor_plane_hide_all`
    //   serve Phase D' output-local / global recovery respectively.

    /// True iff the cursor plane was successfully initialised at
    /// boot AND hasn't been disabled by an auto-fallback latch. The
    /// scene strategy decision (`CursorAssignment`) gates on this
    /// without holding a `PlatformBackend` borrow.
    #[must_use]
    pub(crate) fn cursor_plane_available(&self) -> bool {
        self.outputs
            .iter()
            .enumerate()
            .any(|(output_idx, _)| self.cursor_plane_available_for_output(output_idx))
    }

    /// True iff this output's owning DRM device currently has an eligible
    /// cursor plane. Raw CRTC handles never participate in device selection.
    #[must_use]
    pub(super) fn cursor_plane_available_for_output(&self, output_idx: usize) -> bool {
        let Some(layout) = self.outputs.get(output_idx) else {
            return false;
        };
        self.device_for_key(layout.key.device_key)
            .is_some_and(|device| device.cursor.available_on(layout.output.crtc))
    }

    /// Latest position for every listed output whose owning device selected
    /// the atomic cursor path and whose cursor plane is currently visible.
    #[must_use]
    pub(crate) fn cursor_plane_atomic_positions_for_outputs(
        &self,
        output_indices: &[usize],
    ) -> Vec<crate::drm::atomic::CursorPlanePosition> {
        let mut positions = Vec::new();
        for output_idx in output_indices {
            let Some(layout) = self.outputs.get(*output_idx) else {
                continue;
            };
            let Some(device) = self.device_for_key(layout.key.device_key) else {
                continue;
            };
            let crtc = layout.output.crtc;
            if !device.cursor.atomic_cursor
                || device.cursor.permanently_disabled
                || device.cursor.cursor_hide_recovery.contains_key(&crtc)
                || !device.cursor.hw_bound_on(crtc)
            {
                continue;
            }
            if let Some(position) = device.cursor.atomic_positions.get(&crtc).copied() {
                positions.push(position);
            }
        }
        positions
    }

    /// Whether a cursor-only position can make the scheduling pass runnable.
    pub(crate) fn has_eligible_atomic_cursor_move(&self) -> bool {
        self.devices.iter().any(|device| {
            device.cursor.atomic_cursor
                && device.cursor.atomic_pending_moves.iter().any(|(crtc, _)| {
                    !device
                        .cursor
                        .atomic_cursor_commits_in_flight
                        .contains_key(crtc)
                        && !device.cursor.cursor_retry_waits_for_flip.contains(crtc)
                        && !device.cursor.cursor_hide_recovery.contains_key(crtc)
                        && device.cursor.available_on(*crtc)
                        && device.cursor.hw_bound_on(*crtc)
                })
        })
    }

    /// Only an accepted cursor-only commit owes a flip event. An EBUSY wait owes
    /// none of ours: the old-event drain fail-stops on a missing event, so it must
    /// not wait for one; the wait mark is cleared after the drain instead.
    pub(crate) fn cursor_atomic_event_pending_for_output(&self, output_idx: usize) -> bool {
        let Some(layout) = self.outputs.get(output_idx) else {
            return false;
        };
        self.device_for_key(layout.key.device_key)
            .is_some_and(|device| {
                device
                    .cursor
                    .atomic_cursor_commits_in_flight
                    .contains_key(&layout.output.crtc)
            })
    }

    pub(crate) fn cursor_retry_waits_for_flip(&self, output_idx: usize) -> bool {
        let Some(layout) = self.outputs.get(output_idx) else {
            return false;
        };
        self.device_for_key(layout.key.device_key)
            .is_some_and(|device| {
                device
                    .cursor
                    .cursor_retry_waits_for_flip
                    .contains(&layout.output.crtc)
            })
    }

    /// Whether the scene's output gate must wait for an accepted cursor-only
    /// commit on this output.
    pub(crate) fn cursor_commit_in_flight_for_output(&self, output_idx: usize) -> bool {
        let Some(layout) = self.outputs.get(output_idx) else {
            return false;
        };
        self.device_for_key(layout.key.device_key)
            .is_some_and(|device| {
                device
                    .cursor
                    .atomic_cursor_commits_in_flight
                    .contains_key(&layout.output.crtc)
            })
    }

    /// Whether any CRTC in a grouped transaction is still owned by a
    /// cursor-only commit. Direct scanout and composed unflip submit one
    /// atomic request for every listed output, so all of them must be clear.
    pub(crate) fn cursor_commit_in_flight_for_outputs(&self, output_indices: &[usize]) -> bool {
        output_indices
            .iter()
            .any(|output_idx| self.cursor_commit_in_flight_for_output(*output_idx))
    }

    pub(crate) fn any_cursor_commit_in_flight(&self) -> bool {
        self.devices
            .iter()
            .any(|device| !device.cursor.atomic_cursor_commits_in_flight.is_empty())
    }

    /// Submit the pending move for one output. The caller is the scheduling
    /// pass, after direct and scene work have had their turn. A rejected ioctl
    /// leaves the position unsent and creates no in-flight record.
    pub(crate) fn submit_atomic_cursor_move_for_output(
        &mut self,
        output_idx: usize,
    ) -> io::Result<CursorMoveOutcome> {
        use ::drm::control::AtomicCommitFlags;

        let Some(layout) = self.outputs.get(output_idx) else {
            return Ok(CursorMoveOutcome::default());
        };
        let device_key = layout.key.device_key;
        let crtc = layout.output.crtc;
        let Some(device_idx) = self
            .devices
            .iter()
            .position(|device| device.key == device_key)
        else {
            return Ok(CursorMoveOutcome::default());
        };
        let cursor = &self.devices[device_idx].cursor;
        if !cursor.atomic_cursor
            || cursor.atomic_cursor_commits_in_flight.contains_key(&crtc)
            || cursor.cursor_retry_waits_for_flip.contains(&crtc)
            || cursor.cursor_hide_recovery.contains_key(&crtc)
            || !cursor.available_on(crtc)
            || !cursor.hw_bound_on(crtc)
        {
            return Ok(CursorMoveOutcome::default());
        }
        let Some(position) = cursor.atomic_pending_moves.get(&crtc).copied() else {
            return Ok(CursorMoveOutcome::default());
        };
        let device = Rc::clone(&self.devices[device_idx].device);
        let commit_sink = Rc::clone(&self.devices[device_idx].atomic_commit_sink);
        let mut request = crate::drm::atomic::AtomicCommitRequest::new();
        request.add_cursor_position(position);
        let event = crate::drm::atomic::AtomicCommitEvent {
            crtc,
            motion_timestamp: position.motion_timestamp,
        };
        let atomic_path =
            cursor.state_path() == crate::kms::cursor_plane::topology::CursorStatePath::Atomic;
        let result = if atomic_path {
            commit_sink
                .submit_with_commit_id(
                    &device,
                    AtomicCommitFlags::PAGE_FLIP_EVENT | AtomicCommitFlags::NONBLOCK,
                    crate::drm::atomic::AtomicCommitKind::CursorOnly,
                    request,
                    &[event],
                )
                .map(Some)
        } else {
            commit_sink
                .submit(
                    &device,
                    AtomicCommitFlags::PAGE_FLIP_EVENT | AtomicCommitFlags::NONBLOCK,
                    crate::drm::atomic::AtomicCommitKind::CursorOnly,
                    request,
                    &[event],
                )
                .map(|()| None)
        };
        let commit_id = match result {
            Ok(commit_id) => commit_id,
            Err(error) => {
                let _ = commit_sink.take_last_cursor_rejection();
                if error.raw_os_error() == Some(libc::EBUSY) {
                    self.devices[device_idx]
                        .cursor
                        .cursor_retry_waits_for_flip
                        .insert(crtc);
                    return Ok(CursorMoveOutcome {
                        ebusy_count: 1,
                        ..CursorMoveOutcome::default()
                    });
                }
                let position = [position];
                let outcome = self.reject_atomic_cursor_commit(&position, &error.to_string());
                return Ok(outcome);
            }
        };

        let cursor = &mut self.devices[device_idx].cursor;
        if cursor
            .atomic_pending_moves
            .get(&crtc)
            .is_some_and(|pending| *pending == position)
        {
            cursor.atomic_pending_moves.remove(&crtc);
        }
        if let Some(latest) = cursor.atomic_positions.get_mut(&crtc)
            && *latest == position
        {
            latest.motion_timestamp = None;
        }
        let matches_latest = cursor
            .atomic_positions
            .get(&crtc)
            .is_some_and(|latest| latest.x == position.x && latest.y == position.y);
        if matches_latest
            && let Some(applied) = cursor
                .crtc_states
                .get_mut(&crtc)
                .and_then(|state| state.applied.as_mut())
            && applied.properties.plane == position.plane
        {
            applied.x = position.x;
            applied.y = position.y;
        }
        cursor.atomic_cursor_commits_in_flight.insert(
            crtc,
            crate::kms::render::platform::cursor_state::InFlightCursorCommit {
                commit_id,
                prepared: None,
            },
        );
        Ok(CursorMoveOutcome::default())
    }

    /// Attribute a flip event to this CRTC's cursor-only commit, if present.
    /// The caller still records general scene-commit latency independently.
    pub(crate) fn retire_atomic_cursor_commit_for_output(
        &mut self,
        output_idx: usize,
        event_id: Option<u64>,
    ) -> bool {
        let Some(layout) = self.outputs.get(output_idx) else {
            return false;
        };
        let device_key = layout.key.device_key;
        let crtc = layout.output.crtc;
        let Some(device) = self
            .devices
            .iter_mut()
            .find(|device| device.key == device_key)
        else {
            return false;
        };
        let should_retire = device
            .cursor
            .atomic_cursor_commits_in_flight
            .get(&crtc)
            .is_some_and(|commit| commit.commit_id.is_none() || commit.commit_id == event_id);
        let removed = should_retire
            .then(|| device.cursor.atomic_cursor_commits_in_flight.remove(&crtc))
            .flatten();
        if let Some(prepared) = removed.as_ref().and_then(|commit| commit.prepared.as_ref())
            && let Some(prior) = prepared.prior
            && prepared.next.is_none_or(|next| prior.lease != next.lease)
            && let Some(plane) = device.cursor.plane.as_mut()
            && !plane.sprites_mut().release_user(prior.lease)
        {
            log::error!("render cursor: retired commit had no user for its prior sprite lease");
        }
        let retired = removed.is_some();
        let retry_reenabled = device.cursor.cursor_retry_waits_for_flip.remove(&crtc);
        retired || retry_reenabled
    }

    /// Apply master's rollback rule to every cursor CRTC in a rejected scene
    /// transaction. Atomic-path devices detach their applied plane binding;
    /// Legacy devices keep the legacy hide. In both cases the rejected
    /// position is discarded.
    pub(crate) fn reject_atomic_cursor_commit(
        &mut self,
        positions: &[crate::drm::atomic::CursorPlanePosition],
        error: &str,
    ) -> CursorMoveOutcome {
        let mut outcome = CursorMoveOutcome::default();
        for position in positions {
            let Some(device_idx) = self
                .devices
                .iter()
                .position(|device| device.key == position.device_key)
            else {
                continue;
            };
            let crtc = position.crtc;
            let atomic_path = self.devices[device_idx].cursor.state_path()
                == crate::kms::cursor_plane::topology::CursorStatePath::Atomic;
            {
                let cursor = &mut self.devices[device_idx].cursor;
                cursor.clear_atomic_position(crtc);
                cursor.cursor_retry_waits_for_flip.remove(&crtc);
            }
            if atomic_path {
                let rollback_error = io::Error::other(error.to_owned());
                match self.atomic_rollback(device_idx, crtc, &rollback_error, false) {
                    Ok(()) => outcome.fallback_changed = true,
                    Err(_) => outcome.retry_required = true,
                }
                continue;
            }
            let device_key = self.devices[device_idx].key;
            let hide_result = self.devices[device_idx].cursor.plane.as_mut().map_or_else(
                || Err(io::Error::other("cursor plane unavailable during rollback")),
                |plane| plane.hide(crtc),
            );
            match hide_result {
                Ok(()) => {
                    self.devices[device_idx]
                        .cursor
                        .cursor_hide_recovery
                        .remove(&crtc);
                    self.devices[device_idx].cursor.note_einval(crtc);
                    outcome.fallback_changed = true;
                    log::warn!(
                        "render cursor atomic commit: device {device_key} CRTC {crtc:?} rejected cursor fields ({error}); hid the plane and entered transient software fallback"
                    );
                }
                Err(hide_error) => {
                    self.devices[device_idx]
                        .cursor
                        .cursor_hide_recovery
                        .insert(crtc, CursorHideRecovery::default());
                    outcome.retry_required = true;
                    log::warn!(
                        "render cursor atomic commit: device {device_key} CRTC {crtc:?} rejected cursor fields ({error}); hide failed ({hide_error}), retaining hardware ownership for motion recovery"
                    );
                }
            }
        }
        outcome
    }

    pub(crate) fn defer_cursor_move_outcome(&mut self, outcome: CursorMoveOutcome) {
        self.deferred_cursor_move_outcome.merge(outcome);
    }

    pub(crate) fn take_deferred_cursor_move_outcome(&mut self) -> CursorMoveOutcome {
        std::mem::take(&mut self.deferred_cursor_move_outcome)
    }

    /// Drop atomic per-CRTC state only after the old-event drain has proved
    /// that no cursor-only flip can be attributed to the next topology epoch.
    pub(crate) fn clear_atomic_cursor_state_after_old_events_discard(&mut self) {
        for device_idx in 0..self.devices.len() {
            let discarded_priors: Vec<_> = self.devices[device_idx]
                .cursor
                .atomic_cursor_commits_in_flight
                .values()
                .filter_map(|commit| commit.prepared.as_ref())
                .filter(|prepared| {
                    prepared.prior.is_some_and(|prior| {
                        prepared.next.is_none_or(|next| next.lease != prior.lease)
                    })
                })
                .map(|prepared| prepared.prior)
                .collect();
            for prior in discarded_priors {
                self.release_cursor_bindings_once(device_idx, [prior], "discarded old event");
            }
            let cursor = &mut self.devices[device_idx].cursor;
            cursor.atomic_positions.clear();
            cursor.atomic_pending_moves.clear();
            cursor.atomic_cursor_commits_in_flight.clear();
            cursor.cursor_retry_waits_for_flip.clear();
            cursor.cursor_hide_recovery.clear();
            cursor.transient_fallback_crtcs.clear();
        }
    }

    /// Clear only the unsent values carried by an accepted scene commit. The
    /// latest positions stay cached so subsequent scene commits continue to
    /// include the cursor plane state.
    pub(crate) fn cursor_plane_scene_commit_submitted(
        &mut self,
        positions: &[crate::drm::atomic::CursorPlanePosition],
    ) {
        for position in positions {
            for device in &mut self.devices {
                if device.key != position.device_key {
                    continue;
                }
                let matches_latest = device
                    .cursor
                    .atomic_positions
                    .get(&position.crtc)
                    .is_some_and(|latest| latest == position);
                if !matches_latest {
                    continue;
                }
                if let Some(applied) = device
                    .cursor
                    .crtc_states
                    .get_mut(&position.crtc)
                    .and_then(|state| state.applied.as_mut())
                    && applied.properties.plane == position.plane
                {
                    applied.x = position.x;
                    applied.y = position.y;
                }
                if device
                    .cursor
                    .atomic_pending_moves
                    .get(&position.crtc)
                    .is_some_and(|unsent| unsent == position)
                {
                    device.cursor.atomic_pending_moves.remove(&position.crtc);
                }
                if let Some(latest) = device.cursor.atomic_positions.get_mut(&position.crtc) {
                    latest.motion_timestamp = None;
                }
            }
        }
    }

    /// Seed one already-visible cursor position without issuing a DRM ioctl.
    /// Tests use the normal output-origin conversion and production cache;
    /// production never calls this helper.
    #[cfg(test)]
    pub(crate) fn for_tests_seed_atomic_cursor_position(
        &mut self,
        output_idx: usize,
        plane_handle: ::drm::control::plane::Handle,
        crtc_x: ::drm::control::property::Handle,
        crtc_y: ::drm::control::property::Handle,
        root_x: i32,
        root_y: i32,
        hot_x: u16,
        hot_y: u16,
    ) -> crate::drm::atomic::CursorPlanePosition {
        let layout = &self.outputs[output_idx];
        let device_key = layout.key.device_key;
        let crtc = layout.output.crtc;
        let (crtc_x_local, crtc_y_local) =
            cursor_root_to_crtc_local(root_x, root_y, layout.x, layout.y, hot_x, hot_y);
        let device = self
            .devices
            .iter_mut()
            .find(|device| device.key == device_key)
            .expect("test output has an owning KMS device");
        if device.cursor.plane.is_none() {
            device.cursor.plane = Some(crate::kms::cursor_plane::CursorPlane::for_tests_stub(
                Rc::clone(&device.device),
                64,
                64,
            ));
        }
        let plane = device
            .cursor
            .plane
            .as_mut()
            .expect("test cursor plane was installed");
        plane.for_tests_set_atomic_position_properties(crtc, plane_handle, crtc_x, crtc_y);
        plane.for_tests_set_visible(crtc, true);
        device.cursor.atomic_cursor = true;
        device.cursor.remember_atomic_position(
            device_key,
            crtc,
            crtc_x_local,
            crtc_y_local,
            Some(Instant::now()),
        );
        device
            .cursor
            .atomic_positions
            .get(&crtc)
            .copied()
            .expect("test cursor plane exposes atomic position properties")
    }

    #[cfg(test)]
    pub(crate) fn for_tests_atomic_position_is_unsent(
        &self,
        device_key: crate::platform::drm::DrmDeviceKey,
        crtc: ::drm::control::crtc::Handle,
    ) -> bool {
        self.devices
            .iter()
            .find(|device| device.key == device_key)
            .is_some_and(|device| device.cursor.atomic_pending_moves.contains_key(&crtc))
    }

    #[cfg(test)]
    pub(crate) fn for_tests_atomic_cursor_commit_is_in_flight(
        &self,
        device_key: crate::platform::drm::DrmDeviceKey,
        crtc: ::drm::control::crtc::Handle,
    ) -> bool {
        self.devices
            .iter()
            .find(|device| device.key == device_key)
            .is_some_and(|device| {
                device
                    .cursor
                    .atomic_cursor_commits_in_flight
                    .contains_key(&crtc)
            })
    }

    #[cfg(test)]
    pub(crate) fn for_tests_atomic_cursor_prior_lease(
        &self,
        device_key: crate::platform::drm::DrmDeviceKey,
        crtc: ::drm::control::crtc::Handle,
    ) -> Option<crate::kms::cursor_plane::sprites::SpriteBufferLease> {
        self.devices
            .iter()
            .find(|device| device.key == device_key)?
            .cursor
            .atomic_cursor_commits_in_flight
            .get(&crtc)?
            .prepared
            .as_ref()?
            .prior
            .map(|binding| binding.lease)
    }

    #[cfg(test)]
    pub(crate) fn for_tests_atomic_cursor_pending_position(
        &self,
        device_key: crate::platform::drm::DrmDeviceKey,
        crtc: ::drm::control::crtc::Handle,
    ) -> Option<crate::drm::atomic::CursorPlanePosition> {
        self.devices
            .iter()
            .find(|device| device.key == device_key)
            .and_then(|device| device.cursor.atomic_pending_moves.get(&crtc).copied())
    }

    #[cfg(test)]
    pub(crate) fn for_tests_cursor_hide_recovery_attempts(
        &self,
        device_key: crate::platform::drm::DrmDeviceKey,
        crtc: ::drm::control::crtc::Handle,
    ) -> Option<u8> {
        self.devices
            .iter()
            .find(|device| device.key == device_key)
            .and_then(|device| device.cursor.cursor_hide_recovery.get(&crtc))
            .map(|recovery| recovery.attempts)
    }

    #[cfg(test)]
    pub(crate) fn for_tests_cursor_permanently_disabled(
        &self,
        device_key: crate::platform::drm::DrmDeviceKey,
    ) -> bool {
        self.devices
            .iter()
            .find(|device| device.key == device_key)
            .is_some_and(|device| device.cursor.permanently_disabled)
    }

    #[cfg(test)]
    pub(crate) fn for_tests_cursor_fallback_remaining(
        &self,
        device_key: crate::platform::drm::DrmDeviceKey,
        crtc: ::drm::control::crtc::Handle,
    ) -> Option<u8> {
        self.devices
            .iter()
            .find(|device| device.key == device_key)
            .and_then(|device| device.cursor.transient_fallback_crtcs.get(&crtc))
            .map(|fallback| fallback.remaining_sw_retires)
    }

    #[must_use]
    pub(crate) fn cursor_plane_fits_for_output(
        &self,
        output_idx: usize,
        width: u32,
        height: u32,
    ) -> bool {
        let Some(layout) = self.outputs.get(output_idx) else {
            return false;
        };
        let Some(device) = self.device_for_key(layout.key.device_key) else {
            return false;
        };
        device.cursor.available_on(layout.output.crtc)
            && device.cursor.plane.as_ref().is_some_and(|plane| {
                cursor_dimensions_fit(plane.width(), plane.height(), width, height)
            })
    }

    pub(super) fn cursor_output_route(
        &self,
        output_idx: usize,
    ) -> io::Result<(usize, ::drm::control::crtc::Handle, i32, i32)> {
        let layout = self
            .outputs
            .get(output_idx)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such output"))?;
        let device_idx = self
            .devices
            .iter()
            .position(|device| device.key == layout.key.device_key)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "no DRM device {} for output {output_idx}",
                        layout.key.device_key
                    ),
                )
            })?;
        Ok((device_idx, layout.output.crtc, layout.x, layout.y))
    }

    pub(super) fn note_unbound_cursor_failure(
        &mut self,
        device_idx: usize,
        crtc: ::drm::control::crtc::Handle,
        error: &io::Error,
    ) -> bool {
        let device_key = self.devices[device_idx].key;
        let was_permanently_disabled = self.devices[device_idx].cursor.permanently_disabled;
        let disposition = self.devices[device_idx]
            .cursor
            .note_cursor_failure_pair(crtc, error, None);
        match disposition {
            CursorFailureDisposition::Permanent => {
                if !was_permanently_disabled {
                    log::warn!(
                        "render cursor: device {device_key} permanently rejected cursor ioctls ({error}); using software cursor on that device"
                    );
                }
                true
            }
            CursorFailureDisposition::Transient => {
                log::warn!(
                    "render cursor: device {device_key} CRTC {crtc:?} rejected a temporary cursor state ({error}); rate-limited software fallback"
                );
                true
            }
            CursorFailureDisposition::Unchanged => false,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_tests_note_unbound_cursor_failure(
        &mut self,
        device_idx: usize,
        crtc: ::drm::control::crtc::Handle,
        error: &io::Error,
    ) -> bool {
        self.note_unbound_cursor_failure(device_idx, crtc, error)
    }

    /// Diagnostic/test hook: whether one particular device is permanently
    /// latched to software cursor composition.
    #[must_use]
    pub(super) fn hw_cursor_disabled_for_device(
        &self,
        key: crate::platform::drm::DrmDeviceKey,
    ) -> bool {
        self.device_for_key(key)
            .is_some_and(|device| device.cursor.permanently_disabled)
    }

    #[cfg(test)]
    pub(crate) fn for_tests_hw_cursor_disabled_for_device(
        &self,
        key: crate::platform::drm::DrmDeviceKey,
    ) -> bool {
        self.hw_cursor_disabled_for_device(key)
    }

    /// A new sprite or hotspot invalidates EINVAL observations made against a
    /// previous parameter set. This is deliberately a global sprite fanout,
    /// but every device clears only its own CRTC retry records.
    pub(crate) fn cursor_plane_note_sprite_hotspot(
        &mut self,
        width: u16,
        height: u16,
        hot_x: u16,
        hot_y: u16,
    ) {
        let signature = (width, height, hot_x, hot_y);
        for device in &mut self.devices {
            if device.cursor.sprite_signature != Some(signature) {
                device.cursor.sprite_signature = Some(signature);
                device.cursor.transient_fallback_crtcs.clear();
            }
        }
    }

    pub(crate) fn cursor_plane_upload_image_for_output(
        &mut self,
        output_idx: usize,
        version: u64,
        hot_x: u16,
        hot_y: u16,
        width: u32,
        height: u32,
        bgra_bytes: &[u8],
    ) -> io::Result<crate::kms::render::scene::CursorUploadOutcome> {
        let (device_idx, crtc, _, _) = self.cursor_output_route(output_idx)?;
        if !self.devices[device_idx].cursor.available_on(crtc) {
            return Err(io::Error::other("cursor plane unavailable for output"));
        }
        if self.devices[device_idx].cursor.state_path()
            == crate::kms::cursor_plane::topology::CursorStatePath::Atomic
        {
            let device = Rc::clone(&self.devices[device_idx].device);
            let result = {
                let plane = self.devices[device_idx]
                    .cursor
                    .plane
                    .as_mut()
                    .expect("available cursor state has a plane");
                let buffer_io = plane.buffer_io();
                plane.sprites_mut().prepare(
                    device, buffer_io, version, hot_x, hot_y, width, height, bgra_bytes,
                )
            };
            return match result {
                Ok(crate::kms::cursor_plane::sprites::SpriteStage::Ready) => {
                    Ok(crate::kms::render::scene::CursorUploadOutcome::Ready)
                }
                Ok(crate::kms::cursor_plane::sprites::SpriteStage::Deferred) => {
                    Ok(crate::kms::render::scene::CursorUploadOutcome::Deferred)
                }
                Err(error) => {
                    apply_cursor_operation_error(
                        &mut self.devices[device_idx].cursor,
                        crtc,
                        &error,
                    );
                    Err(error)
                }
            };
        }
        let state = &mut self.devices[device_idx].cursor;
        let result = state
            .plane
            .as_mut()
            .expect("available cursor state has a plane")
            .upload_image(version, width, height, bgra_bytes);
        // Only a real upload attempt is classified. The availability
        // precheck above and scene-side version races are not driver
        // observations and must not extend this output's backoff.
        apply_cursor_operation_result(state, crtc, &result);
        result.map(|()| crate::kms::render::scene::CursorUploadOutcome::Ready)
    }

    #[must_use]
    pub(crate) fn cursor_plane_uploaded_image_matches_for_output(
        &self,
        output_idx: usize,
        version: u64,
        hot_x: u16,
        hot_y: u16,
    ) -> bool {
        let Ok((device_idx, _, _, _)) = self.cursor_output_route(output_idx) else {
            return false;
        };
        let cursor = &self.devices[device_idx].cursor;
        cursor.plane.as_ref().is_some_and(|plane| {
            if cursor.state_path() == crate::kms::cursor_plane::topology::CursorStatePath::Atomic {
                plane
                    .sprites()
                    .candidate_lease()
                    .is_some_and(|lease| lease.matches(version, hot_x, hot_y))
            } else {
                plane.uploaded_version() == Some(version)
            }
        })
    }

    /// Bind the plane on `output_idx`'s CRTC + position at `(x, y)`
    /// in root-space (translated to CRTC-local coords here). The
    /// sole `set_cursor2(crtc, Some(dumb), …)` call site.
    ///
    /// # Errors
    /// `set_cursor2` or `move_cursor` ioctl failure; `NotFound` if
    /// `output_idx` is out of range or plane is unavailable.
    pub(crate) fn cursor_plane_show_on_crtc(
        &mut self,
        output_idx: usize,
        hot_x: u16,
        hot_y: u16,
        x: i32,
        y: i32,
    ) -> Result<(), crate::kms::cursor_plane::CursorShowError> {
        if self
            .cursor_output_route(output_idx)
            .ok()
            .is_some_and(|(device_idx, _, _, _)| {
                self.devices[device_idx].cursor.state_path()
                    == crate::kms::cursor_plane::topology::CursorStatePath::Atomic
            })
        {
            return self.atomic_show(output_idx, hot_x, hot_y, x, y);
        }
        let (device_idx, crtc, layout_x, layout_y) = self
            .cursor_output_route(output_idx)
            .map_err(crate::kms::cursor_plane::CursorShowError::Unbound)?;
        let (cx, cy) = cursor_root_to_crtc_local(x, y, layout_x, layout_y, hot_x, hot_y);
        if !self.devices[device_idx].cursor.available_on(crtc) {
            return Err(crate::kms::cursor_plane::CursorShowError::Unbound(
                io::Error::other("cursor plane unavailable for output"),
            ));
        }
        let result = self.devices[device_idx]
            .cursor
            .plane
            .as_mut()
            .expect("available cursor state has a plane")
            .show(crtc, (i32::from(hot_x), i32::from(hot_y)), cx, cy);
        match result {
            Ok(()) => {
                self.devices[device_idx].cursor.note_cursor_success(crtc);
                let device_key = self.devices[device_idx].key;
                self.devices[device_idx]
                    .cursor
                    .remember_atomic_position(device_key, crtc, cx, cy, None);
                Ok(())
            }
            Err(error) => {
                let device_key = self.devices[device_idx].key;
                if error.remains_visible() {
                    let disposition = apply_cursor_show_failure_state(
                        &mut self.devices[device_idx].cursor,
                        crtc,
                        &error,
                        (x, y, hot_x, hot_y),
                    );
                    if disposition == CursorFailureDisposition::Permanent {
                        log::warn!(
                            "render cursor: device {device_key} reported a permanent cursor failure while the prior HW binding remained visible; retaining actual HW mode until a hide succeeds"
                        );
                    } else if disposition == CursorFailureDisposition::Transient {
                        log::warn!(
                            "render cursor: device {device_key} CRTC {crtc:?} rejected a temporary cursor state while the prior HW binding remained visible; entering rate-limited cursorless fallback"
                        );
                    }
                } else {
                    self.note_unbound_cursor_failure(device_idx, crtc, error.operation_error());
                }
                Err(error)
            }
        }
    }

    /// Cursor move per visible CRTC. Hidden CRTCs are skipped — the
    /// kernel naturally clips off-output coords on the visible ones, so no
    /// per-output geometry test is needed beyond the visibility filter.
    /// Atomic-capable devices record the latest position for the scheduling
    /// pass; other devices keep the legacy move ioctl.
    ///
    /// Returns the legacy move's EBUSY count, if any. Other errors are logged
    /// per-CRTC and not counted.
    ///
    /// # Errors
    /// `Err` only when the plane is unavailable; per-CRTC ioctl
    /// failures are logged + counted (EBUSY) or logged (other).
    pub(crate) fn cursor_plane_move(
        &mut self,
        x: i32,
        y: i32,
        hot_x: u16,
        hot_y: u16,
    ) -> io::Result<CursorMoveOutcome> {
        let motion_timestamp = Instant::now();
        let mut aggregate = CursorMoveOutcome::default();
        let mut found = false;
        for device_idx in 0..self.devices.len() {
            if self.devices[device_idx].cursor.plane.is_none() {
                continue;
            }
            found = true;
            let outcome = self.try_cursor_plane_move_for_device(
                device_idx,
                x,
                y,
                hot_x,
                hot_y,
                motion_timestamp,
            )?;
            aggregate.merge(outcome);
        }
        for device in &self.devices {
            device.atomic_commit_sink.emit_telemetry_if_due();
        }
        found
            .then_some(aggregate)
            .ok_or_else(|| io::Error::other("cursor plane unavailable"))
    }

    /// Retry the most recent pending cursor move, if any. Called from
    /// the backend's page-flip-complete handler — the just-retired
    /// flip means the primary atomic-commit queue freed up for this
    /// CRTC, so the cursor commit that lost the race a few ms ago has
    /// a fresh window to land. Latest-wins: only the most recent
    /// position is retried, intermediate motions are discarded.
    ///
    /// Returns the EBUSY count from this retry (typically 0 if the
    /// commit landed; >0 means the cursor commit raced another
    /// pending primary commit and stays queued for the next page-flip
    /// retire).
    ///
    /// # Errors
    /// `Err` only when the plane is unavailable.
    pub(crate) fn cursor_plane_drain_pending_move_for_output(
        &mut self,
        output_idx: usize,
    ) -> io::Result<CursorMoveOutcome> {
        let (device_idx, _, _, _) = self.cursor_output_route(output_idx)?;
        let Some((x, y, hot_x, hot_y)) = self.devices[device_idx].cursor.pending_move else {
            return Ok(CursorMoveOutcome::default());
        };
        let motion_timestamp = self.devices[device_idx]
            .cursor
            .pending_move_timestamp
            .unwrap_or_else(Instant::now);
        self.try_cursor_plane_move_for_device(device_idx, x, y, hot_x, hot_y, motion_timestamp)
    }

    fn apply_cursor_motion_hide_recovery_result(
        &mut self,
        device_idx: usize,
        crtc: ::drm::control::crtc::Handle,
        recovery: CursorHideRecovery,
        result: io::Result<()>,
        operation: &str,
    ) -> CursorMoveOutcome {
        let device_key = self.devices[device_idx].key;
        let mut outcome = CursorMoveOutcome::default();
        match result {
            Ok(()) => {
                let state = &mut self.devices[device_idx].cursor;
                state.cursor_hide_recovery.remove(&crtc);
                state.clear_atomic_position(crtc);
                state.note_einval(crtc);
                outcome.fallback_changed = true;
                log::warn!(
                    "render cursor hide recovery: device {device_key} CRTC {crtc:?} {operation} succeeded on pointer motion; entering transient software fallback"
                );
            }
            Err(error) => {
                let attempts = recovery.attempts.saturating_add(1);
                let state = &mut self.devices[device_idx].cursor;
                state
                    .cursor_hide_recovery
                    .insert(crtc, CursorHideRecovery { attempts });
                outcome.retry_required = true;
                if attempts >= 3 {
                    state.permanently_disabled = true;
                    state.atomic_positions.clear();
                    state.atomic_pending_moves.clear();
                    state.cursor_retry_waits_for_flip.clear();
                    state.cursor_hide_recovery.clear();
                    outcome.fallback_changed = true;
                    log::error!(
                        "render cursor hide recovery: device {device_key} CRTC {crtc:?} {operation} failed 3 times ({error}); permanently disabling the hardware cursor"
                    );
                } else {
                    log::warn!(
                        "render cursor hide recovery: device {device_key} CRTC {crtc:?} {operation} retry {attempts}/3 failed ({error})"
                    );
                }
            }
        }
        outcome
    }

    /// Internal helper: per-CRTC `move_to` iteration that returns the
    /// number of CRTCs whose move ioctl returned `EBUSY`. Shared by
    /// `cursor_plane_move` (first-attempt path) and
    /// `cursor_plane_drain_pending_move` (retry path).
    fn try_cursor_plane_move_for_device(
        &mut self,
        device_idx: usize,
        x: i32,
        y: i32,
        hot_x: u16,
        hot_y: u16,
        motion_timestamp: Instant,
    ) -> io::Result<CursorMoveOutcome> {
        let device_key = self.devices[device_idx].key;
        let atomic_commit_sink = Rc::clone(&self.devices[device_idx].atomic_commit_sink);
        let layouts: Vec<(::drm::control::crtc::Handle, i32, i32)> = self
            .outputs
            .iter()
            .filter(|layout| layout.key.device_key == device_key)
            .map(|l| (l.output.crtc, l.x, l.y))
            .collect();
        if self.devices[device_idx].cursor.plane.is_none() {
            return Err(io::Error::other("cursor plane unavailable"));
        }
        if self.devices[device_idx].cursor.permanently_disabled {
            return Ok(CursorMoveOutcome::default());
        }
        let atomic_path = self.devices[device_idx].cursor.state_path()
            == crate::kms::cursor_plane::topology::CursorStatePath::Atomic;
        let mut outcome = CursorMoveOutcome::default();
        let mut keep_pending = false;
        for (crtc, layout_x, layout_y) in layouts {
            if self.devices[device_idx].cursor.permanently_disabled {
                break;
            }
            if !self.devices[device_idx].cursor.hw_bound_on(crtc) {
                continue;
            }
            let recovery = self.devices[device_idx]
                .cursor
                .cursor_hide_recovery
                .get(&crtc)
                .copied()
                .filter(|_| atomic_path || self.devices[device_idx].cursor.atomic_cursor);
            if let Some(recovery) = recovery {
                let result = if atomic_path {
                    self.detach_cursor_blocking(device_idx, crtc)
                } else {
                    self.devices[device_idx]
                        .cursor
                        .plane
                        .as_mut()
                        .expect("checked cursor plane")
                        .hide(crtc)
                };
                let operation = if atomic_path { "atomic detach" } else { "hide" };
                outcome.merge(self.apply_cursor_motion_hide_recovery_result(
                    device_idx, crtc, recovery, result, operation,
                ));
                continue;
            }

            let state = &mut self.devices[device_idx].cursor;
            let (position_hot_x, position_hot_y) = if atomic_path {
                state
                    .crtc_states
                    .get(&crtc)
                    .and_then(|crtc_state| crtc_state.applied)
                    .map(|applied| (applied.hot_x, applied.hot_y))
                    .unwrap_or((hot_x, hot_y))
            } else {
                (hot_x, hot_y)
            };
            let (cx, cy) =
                cursor_root_to_crtc_local(x, y, layout_x, layout_y, position_hot_x, position_hot_y);
            if state.atomic_cursor {
                state.remember_atomic_position(device_key, crtc, cx, cy, Some(motion_timestamp));
                if atomic_path
                    && state
                        .crtc_states
                        .get(&crtc)
                        .and_then(|crtc_state| crtc_state.applied)
                        .is_some_and(|applied| applied.x == cx && applied.y == cy)
                {
                    state.atomic_pending_moves.remove(&crtc);
                }
                continue;
            }

            let move_result = state
                .plane
                .as_ref()
                .expect("checked cursor plane")
                .move_to(crtc, cx, cy);
            state.remember_atomic_position(device_key, crtc, cx, cy, Some(motion_timestamp));
            atomic_commit_sink.record_legacy_move(motion_timestamp, Instant::now());
            if let Err(e) = move_result {
                if e.raw_os_error() == Some(libc::EBUSY) {
                    outcome.ebusy_count = outcome.ebusy_count.saturating_add(1);
                    keep_pending = true;
                } else if e.raw_os_error() == Some(libc::EINVAL) || cursor_err_disables_hw(&e) {
                    // A move failure leaves the old HW sprite visible. Only
                    // enter SW fallback after a successful hide rollback.
                    let hide_result = state
                        .plane
                        .as_mut()
                        .expect("checked cursor plane")
                        .hide(crtc);
                    let rollback_error = hide_result.as_ref().err().map(ToString::to_string);
                    keep_pending |= apply_cursor_move_rollback_result(
                        state,
                        crtc,
                        &e,
                        hide_result,
                        &mut outcome,
                    );
                    if let Some(rollback_error) = rollback_error {
                        log::warn!(
                            "render cursor move: device {device_key} CRTC {crtc:?} failed ({e}); hide rollback also failed ({rollback_error}), retaining HW ownership"
                        );
                    }
                } else {
                    log::warn!("render cursor move: device {device_key} CRTC {crtc:?} failed: {e}");
                }
            }
        }
        let keep_pending = keep_pending || outcome.ebusy_count > 0;
        let state = &mut self.devices[device_idx].cursor;
        state.pending_move = keep_pending.then_some((x, y, hot_x, hot_y));
        state.pending_move_timestamp = keep_pending.then_some(motion_timestamp);
        Ok(outcome)
    }

    /// True iff the set of CRTCs whose region the cursor footprint
    /// intersects differs from the set the plane is currently bound on
    /// (`hw_bound_on`).
    ///
    /// The pointer fast path (`cursor_plane_move`) only *repositions*
    /// the cursor on already-bound CRTCs — it never shows the plane on
    /// a CRTC the pointer newly crosses into, nor hides it on one it
    /// leaves. Cross-CRTC show/hide is decided by the scene's
    /// `CursorAssignment` during compose. While an idle desktop
    /// composited every frame (pre-#30) that reassignment happened for
    /// free; now that idle desktops stop compositing, the fast path
    /// must detect a boundary crossing and route it through one compose
    /// tick. This predicate is that detector, using the same footprint
    /// intersection rule as `cursor_footprint_rect` so its membership
    /// decision matches the scene's exactly.
    ///
    /// `(x, y)` is the root-space cursor position, `(hot_x, hot_y)` the
    /// sprite hotspot, `(cw, ch)` the sprite extent.
    pub(crate) fn cursor_crtc_membership_dirty(
        &self,
        x: i32,
        y: i32,
        hot_x: u16,
        hot_y: u16,
        cw: i32,
        ch: i32,
    ) -> bool {
        for l in &self.outputs {
            let Some(device) = self.device_for_key(l.key.device_key) else {
                continue;
            };
            if device.cursor.plane.is_none() {
                continue;
            }
            let dx = x - i32::from(hot_x) - l.x;
            let dy = y - i32::from(hot_y) - l.y;
            let intersects = cursor_footprint_intersects_output(
                dx,
                dy,
                cw,
                ch,
                i32::from(l.width),
                i32::from(l.height),
            );
            if intersects != device.cursor.hw_bound_on(l.output.crtc) {
                return true;
            }
        }
        false
    }

    /// Detach the plane on a single CRTC. Output-local recovery
    /// (Phase D') uses this; the per-CRTC visibility map updates
    /// so subsequent rebind / move calls skip the CRTC cleanly.
    ///
    /// # Errors
    /// `NotFound` if `output_idx` is out of range or plane is
    /// unavailable; `set_cursor2` ioctl failure otherwise.
    pub(crate) fn cursor_plane_hide_on_crtc(&mut self, output_idx: usize) -> io::Result<()> {
        if self
            .cursor_output_route(output_idx)
            .ok()
            .is_some_and(|(device_idx, _, _, _)| {
                self.devices[device_idx].cursor.state_path()
                    == crate::kms::cursor_plane::topology::CursorStatePath::Atomic
            })
        {
            return self.atomic_hide(output_idx);
        }
        let (device_idx, crtc, _, _) = self.cursor_output_route(output_idx)?;
        // A disabled device still owns a plane its failed hides left bound.
        if self.devices[device_idx].cursor.permanently_disabled
            && !self.cursor_plane_visible_for_output(output_idx)
        {
            return Ok(());
        }
        let result = {
            let Some(plane) = self.devices[device_idx].cursor.plane.as_mut() else {
                return Err(io::Error::other("cursor plane unavailable"));
            };
            plane.hide(crtc)
        };
        if result.is_ok() {
            self.devices[device_idx].cursor.clear_atomic_position(crtc);
        }
        apply_cursor_operation_result(&mut self.devices[device_idx].cursor, crtc, &result);
        result
    }

    /// Kernel-side visibility for this output's device-qualified CRTC. Scene
    /// bookkeeping can temporarily lag after an ioctl/rollback failure, so a
    /// compose tick must consult this before deciding it is safe to draw SW.
    #[must_use]
    pub(crate) fn cursor_plane_visible_for_output(&self, output_idx: usize) -> bool {
        let Ok((device_idx, crtc, _, _)) = self.cursor_output_route(output_idx) else {
            return false;
        };
        self.devices[device_idx].cursor.hw_bound_on(crtc)
    }

    /// Physical cursor-plane state across the current output inventory.
    #[must_use]
    pub(crate) fn any_cursor_plane_visible(&self) -> bool {
        (0..self.outputs.len()).any(|output_idx| self.cursor_plane_visible_for_output(output_idx))
    }

    /// Advance only this output's EINVAL retry budget after its own successful
    /// software-composed retirement. Returns true when another repaint is
    /// needed either to consume more backoff or to perform the now-eligible HW
    /// retry. A retirement on another card cannot touch this record.
    pub(crate) fn cursor_plane_note_composed_retirement(&mut self, output_idx: usize) -> bool {
        let Ok((device_idx, crtc, _, _)) = self.cursor_output_route(output_idx) else {
            return false;
        };
        let Some(retry) = self.devices[device_idx]
            .cursor
            .transient_fallback_crtcs
            .get_mut(&crtc)
        else {
            return false;
        };
        if retry.remaining_sw_retires > 0 {
            retry.remaining_sw_retires -= 1;
            return true;
        }
        false
    }

    /// Revalidate every existing per-device cursor plane after an active CRTC
    /// topology change. A genuinely headless-deferred device is deliberately
    /// skipped here: only the post-success explicit-enable hook may make its
    /// first attempt. A prior transient attempt is retried at this later
    /// topology/resume boundary.
    pub(super) fn refresh_cursor_topology_for_devices(
        &mut self,
        changed_devices: &HashSet<crate::platform::drm::DrmDeviceKey>,
    ) {
        self.refresh_cursor_topology_for_devices_with(
            changed_devices,
            initialize_cursor_plane_for_device,
        );
    }

    pub(super) fn refresh_cursor_topology_for_devices_with<F>(
        &mut self,
        changed_devices: &HashSet<crate::platform::drm::DrmDeviceKey>,
        factory: F,
    ) where
        F: FnMut(&mut KmsDevice, &[::drm::control::crtc::Handle], &str),
    {
        self.refresh_cursor_topology_scoped_with(changed_devices, None, factory);
    }

    /// [`Self::refresh_cursor_topology_for_devices_with`]; with `only_crtc`
    /// set, per-CRTC cursor state is dropped for that CRTC alone, so the
    /// device's other live CRTCs keep their queued moves. A per-CRTC RANDR
    /// change uses this; a global quiesce drops every CRTC's state.
    pub(super) fn refresh_cursor_topology_scoped_with<F>(
        &mut self,
        changed_devices: &HashSet<crate::platform::drm::DrmDeviceKey>,
        only_crtc: Option<::drm::control::crtc::Handle>,
        mut factory: F,
    ) where
        F: FnMut(&mut KmsDevice, &[::drm::control::crtc::Handle], &str),
    {
        let mut crtcs_by_device: HashMap<_, Vec<_>> = HashMap::new();
        for output in &self.outputs {
            crtcs_by_device
                .entry(output.key.device_key)
                .or_default()
                .push(output.output.crtc);
        }
        for device in &mut self.devices {
            if !changed_devices.contains(&device.key) {
                continue;
            }
            let atomic = device.cursor.state_path()
                == crate::kms::cursor_plane::topology::CursorStatePath::Atomic;
            if let Some(crtc) = only_crtc {
                device.cursor.clear_atomic_position(crtc);
                if !atomic
                    || device
                        .cursor
                        .atomic_cursor_commits_in_flight
                        .get(&crtc)
                        .is_some_and(|commit| commit.commit_id.is_none())
                {
                    device.cursor.atomic_cursor_commits_in_flight.remove(&crtc);
                }
                device.cursor.cursor_retry_waits_for_flip.remove(&crtc);
                device.cursor.cursor_hide_recovery.remove(&crtc);
                device.cursor.transient_fallback_crtcs.remove(&crtc);
            } else {
                device.cursor.pending_move = None;
                device.cursor.pending_move_timestamp = None;
                device.cursor.atomic_positions.clear();
                device.cursor.atomic_pending_moves.clear();
                if atomic {
                    device
                        .cursor
                        .atomic_cursor_commits_in_flight
                        .retain(|_, commit| commit.commit_id.is_some());
                } else {
                    device.cursor.atomic_cursor_commits_in_flight.clear();
                }
                device.cursor.cursor_retry_waits_for_flip.clear();
                device.cursor.cursor_hide_recovery.clear();
                device.cursor.transient_fallback_crtcs.clear();
            }
            let crtcs = crtcs_by_device.remove(&device.key).unwrap_or_default();
            if device.cursor.should_retry_initialization(!crtcs.is_empty()) {
                factory(device, &crtcs, "lifecycle retry");
            }
            if let Some(plane) = device.cursor.plane.as_mut() {
                plane.sprites_mut().set_active_crtc_count(crtcs.len());
                plane.retain_crtcs(&crtcs.iter().copied().collect());
            }
            let blocked = device
                .cursor
                .plane
                .as_ref()
                .is_some_and(|plane| !plane.supports_crtcs(&crtcs));
            device.cursor.atomic_cursor = device
                .cursor
                .plane
                .as_ref()
                .is_some_and(|plane| plane.supports_atomic_positions(&crtcs));
            if blocked != device.cursor.topology_blocked {
                log::warn!(
                    "render cursor: device {} topology_blocked {} -> {} for {} active CRTC(s)",
                    device.key,
                    device.cursor.topology_blocked,
                    blocked,
                    crtcs.len()
                );
            }
            device.cursor.topology_blocked = blocked;
        }
    }

    #[cfg(test)]
    pub(crate) fn for_tests_install_cursor_plane_from_snapshot(
        &mut self,
        device_idx: usize,
        snapshot: crate::kms::cursor_plane::topology::CursorPlaneTestSnapshot,
        buffer_io: Rc<dyn crate::kms::cursor_plane::ops::CursorBufferIo>,
        width: u32,
        height: u32,
    ) {
        let device = Rc::clone(
            &self
                .devices
                .get(device_idx)
                .expect("test cursor device index")
                .device,
        );
        let device_key = self.devices[device_idx].key;
        let crtcs: Vec<_> = self
            .outputs
            .iter()
            .filter(|output| output.key.device_key == device_key)
            .map(|output| output.output.crtc)
            .collect();
        let plane = crate::kms::cursor_plane::CursorPlane::for_tests_from_snapshot(
            device, &crtcs, buffer_io, snapshot, width, height,
        );
        install_cursor_plane_for_device(
            &mut self.devices[device_idx],
            &crtcs,
            "test snapshot",
            plane,
        );
    }

    /// Run the one-time first-output factory for an opened device that began
    /// genuinely headless. Callers must invoke this only after the successful
    /// ActiveOutput insertion. All post-insertion active CRTCs on the owning
    /// device are passed to the factory, and no other card is touched.
    pub(super) fn initialize_headless_cursor_for_device_with<F>(
        &mut self,
        device_key: crate::platform::drm::DrmDeviceKey,
        boundary: &str,
        factory: F,
    ) -> bool
    where
        F: FnOnce(&mut KmsDevice, &[::drm::control::crtc::Handle], &str),
    {
        let crtcs: Vec<_> = self
            .outputs
            .iter()
            .filter(|output| output.key.device_key == device_key)
            .map(|output| output.output.crtc)
            .collect();
        let Some(device) = self
            .devices
            .iter_mut()
            .find(|device| device.key == device_key)
        else {
            return false;
        };
        if !device
            .cursor
            .should_initialize_headless_deferred(!crtcs.is_empty())
        {
            return false;
        }

        // Consume genuine-deferred before invoking the injectable factory so
        // even a test/factory panic cannot make ordinary probes look like a
        // never-attempted device. The production factory records success or
        // retryable/permanent failure in the remaining state.
        device.cursor.headless_deferred = false;
        factory(device, &crtcs, boundary);
        true
    }

    fn refresh_cursor_topology(&mut self) {
        let all_devices: HashSet<_> = self.devices.iter().map(|device| device.key).collect();
        self.refresh_cursor_topology_for_devices(&all_devices);
    }

    /// Detach the plane on every CRTC the plane has ever been bound
    /// against AND every currently-known output. Global recovery
    /// fallback only — `drain_all`, shutdown, VT-leave, DRM-master
    /// loss. Per Phase D' this also invalidates `uploaded_version`
    /// so the next acquire/modeset re-uploads cleanly.
    ///
    /// # Errors
    /// Per-CRTC failures are logged; this never returns `Err`
    /// unless the plane is unavailable.
    pub(crate) fn cursor_plane_hide_all(&mut self) -> io::Result<()> {
        let outputs: Vec<_> = self
            .outputs
            .iter()
            .map(|layout| (layout.key.device_key, layout.output.crtc))
            .collect();
        let mut found = false;
        for device_idx in 0..self.devices.len() {
            let device_key = self.devices[device_idx].key;
            self.devices[device_idx].cursor.pending_move = None;
            self.devices[device_idx].cursor.pending_move_timestamp = None;
            let permanently_disabled = self.devices[device_idx].cursor.permanently_disabled;
            let atomic_path = self.devices[device_idx].cursor.state_path()
                == crate::kms::cursor_plane::topology::CursorStatePath::Atomic;
            let Some(plane) = self.devices[device_idx].cursor.plane.as_ref() else {
                continue;
            };
            found = true;
            let mut crtcs: Vec<_> = outputs
                .iter()
                .filter(|(key, _)| *key == device_key)
                .map(|(_, crtc)| *crtc)
                .collect();
            for crtc in plane.known_crtcs() {
                if !crtcs.contains(&crtc) {
                    crtcs.push(crtc);
                }
            }
            if atomic_path || permanently_disabled {
                let bound_crtcs: HashSet<_> = crtcs
                    .iter()
                    .copied()
                    .filter(|crtc| {
                        let cursor = &self.devices[device_idx].cursor;
                        cursor.hw_bound_on(*crtc)
                            || (atomic_path
                                && cursor
                                    .atomic_cursor_commits_in_flight
                                    .get(crtc)
                                    .and_then(|commit| commit.prepared.as_ref())
                                    .is_some_and(|prepared| prepared.prior.is_some()))
                    })
                    .collect();
                crtcs.retain(|crtc| bound_crtcs.contains(crtc));
            }
            for crtc in crtcs {
                let result = if atomic_path {
                    self.detach_cursor_blocking(device_idx, crtc)
                } else {
                    self.devices[device_idx]
                        .cursor
                        .plane
                        .as_mut()
                        .expect("cursor plane was checked")
                        .hide(crtc)
                };
                if let Err(error) = result {
                    if error.kind() == io::ErrorKind::PermissionDenied {
                        log::debug!(
                            "render cursor hide_all: device {device_key} CRTC {crtc:?} (no master): {error}"
                        );
                    } else {
                        log::warn!(
                            "render cursor hide_all: device {device_key} CRTC {crtc:?} failed: {error}"
                        );
                    }
                }
            }
            if let Some(plane) = self.devices[device_idx].cursor.plane.as_mut() {
                plane.invalidate_uploaded_version();
            }
        }
        if found {
            Ok(())
        } else {
            Err(io::Error::other("cursor plane unavailable"))
        }
    }

    /// Re-arm the hardware cursor plane on every CRTC that was
    /// showing the cursor before the suspend. This restores the
    /// kernel-side cursor binding that the VT switch tore down.
    ///
    /// Called after a connector snapshot has been applied so the output list
    /// is up to date. The cursor position and hotspot come from the
    /// caller (backend passes `core.cursor_x/y` and the effective
    /// cursor's hotspot).
    pub(crate) fn rearm_cursor(&mut self, hot_x: u16, hot_y: u16, x: i32, y: i32) {
        self.refresh_cursor_topology();
        let routes: Vec<_> = self
            .outputs
            .iter()
            .enumerate()
            .filter_map(|(output_idx, layout)| {
                let device = self.device_for_key(layout.key.device_key)?;
                (device
                    .cursor
                    .crtc_states
                    .get(&layout.output.crtc)
                    .is_some_and(|state| state.resume_visible)
                    || device.cursor.hw_bound_on(layout.output.crtc))
                .then_some((output_idx, device.key, layout.output.crtc))
            })
            .collect();
        log::info!(
            "render resume rearm_cursor: outputs={} initialized_devices={} visible_routes={} hot=({hot_x},{hot_y}) pos=({x},{y})",
            self.outputs.len(),
            self.devices
                .iter()
                .filter(|device| device.cursor.plane.is_some())
                .count(),
            routes.len(),
        );
        let mut shown = 0usize;
        let mut failed = 0usize;
        for (output_idx, device_key, crtc) in routes {
            match self.cursor_plane_show_on_crtc(output_idx, hot_x, hot_y, x, y) {
                Ok(()) => {
                    shown += 1;
                    log::info!(
                        "render resume rearm_cursor: device {device_key} CRTC={crtc:?} show ok"
                    );
                }
                Err(error) => {
                    failed += 1;
                    log::warn!(
                        "render resume rearm_cursor: device {device_key} CRTC={crtc:?} show failed: {error}"
                    );
                }
            }
        }
        log::info!("render resume rearm_cursor: done — shown={shown} failed={failed}");
    }
}

pub(super) fn cursor_root_to_crtc_local(
    x: i32,
    y: i32,
    layout_x: i32,
    layout_y: i32,
    hot_x: u16,
    hot_y: u16,
) -> (i32, i32) {
    (
        x - layout_x - i32::from(hot_x),
        y - layout_y - i32::from(hot_y),
    )
}

/// Whether the cursor footprint `[dx, dx+cw) × [dy, dy+ch)` (in
/// output-local coordinates) overlaps the output's `[0, w) × [0, h)`
/// region. This is the boolean form of `cursor_footprint_rect`'s
/// non-empty condition, kept in sync so `cursor_crtc_membership_dirty`
/// decides on-output membership exactly as the scene does.
pub(super) fn cursor_footprint_intersects_output(
    dx: i32,
    dy: i32,
    cw: i32,
    ch: i32,
    w: i32,
    h: i32,
) -> bool {
    dx < w && dx + cw > 0 && dy < h && dy + ch > 0
}
