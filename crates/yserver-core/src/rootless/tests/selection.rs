//! The host's selection bridge (`rootless/selection.rs`).

use yserver_protocol::x11::{AtomId, ResourceId};

use super::{RecordingHost, assert_no_error};
use crate::{
    resources::{ROOT_WINDOW, ROOTLESS_SELECTION_WINDOW, ROOTLESS_WM_CHECK_WINDOW},
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
/// the host, at the request's time; an owner's SelectionNotify for the
/// server's window is the host's, with the time the owner gave it.
#[test]
fn the_server_hears_the_answer_to_its_own_convert_selection() {
    let mut host = RecordingHost::new();
    let primary = AtomId(host.atom("PRIMARY"));
    let utf8 = AtomId(host.atom("UTF8_STRING"));
    let property = AtomId(host.atom("_YSERVER_SELECTION"));
    selection::convert_selection(&mut host.state, primary, utf8, property, 77).expect("convert");
    assert_eq!(
        selection::take_selection_events(&mut host.state),
        [SelectionEvent::Notify {
            time: 77,
            selection: primary,
            target: utf8,
            property: AtomId(0),
        }]
    );
    // SendEvent (25) of a SelectionNotify to the server's window.
    let mut event = [0u8; 32];
    event[0] = 31;
    event[4..8].copy_from_slice(&0x1234_u32.to_le_bytes());
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
            time: 0x1234,
            selection: primary,
            target: utf8,
            property,
        }]
    );
}

/// The owner of a selection the server asks for is sent the
/// SelectionRequest at the server's request's time, which it answers with.
#[test]
fn the_owner_is_asked_at_the_servers_requests_time() {
    let mut host = RecordingHost::new();
    host.create_window(1, WINDOW, ROOT_WINDOW.0, (0, 0), (10, 10), 0, false);
    let clipboard = AtomId(host.atom("CLIPBOARD"));
    let utf8 = AtomId(host.atom("UTF8_STRING"));
    let property = AtomId(host.atom("_YSERVER_SELECTION"));
    let out = host.request(1, 22, 0, &words(&[WINDOW, clipboard.0, 0]));
    assert_no_error(&out, "SetSelectionOwner");
    host.read(1);
    for time in [5000, 5001] {
        selection::convert_selection(&mut host.state, clipboard, utf8, property, time)
            .expect("convert");
    }
    let out = host.read(1);
    let requests: Vec<&[u8]> = out.chunks(32).filter(|e| e[0] == 30).collect();
    let word = |event: &[u8], at: usize| u32::from_le_bytes(event[at..at + 4].try_into().unwrap());
    assert_eq!(requests.len(), 2, "{out:?}");
    assert_eq!(word(requests[0], 4), 5000);
    assert_eq!(word(requests[1], 4), 5001);
    assert_eq!(word(requests[1], 12), ROOTLESS_SELECTION_WINDOW.0);
    assert!(selection::take_selection_events(&mut host.state).is_empty());
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
        selection::convert_selection(&mut host.state, utf8, missing, utf8, 0),
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
    selection::delete_property(&mut host.state, &mut host.backend, window, property)
        .expect("delete");
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
    // A destroyed window's watch is gone with it, and the host hears so.
    selection::watch_properties(&mut host.state, ResourceId(WINDOW), true);
    host.destroy(1, WINDOW);
    assert_eq!(
        selection::take_selection_events(&mut host.state),
        [SelectionEvent::Destroyed {
            window: ResourceId(WINDOW)
        }]
    );
    host.create_window(1, WINDOW, ROOT_WINDOW.0, (0, 0), (10, 10), 0, false);
    host.change_property_bytes(1, WINDOW, "XSEL_DATA", "STRING", b"z");
    assert!(selection::take_selection_events(&mut host.state).is_empty());
}

/// The host hears SelectionClear when a client takes a selection its window
/// owns, or when the server gives it up, under Xorg's rule
/// (`dix/selection.c:183-191`): the server taking it again is no clear.
#[test]
fn the_host_hears_its_window_lose_a_selection() {
    let mut host = RecordingHost::new();
    host.create_window(1, WINDOW, ROOT_WINDOW.0, (0, 0), (10, 10), 0, false);
    let clipboard = AtomId(host.atom("CLIPBOARD"));
    selection::set_selection_owner(&mut host.state, clipboard, true).expect("own");
    selection::set_selection_owner(&mut host.state, clipboard, true).expect("own again");
    assert!(selection::take_selection_events(&mut host.state).is_empty());
    host.read(1);
    // SetSelectionOwner (22): owner, selection, time.
    let out = host.request(1, 22, 0, &words(&[WINDOW, clipboard.0, 0]));
    assert_no_error(&out, "SetSelectionOwner");
    let time = host.state.selections[&clipboard].last_time_changed;
    assert_eq!(
        selection::take_selection_events(&mut host.state),
        [SelectionEvent::Clear {
            selection: clipboard,
            time
        }]
    );
    assert!(
        host.read(1).chunks(32).all(|e| e[0] != 29),
        "no client hears the server's SelectionClear"
    );
    // The server takes it back and gives it up: the client is cleared, then
    // the server's window is, as Xorg clears an owner releasing its own.
    selection::set_selection_owner(&mut host.state, clipboard, true).expect("own");
    selection::set_selection_owner(&mut host.state, clipboard, false).expect("release");
    let events = selection::take_selection_events(&mut host.state);
    assert!(
        matches!(events.as_slice(), [SelectionEvent::Clear { selection, .. }] if *selection == clipboard),
        "{events:?}"
    );
}

/// Only the bridge's own window is the host's: a client that names it as a
/// selection's owner is cleared itself, and the server's other windows (the
/// WM's check window, which owns `WM_S0`) hear nothing.
#[test]
fn only_the_bridge_window_set_by_the_server_is_the_hosts() {
    let mut host = RecordingHost::new();
    host.connect(2, yserver_protocol::x11::ClientByteOrder::LittleEndian);
    host.create_window(1, WINDOW, ROOT_WINDOW.0, (0, 0), (10, 10), 0, false);
    host.create_window(2, OTHER, ROOT_WINDOW.0, (0, 0), (10, 10), 0, false);
    let bridge = selection::selection_window(&mut host.state);
    let primary = AtomId(host.atom("PRIMARY"));
    let wm_s0 = AtomId(host.atom("WM_S0"));
    let utf8 = AtomId(host.atom("UTF8_STRING"));
    let property = AtomId(host.atom("XSEL_DATA"));
    // Client 1 sets the bridge's window as PRIMARY's owner; client 2's
    // ConvertSelection goes to client 1, and so does the SelectionClear.
    let out = host.request(1, 22, 0, &words(&[bridge.0, primary.0, 0]));
    assert_no_error(&out, "SetSelectionOwner");
    host.read(1);
    host.request(2, 24, 0, &words(&[OTHER, primary.0, utf8.0, property.0, 0]));
    assert_eq!(
        host.read(1).chunks(32).filter(|e| e[0] == 30).count(),
        1,
        "SelectionRequest to the client that set the owner"
    );
    host.request(2, 22, 0, &words(&[OTHER, primary.0, 0]));
    assert_eq!(host.read(1).chunks(32).filter(|e| e[0] == 29).count(), 1);
    // WM_S0, owned by the check window: ConvertSelection is refused to
    // client 2 (no one to answer), a client taking it clears no one, and a
    // SelectionNotify sent to the check window is not the host's.
    host.read(2);
    let out = host.request(2, 24, 0, &words(&[OTHER, wm_s0.0, utf8.0, property.0, 0]));
    let notifies: Vec<&[u8]> = out.chunks(32).filter(|e| e[0] == 31).collect();
    assert_eq!(notifies.len(), 1, "{out:?}");
    assert_eq!(
        u32::from_le_bytes(notifies[0][20..24].try_into().unwrap()),
        0
    );
    host.request(2, 22, 0, &words(&[OTHER, wm_s0.0, 0]));
    let mut event = [0u8; 32];
    event[0] = 31;
    event[8..12].copy_from_slice(&ROOTLESS_WM_CHECK_WINDOW.0.to_le_bytes());
    let mut body = words(&[ROOTLESS_WM_CHECK_WINDOW.0, 0]);
    body.extend_from_slice(&event);
    let out = host.request(1, 25, 0, &body);
    assert!(out.is_empty(), "no error: {out:?}");
    assert!(selection::take_selection_events(&mut host.state).is_empty());
}

/// A watched requestor that goes with its client's connection is heard as
/// destroyed too; a window the host never watched is not.
#[test]
fn a_watched_requestor_gone_with_its_client_ends_the_transfer() {
    let mut host = RecordingHost::new();
    host.connect(2, yserver_protocol::x11::ClientByteOrder::LittleEndian);
    host.create_window(1, WINDOW, ROOT_WINDOW.0, (0, 0), (10, 10), 0, false);
    host.create_window(2, OTHER, ROOT_WINDOW.0, (0, 0), (10, 10), 0, false);
    selection::watch_properties(&mut host.state, ResourceId(OTHER), true);
    host.destroy(1, WINDOW);
    assert!(selection::take_selection_events(&mut host.state).is_empty());
    host.disconnect(2);
    assert_eq!(
        selection::take_selection_events(&mut host.state),
        [SelectionEvent::Destroyed {
            window: ResourceId(OTHER)
        }]
    );
}

/// The host's ChangeProperty and DeleteProperty refuse what the requests
/// would, in `ProcChangeProperty`'s order, and change nothing.
#[test]
fn the_hosts_property_writes_refuse_what_the_requests_would() {
    let mut host = RecordingHost::new();
    host.create_window(1, WINDOW, ROOT_WINDOW.0, (0, 0), (10, 10), 0, false);
    let property = AtomId(host.atom("XSEL_DATA"));
    let utf8 = AtomId(host.atom("UTF8_STRING"));
    let missing_atom = AtomId(0x7fff_0000);
    let missing = ResourceId(0x0066_0001);
    let window = ResourceId(WINDOW);
    selection::watch_properties(&mut host.state, window, true);
    let change = |host: &mut RecordingHost, window, atoms, format, data: &[u8]| {
        selection::change_property(
            &mut host.state,
            &mut host.backend,
            window,
            atoms,
            format,
            data.to_vec(),
        )
    };
    assert_eq!(
        change(&mut host, missing, [property, utf8], 7, b"x"),
        Err(SelectionError::BadValue(7))
    );
    assert_eq!(
        change(&mut host, missing, [property, utf8], 16, b"x"),
        Err(SelectionError::BadLength)
    );
    assert_eq!(
        change(&mut host, window, [property, utf8], 32, b"abcdef"),
        Err(SelectionError::BadLength)
    );
    assert_eq!(
        change(&mut host, missing, [property, utf8], 16, b"xy"),
        Err(SelectionError::BadWindow(missing))
    );
    assert_eq!(
        change(&mut host, window, [property, missing_atom], 8, b"x"),
        Err(SelectionError::BadAtom(missing_atom))
    );
    assert!(selection::take_selection_events(&mut host.state).is_empty());
    assert_eq!(
        selection::delete_property(&mut host.state, &mut host.backend, missing, property),
        Err(SelectionError::BadWindow(missing))
    );
    assert_eq!(
        selection::delete_property(&mut host.state, &mut host.backend, window, missing_atom),
        Err(SelectionError::BadAtom(missing_atom))
    );
    // A whole number of items is written, and heard.
    change(&mut host, window, [property, utf8], 16, b"xy").expect("change");
    selection::delete_property(&mut host.state, &mut host.backend, window, property)
        .expect("delete");
    let deleted: Vec<bool> = selection::take_selection_events(&mut host.state)
        .into_iter()
        .map(|e| match e {
            SelectionEvent::Property { deleted, .. } => deleted,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(deleted, [false, true]);
}

/// The host hears each change of a watched selection's owner, under the
/// three causes XFIXES `SelectionNotify` has: a SetSelectionOwner (the
/// server's own too), the owner's window destroyed, its client gone. An
/// unwatched selection, a stale SetSelectionOwner, or a watch ended hears
/// nothing, and XFIXES subscribers hear what they did before.
#[test]
fn the_host_hears_a_watched_selections_owner_change_as_xfixes_does() {
    let mut host = RecordingHost::new();
    // A server time well past 1 ms, so that `time - 1` below is earlier
    // than the owner's time, and stale, whatever the clock read.
    backdate_clock(&mut host);
    host.connect(2, yserver_protocol::x11::ClientByteOrder::LittleEndian);
    host.create_window(1, WINDOW, ROOT_WINDOW.0, (0, 0), (10, 10), 0, false);
    host.create_window(2, OTHER, ROOT_WINDOW.0, (0, 0), (10, 10), 0, false);
    let clipboard = AtomId(host.atom("CLIPBOARD"));
    let primary = AtomId(host.atom("PRIMARY"));
    let missing = AtomId(0x7fff_0000);
    assert_eq!(
        selection::watch_selection(&mut host.state, missing, true),
        Err(SelectionError::BadAtom(missing))
    );
    selection::watch_selection(&mut host.state, clipboard, true).expect("watch");
    // Client 2 selects XFIXES SelectionNotify (140, minor 2: window,
    // selection, mask) on CLIPBOARD for all three causes.
    let xfixes_version = host.request(2, 140, 0, &words(&[5, 0]));
    assert_eq!(
        xfixes_version[0], 1,
        "XFIXES QueryVersion: {xfixes_version:?}"
    );
    let out = host.request(2, 140, 2, &words(&[OTHER, clipboard.0, 7]));
    assert_no_error(&out, "SelectSelectionInput");

    let owner_of = |host: &RecordingHost| host.state.selections[&clipboard].last_time_changed;
    let out = host.request(1, 22, 0, &words(&[WINDOW, clipboard.0, 0]));
    assert_no_error(&out, "SetSelectionOwner");
    let time = owner_of(&host);
    // A stale SetSelectionOwner is ignored, and so heard by no one.
    host.request(1, 22, 0, &words(&[WINDOW, clipboard.0, time - 1]));
    // PRIMARY is not watched.
    host.request(1, 22, 0, &words(&[WINDOW, primary.0, 0]));
    assert_eq!(
        selection::take_selection_events(&mut host.state),
        [SelectionEvent::Owner {
            selection: clipboard,
            owner: Some(ResourceId(WINDOW)),
            time
        }]
    );
    // The server taking it is heard too, after the client's SelectionClear.
    selection::set_selection_owner(&mut host.state, clipboard, true).expect("own");
    let time = owner_of(&host);
    assert_eq!(
        selection::take_selection_events(&mut host.state),
        [SelectionEvent::Owner {
            selection: clipboard,
            owner: Some(ROOTLESS_SELECTION_WINDOW),
            time
        }]
    );
    // Its window destroyed, then its client gone: the owner is none.
    host.request(1, 22, 0, &words(&[WINDOW, clipboard.0, 0]));
    let _ = selection::take_selection_events(&mut host.state);
    host.destroy(1, WINDOW);
    let gone: Vec<SelectionEvent> = selection::take_selection_events(&mut host.state);
    assert!(
        matches!(gone.as_slice(), [SelectionEvent::Owner { owner: None, .. }]),
        "{gone:?}"
    );
    let mut xfixes = host.read(2);
    xfixes.extend(host.request(2, 22, 0, &words(&[OTHER, clipboard.0, 0])));
    let _ = selection::take_selection_events(&mut host.state);
    let xfixes_heard = xfixes
        .chunks(32)
        .filter(|e| e[0] & 0x7f == crate::nested::XFIXES_FIRST_EVENT)
        .count();
    host.disconnect(2);
    let gone: Vec<SelectionEvent> = selection::take_selection_events(&mut host.state);
    assert!(
        matches!(gone.as_slice(), [SelectionEvent::Owner { owner: None, .. }]),
        "{gone:?}"
    );
    // Client 2 heard its XFIXES notifies throughout: set by 1, by the
    // server, by 1 again, destroyed, set by itself.
    assert_eq!(xfixes_heard, 5);
    // The watch ended: nothing more.
    selection::watch_selection(&mut host.state, clipboard, false).expect("unwatch");
    selection::set_selection_owner(&mut host.state, clipboard, true).expect("own");
    assert!(selection::take_selection_events(&mut host.state).is_empty());
}

/// Set the server's clock 10 s back, so its times are far from 0 and 1.
fn backdate_clock(host: &mut RecordingHost) {
    host.state.start_instant = host
        .state
        .start_instant
        .checked_sub(std::time::Duration::from_secs(10))
        .expect("a clock 10 s back");
}

/// Xorg's staleness rule for SetSelectionOwner (`dix/selection.c`): a time
/// earlier than the selection's last change is ignored, one equal to it is
/// not. The host hears each SetSelectionOwner that is taken, as often as an
/// XFIXES subscriber does: a client setting the owner twice at the same time
/// is two events for both, the earlier time none.
#[test]
fn the_host_hears_a_set_selection_owner_at_the_same_time_and_not_an_earlier_one() {
    let mut host = RecordingHost::new();
    backdate_clock(&mut host);
    host.connect(2, yserver_protocol::x11::ClientByteOrder::LittleEndian);
    host.create_window(1, WINDOW, ROOT_WINDOW.0, (0, 0), (10, 10), 0, false);
    host.create_window(2, OTHER, ROOT_WINDOW.0, (0, 0), (10, 10), 0, false);
    let clipboard = AtomId(host.atom("CLIPBOARD"));
    selection::watch_selection(&mut host.state, clipboard, true).expect("watch");
    // A second watch of the same selection is still one listener.
    selection::watch_selection(&mut host.state, clipboard, true).expect("watch again");
    host.request(2, 140, 0, &words(&[5, 0]));
    let out = host.request(2, 140, 2, &words(&[OTHER, clipboard.0, 7]));
    assert_no_error(&out, "SelectSelectionInput");
    let time = host.state.timestamp_now() - 5_000;
    for t in [time, time, time - 1] {
        let out = host.request(1, 22, 0, &words(&[WINDOW, clipboard.0, t]));
        assert_no_error(&out, "SetSelectionOwner");
    }
    let heard = selection::take_selection_events(&mut host.state);
    let owner = SelectionEvent::Owner {
        selection: clipboard,
        owner: Some(ResourceId(WINDOW)),
        time,
    };
    assert_eq!(
        heard,
        [owner, owner],
        "the same time twice, the earlier never"
    );
    let xfixes = host
        .read(2)
        .chunks(32)
        .filter(|e| e[0] & 0x7f == crate::nested::XFIXES_FIRST_EVENT)
        .count();
    assert_eq!(xfixes, heard.len(), "as many as the XFIXES subscriber");
}
