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
        self.crtc_key_by_id
            .get(&crtc)
            .map_or(0, |output_key| self.nominal_gamma_size(output_key))
    }

    fn set_crtc_gamma(
        &mut self,
        crtc: u32,
        red: &[u16],
        green: &[u16],
        blue: &[u16],
    ) -> io::Result<()> {
        let output_key = self.crtc_key_by_id.get(&crtc).cloned().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("unknown RANDR CRTC 0x{crtc:x}"),
            )
        })?;
        let expected = usize::from(self.crtc_gamma_size(crtc));
        if red.len() != expected || green.len() != expected || blue.len() != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "CRTC 0x{crtc:x} ({output_key:?}): gamma length mismatch (expected {expected}, got {}/{}/{})",
                    red.len(),
                    green.len(),
                    blue.len(),
                ),
            ));
        }
        self.gamma_luts.borrow_mut().insert(
            output_key.clone(),
            GammaLut {
                red: red.to_vec(),
                green: green.to_vec(),
                blue: blue.to_vec(),
            },
        );
        self.apply_gamma_to_live_output(&output_key)
    }

    fn get_crtc_gamma(&self, crtc: u32) -> (Vec<u16>, Vec<u16>, Vec<u16>) {
        let Some(output_key) = self.crtc_key_by_id.get(&crtc) else {
            return (Vec::new(), Vec::new(), Vec::new());
        };
        let lut = match self.live_crtc_and_gamma_size(output_key) {
            Ok(Some((_, _, size))) => self.cached_gamma_for_current_size(output_key, size),
            Ok(None) => self.cached_gamma(output_key),
            Err(e) => {
                log::warn!("kms gamma: {output_key:?} get gamma size failed: {e}");
                self.cached_gamma(output_key)
            }
        };
        (lut.red, lut.green, lut.blue)
    }

    fn render_format_for_ynest_id(&self, ynest_fmt: u32) -> Option<u32> {
        if ynest_fmt == 0 {
            None
        } else {
            Some(ynest_fmt)
        }
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
        let completions = self.platform.drain_scanout_render_completions();
        if !self.scanout_allowed() {
            // Draining avoids a readable aggregator spinning the core loop.
            // The suspend/topology lifecycle subsequently waits A, cancels
            // the ledger, and drains both pool devices before any allocation
            // can be reset or dropped.
            log::debug!(
                "render copied scanout: discarded {} completion(s) while scanout is inactive",
                completions.len(),
            );
            return;
        }
        for completion in completions {
            if self.platform.renderer_failed {
                break;
            }
            if !self
                .scene
                .handle_scanout_render_completion(completion, &mut self.platform)
            {
                self.telemetry.record_missed_pageflip();
            }
            if self.platform.renderer_failed {
                break;
            }
        }
    }

    fn on_page_flip_ready(&mut self, _state: &mut ServerState, drm_fd: std::os::fd::RawFd) {
        // Gate: when not Active we have no DRM master; page-flip events
        // are drained (so the fd doesn't stay readable) but no resubmit
        // or flush_submit_group runs. In Direct mode this is always false
        // → no behaviour change.
        if !self.scanout_allowed() {
            // Discard page-flip retires (no DRM master → don't touch scanout
            // state) but STILL run the sequence handler so the armed-target
            // map clears — leaving a stuck entry across suspend is exactly
            // the permanent-stall failure mode this guards against.
            if let Ok((_flips, sequences)) = self.platform.drain_page_flip_events(drm_fd) {
                for seq in sequences {
                    self.on_crtc_sequence_event(
                        seq.device_key,
                        seq.user_data,
                        seq.time_ns,
                        seq.sequence,
                    );
                }
            }
            log::debug!("render on_page_flip_ready: skipped (seat not Active)");
            return;
        }
        let (flipped, sequences) = match self.platform.drain_page_flip_events(drm_fd) {
            Ok(pair) => pair,
            Err(e) => {
                log::warn!("render: drain_page_flip_events failed: {e}");
                return;
            }
        };
        for (output_idx, clock) in flipped {
            let direct_retired = self.retire_direct_output(output_idx, clock);
            let scene_retired = !direct_retired
                && self.scene.handle_page_flip_complete(
                    output_idx,
                    &mut self.store,
                    &mut self.platform,
                );
            if direct_retired || scene_retired {
                self.telemetry.record_frame_present();
            }
            // Retry only the cursor state owned by the card whose output just
            // retired. A page flip on card A must not consume card B's EBUSY
            // slot or EINVAL backoff.
            match self
                .platform
                .cursor_plane_drain_pending_move_for_output(output_idx)
            {
                Ok(outcome) => {
                    self.handle_cursor_move_outcome(outcome);
                }
                Err(e) => log::debug!("render cursor drain on page-flip retire: {e}"),
            }
        }
        // Idle vblank arming: clear the arm + advance the Present clock for
        // each CRTC sequence the kernel delivered. The run loop reads the
        // updated `(msc, ust)` via `present_get_ust_msc` and fires parked
        // NotifyMSC, then re-arms if any remain.
        for seq in sequences {
            self.on_crtc_sequence_event(seq.device_key, seq.user_data, seq.time_ns, seq.sequence);
        }
        // Sweep retired engine submits + retired drawables now
        // that their fences may have signaled.
        self.engine.poll_retired(&self.platform);
        self.poll_pending_retire_with_invalidate();
        self.sync_descriptor_pool_telemetry();
        // Phase A T7: pageflip retire is a frame boundary — close
        // any open render batch FIRST so its CBs land in the group
        // under the same ticket that the subsequent flush will
        // consume. Then flush the SubmitGroup so an idle next tick
        // (no scene_structure_dirty) does not leave paint CBs
        // buffered until the next compose. Drive through the engine
        // wrapper so parked pending_group_ops commit to `submitted`
        // atomically.
        if let Err(e) = self.engine.flush_render_batch(
            &mut self.store,
            &mut self.platform,
            crate::kms::render::engine::RenderFlushReason::Present,
        ) {
            log::warn!("render on_page_flip_ready: flush_render_batch failed: {e:?}");
        }
        if let Err(e) = self.engine.flush_submit_group(
            &mut self.store,
            &mut self.platform,
            crate::kms::render::submit_group::FlushReason::PageflipRetire,
        ) {
            log::warn!("render on_page_flip_ready: flush_submit_group failed: {e:?}");
        }
    }

    fn before_block(&mut self) {
        // BlockHandler analog (cf. Xorg glamor_block_handler → glamor_flush):
        // every dispatch-loop iteration, just before the core loop blocks,
        // reap render-op resources whose fences have signaled. This is the
        // reclaim half of `on_page_flip_ready` (the scanout / compose / flush
        // half stays page-flip-driven), lifted onto the dispatch loop so it
        // runs even when no page-flip occurs.
        //
        // Without this, the engine `submitted` queue (one per-op command
        // buffer + any displaced images each) is drained ONLY on page-flip.
        // While the display is dark (DPMS-off / monitor standby / VT-away)
        // page-flips stop, but clients keep submitting render ops, so the
        // queue grows without bound until amdgpu can't allocate command-
        // submission memory and the device is lost
        // (project_reclamation_starvation_leak). poll_retired only frees
        // ops whose fence has signaled and is a cheap no-op on an empty
        // queue, so running it every iteration costs nothing at idle.
        self.engine.poll_retired(&self.platform);
        self.poll_pending_retire_with_invalidate();
        // #196: destroy pixmap-pool entries idle past the eviction age, so
        // a burst drains back down; `next_wakeup` wakes us for it.
        if let Some(pool) = self.platform.pixmap_pool.as_ref() {
            pool.trim_idle(std::time::Instant::now());
        }
        // Diagnostic: drive the 1Hz telemetry emit from here too,
        // publishing the live `submitted`-queue depth. maybe_emit()
        // self-gates to 1Hz and is a no-op below threshold, but running
        // it every dispatch iteration means the `render_telemetry:` line (and
        // the submit-trace flush) keep ticking even while the display is
        // dark — exactly when `submitted_queue_depth` is the number worth
        // watching (project_reclamation_starvation_leak). Without this,
        // the only other maybe_emit caller is on the compose path, which
        // is gated off when dark, so telemetry went silent in the window.
        self.telemetry.maybe_emit(self.engine.pending_count());
    }

    fn mark_dirty(&mut self) {
        // Wake the compositor without inventing full-output damage.
        // Paint paths already record per-drawable presentation
        // damage, and cursor motion is projected by build_scene.
        self.scene.wake_for_damage();
    }

    fn flush_before_damage_notify(&mut self) {
        // A compositor may sample a redirected backing after DamageNotify
        // or after retrieving coalesced damage via Subtract/FetchRegion.
        // Close all recording paths before publishing their producer fences.
        if let Err(error) = self.engine.flush_render_batch(
            &mut self.store,
            &mut self.platform,
            crate::kms::render::engine::RenderFlushReason::Other,
        ) {
            log::warn!("render damage boundary batch flush failed: {error:?}");
        }
        if let Err(error) = self.engine.close_open_frame(
            &mut self.store,
            &mut self.platform,
            crate::kms::render::frame_builder::CloseReason::DamageBoundary,
        ) {
            log::warn!("render DamageNotify submission boundary failed: {error:?}");
        }
        // Closing an already-closed frame does not drain command buffers
        // parked in the submit group (including the render batch above).
        if let Err(error) = self.engine.flush_submit_group(
            &mut self.store,
            &mut self.platform,
            crate::kms::render::submit_group::FlushReason::SyncBoundary,
        ) {
            log::warn!("render damage boundary submit flush failed: {error:?}");
        }
    }

    fn next_wakeup(&self) -> Option<std::time::Instant> {
        let now = std::time::Instant::now();
        let allow_kms_timers = self.scanout_allowed() && self.kms_outputs_active;
        let scene_deadline = if allow_kms_timers {
            if self.scene_wants_compose() {
                if self.scene.has_output_ready_for_submit() {
                    Some(now)
                } else {
                    self.scene.earliest_retry_deadline()
                }
            } else {
                self.scene.earliest_retry_deadline()
            }
        } else {
            None
        };
        let needs_present_poll = self.pending_present_batches.iter().any(|batch| {
            matches!(
                batch.wait,
                crate::kms::render::present_completion::PresentBatchWait::Poll
            )
        });
        let needs_source_wait_poll = self.pending_present_source_waits.values().any(|wait| {
            !wait.ready_reported
                && (wait.poll_timeline || wait.fds.iter().any(|fd| !fd.registered && !fd.ready))
        });
        let present_deadline = if needs_present_poll || needs_source_wait_poll {
            Some(now + std::time::Duration::from_millis(1))
        } else {
            None
        };
        let rescan_deadline = self
            .hotplug_rescan_deadline
            .map(|until| if now >= until { now } else { until });
        // A drawable freed while its GPU work was in flight waits in
        // `pending_retire` for `before_block` to see its fence signal; with
        // no other activity nothing would wake us to release it.
        let retire_deadline =
            (self.store.pending_retire_count() > 0).then(|| now + PENDING_RETIRE_POLL_INTERVAL);
        scene_deadline
            .into_iter()
            .chain(present_deadline)
            .chain(rescan_deadline)
            .chain(retire_deadline)
            // Not gated on `allow_kms_timers`: `maybe_composite` closes the
            // paint frame on its timeout while dark too (#177).
            .chain(self.engine.open_frame_timeout_deadline())
            .chain(
                self.platform
                    .pixmap_pool
                    .as_ref()
                    .and_then(|pool| pool.next_trim_deadline()),
            )
            .chain(
                allow_kms_timers
                    .then(|| self.cursor_anim_deadline())
                    .flatten(),
            )
            .min()
    }

    fn maybe_composite(&mut self) -> io::Result<()> {
        // GPU-reset recovery: once the renderer has observed a lost
        // device (a submit returned `ERROR_DEVICE_LOST` → `abort_flush`
        // latched `renderer_failed`), every subsequent tick used to
        // `return Ok(0)` forever — an infinite fence-poll spin that
        // leaves the screen corrupt until a hard reboot (never exits, so
        // the display manager can't respawn either). A card reset is
        // unrecoverable in-process here, so instead request a clean
        // shutdown: the RAII console/DRM-master guards restore a usable
        // TTY on the way out (no GPU access, safe on a dead device), and
        // lightdm respawns us on a fresh device. Checked before the
        // gates below so we exit even while VT-away / DPMS-off. Sends
        // `Message::Shutdown`, which the core loop drains next iteration,
        // so this fires ~once rather than per-frame.
        if self.platform.renderer_failed {
            log::error!(
                "kms: renderer device lost (GPU reset) — requesting clean shutdown \
                 so the display manager can respawn on a fresh device"
            );
            self.request_exit();
            return Ok(());
        }
        // Service the paint-batch deadline ahead of every scanout gate below
        // (VT, DPMS, direct-scanout hold). Closing the frame records its CB
        // and submits it on the render queue — Vulkan only, no KMS commit and
        // no DRM master — and clients keep drawing while the display is dark
        // or held by direct scanout. Behind the VT/DPMS gates the frame stayed
        // open until the 1024-pin ceiling forced it shut, then freed
        // everything it pinned in one burst: a live-allocation/VRAM sawtooth
        // with the monitor off (#177). Behind the direct-scanout gate the
        // final gkrellm update stayed open until unrelated screen activity
        // closed it.
        if let Err(e) = self
            .engine
            .close_open_frame_if_timed_out(&mut self.store, &mut self.platform)
        {
            log::warn!("render maybe_composite: timeout close failed: {e:?}");
        }
        // VT-master gate: while a VT switch is in progress or the GPU is
        // handed to another session, every
        // atomic_commit returns `EACCES`. `composite_and_flip` has
        // the same gate at :3263; `maybe_composite` was missing it
        // and emitted a burst of "atomic commit failed for output …
        // Permission denied" WARNs across the VT-suspend window
        // (observed 2026-05-31 — 77 WARNs in 3 seconds on
        // `just startx` + VT switch under MATE).
        if !self.scanout_allowed() {
            self.drain_paint_submit_telemetry();
            return Ok(());
        }
        // DPMS gate: outputs are inactive (every CRTC has ACTIVE=0 +
        // MODE_ID=0 from disable_output). Submitting an atomic page-flip
        // commit against a disabled CRTC returns EINVAL. Without this
        // gate the core loop's per-iteration `backend.maybe_composite()`
        // call would loop a tight EINVAL storm while DPMS is Off (and
        // the `composite_and_flip` gate at :3196 wouldn't catch it —
        // maybe_composite is a separate scene.tick caller).
        // See project_einval_atomic_commit_storm_wedge memory entry.
        if !self.kms_outputs_active {
            self.drain_paint_submit_telemetry();
            return Ok(());
        }
        // Animated-cursor frame advance — after both gates above so
        // DPMS-off / VT-away never uploads (spec "DPMS / VT gating").
        self.tick_cursor_animation();
        if self.scanout_m2.active() {
            use crate::kms::render::scene::CursorPlaneMode;
            let cursor_mode = self.scene.cursor_mode();
            if self.platform.any_output_transformed() {
                self.request_direct_unflip("composite_tick_crtc_transform");
            } else if !matches!(cursor_mode, CursorPlaneMode::Hw)
                || !self.scene.root_overlay.is_empty()
            {
                let reason = match cursor_mode {
                    CursorPlaneMode::Mixed => "composite_tick_mixed_cursor",
                    CursorPlaneMode::Sw => "composite_tick_software_or_hidden_cursor",
                    CursorPlaneMode::Hw => "composite_tick_root_overlay",
                };
                self.request_direct_unflip(reason);
            }
            // Never race a composed commit against the all-output direct
            // transaction. A requested unflip starts on the first tick after
            // the direct transaction itself has fully retired.
            if self.scanout_m2.pending.is_some()
                || !self.scanout_m2.unflip_awaiting_outputs.is_empty()
                || (self.scanout_m2.hold_direct && !self.scanout_m2.unflip_requested)
            {
                self.drain_render_telemetry();
                self.telemetry.maybe_emit(self.engine.pending_count());
                return Ok(());
            }
            if self.scanout_m2.current.is_some() && self.scanout_m2.unflip_requested {
                if let Err(error) = self.submit_composed_unflip() {
                    log::error!(
                        "scanout_m2: synchronized composed unflip failed: {error}; degrading to per-output composed flips"
                    );
                    // The atomic transaction never replaced the planes: the
                    // kernel is still scanning the direct dma-buf, so the
                    // direct frame's pins must stay held. Arm the composed-flip
                    // retirement machinery and fall through into the per-output
                    // scene compose path below — the scene flips replace the
                    // planes this tick, and `retire_direct_output` releases the
                    // direct frame only after every output has retired on the
                    // composed framebuffer.
                    self.scanout_m2.unflip_awaiting_outputs =
                        (0..self.platform.outputs.len()).collect();
                    self.scanout_m2.degraded_composed_unflip = true;
                    self.scene.mark_scene_structure_dirty();
                } else {
                    // The atomic replacement itself is now pending on every
                    // CRTC. Do not fall through into per-output scene flips in
                    // this same tick: KMS correctly rejects those with EBUSY.
                    self.drain_render_telemetry();
                    self.telemetry.maybe_emit(self.engine.pending_count());
                    return Ok(());
                }
            }
        }
        // One main-loop tick = one frame_id. Submit events
        // recorded between calls share the surrounding tick's
        // id; the scene_compose event of this tick (if it
        // submits) also carries this id.
        self.telemetry.advance_frame();
        let can_submit_scene =
            self.scene_wants_compose() && self.scene.has_output_ready_for_submit();
        // #214 telemetry: did this tick's compose close submit paint, and
        // did the tick then compose anything?
        let mut legacy_close_submitted = false;
        if can_submit_scene {
            // Stage 5 Task 3 (render-composite generalization): flush
            // the render batch — scene.tick samples dst.
            self.drain_engine_present_batches();
            if let Err(e) = self.engine.flush_render_batch(
                &mut self.store,
                &mut self.platform,
                crate::kms::render::engine::RenderFlushReason::Present,
            ) {
                log::warn!("render maybe_composite: flush_render_batch failed: {e:?}");
            }
            // Phase B Invariant M3: close any open frame BEFORE legacy compose
            // records. compose samples drawable storage at record time
            // (scene.rs:1307), so the open frame's layout + ticket-touch overlays
            // must be committed before the compose CB lands. Retires at sub-phase
            // B.4 when compose itself ports into the frame builder.
            // NOTE: integration test for M3 lives in Task 23's mixed-sequence
            // smoke (frame_builder_mixed_sequence_smoke); Task 13 only adds
            // the wiring. Until Task 15 ports composite_glyphs into the frame
            // builder, no frame can be open, so this call is a no-op.
            match self.engine.close_open_frame(
                &mut self.store,
                &mut self.platform,
                crate::kms::render::frame_builder::CloseReason::LegacyScCompose,
            ) {
                Ok(crate::kms::render::frame_builder::CloseOutcome::Submitted { .. }) => {
                    legacy_close_submitted = true;
                }
                Ok(crate::kms::render::frame_builder::CloseOutcome::AlreadyClosed) => {}
                Err(e) => log::warn!("render maybe_composite: close_open_frame failed: {e:?}"),
            }
            // Phase A Task 4: flush the SubmitGroup so scene.tick
            // observes all paint CBs already submitted to the queue.
            // Compose stays on its own dedicated `vkQueueSubmit2`
            // (record_compose) — only the buffered paint group is
            // flushed here. Drive through the engine wrapper so
            // parked `pending_group_ops` commit too.
            if let Err(e) = self.engine.flush_submit_group(
                &mut self.store,
                &mut self.platform,
                crate::kms::render::submit_group::FlushReason::SceneCompose,
            ) {
                log::warn!("render maybe_composite: flush_submit_group failed: {e:?}");
            }
        }
        let result = if !can_submit_scene {
            Ok(())
        } else {
            let cow_host_xid = self.cow_host_xid();
            match self.scene.tick(
                &self.core,
                &mut self.store,
                &mut self.platform,
                &self.windows,
                &mut self.telemetry,
                cow_host_xid,
            ) {
                Ok(composed_outputs) => {
                    if legacy_close_submitted {
                        crate::kms::vk::submit_stats::SUBMITS
                            .record_legacy_sc_tick(!composed_outputs.is_empty());
                    }
                    if self.scanout_m2.reentry_blocked_until_composed
                        && composed_outputs.len() == self.platform.outputs.len()
                    {
                        self.scanout_m2.reentry_blocked_until_composed = false;
                        log::debug!(
                            "scanout_m2: composed fallback submitted; re-entry barrier cleared"
                        );
                    }
                    for output_idx in composed_outputs {
                        self.telemetry.record_composite_submit();
                        // One scene_compose event per output that presented
                        // this tick, keyed by the exact output index.
                        self.telemetry.record_submit_event(SubmitEvent {
                            frame_id: 0,
                            kind: SubmitKind::SceneCompose,
                            target_kind: TargetKind::Output,
                            target_id: u64::try_from(output_idx).unwrap_or(0),
                            batch_size: 1,
                            op: SubmitOp::None,
                            src_class: SrcClass::None,
                            mask_class: SrcClass::None,
                            pipeline_id: None,
                            flags: SubmitFlags::NONE,
                        });
                    }
                    Ok(())
                }
                Err(e) => {
                    if legacy_close_submitted {
                        crate::kms::vk::submit_stats::SUBMITS.record_legacy_sc_tick(false);
                    }
                    log::warn!("render maybe_composite: scene.tick failed: {e:?}");
                    Ok(())
                }
            }
        };
        self.drain_paint_submit_telemetry();
        result
    }

    fn dump_scanout(&mut self) {
        if let Err(e) = do_dump_scanout(self) {
            log::warn!("render dump_scanout: {e}");
        }
    }

    fn report_export_holders(
        &mut self,
        core: &dyn Fn() -> yserver_core::backend::export_holders::CoreHolders,
    ) -> bool {
        let rows = self.export_holder_rows();
        if !self.export_holders.observe(rows) {
            return false;
        }
        let core = core();
        for line in
            crate::kms::render::export_holders::format_report(self.export_holders.rows(), &core)
        {
            log::info!(target: crate::RESOURCE_TELEMETRY_TARGET, "{line}");
        }
        true
    }

    fn dump_drawables(&mut self) {
        if let Err(e) = do_dump_drawables(self) {
            log::warn!("render dump_drawables: {e}");
        }
        // Stage 4d shadow-hunt: COW vs scanout vs present-src must
        // come from the same instant or the comparison is useless
        // (the moment of interest is the first COW-targeted
        // Present after caja paints, which moves on every frame).
        // Pair the scanout dump with the drawable dump so a single
        // Ctrl+Alt+F12 captures all three artifacts atomically.
        if let Err(e) = do_dump_scanout(self) {
            log::warn!("render dump_drawables: scanout side: {e}");
        }
        // Surface the COW + present-src ring state so the user can
        // tell at-a-glance whether the dump captured the expected
        // shape (cow_id set, recent sources non-empty) without
        // having to grep for the per-target log lines.
        log::info!(
            "render dump_drawables: cow_id={:?} recent_present_pixmaps_len={}",
            self.cow_id,
            self.recent_present_pixmaps.len(),
        );
    }

    fn note_present_pixmap(&mut self, src_pixmap_xid: u32, dst_window_xid: u32) {
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

    fn note_present_scanout_candidate(&mut self, candidate: PresentScanoutCandidate) {
        self.observe_scanout_m0(candidate);
    }

    fn try_present_direct(
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

    fn note_present_skip(&mut self) {
        self.telemetry.record_present_skip();
    }

    fn arm_present_source_wait(
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

    fn arm_present_syncobj_wait(
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

    fn drain_ready_present_source_waits(&mut self) -> Vec<u64> {
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

    fn begin_ready_present_destination_write(&mut self, wait_id: u64) {
        if let Some(id) = self
            .pending_present_source_waits
            .get(&wait_id)
            .and_then(|wait| wait.prewaited_destination)
        {
            self.store.begin_prewaited_exported_write(id);
        }
    }

    fn finish_present_source_wait(&mut self, wait_id: u64) {
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

    fn present_flip_in_flight(&self, crtc_id: u32) -> bool {
        self.present_crtc_output(crtc_id)
            .is_some_and(|(output_idx, _)| self.present_flip_in_flight_for_output(output_idx))
    }

    fn present_display_idle(&self, crtc_id: u32) -> bool {
        self.present_crtc_key(crtc_id)
            .is_some_and(|crtc_key| self.present_completion_is_idle_for(crtc_key))
    }

    fn present_absolute_vblank_arm_supported(&self, crtc_id: u32) -> bool {
        self.present_crtc_key(crtc_id).is_some_and(|key| {
            !self
                .crtc_queue_sequence_unsupported_devices
                .contains(&key.device_key)
        })
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
        let id = self.store.lookup(host_xid)?;
        self.store.incref(id);
        let pin_id = self.next_present_source_pin_id;
        self.next_present_source_pin_id = self.next_present_source_pin_id.wrapping_add(1).max(1);
        self.present_source_pins.insert(pin_id, id);
        Some(pin_id)
    }

    fn release_present_source(&mut self, pin_id: u64) {
        let Some(id) = self.present_source_pins.remove(&pin_id) else {
            return;
        };
        self.store_decref_with_invalidate(id);
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
        #[cfg(target_os = "linux")]
        {
            let saw_change = self
                .platform
                .hotplug_monitor
                .as_mut()
                .map(|monitor| monitor.drain())
                .unwrap_or(false);
            if saw_change {
                self.hotplug_rescan_deadline =
                    Some(std::time::Instant::now() + std::time::Duration::from_millis(150));
                log::debug!("kms: display hotplug edge — rescan armed (+150ms)");
            }
        }
    }

    fn reprobe_connectors(&mut self, state: &mut ServerState) -> io::Result<()> {
        // RANDR's forced resource refresh only needs connector presence and
        // mode lists. Full `discover_outputs` also enumerates planes,
        // properties and modifiers and computes hypothetical assignments;
        // under Cinnamon/GPU load that unrelated work blocked dispatch for
        // 90–113 ms every time the desktop polled GetScreenResources.
        let probes = self.platform.probe_all_connectors()?;
        let _ = self.publish_connector_probes(state, &probes);
        Ok(())
    }

    fn set_provider_output_source(
        &mut self,
        state: &mut ServerState,
        provider: u32,
        source_provider: Option<u32>,
    ) -> io::Result<bool> {
        let invalid = |message: String| io::Error::new(io::ErrorKind::InvalidInput, message);
        let sink_endpoint = self
            .current_provider_endpoint_for_id(provider)
            .ok_or_else(|| invalid(format!("unknown or inactive RANDR provider id {provider}")))?;
        let RandrProviderEndpoint::Kms(sink_key) = sink_endpoint else {
            return Err(invalid(format!(
                "RANDR provider {provider} ({sink_endpoint:?}) is not a KMS output sink",
            )));
        };
        let sink_has_connector_inventory = self
            .randr_id_alloc
            .entries()
            .any(|(key, _)| key.device_key == sink_key);
        if !sink_has_connector_inventory {
            return Err(invalid(format!(
                "RANDR provider {provider} ({sink_endpoint:?}) has no connector inventory and is not an output sink",
            )));
        }

        let selected_source = self.selected_render_provider_endpoint();
        if selected_source == Some(sink_endpoint) {
            return Err(invalid(format!(
                "RANDR provider {provider} is the selected renderer's coalesced KMS endpoint; same-device scanout is implicit and it cannot be an output sink",
            )));
        }
        let requested_source = source_provider
            .map(|source_id| {
                self.current_provider_endpoint_for_id(source_id)
                    .ok_or_else(|| {
                        invalid(format!(
                            "unknown or inactive RANDR source provider id {source_id}",
                        ))
                    })
            })
            .transpose()?;
        if let Some(source) = requested_source
            && Some(source) != selected_source
        {
            return Err(invalid(format!(
                "RANDR source provider {} ({source:?}) is not the selected operational renderer {:?}",
                source_provider.expect("requested source has an XID"),
                selected_source,
            )));
        }

        let current_source = self.provider_output_sources.get(&sink_key).copied();
        let source_changes = requested_source != current_source;
        if !source_changes {
            return Ok(false);
        }

        // `platform.outputs` is the authoritative lifetime inventory even
        // while DPMS is off or the VT is suspended: those routes still own
        // scanout pools and can be re-lit. Never revoke or replace their source
        // policy in place.
        if current_source.is_some() {
            let active_connectors: Vec<_> = self
                .platform
                .outputs
                .iter()
                .filter(|output| output.key.device_key == sink_key)
                .map(|output| format!("{} on {}", output.key.connector_name, output.key.device_key))
                .collect();
            if !active_connectors.is_empty() {
                return Err(invalid(format!(
                    "cannot change PRIME Output Source for active sink provider {provider}: {}",
                    active_connectors.join(", "),
                )));
            }
        }

        match requested_source {
            Some(source) => {
                self.provider_output_sources.insert(sink_key, source);
                log::info!(
                    "PRIME Output Source: sink provider {provider} ({sink_endpoint:?}) -> source provider {} ({source:?})",
                    source_provider.expect("attached source has an XID"),
                );
            }
            None => {
                // Startup auto-association is one-shot. Absence therefore
                // records the client's explicit detach for the rest of this
                // backend lifetime; registry rebuilds never repopulate it.
                self.provider_output_sources.remove(&sink_key);
                log::info!(
                    "PRIME Output Source: detached sink provider {provider} ({sink_endpoint:?})"
                );
            }
        }
        self.bump_crtc_config_topology_epoch("PRIME provider output source changed");

        // A provider relationship changes neither the CRTC configuration nor
        // available connector/mode inventory. Rebuild only the projection and
        // preserve both lastSetTime and configTimestamp.
        self.rebuild_randr_state(state, None, false);
        Ok(true)
    }

    fn begin_crtc_config(
        &mut self,
        output_id: u32,
        connector: &str,
        mode: Option<yserver_core::backend::ModeSpec>,
        x: i32,
        y: i32,
    ) -> io::Result<CrtcConfigApply> {
        // Until a worker/helper transport is installed, preserve the existing
        // synchronous backend behavior exactly. Disables and same-device
        // changes also have no disposable PRIME qualification to move away
        // from the core thread.
        let Some(mode_spec) = mode else {
            return self
                .apply_crtc_config(output_id, connector, mode, x, y)
                .map(CrtcConfigApply::Applied);
        };
        if self.crtc_config_probe_executor.is_none() {
            return self
                .apply_crtc_config(output_id, connector, mode, x, y)
                .map(CrtcConfigApply::Applied);
        }

        let output_key = self
            .output_key_by_id
            .get(&output_id)
            .cloned()
            .ok_or_else(|| io::Error::other(format!("unknown RANDR output id {output_id}")))?;
        if output_key.connector_name != connector {
            return Err(io::Error::other(format!(
                "RANDR output {output_id} name mismatch: registry has {}, request resolved {connector}",
                output_key.connector_name
            )));
        }

        // Match apply_crtc_config's policy ordering: an idempotent request may
        // not silently reassert a split output after its provider association
        // was detached while another client was active.
        if !self.provider_output_source_allows(output_key.device_key) {
            let sink_endpoint = RandrProviderEndpoint::Kms(output_key.device_key);
            let sink_provider = self.randr_id_alloc.providers.get(&sink_endpoint).copied();
            let selected_source = self.selected_render_provider_endpoint();
            let source_provider = selected_source
                .and_then(|endpoint| self.randr_id_alloc.providers.get(&endpoint).copied());
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "output {connector} belongs to KMS sink provider {} ({sink_endpoint:?}); attach it to selected source provider {} ({selected_source:?}) with RANDR SetProviderOutputSource before enabling it",
                    sink_provider.map_or_else(|| "<unknown>".to_string(), |id| id.to_string()),
                    source_provider.map_or_else(|| "<none>".to_string(), |id| id.to_string()),
                ),
            ));
        }

        let requested = ConnectorConfig::Enabled {
            mode_w: mode_spec.width,
            mode_h: mode_spec.height,
            vrefresh: mode_spec.vrefresh,
            x,
            y,
        };
        let current = self
            .platform
            .outputs
            .iter()
            .find(|layout| layout.key == output_key)
            .map_or(ConnectorConfig::Off, |layout| ConnectorConfig::Enabled {
                mode_w: layout.width,
                mode_h: layout.height,
                vrefresh: layout.output.picked.vrefresh,
                x: layout.x,
                y: layout.y,
            });
        if current == requested {
            return self
                .apply_crtc_config(output_id, connector, mode, x, y)
                .map(CrtcConfigApply::Applied);
        }

        if self
            .platform
            .vk
            .as_ref()
            .is_some_and(|vk| vk.is_software_rasterizer())
            && std::env::var_os("YSERVER_ALLOW_SOFTWARE_VULKAN").is_none()
        {
            return Err(io::Error::other(format!(
                "begin_crtc_config: refusing to enable {connector} with a software Vulkan \
                 renderer; install a hardware Vulkan driver or set \
                 YSERVER_ALLOW_SOFTWARE_VULKAN=1 for a deliberate software-scanout setup"
            )));
        }
        if self.vt_state != crate::vt::state::VtState::Active {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                format!(
                    "begin_crtc_config: cannot qualify {connector} while VT is {:?}",
                    self.vt_state
                ),
            ));
        }

        let route = self.platform.scanout_route_for_kms(output_key.device_key)?;
        if !self.crtc_enable_needs_async_qualification(&output_key, mode_spec, route) {
            return self
                .apply_crtc_config(output_id, connector, mode, x, y)
                .map(CrtcConfigApply::Applied);
        }

        let output_device = Rc::clone(
            &self
                .platform
                .device_for_output(&output_key)
                .ok_or_else(|| {
                    io::Error::other(format!(
                        "RANDR output {output_id} belongs to unavailable DRM device {}",
                        output_key.device_key
                    ))
                })?
                .device,
        );
        // Discovery and advertised-mode validation are deliberately completed
        // while the old topology is still lit. The live DRM output stays in
        // the pending entry; the executor receives one owned KMS-fd duplicate
        // plus a scalar route request.
        let prepared_output =
            self.discover_crtc_config_output(&output_key, &output_device, connector)?;
        if prepared_output.connector_name != connector {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "begin_crtc_config: discovery returned connector {} for requested {connector}",
                    prepared_output.connector_name
                ),
            ));
        }
        if !prepared_output.modes.iter().any(|candidate| {
            candidate.width == mode_spec.width
                && candidate.height == mode_spec.height
                && candidate.vrefresh == mode_spec.vrefresh
        }) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "connector {connector}: mode {}x{}@{} not in advertised list",
                    mode_spec.width, mode_spec.height, mode_spec.vrefresh
                ),
            ));
        }

        let token = self.enqueue_prepared_crtc_config_probe(
            output_id,
            output_key,
            connector.to_string(),
            mode_spec,
            x,
            y,
            prepared_output,
            route,
        )?;
        Ok(CrtcConfigApply::Pending(token))
    }

    fn drain_ready_crtc_configs(&mut self) -> Vec<CrtcConfigToken> {
        let completions = self
            .crtc_config_probe_executor
            .as_mut()
            .map(|executor| executor.drain_ready())
            .unwrap_or_default();
        for completion in completions {
            if !self
                .pending_crtc_config_probes
                .contains_key(&completion.token)
            {
                // Cancellation may race worker completion. The result is
                // resource-free, so dropping it is sufficient; still notify
                // the executor so it can retire transport bookkeeping.
                if let Some(executor) = self.crtc_config_probe_executor.as_mut() {
                    executor.cancel(completion.token);
                }
                continue;
            }
            if self
                .ready_crtc_config_results
                .contains_key(&completion.token)
            {
                log::debug!(
                    "asynchronous CRTC qualifier returned late/duplicate token {:?}; ignoring it",
                    completion.token
                );
                continue;
            }
            self.ready_crtc_config_results
                .insert(completion.token, completion.result);
            self.ready_crtc_config_announcements
                .push_back(completion.token);
        }
        self.ready_crtc_config_announcements.drain(..).collect()
    }

    fn finish_crtc_config(&mut self, token: CrtcConfigToken) -> io::Result<bool> {
        self.remove_crtc_config_ready_announcement(token);
        self.invalidated_crtc_config_probes.remove(&token);
        let result = match self.ready_crtc_config_results.remove(&token) {
            Some(result) => result,
            None if self.pending_crtc_config_probes.contains_key(&token) => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!("asynchronous CRTC configuration {token:?} is not ready"),
                ));
            }
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("unknown asynchronous CRTC configuration token {token:?}"),
                ));
            }
        };
        let Some(mut pending) = self.pending_crtc_config_probes.remove(&token) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("orphaned asynchronous CRTC result for token {token:?}"),
            ));
        };
        if let Some(executor) = self.crtc_config_probe_executor.as_mut() {
            executor.cancel(token);
        }

        let stale_error = |stage: &str, reason: String| {
            io::Error::new(
                io::ErrorKind::Interrupted,
                format!("asynchronous CRTC configuration {token:?} became stale {stage}: {reason}"),
            )
        };
        if let Some(reason) = self.stale_crtc_config_probe_reason(&pending) {
            return Err(stale_error("before exact-plan replay", reason));
        }
        // Worker failures are terminal for this request but have not touched
        // the live topology. In particular, an indeterminate disposable probe
        // never enters quiesce/recovery on the core thread.
        let qualified = result?;
        let prepared_output = pending
            .prepared_output
            .take()
            .expect("pending CRTC qualification owns its discovered output");

        // Exact live allocation and TEST_ONLY happen while the old topology
        // is still scanning out. The returned object is opaque but owns all
        // uncommitted live resources, so an error or stale result can drop it
        // safely without a blackout or partial installation.
        let prepared = self.platform.prepare_qualified_connector_plan(
            &pending.output_key,
            prepared_output,
            pending.mode,
            pending.x,
            pending.y,
            qualified,
        )?;
        if let Some(reason) = self.stale_crtc_config_probe_reason(&pending) {
            return Err(stale_error("during exact-plan replay", reason));
        }

        let restore_old_on_failure = pending.was_active;
        self.quiesce_before_topology_mutation("asynchronous RANDR CRTC configuration changed")?;
        if let Err(error) = self.platform.install_prepared_connector_plan(prepared) {
            log::error!(
                "finish_crtc_config: installing qualified plan for {} failed: {error}",
                pending.connector
            );
            return Err(self.recover_failed_crtc_config(restore_old_on_failure, error));
        }

        {
            let entry = self.randr_id_alloc.entry_mut(&pending.output_key);
            entry.config = ConnectorConfig::Enabled {
                mode_w: pending.mode.width,
                mode_h: pending.mode.height,
                vrefresh: pending.mode.vrefresh,
                x: pending.x,
                y: pending.y,
            };
            entry.client_configured = true;
            entry.connected = true;
            // The client has placed this output itself; the remembered route
            // and its reserved slot are released.
            entry.last_enabled = None;
        }
        log::info!(
            "finish_crtc_config: enabled {} {}x{}@{} at ({},{}) with qualified plan",
            pending.connector,
            pending.mode.width,
            pending.mode.height,
            pending.mode.vrefresh,
            pending.x,
            pending.y,
        );

        self.prune_armed_targets_to_live_outputs();
        let desired_active = kms_outputs_active_after_crtc_config(
            restore_old_on_failure,
            true,
            self.platform.outputs.len(),
        );
        if let Err(error) = self.scene.rebuild_outputs(&self.platform) {
            log::error!(
                "finish_crtc_config: scene rebuild failed after topology change: {error:?}"
            );
            let error = io::Error::other(format!(
                "finish_crtc_config: scene rebuild failed: {error:?}"
            ));
            let relight = self.relight_after_direct_teardown(
                desired_active,
                "asynchronous RANDR CRTC scene-rebuild failure",
            );
            self.kms_outputs_active = false;
            self.request_exit();
            return Err(relight.err().unwrap_or(error));
        }
        self.relight_after_direct_teardown(
            desired_active,
            "asynchronous RANDR CRTC configuration",
        )?;
        self.kms_outputs_active = desired_active;
        self.update_input_extent(self.platform.fb_w, self.platform.fb_h);
        self.scene.wake_for_damage();
        Ok(true)
    }

    fn cancel_crtc_config(&mut self, token: CrtcConfigToken) {
        self.remove_crtc_config_ready_announcement(token);
        self.invalidated_crtc_config_probes.remove(&token);
        self.pending_crtc_config_probes.remove(&token);
        self.ready_crtc_config_results.remove(&token);
        if let Some(executor) = self.crtc_config_probe_executor.as_mut() {
            executor.cancel(token);
        }
    }

    fn apply_crtc_config(
        &mut self,
        output_id: u32,
        connector: &str,
        mode: Option<yserver_core::backend::ModeSpec>,
        x: i32,
        y: i32,
    ) -> io::Result<bool> {
        let output_key = self
            .output_key_by_id
            .get(&output_id)
            .cloned()
            .ok_or_else(|| io::Error::other(format!("unknown RANDR output id {output_id}")))?;
        if output_key.connector_name != connector {
            return Err(io::Error::other(format!(
                "RANDR output {output_id} name mismatch: registry has {}, request resolved {connector}",
                output_key.connector_name
            )));
        }
        let output_device = Rc::clone(
            &self
                .platform
                .device_for_output(&output_key)
                .ok_or_else(|| {
                    io::Error::other(format!(
                        "RANDR output {output_id} belongs to unavailable DRM device {}",
                        output_key.device_key
                    ))
                })?
                .device,
        );

        // Provider policy authorizes only the attempt. Exact real-operation
        // DMA-BUF allocation/import/render/TEST_ONLY probing remains in
        // `enable_connector`; capability metadata there is diagnostic only.
        // Place this before the idempotency guard so an
        // already-active split output can never be silently reasserted under a
        // missing/stale policy. Production startup auto-associates every
        // distinct sink, while an explicit later detach remains persistent.
        if mode.is_some() && !self.provider_output_source_allows(output_key.device_key) {
            let sink_endpoint = RandrProviderEndpoint::Kms(output_key.device_key);
            let sink_provider = self.randr_id_alloc.providers.get(&sink_endpoint).copied();
            let selected_source = self.selected_render_provider_endpoint();
            let source_provider = selected_source
                .and_then(|endpoint| self.randr_id_alloc.providers.get(&endpoint).copied());
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "output {connector} belongs to KMS sink provider {} ({sink_endpoint:?}); attach it to selected source provider {} ({selected_source:?}) with RANDR SetProviderOutputSource before enabling it",
                    sink_provider.map_or_else(|| "<unknown>".to_string(), |id| id.to_string()),
                    source_provider.map_or_else(|| "<none>".to_string(), |id| id.to_string()),
                ),
            ));
        }

        // ── Idempotency guard (CRITICAL) ──────────────────────────────────
        //
        // MATE / mate-settings-daemon re-assert the SAME SetCrtcConfig many
        // times in a row (bursts of identical requests). Every call here used
        // to run a full quiesce + modeset + scene rebuild + repaint, which on
        // a steady-state desktop hammers the CRTC back-to-back → constant
        // flicker/tearing (observed single-screen, immediate zap). Compare the
        // request against the ACTUAL current scanout state (`platform.outputs`
        // is the source of truth) and no-op when nothing changed, so only a
        // genuine mode/position/on-off change pays the modeset cost.
        let requested = match mode {
            None => ConnectorConfig::Off,
            Some(m) => ConnectorConfig::Enabled {
                mode_w: m.width,
                mode_h: m.height,
                vrefresh: m.vrefresh,
                x,
                y,
            },
        };
        let current = self
            .platform
            .outputs
            .iter()
            .find(|layout| layout.key == output_key)
            .map_or(ConnectorConfig::Off, |l| ConnectorConfig::Enabled {
                mode_w: l.width,
                mode_h: l.height,
                vrefresh: l.output.picked.vrefresh,
                x: l.x,
                y: l.y,
            });
        if current == requested {
            log::debug!(
                "apply_crtc_config: {connector} already at requested config ({requested:?}); no-op"
            );
            // Keep the registry's current-config view in sync (cheap) without
            // touching the hardware. Return `false` = nothing changed, so the
            // handler skips the change-notify (Xorg RRTellChanged only fires
            // on a real change) — this is what breaks MATE's re-assert loop.
            let entry = self.randr_id_alloc.entry_mut(&output_key);
            entry.config = requested;
            // A client asserting a config is an explicit statement of intent
            // about this output, so it releases any remembered route (and
            // with it the reserved slot). Never resurrect a route the client
            // has spoken for.
            entry.last_enabled = None;
            return Ok(false);
        }

        // An opened card may have started with no connected outputs, in which
        // case software Vulkan is valid for headless X rendering. Refuse the
        // first later RANDR scanout enable unless the same explicit override
        // accepted by startup is present; exporting a software-Vulkan BO to
        // real KMS can hard-hang the machine.
        if mode.is_some()
            && self
                .platform
                .vk
                .as_ref()
                .is_some_and(|vk| vk.is_software_rasterizer())
            && std::env::var_os("YSERVER_ALLOW_SOFTWARE_VULKAN").is_none()
        {
            return Err(io::Error::other(format!(
                "apply_crtc_config: refusing to enable {connector} with a software Vulkan \
                 renderer; install a hardware Vulkan driver or set \
                 YSERVER_ALLOW_SOFTWARE_VULKAN=1 for a deliberate software-scanout setup"
            )));
        }

        // Resolve the requested connector before taking the old CRTC set
        // offline. A pure discovery failure must not blank a working desktop.
        // Pool allocation and the actual modeset remain in `enable_connector`
        // after quiescing, where failures can restore the old composed set.
        let prepared_output = if mode.is_some() {
            let reserved_routes: Vec<_> = self
                .platform
                .outputs
                .iter()
                .filter(|layout| {
                    layout.key.device_key == output_key.device_key && layout.key != output_key
                })
                .map(|layout| {
                    (
                        layout.output.encoder,
                        layout.output.crtc,
                        layout.output.plane,
                    )
                })
                .collect();
            Some(
                crate::platform::drm::discover_output_for_connector(
                    &output_device,
                    connector,
                    &reserved_routes,
                )
                .map_err(|e| {
                    log::error!("apply_crtc_config: target discovery for {connector} failed: {e}");
                    e
                })?,
            )
        } else {
            None
        };

        // ── Flip-safety: quiesce the complete old topology ────────────────
        //
        // Both enable and disable modify `platform.outputs` (topology),
        // so we need `drain_all` + `rebuild_outputs` — the same path
        // `fire_randr_changes` uses for hotplug.  The sequence below:
        //
        //   all CRTCs off      — proves no survivor still references a BO
        //                       whose userspace phase is about to be reset.
        //   wait/drain/reset   — retires GPU work and clears both the scene
        //                       ack ledger and matching platform BO phases.
        //   platform mutate   — disable_connector / enable_connector
        //   rebuild + relight — restores every surviving/new active CRTC.
        //
        // The `commit_modeset` / `disable_output` calls in the platform
        // helpers are ALLOW_MODESET atomic commits (not page-flips), so
        // they are always legal after drain_all.  After rebuild_outputs
        // the scene's `pending_acks` is fresh-empty for every output, so
        // the subsequent `wake_for_damage` tick is EBUSY-safe.
        let restore_old_on_failure = self.kms_outputs_active;
        self.quiesce_before_topology_mutation("RANDR CRTC configuration changed")?;

        match mode {
            None => {
                // ── Disable path ─────────────────────────────────────────
                if self.platform.remove_connector_after_all_off(&output_key) {
                    log::info!("apply_crtc_config: disabled {connector}");
                } else {
                    // Already off — still update the registry.
                    log::debug!("apply_crtc_config: {connector} was already off");
                }
                // Update registry: connector stays known, config → Off.
                {
                    let entry = self.randr_id_alloc.entry_mut(&output_key);
                    entry.config = ConnectorConfig::Off;
                    entry.crtc_associated = false;
                    // client_configured is set to record that a client
                    // explicitly disabled this output (not an auto-layout op).
                    entry.client_configured = true;
                    // An explicit disable must not be undone by a later
                    // auto-relight (invariant 6): unplugging a deliberately
                    // disabled monitor may not resurrect it.
                    entry.last_enabled = None;
                }
            }
            Some(mode_spec) => {
                // ── Enable / mode-change path ────────────────────────────
                let output = prepared_output.expect("enabled request prepared its connector");

                // enable_connector handles: mode resolution, pool
                // (re)alloc, commit_modeset, ActiveOutput update,
                // fb extent recompute.
                if let Err(e) = self
                    .platform
                    .enable_connector(&output_key, output, mode_spec, x, y)
                {
                    log::error!("apply_crtc_config: enable_connector({connector}) failed: {e}");
                    if is_terminal_disposable_probe_error(&e) {
                        // The old topology is already quiesced and dark. A
                        // terminal probe failure deliberately retains GPU
                        // owners; rebuilding the scene here would drop the old
                        // composite rings and re-enter vkDeviceWaitIdle on the
                        // same physical GPU. Re-commit only the unchanged old
                        // KMS framebuffers, without rebuilding or dropping any
                        // Vulkan owner, then return to the core loop so
                        // input/VT handling remains responsive.
                        if restore_old_on_failure {
                            match self.platform.dpms_set_outputs_active(true) {
                                Ok(()) => {
                                    self.reapply_gamma_for_live_outputs();
                                    self.kms_outputs_active = !self.platform.outputs.is_empty();
                                    log::error!(
                                        "apply_crtc_config: terminal disposable probe failure; \
                                         restored the unchanged old KMS topology and skipped \
                                         Vulkan teardown/recovery"
                                    );
                                }
                                Err(relight_error) => {
                                    self.kms_outputs_active = false;
                                    log::error!(
                                        "apply_crtc_config: terminal disposable probe failure; \
                                         old KMS topology relight also failed: {relight_error}; \
                                         skipped Vulkan teardown/recovery"
                                    );
                                }
                            }
                        } else {
                            self.kms_outputs_active = false;
                            log::error!(
                                "apply_crtc_config: terminal disposable probe failure from a \
                                 previously headless topology; skipped Vulkan teardown/recovery"
                            );
                        }
                        return Err(e);
                    }
                    return Err(self.recover_failed_crtc_config(restore_old_on_failure, e));
                }

                // Update registry.
                {
                    let entry = self.randr_id_alloc.entry_mut(&output_key);
                    entry.config = ConnectorConfig::Enabled {
                        mode_w: mode_spec.width,
                        mode_h: mode_spec.height,
                        vrefresh: mode_spec.vrefresh,
                        x,
                        y,
                    };
                    entry.crtc_associated = true;
                    entry.client_configured = true;
                    entry.connected = true;
                    // The client has placed this output itself; the
                    // remembered route and its reserved slot are released.
                    entry.last_enabled = None;
                }

                log::info!(
                    "apply_crtc_config: enabled {connector} {}×{}@{} at ({x},{y})",
                    mode_spec.width,
                    mode_spec.height,
                    mode_spec.vrefresh
                );
            }
        }

        self.prune_armed_targets_to_live_outputs();

        // Decide whether the new topology should be lit. A first enable from
        // headless opens the gate; disabling while the old topology was dark
        // keeps surviving outputs dark.
        let desired_active = kms_outputs_active_after_crtc_config(
            restore_old_on_failure,
            matches!(requested, ConnectorConfig::Enabled { .. }),
            self.platform.outputs.len(),
        );

        // ── Scene + RANDR rebuild ─────────────────────────────────────────
        if let Err(e) = self.scene.rebuild_outputs(&self.platform) {
            log::error!("apply_crtc_config: scene rebuild failed after topology change: {e:?}");
            let error = io::Error::other(format!("apply_crtc_config: scene rebuild failed: {e:?}"));
            let relight = self
                .relight_after_direct_teardown(desired_active, "RANDR CRTC scene-rebuild failure");
            // Hardware/platform/registry state has already changed, but the
            // core RANDR projection cannot be rebuilt consistently. Rollback
            // would itself require another fallible modeset, so fail-stop
            // instead of continuing with two contradictory topologies.
            self.kms_outputs_active = false;
            self.request_exit();
            return Err(relight.err().unwrap_or(error));
        }
        self.relight_after_direct_teardown(desired_active, "RANDR CRTC configuration")?;
        self.kms_outputs_active = desired_active;

        // Update input extent (cursor clamp) to reflect new fb size.
        let (new_fb_w, new_fb_h) = (self.platform.fb_w, self.platform.fb_h);
        self.update_input_extent(new_fb_w, new_fb_h);

        self.scene.wake_for_damage();
        Ok(true)
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
        // The root extent is the client's (spec D3, "Two extents"); a CRTC
        // set recomputes `fb_w`/`fb_h` from the modes, which is neither the
        // root nor any footprint.
        let root = (
            state.randr.screen_width.max(1),
            state.randr.screen_height.max(1),
        );
        let root_changed = root != (self.platform.fb_w, self.platform.fb_h);
        if root_changed {
            (self.platform.fb_w, self.platform.fb_h) = root;
            self.update_input_extent(root.0, root.1);
        }
        // Rotation and reflection combined with the client transform, as
        // `RRTransformCompute`: one matrix for footprint, pass and readback.
        let transforms: HashMap<OutputKey, yserver_core::randr::CrtcTransform> = state
            .randr
            .outputs
            .iter()
            .map(|o| (o, o.crtc_transform()))
            .filter(|(_, t)| !t.is_identity())
            .filter_map(|(o, t)| {
                let key = self.output_key_by_id.get(&o.output_id)?.clone();
                Some((key, t))
            })
            .collect();
        let transforms_changed = transforms != self.platform.output_transforms;
        if transforms_changed {
            // No transformed CRTC is ever flipped directly (spec D5).
            self.request_direct_unflip("crtc_transform_changed");
            self.platform.output_transforms = transforms;
            log::info!(
                "kms: CRTC transforms now on {} output(s)",
                self.platform.output_transforms.len()
            );
        }
        if root_changed || transforms_changed {
            if let Err(error) = self.scene.sync_output_layouts(&self.platform) {
                log::error!("kms: scene could not follow the RANDR layout: {error}");
            }
            self.scene.wake_for_damage();
        }
        self.move_pointer_to_nearest_crtc(state);
    }

    fn output_identity(&self, output_id: u32) -> Option<(Vec<u8>, String)> {
        self.output_identity_by_id.get(&output_id).cloned()
    }

    fn set_logical_screen_size(&mut self, w: u16, h: u16) -> io::Result<()> {
        let w = w.max(1);
        let h = h.max(1);
        if (w, h) == (self.platform.fb_w, self.platform.fb_h) {
            return Ok(());
        }

        self.apply_virtual_screen_extent(w, h)?;
        log::info!("render set_logical_screen_size: resized virtual screen to {w}×{h}");
        Ok(())
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
        self.platform
            .vk
            .as_ref()
            .map_or(yserver_protocol::x11::glx::VENDOR_NAMES, |vk| {
                glx_vendor_names_for_driver(vk.driver_id)
            })
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
        if self.cow_id.is_some() {
            // Core only calls this on the 0 → 1 claim edge, so a live
            // `cow_id` here can only be a physical teardown the previous
            // final release deferred behind direct scanout: the protocol
            // resource was logically destroyed but the backend identity
            // and storage stayed alive for their safe replacement. Reuse
            // that identity and ask core to materialize the protocol
            // resource again.
            debug_assert!(
                self.deferred_cow_release,
                "get_overlay_window is the 0 → 1 edge; a live cow_id here \
                 without a deferred release means core and the backend have \
                 drifted",
            );
            self.deferred_cow_release = false;
            return Ok(true);
        }
        let fb_w = self.platform.fb_w.max(1);
        let fb_h = self.platform.fb_h.max(1);
        let storage = match self.platform.allocate_drawable_storage_as(
            fb_w,
            fb_h,
            24,
            crate::kms::vk::mem_accounting::MemCategory::WindowStorage,
        ) {
            Ok(storage) => {
                self.telemetry.record_storage_allocation();
                self.telemetry.record_image_view_create();
                storage
            }
            Err(e) => {
                // Test-fixture / no-Vk path: same shape as
                // `init_root_storage` — fall back to a null-view
                // stub so unit tests can exercise refcount /
                // scene-registration without a live Vk ICD.
                log::debug!("render get_overlay_window: no Vk, using stub COW storage: {e:?}");
                crate::kms::render::store::Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: u32::from(fb_w),
                        height: u32::from(fb_h),
                    },
                    crate::kms::render::platform::PlatformBackend::format_for_depth(24),
                )
            }
        };
        let xid = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
        // Defensive: if a stale mapping somehow survives a prior
        // teardown (decref's PendingFence path detaches xid for us,
        // but a synchronous-destroy path could race), detach first
        // so the allocate doesn't trip XidInUse.
        self.store.detach_xid(xid);
        let id = self
            .store_alloc(xid, DrawableKind::Window, 24, true, storage)
            .map_err(|e| {
                io::Error::other(format!("render get_overlay_window: store alloc: {e:?}"))
            })?;
        // Stage 3f.14 follow-on — zero-fill the fresh storage so
        // the compositor doesn't composite over recycled GPU
        // garbage on its first paint. Best-effort on stub paths.
        let rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D::default(),
            extent: ash::vk::Extent2D {
                width: u32::from(fb_w),
                height: u32::from(fb_h),
            },
        };
        if let Err(e) = self.engine.fill_rect(
            &mut self.store,
            &mut self.platform,
            Dst::server_internal(id),
            rect,
            default_window_init_color(24),
        ) && self.platform.vk.is_some()
        {
            log::warn!("render get_overlay_window: initial fill failed: {e:?}");
        }
        self.cow_id = Some(id);

        // Phase 2 Task 2.2 — also materialize the backend's window-
        // tree projection so the COW participates in build_scene /
        // hit-testing / paint resolution the same way any top-level
        // window does. The xid is the well-known protocol xid; v2
        // keys windows on host xid directly.
        let cow_host_xid = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
        let rank = self.alloc_window_stack_rank();
        let geom = WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 0,
            y: 0,
            width: fb_w,
            height: fb_h,
            depth: 24,
            mapped: true,
            viewable: true,
            // `parent: None` matches windows's convention for a
            // direct child of the root (root is not itself tracked
            // in windows — see register_top_level).
            parent: None,
            stack_rank: rank,
            bg_pixel: None,
            bg_pixmap: None,
            cursor: None,
        };
        self.windows.insert(cow_host_xid, geom);
        // The COW takes the pointer until its input region is emptied, so
        // crossings resolve it like any window (Nonlinear to a sibling).
        self.core.xid_map.insert(
            cow_host_xid,
            yserver_core::resources::COMPOSITE_OVERLAY_WINDOW,
        );
        self.deferred_cow_release = false;
        // Step 2 (DRIFT 2): the COW's place in top_level_order is no longer
        // set here — the GetOverlayWindow core handler reprojects from core
        // children via `sync_top_level_order` AFTER materialize_cow_resource
        // (which inserts the COW as a root child capped on top).
        self.scene.mark_scene_structure_dirty();
        Ok(true)
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
        if self.cow_id.is_none() {
            return Ok(false);
        }
        if self.scanout_m2.active() {
            // Do this while cow_id and its storage owner are authoritative.
            // Failure leaves the COW and the direct pins untouched, so core
            // can keep the caller's claim and let the compositor retry, or
            // fail safely without freeing a scanned buffer.
            self.materialize_direct_shadow_for_unflip()?;
            self.request_direct_unflip("release_last_overlay_window");
            self.deferred_cow_release = true;
            return Ok(true);
        }
        self.finish_cow_release();
        Ok(true)
    }

    fn cow_host_xid(&self) -> Option<u32> {
        // The COW's host xid is the well-known protocol xid once
        // get_overlay_window has materialized; None otherwise.
        if self.cow_id.is_some() {
            Some(yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0)
        } else {
            None
        }
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
        // Same body as v1. `KmsCore` already owns `FontLoader` +
        // `fonts` (it's protocol-bookkeeping per the v2 spec); the
        // backend just wraps the resulting freetype handle in a
        // `FontState` entry against a freshly-allocated xid.
        use std::cell::RefCell;

        use crate::kms::core::{FontState, FreetypeFace};
        let (face, metrics, char_cache) = self.core.font_loader.open_font(name)?;
        let host_xid = self.core.next_host_xid();
        let handle = FontHandle::from_raw(host_xid)
            .ok_or_else(|| io::Error::other("failed to create font handle"))?;
        self.core.fonts.insert(
            host_xid,
            FontState {
                handle: host_xid,
                face: RefCell::new(FreetypeFace(face)),
                metrics: metrics.clone(),
                char_info_cache: char_cache,
                glyph_span_cache: RefCell::new(HashMap::new()),
            },
        );
        Ok((handle, metrics))
    }

    fn close_font(&mut self, _origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        self.core.fonts.remove(&host_xid);
        // Core text keys its atlas entries by the font's host xid.
        self.engine.forget_glyphs(host_xid, None);
        Ok(())
    }

    fn set_font_path(
        &mut self,
        _origin: Option<OriginContext>,
        paths: &[String],
    ) -> Result<(), usize> {
        self.core
            .font_loader
            .set_font_path(paths)
            .map_err(|bad| paths.iter().position(|p| *p == bad).unwrap_or(paths.len()))
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
        // Stage 5 Phase A: real rasterisation. Read source + mask
        // (both depth-1 R8) via `engine.get_image`, lower to BGRA per
        // X11's mask/fore/back rule, allocate a CursorRecord + sprite
        // Pixmap. The cursor is invisible (size 1×1 transparent) if
        // the source pixmap can't be read — matches v1's degenerate
        // shape when the source mirror is missing.
        let xid = self.core.next_host_xid();
        let handle = CursorHandle::from_raw(xid)
            .ok_or_else(|| io::Error::other("create_cursor: xid was 0"))?;
        let (bgra_bytes, color_roles, w, h) = if let Some((src_bytes, w, h)) =
            self.read_cursor_depth1_pixmap(source_pixmap.as_raw())
        {
            let mask_bytes = mask_pixmap.and_then(|mp| {
                let (mb, mw, mh) = self.read_cursor_depth1_pixmap(mp.as_raw())?;
                if mw == w && mh == h {
                    Some(mb)
                } else {
                    log::warn!(
                        "render create_cursor: mask 0x{:x} dims {mw}x{mh} \
                             differ from src dims {w}x{h}; ignoring mask",
                        mp.as_raw(),
                    );
                    None
                }
            });
            let image = crate::kms::render::cursor::rasterise_create_cursor_with_roles(
                &src_bytes,
                w,
                h,
                mask_bytes.as_deref(),
                fore,
                back,
            );
            (image.bgra_bytes, image.color_roles, w, h)
        } else {
            log::warn!(
                "render create_cursor: source pixmap 0x{:x} unreadable; cursor invisible",
                source_pixmap.as_raw(),
            );
            (
                vec![0u8; 4],
                vec![crate::kms::render::cursor::CursorColorRole::Transparent],
                1u16,
                1u16,
            )
        };
        self.insert_monochrome_cursor_record(xid, w, h, hot_x, hot_y, bgra_bytes, color_roles);
        Ok(handle)
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
        // Stage 5 Phase A: real glyph-cursor rasterisation. Render
        // source + (optional) mask glyph via FreeType, build the
        // union bbox + derive the hotspot, then lower to BGRA via
        // the cursor module's shared rasteriser. Same code shape as
        // v1's `create_glyph_cursor` (`kms/backend.rs:9937-10108`).
        let xid = self.core.next_host_xid();
        let handle = CursorHandle::from_raw(xid)
            .ok_or_else(|| io::Error::other("create_glyph_cursor: xid was 0"))?;
        // Render both glyphs into owned Vec<u8>s up front so the
        // FreeType `bitmap()` borrow doesn't span the second
        // load_char call (FreeType invalidates the previous glyph's
        // bitmap when a new load_char fires).
        let src_xid = source_font.as_raw();
        let Some((src_pix, src_w, src_h, src_lsb, src_top)) =
            self.render_glyph_for_cursor(src_xid, source_char)
        else {
            log::warn!(
                "render create_glyph_cursor: source font 0x{src_xid:x} unknown; cursor invisible"
            );
            self.insert_monochrome_cursor_record(
                xid,
                1,
                1,
                0,
                0,
                vec![0u8; 4],
                vec![crate::kms::render::cursor::CursorColorRole::Transparent],
            );
            return Ok(handle);
        };
        let mask_data =
            mask_font.and_then(|mf| self.render_glyph_for_cursor(mf.as_raw(), mask_char));
        let src = crate::kms::render::cursor::GlyphBitmap {
            pixels: &src_pix,
            width: src_w,
            height: src_h,
            lsb: src_lsb,
            top: src_top,
        };
        let mask_bitmap = mask_data.as_ref().map(|(pix, w, h, lsb, top)| {
            crate::kms::render::cursor::GlyphBitmap {
                pixels: pix.as_slice(),
                width: *w,
                height: *h,
                lsb: *lsb,
                top: *top,
            }
        });
        let img = crate::kms::render::cursor::rasterise_glyph_cursor(
            &src,
            mask_bitmap.as_ref(),
            fore,
            back,
        );
        self.insert_monochrome_cursor_record(
            xid,
            img.width,
            img.height,
            img.hot_x,
            img.hot_y,
            img.bgra_bytes,
            img.color_roles,
        );
        Ok(handle)
    }

    fn recolor_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        fore: (u16, u16, u16),
        back: (u16, u16, u16),
    ) -> io::Result<()> {
        // RENDER animated cursors are wrappers around constituent cursors.
        // Xorg's AnimCurRecolorCursor forwards each frame with that frame's
        // own stored colors, so recoloring the wrapper does not replace frame
        // colors. Preserve that behavior rather than recoloring snapshots.
        if self.anim_cursor_records.contains_key(&host_xid) {
            return Ok(());
        }
        let Some(old) = self.cursor_records.get(&host_xid).cloned() else {
            return Ok(());
        };
        let Some(color_roles) = old.color_roles.clone() else {
            // Xorg explicitly ignores RecolorCursor for ARGB cursors.
            return Ok(());
        };
        let version = self.next_cursor_version;
        self.next_cursor_version = self.next_cursor_version.saturating_add(1);
        let record = crate::kms::render::cursor::CursorRecord::new_monochrome(
            old.width,
            old.height,
            old.hot_x,
            old.hot_y,
            color_roles,
            fore,
            back,
            version,
        );
        if let Some(&pixmap_id) = self.cursor_pixmaps.get(&host_xid) {
            self.engine
                .put_image(
                    &mut self.store,
                    &mut self.platform,
                    Dst::server_internal(pixmap_id),
                    ash::vk::Offset2D::default(),
                    ash::vk::Extent2D {
                        width: u32::from(record.width),
                        height: u32::from(record.height),
                    },
                    &record.bgra_bytes,
                    32,
                )
                .map_err(|error| io::Error::other(format!("recolor cursor upload: {error:?}")))?;
        }
        self.cursor_records.insert(host_xid, record);
        if self.effective_cursor_xid == Some(host_xid) {
            self.display_cursor_by_handle(host_xid);
        }
        Ok(())
    }

    fn create_anim_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        frames: &[(CursorHandle, u32)],
    ) -> io::Result<Option<CursorHandle>> {
        // Spec 2026-06-10-animated-cursors-design.md. Snapshot every
        // frame up front — no partial map state on failure.
        if frames.is_empty() {
            return Ok(None);
        }
        let mut snap = Vec::with_capacity(frames.len());
        for (h, delay_ms) in frames {
            let raw = h.as_raw();
            let Some(record) = self.cursor_records.get(&raw) else {
                return Err(io::Error::other(format!(
                    "create_anim_cursor: unknown sub-cursor handle 0x{raw:x}"
                )));
            };
            // Delay 0 → 16ms: a 0 deadline would busy-spin the
            // poll loop (explicit Xorg deviation, see spec).
            let ms = if *delay_ms == 0 { 16 } else { *delay_ms };
            snap.push(crate::kms::render::cursor::AnimFrame {
                source: raw,
                record: std::sync::Arc::clone(record),
                pixmap: self.cursor_pixmaps.get(&raw).copied(),
                delay: std::time::Duration::from_millis(u64::from(ms)),
            });
        }
        let xid = self.core.next_host_xid();
        let handle = CursorHandle::from_raw(xid)
            .ok_or_else(|| io::Error::other("create_anim_cursor: xid was 0"))?;
        // Alias frame 0 in the canonical maps so every static-cursor
        // code path (effective walk, XFixes, scene) works untouched.
        self.cursor_records
            .insert(xid, std::sync::Arc::clone(&snap[0].record));
        if let Some(p) = snap[0].pixmap {
            self.cursor_pixmaps.insert(xid, p);
        }
        self.anim_cursor_records.insert(
            xid,
            crate::kms::render::cursor::AnimCursorRecord { frames: snap },
        );
        Ok(Some(handle))
    }

    fn free_cursor(&mut self, _origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        // The core calls this when the last XID naming the cursor goes;
        // other references may still hold it (see `cursor_referenced`).
        if self.cursor_records.contains_key(&host_xid) {
            self.released_cursors.insert(host_xid);
            self.collect_released_cursors();
        }
        Ok(())
    }

    fn define_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        host_window_xid: u32,
        cursor_host_xid: u32,
    ) -> io::Result<()> {
        // Stage 5 Phase A: store the cursor on the window's
        // attribute slot. Per X11, the cursor visible on screen is
        // the one belonging to the deepest window under the pointer
        // that has a non-None cursor (walking up the parent chain);
        // `cursor_host_xid == 0` is X11 `None` and means "inherit
        // from parent".
        //
        // The sticky `active_cursor` fallback on `KmsCore` matches
        // v1: a DefineCursor on the root container becomes the
        // server-wide default for windows that don't override it.
        let nested = if cursor_host_xid == 0 {
            None
        } else {
            Some(cursor_host_xid)
        };
        if let Some(geom) = self.windows.get_mut(&host_window_xid) {
            geom.cursor = nested;
        }
        if cursor_host_xid != 0 && host_window_xid == self.core.window_id {
            self.core.active_cursor = Some(cursor_host_xid);
        }
        self.refresh_effective_cursor();
        self.collect_released_cursors();
        Ok(())
    }

    fn set_grab_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        cursor_host_xid: Option<u32>,
    ) -> io::Result<()> {
        // Xorg `ActivatePointerGrab`/`DeactivatePointerGrab`: install
        // the grab cursor as the top-priority sprite override, or clear
        // it when the grab ends. `refresh_effective_cursor` re-evaluates
        // the chain (now short-circuited by the override) and swaps the
        // scene `CursorEntry` when the displayed cursor changed.
        self.grab_cursor_override = cursor_host_xid;
        self.refresh_effective_cursor();
        self.collect_released_cursors();
        Ok(())
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
        // Body: drawable(4) + gc(4) + x(2) + y(2) + LISTofTEXTITEM8.
        // Each TEXTITEM8 is `len(u8) delta(i8) chars(len)` for len
        // in 0..=254, or `255 font_id(u32 BE)` for a font change.
        // No inter-item padding.
        if body.len() < 12 {
            return Ok(());
        }
        let x = i16::from_le_bytes([body[8], body[9]]) as i32;
        let y = i16::from_le_bytes([body[10], body[11]]) as i32;
        let mut items = &body[12..];
        let mut cursor_x = x;
        while items.len() >= 2 {
            let len = items[0];
            if len == 255 {
                if items.len() < 5 {
                    break;
                }
                let font_xid = u32::from_be_bytes([items[1], items[2], items[3], items[4]]);
                self.core.current_font = Some(font_xid);
                items = &items[5..];
                continue;
            }
            let delta = items[1] as i8;
            let len = len as usize;
            if items.len() < 2 + len {
                break;
            }
            let text = &items[2..2 + len];
            cursor_x = cursor_x.saturating_add(i32::from(delta));
            if !text.is_empty() {
                let chars: Vec<char> = text.iter().map(|&b| b as char).collect();
                self.render_text_chars(origin, host_xid, foreground, cursor_x, y, &chars)?;
                if let Some(font_state) =
                    self.core.current_font.and_then(|f| self.core.fonts.get(&f))
                {
                    cursor_x = cursor_x.saturating_add(text_advance(font_state, &chars));
                }
            }
            items = &items[2 + len..];
        }
        Ok(())
    }

    fn poly_text16(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        body: &[u8],
    ) -> io::Result<()> {
        // Body: drawable(4) + gc(4) + x(2) + y(2) + LISTofTEXTITEM16.
        // Each TEXTITEM16 is `len(u8) delta(i8) chars(2*len)` (chars
        // are CHAR2B, big-endian) for len in 0..=254, or `255
        // font_id(u32 BE)` for a font change.
        if body.len() < 12 {
            return Ok(());
        }
        let x = i16::from_le_bytes([body[8], body[9]]) as i32;
        let y = i16::from_le_bytes([body[10], body[11]]) as i32;
        let mut cursor_x = x;
        let mut items = &body[12..];
        while items.len() >= 2 {
            let len = items[0];
            if len == 255 {
                if items.len() < 5 {
                    break;
                }
                let font_xid = u32::from_be_bytes([items[1], items[2], items[3], items[4]]);
                self.core.current_font = Some(font_xid);
                items = &items[5..];
                continue;
            }
            let delta = items[1] as i8;
            let len = len as usize;
            let needed = 2 + 2 * len;
            if items.len() < needed {
                break;
            }
            cursor_x = cursor_x.saturating_add(i32::from(delta));
            let mut chars = Vec::with_capacity(len);
            for i in 0..len {
                let codepoint = u16::from_be_bytes([items[2 + 2 * i], items[2 + 2 * i + 1]]) as u32;
                chars.push(char::from_u32(codepoint).unwrap_or('\u{fffd}'));
            }
            if !chars.is_empty() {
                self.render_text_chars(origin, host_xid, foreground, cursor_x, y, &chars)?;
                if let Some(font_state) =
                    self.core.current_font.and_then(|f| self.core.fonts.get(&f))
                {
                    cursor_x = cursor_x.saturating_add(text_advance(font_state, &chars));
                }
            }
            items = &items[needed..];
        }
        Ok(())
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
        // Body: drawable(4) + gc(4) + x(2) + y(2) + string(text_len)
        if body.len() < 12 {
            return Ok(());
        }
        let x = i16::from_le_bytes([body[8], body[9]]) as i32;
        let y = i16::from_le_bytes([body[10], body[11]]) as i32;
        let end = (12usize + text_len as usize).min(body.len());
        let chars: Vec<char> = body[12..end].iter().map(|&b| b as char).collect();
        self.image_text_common(origin, host_xid, foreground, background, x, y, &chars)
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
        if body.len() < 12 {
            return Ok(());
        }
        let x = i16::from_le_bytes([body[8], body[9]]) as i32;
        let y = i16::from_le_bytes([body[10], body[11]]) as i32;
        let mut chars = Vec::with_capacity(text_len as usize);
        let mut pos = 12usize;
        for _ in 0..text_len {
            if pos + 2 > body.len() {
                break;
            }
            let codepoint = u16::from_be_bytes([body[pos], body[pos + 1]]) as u32;
            pos += 2;
            chars.push(char::from_u32(codepoint).unwrap_or('\u{fffd}'));
        }
        self.image_text_common(origin, host_xid, foreground, background, x, y, &chars)
    }

    // ── RENDER ──────────────────────────────────────────────────

    fn render_create_picture(
        &mut self,
        _origin: Option<OriginContext>,
        host_drawable: AnyHandle,
        ynest_format: u32,
        value_mask: u32,
        values: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        // Stage 3b: real picture record. Insert default
        // `PictureRecord::Drawable`, incref a PIXMAP backing in the
        // store (so a `free_pixmap` on the backing survives while this
        // picture wraps it — picture_record_drawable_refcount test),
        // then delegate to render_change_picture for the value-mask
        // body.
        let drawable_xid = host_drawable.as_raw();
        let picture_xid = self.core.next_host_xid();
        self.core.pictures.insert(
            picture_xid,
            PictureRecord::drawable_default(drawable_xid, ynest_format),
        );
        // A window Picture holds no store ref: every use resolves the window's current storage.
        if matches!(host_drawable, AnyHandle::Pixmap(_)) {
            if let Some(id) = self.store.lookup(drawable_xid) {
                self.store.incref(id);
                self.picture_drawable_ids.insert(picture_xid, id);
            } else {
                // Backing not materialized yet (GLX-TFP / Present /
                // DRI3 import). Defer the incref:
                // `apply_pending_picture_refs` pins the backing the
                // moment it materializes, so a later `free_pixmap`
                // can't reach refcount 0 and destroy the drawable out
                // from under this live Picture.
                self.pending_picture_drawable_refs
                    .insert(picture_xid, drawable_xid);
            }
        }
        if value_mask != 0 {
            // Recompose the body shape that render_change_picture
            // expects: picture(4) + value_mask(4) + values.
            let mut body = Vec::with_capacity(8 + values.len());
            body.extend_from_slice(&picture_xid.to_le_bytes());
            body.extend_from_slice(&value_mask.to_le_bytes());
            body.extend_from_slice(values);
            self.render_change_picture(None, picture_xid, &body)?;
        }
        Ok(PictureHandle::from_raw(picture_xid))
    }

    fn render_change_picture(
        &mut self,
        _origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()> {
        change_picture_apply_mask(&mut self.core, host_pic, body);
        Ok(())
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
        if let Some(PictureRecord::Drawable {
            drawable_origin, ..
        }) = self.core.pictures.get_mut(&host_pic)
        {
            *drawable_origin = origin;
        }
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
        let record = self.core.pictures.get(&host_pic)?;
        match record {
            PictureRecord::Drawable { clip, .. } => Some(clip.as_ref().map(|rects| {
                rects
                    .iter()
                    .map(|r| yserver_protocol::x11::xfixes::RegionRect {
                        x: r.x,
                        y: r.y,
                        width: r.width,
                        height: r.height,
                    })
                    .collect()
            })),
            PictureRecord::SolidFill { .. }
            | PictureRecord::LinearGradient { .. }
            | PictureRecord::RadialGradient { .. } => None,
        }
    }

    fn render_free_picture(
        &mut self,
        _origin: Option<OriginContext>,
        host_pic: u32,
    ) -> io::Result<()> {
        // Drop the record; if it was a Drawable variant, decref the
        // backing drawable in the store. SolidFill / Gradient
        // variants have no backing drawable — they own only the
        // GPU-side state on RenderEngine.picture_paint (Stage 3c).
        let retained_drawable_id = self.picture_drawable_ids.remove(&host_pic);
        if let Some(record) = self.core.pictures.remove(&host_pic)
            && record.drawable_host_xid().is_some()
        {
            if let Some(id) = retained_drawable_id {
                self.store_decref_with_invalidate(id);
            } else {
                // A window Picture, or a backing that never
                // materialized — no store ref was ever taken; just drop
                // any deferred ref request.
                self.pending_picture_drawable_refs.remove(&host_pic);
            }
        }
        // Drop any GPU-side state cached for this picture. Stage
        // 3b never populates the map (no gradient LUT built yet),
        // so this is a HashMap::remove no-op today; Stage 3c lazy-
        // builds gradient picture state through the same key, and
        // this teardown hook becomes load-bearing once that lands.
        self.engine.picture_paint_remove(host_pic);
        Ok(())
    }

    fn render_create_glyphset(
        &mut self,
        _origin: Option<OriginContext>,
        ynest_format: u32,
    ) -> io::Result<Option<GlyphSetHandle>> {
        use crate::kms::core::{GlyphSetFormat, GlyphSetState};

        let format = match ynest_format {
            RENDER_FMT_A8 => GlyphSetFormat::A8,
            RENDER_FMT_A1 => GlyphSetFormat::A1,
            RENDER_FMT_ARGB32 => GlyphSetFormat::Argb32,
            _ => GlyphSetFormat::Other,
        };
        let id = self.core.next_host_xid();
        self.core.glyphsets.insert(
            id,
            GlyphSetState {
                format,
                glyphs: HashMap::new(),
            },
        );
        Ok(GlyphSetHandle::from_raw(id))
    }

    fn render_free_glyphset(
        &mut self,
        _origin: Option<OriginContext>,
        host_gs: u32,
    ) -> io::Result<()> {
        // Host glyphset xids are never reused, but the atlas entries
        // would otherwise outlive the set until the next atlas reset.
        self.core.glyphsets.remove(&host_gs);
        self.engine.forget_glyphs(host_gs, None);
        Ok(())
    }

    fn render_add_glyphs(
        &mut self,
        _origin: Option<OriginContext>,
        host_gs: u32,
        body_tail: &[u8],
    ) -> io::Result<()> {
        // Reuses v1's parse_add_glyphs — purely CPU-side, operates
        // on the KmsCore.glyphsets entry. Atlas-side upload (the
        // Vk part) is Stage 3d's render_composite_glyphs path.
        let Some(gs) = self.core.glyphsets.get_mut(&host_gs) else {
            return Ok(());
        };
        // AddGlyphs over a live id replaces its image (Xorg's AddGlyph);
        // the atlas copy of the old one must go with it.
        let redefined: Vec<u32> = body_tail
            .get(..4)
            .map(|n| u32::from_le_bytes([n[0], n[1], n[2], n[3]]) as usize)
            .and_then(|n| body_tail.get(4..4 + n.checked_mul(4)?))
            .map(|ids| {
                ids.chunks_exact(4)
                    .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .filter(|id| gs.glyphs.contains_key(id))
                    .collect()
            })
            .unwrap_or_default();
        crate::kms::backend::parse_add_glyphs(gs, body_tail);
        if !redefined.is_empty() {
            self.engine.forget_glyphs(host_gs, Some(&redefined));
        }
        Ok(())
    }

    fn render_free_glyphs(
        &mut self,
        _origin: Option<OriginContext>,
        host_gs: u32,
        glyph_ids: &[u8],
    ) -> io::Result<()> {
        let Some(gs) = self.core.glyphsets.get_mut(&host_gs) else {
            return Ok(());
        };
        let ids: Vec<u32> = glyph_ids
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        for id in &ids {
            gs.glyphs.remove(id);
        }
        // A freed id can be added again with a different image.
        self.engine.forget_glyphs(host_gs, Some(&ids));
        Ok(())
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
        // A source or mask that is the destination would follow it onto
        // the inferiors; such a self-composite keeps to the window.
        let fanout = if host_src == host_dst || host_mask == host_dst {
            Vec::new()
        } else {
            self.include_inferiors_dst_fanout(host_dst)
        };
        if !fanout.is_empty() {
            self.dst_fanout_active = true;
            let mut painted = self.render_composite(
                _origin, op, host_src, host_mask, host_dst, src_x, src_y, mask_x, mask_y, dst_x,
                dst_y, width, height,
            );
            self.dst_fanout_active = false;
            for (window, (ox, oy), clip) in fanout {
                let (x, y) = (shift_i16(dst_x, -ox), shift_i16(dst_y, -oy));
                let more = self.with_dst_picture_on(host_dst, window, clip, |b| {
                    b.render_composite(
                        _origin, op, host_src, host_mask, host_dst, src_x, src_y, mask_x, mask_y,
                        x, y, width, height,
                    )
                });
                if let (Ok(painted), Some(Ok(more))) = (painted.as_mut(), more) {
                    painted.extend(more.into_iter().map(|r| xfixes::RegionRect {
                        x: shift_i16(r.x, ox),
                        y: shift_i16(r.y, oy),
                        ..r
                    }));
                }
            }
            return painted;
        }
        // Taken BEFORE the composite, because the substitution replaces the
        // source drawable entirely — but only when the request can paint at
        // all, so a zero-area Composite stays as free as it was before the
        // acquisition was hoisted out here.
        let inferiors_snapshot = if composite_needs_source_snapshot(width, height) {
            self.source_inferiors_snapshot(host_src)
        } else {
            None
        };
        let result = self.render_composite_inner(
            inferiors_snapshot,
            op,
            host_src,
            host_mask,
            host_dst,
            src_x,
            src_y,
            mask_x,
            mask_y,
            dst_x,
            dst_y,
            width,
            height,
        );
        if let Some(xid) = inferiors_snapshot {
            let _ = self.free_pixmap(None, xid);
        }
        result
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
        use crate::kms::render::{
            engine::{CompositeGlyphInput, ResolvedSource},
            glyph_pixels::GlyphPixels,
        };

        // Gating: op must be a standard fixed-function PictOp
        // (0..=12 — Clear..Add) and the src picture must be a
        // SolidFill. Saturate + the Disjoint/Conjoint families need
        // dst-readback shader blending the text pipeline doesn't
        // implement; those return Ok(()) with
        // `composite_glyphs_dropped_unsupported` bumped. The
        // standard family flows through per-op blend state derived
        // from `StdPictOp::blend_factors` — notably `Add` into a
        // depth-8 a8 mask pixmap, cairo/Pango's component-alpha
        // text path (the i3-config-wizard black-dialog bug).
        // `mask_fmt` is read but ignored: per-glyph compositing and
        // accumulate-into-maskFormat differ only for OVERLAPPING
        // glyph quads (and are identical for the associative `Add`);
        // true component-alpha glyphsets remain out of scope.
        // Unsupported-counter scope (plan §3d): the gate captures
        // *protocol-supported but engine-unimplemented* shapes —
        // op outside the standard family and source not SolidFill.
        // Stale src/dst picture handles and missing glyphsets are
        // protocol errors, not unsupported features; they log a gap
        // and return Ok without bumping the counter.
        if crate::kms::vk::render_pipeline::StdPictOp::from_u8(op).is_none() || op > 12 {
            self.record_composite_glyphs_drop(format_args!(
                "op={op} is outside the standard fixed-function family (0..=12)"
            ));
            return Ok(Vec::new());
        }
        let Some((src_resolved, src_repeat, _src_xform, _src_ca)) =
            self.resolve_picture_for_render(host_src)
        else {
            log::debug!("render composite_glyphs gap: src 0x{host_src:x} not resolvable");
            return Ok(Vec::new());
        };
        let foreground_premul = match src_resolved {
            ResolvedSource::Solid(c) => c,
            // Stage 3f.13: glyph paint path is still SolidFill-only
            // (matches v1's try_vk_render_composite_glyphs). For a
            // gradient source, collapse to first-stop premul — same
            // shape as the pre-3f.13 fallback, just scoped here
            // instead of in `resolve_picture_for_render`. No
            // counter bump: gradient-on-glyphs is now considered
            // "best effort handled" rather than "unsupported".
            ResolvedSource::Gradient(grad_xid) => {
                first_stop_premul_of_gradient(&self.core, grad_xid).unwrap_or_else(|| {
                    log::debug!(
                        "render composite_glyphs: gradient src 0x{grad_xid:x} \
                         has no stops — treating as transparent"
                    );
                    [0.0, 0.0, 0.0, 0.0]
                })
            }
            // #137 tier 1 — a source whose sampled domain is one pixel
            // under a covering repeat IS a colour, so read it and take
            // the same route as `CreateSolidFill`. Anything else stays
            // dropped: tier 2 (general drawable sources) needs its own
            // spec, and asserting otherwise here would assert tier 2.
            ResolvedSource::Drawable(src_drawable) => {
                match self.uniform_glyph_source_premul(host_src, src_drawable, src_repeat, mask_fmt)
                {
                    Ok(premul) => premul,
                    Err(why) => {
                        self.record_composite_glyphs_drop(format_args!(
                            "src 0x{host_src:x} is a drawable ({src_drawable:?} \
                             repeat={src_repeat:?} mask_fmt={mask_fmt}) — {why}"
                        ));
                        return Ok(Vec::new());
                    }
                }
            }
            ResolvedSource::None => {
                self.record_composite_glyphs_drop(format_args!(
                    "src 0x{host_src:x} resolved to no source picture at all"
                ));
                return Ok(Vec::new());
            }
        };
        let Some((dst_host_xid, dst_clip)) = resolve_dst_picture_for_render(&self.core, host_dst)
        else {
            log::debug!("render composite_glyphs gap: dst 0x{host_dst:x} not Drawable picture");
            return Ok(Vec::new());
        };
        // Stage 4a — resolve through redirect routing.
        let Some(dst_target) = self.resolve_paint_target(dst_host_xid) else {
            log::debug!(
                "render composite_glyphs gap: dst drawable 0x{dst_host_xid:x} not in store"
            );
            return Ok(Vec::new());
        };
        if !self.core.glyphsets.contains_key(&host_gs) {
            log::debug!("render composite_glyphs gap: glyphset 0x{host_gs:x} not registered");
            return Ok(Vec::new());
        }

        // Items parser. Pass 1 (`parse_composite_glyph_items`) walks
        // the wire stream and resolves every glyph it names, tagging
        // each with the picture format its glyphset stores it in;
        // pass 2 below resolves each of those to a
        // `CompositeGlyphInput` borrowing the glyphset's stored pixel
        // bytes as-is (dense A8, raw A1 wire or raw ARGB32 wire).
        // Conversion to A8 is deferred to the engine's atlas-miss
        // branch (`GlyphPixels::to_a8`) so a resident glyph is never
        // re-converted (#2, 2026-07-08 render-optimization gaps).
        // The two passes also keep the immutable
        // `self.core.glyphsets` borrow off the mutable `self.engine`
        // call below.
        //
        // Per X RENDER protocol, `src_x`/`src_y` are the SOURCE
        // picture sampling origin, not the dst pen — same as v1. The
        // first glyph-element's `dx` / `dy` sets the absolute pen
        // position; subsequent elements accumulate.
        let _ = (src_x, src_y);
        let ParsedGlyphItems {
            glyphs: parsed,
            elements,
            found: found_glyphs,
            missing: missing_glyphs,
        } = parse_composite_glyph_items(&self.core.glyphsets, minor, host_gs, x_off, y_off, items);

        if parsed.is_empty() {
            // No drawable glyphs (every entry was zero-size or
            // missing from the glyphset). Not a gap; just nothing
            // to record.
            log::debug!(
                "render composite_glyphs: NOTHING PARSED minor={minor} gs=0x{host_gs:x} \
                 items={} elements={elements} found={found_glyphs} missing={missing_glyphs} \
                 glyphs_in_set={}",
                items.len(),
                self.core
                    .glyphsets
                    .get(&host_gs)
                    .map_or(0, |g| g.glyphs.len()),
            );
            return Ok(Vec::new());
        }
        let mut min_x = i32::MAX;
        let mut min_y = i32::MAX;
        let mut max_x = i32::MIN;
        let mut max_y = i32::MIN;
        for p in &parsed {
            min_x = min_x.min(p.dst_x);
            min_y = min_y.min(p.dst_y);
            max_x = max_x.max(p.dst_x + i32::try_from(p.w).unwrap_or(0));
            max_y = max_y.max(p.dst_y + i32::try_from(p.h).unwrap_or(0));
        }
        let glyph_union_local = Rectangle16 {
            x: i16::try_from(min_x.max(0)).unwrap_or(i16::MAX),
            y: i16::try_from(min_y.max(0)).unwrap_or(i16::MAX),
            width: u16::try_from((max_x - min_x).max(0)).unwrap_or(u16::MAX),
            height: u16::try_from((max_y - min_y).max(0)).unwrap_or(u16::MAX),
        };
        let clip_by_children = dst_picture_clip_by_children(&self.core, host_dst);
        let dst_local_extent = self.dst_local_extent(dst_host_xid, dst_target.backing_id());
        let cliplist_local = self.render_dst_cliplist_local(
            dst_host_xid,
            clip_by_children,
            dst_clip.as_deref(),
            dst_local_extent,
            glyph_union_local,
        );
        if cliplist_local.is_empty() {
            log::debug!(
                "render composite_glyphs: CLIPPED OUT minor={minor} dst=0x{host_dst:x} glyphs={} union={:?} extent={:?} clip_by_children={clip_by_children}",
                parsed.len(),
                glyph_union_local,
                dst_local_extent,
            );
            return Ok(Vec::new());
        }
        let dst_clip =
            Self::shift_dst_picture_clip(Some(cliplist_local.clone()), dst_target.offset());

        // Pass 2: resolve each `Parsed` to a `CompositeGlyphInput`
        // with a stable slice reference. Stage 4a — apply the
        // dst-target offset to each glyph's dst coordinates so a
        // redirected window's glyphs land in the backing.
        let (paint_dx, paint_dy) = dst_target.offset();
        let inputs: Vec<CompositeGlyphInput<'_>> = parsed
            .iter()
            .filter_map(|p| {
                let stored = self
                    .core
                    .glyphsets
                    .get(&p.gs_xid)
                    .and_then(|gs| gs.glyphs.get(&p.glyph_id))
                    .map(|g| g.pixels.as_slice())?;
                // The byte encoding IS the format tag: the engine
                // reads it back with `GlyphPixels::source_format()`,
                // so it can never be told "these bytes are ARGB32"
                // and "this glyph is A8" at the same time.
                let pixels = match p.source_format {
                    GlyphSourceFormat::A8 => GlyphPixels::A8(stored),
                    GlyphSourceFormat::A1 => GlyphPixels::A1Wire(stored),
                    GlyphSourceFormat::Argb32 => GlyphPixels::Argb32Wire(stored),
                };
                Some(CompositeGlyphInput {
                    gs_xid: p.gs_xid,
                    glyph_id: p.glyph_id,
                    w: p.w,
                    h: p.h,
                    pixels,
                    dst_x: p.dst_x + paint_dx,
                    dst_y: p.dst_y + paint_dy,
                })
            })
            .collect();

        if inputs.is_empty() {
            log::debug!(
                "render composite_glyphs: NO PIXELS minor={minor} dst=0x{host_dst:x} parsed={}",
                parsed.len(),
            );
            return Ok(Vec::new());
        }

        // Dst PictFormat ID — the engine classifies the dst's alpha
        // semantics from it (`dst_has_alpha_for_pict_format`), same
        // as the general render_composite path. 0 for unknown xids;
        // the engine then falls back to the depth heuristic.
        let dst_pict_format = picture_pict_format(&self.core, host_dst);
        let stats = self.engine.composite_glyphs(
            &mut self.store,
            &mut self.platform,
            dst_target.dst(),
            op,
            dst_pict_format,
            foreground_premul,
            &inputs,
            dst_clip.as_deref(),
        );
        match stats {
            Ok(s) => {
                if s.atlas_interns > 0 {
                    for _ in 0..s.atlas_interns {
                        self.telemetry.record_atlas_intern();
                    }
                }
                if s.glyph_uploads > 0 {
                    for _ in 0..s.glyph_uploads {
                        self.telemetry.record_glyph_upload();
                    }
                    // One GlyphUpload event per upload submit
                    // (paired with the text-paint CB that
                    // follows). `dst_target.backing_id()` is the eventual
                    // destination — keep on the dst so analysis
                    // can correlate uploads with the dst's text
                    // bursts.
                    let target_kind = self.submit_target_kind(dst_target.backing_id());
                    for _ in 0..s.glyph_uploads {
                        self.telemetry.record_submit_event(SubmitEvent {
                            frame_id: 0,
                            kind: SubmitKind::GlyphUpload,
                            target_kind,
                            target_id: dst_target.backing_id().as_u64(),
                            batch_size: 1,
                            op: SubmitOp::None,
                            src_class: SrcClass::None,
                            mask_class: SrcClass::None,
                            pipeline_id: None,
                            flags: SubmitFlags {
                                readback: false,
                                alias: false,
                                zero_draws: false,
                                upload: true,
                            },
                        });
                    }
                }
                if s.glyphs_dropped > 0 {
                    for _ in 0..s.glyphs_dropped {
                        self.telemetry.record_glyph_dropped_atlas_full();
                    }
                }
                if s.atlas_interns > 0 || !inputs.is_empty() {
                    // Successful composite_glyphs counts as one
                    // paint submit (mirroring `image_text` /
                    // `render_composite` telemetry shape).
                    self.telemetry.record_paint_submit();
                    let glyph_count = u32::try_from(inputs.len()).unwrap_or(u32::MAX);
                    self.trace_render(
                        SubmitKind::CompositeGlyphs,
                        dst_target.backing_id(),
                        glyph_count,
                        op, // wire PictOp (standard family 0..=12, gated above)
                        SrcClass::Solid,
                        None,
                        SubmitFlags::NONE,
                    );
                }
            }
            Err(e) => {
                log::warn!("render composite_glyphs: engine returned {e:?} on dst 0x{host_dst:x}");
            }
        }
        // Phase B.1 Task 21: composite_glyphs may open a frame;
        // drain any resulting close events into telemetry.
        self.drain_frame_builder_telemetry();
        Ok(local_rects_to_region(cliplist_local))
    }

    fn picture_includes_inferiors(&self, host_pic: u32) -> bool {
        !dst_picture_clip_by_children(&self.core, host_pic)
            && matches!(
                self.core.pictures.get(&host_pic),
                Some(PictureRecord::Drawable { .. })
            )
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
        let fanout = self.include_inferiors_dst_fanout(host_dst);
        if !fanout.is_empty() {
            self.dst_fanout_active = true;
            let result =
                self.render_fill_rectangles(_origin, host_dst, op, color, rects, x_off, y_off);
            self.dst_fanout_active = false;
            for (window, (ox, oy), clip) in fanout {
                let (x, y) = (shift_i16(x_off, -ox), shift_i16(y_off, -oy));
                self.with_dst_picture_on(host_dst, window, clip, |b| {
                    b.render_fill_rectangles(_origin, host_dst, op, color, rects, x, y)
                });
            }
            return result;
        }
        let Some((dst_host_xid, dst_clip)) = resolve_dst_picture_for_render(&self.core, host_dst)
        else {
            log::debug!(
                "render render_fill_rectangles gap: host_dst 0x{host_dst:x} not a Drawable picture"
            );
            return Ok(());
        };
        // Stage 4a — redirect routing for dst.
        let Some(dst_target) = self.resolve_paint_target(dst_host_xid) else {
            log::debug!(
                "render render_fill_rectangles gap: dst drawable 0x{dst_host_xid:x} not in store"
            );
            return Ok(());
        };
        let dst_clip = Self::shift_dst_picture_clip(dst_clip, dst_target.offset());
        let dst_clip = self.narrow_dst_clip_to_shared_backing(dst_host_xid, &dst_target, dst_clip);
        let (paint_dx, paint_dy) = dst_target.offset();

        // X RENDER XRenderColor is wire-premultiplied (rendercheck
        // main.c:337-345); pass through unchanged.
        let color_premul = [
            f32::from(u16::from_le_bytes([color[0], color[1]])) / 65535.0,
            f32::from(u16::from_le_bytes([color[2], color[3]])) / 65535.0,
            f32::from(u16::from_le_bytes([color[4], color[5]])) / 65535.0,
            f32::from(u16::from_le_bytes([color[6], color[7]])) / 65535.0,
        ];

        let mut decoded: Vec<crate::kms::vk::ops::render::CompositeRect> =
            Vec::with_capacity(rects.len() / 8);
        for chunk in rects.chunks_exact(8) {
            let rx = i16::from_le_bytes([chunk[0], chunk[1]]).saturating_add(x_off);
            let ry = i16::from_le_bytes([chunk[2], chunk[3]]).saturating_add(y_off);
            let rw = u16::from_le_bytes([chunk[4], chunk[5]]);
            let rh = u16::from_le_bytes([chunk[6], chunk[7]]);
            if rw == 0 || rh == 0 {
                continue;
            }
            decoded.push(crate::kms::vk::ops::render::CompositeRect {
                src_x: 0,
                src_y: 0,
                mask_x: 0,
                mask_y: 0,
                dst_x: i32::from(rx) + paint_dx,
                dst_y: i32::from(ry) + paint_dy,
                width: u32::from(rw),
                height: u32::from(rh),
            });
        }
        if decoded.is_empty() {
            return Ok(());
        }

        let stats = self.engine.render_fill_rectangles(
            &mut self.store,
            &mut self.platform,
            op,
            color_premul,
            dst_target.dst(),
            &decoded,
            dst_clip.as_deref(),
        );
        self.sync_descriptor_pool_telemetry();
        let n_rects = u32::try_from(decoded.len()).unwrap_or(u32::MAX);
        if let Ok(s) = stats {
            if s.recorded_draws > 0 {
                self.telemetry.record_paint_submit();
                self.trace_render(
                    SubmitKind::RenderFill,
                    dst_target.backing_id(),
                    n_rects,
                    op,
                    SrcClass::Solid,
                    None,
                    SubmitFlags {
                        readback: s.used_dst_readback,
                        alias: s.used_src_alias_scratch,
                        zero_draws: false,
                        upload: false,
                    },
                );
            }
            if s.used_dst_readback {
                self.telemetry.record_disjoint_readback();
            }
        } else if let Err(e) = stats {
            log::warn!(
                "render render_fill_rectangles: engine returned {e:?} on dst 0x{host_dst:x}"
            );
        }
        // Phase B.2 Task 15: render_fill_rectangles may open a frame;
        // drain any resulting close events into telemetry so the
        // per-second emit picks them up without stale lag. Mirrors the
        // B.1 drain at the composite_glyphs wrapper.
        self.drain_frame_builder_telemetry();
        Ok(())
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
        use crate::kms::{render::engine::TrapPrimKind, vk::ops::traps as vk_traps};

        // Wire layout: each trapezoid is 40 bytes (10 × i32 16.16
        // fixed-point). Mirrors v1's try_vk_render_trapezoids_path
        // decoder (kms/backend.rs:4286).
        if traps.is_empty() {
            return Ok(Vec::new());
        }
        let n_traps = traps.len() / 40;
        if n_traps == 0 {
            return Ok(Vec::new());
        }
        let mut decoded: Vec<vk_traps::Trapezoid> = Vec::with_capacity(n_traps);
        for chunk in traps.chunks_exact(40) {
            let read_i32 = |o: usize| -> i32 {
                i32::from_le_bytes([chunk[o], chunk[o + 1], chunk[o + 2], chunk[o + 3]])
            };
            decoded.push(vk_traps::Trapezoid {
                top: read_i32(0),
                bottom: read_i32(4),
                left_p1: (read_i32(8), read_i32(12)),
                left_p2: (read_i32(16), read_i32(20)),
                right_p1: (read_i32(24), read_i32(28)),
                right_p2: (read_i32(32), read_i32(36)),
            });
        }
        // Xorg's `fbTrapezoids` (fb/fbtrap.c:164-165) subtracts the
        // first trapezoid's `left.p1` from xSrc/ySrc before forwarding
        // to pixman. This anchors the src origin at the first trap's
        // top-left, regardless of where the trap is in dst space. For
        // GTK CSD shadows (which pass `xSrc=20 ySrc=-25` for the BR
        // corner with `traps[0].left.p1 = (20, -25)`), the subtraction
        // resolves to src=(0,0) → no out-of-bounds sampling. Without
        // it, REPEAT_NONE returns transparent for the OOB rows and the
        // corner shadow has an 8-row α=0 gap.
        // Captured pre-shift; the dx/dy fold below moves the live trap
        // coords into the redirect-target space, but the *adjustment*
        // is from the client-supplied geometry.
        let first_trap_left_p1_x = decoded[0].left_p1.0 >> 16;
        let first_trap_left_p1_y = decoded[0].left_p1.1 >> 16;
        // Resolve src + dst via the same helpers render_composite
        // uses. The trap path doesn't read GC clip — picture clip
        // (from dst) is what scopes the draw (plan §4).
        let Some((src_resolved, src_repeat, src_transform, _src_ca)) =
            self.resolve_picture_for_render(host_src)
        else {
            log::debug!("render render_trapezoids gap: src 0x{host_src:x} not resolvable");
            return Ok(Vec::new());
        };
        let Some((dst_host_xid, dst_clip)) = resolve_dst_picture_for_render(&self.core, host_dst)
        else {
            log::debug!("render render_trapezoids gap: dst 0x{host_dst:x} not Drawable picture");
            return Ok(Vec::new());
        };
        // Stage 4a — redirect routing for dst. The fold of
        // `x_off`/`y_off` and the redirect offset (both in pixel
        // units) into a single fixed-point delta keeps the
        // 16.16-arithmetic single-pass.
        let Some(dst_target) = self.resolve_paint_target(dst_host_xid) else {
            log::debug!(
                "render render_trapezoids gap: dst drawable 0x{dst_host_xid:x} not in store"
            );
            return Ok(Vec::new());
        };
        let dx = (i32::from(x_off) + dst_target.offset().0) << 16;
        let dy = (i32::from(y_off) + dst_target.offset().1) << 16;
        if dx != 0 || dy != 0 {
            for t in &mut decoded {
                t.top = t.top.wrapping_add(dy);
                t.bottom = t.bottom.wrapping_add(dy);
                t.left_p1.0 = t.left_p1.0.wrapping_add(dx);
                t.left_p1.1 = t.left_p1.1.wrapping_add(dy);
                t.left_p2.0 = t.left_p2.0.wrapping_add(dx);
                t.left_p2.1 = t.left_p2.1.wrapping_add(dy);
                t.right_p1.0 = t.right_p1.0.wrapping_add(dx);
                t.right_p1.1 = t.right_p1.1.wrapping_add(dy);
                t.right_p2.0 = t.right_p2.0.wrapping_add(dx);
                t.right_p2.1 = t.right_p2.1.wrapping_add(dy);
            }
        }
        let Some((bx, by, bx1, by1)) = vk_traps::trapezoid_bbox(&decoded) else {
            return Ok(Vec::new());
        };
        let bx = bx.max(0);
        let by = by.max(0);
        if bx1 <= bx || by1 <= by {
            return Ok(Vec::new());
        }
        #[allow(clippy::cast_sign_loss)]
        let bw = (bx1 - bx) as u32;
        #[allow(clippy::cast_sign_loss)]
        let bh = (by1 - by) as u32;
        let bbox_local = Rectangle16 {
            x: i16::try_from((bx - dst_target.offset().0).max(0)).unwrap_or(i16::MAX),
            y: i16::try_from((by - dst_target.offset().1).max(0)).unwrap_or(i16::MAX),
            width: u16::try_from(bw).unwrap_or(u16::MAX),
            height: u16::try_from(bh).unwrap_or(u16::MAX),
        };
        let clip_by_children = dst_picture_clip_by_children(&self.core, host_dst);
        let dst_local_extent = self.dst_local_extent(dst_host_xid, dst_target.backing_id());
        let cliplist_local = self.render_dst_cliplist_local(
            dst_host_xid,
            clip_by_children,
            dst_clip.as_deref(),
            dst_local_extent,
            bbox_local,
        );
        if cliplist_local.is_empty() {
            return Ok(Vec::new());
        }
        let dst_clip =
            Self::shift_dst_picture_clip(Some(cliplist_local.clone()), dst_target.offset());
        let Some((bx, by, bw, bh)) =
            clip_trap_bbox_to_extents((bx, by, bx1, by1), dst_clip.as_deref().unwrap_or(&[]))
        else {
            return Ok(Vec::new());
        };

        // Pack instance bytes (40 bytes per trap; no padding —
        // asserted by `const _:()` in trap_pipeline.rs).
        let stride = std::mem::size_of::<crate::kms::vk::trap_pipeline::TrapInstanceData>();
        let mut instance_bytes = vec![0u8; stride * decoded.len()];
        for (i, t) in decoded.iter().enumerate() {
            let inst = t.to_instance_data();
            instance_bytes[i * stride..(i + 1) * stride].copy_from_slice(inst.as_bytes());
        }

        // Audit #4 (2026-05-19) — same pict_format threading as
        // render_composite. Trap/tri paint into an xRGB32 dst on
        // depth-32 storage must drive "no alpha target," and
        // xRGB32 sources must pin α=ONE on the sample view.
        let src_pict_format = picture_pict_format(&self.core, host_src);
        let dst_pict_format = picture_pict_format(&self.core, host_dst);
        // Source origin in src-pixel space. Two adjustments stacked:
        //   - subtract `(x_off + redirect_offset)` to undo the dx/dy
        //     fold applied to the trap coords above;
        //   - subtract `traps[0].left.p1.{x,y}` to mirror Xorg's
        //     `fbTrapezoids` pixman pre-step (fb/fbtrap.c:164-165) —
        //     anchors src @ (0,0) at the first trap's top-left.
        // The emit folds in bbox for the non-full-dst branch.
        let src_origin_x =
            i32::from(src_x) - (i32::from(x_off) + dst_target.offset().0) - first_trap_left_p1_x;
        let src_origin_y =
            i32::from(src_y) - (i32::from(y_off) + dst_target.offset().1) - first_trap_left_p1_y;
        let stats = self.engine.render_traps_or_tris(
            &mut self.store,
            &mut self.platform,
            op,
            src_resolved,
            dst_target.dst(),
            TrapPrimKind::Trapezoid,
            &instance_bytes,
            #[allow(clippy::cast_possible_truncation)]
            {
                decoded.len() as u32
            },
            (bx, by, bw, bh),
            dst_clip.as_deref(),
            src_repeat,
            src_transform,
            src_origin_x,
            src_origin_y,
            src_pict_format,
            dst_pict_format,
        );
        self.sync_descriptor_pool_telemetry();
        let src_class = self.picture_src_class_by_xid(host_src);
        let n_traps = u32::try_from(decoded.len()).unwrap_or(u32::MAX);
        if let Ok(s) = stats {
            if s.recorded_draws > 0 {
                self.telemetry.record_paint_submit();
                self.trace_render(
                    SubmitKind::RenderTraps,
                    dst_target.backing_id(),
                    n_traps,
                    op,
                    src_class,
                    None,
                    SubmitFlags {
                        readback: s.used_dst_readback,
                        alias: s.used_src_alias_scratch,
                        zero_draws: false,
                        upload: false,
                    },
                );
            }
            if s.used_dst_readback {
                self.telemetry.record_disjoint_readback();
            }
        } else if let Err(e) = stats {
            log::warn!("render render_trapezoids: engine returned {e:?}");
        }
        Ok(local_rects_to_region(cliplist_local))
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
        use crate::kms::{render::engine::TrapPrimKind, vk::ops::traps as vk_traps};

        let read_point = |off: usize, chunk: &[u8]| -> (i32, i32) {
            let x =
                i32::from_le_bytes([chunk[off], chunk[off + 1], chunk[off + 2], chunk[off + 3]]);
            let y = i32::from_le_bytes([
                chunk[off + 4],
                chunk[off + 5],
                chunk[off + 6],
                chunk[off + 7],
            ]);
            (x, y)
        };
        let mut tris: Vec<vk_traps::Triangle> = match minor {
            11 => {
                if !primitives.len().is_multiple_of(24) {
                    return Ok(Vec::new());
                }
                primitives
                    .chunks_exact(24)
                    .map(|c| vk_traps::Triangle {
                        p1: read_point(0, c),
                        p2: read_point(8, c),
                        p3: read_point(16, c),
                    })
                    .collect()
            }
            12 => {
                if !primitives.len().is_multiple_of(8) || primitives.len() < 24 {
                    return Ok(Vec::new());
                }
                let pts: Vec<(i32, i32)> = primitives
                    .chunks_exact(8)
                    .map(|c| read_point(0, c))
                    .collect();
                (0..pts.len() - 2)
                    .map(|i| vk_traps::Triangle {
                        p1: pts[i],
                        p2: pts[i + 1],
                        p3: pts[i + 2],
                    })
                    .collect()
            }
            13 => {
                if !primitives.len().is_multiple_of(8) || primitives.len() < 24 {
                    return Ok(Vec::new());
                }
                let pts: Vec<(i32, i32)> = primitives
                    .chunks_exact(8)
                    .map(|c| read_point(0, c))
                    .collect();
                (1..pts.len() - 1)
                    .map(|i| vk_traps::Triangle {
                        p1: pts[0],
                        p2: pts[i],
                        p3: pts[i + 1],
                    })
                    .collect()
            }
            _ => return Ok(Vec::new()),
        };
        if tris.is_empty() {
            return Ok(Vec::new());
        }
        let Some((src_resolved, src_repeat, src_transform, _src_ca)) =
            self.resolve_picture_for_render(host_src)
        else {
            log::debug!("render render_triangles gap: src 0x{host_src:x} not resolvable");
            return Ok(Vec::new());
        };
        let Some((dst_host_xid, dst_clip)) = resolve_dst_picture_for_render(&self.core, host_dst)
        else {
            log::debug!("render render_triangles gap: dst 0x{host_dst:x} not Drawable picture");
            return Ok(Vec::new());
        };
        // Stage 4a — redirect routing for dst; fold the redirect
        // offset into the same fixed-point delta as `x_off/y_off`.
        let Some(dst_target) = self.resolve_paint_target(dst_host_xid) else {
            log::debug!(
                "render render_triangles gap: dst drawable 0x{dst_host_xid:x} not in store"
            );
            return Ok(Vec::new());
        };
        let dx = (i32::from(x_off) + dst_target.offset().0) << 16;
        let dy = (i32::from(y_off) + dst_target.offset().1) << 16;
        if dx != 0 || dy != 0 {
            for t in &mut tris {
                t.p1.0 = t.p1.0.wrapping_add(dx);
                t.p1.1 = t.p1.1.wrapping_add(dy);
                t.p2.0 = t.p2.0.wrapping_add(dx);
                t.p2.1 = t.p2.1.wrapping_add(dy);
                t.p3.0 = t.p3.0.wrapping_add(dx);
                t.p3.1 = t.p3.1.wrapping_add(dy);
            }
        }
        let Some((bx, by, bx1, by1)) = vk_traps::triangle_bbox(&tris) else {
            return Ok(Vec::new());
        };
        let bx = bx.max(0);
        let by = by.max(0);
        if bx1 <= bx || by1 <= by {
            return Ok(Vec::new());
        }
        #[allow(clippy::cast_sign_loss)]
        let bw = (bx1 - bx) as u32;
        #[allow(clippy::cast_sign_loss)]
        let bh = (by1 - by) as u32;
        let bbox_local = Rectangle16 {
            x: i16::try_from((bx - dst_target.offset().0).max(0)).unwrap_or(i16::MAX),
            y: i16::try_from((by - dst_target.offset().1).max(0)).unwrap_or(i16::MAX),
            width: u16::try_from(bw).unwrap_or(u16::MAX),
            height: u16::try_from(bh).unwrap_or(u16::MAX),
        };
        let clip_by_children = dst_picture_clip_by_children(&self.core, host_dst);
        let dst_local_extent = self.dst_local_extent(dst_host_xid, dst_target.backing_id());
        let cliplist_local = self.render_dst_cliplist_local(
            dst_host_xid,
            clip_by_children,
            dst_clip.as_deref(),
            dst_local_extent,
            bbox_local,
        );
        if cliplist_local.is_empty() {
            return Ok(Vec::new());
        }
        let dst_clip =
            Self::shift_dst_picture_clip(Some(cliplist_local.clone()), dst_target.offset());
        let Some((bx, by, bw, bh)) =
            clip_trap_bbox_to_extents((bx, by, bx1, by1), dst_clip.as_deref().unwrap_or(&[]))
        else {
            return Ok(Vec::new());
        };

        let stride = std::mem::size_of::<crate::kms::vk::trap_pipeline::TriangleInstanceData>();
        let mut instance_bytes = vec![0u8; stride * tris.len()];
        for (i, t) in tris.iter().enumerate() {
            let inst = t.to_instance_data();
            instance_bytes[i * stride..(i + 1) * stride].copy_from_slice(inst.as_bytes());
        }

        // Audit #4 (2026-05-19) — same pict_format threading as
        // the trapezoid path; see that call site for rationale.
        let src_pict_format = picture_pict_format(&self.core, host_src);
        let dst_pict_format = picture_pict_format(&self.core, host_dst);
        // Source origin shifted by the same delta the triangle coords
        // were (x_off + redirect offset). Also subtract the first
        // triangle's `p1.{x,y}` to mirror Xorg's `fbTriangles` pixman
        // pre-step (fb/fbtrap.c:179-180) — anchors src @ (0,0) at the
        // first triangle's p1 regardless of where it sits in dst space.
        let first_tri_p1_x = tris[0].p1.0 >> 16;
        let first_tri_p1_y = tris[0].p1.1 >> 16;
        let src_origin_x =
            i32::from(src_x) - (i32::from(x_off) + dst_target.offset().0) - first_tri_p1_x;
        let src_origin_y =
            i32::from(src_y) - (i32::from(y_off) + dst_target.offset().1) - first_tri_p1_y;
        let stats = self.engine.render_traps_or_tris(
            &mut self.store,
            &mut self.platform,
            op,
            src_resolved,
            dst_target.dst(),
            TrapPrimKind::Triangle,
            &instance_bytes,
            #[allow(clippy::cast_possible_truncation)]
            {
                tris.len() as u32
            },
            (bx, by, bw, bh),
            dst_clip.as_deref(),
            src_repeat,
            src_transform,
            src_origin_x,
            src_origin_y,
            src_pict_format,
            dst_pict_format,
        );
        self.sync_descriptor_pool_telemetry();
        let src_class = self.picture_src_class_by_xid(host_src);
        let n_tris = u32::try_from(tris.len()).unwrap_or(u32::MAX);
        if let Ok(s) = stats {
            if s.recorded_draws > 0 {
                self.telemetry.record_paint_submit();
                self.trace_render(
                    SubmitKind::RenderTris,
                    dst_target.backing_id(),
                    n_tris,
                    op,
                    src_class,
                    None,
                    SubmitFlags {
                        readback: s.used_dst_readback,
                        alias: s.used_src_alias_scratch,
                        zero_draws: false,
                        upload: false,
                    },
                );
            }
            if s.used_dst_readback {
                self.telemetry.record_disjoint_readback();
            }
        } else if let Err(e) = stats {
            log::warn!("render render_triangles: engine returned {e:?}");
        }
        Ok(local_rects_to_region(cliplist_local))
    }

    fn render_create_solid_fill(
        &mut self,
        _origin: Option<OriginContext>,
        color: [u8; 8],
    ) -> io::Result<Option<PictureHandle>> {
        // X RENDER CreateSolidFill: 16-bit-per-channel colour,
        // little-endian, already premultiplied on the wire (per
        // rendercheck main.c:337-345). Store the channels as f32
        // exactly as received — the pipeline samples them
        // unchanged. Layout: r[0..2] g[2..4] b[4..6] a[6..8].
        let r16 = u16::from_le_bytes([color[0], color[1]]);
        let g16 = u16::from_le_bytes([color[2], color[3]]);
        let b16 = u16::from_le_bytes([color[4], color[5]]);
        let a16 = u16::from_le_bytes([color[6], color[7]]);
        let premul = [
            f32::from(r16) / 65535.0,
            f32::from(g16) / 65535.0,
            f32::from(b16) / 65535.0,
            f32::from(a16) / 65535.0,
        ];
        let picture_xid = self.core.next_host_xid();
        self.core.pictures.insert(
            picture_xid,
            PictureRecord::SolidFill {
                premul,
                repeat: Repeat::Normal,
                component_alpha: false,
            },
        );
        Ok(PictureHandle::from_raw(picture_xid))
    }

    fn render_create_linear_gradient(
        &mut self,
        _origin: Option<OriginContext>,
        body: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        // Wire body: p1.x(4) + p1.y(4) + p2.x(4) + p2.y(4) +
        // n_stops(4) + n × stop_pos(4) + n × stop_color(8).
        // Caller passes only the request payload from offset 4 —
        // the first u32 is interpreted as p1.x (sliced at body[4..]).
        if body.len() < 24 {
            return Ok(None);
        }
        let p1x = i32::from_le_bytes(body[4..8].try_into().unwrap());
        let p1y = i32::from_le_bytes(body[8..12].try_into().unwrap());
        let p2x = i32::from_le_bytes(body[12..16].try_into().unwrap());
        let p2y = i32::from_le_bytes(body[16..20].try_into().unwrap());
        let Some(stops) = parse_gradient_stops(body, 20) else {
            return Ok(None);
        };
        let picture_xid = self.core.next_host_xid();
        // Stage 3f.13: build the LUT eagerly so the first
        // render_composite against this picture has it ready. The
        // record + the engine's GradientPicture have parallel
        // lifetimes — render_free_picture drops both. Build
        // failure (no Vk on test fixture, or allocation error) is
        // non-fatal: the record still lands; render_composite
        // logs a gap if it can't find the LUT. This keeps the
        // logic-test fixture (no live Vk) usable without forcing
        // every gradient-create test through lavapipe.
        let engine_stops: Vec<crate::kms::vk::gradient::Stop> = stops
            .iter()
            .map(|s| crate::kms::vk::gradient::Stop {
                pos: s.pos,
                r: s.r,
                g: s.g,
                b: s.b,
                a: s.a,
            })
            .collect();
        if let Err(e) = self.engine.build_and_insert_linear_gradient(
            &mut self.platform,
            picture_xid,
            (p1x, p1y),
            (p2x, p2y),
            &engine_stops,
        ) {
            log::debug!(
                "render render_create_linear_gradient: engine build failed (xid=0x{picture_xid:x}): \
                 {e:?} — record stored; paint will fall back to gap-log"
            );
        }
        self.core.pictures.insert(
            picture_xid,
            PictureRecord::LinearGradient {
                p1: (p1x, p1y),
                p2: (p2x, p2y),
                stops,
                repeat: Repeat::None,
                transform: None,
            },
        );
        Ok(PictureHandle::from_raw(picture_xid))
    }

    fn render_create_radial_gradient(
        &mut self,
        _origin: Option<OriginContext>,
        body: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        // Wire body: icx(4) icy(4) ocx(4) ocy(4) ir(4) or(4)
        // n_stops(4) + stops + colors. Same offset-by-4 convention
        // as linear (first u32 in `body` is past the request header).
        if body.len() < 32 {
            return Ok(None);
        }
        let icx = i32::from_le_bytes(body[4..8].try_into().unwrap());
        let icy = i32::from_le_bytes(body[8..12].try_into().unwrap());
        let ocx = i32::from_le_bytes(body[12..16].try_into().unwrap());
        let ocy = i32::from_le_bytes(body[16..20].try_into().unwrap());
        let ir = i32::from_le_bytes(body[20..24].try_into().unwrap());
        let or_ = i32::from_le_bytes(body[24..28].try_into().unwrap());
        let Some(stops) = parse_gradient_stops(body, 28) else {
            return Ok(None);
        };
        let picture_xid = self.core.next_host_xid();
        // Stage 3f.13: build the radial LUT (256×256 BGRA) eagerly.
        // See `render_create_linear_gradient` for failure-mode
        // rationale.
        let engine_stops: Vec<crate::kms::vk::gradient::Stop> = stops
            .iter()
            .map(|s| crate::kms::vk::gradient::Stop {
                pos: s.pos,
                r: s.r,
                g: s.g,
                b: s.b,
                a: s.a,
            })
            .collect();
        if let Err(e) = self.engine.build_and_insert_radial_gradient(
            &mut self.platform,
            picture_xid,
            (icx, icy, ir),
            (ocx, ocy, or_),
            &engine_stops,
        ) {
            log::debug!(
                "render render_create_radial_gradient: engine build failed (xid=0x{picture_xid:x}): \
                 {e:?} — record stored; paint will fall back to gap-log"
            );
        }
        self.core.pictures.insert(
            picture_xid,
            PictureRecord::RadialGradient {
                inner: (icx, icy, ir),
                outer: (ocx, ocy, or_),
                stops,
                repeat: Repeat::None,
                transform: None,
            },
        );
        Ok(PictureHandle::from_raw(picture_xid))
    }

    fn render_create_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        host_src_pic: PictureHandle,
        x: u16,
        y: u16,
    ) -> io::Result<Option<CursorHandle>> {
        // Stage 5 Phase A: themed/ARGB cursor — Picture wraps a
        // depth-32 BGRA Pixmap. Read the pixmap bytes via
        // `engine.get_image`, allocate a CursorRecord + sprite, and
        // mint a cursor xid. fvwm pattern (CreatePixmap → PutImage
        // → CreatePicture → FreePixmap → CreateCursor) means the
        // backing pixmap may already be gone by the time we arrive,
        // but for Stage 5 we rely on the picture record's
        // `host_xid` resolving back to a live store entry — alias-
        // registry-aware rescue is a follow-up.
        let pic_xid = host_src_pic.as_raw();
        let src_host_xid = match self.core.pictures.get(&pic_xid) {
            Some(crate::kms::core::PictureRecord::Drawable { host_xid, .. }) => *host_xid,
            other => {
                log::debug!(
                    "render render_create_cursor: pic 0x{pic_xid:x} not Drawable (got {:?})",
                    other.map(|_| "non-Drawable"),
                );
                return Ok(None);
            }
        };
        let Some((bgra, w, h)) = self.read_cursor_bgra_pixmap(src_host_xid) else {
            log::debug!(
                "render render_create_cursor: src pixmap 0x{src_host_xid:x} unreadable for pic 0x{pic_xid:x}",
            );
            return Ok(None);
        };
        let xid = self.core.next_host_xid();
        let handle = CursorHandle::from_raw(xid);
        self.insert_cursor_record(xid, w, h, x, y, bgra);
        Ok(handle)
    }

    fn render_set_picture_clip_rectangles(
        &mut self,
        _origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()> {
        // Wire body: picture(4) + clip_x_origin(INT16) +
        // clip_y_origin(INT16) + N × [x y w h]. Pre-shift each
        // rectangle by the clip-origin so the stored list is in
        // dst-coords; the per-rect scissoring path in Stage 3c
        // doesn't track origin separately.
        if body.len() < 8 {
            return Ok(());
        }
        let x_origin = i16::from_le_bytes([body[4], body[5]]) as i32;
        let y_origin = i16::from_le_bytes([body[6], body[7]]) as i32;
        let rects_data = &body[8..];
        let mut rects = Vec::with_capacity(rects_data.len() / 8);
        for chunk in rects_data.chunks_exact(8) {
            let x = (i16::from_le_bytes([chunk[0], chunk[1]]) as i32 + x_origin)
                .clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            let y = (i16::from_le_bytes([chunk[2], chunk[3]]) as i32 + y_origin)
                .clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            let w = u16::from_le_bytes([chunk[4], chunk[5]]);
            let h = u16::from_le_bytes([chunk[6], chunk[7]]);
            rects.push(Rectangle16 {
                x,
                y,
                width: w,
                height: h,
            });
        }
        if let Some(PictureRecord::Drawable {
            clip,
            clip_x,
            clip_y,
            ..
        }) = self.core.pictures.get_mut(&host_pic)
        {
            // X11 RENDER spec semantics:
            //   - `SetPictureClipRectangles` with EMPTY rect list =
            //     empty clip region = composites paint **nothing**.
            //   - `ChangePicture(CPClipMask = None)` clears the clip
            //     back to "no clip" = paint **everywhere** (`clip = None`).
            // The previous implementation collapsed both to None;
            // that broke marco-with-compositing because marco uses
            // the empty-list form between frames as a "stop
            // painting until I set a real clip again" gate. With
            // the buggy collapse, the wallpaper-fill composite
            // that should have been clipped to nothing painted
            // everywhere and overwrote the just-drawn window
            // contents — the Stage 4d "shadow only" symptom.
            *clip = Some(rects);
            // The X RENDER protocol carries clip-origin once per
            // SetPictureClipRectangles; we fold it into the stored
            // rects (above) but also keep clip_x/clip_y so a
            // subsequent CPClipXOrigin / CPClipYOrigin override
            // via ChangePicture composes correctly.
            *clip_x = x_origin.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            *clip_y = y_origin.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        }
        // SolidFill / Gradient pictures: clip is a no-op (no
        // backing drawable to clip).
        Ok(())
    }

    fn render_set_picture_filter(
        &mut self,
        _origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()> {
        // Wire body: picture(4) + name_len(u16) + pad(2) + name +
        // pad + N × FIXED(4) parameters. Stage 3 only honours
        // `nearest`; other filters parse + store so the record-
        // round-trip is honest but `RenderEngine` ignores them at
        // draw time (per Risk 6).
        if body.len() < 8 {
            return Ok(());
        }
        let name_len = u16::from_le_bytes([body[4], body[5]]) as usize;
        if body.len() < 8 + name_len {
            return Ok(());
        }
        let name = &body[8..8 + name_len];
        let filter = match name {
            b"nearest" | b"fast" => PictureFilter::Nearest,
            b"bilinear" | b"good" | b"best" => PictureFilter::Bilinear,
            b"convolution" => PictureFilter::Convolution,
            _ => PictureFilter::Nearest,
        };
        if let Some(PictureRecord::Drawable { filter: f, .. }) =
            self.core.pictures.get_mut(&host_pic)
        {
            *f = filter;
        }
        Ok(())
    }

    fn render_set_picture_transform(
        &mut self,
        _origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()> {
        // Wire body: picture(4) + 9 × FIXED(4) matrix entries (row-
        // major). 16.16 fixed-point; identity is [[1,0,0],[0,1,0],
        // [0,0,1]] in floating shape, [[0x10000, 0, 0], [0, 0x10000,
        // 0], [0, 0, 0x10000]] in fixed.
        if body.len() < 40 {
            return Ok(());
        }
        let mut matrix = [[0i32; 3]; 3];
        for (idx, slot) in matrix.iter_mut().flatten().enumerate() {
            let off = 4 + idx * 4;
            *slot = i32::from_le_bytes(body[off..off + 4].try_into().unwrap());
        }
        let transform = if matrix == [[0x10000, 0, 0], [0, 0x10000, 0], [0, 0, 0x10000]] {
            None
        } else {
            Some(PictTransform { matrix })
        };
        match self.core.pictures.get_mut(&host_pic) {
            Some(PictureRecord::Drawable { transform: t, .. })
            | Some(PictureRecord::LinearGradient { transform: t, .. })
            | Some(PictureRecord::RadialGradient { transform: t, .. }) => *t = transform,
            _ => {}
        }
        Ok(())
    }

    fn render_query_version(&mut self, _origin: Option<OriginContext>) -> io::Result<(u32, u32)> {
        // Advertise RENDER 0.11 (the version v1 reports). Stubbed
        // paint paths still need the version reply to flow through;
        // skipping it would break clients at extension query.
        Ok((0, 11))
    }

    // ── DRI3 — ported from v1 (Stage 4d backfill) ───────────────
    //
    // Body shape mirrors `kms/backend.rs:8613-8869` verbatim — the
    // helpers in `kms::vk::dri3`, `kms::vk::sync`, `kms::render_node`,
    // and `kms::xshmfence` are already shared with v1, so v2 calls
    // them directly. Without these, no compositor (marco, xfwm4,
    // picom, compton) can import redirected window backings as GPU
    // textures and the 4d-close hardware smoke wedges on
    // PresentPixmap → COW.

    fn dri3_open(&mut self, _drawable: u32) -> io::Result<std::os::fd::OwnedFd> {
        // Open a fresh fd at the render-node path per client. dup()'ing
        // a shared long-lived fd would give every client the same
        // kernel struct file, and libdrm_amdgpu maintains GEM handles
        // + contexts in per-struct-file state — the first client
        // populates it, the second crashes in `amdgpu_winsys_create`
        // hitting leftover handles. See
        // feedback_dri3_open_fresh_fd.md.
        let render_node = self
            .platform
            .selected_render_device()
            .and_then(|device| device.render_node.as_ref())
            .ok_or_else(|| {
                io::Error::other("DRI3 unavailable — render node was not resolved at backend init")
            })?;
        render_node.open_fresh().map_err(|e| {
            io::Error::other(format!(
                "open render-node {}: {e}",
                render_node.path().display()
            ))
        })
    }

    fn dri3_capabilities(&self) -> Dri3Caps {
        // DRI3 entirely unavailable when the render-node device or Vulkan
        // weren't resolved at backend init: pixmap import/export still needs
        // both. `render_node_device` is the guard here because it is what the syncobj ioctls and the
        // capability query run on.
        let Some(renderer) = self.platform.selected_render_device() else {
            return Dri3Caps::unsupported();
        };
        if renderer.render_node_device.is_none() || self.platform.vk.is_none() {
            return Dri3Caps::unsupported();
        }
        let vk = self.platform.vk.as_ref().expect("vk Some by branch above");
        // PixmapFromBuffer carries a client-chosen stride. The implicit-linear
        // import path validates it against the exact Vulkan layout per buffer;
        // a second PRIME renderer is therefore not a reason to hide DRI3.
        if !dri3_import_supported_for_topology(renderer.id, self.platform.devices.len()) {
            return Dri3Caps::unsupported();
        }
        let modifiers = vk.image_drm_format_modifier;
        // FenceFromFD / FDFromFence need SYNC_FD semaphore import/export.
        let fence_fd = vk.supports_sync_fd();
        // Syncobj support is a property of the KERNEL, not of the Vulkan
        // driver. The previous NVIDIA blacklist here was a correct response
        // to vkImportSemaphoreFdKHR rejecting DRM syncobj fds, which no
        // longer matters because nothing imports them into Vulkan.
        let syncobj = renderer.syncobj_timeline;
        Dri3Caps {
            version: dri3_version_for(syncobj),
            modifiers,
            fence_fd,
            syncobj,
        }
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
        // Per Phase 4.2 design §3.2: import the dma-buf into a
        // DrawableImage via VK_EXT_image_drm_format_modifier, wrap
        // it as a v2 Storage, allocate a fresh Pixmap entry in
        // the store. Pixmap exists as a real X resource so clients
        // can CopyArea / ChangePicture against it.
        let Some(vk) = self.platform.vk.clone() else {
            return Err(io::Error::other("DRI3 import: Vulkan unavailable"));
        };
        let format = match (depth, bpp) {
            (24 | 32, 32) => ash::vk::Format::B8G8R8A8_UNORM,
            _ => {
                return Err(io::Error::other(format!(
                    "DRI3 import: unsupported (depth={depth}, bpp={bpp}); Phase 4.2 RGB single-plane only"
                )));
            }
        };
        // Vulkan's modifier import is explicit-only, so an implicit
        // layout has to be resolved to a concrete modifier first. gbm
        // does that the way glamor does it for the same request
        // (`gbm_bo_import(GBM_BO_IMPORT_FD)` then `gbm_bo_get_modifier`,
        // ../xserver/glamor/glamor_egl.c:572 and :450).
        //
        // On failure this returns Err rather than falling back to
        // LINEAR. Guessing is what #138 was: a wrong *successful*
        // import corrupts the client's own output and reports success,
        // where a refused one is visible and debuggable.
        // What we tell clients later, which is the half that matters:
        // `DRM_FORMAT_MOD_INVALID` means "layout not named", and a client
        // re-importing on that answer resolves it itself (EGL and GL have
        // an implicit dma-buf import path; Vulkan does not). Claiming
        // LINEAR instead is #138 -- the client believes us and samples a
        // tiled buffer as linear.
        //
        // We cannot do better than "unknown" here: gbm reports
        // DRM_FORMAT_MOD_INVALID for an implicitly imported buffer on
        // amdgpu -- measured, and it does so even for gbm's own fresh
        // allocation -- so there is nothing to resolve against. i915 does
        // report a concrete modifier, but a fix that only works on Intel
        // is not a fix.
        let (vk_modifier, reported_modifier, client_size, implicit_layout) = match modifier {
            Dri3ImportModifier::Explicit(m) => (m, Some(m), None, false),
            // LINEAR is a best-effort for OUR OWN Vulkan view of the
            // buffer, which is only ever sampled if the server itself
            // composites this pixmap. It is deliberately NOT what we
            // report back.
            Dri3ImportModifier::Implicit { size } => (
                crate::kms::vk::dri3::DRM_FORMAT_MOD_LINEAR,
                Some(crate::kms::vk::dri3::DRM_FORMAT_MOD_INVALID),
                Some(size),
                true,
            ),
        };
        let modifier = vk_modifier;
        let drawable = crate::kms::vk::dri3::import_dmabuf_reporting(
            vk.clone(),
            fd,
            u32::from(width),
            u32::from(height),
            format,
            modifier,
            reported_modifier,
            client_size,
            &[crate::kms::vk::dri3::DmabufPlane {
                offset: u64::from(offset),
                pitch: stride,
            }],
        )
        .map_err(|e| io::Error::other(format!("DRI3 import_dmabuf: {e:?}")))?;
        // Build a sample-side view over the imported VkImage. The
        // DRI3 path's own `vk_image_view` (kept as `image_view` on
        // the resulting Storage) is IDENTITY-swizzle and serves as
        // the attachment view; the sample-side view applies the
        // format/depth-aware swizzle the scene compositor relies on
        // (depth-24 BGRA8 → α=ONE).
        let sample_view = crate::kms::render::platform::PlatformBackend::build_sample_view(
            &vk,
            drawable.vk_image,
            drawable.format,
            depth,
        )
        .map_err(|e| io::Error::other(format!("DRI3 import build_sample_view: {e:?}")))?;
        let fourcc = match depth {
            24 => u32::from_le_bytes(*b"XR24"),
            32 => u32::from_le_bytes(*b"AR24"),
            _ => unreachable!("format match above restricts imported depth"),
        };
        let storage = Storage::from_imported_drawable_image(
            drawable,
            sample_view,
            depth,
            ImportedDmabufMetadata {
                fourcc,
                vk_format: format,
                modifier,
                implicit_layout,
                planes: vec![ImportedDmabufPlane {
                    offset: u64::from(offset),
                    pitch: stride,
                }],
                width,
                height,
                depth,
                bpp,
            },
        );
        let host_xid = self.core.next_host_xid();
        self.store_alloc(host_xid, DrawableKind::Pixmap, depth, false, storage)
            .map_err(|e| io::Error::other(format!("DRI3 import store.allocate: {e:?}")))?;
        // Telemetry: an imported pixmap is still a fresh storage
        // entry + a view (the DrawableImage built one inside
        // from_dmabuf). Mirrors init_root_storage's accounting so
        // the per-second counters stay accurate under DRI3 traffic.
        self.telemetry.record_storage_allocation();
        self.telemetry.record_image_view_create();
        PixmapHandle::from_raw(host_xid)
            .ok_or_else(|| io::Error::other("DRI3 import: failed to make PixmapHandle"))
    }

    fn dri3_supported_modifiers(&self, _window: u32, depth: u8, bpp: u8) -> (Vec<u64>, Vec<u64>) {
        let Some(vk) = self.platform.vk.as_ref() else {
            return (vec![0], vec![0]);
        };
        // Map (depth, bpp) to a vk::Format. Phase 4.2 RGB single-
        // plane scope means we only handle depth-24/32 BGRA today.
        let format = match (depth, bpp) {
            (24 | 32, 32) => ash::vk::Format::B8G8R8A8_UNORM,
            _ => return (vec![0], vec![0]),
        };
        // Client pixmaps are composited as sampled window textures, so
        // probe with the sampled client-import usage (keeps SAMPLED, which
        // correctly steers v3dv clients to a tiled modifier).
        let probed = crate::kms::vk::dri3::supported_modifiers_with_planes(
            vk,
            format,
            crate::kms::vk::dri3::CLIENT_IMPORT_USAGE,
        );
        let screen: Vec<u64> = probed.iter().map(|(m, _)| *m).collect();
        // The window list is the SINGLE-PLANE subset, not just LINEAR.
        //
        // `PixmapFromBuffers` import handles one plane today, so a
        // multi-plane layout offered here is one we then refuse with
        // BadAlloc: Mesa logs `dri3_alloc_render_buffer ... failed` and
        // the window renders nothing at all. On this AMD part three of
        // the six tiled modifiers carry DCC and need two planes (three
        // when retiled), which is what makes the naive "advertise
        // everything" version blank every GL client.
        //
        // Collapsing to LINEAR is wrong in the other direction: it is
        // not what the question means once a window is composited
        // rather than flipped, and composited is our default. Xorg
        // answers with the tiled set here too.
        //
        // This is NOT a fix for #138, and was briefly believed to be.
        // Offering the tiled single-plane set left that bug exactly as
        // it was: Chrome's hardware-decoded video arrives over
        // EGL/dma-buf from VA-API and never travels this path. Do not
        // reintroduce that claim.
        //
        // Plane count comes from `drmFormatModifierPlaneCount`, not from
        // decoding modifier bits, so the filter tracks whatever the
        // driver actually reports.
        let window: Vec<u64> = probed
            .iter()
            .filter(|(_, planes)| *planes == 1)
            .map(|(m, _)| *m)
            .collect();
        (window, screen)
    }

    fn dri3_export_pixmap(
        &mut self,
        host_xid: u32,
    ) -> io::Result<(u32, u16, u16, u16, u8, u8, std::os::fd::OwnedFd)> {
        // op 3 is the single-fd, no-modifier subset of op 8. Share the
        // promote+export path and drop the modifier/offset (op 3's reply
        // has no field for them). stride truncates CARD32 → CARD16.
        let e = self.dri3_export_pixmap_buffers(host_xid)?;
        Ok((
            e.size,
            e.width,
            e.height,
            u16::try_from(e.stride).unwrap_or(u16::MAX),
            e.depth,
            e.bpp,
            e.fd,
        ))
    }

    fn dri3_export_pixmap_buffers(&mut self, host_xid: u32) -> io::Result<Dri3PixmapExport> {
        // Resolve xid → DrawableId before any mutable borrows.
        let id = self.store.lookup(host_xid).ok_or_else(|| {
            io::Error::other(format!("DRI3 export: unknown pixmap 0x{host_xid:x}"))
        })?;

        // Promote-if-needed: migrate server-owned pixmaps onto dma-buf-exportable
        // storage (glamor model). Idempotent — early-returns if already exportable.
        if !self
            .store
            .get(id)
            .map(|d| d.storage.is_exportable())
            .unwrap_or(false)
        {
            // promote_drawable_exportable needs &mut self.engine/platform/store,
            // so we must not hold any shared borrow across this call.
            if self.platform.vk.is_none() {
                return Err(io::Error::other("DRI3 export: Vulkan unavailable"));
            }
            self.engine
                .promote_drawable_exportable(&mut self.platform, &mut self.store, id)
                .map_err(|e| io::Error::other(format!("DRI3 export promote: {e:?}")))?;
        }

        // Re-fetch after promotion (storage has been swapped).
        let vk = self
            .platform
            .vk
            .as_ref()
            .ok_or_else(|| io::Error::other("DRI3 export: Vulkan unavailable"))?;
        let drawable = self.store.get(id).ok_or_else(|| {
            io::Error::other(format!("DRI3 export: store entry missing 0x{host_xid:x}"))
        })?;

        let (depth, width, height) = self
            .core
            .alias_registry
            .get(PixmapHandle::from_raw_panicking(host_xid))
            .map(|alias| (alias.depth, alias.width, alias.height))
            .unwrap_or_else(|| {
                (
                    drawable.depth,
                    u16::try_from(drawable.storage.extent.width).unwrap_or(u16::MAX),
                    u16::try_from(drawable.storage.extent.height).unwrap_or(u16::MAX),
                )
            });
        let bpp: u8 = match depth {
            24 | 32 => 32,
            4 | 8 => 8,
            d => d,
        };

        // Export: imported images go through the DrawableImage path; promoted /
        // server-owned images use export_promoted on the storage's raw memory
        // handle + stride/size carried from allocation-time layout query.
        let export = if let Some(imported) = drawable.storage.imported_drawable.as_ref() {
            crate::kms::vk::dri3::export_dmabuf(vk, imported)
                .map_err(|e| io::Error::other(format!("DRI3 export_dmabuf: {e:?}")))?
        } else {
            debug_assert!(
                drawable.storage.export_stride != 0 && drawable.storage.export_size != 0,
                "promoted storage missing export metadata (stride={} size={})",
                drawable.storage.export_stride,
                drawable.storage.export_size,
            );
            crate::kms::vk::dri3::export_promoted(
                vk,
                drawable.storage.memory,
                drawable.storage.export_stride,
                drawable.storage.export_size,
                drawable.storage.export_modifier,
            )
            .map_err(|e| io::Error::other(format!("DRI3 export_promoted: {e:?}")))?
        };

        // GLX-TFP (Tasks 2.3 + 2.4): record/refresh the export tracking
        // entry. Repeated exports (muffin re-exports per damage) reuse the
        // existing entry and its single lifetime ref — the fd dup + sync
        // tracking install only on the FIRST export. Always return the
        // ORIGINAL fd to the client.
        let backing = PixmapHandle::from_raw(host_xid)
            .ok_or_else(|| io::Error::other(format!("DRI3 export: bad xid 0x{host_xid:x}")))?;
        if self
            .exported_dmabufs
            .get(&id)
            .is_none_or(|e| e.fd.is_none())
        {
            let dup = export.fd.try_clone()?;
            // Parallel sync-only dup for the engine flush chokepoint.
            let sync_dup = std::sync::Arc::new(dup.try_clone()?);
            self.ensure_exported_entry(id, backing).fd = Some(dup);
            self.store.set_exported_sync_fd(id, sync_dup);
        }

        Ok(Dri3PixmapExport {
            size: export.size,
            width,
            height,
            stride: export.stride,
            offset: export.offset,
            depth,
            bpp,
            modifier: export.modifier,
            fd: export.fd,
        })
    }

    fn dri3_fence_from_fd(&mut self, fence_xid: u32, fd: std::os::fd::OwnedFd) -> io::Result<()> {
        // Mesa's loader_dri3 sends an xshmfence (memfd + futex) —
        // try that path FIRST. vkImportSemaphoreFdKHR rejects
        // xshmfence fds because they aren't sync_file. Mmap first;
        // fall through to Vulkan import only if mmap fails (i.e.
        // the fd really is a sync_file).
        use std::os::fd::AsFd as _;
        if let Some(mapping) = crate::kms::xshmfence::FenceMapping::map(fd.as_fd()) {
            self.dri3_xshmfences
                .insert(fence_xid, std::sync::Arc::new(mapping));
            log::debug!("DRI3 FenceFromFD 0x{fence_xid:x}: imported as xshmfence");
            return Ok(());
        }
        let Some(vk) = self.platform.vk.as_ref() else {
            return Err(io::Error::other(
                "DRI3 FenceFromFD: fd isn't xshmfence and Vulkan is unavailable",
            ));
        };
        let semaphore = crate::kms::vk::sync::import_sync_file(vk, fd)
            .map_err(|e| io::Error::other(format!("import_sync_file: {e:?}")))?;
        let owned = std::sync::Arc::new(crate::kms::render::owned_semaphore::OwnedSemaphore::new(
            vk.clone(),
            semaphore,
        ));
        // Replacing an entry drops the previous Arc here; if no other
        // clone is outstanding, OwnedSemaphore::Drop calls
        // vkDestroySemaphore.
        let _ = self.dri3_sync_resources.insert(fence_xid, owned);
        Ok(())
    }

    fn dri3_trigger_fence(&mut self, fence_xid: u32) -> io::Result<()> {
        if let Some(mapping) = self.dri3_xshmfences.get(&fence_xid) {
            mapping.trigger();
            return Ok(());
        }
        // VkSemaphore-backed fences: signalling is done via queue
        // submit. (DRI3 1.4 syncobjs are the other path — DRM objects
        // signalled by the `SYNCOBJ_TIMELINE_SIGNAL` ioctls, never
        // Vulkan.) For Phase 4.2 first-cut Copy path the GPU work is
        // already serialized, so a server-only `triggered=true` mirror
        // is sufficient — no GPU operation needed here.
        Ok(())
    }

    fn dri3_fence_triggered(&self, fence_xid: u32) -> Option<bool> {
        self.dri3_xshmfences
            .get(&fence_xid)
            .map(|mapping| mapping.query() != 0)
    }

    fn dri3_reset_fence(&mut self, fence_xid: u32) {
        if let Some(mapping) = self.dri3_xshmfences.get(&fence_xid) {
            mapping.reset();
        }
    }

    fn dri3_destroy_fence(&mut self, fence_xid: u32) {
        // Xorg `miSyncShmScreenDestroyFence`: trigger, then unmap. A
        // deferred Present completion holding an Arc clone keeps the
        // mapping alive until it lets go.
        if let Some(mapping) = self.dri3_xshmfences.remove(&fence_xid) {
            mapping.trigger();
        }
        self.dri3_sync_resources.remove(&fence_xid);
    }

    fn dri3_xshmfence_handle(
        &self,
        fence_xid: u32,
    ) -> Option<std::sync::Arc<dyn yserver_core::backend::XshmfenceHandle>> {
        self.dri3_xshmfences
            .get(&fence_xid)
            .cloned()
            .map(|arc| arc as std::sync::Arc<dyn yserver_core::backend::XshmfenceHandle>)
    }

    fn dri3_syncobj_handle(
        &self,
        syncobj_xid: u32,
    ) -> Option<std::sync::Arc<dyn yserver_core::backend::SyncobjHandle>> {
        self.dri3_syncobjs
            .get(&syncobj_xid)
            .map(|(_, arc)| arc.clone())
            .map(|arc| arc as std::sync::Arc<dyn yserver_core::backend::SyncobjHandle>)
    }

    fn dri3_syncobj_owned(
        &self,
        client_id: yserver_protocol::x11::ClientId,
        syncobj_xid: u32,
    ) -> bool {
        self.dri3_syncobjs
            .get(&syncobj_xid)
            .is_some_and(|(owner, _)| *owner == client_id)
    }

    fn dri3_fd_from_fence(&mut self, fence_xid: u32) -> io::Result<std::os::fd::OwnedFd> {
        let arc = self
            .dri3_sync_resources
            .get(&fence_xid)
            .cloned()
            .ok_or_else(|| {
                io::Error::other(format!("DRI3 FDFromFence: unknown fence 0x{fence_xid:x}"))
            })?;
        let Some(vk) = self.platform.vk.as_ref() else {
            return Err(io::Error::other("DRI3 FDFromFence: Vulkan unavailable"));
        };
        crate::kms::vk::sync::export_sync_file(vk, arc.semaphore())
            .map_err(|e| io::Error::other(format!("export_sync_file: {e:?}")))
    }

    fn dri3_import_syncobj(
        &mut self,
        client_id: yserver_protocol::x11::ClientId,
        syncobj_xid: u32,
        fd: std::os::fd::OwnedFd,
    ) -> io::Result<()> {
        use std::os::fd::AsFd;

        let render_node = self
            .platform
            .selected_render_device()
            .and_then(|device| device.render_node_device.as_ref())
            .cloned()
            .ok_or_else(|| {
                io::Error::other("DRI3 ImportSyncobj: render node not resolved at init")
            })?;
        if self.dri3_syncobjs.contains_key(&syncobj_xid) {
            return Err(io::Error::other(format!(
                "DRI3 ImportSyncobj: syncobj 0x{syncobj_xid:x} already imported"
            )));
        }
        let imported =
            crate::kms::render::imported_syncobj::ImportedSyncobj::import(render_node, fd.as_fd())?;
        // Arc Drop on any replaced entry destroys the previous handle.
        let _ = self
            .dri3_syncobjs
            .insert(syncobj_xid, (client_id, std::sync::Arc::new(imported)));
        Ok(())
    }

    fn dri3_free_syncobj(
        &mut self,
        client_id: yserver_protocol::x11::ClientId,
        syncobj_xid: u32,
    ) -> io::Result<()> {
        let Some((owner, _)) = self.dri3_syncobjs.get(&syncobj_xid) else {
            return Err(io::Error::other(format!(
                "DRI3 FreeSyncobj: unknown syncobj 0x{syncobj_xid:x}"
            )));
        };
        if *owner != client_id {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("DRI3 FreeSyncobj: 0x{syncobj_xid:x} owned by another client"),
            ));
        }
        // Arc Drop destroys the DRM handle when the last reference goes away,
        // which may be later than this call: the deferred completion path
        // pins clones past FreeSyncobj.
        let _ = self.dri3_syncobjs.remove(&syncobj_xid);
        Ok(())
    }

    fn dri3_signal_syncobj(&mut self, syncobj_xid: u32, value: u64) -> io::Result<()> {
        use yserver_core::backend::SyncobjHandle as _;

        let arc = self
            .dri3_syncobjs
            .get(&syncobj_xid)
            .map(|(_, arc)| arc)
            .ok_or_else(|| {
                io::Error::other(format!(
                    "DRI3 SignalSyncobj: unknown syncobj 0x{syncobj_xid:x}"
                ))
            })?;
        arc.signal(value)
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
    fn drain_completed_present_events(
        &mut self,
    ) -> Vec<yserver_core::backend::CompletedPresentEvent> {
        let mut completed = self.drain_completed_present_events_impl();
        completed.append(&mut self.scanout_m2.completed);
        completed
    }

    fn drain_retired_present_idle_events(
        &mut self,
    ) -> Vec<yserver_core::backend::CompletedPresentEvent> {
        std::mem::take(&mut self.scanout_m2.idled)
    }

    fn signal_present_wake(&mut self, present_id: u64) {
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

    fn present_crtc_clock_epoch(&self, crtc_id: u32) -> u64 {
        let Some(crtc_key) = self.present_crtc_key(crtc_id) else {
            return 0;
        };
        self.present_crtc_clock_epochs
            .get(&crtc_id)
            .filter(|(epoch_key, _)| *epoch_key == crtc_key)
            .map_or(0, |(_, epoch)| *epoch)
    }

    fn present_get_ust_msc(&self, crtc_id: u32) -> (u64, u64) {
        self.present_crtc_key(crtc_id).map_or((0, 0), |crtc_key| {
            self.platform.present_get_ust_msc(crtc_key)
        })
    }

    fn present_get_completion_clock(
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

    fn arm_idle_vblanks(&mut self, crtc_id: u32, target_mscs: &[u64]) -> std::io::Result<usize> {
        self.arm_idle_vblanks_ioctl(crtc_id, target_mscs)
    }

    fn arm_present_completion_idle_vblanks(
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

    fn present_capabilities(&self, _window: u32) -> PresentCaps {
        // Mirror v1's conservative "Copy-path only" caps. syncobj
        // tracks Dri3Caps::syncobj. flip_path / async_may_tear stay
        // false until alien-BO scanout integration lands on v2.
        PresentCaps {
            flip_path: false,
            async_may_tear: false,
            syncobj: self.dri3_capabilities().syncobj,
        }
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
        // Stage 5 — unblock protocol-audit #14 (`GetCursorImage`
        // returns 0×0). Source the bytes from the
        // currently-effective `Arc<CursorRecord>` and stamp the
        // current root-space pointer position.
        let xid = self.effective_cursor_xid?;
        let record = self.cursor_records.get(&xid)?;
        #[allow(clippy::cast_possible_truncation)]
        let x = self.core.cursor_x as i16;
        #[allow(clippy::cast_possible_truncation)]
        let y = self.core.cursor_y as i16;
        Some(yserver_core::backend::ActiveCursorImage {
            host_xid: xid,
            width: record.width,
            height: record.height,
            hot_x: record.hot_x,
            hot_y: record.hot_y,
            x,
            y,
            // XFIXES serial is u32; CursorRecord.version is u64
            // server-wide monotonic. Saturate; in practice we'll
            // never roll over a u32 of cursor changes in a session.
            serial: u32::try_from(record.version).unwrap_or(u32::MAX),
            bgra_bytes: std::sync::Arc::new(record.bgra_bytes.clone()),
        })
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
        // Xorg `ReplaceCursor`: windows, grabs and the resource database
        // stop referencing the old cursor. Here that is every window's
        // cursor slot, the sticky root default and the grab override.
        if old_host_xid == new_host_xid {
            return Ok(());
        }
        for geom in self.windows.values_mut() {
            if geom.cursor == Some(old_host_xid) {
                geom.cursor = Some(new_host_xid);
            }
        }
        if self.core.active_cursor == Some(old_host_xid) {
            self.core.active_cursor = Some(new_host_xid);
        }
        if self.grab_cursor_override == Some(old_host_xid) {
            self.grab_cursor_override = Some(new_host_xid);
        }
        self.refresh_effective_cursor();
        self.collect_released_cursors();
        Ok(())
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
        let cap = usize::from(max_names);
        // Font-path (fonts.dir/alias) names first, then the built-ins
        // catalog — ONE global max_names budget across the ordered
        // walk (Xorg traverses FPEs in path order with one count).
        let names: Vec<String> = self.core.font_loader.list_font_names(pattern, cap);

        let mut name_data: Vec<u8> = Vec::new();
        for name in &names {
            name_data.push(u8::try_from(name.len()).unwrap_or(u8::MAX));
            name_data.extend_from_slice(name.as_bytes());
        }
        let pad = (4 - (name_data.len() % 4)) % 4;
        name_data.resize(name_data.len() + pad, 0);

        let extra_words = u32::try_from(name_data.len() / 4).unwrap_or(0);
        let mut reply = vec![0u8; 32 + name_data.len()];
        reply[0] = 1;
        reply[4..8].copy_from_slice(&extra_words.to_le_bytes());
        reply[8..10].copy_from_slice(&u16::try_from(names.len()).unwrap_or(u16::MAX).to_le_bytes());
        reply[32..].copy_from_slice(&name_data);
        Ok(reply)
    }

    fn list_fonts_with_info_proxy(
        &mut self,
        _origin: Option<OriginContext>,
        max_names: u16,
        pattern: &str,
        intern_atom: &mut dyn FnMut(&str) -> u32,
    ) -> io::Result<Vec<Vec<u8>>> {
        let cap = usize::from(max_names);
        // Path fonts first, then built-ins — one global budget
        // (mirrors list_fonts_proxy ordering so the two requests
        // agree on the visible font set).
        let listed = self.core.font_loader.list_fonts_with_info(pattern, cap);

        let mut entries: Vec<(String, FontMetrics)> = Vec::with_capacity(listed.len());
        for crate::kms::core::ListedFont {
            reply_name: name,
            open_name,
            res,
        } in listed
        {
            // The fonts.dir KEY a path font resolved to — not the bare
            // matched alias, not the file-embedded FONT atom — is the
            // round-trippable XA_FONT.
            let path_entry_key = match &res {
                crate::kms::core::FontResolution::File { entry_name, .. } => {
                    Some(entry_name.clone())
                }
                _ => None,
            };
            match self.core.font_loader.font_info(&res, &open_name) {
                Ok(info) => {
                    let mut metrics = info.metrics_without_chars();
                    // Reply NAME keeps Xorg FPE behavior: path / XLFD
                    // names go out verbatim; a bare built-in alias
                    // ("fixed"/"cursor"/"nil2") is rewritten to a full
                    // XLFD so XCreateFontSet can parse a charset.
                    let wire_name = if path_entry_key.is_some()
                        || crate::kms::core::FontLoader::is_xlfd_pattern(&name)
                    {
                        name.clone()
                    } else {
                        crate::kms::core::FontLoader::alias_to_xlfd(&name, &metrics)
                    };
                    // XA_FONT must be a charset-bearing XLFD that
                    // round-trips through open_font (libX11 OpenFonts it
                    // verbatim — omGeneric.c get_prop_name). For a path
                    // font that's the fonts.dir KEY resolve() matched;
                    // the PCF's embedded FONT atom can diverge from the
                    // key (point/dpi/avg-width fields — e.g. Debian
                    // xfonts-* 19px misc-fixed) and then fail to reopen.
                    let font_prop_name = match path_entry_key {
                        Some(entry) => entry,
                        None if crate::kms::core::FontLoader::is_xlfd_pattern(&name) => {
                            name.clone()
                        }
                        None => crate::kms::core::FontLoader::alias_to_xlfd(&name, &metrics),
                    };
                    // Properties: file-embedded (BDF/PCF) ones when
                    // present, interned here; always ensure FONT
                    // (XA_FONT=18 → atom of the wire name) —
                    // libX11's XCreateFontSet resolves non-XLFD base
                    // names EXCLUSIVELY through this property
                    // (omGeneric.c get_prop_name reads XA_FONT off the
                    // first reply and GetAtomName's it); the reply
                    // name alone is not consulted on that path.
                    let named = std::mem::take(&mut metrics.named_properties);
                    let mut props = Vec::with_capacity(named.len() * 8 + 8);
                    for (pname, value) in &named {
                        // XA_FONT (18) is re-emitted below as `wire_name`
                        // — the name the server can actually re-open. A
                        // PCF's file-embedded FONT atom can diverge from
                        // its fonts.dir entry key (point/dpi/avg-width
                        // fields); reporting the embedded one breaks the
                        // XCreateFontSet round-trip (libX11 OpenFonts
                        // XA_FONT verbatim, but `resolve()` only matches
                        // fonts.dir keys) on font sets where they differ
                        // — e.g. Debian xfonts-* 19px misc-fixed. Skip
                        // the embedded FONT here.
                        if pname == "FONT" {
                            continue;
                        }
                        let name_atom = intern_atom(pname);
                        let v: u32 = match value {
                            yserver_protocol::x11::FontPropValue::Card(c) => *c,
                            #[allow(clippy::cast_sign_loss)]
                            yserver_protocol::x11::FontPropValue::Int(i) => *i as u32,
                            yserver_protocol::x11::FontPropValue::Str(s) => intern_atom(s),
                        };
                        props.extend_from_slice(&name_atom.to_le_bytes());
                        props.extend_from_slice(&v.to_le_bytes());
                    }
                    // Always emit XA_FONT (18) = the round-trippable name
                    // (fonts.dir key for path fonts; the synthesized
                    // charset-bearing XLFD for built-in aliases).
                    let font_atom = intern_atom(&font_prop_name);
                    props.extend_from_slice(&18u32.to_le_bytes());
                    props.extend_from_slice(&font_atom.to_le_bytes());
                    metrics.properties = props;
                    entries.push((wire_name, metrics));
                }
                Err(err) => {
                    log::debug!("render ListFontsWithInfo: skipping {name:?} — {err}");
                }
            }
        }

        let total = entries.len();
        let mut replies: Vec<Vec<u8>> = Vec::with_capacity(total + 1);
        for (idx, (name, metrics)) in entries.iter().enumerate() {
            let remaining = u32::try_from(total - idx - 1).unwrap_or(0);
            let mut buf = Vec::new();
            yserver_protocol::x11::write_list_fonts_with_info_reply(
                &mut buf,
                yserver_protocol::x11::ClientByteOrder::LittleEndian,
                yserver_protocol::x11::SequenceNumber(0),
                metrics,
                name,
                remaining,
            )?;
            replies.push(buf);
        }
        let mut term = Vec::new();
        yserver_protocol::x11::write_list_fonts_with_info_terminator(
            &mut term,
            yserver_protocol::x11::ClientByteOrder::LittleEndian,
            yserver_protocol::x11::SequenceNumber(0),
        )?;
        replies.push(term);
        Ok(replies)
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
