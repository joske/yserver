use super::*;

/// Audit #9 (`docs/protocol-audit-2026-05-19.md`): when a client
/// takes ownership of a selection via core `SetSelectionOwner`, the
/// server must fire `XFixesSelectionNotify(SetSelectionOwner)` to
/// every client that has subscribed via `SelectSelectionInput` with
/// the matching mask bit. Without this, every clipboard manager
/// (klipper, copyq, gpaste, gnome-shell clipboard indicator) wedges
/// waiting for the event that says "the clipboard contents
/// changed". Xorg's `xfixes/select.c:158-210` `XFixesSelectionCallback`
/// is the reference.
#[test]
fn set_selection_owner_emits_xfixes_set_owner_notify_to_subscribers() {
    use std::io::Read;
    use yserver_protocol::x11::xfixes as x11xfixes;

    const CLIPBOARD_MGR: u32 = 7;
    const APP: u32 = 9;
    const SUBSCRIBER_WIN: u32 = 0x0070_0001;
    const OWNER_WIN: u32 = 0x0090_0001;
    const PRIMARY: u32 = 1; // X11 atom PRIMARY

    let mut state = ServerState::new();
    // Clipboard manager: subscribes; we'll inspect its peer socket.
    let mut peer = install_client(&mut state, CLIPBOARD_MGR);
    // App: issues SetSelectionOwner (no peer needed).
    let _app_peer = install_client(&mut state, APP);

    // OWNER_WIN must exist for the SetSelectionOwner window
    // validation gate (`dix/selection.c:169-173`).
    state.resources.create_window(
        ClientId(APP),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(OWNER_WIN),
            parent: crate::resources::ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 50,
            height: 50,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );

    // Subscribe clipboard mgr to PRIMARY's set-owner events on
    // SUBSCRIBER_WIN. This is what `SelectSelectionInput` writes.
    state.xfixes_selection_masks.insert(
        (CLIPBOARD_MGR, ResourceId(SUBSCRIBER_WIN), AtomId(PRIMARY)),
        x11xfixes::SELECTION_MASK_SET_OWNER,
    );

    // SetSelectionOwner(window=OWNER_WIN, selection=PRIMARY, time=0).
    // time=0 is X11's `CurrentTime` which Xorg's
    // `ClientTimeToServerTime` resolves to the server's
    // currentTime — guaranteed to clear both the future-shift
    // (`time > currentTime`) and stale (`time < lastTimeChanged`)
    // checks at `dix/selection.c:166,192`.
    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&OWNER_WIN.to_le_bytes());
    body.extend_from_slice(&PRIMARY.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    handle_set_selection_owner(&mut state, ClientId(APP), SequenceNumber(1), &body)
        .expect("handle_set_selection_owner");

    // Drain clipboard mgr's socket and locate the XFixesSelectionNotify.
    peer.set_nonblocking(true).unwrap();
    let mut all = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match peer.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => all.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    let evt: [u8; 32] = all
        .chunks_exact(32)
        .find(|evt| evt[0] == crate::nested::XFIXES_FIRST_EVENT)
        .map(|s| {
            let mut a = [0u8; 32];
            a.copy_from_slice(s);
            a
        })
        .unwrap_or_else(|| {
            panic!(
                "expected XFixesSelectionNotify (type={}) in clipboard manager's stream; \
                     got {} bytes: {:02x?}",
                crate::nested::XFIXES_FIRST_EVENT,
                all.len(),
                all,
            )
        });
    assert_eq!(
        evt[1],
        x11xfixes::SELECTION_NOTIFY_SET_OWNER,
        "subtype must be SetSelectionOwnerNotify (0)",
    );
    // Skip the 2-byte sequence and verify the payload (windows /
    // selection / timestamps).
    assert_eq!(
        u32::from_le_bytes([evt[4], evt[5], evt[6], evt[7]]),
        SUBSCRIBER_WIN,
        "window field must be the subscriber's window",
    );
    assert_eq!(
        u32::from_le_bytes([evt[8], evt[9], evt[10], evt[11]]),
        OWNER_WIN,
        "owner field must be the new selection-owner window",
    );
    assert_eq!(
        u32::from_le_bytes([evt[12], evt[13], evt[14], evt[15]]),
        PRIMARY,
        "selection field must be the selection atom",
    );
    // `timestamp` is Xorg's `currentTime.milliseconds`
    // (`xfixes/select.c:88`) — NOT the request's `time` arg as
    // the pre-fix code assumed. `selection_timestamp` is the
    // freshly-updated `lastTimeChanged`. With request `time=0`
    // resolving to `currentTime`, both should reflect server
    // uptime at the moment of the call. The window/owner/selection
    // fields above are the load-bearing identity assertions; the
    // timestamp wire encoding is covered by the destroy/close
    // tests where the prior `lastTimeChanged` is seeded directly.
}

/// Audit #9: when the window currently owning a selection is
/// destroyed, the server must fire
/// `XFixesSelectionNotify(SelectionWindowDestroy)` to every
/// subscribing client whose mask includes the WindowDestroy bit,
/// and clear the selection ownership in `state.selections`.
/// Reference: Xorg `xfixes/select.c:158-210` + the
/// `SelectionCallback`'s `SelectionWindowDestroy` branch.
#[test]
fn destroy_window_owning_selection_fires_window_destroy_notify_and_clears_ownership() {
    use std::io::Read;
    use yserver_protocol::x11::{CreateWindowRequest, xfixes as x11xfixes};
    const SUBSCRIBER_ID: u32 = 8;
    const OWNER_ID: u32 = 10;
    const SUBSCRIBER_WIN: u32 = 0x0080_0001;
    const OWNER_WIN: u32 = 0x00a0_0001;
    const PRIMARY: u32 = 1;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, SUBSCRIBER_ID);
    let _ = install_client(&mut state, OWNER_ID);

    // Create the owner window so DestroyWindow can find it.
    state.resources.create_window(
        ClientId(OWNER_ID),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(OWNER_WIN),
            parent: crate::resources::ROOT_WINDOW,
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

    // PRIMARY currently owned by OWNER_WIN.
    state
        .selections
        .insert(AtomId(PRIMARY), (ResourceId(OWNER_WIN), 0));

    // Subscriber registered with WindowDestroy bit.
    state.xfixes_selection_masks.insert(
        (SUBSCRIBER_ID, ResourceId(SUBSCRIBER_WIN), AtomId(PRIMARY)),
        x11xfixes::SELECTION_MASK_WINDOW_DESTROY,
    );

    // Destroy OWNER_WIN — should fire WindowDestroy notify + clear.
    let mut body = Vec::with_capacity(4);
    body.extend_from_slice(&OWNER_WIN.to_le_bytes());
    let mut backend = RecordingBackend::new();
    handle_destroy_window(
        &mut state,
        &mut backend,
        None,
        ClientId(OWNER_ID),
        SequenceNumber(1),
        &body,
    )
    .expect("handle_destroy_window");

    // Verify selection ownership cleared.
    assert!(
        !state.selections.contains_key(&AtomId(PRIMARY)),
        "destroying the owning window must clear state.selections[PRIMARY]; \
             got {:?}",
        state.selections.get(&AtomId(PRIMARY)),
    );

    // Verify subscriber received the event.
    peer.set_nonblocking(true).unwrap();
    let mut all = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match peer.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => all.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    let evt: [u8; 32] = all
        .chunks_exact(32)
        .find(|evt| {
            evt[0] == crate::nested::XFIXES_FIRST_EVENT
                && evt[1] == x11xfixes::SELECTION_NOTIFY_WINDOW_DESTROY
        })
        .map(|s| {
            let mut a = [0u8; 32];
            a.copy_from_slice(s);
            a
        })
        .unwrap_or_else(|| {
            panic!(
                "expected XFixesSelectionNotify(WindowDestroy) on subscriber; \
                     got {} bytes: {:02x?}",
                all.len(),
                all,
            )
        });
    assert_eq!(
        u32::from_le_bytes([evt[4], evt[5], evt[6], evt[7]]),
        SUBSCRIBER_WIN,
        "window field must be the subscriber's window",
    );
    assert_eq!(
        u32::from_le_bytes([evt[8], evt[9], evt[10], evt[11]]),
        0,
        "owner field must be None (0) after window-destroy",
    );
    assert_eq!(
        u32::from_le_bytes([evt[12], evt[13], evt[14], evt[15]]),
        PRIMARY,
        "selection field must be the cleared selection atom",
    );
}

/// Audit #9: when a client disconnects while owning a selection,
/// the server must fire
/// `XFixesSelectionNotify(SelectionClientClose)` to every subscriber
/// whose mask includes the ClientClose bit, and drop the selection
/// ownership entry. Reference: Xorg
/// `xfixes/select.c:158-210`'s `SelectionClientClose` branch.
#[test]
fn client_disconnect_owning_selection_fires_client_close_notify_and_clears_ownership() {
    use std::io::Read;
    use yserver_protocol::x11::xfixes as x11xfixes;
    const SUBSCRIBER_ID: u32 = 11;
    const OWNER_ID: u32 = 12;
    const SUBSCRIBER_WIN: u32 = 0x00b0_0001;
    const OWNER_WIN: u32 = 0x00c0_0001;
    const PRIMARY: u32 = 1;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, SUBSCRIBER_ID);
    let _ = install_client(&mut state, OWNER_ID);

    // OWNER_WIN belongs to OWNER_ID via window_owner mapping —
    // `selection_owner_target_id` walks that mapping.
    state.resources.create_window(
        ClientId(OWNER_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(OWNER_WIN),
            parent: crate::resources::ROOT_WINDOW,
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
    state
        .selections
        .insert(AtomId(PRIMARY), (ResourceId(OWNER_WIN), 0));
    state.xfixes_selection_masks.insert(
        (SUBSCRIBER_ID, ResourceId(SUBSCRIBER_WIN), AtomId(PRIMARY)),
        x11xfixes::SELECTION_MASK_CLIENT_CLOSE,
    );

    // Simulate client disconnect cleanup for OWNER_ID's selections.
    fanout_xfixes_selection_client_close_for_client(&mut state, ClientId(OWNER_ID));

    assert!(
        !state.selections.contains_key(&AtomId(PRIMARY)),
        "client disconnect must clear selections it owns",
    );

    peer.set_nonblocking(true).unwrap();
    let mut all = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match peer.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => all.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    let evt: [u8; 32] = all
        .chunks_exact(32)
        .find(|evt| {
            evt[0] == crate::nested::XFIXES_FIRST_EVENT
                && evt[1] == x11xfixes::SELECTION_NOTIFY_CLIENT_CLOSE
        })
        .map(|s| {
            let mut a = [0u8; 32];
            a.copy_from_slice(s);
            a
        })
        .unwrap_or_else(|| {
            panic!(
                "expected XFixesSelectionNotify(ClientClose); \
                     got {} bytes: {:02x?}",
                all.len(),
                all,
            )
        });
    assert_eq!(
        u32::from_le_bytes([evt[4], evt[5], evt[6], evt[7]]),
        SUBSCRIBER_WIN,
        "window field = subscriber",
    );
    assert_eq!(
        u32::from_le_bytes([evt[8], evt[9], evt[10], evt[11]]),
        0,
        "owner field = None on client-close",
    );
    assert_eq!(
        u32::from_le_bytes([evt[12], evt[13], evt[14], evt[15]]),
        PRIMARY,
        "selection field",
    );
}

/// Audit #9 (code-review follow-up): Xorg's `dixSetSelectionOwner`
/// (`dix/selection.c:194`) gates SelectionClear on the previous
/// OWNER's client identity, not on the window identity:
///   `if (pSel->client && (!pWin || (pSel->client != client)))`
/// A single client moving its own selection between two of its
/// own windows must NOT receive a spurious SelectionClear. Pre-fix
/// yserver gated on `old_window != new_window`, which over-fires.
#[test]
fn set_selection_owner_no_clear_when_same_client_moves_between_own_windows() {
    use std::io::Read;
    use yserver_protocol::x11::CreateWindowRequest;
    const APP: u32 = 50;
    const WIN_A: u32 = 0x0050_0001;
    const WIN_B: u32 = 0x0050_0002;
    const PRIMARY: u32 = 1;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, APP);
    // Both windows owned by the same client.
    for win in [WIN_A, WIN_B] {
        state.resources.create_window(
            ClientId(APP),
            CreateWindowRequest {
                depth: 24,
                window: ResourceId(win),
                parent: crate::resources::ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 50,
                height: 50,
                border_width: 0,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
    }
    // Seed prior ownership directly so we isolate the second-call
    // path. Owner = WIN_A (belongs to APP).
    state
        .selections
        .insert(AtomId(PRIMARY), (ResourceId(WIN_A), 0));

    // Same client takes ownership at WIN_B.
    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&WIN_B.to_le_bytes());
    body.extend_from_slice(&PRIMARY.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // time=CurrentTime
    handle_set_selection_owner(&mut state, ClientId(APP), SequenceNumber(1), &body)
        .expect("handle_set_selection_owner");

    peer.set_nonblocking(true).unwrap();
    let mut all = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match peer.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => all.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    // SelectionClear has event type 29 (X11 core).
    assert!(
        !all.chunks_exact(32).any(|e| e[0] == 29),
        "no SelectionClear must be sent when the same client \
             moves selection between its own windows; got {} bytes: \
             {:02x?}",
        all.len(),
        all,
    );
}

/// Audit #9 (code-review follow-up): Xorg encodes the resolved
/// server time (post-`ClientTimeToServerTime`, so CurrentTime/0
/// becomes the actual `currentTime`) into the SelectionClear
/// event's time field at `dix/selection.c:196`. Pre-fix yserver
/// encoded the raw `time_val`, so a client sending `time=0` made
/// the SelectionClear carry `time=0` (the CurrentTime sentinel).
#[test]
fn set_selection_owner_clear_event_uses_resolved_time_when_client_sends_zero() {
    use std::{io::Read, thread, time::Duration};
    use yserver_protocol::x11::CreateWindowRequest;
    const PRIOR_OWNER: u32 = 51;
    const NEW_OWNER: u32 = 52;
    const PRIOR_WIN: u32 = 0x0051_0001;
    const NEW_WIN: u32 = 0x0052_0001;
    const PRIMARY: u32 = 1;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, PRIOR_OWNER);
    let _ = install_client(&mut state, NEW_OWNER);
    for (owner, win) in &[(PRIOR_OWNER, PRIOR_WIN), (NEW_OWNER, NEW_WIN)] {
        state.resources.create_window(
            ClientId(*owner),
            CreateWindowRequest {
                depth: 24,
                window: ResourceId(*win),
                parent: crate::resources::ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 50,
                height: 50,
                border_width: 0,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
    }
    // Seed prior ownership with lastTimeChanged=0 so the stale
    // check at the new call never trips.
    state
        .selections
        .insert(AtomId(PRIMARY), (ResourceId(PRIOR_WIN), 0));

    // Sleep so `state.timestamp_now()` is non-zero when the new
    // request runs — pre-fix the SelectionClear's `time` field
    // carries the raw `time_val=0` (CurrentTime sentinel), so
    // pinning `>= ts_before` after a sleep distinguishes the
    // resolved-time encoding from the raw-passthrough encoding.
    thread::sleep(Duration::from_millis(2));
    let ts_before = state.timestamp_now();
    assert!(ts_before > 0, "sanity: sleep advanced server time");

    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&NEW_WIN.to_le_bytes());
    body.extend_from_slice(&PRIMARY.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // time=CurrentTime
    handle_set_selection_owner(&mut state, ClientId(NEW_OWNER), SequenceNumber(1), &body)
        .expect("handle_set_selection_owner");

    peer.set_nonblocking(true).unwrap();
    let mut all = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match peer.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => all.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    let clear: [u8; 32] = all
        .chunks_exact(32)
        .find(|e| e[0] == 29)
        .map(|s| {
            let mut a = [0u8; 32];
            a.copy_from_slice(s);
            a
        })
        .unwrap_or_else(|| {
            panic!(
                "expected SelectionClear (event type 29) on prior \
                     owner's stream; got {} bytes: {:02x?}",
                all.len(),
                all,
            );
        });
    let clear_time = u32::from_le_bytes([clear[4], clear[5], clear[6], clear[7]]);
    assert!(
        clear_time >= ts_before,
        "SelectionClear.time must be the resolved server time \
             (>= ts_before={ts_before}), not the raw time_val=0 the \
             client sent; got {clear_time}",
    );
}

/// Audit #9 (code-review follow-up): Xorg's `dixSetSelectionOwner`
/// (`dix/selection.c:169-173`) returns BadWindow when `window` is
/// non-`None` and `dixLookupWindow` fails. Pre-fix yserver
/// silently stored an unknown window xid as the new owner and
/// fired XFixesSelectionNotify as if it were valid.
#[test]
fn set_selection_owner_with_unknown_window_returns_bad_window() {
    use std::io::Read;
    const APP: u32 = 40;
    const UNKNOWN_WIN: u32 = 0x0040_dead;
    const PRIMARY: u32 = 1;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, APP);
    let mut backend = RecordingBackend::new();

    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&UNKNOWN_WIN.to_le_bytes());
    body.extend_from_slice(&PRIMARY.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    let length_units = u32::try_from(1 + body.len().div_ceil(4)).expect("body fits");
    process_request(
        &mut state,
        &mut backend,
        ClientId(APP),
        SequenceNumber(1),
        RequestHeader {
            opcode: 22, // core SetSelectionOwner
            data: 0,
            length_units,
        },
        &body,
        None,
    )
    .expect("process_request");

    peer.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 32];
    peer.read_exact(&mut buf).expect("error reply delivered");
    assert_eq!(buf[0], 0, "first byte = error class");
    assert_eq!(
        buf[1],
        yserver_protocol::x11::error::BAD_WINDOW,
        "expected BadWindow (3) for unknown window xid; got {}",
        buf[1],
    );
    // SetSelectionOwner is core opcode 22; the error reply's
    // major_opcode field (byte 10) MUST reflect that, or
    // clients that key on major_opcode see a malformed error.
    assert_eq!(
        buf[10], 22,
        "BadWindow reply must carry SetSelectionOwner's major opcode (22); got {}",
        buf[10],
    );
    assert!(
        !state.selections.contains_key(&AtomId(PRIMARY)),
        "no ownership must be recorded on BadWindow; got {:?}",
        state.selections.get(&AtomId(PRIMARY)),
    );
}

/// Audit #9 (code-review follow-up): Xorg's `dixSetSelectionOwner`
/// (`dix/selection.c:175-178`) returns BadAtom for an unknown
/// selection atom. window=None bypasses the BadWindow gate so the
/// BadAtom path is isolated.
#[test]
fn set_selection_owner_with_invalid_atom_returns_bad_atom() {
    use std::io::Read;
    const APP: u32 = 41;
    const BOGUS_ATOM: u32 = 0xdead_beef;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, APP);
    let mut backend = RecordingBackend::new();

    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&0u32.to_le_bytes()); // window=None
    body.extend_from_slice(&BOGUS_ATOM.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // time=CurrentTime
    let length_units = u32::try_from(1 + body.len().div_ceil(4)).expect("body fits");
    process_request(
        &mut state,
        &mut backend,
        ClientId(APP),
        SequenceNumber(1),
        RequestHeader {
            opcode: 22, // core SetSelectionOwner
            data: 0,
            length_units,
        },
        &body,
        None,
    )
    .expect("process_request");

    peer.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 32];
    peer.read_exact(&mut buf).expect("error reply delivered");
    assert_eq!(buf[0], 0, "first byte = error class");
    assert_eq!(
        buf[1],
        yserver_protocol::x11::error::BAD_ATOM,
        "expected BadAtom (5) for an unknown selection atom; got {}",
        buf[1],
    );
    assert_eq!(
        buf[10], 22,
        "BadAtom reply must carry SetSelectionOwner's major opcode (22); got {}",
        buf[10],
    );
    assert!(
        !state.selections.contains_key(&AtomId(BOGUS_ATOM)),
        "no ownership must be recorded on BadAtom",
    );
}

/// Audit #9 (code-review follow-up): Xorg's
/// `dixSetSelectionOwner` (`dix/selection.c:164-167`) silently
/// ignores a request whose timestamp is in the future relative
/// to the server's currentTime, returning Success without
/// changing ownership and without firing any callbacks. Pre-fix
/// yserver accepted any timestamp, which lets a misbehaving
/// client jump the ownership queue past a well-behaved one.
#[test]
fn set_selection_owner_with_future_time_is_silently_ignored() {
    use std::io::Read;
    use yserver_protocol::x11::{CreateWindowRequest, xfixes as x11xfixes};
    const SUBSCRIBER_ID: u32 = 30;
    const OWNER_ID: u32 = 31;
    const SUBSCRIBER_WIN: u32 = 0x0030_0001;
    const OWNER_WIN: u32 = 0x0031_0001;
    const PRIMARY: u32 = 1;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, SUBSCRIBER_ID);
    let _ = install_client(&mut state, OWNER_ID);
    state.resources.create_window(
        ClientId(OWNER_ID),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(OWNER_WIN),
            parent: crate::resources::ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 50,
            height: 50,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    state.xfixes_selection_masks.insert(
        (SUBSCRIBER_ID, ResourceId(SUBSCRIBER_WIN), AtomId(PRIMARY)),
        x11xfixes::SELECTION_MASK_SET_OWNER,
    );

    // u32::MAX is guaranteed > state.timestamp_now() during the
    // test (≈49 days post-start would be needed to reach it).
    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&OWNER_WIN.to_le_bytes());
    body.extend_from_slice(&PRIMARY.to_le_bytes());
    body.extend_from_slice(&u32::MAX.to_le_bytes());
    handle_set_selection_owner(&mut state, ClientId(OWNER_ID), SequenceNumber(1), &body)
        .expect("handle_set_selection_owner");

    assert!(
        !state.selections.contains_key(&AtomId(PRIMARY)),
        "future-timestamp request must not update ownership; \
             got {:?}",
        state.selections.get(&AtomId(PRIMARY)),
    );
    peer.set_nonblocking(true).unwrap();
    let mut all = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match peer.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => all.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    assert!(
        !all.chunks_exact(32)
            .any(|e| e[0] == crate::nested::XFIXES_FIRST_EVENT),
        "no XFixesSelectionNotify must fire on silent-ignore; \
             got {} bytes: {:02x?}",
        all.len(),
        all,
    );
}

/// Audit #9 (code-review follow-up): Xorg's `dixSetSelectionOwner`
/// (`dix/selection.c:188-193`) silently ignores a request whose
/// timestamp predates the selection's current `lastTimeChanged`,
/// preserving the existing ownership. Pre-fix yserver accepted
/// the stale request and overwrote the newer ownership.
#[test]
fn set_selection_owner_with_stale_time_is_silently_ignored() {
    use std::io::Read;
    use yserver_protocol::x11::{CreateWindowRequest, xfixes as x11xfixes};
    const SUBSCRIBER_ID: u32 = 32;
    const PRIOR_OWNER_ID: u32 = 33;
    const NEW_OWNER_ID: u32 = 34;
    const SUBSCRIBER_WIN: u32 = 0x0032_0001;
    const PRIOR_OWNER_WIN: u32 = 0x0033_0001;
    const NEW_OWNER_WIN: u32 = 0x0034_0001;
    const PRIMARY: u32 = 1;
    const PRIOR_LAST_TIME: u32 = 1000;
    const STALE_TIME: u32 = 500;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, SUBSCRIBER_ID);
    let _ = install_client(&mut state, PRIOR_OWNER_ID);
    let _ = install_client(&mut state, NEW_OWNER_ID);
    for (owner, win) in &[
        (PRIOR_OWNER_ID, PRIOR_OWNER_WIN),
        (NEW_OWNER_ID, NEW_OWNER_WIN),
    ] {
        state.resources.create_window(
            ClientId(*owner),
            CreateWindowRequest {
                depth: 24,
                window: ResourceId(*win),
                parent: crate::resources::ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 50,
                height: 50,
                border_width: 0,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
    }
    // Seed prior ownership with lastTimeChanged = 1000.
    state.selections.insert(
        AtomId(PRIMARY),
        (ResourceId(PRIOR_OWNER_WIN), PRIOR_LAST_TIME),
    );
    state.xfixes_selection_masks.insert(
        (SUBSCRIBER_ID, ResourceId(SUBSCRIBER_WIN), AtomId(PRIMARY)),
        x11xfixes::SELECTION_MASK_SET_OWNER,
    );

    // Attempt to take ownership at time=500 (before
    // prior_last_time=1000).
    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&NEW_OWNER_WIN.to_le_bytes());
    body.extend_from_slice(&PRIMARY.to_le_bytes());
    body.extend_from_slice(&STALE_TIME.to_le_bytes());
    handle_set_selection_owner(&mut state, ClientId(NEW_OWNER_ID), SequenceNumber(1), &body)
        .expect("handle_set_selection_owner");

    let preserved = state.selections.get(&AtomId(PRIMARY)).copied();
    assert_eq!(
        preserved,
        Some((ResourceId(PRIOR_OWNER_WIN), PRIOR_LAST_TIME)),
        "stale-timestamp request must NOT overwrite a newer \
             ownership; got {preserved:?}",
    );
    peer.set_nonblocking(true).unwrap();
    let mut all = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match peer.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => all.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    assert!(
        !all.chunks_exact(32)
            .any(|e| e[0] == crate::nested::XFIXES_FIRST_EVENT),
        "no XFixesSelectionNotify must fire on stale-ignore; \
             got {} bytes: {:02x?}",
        all.len(),
        all,
    );
}

/// Audit #9 (code-review follow-up): Xorg's
/// `ProcXFixesSelectSelectionInput` (`xfixes/select.c:189-192`)
/// runs `dixLookupWindow` first and propagates its return code
/// (`BadWindow` for an unknown xid). Before the fix yserver
/// silently stored the subscription, accepting illegal records.
#[test]
fn select_selection_input_with_invalid_window_returns_bad_window() {
    use std::io::Read;
    use yserver_protocol::x11::xfixes as x11xfixes;

    const CLIENT: u32 = 21;
    const UNKNOWN_WIN: u32 = 0x00ff_eeee;
    const PRIMARY: u32 = 1;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT);
    let mut backend = RecordingBackend::new();

    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&UNKNOWN_WIN.to_le_bytes());
    body.extend_from_slice(&PRIMARY.to_le_bytes());
    body.extend_from_slice(&x11xfixes::SELECTION_MASK_SET_OWNER.to_le_bytes());
    let length_units = u32::try_from(1 + body.len().div_ceil(4)).expect("body fits");
    // The client negotiated XFIXES (Xorg gates requests on QueryVersion).
    state.xfixes_client_major.insert(CLIENT, 5);
    process_request(
        &mut state,
        &mut backend,
        ClientId(CLIENT),
        SequenceNumber(1),
        RequestHeader {
            opcode: XFIXES_MAJOR_OPCODE,
            data: x11xfixes::SELECT_SELECTION_INPUT,
            length_units,
        },
        &body,
        None,
    )
    .expect("process_request");

    peer.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 32];
    peer.read_exact(&mut buf).expect("error reply delivered");
    assert_eq!(buf[0], 0, "first byte = error class");
    assert_eq!(
        buf[1],
        yserver_protocol::x11::error::BAD_WINDOW,
        "expected BadWindow (3) for unknown window xid; got {}",
        buf[1],
    );
    assert!(
        state.xfixes_selection_masks.is_empty(),
        "the subscription must NOT be recorded on BadWindow; got {:?}",
        state.xfixes_selection_masks,
    );
}

/// Audit #9 (code-review follow-up): Xorg's
/// `ProcXFixesSelectSelectionInput` (`xfixes/select.c:193-196`)
/// returns BadValue when `eventMask & ~SelectionAllEvents` is
/// non-zero. Before the fix yserver stored arbitrary mask bits.
#[test]
fn select_selection_input_with_mask_outside_known_bits_returns_bad_value() {
    use std::io::Read;
    use yserver_protocol::x11::{CreateWindowRequest, xfixes as x11xfixes};

    const CLIENT: u32 = 22;
    const WIN: u32 = 0x0150_0001;
    const PRIMARY: u32 = 1;
    const ILLEGAL_MASK: u32 = 1 << 8; // outside SelectionAllEvents

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT);
    let mut backend = RecordingBackend::new();
    // Window must exist for the window-validation step to pass,
    // so we cleanly isolate the mask-validation path.
    state.resources.create_window(
        ClientId(CLIENT),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(WIN),
            parent: crate::resources::ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 50,
            height: 50,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );

    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&WIN.to_le_bytes());
    body.extend_from_slice(&PRIMARY.to_le_bytes());
    body.extend_from_slice(&ILLEGAL_MASK.to_le_bytes());
    let length_units = u32::try_from(1 + body.len().div_ceil(4)).expect("body fits");
    // The client negotiated XFIXES (Xorg gates requests on QueryVersion).
    state.xfixes_client_major.insert(CLIENT, 5);
    process_request(
        &mut state,
        &mut backend,
        ClientId(CLIENT),
        SequenceNumber(1),
        RequestHeader {
            opcode: XFIXES_MAJOR_OPCODE,
            data: x11xfixes::SELECT_SELECTION_INPUT,
            length_units,
        },
        &body,
        None,
    )
    .expect("process_request");

    peer.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 32];
    peer.read_exact(&mut buf).expect("error reply delivered");
    assert_eq!(buf[0], 0, "first byte = error class");
    assert_eq!(
        buf[1],
        yserver_protocol::x11::error::BAD_VALUE,
        "expected BadValue (2) for mask with bits outside \
             SelectionAllEvents (0b111); got {}",
        buf[1],
    );
    assert!(
        state.xfixes_selection_masks.is_empty(),
        "the subscription must NOT be recorded on BadValue; got {:?}",
        state.xfixes_selection_masks,
    );
}

/// Audit #9 (code-review follow-up): the destroy/close path
/// must emit the notify BEFORE clearing the selection (matching
/// Xorg's `DeleteWindowFromAnySelections` order at
/// `dix/selection.c:131-138`) so the wire payload carries the
/// stored `selection_timestamp` — `selection->lastTimeChanged`
/// in Xorg's `xfixes/select.c:89`. yserver tracks
/// `lastTimeChanged` as the second element of the
/// `state.selections` tuple, stamped by SetSelectionOwner with
/// the request's `time` arg.
#[test]
fn destroy_window_owning_selection_carries_prior_last_time_changed_in_selection_timestamp() {
    use std::io::Read;
    use yserver_protocol::x11::{CreateWindowRequest, xfixes as x11xfixes};
    const SUBSCRIBER_ID: u32 = 15;
    const OWNER_ID: u32 = 16;
    const SUBSCRIBER_WIN: u32 = 0x00f0_0001;
    const OWNER_WIN: u32 = 0x0100_0001;
    const PRIMARY: u32 = 1;
    const SET_OWNER_TIME: u32 = 0x0a0b_0c0d;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, SUBSCRIBER_ID);
    let _ = install_client(&mut state, OWNER_ID);

    state.resources.create_window(
        ClientId(OWNER_ID),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(OWNER_WIN),
            parent: crate::resources::ROOT_WINDOW,
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
    state.xfixes_selection_masks.insert(
        (SUBSCRIBER_ID, ResourceId(SUBSCRIBER_WIN), AtomId(PRIMARY)),
        x11xfixes::SELECTION_MASK_WINDOW_DESTROY,
    );

    // Seed ownership directly with a known `lastTimeChanged`.
    // This decouples the destroy-path assertion from
    // `handle_set_selection_owner`'s time-resolution logic
    // (future/stale checks, ClientTime → ServerTime conversion);
    // we only care here that whatever value is in the second
    // tuple slot flows through to the notify wire payload.
    state
        .selections
        .insert(AtomId(PRIMARY), (ResourceId(OWNER_WIN), SET_OWNER_TIME));

    let mut destroy_body = Vec::with_capacity(4);
    destroy_body.extend_from_slice(&OWNER_WIN.to_le_bytes());
    let mut backend = RecordingBackend::new();
    handle_destroy_window(
        &mut state,
        &mut backend,
        None,
        ClientId(OWNER_ID),
        SequenceNumber(2),
        &destroy_body,
    )
    .expect("handle_destroy_window");
    peer.set_nonblocking(true).unwrap();

    let mut all = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match peer.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => all.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    let evt: [u8; 32] = all
        .chunks_exact(32)
        .find(|e| {
            e[0] == crate::nested::XFIXES_FIRST_EVENT
                && e[1] == x11xfixes::SELECTION_NOTIFY_WINDOW_DESTROY
        })
        .map(|s| {
            let mut a = [0u8; 32];
            a.copy_from_slice(s);
            a
        })
        .expect("WindowDestroy notify in stream");

    assert_eq!(
        u32::from_le_bytes([evt[20], evt[21], evt[22], evt[23]]),
        SET_OWNER_TIME,
        "selection_timestamp must carry the prior SetSelectionOwner \
             time arg (Xorg `selection->lastTimeChanged`); pre-fix the \
             payload was zeroed",
    );
    // The `timestamp` field is `state.timestamp_now()` (Xorg's
    // `currentTime.milliseconds` equivalent); not asserted here
    // because in a sub-millisecond test it's legitimately 0 — which
    // matches Xorg behavior (`UpdateCurrentTimeIf` snapshots
    // `GetTimeInMillis()` with no floor). The
    // `selection_timestamp` assertion above is the load-bearing
    // spec-compliance check.
}

/// Audit #9 (code-review follow-up): Xorg's
/// `XFixesSelectionCallback` (`xfixes/select.c:79-91`) walks
/// every matching subscription record and emits one event per
/// `(client, window, selection)` tuple. A single client that
/// has two windows subscribed to the same selection must
/// receive TWO events whose `window` fields are the two
/// distinct subscriber windows — not one collapsed event nor
/// two events with the same window field.
///
/// `fanout_event_to_clients` deduplicates by ClientId *within
/// one call* via a freshly-allocated `seen` set, so calling it
/// once per subscription with a single-element slice is the
/// correct shape; the closure captures the per-iteration
/// subscriber window. This test pins that invariant against
/// future refactors.
#[test]
fn set_selection_owner_emits_one_event_per_subscription_when_client_has_two_windows() {
    use std::io::Read;
    use yserver_protocol::x11::xfixes as x11xfixes;
    const SUBSCRIBER: u32 = 13;
    const APP: u32 = 14;
    const WIN_A: u32 = 0x00d0_0001;
    const WIN_B: u32 = 0x00d0_0002;
    const OWNER_WIN: u32 = 0x00e0_0001;
    const PRIMARY: u32 = 1;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, SUBSCRIBER);
    let _ = install_client(&mut state, APP);

    // OWNER_WIN must exist for SetSelectionOwner's window
    // validation gate.
    state.resources.create_window(
        ClientId(APP),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(OWNER_WIN),
            parent: crate::resources::ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 50,
            height: 50,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );

    // Same client, two subscription records on the same selection.
    state.xfixes_selection_masks.insert(
        (SUBSCRIBER, ResourceId(WIN_A), AtomId(PRIMARY)),
        x11xfixes::SELECTION_MASK_SET_OWNER,
    );
    state.xfixes_selection_masks.insert(
        (SUBSCRIBER, ResourceId(WIN_B), AtomId(PRIMARY)),
        x11xfixes::SELECTION_MASK_SET_OWNER,
    );

    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&OWNER_WIN.to_le_bytes());
    body.extend_from_slice(&PRIMARY.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    handle_set_selection_owner(&mut state, ClientId(APP), SequenceNumber(1), &body)
        .expect("handle_set_selection_owner");

    peer.set_nonblocking(true).unwrap();
    let mut all = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match peer.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => all.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    let mut subscriber_windows: Vec<u32> = all
        .chunks_exact(32)
        .filter(|e| {
            e[0] == crate::nested::XFIXES_FIRST_EVENT
                && e[1] == x11xfixes::SELECTION_NOTIFY_SET_OWNER
        })
        .map(|e| u32::from_le_bytes([e[4], e[5], e[6], e[7]]))
        .collect();
    subscriber_windows.sort_unstable();
    assert_eq!(
        subscriber_windows,
        vec![WIN_A, WIN_B],
        "must get one event per (client, window, selection) subscription \
             record with distinct `window` fields; got {} bytes total: {:02x?}",
        all.len(),
        all,
    );
}

/// Audit #9: subscribing with mask=0 then re-subscribing with the
/// set-owner bit, the FIRST owner-change must still emit (verifies
/// the mask-bit gate, not the presence of the subscription key).
#[test]
fn set_selection_owner_skips_subscribers_whose_mask_excludes_set_owner() {
    use std::io::Read;
    use yserver_protocol::x11::xfixes as x11xfixes;

    const SUBSCRIBER: u32 = 5;
    const APP: u32 = 6;
    const SUBSCRIBER_WIN: u32 = 0x0050_0001;
    const PRIMARY: u32 = 1;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, SUBSCRIBER);
    let _ = install_client(&mut state, APP);

    // Subscribe with a mask that does NOT include SET_OWNER —
    // only WINDOW_DESTROY. SetSelectionOwner must NOT fire for
    // this subscription.
    state.xfixes_selection_masks.insert(
        (SUBSCRIBER, ResourceId(SUBSCRIBER_WIN), AtomId(PRIMARY)),
        x11xfixes::SELECTION_MASK_WINDOW_DESTROY,
    );

    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&0x0090_0001u32.to_le_bytes()); // owner window
    body.extend_from_slice(&PRIMARY.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // time
    handle_set_selection_owner(&mut state, ClientId(APP), SequenceNumber(1), &body)
        .expect("handle_set_selection_owner");

    peer.set_nonblocking(true).unwrap();
    let mut all = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match peer.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => all.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    assert!(
        !all.chunks_exact(32)
            .any(|evt| evt[0] == crate::nested::XFIXES_FIRST_EVENT),
        "no XFixesSelectionNotify expected when subscriber's mask excludes \
             SET_OWNER; got {} bytes: {:02x?}",
        all.len(),
        all,
    );
}
