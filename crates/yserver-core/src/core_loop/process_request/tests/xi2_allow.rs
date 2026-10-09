use super::*;

/// `XIPassiveGrabDevice` with grab_type=Button(0) installs entries
/// in `state.button_grabs`; grab_type=Keycode(1) installs entries
/// in `state.key_grabs`. One entry per modifier (or one entry with
/// modifiers=0 when num_modifiers=0). `XIPassiveUngrabDevice`
/// removes matching entries.
#[test]
fn xi_passive_grab_device_pushes_button_and_key_grabs() {
    const CLIENT_ID: u32 = 1;
    const WINDOW_XID: u32 = 0x0010_0051;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, CLIENT_ID);
    let mut backend = RecordingBackend::new();

    // Helper to build XIPassiveGrabDevice body per
    // `xXIPassiveGrabDeviceReq` in X11/extensions/XI2proto.h:
    // time, grab_window, cursor, detail, deviceid, num_modifiers,
    // mask_len, grab_type, grab_mode, paired_device_mode,
    // owner_events, pad1, mask, modifiers.
    fn build_body(
        window: u32,
        detail: u32,
        device_id: u16,
        grab_type: u8,
        modifiers: &[u32],
    ) -> Vec<u8> {
        #[allow(clippy::cast_possible_truncation)]
        let num_modifiers = modifiers.len() as u16;
        let mut b = Vec::with_capacity(28 + modifiers.len() * 4);
        b.extend_from_slice(&0u32.to_le_bytes()); // time
        b.extend_from_slice(&window.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes()); // cursor
        b.extend_from_slice(&detail.to_le_bytes());
        b.extend_from_slice(&device_id.to_le_bytes());
        b.extend_from_slice(&num_modifiers.to_le_bytes());
        b.extend_from_slice(&0u16.to_le_bytes()); // mask_len
        b.push(grab_type);
        b.push(1); // grab_mode async
        b.push(1); // paired async
        b.push(0); // owner_events
        b.extend_from_slice(&0u16.to_le_bytes()); // pad1
        for m in modifiers {
            b.extend_from_slice(&m.to_le_bytes());
        }
        b
    }
    let header = yserver_protocol::x11::RequestHeader {
        opcode: 131,
        data: 54,
        // XIPassiveGrabDevice is AtLeast(8) and has no exact-length
        // gate (modifier count is read from the body's num_modifiers
        // field, not derived from the request length). 8 is the spec
        // minimum; was 6, which pre-dated the REQUEST_SIZE_MATCH gate
        // and now BadLengths before the handler runs.
        length_units: 8,
    };

    // Button grab on button 3 with modifiers [0, ControlMask(4)].
    let body = build_body(
        WINDOW_XID,
        3,
        crate::xinput::DEVICEID_MASTER_POINTER,
        0,
        &[0, 4],
    );
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(CLIENT_ID),
        SequenceNumber(1),
        header,
        &body,
    )
    .expect("button grab");
    assert_eq!(state.button_grabs.len(), 2, "one grab per modifier");
    assert!(
        state
            .button_grabs
            .iter()
            .any(|g| g.button == 3 && g.modifiers == 0)
    );
    assert!(
        state
            .button_grabs
            .iter()
            .any(|g| g.button == 3 && g.modifiers == 4)
    );

    // Key grab on keycode 67 (F1) with no modifiers → single entry
    // with modifier-mask 0.
    let body = build_body(
        WINDOW_XID,
        67,
        crate::xinput::DEVICEID_MASTER_KEYBOARD,
        1,
        &[],
    );
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(CLIENT_ID),
        SequenceNumber(2),
        header,
        &body,
    )
    .expect("key grab");
    assert_eq!(state.key_grabs.len(), 1);
    assert_eq!(state.key_grabs[0].keycode, 67);
    assert_eq!(state.key_grabs[0].modifiers, 0);

    // XI2 "Any" modifier (bit 31) → core X11 AnyModifier (0x8000).
    let body = build_body(
        WINDOW_XID,
        1,
        crate::xinput::DEVICEID_MASTER_POINTER,
        0,
        &[0x8000_0000],
    );
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(CLIENT_ID),
        SequenceNumber(3),
        header,
        &body,
    )
    .expect("any-modifier grab");
    assert!(
        state
            .button_grabs
            .iter()
            .any(|g| g.button == 1 && g.modifiers == 0x8000),
        "XI2 Any (bit 31) maps to core X11 AnyModifier 0x8000",
    );

    // XIPassiveUngrabDevice — body: window(4) detail(4) deviceid(2)
    // num_modifiers(2) grab_type(1) pad(3) modifiers.
    fn build_ungrab_body(
        window: u32,
        detail: u32,
        device_id: u16,
        grab_type: u8,
        modifiers: &[u32],
    ) -> Vec<u8> {
        #[allow(clippy::cast_possible_truncation)]
        let num_modifiers = modifiers.len() as u16;
        let mut b = Vec::with_capacity(16 + modifiers.len() * 4);
        b.extend_from_slice(&window.to_le_bytes());
        b.extend_from_slice(&detail.to_le_bytes());
        b.extend_from_slice(&device_id.to_le_bytes());
        b.extend_from_slice(&num_modifiers.to_le_bytes());
        b.push(grab_type);
        b.extend_from_slice(&[0u8; 3]);
        for m in modifiers {
            b.extend_from_slice(&m.to_le_bytes());
        }
        b
    }
    let ungrab_header = yserver_protocol::x11::RequestHeader {
        opcode: 131,
        data: 55,
        // XIPassiveUngrabDevice is AtLeast(5), no exact gate. 5 is the
        // spec minimum; was 4 (pre-gate placeholder → BadLength now).
        length_units: 5,
    };
    let body = build_ungrab_body(
        WINDOW_XID,
        3,
        crate::xinput::DEVICEID_MASTER_POINTER,
        0,
        &[0, 4],
    );
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(CLIENT_ID),
        SequenceNumber(4),
        ungrab_header,
        &body,
    )
    .expect("button ungrab");
    assert!(
        !state
            .button_grabs
            .iter()
            .any(|g| g.button == 3 && (g.modifiers == 0 || g.modifiers == 4)),
        "ungrab removes the matching button+modifier entries",
    );
    // AnyModifier grab still present (we didn't ungrab it).
    assert!(
        state
            .button_grabs
            .iter()
            .any(|g| g.button == 1 && g.modifiers == 0x8000)
    );
}

/// Regression test for the XIPassiveGrabDevice field-offset bug.
/// `mask_len` and `grab_type` were swapped: a real client (muffin,
/// xfwm4) sending mask_len=2 + grab_type=Keycode(1) was mis-parsed
/// as grab_type=Enter(2), silently dropping every keyboard
/// accelerator. The body here matches `xXIPassiveGrabDeviceReq`
/// in `X11/extensions/XI2proto.h` byte-for-byte with mask_len=2,
/// which is what real WM clients send.
#[test]
fn xi_passive_grab_device_keycode_with_xi2_mask() {
    const CLIENT_ID: u32 = 1;
    const WINDOW_XID: u32 = 0x0010_0051;
    const KEYCODE: u32 = 116;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, CLIENT_ID);
    let mut backend = RecordingBackend::new();

    // Spec-shaped body with mask_len=2 (two 4-byte mask words,
    // matching what muffin emits for XI device-event masks).
    let mut body = Vec::with_capacity(28 + 8 + 4);
    body.extend_from_slice(&0u32.to_le_bytes()); // time
    body.extend_from_slice(&WINDOW_XID.to_le_bytes()); // grab_window
    body.extend_from_slice(&0u32.to_le_bytes()); // cursor
    body.extend_from_slice(&KEYCODE.to_le_bytes()); // detail
    body.extend_from_slice(&3u16.to_le_bytes()); // deviceid (keyboard)
    body.extend_from_slice(&1u16.to_le_bytes()); // num_modifiers
    body.extend_from_slice(&2u16.to_le_bytes()); // mask_len = 2
    body.push(1); // grab_type = Keycode
    body.push(1); // grab_mode async
    body.push(1); // paired async
    body.push(0); // owner_events
    body.extend_from_slice(&0u16.to_le_bytes()); // pad1
    body.extend_from_slice(&[0u8; 8]); // mask (2 words)
    body.extend_from_slice(&0u32.to_le_bytes()); // modifier 0

    let header = yserver_protocol::x11::RequestHeader {
        opcode: 131,
        data: 54,
        length_units: 10,
    };
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(CLIENT_ID),
        SequenceNumber(1),
        header,
        &body,
    )
    .expect("keycode grab with mask");

    assert_eq!(
        state.key_grabs.len(),
        1,
        "Keycode grab must be recorded — bug was misclassifying \
             as grab_type=Enter due to swapped mask_len/grab_type"
    );
    #[allow(clippy::cast_possible_truncation)]
    let expected_kc = KEYCODE as u8;
    assert_eq!(state.key_grabs[0].keycode, expected_kc);
    assert_eq!(state.key_grabs[0].modifiers, 0);
    assert!(state.button_grabs.is_empty());
}

/// `XIAllowEvents` with `mode=AsyncDevice(0)` releases the
/// passive grab on the calling client's device, matching what
/// mutter/cinnamon shell call after their click-to-focus button
/// grab activates. Before this handler shipped, the call was a
/// debug-logged stub so `state.pointer_grab` stayed pinned until
/// the next ButtonRelease, leaking the grab across intermediate
/// motion events.
#[test]
fn xi_allow_events_async_device_releases_passive_pointer_grab() {
    const CLIENT_ID: u32 = 1;
    const GRAB_WIN: u32 = 0x0010_0051;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, CLIENT_ID);
    let mut backend = RecordingBackend::new();

    // Seed a SYNC passive grab that has activated: the grab is held,
    // the device frozen, and the activating press stored — exactly the
    // state the pointer-fanout Step-3 path leaves after a matched click.
    set_test_pointer_grab(&mut state, CLIENT_ID, GRAB_WIN, true, false);
    state.xi1_frozen.insert(
        crate::xinput::DEVICEID_MASTER_POINTER,
        crate::server::Xi1Freeze {
            state: crate::server::Xi1SyncState::FrozenNoEvent,
            ..Default::default()
        },
    );

    // Body per `xXIAllowEventsReq`: time(4) + deviceid(2) +
    // mode(1) + pad(1). deviceid=2 (master pointer), mode=0
    // (AsyncDevice).
    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&0u32.to_le_bytes()); // time
    body.extend_from_slice(&2u16.to_le_bytes()); // deviceid
    body.push(0); // mode = AsyncDevice
    body.push(0); // pad

    let header = yserver_protocol::x11::RequestHeader {
        opcode: 131,
        data: 53,
        length_units: 3,
    };
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(CLIENT_ID),
        SequenceNumber(1),
        header,
        &body,
    )
    .expect("allow events");

    // AsyncDevice thaws the frozen pointer so events flow again (Xorg
    // AsyncPointer). Per Xorg the passive grab itself persists until the
    // button is released — what matters is the device is no longer
    // frozen (the freeze gate would otherwise queue every later event).
    assert!(
        !state
            .xi1_frozen
            .get(&crate::xinput::DEVICEID_MASTER_POINTER)
            .is_some_and(crate::server::Xi1Freeze::frozen),
        "AsyncDevice must thaw the frozen pointer device"
    );
}

#[test]
fn xi_allow_events_replay_device_replays_frozen_button_press_to_target() {
    use crate::{
        backend::Backend,
        host_x11::{HostPointerEvent, PointerEventKind},
        resources::ROOT_VISUAL,
    };

    const GRAB_CLIENT_ID: u32 = 1;
    const TARGET_CLIENT_ID: u32 = 2;
    const GRAB_WIN: u32 = 0x0010_0051;
    const TARGET_WIN: u32 = 0x0020_0052;
    const HOST_XID: u32 = 0xCAFE_0001;

    let mut state = ServerState::new();
    let mut grab_peer = install_client(&mut state, GRAB_CLIENT_ID);
    let mut target_peer = install_client(&mut state, TARGET_CLIENT_ID);
    let mut backend = RecordingBackend::new();
    grab_peer
        .set_nonblocking(true)
        .expect("grab peer nonblocking");
    target_peer
        .set_nonblocking(true)
        .expect("target peer nonblocking");

    state.resources.create_window(
        ClientId(GRAB_CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(GRAB_WIN),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            border_width: 0,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default()
        },
    );
    state.resources.create_window(
        ClientId(TARGET_CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(TARGET_WIN),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            border_width: 0,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(GRAB_WIN));
    let _ = state.resources.map_window(ResourceId(TARGET_WIN));
    state
        .clients
        .get_mut(&TARGET_CLIENT_ID)
        .expect("target client")
        .event_masks
        .insert(ResourceId(TARGET_WIN), 0x0000_0004);

    set_test_pointer_grab(&mut state, GRAB_CLIENT_ID, GRAB_WIN, true, false);
    {
        let f = state
            .xi1_frozen
            .entry(crate::xinput::DEVICEID_MASTER_POINTER)
            .or_default();
        f.state = crate::server::Xi1SyncState::FrozenWithEvent;
        f.stored = Some(crate::server::QueuedInputEvent::HostPointer(
            HostPointerEvent {
                origin: crate::core_loop::message::InputOrigin::XTest(4),
                kind: PointerEventKind::ButtonPress,
                host_xid: HOST_XID,
                detail: 1,
                time: 0,
                root_x: 10,
                root_y: 10,
                event_x: 10,
                event_y: 10,
                state: 0,
                crossing_mode: 0,
                child: 0,
                raw_dx: 0,
                raw_dy: 0,
                tree_change: false,
            },
        ));
    }
    Backend::register_top_level(&mut backend, None, ResourceId(TARGET_WIN), HOST_XID)
        .expect("register host xid");

    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&0u32.to_le_bytes()); // time
    body.extend_from_slice(&2u16.to_le_bytes()); // deviceid
    body.push(2); // mode = ReplayDevice
    body.push(0); // pad

    let header = yserver_protocol::x11::RequestHeader {
        opcode: 131,
        data: 53,
        length_units: 3,
    };
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(GRAB_CLIENT_ID),
        SequenceNumber(1),
        header,
        &body,
    )
    .expect("allow events");

    assert_eq!(
        state
            .active_pointer_grab
            .map(|grab| (grab.owner, grab.grab_window)),
        Some((ClientId(TARGET_CLIENT_ID), ResourceId(TARGET_WIN))),
        "the replayed press must install the implicit grab on the natural \
             recipient (#94 — Xorg ActivateImplicitGrab on the replayed delivery)"
    );
    assert!(
        state
            .active_pointer_grab
            .is_some_and(|g| g.implicit && !g.via_xi2),
        "replayed core press installs a core-form implicit grab"
    );
    assert!(
        state
            .xi1_frozen
            .get(&crate::xinput::DEVICEID_MASTER_POINTER)
            .and_then(|f| f.stored.as_ref())
            .is_none()
    );

    let mut buf = [0u8; 32];
    let grab_read = grab_peer.read(&mut buf);
    assert!(
        matches!(grab_read, Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock),
        "grab owner must not receive replayed ButtonPress; got {grab_read:?}",
    );
    let target_read = target_peer.read(&mut buf);
    assert!(
        matches!(target_read, Ok(32)),
        "target window subscriber should receive replayed ButtonPress; got {target_read:?}",
    );
    assert_eq!(buf[0], 4, "event type should be ButtonPress");
    assert_eq!(&buf[12..16], &TARGET_WIN.to_le_bytes());
}

/// Regression for the MATE submenu-hover bug. mate-panel holds an
/// active XI2 grab on its main panel window with `owner_events=true`;
/// menu items it pops up are top-level override-redirect windows
/// OWNED BY mate-panel (siblings of the grab window, not
/// descendants). Pre-fix the active-grab-redirect check used pure
/// topology (`target == grab_window || is_descendant_of`),
/// which redirected motion on the menu items to the grab window
/// with grab-relative coords — GTK couldn't track which menu item
/// the cursor was over, hover-delay never fired the submenu.
/// The fix adds an ownership-aware path: window owned by the grab
/// client qualifies for natural delivery alongside descendants.
/// Note: this is a heuristic — the fully spec-correct rule is "the
/// event would normally be reported to the grab client" (a mask-
/// subscription check). Ownership is a close proxy: clients almost
/// always select events on windows they own.
#[test]
fn active_grab_owner_events_natural_delivery_to_sibling_window_owned_by_grab_client() {
    use crate::{
        backend::Backend,
        core_loop::pointer_fanout::pointer_event_fanout_to_state,
        host_x11::{HostPointerEvent, PointerEventKind},
        resources::ROOT_VISUAL,
        server::ActivePointerGrab,
    };

    const GRAB_CLIENT_ID: u32 = 1;
    const GRAB_WIN: u32 = 0x0010_0070;
    const SIBLING_WIN: u32 = 0x0010_0071; // top-level, owned by same client, not descendant of grab window
    const HOST_GRAB_XID: u32 = 0xCAFE_0070;
    const HOST_SIBLING_XID: u32 = 0xCAFE_0071;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, GRAB_CLIENT_ID);
    let mut backend = RecordingBackend::new();
    peer.set_nonblocking(true).expect("nonblocking");

    // Create two top-level windows under root, both owned by the
    // grab client. They are siblings (both children of root), not
    // in an ancestor relationship — the exact topology for
    // mate-panel + popup-menu.
    for (xid, host_xid) in [(GRAB_WIN, HOST_GRAB_XID), (SIBLING_WIN, HOST_SIBLING_XID)] {
        state.resources.create_window(
            ClientId(GRAB_CLIENT_ID),
            yserver_protocol::x11::CreateWindowRequest {
                depth: 24,
                window: ResourceId(xid),
                parent: ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 100,
                height: 100,
                border_width: 0,
                class: 1,
                visual: ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = state.resources.map_window(ResourceId(xid));
        Backend::register_top_level(&mut backend, None, ResourceId(xid), host_xid)
            .expect("register host xid");
    }
    // XI2 mask selecting motion on the sibling window (the menu
    // item the cursor will move over). XI_Motion evtype = 6 →
    // mask bit 1<<6 = 0x40.
    state
        .clients
        .get_mut(&GRAB_CLIENT_ID)
        .expect("client")
        .xi2_masks
        .insert((ResourceId(SIBLING_WIN), 2), 0x40);

    // Install active grab on GRAB_WIN with owner_events=true.
    state.active_pointer_grab = Some(ActivePointerGrab {
        owner: ClientId(GRAB_CLIENT_ID),
        grab_window: ResourceId(GRAB_WIN),
        event_mask: 0,
        cursor: ResourceId(0),
        time: 0,
        owner_events: true,
        via_xi2: true,
        implicit: false,
        passive: false,
        xi2_mask: u64::MAX,
    });
    // Sanity: sibling is NOT a descendant of grab window.
    assert!(
        !state
            .resources
            .is_descendant_of(ResourceId(SIBLING_WIN), ResourceId(GRAB_WIN))
    );
    // And IS owned by the grab client.
    assert_eq!(
        state.resources.window_owner(ResourceId(SIBLING_WIN)),
        Some(ClientId(GRAB_CLIENT_ID)),
    );

    // Cursor at root_x=50, root_y=50 — landing on SIBLING_WIN.
    // host_xid = SIBLING's host xid so the v2-style hit-test lands
    // on the sibling window naturally.
    let motion = HostPointerEvent {
        origin: crate::core_loop::message::InputOrigin::XTest(4),
        kind: PointerEventKind::MotionNotify,
        host_xid: HOST_SIBLING_XID,
        detail: 0,
        time: 0x1000,
        root_x: 50,
        root_y: 50,
        event_x: 50,
        event_y: 50,
        state: 0,
        crossing_mode: 0,
        child: 0,
        raw_dx: 0,
        raw_dy: 0,
        tree_change: false,
    };
    let xid_map = backend.xid_map().clone();
    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, motion, true, false);

    // Read whatever landed on the wire. With the ownership-aware
    // natural-delivery check, the XI2 motion must report against
    // the sibling window (event=SIBLING_WIN) with sensible coords
    // (event_x ≈ 50), NOT redirected to the grab window with
    // grab-relative coords. Pre-fix the event would be on
    // GRAB_WIN with grab-clamped coords — for this layout
    // (sibling at the same root origin) the difference might be
    // subtle in coords but the `event` window XID is the
    // smoking-gun: GRAB vs SIBLING.
    let mut buf = [0u8; 256];
    let n = peer.read(&mut buf).expect("client got at least one event");
    assert!(n >= 32, "expected at least one event, got {n} bytes");
    // Scan for a Generic XI2 event (type=35, second byte=major opcode of XInputExtension).
    // We just need to find the `event` window XID in the body.
    let mut found_sibling = false;
    let mut found_grab = false;
    let mut i = 0;
    while i + 32 <= n {
        // Generic event byte 0 is type. XI2 Motion event window
        // sits at a known offset; rather than parsing the full
        // event structure, scan for either window XID in the
        // first 64 bytes of the body. Cheap+robust.
        let slice = &buf[i..(i + 64).min(n)];
        if slice.windows(4).any(|w| w == SIBLING_WIN.to_le_bytes()) {
            found_sibling = true;
        }
        if slice.windows(4).any(|w| w == GRAB_WIN.to_le_bytes()) {
            found_grab = true;
        }
        i += 32;
    }
    assert!(
        found_sibling,
        "XI2 motion must address the sibling window (owner=grab client) under owner_events=true; got buf bytes {:?}",
        &buf[..n.min(64)],
    );
    // The grab window XID may also appear (e.g., in root/sourceid
    // fields by coincidence) so we don't assert !found_grab — the
    // load-bearing assertion is that the natural target window
    // shows up in the body.
    let _ = found_grab;
}

/// Regression for the MATE menu-click bug: when a release was
/// queued during a sync passive grab freeze (because the WM hadn't
/// yet called AllowEvents), the replay path MUST drain both the
/// frozen press AND the queued release to the natural target — in
/// that order. Pre-fix the release leaked through during freeze
/// and the press was never replayed (frozen state cleared by the
/// rogue release path), so the app saw release-then-nothing.
#[test]
fn xi_allow_events_replay_device_drains_queued_release_after_press() {
    use crate::{
        backend::Backend,
        host_x11::{HostPointerEvent, PointerEventKind},
        resources::ROOT_VISUAL,
    };

    const GRAB_CLIENT_ID: u32 = 1;
    const TARGET_CLIENT_ID: u32 = 2;
    const GRAB_WIN: u32 = 0x0010_0061;
    const TARGET_WIN: u32 = 0x0020_0062;
    const HOST_XID: u32 = 0xCAFE_0010;

    let mut state = ServerState::new();
    let mut grab_peer = install_client(&mut state, GRAB_CLIENT_ID);
    let mut target_peer = install_client(&mut state, TARGET_CLIENT_ID);
    let mut backend = RecordingBackend::new();
    grab_peer
        .set_nonblocking(true)
        .expect("grab peer nonblocking");
    target_peer
        .set_nonblocking(true)
        .expect("target peer nonblocking");

    state.resources.create_window(
        ClientId(GRAB_CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(GRAB_WIN),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            border_width: 0,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default()
        },
    );
    state.resources.create_window(
        ClientId(TARGET_CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(TARGET_WIN),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            border_width: 0,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(GRAB_WIN));
    let _ = state.resources.map_window(ResourceId(TARGET_WIN));
    // Target client selects ButtonPress | ButtonRelease.
    state
        .clients
        .get_mut(&TARGET_CLIENT_ID)
        .expect("target client")
        .event_masks
        .insert(ResourceId(TARGET_WIN), 0x0000_000c);

    set_test_pointer_grab(&mut state, GRAB_CLIENT_ID, GRAB_WIN, true, false);
    let press = HostPointerEvent {
        origin: crate::core_loop::message::InputOrigin::XTest(4),
        kind: PointerEventKind::ButtonPress,
        host_xid: HOST_XID,
        detail: 1,
        time: 0x1c33,
        root_x: 10,
        root_y: 10,
        event_x: 10,
        event_y: 10,
        state: 0,
        crossing_mode: 0,
        child: 0,
        raw_dx: 0,
        raw_dy: 0,
        tree_change: false,
    };
    {
        let f = state
            .xi1_frozen
            .entry(crate::xinput::DEVICEID_MASTER_POINTER)
            .or_default();
        f.state = crate::server::Xi1SyncState::FrozenWithEvent;
        f.stored = Some(crate::server::QueuedInputEvent::HostPointer(press));
    }
    // Queue a release that arrived while frozen.
    let release = HostPointerEvent {
        origin: crate::core_loop::message::InputOrigin::XTest(4),
        kind: PointerEventKind::ButtonRelease,
        time: 0x1c89,
        ..press
    };
    state
        .sync_pending
        .push_back(crate::server::PendingSyncEvent {
            device: crate::xinput::DEVICEID_MASTER_POINTER,
            event: crate::server::QueuedInputEvent::HostPointer(release),
        });
    Backend::register_top_level(&mut backend, None, ResourceId(TARGET_WIN), HOST_XID)
        .expect("register host xid");

    // XIAllowEvents ReplayDevice on master pointer (deviceid=2).
    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&2u16.to_le_bytes());
    body.push(2); // mode = ReplayDevice
    body.push(0); // pad
    let header = yserver_protocol::x11::RequestHeader {
        opcode: 131,
        data: 53,
        length_units: 3,
    };
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(GRAB_CLIENT_ID),
        SequenceNumber(1),
        header,
        &body,
    )
    .expect("allow events");

    // Both frozen slots cleared after drain.
    assert!(
        state
            .xi1_frozen
            .get(&crate::xinput::DEVICEID_MASTER_POINTER)
            .and_then(|f| f.stored.as_ref())
            .is_none()
    );
    assert!(
        !state
            .sync_pending
            .iter()
            .any(|p| p.device == crate::xinput::DEVICEID_MASTER_POINTER)
    );
    assert!(state.active_pointer_grab.is_none());
    assert_eq!(
        state.buttons_down, 0,
        "replayed press and queued release leave the master clear"
    );
    assert_eq!(
        state.xi_devices.device(4).unwrap().buttons_down,
        0,
        "replayed press and queued release leave XTEST 4 clear"
    );

    // Grab owner must not receive the replayed events (they go to
    // the natural target).
    let mut buf = [0u8; 32];
    let grab_read = grab_peer.read(&mut buf);
    assert!(
        matches!(grab_read, Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock),
        "grab owner must not receive replay; got {grab_read:?}",
    );

    // Target must receive BOTH events in order. Read a sequence of
    // 32-byte core events, expecting Press(type=4) then
    // Release(type=5). The XI2 fanout would also fire (each event
    // shipping a GenericEvent), so we read until we've seen one
    // press and one release.
    let mut saw_press = false;
    let mut saw_release = false;
    let mut press_offset = usize::MAX;
    let mut release_offset = usize::MAX;
    let mut total_read = 0usize;
    // Slurp whatever the wire has — bound the read loop to avoid
    // hanging if something goes wrong.
    for iter in 0..8 {
        let mut chunk = [0u8; 256];
        match target_peer.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                // Scan 32-byte boundaries for core ButtonPress(4)
                // / ButtonRelease(5). GenericEvents (XI2) are
                // type=35 and longer; their first byte still
                // shows up at a 32-byte offset.
                let mut i = 0;
                while i + 32 <= n {
                    let evt_type = chunk[i] & 0x7F;
                    if evt_type == 4 && !saw_press {
                        saw_press = true;
                        press_offset = total_read + i;
                    } else if evt_type == 5 && !saw_release {
                        saw_release = true;
                        release_offset = total_read + i;
                    }
                    // GenericEvent is variable-length; advance by 32 conservatively.
                    i += 32;
                }
                total_read += n;
                if saw_press && saw_release {
                    break;
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) => panic!("target read failed at iter {iter}: {e}"),
        }
    }
    assert!(saw_press, "target must receive replayed ButtonPress");
    assert!(saw_release, "target must receive queued ButtonRelease");
    assert!(
        press_offset < release_offset,
        "Press must arrive BEFORE Release at the target (got press@{press_offset}, release@{release_offset})",
    );
}

/// Regression: XI2 `XIAllowEvents(AsyncDevice)` must thaw the unified
/// per-device freeze AND replay the events withheld during the freeze.
/// muffin (Cinnamon) installs XI2 sync passive button grabs; activating
/// one engages `xi1_frozen[POINTER]` via `xi1_check_grab_for_syncs`,
/// after which the unified-freeze gate queues every core pointer event.
/// The XI2 AllowEvents handler historically never touched `xi1_frozen`,
/// so `AsyncDevice` left the pointer frozen forever and dropped the
/// withheld queue — the Cinnamon desktop never saw the drag's
/// ButtonRelease, leaving the selection rubber-band stuck. (Core grabs
/// — marco/xfwm4 — thaw via the core AllowEvents path, so MATE/XFCE
/// were unaffected.)
#[test]
fn xi_allow_events_async_device_thaws_freeze_and_replays_queue() {
    use crate::{
        backend::Backend,
        host_x11::{HostPointerEvent, PointerEventKind},
        resources::ROOT_VISUAL,
        server::{Xi1Freeze, Xi1SyncState},
    };

    const GRAB_CLIENT_ID: u32 = 1;
    const TARGET_CLIENT_ID: u32 = 2;
    const GRAB_WIN: u32 = 0x0010_0061;
    const TARGET_WIN: u32 = 0x0020_0062;
    const HOST_XID: u32 = 0xCAFE_0005;

    let mut state = ServerState::new();
    let mut grab_peer = install_client(&mut state, GRAB_CLIENT_ID);
    let mut target_peer = install_client(&mut state, TARGET_CLIENT_ID);
    let mut backend = RecordingBackend::new();
    grab_peer
        .set_nonblocking(true)
        .expect("grab peer nonblocking");
    target_peer
        .set_nonblocking(true)
        .expect("target peer nonblocking");

    for (client, win) in [(GRAB_CLIENT_ID, GRAB_WIN), (TARGET_CLIENT_ID, TARGET_WIN)] {
        state.resources.create_window(
            ClientId(client),
            yserver_protocol::x11::CreateWindowRequest {
                depth: 24,
                window: ResourceId(win),
                parent: ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 100,
                height: 100,
                border_width: 0,
                class: 1,
                visual: ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = state.resources.map_window(ResourceId(win));
    }
    // Target selects ButtonPress | ButtonRelease.
    state
        .clients
        .get_mut(&TARGET_CLIENT_ID)
        .expect("target client")
        .event_masks
        .insert(ResourceId(TARGET_WIN), 0x0000_000c);

    // muffin's sync passive grab has activated: grab held, the
    // activating press already delivered to the owner, the pointer
    // device frozen, and a release queued during the freeze.
    set_test_pointer_grab(&mut state, GRAB_CLIENT_ID, GRAB_WIN, true, false);
    // The activated passive grab's defining record (present for any
    // real activation) — its mask is what the drain-path redirect
    // reports to the grab owner.
    state.button_grabs.push(crate::server::PassiveButtonGrab {
        device_id: 0,
        owner: ClientId(GRAB_CLIENT_ID),
        grab_window: ResourceId(GRAB_WIN),
        button: 1,
        modifiers: 0x8000,
        owner_events: false,
        event_mask: 0x0000_000c, // press|release
        pointer_mode: 0,         // sync
        keyboard_mode: 1,
        confine_to: ResourceId(0),
        via_xi2: false,
    });
    state.xi1_frozen.insert(
        crate::xinput::DEVICEID_MASTER_POINTER,
        Xi1Freeze {
            state: Xi1SyncState::FrozenNoEvent,
            ..Default::default()
        },
    );
    let press = HostPointerEvent {
        origin: crate::core_loop::message::InputOrigin::XTest(4),
        kind: PointerEventKind::ButtonPress,
        host_xid: HOST_XID,
        detail: 1,
        time: 0x2a00,
        root_x: 10,
        root_y: 10,
        event_x: 10,
        event_y: 10,
        state: 0,
        crossing_mode: 0,
        child: 0,
        raw_dx: 0,
        raw_dy: 0,
        tree_change: false,
    };
    {
        let f = state
            .xi1_frozen
            .entry(crate::xinput::DEVICEID_MASTER_POINTER)
            .or_default();
        f.state = crate::server::Xi1SyncState::FrozenWithEvent;
        f.stored = Some(crate::server::QueuedInputEvent::HostPointer(press));
    }
    state
        .sync_pending
        .push_back(crate::server::PendingSyncEvent {
            device: crate::xinput::DEVICEID_MASTER_POINTER,
            event: crate::server::QueuedInputEvent::HostPointer(HostPointerEvent {
                origin: crate::core_loop::message::InputOrigin::XTest(4),
                kind: PointerEventKind::ButtonRelease,
                time: 0x2a50,
                ..press
            }),
        });
    Backend::register_top_level(&mut backend, None, ResourceId(TARGET_WIN), HOST_XID)
        .expect("register host xid");

    // XIAllowEvents AsyncDevice on master pointer (deviceid=2).
    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&0u32.to_le_bytes()); // time
    body.extend_from_slice(&2u16.to_le_bytes()); // deviceid
    body.push(0); // mode = AsyncDevice
    body.push(0); // pad
    let header = yserver_protocol::x11::RequestHeader {
        opcode: 131,
        data: 53,
        length_units: 3,
    };
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(GRAB_CLIENT_ID),
        SequenceNumber(1),
        header,
        &body,
    )
    .expect("allow events");

    // The unified pointer freeze must be thawed — otherwise every
    // later core pointer event is queued-then-dropped (dead pointer).
    assert!(
        !state
            .xi1_frozen
            .get(&crate::xinput::DEVICEID_MASTER_POINTER)
            .is_some_and(crate::server::Xi1Freeze::frozen),
        "AsyncDevice must thaw the unified per-device pointer freeze"
    );
    assert!(
        state.active_pointer_grab.is_none(),
        "passive grab ended by the terminating (replayed) release, not by AsyncDevice"
    );
    assert!(
        !state
            .sync_pending
            .iter()
            .any(|p| p.device == crate::xinput::DEVICEID_MASTER_POINTER),
        "withheld queue must be replayed, not left pending"
    );

    // AsyncDevice thaws WITHOUT deactivating the grab (Xorg AllowSome
    // THAWED, dix/events.c:1857 — DeactivateGrab is the NOT_GRABBED
    // /Replay case only). So the withheld release replays THROUGH the
    // grab (ComputeFreezes → PlayReleasedEvents → DeliverGrabbedEvent):
    // it is reported to the GRAB OWNER on the grab window, then the
    // terminating release ends the passive grab. It must NOT leak to
    // the foreign natural target (pre-implicit-grab #94 bug).
    assert!(
        peer_saw_event(&mut grab_peer, 5, None),
        "withheld ButtonRelease must replay THROUGH the grab to its owner \
             on AsyncDevice thaw"
    );
    assert!(
        !peer_saw_event(&mut target_peer, 5, None),
        "the grab intercepts the release — it must not leak to the foreign \
             natural target (#94)"
    );
}

/// Helper: drain a nonblocking peer and return true if any 32-byte core
/// event with `type & 0x7f == want_type` (and, if `want_detail` is Some,
/// matching `detail` byte) was delivered.
fn peer_saw_event(peer: &mut UnixStream, want_type: u8, want_detail: Option<u8>) -> bool {
    for _ in 0..8 {
        let mut chunk = [0u8; 512];
        match peer.read(&mut chunk) {
            Ok(0) => return false,
            Ok(n) => {
                let mut i = 0;
                while i + 32 <= n {
                    if chunk[i] & 0x7F == want_type && want_detail.is_none_or(|d| chunk[i + 1] == d)
                    {
                        return true;
                    }
                    i += 32;
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => return false,
            Err(e) => panic!("peer read failed: {e}"),
        }
    }
    false
}

/// ISSUE 1 (known-issues.md 2026-07-15): `AllowEvents` admission keys ONLY
/// on the unified per-device sync state (Xorg `AllowSome`,
/// dix/events.c:1851). When the device is thawed in unified state, a
/// `ReplayDevice` from the grabbing client is a no-op — nothing is
/// replayed — regardless of any residual/legacy activating event.
#[test]
fn xi_allow_events_replay_no_op_when_unified_thawed() {
    use crate::{
        resources::ROOT_VISUAL,
        server::{Xi1Freeze, Xi1SyncState},
    };
    const GRAB_CLIENT_ID: u32 = 1;
    const GRAB_WIN: u32 = 0x0010_0101;

    let mut state = ServerState::new();
    let mut grab_peer = install_client(&mut state, GRAB_CLIENT_ID);
    grab_peer.set_nonblocking(true).expect("nonblocking");
    let mut backend = RecordingBackend::new();

    state.resources.create_window(
        ClientId(GRAB_CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(GRAB_WIN),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            border_width: 0,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(GRAB_WIN));
    state
        .clients
        .get_mut(&GRAB_CLIENT_ID)
        .expect("grab client")
        .event_masks
        .insert(ResourceId(GRAB_WIN), 0x0000_000c); // ButtonPress|ButtonRelease

    set_test_pointer_grab(&mut state, GRAB_CLIENT_ID, GRAB_WIN, true, false);
    // Unified state THAWED — the sole admission authority says "not frozen".
    state.xi1_frozen.insert(
        crate::xinput::DEVICEID_XTEST_POINTER,
        Xi1Freeze {
            state: Xi1SyncState::Thawed,
            ..Default::default()
        },
    );

    // XIAllowEvents XIReplayDevice (mode 2) on master pointer (deviceid=2).
    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    body.extend_from_slice(&2u16.to_le_bytes()); // deviceid
    body.push(2); // XIReplayDevice
    body.push(0);
    let header = yserver_protocol::x11::RequestHeader {
        opcode: 131,
        data: 53,
        length_units: 3,
    };
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(GRAB_CLIENT_ID),
        SequenceNumber(1),
        header,
        &body,
    )
    .expect("allow events");

    assert!(
        !peer_saw_event(&mut grab_peer, 4, None) && !peer_saw_event(&mut grab_peer, 5, None),
        "ReplayDevice on a unified-thawed device must be a no-op (no stale replay)"
    );
}

/// ISSUE 2 (known-issues.md 2026-07-15): a thaw via a path OTHER than
/// AllowEvents must still replay the withheld queue (Xorg ComputeFreezes ->
/// PlayReleasedEvents, dix/events.c:1368-1372), not drop it. Drives the
/// production representation (`sync_pending`) that the freeze gate fills.
#[test]
fn out_of_band_pointer_thaw_replays_withheld_release() {
    use crate::{
        backend::Backend,
        host_x11::{HostPointerEvent, PointerEventKind},
        resources::ROOT_VISUAL,
        server::{PendingSyncEvent, QueuedInputEvent, Xi1Freeze, Xi1SyncState},
    };
    const GRAB_CLIENT_ID: u32 = 1;
    const GRAB_WIN: u32 = 0x0010_0111;
    const HOST_XID: u32 = 0xCAFE_0011;

    let mut state = ServerState::new();
    let mut grab_peer = install_client(&mut state, GRAB_CLIENT_ID);
    grab_peer.set_nonblocking(true).expect("nonblocking");
    let mut backend = RecordingBackend::new();

    state.resources.create_window(
        ClientId(GRAB_CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(GRAB_WIN),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            border_width: 0,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(GRAB_WIN));
    state
        .clients
        .get_mut(&GRAB_CLIENT_ID)
        .expect("grab client")
        .event_masks
        .insert(ResourceId(GRAB_WIN), 0x0000_000c);
    Backend::register_top_level(&mut backend, None, ResourceId(GRAB_WIN), HOST_XID)
        .expect("register host xid");

    set_test_pointer_grab(&mut state, GRAB_CLIENT_ID, GRAB_WIN, true, false);
    // A real activated passive grab always has its defining
    // `button_grabs` record present; the drain-path redirect reads its
    // `event_mask` to know what to report to the grab owner (Xorg
    // DeliverGrabbedEvent consults the grab's own mask). Without it the
    // grab captures the release but reports nothing.
    state.button_grabs.push(crate::server::PassiveButtonGrab {
        device_id: 0,
        owner: ClientId(GRAB_CLIENT_ID),
        grab_window: ResourceId(GRAB_WIN),
        button: 1,
        modifiers: 0x8000,
        owner_events: false,
        event_mask: 0x0000_000c, // press|release
        pointer_mode: 0,         // sync
        keyboard_mode: 1,
        confine_to: ResourceId(0),
        via_xi2: false,
    });
    state.xi1_frozen.insert(
        crate::xinput::DEVICEID_XTEST_POINTER,
        Xi1Freeze {
            state: Xi1SyncState::FrozenNoEvent,
            ..Default::default()
        },
    );
    // A release withheld during the freeze, in the global queue (what the
    // production QUEUE-WHILE-FROZEN gate pushes).
    state.sync_pending.push_back(PendingSyncEvent {
        device: crate::xinput::DEVICEID_XTEST_POINTER,
        event: QueuedInputEvent::HostPointer(HostPointerEvent {
            origin: crate::core_loop::message::InputOrigin::XTest(4),
            kind: PointerEventKind::ButtonRelease,
            host_xid: HOST_XID,
            detail: 1,
            time: 0x2a50,
            root_x: 10,
            root_y: 10,
            event_x: 10,
            event_y: 10,
            state: 0,
            crossing_mode: 0,
            child: 0,
            raw_dx: 0,
            raw_dy: 0,
            tree_change: false,
        }),
    });

    // OUT-OF-BAND thaw (NOT AllowEvents).
    let xid_map = backend.xid_map().clone();
    crate::core_loop::pointer_fanout::xi1_thaw_device(
        &mut state,
        &mut backend,
        &xid_map,
        crate::xinput::DEVICEID_XTEST_POINTER,
    );

    assert!(
        peer_saw_event(&mut grab_peer, 5, None),
        "withheld ButtonRelease must be replayed on an out-of-band thaw, not dropped"
    );
    assert!(
        state.sync_pending.is_empty(),
        "the global queue must be drained on thaw"
    );
}

/// Global-queue replay preserves arrival order ACROSS devices (Xorg
/// PlayReleasedEvents over the single `syncEvents.pending`,
/// dix/events.c:1233-1291): a keyboard event queued before a pointer event
/// is replayed first when both thaw in a single `xi1_compute_freezes` pass.
#[test]
fn compute_freezes_replays_global_queue_in_arrival_order() {
    use crate::{
        backend::Backend,
        host_x11::{HostKeyEvent, HostPointerEvent, PointerEventKind},
        resources::ROOT_VISUAL,
        server::{CoreFocus, PendingSyncEvent, QueuedInputEvent, Xi1Freeze, Xi1SyncState},
    };
    const CLIENT_ID: u32 = 1;
    const WIN: u32 = 0x0010_0121;
    const HOST_XID: u32 = 0xCAFE_0021;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT_ID);
    peer.set_nonblocking(true).expect("nonblocking");
    let mut backend = RecordingBackend::new();

    state.resources.create_window(
        ClientId(CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(WIN),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            border_width: 0,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(WIN));
    Backend::register_top_level(&mut backend, None, ResourceId(WIN), HOST_XID)
        .expect("register host xid");
    // KeyPress|KeyRelease|ButtonPress|ButtonRelease.
    state
        .clients
        .get_mut(&CLIENT_ID)
        .expect("client")
        .event_masks
        .insert(ResourceId(WIN), 0x0000_000f);
    // Key delivery routes to the core focus window.
    state.core_focus = CoreFocus {
        raw: WIN,
        revert_to: 0,
        time: 0,
    };

    for dev in [
        crate::xinput::DEVICEID_XTEST_KEYBOARD,
        crate::xinput::DEVICEID_XTEST_POINTER,
    ] {
        state.xi1_frozen.insert(
            dev,
            Xi1Freeze {
                state: Xi1SyncState::FrozenNoEvent,
                ..Default::default()
            },
        );
    }
    // Enqueue KEY (release) THEN POINTER (release), in that global order.
    state.sync_pending.push_back(PendingSyncEvent {
        device: crate::xinput::DEVICEID_XTEST_KEYBOARD,
        event: QueuedInputEvent::HostKey(HostKeyEvent {
            origin: crate::core_loop::InputOrigin::NestedHost,
            pressed: false,
            keycode: 38,
            time: 0x1000,
            root_x: 5,
            root_y: 5,
            event_x: 5,
            event_y: 5,
            state: 0,
        }),
    });
    state.sync_pending.push_back(PendingSyncEvent {
        device: crate::xinput::DEVICEID_XTEST_POINTER,
        event: QueuedInputEvent::HostPointer(HostPointerEvent {
            origin: crate::core_loop::message::InputOrigin::XTest(4),
            kind: PointerEventKind::ButtonRelease,
            host_xid: HOST_XID,
            detail: 1,
            time: 0x2000,
            root_x: 5,
            root_y: 5,
            event_x: 5,
            event_y: 5,
            state: 0,
            crossing_mode: 0,
            child: 0,
            raw_dx: 0,
            raw_dy: 0,
            tree_change: false,
        }),
    });

    // Thaw BOTH, then ONE replay pass.
    for dev in [
        crate::xinput::DEVICEID_XTEST_KEYBOARD,
        crate::xinput::DEVICEID_XTEST_POINTER,
    ] {
        state.xi1_frozen.get_mut(&dev).unwrap().state = Xi1SyncState::Thawed;
    }
    let xid_map = backend.xid_map().clone();
    crate::core_loop::pointer_fanout::xi1_compute_freezes(&mut state, &mut backend, &xid_map);

    // Read the full wire once and compare offsets: KeyRelease (3) precedes
    // ButtonRelease (5).
    let mut buf = [0u8; 8192];
    let n = peer.read(&mut buf).expect("read wire");
    assert!(
        n >= 64,
        "expected both replayed events on the wire (got {n} bytes)"
    );
    let mut key_pos = None;
    let mut btn_pos = None;
    let mut i = 0;
    while i + 32 <= n {
        let ty = buf[i] & 0x7F;
        if ty == 3 && key_pos.is_none() {
            key_pos = Some(i);
        }
        if ty == 5 && btn_pos.is_none() {
            btn_pos = Some(i);
        }
        i += 32;
    }
    assert!(
        key_pos.is_some() && btn_pos.is_some(),
        "both the keyboard and pointer events must be delivered (key={key_pos:?} btn={btn_pos:?})"
    );
    assert!(
        key_pos < btn_pos,
        "keyboard event (queued first) must replay before the pointer event"
    );
    assert!(state.sync_pending.is_empty(), "queue fully drained");
}

/// Regression for the double-button-map hazard: the freeze gate stores the
/// PHYSICAL detail, and replay re-enters button mapping exactly once. With
/// a non-identity `pointer_mapping_override` (physical 1 -> logical 3), the
/// replayed release must carry logical button 3 (mapped once), not a
/// twice-mapped value.
#[test]
fn frozen_pointer_replay_maps_button_exactly_once() {
    use crate::{
        backend::Backend,
        host_x11::{HostPointerEvent, PointerEventKind},
        resources::ROOT_VISUAL,
        server::{Xi1Freeze, Xi1SyncState},
    };
    const CLIENT_ID: u32 = 1;
    const WIN: u32 = 0x0010_0131;
    const HOST_XID: u32 = 0xCAFE_0031;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT_ID);
    peer.set_nonblocking(true).expect("nonblocking");
    let mut backend = RecordingBackend::new();

    state.resources.create_window(
        ClientId(CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(WIN),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            border_width: 0,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(WIN));
    Backend::register_top_level(&mut backend, None, ResourceId(WIN), HOST_XID)
        .expect("register host xid");
    state
        .clients
        .get_mut(&CLIENT_ID)
        .expect("client")
        .event_masks
        .insert(ResourceId(WIN), 0x0000_000c);

    // Physical button 1 -> logical 3 (identity for 2/3).
    state.pointer_mapping_override = Some(vec![3, 2, 1]);
    // Pointer frozen so the release is withheld at the gate.
    state.xi1_frozen.insert(
        crate::xinput::DEVICEID_XTEST_POINTER,
        Xi1Freeze {
            state: Xi1SyncState::FrozenNoEvent,
            ..Default::default()
        },
    );

    let xid_map = backend.xid_map().clone();
    // Drive a PHYSICAL button-1 release through the real fanout: the gate
    // withholds it into sync_pending with the physical detail preserved.
    let release = HostPointerEvent {
        origin: crate::core_loop::message::InputOrigin::XTest(4),
        kind: PointerEventKind::ButtonRelease,
        host_xid: HOST_XID,
        detail: 1,
        time: 0x2a50,
        root_x: 10,
        root_y: 10,
        event_x: 10,
        event_y: 10,
        state: 0,
        crossing_mode: 0,
        child: 0,
        raw_dx: 0,
        raw_dy: 0,
        tree_change: false,
    };
    let _ = crate::core_loop::pointer_fanout::pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &xid_map,
        release,
        true,
        false,
    );
    assert_eq!(
        state.sync_pending.len(),
        1,
        "release must be withheld while frozen"
    );

    // Thaw and replay.
    crate::core_loop::pointer_fanout::xi1_thaw_device(
        &mut state,
        &mut backend,
        &xid_map,
        crate::xinput::DEVICEID_XTEST_POINTER,
    );

    // The delivered ButtonRelease must carry LOGICAL button 3 (mapped once).
    assert!(
        peer_saw_event(&mut peer, 5, Some(3)),
        "replayed release must map physical 1 -> logical 3 exactly once (not double-mapped)"
    );
}

/// XI2 mode → core AllowSome mode mapping ([`xi2_allow_mode_to_core`]).
#[test]
fn xi2_allow_mode_to_core_maps_all_modes() {
    // Pointer: Async/Sync/Replay → Pointer variants.
    assert_eq!(xi2_allow_mode_to_core(0, false), Some(0)); // AsyncDevice → AsyncPointer
    assert_eq!(xi2_allow_mode_to_core(1, false), Some(1)); // SyncDevice → SyncPointer
    assert_eq!(xi2_allow_mode_to_core(2, false), Some(2)); // ReplayDevice → ReplayPointer
    // Keyboard: → Keyboard variants.
    assert_eq!(xi2_allow_mode_to_core(0, true), Some(3)); // → AsyncKeyboard
    assert_eq!(xi2_allow_mode_to_core(1, true), Some(4)); // → SyncKeyboard
    assert_eq!(xi2_allow_mode_to_core(2, true), Some(5)); // → ReplayKeyboard
    // paired: AsyncPairedDevice acts on the OTHER device.
    assert_eq!(xi2_allow_mode_to_core(3, false), Some(3)); // ptr grab → async keyboard
    assert_eq!(xi2_allow_mode_to_core(3, true), Some(0)); // kbd grab → async pointer
    assert_eq!(xi2_allow_mode_to_core(4, false), Some(6)); // AsyncPair → AsyncBoth
    assert_eq!(xi2_allow_mode_to_core(5, false), Some(7)); // SyncPair  → SyncBoth
    // touch / unknown → unsupported.
    assert_eq!(xi2_allow_mode_to_core(6, false), None); // XIAcceptTouch
    assert_eq!(xi2_allow_mode_to_core(7, false), None); // XIRejectTouch
}

/// Regression: XI2 `XIAllowEvents(XISyncDevice)` (mode 1) must thaw the
/// frozen device and replay the withheld queue. It used to be a no-op
/// ("we're always-async"), so muffin's click-to-focus SYNC path (mutter
/// `events.c` `maybe_unfreeze_pointer_events(EVENTS_UNFREEZE_SYNC)`)
/// could not clear the freeze → `QUEUE-WHILE-FROZEN` piled up → the
/// Cinnamon clicks-and-typing-dead freeze. Now routed through the shared
/// `apply_allow_events` (core SyncPointer).
#[test]
fn xi_allow_events_sync_device_thaws_and_drains_queue() {
    use crate::{
        backend::Backend,
        host_x11::{HostPointerEvent, PointerEventKind},
        resources::ROOT_VISUAL,
    };
    const GRAB_CLIENT_ID: u32 = 1;
    const TARGET_CLIENT_ID: u32 = 2;
    const GRAB_WIN: u32 = 0x0010_0071;
    const TARGET_WIN: u32 = 0x0020_0072;
    const HOST_XID: u32 = 0xCAFE_0007;

    let mut state = ServerState::new();
    let mut grab_peer = install_client(&mut state, GRAB_CLIENT_ID);
    let mut target_peer = install_client(&mut state, TARGET_CLIENT_ID);
    let mut backend = RecordingBackend::new();
    grab_peer.set_nonblocking(true).expect("nonblocking");
    target_peer.set_nonblocking(true).expect("nonblocking");

    for (client, win) in [(GRAB_CLIENT_ID, GRAB_WIN), (TARGET_CLIENT_ID, TARGET_WIN)] {
        state.resources.create_window(
            ClientId(client),
            yserver_protocol::x11::CreateWindowRequest {
                depth: 24,
                window: ResourceId(win),
                parent: ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 100,
                height: 100,
                border_width: 0,
                class: 1,
                visual: ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = state.resources.map_window(ResourceId(win));
    }
    state
        .clients
        .get_mut(&TARGET_CLIENT_ID)
        .expect("target")
        .event_masks
        .insert(ResourceId(TARGET_WIN), 0x0000_000c);

    // Activated SYNC passive grab: held, frozen, activating press stored,
    // a release withheld during the freeze.
    set_test_pointer_grab(&mut state, GRAB_CLIENT_ID, GRAB_WIN, true, false);
    // The activated passive grab's defining record (its mask is what
    // the drain-path redirect reports to the grab owner).
    state.button_grabs.push(crate::server::PassiveButtonGrab {
        device_id: 0,
        owner: ClientId(GRAB_CLIENT_ID),
        grab_window: ResourceId(GRAB_WIN),
        button: 1,
        modifiers: 0x8000,
        owner_events: false,
        event_mask: 0x0000_000c, // press|release
        pointer_mode: 0,         // sync
        keyboard_mode: 1,
        confine_to: ResourceId(0),
        via_xi2: false,
    });
    state.xi1_frozen.insert(
        crate::xinput::DEVICEID_MASTER_POINTER,
        crate::server::Xi1Freeze {
            state: crate::server::Xi1SyncState::FrozenNoEvent,
            ..Default::default()
        },
    );
    let press = HostPointerEvent {
        origin: crate::core_loop::message::InputOrigin::XTest(4),
        kind: PointerEventKind::ButtonPress,
        host_xid: HOST_XID,
        detail: 1,
        time: 0x10,
        root_x: 10,
        root_y: 10,
        event_x: 10,
        event_y: 10,
        state: 0,
        crossing_mode: 0,
        child: 0,
        raw_dx: 0,
        raw_dy: 0,
        tree_change: false,
    };
    {
        let f = state
            .xi1_frozen
            .entry(crate::xinput::DEVICEID_MASTER_POINTER)
            .or_default();
        f.state = crate::server::Xi1SyncState::FrozenWithEvent;
        f.stored = Some(crate::server::QueuedInputEvent::HostPointer(press));
    }
    state
        .sync_pending
        .push_back(crate::server::PendingSyncEvent {
            device: crate::xinput::DEVICEID_MASTER_POINTER,
            event: crate::server::QueuedInputEvent::HostPointer(HostPointerEvent {
                origin: crate::core_loop::message::InputOrigin::XTest(4),
                kind: PointerEventKind::ButtonRelease,
                time: 0x20,
                ..press
            }),
        });
    Backend::register_top_level(&mut backend, None, ResourceId(TARGET_WIN), HOST_XID)
        .expect("register host xid");

    // XIAllowEvents XISyncDevice (mode 1) on master pointer (deviceid 2).
    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&0u32.to_le_bytes()); // time
    body.extend_from_slice(&2u16.to_le_bytes()); // deviceid
    body.push(1); // mode = XISyncDevice
    body.push(0); // pad
    let header = yserver_protocol::x11::RequestHeader {
        opcode: 131,
        data: 53,
        length_units: 3,
    };
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(GRAB_CLIENT_ID),
        SequenceNumber(1),
        header,
        &body,
    )
    .expect("allow events");

    assert!(
        !state
            .xi1_frozen
            .get(&crate::xinput::DEVICEID_MASTER_POINTER)
            .is_some_and(crate::server::Xi1Freeze::frozen),
        "XISyncDevice must thaw the frozen pointer (was a no-op → Cinnamon freeze)"
    );
    assert!(
        !state
            .sync_pending
            .iter()
            .any(|p| p.device == crate::xinput::DEVICEID_MASTER_POINTER),
        "withheld queue must be drained on XISyncDevice"
    );

    // XISyncDevice (FREEZE_NEXT_EVENT) thaws WITHOUT deactivating the
    // grab (Xorg AllowSome, dix/events.c:1864 — only NOT_GRABBED
    // /Replay deactivates). The withheld release replays THROUGH the
    // grab (DeliverGrabbedEvent) to the grab owner, then the
    // terminating release ends the passive grab; it must NOT leak to
    // the foreign natural target (#94).
    assert!(
        peer_saw_event(&mut grab_peer, 5, None),
        "withheld ButtonRelease must replay THROUGH the grab to its owner \
             on XISyncDevice"
    );
    assert!(
        !peer_saw_event(&mut target_peer, 5, None),
        "the grab intercepts the release — it must not leak to the foreign \
             natural target (#94)"
    );
}

#[test]
fn xi_allow_events_replay_device_releases_active_pointer_grab() {
    use crate::{
        backend::Backend,
        host_x11::{HostPointerEvent, PointerEventKind},
        resources::ROOT_VISUAL,
        server::ActivePointerGrab,
    };

    const GRAB_CLIENT_ID: u32 = 1;
    const TARGET_WIN: u32 = 0x0020_0052;
    const HOST_XID: u32 = 0xCAFE_0003;

    let mut state = ServerState::new();
    let _grab_peer = install_client(&mut state, GRAB_CLIENT_ID);
    let mut backend = RecordingBackend::new();

    state.resources.create_window(
        ClientId(GRAB_CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(TARGET_WIN),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            border_width: 0,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(TARGET_WIN));
    state.active_pointer_grab = Some(ActivePointerGrab {
        owner: ClientId(GRAB_CLIENT_ID),
        grab_window: ResourceId(TARGET_WIN),
        event_mask: 0xFFFF,
        cursor: ResourceId(0),
        time: 0,
        owner_events: false,
        via_xi2: true,
        implicit: false,
        passive: false,
        xi2_mask: u64::MAX,
    });
    // ReplayDevice only acts on a FROZEN device with a stored event to
    // replay (Xorg AllowSome) — engage the sync freeze + activating press.
    state.xi1_frozen.insert(
        crate::xinput::DEVICEID_MASTER_POINTER,
        crate::server::Xi1Freeze {
            state: crate::server::Xi1SyncState::FrozenNoEvent,
            ..Default::default()
        },
    );
    {
        let f = state
            .xi1_frozen
            .entry(crate::xinput::DEVICEID_MASTER_POINTER)
            .or_default();
        f.state = crate::server::Xi1SyncState::FrozenWithEvent;
        f.stored = Some(crate::server::QueuedInputEvent::HostPointer(
            HostPointerEvent {
                origin: crate::core_loop::message::InputOrigin::XTest(4),
                kind: PointerEventKind::ButtonPress,
                host_xid: HOST_XID,
                detail: 1,
                time: 0,
                root_x: 10,
                root_y: 10,
                event_x: 10,
                event_y: 10,
                state: 0,
                crossing_mode: 0,
                child: 0,
                raw_dx: 0,
                raw_dy: 0,
                tree_change: false,
            },
        ));
    }
    Backend::register_top_level(&mut backend, None, ResourceId(TARGET_WIN), HOST_XID)
        .expect("register host xid");

    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&0u32.to_le_bytes()); // time
    body.extend_from_slice(&2u16.to_le_bytes()); // deviceid
    body.push(2); // mode = ReplayDevice
    body.push(0); // pad

    let header = yserver_protocol::x11::RequestHeader {
        opcode: 131,
        data: 53,
        length_units: 3,
    };
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(GRAB_CLIENT_ID),
        SequenceNumber(1),
        header,
        &body,
    )
    .expect("allow events replay pointer");

    assert!(state.active_pointer_grab.is_none());
}

#[test]
fn xi_sync_passive_grab_replays_xi2_press_to_target_only_after_allow_events() {
    use crate::{
        backend::Backend,
        host_x11::{HostPointerEvent, PointerEventKind},
        resources::ROOT_VISUAL,
        server::PassiveButtonGrab,
    };

    const GRAB_CLIENT_ID: u32 = 1;
    const TARGET_CLIENT_ID: u32 = 2;
    const TARGET_WIN: u32 = 0x0020_0052;
    const HOST_XID: u32 = 0xCAFE_0002;
    const XI2_BUTTON_PRESS_BIT: u32 = 1 << 4;

    let mut state = ServerState::new();
    let mut grab_peer = install_client(&mut state, GRAB_CLIENT_ID);
    let mut target_peer = install_client(&mut state, TARGET_CLIENT_ID);
    let mut backend = RecordingBackend::new();
    grab_peer
        .set_nonblocking(true)
        .expect("grab peer nonblocking");
    target_peer
        .set_nonblocking(true)
        .expect("target peer nonblocking");

    state.resources.create_window(
        ClientId(TARGET_CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(TARGET_WIN),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            border_width: 0,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(TARGET_WIN));
    state
        .clients
        .get_mut(&GRAB_CLIENT_ID)
        .expect("grab client")
        .xi2_masks
        .insert((ResourceId(TARGET_WIN), 1), u64::from(XI2_BUTTON_PRESS_BIT));
    state
        .clients
        .get_mut(&TARGET_CLIENT_ID)
        .expect("target client")
        .xi2_masks
        .insert((ResourceId(TARGET_WIN), 1), u64::from(XI2_BUTTON_PRESS_BIT));
    state.button_grabs.push(PassiveButtonGrab {
        device_id: 0,
        owner: ClientId(GRAB_CLIENT_ID),
        grab_window: ResourceId(TARGET_WIN),
        button: 1,
        modifiers: 0,
        owner_events: false,
        event_mask: 0xFFFF_FFFF,
        pointer_mode: 0,
        keyboard_mode: 1,
        confine_to: ResourceId(0),
        via_xi2: true,
    });
    Backend::register_top_level(&mut backend, None, ResourceId(TARGET_WIN), HOST_XID)
        .expect("register host xid");

    let event = HostPointerEvent {
        origin: crate::core_loop::message::InputOrigin::XTest(4),
        kind: PointerEventKind::ButtonPress,
        host_xid: HOST_XID,
        detail: 1,
        time: 0x1234,
        root_x: 10,
        root_y: 10,
        event_x: 10,
        event_y: 10,
        state: 0,
        crossing_mode: 0,
        child: 0,
        raw_dx: 0,
        raw_dy: 0,
        tree_change: false,
    };
    let xid_map = backend.xid_map().clone();
    let _dropped =
        pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, event, true, false);

    assert!(state.active_pointer_grab.is_some_and(|grab| grab.passive));
    assert!(
        state
            .xi1_frozen
            .get(&crate::xinput::DEVICEID_MASTER_POINTER)
            .and_then(|f| f.stored.as_ref())
            .is_some()
    );

    let mut buf = [0u8; 128];
    let target_read = target_peer.read(&mut buf);
    assert!(
        matches!(target_read, Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock),
        "sync passive XI2 grab must withhold the initial XI2 press from the target; got {target_read:?}",
    );
    let grab_read = grab_peer.read(&mut buf);
    assert!(
        matches!(grab_read, Ok(n) if n > 0),
        "grab owner should receive the grabbed press stream; got {grab_read:?}",
    );

    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&0x1234u32.to_le_bytes()); // time
    body.extend_from_slice(&2u16.to_le_bytes()); // deviceid
    body.push(2); // mode = ReplayDevice
    body.push(0); // pad

    let header = yserver_protocol::x11::RequestHeader {
        opcode: 131,
        data: 53,
        length_units: 3,
    };
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(GRAB_CLIENT_ID),
        SequenceNumber(1),
        header,
        &body,
    )
    .expect("allow events");

    let target_read = target_peer.read(&mut buf);
    assert!(
        matches!(target_read, Ok(n) if n >= 32 && buf[0] == 35),
        "ReplayDevice must redeliver the XI2 press to the natural target; got {target_read:?}",
    );
}

#[test]
fn xi_sync_passive_grab_release_during_freeze_queues_for_replay() {
    // Regression: in a sync passive grab, a ButtonRelease arriving
    // BEFORE the grab owner sends AllowEvents must NOT clear the
    // grab nor leak to the natural target — it must queue alongside
    // the activating press for replay. Pre-fix, the release cleared
    // `frozen_pointer_event` + `pointer_grab_is_passive`, so when
    // AllowEvents(ReplayPointer/ReplayDevice) finally arrived there
    // was nothing to replay. Visible symptom on MATE: marco's slow
    // ~10 round-trip grab handler vs a fast click → app never sees
    // the press → menu/titlebar clicks dead.
    use crate::{
        backend::Backend,
        host_x11::{HostPointerEvent, PointerEventKind},
        resources::ROOT_VISUAL,
        server::PassiveButtonGrab,
    };

    const CLIENT_ID: u32 = 1;
    const WINDOW_XID: u32 = 0x0020_0053;
    const HOST_XID: u32 = 0xCAFE_0004;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, CLIENT_ID);
    let mut backend = RecordingBackend::new();

    state.resources.create_window(
        ClientId(CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(WINDOW_XID),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            border_width: 0,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(WINDOW_XID));
    state.button_grabs.push(PassiveButtonGrab {
        device_id: 0,
        owner: ClientId(CLIENT_ID),
        grab_window: ResourceId(WINDOW_XID),
        button: 1,
        modifiers: 0,
        owner_events: true,
        event_mask: 0xFFFF_FFFF,
        pointer_mode: 0, // Synchronous → triggers freeze
        keyboard_mode: 1,
        confine_to: ResourceId(0),
        via_xi2: true,
    });
    Backend::register_top_level(&mut backend, None, ResourceId(WINDOW_XID), HOST_XID)
        .expect("register host xid");

    let press = HostPointerEvent {
        origin: crate::core_loop::message::InputOrigin::XTest(4),
        kind: PointerEventKind::ButtonPress,
        host_xid: HOST_XID,
        detail: 1,
        time: 0x1234,
        root_x: 10,
        root_y: 10,
        event_x: 10,
        event_y: 10,
        state: 0,
        crossing_mode: 0,
        child: 0,
        raw_dx: 0,
        raw_dy: 0,
        tree_change: false,
    };
    let xid_map = backend.xid_map().clone();
    let _dropped =
        pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, press, true, false);
    assert!(state.active_pointer_grab.is_some_and(|grab| grab.passive));
    assert!(
        state
            .xi1_frozen
            .get(&crate::xinput::DEVICEID_MASTER_POINTER)
            .and_then(|f| f.stored.as_ref())
            .is_some()
    );
    // No ButtonRelease is withheld yet — it hasn't arrived. (The unified
    // queue may already hold the activating press's XI1 form; that is
    // expected — Xorg stores the activating event too. We assert on the
    // RELEASE specifically, which is what this test is about.)
    assert!(
        !state
            .sync_pending
            .iter()
            .any(|p| p.device == crate::xinput::DEVICEID_MASTER_POINTER
                && matches!(
                    &p.event,
                    crate::server::QueuedInputEvent::HostPointer(e)
                        if e.kind == PointerEventKind::ButtonRelease
                )),
        "no ButtonRelease must be queued before the release arrives"
    );

    // Release arrives BEFORE AllowEvents (the load-bearing case).
    let release = HostPointerEvent {
        origin: crate::core_loop::message::InputOrigin::XTest(4),
        kind: PointerEventKind::ButtonRelease,
        time: 0x1280,
        ..press
    };
    let xid_map = backend.xid_map().clone();
    let _dropped =
        pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, release, true, false);

    // Grab MUST still be active (waiting for AllowEvents). Press
    // is still frozen. Release is in the queue.
    assert!(
        state.active_pointer_grab.is_some(),
        "grab must remain active while frozen — release waits for AllowEvents",
    );
    assert!(state.active_pointer_grab.is_some_and(|grab| grab.passive));
    assert!(
        state
            .xi1_frozen
            .get(&crate::xinput::DEVICEID_MASTER_POINTER)
            .and_then(|f| f.stored.as_ref())
            .is_some()
    );
    // The withheld release must sit in the global replay queue as a core
    // HostPointer event (Xorg syncEvents.pending), not be delivered to the
    // natural target.
    assert!(
        state
            .sync_pending
            .iter()
            .any(|p| p.device == crate::xinput::DEVICEID_MASTER_POINTER
                && matches!(
                    &p.event,
                    crate::server::QueuedInputEvent::HostPointer(e)
                        if e.kind == PointerEventKind::ButtonRelease
                )),
        "release must be queued for replay, not delivered to natural target",
    );
}

/// Xorg semantics (`DeliverGrabbedEvent`, dix/events.c:4361): the
/// `owner_events=true` natural walk is filtered to the GRAB
/// CLIENT — `TryClientEvents` (dix/events.c:2069) returns -1 for
/// any other client ("not delivered due to grab"), aborting
/// propagation; the press then falls back to the grab window.
///
/// This test previously pinned the OPPOSITE ("delivers descendant
/// window regardless of owner", from the 2b2680e cinnamon wip) —
/// that leak is what wedged wmaker on HW 2026-06-04: the sync
/// grab froze the queue while the press went to the app client,
/// so the WM never called AllowEvents and clicks died.
#[test]
fn xi_passive_grab_owner_events_redirects_foreign_descendant_press_to_grab_client() {
    use crate::{
        backend::Backend,
        host_x11::{HostPointerEvent, PointerEventKind},
        resources::ROOT_VISUAL,
        server::PassiveButtonGrab,
    };

    const GRAB_CLIENT_ID: u32 = 1;
    const CHILD_CLIENT_ID: u32 = 2;
    const GRAB_WIN: u32 = 0x0020_0062;
    const CHILD_WIN: u32 = 0x0020_0063;
    const HOST_XID: u32 = 0xCAFE_0005;

    let mut state = ServerState::new();
    let mut grab_peer = install_client(&mut state, GRAB_CLIENT_ID);
    let mut child_peer = install_client(&mut state, CHILD_CLIENT_ID);
    let mut backend = RecordingBackend::new();
    grab_peer
        .set_nonblocking(true)
        .expect("grab peer nonblocking");
    child_peer
        .set_nonblocking(true)
        .expect("child peer nonblocking");

    state.resources.create_window(
        ClientId(GRAB_CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(GRAB_WIN),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            border_width: 0,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default()
        },
    );
    state.resources.create_window(
        ClientId(CHILD_CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(CHILD_WIN),
            parent: ResourceId(GRAB_WIN),
            x: 10,
            y: 10,
            width: 40,
            height: 40,
            border_width: 0,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(GRAB_WIN));
    let _ = state.resources.map_window(ResourceId(CHILD_WIN));
    state
        .clients
        .get_mut(&CHILD_CLIENT_ID)
        .expect("child client")
        .event_masks
        .insert(ResourceId(CHILD_WIN), 0x0000_0004);
    state.button_grabs.push(PassiveButtonGrab {
        device_id: 0,
        owner: ClientId(GRAB_CLIENT_ID),
        grab_window: ResourceId(GRAB_WIN),
        button: 1,
        modifiers: 0,
        owner_events: true,
        event_mask: 0xFFFF_FFFF,
        pointer_mode: 0,
        keyboard_mode: 1,
        confine_to: ResourceId(0),
        via_xi2: true,
    });
    Backend::register_top_level(&mut backend, None, ResourceId(GRAB_WIN), HOST_XID)
        .expect("register host xid");

    let press = HostPointerEvent {
        origin: crate::core_loop::message::InputOrigin::XTest(4),
        kind: PointerEventKind::ButtonPress,
        host_xid: HOST_XID,
        detail: 1,
        time: 0x1234,
        root_x: 20,
        root_y: 20,
        event_x: 20,
        event_y: 20,
        state: 0,
        crossing_mode: 0,
        child: 0,
        raw_dx: 0,
        raw_dy: 0,
        tree_change: false,
    };
    let xid_map = backend.xid_map().clone();
    let _dropped =
        pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, press, true, false);

    let mut buf = [0u8; 128];
    let grab_read = grab_peer.read(&mut buf);
    assert!(
        matches!(grab_read, Ok(n) if n >= 32),
        "press on a foreign-owned descendant must fall back to grab-window \
             delivery to the GRAB client (Xorg dix/events.c:2069 aborts the \
             natural walk on a foreign subscriber); got {grab_read:?}",
    );
    assert_eq!(buf[0], 4, "core ButtonPress to the grab client");
    assert_eq!(
        &buf[12..16],
        &GRAB_WIN.to_le_bytes(),
        "press reported on the grab window",
    );
    let child_read = child_peer.read(&mut buf);
    let child_got_core_press =
        matches!(child_read, Ok(n) if buf[..n.min(128)].chunks(32).any(|c| c[0] == 4));
    assert!(
        !child_got_core_press,
        "the foreign descendant's owner must NOT see the core press while \
             the grab holds; got {child_read:?}",
    );
    assert!(
        state
            .xi1_frozen
            .get(&crate::xinput::DEVICEID_MASTER_POINTER)
            .and_then(|f| f.stored.as_ref())
            .is_some(),
        "GrabModeSync activation with a delivery must freeze the queue",
    );
}

/// During a window move, muffin holds an *active* `XIGrabDevice` on
/// the master pointer. Every master-pointer XI2 event — including the
/// final `XI_ButtonRelease` — must be funnelled to the grab owner,
/// even after the drag has pulled the pointer over another client's
/// window. Regression: the XI2 fanout routed purely by window
/// mask-selection and ignored the active grab, so the release was
/// delivered to the window under the cursor (nemo-desktop) instead of
/// the grab owner (muffin) — leaving muffin stuck in its move loop
/// with the button apparently still held.
#[test]
fn xi_active_device_grab_funnels_button_release_to_grab_owner_not_window_under_cursor() {
    use crate::{
        backend::Backend,
        host_x11::{HostPointerEvent, PointerEventKind},
        resources::ROOT_VISUAL,
        server::ActivePointerGrab,
    };

    const GRAB_CLIENT_ID: u32 = 1; // muffin
    const OTHER_CLIENT_ID: u32 = 2; // nemo-desktop
    const GRAB_WIN: u32 = 0x0010_0011; // muffin's grab window
    const OTHER_WIN: u32 = 0x0025_0003; // nemo-desktop fullscreen window
    const OTHER_HOST_XID: u32 = 0xCAFE_0006;
    const XI2_BUTTON_RELEASE_BIT: u32 = 1 << 5;

    let mut state = ServerState::new();
    let mut grab_peer = install_client(&mut state, GRAB_CLIENT_ID);
    let mut other_peer = install_client(&mut state, OTHER_CLIENT_ID);
    let mut backend = RecordingBackend::new();
    grab_peer.set_nonblocking(true).unwrap();
    other_peer.set_nonblocking(true).unwrap();

    // muffin's grab window — small, off in the corner. The drag pulls
    // the pointer away from it onto the desktop.
    state.resources.create_window(
        ClientId(GRAB_CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(GRAB_WIN),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 50,
            height: 50,
            border_width: 0,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default()
        },
    );
    // nemo-desktop's window — covers where the pointer ends up.
    state.resources.create_window(
        ClientId(OTHER_CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(OTHER_WIN),
            parent: ROOT_WINDOW,
            x: 200,
            y: 200,
            width: 400,
            height: 400,
            border_width: 0,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(GRAB_WIN));
    let _ = state.resources.map_window(ResourceId(OTHER_WIN));

    // nemo selects XI2 ButtonRelease on its own window (XIAllDevices=1).
    state
        .clients
        .get_mut(&OTHER_CLIENT_ID)
        .expect("other client")
        .xi2_masks
        .insert(
            (ResourceId(OTHER_WIN), 1),
            u64::from(XI2_BUTTON_RELEASE_BIT),
        );

    // muffin holds an active master-pointer grab (XIGrabDevice).
    // owner_events=false → every event funnels to the grab owner.
    state.active_pointer_grab = Some(ActivePointerGrab {
        owner: ClientId(GRAB_CLIENT_ID),
        grab_window: ResourceId(GRAB_WIN),
        event_mask: 0xFFFF,
        cursor: ResourceId(0),
        time: 0,
        owner_events: false,
        via_xi2: true,
        implicit: false,
        passive: false,
        xi2_mask: u64::MAX,
    });

    Backend::register_top_level(&mut backend, None, ResourceId(OTHER_WIN), OTHER_HOST_XID)
        .expect("register host xid");

    // Pointer has dragged onto nemo-desktop (200,200..600,600); button 1
    // still held. event_x/y are relative to OTHER_WIN's origin.
    let release = HostPointerEvent {
        origin: crate::core_loop::message::InputOrigin::XTest(4),
        kind: PointerEventKind::ButtonRelease,
        host_xid: OTHER_HOST_XID,
        detail: 1,
        time: 0x2000,
        root_x: 300,
        root_y: 300,
        event_x: 100,
        event_y: 100,
        state: 0x100, // button 1 down
        crossing_mode: 0,
        child: 0,
        raw_dx: 0,
        raw_dy: 0,
        tree_change: false,
    };
    let xid_map = backend.xid_map().clone();
    let press = HostPointerEvent {
        kind: PointerEventKind::ButtonPress,
        state: 0,
        ..release
    };
    let _dropped =
        pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, press, true, false);
    let _ = read_all_available(&mut grab_peer);
    let _ = read_all_available(&mut other_peer);
    grab_peer
        .set_nonblocking(true)
        .expect("restore grab peer mode");
    other_peer
        .set_nonblocking(true)
        .expect("restore other peer mode");

    let _dropped =
        pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, release, true, false);

    let mut buf = [0u8; 256];

    // The window under the cursor must NOT get the XI2 release — the
    // active grab owns the device.
    let other_read = other_peer.read(&mut buf);
    assert!(
        matches!(other_read, Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock),
        "active device grab must withhold the XI2 ButtonRelease from the window under the cursor; got {other_read:?}",
    );

    // The grab owner must receive the XI2 ButtonRelease. The grab owner
    // also gets the parallel core ButtonRelease (32 bytes) from the
    // core redirect, so scan event boundaries for the XI2 GenericEvent.
    let n = match grab_peer.read(&mut buf) {
        Ok(n) => n,
        Err(e) => panic!("grab owner must receive the grabbed XI2 release; got {e:?}"),
    };
    let off = (0..n)
        .step_by(32)
        .find(|&off| off + 10 <= n && buf[off] == 35)
        .expect("grab owner must receive an XI2 GenericEvent (got core-only stream)");
    assert_eq!(
        u16::from_le_bytes([buf[off + 8], buf[off + 9]]),
        5,
        "the grabbed XI2 event must be a ButtonRelease (evtype 5)",
    );
}

/// A *core* `XGrabPointer` grab must NOT funnel XI2 XGE events to
/// the grab owner — the owner asked for core delivery only (Xorg's
/// `DeliverGrabbedEvent` consults the grab's own xi2mask, which is
/// empty for core grabs). Regression: the XI2 active-grab redirect
/// pushed the grab client unconditionally; a plain-Xlib client that
/// linked libXi without calling XIQueryVersion (every xts5 Xlib11
/// TCM) NULL-derefs inside libXi's wire handler on the XGE event,
/// and TET's longjmp out of the SIGSEGV poisons the display mutex —
/// the rest of the test case hangs forever (Xlib11/ButtonPress
/// scenario 890-0 on eiger HW, 2026-06-04).
#[test]
fn core_pointer_grab_does_not_send_xi2_to_grab_owner() {
    use crate::{
        backend::Backend,
        host_x11::{HostPointerEvent, PointerEventKind},
        resources::ROOT_VISUAL,
        server::ActivePointerGrab,
    };

    const GRAB_CLIENT_ID: u32 = 1;
    const GRAB_WIN: u32 = 0x0010_0021;
    const HOST_XID: u32 = 0xCAFE_0016;

    let mut state = ServerState::new();
    let mut grab_peer = install_client(&mut state, GRAB_CLIENT_ID);
    let mut backend = RecordingBackend::new();
    grab_peer.set_nonblocking(true).unwrap();

    state.resources.create_window(
        ClientId(GRAB_CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(GRAB_WIN),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 50,
            height: 50,
            border_width: 0,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(GRAB_WIN));
    // Select core ButtonPress on the grab window so the core
    // redirect has a delivery target.
    state
        .clients
        .get_mut(&GRAB_CLIENT_ID)
        .expect("grab client")
        .event_masks
        .insert(ResourceId(GRAB_WIN), 0x0000_0004);

    // Core XGrabPointer, owner_events=false — the xts5 TP10 shape.
    state.active_pointer_grab = Some(ActivePointerGrab {
        owner: ClientId(GRAB_CLIENT_ID),
        grab_window: ResourceId(GRAB_WIN),
        event_mask: 0xFFFF,
        cursor: ResourceId(0),
        time: 0,
        owner_events: false,
        via_xi2: false,
        implicit: false,
        passive: false,
        xi2_mask: 0,
    });

    Backend::register_top_level(&mut backend, None, ResourceId(GRAB_WIN), HOST_XID)
        .expect("register host xid");

    let press = HostPointerEvent {
        origin: crate::core_loop::message::InputOrigin::XTest(4),
        kind: PointerEventKind::ButtonPress,
        host_xid: HOST_XID,
        detail: 1,
        time: 0x3000,
        root_x: 25,
        root_y: 25,
        event_x: 25,
        event_y: 25,
        state: 0,
        crossing_mode: 0,
        child: 0,
        raw_dx: 0,
        raw_dy: 0,
        tree_change: false,
    };
    let xid_map = backend.xid_map().clone();
    let _dropped =
        pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, press, true, false);

    // The grab owner gets the core ButtonPress (type 4) and nothing
    // else — scan every 32-byte event for an XI2 GenericEvent (35).
    let mut buf = [0u8; 512];
    let n = match grab_peer.read(&mut buf) {
        Ok(n) => n,
        Err(e) => panic!("grab owner must receive the core press; got {e:?}"),
    };
    let mut saw_core_press = false;
    for off in (0..n).step_by(32) {
        assert_ne!(
            buf[off], 35,
            "core grab owner must not receive XI2 GenericEvents",
        );
        if buf[off] == 4 {
            saw_core_press = true;
        }
    }
    assert!(saw_core_press, "core ButtonPress must reach the grab owner");
}

/// `XIQueryPointer` must report currently-held pointer buttons in
/// the XI2 `buttons` bitmask, not only in the legacy KeyButMask
/// `effective_mods`. A GTK4 CSD window move goes through
/// `_NET_WM_MOVERESIZE`: the app sends the request and muffin — which
/// never saw the ButtonPress — queries the pointer to learn whether
/// the initiating button is still held. With an empty `buttons` mask
/// muffin concluded the button was released and aborted the move
/// grab immediately (the window "didn't budge"). Button N maps to
/// bit N (button 1 -> bit 1).
#[test]
fn xi_query_pointer_reports_held_button_in_xi2_button_mask() {
    use crate::{backend::Backend, resources::ROOT_VISUAL};
    use yserver_protocol::x11::ClientByteOrder;

    const CLIENT_ID: u32 = 1;
    const WINDOW_XID: u32 = 0x0020_0070;
    const HOST_XID: u32 = 0xCAFE_0007;

    // Run for both client byte orders. The XI2 button mask is a raw
    // byte array indexed by `XIMaskIsSet` (X11/extensions/XI2.h), so it
    // must NOT be byte-swapped per client order — a big-endian client
    // must still read button 1 in mask byte 0.
    for order in [ClientByteOrder::LittleEndian, ClientByteOrder::BigEndian] {
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, CLIENT_ID);
        peer.set_nonblocking(true).unwrap();
        state.clients.get_mut(&CLIENT_ID).unwrap().byte_order = order;
        let mut backend = RecordingBackend::new();
        // Model button 1 held (KeyButMask Button1Mask = 0x0100).
        backend.query_pointer_mask = 0x0100;

        state.resources.create_window(
            yserver_protocol::x11::ClientId(CLIENT_ID),
            yserver_protocol::x11::CreateWindowRequest {
                depth: 24,
                window: ResourceId(WINDOW_XID),
                parent: ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 100,
                height: 100,
                border_width: 0,
                class: 1,
                visual: ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = state.resources.map_window(ResourceId(WINDOW_XID));
        Backend::register_top_level(&mut backend, None, ResourceId(WINDOW_XID), HOST_XID)
            .expect("register host xid");

        // XIQueryPointer body: window(4) + deviceid(2) + pad(2).
        let mut body = Vec::with_capacity(8);
        body.extend_from_slice(&WINDOW_XID.to_le_bytes());
        body.extend_from_slice(&2u16.to_le_bytes()); // master pointer
        body.extend_from_slice(&0u16.to_le_bytes()); // pad

        let header = yserver_protocol::x11::RequestHeader {
            opcode: 131,
            data: 40, // XIQueryPointer
            length_units: 3,
        };
        handle_xi2_request(
            &mut state,
            &mut backend,
            None,
            ClientId(CLIENT_ID),
            SequenceNumber(1),
            header,
            &body,
        )
        .expect("xi query pointer");

        let mut buf = [0u8; 128];
        let n = peer.read(&mut buf).expect("reply");
        // Reply: 8-byte header + 24 coord bytes; same_screen@32, pad@33,
        // buttons_len@34-35, mods@36-51, group@52-55, button mask@56+.
        assert!(
            n >= 60,
            "{order:?}: XIQueryPointer reply must carry a 1-unit button bitmask (got {n})",
        );
        let buttons_len = match order {
            ClientByteOrder::LittleEndian => u16::from_le_bytes([buf[34], buf[35]]),
            ClientByteOrder::BigEndian => u16::from_be_bytes([buf[34], buf[35]]),
        };
        assert_eq!(
            buttons_len, 1,
            "{order:?}: buttons_len must be 1 so clients can read button state",
        );
        // Byte-indexed exactly as `XIMaskIsSet(mask, 1) = mask[1>>3] &
        // (1 << (1 & 7)) = mask[0] & 0x02` (the way muffin reads it),
        // which is byte-order independent.
        assert_ne!(
            buf[56] & (1 << 1),
            0,
            "{order:?}: button 1 must be held in XI2 button mask byte 0 \
                 (raw byte array, not byte-swapped); muffin's \
                 _NET_WM_MOVERESIZE move aborts on an empty mask",
        );
    }
}

/// Cinnamon alt-tab regression #2 (2026-06-10): `XIQueryPointer`
/// must report the keyboard modifier state in `ModifierInfo` —
/// Xorg `Xi/xiquerypointer.c:120,139` fills `rep.mods` from the
/// paired MASTER_KEYBOARD's XKB state. The backend mask is a core
/// KeyButMask (modifiers in the low byte, buttons at 0x100+);
/// `base_mods` must carry the modifier bits WITHOUT the button
/// bits. Pre-fix `base_mods` was hardcoded 0 — cinnamon's
/// `global.get_pointer()` (GDK → XIQueryPointer) saw "Alt not
/// held" mid-alt-tab and the switcher took the modifier-already-
/// released branch: `_activateSelected()` + destroy, so the popup
/// never appeared (trace: instant raise+SetInputFocus between the
/// modal grab and ungrab, same server timestamp).
#[test]
fn xi_query_pointer_reports_keyboard_mods_in_modifier_info() {
    use yserver_protocol::x11::ClientByteOrder;

    const CLIENT_ID: u32 = 1;
    for order in [ClientByteOrder::LittleEndian, ClientByteOrder::BigEndian] {
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, CLIENT_ID);
        peer.set_nonblocking(true).unwrap();
        state.clients.get_mut(&CLIENT_ID).unwrap().byte_order = order;
        let mut backend = RecordingBackend::new();
        // Alt (Mod1Mask = 0x8) held + button 1 held (Button1Mask =
        // 0x100): the reply must separate them.
        backend.query_pointer_mask = 0x0108;

        // XIQueryPointer body: window(4) + deviceid(2) + pad(2).
        let mut body = Vec::with_capacity(8);
        body.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
        body.extend_from_slice(&2u16.to_le_bytes()); // master pointer
        body.extend_from_slice(&0u16.to_le_bytes()); // pad
        let header = yserver_protocol::x11::RequestHeader {
            opcode: 131,
            data: 40, // XIQueryPointer
            length_units: 3,
        };
        handle_xi2_request(
            &mut state,
            &mut backend,
            None,
            ClientId(CLIENT_ID),
            SequenceNumber(1),
            header,
            &body,
        )
        .expect("xi query pointer");

        let mut buf = [0u8; 128];
        let n = peer.read(&mut buf).expect("reply");
        assert!(n >= 56, "{order:?}: short XIQueryPointer reply ({n})");
        // ModifierInfo at bytes 36..52: base / latched / locked /
        // effective (each CARD32, client byte order).
        let read_u32 = |b: &[u8]| match order {
            ClientByteOrder::LittleEndian => u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            ClientByteOrder::BigEndian => u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
        };
        let base_mods = read_u32(&buf[36..40]);
        let effective_mods = read_u32(&buf[48..52]);
        assert_eq!(
            base_mods, 0x8,
            "{order:?}: base_mods must carry the held Mod1 (Alt) and \
                 not the button bits (Xorg fills it from the paired \
                 keyboard's XKB state)",
        );
        assert_ne!(
            effective_mods & 0x8,
            0,
            "{order:?}: effective_mods must include the held Mod1 — \
                 GDK reads the effective field for global.get_pointer()",
        );
    }
}

/// Cinnamon alt-tab regression #1 (2026-06-10): a SYNC passive key
/// grab (the muffin keybinding) activates on Alt+Tab and freezes
/// the keyboard; muffin then takes an ASYNC active `XIGrabDevice`
/// (pushModal) and later `XIUngrabDevice`s (popModal). Per Xorg the
/// async activation THAWS the frozen device — `ActivateKeyboardGrab`
/// → `CheckGrabForSyncs` (dix/events.c:1424: async → THAWED) — and
/// deactivation runs `ComputeFreezes` (`DeactivateKeyboardGrab`:
/// sync.state = NOT_GRABBED). The XIAllowEvents muffin sends after
/// the ungrab is then a no-op on an already-thawed device. Pre-fix
/// yserver's XI2 grab/ungrab never touched the sync state, so the
/// freeze outlived the whole modal cycle and EVERY later key event
/// was withheld — one alt-tab permanently killed keyboard input
/// (server log: "AllowEvents no-op ... state=FrozenNoEvent";
/// trace: zero key events after the alt-tab cluster).
#[test]
fn async_xi2_grab_and_ungrab_thaw_sync_passive_key_freeze() {
    use crate::{
        core_loop::key_fanout::key_event_fanout_to_state, host_x11::HostKeyEvent, server::KeyGrab,
        xinput::DEVICEID_MASTER_KEYBOARD,
    };

    const APP_WIN: u32 = 0x0030_0001;
    const APP: u32 = 9;
    const WM: u32 = 7;
    let key_event = |pressed: bool, keycode: u8| HostKeyEvent {
        origin: crate::core_loop::InputOrigin::NestedHost,
        pressed,
        keycode,
        time: 1,
        root_x: 10,
        root_y: 20,
        event_x: 10,
        event_y: 20,
        state: 0,
    };

    let mut state = ServerState::new();
    let mut app = install_client(&mut state, APP);
    app.set_nonblocking(true).unwrap();
    let _wm = install_client(&mut state, WM);
    let mut backend = RecordingBackend::new();

    // Focused app window with a core KeyPress selection.
    state.resources.create_window(
        yserver_protocol::x11::ClientId(APP),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(APP_WIN),
            parent: ROOT_WINDOW,
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
    let _ = state.resources.map_window(ResourceId(APP_WIN));
    state
        .clients
        .get_mut(&APP)
        .unwrap()
        .event_masks
        .insert(ResourceId(APP_WIN), 0x1); // KeyPressMask
    state.core_focus.raw = APP_WIN;

    // muffin keybinding: SYNC passive key grab on Tab (keycode 23).
    state.key_grabs.push(KeyGrab {
        device_id: 0,
        owner: ClientId(WM),
        grab_window: ROOT_WINDOW,
        keycode: 23,
        modifiers: 0,
        owner_events: false,
        pointer_mode: 1,
        keyboard_mode: 0, // synchronous → freeze
        via_xi2: true,
        xi2_mask: 0,
    });

    // 1. Alt+Tab press: passive grab activates + keyboard freezes.
    let _ = key_event_fanout_to_state(&mut state, &mut backend, key_event(true, 23));
    assert!(
        state
            .xi1_frozen
            .get(&crate::xinput::DEVICEID_MASTER_KEYBOARD)
            .and_then(|f| f.stored.as_ref())
            .is_some(),
        "sync passive grab must freeze the activating press",
    );

    // 2. pushModal: ASYNC XIGrabDevice on the keyboard. Body per
    //    xXIGrabDeviceReq: window(4) time(4) cursor(4) deviceid(2)
    //    mode(1) paired(1) owner_events(1) pad(1) mask_len(2).
    let mut grab_body = Vec::with_capacity(18);
    grab_body.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    grab_body.extend_from_slice(&0u32.to_le_bytes()); // time = CurrentTime
    grab_body.extend_from_slice(&0u32.to_le_bytes()); // cursor = None
    grab_body.extend_from_slice(&3u16.to_le_bytes()); // master keyboard
    grab_body.push(1); // grab_mode = XIGrabModeAsync
    grab_body.push(1); // paired_device_mode = Async
    grab_body.push(0); // owner_events = false
    grab_body.push(0); // pad
    grab_body.extend_from_slice(&0u16.to_le_bytes()); // mask_len = 0
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(WM),
        SequenceNumber(2),
        yserver_protocol::x11::RequestHeader {
            opcode: 131,
            data: 51, // XIGrabDevice
            length_units: 6,
        },
        &grab_body,
    )
    .expect("xi grab device");
    assert!(
        state
            .xi1_frozen
            .get(&crate::xinput::DEVICEID_MASTER_KEYBOARD)
            .and_then(|f| f.stored.as_ref())
            .is_none(),
        "CheckGrabForSyncs: an ASYNC active grab must thaw the \
             sync-passive-grab freeze (Xorg dix/events.c:1431)",
    );
    assert!(
        !state
            .xi1_frozen
            .get(&DEVICEID_MASTER_KEYBOARD)
            .is_some_and(crate::server::Xi1Freeze::frozen),
        "keyboard sync state must be THAWED after the async grab",
    );

    // 3. popModal: XIUngrabDevice. Body: time(4) deviceid(2) pad(2).
    let mut ungrab_body = Vec::with_capacity(8);
    ungrab_body.extend_from_slice(&0u32.to_le_bytes());
    ungrab_body.extend_from_slice(&3u16.to_le_bytes());
    ungrab_body.extend_from_slice(&0u16.to_le_bytes());
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(WM),
        SequenceNumber(3),
        yserver_protocol::x11::RequestHeader {
            opcode: 131,
            data: 52, // XIUngrabDevice
            length_units: 3,
        },
        &ungrab_body,
    )
    .expect("xi ungrab device");

    // 4. The keyboard must still deliver: a fresh key press reaches
    //    the focused window's client.
    let mut drain = [0u8; 512];
    while app.read(&mut drain).is_ok_and(|n| n > 0) {}
    let _ = key_event_fanout_to_state(&mut state, &mut backend, key_event(true, 38));
    let mut buf = [0u8; 64];
    let n = app.read(&mut buf).unwrap_or(0);
    assert!(
        n >= 32,
        "keyboard must keep delivering after the modal grab cycle \
             (got {n} bytes) — a leaked freeze kills all key input",
    );
    assert_eq!(buf[0] & 0x7f, 2, "must be a core KeyPress");
}

/// `XIGetClientPointer` reply must place `deviceid` at bytes 10-11.
/// Per `xXIGetClientPointerReply` (X11/extensions/XI2proto.h):
/// `set` at byte 8, `pad0` at byte 9, `deviceid` (u16) at bytes
/// 10-11. Regression: the encoder wrote 3 pad bytes after `set`,
/// pushing deviceid to bytes 12-13, so clients read deviceid=0.
/// nemo's desktop rubber-band asks `XIGetClientPointer` for its
/// pointer device, got 0, and queried that for the band anchor —
/// anchoring it at (0,0). caja never calls it, so it was immune.
#[test]
fn xi_get_client_pointer_reply_deviceid_offset() {
    const CLIENT_ID: u32 = 1;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT_ID);
    peer.set_nonblocking(true).unwrap();
    let mut backend = RecordingBackend::new();

    // XIGetClientPointer body: window(4).
    let body = 0u32.to_le_bytes().to_vec();
    let header = yserver_protocol::x11::RequestHeader {
        opcode: 131,
        data: 45, // XIGetClientPointer
        length_units: 2,
    };
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(CLIENT_ID),
        SequenceNumber(1),
        header,
        &body,
    )
    .expect("xi get client pointer");

    let mut buf = [0u8; 64];
    let n = peer.read(&mut buf).expect("reply");
    assert_eq!(n, 32, "reply must be 32 bytes");
    assert_eq!(buf[8], 1, "byte 8: set = True");
    assert_eq!(
        u16::from_le_bytes([buf[10], buf[11]]),
        2,
        "deviceid (bytes 10-11 per xXIGetClientPointerReply) must be the \
             master pointer (2); a wrong offset reads as 0 and breaks nemo's \
             pointer-device lookup for the rubber-band anchor",
    );
}

/// XI2 `FocusIn`/`FocusOut` events share the `xXIEnterEvent` layout
/// and must carry the current pointer position (Xorg populates
/// root_x/root_y/event_x/event_y from it). yserver emits focus events
/// from request handlers that have no backend handle, so it reads the
/// cached `state.pointer_root`. Regression target: focus events
/// shipped at (0,0).
#[test]
fn xi_focus_in_carries_cached_pointer_position() {
    use crate::resources::ROOT_VISUAL;

    const CLIENT_ID: u32 = 1;
    const FOCUS_WIN: u32 = 0x0010_0007;
    const PREV_WIN: u32 = 0x0010_0008;
    const XI_FOCUS_IN: u16 = 9;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT_ID);
    peer.set_nonblocking(true).unwrap();

    for xid in [FOCUS_WIN, PREV_WIN] {
        state.resources.create_window(
            yserver_protocol::x11::ClientId(CLIENT_ID),
            yserver_protocol::x11::CreateWindowRequest {
                depth: 24,
                window: ResourceId(xid),
                parent: ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 100,
                height: 100,
                border_width: 0,
                class: 1,
                visual: ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = state.resources.map_window(ResourceId(xid));
    }
    // Client selects XI_FocusIn on FOCUS_WIN (keyboard device 3).
    state
        .clients
        .get_mut(&CLIENT_ID)
        .unwrap()
        .xi2_masks
        .insert((ResourceId(FOCUS_WIN), 3), 1 << XI_FOCUS_IN);

    // Cache the pointer position; prior focus is on an unrelated window
    // so a Focus crossing actually fires onto FOCUS_WIN.
    state.pointer_root = (631, 641);
    state.core_focus = crate::server::CoreFocus {
        raw: PREV_WIN,
        revert_to: 0,
        time: 0,
    };

    emit_core_focus_transition(&mut state, PREV_WIN, FOCUS_WIN, 0);
    state.core_focus.raw = FOCUS_WIN;

    let mut buf = [0u8; 128];
    let n = peer.read(&mut buf).expect("focus event");
    assert!(n >= 36, "expected an XI2 focus event (got {n} bytes)");
    assert_eq!(buf[0], 35, "must be an XI2 GenericEvent");
    assert_eq!(
        u16::from_le_bytes([buf[8], buf[9]]),
        XI_FOCUS_IN,
        "evtype must be FocusIn",
    );
    // root_x is FP1616 at bytes 32-35 (after the 32-byte boundary).
    let root_x_fp = u32::from_le_bytes([buf[32], buf[33], buf[34], buf[35]]);
    assert_eq!(
        root_x_fp,
        (631i32 << 16) as u32,
        "FocusIn must carry the current pointer X (631), not 0; Xorg \
             populates focus-event coords from the pointer position",
    );
}

/// A sync passive button grab that also froze the keyboard (dtwm's
/// front panel: GrabModeSync for both) and is let go by
/// `AllowEvents(ReplayPointer)`: Xorg DeactivatePointerGrab clears the
/// keyboard's hold with the grab, so typing works again (measured:
/// tools/vng-scenarios/goldens/passive-grab.txt, last line).
#[test]
fn replay_pointer_of_passive_grab_thaws_keyboard_it_froze() {
    const GRAB_CLIENT_ID: u32 = 1;
    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    let _grab_peer = install_client(&mut state, GRAB_CLIENT_ID);
    state.set_pointer_grab(crate::server::ActivePointerGrab {
        owner: ClientId(GRAB_CLIENT_ID),
        grab_window: ROOT_WINDOW,
        event_mask: 0x0008,
        cursor: ResourceId(0),
        time: 0,
        owner_events: false,
        via_xi2: false,
        implicit: false,
        passive: true,
        xi2_mask: 0,
    });
    crate::core_loop::pointer_fanout::xi1_check_grab_for_syncs(
        &mut state,
        crate::xinput::DEVICEID_MASTER_POINTER,
        ClientId(GRAB_CLIENT_ID),
        true,
        true,
    );
    state
        .xi1_frozen
        .entry(crate::xinput::DEVICEID_MASTER_POINTER)
        .or_default()
        .state = crate::server::Xi1SyncState::FrozenWithEvent;
    let kbd_frozen = |state: &ServerState| {
        state
            .xi1_frozen
            .get(&crate::xinput::DEVICEID_MASTER_KEYBOARD)
            .is_some_and(crate::server::Xi1Freeze::frozen)
    };
    assert!(
        kbd_frozen(&state),
        "the grab's sync keyboard mode holds the keyboard"
    );

    let header = yserver_protocol::x11::RequestHeader {
        opcode: 35,
        data: 2, // ReplayPointer
        length_units: 2,
    };
    handle_allow_events(
        &mut state,
        &mut backend,
        ClientId(GRAB_CLIENT_ID),
        SequenceNumber(1),
        header,
        &[0u8; 4],
    )
    .expect("allow events replay pointer");

    assert!(state.active_pointer_grab.is_none());
    assert!(!kbd_frozen(&state), "the keyboard thaws with the grab");
}

/// Core `AllowEvents(ReplayKeyboard)` (mode 5 — what muffin/mutter
/// calls for a declined key) releases the passive keyboard grab and
/// replays the frozen key to the focused window. This is the
/// dead-`p`-in-wezterm fix end-to-end: a sync passive key grab held
/// `p`, the WM declined it, and ReplayKeyboard hands it to the
/// focused client.
#[test]
fn core_replay_keyboard_releases_grab_and_replays_to_focus() {
    use crate::{
        host_x11::HostKeyEvent,
        server::{ActiveKeyboardGrab, ActiveKeyboardGrabSource},
    };

    const GRAB_CLIENT_ID: u32 = 1;
    const FOCUS_CLIENT_ID: u32 = 2;
    const FOCUS_WIN: u32 = 0x0020_0061;
    const KEY_PRESS_MASK: u32 = 0x0000_0001;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    let _grab_peer = install_client(&mut state, GRAB_CLIENT_ID);
    let mut focus_peer = install_client(&mut state, FOCUS_CLIENT_ID);
    focus_peer.set_nonblocking(true).unwrap();

    // Focused client selects core KeyPress on its window.
    {
        let c = state.clients.get_mut(&FOCUS_CLIENT_ID).unwrap();
        c.focused_window = ResourceId(FOCUS_WIN);
        c.event_masks.insert(ResourceId(FOCUS_WIN), KEY_PRESS_MASK);
    }
    state.core_focus.raw = FOCUS_WIN;

    // A sync passive key grab held by the WM is active, with a
    // frozen press waiting.
    state.active_keyboard_grab = Some(ActiveKeyboardGrab {
        owner: ClientId(GRAB_CLIENT_ID),
        grab_window: ROOT_WINDOW,
        owner_events: false,
        source: ActiveKeyboardGrabSource::PassiveKey { keycode: 33 },
        via_xi2: true,
        xi2_mask: 0,
    });
    {
        let f = state
            .xi1_frozen
            .entry(crate::xinput::DEVICEID_MASTER_KEYBOARD)
            .or_default();
        f.state = crate::server::Xi1SyncState::FrozenWithEvent;
        f.stored = Some(crate::server::QueuedInputEvent::HostKey(HostKeyEvent {
            origin: crate::core_loop::InputOrigin::NestedHost,
            pressed: true,
            keycode: 33,
            time: 0x1234,
            root_x: 10,
            root_y: 20,
            event_x: 10,
            event_y: 20,
            state: 0,
        }));
    }

    let header = yserver_protocol::x11::RequestHeader {
        opcode: 35,
        data: 5, // ReplayKeyboard
        length_units: 2,
    };
    handle_allow_events(
        &mut state,
        &mut backend,
        ClientId(GRAB_CLIENT_ID),
        SequenceNumber(1),
        header,
        &[0u8; 4],
    )
    .expect("allow events replay keyboard");

    assert!(
        state.active_keyboard_grab.is_none(),
        "ReplayKeyboard must release the passive keyboard grab"
    );
    assert!(
        state
            .xi1_frozen
            .get(&crate::xinput::DEVICEID_MASTER_KEYBOARD)
            .and_then(|f| f.stored.as_ref())
            .is_none(),
        "the frozen key must be consumed by the replay"
    );
    let mut buf = [0u8; 64];
    let n = focus_peer.read(&mut buf).unwrap_or(0);
    assert!(
        n >= 32 && buf[0] == 2,
        "replayed KeyPress (event type 2) must reach the focused client; got n={n} type={}",
        buf[0]
    );
}

/// GH #59 regression (bspwm/sxhkd dead keyboard). sxhkd holds a
/// SYNCHRONOUS core passive key grab; after the activating chord
/// press freezes the keyboard it issues exactly ONE
/// AllowEvents(SyncKeyboard) and waits. The terminating key release
/// must reach `key_route` (device still FreezeNextEvent, NOT
/// re-frozen), deactivate the passive grab, and thaw — so typing
/// reaches the focused window. Pre-fix the SyncKeyboard
/// `FreezeNextEvent` allowance was consumed by replaying the grab's
/// OWN already-delivered activating press (double-booked in the XI1
/// queue), re-freezing to FrozenWithEvent; the release was then
/// withheld, the grab never deactivated, and the keyboard was dead.
/// Xorg's `ComputeFreezes` only replays the stored event for
/// `Replay*`, not `Sync*` (dix/events.c:1320-1370).
#[test]
fn sync_passive_kbd_grab_synckeyboard_release_thaws() {
    use crate::{
        core_loop::key_fanout::key_event_fanout_to_state,
        host_x11::HostKeyEvent,
        server::{KeyGrab, Xi1SyncState},
    };

    const GRAB_CLIENT_ID: u32 = 1; // sxhkd
    const FOCUS_CLIENT_ID: u32 = 2; // kitty
    const FOCUS_WIN: u32 = 0x0020_0091;
    const KEY_PRESS_MASK: u32 = 0x0000_0001;
    const RETURN_KC: u8 = 36;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    let _grab_peer = install_client(&mut state, GRAB_CLIENT_ID);
    let mut focus_peer = install_client(&mut state, FOCUS_CLIENT_ID);
    focus_peer.set_nonblocking(true).unwrap();
    {
        let c = state.clients.get_mut(&FOCUS_CLIENT_ID).unwrap();
        c.focused_window = ResourceId(FOCUS_WIN);
        c.event_masks.insert(ResourceId(FOCUS_WIN), KEY_PRESS_MASK);
    }
    state.core_focus.raw = FOCUS_WIN;

    // sxhkd's synchronous core passive grab on the chord key.
    state.key_grabs.push(KeyGrab {
        device_id: 0,
        owner: ClientId(GRAB_CLIENT_ID),
        grab_window: ROOT_WINDOW,
        keycode: RETURN_KC,
        modifiers: 0,
        owner_events: true,
        pointer_mode: 1,
        keyboard_mode: 0, // synchronous → freeze
        via_xi2: false,
        xi2_mask: 0,
    });

    let key = |pressed, keycode| HostKeyEvent {
        origin: crate::core_loop::InputOrigin::NestedHost,
        pressed,
        keycode,
        time: 1,
        root_x: 0,
        root_y: 0,
        event_x: 0,
        event_y: 0,
        state: 0,
    };

    // 1) chord press activates the grab and freezes the device.
    let _ = key_event_fanout_to_state(&mut state, &mut backend, key(true, RETURN_KC));
    assert!(
        state.active_keyboard_grab.is_some(),
        "synchronous passive grab must activate on the chord press"
    );

    // 2) sxhkd issues a single AllowEvents(SyncKeyboard) (mode 4).
    let header = yserver_protocol::x11::RequestHeader {
        opcode: 35,
        data: 4, // SyncKeyboard
        length_units: 2,
    };
    handle_allow_events(
        &mut state,
        &mut backend,
        ClientId(GRAB_CLIENT_ID),
        SequenceNumber(1),
        header,
        &[0u8; 4],
    )
    .expect("allow events sync keyboard");

    // 3) the terminating release must deactivate the grab and thaw.
    let _ = key_event_fanout_to_state(&mut state, &mut backend, key(false, RETURN_KC));
    assert!(
        state.active_keyboard_grab.is_none(),
        "the matching key release must deactivate the passive grab"
    );
    let frozen = state
        .xi1_frozen
        .get(&crate::xinput::DEVICEID_XTEST_KEYBOARD)
        .map_or(Xi1SyncState::Thawed, |f| f.state);
    assert_eq!(
        frozen,
        Xi1SyncState::Thawed,
        "the terminating release must thaw the keyboard (not leave it FrozenWithEvent)"
    );

    // 4) typing (an ungrabbed key) must now reach the focused window.
    let _ = key_event_fanout_to_state(&mut state, &mut backend, key(true, 38));
    let mut buf = [0u8; 64];
    let n = focus_peer.read(&mut buf).unwrap_or(0);
    assert!(
        n >= 32 && buf[0] == 2,
        "after the grab releases, typing must reach the focused window; got n={n} type={}",
        buf[0]
    );
}

/// Regression (codex review): ReplayKeyboard must release an
/// EXPLICIT GrabKeyboard(GrabModeSync), not just a passive key
/// grab — Xorg NOT_GRABBED calls DeactivateGrab regardless of
/// grab kind (dix/events.c:1898). Pre-fix the grab stayed active
/// after the replay → stuck keyboard.
#[test]
fn core_replay_keyboard_releases_explicit_sync_grab() {
    use crate::{
        host_x11::HostKeyEvent,
        server::{ActiveKeyboardGrab, ActiveKeyboardGrabSource},
    };

    const GRAB_CLIENT_ID: u32 = 1;
    const FOCUS_WIN: u32 = 0x0020_0071;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    let _grab_peer = install_client(&mut state, GRAB_CLIENT_ID);
    state.core_focus.raw = FOCUS_WIN;

    // Explicit GrabKeyboard(GrabModeSync): active grab + the
    // device frozen-no-event, with a key withheld in the freeze
    // queue (the "with event" condition for replay).
    state.active_keyboard_grab = Some(ActiveKeyboardGrab {
        owner: ClientId(GRAB_CLIENT_ID),
        grab_window: ROOT_WINDOW,
        owner_events: false,
        source: ActiveKeyboardGrabSource::Explicit,
        via_xi2: false,
        xi2_mask: 0,
    });
    {
        let f = state
            .xi1_frozen
            .entry(crate::xinput::DEVICEID_MASTER_KEYBOARD)
            .or_default();
        f.state = crate::server::Xi1SyncState::FrozenNoEvent;
        state
            .sync_pending
            .push_back(crate::server::PendingSyncEvent {
                device: crate::xinput::DEVICEID_MASTER_KEYBOARD,
                event: crate::server::QueuedInputEvent::HostKey(HostKeyEvent {
                    origin: crate::core_loop::InputOrigin::NestedHost,
                    pressed: true,
                    keycode: 38,
                    time: 0x2222,
                    root_x: 1,
                    root_y: 2,
                    event_x: 1,
                    event_y: 2,
                    state: 0,
                }),
            });
    }

    let header = yserver_protocol::x11::RequestHeader {
        opcode: 35,
        data: 5, // ReplayKeyboard
        length_units: 2,
    };
    handle_allow_events(
        &mut state,
        &mut backend,
        ClientId(GRAB_CLIENT_ID),
        SequenceNumber(1),
        header,
        &[0u8; 4],
    )
    .expect("allow events replay keyboard");

    assert!(
        state.active_keyboard_grab.is_none(),
        "ReplayKeyboard must deactivate the explicit sync keyboard grab",
    );
}
