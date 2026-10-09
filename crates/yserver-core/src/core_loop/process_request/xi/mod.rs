mod dispatch;

use super::*;
pub(super) use dispatch::*;

// ─── XI 1.x request validation (XTS XI scenario, the "Got Success,
// Expecting <error>" family) ─────────────────────────────────────────
//
// XI extension error codes are offsets from the extension's first_error
// (157): BadDevice=+0, BadEvent=+1, BadMode=+2, DeviceBusy=+3,
// BadClass=+4 (XIproto.h). Error packets carry major=137 + the
// request's minor.
pub(super) const XI1_ERROR_BAD_DEVICE: u8 = crate::nested::XI2_FIRST_ERROR;
const XI1_ERROR_BAD_MODE: u8 = crate::nested::XI2_FIRST_ERROR + 2;
const XI1_ERROR_BAD_CLASS: u8 = crate::nested::XI2_FIRST_ERROR + 4;

/// Keycode range advertised by ListInputDevices / XIQueryDevice.
const XI1_KEY_MIN: u8 = 8;
const XI1_KEY_MAX: u8 = 255;
/// XIproto `UseXKeyboard` — the "default modifier device" sentinel
/// accepted wherever a modifier_device byte is taken.
const XI1_USE_X_KEYBOARD: u16 = 255;

/// An XI1 device is valid exactly while its ID is present in the shared
/// registry snapshot used by XI1 and XI2 enumeration.
fn xi1_device_valid(devices: &crate::xinput::XiRegistry, id: u16) -> bool {
    devices.device(id).is_some()
}

/// XI1 count fields and bounds come from the same current classes that
/// XIQueryDevice serializes, including copied master classes.
pub(super) fn xi1_device_button_count(devices: &crate::xinput::XiRegistry, id: u16) -> Option<u8> {
    let device = devices.device(id)?;
    let count = device.class_shape.button_count();
    (count != 0).then_some(count)
}

fn xi1_device_valuator_count(devices: &crate::xinput::XiRegistry, id: u16) -> Option<u8> {
    let device = devices.device(id)?;
    let count = device.class_shape.valuator_count();
    (count != 0).then_some(count)
}

/// Registry role decides whether a device ID names one of the two masters.
fn xi1_device_is_master(devices: &crate::xinput::XiRegistry, id: u16) -> bool {
    matches!(
        devices.role(id),
        Some(
            crate::xinput::XiDeviceRole::MasterPointer
                | crate::xinput::XiDeviceRole::MasterKeyboard
        )
    )
}

/// `XI2LASTEVENT` for XI 2.4 (`XI_GestureSwipeEnd`, XI2.h).
const XI2_LAST_EVENT: u32 = 32;

/// Xorg `XICheckInvalidMaskBits` (Xi/xiselectev.c): the lowest set bit
/// above [`XI2_LAST_EVENT`] in an XI2 event mask (a bit array, bit n in
/// byte n/8), if any.
fn xi2_first_invalid_mask_bit(mask: &[u8]) -> Option<u32> {
    mask.iter().enumerate().find_map(|(i, &byte)| {
        let first_bit = u32::try_from(i * 8).ok()?;
        (0..8u32)
            .map(|b| first_bit + b)
            .find(|&bit| bit > XI2_LAST_EVENT && byte & (1 << (bit - first_bit)) != 0)
    })
}

/// Keyboard masters, XTEST keyboards, and registered keyboard facets carry
/// a KeyClass.
pub(crate) fn xi1_device_has_keys(devices: &crate::xinput::XiRegistry, id: u16) -> bool {
    matches!(
        devices.role(id),
        Some(
            crate::xinput::XiDeviceRole::MasterKeyboard
                | crate::xinput::XiDeviceRole::SlaveKeyboard
        )
    )
}

/// Pointer masters, XTEST pointer, and registered pointer facets carry
/// Button and Valuator classes.
pub(crate) fn xi1_device_has_buttons(devices: &crate::xinput::XiRegistry, id: u16) -> bool {
    matches!(
        devices.role(id),
        Some(
            crate::xinput::XiDeviceRole::MasterPointer | crate::xinput::XiDeviceRole::SlavePointer
        )
    )
}

pub(crate) fn xi1_device_has_valuators(devices: &crate::xinput::XiRegistry, id: u16) -> bool {
    xi1_device_has_buttons(devices, id)
}

/// Core grab-modifier validity: any combination of the 8 modifier
/// masks, or AnyModifier (0x8000) alone.
fn xi1_modifiers_valid(modifiers: u16) -> bool {
    modifiers == 0x8000 || modifiers & !0x00ff == 0
}

/// XI 1.x event classes pack the device id in the upper byte
/// (`deviceid << 8 | event_offset`).
fn xi1_event_class_device(class: u32) -> u16 {
    u16::try_from((class >> 8) & 0xff).unwrap_or(u16::MAX)
}

/// Standard 32-byte all-zero XI1 reply (status/count fields read as
/// Success / empty in every minor's reply layout).
fn xi1_zero_reply(
    byte_order: yserver_protocol::x11::ClientByteOrder,
    sequence: SequenceNumber,
) -> Vec<u8> {
    let mut reply = x11::fixed_reply(byte_order, sequence, 0, 0);
    reply.extend_from_slice(&[0u8; 24]);
    reply
}

/// Emit an X error for an XI 1.x request: major = 137, minor = the
/// request's minor opcode.
pub(super) fn xi1_error(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    code: u8,
    value: u32,
    minor: u8,
) -> io::Result<RequestOutcome> {
    emit_x11_error_with_minor(
        state,
        client_id,
        sequence,
        code,
        value,
        u16::from(minor),
        XI2_MAJOR_OPCODE,
    )
}

fn xi1_window_exists(state: &ServerState, xid: u32) -> bool {
    state.resources.window(ResourceId(xid)).is_some()
}

/// Collect the set of XI1 clients receiving a `SendExtensionEvent`
/// dispatched to `dest_window`. Implements Xorg
/// `Xi/exevents.c::SendEvent` (xserver.git:2952-2965) selection +
/// propagation, with two simplifications from the canonical path:
///
///  - per-window per-device do-not-propagate-mask is not yet modelled
///    (xts5 SendEvent 11/12 leave it empty), so the walk doesn't
///    trim classes as it ascends;
///  - `propagate` doesn't honour the `effectiveFocus` stop when
///    `dest_window` was resolved via `InputFocus` (the per-test
///    coverage there is XTS purposes 7-10, which all use
///    propagate=False).
///
/// `mask_is_zero` reflects the `CreateMaskFromList` result: empty
/// when the only class in the list is `noextensioneventclass`
/// (`_noExtensionEvent = 9`). On a zero mask, delivery falls through
/// to the window's creator — Xorg's `DeliverToWindowOwner` short-
/// circuit on `filter == CantBeFiltered`.
fn xi1_send_extension_event_resolve_targets(
    state: &ServerState,
    dest_window: ResourceId,
    request_classes: &[u32],
    mask_is_zero: bool,
    propagate: bool,
) -> std::collections::HashSet<ClientId> {
    let mut current = dest_window;
    // The active class set shrinks as the walk crosses windows whose
    // do-not-propagate-list bans a class (Xorg
    // `wOtherInputMasks(pWin)->dontPropagateMask[d->id]`). When the
    // remaining set is empty, propagation stops without delivery —
    // xts5 XSendExtensionEvent-8.
    let mut active: std::collections::HashSet<u32> = request_classes.iter().copied().collect();
    for _ in 0..256 {
        let mut targets: std::collections::HashSet<ClientId> = std::collections::HashSet::new();
        if mask_is_zero {
            if let Some(owner) = state.resources.window_owner(current) {
                targets.insert(owner);
            }
        } else {
            for (cid, c) in &state.clients {
                if let Some(set) = c.xi1_window_event_classes.get(&current)
                    && active.iter().any(|cl| set.contains(cl))
                {
                    targets.insert(ClientId(*cid));
                }
            }
        }
        if !targets.is_empty() || !propagate {
            return targets;
        }
        // No selector at `current` — apply this window's
        // do-not-propagate list to the active class set before walking
        // up, then move to the parent.
        if let Some(blocked) = state.xi1_window_dont_propagate.get(&current) {
            active.retain(|c| !blocked.contains(c));
            if active.is_empty() {
                return std::collections::HashSet::new();
            }
        }
        match state.resources.parent_of(current) {
            Some(parent) if parent != current => current = parent,
            _ => return std::collections::HashSet::new(),
        }
    }
    std::collections::HashSet::new()
}

/// XISetFocus (XI 2.0 minor 49) — Xorg `ProcXISetFocus`
/// (Xi/xisetdevfocus.c): `SetInputFocus(dev, focus, RevertToParent,
/// time, followOK=TRUE)` on a device with a focus class, i.e. a keyboard.
/// Pointers and unknown ids are BadDevice (Xorg sets no errorValue).
///
/// The master keyboard's focus is the core focus, so device 3 runs core
/// SetInputFocus. Slave keyboard facets keep their own focus
/// (`xi1_device_focus`, shared with XI1 SetDeviceFocus) and may follow
/// the keyboard. FollowKeyboard on the master keyboard itself would make
/// it follow itself: Xorg stores `FollowKeyboardWin` and then crashes
/// dereferencing it (Xvfb 21.1.24 segfaults at address 0x7), so yserver
/// answers BadValue instead.
fn handle_xi_set_focus(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    use crate::core_loop::xi1_focus;
    const MINOR: u8 = 49;
    // Length is gated to at least 4 units above.
    let focus = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
    let req_time = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
    let deviceid = u16::from_le_bytes([body[8], body[9]]);
    let role = state.xi_devices.role(deviceid);
    if !matches!(
        role,
        Some(
            crate::xinput::XiDeviceRole::MasterKeyboard
                | crate::xinput::XiDeviceRole::SlaveKeyboard
        )
    ) {
        return xi1_error(state, client_id, sequence, XI1_ERROR_BAD_DEVICE, 0, MINOR);
    }
    let revert_to = xi1_focus::REVERT_TO_PARENT;
    if role == Some(crate::xinput::XiDeviceRole::MasterKeyboard) {
        if focus == xi1_focus::FOCUS_FOLLOW_KEYBOARD {
            return xi1_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                focus,
                MINOR,
            );
        }
        if let Some((code, value)) = core_focus_window_error(state, ResourceId(focus)) {
            return xi1_error(state, client_id, sequence, code, value, MINOR);
        }
        apply_core_input_focus(
            state,
            client_id,
            sequence,
            ResourceId(focus),
            revert_to,
            req_time,
        );
        return Ok(RequestOutcome::Handled);
    }
    if focus != xi1_focus::FOCUS_FOLLOW_KEYBOARD
        && let Some((code, value)) = core_focus_window_error(state, ResourceId(focus))
    {
        return xi1_error(state, client_id, sequence, code, value, MINOR);
    }
    // Timestamp gate (dix/events.c:4920-4922), as XI1 SetDeviceFocus.
    let now = state.timestamp_now();
    let time = if req_time == 0 { now } else { req_time };
    let prev = xi1_focus::device_focus(state, deviceid);
    if xi1_focus::time_after(time, now) || xi1_focus::time_after(prev.time, time) {
        debug!(
            "client {} #{} XISetFocus device={deviceid} stale time {time} \
             (now={now} last={}) — ignored",
            client_id.0, sequence.0, prev.time
        );
        return Ok(RequestOutcome::Handled);
    }
    xi1_focus::set_device_focus(state, deviceid, focus, revert_to, time);
    debug!(
        "client {} #{} XISetFocus device={deviceid} focus=0x{focus:x}",
        client_id.0, sequence.0
    );
    Ok(RequestOutcome::Handled)
}

/// XIChangeHierarchy (XI 2.0 minor 43) — Xorg `ProcXIChangeHierarchy`
/// (Xi/xichangehierarchy.c) over yserver's fixed device set: masters 2/3
/// (the virtual core pair) and one fixed slave each (4/5).
///
/// The change list is walked exactly as Xorg walks it — same length
/// checks, unknown change types skipped, processing stops at the first
/// failing change — and every change gets the answer Xorg gives for the
/// same devices:
/// - RemoveMaster: return_mode must be Float/AttachToMaster (BadValue);
///   the virtual core pair can never be removed (BadDevice), and a slave
///   is not a master (BadDevice, errorValue = id).
/// - AttachSlave / DetachSlave: masters are BadDevice (errorValue = id);
///   yserver's slaves are fixed, which Xorg answers for its own fixed
///   slaves (the XTest devices, ids 4/5 on Xvfb) with BadDevice,
///   errorValue = id.
/// - AddMaster: Xorg creates a new pair; yserver can't add devices, so it
///   gives Xorg's answer when no device can be allocated (BadAlloc).
///
/// Unknown device ids are BadDevice with errorValue 0 (dixLookupDevice
/// sets none). No change ever succeeds, so no HierarchyChanged event is
/// sent. The change records are opaque to the request swapper and are
/// read in the client's byte order (Xorg swaps only type/length/name_len
/// and reads the device ids unswapped — for a big-endian client on a
/// little-endian server that only changes the errorValue of the same
/// BadDevice error).
fn handle_xi_change_hierarchy(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    byte_order: yserver_protocol::x11::ClientByteOrder,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    use yserver_protocol::x11::ClientByteOrder;
    const MINOR: u8 = 43;
    const ADD_MASTER: u16 = 1;
    const REMOVE_MASTER: u16 = 2;
    const ATTACH_SLAVE: u16 = 3;
    const DETACH_SLAVE: u16 = 4;
    const ATTACH_TO_MASTER: u8 = 1;
    const FLOATING: u8 = 2;
    let rd16 = |o: usize| {
        let b = [body[o], body[o + 1]];
        match byte_order {
            ClientByteOrder::LittleEndian => u16::from_le_bytes(b),
            ClientByteOrder::BigEndian => u16::from_be_bytes(b),
        }
    };
    let fail = |state: &mut ServerState, code: u8, value: u32| {
        xi1_error(state, client_id, sequence, code, value, MINOR)
    };
    // Length is gated to at least 2 units above: num_changes + pad.
    let num_changes = body[0];
    let mut pos = 4usize;
    for _ in 0..num_changes {
        let len = body.len() - pos;
        if len < 4 {
            return fail(state, x11::error::BAD_LENGTH, 0);
        }
        let change_type = rd16(pos);
        let change_bytes = usize::from(rd16(pos + 2)) * 4;
        if len < change_bytes {
            return fail(state, x11::error::BAD_LENGTH, 0);
        }
        // CHANGE_SIZE_MATCH: fixed-size records must state their own size.
        let size_matches = |size: usize| len >= size && change_bytes == size;
        match change_type {
            ADD_MASTER => {
                if len < 8 || usize::from(rd16(pos + 4)) > len - 8 {
                    return fail(state, x11::error::BAD_LENGTH, 0);
                }
                return fail(state, x11::error::BAD_ALLOC, 0);
            }
            REMOVE_MASTER => {
                if !size_matches(12) {
                    return fail(state, x11::error::BAD_LENGTH, 0);
                }
                let return_mode = body[pos + 6];
                if return_mode != ATTACH_TO_MASTER && return_mode != FLOATING {
                    return fail(state, x11::error::BAD_VALUE, 0);
                }
                let deviceid = rd16(pos + 4);
                // A slave is "not a master" (errorValue = id); the virtual
                // core pair can't be removed and unknown ids don't exist
                // (Xorg sets no errorValue for either).
                let value = if xi1_device_valid(&state.xi_devices, deviceid)
                    && !xi1_device_is_master(&state.xi_devices, deviceid)
                {
                    u32::from(deviceid)
                } else {
                    0
                };
                return fail(state, XI1_ERROR_BAD_DEVICE, value);
            }
            ATTACH_SLAVE | DETACH_SLAVE => {
                if !size_matches(8) {
                    return fail(state, x11::error::BAD_LENGTH, 0);
                }
                let deviceid = rd16(pos + 4);
                // Masters and the fixed slaves report their id; unknown
                // ids report 0.
                let value = if xi1_device_valid(&state.xi_devices, deviceid) {
                    u32::from(deviceid)
                } else {
                    0
                };
                return fail(state, XI1_ERROR_BAD_DEVICE, value);
            }
            _ => {}
        }
        pos += change_bytes;
    }
    debug!(
        "client {} #{} XIChangeHierarchy: {num_changes} change(s), nothing to do",
        client_id.0, sequence.0
    );
    Ok(RequestOutcome::Handled)
}

/// Check the variable arrays that Xorg validates with `REQUEST_FIXED_SIZE`
/// or by walking XISelectEvents records. All typed count fields have been
/// swapped to little-endian by the reader before this production entry point.
fn xi2_request_has_complete_dynamic_tail(minor: u8, body: &[u8]) -> bool {
    fn u16_at(body: &[u8], offset: usize) -> Option<usize> {
        let bytes = body.get(offset..offset.checked_add(2)?)?;
        Some(usize::from(u16::from_le_bytes([bytes[0], bytes[1]])))
    }

    fn words_fit(body: &[u8], start: usize, words: usize) -> Option<usize> {
        let bytes = words.checked_mul(4)?;
        let end = start.checked_add(bytes)?;
        (end <= body.len()).then_some(end)
    }

    match minor {
        // SProcXISelectEvents checks each xXIEventMask header and mask_len
        // region, returning BadLength before ProcXISelectEvents mutates masks.
        46 => {
            let Some(num_masks) = u16_at(body, 4) else {
                return false;
            };
            let mut pos = 8usize;
            for _ in 0..num_masks {
                let Some(device_and_len_end) = pos.checked_add(4) else {
                    return false;
                };
                if device_and_len_end > body.len() {
                    return false;
                }
                let Some(mask_words) = u16_at(body, pos + 2) else {
                    return false;
                };
                let Some(mask_end) = words_fit(body, device_and_len_end, mask_words) else {
                    return false;
                };
                pos = mask_end;
            }
            true
        }
        // ProcXIGrabDevice checks mask_len words after its 20-byte body
        // prefix. Those words are byte masks and remain unswapped.
        51 => {
            let Some(mask_len) = u16_at(body, 18) else {
                return false;
            };
            words_fit(body, 20, mask_len).is_some()
        }
        // SProcXIPassiveGrabDevice checks the mask and modifier spans together
        // before its modifier CARD32 swap loop.
        54 => {
            let (Some(num_modifiers), Some(mask_len)) = (u16_at(body, 18), u16_at(body, 20)) else {
                return false;
            };
            let Some(mask_end) = words_fit(body, 28, mask_len) else {
                return false;
            };
            words_fit(body, mask_end, num_modifiers).is_some()
        }
        // SProcXIPassiveUngrabDevice checks num_modifiers CARD32s after its
        // 16-byte fixed body.
        55 => {
            let Some(num_modifiers) = u16_at(body, 10) else {
                return false;
            };
            words_fit(body, 16, num_modifiers).is_some()
        }
        _ => true,
    }
}

/// Record a client's XI version the way Xorg `ProcXIQueryVersion` stores
/// it in `XIClientRec`: the first query sets it; a later query raises it
/// only when both the stored and the new version are 2.2 or newer
/// ("Peter promises to never again break backward compatibility").
/// Otherwise the stored version stays — Xorg then answers a lower request
/// with BadValue and a higher one with the stored version; yserver keeps
/// answering with the negotiated version, so only the stored value (what
/// `FilterRawEvents` reads) follows Xorg here.
fn record_xi2_client_version(state: &mut ServerState, client_id: ClientId, version: (u16, u16)) {
    match state.xi2_client_versions.get(&client_id) {
        None => {
            state.xi2_client_versions.insert(client_id, version);
        }
        Some(&stored) => {
            if version >= (2, 2) && stored >= (2, 2) && version > stored {
                state.xi2_client_versions.insert(client_id, version);
            }
        }
    }
}

fn emit_xi2_device_changed_bootstrap(
    state: &mut ServerState,
    _backend: &mut dyn Backend,
    _origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    major_opcode: u8,
) -> io::Result<()> {
    let time = state.timestamp_now();
    let Some((byte_order, selected_masters)) = state.clients.get(&client_id.0).map(|client| {
        (
            client.byte_order,
            [
                crate::xinput::DEVICEID_MASTER_POINTER,
                crate::xinput::DEVICEID_MASTER_KEYBOARD,
            ]
            .into_iter()
            .filter(|master_id| {
                client
                    .xi2_masks
                    .iter()
                    .any(|(&(window, device_id), &mask)| {
                        window == ROOT_WINDOW
                            && (matches!(device_id, 0 | 1) || device_id == *master_id)
                            && (mask & u64::from(XI2_DEVICE_CHANGED_MASK)) != 0
                    })
            })
            .collect::<Vec<_>>(),
        )
    }) else {
        return Ok(());
    };
    let mut buf = Vec::new();
    for master_id in selected_masters {
        // Xorg serializes the source ID retained by each copied class
        // (`Xi/xiquerydevice.c:278,326`); it is independent of `lastSlave`,
        // which may already have been cleared by disable/removal.
        let Some(sourceid) = state
            .xi_devices
            .device(master_id)
            .map(|device| device.class_sourceid)
        else {
            continue;
        };
        let Some((classes, num_classes)) =
            crate::xinput::hotplug::device_changed_class_block(state, master_id, byte_order)
        else {
            continue;
        };
        let mut event_buf = Vec::new();
        x11::encode_xi2_device_changed_event(
            &mut event_buf,
            byte_order,
            sequence,
            major_opcode,
            master_id,
            time,
            num_classes,
            sourceid,
            crate::xinput::hotplug::XiDeviceChangeReason::SlaveSwitch as u8,
            &classes,
        );
        buf.extend_from_slice(&event_buf);
    }
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(());
    };
    let _outcome = write_to_client(client, client_id, &buf);
    Ok(())
}
