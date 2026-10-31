//! Present wake sources on KMS devices without `CRTC_QUEUE_SEQUENCE`.

use super::*;
use std::{
    cell::Cell,
    os::fd::{AsFd as _, AsRawFd as _},
    rc::Rc,
    time::{Duration, Instant},
};
use yserver_core::backend::PresentClockSource;

/// RANDR CRTC bound to the second, never-flipping output.
const SECOND_CRTC_XID: u32 = 0x5400;

fn recording_source(
    b: &KmsBackend,
) -> Rc<crate::kms::render::platform::flip_events::RecordingPageFlipEventSource> {
    b.platform.devices[0]
        .test_page_flip_event_source
        .clone()
        .expect("fixture records page-flip events")
}

/// Two outputs on one device; Present bound to output 1 which never flips.
/// Output 1's MSC stands at 5 with a completion sample at the same MSC.
fn two_output_fixture() -> (KmsBackend, CrtcKey, Rc<Cell<Instant>>) {
    let mut b = KmsBackend::for_tests();
    push_test_output(&mut b, 2);
    b.pstate.direct.sync_output_count(b.platform.outputs.len());
    let output_key = b.platform.outputs[1].key.clone();
    b.pstate.crtc_key_by_id.insert(SECOND_CRTC_XID, output_key);
    b.refresh_present_crtc_clock_epochs();
    b.scene.scene_structure_dirty = false;
    let second = CrtcKey::for_output(&b.platform.outputs[1]);
    b.platform.vblank_fallback.pipes.insert(second, 1);
    b.platform.record_vblank_clock(second, 5, 1_000);
    b.platform.record_completion_clock(
        second,
        yserver_core::backend::PresentClockSample {
            msc: 5,
            ust: 1_000,
            source: PresentClockSource::PageFlip,
        },
        None,
    );
    let now = Rc::new(Cell::new(Instant::now()));
    let clock = Rc::clone(&now);
    b.for_tests_set_monotonic_clock(Rc::new(move || clock.get()));
    (b, second, now)
}

fn drain_kernel_events(b: &mut KmsBackend) {
    let fd = b.platform.devices[0].device.as_fd().as_raw_fd();
    Backend::on_page_flip_ready(b, &mut yserver_core::server::ServerState::new(), fd);
}

fn arm_recorded_kernel_wake(
    b: &mut KmsBackend,
    second: CrtcKey,
) -> Rc<crate::kms::render::platform::flip_events::RecordingPageFlipEventSource> {
    let source = recording_source(b);
    source.set_legacy_vblank(Ok(5));
    b.pstate
        .crtc_queue_sequence_unsupported_devices
        .insert(second.device_key);
    assert!(Backend::arm_present_shared_wake(b, SECOND_CRTC_XID).expect("kernel wake arm"));
    source
}

fn mark_frozen_at_watch_deadline(
    b: &mut KmsBackend,
    second: CrtcKey,
    now: &Cell<Instant>,
) -> crate::kms::render::platform::vblank_fallback::FrozenClock {
    arm_recorded_kernel_wake(b, second);
    let armed_at = now.get();
    assert!(
        b.next_wakeup()
            .is_some_and(|wake| { wake <= armed_at + KmsBackend::FROZEN_VBLANK_WINDOW })
    );
    now.set(armed_at + KmsBackend::FROZEN_VBLANK_WINDOW);
    b.poll_deferred_input(&mut yserver_core::server::ServerState::new());
    b.platform.vblank_fallback.frozen[&second]
}

#[derive(Clone, Copy)]
struct CorePresentRequest {
    client_id: u32,
    serial: u32,
    x_offset: i16,
    target_msc: u64,
    divisor: u32,
    remainder: u32,
    target_output_idx: usize,
    options: u32,
}

type FrozenCorePresentResult = Option<(
    Vec<(u32, u8, u64, u64)>,
    crate::kms::render::platform::vblank_fallback::FrozenClock,
    Instant,
)>;

#[test]
// Mutation: return the anchor sample instead of elapsed frozen-clock frames.
fn frozen_clock_catches_up_after_a_stall() {
    let (mut b, second, now) = two_output_fixture();
    arm_recorded_kernel_wake(&mut b, second);
    let armed_at = now.get();

    let deadline = b.next_wakeup().expect("the frozen watch wakes the loop");
    assert!(deadline <= armed_at + KmsBackend::FROZEN_VBLANK_WINDOW);
    now.set(armed_at + KmsBackend::FROZEN_VBLANK_WINDOW);
    b.poll_deferred_input(&mut yserver_core::server::ServerState::new());

    let frozen = b
        .platform
        .vblank_fallback
        .frozen
        .get(&second)
        .copied()
        .expect("the silent kernel arm marks its CRTC frozen");
    let marked_at = now.get();
    assert!(
        Backend::arm_present_shared_wake(&mut b, SECOND_CRTC_XID)
            .expect("frozen shared wake uses software")
    );
    now.set(marked_at + frozen.interval * 10);
    b.poll_deferred_input(&mut yserver_core::server::ServerState::new());
    let sample = Backend::present_get_ust_msc(&b, SECOND_CRTC_XID);
    assert_eq!(sample.0, frozen.anchor_msc + 10);
    assert_eq!(
        sample.1,
        frozen.anchor_ust + u64::try_from(frozen.interval.as_nanos() * 10 / 1_000).unwrap()
    );
    assert_eq!(
        b.present_get_completion_clock(SECOND_CRTC_XID).msc,
        frozen.anchor_msc + 10
    );
}

#[test]
// Mutations: return the stored clock sample or omit divisor/remainder from the new target.
fn frozen_clock_reads_are_current_after_inactivity() {
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        io::Read,
        sync::{Arc, Mutex, atomic::AtomicU16},
    };
    use yserver_core::{
        backend::{Backend, WindowHandle},
        core_loop::process_request::process_request,
        server::{BackendCapabilities, ClientState, PresentEventSelection, ServerState},
        transport::Transport,
    };
    use yserver_protocol::x11::{
        ClientId, CreatePixmapRequest, RequestHeader, ResourceId, SequenceNumber, present,
    };

    const WINDOW: ResourceId = yserver_core::resources::ROOT_WINDOW;
    const PIXMAP: ResourceId = ResourceId(0x0100_7d81);
    const EVENT_ID: u32 = 0x0100_7efe;
    let (mut b, second, now) = two_output_fixture();
    let frozen = mark_frozen_at_watch_deadline(&mut b, second, &now);
    let (mut outputs, modes) = Backend::randr_outputs_and_modes(&mut b);
    outputs[1].crtc_id = SECOND_CRTC_XID;
    b.pstate
        .crtc_key_by_id
        .insert(SECOND_CRTC_XID, b.platform.outputs[1].key.clone());
    let second_output_id = outputs[1].output_id;
    let mut state = ServerState::with_randr_outputs_and_modes(
        b.platform.fb_w,
        b.platform.fb_h,
        outputs,
        modes,
        BackendCapabilities::from_backend(&b),
    );
    state.randr.primary_output = second_output_id;
    state.resources.window_mut(WINDOW).unwrap().host_xid =
        WindowHandle::from_raw(Backend::window_id(&b));
    let (fb_w, fb_h) = (b.platform.fb_w, b.platform.fb_h);
    let pixmap_host =
        Backend::create_pixmap(&mut b, None, 24, fb_w, fb_h).expect("Present source pixmap");
    state.resources.create_pixmap(
        ClientId(1),
        CreatePixmapRequest {
            depth: 24,
            pixmap: PIXMAP,
            drawable: WINDOW,
            width: fb_w,
            height: fb_h,
        },
    );
    assert!(state.resources.set_pixmap_host_xid(PIXMAP, pixmap_host));

    let (transport, mut peer) = Transport::pair().expect("core test client transport");
    peer.set_read_timeout(Some(Duration::from_secs(2)))
        .expect("test client read timeout");
    state.clients.insert(
        1,
        ClientState {
            writer: Arc::new(Mutex::new(transport)),
            is_local: true,
            fd_passing: true,
            internal: false,
            peer: yserver_core::peer::PeerIdentity::unknown_unix(),
            byte_order: yserver_protocol::x11::ClientByteOrder::LittleEndian,
            last_sequence: Arc::new(AtomicU16::new(0)),
            resource_id_base: 0,
            resource_id_mask: u32::MAX,
            big_requests_enabled: false,
            event_masks: HashMap::new(),
            save_set: HashSet::new(),
            xi2_masks: HashMap::new(),
            xi1_event_classes: HashSet::new(),
            xi1_window_event_classes: HashMap::new(),
            outbound: VecDeque::new(),
            watching_writable: false,
            write_failed: false,
            gone: false,
            focused_window: WINDOW,
            reader_control: None,
        },
    );
    state.present_event_selections.insert(
        EVENT_ID,
        PresentEventSelection {
            owner: ClientId(1),
            window: WINDOW,
            event_mask: present::EVENT_MASK_COMPLETE_NOTIFY,
        },
    );

    now.set(frozen.anchor_at + Duration::from_secs(2));
    let frames = now.get().duration_since(frozen.anchor_at).as_nanos() / frozen.interval.as_nanos();
    let current_msc = frozen.anchor_msc + u64::try_from(frames).unwrap();
    let current_ust =
        frozen.anchor_ust + u64::try_from(frames * frozen.interval.as_nanos() / 1_000).unwrap();
    assert_eq!(
        Backend::present_get_ust_msc(&b, SECOND_CRTC_XID),
        (current_msc, current_ust)
    );

    let mut notify_body = vec![0_u8; 36];
    notify_body[0..4].copy_from_slice(&WINDOW.0.to_le_bytes());
    notify_body[4..8].copy_from_slice(&501_u32.to_le_bytes());
    process_request(
        &mut state,
        &mut b,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 145,
            data: present::NOTIFY_MSC,
            length_units: 10,
        },
        &notify_body,
        None,
    )
    .expect("Present NotifyMSC after inactivity");
    assert!(
        state.present_pending_msc.is_empty(),
        "NotifyMSC should resolve against the frozen clock: {:#?}",
        state.present_window_msc.get(&WINDOW.0)
    );
    let mut notify = [0_u8; 40];
    peer.read_exact(&mut notify)
        .expect("NotifyMSC CompleteNotify");
    assert_eq!(notify[10], present::COMPLETE_KIND_NOTIFY_MSC);
    assert_eq!(u32::from_le_bytes(notify[20..24].try_into().unwrap()), 501);
    assert_eq!(
        u64::from_le_bytes(notify[24..32].try_into().unwrap()),
        current_ust
    );
    assert_eq!(
        u64::from_le_bytes(notify[32..40].try_into().unwrap()),
        current_msc,
        "NotifyMSC reads the current frozen frame at request time"
    );

    let divisor = 4_u64;
    let remainder = (current_msc + 2) % divisor;
    let mut pixmap_body = vec![0_u8; 68];
    pixmap_body[0..4].copy_from_slice(&WINDOW.0.to_le_bytes());
    pixmap_body[4..8].copy_from_slice(&PIXMAP.0.to_le_bytes());
    pixmap_body[8..12].copy_from_slice(&502_u32.to_le_bytes());
    pixmap_body[24..28].copy_from_slice(&SECOND_CRTC_XID.to_le_bytes());
    pixmap_body[44..52].copy_from_slice(&0_u64.to_le_bytes());
    pixmap_body[52..60].copy_from_slice(&divisor.to_le_bytes());
    pixmap_body[60..68].copy_from_slice(&remainder.to_le_bytes());
    process_request(
        &mut state,
        &mut b,
        ClientId(1),
        SequenceNumber(2),
        RequestHeader {
            opcode: 145,
            data: present::PIXMAP,
            length_units: 18,
        },
        &pixmap_body,
        None,
    )
    .expect("PresentPixmap target after inactivity");
    assert_eq!(
        state.present_window_msc[&WINDOW.0].last_crtc,
        SECOND_CRTC_XID
    );
    let request_target = state
        .present_complete_gate
        .values()
        .next()
        .map(|gate| gate.effective_target_msc)
        .or_else(|| {
            state
                .present_pending_exec
                .values()
                .next()
                .and_then(|pending| pending.pending.effective_target_msc)
        })
        .or_else(|| {
            state
                .present_pending_complete
                .first()
                .map(|pending| pending.effective_target_msc)
        });
    assert_eq!(
        request_target,
        Some(current_msc + 2),
        "new Present applies its divisor from the current frozen frame"
    );
    assert_eq!(
        Backend::present_get_completion_clock(&b, SECOND_CRTC_XID).msc,
        current_msc
    );
}

#[test]
// Mutations: use flip.frame or advance the frozen MSC once per flip.
fn frozen_crtc_flips_do_not_drive_the_clock() {
    let (mut b, second, now) = two_output_fixture();
    let frozen = mark_frozen_at_watch_deadline(&mut b, second, &now);
    let source = recording_source(&b);
    let fd = b.platform.devices[0].device.as_fd().as_raw_fd();
    let mut state = yserver_core::server::ServerState::new();

    for index in 1..=600_u64 {
        now.set(frozen.anchor_at + Duration::from_nanos(1_000_000_000 * index / 600));
        source.queue_flip(second.crtc, u32::try_from(index).unwrap());
        Backend::on_page_flip_ready(&mut b, &mut state, fd);
    }

    let frames = Duration::from_secs(1).as_nanos() / frozen.interval.as_nanos();
    let expected_msc = frozen.anchor_msc + frames as u64;
    let expected_ust = frozen.anchor_ust + (frames * frozen.interval.as_nanos() / 1_000) as u64;
    assert_eq!(
        Backend::present_get_ust_msc(&b, SECOND_CRTC_XID),
        (expected_msc, expected_ust)
    );
    assert_eq!(
        b.platform.ust_msc[&second],
        (expected_msc, expected_ust),
        "each stale flip records the software clock sample"
    );
    assert_eq!(b.platform.completion_clocks[&second].msc, expected_msc);
    assert_eq!(b.platform.completion_clocks[&second].ust, expected_ust);
    assert_eq!(source.queued_flip_count(), 0, "all 600 flip events retired");
}

#[test]
// Mutations: keep the idle gate or suppress frozen ticks while a flip is in flight.
fn frozen_tick_backend_arm_runs_with_a_flip_in_flight() {
    let (mut b, second, now) = two_output_fixture();
    let frozen = mark_frozen_at_watch_deadline(&mut b, second, &now);
    b.scene.test_set_flip_in_flight(true);

    assert!(Backend::present_absolute_vblank_arm_supported(
        &b,
        SECOND_CRTC_XID
    ));
    assert_eq!(
        Backend::arm_present_completion_idle_vblanks(
            &mut b,
            SECOND_CRTC_XID,
            &[frozen.anchor_msc + 1]
        )
        .expect("frozen completion wake remains available during a flip"),
        1
    );
    assert!(b.platform.vblank_fallback.software.contains_key(&second));
    assert!(
        Backend::arm_present_shared_wake(&mut b, SECOND_CRTC_XID)
            .expect("frozen exec wake remains available during a flip")
    );
    let deadline = b
        .next_wakeup()
        .expect("frozen tick schedules the next frame");
    assert!(deadline <= frozen.anchor_at + frozen.interval);
    now.set(frozen.anchor_at + frozen.interval);
    b.poll_deferred_input(&mut yserver_core::server::ServerState::new());

    assert_eq!(
        b.present_get_completion_clock(SECOND_CRTC_XID).msc,
        frozen.anchor_msc + 1
    );
    assert_eq!(
        b.platform.completion_clocks[&second].msc,
        frozen.anchor_msc + 1,
        "frozen software tick records completion time while a flip is in flight"
    );
    assert!(b.scene.has_pending_page_flips(), "the flip stays in flight");
}

#[test]
// Mutation: let a frozen kernel sequence overwrite the software clock.
fn frozen_kernel_sequence_does_not_touch_the_clock() {
    let (mut b, second, now) = two_output_fixture();
    let source = arm_recorded_kernel_wake(&mut b, second);
    let armed_at = now.get();
    now.set(armed_at + KmsBackend::FROZEN_VBLANK_WINDOW);
    b.poll_deferred_input(&mut yserver_core::server::ServerState::new());
    let frozen = b.platform.vblank_fallback.frozen[&second];
    now.set(frozen.anchor_at + frozen.interval * 4);
    let before = b.present_get_ust_msc(SECOND_CRTC_XID);
    let before_general_sample = b.platform.ust_msc[&second];
    let before_completion_sample = b.platform.completion_clocks[&second];
    source.queue_sequence_for_tests(
        crate::kms::render::backend::ABSOLUTE_SEQ_TAG | u64::from(u32::from(second.crtc)),
        99_000_000,
        999,
    );
    drain_kernel_events(&mut b);

    assert_eq!(b.present_get_ust_msc(SECOND_CRTC_XID), before);
    assert_eq!(b.platform.ust_msc[&second], before_general_sample);
    assert_eq!(
        b.platform.completion_clocks[&second],
        before_completion_sample
    );
    assert!(!super::vblank_arm_is_watched(
        &b,
        second,
        super::SHARED_WAKE_VBLANK_ARM_SLOT
    ));
    assert!(
        !b.platform
            .vblank_fallback
            .kernel_arm_watch
            .contains_key(&(second, super::SHARED_WAKE_VBLANK_ARM_SLOT,))
    );
}

#[test]
// Mutation: reset the watch to its original arm time after a firing event.
fn slow_but_firing_arm_is_not_frozen() {
    let (mut b, second, now) = two_output_fixture();
    let source = arm_recorded_kernel_wake(&mut b, second);
    let first_arm = now.get();
    now.set(first_arm + Duration::from_millis(900));
    source.queue_sequence_for_tests(
        crate::kms::render::backend::ABSOLUTE_SEQ_TAG | u64::from(u32::from(second.crtc)),
        1_000,
        6,
    );
    drain_kernel_events(&mut b);
    assert!(
        Backend::arm_present_shared_wake(&mut b, SECOND_CRTC_XID)
            .expect("rearm after a firing event")
    );
    let second_arm = now.get();
    now.set(second_arm + Duration::from_millis(900));
    b.poll_deferred_input(&mut yserver_core::server::ServerState::new());

    assert!(!b.platform.vblank_fallback.frozen.contains_key(&second));
    assert_eq!(
        b.platform.vblank_fallback.kernel_arm_watch[&(second, super::SHARED_WAKE_VBLANK_ARM_SLOT)],
        second_arm
    );
}

#[test]
// Mutation: skip the zero-time events_ready check at the frozen deadline.
fn event_unread_at_deadline_is_not_frozen() {
    let (mut b, second, now) = two_output_fixture();
    let source = arm_recorded_kernel_wake(&mut b, second);
    let armed_at = now.get();
    source.queue_sequence_for_tests(
        crate::kms::render::backend::ABSOLUTE_SEQ_TAG | u64::from(u32::from(second.crtc)),
        2_000,
        6,
    );
    now.set(armed_at + KmsBackend::FROZEN_VBLANK_WINDOW);
    b.poll_deferred_input(&mut yserver_core::server::ServerState::new());

    assert!(!b.platform.vblank_fallback.frozen.contains_key(&second));
    let fd = b.platform.devices[0].device.as_fd().as_raw_fd();
    Backend::on_page_flip_ready(&mut b, &mut yserver_core::server::ServerState::new(), fd);
    assert!(
        !b.platform
            .vblank_fallback
            .kernel_arm_watch
            .contains_key(&(second, super::SHARED_WAKE_VBLANK_ARM_SLOT,))
    );
}

#[test]
// Mutation: create a watch for a reached target that needs no vblank arm.
fn no_arm_means_no_verdict() {
    let (mut b, second, now) = two_output_fixture();
    b.scene.test_set_flip_in_flight(true);
    assert_eq!(
        Backend::arm_present_completion_idle_vblanks(&mut b, SECOND_CRTC_XID, &[5])
            .expect("reached held target needs no completion arm"),
        0
    );
    now.set(now.get() + Duration::from_secs(5));
    b.poll_deferred_input(&mut yserver_core::server::ServerState::new());

    assert!(!b.platform.vblank_fallback.frozen.contains_key(&second));
    assert!(b.platform.vblank_fallback.kernel_arm_watch.is_empty());
}

#[test]
// Mutation: install a kernel watch after the device has fallen back to software.
fn device_on_fallback_has_no_watch() {
    let (mut b, second, now) = two_output_fixture();
    let source = recording_source(&b);
    source.set_legacy_vblank(Err(libc::EOPNOTSUPP));
    b.pstate
        .crtc_queue_sequence_unsupported_devices
        .insert(second.device_key);
    assert_eq!(
        Backend::arm_present_completion_idle_vblanks(&mut b, SECOND_CRTC_XID, &[6])
            .expect("unsupported kernel arm falls back"),
        1
    );
    assert!(b.platform.vblank_fallback.kernel_arm_watch.is_empty());
    now.set(now.get() + Duration::from_secs(5));
    b.poll_deferred_input(&mut yserver_core::server::ServerState::new());
    assert!(!b.platform.vblank_fallback.frozen.contains_key(&second));
}

#[test]
// Mutation: remove the device-qualified CRTC check when a watch expires.
fn freezing_one_crtc_leaves_the_other_on_kernel_clock() {
    let (mut b, second, now) = two_output_fixture();
    let source = arm_recorded_kernel_wake(&mut b, second);
    let outputs = Backend::randr_outputs_and_modes(&mut b).0;
    let first_crtc_id = outputs[0].crtc_id;
    let first = CrtcKey::for_output(&b.platform.outputs[0]);
    b.pstate
        .crtc_key_by_id
        .insert(first_crtc_id, b.platform.outputs[0].key.clone());
    b.platform.vblank_fallback.pipes.insert(first, 0);
    assert!(
        Backend::arm_present_shared_wake(&mut b, first_crtc_id)
            .expect("arm the other output's kernel clock")
    );

    source.queue_sequence_for_tests(
        crate::kms::render::backend::ABSOLUTE_SEQ_TAG | u64::from(u32::from(first.crtc)),
        2_000,
        6,
    );
    now.set(now.get() + Duration::from_millis(900));
    drain_kernel_events(&mut b);
    now.set(now.get() + Duration::from_millis(100));
    b.poll_deferred_input(&mut yserver_core::server::ServerState::new());

    assert_eq!(b.platform.present_get_ust_msc(first).0, 6);
    assert!(!b.platform.vblank_fallback.frozen.contains_key(&first));
    assert!(b.platform.vblank_fallback.frozen.contains_key(&second));
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: omit DPMS-off, VT-away, or own-mode removal from watch cleanup.
fn dpms_off_vt_away_and_own_modeset_end_the_observation() {
    use yserver_core::{
        backend::Backend, core_loop::input_inventory::InputInventory, server::ServerState,
    };

    let (mut dpms, second, now) = two_output_fixture();
    dpms.platform.for_tests_accept_output_power_commits();
    arm_recorded_kernel_wake(&mut dpms, second);
    Backend::set_dpms_power(&mut dpms, 3).expect("DPMS off");
    assert!(dpms.platform.vblank_fallback.kernel_arm_watch.is_empty());
    now.set(now.get() + Duration::from_secs(2));
    dpms.poll_deferred_input(&mut ServerState::new());
    assert!(dpms.platform.vblank_fallback.frozen.is_empty());

    let (mut vt, vt_crtc, vt_now) = two_output_fixture();
    arm_recorded_kernel_wake(&mut vt, vt_crtc);
    Backend::begin_vt_release(&mut vt);
    Backend::finish_vt_release(&mut vt, &mut ServerState::new(), &InputInventory::new());
    assert!(vt.platform.vblank_fallback.kernel_arm_watch.is_empty());
    vt_now.set(vt_now.get() + Duration::from_secs(2));
    vt.poll_deferred_input(&mut ServerState::new());
    assert!(vt.platform.vblank_fallback.frozen.is_empty());

    let Some(mut modeset) = super::tearing::live_scene_backend_with_outputs(2) else {
        println!(
            "SKIP dpms_off_vt_away_and_own_modeset_end_the_observation: Vulkan fixture unavailable"
        );
        return;
    };
    let mode_crtc = CrtcKey::for_output(&modeset.platform.outputs[1]);
    let mode_source = recording_source(&modeset);
    mode_source.set_legacy_vblank(Ok(5));
    modeset
        .pstate
        .crtc_queue_sequence_unsupported_devices
        .insert(mode_crtc.device_key);
    modeset.platform.vblank_fallback.pipes.insert(mode_crtc, 1);
    modeset.platform.for_tests_accept_output_power_commits();
    let output = Backend::randr_outputs_and_modes(&mut modeset).0[1].clone();
    let base = Instant::now();
    let mode_now = Rc::new(Cell::new(base));
    let clock = Rc::clone(&mode_now);
    modeset.for_tests_set_monotonic_clock(Rc::new(move || clock.get()));
    assert!(
        Backend::arm_present_shared_wake(&mut modeset, output.crtc_id)
            .expect("arm watched CRTC before its own modeset")
    );
    mode_now.set(base + KmsBackend::FROZEN_VBLANK_WINDOW);
    modeset.poll_deferred_input(&mut ServerState::new());
    assert!(
        modeset
            .platform
            .vblank_fallback
            .frozen
            .contains_key(&mode_crtc)
    );
    assert!(
        Backend::apply_crtc_config(&mut modeset, output.output_id, &output.name, None, 0, 0,)
            .expect("disable watched CRTC through RandR")
    );
    mode_now.set(mode_now.get() + Duration::from_secs(2));
    modeset.poll_deferred_input(&mut ServerState::new());
    assert!(
        !modeset
            .platform
            .vblank_fallback
            .frozen
            .contains_key(&mode_crtc)
    );
    assert!(
        !modeset
            .platform
            .vblank_fallback
            .kernel_arm_watch
            .keys()
            .any(|(key, _)| *key == mode_crtc)
    );
}

fn run_frozen_core_present_requests(
    requests_to_send: &[CorePresentRequest],
    test_name: &str,
    flip_in_flight: bool,
    frozen_vrefresh: u32,
) -> FrozenCorePresentResult {
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        io::Read,
        sync::{Arc, Mutex, atomic::AtomicU16},
        thread,
    };
    use yserver_core::{
        backend::{Backend, PresentClockSample, PresentClockSource},
        core_loop::{
            CoreSender, Message, ResetPolicy, auth::AuthState, channel,
            poll_tokens::ClientIdAllocator, run_core,
        },
        server::{BackendCapabilities, ClientState, PresentEventSelection, ServerState},
        transport::Transport,
    };
    use yserver_protocol::x11::{
        ClientId, CreatePixmapRequest, RequestHeader, ResourceId, SequenceNumber, present,
    };

    const WINDOW: ResourceId = ResourceId(0x0100_7d80);
    const PIXMAP: ResourceId = ResourceId(0x0100_7d81);
    let Some(mut b) = super::tearing::live_scene_backend_with_outputs(2) else {
        println!("SKIP {test_name}: Vulkan fixture unavailable");
        return None;
    };
    b.platform.outputs[1].output.picked.vrefresh = frozen_vrefresh;
    let second = CrtcKey::for_output(&b.platform.outputs[1]);
    let source = recording_source(&b);
    source.set_legacy_vblank(Ok(0));
    b.platform.vblank_fallback.pipes.insert(second, 1);
    b.pstate
        .crtc_queue_sequence_unsupported_devices
        .insert(second.device_key);
    let (pollable_device_fd, _device_peer) =
        std::os::unix::net::UnixStream::pair().expect("pollable test DRM socket");
    b.platform.devices[0].device = Rc::new(crate::drm::Device::from_inherited_kms_fd(
        std::os::fd::OwnedFd::from(pollable_device_fd),
        "<Present event-space test>",
    ));

    let (outputs, modes) = Backend::randr_outputs_and_modes(&mut b);
    let crtc_ids: Vec<u32> = outputs.iter().map(|output| output.crtc_id).collect();
    let first = CrtcKey::for_output(&b.platform.outputs[0]);
    let mut state = ServerState::with_randr_outputs_and_modes(
        b.platform.fb_w,
        b.platform.fb_h,
        outputs,
        modes,
        BackendCapabilities::from_backend(&b),
    );
    super::tearing::materialize_test_cow_stage(&mut b, &mut state, ClientId(1), WINDOW);
    b.platform.ust_msc.insert(second, (1, 0));
    b.platform.ust_msc.insert(first, (1, 0));
    b.platform.completion_clocks.insert(
        second,
        PresentClockSample {
            msc: 1,
            ust: 0,
            source: PresentClockSource::PageFlip,
        },
    );
    b.platform.completion_clocks.insert(
        first,
        PresentClockSample {
            msc: 1,
            ust: 0,
            source: PresentClockSource::PageFlip,
        },
    );
    let (width, height) = (b.platform.fb_w, b.platform.fb_h);
    let pixmap_host = Backend::create_pixmap(&mut b, None, 24, width, height)
        .expect("create Present source pixmap");
    state.resources.create_pixmap(
        ClientId(1),
        CreatePixmapRequest {
            depth: 24,
            pixmap: PIXMAP,
            drawable: WINDOW,
            width,
            height,
        },
    );
    assert!(state.resources.set_pixmap_host_xid(PIXMAP, pixmap_host));
    state.present_event_selections.insert(
        0x0100_7efe,
        PresentEventSelection {
            owner: ClientId(1),
            window: WINDOW,
            event_mask: present::EVENT_MASK_COMPLETE_NOTIFY,
        },
    );
    let needs_second_client = requests_to_send
        .iter()
        .any(|request| request.client_id == 2);
    assert!(
        requests_to_send
            .iter()
            .all(|request| matches!(request.client_id, 1 | 2)),
        "core-loop fixture registers clients 1 and 2"
    );
    if needs_second_client {
        state.present_event_selections.insert(
            0x0200_7efe,
            PresentEventSelection {
                owner: ClientId(2),
                window: WINDOW,
                event_mask: present::EVENT_MASK_COMPLETE_NOTIFY,
            },
        );
    }

    let (transport, mut peer) = Transport::pair().expect("core test client transport");
    peer.set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set test client read timeout");
    let make_client_state = |transport| ClientState {
        writer: Arc::new(Mutex::new(transport)),
        is_local: true,
        fd_passing: true,
        internal: false,
        peer: yserver_core::peer::PeerIdentity::unknown_unix(),
        byte_order: yserver_protocol::x11::ClientByteOrder::LittleEndian,
        last_sequence: Arc::new(AtomicU16::new(0)),
        resource_id_base: 0,
        resource_id_mask: u32::MAX,
        big_requests_enabled: false,
        event_masks: HashMap::new(),
        save_set: HashSet::new(),
        xi2_masks: HashMap::new(),
        xi1_event_classes: HashSet::new(),
        xi1_window_event_classes: HashMap::new(),
        outbound: VecDeque::new(),
        watching_writable: false,
        write_failed: false,
        gone: false,
        focused_window: yserver_core::resources::ROOT_WINDOW,
        reader_control: None,
    };
    state.clients.insert(1, make_client_state(transport));
    let second_peer = if needs_second_client {
        let (transport, peer) = Transport::pair().expect("second core test client transport");
        peer.set_read_timeout(Some(Duration::from_secs(10)))
            .expect("set second test client read timeout");
        state.clients.insert(2, make_client_state(transport));
        Some(peer)
    } else {
        None
    };
    b.for_tests_set_monotonic_clock(Rc::new(Instant::now));
    b.scene.test_set_flip_in_flight(flip_in_flight);

    let (poll, sender, receiver) = channel().expect("core loop channel");
    let requests = sender.bind();
    let shutdown = sender.clone_handle();
    let request_count = requests_to_send.len();
    let requests_for_driver = requests_to_send.to_vec();
    let crtc_ids_for_driver = crtc_ids;
    let driver = thread::spawn(move || {
        struct ShutdownOnDrop(CoreSender);
        impl Drop for ShutdownOnDrop {
            fn drop(&mut self) {
                let _ = self.0.send(Message::Shutdown);
            }
        }
        let _shutdown_on_drop = ShutdownOnDrop(shutdown);
        let mut second_peer = second_peer;
        let send_present = |sequence: u16, request: CorePresentRequest| {
            let mut body = vec![0_u8; 68];
            body[0..4].copy_from_slice(&WINDOW.0.to_le_bytes());
            body[4..8].copy_from_slice(&PIXMAP.0.to_le_bytes());
            body[8..12].copy_from_slice(&request.serial.to_le_bytes());
            body[20..22].copy_from_slice(&request.x_offset.to_le_bytes());
            body[24..28]
                .copy_from_slice(&crtc_ids_for_driver[request.target_output_idx].to_le_bytes());
            body[36..40].copy_from_slice(&request.options.to_le_bytes());
            body[44..52].copy_from_slice(&request.target_msc.to_le_bytes());
            body[52..60].copy_from_slice(&u64::from(request.divisor).to_le_bytes());
            body[60..68].copy_from_slice(&u64::from(request.remainder).to_le_bytes());
            requests
                .send(Message::Request {
                    id: ClientId(request.client_id),
                    sequence: SequenceNumber(sequence),
                    accepted_at: None,
                    header: RequestHeader {
                        opcode: 145,
                        data: present::PIXMAP,
                        length_units: 18,
                    },
                    body,
                    attached_fd: None,
                })
                .expect("queue PresentPixmap");
        };
        let send_barrier = |sequence: u16| {
            requests
                .send(Message::Request {
                    id: ClientId(1),
                    sequence: SequenceNumber(sequence),
                    accepted_at: None,
                    header: RequestHeader {
                        opcode: 43,
                        data: 0,
                        length_units: 1,
                    },
                    body: Vec::new(),
                    attached_fd: None,
                })
                .expect("queue core-loop barrier");
        };
        let mut completions = Vec::new();
        let mut barrier_replies = 0;
        let read_until = |peer: &mut std::os::unix::net::UnixStream,
                          completions: &mut Vec<(u32, u8, u64, u64)>,
                          barrier_replies: &mut usize,
                          complete_count: usize,
                          barrier_count: usize|
         -> std::io::Result<()> {
            while completions.len() < complete_count || *barrier_replies < barrier_count {
                let mut first = [0_u8; 1];
                peer.read_exact(&mut first)?;
                match first[0] {
                    35 => {
                        let mut event = [0_u8; 40];
                        event[0] = first[0];
                        peer.read_exact(&mut event[1..])?;
                        assert_eq!(
                            u16::from_le_bytes([event[8], event[9]]),
                            u16::from(present::EVENT_COMPLETE_NOTIFY)
                        );
                        completions.push((
                            u32::from_le_bytes(event[20..24].try_into().unwrap()),
                            event[11],
                            u64::from_le_bytes(event[24..32].try_into().unwrap()),
                            u64::from_le_bytes(event[32..40].try_into().unwrap()),
                        ));
                    }
                    1 => {
                        let mut reply = [0_u8; 32];
                        reply[0] = first[0];
                        peer.read_exact(&mut reply[1..])?;
                        *barrier_replies += 1;
                    }
                    response => {
                        return Err(std::io::Error::other(format!(
                            "unexpected X11 response {response}"
                        )));
                    }
                }
            }
            Ok(())
        };
        send_barrier(1);
        read_until(&mut peer, &mut completions, &mut barrier_replies, 0, 1)
            .expect("client 1 reads the initial barrier reply");
        for (offset, request) in requests_for_driver.iter().copied().enumerate() {
            send_present(u16::try_from(offset + 2).unwrap(), request);
        }
        let before_mark_barrier = u16::try_from(request_count + 2).unwrap();
        send_barrier(before_mark_barrier);
        read_until(&mut peer, &mut completions, &mut barrier_replies, 0, 2)
            .expect("client 1 reads the pre-mark barrier reply");
        let early_completions = completions.clone();
        read_until(
            &mut peer,
            &mut completions,
            &mut barrier_replies,
            request_count,
            2,
        )
        .expect("client 1 receives every requested CompleteNotify");
        if let Some(second_peer) = second_peer.as_mut() {
            let mut second_completions = Vec::new();
            let mut second_barrier_replies = 0;
            assert!(
                read_until(
                    second_peer,
                    &mut second_completions,
                    &mut second_barrier_replies,
                    request_count,
                    0,
                )
                .is_ok(),
                "client 2 receives CompleteNotify for the shared frozen-CRTC Presents"
            );
            assert_eq!(
                second_completions, completions,
                "both subscribed clients observe the same frozen-clock completions"
            );
            (second_completions, early_completions, Instant::now())
        } else {
            (completions, early_completions, Instant::now())
        }
    });

    run_core(
        poll,
        receiver,
        sender.clone_handle(),
        &mut state,
        &mut b,
        [],
        &ClientIdAllocator::new(),
        AuthState::new(None),
        ResetPolicy::NoReset,
        None,
    )
    .expect("run_core dispatches PresentPixmap requests");
    let (completions, early_completions, completion_received_at) =
        driver.join().expect("Present client driver");

    assert!(
        early_completions.is_empty(),
        "clock targets remain pending before the window: {early_completions:?}"
    );
    assert!(
        b.platform.vblank_fallback.frozen.contains_key(&second),
        "silent arm must freeze through run_core: watches={:?}, pending_exec={}",
        b.platform.vblank_fallback.kernel_arm_watch,
        state.present_pending_exec.len()
    );
    let frozen = b.platform.vblank_fallback.frozen[&second];
    assert!(source.legacy_vblank_arms().len() <= 2);
    assert_eq!(source.queued_flip_count(), 0, "no flip event was delivered");
    if flip_in_flight {
        assert!(
            b.scene.has_pending_page_flips(),
            "the synthetic flip stays in flight"
        );
    }
    assert_eq!(completions.len(), requests_to_send.len());
    Some((completions, frozen, completion_received_at))
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: drop the frozen-watch deadline from next_wakeup.
fn frozen_crtc_no_events_completes_after_window() {
    let (mut deadline_backend, deadline_crtc, deadline_now) = two_output_fixture();
    arm_recorded_kernel_wake(&mut deadline_backend, deadline_crtc);
    let armed_at = deadline_now.get();
    assert_eq!(
        Backend::next_wakeup(&deadline_backend),
        Some(armed_at + KmsBackend::FROZEN_VBLANK_WINDOW),
        "the frozen-arm observation deadline participates in Backend next_wakeup"
    );

    let requests = [101, 102, 103].map(|serial| CorePresentRequest {
        client_id: 1,
        serial,
        x_offset: i16::try_from(serial - 101).unwrap(),
        target_msc: 1,
        divisor: 0,
        remainder: 0,
        target_output_idx: 1,
        options: 0,
    });
    let Some((completions, frozen, completion_received_at)) = run_frozen_core_present_requests(
        &requests,
        "frozen_crtc_no_events_completes_after_window",
        false,
        4, // Keep the required one-frame bound robust under CPU load.
    ) else {
        return;
    };
    assert_eq!(
        completions
            .iter()
            .map(|(serial, _, _, _)| *serial)
            .collect::<Vec<_>>(),
        [101, 102, 103],
        "CompleteNotify order is preserved; modes={:?}",
        completions
            .iter()
            .map(|(_, mode, _, _)| *mode)
            .collect::<Vec<_>>()
    );
    assert!(
        completion_received_at.saturating_duration_since(frozen.anchor_at)
            <= frozen.interval + Duration::from_millis(100),
        "all target_msc=1 Presents complete within one frozen frame"
    );
    assert!(
        completions
            .iter()
            .all(|(_, _, _, msc)| *msc == frozen.anchor_msc + 1)
    );
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: misencode the remainder, omit divisor/remainder, or route client 2's completion elsewhere.
fn divisor_present_completes_on_frozen_crtc() {
    let requests = [
        CorePresentRequest {
            client_id: 1,
            serial: 201,
            x_offset: 0,
            target_msc: 0,
            divisor: 8,
            remainder: 7,
            target_output_idx: 1,
            options: 0,
        },
        CorePresentRequest {
            client_id: 2,
            serial: 202,
            x_offset: 1,
            target_msc: 0,
            divisor: 8,
            remainder: 7,
            target_output_idx: 1,
            options: 0,
        },
    ];
    let Some((completions, frozen, _)) = run_frozen_core_present_requests(
        &requests,
        "divisor_present_completes_on_frozen_crtc",
        false,
        4, // Give the armed divisor target a full frame under CPU load.
    ) else {
        return;
    };
    assert_eq!(
        completions
            .iter()
            .map(|(serial, _, _, _)| *serial)
            .collect::<Vec<_>>(),
        [201, 202],
        "both independent clients complete on the frozen CRTC"
    );
    assert!(
        completions
            .iter()
            .all(|(_, _, _, msc)| *msc >= frozen.anchor_msc + 6)
    );
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: let the moved window's newer completion overtake its older Presents.
fn window_moved_off_frozen_crtc_completes_in_order() {
    let requests = [
        CorePresentRequest {
            client_id: 1,
            serial: 301,
            x_offset: 0,
            target_msc: 3,
            divisor: 0,
            remainder: 0,
            target_output_idx: 1,
            options: 0,
        },
        CorePresentRequest {
            client_id: 1,
            serial: 302,
            x_offset: 1,
            target_msc: 3,
            divisor: 0,
            remainder: 0,
            target_output_idx: 1,
            options: 0,
        },
        CorePresentRequest {
            client_id: 1,
            serial: 303,
            x_offset: 2,
            target_msc: 0,
            divisor: 0,
            remainder: 0,
            target_output_idx: 0,
            options: yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
        },
    ];
    let Some((completions, _, _)) = run_frozen_core_present_requests(
        &requests,
        "window_moved_off_frozen_crtc_completes_in_order",
        false,
        60,
    ) else {
        return;
    };
    assert_eq!(
        completions
            .iter()
            .map(|(serial, _, _, _)| *serial)
            .collect::<Vec<_>>(),
        [301, 302, 303],
        "the two frozen-domain Presents stay ahead of the moved-window Present"
    );
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: keep the completion-arm idle gate or suppress frozen clock ticks.
fn frozen_tick_runs_with_a_flip_in_flight() {
    let requests = [CorePresentRequest {
        client_id: 1,
        serial: 401,
        x_offset: 0,
        target_msc: 1,
        divisor: 0,
        remainder: 0,
        target_output_idx: 1,
        options: 0,
    }];
    let Some((completions, frozen, _)) = run_frozen_core_present_requests(
        &requests,
        "frozen_tick_runs_with_a_flip_in_flight",
        true,
        4, // Give the software wake a full frame under CPU load.
    ) else {
        return;
    };
    assert_eq!(completions.len(), 1);
    assert_eq!(completions[0].0, 401);
    assert_eq!(completions[0].3, frozen.anchor_msc + 1);
}

#[test]
// Mutation: remove every CRTC watch when an event clears either arm slot.
fn other_slots_event_keeps_the_surviving_watch() {
    let (mut b, second, now) = two_output_fixture();
    let source = arm_recorded_kernel_wake(&mut b, second);
    assert_eq!(
        Backend::arm_present_completion_idle_vblanks(&mut b, SECOND_CRTC_XID, &[6])
            .expect("idle kernel arm"),
        1
    );

    source.queue_sequence_for_tests(u64::from(u32::from(second.crtc)), 2_000, 6);
    drain_kernel_events(&mut b);

    assert!(!super::vblank_arm_is_watched(
        &b,
        second,
        super::IDLE_VBLANK_ARM_SLOT
    ));
    assert!(super::vblank_arm_is_watched(
        &b,
        second,
        super::SHARED_WAKE_VBLANK_ARM_SLOT
    ));
    assert!(
        b.platform
            .vblank_fallback
            .kernel_arm_watch
            .contains_key(&(second, super::SHARED_WAKE_VBLANK_ARM_SLOT))
    );
    now.set(now.get() + KmsBackend::FROZEN_VBLANK_WINDOW);
    assert!(b.next_wakeup().is_some_and(|wake| wake <= now.get()));
    b.poll_deferred_input(&mut yserver_core::server::ServerState::new());
    assert!(b.platform.vblank_fallback.frozen.contains_key(&second));
}

/// The marco freeze: neither CRTC_QUEUE_SEQUENCE nor WAIT_VBLANK on the
/// device, CompleteNotify held for MSC 6 on an output that never flips. The
/// completion clock the core's due check reads must still reach 6.
#[test]
fn held_completion_on_a_non_flipping_output_advances_without_kernel_vblank() {
    let (mut b, _, now) = two_output_fixture();
    recording_source(&b).set_legacy_vblank(Err(libc::EOPNOTSUPP));
    let mut state = yserver_core::server::ServerState::new();

    let armed = b
        .arm_present_completion_idle_vblanks(SECOND_CRTC_XID, &[6])
        .expect("arming never errors on an unsupported device");
    assert_eq!(armed, 1, "a software vblank stands in for the kernel one");
    assert!(!b.present_absolute_vblank_arm_supported(SECOND_CRTC_XID));
    let tick = now.get() + Duration::from_nanos(16_666_666);
    assert_eq!(b.platform.next_software_vblank_deadline(), Some(tick));
    let wakeup = b.next_wakeup().expect("the software vblank wakes the loop");
    assert!(wakeup <= tick);

    now.set(now.get() + Duration::from_millis(17));
    b.poll_deferred_input(&mut state);

    let clock = b.present_get_completion_clock(SECOND_CRTC_XID);
    assert_eq!(clock.msc, 6, "CompleteNotify for MSC 6 is now due");
    assert_eq!(clock.source, PresentClockSource::IdleSequence);
    assert_eq!(b.present_get_ust_msc(SECOND_CRTC_XID).0, 6);
    let first = CrtcKey::for_output(&b.platform.outputs[0]);
    assert_eq!(
        b.platform.present_get_ust_msc(first).0,
        0,
        "the other output's clock is untouched"
    );
    assert!(b.platform.vblank_fallback.software.is_empty());
}

#[test]
fn software_vblank_does_not_double_count_an_msc_another_source_advanced() {
    let (mut b, second, now) = two_output_fixture();
    recording_source(&b).set_legacy_vblank(Err(libc::EOPNOTSUPP));
    let mut state = yserver_core::server::ServerState::new();
    b.arm_present_completion_idle_vblanks(SECOND_CRTC_XID, &[6])
        .unwrap();

    // A page flip on this output lands first.
    b.platform.record_vblank_clock(second, 6, 2_000);
    now.set(now.get() + Duration::from_millis(17));
    b.poll_deferred_input(&mut state);

    assert_eq!(b.present_get_ust_msc(SECOND_CRTC_XID).0, 6);
    assert!(b.platform.vblank_fallback.software.is_empty());
}

#[test]
fn software_vblank_waits_one_frame_period() {
    let (mut b, _, now) = two_output_fixture();
    recording_source(&b).set_legacy_vblank(Err(libc::EOPNOTSUPP));
    let mut state = yserver_core::server::ServerState::new();
    b.arm_present_completion_idle_vblanks(SECOND_CRTC_XID, &[6])
        .unwrap();

    now.set(now.get() + Duration::from_millis(10));
    b.poll_deferred_input(&mut state);
    assert_eq!(b.present_get_completion_clock(SECOND_CRTC_XID).msc, 5);
    assert_eq!(
        b.arm_present_completion_idle_vblanks(SECOND_CRTC_XID, &[6])
            .unwrap(),
        0,
        "one armed software vblank per CRTC"
    );
}

#[test]
fn software_vblanks_stop_while_the_outputs_are_off() {
    let (mut b, _, now) = two_output_fixture();
    recording_source(&b).set_legacy_vblank(Err(libc::EOPNOTSUPP));
    let mut state = yserver_core::server::ServerState::new();
    b.arm_present_completion_idle_vblanks(SECOND_CRTC_XID, &[6])
        .unwrap();

    b.kms_outputs_active = false;
    now.set(now.get() + Duration::from_millis(17));
    b.poll_deferred_input(&mut state);

    assert_eq!(b.present_get_ust_msc(SECOND_CRTC_XID).0, 5);
    assert!(b.platform.next_software_vblank_deadline().is_none());
}

/// Xorg `ms_queue_vblank`: once the device rejects CRTC_QUEUE_SEQUENCE the
/// tagged next-vblank wake is queued through WAIT_VBLANK, and its event releases
/// the held completion through the sequence-event path.
#[test]
fn wait_vblank_fallback_arms_shared_wake_and_its_event_releases_completion() {
    let (mut b, second, _) = two_output_fixture();
    let source = recording_source(&b);
    source.set_legacy_vblank(Ok(5));
    let crtc_id = u32::from(second.crtc);

    assert!(b.present_absolute_vblank_arm_supported(SECOND_CRTC_XID));
    let armed =
        Backend::arm_present_shared_wake(&mut b, SECOND_CRTC_XID).expect("legacy arm succeeds");
    assert!(armed);
    assert!(
        b.pstate
            .crtc_queue_sequence_unsupported_devices
            .contains(&second.device_key),
        "the fixture device rejects CRTC_QUEUE_SEQUENCE"
    );
    assert_eq!(
        source.legacy_vblank_arms(),
        [(
            1,
            true,
            1,
            crtc_id | crate::drm::page_flip::LEGACY_VBLANK_ABSOLUTE_SIGNAL
        )]
    );
    assert!(b.present_absolute_vblank_arm_supported(SECOND_CRTC_XID));

    // Busy scene: only an explicitly armed (tagged) event may release it.
    b.scene.scene_structure_dirty = true;
    source.fire_legacy_vblank_arms(3_000_000, 6);
    drain_kernel_events(&mut b);

    let clock = b.present_get_completion_clock(SECOND_CRTC_XID);
    assert_eq!((clock.msc, clock.ust), (6, 3_000));
    assert!(!super::vblank_arm_is_watched(
        &b,
        second,
        super::SHARED_WAKE_VBLANK_ARM_SLOT
    ));
}

#[test]
fn wait_vblank_fallback_arms_the_relative_idle_vblank() {
    let (mut b, second, _) = two_output_fixture();
    let source = recording_source(&b);
    source.set_legacy_vblank(Ok(5));
    let crtc_id = u32::from(second.crtc);

    assert_eq!(
        b.arm_present_completion_idle_vblanks(SECOND_CRTC_XID, &[6])
            .unwrap(),
        1
    );
    assert_eq!(source.legacy_vblank_arms(), [(1, true, 1, crtc_id)]);
    assert!(b.platform.vblank_fallback.software.is_empty());

    source.fire_legacy_vblank_arms(4_000_000, 6);
    drain_kernel_events(&mut b);
    assert_eq!(b.present_get_completion_clock(SECOND_CRTC_XID).msc, 6);
    assert!(!super::vblank_arm_is_watched(
        &b,
        second,
        super::IDLE_VBLANK_ARM_SLOT
    ));
}

#[test]
fn frame_zero_flip_takes_the_legacy_vblank_count() {
    let (mut b, second, _) = two_output_fixture();
    let source = recording_source(&b);
    source.set_legacy_vblank(Ok(4_242));
    source.queue_flip(second.crtc, 0);
    drain_kernel_events(&mut b);
    assert_eq!(b.platform.present_get_ust_msc(second).0, 4_242);
    assert_eq!(source.legacy_vblank_queries(), 1);
}

#[test]
fn frame_zero_flip_without_wait_vblank_keeps_the_software_count() {
    let (mut b, second, _) = two_output_fixture();
    let source = recording_source(&b);
    source.set_legacy_vblank(Err(libc::EOPNOTSUPP));
    b.platform.software_msc.insert(second, 7);
    source.queue_flip(second.crtc, 0);
    source.queue_flip(second.crtc, 0);
    drain_kernel_events(&mut b);
    assert_eq!(b.platform.present_get_ust_msc(second).0, 9);
    assert_eq!(
        source.legacy_vblank_queries(),
        1,
        "the unsupported query is latched after one try"
    );
    assert!(!b.platform.wait_vblank_supported(second.device_key));
}

#[test]
fn legacy_absolute_user_data_matches_the_sequence_tag() {
    assert_eq!(
        crate::drm::page_flip::SEQUENCE_ABSOLUTE_USER_DATA,
        crate::kms::render::backend::ABSOLUTE_SEQ_TAG
    );
}

#[test]
fn frame_interval_follows_the_mode_timing() {
    use crate::kms::render::platform::vblank_fallback::mode_frame_interval;
    let exact = crate::platform::drm::Mode {
        clock_khz: 148_500,
        htotal: 2_200,
        vtotal: 1_125,
        vrefresh: 60,
        ..Default::default()
    };
    assert_eq!(
        mode_frame_interval(&exact),
        Duration::from_nanos(16_666_666)
    );
    let nominal = crate::platform::drm::Mode {
        vrefresh: 75,
        ..Default::default()
    };
    assert_eq!(
        mode_frame_interval(&nominal),
        Duration::from_nanos(13_333_333)
    );
}
