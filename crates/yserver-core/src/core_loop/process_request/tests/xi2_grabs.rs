use super::*;

/// `XIGrabDevice` must wire into the existing core X11 grab
/// state (`state.active_pointer_grab` for
/// the master pointer, `state.active_keyboard_grab` for the
/// master keyboard) so the pointer/key fanout's
/// `active_grab_target` routing kicks in. Pre-fix the handler
/// was a no-op that just sent Success — GTK thought it owned
/// the device but events still went to whatever window the
/// pointer was over, so popups dismissed on the first stray
/// motion and `gtk_window_present` looped re-mapping at ~50 Hz.
#[test]
fn xi_grab_device_sets_active_grab_state() {
    const CLIENT_ID: u32 = 1;
    const WINDOW_XID: u32 = 0x0010_0050;
    const CURSOR_XID: u32 = 0x0090_0050;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, CLIENT_ID);
    let mut backend = RecordingBackend::new();

    // Pointer grab (deviceid=2) — XIGrabDevice body: window(4)
    // time(4) cursor(4) deviceid(2) mode(1) paired(1)
    // owner_events(1) pad(1) mask_len(2) ...
    let mut body = Vec::with_capacity(24);
    body.extend_from_slice(&WINDOW_XID.to_le_bytes()); // window
    body.extend_from_slice(&0u32.to_le_bytes()); // time
    body.extend_from_slice(&CURSOR_XID.to_le_bytes()); // cursor
    body.extend_from_slice(&2u16.to_le_bytes()); // deviceid (pointer)
    body.extend_from_slice(&[1, 1, 1, 0]); // mode async, paired async, owner_events=1, pad
    body.extend_from_slice(&0u16.to_le_bytes()); // mask_len
    body.extend_from_slice(&[0u8; 2]); // pad

    let header = yserver_protocol::x11::RequestHeader {
        opcode: 131,
        data: 51, // XIGrabDevice
        length_units: 7,
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
    .expect("XIGrabDevice pointer");

    assert_eq!(
        state
            .active_pointer_grab
            .map(|grab| (grab.owner, grab.grab_window)),
        Some((ClientId(CLIENT_ID), ResourceId(WINDOW_XID))),
        "XIGrabDevice(pointer) must set active_pointer_grab so \
             pointer_fanout's active_grab_target redirect kicks in",
    );
    assert!(state.active_pointer_grab.is_some_and(|grab| !grab.passive));
    let active = state.active_pointer_grab.expect("active_pointer_grab set");
    assert_eq!(active.owner, ClientId(CLIENT_ID));
    assert_eq!(active.grab_window, ResourceId(WINDOW_XID));
    assert_eq!(active.cursor, ResourceId(CURSOR_XID));

    // Keyboard grab (deviceid=3) — same wire format.
    let mut body_kbd = Vec::with_capacity(24);
    body_kbd.extend_from_slice(&WINDOW_XID.to_le_bytes());
    body_kbd.extend_from_slice(&0u32.to_le_bytes());
    body_kbd.extend_from_slice(&0u32.to_le_bytes());
    body_kbd.extend_from_slice(&3u16.to_le_bytes()); // keyboard
    body_kbd.extend_from_slice(&[1, 1, 1, 0]);
    body_kbd.extend_from_slice(&0u16.to_le_bytes());
    body_kbd.extend_from_slice(&[0u8; 2]);
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(CLIENT_ID),
        SequenceNumber(2),
        header,
        &body_kbd,
    )
    .expect("XIGrabDevice keyboard");
    let kgrab = state
        .active_keyboard_grab
        .expect("active_keyboard_grab set");
    assert_eq!(kgrab.owner, ClientId(CLIENT_ID));
    assert_eq!(kgrab.grab_window, ResourceId(WINDOW_XID));

    // Ungrab both — XIUngrabDevice body: time(4) deviceid(2) pad(2).
    let ungrab_header = yserver_protocol::x11::RequestHeader {
        opcode: 131,
        data: 52,
        // XIUngrabDevice is Fixed(3): 4 header + time(4) + deviceid(2)
        // + pad(2) = 12 bytes = 3 units. (Was 2 — pre-dated the
        // XTS-driven REQUEST_SIZE_MATCH length gate, so the malformed
        // length slipped through; master now correctly BadLengths it.)
        length_units: 3,
    };
    let mut ungrab_p = Vec::with_capacity(8);
    ungrab_p.extend_from_slice(&0u32.to_le_bytes()); // time
    ungrab_p.extend_from_slice(&2u16.to_le_bytes()); // pointer
    ungrab_p.extend_from_slice(&[0u8; 2]);
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(CLIENT_ID),
        SequenceNumber(3),
        ungrab_header,
        &ungrab_p,
    )
    .expect("XIUngrabDevice pointer");
    assert!(state.active_pointer_grab.is_none());

    let mut ungrab_k = Vec::with_capacity(8);
    ungrab_k.extend_from_slice(&0u32.to_le_bytes());
    ungrab_k.extend_from_slice(&3u16.to_le_bytes());
    ungrab_k.extend_from_slice(&[0u8; 2]);
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(CLIENT_ID),
        SequenceNumber(4),
        ungrab_header,
        &ungrab_k,
    )
    .expect("XIUngrabDevice keyboard");
    assert!(
        state.active_keyboard_grab.is_none(),
        "keyboard grab cleared"
    );
}

fn process_dynamic_request(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    client_id: ClientId,
    sequence: SequenceNumber,
    opcode: u8,
    data: u8,
    body: &[u8],
) {
    let wire_bytes = body.len() + 4;
    assert_eq!(wire_bytes % 4, 0, "X11 request must be four-byte aligned");
    let length_units = u32::try_from(wire_bytes / 4).expect("request length fits CARD32");
    process_request(
        state,
        backend,
        client_id,
        sequence,
        RequestHeader {
            opcode,
            data,
            length_units,
        },
        body,
        None,
    )
    .expect("process_request dispatch");
}

fn process_xi_dynamic_request(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    client_id: ClientId,
    sequence: SequenceNumber,
    minor: u8,
    body: &[u8],
) {
    process_dynamic_request(state, backend, client_id, sequence, 137, minor, body);
}

fn xi_dynamic_wire_grab_body(
    window: u32,
    device_id: u16,
    grab_mode: u8,
    paired_device_mode: u8,
    owner_events: bool,
    mask: Option<u32>,
) -> Vec<u8> {
    let mut body = Vec::with_capacity(if mask.is_some() { 24 } else { 20 });
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // current time
    body.extend_from_slice(&0u32.to_le_bytes()); // no cursor
    body.extend_from_slice(&device_id.to_le_bytes());
    body.extend_from_slice(&[grab_mode, paired_device_mode, u8::from(owner_events), 0]);
    body.extend_from_slice(&u16::from(mask.is_some()).to_le_bytes()); // mask_len in 4-byte units
    if let Some(mask) = mask {
        body.extend_from_slice(&mask.to_le_bytes());
    }
    body
}

fn xi_dynamic_select_events_body(window: u32, device_id: u16, mask: u32) -> Vec<u8> {
    let mut body = Vec::with_capacity(16);
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes()); // one device mask
    body.extend_from_slice(&[0u8; 2]);
    body.extend_from_slice(&device_id.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes()); // one mask word
    body.extend_from_slice(&mask.to_le_bytes());
    body
}

fn xi_dynamic_pointer_event(
    origin: crate::core_loop::InputOrigin,
    kind: crate::host_x11::PointerEventKind,
    detail: u8,
    time: u32,
) -> crate::host_x11::HostPointerEvent {
    crate::host_x11::HostPointerEvent {
        origin,
        kind,
        host_xid: 0,
        detail,
        time,
        root_x: 10,
        root_y: 20,
        event_x: 10,
        event_y: 20,
        state: 0,
        crossing_mode: 0,
        child: 0,
        raw_dx: 0,
        raw_dy: 0,
        tree_change: false,
    }
}

fn xi_dynamic_event_ids(bytes: &[u8]) -> Vec<(u16, u16, u16)> {
    let mut result = Vec::new();
    let mut offset = 0;
    while offset + 32 <= bytes.len() {
        assert_eq!(bytes[offset], 35, "expected GenericEvent");
        let extra = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap());
        let evtype = u16::from_le_bytes(bytes[offset + 8..offset + 10].try_into().unwrap());
        let deviceid = u16::from_le_bytes(bytes[offset + 10..offset + 12].try_into().unwrap());
        let source_offset = if matches!(evtype, 13..=17) { 20 } else { 52 };
        let sourceid = u16::from_le_bytes(
            bytes[offset + source_offset..offset + source_offset + 2]
                .try_into()
                .unwrap(),
        );
        result.push((evtype, deviceid, sourceid));
        offset += 32 + extra as usize * 4;
    }
    assert_eq!(offset, bytes.len(), "event stream fully consumed");
    result
}

#[test]
fn xi_dynamic_grabs_xtest_release_keeps_master_passive_grab_until_master_release() {
    use crate::{
        backend::Backend, core_loop::pointer_fanout::pointer_event_fanout_to_state,
        host_x11::PointerEventKind,
    };

    const OWNER: u32 = 1;
    const SOURCE: u64 = 91;
    let mut state = ServerState::new();
    let _owner_peer = install_client(&mut state, OWNER);
    let mut backend = RecordingBackend::new();
    let (source_id, pointer_id) =
        xi_dynamic_grab_source(&mut state, SOURCE, false, true, "passive-owner-mouse");

    // Core GrabButton: asynchronous pointer, synchronous paired keyboard.
    let mut grab_button = Vec::with_capacity(20);
    grab_button.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    grab_button.extend_from_slice(&0x000cu16.to_le_bytes()); // ButtonPress|ButtonRelease
    grab_button.extend_from_slice(&[1, 0]); // pointer async, keyboard sync
    grab_button.extend_from_slice(&0u32.to_le_bytes()); // no confinement
    grab_button.extend_from_slice(&0u32.to_le_bytes()); // no cursor
    grab_button.extend_from_slice(&[1, 0]); // button 1, pad
    grab_button.extend_from_slice(&0u16.to_le_bytes()); // no modifiers
    process_dynamic_request(
        &mut state,
        &mut backend,
        ClientId(OWNER),
        SequenceNumber(1),
        28, // GrabButton
        0,  // owner_events=false
        &grab_button,
    );
    assert_eq!(state.button_grabs.len(), 1);

    let xid_map = backend.xid_map().clone();
    let _ = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &xid_map,
        xi_dynamic_pointer_event(
            crate::core_loop::InputOrigin::Physical(source_id),
            PointerEventKind::ButtonPress,
            1,
            1,
        ),
        true,
        false,
    );
    assert!(state.active_pointer_grab.is_some_and(|grab| grab.passive));
    assert_eq!(state.buttons_down, 1);
    assert_eq!(state.xi_devices.device(pointer_id).unwrap().buttons_down, 1);
    assert_eq!(
        state.xi1_frozen[&crate::xinput::DEVICEID_MASTER_KEYBOARD].other,
        Some(ClientId(OWNER)),
        "the core passive grab holds its paired master keyboard"
    );

    for kind in [
        PointerEventKind::ButtonPress,
        PointerEventKind::ButtonRelease,
    ] {
        let _ = pointer_event_fanout_to_state(
            &mut state,
            &mut backend,
            &xid_map,
            xi_dynamic_pointer_event(
                crate::core_loop::InputOrigin::XTest(crate::xinput::DEVICEID_XTEST_POINTER),
                kind,
                1,
                2,
            ),
            true,
            false,
        );
    }

    assert!(
        state.active_pointer_grab.is_some_and(|grab| grab.passive),
        "the XTEST release cannot end a passive grab on the master while a physical button remains down"
    );
    assert_eq!(
        state.buttons_down, 1,
        "the master still holds physical button 1"
    );
    assert_eq!(state.xi_devices.device(pointer_id).unwrap().buttons_down, 1);
    assert_eq!(
        state
            .xi_devices
            .device(crate::xinput::DEVICEID_XTEST_POINTER)
            .unwrap()
            .buttons_down,
        0,
        "the XTEST source completed its independent click"
    );
    assert_eq!(
        state.xi1_frozen[&crate::xinput::DEVICEID_MASTER_KEYBOARD].other,
        Some(ClientId(OWNER)),
        "the paired keyboard remains held until the master grab ends"
    );
    assert!(state.sync_pending.is_empty());

    let _ = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &xid_map,
        xi_dynamic_pointer_event(
            crate::core_loop::InputOrigin::Physical(source_id),
            PointerEventKind::ButtonRelease,
            1,
            3,
        ),
        true,
        false,
    );
    assert_eq!(state.buttons_down, 0);
    assert_eq!(state.xi_devices.device(pointer_id).unwrap().buttons_down, 0);
    assert!(state.active_pointer_grab.is_none());
    assert_eq!(
        state.xi1_frozen[&crate::xinput::DEVICEID_MASTER_KEYBOARD].other,
        None
    );
    assert!(state.sync_pending.is_empty());
}

#[test]
fn xi_dynamic_grabs_exact_slave_delivery_uses_grab_mask_without_selection() {
    use crate::{
        backend::Backend, core_loop::pointer_fanout::pointer_event_fanout_to_state,
        host_x11::PointerEventKind,
    };

    const OWNER: u32 = 1;
    const XI_GRAB_DEVICE: u8 = 51;
    const XI_UNGRAB_DEVICE: u8 = 52;
    const GRAB_MASK: u32 = (1 << 4) | (1 << 5) | (1 << 6); // ButtonPress, ButtonRelease, Motion

    let mut state = ServerState::new();
    let mut owner_peer = install_capture_client(&mut state, OWNER);
    let mut backend = RecordingBackend::new();
    let (source_id, pointer_id) =
        xi_dynamic_grab_source(&mut state, 92, false, true, "grab-mask-mouse");
    let grab = xi_dynamic_wire_grab_body(
        ROOT_WINDOW.0,
        pointer_id,
        1, // asynchronous pointer mode
        1, // asynchronous paired-device mode
        false,
        Some(GRAB_MASK),
    );
    process_xi_dynamic_request(
        &mut state,
        &mut backend,
        ClientId(OWNER),
        SequenceNumber(1),
        XI_GRAB_DEVICE,
        &grab,
    );
    let _ = read_all_available(&mut owner_peer); // reply and grab crossings
    assert_eq!(
        state.xi_devices.device(pointer_id).unwrap().attached_master,
        None
    );

    let xid_map = backend.xid_map().clone();
    for (kind, detail, time, expected_type) in [
        (PointerEventKind::ButtonPress, 1, 2, 4),
        (PointerEventKind::MotionNotify, 0, 3, 6),
        (PointerEventKind::ButtonRelease, 1, 4, 5),
    ] {
        let _ = pointer_event_fanout_to_state(
            &mut state,
            &mut backend,
            &xid_map,
            xi_dynamic_pointer_event(
                crate::core_loop::InputOrigin::Physical(source_id),
                kind,
                detail,
                time,
            ),
            true,
            false,
        );
        assert_eq!(
            xi_dynamic_event_ids(&read_all_available(&mut owner_peer)),
            vec![(expected_type, pointer_id, pointer_id)],
            "an exact-device grab uses its XIGrabDevice mask without XISelectEvents"
        );
    }

    assert_eq!(
        state.xi2_pointer_grabs[&pointer_id].xi2_mask,
        u64::from(GRAB_MASK)
    );
    assert_eq!(state.xi_devices.device(pointer_id).unwrap().buttons_down, 0);
    assert_eq!(
        state.buttons_down, 0,
        "the floating slave never changes master held state"
    );
    assert!(state.xi2_pointer_grabs.contains_key(&pointer_id));
    assert_eq!(
        state.xi_devices.device(pointer_id).unwrap().attached_master,
        None
    );
    assert!(state.sync_pending.is_empty());
    assert!(!state.xi1_frozen[&pointer_id].frozen());

    let mut ungrab = Vec::with_capacity(8);
    ungrab.extend_from_slice(&0u32.to_le_bytes());
    ungrab.extend_from_slice(&pointer_id.to_le_bytes());
    ungrab.extend_from_slice(&[0u8; 2]);
    process_xi_dynamic_request(
        &mut state,
        &mut backend,
        ClientId(OWNER),
        SequenceNumber(2),
        XI_UNGRAB_DEVICE,
        &ungrab,
    );
    assert!(!state.xi2_pointer_grabs.contains_key(&pointer_id));
    assert_eq!(
        state.xi_devices.device(pointer_id).unwrap().attached_master,
        Some(crate::xinput::DEVICEID_MASTER_POINTER)
    );
    assert!(!state.xi2_detached_masters.contains_key(&pointer_id));
    assert!(state.sync_pending.is_empty());
}

#[test]
fn xi_dynamic_grabs_floating_slave_does_not_copy_raw_motion_to_masters() {
    use crate::{
        backend::Backend, core_loop::pointer_fanout::pointer_event_fanout_to_state,
        host_x11::PointerEventKind,
    };

    const FACET_CLIENT: u32 = 1;
    const MASTER_CLIENT: u32 = 2;
    const ALL_MASTER_CLIENT: u32 = 3;
    const GRAB_CLIENT: u32 = 4;
    const XI_SELECT_EVENTS: u8 = 46;
    const XI_GRAB_DEVICE: u8 = 51;
    const RAW_MOTION_MASK: u32 = 1 << 17;

    let mut state = ServerState::new();
    let mut facet_peer = install_capture_client(&mut state, FACET_CLIENT);
    let mut master_peer = install_capture_client(&mut state, MASTER_CLIENT);
    let mut all_master_peer = install_capture_client(&mut state, ALL_MASTER_CLIENT);
    let mut grab_peer = install_capture_client(&mut state, GRAB_CLIENT);
    let mut backend = RecordingBackend::new();
    let (source_id, pointer_id) =
        xi_dynamic_grab_source(&mut state, 93, false, true, "floating-raw-mouse");

    for (client, selector) in [
        (FACET_CLIENT, pointer_id),
        (MASTER_CLIENT, crate::xinput::DEVICEID_MASTER_POINTER),
        (ALL_MASTER_CLIENT, 1), // XIAllMasterDevices
    ] {
        process_xi_dynamic_request(
            &mut state,
            &mut backend,
            ClientId(client),
            SequenceNumber(1),
            XI_SELECT_EVENTS,
            &xi_dynamic_select_events_body(ROOT_WINDOW.0, selector, RAW_MOTION_MASK),
        );
    }
    process_xi_dynamic_request(
        &mut state,
        &mut backend,
        ClientId(GRAB_CLIENT),
        SequenceNumber(1),
        XI_GRAB_DEVICE,
        &xi_dynamic_wire_grab_body(ROOT_WINDOW.0, pointer_id, 1, 1, false, None),
    );
    let _ = read_all_available(&mut grab_peer); // reply and grab crossings
    assert_eq!(
        state.xi_devices.device(pointer_id).unwrap().attached_master,
        None
    );

    let xid_map = backend.xid_map().clone();
    let mut motion = xi_dynamic_pointer_event(
        crate::core_loop::InputOrigin::Physical(source_id),
        PointerEventKind::MotionNotify,
        0,
        2,
    );
    motion.raw_dx = 8;
    motion.raw_dy = -3;
    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, motion, true, false);

    assert_eq!(
        xi_dynamic_event_ids(&read_all_available(&mut facet_peer)),
        vec![(17, pointer_id, pointer_id)],
        "floating input retains its raw slave event"
    );
    assert!(
        xi_dynamic_event_ids(&read_all_available(&mut master_peer)).is_empty(),
        "a floating slave has no raw master copy"
    );
    assert!(
        xi_dynamic_event_ids(&read_all_available(&mut all_master_peer)).is_empty(),
        "XIAllMasterDevices receives no raw copy from a floating slave"
    );
    assert_eq!(
        state.xi_devices.device(pointer_id).unwrap().attached_master,
        None
    );
    assert!(state.sync_pending.is_empty());
}

#[test]
fn xi_slave_switch_device_changed_selection_on_child_window_is_delivered() {
    use crate::{core_loop::pointer_fanout::pointer_event_fanout_to_state, host_x11::HostXidMap};

    const CLIENT: u32 = 94;
    const CHILD: u32 = 0x0010_0094;
    const XI_SELECT_EVENTS: u8 = 46;
    const SOURCE: u64 = 0xB14;

    let mut state = ServerState::new();
    let mut peer = install_capture_client(&mut state, CLIENT);
    let mut backend = RecordingBackend::new();
    state.resources.create_window(
        ClientId(CLIENT),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(CHILD),
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
    let (source_id, pointer_id) =
        xi_dynamic_grab_source(&mut state, SOURCE, false, true, "child-switch-mouse");
    process_xi_dynamic_request(
        &mut state,
        &mut backend,
        ClientId(CLIENT),
        SequenceNumber(1),
        XI_SELECT_EVENTS,
        &xi_dynamic_select_events_body(
            CHILD,
            crate::xinput::DEVICEID_MASTER_POINTER,
            crate::xinput::XI2_DEVICE_CHANGED_MASK,
        ),
    );
    assert!(
        read_all_available(&mut peer).is_empty(),
        "a child selection does not receive the root-only bootstrap"
    );
    process_xi_dynamic_request(
        &mut state,
        &mut backend,
        ClientId(CLIENT),
        SequenceNumber(2),
        XI_SELECT_EVENTS,
        &xi_dynamic_select_events_body(
            ROOT_WINDOW.0,
            crate::xinput::DEVICEID_MASTER_POINTER,
            crate::xinput::XI2_DEVICE_CHANGED_MASK,
        ),
    );
    let _root_bootstrap = read_all_available(&mut peer);

    let dropped = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &HostXidMap::new(),
        xi_dynamic_pointer_event(
            crate::core_loop::InputOrigin::Physical(source_id),
            crate::host_x11::PointerEventKind::MotionNotify,
            0,
            1,
        ),
        true,
        false,
    );
    assert!(dropped.is_empty());

    let events = read_all_available(&mut peer);
    let mut delivered = Vec::new();
    let mut offset = 0;
    while offset < events.len() {
        assert_eq!(events[offset], 35, "GenericEvent");
        let units = usize::try_from(u32::from_le_bytes(
            events[offset + 4..offset + 8].try_into().unwrap(),
        ))
        .unwrap();
        let event_len = 32 + 4 * units;
        assert!(
            offset + event_len <= events.len(),
            "complete DeviceChanged event"
        );
        assert_eq!(
            u16::from_le_bytes([events[offset + 8], events[offset + 9]]),
            1,
            "XI_DeviceChanged SlaveSwitch"
        );
        assert_eq!(
            u16::from_le_bytes([events[offset + 10], events[offset + 11]]),
            crate::xinput::DEVICEID_MASTER_POINTER
        );
        assert_eq!(
            u16::from_le_bytes([events[offset + 18], events[offset + 19]]),
            pointer_id,
            "sourceid is the newly active physical slave"
        );
        delivered.push((
            u16::from_le_bytes([events[offset + 10], events[offset + 11]]),
            u16::from_le_bytes([events[offset + 18], events[offset + 19]]),
        ));
        offset += event_len;
    }
    assert_eq!(
        delivered,
        [(crate::xinput::DEVICEID_MASTER_POINTER, pointer_id); 2],
        "Xorg emits one DeviceChanged for root and one for the selected child"
    );
    assert_eq!(
        state.xi_devices.device(pointer_id).unwrap().attached_master,
        Some(crate::xinput::DEVICEID_MASTER_POINTER)
    );
    assert!(state.xi2_pointer_grabs.is_empty());
    assert!(state.active_pointer_grab.is_none());
    assert_eq!(state.buttons_down, 0);
    assert!(state.unpublished_pointer_buttons_down.is_empty());
    assert!(state.sync_pending.is_empty());
    assert!(state.xi1_frozen.values().all(|freeze| {
        freeze.state == crate::server::Xi1SyncState::Thawed
            && freeze.other.is_none()
            && freeze.stored.is_none()
    }));
    assert!(state.clients[&CLIENT].outbound.is_empty());
}

fn xi_dynamic_grab_body(window: u32, device_id: u16, mode: u8) -> Vec<u8> {
    let mut body = Vec::with_capacity(24);
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // current time
    body.extend_from_slice(&0u32.to_le_bytes()); // no cursor
    body.extend_from_slice(&device_id.to_le_bytes());
    body.extend_from_slice(&[mode, 1, 0, 0]); // mode, paired async, owner_events, pad
    body.extend_from_slice(&0u16.to_le_bytes()); // no event-mask words
    body.extend_from_slice(&[0u8; 2]);
    body
}

fn xi_dynamic_passive_grab_body(
    window: u32,
    detail: u32,
    device_id: u16,
    grab_type: u8,
    grab_mode: u8,
    paired_device_mode: u8,
) -> Vec<u8> {
    let mut body = Vec::with_capacity(32);
    body.extend_from_slice(&0u32.to_le_bytes()); // current time
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // no cursor
    body.extend_from_slice(&detail.to_le_bytes());
    body.extend_from_slice(&device_id.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes()); // one modifier tuple
    body.extend_from_slice(&0u16.to_le_bytes()); // no XI2 event-mask words
    body.extend_from_slice(&[grab_type, grab_mode, paired_device_mode, 0, 0, 0]);
    body.extend_from_slice(&0u32.to_le_bytes()); // no modifiers
    body
}

fn xi_dynamic_allow_replay_body(device_id: u16) -> Vec<u8> {
    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&0u32.to_le_bytes()); // current time
    body.extend_from_slice(&device_id.to_le_bytes());
    body.extend_from_slice(&[2, 0]); // XIReplayDevice, pad
    body
}

#[test]
fn xi_dynamic_grabs_classify_keyboard_and_keep_slave_grabs_independent() {
    const KEY_CLIENT: u32 = 1;
    const POINTER_CLIENT_A: u32 = 2;
    const POINTER_CLIENT_B: u32 = 3;
    const WINDOW: u32 = 0x0010_0057;

    let mut state = ServerState::new();
    let _key_peer = install_client(&mut state, KEY_CLIENT);
    let _pointer_peer_a = install_client(&mut state, POINTER_CLIENT_A);
    let _pointer_peer_b = install_client(&mut state, POINTER_CLIENT_B);
    let mut backend = RecordingBackend::new();
    let (_, keyboard_id) = xi_dynamic_grab_source(&mut state, 11, true, false, "kbd-a");
    let (_, pointer_id_a) = xi_dynamic_grab_source(&mut state, 12, false, true, "ptr-a");
    let (_, pointer_id_b) = xi_dynamic_grab_source(&mut state, 13, false, true, "ptr-b");
    let header = yserver_protocol::x11::RequestHeader {
        opcode: 131,
        data: 51,
        length_units: 7,
    };

    for (client, device_id) in [
        (KEY_CLIENT, keyboard_id),
        (POINTER_CLIENT_A, pointer_id_a),
        (POINTER_CLIENT_B, pointer_id_b),
    ] {
        handle_xi2_request(
            &mut state,
            &mut backend,
            None,
            ClientId(client),
            SequenceNumber(1),
            header,
            &xi_dynamic_grab_body(WINDOW, device_id, 1),
        )
        .expect("XIGrabDevice for physical facet");
    }

    assert!(
        state.active_pointer_grab.is_none(),
        "a pointer-slave grab must not occupy the core master-pointer slot"
    );
    assert!(
        state.active_keyboard_grab.is_none(),
        "a keyboard-slave grab must not occupy the core master-keyboard slot"
    );
    assert_eq!(
        state.xi2_keyboard_grabs.get(&keyboard_id).unwrap().owner,
        ClientId(KEY_CLIENT)
    );
    assert_eq!(
        state.xi2_pointer_grabs.get(&pointer_id_a).unwrap().owner,
        ClientId(POINTER_CLIENT_A)
    );
    assert_eq!(
        state.xi2_pointer_grabs.get(&pointer_id_b).unwrap().owner,
        ClientId(POINTER_CLIENT_B)
    );
    assert_eq!(state.xi2_keyboard_grabs.len(), 1);
    assert_eq!(state.xi2_pointer_grabs.len(), 2);
    assert_eq!(
        state.xi2_detached_masters.get(&keyboard_id),
        Some(&crate::xinput::DEVICEID_MASTER_KEYBOARD)
    );
    assert_eq!(
        state.xi2_detached_masters.get(&pointer_id_a),
        Some(&crate::xinput::DEVICEID_MASTER_POINTER)
    );
    assert_eq!(
        state.xi2_detached_masters.get(&pointer_id_b),
        Some(&crate::xinput::DEVICEID_MASTER_POINTER)
    );
    assert_eq!(
        state
            .xi_devices
            .device(keyboard_id)
            .unwrap()
            .attached_master,
        None
    );
    assert_eq!(
        state
            .xi_devices
            .device(pointer_id_a)
            .unwrap()
            .attached_master,
        None
    );
    assert_eq!(
        state
            .xi_devices
            .device(pointer_id_b)
            .unwrap()
            .attached_master,
        None
    );
}

#[test]
fn xi_dynamic_grabs_slave_pointer_replay_reattaches_and_replays_to_natural_target() {
    use crate::{
        backend::Backend,
        core_loop::pointer_fanout::pointer_event_fanout_to_state,
        host_x11::{HostPointerEvent, PointerEventKind},
        resources::ROOT_VISUAL,
    };

    const GRAB_CLIENT: u32 = 1;
    const TARGET_CLIENT: u32 = 2;
    const TARGET_WINDOW: u32 = 0x0010_005A;
    const HOST_XID: u32 = 0xCAFE_005A;

    let mut state = ServerState::new();
    let mut grab_peer = install_client(&mut state, GRAB_CLIENT);
    let mut target_peer = install_client(&mut state, TARGET_CLIENT);
    let mut backend = RecordingBackend::new();
    let (source_id, pointer_id) =
        xi_dynamic_grab_source(&mut state, 41, false, true, "replay-pointer");
    assert!(pointer_id > 5, "test must use a dynamic physical slave ID");
    assert_eq!(
        state.xi_devices.role(pointer_id),
        Some(crate::xinput::XiDeviceRole::SlavePointer)
    );
    state.resources.create_window(
        ClientId(TARGET_CLIENT),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(TARGET_WINDOW),
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
    let _ = state.resources.map_window(ResourceId(TARGET_WINDOW));
    state
        .clients
        .get_mut(&TARGET_CLIENT)
        .expect("target client")
        .event_masks
        .insert(ResourceId(TARGET_WINDOW), 0x0004); // ButtonPressMask
    Backend::register_top_level(&mut backend, None, ResourceId(TARGET_WINDOW), HOST_XID)
        .expect("register physical input target");

    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(GRAB_CLIENT),
        SequenceNumber(1),
        yserver_protocol::x11::RequestHeader {
            opcode: 131,
            data: 54,
            length_units: 9,
        },
        &xi_dynamic_passive_grab_body(
            ROOT_WINDOW.0,
            1,
            pointer_id,
            0, // Button
            0, // synchronous pointer mode
            1, // asynchronous paired-device mode
        ),
    )
    .expect("XIPassiveGrabDevice button");
    let _ = read_all_available(&mut grab_peer); // XIPassiveGrabDevice reply

    let xid_map = backend.xid_map().clone();
    let press = HostPointerEvent {
        origin: crate::core_loop::InputOrigin::Physical(source_id),
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
    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, press, true, false);

    assert!(state.xi2_pointer_grabs.contains_key(&pointer_id));
    assert_eq!(
        state.xi_devices.device(pointer_id).unwrap().attached_master,
        None,
        "activating a synchronous physical-slave button grab detaches that slave"
    );
    assert!(
        state.active_pointer_grab.is_none(),
        "a physical-slave XI2 grab must not occupy the core master slot"
    );
    assert!(matches!(
        state
            .xi1_frozen
            .get(&pointer_id)
            .and_then(|freeze| freeze.stored.as_ref()),
        Some(crate::server::QueuedInputEvent::HostPointer(_))
    ));
    assert!(
        read_all_available(&mut target_peer).is_empty(),
        "the natural target waits for XIReplayDevice"
    );

    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(GRAB_CLIENT),
        SequenceNumber(2),
        yserver_protocol::x11::RequestHeader {
            opcode: 131,
            data: 53,
            length_units: 3,
        },
        &xi_dynamic_allow_replay_body(pointer_id),
    )
    .expect("XIAllowEvents ReplayDevice");

    assert!(!state.xi2_pointer_grabs.contains_key(&pointer_id));
    assert!(state.active_pointer_grab.is_some_and(|grab| {
        grab.implicit
            && grab.owner == ClientId(TARGET_CLIENT)
            && grab.grab_window == ResourceId(TARGET_WINDOW)
    }));
    assert_eq!(
        state.xi_devices.device(pointer_id).unwrap().attached_master,
        Some(crate::xinput::DEVICEID_MASTER_POINTER),
        "ReplayDevice restores the physical slave's original master"
    );
    assert!(!state.xi2_detached_masters.contains_key(&pointer_id));
    assert!(!state.xi1_frozen[&pointer_id].frozen());
    assert!(state.xi1_frozen[&pointer_id].stored.is_none());

    let replay = read_all_or_buffered(&mut state, TARGET_CLIENT, &mut target_peer);
    assert!(
        replay.len() >= 32,
        "expected replayed ButtonPress, got {} bytes",
        replay.len()
    );
    assert_eq!(
        replay[0] & 0x7f,
        4,
        "replay reaches the target as ButtonPress"
    );
    assert_eq!(
        &replay[12..16],
        &TARGET_WINDOW.to_le_bytes(),
        "XIReplayDevice delivers to the natural target after the passive grab"
    );

    let _ = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &xid_map,
        HostPointerEvent {
            kind: PointerEventKind::ButtonRelease,
            time: 0x1235,
            state: 0x0100,
            ..press
        },
        true,
        false,
    );
    assert!(state.xi2_pointer_grabs.is_empty());
    assert!(state.active_pointer_grab.is_none());
}

#[test]
fn xi_dynamic_grabs_slave_keyboard_replay_reattaches_and_replays_to_focus() {
    use crate::{
        core_loop::key_fanout::key_event_fanout_to_state, host_x11::HostKeyEvent,
        resources::ROOT_VISUAL,
    };

    const GRAB_CLIENT: u32 = 1;
    const TARGET_CLIENT: u32 = 2;
    const TARGET_WINDOW: u32 = 0x0010_005B;
    const KEYCODE: u32 = 38;

    let mut state = ServerState::new();
    let mut grab_peer = install_client(&mut state, GRAB_CLIENT);
    let mut target_peer = install_client(&mut state, TARGET_CLIENT);
    let mut backend = RecordingBackend::new();
    let (source_id, keyboard_id) =
        xi_dynamic_grab_source(&mut state, 42, true, false, "replay-keyboard");
    assert!(keyboard_id > 5, "test must use a dynamic physical slave ID");
    assert_eq!(
        state.xi_devices.role(keyboard_id),
        Some(crate::xinput::XiDeviceRole::SlaveKeyboard)
    );
    state.resources.create_window(
        ClientId(TARGET_CLIENT),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(TARGET_WINDOW),
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
    let _ = state.resources.map_window(ResourceId(TARGET_WINDOW));
    state.core_focus.raw = TARGET_WINDOW;
    state
        .clients
        .get_mut(&TARGET_CLIENT)
        .expect("target client")
        .event_masks
        .insert(ResourceId(TARGET_WINDOW), 0x0001); // KeyPressMask

    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(GRAB_CLIENT),
        SequenceNumber(1),
        yserver_protocol::x11::RequestHeader {
            opcode: 131,
            data: 54,
            length_units: 9,
        },
        &xi_dynamic_passive_grab_body(
            ROOT_WINDOW.0,
            KEYCODE,
            keyboard_id,
            1, // Keycode
            0, // synchronous keyboard mode
            1, // asynchronous paired-device mode
        ),
    )
    .expect("XIPassiveGrabDevice keycode");
    let _ = read_all_available(&mut grab_peer); // XIPassiveGrabDevice reply

    let press = HostKeyEvent {
        origin: crate::core_loop::InputOrigin::Physical(source_id),
        pressed: true,
        keycode: KEYCODE as u8,
        time: 0x1234,
        root_x: 10,
        root_y: 20,
        event_x: 10,
        event_y: 20,
        state: 0,
    };
    let _ = key_event_fanout_to_state(&mut state, &mut backend, press);

    assert!(state.xi2_keyboard_grabs.contains_key(&keyboard_id));
    assert_eq!(
        state
            .xi_devices
            .device(keyboard_id)
            .unwrap()
            .attached_master,
        None,
        "activating a synchronous physical-slave key grab detaches that slave"
    );
    assert!(matches!(
        state
            .xi1_frozen
            .get(&keyboard_id)
            .and_then(|freeze| freeze.stored.as_ref()),
        Some(
            crate::server::QueuedInputEvent::HostKey(_)
                | crate::server::QueuedInputEvent::HostKeyTransition(_, _)
        )
    ));
    assert!(
        read_all_available(&mut target_peer).is_empty(),
        "the focus window waits for XIReplayDevice"
    );

    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(GRAB_CLIENT),
        SequenceNumber(2),
        yserver_protocol::x11::RequestHeader {
            opcode: 131,
            data: 53,
            length_units: 3,
        },
        &xi_dynamic_allow_replay_body(keyboard_id),
    )
    .expect("XIAllowEvents ReplayDevice");

    assert!(!state.xi2_keyboard_grabs.contains_key(&keyboard_id));
    assert!(state.active_keyboard_grab.is_none());
    assert_eq!(
        state
            .xi_devices
            .device(keyboard_id)
            .unwrap()
            .attached_master,
        Some(crate::xinput::DEVICEID_MASTER_KEYBOARD),
        "ReplayDevice restores the physical slave's original master"
    );
    assert!(!state.xi2_detached_masters.contains_key(&keyboard_id));
    assert!(!state.xi1_frozen[&keyboard_id].frozen());
    assert!(state.xi1_frozen[&keyboard_id].stored.is_none());

    let replay = read_all_available(&mut target_peer);
    assert!(
        replay.len() >= 32,
        "expected replayed KeyPress, got {} bytes",
        replay.len()
    );
    assert_eq!(replay[0] & 0x7f, 2, "replay reaches focus as KeyPress");
    assert_eq!(
        &replay[12..16],
        &TARGET_WINDOW.to_le_bytes(),
        "XIReplayDevice delivers to the current focus window"
    );
}

#[test]
fn xi_dynamic_grabs_slave_keyboard_release_reattaches_for_next_key() {
    use crate::{
        core_loop::key_fanout::key_event_fanout_to_state, host_x11::HostKeyEvent,
        resources::ROOT_VISUAL,
    };

    const GRAB_CLIENT: u32 = 1;
    const TARGET_CLIENT: u32 = 2;
    const TARGET_WINDOW: u32 = 0x0010_005C;
    const GRABBED_KEYCODE: u32 = 38;
    const NEXT_KEYCODE: u8 = 39;

    let mut state = ServerState::new();
    let mut grab_peer = install_client(&mut state, GRAB_CLIENT);
    let mut target_peer = install_client(&mut state, TARGET_CLIENT);
    let mut backend = RecordingBackend::new();
    let (source_id, keyboard_id) =
        xi_dynamic_grab_source(&mut state, 43, true, false, "release-keyboard");
    assert!(keyboard_id > 5, "test must use a dynamic physical slave ID");
    assert_eq!(
        state.xi_devices.role(keyboard_id),
        Some(crate::xinput::XiDeviceRole::SlaveKeyboard)
    );
    state.resources.create_window(
        ClientId(TARGET_CLIENT),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(TARGET_WINDOW),
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
    let _ = state.resources.map_window(ResourceId(TARGET_WINDOW));
    state.core_focus.raw = TARGET_WINDOW;
    state
        .clients
        .get_mut(&TARGET_CLIENT)
        .expect("target client")
        .event_masks
        .insert(ResourceId(TARGET_WINDOW), 0x0001); // KeyPressMask

    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(GRAB_CLIENT),
        SequenceNumber(1),
        yserver_protocol::x11::RequestHeader {
            opcode: 131,
            data: 54,
            length_units: 9,
        },
        &xi_dynamic_passive_grab_body(
            ROOT_WINDOW.0,
            GRABBED_KEYCODE,
            keyboard_id,
            1, // Keycode
            1, // asynchronous keyboard mode; release is not frozen
            1, // asynchronous paired-device mode
        ),
    )
    .expect("XIPassiveGrabDevice keycode");
    let _ = read_all_available(&mut grab_peer); // XIPassiveGrabDevice reply

    let input = |pressed, keycode| HostKeyEvent {
        origin: crate::core_loop::InputOrigin::Physical(source_id),
        pressed,
        keycode,
        time: 0x1234,
        root_x: 10,
        root_y: 20,
        event_x: 10,
        event_y: 20,
        state: 0,
    };
    let _ = key_event_fanout_to_state(&mut state, &mut backend, input(true, GRABBED_KEYCODE as u8));
    assert!(state.xi2_keyboard_grabs.contains_key(&keyboard_id));
    assert_eq!(
        state
            .xi_devices
            .device(keyboard_id)
            .unwrap()
            .attached_master,
        None,
        "the matching passive grab floats the physical keyboard"
    );

    let _ = key_event_fanout_to_state(
        &mut state,
        &mut backend,
        input(false, GRABBED_KEYCODE as u8),
    );
    assert!(
        !state.xi2_keyboard_grabs.contains_key(&keyboard_id),
        "the matching key release auto-deactivates the slave passive grab"
    );
    assert_eq!(
        state
            .xi_devices
            .device(keyboard_id)
            .unwrap()
            .attached_master,
        Some(crate::xinput::DEVICEID_MASTER_KEYBOARD),
        "matching key release restores the physical slave's original master"
    );
    assert!(!state.xi2_detached_masters.contains_key(&keyboard_id));

    let _ = key_event_fanout_to_state(&mut state, &mut backend, input(true, NEXT_KEYCODE));
    let delivered = read_all_available(&mut target_peer);
    assert!(
        delivered.len() >= 32,
        "expected next core KeyPress, got {} bytes",
        delivered.len()
    );
    assert_eq!(
        delivered[0] & 0x7f,
        2,
        "next key returns to core KeyPress delivery"
    );
    assert_eq!(
        &delivered[12..16],
        &TARGET_WINDOW.to_le_bytes(),
        "next key from the reattached slave reaches the focused window"
    );
    assert_ne!(
        state.keys_down[usize::from(NEXT_KEYCODE / 8)] & (1 << (NEXT_KEYCODE % 8)),
        0,
        "the reattached key also updates master keyboard state"
    );
}

#[test]
fn xi_dynamic_grabs_passive_button_is_scoped_to_its_device() {
    use crate::{
        backend::Backend,
        core_loop::pointer_fanout::pointer_event_fanout_to_state,
        host_x11::{HostPointerEvent, PointerEventKind},
        resources::ROOT_VISUAL,
    };

    const GRAB_CLIENT: u32 = 1;
    const WINDOW: u32 = 0x0010_0058;
    const HOST_XID: u32 = 0xCAFE_0058;
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, GRAB_CLIENT);
    let mut backend = RecordingBackend::new();
    let (_, razer_id) = xi_dynamic_grab_source(&mut state, 21, false, true, "razer");
    let (hyperx_source, hyperx_id) = xi_dynamic_grab_source(&mut state, 22, false, true, "hyperx");
    state.resources.create_window(
        ClientId(GRAB_CLIENT),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(WINDOW),
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
    let _ = state.resources.map_window(ResourceId(WINDOW));

    // XIPassiveGrabDevice: one AnyModifier button grab, tied to the Razer
    // pointer facet. The button mask is intentionally empty; the passive
    // grab still activates and its owner receives the protocol event.
    let mut body = Vec::with_capacity(32);
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&WINDOW.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // cursor
    body.extend_from_slice(&1u32.to_le_bytes()); // button 1
    body.extend_from_slice(&razer_id.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes()); // one modifier tuple
    body.extend_from_slice(&0u16.to_le_bytes()); // no event-mask words
    body.extend_from_slice(&[0, 0, 1, 0, 0, 0]); // button, sync, async, owner=false, pad
    body.extend_from_slice(&0u32.to_le_bytes()); // AnyModifier is encoded as zero here
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(GRAB_CLIENT),
        SequenceNumber(1),
        yserver_protocol::x11::RequestHeader {
            opcode: 131,
            data: 54,
            length_units: 9,
        },
        &body,
    )
    .expect("XIPassiveGrabDevice");

    Backend::register_top_level(&mut backend, None, ResourceId(WINDOW), HOST_XID)
        .expect("register host window");
    let event = |source, kind, root_x, root_y, state_mask| HostPointerEvent {
        origin: crate::core_loop::InputOrigin::Physical(source),
        kind,
        host_xid: HOST_XID,
        detail: 1,
        time: 0x1234,
        root_x,
        root_y,
        event_x: root_x,
        event_y: root_y,
        state: state_mask,
        crossing_mode: 0,
        child: 0,
        raw_dx: 0,
        raw_dy: 0,
        tree_change: false,
    };
    let xid_map = backend.xid_map().clone();
    for kind in [
        PointerEventKind::ButtonPress,
        PointerEventKind::ButtonRelease,
    ] {
        let _ = pointer_event_fanout_to_state(
            &mut state,
            &mut backend,
            &xid_map,
            event(
                hyperx_source,
                kind,
                10,
                10,
                if kind == PointerEventKind::ButtonRelease {
                    0x0100
                } else {
                    0
                },
            ),
            true,
            false,
        );
    }

    assert!(
        !state.xi2_pointer_grabs.contains_key(&razer_id),
        "HyperX press/release must not activate Razer's exact-device passive grab",
    );
    assert!(
        state.active_pointer_grab.is_none(),
        "Razer's exact-device passive grab must not intercept HyperX"
    );
    assert_eq!(
        state.xi_devices.device(hyperx_id).unwrap().buttons_down,
        0,
        "HyperX press/release transitions remain balanced on HyperX"
    );
    assert_eq!(
        state.xi_devices.device(razer_id).unwrap().buttons_down,
        0,
        "Razer's held set is untouched"
    );

    let _ = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &xid_map,
        event(hyperx_source, PointerEventKind::ButtonPress, 10, 10, 0),
        true,
        false,
    );
    let _ = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &xid_map,
        event(
            crate::xinput::InputSourceId(21),
            PointerEventKind::ButtonPress,
            10,
            10,
            0,
        ),
        true,
        false,
    );
    assert_eq!(
        state
            .xi2_pointer_grabs
            .get(&razer_id)
            .map(|grab| grab.owner),
        Some(ClientId(GRAB_CLIENT)),
        "Razer's exact XI2 passive grab activates for Razer"
    );
    assert_eq!(
        state.xi_devices.device(razer_id).unwrap().attached_master,
        None,
        "the passive grab floats only its exact physical slave"
    );
    assert!(state.active_pointer_grab.is_none());

    // The activating press arrived while attached. A later, separate
    // button transition arrives while the Razer facet is floating and
    // must change only its per-slave hold state.
    let master_buttons = state.buttons_down;
    let master_position = state.pointer_root;
    let second_press = HostPointerEvent {
        detail: 2,
        state: 0x0100,
        ..event(
            crate::xinput::InputSourceId(21),
            PointerEventKind::ButtonPress,
            10,
            10,
            0x0100,
        )
    };
    let _ = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &xid_map,
        second_press,
        true,
        false,
    );
    assert_eq!(state.buttons_down, master_buttons);
    assert_eq!(state.pointer_root, master_position);
    assert_eq!(state.xi_devices.device(razer_id).unwrap().buttons_down, 3);
    let second_release = HostPointerEvent {
        kind: PointerEventKind::ButtonRelease,
        state: 0x0300,
        ..second_press
    };
    let _ = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &xid_map,
        second_release,
        true,
        false,
    );
    assert_eq!(state.buttons_down, master_buttons);
    assert_eq!(state.xi_devices.device(razer_id).unwrap().buttons_down, 1);

    for kind in [
        PointerEventKind::ButtonPress,
        PointerEventKind::ButtonRelease,
    ] {
        let _ = pointer_event_fanout_to_state(
            &mut state,
            &mut backend,
            &xid_map,
            event(
                hyperx_source,
                kind,
                10,
                10,
                if kind == PointerEventKind::ButtonRelease {
                    0x0100
                } else {
                    0
                },
            ),
            true,
            false,
        );
    }
    assert!(state.xi2_pointer_grabs.contains_key(&razer_id));
    assert_eq!(state.xi_devices.device(hyperx_id).unwrap().buttons_down, 0);

    let master_position = state.pointer_root;
    let _ = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &xid_map,
        event(
            crate::xinput::InputSourceId(21),
            PointerEventKind::MotionNotify,
            30,
            40,
            0x0100,
        ),
        true,
        false,
    );
    assert_eq!(
        state.pointer_root, master_position,
        "floating motion leaves the master sprite alone"
    );
    assert_eq!(
        state.floating_pointer_positions.get(&razer_id),
        Some(&(30.0, 40.0))
    );

    let _ = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &xid_map,
        event(
            crate::xinput::InputSourceId(21),
            PointerEventKind::ButtonRelease,
            30,
            40,
            0x0100,
        ),
        true,
        false,
    );
    assert!(state.xi2_pointer_grabs.contains_key(&razer_id));
    assert_eq!(
        state.xi_devices.device(razer_id).unwrap().attached_master,
        None,
        "a synchronous activating press stays grabbed until replay"
    );
    assert!(!state.sync_pending.is_empty());

    let mut allow_body = Vec::with_capacity(8);
    allow_body.extend_from_slice(&0u32.to_le_bytes());
    allow_body.extend_from_slice(&razer_id.to_le_bytes());
    allow_body.extend_from_slice(&[2, 0]); // XIReplayDevice
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(GRAB_CLIENT),
        SequenceNumber(2),
        yserver_protocol::x11::RequestHeader {
            opcode: 131,
            data: 53,
            length_units: 3,
        },
        &allow_body,
    )
    .expect("XIAllowEvents ReplayDevice");
    assert!(
        state.xi2_pointer_grabs.is_empty(),
        "passive release ends the exact-device grab"
    );
    assert_eq!(
        state.xi_devices.device(razer_id).unwrap().attached_master,
        Some(crate::xinput::DEVICEID_MASTER_POINTER),
        "passive release restores the original master"
    );
    assert_eq!(state.xi_devices.device(razer_id).unwrap().buttons_down, 0);
    assert_eq!(state.xi_devices.device(hyperx_id).unwrap().buttons_down, 0);
    assert_eq!(
        state.buttons_down, 0,
        "replay drains the attached master hold"
    );
    assert!(!state.xi2_detached_masters.contains_key(&razer_id));
    assert!(!state.floating_pointer_positions.contains_key(&razer_id));
    assert!(!state.xi1_frozen[&razer_id].frozen());
    assert!(state.sync_pending.is_empty());
}

#[test]
fn xi_dynamic_grabs_detach_and_reattach_slave_without_pair_freeze() {
    const CLIENT: u32 = 1;
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT);
    let mut backend = RecordingBackend::new();
    let (source_id, pointer_id) =
        xi_dynamic_grab_source(&mut state, 31, false, true, "floating-razer");
    let grab_body = xi_dynamic_wire_grab_body(
        ROOT_WINDOW.0,
        pointer_id,
        1, // asynchronous pointer mode
        0, // requested synchronous paired-device mode; Xorg forces Async for slaves
        false,
        None,
    );
    process_xi_dynamic_request(
        &mut state,
        &mut backend,
        ClientId(CLIENT),
        SequenceNumber(1),
        51,
        &grab_body,
    );
    let _ = read_all_available(&mut peer); // XIGrabDevice reply and crossings

    assert_eq!(
        state.xi_devices.device(pointer_id).unwrap().attached_master,
        None,
        "an explicit XI2 physical-slave grab detaches the slave"
    );
    assert!(
        state.xi_devices.source(source_id).unwrap().enabled,
        "detaching a live slave does not disable its source"
    );
    assert_eq!(
        state
            .xi1_frozen
            .get(&crate::xinput::DEVICEID_MASTER_KEYBOARD)
            .and_then(|freeze| freeze.other),
        None,
        "a slave grab forces its paired mode to Async and leaves the master keyboard unfrozen",
    );
    assert!(
        !state
            .xi1_frozen
            .get(&crate::xinput::DEVICEID_MASTER_KEYBOARD)
            .is_some_and(crate::server::Xi1Freeze::frozen)
    );

    // XIAsyncPairedDevice remains a no-op after Xorg coerces the paired
    // mode to Async during XIGrabDevice.
    let mut allow_body = Vec::with_capacity(8);
    allow_body.extend_from_slice(&0u32.to_le_bytes());
    allow_body.extend_from_slice(&pointer_id.to_le_bytes());
    allow_body.extend_from_slice(&[3, 0]); // XIAsyncPairedDevice
    process_xi_dynamic_request(
        &mut state,
        &mut backend,
        ClientId(CLIENT),
        SequenceNumber(2),
        53,
        &allow_body,
    );
    assert_eq!(
        state
            .xi1_frozen
            .get(&crate::xinput::DEVICEID_MASTER_KEYBOARD)
            .and_then(|freeze| freeze.other),
        None,
        "XIAllowEvents does not find a paired master hold to release",
    );
    assert!(state.xi2_pointer_grabs.contains_key(&pointer_id));
    assert_eq!(
        state.xi_devices.device(pointer_id).unwrap().attached_master,
        None,
        "allowing the paired device leaves the requested slave grab intact",
    );

    let mut ungrab_body = Vec::with_capacity(8);
    ungrab_body.extend_from_slice(&0u32.to_le_bytes());
    ungrab_body.extend_from_slice(&pointer_id.to_le_bytes());
    ungrab_body.extend_from_slice(&[0u8; 2]);
    process_xi_dynamic_request(
        &mut state,
        &mut backend,
        ClientId(CLIENT),
        SequenceNumber(3),
        52,
        &ungrab_body,
    );
    assert_eq!(
        state.xi_devices.device(pointer_id).unwrap().attached_master,
        Some(crate::xinput::DEVICEID_MASTER_POINTER),
        "ungrab restores the original master attachment"
    );
    assert!(!state.xi2_pointer_grabs.contains_key(&pointer_id));
    assert!(!state.xi2_detached_masters.contains_key(&pointer_id));
    assert!(!state.floating_pointer_positions.contains_key(&pointer_id));
    assert!(!state.xi1_frozen[&pointer_id].frozen());
    assert!(state.sync_pending.is_empty());
}

#[test]
fn xi_dynamic_grabs_disconnect_reattaches_only_the_owners_slave() {
    use crate::{
        backend::Backend,
        core_loop::{message::InputOrigin, pointer_fanout::pointer_event_fanout_to_state},
        host_x11::{HostPointerEvent, PointerEventKind},
    };

    const OWNER: u32 = 1;
    const OTHER_OWNER: u32 = 2;
    let mut state = ServerState::new();
    let _owner_peer = install_client(&mut state, OWNER);
    let _other_peer = install_client(&mut state, OTHER_OWNER);
    let mut backend = RecordingBackend::new();
    let (owner_source, owner_device) =
        xi_dynamic_grab_source(&mut state, 41, false, true, "disconnect-pointer");
    let (_other_source, other_device) =
        xi_dynamic_grab_source(&mut state, 42, false, true, "unrelated-pointer");
    let header = yserver_protocol::x11::RequestHeader {
        opcode: 131,
        data: 51,
        length_units: 7,
    };

    for (client, device, mode) in [(OWNER, owner_device, 0), (OTHER_OWNER, other_device, 1)] {
        handle_xi2_request(
            &mut state,
            &mut backend,
            None,
            ClientId(client),
            SequenceNumber(1),
            header,
            &xi_dynamic_grab_body(ROOT_WINDOW.0, device, mode),
        )
        .expect("XIGrabDevice before disconnect");
    }

    assert!(state.xi1_frozen[&owner_device].frozen());
    let xid_map = backend.xid_map().clone();
    let _ = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &xid_map,
        HostPointerEvent {
            origin: InputOrigin::Physical(owner_source),
            kind: PointerEventKind::ButtonPress,
            host_xid: 0,
            detail: 1,
            time: 0x2233,
            root_x: 12,
            root_y: 13,
            event_x: 12,
            event_y: 13,
            state: 0,
            crossing_mode: 0,
            child: 0,
            raw_dx: 0,
            raw_dy: 0,
            tree_change: false,
        },
        true,
        false,
    );
    assert_eq!(state.sync_pending.len(), 1, "sync-grab event is queued");
    assert_eq!(
        state.xi_devices.device(owner_device).unwrap().buttons_down,
        1
    );

    crate::core_loop::process_disconnect::process_disconnect(
        &mut state,
        &mut backend,
        ClientId(OWNER),
    );

    assert!(!state.xi2_pointer_grabs.contains_key(&owner_device));
    assert_eq!(
        state
            .xi2_pointer_grabs
            .get(&other_device)
            .map(|grab| grab.owner),
        Some(ClientId(OTHER_OWNER)),
        "disconnect leaves the other client's independent grab intact"
    );
    assert_eq!(
        state
            .xi_devices
            .device(owner_device)
            .unwrap()
            .attached_master,
        Some(crate::xinput::DEVICEID_MASTER_POINTER)
    );
    assert_eq!(
        state
            .xi_devices
            .device(other_device)
            .unwrap()
            .attached_master,
        None,
        "the unrelated grabbed slave remains floating"
    );
    assert!(!state.xi2_detached_masters.contains_key(&owner_device));
    assert!(!state.floating_pointer_positions.contains_key(&owner_device));
    assert!(state.xi2_detached_masters.contains_key(&other_device));
    assert!(!state.xi1_frozen[&owner_device].frozen());
    assert!(state.xi1_frozen[&owner_device].stored.is_none());
    assert!(state.sync_pending.is_empty());
    assert_eq!(
        state.xi_devices.device(owner_device).unwrap().buttons_down,
        1,
        "disconnect releases the grab and queue, while the still-held physical button remains tracked"
    );
    assert!(state.active_pointer_grab.is_none());
}

/// On `XIGrabDevice` activation, Xorg synthesises an XI2 crossing
/// event (`XI_Enter`(7) for pointer / `XI_FocusIn`(9) for keyboard)
/// with `mode=NotifyGrab`(1) and `detail=NotifyNonlinear`(3) and
/// delivers it to the grab requester. Without these events, GTK3
/// popup state machines never engage their hover/click tracking —
/// the menu is visible but items don't highlight or activate.
/// Captured in `mate-xorg.xtrace` (Xi/exevents.c::ActivatePointer
/// Grab / ActivateKeyboardGrab). On `XIUngrabDevice` the symmetric
/// `mode=NotifyUngrab`(2) crossings fire.
#[test]
fn xi_grab_device_emits_grab_activation_crossings() {
    use std::io::Read;

    const CLIENT_ID: u32 = 1;
    const WINDOW_XID: u32 = 0x0010_0060;
    const HOST_XID: u32 = 0x0040_0060;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT_ID);
    let mut backend = RecordingBackend::new();

    state.resources.create_window(
        yserver_protocol::x11::ClientId(CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(WINDOW_XID),
            parent: ROOT_WINDOW,
            x: 100,
            y: 200,
            width: 300,
            height: 200,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    {
        let w = state
            .resources
            .window_mut(ResourceId(WINDOW_XID))
            .expect("window installed");
        w.host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(HOST_XID));
    }
    let _ = state.resources.map_window(ResourceId(WINDOW_XID));

    // XIGrabDevice pointer.
    let mut body = Vec::with_capacity(24);
    body.extend_from_slice(&WINDOW_XID.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // time
    body.extend_from_slice(&0u32.to_le_bytes()); // cursor
    body.extend_from_slice(&2u16.to_le_bytes()); // deviceid
    body.extend_from_slice(&[1, 1, 1, 0]);
    body.extend_from_slice(&0u16.to_le_bytes()); // mask_len
    body.extend_from_slice(&[0u8; 2]);
    let header = yserver_protocol::x11::RequestHeader {
        opcode: 131,
        data: 51,
        length_units: 7,
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
    .expect("XIGrabDevice");

    // Drain wire and look for the synthesised XI2 Enter event:
    //   byte 0: 35 (GenericEvent)
    //   byte 1: 137 (XInputExtension major)
    //   bytes 8-9: evtype = 7 (XI_Enter)
    //   byte 18: mode = 1 (NotifyGrab)
    //   byte 19: detail = 0 (NotifyAncestor — sprite is the ancestor)
    peer.set_nonblocking(true).unwrap();
    let mut wire = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        match peer.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => wire.extend_from_slice(&tmp[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
    // Wire contains both the 32-byte XIGrabDevice reply and the
    // synthesised XI2 GenericEvent (76 bytes). Order may vary —
    // scan for the GenericEvent signature: type=35, major=137,
    // evtype=7 (XI_Enter).
    let crossing_offset = (0..wire.len().saturating_sub(76))
        .find(|&i| {
            wire[i] == 35
                && wire[i + 1] == 137
                && u16::from_le_bytes([wire[i + 8], wire[i + 9]]) == 7
        })
        .unwrap_or_else(|| {
            panic!(
                "no XI_Enter GenericEvent found in {} wire bytes: {:?}",
                wire.len(),
                &wire[..wire.len().min(140)],
            )
        });
    let crossing = &wire[crossing_offset..crossing_offset + 76];
    // Crossing event byte layout (from encode_xi2_crossing_event):
    //   0:type=35, 1:major, 2..4:seq, 4..8:length, 8..10:evtype,
    //   10..12:deviceid, 12..16:time, 16..18:sourceid, 18:mode,
    //   19:detail, 20..24:root, 24..28:event, ...
    assert_eq!(crossing[18], 1, "mode = NotifyGrab");
    // The sprite (pointer at (0,0)) is in root, an ancestor of the
    // grab window — Xorg's `DeviceEnterLeaveEvents` IsParent(from,to)
    // branch emits `XI_Enter(NotifyAncestor)` on the grab window
    // (preceded by `XI_Leave(NotifyInferior)` on root). NotifyAncestor
    // = 0. (Pre-fix this was a hardcoded NotifyNonlinear = 3.)
    assert_eq!(crossing[19], 0, "detail = NotifyAncestor");
    let event_window = u32::from_le_bytes([crossing[24], crossing[25], crossing[26], crossing[27]]);
    assert_eq!(event_window, WINDOW_XID);

    // The activation chain also leaves the ancestor (root) the
    // sprite was in: an `XI_Leave(NotifyInferior)` precedes the Enter.
    let leave = (0..wire.len().saturating_sub(28)).find(|&i| {
        wire[i] == 35 && wire[i + 1] == 137 && u16::from_le_bytes([wire[i + 8], wire[i + 9]]) == 8
    });
    let leave = leave.expect("XI_Leave on the ancestor (root) must precede the Enter");
    let leave = &wire[leave..leave + 28];
    assert_eq!(leave[19], 2, "Leave detail = NotifyInferior");
    let leave_window = u32::from_le_bytes([leave[24], leave[25], leave[26], leave[27]]);
    assert_eq!(leave_window, ROOT_WINDOW.0, "Leave is on the root ancestor");
}

/// XIGrabDevice whose `grab_window` is the very window the pointer
/// already sits in must emit NO pointer crossing — Xorg's
/// `DoEnterLeaveEvents` early-returns when `fromWin == toWin`
/// (dix/enterleave.c). mutter/muffin grabs its own full-screen
/// clutter stage on every button press; the pre-fix code emitted an
/// unconditional `XI_Enter(NotifyGrab)` on the grab window, which
/// desynced cinnamon's XEmbed-systray click-forward state machine —
/// pamac's tray icon became unclickable (HW trace 2026-06-22). The
/// sprite is resolved from the live pointer position, matching the
/// production handler.
#[test]
fn xi_grab_device_self_grab_emits_no_crossing() {
    use std::io::Read;

    const CLIENT_ID: u32 = 1;
    // Window covering the pointer position (RecordingBackend's
    // query_pointer reports (0,0)), so it is the sprite window AND
    // the grab window.
    const STAGE_XID: u32 = 0x0010_0070;
    const HOST_XID: u32 = 0x0040_0070;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT_ID);
    let mut backend = RecordingBackend::new();

    state.resources.create_window(
        yserver_protocol::x11::ClientId(CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(STAGE_XID),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 2560,
            height: 1440,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    {
        let w = state
            .resources
            .window_mut(ResourceId(STAGE_XID))
            .expect("window installed");
        w.host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(HOST_XID));
    }
    let _ = state.resources.map_window(ResourceId(STAGE_XID));
    // Precondition: the pointer (0,0) really does resolve to the
    // grab window, otherwise the test would pass trivially.
    assert_eq!(
        state.root_pointer_target_at(0, 0).map(|h| h.0),
        Some(ResourceId(STAGE_XID)),
        "sprite must be the grab window for this fixture"
    );

    // XIGrabDevice pointer (device 2) on the stage window.
    let mut body = Vec::with_capacity(24);
    body.extend_from_slice(&STAGE_XID.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // time
    body.extend_from_slice(&0u32.to_le_bytes()); // cursor
    body.extend_from_slice(&2u16.to_le_bytes()); // deviceid
    body.extend_from_slice(&[1, 1, 1, 0]); // mode, paired, owner_events, pad
    body.extend_from_slice(&0u16.to_le_bytes()); // mask_len
    body.extend_from_slice(&[0u8; 2]);
    let header = yserver_protocol::x11::RequestHeader {
        opcode: 131,
        data: 51,
        length_units: 7,
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
    .expect("XIGrabDevice");

    peer.set_nonblocking(true).unwrap();
    let mut wire = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        match peer.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => wire.extend_from_slice(&tmp[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
    // Scan for any XI2 GenericEvent that is an Enter (evtype 7) or
    // Leave (evtype 8) from the XInput extension (major 137). There
    // must be none — a self-grab is a no-op transition.
    let crossing = (0..wire.len().saturating_sub(10)).find(|&i| {
        wire[i] == 35
            && wire[i + 1] == 137
            && matches!(u16::from_le_bytes([wire[i + 8], wire[i + 9]]), 7 | 8)
    });
    assert!(
        crossing.is_none(),
        "self-grab must emit no XI_Enter/XI_Leave crossing; got {} wire bytes: {:?}",
        wire.len(),
        &wire[..wire.len().min(140)],
    );
}

/// #94 fast-click acceptance: press withheld by muffin's sync XI2
/// passive grab; the RELEASE arrives during the freeze and is queued;
/// XIAllowEvents(ReplayDevice) replays the press (installing the
/// implicit grab on the natural recipient) and the queue drain must
/// deliver the release UNDER that grab (Xorg PlayReleasedEvents →
/// DeliverGrabbedEvent) — not re-hit-test it.
#[test]
fn xi_replay_device_then_queued_release_follows_implicit_grab() {
    use crate::{
        backend::Backend,
        host_x11::{HostPointerEvent, PointerEventKind},
        resources::ROOT_VISUAL,
    };
    const WM: u32 = 1;
    const APP: u32 = 2;
    const GRAB_WIN: u32 = 0x0010_0051;
    const APP_WIN: u32 = 0x0020_0052;
    const HOST_XID: u32 = 0xCAFE_0001;

    let mut state = ServerState::new();
    let _wm_peer = install_client(&mut state, WM);
    let mut app_peer = install_client(&mut state, APP);
    let mut backend = RecordingBackend::new();
    app_peer.set_nonblocking(true).expect("nonblocking");

    for (client, win) in [(WM, GRAB_WIN), (APP, APP_WIN)] {
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
        .get_mut(&APP)
        .expect("app client")
        .event_masks
        .insert(ResourceId(APP_WIN), 0x0000_000c); // press|release
    Backend::register_top_level(&mut backend, None, ResourceId(APP_WIN), HOST_XID)
        .expect("register host xid");

    // Frozen sync passive grab held by the WM, activating press stored,
    // and the fast release already QUEUED (it arrived mid-freeze; the
    // queue gate decremented buttons_down and withheld it).
    set_test_pointer_grab(&mut state, WM, GRAB_WIN, true, false);
    let press = HostPointerEvent {
        origin: crate::core_loop::message::InputOrigin::XTest(4),
        kind: PointerEventKind::ButtonPress,
        host_xid: HOST_XID,
        detail: 1,
        time: 1000,
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
    let release = HostPointerEvent {
        origin: crate::core_loop::message::InputOrigin::XTest(4),
        kind: PointerEventKind::ButtonRelease,
        time: 1010,
        state: 0x100,
        ..press
    };
    state
        .sync_pending
        .push_back(crate::server::PendingSyncEvent {
            device: crate::xinput::DEVICEID_MASTER_POINTER,
            event: crate::server::QueuedInputEvent::HostPointer(release),
        });

    // XIAllowEvents(ReplayDevice) from the WM.
    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&0u32.to_le_bytes()); // time
    body.extend_from_slice(&2u16.to_le_bytes()); // deviceid = master ptr
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
        ClientId(WM),
        SequenceNumber(1),
        header,
        &body,
    )
    .expect("allow events");

    // The replayed press delivered to APP + the queued release drained
    // UNDER the freshly-installed implicit grab.
    let bytes = read_all_available(&mut app_peer);
    let (mut saw_press, mut saw_release) = (false, false);
    let mut off = 0usize;
    while off + 32 <= bytes.len() {
        match bytes[off] & 0x7F {
            4 => saw_press = true,
            5 => {
                saw_release = true;
                assert_eq!(
                    &bytes[off + 12..off + 16],
                    &APP_WIN.to_le_bytes(),
                    "queued release must deliver on the implicit grab window"
                );
            }
            _ => {}
        }
        off += 32;
    }
    assert!(saw_press, "replayed press must reach the natural target");
    assert!(
        saw_release,
        "queued release must follow the implicit grab owner (fast click)"
    );
    assert!(
        state.active_pointer_grab.is_none(),
        "release completes the click — implicit grab torn down"
    );
    assert_eq!(
        state.buttons_down, 0,
        "the queued release leaves the master clear"
    );
    assert_eq!(
        state.xi_devices.device(4).unwrap().buttons_down,
        0,
        "the queued release leaves XTEST 4 clear"
    );
}

/// An implicit grab is a REAL grab for request purposes (Xorg shares
/// deviceGrab.grab): another client's GrabPointer during it fails
/// AlreadyGrabbed (events.c:5240-5243); the owner's GrabPointer
/// replaces it; the owner's UngrabPointer releases it (events.c:5155).
#[test]
fn implicit_grab_interacts_with_grab_requests_like_a_real_grab() {
    use crate::server::ActivePointerGrab;
    const OWNER: u32 = 1;
    const INTRUDER: u32 = 2;
    const WIN: u32 = 0x0010_0001;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    let mut owner_peer = install_client(&mut state, OWNER);
    let mut intruder_peer = install_client(&mut state, INTRUDER);
    owner_peer.set_nonblocking(true).unwrap();
    intruder_peer.set_nonblocking(true).unwrap();
    state.resources.create_window(
        ClientId(OWNER),
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
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(WIN));

    let install_implicit = |state: &mut ServerState| {
        state.active_pointer_grab = Some(ActivePointerGrab {
            owner: ClientId(OWNER),
            grab_window: ResourceId(WIN),
            event_mask: 0x000c,
            cursor: ResourceId(0),
            time: 1000,
            owner_events: false,
            via_xi2: false,
            implicit: true,
            passive: false,
            xi2_mask: 0,
        });
        state.last_pointer_grab_time = 1000;
    };
    let grab_body = || {
        let mut body = Vec::with_capacity(20);
        body.extend_from_slice(&WIN.to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes()); // event-mask
        body.push(1); // pointer-mode async
        body.push(1); // keyboard-mode async
        body.extend_from_slice(&0u32.to_le_bytes()); // confine-to
        body.extend_from_slice(&0u32.to_le_bytes()); // cursor
        body.extend_from_slice(&0u32.to_le_bytes()); // time CurrentTime
        body
    };
    let header = RequestHeader {
        opcode: 26,
        data: 0,
        length_units: 6,
    };
    let grab_status = |peer: &mut std::os::unix::net::UnixStream| -> u8 {
        let wire = read_all_available(peer);
        assert!(wire.len() >= 32, "expected a GrabPointer reply");
        assert_eq!(wire[0], 1, "reply, not an event/error");
        wire[1]
    };

    // (a) Another client's GrabPointer → AlreadyGrabbed, state untouched.
    install_implicit(&mut state);
    handle_grab_pointer(
        &mut state,
        &mut backend,
        ClientId(INTRUDER),
        SequenceNumber(1),
        header,
        &grab_body(),
    )
    .expect("grab pointer");
    assert_eq!(grab_status(&mut intruder_peer), 1, "AlreadyGrabbed");
    assert!(
        state
            .active_pointer_grab
            .is_some_and(|g| g.implicit && g.owner == ClientId(OWNER)),
        "implicit grab untouched by the failed request"
    );

    // (b) The owner's GrabPointer replaces its implicit grab.
    handle_grab_pointer(
        &mut state,
        &mut backend,
        ClientId(OWNER),
        SequenceNumber(2),
        header,
        &grab_body(),
    )
    .expect("grab pointer");
    assert_eq!(grab_status(&mut owner_peer), 0, "GrabSuccess");
    assert!(
        state.active_pointer_grab.is_some_and(|g| !g.implicit),
        "explicit grab replaced the implicit one"
    );

    // (c) Owner's UngrabPointer releases its own implicit grab; a
    // non-owner's UngrabPointer is a no-op.
    install_implicit(&mut state);
    let ungrab_body = 0u32.to_le_bytes(); // time CurrentTime
    handle_ungrab_pointer(
        &mut state,
        &mut backend,
        ClientId(INTRUDER),
        SequenceNumber(3),
        &ungrab_body,
    )
    .expect("ungrab pointer");
    assert!(
        state.active_pointer_grab.is_some(),
        "non-owner UngrabPointer must not touch the implicit grab"
    );
    handle_ungrab_pointer(
        &mut state,
        &mut backend,
        ClientId(OWNER),
        SequenceNumber(4),
        &ungrab_body,
    )
    .expect("ungrab pointer");
    assert!(
        state.active_pointer_grab.is_none(),
        "owner UngrabPointer releases the implicit grab (Xorg SameClient)"
    );
}

/// #94 slow-click acceptance (the traced Cinnamon shape): muffin's
/// sync XI2 passive grab withholds the press; XIAllowEvents(
/// XIReplayDevice) re-delivers it to Steam (XI2-only selector),
/// installing the implicit grab; muffin then MUTATES THE TREE
/// (click-to-focus reparents/restacks) before the user releases.
/// The release must follow the implicit grab to Steam — pre-fix it
/// was re-hit-tested against the mutated tree and lost (steam.xtrace:
/// 164 XI2 presses vs 28 releases → stuck button).
#[test]
fn xi_replay_device_press_then_tree_mutation_release_follows_grab() {
    use crate::{
        backend::Backend,
        core_loop::pointer_fanout::pointer_event_fanout_to_state,
        host_x11::{HostPointerEvent, PointerEventKind},
        resources::ROOT_VISUAL,
    };
    const WM: u32 = 1;
    const APP: u32 = 2;
    const GRAB_WIN: u32 = 0x0010_0051; // WM's passive-grab window
    const APP_WIN: u32 = 0x0020_0052; // Steam-like, XI2-only selector
    const FRAME_WIN: u32 = 0x0010_0060; // WM frame created mid-click
    const COVER_WIN: u32 = 0x0010_0061; // WM window the release re-hits
    const HOST_APP: u32 = 0xCAFE_0001;
    const HOST_COVER: u32 = 0xCAFE_0002;
    const XI2_MASTER: u16 = 2;

    let mut state = ServerState::new();
    let mut wm_peer = install_client(&mut state, WM);
    let mut app_peer = install_client(&mut state, APP);
    let mut backend = RecordingBackend::new();
    wm_peer.set_nonblocking(true).unwrap();
    app_peer.set_nonblocking(true).unwrap();

    for (client, win) in [
        (WM, GRAB_WIN),
        (APP, APP_WIN),
        (WM, FRAME_WIN),
        (WM, COVER_WIN),
    ] {
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
    // Steam shape: cooked XI2 buttons, NO core mask.
    state
        .clients
        .get_mut(&APP)
        .expect("app")
        .xi2_masks
        .insert((ResourceId(APP_WIN), XI2_MASTER), (1 << 4) | (1 << 5));
    // The WM selects XI2 buttons on the covering window — a re-hit-test
    // would deliver the release THERE.
    state
        .clients
        .get_mut(&WM)
        .expect("wm")
        .xi2_masks
        .insert((ResourceId(COVER_WIN), XI2_MASTER), (1 << 4) | (1 << 5));
    Backend::register_top_level(&mut backend, None, ResourceId(APP_WIN), HOST_APP)
        .expect("register app");
    Backend::register_top_level(&mut backend, None, ResourceId(COVER_WIN), HOST_COVER)
        .expect("register cover");

    // Frozen sync passive grab held by the WM, press stored.
    set_test_pointer_grab(&mut state, WM, GRAB_WIN, true, false);
    let press = HostPointerEvent {
        origin: crate::core_loop::message::InputOrigin::XTest(4),
        kind: PointerEventKind::ButtonPress,
        host_xid: HOST_APP,
        detail: 1,
        time: 1000,
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

    // XIAllowEvents(ReplayDevice) from the WM.
    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&2u16.to_le_bytes());
    body.push(2); // ReplayDevice
    body.push(0);
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(WM),
        SequenceNumber(1),
        yserver_protocol::x11::RequestHeader {
            opcode: 131,
            data: 53,
            length_units: 3,
        },
        &body,
    )
    .expect("allow events");

    let xge_events = |bytes: &[u8]| -> Vec<(u16, u32)> {
        let mut found = Vec::new();
        let mut off = 0usize;
        while off + 32 <= bytes.len() {
            let advance = if bytes[off] == 35 {
                found.push((
                    u16::from_le_bytes([bytes[off + 8], bytes[off + 9]]),
                    u32::from_le_bytes(bytes[off + 24..off + 28].try_into().unwrap()),
                ));
                32 + u32::from_le_bytes(bytes[off + 4..off + 8].try_into().unwrap()) as usize * 4
            } else {
                32
            };
            off += advance;
        }
        found
    };
    assert!(
        xge_events(&read_all_available(&mut app_peer)).contains(&(4, APP_WIN)),
        "replayed press must reach APP as XI2 on its window"
    );
    assert!(
        state.active_pointer_grab.is_some_and(|g| g.implicit
            && g.via_xi2
            && g.owner == ClientId(APP)
            && g.grab_window == ResourceId(APP_WIN)),
        "replayed press installs the via_xi2 implicit grab (#94 crux)"
    );

    // muffin-style mutation between press and release: reparent the
    // app window into a WM frame and let a WM window cover the spot.
    let _ = state
        .resources
        .reparent_window(yserver_protocol::x11::ReparentWindowRequest {
            window: ResourceId(APP_WIN),
            parent: ResourceId(FRAME_WIN),
            x: 0,
            y: 0,
        });
    let _ = state.resources.map_window(ResourceId(APP_WIN));

    // Natural release now resolves over the WM's covering window.
    let xid_map = backend.xid_map().clone();
    let release = HostPointerEvent {
        origin: crate::core_loop::message::InputOrigin::XTest(4),
        kind: PointerEventKind::ButtonRelease,
        host_xid: HOST_COVER,
        time: 1200,
        state: 0x100,
        ..press
    };
    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, release, true, false);

    assert!(
        xge_events(&read_all_available(&mut app_peer)).contains(&(5, APP_WIN)),
        "release must follow the implicit grab to APP on the grab window"
    );
    assert!(
        !xge_events(&read_all_available(&mut wm_peer))
            .iter()
            .any(|(evtype, _)| *evtype == 5),
        "the grab captures the release away from the re-hit-tested window"
    );
    assert!(
        state.active_pointer_grab.is_none(),
        "click complete — implicit grab torn down"
    );
}

/// Xorg oracle from `steam-xorg.xtrace` connection 019 — the canonical
/// Steam-webhelper click. The client selects XI2 Button{Press,Release}+
/// Motion on its window W (`XISelectEvents win=W device=0 mask=0x001c0ff2`),
/// receives the `ButtonPress`, then establishes its OWN grab on the master
/// pointer BEFORE the release (`XIGrabDevice grab_window=W device=2
/// grab_mode=Async owner_events=true masks=0x001c0070`), and Xorg still
/// delivers the matching `ButtonRelease` to the client on W — press count
/// equals release count — after which the client `XIUngrabDevice`s.
///
/// This encodes Xorg's observed behavior ONLY. On yserver a delivered
/// press installs an implicit pointer grab, and the client's mid-click
/// `XIGrabDevice` overwrites that slot; the question this pins is whether
/// the release survives that overlap the way it does under Xorg.
#[test]
fn xi_client_grab_device_mid_click_delivers_release_xorg_conn019() {
    use crate::{
        backend::Backend,
        core_loop::pointer_fanout::pointer_event_fanout_to_state,
        host_x11::{HostPointerEvent, PointerEventKind},
        resources::ROOT_VISUAL,
    };
    const APP: u32 = 1;
    const APP_WIN: u32 = 0x0044_000b; // window id shape from the trace
    const HOST_APP: u32 = 0xCAFE_0001;
    const XI2_MASTER: u16 = 2;

    let mut state = ServerState::new();
    let mut app_peer = install_client(&mut state, APP);
    app_peer.set_nonblocking(true).unwrap();
    let mut backend = RecordingBackend::new();

    state.resources.create_window(
        ClientId(APP),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(APP_WIN),
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
    let _ = state.resources.map_window(ResourceId(APP_WIN));
    // Xorg XISelectEvents mask 0x001c0ff2 → Button{Press,Release}+Motion (+more).
    state.clients.get_mut(&APP).expect("app").xi2_masks.insert(
        (ResourceId(APP_WIN), XI2_MASTER),
        (1 << 4) | (1 << 5) | (1 << 6),
    );
    Backend::register_top_level(&mut backend, None, ResourceId(APP_WIN), HOST_APP)
        .expect("register app");
    let xid_map = backend.xid_map().clone();

    let xge = |bytes: &[u8]| -> Vec<(u16, u32)> {
        let mut found = Vec::new();
        let mut off = 0usize;
        while off + 32 <= bytes.len() {
            let advance = if bytes[off] == 35 {
                found.push((
                    u16::from_le_bytes([bytes[off + 8], bytes[off + 9]]),
                    u32::from_le_bytes(bytes[off + 24..off + 28].try_into().unwrap()),
                ));
                32 + u32::from_le_bytes(bytes[off + 4..off + 8].try_into().unwrap()) as usize * 4
            } else {
                32
            };
            off += advance;
        }
        found
    };

    // 1. ButtonPress @ W (Xorg: delivered to the selector on its window).
    let press = HostPointerEvent {
        origin: crate::core_loop::message::InputOrigin::XTest(4),
        kind: PointerEventKind::ButtonPress,
        host_xid: HOST_APP,
        detail: 1,
        time: 1000,
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
    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, press, true, false);
    let after_press = xge(&read_all_available(&mut app_peer));
    let presses = after_press.iter().filter(|(t, _)| *t == 4).count();
    assert!(
        after_press.contains(&(4, APP_WIN)),
        "Xorg: the ButtonPress is delivered to the XI2 selector on its own window"
    );

    // 2. Client's own XIGrabDevice on the master pointer, owner_events=true,
    //    grab_window=W, mask=Button{Press,Release}+Motion (Xorg masks=0x001c0070).
    let mut body = Vec::with_capacity(28);
    body.extend_from_slice(&APP_WIN.to_le_bytes()); // grab_window
    body.extend_from_slice(&0u32.to_le_bytes()); // time = CurrentTime
    body.extend_from_slice(&0u32.to_le_bytes()); // cursor = None
    body.extend_from_slice(&2u16.to_le_bytes()); // device = master pointer
    body.extend_from_slice(&[1, 1, 1, 0]); // async, async, owner_events=1, pad
    body.extend_from_slice(&1u16.to_le_bytes()); // mask_len (one 4-byte unit)
    body.extend_from_slice(&[0u8; 2]); // pad
    body.extend_from_slice(&((1u32 << 4) | (1 << 5) | (1 << 6)).to_le_bytes());
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(APP),
        SequenceNumber(2),
        yserver_protocol::x11::RequestHeader {
            opcode: 131,
            data: 51, // XIGrabDevice
            length_units: 8,
        },
        &body,
    )
    .expect("XIGrabDevice");
    let _ = read_all_available(&mut app_peer); // drain grab reply, if any

    // 3. ButtonRelease @ W, still over W (Xorg: delivered to the client on W,
    //    even though the client established its own grab mid-click).
    let release = HostPointerEvent {
        origin: crate::core_loop::message::InputOrigin::XTest(4),
        kind: PointerEventKind::ButtonRelease,
        time: 1100,
        state: 0x100,
        ..press
    };
    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, release, true, false);
    let after_release = xge(&read_all_available(&mut app_peer));
    let releases = after_release.iter().filter(|(t, _)| *t == 5).count();

    assert!(
        after_release.contains(&(5, APP_WIN)),
        "Xorg: the ButtonRelease must still reach the client on its window even \
             though the client established its own XIGrabDevice mid-click \
             (steam-xorg.xtrace conn 019); got events {after_release:?}"
    );
    assert_eq!(
        presses, releases,
        "Xorg: press count equals release count for the grab client \
             (presses={presses}, releases={releases})"
    );

    // 4. Client XIUngrabDevice(device=2) — grab released.
    let mut ungrab = Vec::with_capacity(8);
    ungrab.extend_from_slice(&0u32.to_le_bytes()); // time
    ungrab.extend_from_slice(&2u16.to_le_bytes()); // device = master pointer
    ungrab.extend_from_slice(&[0u8; 2]); // pad
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(APP),
        SequenceNumber(3),
        yserver_protocol::x11::RequestHeader {
            opcode: 131,
            data: 52, // XIUngrabDevice
            length_units: 3,
        },
        &ungrab,
    )
    .expect("XIUngrabDevice");
    assert!(
        state.active_pointer_grab.is_none(),
        "after the client ungrabs, no pointer grab remains"
    );
}

/// Xorg oracle: `steam-xorg.xtrace` conn 019 repeats the grab-dance click
/// (`ButtonPress → XIGrabDevice → ButtonRelease → XIUngrabDevice`) ~40
/// times, and EVERY click delivers a matched press+release with no residue.
/// The GH #94 report ("several clicks before one registers, growing
/// backlog") is a state-accumulation symptom, so this drives the same
/// cycle N times and asserts each click stays balanced and leaves no
/// stuck grab / lingering pointer-button state behind.
#[test]
fn xi_repeated_grab_dance_clicks_stay_balanced_xorg_conn019() {
    use crate::{
        backend::Backend,
        core_loop::pointer_fanout::pointer_event_fanout_to_state,
        host_x11::{HostPointerEvent, PointerEventKind},
        resources::ROOT_VISUAL,
    };
    const APP: u32 = 1;
    const APP_WIN: u32 = 0x0044_000b;
    const HOST_APP: u32 = 0xCAFE_0001;
    const XI2_MASTER: u16 = 2;

    let mut state = ServerState::new();
    let mut app_peer = install_client(&mut state, APP);
    app_peer.set_nonblocking(true).unwrap();
    let mut backend = RecordingBackend::new();

    state.resources.create_window(
        ClientId(APP),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(APP_WIN),
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
    let _ = state.resources.map_window(ResourceId(APP_WIN));
    state.clients.get_mut(&APP).expect("app").xi2_masks.insert(
        (ResourceId(APP_WIN), XI2_MASTER),
        (1 << 4) | (1 << 5) | (1 << 6),
    );
    Backend::register_top_level(&mut backend, None, ResourceId(APP_WIN), HOST_APP)
        .expect("register app");
    let xid_map = backend.xid_map().clone();

    let xge = |bytes: &[u8]| -> Vec<(u16, u32)> {
        let mut found = Vec::new();
        let mut off = 0usize;
        while off + 32 <= bytes.len() {
            let advance = if bytes[off] == 35 {
                found.push((
                    u16::from_le_bytes([bytes[off + 8], bytes[off + 9]]),
                    u32::from_le_bytes(bytes[off + 24..off + 28].try_into().unwrap()),
                ));
                32 + u32::from_le_bytes(bytes[off + 4..off + 8].try_into().unwrap()) as usize * 4
            } else {
                32
            };
            off += advance;
        }
        found
    };

    let grab_body = |window: u32| {
        let mut body = Vec::with_capacity(28);
        body.extend_from_slice(&window.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&2u16.to_le_bytes());
        body.extend_from_slice(&[1, 1, 1, 0]);
        body.extend_from_slice(&1u16.to_le_bytes());
        body.extend_from_slice(&[0u8; 2]);
        body.extend_from_slice(&((1u32 << 4) | (1 << 5) | (1 << 6)).to_le_bytes());
        body
    };
    let ungrab_body = || {
        let mut body = Vec::with_capacity(8);
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&2u16.to_le_bytes());
        body.extend_from_slice(&[0u8; 2]);
        body
    };

    for i in 0..10u32 {
        let seq = i * 4;
        let base_time = 1000 + i * 100;
        let press = HostPointerEvent {
            origin: crate::core_loop::message::InputOrigin::XTest(4),
            kind: PointerEventKind::ButtonPress,
            host_xid: HOST_APP,
            detail: 1,
            time: base_time,
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
        let _ =
            pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, press, true, false);

        handle_xi2_request(
            &mut state,
            &mut backend,
            None,
            ClientId(APP),
            SequenceNumber((seq + 2) as u16),
            yserver_protocol::x11::RequestHeader {
                opcode: 131,
                data: 51,
                length_units: 8,
            },
            &grab_body(APP_WIN),
        )
        .expect("XIGrabDevice");

        let release = HostPointerEvent {
            origin: crate::core_loop::message::InputOrigin::XTest(4),
            kind: PointerEventKind::ButtonRelease,
            time: base_time + 50,
            state: 0x100,
            ..press
        };
        let _ =
            pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, release, true, false);

        handle_xi2_request(
            &mut state,
            &mut backend,
            None,
            ClientId(APP),
            SequenceNumber((seq + 3) as u16),
            yserver_protocol::x11::RequestHeader {
                opcode: 131,
                data: 52,
                length_units: 3,
            },
            &ungrab_body(),
        )
        .expect("XIUngrabDevice");

        let evs = xge(&read_all_available(&mut app_peer));
        let presses = evs.iter().filter(|(t, _)| *t == 4).count();
        let releases = evs.iter().filter(|(t, _)| *t == 5).count();
        assert!(
            evs.contains(&(4, APP_WIN)) && evs.contains(&(5, APP_WIN)),
            "click {i}: both press and release must reach the client on its window; \
                 got {evs:?}"
        );
        assert_eq!(
            (presses, releases),
            (1, 1),
            "click {i}: exactly one press and one release must be delivered — \
                 balanced-but-duplicated (e.g. 2/2) is also a bug \
                 (presses={presses}, releases={releases})"
        );
        assert!(
            state.active_pointer_grab.is_none(),
            "click {i}: no stuck grab may accumulate between clicks"
        );
        assert_eq!(
            state.buttons_down, 0,
            "click {i}: logical button state must return to 0 after the release"
        );
    }
}

/// Xorg oracle from the FAILING `cinnamon.xtrace` (one Steam click) +
/// `dix/events.c:5240`. The traced sequence that swallows the click:
///   muffin sync-passive-grabs the press, `XIAllowEvents(ReplayDevice)`
///   replays it to Steam; Steam (conn 105) `XIGrabDevice`s the master
///   pointer (device=2, grab_window=its own window, first, never ungrabs);
///   muffin (conn 026) THEN `XIGrabDevice`s the SAME master pointer
///   (device=2, different client); the ButtonRelease is delivered to
///   muffin, and Steam never sees it → the click never completes.
///
/// Xorg rejects the second grab with `AlreadyGrabbed`
/// (`grab && !SameClient`, dix/events.c:5240), so the first grabber keeps
/// the device and the release follows it. This test encodes that rule: a
/// second client's `XIGrabDevice` while another client holds the pointer
/// grab must NOT steal it, and the release must reach the first grabber.
#[test]
fn xi_grab_device_by_other_client_must_not_steal_held_grab_cinnamon_xtrace() {
    use crate::{
        backend::Backend,
        core_loop::pointer_fanout::pointer_event_fanout_to_state,
        host_x11::{HostPointerEvent, PointerEventKind},
        resources::ROOT_VISUAL,
    };
    const STEAM: u32 = 1;
    const MUFFIN: u32 = 2;
    const STEAM_WIN: u32 = 0x0260_000d; // conn 105 grab_window
    const MUFFIN_WIN: u32 = 0x00e0_0011; // conn 026 grab_window
    const HOST_STEAM: u32 = 0xCAFE_0001;
    const XI2_MASTER: u16 = 2;

    let mut state = ServerState::new();
    let mut steam_peer = install_client(&mut state, STEAM);
    let mut muffin_peer = install_client(&mut state, MUFFIN);
    steam_peer.set_nonblocking(true).unwrap();
    muffin_peer.set_nonblocking(true).unwrap();
    let mut backend = RecordingBackend::new();

    for (client, win, x) in [(STEAM, STEAM_WIN, 0i16), (MUFFIN, MUFFIN_WIN, 500i16)] {
        state.resources.create_window(
            ClientId(client),
            yserver_protocol::x11::CreateWindowRequest {
                depth: 24,
                window: ResourceId(win),
                parent: ROOT_WINDOW,
                x,
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
    // Both select XI2 Button{Press,Release} on their own windows, so a
    // misrouted release to muffin would be observable on its wire.
    state
        .clients
        .get_mut(&STEAM)
        .unwrap()
        .xi2_masks
        .insert((ResourceId(STEAM_WIN), XI2_MASTER), (1 << 4) | (1 << 5));
    state
        .clients
        .get_mut(&MUFFIN)
        .unwrap()
        .xi2_masks
        .insert((ResourceId(MUFFIN_WIN), XI2_MASTER), (1 << 4) | (1 << 5));
    Backend::register_top_level(&mut backend, None, ResourceId(STEAM_WIN), HOST_STEAM)
        .expect("register steam");
    let xid_map = backend.xid_map().clone();

    let xge = |bytes: &[u8]| -> Vec<(u16, u32)> {
        let mut found = Vec::new();
        let mut off = 0usize;
        while off + 32 <= bytes.len() {
            let advance = if bytes[off] == 35 {
                found.push((
                    u16::from_le_bytes([bytes[off + 8], bytes[off + 9]]),
                    u32::from_le_bytes(bytes[off + 24..off + 28].try_into().unwrap()),
                ));
                32 + u32::from_le_bytes(bytes[off + 4..off + 8].try_into().unwrap()) as usize * 4
            } else {
                32
            };
            off += advance;
        }
        found
    };
    let grab_body = |window: u32, owner_events: u8| {
        let mut body = Vec::with_capacity(28);
        body.extend_from_slice(&window.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes()); // time
        body.extend_from_slice(&0u32.to_le_bytes()); // cursor
        body.extend_from_slice(&2u16.to_le_bytes()); // device = master pointer
        body.extend_from_slice(&[1, 1, owner_events, 0]);
        body.extend_from_slice(&1u16.to_le_bytes()); // mask_len
        body.extend_from_slice(&[0u8; 2]);
        body.extend_from_slice(&((1u32 << 4) | (1 << 5) | (1 << 6)).to_le_bytes());
        body
    };
    let do_grab = |state: &mut ServerState,
                   backend: &mut RecordingBackend,
                   client: u32,
                   seq: u16,
                   window: u32,
                   owner_events: u8| {
        handle_xi2_request(
            state,
            backend,
            None,
            ClientId(client),
            SequenceNumber(seq),
            yserver_protocol::x11::RequestHeader {
                opcode: 131,
                data: 51,
                length_units: 8,
            },
            &grab_body(window, owner_events),
        )
        .expect("XIGrabDevice");
    };

    // Press @ STEAM_WIN (post-ReplayDevice replay to Steam): Steam gets it,
    // installs the implicit grab.
    let press = HostPointerEvent {
        origin: crate::core_loop::message::InputOrigin::XTest(4),
        kind: PointerEventKind::ButtonPress,
        host_xid: HOST_STEAM,
        detail: 1,
        time: 0xa040,
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
    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, press, true, false);
    let _ = read_all_available(&mut steam_peer);

    // Steam grabs the master pointer FIRST (conn 105, never ungrabs).
    do_grab(&mut state, &mut backend, STEAM, 10, STEAM_WIN, 1);
    assert_eq!(
        state
            .active_pointer_grab
            .map(|grab| (grab.owner, grab.grab_window)),
        Some((ClientId(STEAM), ResourceId(STEAM_WIN))),
        "precondition: Steam holds the master-pointer grab"
    );

    // Muffin then grabs the SAME master pointer (conn 026). Xorg:
    // AlreadyGrabbed → rejected; Steam keeps the grab.
    do_grab(&mut state, &mut backend, MUFFIN, 20, MUFFIN_WIN, 0);
    assert_eq!(
        state
            .active_pointer_grab
            .map(|grab| (grab.owner, grab.grab_window)),
        Some((ClientId(STEAM), ResourceId(STEAM_WIN))),
        "Xorg (dix/events.c:5240): a second client's XIGrabDevice while \
             another client holds the pointer grab must return AlreadyGrabbed \
             and must NOT steal the grab"
    );
    // The rejected grab's reply must carry status=1 (AlreadyGrabbed) at
    // byte offset 8 (xXIGrabDeviceReply). muffin has received nothing else
    // yet (the press went to Steam), so its wire starts with this reply.
    let muffin_reply = read_all_available(&mut muffin_peer);
    assert!(
        muffin_reply.len() >= 9 && muffin_reply[0] == 1 && muffin_reply[8] == 1,
        "muffin's contending XIGrabDevice must reply AlreadyGrabbed(1) at \
             offset 8; got {:?}",
        &muffin_reply[..muffin_reply.len().min(12)]
    );

    // Release over Steam's window — must reach the grab holder (Steam),
    // not muffin.
    let release = HostPointerEvent {
        origin: crate::core_loop::message::InputOrigin::XTest(4),
        kind: PointerEventKind::ButtonRelease,
        time: 0xa0e3,
        state: 0x100,
        ..press
    };
    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, release, true, false);

    let steam_evs = xge(&read_all_available(&mut steam_peer));
    let muffin_evs = xge(&read_all_available(&mut muffin_peer));
    assert!(
        steam_evs.iter().any(|(t, _)| *t == 5),
        "the ButtonRelease must be delivered to the grab holder (Steam); \
             got steam={steam_evs:?} muffin={muffin_evs:?}"
    );
    assert!(
        !muffin_evs.iter().any(|(t, _)| *t == 5),
        "the release must NOT be stolen by the second (rejected) grabber; \
             muffin={muffin_evs:?}"
    );
}

/// Cinnamon dialog acceptance pin. The shell activates a synchronous XI2
/// passive pointer grab, then issues XIReplayDevice. Xorg transitions the
/// device to NOT_GRABBED before replay (`dix/events.c:1898`), so the app's
/// later XIGrabDevice must succeed without superseding a foreign grab.
#[test]
fn xi_dialog_grab_succeeds_over_foreign_passive_grab_cinnamon() {
    use crate::{
        backend::Backend,
        core_loop::pointer_fanout::pointer_event_fanout_to_state,
        host_x11::{HostPointerEvent, PointerEventKind},
        server::PassiveButtonGrab,
    };
    const SHELL: u32 = 1;
    const APP: u32 = 2;
    const GRAB_WIN: u32 = 0x0070_0003;
    const APP_WIN: u32 = 0x0080_0003; // the app dialog's popup
    const HOST_XID: u32 = 0xCAFE_0001;

    let mut state = ServerState::new();
    let _shell_peer = install_client(&mut state, SHELL);
    let mut app_peer = install_client(&mut state, APP);
    app_peer.set_nonblocking(true).unwrap();
    let mut backend = RecordingBackend::new();

    for (client, win) in [(SHELL, GRAB_WIN), (APP, APP_WIN)] {
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
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = state.resources.map_window(ResourceId(win));
    }
    state.button_grabs.push(PassiveButtonGrab {
        device_id: 0,
        owner: ClientId(SHELL),
        grab_window: ResourceId(GRAB_WIN),
        button: 1,
        modifiers: 0x8000,
        owner_events: false,
        event_mask: (1 << 4) | (1 << 5),
        pointer_mode: 0,
        keyboard_mode: 1,
        confine_to: ResourceId(0),
        via_xi2: true,
    });
    Backend::register_top_level(&mut backend, None, ResourceId(GRAB_WIN), HOST_XID)
        .expect("register");
    let xid_map = backend.xid_map().clone();
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
    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, press, true, false);
    assert!(
        state
            .active_pointer_grab
            .is_some_and(|grab| { grab.owner == ClientId(SHELL) && grab.passive && grab.via_xi2 })
    );

    let mut allow_body = Vec::with_capacity(8);
    allow_body.extend_from_slice(&0u32.to_le_bytes());
    allow_body.extend_from_slice(&2u16.to_le_bytes());
    allow_body.push(2); // XIReplayDevice
    allow_body.push(0);
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(SHELL),
        SequenceNumber(1),
        yserver_protocol::x11::RequestHeader {
            opcode: 131,
            data: 53,
            length_units: 3,
        },
        &allow_body,
    )
    .expect("XIAllowEvents ReplayDevice");
    assert!(
        !state
            .active_pointer_grab
            .is_some_and(|grab| grab.owner == ClientId(SHELL)),
        "ReplayDevice must deactivate the foreign passive pointer grab"
    );
    assert!(
        !state
            .active_keyboard_grab
            .is_some_and(|grab| grab.owner == ClientId(SHELL)),
        "no foreign keyboard grab is held at grab time"
    );

    let mut grab_body = Vec::with_capacity(28);
    grab_body.extend_from_slice(&APP_WIN.to_le_bytes());
    grab_body.extend_from_slice(&0u32.to_le_bytes());
    grab_body.extend_from_slice(&0u32.to_le_bytes());
    grab_body.extend_from_slice(&2u16.to_le_bytes());
    grab_body.extend_from_slice(&[1, 1, 1, 0]);
    grab_body.extend_from_slice(&1u16.to_le_bytes());
    grab_body.extend_from_slice(&[0u8; 2]);
    grab_body.extend_from_slice(&((1u32 << 4) | (1 << 5) | (1 << 6)).to_le_bytes());
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(APP),
        SequenceNumber(2),
        yserver_protocol::x11::RequestHeader {
            opcode: 131,
            data: 51,
            length_units: 8,
        },
        &grab_body,
    )
    .expect("XIGrabDevice");
    let ptr_reply = read_all_available(&mut app_peer);
    let reply_start = ptr_reply.len().saturating_sub(32);
    assert!(
        ptr_reply.len() >= 32 && ptr_reply[reply_start] == 1 && ptr_reply[reply_start + 8] == 0,
        "pointer XIGrabDevice must return Success(0), not AlreadyGrabbed(1), \
             over a foreign passive grab; got {:?}",
        &ptr_reply[..ptr_reply.len().min(12)]
    );
}

/// Pure Xorg guard: an implicit grab held by another client is still an
/// active grab and must return AlreadyGrabbed. The real MATE combo works
/// because its deeper XI2 leaf delivery attributes the implicit grab to
/// the app itself, covered by `mate_combo_implicit_grab_attributed_to_app_leaf_xi2`.
#[test]
fn xi_grab_device_over_foreign_implicit_grab_returns_already_grabbed() {
    use crate::server::ActivePointerGrab;
    const CORE_ANCESTOR: u32 = 13; // implicit-grab owner (core-first)
    const APP: u32 = 46; // the GTK app opening the combo
    const IMPLICIT_WIN: u32 = 0x0050_069b;
    const COMBO_WIN: u32 = 0x0150_0ac3;

    let mut state = ServerState::new();
    let mut app_peer = install_client(&mut state, APP);
    let _anc_peer = install_client(&mut state, CORE_ANCESTOR);
    app_peer.set_nonblocking(true).unwrap();
    let mut backend = RecordingBackend::new();

    for (client, win) in [(CORE_ANCESTOR, IMPLICIT_WIN), (APP, COMBO_WIN)] {
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
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = state.resources.map_window(ResourceId(win));
    }

    // A core-first implicit grab attributed to the ancestor's core client.
    state.active_pointer_grab = Some(ActivePointerGrab {
        owner: ClientId(CORE_ANCESTOR),
        grab_window: ResourceId(IMPLICIT_WIN),
        event_mask: 0x000c,
        cursor: ResourceId(0),
        time: 100,
        owner_events: false,
        via_xi2: false,
        implicit: true,
        passive: false,
        xi2_mask: 0,
    });

    // The app's combo XIGrabDevice on the master pointer.
    let mut body = Vec::with_capacity(28);
    body.extend_from_slice(&COMBO_WIN.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&2u16.to_le_bytes());
    body.extend_from_slice(&[1, 1, 1, 0]);
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&[0u8; 2]);
    body.extend_from_slice(&((1u32 << 4) | (1 << 5) | (1 << 6)).to_le_bytes());
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(APP),
        SequenceNumber(1),
        yserver_protocol::x11::RequestHeader {
            opcode: 131,
            data: 51,
            length_units: 8,
        },
        &body,
    )
    .expect("XIGrabDevice");

    assert_eq!(
        state.active_pointer_grab.map(|grab| grab.owner),
        Some(ClientId(CORE_ANCESTOR)),
        "the foreign implicit grab must remain untouched"
    );
    let reply = read_all_available(&mut app_peer);
    let reply_start = reply.len().saturating_sub(32);
    assert!(
        reply.len() >= 32 && reply[reply_start] == 1 && reply[reply_start + 8] == 1,
        "grab over a foreign implicit grab must return AlreadyGrabbed(1); got {:?}",
        &reply[..reply.len().min(12)]
    );
}

/// MATE combo regression: Xorg's `DeliverDeviceEvents` walks from the
/// leaf toward root and tries XI2 before core at each window. An XI2
/// selector on the app leaf therefore owns the implicit grab instead of
/// a core selector on its ancestor. The app's popup grab is SameClient.
#[test]
fn mate_combo_implicit_grab_attributed_to_app_leaf_xi2() {
    use crate::{
        backend::Backend,
        core_loop::pointer_fanout::pointer_event_fanout_to_state,
        host_x11::{HostPointerEvent, PointerEventKind},
        resources::ROOT_VISUAL,
    };
    const CORE_ANCESTOR: u32 = 13;
    const APP: u32 = 46;
    let parent = ResourceId(0x0050_069b); // core-ancestor selector window
    let leaf = ResourceId(0x0150_0007); // the XI2 app's window (hit)
    let combo = ResourceId(0x0150_0ac3); // the app's combo popup
    const HOST_LEAF: u32 = 0xCAFE_0001;
    const XI2_ALL_MASTER: u16 = 1;

    let mut state = ServerState::new();
    let _anc_peer = install_client(&mut state, CORE_ANCESTOR);
    let mut app_peer = install_client(&mut state, APP);
    app_peer.set_nonblocking(true).unwrap();
    let mut backend = RecordingBackend::new();

    state.resources.create_window(
        ClientId(CORE_ANCESTOR),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: parent,
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 200,
            height: 200,
            border_width: 0,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(parent);
    for (client, win, par) in [(APP, leaf, parent), (APP, combo, ROOT_WINDOW)] {
        state.resources.create_window(
            ClientId(client),
            yserver_protocol::x11::CreateWindowRequest {
                depth: 24,
                window: win,
                parent: par,
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
        let _ = state.resources.map_window(win);
    }
    // Ancestor: CORE press|release. App leaf: XI2 under XIAllMasterDevices.
    state
        .clients
        .get_mut(&CORE_ANCESTOR)
        .unwrap()
        .event_masks
        .insert(parent, 0x0000_000c);
    state
        .clients
        .get_mut(&APP)
        .unwrap()
        .xi2_masks
        .insert((leaf, XI2_ALL_MASTER), (1 << 4) | (1 << 5));
    Backend::register_top_level(&mut backend, None, leaf, HOST_LEAF).expect("register");
    let xid_map = backend.xid_map().clone();

    // Real click over the app's leaf.
    let press = HostPointerEvent {
        origin: crate::core_loop::message::InputOrigin::XTest(4),
        kind: PointerEventKind::ButtonPress,
        host_xid: HOST_LEAF,
        detail: 1,
        time: 0x7e1b,
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
    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, press, true, false);
    assert_eq!(
        state.active_pointer_grab.map(|grab| grab.owner),
        Some(ClientId(APP)),
        "the deeper XI2 leaf delivery must own the implicit grab"
    );
    assert!(
        state
            .active_pointer_grab
            .is_some_and(|grab| grab.implicit && grab.via_xi2)
    );
    let _ = read_all_available(&mut app_peer);

    // The app opens its combo → XIGrabDevice on the master pointer.
    let mut body = Vec::with_capacity(28);
    body.extend_from_slice(&combo.0.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&2u16.to_le_bytes());
    body.extend_from_slice(&[1, 1, 1, 0]);
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&[0u8; 2]);
    body.extend_from_slice(&((1u32 << 4) | (1 << 5) | (1 << 6)).to_le_bytes());
    handle_xi2_request(
        &mut state,
        &mut backend,
        None,
        ClientId(APP),
        SequenceNumber(1),
        yserver_protocol::x11::RequestHeader {
            opcode: 131,
            data: 51,
            length_units: 8,
        },
        &body,
    )
    .expect("XIGrabDevice");

    assert_eq!(
        state
            .active_pointer_grab
            .map(|grab| (grab.owner, grab.grab_window)),
        Some((ClientId(APP), combo)),
        "the app's SameClient XIGrabDevice must replace its implicit grab"
    );
    let reply = read_all_available(&mut app_peer);
    let reply_start = reply.len().saturating_sub(32);
    assert!(
        reply.len() >= 32 && reply[reply_start] == 1 && reply[reply_start + 8] == 0,
        "combo grab reply must be Success (0), not AlreadyGrabbed (1); got {:?}",
        &reply[..reply.len().min(12)]
    );
}
