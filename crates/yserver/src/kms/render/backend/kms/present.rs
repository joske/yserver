use crate::kms::render::{
    backend::{
        ABSOLUTE_SEQ_TAG, DirectPresentFrame, DirectScope, KmsBackend, ScanoutM0Target,
        TEARING_ENTRY_PROBATION, TearingState,
        kms::scanout::{
            phase_b_flip_in_flight_for_scheduler, scanout_direct_eligible,
            scanout_m2_is_authoritative_root,
        },
    },
    platform::CrtcKey,
};
use ash::vk;
use std::{collections::HashSet, io, rc::Rc};
use yserver_core::backend::{Backend, PresentCaps, PresentScanoutCandidate, PresentSourceWait};

/// `user_data` value for an absolute per-target arm on `crtc_id` — the
/// single producer of the encoding `on_crtc_sequence_event` decodes, so
/// the two can never drift independently of each other. Free function
/// (not inlined into the ioctl closure) so a unit test can round-trip
/// it straight into `on_crtc_sequence_event` without a live DRM fd.
pub(in crate::kms::render::backend) fn absolute_seq_user_data(crtc_id: u32) -> u64 {
    ABSOLUTE_SEQ_TAG | u64::from(crtc_id)
}

impl KmsBackend {
    fn direct_candidate_authoritative_root(
        &self,
        candidate: PresentScanoutCandidate,
        target: ScanoutM0Target,
    ) -> bool {
        self.direct_candidate_scope(candidate, target) == Some(DirectScope::Root)
    }

    pub(in crate::kms::render::backend) fn direct_candidate_scope(
        &self,
        candidate: PresentScanoutCandidate,
        target: ScanoutM0Target,
    ) -> Option<DirectScope> {
        let root = (u32::from(self.platform.fb_w), u32::from(self.platform.fb_h));
        let rect = self
            .store
            .lookup(candidate.paint_dst_host_xid)
            .and_then(|id| self.window_absolute_rect(id))?;
        let source_extent = (
            u32::from(candidate.src_width),
            u32::from(candidate.src_height),
        );
        let root_rect = vk::Rect2D {
            offset: vk::Offset2D::default(),
            extent: vk::Extent2D {
                width: root.0,
                height: root.1,
            },
        };
        let root_coverage = rect == root_rect && source_extent == root;
        if root_coverage
            && matches!(
                target,
                ScanoutM0Target::Cow | ScanoutM0Target::CowDescendant
            )
            && self.direct_shape_chain_hides_root(candidate.paint_dst_host_xid, root_rect)
        {
            // Muffin empties the COW while a fullscreen client is unredirected;
            // Xorg shows nothing of the shaped-away stage Present.
            return None;
        }
        if scanout_m2_is_authoritative_root(target, root_coverage) {
            return Some(DirectScope::Root);
        }

        if !matches!(
            target,
            ScanoutM0Target::Cow | ScanoutM0Target::CowDescendant | ScanoutM0Target::Unredirected
        ) {
            return None;
        }
        let output_idx = self.platform.outputs.iter().position(|output| {
            rect.offset.x == output.x
                && rect.offset.y == output.y
                && rect.extent.width == u32::from(output.width)
                && rect.extent.height == u32::from(output.height)
                && source_extent == (u32::from(output.width), u32::from(output.height))
        })?;
        let scope = DirectScope::Output(output_idx);
        if matches!(
            target,
            ScanoutM0Target::Cow | ScanoutM0Target::CowDescendant
        ) && self.direct_shape_chain_hides_root(
            candidate.paint_dst_host_xid,
            self.direct_scope_rect(scope),
        ) {
            return None;
        }
        Some(scope)
    }

    pub(in crate::kms::render::backend) fn direct_scope_rect(
        &self,
        scope: DirectScope,
    ) -> vk::Rect2D {
        match scope {
            DirectScope::Root => vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: u32::from(self.platform.fb_w),
                    height: u32::from(self.platform.fb_h),
                },
            },
            DirectScope::Output(output_idx) => {
                self.platform
                    .outputs
                    .get(output_idx)
                    .map_or(vk::Rect2D::default(), |output| vk::Rect2D {
                        offset: vk::Offset2D {
                            x: output.x,
                            y: output.y,
                        },
                        extent: vk::Extent2D {
                            width: u32::from(output.width),
                            height: u32::from(output.height),
                        },
                    })
            }
        }
    }

    pub(in crate::kms::render::backend) fn request_tearing_cursor_handoff(&mut self) {
        if self.pstate.tearing.is_on()
            && self.tearing_cursor_wants_visible()
            && self.pstate.direct.root().active()
        {
            self.request_direct_unflip_for(DirectScope::Root, "tearing_cursor_handoff");
        }
    }

    pub(in crate::kms::render::backend) fn direct_candidate_gate(
        &self,
        candidate: PresentScanoutCandidate,
        target: ScanoutM0Target,
        scope: DirectScope,
        unbordered: bool,
        include_cursor_term: bool,
    ) -> bool {
        let scene_eligible =
            self.direct_scope_scene_eligible(candidate.paint_dst_host_xid, target, scope);
        let outputs = self.direct_scope_outputs(scope);
        let cursor_hw = !include_cursor_term || self.direct_cursor_eligible_for(&outputs);
        let root_overlay_empty = self.direct_root_overlay_empty_for_outputs(&outputs);
        !self.platform.any_output_transformed()
            && self.direct_present_crtc_eligible(scope, candidate.crtc_id, candidate.crtc_epoch)
            && scanout_direct_eligible(
                self.scanout_allowed(),
                self.kms_outputs_active,
                cursor_hw,
                root_overlay_empty,
                scene_eligible,
                unbordered,
                candidate.x_off,
                candidate.y_off,
                candidate.valid_region_xid,
            )
    }

    /// Whether the current scene still allows a retained target to be scanned
    /// out on this owner. Admission and tick revalidation share this policy so
    /// an Unredirected window uses the scene's existing per-scope topmost test,
    /// while COW targets use only the shape-chain coverage rule.
    pub(in crate::kms::render::backend) fn direct_scope_scene_eligible(
        &self,
        leaf_xid: u32,
        target: ScanoutM0Target,
        scope: DirectScope,
    ) -> bool {
        if !matches!(
            target,
            ScanoutM0Target::Cow | ScanoutM0Target::CowDescendant | ScanoutM0Target::Unredirected
        ) {
            return false;
        }
        let direct_rect = self.direct_scope_rect(scope);
        self.direct_shape_chain_covers_root(leaf_xid, direct_rect)
            && (!matches!(target, ScanoutM0Target::Unredirected)
                || self.unredirected_direct_scene_eligible(leaf_xid, direct_rect))
    }

    /// Whether all retained frames for an output owner still pass the scene
    /// rule used at Present admission. The scheduler and the page-flip path
    /// both need this before a queued successor can be submitted.
    pub(in crate::kms::render::backend) fn direct_output_frames_scene_eligible(
        &self,
        output_idx: usize,
    ) -> bool {
        let scope = DirectScope::Output(output_idx);
        self.pstate.direct.get(scope).is_some_and(|owner| {
            [
                owner.current.as_ref(),
                owner.pending.as_ref(),
                owner.queued_successor.as_ref(),
            ]
            .into_iter()
            .flatten()
            .all(|frame| {
                let host_xid = frame.candidate.paint_dst_host_xid;
                let leaf_id = self.store.lookup(host_xid);
                let paint_target = self.resolve_paint_target(host_xid);
                let paint_id = paint_target.map(|target| target.backing_id());
                let target = self.scanout_m0_target(host_xid, leaf_id, paint_id);
                self.direct_scope_scene_eligible(host_xid, target, scope)
            })
        })
    }

    pub(in crate::kms::render::backend) fn tearing_gate_ignoring_cursor(
        &self,
        candidate: PresentScanoutCandidate,
        target: ScanoutM0Target,
        unbordered: bool,
    ) -> bool {
        self.direct_candidate_scope(candidate, target)
            .is_some_and(|scope| {
                self.direct_candidate_gate(candidate, target, scope, unbordered, false)
                    && self.direct_scope_outputs(scope).iter().all(|&output_idx| {
                        self.platform.outputs.get(output_idx).is_some_and(|output| {
                            self.platform
                                .device_for_key(output.key.device_key)
                                .is_some_and(|device| device.async_page_flip)
                        })
                    })
            })
    }

    /// Revalidate the retained W Present against the live tree between
    /// Presents. This deliberately shares the same cursor-ignored gate used
    /// for admission, including its target-specific stacking and shape rules.
    pub(in crate::kms::render::backend) fn tearing_scene_still_owned(&self) -> bool {
        let TearingState::On {
            window,
            host_window,
            crtc_epoch,
            admission,
        } = self.pstate.tearing.state
        else {
            return false;
        };
        let candidate = admission.candidate;
        if admission.target == ScanoutM0Target::Other
            || admission.root_host_xid != self.core.window_id
            || host_window != candidate.paint_dst_host_xid
            || window != candidate.dst_window_xid
            || crtc_epoch != candidate.crtc_epoch
        {
            return false;
        }
        let leaf_id = self.store.lookup(host_window);
        let paint_target = self.resolve_paint_target(host_window);
        let paint_id = paint_target.map(|target| target.backing_id());
        let target = self.scanout_m0_target(host_window, leaf_id, paint_id);
        let unbordered = paint_target.is_none_or(|target| !target.has_border_clip());
        self.tearing_gate_ignoring_cursor(candidate, target, unbordered)
    }

    fn note_tearing_present(
        &mut self,
        candidate: PresentScanoutCandidate,
        authoritative_root: bool,
        target: ScanoutM0Target,
        gate_ignoring_cursor: bool,
    ) -> bool {
        let was_on = self.pstate.tearing.is_on();
        match self.pstate.tearing.state {
            TearingState::On {
                window, crtc_epoch, ..
            } if candidate.dst_window_xid == window
                && (candidate.crtc_epoch != crtc_epoch
                    || !authoritative_root
                    || !gate_ignoring_cursor
                    || candidate.options
                        & yserver_core::present_scheduler::PRESENT_OPTION_ASYNC
                        == 0) =>
            {
                self.exit_tearing("direct_present_no_longer_qualifies");
            }
            TearingState::On { window, .. }
                if authoritative_root && candidate.dst_window_xid != window =>
            {
                self.exit_tearing("authoritative_root_window_changed");
            }
            TearingState::On {
                window,
                host_window,
                crtc_epoch,
                ..
            } => {
                if authoritative_root
                    && candidate.dst_window_xid == window
                    && gate_ignoring_cursor
                    && candidate.options & yserver_core::present_scheduler::PRESENT_OPTION_ASYNC
                        != 0
                {
                    self.pstate.tearing.state = TearingState::On {
                        window,
                        host_window,
                        crtc_epoch,
                        admission: crate::kms::render::backend::TearingAdmissionContext {
                            candidate,
                            target,
                            root_host_xid: self.core.window_id,
                        },
                    };
                }
                return was_on;
            }
            TearingState::Off => {}
        }

        if !authoritative_root {
            return was_on;
        }
        let qualifies = gate_ignoring_cursor
            && candidate.options & yserver_core::present_scheduler::PRESENT_OPTION_ASYNC != 0;
        if !qualifies {
            self.pstate.tearing.reset_probation();
            return was_on;
        }

        if self.pstate.tearing.probation_owner != Some(candidate.dst_window_xid) {
            self.pstate.tearing.probation_owner = Some(candidate.dst_window_xid);
            self.pstate.tearing.probation = 1;
        } else {
            self.pstate.tearing.probation = self
                .pstate
                .tearing
                .probation
                .saturating_add(1)
                .min(TEARING_ENTRY_PROBATION);
        }
        if self.pstate.tearing.probation >= TEARING_ENTRY_PROBATION
            && !self.scene.cursor_transition_pending()
        {
            self.pstate.tearing.state = TearingState::On {
                window: candidate.dst_window_xid,
                // Lifecycle teardown reports the window's host ID. For a
                // redirected window, completion may name its backing pixmap.
                host_window: candidate.paint_dst_host_xid,
                crtc_epoch: candidate.crtc_epoch,
                admission: crate::kms::render::backend::TearingAdmissionContext {
                    candidate,
                    target,
                    root_host_xid: self.core.window_id,
                },
            };
            self.pstate.direct.root_mut().cursor_bound_all = false;
            self.pstate.tearing.hide_failed_logged = false;
            log::info!(
                "kms: entering tearing for window=0x{:x} crtc_epoch={}",
                candidate.dst_window_xid,
                candidate.crtc_epoch,
            );
            if self.tearing_cursor_wants_visible() {
                self.request_direct_unflip_for(DirectScope::Root, "tearing_cursor_handoff");
            }
        }
        was_on
    }

    fn register_tearing_copy_fallback(
        &mut self,
        candidate: PresentScanoutCandidate,
        authoritative_root: bool,
        source_id: Option<crate::kms::render::store::DrawableId>,
    ) {
        if !authoritative_root {
            return;
        }
        if self.pstate.direct.root().active()
            && (self.pstate.direct.root().unflip_requested || self.pstate.direct.root().hold_direct)
            && matches!(
                self.pstate.tearing.state,
                TearingState::On { window, .. } if window == candidate.dst_window_xid
            )
        {
            self.pstate.direct.root_mut().unflip_fallback_source = source_id;
            self.pstate.direct.root_mut().unflip_shadow_ready = false;
        }
    }

    pub(super) fn exit_tearing(&mut self, reason: &'static str) {
        let was_on = self.pstate.tearing.is_on();
        self.pstate.tearing.state = TearingState::Off;
        self.pstate.tearing.reset_probation();
        self.pstate.tearing.hide_failed_logged = false;
        self.pstate.tearing.last_cursor_compose = None;
        self.pstate.tearing.deferred_cursor_wake = None;
        self.platform.async_ledger.clear();
        self.platform.set_tearing_on(false);
        if was_on {
            log::info!("kms: leaving tearing: {reason}");
        }
    }

    pub(in crate::kms::render::backend) fn exit_tearing_for_window(
        &mut self,
        host_xid: u32,
        reason: &'static str,
    ) {
        if matches!(self.pstate.tearing.state, TearingState::On { host_window, .. } if host_window == host_xid)
        {
            self.exit_tearing(reason);
        }
    }

    pub(in crate::kms::render::backend) fn poke_present_completion_wakeup(&self, context: &str) {
        match self.platform.wakeup_eventfd.write(1) {
            Ok(_) | Err(nix::errno::Errno::EAGAIN) => {}
            Err(e) => log::warn!("{context}: wakeup_eventfd write: {e}"),
        }
    }

    /// Watch a batch's sync_file in the completion poller. A registration
    /// failure turns the wait into `Ready` (immediate drain).
    pub(in crate::kms::render::backend) fn register_present_batch_wait(
        &mut self,
        wait: crate::kms::render::present_batch::PresentBatchWait,
    ) -> crate::kms::render::present_batch::PresentBatchWait {
        use crate::kms::render::present_batch::PresentBatchWait;
        let PresentBatchWait::Fd(fd) = &wait else {
            return wait;
        };
        use std::os::fd::{AsFd, AsRawFd};
        // Token mirrors the fd's raw value (matches the native
        // epoll-data / kqueue-udata the inline code used).
        let token = u64::try_from(fd.as_raw_fd()).unwrap_or_default();
        match self
            .platform
            .present_completion_epfd
            .register(fd.as_fd(), token)
        {
            Ok(()) => wait,
            Err(e) => {
                if std::mem::replace(&mut self.pstate.present_poll_register_warned, true) {
                    log::debug!(
                        "deferred PRESENT: poll ADD sync_file fd failed: {e}; \
                         using immediate drain fallback"
                    );
                } else {
                    log::warn!(
                        "deferred PRESENT: poll ADD sync_file fd failed: {e}; \
                         using immediate drain fallback (further failures log at debug)"
                    );
                }
                PresentBatchWait::Ready
            }
        }
    }

    /// Clear the completion wake before a drain re-checks every batch.
    pub(in crate::kms::render::backend) fn clear_present_completion_wake(&self) {
        // nix's EventFd::read returns Ok(u64) with the accumulated counter;
        // Err(EAGAIN) when zero — benign in either case because we re-check
        // batch readiness directly.
        match self.platform.wakeup_eventfd.read() {
            Ok(_) | Err(nix::errno::Errno::EAGAIN) => {}
            Err(e) => log::warn!("wakeup_eventfd read: {e}"),
        }
    }

    pub(in crate::kms::render::backend) fn unregister_present_batch_fd(
        &mut self,
        batch: &crate::kms::render::present_batch::PendingPresentBatch,
    ) {
        use crate::kms::render::present_batch::PresentBatchWait;

        let _keep_export_semaphore_alive_until_batch_drop = batch.signal.as_ref();
        if let PresentBatchWait::Fd(fd) = &batch.wait {
            use std::os::fd::AsFd;
            if let Err(e) = self.platform.present_completion_epfd.unregister(fd.as_fd()) {
                log::warn!("deferred PRESENT: poll DEL sync_file fd failed: {e}");
            }
        }
    }

    pub(in crate::kms::render::backend) fn present_flip_in_flight_for_output(
        &self,
        output_idx: usize,
    ) -> bool {
        let owner = self
            .pstate
            .direct
            .scope_for_output(output_idx)
            .and_then(|scope| self.pstate.direct.get(scope));
        phase_b_flip_in_flight_for_scheduler(
            self.scene.has_pending_page_flip(output_idx),
            owner
                .and_then(|owner| owner.pending.as_ref())
                .is_some_and(|frame| frame.awaiting_outputs.contains(&output_idx)),
            owner.is_some_and(|owner| owner.unflip_outputs_to_replace.contains(&output_idx)),
        )
    }

    /// Standalone CRTC sequence events may pace Pixmap completions only when
    /// the selected output has no visible scanout work pending. This is
    /// evaluated when the event is consumed, closing the race where an idle
    /// arm is followed by damage and a submitted flip before the sequence
    /// arrives. Scene damage remains global because projecting arbitrary
    /// window damage to one output requires the scene walk itself; treating
    /// it conservatively as work in every domain is safe, while unrelated
    /// pageflips are isolated here.
    pub(in crate::kms::render::backend) fn present_completion_is_idle_for(
        &self,
        crtc_key: CrtcKey,
    ) -> bool {
        self.platform
            .output_index_for_crtc(crtc_key)
            .is_some_and(|output_idx| {
                !self.present_flip_in_flight_for_output(output_idx) && !self.scene_wants_compose()
            })
    }

    /// Clear both armed-target maps (relative single-slot + absolute
    /// per-target).
    ///
    /// Called at lifecycle edges where the kernel has already dropped all
    /// queued CRTC sequences: VT suspend (`run_suspend`) and DPMS-off
    /// (`set_dpms_power` sleep side). An in-flight `DRM_CRTC_SEQUENCE` will
    /// never arrive once DRM master is released or the CRTC is disabled, so
    /// any surviving entry would permanently stall the per-CRTC arm cycle.
    pub(crate) fn clear_all_armed_vblank_targets(&mut self) {
        if !self.pstate.armed_vblank_targets.is_empty() {
            self.pstate.armed_vblank_targets.clear();
        }
        if !self.pstate.absolute_vblank_targets.is_empty() {
            self.pstate.absolute_vblank_targets.clear();
        }
        self.platform.vblank_fallback.software.clear();
    }

    /// Drop armed-target entries (both the relative single-slot map and
    /// the absolute per-target map) for CRTCs no longer owned by any
    /// live output. Called after `requery_outputs_and_modeset` retires a
    /// disconnected connector. (A new sequence event for a stale CRTC is
    /// already dropped by `on_crtc_sequence_event`, but the maps must not
    /// retain dead entries — they would mis-dedup a CRTC id reused by a
    /// later hotplug.)
    pub(crate) fn prune_armed_targets_to_live_outputs(&mut self) {
        let live: std::collections::HashSet<CrtcKey> = self
            .platform
            .outputs
            .iter()
            .map(CrtcKey::for_output)
            .collect();
        self.pstate
            .armed_vblank_targets
            .retain(|key, _| live.contains(key));
        self.pstate
            .absolute_vblank_targets
            .retain(|key, _| live.contains(key));
    }

    /// Side-effect-free `DRM_CRTC_SEQUENCE` event handler.
    ///
    /// `user_data` is echoed verbatim from whichever arm queued this
    /// sequence: the low 32 bits are always the crtc_id, and the high
    /// bit (`ABSOLUTE_SEQ_TAG`) discriminates an absolute per-target arm
    /// from the untagged single-slot relative idle arm.
    ///
    /// **Invariant** (clear-arm before any drop):
    /// 1. Untagged event: immediately remove the relative arm's
    ///    armed-target entry for this CRTC — ANY received sequence event
    ///    proves the kernel's clock advanced on that pipe, so the arm is
    ///    spent (whether or not the output is still live). Tagged event:
    ///    retire absolute entries with `target <= sequence` from this
    ///    CRTC's absolute set instead — the two arms are independent, so
    ///    a tagged event must NOT clear the relative slot and vice versa
    ///    (spec §msc-due: neither arm suppresses the other).
    /// 2. Resolve `crtc_id_raw` to a live output index; drop if stale.
    /// 3. Validate `time_ns >= 0`; negative/malformed → log + drop.
    /// 4. Record `(sequence /*msc*/, ust_micros)` into the general per-output
    ///    clock so `present_get_ust_msc` reflects the advance.
    /// 5. Record it in the completion clock only if the scene is idle when
    ///    the event is consumed.
    ///
    /// Clock recording (steps 2-5) is unconditional for both arm kinds.
    ///
    /// NEVER mutates scanout BO state, scene state, or triggers a flip
    /// (black-scanout-regression guard).
    pub(crate) fn on_crtc_sequence_event(
        &mut self,
        device_key: crate::platform::drm::DrmDeviceKey,
        user_data: u64,
        time_ns: i64,
        sequence: u64,
    ) {
        let tagged = user_data & ABSOLUTE_SEQ_TAG != 0;
        let crtc_id_raw = user_data as u32;
        // (1) Clear-arm by kind, BEFORE any validity check.
        let crtc_handle = ::drm::control::from_u32(crtc_id_raw);
        let crtc_key = crtc_handle.map(|crtc| CrtcKey::new(device_key, crtc));
        if tagged {
            if let Some(key) = crtc_key
                && let Some(targets) = self.pstate.absolute_vblank_targets.get_mut(&key)
            {
                targets.retain(|&target| {
                    yserver_core::present_scheduler::msc_is_after(target, sequence)
                });
                if targets.is_empty() {
                    self.pstate.absolute_vblank_targets.remove(&key);
                }
            }
        } else if let Some(key) = crtc_key {
            self.pstate.armed_vblank_targets.remove(&key);
        }
        let Some(handle) = crtc_handle else {
            log::warn!("PRESENT-DBG: CrtcSequence bogus crtc_id={crtc_id_raw} — dropped");
            return;
        };
        // (2) Stale CRTC → drop (arm already cleared above).
        let crtc_key = CrtcKey::new(device_key, handle);
        let Some(output_idx) = self.platform.output_index_for_crtc(crtc_key) else {
            log::warn!(
                "PRESENT-DBG: CrtcSequence for unknown crtc_id={crtc_id_raw} \
                 on device {device_key} (output removed?) — dropped"
            );
            return;
        };
        // (3) time_ns validity → microseconds (ust_msc stores micros).
        let Ok(ns) = u64::try_from(time_ns) else {
            log::warn!(
                "PRESENT-DBG: CrtcSequence negative time_ns={time_ns} \
                 crtc_id={crtc_id_raw} — dropped"
            );
            return;
        };
        // (4) Always advance the general vblank clock. An untagged relative
        // sequence advances the completion clock only as an idle fallback.
        // A tagged absolute sequence was explicitly armed for Present work
        // at this MSC, so it is also a valid completion-clock wake while a
        // page flip is active; otherwise the wake observes a stale clock and
        // the completion still waits for the delayed flip it was meant to
        // avoid.
        let ust = ns / 1000;
        let completion_eligible = tagged || self.present_completion_is_idle_for(crtc_key);
        self.platform.record_vblank_clock(crtc_key, sequence, ust);
        if completion_eligible {
            self.platform.record_completion_clock(
                crtc_key,
                yserver_core::backend::PresentClockSample {
                    msc: sequence,
                    ust,
                    source: yserver_core::backend::PresentClockSource::IdleSequence,
                },
                None,
            );
        }
        log::debug!(
            target: "present_pace",
            "present_clock sample source=sequence output={output_idx} msc={sequence} \
             ust={ust} completion_eligible={completion_eligible}"
        );
    }

    /// Testable seam for `arm_idle_vblanks`: `armer` performs the actual
    /// ioctl (or a stub in tests). Arms a single one-shot vblank only in the
    /// selected Present's device-qualified CRTC domain. One in-flight
    /// sequence per domain is deduplicated independently.
    pub(crate) fn arm_idle_vblanks_with<F>(
        &mut self,
        crtc_key: CrtcKey,
        target_mscs: &[u64],
        mut armer: F,
    ) -> std::io::Result<usize>
    where
        F: FnMut(CrtcKey) -> std::io::Result<bool>,
    {
        if target_mscs.is_empty() {
            return Ok(0);
        }
        // Master loss / VT suspend (`scanout_allowed`) or DPMS off
        // (`kms_outputs_active`): the kernel discarded any queued sequences
        // and a powered-down CRTC rejects new ones with EINVAL, so drop our
        // bookkeeping and skip arming rather than retry every iteration.
        if !self.scanout_allowed() || !self.kms_outputs_active {
            self.clear_all_armed_vblank_targets();
            return Ok(0);
        }
        if !self.kernel_vblank_arm_available(crtc_key.device_key) {
            let now = (self.pstate.monotonic_clock)();
            return Ok(usize::from(
                self.platform.arm_software_vblank(crtc_key, now),
            ));
        }
        if self.pstate.armed_vblank_targets.contains_key(&crtc_key) {
            return Ok(0);
        }
        if armer(crtc_key)? {
            self.pstate.armed_vblank_targets.insert(crtc_key, 0);
            Ok(1)
        } else {
            Ok(0)
        }
    }

    /// Testable seam for the absolute per-target vblank arm (spec
    /// §msc-due future-target rule): `armer` performs the actual ioctl
    /// (or a stub in tests), given `(crtc_id, target)`. Unlike
    /// `arm_idle_vblanks_with`'s single in-flight slot per CRTC, this
    /// tracks an independent **set** of already-armed target MSCs per
    /// CRTC in `absolute_vblank_targets` — multiple in-flight
    /// `CRTC_QUEUE_SEQUENCE`s per CRTC are legal, and a target already in
    /// the set is skipped (dedup) rather than re-armed.
    ///
    /// Arms only the explicitly selected device-qualified CRTC. Callers
    /// resolve the RANDR CRTC XID through the current live output inventory
    /// before entering this seam.
    ///
    /// Returns the count of targets now **covered** by an in-flight arm
    /// — newly issued OR already armed — not just newly issued ioctls
    /// (see the trait doc on `arm_present_absolute_vblank`): the caller
    /// re-arms every iteration, and if a still-parked, already-armed
    /// target counted as 0 here, it would collide with the `Ok(0)` /
    /// `Err` "arm failed, execute immediately" contract and fire early.
    pub(crate) fn arm_present_absolute_vblank_with<F>(
        &mut self,
        crtc_key: CrtcKey,
        targets: &[u64],
        mut armer: F,
    ) -> std::io::Result<usize>
    where
        F: FnMut(CrtcKey, u64 /*target*/) -> std::io::Result<bool>,
    {
        if targets.is_empty() {
            return Ok(0);
        }
        // Master loss / VT suspend (`scanout_allowed`) or DPMS off
        // (`kms_outputs_active`): the kernel discarded any queued sequences
        // and a powered-down CRTC rejects new ones with EINVAL, so drop our
        // bookkeeping and skip arming rather than retry every iteration.
        if !self.scanout_allowed() || !self.kms_outputs_active {
            self.clear_all_armed_vblank_targets();
            return Ok(0);
        }
        if !self.kernel_vblank_arm_available(crtc_key.device_key) {
            return Ok(0);
        }
        let mut covered = 0;
        for &target in targets {
            let already_armed = self
                .pstate
                .absolute_vblank_targets
                .get(&crtc_key)
                .is_some_and(|set| set.contains(&target));
            if already_armed {
                covered += 1;
                continue;
            }
            if armer(crtc_key, target)? {
                self.pstate
                    .absolute_vblank_targets
                    .entry(crtc_key)
                    .or_default()
                    .insert(target);
                covered += 1;
            }
        }
        Ok(covered)
    }

    pub(in crate::kms::render::backend) fn arm_idle_vblanks_ioctl(
        &mut self,
        randr_crtc_id: u32,
        target_mscs: &[u64],
    ) -> std::io::Result<usize> {
        let Some(crtc_key) = self.present_crtc_key(randr_crtc_id) else {
            return Ok(0);
        };
        let Some(mut kernel) = self.kernel_vblank_armer(crtc_key) else {
            return Ok(0);
        };
        let result = self.arm_idle_vblanks_with(crtc_key, target_mscs, |crtc_key| {
            let crtc_id = u32::from(crtc_key.crtc);
            kernel.arm(crtc_key, true, 1, u64::from(crtc_id), crtc_id)
        });
        let kernel_lost = self.finish_kernel_vblank_armer(crtc_key, kernel);
        if kernel_lost && matches!(result, Ok(0)) {
            // The device just proved it has no kernel vblank: arm the software one now.
            return self.arm_idle_vblanks_with(crtc_key, target_mscs, |_| Ok(false));
        }
        result
    }

    /// Whether `device_key` still has a kernel vblank event source:
    /// CRTC_QUEUE_SEQUENCE, or the legacy WAIT_VBLANK fallback.
    pub(in crate::kms::render::backend) fn kernel_vblank_arm_available(
        &self,
        device_key: crate::platform::drm::DrmDeviceKey,
    ) -> bool {
        !self
            .pstate
            .crtc_queue_sequence_unsupported_devices
            .contains(&device_key)
            || self.platform.wait_vblank_supported(device_key)
    }

    fn kernel_vblank_armer(&self, crtc_key: CrtcKey) -> Option<KernelVblankArmer> {
        let device = self.platform.device_for_key(crtc_key.device_key)?;
        Some(KernelVblankArmer {
            device: Rc::clone(&device.device),
            source: Rc::clone(&device.page_flip_event_source),
            sequence_supported: !self
                .pstate
                .crtc_queue_sequence_unsupported_devices
                .contains(&crtc_key.device_key),
            wait_vblank_supported: self.platform.wait_vblank_supported(crtc_key.device_key),
            pipe: self.platform.vblank_fallback.pipes.get(&crtc_key).copied(),
            newly_sequence_unsupported: false,
            wait_vblank_error: None,
        })
    }

    /// Latch what the armer learned; returns whether the device has just
    /// lost its last kernel vblank source.
    fn finish_kernel_vblank_armer(&mut self, crtc_key: CrtcKey, kernel: KernelVblankArmer) -> bool {
        if let Some(pipe) = kernel.pipe {
            self.platform.vblank_fallback.pipes.insert(crtc_key, pipe);
        }
        if kernel.newly_sequence_unsupported {
            log::warn!(
                "DRM_IOCTL_CRTC_QUEUE_SEQUENCE returned EOPNOTSUPP on {} — disabling \
                 sequence arming on that device; falling back to DRM_IOCTL_WAIT_VBLANK",
                crtc_key.device_key
            );
            self.pstate
                .crtc_queue_sequence_unsupported_devices
                .insert(crtc_key.device_key);
        }
        let Some(error) = kernel.wait_vblank_error else {
            return false;
        };
        self.platform
            .note_wait_vblank_error(crtc_key.device_key, &error);
        !self.kernel_vblank_arm_available(crtc_key.device_key)
    }

    /// Fire due software vblanks: each advances its CRTC's MSC by one and
    /// feeds the sequence-event path, unless a flip or another event already
    /// advanced that MSC since the arm.
    pub(in crate::kms::render::backend) fn service_software_vblanks(&mut self) {
        if self.platform.vblank_fallback.software.is_empty() {
            return;
        }
        if !self.scanout_allowed() || !self.kms_outputs_active {
            self.platform.vblank_fallback.software.clear();
            return;
        }
        let now = (self.pstate.monotonic_clock)();
        for (crtc_key, vblank) in self.platform.take_due_software_vblanks(now) {
            let Some(output_idx) = self.platform.output_index_for_crtc(crtc_key) else {
                continue;
            };
            if self.platform.present_get_ust_msc(crtc_key).0 != vblank.armed_msc
                || self.present_flip_in_flight_for_output(output_idx)
            {
                continue;
            }
            let msc = self.platform.advance_software_msc(crtc_key);
            let time_ns = crate::drm::atomic::monotonic_timestamp()
                .and_then(|now| i64::try_from(now.as_nanos()).ok())
                .unwrap_or(0);
            log::debug!(
                target: "present_pace",
                "present_clock software vblank output={output_idx} msc={msc}"
            );
            self.on_crtc_sequence_event(
                crtc_key.device_key,
                u64::from(u32::from(crtc_key.crtc)),
                time_ns,
                msc,
            );
        }
    }
}

/// One kernel vblank arm attempt: CRTC_QUEUE_SEQUENCE while the device takes
/// it, then the legacy WAIT_VBLANK event (Xorg `ms_queue_vblank`).
struct KernelVblankArmer {
    device: Rc<crate::drm::Device>,
    source: Rc<dyn crate::kms::render::platform::flip_events::PageFlipEventSource>,
    sequence_supported: bool,
    wait_vblank_supported: bool,
    pipe: Option<u32>,
    newly_sequence_unsupported: bool,
    wait_vblank_error: Option<io::Error>,
}

impl KernelVblankArmer {
    /// `Ok(false)` when the device has no kernel vblank source left.
    fn arm(
        &mut self,
        crtc_key: CrtcKey,
        relative: bool,
        target: u64,
        user_data: u64,
        signal: u32,
    ) -> io::Result<bool> {
        if self.sequence_supported {
            match crate::drm::page_flip::queue_crtc_sequence(
                &self.device,
                u32::from(crtc_key.crtc),
                relative,
                target,
                user_data,
            ) {
                Ok(_) => return Ok(true),
                Err(e)
                    if e.raw_os_error() == Some(libc::EOPNOTSUPP)
                        || e.raw_os_error() == Some(libc::ENOTTY) =>
                {
                    self.sequence_supported = false;
                    self.newly_sequence_unsupported = true;
                }
                Err(e) => return Err(e),
            }
        }
        if !self.wait_vblank_supported {
            return Ok(false);
        }
        let Some(pipe) = self
            .pipe
            .or_else(|| crate::drm::page_flip::crtc_pipe_index(&self.device, crtc_key.crtc))
        else {
            self.wait_vblank_supported = false;
            self.wait_vblank_error = Some(io::Error::from_raw_os_error(libc::EINVAL));
            return Ok(false);
        };
        self.pipe = Some(pipe);
        // WAIT_VBLANK takes the low 32 bits of the target, as in Xorg.
        match self
            .source
            .queue_vblank_event(&self.device, pipe, relative, target as u32, signal)
        {
            Ok(_) => Ok(true),
            Err(e)
                if crate::kms::render::platform::vblank_fallback::wait_vblank_unsupported_error(
                    &e,
                ) =>
            {
                self.wait_vblank_supported = false;
                self.wait_vblank_error = Some(e);
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }
}

// ───────────────────────────────────────────────────────────────
// `Backend` trait implementation. The shape:
//
// A. Pure accessors — return values from `self.core` or local
//    constants identical to v1.
// B. Bookkeeping mutations — mutate `self.core` (XID map etc.).
// C. Mixed bookkeeping + storage — log a gap; for ops that must
//    return a handle, mint a fresh xid via `self.core.next_host_xid()`
//    so subsequent xid_map lookups stay consistent.
// D. Paint / RENDER / scene — log a gap, return Ok or the
//    default-impl shape.
// ───────────────────────────────────────────────────────────────

impl KmsBackend {
    /// Resolve one protocol-visible RANDR CRTC XID into the live KMS clock
    /// domain that currently owns it.
    ///
    /// `crtc_key_by_id` deliberately names the stable connector resource,
    /// not a raw DRM handle: RANDR keeps CRTC resources for disconnected/off
    /// outputs and a later modeset may assign a different kernel CRTC. Walk
    /// through the current `ActiveOutput` inventory on every Present call so
    /// an off/stale resource cleanly has no clock or arm target. Requiring the
    /// owning device here also prevents an inconsistent topology from routing
    /// an ioctl to some other card with the same raw CRTC handle.
    pub(in crate::kms::render::backend) fn present_crtc_output(
        &self,
        crtc_id: u32,
    ) -> Option<(usize, CrtcKey)> {
        let output_key = self.pstate.crtc_key_by_id.get(&crtc_id)?;
        let (output_idx, output) = self
            .platform
            .outputs
            .iter()
            .enumerate()
            .find(|(_, output)| output.key == *output_key)?;
        let crtc_key = CrtcKey::for_output(output);
        self.platform.device_for_key(crtc_key.device_key)?;
        Some((output_idx, crtc_key))
    }

    pub(in crate::kms::render::backend) fn present_crtc_key(
        &self,
        crtc_id: u32,
    ) -> Option<CrtcKey> {
        self.present_crtc_output(crtc_id).map(|(_, key)| key)
    }

    pub(in crate::kms::render::backend) fn refresh_present_crtc_clock_epochs(&mut self) {
        let live: Vec<(u32, CrtcKey)> = self
            .pstate
            .crtc_key_by_id
            .keys()
            .filter_map(|&crtc_id| {
                self.present_crtc_key(crtc_id)
                    .map(|crtc_key| (crtc_id, crtc_key))
            })
            .collect();
        let live_ids: HashSet<u32> = live.iter().map(|(crtc_id, _)| *crtc_id).collect();
        self.pstate
            .present_crtc_clock_epochs
            .retain(|crtc_id, _| live_ids.contains(crtc_id));

        for (crtc_id, crtc_key) in live {
            let route_unchanged = self
                .pstate
                .present_crtc_clock_epochs
                .get(&crtc_id)
                .is_some_and(|(old_key, _)| *old_key == crtc_key);
            if route_unchanged {
                continue;
            }
            let epoch = self.pstate.next_present_crtc_clock_epoch;
            self.pstate.next_present_crtc_clock_epoch = self
                .pstate
                .next_present_crtc_clock_epoch
                .checked_add(1)
                .expect("Present CRTC clock epoch overflow");
            self.pstate
                .present_crtc_clock_epochs
                .insert(crtc_id, (crtc_key, epoch));
        }
    }
}

impl KmsBackend {
    pub(in crate::kms::render::backend) fn backend_present_note_present_pixmap(
        &mut self,
        src_pixmap_xid: u32,
        dst_window_xid: u32,
    ) {
        const PRESENT_CAP: usize = 32;

        if let Some(source) = self.store.lookup(src_pixmap_xid) {
            for scope in self.pstate.direct.active_scopes() {
                let fallback_matches = self.pstate.direct.get(scope).is_some_and(|owner| {
                    owner.unflip_requested && owner.unflip_fallback_source == Some(source)
                });
                if !fallback_matches {
                    continue;
                }
                let owner = self
                    .pstate
                    .direct
                    .get_mut(scope)
                    .expect("active direct scope remains present");
                owner.unflip_fallback_source = None;
                owner.unflip_shadow_ready = true;
                log::debug!(
                    "scanout_m2: normal Present Copy prepared composed fallback source=0x{src_pixmap_xid:x}"
                );
            }
        }

        if self.recent_present_pixmaps.back() != Some(&(src_pixmap_xid, dst_window_xid)) {
            if self.recent_present_pixmaps.len() == PRESENT_CAP {
                self.recent_present_pixmaps.pop_front();
            }
            self.recent_present_pixmaps
                .push_back((src_pixmap_xid, dst_window_xid));
        }
    }

    pub(in crate::kms::render::backend) fn backend_present_try_present_direct(
        &mut self,
        candidate: PresentScanoutCandidate,
        event: yserver_core::backend::CompletedPresentEvent,
    ) -> io::Result<bool> {
        let present_received_at = std::time::Instant::now();
        let present_received_monotonic = crate::drm::atomic::monotonic_timestamp();
        let source_id = self.store.lookup(candidate.src_host_xid);
        let leaf_id = self.store.lookup(candidate.paint_dst_host_xid);
        let paint_target = self.resolve_paint_target(candidate.paint_dst_host_xid);
        let paint_id = paint_target.map(|target| target.backing_id());
        let target = self.scanout_m0_target(candidate.paint_dst_host_xid, leaf_id, paint_id);
        let candidate_scope = self.direct_candidate_scope(candidate, target);
        let authoritative_root = self.direct_candidate_authoritative_root(candidate, target);
        // #133 step 3 (3.5): reject any candidate whose resolved paint
        // chain carries a border clip. `has_border_clip()` is true iff
        // some window between the presented drawable and its backing has
        // `border_width > 0`, which is exactly the case where content no
        // longer starts at storage (0, 0). A candidate that does not
        // resolve at all is rejected further down.
        let unbordered = paint_target.is_none_or(|t| !t.has_border_clip());
        let tearing_gate_ignoring_cursor =
            self.tearing_gate_ignoring_cursor(candidate, target, unbordered);
        let was_tearing_on = self.note_tearing_present(
            candidate,
            authoritative_root,
            target,
            tearing_gate_ignoring_cursor,
        );
        self.platform.set_tearing_on(self.pstate.tearing.is_on());
        self.register_tearing_copy_fallback(candidate, authoritative_root, source_id);
        if candidate_scope.is_some_and(|scope| {
            self.pstate
                .direct
                .get(scope)
                .is_some_and(|owner| owner.reentry_blocked_until_composed)
        }) {
            return Ok(false);
        }
        // No transformed CRTC is ever flipped directly (spec D5). During
        // tearing, the scene's cursor classification cannot retire a new HW
        // assignment, but the existing direct gate still decides whether this
        // Present itself may be scanned out.
        let eligible = candidate_scope.is_some_and(|scope| {
            self.direct_candidate_gate(candidate, target, scope, unbordered, true)
        });
        if !eligible {
            // A child/video/game Present updates the COW shadow, but Muffin's
            // currently scanned root-stage buffer remains authoritative until
            // Muffin presents its next root frame. Unflipping for every child
            // Present turns playback into direct/composed thrash. Only an
            // ineligible authoritative successor within the candidate's scope
            // invalidates that owner and must expose its Copy fallback.
            if authoritative_root && !(self.pstate.tearing.is_on() && tearing_gate_ignoring_cursor)
            {
                self.pstate
                    .direct
                    .root_mut()
                    .reset_eligible_direct_probation();
                self.request_direct_unflip_for(
                    DirectScope::Root,
                    "ineligible_authoritative_root_present",
                );
                if self.pstate.direct.root().active() {
                    self.pstate.direct.root_mut().unflip_fallback_source = source_id;
                    self.pstate.direct.root_mut().unflip_shadow_ready = false;
                }
                for scope in self.pstate.direct.active_scopes() {
                    let DirectScope::Output(_) = scope else {
                        continue;
                    };
                    let direct_rect = self.direct_scope_rect(scope);
                    let visible = match target {
                        ScanoutM0Target::Cow | ScanoutM0Target::CowDescendant => !self
                            .direct_shape_chain_hides_root(
                                candidate.paint_dst_host_xid,
                                direct_rect,
                            ),
                        ScanoutM0Target::Unredirected => self.unredirected_direct_scene_eligible(
                            candidate.paint_dst_host_xid,
                            direct_rect,
                        ),
                        ScanoutM0Target::Other => true,
                    };
                    if !visible {
                        continue;
                    }
                    if let Some(owner) = self.pstate.direct.get_mut(scope) {
                        owner.reset_eligible_direct_probation();
                    }
                    self.request_direct_unflip_for(scope, "ineligible_authoritative_root_present");
                    if self
                        .pstate
                        .direct
                        .get(scope)
                        .is_some_and(|owner| owner.active())
                        && let Some(owner) = self.pstate.direct.get_mut(scope)
                    {
                        owner.unflip_fallback_source = source_id;
                        owner.unflip_shadow_ready = false;
                    }
                }
            } else if let Some(scope @ DirectScope::Output(_)) = candidate_scope {
                if let Some(owner) = self.pstate.direct.get_mut(scope) {
                    owner.reset_eligible_direct_probation();
                }
                self.request_direct_unflip_for(scope, "ineligible_authoritative_output_present");
                if self
                    .pstate
                    .direct
                    .get(scope)
                    .is_some_and(|owner| owner.active())
                    && let Some(owner) = self.pstate.direct.get_mut(scope)
                {
                    owner.unflip_fallback_source = source_id;
                    owner.unflip_shadow_ready = false;
                }
            }
            return Ok(false);
        }
        let Some(scope) = candidate_scope else {
            return Ok(false);
        };
        let Some((completion_output_idx, _)) = self.present_crtc_output(candidate.crtc_id) else {
            return Ok(false);
        };
        self.platform
            .note_direct_present_received(completion_output_idx);
        debug_assert_eq!(
            event.crtc_id, candidate.crtc_id,
            "direct candidate and completion must share one CRTC domain"
        );
        debug_assert_eq!(
            event.crtc_epoch, candidate.crtc_epoch,
            "direct candidate and completion must share one CRTC epoch"
        );

        let conflicting_scopes: Vec<_> = match scope {
            DirectScope::Root => (0..self.pstate.direct.outputs.len())
                .map(DirectScope::Output)
                .collect(),
            DirectScope::Output(_) => vec![DirectScope::Root],
        };
        let active_conflicts: Vec<_> = conflicting_scopes
            .into_iter()
            .filter(|&other_scope| {
                self.pstate.direct.get(other_scope).is_some_and(|owner| {
                    owner.active()
                        || owner.unflip_requested
                        || !owner.unflip_outputs_to_replace.is_empty()
                        || !owner.unflip_awaiting_outputs.is_empty()
                        || (scope == DirectScope::Root && owner.reentry_blocked_until_composed)
                })
            })
            .collect();
        if !active_conflicts.is_empty() {
            for other_scope in active_conflicts {
                self.request_direct_unflip_for(other_scope, "direct_scope_kind_changed");
            }
            if let Some(owner) = self.pstate.direct.get_mut(scope) {
                owner.reset_eligible_direct_probation();
            }
            return Ok(false);
        }

        if !self
            .pstate
            .direct
            .get_mut(scope)
            .is_some_and(|owner| owner.admit_eligible_direct())
        {
            return Ok(false);
        }

        // Finish a previously-requested composed replacement (cursor seam,
        // overlay, or failed direct successor) before allowing direct re-entry.
        // Otherwise a fast Present stream can repeatedly replace a partial
        // dual-head unflip and starve the CRTC that did not submit yet.
        if self
            .pstate
            .direct
            .get(scope)
            .is_some_and(|owner| owner.unflip_requested)
        {
            return Ok(false);
        }

        let Some(source_id) = source_id else {
            self.request_direct_unflip_for(scope, "eligible_direct_successor_source_missing");
            return Ok(false);
        };
        let Some(fallback_target) = paint_target else {
            self.request_direct_unflip_for(scope, "eligible_direct_successor_paint_target_missing");
            return Ok(false);
        };
        if self
            .pstate
            .direct
            .get(scope)
            .is_some_and(|owner| owner.active())
        {
            // Set only on an actual fallback below. An eligible successor
            // queued behind a direct flip must leave direct ownership intact.
            if let Some(owner) = self.pstate.direct.get_mut(scope) {
                owner.unflip_fallback_source = None;
            }
        }
        let framebuffer_ready = self.direct_source_ready(source_id, leaf_id, scope);
        if !framebuffer_ready {
            self.request_direct_unflip_for(scope, "eligible_direct_successor_framebuffer_missing");
            return Ok(false);
        }
        // Fence a promoted source's writes before this Present is submitted or queued (D8).
        let (in_fence, fence_tally) = if self.pstate.server_scanout.is_promoted(source_id) {
            match self.server_direct_source_fence(source_id, scope) {
                Ok((fence, tally)) => (fence, Some(tally)),
                Err(error) => {
                    log::warn!(
                        target: "yserver::scanout_server",
                        "direct Present of server-owned source_id={} takes Copy: {error}",
                        source_id.as_u64(),
                    );
                    self.request_direct_unflip_for(scope, "server_source_fence_failed");
                    if let Some(owner) = self.pstate.direct.get_mut(scope)
                        && owner.active()
                    {
                        owner.unflip_fallback_source = Some(source_id);
                        owner.unflip_shadow_ready = false;
                    }
                    return Ok(false);
                }
            }
        } else {
            (None, None)
        };

        let source_pin = self.pin_direct_source(source_id);
        let fallback_target_pin = self.pin_direct_source(fallback_target.backing_id());
        let present_id = candidate.present_id;
        let mut frame = DirectPresentFrame {
            source_pin,
            fallback_target_pin,
            source_id,
            candidate,
            target,
            present_received_at,
            present_received_monotonic,
            fallback_target,
            event,
            completion_output_idx,
            completion_clock: None,
            awaiting_outputs: HashSet::new(),
            accepted_event_id: None,
            async_outcome: crate::drm::async_flip::AsyncOutcome::NotRequested,
            in_fence,
            fence_tally,
        };

        if self
            .pstate
            .direct
            .get(scope)
            .is_some_and(|owner| owner.pending.is_some())
        {
            self.retain_direct_present_wake(&frame.event);
            self.queue_direct_successor(scope, frame);
            return Ok(true);
        }
        let direct_outputs = self.direct_scope_outputs(scope);
        if direct_outputs
            .iter()
            .any(|&output_idx| self.scene.has_pending_page_flip(output_idx))
        {
            self.request_direct_unflip_for(scope, "eligible_direct_successor_scene_flip_pending");
            if let Some(owner) = self.pstate.direct.get_mut(scope) {
                owner.unflip_fallback_source = Some(source_id);
                owner.unflip_shadow_ready = false;
            }
            <Self as Backend>::release_present_source(self, source_pin);
            <Self as Backend>::release_present_source(self, fallback_target_pin);
            return Ok(false);
        }
        if self
            .platform
            .scanout_commit_in_flight_for_outputs(&direct_outputs)
        {
            if self
                .pstate
                .direct
                .get(scope)
                .is_some_and(|owner| owner.current.is_some())
            {
                // A fully-retired direct frame owns the current slot. Keep
                // this Present in the existing latest-wins successor slot;
                // the scheduling pass submits it after the last cursor-only
                // commit on the grouped transaction retires.
                self.retain_direct_present_wake(&frame.event);
                self.queue_direct_successor(scope, frame);
                return Ok(true);
            }

            // With no direct owner, this is the first-entry equivalent of a
            // scene flip already in flight: let the caller take its composed
            // path and release both pins. There must be no queued-only direct
            // state for the cursor retirement to discover later.
            self.request_direct_unflip_for(scope, "eligible_direct_present_cursor_commit_pending");
            if let Some(owner) = self.pstate.direct.get_mut(scope) {
                owner.unflip_fallback_source = Some(source_id);
                owner.unflip_shadow_ready = false;
            }
            <Self as Backend>::release_present_source(self, source_pin);
            <Self as Backend>::release_present_source(self, fallback_target_pin);
            return Ok(false);
        }
        let timing = if was_tearing_on && authoritative_root {
            self.direct_frame_flip_timing(&frame, scope)
        } else {
            crate::drm::async_flip::FlipTiming::Vsync
        };
        if let Err(error) = self.submit_direct_frame(&mut frame, scope, timing) {
            self.request_direct_unflip_for(scope, "eligible_direct_successor_submit_failed");
            <Self as Backend>::release_present_source(self, source_pin);
            <Self as Backend>::release_present_source(self, fallback_target_pin);
            if let Some(owner) = self.pstate.direct.get_mut(scope) {
                owner.reset_eligible_direct_probation();
            }
            return Err(error);
        }

        self.retain_direct_present_wake(&frame.event);
        let owner = self
            .pstate
            .direct
            .get_mut(scope)
            .expect("candidate direct scope has an owner slot");
        owner.pending = Some(frame);
        owner.hold_direct = true;
        owner.unflip_requested = false;
        owner.unflip_reason = None;
        owner.unflip_last_reason = None;
        owner.unflip_fallback_source = None;
        owner.unflip_shadow_ready = false;
        log::debug!(
            "scanout_m2: live direct submit source_id={} present_id={} outputs={}",
            source_id.as_u64(),
            present_id,
            direct_outputs.len()
        );
        Ok(true)
    }

    pub(in crate::kms::render::backend) fn backend_present_arm_present_source_wait(
        &mut self,
        src_pixmap_host_xid: u32,
        dst_window_host_xid: u32,
    ) -> io::Result<PresentSourceWait> {
        use std::os::fd::AsFd;

        use crate::kms::{
            render::present_source_wait::{PendingPresentSourceWait, PendingWaitFd},
            vk::dri3::{
                ExportedSyncFile, export_dmabuf_read_access_sync_file,
                export_dmabuf_write_access_sync_file,
            },
        };

        let Some(src_id) = self.store.lookup(src_pixmap_host_xid) else {
            return Ok(PresentSourceWait::Ready);
        };
        let mut fds = Vec::new();
        if let Some(fd) = self
            .store
            .get(src_id)
            .and_then(|d| d.storage.imported_drawable.as_ref())
            .and_then(crate::kms::vk::target::DrawableImage::imported_dma_buf_fd)
        {
            match export_dmabuf_read_access_sync_file(fd) {
                ExportedSyncFile::Idle => {}
                ExportedSyncFile::Unsupported => {
                    if self.pstate.dmabuf_sync_file_warned.replace(true) {
                        log::debug!(
                            target: "yserver::kms::render::present",
                            "present source 0x{src_pixmap_host_xid:x}: dma-buf sync-file export unsupported; copying immediately",
                        );
                    } else {
                        log::warn!(
                            target: "yserver::kms::render::present",
                            "dma-buf sync-file export unsupported on this kernel; Present \
                             sources and destinations will copy immediately for the rest of \
                             this session (first seen at present source 0x{src_pixmap_host_xid:x}). Logged once; \
                             further occurrences at debug.",
                        );
                    }
                }
                ExportedSyncFile::Fd(fd) => fds.push(PendingWaitFd {
                    fd,
                    registered: false,
                    ready: false,
                }),
            }
        }

        let destination_id = self
            .resolve_paint_target(dst_window_host_xid)
            .map(|t| t.backing_id());
        let mut prewaited_destination = None;
        if let Some(dst_id) = destination_id
            && let Some(fd) = self.store.exported_sync_fd(dst_id)
        {
            match export_dmabuf_write_access_sync_file(fd.as_fd()) {
                ExportedSyncFile::Idle => {}
                ExportedSyncFile::Unsupported => {
                    if self.pstate.dmabuf_sync_file_warned.replace(true) {
                        log::debug!(
                            target: "yserver::kms::render::present",
                            "present destination 0x{dst_window_host_xid:x}: dma-buf sync-file export unsupported; copying immediately",
                        );
                    } else {
                        log::warn!(
                            target: "yserver::kms::render::present",
                            "dma-buf sync-file export unsupported on this kernel; Present \
                             sources and destinations will copy immediately for the rest of \
                             this session (first seen at present destination \
                             0x{dst_window_host_xid:x}). Logged once; further occurrences at debug.",
                        );
                    }
                }
                ExportedSyncFile::Fd(fd) => {
                    fds.push(PendingWaitFd {
                        fd,
                        registered: false,
                        ready: false,
                    });
                    prewaited_destination = Some(dst_id);
                }
            }
        }

        let mut pending = PendingPresentSourceWait {
            fds,
            source_id: src_id,
            prewaited_destination,
            syncobj_pin: None,
            timeline_value: None,
            poll_timeline: false,
            ready_reported: false,
        };
        if pending.is_ready() {
            return Ok(PresentSourceWait::Ready);
        }

        let wait_id = self.pstate.next_present_source_wait_id;
        self.pstate.next_present_source_wait_id = self
            .pstate
            .next_present_source_wait_id
            .wrapping_add(1)
            .max(1);
        self.store.incref(src_id);
        for wait_fd in &mut pending.fds {
            match self
                .platform
                .present_completion_epfd
                .register(wait_fd.fd.as_fd(), wait_id)
            {
                Ok(()) => wait_fd.registered = true,
                Err(e) => log::warn!(
                    target: "yserver::kms::render::present",
                    "present 0x{src_pixmap_host_xid:x}: readiness registration failed: {e}; polling",
                ),
            }
        }
        self.pstate
            .pending_present_source_waits
            .insert(wait_id, pending);
        Ok(PresentSourceWait::Deferred(wait_id))
    }

    pub(in crate::kms::render::backend) fn backend_present_arm_present_syncobj_wait(
        &mut self,
        src_pixmap_host_xid: u32,
        dst_window_host_xid: u32,
        acquire_syncobj: u32,
        acquire_value: u64,
    ) -> io::Result<PresentSourceWait> {
        use std::os::fd::AsFd;

        use crate::kms::{
            render::present_source_wait::{PendingPresentSourceWait, PendingWaitFd},
            vk::dri3::{ExportedSyncFile, export_dmabuf_write_access_sync_file},
        };

        let Some(src_id) = self.store.lookup(src_pixmap_host_xid) else {
            return Ok(PresentSourceWait::Ready);
        };
        let (syncobj, event_fd) = if acquire_syncobj == 0 {
            (None, None)
        } else {
            let syncobj = self
                .pstate
                .dri3_syncobjs
                .get(&acquire_syncobj)
                .map(|(_, arc)| arc.clone())
                .ok_or_else(|| {
                    io::Error::other(format!(
                        "PresentPixmapSynced: unknown acquire syncobj 0x{acquire_syncobj:x}"
                    ))
                })?;
            // Skip the ioctl entirely once it has proven unavailable: this
            // runs per Present, and on a kernel without the eventfd
            // interface every call fails identically.
            // Probe the ioctl once, with arguments we control, rather than
            // classifying a per-Present failure by errno: FreeBSD returns
            // EINVAL here, which is indistinguishable from a bad argument on a
            // kernel that does support it.
            let supported = match self.pstate.syncobj_eventfd_supported {
                Some(v) => v,
                None => {
                    let v = self
                        .platform
                        .selected_render_device()
                        .and_then(|device| device.render_node_device.as_ref())
                        .is_some_and(crate::kms::render::imported_syncobj::eventfd_supported);
                    self.pstate.syncobj_eventfd_supported = Some(v);
                    if !v {
                        log::warn!(
                            target: "yserver::kms::render::present",
                            "DRM syncobj eventfd unsupported on this kernel; EVERY \
                             PresentPixmapSynced acquire will use the timeline poll for \
                             the rest of this session. Logged once.",
                        );
                    }
                    v
                }
            };
            let event_fd = if supported {
                match syncobj.signaled_eventfd(acquire_value) {
                    Ok(fd) => Some(fd),
                    Err(e) => {
                        // The probe said the ioctl works, so this is a real
                        // per-call failure and worth reporting every time --
                        // it should not recur.
                        log::warn!(
                            target: "yserver::kms::render::present",
                            "PresentPixmapSynced DRM eventfd registration failed ({e}); \
                             polling this acquire",
                        );
                        None
                    }
                }
            } else {
                None
            };
            (Some(syncobj), event_fd)
        };
        let poll_timeline = syncobj.is_some() && event_fd.is_none();
        let mut fds: Vec<PendingWaitFd> = event_fd
            .into_iter()
            .map(|fd| PendingWaitFd {
                fd,
                registered: false,
                ready: false,
            })
            .collect();
        let destination_id = self
            .resolve_paint_target(dst_window_host_xid)
            .map(|t| t.backing_id());
        let mut prewaited_destination = None;
        if let Some(dst_id) = destination_id
            && let Some(fd) = self.store.exported_sync_fd(dst_id)
        {
            match export_dmabuf_write_access_sync_file(fd.as_fd()) {
                ExportedSyncFile::Idle => {}
                ExportedSyncFile::Unsupported => {
                    if self.pstate.dmabuf_sync_file_warned.replace(true) {
                        log::debug!(
                            target: "yserver::kms::render::present",
                            "PresentPixmapSynced destination 0x{dst_window_host_xid:x}: dma-buf sync-file export unsupported; copying immediately",
                        );
                    } else {
                        log::warn!(
                            target: "yserver::kms::render::present",
                            "dma-buf sync-file export unsupported on this kernel; Present \
                             sources and destinations will copy immediately for the rest of \
                             this session (first seen at PresentPixmapSynced destination \
                             0x{dst_window_host_xid:x}). Logged once; further occurrences at debug.",
                        );
                    }
                }
                ExportedSyncFile::Fd(fd) => {
                    fds.push(PendingWaitFd {
                        fd,
                        registered: false,
                        ready: false,
                    });
                    prewaited_destination = Some(dst_id);
                }
            }
        }
        let mut pending = PendingPresentSourceWait {
            fds,
            source_id: src_id,
            prewaited_destination,
            syncobj_pin: syncobj,
            timeline_value: (acquire_syncobj != 0).then_some(acquire_value),
            poll_timeline,
            ready_reported: false,
        };
        if pending.is_ready() {
            log::debug!(
                target: "present_pace",
                "present acquire already signaled syncobj=0x{acquire_syncobj:x} value={acquire_value}"
            );
            return Ok(PresentSourceWait::Ready);
        }

        let wait_id = self.pstate.next_present_source_wait_id;
        self.pstate.next_present_source_wait_id = self
            .pstate
            .next_present_source_wait_id
            .wrapping_add(1)
            .max(1);
        self.store.incref(src_id);
        for wait_fd in &mut pending.fds {
            match self
                .platform
                .present_completion_epfd
                .register(wait_fd.fd.as_fd(), wait_id)
            {
                Ok(()) => wait_fd.registered = true,
                Err(e) => log::warn!(
                    target: "yserver::kms::render::present",
                    "PresentPixmapSynced acquire eventfd registration failed: {e}; polling",
                ),
            }
        }
        self.pstate
            .pending_present_source_waits
            .insert(wait_id, pending);
        Ok(PresentSourceWait::Deferred(wait_id))
    }

    pub(in crate::kms::render::backend) fn backend_present_drain_ready_present_source_waits(
        &mut self,
    ) -> Vec<u64> {
        use std::os::fd::AsFd;

        let mut ready = Vec::new();
        for (&wait_id, wait) in &mut self.pstate.pending_present_source_waits {
            if wait.ready_reported {
                continue;
            }
            for wait_fd in &mut wait.fds {
                if wait_fd.refresh_ready()
                    && wait_fd.registered
                    && let Err(e) = self
                        .platform
                        .present_completion_epfd
                        .unregister(wait_fd.fd.as_fd())
                {
                    log::warn!("deferred Present source: readiness unregister failed: {e}");
                }
                if wait_fd.ready {
                    wait_fd.registered = false;
                }
            }
            if !wait.is_ready() {
                continue;
            }
            wait.ready_reported = true;
            ready.push(wait_id);
        }
        ready
    }

    pub(in crate::kms::render::backend) fn backend_present_begin_ready_present_destination_write(
        &mut self,
        wait_id: u64,
    ) {
        if let Some(id) = self
            .pstate
            .pending_present_source_waits
            .get(&wait_id)
            .and_then(|wait| wait.prewaited_destination)
        {
            self.store.begin_prewaited_exported_write(id);
        }
    }

    pub(in crate::kms::render::backend) fn backend_present_finish_present_source_wait(
        &mut self,
        wait_id: u64,
    ) {
        use std::os::fd::AsFd;

        let Some(wait) = self.pstate.pending_present_source_waits.remove(&wait_id) else {
            return;
        };
        for wait_fd in &wait.fds {
            if wait_fd.registered
                && let Err(e) = self
                    .platform
                    .present_completion_epfd
                    .unregister(wait_fd.fd.as_fd())
            {
                log::warn!("deferred Present source: readiness unregister failed: {e}");
            }
        }
        if let Some(id) = wait.prewaited_destination {
            self.store.end_prewaited_exported_write(id);
        }
        self.store_decref_with_invalidate(wait.source_id);
    }

    pub(in crate::kms::render::backend) fn backend_present_present_absolute_vblank_arm_supported(
        &self,
        crtc_id: u32,
    ) -> bool {
        self.present_crtc_key(crtc_id)
            .is_some_and(|key| self.kernel_vblank_arm_available(key.device_key))
    }

    pub(in crate::kms::render::backend) fn backend_present_arm_present_absolute_vblank(
        &mut self,
        randr_crtc_id: u32,
        targets: &[u64],
    ) -> io::Result<usize> {
        let Some(crtc_key) = self.present_crtc_key(randr_crtc_id) else {
            return Ok(0);
        };
        let Some(mut kernel) = self.kernel_vblank_armer(crtc_key) else {
            return Ok(0);
        };
        let result =
            self.arm_present_absolute_vblank_with(crtc_key, targets, |crtc_key, target| {
                let crtc_id = u32::from(crtc_key.crtc);
                kernel.arm(
                    crtc_key,
                    false,
                    target,
                    absolute_seq_user_data(crtc_id),
                    crtc_id | crate::drm::page_flip::LEGACY_VBLANK_ABSOLUTE_SIGNAL,
                )
            });
        self.finish_kernel_vblank_armer(crtc_key, kernel);
        result
    }

    /// Stage 5 Task 6.1 — drain batches whose completion semaphore has
    /// signalled (or all batches when `platform.renderer_failed`).
    /// Wake signals fire via the Arc-pinned handle inside the impl
    /// body before the events are returned to the caller.
    pub(in crate::kms::render::backend) fn backend_present_drain_completed_present_events(
        &mut self,
    ) -> Vec<yserver_core::backend::CompletedPresentEvent> {
        let mut completed = self.drain_completed_present_events_impl();
        completed.append(&mut self.pstate.direct.root_mut().completed);
        for owner in &mut self.pstate.direct.outputs {
            completed.append(&mut owner.completed);
        }
        for event in &completed {
            if event.completion_mode != yserver_protocol::x11::present::COMPLETE_MODE_SKIP {
                continue;
            }
            let Some((output_idx, _)) = self.present_crtc_output(event.crtc_id) else {
                continue;
            };
            let Some(output) = self.platform.outputs.get(output_idx) else {
                continue;
            };
            if let Some(device) = self.platform.device_for_output(&output.key) {
                device.atomic_commit_sink.note_skip_completion();
            }
        }
        completed
    }

    pub(in crate::kms::render::backend) fn backend_present_present_crtc_clock_epoch(
        &self,
        crtc_id: u32,
    ) -> u64 {
        let Some(crtc_key) = self.present_crtc_key(crtc_id) else {
            return 0;
        };
        self.pstate
            .present_crtc_clock_epochs
            .get(&crtc_id)
            .filter(|(epoch_key, _)| *epoch_key == crtc_key)
            .map_or(0, |(_, epoch)| *epoch)
    }

    pub(in crate::kms::render::backend) fn backend_present_present_get_ust_msc(
        &self,
        crtc_id: u32,
    ) -> (u64, u64) {
        self.present_crtc_key(crtc_id).map_or((0, 0), |crtc_key| {
            self.platform.present_get_ust_msc(crtc_key)
        })
    }

    pub(in crate::kms::render::backend) fn backend_present_present_get_completion_clock(
        &self,
        crtc_id: u32,
    ) -> yserver_core::backend::PresentClockSample {
        self.present_crtc_key(crtc_id).map_or(
            yserver_core::backend::PresentClockSample {
                msc: 0,
                ust: 0,
                source: yserver_core::backend::PresentClockSource::PageFlip,
            },
            |crtc_key| self.platform.present_get_completion_clock(crtc_key),
        )
    }

    pub(in crate::kms::render::backend) fn backend_present_arm_present_completion_idle_vblanks(
        &mut self,
        crtc_id: u32,
        target_mscs: &[u64],
    ) -> std::io::Result<usize> {
        let Some(crtc_key) = self.present_crtc_key(crtc_id) else {
            return Ok(0);
        };
        if !self.present_completion_is_idle_for(crtc_key) {
            return Ok(0);
        }
        self.arm_idle_vblanks_ioctl(crtc_id, target_mscs)
    }

    pub(in crate::kms::render::backend) fn backend_present_present_capabilities(
        &self,
        _window: u32,
        crtc_id: u32,
    ) -> PresentCaps {
        let async_flip = self
            .present_crtc_output(crtc_id)
            .and_then(|(_, crtc_key)| {
                self.platform
                    .device_for_key(crtc_key.device_key)
                    .map(|device| device.async_page_flip)
            })
            .unwrap_or(false);
        // Mirror v1's conservative "Copy-path only" caps. syncobj
        // tracks Dri3Caps::syncobj. flip_path / async_may_tear stay
        // false until alien-BO scanout integration lands on v2.
        PresentCaps {
            flip_path: false,
            async_flip,
            async_may_tear: false,
            syncobj: self.dri3_capabilities().syncobj,
        }
    }
}
