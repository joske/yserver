//! Core-side event fanout helpers — the state-borrowing replacements
//! for `server::fanout_event` / `server::fanout_raw_event` /
//! `server::pointer_event_fanout`.
//!
//! Each helper takes `&mut ServerState` so it can update each
//! client's `last_sequence`, encode against the client's
//! `byte_order`, and push bytes through `client_io::write_or_buffer`
//! — the same path opcode dispatch will use after the D3 lift.
//! Disconnect outcomes are reported back to the caller as a
//! `Vec<ClientId>` so the core's request-loop can issue
//! `Message::ClientDisconnected` for each one.
//!
//! The pre-lift `EventTarget`-based helpers in `server.rs` remain in
//! place; D3 migrates callers off them.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        LazyLock, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use yserver_protocol::x11::{self, ClientByteOrder, ClientId, ResourceId, SequenceNumber};

use crate::{
    core_loop::client_io::{self, WriteOutcome},
    host_x11::{HostExposeEvent, HostXidMap},
    resources::{MapState, ROOT_WINDOW},
    server::ServerState,
    xinput::XI2_DEVICE_CHANGED_MASK,
};

/// Wire-frame classes emitted to clients while loop telemetry is enabled.
/// Keeping replies and errors alongside events makes retry/synchronization
/// feedback visible, not just asynchronous event fanout.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum OutboundTelemetryKind {
    Reply,
    Error(u8),
    Event(u8),
    GenericEvent { extension: u8, event_type: u16 },
}

static OUTBOUND_TELEMETRY_ENABLED: AtomicBool = AtomicBool::new(false);
static OUTBOUND_TELEMETRY: LazyLock<Mutex<HashMap<(ClientId, OutboundTelemetryKind), u64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) fn enable_outbound_telemetry() {
    OUTBOUND_TELEMETRY_ENABLED.store(true, Ordering::Relaxed);
    OUTBOUND_TELEMETRY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
}

pub(crate) fn take_outbound_telemetry() -> HashMap<(ClientId, OutboundTelemetryKind), u64> {
    let mut counters = OUTBOUND_TELEMETRY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::mem::take(&mut *counters)
}

/// Attribute one server-to-client protocol frame. Callers invoke this before
/// the non-blocking write, so a full socket still counts the feedback the
/// server attempted to deliver. Buffers produced here contain one reply,
/// error, or event (occasionally a chain of 32-byte events).
pub(crate) fn record_outbound_telemetry(client: ClientId, order: ClientByteOrder, bytes: &[u8]) {
    if !OUTBOUND_TELEMETRY_ENABLED.load(Ordering::Relaxed) || bytes.is_empty() {
        return;
    }
    let mut offset = 0;
    let mut counters = OUTBOUND_TELEMETRY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    while offset < bytes.len() {
        let frame = &bytes[offset..];
        let response_type = frame[0] & 0x7f;
        let (kind, frame_len) = match response_type {
            0 => (
                OutboundTelemetryKind::Error(*frame.get(1).unwrap_or(&0)),
                32,
            ),
            1 => (OutboundTelemetryKind::Reply, bytes.len() - offset),
            35 if frame.len() >= 10 => {
                let event_type = match order {
                    ClientByteOrder::LittleEndian => u16::from_le_bytes([frame[8], frame[9]]),
                    ClientByteOrder::BigEndian => u16::from_be_bytes([frame[8], frame[9]]),
                };
                let extra_units = if frame.len() >= 8 {
                    match order {
                        ClientByteOrder::LittleEndian => {
                            u32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]])
                        }
                        ClientByteOrder::BigEndian => {
                            u32::from_be_bytes([frame[4], frame[5], frame[6], frame[7]])
                        }
                    }
                } else {
                    0
                };
                let frame_len = 32_usize.saturating_add(
                    usize::try_from(extra_units)
                        .unwrap_or(usize::MAX)
                        .saturating_mul(4),
                );
                (
                    OutboundTelemetryKind::GenericEvent {
                        extension: frame[1],
                        event_type,
                    },
                    frame_len,
                )
            }
            event => (OutboundTelemetryKind::Event(event), 32),
        };
        *counters.entry((client, kind)).or_default() += 1;
        if frame_len == 0 || frame_len > frame.len() {
            break;
        }
        offset += frame_len;
    }
}

/// Build a deduped client-id list of every client that selected at
/// least one of the bits in `mask_bits` on `window`.
///
/// Replaces `ServerState::subscribers` for the new fanout API. Order
/// follows `HashMap` iteration — already non-deterministic in the old
/// path, so wire ordering is unchanged.
pub fn subscribers_by_id(state: &ServerState, window: ResourceId, mask_bits: u32) -> Vec<ClientId> {
    state
        .clients
        .iter()
        .filter_map(|(id, c)| {
            let mask = c.event_masks.get(&window).copied().unwrap_or(0);
            if mask & mask_bits != 0 {
                Some(ClientId(*id))
            } else {
                None
            }
        })
        .collect()
}

/// Walk up the parent chain from `start`, returning the first window
/// with any client subscribed to `mask_bits`, the (event_x, event_y)
/// translated to be relative to that window, and the subscriber list.
///
/// Mirror of `ServerState::pointer_propagation_target` but in the new
/// state-borrowing fanout API: returns `Vec<ClientId>` instead of
/// `Vec<EventTarget>`. Order follows `HashMap` iteration (matches the
/// pre-lift behaviour).
/// Returns `(propagation_target, x, y, subscribers, child)` where
/// `child` is the immediate descendant of `propagation_target` along
/// the path to `start` (i.e. the X11 `child` field for ButtonPress /
/// Motion events). `child == ResourceId(0)` is the X11 `None` sentinel
/// and indicates the propagation target was reached without walking up
/// (the click landed directly on the subscribed window).
///
/// Window managers use this `child` field to distinguish a bare-root
/// click — for which they typically open the root menu — from a click
/// on an application window that happened to propagate up because the
/// app didn't select core ButtonPress (modern toolkits select XI2
/// instead). Without an accurate `child`, fvwm3's `Mouse 1 R A Menu`
/// binding fires on every click anywhere in the screen.
#[must_use]
pub fn pointer_propagation_target_by_id(
    state: &ServerState,
    start: ResourceId,
    start_x: i16,
    start_y: i16,
    mask_bits: u32,
) -> Option<(ResourceId, i16, i16, Vec<ClientId>, ResourceId)> {
    let mut current = start;
    let mut x = start_x;
    let mut y = start_y;
    let mut child: Option<ResourceId> = None;
    for _ in 0..256 {
        let subs = subscribers_by_id(state, current, mask_bits);
        if !subs.is_empty() {
            return Some((current, x, y, subs, child.unwrap_or(ResourceId(0))));
        }
        let window = state.resources.window(current)?;
        if window.parent == current {
            return None;
        }
        // #133 step 8: the inverse of the hit-test walk's
        // `to_content_coords`, border term included.
        (x, y) = window.to_parent_coords(x, y);
        child = Some(current);
        current = window.parent;
    }
    None
}

/// Returns `Some(client_id)` if `client_id` corresponds to a registered
/// client. Mirror of `ServerState::client_target` in the new fanout
/// API — useful as a guard before fanning out to a single client.
#[must_use]
pub fn client_target_id(state: &ServerState, client_id: ClientId) -> Option<ClientId> {
    state
        .clients
        .contains_key(&client_id.0)
        .then_some(client_id)
}

/// Mirror of `ServerState::selection_owner_target` in the new fanout
/// API: returns the owner window and the owning client's id.
#[must_use]
pub fn selection_owner_target_id(
    state: &ServerState,
    selection: yserver_protocol::x11::AtomId,
) -> Option<(ResourceId, ClientId)> {
    let owner_window = state.selections.get(&selection)?.0;
    let owner_client = state.resources.window_owner(owner_window)?;
    let target = client_target_id(state, owner_client)?;
    Some((owner_window, target))
}

/// `encode(buf, sequence, byte_order)` writes a 32-byte (or larger)
/// X11 event into `buf` against the given sequence/byte order. Same
/// contract as `server::fanout_event`.
pub fn fanout_event_to_clients<F>(
    state: &mut ServerState,
    client_ids: &[ClientId],
    encode: F,
) -> Vec<ClientId>
where
    F: Fn(&mut Vec<u8>, SequenceNumber, ClientByteOrder),
{
    let mut disconnected = Vec::new();
    let mut seen = HashSet::new();
    for cid in client_ids {
        if !seen.insert(cid.0) {
            continue;
        }
        let Some(client) = state.clients.get_mut(&cid.0) else {
            continue;
        };
        let seq = SequenceNumber(client.last_sequence.load(Ordering::Relaxed));
        let order = client.byte_order;
        let mut buf = Vec::with_capacity(32);
        encode(&mut buf, seq, order);
        record_outbound_telemetry(*cid, order, &buf);
        match client_io::write_or_buffer(client, &buf) {
            Ok(WriteOutcome::Done | WriteOutcome::WouldBlock) => {}
            Ok(WriteOutcome::Disconnect) => disconnected.push(*cid),
            Err(_) => disconnected.push(*cid),
        }
    }
    disconnected
}

/// State-borrowing replacement for `server::emit_window_event`.
///
/// Looks up subscribers to `mask_bits` on `window` directly out of
/// `state.clients`, then encodes per client and writes via
/// `client_io::write_or_buffer`. Returns the list of clients whose
/// outbound buffer overflowed so the core's request loop can issue
/// `Message::ClientDisconnected` for each.
pub fn emit_window_event_to_state<F>(
    state: &mut ServerState,
    window: ResourceId,
    mask_bits: u32,
    encode: F,
) -> Vec<ClientId>
where
    F: Fn(&mut Vec<u8>, SequenceNumber, ClientByteOrder),
{
    let targets = subscribers_by_id(state, window, mask_bits);
    if targets.is_empty() {
        return Vec::new();
    }
    fanout_event_to_clients(state, &targets, encode)
}

/// State-borrowing replacement for `nested::emit_xi2_focus_event`.
///
/// Emits an XI2 FocusIn / FocusOut on `window` to clients selecting
/// the matching XI2 evtype on `(window, deviceid)` for any of the
/// fallback device candidates `[5, 3, 1, 0]` (slave keyboard, then
/// master keyboard, then AllMasterDevices, then AllDevices). The
/// encoding is byte-order agnostic, matching the pre-lift helper.
///
/// `xi2_major_opcode` is the XI extension's runtime-assigned major
/// opcode (137 in the current build).
pub fn emit_xi2_focus_event_to_state(
    state: &mut ServerState,
    window: ResourceId,
    evtype: u16,
    xi2_major_opcode: u8,
    mode: u8,
    detail: u8,
    root_x: i16,
    root_y: i16,
) -> Vec<ClientId> {
    let targets: Vec<ClientId> = state
        .clients
        .iter()
        .filter_map(|(id, client)| {
            let mask = client
                .xi2_masks
                .get(&(window, 5))
                .or_else(|| client.xi2_masks.get(&(window, 3)))
                .or_else(|| client.xi2_masks.get(&(window, 1)))
                .or_else(|| client.xi2_masks.get(&(window, 0)))
                .copied()
                .unwrap_or(0);
            if mask & (1 << evtype) != 0 {
                Some(ClientId(*id))
            } else {
                None
            }
        })
        .collect();
    if targets.is_empty() {
        return Vec::new();
    }
    // Focus events carry the pointer position (xXIEnterEvent layout).
    // `event_x/event_y` are relative to the focus window's origin.
    let (origin_x, origin_y) = state.resources.window_absolute_position(window);
    let event_x = i16::try_from(i32::from(root_x).saturating_sub(origin_x)).unwrap_or(i16::MAX);
    let event_y = i16::try_from(i32::from(root_y).saturating_sub(origin_y)).unwrap_or(i16::MAX);
    fanout_event_to_clients(state, &targets, |buf, seq, order| {
        x11::encode_xi2_focus_event(
            buf,
            order,
            seq,
            xi2_major_opcode,
            evtype,
            3,
            0,
            window,
            root_x,
            root_y,
            event_x,
            event_y,
            mode,
            detail,
        );
    })
}

/// `XIDeviceChange` reason (XI2.h:108) — the device's own classes/name
/// changed (as opposed to `XISlaveSwitch`, which reports a master's
/// active slave swap).
const XI_REASON_DEVICE_CHANGE: u8 = 2;

/// Emit an XI2 `XI_DeviceChanged` for the slave pointer (device 4) to
/// every client that selected `XI_DeviceChanged` on it.
///
/// Called after `xi_seed_touchpad` / `xi_clear_touchpad` so a running
/// desktop re-reads device 4 (picking up the new name + libinput
/// properties when a touchpad appears, or the reverted defaults when it
/// disappears). The carried class set mirrors the device-4 classes the
/// XIQueryDevice handler reports (button + 4 valuators + 2 scroll), so a
/// client re-querying after the event sees a consistent device.
///
/// Selection matches device 4 explicitly, plus the `XIAllDevices` (0)
/// and `XIAllMasterDevices` (1) wildcards a client may have used. If no
/// client selected, this is a no-op. Returns clients whose outbound
/// buffer overflowed (for `ClientDisconnected` reporting).
///
/// `xi2_major_opcode` is the XI extension's runtime-assigned major
/// opcode (137 in the current build).
pub fn emit_xi2_device_changed_slave_pointer(
    state: &mut ServerState,
    xi2_major_opcode: u8,
) -> Vec<ClientId> {
    const SLAVE_POINTER: u16 = 4;

    // Clients select on (window, deviceid). DeviceChanged is a
    // hierarchy-wide event clients select on the ROOT window (the same
    // window `process_request`'s XISelectEvents bootstrap requires before
    // it sends the initial DeviceChanged). Match the root window only —
    // matching any window would spuriously deliver to a client that
    // selected DeviceChanged on some unrelated child. Device-id match
    // covers device 4 plus the AllDevices(0)/AllMasterDevices(1)
    // wildcards.
    let targets: Vec<ClientId> = state
        .clients
        .iter()
        .filter_map(|(id, client)| {
            let selected = client.xi2_masks.iter().any(|(&(window, dev), &mask)| {
                window == ROOT_WINDOW
                    // NOTE: XIAllMasterDevices(1) technically covers
                    // masters only; device 4 is a slave. Kept for
                    // delivery breadth; real clients select via
                    // XIAllDevices(0).
                    && matches!(dev, SLAVE_POINTER | 0 | 1)
                    && (mask & XI2_DEVICE_CHANGED_MASK) != 0
            });
            selected.then_some(ClientId(*id))
        })
        .collect();
    if targets.is_empty() {
        return Vec::new();
    }

    let (classes, num_classes) = build_slave_pointer_class_block(state);
    let time = state.timestamp_now();
    fanout_event_to_clients(state, &targets, |buf, seq, order| {
        x11::encode_xi2_device_changed_event(
            buf,
            order,
            seq,
            xi2_major_opcode,
            SLAVE_POINTER,
            time,
            num_classes,
            SLAVE_POINTER, // sourceid = the device itself
            XI_REASON_DEVICE_CHANGE,
            &classes,
        );
    })
}

/// Build the XI2 device-class block for the slave pointer (device 4),
/// returning `(class_bytes, num_classes)`.
///
/// This is the SINGLE source of truth for device 4's class shape:
/// Button(7) + Valuator×4 (X, Y, vert-scroll, horiz-scroll) + Scroll×2
/// (vert, horiz). Both XIQueryDevice's device-4 path (opcode 48) and the
/// touchpad-add/remove `XI_DeviceChanged` fanout call it, so the bytes can
/// never drift apart (asserted byte-identical by
/// `query_device_4_matches_device_changed_block`). `num_classes` is
/// derived from the writes below, not hardcoded at the call sites.
///
/// The scroll valuators MUST stay declared — dropping them fires a
/// Gdk-CRITICAL (`_gdk_x11_device_xi2_add_scroll_valuator` asserts the
/// scroll axis index is within the valuator count).
pub(crate) fn build_slave_pointer_class_block(state: &mut ServerState) -> (Vec<u8>, u16) {
    const DEVICEID: u16 = 4;

    fn write_button_class(buf: &mut Vec<u8>, sourceid: u16, label_atoms: &[x11::AtomId]) {
        let le = ClientByteOrder::LittleEndian;
        let num_buttons = u16::try_from(label_atoms.len()).unwrap_or(u16::MAX);
        let state_words = num_buttons.div_ceil(32) as usize;
        let byte_len = 8 + 4 * state_words + 4 * num_buttons as usize;
        x11::write_u16(le, buf, 1); // type = Button
        x11::write_u16(le, buf, (byte_len / 4) as u16);
        x11::write_u16(le, buf, sourceid);
        x11::write_u16(le, buf, num_buttons);
        buf.extend(std::iter::repeat_n(0u8, 4 * state_words));
        for atom in label_atoms {
            x11::write_u32(le, buf, atom.0);
        }
    }

    fn write_valuator_class(
        buf: &mut Vec<u8>,
        sourceid: u16,
        number: u16,
        label_atom: x11::AtomId,
        min_int: i32,
        max_int: i32,
        mode: u8,
        value: i32,
    ) {
        let le = ClientByteOrder::LittleEndian;
        x11::write_u16(le, buf, 2); // type = Valuator
        x11::write_u16(le, buf, 11);
        x11::write_u16(le, buf, sourceid);
        x11::write_u16(le, buf, number);
        x11::write_u32(le, buf, label_atom.0);
        x11::write_u32(le, buf, min_int as u32);
        x11::write_u32(le, buf, 0);
        x11::write_u32(le, buf, max_int as u32);
        x11::write_u32(le, buf, 0);
        x11::write_u32(le, buf, value as u32);
        x11::write_u32(le, buf, 0);
        x11::write_u32(le, buf, 0); // resolution
        buf.push(mode);
        buf.extend_from_slice(&[0u8; 3]);
    }

    fn write_scroll_class(buf: &mut Vec<u8>, sourceid: u16, number: u16, scroll_type: u16) {
        let le = ClientByteOrder::LittleEndian;
        x11::write_u16(le, buf, 3); // type = Scroll
        x11::write_u16(le, buf, 6);
        x11::write_u16(le, buf, sourceid);
        x11::write_u16(le, buf, number);
        x11::write_u16(le, buf, scroll_type);
        x11::write_u16(le, buf, 0); // pad
        x11::write_u32(le, buf, 0); // flags
        x11::write_u32(le, buf, 1); // increment = 1.0
        x11::write_u32(le, buf, 0);
    }

    let pointer = (
        i32::from(state.randr.screen_width) / 2,
        i32::from(state.randr.screen_height) / 2,
    );
    let button_labels = [
        state.atoms.intern("Button Left", false),
        state.atoms.intern("Button Middle", false),
        state.atoms.intern("Button Right", false),
        state.atoms.intern("Button Wheel Up", false),
        state.atoms.intern("Button Wheel Down", false),
        state.atoms.intern("Button Horiz Wheel Left", false),
        state.atoms.intern("Button Horiz Wheel Right", false),
    ];
    let axis_labels = [
        state.atoms.intern("Rel X", false),
        state.atoms.intern("Rel Y", false),
        state.atoms.intern("Rel Vert Scroll", false),
        state.atoms.intern("Rel Horiz Scroll", false),
    ];
    let scroll = state.scroll_axis_value;

    // `num_classes` is incremented per class written below so the count
    // can never drift from the actual byte content.
    let mut classes = Vec::new();
    let mut num_classes = 0u16;
    write_button_class(&mut classes, DEVICEID, &button_labels);
    num_classes += 1;
    write_valuator_class(
        &mut classes,
        DEVICEID,
        0,
        axis_labels[0],
        -1,
        -1,
        0,
        pointer.0,
    );
    num_classes += 1;
    write_valuator_class(
        &mut classes,
        DEVICEID,
        1,
        axis_labels[1],
        -1,
        -1,
        0,
        pointer.1,
    );
    num_classes += 1;
    write_valuator_class(
        &mut classes,
        DEVICEID,
        2,
        axis_labels[2],
        -1,
        0,
        0,
        scroll[0],
    );
    num_classes += 1;
    write_valuator_class(
        &mut classes,
        DEVICEID,
        3,
        axis_labels[3],
        -1,
        0,
        0,
        scroll[1],
    );
    num_classes += 1;
    write_scroll_class(&mut classes, DEVICEID, 2, 1); // vertical
    num_classes += 1;
    write_scroll_class(&mut classes, DEVICEID, 3, 2); // horizontal
    num_classes += 1;
    (classes, num_classes)
}

/// State-borrowing replacement for `nested::expose_event_fanout`.
///
/// Translates the host xid via `xid_map`, emits an Expose event to
/// every client that selected `ExposureMask` on the resolved nested
/// window, and (for top-level exposes) walks descendants in the
/// exposed area so sub-window children also redraw.
///
/// Returns a deduped list of clients whose outbound buffer overflowed
/// during the fanout.
pub fn expose_event_fanout_to_state(
    state: &mut ServerState,
    xid_map: &HostXidMap,
    ev: HostExposeEvent,
) -> Vec<ClientId> {
    let Some(window) = xid_map.get(&ev.host_xid).copied() else {
        return Vec::new();
    };
    let mut dropped =
        emit_window_event_to_state(state, window, EXPOSURE_MASK_BIT, |buf, seq, order| {
            x11::encode_expose_event(
                buf, seq, order, window, ev.x, ev.y, ev.width, ev.height, ev.count,
            );
        });
    if window == ROOT_WINDOW {
        return dropped;
    }
    let exposed = state.resources.descendants_in_exposed_area(
        window,
        ev.x as i16,
        ev.y as i16,
        ev.width,
        ev.height,
    );
    for rect in exposed {
        let target_window = rect.window;
        let more = emit_window_event_to_state(
            state,
            target_window,
            EXPOSURE_MASK_BIT,
            |buf, seq, order| {
                x11::encode_expose_event(
                    buf,
                    seq,
                    order,
                    target_window,
                    rect.x as u16,
                    rect.y as u16,
                    rect.width,
                    rect.height,
                    0,
                );
            },
        );
        merge_dropped(&mut dropped, more);
    }
    dropped
}

/// State-borrowing replacement for `nested::emit_expose_subtree`.
///
/// Walks every mapped descendant of `root` and emits Expose to those
/// that selected `ExposureMask`. Used after a top-level becomes
/// viewable so deeply-nested widgets repaint immediately.
pub fn emit_expose_subtree_to_state(state: &mut ServerState, root: ResourceId) -> Vec<ClientId> {
    let mut dropped = Vec::new();
    let children: Vec<ResourceId> = state.resources.children(root).to_vec();
    for child in children {
        let extents = state
            .resources
            .window(child)
            .filter(|w| w.map_state == MapState::Viewable)
            .map(|w| (w.width, w.height));
        if let Some((w, h)) = extents {
            let target = child;
            let more =
                emit_window_event_to_state(state, target, EXPOSURE_MASK_BIT, |buf, seq, order| {
                    x11::encode_expose_event(buf, seq, order, target, 0, 0, w, h, 0);
                });
            merge_dropped(&mut dropped, more);
            let recursed = emit_expose_subtree_to_state(state, child);
            merge_dropped(&mut dropped, recursed);
        }
    }
    dropped
}

/// Walks every now-Viewable descendant of `root` and emits
/// `VisibilityNotify(Unobscured)` to those that selected
/// `VisibilityChangeMask`. Used after a top-level is mapped: any
/// previously-mapped descendant transitioned Unviewable→Viewable, and
/// GTK3's frame clock keys content paints off VisibilityNotify
/// (see the call site in `handle_map_window` for the full rationale).
/// Without this, FF's profile-chooser child (mapped while its parent
/// was still unmapped) never gets the visibility transition and never
/// schedules a content paint — visible as "empty shadow".
pub fn emit_visibility_unobscured_subtree_to_state(
    state: &mut ServerState,
    root: ResourceId,
) -> Vec<ClientId> {
    let mut dropped = Vec::new();
    let children: Vec<ResourceId> = state.resources.children(root).to_vec();
    for child in children {
        let viewable = state
            .resources
            .window(child)
            .is_some_and(|w| w.map_state == MapState::Viewable);
        if viewable {
            let target = child;
            let more = emit_window_event_to_state(
                state,
                target,
                VISIBILITY_MASK_BIT,
                |buf, seq, order| {
                    x11::encode_visibility_notify_event(buf, seq, order, target, 0);
                },
            );
            merge_dropped(&mut dropped, more);
            let recursed = emit_visibility_unobscured_subtree_to_state(state, child);
            merge_dropped(&mut dropped, recursed);
        }
    }
    dropped
}

const EXPOSURE_MASK_BIT: u32 = 0x0000_8000;
const VISIBILITY_MASK_BIT: u32 = 0x0001_0000;

fn merge_dropped(into: &mut Vec<ClientId>, more: Vec<ClientId>) {
    for cid in more {
        if !into.contains(&cid) {
            into.push(cid);
        }
    }
}

/// Raw-event variant: `event` is a 32-byte template encoded in
/// `template_byte_order`. For each recipient we copy the template,
/// re-encode into the recipient's byte order via the per-event-type
/// swap table, then patch the sequence number in the recipient's
/// byte order.
///
/// `template_byte_order` is `LittleEndian` for events the server
/// builds itself (SelectionNotify, RANDR, …) and the sender's byte
/// order for `SendEvent`.
pub fn fanout_raw_event_to_clients(
    state: &mut ServerState,
    client_ids: &[ClientId],
    event: &[u8; 32],
    template_byte_order: ClientByteOrder,
) -> Vec<ClientId> {
    use yserver_protocol::x11::wire_swap;
    let mut disconnected = Vec::new();
    let mut seen = HashSet::new();
    let event_type = event[0] & 0x7f;
    let entries = wire_swap::core_event_swap_table(event_type);
    for cid in client_ids {
        if !seen.insert(cid.0) {
            continue;
        }
        let Some(client) = state.clients.get_mut(&cid.0) else {
            continue;
        };
        let recipient_order = client.byte_order;
        let mut buf = *event;
        // Step 1: undo source byte order to native LE so the swap to
        // recipient byte order produces correct bytes.
        wire_swap::swap_in_place(entries, template_byte_order, &mut buf);
        // Step 2: convert from native LE to recipient byte order.
        wire_swap::swap_in_place(entries, recipient_order, &mut buf);
        // Patch the sequence number in the recipient's byte order.
        let seq = client.last_sequence.load(Ordering::Relaxed);
        let seq_bytes = match recipient_order {
            ClientByteOrder::LittleEndian => seq.to_le_bytes(),
            ClientByteOrder::BigEndian => seq.to_be_bytes(),
        };
        buf[2] = seq_bytes[0];
        buf[3] = seq_bytes[1];
        record_outbound_telemetry(*cid, recipient_order, &buf);
        match client_io::write_or_buffer(client, &buf) {
            Ok(WriteOutcome::Done | WriteOutcome::WouldBlock) => {}
            Ok(WriteOutcome::Disconnect) => disconnected.push(*cid),
            Err(_) => disconnected.push(*cid),
        }
    }
    disconnected
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        io::Read,
        os::unix::net::UnixStream,
        sync::{Arc, Mutex, atomic::AtomicU16},
    };

    use crate::{
        resources::ROOT_WINDOW,
        server::{ClientState, ServerState},
    };

    fn make_client(writer: UnixStream, mask_for_root: u32) -> ClientState {
        ClientState {
            writer: Arc::new(Mutex::new(crate::transport::Transport::Unix(writer))),
            byte_order: ClientByteOrder::LittleEndian,
            last_sequence: Arc::new(AtomicU16::new(0)),
            resource_id_base: 0,
            resource_id_mask: 0,
            event_masks: HashMap::from([(ROOT_WINDOW, mask_for_root)]),
            save_set: HashSet::new(),
            big_requests_enabled: false,
            xi2_masks: HashMap::new(),
            xi1_event_classes: HashSet::new(),
            xi1_window_event_classes: HashMap::new(),
            outbound: VecDeque::new(),
            watching_writable: false,
            focused_window: ROOT_WINDOW,
            reader_control: None,
            is_local: true,
            fd_passing: true,
        }
    }

    fn install(state: &mut ServerState, id: u32, mask: u32) -> UnixStream {
        let (a, b) = UnixStream::pair().unwrap();
        let client = make_client(a, mask);
        state.clients.insert(id, client);
        b
    }

    #[test]
    fn subscribers_by_id_filters_by_mask_bit() {
        let mut state = ServerState::new();
        let _peer1 = install(&mut state, 1, 0x40_0000); // PropertyChange
        let _peer2 = install(&mut state, 2, 0x00_0001); // KeyPress only
        let mut got = subscribers_by_id(&state, ROOT_WINDOW, 0x40_0000);
        got.sort_by_key(|c| c.0);
        assert_eq!(got, vec![ClientId(1)]);
    }

    #[test]
    fn fanout_event_to_clients_writes_and_dedups() {
        let mut state = ServerState::new();
        let mut peer1 = install(&mut state, 1, 0xFFFF_FFFF);
        let mut peer2 = install(&mut state, 2, 0xFFFF_FFFF);
        let dropped = fanout_event_to_clients(
            &mut state,
            // Pass id=1 twice — dedup must collapse to a single send.
            &[ClientId(1), ClientId(2), ClientId(1)],
            |buf, _seq, _order| {
                buf.resize(32, 0);
                buf[0] = 0xAB;
            },
        );
        assert!(dropped.is_empty());

        let mut buf1 = [0u8; 64];
        let n1 = peer1.read(&mut buf1).unwrap();
        assert_eq!(n1, 32);
        assert_eq!(buf1[0], 0xAB);

        let mut buf2 = [0u8; 64];
        let n2 = peer2.read(&mut buf2).unwrap();
        assert_eq!(n2, 32);
    }

    #[test]
    fn fanout_raw_event_patches_sequence_per_client() {
        let mut state = ServerState::new();
        let mut peer1 = install(&mut state, 1, 0xFFFF_FFFF);
        let mut peer2 = install(&mut state, 2, 0xFFFF_FFFF);
        // Bump sequences so the two clients have distinct numbers.
        state
            .clients
            .get(&1)
            .unwrap()
            .last_sequence
            .store(0x1234, Ordering::Relaxed);
        state
            .clients
            .get(&2)
            .unwrap()
            .last_sequence
            .store(0x5678, Ordering::Relaxed);

        let template = [0xCDu8; 32];
        let dropped = fanout_raw_event_to_clients(
            &mut state,
            &[ClientId(1), ClientId(2)],
            &template,
            ClientByteOrder::LittleEndian,
        );
        assert!(dropped.is_empty());

        let mut got1 = [0u8; 32];
        peer1.read_exact(&mut got1).unwrap();
        assert_eq!(got1[2], 0x34);
        assert_eq!(got1[3], 0x12);

        let mut got2 = [0u8; 32];
        peer2.read_exact(&mut got2).unwrap();
        assert_eq!(got2[2], 0x78);
        assert_eq!(got2[3], 0x56);
    }

    #[test]
    fn missing_client_id_is_skipped_quietly() {
        let mut state = ServerState::new();
        let _peer = install(&mut state, 1, 0xFFFF_FFFF);
        let dropped = fanout_event_to_clients(
            &mut state,
            &[ClientId(1), ClientId(99)], // 99 doesn't exist
            |buf, _, _| buf.resize(32, 0),
        );
        assert!(dropped.is_empty());
    }

    #[test]
    fn client_target_id_returns_some_only_for_registered() {
        let mut state = ServerState::new();
        let _peer = install(&mut state, 7, 0);
        assert_eq!(client_target_id(&state, ClientId(7)), Some(ClientId(7)));
        assert_eq!(client_target_id(&state, ClientId(99)), None);
    }

    #[test]
    fn emit_xi2_focus_event_to_state_only_writes_clients_with_matching_mask() {
        let mut state = ServerState::new();
        let window = ROOT_WINDOW;
        // Client 1 selects XI2 FocusIn (evtype 9) on (root, deviceid=3).
        let mut peer1 = install(&mut state, 1, 0);
        state
            .clients
            .get_mut(&1)
            .unwrap()
            .xi2_masks
            .insert((window, 3), 1 << 9);
        // Client 2 selects nothing on root.
        let mut peer2 = install(&mut state, 2, 0);

        let dropped = emit_xi2_focus_event_to_state(&mut state, window, 9, 137, 0, 0, 0, 0);
        assert!(dropped.is_empty());

        let mut buf = [0u8; 64];
        let n = peer1.read(&mut buf).unwrap();
        assert!(n >= 32, "client 1 should receive an XI2 focus event");

        peer2
            .set_nonblocking(true)
            .expect("set_nonblocking on peer2");
        let mut other = [0u8; 64];
        match peer2.read(&mut other) {
            Ok(0) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            other => panic!("client 2 unexpectedly received: {other:?}"),
        }
    }

    #[test]
    fn expose_event_fanout_translates_host_xid() {
        let mut state = ServerState::new();
        let mut peer = install(&mut state, 1, 0x0000_8000); // ExposureMask
        let host_xid = 0xdead_beefu32;
        let xid_map: HostXidMap = std::collections::HashMap::from([(host_xid, ROOT_WINDOW)]);
        let dropped = expose_event_fanout_to_state(
            &mut state,
            &xid_map,
            HostExposeEvent {
                host_xid,
                x: 0,
                y: 0,
                width: 10,
                height: 10,
                count: 0,
            },
        );
        assert!(dropped.is_empty());
        let mut buf = [0u8; 32];
        peer.read_exact(&mut buf).unwrap();
        assert_eq!(buf[0], 12); // X11 Expose event opcode
    }

    #[test]
    fn expose_event_fanout_unknown_host_xid_is_quiet() {
        let mut state = ServerState::new();
        let _peer = install(&mut state, 1, 0xFFFF_FFFF);
        let xid_map: HostXidMap = std::collections::HashMap::new();
        let dropped = expose_event_fanout_to_state(
            &mut state,
            &xid_map,
            HostExposeEvent {
                host_xid: 1234,
                x: 0,
                y: 0,
                width: 10,
                height: 10,
                count: 0,
            },
        );
        assert!(dropped.is_empty());
    }

    #[test]
    fn emit_window_event_to_state_only_writes_subscribers() {
        let mut state = ServerState::new();
        let mut peer1 = install(&mut state, 1, 0x40_0000); // PropertyChange
        let mut peer2 = install(&mut state, 2, 0x00_0001); // KeyPress only
        let dropped =
            emit_window_event_to_state(&mut state, ROOT_WINDOW, 0x40_0000, |buf, _seq, _order| {
                buf.resize(32, 0);
                buf[0] = 0x55;
            });
        assert!(dropped.is_empty());

        let mut buf1 = [0u8; 32];
        peer1.read_exact(&mut buf1).unwrap();
        assert_eq!(buf1[0], 0x55);

        peer2
            .set_nonblocking(true)
            .expect("set_nonblocking on peer2");
        let mut buf2 = [0u8; 32];
        match peer2.read(&mut buf2) {
            Ok(0) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            other => panic!("unsubscribed peer2 unexpectedly received: {other:?}"),
        }
    }
}
