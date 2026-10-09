use super::*;

impl KmsBackend {
    // ── Input dispatch (Stage 3f.7) ─────────────────────────────
    //
    // Ports the v1 input cluster onto v2's state surface.
    // Differences from v1's body (kms/backend.rs:6450-6885):
    //
    // - `self.windows` → `self.windows`.
    // - `self.fb_w` / `self.fb_h` → the active output's geometry
    //   read off `self.platform.outputs[0]`.
    // - HW cursor calls (`hw_cursor_active` / `hw_cursor_move` /
    //   `hw_cursor_refresh`) → no-op. Per spec § I7 the HW cursor
    //   plane is parked in v2 until Stage 5 reintroduces it as a
    //   SceneCompositor strategy.
    // - `self.mark_all_outputs_dirty()` →
    //   `self.scene.mark_scene_structure_dirty()`. Pointer-motion-
    //   only redraws are a no-op in Stage 3 anyway (no cursor
    //   scene blit until Stage 4); the dirty flag preserves the
    //   "scene needs a tick" signal for any client paint that
    //   races a motion event.

    /// X11 KeyButMask: bits 0..=7 are modifiers
    /// (Shift/Lock/Control/Mod1..Mod5). Bits 8..=12 are button
    /// state, set by `process_pointer_button` via `button_mask`.
    /// Bits 13..=14 carry the active keyboard group (XkbGroupForCoreState),
    /// sourced from the authoritative `core.locked_group`.
    pub(in crate::kms::render::backend) fn serialize_modifiers(&self) -> u16 {
        Self::serialize_xkb_modifiers(
            &self.core.xkb_state.0,
            self.effective_locked_group(),
            self.keymap_group_count(),
        )
    }

    pub(in crate::kms::render::backend) fn serialize_xkb_modifiers(
        state: &xkbcommon::xkb::State,
        locked_group: u8,
        group_count: u8,
    ) -> u16 {
        let flags = xkbcommon::xkb::STATE_MODS_EFFECTIVE;
        let mut mask: u16 = 0;
        if state.mod_name_is_active("Shift", flags) {
            mask |= 0x01;
        }
        if state.mod_name_is_active("Lock", flags) {
            mask |= 0x02;
        }
        if state.mod_name_is_active("Control", flags) {
            mask |= 0x04;
        }
        if state.mod_name_is_active("Mod1", flags) {
            mask |= 0x08;
        }
        if state.mod_name_is_active("Mod2", flags) {
            mask |= 0x10;
        }
        if state.mod_name_is_active("Mod3", flags) {
            mask |= 0x20;
        }
        if state.mod_name_is_active("Mod4", flags) {
            mask |= 0x40;
        }
        if state.mod_name_is_active("Mod5", flags) {
            mask |= 0x80;
        }
        // XkbGroupForCoreState: active group in bits 13-14. Each keyboard
        // keeps its group lock alongside its own XKB state.
        let group = locked_group.min(group_count.saturating_sub(1));
        mask |= (u16::from(group) & 0x3) << 13;
        mask
    }

    fn current_key_action(&self, keycode: u8) -> Option<crate::kms::xkb_desc::Action> {
        let key = xkbcommon::xkb::Keycode::new(u32::from(keycode));
        let layout = self.core.xkb_state.0.key_get_layout(key);
        let level = self.core.xkb_state.0.key_get_level(key, layout);
        let layout = usize::try_from(layout).ok()?;
        let level = usize::try_from(level).ok()?;
        let width = usize::from(self.core.xkb_desc.keys[usize::from(keycode)].width);
        let slot = layout.checked_mul(width)?.checked_add(level)?;
        Some(self.core.xkb_desc.key_action(keycode, slot))
    }

    pub(in crate::kms::render::backend) fn lock_modifiers_action(
        &self,
        keycode: u8,
    ) -> Option<crate::kms::xkb_desc::Action> {
        self.current_key_action(keycode)
            .filter(|action| action[0] == crate::kms::xkb_desc::SA_LOCK_MODS)
    }

    pub(in crate::kms::render::backend) fn synchronize_floating_keyboard_states(
        &mut self,
        state: &ServerState,
    ) {
        let floating_ids: HashSet<u16> = state
            .xi2_detached_masters
            .keys()
            .copied()
            .filter(|device_id| {
                state.xi_devices.role(*device_id)
                    == Some(yserver_core::xinput::XiDeviceRole::SlaveKeyboard)
            })
            .collect();
        self.floating_keyboard_states
            .retain(|device_id, _| floating_ids.contains(device_id));
        self.lock_filter_priv_by_device
            .retain(|device_id, _| state.xi_devices.device(*device_id).is_some());

        let new_ids: Vec<u16> = floating_ids
            .into_iter()
            .filter(|device_id| !self.floating_keyboard_states.contains_key(device_id))
            .collect();
        for device_id in new_ids {
            let mut xkb_state = xkbcommon::xkb::State::new(&self.core.xkb_keymap.0);
            let master_state = &self.core.xkb_state.0;
            let latched_mods = master_state.serialize_mods(xkbcommon::xkb::STATE_MODS_LATCHED);
            let locked_mods = master_state.serialize_mods(xkbcommon::xkb::STATE_MODS_LOCKED);
            let latched_group = master_state.serialize_layout(xkbcommon::xkb::STATE_LAYOUT_LATCHED);
            let locked_group = self.effective_locked_group();
            // Xorg AttachDevice pushes the master keyboard's locked and
            // latched modifiers/group to attached slaves. A slave that
            // starts a floating interval therefore inherits those masks,
            // while depressed key state remains device-local.
            xkb_state.update_mask(
                0,
                latched_mods,
                locked_mods,
                0,
                latched_group,
                u32::from(locked_group),
            );
            // Detaching a slave for an XI grab changes its attachment, not
            // its key-down bitmap. Xorg's DetachFromMaster only calls
            // AttachDevice(NULL, ...) (dix/events.c:1463-1471), and a later
            // release clears that device bitmap in UpdateDeviceState
            // (Xi/exevents.c:934-943). Seed both the duplicate guard and the
            // local XKB state from the keys the attached facet already held.
            let down_keys: HashSet<u8> = state
                .key_down_by_device
                .get(&device_id)
                .into_iter()
                .flat_map(|held| held.keys().copied())
                .collect();
            let mut lock_filter_priv_by_key = HashMap::new();
            for keycode in &down_keys {
                // Xorg keeps the active LockMods filter on the held key
                // across DetachFromMaster (xkb/xkbActions.c:372-384). Preserve
                // its up-action data for the floating release.
                if let Some(filter) = self
                    .lock_filter_priv_by_device
                    .get(&device_id)
                    .and_then(|held| held.get(keycode))
                {
                    lock_filter_priv_by_key.insert(*keycode, *filter);
                    continue;
                }

                // The attached press has already changed the master state
                // copied above. LockMods and LockGroup both use
                // _XkbFilterLockState (xkb/xkbActions.c:362-384); LockGroup
                // updates locked_group on press (:362-367). ISOLock has a
                // separate filter that can change locked modifiers/group
                // (:397-440). DetachFromMaster only changes attachment
                // (dix/events.c:1461-1468), so replaying those selected
                // actions would apply a second lock to the floating state.
                if self.current_key_action(*keycode).is_some_and(|action| {
                    matches!(
                        action[0],
                        crate::kms::xkb_desc::SA_LOCK_MODS
                            | crate::kms::xkb_desc::SA_LOCK_GROUP
                            | crate::kms::xkb_desc::SA_ISO_LOCK
                    )
                }) {
                    continue;
                }
                xkb_state.update_key(
                    xkbcommon::xkb::Keycode::new(u32::from(*keycode)),
                    xkbcommon::xkb::KeyDirection::Down,
                );
            }
            self.floating_keyboard_states.insert(
                device_id,
                FloatingKeyboardState {
                    xkb_state: crate::kms::core::XkbState(xkb_state),
                    down_keys,
                    lock_filter_priv_by_key,
                    locked_group,
                },
            );
        }
    }

    pub(in crate::kms::render::backend) fn cook_floating_host_key(
        &mut self,
        device_id: u16,
        raw: HostKeyEvent,
    ) -> Option<HostKeyEvent> {
        let key_is_down = self
            .floating_keyboard_states
            .get(&device_id)?
            .down_keys
            .contains(&raw.keycode);
        if raw.pressed == key_is_down {
            log::debug!(
                "floating host key device={device_id} key={} {}: duplicate transition dropped",
                raw.keycode,
                if raw.pressed { "press" } else { "release" },
            );
            return None;
        }

        let group_count = self.keymap_group_count();
        let floating = self.floating_keyboard_states.get_mut(&device_id)?;
        let lock_filter_priv = if raw.pressed {
            None
        } else {
            floating.lock_filter_priv_by_key.remove(&raw.keycode)
        };
        let pre_state = Self::serialize_xkb_modifiers(
            &floating.xkb_state.0,
            floating.locked_group,
            group_count,
        );
        let group_before = floating
            .xkb_state
            .0
            .serialize_layout(xkbcommon::xkb::STATE_LAYOUT_EFFECTIVE);
        floating.xkb_state.0.update_key(
            xkbcommon::xkb::Keycode::new(u32::from(raw.keycode)),
            if raw.pressed {
                xkbcommon::xkb::KeyDirection::Down
            } else {
                xkbcommon::xkb::KeyDirection::Up
            },
        );
        if let Some(filter) = lock_filter_priv
            && !filter.no_unlock
        {
            let xkb_state = &mut floating.xkb_state.0;
            let locked_mods = xkb_state.serialize_mods(xkbcommon::xkb::STATE_MODS_LOCKED);
            let depressed_mods = xkb_state.serialize_mods(xkbcommon::xkb::STATE_MODS_DEPRESSED);
            let latched_mods = xkb_state.serialize_mods(xkbcommon::xkb::STATE_MODS_LATCHED);
            let depressed_layout =
                xkb_state.serialize_layout(xkbcommon::xkb::STATE_LAYOUT_DEPRESSED);
            let latched_layout = xkb_state.serialize_layout(xkbcommon::xkb::STATE_LAYOUT_LATCHED);
            let locked_layout = xkb_state.serialize_layout(xkbcommon::xkb::STATE_LAYOUT_LOCKED);
            // XkbFilterLockState release clears `filter->priv` only when
            // LockNoUnlock is absent (xkb/xkbActions.c:382-384).
            // xkb_state_update_mask preserves the other components while
            // replacing only that saved lock subset.
            xkb_state.update_mask(
                depressed_mods,
                latched_mods,
                locked_mods & !filter.pre_press_locked_mods,
                depressed_layout,
                latched_layout,
                locked_layout,
            );
        }
        let group_after = floating
            .xkb_state
            .0
            .serialize_layout(xkbcommon::xkb::STATE_LAYOUT_EFFECTIVE);
        if group_after != group_before {
            floating.locked_group = u8::try_from(group_after)
                .unwrap_or(0)
                .min(group_count.saturating_sub(1));
        }
        if raw.pressed {
            floating.down_keys.insert(raw.keycode);
        } else {
            floating.down_keys.remove(&raw.keycode);
        }
        let post_state = Self::serialize_xkb_modifiers(
            &floating.xkb_state.0,
            floating.locked_group,
            group_count,
        );
        let state = (pre_state & 0x00ff) | (post_state & 0x6000);
        Some(HostKeyEvent {
            state,
            root_x: self.core.cursor_x as i16,
            root_y: self.core.cursor_y as i16,
            event_x: self.core.cursor_x as i16,
            event_y: self.core.cursor_y as i16,
            ..raw
        })
    }

    pub(in crate::kms::render::backend) fn keymap_group_count(&self) -> u8 {
        self.core.keymap_group_count()
    }

    pub(in crate::kms::render::backend) fn clamp_group_to_keymap(&self, group: u8) -> u8 {
        group.min(self.keymap_group_count().saturating_sub(1))
    }

    pub(in crate::kms::render::backend) fn effective_locked_group(&self) -> u8 {
        self.clamp_group_to_keymap(self.core.locked_group)
    }

    /// Direct-mode wiring: hand over the relay the input thread ends
    /// of which `input_thread::run` polls. Called from `lib.rs` when
    /// spawning the libinput thread.
    pub fn set_led_relay(&mut self, relay: std::sync::Arc<crate::input::LedRelay>) {
        self.led_relay = Some(relay);
    }

    /// Current lock-LED state derived from `xkb_state`, as
    /// `input::Led` bits. Split from [`Self::sync_keyboard_leds`] so
    /// the no-Vk test fixture can pin the XKB→LED mapping without a
    /// libinput device. As on Xorg the keyboard LEDs follow the lit
    /// indicators by index (`XkbDDXUpdateIndicators` hands the driver the
    /// effective state; xf86-input-libinput/evdev light Caps Lock for bit
    /// 0, Num Lock for bit 1, Scroll Lock for bit 2), not by name, so an
    /// indicator renamed by SetNames keeps its LED.
    pub(in crate::kms::render::backend) fn current_led_bits(&self) -> u32 {
        let lit = self.core.xkb_desc.indicators_lit(&self.core.xkb_state.0);
        let mut leds = input::Led::empty();
        for (bit, led) in [
            (0, input::Led::CAPSLOCK),
            (1, input::Led::NUMLOCK),
            (2, input::Led::SCROLLLOCK),
        ] {
            if lit & (1 << bit) != 0 {
                leds |= led;
            }
        }
        leds.bits()
    }

    /// Push the XKB lock-LED state (Caps/Num/Scroll) to the keyboards
    /// when it changed. On a KMS server nothing else drives the LEDs —
    /// Xorg's keyboard driver does this via the XKB indicator state;
    /// we do it via `libinput_device_led_update`. The update crosses to the
    /// input thread via the `LedRelay` eventfd.
    pub(in crate::kms::render::backend) fn sync_keyboard_leds(&mut self) {
        let bits = self.current_led_bits();
        if bits == self.leds_sent {
            return;
        }
        self.leds_sent = bits;
        if let Some(relay) = self.led_relay.as_ref() {
            relay.set(bits);
        }
    }

    /// `ChangeKeyboardMapping` on the keyboard description, as Xorg's
    /// `XkbApplyMappingChange`: `XkbUpdateKeyTypesFromCore` then
    /// `XkbUpdateActions` over the requested keys, ported onto the model
    /// ([`crate::kms::xkb_desc::XkbDesc::apply_keyboard_mapping`]), whose
    /// cooking keymap is then installed. XI1 `ChangeDeviceKeyMapping` comes
    /// through here too.
    pub(in crate::kms::render::backend) fn apply_keyboard_mapping(
        &mut self,
        first_keycode: u8,
        keysyms_per_keycode: u8,
        keysyms: &[u32],
    ) -> yserver_core::backend::KeyboardMappingChange {
        let count = u8::try_from(keysyms.len() / usize::from(keysyms_per_keycode.max(1)))
            .unwrap_or(u8::MAX);
        let changes = self.mutate_keymap(|desc| {
            desc.apply_keyboard_mapping(first_keycode, keysyms_per_keycode, count, keysyms)
        });
        self.mapping_change(&changes)
    }

    /// `SetModifierMapping` on the keyboard description, as Xorg's
    /// `XkbApplyMappingChange` with a modmap: the modmap replaced, then
    /// `XkbUpdateActions` over the whole keycode range
    /// ([`crate::kms::xkb_desc::XkbDesc::apply_modifier_mapping`]).
    pub(in crate::kms::render::backend) fn apply_modifier_mapping(
        &mut self,
        modmap: &[u8; 256],
    ) -> yserver_core::backend::KeyboardMappingChange {
        let changes = self.mutate_keymap(|desc| desc.apply_modifier_mapping(modmap));
        self.mapping_change(&changes)
    }

    /// The one mutation path (§4.2): run `f` (a literal port of an Xorg
    /// handler) on a copy of the description, then make the copy
    /// authoritative with its cooking keymap. Fail-closed: when the cooking
    /// keymap doesn't compile (a writer bug; never expected) the description
    /// and keymap stay and it's logged; the changes still go out, as Xorg's
    /// notifications do for a change that alters nothing.
    pub(crate) fn mutate_keymap(
        &mut self,
        f: impl FnOnce(&mut crate::kms::xkb_desc::XkbDesc) -> crate::kms::xkb_desc::XkbChanges,
    ) -> crate::kms::xkb_desc::XkbChanges {
        let mut desc = self.core.xkb_desc.clone();
        let changes = f(&mut desc);
        if let Err(e) = self.install_desc(desc) {
            log::warn!("xkb: mapping change not applied: {e}");
        }
        changes
    }

    /// XKB SetMap on the keyboard description: Xorg's `ProcXkbSetMap` from
    /// its present-mask check on (the core loop did the size and BadAccess
    /// checks): `_XkbSetMapCheckLength`, `_XkbSetMapChecks`, then
    /// `_XkbSetMap` through the one mutation path
    /// ([`crate::kms::xkb_desc::set_map`]). A request that fails a check
    /// changes nothing and sends nothing. The events are Xorg's: a
    /// NewKeyboardNotify when the request's keycode range isn't the
    /// server's (and then nothing else), else `XkbSendNotification`'s.
    pub(in crate::kms::render::backend) fn xkb_set_map(
        &mut self,
        body: &[u8],
        client_is_ancient: bool,
    ) -> yserver_core::backend::XkbSetOutcome {
        use crate::kms::xkb_desc::{reply::BAD_LENGTH, set_map};
        use yserver_core::backend::{XkbSetEvent, XkbSetOutcome};
        let fail = |e: crate::kms::xkb_desc::reply::XkbError| XkbSetOutcome {
            error: Some((e.code, e.value)),
            events: Vec::new(),
        };
        let words = u16::try_from((body.len() + 4) / 4).unwrap_or(0);
        let mut req = vec![0, 9];
        req.extend_from_slice(&words.to_le_bytes());
        req.extend_from_slice(body);
        let mut h = set_map::SetMapHeader::parse(&req);
        if let Err(e) = set_map::check_present(&h)
            .and_then(|()| set_map::check_length(&h, &req))
            .and_then(|()| set_map::check(&self.core.xkb_desc, &mut h, &req, client_is_ancient))
        {
            return fail(e);
        }
        let Some(m) = set_map::decode(&h, &req) else {
            return fail(crate::kms::xkb_desc::reply::XkbError {
                code: BAD_LENGTH,
                value: 0,
            });
        };
        let mut result = set_map::SetMapResult::default();
        let changes = self.mutate_keymap(|desc| {
            result = desc.set_map(&h, &m);
            result.changes.clone()
        });
        let mut events = Vec::new();
        if let Some(r) = result.range {
            events.push(XkbSetEvent::NewKeyboard(
                yserver_core::backend::XkbNewKeyboardInfo {
                    min_keycode: r.min,
                    max_keycode: r.max,
                    old_min_keycode: r.old_min,
                    old_max_keycode: r.old_max,
                    changed: 0x0001, // XkbNKN_KeycodesMask
                },
            ));
        }
        if let Some(value) = result.bad_length {
            return XkbSetOutcome {
                error: Some((BAD_LENGTH, value)),
                events,
            };
        }
        if result.range.is_some() {
            events.push(XkbSetEvent::Repeats(changes.repeats.clone()));
        } else {
            events.push(XkbSetEvent::Notification(self.mapping_change(&changes)));
        }
        XkbSetOutcome {
            error: None,
            events,
        }
    }

    /// The request `body` (after the 4-byte header) as a whole request with
    /// XKB minor `minor`, for the ports that read Xorg's offsets from
    /// `stuff`.
    fn xkb_whole_request(minor: u8, body: &[u8]) -> Vec<u8> {
        let words = u16::try_from((body.len() + 4) / 4).unwrap_or(0);
        let mut req = vec![0, minor];
        req.extend_from_slice(&words.to_le_bytes());
        req.extend_from_slice(body);
        req
    }

    /// XKB SetCompatMap on the keyboard description: Xorg's
    /// `ProcXkbSetCompatMap` after its size and BadAccess checks (the core
    /// loop's): `_XkbSetCompatMap`'s dry run, then its apply pass through
    /// the one mutation path ([`crate::kms::xkb_desc::set_compat`]). The
    /// events are Xorg's: CompatMapNotify (changedGroups, firstSI and nSI
    /// from the request, nTotalSI after), then for `recomputeActions` the
    /// `XkbSendNotification` of `XkbUpdateActions` over the whole keycode
    /// range (cause: our XKB major, minor 11).
    pub(in crate::kms::render::backend) fn xkb_set_compat_map(
        &mut self,
        body: &[u8],
    ) -> yserver_core::backend::XkbSetOutcome {
        use crate::kms::xkb_desc::set_compat;
        use yserver_core::backend::{XkbSetEvent, XkbSetOutcome};
        let req = Self::xkb_whole_request(11, body);
        let h = set_compat::SetCompatMapHeader::parse(&req);
        if let Err(e) = set_compat::check_compat_map(&self.core.xkb_desc, &h) {
            return XkbSetOutcome {
                error: Some((e.code, e.value)),
                events: Vec::new(),
            };
        }
        let m = set_compat::decode_compat_map(&h, &req);
        let changes = self.mutate_keymap(|desc| desc.set_compat_map(&h, &m));
        let mut events = vec![XkbSetEvent::CompatMap(
            yserver_protocol::x11::XkbCompatMapNotify {
                device_id: 1,
                changed_groups: h.groups,
                first_si: h.first_si,
                n_si: h.n_si,
                n_total_si: u16::try_from(self.core.xkb_desc.compat.len()).unwrap_or(u16::MAX),
            },
        )];
        if h.recompute_actions {
            events.push(XkbSetEvent::Notification(self.mapping_change(&changes)));
        }
        XkbSetOutcome {
            error: None,
            events,
        }
    }

    /// XKB SetIndicatorMap on the keyboard description: Xorg's
    /// `ProcXkbSetIndicatorMap` after its size and BadAccess checks (the core
    /// loop's), `_XkbSetIndicatorMap`'s stores through the one mutation path
    /// ([`crate::kms::xkb_desc::set_compat`]), then `XkbApplyLedMapChanges`:
    /// the new maps' lit state (`XkbUpdateLedAutoState`: only the indicators
    /// whose map was set are recomputed, and nothing when no map is in use)
    /// and its notifications. The keyboard LEDs follow the new maps (the
    /// install resyncs them, Xorg's `XkbDDXUpdateDeviceIndicators`).
    pub(in crate::kms::render::backend) fn xkb_set_indicator_map(
        &mut self,
        body: &[u8],
    ) -> yserver_core::backend::XkbSetOutcome {
        use crate::kms::xkb_desc::set_compat;
        use yserver_core::backend::{XkbIndicatorMapsChange, XkbSetEvent, XkbSetOutcome};
        let req = Self::xkb_whole_request(14, body);
        let (which, maps) = match set_compat::check_indicator_map(&req) {
            Ok(Some(checked)) => checked,
            Ok(None) => return XkbSetOutcome::default(),
            Err(e) => {
                return XkbSetOutcome {
                    error: Some((e.code, e.value)),
                    events: Vec::new(),
                };
            }
        };
        let before = self.core.xkb_desc.indicators_lit(&self.core.xkb_state.0);
        let _ = self.mutate_keymap(|desc| {
            desc.set_indicator_map(which, &maps);
            crate::kms::xkb_desc::XkbChanges::default()
        });
        let desc = &self.core.xkb_desc;
        let lit = desc.indicators_lit(&self.core.xkb_state.0);
        let state = if desc.maps_present() == 0 {
            before
        } else {
            (lit & which) | (before & !which)
        };
        XkbSetOutcome {
            error: None,
            events: vec![XkbSetEvent::IndicatorMaps(XkbIndicatorMapsChange {
                maps_changed: which,
                state_changed: state ^ before,
                state,
                leds_defined: desc.names_present() | desc.maps_present(),
            })],
        }
    }

    /// XKB SetNames on the keyboard description: Xorg's `ProcXkbSetNames`
    /// after its size and BadAccess checks (the core loop's): its checks and
    /// `_XkbSetNamesCheck` ([`crate::kms::xkb_desc::set_names`]), then
    /// `_XkbSetNames` through the one mutation path (a new indicator name
    /// recompiles the cooking keymap, whose LEDs are read by name). The
    /// events are Xorg's: NamesNotify, then for indicator names an
    /// ExtensionDeviceNotify (IndicatorNames) with the names and maps
    /// present and the lit indicators.
    pub(in crate::kms::render::backend) fn xkb_set_names(
        &mut self,
        body: &[u8],
        atom_name: &dyn Fn(u32) -> Option<String>,
    ) -> yserver_core::backend::XkbSetOutcome {
        use crate::kms::xkb_desc::{reply::INDICATOR_NAMES, set_names};
        use yserver_core::backend::{XkbSetEvent, XkbSetOutcome};
        let req = Self::xkb_whole_request(18, body);
        let h = set_names::SetNamesHeader::parse(&req);
        let m = match set_names::check_set_names(&self.core.xkb_desc, &h, &req, atom_name) {
            Ok(m) => m,
            Err(e) => {
                return XkbSetOutcome {
                    error: Some((e.code, e.value)),
                    events: Vec::new(),
                };
            }
        };
        let mut notify = yserver_protocol::x11::XkbNamesNotify::default();
        let _ = self.mutate_keymap(|desc| {
            notify = desc.set_names(&h, &m);
            crate::kms::xkb_desc::XkbChanges::default()
        });
        notify.device_id = 1;
        let mut events = vec![XkbSetEvent::Names(notify)];
        if h.which & INDICATOR_NAMES != 0 {
            let desc = &self.core.xkb_desc;
            events.push(XkbSetEvent::IndicatorNames {
                leds_defined: desc.names_present() | desc.maps_present(),
                state: desc.indicators_lit(&self.core.xkb_state.0),
            });
        }
        XkbSetOutcome {
            error: None,
            events,
        }
    }

    /// XKB SetGeometry on the keyboard description, name only (review
    /// outcome for #171): Xorg's `ProcXkbSetGeometry` after its size and
    /// BadAccess checks (the core loop's) — the name's atom and
    /// `_CheckSetGeom`'s whole walk ([`crate::kms::xkb_desc::set_geometry`])
    /// decide acceptance as on Xorg — then `_XkbSetGeometry`'s visible
    /// effects: the geometry name stored, NamesNotify(GeometryName) when it
    /// changed, and NewKeyboardNotify(Geometry) over the unchanged keycode
    /// range. The geometry body isn't kept, so GetGeometry keeps answering
    /// found=False.
    pub(in crate::kms::render::backend) fn xkb_set_geometry(
        &mut self,
        body: &[u8],
        atom_name: &dyn Fn(u32) -> Option<String>,
    ) -> yserver_core::backend::XkbSetOutcome {
        use crate::kms::xkb_desc::{reply::GEOMETRY_NAME, set_geometry};
        use yserver_core::backend::{XkbNewKeyboardInfo, XkbSetEvent, XkbSetOutcome};
        let req = Self::xkb_whole_request(20, body);
        let h = set_geometry::SetGeometryHeader::parse(&req);
        if let Err(e) =
            set_geometry::check_set_geometry(&h, &req, &|atom| atom_name(atom).is_some())
        {
            return XkbSetOutcome {
                error: Some((e.code, e.value)),
                events: Vec::new(),
            };
        }
        let name = if h.name == 0 { None } else { atom_name(h.name) };
        let new_name = self.core.xkb_desc.names.geometry != name;
        let _ = self.mutate_keymap(|desc| {
            desc.names.geometry = name;
            crate::kms::xkb_desc::XkbChanges::default()
        });
        let mut events = Vec::new();
        if new_name {
            events.push(XkbSetEvent::Names(yserver_protocol::x11::XkbNamesNotify {
                device_id: 1,
                changed: u16::try_from(GEOMETRY_NAME).unwrap_or(0),
                ..Default::default()
            }));
        }
        let desc = &self.core.xkb_desc;
        events.push(XkbSetEvent::NewKeyboard(XkbNewKeyboardInfo {
            min_keycode: desc.min_key_code,
            max_keycode: desc.max_key_code,
            old_min_keycode: desc.min_key_code,
            old_max_keycode: desc.max_key_code,
            changed: 0x0002, // XkbNKN_GeometryMask
        }));
        XkbSetOutcome {
            error: None,
            events,
        }
    }

    /// What a mapping change did, for the notifications the core loop sends
    /// (Xorg `XkbSendNotification`: the MapNotify fields straight from the
    /// changes).
    fn mapping_change(
        &self,
        changes: &crate::kms::xkb_desc::XkbChanges,
    ) -> yserver_core::backend::KeyboardMappingChange {
        let mc = changes.map;
        let desc = &self.core.xkb_desc;
        yserver_core::backend::KeyboardMappingChange {
            map_notify: yserver_protocol::x11::XkbMapNotify {
                device_id: 1,
                ptr_btn_actions: 0,
                changed: mc.changed,
                min_keycode: desc.min_key_code,
                max_keycode: desc.max_key_code,
                first_type: mc.first_type,
                n_types: mc.num_types,
                first_key_sym: mc.first_key_sym,
                n_key_syms: mc.num_key_syms,
                first_key_act: mc.first_key_act,
                n_key_acts: mc.num_key_acts,
                first_key_behavior: mc.first_key_behavior,
                n_key_behaviors: mc.num_key_behaviors,
                first_key_explicit: mc.first_key_explicit,
                n_key_explicit: mc.num_key_explicit,
                first_mod_map_key: mc.first_modmap_key,
                n_mod_map_keys: mc.num_modmap_keys,
                first_vmod_map_key: mc.first_vmodmap_key,
                n_vmod_map_keys: mc.num_vmodmap_keys,
                virtual_mods: mc.vmods,
            },
            num_groups: self.keymap_group_count(),
            enabled_controls: crate::kms::xkb::XKB_ENABLED_CONTROLS,
            repeats: changes.repeats.clone(),
            indicator_map_changed: changes.indicator_map_changes,
            indicator_state: desc.indicators_lit(&self.core.xkb_state.0),
            compat_changed_groups: changes.compat_changed_groups,
            compat_total_si: u16::try_from(desc.compat.len()).unwrap_or(u16::MAX),
        }
    }

    /// Make a mutated description authoritative
    /// ([`crate::kms::core::KmsCore::install_desc`]: fail-closed, carries the
    /// locked state) and resync the lock LEDs, since a lock the change
    /// couldn't carry has been released.
    pub(crate) fn install_desc(
        &mut self,
        desc: crate::kms::xkb_desc::XkbDesc,
    ) -> Result<(u8, u8), crate::kms::core::KeymapTextError> {
        let bounds = self.core.install_desc(desc)?;
        self.sync_keyboard_leds();
        Ok(bounds)
    }

    /// Update xkb_state for `raw` then return a cooked
    /// `HostKeyEvent` with the post-update modifier state +
    /// cursor coords pre-filled. Direct v1 port.
    pub(in crate::kms::render::backend) fn cook_host_key(
        &mut self,
        raw: HostKeyEvent,
    ) -> HostKeyEvent {
        let xkb_keycode = xkbcommon::xkb::Keycode::new(u32::from(raw.keycode));
        let direction = if raw.pressed {
            xkbcommon::xkb::KeyDirection::Down
        } else {
            xkbcommon::xkb::KeyDirection::Up
        };
        // Driver 2 (compiled `grp:` key actions): a key bound to a
        // group-switch action (e.g. `grp:alt_shift_toggle`) advances
        // xkb_state's effective layout inside `update_key`. yserver's
        // authoritative group lives in `core.locked_group` (Driver 1 =
        // XkbLatchLockState writes it directly), so detect the change
        // HERE — read the effective layout before and after the
        // `update_key` — and copy the new group into `locked_group`.
        // This makes the `serialize_modifiers()` call below stamp the
        // NEW group into the very key event that triggered the switch.
        // We react only to an actual before≠after change, so a key that
        // doesn't switch groups leaves a Driver-1 lock untouched.
        // X11 KeyPress/KeyRelease `state` is the modifier state
        // IMMEDIATELY BEFORE the event takes effect (Xorg `dix`
        // computes it before applying the key's own action). Snapshot
        // the real-modifier bits BEFORE update_key so a modifier key's
        // own press does NOT include its own bit (and its release still
        // shows it set). Without this, Alt/Ctrl/Shift/Super press events
        // carried their own bit and releases carried 0 — an off-by-one
        // that misled i3's keyboard debugging (wire-confirmed).
        let pre_state = self.serialize_modifiers();
        let group_before = self
            .core
            .xkb_state
            .0
            .serialize_layout(xkbcommon::xkb::STATE_LAYOUT_EFFECTIVE);
        self.core.xkb_state.0.update_key(xkb_keycode, direction);
        let group_after = self
            .core
            .xkb_state
            .0
            .serialize_layout(xkbcommon::xkb::STATE_LAYOUT_EFFECTIVE);
        if group_after != group_before {
            // LayoutIndex (u32) -> group (0..=3). Out-of-range clamps to 0.
            let group = u8::try_from(group_after).unwrap_or(0);
            self.core.locked_group = self.clamp_group_to_keymap(group);
        }
        self.sync_keyboard_leds();
        // Modifier bits (0..=7) come from the PRE-update snapshot; the
        // active-group bits (13..=14) come from the POST-update state so
        // a `grp:`-bound key that switches layout still stamps the NEW
        // group into the very event that triggered the switch (Driver-2).
        let post_state = self.serialize_modifiers();
        let state = (pre_state & 0x00ff) | (post_state & 0x6000);
        HostKeyEvent {
            state,
            root_x: self.core.cursor_x as i16,
            root_y: self.core.cursor_y as i16,
            event_x: self.core.cursor_x as i16,
            event_y: self.core.cursor_y as i16,
            time: crate::clock::server_time_ms(),
            ..raw
        }
    }

    /// Drain every retained physical source through the guarded per-source
    /// release owner before the VT handoff. Masters and virtual XTEST 4/5
    /// have no source record and retain their held state. XI2 raw listeners
    /// are not updated, and no origin-less releases are synthesized.
    ///
    /// Caller is `run_suspend`.
    pub(in crate::kms::render::backend) fn synthesize_held_releases(
        &mut self,
        state: &mut ServerState,
    ) {
        for source_id in state.xi_devices.source_ids() {
            self.release_device_state(state, source_id);
        }
    }
}
