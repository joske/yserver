use super::*;

use crate::drm::atomic::{
    AtomicCommitKind, AtomicCommitSink,
    tests_support::{AtomicCommitRecord, RecordingAtomicCommitExecutor},
};
use std::collections::HashSet;

fn recording_sink(
    backend: &mut KmsBackend,
    device_idx: usize,
) -> Rc<RecordingAtomicCommitExecutor> {
    let recorder = Rc::new(RecordingAtomicCommitExecutor::default());
    backend.platform.devices[device_idx].atomic_commit_sink =
        Rc::new(AtomicCommitSink::new(recorder.clone()));
    recorder
}

fn raw<T: ::drm::control::ResourceHandle>(value: u32) -> T {
    ::drm::control::from_u32(value).expect("nonzero test handle")
}

fn assert_cursor_properties(
    record: &AtomicCommitRecord,
    position: crate::drm::atomic::CursorPlanePosition,
) {
    for (property, value) in [
        (position.crtc_x, i64::from(position.x) as u64),
        (position.crtc_y, i64::from(position.y) as u64),
    ] {
        assert!(
            record.properties.iter().any(|update| {
                update.object == position.plane.into()
                    && update.property == property
                    && update.value == value
            }),
            "missing cursor property {property:?}={value} on {:?}: {:?}",
            position.plane,
            record.properties,
        );
    }
}

fn set_scene_fence_properties(output: &mut crate::platform::drm::Output) {
    output.plane_in_fence_fd_prop = Some(raw(81));
    output.crtc_out_fence_ptr_prop = Some(raw(82));
}

#[test]
// Mutation: omit `request.add_cursor_position` from the shared scene-flip
// builder. The recorded request must then miss these latest-position values.
fn shared_scene_flip_records_the_latest_cursor_position() {
    let mut backend = KmsBackend::for_tests();
    let recorder = recording_sink(&mut backend, 0);
    let device_key = backend.platform.devices[0].key;
    let position = backend.platform.for_tests_seed_atomic_cursor_position(
        0,
        raw(51),
        raw(61),
        raw(62),
        123,
        45,
        3,
        4,
    );
    assert_eq!((position.x, position.y), (120, 41));
    let positions = backend
        .platform
        .cursor_plane_atomic_positions_for_outputs(&[0]);
    assert_eq!(positions, vec![position]);

    set_scene_fence_properties(&mut backend.platform.outputs[0].output);
    let output = &backend.platform.outputs[0].output;
    let mut out_fence = -1;
    crate::drm::page_flip::submit_scene_flip_with_fences(
        &backend.platform.devices[0].device,
        &backend.platform.devices[0].atomic_commit_sink,
        output,
        raw(71),
        -1,
        &mut out_fence,
        &positions,
        crate::drm::async_flip::FlipTiming::Vsync,
    )
    .expect("record shared scene flip");

    let records = recorder.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].kind, AtomicCommitKind::SceneWithCursor);
    assert!(
        records[0]
            .flags
            .contains(::drm::control::AtomicCommitFlags::NONBLOCK)
    );
    assert_cursor_properties(&records[0], position);
    backend
        .platform
        .cursor_plane_scene_commit_submitted(&positions);
    assert!(
        !backend
            .platform
            .for_tests_atomic_position_is_unsent(device_key, position.crtc)
    );
}

#[test]
// Mutation: source the copied flip's cursor plane from the renderer device.
// Both devices intentionally reuse the raw CRTC handle with different planes.
fn copied_scene_flip_uses_the_sink_devices_cursor_plane() {
    let mut backend = KmsBackend::for_tests();
    let renderer_key = backend.platform.devices[0].key;
    let sink_key = test_device_key(77);
    push_test_device(&mut backend, sink_key);
    backend.platform.outputs[0].key.device_key = renderer_key;
    let renderer_position = backend.platform.for_tests_seed_atomic_cursor_position(
        0,
        raw(151),
        raw(161),
        raw(162),
        100,
        90,
        0,
        0,
    );
    backend.platform.outputs[0].key.device_key = sink_key;
    let sink_position = backend.platform.for_tests_seed_atomic_cursor_position(
        0,
        raw(251),
        raw(261),
        raw(262),
        42,
        33,
        0,
        0,
    );
    assert_ne!(renderer_position.plane, sink_position.plane);

    let recorder = recording_sink(&mut backend, 1);
    set_scene_fence_properties(&mut backend.platform.outputs[0].output);
    let positions = backend
        .platform
        .cursor_plane_atomic_positions_for_outputs(&[0]);
    assert_eq!(positions, vec![sink_position]);
    let sink_device = &backend.platform.devices[1];
    let mut out_fence = -1;
    crate::drm::page_flip::submit_scene_flip_with_fences(
        &sink_device.device,
        &sink_device.atomic_commit_sink,
        &backend.platform.outputs[0].output,
        raw(71),
        -1,
        &mut out_fence,
        &positions,
        crate::drm::async_flip::FlipTiming::Vsync,
    )
    .expect("record copied sink flip");

    let records = recorder.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].kind, AtomicCommitKind::SceneWithCursor);
    assert_cursor_properties(&records[0], sink_position);
    backend
        .platform
        .cursor_plane_scene_commit_submitted(&positions);
    assert!(
        backend
            .platform
            .for_tests_atomic_position_is_unsent(renderer_key, renderer_position.crtc)
    );
    assert!(
        !backend
            .platform
            .for_tests_atomic_position_is_unsent(sink_key, sink_position.crtc)
    );
}

#[test]
// Mutation: return atomic positions for every visible plane without checking
// the per-device atomic capability. The scene flip would gain cursor fields.
fn scene_flip_without_atomic_cursor_capability_has_no_cursor_properties() {
    let mut backend = KmsBackend::for_tests();
    let recorder = recording_sink(&mut backend, 0);
    let device_key = backend.platform.devices[0].key;
    let position = backend.platform.for_tests_seed_atomic_cursor_position(
        0,
        raw(51),
        raw(61),
        raw(62),
        123,
        45,
        3,
        4,
    );
    backend.platform.devices[0].cursor.atomic_cursor = false;
    let positions = backend
        .platform
        .cursor_plane_atomic_positions_for_outputs(&[0]);
    assert!(positions.is_empty());

    set_scene_fence_properties(&mut backend.platform.outputs[0].output);
    let mut out_fence = -1;
    crate::drm::page_flip::submit_scene_flip_with_fences(
        &backend.platform.devices[0].device,
        &backend.platform.devices[0].atomic_commit_sink,
        &backend.platform.outputs[0].output,
        raw(71),
        -1,
        &mut out_fence,
        &positions,
        crate::drm::async_flip::FlipTiming::Vsync,
    )
    .expect("record legacy scene flip");

    let records = recorder.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].kind, AtomicCommitKind::Scene);
    assert!(
        !records[0]
            .properties
            .iter()
            .any(|update| update.object == position.plane.into())
    );
    assert!(
        backend
            .platform
            .for_tests_atomic_position_is_unsent(device_key, position.crtc)
    );
}

fn backend_with_live_scene() -> Option<KmsBackend> {
    let mut backend = match KmsBackend::for_tests_with_vk() {
        Ok(backend) => backend,
        Err(error) => {
            eprintln!("skipping: no Vulkan test device: {error}");
            return None;
        }
    };
    let layouts = backend.platform.scene_output_layouts();
    backend.scene = crate::kms::render::scene::SceneCompositor::new(&backend.platform, &layouts)
        .expect("construct hardware-free live scene");
    install_visible_cursor_via_backend(&mut backend);
    backend.scene.for_tests_set_hw_cursor_mode();
    backend.scene.scene_structure_dirty = false;
    for drawable in backend.store.armed_damaged_ids() {
        if let Some(snapshot) = backend.store.peek_presentation_damage(drawable) {
            backend.store.ack_presentation_damage(snapshot);
        }
    }
    Some(backend)
}

fn install_visible_cursor_via_backend(backend: &mut KmsBackend) {
    use yserver_core::backend::Backend;

    let source = Backend::create_pixmap(backend, None, 1, 1, 1).expect("cursor source pixmap");
    Backend::put_image(backend, None, source.as_raw(), 1, 1, 1, 0, 0, &[1, 0, 0, 0])
        .expect("paint visible cursor pixel");
    let cursor = Backend::create_cursor(
        backend,
        None,
        source,
        None,
        (u16::MAX, u16::MAX, u16::MAX),
        (0, 0, 0),
        0,
        0,
    )
    .expect("create visible cursor");
    Backend::define_cursor(backend, None, backend.core.window_id, cursor.as_raw())
        .expect("define visible root cursor");
    assert!(!backend.effective_cursor_hidden());
}

fn backend_with_hidden_dead_zone_scene() -> Option<KmsBackend> {
    let mut backend = match KmsBackend::for_tests_with_vk() {
        Ok(backend) => backend,
        Err(error) => {
            eprintln!("skipping: no Vulkan test device: {error}");
            return None;
        }
    };
    push_test_output(&mut backend, 0xCA11);
    backend.platform.outputs[1].x = 900;
    backend.platform.outputs[1].width = 800;
    backend.platform.outputs[1].height = 400;
    backend.platform.fb_w = 1700;
    backend.platform.fb_h = 600;
    let layouts = backend.platform.scene_output_layouts();
    backend.scene = crate::kms::render::scene::SceneCompositor::new(&backend.platform, &layouts)
        .expect("construct dead-zone live scene");
    backend.scene.scene_structure_dirty = false;
    for drawable in backend.store.armed_damaged_ids() {
        if let Some(snapshot) = backend.store.peek_presentation_damage(drawable) {
            backend.store.ack_presentation_damage(snapshot);
        }
    }
    Some(backend)
}

fn seed_cursor_position(backend: &mut KmsBackend) -> crate::drm::atomic::CursorPlanePosition {
    backend.platform.for_tests_seed_atomic_cursor_position(
        0,
        raw(51),
        raw(61),
        raw(62),
        100,
        100,
        0,
        0,
    )
}

fn warp(backend: &mut KmsBackend, state: &mut yserver_core::server::ServerState, x: i32, y: i32) {
    use yserver_core::backend::Backend;
    Backend::warp_pointer_root(backend, state, x, y);
}

fn maybe_composite(backend: &mut KmsBackend) {
    use yserver_core::backend::Backend;
    Backend::maybe_composite(backend).expect("run backend scheduling pass");
}

fn retire_test_flip(
    backend: &mut KmsBackend,
    state: &mut yserver_core::server::ServerState,
    frame: u32,
) {
    use std::os::fd::{AsFd as _, AsRawFd as _};
    use yserver_core::backend::Backend;

    let output = &backend.platform.outputs[0].output;
    let crtc = output.crtc;
    backend.platform.devices[0]
        .test_page_flip_event_source
        .as_ref()
        .expect("test event source")
        .queue_flip(crtc, frame);
    let drm_fd = backend.platform.devices[0].device.as_fd().as_raw_fd();
    Backend::on_page_flip_ready(backend, state, drm_fd);
}

fn assert_no_cursor_work(backend: &KmsBackend, device_key: crate::platform::drm::DrmDeviceKey) {
    let crtc = backend.platform.outputs[0].output.crtc;
    assert!(
        !backend
            .platform
            .for_tests_atomic_cursor_commit_is_in_flight(device_key, crtc)
    );
    assert!(
        backend
            .platform
            .for_tests_atomic_cursor_pending_position(device_key, crtc)
            .is_none()
    );
}

#[test]
// Mutation: retain the first position instead of replacing it with the latest.
fn idle_motion_coalesces_until_cursor_flip_retires() {
    use yserver_core::server::ServerState;

    let Some(mut backend) = backend_with_live_scene() else {
        return;
    };
    let recorder = recording_sink(&mut backend, 0);
    let device_key = backend.platform.devices[0].key;
    seed_cursor_position(&mut backend);
    let mut state = ServerState::new();

    warp(&mut backend, &mut state, 120, 80);
    warp(&mut backend, &mut state, 180, 130);
    let first_pending = backend
        .platform
        .cursor_plane_atomic_positions_for_outputs(&[0]);
    assert_eq!(first_pending.len(), 1);
    assert!(first_pending[0].motion_timestamp.is_some());
    assert!(
        recorder.records().is_empty(),
        "motion must not submit directly"
    );
    assert!(
        yserver_core::backend::Backend::next_wakeup(&backend)
            .is_some_and(|wakeup| wakeup <= std::time::Instant::now())
    );
    assert!(backend.scanout_allowed());
    assert!(backend.kms_outputs_active);
    maybe_composite(&mut backend);
    assert_eq!(
        backend.platform.devices[0]
            .atomic_commit_sink
            .for_tests_motion_to_visible_sample_count(),
        0,
        "motion is not visible until its commit retires"
    );

    let first_records = recorder.records();
    assert_eq!(first_records.len(), 1);
    assert_eq!(first_records[0].kind, AtomicCommitKind::CursorOnly);
    assert_cursor_properties(&first_records[0], first_pending[0]);
    assert!(
        backend
            .platform
            .for_tests_atomic_cursor_commit_is_in_flight(device_key, first_pending[0].crtc)
    );
    assert!(
        !backend
            .platform
            .for_tests_atomic_position_is_unsent(device_key, first_pending[0].crtc)
    );

    warp(&mut backend, &mut state, 260, 170);
    let latest = backend
        .platform
        .for_tests_atomic_cursor_pending_position(device_key, first_pending[0].crtc)
        .expect("motion during the cursor flip stays pending");
    assert!(latest.motion_timestamp.is_some());
    maybe_composite(&mut backend);
    assert_eq!(
        recorder.records().len(),
        1,
        "the in-flight commit owns the CRTC"
    );

    retire_test_flip(&mut backend, &mut state, 1);
    assert_eq!(
        backend.platform.devices[0]
            .atomic_commit_sink
            .for_tests_motion_to_visible_sample_count(),
        1,
        "the first cursor flip records motion-to-visible"
    );
    assert!(
        !backend
            .platform
            .for_tests_atomic_cursor_commit_is_in_flight(device_key, latest.crtc)
    );
    maybe_composite(&mut backend);

    let records = recorder.records();
    assert_eq!(records.len(), 2);
    assert_eq!(records[1].kind, AtomicCommitKind::CursorOnly);
    assert_cursor_properties(&records[1], latest);
    assert!(
        backend
            .platform
            .for_tests_atomic_cursor_commit_is_in_flight(device_key, latest.crtc)
    );
    retire_test_flip(&mut backend, &mut state, 2);
    assert_eq!(
        backend.platform.devices[0]
            .atomic_commit_sink
            .for_tests_motion_to_visible_sample_count(),
        2,
        "the latest cursor flip records motion-to-visible"
    );
    assert_no_cursor_work(&backend, device_key);
}

#[test]
// Mutation: restore Task 6's Hidden fast-path skip. Re-entry must wake the
// scene through the Backend pointer entry and make this predicate true.
fn hidden_cursor_motion_from_dead_zone_wakes_scene_on_reentry() {
    use yserver_core::{backend::Backend, server::ServerState};

    let Some(mut backend) = backend_with_hidden_dead_zone_scene() else {
        return;
    };
    install_visible_cursor_via_backend(&mut backend);
    let mut state = ServerState::new();
    Backend::warp_pointer_root(&mut backend, &mut state, 850, 450);
    // Model the initial retirement at a location covered by neither output.
    backend.scene.scene_structure_dirty = false;
    assert!(!backend.cursor_hidden, "this is not XFIXES HideCursor");
    assert_eq!(
        backend.scene.cursor_mode(),
        crate::kms::render::scene::CursorPlaneMode::Hidden,
    );
    assert!(!backend.for_tests_scene_wants_compose());

    // The gap and the shorter output's lower edge form the dead zone.
    assert_eq!(
        (backend.core.cursor_x, backend.core.cursor_y),
        (850.0, 450.0)
    );
    assert_eq!(
        backend.scene.cursor_mode(),
        crate::kms::render::scene::CursorPlaneMode::Hidden,
    );
    // Model the scene consuming that wake. The next pointer event alone must
    // re-arm the scheduling predicate when it returns to output 1.
    backend.scene.scene_structure_dirty = false;
    assert!(!backend.for_tests_scene_wants_compose());
    Backend::warp_pointer_root(&mut backend, &mut state, 950, 350);
    assert_eq!(
        (backend.core.cursor_x, backend.core.cursor_y),
        (950.0, 350.0)
    );
    assert!(backend.for_tests_scene_wants_compose());
}

#[test]
// Mutation: ignore the scene flip blocker and send the position as cursor-only.
fn motion_during_scene_flip_waits_and_scene_request_carries_latest_position() {
    use yserver_core::server::ServerState;

    let Some(mut backend) = backend_with_live_scene() else {
        return;
    };
    let recorder = recording_sink(&mut backend, 0);
    let device_key = backend.platform.devices[0].key;
    seed_cursor_position(&mut backend);
    let crtc = backend.platform.outputs[0].output.crtc;
    backend.scene.test_set_flip_in_flight(true);
    let mut state = ServerState::new();

    warp(&mut backend, &mut state, 210, 95);
    let latest = backend
        .platform
        .for_tests_atomic_cursor_pending_position(device_key, crtc)
        .expect("motion remains unsent while a scene flip is pending");
    maybe_composite(&mut backend);
    assert!(recorder.records().is_empty());

    retire_test_flip(&mut backend, &mut state, 1);
    backend.scene.test_set_flip_in_flight(false);
    let positions = backend
        .platform
        .cursor_plane_atomic_positions_for_outputs(&[0]);
    assert_eq!(positions, vec![latest]);
    set_scene_fence_properties(&mut backend.platform.outputs[0].output);
    let mut out_fence = -1;
    crate::drm::page_flip::submit_scene_flip_with_fences(
        &backend.platform.devices[0].device,
        &backend.platform.devices[0].atomic_commit_sink,
        &backend.platform.outputs[0].output,
        raw(71),
        -1,
        &mut out_fence,
        &positions,
        crate::drm::async_flip::FlipTiming::Vsync,
    )
    .expect("record the next scene request");
    backend
        .platform
        .cursor_plane_scene_commit_submitted(&positions);
    retire_test_flip(&mut backend, &mut state, 2);

    let records = recorder.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].kind, AtomicCommitKind::SceneWithCursor);
    assert_cursor_properties(&records[0], latest);
    assert_no_cursor_work(&backend, device_key);
}

#[test]
// Mutation: make `output_commit_in_flight` always false in the backend sink.
fn cursor_only_commit_blocks_scene_readiness_until_its_flip_retires() {
    use crate::kms::render::scene::OutputSink;
    use yserver_core::server::ServerState;

    let Some(mut backend) = backend_with_live_scene() else {
        return;
    };
    let _recorder = recording_sink(&mut backend, 0);
    let device_key = backend.platform.devices[0].key;
    seed_cursor_position(&mut backend);
    let mut state = ServerState::new();
    warp(&mut backend, &mut state, 230, 160);
    maybe_composite(&mut backend);

    assert!(OutputSink::output_commit_in_flight(&backend.platform, 0));
    assert!(!backend.scene.has_output_ready_for_submit(&backend.platform));
    retire_test_flip(&mut backend, &mut state, 1);
    assert!(!OutputSink::output_commit_in_flight(&backend.platform, 0));
    assert!(backend.scene.has_output_ready_for_submit(&backend.platform));
    assert_no_cursor_work(&backend, device_key);
}

#[test]
// Mutation: let pointer motion submit cursor-only immediately, bypassing scene priority.
fn scene_damage_after_cursor_retirement_submits_scene_with_latest_position() {
    use yserver_core::server::ServerState;

    let Some(mut backend) = backend_with_live_scene() else {
        return;
    };
    let recorder = recording_sink(&mut backend, 0);
    let device_key = backend.platform.devices[0].key;
    seed_cursor_position(&mut backend);
    let mut state = ServerState::new();

    warp(&mut backend, &mut state, 90, 70);
    maybe_composite(&mut backend);
    assert_eq!(recorder.records().len(), 1);
    Backend::mark_dirty(&mut backend);
    maybe_composite(&mut backend);
    assert_eq!(
        recorder.records().len(),
        1,
        "scene waits for cursor retirement"
    );

    retire_test_flip(&mut backend, &mut state, 1);
    warp(&mut backend, &mut state, 340, 210);
    let latest = backend
        .platform
        .for_tests_atomic_cursor_pending_position(
            device_key,
            backend.platform.outputs[0].output.crtc,
        )
        .expect("post-retirement motion remains pending for the scene");
    maybe_composite(&mut backend);
    assert_eq!(
        recorder.records().len(),
        1,
        "scene arbitration precedes cursor-only"
    );

    let positions = backend
        .platform
        .cursor_plane_atomic_positions_for_outputs(&[0]);
    assert_eq!(positions, vec![latest]);
    set_scene_fence_properties(&mut backend.platform.outputs[0].output);
    let mut out_fence = -1;
    crate::drm::page_flip::submit_scene_flip_with_fences(
        &backend.platform.devices[0].device,
        &backend.platform.devices[0].atomic_commit_sink,
        &backend.platform.outputs[0].output,
        raw(72),
        -1,
        &mut out_fence,
        &positions,
        crate::drm::async_flip::FlipTiming::Vsync,
    )
    .expect("record prioritized scene request");
    backend
        .platform
        .cursor_plane_scene_commit_submitted(&positions);
    let records = recorder.records();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].kind, AtomicCommitKind::CursorOnly);
    assert_eq!(records[1].kind, AtomicCommitKind::SceneWithCursor);
    assert_cursor_properties(&records[1], latest);
    retire_test_flip(&mut backend, &mut state, 2);
    assert_no_cursor_work(&backend, device_key);
}
#[test]
// Mutation: do not set `cursor_retry_waits_for_flip` after a cursor-only EBUSY.
fn cursor_only_ebusy_waits_for_the_next_flip_before_retrying() {
    use yserver_core::server::ServerState;

    let Some(mut backend) = backend_with_live_scene() else {
        return;
    };
    let recorder = recording_sink(&mut backend, 0);
    let device_key = backend.platform.devices[0].key;
    let position = seed_cursor_position(&mut backend);
    let mut state = ServerState::new();
    recorder.script_result(Err(io::Error::from_raw_os_error(libc::EBUSY)));

    warp(&mut backend, &mut state, 180, 130);
    maybe_composite(&mut backend);
    let crtc = position.crtc;
    assert_eq!(recorder.records().len(), 1);
    assert_eq!(recorder.records()[0].kind, AtomicCommitKind::CursorOnly);
    assert_eq!(
        backend.platform.devices[0]
            .atomic_commit_sink
            .for_tests_ebusy_count(),
        1,
    );
    assert!(backend.platform.cursor_retry_waits_for_flip(0));
    assert!(
        backend
            .platform
            .for_tests_atomic_position_is_unsent(device_key, crtc)
    );
    assert!(
        !backend
            .platform
            .for_tests_atomic_cursor_commit_is_in_flight(device_key, crtc)
    );
    assert!(!backend.for_tests_scene_wants_compose());

    for _ in 0..3 {
        maybe_composite(&mut backend);
        assert_eq!(
            recorder.records().len(),
            1,
            "no cursor-only retry occurs before a flip event",
        );
        assert!(!backend.for_tests_scene_wants_compose());
    }

    warp(&mut backend, &mut state, 260, 170);
    let latest = backend
        .platform
        .for_tests_atomic_cursor_pending_position(device_key, crtc)
        .expect("new motion replaces the unsent position");
    assert!(backend.platform.cursor_retry_waits_for_flip(0));
    maybe_composite(&mut backend);
    assert_eq!(recorder.records().len(), 1);
    assert!(!backend.for_tests_scene_wants_compose());

    retire_test_flip(&mut backend, &mut state, 1);
    assert!(!backend.platform.cursor_retry_waits_for_flip(0));
    assert!(backend.for_tests_scene_wants_compose());
    maybe_composite(&mut backend);
    let records = recorder.records();
    assert_eq!(records.len(), 2);
    assert_eq!(records[1].kind, AtomicCommitKind::CursorOnly);
    assert_cursor_properties(&records[1], latest);
    assert!(
        backend
            .platform
            .for_tests_atomic_cursor_commit_is_in_flight(device_key, crtc)
    );
    retire_test_flip(&mut backend, &mut state, 2);
    assert_no_cursor_work(&backend, device_key);
}

fn has_cursor_property(
    record: &AtomicCommitRecord,
    position: crate::drm::atomic::CursorPlanePosition,
) -> bool {
    record
        .properties
        .iter()
        .any(|update| update.object == position.plane.into())
}

fn atomic_cursor_fixture(
    test_name: &str,
) -> Option<super::cursor_sprites::support::AtomicPathFixture> {
    let fixture = super::cursor_sprites::support::AtomicPathFixture::new(
        super::cursor_sprites::support::PathKind::Physical,
    )
    .expect("Atomic cursor scene fixture construction");
    if fixture.is_none() {
        println!("SKIP {test_name}: no Vulkan test device");
    }
    fixture
}

fn establish_test_cursor(
    fixture: &mut super::cursor_sprites::support::AtomicPathFixture,
    delivered: &mut HashSet<super::cursor_sprites::support::CompletionKey>,
) -> super::cursor_sprites::support::HardwareCursor {
    let cursor = super::cursor_sprites::support::create_argb_cursor(
        &mut fixture.scene.events.backend,
        (8, 8),
        [0xff, 0xff, 0xff, 0xff],
        (0, 0),
    );
    super::cursor_sprites::support::establish_hardware_cursor(
        fixture,
        delivered,
        cursor,
        (120, 100),
    )
}

#[test]
#[ignore = "requires the lavapipe Vulkan scene fixture"]
// Mutation: restore non-EBUSY scene attribution. Each generic failure carries
// a scripted failed full TEST_ONLY and successful stripped TEST_ONLY; that
// must never detach or release the applied cursor through Backend.
fn scene_enomem_and_eio_do_not_recover_the_position_carrying_cursor() {
    use super::cursor_sprites::support as cursor_support;
    use yserver_core::backend::Backend;

    for errno in [libc::ENOMEM, libc::EIO] {
        let Some(mut fixture) = atomic_cursor_fixture(
            "scene_enomem_and_eio_do_not_recover_the_position_carrying_cursor",
        ) else {
            return;
        };
        let mut delivered = HashSet::new();
        let hardware = establish_test_cursor(&mut fixture, &mut delivered);
        let backend = &fixture.scene.events.backend;
        let binding = cursor_support::applied_cursor_binding(backend, hardware.output_idx)
            .expect("Backend established an applied hardware cursor");
        let device_key = backend.platform.outputs[hardware.output_idx].key.device_key;
        let position = backend
            .platform
            .cursor_plane_atomic_positions_for_outputs(&[0])[0];
        let recorder = Rc::clone(&fixture.scene.events.recorders[&hardware.device_fd]);
        let start = cursor_support::record_count(&fixture, hardware.device_fd);
        cursor_support::damage_root(&mut fixture);
        recorder.script_result(Err(io::Error::from_raw_os_error(errno)));
        recorder.script_result(Err(io::Error::from_raw_os_error(libc::EINVAL)));
        recorder.script_result(Ok(()));

        Backend::maybe_composite(&mut fixture.scene.events.backend)
            .expect("submit the position-carrying scene through Backend");

        let records = recorder.records();
        assert_eq!(
            records.len(),
            start + 1,
            "generic scene failure has no cursor probes or detach"
        );
        let failed = &records[start];
        assert_eq!(failed.kind, AtomicCommitKind::SceneWithCursor);
        assert!(has_cursor_property(failed, position));
        assert!(matches!(
            failed.result,
            crate::drm::atomic::tests_support::ScriptedResult::Error {
                raw_os_error: Some(found),
                ..
            } if found == errno
        ));
        assert_eq!(
            cursor_support::applied_cursor_binding(
                &fixture.scene.events.backend,
                hardware.output_idx,
            ),
            Some(binding),
            "generic scene failure retains the applied binding",
        );
        assert_eq!(
            cursor_support::sprite_users(
                &fixture.scene.events.backend,
                hardware.output_idx,
                binding.lease,
            ),
            Some(1),
            "generic scene failure retains the lease user",
        );
        assert_eq!(
            fixture
                .scene
                .events
                .backend
                .platform
                .for_tests_cursor_fallback_remaining(device_key, hardware.crtc),
            None,
            "generic scene failure does not change cursor qualification",
        );

        recorder.clear_scripted_results();
        let cleanup_start = cursor_support::record_count(&fixture, hardware.device_fd);
        fixture
            .scene
            .events
            .backend
            .platform
            .cursor_plane_hide_on_crtc(hardware.output_idx)
            .expect("teardown detaches the retained test cursor after failed scene submit");
        let detach = recorder
            .records()
            .into_iter()
            .skip(cleanup_start)
            .find(|record| {
                record.kind == AtomicCommitKind::CursorOnly
                    && record.result == crate::drm::atomic::tests_support::ScriptedResult::Ok
            })
            .expect("teardown hide was recorded");
        cursor_support::retire_cursor_record(
            &mut fixture,
            &mut delivered,
            hardware.output_idx,
            &detach,
            99,
        );
        cursor_support::finish_atomic_fixture(fixture, &mut delivered);
    }
}

#[test]
#[ignore = "requires the lavapipe Vulkan scene fixture"]
// Mutation: restore non-EBUSY cursor-only attribution. ENOMEM/EIO must retain
// the applied binding and lease without a Backend-triggered detach.
fn cursor_only_enomem_and_eio_do_not_detach_the_applied_cursor() {
    use super::cursor_sprites::support as cursor_support;

    let Some(mut fixture) =
        atomic_cursor_fixture("cursor_only_enomem_and_eio_do_not_detach_the_applied_cursor")
    else {
        return;
    };
    let mut delivered = HashSet::new();
    let hardware = establish_test_cursor(&mut fixture, &mut delivered);
    let backend = &fixture.scene.events.backend;
    let binding = cursor_support::applied_cursor_binding(backend, hardware.output_idx)
        .expect("Backend established an applied hardware cursor");
    let device_key = backend.platform.outputs[hardware.output_idx].key.device_key;
    let recorder = Rc::clone(&fixture.scene.events.recorders[&hardware.device_fd]);
    cursor_support::drain_unretired_output_events(
        &mut fixture,
        &mut delivered,
        hardware.output_idx,
    );
    let start = cursor_support::record_count(&fixture, hardware.device_fd);

    for (index, errno) in [
        libc::ENOMEM,
        libc::EIO,
        libc::ENOMEM,
        libc::EIO,
        libc::ENOMEM,
    ]
    .into_iter()
    .enumerate()
    {
        recorder.script_result(Err(io::Error::from_raw_os_error(errno)));
        warp(
            &mut fixture.scene.events.backend,
            &mut fixture.scene.events.state,
            120 + i32::try_from(index).expect("test iteration fits"),
            110,
        );
        maybe_composite(&mut fixture.scene.events.backend);

        let records = recorder.records();
        assert_eq!(
            records.len(),
            start + index + 1,
            "a failed move adds only one atomic submission",
        );
        let failed = &records[start + index];
        assert_eq!(failed.kind, AtomicCommitKind::CursorOnly);
        assert!(matches!(
            failed.result,
            crate::drm::atomic::tests_support::ScriptedResult::Error {
                raw_os_error: Some(found),
                ..
            } if found == errno
        ));
        assert!(
            cursor_support::applied_cursor_binding(
                &fixture.scene.events.backend,
                hardware.output_idx,
            ) == Some(binding)
        );
        assert_eq!(
            cursor_support::sprite_users(
                &fixture.scene.events.backend,
                hardware.output_idx,
                binding.lease,
            ),
            Some(1),
            "the applied sprite lease stays owned",
        );
        assert_eq!(
            fixture
                .scene
                .events
                .backend
                .platform
                .for_tests_cursor_fallback_remaining(device_key, hardware.crtc),
            None,
            "a generic I/O failure does not change cursor qualification",
        );
        assert_eq!(
            fixture
                .scene
                .events
                .backend
                .platform
                .for_tests_cursor_hide_recovery_attempts(device_key, hardware.crtc),
            None,
        );
    }

    let detach =
        cursor_support::accept_cursor_detach(&mut fixture, &mut delivered, hardware.output_idx);
    cursor_support::retire_cursor_record(
        &mut fixture,
        &mut delivered,
        hardware.output_idx,
        &detach,
        99,
    );
    cursor_support::finish_atomic_fixture(fixture, &mut delivered);
}

#[test]
#[ignore = "requires the lavapipe Vulkan scene fixture"]
// Mutation: suppress cursor recovery for EINVAL. Backend must still issue the
// existing detach and transient qualification update for an attributed error.
fn cursor_only_einval_still_detaches_and_releases_the_applied_lease() {
    use super::cursor_sprites::support as cursor_support;

    let Some(mut fixture) =
        atomic_cursor_fixture("cursor_only_einval_still_detaches_and_releases_the_applied_lease")
    else {
        return;
    };
    let mut delivered = HashSet::new();
    let hardware = establish_test_cursor(&mut fixture, &mut delivered);
    let binding =
        cursor_support::applied_cursor_binding(&fixture.scene.events.backend, hardware.output_idx)
            .expect("Backend established an applied hardware cursor");
    let device_key = fixture.scene.events.backend.platform.outputs[hardware.output_idx]
        .key
        .device_key;
    let recorder = Rc::clone(&fixture.scene.events.recorders[&hardware.device_fd]);
    let start = cursor_support::record_count(&fixture, hardware.device_fd);
    recorder.script_result(Err(io::Error::from_raw_os_error(libc::EINVAL)));

    warp(
        &mut fixture.scene.events.backend,
        &mut fixture.scene.events.state,
        120,
        110,
    );
    maybe_composite(&mut fixture.scene.events.backend);

    let records = recorder.records();
    assert_eq!(
        records.len(),
        start + 2,
        "recovery issues a blocking detach"
    );
    assert_eq!(records[start].kind, AtomicCommitKind::CursorOnly);
    assert!(matches!(
        records[start].result,
        crate::drm::atomic::tests_support::ScriptedResult::Error {
            raw_os_error: Some(libc::EINVAL),
            ..
        }
    ));
    assert!(
        cursor_support::applied_cursor_binding(&fixture.scene.events.backend, hardware.output_idx,)
            .is_none()
    );
    assert_eq!(
        cursor_support::sprite_users(
            &fixture.scene.events.backend,
            hardware.output_idx,
            binding.lease,
        ),
        Some(0),
        "successful EINVAL recovery releases the applied sprite user",
    );
    assert_eq!(
        fixture
            .scene
            .events
            .backend
            .platform
            .for_tests_cursor_fallback_remaining(device_key, hardware.crtc),
        Some(1),
        "EINVAL retains the existing transient fallback qualification",
    );
    cursor_support::finish_atomic_fixture(fixture, &mut delivered);
}

#[test]
#[ignore = "requires the lavapipe Vulkan scene fixture"]
// Mutation: return Unbound on generic Atomic show errors despite an applied
// binding. The Backend scene must keep the hardware cursor visible for retry.
fn atomic_show_eio_keeps_the_prior_cursor_visible_through_backend() {
    use super::cursor_sprites::support as cursor_support;
    use yserver_core::backend::Backend;

    let Some(mut fixture) =
        atomic_cursor_fixture("atomic_show_eio_keeps_the_prior_cursor_visible_through_backend")
    else {
        return;
    };
    let mut delivered = HashSet::new();
    let hardware = establish_test_cursor(&mut fixture, &mut delivered);
    let prior =
        cursor_support::applied_cursor_binding(&fixture.scene.events.backend, hardware.output_idx)
            .expect("Backend established the prior hardware cursor");
    let recorder = Rc::clone(&fixture.scene.events.recorders[&hardware.device_fd]);
    let start = cursor_support::record_count(&fixture, hardware.device_fd);
    let replacement = cursor_support::create_argb_cursor(
        &mut fixture.scene.events.backend,
        (4, 4),
        [0x23, 0x45, 0x67, 0xff],
        (1, 2),
    );
    cursor_support::define_root_cursor(&mut fixture.scene.events.backend, replacement);
    cursor_support::damage_root(&mut fixture);
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("submit replacement scene through Backend");
    recorder.script_result(Err(io::Error::from_raw_os_error(libc::EIO)));
    cursor_support::retire_pending_output(
        &mut fixture.scene.events,
        &mut delivered,
        hardware.output_idx,
        99,
    );

    let records = recorder.records();
    assert_eq!(
        records.len(),
        start + 2,
        "failed show has no blocking detach"
    );
    assert_eq!(records[start].kind, AtomicCommitKind::SceneWithCursor);
    assert_eq!(records[start + 1].kind, AtomicCommitKind::CursorOnly);
    assert!(matches!(
        records[start + 1].result,
        crate::drm::atomic::tests_support::ScriptedResult::Error {
            raw_os_error: Some(libc::EIO),
            ..
        }
    ));
    assert_eq!(
        cursor_support::applied_cursor_binding(&fixture.scene.events.backend, hardware.output_idx,),
        Some(prior),
        "the old hardware binding remains applied",
    );
    assert_eq!(
        cursor_support::sprite_users(
            &fixture.scene.events.backend,
            hardware.output_idx,
            prior.lease,
        ),
        Some(1),
        "the old hardware lease remains owned",
    );
    assert_eq!(
        fixture.scene.events.backend.scene.cursor_mode(),
        crate::kms::render::scene::CursorPlaneMode::Mixed,
        "Backend retains visible hardware ownership while scheduling a show retry",
    );

    let detach =
        cursor_support::accept_cursor_detach(&mut fixture, &mut delivered, hardware.output_idx);
    cursor_support::retire_cursor_record(
        &mut fixture,
        &mut delivered,
        hardware.output_idx,
        &detach,
        100,
    );
    cursor_support::finish_atomic_fixture(fixture, &mut delivered);
}

#[test]
#[ignore = "requires the lavapipe Vulkan scene fixture"]
// Mutation: roll back every non-EBUSY Atomic hide error. Backend must leave
// the failed frame generic while retaining the applied binding and lease.
fn atomic_hide_eio_keeps_the_applied_cursor_through_backend() {
    use super::cursor_sprites::support as cursor_support;
    use yserver_core::backend::Backend;

    let Some(mut fixture) =
        atomic_cursor_fixture("atomic_hide_eio_keeps_the_applied_cursor_through_backend")
    else {
        return;
    };
    let mut delivered = HashSet::new();
    let hardware = establish_test_cursor(&mut fixture, &mut delivered);
    let prior =
        cursor_support::applied_cursor_binding(&fixture.scene.events.backend, hardware.output_idx)
            .expect("Backend established the prior hardware cursor");
    let device_key = fixture.scene.events.backend.platform.outputs[hardware.output_idx]
        .key
        .device_key;
    let properties = cursor_support::cursor_properties_for_output(
        &fixture.scene.events.backend,
        hardware.output_idx,
    );
    let recorder = Rc::clone(&fixture.scene.events.recorders[&hardware.device_fd]);
    let start = cursor_support::record_count(&fixture, hardware.device_fd);
    Backend::set_cursor_hidden(&mut fixture.scene.events.backend, true);
    cursor_support::damage_root(&mut fixture);
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("submit hide scene through Backend");
    recorder.script_result(Err(io::Error::from_raw_os_error(libc::EIO)));
    cursor_support::retire_pending_output(
        &mut fixture.scene.events,
        &mut delivered,
        hardware.output_idx,
        99,
    );

    let records = recorder.records();
    assert_eq!(records.len(), start + 2, "failed hide has no extra detach");
    assert_eq!(records[start].kind, AtomicCommitKind::SceneWithCursor);
    assert_eq!(records[start + 1].kind, AtomicCommitKind::CursorOnly);
    assert!(matches!(
        records[start + 1].result,
        crate::drm::atomic::tests_support::ScriptedResult::Error {
            raw_os_error: Some(libc::EIO),
            ..
        }
    ));
    assert_eq!(
        cursor_support::applied_cursor_binding(&fixture.scene.events.backend, hardware.output_idx,),
        Some(prior),
        "generic hide failure retains the applied binding",
    );
    assert_eq!(
        cursor_support::sprite_users(
            &fixture.scene.events.backend,
            hardware.output_idx,
            prior.lease,
        ),
        Some(1),
        "generic hide failure retains the lease user",
    );
    assert_eq!(
        fixture
            .scene
            .events
            .backend
            .platform
            .for_tests_cursor_fallback_remaining(device_key, hardware.crtc),
        None,
        "generic hide failure does not change cursor qualification",
    );
    assert_eq!(
        cursor_support::record_property(&records[start + 1], properties.fb_id),
        Some(0),
        "the failed Backend request was the cursor detach",
    );

    let detach =
        cursor_support::accept_cursor_detach(&mut fixture, &mut delivered, hardware.output_idx);
    cursor_support::retire_cursor_record(
        &mut fixture,
        &mut delivered,
        hardware.output_idx,
        &detach,
        100,
    );
    cursor_support::finish_atomic_fixture(fixture, &mut delivered);
}

#[test]
// Mutation: build the M1 TEST_ONLY request without the cursor fields the live
// direct commit carries. The probe must test exactly what is committed.
fn direct_scanout_probe_tests_the_request_it_commits() {
    let mut backend = KmsBackend::for_tests();
    let recorder = recording_sink(&mut backend, 0);
    let position = seed_cursor_position(&mut backend);
    let platform = &backend.platform;
    let planes = [crate::drm::modeset::DirectScanoutPlaneState {
        output: &platform.outputs[0].output,
        src_x: 0,
        src_y: 0,
        src_w: u32::from(platform.outputs[0].width),
        src_h: u32::from(platform.outputs[0].height),
        in_fence_fd: None,
    }];
    let device = &platform.devices[0].device;
    let sink = &platform.devices[0].atomic_commit_sink;
    crate::drm::modeset::test_direct_scanout(device, sink, raw(91), &planes, &[position])
        .expect("record M1 probe");
    crate::drm::modeset::submit_direct_scanout(
        device,
        sink,
        raw(91),
        &planes,
        &[position],
        false,
        crate::drm::async_flip::FlipTiming::Vsync,
        crate::drm::atomic::TearingCommitContext::NONE,
    )
    .expect("record live direct commit");

    let records = recorder.records();
    assert_eq!(records.len(), 2);
    assert_eq!(
        records[0].flags,
        ::drm::control::AtomicCommitFlags::TEST_ONLY
    );
    assert_eq!(records[0].kind, records[1].kind);
    assert_eq!(records[0].properties, records[1].properties);
    assert_cursor_properties(&records[0], position);
}
