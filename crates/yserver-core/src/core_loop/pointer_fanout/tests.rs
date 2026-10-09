use super::*;
use crate::{
    backend::recording::RecordingBackend,
    host_x11::HostXidMap,
    resources::ROOT_WINDOW,
    server::{ScreenSaverActive, ServerState},
};
use yserver_protocol::x11::ClientId;

/// AllowSome state machine pins (Xorg dix/events.c semantics): a
/// master grab freezes ONCE and holds its paired master; FreezeNextEvent
/// re-arms, trips to FrozenWithEvent on a delivered key/button, and
/// deactivation clears the paired device's on-behalf hold.
#[test]
fn xi1_sync_state_machine_pins() {
    use crate::{
        server::{Xi1ActiveGrab, Xi1SyncState},
        xinput::{DEVICEID_MASTER_KEYBOARD as KBD, DEVICEID_MASTER_POINTER as PTR},
    };
    let mut state = ServerState::new();
    let owner = ClientId(7);
    state.xi1_active_grabs.insert(
        PTR,
        Xi1ActiveGrab {
            owner,
            deviceid: PTR,
            grab_window: crate::resources::ROOT_WINDOW,
            owner_events: false,
            this_mode: 0,
            other_mode: 0,
            passive_detail: None,
        },
    );
    // Sync grab activation: this device FrozenNoEvent, paired
    // device held on the grab's behalf.
    xi1_check_grab_for_syncs(&mut state, PTR, owner, true, true);
    assert_eq!(
        state.xi1_frozen[&PTR].state,
        Xi1SyncState::FrozenNoEvent,
        "sync this_mode freezes once at activation"
    );
    assert_eq!(state.xi1_frozen[&KBD].other, Some(owner));
    assert!(state.xi1_frozen[&PTR].frozen());
    assert!(state.xi1_frozen[&KBD].frozen(), "held via sync.other");

    // FreezeNextEvent arming + trip on a delivered button event.
    state.xi1_frozen.get_mut(&PTR).unwrap().state = Xi1SyncState::FreezeNextEvent;
    assert!(!state.xi1_frozen[&PTR].frozen(), "armed ≠ frozen");
    let q = crate::server::Xi1QueuedEvent {
        deviceid: PTR,
        evcode: crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_BUTTON_PRESS_OFFSET,
        detail: 1,
        time: 1,
        root_x: 0,
        root_y: 0,
        event_x: 0,
        event_y: 0,
        state_mask: 0,
        natural_target: crate::resources::ROOT_WINDOW,
        focus_route: crate::server::Xi1FocusRoute::Walk,
        axes: None,
        replay_floor: None,
    };
    xi1_freeze_this_event_if_needed(&mut state, PTR, owner, &q);
    assert_eq!(state.xi1_frozen[&PTR].state, Xi1SyncState::FrozenWithEvent);
    assert!(state.xi1_frozen[&PTR].stored.is_some(), "Replay material");

    // Deactivation thaws this device AND releases the paired hold.
    xi1_deactivate_device_grab(&mut state, PTR);
    assert!(!state.xi1_frozen[&PTR].frozen());
    assert_eq!(state.xi1_frozen[&KBD].other, None);
    assert!(!state.xi1_frozen[&KBD].frozen());
}

#[test]
fn xi_source_removal_grab_cleanup_keeps_registry_for_atomic_unregister() {
    use crate::{
        backend::recording::RecordingBackend,
        core_loop::{DeviceInfo, message::LibinputConfigSnapshot},
        xinput::{InputCapabilities, InputSourceId, XiFacetKind},
    };

    let source = InputSourceId(0xD14);
    let info = DeviceInfo {
        source_id: source,
        enabled: true,
        resume_key: None,
        capabilities: InputCapabilities {
            keyboard: true,
            pointer: true,
            touch: false,
        },
        name: "removal cleanup source".to_owned(),
        device_node: "/dev/input/removal-cleanup".to_owned(),
        sysname: "removal-cleanup".to_owned(),
        vendor_id: 0,
        product_id: 0,
        is_touchpad: false,
        config: LibinputConfigSnapshot::default(),
    };
    let mut state = ServerState::new();
    let ids = state.xi_register_source(&info);
    let mut backend = RecordingBackend::new();

    xi_cleanup_source(&mut state, &mut backend, source);

    assert_eq!(
        state
            .xi_devices
            .source(source)
            .map(|current| current.name.as_str()),
        Some("removal cleanup source"),
        "grab cleanup must leave the source registered until KMS drains state and unregisters both facets together",
    );
    assert_eq!(
        state.xi_devices.facet(source, XiFacetKind::Keyboard),
        Some(ids[0]),
    );
    assert_eq!(
        state.xi_devices.facet(source, XiFacetKind::PointerTouch),
        Some(ids[1]),
    );
}

use crate::server::ClientState;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::Read,
    os::unix::net::UnixStream,
    sync::{Arc, Mutex, atomic::AtomicU16},
};

// Duplicated from process_request.rs::tests. If you change one,
// change both. A shared test_fixtures module is the right home
// long-term; tracked as a follow-up.
fn install_client(state: &mut ServerState, id: u32) -> UnixStream {
    use crate::resources::ROOT_WINDOW;
    use yserver_protocol::x11::ClientByteOrder;
    let (a, b) = UnixStream::pair().unwrap();
    state.clients.insert(
        id,
        ClientState {
            writer: Arc::new(Mutex::new(crate::transport::Transport::Unix(a))),
            byte_order: ClientByteOrder::LittleEndian,
            last_sequence: Arc::new(AtomicU16::new(0)),
            resource_id_base: 0,
            resource_id_mask: u32::MAX,
            event_masks: HashMap::new(),
            save_set: HashSet::new(),
            big_requests_enabled: false,
            xi2_masks: HashMap::new(),
            xi1_event_classes: HashSet::new(),
            xi1_window_event_classes: HashMap::new(),
            outbound: VecDeque::new(),
            watching_writable: false,
            write_failed: false,
            focused_window: ROOT_WINDOW,
            reader_control: None,
            is_local: true,
            fd_passing: true,
        },
    );
    b
}

fn install_capture_client(state: &mut ServerState, id: u32) -> crate::transport::CapturedPeer {
    use crate::resources::ROOT_WINDOW;
    use yserver_protocol::x11::ClientByteOrder;
    let (writer, peer) = crate::transport::Transport::capture_pair();
    state.clients.insert(
        id,
        ClientState {
            writer: Arc::new(Mutex::new(writer)),
            byte_order: ClientByteOrder::LittleEndian,
            last_sequence: Arc::new(AtomicU16::new(0)),
            resource_id_base: 0,
            resource_id_mask: u32::MAX,
            event_masks: HashMap::new(),
            save_set: HashSet::new(),
            big_requests_enabled: false,
            xi2_masks: HashMap::new(),
            xi1_event_classes: HashSet::new(),
            xi1_window_event_classes: HashMap::new(),
            outbound: VecDeque::new(),
            watching_writable: false,
            write_failed: false,
            focused_window: ROOT_WINDOW,
            reader_control: None,
            is_local: true,
            fd_passing: true,
        },
    );
    peer
}

fn read_all_available(peer: &mut UnixStream) -> Vec<u8> {
    peer.set_nonblocking(true).expect("set_nonblocking");
    let mut out = Vec::new();
    let mut buf = [0u8; 512];
    loop {
        match peer.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(err) => panic!("read failed: {err}"),
        }
    }
    peer.set_nonblocking(false).expect("unset_nonblocking");
    out
}

fn read_all_capture_available(peer: &mut crate::transport::CapturedPeer) -> Vec<u8> {
    peer.set_nonblocking(true).expect("set_nonblocking");
    let mut out = Vec::new();
    let mut buf = [0u8; 512];
    loop {
        match peer.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(err) => panic!("read failed: {err}"),
        }
    }
    peer.set_nonblocking(false).expect("unset_nonblocking");
    out
}

fn motion_event() -> HostPointerEvent {
    HostPointerEvent {
        origin: crate::core_loop::message::InputOrigin::XTest(4),
        kind: PointerEventKind::MotionNotify,
        host_xid: 0,
        detail: 0,
        time: 1,
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

fn source_button_event(
    kind: PointerEventKind,
    origin: crate::core_loop::InputOrigin,
    detail: u8,
    time: u32,
) -> HostPointerEvent {
    HostPointerEvent {
        kind,
        detail,
        time,
        origin,
        ..motion_event()
    }
}

fn pointer_source(state: &mut ServerState, source: u64, pointer: bool) -> u16 {
    let info = crate::core_loop::DeviceInfo {
        source_id: crate::xinput::InputSourceId(source),
        enabled: true,
        resume_key: None,
        capabilities: crate::xinput::InputCapabilities {
            keyboard: false,
            pointer,
            touch: false,
        },
        name: format!("pointer source {source}"),
        device_node: format!("/dev/input/event{source}"),
        sysname: format!("event{source}"),
        vendor_id: 1,
        product_id: source as u32,
        is_touchpad: false,
        config: crate::core_loop::message::LibinputConfigSnapshot::default(),
    };
    let facets = state.xi_register_source(&info);
    if pointer {
        facets.first().copied().unwrap_or(0)
    } else {
        0
    }
}

fn xi2_event_ids(bytes: &[u8]) -> Vec<(u16, u16, u16)> {
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

fn xi2_scroll_values(bytes: &[u8]) -> Vec<(u16, u16, i32)> {
    let mut result = Vec::new();
    let mut offset = 0;
    while offset + 32 <= bytes.len() {
        assert_eq!(bytes[offset], 35, "expected GenericEvent");
        let units = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap());
        let evtype = u16::from_le_bytes(bytes[offset + 8..offset + 10].try_into().unwrap());
        let deviceid = u16::from_le_bytes(bytes[offset + 10..offset + 12].try_into().unwrap());
        if evtype == 6 && units == 28 {
            let sourceid = u16::from_le_bytes(bytes[offset + 52..offset + 54].try_into().unwrap());
            let value = i32::from_le_bytes(bytes[offset + 136..offset + 140].try_into().unwrap());
            result.push((deviceid, sourceid, value));
        }
        offset += 32 + units as usize * 4;
    }
    assert_eq!(offset, bytes.len(), "event stream fully consumed");
    result
}

fn xi1_device_event_ids(bytes: &[u8]) -> Vec<(u8, u8)> {
    let mut result = Vec::new();
    let mut offset = 0;
    while offset + 32 <= bytes.len() {
        if bytes[offset] == 35 {
            let units = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap());
            offset += 32 + units as usize * 4;
        } else {
            let event_type = bytes[offset];
            if (crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_BUTTON_PRESS_OFFSET
                ..=crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_BUTTON_RELEASE_OFFSET)
                .contains(&event_type)
            {
                result.push((event_type, bytes[offset + 31] & 0x7f));
            }
            offset += 32;
        }
    }
    assert_eq!(offset, bytes.len(), "event stream fully consumed");
    result
}

fn select_xi2(state: &mut ServerState, client: u32, device: u16, mask: u64) {
    state
        .clients
        .get_mut(&client)
        .unwrap()
        .xi2_masks
        .insert((ROOT_WINDOW, device), mask);
}

fn select_xi1_button(state: &mut ServerState, client: u32, device: u16, offset: u8) {
    state
        .clients
        .get_mut(&client)
        .unwrap()
        .xi1_window_event_classes
        .entry(ROOT_WINDOW)
        .or_default()
        .insert((u32::from(device) << 8) | u32::from(crate::server::XI_FIRST_EVENT + offset));
}

#[test]
fn pointer_source_selection_uses_resolved_xi1_and_xi2_facets() {
    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    let mut xi2_peer = install_capture_client(&mut state, 1);
    let mut xi1_peer = install_capture_client(&mut state, 2);
    let razer = pointer_source(&mut state, 301, true);
    let hyperx = pointer_source(&mut state, 302, true);
    assert_eq!((razer, hyperx), (6, 7));
    select_xi2(&mut state, 1, razer, 1 << 4);
    select_xi1_button(
        &mut state,
        2,
        hyperx,
        crate::xinput::XI_DEVICE_BUTTON_PRESS_OFFSET,
    );

    let dropped = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &HostXidMap::new(),
        source_button_event(
            PointerEventKind::ButtonPress,
            crate::core_loop::InputOrigin::Physical(crate::xinput::InputSourceId(301)),
            1,
            1,
        ),
        true,
        false,
    );
    assert!(dropped.is_empty());
    assert_eq!(
        xi2_event_ids(&read_all_capture_available(&mut xi2_peer)),
        vec![(4, razer, razer)]
    );
    assert!(read_all_capture_available(&mut xi1_peer).is_empty());

    let dropped = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &HostXidMap::new(),
        source_button_event(
            PointerEventKind::ButtonPress,
            crate::core_loop::InputOrigin::Physical(crate::xinput::InputSourceId(302)),
            1,
            2,
        ),
        true,
        false,
    );
    assert!(dropped.is_empty());
    assert!(read_all_capture_available(&mut xi2_peer).is_empty());
    assert_eq!(
        xi1_device_event_ids(&read_all_capture_available(&mut xi1_peer)),
        vec![(
            crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_BUTTON_PRESS_OFFSET,
            hyperx as u8,
        )]
    );

    assert_eq!(state.buttons_down, 1);
    assert_eq!(state.xi_devices.device(razer).unwrap().buttons_down, 1);
    assert_eq!(state.xi_devices.device(hyperx).unwrap().buttons_down, 1);
    assert_eq!(state.sync_pending.len(), 0);
    assert_eq!(state.xi_devices.devices().len(), 6);
    assert_eq!(state.unpublished_pointer_buttons_down.len(), 0);
    assert_eq!(state.scroll_axis_value, [0, 0]);
    assert_eq!(
        state
            .xi_devices
            .devices()
            .iter()
            .map(|device| (device.id, device.buttons_down, device.scroll_axis_values))
            .collect::<Vec<_>>(),
        vec![
            (2, 0, [0, 0]),
            (3, 0, [0, 0]),
            (4, 0, [0, 0]),
            (5, 0, [0, 0]),
            (razer, 1, [0, 0]),
            (hyperx, 1, [0, 0]),
        ],
        "unrelated devices and facets keep their own state"
    );
}

#[test]
fn pointer_source_selection_aggregates_holds_and_preserves_slave_release_cookies() {
    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    let mut peer = install_capture_client(&mut state, 1);
    let razer = pointer_source(&mut state, 311, true);
    let hyperx = pointer_source(&mut state, 312, true);
    let device_mask = (1 << 4) | (1 << 5);
    for device in [2, 4, razer, hyperx] {
        select_xi2(&mut state, 1, device, device_mask);
    }

    macro_rules! send {
        ($event:expr) => {{
            let dropped = pointer_event_fanout_to_state(
                &mut state,
                &mut backend,
                &HostXidMap::new(),
                $event,
                true,
                false,
            );
            assert!(dropped.is_empty());
            xi2_event_ids(&read_all_capture_available(&mut peer))
        }};
    }

    assert_eq!(
        send!(source_button_event(
            PointerEventKind::ButtonPress,
            crate::core_loop::InputOrigin::Physical(crate::xinput::InputSourceId(311)),
            1,
            1,
        )),
        vec![(4, razer, razer), (4, 2, razer)]
    );
    assert!(
        send!(source_button_event(
            PointerEventKind::ButtonPress,
            crate::core_loop::InputOrigin::Physical(crate::xinput::InputSourceId(311)),
            1,
            11,
        ))
        .is_empty(),
        "a duplicate press from one slave is ignored"
    );
    assert_eq!(
        send!(source_button_event(
            PointerEventKind::ButtonPress,
            crate::core_loop::InputOrigin::Physical(crate::xinput::InputSourceId(312)),
            1,
            2,
        )),
        vec![(4, hyperx, hyperx)],
        "a second attached slave keeps its event, but the master press is duplicate"
    );
    assert_eq!(state.buttons_down, 1);
    assert_eq!(state.xi_devices.device(razer).unwrap().buttons_down, 1);
    assert_eq!(state.xi_devices.device(hyperx).unwrap().buttons_down, 1);

    // A virtual XTEST click takes its own slave hold during the physical
    // drag. It cannot create another master press, and it keeps the
    // master down when the first physical source releases.
    assert_eq!(
        send!(source_button_event(
            PointerEventKind::ButtonPress,
            crate::core_loop::InputOrigin::XTest(4),
            1,
            3,
        )),
        vec![(4, 4, 4)]
    );
    assert_eq!(
        send!(source_button_event(
            PointerEventKind::ButtonRelease,
            crate::core_loop::InputOrigin::Physical(crate::xinput::InputSourceId(311)),
            1,
            4,
        )),
        vec![(5, razer, razer)],
        "the per-slave release cookie survives while master release is suppressed"
    );
    assert!(
        send!(source_button_event(
            PointerEventKind::ButtonRelease,
            crate::core_loop::InputOrigin::Physical(crate::xinput::InputSourceId(311)),
            1,
            41,
        ))
        .is_empty(),
        "a duplicate release from one slave is ignored"
    );
    assert_eq!(state.buttons_down, 1);
    assert_eq!(state.xi_devices.device(razer).unwrap().buttons_down, 0);
    assert_eq!(state.xi_devices.device(hyperx).unwrap().buttons_down, 1);
    assert_eq!(state.xi_devices.device(4).unwrap().buttons_down, 1);

    // Removing Razer drops only its source state; HyperX and XTEST remain
    // attached holders, so unplugging one mouse cannot release the drag.
    assert_eq!(
        state.xi_unregister_source(crate::xinput::InputSourceId(311)),
        vec![razer]
    );
    assert!(state.xi_devices.device(razer).is_none());
    assert_eq!(state.xi_devices.device(hyperx).unwrap().buttons_down, 1);
    assert_eq!(state.xi_devices.device(4).unwrap().buttons_down, 1);
    assert_eq!(state.buttons_down, 1);

    assert_eq!(
        send!(source_button_event(
            PointerEventKind::ButtonRelease,
            crate::core_loop::InputOrigin::Physical(crate::xinput::InputSourceId(312)),
            1,
            5,
        )),
        vec![(5, hyperx, hyperx)]
    );
    assert_eq!(state.buttons_down, 1);
    assert_eq!(
        send!(source_button_event(
            PointerEventKind::ButtonRelease,
            crate::core_loop::InputOrigin::XTest(4),
            1,
            6,
        )),
        vec![(5, 4, 4), (5, 2, 4)],
        "the final attached holder releases the master button"
    );
    assert_eq!(state.buttons_down, 0);
    assert_eq!(state.xi_devices.device(hyperx).unwrap().buttons_down, 0);
    assert_eq!(state.xi_devices.device(4).unwrap().buttons_down, 0);
    assert_eq!(state.sync_pending.len(), 0);
    assert_eq!(state.xi_devices.devices().len(), 5);
    assert!(
        state
            .xi_devices
            .source(crate::xinput::InputSourceId(311))
            .is_none()
    );
    assert!(
        state
            .xi_devices
            .facet(
                crate::xinput::InputSourceId(311),
                crate::xinput::XiFacetKind::PointerTouch
            )
            .is_none()
    );
    assert!(
        state
            .xi_devices
            .source(crate::xinput::InputSourceId(312))
            .is_some()
    );
    assert!(state.unpublished_pointer_buttons_down.is_empty());
    assert_eq!(state.scroll_axis_value, [0, 0]);
    assert_eq!(
        state
            .xi_devices
            .devices()
            .iter()
            .map(|device| (device.id, device.buttons_down, device.scroll_axis_values))
            .collect::<Vec<_>>(),
        vec![
            (2, 0, [0, 0]),
            (3, 0, [0, 0]),
            (4, 0, [0, 0]),
            (5, 0, [0, 0]),
            (hyperx, 0, [0, 0]),
        ],
        "the removed source leaves no state and unrelated devices stay clear"
    );
}

#[test]
fn pointer_source_selection_keeps_scroll_values_and_stops_per_source() {
    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    let mut peer = install_capture_client(&mut state, 1);
    let razer = pointer_source(&mut state, 321, true);
    let hyperx = pointer_source(&mut state, 322, true);
    select_xi2(&mut state, 1, razer, 1 << 6);
    select_xi2(&mut state, 1, hyperx, 1 << 6);

    for (source, count) in [(321, 2), (322, 1)] {
        for n in 0..count {
            let dropped = pointer_event_fanout_to_state(
                &mut state,
                &mut backend,
                &HostXidMap::new(),
                source_button_event(
                    PointerEventKind::ButtonPress,
                    crate::core_loop::InputOrigin::Physical(crate::xinput::InputSourceId(source)),
                    5,
                    source as u32 + n as u32,
                ),
                true,
                false,
            );
            assert!(dropped.is_empty());
            let _ = read_all_capture_available(&mut peer);
            let dropped = pointer_event_fanout_to_state(
                &mut state,
                &mut backend,
                &HostXidMap::new(),
                source_button_event(
                    PointerEventKind::ButtonRelease,
                    crate::core_loop::InputOrigin::Physical(crate::xinput::InputSourceId(source)),
                    5,
                    source as u32 + n as u32,
                ),
                true,
                false,
            );
            assert!(dropped.is_empty());
            let _ = read_all_capture_available(&mut peer);
        }
    }
    assert_eq!(
        state.xi_devices.device(razer).unwrap().scroll_axis_values,
        [2, 0]
    );
    assert_eq!(
        state.xi_devices.device(hyperx).unwrap().scroll_axis_values,
        [1, 0]
    );
    assert_eq!(state.scroll_axis_value, [1, 0]);

    for (source, device, expected) in [(321, razer, 2), (322, hyperx, 1)] {
        crate::core_loop::pointer_fanout::emit_scroll_stop_to_state(
            &mut state,
            &HostXidMap::new(),
            crate::core_loop::InputOrigin::Physical(crate::xinput::InputSourceId(source)),
            0,
            10,
            20,
            0,
            source as u32,
        );
        assert_eq!(
            xi2_scroll_values(&read_all_capture_available(&mut peer)),
            vec![(device, device, expected), (device, device, 0)],
            "scroll stop uses the selected source's cumulative valuator"
        );
        assert_eq!(
            state.xi_devices.device(device).unwrap().scroll_axis_values[0],
            expected
        );
    }
    assert_eq!(state.xi_devices.device(razer).unwrap().buttons_down, 0);
    assert_eq!(state.xi_devices.device(hyperx).unwrap().buttons_down, 0);
    assert_eq!(state.buttons_down, 0);
    assert_eq!(state.sync_pending.len(), 0);
    assert_eq!(state.xi_devices.devices().len(), 6);
    assert_eq!(state.scroll_axis_value, [1, 0]);
    assert!(state.unpublished_pointer_buttons_down.is_empty());
    assert_eq!(
        state
            .xi_devices
            .devices()
            .iter()
            .map(|device| (device.id, device.buttons_down, device.scroll_axis_values))
            .collect::<Vec<_>>(),
        vec![
            (2, 0, [0, 0]),
            (3, 0, [0, 0]),
            (4, 0, [0, 0]),
            (5, 0, [0, 0]),
            (razer, 0, [2, 0]),
            (hyperx, 0, [1, 0]),
        ],
        "each source retains its own valuator state"
    );
}

#[test]
fn xi_slave_switch_master_scroll_value_uses_the_current_source_baseline() {
    use crate::core_loop::InputOrigin;

    const RAZER: u64 = 0xA61;
    const HYPERX: u64 = 0xA62;
    const XI_BUTTON_PRESS: u32 = 1 << 4;
    const XI_MOTION: u32 = 1 << 6;
    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    let mut peer = install_capture_client(&mut state, 92);
    let razer = pointer_source(&mut state, RAZER, true);
    let hyperx = pointer_source(&mut state, HYPERX, true);
    let mut selection = Vec::new();
    selection.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    selection.extend_from_slice(&1u16.to_le_bytes());
    selection.extend_from_slice(&[0; 2]);
    selection.extend_from_slice(&2u16.to_le_bytes()); // master pointer
    selection.extend_from_slice(&1u16.to_le_bytes()); // one mask word
    selection.extend_from_slice(
        &(crate::xinput::XI2_DEVICE_CHANGED_MASK | XI_BUTTON_PRESS | XI_MOTION).to_le_bytes(),
    );
    crate::core_loop::process_request::process_request(
        &mut state,
        &mut backend,
        yserver_protocol::x11::ClientId(92),
        yserver_protocol::x11::SequenceNumber(1),
        yserver_protocol::x11::RequestHeader {
            opcode: 137,
            data: 46, // XISelectEvents
            length_units: 5,
        },
        &selection,
        None,
    )
    .expect("select master DeviceChanged, ButtonPress, and Motion events");

    for time in 1..=10 {
        for kind in [
            PointerEventKind::ButtonPress,
            PointerEventKind::ButtonRelease,
        ] {
            let dropped = pointer_event_fanout_to_state(
                &mut state,
                &mut backend,
                &HostXidMap::new(),
                source_button_event(
                    kind,
                    InputOrigin::Physical(crate::xinput::InputSourceId(RAZER)),
                    5,
                    time,
                ),
                true,
                false,
            );
            assert!(dropped.is_empty());
        }
        let _ = read_all_capture_available(&mut peer);
    }

    assert_eq!(state.scroll_axis_value, [10, 0]);
    assert_eq!(
        state.xi_devices.device(razer).unwrap().scroll_axis_values,
        [10, 0]
    );
    assert_eq!(
        state.xi_devices.device(hyperx).unwrap().scroll_axis_values,
        [0, 0]
    );

    let dropped = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &HostXidMap::new(),
        source_button_event(
            PointerEventKind::ButtonPress,
            InputOrigin::Physical(crate::xinput::InputSourceId(HYPERX)),
            5,
            11,
        ),
        true,
        false,
    );
    assert!(dropped.is_empty());
    let bytes = read_all_capture_available(&mut peer);

    let mut switch_baseline = None;
    let mut offset = 0;
    while offset < bytes.len() {
        assert_eq!(bytes[offset], 35, "XI2 GenericEvent");
        let units = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
        let evtype = u16::from_le_bytes([bytes[offset + 8], bytes[offset + 9]]);
        if evtype == 1 {
            let num_classes = u16::from_le_bytes([bytes[offset + 16], bytes[offset + 17]]) as usize;
            let mut class_offset = offset + 32;
            for _ in 0..num_classes {
                let class_type = u16::from_le_bytes([bytes[class_offset], bytes[class_offset + 1]]);
                let class_units =
                    u16::from_le_bytes([bytes[class_offset + 2], bytes[class_offset + 3]]) as usize;
                let source_id =
                    u16::from_le_bytes([bytes[class_offset + 4], bytes[class_offset + 5]]);
                let axis = u16::from_le_bytes([bytes[class_offset + 6], bytes[class_offset + 7]]);
                if class_type == 2 && source_id == hyperx && axis == 2 {
                    switch_baseline = Some(i32::from_le_bytes(
                        bytes[class_offset + 28..class_offset + 32]
                            .try_into()
                            .unwrap(),
                    ));
                }
                class_offset += class_units * 4;
            }
            assert_eq!(class_offset, offset + 32 + units * 4);
        }
        offset += 32 + units * 4;
    }
    assert_eq!(switch_baseline, Some(0));
    assert_eq!(
        xi2_scroll_values(&bytes),
        vec![(2, hyperx, 1)],
        "the master event advances from the switching slave's baseline"
    );
    let dropped = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &HostXidMap::new(),
        source_button_event(
            PointerEventKind::ButtonRelease,
            InputOrigin::Physical(crate::xinput::InputSourceId(HYPERX)),
            5,
            12,
        ),
        true,
        false,
    );
    assert!(dropped.is_empty());
    let _ = read_all_capture_available(&mut peer);
    assert_eq!(state.scroll_axis_value, [1, 0]);
    assert_eq!(
        state.xi_devices.device(razer).unwrap().scroll_axis_values,
        [10, 0]
    );
    assert_eq!(
        state.xi_devices.device(hyperx).unwrap().scroll_axis_values,
        [1, 0]
    );
    assert_eq!(state.buttons_down, 0);
    assert!(state.sync_pending.is_empty());
    assert!(state.unpublished_pointer_buttons_down.is_empty());
}

#[test]
fn xi_scroll_stop_switches_back_from_xtest_and_keeps_master_query_baseline() {
    use crate::core_loop::InputOrigin;
    use yserver_protocol::x11::{RequestHeader, SequenceNumber};

    const CLIENT: u32 = 0xA740;
    const TOUCHPAD_SOURCE: u64 = 0xA7401;
    const XI_DEVICE_CHANGED: u32 = 1 << 1;
    const XI_MOTION: u32 = 1 << 6;

    fn request(
        state: &mut ServerState,
        backend: &mut RecordingBackend,
        sequence: u16,
        minor: u8,
        body: &[u8],
    ) {
        crate::core_loop::process_request::process_request(
            state,
            backend,
            ClientId(CLIENT),
            SequenceNumber(sequence),
            RequestHeader {
                opcode: 137,
                data: minor,
                length_units: u32::try_from((body.len() + 4) / 4).unwrap(),
            },
            body,
            None,
        )
        .expect("production XI2 request dispatch");
    }

    fn query_master_scroll(bytes: &[u8]) -> i32 {
        assert_eq!(bytes[0], 1, "XIQueryDevice reply");
        let devices = u16::from_le_bytes([bytes[8], bytes[9]]) as usize;
        let mut offset = 32;
        for _ in 0..devices {
            let device = u16::from_le_bytes([bytes[offset], bytes[offset + 1]]);
            let classes = u16::from_le_bytes([bytes[offset + 6], bytes[offset + 7]]) as usize;
            let name_len = u16::from_le_bytes([bytes[offset + 8], bytes[offset + 9]]) as usize;
            let mut class_offset = offset + 12 + name_len;
            while !class_offset.is_multiple_of(4) {
                class_offset += 1;
            }
            for _ in 0..classes {
                let class = u16::from_le_bytes([bytes[class_offset], bytes[class_offset + 1]]);
                let units =
                    u16::from_le_bytes([bytes[class_offset + 2], bytes[class_offset + 3]]) as usize;
                let axis = u16::from_le_bytes([bytes[class_offset + 6], bytes[class_offset + 7]]);
                if device == 2 && class == 2 && axis == 2 {
                    return i32::from_le_bytes(
                        bytes[class_offset + 28..class_offset + 32]
                            .try_into()
                            .unwrap(),
                    );
                }
                class_offset += units * 4;
            }
            offset = class_offset;
        }
        panic!("master vertical scroll valuator missing from query reply");
    }

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    let mut peer = install_capture_client(&mut state, CLIENT);
    let touchpad = pointer_source(&mut state, TOUCHPAD_SOURCE, true);

    request(&mut state, &mut backend, 1, 47, &[2, 0, 4, 0]);
    let _ = read_all_capture_available(&mut peer);
    let mut select = Vec::new();
    select.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    select.extend_from_slice(&1u16.to_le_bytes());
    select.extend_from_slice(&[0; 2]);
    select.extend_from_slice(&2u16.to_le_bytes());
    select.extend_from_slice(&1u16.to_le_bytes());
    select.extend_from_slice(&(XI_DEVICE_CHANGED | XI_MOTION).to_le_bytes());
    request(&mut state, &mut backend, 2, 46, &select);
    let _ = read_all_capture_available(&mut peer);

    // Five source events establish the touchpad's valuator before a
    // virtual XTEST motion switches the master back to device 4.
    // Xorg runs UpdateFromMaster on both pointer events and stops at
    // dix/getevents.c:687-708, so the following stop must switch back,
    // copy this source's valuator, and report the same query baseline.
    for time in 10..15 {
        for kind in [
            PointerEventKind::ButtonPress,
            PointerEventKind::ButtonRelease,
        ] {
            let dropped = pointer_event_fanout_to_state(
                &mut state,
                &mut backend,
                &HostXidMap::new(),
                source_button_event(
                    kind,
                    InputOrigin::Physical(crate::xinput::InputSourceId(TOUCHPAD_SOURCE)),
                    5,
                    time,
                ),
                true,
                false,
            );
            assert!(dropped.is_empty());
        }
    }
    assert_eq!(
        state
            .xi_devices
            .device(touchpad)
            .unwrap()
            .scroll_axis_values,
        [5, 0]
    );
    let _ = read_all_capture_available(&mut peer);

    let dropped = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &HostXidMap::new(),
        HostPointerEvent {
            origin: InputOrigin::XTest(4),
            kind: PointerEventKind::MotionNotify,
            ..motion_event()
        },
        true,
        false,
    );
    assert!(dropped.is_empty());
    assert_eq!(state.xi_last_slave(2), Some(4));
    let _ = read_all_capture_available(&mut peer);

    crate::core_loop::pointer_fanout::emit_scroll_stop_to_state(
        &mut state,
        &HostXidMap::new(),
        InputOrigin::Physical(crate::xinput::InputSourceId(TOUCHPAD_SOURCE)),
        0,
        10,
        20,
        0,
        20,
    );
    let stop_bytes = read_all_capture_available(&mut peer);
    let events = xi2_scroll_values(&stop_bytes);
    let mut switch_source = None;
    let mut offset = 0;
    while offset < stop_bytes.len() {
        assert_eq!(stop_bytes[offset], 35, "XI2 GenericEvent");
        let units =
            u32::from_le_bytes(stop_bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
        if u16::from_le_bytes([stop_bytes[offset + 8], stop_bytes[offset + 9]]) == 1 {
            switch_source = Some(u16::from_le_bytes([
                stop_bytes[offset + 18],
                stop_bytes[offset + 19],
            ]));
        }
        offset += 32 + units * 4;
    }
    assert_eq!(
        switch_source,
        Some(touchpad),
        "PointerScrollStop emits the SlaveSwitch DeviceChanged before stop motion"
    );
    assert_eq!(state.xi_last_slave(2), Some(touchpad));
    assert_eq!(events, vec![(2, touchpad, 5), (2, touchpad, 0)]);
    assert_eq!(state.scroll_axis_value, [5, 0]);

    request(&mut state, &mut backend, 3, 48, &[2, 0, 0, 0]);
    assert_eq!(
        query_master_scroll(&read_all_capture_available(&mut peer)),
        5
    );
    assert_eq!(
        state
            .xi_devices
            .device(touchpad)
            .unwrap()
            .scroll_axis_values,
        [5, 0]
    );
    assert_eq!(
        state.xi_devices.device(touchpad).unwrap().attached_master,
        Some(2)
    );
    assert!(state.xi2_pointer_grabs.is_empty());
    assert!(state.active_pointer_grab.is_none());
    assert!(state.sync_pending.is_empty());
    assert_eq!(state.buttons_down, 0);
    assert_eq!(state.xi_devices.device(touchpad).unwrap().buttons_down, 0);
}

#[test]
fn physical_scroll_motion_survives_xtest_hold_of_the_emulated_button() {
    use crate::core_loop::InputOrigin;
    use yserver_protocol::x11::{RequestHeader, SequenceNumber};

    const CLIENT: u32 = 0xA741;
    const SOURCE: u64 = 0xA7411;
    const XI_MOTION: u32 = 1 << 6;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    let mut peer = install_capture_client(&mut state, CLIENT);
    let pointer = pointer_source(&mut state, SOURCE, true);
    let dispatch =
        |state: &mut ServerState, backend: &mut RecordingBackend, sequence, minor, body: &[u8]| {
            crate::core_loop::process_request::process_request(
                state,
                backend,
                ClientId(CLIENT),
                SequenceNumber(sequence),
                RequestHeader {
                    opcode: 137,
                    data: minor,
                    length_units: u32::try_from((body.len() + 4) / 4).unwrap(),
                },
                body,
                None,
            )
            .expect("production XI2 request dispatch")
        };
    dispatch(&mut state, &mut backend, 1, 47, &[2, 0, 4, 0]);
    let _ = read_all_capture_available(&mut peer);
    let mut select = Vec::new();
    select.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    select.extend_from_slice(&1u16.to_le_bytes());
    select.extend_from_slice(&[0; 2]);
    select.extend_from_slice(&2u16.to_le_bytes());
    select.extend_from_slice(&1u16.to_le_bytes());
    select.extend_from_slice(&XI_MOTION.to_le_bytes());
    dispatch(&mut state, &mut backend, 2, 46, &select);
    let _ = read_all_capture_available(&mut peer);

    let press = |origin| source_button_event(PointerEventKind::ButtonPress, origin, 5, 30);
    let release = |origin| source_button_event(PointerEventKind::ButtonRelease, origin, 5, 31);
    {
        let event = press(InputOrigin::XTest(4));
        let dropped = pointer_event_fanout_to_state(
            &mut state,
            &mut backend,
            &HostXidMap::new(),
            event,
            true,
            false,
        );
        assert!(dropped.is_empty());
    }
    let xtest_events = read_all_capture_available(&mut peer);
    assert!(
        xi2_event_ids(&xtest_events)
            .iter()
            .all(|(event_type, ..)| *event_type == 6),
        "Motion selection receives smooth scrolling but no emulated ButtonPress"
    );
    assert_eq!(
        state
            .xi_devices
            .device(crate::xinput::DEVICEID_XTEST_POINTER)
            .unwrap()
            .buttons_down,
        1 << 4,
        "XTEST button transition holds the master even without XI_ButtonPress selected"
    );

    let dropped = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &HostXidMap::new(),
        press(InputOrigin::Physical(crate::xinput::InputSourceId(SOURCE))),
        true,
        false,
    );
    assert!(dropped.is_empty());
    let physical_scroll = read_all_capture_available(&mut peer);

    let dropped = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &HostXidMap::new(),
        release(InputOrigin::Physical(crate::xinput::InputSourceId(SOURCE))),
        true,
        false,
    );
    assert!(dropped.is_empty());
    let release_events = xi2_event_ids(&read_all_capture_available(&mut peer));

    // Xorg makes the scroll valuator Motion before generating legacy
    // wheel-button emulation (dix/getevents.c:1646-1697, 1703-1718). The separate
    // button transition is rejected by master aggregation while XTEST
    // holds button 5, but the physical scroll Motion still reaches the
    // attached master.
    assert_eq!(xi2_scroll_values(&physical_scroll), vec![(2, pointer, 1)]);
    assert!(
        !xi2_event_ids(&physical_scroll)
            .iter()
            .any(|(evtype, device, source)| *evtype == 4 && *device == 2 && *source == pointer),
        "suppressed master ButtonPress stays suppressed"
    );
    assert!(
        release_events.is_empty(),
        "the physical wheel release is aggregated away"
    );
    assert_eq!(
        state.xi_devices.device(pointer).unwrap().scroll_axis_values,
        [1, 0]
    );
    assert_eq!(state.scroll_axis_value, [1, 0]);
    assert_eq!(state.xi_devices.device(pointer).unwrap().buttons_down, 0);
    assert_eq!(
        state.xi_devices.device(pointer).unwrap().attached_master,
        Some(2)
    );
    assert_eq!(
        state
            .xi_devices
            .device(crate::xinput::DEVICEID_XTEST_POINTER)
            .unwrap()
            .buttons_down,
        1 << 4,
        "the XTEST source still holds logical button 5",
    );
    assert_eq!(
        state.buttons_down,
        1 << 4,
        "master button remains held by XTEST"
    );
    assert!(state.xi2_pointer_grabs.is_empty());
    assert!(state.active_pointer_grab.is_none());
    assert!(state.sync_pending.is_empty());
    assert!(state.unpublished_pointer_buttons_down.is_empty());
}

#[test]
fn record_does_not_admit_frozen_input_from_a_floating_pointer() {
    use crate::{core_loop::InputOrigin, host_x11::HostPointerEvent, server::QueuedInputEvent};
    use yserver_protocol::x11::{RequestHeader, SequenceNumber};

    const RECORDER: u32 = 0xA743;
    const GRABBER: u32 = 0xA744;
    const SOURCE: u64 = 0xA7431;
    const CONTEXT: u32 = 1;

    fn request(
        state: &mut ServerState,
        backend: &mut RecordingBackend,
        client: u32,
        sequence: u16,
        opcode: u8,
        minor: u8,
        body: &[u8],
    ) {
        crate::core_loop::process_request::process_request(
            state,
            backend,
            ClientId(client),
            SequenceNumber(sequence),
            RequestHeader {
                opcode,
                data: minor,
                length_units: u32::try_from((body.len() + 4) / 4).unwrap(),
            },
            body,
            None,
        )
        .expect("production request dispatch");
    }

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    let mut recorder = install_capture_client(&mut state, RECORDER);
    let mut grabber = install_capture_client(&mut state, GRABBER);
    let pointer = pointer_source(&mut state, SOURCE, true);

    // CreateContext/EnableContext select core ButtonPress..ButtonRelease
    // through the RECORD dispatcher, like Xorg's Record callback test.
    let mut create_context = Vec::new();
    create_context.extend_from_slice(&CONTEXT.to_le_bytes());
    create_context.extend_from_slice(&[0; 4]); // element header + pad
    create_context.extend_from_slice(&1u32.to_le_bytes()); // one client spec
    create_context.extend_from_slice(&1u32.to_le_bytes()); // one range
    create_context.extend_from_slice(&2u32.to_le_bytes()); // FutureClients
    let mut range = [0u8; 24];
    range[18] = 2; // KeyPress / ButtonPress
    range[19] = 5; // KeyRelease / ButtonRelease
    create_context.extend_from_slice(&range);
    request(
        &mut state,
        &mut backend,
        RECORDER,
        1,
        154,
        1,
        &create_context,
    );
    request(
        &mut state,
        &mut backend,
        RECORDER,
        2,
        154,
        5,
        &CONTEXT.to_le_bytes(),
    );
    let _ = read_all_capture_available(&mut recorder);

    request(&mut state, &mut backend, GRABBER, 1, 137, 47, &[2, 0, 4, 0]);
    let _ = read_all_capture_available(&mut grabber);
    let mut grab = Vec::new();
    grab.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    grab.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    grab.extend_from_slice(&0u32.to_le_bytes()); // no cursor
    grab.extend_from_slice(&pointer.to_le_bytes());
    grab.extend_from_slice(&[0, 1, 0, 0]); // synchronous device, async paired
    grab.extend_from_slice(&0u16.to_le_bytes()); // no event-mask words
    grab.extend_from_slice(&[0; 2]);
    request(&mut state, &mut backend, GRABBER, 2, 137, 51, &grab);
    let _ = read_all_capture_available(&mut grabber);
    assert_eq!(
        state.xi_devices.device(pointer).unwrap().attached_master,
        None
    );
    assert!(state.xi2_pointer_grabs.contains_key(&pointer));
    assert!(state.xi1_frozen[&pointer].frozen());

    let dropped = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &HostXidMap::new(),
        HostPointerEvent {
            origin: InputOrigin::Physical(crate::xinput::InputSourceId(SOURCE)),
            kind: PointerEventKind::ButtonPress,
            detail: 1,
            time: 40,
            ..motion_event()
        },
        true,
        false,
    );
    assert!(dropped.is_empty());
    let record_wire = read_all_capture_available(&mut recorder);
    let mut core_events = Vec::new();
    let mut offset = 0;
    while offset + 32 <= record_wire.len() {
        assert_eq!(record_wire[offset], 1, "RECORD reply stream");
        let words =
            u32::from_le_bytes(record_wire[offset + 4..offset + 8].try_into().unwrap()) as usize;
        let end = offset + 32 + words * 4;
        assert!(end <= record_wire.len(), "complete RECORD reply");
        if record_wire[offset + 1] == 0 && words > 0 {
            core_events.push(record_wire[offset + 32]);
        }
        offset = end;
    }

    // Xorg calls DeviceEventCallback on frozen queue admission, but
    // record/record.c:784 converts to core only for IsMaster. A floating
    // source has no attached master copy (mi/mieq.c:397), so it must not
    // record a core ButtonPress while its own sync grab queues it.
    assert!(
        core_events.is_empty(),
        "floating frozen input is not core RECORD data"
    );
    assert_eq!(state.sync_pending.len(), 1);
    assert_eq!(state.sync_pending[0].device, pointer);
    assert!(matches!(
        state.sync_pending[0].event,
        QueuedInputEvent::HostPointer(HostPointerEvent {
            kind: PointerEventKind::ButtonPress,
            detail: 1,
            ..
        })
    ));
    assert_eq!(state.xi_devices.device(pointer).unwrap().buttons_down, 1);
    assert_eq!(
        state.buttons_down, 0,
        "floating source has no master button hold"
    );
    assert_eq!(
        state.xi_devices.device(pointer).unwrap().attached_master,
        None
    );
    assert!(state.xi1_frozen[&pointer].frozen());
    assert!(state.xi2_pointer_grabs.contains_key(&pointer));
}

#[test]
fn xi_owner_events_fallback_preserves_other_recipients_root_geometry() {
    use crate::{core_loop::InputOrigin, resources::ROOT_VISUAL};
    use yserver_protocol::x11::{RequestHeader, SequenceNumber};

    const GRABBER: u32 = 0xA745;
    const SELECTOR: u32 = 0xA746;
    const SOURCE: u64 = 0xA7451;
    const HOST_XID: u32 = 0xCAFE_A745;
    const CHILD: u32 = 0x0010_A745;
    const XI_MOTION: u32 = 1 << 6;

    fn request(
        state: &mut ServerState,
        backend: &mut RecordingBackend,
        client: u32,
        sequence: u16,
        minor: u8,
        body: &[u8],
    ) {
        crate::core_loop::process_request::process_request(
            state,
            backend,
            ClientId(client),
            SequenceNumber(sequence),
            RequestHeader {
                opcode: 137,
                data: minor,
                length_units: u32::try_from((body.len() + 4) / 4).unwrap(),
            },
            body,
            None,
        )
        .expect("production XI2 request dispatch");
    }

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    let mut grabber = install_capture_client(&mut state, GRABBER);
    let mut selector = install_capture_client(&mut state, SELECTOR);
    let pointer = pointer_source(&mut state, SOURCE, true);

    state.resources.create_window(
        ClientId(SELECTOR),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(CHILD),
            parent: ROOT_WINDOW,
            x: 20,
            y: 30,
            width: 100,
            height: 100,
            border_width: 0,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(CHILD));
    let mut xid_map = HostXidMap::new();
    xid_map.insert(HOST_XID, ResourceId(CHILD));

    for client in [GRABBER, SELECTOR] {
        request(&mut state, &mut backend, client, 1, 47, &[2, 0, 4, 0]);
    }
    let _ = read_all_capture_available(&mut grabber);
    let _ = read_all_capture_available(&mut selector);

    let mut select = Vec::new();
    select.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    select.extend_from_slice(&1u16.to_le_bytes());
    select.extend_from_slice(&[0; 2]);
    select.extend_from_slice(&2u16.to_le_bytes());
    select.extend_from_slice(&1u16.to_le_bytes());
    select.extend_from_slice(&XI_MOTION.to_le_bytes());
    request(&mut state, &mut backend, SELECTOR, 2, 46, &select);
    let _ = read_all_capture_available(&mut selector);

    let mut grab = Vec::new();
    grab.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    grab.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    grab.extend_from_slice(&0u32.to_le_bytes()); // no cursor
    grab.extend_from_slice(&2u16.to_le_bytes()); // master pointer
    grab.extend_from_slice(&[1, 1, 1, 0]); // async, async paired, owner_events=true
    grab.extend_from_slice(&1u16.to_le_bytes());
    grab.extend_from_slice(&XI_MOTION.to_le_bytes());
    request(&mut state, &mut backend, GRABBER, 2, 51, &grab);
    let _ = read_all_capture_available(&mut grabber);
    assert!(
        state
            .active_pointer_grab
            .is_some_and(|active| { active.owner == ClientId(GRABBER) && active.owner_events })
    );

    let dropped = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &xid_map,
        HostPointerEvent {
            origin: InputOrigin::Physical(crate::xinput::InputSourceId(SOURCE)),
            kind: PointerEventKind::MotionNotify,
            host_xid: HOST_XID,
            root_x: 45,
            root_y: 65,
            event_x: 25,
            event_y: 35,
            time: 50,
            ..motion_event()
        },
        true,
        false,
    );
    assert!(dropped.is_empty());

    let selector_bytes = read_all_capture_available(&mut selector);
    let grabber_bytes = read_all_capture_available(&mut grabber);
    assert_eq!(selector_bytes[0], 35, "selector receives XI_Motion");
    assert_eq!(
        u16::from_le_bytes([selector_bytes[8], selector_bytes[9]]),
        6
    );
    assert_eq!(
        u32::from_le_bytes(selector_bytes[24..28].try_into().unwrap()),
        ROOT_WINDOW.0,
        "other recipients keep the root event window under owner-events fallback"
    );
    assert_eq!(
        u32::from_le_bytes(selector_bytes[28..32].try_into().unwrap()),
        CHILD,
        "the selected root event retains its child path"
    );
    assert_eq!(
        i16::from_le_bytes(selector_bytes[42..44].try_into().unwrap()),
        45,
        "root selection receives root-local event_x"
    );
    assert_eq!(
        i16::from_le_bytes(selector_bytes[46..48].try_into().unwrap()),
        65,
        "root selection receives root-local event_y"
    );
    assert_eq!(
        grabber_bytes[0], 35,
        "fallback grab owner receives XI_Motion"
    );
    assert_eq!(
        u32::from_le_bytes(grabber_bytes[24..28].try_into().unwrap()),
        ROOT_WINDOW.0,
        "grab owner gets its grab window"
    );
    assert_eq!(
        u32::from_le_bytes(grabber_bytes[28..32].try_into().unwrap()),
        0,
        "grab-owner fallback has no child"
    );
    assert_eq!(
        i16::from_le_bytes(grabber_bytes[42..44].try_into().unwrap()),
        45
    );
    assert_eq!(
        i16::from_le_bytes(grabber_bytes[46..48].try_into().unwrap()),
        65
    );
    assert_eq!(state.pointer_root, (45, 65));
    assert_eq!(
        state.xi_devices.device(pointer).unwrap().attached_master,
        Some(2)
    );
    assert!(state.sync_pending.is_empty());
    assert_eq!(state.buttons_down, 0);
    assert_eq!(state.xi_devices.device(pointer).unwrap().buttons_down, 0);
    assert_eq!(state.active_pointer_grab.unwrap().owner, ClientId(GRABBER));
}

#[test]
fn xi_slave_switch_pointer_alternates_before_master_motion_and_uses_facet_scroll() {
    use crate::core_loop::InputOrigin;

    const RAZER: u64 = 0xA51;
    const HYPERX: u64 = 0xA52;
    const XI_MOTION: u64 = 1 << 6;
    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    let mut peer = install_capture_client(&mut state, 91);
    let razer = pointer_source(&mut state, RAZER, true);
    let hyperx = pointer_source(&mut state, HYPERX, true);
    state.xi_devices.device_mut(razer).unwrap().name = "Razer".to_owned();
    state.xi_devices.device_mut(hyperx).unwrap().name = "HyperX".to_owned();
    assert_eq!((razer, hyperx), (6, 7));
    select_xi2(
        &mut state,
        91,
        2,
        u64::from(crate::xinput::XI2_DEVICE_CHANGED_MASK),
    );

    // Generate independent scroll state through the ordinary pointer
    // event path before checking the classes carried by later switches.
    for (source, device) in [(RAZER, razer), (HYPERX, hyperx)] {
        for (kind, time) in [
            (PointerEventKind::ButtonPress, 1),
            (PointerEventKind::ButtonRelease, 2),
        ] {
            let _dropped = pointer_event_fanout_to_state(
                &mut state,
                &mut backend,
                &HostXidMap::new(),
                source_button_event(
                    kind,
                    InputOrigin::Physical(crate::xinput::InputSourceId(source)),
                    5,
                    time,
                ),
                true,
                false,
            );
        }
        assert_eq!(
            state.xi_devices.device(device).unwrap().scroll_axis_values,
            [1, 0]
        );
        let _ = read_all_capture_available(&mut peer);
    }

    select_xi2(
        &mut state,
        91,
        2,
        u64::from(crate::xinput::XI2_DEVICE_CHANGED_MASK) | XI_MOTION,
    );
    let mut headers = Vec::new();
    for (source, device) in [(RAZER, razer), (HYPERX, hyperx), (HYPERX, hyperx)] {
        let event = HostPointerEvent {
            origin: InputOrigin::Physical(crate::xinput::InputSourceId(source)),
            ..motion_event()
        };
        let _dropped = pointer_event_fanout_to_state(
            &mut state,
            &mut backend,
            &HostXidMap::new(),
            event,
            true,
            false,
        );
        let bytes = read_all_capture_available(&mut peer);
        let mut offset = 0;
        while offset + 32 <= bytes.len() {
            let event_type = u16::from_le_bytes([bytes[offset + 8], bytes[offset + 9]]);
            let event_device = u16::from_le_bytes([bytes[offset + 10], bytes[offset + 11]]);
            let source_offset = if event_type == 1 { 18 } else { 52 };
            let event_source = u16::from_le_bytes([
                bytes[offset + source_offset],
                bytes[offset + source_offset + 1],
            ]);
            if event_type == 1 {
                let reason = bytes[offset + 20];
                let num_classes =
                    u16::from_le_bytes([bytes[offset + 16], bytes[offset + 17]]) as usize;
                let mut class_offset = offset + 32;
                let mut valuators = Vec::new();
                for _ in 0..num_classes {
                    let class_type =
                        u16::from_le_bytes([bytes[class_offset], bytes[class_offset + 1]]);
                    let units =
                        u16::from_le_bytes([bytes[class_offset + 2], bytes[class_offset + 3]])
                            as usize;
                    let class_source =
                        u16::from_le_bytes([bytes[class_offset + 4], bytes[class_offset + 5]]);
                    if class_type == 2 {
                        let number =
                            u16::from_le_bytes([bytes[class_offset + 6], bytes[class_offset + 7]]);
                        let value = i32::from_le_bytes(
                            bytes[class_offset + 28..class_offset + 32]
                                .try_into()
                                .unwrap(),
                        );
                        valuators.push((class_source, number, value));
                    }
                    class_offset += units * 4;
                }
                assert_eq!(
                    class_offset,
                    offset
                        + 32
                        + u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap(),)
                            as usize
                            * 4
                );
                assert_eq!(event_device, 2, "DeviceChanged is on the master pointer");
                assert_eq!(event_source, device, "sourceid is the newly active slave");
                assert_eq!(reason, 1, "XI2.h XISlaveSwitch");
                assert_eq!(
                    valuators,
                    vec![
                        (device, 0, 400),
                        (device, 1, 300),
                        (device, 2, 1),
                        (device, 3, 0),
                    ],
                    "class valuators use the switching facet's scroll snapshot"
                );
            }
            headers.push((event_type, event_device, event_source));
            let units =
                u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
            offset += 32 + units * 4;
        }
        assert_eq!(offset, bytes.len(), "complete XI2 event stream");
    }
    assert_eq!(
        headers,
        vec![
            (1, 2, razer),
            (6, 2, razer),
            (1, 2, hyperx),
            (6, 2, hyperx),
            (6, 2, hyperx),
        ],
        "a source switch precedes that source's first master event; same-source motion has none"
    );

    // A floating slave still delivers its own form, but cannot replace
    // the attached master's last source.
    assert!(state.detach_xi2_slave(razer));
    let floating = HostPointerEvent {
        origin: InputOrigin::XTest(razer),
        ..motion_event()
    };
    let _dropped = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &HostXidMap::new(),
        floating,
        true,
        false,
    );
    let floating_events = read_all_capture_available(&mut peer);
    assert!(
        !floating_events
            .chunks_exact(32)
            .any(|event| event[0] == 35 && u16::from_le_bytes([event[8], event[9]]) == 1),
        "floating input does not emit a master DeviceChanged"
    );
    assert_eq!(state.xi_last_slave(2), Some(hyperx));
    assert_eq!(state.buttons_down, 0);
    assert!(state.sync_pending.is_empty());
    assert!(state.unpublished_pointer_buttons_down.is_empty());
    assert_eq!(state.xi_devices.devices().len(), 6);
    assert_eq!(
        state.xi_devices.device(razer).unwrap().scroll_axis_values,
        [1, 0]
    );
    assert_eq!(
        state.xi_devices.device(hyperx).unwrap().scroll_axis_values,
        [1, 0]
    );
    assert!(state.xi_devices.device(razer).unwrap().enabled);
    assert!(state.xi_devices.device(hyperx).unwrap().enabled);
}

#[test]
fn pointer_source_selection_unpublished_source_tracks_its_own_hold() {
    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    let mut peer = install_capture_client(&mut state, 1);
    for source in 2000..2122 {
        assert_ne!(pointer_source(&mut state, source, true), 0);
    }
    let unpublished = crate::xinput::InputSourceId(330);
    assert_eq!(pointer_source(&mut state, unpublished.0, true), 0);
    select_xi2(&mut state, 1, 2, 1 << 4 | 1 << 5);
    select_xi2(&mut state, 1, 4, 1 << 4 | 1 << 5);

    for (kind, origin, expected) in [
        (
            PointerEventKind::ButtonPress,
            crate::core_loop::InputOrigin::Physical(unpublished),
            vec![(4, 2, 2)],
        ),
        (
            PointerEventKind::ButtonPress,
            crate::core_loop::InputOrigin::XTest(4),
            vec![(4, 4, 4)],
        ),
        (
            PointerEventKind::ButtonRelease,
            crate::core_loop::InputOrigin::Physical(unpublished),
            Vec::new(),
        ),
        (
            PointerEventKind::ButtonRelease,
            crate::core_loop::InputOrigin::XTest(4),
            vec![(5, 4, 4), (5, 2, 4)],
        ),
    ] {
        let dropped = pointer_event_fanout_to_state(
            &mut state,
            &mut backend,
            &HostXidMap::new(),
            source_button_event(kind, origin, 1, 1),
            true,
            false,
        );
        assert!(dropped.is_empty());
        assert_eq!(
            xi2_event_ids(&read_all_capture_available(&mut peer)),
            expected
        );
    }
    assert_eq!(
        state.unpublished_pointer_buttons_down.get(&unpublished),
        None
    );
    assert_eq!(state.buttons_down, 0);
    assert_eq!(state.xi_devices.devices().len(), 126);
    assert_eq!(state.xi_devices.device(4).unwrap().buttons_down, 0);
    assert_eq!(state.sync_pending.len(), 0);
    assert!(state.xi_devices.source(unpublished).is_some());
    assert!(
        state
            .xi_devices
            .facet(unpublished, crate::xinput::XiFacetKind::PointerTouch)
            .is_none()
    );
    assert_eq!(state.buttons_down, 0);
    assert!(
        state
            .xi_devices
            .devices()
            .iter()
            .all(|device| device.buttons_down == 0)
    );
    assert!(
        state
            .xi_devices
            .devices()
            .iter()
            .all(|device| device.scroll_axis_values == [0, 0])
    );
}

#[test]
fn replayed_pointer_press_records_held_state_until_release() {
    let mut state = ServerState::new();
    let mut backend = RecordingBackend::default();
    let origin = crate::core_loop::InputOrigin::XTest(4);
    let press = source_button_event(PointerEventKind::ButtonPress, origin, 1, 1);

    let _ =
        replay_frozen_pointer_event_to_state(&mut state, &mut backend, &HostXidMap::new(), press);
    assert_eq!(
        state.buttons_down, 1,
        "replayed press marks the master button down"
    );
    assert_eq!(
        state.xi_devices.device(4).unwrap().buttons_down,
        1,
        "replayed press marks its generating XI device down"
    );

    let release = source_button_event(PointerEventKind::ButtonRelease, origin, 1, 2);
    let _ = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &HostXidMap::new(),
        release,
        true,
        false,
    );
    assert_eq!(state.buttons_down, 0, "release leaves the master button up");
    assert_eq!(
        state.xi_devices.device(4).unwrap().buttons_down,
        0,
        "release leaves its generating XI device up"
    );
}

#[test]
fn pointer_source_routing_attributes_two_physical_facets() {
    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    let mut peer = install_capture_client(&mut state, 1);
    let first = pointer_source(&mut state, 101, true);
    let second = pointer_source(&mut state, 102, true);
    assert_eq!((first, second), (6, 7));
    state
        .clients
        .get_mut(&1)
        .unwrap()
        .xi2_masks
        .insert((ROOT_WINDOW, 0), (1 << 6) | (1 << 17));

    for source in [101, 102] {
        let mut event = motion_event();
        event.origin =
            crate::core_loop::InputOrigin::Physical(crate::xinput::InputSourceId(source));
        let dropped = pointer_event_fanout_to_state(
            &mut state,
            &mut backend,
            &HostXidMap::new(),
            event,
            true,
            false,
        );
        assert!(dropped.is_empty());
    }

    let events = xi2_event_ids(&read_all_capture_available(&mut peer));
    let device_forms: Vec<_> = events
        .iter()
        .copied()
        .filter(|(kind, _, _)| *kind == 6)
        .collect();
    let raw_forms: Vec<_> = events
        .iter()
        .copied()
        .filter(|(kind, _, _)| *kind == 17)
        .collect();
    assert_eq!(
        device_forms,
        vec![
            (6, first, first),
            (6, 2, first),
            (6, second, second),
            (6, 2, second)
        ]
    );
    assert_eq!(
        raw_forms,
        vec![
            (17, first, first),
            (17, 2, first),
            (17, second, second),
            (17, 2, second),
        ]
    );
    assert_eq!(
        state.xi_devices.facet(
            crate::xinput::InputSourceId(101),
            crate::xinput::XiFacetKind::PointerTouch
        ),
        Some(first)
    );
    assert_eq!(
        state.xi_devices.facet(
            crate::xinput::InputSourceId(102),
            crate::xinput::XiFacetKind::PointerTouch
        ),
        Some(second)
    );
    assert_eq!(state.buttons_down, 0);
}

#[test]
fn pointer_source_routing_preserves_xtest_targets_and_nested_master_only() {
    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    let mut peer = install_capture_client(&mut state, 1);
    let physical = pointer_source(&mut state, 103, true);
    state
        .clients
        .get_mut(&1)
        .unwrap()
        .xi2_masks
        .insert((ROOT_WINDOW, 0), (1 << 6) | (1 << 17));

    for origin in [
        crate::core_loop::InputOrigin::XTest(4),
        crate::core_loop::InputOrigin::XTest(physical),
        crate::core_loop::InputOrigin::NestedHost,
    ] {
        let mut event = motion_event();
        event.origin = origin;
        let dropped = pointer_event_fanout_to_state(
            &mut state,
            &mut backend,
            &HostXidMap::new(),
            event,
            true,
            false,
        );
        assert!(dropped.is_empty());
    }

    let events = xi2_event_ids(&read_all_capture_available(&mut peer));
    let device_forms: Vec<_> = events
        .iter()
        .copied()
        .filter(|(kind, _, _)| *kind == 6)
        .collect();
    let raw_forms: Vec<_> = events
        .iter()
        .copied()
        .filter(|(kind, _, _)| *kind == 17)
        .collect();
    assert_eq!(
        device_forms,
        vec![
            (6, 4, 4),
            (6, 2, 4),
            (6, physical, physical),
            (6, 2, physical),
            (6, 2, 2)
        ]
    );
    assert_eq!(
        raw_forms,
        vec![
            (17, 4, 4),
            (17, 2, 4),
            (17, physical, physical),
            (17, 2, physical),
            (17, 2, 2),
        ]
    );
    assert_eq!(
        state.xi_devices.facet(
            crate::xinput::InputSourceId(103),
            crate::xinput::XiFacetKind::PointerTouch
        ),
        Some(physical)
    );
    assert_eq!(state.buttons_down, 0);
}

#[test]
fn pointer_source_routing_unpublished_source_is_master_only_and_unknown_is_dropped() {
    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    let mut peer = install_capture_client(&mut state, 1);
    for source in 2000..2122 {
        assert_ne!(pointer_source(&mut state, source, true), 0);
    }
    assert_eq!(
        pointer_source(&mut state, 104, true),
        0,
        "physical XI IDs are exhausted"
    );
    state
        .clients
        .get_mut(&1)
        .unwrap()
        .xi2_masks
        .insert((ROOT_WINDOW, 0), (1 << 6) | (1 << 17));

    let mut unpublished = motion_event();
    unpublished.origin = crate::core_loop::InputOrigin::Physical(crate::xinput::InputSourceId(104));
    let dropped = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &HostXidMap::new(),
        unpublished,
        true,
        false,
    );
    assert!(dropped.is_empty());
    let events = xi2_event_ids(&read_all_capture_available(&mut peer));
    assert_eq!(events, vec![(17, 2, 2), (6, 2, 2)]);

    let before_history = state.pointer_motion_history.len();
    let before_registry = state.xi_devices.devices().len();
    let mut unknown = motion_event();
    unknown.origin = crate::core_loop::InputOrigin::Physical(crate::xinput::InputSourceId(999));
    let dropped = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &HostXidMap::new(),
        unknown,
        true,
        false,
    );
    assert!(dropped.is_empty());
    assert!(read_all_capture_available(&mut peer).is_empty());
    assert_eq!(state.pointer_motion_history.len(), before_history);
    assert_eq!(state.xi_devices.devices().len(), before_registry);
    assert!(
        state
            .xi_devices
            .source(crate::xinput::InputSourceId(999))
            .is_none()
    );
    assert_eq!(state.buttons_down, 0);

    let disabled_source = crate::xinput::InputSourceId(105);
    pointer_source(&mut state, disabled_source.0, true);
    let mut disabled_info = state.xi_devices.source(disabled_source).unwrap().clone();
    disabled_info.enabled = false;
    state.xi_register_source(&disabled_info);
    let mut disabled_press = motion_event();
    disabled_press.kind = PointerEventKind::ButtonPress;
    disabled_press.detail = 1;
    disabled_press.origin = crate::core_loop::InputOrigin::Physical(disabled_source);
    let dropped = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &HostXidMap::new(),
        disabled_press,
        true,
        false,
    );
    assert!(dropped.is_empty());
    assert!(read_all_capture_available(&mut peer).is_empty());
    assert_eq!(
        state.buttons_down, 0,
        "disabled source cannot mutate held state"
    );
    assert_eq!(state.sync_pending.len(), 0);
    assert_eq!(state.xi_devices.devices().len(), before_registry);
    assert!(
        state
            .xi_devices
            .facet(disabled_source, crate::xinput::XiFacetKind::PointerTouch)
            .is_none()
    );
    assert!(!state.xi_devices.source(disabled_source).unwrap().enabled);
}

#[test]
fn translated_motion_is_recorded_once_in_bounded_history() {
    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    let event = motion_event();
    let _ = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &HostXidMap::new(),
        event,
        true,
        false,
    );
    assert_eq!(state.pointer_motion_history.len(), 1);
    assert_eq!(state.pointer_motion_history[0].time, event.time);
    assert_eq!(state.pointer_motion_history[0].root_x, event.root_x);
    assert_eq!(state.pointer_motion_history[0].root_y, event.root_y);
}

/// Regression (HW-confirmed 2026-07-10, Telegram/Qt on silence):
/// a client selecting `XIAllDevices(0)` receives every pointer event
/// in BOTH slave- and master-stamped forms, and the SLAVE form MUST be
/// emitted first — matching Xorg's `mieqProcessDeviceEvent`
/// ("process slave first, then master", mi/mieq.c). Qt compresses
/// consecutive XI_Motion keeping only the last (qxcbconnection.cpp) and
/// drops the slave-deviceid copy (qxcbconnection_xi2.cpp:689). With the
/// old master-first order every master copy was trailed by its slave
/// copy → compressed away → and the surviving slave copy dropped, so
/// ZERO motion (and thus zero smooth-scroll, which rides XI_Motion)
/// reached the widget: hover-scrollbars + wheel dead while clicks
/// (not motion-compressed) worked. Slave-first leaves the master copy
/// — the one Qt keeps — trailing, so it survives compression.
/// Issue #94 follow-up: an XI2 client holding an active `XIGrabDevice`
/// (`owner_events=true`) must receive the grabbed `ButtonRelease` even
/// when the pointer is over a window it never `XISelectEvents`'d on —
/// Xorg `DeliverGrabbedEvent` falls back to grab-window delivery using
/// the grab mask. CEF grabs the pointer on mousedown and holds it to
/// mouseup; the release fires over a window it holds only via the grab,
/// so without the fallback the mouse-up reaches nobody and clicks never
/// complete (Steam nav/menus/close all dead post-crash-fix).
#[test]
fn xi2_grabbed_button_release_reaches_grab_owner_via_grab_window() {
    use crate::server::ActivePointerGrab;
    use yserver_protocol::x11::{CreateWindowRequest, ResourceId};
    const GC: u32 = 2; // grab owner (CEF-like)
    let grab_win = ResourceId(0x0020_0001); // GC's window == grab window
    let hit_win = ResourceId(0x0020_0002); // child GC owns but did NOT select on

    let mut state = ServerState::new();
    let mut backend = crate::backend::recording::RecordingBackend::default();
    let mut gc_peer = install_client(&mut state, GC);

    state.resources.create_window(
        ClientId(GC),
        CreateWindowRequest {
            depth: 24,
            window: grab_win,
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
    state.resources.create_window(
        ClientId(GC),
        CreateWindowRequest {
            depth: 24,
            window: hit_win,
            parent: grab_win,
            x: 10,
            y: 10,
            width: 40,
            height: 40,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(grab_win);
    let _ = state.resources.map_window(hit_win);

    // Active XI2 grab held by GC (matching CEF's XIGrabDevice device=2,
    // async, owner_events=true). GC has NO XISelectEvents mask anywhere
    // — it holds the pointer purely via the grab.
    state.active_pointer_grab = Some(ActivePointerGrab {
        owner: ClientId(GC),
        grab_window: grab_win,
        event_mask: 0xFFFF,
        cursor: ResourceId(0),
        time: 0,
        owner_events: true,
        via_xi2: true,
        implicit: false,
        passive: false,
        xi2_mask: u64::MAX,
    });

    let mut xid_map = HostXidMap::new();
    xid_map.insert(0xCAFE_u32, grab_win);

    let mut rel = motion_event();
    rel.kind = PointerEventKind::ButtonRelease;
    rel.host_xid = 0xCAFE;
    rel.detail = 1;
    rel.root_x = 20;
    rel.root_y = 20;
    rel.event_x = 20;
    rel.event_y = 20;

    let mut press = rel;
    press.kind = PointerEventKind::ButtonPress;
    press.state = 0;
    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, press, true, false);
    let _ = read_all_available(&mut gc_peer);

    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, rel, true, false);

    let bytes = read_all_available(&mut gc_peer);
    let mut found = None;
    let mut off = 0usize;
    while off + 32 <= bytes.len() {
        if bytes[off] == 35 && u16::from_le_bytes([bytes[off + 8], bytes[off + 9]]) == 5 {
            found = Some(u32::from_le_bytes(
                bytes[off + 24..off + 28].try_into().unwrap(),
            ));
            break;
        }
        let length = u32::from_le_bytes(bytes[off + 4..off + 8].try_into().unwrap()) as usize;
        off += 32 + length * 4;
    }
    assert!(
        found.is_some(),
        "grab owner must receive the grabbed XI2 ButtonRelease (Xorg grab-window fallback); got none"
    );
    assert_eq!(
        found.unwrap(),
        grab_win.0,
        "grabbed release must be reported on the grab window"
    );
}

/// Core grabs use the same grab-window fallback as XI2 grabs: when
/// owner-events delivery finds no selected natural target, the grab
/// owner still receives the event on the grab window.
#[test]
fn core_grabbed_button_release_reaches_grab_owner_via_grab_window() {
    use crate::server::ActivePointerGrab;
    use yserver_protocol::x11::{CreateWindowRequest, ResourceId};

    const OWNER: u32 = 2;
    let grab_win = ResourceId(0x0020_0001);
    let hit_win = ResourceId(0x0020_0002);

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::default();
    let mut owner_peer = install_client(&mut state, OWNER);

    for (window, parent, x, y, width, height) in [
        (grab_win, ROOT_WINDOW, 0, 0, 100, 100),
        (hit_win, grab_win, 10, 10, 40, 40),
    ] {
        state.resources.create_window(
            ClientId(OWNER),
            CreateWindowRequest {
                depth: 24,
                window,
                parent,
                x,
                y,
                width,
                height,
                border_width: 0,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
    }
    let _ = state.resources.map_window(grab_win);
    let _ = state.resources.map_window(hit_win);

    // No core event mask is selected on either window. Delivery must
    // therefore fall back to the grab's event mask and grab window.
    state.active_pointer_grab = Some(ActivePointerGrab {
        owner: ClientId(OWNER),
        grab_window: grab_win,
        event_mask: 0xFFFF,
        cursor: ResourceId(0),
        time: 0,
        owner_events: true,
        via_xi2: false,
        implicit: false,
        passive: false,
        xi2_mask: 0,
    });

    let mut xid_map = HostXidMap::new();
    xid_map.insert(0xCAFE, hit_win);

    let mut release = motion_event();
    release.kind = PointerEventKind::ButtonRelease;
    release.host_xid = 0xCAFE;
    release.detail = 1;
    release.root_x = 20;
    release.root_y = 20;
    release.event_x = 10;
    release.event_y = 10;

    let mut press = release;
    press.kind = PointerEventKind::ButtonPress;
    press.state = 0;
    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, press, true, false);
    let _ = read_all_available(&mut owner_peer);

    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, release, true, false);

    // Core ButtonRelease is event code 5 and its event window occupies
    // bytes 12..16 of the fixed 32-byte event.
    let bytes = read_all_available(&mut owner_peer);
    let event_window = bytes.chunks_exact(32).find_map(|event| {
        (event[0] & 0x7F == 5)
            .then(|| u32::from_le_bytes(event[12..16].try_into().expect("event window bytes")))
    });
    assert_eq!(
        event_window,
        Some(grab_win.0),
        "grabbed core release must be reported on the grab window",
    );
}

/// Issue #94 follow-up (Steam menu/Library input-wedge, HW-confirmed
/// 2026-07-15): the QUEUE-WHILE-FROZEN gate must key on the UNIFIED
/// device freeze state alone — `xi1_frozen[PTR].frozen()`, i.e. Xorg's
/// `sync.frozen = sync.other || state >= FROZEN` (dix/events.c:1327) —
/// and NOT additionally on the legacy core passive-grab fields
/// (the former passive-grab flag plus a stored frozen event). Both were set
/// together when a sync passive grab activates, but several paths thaw
/// the unified state independently of the core fields (traced: marco's
/// click-to-focus sync grab thawed by a later UngrabKeyboard, and by a
/// SetInputFocus-driven async grab). With the old OR-gate the pointer
/// then queued every event forever with no path to thaw — total input
/// wedge, zap-only (q climbed to 134 while xi1_frozen[PTR]=Thawed,
/// core_passive_grab=Some, frozen_ptr_event still set). Once the unified
/// state is thawed, events MUST flow, never enqueue.
#[test]
fn thawed_unified_state_does_not_queue_despite_lingering_passive_grab() {
    use crate::xinput::DEVICEID_XTEST_POINTER as PTR;
    const OWNER: u32 = 3;
    let grab_win = yserver_protocol::x11::ResourceId(0x0020_0001);

    let mut state = ServerState::new();
    let mut backend = crate::backend::recording::RecordingBackend::default();
    let _peer = install_client(&mut state, OWNER);

    // A sync passive grab is still active from the core's POV, but the
    // unified per-device freeze has already been thawed out-of-band. The
    // gate must key on the unified freeze state alone: an active passive
    // grab must not resurrect the freeze. (Pre-unification this was a
    // second source of truth; the legacy core slots are now gone, so the
    // only way to express "frozen" is the unified `xi1_frozen` state — left
    // Thawed here, the authoritative signal saying "not frozen".)
    state.set_pointer_grab(crate::server::ActivePointerGrab {
        owner: ClientId(OWNER),
        grab_window: grab_win,
        event_mask: 0xffff,
        cursor: yserver_protocol::x11::ResourceId(0),
        time: 0,
        owner_events: false,
        via_xi2: false,
        implicit: false,
        passive: true,
        xi2_mask: 0,
    });
    assert!(
        !state
            .xi1_frozen
            .get(&PTR)
            .is_some_and(crate::server::Xi1Freeze::frozen),
        "precondition: unified pointer state is thawed"
    );

    let xid_map = HostXidMap::new();
    let ev = motion_event();
    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, ev, true, false);

    assert!(
        !state
            .sync_pending
            .iter()
            .any(|p| p.device == crate::xinput::DEVICEID_XTEST_POINTER),
        "a pointer event must NOT be swallowed into the freeze queue when the \
             unified device state is thawed (Xorg gates enqueue on sync.frozen only); \
             got {} queued — the Steam input-wedge",
        state
            .sync_pending
            .iter()
            .filter(|p| p.device == crate::xinput::DEVICEID_XTEST_POINTER)
            .count(),
    );
}

/// XI2 device propagation stops at the first selected window, across all
/// clients.  The leaf selector receives the event and an unrelated
/// selector on an ancestor does not receive a duplicate.  Xorg does this
/// for Steam's GTK child / CEF top-level pair; delivering both copies made
/// CEF interpret an ordinary menu click as a client-side-decoration drag.
#[test]
fn xi2_device_event_stops_at_deepest_selected_window() {
    use yserver_protocol::x11::{CreateWindowRequest, ResourceId};
    let toplevel = ResourceId(0x0020_0001); // owned by client A
    let leaf = ResourceId(0x0020_0002); // child of toplevel, owned by A

    let mut state = ServerState::new();
    let mut a_peer = install_client(&mut state, 1); // owns both windows
    let mut b_peer = install_client(&mut state, 2); // selects on the toplevel only

    state.resources.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: toplevel,
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
    state.resources.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: leaf,
            parent: toplevel,
            x: 10,
            y: 10,
            width: 40,
            height: 40,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(toplevel);
    let _ = state.resources.map_window(leaf);

    // A selects XI_ButtonPress(4) under XIAllDevices(0) on the LEAF.
    state
        .clients
        .get_mut(&1)
        .unwrap()
        .xi2_masks
        .insert((leaf, 0u16), 1 << 4);
    // B selects the same event on the TOPLEVEL only.
    state
        .clients
        .get_mut(&2)
        .unwrap()
        .xi2_masks
        .insert((toplevel, 0u16), 1 << 4);

    let mut xid_map = HostXidMap::new();
    xid_map.insert(0xCAFE_u32, toplevel);
    let mut backend = crate::backend::recording::RecordingBackend::default();

    let _ = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &xid_map,
        HostPointerEvent {
            origin: crate::core_loop::message::InputOrigin::XTest(4),
            kind: PointerEventKind::ButtonPress,
            host_xid: 0xCAFE,
            detail: 1,
            time: 1,
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
        },
        true,
        false,
    );

    // event window is bytes[24..28] of an XI2 device event (GenericEvent
    // type 35, evtype at [8..10]).
    let button_event_window = |bytes: &[u8]| -> Option<u32> {
        let mut off = 0usize;
        while off + 32 <= bytes.len() {
            if bytes[off] == 35 && u16::from_le_bytes([bytes[off + 8], bytes[off + 9]]) == 4 {
                return Some(u32::from_le_bytes(
                    bytes[off + 24..off + 28].try_into().unwrap(),
                ));
            }
            let length = u32::from_le_bytes(bytes[off + 4..off + 8].try_into().unwrap()) as usize;
            off += 32 + length * 4;
        }
        None
    };

    let a_bytes = read_all_available(&mut a_peer);
    let b_bytes = read_all_available(&mut b_peer);
    assert_eq!(
        button_event_window(&a_bytes),
        Some(leaf.0),
        "owner/leaf-selector must be reported on the hit leaf"
    );
    assert_eq!(
        button_event_window(&b_bytes),
        None,
        "Xorg stops XI2 propagation at the selected leaf; an ancestor \
             selector must not receive a duplicate device event"
    );
    assert_eq!(
        state
            .active_pointer_grab
            .map(|grab| (grab.owner, grab.grab_window)),
        Some((ClientId(1), leaf)),
        "the implicit grab belongs to the deepest XI2 press recipient"
    );
}

#[test]
fn xi2_motion_emits_slave_form_before_master_form() {
    let mut state = ServerState::new();
    let mut backend = crate::backend::recording::RecordingBackend::new();
    let mut peer = install_client(&mut state, 1);
    // XIAllDevices(0), XI_Motion (evtype 6), on the root — where a
    // windowless motion lands. device 0 yields BOTH forms (issue #72).
    state
        .clients
        .get_mut(&1)
        .expect("client")
        .xi2_masks
        .insert((ROOT_WINDOW, 0), 1 << 6);
    let dropped = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &HostXidMap::new(),
        motion_event(),
        true,
        false,
    );
    assert!(dropped.is_empty());
    let bytes = read_all_available(&mut peer);
    // Walk the GenericEvent stream, collecting each XI_Motion(6)'s
    // deviceid. Layout: [0]=35 GenericEvent, [4..8]=length (extra
    // 4-byte units past the 32-byte base), [8..10]=evtype,
    // [10..12]=deviceid.
    let mut motion_deviceids: Vec<u16> = Vec::new();
    let mut off = 0usize;
    while off + 32 <= bytes.len() {
        assert_eq!(bytes[off], 35, "GenericEvent");
        let length = u32::from_le_bytes(bytes[off + 4..off + 8].try_into().unwrap()) as usize;
        let evtype = u16::from_le_bytes(bytes[off + 8..off + 10].try_into().unwrap());
        let deviceid = u16::from_le_bytes(bytes[off + 10..off + 12].try_into().unwrap());
        if evtype == 6 {
            motion_deviceids.push(deviceid);
        }
        off += 32 + length * 4;
    }
    assert_eq!(off, bytes.len(), "event stream fully consumed");
    assert_eq!(
        motion_deviceids,
        vec![XI2_XTEST_POINTER_DEVICE_ID, XI2_MASTER_POINTER_DEVICE_ID],
        "XIAllDevices(0) motion selector must receive the slave-stamped \
             form BEFORE the master-stamped form (Xorg mi/mieq.c order); \
             master-first silently breaks Qt smooth-scroll + hover"
    );
}

/// Pointer family: a core-motion and an XI2-motion subscriber over
/// `OUTBOUND_CAP` are flagged for the core loop to disconnect, and the
/// reading subscriber still gets the motion.
#[test]
fn overflowing_motion_subscribers_are_flagged_for_disconnect() {
    for xi2 in [false, true] {
        let mut state = ServerState::new();
        let mut backend = crate::backend::recording::RecordingBackend::new();
        let _slow = install_client(&mut state, 1);
        let mut fast = install_client(&mut state, 2);
        for id in [1, 2] {
            let client = state.clients.get_mut(&id).expect("client");
            if xi2 {
                client.xi2_masks.insert((ROOT_WINDOW, 1), 1 << 6);
            } else {
                client.event_masks.insert(ROOT_WINDOW, 1 << 6);
            }
        }
        crate::core_loop::client_io::saturate_for_test(state.clients.get_mut(&1).expect("client"));
        let _ = pointer_event_fanout_to_state(
            &mut state,
            &mut backend,
            &HostXidMap::new(),
            motion_event(),
            true,
            false,
        );
        assert_eq!(
            crate::core_loop::client_io::failed_writers(&state.clients),
            [ClientId(1)],
            "xi2={xi2}"
        );
        assert!(!read_all_available(&mut fast).is_empty(), "xi2={xi2}");
    }
}

#[test]
fn xi2_raw_motion_uses_master_device_and_slave_source_ids() {
    let mut state = ServerState::new();
    let mut backend = crate::backend::recording::RecordingBackend::new();
    let mut facet_peer = install_client(&mut state, 1);
    let mut master_peer = install_client(&mut state, 2);
    let mut all_master_peer = install_client(&mut state, 3);
    let mut all_devices_peer = install_client(&mut state, 4);
    let facet = pointer_source(&mut state, 101, true);
    for (client_id, device_id) in [
        (1, facet),
        (2, XI2_MASTER_POINTER_DEVICE_ID),
        (3, 1),
        (4, 0),
    ] {
        state
            .clients
            .get_mut(&client_id)
            .expect("client")
            .xi2_masks
            .insert((ROOT_WINDOW, device_id), 1 << 17);
    }
    let mut event = motion_event();
    event.origin = crate::core_loop::InputOrigin::Physical(crate::xinput::InputSourceId(101));
    let dropped = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &HostXidMap::new(),
        event,
        true,
        false,
    );
    assert!(dropped.is_empty());
    let facet_raw: Vec<_> = xi2_event_ids(&read_all_available(&mut facet_peer))
        .into_iter()
        .filter(|(kind, _, _)| *kind == 17)
        .collect();
    let master_raw: Vec<_> = xi2_event_ids(&read_all_available(&mut master_peer))
        .into_iter()
        .filter(|(kind, _, _)| *kind == 17)
        .collect();
    let all_master_raw: Vec<_> = xi2_event_ids(&read_all_available(&mut all_master_peer))
        .into_iter()
        .filter(|(kind, _, _)| *kind == 17)
        .collect();
    let all_devices_raw: Vec<_> = xi2_event_ids(&read_all_available(&mut all_devices_peer))
        .into_iter()
        .filter(|(kind, _, _)| *kind == 17)
        .collect();
    assert_eq!(
        facet_raw,
        vec![(17, facet, facet)],
        "a raw selection on the physical facet gets only the slave form"
    );
    assert_eq!(
        master_raw,
        vec![(17, XI2_MASTER_POINTER_DEVICE_ID, facet)],
        "a raw selection on the master pointer gets only the master form"
    );
    assert_eq!(
        all_master_raw,
        vec![(17, XI2_MASTER_POINTER_DEVICE_ID, facet)],
        "XIAllMasterDevices gets only the master form"
    );
    assert_eq!(
        all_devices_raw,
        vec![
            (17, facet, facet),
            (17, XI2_MASTER_POINTER_DEVICE_ID, facet),
        ],
        "XIAllDevices gets slave raw first, then the master raw copy"
    );
}

#[test]
fn xi2_raw_buttons_use_both_device_forms_per_selection() {
    let mut state = ServerState::new();
    let mut backend = crate::backend::recording::RecordingBackend::new();
    let mut facet_peer = install_client(&mut state, 1);
    let mut master_peer = install_client(&mut state, 2);
    let mut all_master_peer = install_client(&mut state, 3);
    let mut all_devices_peer = install_client(&mut state, 4);
    let facet = pointer_source(&mut state, 101, true);
    let raw_button_mask = (1 << 15) | (1 << 16);
    for (client_id, device_id) in [
        (1, facet),
        (2, XI2_MASTER_POINTER_DEVICE_ID),
        (3, 1),
        (4, 0),
    ] {
        state
            .clients
            .get_mut(&client_id)
            .expect("client")
            .xi2_masks
            .insert((ROOT_WINDOW, device_id), raw_button_mask);
    }

    let mut press = motion_event();
    press.origin = crate::core_loop::InputOrigin::Physical(crate::xinput::InputSourceId(101));
    press.kind = PointerEventKind::ButtonPress;
    press.detail = 1;
    press.time = 10;
    assert!(
        pointer_event_fanout_to_state(
            &mut state,
            &mut backend,
            &HostXidMap::new(),
            press,
            true,
            false,
        )
        .is_empty()
    );

    let mut release = press;
    release.kind = PointerEventKind::ButtonRelease;
    release.state = 1 << 8;
    release.time = 11;
    assert!(
        pointer_event_fanout_to_state(
            &mut state,
            &mut backend,
            &HostXidMap::new(),
            release,
            true,
            false,
        )
        .is_empty()
    );

    let facet_raw: Vec<_> = xi2_event_ids(&read_all_available(&mut facet_peer))
        .into_iter()
        .filter(|(kind, _, _)| matches!(*kind, 15 | 16))
        .collect();
    let master_raw: Vec<_> = xi2_event_ids(&read_all_available(&mut master_peer))
        .into_iter()
        .filter(|(kind, _, _)| matches!(*kind, 15 | 16))
        .collect();
    let all_master_raw: Vec<_> = xi2_event_ids(&read_all_available(&mut all_master_peer))
        .into_iter()
        .filter(|(kind, _, _)| matches!(*kind, 15 | 16))
        .collect();
    let all_devices_raw: Vec<_> = xi2_event_ids(&read_all_available(&mut all_devices_peer))
        .into_iter()
        .filter(|(kind, _, _)| matches!(*kind, 15 | 16))
        .collect();
    assert_eq!(facet_raw, vec![(15, facet, facet), (16, facet, facet)]);
    assert_eq!(
        master_raw,
        vec![
            (15, XI2_MASTER_POINTER_DEVICE_ID, facet),
            (16, XI2_MASTER_POINTER_DEVICE_ID, facet),
        ]
    );
    assert_eq!(
        all_master_raw,
        vec![
            (15, XI2_MASTER_POINTER_DEVICE_ID, facet),
            (16, XI2_MASTER_POINTER_DEVICE_ID, facet),
        ]
    );
    assert_eq!(
        all_devices_raw,
        vec![
            (15, facet, facet),
            (15, XI2_MASTER_POINTER_DEVICE_ID, facet),
            (16, facet, facet),
            (16, XI2_MASTER_POINTER_DEVICE_ID, facet),
        ],
        "XIAllDevices gets slave raw before master raw for both button edges"
    );
}

#[test]
fn replayed_pointer_press_does_not_repeat_raw_event() {
    let mut state = ServerState::new();
    let mut backend = crate::backend::recording::RecordingBackend::new();
    let mut peer = install_client(&mut state, 1);
    state
        .clients
        .get_mut(&1)
        .expect("client")
        .xi2_masks
        .insert((ROOT_WINDOW, 0), (1 << 4) | (1 << 15));
    let mut press = motion_event();
    press.kind = PointerEventKind::ButtonPress;
    press.detail = 1;

    let dropped =
        replay_frozen_pointer_event_to_state(&mut state, &mut backend, &HostXidMap::new(), press);
    assert!(dropped.is_empty());

    let bytes = read_all_available(&mut peer);
    let mut evtypes = Vec::new();
    let mut off = 0usize;
    while off + 32 <= bytes.len() {
        assert_eq!(bytes[off], 35, "GenericEvent");
        let length = u32::from_le_bytes(bytes[off + 4..off + 8].try_into().unwrap()) as usize;
        evtypes.push(u16::from_le_bytes([bytes[off + 8], bytes[off + 9]]));
        off += 32 + length * 4;
    }
    assert_eq!(off, bytes.len(), "event stream fully consumed");
    assert!(
        evtypes.contains(&4),
        "natural XI_ButtonPress must be replayed"
    );
    assert!(
        !evtypes.iter().any(|evtype| matches!(*evtype, 15..=17)),
        "raw pointer events describe physical input and must not be replayed"
    );
}

#[test]
fn button_target_locked_at_generation_survives_restack() {
    // Click-below regression. A button generated over the top-level A
    // must deliver to A even if a WM raises B above A between the
    // press and its fanout. yserver used to re-resolve the target at
    // delivery (`root_pointer_target_at` on the current tree), so a
    // restack landing in between retargeted the in-flight click to
    // the window raised on top — "click lands on the window below".
    use crate::resources::{ROOT_VISUAL, ROOT_WINDOW};
    use yserver_protocol::x11::{ConfigureWindowRequest, CreateWindowRequest};

    let mut state = ServerState::new();
    // Two overlapping full-size top-levels. Create B first, then A, so
    // A is on top (create inserts at the top of the stack).
    let b = ResourceId(0x0020_0000);
    let a = ResourceId(0x0010_0000);
    for win in [b, a] {
        state.resources.create_window(
            ClientId(1),
            CreateWindowRequest {
                depth: 24,
                window: win,
                parent: ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 400,
                height: 400,
                border_width: 0,
                class: 1,
                visual: ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = state.resources.map_window(win);
    }

    // The producer stamped host_xid HA at generation, when A was on top.
    let host_a: u32 = 0x0040_0aaa;
    let mut xid_map = HostXidMap::new();
    xid_map.insert(host_a, a);
    let press = HostPointerEvent {
        origin: crate::core_loop::message::InputOrigin::XTest(4),
        kind: PointerEventKind::ButtonPress,
        host_xid: host_a,
        detail: 1,
        time: 1,
        root_x: 100,
        root_y: 100,
        event_x: 100,
        event_y: 100,
        state: 0,
        crossing_mode: 0,
        child: 0,
        raw_dx: 0,
        raw_dy: 0,
        tree_change: false,
    };

    // Precondition: A is on top; the press resolves to A.
    assert_eq!(
        resolve_pointer_hit(&state, &xid_map, &press)
            .map(|(t, _, _)| state.top_level_for_target(t)),
        Some(a),
        "precondition: click resolves to the top window A",
    );

    // A WM raises B above A AFTER the press was generated.
    let restacked = state.resources.configure_window(ConfigureWindowRequest {
        window: b,
        value_mask: 0,
        x: None,
        y: None,
        width: None,
        height: None,
        border_width: None,
        sibling: Some(a),
        stack_mode: Some(0), // Above
    });
    assert!(restacked.is_some(), "restack applied");

    // The LIVE hit-test now resolves B (B is topmost). This proves the
    // scenario diverges — so a delivery-time re-resolve would pick B.
    assert_eq!(
        state
            .root_pointer_target_at(100, 100)
            .map(|(t, _, _)| state.top_level_for_target(t)),
        Some(b),
        "after the restack, the live hit-test resolves the now-top window B",
    );

    // Regression: the in-flight button must STILL deliver to A — its
    // generation-time target — not the restacked-on-top B.
    assert_eq!(
        resolve_pointer_hit(&state, &xid_map, &press)
            .map(|(t, _, _)| state.top_level_for_target(t)),
        Some(a),
        "button target is locked at event generation; a restack between \
             press and delivery must not retarget it to the window below",
    );

    // A motion event, by contrast, tracks the live pointer (resolves B).
    let motion = HostPointerEvent {
        origin: crate::core_loop::message::InputOrigin::XTest(4),
        kind: PointerEventKind::MotionNotify,
        ..press
    };
    assert_eq!(
        resolve_pointer_hit(&state, &xid_map, &motion)
            .map(|(t, _, _)| state.top_level_for_target(t)),
        Some(b),
        "motion tracks the live pointer (resolves the now-top B)",
    );
}

#[test]
fn motion_clamps_against_solid_barrier() {
    let mut state = ServerState::new();
    let mut backend = crate::backend::recording::RecordingBackend::new();
    state.pointer_barriers.insert(
        0x0050_0001,
        crate::server::PointerBarrier {
            owner: ClientId(1),
            window: ROOT_WINDOW,
            x1: 100,
            y1: 0,
            x2: 100,
            y2: 200,
            directions: 0,
            devices: Vec::new(),
            hit: false,
            seen: false,
            event_id: 1,
            release_event_id: 0,
            last_timestamp: 0,
        },
    );
    state.pointer_root = (90, 50);
    let mut ev = motion_event();
    ev.root_x = 110;
    ev.root_y = 50;
    let dropped = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &HostXidMap::new(),
        ev,
        true,
        false,
    );
    assert!(dropped.is_empty());
    assert_eq!(state.pointer_root, (99, 50));
    assert_eq!(backend.warped_to, Some((99, 50)));
}

/// Regression (HW-observed 2026-06-18): the clamp moved only
/// `root_x`, but `translate_host_event` later re-derives
/// `root = window.x + event_x`. With the host window in `xid_map`
/// (the real path — the empty-map case above can't exercise it)
/// that overwrote the clamped root with the un-clamped value, so
/// `pointer_root` ended up *past* the wall and the next motion never
/// re-crossed it — a porous barrier. The clamp must shift `event_x`
/// by the same delta so the wall holds across translate.
#[test]
fn clamp_shifts_event_x_so_pointer_root_holds_across_translate() {
    let mut state = ServerState::new();
    let mut backend = crate::backend::recording::RecordingBackend::new();
    state.pointer_barriers.insert(
        0x0050_0001,
        crate::server::PointerBarrier {
            owner: ClientId(1),
            window: ROOT_WINDOW,
            x1: 100,
            y1: 0,
            x2: 100,
            y2: 200,
            directions: 0,
            devices: Vec::new(),
            hit: false,
            seen: false,
            event_id: 1,
            release_event_id: 0,
            last_timestamp: 0,
        },
    );
    state.pointer_root = (90, 50);
    // Map the host window so `translate_host_event` re-derives root
    // from `event_x` — ROOT_WINDOW is at (0,0), so root == event_x.
    let host_xid = 0x1234_u32;
    let mut xid_map = HostXidMap::new();
    xid_map.insert(host_xid, ROOT_WINDOW);
    let mut ev = motion_event();
    ev.host_xid = host_xid;
    ev.root_x = 110;
    ev.root_y = 50;
    ev.event_x = 110;
    ev.event_y = 50;
    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, ev, true, false);
    // Pre-fix this was (110, 50): translate clobbered the clamp.
    assert_eq!(
        state.pointer_root,
        (99, 50),
        "pointer_root must hold at the wall (x1-1), not snap back past it"
    );
    assert_eq!(backend.warped_to, Some((99, 50)));
}

#[test]
fn barrier_bypass_skips_clamp() {
    let mut state = ServerState::new();
    let mut backend = crate::backend::recording::RecordingBackend::new();
    state.pointer_barriers.insert(
        0x0050_0001,
        crate::server::PointerBarrier {
            owner: ClientId(1),
            window: ROOT_WINDOW,
            x1: 100,
            y1: 0,
            x2: 100,
            y2: 200,
            directions: 0,
            devices: Vec::new(),
            hit: false,
            seen: false,
            event_id: 1,
            release_event_id: 0,
            last_timestamp: 0,
        },
    );
    state.pointer_root = (90, 50);
    state.barrier_bypass = true;
    let mut ev = motion_event();
    ev.root_x = 110;
    ev.root_y = 50;
    let dropped = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &HostXidMap::new(),
        ev,
        true,
        false,
    );
    assert!(dropped.is_empty());
    assert_eq!(state.pointer_root, (110, 50));
    assert_eq!(backend.warped_to, None);
}

#[test]
fn barrier_hit_event_delivered_to_selecting_client() {
    let mut state = ServerState::new();
    let mut backend = crate::backend::recording::RecordingBackend::new();
    let mut peer = install_client(&mut state, 1);
    state
        .clients
        .get_mut(&1)
        .expect("client")
        .xi2_masks
        .insert((ROOT_WINDOW, 2), 1 << 25);
    let bid = 0x0050_0002;
    state.pointer_barriers.insert(
        bid,
        crate::server::PointerBarrier {
            owner: ClientId(1),
            window: ROOT_WINDOW,
            x1: 100,
            y1: 0,
            x2: 100,
            y2: 200,
            directions: 0,
            devices: Vec::new(),
            hit: false,
            seen: false,
            event_id: 1,
            release_event_id: 0,
            last_timestamp: 0,
        },
    );
    state.pointer_root = (90, 50);
    let mut ev = motion_event();
    ev.root_x = 110;
    ev.root_y = 50;
    let dropped = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &HostXidMap::new(),
        ev,
        true,
        false,
    );
    assert!(dropped.is_empty());
    assert_eq!(state.pointer_root, (99, 50));
    assert!(state.pointer_barriers.get(&bid).expect("barrier").hit);
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes.len(), 68);
    assert_eq!(bytes[0], 35, "GenericEvent");
    assert_eq!(bytes[1], 137, "XI2 major opcode");
    assert_eq!(&bytes[4..8], &9u32.to_le_bytes(), "length");
    assert_eq!(&bytes[8..10], &25u16.to_le_bytes(), "BarrierHit");
    assert_eq!(&bytes[28..32], &bid.to_le_bytes(), "barrier xid");
    assert_eq!(
        &bytes[44..48],
        &(99i32 << 16).to_le_bytes(),
        "root_x FP1616"
    );
}

fn barrier_hit_bytes_for_selector(device_id: u16) -> Vec<u8> {
    let mut state = ServerState::new();
    let mut backend = crate::backend::recording::RecordingBackend::new();
    let mut peer = install_capture_client(&mut state, 1);
    let source_id = crate::xinput::InputSourceId(101);
    let pointer_id = pointer_source(&mut state, source_id.0, true);
    state
        .clients
        .get_mut(&1)
        .expect("client")
        .xi2_masks
        .insert((ROOT_WINDOW, device_id), 1 << 25);
    assert_eq!(
        barrier_xi2_targets(&state, ROOT_WINDOW, 25),
        if matches!(device_id, 0..=2) {
            vec![ClientId(1)]
        } else {
            Vec::new()
        },
        "barrier selection candidates for device {device_id}",
    );
    let bid = 0x0050_0006;
    state.pointer_barriers.insert(
        bid,
        crate::server::PointerBarrier {
            owner: ClientId(1),
            window: ROOT_WINDOW,
            x1: 100,
            y1: 0,
            x2: 100,
            y2: 200,
            directions: 0,
            devices: Vec::new(),
            hit: false,
            seen: false,
            event_id: 1,
            release_event_id: 0,
            last_timestamp: 0,
        },
    );
    state.pointer_root = (90, 50);
    let mut event = motion_event();
    event.origin = crate::core_loop::message::InputOrigin::Physical(source_id);
    event.root_x = 110;
    event.root_y = 50;

    let dropped = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &HostXidMap::new(),
        event,
        true,
        false,
    );

    assert!(dropped.is_empty());
    assert_eq!(state.pointer_root, (99, 50));
    assert!(state.pointer_barriers.get(&bid).expect("barrier").hit);
    assert_eq!(
        state
            .xi_devices
            .facet(source_id, crate::xinput::XiFacetKind::PointerTouch),
        Some(pointer_id)
    );
    assert_eq!(
        state.xi_devices.source(source_id).map(|info| info.enabled),
        Some(true)
    );
    assert_eq!(
        state
            .xi_devices
            .device(pointer_id)
            .map(|device| device.buttons_down),
        Some(0),
        "barrier handling must not alter the source's held-button state",
    );
    read_all_capture_available(&mut peer)
}

#[test]
fn xi_dynamic_reset_barrier_targets_master_selectors_not_xtest_pointer() {
    let xtest = barrier_hit_bytes_for_selector(crate::xinput::DEVICEID_XTEST_POINTER);
    assert!(
        xtest.is_empty(),
        "a master pointer barrier event must not be selected through virtual XTEST pointer 4"
    );

    for selector in [
        XI2_MASTER_POINTER_DEVICE_ID,
        1, // XIAllMasterDevices
        0, // XIAllDevices
    ] {
        let bytes = barrier_hit_bytes_for_selector(selector);
        assert_eq!(bytes.len(), 68, "selector {selector} receives BarrierHit");
        assert_eq!(&bytes[8..10], &25u16.to_le_bytes(), "BarrierHit");
        assert_eq!(
            &bytes[10..12],
            &XI2_MASTER_POINTER_DEVICE_ID.to_le_bytes(),
            "barrier deviceid remains the master pointer",
        );
        assert_eq!(
            &bytes[40..42],
            &XI2_MASTER_POINTER_DEVICE_ID.to_le_bytes(),
            "barrier sourceid remains the master pointer",
        );
    }
}

/// Regression (HW-observed 2026-06-18): a client that selected
/// BarrierHit under the `XIAllMasterDevices` wildcard (deviceid 1) —
/// what libXi's `XISelectEvents(XIAllMasterDevices, …)` stores —
/// received NO barrier events, because `barrier_xi2_targets` only
/// queried the concrete master-pointer id (2). With no HIT events the
/// client could never call `XIBarrierReleasePointer`, so the pointer
/// was pinned at the wall forever. Selection under the wildcard must
/// deliver.
#[test]
fn barrier_hit_delivered_for_xiallmasterdevices_selection() {
    let mut state = ServerState::new();
    let mut backend = crate::backend::recording::RecordingBackend::new();
    let mut peer = install_client(&mut state, 1);
    // Wildcard device 1 (XIAllMasterDevices), NOT the concrete id 2.
    state
        .clients
        .get_mut(&1)
        .expect("client")
        .xi2_masks
        .insert((ROOT_WINDOW, 1), 1 << 25);
    let bid = 0x0050_0004;
    state.pointer_barriers.insert(
        bid,
        crate::server::PointerBarrier {
            owner: ClientId(1),
            window: ROOT_WINDOW,
            x1: 100,
            y1: 0,
            x2: 100,
            y2: 200,
            directions: 0,
            devices: Vec::new(),
            hit: false,
            seen: false,
            event_id: 1,
            release_event_id: 0,
            last_timestamp: 0,
        },
    );
    state.pointer_root = (90, 50);
    let mut ev = motion_event();
    ev.root_x = 110;
    ev.root_y = 50;
    let _ = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &HostXidMap::new(),
        ev,
        true,
        false,
    );
    let bytes = read_all_available(&mut peer);
    assert_eq!(
        bytes.len(),
        68,
        "BarrierHit must be delivered to the wildcard selector"
    );
    assert_eq!(&bytes[8..10], &25u16.to_le_bytes(), "BarrierHit");
    assert_eq!(&bytes[28..32], &bid.to_le_bytes(), "barrier xid");
}

#[test]
fn release_lets_pointer_cross_then_rearms() {
    let mut state = ServerState::new();
    let mut backend = crate::backend::recording::RecordingBackend::new();
    let bid = 0x0050_0003;
    state.pointer_barriers.insert(
        bid,
        crate::server::PointerBarrier {
            owner: ClientId(1),
            window: ROOT_WINDOW,
            x1: 100,
            y1: 0,
            x2: 100,
            y2: 200,
            directions: 0,
            devices: Vec::new(),
            hit: true,
            seen: false,
            event_id: 1,
            release_event_id: 1,
            last_timestamp: 10,
        },
    );
    state.pointer_root = (90, 50);
    let mut ev = motion_event();
    ev.root_x = 110;
    ev.root_y = 50;
    ev.time = 20;
    let dropped = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &HostXidMap::new(),
        ev,
        true,
        false,
    );
    assert!(dropped.is_empty());
    assert_eq!(state.pointer_root, (110, 50));
    let barrier = state.pointer_barriers.get(&bid).expect("barrier");
    assert!(!barrier.hit, "leave sweep should clear hit");
    assert_eq!(barrier.event_id, 2, "leave sweep re-arms the barrier");

    let mut ev2 = motion_event();
    ev2.root_x = 90;
    ev2.root_y = 50;
    ev2.time = 21;
    let dropped = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &HostXidMap::new(),
        ev2,
        true,
        false,
    );
    assert!(dropped.is_empty());
    assert_eq!(
        state.pointer_root,
        (100, 50),
        "re-armed barrier clamps again"
    );
}

/// wmaker wedge regression (2026-06-04, silence HW): a WM places a
/// SYNCHRONOUS `owner_events=true` button grab on a CLIENT's window
/// (click-to-focus). A press on that window's subtree must be
/// reported to the GRAB CLIENT on the grab window — per Xorg
/// `DeliverGrabbedEvent` (dix/events.c:4361), the `owner_events`
/// natural walk is filtered to the grab client: `TryClientEvents`
/// (dix/events.c:2069) returns -1 for any other client ("not
/// delivered due to grab"), aborting propagation, and the event
/// falls back to the grab window. Pre-fix, the descendant arm of
/// `target_qualifies_for_natural` leaked the press to the app
/// client while the sync grab froze the queue — the WM never saw
/// the press, never called AllowEvents, and the pointer stream
/// stayed frozen forever (cursor moves, clicks dead).
#[test]
fn passive_sync_grab_on_foreign_window_delivers_to_grab_client_and_freezes() {
    use yserver_protocol::x11::ResourceId;

    let mut state = ServerState::new();
    let grab_window = ResourceId(0x0020_0001); // app client's top-level
    let child_window = ResourceId(0x0020_0002); // GL child producing the click

    let mut wm_peer = install_client(&mut state, 1);
    let mut app_peer = install_client(&mut state, 2);

    state.resources.create_window(
        ClientId(2),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: grab_window,
            parent: crate::resources::ROOT_WINDOW,
            x: 610,
            y: 250,
            width: 1280,
            height: 800,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    state.resources.create_window(
        ClientId(2),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: child_window,
            parent: grab_window,
            x: 0,
            y: 0,
            width: 1280,
            height: 800,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(grab_window);
    let _ = state.resources.map_window(child_window);

    // The WM has NO event mask anywhere on the chain — its
    // interest is expressed solely via the grab. The app client
    // selects ButtonPress on its child (the leak target pre-fix).
    state
        .clients
        .get_mut(&2)
        .unwrap()
        .event_masks
        .insert(child_window, 0x0000_0004);

    // wmaker idiom: XGrabButton(AnyButton, AnyModifier, client_win,
    // owner_events=True, ButtonPressMask, GrabModeSync,
    // GrabModeAsync).
    state.button_grabs.push(crate::server::PassiveButtonGrab {
        device_id: 0,
        owner: ClientId(1),
        grab_window,
        button: 0,         // AnyButton
        modifiers: 0x8000, // AnyModifier
        owner_events: true,
        event_mask: 0x0000_0004, // ButtonPressMask
        pointer_mode: 0,         // GrabModeSync
        keyboard_mode: 1,
        confine_to: ResourceId(0),
        via_xi2: false,
    });

    let mut xid_map = HostXidMap::new();
    // KMS stamps the actual GL child as the producer but event_x/y are
    // root-relative.  The Steam trace had root/event=(909,299) while
    // the grabbed toplevel was at (610,250).
    xid_map.insert(0xCAFE_u32, child_window);
    let mut backend = crate::backend::recording::RecordingBackend::default();

    let _ = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &xid_map,
        HostPointerEvent {
            origin: crate::core_loop::message::InputOrigin::XTest(4),
            kind: PointerEventKind::ButtonPress,
            host_xid: 0xCAFE,
            detail: 1,
            time: 0,
            root_x: 909,
            root_y: 299,
            event_x: 909,
            event_y: 299,
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

    let wm_bytes = read_all_available(&mut wm_peer);
    assert!(
        wm_bytes.len() >= 32,
        "sync passive grab must deliver the activating press to the \
             grab client (the WM) — otherwise nobody ever AllowEvents and \
             the frozen pointer queue wedges; got {} bytes",
        wm_bytes.len(),
    );
    assert_eq!(wm_bytes[0], 4, "event type should be ButtonPress");
    assert_eq!(
        &wm_bytes[12..16],
        &grab_window.0.to_le_bytes(),
        "press must be reported on the grab window (Xorg grab-window \
             fallback — the grab client has no mask on the natural chain)",
    );
    assert_eq!(
        i16::from_le_bytes([wm_bytes[24], wm_bytes[25]]),
        299,
        "event-x must be relative to the passive grab window",
    );
    assert_eq!(
        i16::from_le_bytes([wm_bytes[26], wm_bytes[27]]),
        49,
        "event-y must be relative to the passive grab window",
    );

    let app_bytes = read_all_available(&mut app_peer);
    let app_core: Vec<&[u8]> = app_bytes.chunks(32).filter(|c| c[0] == 4).collect();
    assert!(
        app_core.is_empty(),
        "the app client must NOT see the core press while the grab \
             holds (Xorg TryClientEvents: 'not delivered due to grab'); \
             got {} core ButtonPress event(s)",
        app_core.len(),
    );

    assert!(
        state
            .xi1_frozen
            .get(&crate::xinput::DEVICEID_MASTER_POINTER)
            .and_then(|f| f.stored.as_ref())
            .is_some(),
        "GrabModeSync activation must freeze the pointer queue",
    );
    assert_eq!(
        state
            .active_pointer_grab
            .map(|grab| (grab.owner, grab.grab_window)),
        Some((ClientId(1), grab_window)),
        "passive grab must be active for client 1",
    );
}

/// icewm framed-taskbar regression (2026-07-01, air HW): icewm
/// frames its OWN taskbar as a managed client and puts a sync
/// AnyModifier `owner_events=true` button grab on the client
/// container (`YClientContainer::grabButtons`). The taskbar widgets
/// are descendants of that container and — being icewm's own
/// windows — ALSO select ButtonPress. The activating press must
/// still be reported on the GRAB WINDOW (the container), because
/// Xorg moves the sprite up to the grab window on activation
/// (`ActivatePointerGrab` → `DoEnterLeaveEvents` NotifyGrab). Only
/// then does icewm's `YClientContainer::handleButton` run and call
/// `XAllowEvents(ReplayPointer)`. Pre-fix the press leaked to the
/// deepest widget (whose handler never thaws), wedging every panel
/// click. The reported `child` is the container's child on the path
/// toward the pointer (Xorg `FindChildForEvent`).
#[test]
fn passive_sync_grab_reports_activating_press_on_grab_window_not_owned_descendant() {
    use yserver_protocol::x11::ResourceId;

    let mut state = ServerState::new();
    let container = ResourceId(0x0020_0001); // grab window (client container)
    let client_win = ResourceId(0x0020_0002); // framed taskbar client
    let widget = ResourceId(0x0020_0003); // a taskbar button

    let mut wm_peer = install_client(&mut state, 1);

    for (win, parent, x, y, w, h) in [
        (container, crate::resources::ROOT_WINDOW, 0, 0, 400, 24),
        (client_win, container, 0, 0, 400, 24),
        (widget, client_win, 10, 5, 40, 15),
    ] {
        state.resources.create_window(
            ClientId(1),
            yserver_protocol::x11::CreateWindowRequest {
                depth: 24,
                window: win,
                parent,
                x,
                y,
                width: w,
                height: h,
                border_width: 0,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = state.resources.map_window(win);
        // icewm selects ButtonPress on the container AND every widget.
        state
            .clients
            .get_mut(&1)
            .unwrap()
            .event_masks
            .insert(win, 0x0000_0004);
    }

    // icewm idiom: XGrabButton(button, AnyModifier, container,
    // owner_events=True, ButtonPressMask, GrabModeSync, GrabModeAsync).
    state.button_grabs.push(crate::server::PassiveButtonGrab {
        device_id: 0,
        owner: ClientId(1),
        grab_window: container,
        button: 1,
        modifiers: 0x8000, // AnyModifier
        owner_events: true,
        event_mask: 0x0000_0004, // ButtonPressMask
        pointer_mode: 0,         // GrabModeSync
        keyboard_mode: 1,
        confine_to: ResourceId(0),
        via_xi2: false,
    });

    let mut xid_map = HostXidMap::new();
    xid_map.insert(0xCAFE_u32, container);
    let mut backend = crate::backend::recording::RecordingBackend::default();

    // Press over the widget at (20, 8) relative to the container
    // (inside the widget, which spans x∈[10,50) y∈[5,20)).
    let _ = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &xid_map,
        HostPointerEvent {
            origin: crate::core_loop::message::InputOrigin::XTest(4),
            kind: PointerEventKind::ButtonPress,
            host_xid: 0xCAFE,
            detail: 1,
            time: 0,
            root_x: 20,
            root_y: 8,
            event_x: 20,
            event_y: 8,
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

    let wm_bytes = read_all_available(&mut wm_peer);
    assert!(wm_bytes.len() >= 32, "grab client must receive the press");
    assert_eq!(wm_bytes[0], 4, "event type should be ButtonPress");
    assert_eq!(
        &wm_bytes[12..16],
        &container.0.to_le_bytes(),
        "activating press must be reported on the GRAB WINDOW (the \
             container), not the owned descendant widget — Xorg moves the \
             sprite up to the grab window on activation",
    );
    assert_eq!(
        &wm_bytes[16..20],
        &client_win.0.to_le_bytes(),
        "child must be the grab window's child on the path toward the \
             pointer (Xorg FindChildForEvent)",
    );
    // event-x/y are relative to the grab window (container at 0,0).
    assert_eq!(
        i16::from_le_bytes([wm_bytes[24], wm_bytes[25]]),
        20,
        "event-x is relative to the grab window",
    );
    assert_eq!(
        i16::from_le_bytes([wm_bytes[26], wm_bytes[27]]),
        8,
        "event-y is relative to the grab window",
    );

    assert!(
        state
            .xi1_frozen
            .get(&crate::xinput::DEVICEID_MASTER_POINTER)
            .and_then(|f| f.stored.as_ref())
            .is_some(),
        "GrabModeSync activation must freeze the pointer queue",
    );
    assert_eq!(
        state
            .active_pointer_grab
            .map(|grab| (grab.owner, grab.grab_window)),
        Some((ClientId(1), container)),
        "passive grab must be active for client 1",
    );
}

/// CDE dtwm's front panel: a sync passive grab whose event mask holds
/// ButtonRelease only. Xorg ActivatePassiveGrab hands the activating
/// press to the grabbing client regardless of the grab's mask (measured:
/// tools/vng-scenarios/goldens/passive-grab.txt); without it dtwm never
/// AllowEvents and both devices stay frozen.
#[test]
fn passive_sync_grab_delivers_activating_press_outside_grab_mask() {
    use yserver_protocol::x11::ResourceId;

    let mut state = ServerState::new();
    let panel = ResourceId(0x0020_0001);
    let mut wm_peer = install_client(&mut state, 1);
    state.resources.create_window(
        ClientId(1),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: panel,
            parent: crate::resources::ROOT_WINDOW,
            width: 200,
            height: 200,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(panel);
    state.button_grabs.push(crate::server::PassiveButtonGrab {
        device_id: 0,
        owner: ClientId(1),
        grab_window: panel,
        button: 0,         // AnyButton
        modifiers: 0x8000, // AnyModifier
        owner_events: false,
        event_mask: 0x0000_0008, // ButtonReleaseMask
        pointer_mode: 0,         // GrabModeSync
        keyboard_mode: 0,        // GrabModeSync
        confine_to: ResourceId(0),
        via_xi2: false,
    });
    let mut xid_map = HostXidMap::new();
    xid_map.insert(0xCAFE_u32, panel);
    let mut backend = crate::backend::recording::RecordingBackend::default();

    let _ = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &xid_map,
        HostPointerEvent {
            kind: PointerEventKind::ButtonPress,
            host_xid: 0xCAFE,
            detail: 1,
            time: 0,
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
            origin: crate::core_loop::message::InputOrigin::XTest(4),
        },
        true,
        false,
    );

    let wm_bytes = read_all_available(&mut wm_peer);
    assert!(wm_bytes.len() >= 32, "grab client must receive the press");
    assert_eq!(wm_bytes[0], 4, "event type should be ButtonPress");
    assert_eq!(&wm_bytes[12..16], &panel.0.to_le_bytes());
    assert!(
        state
            .xi1_frozen
            .get(&crate::xinput::DEVICEID_MASTER_POINTER)
            .is_some_and(|f| f.stored.is_some()),
        "the press is stored for a ReplayPointer",
    );
}

/// icewm stuck-tooltip regression (2026-07-01, air HW): activating a
/// passive grab must emit the `NotifyGrab` Leave/Enter crossing chain
/// (Xorg `ActivatePointerGrab` → `DoEnterLeaveEvents(sprite → grab
/// window)`). icewm's `YWindow::handleCrossing` hides a panel tooltip
/// on ANY LeaveNotify; without the widget's grab-Leave the tooltip
/// showing on the clicked panel item never disappears (traced:
/// tooltip windows Created+Mapped, never Destroyed).
#[test]
fn passive_grab_activation_emits_notify_grab_leave_on_the_sprite_widget() {
    use yserver_protocol::x11::ResourceId;

    let mut state = ServerState::new();
    let container = ResourceId(0x0020_0001); // grab window
    let client_win = ResourceId(0x0020_0002);
    let widget = ResourceId(0x0020_0003); // the hovered panel item

    let mut wm_peer = install_client(&mut state, 1);

    for (win, parent, x, y, w, h) in [
        (container, crate::resources::ROOT_WINDOW, 0, 0, 400, 24),
        (client_win, container, 0, 0, 400, 24),
        (widget, client_win, 10, 5, 40, 15),
    ] {
        state.resources.create_window(
            ClientId(1),
            yserver_protocol::x11::CreateWindowRequest {
                depth: 24,
                window: win,
                parent,
                x,
                y,
                width: w,
                height: h,
                border_width: 0,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = state.resources.map_window(win);
        // icewm selects ButtonPress | EnterWindow | LeaveWindow.
        state
            .clients
            .get_mut(&1)
            .unwrap()
            .event_masks
            .insert(win, 0x0000_0004 | 0x0000_0010 | 0x0000_0020);
    }

    state.button_grabs.push(crate::server::PassiveButtonGrab {
        device_id: 0,
        owner: ClientId(1),
        grab_window: container,
        button: 1,
        modifiers: 0x8000,
        owner_events: true,
        event_mask: 0x0000_0004,
        pointer_mode: 0,
        keyboard_mode: 1,
        confine_to: ResourceId(0),
        via_xi2: false,
    });

    let mut xid_map = HostXidMap::new();
    xid_map.insert(0xCAFE_u32, container);
    let mut backend = crate::backend::recording::RecordingBackend::default();

    let _ = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &xid_map,
        HostPointerEvent {
            origin: crate::core_loop::message::InputOrigin::XTest(4),
            kind: PointerEventKind::ButtonPress,
            host_xid: 0xCAFE,
            detail: 1,
            time: 0,
            root_x: 20,
            root_y: 8,
            event_x: 20,
            event_y: 8,
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

    // LeaveNotify = type 8; mode byte is at offset 30; NotifyGrab = 1.
    let leave_grab_on_widget = read_all_available(&mut wm_peer)
        .chunks(32)
        .filter(|c| c.len() == 32)
        .any(|c| c[0] == 8 && c[30] == 1 && c[12..16] == widget.0.to_le_bytes());
    assert!(
        leave_grab_on_widget,
        "grab activation must deliver LeaveNotify(mode=Grab) on the \
             sprite widget so icewm dismisses its panel tooltip",
    );
}

/// icewm never-disappearing-tooltip regression (2026-07-02, air HW).
/// A NORMAL-mode crossing chain (no grab, pure hover) between two
/// sibling widgets A→B must deliver A's `LeaveNotify` on window A —
/// even though the cursor now physically sits over sibling B, so the
/// live deepest hit is B. The producer (`update_pointer_window` →
/// `normal_mode_crossings`) stamps each chain event with its own
/// window's `host_xid`; core delivery must honour that per-window
/// endpoint, not collapse every chain event onto the live deepest
/// hit. Pre-fix, `resolve_pointer_hit` re-resolved crossings to the
/// live hit (B) for BOTH the Leave and the Enter, so widget A never
/// saw a LeaveNotify with `event=A` and icewm's per-widget tooltip
/// (hidden on any LeaveNotify) orphaned. The XI2 path already
/// resolved crossings per-window; this pins the core path to match.
#[test]
fn normal_crossing_leave_delivers_on_left_sibling_not_live_hit() {
    use yserver_protocol::x11::ResourceId;

    let mut state = ServerState::new();
    let client_win = ResourceId(0x0020_0001);
    let widget_a = ResourceId(0x0020_0002); // pointer leaving this
    let widget_b = ResourceId(0x0020_0003); // pointer now over this

    let mut peer = install_client(&mut state, 1);
    let mut xi_peer = install_client(&mut state, 2);
    let mut slave_only_peer = install_client(&mut state, 3);

    for (win, parent, x, y, w, h) in [
        (client_win, crate::resources::ROOT_WINDOW, 0, 0, 400, 24),
        (widget_a, client_win, 10, 5, 40, 15), // abs (10,5)-(50,20)
        (widget_b, client_win, 60, 5, 40, 15), // abs (60,5)-(100,20)
    ] {
        state.resources.create_window(
            ClientId(1),
            yserver_protocol::x11::CreateWindowRequest {
                depth: 24,
                window: win,
                parent,
                x,
                y,
                width: w,
                height: h,
                border_width: 0,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = state.resources.map_window(win);
        // icewm selects EnterWindow | LeaveWindow on each widget.
        state
            .clients
            .get_mut(&1)
            .unwrap()
            .event_masks
            .insert(win, 0x0000_0010 | 0x0000_0020);
        // A second client selects the XI2 crossing form so the same
        // per-window coordinates and crossing detail are pinned there.
        state
            .clients
            .get_mut(&2)
            .unwrap()
            .xi2_masks
            .insert((win, 0), (1 << 7) | (1 << 8));
        // A third selects the crossings for the XTEST slave alone.
        state
            .clients
            .get_mut(&3)
            .unwrap()
            .xi2_masks
            .insert((win, 4), (1 << 7) | (1 << 8));
    }

    let mut xid_map = HostXidMap::new();
    xid_map.insert(0xA001_u32, widget_a);
    xid_map.insert(0xB001_u32, widget_b);
    let mut backend = crate::backend::recording::RecordingBackend::default();

    // Cursor now sits over widget B (root (70,10)); the producer emits
    // the Leave for A and the Enter for B at that same position.
    let _ = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &xid_map,
        HostPointerEvent {
            origin: crate::core_loop::message::InputOrigin::XTest(4),
            kind: PointerEventKind::LeaveNotify,
            host_xid: 0xA001, // the window being LEFT
            detail: 3,        // Nonlinear
            time: 1,
            root_x: 70,
            root_y: 10,
            event_x: 60, // relative to widget_a origin (10,5)
            event_y: 5,
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
    let _ = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &xid_map,
        HostPointerEvent {
            origin: crate::core_loop::message::InputOrigin::XTest(4),
            kind: PointerEventKind::EnterNotify,
            host_xid: 0xB001, // the window being ENTERED
            detail: 3,        // Nonlinear
            time: 1,
            root_x: 70,
            root_y: 10,
            event_x: 10, // relative to widget_b origin (60,5)
            event_y: 5,
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

    // LeaveNotify = type 8; event window at bytes 12..16.
    let events: Vec<[u8; 32]> = read_all_available(&mut peer)
        .chunks(32)
        .filter(|c| c.len() == 32)
        .map(|c| c.try_into().unwrap())
        .collect();
    let leave_on_a = events
        .iter()
        .any(|c| c[0] == 8 && c[12..16] == widget_a.0.to_le_bytes());
    let enter_on_b = events
        .iter()
        .any(|c| c[0] == 7 && c[12..16] == widget_b.0.to_le_bytes());
    assert!(
        leave_on_a,
        "LeaveNotify must be delivered on the window being LEFT (widget A), \
             not collapsed onto the live deepest hit (widget B); icewm hides its \
             per-widget tooltip on this Leave",
    );
    assert!(
        enter_on_b,
        "EnterNotify must be delivered on the window being ENTERED (widget B)",
    );
    let leave_on_a = events
        .iter()
        .find(|c| c[0] == 8 && c[12..16] == widget_a.0.to_le_bytes())
        .unwrap();
    assert_eq!(
        i16::from_le_bytes(leave_on_a[24..26].try_into().unwrap()),
        60
    );
    assert_eq!(
        i16::from_le_bytes(leave_on_a[26..28].try_into().unwrap()),
        5
    );
    let enter_on_b = events
        .iter()
        .find(|c| c[0] == 7 && c[12..16] == widget_b.0.to_le_bytes())
        .unwrap();
    assert_eq!(
        i16::from_le_bytes(enter_on_b[24..26].try_into().unwrap()),
        10
    );
    assert_eq!(
        i16::from_le_bytes(enter_on_b[26..28].try_into().unwrap()),
        5
    );

    let xi_bytes = read_all_available(&mut xi_peer);
    let xi_events: Vec<&[u8]> = xi_bytes.chunks(76).collect();
    // Xorg (goldens/xi2-crossing-devices.txt): an attached slave owns no
    // sprite, so XIAllDevices gets ONE Leave and ONE Enter, deviceid =
    // master pointer, sourceid = the XTEST slave that moved it.
    assert_eq!(
        xi_events.len(),
        2,
        "XIAllDevices receives only the master form of Leave + Enter"
    );
    for e in &xi_events {
        assert_eq!(
            u16::from_le_bytes(e[10..12].try_into().unwrap()),
            2,
            "deviceid"
        );
        assert_eq!(
            u16::from_le_bytes(e[16..18].try_into().unwrap()),
            4,
            "sourceid"
        );
    }
    assert!(
        read_all_available(&mut slave_only_peer).is_empty(),
        "a selection on the attached XTEST slave alone gets no crossings"
    );
    let xi_leave = xi_events
        .iter()
        .find(|e| u16::from_le_bytes(e[8..10].try_into().unwrap()) == 8)
        .unwrap();
    assert_eq!(xi_leave[19], 3, "XI2 Leave preserves NotifyNonlinear");
    assert_eq!(
        i32::from_le_bytes(xi_leave[40..44].try_into().unwrap()) >> 16,
        60
    );
    assert_eq!(
        i32::from_le_bytes(xi_leave[44..48].try_into().unwrap()) >> 16,
        5
    );
    let xi_enter = xi_events
        .iter()
        .find(|e| u16::from_le_bytes(e[8..10].try_into().unwrap()) == 7)
        .unwrap();
    assert_eq!(xi_enter[19], 3, "XI2 Enter preserves NotifyNonlinear");
    assert_eq!(
        i32::from_le_bytes(xi_enter[40..44].try_into().unwrap()) >> 16,
        10
    );
    assert_eq!(
        i32::from_le_bytes(xi_enter[44..48].try_into().unwrap()) >> 16,
        5
    );
}

/// Xorg `Xi/exevents.c:1854` runs `CheckMotion` (and so crossings) only
/// for a master or a floating slave; `dix/events.c:3244-3252` stamps the
/// slave as `sourceid`.
#[test]
fn xi2_crossings_belong_to_the_device_that_owns_the_sprite() {
    let attached = PointerXiSource {
        slave_deviceid: Some(7),
        sourceid: 7,
        attached_master: Some(XI2_MASTER_POINTER_DEVICE_ID),
    };
    let floating = PointerXiSource {
        slave_deviceid: Some(7),
        sourceid: 7,
        attached_master: None,
    };
    let master = PointerXiSource {
        slave_deviceid: None,
        sourceid: XI2_MASTER_POINTER_DEVICE_ID,
        attached_master: Some(XI2_MASTER_POINTER_DEVICE_ID),
    };
    assert!(matches!(
        xi2_crossing_form(attached),
        (Xi2PointerForm::Master, XI2_MASTER_POINTER_DEVICE_ID)
    ));
    assert!(matches!(
        xi2_crossing_form(floating),
        (Xi2PointerForm::Slave, 7)
    ));
    assert!(matches!(
        xi2_crossing_form(master),
        (Xi2PointerForm::Master, XI2_MASTER_POINTER_DEVICE_ID)
    ));
}

/// openbox resize regression: an active core grab can live on a
/// tiny hidden helper window while the WM selects motion on a
/// visible frame window containing a foreign app child. With
/// `owner_events=true`, motion over that foreign child must still
/// propagate naturally to the WM's frame window, not redirect to
/// the helper grab window.
#[test]
fn active_grab_owner_events_uses_grab_client_filtered_propagation() {
    use crate::{backend::Backend, resources::ROOT_VISUAL, server::ActivePointerGrab};

    const WM_CLIENT_ID: u32 = 1;
    const APP_CLIENT_ID: u32 = 2;
    const GRAB_WIN: u32 = 0x0010_0080;
    const FRAME_WIN: u32 = 0x0010_0081;
    const APP_CHILD_WIN: u32 = 0x0020_0082;
    const HOST_FRAME_XID: u32 = 0xCAFE_0081;

    let mut state = ServerState::new();
    let mut wm_peer = install_client(&mut state, WM_CLIENT_ID);
    let mut backend = RecordingBackend::new();
    wm_peer.set_nonblocking(true).expect("nonblocking");

    state.resources.create_window(
        ClientId(WM_CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(GRAB_WIN),
            parent: ROOT_WINDOW,
            x: -100,
            y: -100,
            width: 1,
            height: 1,
            border_width: 0,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default()
        },
    );
    state.resources.create_window(
        ClientId(WM_CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(FRAME_WIN),
            parent: ROOT_WINDOW,
            x: 500,
            y: 300,
            width: 200,
            height: 150,
            border_width: 0,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default()
        },
    );
    state.resources.create_window(
        ClientId(APP_CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(APP_CHILD_WIN),
            parent: ResourceId(FRAME_WIN),
            x: 20,
            y: 20,
            width: 100,
            height: 80,
            border_width: 0,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(GRAB_WIN));
    let _ = state.resources.map_window(ResourceId(FRAME_WIN));
    let _ = state.resources.map_window(ResourceId(APP_CHILD_WIN));

    state
        .clients
        .get_mut(&WM_CLIENT_ID)
        .expect("wm client")
        .event_masks
        .insert(ResourceId(FRAME_WIN), 0x0000_0040);

    Backend::register_top_level(&mut backend, None, ResourceId(FRAME_WIN), HOST_FRAME_XID)
        .expect("register frame host xid");

    state.active_pointer_grab = Some(ActivePointerGrab {
        owner: ClientId(WM_CLIENT_ID),
        grab_window: ResourceId(GRAB_WIN),
        event_mask: 0x0000_0040,
        cursor: ResourceId(0),
        time: 0,
        owner_events: true,
        via_xi2: false,
        implicit: false,
        passive: false,
        xi2_mask: 0,
    });

    let motion = HostPointerEvent {
        origin: crate::core_loop::message::InputOrigin::XTest(4),
        kind: PointerEventKind::MotionNotify,
        host_xid: HOST_FRAME_XID,
        detail: 0,
        time: 0x2000,
        root_x: 545,
        root_y: 345,
        event_x: 45,
        event_y: 45,
        state: 0,
        crossing_mode: 0,
        child: 0,
        raw_dx: 0,
        raw_dy: 0,
        tree_change: false,
    };
    let xid_map = backend.xid_map().clone();
    let dropped =
        pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, motion, true, false);
    assert!(dropped.is_empty());

    let mut buf = [0u8; 32];
    let n = wm_peer.read(&mut buf).expect("wm got motion");
    assert_eq!(n, 32, "expected one core MotionNotify");
    assert_eq!(buf[0], 6, "event type should be MotionNotify");
    assert_eq!(
        &buf[12..16],
        &FRAME_WIN.to_le_bytes(),
        "motion must report against the WM-selected frame window, not the hidden grab helper",
    );
    assert_eq!(
        i16::from_le_bytes([buf[24], buf[25]]),
        45,
        "event_x must stay frame-relative",
    );
    assert_eq!(
        i16::from_le_bytes([buf[26], buf[27]]),
        45,
        "event_y must stay frame-relative",
    );
}

/// Regression (#90, ImageMagick `import` on real HW): an active core
/// pointer grab that selected `ButtonMotion` but NOT `PointerMotion`
/// must not be fed no-button motion. `import` does
/// `XGrabPointer(owner_events=false,
/// ButtonPress|ButtonRelease|ButtonMotion|OwnerGrabButton)`; pre-fix
/// yserver delivered every MotionNotify (incl. state=0), so `import`
/// drew its selection rectangle from the origin before any button was
/// pressed. Xorg `DeliverGrabbedEvent` only reports events the grab's
/// mask selected — but still CAPTURES the rest (never propagates them
/// to the natural target).
#[test]
fn active_grab_buttonmotion_mask_suppresses_no_button_motion() {
    use crate::{backend::Backend, resources::ROOT_VISUAL, server::ActivePointerGrab};

    const WM_CLIENT_ID: u32 = 1;
    const APP_CLIENT_ID: u32 = 2;
    const GRAB_WIN: u32 = 0x0010_0090;
    const APP_WIN: u32 = 0x0020_0091;
    const HOST_APP_XID: u32 = 0xCAFE_0091;
    // ButtonPress | ButtonRelease | ButtonMotion (import's mask minus
    // OwnerGrabButton, which is not a delivery-selection bit). Note it
    // deliberately does NOT include PointerMotion (0x40).
    const IMPORT_MASK: u32 = 0x0000_0004 | 0x0000_0008 | 0x0000_2000;

    let mut state = ServerState::new();
    let mut wm_peer = install_client(&mut state, WM_CLIENT_ID);
    let mut app_peer = install_client(&mut state, APP_CLIENT_ID);
    let mut backend = RecordingBackend::new();

    // Tiny offscreen helper window owned by the grab client (import's
    // grab is on the root, but any grab window exercises the same
    // owner_events=false redirect path).
    state.resources.create_window(
        ClientId(WM_CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(GRAB_WIN),
            parent: ROOT_WINDOW,
            x: -100,
            y: -100,
            width: 1,
            height: 1,
            border_width: 0,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default()
        },
    );
    // A foreign app window the pointer is actually over; it selects
    // PointerMotion so we can prove the grab CAPTURES the suppressed
    // motion rather than leaking it to the natural target.
    state.resources.create_window(
        ClientId(APP_CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(APP_WIN),
            parent: ROOT_WINDOW,
            x: 400,
            y: 300,
            width: 300,
            height: 200,
            border_width: 0,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(GRAB_WIN));
    let _ = state.resources.map_window(ResourceId(APP_WIN));

    state
        .clients
        .get_mut(&APP_CLIENT_ID)
        .expect("app client")
        .event_masks
        .insert(ResourceId(APP_WIN), 0x0000_0040); // PointerMotion

    Backend::register_top_level(&mut backend, None, ResourceId(APP_WIN), HOST_APP_XID)
        .expect("register app host xid");

    state.active_pointer_grab = Some(ActivePointerGrab {
        owner: ClientId(WM_CLIENT_ID),
        grab_window: ResourceId(GRAB_WIN),
        event_mask: IMPORT_MASK as u16,
        cursor: ResourceId(0),
        time: 0,
        owner_events: false,
        via_xi2: false,
        implicit: false,
        passive: false,
        xi2_mask: 0,
    });

    let xid_map = backend.xid_map().clone();

    // (1) No-button motion (state=0): ButtonMotion mask must suppress
    //     delivery to the grab client, AND the grab must capture it so
    //     it never reaches the app's PointerMotion selection.
    let no_button_motion = HostPointerEvent {
        origin: crate::core_loop::message::InputOrigin::XTest(4),
        kind: PointerEventKind::MotionNotify,
        host_xid: HOST_APP_XID,
        detail: 0,
        time: 0x1000,
        root_x: 450,
        root_y: 350,
        event_x: 50,
        event_y: 50,
        state: 0,
        crossing_mode: 0,
        child: 0,
        raw_dx: 0,
        raw_dy: 0,
        tree_change: false,
    };
    let dropped = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &xid_map,
        no_button_motion,
        true,
        false,
    );
    assert!(dropped.is_empty());
    assert!(
        read_all_available(&mut wm_peer).is_empty(),
        "grab client selected ButtonMotion (not PointerMotion) — no-button motion must NOT be delivered",
    );
    assert!(
        read_all_available(&mut app_peer).is_empty(),
        "active grab must CAPTURE the suppressed motion, not leak it to the app's natural PointerMotion selection",
    );

    // (2) Motion with Button1 held (state=0x100): now the event
    //     carries the ButtonMotion bit → the grab client receives it,
    //     reported against the grab window (owner_events=false).
    let button_motion = HostPointerEvent {
        origin: crate::core_loop::message::InputOrigin::XTest(4),
        kind: PointerEventKind::MotionNotify,
        host_xid: HOST_APP_XID,
        detail: 0,
        time: 0x1001,
        root_x: 451,
        root_y: 351,
        event_x: 51,
        event_y: 51,
        state: 0x0100,
        crossing_mode: 0,
        child: 0,
        raw_dx: 0,
        raw_dy: 0,
        tree_change: false,
    };
    let dropped = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &xid_map,
        button_motion,
        true,
        false,
    );
    assert!(dropped.is_empty());
    let wm_bytes = read_all_available(&mut wm_peer);
    assert_eq!(wm_bytes.len(), 32, "expected exactly one core MotionNotify");
    assert_eq!(wm_bytes[0], 6, "event type should be MotionNotify");
    assert_eq!(
        &wm_bytes[12..16],
        &GRAB_WIN.to_le_bytes(),
        "owner_events=false: motion reported against the grab window",
    );
    assert!(
        read_all_available(&mut app_peer).is_empty(),
        "button-held motion is still captured by the grab, not sent to the app",
    );

    // (3) ButtonPress (mask includes ButtonPress) is still delivered.
    let press = HostPointerEvent {
        origin: crate::core_loop::message::InputOrigin::XTest(4),
        kind: PointerEventKind::ButtonPress,
        host_xid: HOST_APP_XID,
        detail: 1,
        time: 0x1002,
        root_x: 451,
        root_y: 351,
        event_x: 51,
        event_y: 51,
        state: 0,
        crossing_mode: 0,
        child: 0,
        raw_dx: 0,
        raw_dy: 0,
        tree_change: false,
    };
    let dropped =
        pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, press, true, false);
    assert!(dropped.is_empty());
    let wm_bytes = read_all_available(&mut wm_peer);
    assert_eq!(wm_bytes.len(), 32, "expected exactly one core ButtonPress");
    assert_eq!(wm_bytes[0], 4, "event type should be ButtonPress");
    assert_eq!(
        &wm_bytes[12..16],
        &GRAB_WIN.to_le_bytes(),
        "owner_events=false: press reported against the grab window",
    );
}

#[test]
fn pointer_event_resets_dpms_last_activity() {
    use std::time::{Duration, Instant};
    let mut state = ServerState::new();
    state.dpms.kms_capable = true;
    state.dpms.enabled = true;
    state.dpms.last_activity = Instant::now() - Duration::from_secs(10);
    let stale = state.dpms.last_activity;
    let xid_map = HostXidMap::new();
    let mut backend = crate::backend::recording::RecordingBackend::default();

    let _ = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &xid_map,
        motion_event(),
        true,
        false,
    );

    let elapsed = state.dpms.last_activity.duration_since(stale);
    assert!(
        elapsed > Duration::from_secs(9),
        "last_activity should be ≈now, not stale"
    );
}

#[test]
fn pointer_event_during_off_wakes_via_set_dpms_power_on() {
    let mut state = ServerState::new();
    state.dpms.kms_capable = true;
    state.dpms.enabled = true;
    state.dpms.power_level = 3; // Off
    let xid_map = HostXidMap::new();
    let mut backend = crate::backend::recording::RecordingBackend::default();

    let _ = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &xid_map,
        motion_event(),
        true,
        false,
    );

    let calls = backend.calls.lock().unwrap().clone();
    assert!(
        calls
            .iter()
            .any(|c| matches!(c, crate::backend::recording::RecordedCall::SetDpmsPower(0))),
        "wake must call set_dpms_power(0); got {calls:?}"
    );
    assert_eq!(
        state.dpms.power_level, 0,
        "in-memory level should be On after wake"
    );
}

#[test]
fn pointer_event_during_off_with_backend_error_still_advances_state() {
    let mut state = ServerState::new();
    state.dpms.kms_capable = true;
    state.dpms.enabled = true;
    state.dpms.power_level = 3;
    let xid_map = HostXidMap::new();
    let mut backend = crate::backend::recording::RecordingBackend::default();
    backend.dpms_set_returns_err = true;

    let _ = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &xid_map,
        motion_event(),
        true,
        false,
    );

    assert_eq!(
        state.dpms.power_level, 0,
        "state must advance on backend error"
    );
}

#[test]
fn pointer_event_during_screen_saver_on_flips_off_via_independent_path() {
    // Standalone SS-On (DPMS still On). Motion event must flip
    // SS Off with forced=0.
    let mut state = ServerState::new();
    state.dpms.kms_capable = true;
    state.dpms.enabled = true;
    // dpms.power_level already 0 from new()
    state.screensaver.active = ScreenSaverActive::On;
    state.screensaver.selected_by.insert(ClientId(1), 0x01);
    let xid_map = HostXidMap::new();
    let mut backend = crate::backend::recording::RecordingBackend::default();

    let _ = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &xid_map,
        motion_event(),
        true,
        false,
    );

    assert_eq!(state.screensaver.active, ScreenSaverActive::Off);
    assert!(!state.screensaver.forced, "input-driven Off is non-forced");
}

#[test]
fn pointer_event_updates_global_and_per_device_vcp_last_activity() {
    use std::time::Duration;
    let mut state = ServerState::new();
    state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(30);
    let stale = state.dpms.last_activity;
    let xid_map = HostXidMap::new();
    let mut backend = crate::backend::recording::RecordingBackend::default();

    let _ = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &xid_map,
        motion_event(),
        true,
        false,
    );

    assert!(
        state.dpms.last_activity > stale,
        "global last_activity advanced"
    );
    let vcp = state
        .per_device_last_activity
        .get(&2)
        .copied()
        .expect("VCP per-device entry inserted");
    assert!(vcp > stale, "VCP per-device last_activity advanced");
}

#[test]
fn pointer_event_fires_neg_transition_alarm_when_prior_idle_crosses_threshold() {
    use std::time::Duration;
    use yserver_protocol::x11::sync as x11sync;
    let mut state = ServerState::new();
    // User idle for 90s, NegativeTransition alarm at 60s.
    state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(90);
    state
        .per_device_last_activity
        .insert(2, std::time::Instant::now() - Duration::from_secs(90));
    let alarm_id = 0x4000;
    state.sync_alarms.insert(
        alarm_id,
        crate::server::SyncAlarm {
            owner: ClientId(1),
            counter: x11sync::IDLETIME_DEVICE_VCP,
            wait_value: 60_000,
            delta: 0,
            test_type: x11sync::TEST_NEGATIVE_TRANSITION,
            events: false,
            state: x11sync::ALARM_STATE_ACTIVE,
            event_clients: Vec::new(),
            value_type: 0,
            raw_wait: 60_000,
            check_type: x11sync::TEST_NEGATIVE_TRANSITION,
        },
    );
    let xid_map = HostXidMap::new();
    let mut backend = crate::backend::recording::RecordingBackend::default();

    let _ = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &xid_map,
        motion_event(),
        true,
        false,
    );

    // Alarm stays Active (Transition + delta=0 — Task 2 fix); cache reflects post-wake idle=0.
    assert_eq!(
        state.sync_alarms[&alarm_id].state,
        x11sync::ALARM_STATE_ACTIVE
    );
    assert_eq!(
        state
            .idletime_last_evaluated
            .get(&x11sync::IDLETIME_DEVICE_VCP)
            .copied(),
        Some(0),
        "post-wake last_evaluated should be 0"
    );
}

#[test]
fn pointer_event_fires_neg_transition_alarm_on_per_device_idletime_vcp() {
    use std::time::Duration;
    use yserver_protocol::x11::sync as x11sync;
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(90);
    state
        .per_device_last_activity
        .insert(2, std::time::Instant::now() - Duration::from_secs(90));
    let alarm_id = 0x5000;
    state.sync_alarms.insert(
        alarm_id,
        crate::server::SyncAlarm {
            owner: ClientId(1),
            counter: x11sync::IDLETIME_DEVICE_VCP,
            wait_value: 60_000,
            delta: 0,
            test_type: x11sync::TEST_NEGATIVE_TRANSITION,
            events: true, // load-bearing
            state: x11sync::ALARM_STATE_ACTIVE,
            event_clients: Vec::new(),
            value_type: 0,
            raw_wait: 60_000,
            check_type: x11sync::TEST_NEGATIVE_TRANSITION,
        },
    );
    let xid_map = HostXidMap::new();
    let mut backend = crate::backend::recording::RecordingBackend::default();

    let _ = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &xid_map,
        motion_event(),
        true,
        false,
    );

    // PRIMARY: AlarmNotify event type 84.
    let bytes = read_all_available(&mut peer);
    // AlarmNotify is a 32-byte sequential event; type byte at offset 0.
    assert!(
        bytes.len() >= 32,
        "expected AlarmNotify event (32B); got {} bytes",
        bytes.len()
    );
    assert_eq!(
        bytes[0], 84,
        "AlarmNotify event type (SYNC_FIRST_EVENT + 1)"
    );
    assert_eq!(bytes[1], 1, "AlarmNotify kind = AlarmNotify (1)");
    assert_eq!(
        state.sync_alarms[&alarm_id].state,
        x11sync::ALARM_STATE_ACTIVE
    );
    assert_eq!(
        state
            .idletime_last_evaluated
            .get(&x11sync::IDLETIME_DEVICE_VCP)
            .copied(),
        Some(0)
    );
}

/// Regression (e27 clicks dead): the device classification that gates
/// both the core/XI2 dedup and the XI2 `deviceid` stamp. A client
/// selecting on the SLAVE pointer must classify as slave (so the dedup
/// leaves its CORE ButtonPress intact — Enlightenment needs the core
/// click); a client selecting the MASTER, `XIAllMasterDevices`, or
/// `XIAllDevices` must classify as master (so it is still deduped,
/// preserving Chromium's no-double-ButtonPress fix). HW-confirmed on
/// both e27 (clicks work) and Chrome (no double-click) 2026-06-25.
#[test]
fn xi2_stamp_deviceid_classifies_slave_vs_master_selectors() {
    let mut state = ServerState::new();
    let _p1 = install_client(&mut state, 1);
    let _p2 = install_client(&mut state, 2);
    let _p3 = install_client(&mut state, 3);
    let _p4 = install_client(&mut state, 4);
    let win = ResourceId(0x10_0009);
    const XI_BUTTON_PRESS_MASK: u32 = 1 << 4;
    // client 1: slave-pointer selection (Enlightenment's pattern).
    state.clients.get_mut(&1).unwrap().xi2_masks.insert(
        (win, XI2_XTEST_POINTER_DEVICE_ID),
        u64::from(XI_BUTTON_PRESS_MASK),
    );
    // client 2: master-pointer selection (Chromium's pattern).
    state.clients.get_mut(&2).unwrap().xi2_masks.insert(
        (win, XI2_MASTER_POINTER_DEVICE_ID),
        u64::from(XI_BUTTON_PRESS_MASK),
    );
    // client 3: XIAllMasterDevices (1) wildcard.
    state
        .clients
        .get_mut(&3)
        .unwrap()
        .xi2_masks
        .insert((win, 1), u64::from(XI_BUTTON_PRESS_MASK));
    // client 4: no XI2 selection at all.

    assert_eq!(
        xi2_stamp_deviceid_for_source(&state, ClientId(1), win, win, 4, Some(4)),
        XI2_XTEST_POINTER_DEVICE_ID,
        "slave-device selector must stay slave (keeps core delivery)"
    );
    assert_eq!(
        xi2_stamp_deviceid_for_source(&state, ClientId(2), win, win, 4, Some(4)),
        XI2_MASTER_POINTER_DEVICE_ID,
        "master-device selector must be master (stays deduped)"
    );
    assert_eq!(
        xi2_stamp_deviceid_for_source(&state, ClientId(3), win, win, 4, Some(4)),
        XI2_MASTER_POINTER_DEVICE_ID,
        "XIAllMasterDevices selector classifies as master"
    );
    assert_eq!(
        xi2_stamp_deviceid_for_source(&state, ClientId(4), win, win, 4, Some(4)),
        XI2_MASTER_POINTER_DEVICE_ID,
        "no XI2 selection defaults to master"
    );
}

/// Issue #72: lite-xl (SDL2) got NO mouse interactions because it
/// splits its XI2 selection across device-id wildcards on the SAME
/// window — Motion/Touch/Gesture under `XIAllMasterDevices(1)` but
/// ButtonPress/Release under `XIAllDevices(0)`. The old first-match
/// `xi2_mask_for_client` returned only the device-1 mask (no button
/// bits) and never OR'd in device-0, so every XI_ButtonPress/Release
/// was delivered to NOBODY (`xi2_targets=[]`) — no clicks, no scroll,
/// no scrollbar drag. The masks below are the real values from the
/// lite-xl HW log.
#[test]
fn xi2_split_device_wildcards_deliver_button_press_issue_72() {
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 11);
    let window = ResourceId(0x0080_0037);
    // XIAllMasterDevices(1): Motion(6)+Enter(7)+Touch(18/19/20)+
    // Gesture(27/28/29) — NO button bits.
    state
        .clients
        .get_mut(&11)
        .unwrap()
        .xi2_masks
        .insert((window, 1u16), 0x381c_00c0u64);
    // XIAllDevices(0): DeviceChanged(1)+ButtonPress(4)+ButtonRelease(5)+
    // Motion(6)+Enter(7)+Leave(8)+Hierarchy(11)+Property(12).
    state
        .clients
        .get_mut(&11)
        .unwrap()
        .xi2_masks
        .insert((window, 0u16), 0x19f2u64);

    // XI_ButtonPress (4) is selected ONLY under XIAllDevices(0). Under
    // the old first-match semantics device 1 matched first and had no
    // button bit -> empty targets -> the click reached no client.
    let press_targets = compute_xi2_targets_for_source(&state, window, window, 4, Some(4));
    assert!(
        press_targets.contains(&ClientId(11)),
        "issue #72: XI_ButtonPress selected under XIAllDevices(0) must be \
             delivered even when motion is selected under XIAllMasterDevices(1)"
    );

    // XI_ButtonRelease (5) is likewise only under XIAllDevices(0).
    let release_targets = compute_xi2_targets_for_source(&state, window, window, 5, Some(4));
    assert!(
        release_targets.contains(&ClientId(11)),
        "issue #72: XI_ButtonRelease under XIAllDevices(0) must be delivered"
    );

    // Sanity: XI_Motion (6) — selected under BOTH wildcards — still
    // reaches the client (this arm passed even under first-match).
    let motion_targets = compute_xi2_targets_for_source(&state, window, window, 6, Some(4));
    assert!(
        motion_targets.contains(&ClientId(11)),
        "XI_Motion must continue to be delivered"
    );
}

/// Regression for issue #72 at the decision point: the per-form
/// membership `xi2_pointer_forms` computes. A client selecting under
/// `XIAllDevices(0)` (the SDL3/lite-xl idiom) MUST land in BOTH the
/// master- and slave-stamped delivery buckets. SDL3 parses smooth
/// scroll ONLY off the slave-stamped motion (its handler gates on
/// `deviceid == sourceid`); the pre-fix single-form (master-only /
/// first-match) bucketing returned `wants_slave == false`, leaving
/// lite-xl unable to scroll.
#[test]
fn xi2_pointer_forms_all_devices_selector_gets_both_forms_issue_72() {
    const MOTION: u16 = 6;
    const BUTTON_PRESS: u16 = 4;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 11);
    let win = ResourceId(0x0080_0037);

    let set_masks = |state: &mut ServerState, masks: &[(u16, u32)]| {
        let client = state.clients.get_mut(&11).unwrap();
        client.xi2_masks.clear();
        for &(dev, mask) in masks {
            client.xi2_masks.insert((win, dev), u64::from(mask));
        }
    };

    // Case 1 — real lite-xl/SDL3 masks: XIAllMasterDevices(1) carries
    // Motion (no button bits); XIAllDevices(0) carries ButtonPress+
    // Motion. Both evtypes MUST yield BOTH forms so SDL3 sees the
    // slave-stamped scroll motion. `wants_slave == true` here is the
    // core issue-#72 fix: it was false under master-only bucketing.
    set_masks(&mut state, &[(1u16, 0x381c_00c0u32), (0u16, 0x19f2u32)]);
    assert_eq!(
        xi2_pointer_forms_for_source(&state, ClientId(11), win, win, MOTION, Some(4)),
        (true, true),
        "issue #72: an XIAllDevices(0) selector must get BOTH forms for \
             Motion so SDL3 can parse smooth scroll off the slave-stamped motion"
    );
    assert_eq!(
        xi2_pointer_forms_for_source(&state, ClientId(11), win, win, BUTTON_PRESS, Some(4)),
        (true, true),
        "ButtonPress selected under XIAllDevices(0) must also yield BOTH forms"
    );

    // Case 2 — XIAllMasterDevices(1) only, Motion bit set: device 1
    // covers master devices only, so master form yes, slave form no.
    set_masks(&mut state, &[(1u16, 0x0000_0040u32)]);
    assert_eq!(
        xi2_pointer_forms_for_source(&state, ClientId(11), win, win, MOTION, Some(4)),
        (true, false),
        "XIAllMasterDevices(1) covers master devices only: master form, NO slave form"
    );

    // Case 3 — concrete slave pointer only (Enlightenment's pattern):
    // slave form yes, master form no; preserves core delivery.
    set_masks(&mut state, &[(XI2_XTEST_POINTER_DEVICE_ID, 0x0000_0040u32)]);
    assert_eq!(
        xi2_pointer_forms_for_source(&state, ClientId(11), win, win, MOTION, Some(4)),
        (false, true),
        "a concrete slave-pointer selector gets the slave form only"
    );

    // Case 4 — XIAllDevices(0) only: device 0 matches both master and
    // slave, so both forms.
    set_masks(&mut state, &[(0u16, 0x0000_0040u32)]);
    assert_eq!(
        xi2_pointer_forms_for_source(&state, ClientId(11), win, win, MOTION, Some(4)),
        (true, true),
        "XIAllDevices(0) matches both master and slave: BOTH forms"
    );
}

/// X11 implicit pointer grab (#94, Xorg dix/events.c:2150-2193 +
/// 2415-2421): a delivered ButtonPress activates an async grab owned
/// by the press recipient on the press's event window; the matching
/// ButtonRelease is delivered under that grab even when the hit-test
/// now resolves to another client's window (muffin mutates the tree
/// between press and release — Steam got presses without releases).
#[test]
fn implicit_grab_core_release_follows_press_recipient() {
    use yserver_protocol::x11::{CreateWindowRequest, ResourceId};
    const APP: u32 = 1;
    const OTHER: u32 = 2;
    let win_a = ResourceId(0x0010_0001);
    let win_b = ResourceId(0x0020_0001);
    const HOST_A: u32 = 0xCAFE_0001;
    const HOST_B: u32 = 0xCAFE_0002;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::default();
    let mut app_peer = install_client(&mut state, APP);
    let mut other_peer = install_client(&mut state, OTHER);

    for (client, win, x) in [(APP, win_a, 0i16), (OTHER, win_b, 500i16)] {
        state.resources.create_window(
            ClientId(client),
            CreateWindowRequest {
                depth: 24,
                window: win,
                parent: ROOT_WINDOW,
                x,
                y: 0,
                width: 100,
                height: 100,
                border_width: 0,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = state.resources.map_window(win);
    }
    // ButtonPress|ButtonRelease.
    for (client, win) in [(APP, win_a), (OTHER, win_b)] {
        state
            .clients
            .get_mut(&client)
            .unwrap()
            .event_masks
            .insert(win, 0x0000_000c);
    }
    let mut xid_map = HostXidMap::new();
    xid_map.insert(HOST_A, win_a);
    xid_map.insert(HOST_B, win_b);

    let mut press = motion_event();
    press.kind = PointerEventKind::ButtonPress;
    press.host_xid = HOST_A;
    press.detail = 1;
    press.time = 1000;
    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, press, true, false);
    assert!(
        state
            .active_pointer_grab
            .is_some_and(|g| g.implicit && g.owner == ClientId(APP) && g.grab_window == win_a),
        "delivered press must install the implicit grab (Xorg dix/events.c:2415)"
    );
    let _ = read_all_available(&mut app_peer); // drain the press

    // Release resolves over OTHER's window (the WM-mutated-tree shape).
    let mut release = motion_event();
    release.kind = PointerEventKind::ButtonRelease;
    release.host_xid = HOST_B;
    release.detail = 1;
    release.time = 1010;
    release.root_x = 550;
    release.root_y = 10;
    release.event_x = 50;
    release.event_y = 10;
    release.state = 0x100;
    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, release, true, false);

    let bytes = read_all_available(&mut app_peer);
    let mut saw_release = false;
    let mut off = 0usize;
    while off + 32 <= bytes.len() {
        if bytes[off] & 0x7F == 5 {
            saw_release = true;
            assert_eq!(
                &bytes[off + 12..off + 16],
                &win_a.0.to_le_bytes(),
                "grabbed release must be reported on the grab (press) window"
            );
        }
        off += 32;
    }
    assert!(
        saw_release,
        "implicit grab: the release must follow the press recipient, not re-hit-test"
    );
    let other_bytes = read_all_available(&mut other_peer);
    assert!(
        !other_bytes.chunks(32).any(|c| c[0] & 0x7F == 5),
        "the grab captures the release — OTHER must not receive it"
    );
    assert!(
        state.active_pointer_grab.is_none(),
        "final release tears the implicit grab down (Xi/exevents.c:1931)"
    );
}

/// XI2 form of the implicit grab (#94 — the actual Steam/Cinnamon
/// shape: Steam selects cooked XI2 buttons, no core mask). The XI2
/// ButtonPress installs a via_xi2 implicit grab; the release delivers
/// to the owner on the grab window through the XI2 redirect.
#[test]
fn implicit_grab_xi2_release_follows_press_recipient() {
    use yserver_protocol::x11::{CreateWindowRequest, ResourceId};
    const APP: u32 = 1;
    const OTHER: u32 = 2;
    let win_a = ResourceId(0x0010_0001);
    let win_b = ResourceId(0x0020_0001);
    const HOST_A: u32 = 0xCAFE_0001;
    const HOST_B: u32 = 0xCAFE_0002;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::default();
    let mut app_peer = install_client(&mut state, APP);
    let _other_peer = install_client(&mut state, OTHER);

    for (client, win, x) in [(APP, win_a, 0i16), (OTHER, win_b, 500i16)] {
        state.resources.create_window(
            ClientId(client),
            CreateWindowRequest {
                depth: 24,
                window: win,
                parent: ROOT_WINDOW,
                x,
                y: 0,
                width: 100,
                height: 100,
                border_width: 0,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = state.resources.map_window(win);
    }
    // XI_ButtonPress(4) | XI_ButtonRelease(5) on the master pointer.
    state
        .clients
        .get_mut(&APP)
        .unwrap()
        .xi2_masks
        .insert((win_a, XI2_MASTER_POINTER_DEVICE_ID), (1 << 4) | (1 << 5));
    state
        .clients
        .get_mut(&OTHER)
        .unwrap()
        .xi2_masks
        .insert((win_b, XI2_MASTER_POINTER_DEVICE_ID), (1 << 4) | (1 << 5));
    let mut xid_map = HostXidMap::new();
    xid_map.insert(HOST_A, win_a);
    xid_map.insert(HOST_B, win_b);

    let mut press = motion_event();
    press.kind = PointerEventKind::ButtonPress;
    press.host_xid = HOST_A;
    press.detail = 1;
    press.time = 1000;
    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, press, true, false);
    assert!(
        state
            .active_pointer_grab
            .is_some_and(|g| g.implicit && g.via_xi2 && g.owner == ClientId(APP)),
        "XI2-delivered press must install a via_xi2 implicit grab"
    );
    let _ = read_all_available(&mut app_peer);

    let mut release = motion_event();
    release.kind = PointerEventKind::ButtonRelease;
    release.host_xid = HOST_B;
    release.detail = 1;
    release.time = 1010;
    release.root_x = 550;
    release.root_y = 10;
    release.event_x = 50;
    release.event_y = 10;
    release.state = 0x100;
    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, release, true, false);

    let bytes = read_all_available(&mut app_peer);
    let mut found_win = None;
    let mut off = 0usize;
    while off + 32 <= bytes.len() {
        if bytes[off] == 35 && u16::from_le_bytes([bytes[off + 8], bytes[off + 9]]) == 5 {
            found_win = Some(u32::from_le_bytes(
                bytes[off + 24..off + 28].try_into().unwrap(),
            ));
            break;
        }
        let length = u32::from_le_bytes(bytes[off + 4..off + 8].try_into().unwrap()) as usize;
        off += 32 + length * 4;
    }
    assert_eq!(
        found_win,
        Some(win_a.0),
        "XI2 release must reach the implicit-grab owner on the grab window"
    );
    assert!(
        state.active_pointer_grab.is_none(),
        "grab released after final release"
    );
}

/// Xorg oracle from the FAILING `xfce.xtrace` vs `xfce-xorg.xtrace`: an
/// XFCE dialog selects XI2 buttons under `device=1` (XIAllMasterDevices,
/// not the master id 2), gets its ButtonPress (delivered as master 0x02),
/// but yserver dropped EVERY XI2 ButtonRelease (xtrace: 13 XI2 presses, 0
/// XI2 releases; Xorg delivered 3/3). The dialog "reacts to the click but
/// it never takes effect". Every PASSING implicit-grab test selects under
/// device 2, so this pins the device=1 (all-master) selector: press AND
/// release must both reach the selector on its window.
#[test]
fn implicit_grab_xi2_release_reaches_all_master_selector_xfce() {
    use yserver_protocol::x11::{CreateWindowRequest, ResourceId};
    const APP: u32 = 1;
    const XI2_ALL_MASTER: u16 = 1; // XIAllMasterDevices
    let win_a = ResourceId(0x0050_0003);
    const HOST_A: u32 = 0xCAFE_0001;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::default();
    let mut app_peer = install_client(&mut state, APP);

    state.resources.create_window(
        ClientId(APP),
        CreateWindowRequest {
            depth: 24,
            window: win_a,
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
    let _ = state.resources.map_window(win_a);
    // XI_ButtonPress(4) | XI_ButtonRelease(5) selected under XIAllMasterDevices.
    state
        .clients
        .get_mut(&APP)
        .unwrap()
        .xi2_masks
        .insert((win_a, XI2_ALL_MASTER), (1 << 4) | (1 << 5));
    let mut xid_map = HostXidMap::new();
    xid_map.insert(HOST_A, win_a);

    let mut press = motion_event();
    press.kind = PointerEventKind::ButtonPress;
    press.host_xid = HOST_A;
    press.detail = 1;
    press.time = 1000;
    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, press, true, false);

    let xge_evtypes = |bytes: &[u8]| -> Vec<u16> {
        let mut found = Vec::new();
        let mut off = 0usize;
        while off + 32 <= bytes.len() {
            let advance = if bytes[off] == 35 {
                found.push(u16::from_le_bytes([bytes[off + 8], bytes[off + 9]]));
                32 + u32::from_le_bytes(bytes[off + 4..off + 8].try_into().unwrap()) as usize * 4
            } else {
                32
            };
            off += advance;
        }
        found
    };
    assert!(
        xge_evtypes(&read_all_available(&mut app_peer)).contains(&4),
        "precondition: the all-master XI2 selector receives the ButtonPress"
    );

    let mut release = motion_event();
    release.kind = PointerEventKind::ButtonRelease;
    release.host_xid = HOST_A;
    release.detail = 1;
    release.time = 1010;
    release.state = 0x100;
    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, release, true, false);

    assert!(
        xge_evtypes(&read_all_available(&mut app_peer)).contains(&5),
        "XI2 ButtonRelease must reach the XIAllMasterDevices selector on its \
             window (xfce.xtrace dropped it: 0 XI2 releases vs Xorg 3/3)"
    );
}

/// #94 XFCE dialog repro (fail `xfce.xtrace` / pass `xfce-xorg.xtrace`).
/// The real shape: a CORE selector on an ANCESTOR window (xtrace conn 005,
/// event=0x00300a58) plus the XI2 dialog on a leaf child (conn 014,
/// event=0x00500003). The deeper XI2 delivery owns the implicit grab, so
/// the release follows that XI2 grab rather than propagating naturally to
/// the core ancestor.
#[test]
fn implicit_grab_release_reaches_xi2_leaf_with_core_ancestor_selector_xfce() {
    use yserver_protocol::x11::{CreateWindowRequest, ResourceId};
    const CORE_ANCESTOR: u32 = 1; // xtrace conn 005 (core selector on ancestor)
    const XI2_LEAF: u32 = 2; // xtrace conn 014 (XI2 dialog)
    let parent = ResourceId(0x0030_0a58);
    let leaf = ResourceId(0x0050_0003);
    const HOST_LEAF: u32 = 0xCAFE_0001;
    const XI2_ALL_MASTER: u16 = 1;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::default();
    let mut core_peer = install_client(&mut state, CORE_ANCESTOR);
    let mut leaf_peer = install_client(&mut state, XI2_LEAF);

    // parent (core selector) ← leaf (XI2 selector), leaf covers the hit.
    state.resources.create_window(
        ClientId(CORE_ANCESTOR),
        CreateWindowRequest {
            depth: 24,
            window: parent,
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 200,
            height: 200,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(parent);
    state.resources.create_window(
        ClientId(XI2_LEAF),
        CreateWindowRequest {
            depth: 24,
            window: leaf,
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
    let _ = state.resources.map_window(leaf);
    // Ancestor: CORE ButtonPress|ButtonRelease (0x0c). Leaf: XI2 under
    // XIAllMasterDevices.
    state
        .clients
        .get_mut(&CORE_ANCESTOR)
        .unwrap()
        .event_masks
        .insert(parent, 0x0000_000c);
    state
        .clients
        .get_mut(&XI2_LEAF)
        .unwrap()
        .xi2_masks
        .insert((leaf, XI2_ALL_MASTER), (1 << 4) | (1 << 5));
    let mut xid_map = HostXidMap::new();
    xid_map.insert(HOST_LEAF, leaf);

    let mut press = motion_event();
    press.kind = PointerEventKind::ButtonPress;
    press.host_xid = HOST_LEAF;
    press.detail = 1;
    press.time = 1000;
    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, press, true, false);
    let _ = read_all_available(&mut core_peer);
    let leaf_after_press = read_all_available(&mut leaf_peer);

    let xge_evtypes = |bytes: &[u8]| -> Vec<u16> {
        let mut found = Vec::new();
        let mut off = 0usize;
        while off + 32 <= bytes.len() {
            let advance = if bytes[off] == 35 {
                found.push(u16::from_le_bytes([bytes[off + 8], bytes[off + 9]]));
                32 + u32::from_le_bytes(bytes[off + 4..off + 8].try_into().unwrap()) as usize * 4
            } else {
                32
            };
            off += advance;
        }
        found
    };
    assert!(
        xge_evtypes(&leaf_after_press).contains(&4),
        "precondition: the XI2 leaf selector receives the ButtonPress"
    );

    let mut release = motion_event();
    release.kind = PointerEventKind::ButtonRelease;
    release.host_xid = HOST_LEAF;
    release.detail = 1;
    release.time = 1010;
    release.state = 0x100;
    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, release, true, false);

    let core_after = read_all_available(&mut core_peer);
    let leaf_after = read_all_available(&mut leaf_peer);
    assert!(
        !core_after.chunks(32).any(|c| c.first() == Some(&5)),
        "the XI2 implicit grab captures the release before core ancestor propagation"
    );
    assert!(
        xge_evtypes(&leaf_after).contains(&5),
        "XI2 ButtonRelease must reach the XI2 leaf selector even though a \
             core client selects on an ancestor (xfce.xtrace: leaf got the \
             press but the release was dropped)"
    );
}

/// Enlightenment (E27) dock-click repro, hardware-measured on silence.
/// One client selects BOTH core ButtonPress|ButtonRelease AND XI2 on the
/// SLAVE pointer for the same window; the core dedup keeps the core form
/// for slave stamps, so the press is delivered twice — once per protocol.
///
/// The implicit grab that press installs must be typed CORE, not XI2:
/// Xorg `DeliverEventsToWindow` delivers core first and activates the
/// grab from that call, so `ActivateImplicitGrab` sees `ButtonPress` and
/// picks `grabtype = CORE` (dix/events.c:2158), which its own comment at
/// dix/events.c:2417 spells out — "since core events are delivered first,
/// an implicit grab may be activated on a core grab, stopping the XI
/// events."
///
/// Typing it XI2 set `via_xi2`, and the active-grab redirect then
/// suppressed the CORE form of the release while still marking the event
/// handled — captured and dropped, with natural propagation skipped too.
/// E27 lost the mouse-up on every dock click: the button stayed down
/// client-side, so clicks became drags and then input wedged entirely.
///
/// Oracle is measured, not assumed — silence, same session, same clicks:
///   yserver before  core 10/2   XI2 9/8
///   yserver after   core 28/28  XI2 28/28
///   Xorg baseline   core 3/3    XI2 3/3
/// Core press and release balance 1:1 on Xorg, so the release must land.
#[test]
fn implicit_grab_core_release_survives_dual_core_and_slave_xi2_selection_e27() {
    use yserver_protocol::x11::{CreateWindowRequest, ResourceId};
    const APP: u32 = 1;
    let canvas = ResourceId(0x0010_0009); // E27 canvas, from the trace
    const HOST_CANVAS: u32 = 0x0040_0dbb;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::default();
    let mut app_peer = install_client(&mut state, APP);

    state.resources.create_window(
        ClientId(APP),
        CreateWindowRequest {
            depth: 24,
            window: canvas,
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 200,
            height: 200,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(canvas);
    // Core ButtonPress|ButtonRelease (0x0c) AND XI2 press/release on the
    // SLAVE pointer — both on the same window, the E27 shape.
    state
        .clients
        .get_mut(&APP)
        .unwrap()
        .event_masks
        .insert(canvas, 0x0000_000c);
    state
        .clients
        .get_mut(&APP)
        .unwrap()
        .xi2_masks
        .insert((canvas, XI2_XTEST_POINTER_DEVICE_ID), (1 << 4) | (1 << 5));
    let mut xid_map = HostXidMap::new();
    xid_map.insert(HOST_CANVAS, canvas);

    // Walk the wire correctly: XGE (type 35) carries a length in words,
    // so a flat chunks(32) misaligns once both forms share a stream.
    let split_events = |bytes: &[u8]| -> (Vec<u8>, Vec<u16>) {
        let (mut core, mut xge) = (Vec::new(), Vec::new());
        let mut off = 0usize;
        while off + 32 <= bytes.len() {
            if bytes[off] == 35 {
                xge.push(u16::from_le_bytes([bytes[off + 8], bytes[off + 9]]));
                off += 32
                    + u32::from_le_bytes(bytes[off + 4..off + 8].try_into().unwrap()) as usize * 4;
            } else {
                core.push(bytes[off] & 0x7f);
                off += 32;
            }
        }
        (core, xge)
    };

    let mut press = motion_event();
    press.kind = PointerEventKind::ButtonPress;
    press.host_xid = HOST_CANVAS;
    press.detail = 1;
    press.time = 1000;
    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, press, true, false);

    let (core_press, xge_press) = split_events(&read_all_available(&mut app_peer));
    assert!(
        core_press.contains(&4),
        "precondition: the core selector receives the ButtonPress"
    );
    assert!(
        xge_press.contains(&4),
        "precondition: the slave XI2 selector also receives the press \
             (the slave carve-out in the core dedup keeps both forms)"
    );
    assert!(
        state
            .active_pointer_grab
            .is_some_and(|g| g.implicit && !g.via_xi2),
        "the implicit grab must be typed CORE when both forms were \
             delivered to the same window (Xorg ActivateImplicitGrab)"
    );

    let mut release = motion_event();
    release.kind = PointerEventKind::ButtonRelease;
    release.host_xid = HOST_CANVAS;
    release.detail = 1;
    release.time = 1010;
    release.state = 0x100;
    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, release, true, false);

    let (core_release, _) = split_events(&read_all_available(&mut app_peer));
    assert!(
        core_release.contains(&5),
        "core ButtonRelease must reach the client — an XI2-typed implicit \
             grab captured it at the active-grab redirect and dropped it, \
             wedging E27 (measured core 10 press / 2 release; Xorg 3/3)"
    );
}

/// Under a via_xi2 implicit grab, delivery is filtered by the owner's
/// XI2 selection on the grab window (Xorg merges the window xi2mask
/// into the implicit GrabRec): press+release-only selectors must not
/// receive XI_Motion during the click, and the motion must not leak
/// to the window under the cursor either (the grab captures it).
#[test]
fn implicit_grab_xi2_motion_filtered_by_owner_selection() {
    use yserver_protocol::x11::{CreateWindowRequest, ResourceId};
    const APP: u32 = 1;
    const OTHER: u32 = 2;
    let win_a = ResourceId(0x0010_0001);
    let win_b = ResourceId(0x0020_0001);
    const HOST_A: u32 = 0xCAFE_0001;
    const HOST_B: u32 = 0xCAFE_0002;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::default();
    let mut app_peer = install_client(&mut state, APP);
    let mut other_peer = install_client(&mut state, OTHER);

    for (client, win, x) in [(APP, win_a, 0i16), (OTHER, win_b, 500i16)] {
        state.resources.create_window(
            ClientId(client),
            CreateWindowRequest {
                depth: 24,
                window: win,
                parent: ROOT_WINDOW,
                x,
                y: 0,
                width: 100,
                height: 100,
                border_width: 0,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = state.resources.map_window(win);
    }
    // APP: XI_ButtonPress(4)|XI_ButtonRelease(5) ONLY — no XI_Motion(6).
    state
        .clients
        .get_mut(&APP)
        .unwrap()
        .xi2_masks
        .insert((win_a, XI2_MASTER_POINTER_DEVICE_ID), (1 << 4) | (1 << 5));
    // OTHER selects XI_Motion on its own window — the grab must
    // capture the motion away from it anyway.
    state
        .clients
        .get_mut(&OTHER)
        .unwrap()
        .xi2_masks
        .insert((win_b, XI2_MASTER_POINTER_DEVICE_ID), 1 << 6);
    let mut xid_map = HostXidMap::new();
    xid_map.insert(HOST_A, win_a);
    xid_map.insert(HOST_B, win_b);

    let mut press = motion_event();
    press.kind = PointerEventKind::ButtonPress;
    press.host_xid = HOST_A;
    press.detail = 1;
    press.time = 1000;
    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, press, true, false);
    assert!(
        state
            .active_pointer_grab
            .is_some_and(|g| g.implicit && g.via_xi2),
        "precondition: XI2 implicit grab installed"
    );
    let _ = read_all_available(&mut app_peer);
    let _ = read_all_available(&mut other_peer);

    // Motion over OTHER's window while the implicit grab is held.
    let mut motion = motion_event();
    motion.host_xid = HOST_B;
    motion.time = 1005;
    motion.root_x = 550;
    motion.root_y = 10;
    motion.event_x = 50;
    motion.event_y = 10;
    motion.state = 0x100;
    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, motion, true, false);

    let xge_evtypes = |bytes: &[u8]| -> Vec<u16> {
        let mut found = Vec::new();
        let mut off = 0usize;
        while off + 32 <= bytes.len() {
            let advance = if bytes[off] == 35 {
                found.push(u16::from_le_bytes([bytes[off + 8], bytes[off + 9]]));
                32 + u32::from_le_bytes(bytes[off + 4..off + 8].try_into().unwrap()) as usize * 4
            } else {
                32
            };
            off += advance;
        }
        found
    };
    assert!(
        !xge_evtypes(&read_all_available(&mut app_peer)).contains(&6),
        "owner did not select XI_Motion on the grab window — the \
             implicit grab must not over-deliver it (Xorg xi2mask filter)"
    );
    assert!(
        xge_evtypes(&read_all_available(&mut other_peer)).is_empty(),
        "the grab captures motion — the window under the cursor gets nothing"
    );

    // The release (selected) still delivers to the owner on the grab window.
    let mut release = motion_event();
    release.kind = PointerEventKind::ButtonRelease;
    release.host_xid = HOST_B;
    release.detail = 1;
    release.time = 1010;
    release.root_x = 550;
    release.root_y = 10;
    release.event_x = 50;
    release.event_y = 10;
    release.state = 0x100;
    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, release, true, false);
    assert!(
        xge_evtypes(&read_all_available(&mut app_peer)).contains(&5),
        "selected XI_ButtonRelease still delivers under the implicit grab"
    );
    assert!(
        state.active_pointer_grab.is_none(),
        "grab torn down after final release"
    );
}

/// One mapped 100x100 window at (x,0) owned by `client`, host xid
/// registered in `map`. Implicit-grab test scaffolding.
fn implicit_test_window(
    state: &mut ServerState,
    map: &mut HostXidMap,
    client: u32,
    win: u32,
    host: u32,
    x: i16,
) -> yserver_protocol::x11::ResourceId {
    use yserver_protocol::x11::{CreateWindowRequest, ResourceId};
    let id = ResourceId(win);
    state.resources.create_window(
        ClientId(client),
        CreateWindowRequest {
            depth: 24,
            window: id,
            parent: ROOT_WINDOW,
            x,
            y: 0,
            width: 100,
            height: 100,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(id);
    map.insert(host, id);
    id
}

fn button_event(kind: PointerEventKind, host: u32, button: u8, time: u32) -> HostPointerEvent {
    let mut ev = motion_event();
    ev.kind = kind;
    ev.host_xid = host;
    ev.detail = button;
    ev.time = time;
    ev
}

fn install_passive_test_grab(
    state: &mut ServerState,
    xid_map: &mut HostXidMap,
    owner: u32,
    event_mask: u32,
    via_xi2: bool,
) -> yserver_protocol::x11::ResourceId {
    let win = implicit_test_window(state, xid_map, owner, 0x0010_0001, 0xCAFE_0001, 0);
    state.button_grabs.push(crate::server::PassiveButtonGrab {
        device_id: 0,
        owner: ClientId(owner),
        grab_window: win,
        button: 1,
        modifiers: 0x8000,
        owner_events: false,
        event_mask,
        pointer_mode: 1,
        keyboard_mode: 1,
        confine_to: yserver_protocol::x11::ResourceId(0),
        via_xi2,
    });
    win
}

#[test]
fn passive_grab_activation_populates_and_final_release_clears_record() {
    let mut state = ServerState::new();
    let mut backend = RecordingBackend::default();
    let _peer = install_client(&mut state, 1);
    let mut xid_map = HostXidMap::new();
    let win = install_passive_test_grab(&mut state, &mut xid_map, 1, 0x000c, false);

    let fan = |state: &mut ServerState, backend: &mut RecordingBackend, kind, button, time| {
        let event = button_event(kind, 0xCAFE_0001, button, time);
        let _ = pointer_event_fanout_to_state(state, backend, &xid_map, event, true, false);
    };
    fan(
        &mut state,
        &mut backend,
        PointerEventKind::ButtonPress,
        1,
        1000,
    );
    assert!(state.active_pointer_grab.is_some_and(|grab| {
        grab.owner == ClientId(1) && grab.grab_window == win && grab.passive && !grab.implicit
    }));

    fan(
        &mut state,
        &mut backend,
        PointerEventKind::ButtonPress,
        3,
        1001,
    );
    fan(
        &mut state,
        &mut backend,
        PointerEventKind::ButtonRelease,
        1,
        1002,
    );
    assert!(
        state.active_pointer_grab.is_some_and(|grab| grab.passive),
        "a non-final release must not deactivate the passive grab"
    );

    fan(
        &mut state,
        &mut backend,
        PointerEventKind::ButtonRelease,
        3,
        1003,
    );
    assert!(
        state.active_pointer_grab.is_none(),
        "the final release must clear the passive grab"
    );
}

#[test]
fn active_grab_target_uses_matched_passive_grab_not_last_registered() {
    let mut state = ServerState::new();
    let mut backend = RecordingBackend::default();
    let _peer = install_client(&mut state, 1);
    let mut xid_map = HostXidMap::new();
    let win = implicit_test_window(&mut state, &mut xid_map, 1, 0x0010_0001, 0xCAFE_0001, 0);
    for (modifiers, event_mask) in [(0, 0x000c), (1, 0x0040)] {
        state.button_grabs.push(crate::server::PassiveButtonGrab {
            device_id: 0,
            owner: ClientId(1),
            grab_window: win,
            button: 1,
            modifiers,
            owner_events: false,
            event_mask,
            pointer_mode: 1,
            keyboard_mode: 1,
            confine_to: yserver_protocol::x11::ResourceId(0),
            via_xi2: false,
        });
    }
    let press = button_event(PointerEventKind::ButtonPress, 0xCAFE_0001, 1, 1000);
    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, press, true, false);

    let (_, _, _, _, _, _, mask) = active_grab_target(&state).expect("passive grab active");
    assert_eq!(
        mask, 0x000c,
        "the active record must snapshot the grab that matched, not the last registration"
    );
}

#[test]
fn xi2_passive_grab_delivers_final_release_before_teardown() {
    let mut state = ServerState::new();
    let mut backend = RecordingBackend::default();
    let mut peer = install_client(&mut state, 1);
    let mut xid_map = HostXidMap::new();
    let win = install_passive_test_grab(&mut state, &mut xid_map, 1, (1 << 4) | (1 << 5), true);

    let press = button_event(PointerEventKind::ButtonPress, 0xCAFE_0001, 1, 1000);
    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, press, true, false);
    let _ = read_all_available(&mut peer);
    let mut release = button_event(PointerEventKind::ButtonRelease, 0xCAFE_0001, 1, 1001);
    release.state = 0x0100;
    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, release, true, false);

    let bytes = read_all_available(&mut peer);
    assert!(bytes.windows(28).any(|event| {
        event[0] == 35
            && u16::from_le_bytes([event[8], event[9]]) == 5
            && u32::from_le_bytes(event[24..28].try_into().unwrap()) == win.0
    }));
    assert!(state.active_pointer_grab.is_none());
}

/// Xorg gate is `if (deliveries)` (dix/events.c:2415): a press nobody
/// selected installs nothing.
#[test]
fn implicit_grab_not_installed_when_press_undelivered() {
    let mut state = ServerState::new();
    let mut backend = RecordingBackend::default();
    let _peer = install_client(&mut state, 1);
    let mut xid_map = HostXidMap::new();
    // Window exists but no client selects button events anywhere.
    let _w = implicit_test_window(&mut state, &mut xid_map, 1, 0x0010_0001, 0xCAFE_0001, 0);
    let press = button_event(PointerEventKind::ButtonPress, 0xCAFE_0001, 1, 1000);
    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, press, true, false);
    assert!(state.active_pointer_grab.is_none());
}

/// A press while an explicit grab is active never installs (Xorg
/// `if (!grab ...)`) — and must not clobber the explicit record.
#[test]
fn implicit_grab_not_installed_under_explicit_grab() {
    use crate::server::ActivePointerGrab;
    let mut state = ServerState::new();
    let mut backend = RecordingBackend::default();
    let _peer = install_client(&mut state, 1);
    let mut xid_map = HostXidMap::new();
    let w = implicit_test_window(&mut state, &mut xid_map, 1, 0x0010_0001, 0xCAFE_0001, 0);
    state
        .clients
        .get_mut(&1)
        .unwrap()
        .event_masks
        .insert(w, 0x0000_000c);
    let explicit = ActivePointerGrab {
        owner: ClientId(1),
        grab_window: w,
        event_mask: 0x000c,
        cursor: yserver_protocol::x11::ResourceId(0),
        time: 500,
        owner_events: false,
        via_xi2: false,
        implicit: false,
        passive: false,
        xi2_mask: 0,
    };
    state.active_pointer_grab = Some(explicit);
    let press = button_event(PointerEventKind::ButtonPress, 0xCAFE_0001, 1, 1000);
    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, press, true, false);
    assert!(
        state
            .active_pointer_grab
            .is_some_and(|g| !g.implicit && g.time == 500),
        "explicit grab record must be untouched by the press"
    );
    // And the explicit grab does NOT auto-release on the final release.
    let release = button_event(PointerEventKind::ButtonRelease, 0xCAFE_0001, 1, 1010);
    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, release, true, false);
    assert!(
        state.active_pointer_grab.is_some(),
        "explicit grabs persist until UngrabPointer (only implicit auto-releases)"
    );
}

/// Multi-button click: the grab holds until ALL buttons release
/// (Xi/exevents.c:1935 `!b->buttonsDown`), a second press neither
/// reinstalls nor activates passive grabs (dix `if (!grab &&
/// CheckDeviceGrabs...)`, yserver's active_grab_present gate), and
/// pressing again after a partial release still doesn't reinstall
/// (pins the no-transition-gate model).
#[test]
fn implicit_grab_multi_button_lifecycle() {
    let mut state = ServerState::new();
    let mut backend = RecordingBackend::default();
    let _peer = install_client(&mut state, 1);
    let _wm = install_client(&mut state, 2);
    let mut xid_map = HostXidMap::new();
    let w = implicit_test_window(&mut state, &mut xid_map, 1, 0x0010_0001, 0xCAFE_0001, 0);
    state
        .clients
        .get_mut(&1)
        .unwrap()
        .event_masks
        .insert(w, 0x0000_000c);
    // A passive grab that WOULD match button 3 — it must not activate
    // while the implicit grab holds the device.
    state.button_grabs.push(crate::server::PassiveButtonGrab {
        device_id: 0,
        owner: ClientId(2),
        grab_window: w,
        button: 3,
        modifiers: 0x8000, // AnyModifier
        owner_events: false,
        event_mask: 0x0000_000c,
        pointer_mode: 1,
        keyboard_mode: 1,
        confine_to: yserver_protocol::x11::ResourceId(0),
        via_xi2: false,
    });

    let fan = |state: &mut ServerState, backend: &mut RecordingBackend, kind, button, time| {
        let ev = button_event(kind, 0xCAFE_0001, button, time);
        let _ = pointer_event_fanout_to_state(state, backend, &xid_map, ev, true, false);
    };
    fan(
        &mut state,
        &mut backend,
        PointerEventKind::ButtonPress,
        1,
        1000,
    );
    assert!(state.active_pointer_grab.is_some_and(|g| g.implicit));
    fan(
        &mut state,
        &mut backend,
        PointerEventKind::ButtonPress,
        3,
        1005,
    );
    assert!(
        state
            .active_pointer_grab
            .is_some_and(|g| g.implicit && !g.passive),
        "second press: no passive activation, implicit grab unchanged"
    );
    fan(
        &mut state,
        &mut backend,
        PointerEventKind::ButtonRelease,
        1,
        1010,
    );
    assert!(
        state.active_pointer_grab.is_some(),
        "grab persists while button 3 is still down"
    );
    fan(
        &mut state,
        &mut backend,
        PointerEventKind::ButtonPress,
        1,
        1015,
    );
    assert!(
        state
            .active_pointer_grab
            .is_some_and(|g| g.implicit && g.time == 1000),
        "re-press during the grab must not reinstall (time unchanged)"
    );
    fan(
        &mut state,
        &mut backend,
        PointerEventKind::ButtonRelease,
        1,
        1020,
    );
    fan(
        &mut state,
        &mut backend,
        PointerEventKind::ButtonRelease,
        3,
        1025,
    );
    assert!(
        state.active_pointer_grab.is_none(),
        "final release tears down"
    );
}
