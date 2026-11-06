use crate::{
    access::{AccessMode, RequestOpcodes, ResourceKind},
    backend::{Backend, OriginContext},
    core_loop::{
        fanout::fanout_event_to_clients,
        process_request::{
            RequestOutcome, XI2_FIRST_ERROR, XI2_MAJOR_OPCODE, emit_x11_error_with_minor,
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
use std::{collections::HashSet, io};
use yserver_protocol::{
    x11,
    x11::{ClientByteOrder, ClientId, RequestHeader, ResourceId, SequenceNumber},
};

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
        let num_masks = u16::from_le_bytes([body[4], body[5]]) as usize;
        // Xorg ProcXISelectEvents checks every mask before
        // setting any, so a rejected request changes nothing.
        let mut masks: Vec<(u16, u64)> = Vec::with_capacity(num_masks);
        let mut pos = 8;
        for _ in 0..num_masks {
            if pos + 4 > body.len() {
                break;
            }
            let deviceid = u16::from_le_bytes([body[pos], body[pos + 1]]);
            let mask_len = u16::from_le_bytes([body[pos + 2], body[pos + 3]]) as usize;
            pos += 4;
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
            masks.push((deviceid, u64::from_le_bytes(word)));
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
        if let Some(client) = state.clients.get_mut(&client_id.0) {
            for (deviceid, mask) in masks {
                debug!(
                    "client {} XISelectEvents window=0x{:x} deviceid={} mask=0x{:x}",
                    client_id.0, window.0, deviceid, mask
                );
                if mask == 0 {
                    client.xi2_masks.remove(&(window, deviceid));
                } else {
                    client.xi2_masks.insert((window, deviceid), mask);
                    if window == ROOT_WINDOW
                        && matches!(deviceid, 0..=3)
                        && (mask & u64::from(XI2_DEVICE_CHANGED_MASK)) != 0
                    {
                        send_device_changed_bootstrap = true;
                    }
                }
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
            .clients
            .get(&client_id.0)
            .map(|client| {
                client
                    .xi2_masks
                    .iter()
                    .filter(|&(&(win, _), &mask)| win == window && mask != 0)
                    .map(|(&(_, dev), &mask)| (dev, mask))
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
// An XEventClass packs `(deviceid << 8) | event_code`. We only
// implement XI1 `DevicePropertyNotify` (event code 16 within
// the XInput block, i.e. low byte = `XI_FIRST_EVENT + 16` = 82
// — yserver assigns XInput the contiguous block 66..=82), so
// classes for other XI1 events (motion / button / key / etc.)
// are accepted but ignored. We do NOT error on unknown classes:
// a single SelectExtensionEvent often selects multiple classes
// and Xorg silently drops the ones it can't service.
//
// Selection state is canonical per window, as in Xorg. Events such
// as DevicePropertyNotify carry no event-window field, so a separate
// aggregate is derived from the per-window state for delivery.
//
// Replace semantics: for every deviceid mentioned in the
// supplied class list, this client's prior selections for that
// deviceid on this window are dropped before the new set is
// installed. A client
// unsubscribes a given deviceid by passing at least one class
// for it whose low byte is not `DevicePropertyNotify` — the
// deviceid lands in `touched_devices` and its prior entries
// are cleared; the unsupported class itself is silently
// dropped. `count == 0` is a no-op (the existing selection
// survives unchanged), matching Xorg's behaviour.
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
    if presence_selected && let Some(client) = state.clients.get_mut(&client_id.0) {
        // Xorg installs DevicePresenceNotifyMask in
        // HandleDevicePresenceMask before ordinary classes are
        // validated. Preserve that ordering if a later class in
        // this request reports BadClass.
        client
            .xi1_window_event_classes
            .entry(ResourceId(sel_window))
            .or_default()
            .insert(crate::xinput::XI1_DEVICE_PRESENCE_CLASS);
        client.rebuild_xi1_global_event_classes();
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
    // Decode the requested class list and collect the set of
    // deviceids that appear (so we can drop the client's stale
    // entries for *those* devices — Xorg's "replace per device"
    // semantics, see Xi/selectev.c::ProcXSelectExtensionEvent).
    let mut accepted_classes: Vec<u32> = Vec::with_capacity(ordinary_classes.len());
    let mut touched_devices: HashSet<u8> = HashSet::new();
    for class in ordinary_classes {
        #[allow(clippy::cast_possible_truncation)]
        let class_low = class as u8; // event code in the XInput block
        #[allow(clippy::cast_possible_truncation)]
        let dev_byte = (class >> 8) as u8;
        touched_devices.insert(dev_byte);
        // DevicePropertyNotify classes are device-scoped; the
        // input-event classes (DeviceKeyPress..DeviceMotionNotify),
        // the focus classes (DeviceFocusIn/Out, consumed by
        // `xi1_focus::emit_device_focus`) and DeviceStateNotify
        // (consumed by `xi1_state_notify::deliver_state_notify`)
        // select per window like core input events and are
        // recorded in `xi1_window_event_classes` below. Other
        // classes are silently discarded, matching Xorg: it walks
        // `xi_all_events` and skips entries it cannot service.
        if crate::server::xi1_class_is_global_notification(class) {
            // DevicePropertyNotify / DeviceMappingNotify /
            // ChangeDeviceNotify are server-wide per-device
            // events: Xorg `Xi/exevents.c::SendMappingNotify` /
            // `SendDeviceNotify` fan them out by walking the
            // global client list, NOT a per-window event-mask.
            accepted_classes.push(class);
        } else if (XI_FIRST_EVENT + XI_DEVICE_KEY_PRESS_OFFSET
            ..=XI_FIRST_EVENT + crate::xinput::XI_DEVICE_FOCUS_OUT_OFFSET)
            .contains(&class_low)
            || class_low == XI_FIRST_EVENT + crate::xinput::XI_DEVICE_STATE_NOTIFY_OFFSET
        {
            accepted_classes.push(class);
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
    if let Some(client) = state.clients.get_mut(&client_id.0) {
        // Selection state belongs to the request window. Drop stale
        // entries for the named devices on that window only (Xorg
        // replacement semantics), then install the accepted list.
        if !touched_devices.is_empty()
            && let Some(set) = client
                .xi1_window_event_classes
                .get_mut(&ResourceId(sel_window))
        {
            set.retain(|c| {
                if *c == crate::xinput::XI1_DEVICE_PRESENCE_CLASS {
                    return true;
                }
                #[allow(clippy::cast_possible_truncation)]
                let dev_byte = (c >> 8) as u8;
                !touched_devices.contains(&dev_byte)
            });
        }
        if !accepted_classes.is_empty() {
            let set = client
                .xi1_window_event_classes
                .entry(ResourceId(sel_window))
                .or_default();
            for class in accepted_classes {
                set.insert(class);
            }
        }

        // Keep the delivery-oriented aggregate in sync. Global XI1
        // notifications have no event-window field, but selecting
        // them is still per-window protocol state.
        client.rebuild_xi1_global_event_classes();
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
            .clients
            .get(&client_id.0)
            .and_then(|client| client.xi1_window_event_classes.get(&window))
            .map(|classes| classes.iter().copied().collect())
            .unwrap_or_default();
        this_client.sort_unstable();

        let mut all_clients: Vec<u32> = state
            .clients
            .values()
            .filter_map(|client| client.xi1_window_event_classes.get(&window))
            .flat_map(|classes| classes.iter().copied())
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
        let mut classes: Vec<u32> = Vec::with_capacity(count);
        for i in 0..count {
            let off = 8 + i * 4;
            if off + 4 > body.len() {
                break;
            }
            let class =
                u32::from_le_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]]);
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
            classes.push(class);
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
        // mode 0 = AddToList, 1 = DeleteFromList. Xorg
        // `Xi/chgdprop.c::ChangeDeviceDontPropagateList` applies the
        // change to `OtherInputMasks.dontPropagateMask[deviceid]`;
        // the deviceid is encoded in the high byte of each class so
        // the per-device split is implicit in the class set.
        let entry = state
            .xi1_window_dont_propagate
            .entry(ResourceId(win))
            .or_default();
        if mode == 0 {
            for c in &classes {
                entry.insert(*c);
            }
        } else {
            for c in &classes {
                entry.remove(c);
            }
            if entry.is_empty() {
                state.xi1_window_dont_propagate.remove(&ResourceId(win));
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
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    byte_order: ClientByteOrder,
    minor: u8,
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
        // Xorg Xi/sendexev.c order: device, then the class list
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
        // Per Xorg `Xi/sendexev.c::ProcXSendExtensionEvent` →
        // `Xi/exevents.c::SendEvent`: resolve the destination,
        // then deliver each supplied event to (a) clients whose
        // XI1 selection on the destination window intersects the
        // request's class list, and (b) the window's CREATOR when
        // the per-device mask is empty (the `noextensioneventclass`
        // case — Xorg `DeliverToWindowOwner` short-circuits on
        // `filter == CantBeFiltered` = `NoEventMask` = 0). The
        // class-intersection rule preserves XI1 event chains
        // (DeviceKeyPress + DeviceValuator continuation), which
        // the client selects via the head class only.
        // Propagate semantics are not yet honoured — direct-window
        // match only, like core SendEvent with propagate=False.
        //
        // Specials (xserver.git Xi/exevents.c:2915-2944):
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
                        // Xorg `effectiveFocus` rule (xserver.git
                        // Xi/exevents.c:2936-2941): if the focus is
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
            // Xorg `Xi/grabdev.c::CreateMaskFromList` walks the
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
            let targets = xi1_send_extension_event_resolve_targets(
                state,
                dest_window,
                &request_classes,
                mask_is_zero,
                propagate,
            );
            if !targets.is_empty() {
                // Pre-snapshot all event bytes so the fanout
                // closure can borrow them per-recipient.
                let mut events_bytes: Vec<[u8; 32]> = Vec::with_capacity(num_events);
                for i in 0..num_events {
                    let off = 12 + i * 32;
                    let Some(ev) = body.get(off..off + 32) else {
                        break;
                    };
                    events_bytes.push(ev.try_into().expect("32-byte slice"));
                }
                let targets_vec: Vec<ClientId> = targets.into_iter().collect();
                let _dropped = fanout_event_to_clients(state, &targets_vec, |buf, seq, order| {
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
