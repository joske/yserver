//! A host's selection bridge: a window of the server's own that owns X
//! selections on the host's behalf and asks X clients for theirs, as a
//! Wayland compositor's X window manager does with a window of its own
//! (wlroots' `xwayland/selection`).
//!
//! A server window has no connection, so what the selection protocol would
//! send it -- the `SelectionRequest` a client's `ConvertSelection` sends a
//! selection's owner, the `SelectionNotify` an owner sends the server
//! window that asked, and the `PropertyNotify` of a property on it -- is
//! queued in `Rootless` for the host to take ([`take_selection_events`]).
//! So is the `PropertyNotify` of a client's window the host watches
//! ([`watch_properties`]), the requestor of a transfer it answers. With
//! both the host can run ICCCM's INCR transfers either way; the protocol
//! itself is the host's.
//!
//! The server's own actions use the requests' apply halves, past their
//! parsing and their checks: every client hears what it would from another
//! client's request. The server passes every access check
//! (`ServerState::check_resource` allows `SERVER_OWNER`), so what is left to
//! check is what the request would refuse as `BadWindow` or `BadAtom`.

use std::collections::HashSet;

use yserver_protocol::x11::{AtomId, ClientByteOrder, ResourceId, SequenceNumber};

use crate::{
    backend::Backend,
    resources::{ROOT_WINDOW, ROOTLESS_SELECTION_WINDOW, SERVER_OWNER},
    server::ServerState,
};

/// A selection event for the server's selection window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionEvent {
    /// A client's `ConvertSelection` of a selection the server's window
    /// owns: the `SelectionRequest` the owner would be sent.
    Request {
        /// The request's time, or 0 for `CurrentTime`.
        time: u32,
        /// The client's window the answer goes to.
        requestor: ResourceId,
        selection: AtomId,
        /// The type asked for.
        target: AtomId,
        /// Where on `requestor` to put it; `None` from an obsolete client,
        /// which means `target`.
        property: AtomId,
    },
    /// The `SelectionNotify` a selection's owner sent the server's window
    /// that asked for it, or the server sent itself when nothing owned it.
    Notify {
        selection: AtomId,
        /// The type asked for.
        target: AtomId,
        /// Where the answer is on the server's window; `None` when there is
        /// none.
        property: AtomId,
    },
    /// The `PropertyNotify` of `property` on the server's selection window
    /// or a window the host watches: a new value, or `deleted`. `time` is
    /// the event's, the server's time when it changed.
    Property {
        window: ResourceId,
        property: AtomId,
        time: u32,
        deleted: bool,
    },
}

/// What the server's selection request named that does not exist: the
/// error the request would have had.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionError {
    BadWindow(ResourceId),
    BadAtom(AtomId),
}

/// The selection bridge's share of `Rootless`.
#[derive(Debug, Default)]
pub(crate) struct SelectionBridge {
    /// What was sent the server's selection window.
    events: Vec<SelectionEvent>,
    /// The client windows whose `PropertyNotify` the host hears.
    watched: HashSet<ResourceId>,
}

/// The server's selection window, made the first time it is asked for (and
/// again after a server reset took it): an unmapped `InputOnly` child of the
/// root.
#[must_use]
pub fn selection_window(state: &mut ServerState) -> ResourceId {
    if state.resources.window(ROOTLESS_SELECTION_WINDOW).is_none() {
        super::wm::create_input_only_as_server(state, ROOTLESS_SELECTION_WINDOW);
    }
    ROOTLESS_SELECTION_WINDOW
}

/// What was sent the server's selection window since the last call, in the
/// order it was sent.
pub fn take_selection_events(state: &mut ServerState) -> Vec<SelectionEvent> {
    state
        .rootless
        .as_mut()
        .map(|r| std::mem::take(&mut r.selection.events))
        .unwrap_or_default()
}

/// Hear the `PropertyNotify` of `window` ([`SelectionEvent::Property`]), or
/// stop (`watch == false`), as a client selecting `PropertyChangeMask` there
/// would: what an owner sending a requestor an INCR transfer waits for. The
/// watch ends when the window is destroyed. No-op when rootless is off.
pub fn watch_properties(state: &mut ServerState, window: ResourceId, watch: bool) {
    let Some(rootless) = state.rootless.as_mut() else {
        return;
    };
    if watch {
        rootless.selection.watched.insert(window);
    } else {
        rootless.selection.watched.remove(&window);
    }
}

/// `window` was destroyed: its watch ends (`DeleteWindowFromAnyEvents`).
pub(crate) fn forget_window(state: &mut ServerState, window: ResourceId) {
    if let Some(rootless) = state.rootless.as_mut() {
        rootless.selection.watched.remove(&window);
    }
}

fn is_server_window(state: &ServerState, window: ResourceId) -> bool {
    window != ROOT_WINDOW && state.resources.window_owner(window) == Some(SERVER_OWNER)
}

fn push(state: &mut ServerState, event: SelectionEvent) {
    if let Some(rootless) = state.rootless.as_mut() {
        rootless.selection.events.push(event);
    }
}

/// `ConvertSelection` hook: a selection the server's window owns is
/// answered by the host. Whether the request was taken here.
pub(crate) fn note_request(
    state: &mut ServerState,
    owner: ResourceId,
    event: SelectionEvent,
) -> bool {
    if state.rootless.is_none() || !is_server_window(state, owner) {
        return false;
    }
    push(state, event);
    true
}

/// `ConvertSelection` hook, for a selection nothing owns: the server's
/// window that asked hears the refusal. Whether it was taken here.
pub(crate) fn note_refusal(
    state: &mut ServerState,
    requestor: ResourceId,
    selection: AtomId,
    target: AtomId,
) -> bool {
    if state.rootless.is_none() || !is_server_window(state, requestor) {
        return false;
    }
    push(
        state,
        SelectionEvent::Notify {
            selection,
            target,
            property: AtomId(0),
        },
    );
    true
}

/// `SendEvent` hook: a `SelectionNotify` for the server's window, the
/// answer to a [`convert_selection`]. Whether it was taken here.
pub(crate) fn note_selection_notify(
    state: &mut ServerState,
    destination: ResourceId,
    words: [u32; 3],
) -> bool {
    if state.rootless.is_none() || !is_server_window(state, destination) {
        return false;
    }
    let [selection, target, property] = words;
    push(
        state,
        SelectionEvent::Notify {
            selection: AtomId(selection),
            target: AtomId(target),
            property: AtomId(property),
        },
    );
    true
}

/// `PropertyNotify` hook, beside the clients' events: the host hears it of
/// the server's selection window and of the windows it watches.
pub(crate) fn note_property_notify(
    state: &mut ServerState,
    window: ResourceId,
    property: AtomId,
    time: u32,
    deleted: bool,
) {
    let Some(rootless) = state.rootless.as_ref() else {
        return;
    };
    let heard = if window == ROOTLESS_SELECTION_WINDOW {
        is_server_window(state, window)
    } else {
        rootless.selection.watched.contains(&window)
    };
    if heard {
        push(
            state,
            SelectionEvent::Property {
                window,
                property,
                time,
                deleted,
            },
        );
    }
}

fn atoms_exist(state: &ServerState, atoms: &[AtomId]) -> Result<(), SelectionError> {
    match atoms.iter().find(|atom| !state.atoms.exists(**atom)) {
        Some(atom) => Err(SelectionError::BadAtom(*atom)),
        None => Ok(()),
    }
}

/// Make the server's selection window the owner of `selection`, or give it
/// up (`own == false`), as a client's `SetSelectionOwner` at `CurrentTime`
/// would: the previous owner hears `SelectionClear`, and XFIXES subscribers
/// are told.
///
/// # Errors
///
/// `selection` is not an atom.
pub fn set_selection_owner(
    state: &mut ServerState,
    selection: AtomId,
    own: bool,
) -> Result<(), SelectionError> {
    atoms_exist(state, &[selection])?;
    let window = if own {
        selection_window(state)
    } else {
        ResourceId(0)
    };
    crate::core_loop::process_request::apply_set_selection_owner(
        state,
        SERVER_OWNER,
        SequenceNumber(0),
        window,
        selection,
        0,
    );
    Ok(())
}

/// Ask for `selection` as `target`, to be put in `property` on the server's
/// selection window, as a client's `ConvertSelection` at `CurrentTime`
/// would. The answer is a [`SelectionEvent::Notify`].
///
/// # Errors
///
/// `selection`, `target` or `property` is not an atom.
pub fn convert_selection(
    state: &mut ServerState,
    selection: AtomId,
    target: AtomId,
    property: AtomId,
) -> Result<(), SelectionError> {
    atoms_exist(state, &[selection, target, property])?;
    let requestor = selection_window(state);
    crate::core_loop::process_request::apply_convert_selection(
        state,
        SERVER_OWNER,
        SequenceNumber(0),
        requestor,
        [selection, target, property],
        0,
    );
    Ok(())
}

/// Tell `requestor`'s client the answer to its `ConvertSelection` is in
/// `property` (`None` for a refusal), as a selection owner's `SendEvent` of
/// a `SelectionNotify` with no event mask would: the event goes to the
/// window's creator, marked as sent. `time` is the request's
/// ([`SelectionEvent::Request`]), as ICCCM §2.2 has the owner answer.
///
/// # Errors
///
/// `requestor` is not a window.
pub fn send_selection_notify(
    state: &mut ServerState,
    time: u32,
    requestor: ResourceId,
    [selection, target, property]: [AtomId; 3],
) -> Result<(), SelectionError> {
    let targets = crate::core_loop::process_request::send_event_recipients(state, requestor, 0, 0)
        .map_err(|_| SelectionError::BadWindow(requestor))?;
    let mut event = [0u8; 32];
    event[0] = 31 | 0x80; // SelectionNotify, from SendEvent
    event[4..8].copy_from_slice(&time.to_le_bytes());
    event[8..12].copy_from_slice(&requestor.0.to_le_bytes());
    event[12..16].copy_from_slice(&selection.0.to_le_bytes());
    event[16..20].copy_from_slice(&target.0.to_le_bytes());
    event[20..24].copy_from_slice(&property.0.to_le_bytes());
    let _dropped = crate::core_loop::fanout::fanout_raw_event_to_clients(
        state,
        &targets,
        &event,
        ClientByteOrder::LittleEndian,
    );
    Ok(())
}

/// Replace `property` on `window` with `data` of `type` in `format` (8, 16
/// or 32 bits, `data` in this machine's order), as a client's
/// `ChangeProperty` would: the window's watchers hear `PropertyNotify`. How
/// the host answers a [`SelectionEvent::Request`]. A format other than 8,
/// 16 or 32 is ignored.
pub fn change_property(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    window: ResourceId,
    [property, r#type]: [AtomId; 2],
    format: u8,
    data: Vec<u8>,
) {
    use crate::properties::PropertyFormat;
    let format = match format {
        8 => PropertyFormat::F8,
        16 => PropertyFormat::F16,
        32 => PropertyFormat::F32,
        _ => return,
    };
    crate::core_loop::process_request::change_property_as_server(
        state, backend, window, property, r#type, format, data,
    );
}

/// Delete `property` from `window`, as a client's `DeleteProperty` would:
/// the window's watchers hear `PropertyNotify`. How the host, having read an
/// INCR chunk an owner put on the server's window, asks for the next.
pub fn delete_property(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    window: ResourceId,
    property: AtomId,
) {
    crate::core_loop::process_request::delete_window_property(state, backend, window, property);
}
