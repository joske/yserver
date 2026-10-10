use super::*;

impl PlatformBackend {
    pub(super) fn prune_present_clocks_to_live_outputs(&mut self) {
        let live: HashSet<CrtcKey> = self.outputs.iter().map(CrtcKey::for_output).collect();
        self.ust_msc.retain(|key, _| live.contains(key));
        self.completion_clocks.retain(|key, _| live.contains(key));
        self.software_msc.retain(|key, _| live.contains(key));
    }

    pub(crate) fn poll_fds(&self) -> Vec<(RawFd, BackendFdKind)> {
        let mut fds = Vec::with_capacity(4 + self.devices.len());
        if let Some(ctx) = self.input_ctx.as_ref() {
            fds.push((ctx.fd(), BackendFdKind::Libinput));
        }
        for device in &self.devices {
            fds.push((device.device.as_fd().as_raw_fd(), BackendFdKind::Drm));
        }
        #[cfg(target_os = "linux")]
        if let Some(mon) = self.hotplug_monitor.as_ref() {
            fds.push((mon.raw_fd(), BackendFdKind::DrmHotplug));
        }
        // Stage 5 Task 6.1: stable inner epfd for deferred PRESENT
        // completion. Always present.
        fds.push((
            self.present_completion_epfd.as_raw_fd(),
            BackendFdKind::PresentCompletion,
        ));
        fds.push((
            self.scanout_render_completion_epfd.as_raw_fd(),
            BackendFdKind::ScanoutRenderCompletion,
        ));
        fds
    }

    /// Register one source-renderer completion with the stable copied-scanout
    /// readiness set.  The returned job id is never derived from the fd and is
    /// paired with a device-qualified output identity so output-vector
    /// reordering cannot retarget a completion.
    pub(crate) fn register_scanout_render_completion(
        &mut self,
        output_key: OutputKey,
        bo_idx: usize,
        fd: Option<OwnedFd>,
    ) -> io::Result<u64> {
        let job_id = self.next_scanout_render_job_id;
        self.next_scanout_render_job_id = self
            .next_scanout_render_job_id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("scanout render job id overflow"))?;
        if let Some(fd) = fd.as_ref() {
            self.scanout_render_completion_epfd
                .register(fd.as_fd(), job_id)?;
        }
        self.pending_scanout_render_completions
            .push_back(PendingScanoutRenderCompletion {
                job_id,
                output_key,
                bo_idx,
                fd,
            });
        Ok(job_id)
    }

    /// Drain every currently readable copied-scanout render completion.
    /// Different outputs are independent, so readiness is not constrained by
    /// queue-front order.
    pub(crate) fn drain_scanout_render_completions(&mut self) -> Vec<ReadyScanoutRenderCompletion> {
        use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

        let mut ready = Vec::new();
        let mut index = 0;
        while index < self.pending_scanout_render_completions.len() {
            let is_ready = {
                let pending = &self.pending_scanout_render_completions[index];
                if let Some(fd) = pending.fd.as_ref() {
                    let mut fds = [PollFd::new(fd.as_fd(), PollFlags::POLLIN)];
                    match poll(&mut fds, PollTimeout::ZERO) {
                        Ok(0) => false,
                        Ok(_) => fds[0].revents().is_some_and(|events| {
                            events.intersects(
                                PollFlags::POLLIN | PollFlags::POLLERR | PollFlags::POLLHUP,
                            )
                        }),
                        Err(error) => {
                            log::warn!("scanout render completion poll failed: {error}");
                            true
                        }
                    }
                } else {
                    true
                }
            };
            if !is_ready {
                index += 1;
                continue;
            }
            let pending = self
                .pending_scanout_render_completions
                .remove(index)
                .expect("scanout completion index was checked");
            if let Some(fd) = pending.fd.as_ref()
                && let Err(error) = self.scanout_render_completion_epfd.unregister(fd.as_fd())
            {
                log::warn!("scanout render completion unregister failed: {error}");
            }
            ready.push(ReadyScanoutRenderCompletion {
                job_id: pending.job_id,
                output_key: pending.output_key,
                bo_idx: pending.bo_idx,
                fd: pending.fd,
            });
        }
        ready
    }

    pub(crate) fn cancel_scanout_render_completions_for_output(&mut self, output_key: &OutputKey) {
        let mut index = 0;
        while index < self.pending_scanout_render_completions.len() {
            if self.pending_scanout_render_completions[index].output_key != *output_key {
                index += 1;
                continue;
            }
            let pending = self
                .pending_scanout_render_completions
                .remove(index)
                .expect("scanout completion index was checked");
            if let Some(fd) = pending.fd.as_ref()
                && let Err(error) = self.scanout_render_completion_epfd.unregister(fd.as_fd())
            {
                log::warn!("scanout render completion cancellation unregister failed: {error}");
            }
        }
    }

    pub(crate) fn clear_scanout_render_completions(&mut self) {
        while let Some(pending) = self.pending_scanout_render_completions.pop_front() {
            if let Some(fd) = pending.fd.as_ref()
                && let Err(error) = self.scanout_render_completion_epfd.unregister(fd.as_fd())
            {
                log::warn!("scanout render completion teardown unregister failed: {error}");
            }
        }
    }

    /// Drain page-flip events that belong to the topology epoch just taken
    /// fully off-screen. A blocking ALLOW_MODESET waits prior flips, but the
    /// kernel can signal that wait just before it links the corresponding
    /// event onto the DRM fd. Wait boundedly for every CRTC known to have had
    /// a pending flip, then drain any already-ready tail. Sequence events are
    /// intentionally discarded too: all vblank arm bookkeeping was cleared
    /// when the CRTCs were disabled.
    pub(crate) fn discard_old_drm_events_after_all_off(
        &self,
        expected_pageflips: &HashSet<CrtcKey>,
        timeout: std::time::Duration,
    ) -> io::Result<()> {
        let mut expected = expected_pageflips.clone();
        let deadline = std::time::Instant::now() + timeout;

        loop {
            let wait_ms = if expected.is_empty() {
                0
            } else {
                let now = std::time::Instant::now();
                if now >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!(
                            "timed out waiting for {} old DRM page-flip event(s): {expected:?}",
                            expected.len()
                        ),
                    ));
                }
                i32::try_from((deadline - now).as_millis().max(1)).unwrap_or(i32::MAX)
            };
            let mut poll_fds: Vec<libc::pollfd> = self
                .devices
                .iter()
                .map(|device| libc::pollfd {
                    fd: device.device.as_fd().as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                })
                .collect();
            if poll_fds.is_empty() {
                return if expected.is_empty() {
                    Ok(())
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "old page flips remained but no DRM device is open",
                    ))
                };
            }
            let nfds = libc::nfds_t::try_from(poll_fds.len())
                .map_err(|_| io::Error::other("too many DRM fds to poll"))?;
            // SAFETY: `poll_fds` is a live contiguous array of `nfds`
            // initialized pollfd records for the duration of this call.
            let ready = unsafe { libc::poll(poll_fds.as_mut_ptr(), nfds, wait_ms) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if ready == 0 {
                if expected.is_empty() {
                    return Ok(());
                }
                continue;
            }

            for (device, poll_fd) in self.devices.iter().zip(&poll_fds) {
                let error_events = libc::POLLERR | libc::POLLHUP | libc::POLLNVAL;
                if poll_fd.revents & error_events != 0 {
                    return Err(io::Error::other(format!(
                        "DRM fd {} reported poll error flags 0x{:x} while draining old events",
                        poll_fd.fd, poll_fd.revents
                    )));
                }
                if poll_fd.revents & libc::POLLIN == 0 {
                    continue;
                }
                let device_key = device.key;
                crate::drm::page_flip::drain_events(
                    &device.device,
                    |crtc, _frame, _duration| {
                        expected.remove(&CrtcKey::new(device_key, crtc));
                    },
                    |_user_data, _time_ns, _sequence| {},
                )?;
            }
        }
    }

    pub(crate) fn drain_page_flip_events(
        &mut self,
        drm_fd: RawFd,
    ) -> io::Result<DrainedPageFlipEvents> {
        use ::drm::control::crtc;

        let device_index = self.drm_device_index_for_fd(drm_fd).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("page-flip readiness from unknown DRM fd {drm_fd}"),
            )
        })?;
        let device_key = self.devices[device_index].key;
        let device = Rc::clone(&self.devices[device_index].device);

        // Capture the kernel vblank (msc=frame, ust=duration) alongside the
        // CRTC so Present pacing can complete NotifyMSC with real values.
        let mut flipped: Vec<(crtc::Handle, u32, std::time::Duration)> = Vec::new();
        let mut sequenced: Vec<SequenceCompletion> = Vec::new();
        crate::drm::page_flip::drain_events(
            &device,
            |c, frame, dur| {
                flipped.push((c, frame, dur));
            },
            |user_data, time_ns, sequence| {
                // Raw kernel values; validation (time_ns sign, crtc_id
                // resolution) and tag decode happen in
                // `on_crtc_sequence_event`.
                sequenced.push(SequenceCompletion {
                    device_key,
                    user_data,
                    time_ns,
                    sequence,
                });
            },
        )?;

        let mut completions = Vec::with_capacity(flipped.len());
        for (crtc, frame, dur) in flipped {
            let crtc_key = CrtcKey::new(device_key, crtc);
            let Some(output_idx) = self.output_index_for_crtc(crtc_key) else {
                log::warn!("render: pageflip-complete for unknown CRTC {crtc:?} on {device_key}");
                continue;
            };
            // u32 frame → u64 MSC (kernel wraps at 2^32; monotonic enough
            // for a frame clock within a session). UST in microseconds.
            let ust = u64::try_from(dur.as_micros()).unwrap_or(u64::MAX);
            // apple_drm (Asahi) reports `frame == 0` on every page-flip
            // completion — the kernel does not maintain a CRTC sequence
            // counter — and rejects `DRM_IOCTL_CRTC_QUEUE_SEQUENCE` with
            // `EOPNOTSUPP`, so the idle-vblank arming path can't advance
            // the clock either. Without a non-zero MSC the Present
            // NotifyMSC path deadlocks (picom presents frame 0 then blocks
            // forever). Fall back to a per-output software counter that
            // increments on every flip when the kernel reports 0; on
            // drivers that report a real frame this stays untouched.
            let msc = if frame == 0 {
                let next = self
                    .software_msc
                    .get(&crtc_key)
                    .copied()
                    .unwrap_or(0)
                    .saturating_add(1);
                self.software_msc.insert(crtc_key, next);
                log::debug!(
                    target: "yserver::kms::render::platform",
                    "render pageflip software-msc fallback output={output_idx} msc={next} \
                     ust={ust} (kernel reports frame=0)"
                );
                next
            } else {
                u64::from(frame)
            };
            log::debug!(
                target: "yserver::kms::render::platform",
                "render pageflip ust_msc output={output_idx} msc={msc} kernel_frame={frame} kernel_ust_micros={ust}"
            );
            self.record_vblank_clock(crtc_key, msc, ust);
            let sample = PresentClockSample {
                msc,
                ust,
                source: PresentClockSource::PageFlip,
            };
            self.record_completion_clock(crtc_key, sample);
            log::debug!(
                target: "present_pace",
                "present_clock sample source=pageflip output={output_idx} msc={msc} ust={ust}"
            );
            completions.push((output_idx, sample));
        }
        Ok((completions, sequenced))
    }

    /// Latest kernel `(msc, ust_micros)` for one device-qualified CRTC, or
    /// `(0, 0)` before that display domain has produced a pageflip/sequence
    /// event. Samples from other cards or CRTCs must never influence this
    /// result: their MSC counters are unrelated even when raw handles match.
    pub(crate) fn present_get_ust_msc(&self, crtc_key: CrtcKey) -> (u64, u64) {
        self.ust_msc.get(&crtc_key).copied().unwrap_or((0, 0))
    }

    /// Latest completion-eligible clock for one device-qualified CRTC.
    pub(crate) fn present_get_completion_clock(&self, crtc_key: CrtcKey) -> PresentClockSample {
        self.completion_clocks
            .get(&crtc_key)
            .copied()
            .unwrap_or(PresentClockSample {
                msc: 0,
                ust: 0,
                source: PresentClockSource::PageFlip,
            })
    }

    /// Record a general vblank sample without allowing late events to move
    /// this CRTC domain's Present clock backwards.
    pub(crate) fn record_vblank_clock(&mut self, crtc_key: CrtcKey, msc: u64, ust: u64) {
        let replace = self.ust_msc.get(&crtc_key).is_none_or(|(old_msc, _)| {
            msc == *old_msc || yserver_core::present_scheduler::msc_is_after(msc, *old_msc)
        });
        if replace {
            self.ust_msc.insert(crtc_key, (msc, ust));
        }
    }

    /// Record a completion-eligible clock sample. At equal MSC, prefer a
    /// pageflip sample over an idle-sequence sample so provenance reflects
    /// the stronger event if both arrive for the same field.
    pub(crate) fn record_completion_clock(
        &mut self,
        crtc_key: CrtcKey,
        sample: PresentClockSample,
    ) {
        let replace = self.completion_clocks.get(&crtc_key).is_none_or(|old| {
            yserver_core::present_scheduler::msc_is_after(sample.msc, old.msc)
                || (sample.msc == old.msc
                    && (old.source != PresentClockSource::PageFlip
                        || sample.source == PresentClockSource::PageFlip))
        });
        if replace {
            self.completion_clocks.insert(crtc_key, sample);
        }
    }

    /// Page-flip-complete callback. Walks the output's BOs, finds
    /// the one currently `Pending` (just retired by the kernel),
    /// transitions its state, and returns the retirement info.
    /// `None` means no flip was pending — a spurious or
    /// startup-flushed event.
    ///
    /// The caller (SceneCompositor) then advances the matching
    /// `bo_generations[output_idx][bo_idx].last_present_generation`
    /// via [`Self::commit_bo_present`].
    pub(crate) fn on_page_flip_complete(
        &mut self,
        output_idx: usize,
    ) -> Option<PageFlipRetirement> {
        self.debug_assert_scanout_pool_route(output_idx);
        let scanout = self.scanout_pools.get_mut(output_idx)?.as_mut()?;
        // First pass: find any BO currently `Pending`. Walk only
        // — don't mutate during the search.
        let mut pending: Option<usize> = None;
        let mut on_screen: Option<usize> = None;
        let pool = scanout.display_pool_mut();
        for (i, bo) in pool.bos.iter().enumerate() {
            match bo.state.phase {
                BoPhase::Pending => {
                    if let Some(prev) = pending {
                        // More than one pending — shouldn't
                        // happen; the kernel flips one at a time.
                        log::warn!(
                            "render on_page_flip_complete: output {output_idx} has >1 pending BO; \
                             retiring first found ({prev})",
                        );
                    } else {
                        pending = Some(i);
                    }
                }
                BoPhase::OnScreen => {
                    on_screen = Some(i);
                }
                _ => {}
            }
        }
        let presented = pending?;
        // Transitions:
        //   - the previously OnScreen bo goes Retiring → Free
        //   - the previously Pending bo goes OnScreen
        // Doing it in this order matches v1's compositor.
        let retired = if let Some(prev) = on_screen {
            pool.bos[prev].state.transition_to_retiring();
            let released = pool.bos[prev].state.transition_to_free_after_retire();
            if let Some(fd) = released {
                // SAFETY: the release fence fd was owned by us;
                // close it now that the BO is free.
                unsafe { libc::close(fd) };
            }
            Some(prev)
        } else {
            None
        };
        pool.bos[presented].state.transition_to_on_screen();
        let mut copied_ownership_failed = false;
        if let Some(copied) = scanout.copied_mut() {
            if let Some(retired) = retired
                && let Err(error) = copied.note_kms_retired(retired)
            {
                log::error!(
                    "render on_page_flip_complete: copied ownership ledger failed for output \
                     {output_idx} retired bo {retired}: {error}"
                );
                copied_ownership_failed = true;
            }
            // KMS retirement proves the sink copy completed (the atomic flip
            // waited on its exported fence), so the paired A source and B
            // import-wait payload may now be reused.
            copied.release_completed_source(presented);
        }
        if copied_ownership_failed {
            self.renderer_failed = true;
        }

        let logged_first = self
            .first_pageflip_logged
            .get_mut(output_idx)
            .map(|f| std::mem::replace(f, true))
            .unwrap_or(true);
        if !logged_first {
            log::info!("render: first pageflip complete on output {output_idx} (bo {presented})",);
        } else {
            log::debug!("render: pageflip complete on output {output_idx} (bo {presented})",);
        }
        Some(PageFlipRetirement {
            retired_bo_idx: retired,
            presented_bo_idx: presented,
            generation: 0, // assigned by record_present; this is informational
        })
    }
}
