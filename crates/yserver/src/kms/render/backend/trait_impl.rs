use super::*;

impl Backend for KmsBackend {
    // ── A. Accessors (mirror KmsBackend exactly) ────────────────

    fn window_id(&self) -> u32 {
        self.core.window_id
    }

    fn root_visual_xid(&self) -> u32 {
        self.core.root_visual_xid
    }

    fn argb_visual_xid(&self) -> Option<u32> {
        Some(ARGB_VISUAL.0)
    }

    fn argb_colormap_xid(&self) -> Option<u32> {
        Some(ARGB_COLORMAP.0)
    }

    fn fb_dimensions(&self) -> (u16, u16) {
        KmsBackend::fb_dimensions(self)
    }

    fn randr_outputs_and_modes(
        &mut self,
    ) -> (
        Vec<yserver_core::randr::RandrOutput>,
        Vec<yserver_core::randr::RandrMode>,
    ) {
        KmsBackend::randr_outputs_and_modes(self)
    }

    fn randr_providers(&mut self) -> Vec<yserver_core::randr::RandrProvider> {
        KmsBackend::randr_providers(self)
    }

    fn render_opcode(&self) -> Option<u8> {
        Some(133)
    }

    fn xkb_opcode(&self) -> Option<u8> {
        Some(136)
    }

    fn xkb_info(&self) -> Option<(u8, u8, u8)> {
        Some((136, 85, 162))
    }

    fn set_locked_group(&mut self, group: u8) {
        self.core.locked_group = self.clamp_group_to_keymap(group);
    }

    fn current_group(&self) -> u8 {
        self.effective_locked_group()
    }

    fn current_xkb_mods(&self) -> (u8, u8, u8, u8) {
        Self::backend_keyboard_current_xkb_mods(self)
    }

    fn composite_opcode(&self) -> Option<u8> {
        Some(144)
    }

    fn crtc_gamma_size(&self, crtc: u32) -> u16 {
        Self::backend_randr_crtc_gamma_size(self, crtc)
    }

    fn set_crtc_gamma(
        &mut self,
        crtc: u32,
        red: &[u16],
        green: &[u16],
        blue: &[u16],
    ) -> io::Result<()> {
        Self::backend_randr_set_crtc_gamma(self, crtc, red, green, blue)
    }

    fn get_crtc_gamma(&self, crtc: u32) -> (Vec<u16>, Vec<u16>, Vec<u16>) {
        Self::backend_randr_get_crtc_gamma(self, crtc)
    }

    fn render_format_for_ynest_id(&self, ynest_fmt: u32) -> Option<u32> {
        Self::backend_render_ops_render_format_for_ynest_id(self, ynest_fmt)
    }

    fn ping(&mut self, _origin: Option<OriginContext>) -> io::Result<()> {
        Ok(())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn xid_map(&self) -> &HostXidMap {
        &self.core.xid_map
    }

    fn sync_top_level_order(&mut self, state: &ServerState) {
        Self::backend_windows_sync_top_level_order(self, state)
    }

    fn sync_floating_keyboard_states(&mut self, state: &ServerState) {
        self.synchronize_floating_keyboard_states(state);
    }

    fn disable_xi_facet(&mut self, state: &mut ServerState, device_id: u16) {
        let _released = self.release_device_facet_holds(state, device_id);
    }

    fn enable_xi_facet(&mut self, state: &mut ServerState, _device_id: u16) {
        self.synchronize_floating_keyboard_states(state);
    }

    fn on_host_input(&mut self, state: &mut ServerState, ev: HostInputEvent) {
        Self::backend_devices_on_host_input(self, state, ev)
    }

    fn on_scanout_render_completion(&mut self, _state: &mut ServerState) {
        Self::backend_scanout_on_scanout_render_completion(self, _state)
    }

    fn on_page_flip_ready(&mut self, _state: &mut ServerState, drm_fd: std::os::fd::RawFd) {
        Self::backend_scanout_on_page_flip_ready(self, _state, drm_fd)
    }

    fn before_block(&mut self) {
        Self::backend_scanout_before_block(self)
    }

    fn mark_dirty(&mut self) {
        // Wake the compositor without inventing full-output damage.
        // Paint paths already record per-drawable presentation
        // damage, and cursor motion is projected by build_scene.
        self.scene.wake_for_damage();
    }

    fn flush_before_damage_notify(&mut self) {
        Self::backend_scanout_flush_before_damage_notify(self)
    }

    fn next_wakeup(&self) -> Option<std::time::Instant> {
        Self::backend_scanout_next_wakeup(self)
    }

    fn maybe_composite(&mut self) -> io::Result<()> {
        Self::backend_scanout_maybe_composite(self)
    }

    fn dump_scanout(&mut self) {
        Self::backend_readback_dump_scanout(self)
    }

    fn report_export_holders(
        &mut self,
        core: &dyn Fn() -> yserver_core::backend::export_holders::CoreHolders,
    ) -> bool {
        Self::backend_export_report_export_holders(self, core)
    }

    fn dump_drawables(&mut self) {
        Self::backend_dump_dump_drawables(self)
    }

    fn note_present_pixmap(&mut self, src_pixmap_xid: u32, dst_window_xid: u32) {
        Self::backend_present_note_present_pixmap(self, src_pixmap_xid, dst_window_xid)
    }

    fn note_present_scanout_candidate(&mut self, candidate: PresentScanoutCandidate) {
        self.observe_scanout_m0(candidate);
    }

    fn try_present_direct(
        &mut self,
        candidate: PresentScanoutCandidate,
        event: yserver_core::backend::CompletedPresentEvent,
    ) -> io::Result<bool> {
        Self::backend_present_try_present_direct(self, candidate, event)
    }

    fn note_present_skip(&mut self) {
        self.telemetry.record_present_skip();
    }

    fn arm_present_source_wait(
        &mut self,
        src_pixmap_host_xid: u32,
        dst_window_host_xid: u32,
    ) -> io::Result<PresentSourceWait> {
        Self::backend_present_arm_present_source_wait(
            self,
            src_pixmap_host_xid,
            dst_window_host_xid,
        )
    }

    fn arm_present_syncobj_wait(
        &mut self,
        src_pixmap_host_xid: u32,
        dst_window_host_xid: u32,
        acquire_syncobj: u32,
        acquire_value: u64,
    ) -> io::Result<PresentSourceWait> {
        Self::backend_present_arm_present_syncobj_wait(
            self,
            src_pixmap_host_xid,
            dst_window_host_xid,
            acquire_syncobj,
            acquire_value,
        )
    }

    fn drain_ready_present_source_waits(&mut self) -> Vec<u64> {
        Self::backend_present_drain_ready_present_source_waits(self)
    }

    fn begin_ready_present_destination_write(&mut self, wait_id: u64) {
        Self::backend_present_begin_ready_present_destination_write(self, wait_id)
    }

    fn finish_present_source_wait(&mut self, wait_id: u64) {
        Self::backend_present_finish_present_source_wait(self, wait_id)
    }

    fn present_flip_in_flight(&self, crtc_id: u32) -> bool {
        self.present_crtc_output(crtc_id)
            .is_some_and(|(output_idx, _)| self.present_flip_in_flight_for_output(output_idx))
    }

    fn present_display_idle(&self, crtc_id: u32) -> bool {
        self.present_crtc_key(crtc_id)
            .is_some_and(|crtc_key| self.present_completion_is_idle_for(crtc_key))
    }

    fn present_absolute_vblank_arm_supported(&self, crtc_id: u32) -> bool {
        Self::backend_present_present_absolute_vblank_arm_supported(self, crtc_id)
    }

    fn arm_present_absolute_vblank(
        &mut self,
        randr_crtc_id: u32,
        targets: &[u64],
    ) -> io::Result<usize> {
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
        let result =
            self.arm_present_absolute_vblank_with(crtc_key, targets, |crtc_key, target| {
                let crtc_id = u32::from(crtc_key.crtc);
                match crate::drm::page_flip::queue_crtc_sequence(
                    &device,
                    crtc_id,
                    /* relative */ false,
                    target,
                    absolute_seq_user_data(crtc_id),
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
                "DRM_IOCTL_CRTC_QUEUE_SEQUENCE returned EOPNOTSUPP from the absolute \
                 vblank arm on {} — disabling sequence arming on that device",
                crtc_key.device_key,
            );
            self.crtc_queue_sequence_unsupported_devices
                .insert(crtc_key.device_key);
        }
        result
    }

    fn present_scanout_blackout(&self) -> bool {
        !(self.scanout_allowed() && self.kms_outputs_active)
    }

    fn pin_present_source(&mut self, host_xid: u32) -> Option<u64> {
        Self::backend_present_pin_present_source(self, host_xid)
    }

    fn release_present_source(&mut self, pin_id: u64) {
        Self::backend_present_release_present_source(self, pin_id)
    }

    fn poll_fds(&self) -> Vec<(std::os::fd::RawFd, BackendFdKind)> {
        // Direct mode only: DRM fd + present-completion epfd. libinput runs
        // on its own thread, not the core poll.
        self.platform.poll_fds()
    }

    fn vt_switching_armed(&self) -> bool {
        self.vt_switching_armed
    }

    fn set_input_sender(&mut self, sender: yserver_core::core_loop::CoreSender) {
        Self::backend_session_set_input_sender(self, sender)
    }

    fn request_vt_switch(&mut self, vt: u32) {
        Self::backend_session_request_vt_switch(self, vt)
    }

    fn begin_vt_release(&mut self) -> bool {
        Self::backend_session_begin_vt_release(self)
    }

    fn finish_vt_release(
        &mut self,
        state: &mut ServerState,
        _input_inventory: &yserver_core::core_loop::input_inventory::InputInventory,
    ) {
        Self::backend_session_finish_vt_release(self, state, _input_inventory)
    }

    fn on_vt_acquire(&mut self, state: &mut ServerState) {
        use crate::vt::state::VtEventKind;
        use ::drm::Device as _;

        log::info!("kms: VT acquire — begin; VT_RELDISP(VT_ACKACQ)");
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        if let Some(console_guard) = self.console_guard.as_ref()
            && let Err(err) = console_guard.vt_reldisp(crate::kms::console::VT_ACKACQ)
        {
            log::warn!("kms: VT_ACKACQ failed: {err}");
        }
        log::info!("kms: VT acquire — acked; drmSetMaster");

        let mut acquired = true;
        for device in &self.platform.devices {
            if let Err(err) = try_acquire_master_bounded(
                || device.device.acquire_master_lock(),
                10,
                std::time::Duration::from_millis(5),
            ) {
                log::error!(
                    "kms: drmSetMaster failed on {} after retries: {err}",
                    device.key
                );
                acquired = false;
            }
        }
        if !acquired {
            log::error!(
                "kms: at least one DRM device did not regain master; keeping scanout closed and exiting"
            );
            for device in &self.platform.devices {
                if let Err(error) = device.device.release_master_lock() {
                    log::warn!(
                        "kms: cleanup drmDropMaster failed on {} after partial acquire: {error}",
                        device.key
                    );
                }
            }
            self.request_exit();
            return;
        }

        log::info!("kms: VT acquire — master held={acquired}; run_resume");
        self.drive_vt_event(state, VtEventKind::Enable);
        if self.vt_state != crate::vt::state::VtState::Active {
            log::error!(
                "kms: VT acquire did not reach Active ({:?}); keeping input paused",
                self.vt_state
            );
            return;
        }
        log::info!("kms: VT acquire — resumed; resume input");
        self.resume_input_thread();

        log::info!("kms: VT acquire — done");
    }

    fn on_display_hotplug(&mut self, _state: &mut ServerState) {
        Self::backend_randr_on_display_hotplug(self, _state)
    }

    fn reprobe_connectors(&mut self, state: &mut ServerState) -> io::Result<()> {
        Self::backend_randr_reprobe_connectors(self, state)
    }

    fn set_provider_output_source(
        &mut self,
        state: &mut ServerState,
        provider: u32,
        source_provider: Option<u32>,
    ) -> io::Result<bool> {
        Self::backend_randr_set_provider_output_source(self, state, provider, source_provider)
    }

    fn begin_crtc_config(
        &mut self,
        output_id: u32,
        connector: &str,
        mode: Option<yserver_core::backend::ModeSpec>,
        x: i32,
        y: i32,
    ) -> io::Result<CrtcConfigApply> {
        Self::backend_randr_begin_crtc_config(self, output_id, connector, mode, x, y)
    }

    fn drain_ready_crtc_configs(&mut self) -> Vec<CrtcConfigToken> {
        Self::backend_randr_drain_ready_crtc_configs(self)
    }

    fn finish_crtc_config(&mut self, token: CrtcConfigToken) -> io::Result<bool> {
        Self::backend_randr_finish_crtc_config(self, token)
    }

    fn cancel_crtc_config(&mut self, token: CrtcConfigToken) {
        Self::backend_randr_cancel_crtc_config(self, token)
    }

    fn apply_crtc_config(
        &mut self,
        output_id: u32,
        connector: &str,
        mode: Option<yserver_core::backend::ModeSpec>,
        x: i32,
        y: i32,
    ) -> io::Result<bool> {
        Self::backend_randr_apply_crtc_config(self, output_id, connector, mode, x, y)
    }

    fn refresh_randr_state_set_time(
        &mut self,
        state: &mut yserver_core::server::ServerState,
        set_time: u32,
    ) {
        // A CRTC set bumps lastSetTime (to the client timestamp) but NOT
        // lastConfigTime (the available configuration didn't change).
        self.rebuild_randr_state(state, Some(set_time), false);
    }

    fn randr_layout_changed(&mut self, state: &mut ServerState) {
        Self::backend_randr_randr_layout_changed(self, state)
    }

    fn output_identity(&self, output_id: u32) -> Option<(Vec<u8>, String)> {
        self.output_identity_by_id.get(&output_id).cloned()
    }

    fn set_logical_screen_size(&mut self, w: u16, h: u16) -> io::Result<()> {
        Self::backend_randr_set_logical_screen_size(self, w, h)
    }

    fn on_libinput_ready(&mut self, _state: &mut ServerState) {
        // Direct mode: libinput runs on the dedicated input thread and reaches
        // the core via Message::HostInput, so there is no on-core libinput fd
        // to dispatch here.
    }

    fn poll_deferred_input(&mut self, state: &mut ServerState) {
        Self::backend_session_poll_deferred_input(self, state)
    }

    fn start_device_config(
        &mut self,
        source: yserver_core::xinput::InputSourceId,
        change: yserver_core::xinput::libinput_props::DeviceConfigChange,
        cancel: yserver_core::xinput::libinput_props::DeviceConfigCancelToken,
    ) -> Result<
        yserver_core::xinput::libinput_props::DeviceConfigStart,
        yserver_core::xinput::libinput_props::DeviceConfigError,
    > {
        Self::backend_session_start_device_config(self, source, change, cancel)
    }

    fn probe_input_devices(&mut self, _state: &mut ServerState) -> usize {
        // Direct mode: libinput lives on the dedicated input thread; there is
        // no on-core context to drain a startup enumeration from.
        0
    }

    fn create_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        host_parent: WindowHandle,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        border_width: u16,
        visual: HostSubwindowVisual,
        background_pixel: Option<u32>,
        background_pixmap: Option<u32>,
    ) -> io::Result<WindowHandle> {
        Self::backend_windows_create_subwindow(
            self,
            _origin,
            host_parent,
            x,
            y,
            width,
            height,
            border_width,
            visual,
            background_pixel,
            background_pixmap,
        )
    }

    fn destroy_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
    ) -> io::Result<()> {
        Self::backend_windows_destroy_subwindow(self, _origin, host_xid)
    }

    fn map_subwindow(&mut self, _origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        Self::backend_windows_map_subwindow(self, _origin, host_xid)
    }

    fn realize_window_storage(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
    ) -> io::Result<()> {
        Self::backend_windows_realize_window_storage(self, _origin, host_xid)
    }

    fn release_window_storage(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
    ) -> io::Result<()> {
        Self::backend_windows_release_window_storage(self, _origin, host_xid)
    }

    fn unmap_subwindow(&mut self, _origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        Self::backend_windows_unmap_subwindow(self, _origin, host_xid)
    }

    fn configure_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        config: HostSubwindowConfig,
    ) -> io::Result<()> {
        Self::backend_windows_configure_subwindow(self, _origin, host_xid, config)
    }

    fn reparent_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        host_parent: u32,
        x: i16,
        y: i16,
    ) -> io::Result<()> {
        Self::backend_windows_reparent_subwindow(self, _origin, host_xid, host_parent, x, y)
    }

    fn change_subwindow_attributes(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        value_mask: u32,
        values: &[u32],
    ) -> io::Result<()> {
        Self::backend_windows_change_subwindow_attributes(
            self, _origin, host_xid, value_mask, values,
        )
    }

    fn update_host_event_mask(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _mask: u32,
        _enabled: bool,
    ) -> io::Result<()> {
        // No-op on KMS, same shape as v1. The trait method is a
        // holdover from Phase 6.3 ynest where it forwarded event-mask
        // changes to a host X server; KMS owns the display directly
        // and has no upstream server to notify. Event delivery on KMS
        // is driven entirely from libinput/seat plumbing inside the
        // backend, so there's nothing to update here.
        Ok(())
    }

    fn register_top_level(
        &mut self,
        _origin: Option<OriginContext>,
        nested_id: ResourceId,
        host_xid: u32,
    ) -> io::Result<()> {
        Self::backend_windows_register_top_level(self, _origin, nested_id, host_xid)
    }

    fn register_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        nested_id: ResourceId,
        host_xid: u32,
    ) -> io::Result<()> {
        Self::backend_windows_register_subwindow(self, _origin, nested_id, host_xid)
    }

    fn unregister_host_window(&mut self, host_xid: u32) {
        self.core.xid_map.remove(&host_xid);
    }

    /// Stage 4b: opt v2 into the full COMPOSITE-redirect
    /// activation path. The `process_request.rs` Composite
    /// handler gates its `activate_redirect_backing_for` call
    /// on this flag so v1 (which returns the default `false`)
    /// stays on the pre-Stage-4 "redirect record only" shape
    /// that the `92a2a83 → 3751c11` revert established.
    fn supports_redirect_activation(&self) -> bool {
        true
    }

    fn supports_dmabuf_export(&self) -> bool {
        self.dmabuf_export_supported
    }

    fn glx_vendor_names(&self) -> &'static str {
        Self::backend_export_glx_vendor_names(self)
    }

    fn acquire_glx_pixmap_export(&mut self, host_xid: u32) {
        KmsBackend::acquire_glx_pixmap_export(self, host_xid);
    }

    fn release_glx_pixmap_export(&mut self, host_xid: u32) {
        KmsBackend::release_glx_pixmap_export(self, host_xid);
    }

    fn client_disconnected(&mut self, client_id: yserver_protocol::x11::ClientId) {
        self.scene.root_overlay_on_disconnect(client_id);
    }

    fn reset_input_session(&mut self, old_state: &mut ServerState) {
        Self::backend_devices_reset_input_session(self, old_state)
    }

    fn promote_pixmap_exportable(&mut self, host_xid: u32) -> bool {
        KmsBackend::promote_pixmap_exportable(self, host_xid)
    }

    /// Stage 4c.4 — flip a window's scene-participation under
    /// COMPOSITE redirect. Delegates to `DrawableStore::
    /// set_scene_participating` (which clears unpresented
    /// presentation damage + bumps the epoch on a true→false
    /// transition per spec §I5) and fires scene-structure damage
    /// for the redirect transition.
    ///
    /// **Scene-structure damage** — always fires per the plan's
    /// Cross-cutting §"Concrete scene-structure damage":
    ///   - `participating=true` (un-redirect / Automatic-activate):
    ///     rect = W's current screen rect — the scene newly
    ///     includes W and must paint W's location.
    ///   - `participating=false` (Manual-activate): rect = W's
    ///     pre-flip rect — the scene NO LONGER includes W but
    ///     whatever is underneath must repaint the area where W
    ///     used to be.
    ///
    /// In both branches we capture the rect BEFORE the flip
    /// (pre-flip and post-flip geometry coincide because the
    /// participation flip itself doesn't move W); the only
    /// difference is semantic. When `window_absolute_rect`
    /// returns `None` (root or untracked geometry), fall back to
    /// the coarse `mark_scene_structure_dirty` — correctness-
    /// preserving, just wider than needed.
    fn set_window_scene_participation(
        &mut self,
        _origin: Option<OriginContext>,
        host_window: WindowHandle,
        participating: bool,
    ) -> io::Result<()> {
        Self::backend_redirect_set_window_scene_participation(
            self,
            _origin,
            host_window,
            participating,
        )
    }

    /// Stage 4c.4 — flip a backing's scene-participation under
    /// COMPOSITE redirect. Used by Automatic mode so paint that
    /// resolves through the backing accumulates presentation
    /// damage on B (which the scene walk picks up via W's
    /// `redirected_target` indirection in 4c's `build_scene`
    /// patch). No scene-structure damage from this call — the
    /// geometric damage of a mode-flip is the W-side call's
    /// responsibility (the blit-source identity flip is
    /// geometrically on W; backings have no on-screen geometry
    /// of their own).
    fn set_backing_scene_participation(
        &mut self,
        _origin: Option<OriginContext>,
        backing: PixmapHandle,
        participating: bool,
    ) -> io::Result<()> {
        Self::backend_redirect_set_backing_scene_participation(
            self,
            _origin,
            backing,
            participating,
        )
    }

    /// Stage 4b: real `name_window_pixmap`. Mirrors v1
    /// (`kms/backend.rs:9523-9544`) — lookup `host_window_to_backing`,
    /// incref the alias registry, return the SAME handle.
    /// Returns `NotFound` if the window isn't redirected
    /// (`allocate_redirected_backing` was never called for it).
    fn name_window_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        host_window: WindowHandle,
    ) -> io::Result<PixmapHandle> {
        Self::backend_redirect_name_window_pixmap(self, _origin, host_window)
    }

    /// Stage 4b: real `allocate_redirected_backing`. Mirrors v1
    /// (`kms/backend.rs:9568-9607`) with one v2-specific addition:
    /// after allocating the backing and registering it in
    /// `alias_registry` + `host_window_to_backing`, also flip
    /// `store.set_redirected_target(W_id, Some(B_id))` so v2's
    /// `resolve_paint_target` routes future paint to the backing.
    ///
    /// **Seed-copy ordering** per the plan's Cross-cutting
    /// §"Initial backing content" decision: the W→B copy fires
    /// BEFORE `set_redirected_target` flips routing, so the copy
    /// reads from W's own storage (not B's). Descendant seed-copy
    /// follows the same one-shot walk, in stable sibling z-order,
    /// so overlapping frame/decor children seed into the backing in
    /// the same order they would appear on screen.
    fn allocate_redirected_backing(
        &mut self,
        origin: Option<OriginContext>,
        host_window: WindowHandle,
        width: u16,
        height: u16,
        depth: u8,
    ) -> io::Result<PixmapHandle> {
        Self::backend_redirect_allocate_redirected_backing(
            self,
            origin,
            host_window,
            width,
            height,
            depth,
        )
    }

    /// Stage 4b: real `release_redirected_backing`. Mirrors v1
    /// (`kms/backend.rs:9547-9566`) — clear the
    /// `host_window_to_backing` entry, drop the Reason-1 hold,
    /// free pixmap on refcount=0.
    ///
    /// v2-specific addition: when the redirect map clears, also
    /// drop `store.set_redirected_target` for every window that
    /// was routed through this backing. Multiple windows can
    /// alias the same backing only via NameWindowPixmap (which
    /// is the alias-handle, not a separate redirect), but the
    /// loop is cheap and matches the plan's defensive contract.
    ///
    /// Stage 4c.4 round-3 finding: drop B's `scene_participating`
    /// flag internally so the protocol handler (RedirectWindow
    /// unredirect / destroy path) doesn't need a separate
    /// `set_backing_scene_participation(false)` call. The trait
    /// docstring is the canonical statement of this contract.
    fn retain_backing_storage(
        &mut self,
        _origin: Option<OriginContext>,
        backing: PixmapHandle,
    ) -> io::Result<()> {
        Self::backend_redirect_retain_backing_storage(self, _origin, backing)
    }

    fn redirected_backing_can_fit(
        &self,
        backing: PixmapHandle,
        width: u16,
        height: u16,
        depth: u8,
    ) -> bool {
        Self::backend_redirect_redirected_backing_can_fit(self, backing, width, height, depth)
    }

    fn update_redirected_backing_geometry(
        &mut self,
        _origin: Option<OriginContext>,
        backing: PixmapHandle,
        width: u16,
        height: u16,
        depth: u8,
    ) -> io::Result<()> {
        Self::backend_redirect_update_redirected_backing_geometry(
            self, _origin, backing, width, height, depth,
        )
    }

    fn drop_backing_storage(
        &mut self,
        origin: Option<OriginContext>,
        backing: PixmapHandle,
    ) -> io::Result<()> {
        Self::backend_redirect_drop_backing_storage(self, origin, backing)
    }

    fn release_window_pixmap_name(
        &mut self,
        origin: Option<OriginContext>,
        backing: PixmapHandle,
    ) -> io::Result<()> {
        Self::backend_redirect_release_window_pixmap_name(self, origin, backing)
    }

    fn release_redirected_backing(
        &mut self,
        origin: Option<OriginContext>,
        backing: PixmapHandle,
    ) -> io::Result<()> {
        Self::backend_redirect_release_redirected_backing(self, origin, backing)
    }

    /// Stage 4d — Composite Overlay Window allocation.
    ///
    /// The **0 → 1 claim edge only**: core owns the claim list and this
    /// backend counts nothing, so every call here is a first claim.
    /// Allocates screen-extent depth-24 storage at xid
    /// `COMPOSITE_OVERLAY_WINDOW` (0x103) and stores the resulting
    /// `DrawableId` on `self.cow_id`. The drawable stays off the normal
    /// scene path; xfwm4 paints its composited desktop into its own child
    /// window, so adding the COW as a topmost scene layer would cover the
    /// real output with a stale black surface.
    ///
    /// Initial fill: storage from `allocate_drawable_storage`
    /// is uninitialised Vk-DEVICE_LOCAL memory (same problem
    /// Stage 3f.14 fixed for `create_pixmap`). We do an explicit
    /// OPAQUE-black fill via `engine.fill_rect` so the
    /// compositor's first paint composites over a known value
    /// rather than recycled GPU garbage. Opaque, not transparent:
    /// the COW is depth-24, and a depth-24 drawable has no alpha
    /// channel on X11 — see `default_window_init_color`. The fill is
    /// best-effort — on the stub fixture (no Vk) `engine.fill_rect`
    /// errors; log + continue (storage already exists at xid level).
    fn get_overlay_window(&mut self, _origin: Option<OriginContext>) -> io::Result<bool> {
        Self::backend_scanout_get_overlay_window(self, _origin)
    }

    /// Stage 4d — Composite Overlay Window release.
    ///
    /// The **1 → 0 claim edge only**: core owns the claim list, so every
    /// call here is the final release. Decrefs the store storage and
    /// clears `self.cow_id`. If direct scanout is active, the logical
    /// release succeeds immediately but physical teardown is deferred
    /// until the composed replacement retires.
    /// `DrawableStore::decref` removes the xid mapping (immediately on
    /// synchronous-destroy, deferred on `PendingFence`) so the next
    /// `GetOverlayWindow` reallocates fresh storage at the same xid.
    ///
    /// Returns `Ok(false)` when nothing was materialized — defensive
    /// only; core does not call this without a claim.
    fn release_overlay_window(&mut self, _origin: Option<OriginContext>) -> io::Result<bool> {
        Self::backend_scanout_release_overlay_window(self, _origin)
    }

    fn cow_host_xid(&self) -> Option<u32> {
        Self::backend_scanout_cow_host_xid(self)
    }

    fn create_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        depth: u8,
        width: u16,
        height: u16,
    ) -> io::Result<PixmapHandle> {
        Self::backend_redirect_create_pixmap(self, _origin, depth, width, height)
    }

    fn free_pixmap(&mut self, _origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        Self::backend_redirect_free_pixmap(self, _origin, host_xid)
    }

    fn open_font(
        &mut self,
        _origin: Option<OriginContext>,
        name: &str,
    ) -> io::Result<(FontHandle, FontMetrics)> {
        Self::backend_text_open_font(self, _origin, name)
    }

    fn close_font(&mut self, _origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        Self::backend_text_close_font(self, _origin, host_xid)
    }

    fn set_font_path(
        &mut self,
        _origin: Option<OriginContext>,
        paths: &[String],
    ) -> Result<(), usize> {
        Self::backend_text_set_font_path(self, _origin, paths)
    }

    fn font_path(&self) -> Vec<String> {
        self.core.font_loader.font_path.clone()
    }

    fn paint_window_background_rect(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        Self::backend_windows_paint_window_background_rect(
            self, _origin, host_xid, x, y, width, height,
        )
    }

    fn create_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        source_pixmap: PixmapHandle,
        mask_pixmap: Option<PixmapHandle>,
        fore: (u16, u16, u16),
        back: (u16, u16, u16),
        hot_x: u16,
        hot_y: u16,
    ) -> io::Result<CursorHandle> {
        Self::backend_cursor_create_cursor(
            self,
            _origin,
            source_pixmap,
            mask_pixmap,
            fore,
            back,
            hot_x,
            hot_y,
        )
    }

    fn create_glyph_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        source_font: FontHandle,
        mask_font: Option<FontHandle>,
        source_char: u16,
        mask_char: u16,
        fore: (u16, u16, u16),
        back: (u16, u16, u16),
    ) -> io::Result<CursorHandle> {
        Self::backend_cursor_create_glyph_cursor(
            self,
            _origin,
            source_font,
            mask_font,
            source_char,
            mask_char,
            fore,
            back,
        )
    }

    fn recolor_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        fore: (u16, u16, u16),
        back: (u16, u16, u16),
    ) -> io::Result<()> {
        Self::backend_cursor_recolor_cursor(self, _origin, host_xid, fore, back)
    }

    fn create_anim_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        frames: &[(CursorHandle, u32)],
    ) -> io::Result<Option<CursorHandle>> {
        Self::backend_cursor_create_anim_cursor(self, _origin, frames)
    }

    fn free_cursor(&mut self, _origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        Self::backend_cursor_free_cursor(self, _origin, host_xid)
    }

    fn define_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        host_window_xid: u32,
        cursor_host_xid: u32,
    ) -> io::Result<()> {
        Self::backend_cursor_define_cursor(self, _origin, host_window_xid, cursor_host_xid)
    }

    fn set_grab_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        cursor_host_xid: Option<u32>,
    ) -> io::Result<()> {
        Self::backend_cursor_set_grab_cursor(self, _origin, cursor_host_xid)
    }

    fn set_container_background_pixel(
        &mut self,
        _origin: Option<OriginContext>,
        pixel: u32,
    ) -> io::Result<()> {
        Self::backend_windows_set_container_background_pixel(self, _origin, pixel)
    }

    fn set_container_background_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        host_pixmap_xid: u32,
    ) -> io::Result<()> {
        Self::backend_windows_set_container_background_pixmap(self, _origin, host_pixmap_xid)
    }

    fn clear_clip_rectangles(&mut self, _origin: Option<OriginContext>) -> io::Result<()> {
        Self::backend_clip_clear_clip_rectangles(self, _origin)
    }

    fn set_clip_rectangles(
        &mut self,
        _origin: Option<OriginContext>,
        clip: Option<ClipRectangles>,
    ) -> io::Result<()> {
        Self::backend_clip_set_clip_rectangles(self, _origin, clip)
    }

    fn set_clip_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        host_pixmap: u32,
        clip_x_origin: i16,
        clip_y_origin: i16,
    ) -> io::Result<()> {
        Self::backend_clip_set_clip_pixmap(self, _origin, host_pixmap, clip_x_origin, clip_y_origin)
    }

    fn set_gc_fill_solid(&mut self, _origin: Option<OriginContext>) -> io::Result<()> {
        Self::backend_draw_set_gc_fill_solid(self, _origin)
    }

    fn set_gc_fill_tiled(
        &mut self,
        _origin: Option<OriginContext>,
        host_pixmap: u32,
        tile_x_origin: i16,
        tile_y_origin: i16,
    ) -> io::Result<()> {
        Self::backend_draw_set_gc_fill_tiled(
            self,
            _origin,
            host_pixmap,
            tile_x_origin,
            tile_y_origin,
        )
    }

    fn apply_clip_state(
        &mut self,
        _origin: Option<OriginContext>,
        clip: &ClipState,
    ) -> io::Result<()> {
        Self::backend_clip_apply_clip_state(self, _origin, clip)
    }

    fn apply_fill_state(
        &mut self,
        _origin: Option<OriginContext>,
        fill: &FillState,
    ) -> io::Result<()> {
        Self::backend_draw_apply_fill_state(self, _origin, fill)
    }

    fn apply_draw_state(
        &mut self,
        _origin: Option<OriginContext>,
        state: &DrawState,
    ) -> io::Result<()> {
        Self::backend_draw_apply_draw_state(self, _origin, state)
    }

    fn copy_area(
        &mut self,
        _origin: Option<OriginContext>,
        src_host_xid: u32,
        dst_host_xid: u32,
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        Self::backend_draw_copy_area(
            self,
            _origin,
            src_host_xid,
            dst_host_xid,
            src_x,
            src_y,
            dst_x,
            dst_y,
            width,
            height,
        )
    }

    fn copy_plane(
        &mut self,
        _origin: Option<OriginContext>,
        src_host_xid: u32,
        dst_host_xid: u32,
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
        plane: u32,
    ) -> io::Result<()> {
        Self::backend_draw_copy_plane(
            self,
            _origin,
            src_host_xid,
            dst_host_xid,
            src_x,
            src_y,
            dst_x,
            dst_y,
            width,
            height,
            plane,
        )
    }

    fn put_image(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        depth: u8,
        width: u16,
        height: u16,
        dst_x: i16,
        dst_y: i16,
        data: &[u8],
    ) -> io::Result<()> {
        Self::backend_draw_put_image(
            self, _origin, host_xid, depth, width, height, dst_x, dst_y, data,
        )
    }

    fn get_image(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        format: u8,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        plane_mask: u32,
    ) -> io::Result<Option<Vec<u8>>> {
        Self::backend_draw_get_image(
            self, _origin, host_xid, format, x, y, width, height, plane_mask,
        )
    }

    fn read_depth1_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
    ) -> io::Result<Option<(u32, u32, Vec<u8>)>> {
        Self::backend_draw_read_depth1_pixmap(self, _origin, host_xid)
    }

    fn clear_area(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        background_pixel: u32,
        background_pixmap_host_xid: Option<u32>,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        tile_origin: (i32, i32),
    ) -> io::Result<()> {
        self.clear_window_area_with_background(
            host_xid,
            background_pixel,
            background_pixmap_host_xid,
            x,
            y,
            width,
            height,
            tile_origin,
        )
    }

    fn poly_line(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        coordinate_mode: u8,
        points: &[u8],
    ) -> io::Result<()> {
        Self::backend_draw_poly_line(self, origin, host_xid, foreground, coordinate_mode, points)
    }

    fn poly_segment(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        segments: &[u8],
    ) -> io::Result<()> {
        Self::backend_draw_poly_segment(self, origin, host_xid, foreground, segments)
    }

    fn poly_rectangle(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        rectangles: &[u8],
    ) -> io::Result<()> {
        Self::backend_draw_poly_rectangle(self, origin, host_xid, foreground, rectangles)
    }

    fn poly_arc(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        arcs: &[u8],
    ) -> io::Result<()> {
        Self::backend_draw_poly_arc(self, origin, host_xid, foreground, arcs)
    }

    fn poly_point(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        coordinate_mode: u8,
        points: &[u8],
    ) -> io::Result<()> {
        Self::backend_draw_poly_point(self, origin, host_xid, foreground, coordinate_mode, points)
    }

    fn poly_fill_rectangle(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        rectangles: &[u8],
    ) -> io::Result<()> {
        Self::backend_draw_poly_fill_rectangle(self, origin, host_xid, foreground, rectangles)
    }

    fn poly_fill_arc(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        arcs: &[u8],
    ) -> io::Result<()> {
        Self::backend_draw_poly_fill_arc(self, origin, host_xid, foreground, arcs)
    }

    fn fill_poly(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        coord_mode: u8,
        points: &[u8],
    ) -> io::Result<()> {
        Self::backend_draw_fill_poly(self, origin, host_xid, foreground, coord_mode, points)
    }

    fn fill_rectangle(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        Self::backend_draw_fill_rectangle(self, origin, host_xid, foreground, x, y, width, height)
    }

    fn poly_text8(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        body: &[u8],
    ) -> io::Result<()> {
        Self::backend_draw_poly_text8(self, origin, host_xid, foreground, body)
    }

    fn poly_text16(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        body: &[u8],
    ) -> io::Result<()> {
        Self::backend_draw_poly_text16(self, origin, host_xid, foreground, body)
    }

    fn image_text8(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        background: u32,
        text_len: u8,
        body: &[u8],
    ) -> io::Result<()> {
        Self::backend_draw_image_text8(
            self, origin, host_xid, foreground, background, text_len, body,
        )
    }

    fn image_text16(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        background: u32,
        text_len: u8,
        body: &[u8],
    ) -> io::Result<()> {
        Self::backend_draw_image_text16(
            self, origin, host_xid, foreground, background, text_len, body,
        )
    }

    fn render_create_picture(
        &mut self,
        _origin: Option<OriginContext>,
        host_drawable: AnyHandle,
        ynest_format: u32,
        value_mask: u32,
        values: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        Self::backend_render_ops_render_create_picture(
            self,
            _origin,
            host_drawable,
            ynest_format,
            value_mask,
            values,
        )
    }

    fn render_change_picture(
        &mut self,
        _origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()> {
        Self::backend_render_ops_render_change_picture(self, _origin, host_pic, body)
    }

    /// Audit #8 (2026-05-19) — store the drawable-space origin of
    /// the wrapped surface on the picture record. The protocol
    /// layer calls this right after `render_create_picture` with
    /// the parent-relative `(x, y)` of a window-backed drawable
    /// (process_request.rs:1153). Pre-fix v2 inherited the trait
    /// default no-op so `drawable_origin` stayed at the
    /// `drawable_default` `(0, 0)` — clips on CSD-frame-child
    /// pictures couldn't translate external region geometry into
    /// picture-local coords.
    ///
    /// Non-Drawable picture variants (SolidFill / Linear /
    /// Radial gradient) have no drawable to anchor — tolerated
    /// no-op so the caller doesn't need to discriminate at the
    /// call site.
    fn set_picture_drawable_origin(&mut self, host_pic: u32, origin: (i16, i16)) {
        Self::backend_render_ops_set_picture_drawable_origin(self, host_pic, origin)
    }

    /// Audit #8 (2026-05-19) — return the picture's `clientClip` for
    /// `CreateRegionFromPicture` (XFixes). Outer `Option` distinguishes
    /// "picture doesn't carry a clientClip at all" (Solidfill /
    /// gradient → `None`, dispatcher emits BadMatch) from "picture
    /// exists and we know its clip state" (Drawable → `Some(_)`).
    /// Inner `Option` distinguishes "no clip set yet" (`Some(None)`,
    /// also BadMatch per X11 spec — can't extract a region from a
    /// picture with no clip) from "clip set" (`Some(Some(rects))`,
    /// returned as the region's rects).
    ///
    /// Pre-fix v2 inherited the trait default `None` so EVERY
    /// `CreateRegionFromPicture` call returned BadMatch — even for
    /// pictures with legitimate clipped state. Visible in clipboard
    /// managers / window managers that use this XFixes path.
    fn picture_client_clip_rects(
        &mut self,
        host_pic: u32,
    ) -> Option<Option<Vec<yserver_protocol::x11::xfixes::RegionRect>>> {
        Self::backend_clip_picture_client_clip_rects(self, host_pic)
    }

    fn render_free_picture(
        &mut self,
        _origin: Option<OriginContext>,
        host_pic: u32,
    ) -> io::Result<()> {
        Self::backend_render_ops_render_free_picture(self, _origin, host_pic)
    }

    fn render_create_glyphset(
        &mut self,
        _origin: Option<OriginContext>,
        ynest_format: u32,
    ) -> io::Result<Option<GlyphSetHandle>> {
        Self::backend_render_ops_render_create_glyphset(self, _origin, ynest_format)
    }

    fn render_free_glyphset(
        &mut self,
        _origin: Option<OriginContext>,
        host_gs: u32,
    ) -> io::Result<()> {
        Self::backend_render_ops_render_free_glyphset(self, _origin, host_gs)
    }

    fn render_add_glyphs(
        &mut self,
        _origin: Option<OriginContext>,
        host_gs: u32,
        body_tail: &[u8],
    ) -> io::Result<()> {
        Self::backend_render_ops_render_add_glyphs(self, _origin, host_gs, body_tail)
    }

    fn render_free_glyphs(
        &mut self,
        _origin: Option<OriginContext>,
        host_gs: u32,
        glyph_ids: &[u8],
    ) -> io::Result<()> {
        Self::backend_render_ops_render_free_glyphs(self, _origin, host_gs, glyph_ids)
    }

    /// #135 — acquire the IncludeInferiors root snapshot, run the composite,
    /// then release the snapshot on the way out.
    ///
    /// The split exists so the release has exactly ONE site. The first version
    /// of this freed the scratch pixmap at each of the six exits of
    /// `render_composite_inner` by hand, which is a leak waiting for the next
    /// early return to be added — one screen of storage per composite, and
    /// nothing in-tree can catch it because the snapshot only materialises
    /// with a live scanout (codex flagged exactly this risk). Structure it out
    /// instead of testing for it.
    fn render_composite(
        &mut self,
        _origin: Option<OriginContext>,
        op: u8,
        host_src: u32,
        host_mask: u32,
        host_dst: u32,
        src_x: i16,
        src_y: i16,
        mask_x: i16,
        mask_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<Vec<xfixes::RegionRect>> {
        Self::backend_render_ops_render_composite(
            self, _origin, op, host_src, host_mask, host_dst, src_x, src_y, mask_x, mask_y, dst_x,
            dst_y, width, height,
        )
    }

    fn render_composite_glyphs(
        &mut self,
        _origin: Option<OriginContext>,
        minor: u8,
        op: u8,
        host_src: u32,
        host_dst: u32,
        mask_fmt: u32,
        host_gs: u32,
        src_x: i16,
        src_y: i16,
        items: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<Vec<xfixes::RegionRect>> {
        Self::backend_text_render_composite_glyphs(
            self, _origin, minor, op, host_src, host_dst, mask_fmt, host_gs, src_x, src_y, items,
            x_off, y_off,
        )
    }

    fn picture_includes_inferiors(&self, host_pic: u32) -> bool {
        Self::backend_render_ops_picture_includes_inferiors(self, host_pic)
    }

    fn render_fill_rectangles(
        &mut self,
        _origin: Option<OriginContext>,
        host_dst: u32,
        op: u8,
        color: [u8; 8],
        rects: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()> {
        Self::backend_render_ops_render_fill_rectangles(
            self, _origin, host_dst, op, color, rects, x_off, y_off,
        )
    }

    fn render_trapezoids(
        &mut self,
        _origin: Option<OriginContext>,
        op: u8,
        host_src: u32,
        host_dst: u32,
        _host_mask_format: u32,
        src_x: i16,
        src_y: i16,
        traps: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<Vec<xfixes::RegionRect>> {
        Self::backend_render_ops_render_trapezoids(
            self,
            _origin,
            op,
            host_src,
            host_dst,
            _host_mask_format,
            src_x,
            src_y,
            traps,
            x_off,
            y_off,
        )
    }

    fn render_triangles_op(
        &mut self,
        _origin: Option<OriginContext>,
        minor: u8,
        op: u8,
        host_src: u32,
        host_dst: u32,
        _host_mask_format: u32,
        src_x: i16,
        src_y: i16,
        primitives: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<Vec<xfixes::RegionRect>> {
        Self::backend_render_ops_render_triangles_op(
            self,
            _origin,
            minor,
            op,
            host_src,
            host_dst,
            _host_mask_format,
            src_x,
            src_y,
            primitives,
            x_off,
            y_off,
        )
    }

    fn render_create_solid_fill(
        &mut self,
        _origin: Option<OriginContext>,
        color: [u8; 8],
    ) -> io::Result<Option<PictureHandle>> {
        Self::backend_render_ops_render_create_solid_fill(self, _origin, color)
    }

    fn render_create_linear_gradient(
        &mut self,
        _origin: Option<OriginContext>,
        body: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        Self::backend_render_ops_render_create_linear_gradient(self, _origin, body)
    }

    fn render_create_radial_gradient(
        &mut self,
        _origin: Option<OriginContext>,
        body: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        Self::backend_render_ops_render_create_radial_gradient(self, _origin, body)
    }

    fn render_create_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        host_src_pic: PictureHandle,
        x: u16,
        y: u16,
    ) -> io::Result<Option<CursorHandle>> {
        Self::backend_cursor_render_create_cursor(self, _origin, host_src_pic, x, y)
    }

    fn render_set_picture_clip_rectangles(
        &mut self,
        _origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()> {
        Self::backend_clip_render_set_picture_clip_rectangles(self, _origin, host_pic, body)
    }

    fn render_set_picture_filter(
        &mut self,
        _origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()> {
        Self::backend_render_ops_render_set_picture_filter(self, _origin, host_pic, body)
    }

    fn render_set_picture_transform(
        &mut self,
        _origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()> {
        Self::backend_render_ops_render_set_picture_transform(self, _origin, host_pic, body)
    }

    fn render_query_version(&mut self, _origin: Option<OriginContext>) -> io::Result<(u32, u32)> {
        // Advertise RENDER 0.11 (the version v1 reports). Stubbed
        // paint paths still need the version reply to flow through;
        // skipping it would break clients at extension query.
        Ok((0, 11))
    }

    fn dri3_open(&mut self, _drawable: u32) -> io::Result<std::os::fd::OwnedFd> {
        Self::backend_export_dri3_open(self, _drawable)
    }

    fn dri3_capabilities(&self) -> Dri3Caps {
        Self::backend_export_dri3_capabilities(self)
    }

    fn dri3_import_pixmap(
        &mut self,
        fd: std::os::fd::OwnedFd,
        width: u16,
        height: u16,
        stride: u32,
        offset: u32,
        modifier: Dri3ImportModifier,
        depth: u8,
        bpp: u8,
    ) -> io::Result<PixmapHandle> {
        Self::backend_export_dri3_import_pixmap(
            self, fd, width, height, stride, offset, modifier, depth, bpp,
        )
    }

    fn dri3_supported_modifiers(&self, _window: u32, depth: u8, bpp: u8) -> (Vec<u64>, Vec<u64>) {
        Self::backend_export_dri3_supported_modifiers(self, _window, depth, bpp)
    }

    fn dri3_export_pixmap(
        &mut self,
        host_xid: u32,
    ) -> io::Result<(u32, u16, u16, u16, u8, u8, std::os::fd::OwnedFd)> {
        Self::backend_export_dri3_export_pixmap(self, host_xid)
    }

    fn dri3_export_pixmap_buffers(&mut self, host_xid: u32) -> io::Result<Dri3PixmapExport> {
        Self::backend_export_dri3_export_pixmap_buffers(self, host_xid)
    }

    fn dri3_fence_from_fd(&mut self, fence_xid: u32, fd: std::os::fd::OwnedFd) -> io::Result<()> {
        Self::backend_export_dri3_fence_from_fd(self, fence_xid, fd)
    }

    fn dri3_trigger_fence(&mut self, fence_xid: u32) -> io::Result<()> {
        Self::backend_export_dri3_trigger_fence(self, fence_xid)
    }

    fn dri3_fence_triggered(&self, fence_xid: u32) -> Option<bool> {
        Self::backend_export_dri3_fence_triggered(self, fence_xid)
    }

    fn dri3_reset_fence(&mut self, fence_xid: u32) {
        Self::backend_export_dri3_reset_fence(self, fence_xid)
    }

    fn dri3_destroy_fence(&mut self, fence_xid: u32) {
        Self::backend_export_dri3_destroy_fence(self, fence_xid)
    }

    fn dri3_xshmfence_handle(
        &self,
        fence_xid: u32,
    ) -> Option<std::sync::Arc<dyn yserver_core::backend::XshmfenceHandle>> {
        Self::backend_export_dri3_xshmfence_handle(self, fence_xid)
    }

    fn dri3_syncobj_handle(
        &self,
        syncobj_xid: u32,
    ) -> Option<std::sync::Arc<dyn yserver_core::backend::SyncobjHandle>> {
        Self::backend_export_dri3_syncobj_handle(self, syncobj_xid)
    }

    fn dri3_syncobj_owned(
        &self,
        client_id: yserver_protocol::x11::ClientId,
        syncobj_xid: u32,
    ) -> bool {
        Self::backend_export_dri3_syncobj_owned(self, client_id, syncobj_xid)
    }

    fn dri3_fd_from_fence(&mut self, fence_xid: u32) -> io::Result<std::os::fd::OwnedFd> {
        Self::backend_export_dri3_fd_from_fence(self, fence_xid)
    }

    fn dri3_import_syncobj(
        &mut self,
        client_id: yserver_protocol::x11::ClientId,
        syncobj_xid: u32,
        fd: std::os::fd::OwnedFd,
    ) -> io::Result<()> {
        Self::backend_export_dri3_import_syncobj(self, client_id, syncobj_xid, fd)
    }

    fn dri3_free_syncobj(
        &mut self,
        client_id: yserver_protocol::x11::ClientId,
        syncobj_xid: u32,
    ) -> io::Result<()> {
        Self::backend_export_dri3_free_syncobj(self, client_id, syncobj_xid)
    }

    fn dri3_signal_syncobj(&mut self, syncobj_xid: u32, value: u64) -> io::Result<()> {
        Self::backend_export_dri3_signal_syncobj(self, syncobj_xid, value)
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
    fn enqueue_present_completion(
        &mut self,
        event: yserver_core::backend::CompletedPresentEvent,
        dst_host_xid: u32,
    ) {
        Self::backend_present_enqueue_present_completion(self, event, dst_host_xid)
    }

    /// Stage 5 Task 6.1 — drain batches whose completion semaphore has
    /// signalled (or all batches when `platform.renderer_failed`).
    /// Wake signals fire via the Arc-pinned handle inside the impl
    /// body before the events are returned to the caller.
    fn drain_completed_present_events(
        &mut self,
    ) -> Vec<yserver_core::backend::CompletedPresentEvent> {
        Self::backend_present_drain_completed_present_events(self)
    }

    fn drain_retired_present_idle_events(
        &mut self,
    ) -> Vec<yserver_core::backend::CompletedPresentEvent> {
        std::mem::take(&mut self.scanout_m2.idled)
    }

    fn signal_present_wake(&mut self, present_id: u64) {
        Self::backend_present_signal_present_wake(self, present_id)
    }

    fn present_crtc_clock_epoch(&self, crtc_id: u32) -> u64 {
        Self::backend_present_present_crtc_clock_epoch(self, crtc_id)
    }

    fn present_get_ust_msc(&self, crtc_id: u32) -> (u64, u64) {
        Self::backend_present_present_get_ust_msc(self, crtc_id)
    }

    fn present_get_completion_clock(
        &self,
        crtc_id: u32,
    ) -> yserver_core::backend::PresentClockSample {
        Self::backend_present_present_get_completion_clock(self, crtc_id)
    }

    fn arm_idle_vblanks(&mut self, crtc_id: u32, target_mscs: &[u64]) -> std::io::Result<usize> {
        self.arm_idle_vblanks_ioctl(crtc_id, target_mscs)
    }

    fn arm_present_completion_idle_vblanks(
        &mut self,
        crtc_id: u32,
        target_mscs: &[u64],
    ) -> std::io::Result<usize> {
        Self::backend_present_arm_present_completion_idle_vblanks(self, crtc_id, target_mscs)
    }

    fn present_capabilities(&self, _window: u32) -> PresentCaps {
        Self::backend_present_present_capabilities(self, _window)
    }

    fn xkb_proxy(
        &mut self,
        _origin: Option<OriginContext>,
        minor: u8,
        body: &[u8],
        intern_atom: &mut dyn FnMut(&str) -> u32,
    ) -> io::Result<Option<Vec<u8>>> {
        Self::backend_keyboard_xkb_proxy(self, _origin, minor, body, intern_atom)
    }

    fn xkb_set(
        &mut self,
        minor: u8,
        body: &[u8],
        client_is_ancient: bool,
        atom_name: &dyn Fn(u32) -> Option<String>,
    ) -> Option<yserver_core::backend::XkbSetOutcome> {
        Self::backend_keyboard_xkb_set(self, minor, body, client_is_ancient, atom_name)
    }

    fn xkb_get_kbd_by_name(
        &mut self,
        body: &[u8],
        intern_atom: &mut dyn FnMut(&str) -> u32,
    ) -> Option<(Vec<u8>, Option<yserver_core::backend::XkbNewKeyboardInfo>)> {
        Self::backend_keyboard_xkb_get_kbd_by_name(self, body, intern_atom)
    }

    fn set_keymap_rmlvo(
        &mut self,
        rules: &str,
        model: &str,
        layout: &str,
        variant: &str,
        options: Option<&str>,
    ) -> Option<(u8, u8)> {
        Self::backend_keyboard_set_keymap_rmlvo(self, rules, model, layout, variant, options)
    }

    fn current_xkb_rules_names(&self) -> Option<[String; 5]> {
        Self::backend_keyboard_current_xkb_rules_names(self)
    }

    fn get_active_cursor_image(&self) -> Option<yserver_core::backend::ActiveCursorImage> {
        Self::backend_cursor_get_active_cursor_image(self)
    }

    fn load_keymap_by_components(&mut self, symbols: &str) -> KeymapLoad {
        Self::backend_keyboard_load_keymap_by_components(self, symbols)
    }

    fn replace_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        old_host_xid: u32,
        new_host_xid: u32,
    ) -> io::Result<()> {
        Self::backend_cursor_replace_cursor(self, _origin, old_host_xid, new_host_xid)
    }

    fn set_cursor_hidden(&mut self, hidden: bool) {
        self.apply_cursor_hidden(hidden);
    }

    fn take_displayed_cursor_change(&mut self) -> Option<yserver_core::backend::DisplayedCursor> {
        self.displayed_cursor_pending.take()
    }

    fn set_shape_rectangles(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        kind: u8,
        rects: Option<&[xfixes::RegionRect]>,
    ) -> io::Result<()> {
        Self::backend_windows_set_shape_rectangles(self, _origin, host_xid, kind, rects)
    }

    // ── Misc ────────────────────────────────────────────────────

    fn warp_pointer(
        &mut self,
        _origin: Option<OriginContext>,
        _dst_host_xid: u32,
        _dst_x: i16,
        _dst_y: i16,
    ) -> io::Result<()> {
        // The window-relative form is unused on KMS — the handler
        // resolves the destination to root coords (only ServerState
        // knows window positions) and calls `warp_pointer_root`.
        Ok(())
    }

    fn warp_pointer_root(&mut self, state: &mut ServerState, x: i32, y: i32) {
        Self::backend_pointer_warp_pointer_root(self, state, x, y)
    }

    fn windows_restructured(&mut self, state: &mut ServerState) {
        Self::backend_windows_windows_restructured(self, state)
    }

    fn query_pointer(&mut self, _origin: Option<OriginContext>) -> io::Result<PointerPosition> {
        Self::backend_pointer_query_pointer(self, _origin)
    }

    fn list_fonts_proxy(
        &mut self,
        _origin: Option<OriginContext>,
        max_names: u16,
        pattern: &str,
    ) -> io::Result<Vec<u8>> {
        Self::backend_text_list_fonts_proxy(self, _origin, max_names, pattern)
    }

    fn list_fonts_with_info_proxy(
        &mut self,
        _origin: Option<OriginContext>,
        max_names: u16,
        pattern: &str,
        intern_atom: &mut dyn FnMut(&str) -> u32,
    ) -> io::Result<Vec<Vec<u8>>> {
        Self::backend_text_list_fonts_with_info_proxy(
            self,
            _origin,
            max_names,
            pattern,
            intern_atom,
        )
    }

    fn get_atom_name(
        &mut self,
        _origin: Option<OriginContext>,
        _atom: u32,
    ) -> io::Result<Option<String>> {
        // Atom store lives in ServerState, not the backend. v2 has
        // nothing to add here.
        Ok(None)
    }

    fn get_keyboard_mapping(
        &mut self,
        _origin: Option<OriginContext>,
        first_keycode: u8,
        count: u8,
    ) -> io::Result<(u8, Vec<u32>)> {
        Self::backend_keyboard_get_keyboard_mapping(self, _origin, first_keycode, count)
    }

    fn change_keyboard_mapping(
        &mut self,
        first_keycode: u8,
        keysyms_per_keycode: u8,
        keysyms: &[u32],
    ) -> Option<yserver_core::backend::KeyboardMappingChange> {
        Some(self.apply_keyboard_mapping(first_keycode, keysyms_per_keycode, keysyms))
    }

    fn set_modifier_mapping(
        &mut self,
        modmap: &[u8; 256],
    ) -> Option<yserver_core::backend::KeyboardMappingChange> {
        Some(self.apply_modifier_mapping(modmap))
    }

    fn keymap_auto_repeats(&self) -> Option<[u8; 32]> {
        Some(self.core.xkb_desc.per_key_repeat)
    }

    fn get_modifier_mapping(
        &mut self,
        _origin: Option<OriginContext>,
    ) -> io::Result<(u8, Vec<u8>)> {
        // Xorg's generate_modkeymap over the description's modmap, the same data XKB GetMap sends.
        Ok(self.core.xkb_desc.modifier_mapping())
    }

    fn dpms_capable(&self) -> bool {
        true
    }

    fn set_dpms_power(&mut self, level: u8) -> std::io::Result<()> {
        Self::backend_session_set_dpms_power(self, level)
    }
}
