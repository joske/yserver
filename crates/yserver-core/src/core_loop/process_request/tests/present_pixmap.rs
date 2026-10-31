use super::*;

fn present_test_output(
    output_id: u32,
    crtc_id: u32,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    enabled: bool,
) -> crate::randr::RandrOutput {
    let mode_id = if enabled {
        crtc_id.wrapping_add(0x1000)
    } else {
        0
    };
    crate::randr::RandrOutput {
        name: format!("test-{output_id}"),
        output_id,
        crtc_id,
        mode_id,
        connected: true,
        x,
        y,
        width: if enabled { width } else { 0 },
        height: if enabled { height } else { 0 },
        vrefresh: if enabled { 60 } else { 0 },
        timing: None,
        mm_width: 0,
        mm_height: 0,
        mode_ids: vec![crtc_id.wrapping_add(0x1000)],
        num_preferred: 1,
        pending_transform: Default::default(),
        current_transform: Default::default(),
        rotation: crate::randr::RR_ROTATE_0,
    }
}

/// Present family: a Present ConfigureNotify selector over
/// `OUTBOUND_CAP` is flagged for the core loop to disconnect, and the
/// reading selector still gets the event.
#[test]
fn overflowing_present_selector_is_flagged_for_disconnect() {
    use crate::server::PresentEventSelection;
    use yserver_protocol::x11::present as x11present;

    const WINDOW: ResourceId = ResourceId(0x200);
    let mut state = ServerState::new();
    let _slow = install_client(&mut state, 1);
    let mut fast = install_client(&mut state, 2);
    for (eid, owner) in [(0x0010_0042, 1), (0x0020_0042, 2)] {
        state.present_event_selections.insert(
            eid,
            PresentEventSelection {
                owner: ClientId(owner),
                window: WINDOW,
                event_mask: x11present::EVENT_MASK_CONFIGURE_NOTIFY,
            },
        );
    }
    client_io::saturate_for_test(state.clients.get_mut(&1).unwrap());
    fire_present_configure_notify_for_window(
        &mut state,
        WINDOW,
        yserver_protocol::x11::Geometry {
            root: crate::resources::ROOT_WINDOW,
            x: 1,
            y: 2,
            width: 30,
            height: 40,
            border_width: 0,
            depth: 24,
        },
    );
    assert_eq!(client_io::failed_writers(&state.clients), [ClientId(1)]);
    let bytes = read_all_available(&mut fast);
    assert_eq!(bytes.first(), Some(&35), "Present GenericEvent");
}

/// Xorg's Present screen hook receives every real ConfigNotify, including
/// a pure move. Steam's DRI3 popup windows select this event and use its
/// x/y to keep the presentation surface synchronized with the native
/// override-redirect window.
#[test]
fn configure_window_pure_move_emits_present_configure_notify() {
    use crate::server::PresentEventSelection;
    use yserver_protocol::x11::present as x11present;

    const WINDOW: ResourceId = ResourceId(0x200);
    const PRESENT_EID: u32 = 0x0010_0042;

    let (mut state, mut peer) = two_children_under_root();
    state
        .clients
        .get_mut(&1)
        .unwrap()
        .event_masks
        .insert(WINDOW, 0x0002_0000);
    state.present_event_selections.insert(
        PRESENT_EID,
        PresentEventSelection {
            owner: ClientId(1),
            window: WINDOW,
            event_mask: x11present::EVENT_MASK_CONFIGURE_NOTIFY,
        },
    );
    let _ = read_all_available(&mut peer);

    let body = cw_restack_body(WINDOW.0, 0x0003, &[610, 250]); // CWX|CWY
    let mut backend = RecordingBackend::new();
    handle_configure_window(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        &body,
    )
    .expect("handle_configure_window");

    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes.len(), 72, "Present XGE plus core ConfigureNotify");
    // Xorg dix/window.c invokes the Present ConfigNotify screen hook
    // before DeliverEvents sends the core ConfigureNotify. This ordering
    // matters to clients which merge the two configure streams.
    assert_eq!(bytes[0], 35, "GenericEvent");
    assert_eq!(bytes[1], 145, "PRESENT major opcode");
    assert_eq!(
        u16::from_le_bytes(bytes[8..10].try_into().unwrap()),
        u16::from(x11present::EVENT_CONFIGURE_NOTIFY),
    );
    assert_eq!(
        u32::from_le_bytes(bytes[12..16].try_into().unwrap()),
        PRESENT_EID,
    );
    assert_eq!(
        u32::from_le_bytes(bytes[16..20].try_into().unwrap()),
        WINDOW.0,
    );
    assert_eq!(i16::from_le_bytes(bytes[20..22].try_into().unwrap()), 610);
    assert_eq!(i16::from_le_bytes(bytes[22..24].try_into().unwrap()), 250);
    assert_eq!(u16::from_le_bytes(bytes[24..26].try_into().unwrap()), 50);
    assert_eq!(u16::from_le_bytes(bytes[26..28].try_into().unwrap()), 50);
    assert_eq!(bytes[40] & 0x7f, 22, "core ConfigureNotify follows Present");
    assert_eq!(
        u32::from_le_bytes(bytes[48..52].try_into().unwrap()),
        WINDOW.0,
    );
}

#[test]
fn present_default_crtc_uses_the_transformed_footprint() {
    const WINDOW: u32 = 0x0001_1001;
    let mut state = ServerState::new();
    state.randr = crate::randr::RandrState::from_outputs(
        1,
        vec![
            present_test_output(1, 11, 0, 0, 2560, 1440, true),
            present_test_output(2, 22, 2560, 0, 2560, 1440, true),
        ],
    );
    state.randr.primary_output = 1;
    // Below CRTC 22's mode but inside its 2.0 footprint.
    create_present_test_window(&mut state, WINDOW, 2600, 1500, 500, 500);
    assert_eq!(
        default_present_crtc_for_window(&state, ResourceId(WINDOW)),
        11
    );
    state.randr.outputs[1].current_transform =
        crate::randr::CrtcTransform::new(rr_scale(MUFFIN_2_0), None, Vec::new()).unwrap();
    assert_eq!(
        default_present_crtc_for_window(&state, ResourceId(WINDOW)),
        22
    );
}

/// PresentPixmapSynced 84-byte fixed prefix. Offsets per the :40644
/// test's comment: window(0) pixmap(4) serial(8) valid(12) update(16)
/// x_off(20) y_off(22) target_crtc(24) acquire_syncobj(28)
/// release_syncobj(32) acquire_point(36) release_point(44) options(52)
/// pad(56) target_msc(60) divisor(68) remainder(76).
fn pixmap_synced_body(
    window: u32,
    pixmap: u32,
    acquire_syncobj: u32,
    release_syncobj: u32,
    acquire_point: u64,
    release_point: u64,
) -> Vec<u8> {
    let mut body = vec![0u8; 84];
    body[0..4].copy_from_slice(&window.to_le_bytes());
    body[4..8].copy_from_slice(&pixmap.to_le_bytes());
    body[28..32].copy_from_slice(&acquire_syncobj.to_le_bytes());
    body[32..36].copy_from_slice(&release_syncobj.to_le_bytes());
    body[36..44].copy_from_slice(&acquire_point.to_le_bytes());
    body[44..52].copy_from_slice(&release_point.to_le_bytes());
    body
}

fn pixmap_body(window: u32, pixmap: u32) -> Vec<u8> {
    let mut body = vec![0u8; 68];
    body[0..4].copy_from_slice(&window.to_le_bytes());
    body[4..8].copy_from_slice(&pixmap.to_le_bytes());
    body
}

fn dispatch_present_body(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    client_id: ClientId,
    sequence: u16,
    minor: u8,
    body: &[u8],
) {
    process_request(
        state,
        backend,
        client_id,
        SequenceNumber(sequence),
        RequestHeader {
            opcode: 145,
            data: minor,
            length_units: u32::try_from(1 + body.len() / 4).unwrap(),
        },
        body,
        None,
    )
    .unwrap();
}

fn future_present_pixmap_request(
    state: &mut ServerState,
    window: u32,
    pixmap: u32,
    present_id: u32,
    target_msc: u64,
) -> Vec<u8> {
    if state.resources.window(ResourceId(window)).is_none() {
        create_present_test_window(state, window, 0, 0, 32, 32);
        let target = state
            .resources
            .window_mut(ResourceId(window))
            .expect("created Present window");
        target.host_xid = crate::backend::WindowHandle::from_raw(window | 0x0040_0000);
        target.map_state = crate::resources::MapState::Viewable;
    }
    state.resources.create_pixmap(
        ClientId(1),
        CreatePixmapRequest {
            depth: 24,
            pixmap: ResourceId(pixmap),
            drawable: ResourceId(window),
            width: 32,
            height: 32,
        },
    );
    let _ = state.resources.set_pixmap_host_xid(
        ResourceId(pixmap),
        crate::backend::PixmapHandle::from_raw(pixmap | 0x0040_0000)
            .expect("valid test pixmap host xid"),
    );
    let mut body = pixmap_body(window, pixmap);
    body[8..12].copy_from_slice(&present_id.to_le_bytes());
    body[44..52].copy_from_slice(&target_msc.to_le_bytes());
    body
}

fn dispatch_future_present_pixmap(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    window: u32,
    pixmap: u32,
    present_id: u32,
    target_msc: u64,
) {
    let body = future_present_pixmap_request(state, window, pixmap, present_id, target_msc);
    dispatch_present_body(
        state,
        backend,
        ClientId(1),
        u16::try_from(present_id).unwrap(),
        yserver_protocol::x11::present::PIXMAP,
        &body,
    );
}

fn run_core_present_wait_drain(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    peer: &mut UnixStream,
    sequence: u16,
) -> Vec<u8> {
    run_core_present_requests_and_wait(
        state,
        backend,
        peer,
        vec![(
            SequenceNumber(sequence),
            RequestHeader {
                opcode: 43, // GetInputFocus: round-trip after the core drains backend waits.
                data: 0,
                length_units: 1,
            },
            Vec::new(),
        )],
    )
}

fn run_core_present_request_and_wait(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    peer: &mut UnixStream,
    present_sequence: u16,
    round_trip_sequence: u16,
    minor: u8,
    body: &[u8],
) -> Vec<u8> {
    run_core_present_requests_and_wait(
        state,
        backend,
        peer,
        vec![
            (
                SequenceNumber(present_sequence),
                RequestHeader {
                    opcode: 145,
                    data: minor,
                    length_units: u32::try_from(1 + body.len() / 4).unwrap(),
                },
                body.to_vec(),
            ),
            (
                SequenceNumber(round_trip_sequence),
                RequestHeader {
                    opcode: 43, // GetInputFocus: confirms Present dispatch and wait drain.
                    data: 0,
                    length_units: 1,
                },
                Vec::new(),
            ),
        ],
    )
}

fn run_core_present_requests_and_wait(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    peer: &mut UnixStream,
    requests: Vec<(SequenceNumber, RequestHeader, Vec<u8>)>,
) -> Vec<u8> {
    use std::io::Read;

    struct ShutdownOnDrop {
        sender: crate::core_loop::CoreSender,
        active: bool,
    }

    impl Drop for ShutdownOnDrop {
        fn drop(&mut self) {
            if self.active {
                let _ = self.sender.send(crate::core_loop::Message::Shutdown);
            }
        }
    }

    let (poll, sender, receiver) = crate::core_loop::channel().expect("core channel");
    let request_sender = sender.bind();
    for (sequence, header, body) in requests {
        request_sender
            .send(crate::core_loop::Message::Request {
                id: ClientId(1),
                sequence,
                accepted_at: None,
                header,
                body,
                attached_fd: None,
            })
            .expect("queue request for run_core");
    }
    let sender_for_core = sender.clone_handle();

    std::thread::scope(|scope| {
        let runner = scope.spawn(|| {
            crate::core_loop::run_core(
                poll,
                receiver,
                sender_for_core,
                state,
                backend,
                [],
                &crate::core_loop::poll_tokens::ClientIdAllocator::new(),
                crate::core_loop::auth::AuthState::new(None),
                crate::core_loop::ResetPolicy::NoReset,
                None,
            )
        });
        let mut shutdown = ShutdownOnDrop {
            sender: sender.clone_handle(),
            active: true,
        };

        let mut reply = [0; 32];
        peer.read_exact(&mut reply)
            .expect("GetInputFocus response proves request processing completed");
        assert_eq!(reply[0], 1, "GetInputFocus reply");
        shutdown
            .sender
            .send(crate::core_loop::Message::Shutdown)
            .expect("stop run_core after its iteration tail");
        shutdown.active = false;
        runner.join().expect("run_core thread panicked").unwrap();
    });
    read_all_available(peer)
}

fn assert_present_error(peer: &mut UnixStream, expected_code: u8, expected_value: u32) {
    let mut error = [0u8; 32];
    peer.read_exact(&mut error).unwrap();
    assert_eq!(read_error(&error), expected_code);
    assert_eq!(
        u32::from_le_bytes(error[4..8].try_into().unwrap()),
        expected_value
    );
}

#[test]
fn future_synced_acquire_wait_is_superseded_without_blocking_successor_completion() {
    use crate::backend::recording::RecordedCall;
    use yserver_protocol::x11::{CreatePixmapRequest, CreateWindowRequest, present as x11present};

    const CLIENT: u32 = 1;
    const WINDOW: u32 = 0x00e0_0e01;
    const WINDOW_HOST: u32 = 0x0040_0e01;
    const OLDER_PIXMAP: u32 = 0x00e0_0e10;
    const SUCCESSOR_PIXMAP: u32 = 0x00e0_0e11;
    const OLDER_HOST: u32 = 0x0040_0e10;
    const SUCCESSOR_HOST: u32 = 0x0040_0e11;
    const ACQUIRE: u32 = 0x00e0_0e30;
    const RELEASE: u32 = 0x00e0_0e31;
    const WAIT_ID: u64 = 0x7200;
    const PRESENT_EID: u32 = 0x0020_0e40;
    const TARGET_MSC: u64 = 105;

    // Mutation: restore the `!entry.source_ready` skip; the future fence
    // waiter must be scrapped, while due fence waiters remain protected by
    // `synced_async_present_acquire_waits_survive_the_next_frame_and_execute`.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT);
    peer.set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .expect("bound core round-trip waits");
    let mut backend = RecordingBackend::new();
    backend.present_ust_msc = (100, 0x1000);
    backend.present_display_idle = false;
    backend.present_absolute_vblank_arm_supported = true;
    backend.present_scanout_blackout = false;
    backend.present_syncobj_wait = crate::backend::PresentSourceWait::Deferred(WAIT_ID);
    backend.seed_dri3_syncobj_for_test(ACQUIRE, ClientId(CLIENT));
    backend.seed_dri3_syncobj_for_test(RELEASE, ClientId(CLIENT));
    state.present_event_selections.insert(
        PRESENT_EID,
        crate::server::PresentEventSelection {
            owner: ClientId(CLIENT),
            window: ResourceId(WINDOW),
            event_mask: x11present::EVENT_MASK_COMPLETE_NOTIFY,
        },
    );

    state.resources.create_window(
        ClientId(CLIENT),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(WINDOW),
            parent: ROOT_WINDOW,
            width: 100,
            height: 100,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(WINDOW));
    state
        .resources
        .window_mut(ResourceId(WINDOW))
        .expect("destination window")
        .host_xid = crate::backend::WindowHandle::from_raw(WINDOW_HOST);
    for (pixmap, host_xid) in [
        (OLDER_PIXMAP, OLDER_HOST),
        (SUCCESSOR_PIXMAP, SUCCESSOR_HOST),
    ] {
        state.resources.create_pixmap(
            ClientId(CLIENT),
            CreatePixmapRequest {
                depth: 24,
                pixmap: ResourceId(pixmap),
                drawable: ResourceId(WINDOW),
                width: 100,
                height: 100,
            },
        );
        assert!(state.resources.set_pixmap_host_xid(
            ResourceId(pixmap),
            crate::backend::PixmapHandle::from_raw(host_xid).expect("valid test pixmap host xid"),
        ));
    }

    let mut older = pixmap_synced_body(WINDOW, OLDER_PIXMAP, ACQUIRE, RELEASE, 1, 2);
    older[8..12].copy_from_slice(&1_u32.to_le_bytes());
    older[60..68].copy_from_slice(&TARGET_MSC.to_le_bytes());
    let _ = run_core_present_request_and_wait(
        &mut state,
        &mut backend,
        &mut peer,
        1,
        2,
        x11present::PIXMAP_SYNCED,
        &older,
    );
    assert!(
        state
            .present_pending_exec
            .get(&1)
            .is_some_and(|entry| !entry.source_ready && entry.wait_id == Some(WAIT_ID))
    );

    let mut successor = pixmap_body(WINDOW, SUCCESSOR_PIXMAP);
    successor[8..12].copy_from_slice(&2_u32.to_le_bytes());
    successor[44..52].copy_from_slice(&TARGET_MSC.to_le_bytes());
    let _ = run_core_present_request_and_wait(
        &mut state,
        &mut backend,
        &mut peer,
        3,
        4,
        x11present::PIXMAP,
        &successor,
    );
    assert!(state.present_pending_exec.contains_key(&2));
    assert!(!state.present_pending_exec.contains_key(&1));
    assert!(state.present_wait_to_id.is_empty());
    assert_eq!(backend.finished_present_source_waits, vec![WAIT_ID]);
    assert_eq!(backend.present_skip_count, 1);

    // The successor was parked at MSC 105. Advancing the real run_core clock
    // executes it while the older acquire point remains unsignalled.
    backend.present_ust_msc = (TARGET_MSC, 0x2000);
    let mut wire = run_core_present_wait_drain(&mut state, &mut backend, &mut peer, 5);
    let successor_completion = backend
        .enqueued_present_completions
        .iter()
        .find(|(event, _)| event.present_id == 2)
        .expect("the executed successor enqueued its completion")
        .0
        .clone();
    backend
        .completed_present_events_to_drain
        .push(successor_completion);
    wire.extend(run_core_present_wait_drain(
        &mut state,
        &mut backend,
        &mut peer,
        6,
    ));
    let copied_sources: Vec<u32> = backend
        .calls()
        .iter()
        .filter_map(|call| match call {
            RecordedCall::CopyArea { src_host_xid, .. } => Some(*src_host_xid),
            _ => None,
        })
        .collect();
    assert_eq!(copied_sources, vec![SUCCESSOR_HOST]);
    assert_eq!(
        complete_notify_modes(&wire),
        vec![
            x11present::COMPLETE_MODE_SKIP,
            x11present::COMPLETE_MODE_COPY
        ],
        "the scrapped future waiter completes as Skip before its successor's Copy"
    );
    assert_eq!(
        *backend.signalled_dri3_syncobjs.lock().unwrap(),
        vec![(RELEASE, 2)],
        "scrapping releases the older synced buffer through its release point"
    );
    assert_eq!(backend.finished_present_source_waits, vec![WAIT_ID]);
    assert_eq!(backend.present_skip_count, 1);
    assert!(state.present_pending_exec.is_empty());
    assert!(state.present_wait_to_id.is_empty());
    assert!(state.present_pending_complete.is_empty());
    assert!(state.present_complete_gate.is_empty());
    assert!(backend.ready_present_source_waits.is_empty());
}

#[test]
fn synced_async_present_acquire_waits_survive_the_next_frame_and_execute() {
    use crate::backend::recording::RecordedCall;
    use yserver_protocol::x11::{CreatePixmapRequest, CreateWindowRequest};

    const CLIENT: u32 = 1;
    const WINDOW: u32 = 0x00e0_0b01;
    const WINDOW_HOST: u32 = 0x0040_0b01;
    const PIXMAP_BASE: u32 = 0x00e0_0b10;
    const PIXMAP_HOST_BASE: u32 = 0x0040_0b10;
    const ACQUIRE: u32 = 0x00e0_0b30;
    const RELEASE: u32 = 0x00e0_0b31;
    const WAIT_BASE: u64 = 0x7000;
    const FRAME_COUNT: u32 = 20;

    // Mutation: remove the `source_ready` gate from core supersession so a
    // successor scraps an entry still waiting on its acquire point.
    // Logical timing: submit every 3 ms and signal that frame's acquire point
    // 4 ms later, after the next submission. The explicit Backend wait ids
    // keep this schedule deterministic without sleeping.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT);
    peer.set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .expect("bound core round-trip waits");
    let mut backend = RecordingBackend::new();
    backend.present_ust_msc = (10, 0x1000);
    backend.present_flip_in_flight = true;
    backend.seed_dri3_syncobj_for_test(ACQUIRE, ClientId(CLIENT));
    backend.seed_dri3_syncobj_for_test(RELEASE, ClientId(CLIENT));

    state.resources.create_window(
        ClientId(CLIENT),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(WINDOW),
            parent: ROOT_WINDOW,
            width: 100,
            height: 100,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(WINDOW));
    state
        .resources
        .window_mut(ResourceId(WINDOW))
        .expect("destination window")
        .host_xid = crate::backend::WindowHandle::from_raw(WINDOW_HOST);

    for frame in 0..FRAME_COUNT {
        let pixmap = PIXMAP_BASE + frame;
        let pixmap_host = PIXMAP_HOST_BASE + frame;
        state.resources.create_pixmap(
            ClientId(CLIENT),
            CreatePixmapRequest {
                depth: 24,
                pixmap: ResourceId(pixmap),
                drawable: ResourceId(WINDOW),
                width: 100,
                height: 100,
            },
        );
        let _ = state.resources.set_pixmap_host_xid(
            ResourceId(pixmap),
            crate::backend::PixmapHandle::from_raw(pixmap_host)
                .expect("valid test pixmap host xid"),
        );

        let acquire_point = u64::from(frame) + 1;
        let release_point = acquire_point + 1;
        let mut body = pixmap_synced_body(
            WINDOW,
            pixmap,
            ACQUIRE,
            RELEASE,
            acquire_point,
            release_point,
        );
        body[8..12].copy_from_slice(&(frame + 1).to_le_bytes());
        body[52..56].copy_from_slice(&crate::present_scheduler::PRESENT_OPTION_ASYNC.to_le_bytes());
        let wait_id = WAIT_BASE + u64::from(frame);
        if frame > 0 {
            let preceding_present_id = u64::from(frame);
            let preceding_wait_id = WAIT_BASE + u64::from(frame - 1);
            assert!(
                state
                    .present_pending_exec
                    .get(&preceding_present_id)
                    .is_some_and(
                        |entry| !entry.source_ready && entry.wait_id == Some(preceding_wait_id)
                    ),
                "frame {frame} arrives at 3 ms while the preceding frame's 4 ms acquire wait is pending"
            );
            backend.ready_present_source_waits.push(preceding_wait_id);
        }
        backend.present_syncobj_wait = crate::backend::PresentSourceWait::Deferred(wait_id);
        let present_sequence = frame * 3 + 1;
        let _ = run_core_present_request_and_wait(
            &mut state,
            &mut backend,
            &mut peer,
            u16::try_from(present_sequence).unwrap(),
            u16::try_from(present_sequence + 1).unwrap(),
            yserver_protocol::x11::present::PIXMAP_SYNCED,
            &body,
        );
    }

    backend
        .ready_present_source_waits
        .push(WAIT_BASE + u64::from(FRAME_COUNT - 1));
    let _ = run_core_present_wait_drain(
        &mut state,
        &mut backend,
        &mut peer,
        u16::try_from(FRAME_COUNT * 3).unwrap(),
    );

    let copied_sources: Vec<u32> = backend
        .calls()
        .iter()
        .filter_map(|call| match call {
            RecordedCall::CopyArea { src_host_xid, .. } => Some(*src_host_xid),
            _ => None,
        })
        .collect();
    assert_eq!(
        copied_sources,
        (0..FRAME_COUNT)
            .map(|frame| PIXMAP_HOST_BASE + frame)
            .collect::<Vec<_>>(),
        "all 20 acquire-waiting frames execute once, in arrival order"
    );
    assert!(state.present_pending_exec.is_empty());
    assert!(state.present_wait_to_id.is_empty());
    assert!(state.present_pending_complete.is_empty());
    assert_eq!(
        backend.present_skip_count, 0,
        "no acquire-waiting frame is scrapped"
    );
    let mut completion_gates: Vec<u64> = state.present_complete_gate.keys().copied().collect();
    completion_gates.sort_unstable();
    assert_eq!(
        completion_gates,
        (1..=u64::from(FRAME_COUNT)).collect::<Vec<_>>()
    );
}

#[test]
fn ready_present_acquire_waits_in_one_drain_execute_in_arrival_order() {
    use crate::backend::recording::RecordedCall;
    use yserver_protocol::x11::{CreatePixmapRequest, CreateWindowRequest};

    const CLIENT: u32 = 1;
    const WINDOW: u32 = 0x00e0_0d01;
    const WINDOW_HOST: u32 = 0x0040_0d01;
    const PIXMAP_BASE: u32 = 0x00e0_0d10;
    const PIXMAP_HOST_BASE: u32 = 0x0040_0d10;
    const ACQUIRE: u32 = 0x00e0_0d30;
    const RELEASE: u32 = 0x00e0_0d31;
    const WAIT_BASE: u64 = 0x7100;
    const FRAME_COUNT: u32 = 3;

    // Mutation: remove the ready-wait sort by Present ID; reversed ready IDs
    // must then copy in the wrong order.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT);
    peer.set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .expect("bound core round-trip waits");
    let mut backend = RecordingBackend::new();
    backend.present_ust_msc = (10, 0x1000);
    backend.present_flip_in_flight = true;
    backend.seed_dri3_syncobj_for_test(ACQUIRE, ClientId(CLIENT));
    backend.seed_dri3_syncobj_for_test(RELEASE, ClientId(CLIENT));
    state.resources.create_window(
        ClientId(CLIENT),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(WINDOW),
            parent: ROOT_WINDOW,
            width: 100,
            height: 100,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(WINDOW));
    state
        .resources
        .window_mut(ResourceId(WINDOW))
        .expect("destination window")
        .host_xid = crate::backend::WindowHandle::from_raw(WINDOW_HOST);

    for frame in 0..FRAME_COUNT {
        let pixmap = PIXMAP_BASE + frame;
        state.resources.create_pixmap(
            ClientId(CLIENT),
            CreatePixmapRequest {
                depth: 24,
                pixmap: ResourceId(pixmap),
                drawable: ResourceId(WINDOW),
                width: 100,
                height: 100,
            },
        );
        let _ = state.resources.set_pixmap_host_xid(
            ResourceId(pixmap),
            crate::backend::PixmapHandle::from_raw(PIXMAP_HOST_BASE + frame)
                .expect("valid test pixmap host xid"),
        );

        let present_id = u64::from(frame + 1);
        let wait_id = WAIT_BASE + u64::from(frame);
        let mut body =
            pixmap_synced_body(WINDOW, pixmap, ACQUIRE, RELEASE, present_id, present_id + 1);
        body[8..12].copy_from_slice(&(frame + 1).to_le_bytes());
        body[52..56].copy_from_slice(&crate::present_scheduler::PRESENT_OPTION_ASYNC.to_le_bytes());
        backend.present_syncobj_wait = crate::backend::PresentSourceWait::Deferred(wait_id);
        let present_sequence = frame * 2 + 1;
        let _ = run_core_present_request_and_wait(
            &mut state,
            &mut backend,
            &mut peer,
            u16::try_from(present_sequence).unwrap(),
            u16::try_from(present_sequence + 1).unwrap(),
            yserver_protocol::x11::present::PIXMAP_SYNCED,
            &body,
        );
        assert!(state.present_pending_exec.contains_key(&present_id));
    }

    for frame in (0..FRAME_COUNT).rev() {
        backend
            .ready_present_source_waits
            .push(WAIT_BASE + u64::from(frame));
    }
    let _ = run_core_present_wait_drain(
        &mut state,
        &mut backend,
        &mut peer,
        u16::try_from(FRAME_COUNT * 2 + 1).unwrap(),
    );

    let copied_sources: Vec<u32> = backend
        .calls()
        .iter()
        .filter_map(|call| match call {
            RecordedCall::CopyArea { src_host_xid, .. } => Some(*src_host_xid),
            _ => None,
        })
        .collect();
    assert_eq!(
        copied_sources,
        (0..FRAME_COUNT)
            .map(|frame| PIXMAP_HOST_BASE + frame)
            .collect::<Vec<_>>(),
        "a single reversed readiness drain still executes in Present arrival order"
    );
    assert_eq!(
        backend.finished_present_source_waits,
        (0..FRAME_COUNT)
            .map(|frame| WAIT_BASE + u64::from(frame))
            .collect::<Vec<_>>(),
        "each waiter is finished once in arrival order"
    );
    assert_eq!(backend.present_skip_count, 0);
    assert!(state.present_pending_exec.is_empty());
    assert!(state.present_wait_to_id.is_empty());
    assert!(state.present_pending_complete.is_empty());
}

#[test]
fn late_older_acquire_wait_completes_as_skip_after_newer_same_target_executes() {
    use crate::backend::recording::RecordedCall;
    use yserver_protocol::x11::{CreatePixmapRequest, CreateWindowRequest, present as x11present};

    const CLIENT: u32 = 1;
    const WINDOW: u32 = 0x00e0_0c01;
    const WINDOW_HOST: u32 = 0x0040_0c01;
    const OLDER_PIXMAP: u32 = 0x00e0_0c10;
    const NEWER_PIXMAP: u32 = 0x00e0_0c11;
    const OLDER_HOST: u32 = 0x0040_0c10;
    const NEWER_HOST: u32 = 0x0040_0c11;
    const ACQUIRE: u32 = 0x00e0_0c30;
    const RELEASE: u32 = 0x00e0_0c31;
    const OLDER_WAIT: u64 = 0x7100;
    const NEWER_WAIT: u64 = 0x7101;
    const PRESENT_EID: u32 = 0x0020_0c40;

    // Mutation: remove the stale-wait check after a newer covered Present
    // executes; the older source must not be copied after the newer one.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT);
    peer.set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .expect("bound core round-trip waits");
    let mut backend = RecordingBackend::new();
    backend.present_ust_msc = (10, 0x1000);
    backend.present_flip_in_flight = true;
    backend.seed_dri3_syncobj_for_test(ACQUIRE, ClientId(CLIENT));
    backend.seed_dri3_syncobj_for_test(RELEASE, ClientId(CLIENT));
    state.present_event_selections.insert(
        PRESENT_EID,
        crate::server::PresentEventSelection {
            owner: ClientId(CLIENT),
            window: ResourceId(WINDOW),
            event_mask: x11present::EVENT_MASK_COMPLETE_NOTIFY,
        },
    );

    state.resources.create_window(
        ClientId(CLIENT),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(WINDOW),
            parent: ROOT_WINDOW,
            width: 100,
            height: 100,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(WINDOW));
    state
        .resources
        .window_mut(ResourceId(WINDOW))
        .expect("destination window")
        .host_xid = crate::backend::WindowHandle::from_raw(WINDOW_HOST);
    for (pixmap, host_xid) in [(OLDER_PIXMAP, OLDER_HOST), (NEWER_PIXMAP, NEWER_HOST)] {
        state.resources.create_pixmap(
            ClientId(CLIENT),
            CreatePixmapRequest {
                depth: 24,
                pixmap: ResourceId(pixmap),
                drawable: ResourceId(WINDOW),
                width: 100,
                height: 100,
            },
        );
        let _ = state.resources.set_pixmap_host_xid(
            ResourceId(pixmap),
            crate::backend::PixmapHandle::from_raw(host_xid).expect("valid test pixmap host xid"),
        );
    }

    for (pixmap, serial, wait_id) in [
        (OLDER_PIXMAP, 1_u32, OLDER_WAIT),
        (NEWER_PIXMAP, 2_u32, NEWER_WAIT),
    ] {
        let mut body = pixmap_synced_body(WINDOW, pixmap, ACQUIRE, RELEASE, u64::from(serial), 10);
        body[8..12].copy_from_slice(&serial.to_le_bytes());
        body[52..56].copy_from_slice(&crate::present_scheduler::PRESENT_OPTION_ASYNC.to_le_bytes());
        backend.present_syncobj_wait = crate::backend::PresentSourceWait::Deferred(wait_id);
        let present_sequence = serial * 2 - 1;
        let _ = run_core_present_request_and_wait(
            &mut state,
            &mut backend,
            &mut peer,
            u16::try_from(present_sequence).unwrap(),
            u16::try_from(present_sequence + 1).unwrap(),
            x11present::PIXMAP_SYNCED,
            &body,
        );
    }
    assert_eq!(state.present_pending_exec.len(), 2);

    backend.ready_present_source_waits.push(NEWER_WAIT);
    let _ = run_core_present_wait_drain(&mut state, &mut backend, &mut peer, 5);
    assert_eq!(state.present_pending_exec.len(), 1);
    assert!(state.present_pending_exec.contains_key(&1));

    backend.ready_present_source_waits.push(OLDER_WAIT);
    let late_older_wire = run_core_present_wait_drain(&mut state, &mut backend, &mut peer, 6);

    let copied_sources: Vec<u32> = backend
        .calls()
        .iter()
        .filter_map(|call| match call {
            RecordedCall::CopyArea { src_host_xid, .. } => Some(*src_host_xid),
            _ => None,
        })
        .collect();
    assert_eq!(copied_sources, vec![NEWER_HOST]);
    assert_eq!(
        complete_notify_modes(&late_older_wire),
        vec![x11present::COMPLETE_MODE_SKIP],
        "the older frame completes as Skip only after its acquire point signals"
    );
    assert_eq!(
        *backend.signalled_dri3_syncobjs.lock().unwrap(),
        vec![(RELEASE, 10)],
        "the skipped older Present releases its synced buffer after its wait resolves"
    );
    assert_eq!(backend.present_skip_count, 1);
    assert!(state.present_pending_exec.is_empty());
    assert!(state.present_wait_to_id.is_empty());
    assert!(state.present_pending_complete.is_empty());
    assert_eq!(
        state
            .present_complete_gate
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        vec![2]
    );
}

fn dispatch_pixmap_synced(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    client_id: yserver_protocol::x11::ClientId,
    body: &[u8],
) {
    process_request(
        state,
        backend,
        client_id,
        SequenceNumber(1),
        RequestHeader {
            opcode: 145,
            data: yserver_protocol::x11::present::PIXMAP_SYNCED,
            length_units: u32::try_from(1 + body.len() / 4).unwrap(),
        },
        body,
        None,
    )
    .unwrap();
}

#[test]
fn present_pixmap_common_validation_order_and_card32_remainder() {
    const PIXMAP: u32 = 0x0010_2001;
    const VALID: u32 = 0x0010_2002;
    const UPDATE: u32 = 0x0010_2003;
    const BAD_CRTC: u32 = 0x0010_2004;
    const WAIT_FENCE: u32 = 0x0010_2005;
    const IDLE_FENCE: u32 = 0x0010_2006;
    const BAD_OPTIONS: u32 = 0x8000_001f;
    const LARGE_REMAINDER: u64 = 0x1_9abc_def0;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    state.resources.create_pixmap(
        ClientId(1),
        CreatePixmapRequest {
            depth: 24,
            pixmap: ResourceId(PIXMAP),
            drawable: ROOT_WINDOW,
            width: 16,
            height: 16,
        },
    );

    let mut body = pixmap_body(ROOT_WINDOW.0, PIXMAP);
    body[12..16].copy_from_slice(&VALID.to_le_bytes());
    body[16..20].copy_from_slice(&UPDATE.to_le_bytes());
    body[24..28].copy_from_slice(&BAD_CRTC.to_le_bytes());
    body[28..32].copy_from_slice(&WAIT_FENCE.to_le_bytes());
    body[32..36].copy_from_slice(&IDLE_FENCE.to_le_bytes());
    body[36..40].copy_from_slice(&BAD_OPTIONS.to_le_bytes());
    body[60..68].copy_from_slice(&LARGE_REMAINDER.to_le_bytes());

    dispatch_present_body(
        &mut state,
        &mut backend,
        ClientId(1),
        1,
        yserver_protocol::x11::present::PIXMAP,
        &body,
    );
    assert_present_error(&mut peer, XFIXES_BAD_REGION, VALID);

    state.xfixes_regions.insert(
        VALID,
        crate::server::XFixesRegion {
            owner: ClientId(1),
            region: crate::region::Region::default(),
        },
    );
    dispatch_present_body(
        &mut state,
        &mut backend,
        ClientId(1),
        2,
        yserver_protocol::x11::present::PIXMAP,
        &body,
    );
    assert_present_error(&mut peer, XFIXES_BAD_REGION, UPDATE);

    state.xfixes_regions.insert(
        UPDATE,
        crate::server::XFixesRegion {
            owner: ClientId(1),
            region: crate::region::Region::default(),
        },
    );
    dispatch_present_body(
        &mut state,
        &mut backend,
        ClientId(1),
        3,
        yserver_protocol::x11::present::PIXMAP,
        &body,
    );
    assert_present_error(&mut peer, RANDR_BAD_CRTC, BAD_CRTC);

    body[24..28].copy_from_slice(&0u32.to_le_bytes());
    dispatch_present_body(
        &mut state,
        &mut backend,
        ClientId(1),
        4,
        yserver_protocol::x11::present::PIXMAP,
        &body,
    );
    assert_present_error(&mut peer, SYNC_BAD_FENCE, WAIT_FENCE);

    state.sync_fences.insert(
        WAIT_FENCE,
        crate::server::SyncFence {
            owner: ClientId(1),
            triggered: false,
        },
    );
    dispatch_present_body(
        &mut state,
        &mut backend,
        ClientId(1),
        5,
        yserver_protocol::x11::present::PIXMAP,
        &body,
    );
    assert_present_error(&mut peer, SYNC_BAD_FENCE, IDLE_FENCE);

    state.sync_fences.insert(
        IDLE_FENCE,
        crate::server::SyncFence {
            owner: ClientId(1),
            triggered: false,
        },
    );
    dispatch_present_body(
        &mut state,
        &mut backend,
        ClientId(1),
        6,
        yserver_protocol::x11::present::PIXMAP,
        &body,
    );
    assert_present_error(&mut peer, x11::error::BAD_VALUE, BAD_OPTIONS);

    body[36..40].copy_from_slice(&0u32.to_le_bytes());
    dispatch_present_body(
        &mut state,
        &mut backend,
        ClientId(1),
        7,
        yserver_protocol::x11::present::PIXMAP,
        &body,
    );
    assert_present_error(&mut peer, x11::error::BAD_VALUE, LARGE_REMAINDER as u32);
    assert!(!state.present_window_msc.contains_key(&ROOT_WINDOW.0));
}

#[test]
fn present_pixmap_synced_validation_order_and_card32_remainder() {
    const PIXMAP: u32 = 0x0010_2101;
    const ACQUIRE: u32 = 0x0010_2102;
    const RELEASE: u32 = 0x0010_2103;
    const BAD_ACQUIRE: u32 = 0x0010_21a2;
    const BAD_RELEASE: u32 = 0x0010_21a3;
    const VALID: u32 = 0x0010_2104;
    const UPDATE: u32 = 0x0010_2105;
    const BAD_CRTC: u32 = 0x0010_2106;
    const BAD_OPTIONS: u32 = 0x4000_001f;
    const LARGE_REMAINDER: u64 = 0x1_7654_3210;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    backend.seed_dri3_syncobj_for_test(ACQUIRE, ClientId(1));
    backend.seed_dri3_syncobj_for_test(RELEASE, ClientId(1));
    state.resources.create_pixmap(
        ClientId(1),
        CreatePixmapRequest {
            depth: 24,
            pixmap: ResourceId(PIXMAP),
            drawable: ROOT_WINDOW,
            width: 16,
            height: 16,
        },
    );

    let mut body = pixmap_synced_body(ROOT_WINDOW.0, PIXMAP, BAD_ACQUIRE, RELEASE, 1, 2);
    body[12..16].copy_from_slice(&VALID.to_le_bytes());
    body[16..20].copy_from_slice(&UPDATE.to_le_bytes());
    body[24..28].copy_from_slice(&BAD_CRTC.to_le_bytes());
    body[52..56].copy_from_slice(&BAD_OPTIONS.to_le_bytes());
    body[76..84].copy_from_slice(&LARGE_REMAINDER.to_le_bytes());

    dispatch_present_body(
        &mut state,
        &mut backend,
        ClientId(1),
        1,
        yserver_protocol::x11::present::PIXMAP_SYNCED,
        &body,
    );
    assert_present_error(&mut peer, x11::error::BAD_VALUE, BAD_ACQUIRE);

    body[28..32].copy_from_slice(&ACQUIRE.to_le_bytes());
    body[32..36].copy_from_slice(&BAD_RELEASE.to_le_bytes());
    dispatch_present_body(
        &mut state,
        &mut backend,
        ClientId(1),
        2,
        yserver_protocol::x11::present::PIXMAP_SYNCED,
        &body,
    );
    assert_present_error(&mut peer, x11::error::BAD_VALUE, BAD_RELEASE);

    body[32..36].copy_from_slice(&RELEASE.to_le_bytes());
    body[36..44].copy_from_slice(&0u64.to_le_bytes());
    dispatch_present_body(
        &mut state,
        &mut backend,
        ClientId(1),
        3,
        yserver_protocol::x11::present::PIXMAP_SYNCED,
        &body,
    );
    assert_present_error(&mut peer, x11::error::BAD_VALUE, 0);

    body[36..44].copy_from_slice(&1u64.to_le_bytes());
    dispatch_present_body(
        &mut state,
        &mut backend,
        ClientId(1),
        4,
        yserver_protocol::x11::present::PIXMAP_SYNCED,
        &body,
    );
    assert_present_error(&mut peer, XFIXES_BAD_REGION, VALID);

    state.xfixes_regions.insert(
        VALID,
        crate::server::XFixesRegion {
            owner: ClientId(1),
            region: crate::region::Region::default(),
        },
    );
    dispatch_present_body(
        &mut state,
        &mut backend,
        ClientId(1),
        5,
        yserver_protocol::x11::present::PIXMAP_SYNCED,
        &body,
    );
    assert_present_error(&mut peer, XFIXES_BAD_REGION, UPDATE);

    state.xfixes_regions.insert(
        UPDATE,
        crate::server::XFixesRegion {
            owner: ClientId(1),
            region: crate::region::Region::default(),
        },
    );
    dispatch_present_body(
        &mut state,
        &mut backend,
        ClientId(1),
        6,
        yserver_protocol::x11::present::PIXMAP_SYNCED,
        &body,
    );
    assert_present_error(&mut peer, RANDR_BAD_CRTC, BAD_CRTC);

    body[24..28].copy_from_slice(&0u32.to_le_bytes());
    dispatch_present_body(
        &mut state,
        &mut backend,
        ClientId(1),
        7,
        yserver_protocol::x11::present::PIXMAP_SYNCED,
        &body,
    );
    assert_present_error(&mut peer, x11::error::BAD_VALUE, BAD_OPTIONS);

    body[52..56].copy_from_slice(&0u32.to_le_bytes());
    dispatch_present_body(
        &mut state,
        &mut backend,
        ClientId(1),
        8,
        yserver_protocol::x11::present::PIXMAP_SYNCED,
        &body,
    );
    assert_present_error(&mut peer, x11::error::BAD_VALUE, LARGE_REMAINDER as u32);
    assert!(!state.present_window_msc.contains_key(&ROOT_WINDOW.0));
}

#[test]
fn present_pixmap_synced_unknown_acquire_syncobj_is_bad_value() {
    use yserver_protocol::x11::{ClientId, CreatePixmapRequest, CreateWindowRequest};
    const WINDOW_XID: u32 = 0x00e0_0403;
    const PIXMAP_XID: u32 = 0x00e0_0404;
    const ACQUIRE_SYNCOBJ: u32 = 0x00e0_0bad; // never imported
    const RELEASE_SYNCOBJ: u32 = 0x00e0_0408;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 17);
    let mut backend = RecordingBackend::new();
    // The release syncobj is a valid, imported resource; the acquire is
    // NOT. Row 4: an unknown acquire xid must produce a Value error, not
    // a silent no-reply hang.
    backend.seed_dri3_syncobj_for_test(RELEASE_SYNCOBJ, ClientId(17));

    state.resources.create_window(
        ClientId(17),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(WINDOW_XID),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 800,
            height: 600,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(WINDOW_XID));
    if let Some(w) = state.resources.window_mut(ResourceId(WINDOW_XID)) {
        w.host_xid = crate::backend::WindowHandle::from_raw(0x400403);
    }
    state.resources.create_pixmap(
        ClientId(17),
        CreatePixmapRequest {
            depth: 24,
            pixmap: ResourceId(PIXMAP_XID),
            drawable: ResourceId(WINDOW_XID),
            width: 800,
            height: 600,
        },
    );
    let _ = state.resources.set_pixmap_host_xid(
        ResourceId(PIXMAP_XID),
        crate::backend::PixmapHandle::from_raw(0x400404).expect("valid host pixmap"),
    );

    let mut body = pixmap_synced_body(
        WINDOW_XID,
        PIXMAP_XID,
        ACQUIRE_SYNCOBJ,
        RELEASE_SYNCOBJ,
        1,
        1,
    );
    body[76..84].copy_from_slice(&1u64.to_le_bytes());
    dispatch_pixmap_synced(&mut state, &mut backend, ClientId(17), &body);

    peer.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 32];
    peer.read_exact(&mut buf).unwrap();
    assert_eq!(
        read_error(&buf),
        x11::error::BAD_VALUE,
        "unknown acquire syncobj must be BadValue, not a silent no-reply wait",
    );
    // X error: bytes 4-7 are the bad-value argument. It must carry the
    // offending xid, mirroring Xorg's VERIFY_DRI3_SYNCOBJ
    // (client->errorValue = id).
    assert_eq!(
        u32::from_le_bytes(buf[4..8].try_into().unwrap()),
        ACQUIRE_SYNCOBJ,
        "BadValue must carry the offending syncobj xid",
    );
}

#[test]
fn rejected_synced_depth_mismatch_does_not_change_remembered_crtc() {
    const CLIENT: u32 = 17;
    const WINDOW: u32 = 0x00e0_0451;
    const PIXMAP: u32 = 0x00e0_0452;
    const ACQUIRE: u32 = 0x00e0_0453;
    const RELEASE: u32 = 0x00e0_0454;
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT);
    state.randr = crate::randr::RandrState::from_outputs(
        1,
        vec![
            present_test_output(1, 11, 0, 0, 100, 100, true),
            present_test_output(2, 22, 100, 0, 100, 100, true),
        ],
    );
    state.resources.create_window(
        ClientId(CLIENT),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(WINDOW),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 64,
            height: 64,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    state
        .resources
        .window_mut(ResourceId(WINDOW))
        .unwrap()
        .host_xid = crate::backend::WindowHandle::from_raw(0x0040_0451);
    state.resources.create_pixmap(
        ClientId(CLIENT),
        CreatePixmapRequest {
            depth: 32,
            pixmap: ResourceId(PIXMAP),
            drawable: ResourceId(WINDOW),
            width: 64,
            height: 64,
        },
    );
    let _ = state.resources.set_pixmap_host_xid(
        ResourceId(PIXMAP),
        crate::backend::PixmapHandle::from_raw(0x0040_0452).unwrap(),
    );
    let mut backend = RecordingBackend::new();
    backend.seed_dri3_syncobj_for_test(ACQUIRE, ClientId(CLIENT));
    backend.seed_dri3_syncobj_for_test(RELEASE, ClientId(CLIENT));
    let original = select_present_domain(&mut state, &mut backend, WINDOW, 11, false).unwrap();

    let mut body = pixmap_synced_body(WINDOW, PIXMAP, ACQUIRE, RELEASE, 1, 1);
    body[24..28].copy_from_slice(&22u32.to_le_bytes());
    dispatch_pixmap_synced(&mut state, &mut backend, ClientId(CLIENT), &body);
    let mut error = [0u8; 32];
    peer.read_exact(&mut error).unwrap();
    assert_eq!(error[1], x11::error::BAD_MATCH);
    let remembered = state.present_window_msc[&WINDOW];
    assert_eq!(remembered.last_crtc, original.crtc_id);
    assert_eq!(remembered.last_crtc_epoch, original.crtc_epoch);
    assert!(!state.present_window_generations.contains_key(&WINDOW));
}

#[test]
fn present_pixmap_synced_zero_point_is_bad_value() {
    use yserver_protocol::x11::ClientId;
    const WINDOW_XID: u32 = 0x00e0_0403;
    const PIXMAP_XID: u32 = 0x00e0_0404;
    const SYNCOBJ: u32 = 0x00e0_0407;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 17);
    let mut backend = RecordingBackend::new();
    backend.seed_dri3_syncobj_for_test(SYNCOBJ, ClientId(17));

    // Both syncobjs imported; acquire_point == 0 is the violation.
    dispatch_pixmap_synced(
        &mut state,
        &mut backend,
        ClientId(17),
        &pixmap_synced_body(WINDOW_XID, PIXMAP_XID, SYNCOBJ, SYNCOBJ, 0, 2),
    );

    peer.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 32];
    peer.read_exact(&mut buf).unwrap();
    assert_eq!(
        read_error(&buf),
        x11::error::BAD_VALUE,
        "acquire_point 0 must be BadValue"
    );
}

#[test]
fn present_pixmap_synced_acquire_gte_release_is_bad_value() {
    use yserver_protocol::x11::ClientId;
    const WINDOW_XID: u32 = 0x00e0_0403;
    const PIXMAP_XID: u32 = 0x00e0_0404;
    const SYNCOBJ: u32 = 0x00e0_0407;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 17);
    let mut backend = RecordingBackend::new();
    backend.seed_dri3_syncobj_for_test(SYNCOBJ, ClientId(17));

    // Same syncobj for acquire and release: acquire_value >=
    // release_value is the violation.
    dispatch_pixmap_synced(
        &mut state,
        &mut backend,
        ClientId(17),
        &pixmap_synced_body(WINDOW_XID, PIXMAP_XID, SYNCOBJ, SYNCOBJ, 5, 5),
    );

    peer.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 32];
    peer.read_exact(&mut buf).unwrap();
    assert_eq!(
        read_error(&buf),
        x11::error::BAD_VALUE,
        "acquire_value >= release_value on the same syncobj must be BadValue",
    );
}

#[test]
fn present_pixmap_synced_another_clients_acquire_syncobj_is_bad_value() {
    use yserver_protocol::x11::{ClientId, CreatePixmapRequest, CreateWindowRequest};
    const WINDOW_XID: u32 = 0x00e0_0403;
    const PIXMAP_XID: u32 = 0x00e0_0404;
    const ACQUIRE_SYNCOBJ: u32 = 0x00e0_0bad; // owned by client A (17)
    const RELEASE_SYNCOBJ: u32 = 0x00e0_0408; // owned by client B (18)

    let mut state = ServerState::new();
    let _peer_a = install_client(&mut state, 17);
    let mut peer_b = install_client(&mut state, 18);
    let mut backend = RecordingBackend::new();
    backend.seed_dri3_syncobj_for_test(ACQUIRE_SYNCOBJ, ClientId(17));
    backend.seed_dri3_syncobj_for_test(RELEASE_SYNCOBJ, ClientId(18));

    state.resources.create_window(
        ClientId(17),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(WINDOW_XID),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 800,
            height: 600,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(WINDOW_XID));
    if let Some(w) = state.resources.window_mut(ResourceId(WINDOW_XID)) {
        w.host_xid = crate::backend::WindowHandle::from_raw(0x400403);
    }
    state.resources.create_pixmap(
        ClientId(17),
        CreatePixmapRequest {
            depth: 24,
            pixmap: ResourceId(PIXMAP_XID),
            drawable: ResourceId(WINDOW_XID),
            width: 800,
            height: 600,
        },
    );
    let _ = state.resources.set_pixmap_host_xid(
        ResourceId(PIXMAP_XID),
        crate::backend::PixmapHandle::from_raw(0x400404).expect("valid host pixmap"),
    );

    // B presents using A's acquire syncobj. Xorg's VERIFY_DRI3_SYNCOBJ is
    // client-scoped, so the ownership check must reject B as BadValue —
    // otherwise B could advance A's timeline on completion and corrupt A's
    // buffer reuse.
    dispatch_pixmap_synced(
        &mut state,
        &mut backend,
        ClientId(18),
        &pixmap_synced_body(
            WINDOW_XID,
            PIXMAP_XID,
            ACQUIRE_SYNCOBJ,
            RELEASE_SYNCOBJ,
            1,
            1,
        ),
    );

    peer_b.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 32];
    peer_b.read_exact(&mut buf).unwrap();
    assert_eq!(
        read_error(&buf),
        x11::error::BAD_VALUE,
        "B must not be able to present against A's acquire syncobj",
    );
    // X error: bytes 4-7 are the bad-value argument — the offending
    // acquire xid, mirroring Xorg's VERIFY_DRI3_SYNCOBJ.
    assert_eq!(
        u32::from_le_bytes(buf[4..8].try_into().unwrap()),
        ACQUIRE_SYNCOBJ,
        "BadValue must carry the offending acquire xid",
    );
}

#[test]
fn present_default_crtc_uses_overlap_primary_tie_and_stable_order() {
    const WINDOW: u32 = 0x0001_1001;
    let mut state = ServerState::new();
    state.randr = crate::randr::RandrState::from_outputs(
        1,
        vec![
            present_test_output(2, 22, 0, 0, 100, 100, true),
            present_test_output(1, 11, 100, 0, 100, 100, true),
        ],
    );
    state.randr.primary_output = 1;
    create_present_test_window(&mut state, WINDOW, 50, 0, 100, 50);

    assert_eq!(
        default_present_crtc_for_window(&state, ResourceId(WINDOW)),
        11,
        "the primary output wins an equal-area overlap"
    );
    state.randr.primary_output = 999;
    assert_eq!(
        default_present_crtc_for_window(&state, ResourceId(WINDOW)),
        22,
        "without a primary tie, stable output enumeration wins"
    );
    let window = state.resources.window_mut(ResourceId(WINDOW)).unwrap();
    window.x = 10;
    window.width = 70;
    assert_eq!(
        default_present_crtc_for_window(&state, ResourceId(WINDOW)),
        22,
        "greatest intersection wins before tie-breaking"
    );

    for output in &mut state.randr.outputs {
        output.mode_id = 0;
    }
    assert_eq!(
        default_present_crtc_for_window(&state, ResourceId(WINDOW)),
        0,
        "zero enabled outputs use the synthetic headless domain"
    );
}

#[test]
fn explicit_off_crtc_is_valid_but_unpaced() {
    const CRTC: u32 = 55;
    const WINDOW: u32 = 0x0001_1051;
    const PIXMAP: u32 = 0x0001_1052;
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    state.randr = crate::randr::RandrState::from_outputs(
        1,
        vec![present_test_output(5, CRTC, 0, 0, 1920, 1080, false)],
    );
    create_present_test_window(&mut state, WINDOW, 0, 0, 64, 64);
    {
        let window = state.resources.window_mut(ResourceId(WINDOW)).unwrap();
        window.host_xid = crate::backend::WindowHandle::from_raw(0x0040_1051);
        // A Present to an unviewable window copies nothing (Xorg micopy.c:157).
        window.map_state = crate::resources::MapState::Viewable;
    }
    state.resources.create_pixmap(
        ClientId(1),
        CreatePixmapRequest {
            depth: 24,
            pixmap: ResourceId(PIXMAP),
            drawable: ResourceId(WINDOW),
            width: 64,
            height: 64,
        },
    );
    let _ = state.resources.set_pixmap_host_xid(
        ResourceId(PIXMAP),
        crate::backend::PixmapHandle::from_raw(0x0040_1052).unwrap(),
    );
    let mut backend = RecordingBackend::new();
    backend.present_ust_msc_by_crtc.insert(CRTC, (999, 0x1234));

    let domain = select_present_domain(&mut state, &mut backend, ROOT_WINDOW.0, CRTC, false)
        .expect("a valid-but-Off RANDR CRTC is accepted");
    assert_eq!((domain.crtc_id, domain.crtc_epoch), (CRTC, 0));
    assert_eq!((domain.raw_msc, domain.raw_ust), (0, 0));
    assert_eq!(
        effective_present_target_raw(domain, 500, 7, 3, 0),
        None,
        "Off CRTCs degrade to immediate/unpaced operation"
    );

    let mut body = vec![0u8; 68];
    body[0..4].copy_from_slice(&WINDOW.to_le_bytes());
    body[4..8].copy_from_slice(&PIXMAP.to_le_bytes());
    body[24..28].copy_from_slice(&CRTC.to_le_bytes());
    body[44..52].copy_from_slice(&500u64.to_le_bytes());
    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 145,
            data: yserver_protocol::x11::present::PIXMAP,
            length_units: 18,
        },
        &body,
        None,
    )
    .unwrap();
    assert!(state.present_pending_exec.is_empty());
    assert!(state.present_complete_gate.is_empty());
    assert!(backend.calls().iter().any(|call| matches!(
        call,
        RecordedCall::CopyArea {
            src_host_xid: 0x0040_1052,
            dst_host_xid: 0x0040_1051,
            ..
        }
    )));
    assert!(read_all_available(&mut peer).is_empty(), "no X error");
}

#[test]
fn clocked_async_present_keeps_xorg_current_msc_target_identity() {
    let domain = PresentDomainSelection {
        crtc_id: 55,
        crtc_epoch: 7,
        msc_offset: 0,
        raw_msc: 500,
        raw_ust: 50_000,
    };

    assert_eq!(
        effective_present_target_raw(
            domain,
            0,
            0,
            0,
            crate::present_scheduler::PRESENT_OPTION_ASYNC,
        ),
        Some(500),
        "Xorg retains the current CRTC MSC as the target identity for an immediate async Present",
    );
}

#[test]
fn invalid_explicit_present_crtc_is_bad_crtc_with_error_value() {
    const PIXMAP: u32 = 0x0001_1101;
    const BAD_CRTC: u32 = 0xdead_beef;
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    state.resources.create_pixmap(
        ClientId(1),
        CreatePixmapRequest {
            depth: 24,
            pixmap: ResourceId(PIXMAP),
            drawable: ROOT_WINDOW,
            width: 16,
            height: 16,
        },
    );
    let mut body = vec![0u8; 68];
    body[0..4].copy_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    body[4..8].copy_from_slice(&PIXMAP.to_le_bytes());
    body[24..28].copy_from_slice(&BAD_CRTC.to_le_bytes());
    body[60..68].copy_from_slice(&1u64.to_le_bytes());
    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 145,
            data: yserver_protocol::x11::present::PIXMAP,
            length_units: 18,
        },
        &body,
        None,
    )
    .unwrap();

    let mut error = [0u8; 32];
    peer.read_exact(&mut error).unwrap();
    assert_eq!(error[0], 0);
    assert_eq!(error[1], RANDR_BAD_CRTC);
    assert_eq!(
        u32::from_le_bytes(error[4..8].try_into().unwrap()),
        BAD_CRTC
    );
    assert!(
        !state.present_window_msc.contains_key(&ROOT_WINDOW.0),
        "a rejected request must not bind the window domain"
    );
}

#[test]
fn notify_msc_reuses_last_present_domain_while_pixmap_reselects() {
    const WINDOW: u32 = 0x0001_1201;
    let mut state = ServerState::new();
    state.randr = crate::randr::RandrState::from_outputs(
        1,
        vec![
            present_test_output(1, 11, 0, 0, 100, 100, true),
            present_test_output(2, 22, 100, 0, 100, 100, true),
        ],
    );
    create_present_test_window(&mut state, WINDOW, 0, 0, 50, 50);
    let mut backend = RecordingBackend::new();

    let explicit = select_present_domain(&mut state, &mut backend, WINDOW, 22, false).unwrap();
    assert_eq!(explicit.crtc_id, 22);
    let notify = select_present_domain(&mut state, &mut backend, WINDOW, 0, true).unwrap();
    assert_eq!(notify.crtc_id, 22, "NotifyMSC reuses the remembered CRTC");
    let pixmap = select_present_domain(&mut state, &mut backend, WINDOW, 0, false).unwrap();
    assert_eq!(pixmap.crtc_id, 11, "Pixmap reselects from current coverage");
}

#[test]
fn domain_switch_preserves_window_msc_and_unshifted_remainder_snapshot() {
    const WINDOW: u32 = 0x0001_1301;
    let mut state = ServerState::new();
    state.randr = crate::randr::RandrState::from_outputs(
        1,
        vec![
            present_test_output(1, 11, 0, 0, 100, 100, true),
            present_test_output(2, 22, 100, 0, 100, 100, true),
        ],
    );
    create_present_test_window(&mut state, WINDOW, 0, 0, 50, 50);
    let mut backend = RecordingBackend::new();
    backend.present_crtc_clock_epoch_by_crtc.insert(11, 1);
    backend.present_crtc_clock_epoch_by_crtc.insert(22, 7);
    backend.present_ust_msc_by_crtc.insert(11, (100, 1));
    backend.present_ust_msc_by_crtc.insert(22, (1_007, 2));

    let first = select_present_domain(&mut state, &mut backend, WINDOW, 11, false).unwrap();
    assert_eq!(first.raw_msc.wrapping_sub(first.msc_offset), 100);
    backend.present_ust_msc_by_crtc.insert(11, (120, 3));
    let switched = select_present_domain(&mut state, &mut backend, WINDOW, 22, false).unwrap();
    assert_eq!(switched.msc_offset, 887);
    assert_eq!(switched.raw_msc.wrapping_sub(switched.msc_offset), 120);

    let effective = effective_present_target_raw(switched, 100, 10, 3, 0).unwrap();
    assert_eq!(effective, 1_013);
    assert_eq!(
        effective % 10,
        3,
        "the wire remainder is not offset-shifted"
    );

    let mut pending =
        present_pending_entry_with(700, WINDOW, 0x0040_1302, Some(effective), true).pending;
    pending.crtc_id = switched.crtc_id;
    pending.crtc_epoch = switched.crtc_epoch;
    pending.msc_offset = switched.msc_offset;
    let event = completed_event_for_pending(&pending);
    backend.present_ust_msc_by_crtc.insert(11, (200, 4));
    let _later = select_present_domain(&mut state, &mut backend, WINDOW, 11, false).unwrap();
    assert_eq!(
        event.msc_offset, 887,
        "in-flight events retain their offset snapshot"
    );
    assert_eq!(
        present_wire_clock(
            crate::backend::PresentClockSample {
                msc: effective,
                ust: 5,
                source: crate::backend::PresentClockSource::PageFlip,
            },
            event.msc_offset,
        )
        .msc,
        126
    );
}

#[test]
fn notify_msc_reaches_wrapped_raw_target_after_domain_switch() {
    const WINDOW: u32 = 0x0001_1351;
    let mut state = ServerState::new();
    state.randr = crate::randr::RandrState::from_outputs(
        1,
        vec![
            present_test_output(1, 11, 0, 0, 100, 100, true),
            present_test_output(2, 22, 100, 0, 100, 100, true),
        ],
    );
    create_present_test_window(&mut state, WINDOW, 0, 0, 50, 50);
    let mut backend = RecordingBackend::new();
    backend.present_crtc_clock_epoch_by_crtc.insert(11, 1);
    backend.present_crtc_clock_epoch_by_crtc.insert(22, 1);
    backend.present_ust_msc_by_crtc.insert(11, (20, 1));
    backend.present_ust_msc_by_crtc.insert(22, (10, 2));
    let _ = select_present_domain(&mut state, &mut backend, WINDOW, 11, false).unwrap();
    let switched = select_present_domain(&mut state, &mut backend, WINDOW, 22, false).unwrap();
    assert_eq!(switched.msc_offset, u64::MAX - 9);
    let raw_target = 9u64.wrapping_add(switched.msc_offset);
    assert_eq!(raw_target, u64::MAX);
    state
        .present_pending_msc
        .push(crate::server::PendingNotifyMsc {
            owner: ClientId(1),
            window: WINDOW,
            crtc_id: 22,
            crtc_epoch: 1,
            msc_offset: switched.msc_offset,
            serial: 1,
            target_msc: raw_target,
            divisor: 0,
            remainder: 0,
            byte_order: ClientByteOrder::LittleEndian,
        });

    fire_due_present_notify_msc_for_domain(&mut state, 22, 1, 1, 3, false);
    assert!(
        state.present_pending_msc.is_empty(),
        "raw MSC 1 is after wrapped target u64::MAX"
    );
}

#[test]
fn same_crtc_new_epoch_rebases_from_cached_old_raw_clock() {
    const WINDOW: u32 = 0x0001_1401;
    const CRTC: u32 = 11;
    let mut state = ServerState::new();
    state.randr = crate::randr::RandrState::from_outputs(
        1,
        vec![present_test_output(1, CRTC, 0, 0, 100, 100, true)],
    );
    create_present_test_window(&mut state, WINDOW, 0, 0, 50, 50);
    let mut backend = RecordingBackend::new();
    backend.present_crtc_clock_epoch_by_crtc.insert(CRTC, 1);
    backend.present_ust_msc_by_crtc.insert(CRTC, (100, 1));
    let _ = select_present_domain(&mut state, &mut backend, WINDOW, CRTC, false).unwrap();
    backend.present_ust_msc_by_crtc.insert(CRTC, (120, 2));
    refresh_present_crtc_general_clock(&mut state, &mut backend, CRTC, 1);

    backend.present_crtc_clock_epoch_by_crtc.insert(CRTC, 2);
    backend.present_ust_msc_by_crtc.insert(CRTC, (1_000, 3));
    let rebound = select_present_domain(&mut state, &mut backend, WINDOW, CRTC, false).unwrap();
    assert_eq!(rebound.msc_offset, 880);
    assert_eq!(rebound.raw_msc.wrapping_sub(rebound.msc_offset), 120);
    assert!(state.present_crtc_clocks.contains_key(&(CRTC, 1)));
    assert!(state.present_crtc_clocks.contains_key(&(CRTC, 2)));
}

#[test]
fn exact_completion_sample_does_not_regress_domain_due_clock() {
    let mut state = ServerState::new();
    seed_present_domain_clock(&mut state, 11, 1, 200, 2);
    let older = crate::backend::PresentClockSample {
        msc: 150,
        ust: 1,
        source: crate::backend::PresentClockSource::PageFlip,
    };
    let mut backend = RecordingBackend::new();
    let returned =
        refresh_present_crtc_completion_clock(&mut state, &mut backend, 11, 1, Some(older));
    assert_eq!(returned.msc, 150, "the event retains its exact stamp");
    assert_eq!(
        state.present_crtc_clocks[&(11, 1)].completion.msc,
        200,
        "the shared due clock never regresses"
    );
    fire_due_present_completions_for_domain(&mut state, &mut backend, 11, 1, older);
    assert_eq!(state.present_crtc_clocks[&(11, 1)].completion.msc, 200);
}

#[test]
fn older_exact_sample_stamps_event_but_domain_clock_decides_due() {
    const WINDOW: u32 = 0x0001_1451;
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    state.present_event_selections.insert(
        0x0001_14e1,
        crate::server::PresentEventSelection {
            owner: ClientId(1),
            window: ResourceId(WINDOW),
            event_mask: yserver_protocol::x11::present::EVENT_MASK_COMPLETE_NOTIFY,
        },
    );
    seed_present_domain_clock(&mut state, 11, 1, 120, 12);
    let mut pending = due_pending_complete(
        WINDOW,
        750,
        100,
        yserver_protocol::x11::present::COMPLETE_MODE_COPY,
        false,
    );
    pending.event.crtc_id = 11;
    pending.event.crtc_epoch = 1;
    pending.event.completion_clock = Some(crate::backend::PresentClockSample {
        msc: 90,
        ust: 9,
        source: crate::backend::PresentClockSource::PageFlip,
    });
    state.present_pending_complete.push(pending);
    let mut backend = RecordingBackend::new();
    backend.present_crtc_clock_epoch_by_crtc.insert(11, 1);

    fire_due_present_completions_for_domain(
        &mut state,
        &mut backend,
        11,
        1,
        crate::backend::PresentClockSample {
            msc: 120,
            ust: 12,
            source: crate::backend::PresentClockSource::PageFlip,
        },
    );
    assert!(
        state.present_pending_complete.is_empty(),
        "cached domain MSC 120 releases target 100 despite exact stamp 90"
    );
    let mut event = [0u8; 40];
    peer.read_exact(&mut event).unwrap();
    assert_eq!(
        u64::from_le_bytes(event[32..40].try_into().unwrap()),
        90,
        "the grouped-direct reference sample remains the wire timestamp"
    );
}

#[test]
fn supersession_never_crosses_crtc_or_epoch_domains() {
    const WINDOW: u32 = 0x0001_1501;
    const PREDECESSOR: u64 = 800;
    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    backend.present_crtc_clock_epoch_by_crtc.insert(11, 1);
    backend.present_ust_msc_by_crtc.insert(11, (100, 1));
    let mut predecessor =
        present_pending_entry_with(PREDECESSOR, WINDOW, 0x0040_1502, Some(500), true);
    predecessor.pending.crtc_id = 11;
    predecessor.pending.crtc_epoch = 1;
    state.present_pending_exec.insert(PREDECESSOR, predecessor);

    let mut successor =
        present_pending_entry_with(801, WINDOW, 0x0040_1503, Some(500), true).pending;
    successor.crtc_id = 22;
    successor.crtc_epoch = 1;
    supersede_covered_pending_presents(&mut state, &mut backend, &successor);
    assert!(state.present_pending_exec.contains_key(&PREDECESSOR));
    successor.crtc_id = 11;
    successor.crtc_epoch = 2;
    supersede_covered_pending_presents(&mut state, &mut backend, &successor);
    assert!(state.present_pending_exec.contains_key(&PREDECESSOR));
    successor.crtc_epoch = 1;
    supersede_covered_pending_presents(&mut state, &mut backend, &successor);
    assert!(
        !state.present_pending_exec.contains_key(&PREDECESSOR),
        "the control case scraps only inside the identical raw clock domain"
    );
}

#[test]
fn epoch_mismatch_is_not_rearmed_and_fails_open_unpaced() {
    const PRESENT_ID: u64 = 900;
    let mut state = ServerState::new();
    let mut entry =
        present_pending_entry_with(PRESENT_ID, 0x0001_1601, 0x0040_1602, Some(1_000), true);
    entry.pending.crtc_id = 11;
    entry.pending.crtc_epoch = 1;
    state.present_pending_exec.insert(PRESENT_ID, entry);
    seed_present_domain_clock(&mut state, 11, 1, 10, 1);
    let mut backend = RecordingBackend::new();
    backend.present_crtc_clock_epoch_by_crtc.insert(11, 2);
    backend.present_absolute_vblank_arm_supported = true;

    crate::core_loop::run::arm_present_idle_vblanks(&mut state, &mut backend);
    assert!(backend.armed_shared_wake_crtcs.is_empty());
    assert!(state.present_pending_exec.contains_key(&PRESENT_ID));
    drain_due_present_pending_exec(&mut state, &mut backend);
    assert!(state.present_pending_exec.is_empty());
    assert!(backend.calls().iter().any(|call| matches!(
        call,
        RecordedCall::CopyArea {
            src_host_xid: 0x0040_1602,
            ..
        }
    )));
}

#[test]
fn destroyed_window_generation_suppresses_hidden_direct_events_after_xid_reuse() {
    const WINDOW: u32 = 0x0001_1701;
    const DIRECT_ID: u64 = 901;
    const ABSENT_COPY_ID: u64 = 902;
    const REUSED_COPY_ID: u64 = 903;
    const PURGED_COPY_ID: u64 = 904;
    const ABSENT_FENCE: u32 = 0x0001_17f1;
    const REUSED_FENCE: u32 = 0x0001_17f2;
    const PURGED_FENCE: u32 = 0x0001_17f3;
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    create_present_test_window(&mut state, WINDOW, 0, 0, 50, 50);
    let old_generation = state.present_window_generation(WINDOW);
    state.present_event_selections.insert(
        0x0001_17e1,
        crate::server::PresentEventSelection {
            owner: ClientId(1),
            window: ResourceId(WINDOW),
            event_mask: yserver_protocol::x11::present::EVENT_MASK_COMPLETE_NOTIFY,
        },
    );
    state.present_complete_gate.insert(
        DIRECT_ID,
        crate::server::PresentCompleteGate {
            crtc_id: 0,
            crtc_epoch: 0,
            msc_offset: 0,
            effective_target_msc: 1,
            owner: ClientId(1),
            dst_window_xid: WINDOW,
        },
    );
    let event = crate::backend::CompletedPresentEvent {
        client_id: ClientId(1),
        serial: 1,
        host_xid: 0x0001_1702,
        dst_host_xid: WINDOW,
        options: 0,
        present_id: DIRECT_ID,
        window_generation: old_generation,
        crtc_id: 0,
        crtc_epoch: 0,
        msc_offset: 0,
        completion_clock: None,
        wake: crate::backend::PresentWake::Pixmap { idle_fence_xid: 0 },
        completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_FLIP,
        emit_idle: false,
    };
    let absent_copy = crate::backend::CompletedPresentEvent {
        present_id: ABSENT_COPY_ID,
        wake: crate::backend::PresentWake::Pixmap {
            idle_fence_xid: ABSENT_FENCE,
        },
        completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
        emit_idle: true,
        ..event.clone()
    };
    let reused_copy = crate::backend::CompletedPresentEvent {
        present_id: REUSED_COPY_ID,
        wake: crate::backend::PresentWake::Pixmap {
            idle_fence_xid: REUSED_FENCE,
        },
        completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
        emit_idle: true,
        ..event.clone()
    };
    let purged_copy = crate::backend::CompletedPresentEvent {
        present_id: PURGED_COPY_ID,
        wake: crate::backend::PresentWake::Pixmap {
            idle_fence_xid: PURGED_FENCE,
        },
        completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
        emit_idle: true,
        ..event.clone()
    };
    for fence in [ABSENT_FENCE, REUSED_FENCE, PURGED_FENCE] {
        state.sync_fences.insert(
            fence,
            crate::server::SyncFence {
                owner: ClientId(1),
                triggered: false,
            },
        );
    }
    state
        .present_pending_complete
        .push(crate::server::PendingPresentComplete {
            event: purged_copy,
            effective_target_msc: 1,
            mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
            emit_idle: true,
        });
    let mut backend = RecordingBackend::new();

    destroy_window_subtree(&mut state, &mut backend, None, ResourceId(WINDOW)).expect("destroy");
    assert!(!state.present_window_generations.contains_key(&WINDOW));
    assert!(state.present_complete_gate.is_empty());
    assert!(state.present_event_selections.is_empty());
    assert_eq!(backend.signalled_present_wakes, vec![PURGED_COPY_ID]);
    assert!(state.sync_fences[&PURGED_FENCE].triggered);

    backend.completed_present_events_to_drain.push(absent_copy);
    let _ = read_all_available(&mut peer);
    crate::core_loop::run::run_iteration_tail(&mut state, &mut backend);
    assert_eq!(
        backend.signalled_present_wakes,
        vec![PURGED_COPY_ID, ABSENT_COPY_ID],
        "a stale hidden Copy releases exactly when its GPU completion drains"
    );
    assert!(state.sync_fences[&ABSENT_FENCE].triggered);
    assert!(read_all_available(&mut peer).is_empty());

    create_present_test_window(&mut state, WINDOW, 0, 0, 50, 50);
    let new_generation = state.present_window_generation(WINDOW);
    assert_ne!(new_generation, old_generation);
    state.present_event_selections.insert(
        0x0001_17e2,
        crate::server::PresentEventSelection {
            owner: ClientId(1),
            window: ResourceId(WINDOW),
            event_mask: yserver_protocol::x11::present::EVENT_MASK_COMPLETE_NOTIFY,
        },
    );
    backend
        .completed_present_events_to_drain
        .push(event.clone());
    backend.completed_present_events_to_drain.push(reused_copy);
    let _ = read_all_available(&mut peer);
    crate::core_loop::run::run_iteration_tail(&mut state, &mut backend);
    assert!(read_all_available(&mut peer).is_empty());
    assert_eq!(
        backend.signalled_present_wakes,
        vec![PURGED_COPY_ID, ABSENT_COPY_ID, REUSED_COPY_ID],
        "stale Copy releases, but stale direct Complete does not idle early"
    );
    assert!(state.sync_fences[&REUSED_FENCE].triggered);

    backend.retired_present_idle_events_to_drain.push(event);
    crate::core_loop::run::run_iteration_tail(&mut state, &mut backend);
    assert_eq!(
        backend.signalled_present_wakes,
        vec![PURGED_COPY_ID, ABSENT_COPY_ID, REUSED_COPY_ID, DIRECT_ID]
    );
    assert!(read_all_available(&mut peer).is_empty());
}

#[test]
fn run_loop_groups_all_present_arm_sites_by_crtc_epoch() {
    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    backend.present_crtc_clock_epoch_by_crtc.insert(11, 1);
    backend.present_crtc_clock_epoch_by_crtc.insert(22, 2);
    state.present_pending_msc.extend([
        crate::server::PendingNotifyMsc {
            owner: ClientId(1),
            window: 1,
            crtc_id: 11,
            crtc_epoch: 1,
            msc_offset: 0,
            serial: 1,
            target_msc: 101,
            divisor: 0,
            remainder: 0,
            byte_order: ClientByteOrder::LittleEndian,
        },
        crate::server::PendingNotifyMsc {
            owner: ClientId(1),
            window: 2,
            crtc_id: 11,
            crtc_epoch: 1,
            msc_offset: 0,
            serial: 2,
            target_msc: 102,
            divisor: 0,
            remainder: 0,
            byte_order: ClientByteOrder::LittleEndian,
        },
        crate::server::PendingNotifyMsc {
            owner: ClientId(1),
            window: 3,
            crtc_id: 22,
            crtc_epoch: 2,
            msc_offset: 0,
            serial: 3,
            target_msc: 201,
            divisor: 0,
            remainder: 0,
            byte_order: ClientByteOrder::LittleEndian,
        },
    ]);
    crate::core_loop::run::arm_present_idle_vblanks(&mut state, &mut backend);
    assert_eq!(
        backend.armed_idle_vblank_targets,
        vec![(11, vec![101, 102]), (22, vec![201])]
    );

    state.present_pending_msc.clear();
    let mut c1 = due_pending_complete(10, 1, 111, 0, true);
    c1.event.crtc_id = 11;
    c1.event.crtc_epoch = 1;
    let mut c2 = due_pending_complete(20, 2, 222, 0, true);
    c2.event.crtc_id = 22;
    c2.event.crtc_epoch = 2;
    state.present_pending_complete.extend([c1, c2]);
    crate::core_loop::run::arm_present_idle_vblanks(&mut state, &mut backend);
    assert_eq!(
        backend.armed_completion_idle_vblank_targets,
        vec![(11, vec![111]), (22, vec![222])]
    );

    state.present_pending_complete.clear();
    let mut e1 = present_pending_entry_with(3, 30, 31, Some(110), true);
    e1.pending.crtc_id = 11;
    e1.pending.crtc_epoch = 1;
    let mut e2 = present_pending_entry_with(4, 40, 41, Some(220), true);
    e2.pending.crtc_id = 22;
    e2.pending.crtc_epoch = 2;
    state.present_pending_exec.insert(3, e1);
    state.present_pending_exec.insert(4, e2);
    seed_present_domain_clock(&mut state, 11, 1, 100, 1);
    seed_present_domain_clock(&mut state, 22, 2, 200, 2);
    backend.present_absolute_vblank_arm_supported = true;
    crate::core_loop::run::arm_present_idle_vblanks(&mut state, &mut backend);
    assert_eq!(backend.armed_shared_wake_crtcs, vec![11, 22]);
}

#[test]
fn headless_notify_msc_completes_without_parking() {
    let mut state = ServerState::new();
    state.randr = crate::randr::RandrState::from_outputs(1, Vec::new());
    let mut peer = install_client(&mut state, 1);
    state.present_event_selections.insert(
        1,
        crate::server::PresentEventSelection {
            owner: ClientId(1),
            window: ROOT_WINDOW,
            event_mask: yserver_protocol::x11::present::EVENT_MASK_COMPLETE_NOTIFY,
        },
    );
    let mut body = vec![0u8; 36];
    body[0..4].copy_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    body[4..8].copy_from_slice(&1u32.to_le_bytes());
    body[12..20].copy_from_slice(&500u64.to_le_bytes());
    let mut backend = RecordingBackend::new();
    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 145,
            data: yserver_protocol::x11::present::NOTIFY_MSC,
            length_units: 10,
        },
        &body,
        None,
    )
    .unwrap();
    assert!(state.present_pending_msc.is_empty());
    assert_eq!(state.present_window_msc[&ROOT_WINDOW.0].last_crtc, 0);
    let mut event = [0u8; 40];
    peer.read_exact(&mut event).unwrap();
    assert_eq!(u64::from_le_bytes(event[32..40].try_into().unwrap()), 0);
}

#[test]
fn present_notify_msc_parks_then_fires_on_vblank_advance() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    const WINDOW: u32 = 0x100;
    const EID: u32 = 0x0100_0001;
    const SERIAL: u32 = 0x0100_0002;
    const COMPLETE_NOTIFY_MASK: u32 = 0x2;

    let mut select_body = Vec::new();
    select_body.extend_from_slice(&EID.to_le_bytes());
    select_body.extend_from_slice(&WINDOW.to_le_bytes());
    select_body.extend_from_slice(&COMPLETE_NOTIFY_MASK.to_le_bytes());
    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 145,
            data: yserver_protocol::x11::present::SELECT_INPUT,
            length_units: 4,
        },
        &select_body,
        None,
    )
    .expect("Present SelectInput");

    let mut notify_body = Vec::new();
    notify_body.extend_from_slice(&WINDOW.to_le_bytes());
    notify_body.extend_from_slice(&SERIAL.to_le_bytes());
    notify_body.extend_from_slice(&0_u32.to_le_bytes()); // pad
    notify_body.extend_from_slice(&0_u64.to_le_bytes()); // target_msc
    notify_body.extend_from_slice(&0_u64.to_le_bytes()); // divisor
    notify_body.extend_from_slice(&0_u64.to_le_bytes()); // remainder
    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(2),
        RequestHeader {
            opcode: 145,
            data: yserver_protocol::x11::present::NOTIFY_MSC,
            length_units: 10,
        },
        &notify_body,
        None,
    )
    .expect("Present NotifyMSC");

    // Vblank-paced clock: with no pageflip yet (present_kernel_msc == 0)
    // the request is PARKED rather than completed immediately — emitting
    // ust=0 would be rejected by a `present`-scheduler compositor (picom).
    assert_eq!(
        state.present_pending_msc.len(),
        1,
        "NotifyMSC parks until a real (msc, ust) is available"
    );

    // Simulate a pageflip / armed-vblank advancing the kernel clock. This
    // is what `drain_present_completions` does after the backend reports a
    // retirement; it drains every parked request whose target is satisfied.
    const FIRED_MSC: u64 = 100;
    const FIRED_UST: u64 = 0x1234_5678;
    fire_due_present_notify_msc(&mut state, FIRED_MSC, FIRED_UST);
    assert!(
        state.present_pending_msc.is_empty(),
        "satisfied parked request is removed after firing"
    );

    let mut event = [0u8; 40];
    peer.read_exact(&mut event).expect("CompleteNotify event");
    assert_eq!(event[0], 35, "GenericEvent");
    assert_eq!(event[1], 145, "Present extension major opcode");
    assert_eq!(
        u16::from_le_bytes([event[8], event[9]]),
        u16::from(yserver_protocol::x11::present::EVENT_COMPLETE_NOTIFY)
    );
    assert_eq!(
        event[10],
        yserver_protocol::x11::present::COMPLETE_KIND_NOTIFY_MSC
    );
    assert_eq!(
        u32::from_le_bytes([event[12], event[13], event[14], event[15]]),
        EID
    );
    assert_eq!(
        u32::from_le_bytes([event[16], event[17], event[18], event[19]]),
        WINDOW
    );
    assert_eq!(
        u32::from_le_bytes([event[20], event[21], event[22], event[23]]),
        SERIAL
    );
    // UST (offset 24) and MSC (offset 32) carry the real kernel values.
    assert_eq!(
        u64::from_le_bytes([
            event[24], event[25], event[26], event[27], event[28], event[29], event[30], event[31],
        ]),
        FIRED_UST,
        "CompleteNotify reports the real UST from the vblank advance"
    );
    assert_eq!(
        u64::from_le_bytes([
            event[32], event[33], event[34], event[35], event[36], event[37], event[38], event[39],
        ]),
        FIRED_MSC,
        "CompleteNotify reports the real MSC from the vblank advance"
    );
}

#[test]
fn notify_msc_resource_precedence_and_card32_remainder_error_value() {
    const UNKNOWN_WINDOW: u32 = 0x00ff_aa01;
    const LARGE_REMAINDER: u64 = 0x1_0000_0001;
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();

    let mut body = vec![0u8; 36];
    body[0..4].copy_from_slice(&UNKNOWN_WINDOW.to_le_bytes());
    body[28..36].copy_from_slice(&LARGE_REMAINDER.to_le_bytes());
    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 145,
            data: yserver_protocol::x11::present::NOTIFY_MSC,
            length_units: 10,
        },
        &body,
        None,
    )
    .unwrap();
    let mut error = [0u8; 32];
    peer.read_exact(&mut error).unwrap();
    assert_eq!(error[1], x11::error::BAD_WINDOW);
    assert_eq!(
        u32::from_le_bytes(error[4..8].try_into().unwrap()),
        UNKNOWN_WINDOW
    );

    body[0..4].copy_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(2),
        RequestHeader {
            opcode: 145,
            data: yserver_protocol::x11::present::NOTIFY_MSC,
            length_units: 10,
        },
        &body,
        None,
    )
    .unwrap();
    peer.read_exact(&mut error).unwrap();
    assert_eq!(error[1], x11::error::BAD_VALUE);
    assert_eq!(
        u32::from_le_bytes(error[4..8].try_into().unwrap()),
        1,
        "Xorg stores remainder in CARD32 errorValue (low-32 truncation)"
    );
    assert!(!state.present_window_msc.contains_key(&ROOT_WINDOW.0));
}

#[test]
fn present_notify_msc_remainder_ge_divisor_is_rejected_not_parked() {
    // Xorg rejects remainder >= divisor with BadValue. Pre-fix, such a
    // request parked forever: current_msc % divisor is always < divisor, so
    // it could never equal a remainder >= divisor, growing the parked list
    // unbounded and re-scanning it every vblank.
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    const WINDOW: u32 = 0x100;

    let mut notify_body = Vec::new();
    notify_body.extend_from_slice(&WINDOW.to_le_bytes());
    notify_body.extend_from_slice(&1_u32.to_le_bytes()); // serial
    notify_body.extend_from_slice(&0_u32.to_le_bytes()); // pad
    notify_body.extend_from_slice(&0_u64.to_le_bytes()); // target_msc
    notify_body.extend_from_slice(&2_u64.to_le_bytes()); // divisor
    notify_body.extend_from_slice(&5_u64.to_le_bytes()); // remainder >= divisor
    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 145,
            data: yserver_protocol::x11::present::NOTIFY_MSC,
            length_units: 10,
        },
        &notify_body,
        None,
    )
    .expect("Present NotifyMSC (invalid divisor/remainder)");

    assert!(
        state.present_pending_msc.is_empty(),
        "remainder >= divisor must be rejected, not parked forever"
    );
}

#[test]
fn present_pending_msc_purged_on_client_disconnect() {
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    const WINDOW: u32 = 0x100;

    // Park a NotifyMSC (present_kernel_msc == 0 → always parks).
    let mut notify_body = Vec::new();
    notify_body.extend_from_slice(&WINDOW.to_le_bytes());
    notify_body.extend_from_slice(&1_u32.to_le_bytes()); // serial
    notify_body.extend_from_slice(&0_u32.to_le_bytes()); // pad
    notify_body.extend_from_slice(&1_u64.to_le_bytes()); // target_msc
    notify_body.extend_from_slice(&0_u64.to_le_bytes()); // divisor
    notify_body.extend_from_slice(&0_u64.to_le_bytes()); // remainder
    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 145,
            data: yserver_protocol::x11::present::NOTIFY_MSC,
            length_units: 10,
        },
        &notify_body,
        None,
    )
    .expect("Present NotifyMSC");
    assert_eq!(state.present_pending_msc.len(), 1, "request parked");

    // Client disconnects → its parked requests must be purged, else they
    // are re-scanned every vblank forever with no client to satisfy them.
    crate::core_loop::process_disconnect::process_disconnect(&mut state, &mut backend, ClientId(1));
    assert!(
        state.present_pending_msc.is_empty(),
        "parked NotifyMSC purged when its owning client disconnects"
    );
}

#[test]
fn deferred_present_pixmap_copies_only_after_source_readiness() {
    use crate::server::{PendingPresentEntry, PendingPresentPixmap, PendingPresentRequest};
    use yserver_protocol::x11::present::PixmapRequest;

    const WAIT_ID: u64 = 7;
    const PRESENT_ID: u64 = 42;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    let pending = PendingPresentPixmap {
        origin: None,
        client_id: ClientId(1),
        request: PendingPresentRequest::Pixmap(PixmapRequest {
            window: 0x101,
            pixmap: 0x102,
            serial: 3,
            valid: 0,
            update: 0,
            x_off: 4,
            y_off: 5,
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
        src_host_xid: 0x400102,
        paint_dst_host_xid: 0x400101,
        completion_dst_host_xid: 0x400101,
        src_width: 800,
        src_height: 600,
        update_rects: None,
        present_id: PRESENT_ID,
        window_generation: 0,
        crtc_id: 0,
        crtc_epoch: 0,
        msc_offset: 0,
        effective_target_msc: None,
    };
    state.present_wait_to_id.insert(WAIT_ID, PRESENT_ID);
    state.present_pending_exec.insert(
        PRESENT_ID,
        PendingPresentEntry {
            pending,
            source_ready: false,
            superseded_by_executed_present_id: None,
            wait_id: Some(WAIT_ID),
            pin: Some(999),
        },
    );

    drain_ready_present_pixmaps(&mut state, &mut backend);
    assert!(
        backend
            .calls()
            .iter()
            .all(|call| !matches!(call, RecordedCall::CopyArea { .. }))
    );

    backend.ready_present_source_waits.push(WAIT_ID);
    drain_ready_present_pixmaps(&mut state, &mut backend);
    assert!(backend.calls().iter().any(|call| matches!(
        call,
        RecordedCall::CopyArea {
            src_host_xid: 0x400102,
            dst_host_xid: 0x400101,
            dst_x: 4,
            dst_y: 5,
            width: 800,
            height: 600,
            ..
        }
    )));
    // The WAIT pin (finish_present_source_wait) and the ENTRY pin
    // (release_present_source) are distinct releases — both fire
    // exactly once for this one parked entry.
    assert_eq!(backend.finished_present_source_waits, vec![WAIT_ID]);
    assert_eq!(backend.released_present_sources, vec![999]);
    assert!(state.present_pending_exec.is_empty());
    assert!(state.present_wait_to_id.is_empty());
}

#[test]
fn destroyed_window_purges_parked_present_wait_before_producer_ready() {
    // Task 8 Step 1b (extended for Task 5's unified store): a
    // PresentPixmap still parked on its async source wait when its
    // destination window is destroyed must be purged so it can never
    // execute against a dead drawable. The idle fence is released
    // by-xid (client still alive), the WAIT pin
    // (`finish_present_source_wait`) and the distinct ENTRY pin
    // (`release_present_source`) each drop exactly once, the side map
    // row is cleaned up, and a later producer-ready drain then creates
    // NO copy and NO gate.
    use crate::server::{
        PendingPresentEntry, PendingPresentPixmap, PendingPresentRequest, SyncFence,
    };
    use yserver_protocol::x11::{CreateWindowRequest, present::PixmapRequest};

    const CLIENT: u32 = 1;
    const WINDOW_XID: u32 = 0x0000_0101;
    const IDLE_FENCE: u32 = 0x0000_0555;
    const WAIT_ID: u64 = 7;
    const PRESENT_ID: u64 = 4242;
    const PIN_ID: u64 = 8181;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, CLIENT);
    let mut backend = RecordingBackend::new();

    state.resources.create_window(
        ClientId(CLIENT),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(WINDOW_XID),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 800,
            height: 600,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    state.sync_fences.insert(
        IDLE_FENCE,
        SyncFence {
            owner: ClientId(CLIENT),
            triggered: false,
        },
    );

    let pending = PendingPresentPixmap {
        origin: None,
        client_id: ClientId(CLIENT),
        request: PendingPresentRequest::Pixmap(PixmapRequest {
            window: WINDOW_XID,
            pixmap: 0x102,
            serial: 3,
            valid: 0,
            update: 0,
            x_off: 4,
            y_off: 5,
            target_crtc: 0,
            wait_fence: 0,
            idle_fence: IDLE_FENCE,
            options: 0,
            target_msc: 0,
            divisor: 0,
            remainder: 0,
            notifies: Vec::new(),
        }),
        wake: crate::backend::PresentWake::Pixmap {
            idle_fence_xid: IDLE_FENCE,
        },
        masked_options: 0,
        src_host_xid: 0x400102,
        paint_dst_host_xid: 0x400101,
        completion_dst_host_xid: 0x400101,
        src_width: 800,
        src_height: 600,
        update_rects: None,
        present_id: PRESENT_ID,
        window_generation: 0,
        crtc_id: 0,
        crtc_epoch: 0,
        msc_offset: 0,
        effective_target_msc: None,
    };
    state.present_wait_to_id.insert(WAIT_ID, PRESENT_ID);
    state.present_pending_exec.insert(
        PRESENT_ID,
        PendingPresentEntry {
            pending,
            source_ready: false,
            superseded_by_executed_present_id: None,
            wait_id: Some(WAIT_ID),
            pin: Some(PIN_ID),
        },
    );

    destroy_window_subtree(&mut state, &mut backend, None, ResourceId(WINDOW_XID))
        .expect("destroy");

    // (a) the parked entry and its side-map row are removed,
    assert!(
        state.present_pending_exec.is_empty(),
        "parked PresentPixmap purged when its destination window is destroyed"
    );
    assert!(
        state.present_wait_to_id.is_empty(),
        "wait_id -> present_id side map row dropped alongside the entry"
    );
    // (b) its idle fence was triggered by-xid (client alive), and both
    // the WAIT pin and the distinct ENTRY pin dropped exactly once,
    assert_eq!(
        backend.triggered_dri3_fences,
        vec![IDLE_FENCE],
        "idle fence released exactly once on window-destroy teardown"
    );
    assert!(
        state.sync_fences[&IDLE_FENCE].triggered,
        "QueryFence mirror must agree with the released idle fence"
    );
    assert_eq!(
        backend.finished_present_source_waits,
        vec![WAIT_ID],
        "wait pin dropped exactly once on window-destroy teardown"
    );
    assert_eq!(
        backend.released_present_sources,
        vec![PIN_ID],
        "entry pin dropped exactly once on window-destroy teardown"
    );

    // (c) a later producer-ready drain creates no orphan copy + no gate.
    let calls_before = backend.calls().len();
    backend.ready_present_source_waits.push(WAIT_ID);
    drain_ready_present_pixmaps(&mut state, &mut backend);
    assert!(
        backend.calls()[calls_before..]
            .iter()
            .all(|call| !matches!(call, RecordedCall::CopyArea { .. })),
        "no copy is issued for a present whose window was destroyed"
    );
    assert!(
        state.present_complete_gate.is_empty(),
        "no completion gate is inserted for a purged present"
    );
    // The later drain reports an unknown wait_id (the entry was
    // already purged) — must not double-release either pin.
    assert_eq!(
        backend.finished_present_source_waits,
        vec![WAIT_ID, WAIT_ID],
        "backend's own unknown-wait_id report is a no-op guard at the caller, \
             but finish_present_source_wait is still invoked once per drain call"
    );
    assert_eq!(
        backend.released_present_sources,
        vec![PIN_ID],
        "entry pin must not be released a second time by the later drain"
    );
}

fn due_pending_complete(
    window: u32,
    present_id: u64,
    effective_target_msc: u64,
    mode: u8,
    emit_idle: bool,
) -> crate::server::PendingPresentComplete {
    use crate::backend::{CompletedPresentEvent, PresentWake};

    crate::server::PendingPresentComplete {
        event: CompletedPresentEvent {
            client_id: ClientId(1),
            serial: 1,
            host_xid: window,
            dst_host_xid: window,
            options: 0,
            present_id,
            window_generation: 0,
            crtc_id: 0,
            crtc_epoch: 0,
            msc_offset: 0,
            completion_clock: None,
            wake: PresentWake::Pixmap { idle_fence_xid: 0 },
            completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
            emit_idle: true,
        },
        effective_target_msc,
        mode,
        emit_idle,
    }
}

/// Round-3 inversion vector (spec §"Ordered completion delivery
/// (per-window `present_id` order)", blocked-check state 2:
/// executed-but-undrained). P1 executed already (its GPU copy is
/// done) but its completion event has not drained yet — it still sits
/// in `present_complete_gate`. P3 (not modelled — Task 8 doesn't
/// exist on this branch yet) scrapped P2 and parked `Skip(P2)`
/// directly into `present_pending_complete`, already due. Pre-fix,
/// `fire_due_present_completions` swept the queue in raw order and
/// had no notion of the gate at all, so it would deliver the due
/// Skip(P2) immediately — a backward serial, since P1 < P2 for the
/// same window. The per-window hold-back must block Skip(P2) until
/// P1 resolves, then deliver Copy(P1) before Skip(P2).
#[test]
fn due_skip_is_held_back_behind_a_smaller_id_undrained_gate_entry() {
    use crate::server::PresentCompleteGate;
    use yserver_protocol::x11::present as x11present;

    const WINDOW_XID: u32 = 0x0000_0101;
    const PRESENT_EID: u32 = 0x0010_0011;
    const P1: u64 = 10;
    const P2: u64 = 11;
    const TARGET_MSC: u64 = 100;
    let clock = crate::backend::PresentClockSample {
        msc: TARGET_MSC,
        ust: 0x1000,
        source: crate::backend::PresentClockSource::PageFlip,
    };

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    // COMPLETE_NOTIFY_MASK only, so `complete_notify_modes` doesn't
    // have to skate around IdleNotify sizes/interleaving.
    state.present_event_selections.insert(
        PRESENT_EID,
        crate::server::PresentEventSelection {
            owner: ClientId(1),
            window: ResourceId(WINDOW_XID),
            event_mask: x11present::EVENT_MASK_COMPLETE_NOTIFY,
        },
    );
    let _ = read_all_available(&mut peer);
    let mut backend = RecordingBackend::new();

    state.present_complete_gate.insert(
        P1,
        PresentCompleteGate {
            crtc_id: 0,
            crtc_epoch: 0,
            msc_offset: 0,
            effective_target_msc: TARGET_MSC,
            owner: ClientId(1),
            dst_window_xid: WINDOW_XID,
        },
    );
    state.present_pending_complete.push(due_pending_complete(
        WINDOW_XID,
        P2,
        TARGET_MSC,
        x11present::COMPLETE_MODE_SKIP,
        false,
    ));

    fire_due_present_completions(&mut state, &mut backend, clock);
    assert!(
        backend.signalled_present_wakes.is_empty(),
        "Skip(P2) must not deliver while P1 (smaller present_id) is \
             still undrained in present_complete_gate — pre-fix this fires"
    );
    assert_eq!(
        state.present_pending_complete.len(),
        1,
        "Skip(P2) stays parked, held back"
    );
    assert!(
        read_all_available(&mut peer).is_empty(),
        "no CompleteNotify reaches the client while held back"
    );

    // P1 resolves (its fence retires late): the gate empties and its
    // own completion joins the queue, due, same as Skip(P2).
    state.present_complete_gate.remove(&P1);
    state.present_pending_complete.push(due_pending_complete(
        WINDOW_XID,
        P1,
        TARGET_MSC,
        x11present::COMPLETE_MODE_COPY,
        true,
    ));

    fire_due_present_completions(&mut state, &mut backend, clock);
    assert!(
        state.present_pending_complete.is_empty(),
        "both entries deliver once P1 is no longer blocking"
    );
    assert_eq!(
        backend.signalled_present_wakes,
        vec![P1],
        "Copy(P1) signals its wake; Skip(P2)'s emit_idle=false gates \
             signal_present_wake off (fix 3 — its wake was already \
             released at scrap)"
    );
    // Order proof independent of the (now Skip-gated) wake log: the
    // wire itself must read Copy(P1) then Skip(P2).
    assert_eq!(
        complete_notify_modes(&read_all_available(&mut peer)),
        vec![
            x11present::COMPLETE_MODE_COPY,
            x11present::COMPLETE_MODE_SKIP
        ],
        "Copy(P1) delivers before Skip(P2): per-window present_id order"
    );
}

/// Blocked-check state 1: msc-parked-unexecuted (`present_pending_exec`).
/// Entry A (smaller present_id) is still parked, unexecuted, in the
/// store for window W; Skip(B) (larger present_id) is already due in
/// the queue. B must be held back until A leaves the store, then both
/// deliver in id order.
#[test]
fn due_skip_is_held_back_behind_a_smaller_id_unexecuted_store_entry() {
    use yserver_protocol::x11::present as x11present;

    const WINDOW_XID: u32 = 0x0000_0202;
    const PRESENT_EID: u32 = 0x0010_0022;
    const A: u64 = 20;
    const B: u64 = 21;
    const TARGET_MSC: u64 = 50;
    let clock = crate::backend::PresentClockSample {
        msc: TARGET_MSC,
        ust: 0x2000,
        source: crate::backend::PresentClockSource::PageFlip,
    };

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    state.present_event_selections.insert(
        PRESENT_EID,
        crate::server::PresentEventSelection {
            owner: ClientId(1),
            window: ResourceId(WINDOW_XID),
            event_mask: x11present::EVENT_MASK_COMPLETE_NOTIFY,
        },
    );
    let _ = read_all_available(&mut peer);
    let mut backend = RecordingBackend::new();

    state
        .present_pending_exec
        .insert(A, stub_pending_present_entry(WINDOW_XID, A));
    state.present_pending_complete.push(due_pending_complete(
        WINDOW_XID,
        B,
        TARGET_MSC,
        x11present::COMPLETE_MODE_SKIP,
        false,
    ));

    fire_due_present_completions(&mut state, &mut backend, clock);
    assert!(
        backend.signalled_present_wakes.is_empty(),
        "Skip(B) held back while A (smaller present_id) is still \
             unexecuted in present_pending_exec"
    );

    // A executes and completes.
    state.present_pending_exec.remove(&A);
    state.present_pending_complete.push(due_pending_complete(
        WINDOW_XID,
        A,
        TARGET_MSC,
        x11present::COMPLETE_MODE_COPY,
        true,
    ));

    fire_due_present_completions(&mut state, &mut backend, clock);
    assert_eq!(
        backend.signalled_present_wakes,
        vec![A],
        "Copy(A) signals its wake; Skip(B)'s emit_idle=false gates it off"
    );
    assert_eq!(
        complete_notify_modes(&read_all_available(&mut peer)),
        vec![
            x11present::COMPLETE_MODE_COPY,
            x11present::COMPLETE_MODE_SKIP
        ],
        "both deliver in arrival (present_id) order once A clears the store"
    );
}

/// Per-window isolation: window X's stalled completion must not delay
/// window Y's due completion in the same sweep.
#[test]
fn per_window_hold_back_does_not_cross_windows() {
    use crate::server::PresentCompleteGate;
    use yserver_protocol::x11::present as x11present;

    const WINDOW_X: u32 = 0x0000_0303;
    const WINDOW_Y: u32 = 0x0000_0404;
    const X_SMALL: u64 = 30;
    const X_SKIP: u64 = 31;
    const Y_ID: u64 = 32;
    const TARGET_MSC: u64 = 70;
    let clock = crate::backend::PresentClockSample {
        msc: TARGET_MSC,
        ust: 0x3000,
        source: crate::backend::PresentClockSource::PageFlip,
    };

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();

    state.present_complete_gate.insert(
        X_SMALL,
        PresentCompleteGate {
            crtc_id: 0,
            crtc_epoch: 0,
            msc_offset: 0,
            effective_target_msc: TARGET_MSC,
            owner: ClientId(1),
            dst_window_xid: WINDOW_X,
        },
    );
    state.present_pending_complete.push(due_pending_complete(
        WINDOW_X,
        X_SKIP,
        TARGET_MSC,
        x11present::COMPLETE_MODE_SKIP,
        false,
    ));
    state.present_pending_complete.push(due_pending_complete(
        WINDOW_Y,
        Y_ID,
        TARGET_MSC,
        x11present::COMPLETE_MODE_COPY,
        true,
    ));

    fire_due_present_completions(&mut state, &mut backend, clock);
    assert_eq!(
        backend.signalled_present_wakes,
        vec![Y_ID],
        "window Y's due completion delivers even though window X is stalled"
    );
    assert_eq!(
        state.present_pending_complete.len(),
        1,
        "window X's Skip stays held back"
    );
    assert_eq!(state.present_pending_complete[0].event.present_id, X_SKIP);
}

/// Blocked-check state 3: parked-not-yet-due, i.e. an earlier entry
/// in the very same queue group. id=5 (smaller, Copy) has a FUTURE
/// target; id=7 (larger, Skip) is due right now. Even though id=7 is
/// individually due, it must not be delivered ahead of id=5 — the
/// per-window walk has to stop (not skip past) the first blocked
/// entry it meets. Reworded as a mutation check: a `break`→`continue`
/// slip in the sweep's inner loop would let id=7 slide through on the
/// first pass while id=5 stays parked — this test's first-pass
/// assertion (nothing delivered) catches exactly that; verified by
/// hand-applying the mutation locally (see report).
#[test]
fn future_smaller_id_blocks_due_larger_id_in_same_queue_group() {
    use yserver_protocol::x11::present as x11present;

    const WINDOW_XID: u32 = 0x0000_0707;
    const PRESENT_EID: u32 = 0x0010_0077;
    const SMALLER_FUTURE: u64 = 5;
    const LARGER_DUE: u64 = 7;
    const FUTURE_MSC: u64 = 1_000;
    const DUE_MSC: u64 = 10;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    state.present_event_selections.insert(
        PRESENT_EID,
        crate::server::PresentEventSelection {
            owner: ClientId(1),
            window: ResourceId(WINDOW_XID),
            event_mask: x11present::EVENT_MASK_COMPLETE_NOTIFY,
        },
    );
    let _ = read_all_available(&mut peer);
    let mut backend = RecordingBackend::new();

    state.present_pending_complete.push(due_pending_complete(
        WINDOW_XID,
        SMALLER_FUTURE,
        FUTURE_MSC,
        x11present::COMPLETE_MODE_COPY,
        true,
    ));
    state.present_pending_complete.push(due_pending_complete(
        WINDOW_XID,
        LARGER_DUE,
        DUE_MSC,
        x11present::COMPLETE_MODE_SKIP,
        false,
    ));

    // First sweep: clock is past id=7's target but nowhere near
    // id=5's. Nothing for this window may deliver.
    let clock_mid = crate::backend::PresentClockSample {
        msc: 50,
        ust: 0x7000,
        source: crate::backend::PresentClockSource::PageFlip,
    };
    fire_due_present_completions(&mut state, &mut backend, clock_mid);
    assert!(
        backend.signalled_present_wakes.is_empty(),
        "neither entry delivers: id=7 is due but held behind id=5's \
             still-future target (state 3 — parked-not-yet-due)"
    );
    assert_eq!(
        state.present_pending_complete.len(),
        2,
        "both entries retained"
    );
    assert!(
        read_all_available(&mut peer).is_empty(),
        "no CompleteNotify reaches the client on the blocked first sweep"
    );

    // Second sweep: clock passes both targets — both deliver, in order.
    let clock_late = crate::backend::PresentClockSample {
        msc: FUTURE_MSC + 1,
        ust: 0x7001,
        source: crate::backend::PresentClockSource::PageFlip,
    };
    fire_due_present_completions(&mut state, &mut backend, clock_late);
    assert!(state.present_pending_complete.is_empty());
    assert_eq!(
        complete_notify_modes(&read_all_available(&mut peer)),
        vec![
            x11present::COMPLETE_MODE_COPY,
            x11present::COMPLETE_MODE_SKIP
        ],
        "Copy(5) then Skip(7), once both are due"
    );
}

/// A parked Skip (supersession scrap) must emit no second IdleNotify
/// and touch no fence mirror at delivery — both were already handled
/// at scrap time (spec §"Ordered completion delivery" item 1). Only
/// the CompleteNotify goes out, with mode byte `COMPLETE_MODE_SKIP`.
#[test]
fn parked_skip_delivery_emits_only_complete_notify_mode_skip() {
    use crate::{backend::CompletedPresentEvent, server::SyncFence};
    use yserver_protocol::x11::present as x11present;

    const WINDOW_XID: u32 = 0x0000_0505;
    const PRESENT_EID: u32 = 0x0010_0099;
    const IDLE_FENCE: u32 = 0x0000_0777;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    state.present_event_selections.insert(
        PRESENT_EID,
        crate::server::PresentEventSelection {
            owner: ClientId(1),
            window: ResourceId(WINDOW_XID),
            event_mask: x11present::EVENT_MASK_COMPLETE_NOTIFY | x11present::EVENT_MASK_IDLE_NOTIFY,
        },
    );
    // A live fence the scrap path already triggered; delivery must
    // not touch it (it's already `true`, but the *write* itself must
    // not happen — a real regression would re-trigger a fence a
    // client has since reused for a fresh present, per the spec).
    state.sync_fences.insert(
        IDLE_FENCE,
        SyncFence {
            owner: ClientId(1),
            triggered: true,
        },
    );
    let _ = read_all_available(&mut peer);

    let mut backend = RecordingBackend::new();
    let event = CompletedPresentEvent {
        client_id: ClientId(1),
        serial: 5,
        host_xid: WINDOW_XID,
        dst_host_xid: WINDOW_XID,
        options: 0,
        present_id: 99,
        window_generation: 0,
        crtc_id: 0,
        crtc_epoch: 0,
        msc_offset: 0,
        completion_clock: None,
        wake: crate::backend::PresentWake::Pixmap {
            idle_fence_xid: IDLE_FENCE,
        },
        completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
        emit_idle: true,
    };
    let clock = crate::backend::PresentClockSample {
        msc: 42,
        ust: 0x9999,
        source: crate::backend::PresentClockSource::PageFlip,
    };
    complete_present_with_clock(
        &mut state,
        &mut backend,
        &event,
        clock,
        x11present::COMPLETE_MODE_SKIP,
        false,
    );

    let bytes = read_all_available(&mut peer);
    assert_eq!(
        bytes.len(),
        40,
        "exactly one event (CompleteNotify only, no IdleNotify): got {} bytes",
        bytes.len()
    );
    assert_eq!(bytes[0], 35, "GenericEvent");
    assert_eq!(bytes[1], 145, "PRESENT major opcode");
    assert_eq!(
        u16::from_le_bytes(bytes[8..10].try_into().unwrap()),
        u16::from(x11present::EVENT_COMPLETE_NOTIFY),
    );
    assert_eq!(bytes[11], x11present::COMPLETE_MODE_SKIP);
    assert!(
        state.sync_fences[&IDLE_FENCE].triggered,
        "fence mirror must still read triggered=true (scrap set it) — \
             this only proves delivery didn't touch it; a fresh false->false \
             would look identical, so this test also relies on \
             `complete_present_with_clock`'s emit_idle=false code path \
             skipping the write entirely (see source)"
    );
}

#[test]
fn direct_flip_completion_and_buffer_idle_are_separate_retirements() {
    use crate::{backend::CompletedPresentEvent, server::SyncFence};
    use yserver_protocol::x11::present as x11present;

    const WINDOW_XID: u32 = 0x0000_0606;
    const PIXMAP_XID: u32 = 0x0000_0607;
    const PRESENT_EID: u32 = 0x0010_00aa;
    const IDLE_FENCE: u32 = 0x0000_0888;
    const PRESENT_ID: u64 = 123;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    state.present_event_selections.insert(
        PRESENT_EID,
        crate::server::PresentEventSelection {
            owner: ClientId(1),
            window: ResourceId(WINDOW_XID),
            event_mask: x11present::EVENT_MASK_COMPLETE_NOTIFY | x11present::EVENT_MASK_IDLE_NOTIFY,
        },
    );
    state.sync_fences.insert(
        IDLE_FENCE,
        SyncFence {
            owner: ClientId(1),
            triggered: false,
        },
    );
    let _ = read_all_available(&mut peer);

    let event = CompletedPresentEvent {
        client_id: ClientId(1),
        serial: 7,
        host_xid: PIXMAP_XID,
        dst_host_xid: WINDOW_XID,
        options: 0,
        present_id: PRESENT_ID,
        window_generation: 0,
        crtc_id: 0,
        crtc_epoch: 0,
        msc_offset: 0,
        completion_clock: None,
        wake: crate::backend::PresentWake::Pixmap {
            idle_fence_xid: IDLE_FENCE,
        },
        completion_mode: x11present::COMPLETE_MODE_FLIP,
        emit_idle: false,
    };
    let clock = crate::backend::PresentClockSample {
        msc: 55,
        ust: 66,
        source: crate::backend::PresentClockSource::PageFlip,
    };
    let mut backend = RecordingBackend::new();

    complete_present_with_clock(
        &mut state,
        &mut backend,
        &event,
        clock,
        event.completion_mode,
        event.emit_idle,
    );
    let complete = read_all_available(&mut peer);
    assert_eq!(complete.len(), 40, "flip retirement emits one event");
    assert_eq!(
        u16::from_le_bytes(complete[8..10].try_into().unwrap()),
        u16::from(x11present::EVENT_COMPLETE_NOTIFY)
    );
    assert_eq!(complete[11], x11present::COMPLETE_MODE_FLIP);
    assert!(!state.sync_fences[&IDLE_FENCE].triggered);
    assert!(backend.signalled_present_wakes.is_empty());

    retire_present_idle(&mut state, &mut backend, &event);
    let idle = read_all_available(&mut peer);
    assert_eq!(idle.len(), 32, "replacement retirement emits one event");
    assert_eq!(
        u16::from_le_bytes(idle[8..10].try_into().unwrap()),
        u16::from(x11present::EVENT_IDLE_NOTIFY)
    );
    assert!(state.sync_fences[&IDLE_FENCE].triggered);
    assert_eq!(backend.signalled_present_wakes, vec![PRESENT_ID]);
}

#[test]
fn free_pixmap_between_park_and_drain_still_executes_the_pinned_copy() {
    // Task 5 TDD (iii): `FreePixmap` on the *source* pixmap is legal
    // immediately after `PresentPixmap` — Xorg refs the vblank's
    // pixmap for exactly this reason. The entry pin exists so a copy
    // still parked on its producer-fence wait at that point keeps
    // targeting the pinned drawable rather than a dead xid. Drive the
    // real Deferred arm through `process_request` (not a manually
    // constructed entry) so the pin comes from the production code
    // path, `FreePixmap` the source in between, then verify the drain
    // still issues the copy and releases the entry pin exactly once.
    use yserver_protocol::x11::{CreatePixmapRequest, CreateWindowRequest};

    const CLIENT: u32 = 21;
    const WINDOW_XID: u32 = 0x00e0_2103;
    const PIXMAP_XID: u32 = 0x00e0_2104;
    const WINDOW_HOST_XID: u32 = 0x0040_2103;
    const PIXMAP_HOST_XID: u32 = 0x0040_2104;
    const WAIT_ID: u64 = 33;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, CLIENT);
    let mut backend = RecordingBackend::new();
    backend.present_source_wait = crate::backend::PresentSourceWait::Deferred(WAIT_ID);

    state.resources.create_window(
        ClientId(CLIENT),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(WINDOW_XID),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 800,
            height: 600,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(WINDOW_XID));
    if let Some(w) = state.resources.window_mut(ResourceId(WINDOW_XID)) {
        w.host_xid = crate::backend::WindowHandle::from_raw(WINDOW_HOST_XID);
    }
    state.resources.create_pixmap(
        ClientId(CLIENT),
        CreatePixmapRequest {
            depth: 24,
            pixmap: ResourceId(PIXMAP_XID),
            drawable: ResourceId(WINDOW_XID),
            width: 800,
            height: 600,
        },
    );
    let _ = state.resources.set_pixmap_host_xid(
        ResourceId(PIXMAP_XID),
        crate::backend::PixmapHandle::from_raw(PIXMAP_HOST_XID).expect("valid host pixmap"),
    );

    // PresentPixmap (opcode 1) fixed prefix, mirroring the 68-byte
    // body other tests in this module build: window pixmap serial
    // valid update x_off y_off ... (only window/pixmap set here — a
    // full-pixmap copy, no update region).
    let mut body = vec![0u8; 68];
    body[0..4].copy_from_slice(&WINDOW_XID.to_le_bytes());
    body[4..8].copy_from_slice(&PIXMAP_XID.to_le_bytes());

    process_request(
        &mut state,
        &mut backend,
        ClientId(CLIENT),
        SequenceNumber(1),
        RequestHeader {
            opcode: 145,
            data: yserver_protocol::x11::present::PIXMAP,
            length_units: u32::try_from(1 + body.len() / 4).unwrap(),
        },
        &body,
        None,
    )
    .unwrap();

    assert_eq!(
        state.present_pending_exec.len(),
        1,
        "the source wait parks exactly one entry"
    );
    let pin = state
        .present_pending_exec
        .values()
        .next()
        .unwrap()
        .pin
        .expect("Deferred arm takes an entry pin");

    // FreePixmap the source in between park and drain.
    handle_free_pixmap(
        &mut state,
        &mut backend,
        None,
        ClientId(CLIENT),
        SequenceNumber(2),
        &free_pixmap_body(PIXMAP_XID),
    )
    .expect("free pixmap");
    assert!(
        backend
            .calls()
            .iter()
            .any(|call| matches!(call, RecordedCall::FreePixmap(PIXMAP_HOST_XID))),
        "FreePixmap on the source must still free the host pixmap"
    );

    let calls_before = backend.calls().len();
    backend.ready_present_source_waits.push(WAIT_ID);
    drain_ready_present_pixmaps(&mut state, &mut backend);

    assert!(
        backend.calls()[calls_before..].iter().any(|call| matches!(
            call,
            RecordedCall::CopyArea {
                src_host_xid: PIXMAP_HOST_XID,
                dst_host_xid: WINDOW_HOST_XID,
                ..
            }
        )),
        "the drain still executes the copy against the pinned source xid, \
             not a dropped/dead one"
    );
    assert_eq!(
        backend.released_present_sources,
        vec![pin],
        "the entry pin is released exactly once, at execution"
    );
    assert!(state.present_pending_exec.is_empty());
    assert!(state.present_wait_to_id.is_empty());
}

// ────────────────────────────────────────────────────────────────
// Task 7 Step 4: msc-due classification + deferral.
// ────────────────────────────────────────────────────────────────

/// (i) A future-target present must not copy or damage anything until
/// it becomes due. Drives `drain_due_present_pending_exec` directly
/// against a manually parked (source-ready) entry — the due-pass is
/// the sole place a still-parked future-target entry can execute.
#[test]
fn future_target_present_produces_no_copy_until_due() {
    const PRESENT_ID: u64 = 500;
    const WINDOW_HOST_XID: u32 = 0x0040_0601;
    const PIXMAP_HOST_XID: u32 = 0x0040_0602;
    const EFF: u64 = 105;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    backend.present_ust_msc = (100, 0x1000); // clock=100, eff(105) > 101: future.
    seed_present_clock(&mut state, 100, 0x1000);
    // Isolate the plain clock-driven due rule from the idle-display
    // fallback (RecordingBackend defaults `present_display_idle` to
    // `true`, matching "no backend ever composes" — which would
    // otherwise execute this future-target entry immediately via
    // that separate fallback rung, not the one this test pins).
    backend.present_display_idle = false;

    state.present_pending_exec.insert(
        PRESENT_ID,
        present_pending_entry_with(
            PRESENT_ID,
            WINDOW_HOST_XID,
            PIXMAP_HOST_XID,
            Some(EFF),
            true, // source_ready
        ),
    );

    drain_due_present_pending_exec(&mut state, &mut backend);
    assert!(
        backend
            .calls()
            .iter()
            .all(|call| !matches!(call, RecordedCall::CopyArea { .. })),
        "no copy while the target is still in the future"
    );
    assert_eq!(
        state.present_pending_exec.len(),
        1,
        "the entry stays parked"
    );

    // Clock catches up to the target: now due.
    backend.present_ust_msc = (EFF, 0x2000);
    seed_present_clock(&mut state, EFF, 0x2000);
    drain_due_present_pending_exec(&mut state, &mut backend);
    assert!(
        backend.calls().iter().any(|call| matches!(
            call,
            RecordedCall::CopyArea {
                src_host_xid: PIXMAP_HOST_XID,
                dst_host_xid: WINDOW_HOST_XID,
                ..
            }
        )),
        "the copy executes once the clock reaches the target"
    );
    assert!(state.present_pending_exec.is_empty());
}

#[test]
// Mutation: treat an unsupported wake capability as a failed supported arm and execute the target early.
fn unsupported_wake_keeps_flip_driven_present_paced() {
    const PRESENT_ID: u64 = 508;
    const WINDOW_HOST_XID: u32 = 0x0040_0701;
    const PIXMAP_HOST_XID: u32 = 0x0040_0702;
    const TARGET_MSC: u64 = 120;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    backend.present_ust_msc = (100, 0x1000);
    backend.present_flip_in_flight = true;
    backend.present_display_idle = false;
    backend.present_absolute_vblank_arm_supported = false;
    seed_present_clock(&mut state, 100, 0x1000);
    state.present_pending_exec.insert(
        PRESENT_ID,
        present_pending_entry_with(
            PRESENT_ID,
            WINDOW_HOST_XID,
            PIXMAP_HOST_XID,
            Some(TARGET_MSC),
            true,
        ),
    );

    crate::core_loop::run::run_iteration_tail(&mut state, &mut backend);

    assert!(
        backend
            .calls()
            .iter()
            .all(|call| !matches!(call, RecordedCall::CopyArea { .. })),
        "unsupported wake capability alone must not copy a future target while the flip advances"
    );
    assert!(
        state.present_pending_exec.contains_key(&PRESENT_ID),
        "the Present remains parked until the advancing flip clock reaches MSC 120"
    );

    backend.present_ust_msc = (TARGET_MSC, 0x2000);
    backend.present_flip_in_flight = false;
    seed_present_clock(&mut state, TARGET_MSC, 0x2000);
    crate::core_loop::run::run_iteration_tail(&mut state, &mut backend);

    assert!(
        backend.calls().iter().any(|call| matches!(
            call,
            RecordedCall::CopyArea {
                src_host_xid: PIXMAP_HOST_XID,
                dst_host_xid: WINDOW_HOST_XID,
                ..
            }
        )),
        "the parked Present copies when the general MSC reaches its target"
    );
    assert!(state.present_pending_exec.is_empty());
}

/// Review fix (test addition b): the due-pass must RE-PARK an
/// immediate-target entry when `flip_in_flight` is STILL true at
/// drain time — the previously-covered vector only pinned the park
/// decision at ARRIVAL, not the due-pass's own re-evaluation using a
/// freshly sampled `flip_in_flight`.
#[test]
fn due_pass_reparks_immediate_target_when_flip_still_in_flight() {
    const PRESENT_ID: u64 = 509;
    const WINDOW_HOST_XID: u32 = 0x0040_0f01;
    const PIXMAP_HOST_XID: u32 = 0x0040_0f02;
    const CLOCK: u64 = 300;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    backend.present_ust_msc = (CLOCK, 0x1000);
    backend.present_flip_in_flight = true; // still in flight at drain time.
    // Isolate the plain due-pass decision from the fallback rungs.
    backend.present_display_idle = false;
    backend.present_absolute_vblank_arm_supported = false;
    backend.present_scanout_blackout = false;

    state.present_pending_exec.insert(
        PRESENT_ID,
        present_pending_entry_with(
            PRESENT_ID,
            WINDOW_HOST_XID,
            PIXMAP_HOST_XID,
            Some(CLOCK + 1), // immediate target.
            true,
        ),
    );

    drain_due_present_pending_exec(&mut state, &mut backend);

    assert!(
        backend
            .calls()
            .iter()
            .all(|call| !matches!(call, RecordedCall::CopyArea { .. })),
        "an immediate target must stay parked while a flip is still in \
             flight at the due-pass's own re-evaluation, not just at arrival"
    );
    assert_eq!(state.present_pending_exec.len(), 1, "entry stays parked");
}

/// (ii) Immediate-target arrival: executes with no flip in flight,
/// parks with one — driven end-to-end through `process_request` so
/// the arrival evaluation (not just the pure classifier) is pinned.
#[test]
fn immediate_target_present_executes_at_arrival_without_flip_in_flight() {
    const CLIENT: u32 = 30;
    const WINDOW_XID: u32 = 0x00e0_3001;
    const PIXMAP_XID: u32 = 0x00e0_3002;
    const WINDOW_HOST_XID: u32 = 0x0040_3001;
    const PIXMAP_HOST_XID: u32 = 0x0040_3002;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, CLIENT);
    let mut backend = RecordingBackend::new();
    backend.present_ust_msc = (10, 0x1000); // eff will land at clock+1 = 11.
    backend.present_flip_in_flight = false;

    setup_present_pixmap_source_and_dest(
        &mut state,
        CLIENT,
        WINDOW_XID,
        PIXMAP_XID,
        WINDOW_HOST_XID,
        PIXMAP_HOST_XID,
    );

    let mut body = vec![0u8; 68];
    body[0..4].copy_from_slice(&WINDOW_XID.to_le_bytes());
    body[4..8].copy_from_slice(&PIXMAP_XID.to_le_bytes());
    process_request(
        &mut state,
        &mut backend,
        ClientId(CLIENT),
        SequenceNumber(1),
        RequestHeader {
            opcode: 145,
            data: yserver_protocol::x11::present::PIXMAP,
            length_units: u32::try_from(1 + body.len() / 4).unwrap(),
        },
        &body,
        None,
    )
    .unwrap();

    assert!(
        backend.calls().iter().any(|call| matches!(
            call,
            RecordedCall::CopyArea {
                src_host_xid: PIXMAP_HOST_XID,
                dst_host_xid: WINDOW_HOST_XID,
                ..
            }
        )),
        "immediate target with no flip in flight executes at arrival"
    );
    assert!(state.present_pending_exec.is_empty());
}

#[test]
fn immediate_target_present_parks_at_arrival_with_flip_in_flight() {
    const CLIENT: u32 = 31;
    const WINDOW_XID: u32 = 0x00e0_3101;
    const PIXMAP_XID: u32 = 0x00e0_3102;
    const WINDOW_HOST_XID: u32 = 0x0040_3101;
    const PIXMAP_HOST_XID: u32 = 0x0040_3102;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, CLIENT);
    let mut backend = RecordingBackend::new();
    backend.present_ust_msc = (10, 0x1000);
    backend.present_flip_in_flight = true;

    setup_present_pixmap_source_and_dest(
        &mut state,
        CLIENT,
        WINDOW_XID,
        PIXMAP_XID,
        WINDOW_HOST_XID,
        PIXMAP_HOST_XID,
    );

    let mut body = vec![0u8; 68];
    body[0..4].copy_from_slice(&WINDOW_XID.to_le_bytes());
    body[4..8].copy_from_slice(&PIXMAP_XID.to_le_bytes());
    process_request(
        &mut state,
        &mut backend,
        ClientId(CLIENT),
        SequenceNumber(1),
        RequestHeader {
            opcode: 145,
            data: yserver_protocol::x11::present::PIXMAP,
            length_units: u32::try_from(1 + body.len() / 4).unwrap(),
        },
        &body,
        None,
    )
    .unwrap();

    assert!(
        backend
            .calls()
            .iter()
            .all(|call| !matches!(call, RecordedCall::CopyArea { .. })),
        "immediate target with a flip in flight must not copy at arrival"
    );
    assert_eq!(
        state.present_pending_exec.len(),
        1,
        "the present parks instead"
    );
    assert!(
        state
            .present_pending_exec
            .values()
            .next()
            .unwrap()
            .source_ready,
        "parked-for-msc entries are source_ready (only the clock is blocking)"
    );
}

#[test]
fn clocked_async_present_executes_during_in_flight_flip() {
    const CLIENT: u32 = 32;
    const WINDOW_XID: u32 = 0x00e0_3201;
    const PIXMAP_XID: u32 = 0x00e0_3202;
    const WINDOW_HOST_XID: u32 = 0x0040_3201;
    const PIXMAP_HOST_XID: u32 = 0x0040_3202;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, CLIENT);
    let mut backend = RecordingBackend::new();
    backend.present_ust_msc = (10, 0x1000);
    backend.present_flip_in_flight = true;

    setup_present_pixmap_source_and_dest(
        &mut state,
        CLIENT,
        WINDOW_XID,
        PIXMAP_XID,
        WINDOW_HOST_XID,
        PIXMAP_HOST_XID,
    );

    let mut body = vec![0u8; 68];
    body[0..4].copy_from_slice(&WINDOW_XID.to_le_bytes());
    body[4..8].copy_from_slice(&PIXMAP_XID.to_le_bytes());
    body[36..40].copy_from_slice(&crate::present_scheduler::PRESENT_OPTION_ASYNC.to_le_bytes());
    process_request(
        &mut state,
        &mut backend,
        ClientId(CLIENT),
        SequenceNumber(1),
        RequestHeader {
            opcode: 145,
            data: yserver_protocol::x11::present::PIXMAP,
            length_units: u32::try_from(1 + body.len() / 4).unwrap(),
        },
        &body,
        None,
    )
    .unwrap();

    assert!(
        backend.calls().iter().any(|call| matches!(
            call,
            RecordedCall::CopyArea {
                src_host_xid: PIXMAP_HOST_XID,
                dst_host_xid: WINDOW_HOST_XID,
                ..
            }
        )),
        "a clocked immediate async Present must reach the backend successor path instead of parking behind the current flip",
    );
    assert!(
        state.present_pending_exec.is_empty(),
        "the core scheduler must not retain the async successor until vblank",
    );
    assert_eq!(
        state
            .present_complete_gate
            .values()
            .next()
            .map(|gate| gate.effective_target_msc),
        Some(10),
        "the Xorg current-MSC identity must survive through backend completion gating",
    );
}

/// Shared window+pixmap setup for the arrival-evaluation tests above.
fn setup_present_pixmap_source_and_dest(
    state: &mut ServerState,
    client: u32,
    window_xid: u32,
    pixmap_xid: u32,
    window_host_xid: u32,
    pixmap_host_xid: u32,
) {
    use yserver_protocol::x11::{CreatePixmapRequest, CreateWindowRequest};

    state.resources.create_window(
        ClientId(client),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(window_xid),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 800,
            height: 600,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(window_xid));
    if let Some(w) = state.resources.window_mut(ResourceId(window_xid)) {
        w.host_xid = crate::backend::WindowHandle::from_raw(window_host_xid);
    }
    state.resources.create_pixmap(
        ClientId(client),
        CreatePixmapRequest {
            depth: 24,
            pixmap: ResourceId(pixmap_xid),
            drawable: ResourceId(window_xid),
            width: 800,
            height: 600,
        },
    );
    let _ = state.resources.set_pixmap_host_xid(
        ResourceId(pixmap_xid),
        crate::backend::PixmapHandle::from_raw(pixmap_host_xid).expect("valid host pixmap"),
    );
}

/// (iii) A parked immediate-target entry executes on the flip-
/// retirement wakeup, and its `mark_dirty` precedes `maybe_composite`
/// in the SAME `run_iteration_tail` call — reusing Task 4's call-order
/// instrumentation, now exercised via the msc-due path (source_ready,
/// no wait_id) rather than the source-wait path.
#[test]
fn parked_immediate_target_executes_and_marks_dirty_before_compose_in_same_tail() {
    use crate::backend::recording::RecordedCall;

    const PRESENT_ID: u64 = 0x9001;
    const WINDOW_HOST_XID: u32 = 0x0040_9001;
    const PIXMAP_HOST_XID: u32 = 0x0040_9002;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    // The entry parked earlier because a flip was in flight; that
    // flip has now retired (this iteration's fresh sample) and the
    // clock has caught up so the target is due.
    backend.present_ust_msc = (200, 0x3000);
    backend.present_flip_in_flight = false;

    state.present_pending_exec.insert(
        PRESENT_ID,
        present_pending_entry_with(
            PRESENT_ID,
            WINDOW_HOST_XID,
            PIXMAP_HOST_XID,
            Some(200), // eff <= clock: due now.
            true,
        ),
    );

    crate::core_loop::run::run_iteration_tail(&mut state, &mut backend);

    let calls = backend.calls();
    let mark_dirty_idx = calls
        .iter()
        .position(|c| matches!(c, RecordedCall::MarkDirty))
        .expect("due entry executed and marked dirty");
    let composite_idx = calls
        .iter()
        .position(|c| matches!(c, RecordedCall::MaybeComposite))
        .expect("maybe_composite invoked");
    assert!(
        mark_dirty_idx < composite_idx,
        "due-pass mark_dirty ({mark_dirty_idx}) must precede maybe_composite ({composite_idx})"
    );
    assert!(state.present_pending_exec.is_empty());
}

/// (iv) Idle-display fallback: only on `!present_absolute_vblank_arm_supported()`
/// backends, and only when `present_display_idle()` is true, does a
/// still-parked source-ready entry execute early.
#[test]
fn idle_display_fallback_executes_parked_entry_when_display_idle() {
    const PRESENT_ID: u64 = 501;
    const WINDOW_HOST_XID: u32 = 0x0040_0701;
    const PIXMAP_HOST_XID: u32 = 0x0040_0702;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    backend.present_ust_msc = (100, 0x1000);
    backend.present_absolute_vblank_arm_supported = false;
    backend.present_display_idle = true;

    state.present_pending_exec.insert(
        PRESENT_ID,
        present_pending_entry_with(
            PRESENT_ID,
            WINDOW_HOST_XID,
            PIXMAP_HOST_XID,
            Some(200), // far future — never due by clock alone here.
            true,
        ),
    );

    drain_due_present_pending_exec(&mut state, &mut backend);
    assert!(
        backend.calls().iter().any(|call| matches!(
            call,
            RecordedCall::CopyArea {
                src_host_xid: PIXMAP_HOST_XID,
                ..
            }
        )),
        "an idle display with no absolute-arm support must execute the parked entry"
    );
    assert!(state.present_pending_exec.is_empty());
}

#[test]
fn idle_display_fallback_does_not_fire_when_display_not_idle() {
    const PRESENT_ID: u64 = 502;
    const WINDOW_HOST_XID: u32 = 0x0040_0801;
    const PIXMAP_HOST_XID: u32 = 0x0040_0802;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    backend.present_ust_msc = (100, 0x1000);
    backend.present_absolute_vblank_arm_supported = false;
    backend.present_display_idle = false;

    state.present_pending_exec.insert(
        PRESENT_ID,
        present_pending_entry_with(
            PRESENT_ID,
            WINDOW_HOST_XID,
            PIXMAP_HOST_XID,
            Some(200),
            true,
        ),
    );

    drain_due_present_pending_exec(&mut state, &mut backend);
    assert!(
        backend
            .calls()
            .iter()
            .all(|call| !matches!(call, RecordedCall::CopyArea { .. })),
        "not-idle display must not trigger the idle fallback"
    );
    assert_eq!(state.present_pending_exec.len(), 1, "entry stays parked");
}

#[test]
fn idle_display_fallback_does_not_apply_when_arm_supported() {
    // Spec §msc-due: the idle-display fallback applies ONLY on
    // `!present_absolute_vblank_arm_supported()` drivers — even with
    // `present_display_idle() == true`, an arm-capable driver must not
    // take this shortcut (it would reintroduce the early-frame bug
    // for mpv on sequence-capable drivers).
    const PRESENT_ID: u64 = 503;
    const WINDOW_HOST_XID: u32 = 0x0040_0901;
    const PIXMAP_HOST_XID: u32 = 0x0040_0902;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    backend.present_ust_msc = (100, 0x1000);
    backend.present_absolute_vblank_arm_supported = true;
    backend.present_display_idle = true;

    state.present_pending_exec.insert(
        PRESENT_ID,
        present_pending_entry_with(
            PRESENT_ID,
            WINDOW_HOST_XID,
            PIXMAP_HOST_XID,
            Some(200),
            true,
        ),
    );

    drain_due_present_pending_exec(&mut state, &mut backend);
    assert!(
        backend
            .calls()
            .iter()
            .all(|call| !matches!(call, RecordedCall::CopyArea { .. })),
        "arm-supported backends never take the idle-display shortcut"
    );
    assert_eq!(state.present_pending_exec.len(), 1);
}

/// (v) Blackout: parked entries execute AND queued completions deliver
/// with the frozen clock, independent of clock advance.
#[test]
fn blackout_flushes_parked_entries_and_queued_completions() {
    use yserver_protocol::x11::present as x11present;

    const PRESENT_ID: u64 = 504;
    const WINDOW_HOST_XID: u32 = 0x0040_0a01;
    const PIXMAP_HOST_XID: u32 = 0x0040_0a02;
    const QUEUED_ID: u64 = 900;
    const WINDOW_XID: u32 = 0x0000_0a03; // dst_host_xid key for the queued completion

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    backend.present_ust_msc = (100, 0x1000); // frozen: never advances during blackout.
    backend.present_scanout_blackout = true;
    backend.present_absolute_vblank_arm_supported = false;
    backend.present_display_idle = false; // prove blackout fires independent of idle.

    state.present_pending_exec.insert(
        PRESENT_ID,
        present_pending_entry_with(
            PRESENT_ID,
            WINDOW_HOST_XID,
            PIXMAP_HOST_XID,
            Some(5_000), // far future — would never be due by clock or idle alone.
            true,
        ),
    );
    // A queued completion whose target is also far in the future —
    // must still flush, stamped with the frozen clock.
    state.present_pending_complete.push(due_pending_complete(
        WINDOW_XID,
        QUEUED_ID,
        5_000,
        x11present::COMPLETE_MODE_COPY,
        true,
    ));

    drain_due_present_pending_exec(&mut state, &mut backend);

    assert!(
        backend.calls().iter().any(|call| matches!(
            call,
            RecordedCall::CopyArea {
                src_host_xid: PIXMAP_HOST_XID,
                ..
            }
        )),
        "blackout must force-execute the parked msc-due entry"
    );
    assert!(state.present_pending_exec.is_empty());
    assert!(
        state.present_pending_complete.is_empty(),
        "the queued completion must flush too, not just the parked entry"
    );
    assert_eq!(
        backend.signalled_present_wakes,
        vec![QUEUED_ID],
        "the queued completion delivers despite its target being far in the future"
    );
}

/// Review fix (blocker): blackout must flush queued completions even
/// when `present_pending_exec` is completely EMPTY — the DPMS-off
/// case where flips keep retiring normally (so every arrival
/// classifies `ExecuteNow` and the store never accumulates an entry)
/// but `present_pending_complete` still carries an entry gated
/// against a frozen completion clock. Pre-fix, `drain_due_present_pending_exec`'s
/// leading `present_pending_exec.is_empty()` check returned before
/// ever reaching the blackout branch (the sole caller of
/// `fire_all_present_completions_now`), so this entry would park
/// forever.
#[test]
fn blackout_flushes_queued_completions_even_with_empty_exec_store() {
    use yserver_protocol::x11::present as x11present;

    const QUEUED_ID: u64 = 901;
    const WINDOW_XID: u32 = 0x0000_0a04;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    backend.present_ust_msc = (100, 0x1000);
    backend.present_scanout_blackout = true;

    assert!(
        state.present_pending_exec.is_empty(),
        "precondition: the msc-due store is empty — this is the DPMS-off \
             case where flips keep retiring so nothing ever msc-parks"
    );
    state.present_pending_complete.push(due_pending_complete(
        WINDOW_XID,
        QUEUED_ID,
        5_000, // far future — would never be due by clock alone.
        x11present::COMPLETE_MODE_COPY,
        true,
    ));

    drain_due_present_pending_exec(&mut state, &mut backend);

    assert!(
        state.present_pending_complete.is_empty(),
        "the queued completion must flush during blackout even with an \
             empty present_pending_exec store"
    );
    assert_eq!(backend.signalled_present_wakes, vec![QUEUED_ID]);
}

/// Review fix: pin the residual blackout hold-back case documented on
/// `fire_all_present_completions_now` — `drain_due_present_pending_exec`'s
/// blackout branch force-executes only `source_ready` entries, so a
/// `source_ready:false` entry (still waiting on its own producer
/// fence — e.g. an uncovered sliver the successor gate declined to
/// scrap) keeps its window's parked completions held back through
/// the flush, even with blackout on. Once the producer fence signals
/// (`source_ready` flips true), the next due-pass + flush drains
/// both.
#[test]
fn blackout_holds_back_completions_behind_a_not_source_ready_entry_until_it_resolves() {
    use yserver_protocol::x11::present as x11present;

    const WINDOW_HOST_XID: u32 = 0x0040_0a05;
    const PIXMAP_HOST_XID: u32 = 0x0040_0a06;
    const BLOCKER_ID: u64 = 1;
    const QUEUED_ID: u64 = 2;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    backend.present_ust_msc = (100, 0x1000);
    backend.present_scanout_blackout = true;

    // `effective_target_msc: None` for the no-clock blocker so its own
    // execution delivers inline with no `present_complete_gate` row
    // (the no-target arm at `execute_present_pixmap_copy`'s
    // gate insert — `if let Some(eff) = ... `) — otherwise resolving
    // it would additionally require draining the gate (a distinct,
    // already-covered mechanism), which would muddy this test's
    // narrow point: every execution filter here (`due`, idle
    // fallback, blackout) is gated on `source_ready` alone, so this
    // entry is blocked purely by that flag regardless of clock/eff.
    state.present_pending_exec.insert(
        BLOCKER_ID,
        present_pending_entry_with(
            BLOCKER_ID,
            WINDOW_HOST_XID,
            PIXMAP_HOST_XID,
            None,
            false, // still waiting on its own producer fence
        ),
    );
    state.present_pending_complete.push(due_pending_complete(
        WINDOW_HOST_XID,
        QUEUED_ID,
        5_000, // far future — only "due" via the blackout force.
        x11present::COMPLETE_MODE_COPY,
        true,
    ));

    drain_due_present_pending_exec(&mut state, &mut backend);

    assert_eq!(
        state.present_pending_exec.len(),
        1,
        "blackout must NOT force-execute a source_ready:false entry"
    );
    assert_eq!(
        state.present_pending_complete.len(),
        1,
        "the due completion for the same window must stay held back behind \
             the unresolved blocker, even during blackout"
    );
    assert!(backend.signalled_present_wakes.is_empty());

    // The blocker's own producer fence signals.
    state
        .present_pending_exec
        .get_mut(&BLOCKER_ID)
        .unwrap()
        .source_ready = true;

    drain_due_present_pending_exec(&mut state, &mut backend);

    assert!(
        state.present_pending_exec.is_empty(),
        "the next blackout pass must force-execute the now-source_ready blocker"
    );
    assert!(
        state.present_pending_complete.is_empty(),
        "and the queue drains: the held-back completion delivers in the same pass"
    );
    assert_eq!(backend.signalled_present_wakes, vec![QUEUED_ID]);
}

/// (vi) Arm failure / `Ok(0)`: entries execute immediately in the
/// same pass (the third arming call site, `run::arm_present_idle_vblanks`).
#[test]
fn shared_wake_false_executes_distant_parked_entries_immediately() {
    const PRESENT_ID: u64 = 505;
    const WINDOW_HOST_XID: u32 = 0x0040_0b01;
    const PIXMAP_HOST_XID: u32 = 0x0040_0b02;
    const CLOCK: u64 = 100;
    const EFF: u64 = 110;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    backend.present_ust_msc = (CLOCK, 0x1000);
    seed_present_clock(&mut state, CLOCK, 0x1000);
    backend.present_absolute_vblank_arm_supported = true;
    backend.arm_present_shared_wake_result = Some(Ok(false));

    state.present_pending_exec.insert(
        PRESENT_ID,
        present_pending_entry_with(
            PRESENT_ID,
            WINDOW_HOST_XID,
            PIXMAP_HOST_XID,
            Some(EFF),
            true,
        ),
    );

    crate::core_loop::run::arm_present_idle_vblanks(&mut state, &mut backend);

    assert_eq!(backend.armed_shared_wake_crtcs, [0]);
    assert!(
        backend.calls().iter().any(|call| matches!(
            call,
            RecordedCall::CopyArea {
                src_host_xid: PIXMAP_HOST_XID,
                ..
            }
        )),
        "Ok(0) (nothing covered) must execute the entry immediately"
    );
    assert!(state.present_pending_exec.is_empty());
}

#[test]
fn shared_wake_err_executes_distant_parked_entries_immediately() {
    const PRESENT_ID: u64 = 506;
    const WINDOW_HOST_XID: u32 = 0x0040_0c01;
    const PIXMAP_HOST_XID: u32 = 0x0040_0c02;
    const CLOCK: u64 = 100;
    const EFF: u64 = 110;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    backend.present_ust_msc = (CLOCK, 0x1000);
    seed_present_clock(&mut state, CLOCK, 0x1000);
    backend.present_absolute_vblank_arm_supported = true;
    backend.arm_present_shared_wake_result = Some(Err(std::io::ErrorKind::Other));

    state.present_pending_exec.insert(
        PRESENT_ID,
        present_pending_entry_with(
            PRESENT_ID,
            WINDOW_HOST_XID,
            PIXMAP_HOST_XID,
            Some(EFF),
            true,
        ),
    );

    crate::core_loop::run::arm_present_idle_vblanks(&mut state, &mut backend);

    assert!(
        backend.calls().iter().any(|call| matches!(
            call,
            RecordedCall::CopyArea {
                src_host_xid: PIXMAP_HOST_XID,
                ..
            }
        )),
        "Err must execute the entry immediately, same as Ok(0)"
    );
    assert!(state.present_pending_exec.is_empty());
}

#[test]
fn shared_wake_success_leaves_entry_parked() {
    const PRESENT_ID: u64 = 507;
    const WINDOW_HOST_XID: u32 = 0x0040_0d01;
    const PIXMAP_HOST_XID: u32 = 0x0040_0d02;
    const CLOCK: u64 = 100;
    const EFF: u64 = 110;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    backend.present_ust_msc = (CLOCK, 0x1000);
    seed_present_clock(&mut state, CLOCK, 0x1000);
    backend.present_absolute_vblank_arm_supported = true;
    // Default (None) result mimics a real always-succeeds arm.

    state.present_pending_exec.insert(
        PRESENT_ID,
        present_pending_entry_with(
            PRESENT_ID,
            WINDOW_HOST_XID,
            PIXMAP_HOST_XID,
            Some(EFF),
            true,
        ),
    );

    crate::core_loop::run::arm_present_idle_vblanks(&mut state, &mut backend);

    assert!(
        backend
            .calls()
            .iter()
            .all(|call| !matches!(call, RecordedCall::CopyArea { .. })),
        "a successful arm must not execute — the entry stays parked, awaiting the arm's event"
    );
    assert_eq!(backend.armed_shared_wake_crtcs, [0]);
    assert_eq!(state.present_pending_exec.len(), 1);
}

// Mutation: treat a target at or before the completion clock as ahead.
#[test]
fn shared_wake_held_presents_past_target_arm_nothing() {
    use crate::server::PresentCompleteGate;
    use yserver_protocol::x11::present as x11present;

    const WINDOW: u32 = 0x0001_5101;
    const EVENT_ID: u32 = 0x0010_5101;
    const BLOCKER: u64 = 0;
    const FIRST: u64 = 1;
    const COUNT: u64 = 40;
    const CLOCK: u64 = 100;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    peer.set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .expect("bound core round-trip waits");
    let mut backend = RecordingBackend::new();
    backend.present_ust_msc = (CLOCK, 1_000);
    backend.present_absolute_vblank_arm_supported = true;
    seed_present_domain_clock(&mut state, 11, 1, CLOCK, 1_000);
    backend.present_crtc_clock_epoch_by_crtc.insert(11, 1);
    state.present_event_selections.insert(
        EVENT_ID,
        crate::server::PresentEventSelection {
            owner: ClientId(1),
            window: ResourceId(WINDOW),
            event_mask: x11present::EVENT_MASK_COMPLETE_NOTIFY,
        },
    );
    state.present_complete_gate.insert(
        BLOCKER,
        PresentCompleteGate {
            crtc_id: 11,
            crtc_epoch: 1,
            msc_offset: 0,
            effective_target_msc: CLOCK,
            owner: ClientId(1),
            dst_window_xid: WINDOW,
        },
    );
    create_present_test_window(&mut state, WINDOW, 0, 0, 32, 32);
    let target = state
        .resources
        .window_mut(ResourceId(WINDOW))
        .expect("created Present window");
    target.host_xid = crate::backend::WindowHandle::from_raw(WINDOW | 0x0040_0000);
    target.map_state = crate::resources::MapState::Viewable;
    for present_id in FIRST..FIRST + COUNT {
        let pixmap = 0x0041_5100 + present_id as u32;
        state.resources.create_pixmap(
            ClientId(1),
            CreatePixmapRequest {
                depth: 24,
                pixmap: ResourceId(pixmap),
                drawable: ResourceId(WINDOW),
                width: 32,
                height: 32,
            },
        );
        assert!(state.resources.set_pixmap_host_xid(
            ResourceId(pixmap),
            crate::backend::PixmapHandle::from_raw(pixmap).expect("valid Present pixmap host xid"),
        ));
        let mut body = pixmap_body(WINDOW, pixmap);
        body[8..12].copy_from_slice(&(present_id as u32).to_le_bytes());
        body[36..40].copy_from_slice(&crate::present_scheduler::PRESENT_OPTION_ASYNC.to_le_bytes());
        body[44..52].copy_from_slice(&CLOCK.to_le_bytes());
        let sequence = u16::try_from((present_id - FIRST) * 2 + 1).unwrap();
        assert!(
            run_core_present_request_and_wait(
                &mut state,
                &mut backend,
                &mut peer,
                sequence,
                sequence + 1,
                x11present::PIXMAP,
                &body,
            )
            .is_empty()
        );
    }
    assert!(state.present_complete_gate.contains_key(&BLOCKER));
    assert_eq!(backend.enqueued_present_completions.len(), COUNT as usize);
    assert_eq!(
        backend
            .enqueued_present_completions
            .iter()
            .map(|(event, _)| event.present_id)
            .collect::<Vec<_>>(),
        (FIRST..FIRST + COUNT).collect::<Vec<_>>()
    );
    backend.completed_present_events_to_drain.extend(
        std::mem::take(&mut backend.enqueued_present_completions)
            .into_iter()
            .map(|(event, _)| event),
    );
    let blocked_wire = run_core_present_wait_drain(
        &mut state,
        &mut backend,
        &mut peer,
        u16::try_from(COUNT * 2 + 1).unwrap(),
    );
    assert!(
        blocked_wire.is_empty(),
        "the older gate must hold the real completions: wire={}, pending={}, gates={:?}",
        blocked_wire.len(),
        state.present_pending_complete.len(),
        state.present_complete_gate
    );
    assert_eq!(state.present_pending_complete.len(), COUNT as usize);

    assert!(
        backend.armed_shared_wake_crtcs.is_empty(),
        "reached completions must not arm: arms={:?}, targets={:?}, clocks={:?}",
        backend.armed_shared_wake_crtcs,
        state
            .present_pending_complete
            .iter()
            .map(|pending| pending.effective_target_msc)
            .collect::<Vec<_>>(),
        state.present_crtc_clocks
    );
    state.present_complete_gate.remove(&BLOCKER);
    let wire = run_core_present_wait_drain(
        &mut state,
        &mut backend,
        &mut peer,
        u16::try_from(COUNT * 2 + 2).unwrap(),
    );
    assert!(state.present_pending_complete.is_empty());
    let serials: Vec<u32> = wire
        .chunks_exact(40)
        .map(|event| u32::from_le_bytes(event[20..24].try_into().unwrap()))
        .collect();
    assert_eq!(
        serials,
        (FIRST..FIRST + COUNT)
            .map(|id| id as u32)
            .collect::<Vec<_>>()
    );
}

// Mutation: restore per-target arming, stop re-arming after the first wake, or skip DestroyWindow cleanup.
#[test]
fn shared_wake_future_targets_make_one_call_per_domain_per_iteration() {
    use yserver_protocol::x11::present as x11present;

    const FIRST_WINDOW: u32 = 0x0001_5201;
    const SECOND_WINDOW: u32 = 0x0001_5202;
    const DESTROYED_WINDOW: u32 = 0x0001_5203;
    const FIRST_EVENT: u32 = 0x0010_5201;
    const SECOND_EVENT: u32 = 0x0010_5202;
    const COUNT_PER_WINDOW: u64 = 130;
    const FIRST_ID: u32 = 1;
    const SECOND_ID: u32 = 1001;
    const CLOCK: u64 = u64::MAX - 32;
    let tick_msc = |step: u64| {
        if step <= 32 {
            CLOCK.wrapping_add(step)
        } else {
            step - 32
        }
    };

    let mut state = ServerState::new();
    let mut first_peer = install_client(&mut state, 1);
    first_peer
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .expect("bound completion round-trip waits");
    let mut backend = RecordingBackend::new();
    backend.present_ust_msc = (CLOCK, 1_000);
    backend.present_flip_in_flight = true;
    backend.present_absolute_vblank_arm_supported = true;
    state.present_event_selections.extend([
        (
            FIRST_EVENT,
            crate::server::PresentEventSelection {
                owner: ClientId(1),
                window: ResourceId(FIRST_WINDOW),
                event_mask: x11present::EVENT_MASK_COMPLETE_NOTIFY,
            },
        ),
        (
            SECOND_EVENT,
            crate::server::PresentEventSelection {
                owner: ClientId(1),
                window: ResourceId(SECOND_WINDOW),
                event_mask: x11present::EVENT_MASK_COMPLETE_NOTIFY,
            },
        ),
    ]);

    let mut requests = Vec::with_capacity((COUNT_PER_WINDOW * 2 + 3) as usize);
    let mut sequence = 1_u16;
    for step in 1..=COUNT_PER_WINDOW {
        let target = tick_msc(step);
        for (window, pixmap, present_id) in [
            (
                FIRST_WINDOW,
                0x0040_5200 + step as u32,
                FIRST_ID + step as u32 - 1,
            ),
            (
                SECOND_WINDOW,
                0x0041_5200 + step as u32,
                SECOND_ID + step as u32 - 1,
            ),
        ] {
            let body =
                future_present_pixmap_request(&mut state, window, pixmap, present_id, target);
            requests.push((
                SequenceNumber(sequence),
                RequestHeader {
                    opcode: 145,
                    data: x11present::PIXMAP,
                    length_units: u32::try_from(1 + body.len() / 4).unwrap(),
                },
                body,
            ));
            sequence = sequence.checked_add(1).expect("test sequence fits");
        }
    }
    let destroyed_body = future_present_pixmap_request(
        &mut state,
        DESTROYED_WINDOW,
        0x0042_5201,
        0x5201,
        tick_msc(COUNT_PER_WINDOW),
    );
    requests.push((
        SequenceNumber(sequence),
        RequestHeader {
            opcode: 145,
            data: x11present::PIXMAP,
            length_units: u32::try_from(1 + destroyed_body.len() / 4).unwrap(),
        },
        destroyed_body,
    ));
    sequence = sequence.checked_add(1).expect("test sequence fits");
    requests.push((
        SequenceNumber(sequence),
        RequestHeader {
            opcode: 4,
            data: 0,
            length_units: 2,
        },
        DESTROYED_WINDOW.to_le_bytes().to_vec(),
    ));
    sequence = sequence.checked_add(1).expect("test sequence fits");
    requests.push((
        SequenceNumber(sequence),
        RequestHeader {
            opcode: 43,
            data: 0,
            length_units: 1,
        },
        Vec::new(),
    ));

    seed_present_clock(&mut state, CLOCK, 1_000);
    let wire =
        run_core_present_requests_and_wait(&mut state, &mut backend, &mut first_peer, requests);
    assert!(
        wire.is_empty(),
        "future requests and DestroyWindow emit no completion"
    );
    assert_eq!(
        state.present_pending_exec.len(),
        (COUNT_PER_WINDOW * 2) as usize
    );
    assert!(
        state
            .present_pending_exec
            .values()
            .all(|entry| { entry.pending.request.window() != DESTROYED_WINDOW })
    );
    assert_eq!(state.present_pending_complete.len(), 0);
    let shared_crtc = *backend
        .armed_shared_wake_crtcs
        .first()
        .expect("future Present targets request a shared wake");
    seed_present_domain_clock(&mut state, shared_crtc, 0, CLOCK, 1_000);
    assert!(
        backend
            .armed_shared_wake_crtcs
            .iter()
            .all(|&crtc| crtc == shared_crtc)
    );

    let mut observed_by_window = std::collections::BTreeMap::<u32, Vec<(u32, u64)>>::new();
    for step in 1..=COUNT_PER_WINDOW {
        let msc = tick_msc(step);
        let ust = 1_000 + step;
        backend
            .present_ust_msc_by_crtc
            .insert(shared_crtc, (msc, ust));
        seed_present_domain_clock(&mut state, shared_crtc, 0, msc, ust);
        let calls_before = backend.armed_shared_wake_crtcs.len();
        let first_wire =
            run_core_present_wait_drain(&mut state, &mut backend, &mut first_peer, sequence);
        sequence = sequence.checked_add(1).expect("test sequence fits");
        assert!(
            first_wire.is_empty(),
            "copies enqueue completion events for the next drain"
        );
        let expect_wake = step < COUNT_PER_WINDOW;
        assert_eq!(
            backend.armed_shared_wake_crtcs.len() - calls_before,
            usize::from(expect_wake),
            "one shared wake for this CRTC while any future work remains at target {step}"
        );
        backend.completed_present_events_to_drain.extend(
            std::mem::take(&mut backend.enqueued_present_completions)
                .into_iter()
                .map(|(event, _)| event),
        );

        let calls_before = backend.armed_shared_wake_crtcs.len();
        let wire = run_core_present_wait_drain(&mut state, &mut backend, &mut first_peer, sequence);
        sequence = sequence.checked_add(1).expect("test sequence fits");
        assert_eq!(
            backend.armed_shared_wake_crtcs.len() - calls_before,
            usize::from(expect_wake),
            "one shared wake for this CRTC while any future work remains during completion drain"
        );
        assert_eq!(wire.len(), 80, "both windows complete at target {step}");
        for event in wire.chunks_exact(40) {
            assert_eq!(u64::from_le_bytes(event[32..40].try_into().unwrap()), msc);
            let window = u32::from_le_bytes(event[16..20].try_into().unwrap());
            let present_id = u32::from_le_bytes(event[20..24].try_into().unwrap());
            observed_by_window
                .entry(window)
                .or_default()
                .push((present_id, msc));
        }
        assert_eq!(
            state.present_pending_complete.len(),
            0,
            "all due completions drain at step={step}, msc={msc}"
        );
        assert_eq!(
            state.present_pending_exec.len(),
            ((COUNT_PER_WINDOW - step) * 2) as usize
        );
    }

    for (window, first_id) in [(FIRST_WINDOW, FIRST_ID), (SECOND_WINDOW, SECOND_ID)] {
        let observed = observed_by_window
            .remove(&window)
            .expect("each Present window emitted completions");
        let expected = (0..COUNT_PER_WINDOW)
            .map(|offset| (first_id + offset as u32, tick_msc(offset + 1)))
            .collect::<Vec<_>>();
        assert_eq!(
            observed, expected,
            "window 0x{window:x} completion order and MSC"
        );
    }
    assert!(observed_by_window.is_empty());
    assert!(state.present_pending_complete.is_empty());
    assert!(state.present_pending_exec.is_empty());
    assert!(
        backend
            .armed_shared_wake_crtcs
            .iter()
            .all(|&crtc| crtc == shared_crtc)
    );
}

// Mutation: require a nonzero MSC even when there is no usable clock.
#[test]
fn shared_wake_no_usable_clock_counts_as_ahead() {
    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    backend.present_absolute_vblank_arm_supported = true;
    backend.present_crtc_clock_epoch_by_crtc.insert(11, 1);
    let mut pending = due_pending_complete(0x1521, 1, 20, 0, false);
    pending.event.crtc_id = 11;
    pending.event.crtc_epoch = 1;
    state.present_pending_complete.push(pending);

    crate::core_loop::run::arm_present_idle_vblanks(&mut state, &mut backend);

    assert_eq!(backend.armed_shared_wake_crtcs, [11]);
}

// Mutation: use the completion clock for failure fallback (or report covered on failure).
#[test]
fn shared_wake_failure_executes_parked_entries_immediately() {
    use crate::backend::recording::RecordedCall;

    const FAR_ID: u64 = 1;
    const NEXT_ID: u64 = 2;
    const DISTANT_ID: u64 = 3;
    const FAR_PIXMAP: u32 = 0x0040_5301;
    const NEXT_PIXMAP: u32 = 0x0040_5302;
    const DISTANT_PIXMAP: u32 = 0x0040_5303;

    for result in [Some(Ok(false)), Some(Err(std::io::ErrorKind::Other))] {
        let mut state = ServerState::new();
        let _peer = install_client(&mut state, 1);
        let mut backend = RecordingBackend::new();
        backend.present_ust_msc = (100, 1_000);
        backend.present_absolute_vblank_arm_supported = true;
        backend.arm_present_shared_wake_result = result;
        seed_present_clock(&mut state, 100, 1_000);
        backend.present_flip_in_flight = true;
        dispatch_future_present_pixmap(
            &mut state,
            &mut backend,
            0x1531,
            FAR_PIXMAP,
            FAR_ID as u32,
            120,
        );
        dispatch_future_present_pixmap(
            &mut state,
            &mut backend,
            0x1532,
            NEXT_PIXMAP,
            NEXT_ID as u32,
            101,
        );
        dispatch_future_present_pixmap(
            &mut state,
            &mut backend,
            0x1533,
            DISTANT_PIXMAP,
            DISTANT_ID as u32,
            102,
        );
        let parked = state
            .present_pending_exec
            .values()
            .next()
            .expect("future Present is parked");
        state
            .present_crtc_clocks
            .get_mut(&(parked.pending.crtc_id, parked.pending.crtc_epoch))
            .expect("Present's general clock was cached at arrival")
            .completion = crate::backend::PresentClockSample {
            msc: 90,
            ust: 900,
            source: crate::backend::PresentClockSource::PageFlip,
        };

        crate::core_loop::run::arm_present_idle_vblanks(&mut state, &mut backend);

        assert!(backend.calls().iter().any(|call| matches!(
            call,
            RecordedCall::CopyArea {
                src_host_xid: FAR_PIXMAP,
                ..
            }
        )));
        assert!(
            backend.calls().iter().any(|call| matches!(
                call,
                RecordedCall::CopyArea {
                    src_host_xid: DISTANT_PIXMAP,
                    ..
                }
            )),
            "general MSC 100 makes target 102 distant despite completion MSC 90"
        );
        assert!(!backend.calls().iter().any(|call| matches!(
            call,
            RecordedCall::CopyArea {
                src_host_xid: NEXT_PIXMAP,
                ..
            }
        )));
        assert!(!state.present_pending_exec.contains_key(&FAR_ID));
        assert!(!state.present_pending_exec.contains_key(&DISTANT_ID));
        assert!(state.present_pending_exec.contains_key(&NEXT_ID));
    }
}

// Mutation: omit next-MSC entries while a page flip is in flight.
#[test]
fn shared_wake_next_msc_entry_behind_a_flip_is_armed() {
    const PRESENT_ID: u64 = 1;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    backend.present_ust_msc = (100, 1_000);
    backend.present_absolute_vblank_arm_supported = true;
    backend.present_flip_in_flight = true;
    seed_present_clock(&mut state, 100, 1_000);
    dispatch_future_present_pixmap(&mut state, &mut backend, 0x1541, 0x5401, 1, 101);

    crate::core_loop::run::arm_present_idle_vblanks(&mut state, &mut backend);

    assert_eq!(backend.armed_shared_wake_crtcs, [2]);
    assert!(state.present_pending_exec.contains_key(&PRESENT_ID));
    assert!(backend.calls().iter().all(|call| !matches!(
        call,
        crate::backend::recording::RecordedCall::CopyArea { .. }
    )));
}

// Mutation: route NotifyMSC through the tagged shared slot instead of the idle vblank path.
#[test]
fn notify_msc_pacing_unchanged_with_shared_wake() {
    use yserver_protocol::x11::present as x11present;

    const WINDOW: u32 = 0x0000_1551;
    const EVENT_ID: u32 = 0x0010_1551;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    peer.set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .expect("bound core round-trip waits");
    let mut backend = RecordingBackend::new();
    backend.present_absolute_vblank_arm_supported = true;
    backend.present_ust_msc = (100, 1_000);
    seed_present_clock(&mut state, 100, 1_000);
    state.present_event_selections.insert(
        EVENT_ID,
        crate::server::PresentEventSelection {
            owner: ClientId(1),
            window: ResourceId(WINDOW),
            event_mask: x11present::EVENT_MASK_COMPLETE_NOTIFY,
        },
    );
    state
        .present_pending_msc
        .push(crate::server::PendingNotifyMsc {
            owner: ClientId(1),
            window: WINDOW,
            crtc_id: 0,
            crtc_epoch: 0,
            msc_offset: 0,
            serial: 77,
            target_msc: 110,
            divisor: 0,
            remainder: 0,
            byte_order: ClientByteOrder::LittleEndian,
        });

    assert!(run_core_present_wait_drain(&mut state, &mut backend, &mut peer, 1).is_empty());
    assert_eq!(backend.armed_idle_vblank_targets, [(0, vec![110])]);
    assert!(backend.armed_shared_wake_crtcs.is_empty());

    assert_eq!(state.present_pending_msc.len(), 1);
    backend.present_ust_msc = (110, 2_000);
    seed_present_clock(&mut state, 110, 2_000);
    let wire = run_core_present_wait_drain(&mut state, &mut backend, &mut peer, 2);
    assert!(state.present_pending_msc.is_empty());
    assert_eq!(
        wire.len(),
        40,
        "one NotifyMSC event is emitted on its target"
    );
    assert_eq!(wire[0], 35, "GenericEvent");
    assert_eq!(wire[10], x11present::COMPLETE_KIND_NOTIFY_MSC);
    assert_eq!(
        u32::from_le_bytes(wire[12..16].try_into().unwrap()),
        EVENT_ID
    );
    assert_eq!(u32::from_le_bytes(wire[20..24].try_into().unwrap()), 77);
    assert_eq!(u64::from_le_bytes(wire[24..32].try_into().unwrap()), 2_000);
    assert_eq!(u64::from_le_bytes(wire[32..40].try_into().unwrap()), 110);
}

/// (vii) One-clock contract / no-reclassification: a present arriving
/// when the GENERAL clock is ahead of the (deliberately stale)
/// completion clock must classify against the general clock alone —
/// eff == general + 1 reads as immediate-target, never future.
#[test]
fn arrival_classifies_against_general_clock_not_completion_clock() {
    const CLIENT: u32 = 32;
    const WINDOW_XID: u32 = 0x00e0_3201;
    const PIXMAP_XID: u32 = 0x00e0_3202;
    const WINDOW_HOST_XID: u32 = 0x0040_3201;
    const PIXMAP_HOST_XID: u32 = 0x0040_3202;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, CLIENT);
    let mut backend = RecordingBackend::new();
    // General clock (used for eff + msc-due) is well ahead of the
    // completion clock (used only for stamping/gate release —
    // untouched by this path). If the arrival evaluation mistakenly
    // read the completion clock, eff (general_clock + 1 = 101) would
    // be far past completion_clock + 1 (11) and misclassify as
    // future-target, parking instead of executing.
    backend.present_ust_msc = (100, 0x1000);
    backend.present_completion_clock = Some(crate::backend::PresentClockSample {
        msc: 10,
        ust: 0x0010,
        source: crate::backend::PresentClockSource::PageFlip,
    });
    backend.present_flip_in_flight = false;

    setup_present_pixmap_source_and_dest(
        &mut state,
        CLIENT,
        WINDOW_XID,
        PIXMAP_XID,
        WINDOW_HOST_XID,
        PIXMAP_HOST_XID,
    );

    let mut body = vec![0u8; 68];
    body[0..4].copy_from_slice(&WINDOW_XID.to_le_bytes());
    body[4..8].copy_from_slice(&PIXMAP_XID.to_le_bytes());
    process_request(
        &mut state,
        &mut backend,
        ClientId(CLIENT),
        SequenceNumber(1),
        RequestHeader {
            opcode: 145,
            data: yserver_protocol::x11::present::PIXMAP,
            length_units: u32::try_from(1 + body.len() / 4).unwrap(),
        },
        &body,
        None,
    )
    .unwrap();

    assert!(
        backend.calls().iter().any(|call| matches!(
            call,
            RecordedCall::CopyArea {
                src_host_xid: PIXMAP_HOST_XID,
                ..
            }
        )),
        "eff == general_clock + 1 must classify as immediate-target (execute at arrival), \
             not future-target — a completion-clock-driven misclassification would park instead"
    );
    assert!(state.present_pending_exec.is_empty());
}

/// (viii) Combined source-wait + msc-due: the source wait signals
/// (source_ready becomes true) but msc-due says Park — the entry
/// stays parked with no copy; only once the clock catches up does the
/// due-pass execute it.
#[test]
fn source_wait_resolution_reclassifies_and_stays_parked_until_due() {
    const WAIT_ID: u64 = 44;
    const PRESENT_ID: u64 = 600;
    const WINDOW_HOST_XID: u32 = 0x0040_0e01;
    const PIXMAP_HOST_XID: u32 = 0x0040_0e02;
    const EFF: u64 = 205;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    backend.present_ust_msc = (100, 0x1000); // eff(205) is far future at wait-resolution time.

    let mut entry = present_pending_entry_with(
        PRESENT_ID,
        WINDOW_HOST_XID,
        PIXMAP_HOST_XID,
        Some(EFF),
        false,
    );
    entry.wait_id = Some(WAIT_ID);
    state.present_wait_to_id.insert(WAIT_ID, PRESENT_ID);
    state.present_pending_exec.insert(PRESENT_ID, entry);

    backend.ready_present_source_waits.push(WAIT_ID);
    drain_ready_present_pixmaps(&mut state, &mut backend);

    assert!(
        backend
            .calls()
            .iter()
            .all(|call| !matches!(call, RecordedCall::CopyArea { .. })),
        "source signalled but msc-due says Park: no copy yet"
    );
    assert_eq!(
        state.present_pending_exec.len(),
        1,
        "the entry stays parked, now purely on msc-due"
    );
    let reclassified = state.present_pending_exec.get(&PRESENT_ID).unwrap();
    assert!(
        reclassified.source_ready,
        "wait resolution still marks source_ready = true"
    );
    assert!(
        reclassified.wait_id.is_none(),
        "the wait_id clears — this entry no longer waits on the source"
    );
    assert_eq!(
        backend.finished_present_source_waits,
        vec![WAIT_ID],
        "the WAIT pin still drops at wait resolution regardless of the msc-due outcome"
    );
    assert!(
        backend.released_present_sources.is_empty(),
        "the ENTRY pin must NOT be released yet — the entry is still parked"
    );

    // Clock catches up: the due-pass executes it.
    backend.present_ust_msc = (EFF, 0x2000);
    drain_due_present_pending_exec(&mut state, &mut backend);
    assert!(
        backend.calls().iter().any(|call| matches!(
            call,
            RecordedCall::CopyArea {
                src_host_xid: PIXMAP_HOST_XID,
                ..
            }
        )),
        "once due, the due-pass executes the copy"
    );
    assert!(state.present_pending_exec.is_empty());
}

#[test]
fn present_pixmap_update_region_emits_damage_on_destination_window() {
    use crate::server::DamageObject;
    use yserver_protocol::x11::{CreatePixmapRequest, CreateWindowRequest, xfixes::RegionRect};

    const CLIENT: u32 = 14;
    const WINDOW_XID: u32 = 0x00e0_0103;
    const PIXMAP_XID: u32 = 0x00e0_0104;
    const DAMAGE_XID: u32 = 0x00e0_0105;
    const UPDATE_REGION_XID: u32 = 0x00e0_0106;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, CLIENT);
    let mut backend = RecordingBackend::new();

    state.resources.create_window(
        ClientId(CLIENT),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(WINDOW_XID),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 800,
            height: 600,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(WINDOW_XID));
    if let Some(w) = state.resources.window_mut(ResourceId(WINDOW_XID)) {
        w.host_xid = crate::backend::WindowHandle::from_raw(0x400103);
    }

    state.resources.create_pixmap(
        ClientId(CLIENT),
        CreatePixmapRequest {
            depth: 24,
            pixmap: ResourceId(PIXMAP_XID),
            drawable: ResourceId(WINDOW_XID),
            width: 800,
            height: 600,
        },
    );
    let _ = state.resources.set_pixmap_host_xid(
        ResourceId(PIXMAP_XID),
        crate::backend::PixmapHandle::from_raw(0x400104).expect("valid host pixmap"),
    );

    state.damage_objects.insert(
        DAMAGE_XID,
        DamageObject {
            owner: ClientId(CLIENT),
            drawable: ResourceId(WINDOW_XID),
            level: 3,
            rects: Vec::new(),
            pending_notify_fired: false,
            last_reported_geometry: None,
            list_pos: 0,
            created: 0,
        },
    );
    state.xfixes_regions.insert(
        UPDATE_REGION_XID,
        crate::server::XFixesRegion {
            owner: ClientId(CLIENT),
            region: crate::region::Region::from_rects(&[
                RegionRect {
                    x: 100,
                    y: 120,
                    width: 80,
                    height: 40,
                },
                RegionRect {
                    x: 220,
                    y: 260,
                    width: 25,
                    height: 35,
                },
            ]),
        },
    );

    let mut body = vec![0u8; 68];
    body[0..4].copy_from_slice(&WINDOW_XID.to_le_bytes());
    body[4..8].copy_from_slice(&PIXMAP_XID.to_le_bytes());
    body[16..20].copy_from_slice(&UPDATE_REGION_XID.to_le_bytes());

    process_request(
        &mut state,
        &mut backend,
        ClientId(CLIENT),
        SequenceNumber(1),
        RequestHeader {
            opcode: 145,
            data: yserver_protocol::x11::present::PIXMAP,
            length_units: u32::try_from(1 + body.len() / 4).unwrap(),
        },
        &body,
        None,
    )
    .unwrap();

    let damage = state
        .damage_objects
        .get(&DAMAGE_XID)
        .expect("damage object");
    assert_eq!(
        damage.rects,
        state.xfixes_regions[&UPDATE_REGION_XID].rects(),
        "PresentPixmap must feed its update region into DAMAGE on the destination window",
    );
}

#[test]
fn present_pixmap_synced_update_region_emits_damage_on_destination_window() {
    // Regression: the explicit-sync (v1.4) PixmapSynced path copied
    // the client pixmap into the window but never reported X11 damage,
    // unlike the v1.0 Pixmap path. Under an external compositor
    // (fastcompmgr) the missing DamageNotify meant the window never
    // recomposited — vkcube froze until a drag forced a repaint.
    use crate::server::DamageObject;
    use yserver_protocol::x11::{CreatePixmapRequest, CreateWindowRequest, xfixes::RegionRect};

    const CLIENT: u32 = 17;
    const WINDOW_XID: u32 = 0x00e0_0403;
    const PIXMAP_XID: u32 = 0x00e0_0404;
    const DAMAGE_XID: u32 = 0x00e0_0405;
    const UPDATE_REGION_XID: u32 = 0x00e0_0406;
    const ACQUIRE_SYNCOBJ: u32 = 0x00e0_0407;
    const ACQUIRE_VALUE: u64 = 42;
    const RELEASE_SYNCOBJ: u32 = 0x00e0_0408;
    const RELEASE_VALUE: u64 = 43;
    const WAIT_ID: u64 = 77;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, CLIENT);
    let mut backend = RecordingBackend::new();
    backend.present_syncobj_wait = crate::backend::PresentSourceWait::Deferred(WAIT_ID);
    // Task 3: PresentPixmapSynced now validates both syncobjs per
    // presentproto 1.4 — an unregistered xid (or a None release syncobj)
    // is a Value error before any arm. Register both as imported so this
    // test exercises the copy/damage path, not the new rejection.
    backend.seed_dri3_syncobj_for_test(ACQUIRE_SYNCOBJ, ClientId(CLIENT));
    let original_release = backend.seed_dri3_syncobj_for_test(RELEASE_SYNCOBJ, ClientId(CLIENT));

    state.resources.create_window(
        ClientId(CLIENT),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(WINDOW_XID),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 800,
            height: 600,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(WINDOW_XID));
    if let Some(w) = state.resources.window_mut(ResourceId(WINDOW_XID)) {
        w.host_xid = crate::backend::WindowHandle::from_raw(0x400403);
    }

    state.resources.create_pixmap(
        ClientId(CLIENT),
        CreatePixmapRequest {
            depth: 24,
            pixmap: ResourceId(PIXMAP_XID),
            drawable: ResourceId(WINDOW_XID),
            width: 800,
            height: 600,
        },
    );
    let _ = state.resources.set_pixmap_host_xid(
        ResourceId(PIXMAP_XID),
        crate::backend::PixmapHandle::from_raw(0x400404).expect("valid host pixmap"),
    );

    state.damage_objects.insert(
        DAMAGE_XID,
        DamageObject {
            owner: ClientId(CLIENT),
            drawable: ResourceId(WINDOW_XID),
            level: 3,
            rects: Vec::new(),
            pending_notify_fired: false,
            last_reported_geometry: None,
            list_pos: 0,
            created: 0,
        },
    );
    state.xfixes_regions.insert(
        UPDATE_REGION_XID,
        crate::server::XFixesRegion {
            owner: ClientId(CLIENT),
            region: crate::region::Region::from_rects(&[
                RegionRect {
                    x: 100,
                    y: 120,
                    width: 80,
                    height: 40,
                },
                RegionRect {
                    x: 220,
                    y: 260,
                    width: 25,
                    height: 35,
                },
            ]),
        },
    );

    // PixmapSynced (opcode 5) fixed prefix = 84 bytes:
    //   window(4) pixmap(4) serial(4) valid(4) update(4) x_off(2)
    //   y_off(2) target_crtc(4) acquire_syncobj(4) release_syncobj(4)
    //   acquire_point(8) release_point(8) options(4) pad(4)
    //   target_msc(8) divisor(8) remainder(8).
    let mut body = vec![0u8; 84];
    body[0..4].copy_from_slice(&WINDOW_XID.to_le_bytes());
    body[4..8].copy_from_slice(&PIXMAP_XID.to_le_bytes());
    body[16..20].copy_from_slice(&UPDATE_REGION_XID.to_le_bytes());
    body[28..32].copy_from_slice(&ACQUIRE_SYNCOBJ.to_le_bytes());
    body[32..36].copy_from_slice(&RELEASE_SYNCOBJ.to_le_bytes());
    body[36..44].copy_from_slice(&ACQUIRE_VALUE.to_le_bytes());
    body[44..52].copy_from_slice(&RELEASE_VALUE.to_le_bytes());

    process_request(
        &mut state,
        &mut backend,
        ClientId(CLIENT),
        SequenceNumber(1),
        RequestHeader {
            opcode: 145,
            data: yserver_protocol::x11::present::PIXMAP_SYNCED,
            length_units: u32::try_from(1 + body.len() / 4).unwrap(),
        },
        &body,
        None,
    )
    .unwrap();

    assert_eq!(
        backend.armed_present_syncobj_waits,
        vec![(0x400404, ACQUIRE_SYNCOBJ, ACQUIRE_VALUE)],
        "PixmapSynced must arm its explicit acquire timeline point",
    );
    assert!(
        state.damage_objects[&DAMAGE_XID].rects.is_empty(),
        "the destination must not be copied or damaged before acquire signals",
    );
    assert_eq!(state.present_pending_exec.len(), 1);
    backend
        .dri3_free_syncobj(ClientId(CLIENT), RELEASE_SYNCOBJ)
        .expect("free original release syncobj");
    let replacement = backend.seed_dri3_syncobj_for_test(RELEASE_SYNCOBJ, ClientId(CLIENT));
    let parked_release = match &state
        .present_pending_exec
        .values()
        .next()
        .expect("parked synced Present")
        .pending
        .wake
    {
        crate::backend::PresentWake::PixmapSynced { release, .. } => release,
        crate::backend::PresentWake::Pixmap { .. } => panic!("expected synced wake"),
    };
    assert!(
        std::sync::Arc::ptr_eq(parked_release, &original_release),
        "accepted Present must retain the original release object"
    );
    assert!(
        !std::sync::Arc::ptr_eq(parked_release, &replacement),
        "reusing the XID must not retarget the parked Present"
    );
    parked_release
        .signal(RELEASE_VALUE)
        .expect("pinned release remains usable after FreeSyncobj");
    assert_eq!(
        *backend.signalled_dri3_syncobjs.lock().unwrap(),
        vec![(RELEASE_SYNCOBJ, RELEASE_VALUE)]
    );
    assert_eq!(
        state.present_pending_exec.values().next().unwrap().pin,
        Some(1),
        "the Deferred arm takes an entry pin on the source drawable"
    );

    backend.ready_present_source_waits.push(WAIT_ID);
    drain_ready_present_pixmaps(&mut state, &mut backend);

    let damage = state
        .damage_objects
        .get(&DAMAGE_XID)
        .expect("damage object");
    assert_eq!(
        damage.rects,
        state.xfixes_regions[&UPDATE_REGION_XID].rects(),
        "PresentPixmapSynced must feed its update region into DAMAGE on the destination window",
    );
}

#[test]
fn present_pixmap_copy_uses_update_region_rects_as_copy_clips() {
    use crate::backend::recording::RecordedCall;
    use yserver_protocol::x11::{CreatePixmapRequest, CreateWindowRequest, xfixes::RegionRect};

    const CLIENT: u32 = 15;
    const WINDOW_XID: u32 = 0x00e0_0203;
    const PIXMAP_XID: u32 = 0x00e0_0204;
    const UPDATE_REGION_XID: u32 = 0x00e0_0206;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, CLIENT);
    let mut backend = RecordingBackend::new();

    state.resources.create_window(
        ClientId(CLIENT),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(WINDOW_XID),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 800,
            height: 600,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(WINDOW_XID));
    if let Some(w) = state.resources.window_mut(ResourceId(WINDOW_XID)) {
        w.host_xid = crate::backend::WindowHandle::from_raw(0x400203);
    }

    state.resources.create_pixmap(
        ClientId(CLIENT),
        CreatePixmapRequest {
            depth: 24,
            pixmap: ResourceId(PIXMAP_XID),
            drawable: ResourceId(WINDOW_XID),
            width: 800,
            height: 600,
        },
    );
    let _ = state.resources.set_pixmap_host_xid(
        ResourceId(PIXMAP_XID),
        crate::backend::PixmapHandle::from_raw(0x400204).expect("valid host pixmap"),
    );

    state.xfixes_regions.insert(
        UPDATE_REGION_XID,
        crate::server::XFixesRegion {
            owner: ClientId(CLIENT),
            region: crate::region::Region::from_rects(&[
                RegionRect {
                    x: 100,
                    y: 120,
                    width: 80,
                    height: 40,
                },
                RegionRect {
                    x: 220,
                    y: 260,
                    width: 25,
                    height: 35,
                },
            ]),
        },
    );

    let mut body = vec![0u8; 68];
    body[0..4].copy_from_slice(&WINDOW_XID.to_le_bytes());
    body[4..8].copy_from_slice(&PIXMAP_XID.to_le_bytes());
    body[16..20].copy_from_slice(&UPDATE_REGION_XID.to_le_bytes());
    body[20..22].copy_from_slice(&7_i16.to_le_bytes());
    body[22..24].copy_from_slice(&11_i16.to_le_bytes());

    process_request(
        &mut state,
        &mut backend,
        ClientId(CLIENT),
        SequenceNumber(1),
        RequestHeader {
            opcode: 145,
            data: yserver_protocol::x11::present::PIXMAP,
            length_units: u32::try_from(1 + body.len() / 4).unwrap(),
        },
        &body,
        None,
    )
    .unwrap();

    let copies: Vec<_> = backend
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            RecordedCall::CopyArea {
                src_host_xid,
                dst_host_xid,
                src_x,
                src_y,
                dst_x,
                dst_y,
                width,
                height,
            } => Some((
                src_host_xid,
                dst_host_xid,
                src_x,
                src_y,
                dst_x,
                dst_y,
                width,
                height,
            )),
            _ => None,
        })
        .collect();

    assert_eq!(
        copies,
        vec![
            (0x400204, 0x400203, 100, 120, 107, 131, 80, 40),
            (0x400204, 0x400203, 220, 260, 227, 271, 25, 35),
        ],
        "PresentPixmap Copy must copy only the update rects, offset into the destination by x_off/y_off",
    );
}

#[test]
fn present_pixmap_copy_to_redirected_window_preserves_window_destination() {
    use crate::backend::recording::RecordedCall;
    use yserver_protocol::x11::{CreatePixmapRequest, CreateWindowRequest};

    const CLIENT: u32 = 16;
    const WINDOW_XID: u32 = 0x00e0_0303;
    const PIXMAP_XID: u32 = 0x00e0_0304;
    const BACKING_XID: u32 = 0x0040_03ff;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, CLIENT);
    let mut backend = RecordingBackend::new();

    state.resources.create_window(
        ClientId(CLIENT),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(WINDOW_XID),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 640,
            height: 480,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(WINDOW_XID));
    if let Some(w) = state.resources.window_mut(ResourceId(WINDOW_XID)) {
        w.host_xid = crate::backend::WindowHandle::from_raw(0x400303);
        w.redirected_backing = Some(crate::resources::RedirectedBacking {
            host_pixmap: crate::backend::PixmapHandle::from_raw_for_test(BACKING_XID),
            width: 640,
            height: 480,
            depth: 24,
        });
    }

    state.resources.create_pixmap(
        ClientId(CLIENT),
        CreatePixmapRequest {
            depth: 24,
            pixmap: ResourceId(PIXMAP_XID),
            drawable: ResourceId(WINDOW_XID),
            width: 640,
            height: 480,
        },
    );
    let _ = state.resources.set_pixmap_host_xid(
        ResourceId(PIXMAP_XID),
        crate::backend::PixmapHandle::from_raw(0x400304).expect("valid host pixmap"),
    );

    let mut body = vec![0u8; 68];
    body[0..4].copy_from_slice(&WINDOW_XID.to_le_bytes());
    body[4..8].copy_from_slice(&PIXMAP_XID.to_le_bytes());
    body[20..22].copy_from_slice(&13_i16.to_le_bytes());
    body[22..24].copy_from_slice(&17_i16.to_le_bytes());

    process_request(
        &mut state,
        &mut backend,
        ClientId(CLIENT),
        SequenceNumber(1),
        RequestHeader {
            opcode: 145,
            data: yserver_protocol::x11::present::PIXMAP,
            length_units: u32::try_from(1 + body.len() / 4).unwrap(),
        },
        &body,
        None,
    )
    .unwrap();

    let copies: Vec<_> = backend
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            RecordedCall::CopyArea {
                src_host_xid,
                dst_host_xid,
                src_x,
                src_y,
                dst_x,
                dst_y,
                width,
                height,
            } => Some((
                src_host_xid,
                dst_host_xid,
                src_x,
                src_y,
                dst_x,
                dst_y,
                width,
                height,
            )),
            _ => None,
        })
        .collect();

    assert_eq!(
        copies,
        vec![(0x400304, 0x400303, 0, 0, 13, 17, 640, 480)],
        "PresentPixmap must paint through the destination window xid, not its redirected backing xid, so the backend can retain ClipByChildren and hierarchy stacking while resolving that window to the backing",
    );
    let completion_hosts: Vec<u32> = backend
        .enqueued_present_completions
        .iter()
        .map(|(_, host)| *host)
        .collect();
    assert_eq!(
        completion_hosts,
        vec![BACKING_XID],
        "present_completion_keeps_backing_identity: the completion fence attaches to the backing",
    );
}

/// Xorg: the flip check fails on the empty clipList (present_scmd.c:122),
/// the Copy to an unrealized window is a no-op (micopy.c:157), and idle
/// plus CompleteModeCopy are still sent (present_execute.c:137-156).
#[test]
fn present_to_unviewable_window_skips_copy_and_damage_but_completes() {
    use crate::{backend::recording::RecordedCall, resources::MapState, server::DamageObject};

    const PARENT: u32 = 0x0002_0021;
    const WINDOW: u32 = 0x0002_0022;
    const DAMAGE_XID: u32 = 0x0002_0023;
    const PRESENT_ID: u64 = 0x45;
    const TARGET_MSC: u64 = 600;

    for (parent_state, window_state, hidden) in [
        (MapState::Viewable, MapState::Unmapped, true),
        (MapState::Unmapped, MapState::Unviewable, true),
        (MapState::Viewable, MapState::Viewable, false),
    ] {
        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();
        backend.present_direct_result = true;
        for (xid, parent, map_state) in [
            (PARENT, ROOT_WINDOW, parent_state),
            (WINDOW, ResourceId(PARENT), window_state),
        ] {
            state.resources.create_window(
                ClientId(1),
                yserver_protocol::x11::CreateWindowRequest {
                    depth: 24,
                    window: ResourceId(xid),
                    parent,
                    width: 100,
                    height: 100,
                    class: 1,
                    visual: crate::resources::ROOT_VISUAL,
                    ..Default::default()
                },
            );
            state
                .resources
                .window_mut(ResourceId(xid))
                .expect("window")
                .map_state = map_state;
        }
        state.damage_objects.insert(
            DAMAGE_XID,
            DamageObject {
                owner: ClientId(1),
                drawable: ResourceId(WINDOW),
                level: 3,
                rects: Vec::new(),
                pending_notify_fired: false,
                last_reported_geometry: None,
                list_pos: 0,
                created: 0,
            },
        );
        let pending = SupersessionFixture::new(PRESENT_ID, WINDOW)
            .eff(Some(TARGET_MSC))
            .geometry(0, 0, 100, 100)
            .pending();

        execute_present_pixmap_copy(&mut state, &mut backend, pending).expect("present");

        let copies = backend
            .calls()
            .iter()
            .filter(|call| matches!(call, RecordedCall::CopyArea { .. }))
            .count();
        let damaged = !state.damage_objects[&DAMAGE_XID].rects.is_empty();
        let gate = state.present_complete_gate.get(&PRESENT_ID).expect("gate");
        assert_eq!(gate.effective_target_msc, TARGET_MSC);
        if hidden {
            assert!(
                backend.present_direct_candidates.is_empty(),
                "{window_state:?}: no direct attempt"
            );
            assert_eq!(copies, 0, "{window_state:?}: no copy");
            assert!(!damaged, "{window_state:?}: no damage");
            assert_eq!(backend.enqueued_present_completions.len(), 1);
            let (event, _) = &backend.enqueued_present_completions[0];
            assert_eq!(
                event.completion_mode,
                yserver_protocol::x11::present::COMPLETE_MODE_COPY
            );
            assert!(event.emit_idle, "{window_state:?}: idle still sent");
            assert_eq!(event.present_id, PRESENT_ID);
        } else {
            // Viewable control: direct path taken as before.
            assert_eq!(backend.present_direct_candidates.len(), 1);
            assert!(damaged);
            assert!(backend.enqueued_present_completions.is_empty());
        }
    }
}

#[test]
// Mutation: restore the CARDINAL type check or exact-one check; INTEGER/1 and CARDINAL/2 must grant permission.
// Mutation: make the shared peer-PID helper return None on Linux.
fn present_candidate_reports_variable_refresh_property() {
    use crate::backend::recording::RecordingBackend;
    use yserver_protocol::x11::{CreatePixmapRequest, CreateWindowRequest, present as x11present};

    const WINDOW: u32 = 0x0002_1021;
    const WINDOW_HOST: u32 = 0x0040_1021;
    const PIXMAP: u32 = 0x0002_1022;
    const PIXMAP_HOST: u32 = 0x0040_1022;
    const PROPERTY_NAME: &str = "_VARIABLE_REFRESH";
    const CARDINAL: u32 = 6;
    const INTEGER: u32 = 19;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    backend.present_direct_result = true;
    state.randr = crate::randr::RandrState::from_outputs(
        1,
        vec![present_test_output(1, 11, 0, 0, 100, 100, true)],
    );
    state.randr.primary_output = 1;
    state.resources.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(WINDOW),
            parent: ROOT_WINDOW,
            width: 100,
            height: 100,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(WINDOW));
    state
        .resources
        .window_mut(ResourceId(WINDOW))
        .expect("destination window")
        .host_xid = crate::backend::WindowHandle::from_raw(WINDOW_HOST);
    state.resources.create_pixmap(
        ClientId(1),
        CreatePixmapRequest {
            depth: 24,
            pixmap: ResourceId(PIXMAP),
            drawable: ResourceId(WINDOW),
            width: 100,
            height: 100,
        },
    );
    assert!(state.resources.set_pixmap_host_xid(
        ResourceId(PIXMAP),
        crate::backend::PixmapHandle::from_raw(PIXMAP_HOST).expect("test pixmap host xid"),
    ));

    fn present(state: &mut ServerState, backend: &mut RecordingBackend, serial: u32) {
        let mut body = pixmap_body(WINDOW, PIXMAP);
        body[8..12].copy_from_slice(&serial.to_le_bytes());
        process_request(
            state,
            backend,
            ClientId(1),
            SequenceNumber(u16::try_from(serial).unwrap()),
            RequestHeader {
                opcode: 145,
                data: x11present::PIXMAP,
                length_units: u32::try_from(1 + body.len() / 4).unwrap(),
            },
            &body,
            None,
        )
        .expect("Present request");
    }

    fn change_property(
        state: &mut ServerState,
        backend: &mut RecordingBackend,
        property: u32,
        property_type: u32,
        format: u8,
        items: &[u32],
        sequence: u16,
    ) {
        let mut body = vec![0_u8; 20];
        body[0..4].copy_from_slice(&WINDOW.to_le_bytes());
        body[4..8].copy_from_slice(&property.to_le_bytes());
        body[8..12].copy_from_slice(&property_type.to_le_bytes());
        body[12] = format;
        body[16..20].copy_from_slice(&u32::try_from(items.len()).unwrap().to_le_bytes());
        for item in items {
            match format {
                8 => body.push(u8::try_from(*item).unwrap()),
                _ => body.extend_from_slice(&item.to_le_bytes()),
            }
        }
        while !body.len().is_multiple_of(4) {
            body.push(0);
        }
        process_request(
            state,
            backend,
            ClientId(1),
            SequenceNumber(sequence),
            RequestHeader {
                opcode: 18,
                data: 0,
                length_units: u32::try_from(1 + body.len() / 4).unwrap(),
            },
            &body,
            None,
        )
        .expect("ChangeProperty request");
    }

    present(&mut state, &mut backend, 1);
    assert_eq!(
        state.atoms.id_for(PROPERTY_NAME),
        None,
        "Present does not intern the property atom"
    );
    assert_eq!(
        backend
            .present_direct_candidates
            .first()
            .expect("initial Present reaches Backend")
            .client_pid,
        Some(std::process::id()),
        "PresentScanoutCandidate carries the peer PID captured at setup"
    );
    assert!(
        !backend
            .present_direct_candidates
            .first()
            .expect("initial Present reaches Backend")
            .variable_refresh,
        "a missing _VARIABLE_REFRESH property does not grant permission"
    );
    let property = state.atoms.intern(PROPERTY_NAME, false).0;
    for (sequence, property_type, format, values, expected) in [
        (2, CARDINAL, 32, vec![], false),
        (3, CARDINAL, 32, vec![0], false),
        (4, CARDINAL, 32, vec![2], true),
        (5, INTEGER, 32, vec![1], true),
        (6, CARDINAL, 8, vec![1], false),
        (7, CARDINAL, 32, vec![1, 2], false),
        (8, CARDINAL, 32, vec![1], true),
    ] {
        change_property(
            &mut state,
            &mut backend,
            property,
            property_type,
            format,
            &values,
            sequence,
        );
        present(&mut state, &mut backend, u32::from(sequence) + 10);
        assert_eq!(
            backend
                .present_direct_candidates
                .last()
                .expect("Present reaches Backend")
                .variable_refresh,
            expected,
            "type={property_type} format={format} value={values:?}"
        );
    }
    assert_eq!(backend.present_direct_candidates.len(), 8);
}

#[test]
fn present_async_may_tear_bit_is_0x10() {
    // presenttokens.h: PresentOptionAsyncMayTear = (1 << 4).
    assert_eq!(0x10u32, 1u32 << 4);
    // Stripping AsyncMayTear must NOT strip Suboptimal (0x8) or Async (0x1).
    let opts = 0x1 | 0x8 | 0x10;
    assert_eq!(opts & !0x10u32, 0x1 | 0x8);
}
