//! A host's selection bridge: a window of the server's own that owns X
//! selections on the host's behalf and asks X clients for theirs, as a
//! Wayland compositor's X window manager does with a window of its own
//! (wlroots' `xwayland/selection`).
//!
//! A server window has no connection, so what the selection protocol would
//! send it -- the `SelectionRequest` a client's `ConvertSelection` sends a
//! selection's owner, the `SelectionClear` its owner hears when another
//! takes it, the `SelectionNotify` an owner sends the server window that
//! asked, and the `PropertyNotify` of a property on it -- is queued in
//! `Rootless` for the host to take ([`take_selection_events`]). So is the
//! `PropertyNotify` of a client's window the host watches
//! ([`watch_properties`]), the requestor of a transfer it answers, and that
//! window's destruction. With both the host can run ICCCM's INCR transfers
//! either way; the protocol itself is the host's. And as a compositor's X
//! window manager selects XFIXES `SelectionNotify`, the host hears who owns
//! the selections it watches ([`watch_selection`]) each time that changes,
//! so it never has to look.
//!
//! Only the bridge's own window ([`selection_window`]) is the host's: the
//! server's other windows (the window manager's check window, which owns
//! `WM_S0`) hear nothing, as a window whose client is gone.
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
    resources::{ROOTLESS_SELECTION_WINDOW, SERVER_OWNER},
    server::{SelectionOwner, ServerState},
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
        /// The answer's time. ICCCM §2.2 has the owner answer with its
        /// request's ([`convert_selection`]'s `time`), so an answer with
        /// another time is to another request.
        time: u32,
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
    /// The `SelectionClear` the server's window would be sent: it owned
    /// `selection`, and a client took it or the server gave it up
    /// (`dix/selection.c:183-191`). `time` is the new owner's.
    Clear { selection: AtomId, time: u32 },
    /// A window the host watched ([`watch_properties`]) was destroyed: its
    /// watch is over, and so is a transfer to it.
    Destroyed { window: ResourceId },
    /// The owner of a selection the host watches ([`watch_selection`])
    /// changed, as XFIXES `SelectionNotify` tells a client that selected
    /// it, under the same three causes (`xfixes/select.c`): a
    /// `SetSelectionOwner`, the server's own included, gives `owner`
    /// (`None` for a release); the owner's window destroyed or its client
    /// gone gives `None`. `time` is the selection's last-change time.
    Owner {
        selection: AtomId,
        owner: Option<ResourceId>,
        time: u32,
    },
}

/// What the server's selection request named that does not exist: the
/// error the request would have had.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionError {
    BadWindow(ResourceId),
    BadAtom(AtomId),
    /// A property format other than 8, 16 or 32.
    BadValue(u32),
    /// Property data that is not a whole number of `format`-bit items.
    BadLength,
}

/// The selection bridge's share of `Rootless`.
#[derive(Debug, Default)]
pub(crate) struct SelectionBridge {
    /// What was sent the server's selection window.
    events: Vec<SelectionEvent>,
    /// The client windows whose `PropertyNotify` the host hears.
    watched: HashSet<ResourceId>,
    /// The selections whose owner changes the host hears.
    selections: HashSet<AtomId>,
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

/// Hear each change of `selection`'s owner ([`SelectionEvent::Owner`]), or
/// stop (`watch == false`), as a client's XFIXES `SelectSelectionInput`
/// with all three causes would. What owns it now is `state.selections`,
/// read once when the watch starts. No-op when rootless is off.
///
/// # Errors
///
/// `selection` is not an atom.
pub fn watch_selection(
    state: &mut ServerState,
    selection: AtomId,
    watch: bool,
) -> Result<(), SelectionError> {
    atoms_exist(state, &[selection])?;
    if let Some(rootless) = state.rootless.as_mut() {
        if watch {
            rootless.selection.selections.insert(selection);
        } else {
            rootless.selection.selections.remove(&selection);
        }
    }
    Ok(())
}

/// XFIXES `SelectionNotify` hook: `selection`'s owner is `owner` now (0 for
/// none), its last change at `time`; a watched selection is the host's to
/// hear.
pub(crate) fn note_owner(state: &mut ServerState, selection: AtomId, owner: u32, time: u32) {
    let Some(rootless) = state.rootless.as_mut() else {
        return;
    };
    if rootless.selection.selections.contains(&selection) {
        rootless.selection.events.push(SelectionEvent::Owner {
            selection,
            owner: (owner != 0).then_some(ResourceId(owner)),
            time,
        });
    }
}

/// `window` was destroyed: its watch ends (`DeleteWindowFromAnyEvents`),
/// and the host hears it ([`SelectionEvent::Destroyed`]).
pub(crate) fn forget_window(state: &mut ServerState, window: ResourceId) {
    if let Some(rootless) = state.rootless.as_mut()
        && rootless.selection.watched.remove(&window)
    {
        rootless
            .selection
            .events
            .push(SelectionEvent::Destroyed { window });
    }
}

/// `window` is the bridge's own, the server's selection window.
fn is_bridge_window(state: &ServerState, window: ResourceId) -> bool {
    window == ROOTLESS_SELECTION_WINDOW
        && state.resources.window_owner(window) == Some(SERVER_OWNER)
}

fn push(state: &mut ServerState, event: SelectionEvent) {
    if let Some(rootless) = state.rootless.as_mut() {
        rootless.selection.events.push(event);
    }
}

/// The server set `owner` (the record's `client`, the one that hears
/// what the selection's owner is sent), and it is the bridge's window.
fn bridge_owns(state: &ServerState, owner: &SelectionOwner) -> bool {
    owner.client == Some(SERVER_OWNER) && is_bridge_window(state, owner.window)
}

/// `ConvertSelection` hook: a selection the server's window owns is
/// answered by the host. Whether the request was taken here.
pub(crate) fn note_request(
    state: &mut ServerState,
    owner: &SelectionOwner,
    event: SelectionEvent,
) -> bool {
    if state.rootless.is_none() || !bridge_owns(state, owner) {
        return false;
    }
    push(state, event);
    true
}

/// `SetSelectionOwner` hook: `old` was the server's window, which is to
/// hear `SelectionClear` for `selection`. Whether it was taken here.
pub(crate) fn note_selection_clear(
    state: &mut ServerState,
    old: &SelectionOwner,
    selection: AtomId,
    time: u32,
) -> bool {
    if state.rootless.is_none() || !bridge_owns(state, old) {
        return false;
    }
    push(state, SelectionEvent::Clear { selection, time });
    true
}

/// `ConvertSelection` hook, for a selection nothing owns: the server's
/// window that asked hears the refusal. Whether it was taken here.
pub(crate) fn note_refusal(
    state: &mut ServerState,
    requestor: ResourceId,
    time: u32,
    selection: AtomId,
    target: AtomId,
) -> bool {
    if state.rootless.is_none() || !is_bridge_window(state, requestor) {
        return false;
    }
    push(
        state,
        SelectionEvent::Notify {
            time,
            selection,
            target,
            property: AtomId(0),
        },
    );
    true
}

/// `SendEvent` hook: a `SelectionNotify` for the server's window, the
/// answer to a [`convert_selection`]: its time, selection, target and
/// property. Whether it was taken here.
pub(crate) fn note_selection_notify(
    state: &mut ServerState,
    destination: ResourceId,
    words: [u32; 4],
) -> bool {
    if state.rootless.is_none() || !is_bridge_window(state, destination) {
        return false;
    }
    let [time, selection, target, property] = words;
    push(
        state,
        SelectionEvent::Notify {
            time,
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
        is_bridge_window(state, window)
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
/// selection window, as a client's `ConvertSelection` at `time` would. The
/// answer is a [`SelectionEvent::Notify`], which has the time the owner
/// answered with: its request's (ICCCM §2.2), so a host that asks each time
/// with a time of its own knows which request an answer is to.
///
/// # Errors
///
/// `selection`, `target` or `property` is not an atom.
pub fn convert_selection(
    state: &mut ServerState,
    selection: AtomId,
    target: AtomId,
    property: AtomId,
    time: u32,
) -> Result<(), SelectionError> {
    atoms_exist(state, &[selection, target, property])?;
    let requestor = selection_window(state);
    crate::core_loop::process_request::apply_convert_selection(
        state,
        SERVER_OWNER,
        SequenceNumber(0),
        requestor,
        [selection, target, property],
        time,
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
/// the host answers a [`SelectionEvent::Request`].
///
/// # Errors
///
/// What `ChangeProperty` would refuse, in its order (`ProcChangeProperty`):
/// `format` is not 8, 16 or 32, `data` is not a whole number of items,
/// `window` is not a window, or `property` or `type` is not an atom. Nothing
/// changes.
pub fn change_property(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    window: ResourceId,
    [property, r#type]: [AtomId; 2],
    format: u8,
    data: Vec<u8>,
) -> Result<(), SelectionError> {
    let format = crate::properties::PropertyFormat::from_protocol(format)
        .ok_or(SelectionError::BadValue(u32::from(format)))?;
    if !data.len().is_multiple_of(format.bytes()) {
        return Err(SelectionError::BadLength);
    }
    window_exists(state, window)?;
    atoms_exist(state, &[property, r#type])?;
    crate::core_loop::process_request::change_property_as_server(
        state, backend, window, property, r#type, format, data,
    );
    Ok(())
}

fn window_exists(state: &ServerState, window: ResourceId) -> Result<(), SelectionError> {
    match state.resources.window(window) {
        Some(_) => Ok(()),
        None => Err(SelectionError::BadWindow(window)),
    }
}

/// Delete `property` from `window`, as a client's `DeleteProperty` would:
/// the window's watchers hear `PropertyNotify`. How the host, having read an
/// INCR chunk an owner put on the server's window, asks for the next.
///
/// # Errors
///
/// `window` is not a window, or `property` is not an atom, as
/// `DeleteProperty` would refuse. Nothing changes.
pub fn delete_property(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    window: ResourceId,
    property: AtomId,
) -> Result<(), SelectionError> {
    window_exists(state, window)?;
    atoms_exist(state, &[property])?;
    crate::core_loop::process_request::delete_window_property(state, backend, window, property);
    Ok(())
}
