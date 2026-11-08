use super::{DirectPresentFrame, DirectScope, KmsBackend, ScanoutM0Target};

impl KmsBackend {
    pub(super) fn service_idle_direct_vrr_clears(&mut self, now: std::time::Instant) {
        let wanted_by_output = self.live_direct_vrr_wanted_by_output_at(now);
        for scope in self.pstate.direct.active_scopes() {
            let Some(activity) = self
                .pstate
                .direct
                .get(scope)
                .map(|owner| owner.vrr_activity.get())
            else {
                continue;
            };
            let Some(window) = activity.window else {
                continue;
            };
            let _ = self.direct_vrr_activity_active_at(scope, window, now);
            let Some((has_pending, has_queued)) = self
                .pstate
                .direct
                .get(scope)
                .map(|owner| (owner.pending.is_some(), owner.queued_successor.is_some()))
            else {
                continue;
            };
            if self
                .pstate
                .direct
                .get(scope)
                .is_none_or(|owner| owner.unflip_last_reason.get() != Some("client_idle"))
            {
                continue;
            }
            if let Some(owner) = self.pstate.direct.get_mut(scope) {
                owner.unflip_last_reason.set(Some("client_idle"));
            }
            for output_idx in self.direct_scope_outputs(scope) {
                if !self.direct_output_vrr_capable(output_idx) {
                    continue;
                }
                if wanted_by_output.get(output_idx).copied().unwrap_or(false) {
                    self.clear_idle_retry_if_wanted(scope);
                    if let Some(owner) = self.pstate.direct.get(scope)
                        && owner.unflip_last_reason.get() == Some("client_idle")
                    {
                        owner.unflip_last_reason.set(None);
                    }
                    continue;
                }
                let output = &self.platform.outputs[output_idx].output;
                let commit_in_flight = self
                    .platform
                    .scanout_commit_in_flight_for_output(output_idx);
                if output.vrr_clear_waiting_for_flip.get()
                    && !has_pending
                    && !has_queued
                    && !commit_in_flight
                {
                    continue;
                }
                if output
                    .vrr_clear_retry_deadline
                    .get()
                    .is_some_and(|deadline| now < deadline)
                {
                    continue;
                }
                if has_pending || has_queued || commit_in_flight {
                    output.vrr_clear_waiting_for_flip.set(true);
                    output.vrr_clear_retry_deadline.set(None);
                    continue;
                }
                if !output.vrr_committed.get() {
                    output.vrr_clear_waiting_for_flip.set(false);
                    continue;
                }
                if let Err(error) = self.submit_pending_crtc_property_for_output(
                    output_idx,
                    false,
                    now,
                    "client_idle",
                ) {
                    log::warn!(
                        "kms: standalone idle VRR clear for CRTC {:?} failed: {error}",
                        output.crtc
                    );
                }
            }
            let outputs_resolved = self.direct_scope_outputs(scope).iter().all(|&output_idx| {
                let output = &self.platform.outputs[output_idx].output;
                !output.vrr_committed.get()
                    && !output.vrr_clear_waiting_for_flip.get()
                    && output.vrr_clear_retry_deadline.get().is_none()
                    && !self
                        .platform
                        .scanout_commit_in_flight_for_output(output_idx)
            });
            if outputs_resolved
                && let Some(owner) = self.pstate.direct.get(scope)
                && owner.unflip_last_reason.get() == Some("client_idle")
            {
                owner.unflip_last_reason.set(None);
            }
        }
    }

    fn direct_scope_vrr_capable(&self, scope: DirectScope) -> bool {
        self.direct_scope_outputs(scope)
            .into_iter()
            .any(|output_idx| self.direct_output_vrr_capable(output_idx))
    }

    pub(super) fn direct_output_vrr_capable(&self, output_idx: usize) -> bool {
        self.pstate.vrr_mode != crate::launch::VrrMode::Off
            && self
                .platform
                .outputs
                .get(output_idx)
                .is_some_and(|layout| layout.output.vrr_capable())
    }

    pub(in crate::kms::render::backend) fn note_direct_vrr_present(
        &self,
        scope: DirectScope,
        candidate: &yserver_core::backend::PresentScanoutCandidate,
        target: ScanoutM0Target,
        now: std::time::Instant,
    ) {
        if target != ScanoutM0Target::Unredirected || !self.direct_scope_vrr_capable(scope) {
            return;
        }
        let wants_by_policy = match self.pstate.vrr_mode {
            crate::launch::VrrMode::Off => false,
            crate::launch::VrrMode::Property => candidate.variable_refresh,
            crate::launch::VrrMode::All => !self.vrr_client_excluded(candidate),
        };
        let Some(owner) = self.pstate.direct.get(scope) else {
            return;
        };
        let mut activity = owner.vrr_activity.get();
        let window_changed_while_active = activity.window.is_some_and(|window| {
            window != candidate.dst_window_xid
                && activity.qualifying_intervals
                    >= crate::kms::render::backend::kms::state::DIRECT_VRR_ACTIVE_INTERVALS
        });
        if window_changed_while_active {
            for output_idx in self.direct_scope_outputs(scope) {
                if self.direct_output_vrr_capable(output_idx) {
                    let output = &self.platform.outputs[output_idx].output;
                    output.vrr_clear_retry_deadline.set(None);
                    output.vrr_clear_waiting_for_flip.set(true);
                }
            }
        }
        if activity
            .window
            .is_some_and(|window| window != candidate.dst_window_xid)
        {
            activity = crate::kms::render::backend::kms::state::DirectVrrActivity {
                window: Some(candidate.dst_window_xid),
                ..Default::default()
            };
        }
        if !wants_by_policy {
            owner.vrr_activity.set(activity);
            return;
        }
        let (expired, activated_interval) = activity.note_present(candidate.dst_window_xid, now);
        owner.vrr_activity.set(activity);
        if let Some(window) = expired {
            if let Some(owner) = self.pstate.direct.get(scope) {
                owner.unflip_last_reason.set(Some("client_idle"));
            }
            self.mark_direct_vrr_expired(scope, now);
            log::debug!("kms: vrr activity expired scope={scope:?} window=0x{window:x}");
        }
        if let Some(interval) = activated_interval {
            log::debug!(
                "kms: vrr activity active scope={scope:?} window=0x{:x} interval_ns={}",
                candidate.dst_window_xid,
                interval.as_nanos()
            );
        }
    }

    pub(super) fn direct_vrr_activity_active_at(
        &self,
        scope: DirectScope,
        window: u32,
        now: std::time::Instant,
    ) -> bool {
        if !self.direct_scope_vrr_capable(scope) {
            return false;
        }
        let Some(owner) = self.pstate.direct.get(scope) else {
            return false;
        };
        let mut activity = owner.vrr_activity.get();
        let (active, expired) = activity.is_active_at(window, now);
        owner.vrr_activity.set(activity);
        if let Some(window) = expired {
            if let Some(owner) = self.pstate.direct.get(scope) {
                owner.unflip_last_reason.set(Some("client_idle"));
            }
            self.mark_direct_vrr_expired(scope, now);
            log::debug!("kms: vrr activity expired scope={scope:?} window=0x{window:x}");
        }
        active
    }

    fn mark_direct_vrr_expired(&self, scope: DirectScope, now: std::time::Instant) {
        let has_pending_or_queued = self
            .pstate
            .direct
            .get(scope)
            .is_some_and(|owner| owner.pending.is_some() || owner.queued_successor.is_some());
        for output_idx in self.direct_scope_outputs(scope) {
            if !self.direct_output_vrr_capable(output_idx) {
                continue;
            }
            let output = &self.platform.outputs[output_idx].output;
            let wait_for_flip = has_pending_or_queued
                || self
                    .platform
                    .scanout_commit_in_flight_for_output(output_idx);
            let clear_may_be_needed = output.vrr_committed.get() || wait_for_flip;
            output
                .vrr_clear_retry_deadline
                .set((clear_may_be_needed && !wait_for_flip).then_some(now));
            output
                .vrr_clear_waiting_for_flip
                .set(clear_may_be_needed && wait_for_flip);
        }
    }

    fn clear_idle_retry_if_wanted(&self, scope: DirectScope) {
        for output_idx in self.direct_scope_outputs(scope) {
            let output = &self.platform.outputs[output_idx].output;
            output.vrr_clear_retry_deadline.set(None);
            output.vrr_clear_waiting_for_flip.set(false);
        }
    }

    pub(super) fn direct_frame_wants_vrr_at(
        &self,
        frame: &DirectPresentFrame,
        scope: DirectScope,
        now: std::time::Instant,
    ) -> bool {
        if frame.target != ScanoutM0Target::Unredirected {
            return false;
        }
        let base_wanted = match self.pstate.vrr_mode {
            crate::launch::VrrMode::Off => false,
            crate::launch::VrrMode::Property => frame.candidate.variable_refresh,
            crate::launch::VrrMode::All => !self.vrr_client_excluded(&frame.candidate),
        };
        if !base_wanted
            || !self.direct_scope_vrr_capable(scope)
            || !self.direct_vrr_activity_active_at(scope, frame.candidate.dst_window_xid, now)
        {
            return false;
        }
        self.clear_idle_retry_if_wanted(scope);
        true
    }

    pub(in crate::kms::render::backend) fn live_direct_vrr_wanted_by_output(&self) -> Vec<bool> {
        self.live_direct_vrr_wanted_by_output_at(self.monotonic_now())
    }

    pub(in crate::kms::render::backend) fn live_direct_vrr_wanted_by_output_at(
        &self,
        now: std::time::Instant,
    ) -> Vec<bool> {
        (0..self.platform.outputs.len())
            .map(|output_idx| {
                if !self.direct_output_vrr_capable(output_idx) {
                    return false;
                }
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
                self.direct_frame_wants_vrr_at(frame, scope, now)
            })
            .collect()
    }
}
