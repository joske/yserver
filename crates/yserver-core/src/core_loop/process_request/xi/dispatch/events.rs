use crate::{
    access::{AccessMode, EventBatch, EventSubject, RequestOpcodes, ResourceKind},
    backend::{Backend, OriginContext},
    core_loop::{
        fanout::fanout_event_to_clients,
        process_request::{
            RequestOutcome, XI2_FIRST_ERROR, XI2_MAJOR_OPCODE, describe_wire_event,
            emit_x11_error_with_minor,
            xi::{
                XI1_ERROR_BAD_CLASS, XI1_ERROR_BAD_DEVICE, XI1_ERROR_BAD_MODE,
                dispatch::xi_write_reply, emit_xi2_device_changed_bootstrap, xi1_device_valid,
                xi1_error, xi1_event_class_device, xi1_send_extension_event_resolve_targets,
                xi1_window_exists, xi2_first_invalid_mask_bit,
            },
        },
    },
    resources::ROOT_WINDOW,
    server::{ServerState, XI_FIRST_EVENT},
    xinput::{XI_DEVICE_KEY_PRESS_OFFSET, XI2_DEVICE_CHANGED_MASK},
};
use log::debug;
use std::{
    collections::{HashMap, HashSet},
    io,
};
use yserver_protocol::{
    x11,
    x11::{ClientByteOrder, ClientId, RequestHeader, ResourceId, SequenceNumber},
};

impl ServerState {
    pub fn drop_window_subscriptions(&mut self, windows: &[ResourceId]) {
        for client in self.clients.values_mut() {
            for window in windows {
                client.event_masks.remove(window);
                client.xi1_window_event_classes.remove(window);
            }
            client
                .xi2_masks
                .retain(|(window, _), _| !windows.contains(window));
            client.rebuild_xi1_global_event_classes();
        }
        self.xi_input_clients
            .retain(|window, _| !windows.contains(window));
        self.xi1_window_input_events
            .retain(|window, _| !windows.contains(window));
        self.xi1_window_dont_propagate
            .retain(|window, _| !windows.contains(window));
        // Xorg `CursorFreeWindow`: a destroyed window takes every
        // client's XFIXES cursor selection on it along.
        self.xfixes_cursor_masks
            .retain(|(_, window), _| !windows.contains(window));
    }

    /// Returns the position of the first matching Xorg input-client entry;
    /// duplicate entries are retained and request paths update this first one.
    pub(crate) fn xi_input_client_index(
        &self,
        window: ResourceId,
        client: ClientId,
    ) -> Option<usize> {
        self.xi_input_clients
            .get(&window)?
            .iter()
            .position(|entry| !entry.server_owned_placeholder && entry.client == client)
    }

    /// Prepends a new Xorg input-client entry, including duplicate clients
    /// created by `HandleDevicePresenceMask`.
    pub(crate) fn prepend_xi_input_client(&mut self, window: ResourceId, client: ClientId) {
        self.xi_input_clients.entry(window).or_default().insert(
            0,
            crate::server::XiInputClient {
                client,
                server_owned_placeholder: false,
                xi1_masks: HashMap::new(),
                xi2_masks: HashMap::new(),
            },
        );
    }

    /// Returns the existing first entry or prepends one when this protocol
    /// path creates a shared XI selection entry.
    pub(crate) fn ensure_xi_input_client(
        &mut self,
        window: ResourceId,
        client: ClientId,
    ) -> (usize, bool) {
        if let Some(index) = self.xi_input_client_index(window, client) {
            (index, false)
        } else {
            self.prepend_xi_input_client(window, client);
            (0, true)
        }
    }

    /// Rebuilds legacy client-local lookup maps from the canonical per-window
    /// input-client entries, so existing delivery helpers remain projections.
    pub(crate) fn refresh_xi_client_selection_projection(&mut self, client: ClientId) {
        let mut xi1 = HashMap::<ResourceId, HashSet<u32>>::new();
        let mut xi2 = HashMap::<(ResourceId, u16), u64>::new();
        for (window, entries) in &self.xi_input_clients {
            for entry in entries.iter().filter(|entry| entry.client == client) {
                for classes in entry.xi1_masks.values() {
                    xi1.entry(*window)
                        .or_default()
                        .extend(classes.iter().copied());
                }
                for (&device, &mask) in &entry.xi2_masks {
                    if mask != 0 {
                        *xi2.entry((*window, device)).or_default() |= mask;
                    }
                }
            }
        }
        if let Some(client_state) = self.clients.get_mut(&client.0) {
            client_state.xi1_window_event_classes = xi1;
            client_state.xi2_masks = xi2;
            client_state.rebuild_xi1_global_event_classes();
        }
    }

    /// Implements Xorg's `ShouldFreeInputMasks`: the retained XI1 aggregate
    /// and the current do-not-propagate masks both keep the window storage live.
    pub(crate) fn should_free_xi_input_masks(&self, window: ResourceId) -> bool {
        let has_input_events = self
            .xi1_window_input_events
            .get(&window)
            .is_some_and(|per_device| per_device.values().any(|mask| *mask != 0));
        let has_dont_propagate = self
            .xi1_window_dont_propagate
            .get(&window)
            .is_some_and(|classes| !classes.is_empty());
        !has_input_events && !has_dont_propagate
    }

    /// Removes one `FreeResource` target entry and refreshes that client's
    /// projected maps so freeing also drops this entry's XI1 and XI2 masks.
    pub(crate) fn remove_xi_input_client(
        &mut self,
        window: ResourceId,
        index: usize,
    ) -> Option<ClientId> {
        let entries = self.xi_input_clients.get_mut(&window)?;
        if index >= entries.len() {
            return None;
        }
        let client = entries[index].client;
        let has_dont_propagate = self
            .xi1_window_dont_propagate
            .get(&window)
            .is_some_and(|classes| !classes.is_empty());
        // ../xserver/Xi/exevents.c:2833-2868: only the sole remaining
        // node can retain storage. Transfer its resource ownership intact;
        // its XI1/XI2 masks still participate in exclusivity and aggregation.
        if entries.len() == 1 && has_dont_propagate {
            entries[index].client = ClientId(0);
            entries[index].server_owned_placeholder = true;
        } else {
            entries.remove(index);
            if entries.is_empty() {
                self.xi_input_clients.remove(&window);
                self.xi1_window_input_events.remove(&window);
                self.xi1_window_dont_propagate.remove(&window);
            }
        }
        self.refresh_xi_client_selection_projection(client);
        Some(client)
    }

    /// Drops every shared XI input-client entry owned by a disconnected
    /// client. If the last client leaves, Xorg frees all window input-mask
    /// storage unless DNP masks keep it alive as a server-owned placeholder.
    pub(crate) fn forget_xi_extension_client(&mut self, client: ClientId) {
        let windows: Vec<_> = self
            .xi_input_clients
            .iter()
            .filter(|(_, entries)| {
                entries
                    .iter()
                    .any(|entry| !entry.server_owned_placeholder && entry.client == client)
            })
            .map(|(window, _)| *window)
            .collect();
        for window in windows {
            // Disconnect frees resources individually. An older server-owned
            // node counts as a remaining node, even after DNP was cleared.
            while let Some(index) = self.xi_input_client_index(window, client) {
                self.remove_xi_input_client(window, index);
            }
        }
    }
}

#[allow(clippy::needless_return)]
pub(super) fn handle_xi_select_events(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
    access_request: RequestOpcodes,
) -> io::Result<RequestOutcome> {
    debug!("client {} #{} XISelectEvents", client_id.0, sequence.0);
    let mut send_device_changed_bootstrap = false;
    if body.len() >= 8 {
        let window = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
        if state.resources.window(window).is_none() {
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                x11::error::BAD_WINDOW,
                window.0,
                46,
                header.opcode,
            );
        }
        let num_masks = u16::from_le_bytes([body[4], body[5]]) as usize;
        // Xorg ProcXISelectEvents checks every mask before
        // setting any, so a rejected request changes nothing.
        let mut masks: Vec<(u16, u64, usize)> = Vec::with_capacity(num_masks);
        let mut pos = 8;
        for _ in 0..num_masks {
            if pos + 4 > body.len() {
                break;
            }
            let deviceid = u16::from_le_bytes([body[pos], body[pos + 1]]);
            let mask_len = u16::from_le_bytes([body[pos + 2], body[pos + 3]]) as usize;
            pos += 4;
            if !matches!(deviceid, 0 | 1) && state.xi_devices.device(deviceid).is_none() {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    XI1_ERROR_BAD_DEVICE, // Xorg validates the device before applying any XISelectEvents masks (Xi/xiselectev.c:178-185,304-324)
                    u32::from(deviceid),
                    46,
                    header.opcode,
                );
            }
            let byte_len = mask_len.saturating_mul(4);
            if pos + byte_len > body.len() {
                break;
            }
            let mask_bytes = &body[pos..pos + byte_len];
            // XICheckInvalidMaskBits: a bit past XI2LASTEVENT is
            // BadValue with the bit number as errorValue.
            if let Some(bit) = xi2_first_invalid_mask_bit(mask_bytes) {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    bit,
                    46,
                    header.opcode,
                );
            }
            let hierarchy_event = crate::xinput::XI2_HIERARCHY_CHANGED_EVENT_TYPE;
            let selects_hierarchy = mask_bytes
                .get((hierarchy_event / 8) as usize)
                .is_some_and(|byte| byte & (1 << (hierarchy_event % 8)) != 0);
            if deviceid != 0 && selects_hierarchy {
                // Xorg Xi/xiselectev.c:186-193 permits this mask only
                // on XIAllDevices. Preserve its lookup-before-mask
                // validation order for concrete device ids.
                if deviceid != 1 && state.xi_devices.device(deviceid).is_none() {
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        XI2_FIRST_ERROR, // XI_BadDevice
                        u32::from(deviceid),
                        46,
                        header.opcode,
                    );
                }
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    hierarchy_event,
                    46,
                    header.opcode,
                );
            }
            // The mask is a bit array (bit n in byte n/8), so the
            // bytes are little-endian whatever the client order.
            let mut word = [0u8; 8];
            let keep = byte_len.min(8);
            word[..keep].copy_from_slice(&mask_bytes[..keep]);
            masks.push((deviceid, u64::from_le_bytes(word), mask_len));
            pos += byte_len;
        }
        crate::core_loop::process_request::return_on_access_denial!(
            crate::core_loop::process_request::validate_resource_if_present(
                state,
                client_id,
                window,
                ResourceKind::Window,
                AccessMode::RECEIVE,
                access_request,
            ),
            state,
            client_id,
            sequence,
            access_request,
        );
        for (deviceid, mask, mask_len) in masks {
            debug!(
                "client {} XISelectEvents window=0x{:x} deviceid={} mask=0x{:x}",
                client_id.0, window.0, deviceid, mask
            );
            let entry_index = if mask_len != 0 {
                Some(state.ensure_xi_input_client(window, client_id).0)
            } else {
                state.xi_input_client_index(window, client_id)
            };
            if let Some(entry_index) = entry_index {
                if let Some(entry) = state
                    .xi_input_clients
                    .get_mut(&window)
                    .and_then(|entries| entries.get_mut(entry_index))
                {
                    if mask_len == 0 || mask == 0 {
                        entry.xi2_masks.remove(&deviceid);
                    } else {
                        entry.xi2_masks.insert(deviceid, mask);
                    }
                }
                state.refresh_xi_client_selection_projection(client_id);
            }
            if mask_len != 0
                && window == ROOT_WINDOW
                && matches!(deviceid, 0..=3)
                && (mask & u64::from(XI2_DEVICE_CHANGED_MASK)) != 0
            {
                send_device_changed_bootstrap = true;
            }
        }
    }
    if send_device_changed_bootstrap {
        emit_xi2_device_changed_bootstrap(
            state,
            backend,
            origin,
            client_id,
            sequence,
            XI2_MAJOR_OPCODE,
        )?;
    }
    return Ok(RequestOutcome::Handled);
}

pub(super) fn handle_xi_get_selected_events(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    byte_order: ClientByteOrder,
    access_request: RequestOpcodes,
) -> io::Result<RequestOutcome> {
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    {
        debug!("client {} #{} XIGetSelectedEvents", client_id.0, sequence.0);
        if body.len() < 4 {
            return Ok(RequestOutcome::Handled);
        }
        let window = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
        crate::core_loop::process_request::return_on_access_denial!(
            crate::core_loop::process_request::validate_resource_if_present(
                state,
                client_id,
                window,
                ResourceKind::Window,
                AccessMode::GET_ATTR,
                access_request,
            ),
            state,
            client_id,
            sequence,
            access_request,
        );
        // Xorg ProcXIGetSelectedEvents: one xXIEventMask per device
        // in device-id order, each trimmed to the words that hold set
        // bits. deviceid/mask_len follow the client byte order; the
        // mask itself is a bit array (bit n in byte n/8), written as
        // little-endian bytes for every client.
        let mut selected: Vec<(u16, u64)> = state
            .xi_input_clients
            .get(&window)
            .into_iter()
            .flatten()
            .find(|entry| entry.client == client_id && !entry.server_owned_placeholder)
            .map(|entry| {
                entry
                    .xi2_masks
                    .iter()
                    .filter(|(_, mask)| **mask != 0)
                    .map(|(&device, &mask)| (device, mask))
                    .collect()
            })
            .unwrap_or_default();
        selected.sort_unstable_by_key(|&(dev, _)| dev);
        let mut masks = Vec::new();
        for &(dev, mask) in &selected {
            let mask_len: u16 = if mask >> 32 == 0 { 1 } else { 2 };
            x11::write_u16(byte_order, &mut masks, dev);
            x11::write_u16(byte_order, &mut masks, mask_len);
            masks.extend_from_slice(&mask.to_le_bytes()[..4 * usize::from(mask_len)]);
        }
        let num_masks = u16::try_from(selected.len()).unwrap_or(u16::MAX);
        let mut reply = x11::fixed_reply(
            byte_order,
            sequence,
            0,
            x11::checked_units(masks.len())? as u32,
        );
        x11::write_u16(byte_order, &mut reply, num_masks);
        reply.extend_from_slice(&[0; 22]);
        reply.extend_from_slice(&masks);
        buf.extend_from_slice(&reply);
    }
    xi_write_reply(state, client_id, buf)
}

// XI 1.x SelectExtensionEvent (minor 6; xSelectExtensionEventReq,
// XIproto.h). Body after the 4-byte generic header:
//   body[0..4] window (Window = CARD32),
//   body[4..6] count  (CARD16, number of XEventClass values),
//   body[6..8] pad00  (CARD16),
//   body[8..]  count × XEventClass (CARD32).
//
// An XEventClass packs `(deviceid << 8) | event_code`. Xorg 21.1.24
// registers the non-delivering classes 0..=8, NoExtensionEvent (9),
// and the event classes in `Xi/extinit.c:1071-1110`; `Xi/selectev.c:163-177`
// applies each decoded device mask. Unknown classes are ignored, as in
// CreateMaskFromList, while recognized classes replace that device's mask.
//
// Selection state is canonical per window, as in Xorg. Events such
// as DevicePropertyNotify carry no event-window field, so a separate
// aggregate is derived from the per-window state for delivery.
//
// Replace semantics: for every deviceid mentioned in the
// supplied class list, this client's prior selections for that
// deviceid on this window are dropped before the new set is
// installed. A client
// unsubscribes a given deviceid by passing a recognized class for it
// whose EventInfo mask is zero, such as NoExtensionEvent; that deviceid
// lands in `touched_devices` and its prior entries are cleared. An
// unknown class does not touch the device. `count == 0` is a no-op
// (the existing selection survives unchanged), matching Xorg.
#[allow(clippy::needless_return)]
pub(super) fn handle_xi1_select_extension_event(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    minor: u8,
    access_request: RequestOpcodes,
) -> io::Result<RequestOutcome> {
    if body.len() < 8 {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            16, // BadLength
            0,
            6,
            XI2_MAJOR_OPCODE,
        );
    }
    let count = usize::from(u16::from_le_bytes([body[4], body[5]]));
    let expected_len = 8usize.saturating_add(count.saturating_mul(4));
    if body.len() < expected_len {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            16, // BadLength
            0,
            6,
            XI2_MAJOR_OPCODE,
        );
    }
    // Xorg Xi/selectev.c validates the window first (BadWindow),
    // then rejects classes whose embedded deviceid names no
    // device (BadClass) — XTS XSelectExtensionEvent-11/-13.
    let sel_window = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
    if state.resources.window(ResourceId(sel_window)).is_none() {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_WINDOW,
            sel_window,
            u16::from(minor),
            XI2_MAJOR_OPCODE,
        );
    }
    crate::core_loop::process_request::return_on_access_denial!(
        crate::core_loop::process_request::validate_resource_if_present(
            state,
            client_id,
            ResourceId(sel_window),
            ResourceKind::Window,
            AccessMode::RECEIVE,
            access_request,
        ),
        state,
        client_id,
        sequence,
        access_request,
    );
    // Xorg's HandleDevicePresenceMask runs before ordinary XI1
    // class validation and removes every class whose device field
    // is 256. Only its `_devicePresence` class selects a mask.
    // Preserve that class separately: truncating the device field
    // to eight bits would turn it into XIAllDevices (id 0).
    let mut ordinary_classes = Vec::with_capacity(count);
    let mut presence_selected = false;
    for i in 0..count {
        let off = 8 + i * 4;
        let class = u32::from_le_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]]);
        if class >> 8 == 256 {
            if class & 0xff == 0 {
                presence_selected = true;
            }
            continue;
        }
        ordinary_classes.push(class);
    }
    if presence_selected && state.clients.contains_key(&client_id.0) {
        // HandleDevicePresenceMask always prepends, even if this client
        // already has an entry. Preserve its early application if a later
        // ordinary class in the same request reports BadClass.
        let window = ResourceId(sel_window);
        state.prepend_xi_input_client(window, client_id);
        let entry = &mut state.xi_input_clients.get_mut(&window).unwrap()[0];
        entry
            .xi1_masks
            .entry(256)
            .or_default()
            .insert(crate::xinput::XI1_DEVICE_PRESENCE_CLASS);
        *state
            .xi1_window_input_events
            .entry(window)
            .or_default()
            .entry(0)
            .or_default() |= 1 << 18;
        state.refresh_xi_client_selection_projection(client_id);
    }
    for class in &ordinary_classes {
        if !xi1_device_valid(&state.xi_devices, xi1_event_class_device(*class)) {
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                XI1_ERROR_BAD_CLASS,
                *class,
                u16::from(minor),
                XI2_MAJOR_OPCODE,
            );
        }
    }
    // Decode only classes CreateMaskFromList recognizes. In particular,
    // NoExtensionEvent touches its device with a zero mask, while an
    // unknown event class never reaches SelectForWindow.
    let mut accepted_classes: Vec<u32> = Vec::with_capacity(ordinary_classes.len());
    let mut touched_devices: HashSet<u8> = HashSet::new();
    for class in ordinary_classes {
        #[allow(clippy::cast_possible_truncation)]
        let class_low = class as u8; // event code in the XInput block
        #[allow(clippy::cast_possible_truncation)]
        let dev_byte = (class >> 8) as u8;
        let recognized = if crate::server::xi1_class_is_global_notification(class) {
            // DevicePropertyNotify / DeviceMappingNotify /
            // ChangeDeviceNotify are server-wide per-device
            // events: Xorg `Xi/exevents.c::SendMappingNotify` /
            // `SendDeviceNotify` fan them out by walking the
            // global client list, NOT a per-window event-mask.
            accepted_classes.push(class);
            true
        } else if class_low <= crate::xinput::XI1_DEVICE_OWNER_GRAB_BUTTON_CLASS {
            // CreateMaskFromList validates the device, not its button
            // capability (Xorg 21.1.24 `Xi/grabdev.c:171-199`). Classes
            // 7 and 8 both select DeviceButtonGrabMask.
            accepted_classes.push(class);
            true
        } else if class_low == 9 {
            // `_noExtensionEvent` has a known EventInfo entry with mask 0.
            true
        } else if (XI_FIRST_EVENT + XI_DEVICE_KEY_PRESS_OFFSET
            ..=XI_FIRST_EVENT + crate::xinput::XI_DEVICE_FOCUS_OUT_OFFSET)
            .contains(&class_low)
            || class_low == XI_FIRST_EVENT + crate::xinput::XI_DEVICE_STATE_NOTIFY_OFFSET
        {
            accepted_classes.push(class);
            true
        } else {
            // CreateMaskFromList ignores unknown classes, leaving this
            // device's temporary mask untouched and never selecting it.
            false
        };
        if recognized {
            touched_devices.insert(dev_byte);
        }
    }
    debug!(
        "client {} #{} XSelectExtensionEvent window=0x{:x} count={} accepted={} window_input={}",
        client_id.0,
        sequence.0,
        sel_window,
        count,
        accepted_classes
            .iter()
            .filter(|class| crate::server::xi1_class_is_global_notification(**class))
            .count(),
        accepted_classes
            .iter()
            .filter(|class| !crate::server::xi1_class_is_global_notification(**class))
            .count(),
    );
    let window = ResourceId(sel_window);
    let mut ordered_devices: Vec<u8> = touched_devices.into_iter().collect();
    ordered_devices.sort_unstable();
    for device in ordered_devices {
        let device_classes: Vec<u32> = accepted_classes
            .iter()
            .copied()
            .filter(|class| class >> 8 == u32::from(device))
            .collect();
        let selects_grab_class = device_classes.iter().any(|class| {
            u8::try_from(*class & 0xff).is_ok_and(crate::xinput::xi1_class_is_button_grab)
        });
        if selects_grab_class
            && state.xi_input_clients.get(&window).is_some_and(|entries| {
                entries.iter().any(|entry| {
                    entry.client != client_id
                        && entry
                            .xi1_masks
                            .get(&u16::from(device))
                            .is_some_and(|classes| {
                                classes.iter().any(|class| {
                                    u8::try_from(*class & 0xff)
                                        .is_ok_and(crate::xinput::xi1_class_is_button_grab)
                                })
                            })
                })
            })
        {
            // Xorg applies SelectForWindow in ascending device-id order.
            // Earlier devices already applied; this device and all later
            // ones remain unchanged after its exclusive-mask conflict.
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                x11::error::BAD_ACCESS,
                0,
                u16::from(minor),
                XI2_MAJOR_OPCODE,
            );
        }

        let (entry_index, created) = state.ensure_xi_input_client(window, client_id);
        if let Some(entry) = state
            .xi_input_clients
            .get_mut(&window)
            .and_then(|entries| entries.get_mut(entry_index))
        {
            if device_classes.is_empty() {
                entry.xi1_masks.remove(&u16::from(device));
            } else {
                entry
                    .xi1_masks
                    .insert(u16::from(device), device_classes.iter().copied().collect());
            }
        }
        if !device_classes.is_empty() {
            let mut aggregate = 0u64;
            for class in &device_classes {
                let event_code = class & 0xff;
                if event_code <= 6 {
                    // Xorg registers these non-delivering classes with the
                    // motion-hint/button-motion masks (Xi/extinit.c:1091-1097).
                    // Keep yserver's snapshot qualifier bits outside XI1 event
                    // offsets 0..=16 and the reserved bits 17/18.
                    aggregate |= 1u64 << (19 + event_code);
                } else if u8::try_from(event_code)
                    .is_ok_and(crate::xinput::xi1_class_is_button_grab)
                {
                    // DeviceButtonGrabMask is shared by classes 7 and 8
                    // (Xorg 21.1.24 Xi/extinit.c:144-145,1105-1106).
                    aggregate |= 1 << 17;
                } else if let Some(offset) = event_code.checked_sub(u32::from(XI_FIRST_EVENT)) {
                    aggregate |= 1u64 << offset;
                }
            }
            if aggregate != 0 {
                *state
                    .xi1_window_input_events
                    .entry(window)
                    .or_default()
                    .entry(u16::from(device))
                    .or_default() |= aggregate;
            }
        }
        state.refresh_xi_client_selection_projection(client_id);

        // SelectForWindow frees an existing input-client resource only when
        // its XI1 masks are empty and ShouldFreeInputMasks agrees. A newly
        // added zero-mask NoExtensionEvent entry is retained.
        let entry_has_xi1_masks = state
            .xi_input_clients
            .get(&window)
            .and_then(|entries| entries.get(entry_index))
            .is_some_and(|entry| entry.xi1_masks.values().any(|classes| !classes.is_empty()));
        if !created
            && device_classes.is_empty()
            && !entry_has_xi1_masks
            && state.should_free_xi_input_masks(window)
        {
            state.remove_xi_input_client(window, entry_index);
        }
    }
    return Ok(RequestOutcome::Handled);
}

// GetSelectedExtensionEvents: { window }. The trailing payload is
// this client's classes followed by all clients' classes for the
// requested window (Xorg Xi/getselev.c).
pub(super) fn handle_xi1_get_selected_extension_events(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    byte_order: ClientByteOrder,
    minor: u8,
    access_request: RequestOpcodes,
) -> io::Result<RequestOutcome> {
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    {
        let win = u32::from_le_bytes([
            *body.first().unwrap_or(&0),
            *body.get(1).unwrap_or(&0),
            *body.get(2).unwrap_or(&0),
            *body.get(3).unwrap_or(&0),
        ]);
        if !xi1_window_exists(state, win) {
            return xi1_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_WINDOW,
                win,
                minor,
            );
        }
        crate::core_loop::process_request::return_on_access_denial!(
            crate::core_loop::process_request::validate_resource_if_present(
                state,
                client_id,
                ResourceId(win),
                ResourceKind::Window,
                AccessMode::GET_ATTR,
                access_request,
            ),
            state,
            client_id,
            sequence,
            access_request,
        );
        let window = ResourceId(win);
        let mut this_client: Vec<u32> = state
            .xi_input_clients
            .get(&window)
            .into_iter()
            .flatten()
            .find(|entry| entry.client == client_id && !entry.server_owned_placeholder)
            .map(|entry| entry.xi1_masks.values().flatten().copied().collect())
            .unwrap_or_default();
        this_client.sort_unstable();

        let mut all_clients: Vec<u32> = state
            .xi_input_clients
            .get(&window)
            .into_iter()
            .flatten()
            // Server ownership suppresses delivery, not query visibility
            // (../xserver/Xi/getselev.c:118-151).
            .flat_map(|entry| entry.xi1_masks.values())
            .flatten()
            .copied()
            .collect();
        all_clients.sort_unstable();

        let total_classes = this_client.len().saturating_add(all_clients.len());
        let length_words = u32::try_from(total_classes).unwrap_or(u32::MAX);
        let this_count = u16::try_from(this_client.len()).unwrap_or(u16::MAX);
        let all_count = u16::try_from(all_clients.len()).unwrap_or(u16::MAX);
        let mut reply = x11::fixed_reply(byte_order, sequence, minor, length_words);
        x11::write_u16(byte_order, &mut reply, this_count);
        x11::write_u16(byte_order, &mut reply, all_count);
        reply.extend_from_slice(&[0u8; 20]);
        for class in this_client.into_iter().chain(all_clients) {
            x11::write_u32(byte_order, &mut reply, class);
        }
        buf.extend_from_slice(&reply);
    }
    xi_write_reply(state, client_id, buf)
}

// ChangeDeviceDontPropagateList (void): { window, count, mode },
// classes follow. mode ∈ {AddToList=0, DeleteFromList=1}.
pub(super) fn handle_xi1_change_device_dont_propagate_list(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    minor: u8,
    access_request: RequestOpcodes,
) -> io::Result<RequestOutcome> {
    {
        if body.len() < 8 {
            return Ok(RequestOutcome::Handled);
        }
        let win = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
        if !xi1_window_exists(state, win) {
            return xi1_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_WINDOW,
                win,
                minor,
            );
        }
        let mode = body[6];
        if mode > 1 {
            return xi1_error(
                state,
                client_id,
                sequence,
                XI1_ERROR_BAD_MODE,
                u32::from(mode),
                minor,
            );
        }
        let count = usize::from(u16::from_le_bytes([body[4], body[5]]));
        // CreateMaskFromList first decodes every recognized class. Keep
        // unsuppressible bits until per-device validation; silently dropping
        // them would allocate storage for a request Xorg rejects.
        let mut devices = std::collections::BTreeMap::<u16, (u32, HashSet<u32>)>::new();
        for i in 0..count {
            let off = 8 + i * 4;
            let Some(bytes) = body.get(off..off + 4) else {
                break;
            };
            let class = u32::from_le_bytes(bytes.try_into().unwrap());
            let device = xi1_event_class_device(class);
            if !xi1_device_valid(&state.xi_devices, device) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    XI1_ERROR_BAD_CLASS,
                    class,
                    minor,
                );
            }
            let code = u8::try_from(class & 0xff).unwrap();
            // Xorg EventInfo masks (../xserver/Xi/extinit.c:132-147,
            // 1071-1111). Only the five input masks propagate.
            let mask = match code.checked_sub(XI_FIRST_EVENT) {
                Some(1) => 1,
                Some(2) => 1 << 1,
                Some(3) => 1 << 2,
                Some(4) => 1 << 3,
                Some(5) => 1 << 6,
                Some(6 | 7) => 1 << 14,
                Some(8 | 9) => 1 << 4,
                Some(10) => 1 << 5,
                Some(11) => 1 << 15,
                Some(12) => 1 << 16,
                Some(16) => 1 << 19,
                _ => match code {
                    0 => 1 << 7,
                    1..=6 => 1 << (7 + code),
                    7 | 8 => 1 << 17,
                    _ => 0, // unknown classes and NoExtensionEvent
                },
            };
            if mask != 0 {
                let entry = devices.entry(device).or_default();
                entry.0 |= mask;
                entry.1.insert(class);
            }
        }
        crate::core_loop::process_request::return_on_access_denial!(
            crate::core_loop::process_request::validate_resource_if_present(
                state,
                client_id,
                ResourceId(win),
                ResourceKind::Window,
                AccessMode::SET_ATTR,
                access_request,
            ),
            state,
            client_id,
            sequence,
            access_request,
        );
        let window = ResourceId(win);
        if mode == 1 && !state.xi_input_clients.contains_key(&window) {
            return xi_write_reply(state, client_id, Vec::new());
        }
        // ../xserver/Xi/chgprop.c:126-140 invokes suppression once for
        // each nonzero temporary mask, in ascending device order. Each call
        // may free the head resource, including an XI2-only entry.
        for (device, (mask, classes)) in devices {
            if mode == 0 && mask & !0x4f != 0 {
                let previous = state
                    .xi1_window_dont_propagate
                    .get(&window)
                    .map_or(0, |existing| {
                        existing
                            .iter()
                            .filter(|class| xi1_event_class_device(**class) == device)
                            .fold(0, |bits, class| {
                                bits | match (*class & 0xff).checked_sub(u32::from(XI_FIRST_EVENT))
                                {
                                    Some(1) => 1,
                                    Some(2) => 2,
                                    Some(3) => 4,
                                    Some(4) => 8,
                                    Some(5) => 64,
                                    _ => 0,
                                }
                            })
                    });
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    XI1_ERROR_BAD_CLASS,
                    mask | previous,
                    minor,
                );
            }
            if mode == 0 {
                if !state.xi_input_clients.contains_key(&window) {
                    state.prepend_xi_input_client(window, client_id);
                }
                state
                    .xi1_window_dont_propagate
                    .entry(window)
                    .or_default()
                    .extend(classes);
            } else if let Some(existing) = state.xi1_window_dont_propagate.get_mut(&window) {
                for class in classes {
                    existing.remove(&class);
                }
                if existing.is_empty() {
                    state.xi1_window_dont_propagate.remove(&window);
                }
            }
            if state.should_free_xi_input_masks(window) {
                state.remove_xi_input_client(window, 0);
            }
        }
        debug!(
            "client {} #{} XI1 ChangeDeviceDontPropagateList win=0x{win:x} mode={mode} count={count}",
            client_id.0, sequence.0
        );
    }
    xi_write_reply(state, client_id, Vec::new())
}

// GetDeviceDontPropagateList: { window }. Reply layout
// (XIproto.h:`xGetDeviceDontPropagateListReply`):
//   bytes 0..1 = reply opcode (1) + pad
//   bytes 2..4 = sequence
//   bytes 4..8 = length (32-bit units of trailing class array)
//   bytes 8..10 = count (CARD16)
//   bytes 10..32 = pad
//   trailing `count` × XEventClass (CARD32) at byte 32
pub(super) fn handle_xi1_get_device_dont_propagate_list(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    byte_order: ClientByteOrder,
    minor: u8,
    access_request: RequestOpcodes,
) -> io::Result<RequestOutcome> {
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    {
        let win = u32::from_le_bytes([
            *body.first().unwrap_or(&0),
            *body.get(1).unwrap_or(&0),
            *body.get(2).unwrap_or(&0),
            *body.get(3).unwrap_or(&0),
        ]);
        if !xi1_window_exists(state, win) {
            return xi1_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_WINDOW,
                win,
                minor,
            );
        }
        crate::core_loop::process_request::return_on_access_denial!(
            crate::core_loop::process_request::validate_resource_if_present(
                state,
                client_id,
                ResourceId(win),
                ResourceKind::Window,
                AccessMode::GET_ATTR,
                access_request,
            ),
            state,
            client_id,
            sequence,
            access_request,
        );
        let classes: Vec<u32> = state
            .xi1_window_dont_propagate
            .get(&ResourceId(win))
            .map(|set| set.iter().copied().collect())
            .unwrap_or_default();
        #[allow(clippy::cast_possible_truncation)]
        let count = classes.len() as u16;
        let length_words = u32::try_from(classes.len()).unwrap_or(u32::MAX);
        let mut reply = x11::fixed_reply(byte_order, sequence, 0, length_words);
        x11::write_u16(byte_order, &mut reply, count); // bytes 8..10
        reply.extend_from_slice(&[0u8; 22]); // pad to 32
        for c in &classes {
            x11::write_u32(byte_order, &mut reply, *c);
        }
        buf.extend_from_slice(&reply);
    }
    xi_write_reply(state, client_id, buf)
}

// SendExtensionEvent (void): { destination, deviceid, propagate,
// count, num_events } + events (32 bytes each) + classes.
pub(super) fn handle_xi1_send_extension_event(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    byte_order: ClientByteOrder,
    minor: u8,
    access_request: RequestOpcodes,
) -> io::Result<RequestOutcome> {
    {
        if body.len() < 12 {
            return Ok(RequestOutcome::Handled);
        }
        let dest = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
        let dev = u16::from(body[4]);
        let class_count = usize::from(u16::from_le_bytes([body[6], body[7]]));
        let num_events = usize::from(body[8]);
        if !xi1_device_valid(&state.xi_devices, dev) {
            return xi1_error(
                state,
                client_id,
                sequence,
                XI1_ERROR_BAD_DEVICE,
                u32::from(dev),
                minor,
            );
        }
        // Xorg Xi/sendexev.c order (`Xi/sendexev.c:142-167`): device, then
        // the class list
        // (CreateMaskFromList → BadClass), and only then the
        // destination window inside SendEvent — XTS
        // XSendExtensionEvent-20 sends a stale window + bogus class
        // and expects the BadClass.
        let classes_off = 12 + num_events * 32;
        // The XI swap table for minor 31 marks everything past the
        // header as `OpaqueTail` because the per-event payload
        // (the sender's byte order, like core SendEvent's xEvent
        // template) must NOT be swapped. That leaves the trailing
        // XEventClass[] in the client's native order; read each
        // class with `byte_order` awareness rather than blind LE.
        let read_class = |off: usize| -> Option<u32> {
            let bytes = body.get(off..off + 4)?;
            let arr = [bytes[0], bytes[1], bytes[2], bytes[3]];
            Some(match byte_order {
                yserver_protocol::x11::ClientByteOrder::LittleEndian => u32::from_le_bytes(arr),
                yserver_protocol::x11::ClientByteOrder::BigEndian => u32::from_be_bytes(arr),
            })
        };
        for i in 0..class_count {
            let off = classes_off + i * 4;
            let Some(class) = read_class(off) else {
                break;
            };
            if !xi1_device_valid(&state.xi_devices, xi1_event_class_device(class)) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    XI1_ERROR_BAD_CLASS,
                    class,
                    minor,
                );
            }
        }
        // Like core SendEvent, destination accepts the specials
        // PointerWindow (0) and InputFocus (1).
        if !matches!(dest, 0 | 1) && !xi1_window_exists(state, dest) {
            return xi1_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_WINDOW,
                dest,
                minor,
            );
        }
        // Per Xorg `Xi/sendexev.c::ProcXSendExtensionEvent`
        // (`Xi/sendexev.c:142-169`) → `Xi/exevents.c::SendEvent`
        // (`Xi/exevents.c:2905-2963`): resolve the destination,
        // then deliver each supplied event to (a) clients whose
        // XI1 selection on the destination window intersects the
        // request's class list, and (b) the window's CREATOR when
        // the per-device mask is empty (the `noextensioneventclass`
        // case — Xorg `DeliverToWindowOwner` short-circuits on
        // `filter == CantBeFiltered` = `NoEventMask` = 0
        // (`dix/events.c:2205-2208`). The
        // class-intersection rule preserves XI1 event chains
        // (DeviceKeyPress + DeviceValuator continuation), which
        // the client selects via the head class only.
        // Propagation follows the XI class do-not-propagate walk in
        // `xi1_send_extension_event_resolve_targets` below.
        //
        // Specials (Xorg 21.1.24 `Xi/exevents.c:2912-2943`):
        //  - PointerWindow → sprite window (deepest mapped window
        //    under the pointer);
        //  - InputFocus → device's focus window, with PointerRoot
        //    falling back to the sprite and FollowKeyboard chasing
        //    the core focus inside `resolve_focus`.
        let dest_window: Option<ResourceId> = match dest {
            0 => Some(crate::core_loop::key_fanout::deepest_window_at_pointer(
                state,
            )),
            1 => {
                let focus = crate::core_loop::xi1_focus::device_focus(state, dev).focus;
                match crate::core_loop::xi1_focus::resolve_focus(state, focus) {
                    crate::core_loop::xi1_focus::Xi1FocusTarget::None => None,
                    crate::core_loop::xi1_focus::Xi1FocusTarget::PointerRoot => Some(
                        crate::core_loop::key_fanout::deepest_window_at_pointer(state),
                    ),
                    crate::core_loop::xi1_focus::Xi1FocusTarget::Window(f) => {
                        // Xorg `effectiveFocus` rule
                        // (`Xi/exevents.c:2933-2941`): if the focus is
                        // an ancestor of (or equal to) the sprite,
                        // deliver to the sprite — the focus
                        // bounds propagation but the deepest
                        // pointer window is the actual target.
                        // Otherwise the sprite is outside the focus
                        // subtree and delivery goes to the focus
                        // directly. xts5 XSendExtensionEvent-3
                        // probes the inferior-of-focus case.
                        let sprite = crate::core_loop::key_fanout::deepest_window_at_pointer(state);
                        if f == sprite || crate::core_loop::xi1_focus::is_ancestor(state, f, sprite)
                        {
                            Some(sprite)
                        } else {
                            Some(f)
                        }
                    }
                }
            }
            w => Some(ResourceId(w)),
        };
        if let Some(dest_window) = dest_window {
            // Collect the request's class list (already
            // validated above for `xi1_device_valid`). Same
            // byte-order reasoning as the BadClass loop above —
            // classes are in the sender's native order because the
            // XI swap table can't carve them out of the variable
            // event payload that precedes them.
            let mut request_classes: Vec<u32> = Vec::with_capacity(class_count);
            for i in 0..class_count {
                let off = classes_off + i * 4;
                let Some(class) = read_class(off) else {
                    break;
                };
                request_classes.push(class);
            }
            // Xorg `Xi/grabdev.c::CreateMaskFromList`
            // (`Xi/grabdev.c:171-198`) walks the
            // class list and OR's per-class mask bits into the
            // per-device mask. `noextensioneventclass` (low byte
            // `_noExtensionEvent = 9` per XI.h:260) has no
            // associated mask bit, so it contributes nothing —
            // a request whose only classes are noextensioneventclass
            // entries (or an empty list) ends up with an
            // all-zero mask, which `DeliverToWindowOwner` treats
            // as `CantBeFiltered` and delivers to the window
            // creator unconditionally. Without this fall-through
            // xts5 XSendExtensionEvent 1–4 (which always pass
            // `noextensioneventclass`) fail because the test
            // process never calls XSelectExtensionEvent first.
            const XI1_NO_EXTENSION_EVENT_OFFSET: u8 = 9;
            #[allow(clippy::cast_possible_truncation)]
            let mask_is_zero = request_classes.is_empty()
                || request_classes
                    .iter()
                    .all(|c| (c & 0xff) as u8 == XI1_NO_EXTENSION_EVENT_OFFSET);
            let propagate = matches!(body.get(5), Some(&p) if p != 0);
            // Keep the full batch for policy checks and the existing fanout
            // writer; the template bytes themselves stay in sender order.
            let mut events_bytes: Vec<[u8; 32]> = Vec::with_capacity(num_events);
            for i in 0..num_events {
                let off = 12 + i * 32;
                let Some(ev) = body.get(off..off + 32) else {
                    break;
                };
                events_bytes.push(ev.try_into().expect("32-byte slice"));
            }
            let descriptions: Vec<_> = events_bytes
                .iter()
                .map(|event| describe_wire_event(event, byte_order, backend))
                .collect();
            let event_batch = EventBatch::new(&descriptions);
            let candidates = xi1_send_extension_event_resolve_targets(
                state,
                dest_window,
                &request_classes,
                mask_is_zero,
                propagate,
            );
            let mut fanout_recipients = None;
            for candidate in candidates {
                if !matches!(
                    state.check_send(
                        client_id,
                        candidate.window,
                        Some(dev),
                        &event_batch,
                        Some(access_request),
                    ),
                    crate::access::Decision::Allow
                ) {
                    break;
                }
                let zero_mask_creator_selected = mask_is_zero && !candidate.recipients.is_empty();
                let recipients: Vec<ClientId> = candidate
                    .recipients
                    .iter()
                    .copied()
                    .filter(|recipient| {
                        matches!(
                            state.check_receive(
                                *recipient,
                                EventSubject::Window(candidate.window),
                                &event_batch,
                                Some(access_request),
                            ),
                            crate::access::Decision::Allow
                        )
                    })
                    .collect();
                if !recipients.is_empty() {
                    fanout_recipients = Some(recipients);
                    break;
                }
                if zero_mask_creator_selected || !candidate.continue_propagation {
                    break;
                }
            }
            if let Some(recipients) = fanout_recipients {
                let _dropped = fanout_event_to_clients(state, &recipients, |buf, seq, order| {
                    for ev_bytes in &events_bytes {
                        let mut out = *ev_bytes;
                        out[0] |= 0x80; // send_event
                        let seq_bytes = match order {
                            yserver_protocol::x11::ClientByteOrder::LittleEndian => {
                                seq.0.to_le_bytes()
                            }
                            yserver_protocol::x11::ClientByteOrder::BigEndian => {
                                seq.0.to_be_bytes()
                            }
                        };
                        out[2] = seq_bytes[0];
                        out[3] = seq_bytes[1];
                        buf.extend_from_slice(&out);
                    }
                });
            }
        }
        debug!(
            "client {} #{} XI1 SendExtensionEvent device={dev}",
            client_id.0, sequence.0
        );
    }
    xi_write_reply(state, client_id, Vec::new())
}
