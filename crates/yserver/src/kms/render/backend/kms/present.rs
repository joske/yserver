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
