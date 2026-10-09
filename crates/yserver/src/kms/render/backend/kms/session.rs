use super::*;

pub(in crate::kms::render::backend) fn try_acquire_master_bounded<F>(
    mut attempt: F,
    attempts: usize,
    delay: std::time::Duration,
) -> io::Result<()>
where
    F: FnMut() -> io::Result<()>,
{
    for idx in 0..attempts {
        match attempt() {
            Ok(()) => return Ok(()),
            Err(err) if err.raw_os_error() == Some(libc::EBUSY) && idx + 1 < attempts => {
                std::thread::sleep(delay);
            }
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::from_raw_os_error(libc::EBUSY))
}

impl KmsBackend {
    /// Real-DRM-real-Vk constructor. Per Stage 2a, the platform
    /// layer (DRM device, output layouts, libinput, VkContext,
    /// ops command pool, fence pool, per-output scanout pools)
    /// is real; v2's `DrawableStore` / `RenderEngine` /
    /// `SceneCompositor` are still stubs and paint paths log
    /// gaps.
    ///
    /// # Errors
    ///
    /// Propagates DRM / Vk / libinput init failures from
    /// `PlatformBackend::open_with_commit`, plus FontLoader / XKB
    /// init failures from `KmsCore::new`.
    pub fn open(
        device_paths: &[std::path::PathBuf],
        console_guard: crate::kms::ConsoleGuardOpt,
        layout: Option<String>,
    ) -> io::Result<Self> {
        Self::open_with_commit(
            device_paths,
            console_guard,
            layout,
            drm::modeset::commit_modeset,
        )
    }

    fn open_with_commit(
        device_paths: &[std::path::PathBuf],
        console_guard: crate::kms::ConsoleGuardOpt,
        layout: Option<String>,
        commit: fn(
            &crate::drm::Device,
            &crate::platform::drm::Output,
            ::drm::control::framebuffer::Handle,
        ) -> io::Result<()>,
    ) -> io::Result<Self> {
        let platform = PlatformBackend::open_with_commit(device_paths, commit)?;
        let (fb_w, fb_h) = (platform.fb_w, platform.fb_h);
        let mut core = KmsCore::new(fb_w, fb_h, layout)?;
        // Warp the pointer to the centre of the primary output at startup
        // (matches Xorg). The framebuffer centre lands on the monitor seam on
        // a multi-head layout, so centre on output 0 instead.
        let (init_cx, init_cy) =
            crate::kms::backend::primary_output_center(&platform.outputs, fb_w, fb_h);
        core.cursor_x = init_cx as f32;
        core.cursor_y = init_cy as f32;
        let engine = RenderEngine::new(&platform)
            .map_err(|e| io::Error::other(format!("render RenderEngine::new failed: {e:?}")))?;
        let scene = SceneCompositor::new(&platform)
            .map_err(|e| io::Error::other(format!("render SceneCompositor::new failed: {e:?}")))?;
        let dmabuf_export_supported = platform
            .vk
            .as_ref()
            .is_some_and(probe_dmabuf_export_support);
        let crtc_config_probe_executor: Option<Box<dyn CrtcConfigProbeExecutor>> = Some(Box::new(
            crate::kms::render::probe_executor::ProcessProbeExecutor::new()?,
        ));
        let kms_outputs_active = !platform.outputs.is_empty();
        let mut b = Self {
            core,
            platform,
            logged_gaps: RefCell::new(HashSet::new()),
            store: DrawableStore::new(),
            engine,
            scene,
            windows: TrackedWindows::default(),
            subwindow_clip_cache: std::cell::RefCell::new(None),
            shape_generation: 0,
            next_window_stack_rank: 1,
            telemetry: Telemetry::new(),
            last_observed_pool_creates: 0,
            last_observed_pool_resets: 0,
            cow_id: None,
            deferred_cow_release: false,
            scanout_m0: ScanoutM0Telemetry::default(),
            scanout_m1: ScanoutM1ProbeCache::new(),
            scanout_m2: ScanoutM2State::new(),
            armed_vblank_targets: std::collections::HashMap::new(),
            absolute_vblank_targets: std::collections::HashMap::new(),
            crtc_queue_sequence_unsupported_devices: HashSet::new(),
            clip_mask_cache: None,
            depth1_mask_cache: crate::kms::backend::Depth1MaskCache::new(256),
            uniform_glyph_source_cache: crate::kms::backend::UniformGlyphSourceCache::new(64),
            clip_mask_snapshot: None,
            fill_pattern_cache: None,
            kms_outputs_active,
            clear_window_area_calls: 0,
            engine_copy_area_calls: 0,
            recent_present_pixmaps: std::collections::VecDeque::with_capacity(32),
            picture_drawable_ids: HashMap::new(),
            pending_picture_drawable_refs: HashMap::new(),
            root_readback_warn: WarnThrottle::default(),
            dst_fanout_active: false,
            dri3_xshmfences: HashMap::new(),
            dri3_sync_resources: HashMap::new(),
            dri3_syncobjs: HashMap::new(),
            syncobj_eventfd_supported: None,
            dmabuf_sync_file_warned: Cell::new(false),
            pending_present_batches: std::collections::VecDeque::new(),
            retained_present_wakes: std::collections::HashMap::new(),
            pending_present_source_waits: HashMap::new(),
            next_present_source_wait_id: 1,
            present_source_pins: HashMap::new(),
            next_present_source_pin_id: 1,
            pending_completed_events_on_shutdown: Vec::new(),
            cursor_records: HashMap::new(),
            cursor_pixmaps: HashMap::new(),
            next_cursor_version: 1,
            default_cursor_xid: None,
            input_only_pointer_hosts: HashMap::new(),
            effective_cursor_xid: None,
            grab_cursor_override: None,
            cursor_hidden: false,
            displayed_cursor_pending: None,
            anim_cursor_records: HashMap::new(),
            released_cursors: HashSet::new(),
            active_cursor_anim: None,
            last_drained_fb_opens: 0,
            // Direct mode: no on-core libinput; the core sender is installed
            // after the channel is created in `lib.rs`.
            vt_state: crate::vt::state::VtState::Active,
            vt_pending: crate::vt::state::VtPending::default(),
            console_guard,
            vt_switching_armed: false,
            led_relay: None,
            leds_sent: 0,
            floating_keyboard_states: HashMap::new(),
            lock_filter_priv_by_device: HashMap::new(),
            input_sender: None,
            crtc_config_probe_executor,
            pending_crtc_config_probes: HashMap::new(),
            ready_crtc_config_results: HashMap::new(),
            invalidated_crtc_config_probes: HashSet::new(),
            ready_crtc_config_announcements: VecDeque::new(),
            next_crtc_config_token: 1,
            next_device_config_token: 1,
            crtc_config_topology_epoch: 0,
            #[cfg(test)]
            crtc_config_discovery_override: None,
            input_thread_control: None,
            randr_id_alloc: RandrIdAllocator::default(),
            provider_output_sources: HashMap::new(),
            output_identity_by_id: std::collections::HashMap::new(),
            output_key_by_id: std::collections::HashMap::new(),
            crtc_key_by_id: std::collections::HashMap::new(),
            present_crtc_clock_epochs: HashMap::new(),
            next_present_crtc_clock_epoch: 1,
            hotplug_rescan_deadline: None,
            gamma_luts: RefCell::new(HashMap::new()),
            exported_dmabufs: HashMap::new(),
            dmabuf_export_supported,
            export_holders: Default::default(),
        };
        // Validate every route already committed during platform bring-up,
        // then apply Xorg's one-shot AutoBindGPU-shaped startup policy: every
        // opened KMS endpoint distinct from the selected renderer becomes an
        // output sink for that renderer. This remains inside the
        // construction-wide rollback window, so an invariant failure drops
        // `b` with the platform's initial-scanout rollback still armed.
        b.initialize_provider_output_sources()?;
        b.seed_initial_connector_topology()?;
        b.init_root_storage();
        // Stage 3f.8: bake the default-arrow software cursor.
        // Best-effort — a failure logs + leaves the cursor invisible
        // (matches pre-3f.8 behaviour, no regression).
        if let Err(e) = b.init_cursor_sprite() {
            log::warn!("render: software cursor init failed: {e:?} — no visible cursor");
        }
        b.arm_direct_vt_switching();
        // Every fallible outer-constructor step has now succeeded. Normal
        // shutdown owns modeset teardown from here; construction errors above
        // let PlatformBackend::drop roll back the initial scanout first.
        b.platform.disarm_initial_scanout_rollback();
        Ok(b)
    }

    fn arm_direct_vt_switching(&mut self) {
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        {
            let Some(console_guard) = self.console_guard.as_ref() else {
                return;
            };
            if self.vt_switching_armed {
                return;
            }
            match console_guard.arm_vt_process(nix::libc::SIGUSR1, nix::libc::SIGUSR2) {
                Ok(()) => {
                    self.vt_switching_armed = true;
                    log::info!("kms: direct-mode VT_PROCESS armed (SIGUSR1/SIGUSR2)");
                }
                Err(err) => {
                    log::warn!("kms: arm VT_PROCESS failed: {err}");
                }
            }
        }
    }

    /// Hand the libinput context off to the dedicated input thread.
    /// Mirrors `KmsBackend::take_input_ctx`.
    #[must_use]
    pub fn take_input_ctx(&mut self) -> Option<crate::input::SendContext> {
        self.platform.take_input_ctx()
    }

    /// Whether direct-mode VT switching has been armed on the
    /// controlling console.
    #[must_use]
    pub fn vt_switching_armed(&self) -> bool {
        self.vt_switching_armed
    }

    /// Direct-mode input-thread control handle, set when the separate
    /// libinput thread is spawned.
    pub(crate) fn set_input_thread_control(
        &mut self,
        control: std::sync::Arc<crate::input_thread::InputThreadControl>,
    ) {
        self.input_thread_control = Some(control);
    }

    pub(in crate::kms::render::backend) fn pause_input_thread(&self) -> bool {
        if let Some(control) = self.input_thread_control.as_ref() {
            control.pause();
            true
        } else {
            false
        }
    }

    pub(in crate::kms::render::backend) fn resume_input_thread(&self) {
        if let Some(control) = self.input_thread_control.as_ref() {
            control.resume();
        }
    }

    /// Notify absolute input mapping of a new virtual framebuffer extent so
    /// coordinates use the correct range after a resize or hotplug.
    ///
    /// The input-event accumulator lives on the dedicated input thread; send
    /// the new extent via `InputThreadControl::push_resize`.
    pub(in crate::kms::render::backend) fn update_input_extent(&mut self, fb_w: u16, fb_h: u16) {
        let w = u32::from(fb_w);
        let h = u32::from(fb_h);
        if let Some(ctrl) = self.input_thread_control.as_ref() {
            ctrl.push_resize(w, h);
        }
    }

    /// Initial composite + flip. v2's SceneCompositor records
    /// one compose CB per output and atomic-flips. On a fresh
    /// boot the scene typically has no mapped windows yet, so
    /// this paints the `bg_pixel` clear color and flips.
    ///
    /// # Errors
    ///
    /// Returns the first per-output Vk / DRM failure; subsequent
    /// outputs still attempted.
    pub fn composite_and_flip(&mut self, state: &ServerState) -> io::Result<()> {
        // DPMS gate: outputs are inactive, page-flip would EBUSY.
        if state.dpms.power_level != 0 {
            return Ok(());
        }
        // Gate: no modeset/pageflip/submit when not holding DRM master.
        // In Direct mode vt_state is always Active → no behaviour change.
        if !self.scanout_allowed() {
            log::debug!("render composite_and_flip: skipped (seat not Active)");
            return Ok(());
        }
        // Flush buffered paint before composing, exactly as
        // `maybe_composite` does — `scene.tick` must observe every paint CB
        // already submitted to the queue. `init_root_storage` fills the root
        // with `bg_pixel` through the batched engine, so without this the
        // first compose samples root storage *before* that fill executes and
        // flips pre-fill content (a black frame instead of the root
        // background). Worse, that compose also peeks and acks the root's
        // presentation damage, so nothing reports the difference afterwards —
        // only the unconditional full redraw of the following frames hides it.
        // Found by the damage-completeness audit; see
        // docs/superpowers/findings/2026-09-01-damage-completeness-audit.md.
        if let Err(e) = self.engine.close_open_frame(
            &mut self.store,
            &mut self.platform,
            crate::kms::render::frame_builder::CloseReason::LegacyScCompose,
        ) {
            log::warn!("render composite_and_flip: close_open_frame failed: {e:?}");
        }
        if let Err(e) = self.engine.flush_submit_group(
            &mut self.store,
            &mut self.platform,
            crate::kms::render::submit_group::FlushReason::SceneCompose,
        ) {
            log::warn!("render composite_and_flip: flush_submit_group failed: {e:?}");
        }
        let cow_host_xid = self.cow_host_xid();
        match self.scene.tick(
            &self.core,
            &mut self.store,
            &mut self.platform,
            &self.windows,
            &mut self.telemetry,
            cow_host_xid,
        ) {
            Ok(_) => Ok(()),
            Err(e) => Err(io::Error::other(format!(
                "render composite_and_flip: {e:?}"
            ))),
        }
    }

    /// Post-loop teardown — delegates to PlatformBackend, which
    /// disables each output and disarms scanout pools whose
    /// disable failed (matching v1's behaviour to avoid leaking
    /// framebuffers KMS may still hold).
    ///
    /// # Errors
    ///
    /// Propagates the first per-output `drm::modeset::disable_output`
    /// failure; subsequent outputs still attempted.
    pub fn disable_output(&mut self) -> io::Result<()> {
        // Stage 5 Task 6.1: explicitly flush open render batches
        // before drain_all walks the submitted queue. drain_all only
        // waits on already-submitted CBs; an open pending batch
        // wouldn't be there yet.
        self.drain_engine_present_batches();
        if let Err(e) = self.engine.flush_render_batch(
            &mut self.store,
            &mut self.platform,
            crate::kms::render::engine::RenderFlushReason::Other,
        ) {
            log::warn!("render disable_output: flush_render_batch failed: {e:?}");
        }

        // Drain in-flight paint + compose submits before the
        // platform's `device_wait_idle` + pool destruction so
        // each subsystem's book-keeping reclaims its handles
        // against the still-live pool.
        self.engine.shutdown(&mut self.store, &mut self.platform);
        // Phase B.1 Task 21: drain close events emitted by shutdown.
        self.drain_frame_builder_telemetry();
        self.sync_descriptor_pool_telemetry();
        self.scene.drain_all(&mut self.platform);

        // Stage 5 Task 6.1: drain the pending PRESENT batch queue
        // unconditionally. After drain_all every submitted paint
        // ticket is signaled or the renderer failed; first pop ready
        // batches via drain_completed_present_events_impl (which
        // closes sync_file FDs + fires Arc wake signals), then
        // force-fire any remaining unsignaled batches. All accumulated
        // events go to `pending_completed_events_on_shutdown` for the
        // caller (lib.rs::run) to fan out to clients before the socket
        // is torn down.
        let completed = self.drain_completed_present_events_impl();
        self.pending_completed_events_on_shutdown.extend(completed);
        let completed = self.force_drain_all_present_batches();
        self.pending_completed_events_on_shutdown.extend(completed);

        // Flush the submit trace after the drains record their
        // final events, before platform teardown — a VkDevice
        // destroy can hang on some drivers (msm/Renoir) and
        // `BufWriter::Drop` would lose the buffered tail to a
        // subsequent power-cycle. See `submit_trace::SubmitTrace::flush`.
        self.telemetry.flush_submit_trace();
        let result = self.platform.disable_output();
        if result.is_ok() {
            self.stop_direct_after_scanout_replaced("shutdown");
            self.pending_completed_events_on_shutdown
                .append(&mut self.scanout_m2.completed);
            self.scanout_m1.clear("shutdown");
        }
        result
    }

    /// Suspend sequence — called by `drive_vt_event` when the state machine
    /// decides `BeginSuspend`. `vt_state` is already `Suspending` at entry;
    /// the scanout gate is already closed.
    ///
    /// Steps:
    /// 1. Gate already closed (state is `Suspending`).
    /// 2. Release held state and active grabs for every physical source.
    /// 3. Wait for in-flight GPU work (bounded).
    /// 4. Drain pageflip and scanout state that will not receive completion
    ///    events after DRM master is dropped.
    ///
    /// # Caller
    ///
    /// `drive_vt_event` on `BeginSuspend`.
    fn run_suspend(&mut self, state: &mut ServerState) {
        log::info!(
            "kms: run_suspend enter — down_keys={} button_mask=0x{:04x}",
            self.core.down_keys.len(),
            self.core.button_mask,
        );
        // 3. Drain physical sources before yielding the VT. Source records
        // include unpublished endpoints; release_device_state leaves their
        // inventory and XI identities available for a proven continuation.
        self.synthesize_held_releases(state);

        // 3b. DPMS: post-resume the user expects "On from their
        //     perspective". No backend call here — we already gave up
        //     DRM master (or are about to in `on_vt_release`), and the
        //     resume path's commit_modeset will re-light
        //     the CRTC. No notify either — clients aren't receiving
        //     events during the suspend window; Xorg matches (clients
        //     don't see a forced transition on VT switch). Mirrors
        //     Xorg hw/xfree86/common/xf86Events.c:358-360.
        state.dpms.power_level = 0;
        state.dpms.last_activity = std::time::Instant::now();

        let direct_shadow_error = if self.scanout_m2.active() {
            self.materialize_direct_shadow_for_unflip().err()
        } else {
            None
        };

        let old_pending_pageflips = self.pending_pageflip_crtcs();

        // 4. Wait for in-flight GPU work, bounded.
        self.platform.wait_idle_bounded();

        // The primary plane is the only plane `disable_output` detaches.
        // A hardware cursor is programmed through its own plane/ioctl, and
        // some drivers (notably AMD Polaris) reject an atomic CRTC disable
        // while that plane remains attached.  Normally `scene.drain_all`
        // hides it, but that must happen *after* the all-off transaction so
        // scene acknowledgements and BO phases are not discarded while KMS
        // may still scan them out.  Hide cursor planes separately first.
        //
        // This is intentionally best-effort: a software cursor has no plane,
        // and an already-detached cursor needs no recovery.  The following
        // all-off atomic transaction remains the authoritative VT boundary.
        if let Err(error) = self.platform.cursor_plane_hide_all() {
            log::debug!("kms: VT suspend could not hide hardware cursor planes: {error}");
        }

        // 4b. Take the complete old CRTC set off-screen before discarding any
        //     scene acknowledgement or BO phase. Otherwise a surviving CRTC
        //     can still reference a buffer userspace has just made reusable.
        if let Err(error) = self.platform.dpms_set_outputs_active(false) {
            log::error!(
                "kms: VT suspend could not disable the complete old topology: {error}; exiting"
            );
            self.request_exit();
            self.clear_all_armed_vblank_targets();
            return;
        }
        self.clear_all_armed_vblank_targets();
        if let Err(error) = self.platform.discard_old_drm_events_after_all_off(
            &old_pending_pageflips,
            std::time::Duration::from_secs(1),
        ) {
            log::error!("kms: VT suspend could not drain old DRM events: {error}; exiting");
            self.request_exit();
            return;
        }
        if self.scanout_m2.active() {
            self.stop_direct_after_scanout_replaced("VT suspend");
        }

        // The old framebuffer references and events are gone. It is now safe
        // to discard the scene's ack ledger and reset every pool phase.
        self.scene.drain_all(&mut self.platform);

        // 4c. Reset the PLATFORM scanout-BO state too. `drain_all` (4b)
        //     clears the SCENE's pending_acks, but the platform pool still
        //     holds the orphaned flip's BO in Pending/OnScreen — its
        //     page-flip-complete will never arrive after master loss. Left
        //     alone, each VT round leaks a BO until `acquire_scanout_bo`
        //     starves → `reason=NoBO` wedge (observed after a few switches),
        //     plus the `on_page_flip_complete: >1 pending BO` warning from
        //     stale Pending BOs. Force every BO back to Free here so resume
        //     starts with a clean pool; the deferred full-damage repaint
        //     re-renders (content marked invalidated).
        if let Err(error) = self.platform.reset_scanout_bos_for_suspend() {
            self.kms_outputs_active = false;
            log::error!(
                "kms: VT suspend could not quiesce copied scanout devices after all outputs \
                 were disabled: {error}; preserving quarantine and exiting"
            );
            self.request_exit();
            return;
        }

        // M1 framebuffers are diagnostic-only and never active, but their GEM
        // registrations belong to this DRM-master/topology epoch.
        if !self.scanout_m2.active() {
            self.scanout_m1.clear("VT suspend");
        }

        if let Some(error) = direct_shadow_error {
            // VT release itself must finish so the kernel can switch away,
            // but the only authoritative direct source is now gone. Never
            // resume stale composed content after this safe all-off cleanup.
            log::error!("scanout_m2: VT-suspend lazy fallback Copy failed: {error}; exiting");
            self.kms_outputs_active = false;
            self.request_exit();
        }

        // Input is paused by `on_vt_release` before this runs; there is no
        // on-core libinput context to suspend here.
        log::info!("kms: run_suspend exit");
    }

    /// Resume sequence — called by `drive_vt_event` when the state
    /// machine decides `BeginResume`. `vt_state` is already
    /// `Resuming` at entry.
    ///
    /// Steps:
    /// 1. State is already `Resuming`.
    /// 2. Re-query connectors, drop missing, re-commit modeset on the
    ///    existing device. If all commits fail (card gone), log + exit
    ///    (Risk #4).
    /// 3. Re-arm the hardware cursor plane.
    /// 4. Full-damage repaint is deferred to after `resume_complete`
    ///    commits `Active` (gate must be open first) — handled in
    ///    `drive_vt_event`.
    fn run_resume(&mut self, state: &mut ServerState) -> bool {
        log::info!(
            "kms: run_resume enter — cursor=({:.0},{:.0}) effective_cursor_xid={:?}",
            self.core.cursor_x,
            self.core.cursor_y,
            self.effective_cursor_xid,
        );
        // 2. Gather every device's connector state before mutating live
        // outputs. A failed probe is fatal on resume: applying only a prefix
        // would create a fabricated combined topology.
        log::info!("kms: run_resume step 2 — probe connector snapshot");
        let snapshot = match self.platform.probe_connector_snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                log::error!("kms: resume: connector probe failed: {error}; exiting");
                self.request_exit();
                return false;
            }
        };
        let active_removed = self.platform.outputs.iter().any(|output| {
            !snapshot
                .iter()
                .any(|entry| entry.preserves_active_output(output))
        });

        // A retained direct framebuffer must be released against the old
        // routes even when the connector set is unchanged. Any active removal
        // likewise requires all old CRTCs to be disabled before pools drop.
        let topology_quiesced = self.scanout_m2.active() || active_removed;
        if topology_quiesced
            && let Err(error) = self.quiesce_before_topology_mutation("VT resume topology apply")
        {
            log::error!("kms: resume: old topology could not be quiesced: {error}");
            return false;
        }

        let configured = self.randr_id_alloc.client_configured_keys();
        let known_connected = self.randr_id_alloc.connected_keys();
        let rescan = self
            .platform
            .apply_connector_snapshot(snapshot, &known_connected);
        let registry_delta = self.reconcile_connector_registry(
            &rescan.connected,
            &rescan.dropped_keys,
            &rescan.dropped_layouts,
        );
        let active_topology_changed = !rescan.dropped_old_indices.is_empty();
        // Layout policy is the caller's (see the connector-snapshot doc
        // comment). A VT resume does not relight remembered routes — the
        // rescan that owns that runs once the VT is Active again — but it
        // must still honour the reserved slots, or a survivor packs into a
        // hole the later relight lands in (invariant 7).
        if active_topology_changed {
            let reserved = self.reserved_layout_slots();
            self.platform
                .recompact_horizontal_layout(&configured, &reserved);
            self.platform
                .recompute_fb_extent_with_reservations(&reserved);
        }
        if (!registry_delta.is_empty() || active_topology_changed)
            && !self.fire_randr_changes(
                state,
                rescan,
                &registry_delta.changed_keys,
                registry_delta.config_changed,
                active_topology_changed,
                false,
            )
        {
            return false;
        }

        // Drop armed-target entries for CRTCs retired while suspended. A
        // successful all-off quiesce cleared every arm already; prune remains
        // useful for metadata-only/no-direct transitions.
        self.prune_armed_targets_to_live_outputs();

        // 2a. Re-light every live output. The connector snapshot is read-only
        //     for surviving routes; on a VT/seat resume the kernel may have
        //     dropped the mode while another VT held DRM master, so the CRTCs
        //     are dark until we re-commit. The steady-state flip path only
        //     sets FB_ID — it can't re-establish the mode. Drive the same
        //     re-light the DPMS-on path uses; it's a no-op-cost full
        //     modeset on outputs that are already active.
        //     `dpms_set_outputs_active` attempts every output and returns the
        //     first failure after trying the rest. Any failure is fatal here:
        //     a normal FB-only flip cannot reconstruct MODE_ID/ACTIVE for a
        //     CRTC whose full resume modeset failed.
        if let Err(e) = self.platform.dpms_set_outputs_active(true) {
            // A normal FB-only flip cannot reconstruct MODE_ID/ACTIVE after a
            // failed VT-resume modeset. Claiming Active would strand the dark
            // CRTC permanently, so fail-stop even when the connector snapshot
            // itself was unchanged.
            log::error!("kms: resume: composed re-light failed: {e}; exiting");
            self.request_exit();
            self.kms_outputs_active = false;
            return false;
        }
        self.reapply_gamma_for_live_outputs();

        // 2b. DPMS: every output was just re-lit, so reconcile the backend
        //     cache. state.dpms.power_level was reset to On in run_suspend;
        //     this brings the binary cache into agreement so a later DPMS
        //     Off request actually fires the modeset commit instead of
        //     no-opping through the same-binary-state guard.
        self.kms_outputs_active = !self.platform.outputs.is_empty();

        // 3. Re-arm the hardware cursor plane. Use the current cursor
        //    position + effective cursor hotspot.
        let (hot_x, hot_y) = self
            .effective_cursor_xid
            .and_then(|xid| self.cursor_records.get(&xid))
            .map(|rec| (rec.hot_x, rec.hot_y))
            .unwrap_or((0, 0));
        #[allow(clippy::cast_possible_truncation)]
        let cx = self.core.cursor_x as i32;
        #[allow(clippy::cast_possible_truncation)]
        let cy = self.core.cursor_y as i32;
        self.platform.rearm_cursor(hot_x, hot_y, cx, cy);

        // Input is resumed by `on_vt_acquire` after this returns; there is no
        // on-core libinput context to resume here.

        // 4. Full-damage repaint deferred to `drive_vt_event` after
        //    `resume_complete` commits `Active` and opens the scanout gate.
        log::info!("kms: run_resume exit");
        true
    }

    /// Request process shutdown through the core-channel sender
    /// (same mechanism the input thread uses for Zap). Called on
    /// unrecoverable errors during resume or renderer/device loss.
    pub(in crate::kms::render::backend) fn request_exit(&self) {
        if let Some(s) = &self.input_sender {
            let _ = s.send(yserver_core::core_loop::Message::Shutdown);
        } else {
            log::error!("kms: request_exit: no input_sender — cannot signal shutdown");
        }
    }

    /// Per-event state-machine driver. Extracted so both
    /// the real VT handlers and the test injection entry point
    /// (`inject_seat_event_for_test`) share the same logic.
    pub(in crate::kms::render::backend) fn drive_vt_event(
        &mut self,
        state: &mut ServerState,
        ev: crate::vt::state::VtEventKind,
    ) {
        use crate::vt::state::{VtAction, VtEventKind};

        // Even an Active→Suspended→Active ABA cycle invalidates a result that
        // was qualified against the old DRM-master/resource lifetime.
        self.bump_crtc_config_topology_epoch("VT transition");

        // Drive the state machine to a stable state. The loop consumes
        // any counter-event coalesced into the pending flags so a fast VT
        // flip can't strand us: after a suspend, a coalesced `pending_enable`
        // resumes; after a resume, a coalesced `pending_disable` re-suspends
        // (the no-blink boundary, via `resume_complete`).
        let entry_state = self.vt_state;
        let mut action = self.vt_state.on_event(&mut self.vt_pending, ev);
        log::info!(
            "kms: drive_vt_event ev={ev:?} {entry_state:?}→{:?} action={action:?}",
            self.vt_state,
        );
        loop {
            match action {
                VtAction::BeginSuspend => {
                    self.run_suspend(state);
                    self.vt_state.suspend_complete(&self.vt_pending);
                    if self.vt_pending.pending_enable {
                        self.vt_pending.pending_enable = false;
                        // Re-drive through the state machine so it
                        // transitions Suspended → Resuming (and returns
                        // BeginResume) — resume_complete asserts Resuming.
                        action = self
                            .vt_state
                            .on_event(&mut self.vt_pending, VtEventKind::Enable);
                        continue;
                    }
                    break;
                }
                VtAction::BeginResume => {
                    if !self.run_resume(state) {
                        // Keep the state machine in Resuming with the scanout
                        // gate closed. `request_exit` has already been issued;
                        // never publish Active after a failed all-device
                        // probe/discovery or unsafe direct-scanout recovery.
                        break;
                    }
                    // `resume_complete` returns `BeginSuspend` (consuming
                    // `pending_disable`) for the no-blink boundary, else
                    // commits `Active`.
                    action = self.vt_state.resume_complete(&mut self.vt_pending);
                    if matches!(action, VtAction::BeginSuspend) {
                        continue;
                    }
                    // Committed Active: scanout gate is open — post a
                    // full-damage repaint on all outputs.
                    self.scene.wake_for_damage();
                    break;
                }
                VtAction::Nothing => {
                    log::debug!(
                        "kms: VT event {:?} ignored in state {:?}",
                        ev,
                        self.vt_state
                    );
                    break;
                }
            }
        }
    }
}

impl KmsBackend {
    pub(in crate::kms::render::backend) fn backend_session_set_input_sender(
        &mut self,
        sender: yserver_core::core_loop::CoreSender,
    ) {
        if let Some(executor) = self.crtc_config_probe_executor.as_mut() {
            executor.set_core_sender(sender.clone_handle());
        }
        self.input_sender = Some(sender);
        if !self.ready_crtc_config_announcements.is_empty() {
            self.wake_crtc_config_ready();
        }
    }

    pub(in crate::kms::render::backend) fn backend_session_request_vt_switch(&mut self, vt: u32) {
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        {
            let Some(console_guard) = self.console_guard.as_ref() else {
                log::warn!("kms: request_vt_switch({vt}) — no console guard; ignoring");
                return;
            };
            log::info!("kms: VT_ACTIVATE({vt}) — requesting switch");
            if let Err(err) = console_guard.vt_activate(vt) {
                log::warn!("kms: VT_ACTIVATE({vt}) failed: {err}");
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
        {
            log::warn!("kms: request_vt_switch({vt}) — not supported on this platform");
        }
        // No VT_WAITACTIVE: the kernel now sends us the release signal,
        // which the core loop services next (on_vt_release). Blocking here
        // would deadlock that handshake.
    }

    pub(in crate::kms::render::backend) fn backend_session_begin_vt_release(&mut self) -> bool {
        log::info!("kms: VT release — begin (pause input)");
        self.pause_input_thread()
    }

    pub(in crate::kms::render::backend) fn backend_session_finish_vt_release(
        &mut self,
        state: &mut ServerState,
        _input_inventory: &yserver_core::core_loop::input_inventory::InputInventory,
    ) {
        use crate::vt::state::VtEventKind;
        use ::drm::Device as _;

        // Step logging is load-bearing: if a switch wedges, the last line
        // printed pinpoints which step stalled (kernel blocks the VT switch
        // until VT_RELDISP, so a stall here freezes the whole session).
        log::info!("kms: VT release — input paused; run_suspend");
        self.drive_vt_event(state, VtEventKind::Disable);
        log::info!("kms: VT release — suspended; drmDropMaster");
        for device in &self.platform.devices {
            if let Err(err) = device.device.release_master_lock() {
                log::warn!("kms: drmDropMaster failed on {}: {err}", device.key);
            }
        }
        log::info!("kms: VT release — master dropped; VT_RELDISP(1)");
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        if let Some(console_guard) = self.console_guard.as_ref()
            && let Err(err) = console_guard.vt_reldisp(1)
        {
            log::warn!("kms: VT_RELDISP(1) failed: {err}");
        }
        log::info!("kms: VT release — done (switch should complete now)");
    }

    pub(in crate::kms::render::backend) fn backend_session_poll_deferred_input(
        &mut self,
        state: &mut ServerState,
    ) {
        if let Some(deadline) = self.hotplug_rescan_deadline
            && std::time::Instant::now() >= deadline
        {
            self.hotplug_rescan_deadline = None;
            self.run_display_rescan(state);
        }
    }

    pub(in crate::kms::render::backend) fn backend_session_start_device_config(
        &mut self,
        source: yserver_core::xinput::InputSourceId,
        change: yserver_core::xinput::libinput_props::DeviceConfigChange,
        cancel: yserver_core::xinput::libinput_props::DeviceConfigCancelToken,
    ) -> Result<
        yserver_core::xinput::libinput_props::DeviceConfigStart,
        yserver_core::xinput::libinput_props::DeviceConfigError,
    > {
        // libinput lives on the separate
        // input thread, so forward the write over the control channel —
        // the thread applies it to its own device map on the next wakeup.
        // The apply is async, so libinput's Unsupported/Invalid can't be
        // surfaced here; report success and let the input thread log any
        // rejection. Without this the write would be silently dropped and
        // every client device-config knob (natural scroll, tap, accel…)
        // would be a no-op under lightdm.
        let Some(control) = self.input_thread_control.as_ref() else {
            return Err(yserver_core::xinput::libinput_props::DeviceConfigError::SourceGone);
        };
        let token = yserver_core::xinput::libinput_props::DeviceConfigToken(
            self.next_device_config_token.max(1),
        );
        self.next_device_config_token = token.0.wrapping_add(1).max(1);
        control.push_config(token, source, change, cancel);
        Ok(yserver_core::xinput::libinput_props::DeviceConfigStart::Pending(token))
    }

    pub(in crate::kms::render::backend) fn backend_session_set_dpms_power(
        &mut self,
        level: u8,
    ) -> std::io::Result<()> {
        // Levels 1/2/3 collapse to "outputs off"; only 0 is "on".
        let want_active = level == 0;
        // Every path below issues a modeset/atomic commit, which requires DRM
        // master. `scanout_allowed()`'s contract is "gate every
        // master-requiring operation on this", and DPMS was not gated: on a
        // VT switch away we drop master, and a screensaver blank arriving
        // after that fails with EACCES mid-way through
        // `dpms_set_outputs_active`, leaving some outputs disabled and the
        // cached `kms_outputs_active` disagreeing with the hardware.
        //
        // Seen in the wild (discussion #79, Alpine/AMD GX-424CC, 2026-07-27):
        //   kms: run_suspend libseat disable() ok
        //   dpms: apply_dpms_transition 0 -> 3
        //   disable_output for eDP-1 failed: atomic commit rejected:
        //       Permission denied (os error 13)
        // While suspended the outputs are already dark and whoever owns the
        // VT drives its own DPMS, so skipping is also the correct behaviour,
        // not merely the safe one. `run_resume` re-establishes output state on
        // the way back, so nothing needs deferring.
        if !self.scanout_allowed() {
            log::info!(
                "kms: set_dpms_power(level={level}) — session not active \
                 (vt_state={:?}); skipping, outputs are already dark",
                self.vt_state,
            );
            return Ok(());
        }
        if want_active == self.kms_outputs_active {
            log::info!(
                "kms: set_dpms_power(level={level}) — same binary state \
                 (kms_outputs_active={}), no-op",
                self.kms_outputs_active,
            );
            return Ok(()); // same binary state (e.g. Standby → Suspend)
        }
        self.bump_crtc_config_topology_epoch("DPMS output state changed");
        log::info!(
            "kms: set_dpms_power(level={level}) — transition active={} → {want_active}",
            self.kms_outputs_active,
        );

        if want_active {
            // ── Wake side. Mirrors KmsBackend::run_resume around the
            //    modeset commit: commit_modeset, then re-arm the cursor
            //    plane via legacy ioctl. Without rearm_cursor the cursor
            //    plane stays bound to a CRTC that was disabled — the
            //    first subsequent atomic page-flip then EINVALs because
            //    the kernel sees a stale plane→CRTC reference. See
            //    project_einval_atomic_commit_storm_wedge memory entry.
            //
            // ALWAYS run rearm_cursor + wake_for_damage regardless of
            // dpms_set_outputs_active's result. That helper is best-
            // effort: it returns the FIRST per-output failure but keeps
            // attempting the rest, so a partial-success scenario (one
            // output came up, another didn't) returns Err — but the
            // outputs that DID come up still need their cursor plane
            // rebound and damage queued. Cache flip is conservative —
            // only mark fully-on if every output succeeded; on partial
            // failure, the next set_dpms_power(On) retry sees
            // kms_outputs_active=false and re-attempts (idempotent on
            // the outputs that already came up).
            let res = self.platform.dpms_set_outputs_active(true);
            self.reapply_gamma_for_live_outputs();
            let (hot_x, hot_y) = self
                .effective_cursor_xid
                .and_then(|xid| self.cursor_records.get(&xid))
                .map(|rec| (rec.hot_x, rec.hot_y))
                .unwrap_or((0, 0));
            #[allow(clippy::cast_possible_truncation)]
            let cx = self.core.cursor_x as i32;
            #[allow(clippy::cast_possible_truncation)]
            let cy = self.core.cursor_y as i32;
            log::info!("kms: dpms wake — rearm_cursor hot=({hot_x},{hot_y}) pos=({cx},{cy})");
            self.platform.rearm_cursor(hot_x, hot_y, cx, cy);
            // Outputs were dark; any incremental damage tracking is
            // stale. Force a fresh full frame on the next composite tick.
            self.scene.wake_for_damage();
            if res.is_ok() {
                self.kms_outputs_active = !self.platform.outputs.is_empty();
            }
            res
        } else {
            // ── Sleep side. The complete old CRTC set must be disabled and
            //    its queued flip events consumed before scene acknowledgements
            //    or BO phases are reset. Otherwise a live front buffer can be
            //    reused, or a stale event can retire a fresh post-wake flip.
            let direct_shadow_error = if self.scanout_m2.active() {
                self.materialize_direct_shadow_for_unflip().err()
            } else {
                None
            };
            let old_pending_pageflips = self.pending_pageflip_crtcs();
            log::info!("kms: dpms sleep — wait_idle_bounded");
            self.platform.wait_idle_bounded();
            log::info!("kms: dpms sleep — disable_output per output");
            if let Err(error) = self.platform.dpms_set_outputs_active(false) {
                // The helper attempted every CRTC, so this may be a partial
                // all-off. Without a transactional rollback the only safe
                // policy is to keep allocations/direct pins and fail-stop.
                self.kms_outputs_active = false;
                log::error!("kms: DPMS off could not disable every output: {error}; exiting");
                self.request_exit();
                return Err(error);
            }
            self.clear_all_armed_vblank_targets();
            if let Err(error) = self.platform.discard_old_drm_events_after_all_off(
                &old_pending_pageflips,
                std::time::Duration::from_secs(1),
            ) {
                log::error!("kms: DPMS off could not drain old DRM events: {error}; exiting");
                self.request_exit();
                return Err(error);
            }
            self.stop_direct_after_scanout_replaced("DPMS off");
            self.scanout_m1.clear("DPMS off");
            log::info!("kms: dpms sleep — scene.drain_all");
            self.scene.drain_all(&mut self.platform);
            log::info!("kms: dpms sleep — reset_scanout_bos_for_suspend");
            if let Err(error) = self.platform.reset_scanout_bos_for_suspend() {
                self.kms_outputs_active = false;
                log::error!(
                    "kms: DPMS off could not quiesce copied scanout devices after all outputs \
                     were disabled: {error}; preserving quarantine and exiting"
                );
                self.request_exit();
                return Err(error);
            }
            self.kms_outputs_active = false;
            if let Some(error) = direct_shadow_error {
                log::error!("scanout_m2: DPMS-off lazy fallback Copy failed: {error}; exiting");
                self.request_exit();
                return Err(error);
            }
            Ok(())
        }
    }
}
