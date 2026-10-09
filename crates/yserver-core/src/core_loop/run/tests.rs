use super::*;

fn install_xi_config_test_client(
    state: &mut ServerState,
    client_id: u32,
) -> crate::transport::CapturedPeer {
    use crate::server::ClientState;
    use std::sync::{Arc, Mutex, atomic::AtomicU16};
    use yserver_protocol::x11::ClientByteOrder;

    let (transport, peer) = crate::transport::Transport::capture_pair();
    state.clients.insert(
        client_id,
        ClientState {
            writer: Arc::new(Mutex::new(transport)),
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
            focused_window: crate::resources::ROOT_WINDOW,
            reader_control: None,
            is_local: true,
            fd_passing: true,
        },
    );
    peer
}

/// A client on `writer` whose root event mask is `root_mask`.
fn unix_client(
    writer: std::os::unix::net::UnixStream,
    id: u32,
    root_mask: u32,
) -> crate::server::ClientState {
    use std::sync::{Arc, Mutex, atomic::AtomicU16};
    crate::server::ClientState {
        writer: Arc::new(Mutex::new(crate::transport::Transport::Unix(writer))),
        byte_order: yserver_protocol::x11::ClientByteOrder::LittleEndian,
        last_sequence: Arc::new(AtomicU16::new(0)),
        resource_id_base: id << 21,
        resource_id_mask: 0x001f_ffff,
        event_masks: if root_mask == 0 {
            HashMap::new()
        } else {
            HashMap::from([(crate::resources::ROOT_WINDOW, root_mask)])
        },
        save_set: HashSet::new(),
        big_requests_enabled: false,
        xi2_masks: HashMap::new(),
        xi1_event_classes: HashSet::new(),
        xi1_window_event_classes: HashMap::new(),
        outbound: VecDeque::new(),
        watching_writable: false,
        write_failed: false,
        focused_window: crate::resources::ROOT_WINDOW,
        reader_control: None,
        is_local: true,
        fd_passing: true,
    }
}

/// Client `owner`'s 10x10 child of the root.
fn create_child_window(state: &mut ServerState, owner: u32) -> yserver_protocol::x11::ResourceId {
    let window = yserver_protocol::x11::ResourceId(owner << 21 | 1);
    state.resources.create_window(
        yserver_protocol::x11::ClientId(owner),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window,
            parent: crate::resources::ROOT_WINDOW,
            width: 10,
            height: 10,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    window
}

/// The core loop disconnects every client a write failed on, and
/// repeats: here the slow client's own teardown (its window's
/// DestroyNotify) overflows a second slow client, which goes too. The
/// reading client stays and sees the DestroyNotify.
#[test]
fn failed_writers_are_disconnected_including_ones_failed_by_a_disconnect() {
    use crate::backend::recording::RecordingBackend;
    use std::{io::Read, os::unix::net::UnixStream};
    const SUBSTRUCTURE_NOTIFY: u32 = 1 << 19;

    let mut state = ServerState::new();
    let mut peers = Vec::new();
    for (id, mask) in [(1, 0), (2, SUBSTRUCTURE_NOTIFY), (3, SUBSTRUCTURE_NOTIFY)] {
        let (a, b) = UnixStream::pair().unwrap();
        peers.push(b);
        state.clients.insert(id, unix_client(a, id, mask));
    }
    let window = create_child_window(&mut state, 1);
    for id in [1, 2] {
        client_io::saturate_for_test(state.clients.get_mut(&id).unwrap());
    }
    state.clients.get_mut(&1).unwrap().write_failed = true;

    disconnect_failed_writers(
        &mut state,
        &mut RecordingBackend::new(),
        &mut PendingBackendRequests::default(),
        &mut XiConfigLane::default(),
        &mut ResetTrigger::new(ResetPolicy::NoReset),
    );
    assert!(!state.clients.contains_key(&1));
    assert!(
        !state.clients.contains_key(&2),
        "failed by 1's DestroyNotify"
    );
    assert!(state.clients.contains_key(&3));
    assert!(state.resources.window(window).is_none());
    let reader = &mut peers[2];
    reader.set_nonblocking(true).unwrap();
    let mut event = [0u8; 32];
    reader.read_exact(&mut event).unwrap();
    assert_eq!(event[0], 17, "DestroyNotify");
}

/// Output a forced disconnect produces for a healthy client whose
/// socket is full gets WRITABLE interest before the loop blocks again;
/// otherwise those bytes would wait for a wakeup that never comes.
#[test]
fn output_buffered_by_a_disconnect_gets_writable_interest() {
    use crate::backend::recording::RecordingBackend;
    use std::{
        io::{ErrorKind, Write},
        os::{fd::AsRawFd, unix::net::UnixStream},
    };
    const SUBSTRUCTURE_NOTIFY: u32 = 1 << 19;

    let poll = Poll::new().unwrap();
    let mut state = ServerState::new();
    let mut peers = Vec::new();
    for (id, mask) in [(1, 0), (2, SUBSTRUCTURE_NOTIFY)] {
        let (a, b) = UnixStream::pair().unwrap();
        a.set_nonblocking(true).unwrap();
        poll.registry()
            .register(
                &mut SourceFd(&a.as_raw_fd()),
                client_token(yserver_protocol::x11::ClientId(id)),
                Interest::READABLE,
            )
            .unwrap();
        peers.push(b);
        state.clients.insert(id, unix_client(a, id, mask));
    }
    let window = create_child_window(&mut state, 1);
    // Fill client 2's socket: the DestroyNotify must buffer.
    {
        let mut writer = state.clients[&2].writer.lock().unwrap();
        let chunk = [0u8; 4096];
        loop {
            match writer.write(&chunk) {
                Ok(_) => {}
                Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                Err(err) => panic!("filling the socket: {err}"),
            }
        }
    }
    state.clients.get_mut(&1).unwrap().write_failed = true;

    settle_client_output(
        poll.registry(),
        &mut state,
        &mut RecordingBackend::new(),
        &mut PendingBackendRequests::default(),
        &mut XiConfigLane::default(),
        &mut ResetTrigger::new(ResetPolicy::NoReset),
    );
    assert!(!state.clients.contains_key(&1));
    assert!(state.resources.window(window).is_none());
    let healthy = &state.clients[&2];
    assert!(!healthy.write_failed);
    assert_eq!(healthy.outbound.len(), 32, "the DestroyNotify");
    assert!(healthy.watching_writable);
    drop(peers);
}

fn xi_vt_config_fixture(
    client: u32,
    source: crate::xinput::InputSourceId,
) -> (
    ServerState,
    crate::transport::CapturedPeer,
    InputInventory,
    crate::core_loop::DeviceInfo,
    u16,
) {
    let mut state = ServerState::new();
    let peer = install_xi_config_test_client(&mut state, client);
    let info = crate::core_loop::DeviceInfo {
        source_id: source,
        enabled: true,
        resume_key: None,
        capabilities: crate::xinput::InputCapabilities {
            pointer: true,
            ..Default::default()
        },
        name: "VT config pointer".into(),
        device_node: format!("/dev/input/event{}", source.0),
        sysname: format!("event{}", source.0),
        vendor_id: 1,
        product_id: 2,
        is_touchpad: false,
        config: crate::core_loop::message::LibinputConfigSnapshot {
            accel: crate::core_loop::message::FloatSetting {
                available: true,
                current: 0.0,
                default: 0.0,
            },
            ..Default::default()
        },
    };
    let device_id = state.xi_register_source(&info)[0];
    let mut inventory = InputInventory::new();
    inventory.add(info.clone());
    (state, peer, inventory, info, device_id)
}

fn xi_vt_accel_request(
    client: yserver_protocol::x11::ClientId,
    sequence: u16,
    device_id: u16,
    state: &ServerState,
    speed: f32,
) -> DeferredRequest {
    let mut body = Vec::new();
    body.extend_from_slice(&device_id.to_le_bytes());
    body.push(crate::xinput::XI_PROP_MODE_REPLACE);
    body.push(32);
    body.extend_from_slice(
        &state
            .atoms
            .id_for("libinput Accel Speed")
            .expect("acceleration property atom")
            .0
            .to_le_bytes(),
    );
    body.extend_from_slice(&state.float_atom.0.to_le_bytes());
    body.extend_from_slice(&1u32.to_le_bytes());
    body.extend_from_slice(&speed.to_le_bytes());
    DeferredRequest {
        id: client,
        sequence: yserver_protocol::x11::SequenceNumber(sequence),
        accepted_at: None,
        header: yserver_protocol::x11::RequestHeader {
            opcode: 137,
            data: 57,
            length_units: 6,
        },
        body,
        attached_fd: None,
    }
}

fn xi_vt_following_focus_request(client: yserver_protocol::x11::ClientId) -> DeferredRequest {
    DeferredRequest {
        id: client,
        sequence: yserver_protocol::x11::SequenceNumber(2),
        accepted_at: None,
        header: yserver_protocol::x11::RequestHeader {
            opcode: 43,
            data: 0,
            length_units: 1,
        },
        body: Vec::new(),
        attached_fd: None,
    }
}

fn send_vt_test_request(sender: &crate::core_loop::sender::BoundSender, request: DeferredRequest) {
    sender
        .send(Message::Request {
            id: request.id,
            sequence: request.sequence,
            accepted_at: request.accepted_at,
            header: request.header,
            body: request.body,
            attached_fd: request.attached_fd,
        })
        .expect("queue runner request");
}

fn run_core_for_vt_test(
    state: &mut ServerState,
    backend: &mut crate::backend::recording::RecordingBackend,
    enqueue: impl FnOnce(&CoreSender, &crate::core_loop::sender::BoundSender),
) -> InputInventory {
    let (poll, sender, receiver) = crate::core_loop::channel().expect("core channel");
    let request_sender = sender.bind();
    enqueue(&sender, &request_sender);
    let mut input_inventory = InputInventory::new();
    run_core_with_inventory(
        poll,
        receiver,
        sender.clone_handle(),
        state,
        backend,
        [],
        &ClientIdAllocator::new(),
        AuthState::new(None),
        ResetPolicy::NoReset,
        None,
        &mut input_inventory,
    )
    .expect("run core through VT release and shutdown messages");
    input_inventory
}

fn run_core_for_vt_test_with_timeout(
    mut state: ServerState,
    mut backend: crate::backend::recording::RecordingBackend,
    peer: crate::transport::CapturedPeer,
    timeout: std::time::Duration,
    enqueue: impl FnOnce(&CoreSender, &crate::core_loop::sender::BoundSender) + Send + 'static,
) -> Result<
    (
        ServerState,
        crate::backend::recording::RecordingBackend,
        crate::transport::CapturedPeer,
        InputInventory,
    ),
    std::sync::mpsc::RecvTimeoutError,
> {
    let (finished_tx, finished_rx) = std::sync::mpsc::sync_channel(1);
    let runner = std::thread::spawn(move || {
        let input_inventory = run_core_for_vt_test(&mut state, &mut backend, enqueue);
        let _ = finished_tx.send((state, backend, peer, input_inventory));
    });
    drop(runner);
    finished_rx.recv_timeout(timeout)
}

#[test]
fn xi_vt_timeout_cancelled_command_keeps_bad_match_and_state() {
    // Mutation killed: omit the core's cancellation before it fails the
    // still-pending request at the VT barrier timeout.
    use crate::xinput::libinput_props::{DeviceConfigError, DeviceConfigStart, DeviceConfigToken};
    use std::io::Read;

    let client = yserver_protocol::x11::ClientId(62);
    let source = crate::xinput::InputSourceId(620);
    let (mut state, peer, _fixture_inventory, info, device) =
        xi_vt_config_fixture(client.0, source);
    state.clients.get_mut(&client.0).unwrap().xi2_masks.insert(
        (crate::resources::ROOT_WINDOW, 0),
        u64::from(crate::xinput::XI2_PROPERTY_EVENT_MASK),
    );
    let mut backend = crate::backend::recording::RecordingBackend::new();
    backend.vt_switching_armed = true;
    backend.vt_release_pause_queued = true;
    backend.vt_release_probe_client = Some(client.0);
    backend.vt_release_probe_source = Some(source);
    let release_finished = backend.vt_release_finished.clone();
    backend
        .device_config_start_results
        .push_back(Ok(DeviceConfigStart::Pending(DeviceConfigToken(620))));
    let request = xi_vt_accel_request(client, 1, device, &state, 0.75);
    let registry_before: Vec<_> = state
        .xi_devices
        .devices()
        .iter()
        .map(|entry| {
            (
                entry.id,
                entry.source_id,
                entry.enabled,
                entry.attached_master,
            )
        })
        .collect();
    let properties_before: Vec<_> = state
        .xi_devices
        .devices()
        .iter()
        .map(|entry| (entry.id, entry.properties.clone()))
        .collect();
    let held_before = (
        state.keys_down,
        state.buttons_down,
        state.key_down_by_device.clone(),
    );
    let detached_before = state.xi2_detached_masters.clone();
    let floating_before = state.floating_pointer_positions.clone();
    let selections_before = (
        state.clients[&client.0].xi2_masks.clone(),
        state.clients[&client.0].xi1_event_classes.clone(),
        state.clients[&client.0].xi1_window_event_classes.clone(),
        state.clients[&client.0].event_masks.clone(),
    );

    let (state, backend, mut peer, inventory) = run_core_for_vt_test_with_timeout(
        state,
        backend,
        peer,
        std::time::Duration::from_secs(2),
        move |sender, requests| {
            sender
                .send(Message::HostInput(HostInputEvent::DeviceAdded(
                    info.clone(),
                )))
                .unwrap();
            send_vt_test_request(requests, request);
            sender.send(Message::VtRelease).unwrap();
            sender.send(Message::VtAcquire).unwrap();
            sender
                .send(Message::HostInput(HostInputEvent::DeviceResumed(info)))
                .unwrap();
            let late_result_sender = sender.clone_handle();
            drop(std::thread::spawn(move || {
                std::thread::sleep(VT_INPUT_PAUSE_TIMEOUT + std::time::Duration::from_millis(100));
                while !release_finished.load(std::sync::atomic::Ordering::SeqCst) {
                    std::thread::yield_now();
                }
                late_result_sender
                    .send(Message::DeviceConfigResult {
                        token: DeviceConfigToken(620),
                        source,
                        result: Err(DeviceConfigError::Cancelled),
                    })
                    .unwrap();
                late_result_sender.send(Message::Shutdown).unwrap();
            }));
        },
    )
    .unwrap_or_else(|error| panic!("cancelled VT config timed out: {error:?}"));

    assert!(backend.device_config_cancel_tokens[0].is_cancelled());
    assert_eq!(backend.started_device_configs.len(), 1);
    assert_eq!(inventory.get(source).unwrap().config.accel.current, 0.0);
    assert_eq!(backend.vt_release_inventory_accel_before_finish, Some(0.0));
    assert_eq!(
        state
            .xi_devices
            .devices()
            .iter()
            .map(|entry| (
                entry.id,
                entry.source_id,
                entry.enabled,
                entry.attached_master
            ))
            .collect::<Vec<_>>(),
        registry_before,
    );
    for (id, properties) in properties_before {
        assert_eq!(state.xi_devices.device(id).unwrap().properties, properties);
    }
    let mut error = [0; 32];
    peer.read_exact(&mut error).unwrap();
    assert_eq!(error[0], 0);
    assert_eq!(error[1], yserver_protocol::x11::error::BAD_MATCH);
    peer.set_nonblocking(true).unwrap();
    let mut extra = [0; 1];
    assert!(
        matches!(peer.read(&mut extra), Err(ref err) if err.kind() == io::ErrorKind::WouldBlock),
        "cancelled result emits no property event or second reply",
    );
    assert_eq!(
        (
            state.keys_down,
            state.buttons_down,
            state.key_down_by_device.clone(),
        ),
        held_before,
    );
    assert_eq!(state.xi2_detached_masters, detached_before);
    assert_eq!(state.floating_pointer_positions, floating_before);
    assert_eq!(
        (
            state.clients[&client.0].xi2_masks.clone(),
            state.clients[&client.0].xi1_event_classes.clone(),
            state.clients[&client.0].xi1_window_event_classes.clone(),
            state.clients[&client.0].event_masks.clone(),
        ),
        selections_before,
    );
}

#[test]
fn xi_vt_release_config_write_without_input_thread_does_not_hang() {
    // Mutation killed: wait for InputPaused even though begin_vt_release
    // did not queue an input-thread pause barrier.
    let client = yserver_protocol::x11::ClientId(60);
    let mut state = ServerState::new();
    let peer = install_xi_config_test_client(&mut state, client.0);
    let mut backend = crate::backend::recording::RecordingBackend::new();
    backend.vt_switching_armed = true;
    let registry_before: Vec<_> = state
        .xi_devices
        .devices()
        .iter()
        .map(|entry| {
            (
                entry.id,
                entry.source_id,
                entry.enabled,
                entry.attached_master,
            )
        })
        .collect();
    let properties_before: Vec<_> = state
        .xi_devices
        .devices()
        .iter()
        .map(|entry| (entry.id, entry.properties.clone()))
        .collect();
    let held_before = (
        state.keys_down,
        state.buttons_down,
        state.key_down_by_device.clone(),
    );
    let detached_before = state.xi2_detached_masters.clone();
    let floating_before = state.floating_pointer_positions.clone();
    let selections_before = (
        state.clients[&client.0].xi2_masks.clone(),
        state.clients[&client.0].xi1_event_classes.clone(),
        state.clients[&client.0].xi1_window_event_classes.clone(),
        state.clients[&client.0].event_masks.clone(),
    );

    let (state, backend, _peer, _inventory) = run_core_for_vt_test_with_timeout(
        state,
        backend,
        peer,
        std::time::Duration::from_millis(250),
        |sender, _requests| {
            sender.send(Message::VtRelease).unwrap();
            sender.send(Message::Shutdown).unwrap();
        },
    )
    .unwrap_or_else(|error| panic!("VT release without input thread timed out: {error:?}"));

    assert!(
        backend
            .vt_release_finished
            .load(std::sync::atomic::Ordering::SeqCst),
        "VT release must finish without an input-thread barrier",
    );
    assert_eq!(
        state
            .xi_devices
            .devices()
            .iter()
            .map(|entry| (
                entry.id,
                entry.source_id,
                entry.enabled,
                entry.attached_master
            ))
            .collect::<Vec<_>>(),
        registry_before,
    );
    assert_eq!(
        state
            .xi_devices
            .devices()
            .iter()
            .map(|entry| (entry.id, entry.properties.clone()))
            .collect::<Vec<_>>(),
        properties_before,
    );
    assert_eq!(
        (
            state.keys_down,
            state.buttons_down,
            state.key_down_by_device.clone(),
        ),
        held_before,
    );
    assert_eq!(state.xi2_detached_masters, detached_before);
    assert_eq!(state.floating_pointer_positions, floating_before);
    assert_eq!(
        (
            state.clients[&client.0].xi2_masks.clone(),
            state.clients[&client.0].xi1_event_classes.clone(),
            state.clients[&client.0].xi1_window_event_classes.clone(),
            state.clients[&client.0].event_masks.clone(),
        ),
        selections_before,
    );
}

#[test]
fn xi_vt_timeout_late_applied_reconciles_inventory_property_and_event() {
    // Mutation killed: discard a timed-out operation's late Applied
    // result instead of reconciling its confirmed state without a reply.
    use crate::xinput::libinput_props::{DeviceConfigStart, DeviceConfigToken};
    use std::io::Read;

    let client = yserver_protocol::x11::ClientId(61);
    let source = crate::xinput::InputSourceId(610);
    let (mut state, peer, _fixture_inventory, info, device) =
        xi_vt_config_fixture(client.0, source);
    let accel_atom = state.atoms.id_for("libinput Accel Speed").unwrap();
    state.clients.get_mut(&client.0).unwrap().xi2_masks.insert(
        (crate::resources::ROOT_WINDOW, 0),
        u64::from(crate::xinput::XI2_PROPERTY_EVENT_MASK),
    );
    let mut backend = crate::backend::recording::RecordingBackend::new();
    backend.vt_switching_armed = true;
    backend.vt_release_pause_queued = true;
    backend.vt_release_probe_client = Some(client.0);
    backend.vt_release_probe_source = Some(source);
    let release_finished = backend.vt_release_finished.clone();
    backend
        .device_config_start_results
        .push_back(Ok(DeviceConfigStart::Pending(DeviceConfigToken(610))));
    let request = xi_vt_accel_request(client, 1, device, &state, 0.75);
    let registry_before: Vec<_> = state
        .xi_devices
        .devices()
        .iter()
        .map(|entry| {
            (
                entry.id,
                entry.source_id,
                entry.enabled,
                entry.session_enabled,
                entry.client_disabled,
                entry.attached_master,
            )
        })
        .collect();
    let property_maps_before: Vec<_> = state
        .xi_devices
        .devices()
        .iter()
        .map(|entry| (entry.id, entry.properties.clone()))
        .collect();
    let held_before = (
        state.keys_down,
        state.buttons_down,
        state.key_down_by_device.clone(),
        state.xi_devices.device(device).unwrap().buttons_down,
    );
    let detached_before = state.xi2_detached_masters.clone();
    let floating_before = state.floating_pointer_positions.clone();
    let selections_before = (
        state.clients[&client.0].xi2_masks.clone(),
        state.clients[&client.0].xi1_event_classes.clone(),
        state.clients[&client.0].xi1_window_event_classes.clone(),
        state.clients[&client.0].event_masks.clone(),
    );

    let started_at = Instant::now();
    let (state, backend, mut peer, inventory) = run_core_for_vt_test_with_timeout(
        state,
        backend,
        peer,
        std::time::Duration::from_secs(2),
        move |sender, requests| {
            sender
                .send(Message::HostInput(HostInputEvent::DeviceAdded(
                    info.clone(),
                )))
                .unwrap();
            send_vt_test_request(requests, request);
            sender.send(Message::VtRelease).unwrap();
            sender.send(Message::VtAcquire).unwrap();
            sender
                .send(Message::HostInput(HostInputEvent::DeviceResumed(info)))
                .unwrap();
            let late_result_sender = sender.clone_handle();
            drop(std::thread::spawn(move || {
                std::thread::sleep(VT_INPUT_PAUSE_TIMEOUT + std::time::Duration::from_millis(100));
                while !release_finished.load(std::sync::atomic::Ordering::SeqCst) {
                    std::thread::yield_now();
                }
                late_result_sender
                    .send(Message::DeviceConfigResult {
                        token: DeviceConfigToken(610),
                        source,
                        result: Ok(()),
                    })
                    .unwrap();
                late_result_sender.send(Message::Shutdown).unwrap();
            }));
        },
    )
    .unwrap_or_else(|error| panic!("unacknowledged VT pause barrier timed out: {error:?}"));
    assert!(
        started_at.elapsed() >= VT_INPUT_PAUSE_TIMEOUT,
        "a queued pause barrier must receive the full bounded wait",
    );

    assert!(backend.vt_release_wire_visible_before_finish);
    assert!(
        backend
            .vt_release_finished
            .load(std::sync::atomic::Ordering::SeqCst)
    );
    assert_eq!(backend.started_device_configs.len(), 1);
    assert!(
        backend.device_config_cancel_tokens[0].is_cancelled(),
        "VT timeout shares cancellation with the submitted input command",
    );
    let mut error = [0; 32];
    peer.read_exact(&mut error).unwrap();
    assert_eq!(error[0], 0);
    assert_eq!(error[1], yserver_protocol::x11::error::BAD_MATCH);
    for (id, properties) in property_maps_before {
        let current = &state.xi_devices.device(id).unwrap().properties;
        if id == device {
            assert_eq!(current.len(), properties.len());
            for (property, previous) in properties {
                let actual = current.get(&property).expect("property remains present");
                if property == accel_atom {
                    assert_eq!(actual.data, 0.75_f32.to_le_bytes());
                    assert_eq!(actual.format, previous.format);
                    assert_eq!(actual.type_atom, previous.type_atom);
                } else {
                    assert_eq!(actual, &previous);
                }
            }
        } else {
            assert_eq!(current, &properties);
        }
    }
    assert_eq!(backend.vt_release_inventory_accel_before_finish, Some(0.0));
    assert_eq!(
        inventory.get(source).unwrap().config.accel.current,
        0.75,
        "confirmed late result updates the process-lifetime inventory",
    );
    assert_eq!(
        state
            .xi_devices
            .source(source)
            .unwrap()
            .config
            .accel
            .current,
        0.75,
        "confirmed late result updates the active XI source snapshot",
    );
    let mut property_event = [0; 32];
    peer.read_exact(&mut property_event).unwrap();
    assert_eq!(property_event[0], 35, "XI2 GenericEvent");
    assert_eq!(
        u16::from_le_bytes([property_event[8], property_event[9]]),
        12,
        "XI_PropertyEvent follows reconciliation",
    );
    assert_eq!(
        u16::from_le_bytes([property_event[10], property_event[11]]),
        device,
        "property event names the current facet",
    );
    assert_eq!(
        u32::from_le_bytes(property_event[16..20].try_into().unwrap()),
        accel_atom.0,
    );
    assert_eq!(property_event[20], 2, "what = Modified");
    peer.set_nonblocking(true).unwrap();
    let mut late_reply = [0; 1];
    assert!(
        matches!(peer.read(&mut late_reply), Err(ref err) if err.kind() == io::ErrorKind::WouldBlock),
        "late reconciliation emits no second reply",
    );
    assert_eq!(
        state
            .xi_devices
            .devices()
            .iter()
            .map(|entry| (
                entry.id,
                entry.source_id,
                entry.enabled,
                entry.session_enabled,
                entry.client_disabled,
                entry.attached_master,
            ))
            .collect::<Vec<_>>(),
        registry_before,
    );
    assert_eq!(
        (
            state.keys_down,
            state.buttons_down,
            state.key_down_by_device.clone(),
            state.xi_devices.device(device).unwrap().buttons_down,
        ),
        held_before,
    );
    assert_eq!(state.xi2_detached_masters, detached_before);
    assert_eq!(state.floating_pointer_positions, floating_before);
    assert_eq!(
        (
            state.clients[&client.0].xi2_masks.clone(),
            state.clients[&client.0].xi1_event_classes.clone(),
            state.clients[&client.0].xi1_window_event_classes.clone(),
            state.clients[&client.0].event_masks.clone(),
        ),
        selections_before,
    );
}

#[test]
fn xi_config_completion_same_client_request_order_survives_error() {
    use crate::xinput::libinput_props::{DeviceConfigError, DeviceConfigStart, DeviceConfigToken};
    use std::io::Read;
    use yserver_protocol::x11::{ClientId, SequenceNumber};

    let client = ClientId(57);
    let mut state = ServerState::new();
    let mut peer = install_xi_config_test_client(&mut state, client.0);
    let info = crate::core_loop::DeviceInfo {
        source_id: crate::xinput::InputSourceId(570),
        enabled: true,
        resume_key: None,
        capabilities: crate::xinput::InputCapabilities {
            pointer: true,
            ..Default::default()
        },
        name: "ordered config pointer".into(),
        device_node: "/dev/input/event570".into(),
        sysname: "event570".into(),
        vendor_id: 1,
        product_id: 2,
        is_touchpad: false,
        config: crate::core_loop::message::LibinputConfigSnapshot {
            accel: crate::core_loop::message::FloatSetting {
                available: true,
                current: 0.0,
                default: 0.0,
            },
            ..Default::default()
        },
    };
    let ids = state.xi_register_source(&info);
    let device_id = ids[0];
    let source = info.source_id;
    let mut inventory = InputInventory::new();
    inventory.add(info);
    let property = state.atoms.id_for("libinput Accel Speed").unwrap();
    let before = state
        .xi_devices
        .device(device_id)
        .unwrap()
        .properties
        .clone();
    let mut body = Vec::new();
    body.extend_from_slice(&device_id.to_le_bytes());
    body.push(crate::xinput::XI_PROP_MODE_REPLACE);
    body.push(32);
    body.extend_from_slice(&property.0.to_le_bytes());
    body.extend_from_slice(&state.float_atom.0.to_le_bytes());
    body.extend_from_slice(&1u32.to_le_bytes());
    body.extend_from_slice(&0.5_f32.to_le_bytes());
    let first = DeferredRequest {
        id: client,
        sequence: SequenceNumber(1),
        accepted_at: None,
        header: yserver_protocol::x11::RequestHeader {
            opcode: 137,
            data: 57,
            length_units: 6,
        },
        body,
        attached_fd: None,
    };

    let token = DeviceConfigToken(570);
    let mut backend = crate::backend::recording::RecordingBackend::new();
    backend
        .device_config_start_results
        .push_back(Ok(DeviceConfigStart::Pending(token)));
    let mut pending = PendingBackendRequests::default();
    let mut lane = XiConfigLane::default();
    let mut reset_trigger = ResetTrigger::new(ResetPolicy::NoReset);
    let mut requests_this_iter = 0;
    let mut request_budget = 10;
    process_one_request(
        &mut state,
        &mut backend,
        &mut inventory,
        &mut LoopTelemetry::default(),
        &mut pending,
        &mut lane,
        &mut reset_trigger,
        Generation::default(),
        &mut requests_this_iter,
        &mut request_budget,
        first,
    );
    assert!(pending.client_is_blocked(client));
    assert!(!lane.is_empty());
    assert_eq!(backend.started_device_configs.len(), 1);
    assert_eq!(
        state.xi_devices.device(device_id).unwrap().properties,
        before
    );

    let mut later = DeferredRequest {
        id: client,
        sequence: SequenceNumber(2),
        accepted_at: None,
        header: yserver_protocol::x11::RequestHeader {
            opcode: 43, // GetInputFocus reply is easy to identify.
            data: 0,
            length_units: 1,
        },
        body: Vec::new(),
        attached_fd: None,
    };
    let mut deferred = FairRequestQueue::default();
    deferred.push_back(later);
    assert!(deferred.pop_front_unblocked(&pending, &state).is_none());
    peer.set_nonblocking(true).unwrap();
    let mut bytes = [0u8; 64];
    assert!(
        matches!(peer.read(&mut bytes), Err(ref error) if error.kind() == io::ErrorKind::WouldBlock)
    );
    peer.set_nonblocking(false).unwrap();

    dispatch_device_config_result(
        &mut state,
        &mut backend,
        &mut inventory,
        &mut pending,
        &mut lane,
        &mut reset_trigger,
        Message::DeviceConfigResult {
            token,
            source,
            result: Err(DeviceConfigError::Invalid),
        },
        Generation::default(),
    );
    assert!(!pending.client_is_blocked(client));
    later = deferred
        .pop_front_unblocked(&pending, &state)
        .expect("later request unblocks after the earlier error");
    process_one_request(
        &mut state,
        &mut backend,
        &mut inventory,
        &mut LoopTelemetry::default(),
        &mut pending,
        &mut lane,
        &mut reset_trigger,
        Generation::default(),
        &mut requests_this_iter,
        &mut request_budget,
        later,
    );

    let mut wire = [0u8; 64];
    peer.read_exact(&mut wire).unwrap();
    assert_eq!(wire[0], 0, "first packet is the earlier request's X_Error");
    assert_eq!(wire[1], yserver_protocol::x11::error::BAD_VALUE);
    assert_eq!(u16::from_le_bytes([wire[2], wire[3]]), 1);
    assert_eq!(u16::from_le_bytes([wire[8], wire[9]]), 57);
    assert_eq!(wire[10], 137);
    assert_eq!(wire[32], 1, "second packet answers GetInputFocus");
    assert_eq!(u16::from_le_bytes([wire[34], wire[35]]), 2);
    assert_eq!(
        state.xi_devices.device(device_id).unwrap().properties,
        before
    );
    assert_eq!(inventory.get(source).unwrap().config.accel.current, 0.0);
    assert!(lane.is_empty());
    assert!(pending.is_empty());
}

#[test]
fn xi_vt_release_config_write_applied_reply_precedes_release_completion() {
    // Mutation killed: finish VT release before the pause barrier drains
    // the in-flight XI config, leaving the client blocked across release.
    use crate::xinput::libinput_props::{DeviceConfigStart, DeviceConfigToken};
    use std::io::Read;

    let client = yserver_protocol::x11::ClientId(58);
    let source = crate::xinput::InputSourceId(580);
    let (mut state, mut peer, _fixture_inventory, info, device) =
        xi_vt_config_fixture(client.0, source);
    let mut backend = crate::backend::recording::RecordingBackend::new();
    backend.vt_switching_armed = true;
    backend.vt_release_pause_queued = true;
    backend.vt_release_probe_client = Some(client.0);
    backend.vt_release_probe_source = Some(source);
    backend
        .device_config_start_results
        .push_back(Ok(DeviceConfigStart::Pending(DeviceConfigToken(580))));
    let registry_before: Vec<_> = state
        .xi_devices
        .devices()
        .iter()
        .map(|entry| {
            (
                entry.id,
                entry.source_id,
                entry.enabled,
                entry.session_enabled,
                entry.client_disabled,
                entry.attached_master,
            )
        })
        .collect();
    let property_maps_before: Vec<_> = state
        .xi_devices
        .devices()
        .iter()
        .map(|entry| (entry.id, entry.properties.clone()))
        .collect();
    let held_before = (
        state.keys_down,
        state.buttons_down,
        state.key_down_by_device.clone(),
        state.xi_devices.device(device).unwrap().buttons_down,
    );
    let detached_before = state.xi2_detached_masters.clone();
    let floating_before = state.floating_pointer_positions.clone();
    let selections_before = (
        state.clients[&client.0].xi2_masks.clone(),
        state.clients[&client.0].xi1_event_classes.clone(),
        state.clients[&client.0].xi1_window_event_classes.clone(),
        state.clients[&client.0].event_masks.clone(),
    );
    let request = xi_vt_accel_request(client, 1, device, &state, 0.5);
    run_core_for_vt_test(&mut state, &mut backend, |sender, requests| {
        sender
            .send(Message::HostInput(HostInputEvent::DeviceAdded(info)))
            .unwrap();
        send_vt_test_request(requests, request);
        sender.send(Message::VtRelease).unwrap();
        send_vt_test_request(requests, xi_vt_following_focus_request(client));
        sender
            .send(Message::DeviceConfigResult {
                token: DeviceConfigToken(580),
                source,
                result: Ok(()),
            })
            .unwrap();
        sender.send(Message::InputPaused).unwrap();
        sender.send(Message::Shutdown).unwrap();
    });
    assert!(
        backend.vt_release_wire_visible_before_finish,
        "runner must write the following reply before VT release finishes",
    );
    assert_eq!(
        backend.vt_release_inventory_accel_before_finish,
        Some(0.5),
        "the runner inventory carries the applied value at the release boundary",
    );
    let mut reply = [0; 32];
    peer.read_exact(&mut reply).unwrap();
    assert_eq!(reply[0], 1);
    assert_eq!(u16::from_le_bytes([reply[2], reply[3]]), 2);
    assert!(
        backend
            .vt_release_finished
            .load(std::sync::atomic::Ordering::SeqCst)
    );
    assert_eq!(backend.started_device_configs.len(), 1);
    assert_eq!(
        state
            .xi_devices
            .devices()
            .iter()
            .map(|entry| (
                entry.id,
                entry.source_id,
                entry.enabled,
                entry.session_enabled,
                entry.client_disabled,
                entry.attached_master,
            ))
            .collect::<Vec<_>>(),
        registry_before,
    );
    for (id, before) in property_maps_before {
        let after = &state.xi_devices.device(id).unwrap().properties;
        if id != device {
            assert_eq!(*after, before);
            continue;
        }
        assert_eq!(after.len(), before.len());
        for (atom, old_value) in before {
            if atom == state.atoms.id_for("libinput Accel Speed").unwrap() {
                assert_eq!(after.get(&atom).unwrap().data, 0.5_f32.to_le_bytes());
            } else {
                assert_eq!(after.get(&atom), Some(&old_value));
            }
        }
    }
    assert_eq!(
        (
            state.keys_down,
            state.buttons_down,
            state.key_down_by_device.clone(),
            state.xi_devices.device(device).unwrap().buttons_down,
        ),
        held_before,
    );
    assert_eq!(state.xi2_detached_masters, detached_before);
    assert_eq!(state.floating_pointer_positions, floating_before);
    assert_eq!(
        (
            state.clients[&client.0].xi2_masks.clone(),
            state.clients[&client.0].xi1_event_classes.clone(),
            state.clients[&client.0].xi1_window_event_classes.clone(),
            state.clients[&client.0].event_masks.clone(),
        ),
        selections_before,
    );
}

#[test]
fn xi_vt_release_config_write_retired_handle_bad_match_has_no_resume_reply() {
    // Mutation killed: park SourceGone until source rebind (or expose it as
    // BadDevice), allowing the pre-pause property write to answer late.
    use crate::xinput::libinput_props::{DeviceConfigStart, DeviceConfigToken};
    use std::io::Read;

    let client = yserver_protocol::x11::ClientId(59);
    let source = crate::xinput::InputSourceId(590);
    let (mut state, mut peer, _fixture_inventory, info, device) =
        xi_vt_config_fixture(client.0, source);
    let mut backend = crate::backend::recording::RecordingBackend::new();
    backend.vt_switching_armed = true;
    backend.vt_release_pause_queued = true;
    backend.vt_release_probe_client = Some(client.0);
    backend.vt_release_probe_source = Some(source);
    backend
        .device_config_start_results
        .push_back(Ok(DeviceConfigStart::Pending(DeviceConfigToken(590))));
    let request = xi_vt_accel_request(client, 1, device, &state, 0.75);
    let property_maps_before: Vec<_> = state
        .xi_devices
        .devices()
        .iter()
        .map(|entry| (entry.id, entry.properties.clone()))
        .collect();
    let inventory_accel_before = 0.0;
    let registry_before: Vec<_> = state
        .xi_devices
        .devices()
        .iter()
        .map(|entry| {
            (
                entry.id,
                entry.source_id,
                entry.enabled,
                entry.session_enabled,
                entry.client_disabled,
                entry.attached_master,
            )
        })
        .collect();
    let held_before = (
        state.keys_down,
        state.buttons_down,
        state.key_down_by_device.clone(),
    );
    let detached_before = state.xi2_detached_masters.clone();
    let floating_before = state.floating_pointer_positions.clone();
    let selections_before = (
        state.clients[&client.0].xi2_masks.clone(),
        state.clients[&client.0].xi1_event_classes.clone(),
        state.clients[&client.0].xi1_window_event_classes.clone(),
        state.clients[&client.0].event_masks.clone(),
    );

    run_core_for_vt_test(&mut state, &mut backend, |sender, requests| {
        sender
            .send(Message::HostInput(HostInputEvent::DeviceAdded(
                info.clone(),
            )))
            .unwrap();
        send_vt_test_request(requests, request);
        sender.send(Message::VtRelease).unwrap();
        sender
            .send(Message::DeviceConfigResult {
                token: DeviceConfigToken(590),
                source,
                result: Err(crate::xinput::libinput_props::DeviceConfigError::SourceGone),
            })
            .unwrap();
        sender.send(Message::InputPaused).unwrap();
        sender.send(Message::VtAcquire).unwrap();
        sender
            .send(Message::HostInput(HostInputEvent::DeviceResumed(info)))
            .unwrap();
        sender.send(Message::Shutdown).unwrap();
    });
    assert!(backend.vt_release_wire_visible_before_finish);
    assert!(
        backend
            .vt_release_finished
            .load(std::sync::atomic::Ordering::SeqCst)
    );
    let mut error = [0; 32];
    peer.read_exact(&mut error).unwrap();
    assert_eq!(error[0], 0);
    assert_eq!(error[1], yserver_protocol::x11::error::BAD_MATCH);
    for (id, properties) in property_maps_before {
        assert_eq!(
            state.xi_devices.device(id).unwrap().properties,
            properties,
            "failed SourceGone does not alter any device property map",
        );
    }
    assert_eq!(
        backend.vt_release_inventory_accel_before_finish,
        Some(inventory_accel_before),
    );
    peer.set_nonblocking(true).unwrap();
    let mut late_reply = [0; 1];
    assert!(
        matches!(peer.read(&mut late_reply), Err(ref err) if err.kind() == io::ErrorKind::WouldBlock),
        "resume must not emit a second or delayed answer for the pre-pause write",
    );
    assert_eq!(
        state
            .xi_devices
            .devices()
            .iter()
            .map(|entry| (
                entry.id,
                entry.source_id,
                entry.enabled,
                entry.session_enabled,
                entry.client_disabled,
                entry.attached_master,
            ))
            .collect::<Vec<_>>(),
        registry_before,
    );
    assert_eq!(
        (
            state.keys_down,
            state.buttons_down,
            state.key_down_by_device.clone(),
        ),
        held_before,
    );
    assert_eq!(state.xi2_detached_masters, detached_before);
    assert_eq!(state.floating_pointer_positions, floating_before);
    assert_eq!(
        (
            state.clients[&client.0].xi2_masks.clone(),
            state.clients[&client.0].xi1_event_classes.clone(),
            state.clients[&client.0].xi1_window_event_classes.clone(),
            state.clients[&client.0].event_masks.clone(),
        ),
        selections_before,
    );
}

/// #132: `xrandr --dpi` (RRSetScreenSize with the same pixels, new mm)
/// reaches NEW clients' setup reply, as Xorg's `pScreen->mmWidth`.
/// Measured in vng (tools/vng-scenarios/xrandr-dpi.sh): 1280x800 at
/// `--dpi 108` → 301x188 mm on Xorg 21.1.24 and on yserver.
#[test]
fn setup_allocate_reports_randr_screen_mm() {
    let mut state = ServerState::new();
    let (w, h) = (state.randr.screen_width, state.randr.screen_height);
    state.randr.set_logical_size(w, h, 301, 188);
    let (tx, rx) = crossbeam_channel::bounded(1);
    handle_setup_allocate(&mut state, yserver_protocol::x11::ClientId(1), tx);
    let resp = rx.try_recv().expect("setup allocate response");
    assert_eq!((resp.screen_width_mm, resp.screen_height_mm), (301, 188));
}
use std::os::unix::net::UnixStream;

#[test]
fn listener_accept_budget_leaves_flood_backlog_for_next_turn() {
    let path = std::env::temp_dir().join(format!("yserver-accept-budget-{}", std::process::id()));
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let peers: Vec<_> = (0..33)
        .map(|_| UnixStream::connect(&path).unwrap())
        .collect();
    std::fs::remove_file(path).unwrap();
    let listener = Listener::Unix(listener);
    let alloc = ClientIdAllocator::new();
    let (_poll, sender, _rx) = channel().unwrap();
    let registry = setup_thread::make_registry();

    accept_pending(&listener, &alloc, &sender, &registry, &AuthState::new(None));
    let accepted = alloc.peek().0 - 1;
    setup_thread::shutdown_all(&registry);
    drop(peers);
    assert_eq!(
        accepted, 16,
        "one listener must yield after its accept budget"
    );
}

#[test]
fn ready_listeners_round_robin_under_accept_flood() {
    for tcp_count in [1, 33] {
        let path = std::env::temp_dir().join(format!(
            "yserver-accept-fair-{}-{tcp_count}",
            std::process::id()
        ));
        let unix = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let tcp = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let unix_peers: Vec<_> = (0..33)
            .map(|_| UnixStream::connect(&path).unwrap())
            .collect();
        let tcp_peers: Vec<_> = (0..tcp_count)
            .map(|_| std::net::TcpStream::connect(tcp.local_addr().unwrap()).unwrap())
            .collect();
        std::fs::remove_file(path).unwrap();
        let listeners = [Listener::Unix(unix), Listener::Tcp(tcp)];
        for listener in &listeners {
            listener.set_nonblocking(true).unwrap();
        }
        let alloc = ClientIdAllocator::new();
        let (_poll, sender, _rx) = channel().unwrap();
        let registry = setup_thread::make_registry();
        let mut readiness = ListenerReadiness::new(2);
        readiness.mark_ready(0);
        readiness.mark_ready(1);
        readiness.accept_ready(
            &listeners,
            &alloc,
            &sender,
            &registry,
            &AuthState::new(None),
        );
        {
            let clients = registry.lock().unwrap();
            assert!(matches!(
                clients[&yserver_protocol::x11::ClientId(1)],
                Transport::Unix(_)
            ));
            assert!(
                matches!(
                    clients[&yserver_protocol::x11::ClientId(17)],
                    Transport::Tcp(_)
                ),
                "TCP must be accepted within one Unix accept budget, even during a flood"
            );
            assert_eq!(clients.len(), if tcp_count == 1 { 17 } else { 32 });
        }
        // No fresh readiness marks. Queued accepts must persist, and the
        // second round must start at TCP rather than repeat Unix-first.
        readiness.accept_ready(
            &listeners,
            &alloc,
            &sender,
            &registry,
            &AuthState::new(None),
        );
        if tcp_count == 33 {
            let clients = registry.lock().unwrap();
            assert!(matches!(
                clients[&yserver_protocol::x11::ClientId(33)],
                Transport::Tcp(_)
            ));
            assert!(matches!(
                clients[&yserver_protocol::x11::ClientId(49)],
                Transport::Unix(_)
            ));
        }
        readiness.accept_ready(
            &listeners,
            &alloc,
            &sender,
            &registry,
            &AuthState::new(None),
        );
        assert!(
            !readiness.has_pending(),
            "WouldBlock clears retained readiness"
        );
        assert_eq!(alloc.peek().0 - 1, 33 + tcp_count);
        setup_thread::shutdown_all(&registry);
        drop((unix_peers, tcp_peers));
    }
}

#[test]
fn listener_backlog_completes_without_a_fresh_readiness_edge() {
    use std::io::{Read, Write};
    let path = std::env::temp_dir().join(format!("yserver-accept-edge-{}", std::process::id()));
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    // Queue more than two accept budgets before the listener is registered:
    // there is one readiness edge, with no later connection to wake it.
    let mut peers: Vec<_> = (0..33)
        .map(|_| {
            let mut peer = UnixStream::connect(&path).unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            peer.write_all(&[b'l', 0, 11, 0, 0, 0, 0, 0, 0, 0, 0, 0])
                .unwrap();
            peer
        })
        .collect();
    std::fs::remove_file(path).unwrap();
    let (poll, sender, rx) = channel().unwrap();
    let sender_for_core = sender.clone_handle();
    let handle = std::thread::spawn(move || {
        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();
        run_core(
            poll,
            rx,
            sender_for_core,
            &mut state,
            &mut backend,
            [Listener::Unix(listener)],
            &ClientIdAllocator::new(),
            AuthState::new(None),
            ResetPolicy::NoReset,
            None,
        )
    });
    let result: io::Result<()> = (|| {
        for peer in &mut peers {
            let mut header = [0; 8];
            peer.read_exact(&mut header)?;
            assert_eq!(header[0], 1);
            let len = usize::from(u16::from_le_bytes([header[6], header[7]])) * 4;
            peer.read_exact(&mut vec![0; len])?;
        }
        Ok(())
    })();
    sender.send(Message::Shutdown).unwrap();
    handle.join().unwrap().unwrap();
    result.expect("all queued connections must finish setup without another accept edge");
}

#[test]
fn randr_change_fanout_orders_screen_then_all_crtcs_then_all_outputs() {
    use crate::server::ClientState;
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        io::Read,
        os::unix::net::UnixStream,
        sync::{Arc, Mutex, atomic::AtomicU16},
    };
    use yserver_protocol::x11::{ClientByteOrder, randr as x11randr};

    let mut state = ServerState::new();
    let first = state.randr.outputs[0].clone();
    let mut second = first.clone();
    second.name = "HDMI-A-1".into();
    second.output_id = first.output_id + 10;
    second.crtc_id = first.crtc_id + 10;
    state.randr.outputs.push(second.clone());

    let (mut peer, writer) = UnixStream::pair().unwrap();
    writer.set_nonblocking(true).unwrap();
    state.clients.insert(
        7,
        ClientState {
            writer: Arc::new(Mutex::new(crate::transport::Transport::Unix(writer))),
            byte_order: ClientByteOrder::LittleEndian,
            last_sequence: Arc::new(AtomicU16::new(9)),
            resource_id_base: 0,
            resource_id_mask: 0,
            event_masks: HashMap::new(),
            save_set: HashSet::new(),
            big_requests_enabled: false,
            xi2_masks: HashMap::new(),
            xi1_event_classes: HashSet::new(),
            xi1_window_event_classes: HashMap::new(),
            outbound: VecDeque::new(),
            watching_writable: false,
            write_failed: false,
            focused_window: crate::resources::ROOT_WINDOW,
            reader_control: None,
            is_local: true,
            fd_passing: true,
        },
    );
    state.randr_select_masks.insert(
        (7, crate::resources::ROOT_WINDOW),
        x11randr::NOTIFY_MASK_SCREEN_CHANGE
            | x11randr::NOTIFY_MASK_CRTC_CHANGE
            | x11randr::NOTIFY_MASK_OUTPUT_CHANGE,
    );

    emit_randr_connector_change_notifications(
        &mut state,
        &[(first.output_id, first.crtc_id, first.mode_id)],
        &[(second.output_id, second.crtc_id, second.mode_id)],
    );

    let mut wire = [0; 96];
    peer.read_exact(&mut wire).unwrap();
    const RANDR_FIRST_EVENT: u8 = 89;
    assert_eq!(wire[0] & 0x7f, RANDR_FIRST_EVENT);
    assert_eq!(wire[32] & 0x7f, RANDR_FIRST_EVENT + 1);
    assert_eq!(wire[33], x11randr::NOTIFY_CRTC_CHANGE);
    assert_eq!(wire[64] & 0x7f, RANDR_FIRST_EVENT + 1);
    assert_eq!(wire[65], x11randr::NOTIFY_OUTPUT_CHANGE);
    assert_eq!(
        u32::from_le_bytes(wire[44..48].try_into().unwrap()),
        first.crtc_id
    );
    assert_eq!(
        u32::from_le_bytes(wire[80..84].try_into().unwrap()),
        second.output_id
    );
}

/// The count cap alone was sized on an assumption of ~0.25 ms per
/// request (`MAX_REQUESTS_PER_ITER`'s own doc comment). Measured on
/// silence under MATE + adapta-nokto during a window drag, single
/// requests reach 44-50 ms (`longest=op70:44.23ms`), so 32 of them
/// is ~1.4 s in one iteration — during which host input and backend-fd
/// readiness sit undelivered and the cursor visibly stalls (`gap_max`
/// 225-360 ms). These pin the deadline half of the budget.
#[test]
fn budget_not_exhausted_when_both_count_and_time_remain() {
    assert!(!budget_exhausted(31, Duration::from_millis(1)));
}

#[test]
fn budget_exhausted_when_count_runs_out() {
    assert!(budget_exhausted(0, Duration::from_millis(0)));
}

/// THE FIX: slow requests must stop the drain even with count left.
#[test]
fn budget_exhausted_when_deadline_passed_despite_count_remaining() {
    assert!(budget_exhausted(31, REQUEST_TIME_BUDGET));
    assert!(budget_exhausted(
        31,
        REQUEST_TIME_BUDGET + Duration::from_millis(40)
    ));
}

/// One 44 ms request must not authorise 31 more. This is the
/// 1.4 s-iteration case the count-only cap allowed.
#[test]
fn budget_stops_after_a_single_overrunning_request() {
    let elapsed_after_one_slow_request = Duration::from_millis(44);
    assert!(budget_exhausted(
        MAX_REQUESTS_PER_ITER - 1,
        elapsed_after_one_slow_request
    ));
}

/// Forward progress: the budget is checked before each request with
/// elapsed measured from the top of the iteration, so the first
/// request of an iteration always runs. Without this the loop could
/// livelock without draining anything.
#[test]
fn budget_permits_the_first_request_of_an_iteration() {
    assert!(!budget_exhausted(MAX_REQUESTS_PER_ITER, Duration::ZERO));
}

/// The fast path must be unchanged: 32 × 0.25 ms = 8 ms, so a
/// well-behaved burst still exhausts on count, not on time.
#[test]
fn typical_fast_requests_still_exhaust_on_count_first() {
    let typical = Duration::from_micros(250);
    let elapsed_at_cap = typical * u32::try_from(MAX_REQUESTS_PER_ITER).unwrap();
    assert!(
        elapsed_at_cap <= REQUEST_TIME_BUDGET,
        "time budget must not bind before the count cap for ~0.25ms requests \
             (elapsed_at_cap={elapsed_at_cap:?}, budget={REQUEST_TIME_BUDGET:?})"
    );
}

#[test]
fn export_holders_report_is_gated_paced_and_rechecks_after_change() {
    let t0 = Instant::now();
    let off = LoopTelemetry::default();
    assert!(!off.export_holders_due(t0));
    let mut on = LoopTelemetry {
        enabled: true,
        ..LoopTelemetry::default()
    };
    assert!(on.export_holders_due(t0));
    on.note_export_holders(t0, true);
    assert!(!on.export_holders_due(t0 + Duration::from_millis(999)));
    assert!(on.export_holders_due(t0 + TELEMETRY_EMIT_INTERVAL));
    assert_eq!(
        on.export_holders_deadline(),
        Some(t0 + TELEMETRY_EMIT_INTERVAL)
    );
    on.note_export_holders(t0 + TELEMETRY_EMIT_INTERVAL, false);
    assert_eq!(on.export_holders_deadline(), None);
}

#[test]
fn loop_telemetry_attributes_burst_depth_age_and_sequence_boundary() {
    let client = yserver_protocol::x11::ClientId(17);
    let other = yserver_protocol::x11::ClientId(23);
    let mut telemetry = LoopTelemetry {
        enabled: true,
        ..LoopTelemetry::default()
    };

    telemetry.record_channel_drain(65_541, &HashMap::from([(client, 65_536), (other, 5)]));
    telemetry.record_request_accepted(client, yserver_protocol::x11::SequenceNumber(0xffff));
    telemetry.record_request_accepted(client, yserver_protocol::x11::SequenceNumber(0x0000));
    telemetry.record_deferred_push(client);
    telemetry.record_deferred_push(client);
    telemetry.record_deferred_push(other);
    telemetry.record_deferred_pop(client);
    telemetry.record_request(
        client,
        133,
        26,
        Duration::from_micros(20),
        Duration::from_millis(1_750),
    );

    assert_eq!(telemetry.channel_request_batch_max, 65_541);
    assert_eq!(telemetry.channel_client_batch_max, (17, 65_536));
    assert_eq!(telemetry.deferred_current, 2);
    assert_eq!(telemetry.max_deferred_depth, 3);
    let client_stats = &telemetry.clients[&client];
    assert_eq!(client_stats.deferred_current, 1);
    assert_eq!(client_stats.deferred_max, 2);
    assert_eq!(client_stats.accepted, 2);
    assert_eq!(client_stats.request_age_max, Duration::from_millis(1_750));
    assert_eq!(client_stats.requests_by_opcode[&(133, Some(26))], 1);
    assert_eq!(client_stats.sequence_ffff, 1);
    assert_eq!(client_stats.sequence_zero, 1);
}

fn deferred_request(id: u32, opcode: u8) -> DeferredRequest {
    DeferredRequest {
        id: yserver_protocol::x11::ClientId(id),
        sequence: yserver_protocol::x11::SequenceNumber(1),
        accepted_at: None,
        header: yserver_protocol::x11::RequestHeader {
            opcode,
            data: 0,
            length_units: 1,
        },
        body: Vec::new(),
        attached_fd: None,
    }
}

#[test]
fn server_grab_blocks_only_non_owner_requests() {
    let mut state = ServerState::new();
    state.server_grab_owner = Some(yserver_protocol::x11::ClientId(7));

    assert!(!blocked_by_server_grab(&state, &deferred_request(7, 127)));
    assert!(blocked_by_server_grab(&state, &deferred_request(8, 127)));
    state.server_grab_owner = None;
    assert!(!blocked_by_server_grab(&state, &deferred_request(8, 127)));
}

#[test]
fn released_server_grab_waiters_join_round_robin_in_waiter_order() {
    let mut deferred = FairRequestQueue::default();
    deferred.push_back(deferred_request(9, 90));
    let mut waiters = VecDeque::from([deferred_request(2, 20), deferred_request(3, 30)]);

    release_server_grab_waiters(&mut deferred, &mut waiters, &mut LoopTelemetry::default());

    let mut order = Vec::new();
    while let Some(req) = deferred.pop_front() {
        order.push((req.id.0, req.header.opcode));
    }
    assert_eq!(order, [(9, 90), (2, 20), (3, 30)]);
    assert!(waiters.is_empty());
}

#[test]
fn released_server_grab_prefix_stays_ahead_of_same_client_suffix() {
    let mut deferred = FairRequestQueue::default();
    // Requests 16 and 17 were popped and parked while another client held
    // GrabServer. Requests 18 and 19 were already the remaining suffix in
    // the fair queue. The old release path appended the parked prefix and
    // dispatched 18,19,16,17, corrupting Xlib/XCB sequence tracking.
    deferred.push_back(deferred_request(66, 18));
    deferred.push_back(deferred_request(66, 19));
    deferred.push_back(deferred_request(12, 90));
    let mut waiters = VecDeque::from([deferred_request(66, 16), deferred_request(66, 17)]);

    release_server_grab_waiters(&mut deferred, &mut waiters, &mut LoopTelemetry::default());

    let mut client_66_order = Vec::new();
    while let Some(req) = deferred.pop_front() {
        if req.id.0 == 66 {
            client_66_order.push(req.header.opcode);
        }
    }
    assert_eq!(client_66_order, [16, 17, 18, 19]);
    assert!(waiters.is_empty());
}

#[test]
fn fair_queue_round_robins_clients_and_preserves_each_clients_order() {
    let mut queue = FairRequestQueue::default();
    queue.push_back(deferred_request(57, 1));
    queue.push_back(deferred_request(57, 2));
    queue.push_back(deferred_request(12, 10));
    queue.push_back(deferred_request(57, 3));
    queue.push_back(deferred_request(12, 11));

    let mut order = Vec::new();
    while let Some(req) = queue.pop_front() {
        order.push((req.id.0, req.header.opcode));
    }
    assert_eq!(order, [(57, 1), (12, 10), (57, 2), (12, 11), (57, 3)]);
    assert!(queue.is_empty());
}

#[test]
fn pending_backend_request_blocks_only_its_clients_fifo() {
    use yserver_protocol::x11::ClientByteOrder;

    let blocked = yserver_protocol::x11::ClientId(57);
    let other = yserver_protocol::x11::ClientId(12);
    let token = CrtcConfigToken(91);
    let mut pending = PendingBackendRequests::default();
    pending
        .park_crtc(ParkedCrtcConfig {
            client_id: blocked,
            sequence: yserver_protocol::x11::SequenceNumber(1),
            continuation: PendingCrtcConfig {
                token,
                completion: crate::core_loop::process_request::CrtcConfigCompletion {
                    output_id: 1,
                    set_time: 0,
                    output_bbox_before: None,
                    byte_order: ClientByteOrder::LittleEndian,
                    apply_transform: None,
                    apply_rotation: None,
                    reply: crate::core_loop::process_request::CrtcConfigReply::CrtcConfig,
                },
            },
            request_wire_bytes: 28,
        })
        .unwrap();

    let state = ServerState::new();
    let mut queue = FairRequestQueue::default();
    queue.push_back(deferred_request(blocked.0, 2));
    queue.push_back(deferred_request(other.0, 10));
    queue.push_back(deferred_request(blocked.0, 3));

    let runnable = queue.pop_front_unblocked(&pending, &state).unwrap();
    assert_eq!((runnable.id, runnable.header.opcode), (other, 10));
    assert!(
        queue.pop_front_unblocked(&pending, &state).is_none(),
        "later requests from the pending client must stay parked"
    );
    assert!(
        !queue.has_runnable(&pending, &state),
        "a blocked-only queue must not force a zero-timeout poll spin"
    );

    pending.take_crtc(token).unwrap();
    let first = queue.pop_front_unblocked(&pending, &state).unwrap();
    let second = queue.pop_front_unblocked(&pending, &state).unwrap();
    assert_eq!((first.header.opcode, second.header.opcode), (2, 3));
}

/// A client suspended by SYNC Await (Xorg `IgnoreClient`) keeps its
/// later requests queued in order while other clients run, the queue
/// does not spin the poll on it, and resuming releases them in order.
#[test]
fn sync_await_suspends_only_the_awaiting_client() {
    let awaiting = yserver_protocol::x11::ClientId(57);
    let other = yserver_protocol::x11::ClientId(12);
    let pending = PendingBackendRequests::default();
    let mut state = ServerState::new();
    state
        .sync_awaits
        .insert(awaiting.0, crate::server::SyncAwait::default());

    let mut queue = FairRequestQueue::default();
    queue.push_back(deferred_request(awaiting.0, 2));
    queue.push_back(deferred_request(other.0, 10));
    queue.push_back(deferred_request(awaiting.0, 3));

    let runnable = queue.pop_front_unblocked(&pending, &state).unwrap();
    assert_eq!((runnable.id, runnable.header.opcode), (other, 10));
    assert!(queue.pop_front_unblocked(&pending, &state).is_none());
    assert!(!queue.has_runnable(&pending, &state));

    state.sync_awaits.remove(&awaiting.0);
    assert!(queue.has_runnable(&pending, &state));
    let first = queue.pop_front_unblocked(&pending, &state).unwrap();
    let second = queue.pop_front_unblocked(&pending, &state).unwrap();
    assert_eq!((first.header.opcode, second.header.opcode), (2, 3));
}

/// A RECORD data connection whose stream write failed is no longer
/// recording, but its pipelined requests must not run before the core
/// loop disconnects it.
#[test]
fn failed_record_client_keeps_its_queued_requests_parked() {
    let failed = yserver_protocol::x11::ClientId(57);
    let other = yserver_protocol::x11::ClientId(12);
    let pending = PendingBackendRequests::default();
    let mut state = ServerState::new();
    state.record.fail_recorder_for_test(failed);
    assert!(!crate::core_loop::record::client_is_recording(
        &state, failed
    ));

    let mut queue = FairRequestQueue::default();
    queue.push_back(deferred_request(failed.0, 2));
    queue.push_back(deferred_request(other.0, 10));

    let runnable = queue.pop_front_unblocked(&pending, &state).unwrap();
    assert_eq!((runnable.id, runnable.header.opcode), (other, 10));
    assert!(queue.pop_front_unblocked(&pending, &state).is_none());
    assert!(!queue.has_runnable(&pending, &state));
    assert_eq!(
        crate::core_loop::record::take_failed_recorders(&mut state),
        [failed]
    );
}

/// A client a write failed on must not run its pipelined requests
/// (GrabServer, SetCloseDownMode…) before the core loop disconnects
/// it; other clients keep running.
#[test]
fn failed_writer_keeps_its_queued_requests_parked() {
    let failed = yserver_protocol::x11::ClientId(57);
    let other = yserver_protocol::x11::ClientId(12);
    let pending = PendingBackendRequests::default();
    let mut state = ServerState::new();
    for id in [failed.0, other.0] {
        let (a, _) = std::os::unix::net::UnixStream::pair().unwrap();
        state.clients.insert(id, unix_client(a, id, 0));
    }
    state.clients.get_mut(&failed.0).unwrap().write_failed = true;

    let mut queue = FairRequestQueue::default();
    queue.push_back(deferred_request(failed.0, 2));
    queue.push_back(deferred_request(other.0, 10));

    let runnable = queue.pop_front_unblocked(&pending, &state).unwrap();
    assert_eq!((runnable.id, runnable.header.opcode), (other, 10));
    assert!(queue.pop_front_unblocked(&pending, &state).is_none());
    assert!(!queue.has_runnable(&pending, &state));
}

#[test]
fn ready_crtc_completion_replies_unblocks_and_returns_reader_credit() {
    use crate::{backend::recording::RecordingBackend, server::ClientState};
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        io::Read,
        os::unix::net::UnixStream,
        sync::{Arc, Mutex, atomic::AtomicU16},
    };
    use yserver_protocol::x11::{ClientByteOrder, ClientId, SequenceNumber};

    let client_id = ClientId(57);
    let token = CrtcConfigToken(92);
    let (mut peer, writer) = UnixStream::pair().unwrap();
    writer.set_nonblocking(true).unwrap();
    let (control_tx, control_rx) = crossbeam_channel::unbounded();
    let mut state = ServerState::new();
    state.clients.insert(
        client_id.0,
        ClientState {
            writer: Arc::new(Mutex::new(crate::transport::Transport::Unix(writer))),
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
            focused_window: crate::resources::ROOT_WINDOW,
            reader_control: Some(control_tx),
            is_local: true,
            fd_passing: true,
        },
    );

    let completion = crate::core_loop::process_request::CrtcConfigCompletion {
        output_id: state.randr.outputs[0].output_id,
        set_time: 123,
        output_bbox_before: enabled_output_bbox(&state),
        byte_order: ClientByteOrder::LittleEndian,
        apply_transform: None,
        apply_rotation: None,
        reply: crate::core_loop::process_request::CrtcConfigReply::CrtcConfig,
    };
    let continuation = PendingCrtcConfig {
        token,
        completion: completion.clone(),
    };
    let mut pending = PendingBackendRequests::default();
    pending
        .park_crtc(ParkedCrtcConfig {
            client_id,
            sequence: SequenceNumber(9),
            continuation,
            request_wire_bytes: 28,
        })
        .unwrap();
    let mut backend = RecordingBackend::new();
    backend.ready_crtc_configs.push(token);
    backend.crtc_config_results.insert(token, Ok(false));
    let mut xi_config_lane = XiConfigLane::default();

    drain_ready_crtc_configs(
        &mut state,
        &mut backend,
        &mut pending,
        &mut xi_config_lane,
        &mut ResetTrigger::new(ResetPolicy::NoReset),
    );

    let mut reply = [0_u8; 32];
    peer.read_exact(&mut reply).unwrap();
    assert_eq!((reply[0], reply[1]), (1, 0), "success reply, status=0");
    assert!(!pending.client_is_blocked(client_id));
    assert_eq!(backend.finished_crtc_configs, [token]);
    assert!(backend.cancelled_crtc_configs.is_empty());
    assert!(matches!(
        control_rx.try_recv(),
        Ok(crate::server::ReaderControl::GrantRequestBytes(28))
    ));

    // Disconnecting a still-pending client cancels its backend token and
    // never waits for the worker to finish.
    let cancel_token = CrtcConfigToken(93);
    pending
        .park_crtc(ParkedCrtcConfig {
            client_id,
            sequence: SequenceNumber(10),
            continuation: PendingCrtcConfig {
                token: cancel_token,
                completion,
            },
            request_wire_bytes: 28,
        })
        .unwrap();
    let mut xi_config_lane = XiConfigLane::default();
    disconnect_with_pending_cleanup(
        &mut state,
        &mut backend,
        &mut pending,
        &mut xi_config_lane,
        &mut ResetTrigger::new(ResetPolicy::NoReset),
        client_id,
    );
    assert_eq!(backend.cancelled_crtc_configs, [cancel_token]);
    assert!(!state.clients.contains_key(&client_id.0));
}

/// The grab owner can be dropped by paths that carry no release check of
/// their own — `process_disconnect` runs at two sites outside the message
/// loop (a failed outbound write, and the writable-interest reconcile).
/// The loop therefore re-checks once per iteration. This pins the state
/// that made that necessary: waiters parked while `deferred_requests` is
/// EMPTY, because the poll timeout keys off `deferred_requests` alone, so
/// a waiter left in the side queue would strand its client until
/// unrelated traffic happened to wake the loop.
#[test]
fn owner_disconnect_outside_the_message_loop_still_frees_waiters() {
    let mut state = ServerState::new();
    state.server_grab_owner = Some(yserver_protocol::x11::ClientId(1));
    let mut deferred = FairRequestQueue::default();
    let mut waiters = VecDeque::from([deferred_request(2, 20)]);

    // While the grab is held, a waiter must stay parked.
    if state.server_grab_owner.is_none() {
        release_server_grab_waiters(&mut deferred, &mut waiters, &mut LoopTelemetry::default());
    }
    assert_eq!(waiters.len(), 1, "grab still held: waiter stays parked");
    assert!(deferred.is_empty(), "nothing runnable while grabbed");

    // Owner reaped by a path with no release check of its own (this is
    // what process_disconnect does at run.rs' two non-message sites).
    state.server_grab_owner = None;

    // The per-iteration re-check must pick it up on its own.
    if state.server_grab_owner.is_none() {
        release_server_grab_waiters(&mut deferred, &mut waiters, &mut LoopTelemetry::default());
    }
    assert!(waiters.is_empty(), "released grab must free its waiters");
    assert_eq!(deferred.pop_front().map(|r| r.id.0), Some(2));
    assert!(deferred.is_empty());
}
use crate::{
    backend::recording::RecordingBackend,
    core_loop::sender::channel,
    server::{ScreenSaverActive, ServerState},
};
use std::time::Duration;

/// I5 test: `reconcile_client_writable_interest` toggles a client's
/// `watching_writable` flag in lock-step with `outbound`'s emptiness,
/// and is a no-op when nothing changed. Tests against a real
/// `mio::Registry` so the reregister error path is also exercised.
#[test]
fn reconcile_writable_interest_tracks_outbound_state() {
    use crate::server::ClientState;
    use mio::{Interest, Poll, unix::SourceFd};
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        io::{Read, Write},
        os::{fd::AsRawFd, unix::net::UnixStream},
        sync::{Arc, Mutex, atomic::AtomicU16},
    };
    use yserver_protocol::x11::{ClientByteOrder, ClientId as Cid};

    let poll = Poll::new().unwrap();
    // We just need a real fd registered with the poller.
    let (mut peer, writer) = UnixStream::pair().unwrap();
    writer.set_nonblocking(true).unwrap();
    let writer_arc = Arc::new(Mutex::new(crate::transport::Transport::Unix(writer)));
    let raw = writer_arc.lock().unwrap().as_raw_fd();
    let token = client_token(Cid(7));
    poll.registry()
        .register(&mut SourceFd(&raw), token, Interest::READABLE)
        .unwrap();

    let mut state = ServerState::new();
    state.clients.insert(
        7,
        ClientState {
            writer: writer_arc,
            byte_order: ClientByteOrder::LittleEndian,
            last_sequence: Arc::new(AtomicU16::new(0)),
            resource_id_base: 0,
            resource_id_mask: 0,
            event_masks: HashMap::new(),
            save_set: HashSet::new(),
            big_requests_enabled: false,
            xi2_masks: HashMap::new(),
            xi1_event_classes: HashSet::new(),
            xi1_window_event_classes: HashMap::new(),
            outbound: VecDeque::new(),
            watching_writable: false,
            write_failed: false,
            focused_window: crate::resources::ROOT_WINDOW,
            reader_control: None,
            is_local: true,
            fd_passing: true,
        },
    );

    // outbound is empty, watching_writable is false → no-op.
    let disc = reconcile_client_writable_interest(poll.registry(), &mut state);
    assert!(disc.is_empty());
    assert!(!state.clients[&7].watching_writable);

    // Outbound becomes non-empty AND the peer doesn't read → reconcile's
    // proactive drain attempt cannot empty it, so watching_writable
    // flips on.
    //
    // Fill the kernel buffer first so any drain attempt returns
    // WouldBlock instead of writing through to `peer`.
    //
    // Fill until the kernel actually reports WouldBlock rather than
    // writing one fixed-size buffer: the capacity is a tunable the
    // test cannot assume. On the Linux box this was reported from,
    // `net.core.wmem_default` was the stock 212992 (~228 KiB
    // absorbed), so a single 256 KiB write cleared it by only ~11%
    // and was swallowed whole where that sysctl had been raised;
    // other platforms size it differently again. The drain
    // inside reconcile then succeeded, `outbound` emptied, and
    // `watching_writable` never flipped on — #107.
    //
    // SO_SNDBUF is also raised, best-effort, so a machine with the
    // stock sysctl still exercises the large-buffer case. It is only
    // an amplifier: the kernel may clamp it (Linux) or refuse it
    // (FreeBSD ENOBUFS), and the loop below is correct either way,
    // so the result is deliberately not asserted on.
    unsafe {
        let sz: libc::c_int = 512 * 1024;
        libc::setsockopt(
            raw,
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            std::ptr::addr_of!(sz).cast(),
            u32::try_from(std::mem::size_of::<libc::c_int>()).unwrap(),
        );
    }
    let chunk = vec![0xABu8; 64 * 1024];
    let mut filled = false;
    // Bounded so a kernel that never reports WouldBlock fails the
    // assertion below instead of spinning.
    for _ in 0..1024 {
        match state.clients[&7].writer.lock().unwrap().write(&chunk) {
            Ok(0) => break,
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                filled = true;
                break;
            }
            // Anything else (EPIPE, EINTR…) is a broken fixture, not
            // a full buffer — surface it rather than folding it into
            // the generic "never reported WouldBlock" failure.
            Err(err) => panic!("unexpected error filling the send buffer: {err}"),
        }
    }
    assert!(filled, "kernel send buffer never reported WouldBlock");
    state
        .clients
        .get_mut(&7)
        .unwrap()
        .outbound
        .extend([1u8, 2, 3]);
    let disc = reconcile_client_writable_interest(poll.registry(), &mut state);
    assert!(disc.is_empty());
    assert!(state.clients[&7].watching_writable);

    // Peer drains → kernel buffer empties → drain succeeds inside reconcile,
    // outbound goes empty, watching_writable flips off.
    //
    // Read until WouldBlock for the same reason the fill loops: one
    // read of a fixed size is not guaranteed to empty the queue, and
    // leftover bytes would make reconcile's drain block again and
    // leave `outbound` non-empty.
    let mut sink = vec![0u8; 64 * 1024];
    peer.set_nonblocking(true).unwrap();
    loop {
        match peer.read(&mut sink) {
            Ok(0) => break,
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(err) => panic!("unexpected error draining the peer: {err}"),
        }
    }
    let disc = reconcile_client_writable_interest(poll.registry(), &mut state);
    assert!(disc.is_empty());
    assert!(state.clients[&7].outbound.is_empty());
    assert!(!state.clients[&7].watching_writable);

    drop(peer);
}

/// Multi-device regression: two DRM fds of the same kind must get
/// distinct poll tokens, and readiness on the second fd must carry
/// that exact fd through `Backend::on_page_flip_ready`.
#[test]
fn drm_readiness_routes_to_the_exact_backend_fd() {
    use crate::backend::{BackendFdKind, recording::RecordingBackend};
    use std::{io::Write, os::fd::AsRawFd};

    let (poll, sender, rx) = channel().unwrap();
    let sender_for_core = sender.clone_handle();
    let (drm_reader_a, _drm_writer_a) = UnixStream::pair().unwrap();
    let (drm_reader_b, mut drm_writer_b) = UnixStream::pair().unwrap();
    let drm_fd_a = drm_reader_a.as_raw_fd();
    let drm_fd_b = drm_reader_b.as_raw_fd();
    let (ready_tx, ready_rx) = crossbeam_channel::unbounded();
    let mut backend = RecordingBackend::new().with_poll_sources(
        vec![
            (drm_fd_a, BackendFdKind::Drm),
            (drm_fd_b, BackendFdKind::Drm),
        ],
        ready_tx,
    );
    let handle = std::thread::spawn(move || {
        // `RecordingBackend` intentionally stores only raw fds; keep
        // their owners alive for the duration of the core loop.
        let _drm_readers = (drm_reader_a, drm_reader_b);
        let mut state = ServerState::new();
        let alloc = ClientIdAllocator::new();
        let result = run_core(
            poll,
            rx,
            sender_for_core,
            &mut state,
            &mut backend,
            None,
            &alloc,
            AuthState::new(None),
            ResetPolicy::NoReset,
            None,
        );
        (result, backend)
    });

    drm_writer_b.write_all(&[1]).unwrap();
    assert_eq!(
        ready_rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        drm_fd_b,
        "readiness from the second DRM source must retain its fd identity"
    );
    sender.send(Message::Shutdown).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !handle.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(handle.is_finished(), "run_core did not return");
    let (result, backend) = handle.join().unwrap();
    result.unwrap();
    let dispatched_fds = backend.page_flip_fds.lock().unwrap();
    assert!(
        !dispatched_fds.is_empty(),
        "the readable DRM source must be dispatched"
    );
    assert!(
        dispatched_fds.iter().all(|fd| *fd == drm_fd_b),
        "idle DRM fd {drm_fd_a} was dispatched: {dispatched_fds:?}",
    );
}

/// A fake XDMCP manager on loopback, for the two loop-level tests
/// below. The service-level behaviour is covered in `core_loop::xdmcp`;
/// what these prove is the *plumbing* — the UDP socket really is in
/// this poll set, its readiness really is dispatched, and the reset
/// hook really runs after the new generation is installed.
#[cfg(feature = "xdmcp")]
struct XdmcpManagerFixture {
    socket: std::net::UdpSocket,
}

#[cfg(feature = "xdmcp")]
impl XdmcpManagerFixture {
    fn new() -> Self {
        let socket = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        Self { socket }
    }

    fn service(&self, once: bool) -> XdmcpService {
        use crate::core_loop::xdmcp::{XdmcpMode, XdmcpSetup};
        XdmcpService::bind(&XdmcpSetup {
            mode: XdmcpMode::Query("127.0.0.1".into()),
            port: self.socket.local_addr().unwrap().port(),
            from: Some("127.0.0.1".into()),
            class: None,
            display_id: None,
            once,
            display_number: 7,
        })
        .unwrap()
    }

    fn expect(&self, what: &str) -> (yserver_protocol::xdmcp::XdmcpMessage, std::net::SocketAddr) {
        let mut buf = [0_u8; 8192];
        let (len, from) = self
            .socket
            .recv_from(&mut buf)
            .unwrap_or_else(|e| panic!("no {what} from the display: {e}"));
        (
            yserver_protocol::xdmcp::decode_message(&buf[..len]).unwrap(),
            from,
        )
    }

    fn send(&self, to: std::net::SocketAddr, message: &yserver_protocol::xdmcp::XdmcpMessage) {
        let packet = yserver_protocol::xdmcp::encode_message(message).unwrap();
        self.socket.send_to(&packet, to).unwrap();
    }
}

/// The socket is registered with the core poller, its readiness is
/// dispatched, and a reset re-queries — from the loop, not from a
/// hand-driven service.
#[cfg(feature = "xdmcp")]
#[test]
fn the_xdmcp_socket_is_polled_and_a_reset_re_queries() {
    use crate::backend::recording::RecordingBackend;
    use yserver_protocol::xdmcp::XdmcpMessage;

    let manager = XdmcpManagerFixture::new();
    let service = manager.service(false);
    let (poll, sender, rx) = channel().unwrap();
    let sender_for_core = sender.clone_handle();
    let handle = std::thread::spawn(move || {
        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();
        let alloc = ClientIdAllocator::new();
        run_core(
            poll,
            rx,
            sender_for_core,
            &mut state,
            &mut backend,
            None,
            &alloc,
            AuthState::new_with_xdmcp(None, true),
            ResetPolicy::Reset,
            Some(service),
        )
    });

    let (query, display) = manager.expect("the startup Query");
    assert!(matches!(query, XdmcpMessage::Query { .. }), "{query:?}");

    manager.send(
        display,
        &XdmcpMessage::Willing {
            authentication_name: Vec::new(),
            hostname: b"fake-dm".to_vec(),
            status: b"willing".to_vec(),
        },
    );
    let (request, _) = manager.expect("a Request");
    assert!(
        matches!(request, XdmcpMessage::Request { .. }),
        "the loop did not dispatch the socket's readiness: {request:?}"
    );

    // A forced reset (the SIGHUP path) crosses the boundary; the XDMCP
    // hook then re-queries on the NEW generation.
    sender.send(Message::ResetRequested).unwrap();
    let (requery, _) = manager.expect("a re-query after the reset");
    assert!(matches!(requery, XdmcpMessage::Query { .. }), "{requery:?}");

    sender.send(Message::Shutdown).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !handle.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(handle.is_finished(), "run_core did not return");
    handle.join().unwrap().unwrap();
}

/// A `Terminate` from the machine ends the loop cleanly — the other
/// half of the outcome wiring. `Failed` gets there in three packets
/// instead of the 126 seconds a retransmission timeout would take.
#[cfg(feature = "xdmcp")]
#[test]
fn an_xdmcp_terminate_ends_the_core_loop() {
    use crate::backend::recording::RecordingBackend;
    use yserver_protocol::xdmcp::{MIT_MAGIC_COOKIE_1, XdmcpMessage};

    let manager = XdmcpManagerFixture::new();
    let service = manager.service(false);
    let (poll, sender, rx) = channel().unwrap();
    let sender_for_core = sender.clone_handle();
    let handle = std::thread::spawn(move || {
        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();
        let alloc = ClientIdAllocator::new();
        run_core(
            poll,
            rx,
            sender_for_core,
            &mut state,
            &mut backend,
            None,
            &alloc,
            AuthState::new_with_xdmcp(None, true),
            ResetPolicy::Reset,
            Some(service),
        )
    });

    let (_, display) = manager.expect("the startup Query");
    manager.send(
        display,
        &XdmcpMessage::Willing {
            authentication_name: Vec::new(),
            hostname: b"fake-dm".to_vec(),
            status: b"willing".to_vec(),
        },
    );
    let _ = manager.expect("a Request");
    manager.send(
        display,
        &XdmcpMessage::Accept {
            session_id: 0x1234,
            authentication_name: Vec::new(),
            authentication_data: Vec::new(),
            authorization_name: MIT_MAGIC_COOKIE_1.to_vec(),
            authorization_data: b"cookie".to_vec(),
        },
    );
    let _ = manager.expect("a Manage");
    manager.send(
        display,
        &XdmcpMessage::Failed {
            session_id: 0x1234,
            status: b"no session for you".to_vec(),
        },
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    while !handle.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        handle.is_finished(),
        "a fatal XDMCP packet did not stop the loop"
    );
    handle.join().unwrap().unwrap();
    drop(sender);
}
/// A setup that lost the `Refuse` race is dropped as orphaned — and
/// that drop must not start a generation.
///
/// `AuthState` is atomic per call, but a setup thread that already
/// passed `check` is past that point: a `Refuse` can clear the
/// session cookie while the thread is one instruction from sending
/// its `ClientSetupComplete`. `XdmcpService::note_client_established`
/// reports that loser and the loop disconnects it. Under XDMCP the
/// policy is an implied `-reset`, so if the completion had armed the
/// reset trigger, the orphan's *own* disconnect would drain an armed
/// client set and cross the generation boundary — tearing down the
/// negotiation that is at that very moment retrying its `Request`.
///
/// Arming therefore belongs to the caller, after XDMCP admission has
/// decided. What this pins is that decision order: the orphan goes
/// away, the generation does not move, and the manager's outstanding
/// offer is undisturbed. The injected message stands in for the
/// racing setup thread exactly as it reaches the loop — a **remote**
/// client, because the orphan rule deliberately spares local ones
/// (Xorg's `XdmcpOpenDisplay` ignores a unix client, which the XDMCP
/// cookie never authorized).
/// `is_local` is an ADDRESS property. A TCP peer on this machine is a
/// local client — Xorg's `xtransLocalClient` says so — and therefore
/// keeps MIT-SHM, whose legacy `Attach` passes a SysV shmid rather
/// than a descriptor and so works fine without fd passing.
///
/// Deriving it from the transport instead, as this did until
/// 2026-09-10, refused shared memory to a same-machine XDMCP session
/// (`DISPLAY=127.0.0.1:1`) and pushed every image over the wire.
#[test]
fn a_tcp_peer_on_this_machine_is_a_local_client() {
    use std::net::{IpAddr, Ipv4Addr};

    assert!(
        address_is_ours(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        "127.0.0.1 is ours"
    );
    assert!(
        address_is_ours(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2))),
        "the whole loopback range is ours, not just 127.0.0.1"
    );
    // TEST-NET-3 (RFC 5737): reserved for documentation, so it cannot
    // be a real interface address on the machine running this test.
    assert!(
        !address_is_ours(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 1))),
        "a documentation-range address is not ours"
    );
}

#[cfg(feature = "xdmcp")]
#[test]
fn an_orphaned_xdmcp_client_does_not_reset_the_generation() {
    use crate::backend::recording::RecordingBackend;
    use std::io::Read;
    use yserver_protocol::{
        x11::ClientByteOrder,
        xdmcp::{MIT_MAGIC_COOKIE_1, XdmcpMessage, decode_message},
    };

    /// How long the "no reset happened" assertions watch for. The
    /// XDMCP retransmit floor is `XDM_MIN_RTX` = 2 s, so nothing the
    /// healthy machine does can land inside this window; a reset's
    /// re-query would land immediately.
    const QUIET: Duration = Duration::from_millis(400);
    const SESSION: u32 = 0x1234;

    let manager = XdmcpManagerFixture::new();
    let service = manager.service(false);
    let (poll, sender, rx) = channel().unwrap();
    // The generation is read from the counter, not inferred from
    // timing: the boundary bumps it and nothing else in the loop
    // does.
    let generations = rx.generation_counter();
    let sender_for_core = sender.clone_handle();
    let client_ids = std::sync::Arc::new(ClientIdAllocator::new());
    let client_ids_for_core = client_ids.clone();
    let handle = std::thread::spawn(move || {
        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();
        run_core(
            poll,
            rx,
            sender_for_core,
            &mut state,
            &mut backend,
            None,
            &client_ids_for_core,
            AuthState::new_with_xdmcp(None, true),
            ResetPolicy::Reset,
            Some(service),
        )
    });

    let start_generation = generations.current();

    // Query -> Willing -> Request -> Accept installs the session
    // cookie, and the machine answers with Manage.
    let (query, display) = manager.expect("the startup Query");
    assert!(matches!(query, XdmcpMessage::Query { .. }), "{query:?}");
    manager.send(
        display,
        &XdmcpMessage::Willing {
            authentication_name: Vec::new(),
            hostname: b"fake-dm".to_vec(),
            status: b"willing".to_vec(),
        },
    );
    let (request, _) = manager.expect("a Request");
    assert!(
        matches!(request, XdmcpMessage::Request { .. }),
        "{request:?}"
    );
    manager.send(
        display,
        &XdmcpMessage::Accept {
            session_id: SESSION,
            authentication_name: Vec::new(),
            authentication_data: Vec::new(),
            authorization_name: MIT_MAGIC_COOKIE_1.to_vec(),
            authorization_data: b"cookie".to_vec(),
        },
    );
    let (manage, _) = manager.expect("a Manage");
    assert!(matches!(manage, XdmcpMessage::Manage { .. }), "{manage:?}");

    // The Refuse clears the cookie and sends the machine back round
    // to Request. Reading that retry is the synchronisation point:
    // it cannot be on the wire until the Refuse has been fully
    // applied, so the injection below is unambiguously *after* the
    // clear. No generation change — the offer is being retried, not
    // abandoned.
    manager.send(
        display,
        &XdmcpMessage::Refuse {
            session_id: SESSION,
        },
    );
    let (retry, _) = manager.expect("the Request retry after the Refuse");
    assert!(
        matches!(retry, XdmcpMessage::Request { .. }),
        "a Refuse must resend the Request, got {retry:?}"
    );
    assert_eq!(
        generations.current(),
        start_generation,
        "a Refuse retries the offer; it does not cross a boundary"
    );

    // The racing setup thread's completion, arriving now.
    let orphan = client_ids.allocate();
    let (core_side, mut peer) = UnixStream::pair().expect("socketpair");
    peer.set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    let stale = sender.bind();
    stale
        .send(Message::ClientSetupComplete {
            id: orphan,
            generation: stale.generation(),
            stream: Transport::Unix(core_side),
            resource_id_base: 0x0020_0000,
            resource_id_mask: 0x000F_FFFF,
            byte_order: ClientByteOrder::LittleEndian,
            // Remote: only a TCP client can have been authorized by
            // the session credential the Refuse just revoked.
            is_local: false,
            fd_passing: false,
            setup_reply: Vec::new(),
        })
        .expect("send the racing completion");

    // 1. The orphan is dropped. `process_disconnect` shuts the
    //    socket down on both sides, so this is EOF, not a timeout.
    let mut sink = [0_u8; 1];
    match peer.read(&mut sink) {
        Ok(0) => {}
        other => panic!("a client with no session must be disconnected; read {other:?}"),
    }

    // Watch the manager socket for a while before judging anything.
    // The boundary is crossed at the *end* of the iteration the
    // disconnect ran in, so EOF above races the bump by microseconds
    // — this window is what makes the two assertions below decisive
    // rather than a coin flip. Collect only; asserting inside the
    // loop would let the re-query fire first and hide which of the
    // two actually broke.
    manager
        .socket
        .set_read_timeout(Some(QUIET))
        .expect("quiet-window timeout");
    let mut buf = [0_u8; 8192];
    let mut seen = Vec::new();
    let deadline = Instant::now() + QUIET;
    while Instant::now() < deadline {
        let len = match manager.socket.recv_from(&mut buf) {
            Ok((len, _)) => len,
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break;
            }
            Err(err) => panic!("unexpected error reading the manager socket: {err}"),
        };
        seen.push(decode_message(&buf[..len]).expect("decode"));
    }

    // 2. The regression itself.
    assert_eq!(
        generations.current(),
        start_generation,
        "an orphaned client never became established; its disconnect must not \
             drain an armed session and start a new generation"
    );

    // 3. And the negotiation carried on untouched: the retry read
    //    above is the manager's Request, and no Query followed it —
    //    a Query is what a reset's re-query looks like.
    assert!(
        !seen
            .iter()
            .any(|message| matches!(message, XdmcpMessage::Query { .. })),
        "the display re-queried mid-negotiation: {seen:?}"
    );

    sender.send(Message::Shutdown).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !handle.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(handle.is_finished(), "run_core did not return");
    handle.join().unwrap().unwrap();
}

#[test]
fn copied_scanout_completion_fd_dispatches_dedicated_hook() {
    use crate::backend::{BackendFdKind, recording::RecordingBackend};
    use std::{io::Write, os::fd::AsRawFd, sync::atomic::Ordering};

    let (poll, sender, rx) = channel().unwrap();
    let sender_for_core = sender.clone_handle();
    let (completion_reader, mut completion_writer) = UnixStream::pair().unwrap();
    let completion_fd = completion_reader.as_raw_fd();
    let (unused_page_tx, _unused_page_rx) = crossbeam_channel::unbounded();
    let (ready_tx, ready_rx) = crossbeam_channel::unbounded();
    let mut backend = RecordingBackend::new()
        .with_poll_sources(
            vec![(completion_fd, BackendFdKind::ScanoutRenderCompletion)],
            unused_page_tx,
        )
        .with_scanout_render_completion_notification(ready_tx);
    let handle = std::thread::spawn(move || {
        // `RecordingBackend` stores only the raw fd, so retain its owner
        // until `run_core` has unregistered every backend source.
        let _completion_reader = completion_reader;
        let mut state = ServerState::new();
        let alloc = ClientIdAllocator::new();
        let result = run_core(
            poll,
            rx,
            sender_for_core,
            &mut state,
            &mut backend,
            None,
            &alloc,
            AuthState::new(None),
            ResetPolicy::NoReset,
            None,
        );
        (result, backend)
    });

    completion_writer.write_all(&[1]).unwrap();
    ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    sender.send(Message::Shutdown).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !handle.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(handle.is_finished(), "run_core did not return");
    let (result, backend) = handle.join().unwrap();
    result.unwrap();
    assert!(
        backend
            .scanout_render_completion_count
            .load(Ordering::Relaxed)
            >= 1,
        "readiness must dispatch the copied-scanout completion hook"
    );
    assert_eq!(
        backend.page_flip_count.load(Ordering::Relaxed),
        0,
        "copied-scanout readiness must not be misrouted as a DRM page flip"
    );
}

/// Regression (project_reclamation_starvation_leak): the core loop
/// must drive backend GPU-resource reclamation (`before_block`) every
/// iteration, INDEPENDENT of page-flips. The KMS v2 backend reaped
/// per-op command buffers only from `on_page_flip_ready`; while the
/// display was dark (DPMS-off / standby / VT-away) no flips occurred,
/// so a client that kept drawing grew the engine `submitted` queue
/// without bound until the GPU lost its device. Here we run the loop
/// with ZERO DRM readiness events and assert `before_block` still
/// fired — i.e. reclamation rides the dispatch loop, not scanout.
#[test]
fn before_block_runs_without_any_page_flip() {
    use crate::backend::recording::RecordingBackend;
    use std::sync::atomic::Ordering;

    let (poll, sender, rx) = channel().unwrap();
    let sender_for_core = sender.clone_handle();
    let mut backend = RecordingBackend::new();
    let handle = std::thread::spawn(move || {
        let mut state = ServerState::new();
        let alloc = ClientIdAllocator::new();
        let result = run_core(
            poll,
            rx,
            sender_for_core,
            &mut state,
            &mut backend,
            None,
            &alloc,
            AuthState::new(None),
            ResetPolicy::NoReset,
            None,
        );
        (result, backend)
    });
    // No DRM readiness — only a Shutdown. The loop must still run at
    // least one iteration, calling before_block before it blocks.
    sender.send(Message::Shutdown).unwrap();
    // Generous deadline so a slow/loaded CI box can't spuriously fail:
    // the loop breaks the instant the thread finishes, so this only
    // bounds the pathological-hang case.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !handle.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(handle.is_finished(), "run_core did not return");
    let (result, backend) = handle.join().unwrap();
    result.unwrap();
    assert_eq!(
        backend.page_flip_count.load(Ordering::Relaxed),
        0,
        "test must exercise the no-page-flip path",
    );
    assert!(
        backend.before_block_count.load(Ordering::Relaxed) >= 1,
        "before_block must run each iteration even with no page-flips \
             (reclamation must not be gated on scanout)",
    );
}

/// `handle_host_input` arms the auto-repeat timer on a real
/// KeyPress, replaces it on a different KeyPress, and clears it
/// on the matching KeyRelease. Regression coverage for backend-owned input
/// dispatch paths that must not call `backend.on_host_input` directly,
/// bypassing this wrapper.
#[test]
fn handle_host_input_arms_repeat_state() {
    use crate::{backend::recording::RecordingBackend, host_x11::HostKeyEvent};

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();

    let key = |keycode: u8, pressed: bool| {
        HostInputEvent::Key(HostKeyEvent {
            origin: crate::core_loop::InputOrigin::NestedHost,
            pressed,
            keycode,
            time: 0,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            state: 0,
        })
    };

    // Press A → armed on A.
    handle_host_input(&mut state, &mut backend, key(38, true));
    let armed = state
        .key_repeats
        .get(&crate::core_loop::InputOrigin::NestedHost)
        .expect("press should arm repeat state for NestedHost");
    assert_eq!(armed.event.keycode, 38);
    assert!(armed.event.pressed);

    // Press B (different keycode) → replaces armed key.
    handle_host_input(&mut state, &mut backend, key(39, true));
    let armed = state
        .key_repeats
        .get(&crate::core_loop::InputOrigin::NestedHost)
        .expect("second press should replace the same origin's repeat");
    assert_eq!(armed.event.keycode, 39);

    // Release A while B is armed → ignored (only the armed key's
    // release clears).
    handle_host_input(&mut state, &mut backend, key(38, false));
    assert!(
        state
            .key_repeats
            .contains_key(&crate::core_loop::InputOrigin::NestedHost),
        "release of non-armed key must not clear",
    );

    // Release B → clears.
    handle_host_input(&mut state, &mut backend, key(39, false));
    assert!(state.key_repeats.is_empty());
}

/// Regression guard for the idle free-run fix (cut 2a): the caller
/// pokes the compositor (`mark_dirty`) only when a repeat actually
/// fires. `fire_pending_repeats` must return `false` on the
/// every-iteration "armed but not yet due" path (else a held/stuck
/// key busy-spins the compositor at the loop rate) and `true` only
/// when it fans out.
#[test]
fn fire_pending_repeats_reports_whether_it_fired() {
    use std::time::{Duration, Instant};

    use crate::{backend::recording::RecordingBackend, host_x11::HostKeyEvent};

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();

    // Nothing armed → no fire.
    assert!(!fire_pending_repeats(&mut state, &mut backend));

    // Arm a repeatable key (keycode 38 auto-repeats by default —
    // the sibling test relies on this too).
    handle_host_input(
        &mut state,
        &mut backend,
        HostInputEvent::Key(HostKeyEvent {
            origin: crate::core_loop::InputOrigin::NestedHost,
            pressed: true,
            keycode: 38,
            time: 0,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            state: 0,
        }),
    );
    assert!(
        state
            .key_repeats
            .contains_key(&crate::core_loop::InputOrigin::NestedHost)
    );
    let transition = crate::core_loop::key_fanout::key_transition_status(
        &state,
        crate::core_loop::InputOrigin::NestedHost,
        38,
        true,
    )
    .expect("NestedHost has a master keyboard");
    crate::core_loop::key_fanout::commit_key_transition(
        &mut state,
        crate::core_loop::InputOrigin::NestedHost,
        38,
        true,
        transition,
    );

    // Freshly armed → `next_fire` is INITIAL_DELAY in the future →
    // NOT due → must return false (the idle busy-spin case).
    assert!(
        !fire_pending_repeats(&mut state, &mut backend),
        "armed-but-not-due must not report a fire",
    );

    // Force the deadline into the past → must fire.
    if let Some(s) = state
        .key_repeats
        .get_mut(&crate::core_loop::InputOrigin::NestedHost)
    {
        s.next_fire = Instant::now() - Duration::from_millis(1);
    }
    assert!(
        fire_pending_repeats(&mut state, &mut backend),
        "a due repeat must report a fire",
    );
}

/// A fired repeat reaches the backend as `KeyRepeat`, not device `Key`:
/// it models Xorg's XKB soft repeat, which bypasses GetKeyboardEvents
/// and so generates no XI2 raw key event (issue #173).
#[test]
fn fire_pending_repeats_sends_key_repeat_not_device_key() {
    use std::time::{Duration, Instant};

    use crate::{
        backend::recording::{RecordedCall, RecordingBackend},
        host_x11::HostKeyEvent,
    };

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    handle_host_input(
        &mut state,
        &mut backend,
        HostInputEvent::Key(HostKeyEvent {
            origin: crate::core_loop::InputOrigin::NestedHost,
            pressed: true,
            keycode: 38,
            time: 0,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            state: 0,
        }),
    );
    let transition = crate::core_loop::key_fanout::key_transition_status(
        &state,
        crate::core_loop::InputOrigin::NestedHost,
        38,
        true,
    )
    .expect("NestedHost has a master keyboard");
    crate::core_loop::key_fanout::commit_key_transition(
        &mut state,
        crate::core_loop::InputOrigin::NestedHost,
        38,
        true,
        transition,
    );
    if let Some(s) = state
        .key_repeats
        .get_mut(&crate::core_loop::InputOrigin::NestedHost)
    {
        s.next_fire = Instant::now() - Duration::from_millis(1);
    }
    assert!(fire_pending_repeats(&mut state, &mut backend));

    let keys: Vec<RecordedCall> = backend
        .calls()
        .into_iter()
        .filter(|c| matches!(c, RecordedCall::HostKey { .. }))
        .collect();
    assert_eq!(
        keys,
        vec![
            RecordedCall::HostKey {
                keycode: 38,
                pressed: true,
                repeat: false,
            },
            RecordedCall::HostKey {
                keycode: 38,
                pressed: false,
                repeat: true,
            },
            RecordedCall::HostKey {
                keycode: 38,
                pressed: true,
                repeat: true,
            },
        ]
    );
    let transition = crate::core_loop::key_fanout::key_transition_status(
        &state,
        crate::core_loop::InputOrigin::NestedHost,
        38,
        true,
    )
    .expect("NestedHost has a master keyboard");
    crate::core_loop::key_fanout::commit_key_transition(
        &mut state,
        crate::core_loop::InputOrigin::NestedHost,
        38,
        true,
        transition,
    );
}

/// Helper: a touchpad `DeviceInfo` mirroring libinput's enumeration
/// of a Synaptics pad (matches xinput.rs's `touchpad_info`).
#[cfg(test)]
fn probe_touchpad_info() -> crate::core_loop::DeviceInfo {
    use crate::core_loop::message::{BoolSetting, LibinputConfigSnapshot};
    crate::core_loop::DeviceInfo {
        source_id: crate::xinput::InputSourceId(u64::from(line!())),
        enabled: true,
        resume_key: None,
        capabilities: crate::xinput::InputCapabilities {
            keyboard: false,
            pointer: true,
            touch: false,
        },
        name: "SynPS/2 Synaptics TouchPad".into(),
        device_node: "/dev/input/event4".into(),
        sysname: "event4".into(),
        vendor_id: 0x046d,
        product_id: 0xc52f,
        is_touchpad: true,
        config: LibinputConfigSnapshot {
            tap: BoolSetting {
                available: true,
                current: true,
                default: false,
            },
            natural_scroll: BoolSetting {
                available: true,
                current: false,
                default: true,
            },
            dwt: BoolSetting {
                available: true,
                current: true,
                default: true,
            },
            ..Default::default()
        },
    }
}

#[cfg(test)]
fn xtest_pointer_name(state: &ServerState) -> String {
    state
        .xi_devices
        .iter()
        .find(|d| d.id == crate::xinput::DEVICEID_XTEST_POINTER)
        .expect("XTEST pointer (id 4) always present")
        .name
        .clone()
}

/// A backend with no on-core libinput (the trait default, and what
/// Direct-mode / host-X11 / ynest present) is a clean no-op probe:
/// returns 0 and leaves the static device model untouched.
#[test]
fn probe_input_devices_default_is_noop() {
    use crate::backend::recording::RecordingBackend;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new(); // no probe_rounds configured
    let before = xtest_pointer_name(&state);

    let seeded = backend.probe_input_devices(&mut state);

    assert_eq!(seeded, 0, "no-op probe seeds nothing");
    assert_eq!(
        xtest_pointer_name(&state),
        before,
        "device 4 unchanged when nothing to probe",
    );
}

/// A backend whose startup probe enumerates a touchpad seeds its
/// physical facet before the serve loop while keeping XTEST 4 intact.
#[test]
fn probe_input_devices_seeds_touchpad_before_loop() {
    use crate::backend::recording::RecordingBackend;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    // One non-empty round (the touchpad), then libinput goes quiet.
    backend.probe_rounds.push_back(vec![probe_touchpad_info()]);

    assert_ne!(
        xtest_pointer_name(&state),
        "SynPS/2 Synaptics TouchPad",
        "precondition: device 4 starts as the virtual XTEST pointer",
    );

    let seeded = backend.probe_input_devices(&mut state);

    assert_eq!(seeded, 1, "exactly one device seeded");
    assert_eq!(
        xtest_pointer_name(&state),
        crate::xinput::registry::NAME_XTEST_POINTER,
        "device 4 remains the virtual XTEST pointer after startup probe",
    );
    let physical_pointer = state
        .xi_devices
        .iter()
        .find(|device| {
            device.name == "SynPS/2 Synaptics TouchPad"
                && device.facet == Some(crate::xinput::XiFacetKind::PointerTouch)
        })
        .expect("startup probe registers a physical touchpad pointer facet");
    assert!(physical_pointer.id >= 6, "physical facet IDs start at 6");
    let tap_atom = state
        .atoms
        .id_for(crate::xinput::PROP_TAPPING_ENABLED)
        .unwrap();
    assert!(physical_pointer.properties.contains_key(&tap_atom));
    assert!(
        !state
            .xi_devices
            .device(crate::xinput::DEVICEID_XTEST_POINTER)
            .unwrap()
            .properties
            .contains_key(&tap_atom)
    );
}

/// The bounded drain TERMINATES: with libinput perpetually empty it
/// stops after two consecutive empty rounds (not the MAX_ROUNDS
/// ceiling), and even an adversarial always-non-empty source is
/// capped at the ceiling rather than spinning forever.
#[test]
fn probe_input_devices_bounded_drain_terminates() {
    use crate::backend::recording::RecordingBackend;

    // Empty source → stops after the 2 empty rounds, well under the
    // 8-round ceiling.
    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    let seeded = backend.probe_input_devices(&mut state);
    assert_eq!(seeded, 0);
    assert_eq!(
        backend.probe_rounds_run.get(),
        2,
        "two consecutive empty rounds end the drain",
    );

    // Adversarial source that never goes empty → capped at the
    // MAX_ROUNDS ceiling, never unbounded.
    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    for _ in 0..100 {
        backend.probe_rounds.push_back(vec![probe_touchpad_info()]);
    }
    let seeded = backend.probe_input_devices(&mut state);
    assert_eq!(
        backend.probe_rounds_run.get(),
        8,
        "drain is capped at the MAX_ROUNDS ceiling",
    );
    assert_eq!(seeded, 8, "one device seeded per capped round");
}

#[test]
fn shutdown_returns() {
    use crate::{backend::recording::RecordingBackend, core_loop::poll_tokens::ClientIdAllocator};

    let (poll, sender, rx) = channel().unwrap();
    let sender_for_core = sender.clone_handle();
    let handle = std::thread::spawn(move || {
        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();
        let alloc = ClientIdAllocator::new();
        run_core(
            poll,
            rx,
            sender_for_core,
            &mut state,
            &mut backend,
            None,
            &alloc,
            AuthState::new(None),
            ResetPolicy::NoReset,
            None,
        )
    });
    sender.send(Message::Shutdown).unwrap();
    // Bound the wait so a regression that fails to return does not
    // hang the test runner.
    for _ in 0..50 {
        if handle.is_finished() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(handle.is_finished(), "run_core did not return on Shutdown");
    handle.join().unwrap().unwrap();
}

#[test]
fn evaluator_fires_idle_activation_when_deadline_elapsed() {
    let mut state = ServerState::new();
    state.screensaver.timeout_ms = 60_000;
    state.dpms.last_activity = Instant::now() - Duration::from_secs(61);
    // No client installed — emit_screen_saver_notify short-circuits
    // on empty selected_by; we're asserting state transition only.
    let mut backend = RecordingBackend::default();

    super::evaluate_screen_saver_post_poll(&mut state, &mut backend);

    assert_eq!(
        state.screensaver.active,
        ScreenSaverActive::On,
        "elapsed idle deadline must drive SS On"
    );
}

#[test]
fn evaluator_fires_cycle_and_advances_next_cycle() {
    let mut state = ServerState::new();
    state.screensaver.active = ScreenSaverActive::On;
    state.screensaver.interval_ms = 60_000;
    let past = Instant::now() - Duration::from_millis(10);
    state.screensaver.next_cycle = Some(past);
    let mut backend = RecordingBackend::default();

    super::evaluate_screen_saver_post_poll(&mut state, &mut backend);

    let next = state.screensaver.next_cycle.expect("re-armed by evaluator");
    assert!(
        next > past,
        "next_cycle must advance past the prior deadline"
    );
}

#[test]
fn evaluator_idle_path_skipped_while_dpms_blanked() {
    // Xorg WaitFor.c:457 — when DPMS is non-On the SS idle timer
    // is suppressed; the DPMS→SS coupling already handled it.
    let mut state = ServerState::new();
    state.screensaver.timeout_ms = 60_000;
    state.dpms.last_activity = Instant::now() - Duration::from_secs(120);
    state.dpms.power_level = 3; // Off
    let mut backend = RecordingBackend::default();

    super::evaluate_screen_saver_post_poll(&mut state, &mut backend);

    assert_eq!(
        state.screensaver.active,
        ScreenSaverActive::Off,
        "evaluator must not fire SS when DPMS is blanked"
    );
}

#[test]
fn idletime_evaluator_fires_pos_transition_when_deadline_elapsed() {
    use std::time::Duration;
    use yserver_protocol::x11::{ClientId, sync as x11sync};
    let mut state = ServerState::new();
    // Pre-arm: a PositiveTransition alarm at 60_000ms, last_activity 61s ago.
    state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(61);
    let alarm_id = 0x2000;
    state.sync_alarms.insert(
        alarm_id,
        crate::server::SyncAlarm {
            owner: ClientId(1),
            counter: x11sync::IDLETIME_COUNTER,
            wait_value: 60_000,
            delta: 0,
            test_type: x11sync::TEST_POSITIVE_TRANSITION,
            events: false, // skip wire delivery; assert state mutation only
            state: x11sync::ALARM_STATE_ACTIVE,
            event_clients: Vec::new(),
            value_type: 0,
            raw_wait: 60_000,
            check_type: x11sync::TEST_POSITIVE_TRANSITION,
        },
    );
    let mut backend = RecordingBackend::default();

    super::evaluate_idletime_alarms_post_poll(&mut state, &mut backend);

    // PositiveTransition + delta=0 stays Active (Task 2 fix).
    let after = &state.sync_alarms[&alarm_id];
    assert_eq!(after.state, x11sync::ALARM_STATE_ACTIVE);
    // last_evaluated cache updated for global IDLETIME.
    assert!(
        state
            .idletime_last_evaluated
            .get(&x11sync::IDLETIME_COUNTER)
            .copied()
            .unwrap_or(0)
            >= 60_000,
        "last_evaluated cache should advance past the trigger value"
    );
}

#[test]
fn idletime_evaluator_skips_when_no_idletime_alarms() {
    let mut state = ServerState::new();
    let mut backend = RecordingBackend::default();
    // No alarms at all — must not panic, must not insert spurious cache entries.
    super::evaluate_idletime_alarms_post_poll(&mut state, &mut backend);
    assert!(state.idletime_last_evaluated.is_empty());
}

/// Task 4/7 completion pacing: a completion whose gate targets a future
/// vblank parks on drain (its wake is NOT signalled yet), and only fires —
/// signalling `signal_present_wake` — once `fire_due_present_completions`
/// runs at an MSC that has reached the target. Sibling to the NotifyMSC
/// `parks_then_fires_on_vblank_advance` test.
#[test]
fn gated_present_completion_parks_then_fires_on_vblank_advance() {
    use crate::{
        backend::{CompletedPresentEvent, PresentWake, recording::RecordingBackend},
        server::PresentCompleteGate,
    };
    use yserver_protocol::x11::ClientId;

    const PRESENT_ID: u64 = 0x42;
    const TARGET_MSC: u64 = 200;
    const WINDOW_XID: u32 = 0x0000_0101;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();

    // A standalone sequence has advanced the general clock beyond the
    // target, but the completion-eligible clock is still zero. The gate
    // must park rather than taking the old already-due immediate path.
    state.present_crtc_clocks.insert(
        (0, 0),
        crate::server::PresentCrtcClock {
            epoch: 0,
            msc: 250,
            ust: 0,
            completion: crate::backend::PresentClockSample {
                msc: 0,
                ust: 0,
                source: crate::backend::PresentClockSource::Immediate,
            },
        },
    );
    state.present_complete_gate.insert(
        PRESENT_ID,
        PresentCompleteGate {
            crtc_id: 0,
            crtc_epoch: 0,
            msc_offset: 0,
            effective_target_msc: TARGET_MSC,
            owner: ClientId(1),
            dst_window_xid: WINDOW_XID,
        },
    );
    // Backend reports the copy's GPU completion this iteration.
    backend
        .completed_present_events_to_drain
        .push(CompletedPresentEvent {
            client_id: ClientId(1),
            serial: 7,
            host_xid: WINDOW_XID,
            dst_host_xid: WINDOW_XID,
            options: 0,
            present_id: PRESENT_ID,
            window_generation: 0,
            crtc_id: 0,
            crtc_epoch: 0,
            msc_offset: 0,
            completion_clock: Some(crate::backend::PresentClockSample {
                msc: 0,
                ust: 0,
                source: crate::backend::PresentClockSource::Immediate,
            }),
            wake: PresentWake::Pixmap { idle_fence_xid: 0 },
            completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
            emit_idle: true,
        });

    // Drain: the future-target gate parks the completion — no wake yet.
    // (RecordingBackend's completion clock is (0,0), so the drain's own
    // `fire_due_present_completions` is skipped and the park holds.)
    drain_present_completions(&mut state, &mut backend);
    assert_eq!(
        state.present_pending_complete.len(),
        1,
        "future-target completion parks on drain"
    );
    assert!(
        state.present_complete_gate.is_empty(),
        "gate consumed when the copy completes"
    );
    assert!(
        backend.signalled_present_wakes.is_empty(),
        "parked completion's wake is NOT signalled before the vblank"
    );

    // Vblank advances to the target: the parked completion fires + signals.
    // The zero sample above is a fixture-only stand-in for the initial
    // copy-completion clock. Real copy completions carry `None`, allowing
    // the later selected-domain vblank sample to stamp the paced event.
    state.present_pending_complete[0].event.completion_clock = None;
    crate::core_loop::process_request::fire_due_present_completions(
        &mut state,
        &mut backend,
        crate::backend::PresentClockSample {
            msc: TARGET_MSC,
            ust: 0x1234,
            source: crate::backend::PresentClockSource::PageFlip,
        },
    );
    assert!(
        state.present_pending_complete.is_empty(),
        "parked completion released once its target MSC is reached"
    );
    assert_eq!(
        backend.signalled_present_wakes,
        vec![PRESENT_ID],
        "signal_present_wake fires exactly once at the target vblank"
    );
}

/// The gate-absent / already-reached path must NOT park: the completion
/// fires immediately on drain and signals its wake once.
#[test]
fn ungated_present_completion_fires_immediately_without_parking() {
    use crate::backend::{CompletedPresentEvent, PresentWake, recording::RecordingBackend};
    use yserver_protocol::x11::ClientId;

    const PRESENT_ID: u64 = 0x43;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    // No gate recorded for this present_id → complete-now arm.
    backend
        .completed_present_events_to_drain
        .push(CompletedPresentEvent {
            client_id: ClientId(1),
            serial: 8,
            host_xid: 0x0000_0202,
            dst_host_xid: 0x0000_0202,
            options: 0,
            present_id: PRESENT_ID,
            window_generation: 0,
            crtc_id: 0,
            crtc_epoch: 0,
            msc_offset: 0,
            completion_clock: None,
            wake: PresentWake::Pixmap { idle_fence_xid: 0 },
            completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
            emit_idle: true,
        });

    drain_present_completions(&mut state, &mut backend);
    assert!(
        state.present_pending_complete.is_empty(),
        "gate-absent completion does not park"
    );
    assert_eq!(
        backend.signalled_present_wakes,
        vec![PRESENT_ID],
        "gate-absent completion signals its wake immediately"
    );
}

/// Spec §"Ordered completion delivery" item 2: the due arm of the
/// drain (a completion whose gate is already satisfied when its GPU
/// fence retires) must route through `present_pending_complete`
/// instead of firing inline via `complete_present_with_clock` — so
/// that the per-window sweep in `fire_due_present_completions`, not
/// raw arrival order, decides delivery order against anything else
/// already parked for the same window. Pre-fix this fired here
/// directly and never touched the queue at all.
#[test]
fn due_gate_arm_pushes_into_queue_instead_of_firing_inline() {
    use crate::{
        backend::{CompletedPresentEvent, PresentWake, recording::RecordingBackend},
        server::PresentCompleteGate,
    };
    use yserver_protocol::x11::ClientId;

    const PRESENT_ID: u64 = 0x44;
    const WINDOW_XID: u32 = 0x0000_0303;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    // effective_target_msc 0 is already satisfied against
    // RecordingBackend's default (0, 0) completion clock — the "due"
    // arm, not the "still future" park arm.
    state.present_complete_gate.insert(
        PRESENT_ID,
        PresentCompleteGate {
            crtc_id: 0,
            crtc_epoch: 0,
            msc_offset: 0,
            effective_target_msc: 0,
            owner: ClientId(1),
            dst_window_xid: WINDOW_XID,
        },
    );
    backend
        .completed_present_events_to_drain
        .push(CompletedPresentEvent {
            client_id: ClientId(1),
            serial: 9,
            host_xid: WINDOW_XID,
            dst_host_xid: WINDOW_XID,
            options: 0,
            present_id: PRESENT_ID,
            window_generation: 0,
            crtc_id: 0,
            crtc_epoch: 0,
            msc_offset: 0,
            completion_clock: None,
            wake: PresentWake::Pixmap { idle_fence_xid: 0 },
            completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
            emit_idle: true,
        });

    drain_present_completions(&mut state, &mut backend);
    assert!(
        state.present_complete_gate.is_empty(),
        "gate consumed when the copy completes"
    );
    assert_eq!(
        state.present_pending_complete.len(),
        1,
        "the due arm pushes into the ordered queue rather than firing \
             inline (RecordingBackend's zero completion clock means the \
             same-pass sweep can't drain it yet, which is fine — this test \
             only pins that it did NOT fire inline)"
    );
    assert!(
        backend.signalled_present_wakes.is_empty(),
        "must not signal the wake inline — delivery is the sweep's job"
    );
}

/// Spec round-4 F6: presents without a usable clock
/// (`effective_target_msc == None`, no gate entry — the drain's
/// gate-absent arm) sit outside the
/// per-window hold-back entirely and complete immediately, even ahead
/// of an earlier-arrived, still-unresolved synced present parked for
/// the same window. This is Xorg-parity and pre-existing; documented
/// so it isn't mistaken for a hold-back bug.
#[test]
fn no_clock_present_completion_bypasses_per_window_hold_back() {
    use crate::{
        backend::{CompletedPresentEvent, PresentWake, recording::RecordingBackend},
        server::PendingPresentComplete,
    };
    use yserver_protocol::x11::{ClientId, present as x11present};

    const WINDOW_XID: u32 = 0x0000_0606;
    const PARKED_SMALLER_ID: u64 = 5;
    const ASYNC_ID: u64 = 6;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();

    // An earlier, smaller-id synced present is still parked/unresolved
    // for this window.
    state.present_pending_complete.push(PendingPresentComplete {
        event: CompletedPresentEvent {
            client_id: ClientId(1),
            serial: 1,
            host_xid: WINDOW_XID,
            dst_host_xid: WINDOW_XID,
            options: 0,
            present_id: PARKED_SMALLER_ID,
            window_generation: 0,
            crtc_id: 0,
            crtc_epoch: 0,
            msc_offset: 0,
            completion_clock: None,
            wake: PresentWake::Pixmap { idle_fence_xid: 0 },
            completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
            emit_idle: true,
        },
        effective_target_msc: 0,
        mode: x11present::COMPLETE_MODE_COPY,
        emit_idle: true,
    });

    // A later async completion for the same window: no gate entry at all.
    backend
        .completed_present_events_to_drain
        .push(CompletedPresentEvent {
            client_id: ClientId(1),
            serial: 2,
            host_xid: WINDOW_XID,
            dst_host_xid: WINDOW_XID,
            options: 0,
            present_id: ASYNC_ID,
            window_generation: 0,
            crtc_id: 0,
            crtc_epoch: 0,
            msc_offset: 0,
            completion_clock: None,
            wake: PresentWake::Pixmap { idle_fence_xid: 0 },
            completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
            emit_idle: true,
        });

    drain_present_completions(&mut state, &mut backend);
    assert_eq!(
        backend.signalled_present_wakes,
        vec![ASYNC_ID],
        "the async completion fires immediately, bypassing hold-back"
    );
    assert_eq!(
        state.present_pending_complete.len(),
        1,
        "the earlier parked synced present is untouched by the async path"
    );
}

/// Review fix (post-Task-6): the async exemption above covers async
/// firing ahead of a still-HELD entry — it must NOT cover an async
/// completion overtaking a gated Copy that is already due and simply
/// hasn't been swept yet. In one `drain_present_completions` pass,
/// `completed = [X(gated, due, id=5), Y(async, id=7)]` for the SAME
/// window: X's due-arm pushes into the queue (per Task 6 Step 3), then
/// Y's gate-absent arm used to fire straight through, landing before
/// X's post-loop sweep — id=7 then id=5, a backward serial that
/// didn't exist pre-Task-6 (eager firing kept them in arrival order).
/// Fixed by flushing due-and-unblocked entries from the queue before
/// the async arm fires inline, so id=5 goes out first.
#[test]
fn gated_due_copy_delivers_before_same_drain_async_completion() {
    use crate::{
        backend::{CompletedPresentEvent, PresentWake, recording::RecordingBackend},
        server::PresentCompleteGate,
    };
    use yserver_protocol::x11::ClientId;

    const WINDOW_XID: u32 = 0x0000_0808;
    const GATED_ID: u64 = 5;
    const ASYNC_ID: u64 = 7;
    const TARGET_MSC: u64 = 300;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    // A real, nonzero completion clock this time (RecordingBackend
    // defaults to (0,0), which would make fire_due_present_completions
    // bail before ever reaching the ordering bug this test pins).
    backend.present_ust_msc = (TARGET_MSC, 0xABCD);

    state.present_complete_gate.insert(
        GATED_ID,
        PresentCompleteGate {
            crtc_id: 0,
            crtc_epoch: 0,
            msc_offset: 0,
            effective_target_msc: TARGET_MSC,
            owner: ClientId(1),
            dst_window_xid: WINDOW_XID,
        },
    );
    // Arrival order within one drain: the gated-due entry first, the
    // async one second — matching the reviewer's vector.
    backend
        .completed_present_events_to_drain
        .push(CompletedPresentEvent {
            client_id: ClientId(1),
            serial: 1,
            host_xid: WINDOW_XID,
            dst_host_xid: WINDOW_XID,
            options: 0,
            present_id: GATED_ID,
            window_generation: 0,
            crtc_id: 0,
            crtc_epoch: 0,
            msc_offset: 0,
            completion_clock: Some(crate::backend::PresentClockSample {
                msc: TARGET_MSC,
                ust: 0xABCD,
                source: crate::backend::PresentClockSource::PageFlip,
            }),
            wake: PresentWake::Pixmap { idle_fence_xid: 0 },
            completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
            emit_idle: true,
        });
    backend
        .completed_present_events_to_drain
        .push(CompletedPresentEvent {
            client_id: ClientId(1),
            serial: 2,
            host_xid: WINDOW_XID,
            dst_host_xid: WINDOW_XID,
            options: 0,
            present_id: ASYNC_ID,
            window_generation: 0,
            crtc_id: 0,
            crtc_epoch: 0,
            msc_offset: 0,
            completion_clock: None,
            wake: PresentWake::Pixmap { idle_fence_xid: 0 },
            completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
            emit_idle: true,
        });

    drain_present_completions(&mut state, &mut backend);
    assert_eq!(
        backend.signalled_present_wakes,
        vec![GATED_ID, ASYNC_ID],
        "Copy(5) must deliver before async(7) in the same drain pass — \
             pre-fix this reads [7, 5]"
    );
}

/// Task 4 (spec "Loop-order and clock contract" item 1): the tail's
/// drain must run BEFORE `maybe_composite`, so a present executed in
/// this iteration's drain is visible to this iteration's compose
/// instead of slipping a full period behind unrelated damage. Drives
/// both halves of the drain — a source-ready `PresentPixmap` copy
/// (whose execution marks dirty, `process_request.rs:8714`) and a
/// canned GPU-completion event (`drain_completed_present_events`) —
/// and asserts both are recorded before `maybe_composite` in
/// `RecordingBackend`'s call log. Fails against the pre-Task-4 order
/// (`maybe_composite` before the drain).
#[test]
fn run_iteration_tail_drains_present_work_before_compositing() {
    use crate::{
        backend::{
            CompletedPresentEvent, PresentWake,
            recording::{RecordedCall, RecordingBackend},
        },
        server::{PendingPresentEntry, PendingPresentPixmap, PendingPresentRequest},
    };
    use yserver_protocol::x11::{ClientId, present::PixmapRequest};

    const WAIT_ID: u64 = 7;
    const DEFERRED_PRESENT_ID: u64 = 0x77;
    const PRESENT_ID: u64 = 0x99;
    const WINDOW_XID: u32 = 0x0000_0303;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();

    // A source-ready PresentPixmap copy: draining it runs
    // `execute_present_pixmap_copy` then `mark_dirty` — the real
    // production link between "the drain executed something" and
    // "compose must see it this iteration".
    state
        .present_wait_to_id
        .insert(WAIT_ID, DEFERRED_PRESENT_ID);
    state.present_pending_exec.insert(
        DEFERRED_PRESENT_ID,
        PendingPresentEntry {
            pending: PendingPresentPixmap {
                origin: None,
                client_id: ClientId(1),
                request: PendingPresentRequest::Pixmap(PixmapRequest {
                    window: WINDOW_XID,
                    pixmap: 0x304,
                    serial: 9,
                    valid: 0,
                    update: 0,
                    x_off: 0,
                    y_off: 0,
                    target_crtc: 0,
                    wait_fence: 0,
                    idle_fence: 0,
                    options: 0,
                    target_msc: 0,
                    divisor: 0,
                    remainder: 0,
                    notifies: Vec::new(),
                }),
                wake: crate::backend::PresentWake::Pixmap { idle_fence_xid: 0 },
                masked_options: 0,
                src_host_xid: 0x0040_0304,
                paint_dst_host_xid: 0x0040_0303,
                completion_dst_host_xid: 0x0040_0303,
                src_width: 10,
                src_height: 10,
                update_rects: None,
                present_id: DEFERRED_PRESENT_ID,
                window_generation: 0,
                crtc_id: 0,
                crtc_epoch: 0,
                msc_offset: 0,
                effective_target_msc: None,
            },
            source_ready: false,
            wait_id: Some(WAIT_ID),
            pin: None,
        },
    );
    backend.ready_present_source_waits.push(WAIT_ID);

    // A canned GPU-completion event: exercises the second drain half.
    backend
        .completed_present_events_to_drain
        .push(CompletedPresentEvent {
            client_id: ClientId(1),
            serial: 9,
            host_xid: WINDOW_XID,
            dst_host_xid: WINDOW_XID,
            options: 0,
            present_id: PRESENT_ID,
            window_generation: 0,
            crtc_id: 0,
            crtc_epoch: 0,
            msc_offset: 0,
            completion_clock: None,
            wake: PresentWake::Pixmap { idle_fence_xid: 0 },
            completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
            emit_idle: true,
        });

    run_iteration_tail(&mut state, &mut backend);

    let calls = backend.calls();
    let mark_dirty_idx = calls
        .iter()
        .position(|c| matches!(c, RecordedCall::MarkDirty))
        .expect("source-ready copy executed and marked dirty");
    let drain_completed_idx = calls
        .iter()
        .position(|c| matches!(c, RecordedCall::DrainCompletedPresentEvents))
        .expect("completed present events drained");
    let composite_idx = calls
        .iter()
        .position(|c| matches!(c, RecordedCall::MaybeComposite))
        .expect("maybe_composite invoked");

    assert!(
        mark_dirty_idx < composite_idx,
        "drain's mark_dirty ({mark_dirty_idx}) must precede maybe_composite ({composite_idx})"
    );
    assert!(
        drain_completed_idx < composite_idx,
        "drain_completed_present_events ({drain_completed_idx}) must precede maybe_composite ({composite_idx})"
    );
}

/// Fix-forward: idle-vblank arming for a parked Present completion must
/// run AFTER `maybe_composite`, not inside the pre-compose drain.
/// `mark_dirty()` alone (no output damage) makes a real KMS compose
/// return `Skipped(EmptyDamage)`, which still clears
/// `scene_wants_compose()` — so `present_completion_is_idle()` only
/// reports idle post-compose. Arming pre-compose would see a dirty
/// scene and arm nothing, stranding the parked `CompleteNotify` with no
/// fd left to wake `poll`. Fails against the arm folded into
/// `drain_present_completions` (landing before `MaybeComposite`).
#[test]
fn run_iteration_tail_arms_present_completion_idle_vblanks_after_compositing() {
    use crate::{
        backend::{
            CompletedPresentEvent, PresentWake,
            recording::{RecordedCall, RecordingBackend},
        },
        server::PendingPresentComplete,
    };
    use yserver_protocol::x11::ClientId;

    const PARKED_PRESENT_ID: u64 = 0x55;
    const DRAINED_PRESENT_ID: u64 = 0x56;
    const WINDOW_XID: u32 = 0x0000_0505;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();

    // Something for the arm to arm: a completion already parked on a
    // future target MSC.
    state.present_pending_complete.push(PendingPresentComplete {
        event: CompletedPresentEvent {
            client_id: ClientId(1),
            serial: 10,
            host_xid: WINDOW_XID,
            dst_host_xid: WINDOW_XID,
            options: 0,
            present_id: PARKED_PRESENT_ID,
            window_generation: 0,
            crtc_id: 0,
            crtc_epoch: 0,
            msc_offset: 0,
            completion_clock: None,
            wake: PresentWake::Pixmap { idle_fence_xid: 0 },
            completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
            emit_idle: true,
        },
        effective_target_msc: 500,
        mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
        emit_idle: true,
    });

    // A canned GPU-completion event so the drain half also runs.
    backend
        .completed_present_events_to_drain
        .push(CompletedPresentEvent {
            client_id: ClientId(1),
            serial: 11,
            host_xid: WINDOW_XID,
            dst_host_xid: WINDOW_XID,
            options: 0,
            present_id: DRAINED_PRESENT_ID,
            window_generation: 0,
            crtc_id: 0,
            crtc_epoch: 0,
            msc_offset: 0,
            completion_clock: None,
            wake: PresentWake::Pixmap { idle_fence_xid: 0 },
            completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
            emit_idle: true,
        });

    run_iteration_tail(&mut state, &mut backend);

    let calls = backend.calls();
    let drain_completed_idx = calls
        .iter()
        .position(|c| matches!(c, RecordedCall::DrainCompletedPresentEvents))
        .expect("completed present events drained");
    let composite_idx = calls
        .iter()
        .position(|c| matches!(c, RecordedCall::MaybeComposite))
        .expect("maybe_composite invoked");
    let arm_idx = calls
        .iter()
        .position(|c| matches!(c, RecordedCall::ArmPresentCompletionIdleVblanks))
        .expect("parked completion armed an idle vblank");

    assert!(
        drain_completed_idx < composite_idx,
        "drain ({drain_completed_idx}) must still precede compose ({composite_idx})"
    );
    assert!(
        composite_idx < arm_idx,
        "arm ({arm_idx}) must run after compose ({composite_idx}), not inside the pre-compose drain"
    );
}

#[test]
fn run_iteration_tail_flushes_damage_before_compositing() {
    use crate::backend::recording::{RecordedCall, RecordingBackend};

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    state.damage_notify_flush_pending = true;

    run_iteration_tail(&mut state, &mut backend);

    let calls = backend.calls();
    let flush = calls
        .iter()
        .position(|call| matches!(call, RecordedCall::FlushBeforeDamageNotify))
        .expect("damage boundary flushed");
    let compose = calls
        .iter()
        .position(|call| matches!(call, RecordedCall::MaybeComposite))
        .expect("compose attempted");
    assert!(flush < compose);
    assert!(!state.damage_notify_flush_pending);
}
