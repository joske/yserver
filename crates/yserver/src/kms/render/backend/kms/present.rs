use super::*;

/// `user_data` value for an absolute per-target arm on `crtc_id` — the
/// single producer of the encoding `on_crtc_sequence_event` decodes, so
/// the two can never drift independently of each other. Free function
/// (not inlined into the ioctl closure) so a unit test can round-trip
/// it straight into `on_crtc_sequence_event` without a live DRM fd.
pub(in crate::kms::render::backend) fn absolute_seq_user_data(crtc_id: u32) -> u64 {
    ABSOLUTE_SEQ_TAG | u64::from(crtc_id)
}

impl KmsBackend {
    /// Stage 5 Task 6.1: pick up any PRESENT completions that were
    /// queued past `disable_output` so the caller (lib.rs::run) can
    /// fan them out to clients before tearing down the socket.
    pub fn take_shutdown_present_events(
        &mut self,
    ) -> Vec<yserver_core::backend::CompletedPresentEvent> {
        std::mem::take(&mut self.pending_completed_events_on_shutdown)
    }

    /// Shutdown-only: walk every drawable still in `store` and
    /// destroy its Vk handles (`VkImage` / `VkImageView` /
    /// `VkDeviceMemory`). `DrawableStore` has no `Drop` impl
    /// because `Storage::destroy` needs `&PlatformBackend` for
    /// pool-return + DRI3-import handling — both fields are
    /// owned by `KmsBackend`, so this method bridges them.
    /// Without this, MATE's resident pixmaps at SIGTERM leak
    /// (948 VkDeviceMemory observed on bee/MATE 2026-05-31
    /// post-FenceTicket fix). Caller is `lib.rs`'s explicit
    /// shutdown block, after PRESENT completions are drained
    /// and before `backend` drops.
    pub fn shutdown_destroy_drawables(&mut self) {
        self.store.shutdown_destroy_all(&self.platform);
    }

    /// Signal (and release) every wake pin still retained after the last
    /// drain. `fire_pending_present_entry` now retains instead of signalling,
    /// so at teardown these must be flushed to release the buffers a client is
    /// blocked on. Called in the shutdown sequence before
    /// `shutdown_destroy_drawables`.
    pub fn signal_all_retained_present_wakes(&mut self) {
        let ids: Vec<u64> = self.retained_present_wakes.keys().copied().collect();
        for id in ids {
            self.signal_present_wake(id);
        }
    }

    fn fire_pending_present_entry(
        &mut self,
        entry: crate::kms::render::present_completion::PendingPresentEntry,
    ) -> yserver_core::backend::CompletedPresentEvent {
        // Do NOT signal here. Retain the pin keyed by present_id so core can
        // release it at the target vblank (frame pacing). PinnedWake::None
        // entries need no retention.
        // Destructure so `wake_pin` is moved exactly once (PinnedWake is not
        // Copy — a `matches!(entry.wake_pin, ..)` guard would move it first and
        // fail to compile).
        use crate::kms::render::present_completion::{PendingPresentEntry, PinnedWake};
        let PendingPresentEntry { wake_pin, event } = entry;
        if !matches!(wake_pin, PinnedWake::None) {
            self.retained_present_wakes
                .insert(event.present_id, wake_pin);
        }
        event
    }

    fn poke_present_completion_wakeup(&self, context: &str) {
        match self.platform.wakeup_eventfd.write(1) {
            Ok(_) | Err(nix::errno::Errno::EAGAIN) => {}
            Err(e) => log::warn!("{context}: wakeup_eventfd write: {e}"),
        }
    }

    pub(in crate::kms::render::backend) fn register_pending_present_batch(
        &mut self,
        mut batch: crate::kms::render::present_completion::PendingPresentBatch,
    ) {
        use crate::kms::render::present_completion::PresentBatchWait;

        if batch.events.is_empty() {
            return;
        }

        let should_wake = match &batch.wait {
            PresentBatchWait::Fd(fd) => {
                use std::os::fd::{AsFd, AsRawFd};
                // Token mirrors the fd's raw value (matches the native
                // epoll-data / kqueue-udata the inline code used).
                let token = u64::try_from(fd.as_raw_fd()).unwrap_or_default();
                let add_result = self
                    .platform
                    .present_completion_epfd
                    .register(fd.as_fd(), token);
                match add_result {
                    Ok(()) => false,
                    Err(e) => {
                        log::warn!(
                            "deferred PRESENT: poll ADD sync_file fd failed: {e}; \
                             using immediate drain fallback"
                        );
                        batch.wait = PresentBatchWait::Ready;
                        true
                    }
                }
            }
            PresentBatchWait::Ready | PresentBatchWait::Poll => true,
        };

        self.pending_present_batches.push_back(batch);
        if should_wake {
            self.poke_present_completion_wakeup("register_pending_present_batch");
        }
    }

    pub(in crate::kms::render::backend) fn drain_engine_present_batches(&mut self) {
        for batch in self.engine.drain_present_batches() {
            self.register_pending_present_batch(batch);
        }
    }

    fn pending_present_batch_ready(
        batch: &crate::kms::render::present_completion::PendingPresentBatch,
        vk: Option<&std::sync::Arc<crate::kms::vk::device::VkContext>>,
    ) -> bool {
        use std::os::fd::AsFd;

        use crate::kms::render::present_completion::PresentBatchWait;
        use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

        // B.2-context fix (vkdebug VUID-vkDestroySemaphore-semaphore-05149):
        // sync_file readiness alone is NOT sufficient to drop the batch
        // (which destroys `batch.signal` — the export semaphore — via
        // PresentCompletionSignal::Drop). Vulkan requires the queue
        // submit fence to signal before any of its semaphores are
        // destroyed; the sync_file becomes readable as soon as the
        // KMS pageflip retires, which can be before the GPU compose
        // CB's fence signals. Gate every wait variant additionally on
        // the ticket having signaled when one is present.
        let sync_ready = match &batch.wait {
            PresentBatchWait::Ready => true,
            PresentBatchWait::Fd(fd) => {
                let mut fds = [PollFd::new(fd.as_fd(), PollFlags::POLLIN)];
                match poll(&mut fds, PollTimeout::ZERO) {
                    Ok(0) => false,
                    Ok(_) => fds[0]
                        .revents()
                        .is_some_and(|r| r.intersects(PollFlags::POLLIN | PollFlags::POLLERR)),
                    Err(e) => {
                        log::warn!("deferred PRESENT: poll(sync_file fd): {e}");
                        true
                    }
                }
            }
            PresentBatchWait::Poll => true,
        };
        if !sync_ready {
            return false;
        }
        // Even if the sync_file signaled / Ready variant, the
        // Vulkan-side fence must have signaled too before we let the
        // batch (and its export semaphore) drop. Skip the ticket
        // check only when no ticket was attached (degraded path).
        match (&batch.ticket, vk) {
            (Some(ticket), Some(v)) => ticket.poll_signaled(v),
            (Some(_), None) => false,
            (None, _) => true,
        }
    }

    fn unregister_present_batch_fd(
        &mut self,
        batch: &crate::kms::render::present_completion::PendingPresentBatch,
    ) {
        use crate::kms::render::present_completion::PresentBatchWait;

        let _keep_export_semaphore_alive_until_batch_drop = batch.signal.as_ref();
        if let PresentBatchWait::Fd(fd) = &batch.wait {
            use std::os::fd::AsFd;
            if let Err(e) = self.platform.present_completion_epfd.unregister(fd.as_fd()) {
                log::warn!("deferred PRESENT: poll DEL sync_file fd failed: {e}");
            }
        }
    }

    pub(in crate::kms::render::backend) fn force_drain_all_present_batches(
        &mut self,
    ) -> Vec<yserver_core::backend::CompletedPresentEvent> {
        let mut completed = Vec::new();
        while let Some(batch) = self.pending_present_batches.pop_front() {
            self.unregister_present_batch_fd(&batch);
            for entry in batch.events {
                completed.push(self.fire_pending_present_entry(entry));
            }
        }
        completed
    }

    /// Stage 5 Task 6.1 — algorithm body for `Backend::
    /// drain_completed_present_events`. Walks the front of
    /// `pending_present_batches`, popping whole batches whose exported
    /// sync_file is readable, whose ready sentinel fired, or whose
    /// degraded polling ticket has signaled. Fires wake signals via the
    /// Arc-pinned handles and returns the public event payloads.
    pub(in crate::kms::render::backend) fn drain_completed_present_events_impl(
        &mut self,
    ) -> Vec<yserver_core::backend::CompletedPresentEvent> {
        self.drain_engine_present_batches();
        // Drain the wakeup_eventfd to clear it. nix's EventFd::read
        // returns Ok(u64) with the accumulated counter; Err(EAGAIN)
        // when zero — benign in either case because we re-check batch
        // readiness directly.
        match self.platform.wakeup_eventfd.read() {
            Ok(_) | Err(nix::errno::Errno::EAGAIN) => {}
            Err(e) => log::warn!("wakeup_eventfd read: {e}"),
        }

        let renderer_failed = self.platform.renderer_failed;
        if renderer_failed && !self.pending_present_batches.is_empty() {
            let pending_events: usize = self
                .pending_present_batches
                .iter()
                .map(|batch| batch.events.len())
                .sum();
            log::warn!("renderer_failed: force-firing {pending_events} pending PRESENT entries");
        }

        // Clone the Arc<VkContext> so the loop body can borrow `self`
        // mutably (for `dri3_*_via_handle`) without a conflict with
        // `self.platform.vk`.
        let vk = self.platform.vk.as_ref().cloned();

        let mut completed = Vec::new();
        while let Some(front) = self.pending_present_batches.front() {
            let ready = renderer_failed || Self::pending_present_batch_ready(front, vk.as_ref());
            if !ready {
                break;
            }
            let entry = self
                .pending_present_batches
                .pop_front()
                .expect("just peeked");
            self.unregister_present_batch_fd(&entry);
            for event in entry.events {
                completed.push(self.fire_pending_present_entry(event));
            }
        }
        completed
    }

    pub(in crate::kms::render::backend) fn present_flip_in_flight_for_output(
        &self,
        output_idx: usize,
    ) -> bool {
        phase_b_flip_in_flight_for_scheduler(
            self.scene.has_pending_page_flip(output_idx),
            self.scanout_m2
                .pending
                .as_ref()
                .is_some_and(|frame| frame.awaiting_outputs.contains(&output_idx)),
            self.scanout_m2
                .unflip_awaiting_outputs
                .contains(&output_idx),
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
        if !self.armed_vblank_targets.is_empty() {
            self.armed_vblank_targets.clear();
        }
        if !self.absolute_vblank_targets.is_empty() {
            self.absolute_vblank_targets.clear();
        }
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
        self.armed_vblank_targets
            .retain(|key, _| live.contains(key));
        self.absolute_vblank_targets
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
                && let Some(targets) = self.absolute_vblank_targets.get_mut(&key)
            {
                targets.retain(|&target| {
                    yserver_core::present_scheduler::msc_is_after(target, sequence)
                });
                if targets.is_empty() {
                    self.absolute_vblank_targets.remove(&key);
                }
            }
        } else if let Some(key) = crtc_key {
            self.armed_vblank_targets.remove(&key);
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
        if self
            .crtc_queue_sequence_unsupported_devices
            .contains(&crtc_key.device_key)
            || self.armed_vblank_targets.contains_key(&crtc_key)
        {
            return Ok(0);
        }
        if armer(crtc_key)? {
            self.armed_vblank_targets.insert(crtc_key, 0);
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
        if self
            .crtc_queue_sequence_unsupported_devices
            .contains(&crtc_key.device_key)
        {
            return Ok(0);
        }
        let mut covered = 0;
        for &target in targets {
            let already_armed = self
                .absolute_vblank_targets
                .get(&crtc_key)
                .is_some_and(|set| set.contains(&target));
            if already_armed {
                covered += 1;
                continue;
            }
            if armer(crtc_key, target)? {
                self.absolute_vblank_targets
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
        let Some(device) = self
            .platform
            .device_for_key(crtc_key.device_key)
            .map(|device| Rc::clone(&device.device))
        else {
            return Ok(0);
        };
        let mut newly_unsupported = false;
        let result = self.arm_idle_vblanks_with(crtc_key, target_mscs, |crtc_key| {
            let crtc_id = u32::from(crtc_key.crtc);
            match crate::drm::page_flip::queue_crtc_sequence(
                &device,
                crtc_id,
                /* relative */ true,
                /* sequence */ 1,
                /* user_data */ u64::from(crtc_id),
            ) {
                Ok(_) => Ok(true),
                Err(e)
                    if e.raw_os_error() == Some(libc::EOPNOTSUPP)
                        || e.raw_os_error() == Some(libc::ENOTTY) =>
                {
                    newly_unsupported = true;
                    Ok(false)
                }
                Err(e) => Err(e),
            }
        });
        if newly_unsupported {
            log::warn!(
                "DRM_IOCTL_CRTC_QUEUE_SEQUENCE returned EOPNOTSUPP on {} — disabling \
                 vblank arming on that DRM device (flip-driven MSC only)",
                crtc_key.device_key
            );
            self.crtc_queue_sequence_unsupported_devices
                .insert(crtc_key.device_key);
        }
        result
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
        let output_key = self.crtc_key_by_id.get(&crtc_id)?;
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
            .crtc_key_by_id
            .keys()
            .filter_map(|&crtc_id| {
                self.present_crtc_key(crtc_id)
                    .map(|crtc_key| (crtc_id, crtc_key))
            })
            .collect();
        let live_ids: HashSet<u32> = live.iter().map(|(crtc_id, _)| *crtc_id).collect();
        self.present_crtc_clock_epochs
            .retain(|crtc_id, _| live_ids.contains(crtc_id));

        for (crtc_id, crtc_key) in live {
            let route_unchanged = self
                .present_crtc_clock_epochs
                .get(&crtc_id)
                .is_some_and(|(old_key, _)| *old_key == crtc_key);
            if route_unchanged {
                continue;
            }
            let epoch = self.next_present_crtc_clock_epoch;
            self.next_present_crtc_clock_epoch = self
                .next_present_crtc_clock_epoch
                .checked_add(1)
                .expect("Present CRTC clock epoch overflow");
            self.present_crtc_clock_epochs
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

        if self.scanout_m2.unflip_requested
            && self
                .store
                .lookup(src_pixmap_xid)
                .is_some_and(|source| Some(source) == self.scanout_m2.unflip_fallback_source)
        {
            self.scanout_m2.unflip_fallback_source = None;
            self.scanout_m2.unflip_shadow_ready = true;
            log::debug!(
                "scanout_m2: normal Present Copy prepared composed fallback source=0x{src_pixmap_xid:x}"
            );
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
        if self.scanout_m2.reentry_blocked_until_composed {
            return Ok(false);
        }
        let source_id = self.store.lookup(candidate.src_host_xid);
        let leaf_id = self.store.lookup(candidate.paint_dst_host_xid);
        let paint_target = self.resolve_paint_target(candidate.paint_dst_host_xid);
        let paint_id = paint_target.map(|target| target.backing_id());
        let target = self.scanout_m0_target(candidate.paint_dst_host_xid, leaf_id, paint_id);
        let root = (u32::from(self.platform.fb_w), u32::from(self.platform.fb_h));
        let root_coverage = leaf_id
            .and_then(|id| self.window_absolute_rect(id))
            .is_some_and(|rect| {
                rect.offset.x == 0
                    && rect.offset.y == 0
                    && (rect.extent.width, rect.extent.height) == root
                    && (
                        u32::from(candidate.src_width),
                        u32::from(candidate.src_height),
                    ) == root
            });
        let authoritative_root = scanout_m2_is_authoritative_root(target, root_coverage);
        let scene_eligible = (!matches!(target, ScanoutM0Target::Unredirected)
            || self.unredirected_direct_scene_eligible(candidate.paint_dst_host_xid, root))
            && self.direct_shape_chain_covers_root(candidate.paint_dst_host_xid, root);
        // #133 step 3 (3.5): reject any candidate whose resolved paint
        // chain carries a border clip. `has_border_clip()` is true iff
        // some window between the presented drawable and its backing has
        // `border_width > 0`, which is exactly the case where content no
        // longer starts at storage (0, 0). A candidate that does not
        // resolve at all is rejected further down.
        let unbordered = paint_target.is_none_or(|t| !t.has_border_clip());
        // No transformed CRTC is ever flipped directly (spec D5).
        let eligible = !self.platform.any_output_transformed()
            && self.direct_present_crtc_eligible(candidate.crtc_id, candidate.crtc_epoch)
            && scanout_direct_eligible(
                self.scanout_allowed(),
                self.kms_outputs_active,
                matches!(
                    self.scene.cursor_mode(),
                    crate::kms::render::scene::CursorPlaneMode::Hw
                ),
                self.scene.root_overlay.is_empty(),
                authoritative_root && scene_eligible,
                unbordered,
                candidate.x_off,
                candidate.y_off,
                candidate.valid_region_xid,
            );
        if !eligible {
            // A child/video/game Present updates the COW shadow, but Muffin's
            // currently scanned root-stage buffer remains authoritative until
            // Muffin presents its next root frame. Unflipping for every child
            // Present turns playback into direct/composed thrash. Only an
            // ineligible authoritative-root successor invalidates the direct
            // ownership contract and must expose the Copy fallback.
            if authoritative_root {
                self.scanout_m2.reset_eligible_root_probation();
                self.request_direct_unflip("ineligible_authoritative_root_present");
                if self.scanout_m2.active() {
                    self.scanout_m2.unflip_fallback_source = source_id;
                    self.scanout_m2.unflip_shadow_ready = false;
                }
            }
            return Ok(false);
        }
        let Some((completion_output_idx, _)) = self.present_crtc_output(candidate.crtc_id) else {
            return Ok(false);
        };
        debug_assert_eq!(
            event.crtc_id, candidate.crtc_id,
            "direct candidate and completion must share one CRTC domain"
        );
        debug_assert_eq!(
            event.crtc_epoch, candidate.crtc_epoch,
            "direct candidate and completion must share one CRTC epoch"
        );

        if !self.scanout_m2.admit_eligible_root() {
            return Ok(false);
        }

        // Finish a previously-requested composed replacement (cursor seam,
        // overlay, or failed direct successor) before allowing direct re-entry.
        // Otherwise a fast Present stream can repeatedly replace a partial
        // dual-head unflip and starve the CRTC that did not submit yet.
        if self.scanout_m2.unflip_requested {
            return Ok(false);
        }

        let Some(source_id) = source_id else {
            self.request_direct_unflip("eligible_direct_successor_source_missing");
            return Ok(false);
        };
        let Some(fallback_target) = paint_target else {
            self.request_direct_unflip("eligible_direct_successor_paint_target_missing");
            return Ok(false);
        };
        if self.scanout_m2.active() {
            // Set only on an actual fallback below. An eligible successor
            // queued behind a direct flip must leave direct ownership intact.
            self.scanout_m2.unflip_fallback_source = None;
        }
        let framebuffer_ready = self
            .scanout_m1
            .entries
            .get(&source_id)
            .and_then(ScanoutM1ProbeEntry::framebuffer)
            .is_some();
        if !framebuffer_ready {
            self.request_direct_unflip("eligible_direct_successor_framebuffer_missing");
            return Ok(false);
        }

        let source_pin = self.pin_direct_source(source_id);
        let fallback_target_pin = self.pin_direct_source(fallback_target.backing_id());
        let present_id = candidate.present_id;
        let mut frame = DirectPresentFrame {
            source_pin,
            fallback_target_pin,
            source_id,
            candidate,
            fallback_target,
            event,
            completion_output_idx,
            completion_clock: None,
            awaiting_outputs: HashSet::new(),
        };

        if self.scanout_m2.pending.is_some() {
            self.retain_direct_present_wake(&frame.event);
            self.queue_direct_successor(frame);
            return Ok(true);
        }
        if self.scene.has_pending_page_flips() {
            self.request_direct_unflip("eligible_direct_successor_scene_flip_pending");
            self.scanout_m2.unflip_fallback_source = Some(source_id);
            self.scanout_m2.unflip_shadow_ready = false;
            <Self as Backend>::release_present_source(self, source_pin);
            <Self as Backend>::release_present_source(self, fallback_target_pin);
            return Ok(false);
        }
        if let Err(error) = self.submit_direct_frame(&mut frame) {
            self.request_direct_unflip("eligible_direct_successor_submit_failed");
            <Self as Backend>::release_present_source(self, source_pin);
            <Self as Backend>::release_present_source(self, fallback_target_pin);
            self.scanout_m2.reset_eligible_root_probation();
            return Err(error);
        }

        self.retain_direct_present_wake(&frame.event);
        self.scanout_m2.pending = Some(frame);
        self.scanout_m2.hold_direct = true;
        self.scanout_m2.unflip_requested = false;
        self.scanout_m2.unflip_reason = None;
        self.scanout_m2.unflip_last_reason = None;
        self.scanout_m2.unflip_fallback_source = None;
        self.scanout_m2.unflip_shadow_ready = false;
        log::debug!(
            "scanout_m2: live direct submit source_id={} present_id={} outputs={}",
            source_id.as_u64(),
            present_id,
            self.platform.outputs.len()
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
                    if self.dmabuf_sync_file_warned.replace(true) {
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
                    if self.dmabuf_sync_file_warned.replace(true) {
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

        let wait_id = self.next_present_source_wait_id;
        self.next_present_source_wait_id = self.next_present_source_wait_id.wrapping_add(1).max(1);
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
        self.pending_present_source_waits.insert(wait_id, pending);
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
            let supported = match self.syncobj_eventfd_supported {
                Some(v) => v,
                None => {
                    let v = self
                        .platform
                        .selected_render_device()
                        .and_then(|device| device.render_node_device.as_ref())
                        .is_some_and(crate::kms::render::imported_syncobj::eventfd_supported);
                    self.syncobj_eventfd_supported = Some(v);
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
                    if self.dmabuf_sync_file_warned.replace(true) {
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

        let wait_id = self.next_present_source_wait_id;
        self.next_present_source_wait_id = self.next_present_source_wait_id.wrapping_add(1).max(1);
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
        self.pending_present_source_waits.insert(wait_id, pending);
        Ok(PresentSourceWait::Deferred(wait_id))
    }

    pub(in crate::kms::render::backend) fn backend_present_drain_ready_present_source_waits(
        &mut self,
    ) -> Vec<u64> {
        use std::os::fd::AsFd;

        let mut ready = Vec::new();
        for (&wait_id, wait) in &mut self.pending_present_source_waits {
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

        let Some(wait) = self.pending_present_source_waits.remove(&wait_id) else {
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
        self.present_crtc_key(crtc_id).is_some_and(|key| {
            !self
                .crtc_queue_sequence_unsupported_devices
                .contains(&key.device_key)
        })
    }

    pub(in crate::kms::render::backend) fn backend_present_pin_present_source(
        &mut self,
        host_xid: u32,
    ) -> Option<u64> {
        let id = self.store.lookup(host_xid)?;
        self.store.incref(id);
        let pin_id = self.next_present_source_pin_id;
        self.next_present_source_pin_id = self.next_present_source_pin_id.wrapping_add(1).max(1);
        self.present_source_pins.insert(pin_id, id);
        Some(pin_id)
    }

    pub(in crate::kms::render::backend) fn backend_present_release_present_source(
        &mut self,
        pin_id: u64,
    ) {
        let Some(id) = self.present_source_pins.remove(&pin_id) else {
            return;
        };
        self.store_decref_with_invalidate(id);
    }

    /// Stage 5 Task 6.1 — queue a deferred PRESENT completion.
    ///
    /// COW-targeted PRESENT attaches the completion payload to the
    /// still-open COW copy batch. When that batch submits, it signals a
    /// dedicated export-only semaphore in the same queue submission;
    /// the exported sync_file FD drives completion without touching the
    /// `FenceTicket` used for yserver's internal lifetime tracking.
    /// Non-COW PRESENT whose copy is still in the open frame does the same
    /// and closes that frame at once (#214). Otherwise it falls back to one
    /// signal-only queue submit after the already-submitted copy, relying
    /// on same-queue ordering.
    pub(in crate::kms::render::backend) fn backend_present_enqueue_present_completion(
        &mut self,
        event: yserver_core::backend::CompletedPresentEvent,
        dst_host_xid: u32,
    ) {
        use yserver_core::backend::PresentWake;

        use crate::kms::render::present_completion::{
            PendingPresentBatch, PendingPresentEntry, PinnedWake, PresentBatchWait,
        };

        let wake_pin = match &event.wake {
            PresentWake::Pixmap { idle_fence_xid } if *idle_fence_xid != 0 => {
                match self.dri3_xshmfence_handle(*idle_fence_xid) {
                    Some(h) => PinnedWake::Pixmap(h),
                    None => PinnedWake::None,
                }
            }
            PresentWake::PixmapSynced {
                release,
                release_syncobj,
                release_value,
            } if *release_syncobj != 0 => PinnedWake::PixmapSynced {
                handle: release.clone(),
                value: *release_value,
            },
            _ => PinnedWake::None,
        };

        let mut entry = PendingPresentEntry { wake_pin, event };

        if let Some(cow_id) = self.cow_id
            && self.store.lookup(dst_host_xid) == Some(cow_id)
        {
            match self.engine.attach_present_completion(cow_id, entry) {
                Ok(()) => return,
                Err(returned) => entry = returned,
            }
        }

        // The copy wrote wherever `resolve_paint_target` routed it: a window
        // inside a redirected parent shares that ancestor's backing, not its
        // own leaf storage. Unviewable windows resolve to None (copy dropped).
        // A shared backing may also match another writer's op when this
        // copy clipped to nothing; harmless, the signal still follows it.
        let completion_target = self
            .resolve_paint_target(dst_host_xid)
            .map(PaintTarget::backing_id);

        // Phase A: close any open render batch FIRST so its CBs land
        // in the group under the same ticket the flush will consume.
        // Then ensure all prior paint is on the queue BEFORE the
        // completion signal. Engine-driven so any parked pending_group_ops
        // graduate to `submitted` atomically with the submit.
        // Spec § "Phase A — concrete scope" trigger 2 (Codex pass-3 fix).
        if let Err(e) = self.engine.flush_render_batch(
            &mut self.store,
            &mut self.platform,
            crate::kms::render::engine::RenderFlushReason::Present,
        ) {
            log::warn!("render enqueue_present_completion: flush_render_batch failed: {e:?}");
        }

        // #214: when the open frame holds the copy (an op writing the
        // destination), attach the completion to it and close it now: the
        // export signal rides the paint submit, one vkQueueSubmit2 instead
        // of paint + a signal-only submit. The close publishes the release
        // fence and hands the batch to the completion scheduler; a close
        // that fails keeps the entry as a ready batch (never dropped).
        // Otherwise (copy already submitted, clipped to nothing, or recorded
        // another way) fall through to the signal-only submit.
        if let Some(dst_id) = completion_target {
            match self.engine.attach_present_completion(dst_id, entry) {
                Ok(()) => {
                    if let Err(e) = self.engine.close_open_frame(
                        &mut self.store,
                        &mut self.platform,
                        crate::kms::render::frame_builder::CloseReason::PresentCompletionSignal,
                    ) {
                        log::warn!(
                            "render enqueue_present_completion: close_open_frame failed: {e:?}"
                        );
                    }
                    self.drain_frame_builder_telemetry();
                    self.drain_engine_present_batches();
                    return;
                }
                Err(returned) => entry = returned,
            }
        }
        // Phase B.1 close trigger 1b: close any open frame before the
        // signal-only submit so the semaphore-export's SYNC_FD captures a
        // queued signal-op for ANY paint work that came through the frame
        // builder. Same hazard as Task 6.1 (VUID-VkFenceGetFdInfoKHR-handleType-01457).
        if let Err(e) = self.engine.close_open_frame(
            &mut self.store,
            &mut self.platform,
            crate::kms::render::frame_builder::CloseReason::PresentCompletionSignal,
        ) {
            log::warn!("render enqueue_present_completion: close_open_frame failed: {e:?}");
        }
        // Phase B.1 Task 21: drain frame-builder close events into telemetry.
        self.drain_frame_builder_telemetry();
        if let Err(e) = self.engine.flush_submit_group(
            &mut self.store,
            &mut self.platform,
            crate::kms::render::submit_group::FlushReason::PresentCompletionSignal,
        ) {
            log::warn!("render enqueue_present_completion: flush_submit_group failed: {e:?}");
            // Fall through; the signal-only submit will fail with
            // renderer_failed and the caller's error handling kicks in.
        }

        let fallback_ticket = completion_target
            .or_else(|| self.store.lookup(dst_host_xid))
            .and_then(|id| self.store.get(id))
            .and_then(|d| d.last_render_ticket.clone());

        let mut batch_ticket = fallback_ticket;
        let (wait, signal) = match (
            self.platform.acquire_present_completion_signal(),
            self.platform.acquire_fence_ticket(),
        ) {
            (Ok(signal), Ok(ticket)) => {
                match self
                    .platform
                    .submit_present_completion_signal(&signal, ticket.fence())
                {
                    Ok(()) => {
                        batch_ticket = Some(ticket);
                        match signal.export_sync_file_fd() {
                            Ok(Some(fd)) => {
                                if let Err(e) = entry.publish_release_fence(&fd) {
                                    log::warn!(
                                        "enqueue_present_completion: publish Present release \
                                         fence failed: {e}; falling back to host signal"
                                    );
                                }
                                (PresentBatchWait::Fd(fd), Some(signal))
                            }
                            Ok(None) => (PresentBatchWait::Ready, Some(signal)),
                            Err(e) => {
                                log::warn!(
                                    "enqueue_present_completion: vkGetSemaphoreFdKHR(SYNC_FD) failed: {e:?}; \
                                     falling back to FenceTicket polling"
                                );
                                (PresentBatchWait::Poll, Some(signal))
                            }
                        }
                    }
                    Err(e) => {
                        log::warn!(
                            "enqueue_present_completion: signal-only queue submit failed: {e:?}; \
                             falling back to prior FenceTicket polling"
                        );
                        (PresentBatchWait::Poll, Some(signal))
                    }
                }
            }
            (Err(e), _) => {
                log::warn!(
                    "enqueue_present_completion: completion semaphore allocation failed: {e:?}; \
                     falling back to FenceTicket polling"
                );
                (PresentBatchWait::Poll, None)
            }
            (Ok(_signal), Err(e)) => {
                log::warn!(
                    "enqueue_present_completion: completion fence allocation failed: {e:?}; \
                     falling back to prior FenceTicket polling"
                );
                (PresentBatchWait::Poll, None)
            }
        };

        self.register_pending_present_batch(PendingPresentBatch {
            wait,
            ticket: batch_ticket,
            signal,
            events: vec![entry],
        });
    }

    /// Stage 5 Task 6.1 — drain batches whose completion semaphore has
    /// signalled (or all batches when `platform.renderer_failed`).
    /// Wake signals fire via the Arc-pinned handle inside the impl
    /// body before the events are returned to the caller.
    pub(in crate::kms::render::backend) fn backend_present_drain_completed_present_events(
        &mut self,
    ) -> Vec<yserver_core::backend::CompletedPresentEvent> {
        let mut completed = self.drain_completed_present_events_impl();
        completed.append(&mut self.scanout_m2.completed);
        completed
    }

    pub(in crate::kms::render::backend) fn backend_present_signal_present_wake(
        &mut self,
        present_id: u64,
    ) {
        use crate::kms::render::present_completion::PinnedWake;
        let Some(pin) = self.retained_present_wakes.remove(&present_id) else {
            return;
        };
        match pin {
            PinnedWake::Pixmap(h) => {
                if let Err(e) = self.dri3_trigger_fence_via_handle(&h) {
                    log::warn!("signal_present_wake: dri3_trigger_fence_via_handle failed: {e}");
                }
            }
            PinnedWake::PixmapSynced { handle, value } => {
                if let Err(e) = self.dri3_signal_syncobj_via_handle(&handle, value) {
                    log::warn!("signal_present_wake: dri3_signal_syncobj_via_handle failed: {e}");
                }
            }
            // The release point already carries the GPU completion fence.
            // Consuming the pin here drops its retained handle without
            // advancing the timeline from the host.
            PinnedWake::PixmapSyncedFencePublished {
                handle: _handle,
                value: _value,
            } => {}
            PinnedWake::None => {}
        }
    }

    pub(in crate::kms::render::backend) fn backend_present_present_crtc_clock_epoch(
        &self,
        crtc_id: u32,
    ) -> u64 {
        let Some(crtc_key) = self.present_crtc_key(crtc_id) else {
            return 0;
        };
        self.present_crtc_clock_epochs
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
    ) -> PresentCaps {
        // Mirror v1's conservative "Copy-path only" caps. syncobj
        // tracks Dri3Caps::syncobj. flip_path / async_may_tear stay
        // false until alien-BO scanout integration lands on v2.
        PresentCaps {
            flip_path: false,
            async_may_tear: false,
            syncobj: self.dri3_capabilities().syncobj,
        }
    }
}
