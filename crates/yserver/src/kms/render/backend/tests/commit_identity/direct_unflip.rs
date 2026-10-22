//! Direct-frame and unflip retirement by accepted atomic commit identity.

use super::{
    atomic_events::{complete_recorded_attempt, pending_ids},
    recording_scene::{DirectPresentSetup, P1SceneRoute, RecordingSceneFixture},
    support::accepted_event_id,
};
use crate::drm::atomic::tests_support::{AtomicCommitRecord, ScriptedResult};
use std::{
    collections::{HashMap, HashSet},
    os::fd::RawFd,
    time::{Duration, Instant},
};
use yserver_core::backend::Backend;

const DIRECT_PROBATION: usize = 8;
const WAIT_DEADLINE: Duration = Duration::from_secs(5);

pub(super) fn direct_fixture(
    output_count: usize,
    route: P1SceneRoute,
    present_id: u64,
    options: u32,
) -> Option<(RecordingSceneFixture, DirectPresentSetup)> {
    let mut fixture = RecordingSceneFixture::with_route(output_count, route)
        .expect("direct recording scene fixture construction")?;
    let direct = fixture.install_direct_present(present_id, options);
    Some((fixture, direct))
}

pub(super) fn fd(fixture: &RecordingSceneFixture) -> RawFd {
    fixture.kms_fd(0)
}

pub(super) fn record_count(fixture: &RecordingSceneFixture, device_fd: RawFd) -> usize {
    fixture.events.recorders[&device_fd].records().len()
}

pub(super) fn records_since(
    fixture: &RecordingSceneFixture,
    device_fd: RawFd,
    offset: usize,
) -> Vec<AtomicCommitRecord> {
    fixture.events.recorders[&device_fd]
        .records()
        .into_iter()
        .skip(offset)
        .filter(|record| record.device_fd == device_fd)
        .collect()
}

pub(super) fn accepted_records(records: &[AtomicCommitRecord]) -> Vec<AtomicCommitRecord> {
    records
        .iter()
        .filter(|record| accepted_event_id(record).is_some())
        .cloned()
        .collect()
}

fn admit_direct(fixture: &mut RecordingSceneFixture, direct: &DirectPresentSetup) {
    for attempt in 0..DIRECT_PROBATION {
        let accepted = Backend::try_present_direct(
            &mut fixture.events.backend,
            direct.candidate,
            direct.event.clone(),
        )
        .expect("direct candidate through Backend");
        assert_eq!(
            accepted,
            attempt + 1 == DIRECT_PROBATION,
            "the existing direct probation must be driven by Backend entries"
        );
    }
}

fn enter_async_tearing(fixture: &mut RecordingSceneFixture, direct: &DirectPresentSetup) {
    let device = &mut fixture.events.backend.platform.devices[0];
    device.async_page_flip = true;
    let start = record_count(fixture, fd(fixture));
    super::super::tearing::submit_initial_tearing_frame(&mut fixture.events.backend);
    let initial = records_since(fixture, fd(fixture), start)
        .into_iter()
        .find(|record| accepted_event_id(record).is_some())
        .expect("initial tearing scene frame accepted");
    let initial_id = accepted_event_id(&initial).expect("accepted initial frame has an ID");
    complete_recorded_attempt(fixture, fd(fixture), &initial, initial_id, 41);
    super::super::tearing::finish_entering_tearing(
        &mut fixture.events.backend,
        direct.target_xid,
        direct.target_xid,
        direct.target_xid,
        direct.candidate.present_id + 100,
    );
    Backend::set_cursor_hidden(&mut fixture.events.backend, true);
}

pub(super) fn submit_direct(
    fixture: &mut RecordingSceneFixture,
    direct: &DirectPresentSetup,
    device_fd: RawFd,
) -> Vec<AtomicCommitRecord> {
    let start = record_count(fixture, device_fd);
    admit_direct(fixture, direct);
    records_since(fixture, device_fd, start)
}

pub(super) fn inject_output(
    fixture: &mut RecordingSceneFixture,
    device_fd: RawFd,
    record: &AtomicCommitRecord,
    output_idx: usize,
    event_id: u64,
    frame: u32,
    ust: Duration,
) {
    fixture.events.queue_commit_completion(
        device_fd,
        record.events[output_idx].crtc,
        event_id,
        frame,
        ust,
    );
    Backend::on_page_flip_ready(
        &mut fixture.events.backend,
        &mut fixture.events.state,
        device_fd,
    );
}

pub(super) fn request_unflip(fixture: &mut RecordingSceneFixture, direct: &DirectPresentSetup) {
    let mut successor = direct.candidate;
    successor.present_id += 1;
    successor.valid_region_xid = 0x55;
    let mut event = direct.event.clone();
    event.present_id = successor.present_id;
    event.serial = event.serial.wrapping_add(1);
    assert!(
        !Backend::try_present_direct(&mut fixture.events.backend, successor, event)
            .expect("ineligible authoritative-root successor"),
        "the successor must request a composed unflip"
    );
    assert!(fixture.events.backend.pstate.scanout_m2.unflip_requested);
}

pub(super) fn drain_present_notifications(
    fixture: &mut RecordingSceneFixture,
    expected_completed: &[u64],
    expected_idled: &[u64],
) {
    let completed = Backend::drain_completed_present_events(&mut fixture.events.backend);
    assert_eq!(
        completed.len(),
        expected_completed.len(),
        "unexpected number of direct Complete events"
    );
    assert_eq!(
        completed
            .iter()
            .map(|event| event.present_id)
            .collect::<Vec<_>>(),
        expected_completed,
        "direct Complete events must belong to the expected Presents"
    );
    let idled = Backend::drain_retired_present_idle_events(&mut fixture.events.backend);
    assert_eq!(
        idled.len(),
        expected_idled.len(),
        "unexpected number of direct Idle events"
    );
    assert_eq!(
        idled
            .iter()
            .map(|event| event.present_id)
            .collect::<Vec<_>>(),
        expected_idled,
        "direct Idle events must belong to the expected Presents"
    );
}

pub(super) fn complete_direct_initial(
    fixture: &mut RecordingSceneFixture,
    direct_record: &AtomicCommitRecord,
    direct_id: u64,
    device_fd: RawFd,
) {
    for (output_idx, event) in direct_record.events.iter().enumerate() {
        fixture.events.queue_commit_completion(
            device_fd,
            event.crtc,
            direct_id,
            50 + u32::try_from(output_idx).expect("output index fits"),
            Duration::from_millis(500 + output_idx as u64 * 10),
        );
    }
    Backend::on_page_flip_ready(
        &mut fixture.events.backend,
        &mut fixture.events.state,
        device_fd,
    );
    assert!(fixture.events.backend.pstate.scanout_m2.pending.is_none());
    assert!(fixture.events.backend.pstate.scanout_m2.current.is_some());
}

pub(super) fn grouped_unflip_record(
    fixture: &mut RecordingSceneFixture,
    device_fd: RawFd,
    offset: usize,
    output_count: usize,
) -> AtomicCommitRecord {
    Backend::maybe_composite(&mut fixture.events.backend).expect("submit grouped unflip");
    records_since(fixture, device_fd, offset)
        .into_iter()
        .find(|record| record.events.len() == output_count && accepted_event_id(record).is_some())
        .expect("grouped composed replacement accepted")
}

fn scene_completion_for_output(
    fixture: &mut RecordingSceneFixture,
    output_idx: usize,
    device_fd: RawFd,
    offset: usize,
) -> AtomicCommitRecord {
    let deadline = Instant::now() + WAIT_DEADLINE;
    loop {
        Backend::before_block(&mut fixture.events.backend);
        let root = fixture.events.backend.core.window_id;
        let width = fixture.events.backend.platform.fb_w;
        let height = fixture.events.backend.platform.fb_h;
        Backend::clear_area(
            &mut fixture.events.backend,
            None,
            root,
            0,
            None,
            0,
            0,
            width,
            height,
            (0, 0),
        )
        .expect("damage root through Backend");
        Backend::maybe_composite(&mut fixture.events.backend)
            .expect("compose degraded replacement through Backend");
        if let Some(record) = records_since(fixture, device_fd, offset)
            .into_iter()
            .find(|record| {
                accepted_event_id(record).is_some()
                    && record.events.len() == 1
                    && record.events[0].crtc
                        == fixture.events.backend.platform.outputs[output_idx]
                            .output
                            .crtc
            })
        {
            return record;
        }
        assert!(
            Instant::now() < deadline,
            "output {output_idx} did not accept a degraded replacement"
        );
    }
}

fn compose_copied_job(fixture: &mut RecordingSceneFixture) -> u64 {
    let deadline = Instant::now() + WAIT_DEADLINE;
    loop {
        Backend::before_block(&mut fixture.events.backend);
        let root = fixture.events.backend.core.window_id;
        let width = fixture.events.backend.platform.fb_w;
        let height = fixture.events.backend.platform.fb_h;
        Backend::clear_area(
            &mut fixture.events.backend,
            None,
            root,
            0,
            None,
            0,
            0,
            width,
            height,
            (0, 0),
        )
        .expect("damage copied scene through Backend");
        Backend::maybe_composite(&mut fixture.events.backend)
            .expect("tick copied scene through Backend");
        if let Some(&job_id) = fixture.pending_render_job_ids().last() {
            return job_id;
        }
        assert!(
            Instant::now() < deadline,
            "copied scene did not submit a render job"
        );
    }
}

fn deliver_copied_job(fixture: &mut RecordingSceneFixture, job_id: u64) {
    fixture
        .readiness
        .as_ref()
        .expect("copied fixture has a readiness observer")
        .release_ready_job(job_id);
    let deadline = Instant::now() + WAIT_DEADLINE;
    loop {
        Backend::on_scanout_render_completion(
            &mut fixture.events.backend,
            &mut fixture.events.state,
        );
        if !fixture.pending_render_job_ids().contains(&job_id) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "copied completion was not delivered"
        );
        Backend::before_block(&mut fixture.events.backend);
    }
}

pub(super) fn direct_source_pins(
    fixture: &RecordingSceneFixture,
    source_id: crate::kms::render::store::DrawableId,
) -> usize {
    fixture
        .events
        .backend
        .present_source_pins
        .values()
        .filter(|&&id| id == source_id)
        .count()
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: overwrite the selected reference-output clock when a later output retires.
fn p1_grouped_direct_awaits_accepted_id_on_every_output() {
    let Some((mut fixture, direct)) = direct_fixture(2, P1SceneRoute::Shared, 201, 0) else {
        return;
    };
    let device_fd = fd(&fixture);
    let attempts = submit_direct(&mut fixture, &direct, device_fd);
    let accepted = accepted_records(&attempts);
    assert_eq!(accepted.len(), 1);
    let direct_record = &accepted[0];
    let direct_id = accepted_event_id(direct_record).expect("direct request accepted");
    assert_eq!(direct_record.events.len(), 2);
    assert_ne!(direct_id, 0);
    assert_eq!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .pending
            .as_ref()
            .and_then(|frame| frame.accepted_event_id),
        Some(direct_id),
        "the live direct frame retains its accepted grouped ID"
    );
    for event in &direct_record.events {
        assert_eq!(pending_ids(&fixture, device_fd, event.crtc), [direct_id]);
    }

    let reference_clock = yserver_core::backend::PresentClockSample {
        msc: 61,
        ust: 6_100_000,
        source: yserver_core::backend::PresentClockSource::PageFlip,
    };
    fixture.events.queue_commit_completion(
        device_fd,
        direct_record.events[0].crtc,
        u64::MAX,
        60,
        Duration::from_millis(6_000),
    );
    fixture.events.queue_commit_completion(
        device_fd,
        direct_record.events[0].crtc,
        direct_id,
        61,
        Duration::from_millis(6_100),
    );
    Backend::on_page_flip_ready(
        &mut fixture.events.backend,
        &mut fixture.events.state,
        device_fd,
    );
    assert!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .completed
            .is_empty()
    );
    assert!(fixture.events.backend.pstate.scanout_m2.idled.is_empty());
    assert_eq!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .pending
            .as_ref()
            .unwrap()
            .awaiting_outputs,
        HashSet::from([1])
    );
    assert_eq!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .pending
            .as_ref()
            .unwrap()
            .completion_clock,
        Some(reference_clock)
    );
    fixture.events.queue_commit_completion(
        device_fd,
        direct_record.events[0].crtc,
        direct_id,
        62,
        Duration::from_millis(6_200),
    );
    Backend::on_page_flip_ready(
        &mut fixture.events.backend,
        &mut fixture.events.state,
        device_fd,
    );
    assert!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .completed
            .is_empty()
    );
    assert_eq!(
        pending_ids(&fixture, device_fd, direct_record.events[1].crtc),
        [direct_id]
    );
    inject_output(
        &mut fixture,
        device_fd,
        direct_record,
        1,
        direct_id,
        63,
        Duration::from_millis(6_300),
    );
    assert!(fixture.events.backend.pstate.scanout_m2.pending.is_none());
    assert_eq!(fixture.events.backend.pstate.scanout_m2.completed.len(), 1);
    assert_eq!(
        fixture.events.backend.pstate.scanout_m2.completed[0].completion_clock,
        Some(reference_clock),
        "the selected reference output supplies the completion clock"
    );
    drain_present_notifications(&mut fixture, &[direct.candidate.present_id], &[]);

    request_unflip(&mut fixture, &direct);
    let offset = record_count(&fixture, device_fd);
    let replacement = grouped_unflip_record(&mut fixture, device_fd, offset, 2);
    let replacement_id = accepted_event_id(&replacement).unwrap();
    complete_recorded_attempt(&mut fixture, device_fd, &replacement, replacement_id, 64);
    assert!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .unflip_outputs_to_replace
            .is_empty()
    );
    assert!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .unflip_awaiting_outputs
            .is_empty()
    );
    assert!(fixture.events.backend.present_source_pins.is_empty());
    drain_present_notifications(&mut fixture, &[], &[direct.candidate.present_id]);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: discard the accepted Vsync ID after async EINVAL.
fn p1_direct_async_fallback_uses_only_vsync_acceptance() {
    let Some((mut fixture, direct)) = direct_fixture(
        1,
        P1SceneRoute::Shared,
        202,
        yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
    ) else {
        return;
    };
    enter_async_tearing(&mut fixture, &direct);
    let device_fd = fd(&fixture);
    let recorder = fixture.events.recorders[&device_fd].clone();
    recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EINVAL)));
    let attempts = submit_direct(&mut fixture, &direct, device_fd);
    assert_eq!(
        attempts.len(),
        2,
        "async request followed by Vsync fallback"
    );
    assert_eq!(
        attempts[0].kind,
        crate::drm::atomic::AtomicCommitKind::Async
    );
    assert_ne!(attempts[0].result, ScriptedResult::Ok);
    assert_eq!(attempts[1].result, ScriptedResult::Ok);
    let async_id = attempts[0].user_data;
    let vsync_id = accepted_event_id(&attempts[1]).expect("Vsync fallback accepted");
    assert_ne!(async_id, 0);
    assert_ne!(vsync_id, async_id);
    assert_eq!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .pending
            .as_ref()
            .and_then(|frame| frame.accepted_event_id),
        Some(vsync_id),
        "the frame awaits only the accepted Vsync request"
    );
    let crtc = attempts[1].events[0].crtc;
    fixture.events.queue_commit_completion(
        device_fd,
        crtc,
        async_id,
        70,
        Duration::from_millis(7_000),
    );
    Backend::on_page_flip_ready(
        &mut fixture.events.backend,
        &mut fixture.events.state,
        device_fd,
    );
    assert!(fixture.events.backend.pstate.scanout_m2.pending.is_some());
    assert_eq!(pending_ids(&fixture, device_fd, crtc), [vsync_id]);
    complete_recorded_attempt(&mut fixture, device_fd, &attempts[1], vsync_id, 71);
    assert!(fixture.events.backend.pstate.scanout_m2.current.is_some());
    drain_present_notifications(&mut fixture, &[direct.candidate.present_id], &[]);
    request_unflip(&mut fixture, &direct);
    let offset = record_count(&fixture, device_fd);
    let replacement = grouped_unflip_record(&mut fixture, device_fd, offset, 1);
    let replacement_id = accepted_event_id(&replacement).unwrap();
    complete_recorded_attempt(&mut fixture, device_fd, &replacement, replacement_id, 72);
    assert!(fixture.events.backend.present_source_pins.is_empty());
    drain_present_notifications(&mut fixture, &[], &[direct.candidate.present_id]);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: convert the two failed atomic attempts into a successful direct submit with cookie zero.
fn p1_direct_both_attempts_fail_without_obligation_or_pin_leak() {
    let Some((mut fixture, async_direct)) = direct_fixture(
        1,
        P1SceneRoute::Shared,
        203,
        yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
    ) else {
        return;
    };
    enter_async_tearing(&mut fixture, &async_direct);
    let device_fd = fd(&fixture);
    let recorder = fixture.events.recorders[&device_fd].clone();
    recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EINVAL)));
    recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EIO)));
    let start = record_count(&fixture, device_fd);
    for _ in 0..DIRECT_PROBATION - 1 {
        assert!(
            !Backend::try_present_direct(
                &mut fixture.events.backend,
                async_direct.candidate,
                async_direct.event.clone(),
            )
            .expect("direct candidate probation through Backend")
        );
    }
    assert!(
        Backend::try_present_direct(
            &mut fixture.events.backend,
            async_direct.candidate,
            async_direct.event.clone(),
        )
        .is_err(),
        "both direct attempts fail through Backend"
    );
    let failed = records_since(&fixture, device_fd, start);
    assert_eq!(failed.len(), 2);
    assert!(
        failed
            .iter()
            .all(|attempt| attempt.result != ScriptedResult::Ok)
    );
    assert!(fixture.events.backend.pstate.scanout_m2.pending.is_none());
    assert!(fixture.events.backend.pstate.scanout_m2.current.is_none());
    assert!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .queued_successor
            .is_none()
    );
    assert!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .unflip_awaiting_outputs
            .is_empty()
    );
    assert!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .unflip_outputs_to_replace
            .is_empty()
    );
    assert!(fixture.events.backend.present_source_pins.is_empty());
    for event in &failed[0].events {
        assert!(pending_ids(&fixture, device_fd, event.crtc).is_empty());
    }

    let mut later = async_direct;
    later.candidate.options = 0;
    let mut later_event = later.event.clone();
    later_event.options = 0;
    later.event = later_event;
    let accepted_attempts = submit_direct(&mut fixture, &later, device_fd);
    let accepted = accepted_records(&accepted_attempts);
    assert_eq!(accepted.len(), 1);
    let accepted_id = accepted_event_id(&accepted[0]).unwrap();
    assert_ne!(accepted_id, failed[0].user_data);
    assert_ne!(accepted_id, failed[1].user_data);
    for attempt in &failed {
        for event in &attempt.events {
            fixture.events.queue_commit_completion(
                device_fd,
                event.crtc,
                attempt.user_data,
                80,
                Duration::from_millis(8_000),
            );
        }
    }
    Backend::on_page_flip_ready(
        &mut fixture.events.backend,
        &mut fixture.events.state,
        device_fd,
    );
    assert!(fixture.events.backend.pstate.scanout_m2.pending.is_some());
    assert_eq!(
        pending_ids(&fixture, device_fd, accepted[0].events[0].crtc),
        [accepted_id]
    );
    complete_recorded_attempt(&mut fixture, device_fd, &accepted[0], accepted_id, 81);
    assert!(fixture.events.backend.pstate.scanout_m2.current.is_some());
    drain_present_notifications(&mut fixture, &[later.candidate.present_id], &[]);
    request_unflip(&mut fixture, &later);
    let offset = record_count(&fixture, device_fd);
    let replacement = grouped_unflip_record(&mut fixture, device_fd, offset, 1);
    let replacement_id = accepted_event_id(&replacement).unwrap();
    complete_recorded_attempt(&mut fixture, device_fd, &replacement, replacement_id, 82);
    assert!(fixture.events.backend.present_source_pins.is_empty());
    drain_present_notifications(&mut fixture, &[], &[later.candidate.present_id]);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: attribute an explicit ID miss to the queue front so a late direct cookie steals the replacement event.
fn p1_grouped_unflip_ignores_late_direct_or_duplicate_cookie() {
    let Some((mut fixture, direct)) = direct_fixture(2, P1SceneRoute::Shared, 204, 0) else {
        return;
    };
    let device_fd = fd(&fixture);
    let attempts = submit_direct(&mut fixture, &direct, device_fd);
    let initial = accepted_records(&attempts).remove(0);
    let direct_id = accepted_event_id(&initial).unwrap();
    complete_direct_initial(&mut fixture, &initial, direct_id, device_fd);
    drain_present_notifications(&mut fixture, &[direct.candidate.present_id], &[]);
    request_unflip(&mut fixture, &direct);
    let offset = record_count(&fixture, device_fd);
    let replacement = grouped_unflip_record(&mut fixture, device_fd, offset, 2);
    let replacement_id = accepted_event_id(&replacement).unwrap();
    assert_eq!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .unflip_awaiting_outputs,
        HashMap::from([(0, replacement_id), (1, replacement_id)])
    );
    assert_eq!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .unflip_outputs_to_replace,
        HashSet::from([0, 1])
    );

    fixture.events.queue_commit_completion(
        device_fd,
        replacement.events[0].crtc,
        direct_id,
        90,
        Duration::from_millis(9_000),
    );
    Backend::on_page_flip_ready(
        &mut fixture.events.backend,
        &mut fixture.events.state,
        device_fd,
    );
    assert_eq!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .unflip_awaiting_outputs
            .len(),
        2,
        "a late direct cookie cannot satisfy an accepted replacement"
    );
    inject_output(
        &mut fixture,
        device_fd,
        &replacement,
        0,
        replacement_id,
        91,
        Duration::from_millis(9_100),
    );
    assert_eq!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .unflip_awaiting_outputs,
        HashMap::from([(1, replacement_id)])
    );
    assert_eq!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .unflip_outputs_to_replace,
        HashSet::from([1])
    );
    assert!(fixture.events.backend.pstate.scanout_m2.current.is_some());
    assert!(!fixture.events.backend.present_source_pins.is_empty());
    fixture.events.queue_commit_completion(
        device_fd,
        replacement.events[0].crtc,
        replacement_id,
        92,
        Duration::from_millis(9_200),
    );
    Backend::on_page_flip_ready(
        &mut fixture.events.backend,
        &mut fixture.events.state,
        device_fd,
    );
    assert_eq!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .unflip_awaiting_outputs,
        HashMap::from([(1, replacement_id)])
    );
    inject_output(
        &mut fixture,
        device_fd,
        &replacement,
        1,
        replacement_id,
        93,
        Duration::from_millis(9_300),
    );
    assert!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .unflip_awaiting_outputs
            .is_empty()
    );
    assert!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .unflip_outputs_to_replace
            .is_empty()
    );
    assert!(fixture.events.backend.present_source_pins.is_empty());
    drain_present_notifications(&mut fixture, &[], &[direct.candidate.present_id]);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: release direct pins while an output still needs replacement; clear unflip_requested before the final replacement retires.
fn p1_degraded_shared_unflip_waits_for_accepted_replacements() {
    let Some((mut fixture, direct)) = direct_fixture(2, P1SceneRoute::Shared, 205, 0) else {
        return;
    };
    let device_fd = fd(&fixture);
    let initial_attempts = submit_direct(&mut fixture, &direct, device_fd);
    let initial = accepted_records(&initial_attempts).remove(0);
    let direct_id = accepted_event_id(&initial).unwrap();
    complete_direct_initial(&mut fixture, &initial, direct_id, device_fd);
    drain_present_notifications(&mut fixture, &[direct.candidate.present_id], &[]);
    request_unflip(&mut fixture, &direct);

    let recorder = fixture.events.recorders[&device_fd].clone();
    let offset = record_count(&fixture, device_fd);
    // Fresh grouped unflip, then the retained-framebuffer grouped unflip.
    recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EIO)));
    recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EIO)));
    recorder.script_result(Ok(()));
    recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EIO)));
    Backend::maybe_composite(&mut fixture.events.backend).expect("degrade grouped unflip");
    let attempts = records_since(&fixture, device_fd, offset);
    let failed_group = attempts
        .iter()
        .find(|record| record.events.len() == 2 && record.result != ScriptedResult::Ok)
        .expect("grouped unflip rejected");
    let accepted_scene: Vec<_> = accepted_records(&attempts)
        .into_iter()
        .filter(|record| record.events.len() == 1)
        .collect();
    let rejected_scene: Vec<_> = attempts
        .iter()
        .filter(|record| record.events.len() == 1 && record.result != ScriptedResult::Ok)
        .collect();
    assert_eq!(accepted_scene.len(), 1);
    assert_eq!(rejected_scene.len(), 1);
    let replaced = fixture
        .events
        .backend
        .platform
        .outputs
        .iter()
        .position(|output| output.output.crtc == accepted_scene[0].events[0].crtc)
        .unwrap();
    let waiting = 1 - replaced;
    let accepted_id = accepted_event_id(&accepted_scene[0]).unwrap();
    let rejected_id = rejected_scene[0].user_data;
    assert_ne!(failed_group.user_data, accepted_id);
    assert_eq!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .unflip_outputs_to_replace,
        HashSet::from([0, 1])
    );
    assert_eq!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .unflip_awaiting_outputs,
        HashMap::from([(replaced, accepted_id)])
    );

    fixture.events.queue_commit_completion(
        device_fd,
        accepted_scene[0].events[0].crtc,
        direct_id,
        100,
        Duration::from_millis(10_000),
    );
    let rejected_crtc = rejected_scene[0].events[0].crtc;
    fixture.events.queue_commit_completion(
        device_fd,
        rejected_crtc,
        rejected_id,
        101,
        Duration::from_millis(10_100),
    );
    Backend::on_page_flip_ready(
        &mut fixture.events.backend,
        &mut fixture.events.state,
        device_fd,
    );
    assert_eq!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .unflip_outputs_to_replace
            .len(),
        2
    );
    assert_eq!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .unflip_awaiting_outputs,
        HashMap::from([(replaced, accepted_id)])
    );

    inject_output(
        &mut fixture,
        device_fd,
        &accepted_scene[0],
        0,
        accepted_id,
        102,
        Duration::from_millis(10_200),
    );
    assert_eq!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .unflip_outputs_to_replace,
        HashSet::from([waiting])
    );
    assert!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .unflip_awaiting_outputs
            .is_empty(),
        "no accepted replacement ID remains while one output still requires replacement"
    );
    assert!(
        fixture.events.backend.pstate.scanout_m2.current.is_some(),
        "an empty accepted map cannot retire direct scanout while another output still needs replacement"
    );
    assert_eq!(direct_source_pins(&fixture, direct.source_id), 1);
    assert_eq!(fixture.events.backend.present_source_pins.len(), 2);

    let early_reentry_id = direct.candidate.present_id + 10_000;
    let mut early_reentry_candidate = direct.candidate;
    early_reentry_candidate.present_id = early_reentry_id;
    let mut early_reentry_event = direct.event.clone();
    early_reentry_event.present_id = early_reentry_id;
    early_reentry_event.serial = early_reentry_event.serial.wrapping_add(10_000);
    let before_early_reentry = record_count(&fixture, device_fd);
    assert!(
        !Backend::try_present_direct(
            &mut fixture.events.backend,
            early_reentry_candidate,
            early_reentry_event,
        )
        .expect("eligible Present remains behind degraded unflip"),
        "direct admission stays closed while any degraded output needs replacement"
    );
    assert_eq!(
        record_count(&fixture, device_fd),
        before_early_reentry,
        "the eligible Present must not submit a direct commit"
    );
    assert!(fixture.events.backend.pstate.scanout_m2.unflip_requested);
    assert!(fixture.events.backend.pstate.scanout_m2.pending.is_none());

    let later_offset = record_count(&fixture, device_fd);
    recorder.script_result(Ok(()));
    let later = scene_completion_for_output(&mut fixture, waiting, device_fd, later_offset);
    let later_id = accepted_event_id(&later).unwrap();
    assert_eq!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .unflip_awaiting_outputs,
        HashMap::from([(waiting, later_id)])
    );
    let later_scene_frames = accepted_records(&records_since(&fixture, device_fd, later_offset));
    assert!(later_scene_frames.iter().any(|record| {
        accepted_event_id(record) == Some(later_id) && record.events[0].crtc == later.events[0].crtc
    }));
    for (index, record) in later_scene_frames.iter().enumerate() {
        complete_recorded_attempt(
            &mut fixture,
            device_fd,
            record,
            accepted_event_id(record).unwrap(),
            103 + u32::try_from(index).expect("scene frame index fits"),
        );
    }
    assert!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .unflip_outputs_to_replace
            .is_empty()
    );
    assert!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .unflip_awaiting_outputs
            .is_empty()
    );
    assert!(fixture.events.backend.present_source_pins.is_empty());
    drain_present_notifications(&mut fixture, &[], &[direct.candidate.present_id]);

    fixture.drain_deferred_pool_releases(Duration::from_secs(5));
    assert!(
        !fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .reentry_blocked_until_composed,
        "normal composed fallback reopens direct admission after every replacement retires"
    );
    let admitted_reentry_id = direct.candidate.present_id + 10_001;
    let admitted_start = record_count(&fixture, device_fd);
    for attempt in 0..DIRECT_PROBATION {
        let present_id = admitted_reentry_id + u64::try_from(attempt).unwrap();
        let mut candidate = direct.candidate;
        candidate.present_id = present_id;
        let mut event = direct.event.clone();
        event.present_id = present_id;
        event.serial = event.serial.wrapping_add(u32::try_from(attempt).unwrap());
        let accepted = Backend::try_present_direct(&mut fixture.events.backend, candidate, event)
            .expect("eligible Present after all unflip requirements retire");
        assert_eq!(
            accepted,
            attempt + 1 == DIRECT_PROBATION,
            "direct admission resumes only after the existing probation"
        );
    }
    let reentered = accepted_records(&records_since(&fixture, device_fd, admitted_start));
    assert_eq!(reentered.len(), 1, "the admitted Present submits once");
    let reentered_id = accepted_event_id(&reentered[0]).unwrap();
    complete_recorded_attempt(&mut fixture, device_fd, &reentered[0], reentered_id, 104);
    drain_present_notifications(
        &mut fixture,
        &[admitted_reentry_id + (DIRECT_PROBATION as u64 - 1)],
        &[],
    );

    request_unflip(&mut fixture, &direct);
    let cleanup_offset = record_count(&fixture, device_fd);
    let cleanup = grouped_unflip_record(&mut fixture, device_fd, cleanup_offset, 2);
    let cleanup_id = accepted_event_id(&cleanup).unwrap();
    complete_recorded_attempt(&mut fixture, device_fd, &cleanup, cleanup_id, 105);
    assert!(fixture.events.backend.present_source_pins.is_empty());
    drain_present_notifications(
        &mut fixture,
        &[],
        &[admitted_reentry_id + (DIRECT_PROBATION as u64 - 1)],
    );
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: omit post-tick registration of an already-signalled copied replacement accepted inside scene.tick.
fn p1_degraded_copied_unflip_registers_immediately_accepted_replacement() {
    let Some((mut fixture, direct)) = direct_fixture(1, P1SceneRoute::CopiedSameDevice, 206, 0)
    else {
        return;
    };
    let device_fd = fd(&fixture);
    let initial_attempts = submit_direct(&mut fixture, &direct, device_fd);
    let initial = accepted_records(&initial_attempts).remove(0);
    let direct_id = accepted_event_id(&initial).unwrap();
    complete_direct_initial(&mut fixture, &initial, direct_id, device_fd);
    drain_present_notifications(&mut fixture, &[direct.candidate.present_id], &[]);
    request_unflip(&mut fixture, &direct);
    fixture.events.recorders[&device_fd]
        .script_result(Err(std::io::Error::from_raw_os_error(libc::EIO)));
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("degrade copied grouped unflip and tick scene");

    let deadline = Instant::now() + WAIT_DEADLINE;
    let replacement = loop {
        let Some(replacement) = records_since(&fixture, device_fd, 0)
            .into_iter()
            .rev()
            .find(|record| {
                record.events.len() == 1
                    && record.device_fd == device_fd
                    && accepted_event_id(record).is_some()
                    && record.kind != crate::drm::atomic::AtomicCommitKind::Async
            })
            .filter(|record| record.user_data != direct_id)
        else {
            assert!(
                Instant::now() < deadline,
                "scene tick did not accept a copied replacement"
            );
            Backend::before_block(&mut fixture.events.backend);
            let root = fixture.events.backend.core.window_id;
            let width = fixture.events.backend.platform.fb_w;
            let height = fixture.events.backend.platform.fb_h;
            Backend::clear_area(
                &mut fixture.events.backend,
                None,
                root,
                0,
                None,
                0,
                0,
                width,
                height,
                (0, 0),
            )
            .expect("damage copied replacement through Backend");
            Backend::maybe_composite(&mut fixture.events.backend)
                .expect("tick copied replacement through Backend");
            continue;
        };
        if replacement.events[0].crtc == fixture.events.backend.platform.outputs[0].output.crtc
            && replacement.user_data != direct_id
        {
            break replacement;
        }
        assert!(
            Instant::now() < deadline,
            "copied replacement event targeted another CRTC"
        );
    };
    let replacement_id = accepted_event_id(&replacement).unwrap();
    assert_eq!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .unflip_awaiting_outputs,
        HashMap::from([(0, replacement_id)]),
        "scene tick registers the accepted sink ID before returning"
    );
    assert_eq!(
        fixture.events.backend.scene.pending_page_flip_event_id(0),
        Some(replacement_id)
    );
    assert!(fixture.pending_render_job_ids().is_empty());

    fixture.events.queue_commit_completion(
        device_fd,
        replacement.events[0].crtc,
        direct_id,
        110,
        Duration::from_millis(11_000),
    );
    Backend::on_page_flip_ready(
        &mut fixture.events.backend,
        &mut fixture.events.state,
        device_fd,
    );
    assert_eq!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .unflip_awaiting_outputs,
        HashMap::from([(0, replacement_id)])
    );
    complete_recorded_attempt(&mut fixture, device_fd, &replacement, replacement_id, 111);
    assert!(!fixture.events.backend.scene.has_pending_page_flips());
    assert!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .unflip_outputs_to_replace
            .is_empty()
    );
    assert!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .unflip_awaiting_outputs
            .is_empty()
    );
    assert!(fixture.events.backend.present_source_pins.is_empty());
    assert!(
        fixture
            .copy_ownership_observation()
            .iter()
            .all(|row| { !(row.contains("phase=Free") && row.contains("ForeignPendingKms")) })
    );
    drain_present_notifications(&mut fixture, &[], &[direct.candidate.present_id]);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: record source-render readiness as a sink ID; select the grouped rejection instead of the rejected sink submit.
fn p1_degraded_copied_unflip_has_no_id_before_sink_acceptance() {
    let Some((mut fixture, direct)) = direct_fixture(1, P1SceneRoute::CopiedSameDevice, 207, 0)
    else {
        return;
    };
    let readiness = fixture.readiness.as_ref().unwrap();
    readiness.hold_ready_jobs();
    let device_fd = fd(&fixture);
    let initial_attempts = submit_direct(&mut fixture, &direct, device_fd);
    let initial = accepted_records(&initial_attempts).remove(0);
    let direct_id = accepted_event_id(&initial).unwrap();
    complete_direct_initial(&mut fixture, &initial, direct_id, device_fd);
    drain_present_notifications(&mut fixture, &[direct.candidate.present_id], &[]);
    request_unflip(&mut fixture, &direct);
    let recorder = fixture.events.recorders[&device_fd].clone();
    recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EIO)));
    let start = record_count(&fixture, device_fd);
    Backend::maybe_composite(&mut fixture.events.backend).expect("degrade grouped copied unflip");
    let job_id = fixture
        .pending_render_job_ids()
        .last()
        .copied()
        .unwrap_or_else(|| compose_copied_job(&mut fixture));
    assert!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .unflip_awaiting_outputs
            .is_empty(),
        "source render readiness must not create an accepted sink identity"
    );
    assert_eq!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .unflip_outputs_to_replace,
        HashSet::from([0])
    );
    assert_eq!(
        fixture.events.backend.scene.pending_page_flip_event_id(0),
        None
    );
    assert_eq!(direct_source_pins(&fixture, direct.source_id), 1);
    assert_eq!(fixture.events.backend.present_source_pins.len(), 2);
    assert!(
        records_since(&fixture, device_fd, start)
            .iter()
            .all(|record| accepted_event_id(record).is_none())
    );

    recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EINVAL)));
    let sink_start = record_count(&fixture, device_fd);
    deliver_copied_job(&mut fixture, job_id);
    let sink_attempts = records_since(&fixture, device_fd, sink_start);
    assert_eq!(
        sink_attempts.len(),
        1,
        "the selector begins immediately before the rejected sink submission"
    );
    let rejected = sink_attempts
        .into_iter()
        .find(|record| record.events.len() == 1 && record.result != ScriptedResult::Ok)
        .expect("sink acceptance rejected");
    assert!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .unflip_awaiting_outputs
            .is_empty()
    );
    assert_eq!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .unflip_outputs_to_replace,
        HashSet::from([0])
    );
    assert_eq!(
        fixture.events.backend.scene.pending_page_flip_event_id(0),
        None
    );

    let accepted_start = record_count(&fixture, device_fd);
    let next_job = compose_copied_job(&mut fixture);
    deliver_copied_job(&mut fixture, next_job);
    let accepted = records_since(&fixture, device_fd, accepted_start)
        .into_iter()
        .find(|record| accepted_event_id(record).is_some())
        .expect("later copied sink request accepted");
    let accepted_id = accepted_event_id(&accepted).unwrap();
    assert_ne!(accepted_id, rejected.user_data);
    assert_eq!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .unflip_awaiting_outputs,
        HashMap::from([(0, accepted_id)])
    );
    assert_eq!(
        fixture.events.backend.scene.pending_page_flip_event_id(0),
        Some(accepted_id)
    );

    fixture.events.queue_commit_completion(
        device_fd,
        accepted.events[0].crtc,
        direct_id,
        120,
        Duration::from_millis(12_000),
    );
    Backend::on_page_flip_ready(
        &mut fixture.events.backend,
        &mut fixture.events.state,
        device_fd,
    );
    assert_eq!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .unflip_awaiting_outputs,
        HashMap::from([(0, accepted_id)])
    );
    complete_recorded_attempt(&mut fixture, device_fd, &accepted, accepted_id, 121);
    assert!(!fixture.events.backend.scene.has_pending_page_flips());
    assert!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .unflip_outputs_to_replace
            .is_empty()
    );
    assert!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .unflip_awaiting_outputs
            .is_empty()
    );
    assert!(fixture.events.backend.present_source_pins.is_empty());
    assert!(fixture.pending_render_job_ids().is_empty());
    assert!(
        fixture
            .copy_ownership_observation()
            .iter()
            .all(|row| { !(row.contains("phase=Free") && row.contains("ForeignPendingKms")) })
    );
    drain_present_notifications(&mut fixture, &[], &[direct.candidate.present_id]);
    fixture.finish_after_normal_drain();
}
