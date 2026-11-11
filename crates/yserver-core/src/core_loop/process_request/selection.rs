use crate::{
    access::{AccessMode, RequestOpcodes, ResourceKind, ResourceTarget},
    backend::Backend,
    core_loop::{
        fanout::{
            client_target_id, fanout_event_to_clients, fanout_raw_event_to_clients,
            selection_owner_target_id, subscribers_by_id,
        },
        process_request::{RequestOutcome, emit_x11_error, write_to_client},
    },
    resources::ROOT_WINDOW,
    server::{SelectionOwner, ServerState},
};
use log::debug;
use std::io;
use yserver_protocol::{
    x11,
    x11::{AtomId, ClientId, RequestHeader, ResourceId, SequenceNumber},
};

pub(super) fn handle_set_selection_owner(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    access_request: RequestOpcodes,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if body.len() < 8 {
        debug!(
            "client {} #{} SetSelectionOwner (short body)",
            client_id.0, sequence.0
        );
        return Ok(RequestOutcome::Handled);
    }
    let window = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
    let selection = AtomId(u32::from_le_bytes([body[4], body[5], body[6], body[7]]));
    let time_val = if body.len() >= 12 {
        u32::from_le_bytes([body[8], body[9], body[10], body[11]])
    } else {
        0u32
    };

    // Xorg `dixSetSelectionOwner` (dix/selection.c:156-211) ordering:
    //   1. Reject future timestamps silently (line 166-167)
    //   2. Validate the window, check access, then validate the atom
    //      (line 169-178) — we accept any window here pre-existing
    //      behaviour; tightening that is a separate audit item
    //   3. Reject stale timestamps silently vs prior lastTimeChanged
    //      (line 192-193)
    //   4. Update ownership + lastTimeChanged (line 204-207)
    //   5. Call the SelectionCallback → XFixesSelectionNotify
    //      with timestamp = currentTime, selection_timestamp = the
    //      *new* lastTimeChanged (xfixes/select.c:88-89)
    if time_val != 0 && time_val > state.timestamp_now() {
        // Future-shifted: silent no-op (Success per Xorg).
        return Ok(RequestOutcome::Handled);
    }
    // BadWindow if `window != None` and the xid doesn't resolve
    // (`dix/selection.c:169-173`). `window == 0` (None) means
    // "release ownership" and is the only valid non-existent value.
    // The major opcode in the error reply is the core
    // SetSelectionOwner opcode (22), not 0 — clients keying on
    // `major_opcode` see a malformed error otherwise.
    if window.0 != 0 && state.resources.window(window).is_none() {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_WINDOW,
            window.0,
            22,
        );
    }
    if window.0 != 0
        && let Err((code, bad_value)) = super::validate_resource_access(
            state,
            client_id,
            ResourceTarget {
                owner: None,
                id: window,
                kind: ResourceKind::Window,
                creation: None,
            },
            AccessMode::SET_ATTR,
            access_request,
        )
    {
        return emit_x11_error(state, client_id, sequence, code, bad_value, 22);
    }
    // BadAtom if the selection atom isn't allocated
    // (`dix/selection.c:175-178`). `state.atoms.exists` is the
    // equivalent of Xorg's `ValidAtom`.
    if !state.atoms.exists(selection) {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_ATOM,
            selection.0,
            22,
        );
    }
    apply_set_selection_owner(state, client_id, sequence, window, selection, time_val);
    Ok(RequestOutcome::Handled)
}

/// SetSelectionOwner after validation: `window` is `None` or a window
/// `client_id` may set attributes on, and `selection` an atom. A time in
/// the future or before the selection's last change is ignored, as the
/// request's.
pub(crate) fn apply_set_selection_owner(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    window: ResourceId,
    selection: AtomId,
    time_val: u32,
) {
    let current_time = state.timestamp_now();
    // `ClientTimeToServerTime`: X11 time=0 (`CurrentTime`) resolves
    // to the server's current time; otherwise the client's value
    // wins, unless it is in the future (step 1 of the handler's order),
    // which is ignored.
    let resolved_time = if time_val == 0 {
        current_time
    } else {
        time_val
    };
    if time_val != 0 && time_val > current_time {
        return;
    }
    if let Some(prior) = state.selections.get(&selection)
        && resolved_time < prior.last_time_changed
    {
        // Stale: silent no-op.
        return;
    }
    // Snapshot prior ownership BEFORE we mutate state.selections so
    // the SelectionClear gate / event payload sees the previous
    // owner: its window and the client that set it. A release (None)
    // keeps the record with its time and `client = NullClient`
    // (`dix/selection.c:204-207`), creating it if the selection was never
    // set (`:196-213`).
    let old = state.selections.get(&selection).copied();
    state.selections.insert(
        selection,
        SelectionOwner {
            window,
            last_time_changed: resolved_time,
            client: (window.0 != 0).then_some(client_id),
        },
    );
    let name = state.atoms.name(selection).map(str::to_owned);
    debug!(
        "client {} #{} SetSelectionOwner {} -> 0x{:x}",
        client_id.0,
        sequence.0,
        name.as_deref().unwrap_or("?"),
        window.0
    );

    // SelectionClear gating per Xorg `dix/selection.c:183-191`:
    //   if (pSel->client && (!pWin || (pSel->client != client)))
    // i.e. fire only when there WAS a prior owner AND either the
    // new owner is None OR the setting client differs. `pSel->client`
    // is the client that set the prior owner, not the owner window's
    // creator, and the SelectionClear goes to it. The event's time field carries the resolved
    // server time (`time.milliseconds` post-`ClientTimeToServerTime`
    // at line 185), not the raw `time_val` — a client sending
    // `CurrentTime`/0 must NOT see 0 in the event payload.
    if let Some(old) = old
        && let Some(old_client) = old.client
        && (window.0 == 0 || old_client != client_id)
    {
        let old_window = old.window;
        // The server's selection window has no connection: its rootless
        // host hears the SelectionClear instead.
        if !crate::rootless::selection::note_selection_clear(state, &old, selection, resolved_time)
            && let Some(old_target) = client_target_id(state, old_client)
        {
            let _dropped = fanout_event_to_clients(state, &[old_target], |buf, seq, order| {
                x11::encode_selection_clear_event(
                    buf,
                    seq,
                    order,
                    resolved_time,
                    old_window,
                    selection,
                );
            });
        }
    }

    // Audit #9 (docs/protocol-audit-2026-05-19.md) — fire
    // XFixesSelectionNotify(SetSelectionOwner) to every client that
    // subscribed to this selection via `SelectSelectionInput` with
    // the matching mask bit. Xorg fires this from
    // `XFixesSelectionCallback` (`xfixes/select.c:158-210`) which
    // is registered as a `SelectionCallback` against the core
    // selection-mgmt code. Without it, clipboard managers wedge
    // forever waiting for the "selection changed" signal.
    //
    // Wire payload per Xorg `xfixes/select.c:88-89`:
    //   timestamp           = currentTime (NOT the request's time arg)
    //   selection_timestamp = pSel->lastTimeChanged (= resolved_time
    //                         we just stored)
    fanout_xfixes_selection_notify(
        state,
        selection,
        yserver_protocol::x11::xfixes::SELECTION_NOTIFY_SET_OWNER,
        window.0,
        current_time,
        resolved_time,
    );
}

/// Xorg `DeleteWindowFromAnySelections` (`dix/selection.c:113-125`): every
/// selection owned by one of `owned_windows` fires
/// `XFixesSelectionNotify(subtype)` and is cleared. `destroy_window_subtree`
/// passes `SelectionWindowDestroy`.
pub(super) fn drop_selections_owned_by_windows(
    state: &mut ServerState,
    owned_windows: &[ResourceId],
    subtype: u8,
) {
    drop_selections_where(state, subtype, |owner| {
        owned_windows.contains(&owner.window)
    });
}

/// Xorg `DeleteClientFromAnySelections` (`dix/selection.c:128-139`), run by
/// `CloseDownClient` (`dix/dispatch.c:3495`) before the client's resources
/// go: every selection `cid` set fires
/// `XFixesSelectionNotify(SelectionClientClose)` and is cleared, whoever owns
/// its window. A selection another client set on one of `cid`'s windows is
/// left for the window's destruction (SelectionWindowDestroy).
pub(crate) fn fanout_xfixes_selection_client_close_for_client(
    state: &mut ServerState,
    cid: ClientId,
) {
    drop_selections_where(
        state,
        yserver_protocol::x11::xfixes::SELECTION_NOTIFY_CLIENT_CLOSE,
        |owner| owner.client == Some(cid),
    );
}

fn drop_selections_where(
    state: &mut ServerState,
    subtype: u8,
    matches: impl Fn(&SelectionOwner) -> bool,
) {
    // Capture (selection, prior_last_time_changed) BEFORE clearing so
    // the notify can carry the prior `selection_timestamp` — Xorg
    // fires the callback BEFORE mutating `pSel->window`/`pSel->client`,
    // and the callback at `xfixes/select.c:89` reads
    // `selection->lastTimeChanged`.
    let to_drop: Vec<(AtomId, u32)> = state
        .selections
        .iter()
        .filter(|(_, owner)| matches(owner))
        .map(|(sel, owner)| (*sel, owner.last_time_changed))
        .collect();
    if to_drop.is_empty() {
        return;
    }
    // `timestamp_now()` matches Xorg's `UpdateCurrentTimeIf` shape
    // (`dix/dispatch.c:226-236`): bare monotonic-ms snapshot, no
    // floor. X11 timestamp 0 only has dedicated semantics on the
    // INPUT side (request `time` arg = "use CurrentTime"); event
    // payloads emitted in the first ms after server start carry the
    // actual `0` per Xorg, so the clamp this used to apply was a
    // protocol mismatch.
    let now = state.timestamp_now();
    for (sel, prior_last_time_changed) in to_drop {
        // owner=0 (None) — see Xorg `xfixes/select.c:85-86`:
        // destroy/close subtypes always carry owner=None.
        fanout_xfixes_selection_notify(state, sel, subtype, 0, now, prior_last_time_changed);
        // Xorg keeps the record and its `lastTimeChanged`
        // (`dix/selection.c:120-122`, `:134-136`).
        if let Some(owner) = state.selections.get_mut(&sel) {
            owner.window = ResourceId(0);
            owner.client = None;
        }
    }
}

/// Audit #9 fanout helper — emits an `XFixesSelectionNotify` to every
/// `SelectSelectionInput` subscription on this selection whose mask
/// includes the matching subtype bit. Each subscription gets its own
/// event (the `window` field carries the *subscriber's* window, not
/// the owner's, per the X11 protocol).
///
/// `owner_window` is the new owner (or 0 = `None`). `timestamp` is
/// currentTime and `selection_timestamp` the selection's
/// `lastTimeChanged` (`xfixes/select.c:78-89`).
fn fanout_xfixes_selection_notify(
    state: &mut ServerState,
    selection: AtomId,
    subtype: u8,
    owner_window: u32,
    timestamp: u32,
    selection_timestamp: u32,
) {
    use yserver_protocol::x11::xfixes as x11xfixes;

    // Xorg's selection callbacks reach every listener of the change; the
    // rootless host is one, for the selections it watches.
    crate::rootless::selection::note_owner(state, selection, owner_window, selection_timestamp);
    let mask_bit = 1u32 << subtype;
    // Snapshot matching subscriptions before mutating `state.clients`
    // inside the fanout writer (which needs `&mut state`).
    let matched: Vec<(ClientId, u32)> = state
        .xfixes_selection_masks
        .iter()
        .filter_map(|((cid, win, sel), mask)| {
            if *sel == selection && mask & mask_bit != 0 {
                Some((ClientId(*cid), win.0))
            } else {
                None
            }
        })
        .collect();
    for (cid, subscriber_window) in matched {
        let _dropped = fanout_event_to_clients(state, &[cid], |buf, seq, order| {
            x11xfixes::encode_selection_notify_event(
                buf,
                order,
                crate::nested::XFIXES_FIRST_EVENT,
                subtype,
                seq,
                subscriber_window,
                owner_window,
                selection.0,
                timestamp,
                selection_timestamp,
            );
        });
    }
}

pub(super) fn handle_convert_selection(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    access_request: RequestOpcodes,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if body.len() < 20 {
        return Ok(RequestOutcome::Handled);
    }
    let requestor = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
    let selection = AtomId(u32::from_le_bytes([body[4], body[5], body[6], body[7]]));
    let target_atom = AtomId(u32::from_le_bytes([body[8], body[9], body[10], body[11]]));
    let property = AtomId(u32::from_le_bytes([body[12], body[13], body[14], body[15]]));
    let time_val = u32::from_le_bytes([body[16], body[17], body[18], body[19]]);

    // BadWindow for the requestor, then BadAtom carrying the property as its
    // value whichever atom is bad (`dix/selection.c:271-280`).
    if state.resources.window(requestor).is_none() {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_WINDOW,
            requestor.0,
            24,
        );
    }
    if let Err((code, bad_value)) = super::validate_resource_access(
        state,
        client_id,
        ResourceTarget {
            owner: None,
            id: requestor,
            kind: ResourceKind::Window,
            creation: None,
        },
        AccessMode::SET_ATTR,
        access_request,
    ) {
        return emit_x11_error(state, client_id, sequence, code, bad_value, 24);
    }
    if !state.atoms.exists(selection)
        || !state.atoms.exists(target_atom)
        || (property.0 != 0 && !state.atoms.exists(property))
    {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_ATOM,
            property.0,
            24,
        );
    }
    apply_convert_selection(
        state,
        client_id,
        sequence,
        requestor,
        [selection, target_atom, property],
        time_val,
    );
    Ok(RequestOutcome::Handled)
}

/// ConvertSelection after validation: `requestor` is a window `client_id`
/// may set attributes on, and `selection`, `target_atom` and `property`
/// (unless `None`) are atoms.
pub(crate) fn apply_convert_selection(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    requestor: ResourceId,
    [selection, target_atom, property]: [AtomId; 3],
    time_val: u32,
) {
    // A selection the server's own window owns is answered by the rootless
    // host that made it, which has no connection for the SelectionRequest.
    if let Some(owner) = state
        .selections
        .get(&selection)
        .copied()
        .filter(|owner| owner.window.0 != 0)
        && crate::rootless::selection::note_request(
            state,
            &owner,
            crate::rootless::SelectionEvent::Request {
                time: time_val,
                requestor,
                selection,
                target: target_atom,
                property,
            },
        )
    {
        debug!(
            "client {} #{} ConvertSelection -> the server's window 0x{:x}",
            client_id.0, sequence.0, owner.window.0
        );
        return;
    }

    if let Some((owner_window, owner_id)) = selection_owner_target_id(state, selection) {
        let _dropped = fanout_event_to_clients(state, &[owner_id], |buf, seq, order| {
            x11::encode_selection_request_event(
                buf,
                seq,
                order,
                time_val,
                owner_window,
                requestor,
                selection,
                target_atom,
                property,
            );
        });
        debug!(
            "client {} #{} ConvertSelection -> owner 0x{:x}",
            client_id.0, sequence.0, owner_window.0
        );
    } else if crate::rootless::selection::note_refusal(
        state,
        requestor,
        time_val,
        selection,
        target_atom,
    ) {
        // No owner, and the server's window asked: its host hears it.
    } else {
        // No owner, or its setter is gone (`dix/selection.c:297-301`):
        // SelectionNotify(None) to the client that issued ConvertSelection
        // (`dix/selection.c:304-311`), whoever created the requestor window.
        if let Some(rt) = client_target_id(state, client_id) {
            let mut template = [0u8; 32];
            template[0] = 31; // SelectionNotify
            template[4..8].copy_from_slice(&time_val.to_le_bytes());
            template[8..12].copy_from_slice(&requestor.0.to_le_bytes());
            template[12..16].copy_from_slice(&selection.0.to_le_bytes());
            template[16..20].copy_from_slice(&target_atom.0.to_le_bytes());
            // property = 0 (None): conversion failed.
            let _dropped = fanout_raw_event_to_clients(
                state,
                &[rt],
                &template,
                yserver_protocol::x11::ClientByteOrder::LittleEndian,
            );
        }
        debug!(
            "client {} #{} ConvertSelection: no owner, sent SelectionNotify(None)",
            client_id.0, sequence.0
        );
    }
}

pub(super) fn handle_send_event(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    use yserver_protocol::x11::ClientByteOrder;
    // The 32-byte event template inside SendEvent is in the *sender's*
    // byte order. Note: the request body's typed prefix (destination +
    // event_mask) was already swapped to LE by request_swap; only the
    // template itself stays in the sender's byte order so we can
    // re-encode per recipient.
    let sender_byte_order = state
        .clients
        .get(&client_id.0)
        .map_or(ClientByteOrder::LittleEndian, |c| c.byte_order);
    let Some(req) = x11::send_event_request(header.data, body) else {
        debug!(
            "client {} #{} SendEvent (parse failed)",
            client_id.0, sequence.0
        );
        return Ok(RequestOutcome::Handled);
    };
    // Xorg `ProcSendEvent` (dix/events.c:5563-5590) validates the template
    // and mask before resolving the destination.
    let event_type = req.event[0] & 0x7f;
    if !matches!(event_type, 2..=34 | 64..) {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            u32::from(event_type),
            25,
        );
    }
    if event_type == 33 && !matches!(req.event[1], 8 | 16 | 32) {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            u32::from(req.event[1]),
            25,
        );
    }
    if req.event_mask & !ALL_EVENT_MASKS != 0 {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            req.event_mask,
            25,
        );
    }
    // A SelectionNotify for the server's own selection window, the answer
    // to the rootless host's ConvertSelection: the host takes it, as no
    // client can.
    if event_type == 31 {
        let word = |at: usize| {
            let bytes = [
                req.event[at],
                req.event[at + 1],
                req.event[at + 2],
                req.event[at + 3],
            ];
            match sender_byte_order {
                ClientByteOrder::BigEndian => u32::from_be_bytes(bytes),
                ClientByteOrder::LittleEndian => u32::from_le_bytes(bytes),
            }
        };
        if crate::rootless::selection::note_selection_notify(
            state,
            req.destination,
            [word(4), word(12), word(16), word(20)],
        ) {
            debug!(
                "client {} #{} SendEvent SelectionNotify to the server's window 0x{:x}",
                client_id.0, sequence.0, req.destination.0
            );
            return Ok(RequestOutcome::Handled);
        }
    }
    let targets = match send_event_recipients(state, req.destination, req.event_mask, header.data) {
        Ok(targets) => targets,
        Err((code, value)) => return emit_x11_error(state, client_id, sequence, code, value, 25),
    };
    // Set the sent-event bit (bit 7 of first byte).
    let mut event_copy = *req.event;
    event_copy[0] |= 0x80;
    let _dropped = fanout_raw_event_to_clients(state, &targets, &event_copy, sender_byte_order);
    crate::rootless::note_root_client_message(
        state,
        backend,
        req.destination,
        req.event,
        sender_byte_order,
    );
    let core_type_logged = req.event[0] & 0x7f;
    // For synthetic ConfigureNotify (type=22), decode and log x/y/w/h
    // so we can see what the WM tells clients about their root
    // position. The wire body (after the 4-byte type/seq prefix) is:
    //   event_window(4) + window(4) + above_sibling(4) +
    //   x(2) + y(2) + width(2) + height(2) + border(2) +
    //   override_redirect(1) + pad(1)
    if core_type_logged == 22 {
        let e = &req.event;
        let event_w = u32::from_le_bytes([e[4], e[5], e[6], e[7]]);
        let window_w = u32::from_le_bytes([e[8], e[9], e[10], e[11]]);
        let above = u32::from_le_bytes([e[12], e[13], e[14], e[15]]);
        let x = i16::from_le_bytes([e[16], e[17]]);
        let y = i16::from_le_bytes([e[18], e[19]]);
        let w = u16::from_le_bytes([e[20], e[21]]);
        let h = u16::from_le_bytes([e[22], e[23]]);
        let bw = u16::from_le_bytes([e[24], e[25]]);
        let or = e[26];
        debug!(
            "client {} #{} SendEvent type=22 (ConfigureNotify) dest=0x{:x} \
             ev_win=0x{:x} win=0x{:x} above=0x{:x} pos=({},{}) size=({}x{}) \
             border={} override={} targets={:?}",
            client_id.0,
            sequence.0,
            req.destination.0,
            event_w,
            window_w,
            above,
            x,
            y,
            w,
            h,
            bw,
            or,
            targets.iter().map(|c| c.0).collect::<Vec<_>>(),
        );
    } else {
        debug!(
            "client {} #{} SendEvent type={} dest=0x{:x} event_mask=0x{:x} propagate={} targets={:?}",
            client_id.0,
            sequence.0,
            core_type_logged,
            req.destination.0,
            req.event_mask,
            req.propagate,
            targets.iter().map(|c| c.0).collect::<Vec<_>>(),
        );
    }
    Ok(RequestOutcome::Handled)
}

/// Xorg `AllEventMasks` (`include/inputstr.h`): the 25 core event-mask bits.
const ALL_EVENT_MASKS: u32 = 0x01FF_FFFF;

/// The clients a `SendEvent` reaches — Xorg `ProcSendEvent`
/// (`dix/events.c:5592-5640`) with `DeliverEventsToWindow`
/// (`dix/events.c:2364`). A core event template is matched against CORE
/// event masks only: `GetClientsForDelivery` (`dix/events.c:2241`) takes the
/// window's core `OtherClients` for any core type, so an XI2 selection never
/// filters a synthetic core event, and `SendEvent` never produces XI2 events.
///
/// `Err((code, value))` is the X error to raise.
pub(crate) fn send_event_recipients(
    state: &ServerState,
    destination: ResourceId,
    event_mask: u32,
    propagate: u8,
) -> Result<Vec<ClientId>, (u8, u32)> {
    let sprite = state
        .root_pointer_target_at(state.pointer_root.0, state.pointer_root.1)
        .map_or(ROOT_WINDOW, |(w, _, _)| w);
    let mut effective_focus = None;
    let mut window = match destination.0 {
        0 => sprite,
        1 => {
            let focus = match state.core_focus.raw {
                0 => return Ok(Vec::new()),
                1 => ROOT_WINDOW,
                w => ResourceId(w),
            };
            // `IsParent(inputFocus, pSprite->win)`: the pointer is in a
            // strict inferior of the focus window.
            let window = if is_strict_ancestor(state, focus, sprite) {
                sprite
            } else {
                focus
            };
            effective_focus = Some(focus);
            window
        }
        _ => {
            if state.resources.window(destination).is_none() {
                return Err((x11::error::BAD_WINDOW, destination.0));
            }
            destination
        }
    };
    if propagate > 1 {
        return Err((x11::error::BAD_VALUE, u32::from(propagate)));
    }
    let mut mask = event_mask;
    loop {
        let targets: Vec<ClientId> = if mask == 0 {
            // `CantBeFiltered`: only the window's creator, never the server.
            state
                .resources
                .window_owner(window)
                .filter(|_| window != ROOT_WINDOW)
                .and_then(|owner| client_target_id(state, owner))
                .into_iter()
                .collect()
        } else {
            subscribers_by_id(state, window, mask)
        };
        if !targets.is_empty() || propagate == 0 || effective_focus == Some(window) {
            return Ok(targets);
        }
        let Some(win) = state.resources.window(window) else {
            return Ok(Vec::new());
        };
        mask &= !u32::from(win.do_not_propagate_mask);
        if mask == 0 || win.parent == window {
            return Ok(Vec::new());
        }
        window = win.parent;
    }
}

/// Xorg `IsParent(a, b)`: `a` is a strict ancestor of `b`.
fn is_strict_ancestor(state: &ServerState, ancestor: ResourceId, window: ResourceId) -> bool {
    let mut current = window;
    while let Some(parent) = state.resources.parent_of(current) {
        if parent == current {
            return false;
        }
        if parent == ancestor {
            return true;
        }
        current = parent;
    }
    false
}

pub(super) fn handle_get_selection_owner(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    let owner = if body.len() >= 4 {
        let selection = AtomId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
        state
            .selections
            .get(&selection)
            .map_or(ResourceId(0), |owner| owner.window)
    } else {
        ResourceId(0)
    };
    debug!(
        "client {} #{} GetSelectionOwner -> 0x{:x}",
        client_id.0, sequence.0, owner.0
    );
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let byte_order = client.byte_order;
    let mut buf: Vec<u8> = Vec::with_capacity(32);
    x11::write_get_selection_owner_reply(&mut buf, byte_order, sequence, owner)?;
    Ok(write_to_client(client, client_id, &buf))
}
