//! Core-side event fanout helpers — the state-borrowing replacements
//! for `server::fanout_event` / `server::fanout_raw_event` /
//! `server::pointer_event_fanout`.
//!
//! Each helper takes `&mut ServerState` so it can update each
//! client's `last_sequence`, encode against the client's
//! `byte_order`, and push bytes through `client_io::write_or_buffer`
//! — the same path opcode dispatch will use after the D3 lift.
//! Clients whose write failed are returned as a `Vec<ClientId>` for
//! information only: `client_io` has already flagged them, and the core
//! loop disconnects them (`run::disconnect_failed_writers`).
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

/// XI2 device ids whose selection ABSORBS a core pointer event: the
/// master pointer plus the `XIAllMasterDevices(1)` / `XIAllDevices(0)`
/// wildcards.
///
/// The SLAVE pointer id is deliberately ABSENT. Core events are generated
/// from the master device and a slave-device XI2 selection is matched in
/// its own delivery pass, so it does not suppress the core form: a client
/// selecting core AND slave-XI2 on one window legitimately receives BOTH.
/// Hardware-measured, not inferred — see
/// `implicit_grab_core_release_survives_dual_core_and_slave_xi2_selection_e27`,
/// whose Xorg baseline delivers core 3/3 and XI2 3/3 on the same clicks.
/// Including the slave id here silently dropped E27's core press.
const XI2_ABSORBING_POINTER_DEVICES: [u16; 3] = [2, 1, 0];

/// Does any client hold an XI2 selection for `evtype` on this window?
///
/// Xorg's `DeliverDeviceEvents` (`dix/events.c:2895-2916`) tries XI2, then
/// XI1, then CORE **per window**, returning as soon as any flavour
/// delivers — so an XI2 selection ABSORBS the event and the core
/// propagation walk stops there, whoever selected core further up.
fn xi2_pointer_selected_on(state: &ServerState, window: ResourceId, evtype: u16) -> bool {
    let bit = 1u64 << evtype;
    state.clients.values().any(|client| {
        XI2_ABSORBING_POINTER_DEVICES.iter().any(|device| {
            client
                .xi2_masks
                .get(&(window, *device))
                .is_some_and(|mask| mask & bit != 0)
        })
    })
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
    xi2_evtype: Option<u16>,
) -> Option<(ResourceId, i16, i16, Vec<ClientId>, ResourceId)> {
    let mut current = start;
    let mut x = start_x;
    let mut y = start_y;
    let mut child: Option<ResourceId> = None;
    for _ in 0..256 {
        // XI2 FIRST, per Xorg's per-window flavour order. A window whose
        // XI2 selection matches consumes the event: no core delivery here,
        // and crucially no walk past it. Without this a GTK client — XI2,
        // no core mask — let the press climb to the reparenting WM's
        // frame, handing the WM a press Xorg never sends. That armed
        // OpenBox's drag state, so a later bare MotionNotify started a
        // Move/Resize with no button held (#141).
        if let Some(evtype) = xi2_evtype
            && xi2_pointer_selected_on(state, current, evtype)
        {
            return None;
        }
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
/// the matching XI2 evtype on the master keyboard or either wildcard
/// selector. Focus transitions carry master keyboard deviceid 3, so
/// the XTEST keyboard's exact-device selector is not eligible. The
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
            let mask = [
                crate::xinput::DEVICEID_MASTER_KEYBOARD,
                1, // XIAllMasterDevices applies to this master device.
                0, // XIAllDevices applies to every device.
            ]
            .into_iter()
            .filter_map(|device_id| client.xi2_masks.get(&(window, device_id)))
            .copied()
            .fold(0, |combined, selected| combined | selected);
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

/// Emit an XI2 `XI_DeviceChanged` for the bootstrapped virtual XTEST
/// pointer to clients that selected that device or `XIAllDevices`.
/// Physical registry facets are never represented by device 4 here.
/// The carried class set keeps the query-compatible button + 4 valuator
/// + 2 scroll shape.
///
/// `XIAllMasterDevices` does not select this slave event. If no client
/// selected, this is a no-op. Returns clients whose outbound
/// buffer overflowed (for `ClientDisconnected` reporting).
///
/// `xi2_major_opcode` is the XI extension's runtime-assigned major
/// opcode (137 in the current build).
pub fn emit_xi2_device_changed_slave_pointer(
    state: &mut ServerState,
    xi2_major_opcode: u8,
) -> Vec<ClientId> {
    const XTEST_POINTER: u16 = crate::xinput::DEVICEID_XTEST_POINTER;

    // Clients select on (window, deviceid). DeviceChanged is a
    // hierarchy-wide event clients select on the ROOT window (the same
    // window `process_request`'s XISelectEvents bootstrap requires before
    // it sends the initial DeviceChanged). Match the root window only —
    // matching any window would spuriously deliver to a client that
    // selected DeviceChanged on some unrelated child. The XTEST device
    // matches its exact selector and XIAllDevices(0); it is not a master,
    // so XIAllMasterDevices(1) does not match.
    let targets: Vec<ClientId> = state
        .clients
        .iter()
        .filter_map(|(id, client)| {
            let selected = client.xi2_masks.iter().any(|(&(window, dev), &mask)| {
                window == ROOT_WINDOW
                    && matches!(dev, XTEST_POINTER | 0)
                    && (mask & u64::from(XI2_DEVICE_CHANGED_MASK)) != 0
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
            XTEST_POINTER,
            time,
            num_classes,
            XTEST_POINTER, // sourceid = the XTEST device itself
            XI_REASON_DEVICE_CHANGE,
            &classes,
        );
    })
}

/// Build the XI2 device-class block for the virtual XTEST pointer.
///
/// The query encoder owns the shared button/valuator/scroll class layout;
/// both XIQueryDevice and DeviceChanged use it so their class blocks remain
/// byte-identical for little-endian clients.
pub(crate) fn build_slave_pointer_class_block(state: &mut ServerState) -> (Vec<u8>, u16) {
    build_pointer_class_block_for_device(state, crate::xinput::DEVICEID_XTEST_POINTER)
}

pub(crate) fn build_pointer_class_block_for_device(
    state: &mut ServerState,
    device_id: u16,
) -> (Vec<u8>, u16) {
    let class_data = pointer_class_data_for_device(state, device_id);
    crate::xinput::query::build_pointer_classes(
        ClientByteOrder::LittleEndian,
        device_id,
        class_data,
    )
}

pub(crate) fn pointer_class_data_for_device(
    state: &mut ServerState,
    device_id: u16,
) -> crate::xinput::query::XiQueryClassData {
    crate::xinput::query::XiQueryClassData {
        button_labels: [
            state.atoms.intern("Button Left", false),
            state.atoms.intern("Button Middle", false),
            state.atoms.intern("Button Right", false),
            state.atoms.intern("Button Wheel Up", false),
            state.atoms.intern("Button Wheel Down", false),
            state.atoms.intern("Button Horiz Wheel Left", false),
            state.atoms.intern("Button Horiz Wheel Right", false),
        ],
        button_state: if device_id == crate::xinput::DEVICEID_MASTER_POINTER {
            state.buttons_down
        } else {
            state
                .xi_devices
                .device(device_id)
                .map_or(0, |device| device.buttons_down)
        },
        axis_labels: [
            state.atoms.intern("Rel X", false),
            state.atoms.intern("Rel Y", false),
            state.atoms.intern("Rel Vert Scroll", false),
            state.atoms.intern("Rel Horiz Scroll", false),
        ],
        pointer: (
            i32::from(state.randr.screen_width) / 2,
            i32::from(state.randr.screen_height) / 2,
        ),
        scroll: if device_id == crate::xinput::DEVICEID_MASTER_POINTER {
            state.scroll_axis_value
        } else {
            state
                .xi_devices
                .device(device_id)
                .map_or(state.scroll_axis_value, |device| device.scroll_axis_values)
        },
    }
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

    fn make_client_with_transport(
        writer: crate::transport::Transport,
        mask_for_root: u32,
    ) -> ClientState {
        ClientState {
            writer: Arc::new(Mutex::new(writer)),
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
            write_failed: false,
            output_held: false,
            output_gate: None,
            focused_window: ROOT_WINDOW,
            reader_control: None,
            is_local: true,
            fd_passing: true,
        }
    }

    fn make_client(writer: UnixStream, mask_for_root: u32) -> ClientState {
        make_client_with_transport(crate::transport::Transport::Unix(writer), mask_for_root)
    }

    fn install(state: &mut ServerState, id: u32, mask: u32) -> UnixStream {
        let (a, b) = UnixStream::pair().unwrap();
        let client = make_client(a, mask);
        state.clients.insert(id, client);
        b
    }

    fn install_capture(state: &mut ServerState, id: u32) -> crate::transport::CapturedPeer {
        let (writer, peer) = crate::transport::Transport::capture_pair();
        state
            .clients
            .insert(id, make_client_with_transport(writer, 0));
        peer
    }

    fn read_all_capture(peer: &mut crate::transport::CapturedPeer) -> Vec<u8> {
        peer.set_nonblocking(true).expect("set capture nonblocking");
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 128];
        loop {
            match peer.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => bytes.extend_from_slice(&buffer[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("capture read failed: {error}"),
            }
        }
        bytes
    }

    /// Issue #141 — a reparenting WM must not be handed a core press that
    /// an XI2 client absorbed. Xorg's `DeliverDeviceEvents`
    /// (`dix/events.c:2895-2916`) tries XI2, then XI1, then core PER
    /// WINDOW and returns as soon as any flavour delivers, so a GTK-style
    /// child selecting XI2 and no core mask consumes the event and the
    /// walk stops — the frame above it sees nothing.
    ///
    /// Oracle is measured, not read off the source:
    /// `tools/replay-propagation-probe.c` run under
    /// `tools/vng-scenarios/replay-propagation.sh` against Xorg 21.1 and
    /// yserver in the same harness. Xorg gave the WM 1 core press (its own
    /// grab activation) and no propagated second one; yserver gave 2, the
    /// second on the PARENT. That second press armed OpenBox's drag state,
    /// so a later bare MotionNotify started a Move/Resize with no button
    /// held — windows following the pointer once every minute or two.
    ///
    /// The slave half is the E27 carve-out and is asserted here too: a
    /// SLAVE-device XI2 selection must NOT absorb, because core events come
    /// from the master and the slave form is a separate delivery pass. See
    /// `implicit_grab_core_release_survives_dual_core_and_slave_xi2_selection_e27`,
    /// whose Xorg baseline delivers both forms 3/3.
    #[test]
    fn xi2_master_selection_absorbs_core_propagation_but_slave_does_not() {
        use yserver_protocol::x11::CreateWindowRequest;
        const WM: u32 = 1;
        const APP: u32 = 2;
        const XI_BUTTON_PRESS: u16 = 4;
        const FRAME: ResourceId = ResourceId(0x0010_0001);
        const CHILD: ResourceId = ResourceId(0x0020_0001);

        let build = |xi2_device: u16| {
            let mut state = ServerState::new();
            let _wm_peer = install(&mut state, WM, 0);
            let _app_peer = install(&mut state, APP, 0);
            for (window, parent) in [(FRAME, ROOT_WINDOW), (CHILD, FRAME)] {
                state.resources.create_window(
                    ClientId(if window == FRAME { WM } else { APP }),
                    CreateWindowRequest {
                        depth: 24,
                        window,
                        parent,
                        x: 0,
                        y: 0,
                        width: 100,
                        height: 100,
                        border_width: 0,
                        class: 1,
                        visual: crate::resources::ROOT_VISUAL,
                        ..Default::default()
                    },
                );
                let _ = state.resources.map_window(window);
            }
            // The frame selects core ButtonPress, as a reparenting WM does.
            state
                .clients
                .get_mut(&WM)
                .unwrap()
                .event_masks
                .insert(FRAME, 0x0000_0004);
            // The child selects XI2 ButtonPress and NO core mask.
            state
                .clients
                .get_mut(&APP)
                .unwrap()
                .xi2_masks
                .insert((CHILD, xi2_device), 1 << XI_BUTTON_PRESS);
            state
        };

        // XIAllMasterDevices: absorbed, nothing propagates to the frame.
        let state = build(1);
        assert!(
            pointer_propagation_target_by_id(
                &state,
                CHILD,
                5,
                5,
                0x0000_0004,
                Some(XI_BUTTON_PRESS)
            )
            .is_none(),
            "an XI2 master-device selection must absorb the core press, \
             leaving the WM's frame nothing to receive",
        );

        // Slave device: NOT absorbed — core still propagates to the frame.
        let state = build(4);
        let (win, _, _, subs, _) = pointer_propagation_target_by_id(
            &state,
            CHILD,
            5,
            5,
            0x0000_0004,
            Some(XI_BUTTON_PRESS),
        )
        .expect("a slave-device XI2 selection must not absorb the core press");
        assert_eq!(win, FRAME);
        assert_eq!(subs, vec![ClientId(WM)]);

        // And with no XI2 evtype in play (crossing events) the walk is
        // unchanged: Enter/Leave are not routed by the flavour-ordered loop.
        let state = build(1);
        let (win, _, _, _, _) =
            pointer_propagation_target_by_id(&state, CHILD, 5, 5, 0x0000_0004, None)
                .expect("without an absorbing evtype the walk still finds the frame");
        assert_eq!(win, FRAME);
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
    fn xi2_focus_event_for_master_does_not_target_xtest_keyboard() {
        let mut state = ServerState::new();
        let window = ROOT_WINDOW;
        let mut xtest_peer = install_capture(&mut state, 1);
        let mut master_peer = install_capture(&mut state, 2);
        let mut all_masters_peer = install_capture(&mut state, 3);
        let mut all_devices_peer = install_capture(&mut state, 4);
        for (client_id, device_id) in [
            (1, crate::xinput::DEVICEID_XTEST_KEYBOARD),
            (2, crate::xinput::DEVICEID_MASTER_KEYBOARD),
            (3, 1),
            (4, 0),
        ] {
            state
                .clients
                .get_mut(&client_id)
                .expect("registered focus client")
                .xi2_masks
                .insert((window, device_id), 1 << 9);
        }
        state
            .clients
            .get_mut(&2)
            .expect("registered master focus client")
            .xi2_masks
            .insert((window, crate::xinput::DEVICEID_MASTER_KEYBOARD), 1 << 2);
        state
            .clients
            .get_mut(&2)
            .expect("registered master focus client")
            .xi2_masks
            .insert((window, 1), 1 << 9);

        assert!(emit_xi2_focus_event_to_state(&mut state, window, 9, 137, 0, 0, 0, 0).is_empty());

        assert!(
            read_all_capture(&mut xtest_peer).is_empty(),
            "the focus transition is for master keyboard 3, not XTEST keyboard 5"
        );
        for (peer, label) in [
            (&mut master_peer, "master keyboard"),
            (&mut all_masters_peer, "XIAllMasterDevices"),
            (&mut all_devices_peer, "XIAllDevices"),
        ] {
            let event = read_all_capture(peer);
            assert_eq!(event.len(), 76, "{label} receives one focus event");
            assert_eq!(u16::from_le_bytes(event[8..10].try_into().unwrap()), 9);
            assert_eq!(
                u16::from_le_bytes(event[10..12].try_into().unwrap()),
                crate::xinput::DEVICEID_MASTER_KEYBOARD,
                "focus event deviceid is the master keyboard"
            );
            assert_eq!(
                u16::from_le_bytes(event[16..18].try_into().unwrap()),
                crate::xinput::DEVICEID_MASTER_KEYBOARD,
                "focus event sourceid is the master keyboard"
            );
        }
        assert_eq!(state.clients.len(), 4);
        assert_eq!(
            state.clients[&1].xi2_masks,
            HashMap::from([((window, crate::xinput::DEVICEID_XTEST_KEYBOARD), 1 << 9)])
        );
        assert_eq!(
            state.clients[&2].xi2_masks,
            HashMap::from([
                ((window, crate::xinput::DEVICEID_MASTER_KEYBOARD), 1 << 2),
                ((window, 1), 1 << 9),
            ])
        );
        assert_eq!(
            state.clients[&3].xi2_masks,
            HashMap::from([((window, 1), 1 << 9)])
        );
        assert_eq!(
            state.clients[&4].xi2_masks,
            HashMap::from([((window, 0), 1 << 9)])
        );
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

    /// Property / structure notify family: a subscriber over
    /// `OUTBOUND_CAP` is reported and flagged for the core loop to
    /// disconnect, gets nothing more, and the other subscriber still gets
    /// every event; the raw (SendEvent / server-built) fanout too.
    #[test]
    fn overflowing_subscriber_is_flagged_and_the_others_still_receive() {
        const PROPERTY_CHANGE: u32 = 1 << 22;
        let mut state = ServerState::new();
        let _slow = install(&mut state, 1, PROPERTY_CHANGE);
        let mut fast = install(&mut state, 2, PROPERTY_CHANGE);
        fast.set_nonblocking(true).unwrap();
        client_io::saturate_for_test(state.clients.get_mut(&1).unwrap());

        let encode = |buf: &mut Vec<u8>, _: SequenceNumber, _: ClientByteOrder| {
            buf.extend_from_slice(&[28u8; 32]);
        };
        let dropped = emit_window_event_to_state(&mut state, ROOT_WINDOW, PROPERTY_CHANGE, encode);
        assert_eq!(dropped, [ClientId(1)]);
        assert_eq!(client_io::failed_writers(&state.clients), [ClientId(1)]);
        let _ = emit_window_event_to_state(&mut state, ROOT_WINDOW, PROPERTY_CHANGE, encode);
        let _ = fanout_raw_event_to_clients(
            &mut state,
            &[ClientId(1), ClientId(2)],
            &[28u8; 32],
            ClientByteOrder::LittleEndian,
        );
        assert_eq!(state.clients[&1].outbound.len(), client_io::OUTBOUND_CAP);
        let mut got = [0u8; 128];
        assert_eq!(fast.read(&mut got).unwrap(), 96);
        assert_eq!(client_io::failed_writers(&state.clients), [ClientId(1)]);
    }
}
