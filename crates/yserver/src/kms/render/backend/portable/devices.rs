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
