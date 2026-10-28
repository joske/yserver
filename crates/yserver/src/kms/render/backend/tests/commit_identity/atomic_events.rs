use super::{
    recording_scene::RecordingSceneFixture,
    support::{AtomicEventFixture, accepted_event_id},
};
use crate::{
    drm::atomic::{
        AtomicCommitEvent, AtomicCommitKind,
        tests_support::{AtomicCommitRecord, ScriptedResult},
    },
    kms::render::platform::CrtcKey,
};
use std::{
    os::fd::{AsFd as _, AsRawFd as _, RawFd},
    time::Duration,
};
use yserver_core::backend::Backend;

fn raw<H: ::drm::control::ResourceHandle>(value: u32) -> H {
    ::drm::control::from_u32(value).expect("nonzero test handle")
}

fn record(
    user_data: u64,
    flags: ::drm::control::AtomicCommitFlags,
    events: Vec<AtomicCommitEvent>,
    result: ScriptedResult,
) -> AtomicCommitRecord {
    AtomicCommitRecord {
        flags,
        kind: AtomicCommitKind::Scene,
        properties: Vec::new(),
        events,
        result,
        device_fd: 7,
        user_data,
    }
}

pub(super) fn submit_damaged_frame(
    fixture: &mut RecordingSceneFixture,
) -> Vec<(RawFd, AtomicCommitRecord)> {
    let offsets: std::collections::HashMap<_, _> = fixture
        .events
        .recorders
        .iter()
        .map(|(&fd, recorder)| (fd, recorder.records().len()))
        .collect();
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
    Backend::maybe_composite(&mut fixture.events.backend).expect("compose through Backend");
    let attempts: Vec<_> = fixture
        .events
        .recorders
        .iter()
        .flat_map(|(&fd, recorder)| {
            recorder
                .records()
                .into_iter()
                .skip(offsets[&fd])
                .filter(move |record| {
                    record.device_fd == fd
                        && record
                            .flags
                            .contains(::drm::control::AtomicCommitFlags::PAGE_FLIP_EVENT)
                        && !record
                            .flags
                            .contains(::drm::control::AtomicCommitFlags::TEST_ONLY)
                        && !record.events.is_empty()
                })
                .map(move |record| (fd, record))
        })
        .collect();
    assert!(
        !attempts.is_empty(),
        "a damaged Backend cycle must issue an event-producing scene request"
    );
    for (_, attempt) in &attempts {
        assert_ne!(
            attempt.user_data, 0,
            "event request must reach recorder with cookie"
        );
    }
    attempts
}

pub(crate) fn complete_recorded_attempt(
    fixture: &mut RecordingSceneFixture,
    fd: RawFd,
    record: &AtomicCommitRecord,
    cookie: u64,
    frame: u32,
) {
    // The kernel retires a flip only after the frame's IN_FENCE_FD (the GPU
    // work) signalled; stand in for that before injecting its completion.
    fixture.events.wait_for_submitted_gpu_work();
    for event in &record.events {
        fixture.events.queue_commit_completion(
            fd,
            event.crtc,
            cookie,
            frame,
            Duration::from_millis(u64::from(frame) * 10),
        );
    }
    Backend::on_page_flip_ready(&mut fixture.events.backend, &mut fixture.events.state, fd);
}

pub(super) fn pending_ids(
    fixture: &RecordingSceneFixture,
    fd: RawFd,
    crtc: ::drm::control::crtc::Handle,
) -> Vec<u64> {
    let device = fixture
        .events
        .backend
        .platform
        .devices
        .iter()
        .find(|device| device.device.as_fd().as_raw_fd() == fd)
        .expect("pending-ID device fd exists");
    device
        .atomic_commit_sink
        .for_tests_pending_page_flip_commit_ids(&device.device, crtc)
}

fn one_recording_scene() -> Option<RecordingSceneFixture> {
    RecordingSceneFixture::new(1).expect("recording scene fixture construction")
}

struct LogicalVtResume;

impl crate::kms::render::backend::kms::session::VtResumeExecutor for LogicalVtResume {
    fn acquire_master(
        &self,
        _device: &crate::kms::render::platform::KmsDevice,
    ) -> std::io::Result<()> {
        Ok(())
    }

    fn release_master(
        &self,
        _device: &crate::kms::render::platform::KmsDevice,
    ) -> std::io::Result<()> {
        Ok(())
    }

    fn probe_connector_snapshot(
        &self,
        platform: &crate::kms::render::platform::PlatformBackend,
    ) -> std::io::Result<Vec<crate::kms::render::platform::ConnectorSnapshot>> {
        Ok(platform
            .outputs
            .iter()
            .map(|output| crate::kms::render::platform::ConnectorSnapshot {
                key: output.key.clone(),
                modes: vec![output.output.picked.clone()],
                mm_width: output.output.mm_width,
                mm_height: output.output.mm_height,
                edid: output.output.edid.clone(),
                connector_type: output.output.connector_type.clone(),
                vrr_capable: output.output.connector_vrr_capable,
            })
            .collect())
    }
}

pub(in crate::kms::render::backend::tests) fn install_recording_vt_io(
    fixture: &mut RecordingSceneFixture,
) {
    fixture
        .events
        .backend
        .platform
        .for_tests_accept_output_power_commits();
    fixture
        .events
        .backend
        .for_tests_set_vt_resume_executor(std::rc::Rc::new(LogicalVtResume));
}

pub(super) fn assert_pending_bo_phases(
    fixture: &RecordingSceneFixture,
) -> Vec<Vec<crate::kms::vk::scanout::BoPhase>> {
    fixture
        .events
        .backend
        .platform
        .scanout_pools
        .iter()
        .map(|pool| {
            pool.as_ref()
                .map(|pool| {
                    pool.display_pool()
                        .bos
                        .iter()
                        .map(|bo| bo.state.phase)
                        .collect()
                })
                .unwrap_or_default()
        })
        .collect()
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: allocate after executor invocation, enqueue a rejected request, or reuse its ID.
fn p1_submit_allocates_cookie_before_executor_and_only_enqueues_success() {
    let Some(mut fixture) =
        RecordingSceneFixture::new(2).expect("two-output recording scene fixture construction")
    else {
        return;
    };
    let fd = fixture.events.backend.platform.devices[0]
        .device
        .as_fd()
        .as_raw_fd();
    fixture.events.recorders[&fd].script_result(Ok(()));
    fixture.events.recorders[&fd]
        .script_result(Err(std::io::Error::from_raw_os_error(libc::EINVAL)));
    let first_attempts = submit_damaged_frame(&mut fixture);
    assert_eq!(first_attempts.len(), 2);
    let accepted_attempt = first_attempts
        .iter()
        .find(|(_, record)| accepted_event_id(record).is_some())
        .expect("one first scene request accepted");
    let rejected = first_attempts
        .iter()
        .find(|(_, record)| record.result != ScriptedResult::Ok)
        .expect("one first scene request rejected");
    let first_id = accepted_event_id(&accepted_attempt.1).expect("first scene request accepted");
    let first_crtc = accepted_attempt.1.events[0].crtc;
    let rejected_crtc = rejected.1.events[0].crtc;
    let rejected_id = rejected.1.user_data;
    assert_ne!(rejected_id, 0, "rejected attempt still consumed a cookie");
    assert_eq!(
        rejected.1.result,
        ScriptedResult::Error {
            kind: std::io::ErrorKind::InvalidInput,
            raw_os_error: Some(libc::EINVAL),
        }
    );
    assert_eq!(pending_ids(&fixture, fd, first_crtc), [first_id]);
    assert!(pending_ids(&fixture, fd, rejected_crtc).is_empty());
    complete_recorded_attempt(&mut fixture, fd, &accepted_attempt.1, first_id, 51);

    let second = submit_damaged_frame(&mut fixture);
    let later = second
        .iter()
        .find(|(_, record)| accepted_event_id(record).is_some())
        .expect("a later scene request is accepted");
    let second_id = accepted_event_id(&later.1).expect("later scene request accepted");
    let second_crtc = later.1.events[0].crtc;
    assert_ne!(first_id, rejected_id);
    assert_ne!(
        rejected_id, second_id,
        "failed-attempt cookie cannot be reused for later accepted work"
    );
    assert_ne!(first_id, second_id);
    assert_eq!(pending_ids(&fixture, fd, second_crtc), [second_id]);

    fixture.events.queue_commit_completion(
        fd,
        second_crtc,
        rejected_id,
        52,
        Duration::from_millis(520),
    );
    Backend::on_page_flip_ready(&mut fixture.events.backend, &mut fixture.events.state, fd);
    assert_eq!(pending_ids(&fixture, fd, second_crtc), [second_id]);
    assert!(fixture.events.backend.scene.has_pending_page_flips());
    for (offset, (_, attempt)) in second.iter().enumerate() {
        let Some(id) = accepted_event_id(attempt) else {
            continue;
        };
        complete_recorded_attempt(
            &mut fixture,
            fd,
            attempt,
            id,
            53 + u32::try_from(offset).expect("attempt index fits"),
        );
    }
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: on an explicit-cookie miss, remove the queue front and attribute it to the new frame.
fn p1_explicit_cookie_duplicate_and_unknown_retire_nothing() {
    let Some(mut fixture) = one_recording_scene() else {
        return;
    };
    let fd = fixture.events.backend.platform.devices[0]
        .device
        .as_fd()
        .as_raw_fd();
    let first = submit_damaged_frame(&mut fixture);
    let first_id = accepted_event_id(&first[0].1).expect("first scene request accepted");
    let crtc = first[0].1.events[0].crtc;
    complete_recorded_attempt(&mut fixture, fd, &first[0].1, first_id, 61);

    let next = submit_damaged_frame(&mut fixture);
    let next_id = accepted_event_id(&next[0].1).expect("next scene request accepted");
    assert_ne!(first_id, next_id);
    let phases_before = assert_pending_bo_phases(&fixture);
    fixture
        .events
        .queue_commit_completion(fd, crtc, first_id, 62, Duration::from_millis(620));
    fixture
        .events
        .queue_commit_completion(fd, crtc, u64::MAX, 63, Duration::from_millis(630));
    Backend::on_page_flip_ready(&mut fixture.events.backend, &mut fixture.events.state, fd);
    assert_eq!(pending_ids(&fixture, fd, crtc), [next_id]);
    assert!(fixture.events.backend.scene.has_pending_page_flips());
    assert_eq!(assert_pending_bo_phases(&fixture), phases_before);

    complete_recorded_attempt(&mut fixture, fd, &next[0].1, next_id, 64);
    assert!(pending_ids(&fixture, fd, crtc).is_empty());
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: disable zero-cookie FIFO attribution so the accepted scene obligation remains pending.
// Mutation: a zero-cookie match removes the entry but returns None; the scene frame then stays pending.
fn p1_zero_cookie_preserves_fifo_attribution() {
    let Some(mut fixture) = one_recording_scene() else {
        return;
    };
    let fd = fixture.events.backend.platform.devices[0]
        .device
        .as_fd()
        .as_raw_fd();
    let sink = fixture.events.backend.platform.devices[0]
        .atomic_commit_sink
        .clone();
    let unmatched_before = sink.for_tests_unmatched_page_flip_event_count();
    let accepted = submit_damaged_frame(&mut fixture);
    assert_eq!(accepted.len(), 1);
    assert_eq!(accepted[0].1.kind, AtomicCommitKind::Scene);
    let id = accepted_event_id(&accepted[0].1).expect("scene request accepted");
    let crtc = accepted[0].1.events[0].crtc;
    assert_eq!(pending_ids(&fixture, fd, crtc), [id]);
    assert_eq!(
        fixture.events.backend.scene.pending_page_flip_event_id(0),
        Some(id)
    );

    fixture
        .events
        .queue_commit_completion(fd, crtc, 0, 71, Duration::from_millis(710));
    Backend::on_page_flip_ready(&mut fixture.events.backend, &mut fixture.events.state, fd);
    assert!(pending_ids(&fixture, fd, crtc).is_empty());
    // The zero cookie matched the oldest entry and handed its id to scene
    // retirement, which retired the accepted frame through Backend work.
    assert!(
        !fixture.events.backend.scene.has_pending_page_flips(),
        "a zero-cookie completion must retire the accepted scene frame"
    );
    assert_eq!(
        fixture.events.backend.scene.pending_page_flip_event_id(0),
        None
    );
    assert_eq!(
        sink.for_tests_unmatched_page_flip_event_count(),
        unmatched_before,
        "the zero cookie matched the accepted queue entry instead of being unmatched"
    );
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: key associations by CRTC alone or drain device B's queue on device A readiness.
fn p1_identical_crtc_handles_are_device_qualified() {
    let Some(mut fixture) = RecordingSceneFixture::new_with_identical_crtcs_on_two_devices()
        .expect("two-device recording scene fixture construction")
    else {
        return;
    };
    let fd_a = fixture.events.backend.platform.devices[0]
        .device
        .as_fd()
        .as_raw_fd();
    let fd_b = fixture.events.backend.platform.devices[1]
        .device
        .as_fd()
        .as_raw_fd();
    assert_ne!(fd_a, fd_b);
    let shared_recorder = std::rc::Rc::new(
        crate::drm::atomic::tests_support::RecordingAtomicCommitExecutor::default(),
    );
    let shared_sink = std::rc::Rc::new(crate::drm::atomic::AtomicCommitSink::new(
        shared_recorder.clone(),
    ));
    for device in &mut fixture.events.backend.platform.devices {
        device.atomic_commit_sink = shared_sink.clone();
    }
    fixture.events.recorders.clear();
    fixture
        .events
        .recorders
        .insert(fd_a, shared_recorder.clone());
    fixture
        .events
        .recorders
        .insert(fd_b, shared_recorder.clone());

    let attempts = submit_damaged_frame(&mut fixture);
    assert_eq!(
        attempts.len(),
        2,
        "ordinary scene requests target both devices"
    );
    let attempt_a = attempts.iter().find(|(fd, _)| *fd == fd_a).unwrap();
    let attempt_b = attempts.iter().find(|(fd, _)| *fd == fd_b).unwrap();
    let id_a = accepted_event_id(&attempt_a.1).expect("device A request accepted");
    let id_b = accepted_event_id(&attempt_b.1).expect("device B request accepted");
    assert_ne!(id_a, id_b);
    let crtc = attempt_a.1.events[0].crtc;
    assert_eq!(attempt_b.1.events[0].crtc, crtc);
    assert_eq!(pending_ids(&fixture, fd_a, crtc), [id_a]);
    assert_eq!(pending_ids(&fixture, fd_b, crtc), [id_b]);

    complete_recorded_attempt(&mut fixture, fd_a, &attempt_a.1, id_a, 81);
    assert!(pending_ids(&fixture, fd_a, crtc).is_empty());
    assert_eq!(pending_ids(&fixture, fd_b, crtc), [id_b]);
    assert!(fixture.events.backend.scene.has_pending_page_flips());
    complete_recorded_attempt(&mut fixture, fd_b, &attempt_b.1, id_b, 82);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: match a cookie anywhere on the device instead of requiring its event CRTC.
fn p1_cookie_on_wrong_crtc_leaves_original_obligation() {
    let Some(mut fixture) =
        RecordingSceneFixture::new(2).expect("two-output recording scene fixture construction")
    else {
        return;
    };
    let fd = fixture.events.backend.platform.devices[0]
        .device
        .as_fd()
        .as_raw_fd();
    let attempts = submit_damaged_frame(&mut fixture);
    assert_eq!(
        attempts.len(),
        2,
        "ordinary scene requests target both CRTCs"
    );
    let attempt_a = attempts
        .iter()
        .find(|(_, record)| {
            record.events[0].crtc == fixture.events.backend.platform.outputs[0].output.crtc
        })
        .unwrap();
    let attempt_b = attempts
        .iter()
        .find(|(_, record)| {
            record.events[0].crtc == fixture.events.backend.platform.outputs[1].output.crtc
        })
        .unwrap();
    let id_a = accepted_event_id(&attempt_a.1).expect("CRTC A request accepted");
    let id_b = accepted_event_id(&attempt_b.1).expect("CRTC B request accepted");
    assert_ne!(id_a, id_b);
    let crtc_a = attempt_a.1.events[0].crtc;
    let crtc_b = attempt_b.1.events[0].crtc;
    let phases_before = assert_pending_bo_phases(&fixture);

    fixture
        .events
        .queue_commit_completion(fd, crtc_b, id_a, 91, Duration::from_millis(910));
    Backend::on_page_flip_ready(&mut fixture.events.backend, &mut fixture.events.state, fd);
    assert_eq!(pending_ids(&fixture, fd, crtc_a), [id_a]);
    assert_eq!(pending_ids(&fixture, fd, crtc_b), [id_b]);
    assert!(fixture.events.backend.scene.has_pending_page_flips());
    assert_eq!(assert_pending_bo_phases(&fixture), phases_before);

    complete_recorded_attempt(&mut fixture, fd, &attempt_b.1, id_b, 92);
    assert_eq!(pending_ids(&fixture, fd, crtc_a), [id_a]);
    assert!(pending_ids(&fixture, fd, crtc_b).is_empty());
    complete_recorded_attempt(&mut fixture, fd, &attempt_a.1, id_a, 93);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: drop unmatched flips before platform clock recording or count them as matched samples.
fn p1_unmatched_cookie_still_records_clock_without_retirement() {
    let Some(mut fixture) = one_recording_scene() else {
        return;
    };
    let fd = fixture.events.backend.platform.devices[0]
        .device
        .as_fd()
        .as_raw_fd();
    let attempt = submit_damaged_frame(&mut fixture);
    let accepted = accepted_event_id(&attempt[0].1).expect("scene request accepted");
    let crtc = attempt[0].1.events[0].crtc;
    let device_key = fixture.events.backend.platform.devices[0].key;
    let before_latency = fixture.events.backend.platform.devices[0]
        .atomic_commit_sink
        .for_tests_present_latency_state();
    let before_unmatched = fixture.events.backend.platform.devices[0]
        .atomic_commit_sink
        .for_tests_unmatched_page_flip_event_count();

    fixture.events.queue_commit_completion(
        fd,
        crtc,
        accepted.wrapping_add(0x1_0000),
        99,
        Duration::from_millis(990),
    );
    Backend::on_page_flip_ready(&mut fixture.events.backend, &mut fixture.events.state, fd);
    assert_eq!(
        fixture
            .events
            .backend
            .platform
            .present_get_ust_msc(CrtcKey::new(device_key, crtc)),
        (99, 990_000),
        "unmatched event still advances the platform clock"
    );
    assert_eq!(pending_ids(&fixture, fd, crtc), [accepted]);
    assert!(fixture.events.backend.scene.has_pending_page_flips());
    assert_eq!(
        fixture.events.backend.platform.devices[0]
            .atomic_commit_sink
            .for_tests_unmatched_page_flip_event_count(),
        before_unmatched + 1,
        "unmatched explicit identity must leave an observable diagnostic"
    );
    assert_eq!(
        fixture.events.backend.platform.devices[0]
            .atomic_commit_sink
            .for_tests_present_latency_state(),
        before_latency,
        "unmatched identity cannot add a matched Present-latency sample"
    );

    complete_recorded_attempt(&mut fixture, fd, &attempt[0].1, accepted, 100);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: have the live DRM event source discard user_data after the raw decoder.
fn p1_raw_fd_event_path_preserves_explicit_cookie() {
    let Some(mut fixture) = one_recording_scene() else {
        return;
    };
    let fd = fixture.events.use_raw_event_source_for_device(0);
    let attempt = submit_damaged_frame(&mut fixture);
    assert_eq!(attempt.len(), 1);
    assert_eq!(attempt[0].0, fd);
    let accepted = accepted_event_id(&attempt[0].1).expect("scene request accepted");
    let crtc = attempt[0].1.events[0].crtc;
    assert_eq!(pending_ids(&fixture, fd, crtc), [accepted]);

    fixture.events.queue_commit_completion(
        fd,
        crtc,
        accepted.wrapping_add(0x1_0000),
        111,
        Duration::from_millis(1110),
    );
    Backend::on_page_flip_ready(&mut fixture.events.backend, &mut fixture.events.state, fd);
    assert_eq!(
        pending_ids(&fixture, fd, crtc),
        [accepted],
        "raw decoder must preserve the unmatched explicit cookie"
    );
    assert!(fixture.events.backend.scene.has_pending_page_flips());

    complete_recorded_attempt(&mut fixture, fd, &attempt[0].1, accepted, 112);
    assert!(pending_ids(&fixture, fd, crtc).is_empty());
    assert!(!fixture.events.backend.scene.has_pending_page_flips());
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: reset the allocator or retain the old pending association across the all-off boundary.
fn p1_discarded_stream_old_cookie_cannot_match_new_work() {
    use yserver_core::core_loop::input_inventory::InputInventory;

    let Some(mut fixture) = one_recording_scene() else {
        return;
    };
    install_recording_vt_io(&mut fixture);
    let fd = fixture.events.backend.platform.devices[0]
        .device
        .as_fd()
        .as_raw_fd();
    let old = submit_damaged_frame(&mut fixture);
    let old_id = accepted_event_id(&old[0].1).expect("pre-release scene request accepted");
    let crtc = old[0].1.events[0].crtc;
    fixture
        .events
        .queue_commit_completion(fd, crtc, old_id, 121, Duration::from_millis(1210));

    let _ = Backend::begin_vt_release(&mut fixture.events.backend);
    Backend::finish_vt_release(
        &mut fixture.events.backend,
        &mut fixture.events.state,
        &InputInventory::default(),
    );
    assert!(!fixture.events.backend.scene.has_pending_page_flips());
    assert!(pending_ids(&fixture, fd, crtc).is_empty());
    assert_eq!(fixture.events.event_sources[&fd].queued_flip_count(), 0);

    Backend::on_vt_acquire(&mut fixture.events.backend, &mut fixture.events.state);
    let new = submit_damaged_frame(&mut fixture);
    let new_id = accepted_event_id(&new[0].1).expect("post-acquire scene request accepted");
    assert_ne!(
        old_id, new_id,
        "allocator must survive event-stream discard"
    );
    assert_eq!(pending_ids(&fixture, fd, crtc), [new_id]);
    fixture
        .events
        .queue_commit_completion(fd, crtc, old_id, 122, Duration::from_millis(1220));
    Backend::on_page_flip_ready(&mut fixture.events.backend, &mut fixture.events.state, fd);
    assert_eq!(pending_ids(&fixture, fd, crtc), [new_id]);
    assert!(fixture.events.backend.scene.has_pending_page_flips());

    complete_recorded_attempt(&mut fixture, fd, &new[0].1, new_id, 123);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: omit inactive-path attribution or skip valid sequence-event handling.
fn p1_inactive_drain_filters_unmatched_explicit_cookie() {
    use yserver_core::core_loop::input_inventory::InputInventory;

    let Some(mut fixture) = one_recording_scene() else {
        return;
    };
    install_recording_vt_io(&mut fixture);
    let fd = fixture.events.backend.platform.devices[0]
        .device
        .as_fd()
        .as_raw_fd();
    let crtc = fixture.events.backend.platform.outputs[0].output.crtc;
    let source = fixture.events.event_sources[&fd].clone();

    let _ = Backend::begin_vt_release(&mut fixture.events.backend);
    Backend::finish_vt_release(
        &mut fixture.events.backend,
        &mut fixture.events.state,
        &InputInventory::default(),
    );
    assert!(!fixture.events.backend.scanout_allowed());
    let records_before = fixture.events.recorders[&fd].records().len();
    let unmatched_before = fixture.events.backend.platform.devices[0]
        .atomic_commit_sink
        .for_tests_unmatched_page_flip_event_count();
    let key = CrtcKey::new(fixture.events.backend.platform.devices[0].key, crtc);
    fixture.events.queue_commit_completion(
        fd,
        crtc,
        0xDEAD_BEEF_1234_5678,
        100,
        Duration::from_millis(1300),
    );
    fixture
        .events
        .queue_sequence_completion(fd, u64::from(u32::from(crtc)), 1_234_000, 101);
    Backend::on_page_flip_ready(&mut fixture.events.backend, &mut fixture.events.state, fd);
    assert_eq!(
        fixture.events.recorders[&fd].records().len(),
        records_before
    );
    assert_eq!(
        fixture.events.backend.platform.devices[0]
            .atomic_commit_sink
            .for_tests_unmatched_page_flip_event_count(),
        unmatched_before + 1,
        "inactive drain must report its unmatched explicit cookie"
    );
    assert!(pending_ids(&fixture, fd, crtc).is_empty());
    assert!(!fixture.events.backend.scene.has_pending_page_flips());
    assert_eq!(
        fixture.events.backend.platform.present_get_ust_msc(key),
        (101, 1234),
        "valid sequence event still advances the inactive device clock"
    );
    assert_eq!(source.queued_flip_count(), 0);

    Backend::on_vt_acquire(&mut fixture.events.backend, &mut fixture.events.state);
    let accepted = submit_damaged_frame(&mut fixture);
    let id = accepted_event_id(&accepted[0].1).expect("post-acquire scene request accepted");
    complete_recorded_attempt(&mut fixture, fd, &accepted[0].1, id, 102);
    fixture.finish_after_normal_drain();
}

#[test]
// Mutation: drain every device's event source on one DRM-fd readiness notification.
fn p1_fixture_routes_events_only_from_the_ready_fd() {
    use std::os::fd::AsRawFd as _;

    let mut fixture = AtomicEventFixture::new(2);
    let crtc = raw(1);
    let fd_a = fixture.backend.platform.devices[0]
        .device
        .as_fd()
        .as_raw_fd();
    let fd_b = fixture.backend.platform.devices[1]
        .device
        .as_fd()
        .as_raw_fd();
    let key_a = fixture.backend.platform.devices[0].key;
    let key_b = fixture.backend.platform.devices[1].key;
    assert_ne!(fd_a, fd_b);
    assert_eq!(
        u32::from(crtc),
        u32::from(fixture.backend.platform.outputs[0].output.crtc)
    );
    assert_eq!(
        u32::from(crtc),
        u32::from(fixture.backend.platform.outputs[1].output.crtc)
    );

    fixture.queue_commit_completion(fd_a, crtc, 0x1_0000_0001, 31, Duration::from_millis(900));
    fixture.queue_commit_completion(fd_b, crtc, 0x2_0000_0002, 42, Duration::from_millis(1200));
    assert_eq!(fixture.event_sources[&fd_a].queued_flip_count(), 1);
    assert_eq!(fixture.event_sources[&fd_b].queued_flip_count(), 1);

    Backend::on_page_flip_ready(&mut fixture.backend, &mut fixture.state, fd_a);

    assert_eq!(
        fixture
            .backend
            .platform
            .present_get_ust_msc(CrtcKey::new(key_a, crtc)),
        (31, 900_000)
    );
    assert_eq!(
        fixture
            .backend
            .platform
            .present_get_ust_msc(CrtcKey::new(key_b, crtc)),
        (0, 0)
    );
    assert_eq!(fixture.event_sources[&fd_a].queued_flip_count(), 0);
    assert_eq!(fixture.event_sources[&fd_b].queued_flip_count(), 1);

    Backend::on_page_flip_ready(&mut fixture.backend, &mut fixture.state, fd_b);

    assert_eq!(
        fixture
            .backend
            .platform
            .present_get_ust_msc(CrtcKey::new(key_b, crtc)),
        (42, 1_200_000)
    );
    assert_eq!(fixture.event_sources[&fd_a].queued_flip_count(), 0);
    assert_eq!(fixture.event_sources[&fd_b].queued_flip_count(), 0);
    fixture.assert_quiescent();
}

#[test]
// Mutation: accept rejected/TEST_ONLY cookies or truncate accepted cookies to u32.
fn p1_fixture_acceptance_reader_excludes_nonaccepted_records() {
    use ::drm::control::AtomicCommitFlags as Flags;

    let event = vec![AtomicCommitEvent {
        crtc: raw(1),
        motion_timestamp: None,
    }];
    let cookie = 0x1_0000_0005;
    let success = record(
        cookie,
        Flags::PAGE_FLIP_EVENT,
        event.clone(),
        ScriptedResult::Ok,
    );
    let rejected = record(
        cookie,
        Flags::PAGE_FLIP_EVENT,
        event.clone(),
        ScriptedResult::Error {
            kind: std::io::ErrorKind::InvalidInput,
            raw_os_error: Some(libc::EINVAL),
        },
    );
    let test_only = record(
        cookie,
        Flags::PAGE_FLIP_EVENT | Flags::TEST_ONLY,
        event.clone(),
        ScriptedResult::Ok,
    );
    let no_event = record(cookie, Flags::NONBLOCK, Vec::new(), ScriptedResult::Ok);
    let zero_cookie = record(0, Flags::PAGE_FLIP_EVENT, event, ScriptedResult::Ok);

    assert_eq!(accepted_event_id(&success), Some(cookie));
    assert_eq!(accepted_event_id(&rejected), None);
    assert_eq!(accepted_event_id(&test_only), None);
    assert_eq!(accepted_event_id(&no_event), None);
    assert_eq!(accepted_event_id(&zero_cookie), None);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: replace the live scene with a fake flip override, omit backend
// event delivery, or suppress re-damage during deferred-release draining.
fn p1_recording_scene_smoke_completes_through_backend() {
    use std::os::fd::{AsFd as _, AsRawFd as _};

    let Some(mut fixture) = super::recording_scene::RecordingSceneFixture::new(1)
        .expect("recording scene fixture construction")
    else {
        return;
    };
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
    Backend::maybe_composite(&mut fixture.events.backend).expect("compose through Backend");

    let fd = fixture.events.backend.platform.devices[0]
        .device
        .as_fd()
        .as_raw_fd();
    let records = fixture.events.recorders[&fd].records();
    assert!(
        !records.is_empty(),
        "the real atomic recorder must see submit"
    );
    assert!(records.iter().all(|record| record.device_fd == fd));
    assert!(fixture.events.backend.scene.has_pending_page_flips());
    for record in records
        .iter()
        .filter(|record| accepted_event_id(record).is_some())
    {
        let event_id = accepted_event_id(record).expect("accepted KMS event has a cookie");
        for event in &record.events {
            fixture.events.queue_commit_completion(
                fd,
                event.crtc,
                event_id,
                51,
                Duration::from_millis(1500),
            );
        }
    }
    assert_eq!(fixture.events.event_sources[&fd].queued_flip_count(), 1);
    Backend::on_page_flip_ready(&mut fixture.events.backend, &mut fixture.events.state, fd);
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("drain normal scene completion work");
    // The helper drives a new damaged frame so SceneCompositor::tick reaches
    // its production deferred-release drain point.
    fixture.drain_deferred_pool_releases(Duration::from_secs(5));

    fixture.assert_quiescent();
    assert!(
        fixture
            .events
            .backend
            .platform
            .scanout_pools
            .iter()
            .filter_map(Option::as_ref)
            .flat_map(|scanout| scanout.display_pool().bos.iter())
            .all(|bo| bo.state.phase != crate::kms::vk::scanout::BoPhase::Pending),
        "normal Backend retirement must leave no Pending scanout BO"
    );
    let root = fixture.events.backend.core.window_id;
    for xid in [0x6a00, 0x6a01, 0x6a02, root] {
        Backend::free_pixmap(&mut fixture.events.backend, None, xid)
            .expect("free recording scene drawable through Backend");
    }
    for _ in 0..32 {
        Backend::before_block(&mut fixture.events.backend);
    }
    assert_eq!(fixture.events.backend.store.pending_retire_count(), 0);
    assert!(fixture.events.backend.store.lookup(root).is_none());
    fixture.finish();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: hand carried damage over without raising the scene dirty bit.
fn paint_carried_to_a_flipping_output_stays_scheduled_after_the_composer_retires() {
    let Some(mut fixture) =
        RecordingSceneFixture::new(2).expect("two-output recording scene fixture construction")
    else {
        return;
    };
    let fd = fixture.events.backend.platform.devices[0]
        .device
        .as_fd()
        .as_raw_fd();
    let first = submit_damaged_frame(&mut fixture);
    assert_eq!(first.len(), 2, "both outputs compose the first paint");
    let (composer, flipping) = (&first[0].1, &first[1].1);
    let composer_crtc = composer.events[0].crtc;
    complete_recorded_attempt(
        &mut fixture,
        fd,
        composer,
        accepted_event_id(composer).expect("accepted"),
        71,
    );

    // The second paint reaches only the output whose flip retired.
    fixture.events.backend.scene.scene_structure_dirty = false;
    let second = submit_damaged_frame(&mut fixture);
    assert_eq!(second.len(), 1, "the flipping output cannot compose yet");
    assert_eq!(second[0].1.events[0].crtc, composer_crtc);
    complete_recorded_attempt(
        &mut fixture,
        fd,
        &second[0].1,
        accepted_event_id(&second[0].1).expect("accepted"),
        72,
    );
    complete_recorded_attempt(
        &mut fixture,
        fd,
        flipping,
        accepted_event_id(flipping).expect("accepted"),
        73,
    );
    assert!(
        !fixture
            .events
            .backend
            .store
            .has_pending_presentation_damage(),
        "the composer's retire acked the second paint"
    );
    assert!(
        fixture.events.backend.scene_wants_scene(),
        "the other output still owes the second paint"
    );

    let before = fixture.events.recorders[&fd].records().len();
    Backend::maybe_composite(&mut fixture.events.backend).expect("compose through Backend");
    let carried: Vec<_> = fixture.events.recorders[&fd]
        .records()
        .into_iter()
        .skip(before)
        .filter(|record| accepted_event_id(record).is_some())
        .collect();
    assert_eq!(
        carried.len(),
        1,
        "the owed output composes the carried paint"
    );
    assert_ne!(carried[0].events[0].crtc, composer_crtc);
    complete_recorded_attempt(
        &mut fixture,
        fd,
        &carried[0],
        accepted_event_id(&carried[0]).expect("accepted"),
        74,
    );
    fixture.finish_after_normal_drain();
}
