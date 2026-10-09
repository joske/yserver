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
        let s = &self.core.xkb_state.0;
        (
            s.serialize_mods(xkbcommon::xkb::STATE_MODS_EFFECTIVE) as u8,
            s.serialize_mods(xkbcommon::xkb::STATE_MODS_DEPRESSED) as u8,
            s.serialize_mods(xkbcommon::xkb::STATE_MODS_LATCHED) as u8,
            s.serialize_mods(xkbcommon::xkb::STATE_MODS_LOCKED) as u8,
        )
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
        // Stage 3f.7 port of v1's on_host_input. Key events go
        // through the cook → key fanout path; pointer events flow
        // into `pending_pointer_events`, which we drain to the
        // pointer fanout after each call so the buffer stays empty
        // between events (matches v1's contract).
        use yserver_core::core_loop::{
            HostInputEvent, InputOrigin, pointer_fanout::pointer_event_fanout_to_state,
        };

        let rejected_origin = match &ev {
            HostInputEvent::Key(raw) | HostInputEvent::KeyRepeat(raw)
                if !yserver_core::core_loop::key_fanout::keyboard_origin_is_live(
                    state, raw.origin,
                ) =>
            {
                Some(raw.origin)
            }
            HostInputEvent::PointerMotion { origin, .. }
            | HostInputEvent::PointerButton { origin, .. }
            | HostInputEvent::PointerScrollStop { origin, .. }
                if !yserver_core::core_loop::pointer_fanout::pointer_origin_is_live(
                    state, *origin,
                ) =>
            {
                Some(*origin)
            }
            _ => None,
        };
        if let Some(origin) = rejected_origin {
            log::trace!("dropping input from unknown, disabled, or invalid origin {origin:?}");
            return;
        }

        self.synchronize_floating_keyboard_states(state);

        let pointer_button_origin = match &ev {
            HostInputEvent::PointerButton { origin, .. } => Some(*origin),
            _ => None,
        };
        let mut floating_position_update = None;
        match ev {
            HostInputEvent::PointerMotion {
                origin,
                x,
                y,
                relative,
                dx,
                dy,
                motion_delta,
                ..
            } => {
                if let Some(device_id) =
                    yserver_core::core_loop::pointer_fanout::floating_pointer_device_id(
                        state, origin,
                    )
                {
                    // XTestFakeInput sends relative MotionNotify valuators to
                    // GetPointerEvents without POINTER_ABSOLUTE
                    // (Xext/xtest.c:415-419); Xorg's GetPointerEvents applies
                    // those deltas to that device's current sprite position
                    // (dix/getevents.c:1308-1310,1438-1440). Keep the same
                    // per-device accumulator for floating XTEST and physical
                    // slaves.
                    let position = if relative {
                        let current = state
                            .floating_pointer_positions
                            .get(&device_id)
                            .copied()
                            .unwrap_or_else(|| {
                                (
                                    f32::from(state.pointer_root.0),
                                    f32::from(state.pointer_root.1),
                                )
                            });
                        let delta = motion_delta.unwrap_or([f64::from(dx), f64::from(dy)]);
                        (
                            (current.0 + delta[0] as f32)
                                .clamp(0.0, self.platform.fb_w.saturating_sub(1) as f32),
                            (current.1 + delta[1] as f32)
                                .clamp(0.0, self.platform.fb_h.saturating_sub(1) as f32),
                        )
                    } else {
                        (x as f32, y as f32)
                    };
                    if relative {
                        floating_position_update = Some((device_id, position));
                    }
                    #[allow(clippy::cast_possible_truncation)]
                    let (root_x, root_y) = (position.0 as i16, position.1 as i16);
                    let buttons = state
                        .xi_devices
                        .device(device_id)
                        .map_or(0, |device| (device.buttons_down & 0x001f) << 8);
                    self.emit_floating_pointer_event_at(
                        state,
                        origin,
                        PointerEventKind::MotionNotify,
                        0,
                        root_x,
                        root_y,
                        self.serialize_modifiers() | buttons,
                        dx,
                        dy,
                    );
                } else {
                    self.core.button_mask = (state.buttons_down & 0x001f) << 8;
                    if relative && matches!(origin, InputOrigin::Physical(_)) {
                        let delta = motion_delta.unwrap_or([f64::from(dx), f64::from(dy)]);
                        self.process_pointer_absolute(
                            state,
                            self.core.cursor_x + delta[0] as f32,
                            self.core.cursor_y + delta[1] as f32,
                            true,
                            dx,
                            dy,
                            origin,
                        );
                    } else {
                        self.process_pointer_absolute(
                            state, x as f32, y as f32, relative, dx, dy, origin,
                        );
                    }
                }
            }
            HostInputEvent::PointerButton {
                origin,
                button,
                pressed,
                ..
            } => {
                if let Some(device_id) =
                    yserver_core::core_loop::pointer_fanout::floating_pointer_device_id(
                        state, origin,
                    )
                {
                    let detail = match button {
                        0x110 => 1,
                        0x111 => 3,
                        0x112 => 2,
                        0x113 => 8,
                        0x114 => 9,
                        0x115 => 10, // BTN_FORWARD -> X 10 via btn_linux2xorg (xf86-input-libinput/src/xf86libinput.c:253-272)
                        0x180 => 4,
                        0x181 => 5,
                        0x182 => 6,
                        0x183 => 7,
                        _ => return,
                    };
                    let held = state
                        .xi_devices
                        .device(device_id)
                        .map_or(0, |device| device.buttons_down);
                    let button_bit = if (1..=5).contains(&detail) {
                        1u16 << (detail - 1)
                    } else {
                        0
                    };
                    let (root_x, root_y) = state
                        .floating_pointer_positions
                        .get(&device_id)
                        .copied()
                        .unwrap_or_else(|| {
                            (
                                f32::from(state.pointer_root.0),
                                f32::from(state.pointer_root.1),
                            )
                        });
                    #[allow(clippy::cast_possible_truncation)]
                    let (root_x, root_y) = (root_x as i16, root_y as i16);
                    let state_mask = (self.serialize_modifiers() | ((held & 0x001f) << 8))
                        | if pressed { 0 } else { button_bit << 8 };
                    self.emit_floating_pointer_event_at(
                        state,
                        origin,
                        if pressed {
                            PointerEventKind::ButtonPress
                        } else {
                            PointerEventKind::ButtonRelease
                        },
                        detail,
                        root_x,
                        root_y,
                        state_mask,
                        0,
                        0,
                    );
                } else {
                    self.core.button_mask = (state.buttons_down & 0x001f) << 8;
                    self.process_pointer_button(u32::from(button), pressed, state, origin);
                }
            }
            HostInputEvent::PointerScrollStop { origin, .. } => {
                if let Some(device_id) =
                    yserver_core::core_loop::pointer_fanout::floating_pointer_device_id(
                        state, origin,
                    )
                {
                    let (root_x, root_y) = state
                        .floating_pointer_positions
                        .get(&device_id)
                        .copied()
                        .unwrap_or_else(|| {
                            (
                                f32::from(state.pointer_root.0),
                                f32::from(state.pointer_root.1),
                            )
                        });
                    #[allow(clippy::cast_possible_truncation)]
                    let (root_x, root_y) = (root_x as i16, root_y as i16);
                    let buttons = state
                        .xi_devices
                        .device(device_id)
                        .map_or(0, |device| (device.buttons_down & 0x001f) << 8);
                    let previous = (self.core.cursor_x, self.core.cursor_y);
                    self.core.cursor_x = f32::from(root_x);
                    self.core.cursor_y = f32::from(root_y);
                    let host_xid = self.resource_pointer_host_xid(state);
                    let xid_map = self.core.xid_map.clone();
                    yserver_core::core_loop::pointer_fanout::emit_scroll_stop_to_state(
                        state,
                        &xid_map,
                        origin,
                        host_xid,
                        root_x,
                        root_y,
                        self.serialize_modifiers() | buttons,
                        crate::clock::server_time_ms(),
                    );
                    self.core.cursor_x = previous.0;
                    self.core.cursor_y = previous.1;
                    return;
                }
                // Fingers lifted from a two-finger scroll. Emit a delta-0 XI2
                // scroll motion (→ GDK `scroll.is_stop`) so Firefox's
                // SwipeTracker commits a horizontal history-swipe (bug
                // 1539730). Direct XI2 emission — nothing queued to
                // pending_pointer_events — so return before the drain below.
                let host_xid = self.resource_pointer_host_xid(state);
                let state_mask = self.serialize_modifiers() | self.core.button_mask;
                let xid_map = self.core.xid_map.clone();
                yserver_core::core_loop::pointer_fanout::emit_scroll_stop_to_state(
                    state,
                    &xid_map,
                    origin,
                    host_xid,
                    self.core.cursor_x as i16,
                    self.core.cursor_y as i16,
                    state_mask,
                    crate::clock::server_time_ms(),
                );
                return;
            }
            HostInputEvent::Key(raw) => {
                self.handle_host_key(state, raw, false, true);
                return;
            }
            HostInputEvent::KeyRepeat(raw) => {
                self.handle_host_key(state, raw, true, false);
                return;
            }
            HostInputEvent::DeviceAdded(info) => {
                log::info!(
                    "xi-device: added source={} {:?} node={} touchpad={}",
                    info.source_id.0,
                    info.name,
                    info.device_node,
                    info.is_touchpad,
                );
                let continuing = state.xi_devices.source(info.source_id).is_some();
                if !continuing {
                    let mut disabled_info = info.clone();
                    disabled_info.enabled = false;
                    let added_ids = state.xi_register_source(&disabled_info);
                    if added_ids.is_empty() {
                        // A retained source with no XI facet still participates
                        // in guarded internal input delivery.
                        state.xi_refresh_source(&info);
                    }
                    for id in added_ids {
                        let _dropped = yserver_core::xinput::hotplug::emit_xi1_device_presence(
                            state,
                            id,
                            yserver_core::xinput::hotplug::DevicePresenceChange::Added,
                        );
                        let _dropped = yserver_core::xinput::hotplug::emit_xi_hierarchy_changed(
                            state,
                            yserver_core::xinput::hotplug::XiHierarchyStep::SlaveAdded,
                            id,
                            None,
                            None,
                        );
                        if info.enabled && state.xi_set_facet_session_enabled(id, true) {
                            yserver_core::xinput::hotplug::publish_facet_enabled(state, id);
                        }
                    }
                } else {
                    let ids = state.xi_refresh_source(&info);
                    for id in ids {
                        if info.enabled && state.xi_set_facet_session_enabled(id, true) {
                            yserver_core::xinput::hotplug::publish_facet_enabled(state, id);
                        }
                    }
                }
                return;
            }
            HostInputEvent::DeviceResumed(info) => {
                log::info!(
                    "xi-device: resumed source={} {:?} node={} touchpad={}",
                    info.source_id.0,
                    info.name,
                    info.device_node,
                    info.is_touchpad,
                );
                let enabled_ids = state.xi_refresh_source(&info);
                for id in enabled_ids {
                    if info.enabled && state.xi_set_facet_session_enabled(id, true) {
                        yserver_core::xinput::hotplug::publish_facet_enabled(state, id);
                    }
                }
                return;
            }
            HostInputEvent::DeviceSuspended { source_id } => {
                log::info!("xi-device: suspended source={}", source_id.0);
                if state
                    .xi_devices
                    .source(source_id)
                    .is_some_and(|info| info.enabled)
                {
                    self.release_device_state(state, source_id);
                }
                // release_device_state publishes Disabled synchronously;
                // a delayed duplicate suspend has no transition to publish.
                return;
            }
            HostInputEvent::DeviceRemoved { source_id } => {
                log::info!("xi-device: removed source={}", source_id.0);
                if !self.release_device_holds(state, source_id) {
                    return;
                }
                if state.xi_devices.source(source_id).is_none() {
                    return;
                }
                yserver_core::core_loop::pointer_fanout::xi_cleanup_source(state, self, source_id);
                let removed_ids: Vec<u16> = [
                    state
                        .xi_devices
                        .facet(source_id, yserver_core::xinput::XiFacetKind::Keyboard),
                    state
                        .xi_devices
                        .facet(source_id, yserver_core::xinput::XiFacetKind::PointerTouch),
                ]
                .into_iter()
                .flatten()
                .collect();
                if removed_ids.is_empty() {
                    state.xi_unregister_source(source_id);
                }
                for id in removed_ids {
                    Self::publish_disabled_xi_facet(state, id);
                    let Some(removed) = state.xi_unregister_facet(id) else {
                        continue;
                    };
                    let _dropped = yserver_core::xinput::hotplug::emit_xi1_device_presence(
                        state,
                        id,
                        yserver_core::xinput::hotplug::DevicePresenceChange::Removed,
                    );
                    let _dropped = yserver_core::xinput::hotplug::emit_xi_hierarchy_changed(
                        state,
                        yserver_core::xinput::hotplug::XiHierarchyStep::SlaveRemoved,
                        id,
                        Some(&removed),
                        None,
                    );
                }
                self.synchronize_floating_keyboard_states(state);
                return;
            }
        }

        // Drain pointer events queued by the process_pointer_* call.
        // `process_pointer_absolute` builds the queued events before this
        // fanout runs, so keep its absolute-motion barrier policy active
        // until those events have actually reached the pointer fanout.
        let previous_barrier_bypass = state.barrier_bypass;
        let pending_motion_barrier_bypass =
            std::mem::take(&mut self.core.pending_motion_barrier_bypass);
        state.barrier_bypass = previous_barrier_bypass || pending_motion_barrier_bypass;
        let pending = std::mem::take(&mut self.core.pending_pointer_events);
        let xid_map = self.core.xid_map.clone();
        for ev in pending {
            let _dropped = pointer_event_fanout_to_state(state, self, &xid_map, ev, true, false);
        }
        state.barrier_bypass = previous_barrier_bypass;
        if let Some((device_id, position)) = floating_position_update
            && state.floating_pointer_positions.contains_key(&device_id)
        {
            // The fanout caches the integer event coordinates. Restore the
            // KMS-integrated position so successive subpixel motions retain
            // their fractional remainder while the slave is detached.
            state.floating_pointer_positions.insert(device_id, position);
        }
        if pointer_button_origin.is_some_and(|origin| {
            yserver_core::core_loop::pointer_fanout::pointer_origin_is_live(state, origin)
        }) {
            // The fanout applies per-slave duplicate and master aggregation
            // guards. Mirror its resulting master state back to KMS so later
            // motion and button event masks stay authoritative.
            self.core.button_mask = (state.buttons_down & 0x001f) << 8;
        }
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
        if let Some(executor) = self.crtc_config_probe_executor.as_mut() {
            executor.set_core_sender(sender.clone_handle());
        }
        self.input_sender = Some(sender);
        if !self.ready_crtc_config_announcements.is_empty() {
            self.wake_crtc_config_ready();
        }
    }

    fn request_vt_switch(&mut self, vt: u32) {
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

    fn begin_vt_release(&mut self) -> bool {
        log::info!("kms: VT release — begin (pause input)");
        self.pause_input_thread()
    }

    fn finish_vt_release(
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
        if let Some(deadline) = self.hotplug_rescan_deadline
            && std::time::Instant::now() >= deadline
        {
            self.hotplug_rescan_deadline = None;
            self.run_display_rescan(state);
        }
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
        use yserver_core::core_loop::{HostInputEvent, InputOrigin};

        // Physical cleanup owns published and unpublished source state,
        // including XTEST holds explicitly aimed at one of those facets.
        // Unlike VT/removal cleanup, this leaves every source's enabled bit
        // untouched so inventory replay can reproduce its current status.
        let sources = old_state.xi_devices.source_ids();
        for source_id in sources {
            let _retained = self.release_device_holds(old_state, source_id);
        }

        // Release any remaining virtual/master keyboard holds through the
        // ordinary guarded transition path. Physical holds above have
        // already disappeared, so duplicate map entries become no-ops.
        let mut held_keys: Vec<(u16, u8, InputOrigin)> = old_state
            .key_down_by_device
            .iter()
            .flat_map(|(&device_id, keys)| {
                keys.iter()
                    .map(move |(&keycode, &origin)| (device_id, keycode, origin))
            })
            .collect();
        held_keys.sort_by_key(|(device_id, keycode, _)| (*device_id, *keycode));
        let mut released_keys = HashSet::new();
        for (_, keycode, origin) in held_keys {
            if !released_keys.insert((origin, keycode)) {
                continue;
            }
            self.handle_host_key(
                old_state,
                HostKeyEvent {
                    origin,
                    pressed: false,
                    keycode,
                    time: crate::clock::server_time_ms(),
                    root_x: self.core.cursor_x as i16,
                    root_y: self.core.cursor_y as i16,
                    event_x: self.core.cursor_x as i16,
                    event_y: self.core.cursor_y as i16,
                    state: 0,
                },
                false,
                false,
            );
        }

        // Then drain any pointer device holds still present. In normal input
        // this is principally virtual XTEST 4/5; physical holds were retired
        // above while their source/facet identity was still available.
        let mut held_pointers: Vec<(u16, InputOrigin, u16)> = old_state
            .xi_devices
            .iter()
            .filter(|device| device.buttons_down != 0)
            .map(|device| {
                let origin = device
                    .source_id
                    .map(InputOrigin::Physical)
                    .unwrap_or(InputOrigin::XTest(device.id));
                (device.id, origin, device.buttons_down)
            })
            .collect();
        held_pointers.sort_by_key(|(device_id, _, _)| *device_id);
        for (_, origin, buttons) in held_pointers {
            for detail in 1u16..=10 {
                if buttons & (1 << (detail - 1)) == 0 {
                    continue;
                }
                let button = match detail {
                    1 => 0x110,
                    2 => 0x112,
                    3 => 0x111,
                    4 => 0x180,
                    5 => 0x181,
                    6 => 0x182,
                    7 => 0x183,
                    8 => 0x113,
                    9 => 0x114,
                    10 => 0x115, // BTN_FORWARD -> X 10 via btn_linux2xorg (xf86-input-libinput/src/xf86libinput.c:253-272)
                    _ => unreachable!("button release loop is bounded to 1..=10"),
                };
                Backend::on_host_input(
                    self,
                    old_state,
                    HostInputEvent::PointerButton {
                        origin,
                        button,
                        pressed: false,
                        time: crate::clock::server_time_ms(),
                    },
                );
            }
        }

        // A generation boundary owns no old-session replay, XI grabs, or
        // KMS-queued pointer events. Do this only after guarded releases have
        // had the old registry and client windows available for fanout.
        old_state.key_repeats.clear();
        old_state.sync_pending.clear();
        old_state.playing_sync_events = false;
        old_state.xi1_frozen.clear();
        old_state.xi1_active_grabs.clear();
        old_state.xi1_passive_grabs.clear();
        old_state.xi1_device_focus.clear();
        old_state.xi1_device_input_state.clear();
        old_state.xi2_pointer_grabs.clear();
        old_state.xi2_keyboard_grabs.clear();
        old_state.xi2_detached_masters.clear();
        old_state.floating_pointer_positions.clear();
        old_state.key_grabs.clear();
        old_state.button_grabs.clear();
        old_state.active_pointer_grab = None;
        old_state.active_keyboard_grab = None;
        old_state.key_down_by_device.clear();
        old_state.unpublished_keyboard_keys_down.clear();
        old_state.unpublished_pointer_buttons_down.clear();
        old_state.keys_down.fill(0);
        old_state.buttons_down = 0;
        let old_device_ids: Vec<u16> = old_state
            .xi_devices
            .iter()
            .map(|device| device.id)
            .collect();
        for device in old_state.xi_devices.iter_mut() {
            device.buttons_down = 0;
        }
        for device_id in old_device_ids {
            old_state.xi_clear_last_slave(device_id);
        }

        self.core.pending_pointer_events.clear();
        self.core.pending_motion_barrier_bypass = false;
        self.core.button_mask = 0;
        self.core.down_keys.clear();
        self.floating_keyboard_states.clear();
        self.core.reset_keyboard_session();
        let _cursor_result = Backend::set_grab_cursor(self, None, None);
        self.sync_keyboard_leds();
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

    // ── Other extensions ────────────────────────────────────────

    fn xkb_proxy(
        &mut self,
        _origin: Option<OriginContext>,
        minor: u8,
        body: &[u8],
        intern_atom: &mut dyn FnMut(&str) -> u32,
    ) -> io::Result<Option<Vec<u8>>> {
        // Mirror v1's xkb_proxy verbatim — pure protocol
        // bookkeeping using the shared `KmsCore.xkb_keymap`.
        // Without this, Xlib clients abort at the XKEYBOARD
        // UseExtension handshake, so no real-app smoke is
        // possible. The behaviour-level fix is identical to v1
        // (reply minors get bodies, void minors return None).
        use crate::kms::{xkb as xkb_replies, xkb_desc::reply};
        let desc = &self.core.xkb_desc;
        let major = self.xkb_opcode().unwrap_or(0);
        let or_error = |r: Result<Vec<u8>, reply::XkbError>| {
            r.unwrap_or_else(|e| reply::error_packet(e, major, minor))
        };
        let reply = match minor {
            0 => Some(xkb_replies::reply_use_extension()),
            4 => Some(xkb_replies::reply_get_state(
                &self.core.xkb_state.0,
                self.effective_locked_group(),
            )),
            6 => Some(reply::reply_get_controls(desc)),
            8 => Some(or_error(reply::reply_get_map(desc, body))),
            10 => Some(or_error(reply::reply_get_compat_map(desc, body))),
            // minor 13 is GetIndicatorMap (clients send it 8×); minor 22 is
            // ListComponents, which clients don't send — a minimal reply is
            // safe there, an IndicatorMap-shaped reply is wrong.
            13 => Some(reply::reply_get_indicator_map(desc, body)),
            15 => Some(reply::reply_get_named_indicator(
                desc,
                desc.indicators_lit(&self.core.xkb_state.0),
                body,
                intern_atom,
            )),
            17 => Some(or_error(reply::reply_get_names(desc, body, intern_atom))),
            21 => Some(xkb_replies::reply_per_client_flags(body)),
            22 => Some(xkb_replies::reply_minimal(22)),
            24 => Some(xkb_replies::reply_get_device_info()),
            12 | 19 | 23 | 101 => Some(xkb_replies::reply_minimal(minor)),
            1 | 3 | 5 | 7 | 9 | 11 | 14 | 16 | 18 | 20 | 25 => None,
            _ => {
                log::debug!("render xkb: unknown minor {minor}, no reply sent");
                None
            }
        };
        Ok(reply)
    }

    fn xkb_set(
        &mut self,
        minor: u8,
        body: &[u8],
        client_is_ancient: bool,
        atom_name: &dyn Fn(u32) -> Option<String>,
    ) -> Option<yserver_core::backend::XkbSetOutcome> {
        match minor {
            9 => Some(self.xkb_set_map(body, client_is_ancient)),
            11 => Some(self.xkb_set_compat_map(body)),
            14 => Some(self.xkb_set_indicator_map(body)),
            18 => Some(self.xkb_set_names(body, atom_name)),
            20 => Some(self.xkb_set_geometry(body, atom_name)),
            _ => None,
        }
    }

    fn xkb_get_kbd_by_name(
        &mut self,
        body: &[u8],
        intern_atom: &mut dyn FnMut(&str) -> u32,
    ) -> Option<(Vec<u8>, Option<yserver_core::backend::XkbNewKeyboardInfo>)> {
        // xkbGetKbdByNameReq body (after the 4-byte XKB request header the core
        // loop already stripped): deviceSpec(2) need(2) want(2) load(1) pad(1),
        // then CARD8-length-prefixed component strings in the order
        // keymap, keycodes, types, compat, symbols, geometry
        // (xkb.c ProcXkbGetKbdByName, GetComponentSpec). We need `symbols`
        // (index 4) plus the want/need masks and the load flag.
        if body.len() < 8 {
            return None;
        }
        let need = u16::from_le_bytes([body[2], body[3]]);
        let want = u16::from_le_bytes([body[4], body[5]]);
        let load = body[6] != 0;

        // Walk the 6 length-prefixed component strings; capture #4 (symbols).
        let mut off = 8usize;
        let mut symbols: Option<&[u8]> = None;
        for idx in 0..6 {
            if off >= body.len() {
                break;
            }
            let len = usize::from(body[off]);
            off += 1;
            let end = off.saturating_add(len);
            if end > body.len() {
                break;
            }
            if idx == 4 {
                symbols = Some(&body[off..end]);
            }
            off = end;
        }
        let symbols = std::str::from_utf8(symbols.unwrap_or(&[])).ok()?;

        // Capture the OLD keycode range before any load, for the NKN.
        let (old_min, old_max) = (
            self.core.xkb_desc.min_key_code,
            self.core.xkb_desc.max_key_code,
        );

        // Load the requested multi-group keymap when the client asked
        // (Cinnamon always sends load=1 for a runtime layout-add).
        let load_result = if load {
            self.load_keymap_by_components(symbols)
        } else {
            // No load: report against the current keymap as "located".
            KeymapLoad::Loaded {
                min_keycode: old_min,
                max_keycode: old_max,
                changed: false,
            }
        };
        let loaded = matches!(load_result, KeymapLoad::Loaded { .. });

        // Build the reply from the now-current keymap.
        let reply = crate::kms::xkb::reply_get_kbd_by_name(
            &self.core.xkb_desc,
            want,
            need,
            loaded,
            intern_atom,
        );

        // Broadcast a NewKeyboardNotify only when a load actually changed the
        // map (a no-op reload shouldn't churn every client's keymap). The
        // captured Xorg reply uses changed = Keycodes|Geometry (0x0003).
        let notify = match load_result {
            KeymapLoad::Loaded {
                min_keycode,
                max_keycode,
                changed: true,
            } => Some(yserver_core::backend::XkbNewKeyboardInfo {
                min_keycode,
                max_keycode,
                old_min_keycode: old_min,
                old_max_keycode: old_max,
                changed: 0x0003, // XkbNKN_KeycodesMask | XkbNKN_GeometryMask
            }),
            _ => None,
        };

        Some((reply, notify))
    }

    fn set_keymap_rmlvo(
        &mut self,
        rules: &str,
        model: &str,
        layout: &str,
        variant: &str,
        options: Option<&str>,
    ) -> Option<(u8, u8)> {
        let range = self.core.recompile_keymap(&crate::kms::core::XkbRmlvo {
            rules: rules.to_string(),
            model: model.to_string(),
            layout: layout.to_string(),
            variant: variant.to_string(),
            options: options.map(str::to_string),
        })?;
        // The reload starts from a fresh state (locks released).
        self.sync_keyboard_leds();
        Some(range)
    }

    fn current_xkb_rules_names(&self) -> Option<[String; 5]> {
        let r = &self.core.xkb_rmlvo;
        Some([
            r.rules.clone(),
            r.model.clone(),
            r.layout.clone(),
            r.variant.clone(),
            r.options.clone().unwrap_or_default(),
        ])
    }

    fn get_active_cursor_image(&self) -> Option<yserver_core::backend::ActiveCursorImage> {
        Self::backend_cursor_get_active_cursor_image(self)
    }

    fn load_keymap_by_components(&mut self, symbols: &str) -> KeymapLoad {
        let Some(parsed) = crate::kms::xkb::parse_symbols_layouts(symbols) else {
            return KeymapLoad::Failed; // fail-closed: keep current keymap
        };
        let rmlvo = crate::kms::core::XkbRmlvo {
            rules: "evdev".to_string(),
            model: "pc105".to_string(),
            layout: parsed.layouts,
            variant: parsed.variants,
            options: if parsed.options.is_empty() {
                None
            } else {
                Some(parsed.options)
            },
        };
        // Already active and not edited since? Still a successful load, but
        // changed=false (Xorg reports loaded=TRUE even on an unchanged
        // reload). An edited keymap reloads (Xorg always reloads).
        if self.core.keymap_is_pristine(&rmlvo) {
            let (min, max) = (
                self.core.xkb_desc.min_key_code,
                self.core.xkb_desc.max_key_code,
            );
            return KeymapLoad::Loaded {
                min_keycode: min,
                max_keycode: max,
                changed: false,
            };
        }
        match self.core.recompile_keymap(&rmlvo) {
            Some((min, max)) => {
                // New map -> group 0 active until the next LatchLockState.
                self.core.locked_group = 0;
                // Fresh state: locks released.
                self.sync_keyboard_leds();
                KeymapLoad::Loaded {
                    min_keycode: min,
                    max_keycode: max,
                    changed: true,
                }
            }
            // A pristine match was ruled out above, so None here means a
            // compile failure -> keep the current keymap.
            None => KeymapLoad::Failed,
        }
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
        // Route through the absolute-motion input path: updates the
        // tracked cursor (and HW cursor plane) and fans out the
        // motion/crossing events WarpPointer is specified to generate
        // ("as if the user had instantaneously moved the pointer").
        self.on_host_input(
            state,
            yserver_core::core_loop::HostInputEvent::PointerMotion {
                origin: yserver_core::core_loop::InputOrigin::NestedHost,
                x,
                y,
                time: 0,
                relative: false,
                dx: 0,
                dy: 0,
                motion_delta: None,
            },
        );
    }

    fn windows_restructured(&mut self, state: &mut ServerState) {
        Self::backend_windows_windows_restructured(self, state)
    }

    fn query_pointer(&mut self, _origin: Option<OriginContext>) -> io::Result<PointerPosition> {
        // Return the current core-tracked cursor position. No
        // window-focus lookup — Stage 1b doesn't model focus.
        //
        // The mask is a full X11 KeyButMask: live keyboard modifiers
        // (xkb state, low byte) | held buttons (0x100+). Xorg's
        // QueryPointer/XIQueryPointer report the paired keyboard's
        // modifier state here; pre-fix only buttons were included, so
        // cinnamon's alt-tab switcher (global.get_pointer() via
        // XIQueryPointer) read "Alt not held" mid-alt-tab and
        // instantly cancelled the popup.
        Ok(PointerPosition {
            same_screen: true,
            #[allow(clippy::cast_possible_truncation)]
            win_x: self.core.cursor_x as i16,
            #[allow(clippy::cast_possible_truncation)]
            win_y: self.core.cursor_y as i16,
            mask: self.core.button_mask | self.serialize_modifiers(),
        })
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
        // Xorg's XkbGetCoreMap layout (one width for the whole map, §12.4 group order).
        let map = self.core.xkb_desc.core_map();
        Ok((map.width, map.rows(first_keycode, count)))
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
