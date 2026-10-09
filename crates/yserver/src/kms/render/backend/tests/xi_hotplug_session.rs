use super::*;

#[test]
fn kms_fake_input_button_ten_uses_btn_forward_not_btn_task() {
    // Mutation killed: map BTN_FORWARD through BTN_TASK in the KMS
    // code-to-detail table, or alias physical BTN_TASK to X button 10.
    use crate::kms::render::backend::KmsBackend;
    use yserver_core::{
        backend::Backend,
        core_loop::{DeviceInfo, HostInputEvent, InputOrigin, process_request},
        resources::ROOT_WINDOW,
        server::ServerState,
        xinput::{DEVICEID_XTEST_POINTER, InputCapabilities, InputSourceId, XiFacetKind},
    };
    use yserver_protocol::x11::{ClientId, RequestHeader, SequenceNumber};

    const CLIENT: u32 = 5;
    const SOURCE: InputSourceId = InputSourceId(0xB1041);
    const XI_BUTTON_PRESS_RELEASE: u32 = (1 << 4) | (1 << 5);

    fn assert_button_events(bytes: &[u8], expected_type: u16) {
        assert!(!bytes.is_empty(), "selected button event is delivered");
        let mut offset = 0;
        let mut count = 0;
        while offset < bytes.len() {
            assert_eq!(bytes[offset], 35, "XI2 GenericEvent");
            let extra_units = u32::from_le_bytes(
                bytes[offset + 4..offset + 8]
                    .try_into()
                    .expect("GenericEvent length"),
            ) as usize;
            let event_len = 32 + extra_units * 4;
            assert!(offset + event_len <= bytes.len(), "complete GenericEvent");
            assert_eq!(
                u16::from_le_bytes(
                    bytes[offset + 8..offset + 10]
                        .try_into()
                        .expect("event type")
                ),
                expected_type,
            );
            assert_eq!(
                u16::from_le_bytes(
                    bytes[offset + 16..offset + 18]
                        .try_into()
                        .expect("button detail")
                ),
                10,
                "BTN_FORWARD is X button 10",
            );
            count += 1;
            offset += event_len;
        }
        assert!(count > 0);
    }

    fn input_state_snapshot(state: &ServerState) -> String {
        format!(
            "{:?}",
            (
                state
                    .xi_devices
                    .devices()
                    .iter()
                    .map(|device| (
                        device.id,
                        device.source_id,
                        device.facet,
                        device.enabled,
                        device.attached_master,
                        device.buttons_down,
                        device.properties.clone(),
                    ))
                    .collect::<Vec<_>>(),
                state.keys_down,
                state.buttons_down,
                state.key_down_by_device.clone(),
                state.xi2_detached_masters.clone(),
                state.floating_pointer_positions.clone(),
                state.clients[&CLIENT].xi2_masks.clone(),
            )
        )
    }

    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    backend
        .core
        .xid_map
        .insert(backend.core.window_id, ROOT_WINDOW);
    let mut peer = kbd_map_client_id(&mut state, CLIENT);

    kbd_map_request(&mut state, &mut backend, 137, 47, &[2, 0, 3, 0]);
    assert_eq!(kbd_map_drain(&mut peer)[0], 1, "XIQueryVersion reply");
    let mut select = Vec::new();
    select.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    select.extend_from_slice(&2u16.to_le_bytes());
    select.extend_from_slice(&[0; 2]);
    for device_id in [2u16, DEVICEID_XTEST_POINTER] {
        select.extend_from_slice(&device_id.to_le_bytes());
        select.extend_from_slice(&1u16.to_le_bytes());
        select.extend_from_slice(&XI_BUTTON_PRESS_RELEASE.to_le_bytes());
    }
    kbd_map_request(&mut state, &mut backend, 137, 46, &select);
    assert!(
        kbd_map_drain(&mut peer).is_empty(),
        "XISelectEvents has no reply"
    );
    let baseline = input_state_snapshot(&state);

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceAdded(DeviceInfo {
            source_id: SOURCE,
            enabled: true,
            resume_key: None,
            capabilities: InputCapabilities {
                keyboard: false,
                pointer: true,
                touch: false,
            },
            name: "BTN_FORWARD regression mouse".to_owned(),
            device_node: "/dev/input/event-btn-forward-test".to_owned(),
            sysname: "event-btn-forward-test".to_owned(),
            vendor_id: 1,
            product_id: 1,
            is_touchpad: false,
            config: Default::default(),
        }),
    );
    let pointer_id = state
        .xi_devices
        .facet(SOURCE, XiFacetKind::PointerTouch)
        .expect("physical pointer facet follows production add path");
    let after_add = input_state_snapshot(&state);

    for (sequence, event_type, expected_type) in [
        (2, yserver_protocol::x11::xtest::FAKE_BUTTON_PRESS, 4u16),
        (3, yserver_protocol::x11::xtest::FAKE_BUTTON_RELEASE, 5u16),
    ] {
        let mut body = vec![0u8; 28];
        body[0] = event_type;
        body[1] = 10;
        process_request::process_request(
            &mut state,
            &mut backend,
            ClientId(CLIENT),
            SequenceNumber(sequence),
            RequestHeader {
                opcode: 146,
                data: 2,
                length_units: 8,
            },
            &body,
            None,
        )
        .expect("production XTEST FakeInput request");
        assert_button_events(&kbd_map_drain(&mut peer), expected_type);
    }

    for (pressed, expected_type) in [(true, 4u16), (false, 5u16)] {
        Backend::on_host_input(
            &mut backend,
            &mut state,
            HostInputEvent::PointerButton {
                origin: InputOrigin::Physical(SOURCE),
                button: 0x115, // BTN_FORWARD -> X 10 (btn_linux2xorg, xf86-input-libinput/src/xf86libinput.c:253-272).
                pressed,
                time: 10,
            },
        );
        assert_button_events(&kbd_map_drain(&mut peer), expected_type);
    }
    assert_eq!(input_state_snapshot(&state), after_add);

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::PointerButton {
            origin: InputOrigin::Physical(SOURCE),
            button: 0x117, // BTN_TASK must not be aliased to X button 10.
            pressed: true,
            time: 11,
        },
    );
    assert!(kbd_map_drain(&mut peer).is_empty());
    assert_eq!(input_state_snapshot(&state), after_add);

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceRemoved { source_id: SOURCE },
    );
    assert!(kbd_map_drain(&mut peer).is_empty());
    assert_eq!(input_state_snapshot(&state), baseline);
    assert!(state.xi_devices.source(SOURCE).is_none());
    assert!(state.xi_devices.device(pointer_id).is_none());
    assert_eq!(state.xi_devices.len(), 4, "masters and XTEST only");
}

#[test]
fn kms_button_diagnostic_ignores_xtest_hold_for_physical_wheel_pair() {
    use crate::kms::render::backend::KmsBackend;
    use yserver_core::{
        core_loop::{DeviceInfo, InputOrigin, message::LibinputConfigSnapshot},
        server::ServerState,
        xinput::{DEVICEID_XTEST_POINTER, InputCapabilities, InputSourceId, XiFacetKind},
    };

    let source = InputSourceId(0xD1A6);
    let mut state = ServerState::new();
    state.xi_register_source(&DeviceInfo {
        source_id: source,
        enabled: true,
        resume_key: None,
        capabilities: InputCapabilities {
            keyboard: false,
            pointer: true,
            touch: false,
        },
        name: "diagnostic test mouse".into(),
        device_node: "/dev/input/event-diagnostic".into(),
        sysname: "event-diagnostic".into(),
        vendor_id: 0,
        product_id: 0,
        is_touchpad: false,
        config: LibinputConfigSnapshot::default(),
    });
    let physical_pointer = state
        .xi_devices
        .facet(source, XiFacetKind::PointerTouch)
        .expect("physical pointer facet");

    // XTEST holds logical button 5. The master aggregates that hold, but
    // the physical source starts its wheel press with button 5 clear.
    state
        .xi_devices
        .device_mut(DEVICEID_XTEST_POINTER)
        .expect("XTEST pointer")
        .buttons_down = 1 << 4;
    state.buttons_down = 1 << 4;
    let physical = InputOrigin::Physical(source);
    assert_eq!(
        KmsBackend::button_diagnostic(&state, physical, 0x1000, true),
        None,
        "another device's held button 5 must not flag the physical press",
    );

    // Fanout records the physical source's accepted press before it
    // receives the matching release. XTEST still owns the master hold.
    state
        .xi_devices
        .device_mut(physical_pointer)
        .expect("physical pointer")
        .buttons_down |= 1 << 4;
    assert_eq!(
        KmsBackend::button_diagnostic(&state, physical, 0x1000, false),
        None,
        "the physical release balances its own press",
    );
    state
        .xi_devices
        .device_mut(physical_pointer)
        .expect("physical pointer")
        .buttons_down &= !(1 << 4);
    assert_eq!(
        KmsBackend::button_diagnostic(&state, physical, 0x1000, true),
        None,
        "a repeated physical wheel pair can start while XTEST holds button 5",
    );
    state
        .xi_devices
        .device_mut(physical_pointer)
        .expect("physical pointer")
        .buttons_down |= 1 << 4;
    assert_eq!(
        KmsBackend::button_diagnostic(&state, physical, 0x1000, false),
        None,
        "the repeated physical release also balances",
    );
}

#[test]
fn kms_button_diagnostic_warns_on_duplicate_physical_press() {
    use crate::kms::render::backend::{KmsBackend, KmsButtonDiagnostic};
    use yserver_core::{
        core_loop::{DeviceInfo, InputOrigin, message::LibinputConfigSnapshot},
        server::ServerState,
        xinput::{InputCapabilities, InputSourceId, XiFacetKind},
    };

    let source = InputSourceId(0xD1A7);
    let mut state = ServerState::new();
    state.xi_register_source(&DeviceInfo {
        source_id: source,
        enabled: true,
        resume_key: None,
        capabilities: InputCapabilities {
            keyboard: false,
            pointer: true,
            touch: false,
        },
        name: "diagnostic test mouse".into(),
        device_node: "/dev/input/event-diagnostic".into(),
        sysname: "event-diagnostic".into(),
        vendor_id: 0,
        product_id: 0,
        is_touchpad: false,
        config: LibinputConfigSnapshot::default(),
    });
    let physical_pointer = state
        .xi_devices
        .facet(source, XiFacetKind::PointerTouch)
        .expect("physical pointer facet");
    let physical = InputOrigin::Physical(source);

    assert_eq!(
        KmsBackend::button_diagnostic(&state, physical, 0x0100, true),
        None,
        "the first physical Button1 press is balanced",
    );
    state
        .xi_devices
        .device_mut(physical_pointer)
        .expect("physical pointer")
        .buttons_down |= 1;
    state.buttons_down |= 1;
    assert_eq!(
        KmsBackend::button_diagnostic(&state, physical, 0x0100, true),
        Some(KmsButtonDiagnostic::PressAlreadyHeld { mask: 0x0100 }),
        "a second physical Button1 press without release must warn",
    );
}

#[test]
fn xi_hotplug_per_window_host_lifecycle_delivers_each_window_copy() {
    use yserver_core::{
        backend::Backend,
        core_loop::{HostInputEvent, process_disconnect::process_disconnect, process_request},
        resources::ROOT_WINDOW,
        server::ServerState,
        xinput::{
            InputSourceId, XI1_DEVICE_PRESENCE_CLASS, XI2_HIERARCHY_CHANGED_MASK, XiFacetKind,
        },
    };
    use yserver_protocol::x11::{ClientId, RequestHeader, ResourceId, SequenceNumber};

    const CLIENT_A: u32 = 5;
    const CLIENT_B: u32 = 6;
    const SOURCE: InputSourceId = InputSourceId(0xA11CE);
    let child = ResourceId(0x10_0A11);

    #[derive(Debug, PartialEq)]
    struct Xi1InputStateSnapshot {
        device: u16,
        keys_down: [u8; 32],
        buttons_down: [u8; 32],
        valuator_mode: u8,
        valuators: [i32; 4],
    }

    fn dispatch(
        state: &mut ServerState,
        backend: &mut KmsBackend,
        client: u32,
        sequence: u16,
        opcode: u8,
        data: u8,
        body: &[u8],
    ) {
        let outcome = process_request::process_request(
            state,
            backend,
            ClientId(client),
            SequenceNumber(sequence),
            RequestHeader {
                opcode,
                data,
                length_units: u32::try_from(1 + body.len().div_ceil(4)).unwrap(),
            },
            body,
            None,
        )
        .expect("request passes through the core dispatcher");
        assert!(
            matches!(outcome, process_request::RequestOutcome::Handled),
            "request outcome: {outcome:?}"
        );
    }

    fn xi1_device_input_snapshot(state: &ServerState) -> Vec<Xi1InputStateSnapshot> {
        let mut snapshot = state
            .xi1_device_input_state
            .iter()
            .map(|(device, input)| Xi1InputStateSnapshot {
                device: *device,
                keys_down: input.keys_down,
                buttons_down: input.buttons_down,
                valuator_mode: input.valuator_mode,
                valuators: input.valuators,
            })
            .collect::<Vec<_>>();
        snapshot.sort_unstable_by_key(|input| input.device);
        snapshot
    }

    fn select_hierarchy(
        state: &mut ServerState,
        backend: &mut KmsBackend,
        client: u32,
        sequence: u16,
        window: ResourceId,
    ) {
        let mut body = Vec::with_capacity(16);
        body.extend_from_slice(&window.0.to_le_bytes());
        body.extend_from_slice(&1u16.to_le_bytes()); // one mask
        body.extend_from_slice(&[0; 2]);
        body.extend_from_slice(&0u16.to_le_bytes()); // XIAllDevices
        body.extend_from_slice(&1u16.to_le_bytes()); // one 32-bit mask word
        body.extend_from_slice(&XI2_HIERARCHY_CHANGED_MASK.to_le_bytes());
        dispatch(state, backend, client, sequence, 137, 46, &body);
    }

    fn select_presence(
        state: &mut ServerState,
        backend: &mut KmsBackend,
        client: u32,
        sequence: u16,
        window: ResourceId,
    ) {
        let mut body = Vec::with_capacity(12);
        body.extend_from_slice(&window.0.to_le_bytes());
        body.extend_from_slice(&1u16.to_le_bytes()); // one event class
        body.extend_from_slice(&[0; 2]);
        body.extend_from_slice(&XI1_DEVICE_PRESENCE_CLASS.to_le_bytes());
        dispatch(state, backend, client, sequence, 137, 6, &body);
    }

    fn event_copies(bytes: &[u8], presence: bool) -> Vec<&[u8]> {
        let mut found = Vec::new();
        let mut offset = 0;
        while offset < bytes.len() {
            assert!(offset + 32 <= bytes.len(), "complete event header");
            let generic = bytes[offset] == 35;
            let event_len = if generic {
                let units =
                    u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
                32 + units * 4
            } else {
                32
            };
            assert!(offset + event_len <= bytes.len(), "complete event body");
            let matches = if presence {
                bytes[offset] & 0x7f == 81 // XI_FIRST_EVENT 66 + DevicePresenceNotify 15
            } else {
                generic
                    && u16::from_le_bytes([bytes[offset + 8], bytes[offset + 9]])
                        == u16::try_from(yserver_core::xinput::XI2_HIERARCHY_CHANGED_EVENT_TYPE)
                            .unwrap()
            };
            if matches {
                found.push(&bytes[offset..offset + event_len]);
            }
            offset += event_len;
        }
        found
    }

    fn assert_per_window_copies(bytes: &[u8], expected: usize, presence: bool) {
        let copies = event_copies(bytes, presence);
        assert_eq!(
            copies.len(),
            expected,
            "one copy for each selected window; presence={presence}; wire={bytes:02x?}"
        );
        if copies.len() == 2 {
            assert_eq!(copies[0], copies[1], "wire copies have no window field");
        }
    }

    // Mutation killed: make either lifecycle emitter deduplicate targets per client.
    // Xorg delivers separately to each selected window, including descendants
    // (Xi/exevents.c:3279-3312; Xi/xichangehierarchy.c:119; dix/devices.c:333-346).
    let mut state = ServerState::new();
    let mut backend = KmsBackend::for_tests();
    backend
        .core
        .xid_map
        .insert(backend.core.window_id, ROOT_WINDOW);
    let mut peer_a = kbd_map_client_id(&mut state, CLIENT_A);
    let mut peer_b = kbd_map_client_id(&mut state, CLIENT_B);
    let initial_devices = state
        .xi_devices
        .devices()
        .iter()
        .map(|device| {
            (
                device.id,
                device.source_id,
                device.facet,
                device.enabled,
                device.attached_master,
                device.buttons_down,
                device.properties.clone(),
            )
        })
        .collect::<Vec<_>>();
    let initial_keys_down = state.keys_down;
    let initial_buttons_down = state.buttons_down;
    let initial_key_down_by_device = state.key_down_by_device.clone();
    let initial_xi1_device_input_state = xi1_device_input_snapshot(&state);
    let initial_detached_masters = state.xi2_detached_masters.clone();
    let initial_floating_pointer_positions = state.floating_pointer_positions.clone();
    let mut create = Vec::with_capacity(28);
    create.extend_from_slice(&child.0.to_le_bytes());
    create.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    create.extend_from_slice(&0i16.to_le_bytes());
    create.extend_from_slice(&0i16.to_le_bytes());
    create.extend_from_slice(&32u16.to_le_bytes());
    create.extend_from_slice(&32u16.to_le_bytes());
    create.extend_from_slice(&0u16.to_le_bytes());
    create.extend_from_slice(&0u16.to_le_bytes());
    create.extend_from_slice(&0u32.to_le_bytes());
    create.extend_from_slice(&0u32.to_le_bytes());
    dispatch(&mut state, &mut backend, CLIENT_A, 1, 1, 0, &create);
    select_hierarchy(&mut state, &mut backend, CLIENT_A, 2, ROOT_WINDOW);
    select_hierarchy(&mut state, &mut backend, CLIENT_A, 3, child);
    select_hierarchy(&mut state, &mut backend, CLIENT_B, 1, ROOT_WINDOW);
    select_presence(&mut state, &mut backend, CLIENT_A, 4, ROOT_WINDOW);
    select_presence(&mut state, &mut backend, CLIENT_A, 5, child);
    select_presence(&mut state, &mut backend, CLIENT_B, 2, ROOT_WINDOW);
    assert!(kbd_map_drain(&mut peer_a).is_empty());
    assert!(kbd_map_drain(&mut peer_b).is_empty());

    let mut info = dynamic_test_device(SOURCE, true, false);
    info.enabled = false;
    Backend::on_host_input(&mut backend, &mut state, HostInputEvent::DeviceAdded(info));
    let added_a = kbd_map_drain(&mut peer_a);
    let added_b = kbd_map_drain(&mut peer_b);
    for (bytes, expected) in [(&added_a, 2), (&added_b, 1)] {
        assert_per_window_copies(bytes, expected, true);
        assert_per_window_copies(bytes, expected, false);
    }
    assert!(
        state
            .xi_devices
            .facet(SOURCE, XiFacetKind::Keyboard)
            .is_some()
    );

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceRemoved { source_id: SOURCE },
    );
    let removed_a = kbd_map_drain(&mut peer_a);
    let removed_b = kbd_map_drain(&mut peer_b);
    for (bytes, expected) in [(&removed_a, 2), (&removed_b, 1)] {
        assert_per_window_copies(bytes, expected, true);
        assert_per_window_copies(bytes, expected, false);
    }
    assert!(state.xi_devices.source_ids().is_empty());

    process_disconnect(&mut state, &mut backend, ClientId(CLIENT_A));
    process_disconnect(&mut state, &mut backend, ClientId(CLIENT_B));
    assert!(state.clients.is_empty(), "all selections are released");
    assert!(
        state.resources.window(child).is_none(),
        "child window is released"
    );
    assert_eq!(state.selections, Default::default());
    assert_eq!(state.xi_devices.source_ids(), Vec::<InputSourceId>::new());
    assert_eq!(
        state
            .xi_devices
            .devices()
            .iter()
            .map(|device| {
                (
                    device.id,
                    device.source_id,
                    device.facet,
                    device.enabled,
                    device.attached_master,
                    device.buttons_down,
                    device.properties.clone(),
                )
            })
            .collect::<Vec<_>>(),
        initial_devices,
        "registry entries and property maps return to their pre-hotplug state"
    );
    assert_eq!(state.keys_down, initial_keys_down);
    assert_eq!(state.buttons_down, initial_buttons_down);
    assert_eq!(state.key_down_by_device, initial_key_down_by_device);
    assert_eq!(
        xi1_device_input_snapshot(&state),
        initial_xi1_device_input_state
    );
    assert_eq!(state.xi2_detached_masters, initial_detached_masters);
    assert_eq!(
        state.floating_pointer_positions,
        initial_floating_pointer_positions
    );
}

#[test]
fn xi2_dynamic_hotplug_publishes_atomic_hierarchy_steps() {
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        io::{ErrorKind, Read},
        os::unix::net::UnixStream,
        sync::{Arc, Mutex, atomic::AtomicU16},
    };
    use yserver_core::{
        backend::Backend,
        core_loop::{DeviceInfo, HostInputEvent, process_request},
        server::{ClientState, ServerState},
        xinput::{InputCapabilities, InputSourceId},
    };
    use yserver_protocol::x11::{ClientByteOrder, ClientId, RequestHeader, SequenceNumber};

    fn install(state: &mut ServerState, id: u32) -> UnixStream {
        let (peer, writer) = UnixStream::pair().expect("client socket pair");
        writer.set_nonblocking(true).expect("nonblocking writer");
        peer.set_nonblocking(true).expect("nonblocking peer");
        state.clients.insert(
            id,
            ClientState {
                writer: Arc::new(Mutex::new(yserver_core::transport::Transport::Unix(writer))),
                is_local: true,
                fd_passing: true,
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
                focused_window: yserver_core::resources::ROOT_WINDOW,
                reader_control: None,
            },
        );
        peer
    }

    fn read_events(peer: &mut UnixStream) -> Vec<Vec<u8>> {
        let mut wire = vec![0; 4096];
        let mut used = 0;
        loop {
            match peer.read(&mut wire[used..]) {
                Ok(0) => break,
                Ok(n) => {
                    used += n;
                    if used == wire.len() {
                        break;
                    }
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) => panic!("read event stream: {error}"),
            }
        }
        wire.truncate(used);
        let mut events = Vec::new();
        let mut offset = 0;
        while offset < wire.len() {
            assert!(offset + 32 <= wire.len(), "complete generic event header");
            let words =
                u32::from_le_bytes(wire[offset + 4..offset + 8].try_into().unwrap()) as usize;
            let event_len = 32 + words * 4;
            assert!(offset + event_len <= wire.len(), "complete hierarchy event");
            events.push(wire[offset..offset + event_len].to_vec());
            offset += event_len;
        }
        events
    }

    fn event_info(event: &[u8], device_id: u16) -> (u16, u8, u8, u32) {
        assert_eq!(event[0], 35, "GenericEvent");
        assert_eq!(event[1], 137, "XI extension opcode");
        assert_eq!(u16::from_le_bytes([event[8], event[9]]), 11);
        assert_eq!(
            u16::from_le_bytes([event[10], event[11]]),
            0,
            "XIAllDevices"
        );
        let count = u16::from_le_bytes([event[20], event[21]]) as usize;
        let length = u32::from_le_bytes(event[4..8].try_into().unwrap()) as usize;
        assert_eq!(length * 4, count * 12, "full xXIHierarchyInfo list");
        let aggregate = event[32..32 + count * 12]
            .chunks_exact(12)
            .fold(0, |flags, info| {
                flags | u32::from_le_bytes(info[8..12].try_into().unwrap())
            });
        assert_eq!(
            u32::from_le_bytes(event[16..20].try_into().unwrap()),
            aggregate,
            "event flags OR all info flags"
        );
        for info in event[32..32 + count * 12].chunks_exact(12) {
            if u16::from_le_bytes([info[0], info[1]]) == device_id {
                return (
                    u16::from_le_bytes([info[2], info[3]]),
                    info[4],
                    info[5],
                    u32::from_le_bytes(info[8..12].try_into().unwrap()),
                );
            }
        }
        panic!("hierarchy event omitted device {device_id}");
    }

    fn event_device_ids(event: &[u8]) -> Vec<u16> {
        let count = u16::from_le_bytes([event[20], event[21]]) as usize;
        event[32..32 + count * 12]
            .chunks_exact(12)
            .map(|info| u16::from_le_bytes([info[0], info[1]]))
            .collect()
    }

    fn source_info(source_id: InputSourceId) -> DeviceInfo {
        DeviceInfo {
            source_id,
            enabled: true,
            resume_key: None,
            capabilities: InputCapabilities {
                keyboard: true,
                pointer: true,
                touch: false,
            },
            name: "mixed hotplug test device".to_owned(),
            device_node: "/dev/input/event-test".to_owned(),
            sysname: "event-test".to_owned(),
            vendor_id: 0x1234,
            product_id: 0x5678,
            is_touchpad: false,
            config: Default::default(),
        }
    }

    let source = InputSourceId(0x15_aa);
    let info = source_info(source);
    let mut state = ServerState::new();
    let mut backend = KmsBackend::for_tests();
    let mut selected = install(&mut state, 1);
    let mut unrelated = install(&mut state, 2);
    let mut hierarchy_selection = Vec::new();
    hierarchy_selection.extend_from_slice(&yserver_core::resources::ROOT_WINDOW.0.to_le_bytes());
    hierarchy_selection.extend_from_slice(&1u16.to_le_bytes());
    hierarchy_selection.extend_from_slice(&[0; 2]);
    hierarchy_selection.extend_from_slice(&0u16.to_le_bytes()); // XIAllDevices
    hierarchy_selection.extend_from_slice(&1u16.to_le_bytes());
    hierarchy_selection.extend_from_slice(&(1_u32 << 11).to_le_bytes());
    process_request::process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 137,
            data: 46, // XISelectEvents
            length_units: 5,
        },
        &hierarchy_selection,
        None,
    )
    .expect("select XI_HierarchyChanged through process_request");

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceAdded(info.clone()),
    );
    for (sequence, device_id) in [6_u16, 7].into_iter().enumerate() {
        let device_id_bytes = device_id.to_le_bytes();
        let query_outcome = process_request::process_request(
            &mut state,
            &mut backend,
            ClientId(2),
            SequenceNumber(u16::try_from(sequence + 1).unwrap()),
            RequestHeader {
                opcode: 137,
                data: 48, // XIQueryDevice
                length_units: 2,
            },
            &[device_id_bytes[0], device_id_bytes[1], 0, 0],
            None,
        )
        .expect("XIQueryDevice sees a newly registered facet before its event is read");
        assert!(
            matches!(query_outcome, process_request::RequestOutcome::Handled),
            "XIQueryDevice request outcome: {query_outcome:?}"
        );
        let replies = read_events(&mut unrelated);
        assert_eq!(
            replies.len(),
            1,
            "XIQueryDevice reply for device {device_id}, sequence {}; buffered outbound bytes={}",
            sequence + 1,
            state.clients[&2].outbound.len(),
        );
        assert_eq!(replies[0][0], 1, "XIQueryDevice reply");
        assert_eq!(u16::from_le_bytes([replies[0][8], replies[0][9]]), 1);
        assert_eq!(
            u16::from_le_bytes([replies[0][32], replies[0][33]]),
            device_id
        );
        assert_eq!(replies[0][42], 1, "XIQueryDevice reports enabled facets");
    }
    let added_and_enabled = read_events(&mut selected);
    // Corrected: the previous assertion expected one aggregate Added and
    // one aggregate Enabled event. Xorg sets only flags[dev->id] in
    // dix/devices.c:601-605 and :417-421; xichangehierarchy.c:85-99
    // serializes each device's flags independently.
    assert_eq!(
        added_and_enabled.len(),
        4,
        "each mixed-source facet gets Added then Enabled; client={:?}; outbound={}",
        state.clients[&1].xi2_masks,
        state.clients[&1].outbound.len(),
    );
    for event in &added_and_enabled {
        assert_eq!(event_device_ids(event), vec![2, 3, 4, 5, 6, 7]);
    }
    assert_eq!(
        event_info(&added_and_enabled[0], 2),
        (3, 1, 1, 0),
        "master pointer is attached to its paired keyboard"
    );
    assert_eq!(
        event_info(&added_and_enabled[0], 3),
        (2, 2, 1, 0),
        "master keyboard is attached to its paired pointer"
    );
    // Xorg creates an off device before ActivateDevice publishes
    // XISlaveAdded (devices.c:307-311, 601-605); EnableDevice attaches it
    // only afterwards (devices.c:388-394).
    assert_eq!(event_info(&added_and_enabled[0], 6), (0, 5, 0, 1 << 2));
    assert_eq!(event_info(&added_and_enabled[0], 7), (0, 5, 0, 0));
    assert_eq!(event_info(&added_and_enabled[1], 6), (3, 4, 1, 1 << 6));
    assert_eq!(event_info(&added_and_enabled[1], 7), (0, 5, 0, 0));
    assert_eq!(event_info(&added_and_enabled[2], 6), (3, 4, 1, 0));
    assert_eq!(event_info(&added_and_enabled[2], 7), (0, 5, 0, 1 << 2));
    assert_eq!(event_info(&added_and_enabled[3], 7), (2, 3, 1, 1 << 6));

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceSuspended { source_id: source },
    );
    let disabled = read_events(&mut selected);
    assert_eq!(disabled.len(), 2, "VT suspension disables each facet");
    assert_eq!(event_device_ids(&disabled[0]), vec![2, 3, 4, 5, 6, 7]);
    assert_eq!(event_device_ids(&disabled[1]), vec![2, 3, 4, 5, 6, 7]);
    assert_eq!(event_info(&disabled[0], 6), (3, 4, 0, 1 << 7));
    assert_eq!(event_info(&disabled[0], 7), (2, 3, 1, 0));
    assert_eq!(event_info(&disabled[1], 6), (0, 5, 0, 0));
    assert_eq!(event_info(&disabled[1], 7), (2, 3, 0, 1 << 7));

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceResumed(info.clone()),
    );
    let enabled = read_events(&mut selected);
    assert_eq!(enabled.len(), 2, "VT continuation enables each facet");
    assert_eq!(event_device_ids(&enabled[0]), vec![2, 3, 4, 5, 6, 7]);
    assert_eq!(event_device_ids(&enabled[1]), vec![2, 3, 4, 5, 6, 7]);
    assert_eq!(event_info(&enabled[0], 6), (3, 4, 1, 1 << 6));
    assert_eq!(event_info(&enabled[0], 7), (0, 5, 0, 0));
    assert_eq!(event_info(&enabled[1], 6), (3, 4, 1, 0));
    assert_eq!(event_info(&enabled[1], 7), (2, 3, 1, 1 << 6));

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceRemoved { source_id: source },
    );
    let disabled_and_removed = read_events(&mut selected);
    assert_eq!(
        disabled_and_removed.len(),
        4,
        "each facet is Disabled then Removed before the next facet"
    );
    assert_eq!(
        event_device_ids(&disabled_and_removed[0]),
        vec![2, 3, 4, 5, 6, 7]
    );
    assert_eq!(
        event_device_ids(&disabled_and_removed[1]),
        vec![2, 3, 4, 5, 7, 6]
    );
    assert_eq!(
        event_device_ids(&disabled_and_removed[2]),
        vec![2, 3, 4, 5, 7]
    );
    assert_eq!(
        event_device_ids(&disabled_and_removed[3]),
        vec![2, 3, 4, 5, 7]
    );
    assert_eq!(event_info(&disabled_and_removed[0], 6), (3, 4, 0, 1 << 7));
    assert_eq!(event_info(&disabled_and_removed[0], 7), (2, 3, 1, 0));
    assert_eq!(event_info(&disabled_and_removed[1], 6), (0, 0, 0, 1 << 3));
    assert_eq!(event_info(&disabled_and_removed[2], 7), (2, 3, 0, 1 << 7));
    assert_eq!(event_info(&disabled_and_removed[3], 7), (0, 0, 0, 1 << 3));

    assert!(state.xi_devices.source(source).is_none());
    assert!(state.xi_devices.device(6).is_none());
    assert!(state.xi_devices.device(7).is_none());
    assert!(state.take_xi_removed_device_descriptors().is_empty());
    assert_eq!(
        state.xi_devices.len(),
        4,
        "only masters and virtual devices remain"
    );

    let unmatched_source = InputSourceId(source.0 + 1);
    let unmatched_info = source_info(unmatched_source);
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceAdded(unmatched_info.clone()),
    );
    let fresh_add = read_events(&mut selected);
    assert_eq!(
        fresh_add.len(),
        4,
        "unmatched mixed endpoint starts each facet with Added/Enabled"
    );
    assert_eq!(event_info(&fresh_add[0], 6), (0, 5, 0, 1 << 2));
    assert_eq!(event_info(&fresh_add[1], 6), (3, 4, 1, 1 << 6));
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceSuspended {
            source_id: unmatched_source,
        },
    );
    let unmatched_disabled = read_events(&mut selected);
    assert_eq!(unmatched_disabled.len(), 2);
    assert_eq!(event_info(&unmatched_disabled[0], 6), (3, 4, 0, 1 << 7));
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceRemoved {
            source_id: unmatched_source,
        },
    );
    let unmatched_removed = read_events(&mut selected);
    assert_eq!(
        unmatched_removed.len(),
        2,
        "removing an already-suspended mixed source emits Removed per facet"
    );
    assert_eq!(event_info(&unmatched_removed[0], 6), (0, 0, 0, 1 << 3));
    assert_eq!(event_info(&unmatched_removed[1], 7), (0, 0, 0, 1 << 3));
    assert!(state.xi_devices.source(unmatched_source).is_none());

    let fresh_source = InputSourceId(source.0 + 2);
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceAdded(source_info(fresh_source)),
    );
    let readd = read_events(&mut selected);
    assert_eq!(
        readd.len(),
        4,
        "a fresh mixed source gets Added/Enabled for each facet after Removed"
    );
    assert_eq!(event_info(&readd[0], 6), (0, 5, 0, 1 << 2));
    assert_eq!(event_info(&readd[1], 6), (3, 4, 1, 1 << 6));
    assert!(state.xi_devices.source(fresh_source).is_some());
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceRemoved {
            source_id: fresh_source,
        },
    );
    let final_remove = read_events(&mut selected);
    assert_eq!(final_remove.len(), 4);
    assert_eq!(event_info(&final_remove[0], 6), (3, 4, 0, 1 << 7));
    assert_eq!(event_info(&final_remove[1], 6), (0, 0, 0, 1 << 3));
    assert_eq!(event_info(&final_remove[2], 7), (2, 3, 0, 1 << 7));
    assert_eq!(event_info(&final_remove[3], 7), (0, 0, 0, 1 << 3));

    assert!(state.xi_devices.source(fresh_source).is_none());
    assert!(state.xi_devices.device(6).is_none());
    assert!(state.xi_devices.device(7).is_none());
    assert!(state.take_xi_removed_device_descriptors().is_empty());
    assert_eq!(
        state.xi_devices.len(),
        4,
        "only masters and virtual devices remain"
    );
    assert!(state.xi_devices.source_ids().is_empty());
    assert!(state.key_down_by_device.is_empty());
    assert!(state.unpublished_keyboard_keys_down.is_empty());
    assert!(state.unpublished_pointer_buttons_down.is_empty());
    assert_eq!(state.buttons_down, 0);
    assert!(backend.core.down_keys.is_empty());
    assert_eq!(backend.core.button_mask, 0);
    assert!(backend.core.pending_pointer_events.is_empty());
    assert_eq!(state.xi_devices.device(4).unwrap().buttons_down, 0);
    assert_eq!(state.xi_devices.device(5).unwrap().buttons_down, 0);
    assert!(read_events(&mut unrelated).is_empty());

    let mut floating_info = source_info(InputSourceId(source.0 + 10));
    floating_info.capabilities.keyboard = false;
    floating_info.name = "floating mouse before unrelated hotplug".to_owned();
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceAdded(floating_info.clone()),
    );
    let initial_floating_events = read_events(&mut selected);
    assert_eq!(initial_floating_events.len(), 2);
    let floating_id = state
        .xi_devices
        .facet(
            floating_info.source_id,
            yserver_core::xinput::XiFacetKind::PointerTouch,
        )
        .expect("pointer facet added");

    let mut grab = Vec::with_capacity(20);
    grab.extend_from_slice(&yserver_core::resources::ROOT_WINDOW.0.to_le_bytes());
    grab.extend_from_slice(&0u32.to_le_bytes()); // current time
    grab.extend_from_slice(&0u32.to_le_bytes()); // no cursor
    grab.extend_from_slice(&floating_id.to_le_bytes());
    grab.extend_from_slice(&[1, 1, 0, 0]); // async device, async paired device
    grab.extend_from_slice(&0u16.to_le_bytes()); // no XI2 event masks
    process_request::process_request(
        &mut state,
        &mut backend,
        ClientId(2),
        SequenceNumber(99),
        RequestHeader {
            opcode: 137,
            data: 51, // XIGrabDevice
            length_units: 6,
        },
        &grab,
        None,
    )
    .expect("grab a real pointer facet through process_request");
    let grab_reply = read_events(&mut unrelated);
    assert_eq!(grab_reply.len(), 1, "XIGrabDevice reply");
    assert_eq!(grab_reply[0][0], 1, "XIGrabDevice reply type");
    assert_eq!(grab_reply[0][8], 0, "XIGrabDevice reports Success");
    assert_eq!(
        state
            .xi_devices
            .device(floating_id)
            .unwrap()
            .attached_master,
        None,
        "XIGrabDevice detached the physical slave"
    );
    let active_grab = state
        .xi2_pointer_grabs
        .get(&floating_id)
        .expect("the floating pointer keeps its explicit XI2 grab");
    assert_eq!(active_grab.owner, ClientId(2));
    assert_eq!(
        active_grab.grab_window,
        yserver_core::resources::ROOT_WINDOW
    );
    assert!(active_grab.via_xi2 && !active_grab.implicit && !active_grab.passive);
    assert_eq!(
        state.xi2_detached_masters.get(&floating_id),
        Some(&yserver_core::xinput::DEVICEID_MASTER_POINTER),
        "the original master is retained while the slave is floating"
    );

    let mut next_info = source_info(InputSourceId(source.0 + 11));
    next_info.capabilities.keyboard = false;
    next_info.name = "second mouse".to_owned();
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceAdded(next_info),
    );
    let after_unrelated_hotplug = read_events(&mut selected);
    assert_eq!(after_unrelated_hotplug.len(), 2);
    assert_eq!(
        event_info(&after_unrelated_hotplug[0], floating_id),
        (0, 5, 1, 0),
        "a floating slave remains XIFloatingSlave in subsequent hierarchy snapshots"
    );
    assert_eq!(
        state
            .xi_devices
            .device(floating_id)
            .unwrap()
            .attached_master,
        None
    );
    assert_eq!(
        state
            .xi2_pointer_grabs
            .get(&floating_id)
            .map(|grab| grab.owner),
        Some(ClientId(2)),
        "unrelated hotplug leaves the active slave grab intact"
    );
    assert!(state.active_pointer_grab.is_none());
    assert!(state.xi2_keyboard_grabs.is_empty());
    assert_eq!(state.buttons_down, 0);
    assert_eq!(
        state.xi_devices.device(floating_id).unwrap().buttons_down,
        0
    );
    assert!(state.unpublished_pointer_buttons_down.is_empty());
    assert!(state.sync_pending.is_empty());
    assert!(state.xi1_frozen.values().all(|freeze| {
        freeze.state == yserver_core::server::Xi1SyncState::Thawed
            && freeze.other.is_none()
            && freeze.stored.is_none()
    }));
    assert!(backend.core.pending_pointer_events.is_empty());
    assert!(state.clients[&1].outbound.is_empty());
    assert!(state.clients[&2].outbound.is_empty());
    assert!(read_events(&mut selected).is_empty());
    assert!(read_events(&mut unrelated).is_empty());
}

// Kills: retaining an attached_master for a physically suspended facet,
// which would make XIQueryDevice report it as an attached slave.
#[test]
fn xi_dynamic_session_disable_floats_query_and_resume_reattaches_home_master() {
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        io::{ErrorKind, Read},
        os::unix::net::UnixStream,
        sync::{Arc, Mutex, atomic::AtomicU16},
    };
    use yserver_core::{
        backend::Backend,
        core_loop::{DeviceInfo, HostInputEvent, process_request},
        server::{ClientState, ServerState},
        transport::Transport,
        xinput::{DEVICEID_MASTER_POINTER, InputCapabilities, InputSourceId, XiFacetKind},
    };
    use yserver_protocol::x11::{ClientByteOrder, ClientId, RequestHeader, SequenceNumber};

    fn install(state: &mut ServerState, id: u32) -> UnixStream {
        let (peer, writer) = UnixStream::pair().expect("client socket pair");
        peer.set_nonblocking(true).expect("nonblocking peer");
        writer.set_nonblocking(true).expect("nonblocking writer");
        state.clients.insert(
            id,
            ClientState {
                writer: Arc::new(Mutex::new(Transport::Unix(writer))),
                is_local: true,
                fd_passing: true,
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
                focused_window: yserver_core::resources::ROOT_WINDOW,
                reader_control: None,
            },
        );
        peer
    }

    fn drain(peer: &mut UnixStream) -> Vec<u8> {
        let mut wire = Vec::new();
        let mut chunk = [0u8; 512];
        loop {
            match peer.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => wire.extend_from_slice(&chunk[..count]),
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) => panic!("read client wire: {error}"),
            }
        }
        wire
    }

    fn drain_query_reply(peer: &mut UnixStream) -> Vec<u8> {
        use std::time::Duration;

        let mut wire = Vec::new();
        for _ in 0..100 {
            wire.extend_from_slice(&drain(peer));
            if wire.len() >= 8 {
                let extra_words = u32::from_le_bytes(wire[4..8].try_into().unwrap()) as usize;
                let expected_len = 32 + extra_words * 4;
                if wire.len() >= expected_len {
                    wire.truncate(expected_len);
                    return wire;
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        wire
    }

    fn query_device(
        state: &mut ServerState,
        backend: &mut KmsBackend,
        peer: &mut UnixStream,
        sequence: u16,
        device_id: u16,
    ) -> Vec<u8> {
        let [lo, hi] = device_id.to_le_bytes();
        let outcome = process_request::process_request(
            state,
            backend,
            ClientId(2),
            SequenceNumber(sequence),
            RequestHeader {
                opcode: 137,
                data: 48, // XIQueryDevice
                length_units: 2,
            },
            &[lo, hi, 0, 0],
            None,
        )
        .expect("XIQueryDevice through process_request");
        assert!(
            matches!(outcome, process_request::RequestOutcome::Handled),
            "XIQueryDevice outcome: {outcome:?}"
        );
        use yserver_core::core_loop::client_io::{self, WriteOutcome};
        for _ in 0..100 {
            let outcome =
                client_io::drain_outbound(state.clients.get_mut(&2).expect("query client"))
                    .expect("flush queued XIQueryDevice reply");
            if outcome == WriteOutcome::Done {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let reply = drain_query_reply(peer);
        assert!(
            !reply.is_empty(),
            "XIQueryDevice reply missing; client outbound queue has {} bytes",
            state.clients[&2].outbound.len()
        );
        reply
    }

    let source = InputSourceId(0x16_41);
    let info = DeviceInfo {
        source_id: source,
        enabled: true,
        resume_key: None,
        capabilities: InputCapabilities {
            keyboard: false,
            pointer: true,
            touch: false,
        },
        name: "session disable mouse".to_owned(),
        device_node: "/dev/input/event-session-disable".to_owned(),
        sysname: "event-session-disable".to_owned(),
        vendor_id: 0x1234,
        product_id: 0x5678,
        is_touchpad: false,
        config: Default::default(),
    };

    let mut state = ServerState::new();
    let initial_ids: Vec<_> = state
        .xi_devices
        .devices()
        .iter()
        .map(|device| device.id)
        .collect();
    let initial_properties: HashMap<_, _> = state
        .xi_devices
        .devices()
        .iter()
        .map(|device| (device.id, device.properties.clone()))
        .collect();
    let mut query_peer = install(&mut state, 2);
    let initial_selections = state.clients[&2].xi2_masks.clone();
    let mut backend = KmsBackend::for_tests();

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceAdded(info.clone()),
    );
    let pointer = state
        .xi_devices
        .facet(source, XiFacetKind::PointerTouch)
        .expect("added pointer facet");
    let before = query_device(&mut state, &mut backend, &mut query_peer, 1, pointer);
    assert_eq!(before[0], 1, "XIQueryDevice reply");
    assert_eq!(u16::from_le_bytes(before[34..36].try_into().unwrap()), 3);
    assert_eq!(
        u16::from_le_bytes(before[36..38].try_into().unwrap()),
        DEVICEID_MASTER_POINTER
    );
    assert_eq!(before[42], 1);

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceSuspended { source_id: source },
    );
    let disabled = query_device(&mut state, &mut backend, &mut query_peer, 2, pointer);
    assert_eq!(disabled[0], 1, "XIQueryDevice reply");
    assert_eq!(
        u16::from_le_bytes(disabled[34..36].try_into().unwrap()),
        5,
        "a disabled pointer is XIFloatingSlave"
    );
    assert_eq!(
        u16::from_le_bytes(disabled[36..38].try_into().unwrap()),
        0,
        "a disabled pointer has no attachment"
    );
    assert_eq!(disabled[42], 0, "XIQueryDevice reports disabled");

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceResumed(info.clone()),
    );
    let resumed = query_device(&mut state, &mut backend, &mut query_peer, 3, pointer);
    assert_eq!(resumed[0], 1, "XIQueryDevice reply");
    assert_eq!(u16::from_le_bytes(resumed[34..36].try_into().unwrap()), 3);
    assert_eq!(
        u16::from_le_bytes(resumed[36..38].try_into().unwrap()),
        DEVICEID_MASTER_POINTER,
        "resume attaches to the facet's home master"
    );
    assert_eq!(resumed[42], 1);

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceRemoved { source_id: source },
    );
    assert!(state.xi_devices.source(source).is_none());
    assert_eq!(
        state
            .xi_devices
            .devices()
            .iter()
            .map(|device| device.id)
            .collect::<Vec<_>>(),
        initial_ids,
        "removal restores the original registry"
    );
    for device in state.xi_devices.devices() {
        assert_eq!(device.properties, initial_properties[&device.id]);
    }
    assert_eq!(state.clients[&2].xi2_masks, initial_selections);
    assert!(state.xi2_detached_masters.is_empty());
    assert!(state.floating_pointer_positions.is_empty());
    assert!(state.xi2_pointer_grabs.is_empty());
    assert!(state.xi2_keyboard_grabs.is_empty());
    assert!(state.key_down_by_device.is_empty());
    assert!(state.unpublished_keyboard_keys_down.is_empty());
    assert!(state.unpublished_pointer_buttons_down.is_empty());
    assert_eq!(state.xi_last_slave(DEVICEID_MASTER_POINTER), None);
    assert_eq!(
        state.xi_last_slave(yserver_core::xinput::DEVICEID_MASTER_KEYBOARD),
        None
    );
    assert!(state.keys_down.iter().all(|byte| *byte == 0));
    assert_eq!(state.buttons_down, 0);
    assert_eq!(state.xi_devices.device(4).unwrap().buttons_down, 0);
    assert_eq!(state.xi_devices.device(5).unwrap().buttons_down, 0);
    assert!(state.clients[&2].xi1_event_classes.is_empty());
    assert!(state.clients[&2].xi1_window_event_classes.is_empty());
    assert!(state.sync_pending.is_empty());
    assert!(state.clients[&2].outbound.is_empty());
    assert!(drain(&mut query_peer).is_empty());
}

// Kills: disable keeping the grab-detach entry (`xi2_detached_masters`),
// which lets XIUngrabDevice re-attach a disabled slave to its old master.
#[test]
fn xi_dynamic_session_disable_clears_grab_detach_before_ungrab() {
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        io::{ErrorKind, Read},
        os::unix::net::UnixStream,
        sync::{Arc, Mutex, atomic::AtomicU16},
    };
    use yserver_core::{
        backend::Backend,
        core_loop::{DeviceInfo, HostInputEvent, process_request},
        server::{ClientState, ServerState},
        transport::Transport,
        xinput::{DEVICEID_MASTER_POINTER, InputCapabilities, InputSourceId, XiFacetKind},
    };
    use yserver_protocol::x11::{ClientByteOrder, ClientId, RequestHeader, SequenceNumber};

    fn install(state: &mut ServerState, id: u32) -> UnixStream {
        let (peer, writer) = UnixStream::pair().expect("client socket pair");
        peer.set_nonblocking(true).expect("nonblocking peer");
        writer.set_nonblocking(true).expect("nonblocking writer");
        state.clients.insert(
            id,
            ClientState {
                writer: Arc::new(Mutex::new(Transport::Unix(writer))),
                is_local: true,
                fd_passing: true,
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
                focused_window: yserver_core::resources::ROOT_WINDOW,
                reader_control: None,
            },
        );
        peer
    }

    fn drain(peer: &mut UnixStream) -> Vec<u8> {
        let mut wire = Vec::new();
        let mut chunk = [0u8; 512];
        loop {
            match peer.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => wire.extend_from_slice(&chunk[..count]),
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) => panic!("read client wire: {error}"),
            }
        }
        wire
    }

    fn request(
        state: &mut ServerState,
        backend: &mut KmsBackend,
        peer: &mut UnixStream,
        minor: u8,
        sequence: u16,
        body: &[u8],
    ) -> Vec<u8> {
        process_request::process_request(
            state,
            backend,
            ClientId(1),
            SequenceNumber(sequence),
            RequestHeader {
                opcode: 137,
                data: minor,
                length_units: u32::try_from((body.len() + 4) / 4).unwrap(),
            },
            body,
            None,
        )
        .expect("XI request through process_request");
        drain(peer)
    }

    let source = InputSourceId(0x16_42);
    let info = DeviceInfo {
        source_id: source,
        enabled: true,
        resume_key: None,
        capabilities: InputCapabilities {
            keyboard: false,
            pointer: true,
            touch: false,
        },
        name: "grabbed session disable mouse".to_owned(),
        device_node: "/dev/input/event-grab-disable".to_owned(),
        sysname: "event-grab-disable".to_owned(),
        vendor_id: 0x1234,
        product_id: 0x5678,
        is_touchpad: false,
        config: Default::default(),
    };

    let mut state = ServerState::new();
    let initial_ids: Vec<_> = state
        .xi_devices
        .devices()
        .iter()
        .map(|device| device.id)
        .collect();
    let initial_properties: HashMap<_, _> = state
        .xi_devices
        .devices()
        .iter()
        .map(|device| (device.id, device.properties.clone()))
        .collect();
    let mut owner_peer = install(&mut state, 1);
    let mut backend = KmsBackend::for_tests();

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceAdded(info.clone()),
    );
    let pointer = state
        .xi_devices
        .facet(source, XiFacetKind::PointerTouch)
        .expect("added pointer facet");
    let mut grab = Vec::new();
    grab.extend_from_slice(&yserver_core::resources::ROOT_WINDOW.0.to_le_bytes());
    grab.extend_from_slice(&0u32.to_le_bytes());
    grab.extend_from_slice(&0u32.to_le_bytes());
    grab.extend_from_slice(&pointer.to_le_bytes());
    grab.extend_from_slice(&[1, 1, 0, 0]); // async modes, no owner events
    grab.extend_from_slice(&0u16.to_le_bytes()); // no XI event mask
    let _ = request(&mut state, &mut backend, &mut owner_peer, 51, 1, &grab);
    assert_eq!(
        state.xi2_detached_masters.get(&pointer),
        Some(&DEVICEID_MASTER_POINTER)
    );
    assert!(state.floating_pointer_positions.contains_key(&pointer));

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceSuspended { source_id: source },
    );
    assert!(!state.xi_devices.device(pointer).unwrap().enabled);
    assert_eq!(
        state.xi_devices.device(pointer).unwrap().attached_master,
        None
    );
    assert!(
        state.xi2_pointer_grabs.contains_key(&pointer),
        "the explicit XI grab remains active across session disable"
    );
    assert!(
        !state.xi2_detached_masters.contains_key(&pointer),
        "disable discards the separate saved grab attachment"
    );
    assert!(!state.floating_pointer_positions.contains_key(&pointer));

    let mut ungrab = Vec::new();
    ungrab.extend_from_slice(&0u32.to_le_bytes());
    ungrab.extend_from_slice(&pointer.to_le_bytes());
    ungrab.extend_from_slice(&[0; 2]);
    let _ = request(&mut state, &mut backend, &mut owner_peer, 52, 2, &ungrab);
    assert!(state.xi2_pointer_grabs.is_empty());
    assert!(!state.xi2_detached_masters.contains_key(&pointer));
    assert!(!state.floating_pointer_positions.contains_key(&pointer));
    assert_eq!(
        state.xi_devices.device(pointer).unwrap().attached_master,
        None,
        "ungrab must leave a disabled facet floating"
    );

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceResumed(info.clone()),
    );
    assert!(state.xi_devices.device(pointer).unwrap().enabled);
    assert_eq!(
        state.xi_devices.device(pointer).unwrap().attached_master,
        Some(DEVICEID_MASTER_POINTER)
    );
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceRemoved { source_id: source },
    );

    assert!(state.xi_devices.source(source).is_none());
    assert_eq!(
        state
            .xi_devices
            .devices()
            .iter()
            .map(|device| device.id)
            .collect::<Vec<_>>(),
        initial_ids
    );
    for device in state.xi_devices.devices() {
        assert_eq!(device.properties, initial_properties[&device.id]);
    }
    assert!(state.clients[&1].xi2_masks.is_empty());
    assert!(state.xi2_detached_masters.is_empty());
    assert!(state.floating_pointer_positions.is_empty());
    assert!(state.xi2_pointer_grabs.is_empty());
    assert!(state.xi2_keyboard_grabs.is_empty());
    assert_eq!(state.xi_last_slave(DEVICEID_MASTER_POINTER), None);
    assert_eq!(
        state.xi_last_slave(yserver_core::xinput::DEVICEID_MASTER_KEYBOARD),
        None
    );
    assert!(
        state
            .xi1_frozen
            .values()
            .all(|freeze| freeze.other.is_none())
    );
    assert!(state.sync_pending.is_empty());
    assert!(state.key_down_by_device.is_empty());
    assert!(state.unpublished_keyboard_keys_down.is_empty());
    assert!(state.unpublished_pointer_buttons_down.is_empty());
    assert!(state.keys_down.iter().all(|byte| *byte == 0));
    assert_eq!(state.buttons_down, 0);
    assert_eq!(state.xi_devices.device(4).unwrap().buttons_down, 0);
    assert_eq!(state.xi_devices.device(5).unwrap().buttons_down, 0);
    assert!(state.clients[&1].xi1_event_classes.is_empty());
    assert!(state.clients[&1].xi1_window_event_classes.is_empty());
    assert!(backend.core.down_keys.is_empty());
    assert_eq!(backend.core.button_mask, 0);
    assert!(state.clients[&1].outbound.is_empty());
    assert!(drain(&mut owner_peer).is_empty());
}

// Kills: batching hierarchy flags across facets, emitting a disabled
// descriptor after floating, and retaining lifecycle state after removal.
#[test]
fn xi1_xi2_dynamic_hotplug_presence_precedes_hierarchy_for_each_transition() {
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        io::{ErrorKind, Read},
        os::unix::net::UnixStream,
        sync::{Arc, Mutex, atomic::AtomicU16},
    };
    use yserver_core::{
        backend::Backend,
        core_loop::{DeviceInfo, HostInputEvent, process_request},
        server::{ClientState, ServerState},
        transport::Transport,
        xinput::{InputCapabilities, InputSourceId},
    };
    use yserver_protocol::x11::{ClientByteOrder, ClientId, RequestHeader, SequenceNumber};

    const PRESENCE_CLASS: u32 = 256 << 8;
    const PRESENCE_EVENT: u8 = 81;
    const XI_SELECT_EVENTS: u8 = 46;
    const HIERARCHY_MASK: u32 = 1 << 11;
    const HIERARCHY_CHANGED: u16 = 11;

    fn install(state: &mut ServerState, id: u32) -> UnixStream {
        let (peer, writer) = UnixStream::pair().expect("client socket pair");
        peer.set_nonblocking(true).expect("nonblocking peer");
        writer.set_nonblocking(true).expect("nonblocking writer");
        state.clients.insert(
            id,
            ClientState {
                writer: Arc::new(Mutex::new(Transport::Unix(writer))),
                is_local: true,
                fd_passing: true,
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
                focused_window: yserver_core::resources::ROOT_WINDOW,
                reader_control: None,
            },
        );
        peer
    }

    fn request(
        state: &mut ServerState,
        backend: &mut KmsBackend,
        sequence: u16,
        data: u8,
        body: &[u8],
    ) {
        process_request::process_request(
            state,
            backend,
            ClientId(1),
            SequenceNumber(sequence),
            RequestHeader {
                opcode: 137,
                data,
                length_units: u32::try_from((body.len() + 4) / 4).unwrap(),
            },
            body,
            None,
        )
        .expect("process event selection request");
    }

    fn drain(peer: &mut UnixStream) -> Vec<u8> {
        let mut wire = Vec::new();
        let mut chunk = [0u8; 512];
        loop {
            match peer.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => wire.extend_from_slice(&chunk[..count]),
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) => panic!("read capture: {error}"),
            }
        }
        wire
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Transition {
        Presence(u8, u8),
        Hierarchy(u32, u16, u8, u16, u8),
    }

    fn transitions(wire: &[u8]) -> Vec<Transition> {
        let mut events = Vec::new();
        let mut offset = 0;
        while offset < wire.len() {
            if wire[offset] == 35 {
                let units = usize::try_from(u32::from_le_bytes(
                    wire[offset + 4..offset + 8].try_into().unwrap(),
                ))
                .unwrap();
                assert_eq!(
                    u16::from_le_bytes([wire[offset + 8], wire[offset + 9]]),
                    HIERARCHY_CHANGED,
                    "XI2 HierarchyChanged event"
                );
                let event_flags =
                    u32::from_le_bytes(wire[offset + 16..offset + 20].try_into().unwrap());
                let count = usize::from(u16::from_le_bytes(
                    wire[offset + 20..offset + 22].try_into().unwrap(),
                ));
                let infos = wire[offset + 32..offset + 32 + count * 12]
                    .chunks_exact(12)
                    .filter(|info| u32::from_le_bytes(info[8..12].try_into().unwrap()) != 0)
                    .collect::<Vec<_>>();
                assert_eq!(infos.len(), 1, "one changed device per hierarchy event");
                let info = infos[0];
                let device_flags = u32::from_le_bytes(info[8..12].try_into().unwrap());
                assert_eq!(event_flags, device_flags, "event flags name one facet");
                events.push(Transition::Hierarchy(
                    device_flags,
                    u16::from_le_bytes(info[0..2].try_into().unwrap()),
                    info[4],
                    u16::from_le_bytes(info[2..4].try_into().unwrap()),
                    info[5],
                ));
                offset += 32 + units * 4;
            } else {
                assert_eq!(wire[offset], PRESENCE_EVENT, "XI1 DevicePresenceNotify");
                events.push(Transition::Presence(wire[offset + 8], wire[offset + 9]));
                offset += 32;
            }
        }
        assert_eq!(offset, wire.len(), "complete mixed-protocol stream");
        events
    }

    fn presence(change: u8, device_id: u8) -> Transition {
        Transition::Presence(change, device_id)
    }

    fn hierarchy(flags: u32, device_id: u16, use_: u8, attachment: u16, enabled: u8) -> Transition {
        Transition::Hierarchy(flags, device_id, use_, attachment, enabled)
    }

    fn query_device(
        state: &mut ServerState,
        backend: &mut KmsBackend,
        peer: &mut UnixStream,
        sequence: u16,
        device_id: u16,
    ) -> Vec<u8> {
        let [lo, hi] = device_id.to_le_bytes();
        let outcome = process_request::process_request(
            state,
            backend,
            ClientId(2),
            SequenceNumber(sequence),
            RequestHeader {
                opcode: 137,
                data: 48,
                length_units: 2,
            },
            &[lo, hi, 0, 0],
            None,
        )
        .expect("dispatch XIQueryDevice request");
        assert!(
            matches!(outcome, process_request::RequestOutcome::Handled),
            "XIQueryDevice outcome: {outcome:?}"
        );
        use yserver_core::core_loop::client_io::{self, WriteOutcome};
        for _ in 0..100 {
            if client_io::drain_outbound(state.clients.get_mut(&2).unwrap()).unwrap()
                == WriteOutcome::Done
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let reply = drain(peer);
        assert!(!reply.is_empty(), "XIQueryDevice reply was dispatched");
        reply
    }

    let mut state = ServerState::new();
    let mut peer = install(&mut state, 1);
    let mut query_peer = install(&mut state, 2);
    let mut backend = KmsBackend::for_tests();
    let root = yserver_core::resources::ROOT_WINDOW.0;

    let mut xi2_selection = Vec::new();
    xi2_selection.extend_from_slice(&root.to_le_bytes());
    xi2_selection.extend_from_slice(&1u16.to_le_bytes());
    xi2_selection.extend_from_slice(&[0; 2]);
    xi2_selection.extend_from_slice(&0u16.to_le_bytes()); // XIAllDevices
    xi2_selection.extend_from_slice(&1u16.to_le_bytes());
    xi2_selection.extend_from_slice(&HIERARCHY_MASK.to_le_bytes());
    request(
        &mut state,
        &mut backend,
        1,
        XI_SELECT_EVENTS,
        &xi2_selection,
    );

    let mut xi1_selection = Vec::new();
    xi1_selection.extend_from_slice(&root.to_le_bytes());
    xi1_selection.extend_from_slice(&1u16.to_le_bytes());
    xi1_selection.extend_from_slice(&0u16.to_le_bytes());
    xi1_selection.extend_from_slice(&PRESENCE_CLASS.to_le_bytes());
    request(&mut state, &mut backend, 2, 6, &xi1_selection);
    assert!(drain(&mut peer).is_empty(), "both selections have no reply");
    let selected_hierarchy = state.clients[&1].xi2_masks.clone();
    let selected_presence = state.clients[&1].xi1_event_classes.clone();
    let initial_devices: Vec<u16> = state
        .xi_devices
        .devices()
        .iter()
        .map(|device| device.id)
        .collect();
    let initial_properties: HashMap<_, _> = state
        .xi_devices
        .devices()
        .iter()
        .map(|device| (device.id, device.properties.clone()))
        .collect();

    let info = DeviceInfo {
        source_id: InputSourceId(0x16_31),
        enabled: true,
        resume_key: None,
        capabilities: InputCapabilities {
            keyboard: true,
            pointer: true,
            touch: false,
        },
        name: "mixed-order keyboard and mouse".to_owned(),
        device_node: "/dev/input/event-mixed-order".to_owned(),
        sysname: "event-mixed-order".to_owned(),
        vendor_id: 0x1234,
        product_id: 0x5678,
        is_touchpad: false,
        config: Default::default(),
    };

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceAdded(info.clone()),
    );
    assert_eq!(
        transitions(&drain(&mut peer)),
        vec![
            presence(0, 6),
            hierarchy(1 << 2, 6, 5, 0, 0),
            presence(2, 6),
            hierarchy(1 << 6, 6, 4, 3, 1),
            presence(0, 7),
            hierarchy(1 << 2, 7, 5, 0, 0),
            presence(2, 7),
            hierarchy(1 << 6, 7, 3, 2, 1),
        ],
        "each facet is Added then Enabled, with presence before hierarchy"
    );

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceSuspended {
            source_id: info.source_id,
        },
    );
    assert_eq!(
        transitions(&drain(&mut peer)),
        vec![
            presence(3, 6),
            hierarchy(1 << 7, 6, 4, 3, 0),
            presence(3, 7),
            hierarchy(1 << 7, 7, 3, 2, 0),
        ],
        "disabled descriptors keep the pre-float attachment"
    );
    let disabled_pointer = query_device(&mut state, &mut backend, &mut query_peer, 3, 7);
    assert_eq!(disabled_pointer[0], 1, "XIQueryDevice reply");
    assert_eq!(
        u16::from_le_bytes(disabled_pointer[34..36].try_into().unwrap()),
        5
    );
    assert_eq!(
        u16::from_le_bytes(disabled_pointer[36..38].try_into().unwrap()),
        0
    );
    assert_eq!(disabled_pointer[42], 0);
    assert_eq!(state.xi_devices.device(6).unwrap().attached_master, None);
    assert_eq!(state.xi_devices.device(7).unwrap().attached_master, None);

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceResumed(info.clone()),
    );
    assert_eq!(
        transitions(&drain(&mut peer)),
        vec![
            presence(2, 6),
            hierarchy(1 << 6, 6, 4, 3, 1),
            presence(2, 7),
            hierarchy(1 << 6, 7, 3, 2, 1),
        ],
        "resume enables one facet at a time in creation order"
    );

    for keycode in [38, 56] {
        Backend::on_host_input(
            &mut backend,
            &mut state,
            HostInputEvent::Key(yserver_core::host_x11::HostKeyEvent {
                origin: yserver_core::core_loop::InputOrigin::Physical(info.source_id),
                keycode,
                pressed: true,
                state: 0,
                root_x: 0,
                root_y: 0,
                event_x: 0,
                event_y: 0,
                time: 0,
            }),
        );
    }
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::PointerButton {
            origin: yserver_core::core_loop::InputOrigin::Physical(info.source_id),
            button: 0x110,
            pressed: true,
            time: 0,
        },
    );
    assert_eq!(backend.core.down_keys.len(), 2);
    assert_ne!(backend.core.button_mask, 0);
    assert!(!state.key_down_by_device.is_empty());
    assert_ne!(state.buttons_down, 0);

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceRemoved {
            source_id: info.source_id,
        },
    );
    assert_eq!(
        transitions(&drain(&mut peer)),
        vec![
            presence(3, 6),
            hierarchy(1 << 7, 6, 4, 3, 0),
            presence(1, 6),
            hierarchy(1 << 3, 6, 0, 0, 0),
            presence(3, 7),
            hierarchy(1 << 7, 7, 3, 2, 0),
            presence(1, 7),
            hierarchy(1 << 3, 7, 0, 0, 0),
        ],
        "disable then remove each facet before advancing"
    );
    assert!(state.xi_devices.source(info.source_id).is_none());
    assert!(
        state
            .xi_devices
            .facet(
                info.source_id,
                yserver_core::xinput::XiFacetKind::PointerTouch
            )
            .is_none()
    );
    assert!(state.xi2_pointer_grabs.is_empty());
    assert!(state.xi2_keyboard_grabs.is_empty());
    assert!(state.xi2_detached_masters.is_empty());
    assert!(state.floating_pointer_positions.is_empty());
    assert!(state.xi1_frozen.is_empty());
    assert!(state.sync_pending.is_empty());
    assert!(state.key_down_by_device.is_empty());
    assert!(state.unpublished_keyboard_keys_down.is_empty());
    assert!(state.unpublished_pointer_buttons_down.is_empty());
    assert_eq!(state.buttons_down, 0);
    assert!(state.keys_down.iter().all(|byte| *byte == 0));
    assert!(backend.core.down_keys.is_empty());
    assert_eq!(backend.core.button_mask, 0);
    assert_eq!(
        state
            .xi_devices
            .devices()
            .iter()
            .map(|device| device.id)
            .collect::<Vec<_>>(),
        initial_devices
    );
    for device in state.xi_devices.devices() {
        assert_eq!(device.properties, initial_properties[&device.id]);
    }
    assert_eq!(state.clients[&1].xi2_masks, selected_hierarchy);
    assert_eq!(state.clients[&1].xi1_event_classes, selected_presence);
    assert!(state.active_pointer_grab.is_none());
    assert!(state.active_keyboard_grab.is_none());
    assert!(state.clients[&1].outbound.is_empty());
    assert!(drain(&mut peer).is_empty());

    // A physical unplug can arrive after VT suspension already emitted
    // Disabled. Removal then emits Removed only; Xorg DisableDevice
    // returns without another transition for an already-disabled
    // device (dix/devices.c:468-469).
    let suspended_source = InputSourceId(0x1632);
    let mut suspended_info = info.clone();
    suspended_info.source_id = suspended_source;
    suspended_info.name = "removed while suspended".to_owned();
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceAdded(suspended_info),
    );
    assert_eq!(
        transitions(&drain(&mut peer)),
        vec![
            presence(0, 6),
            hierarchy(1 << 2, 6, 5, 0, 0),
            presence(2, 6),
            hierarchy(1 << 6, 6, 4, 3, 1),
            presence(0, 7),
            hierarchy(1 << 2, 7, 5, 0, 0),
            presence(2, 7),
            hierarchy(1 << 6, 7, 3, 2, 1),
        ],
        "a replacement mixed source starts each facet with Added then Enabled",
    );
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceSuspended {
            source_id: suspended_source,
        },
    );
    assert_eq!(
        transitions(&drain(&mut peer)),
        vec![
            presence(3, 6),
            hierarchy(1 << 7, 6, 4, 3, 0),
            presence(3, 7),
            hierarchy(1 << 7, 7, 3, 2, 0),
        ],
        "VT suspension reports Disabled once",
    );
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceRemoved {
            source_id: suspended_source,
        },
    );
    assert_eq!(
        transitions(&drain(&mut peer)),
        vec![
            presence(1, 6),
            hierarchy(1 << 3, 6, 0, 0, 0),
            presence(1, 7),
            hierarchy(1 << 3, 7, 0, 0, 0),
        ],
        "removal of an already-suspended mouse reports Removed only",
    );
    assert!(state.xi_devices.source(suspended_source).is_none());
    assert!(
        state
            .xi_devices
            .facet(
                suspended_source,
                yserver_core::xinput::XiFacetKind::PointerTouch
            )
            .is_none()
    );
    assert!(state.xi2_pointer_grabs.is_empty());
    assert!(state.xi2_keyboard_grabs.is_empty());
    assert!(state.xi2_detached_masters.is_empty());
    assert!(state.floating_pointer_positions.is_empty());
    assert!(state.xi1_frozen.is_empty());
    assert!(state.sync_pending.is_empty());
    assert!(state.key_down_by_device.is_empty());
    assert!(state.unpublished_keyboard_keys_down.is_empty());
    assert!(state.unpublished_pointer_buttons_down.is_empty());
    assert_eq!(state.buttons_down, 0);
    assert_eq!(
        state
            .xi_devices
            .devices()
            .iter()
            .map(|device| device.id)
            .collect::<Vec<_>>(),
        initial_devices
    );
    for device in state.xi_devices.devices() {
        assert_eq!(device.properties, initial_properties[&device.id]);
    }
    assert_eq!(state.clients[&1].xi2_masks, selected_hierarchy);
    assert_eq!(state.clients[&1].xi1_event_classes, selected_presence);
    assert!(backend.core.down_keys.is_empty());
    assert_eq!(backend.core.button_mask, 0);
    assert!(state.clients[&1].outbound.is_empty());
    assert!(drain(&mut peer).is_empty());
}

#[test]
fn xi1_dynamic_hotplug_notifies_selected_client_at_each_facet_transition() {
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        io::{ErrorKind, Read},
        os::unix::net::UnixStream,
        sync::{Arc, Mutex, atomic::AtomicU16},
        time::{Duration, Instant},
    };
    use yserver_core::{
        backend::Backend,
        core_loop::{DeviceInfo, HostInputEvent, process_request},
        server::{ClientState, ServerState},
        xinput::{InputCapabilities, InputSourceId},
    };
    use yserver_protocol::x11::{ClientByteOrder, ClientId, RequestHeader, SequenceNumber};

    const PRESENCE_CLASS: u32 = 256 << 8;
    const PRESENCE_EVENT: u8 = 81; // XI_FIRST_EVENT (66) + DevicePresenceNotify (15)
    const SEQUENCE: u16 = 73;

    fn install(state: &mut ServerState, id: u32) -> UnixStream {
        let (peer, writer) = UnixStream::pair().expect("client socket pair");
        writer.set_nonblocking(true).expect("nonblocking writer");
        peer.set_nonblocking(true).expect("nonblocking peer");
        state.clients.insert(
            id,
            ClientState {
                writer: Arc::new(Mutex::new(yserver_core::transport::Transport::Unix(writer))),
                is_local: true,
                fd_passing: true,
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
                focused_window: yserver_core::resources::ROOT_WINDOW,
                reader_control: None,
            },
        );
        peer
    }

    fn read_events(
        state: &mut ServerState,
        client_id: u32,
        peer: &mut UnixStream,
    ) -> Vec<[u8; 32]> {
        let mut wire = Vec::new();
        let mut bytes = [0u8; 256];
        loop {
            match peer.read(&mut bytes) {
                Ok(0) => break,
                Ok(count) => wire.extend_from_slice(&bytes[..count]),
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) => panic!("read event stream: {error}"),
            }
        }
        wire.extend(
            state
                .clients
                .get_mut(&client_id)
                .expect("event client")
                .outbound
                .drain(..),
        );
        assert_eq!(wire.len() % 32, 0, "XI1 events have 32-byte wire size");
        wire.chunks_exact(32)
            .map(|event| event.try_into().expect("32-byte XI1 event"))
            .collect()
    }

    fn read_expected_events(
        state: &mut ServerState,
        client_id: u32,
        peer: &mut UnixStream,
        expected: usize,
    ) -> Vec<[u8; 32]> {
        let mut events = read_events(state, client_id, peer);
        let deadline = Instant::now() + Duration::from_secs(1);
        while events.len() < expected && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
            events.extend(read_events(state, client_id, peer));
        }
        events
    }

    fn assert_presence_event(event: &[u8; 32], change: u8, id: u8) {
        assert_eq!(event[0], PRESENCE_EVENT, "XI_FIRST_EVENT + 15");
        assert_eq!(u16::from_le_bytes([event[2], event[3]]), SEQUENCE);
        assert_eq!(event[8], change, "Xorg DevicePresenceNotify change");
        assert_eq!(event[9], id, "physical facet id");
        assert_eq!(u16::from_le_bytes([event[10], event[11]]), 0, "control");
        assert!(event[12..].iter().all(|byte| *byte == 0), "wire padding");
    }

    let mut state = ServerState::new();
    let mut selected = install(&mut state, 1);
    let mut unselected = install(&mut state, 2);
    let mut backend = KmsBackend::for_tests();
    let mut selection = Vec::with_capacity(12);
    selection.extend_from_slice(&yserver_core::resources::ROOT_WINDOW.0.to_le_bytes());
    selection.extend_from_slice(&1u16.to_le_bytes());
    selection.extend_from_slice(&0u16.to_le_bytes());
    selection.extend_from_slice(&PRESENCE_CLASS.to_le_bytes());
    process_request::process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(SEQUENCE),
        RequestHeader {
            opcode: 137,
            data: 6,
            length_units: 4,
        },
        &selection,
        None,
    )
    .expect("MATE-style SelectExtensionEvent request");
    assert!(
        read_events(&mut state, 1, &mut selected).is_empty(),
        "selection has no reply"
    );
    assert_eq!(
        state.clients[&1].xi1_event_classes,
        HashSet::from([PRESENCE_CLASS]),
        "process_request stored device-256 presence selection"
    );

    let source = InputSourceId(0x16_01);
    let info = DeviceInfo {
        source_id: source,
        enabled: true,
        resume_key: None,
        capabilities: InputCapabilities {
            keyboard: false,
            pointer: true,
            touch: false,
        },
        name: "XI1 hotplug test mouse".to_owned(),
        device_node: "/dev/input/event-xi1-test".to_owned(),
        sysname: "event-xi1-test".to_owned(),
        vendor_id: 0x1234,
        product_id: 0x5678,
        is_touchpad: false,
        config: Default::default(),
    };
    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceAdded(info.clone()),
    );
    let added_enabled = read_expected_events(&mut state, 1, &mut selected, 2);
    assert_eq!(
        added_enabled.len(),
        2,
        "Added then Enabled; selection={:?}; source={:?}; devices={:?}",
        state.clients[&1].xi1_event_classes,
        state.xi_devices.source(source),
        state.xi_devices.devices(),
    );
    assert_presence_event(&added_enabled[0], 0, 6);
    assert_presence_event(&added_enabled[1], 2, 6);
    assert!(
        read_events(&mut state, 2, &mut unselected).is_empty(),
        "unselected client"
    );

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceSuspended { source_id: source },
    );
    let disabled = read_expected_events(&mut state, 1, &mut selected, 1);
    assert_presence_event(&disabled[0], 3, 6);
    assert!(read_events(&mut state, 2, &mut unselected).is_empty());

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceResumed(info),
    );
    let enabled = read_expected_events(&mut state, 1, &mut selected, 1);
    assert_presence_event(&enabled[0], 2, 6);
    assert!(read_events(&mut state, 2, &mut unselected).is_empty());

    Backend::on_host_input(
        &mut backend,
        &mut state,
        HostInputEvent::DeviceRemoved { source_id: source },
    );
    let disabled_removed = read_expected_events(&mut state, 1, &mut selected, 2);
    assert_eq!(disabled_removed.len(), 2, "Disabled then Removed");
    assert_presence_event(&disabled_removed[0], 3, 6);
    assert_presence_event(&disabled_removed[1], 1, 6);
    assert!(
        read_events(&mut state, 2, &mut unselected).is_empty(),
        "unselected client"
    );

    assert!(state.xi_devices.source(source).is_none());
    assert!(state.xi_devices.device(6).is_none());
    assert!(state.xi_devices.source_ids().is_empty());
    assert_eq!(state.xi_devices.len(), 4, "masters and XTEST only");
    assert!(state.key_down_by_device.is_empty());
    assert!(state.unpublished_pointer_buttons_down.is_empty());
    assert_eq!(state.buttons_down, 0);
}
