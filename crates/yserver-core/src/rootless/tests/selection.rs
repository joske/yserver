//! The host's selection bridge (`rootless/selection.rs`).

use yserver_protocol::x11::{AtomId, ResourceId};

use super::{RecordingHost, assert_no_error};
use crate::{
    resources::{ROOT_WINDOW, ROOTLESS_SELECTION_WINDOW},
    rootless::{SelectionError, SelectionEvent, selection},
};

const WINDOW: u32 = 0x0020_0001;
/// Client 2's window.
const OTHER: u32 = 0x0040_0001;

fn words(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

/// A client's ConvertSelection of a selection the server's window owns is
/// the host's to answer: it is queued, and nothing reaches the client.
#[test]
fn a_selection_the_server_owns_is_asked_of_the_host() {
    let mut host = RecordingHost::new();
    host.create_window(1, WINDOW, ROOT_WINDOW.0, (0, 0), (10, 10), 0, false);
    let clipboard = AtomId(host.atom("CLIPBOARD"));
    let utf8 = AtomId(host.atom("UTF8_STRING"));
    let property = AtomId(host.atom("XSEL_DATA"));
    selection::set_selection_owner(&mut host.state, clipboard, true).expect("own");
    // ConvertSelection (24): requestor, selection, target, property, time.
    let reply = host.request(
        1,
        24,
        0,
        &words(&[WINDOW, clipboard.0, utf8.0, property.0, 0]),
    );
    assert!(reply.is_empty(), "the client waits for the host: {reply:?}");
    assert_eq!(
        selection::take_selection_events(&mut host.state),
        [SelectionEvent::Request {
            time: 0,
            requestor: ResourceId(WINDOW),
            selection: clipboard,
            target: utf8,
            property,
        }]
    );
}

/// The server's ConvertSelection of a selection nothing owns is refused to
/// the host; an owner's SelectionNotify for the server's window is the
/// host's.
#[test]
fn the_server_hears_the_answer_to_its_own_convert_selection() {
    let mut host = RecordingHost::new();
    let primary = AtomId(host.atom("PRIMARY"));
    let utf8 = AtomId(host.atom("UTF8_STRING"));
    let property = AtomId(host.atom("_YSERVER_SELECTION"));
    selection::convert_selection(&mut host.state, primary, utf8, property).expect("convert");
    assert_eq!(
        selection::take_selection_events(&mut host.state),
        [SelectionEvent::Notify {
            selection: primary,
            target: utf8,
            property: AtomId(0),
        }]
    );
    // SendEvent (25) of a SelectionNotify to the server's window.
    let mut event = [0u8; 32];
    event[0] = 31;
    event[8..12].copy_from_slice(&ROOTLESS_SELECTION_WINDOW.0.to_le_bytes());
    event[12..16].copy_from_slice(&primary.0.to_le_bytes());
    event[16..20].copy_from_slice(&utf8.0.to_le_bytes());
    event[20..24].copy_from_slice(&property.0.to_le_bytes());
    let mut body = words(&[ROOTLESS_SELECTION_WINDOW.0, 0]);
    body.extend_from_slice(&event);
    let reply = host.request(1, 25, 0, &body);
    assert!(reply.is_empty(), "no error: {reply:?}");
    assert_eq!(
        selection::take_selection_events(&mut host.state),
        [SelectionEvent::Notify {
            selection: primary,
            target: utf8,
            property,
        }]
    );
}

/// The server's SetSelectionOwner takes the selection from a client as
/// another client's would: the client that set it hears SelectionClear.
#[test]
fn the_server_taking_a_selection_clears_the_client_that_had_it() {
    let mut host = RecordingHost::new();
    host.create_window(1, WINDOW, ROOT_WINDOW.0, (0, 0), (10, 10), 0, false);
    let clipboard = AtomId(host.atom("CLIPBOARD"));
    // SetSelectionOwner (22): owner, selection, time.
    let out = host.request(1, 22, 0, &words(&[WINDOW, clipboard.0, 0]));
    assert_no_error(&out, "SetSelectionOwner");
    selection::set_selection_owner(&mut host.state, clipboard, true).expect("own");
    let out = host.read(1);
    let clears: Vec<&[u8]> = out.chunks(32).filter(|e| e[0] == 29).collect();
    assert_eq!(clears.len(), 1, "{out:?}");
    let word = |at: usize| u32::from_le_bytes(clears[0][at..at + 4].try_into().unwrap());
    assert_eq!(word(8), WINDOW);
    assert_eq!(word(12), clipboard.0);
    assert_eq!(
        host.state.selections[&clipboard].window,
        ROOTLESS_SELECTION_WINDOW
    );
}

/// What the request would refuse as BadAtom or BadWindow, the server's
/// typed actions refuse too, and change nothing.
#[test]
fn the_server_names_no_atom_or_window_that_is_not_there() {
    let mut host = RecordingHost::new();
    let missing = AtomId(0x7fff_0000);
    let utf8 = AtomId(host.atom("UTF8_STRING"));
    assert_eq!(
        selection::set_selection_owner(&mut host.state, missing, true),
        Err(SelectionError::BadAtom(missing))
    );
    assert!(!host.state.selections.contains_key(&missing));
    assert_eq!(
        selection::convert_selection(&mut host.state, utf8, missing, utf8),
        Err(SelectionError::BadAtom(missing))
    );
    assert_eq!(
        selection::send_selection_notify(&mut host.state, 0, ResourceId(0x0066_0001), [utf8; 3]),
        Err(SelectionError::BadWindow(ResourceId(0x0066_0001)))
    );
    assert!(selection::take_selection_events(&mut host.state).is_empty());
}

/// The host's answer to a client's request is a sent SelectionNotify, at
/// the request's time, to the requestor's creator.
#[test]
fn the_hosts_answer_reaches_the_requestor_as_a_sent_event() {
    let mut host = RecordingHost::new();
    host.create_window(1, WINDOW, ROOT_WINDOW.0, (0, 0), (10, 10), 0, false);
    let clipboard = AtomId(host.atom("CLIPBOARD"));
    let utf8 = AtomId(host.atom("UTF8_STRING"));
    let property = AtomId(host.atom("XSEL_DATA"));
    host.read(1);
    selection::send_selection_notify(
        &mut host.state,
        1234,
        ResourceId(WINDOW),
        [clipboard, utf8, property],
    )
    .expect("notify");
    let out = host.read(1);
    assert_eq!(out.len(), 32, "{out:?}");
    let word = |at: usize| u32::from_le_bytes(out[at..at + 4].try_into().unwrap());
    assert_eq!(out[0], 31 | 0x80);
    assert_eq!(
        [word(4), word(8), word(12), word(16), word(20)],
        [1234, WINDOW, clipboard.0, utf8.0, property.0]
    );
}

/// INCR to the host: the owner's chunks on the server's window, and their
/// deletion by the host, are PropertyNotify the host hears; the owner hears
/// the deletion it waits for.
#[test]
fn the_host_hears_property_changes_on_its_selection_window() {
    let mut host = RecordingHost::new();
    host.create_window(1, WINDOW, ROOT_WINDOW.0, (0, 0), (10, 10), 0, false);
    let window = selection::selection_window(&mut host.state);
    let property = AtomId(host.atom("_YSERVER_SELECTION"));
    // The owner selects PropertyChange on the requestor, as INCR has it.
    let out = host.request(1, 2, 0, &words(&[window.0, 0x0800, 0x0040_0000]));
    assert_no_error(&out, "ChangeWindowAttributes");
    host.change_property_bytes(1, window.0, "_YSERVER_SELECTION", "INCR", b"abcd");
    selection::delete_property(&mut host.state, &mut host.backend, window, property);
    let events = selection::take_selection_events(&mut host.state);
    let [
        SelectionEvent::Property {
            window: w1,
            property: p1,
            time: t1,
            deleted: false,
        },
        SelectionEvent::Property {
            window: w2,
            property: p2,
            time: t2,
            deleted: true,
        },
    ] = events.as_slice()
    else {
        panic!("{events:?}");
    };
    assert_eq!((*w1, *p1, *w2, *p2), (window, property, window, property));
    assert!(t2 >= t1);
    // The NewValue went to the owner with its ChangeProperty's reply.
    let out = host.read(1);
    let notifies: Vec<(u32, u8)> = out
        .chunks(32)
        .filter(|e| e[0] == 28)
        .map(|e| (u32::from_le_bytes(e[12..16].try_into().unwrap()), e[16]))
        .collect();
    assert_eq!(
        notifies,
        [(*t2, 1)],
        "the owner heard the deletion, at the host's time"
    );
}

/// INCR from the host: a requestor's window it watches is heard until the
/// watch ends or the window goes; one it does not watch never is.
#[test]
fn the_host_hears_a_watched_requestors_properties_until_the_watch_ends() {
    let mut host = RecordingHost::new();
    host.connect(2, yserver_protocol::x11::ClientByteOrder::LittleEndian);
    host.create_window(1, WINDOW, ROOT_WINDOW.0, (0, 0), (10, 10), 0, false);
    host.create_window(2, OTHER, ROOT_WINDOW.0, (0, 0), (10, 10), 0, false);
    selection::watch_properties(&mut host.state, ResourceId(WINDOW), true);
    host.change_property_bytes(2, OTHER, "XSEL_DATA", "STRING", b"no");
    assert!(selection::take_selection_events(&mut host.state).is_empty());
    host.change_property_bytes(1, WINDOW, "XSEL_DATA", "STRING", b"x");
    host.delete_property(1, WINDOW, "XSEL_DATA");
    let property = AtomId(host.atom("XSEL_DATA"));
    let heard: Vec<(ResourceId, AtomId, bool)> = selection::take_selection_events(&mut host.state)
        .into_iter()
        .map(|e| match e {
            SelectionEvent::Property {
                window,
                property,
                deleted,
                ..
            } => (window, property, deleted),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        heard,
        [
            (ResourceId(WINDOW), property, false),
            (ResourceId(WINDOW), property, true)
        ]
    );
    selection::watch_properties(&mut host.state, ResourceId(WINDOW), false);
    host.change_property_bytes(1, WINDOW, "XSEL_DATA", "STRING", b"y");
    assert!(selection::take_selection_events(&mut host.state).is_empty());
    // A destroyed window's watch is gone with it.
    selection::watch_properties(&mut host.state, ResourceId(WINDOW), true);
    host.destroy(1, WINDOW);
    host.create_window(1, WINDOW, ROOT_WINDOW.0, (0, 0), (10, 10), 0, false);
    host.change_property_bytes(1, WINDOW, "XSEL_DATA", "STRING", b"z");
    assert!(selection::take_selection_events(&mut host.state).is_empty());
}
