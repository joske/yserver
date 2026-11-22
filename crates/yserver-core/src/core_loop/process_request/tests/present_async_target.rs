use super::*;

// Async-target fixtures below construct their X resources and Present
// selection through request dispatch, then use the existing run_core drivers
// for Presents, wait readiness, and completion delivery. That keeps these
// tests on the production request and scheduling paths without a host display.
const ASYNC_PRESENT_WINDOW: u32 = 0x0003_1001;
const ASYNC_PRESENT_EVENT: u32 = 0x0031_0001;
const ASYNC_PRESENT_CLOCK: u64 = 100;
const ASYNC_PRESENT_UST: u64 = 0x1000;

fn dispatch_async_present_test_request(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    sequence: u16,
    opcode: u8,
    data: u8,
    body: &[u8],
) {
    process_request(
        state,
        backend,
        ClientId(1),
        SequenceNumber(sequence),
        RequestHeader {
            opcode,
            data,
            length_units: u32::try_from(1 + body.len().div_ceil(4)).unwrap(),
        },
        body,
        None,
    )
    .expect("async Present fixture request");
}

fn async_present_test_fixture() -> (ServerState, UnixStream, RecordingBackend) {
    let mut state = ServerState::new();
    let peer = install_client(&mut state, 1);
    peer.set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .expect("bound core round-trip waits");
    let mut backend = RecordingBackend::new();
    backend.present_ust_msc = (ASYNC_PRESENT_CLOCK, ASYNC_PRESENT_UST);
    seed_present_clock(&mut state, ASYNC_PRESENT_CLOCK, ASYNC_PRESENT_UST);

    let mut create_window = Vec::with_capacity(28);
    create_window.extend_from_slice(&ASYNC_PRESENT_WINDOW.to_le_bytes());
    create_window.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    create_window.extend_from_slice(&0_i16.to_le_bytes());
    create_window.extend_from_slice(&0_i16.to_le_bytes());
    create_window.extend_from_slice(&32_u16.to_le_bytes());
    create_window.extend_from_slice(&32_u16.to_le_bytes());
    create_window.extend_from_slice(&0_u16.to_le_bytes());
    create_window.extend_from_slice(&1_u16.to_le_bytes()); // InputOutput
    create_window.extend_from_slice(&0_u32.to_le_bytes()); // CopyFromParent visual
    create_window.extend_from_slice(&0_u32.to_le_bytes()); // no attributes
    dispatch_async_present_test_request(&mut state, &mut backend, 1, 1, 24, &create_window);

    dispatch_async_present_test_request(
        &mut state,
        &mut backend,
        2,
        8,
        0,
        &ASYNC_PRESENT_WINDOW.to_le_bytes(),
    );

    let mut select_input = Vec::with_capacity(12);
    select_input.extend_from_slice(&ASYNC_PRESENT_EVENT.to_le_bytes());
    select_input.extend_from_slice(&ASYNC_PRESENT_WINDOW.to_le_bytes());
    select_input.extend_from_slice(
        &yserver_protocol::x11::present::EVENT_MASK_COMPLETE_NOTIFY.to_le_bytes(),
    );
    dispatch_async_present_test_request(
        &mut state,
        &mut backend,
        3,
        145,
        yserver_protocol::x11::present::SELECT_INPUT,
        &select_input,
    );

    create_async_present_test_pixmap(&mut state, &mut backend, 4, 0x0003_1010);
    (state, peer, backend)
}

fn create_async_present_test_pixmap(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    sequence: u16,
    pixmap: u32,
) {
    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&pixmap.to_le_bytes());
    body.extend_from_slice(&ASYNC_PRESENT_WINDOW.to_le_bytes());
    body.extend_from_slice(&32_u16.to_le_bytes());
    body.extend_from_slice(&32_u16.to_le_bytes());
    dispatch_async_present_test_request(state, backend, sequence, 53, 24, &body);
}

fn async_present_pixmap_body(
    pixmap: u32,
    serial: u32,
    target_msc: u64,
    options: u32,
    wait_fence: u32,
) -> Vec<u8> {
    let mut body = pixmap_body(ASYNC_PRESENT_WINDOW, pixmap);
    body[8..12].copy_from_slice(&serial.to_le_bytes());
    body[32..36].copy_from_slice(&wait_fence.to_le_bytes());
    body[36..40].copy_from_slice(&options.to_le_bytes());
    body[44..52].copy_from_slice(&target_msc.to_le_bytes());
    body
}

fn run_async_present_request(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    peer: &mut UnixStream,
    sequence: u16,
    pixmap: u32,
    serial: u32,
    target_msc: u64,
    options: u32,
) -> Vec<u8> {
    let body = async_present_pixmap_body(pixmap, serial, target_msc, options, 0);
    run_core_present_request_and_wait(
        state,
        backend,
        peer,
        sequence,
        sequence.wrapping_add(1),
        yserver_protocol::x11::present::PIXMAP,
        &body,
    )
}

fn drain_recorded_async_present_completions(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    peer: &mut UnixStream,
    sequence: u16,
    clock_msc: u64,
) -> Vec<u8> {
    let recorded = backend
        .enqueued_present_completions
        .iter()
        .map(|(event, _)| event.clone())
        .collect::<Vec<_>>();
    backend.completed_present_events_to_drain.extend(recorded);
    backend.present_ust_msc = (clock_msc, ASYNC_PRESENT_UST + clock_msc);
    run_core_present_wait_drain(state, backend, peer, sequence)
}

fn assert_async_present_end_state(state: &ServerState, backend: &RecordingBackend) {
    assert!(
        state.present_pending_exec.is_empty(),
        "pending execution entries"
    );
    assert!(
        state.present_pending_complete.is_empty(),
        "pending completion entries"
    );
    assert!(state.present_complete_gate.is_empty(), "completion gates");
    assert!(state.present_wait_to_id.is_empty(), "source wait index");
    assert!(
        backend.completed_present_events_to_drain.is_empty(),
        "backend-held completions were drained"
    );
    assert!(
        backend.ready_present_source_waits.is_empty(),
        "backend-held source readiness was drained"
    );
    assert_eq!(
        backend.pinned_present_sources.len(),
        backend.released_present_sources.len(),
        "every Present source pin was released"
    );
}

#[test]
fn async_next_target_present_is_synced() {
    use crate::present_scheduler::PRESENT_OPTION_ASYNC;

    let (mut state, mut peer, mut backend) = async_present_test_fixture();
    backend.present_direct_result = false;
    let wire = run_async_present_request(
        &mut state,
        &mut backend,
        &mut peer,
        10,
        0x0003_1010,
        10,
        ASYNC_PRESENT_CLOCK + 1,
        PRESENT_OPTION_ASYNC,
    );

    assert_eq!(backend.present_direct_candidates.len(), 1);
    assert_eq!(
        backend.present_direct_candidates[0].options & PRESENT_OPTION_ASYNC,
        0,
        "a next-target Async request reaches scanout without Async"
    );
    assert_eq!(backend.enqueued_present_completions.len(), 1);
    assert_eq!(
        backend.enqueued_present_completions[0].0.options & PRESENT_OPTION_ASYNC,
        PRESENT_OPTION_ASYNC,
        "the Copy completion retains the request's original options"
    );
    assert!(wire.is_empty(), "completion awaits its target MSC");

    let completion_wire = drain_recorded_async_present_completions(
        &mut state,
        &mut backend,
        &mut peer,
        12,
        ASYNC_PRESENT_CLOCK + 1,
    );
    assert_eq!(
        complete_notify_modes(&completion_wire),
        [yserver_protocol::x11::present::COMPLETE_MODE_COPY]
    );
    assert_async_present_end_state(&state, &backend);
}

#[test]
fn async_current_target_keeps_async() {
    use crate::{backend::PresentSourceWait, present_scheduler::PRESENT_OPTION_ASYNC};

    let (mut state, mut peer, mut backend) = async_present_test_fixture();
    backend.present_source_wait = PresentSourceWait::Deferred(0x3100);
    backend.present_display_idle = false;
    backend.present_absolute_vblank_arm_supported = true;
    let wire = run_core_present_request_and_wait(
        &mut state,
        &mut backend,
        &mut peer,
        10,
        11,
        yserver_protocol::x11::present::PIXMAP,
        &async_present_pixmap_body(
            0x0003_1010,
            10,
            ASYNC_PRESENT_CLOCK,
            PRESENT_OPTION_ASYNC,
            0,
        ),
    );
    assert!(wire.is_empty());
    assert!(backend.present_direct_candidates.is_empty());
    assert!(
        state.present_pending_exec.values().any(|entry| {
            !entry.source_ready
                && entry.wait_id == Some(0x3100)
                && !entry.pending.future_target_sync
        }),
        "the producer wait keeps the request pending until readiness"
    );

    backend.ready_present_source_waits.push(0x3100);
    let ready_wire = run_core_present_wait_drain(&mut state, &mut backend, &mut peer, 12);
    assert!(ready_wire.is_empty());
    assert_eq!(backend.present_direct_candidates.len(), 1);
    assert_eq!(
        backend.present_direct_candidates[0].options & PRESENT_OPTION_ASYNC,
        PRESENT_OPTION_ASYNC,
        "a target that was current at arrival keeps Async"
    );
    assert_eq!(
        backend.enqueued_present_completions[0].0.options & PRESENT_OPTION_ASYNC,
        PRESENT_OPTION_ASYNC
    );
    let completion_wire = drain_recorded_async_present_completions(
        &mut state,
        &mut backend,
        &mut peer,
        13,
        ASYNC_PRESENT_CLOCK,
    );
    assert_eq!(
        complete_notify_modes(&completion_wire),
        [yserver_protocol::x11::present::COMPLETE_MODE_COPY]
    );
    assert_async_present_end_state(&state, &backend);
}

#[test]
fn async_late_after_on_time_keeps_async() {
    use crate::present_scheduler::PRESENT_OPTION_ASYNC;

    let (mut state, mut peer, mut backend) = async_present_test_fixture();
    backend.present_direct_result = false;
    create_async_present_test_pixmap(&mut state, &mut backend, 5, 0x0003_1011);
    let first_wire = run_async_present_request(
        &mut state,
        &mut backend,
        &mut peer,
        10,
        0x0003_1010,
        10,
        ASYNC_PRESENT_CLOCK + 1,
        PRESENT_OPTION_ASYNC,
    );
    assert!(first_wire.is_empty());

    let request_crtc = backend.present_direct_candidates[0].crtc_id;
    backend.present_ust_msc_by_crtc.insert(
        request_crtc,
        (ASYNC_PRESENT_CLOCK + 1, ASYNC_PRESENT_UST + 1),
    );
    let _ = run_core_present_wait_drain(&mut state, &mut backend, &mut peer, 12);
    let late_wire = run_async_present_request(
        &mut state,
        &mut backend,
        &mut peer,
        13,
        0x0003_1011,
        11,
        ASYNC_PRESENT_CLOCK,
        PRESENT_OPTION_ASYNC,
    );
    assert!(late_wire.is_empty());

    assert_eq!(backend.present_direct_candidates.len(), 2);
    assert_eq!(
        backend.present_direct_candidates[0].options & PRESENT_OPTION_ASYNC,
        0,
        "the on-time next-target request is synced"
    );
    assert_eq!(
        backend.present_direct_candidates[1].options & PRESENT_OPTION_ASYNC,
        PRESENT_OPTION_ASYNC,
        "a target at or before the advanced arrival clock stays Async"
    );
    assert_eq!(backend.enqueued_present_completions.len(), 2);
    assert!(
        backend
            .enqueued_present_completions
            .iter()
            .all(|(event, _)| event.options & PRESENT_OPTION_ASYNC != 0)
    );
    let completion_wire = drain_recorded_async_present_completions(
        &mut state,
        &mut backend,
        &mut peer,
        15,
        ASYNC_PRESENT_CLOCK + 1,
    );
    assert_eq!(
        complete_notify_modes(&completion_wire),
        [
            yserver_protocol::x11::present::COMPLETE_MODE_COPY,
            yserver_protocol::x11::present::COMPLETE_MODE_COPY
        ]
    );
    assert_async_present_end_state(&state, &backend);
}

#[test]
fn async_future_target_fence_deferred_past_target_stays_synced() {
    use crate::{backend::PresentSourceWait, present_scheduler::PRESENT_OPTION_ASYNC};

    const WAIT_ID: u64 = 0x3200;
    let (mut state, mut peer, mut backend) = async_present_test_fixture();
    backend.present_source_wait = PresentSourceWait::Deferred(WAIT_ID);
    backend.present_display_idle = false;
    backend.present_absolute_vblank_arm_supported = true;
    let wire = run_core_present_request_and_wait(
        &mut state,
        &mut backend,
        &mut peer,
        10,
        11,
        yserver_protocol::x11::present::PIXMAP,
        &async_present_pixmap_body(
            0x0003_1010,
            10,
            ASYNC_PRESENT_CLOCK + 1,
            PRESENT_OPTION_ASYNC,
            0,
        ),
    );
    assert!(wire.is_empty());
    assert!(backend.present_direct_candidates.is_empty());
    assert!(state.present_pending_exec.values().any(|entry| {
        !entry.source_ready && entry.wait_id == Some(WAIT_ID) && entry.pending.future_target_sync
    }));

    let request_crtc = state
        .present_pending_exec
        .values()
        .next()
        .expect("source wait is pending")
        .pending
        .crtc_id;
    backend.present_ust_msc_by_crtc.insert(
        request_crtc,
        (ASYNC_PRESENT_CLOCK + 2, ASYNC_PRESENT_UST + 2),
    );
    backend.ready_present_source_waits.push(WAIT_ID);
    let ready_wire = run_core_present_wait_drain(&mut state, &mut backend, &mut peer, 12);
    assert!(ready_wire.is_empty());
    assert_eq!(backend.present_direct_candidates.len(), 1);
    assert_eq!(
        backend.present_direct_candidates[0].options & PRESENT_OPTION_ASYNC,
        0,
        "arrival-time synced decision survives producer readiness after target"
    );
    assert_eq!(backend.finished_present_source_waits, [WAIT_ID]);

    let completion_wire = drain_recorded_async_present_completions(
        &mut state,
        &mut backend,
        &mut peer,
        13,
        ASYNC_PRESENT_CLOCK + 2,
    );
    assert_eq!(
        complete_notify_modes(&completion_wire),
        [yserver_protocol::x11::present::COMPLETE_MODE_COPY]
    );
    assert_async_present_end_state(&state, &backend);
}

#[test]
fn async_synced_variant_acquire_deferred_stays_synced() {
    use crate::{backend::PresentSourceWait, present_scheduler::PRESENT_OPTION_ASYNC};

    const ACQUIRE: u32 = 0x0003_1020;
    const RELEASE: u32 = 0x0003_1021;
    const WAIT_ID: u64 = 0x3300;
    let (mut state, mut peer, mut backend) = async_present_test_fixture();
    backend.seed_dri3_syncobj_for_test(ACQUIRE, ClientId(1));
    backend.seed_dri3_syncobj_for_test(RELEASE, ClientId(1));
    backend.present_syncobj_wait = PresentSourceWait::Deferred(WAIT_ID);
    backend.present_display_idle = false;
    backend.present_absolute_vblank_arm_supported = true;

    let mut body = pixmap_synced_body(ASYNC_PRESENT_WINDOW, 0x0003_1010, ACQUIRE, RELEASE, 1, 2);
    body[8..12].copy_from_slice(&10_u32.to_le_bytes());
    body[52..56].copy_from_slice(&PRESENT_OPTION_ASYNC.to_le_bytes());
    body[60..68].copy_from_slice(&(ASYNC_PRESENT_CLOCK + 1).to_le_bytes());
    let wire = run_core_present_request_and_wait(
        &mut state,
        &mut backend,
        &mut peer,
        10,
        11,
        yserver_protocol::x11::present::PIXMAP_SYNCED,
        &body,
    );
    assert!(wire.is_empty());
    assert!(backend.present_direct_candidates.is_empty());
    assert!(state.present_pending_exec.values().any(|entry| {
        !entry.source_ready && entry.wait_id == Some(WAIT_ID) && entry.pending.future_target_sync
    }));

    let request_crtc = state
        .present_pending_exec
        .values()
        .next()
        .expect("acquire wait is pending")
        .pending
        .crtc_id;
    backend.present_ust_msc_by_crtc.insert(
        request_crtc,
        (ASYNC_PRESENT_CLOCK + 2, ASYNC_PRESENT_UST + 2),
    );
    backend.ready_present_source_waits.push(WAIT_ID);
    let ready_wire = run_core_present_wait_drain(&mut state, &mut backend, &mut peer, 12);
    assert!(ready_wire.is_empty());
    assert_eq!(backend.present_direct_candidates.len(), 1);
    assert_eq!(
        backend.present_direct_candidates[0].options & PRESENT_OPTION_ASYNC,
        0,
        "PresentPixmapSynced retains its arrival-time synced decision"
    );
    assert!(backend.present_direct_candidates[0].explicit_sync);
    assert_eq!(backend.finished_present_source_waits, [WAIT_ID]);

    let completion_wire = drain_recorded_async_present_completions(
        &mut state,
        &mut backend,
        &mut peer,
        13,
        ASYNC_PRESENT_CLOCK + 2,
    );
    assert_eq!(
        complete_notify_modes(&completion_wire),
        [yserver_protocol::x11::present::COMPLETE_MODE_COPY]
    );
    assert_async_present_end_state(&state, &backend);
}

#[test]
fn async_distant_target_idle_fallback_is_synced() {
    use crate::present_scheduler::PRESENT_OPTION_ASYNC;

    let (mut state, mut peer, mut backend) = async_present_test_fixture();
    backend.present_direct_result = false;
    backend.present_absolute_vblank_arm_supported = false;
    backend.present_display_idle = true;
    let wire = run_async_present_request(
        &mut state,
        &mut backend,
        &mut peer,
        10,
        0x0003_1010,
        10,
        ASYNC_PRESENT_CLOCK + 20,
        PRESENT_OPTION_ASYNC,
    );
    assert!(wire.is_empty());
    assert_eq!(backend.present_direct_candidates.len(), 1);
    assert_eq!(
        backend.present_direct_candidates[0].options & PRESENT_OPTION_ASYNC,
        0,
        "idle-display fallback executes a future-target request as synced"
    );
    assert_eq!(backend.enqueued_present_completions.len(), 1);

    let completion_wire = drain_recorded_async_present_completions(
        &mut state,
        &mut backend,
        &mut peer,
        12,
        ASYNC_PRESENT_CLOCK + 20,
    );
    assert_eq!(
        complete_notify_modes(&completion_wire),
        [yserver_protocol::x11::present::COMPLETE_MODE_COPY]
    );
    assert_async_present_end_state(&state, &backend);
}

#[test]
fn async_distant_target_failed_arm_is_synced() {
    use crate::present_scheduler::PRESENT_OPTION_ASYNC;

    for result in [Some(Ok(false)), Some(Err(std::io::ErrorKind::Other))] {
        let (mut state, mut peer, mut backend) = async_present_test_fixture();
        backend.present_direct_result = false;
        backend.present_absolute_vblank_arm_supported = true;
        backend.present_display_idle = false;
        backend.arm_present_shared_wake_result = result;
        let wire = run_async_present_request(
            &mut state,
            &mut backend,
            &mut peer,
            10,
            0x0003_1010,
            10,
            ASYNC_PRESENT_CLOCK + 20,
            PRESENT_OPTION_ASYNC,
        );
        assert!(wire.is_empty());
        assert!(
            !backend.armed_shared_wake_crtcs.is_empty(),
            "the distant entry reaches the shared-wake arm for {result:?}"
        );
        assert_eq!(backend.present_direct_candidates.len(), 1);
        assert_eq!(
            backend.present_direct_candidates[0].options & PRESENT_OPTION_ASYNC,
            0,
            "failed arm executes the parked request as synced ({result:?})"
        );
        assert_eq!(backend.enqueued_present_completions.len(), 1);

        let completion_wire = drain_recorded_async_present_completions(
            &mut state,
            &mut backend,
            &mut peer,
            12,
            ASYNC_PRESENT_CLOCK + 20,
        );
        assert_eq!(
            complete_notify_modes(&completion_wire),
            [yserver_protocol::x11::present::COMPLETE_MODE_COPY]
        );
        assert_async_present_end_state(&state, &backend);
    }
}

#[test]
fn synced_present_unchanged() {
    use crate::backend::PresentSourceWait;

    const WAIT_ID: u64 = 0x3400;
    let (mut state, mut peer, mut backend) = async_present_test_fixture();
    backend.present_source_wait = PresentSourceWait::Deferred(WAIT_ID);
    backend.present_display_idle = false;
    backend.present_absolute_vblank_arm_supported = true;
    let wire = run_core_present_request_and_wait(
        &mut state,
        &mut backend,
        &mut peer,
        10,
        11,
        yserver_protocol::x11::present::PIXMAP,
        &async_present_pixmap_body(0x0003_1010, 10, ASYNC_PRESENT_CLOCK + 1, 0, 0),
    );
    assert!(wire.is_empty());
    assert!(state.present_pending_exec.values().any(|entry| {
        !entry.source_ready && entry.wait_id == Some(WAIT_ID) && !entry.pending.future_target_sync
    }));
    backend.ready_present_source_waits.push(WAIT_ID);
    let ready_wire = run_core_present_wait_drain(&mut state, &mut backend, &mut peer, 12);
    assert!(ready_wire.is_empty());
    assert_eq!(backend.present_direct_candidates.len(), 1);
    assert_eq!(backend.present_direct_candidates[0].options, 0);
    assert_eq!(backend.enqueued_present_completions[0].0.options, 0);

    let completion_wire = drain_recorded_async_present_completions(
        &mut state,
        &mut backend,
        &mut peer,
        13,
        ASYNC_PRESENT_CLOCK + 1,
    );
    assert_eq!(
        complete_notify_modes(&completion_wire),
        [yserver_protocol::x11::present::COMPLETE_MODE_COPY]
    );
    assert_async_present_end_state(&state, &backend);
}

#[test]
fn async_same_target_supersession_unchanged() {
    use crate::present_scheduler::PRESENT_OPTION_ASYNC;

    let (mut state, mut peer, mut backend) = async_present_test_fixture();
    backend.present_direct_result = false;
    backend.present_absolute_vblank_arm_supported = true;
    backend.present_display_idle = false;
    backend.present_flip_in_flight = true;
    backend.arm_present_shared_wake_result = Some(Ok(true));
    create_async_present_test_pixmap(&mut state, &mut backend, 5, 0x0003_1011);

    let first_wire = run_async_present_request(
        &mut state,
        &mut backend,
        &mut peer,
        10,
        0x0003_1010,
        10,
        ASYNC_PRESENT_CLOCK + 1,
        PRESENT_OPTION_ASYNC,
    );
    assert!(first_wire.is_empty());
    assert_eq!(state.present_pending_exec.len(), 1);
    assert!(
        state
            .present_pending_exec
            .values()
            .next()
            .unwrap()
            .source_ready
    );

    let second_wire = run_async_present_request(
        &mut state,
        &mut backend,
        &mut peer,
        12,
        0x0003_1011,
        11,
        ASYNC_PRESENT_CLOCK + 1,
        PRESENT_OPTION_ASYNC,
    );
    assert!(second_wire.is_empty());
    assert_eq!(state.present_pending_exec.len(), 1);
    assert!(
        state
            .present_pending_exec
            .values()
            .next()
            .unwrap()
            .source_ready
    );
    assert_eq!(state.present_pending_complete.len(), 1);
    assert_eq!(
        state.present_pending_complete[0].mode,
        yserver_protocol::x11::present::COMPLETE_MODE_SKIP,
        "today's supersession queues the predecessor Skip"
    );
    assert_eq!(state.present_pending_complete[0].event.present_id, 1);
    assert!(
        state
            .present_pending_exec
            .values()
            .next()
            .unwrap()
            .pending
            .present_id
            > 1,
        "the successor remains parked behind the in-flight flip"
    );
    assert!(complete_notify_modes(&read_all_available(&mut peer)).is_empty());

    let request_crtc = state
        .present_pending_exec
        .values()
        .next()
        .expect("successor remains parked")
        .pending
        .crtc_id;
    backend.present_ust_msc_by_crtc.insert(
        request_crtc,
        (ASYNC_PRESENT_CLOCK + 1, ASYNC_PRESENT_UST + 1),
    );
    backend.present_flip_in_flight = false;
    let skip_wire = run_core_present_wait_drain(&mut state, &mut backend, &mut peer, 14);
    assert_eq!(backend.present_direct_candidates.len(), 1);
    assert_eq!(
        backend.present_direct_candidates[0].options & PRESENT_OPTION_ASYNC,
        0,
        "the future-target successor remains synced after its deferral"
    );
    assert_eq!(
        complete_notify_modes(&skip_wire),
        [yserver_protocol::x11::present::COMPLETE_MODE_SKIP],
        "the queued Skip is delivered before the successor's Copy completion"
    );
    assert_eq!(backend.enqueued_present_completions.len(), 1);

    let copy_wire = drain_recorded_async_present_completions(
        &mut state,
        &mut backend,
        &mut peer,
        15,
        ASYNC_PRESENT_CLOCK + 1,
    );
    assert_eq!(
        complete_notify_modes(&copy_wire),
        [yserver_protocol::x11::present::COMPLETE_MODE_COPY]
    );
    assert_async_present_end_state(&state, &backend);
}

#[test]
fn async_future_target_pending_survives_disconnect() {
    use crate::present_scheduler::PRESENT_OPTION_ASYNC;

    let (mut state, mut peer, mut backend) = async_present_test_fixture();
    backend.present_absolute_vblank_arm_supported = true;
    backend.present_display_idle = false;
    backend.arm_present_shared_wake_result = Some(Ok(true));
    let wire = run_async_present_request(
        &mut state,
        &mut backend,
        &mut peer,
        10,
        0x0003_1010,
        10,
        ASYNC_PRESENT_CLOCK + 20,
        PRESENT_OPTION_ASYNC,
    );
    assert!(wire.is_empty());
    let entry = state
        .present_pending_exec
        .values()
        .next()
        .expect("future-target Present remains parked");
    assert!(entry.source_ready);
    let pin = entry.pin.expect("parked Present source is pinned");
    assert_eq!(backend.pinned_present_sources.len(), 1);

    crate::core_loop::process_disconnect::process_disconnect(&mut state, &mut backend, ClientId(1));
    assert!(state.present_pending_exec.is_empty());
    assert!(state.present_event_selections.is_empty());
    assert_eq!(backend.released_present_sources, [pin]);
    assert_async_present_end_state(&state, &backend);
}
