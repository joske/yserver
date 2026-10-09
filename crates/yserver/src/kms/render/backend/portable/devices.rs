use super::*;

impl KmsBackend {
    fn stamp_host_key_without_master_transition(
        &self,
        raw: HostKeyEvent,
        time: u32,
    ) -> HostKeyEvent {
        HostKeyEvent {
            state: self.serialize_modifiers(),
            root_x: self.core.cursor_x as i16,
            root_y: self.core.cursor_y as i16,
            event_x: self.core.cursor_x as i16,
            event_y: self.core.cursor_y as i16,
            time,
            ..raw
        }
    }

    pub(in crate::kms::render::backend) fn handle_host_key(
        &mut self,
        state: &mut ServerState,
        raw: HostKeyEvent,
        repeat: bool,
        generate_raw: bool,
    ) {
        use yserver_core::core_loop::key_fanout::{
            commit_key_transition, key_event_fanout_after_transition, key_transition_status,
        };

        let Some(transition) = key_transition_status(state, raw.origin, raw.keycode, raw.pressed)
        else {
            return;
        };
        let time = crate::clock::server_time_ms();
        if transition.device_accepted {
            let _dropped =
                yserver_core::core_loop::key_fanout::announce_key_source_switch(state, raw.origin);
        }
        if generate_raw {
            let key_was_down = yserver_core::core_loop::key_fanout::keyboard_key_is_down(
                state,
                raw.origin,
                raw.keycode,
            );
            let _dropped = yserver_core::core_loop::key_fanout::raw_key_event_to_state(
                state,
                yserver_core::core_loop::key_fanout::RawKeyEvent {
                    origin: raw.origin,
                    keycode: raw.keycode,
                    pressed: raw.pressed,
                    time,
                },
                key_was_down,
                self.core.xkb_desc.modmap[usize::from(raw.keycode)] != 0,
            );
        }
        if !transition.device_accepted {
            return;
        }

        self.synchronize_floating_keyboard_states(state);
        let floating_keyboard =
            yserver_core::core_loop::key_fanout::keyboard_origin_is_floating(state, raw.origin);
        let keyboard_device_id = match raw.origin {
            yserver_core::core_loop::InputOrigin::Physical(source_id) => state
                .xi_devices
                .facet(source_id, yserver_core::xinput::XiFacetKind::Keyboard),
            yserver_core::core_loop::InputOrigin::XTest(device_id) => Some(device_id),
            yserver_core::core_loop::InputOrigin::NestedHost => None,
        };
        if !floating_keyboard && let Some(device_id) = keyboard_device_id {
            if raw.pressed {
                if let Some(action) = self.lock_modifiers_action(raw.keycode) {
                    let locked_mods = self
                        .core
                        .xkb_state
                        .0
                        .serialize_mods(xkbcommon::xkb::STATE_MODS_LOCKED);
                    let pre_press_locked_mods = if transition.master_accepted {
                        locked_mods & u32::from(action[2])
                    } else {
                        0
                    };
                    self.lock_filter_priv_by_device
                        .entry(device_id)
                        .or_default()
                        .insert(
                            raw.keycode,
                            LockFilterPriv {
                                pre_press_locked_mods,
                                no_unlock: action[1] & XKB_SA_LOCK_NO_UNLOCK != 0,
                            },
                        );
                }
            } else if let Some(held) = self.lock_filter_priv_by_device.get_mut(&device_id) {
                held.remove(&raw.keycode);
                if held.is_empty() {
                    self.lock_filter_priv_by_device.remove(&device_id);
                }
            }
        }
        let cooked = if floating_keyboard {
            let device_id = keyboard_device_id;
            let Some(device_id) = device_id else {
                return;
            };
            let Some(cooked) = self.cook_floating_host_key(device_id, HostKeyEvent { time, ..raw })
            else {
                return;
            };
            if !raw.pressed
                && let Some(held) = self.lock_filter_priv_by_device.get_mut(&device_id)
            {
                held.remove(&raw.keycode);
                if held.is_empty() {
                    self.lock_filter_priv_by_device.remove(&device_id);
                }
            }
            cooked
        } else if transition.master_accepted {
            let mut cooked = self.cook_host_key(HostKeyEvent { time, ..raw });
            cooked.time = time;
            if cooked.pressed {
                self.core.down_keys.insert(cooked.keycode);
            } else {
                self.core.down_keys.remove(&cooked.keycode);
            }
            cooked
        } else {
            self.stamp_host_key_without_master_transition(raw, time)
        };

        commit_key_transition(state, raw.origin, raw.keycode, raw.pressed, transition);
        // RECORD's core conversion is attached to master-device callbacks
        // only (record/record.c:784-794). A slave edge can update its own
        // bitmap while the master rejects the duplicate or the slave is
        // floating; those edges must not be recorded as core transitions.
        if transition.master_accepted && !(repeat && !cooked.pressed) {
            yserver_core::core_loop::record::record_device_event(
                state,
                yserver_core::core_loop::record::RecordedDeviceEvent {
                    event_type: if cooked.pressed { 2 } else { 3 },
                    detail: cooked.keycode,
                    repeat,
                    time: cooked.time,
                    root_x: cooked.root_x,
                    root_y: cooked.root_y,
                    state: cooked.state,
                },
            );
        }
        let _dropped =
            key_event_fanout_after_transition(state, self, cooked, transition.master_accepted);
        self.synchronize_floating_keyboard_states(state);
    }

    /// Release all held keys attributed to one physical keyboard facet.
    /// Each release re-enters the normal per-device and master guards; an
    /// earlier physical release therefore cannot be emitted twice. This is
    /// also how explicit XTEST holds targeting the physical facet are drained.
    pub(crate) fn release_keyboard_source_keys(
        &mut self,
        state: &mut ServerState,
        source_id: yserver_core::xinput::InputSourceId,
    ) {
        let device_id = state
            .xi_devices
            .facet(source_id, yserver_core::xinput::XiFacetKind::Keyboard);
        let mut held: Vec<(u8, yserver_core::core_loop::InputOrigin)> =
            if let Some(device_id) = device_id {
                state
                    .key_down_by_device
                    .get(&device_id)
                    .into_iter()
                    .flat_map(|keys| keys.iter().map(|(key, origin)| (*key, *origin)))
                    .collect()
            } else {
                state
                    .unpublished_keyboard_keys_down
                    .get(&source_id)
                    .into_iter()
                    .flat_map(|keys| keys.iter().map(|(key, origin)| (*key, *origin)))
                    .collect()
            };
        held.sort_by_key(|(keycode, _)| *keycode);
        for (keycode, origin) in held {
            self.handle_host_key(
                state,
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
    }

    fn release_pointer_source_buttons(
        &mut self,
        state: &mut ServerState,
        source_id: yserver_core::xinput::InputSourceId,
    ) {
        use yserver_core::{
            core_loop::{HostInputEvent, InputOrigin},
            xinput::XiFacetKind,
        };

        let held = state
            .xi_devices
            .facet(source_id, XiFacetKind::PointerTouch)
            .and_then(|device_id| state.xi_devices.device(device_id))
            .map(|device| device.buttons_down)
            .or_else(|| {
                state
                    .unpublished_pointer_buttons_down
                    .get(&source_id)
                    .copied()
            })
            .unwrap_or(0);

        // `buttons_down` stores logical button details. Find the physical
        // detail that produced each held logical button so re-entering the
        // host-input path applies SetPointerMapping exactly once. Xorg keeps
        // the physical down bit and maps it at delivery (Xi/exevents.c:1922-
        // 1934); ReleaseButtonsAndKeys walks those physical bits in order
        // (dix/devices.c:2636-2642).
        let mapping = state.pointer_mapping_override.as_deref();
        let mut releases = Vec::new();
        for logical_detail in 1..=10 {
            let bit = 1u16 << (logical_detail - 1);
            if held & bit == 0 {
                continue;
            }
            let physical_detail = mapping
                .and_then(|map| {
                    if usize::from(logical_detail) > map.len() {
                        None
                    } else {
                        map.iter()
                            .position(|mapped| *mapped == logical_detail)
                            .and_then(|index| u8::try_from(index + 1).ok())
                    }
                })
                .unwrap_or(logical_detail);
            let code = match physical_detail {
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
            releases.push((physical_detail, code));
        }
        releases.sort_unstable_by_key(|(detail, _)| *detail);
        for (_, code) in releases {
            Backend::on_host_input(
                self,
                state,
                HostInputEvent::PointerButton {
                    origin: InputOrigin::Physical(source_id),
                    button: code,
                    pressed: false,
                    time: crate::clock::server_time_ms(),
                },
            );
        }
    }

    /// Drain one physical source's holds through the same duplicate guards
    /// and fanout used by ordinary input, without changing the source's
    /// enabled state. Server reset uses this while the old XI topology is
    /// still present; VT/removal cleanup additionally disables or unregisters
    /// the source afterwards.
    pub(in crate::kms::render::backend) fn release_device_holds(
        &mut self,
        state: &mut ServerState,
        source_id: yserver_core::xinput::InputSourceId,
    ) -> bool {
        if state.xi_devices.source(source_id).is_none() {
            return false;
        }

        state
            .key_repeats
            .remove(&yserver_core::core_loop::InputOrigin::Physical(source_id));
        self.release_pointer_source_buttons(state, source_id);
        self.release_keyboard_source_keys(state, source_id);
        self.synchronize_floating_keyboard_states(state);
        true
    }

    /// Drain just one physical XI facet while it is still enabled. Client
    /// disable requests use this narrower cleanup so a mixed source keeps
    /// delivering through its sibling facet.
    pub(in crate::kms::render::backend) fn release_device_facet_holds(
        &mut self,
        state: &mut ServerState,
        device_id: u16,
    ) -> bool {
        let Some(device) = state
            .xi_devices
            .device(device_id)
            .filter(|device| device.enabled)
            .cloned()
        else {
            return false;
        };
        let Some(source_id) = device.source_id else {
            return false;
        };
        match device.facet {
            Some(yserver_core::xinput::XiFacetKind::Keyboard) => {
                state
                    .key_repeats
                    .remove(&yserver_core::core_loop::InputOrigin::Physical(source_id));
                self.release_keyboard_source_keys(state, source_id);
            }
            Some(yserver_core::xinput::XiFacetKind::PointerTouch) => {
                self.release_pointer_source_buttons(state, source_id);
            }
            None => return false,
        }
        self.synchronize_floating_keyboard_states(state);
        true
    }

    /// Disable one physical facet in Xorg order. The hierarchy descriptor is
    /// captured while attached, then explicitly handed to the emitter before
    /// the live facet is floated.
    pub(in crate::kms::render::backend) fn publish_disabled_xi_facet(
        state: &mut ServerState,
        device_id: u16,
    ) {
        let was_enabled = state
            .xi_devices
            .device(device_id)
            .is_some_and(|device| device.enabled);

        // Session state is independent of client disable. A facet the client
        // already disabled still needs to remember that the VT went away so
        // a Device Enabled=1 write cannot reattach it before resume.
        state.xi_set_facet_session_enabled(device_id, false);
        if !was_enabled {
            return;
        }

        yserver_core::xinput::hotplug::publish_facet_disabled(state, device_id);
    }

    /// Own physical-device held-state cleanup for removal and VT suspension.
    /// Releases pass through the ordinary KMS input and XI fanout paths while
    /// the source is still live. XI grabs remain active across suspension, as
    /// DisableDevice releases holds but does not deactivate device grabs.
    pub(crate) fn release_device_state(
        &mut self,
        state: &mut ServerState,
        source_id: yserver_core::xinput::InputSourceId,
    ) {
        if !self.release_device_holds(state, source_id) {
            return;
        }

        let facet_ids: Vec<u16> = [
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
        if facet_ids.is_empty()
            && let Some(mut info) = state.xi_devices.source(source_id).cloned()
        {
            info.enabled = false;
            state.xi_register_source(&info);
        }
        for id in facet_ids {
            Self::publish_disabled_xi_facet(state, id);
        }
    }
}

impl KmsBackend {
    pub(in crate::kms::render::backend) fn backend_devices_on_host_input(
        &mut self,
        state: &mut ServerState,
        ev: HostInputEvent,
    ) {
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

    pub(in crate::kms::render::backend) fn backend_devices_reset_input_session(
        &mut self,
        old_state: &mut ServerState,
    ) {
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
}
