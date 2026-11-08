use super::support::{AtomicEventFixture, accepted_event_id, install_controlled_clock};
use crate::{
    drm::atomic::{
        AtomicCommitKind, AtomicCommitSink,
        tests_support::{AtomicCommitRecord, RecordingAtomicCommitExecutor, ScriptedResult},
    },
    kms::render::{backend::KmsBackend, platform::CrtcKey},
};
use std::{
    io,
    os::fd::{AsFd as _, AsRawFd as _, RawFd},
    rc::Rc,
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};
use yserver_core::backend::{Backend, ModeSpec, PresentClockSample, PresentClockSource};

fn handle<H: ::drm::control::ResourceHandle>(value: u32) -> H {
    ::drm::control::from_u32(value).expect("nonzero gamma test handle")
}

fn enable_gamma(
    backend: &mut KmsBackend,
    recorder: &RecordingAtomicCommitExecutor,
    output_idx: usize,
    gamma_size: u16,
) -> (u32, ::drm::control::crtc::Handle) {
    let output = &mut backend.platform.outputs[output_idx].output;
    output.crtc_gamma_lut_prop = Some(handle(70));
    output.crtc_degamma_lut_prop = Some(handle(71));
    output.crtc_ctm_prop = Some(handle(72));
    if output.plane_in_fence_fd_prop.is_none() {
        output.plane_in_fence_fd_prop = Some(handle(73));
    }
    if output.crtc_out_fence_ptr_prop.is_none() {
        output.crtc_out_fence_ptr_prop = Some(handle(74));
    }
    let crtc = output.crtc;
    let output_key = backend.platform.outputs[output_idx].key.clone();
    let crtc_xid = u32::from(crtc);
    backend.pstate.crtc_key_by_id.insert(crtc_xid, output_key);
    recorder.set_gamma_size_for_tests(gamma_size);
    (crtc_xid, crtc)
}

fn gamma_backend_with_recorder() -> (
    KmsBackend,
    Rc<RecordingAtomicCommitExecutor>,
    u32,
    ::drm::control::crtc::Handle,
) {
    let mut backend = KmsBackend::for_tests();
    let recorder = Rc::new(RecordingAtomicCommitExecutor::default());
    backend.platform.devices[0].atomic_commit_sink =
        Rc::new(AtomicCommitSink::new(recorder.clone()));
    let (crtc_xid, crtc) = enable_gamma(&mut backend, &recorder, 0, 4);
    backend.scene.scene_structure_dirty = false;
    (backend, recorder, crtc_xid, crtc)
}

fn gamma_event_fixture() -> (
    AtomicEventFixture,
    Rc<RecordingAtomicCommitExecutor>,
    u32,
    ::drm::control::crtc::Handle,
    RawFd,
) {
    let mut events = AtomicEventFixture::from_backend(KmsBackend::for_tests());
    let fd = events.backend.platform.devices[0]
        .device
        .as_fd()
        .as_raw_fd();
    let recorder = events.recorders[&fd].clone();
    let (crtc_xid, crtc) = enable_gamma(&mut events.backend, &recorder, 0, 4);
    events.backend.scene.scene_structure_dirty = false;
    (events, recorder, crtc_xid, crtc, fd)
}

fn seed_recent_gamma_page_flip(backend: &mut KmsBackend, output_idx: usize) {
    let key = CrtcKey::for_output(&backend.platform.outputs[output_idx]);
    let received_at = crate::drm::atomic::monotonic_timestamp()
        .expect("CLOCK_MONOTONIC is available in the test environment");
    let ust = received_at.as_micros();
    backend.platform.record_completion_clock(
        key,
        PresentClockSample {
            msc: 1,
            ust: u64::try_from(ust).expect("monotonic microseconds fit u64"),
            source: PresentClockSource::PageFlip,
        },
        Some(received_at),
    );
}

fn set_test_gamma(backend: &mut KmsBackend, crtc_xid: u32, value: u16) {
    Backend::set_crtc_gamma(
        backend,
        crtc_xid,
        &[value; 4],
        &[value.wrapping_add(1); 4],
        &[value.wrapping_add(2); 4],
    )
    .expect("set RandR gamma through Backend");
}

fn contains_crtc_property(
    record: &AtomicCommitRecord,
    crtc: ::drm::control::crtc::Handle,
    property: ::drm::control::property::Handle,
) -> bool {
    record
        .properties
        .iter()
        .any(|update| update.object == crtc.into() && update.property == property)
}

fn complete_recorded_attempt(
    fixture: &mut AtomicEventFixture,
    fd: RawFd,
    record: &AtomicCommitRecord,
    frame: u32,
) {
    complete_recorded_attempt_at(fixture, fd, record, frame, None);
}

fn complete_recorded_attempt_at(
    fixture: &mut AtomicEventFixture,
    fd: RawFd,
    record: &AtomicCommitRecord,
    frame: u32,
    clock_sample: Option<(Duration, Duration)>,
) {
    let id = accepted_event_id(record).expect("accepted request carries a commit identity");
    let default_ust = Duration::from_millis(u64::from(frame) * 10);
    for event in &record.events {
        if let Some((ust, received_at)) = clock_sample {
            fixture.queue_commit_completion_at(fd, event.crtc, id, frame, ust, received_at);
        } else {
            fixture.queue_commit_completion(fd, event.crtc, id, frame, default_ust);
        }
    }
    Backend::on_page_flip_ready(&mut fixture.backend, &mut fixture.state, fd);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: drop the pending LUT when the standalone GammaOnly commit returns EBUSY.
fn gamma_standalone_ebusy_retries_after_the_next_crtc_event() {
    let Some(fixture) = super::recording_scene::RecordingSceneFixture::new(1)
        .expect("recording scene fixture construction")
    else {
        eprintln!("skip: no Vulkan scene fixture available");
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    let (crtc_xid, crtc, fd, recorder) = enable_recording_scene_gamma(&mut fixture);
    super::super::tearing::enter_tearing_without_direct_scanout(
        &mut fixture.events.backend,
        0x7a01,
        17_001,
    );
    recorder.clear_records();
    recorder.script_result(Err(io::Error::from_raw_os_error(libc::EBUSY)));
    let red = vec![0x1111; 4];
    let green = vec![0x2222; 4];
    let blue = vec![0x3333; 4];
    Backend::set_crtc_gamma(&mut fixture.events.backend, crtc_xid, &red, &green, &blue)
        .expect("EBUSY leaves the cached gamma pending");
    assert_eq!(
        recorder.records()[0].kind,
        AtomicCommitKind::GammaOnly,
        "idle gamma uses the event-producing standalone stream"
    );
    assert!(matches!(
        recorder.records()[0].result,
        ScriptedResult::Error {
            raw_os_error: Some(error),
            ..
        } if error == libc::EBUSY
    ));
    assert!(
        fixture.events.backend.platform.outputs[0]
            .output
            .pending_gamma_lut
            .borrow()
            .is_some()
    );

    // Submit an async frame through Backend while tearing is active. Its
    // event frees the stream and triggers the retained standalone retry.
    let async_attempts = super::atomic_events::submit_damaged_frame(&mut fixture);
    let async_record = async_attempts
        .iter()
        .map(|(_, record)| record)
        .find(|record| record.kind == AtomicCommitKind::Async)
        .expect("Backend submits an async frame while tearing is active")
        .clone();
    assert_eq!(async_record.kind, AtomicCommitKind::Async);
    assert!(!contains_crtc_property(&async_record, crtc, handle(70)));
    complete_recorded_attempt(&mut fixture.events, fd, &async_record, 1);

    let records = recorder.records();
    assert_eq!(
        records.len(),
        3,
        "gamma retries once after the next CRTC event"
    );
    let retry = &records[2];
    assert_eq!(retry.kind, AtomicCommitKind::GammaOnly);
    assert!(accepted_event_id(retry).is_some());
    assert!(contains_crtc_property(retry, crtc, handle(70)));
    assert!(!contains_crtc_property(&records[1], crtc, handle(70)));
    assert!(
        fixture.events.backend.platform.outputs[0]
            .output
            .pending_gamma_lut
            .borrow()
            .is_none()
    );
    assert_eq!(
        fixture.events.backend.get_crtc_gamma(crtc_xid),
        (red, green, blue)
    );
    complete_recorded_attempt(&mut fixture.events, fd, retry, 2);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: drop the per-CRTC GAMMA_LUT disable latch after EINVAL.
fn gamma_einval_latches_crtc_off_and_resubmits_the_frame() {
    let Some(fixture) = super::recording_scene::RecordingSceneFixture::new(1)
        .expect("recording scene fixture construction")
    else {
        eprintln!("skip: no Vulkan scene fixture available");
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    let (crtc_xid, crtc, fd, recorder) = enable_recording_scene_gamma(&mut fixture);
    let initial = super::atomic_events::submit_damaged_frame(&mut fixture);
    let initial_scene = initial
        .iter()
        .find(|(_, record)| {
            record.kind == AtomicCommitKind::Scene && accepted_event_id(record).is_some()
        })
        .expect("initial Backend scene frame is accepted")
        .1
        .clone();
    let red = vec![0x1010; 4];
    let green = vec![0x2020; 4];
    let blue = vec![0x3030; 4];
    Backend::set_crtc_gamma(&mut fixture.events.backend, crtc_xid, &red, &green, &blue)
        .expect("cache gamma while the Backend scene commit is in flight");
    let root = fixture.events.backend.core.window_id;
    let (width, height) = (
        fixture.events.backend.platform.fb_w,
        fixture.events.backend.platform.fb_h,
    );
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
    .expect("queue the retry frame before the first event retires");
    complete_recorded_attempt(&mut fixture.events, fd, &initial_scene, 1);
    let blob_offset = recorder.created_property_blobs().len();
    recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EINVAL)));
    recorder.script_result(Ok(()));

    let scene_attempts = super::atomic_events::submit_damaged_frame(&mut fixture);
    let rejected = scene_attempts
        .iter()
        .map(|(_, record)| record)
        .find(|record| record.result != ScriptedResult::Ok)
        .expect("Backend scene request carrying gamma is rejected with EINVAL");
    assert_eq!(rejected.kind, AtomicCommitKind::Scene);
    assert!(contains_crtc_property(rejected, crtc, handle(70)));
    let accepted = scene_attempts
        .iter()
        .map(|(_, record)| record)
        .find(|record| record.result == ScriptedResult::Ok)
        .expect("Backend retries the scene without gamma and accepts the frame")
        .clone();
    assert_eq!(accepted.kind, AtomicCommitKind::Scene);
    assert!(!contains_crtc_property(&accepted, crtc, handle(70)));
    assert!(accepted.properties.iter().any(|update| {
        update.object
            == fixture.events.backend.platform.outputs[0]
                .output
                .plane
                .into()
            && update.property
                == fixture.events.backend.platform.outputs[0]
                    .output
                    .plane_fb_id_prop
    }));
    let frame_id = accepted_event_id(&accepted).expect("accepted scene has an event identity");
    assert_eq!(
        fixture.events.backend.platform.devices[0]
            .atomic_commit_sink
            .for_tests_pending_page_flip_commit_ids(
                &fixture.events.backend.platform.devices[0].device,
                crtc,
            ),
        [frame_id],
        "the retried frame owns its normal CRTC event association"
    );
    assert!(
        fixture.events.backend.platform.outputs[0]
            .output
            .gamma_lut_disabled
            .get()
    );
    assert!(
        fixture.events.backend.platform.outputs[0]
            .output
            .pending_gamma_lut
            .borrow()
            .is_none()
    );
    assert_eq!(
        fixture.events.backend.get_crtc_gamma(crtc_xid),
        (red.clone(), green.clone(), blue.clone())
    );
    let created: Vec<_> = recorder
        .created_property_blobs()
        .into_iter()
        .skip(blob_offset)
        .map(|(blob, _)| blob)
        .collect();
    let destroyed: Vec<_> = recorder
        .destroyed_property_blobs()
        .into_iter()
        .filter(|blob| created.contains(blob))
        .collect();
    assert_eq!(created.len(), 1, "the rejected scene creates one LUT blob");
    assert_eq!(
        destroyed, created,
        "the rejected LUT blob is destroyed once"
    );

    let before = recorder.records().len();
    Backend::set_crtc_gamma(&mut fixture.events.backend, crtc_xid, &red, &green, &blue)
        .expect("RandR cache stays writable after the per-CRTC capability latch");
    assert_eq!(recorder.records().len(), before);

    complete_recorded_attempt(&mut fixture.events, fd, &accepted, 2);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: latch gamma before the no-LUT retry.
fn p1_gamma_scene_retry_eio_keeps_lut_pending() {
    let Some(fixture) = super::recording_scene::RecordingSceneFixture::new(1)
        .expect("recording scene fixture construction")
    else {
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    let (crtc_xid, crtc, fd, recorder) = enable_recording_scene_gamma(&mut fixture);
    let gamma_prop = handle(170);
    fixture.events.backend.platform.outputs[0]
        .output
        .crtc_gamma_lut_prop = Some(gamma_prop);
    fixture.events.backend.platform.outputs[0]
        .output
        .crtc_degamma_lut_prop = Some(handle(171));
    fixture.events.backend.platform.outputs[0]
        .output
        .crtc_ctm_prop = Some(handle(172));
    let initial = super::atomic_events::submit_damaged_frame(&mut fixture);
    let initial_scene = initial
        .iter()
        .map(|(_, record)| record)
        .find(|record| {
            record.kind == AtomicCommitKind::Scene && accepted_event_id(record).is_some()
        })
        .expect("initial Backend scene frame is accepted")
        .clone();
    complete_recorded_attempt(&mut fixture.events, fd, &initial_scene, 1);

    install_controlled_clock(&mut fixture.events.backend);
    set_test_gamma(&mut fixture.events.backend, crtc_xid, 0x4567);
    assert!(
        recorder
            .records()
            .iter()
            .all(|record| { record.kind != AtomicCommitKind::GammaOnly })
    );
    assert!(
        fixture.events.backend.platform.outputs[0]
            .output
            .pending_gamma_lut
            .borrow()
            .is_some()
    );
    assert!(
        !fixture.events.backend.platform.outputs[0]
            .output
            .gamma_lut_disabled
            .get()
    );

    let record_offset = recorder.records().len();
    let blob_offset = recorder.created_property_blobs().len();
    recorder.script_result(Err(io::Error::from_raw_os_error(libc::EINVAL)));
    recorder.script_result(Err(io::Error::from_raw_os_error(libc::EIO)));
    let root = fixture.events.backend.core.window_id;
    let (width, height) = (
        fixture.events.backend.platform.fb_w,
        fixture.events.backend.platform.fb_h,
    );
    let failed_attempts: Vec<_> = super::atomic_events::submit_damaged_frame(&mut fixture)
        .into_iter()
        .filter(|(attempt_fd, record)| *attempt_fd == fd && record.kind == AtomicCommitKind::Scene)
        .map(|(_, record)| record)
        .collect();
    assert_eq!(failed_attempts.len(), 2, "the frame is submitted twice");
    assert!(contains_crtc_property(
        &failed_attempts[0],
        crtc,
        gamma_prop
    ));
    assert!(!contains_crtc_property(
        &failed_attempts[1],
        crtc,
        gamma_prop
    ));
    let output = &fixture.events.backend.platform.outputs[0].output;
    let fb_id = |record: &AtomicCommitRecord| {
        record
            .properties
            .iter()
            .find(|update| {
                update.object == output.plane.into() && update.property == output.plane_fb_id_prop
            })
            .map(|update| update.value)
    };
    let first_fb_id = fb_id(&failed_attempts[0]);
    let retry_fb_id = fb_id(&failed_attempts[1]);
    assert!(first_fb_id.is_some(), "the frame updates its primary plane");
    assert_eq!(
        first_fb_id, retry_fb_id,
        "the no-LUT retry keeps the same scene framebuffer"
    );
    assert!(matches!(
        failed_attempts[0].result,
        ScriptedResult::Error { raw_os_error: Some(error), .. } if error == libc::EINVAL
    ));
    assert!(matches!(
        failed_attempts[1].result,
        ScriptedResult::Error { raw_os_error: Some(error), .. } if error == libc::EIO
    ));
    assert!(
        !fixture.events.backend.platform.outputs[0]
            .output
            .gamma_lut_disabled
            .get(),
        "unrelated EIO does not latch gamma off"
    );
    assert!(
        fixture.events.backend.platform.outputs[0]
            .output
            .pending_gamma_lut
            .borrow()
            .is_some(),
        "the LUT remains pending"
    );
    let created: Vec<_> = recorder
        .created_property_blobs()
        .into_iter()
        .skip(blob_offset)
        .map(|(blob, _)| blob)
        .collect();
    let destroyed: Vec<_> = recorder
        .destroyed_property_blobs()
        .into_iter()
        .filter(|blob| created.contains(blob))
        .collect();
    assert_eq!(created.len(), 1, "the rejected frame creates one LUT blob");
    assert_eq!(
        destroyed, created,
        "the rejected LUT blob is destroyed once"
    );
    assert_eq!(
        recorder.records().len() - record_offset,
        2,
        "both failing attempts reach the recording executor"
    );
    assert_eq!(
        fixture.events.backend.platform.devices[0]
            .atomic_commit_sink
            .for_tests_pending_page_flip_entry_count(),
        0,
        "failed scene attempts install no flip obligation"
    );
    assert!(!fixture.events.backend.scene.has_pending_page_flips());
    assert_eq!(
        fixture.events.backend.scene.pending_page_flip_event_id(0),
        None
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let accepted = loop {
        let before = recorder.records().len();
        Backend::before_block(&mut fixture.events.backend);
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
        .expect("damage root after failed scene submit");
        Backend::maybe_composite(&mut fixture.events.backend)
            .expect("compose after failed scene submit");
        if let Some(record) = recorder.records().into_iter().skip(before).find(|record| {
            record.kind == AtomicCommitKind::Scene && accepted_event_id(record).is_some()
        }) {
            break record;
        }
        assert!(
            Instant::now() < deadline,
            "the pending LUT is retried on the next scene frame"
        );
    };
    assert!(contains_crtc_property(&accepted, crtc, gamma_prop));
    assert!(
        !fixture.events.backend.platform.outputs[0]
            .output
            .gamma_lut_disabled
            .get()
    );
    assert!(
        fixture.events.backend.platform.outputs[0]
            .output
            .pending_gamma_lut
            .borrow()
            .is_none()
    );
    complete_recorded_attempt(&mut fixture.events, fd, &accepted, 2);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: snapshot the composed gamma retry before later plane and cursor updates are added.
fn gamma_composed_einval_retry_preserves_every_plane_cursor_and_event() {
    let Some((fixture, direct)) = super::direct_unflip::direct_fixture(
        2,
        super::recording_scene::P1SceneRoute::Shared,
        17_003,
        0,
    ) else {
        eprintln!("skip: no Vulkan scene fixture available");
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    let device_fd = super::direct_unflip::fd(&fixture);
    let recorder = fixture.events.recorders[&device_fd].clone();
    let (crtc_xid, gamma_crtc) = enable_gamma(&mut fixture.events.backend, &recorder, 0, 4);
    let direct_attempts = super::direct_unflip::submit_direct(&mut fixture, &direct, device_fd);
    let direct_record = direct_attempts
        .iter()
        .find(|record| accepted_event_id(record).is_some())
        .expect("direct frame is accepted through Backend");
    let direct_id = accepted_event_id(direct_record).expect("accepted direct frame has an ID");
    super::direct_unflip::complete_direct_initial(
        &mut fixture,
        direct_record,
        direct_id,
        device_fd,
    );
    for output_idx in 0..2usize {
        let output_handle_idx = u32::try_from(output_idx).expect("output index fits");
        fixture
            .events
            .backend
            .platform
            .for_tests_seed_atomic_cursor_position(
                output_idx,
                handle(90 + output_handle_idx),
                handle(92 + output_handle_idx),
                handle(94 + output_handle_idx),
                100 + i32::try_from(output_idx).expect("cursor x index fits") * 800,
                100,
                0,
                0,
            );
    }
    super::direct_unflip::drain_present_notifications(
        &mut fixture,
        &[direct.candidate.present_id],
        &[],
    );

    recorder.script_result(Err(io::Error::from_raw_os_error(libc::EBUSY)));
    Backend::set_crtc_gamma(
        &mut fixture.events.backend,
        crtc_xid,
        &[0x1111; 4],
        &[0x2222; 4],
        &[0x3333; 4],
    )
    .expect("retain gamma after the standalone attempt returns EBUSY");
    assert!(
        fixture.events.backend.platform.outputs[0]
            .output
            .pending_gamma_lut
            .borrow()
            .is_some()
    );
    super::direct_unflip::request_unflip(&mut fixture, &direct);
    recorder.script_result(Err(io::Error::from_raw_os_error(libc::EINVAL)));
    recorder.script_result(Ok(()));
    let offset = recorder.records().len();
    let accepted = super::direct_unflip::grouped_unflip_record(&mut fixture, device_fd, offset, 2);
    let attempts = recorder
        .records()
        .into_iter()
        .skip(offset)
        .collect::<Vec<_>>();
    let rejected = attempts
        .iter()
        .find(|record| record.result != ScriptedResult::Ok)
        .expect("gamma-bearing composed Backend request is rejected with EINVAL");
    let rejected_has_gamma = contains_crtc_property(rejected, gamma_crtc, handle(70));
    let accepted_kind = accepted.kind;
    let accepted_has_gamma = contains_crtc_property(&accepted, gamma_crtc, handle(70));
    let all_primary_planes_present = fixture
        .events
        .backend
        .platform
        .outputs
        .iter()
        .all(|output| {
            accepted.properties.iter().any(|update| {
                update.object == output.output.plane.into()
                    && update.property == output.output.plane_fb_id_prop
            })
        });
    let cursors_present = (0..2u32).all(|output_idx| {
        let cursor_plane = handle::<::drm::control::plane::Handle>(90 + output_idx);
        accepted.properties.iter().any(|update| {
            update.object == cursor_plane.into() && update.property == handle(92 + output_idx)
        }) && accepted.properties.iter().any(|update| {
            update.object == cursor_plane.into() && update.property == handle(94 + output_idx)
        })
    });
    let accepted_id = accepted_event_id(&accepted).expect("fallback owns a grouped event ID");
    let pending_ids_match = accepted.events.iter().all(|event| {
        let device = &fixture.events.backend.platform.devices[0];
        device
            .atomic_commit_sink
            .for_tests_pending_page_flip_commit_ids(&device.device, event.crtc)
            == [accepted_id]
    });
    complete_recorded_attempt(&mut fixture.events, device_fd, &accepted, 2);
    let events_retired = accepted.events.iter().all(|event| {
        let device = &fixture.events.backend.platform.devices[0];
        device
            .atomic_commit_sink
            .for_tests_pending_page_flip_commit_ids(&device.device, event.crtc)
            .is_empty()
    });
    super::direct_unflip::drain_present_notifications(
        &mut fixture,
        &[],
        &[direct.candidate.present_id],
    );
    assert!(
        fixture.events.backend.platform.outputs[0]
            .output
            .gamma_lut_disabled
            .get(),
        "successful composed retry confirms GAMMA_LUT caused EINVAL"
    );
    assert!(
        fixture.events.backend.platform.outputs[0]
            .output
            .pending_gamma_lut
            .borrow()
            .is_none(),
        "successful composed retry consumes the pending LUT"
    );
    fixture.finish_after_normal_drain();
    assert!(rejected_has_gamma, "rejected request carried GAMMA_LUT");
    assert_eq!(accepted_kind, AtomicCommitKind::SceneWithCursor);
    assert!(!accepted_has_gamma, "accepted retry omits the rejected LUT");
    assert!(
        all_primary_planes_present,
        "retry retains every primary plane update"
    );
    assert!(
        cursors_present,
        "retry retains every cursor position update"
    );
    assert!(
        pending_ids_match,
        "each CRTC tracks the accepted retry identity"
    );
    assert!(events_retired, "each CRTC retires the accepted retry event");
}

fn enable_recording_scene_gamma(
    fixture: &mut super::recording_scene::RecordingSceneFixture,
) -> (
    u32,
    ::drm::control::crtc::Handle,
    RawFd,
    Rc<RecordingAtomicCommitExecutor>,
) {
    let fd = fixture.kms_fd(0);
    let recorder = fixture.events.recorders[&fd].clone();
    let (crtc_xid, crtc) = enable_gamma(&mut fixture.events.backend, &recorder, 0, 4);
    (crtc_xid, crtc, fd, recorder)
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: append gamma and VRR changes to one grouped request.
fn vrr_clear_and_gamma_never_share_a_request() {
    let Some((fixture, direct)) = super::direct_unflip::direct_fixture(
        2,
        super::recording_scene::P1SceneRoute::Shared,
        18_012,
        0,
    ) else {
        eprintln!("skip: no Vulkan scene fixture available");
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    let fd = super::direct_unflip::fd(&fixture);
    let mut direct = direct;
    let recorder = fixture.events.recorders[&fd].clone();
    let (crtc_a_xid, crtc_a) = enable_gamma(&mut fixture.events.backend, &recorder, 0, 4);
    let crtc_b = fixture.events.backend.platform.outputs[1].output.crtc;
    {
        let output = &mut fixture.events.backend.platform.outputs[1].output;
        output.crtc_vrr_enabled_prop = Some(handle(90));
        output.connector_vrr_capable = true;
        output.vrr_committed.set(false);
    }

    let initial = super::vrr::qualified_direct_record(&mut fixture, &mut direct, fd);
    assert!(contains_crtc_property(&initial, crtc_b, handle(90)));
    super::direct_unflip::complete_direct_initial(
        &mut fixture,
        &initial,
        accepted_event_id(&initial).expect("initial direct frame has an event ID"),
        fd,
    );
    super::direct_unflip::drain_present_notifications(
        &mut fixture,
        &[direct.candidate.present_id],
        &[direct.candidate.present_id - 1],
    );

    set_test_gamma(&mut fixture.events.backend, crtc_a_xid, 0x4567);
    assert!(
        fixture.events.backend.platform.outputs[0]
            .output
            .pending_gamma_lut
            .borrow()
            .is_some(),
        "RandR leaves output A's gamma LUT pending during direct scanout"
    );
    assert!(
        recorder
            .records()
            .iter()
            .all(|record| record.kind != AtomicCommitKind::GammaOnly),
        "gamma does not submit a standalone request while direct scanout owns output A"
    );

    super::direct_unflip::request_unflip(&mut fixture, &direct);
    let unflip_offset = recorder.records().len();
    recorder.script_result(Err(io::Error::from_raw_os_error(libc::EINVAL)));
    recorder.script_result(Ok(()));
    let accepted_retry =
        super::direct_unflip::grouped_unflip_record(&mut fixture, fd, unflip_offset, 2);
    let attempts = super::direct_unflip::records_since(&fixture, fd, unflip_offset);
    assert_eq!(
        attempts.len(),
        2,
        "the request is retried once without CRTC properties"
    );
    assert!(
        fixture.events.backend.platform.outputs[1]
            .output
            .vrr_disabled
            .get(),
        "the accepted stripped retry attributes EINVAL to VRR on output B"
    );
    assert!(
        !fixture.events.backend.platform.outputs[0]
            .output
            .gamma_lut_disabled
            .get(),
        "a VRR EINVAL never latches GAMMA_LUT off on output A"
    );
    assert!(
        matches!(
            attempts[0].result,
            ScriptedResult::Error {
                raw_os_error: Some(error),
                ..
            } if error == libc::EINVAL
        ),
        "the property-bearing request is rejected with EINVAL"
    );
    assert!(contains_crtc_property(&attempts[0], crtc_b, handle(90)));
    assert!(
        !contains_crtc_property(&attempts[0], crtc_a, handle(70)),
        "the VRR clear request does not carry output A's GAMMA_LUT"
    );
    assert!(
        !contains_crtc_property(&attempts[0], crtc_a, handle(90)),
        "the request changes only output B's VRR property"
    );
    assert_eq!(attempts[1].result, ScriptedResult::Ok);
    assert!(accepted_event_id(&attempts[1]).is_some());
    assert!(
        !contains_crtc_property(&attempts[1], crtc_a, handle(70))
            && !contains_crtc_property(&attempts[1], crtc_b, handle(90)),
        "the stripped retry removes all optional CRTC changes"
    );
    assert!(
        fixture.events.backend.platform.outputs[1]
            .output
            .vrr_committed
            .get(),
        "the accepted stripped retry leaves output B's kernel VRR value unchanged"
    );

    complete_recorded_attempt(&mut fixture.events, fd, &accepted_retry, 84);
    super::direct_unflip::drain_present_notifications(
        &mut fixture,
        &[],
        &[direct.candidate.present_id],
    );
    fixture.finish_after_normal_drain();
}

struct LogicalVtResume;

impl crate::kms::render::backend::kms::session::VtResumeExecutor for LogicalVtResume {
    fn acquire_master(&self, _device: &crate::kms::render::platform::KmsDevice) -> io::Result<()> {
        Ok(())
    }

    fn release_master(&self, _device: &crate::kms::render::platform::KmsDevice) -> io::Result<()> {
        Ok(())
    }

    fn probe_connector_snapshot(
        &self,
        platform: &crate::kms::render::platform::PlatformBackend,
    ) -> io::Result<Vec<crate::kms::render::platform::ConnectorSnapshot>> {
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

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: let a scene flip through while the standalone GAMMA_LUT event is pending.
fn p1_gamma_standalone_serializes_scene_until_its_event() {
    let Some(fixture) = super::recording_scene::RecordingSceneFixture::new(1)
        .expect("recording scene fixture construction")
    else {
        eprintln!("skip: no Vulkan scene fixture available");
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    let (crtc_xid, crtc, fd, recorder) = enable_recording_scene_gamma(&mut fixture);
    let initial = super::atomic_events::submit_damaged_frame(&mut fixture);
    let stale_receipt = crate::drm::atomic::monotonic_timestamp()
        .expect("CLOCK_MONOTONIC is available in the test environment")
        .checked_sub(Duration::from_secs(3))
        .expect("test CLOCK_MONOTONIC age is available");
    complete_recorded_attempt_at(
        &mut fixture.events,
        fd,
        &initial[0].1,
        0,
        Some((Duration::ZERO, stale_receipt)),
    );
    recorder.clear_records();
    Backend::set_crtc_gamma(
        &mut fixture.events.backend,
        crtc_xid,
        &[0x1111; 4],
        &[0x2222; 4],
        &[0x3333; 4],
    )
    .expect("submit standalone gamma through Backend");
    let gamma = recorder.records()[0].clone();
    assert_eq!(gamma.kind, AtomicCommitKind::GammaOnly);
    assert!(gamma.events[0].crtc == crtc);
    let gamma_id = accepted_event_id(&gamma).expect("standalone gamma carries a commit identity");
    assert!(
        !fixture
            .events
            .backend
            .scene
            .output_ready_for_submit(0, &fixture.events.backend.platform)
    );

    let root = fixture.events.backend.core.window_id;
    let (width, height) = (
        fixture.events.backend.platform.fb_w,
        fixture.events.backend.platform.fb_h,
    );
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
    .expect("queue visible scene damage through Backend");
    Backend::maybe_composite(&mut fixture.events.backend).expect("compose while gamma is pending");
    assert_eq!(
        recorder.records().len(),
        1,
        "scene submit waits for gamma event"
    );
    assert!(!fixture.events.backend.scene.has_pending_page_flips());

    fixture
        .events
        .queue_commit_completion(fd, crtc, gamma_id, 1, Duration::from_millis(10));
    let events = fixture.events_mut();
    Backend::on_page_flip_ready(&mut events.backend, &mut events.state, fd);
    let scene_attempts = super::atomic_events::submit_damaged_frame(&mut fixture);
    let scene = scene_attempts
        .iter()
        .find(|(_, record)| {
            record.kind == AtomicCommitKind::Scene && accepted_event_id(record).is_some()
        })
        .expect("scene damage submits after the gamma event");
    assert!(!contains_crtc_property(&scene.1, crtc, handle(70)));
    complete_recorded_attempt(&mut fixture.events, fd, &scene.1, 2);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: append the pending CRTC property to an async frame instead of waiting for vsync.
fn p1_gamma_async_flip_defers_lut_to_the_next_vsync_commit() {
    let Some(fixture) = super::recording_scene::RecordingSceneFixture::new(1)
        .expect("recording scene fixture construction")
    else {
        eprintln!("skip: no Vulkan scene fixture available");
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    let (crtc_xid, crtc, fd, recorder) = enable_recording_scene_gamma(&mut fixture);
    super::super::tearing::enter_tearing_without_direct_scanout(
        &mut fixture.events.backend,
        0x7a02,
        17_002,
    );
    recorder.clear_records();
    let first_attempts = super::atomic_events::submit_damaged_frame(&mut fixture);
    let first_async = first_attempts
        .iter()
        .map(|(_, record)| record)
        .find(|record| record.kind == AtomicCommitKind::Async)
        .expect("Backend submits async scene frame while tearing is active")
        .clone();
    assert!(accepted_event_id(&first_async).is_some());

    let red = vec![0x1111; 4];
    let green = vec![0x2222; 4];
    let blue = vec![0x3333; 4];
    let (base, elapsed_ns) = install_controlled_clock(&mut fixture.events.backend);
    Backend::set_crtc_gamma(&mut fixture.events.backend, crtc_xid, &red, &green, &blue)
        .expect("retain gamma while the async frame is in flight");
    assert!(
        fixture.events.backend.platform.outputs[0]
            .output
            .pending_gamma_lut
            .borrow()
            .is_some()
    );
    let root = fixture.events.backend.core.window_id;
    let (width, height) = (
        fixture.events.backend.platform.fb_w,
        fixture.events.backend.platform.fb_h,
    );
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
    .expect("queue scene damage so the LUT can wait for a frame");
    complete_recorded_attempt(&mut fixture.events, fd, &first_async, 1);
    assert_eq!(
        recorder.records().len(),
        1,
        "async retirement does not add a CRTC commit while scene work is queued"
    );

    let async_attempts = super::atomic_events::submit_damaged_frame(&mut fixture);
    let async_record = async_attempts
        .iter()
        .map(|(_, record)| record)
        .find(|record| record.kind == AtomicCommitKind::Async)
        .expect("Backend submits the next async scene frame")
        .clone();
    assert_eq!(async_record.kind, AtomicCommitKind::Async);
    assert!(!contains_crtc_property(&async_record, crtc, handle(70)));
    assert!(accepted_event_id(&async_record).is_some());
    complete_recorded_attempt(&mut fixture.events, fd, &async_record, 2);

    assert!(
        recorder
            .records()
            .iter()
            .all(|record| record.kind != AtomicCommitKind::GammaOnly),
        "async retirement keeps gamma held before its one-refresh deadline"
    );
    assert_eq!(
        fixture.events.backend.platform.outputs[0]
            .output
            .pending_gamma_deadline
            .get(),
        Some(base + Duration::from_nanos(16_666_666)),
        "async gamma keeps the first one-refresh deadline"
    );
    elapsed_ns.store(16_666_667, Ordering::SeqCst);
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("expired async gamma deadline submits the synchronous commit");

    let gamma_commit = recorder
        .records()
        .into_iter()
        .find(|record| record.kind == AtomicCommitKind::GammaOnly)
        .expect("retiring async work submits the deferred synchronous gamma commit");
    assert!(contains_crtc_property(&gamma_commit, crtc, handle(70)));
    assert!(contains_crtc_property(&gamma_commit, crtc, handle(71)));
    assert!(contains_crtc_property(&gamma_commit, crtc, handle(72)));
    assert!(
        !gamma_commit
            .flags
            .contains(::drm::control::AtomicCommitFlags::PAGE_FLIP_ASYNC)
    );
    assert!(
        fixture.events.backend.platform.outputs[0]
            .output
            .pending_gamma_lut
            .borrow()
            .is_none()
    );
    complete_recorded_attempt(&mut fixture.events, fd, &gamma_commit, 3);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: retry GammaOnly as soon as an async flip retires, before its deadline.
fn p1_gamma_active_async_flips_do_not_carry_or_advance_the_lut_deadline() {
    let Some(fixture) = super::recording_scene::RecordingSceneFixture::new(1)
        .expect("recording scene fixture construction")
    else {
        eprintln!("skip: no Vulkan scene fixture available");
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    let (crtc_xid, crtc, fd, recorder) = enable_recording_scene_gamma(&mut fixture);
    super::super::tearing::enter_tearing_without_direct_scanout(
        &mut fixture.events.backend,
        0x7a03,
        17_003,
    );
    recorder.clear_records();
    let first_attempts = super::atomic_events::submit_damaged_frame(&mut fixture);
    let first_async = first_attempts
        .iter()
        .map(|(_, record)| record)
        .find(|record| record.kind == AtomicCommitKind::Async)
        .expect("Backend submits async scene frame while tearing is active")
        .clone();
    seed_recent_gamma_page_flip(&mut fixture.events.backend, 0);
    let (base, elapsed_ns) = install_controlled_clock(&mut fixture.events.backend);

    set_test_gamma(&mut fixture.events.backend, crtc_xid, 0x6666);
    let records = recorder.records();
    assert_eq!(
        records.len(),
        1,
        "gamma adds no commit while async is in flight"
    );
    assert_eq!(records[0].kind, AtomicCommitKind::Async);
    assert!(!contains_crtc_property(&records[0], crtc, handle(70)));
    assert_eq!(
        Backend::next_wakeup(&fixture.events.backend),
        Some(base + Duration::from_nanos(16_666_666))
    );
    let root = fixture.events.backend.core.window_id;
    let (width, height) = (
        fixture.events.backend.platform.fb_w,
        fixture.events.backend.platform.fb_h,
    );
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
    .expect("queue scene damage while gamma waits");
    complete_recorded_attempt(&mut fixture.events, fd, &first_async, 1);

    let second_attempts = super::atomic_events::submit_damaged_frame(&mut fixture);
    let second_async = second_attempts
        .iter()
        .map(|(_, record)| record)
        .find(|record| record.kind == AtomicCommitKind::Async)
        .expect("Backend submits another async scene frame")
        .clone();
    assert!(!contains_crtc_property(&second_async, crtc, handle(70)));
    complete_recorded_attempt(&mut fixture.events, fd, &second_async, 2);
    assert_eq!(
        recorder.records().len(),
        2,
        "async retirement before the deadline must not submit GammaOnly"
    );
    assert!(
        fixture.events.backend.platform.outputs[0]
            .output
            .pending_gamma_lut
            .borrow()
            .is_some()
    );

    elapsed_ns.store(16_666_667, Ordering::SeqCst);
    Backend::maybe_composite(&mut fixture.events.backend).expect("expired gamma deadline ticks");
    let gamma = recorder
        .records()
        .into_iter()
        .find(|record| record.kind == AtomicCommitKind::GammaOnly)
        .expect("deadline submits gamma after async work retires");
    assert!(contains_crtc_property(&gamma, crtc, handle(70)));
    assert!(
        fixture.events.backend.platform.outputs[0]
            .output
            .pending_gamma_lut
            .borrow()
            .is_none()
    );
    complete_recorded_attempt(&mut fixture.events, fd, &gamma, 3);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: make the shared gamma reapply helper a no-op after modeset and VT acquire.
fn p1_gamma_reapplies_after_modeset_and_vt_resume() {
    let Some(fixture) = super::recording_scene::RecordingSceneFixture::new(1)
        .expect("recording scene fixture construction")
    else {
        eprintln!("skip: no Vulkan scene fixture available");
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    fixture
        .events
        .backend
        .platform
        .for_tests_accept_output_power_commits();
    fixture
        .events
        .backend
        .for_tests_set_vt_resume_executor(Rc::new(LogicalVtResume));
    let (crtc_xid, crtc, fd, recorder) = enable_recording_scene_gamma(&mut fixture);

    let initial = super::atomic_events::submit_damaged_frame(&mut fixture);
    let initial_scene = initial
        .iter()
        .find(|(_, record)| {
            record.kind == AtomicCommitKind::Scene && accepted_event_id(record).is_some()
        })
        .expect("initial scene frame is accepted");
    let stale_receipt = crate::drm::atomic::monotonic_timestamp()
        .expect("CLOCK_MONOTONIC is available in the test environment")
        .checked_sub(Duration::from_secs(3))
        .expect("test CLOCK_MONOTONIC age is available");
    complete_recorded_attempt_at(
        &mut fixture.events,
        fd,
        &initial_scene.1,
        1,
        Some((Duration::from_millis(10), stale_receipt)),
    );
    recorder.clear_records();

    let red = vec![0x1111; 4];
    let green = vec![0x2222; 4];
    let blue = vec![0x3333; 4];
    Backend::set_crtc_gamma(&mut fixture.events.backend, crtc_xid, &red, &green, &blue)
        .expect("set cached gamma through Backend");
    let gamma = recorder
        .records()
        .into_iter()
        .find(|record| record.kind == AtomicCommitKind::GammaOnly)
        .expect("idle gamma is applied through the standalone commit");
    let stale_gamma_receipt = crate::drm::atomic::monotonic_timestamp()
        .expect("CLOCK_MONOTONIC is available in the test environment")
        .checked_sub(Duration::from_secs(3))
        .expect("test CLOCK_MONOTONIC age is available");
    complete_recorded_attempt_at(
        &mut fixture.events,
        fd,
        &gamma,
        2,
        Some((Duration::from_millis(20), stale_gamma_receipt)),
    );

    let (mode, connector, output_id, output_key, x, y) = {
        let backend = &mut fixture.events.backend;
        let layout = &backend.platform.outputs[0];
        let mode = ModeSpec {
            width: layout.width,
            height: layout.height,
            vrefresh: layout.output.picked.vrefresh.saturating_sub(1),
        };
        let connector = layout.key.connector_name.clone();
        let output_key = layout.key.clone();
        let (x, y) = (layout.x, layout.y);
        let output_id = Backend::randr_outputs_and_modes(backend)
            .0
            .into_iter()
            .find(|output| output.name == connector)
            .expect("output has a RandR id")
            .output_id;
        (mode, connector, output_id, output_key, x, y)
    };
    let mut discovered = super::super::randr::clone_test_drm_output(
        &fixture.events.backend.platform.outputs[0].output,
    );
    let changed_mode =
        super::super::test_advertised_mode(mode.width, mode.height, mode.vrefresh, false);
    // The synthetic DRM device cannot answer get_connector; pre-resolve the
    // overridden output's advertised mode just as discovery would.
    discovered.picked = changed_mode.clone();
    discovered.modes.push(changed_mode);
    fixture.events.backend.pstate.crtc_config_discovery_override = Some(discovered);
    fixture
        .events
        .backend
        .initialize_provider_output_sources()
        .expect("authorize the live test output's KMS provider");
    let mode_offset = recorder.records().len();
    assert!(
        Backend::apply_crtc_config(
            &mut fixture.events.backend,
            output_id,
            &connector,
            Some(mode),
            x,
            y,
        )
        .expect("apply a real mode change through Backend")
    );

    let mode_records = recorder.records();
    let mode_gamma = mode_records
        .iter()
        .skip(mode_offset)
        .find(|record| contains_crtc_property(record, crtc, handle(70)))
        .cloned();
    assert!(
        mode_gamma.is_some(),
        "modeset reapply reaches the atomic stream"
    );
    if let Some(record) = mode_gamma
        .as_ref()
        .filter(|record| accepted_event_id(record).is_some())
    {
        complete_recorded_attempt(&mut fixture.events, fd, record, 3);
    }
    let mode_scene = super::atomic_events::submit_damaged_frame(&mut fixture);
    let accepted_mode_scene = mode_scene
        .iter()
        .find(|(_, record)| {
            record.kind == AtomicCommitKind::Scene && accepted_event_id(record).is_some()
        })
        .expect("modeset damage reaches a scene commit");
    complete_recorded_attempt(&mut fixture.events, fd, &accepted_mode_scene.1, 4);

    let _ = Backend::begin_vt_release(&mut fixture.events.backend);
    let events = fixture.events_mut();
    Backend::finish_vt_release(
        &mut events.backend,
        &mut events.state,
        &yserver_core::core_loop::input_inventory::InputInventory::default(),
    );
    let resume_offset = recorder.records().len();
    let events = fixture.events_mut();
    Backend::on_vt_acquire(&mut events.backend, &mut events.state);
    let resume_records = recorder.records();
    let resume_gamma = resume_records
        .iter()
        .skip(resume_offset)
        .find(|record| contains_crtc_property(record, crtc, handle(70)))
        .cloned();
    if let Some(record) = resume_gamma
        .as_ref()
        .filter(|record| accepted_event_id(record).is_some())
    {
        complete_recorded_attempt(&mut fixture.events, fd, record, 5);
    }
    let resumed_scene = super::atomic_events::submit_damaged_frame(&mut fixture);
    let accepted_resumed_scene = resumed_scene
        .iter()
        .find(|(_, record)| {
            record.kind == AtomicCommitKind::Scene && accepted_event_id(record).is_some()
        })
        .expect("resume damage reaches a scene commit");
    let resumed_scene_has_gamma =
        contains_crtc_property(&accepted_resumed_scene.1, crtc, handle(70));
    assert!(
        resume_gamma.is_some() || resumed_scene_has_gamma,
        "VT reapply reaches either the idle CRTC commit or the next scene vsync"
    );
    let resumed_crtc_xid = Backend::randr_outputs_and_modes(&mut fixture.events.backend)
        .0
        .into_iter()
        .find(|output| output.name == connector)
        .expect("resumed output has a RandR CRTC")
        .crtc_id;
    assert_eq!(
        fixture.events.backend.get_crtc_gamma(resumed_crtc_xid),
        (red, green, blue)
    );
    complete_recorded_attempt(&mut fixture.events, fd, &accepted_resumed_scene.1, 6);
    assert_eq!(fixture.events.backend.platform.outputs[0].key, output_key);
    fixture.finish_after_normal_drain();
}

#[test]
// Mutations: submit immediately for active gamma, or omit its deadline from next_wakeup.
fn gamma_active_crtc_exposes_deadline_and_submits_once_when_no_frame_arrives() {
    let (mut backend, recorder, crtc_xid, _) = gamma_backend_with_recorder();
    let (base, elapsed_ns) = install_controlled_clock(&mut backend);
    seed_recent_gamma_page_flip(&mut backend, 0);

    set_test_gamma(&mut backend, crtc_xid, 0x1111);
    assert!(
        recorder.records().is_empty(),
        "active gamma waits for a frame"
    );

    let deadline = Backend::next_wakeup(&backend).expect("gamma deadline wakes scanout");
    assert_eq!(
        deadline,
        base + Duration::from_nanos(16_666_666),
        "60 Hz gamma deadline is one refresh period after the request"
    );

    elapsed_ns.store(16_666_667, Ordering::SeqCst);
    Backend::maybe_composite(&mut backend).expect("expired gamma deadline submits");
    let records = recorder.records();
    assert_eq!(records.len(), 1, "one expired request yields one commit");
    assert_eq!(records[0].kind, AtomicCommitKind::GammaOnly);
    assert_eq!(
        recorder.created_property_blobs().len(),
        1,
        "the standalone commit carries exactly one LUT blob"
    );
    assert!(
        backend.platform.outputs[0]
            .output
            .pending_gamma_lut
            .borrow()
            .is_none(),
        "accepted standalone commit consumes the pending LUT"
    );
    assert!(backend.next_wakeup().is_none(), "deadline is consumed once");
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: remove the kms_outputs_active condition from immediate GammaOnly
// submission; the zero-commit assertion while DPMS is off must fail.
fn p1_gamma_set_while_dpms_off_stays_pending_and_applies_after_wake() {
    let context_backend = match KmsBackend::for_tests_with_vk() {
        Ok(backend) => backend,
        Err(error) if crate::kms::render::backend::tests::vulkan_absence_is_expected(&error) => {
            eprintln!(
                "SKIP p1_gamma_set_while_dpms_off_stays_pending_and_applies_after_wake: Vulkan ICD unavailable ({error})"
            );
            return;
        }
        Err(error) => panic!("unexpected Vulkan fixture setup error: {error}"),
    };
    let vk = Arc::clone(
        context_backend
            .platform
            .gpu
            .vk
            .as_ref()
            .expect("recording scene fixture provides a Vulkan context"),
    );
    let mut backend = KmsBackend::for_tests();
    let layout = &backend.platform.outputs[0];
    let scanout_pool = crate::kms::vk::scanout::ScanoutBoPool::for_tests_on_screen_framebuffer(
        vk,
        Rc::clone(&backend.platform.devices[0].device),
        layout.scanout_route,
        u32::from(layout.width),
        u32::from(layout.height),
        handle(0x8801),
    );
    backend.platform.scanout_pools[0] =
        Some(crate::kms::vk::scanout::OutputScanout::Shared(scanout_pool));
    backend.platform.for_tests_accept_output_power_commits();
    let mut fixture = AtomicEventFixture::from_backend(backend);
    let fd = fixture.backend.platform.devices[0]
        .device
        .as_fd()
        .as_raw_fd();
    let recorder = fixture.recorders[&fd].clone();
    let (crtc_xid, crtc) = enable_gamma(&mut fixture.backend, &recorder, 0, 4);
    fixture.backend.scene.scene_structure_dirty = false;
    let (base, elapsed_ns) = install_controlled_clock(&mut fixture.backend);

    Backend::set_dpms_power(&mut fixture.backend, 3).expect("DPMS off through Backend");
    assert!(
        !fixture.backend.kms_outputs_active,
        "DPMS turns outputs off"
    );
    let crtc_key = CrtcKey::for_output(&fixture.backend.platform.outputs[0]);
    fixture
        .backend
        .platform
        .last_page_flip_received_at
        .remove(&crtc_key);
    assert!(!fixture.backend.scene.has_pending_page_flip(0));
    assert!(!fixture.backend.scene.output_submit_retry_pending(0));
    assert!(!fixture.backend.scene.owes_repaint());
    assert!(!fixture.backend.store.has_pending_presentation_damage());
    assert!(fixture.backend.pstate.direct.root().pending.is_none());
    assert!(
        fixture
            .backend
            .pstate
            .direct
            .root()
            .queued_successor
            .is_none()
    );
    let off_request_record_count = recorder.records().len();

    let red = vec![0x1111; 4];
    let green = vec![0x2222; 4];
    let blue = vec![0x3333; 4];
    Backend::set_crtc_gamma(&mut fixture.backend, crtc_xid, &red, &green, &blue)
        .expect("set CRTC gamma while DPMS is off");

    assert_eq!(
        recorder.records().len(),
        off_request_record_count,
        "SetCrtcGamma while DPMS is off submits no atomic commit"
    );
    assert!(
        !fixture.backend.platform.outputs[0]
            .output
            .gamma_lut_disabled
            .get(),
        "DPMS-off gamma does not latch GAMMA_LUT off"
    );
    assert_eq!(
        fixture.backend.platform.outputs[0]
            .output
            .pending_gamma_lut
            .borrow()
            .as_ref(),
        Some(&(red.clone(), green.clone(), blue.clone())),
        "DPMS-off gamma remains pending"
    );
    let first_deadline = fixture.backend.platform.outputs[0]
        .output
        .pending_gamma_deadline
        .get()
        .expect("DPMS-off gamma keeps the existing refresh deadline");
    assert_eq!(
        first_deadline,
        base + Duration::from_nanos(16_666_666),
        "DPMS-off gamma retains its one-refresh deadline"
    );

    Backend::set_dpms_power(&mut fixture.backend, 0).expect("DPMS on through Backend");
    assert!(fixture.backend.kms_outputs_active, "DPMS turns outputs on");
    assert!(
        fixture.backend.platform.outputs[0]
            .output
            .pending_gamma_lut
            .borrow()
            .is_some(),
        "DPMS-on reapply retains the pending LUT until the existing wake path sends it"
    );
    assert_eq!(
        fixture.backend.platform.outputs[0]
            .output
            .pending_gamma_deadline
            .get(),
        Some(first_deadline),
        "DPMS-on reapply preserves the pending deadline"
    );
    assert!(
        recorder.records()[off_request_record_count..]
            .iter()
            .all(|record| record.kind != AtomicCommitKind::GammaOnly),
        "DPMS-on reapply does not submit before the existing deadline"
    );

    elapsed_ns.store(16_666_667, Ordering::SeqCst);
    Backend::maybe_composite(&mut fixture.backend)
        .expect("expired post-wake gamma deadline submits");
    let first_gamma = recorder
        .records()
        .into_iter()
        .find(|record| {
            record.kind == AtomicCommitKind::GammaOnly
                && contains_crtc_property(record, crtc, handle(70))
        })
        .expect("post-wake GammaOnly commit carries GAMMA_LUT");
    assert!(
        accepted_event_id(&first_gamma).is_some(),
        "post-wake GAMMA_LUT commit has an injected P1 commit ID"
    );
    let first_blob_id = first_gamma
        .properties
        .iter()
        .find(|update| update.object == crtc.into() && update.property == handle(70))
        .expect("post-wake commit updates this CRTC's GAMMA_LUT")
        .value;
    let first_blob = recorder
        .created_property_blobs()
        .into_iter()
        .find(|(id, _)| *id == first_blob_id)
        .expect("post-wake commit's GAMMA_LUT blob was recorded")
        .1;
    let expected_first_blob: Vec<u8> = red
        .iter()
        .zip(&green)
        .zip(&blue)
        .flat_map(|((&red, &green), &blue)| {
            [red, green, blue, 0].into_iter().flat_map(u16::to_ne_bytes)
        })
        .collect();
    assert_eq!(
        first_blob, expected_first_blob,
        "stored ramp reaches the CRTC"
    );
    assert!(
        !fixture.backend.platform.outputs[0]
            .output
            .gamma_lut_disabled
            .get(),
        "successful post-wake LUT keeps gamma enabled"
    );
    assert!(
        fixture.backend.platform.outputs[0]
            .output
            .pending_gamma_lut
            .borrow()
            .is_none(),
        "successful post-wake commit consumes the pending LUT"
    );
    complete_recorded_attempt(&mut fixture, fd, &first_gamma, 1);

    let later_red = vec![0x4444; 4];
    let later_green = vec![0x5555; 4];
    let later_blue = vec![0x6666; 4];
    let later_request_record_count = recorder.records().len();
    Backend::set_crtc_gamma(
        &mut fixture.backend,
        crtc_xid,
        &later_red,
        &later_green,
        &later_blue,
    )
    .expect("later SetCrtcGamma still updates the CRTC");
    assert_eq!(recorder.records().len(), later_request_record_count);
    assert_eq!(
        fixture.backend.platform.outputs[0]
            .output
            .pending_gamma_lut
            .borrow()
            .as_ref(),
        Some(&(later_red.clone(), later_green.clone(), later_blue.clone())),
        "later SetCrtcGamma remains pending behind the recent flip"
    );

    elapsed_ns.store(33_333_334, Ordering::SeqCst);
    Backend::maybe_composite(&mut fixture.backend).expect("later gamma deadline submits");
    let later_gamma = recorder
        .records()
        .into_iter()
        .skip(later_request_record_count)
        .find(|record| {
            record.kind == AtomicCommitKind::GammaOnly
                && contains_crtc_property(record, crtc, handle(70))
        })
        .expect("later GammaOnly commit still carries GAMMA_LUT");
    assert!(accepted_event_id(&later_gamma).is_some());
    let later_blob_id = later_gamma
        .properties
        .iter()
        .find(|update| update.object == crtc.into() && update.property == handle(70))
        .expect("later commit updates this CRTC's GAMMA_LUT")
        .value;
    let later_blob = recorder
        .created_property_blobs()
        .into_iter()
        .find(|(id, _)| *id == later_blob_id)
        .expect("later GAMMA_LUT blob was recorded")
        .1;
    let expected_later_blob: Vec<u8> = later_red
        .iter()
        .zip(&later_green)
        .zip(&later_blue)
        .flat_map(|((&red, &green), &blue)| {
            [red, green, blue, 0].into_iter().flat_map(u16::to_ne_bytes)
        })
        .collect();
    assert_eq!(
        later_blob, expected_later_blob,
        "later ramp reaches the CRTC"
    );
    assert!(
        !fixture.backend.platform.outputs[0]
            .output
            .gamma_lut_disabled
            .get()
    );
    assert!(
        fixture.backend.platform.outputs[0]
            .output
            .pending_gamma_lut
            .borrow()
            .is_none()
    );
    complete_recorded_attempt(&mut fixture, fd, &later_gamma, 2);
    fixture.assert_quiescent();
}

#[test]
// Mutation: discard the pageflip timestamp when a newer idle sequence replaces the completion clock.
fn gamma_activity_survives_a_newer_idle_sequence_sample() {
    let (mut backend, recorder, crtc_xid, crtc) = gamma_backend_with_recorder();
    install_controlled_clock(&mut backend);
    seed_recent_gamma_page_flip(&mut backend, 0);

    let key = CrtcKey::for_output(&backend.platform.outputs[0]);
    let last_page_flip_received_at = backend
        .platform
        .present_get_last_page_flip_received_at(key)
        .expect("seeded pageflip retirement is retained");
    let sequence_time_ns = i64::try_from(
        last_page_flip_received_at
            .as_nanos()
            .saturating_add(10_000_000),
    )
    .expect("test sequence timestamp fits i64");
    let device_key = backend
        .platform
        .primary_device()
        .expect("gamma backend has a primary DRM device")
        .key;
    backend.on_crtc_sequence_event(device_key, u64::from(u32::from(crtc)), sequence_time_ns, 2);
    assert_eq!(
        backend.platform.present_get_completion_clock(key).source,
        PresentClockSource::IdleSequence,
        "the newer sequence replaces the completion-eligible sample"
    );

    set_test_gamma(&mut backend, crtc_xid, 0x2222);

    assert!(
        recorder.records().is_empty(),
        "a pageflip within two refresh periods keeps gamma active after a sequence event"
    );
    assert!(
        backend.platform.outputs[0]
            .output
            .pending_gamma_deadline
            .get()
            .is_some(),
        "active gamma retains its one-refresh deadline"
    );
}

#[test]
// Mutations: push the deadline on each update, or treat vrefresh 0 as zero Hz.
fn gamma_replacement_keeps_first_deadline_and_submits_the_second_ramp() {
    let (mut backend, recorder, crtc_xid, _) = gamma_backend_with_recorder();
    backend.platform.outputs[0].output.picked.vrefresh = 0;
    let (base, elapsed_ns) = install_controlled_clock(&mut backend);
    seed_recent_gamma_page_flip(&mut backend, 0);

    set_test_gamma(&mut backend, crtc_xid, 0x1111);
    let first_deadline = Backend::next_wakeup(&backend).expect("first gamma deadline");
    assert_eq!(
        first_deadline,
        base + Duration::from_nanos(16_666_666),
        "unknown refresh uses the 60 Hz period"
    );

    elapsed_ns.store(8_000_000, Ordering::SeqCst);
    set_test_gamma(&mut backend, crtc_xid, 0x4444);
    assert_eq!(
        Backend::next_wakeup(&backend),
        Some(first_deadline),
        "replacing the LUT must retain the first deadline"
    );
    assert!(recorder.records().is_empty());

    elapsed_ns.store(16_666_667, Ordering::SeqCst);
    Backend::maybe_composite(&mut backend).expect("first deadline submits replacement");
    let records = recorder.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].kind, AtomicCommitKind::GammaOnly);
    let blobs = recorder.created_property_blobs();
    assert_eq!(blobs.len(), 1);
    assert_eq!(
        &blobs[0].1[..8],
        &[0x44, 0x44, 0x45, 0x44, 0x46, 0x44, 0, 0],
        "the first deadline submits the latest ramp"
    );
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: set the deadline only for active CRTCs.
fn gamma_held_on_idle_crtc_by_other_output_gets_one_refresh_deadline() {
    let Some((fixture, direct)) = super::direct_unflip::direct_fixture(
        2,
        super::recording_scene::P1SceneRoute::Shared,
        17_005,
        0,
    ) else {
        eprintln!("skip: no Vulkan scene fixture available");
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    let fd = super::direct_unflip::fd(&fixture);
    let recorder = fixture.events.recorders[&fd].clone();
    let (crtc_xid, crtc) = enable_gamma(&mut fixture.events.backend, &recorder, 1, 4);
    let busy_crtc = fixture.events.backend.platform.outputs[0].output.crtc;
    assert_ne!(crtc, busy_crtc, "the outputs use distinct CRTCs");

    let idle_output = fixture.events.backend.platform.outputs.remove(1);
    let output_count = fixture.events.backend.platform.outputs.len();
    fixture
        .events
        .backend
        .pstate
        .direct
        .sync_output_count(output_count);
    let direct_records = super::direct_unflip::submit_direct(&mut fixture, &direct, fd);
    fixture.events.backend.platform.outputs.push(idle_output);
    let output_count = fixture.events.backend.platform.outputs.len();
    fixture
        .events
        .backend
        .pstate
        .direct
        .sync_output_count(output_count);
    let direct_record = direct_records
        .into_iter()
        .find(|record| accepted_event_id(record).is_some())
        .expect("output 0 direct frame is accepted through Backend");
    assert_eq!(
        direct_record.events.len(),
        1,
        "only output 0 owns the direct flip"
    );
    assert_eq!(direct_record.events[0].crtc, busy_crtc);
    let direct_id = accepted_event_id(&direct_record).expect("direct frame has a P1 commit ID");
    assert_eq!(
        fixture.events.backend.platform.devices[0]
            .atomic_commit_sink
            .for_tests_pending_page_flip_commit_ids(
                &fixture.events.backend.platform.devices[0].device,
                busy_crtc,
            ),
        [direct_id],
        "output 0's accepted direct flip remains in flight"
    );
    assert!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .pending
            .is_some()
    );
    assert!(
        !fixture.events.backend.platform.devices[0]
            .atomic_commit_sink
            .page_flip_commit_in_flight(&fixture.events.backend.platform.devices[0].device, crtc,),
        "output 1's CRTC is idle"
    );
    recorder.clear_records();

    let (base, elapsed_ns) = install_controlled_clock(&mut fixture.events.backend);
    set_test_gamma(&mut fixture.events.backend, crtc_xid, 0x6666);
    assert!(
        recorder.records().is_empty(),
        "queued output 0 work holds gamma"
    );
    assert!(
        fixture.events.backend.platform.outputs[1]
            .output
            .pending_gamma_lut
            .borrow()
            .is_some(),
        "the held LUT remains pending on output 1"
    );
    let deadline = base + Duration::from_nanos(16_666_666);
    let observed_deadline = fixture.events.backend.platform.outputs[1]
        .output
        .pending_gamma_deadline
        .get();
    let had_scheduled_wakeup = Backend::next_wakeup(&fixture.events.backend).is_some();

    elapsed_ns.store(16_666_665, Ordering::SeqCst);
    Backend::maybe_composite(&mut fixture.events.backend).expect("pre-deadline backend tick");
    let no_gamma_before_deadline = recorder.records().is_empty();
    let scheduled_no_later_than_deadline =
        Backend::next_wakeup(&fixture.events.backend).is_some_and(|wakeup| wakeup <= deadline);

    elapsed_ns.store(16_666_667, Ordering::SeqCst);
    Backend::maybe_composite(&mut fixture.events.backend).expect("expired gamma deadline tick");
    let records = recorder.records();
    let gamma_records: Vec<_> = records
        .iter()
        .filter(|record| record.kind == AtomicCommitKind::GammaOnly)
        .collect();
    let gamma_count = gamma_records.len();
    let gamma = gamma_records.first().copied().cloned();
    let gamma_targets_idle_crtc = gamma.as_ref().is_some_and(|gamma| {
        gamma.events.len() == 1
            && gamma.events[0].crtc == crtc
            && contains_crtc_property(gamma, crtc, handle(70))
            && accepted_event_id(gamma).is_some()
    });
    let pending_lut_cleared_after_deadline = fixture.events.backend.platform.outputs[1]
        .output
        .pending_gamma_lut
        .borrow()
        .is_none();

    if let Some(gamma) = &gamma {
        complete_recorded_attempt(&mut fixture.events, fd, gamma, 2);
    }
    super::direct_unflip::complete_direct_initial(&mut fixture, &direct_record, direct_id, fd);
    super::direct_unflip::drain_present_notifications(
        &mut fixture,
        &[direct.candidate.present_id],
        &[],
    );
    super::direct_unflip::request_unflip(&mut fixture, &direct);
    let unflip_offset = recorder.records().len();
    let unflip = super::direct_unflip::grouped_unflip_record(&mut fixture, fd, unflip_offset, 2);
    complete_recorded_attempt(&mut fixture.events, fd, &unflip, 3);
    super::direct_unflip::drain_present_notifications(
        &mut fixture,
        &[],
        &[direct.candidate.present_id],
    );
    fixture.finish_after_normal_drain();

    assert_eq!(
        observed_deadline,
        Some(deadline),
        "the idle CRTC's held LUT gets one refresh deadline"
    );
    assert!(
        had_scheduled_wakeup,
        "the Backend has a scheduled wake for pending work"
    );
    assert!(
        no_gamma_before_deadline,
        "no gamma-only commit goes out before the deadline"
    );
    assert!(
        scheduled_no_later_than_deadline,
        "the Backend remains scheduled no later than the gamma deadline"
    );
    assert_eq!(
        gamma_count, 1,
        "the deadline submits exactly one gamma-only commit"
    );
    assert!(
        gamma_targets_idle_crtc,
        "gamma-only targets output 1's CRTC with a P1 ID"
    );
    assert!(
        pending_lut_cleared_after_deadline,
        "accepted standalone commit consumes the pending LUT"
    );
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: omit the pending GAMMA_LUT from the next active scene flip.
fn p1_gamma_active_scene_carries_lut_on_its_next_vsync_flip() {
    let Some(fixture) = super::recording_scene::RecordingSceneFixture::new(1)
        .expect("recording scene fixture construction")
    else {
        eprintln!("skip: no Vulkan scene fixture available");
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    let (crtc_xid, crtc, fd, recorder) = enable_recording_scene_gamma(&mut fixture);
    let initial = super::atomic_events::submit_damaged_frame(&mut fixture);
    let initial_scene = initial
        .iter()
        .find(|(_, record)| {
            record.kind == AtomicCommitKind::Scene && accepted_event_id(record).is_some()
        })
        .expect("initial Backend scene frame is accepted")
        .1
        .clone();
    complete_recorded_attempt(&mut fixture.events, fd, &initial_scene, 1);
    seed_recent_gamma_page_flip(&mut fixture.events.backend, 0);
    recorder.clear_records();

    set_test_gamma(&mut fixture.events.backend, crtc_xid, 0x3333);
    assert!(
        recorder.records().is_empty(),
        "active scene gamma waits for vsync"
    );
    let root = fixture.events.backend.core.window_id;
    let (width, height) = (
        fixture.events.backend.platform.fb_w,
        fixture.events.backend.platform.fb_h,
    );
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
    .expect("queue scene damage through Backend");
    let attempts = super::atomic_events::submit_damaged_frame(&mut fixture);
    let scene = attempts
        .iter()
        .map(|(_, record)| record)
        .find(|record| {
            record.kind == AtomicCommitKind::Scene && accepted_event_id(record).is_some()
        })
        .expect("next Backend scene flip is accepted");
    assert!(contains_crtc_property(scene, crtc, handle(70)));
    assert!(
        recorder
            .records()
            .iter()
            .all(|record| record.kind != AtomicCommitKind::GammaOnly),
        "the active scene flip carries gamma without a gamma-only commit"
    );
    complete_recorded_attempt(&mut fixture.events, fd, scene, 2);
    assert!(
        fixture.events.backend.platform.outputs[0]
            .output
            .pending_gamma_lut
            .borrow()
            .is_none()
    );
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: decide activity from kernel UST again; omit GAMMA_LUT from the next direct frame.
fn p1_gamma_active_direct_frame_carries_lut_without_gamma_only_commit() {
    let Some((fixture, mut direct)) = super::direct_unflip::direct_fixture(
        2,
        super::recording_scene::P1SceneRoute::Shared,
        17_004,
        0,
    ) else {
        eprintln!("skip: no Vulkan scene fixture available");
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    let fd = super::direct_unflip::fd(&fixture);
    let recorder = fixture.events.recorders[&fd].clone();
    let (crtc_xid, crtc) = enable_gamma(&mut fixture.events.backend, &recorder, 0, 4);
    let initial = super::direct_unflip::submit_direct(&mut fixture, &direct, fd);
    let initial = initial
        .iter()
        .find(|record| accepted_event_id(record).is_some())
        .expect("initial direct Backend frame is accepted")
        .clone();
    let received_at = crate::drm::atomic::monotonic_timestamp()
        .expect("CLOCK_MONOTONIC is available in the test environment");
    let ust = received_at
        .checked_sub(Duration::from_secs(1))
        .expect("test receipt is at least one second into CLOCK_MONOTONIC");
    complete_recorded_attempt_at(
        &mut fixture.events,
        fd,
        &initial,
        50,
        Some((ust, received_at)),
    );
    assert!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .pending
            .is_none()
    );
    assert!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .current
            .is_some()
    );
    super::direct_unflip::drain_present_notifications(
        &mut fixture,
        &[direct.candidate.present_id],
        &[],
    );
    let events = &mut fixture.events;
    let received_at = events
        .backend
        .platform
        .present_get_last_page_flip_received_at(CrtcKey::for_output(
            &events.backend.platform.outputs[0],
        ))
        .expect("injected direct page-flip receipt is recorded");
    let receipt_age = crate::drm::atomic::monotonic_timestamp()
        .expect("CLOCK_MONOTONIC is available in the test environment")
        .checked_sub(received_at)
        .expect("event receipt is not in the future");
    assert!(
        receipt_age <= Duration::from_millis(100),
        "event receipt falls inside the last 100ms"
    );
    assert_eq!(
        events
            .backend
            .platform
            .present_get_completion_clock(CrtcKey::for_output(&events.backend.platform.outputs[0],))
            .ust,
        u64::try_from(ust.as_micros()).expect("test UST fits u64"),
        "Present retains the original kernel UST"
    );
    recorder.clear_records();

    set_test_gamma(&mut fixture.events.backend, crtc_xid, 0x5555);
    assert!(
        fixture.events.backend.platform.outputs[0]
            .output
            .pending_gamma_deadline
            .get()
            .is_some(),
        "fresh event receipt keeps the CRTC active despite its older UST"
    );
    assert!(
        recorder.records().is_empty(),
        "active direct gamma waits for a frame"
    );

    let previous_present_id = direct.candidate.present_id;
    let mut next_candidate = direct.candidate;
    next_candidate.present_id = next_candidate.present_id.wrapping_add(1);
    let mut next_event = direct.event.clone();
    next_event.present_id = next_candidate.present_id;
    next_event.serial = next_event.serial.wrapping_add(1);
    direct.candidate = next_candidate;
    direct.event = next_event.clone();
    let mut accepted = false;
    for _ in 0..8 {
        accepted = Backend::try_present_direct(
            &mut fixture.events.backend,
            next_candidate,
            next_event.clone(),
        )
        .expect("next direct frame through Backend");
        if accepted {
            break;
        }
    }
    assert!(accepted, "next direct frame reaches scanout");
    let direct_record = recorder
        .records()
        .into_iter()
        .find(|record| accepted_event_id(record).is_some())
        .expect("accepted direct frame has a recorded atomic commit");
    assert!(contains_crtc_property(&direct_record, crtc, handle(70)));
    assert!(
        recorder
            .records()
            .iter()
            .all(|record| record.kind != AtomicCommitKind::GammaOnly),
        "the direct frame carries gamma without a gamma-only commit"
    );
    let direct_id = accepted_event_id(&direct_record).expect("direct commit carries an ID");
    super::direct_unflip::complete_direct_initial(&mut fixture, &direct_record, direct_id, fd);
    super::direct_unflip::drain_present_notifications(
        &mut fixture,
        &[next_candidate.present_id],
        &[previous_present_id],
    );
    super::direct_unflip::request_unflip(&mut fixture, &direct);
    let unflip_offset = recorder.records().len();
    let unflip = super::direct_unflip::grouped_unflip_record(&mut fixture, fd, unflip_offset, 2);
    complete_recorded_attempt(&mut fixture.events, fd, &unflip, 3);
    super::direct_unflip::drain_present_notifications(
        &mut fixture,
        &[],
        &[next_candidate.present_id],
    );
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: latch gamma before the no-LUT retry; return the first EINVAL instead of retry EIO.
fn p1_gamma_direct_retry_eio_keeps_lut_pending_and_returns_retry_error() {
    let Some((fixture, direct)) = super::direct_unflip::direct_fixture(
        2,
        super::recording_scene::P1SceneRoute::Shared,
        17_006,
        0,
    ) else {
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    let fd = super::direct_unflip::fd(&fixture);
    let recorder = fixture.events.recorders[&fd].clone();
    let (crtc_xid, crtc) = enable_gamma(&mut fixture.events.backend, &recorder, 0, 4);
    let gamma_prop = handle(170);
    fixture.events.backend.platform.outputs[0]
        .output
        .crtc_gamma_lut_prop = Some(gamma_prop);
    fixture.events.backend.platform.outputs[0]
        .output
        .crtc_degamma_lut_prop = Some(handle(171));
    fixture.events.backend.platform.outputs[0]
        .output
        .crtc_ctm_prop = Some(handle(172));
    let initial = super::direct_unflip::submit_direct(&mut fixture, &direct, fd)
        .into_iter()
        .find(|record| accepted_event_id(record).is_some())
        .expect("initial direct Backend frame is accepted");
    super::direct_unflip::complete_direct_initial(
        &mut fixture,
        &initial,
        accepted_event_id(&initial).expect("initial direct frame has a commit ID"),
        fd,
    );
    super::direct_unflip::drain_present_notifications(
        &mut fixture,
        &[direct.candidate.present_id],
        &[],
    );

    install_controlled_clock(&mut fixture.events.backend);
    set_test_gamma(&mut fixture.events.backend, crtc_xid, 0x6789);
    assert!(
        recorder
            .records()
            .iter()
            .all(|record| { record.kind != AtomicCommitKind::GammaOnly })
    );
    assert!(
        fixture.events.backend.platform.outputs[0]
            .output
            .pending_gamma_lut
            .borrow()
            .is_some()
    );
    assert!(
        !fixture.events.backend.platform.outputs[0]
            .output
            .gamma_lut_disabled
            .get()
    );

    let record_offset = recorder.records().len();
    let blob_offset = recorder.created_property_blobs().len();
    recorder.script_result(Err(io::Error::from_raw_os_error(libc::EINVAL)));
    recorder.script_result(Err(io::Error::from_raw_os_error(libc::EIO)));
    let mut next_candidate = direct.candidate;
    next_candidate.present_id = next_candidate.present_id.wrapping_add(1);
    let mut next_event = direct.event.clone();
    next_event.present_id = next_candidate.present_id;
    next_event.serial = next_event.serial.wrapping_add(1);
    let mut submit_error = None;
    for _ in 0..8 {
        match Backend::try_present_direct(
            &mut fixture.events.backend,
            next_candidate,
            next_event.clone(),
        ) {
            Ok(false) => {}
            Ok(true) => panic!("the scripted direct frame must fail before acceptance"),
            Err(error) => {
                submit_error = Some(error);
                break;
            }
        }
    }
    let submit_error = submit_error.expect("the direct path reaches the scripted submits");
    assert_eq!(
        submit_error.raw_os_error(),
        Some(libc::EIO),
        "the low-level direct submit returns the no-LUT retry error"
    );
    let failed_attempts: Vec<_> = recorder.records().into_iter().skip(record_offset).collect();
    assert_eq!(
        failed_attempts.len(),
        2,
        "the direct frame is submitted twice"
    );
    assert!(contains_crtc_property(
        &failed_attempts[0],
        crtc,
        gamma_prop
    ));
    assert!(!contains_crtc_property(
        &failed_attempts[1],
        crtc,
        gamma_prop
    ));
    for layout in &fixture.events.backend.platform.outputs {
        let fb_id = |record: &AtomicCommitRecord| {
            record
                .properties
                .iter()
                .find(|update| {
                    update.object == layout.output.plane.into()
                        && update.property == layout.output.plane_fb_id_prop
                })
                .map(|update| update.value)
        };
        let first_fb_id = fb_id(&failed_attempts[0]);
        let retry_fb_id = fb_id(&failed_attempts[1]);
        assert!(first_fb_id.is_some(), "the direct frame updates each plane");
        assert_eq!(
            first_fb_id, retry_fb_id,
            "the no-LUT retry keeps each direct framebuffer"
        );
    }
    assert!(matches!(
        failed_attempts[0].result,
        ScriptedResult::Error { raw_os_error: Some(error), .. } if error == libc::EINVAL
    ));
    assert!(matches!(
        failed_attempts[1].result,
        ScriptedResult::Error { raw_os_error: Some(error), .. } if error == libc::EIO
    ));
    assert!(
        !fixture.events.backend.platform.outputs[0]
            .output
            .gamma_lut_disabled
            .get()
    );
    assert!(
        fixture.events.backend.platform.outputs[0]
            .output
            .pending_gamma_lut
            .borrow()
            .is_some()
    );
    assert!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .pending
            .is_none()
    );
    assert!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .queued_successor
            .is_none()
    );
    assert_eq!(
        fixture.events.backend.platform.devices[0]
            .atomic_commit_sink
            .for_tests_pending_page_flip_entry_count(),
        0,
        "failed direct attempts install no flip obligation"
    );
    let created: Vec<_> = recorder
        .created_property_blobs()
        .into_iter()
        .skip(blob_offset)
        .map(|(blob, _)| blob)
        .collect();
    let destroyed: Vec<_> = recorder
        .destroyed_property_blobs()
        .into_iter()
        .filter(|blob| created.contains(blob))
        .collect();
    assert_eq!(
        created.len(),
        1,
        "the rejected direct frame creates one LUT blob"
    );
    assert_eq!(
        destroyed, created,
        "the rejected LUT blob is destroyed once"
    );

    let unflip_offset = recorder.records().len();
    let unflip = super::direct_unflip::grouped_unflip_record(&mut fixture, fd, unflip_offset, 2);
    assert!(contains_crtc_property(&unflip, crtc, gamma_prop));
    assert!(
        !fixture.events.backend.platform.outputs[0]
            .output
            .gamma_lut_disabled
            .get()
    );
    assert!(
        fixture.events.backend.platform.outputs[0]
            .output
            .pending_gamma_lut
            .borrow()
            .is_none()
    );
    complete_recorded_attempt(&mut fixture.events, fd, &unflip, 2);
    super::direct_unflip::drain_present_notifications(
        &mut fixture,
        &[],
        &[direct.candidate.present_id],
    );
    fixture.finish_after_normal_drain();
}

#[test]
// Mutation: decide CRTC activity from the recent kernel UST again.
fn gamma_idle_crtc_uses_old_receipt_even_when_kernel_ust_is_recent() {
    let (mut events, recorder, crtc_xid, crtc, fd) = gamma_event_fixture();
    let now = crate::drm::atomic::monotonic_timestamp()
        .expect("CLOCK_MONOTONIC is available in the test environment");
    let received_at = now
        .checked_sub(Duration::from_secs(3))
        .expect("test CLOCK_MONOTONIC age is available");
    let ust = now
        .checked_sub(Duration::from_millis(10))
        .expect("test CLOCK_MONOTONIC has a recent UST");
    set_test_gamma(&mut events.backend, crtc_xid, 0x3333);
    let accepted_gamma = recorder
        .records()
        .into_iter()
        .find(|record| record.kind == AtomicCommitKind::GammaOnly)
        .expect("initial idle gamma uses an accepted standalone commit");
    assert!(accepted_event_id(&accepted_gamma).is_some());
    complete_recorded_attempt_at(
        &mut events,
        fd,
        &accepted_gamma,
        100,
        Some((ust, received_at)),
    );
    recorder.clear_records();

    set_test_gamma(&mut events.backend, crtc_xid, 0x4444);

    let records = recorder.records();
    assert_eq!(
        records.len(),
        1,
        "idle gamma submits its standalone commit at once"
    );
    assert_eq!(records[0].kind, AtomicCommitKind::GammaOnly);
    assert!(contains_crtc_property(&records[0], crtc, handle(70)));
    assert!(
        events.backend.platform.outputs[0]
            .output
            .pending_gamma_lut
            .borrow()
            .is_none(),
        "accepted idle commit consumes the pending LUT"
    );
}
