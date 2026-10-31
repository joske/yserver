use super::{
    commit_identity::recording_scene::{
        build_recording_scene_backend, init_tearing_test_logger, raw, set_scene_fence_properties,
        set_unique_direct_plane_properties, skip_unless_vulkan_fed_scanout,
        supports_vulkan_fed_scanout,
    },
    *,
};

use crate::drm::atomic::{
    AtomicCommitKind, AtomicCommitSink, AtomicPropertyUpdate,
    tests_support::RecordingAtomicCommitExecutor,
};

#[test]
// Mutation: make the predicate ignore either required Vulkan export capability.
fn vulkan_fed_scanout_requires_sync_fd_and_drm_format_modifier() {
    assert!(supports_vulkan_fed_scanout(true, true));
    assert!(!supports_vulkan_fed_scanout(false, true));
    assert!(!supports_vulkan_fed_scanout(true, false));
    assert!(!supports_vulkan_fed_scanout(false, false));
}

fn recording_sink(
    backend: &mut KmsBackend,
    device_idx: usize,
) -> std::rc::Rc<RecordingAtomicCommitExecutor> {
    let recorder = std::rc::Rc::new(RecordingAtomicCommitExecutor::default());
    backend.platform.devices[device_idx].atomic_commit_sink =
        std::rc::Rc::new(AtomicCommitSink::new(recorder.clone()));
    recorder
}

fn retire_recorded_output_commits(
    fixture: &mut super::commit_identity::recording_scene::RecordingSceneFixture,
    output_idx: usize,
    record_start: usize,
    frame_start: u32,
) {
    use yserver_core::backend::Backend;

    let device_fd = fixture.kms_fd(output_idx);
    let crtc = fixture.events.backend.platform.outputs[output_idx]
        .output
        .crtc;
    let mut delivered = std::collections::HashSet::new();
    for offset in 0..16_u32 {
        let pending = fixture.events.recorders[&device_fd]
            .records()
            .into_iter()
            .skip(record_start)
            .filter_map(|record| {
                let id = super::commit_identity::support::accepted_event_id(&record)?;
                (record.device_fd == device_fd
                    && record.events.iter().any(|event| event.crtc == crtc)
                    && !delivered.contains(&id))
                .then_some(id)
            })
            .next();
        let Some(id) = pending else {
            return;
        };
        fixture.events.queue_commit_completion(
            device_fd,
            crtc,
            id,
            frame_start.saturating_add(offset),
            std::time::Duration::from_micros(u64::from(frame_start + offset)),
        );
        Backend::on_page_flip_ready(
            &mut fixture.events.backend,
            &mut fixture.events.state,
            device_fd,
        );
        delivered.insert(id);
    }
    panic!("Output({output_idx}) accepted too many commits to retire in the fixture");
}

fn enter_output_owner_through_backend(
    fixture: &mut super::commit_identity::recording_scene::RecordingSceneFixture,
    direct: &super::commit_identity::recording_scene::DirectPresentSetup,
    output_idx: usize,
) -> crate::drm::atomic::tests_support::AtomicCommitRecord {
    use yserver_core::backend::Backend;

    fixture
        .events
        .backend
        .for_tests_set_scanout_m1_prober(super::present_scanout::record_scanout_m1_probe);
    Backend::note_present_scanout_candidate(&mut fixture.events.backend, direct.candidate);
    let device_fd = fixture.kms_fd(output_idx);
    let before = fixture.events.recorders[&device_fd].records().len();
    let probation = usize::from(crate::kms::render::backend::SCANOUT_M2_ELIGIBLE_ROOT_PROBATION);
    for attempt in 0..probation {
        assert_eq!(
            Backend::try_present_direct(
                &mut fixture.events.backend,
                direct.candidate,
                direct.event.clone(),
            )
            .expect("output owner Present through Backend"),
            attempt + 1 == probation,
            "Output({output_idx}) enters on its final probationary Present"
        );
    }
    let crtc = fixture.events.backend.platform.outputs[output_idx]
        .output
        .crtc;
    let direct_record = fixture.events.recorders[&device_fd]
        .records()
        .into_iter()
        .skip(before)
        .find(|record| {
            record.kind != AtomicCommitKind::CursorOnly
                && record.events.len() == 1
                && record.events[0].crtc == crtc
                && super::commit_identity::support::accepted_event_id(record).is_some()
        })
        .expect("Output Present produced one accepted direct commit");
    let tearing_outputs = fixture.events.backend.tearing_scope_outputs();
    if tearing_outputs.contains(&output_idx) && direct_record.kind != AtomicCommitKind::Async {
        retire_recorded_output_commits(fixture, output_idx, before, 30);
        let mut async_candidate = direct.candidate;
        async_candidate.present_id += 10_000;
        let mut async_event = direct.event.clone();
        async_event.present_id = async_candidate.present_id;
        async_event.serial =
            u32::try_from(async_candidate.present_id).expect("follow-up Present serial fits");
        let async_before = fixture.events.recorders[&device_fd].records().len();
        assert!(
            Backend::try_present_direct(&mut fixture.events.backend, async_candidate, async_event,)
                .expect("tearing owner accepts its follow-up direct Present")
        );
        let async_record = fixture.events.recorders[&device_fd]
            .records()
            .into_iter()
            .skip(async_before)
            .find(|record| {
                record.events.len() == 1
                    && record.events[0].crtc == crtc
                    && super::commit_identity::support::accepted_event_id(record).is_some()
            })
            .expect("tearing owner follow-up Present is recorded");
        assert_eq!(async_record.kind, AtomicCommitKind::Async);
        retire_recorded_output_commits(fixture, output_idx, async_before, 31);
        return async_record;
    }
    direct_record
}

fn output_probation_present(
    fixture: &mut super::commit_identity::recording_scene::RecordingSceneFixture,
    delivered: &mut std::collections::HashSet<super::cursor_sprites::support::CompletionKey>,
    direct: &super::commit_identity::recording_scene::DirectPresentSetup,
    output_idx: usize,
    present_id: u64,
    options: u32,
    frame: u32,
) -> bool {
    use yserver_core::backend::Backend;

    let mut candidate = direct.candidate;
    candidate.present_id = present_id;
    candidate.options = options;
    let mut event = direct.event.clone();
    event.present_id = present_id;
    event.serial = u32::try_from(present_id).expect("Present serial fits");
    event.options = options;
    Backend::note_present_scanout_candidate(&mut fixture.events.backend, candidate);
    let accepted = Backend::try_present_direct(&mut fixture.events.backend, candidate, event)
        .expect("probation Present through Backend");
    if accepted {
        super::cursor_sprites::support::retire_pending_output(
            &mut fixture.events,
            delivered,
            output_idx,
            frame,
        );
        let completed = Backend::drain_completed_present_events(&mut fixture.events.backend);
        assert_eq!(
            completed
                .iter()
                .filter(|event| event.present_id == present_id)
                .count(),
            1,
            "accepted probation Present completes exactly once"
        );
    }
    accepted
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: run tearing observation for non-Root candidates.
fn output_candidates_leave_root_tearing_probation_unchanged() {
    use yserver_core::backend::Backend;

    let Some(mut fixture) =
        super::commit_identity::recording_scene::RecordingSceneFixture::with_output_geometry(
            (3840, 1080),
            &[(0, 0, 1920, 1080, 75), (1920, 0, 1920, 1080, 75)],
        )
        .expect("same-refresh two-output recording fixture")
    else {
        eprintln!(
            "SKIP output_candidates_leave_root_tearing_probation_unchanged: Vulkan fixture unavailable"
        );
        return;
    };
    for device in &mut fixture.events.backend.platform.devices {
        device.async_page_flip = true;
    }
    let output = fixture.install_direct_output_present(
        1,
        15_100,
        yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
    );
    let root = fixture.install_direct_present(
        15_101,
        yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
    );
    let root_window = root.candidate.dst_window_xid;
    let probation = u64::from(crate::kms::render::backend::TEARING_ENTRY_PROBATION);

    for index in 0..probation - 1 {
        let mut candidate = root.candidate;
        candidate.present_id += index;
        let mut event = root.event.clone();
        event.present_id = candidate.present_id;
        event.serial = u32::try_from(candidate.present_id).expect("root serial fits");
        Backend::note_present_scanout_candidate(&mut fixture.events.backend, candidate);
        assert!(
            !Backend::try_present_direct(&mut fixture.events.backend, candidate, event)
                .expect("Root async probation Present"),
            "Root remains below direct admission and tearing entry thresholds"
        );
        assert_eq!(
            fixture
                .events
                .backend
                .pstate
                .direct
                .root()
                .tearing_probation
                .count,
            u8::try_from(index + 1).expect("probation count fits"),
            "Root builds its partial tearing streak"
        );

        let mut declined = output.candidate;
        declined.client_id = 2;
        declined.present_id += index;
        declined.valid_region_xid = 0xdead;
        let mut declined_event = output.event.clone();
        declined_event.client_id = yserver_protocol::x11::ClientId(2);
        declined_event.present_id = declined.present_id;
        declined_event.serial = u32::try_from(declined.present_id).expect("output serial fits");
        Backend::note_present_scanout_candidate(&mut fixture.events.backend, declined);
        assert!(
            !Backend::try_present_direct(&mut fixture.events.backend, declined, declined_event,)
                .expect("declined output-sized async Present"),
            "Output(1) Present falls back to Copy"
        );
        assert_eq!(
            fixture
                .events
                .backend
                .pstate
                .direct
                .root()
                .tearing_probation
                .count,
            u8::try_from(index + 1).expect("probation count fits"),
            "Output(1) does not change Root's probation count"
        );
        assert_eq!(
            fixture
                .events
                .backend
                .pstate
                .direct
                .root()
                .tearing_probation
                .window,
            Some(root_window),
            "Output(1) does not replace Root's probation window"
        );
        assert!(
            matches!(
                fixture.events.backend.pstate.tearing.state,
                crate::kms::render::backend::TearingState::Off
            ),
            "an Output candidate cannot enter tearing"
        );
    }

    let mut final_root = root.candidate;
    final_root.present_id += probation - 1;
    let mut final_event = root.event;
    final_event.present_id = final_root.present_id;
    final_event.serial = u32::try_from(final_root.present_id).expect("root serial fits");
    Backend::note_present_scanout_candidate(&mut fixture.events.backend, final_root);
    let _submitted =
        Backend::try_present_direct(&mut fixture.events.backend, final_root, final_event)
            .expect("Root enters at the final probation Present");
    assert!(matches!(
        fixture.events.backend.pstate.tearing.state,
        crate::kms::render::backend::TearingState::On { window, .. }
            if window == root_window
    ));
    assert_eq!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .tearing_probation
            .count,
        u8::try_from(probation).expect("threshold fits"),
        "Root enters exactly at the configured threshold"
    );
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: skip observation for every non-Root scope, incumbent included.
fn incumbent_tearing_window_with_scope_none_ends_tearing() {
    use yserver_core::backend::Backend;

    const WINDOW: u32 = 0x7a01;
    let Some(mut backend) = live_scene_backend() else {
        eprintln!(
            "SKIP incumbent_tearing_window_with_scope_none_ends_tearing: Vulkan fixture unavailable"
        );
        return;
    };
    let recorder = recording_sink(&mut backend, 0);
    let mut state = enter_tearing_without_direct_scanout(&mut backend, WINDOW, 15_200);
    assert!(matches!(
        backend.pstate.tearing.state,
        crate::kms::render::backend::TearingState::On { window: WINDOW, .. }
    ));

    let before_async_scene = recorder.records().len();
    damage_root_and_composite(&mut backend);
    let async_records = recorder.records();
    let async_scene = async_records[before_async_scene..]
        .iter()
        .find(|record| record.kind == AtomicCommitKind::Async)
        .expect("Root tearing submits an async composed frame");
    assert!(
        async_scene
            .flags
            .contains(::drm::control::AtomicCommitFlags::PAGE_FLIP_ASYNC)
    );
    retire_composed_frame(&mut backend, &mut state, 60);

    let (width, height) = backend.platform.fb_dimensions();
    let (mut candidate, mut event, _) = prepare_direct_present_for_ids(
        &mut backend,
        15_300,
        WINDOW,
        WINDOW,
        width / 2,
        height / 2,
        0,
    );
    candidate.completion_dst_host_xid = WINDOW;
    event.dst_host_xid = WINDOW;
    Backend::note_present_scanout_candidate(&mut backend, candidate);
    assert!(
        !Backend::try_present_direct(&mut backend, candidate, event)
            .expect("smaller non-Root-scope Present"),
        "the smaller source is not directly scanned out"
    );
    assert_tearing_off(&backend);

    let before_vsync_scene = recorder.records().len();
    damage_root_and_composite(&mut backend);
    let post_tearing_records = recorder.records();
    let vsync_records = &post_tearing_records[before_vsync_scene..];
    assert!(
        vsync_records.iter().any(|record| matches!(
            record.kind,
            AtomicCommitKind::Scene | AtomicCommitKind::SceneWithCursor
        )),
        "the post-tearing scene frame is submitted"
    );
    assert!(vsync_records.iter().all(|record| {
        !record
            .flags
            .contains(::drm::control::AtomicCommitFlags::PAGE_FLIP_ASYNC)
    }));
}

fn update<H: ::drm::control::ResourceHandle>(
    object: H,
    property: ::drm::control::property::Handle,
    value: u64,
) -> AtomicPropertyUpdate {
    AtomicPropertyUpdate::new(object, property, value)
}

fn prepare_direct_present(
    backend: &mut KmsBackend,
    present_id: u64,
    width: u16,
    height: u16,
) -> (
    yserver_core::backend::PresentScanoutCandidate,
    yserver_core::backend::CompletedPresentEvent,
    crate::drm::async_flip::AsyncShape,
) {
    prepare_direct_present_for_window(
        backend,
        present_id,
        0x7380,
        width,
        height,
        yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
    )
}

fn prepare_direct_present_for_window(
    backend: &mut KmsBackend,
    present_id: u64,
    target_xid: u32,
    width: u16,
    height: u16,
    options: u32,
) -> (
    yserver_core::backend::PresentScanoutCandidate,
    yserver_core::backend::CompletedPresentEvent,
    crate::drm::async_flip::AsyncShape,
) {
    prepare_direct_present_for_ids(
        backend, present_id, target_xid, target_xid, width, height, options,
    )
}

fn prepare_direct_present_for_ids(
    backend: &mut KmsBackend,
    present_id: u64,
    protocol_window_xid: u32,
    host_window_xid: u32,
    width: u16,
    height: u16,
    options: u32,
) -> (
    yserver_core::backend::PresentScanoutCandidate,
    yserver_core::backend::CompletedPresentEvent,
    crate::drm::async_flip::AsyncShape,
) {
    prepare_direct_present_for_ids_with_storage(
        backend,
        present_id,
        protocol_window_xid,
        host_window_xid,
        width,
        height,
        options,
        false,
    )
}

fn prepare_direct_present_for_ids_with_storage(
    backend: &mut KmsBackend,
    present_id: u64,
    protocol_window_xid: u32,
    host_window_xid: u32,
    width: u16,
    height: u16,
    options: u32,
    allocate_live_storage: bool,
) -> (
    yserver_core::backend::PresentScanoutCandidate,
    yserver_core::backend::CompletedPresentEvent,
    crate::drm::async_flip::AsyncShape,
) {
    use crate::kms::render::store::{DrawableKind, Storage};
    use ash::vk;
    use yserver_core::backend::{CompletedPresentEvent, PresentScanoutCandidate, PresentWake};

    let source_xid = 0x8300 + u32::try_from(present_id).expect("test id fits") * 0x10;
    if !backend.windows.contains_key(&host_window_xid) {
        let target_id = super::seed_window(backend, host_window_xid, None, 0, 0);
        if allocate_live_storage {
            backend.store.detach_xid(host_window_xid);
            backend.store_decref_with_invalidate(target_id);
            let storage = backend
                .platform
                .gpu
                .allocate_drawable_storage(width, height, 24)
                .expect("allocate live fallback window storage");
            backend
                .store
                .allocate(host_window_xid, DrawableKind::Window, 24, true, storage)
                .expect("register live fallback window storage");
        }
    }
    if allocate_live_storage
        && let Some(target_id) = backend.store.lookup(host_window_xid)
        && backend
            .store
            .get(target_id)
            .is_some_and(|drawable| drawable.storage.image == vk::Image::null())
    {
        backend.store.detach_xid(host_window_xid);
        backend.store_decref_with_invalidate(target_id);
        let storage = backend
            .platform
            .gpu
            .allocate_drawable_storage(width, height, 24)
            .expect("upgrade test fallback window storage to live Vulkan storage");
        backend
            .store
            .allocate(host_window_xid, DrawableKind::Window, 24, true, storage)
            .expect("register upgraded live fallback window storage");
    }
    let target = backend
        .windows
        .get_mut(&host_window_xid)
        .expect("target window");
    target.width = width;
    target.height = height;
    if (width, height) == (backend.platform.fb_w, backend.platform.fb_h)
        && backend
            .windows
            .get(&host_window_xid)
            .is_some_and(|window| window.parent.is_none())
        && !backend.core.top_level_order.contains(&host_window_xid)
    {
        backend.core.top_level_order.push(host_window_xid);
    }
    let mut source_storage = if allocate_live_storage {
        backend
            .platform
            .gpu
            .allocate_drawable_storage(width, height, 24)
            .expect("allocate live direct source storage")
    } else {
        Storage::for_tests_null(
            vk::Extent2D {
                width: u32::from(width),
                height: u32::from(height),
            },
            vk::Format::B8G8R8A8_UNORM,
        )
    };
    // Direct sources model imported client buffers.
    source_storage.imported_dmabuf = Some(super::imported_dmabuf_for_test(width, height));
    let source_id = backend
        .store
        .allocate(source_xid, DrawableKind::Pixmap, 24, false, source_storage)
        .expect("allocate direct source");
    let mut crtcs: Vec<_> = backend
        .platform
        .outputs
        .iter()
        .map(|layout| layout.output.crtc)
        .collect();
    crtcs.sort_by_key(|crtc| u32::from(*crtc));
    let shape = crate::drm::async_flip::AsyncShape {
        fourcc: 0x3432_5258,
        modifier: 0,
        pitch: u32::from(width) * 4,
        width: u32::from(width),
        height: u32::from(height),
        crtcs,
        direct: true,
    };
    backend.pstate.scanout_m1.entries.insert(
        (source_id, crate::kms::render::backend::DirectScope::Root),
        crate::kms::render::backend::ScanoutM1ProbeEntry::accepted_with_shape(
            crate::drm::modeset::DirectScanoutProbeFramebuffer::for_tests(raw(
                0x8100 + u32::try_from(present_id).expect("test id fits")
            )),
            shape.clone(),
        ),
    );

    let output_layout = &backend.platform.outputs[0];
    let crtc_id = backend
        .pstate
        .crtc_key_by_id
        .iter()
        .find_map(|(&crtc_id, key)| (key == &output_layout.key).then_some(crtc_id))
        .unwrap_or_else(|| u32::from(output_layout.output.crtc));
    let crtc_epoch = backend.present_crtc_clock_epoch(crtc_id);
    let candidate = PresentScanoutCandidate {
        client_id: 1,
        client_pid: None,
        present_id,
        crtc_id,
        crtc_epoch,
        src_pixmap_xid: source_xid,
        dst_window_xid: protocol_window_xid,
        variable_refresh: false,
        src_host_xid: source_xid,
        paint_dst_host_xid: host_window_xid,
        completion_dst_host_xid: host_window_xid,
        src_width: width,
        src_height: height,
        x_off: 0,
        y_off: 0,
        valid_region_xid: 0,
        update_region_xid: 0,
        update_is_full: true,
        explicit_sync: false,
        options,
    };
    let event = CompletedPresentEvent {
        client_id: yserver_protocol::x11::ClientId(1),
        serial: u32::try_from(present_id).expect("test serial fits"),
        host_xid: source_xid,
        dst_host_xid: host_window_xid,
        options,
        present_id,
        window_generation: 0,
        crtc_id,
        crtc_epoch,
        msc_offset: 0,
        completion_clock: None,
        wake: PresentWake::Pixmap { idle_fence_xid: 0 },
        completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
        emit_idle: true,
    };
    (candidate, event, shape)
}

fn retire_recorded_direct_flip(
    backend: &mut KmsBackend,
    state: &mut yserver_core::server::ServerState,
    frame: u32,
) {
    use std::os::fd::{AsFd as _, AsRawFd as _};
    use yserver_core::backend::Backend;

    let source = backend.platform.devices[0]
        .test_page_flip_event_source
        .as_ref()
        .expect("recording page-flip source");
    for (index, output) in backend.platform.outputs.iter().enumerate() {
        source.queue_flip(output.output.crtc, frame + u32::try_from(index).unwrap());
    }
    let drm_fd = backend.platform.devices[0].device.as_fd().as_raw_fd();
    Backend::on_page_flip_ready(backend, state, drm_fd);
}

fn direct_backend(output_count: usize) -> Option<KmsBackend> {
    init_tearing_test_logger();
    let mut backend = KmsBackend::for_tests_with_vk().expect("Vulkan test fixture");
    if !skip_unless_vulkan_fed_scanout(&backend) {
        return None;
    }
    if output_count == 2 {
        backend.platform.fb_w = 1600;
        let mut output_b = clone_test_drm_output(&backend.platform.outputs[0].output);
        output_b.connector = raw(103);
        output_b.encoder = raw(104);
        output_b.crtc = raw(101);
        output_b.plane = raw(102);
        output_b.connector_name = "test-b".to_owned();
        let route = backend.platform.outputs[0].scanout_route;
        let active = crate::kms::backend::ActiveOutput::new(
            route,
            output_b,
            crate::drm::Swapchain::empty_for_tests(),
            800,
            0,
        );
        let key = active.key.clone();
        backend.platform.outputs.push(active);
        backend.platform.outputs[1].x = 800;
        backend.platform.fb_h = 600;
        backend.pstate.crtc_key_by_id.insert(101, key);
        backend.refresh_present_crtc_clock_epochs();
    } else {
        assert_eq!(output_count, 1);
        backend.platform.fb_w = 800;
        backend.platform.fb_h = 600;
    }
    backend
        .pstate
        .direct
        .sync_output_count(backend.platform.outputs.len());
    for layout in &backend.platform.outputs {
        backend
            .pstate
            .crtc_key_by_id
            .insert(u32::from(layout.output.crtc), layout.key.clone());
    }
    backend.refresh_present_crtc_clock_epochs();
    let layouts = backend.platform.scene_output_layouts();
    backend.scene = crate::kms::render::scene::SceneCompositor::new(&backend.platform, &layouts)
        .expect("construct direct test scene");
    backend.scene.scene_structure_dirty = false;
    backend.pstate.direct.root_mut().cursor_bound_all = true;
    backend.pstate.direct.root_mut().eligible_streak =
        crate::kms::render::backend::SCANOUT_M2_ELIGIBLE_ROOT_PROBATION - 1;
    backend.platform.requested_flip_timing = crate::drm::async_flip::FlipTiming::Async;
    for device in &mut backend.platform.devices {
        device.async_page_flip = true;
    }
    Some(backend)
}

fn set_direct_second_crtc(backend: &mut KmsBackend, crtc_id: u32) {
    let previous = u32::from(backend.platform.outputs[1].output.crtc);
    let output_key = backend.platform.outputs[1].key.clone();
    backend.platform.outputs[1].output.crtc = raw(crtc_id);
    backend.pstate.crtc_key_by_id.remove(&previous);
    backend.pstate.crtc_key_by_id.insert(crtc_id, output_key);
    backend.refresh_present_crtc_clock_epochs();
}

fn try_direct_present(
    backend: &mut KmsBackend,
    present_id: u64,
    width: u16,
    height: u16,
) -> crate::drm::async_flip::AsyncShape {
    try_direct_present_for_window(
        backend,
        present_id,
        0x7380,
        width,
        height,
        yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
    )
    .expect("eligible direct Present must submit")
}

fn qualify_test_direct_async(
    backend: &mut KmsBackend,
    recorder: &std::rc::Rc<RecordingAtomicCommitExecutor>,
    window: u32,
    first_present: u64,
) {
    use yserver_core::backend::Backend;

    let mut state = enter_tearing_without_direct_scanout(backend, window, first_present);
    Backend::set_cursor_hidden(backend, true);
    damage_root_and_composite(backend);
    assert!(
        backend.scene.has_pending_page_flips(),
        "expected a pending scene page flip: {}",
        tearing_composite_diagnostics(backend, None),
    );
    retire_composed_frame(backend, &mut state, 20);
    assert!(matches!(
        backend.pstate.tearing.state,
        crate::kms::render::backend::TearingState::On { window: owner, .. } if owner == window
    ));
    assert_eq!(
        backend.scene.cursor_mode(),
        crate::kms::render::scene::CursorPlaneMode::Hidden
    );
    let (width, height) = backend.platform.fb_dimensions();
    let before_warmup = recorder.records().len();
    for index in 0..7_u64 {
        assert!(
            try_direct_present_for_window(
                backend,
                first_present + 8 + index,
                window,
                width,
                height,
                yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
            )
            .is_none()
        );
    }
    assert_eq!(recorder.records().len(), before_warmup);
    assert_eq!(backend.pstate.direct.root().eligible_streak, 7);
    assert!(backend.pstate.direct.root().current.is_none());
    assert!(backend.pstate.direct.root().pending.is_none());
    assert!(backend.pstate.direct.root().queued_successor.is_none());
    recorder.clear_records();
}

fn try_direct_present_for_window(
    backend: &mut KmsBackend,
    present_id: u64,
    target_xid: u32,
    width: u16,
    height: u16,
    options: u32,
) -> Option<crate::drm::async_flip::AsyncShape> {
    try_direct_present_for_ids(
        backend, present_id, target_xid, target_xid, target_xid, width, height, options,
    )
}

fn try_direct_present_for_ids(
    backend: &mut KmsBackend,
    present_id: u64,
    protocol_window_xid: u32,
    host_window_xid: u32,
    completion_dst_host_xid: u32,
    width: u16,
    height: u16,
    options: u32,
) -> Option<crate::drm::async_flip::AsyncShape> {
    use yserver_core::backend::Backend;

    let (mut candidate, mut event, shape) = prepare_direct_present_for_ids(
        backend,
        present_id,
        protocol_window_xid,
        host_window_xid,
        width,
        height,
        options,
    );
    candidate.completion_dst_host_xid = completion_dst_host_xid;
    event.dst_host_xid = completion_dst_host_xid;
    let submitted = Backend::try_present_direct(backend, candidate, event)
        .unwrap_or_else(|error| panic!("direct Present entry {present_id}: {error}"));
    submitted.then_some(shape)
}

fn try_direct_present_with_live_storage(
    backend: &mut KmsBackend,
    present_id: u64,
    target_xid: u32,
    width: u16,
    height: u16,
    options: u32,
) -> Option<crate::drm::async_flip::AsyncShape> {
    use yserver_core::backend::Backend;

    let (candidate, event, shape) = prepare_direct_present_for_ids_with_storage(
        backend, present_id, target_xid, target_xid, width, height, options, true,
    );
    let submitted = Backend::try_present_direct(backend, candidate, event)
        .unwrap_or_else(|error| panic!("live-storage direct Present {present_id}: {error}"));
    submitted.then_some(shape)
}

fn try_live_direct_for_ids(
    backend: &mut KmsBackend,
    present_id: u64,
    protocol_window_xid: u32,
    host_window_xid: u32,
    options: u32,
) -> Option<crate::drm::async_flip::AsyncShape> {
    use yserver_core::backend::Backend;

    let (candidate, event, shape) = prepare_direct_present_for_ids_with_storage(
        backend,
        present_id,
        protocol_window_xid,
        host_window_xid,
        800,
        600,
        options,
        true,
    );
    let submitted = Backend::try_present_direct(backend, candidate, event)
        .unwrap_or_else(|error| panic!("live direct Present {present_id}: {error}"));
    submitted.then_some(shape)
}

fn retire_direct_present(
    backend: &mut KmsBackend,
    state: &mut yserver_core::server::ServerState,
    frame: u32,
) {
    use yserver_core::backend::Backend;

    retire_recorded_direct_flip(backend, state, frame);
    Backend::maybe_composite(backend).expect("settle post-retirement scene work");
}

fn qualify_direct_async(
    backend: &mut KmsBackend,
    recorder: &std::rc::Rc<RecordingAtomicCommitExecutor>,
    state: &mut yserver_core::server::ServerState,
    window: u32,
    start_present: u64,
    count: u32,
) {
    use yserver_core::backend::Backend;

    use crate::drm::async_flip::FlipTiming;

    assert_eq!(backend.platform.requested_flip_timing, FlipTiming::Async);
    for index in 0..count {
        let present_id = start_present + u64::from(index);
        let before = recorder.records().len();
        let (width, height) = backend.platform.fb_dimensions();
        let (candidate, event, _) = prepare_direct_present_for_window(
            backend,
            present_id,
            window,
            width,
            height,
            yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
        );
        let leaf_id = backend.store.lookup(candidate.paint_dst_host_xid);
        let paint_target = backend.resolve_paint_target(candidate.paint_dst_host_xid);
        let paint_id = paint_target.map(|target| target.backing_id());
        let m0_target = backend.scanout_m0_target(candidate.paint_dst_host_xid, leaf_id, paint_id);
        let root = (
            u32::from(backend.platform.fb_w),
            u32::from(backend.platform.fb_h),
        );
        let window_rect = leaf_id.and_then(|id| backend.window_absolute_rect(id));
        let root_coverage = window_rect.is_some_and(|rect| {
            rect.offset.x == 0
                && rect.offset.y == 0
                && (rect.extent.width, rect.extent.height) == root
                && (
                    u32::from(candidate.src_width),
                    u32::from(candidate.src_height),
                ) == root
        });
        let authoritative =
            crate::kms::render::backend::scanout_m2_is_authoritative_root(m0_target, root_coverage);
        let direct_gate = backend.direct_present_crtc_eligible(
            crate::kms::render::backend::DirectScope::Root,
            candidate.crtc_id,
            candidate.crtc_epoch,
        ) && crate::kms::render::backend::scanout_direct_eligible(
            backend.scanout_allowed(),
            backend.kms_outputs_active,
            true,
            backend.scene.root_overlay.is_empty(),
            authoritative
                && backend.direct_shape_chain_covers_root(
                    candidate.paint_dst_host_xid,
                    backend.direct_scope_rect(crate::kms::render::backend::DirectScope::Root),
                ),
            paint_target.is_none_or(|target| !target.has_border_clip()),
            candidate.x_off,
            candidate.y_off,
            candidate.valid_region_xid,
        );
        let diagnostic = format!(
            "m0={m0_target:?} root={root:?} window_rect={window_rect:?} source={}x{} root_coverage={root_coverage} authoritative={authoritative} direct_gate={direct_gate} async_devices={} topology={} crtc_eligible={}",
            candidate.src_width,
            candidate.src_height,
            backend.platform.outputs.iter().all(|output| backend
                .platform
                .device_for_key(output.key.device_key)
                .is_some_and(|device| device.async_page_flip)),
            backend
                .direct_scanout_topology_eligible(crate::kms::render::backend::DirectScope::Root),
            backend.direct_present_crtc_eligible(
                crate::kms::render::backend::DirectScope::Root,
                candidate.crtc_id,
                candidate.crtc_epoch,
            ),
        );
        let submitted = Backend::try_present_direct(backend, candidate, event)
            .unwrap_or_else(|error| panic!("direct Present entry {present_id}: {error}"));
        assert!(
            submitted,
            "qualifying root Present {present_id} must submit ({diagnostic}; tearing={:?}, probation={}, owner={:?}, m2_streak={}, scene_cursor={:?}, scene_pending={})",
            backend.pstate.tearing.state,
            backend.pstate.direct.root().tearing_probation.count,
            backend.pstate.direct.root().tearing_probation.window,
            backend.pstate.direct.root().eligible_streak,
            backend.scene.cursor_mode(),
            backend.scene.has_pending_page_flips(),
        );
        retire_direct_present(backend, state, index + 1);
        assert_eq!(recorder.records().len(), before + 1);
        assert_eq!(
            recorder.records()[before].kind,
            AtomicCommitKind::Scene,
            "the probation frame must remain Vsync"
        );
    }
}

pub(super) fn enter_tearing_without_direct_scanout(
    backend: &mut KmsBackend,
    window: u32,
    first_present: u64,
) -> yserver_core::server::ServerState {
    enter_tearing_without_direct_scanout_for_ids(backend, window, window, window, first_present)
}

fn enter_tearing_without_direct_scanout_for_ids(
    backend: &mut KmsBackend,
    protocol_window_xid: u32,
    host_window_xid: u32,
    completion_dst_host_xid: u32,
    first_present: u64,
) -> yserver_core::server::ServerState {
    let mut state = ServerState::new();
    enter_tearing_without_direct_scanout_for_state_ids(
        backend,
        &mut state,
        protocol_window_xid,
        host_window_xid,
        completion_dst_host_xid,
        first_present,
    );
    state
}

fn enter_tearing_without_direct_scanout_for_state_ids(
    backend: &mut KmsBackend,
    state: &mut yserver_core::server::ServerState,
    protocol_window_xid: u32,
    host_window_xid: u32,
    completion_dst_host_xid: u32,
    first_present: u64,
) {
    submit_initial_tearing_frame(backend);
    retire_composed_frame(backend, state, 1);
    finish_entering_tearing(
        backend,
        protocol_window_xid,
        host_window_xid,
        completion_dst_host_xid,
        first_present,
    );
}

/// First half of tearing entry: select the visible root cursor and submit the
/// initial scene frame. The caller retires that frame.
pub(super) fn submit_initial_tearing_frame(backend: &mut KmsBackend) {
    use yserver_core::backend::Backend;

    let cursor_xid = 0x7f10;
    backend.insert_cursor_record(cursor_xid, 1, 1, 0, 0, vec![0xff; 4]);
    let root = backend.core.window_id;
    Backend::define_cursor(backend, None, root, cursor_xid)
        .expect("select the fixture's visible root cursor");
    Backend::set_cursor_hidden(backend, true);
    Backend::set_cursor_hidden(backend, false);
    damage_root(backend);
    let composite_result = Backend::maybe_composite(backend);
    assert!(
        composite_result.is_ok() && backend.scene.has_pending_page_flips(),
        "initial tearing scene compose did not submit a page flip: maybe_composite={composite_result:?}; {}",
        tearing_composite_diagnostics(
            backend,
            backend.platform.devices[0]
                .atomic_commit_sink
                .for_tests_record_count(),
        ),
    );
}

/// Second half of tearing entry, after the initial frame retired.
pub(super) fn finish_entering_tearing(
    backend: &mut KmsBackend,
    protocol_window_xid: u32,
    host_window_xid: u32,
    completion_dst_host_xid: u32,
    first_present: u64,
) {
    assert_eq!(
        backend.scene.cursor_mode(),
        crate::kms::render::scene::CursorPlaneMode::Sw,
        "the fixture has a visible software cursor because it has no HW plane"
    );
    for device in &mut backend.platform.devices {
        device.async_page_flip = true;
    }
    let (width, height) = backend.platform.fb_dimensions();

    for index in 0..u64::from(crate::kms::render::backend::TEARING_ENTRY_PROBATION) {
        assert!(
            try_direct_present_for_ids(
                backend,
                first_present + index,
                protocol_window_xid,
                host_window_xid,
                completion_dst_host_xid,
                width,
                height,
                yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
            )
            .is_none(),
            "the real direct gate rejects the SW cursor"
        );
    }
    assert!(
        matches!(
            backend.pstate.tearing.state,
            crate::kms::render::backend::TearingState::On { window: owner, .. }
                if owner == protocol_window_xid
        ),
        "probation={} owner={:?} cursor={:?} root_order={:?}",
        backend.pstate.direct.root().tearing_probation.count,
        backend.pstate.direct.root().tearing_probation.window,
        backend.scene.cursor_mode(),
        backend.core.top_level_order
    );
}

fn tearing_composite_diagnostics(backend: &KmsBackend, recorder_count: Option<usize>) -> String {
    let now = backend.monotonic_now();
    let deadline = backend.for_tests_tearing_cursor_compose_deadline_at(now);
    let scene_wants_scene = backend.for_tests_scene_wants_scene();
    let has_output_ready_for_submit = backend.scene.has_output_ready_for_submit(&backend.platform);
    let submit_retry_state: Vec<_> = backend
        .scene
        .submit_retry_state_for_tests()
        .into_iter()
        .map(|(output_idx, retry_at, failed_submit_bo_count)| {
            let next_submit_retry_at = retry_at.map(|retry_at| {
                retry_at.checked_duration_since(now).map_or_else(
                    || format!("overdue by {:?}", now.duration_since(retry_at)),
                    |remaining| format!("in {remaining:?}"),
                )
            });
            format!(
                "output[{output_idx}]: next_submit_retry_at={next_submit_retry_at:?}, \
                 failed_submit_bo_count={failed_submit_bo_count}"
            )
        })
        .collect();
    let vk = backend.platform.gpu.vk.as_deref();
    let supports_sync_fd = vk.is_some_and(|vk| vk.supports_sync_fd());
    let sync_fd_export_probe =
        vk.map_or_else(|| "NoVkContext".to_owned(), signalled_sync_fd_export_probe);
    let output_readiness: Vec<_> = (0..backend.platform.outputs.len())
        .map(|output_idx| {
            format!(
                "output[{output_idx}]: scene_wants_scene={scene_wants_scene}, output_ready_for_submit={}",
                backend.scene.output_ready_for_submit(output_idx, &backend.platform),
            )
        })
        .collect();
    let scanout_pools: Vec<_> = (0..backend.platform.outputs.len())
        .map(|output_idx| {
            let phases = backend
                .platform
                .scanout_pools
                .get(output_idx)
                .and_then(Option::as_ref)
                .map(|pool| {
                    pool.display_pool()
                        .bos
                        .iter()
                        .map(|bo| bo.state.phase)
                        .collect::<Vec<_>>()
                });
            (output_idx, phases.is_some(), phases)
        })
        .collect();

    format!(
        "cursor_mode={:?}, scene_wants_scene={scene_wants_scene}, scene_wants_compose={}, has_output_ready_for_submit={has_output_ready_for_submit}, output_readiness={output_readiness:?}, submit_retry_state={submit_retry_state:?}, renderer_failed={}, supports_sync_fd={supports_sync_fd}, sync_fd_export_probe={sync_fd_export_probe}, cursor_compose_deadline={deadline:?} at monotonic_now={now:?} (last_cursor_compose={:?}, interval={:?}), scanout_pools={scanout_pools:?}, recorder_record_count={recorder_count:?}",
        backend.scene.cursor_mode(),
        backend.for_tests_scene_wants_compose(),
        backend.platform_renderer_failed_for_tests(),
        backend.pstate.tearing.last_cursor_compose,
        backend
            .pstate
            .tearing
            .cursor_compose_interval(&backend.platform, &backend.tearing_scope_outputs()),
    )
}

fn signalled_sync_fd_export_probe(vk: &crate::kms::vk::device::VkContext) -> String {
    use std::os::fd::{FromRawFd as _, OwnedFd};

    let ext = match vk.semaphore_fd_ext() {
        Ok(ext) => ext,
        Err(error) => return format!("Err({error:?})"),
    };
    let mut export_info = ash::vk::ExportSemaphoreCreateInfo::default()
        .handle_types(ash::vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
    let create_info = ash::vk::SemaphoreCreateInfo::default().push_next(&mut export_info);
    // SAFETY: the device is held by the fixture's live VkContext.
    let semaphore = match unsafe { vk.device.create_semaphore(&create_info, None) } {
        Ok(semaphore) => semaphore,
        Err(error) => return format!("Err(create_semaphore: {error:?})"),
    };

    let signal_info = [ash::vk::SemaphoreSubmitInfo::default()
        .semaphore(semaphore)
        .stage_mask(ash::vk::PipelineStageFlags2::ALL_COMMANDS)];
    let submit_info = ash::vk::SubmitInfo2::default().signal_semaphore_infos(&signal_info);
    // SAFETY: the semaphore belongs to this device and is signalled once on
    // its graphics queue, with no command buffers or waits.
    if let Err(error) = unsafe {
        vk.device
            .queue_submit2(vk.graphics_queue, &[submit_info], ash::vk::Fence::null())
    } {
        // SAFETY: queue_submit2 failed, so no operation references the semaphore.
        unsafe { vk.device.destroy_semaphore(semaphore, None) };
        return format!("Err(queue_submit2: {error:?})");
    }
    // SAFETY: waiting the submitting queue idle retires the signal operation.
    if let Err(error) = unsafe { vk.device.queue_wait_idle(vk.graphics_queue) } {
        // SAFETY: queue_wait_idle failed, so the probe cannot safely export it.
        unsafe { vk.device.destroy_semaphore(semaphore, None) };
        return format!("Err(queue_wait_idle: {error:?})");
    }

    let export_info = ash::vk::SemaphoreGetFdInfoKHR::default()
        .semaphore(semaphore)
        .handle_type(ash::vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
    // SAFETY: extension availability was checked above and the semaphore was
    // created with SYNC_FD export capability and has a signalled payload.
    let export_result = unsafe { ext.get_semaphore_fd(&export_info) };
    // SAFETY: the queue is idle and export has returned, so no operation uses it.
    unsafe { vk.device.destroy_semaphore(semaphore, None) };

    match export_result {
        Ok(-1) => "Ok(-1)".to_owned(),
        Ok(fd) if fd >= 0 => {
            // SAFETY: successful Vulkan fd export transfers this descriptor to us.
            drop(unsafe { OwnedFd::from_raw_fd(fd) });
            "Ok(fd>=0)".to_owned()
        }
        Ok(fd) => format!("Ok({fd})"),
        Err(error) => format!("Err({error:?})"),
    }
}

fn assert_tearing_off(backend: &KmsBackend) {
    assert!(matches!(
        backend.pstate.tearing.state,
        crate::kms::render::backend::TearingState::Off
    ));
    assert_eq!(backend.pstate.direct.root().tearing_probation.count, 0);
    assert_eq!(backend.pstate.direct.root().tearing_probation.window, None);
    assert_eq!(backend.pstate.tearing.last_cursor_compose, None);
    assert_eq!(backend.pstate.tearing.deferred_cursor_wake, None);
    assert!(backend.platform.async_ledger.for_tests_is_empty());
}

fn assert_direct_queue_drained(backend: &KmsBackend) {
    assert!(backend.pstate.direct.root().pending.is_none());
    assert!(backend.pstate.direct.root().queued_successor.is_none());
    assert!(
        backend
            .pstate
            .direct
            .root()
            .deferred_successor_skips
            .is_empty()
    );
}

fn assert_commit_sequence(
    recorder: &std::rc::Rc<RecordingAtomicCommitExecutor>,
    expected: &[AtomicCommitKind],
) {
    let actual: Vec<_> = recorder
        .records()
        .iter()
        .map(|record| record.kind)
        .collect();
    assert_eq!(actual, expected);
}

fn live_tearing_cursor_backend(
    window: u32,
    first_present: u64,
) -> Option<(
    KmsBackend,
    std::rc::Rc<RecordingAtomicCommitExecutor>,
    yserver_core::server::ServerState,
)> {
    let mut backend = live_scene_backend()?;
    let recorder = recording_sink(&mut backend, 0);
    let state = enter_tearing_without_direct_scanout(&mut backend, window, first_present);
    recorder.clear_records();
    assert!(backend.pstate.tearing.is_on());
    assert_eq!(
        backend.scene.cursor_mode(),
        crate::kms::render::scene::CursorPlaneMode::Sw,
        "the fixture has a visible software cursor"
    );
    Some((backend, recorder, state))
}

fn seed_pending_tearing_cursor_wake(backend: &mut KmsBackend) {
    let interval = backend
        .pstate
        .tearing
        .cursor_compose_interval(&backend.platform, &backend.tearing_scope_outputs());
    let now = std::time::Instant::now();
    backend.pstate.tearing.last_cursor_compose = Some(now);
    backend.pstate.tearing.deferred_cursor_wake = Some(now + interval);
}
fn retire_registered_cursor_in_dead_zone(
    backend: &mut KmsBackend,
    state: &mut yserver_core::server::ServerState,
) {
    use yserver_core::backend::Backend;

    let source = Backend::create_pixmap(backend, None, 1, 1, 1)
        .expect("create visible cursor source pixmap");
    Backend::put_image(backend, None, source.as_raw(), 1, 1, 1, 0, 0, &[1, 0, 0, 0])
        .expect("paint visible cursor source bit");
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
        .expect("register cursor on root");
    assert!(backend.cursor_records.contains_key(&cursor.as_raw()));
    assert_eq!(backend.effective_cursor_xid, Some(cursor.as_raw()));
    assert!(!backend.cursor_hidden, "the registered cursor is visible");

    // Output 0 ends at (800, 600); output 1 starts at (900, 0) and
    // ends at (1700, 400). This point lies in the real gap between them.
    Backend::warp_pointer_root(backend, state, 850, 450);
    assert_eq!(
        (backend.core.cursor_x, backend.core.cursor_y),
        (850.0, 450.0)
    );
    damage_root_and_composite(backend);
    assert!(
        backend.scene.has_pending_page_flips(),
        "expected a pending scene page flip: {}",
        tearing_composite_diagnostics(backend, None),
    );
    retire_composed_frame(backend, state, 1);
    assert_eq!(
        backend.scene.cursor_mode(),
        crate::kms::render::scene::CursorPlaneMode::Hidden,
        "the retired scene classifies the registered visible cursor as off-output"
    );
    assert_eq!(backend.effective_cursor_xid, Some(cursor.as_raw()));
    assert!(
        !backend.cursor_hidden,
        "Hidden came from the dead zone, not XFIXES"
    );
}

pub(super) fn materialize_test_cow_stage(
    backend: &mut KmsBackend,
    state: &mut yserver_core::server::ServerState,
    client_id: yserver_protocol::x11::ClientId,
    window_resource: yserver_protocol::x11::ResourceId,
) -> u32 {
    use yserver_core::{
        backend::{Backend, WindowHandle},
        host_x11::HostSubwindowVisual,
        resources::{COMPOSITE_OVERLAY_WINDOW, ROOT_VISUAL, ROOT_WINDOW},
    };
    use yserver_protocol::x11::CreateWindowRequest;

    state
        .resources
        .window_mut(ROOT_WINDOW)
        .expect("core root")
        .host_xid = WindowHandle::from_raw(backend.window_id());
    assert!(Backend::get_overlay_window(backend, None).expect("materialize backend COW"));
    let cow_host = Backend::cow_host_xid(backend).expect("materialized COW host xid");
    let cow_handle = WindowHandle::from_raw(cow_host).expect("COW host handle");
    state.resources.materialize_cow_resource(cow_handle);

    let width = backend.platform.fb_w;
    let height = backend.platform.fb_h;
    let stage_handle = Backend::create_subwindow(
        backend,
        None,
        cow_handle,
        0,
        0,
        width,
        height,
        0,
        HostSubwindowVisual::DepthOnly { depth: 24 },
        None,
        None,
    )
    .expect("create full-root COW stage");
    state.resources.create_window(
        client_id,
        CreateWindowRequest {
            depth: 24,
            window: window_resource,
            parent: COMPOSITE_OVERLAY_WINDOW,
            x: 0,
            y: 0,
            width,
            height,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default()
        },
    );
    state
        .resources
        .window_mut(window_resource)
        .expect("COW stage resource")
        .host_xid = Some(stage_handle);
    let _ = state.resources.map_window(window_resource);
    Backend::realize_window_storage(
        backend,
        None,
        stage_handle.as_raw(),
        yserver_core::backend::RealizeBackground::Paint,
    )
    .expect("realize COW stage storage");
    Backend::map_subwindow(backend, None, stage_handle.as_raw()).expect("map COW stage");
    Backend::register_subwindow(backend, None, window_resource, stage_handle.as_raw())
        .expect("register COW stage");
    Backend::sync_top_level_order(backend, state);
    stage_handle.as_raw()
}
fn create_bitmap_cursor_via_backend(
    backend: &mut KmsBackend,
    source_bit: bool,
    mask_bit: bool,
) -> yserver_core::backend::CursorHandle {
    use yserver_core::backend::Backend;

    let source = Backend::create_pixmap(backend, None, 1, 1, 1).expect("cursor source pixmap");
    let mask = Backend::create_pixmap(backend, None, 1, 1, 1).expect("cursor mask pixmap");
    let source_bytes = if source_bit { [1, 0, 0, 0] } else { [0; 4] };
    let mask_bytes = if mask_bit { [1, 0, 0, 0] } else { [0; 4] };
    Backend::put_image(backend, None, source.as_raw(), 1, 1, 1, 0, 0, &source_bytes)
        .expect("paint cursor source bit");
    Backend::put_image(backend, None, mask.as_raw(), 1, 1, 1, 0, 0, &mask_bytes)
        .expect("paint cursor mask bit");
    Backend::create_cursor(
        backend,
        None,
        source,
        Some(mask),
        (u16::MAX, u16::MAX, u16::MAX),
        (0, 0, 0),
        0,
        0,
    )
    .expect("create bitmap cursor")
}

fn create_render_cursor_via_backend(
    backend: &mut KmsBackend,
) -> yserver_core::backend::CursorHandle {
    use yserver_core::backend::{AnyHandle, Backend};

    let pixmap = Backend::create_pixmap(backend, None, 32, 1, 1).expect("RENDER cursor pixmap");
    Backend::put_image(
        backend,
        None,
        pixmap.as_raw(),
        32,
        1,
        1,
        0,
        0,
        &[0, 0, 0, 0],
    )
    .expect("paint transparent RENDER pixel");
    let picture =
        Backend::render_create_picture(backend, None, AnyHandle::Pixmap(pixmap), 0, 0, &[])
            .expect("create RENDER picture")
            .expect("RENDER picture handle");
    Backend::render_create_cursor(backend, None, picture, 0, 0)
        .expect("create RENDER cursor")
        .expect("RENDER cursor handle")
}

fn present_until_direct_async(
    backend: &mut KmsBackend,
    recorder: &std::rc::Rc<RecordingAtomicCommitExecutor>,
    state: &mut yserver_core::server::ServerState,
    window: u32,
    first_present: u64,
) {
    let (width, height) = backend.platform.fb_dimensions();
    for device in &mut backend.platform.devices {
        device.async_page_flip = true;
    }
    let attempts = crate::kms::render::backend::TEARING_ENTRY_PROBATION
        + crate::kms::render::backend::SCANOUT_M2_ELIGIBLE_ROOT_PROBATION
        + 4;
    for offset in 0..u64::from(attempts) {
        let before = recorder.records().len();
        let Some(_) = try_direct_present_with_live_storage(
            backend,
            first_present + offset,
            window,
            width,
            height,
            yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
        ) else {
            assert_eq!(recorder.records().len(), before);
            continue;
        };
        let record = recorder
            .records()
            .get(before)
            .cloned()
            .expect("accepted direct Present records one commit");
        assert!(!backend.platform.any_cursor_plane_visible());
        retire_direct_present(backend, state, u32::try_from(offset + 60).unwrap());
        if backend.pstate.tearing.is_on() && record.kind == AtomicCommitKind::Async {
            assert!(backend.pstate.direct.root().active());
            return;
        }
        assert_eq!(record.kind, AtomicCommitKind::Scene);
    }
    panic!(
        "transparent cursor did not reach direct async; tearing={:?}, m2_streak={}, commits={:?}",
        backend.pstate.tearing.state,
        backend.pstate.direct.root().eligible_streak,
        recorder
            .records()
            .iter()
            .map(|r| r.kind)
            .collect::<Vec<_>>(),
    );
}
fn qualify_from_dead_zone(
    backend: &mut KmsBackend,
    recorder: &std::rc::Rc<RecordingAtomicCommitExecutor>,
    state: &mut yserver_core::server::ServerState,
    window: u32,
    first_present: u64,
) {
    let (width, height) = backend.platform.fb_dimensions();
    for index in 0..u64::from(crate::kms::render::backend::TEARING_ENTRY_PROBATION) {
        let before = recorder.records().len();
        let submitted =
            if index + 1 == u64::from(crate::kms::render::backend::TEARING_ENTRY_PROBATION) {
                try_direct_present_with_live_storage(
                    backend,
                    first_present + index,
                    window,
                    width,
                    height,
                    yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
                )
                .is_some()
            } else {
                try_direct_present_for_window(
                    backend,
                    first_present + index,
                    window,
                    width,
                    height,
                    yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
                )
                .is_some()
            };
        let output_count = backend.platform.outputs.len();
        if submitted {
            assert_eq!(index, 7, "M2 admission submits the eighth root Present");
            assert_eq!(recorder.records().len(), before + 1);
            assert_eq!(recorder.records()[before].kind, AtomicCommitKind::Scene);
            retire_direct_present(backend, state, index as u32 + 2);
        } else {
            assert!(
                index < 7,
                "the eighth eligible root Present reaches M2 admission"
            );
            damage_root_and_composite(backend);
            assert_eq!(recorder.records().len(), before + output_count);
            assert!(
                recorder.records()[before..]
                    .iter()
                    .all(|record| record.kind == AtomicCommitKind::Scene)
            );
            retire_composed_frame(backend, state, index as u32 + 2);
        }
    }
    assert!(matches!(
        backend.pstate.tearing.state,
        crate::kms::render::backend::TearingState::On { window: owner, .. } if owner == window
    ));
    let expected = vec![AtomicCommitKind::Scene; backend.platform.outputs.len() * 8 + 1];
    assert_commit_sequence(recorder, &expected);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: drop the Async option, fullscreen gate, or CRTC async capability check.
fn entry_needs_async_option_gate_and_capability() {
    use yserver_core::backend::Backend;

    const WINDOW: u32 = 0x7400;
    let Some(mut backend) = direct_backend(1) else {
        return;
    };
    let recorder = recording_sink(&mut backend, 0);
    let mut state = ServerState::new();
    Backend::set_cursor_hidden(&mut backend, true);
    qualify_direct_async(&mut backend, &recorder, &mut state, WINDOW, 100, 8);
    assert!(matches!(
        backend.pstate.tearing.state,
        crate::kms::render::backend::TearingState::On { window: WINDOW, .. }
    ));

    let before = recorder.records().len();
    let _shape = try_direct_present_for_window(
        &mut backend,
        108,
        WINDOW,
        800,
        600,
        yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
    )
    .expect("ninth direct Present");
    assert_eq!(
        recorder.records()[before].kind,
        AtomicCommitKind::Async,
        "Async begins on the first direct commit after the eight-frame entry probation"
    );
    retire_direct_present(&mut backend, &mut state, 9);

    let before = recorder.records().len();
    assert!(try_direct_present_for_window(&mut backend, 109, WINDOW, 800, 600, 0).is_some());
    assert_eq!(recorder.records()[before].kind, AtomicCommitKind::Scene);
    assert_tearing_off(&backend);
    retire_direct_present(&mut backend, &mut state, 10);
    assert!(backend.platform.async_ledger.for_tests_is_empty());
    let mut expected = vec![AtomicCommitKind::Scene; 8];
    expected.extend([AtomicCommitKind::Async, AtomicCommitKind::Scene]);
    assert_commit_sequence(&recorder, &expected);
    assert_direct_queue_drained(&backend);

    let Some(mut no_option) = direct_backend(1) else {
        return;
    };
    let no_option_recorder = recording_sink(&mut no_option, 0);
    let mut no_option_state = ServerState::new();
    for index in 0..9 {
        assert!(
            try_direct_present_for_window(&mut no_option, 200 + index, WINDOW + 1, 800, 600, 0,)
                .is_some()
        );
        retire_direct_present(&mut no_option, &mut no_option_state, index as u32 + 1);
    }
    assert_tearing_off(&no_option);
    assert_commit_sequence(&no_option_recorder, &[AtomicCommitKind::Scene; 9]);
    assert_direct_queue_drained(&no_option);

    let Some(mut not_fullscreen) = direct_backend(1) else {
        return;
    };
    let not_fullscreen_recorder = recording_sink(&mut not_fullscreen, 0);
    for index in 0..9 {
        assert!(
            try_direct_present_for_window(
                &mut not_fullscreen,
                300 + index,
                WINDOW + 2,
                400,
                300,
                yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
            )
            .is_none()
        );
        Backend::maybe_composite(&mut not_fullscreen).expect("finish declined Present");
    }
    assert_tearing_off(&not_fullscreen);
    assert!(not_fullscreen_recorder.records().is_empty());
    assert_direct_queue_drained(&not_fullscreen);

    let Some(mut no_capability) = direct_backend(1) else {
        return;
    };
    no_capability.platform.devices[0].async_page_flip = false;
    let no_capability_recorder = recording_sink(&mut no_capability, 0);
    let mut no_capability_state = ServerState::new();
    for index in 0..9 {
        assert!(
            try_direct_present_for_window(
                &mut no_capability,
                400 + index,
                WINDOW + 3,
                800,
                600,
                yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
            )
            .is_some()
        );
        retire_direct_present(
            &mut no_capability,
            &mut no_capability_state,
            index as u32 + 1,
        );
    }
    assert_tearing_off(&no_capability);
    assert_commit_sequence(&no_capability_recorder, &[AtomicCommitKind::Scene; 9]);
    assert_direct_queue_drained(&no_capability);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: fail to reset the tearing probation after an authoritative-root Present without Async.
fn probation_resets_on_non_async_root_present() {
    use yserver_core::server::ServerState;

    const WINDOW: u32 = 0x7500;
    let Some(mut backend) = direct_backend(1) else {
        return;
    };
    let recorder = recording_sink(&mut backend, 0);
    let mut state = ServerState::new();
    qualify_direct_async(&mut backend, &recorder, &mut state, WINDOW, 500, 7);
    assert!(try_direct_present_for_window(&mut backend, 507, WINDOW, 800, 600, 0).is_some());
    retire_direct_present(&mut backend, &mut state, 8);
    qualify_direct_async(&mut backend, &recorder, &mut state, WINDOW, 508, 7);
    assert!(matches!(
        backend.pstate.tearing.state,
        crate::kms::render::backend::TearingState::Off
    ));
    assert_eq!(backend.pstate.direct.root().tearing_probation.count, 7);
    assert_eq!(
        backend.pstate.direct.root().tearing_probation.window,
        Some(WINDOW)
    );
    assert_commit_sequence(&recorder, &[AtomicCommitKind::Scene; 15]);
    assert_direct_queue_drained(&backend);
}
#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: let a non-fullscreen Present from another window change the active tearing owner.
fn async_from_non_fullscreen_window_does_not_affect_on() {
    use yserver_core::{backend::Backend, server::ServerState};

    const WINDOW: u32 = 0x7600;
    let Some(mut backend) = direct_backend(1) else {
        return;
    };
    let recorder = recording_sink(&mut backend, 0);
    let mut state = ServerState::new();
    qualify_direct_async(&mut backend, &recorder, &mut state, WINDOW, 700, 8);

    assert!(
        try_direct_present_for_window(
            &mut backend,
            708,
            WINDOW + 1,
            400,
            300,
            yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
        )
        .is_none()
    );
    Backend::maybe_composite(&mut backend).expect("settle non-fullscreen Present");
    assert!(matches!(
        backend.pstate.tearing.state,
        crate::kms::render::backend::TearingState::On { window: WINDOW, .. }
    ));

    let before = recorder.records().len();
    assert!(
        try_direct_present_for_window(
            &mut backend,
            709,
            WINDOW,
            800,
            600,
            yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
        )
        .is_some()
    );
    assert_eq!(recorder.records()[before].kind, AtomicCommitKind::Async);
    retire_direct_present(&mut backend, &mut state, 9);
    let mut expected = vec![AtomicCommitKind::Scene; 8];
    expected.push(AtomicCommitKind::Async);
    assert_commit_sequence(&recorder, &expected);
    assert_direct_queue_drained(&backend);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: ignore a same-window Present that drops PresentOptionAsync while tearing is active.
fn non_async_present_from_w_exits() {
    use yserver_core::server::ServerState;

    const WINDOW: u32 = 0x7700;
    let Some(mut backend) = direct_backend(1) else {
        return;
    };
    let recorder = recording_sink(&mut backend, 0);
    let mut state = ServerState::new();
    qualify_direct_async(&mut backend, &recorder, &mut state, WINDOW, 800, 8);
    assert!(
        try_direct_present_for_window(
            &mut backend,
            808,
            WINDOW,
            800,
            600,
            yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
        )
        .is_some()
    );
    assert_eq!(
        recorder.records().last().unwrap().kind,
        AtomicCommitKind::Async
    );
    retire_direct_present(&mut backend, &mut state, 9);
    let old_shape = backend
        .pstate
        .scanout_m1
        .entries
        .values()
        .find_map(|entry| entry.async_shape.clone())
        .expect("an accepted async shape was cached");
    assert!(!backend.platform.async_ledger.for_tests_is_empty());

    let before = recorder.records().len();
    assert!(try_direct_present_for_window(&mut backend, 809, WINDOW, 800, 600, 0).is_some());
    assert_eq!(recorder.records()[before].kind, AtomicCommitKind::Scene);
    assert_tearing_off(&backend);
    retire_direct_present(&mut backend, &mut state, 10);
    assert!(backend.platform.async_ledger.for_tests_is_empty());

    qualify_direct_async(&mut backend, &recorder, &mut state, WINDOW, 810, 8);
    assert!(matches!(
        backend.pstate.tearing.state,
        crate::kms::render::backend::TearingState::On { .. }
    ));
    assert_eq!(recorder.records().len(), 18);
    assert_ne!(old_shape.width, 0);
    let mut expected = vec![AtomicCommitKind::Scene; 8];
    expected.extend([AtomicCommitKind::Async, AtomicCommitKind::Scene]);
    expected.extend([AtomicCommitKind::Scene; 8]);
    assert_commit_sequence(&recorder, &expected);
    assert_direct_queue_drained(&backend);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: store `completion_dst_host_xid` as the lifecycle owner, or keep lifecycle exits behind direct-frame ownership.
fn lifecycle_teardown_exits_protocol_owner_without_direct_frame() {
    use yserver_core::backend::Backend;

    const PROTOCOL_WINDOW: u32 = 0x7701;
    const HOST_WINDOW: u32 = 0x9701;
    const REDIRECTED_BACKING_PIXMAP: u32 = 0xa701;

    for destroy in [false, true] {
        let Some(mut backend) = live_scene_backend() else {
            return;
        };
        let recorder = recording_sink(&mut backend, 0);
        let _state = enter_tearing_without_direct_scanout_for_ids(
            &mut backend,
            PROTOCOL_WINDOW,
            HOST_WINDOW,
            REDIRECTED_BACKING_PIXMAP,
            820,
        );
        assert!(matches!(
            backend.pstate.tearing.state,
            crate::kms::render::backend::TearingState::On {
                window: PROTOCOL_WINDOW,
                ..
            }
        ));
        assert!(backend.pstate.direct.root().pending.is_none());
        assert!(backend.pstate.direct.root().current.is_none());
        assert!(backend.pstate.direct.root().queued_successor.is_none());
        assert_eq!(
            recorder.records().len(),
            1,
            "only the setup scene frame committed"
        );

        if destroy {
            Backend::destroy_subwindow(&mut backend, None, HOST_WINDOW)
                .expect("destroy teardown entry");
        } else {
            Backend::unmap_subwindow(&mut backend, None, HOST_WINDOW)
                .expect("unmap teardown entry");
        }

        assert_tearing_off(&backend);
        assert!(backend.pstate.direct.root().pending.is_none());
        assert!(backend.pstate.direct.root().current.is_none());
        assert!(backend.pstate.direct.root().queued_successor.is_none());
        assert_commit_sequence(&recorder, &[AtomicCommitKind::Scene]);
    }
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: return on the re-entry barrier before evaluating an authoritative Present's probation transition.
fn non_async_authoritative_present_resets_probation_during_reentry_barrier() {
    use yserver_core::{
        backend::{Backend, DrawState, GcFunction, OriginContext, SubwindowMode},
        server::ServerState,
    };
    use yserver_protocol::x11::ClientId;

    const WINDOW: u32 = 0x7702;
    let Some(mut backend) = live_scene_backend() else {
        return;
    };
    for device in &mut backend.platform.devices {
        device.async_page_flip = true;
    }
    let recorder = recording_sink(&mut backend, 0);
    let mut state = ServerState::new();
    let (width, height) = backend.platform.fb_dimensions();
    for index in 0..7_u64 {
        assert!(
            try_direct_present_with_live_storage(
                &mut backend,
                840 + index,
                WINDOW,
                width,
                height,
                yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
            )
            .is_none()
        );
        damage_root_and_composite(&mut backend);
        retire_composed_frame(&mut backend, &mut state, index as u32 + 1);
    }
    assert_eq!(backend.pstate.direct.root().tearing_probation.count, 7);
    assert_eq!(backend.pstate.direct.root().eligible_streak, 7);
    assert!(
        try_direct_present_with_live_storage(
            &mut backend,
            847,
            WINDOW,
            width,
            height,
            yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
        )
        .is_some()
    );
    assert!(backend.pstate.tearing.is_on());
    retire_direct_present(&mut backend, &mut state, 8);
    assert!(backend.pstate.direct.root().current.is_some());

    let origin = Some(OriginContext {
        client_id: ClientId(7),
        nested_seq: 0,
        opcode: 70,
    });
    let root = backend.core.window_id;
    Backend::apply_draw_state(
        &mut backend,
        origin,
        &DrawState {
            function: GcFunction::Invert,
            subwindow_mode: SubwindowMode::IncludeInferiors,
            ..DrawState::default()
        },
    )
    .expect("set root-overlay GC state");
    Backend::fill_rectangle(&mut backend, origin, root, u32::MAX, 10, 10, 50, 1)
        .expect("toggle root overlay while direct scanout is active");
    assert!(!backend.scene.root_overlay.is_empty());
    Backend::maybe_composite(&mut backend).expect("submit grouped composed unflip");
    assert!(backend.pstate.direct.root().unflip_requested);
    assert!(
        !backend
            .pstate
            .direct
            .root()
            .unflip_outputs_to_replace
            .is_empty()
    );
    retire_composed_frame(&mut backend, &mut state, 9);
    assert!(backend.pstate.direct.root().reentry_blocked_until_composed);
    assert!(backend.pstate.direct.root().current.is_none());
    assert_tearing_off(&backend);

    Backend::fill_rectangle(&mut backend, origin, root, u32::MAX, 10, 10, 50, 1)
        .expect("remove the overlay before the option-only Present");
    assert!(backend.scene.root_overlay.is_empty());

    assert!(try_direct_present_for_window(&mut backend, 848, WINDOW, width, height, 0).is_none());
    assert_eq!(backend.pstate.direct.root().tearing_probation.count, 0);
    assert_eq!(backend.pstate.direct.root().tearing_probation.window, None);
    assert!(backend.pstate.direct.root().reentry_blocked_until_composed);
    assert_tearing_off(&backend);
    damage_root_and_composite(&mut backend);
    retire_composed_frame(&mut backend, &mut state, 10);
    assert!(!backend.pstate.direct.root().reentry_blocked_until_composed);
    assert_commit_sequence(&recorder, &[AtomicCommitKind::Scene; 4]);
    assert_direct_queue_drained(&backend);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: advance tearing probation for both 0x9 and 0x8 Present options.
fn alternating_async_and_vsync_presents_never_enter() {
    use yserver_core::server::ServerState;

    const WINDOW: u32 = 0x7800;
    let Some(mut backend) = direct_backend(1) else {
        return;
    };
    let recorder = recording_sink(&mut backend, 0);
    let mut state = ServerState::new();
    for index in 0..40_u64 {
        let options = if index % 2 == 0 { 0x9 } else { 0x8 };
        assert!(
            try_direct_present_for_window(&mut backend, 900 + index, WINDOW, 800, 600, options,)
                .is_some()
        );
        retire_direct_present(&mut backend, &mut state, index as u32 + 1);
    }
    assert_tearing_off(&backend);
    assert_commit_sequence(&recorder, &[AtomicCommitKind::Scene; 40]);
    assert_direct_queue_drained(&backend);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: count qualifying Presents from different root windows as one probation streak.
fn probation_restarts_when_another_window_takes_the_root() {
    use yserver_core::server::ServerState;

    const WINDOW_A: u32 = 0x7900;
    const WINDOW_B: u32 = 0x7a00;
    let Some(mut backend) = direct_backend(1) else {
        return;
    };
    let recorder = recording_sink(&mut backend, 0);
    let mut state = ServerState::new();
    qualify_direct_async(&mut backend, &recorder, &mut state, WINDOW_A, 1000, 4);
    qualify_direct_async(&mut backend, &recorder, &mut state, WINDOW_B, 1004, 4);
    assert!(matches!(
        backend.pstate.tearing.state,
        crate::kms::render::backend::TearingState::Off
    ));
    assert_eq!(backend.pstate.direct.root().tearing_probation.count, 4);
    assert_eq!(
        backend.pstate.direct.root().tearing_probation.window,
        Some(WINDOW_B)
    );
    qualify_direct_async(&mut backend, &recorder, &mut state, WINDOW_B, 1008, 4);
    assert!(matches!(
        backend.pstate.tearing.state,
        crate::kms::render::backend::TearingState::On {
            window: WINDOW_B,
            ..
        }
    ));
    assert_commit_sequence(&recorder, &[AtomicCommitKind::Scene; 12]);
    assert_direct_queue_drained(&backend);
}
#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: leave tearing active across DPMS off, or preserve its probation across DPMS on.
fn dpms_off_exits_tearing_and_on_requires_fresh_probation() {
    use yserver_core::backend::Backend;

    const WINDOW: u32 = 0x7d00;
    let Some(mut backend) = live_scene_backend() else {
        return;
    };
    backend.platform.for_tests_accept_output_power_commits();
    let recorder = recording_sink(&mut backend, 0);
    let mut state = enter_tearing_without_direct_scanout(&mut backend, WINDOW, 1300);
    assert_eq!(recorder.records().len(), 1);
    assert_eq!(recorder.records()[0].kind, AtomicCommitKind::Scene);

    Backend::set_dpms_power(&mut backend, 3).expect("DPMS off fixture");
    assert_tearing_off(&backend);
    assert!(backend.platform.async_ledger.for_tests_is_empty());
    assert!(backend.pstate.direct.root().pending.is_none());
    assert!(backend.pstate.direct.root().queued_successor.is_none());
    assert!(backend.pstate.direct.root().current.is_none());

    Backend::set_dpms_power(&mut backend, 0).expect("DPMS on fixture");
    Backend::set_cursor_hidden(&mut backend, true);
    damage_root_and_composite(&mut backend);
    retire_composed_frame(&mut backend, &mut state, 1);
    assert!(!backend.platform.any_cursor_plane_visible());
    recorder.clear_records();
    let before = recorder.records().len();
    let mut resumed_commit_count = 0;
    for index in 0..8_u32 {
        let present_id = 1309 + u64::from(index);
        let result = if index == 7 {
            try_direct_present_with_live_storage(
                &mut backend,
                present_id,
                WINDOW,
                800,
                600,
                yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
            )
        } else {
            try_direct_present_for_window(
                &mut backend,
                present_id,
                WINDOW,
                800,
                600,
                yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
            )
        };
        if result.is_some() {
            resumed_commit_count += 1;
            retire_direct_present(&mut backend, &mut state, index + 1);
        }
    }
    assert!(matches!(
        backend.pstate.tearing.state,
        crate::kms::render::backend::TearingState::On { .. }
    ));
    assert_eq!(
        resumed_commit_count, 1,
        "M2 keeps its independent admission probation"
    );
    assert_eq!(recorder.records().len(), before + 1);
    assert_eq!(recorder.records()[before].kind, AtomicCommitKind::Scene);

    assert!(
        try_direct_present_for_window(
            &mut backend,
            1317,
            WINDOW,
            800,
            600,
            yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
        )
        .is_some()
    );
    assert_eq!(
        recorder.records().last().unwrap().kind,
        AtomicCommitKind::Async
    );
    retire_direct_present(&mut backend, &mut state, 9);
    assert_commit_sequence(
        &recorder,
        &[AtomicCommitKind::Scene, AtomicCommitKind::Async],
    );
    assert_direct_queue_drained(&backend);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: skip clearing partial Root tearing probation when exit_tearing runs while Off.
fn dpms_off_resets_partial_root_tearing_probation() {
    use yserver_core::backend::Backend;

    const WINDOW: u32 = 0x7d01;
    let Some(mut backend) = live_scene_backend() else {
        eprintln!(
            "SKIP dpms_off_resets_partial_root_tearing_probation: Vulkan fixture unavailable"
        );
        return;
    };
    for device in &mut backend.platform.devices {
        device.async_page_flip = true;
    }
    backend.platform.for_tests_accept_output_power_commits();
    let recorder = recording_sink(&mut backend, 0);
    let mut state = yserver_core::server::ServerState::new();
    let (width, height) = backend.platform.fb_dimensions();

    for index in 0..7_u64 {
        assert!(
            try_direct_present_with_live_storage(
                &mut backend,
                1320 + index,
                WINDOW,
                width,
                height,
                yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
            )
            .is_none()
        );
        damage_root_and_composite(&mut backend);
        retire_composed_frame(&mut backend, &mut state, index as u32 + 1);
    }
    assert!(matches!(
        backend.pstate.tearing.state,
        crate::kms::render::backend::TearingState::Off
    ));
    assert_eq!(backend.pstate.direct.root().tearing_probation.count, 7);
    assert_eq!(
        backend.pstate.direct.root().tearing_probation.window,
        Some(WINDOW)
    );

    Backend::set_dpms_power(&mut backend, 3).expect("DPMS off fixture");
    assert_eq!(backend.pstate.direct.root().tearing_probation.count, 0);
    assert_eq!(backend.pstate.direct.root().tearing_probation.window, None);
    assert_tearing_off(&backend);

    Backend::set_dpms_power(&mut backend, 0).expect("DPMS on fixture");
    Backend::set_cursor_hidden(&mut backend, true);
    damage_root_and_composite(&mut backend);
    retire_composed_frame(&mut backend, &mut state, 8);
    recorder.clear_records();

    assert!(
        try_direct_present_with_live_storage(
            &mut backend,
            1327,
            WINDOW,
            width,
            height,
            yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
        )
        .is_some()
    );
    assert!(matches!(
        backend.pstate.tearing.state,
        crate::kms::render::backend::TearingState::Off
    ));
    assert!(backend.platform.async_ledger.for_tests_is_empty());
    assert_eq!(backend.pstate.direct.root().tearing_probation.count, 1);
    assert_eq!(
        backend.pstate.direct.root().tearing_probation.window,
        Some(WINDOW)
    );
    assert_eq!(recorder.records().len(), 1);
    assert_eq!(recorder.records()[0].kind, AtomicCommitKind::Scene);
    retire_direct_present(&mut backend, &mut state, 9);
    assert_commit_sequence(&recorder, &[AtomicCommitKind::Scene]);
    assert_direct_queue_drained(&backend);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: retain tearing on VT release or skip the fresh eight-Present Vsync probation after acquire.
fn vt_release_exits_tearing_and_resume_needs_probation() {
    use yserver_core::{backend::Backend, core_loop::input_inventory::InputInventory};

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
                    mm_width: 0,
                    mm_height: 0,
                    edid: Vec::new(),
                    connector_type: "test".to_owned(),
                    vrr_capable: false,
                })
                .collect())
        }
    }

    const WINDOW: u32 = 0x7e00;
    let Some(mut backend) = live_scene_backend() else {
        return;
    };
    backend.for_tests_set_vt_resume_executor(std::rc::Rc::new(LogicalVtResume));
    backend.platform.for_tests_accept_output_power_commits();
    let recorder = recording_sink(&mut backend, 0);
    let mut state = enter_tearing_without_direct_scanout(&mut backend, WINDOW, 1400);
    assert_commit_sequence(&recorder, &[AtomicCommitKind::Scene]);
    Backend::set_cursor_hidden(&mut backend, true);
    damage_root_and_composite(&mut backend);
    assert!(
        backend.scene.has_pending_page_flips(),
        "expected a pending scene page flip: {}",
        tearing_composite_diagnostics(&backend, None),
    );
    retire_composed_frame(&mut backend, &mut state, 2);
    assert_eq!(
        backend.scene.cursor_mode(),
        crate::kms::render::scene::CursorPlaneMode::Hidden
    );
    assert!(backend.pstate.tearing.is_on());
    assert!(backend.pstate.direct.root().current.is_none());

    Backend::finish_vt_release(&mut backend, &mut state, &InputInventory::default());
    assert_tearing_off(&backend);
    assert!(backend.platform.async_ledger.for_tests_is_empty());
    assert!(backend.pstate.direct.root().pending.is_none());
    assert!(backend.pstate.direct.root().queued_successor.is_none());
    assert!(backend.pstate.direct.root().current.is_none());
    Backend::on_vt_acquire(&mut backend, &mut state);
    assert_tearing_off(&backend);
    assert_direct_queue_drained(&backend);
    assert_eq!(backend.pstate.vt_state, crate::vt::state::VtState::Active);
    assert!(backend.scanout_allowed());
    assert!(backend.kms_outputs_active);
    assert!(backend.platform.devices[0].async_page_flip);
    damage_root_and_composite(&mut backend);
    assert!(
        backend.scene.has_pending_page_flips(),
        "resumed VT must queue the repaint before new Presents"
    );
    retire_composed_frame(&mut backend, &mut state, 3);
    assert!(!backend.pstate.direct.root().reentry_blocked_until_composed);
    assert_eq!(
        backend.scene.cursor_mode(),
        crate::kms::render::scene::CursorPlaneMode::Hidden
    );

    let before_resume = recorder.records().len();
    for index in 0..u64::from(crate::kms::render::backend::TEARING_ENTRY_PROBATION) {
        let before_present = recorder.records().len();
        let submitted = try_direct_present_for_window(
            &mut backend,
            1409 + index,
            WINDOW,
            800,
            600,
            yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
        )
        .is_some();
        if submitted {
            assert_eq!(index, 7, "M2 admission resumes on the eighth root Present");
            assert_eq!(recorder.records().len(), before_present + 1);
            assert_eq!(
                recorder.records()[before_present].kind,
                AtomicCommitKind::Scene
            );
            retire_direct_present(&mut backend, &mut state, index as u32 + 10);
        } else {
            assert!(index < 7);
            assert_eq!(recorder.records().len(), before_present);
            assert_eq!(
                backend.pstate.direct.root().tearing_probation.count,
                index as u8 + 1
            );
            assert_eq!(
                backend.pstate.direct.root().eligible_streak,
                index as u8 + 1
            );
            damage_root_and_composite(&mut backend);
            assert!(
                backend.scene.has_pending_page_flips(),
                "expected a pending scene page flip: {}",
                tearing_composite_diagnostics(&backend, None),
            );
            assert_eq!(recorder.records().len(), before_present + 1);
            assert_eq!(
                recorder.records()[before_present].kind,
                AtomicCommitKind::Scene
            );
            retire_composed_frame(&mut backend, &mut state, index as u32 + 10);
        }
    }
    assert!(matches!(
        backend.pstate.tearing.state,
        crate::kms::render::backend::TearingState::On { window: WINDOW, .. }
    ));
    assert_eq!(
        recorder.records()[before_resume..]
            .iter()
            .map(|record| record.kind)
            .collect::<Vec<_>>(),
        vec![AtomicCommitKind::Scene; 8],
        "all eight resumed Presents remain Vsync, with the eighth direct"
    );

    assert!(
        try_direct_present_for_window(
            &mut backend,
            1417,
            WINDOW,
            800,
            600,
            yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
        )
        .is_some()
    );
    assert_eq!(
        recorder.records().last().unwrap().kind,
        AtomicCommitKind::Async
    );
    retire_direct_present(&mut backend, &mut state, 18);
    assert_eq!(
        recorder.records()[before_resume..]
            .iter()
            .map(|record| record.kind)
            .collect::<Vec<_>>(),
        [AtomicCommitKind::Scene; 8]
            .into_iter()
            .chain([AtomicCommitKind::Async])
            .collect::<Vec<_>>(),
    );
    assert!(!backend.platform.any_cursor_plane_visible());

    assert!(try_direct_present_for_window(&mut backend, 1418, WINDOW, 800, 600, 0).is_some());
    assert_eq!(
        recorder.records().last().unwrap().kind,
        AtomicCommitKind::Scene
    );
    assert_tearing_off(&backend);
    retire_direct_present(&mut backend, &mut state, 19);
    assert_eq!(
        recorder.records()[before_resume..]
            .iter()
            .map(|record| record.kind)
            .collect::<Vec<_>>(),
        [AtomicCommitKind::Scene; 8]
            .into_iter()
            .chain([AtomicCommitKind::Async, AtomicCommitKind::Scene,])
            .collect::<Vec<_>>()
    );
    assert_direct_queue_drained(&backend);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: let topology quiesce preserve the active tearing state and async ledger.
fn topology_change_exits_tearing() {
    use yserver_core::backend::Backend;

    const WINDOW: u32 = 0x7f00;
    let Some(mut backend) = live_scene_backend() else {
        return;
    };
    backend.platform.for_tests_accept_output_power_commits();
    let recorder = recording_sink(&mut backend, 0);
    let _state = enter_tearing_without_direct_scanout(&mut backend, WINDOW, 1500);
    assert_eq!(recorder.records().len(), 1);

    let (outputs, _) = Backend::randr_outputs_and_modes(&mut backend);
    let output = outputs.first().expect("test output projection");
    let connector = backend.platform.outputs[0].key.connector_name.clone();
    let _result =
        Backend::apply_crtc_config(&mut backend, output.output_id, &connector, None, 0, 0);
    assert_tearing_off(&backend);
    assert!(backend.platform.async_ledger.for_tests_is_empty());
    assert!(backend.pstate.direct.root().pending.is_none());
    assert!(backend.pstate.direct.root().queued_successor.is_none());
    assert!(backend.pstate.direct.root().current.is_none());
    assert_commit_sequence(&recorder, &[AtomicCommitKind::Scene]);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: keep cursor positions in the async builder, or keep OUT_FENCE_PTR.
fn async_direct_request_has_flag_no_out_fence_no_cursor() {
    use yserver_core::server::ServerState;

    let Some(mut backend) = live_scene_backend() else {
        return;
    };
    let recorder = recording_sink(&mut backend, 0);
    qualify_test_direct_async(&mut backend, &recorder, 0x7380, 600);
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
    backend.platform.devices[0]
        .cursor
        .crtc_states
        .get_mut(&position.crtc)
        .unwrap()
        .applied = None;
    set_scene_fence_properties(&mut backend.platform.outputs[0].output);
    let shape = try_direct_present(&mut backend, 61, 800, 600);

    let records = recorder.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].kind, AtomicCommitKind::Async);
    assert!(
        records[0]
            .flags
            .contains(::drm::control::AtomicCommitFlags::PAGE_FLIP_ASYNC)
    );
    assert!(
        records[0]
            .flags
            .contains(::drm::control::AtomicCommitFlags::NONBLOCK)
    );
    let output = &backend.platform.outputs[0].output;
    assert!(!records[0].properties.iter().any(|update| {
        update.object == position.plane.into()
            || update.object == output.crtc.into() && update.property == raw(82)
    }));
    assert_eq!(
        backend
            .pstate
            .direct
            .root()
            .pending
            .as_ref()
            .map(|frame| frame.async_outcome),
        Some(crate::drm::async_flip::AsyncOutcome::Accepted),
    );
    let mut state = ServerState::new();
    retire_recorded_direct_flip(&mut backend, &mut state, 1);
    assert!(backend.pstate.direct.root().pending.is_none());
    assert!(backend.pstate.direct.root().queued_successor.is_none());
    assert!(
        backend
            .pstate
            .direct
            .root()
            .deferred_successor_skips
            .is_empty()
    );
    assert!(
        backend
            .platform
            .async_ledger
            .allows(crate::drm::async_flip::AsyncStream::Direct, &shape)
    );
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: keep cursor positions in the async builder, or keep OUT_FENCE_PTR.
fn async_scene_request_has_flag_no_out_fence_no_cursor() {
    use yserver_core::backend::Backend;

    let Some(mut backend) = live_scene_backend() else {
        return;
    };
    let recorder = recording_sink(&mut backend, 0);
    let mut state = enter_tearing_without_direct_scanout(&mut backend, 0x7801, 12_050);
    recorder.clear_records();
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
    backend.platform.devices[0]
        .cursor
        .crtc_states
        .get_mut(&position.crtc)
        .unwrap()
        .applied = None;
    damage_root(&mut backend);
    Backend::maybe_composite(&mut backend).expect("submit composed Async while tearing is On");

    let records = recorder.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].kind, AtomicCommitKind::Async);
    assert!(
        records[0]
            .flags
            .contains(::drm::control::AtomicCommitFlags::PAGE_FLIP_ASYNC)
    );
    let output = &backend.platform.outputs[0].output;
    assert!(!records[0].properties.iter().any(|update| {
        update.object == position.plane.into()
            || update.object == output.crtc.into() && update.property == raw(82)
    }));
    assert_eq!(
        backend
            .platform
            .async_outcomes
            .get(&crate::drm::async_flip::AsyncStream::Composed(output.crtc)),
        Some(&crate::drm::async_flip::AsyncOutcome::Accepted),
    );
    retire_composed_frame(&mut backend, &mut state, 1);
    assert!(!backend.scene.has_pending_page_flips());
    assert!(backend.pstate.tearing.is_on());
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: add PAGE_FLIP_ASYNC unconditionally to either producer.
fn vsync_requests_unchanged() {
    use yserver_core::{backend::Backend, server::ServerState};

    let Some(mut direct) = direct_backend(1) else {
        return;
    };
    direct.platform.requested_flip_timing = crate::drm::async_flip::FlipTiming::Vsync;
    let direct_recorder = recording_sink(&mut direct, 0);
    let cursor = create_bitmap_cursor_via_backend(&mut direct, true, true);
    let root = direct.core.window_id;
    Backend::define_cursor(&mut direct, None, root, cursor.as_raw())
        .expect("select visible cursor for direct commit");
    let direct_position = direct.platform.for_tests_seed_atomic_cursor_position(
        0,
        raw(51),
        raw(61),
        raw(62),
        123,
        45,
        3,
        4,
    );
    set_scene_fence_properties(&mut direct.platform.outputs[0].output);
    set_unique_direct_plane_properties(&mut direct.platform.outputs[0].output);
    let direct_shape = try_direct_present(&mut direct, 62, 800, 600);

    let direct_records = direct_recorder.records();
    assert_eq!(direct_records.len(), 1);
    let expected_flags = ::drm::control::AtomicCommitFlags::PAGE_FLIP_EVENT
        | ::drm::control::AtomicCommitFlags::NONBLOCK;
    assert_eq!(direct_records[0].flags, expected_flags);
    assert_eq!(direct_records[0].kind, AtomicCommitKind::SceneWithCursor);
    let direct_output = &direct.platform.outputs[0].output;

    let expected_direct = vec![
        update(
            direct_output.plane,
            direct_output.plane_fb_id_prop,
            0x8100 + 62,
        ),
        update(
            direct_output.plane,
            direct_output.plane_crtc_id_prop,
            u64::from(u32::from(direct_output.crtc)),
        ),
        update(direct_output.plane, direct_output.plane_src_x_prop, 0),
        update(direct_output.plane, direct_output.plane_src_y_prop, 0),
        update(
            direct_output.plane,
            direct_output.plane_src_w_prop,
            800 << 16,
        ),
        update(
            direct_output.plane,
            direct_output.plane_src_h_prop,
            600 << 16,
        ),
        update(direct_output.plane, direct_output.plane_crtc_x_prop, 0),
        update(direct_output.plane, direct_output.plane_crtc_y_prop, 0),
        update(direct_output.plane, direct_output.plane_crtc_w_prop, 800),
        update(direct_output.plane, direct_output.plane_crtc_h_prop, 600),
        update(
            direct_position.plane,
            direct_position.crtc_x,
            i64::from(direct_position.x) as u64,
        ),
        update(
            direct_position.plane,
            direct_position.crtc_y,
            i64::from(direct_position.y) as u64,
        ),
    ];
    assert_eq!(direct_records[0].properties, expected_direct);
    let mut direct_state = ServerState::new();
    retire_recorded_direct_flip(&mut direct, &mut direct_state, 1);
    assert!(direct.pstate.direct.root().pending.is_none());
    assert!(
        direct
            .platform
            .async_ledger
            .allows(crate::drm::async_flip::AsyncStream::Direct, &direct_shape)
    );

    let Some(mut scene) = live_scene_backend() else {
        return;
    };
    scene.platform.requested_flip_timing = crate::drm::async_flip::FlipTiming::Vsync;
    let scene_recorder = recording_sink(&mut scene, 0);
    let scene_position = scene.platform.for_tests_seed_atomic_cursor_position(
        0,
        raw(51),
        raw(61),
        raw(62),
        123,
        45,
        3,
        4,
    );
    damage_root_and_composite(&mut scene);
    let scene_records = scene_recorder.records();
    assert_eq!(scene_records.len(), 1);
    assert_eq!(scene_records[0].flags, expected_flags);
    assert_eq!(scene_records[0].kind, AtomicCommitKind::SceneWithCursor);
    let scene_output = &scene.platform.outputs[0].output;
    let scene_fb = scene_records[0].properties[0].value;
    let in_fence_fd = scene_records[0].properties[2].value;
    let out_fence_ptr = scene_records[0].properties[3].value;
    assert!(
        in_fence_fd == u64::MAX
            || in_fence_fd <= u64::try_from(i32::MAX).expect("i32 max is nonnegative")
    );
    assert_ne!(out_fence_ptr, 0);
    let pool_has_framebuffer = scene.platform.scanout_pools[0]
        .as_ref()
        .and_then(|pool| match pool {
            crate::kms::vk::scanout::OutputScanout::Shared(pool) => Some(pool),
            crate::kms::vk::scanout::OutputScanout::Copied(_) => None,
        })
        .is_some_and(|pool| {
            pool.bos.iter().any(|bo| {
                bo.fb_handle
                    .is_some_and(|fb| u64::from(u32::from(fb)) == scene_fb)
            })
        });
    assert!(pool_has_framebuffer, "scene flip uses a pool framebuffer");

    let expected_scene = vec![
        update(scene_output.plane, scene_output.plane_fb_id_prop, scene_fb),
        update(
            scene_output.plane,
            scene_output.plane_crtc_id_prop,
            u64::from(u32::from(scene_output.crtc)),
        ),
        update(scene_output.plane, raw(81), in_fence_fd),
        update(scene_output.crtc, raw(82), out_fence_ptr),
        update(
            scene_position.plane,
            scene_position.crtc_x,
            i64::from(scene_position.x) as u64,
        ),
        update(
            scene_position.plane,
            scene_position.crtc_y,
            i64::from(scene_position.y) as u64,
        ),
    ];
    assert_eq!(scene_records[0].properties, expected_scene);
    let mut scene_state = ServerState::new();
    retire_composed_frame(&mut scene, &mut scene_state, 2);
    assert!(!scene.scene.has_pending_page_flips());
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: drop the same-frame Vsync resubmit or consume a cursor move on an
// accepted async flip.
fn async_einval_resubmits_same_frame_vsync() {
    use yserver_core::server::ServerState;

    let Some(mut backend) = live_scene_backend() else {
        return;
    };
    let recorder = recording_sink(&mut backend, 0);
    qualify_test_direct_async(&mut backend, &recorder, 0x7380, 3000);
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
    backend.platform.devices[0]
        .cursor
        .crtc_states
        .get_mut(&position.crtc)
        .unwrap()
        .applied = None;
    recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EINVAL)));
    recorder.script_result(Ok(()));
    recorder.script_result(Ok(()));
    let shape = try_direct_present(&mut backend, 31, 800, 600);

    assert_eq!(
        backend
            .pstate
            .direct
            .root()
            .pending
            .as_ref()
            .map(|frame| frame.async_outcome),
        Some(crate::drm::async_flip::AsyncOutcome::FellBack { suspended: false }),
    );
    let first = recorder.records();
    assert_eq!(first.len(), 2);
    assert_eq!(first[0].kind, AtomicCommitKind::Async);
    assert_eq!(first[1].kind, AtomicCommitKind::Scene);
    assert_eq!(first[0].properties[0].value, first[1].properties[0].value);
    assert!(
        first[0]
            .properties
            .iter()
            .all(|update| update.object != position.plane.into())
    );
    assert!(
        first[1]
            .properties
            .iter()
            .all(|update| update.object != position.plane.into())
    );
    assert!(
        backend
            .platform
            .async_ledger
            .allows(crate::drm::async_flip::AsyncStream::Direct, &shape,)
    );

    let mut state = ServerState::new();
    retire_recorded_direct_flip(&mut backend, &mut state, 1);
    let pending_cursor = backend.platform.for_tests_seed_atomic_cursor_position(
        0,
        raw(51),
        raw(61),
        raw(62),
        123,
        45,
        3,
        4,
    );
    backend.platform.devices[0]
        .cursor
        .crtc_states
        .get_mut(&pending_cursor.crtc)
        .unwrap()
        .applied = None;
    let _next_shape = try_direct_present(&mut backend, 32, 800, 600);
    assert!(
        backend
            .platform
            .for_tests_atomic_position_is_unsent(pending_cursor.device_key, pending_cursor.crtc,)
    );
    let records = recorder.records();
    assert_eq!(records.len(), 3);
    assert_eq!(records[2].kind, AtomicCommitKind::Async);
    assert_eq!(
        backend
            .pstate
            .direct
            .root()
            .pending
            .as_ref()
            .map(|frame| frame.async_outcome),
        Some(crate::drm::async_flip::AsyncOutcome::Accepted),
    );
    retire_recorded_direct_flip(&mut backend, &mut state, 2);
    assert!(backend.pstate.direct.root().pending.is_none());
    assert!(backend.pstate.direct.root().queued_successor.is_none());
    assert!(
        backend
            .pstate
            .direct
            .root()
            .deferred_successor_skips
            .is_empty()
    );
    assert_eq!(
        backend
            .platform
            .async_ledger
            .for_tests_consecutive_einval(crate::drm::async_flip::AsyncStream::Direct,),
        0
    );
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: suspend after one EINVAL, or never suspend this grouped stream.
fn multi_crtc_async_einval_suspends_after_two() {
    use yserver_core::server::ServerState;

    let Some(mut backend) = live_scene_backend_with_outputs(2) else {
        return;
    };
    let recorder = recording_sink(&mut backend, 0);
    qualify_test_direct_async(&mut backend, &recorder, 0x7380, 4000);
    for _ in 0..2 {
        recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EINVAL)));
        recorder.script_result(Ok(()));
    }
    for _ in 0..3 {
        recorder.script_result(Ok(()));
    }
    let mut state = ServerState::new();

    let first_shape = try_direct_present(&mut backend, 41, 1600, 600);
    let mut records = recorder.records();
    assert_eq!(
        records.iter().map(|record| record.kind).collect::<Vec<_>>(),
        [AtomicCommitKind::Async, AtomicCommitKind::Scene,]
    );
    assert_eq!(
        backend
            .pstate
            .direct
            .root()
            .pending
            .as_ref()
            .map(|frame| frame.async_outcome),
        Some(crate::drm::async_flip::AsyncOutcome::FellBack { suspended: false })
    );
    assert_eq!(
        records[0].events.len(),
        2,
        "direct transaction groups both CRTCs"
    );
    assert_eq!(records[1].events.len(), 2);
    retire_recorded_direct_flip(&mut backend, &mut state, 10);

    let _same_shape = try_direct_present(&mut backend, 42, 1600, 600);
    records = recorder.records();
    assert_eq!(
        records.iter().map(|record| record.kind).collect::<Vec<_>>(),
        [
            AtomicCommitKind::Async,
            AtomicCommitKind::Scene,
            AtomicCommitKind::Async,
            AtomicCommitKind::Scene,
        ]
    );
    assert_eq!(
        backend
            .pstate
            .direct
            .root()
            .pending
            .as_ref()
            .map(|frame| frame.async_outcome),
        Some(crate::drm::async_flip::AsyncOutcome::FellBack { suspended: true })
    );
    retire_recorded_direct_flip(&mut backend, &mut state, 20);

    for present_id in [43, 44] {
        let _same_shape = try_direct_present(&mut backend, present_id, 1600, 600);
        retire_recorded_direct_flip(&mut backend, &mut state, u32::try_from(present_id).unwrap());
    }
    records = recorder.records();
    assert_eq!(
        records.iter().map(|record| record.kind).collect::<Vec<_>>(),
        [
            AtomicCommitKind::Async,
            AtomicCommitKind::Scene,
            AtomicCommitKind::Async,
            AtomicCommitKind::Scene,
            AtomicCommitKind::Scene,
            AtomicCommitKind::Scene,
        ]
    );
    assert!(
        backend
            .platform
            .async_ledger
            .for_tests_consecutive_einval(crate::drm::async_flip::AsyncStream::Direct,)
            >= 2
    );

    backend.platform.outputs[1].output.crtc = raw(105);
    let output_key = backend.platform.outputs[1].key.clone();
    backend.pstate.crtc_key_by_id.remove(&101);
    backend.pstate.crtc_key_by_id.insert(105, output_key);
    backend.refresh_present_crtc_clock_epochs();
    let changed_shape = try_direct_present(&mut backend, 45, 1600, 600);
    assert_ne!(first_shape.crtcs, changed_shape.crtcs);
    records = recorder.records();
    assert_eq!(
        records.last().expect("shape-change commit").kind,
        AtomicCommitKind::Async
    );
    assert_eq!(
        backend
            .pstate
            .direct
            .root()
            .pending
            .as_ref()
            .map(|frame| frame.async_outcome),
        Some(crate::drm::async_flip::AsyncOutcome::Accepted)
    );
    retire_recorded_direct_flip(&mut backend, &mut state, 50);
    assert!(backend.pstate.direct.root().pending.is_none());
    assert!(backend.pstate.direct.root().queued_successor.is_none());
    assert!(
        backend
            .pstate
            .direct
            .root()
            .deferred_successor_skips
            .is_empty()
    );
    assert_eq!(
        backend
            .platform
            .async_ledger
            .for_tests_consecutive_einval(crate::drm::async_flip::AsyncStream::Direct,),
        0
    );
    assert!(
        backend
            .platform
            .async_ledger
            .allows(crate::drm::async_flip::AsyncStream::Direct, &changed_shape,)
    );
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: fail to make a changed shape current before its EBUSY submission.
fn direct_shape_change_on_ebusy_resets_consecutive_einval_history() {
    use yserver_core::{backend::Backend, server::ServerState};

    let Some(mut backend) = live_scene_backend_with_outputs(2) else {
        return;
    };
    let recorder = recording_sink(&mut backend, 0);
    qualify_test_direct_async(&mut backend, &recorder, 0x7380, 5000);
    recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EINVAL)));
    recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EBUSY)));
    recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EBUSY)));
    recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EINVAL)));
    recorder.script_result(Ok(()));
    let stream = crate::drm::async_flip::AsyncStream::Direct;
    let mut state = ServerState::new();

    let (candidate_a, event_a, shape_a) = prepare_direct_present(&mut backend, 71, 1600, 600);
    assert_eq!(
        Backend::try_present_direct(&mut backend, candidate_a, event_a)
            .expect_err("first Vsync fallback remains EBUSY")
            .raw_os_error(),
        Some(libc::EBUSY),
    );
    assert!(backend.pstate.direct.root().pending.is_none());
    assert_eq!(
        backend
            .platform
            .async_ledger
            .for_tests_consecutive_einval(stream),
        1,
    );

    backend.pstate.direct.root_mut().eligible_streak =
        crate::kms::render::backend::SCANOUT_M2_ELIGIBLE_ROOT_PROBATION - 1;
    set_direct_second_crtc(&mut backend, 105);
    let (candidate_b, event_b, shape_b) = prepare_direct_present(&mut backend, 72, 1600, 600);
    assert_ne!(shape_a, shape_b);
    assert_eq!(
        Backend::try_present_direct(&mut backend, candidate_b, event_b)
            .expect_err("changed-shape async EBUSY remains an error")
            .raw_os_error(),
        Some(libc::EBUSY),
    );
    assert_eq!(
        backend
            .platform
            .async_ledger
            .for_tests_consecutive_einval(stream),
        0,
        "submitting shape B resets A even when B returns EBUSY",
    );
    assert!(backend.pstate.direct.root().pending.is_none());
    assert!(backend.pstate.direct.root().queued_successor.is_none());
    assert!(
        backend
            .pstate
            .direct
            .root()
            .deferred_successor_skips
            .is_empty()
    );

    backend.pstate.direct.root_mut().eligible_streak =
        crate::kms::render::backend::SCANOUT_M2_ELIGIBLE_ROOT_PROBATION - 1;
    set_direct_second_crtc(&mut backend, 201);
    let shape_a_again = try_direct_present(&mut backend, 73, 1600, 600);
    assert_eq!(shape_a, shape_a_again);
    assert_eq!(
        backend
            .pstate
            .direct
            .root()
            .pending
            .as_ref()
            .map(|frame| frame.async_outcome),
        Some(crate::drm::async_flip::AsyncOutcome::FellBack { suspended: false }),
    );
    let records = recorder.records();
    assert_eq!(
        records.iter().map(|record| record.kind).collect::<Vec<_>>(),
        [
            AtomicCommitKind::Async,
            AtomicCommitKind::Scene,
            AtomicCommitKind::Async,
            AtomicCommitKind::Async,
            AtomicCommitKind::Scene,
        ],
    );
    assert_eq!(
        records[1].properties[0].value,
        records[0].properties[0].value
    );
    assert_eq!(
        records[4].properties[0].value,
        records[3].properties[0].value
    );
    retire_recorded_direct_flip(&mut backend, &mut state, 2);
    assert!(backend.pstate.direct.root().pending.is_none());
    assert!(backend.pstate.direct.root().queued_successor.is_none());
    assert!(
        backend
            .pstate
            .direct
            .root()
            .deferred_successor_skips
            .is_empty()
    );
    assert_eq!(
        backend
            .platform
            .async_ledger
            .for_tests_consecutive_einval(stream),
        1,
    );
    assert!(backend.platform.async_ledger.allows(stream, &shape_a));
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: treat EBUSY as EINVAL and resubmit the direct frame as Vsync.
fn async_ebusy_is_not_suspension() {
    use yserver_core::backend::Backend;

    let Some(mut backend) = live_scene_backend() else {
        return;
    };
    let recorder = recording_sink(&mut backend, 0);
    qualify_test_direct_async(&mut backend, &recorder, 0x7380, 6000);
    recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EBUSY)));
    let (candidate, event, shape) = prepare_direct_present(&mut backend, 51, 800, 600);
    assert_eq!(
        Backend::try_present_direct(&mut backend, candidate, event)
            .expect_err("async EBUSY remains an error")
            .raw_os_error(),
        Some(libc::EBUSY),
    );
    let records = recorder.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].kind, AtomicCommitKind::Async);
    assert_eq!(
        backend
            .platform
            .async_ledger
            .for_tests_consecutive_einval(crate::drm::async_flip::AsyncStream::Direct,),
        0,
    );
    assert!(
        backend
            .platform
            .async_ledger
            .allows(crate::drm::async_flip::AsyncStream::Direct, &shape,)
    );
    assert!(backend.pstate.direct.root().pending.is_none());
    assert!(backend.pstate.direct.root().queued_successor.is_none());
    assert!(
        backend
            .pstate
            .direct
            .root()
            .deferred_successor_skips
            .is_empty()
    );
}

fn live_scene_backend() -> Option<KmsBackend> {
    live_scene_backend_with_outputs(1)
}

pub(super) fn live_scene_backend_with_outputs(output_count: usize) -> Option<KmsBackend> {
    scene_backend_with_recording_pools(output_count)
        .unwrap_or_else(|error| panic!("recording scene fixture unavailable: {error}"))
}

#[test]
// Mutation T09: let any pending direct transaction suppress cursor wakeups on all outputs.
fn composed_cursor_wakeup_policy() {
    use yserver_core::backend::Backend;

    let mut backend = KmsBackend::for_tests();
    super::push_test_output(&mut backend, 0x7499);
    let fallback_id = seed_window(&mut backend, 0x7498, None, 0, 0);
    install_direct_frame_for_target_test(&mut backend, 0x7499, fallback_id, false);
    let mut pending = backend
        .pstate
        .direct
        .root_mut()
        .pending
        .take()
        .expect("test helper installed a pending frame");
    backend.pstate.direct.root_mut().hold_direct = false;
    pending.completion_output_idx = 1;
    pending.awaiting_outputs = std::collections::HashSet::from([1]);
    backend.pstate.direct.outputs[1].pending = Some(pending);
    backend.pstate.direct.outputs[1].hold_direct = true;

    // Keep the cursor move as the only possible immediate source. In
    // particular, the pending output owner itself must not schedule a wake.
    backend.scene.scene_structure_dirty = false;
    for drawable in backend.store.armed_damaged_ids() {
        if let Some(snapshot) = backend.store.peek_presentation_damage(drawable) {
            backend.store.ack_presentation_damage(snapshot);
        }
    }
    assert!(
        !backend.for_tests_scene_wants_scene(),
        "scene damage is quiet"
    );
    assert!(
        Backend::next_wakeup(&backend).is_none(),
        "pending Output(1) alone does not schedule an immediate wake"
    );
    let position = backend.platform.for_tests_seed_atomic_cursor_position(
        0,
        raw(51),
        raw(61),
        raw(62),
        100,
        100,
        0,
        0,
    );
    assert!(
        backend
            .platform
            .for_tests_atomic_position_is_unsent(position.device_key, position.crtc,)
    );

    let wakeup = Backend::next_wakeup(&backend);

    assert!(
        wakeup.is_some_and(|deadline| deadline <= std::time::Instant::now()),
        "an eligible cursor move on output 0 wakes despite Output(1)'s pending direct frame"
    );
    assert!(
        backend.pstate.direct.outputs[1].pending.is_some(),
        "next_wakeup does not consume Output(1)'s pending frame"
    );
    assert!(
        backend
            .platform
            .for_tests_atomic_position_is_unsent(position.device_key, position.crtc,)
    );
}

fn live_scene_backend_with_dead_zone() -> Option<KmsBackend> {
    let mut backend = scene_backend_with_recording_pools_and_dead_zone(2, true)
        .unwrap_or_else(|error| panic!("dead-zone scene fixture unavailable: {error}"))?;
    for device in &mut backend.platform.devices {
        device.async_page_flip = true;
    }
    Some(backend)
}

fn scene_backend_with_recording_pools(output_count: usize) -> Result<Option<KmsBackend>, String> {
    build_recording_scene_backend(output_count, false, false)
}

fn scene_backend_with_recording_pools_and_dead_zone(
    output_count: usize,
    dead_zone: bool,
) -> Result<Option<KmsBackend>, String> {
    build_recording_scene_backend(output_count, dead_zone, false)
}

fn damage_root_and_composite(backend: &mut KmsBackend) {
    use yserver_core::backend::Backend;
    damage_root(backend);
    Backend::maybe_composite(backend).expect("Backend::maybe_composite");
}

fn damage_root(backend: &mut KmsBackend) {
    use yserver_core::backend::Backend;
    let root = backend.core.window_id;
    let width = backend.platform.fb_w;
    let height = backend.platform.fb_h;
    Backend::clear_area(backend, None, root, 0, None, 0, 0, width, height, (0, 0))
        .expect("Backend::clear_area root damage");
}

fn retire_composed_frame(
    backend: &mut KmsBackend,
    state: &mut yserver_core::server::ServerState,
    frame: u32,
) {
    use std::os::fd::{AsFd as _, AsRawFd as _};
    use yserver_core::backend::Backend;

    backend.platform.wait_idle_bounded();
    for output in &backend.platform.outputs {
        backend.platform.devices[0]
            .test_page_flip_event_source
            .as_ref()
            .expect("recording page-flip source")
            .queue_flip(output.output.crtc, frame);
    }
    let drm_fd = backend.platform.devices[0].device.as_fd().as_raw_fd();
    Backend::on_page_flip_ready(backend, state, drm_fd);
}

fn assert_scene_async_vsync_pair(
    records: &[crate::drm::atomic::tests_support::AtomicCommitRecord],
    start: usize,
) {
    assert_eq!(records[start].kind, AtomicCommitKind::Async);
    assert_eq!(records[start + 1].kind, AtomicCommitKind::Scene);
    assert_eq!(
        records[start].properties[0].value,
        records[start + 1].properties[0].value
    );
    assert!(
        records[start]
            .flags
            .contains(::drm::control::AtomicCommitFlags::PAGE_FLIP_ASYNC)
    );
    assert!(!records[start].properties.iter().any(|update| {
        update.object == records[start + 1].events[0].crtc.into() && update.property == raw(82)
    }));
    assert!(records[start + 1].properties.iter().any(|update| {
        update.object == records[start + 1].events[0].crtc.into() && update.property == raw(82)
    }));
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: gate the scene deadline on a vblank target.
fn composed_frame_submits_without_waiting_for_vblank_tick() {
    use yserver_core::{
        backend::Backend,
        server::{BackendCapabilities, ServerState},
    };
    use yserver_protocol::x11::{ClientId, ResourceId};

    const WINDOW: ResourceId = ResourceId(0x0100_7800);

    let Some(mut backend) = live_scene_backend() else {
        return;
    };
    for device in &mut backend.platform.devices {
        device.async_page_flip = true;
    }
    let (pollable_device, _device_peer) = pollable_test_device();
    backend.platform.devices[0].device = pollable_device;
    let recorder = recording_sink(&mut backend, 0);
    let (outputs, modes) = Backend::randr_outputs_and_modes(&mut backend);
    let (screen_width, screen_height) = Backend::fb_dimensions(&backend);
    let mut core_state = ServerState::with_randr_outputs_and_modes(
        screen_width,
        screen_height,
        outputs,
        modes,
        BackendCapabilities::from_backend(&backend),
    );
    let stage_host = materialize_test_cow_stage(&mut backend, &mut core_state, ClientId(1), WINDOW);
    let (width, height) = Backend::fb_dimensions(&backend);
    let copy = Backend::create_pixmap(&mut backend, None, 24, width, height)
        .expect("create W Copy pixmap");
    let mut scene_state = enter_tearing_without_direct_scanout_for_ids(
        &mut backend,
        WINDOW.0,
        stage_host,
        stage_host,
        12_000,
    );
    recorder.clear_records();
    assert!(!backend.platform.any_cursor_plane_visible());

    // Retire an earlier composed frame so the arriving W Copy is the only
    // source of the immediate next scene wake.
    damage_root(&mut backend);
    Backend::maybe_composite(&mut backend).expect("submit prior composed frame");
    assert_eq!(recorder.records().len(), 1);
    assert_eq!(recorder.records()[0].kind, AtomicCommitKind::Async);
    assert!(backend.pstate.tearing.is_on());
    assert!(!backend.platform.any_cursor_plane_visible());
    retire_composed_frame(&mut backend, &mut scene_state, 45);
    assert!(!backend.scene.has_pending_page_flips());

    // This is the Backend::copy_area call used by Present's Copy fallback.
    // Inspect the wake before a core-loop tail or a second compose iteration
    // can consume the damage.
    Backend::copy_area(
        &mut backend,
        None,
        copy.as_raw(),
        stage_host,
        0,
        0,
        0,
        0,
        width,
        height,
    )
    .expect("record W Copy damage into its COW stage");
    Backend::note_present_pixmap(&mut backend, copy.as_raw(), stage_host);
    let deadline = Backend::next_wakeup(&backend).expect("W Copy damage schedules a scene wake");
    assert!(
        deadline <= std::time::Instant::now(),
        "W Copy damage must arm an immediate scene wake, got {deadline:?}"
    );

    Backend::maybe_composite(&mut backend).expect("compose W Copy in one pass");
    assert_eq!(recorder.records().len(), 2);
    assert_eq!(recorder.records()[1].kind, AtomicCommitKind::Async);
    assert!(
        backend.scene.has_pending_page_flips(),
        "expected a pending scene page flip: {}",
        tearing_composite_diagnostics(&backend, None),
    );
    assert!(backend.pstate.tearing.is_on());
    assert!(!backend.platform.any_cursor_plane_visible());
    retire_composed_frame(&mut backend, &mut scene_state, 46);
    assert!(!backend.scene.has_pending_page_flips());
    assert!(backend.pending_present_batches.is_empty());
    assert!(backend.pstate.tearing.is_on());
    assert!(!backend.pstate.direct.root().active());
    assert!(!backend.pstate.direct.root().unflip_requested);
    assert_direct_queue_drained(&backend);
    assert_commit_sequence(
        &recorder,
        &[AtomicCommitKind::Async, AtomicCommitKind::Async],
    );
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: keep scene flips on Vsync even after eight cursor-ignored qualifying Presents.
fn initial_sw_cursor_enters_tearing() {
    use yserver_core::backend::Backend;

    let Some(mut backend) = live_scene_backend() else {
        return;
    };
    let recorder = recording_sink(&mut backend, 0);
    let mut state = enter_tearing_without_direct_scanout(&mut backend, 0x7801, 12_050);
    assert!(backend.pstate.tearing.is_on());
    assert_eq!(
        backend.scene.cursor_mode(),
        crate::kms::render::scene::CursorPlaneMode::Sw
    );
    assert!(!backend.platform.any_cursor_plane_visible());
    recorder.clear_records();

    damage_root(&mut backend);
    Backend::maybe_composite(&mut backend).expect("submit initial composed tearing frame");
    assert_eq!(recorder.records().len(), 1);
    assert_eq!(recorder.records()[0].kind, AtomicCommitKind::Async);
    retire_composed_frame(&mut backend, &mut state, 41);
    assert!(backend.pstate.tearing.is_on());
    assert!(!backend.scene.has_pending_page_flips());
    assert!(!backend.platform.any_cursor_plane_visible());
    assert_commit_sequence(&recorder, &[AtomicCommitKind::Async]);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: trigger the handoff from cursor_mode() instead of desired visibility.
fn show_cursor_during_held_direct_hidden_tearing_makes_cursor_visible() {
    use yserver_core::{backend::Backend, server::ServerState};

    const WINDOW: u32 = 0x7802;
    let Some(mut backend) = live_scene_backend() else {
        return;
    };
    let recorder = recording_sink(&mut backend, 0);
    qualify_test_direct_async(&mut backend, &recorder, WINDOW, 12_600);
    let mut state = ServerState::new();
    let shape = try_direct_present_with_live_storage(
        &mut backend,
        12_699,
        WINDOW,
        800,
        600,
        yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
    )
    .expect("hold direct async frame");
    retire_direct_present(&mut backend, &mut state, 42);
    assert!(backend.pstate.direct.root().current.is_some());
    assert!(!backend.platform.any_cursor_plane_visible());
    assert!(
        backend
            .platform
            .async_ledger
            .allows(crate::drm::async_flip::AsyncStream::Direct, &shape,)
    );

    Backend::set_cursor_hidden(&mut backend, false);
    assert!(backend.pstate.direct.root().unflip_requested);
    assert_eq!(
        backend.pstate.direct.root().unflip_reason,
        Some("tearing_cursor_handoff")
    );
    assert!(backend.pstate.direct.root().current.is_some());
    Backend::maybe_composite(&mut backend).expect("submit grouped cursor handoff");
    assert!(!recorder.records().is_empty());
    assert!(backend.pstate.direct.root().current.is_some());
    retire_composed_frame(&mut backend, &mut state, 43);
    assert!(backend.pstate.direct.root().current.is_none());

    damage_root(&mut backend);
    Backend::maybe_composite(&mut backend).expect("compose visible software cursor");
    assert!(
        backend.scene.has_pending_page_flips(),
        "expected a pending scene page flip: {}",
        tearing_composite_diagnostics(&backend, None),
    );
    assert!(!backend.platform.any_cursor_plane_visible());
    assert!(backend.pstate.tearing.is_on());
    retire_composed_frame(&mut backend, &mut state, 44);
    assert_eq!(
        backend.scene.cursor_mode(),
        crate::kms::render::scene::CursorPlaneMode::Sw
    );
    assert!(!backend.cursor_hidden);
    assert!(backend.effective_cursor_xid.is_some());
    assert!(!backend.platform.any_cursor_plane_visible());
    assert!(!backend.scene.has_pending_page_flips());
    assert_commit_sequence(
        &recorder,
        &[
            AtomicCommitKind::Async,
            AtomicCommitKind::Scene,
            AtomicCommitKind::Async,
        ],
    );
    assert!(
        backend.scene.for_tests_cursor_footprint_was_presented(0),
        "the retired software scene frame includes the visible cursor footprint"
    );
}
#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: decide handoff from the retired Hidden classification instead of the cursor footprint.
fn dead_zone_motion_during_tearing_requests_cursor_handoff() {
    use yserver_core::{backend::Backend, server::ServerState};

    const WINDOW: u32 = 0x7803;
    let Some(mut backend) = live_scene_backend_with_dead_zone() else {
        return;
    };
    let recorder = recording_sink(&mut backend, 0);
    let mut state = ServerState::new();
    retire_registered_cursor_in_dead_zone(&mut backend, &mut state);
    qualify_from_dead_zone(&mut backend, &recorder, &mut state, WINDOW, 12_800);
    assert!(backend.pstate.direct.root().current.is_some());
    assert_eq!(
        backend.scene.cursor_mode(),
        crate::kms::render::scene::CursorPlaneMode::Hidden
    );
    assert!(!backend.cursor_hidden);

    Backend::warp_pointer_root(&mut backend, &mut state, 100, 100);
    Backend::maybe_composite(&mut backend).expect("wake scene after entering an output");
    assert!(backend.pstate.direct.root().unflip_requested);
    assert_eq!(
        backend.pstate.direct.root().unflip_reason,
        Some("tearing_cursor_handoff")
    );
    assert!(backend.pstate.direct.root().current.is_some());
    assert!(backend.pstate.tearing.is_on());
    Backend::maybe_composite(&mut backend).expect("submit motion handoff replacement");
    retire_composed_frame(&mut backend, &mut state, 48);
    assert!(backend.pstate.direct.root().current.is_none());
    assert!(!backend.scene.has_pending_page_flips());
    assert!(backend.pstate.tearing.is_on());
    assert!(!backend.platform.any_cursor_plane_visible());
    assert_eq!(
        backend.scene.cursor_mode(),
        crate::kms::render::scene::CursorPlaneMode::Sw
    );

    Backend::maybe_composite(&mut backend).expect("draw cursor after dead-zone handoff");
    assert!(
        backend.scene.has_pending_page_flips(),
        "expected a pending scene page flip: {}",
        tearing_composite_diagnostics(&backend, None),
    );
    assert_eq!(
        recorder.records().last().unwrap().kind,
        AtomicCommitKind::Async
    );
    retire_composed_frame(&mut backend, &mut state, 49);
    assert!(backend.scene.for_tests_cursor_footprint_was_presented(0));
    assert!(!backend.scene.for_tests_cursor_footprint_was_presented(1));
    assert_direct_queue_drained(&backend);
    let outputs = backend.platform.outputs.len();
    let mut expected = vec![AtomicCommitKind::Scene; outputs * 8 + 2];
    for _ in 0..outputs {
        expected.push(AtomicCommitKind::Async);
    }
    assert_commit_sequence(&recorder, &expected);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: revalidate geometry and stacking but skip the retained shape chain.
fn shape_loss_without_w_present_returns_to_vsync() {
    use yserver_core::backend::Backend;
    use yserver_protocol::x11::xfixes::RegionRect;

    for (index, shape) in [
        vec![RegionRect {
            x: 0,
            y: 0,
            width: 400,
            height: 600,
        }],
        Vec::new(),
    ]
    .into_iter()
    .enumerate()
    {
        let Some(mut backend) = live_scene_backend() else {
            return;
        };
        let recorder = recording_sink(&mut backend, 0);
        let owner = 0x7810;
        let mut state =
            enter_tearing_without_direct_scanout(&mut backend, owner, 12_100 + index as u64 * 20);
        recorder.clear_records();
        Backend::set_shape_rectangles(&mut backend, None, owner, 0, Some(&shape))
            .expect("apply changed owner Bounding shape");
        damage_root(&mut backend);
        Backend::maybe_composite(&mut backend).expect("compose after shape loss");

        assert_tearing_off(&backend);
        assert_eq!(recorder.records().len(), 1);
        assert_eq!(recorder.records()[0].kind, AtomicCommitKind::Scene);
        assert!(
            !recorder.records()[0]
                .flags
                .contains(::drm::control::AtomicCommitFlags::PAGE_FLIP_ASYNC)
        );
        retire_composed_frame(&mut backend, &mut state, 46 + index as u32);
        assert!(!backend.scene.has_pending_page_flips());
    }
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: apply the unredirected topmost-window check to COW descendant targets.
fn cow_descendant_composed_tearing_stays_on_across_passes() {
    use yserver_core::backend::Backend;

    let Some(mut backend) = live_scene_backend() else {
        return;
    };
    let recorder = recording_sink(&mut backend, 0);
    let cow = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
    let stage = 0x7850;
    let root = backend.core.window_id;
    let _cow_id = super::seed_window(&mut backend, cow, Some(root), 0, 0);
    let _stage_id = super::seed_window(&mut backend, stage, Some(cow), 0, 0);
    for xid in [cow, stage] {
        let geometry = backend.windows.get_mut(&xid).expect("COW tree member");
        geometry.width = backend.platform.fb_w;
        geometry.height = backend.platform.fb_h;
    }
    let mut state = enter_tearing_without_direct_scanout(&mut backend, stage, 12_900);
    recorder.clear_records();
    assert!(matches!(
        backend.pstate.tearing.state,
        crate::kms::render::backend::TearingState::On { admission, .. }
            if admission.target == crate::kms::render::backend::ScanoutM0Target::CowDescendant
    ));

    for frame in 50..60 {
        damage_root(&mut backend);
        Backend::maybe_composite(&mut backend).expect("compose stable COW descendant scene");
        assert_eq!(
            recorder.records().last().unwrap().kind,
            AtomicCommitKind::Async
        );
        assert!(backend.pstate.tearing.is_on());
        assert!(!backend.platform.any_cursor_plane_visible());
        retire_composed_frame(&mut backend, &mut state, frame);
        assert!(!backend.scene.has_pending_page_flips());
    }
    assert_eq!(recorder.records().len(), 10);
    assert!(backend.pstate.tearing.is_on());
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: a hidden COW stage Present exits tearing or requests a direct unflip.
fn hidden_cow_stage_present_does_not_exit_tearing() {
    use yserver_core::backend::Backend;

    let Some(mut backend) = live_scene_backend() else {
        return;
    };
    let recorder = recording_sink(&mut backend, 0);
    let cow = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
    let (client, stage) = (0x7851, 0x7852);
    let root = backend.core.window_id;
    super::seed_window(&mut backend, client, None, 0, 0);
    super::seed_window(&mut backend, cow, Some(root), 0, 0);
    super::seed_window(&mut backend, stage, Some(cow), 0, 0);
    for xid in [client, cow, stage] {
        let geometry = backend.windows.get_mut(&xid).expect("COW tree member");
        geometry.width = backend.platform.fb_w;
        geometry.height = backend.platform.fb_h;
    }
    let _state = enter_tearing_without_direct_scanout(&mut backend, client, 12_950);
    assert!(backend.pstate.tearing.is_on());
    recorder.clear_records();
    Backend::set_shape_rectangles(&mut backend, None, cow, 0, Some(&[]))
        .expect("shape COW Bounding region empty");

    let (width, height) = backend.platform.fb_dimensions();
    assert!(
        try_direct_present_for_window(
            &mut backend,
            13_000,
            stage,
            width,
            height,
            yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
        )
        .is_none(),
        "a shaped-away stage Present must not submit as direct scanout"
    );

    assert!(backend.pstate.tearing.is_on());
    assert!(!backend.pstate.direct.root().unflip_requested);
    assert_ne!(
        backend.pstate.direct.root().unflip_last_reason,
        Some("ineligible_authoritative_root_present")
    );
    assert!(recorder.records().is_empty());
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: reset M2 probation for cursor-only Copies while tearing is on, or stop a limited compositor Present from resetting normal M2 probation.
fn cursor_only_ineligibility_does_not_reset_direct_probation_while_on() {
    use yserver_core::{backend::Backend, resources::COMPOSITE_OVERLAY_WINDOW};

    let Some(mut backend) = live_scene_backend() else {
        return;
    };
    let recorder = recording_sink(&mut backend, 0);
    let _state = enter_tearing_without_direct_scanout(&mut backend, 0x7820, 12_200);
    recorder.clear_records();
    backend.pstate.direct.root_mut().eligible_streak = 3;
    let before_m2_probation = backend.pstate.direct.root().eligible_streak;
    for present_id in 12_300..12_320 {
        assert!(
            try_direct_present_for_window(
                &mut backend,
                present_id,
                0x7820,
                800,
                600,
                yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
            )
            .is_none()
        );
    }
    assert!(backend.pstate.tearing.is_on());
    assert_eq!(
        backend.pstate.direct.root().eligible_streak,
        before_m2_probation
    );
    assert_ne!(
        backend.pstate.direct.root().unflip_last_reason,
        Some("ineligible_authoritative_root_present")
    );
    assert!(recorder.records().is_empty());

    // A fresh compositor COW target exercises the normal M2 gate outside
    // tearing. Full Presents advance probation; alternating region-limited
    // Presents reset it, so the full frames cannot accumulate into admission.
    let Some(mut compositor) = live_scene_backend() else {
        return;
    };
    let compositor_recorder = recording_sink(&mut compositor, 0);
    let root = compositor.core.window_id;
    let cow = COMPOSITE_OVERLAY_WINDOW.0;
    let stage = 0x7821;
    super::seed_window(&mut compositor, cow, Some(root), 0, 0);
    super::seed_window(&mut compositor, stage, Some(cow), 0, 0);
    let (width, height) = compositor.platform.fb_dimensions();
    for xid in [cow, stage] {
        let window = compositor.windows.get_mut(&xid).expect("COW chain window");
        window.width = width;
        window.height = height;
    }
    compositor.core.top_level_order.push(cow);
    compositor.pstate.direct.root_mut().eligible_streak = 0;

    for index in 0..5_u64 {
        let full_id = 12_400 + index * 2;
        let (candidate, event, _) = prepare_direct_present_for_ids(
            &mut compositor,
            full_id,
            stage,
            stage,
            width,
            height,
            0,
        );
        assert!(
            !Backend::try_present_direct(&mut compositor, candidate, event)
                .expect("full compositor Present remains in M2 probation")
        );
        assert_eq!(
            compositor.pstate.direct.root().eligible_streak,
            1,
            "each full compositor Present advances probation after the preceding limited Present reset it"
        );

        let (mut candidate, event, _) = prepare_direct_present_for_ids(
            &mut compositor,
            full_id + 1,
            stage,
            stage,
            width,
            height,
            0,
        );
        candidate.valid_region_xid = 0x90;
        assert!(
            !Backend::try_present_direct(&mut compositor, candidate, event)
                .expect("region-limited compositor Present takes Copy path")
        );
        assert_eq!(compositor.pstate.direct.root().eligible_streak, 0);
        assert!(!compositor.pstate.tearing.is_on());
    }

    assert_tearing_off(&compositor);
    assert_eq!(compositor.pstate.direct.root().eligible_streak, 0);
    assert!(!compositor.pstate.direct.root().active());
    assert!(!compositor.pstate.direct.root().unflip_requested);
    assert!(compositor.scene.root_overlay.is_empty());
    assert!(compositor_recorder.records().is_empty());
    assert!(!compositor.scene.has_pending_page_flips());
    assert_direct_queue_drained(&compositor);
    assert!(compositor.platform.async_ledger.for_tests_is_empty());
}
#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: skip tearing_scene_still_owned in maybe_composite after a new top-level is mapped.
fn overlapping_window_without_w_present_returns_to_vsync() {
    use yserver_core::{backend::Backend, resources::ROOT_WINDOW};
    use yserver_protocol::x11::{ClientId, CreateWindowRequest, ResourceId};

    const WINDOW: u32 = 0x7860;
    const DIALOG: ResourceId = ResourceId(0x7861);
    let Some(mut backend) = live_scene_backend() else {
        return;
    };
    let recorder = recording_sink(&mut backend, 0);
    let mut state = enter_tearing_without_direct_scanout(&mut backend, WINDOW, 14_000);
    recorder.clear_records();

    // Mirror W into core's resource tree, retaining the host xid already used
    // by the exercised Present entry above.
    state.resources.create_window(
        ClientId(14),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(WINDOW),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 800,
            height: 600,
            border_width: 0,
            class: 1,
            visual: yserver_core::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    state
        .resources
        .window_mut(ResourceId(WINDOW))
        .expect("W core resource")
        .host_xid = yserver_core::backend::WindowHandle::from_raw(WINDOW);
    let _ = state.resources.map_window(ResourceId(WINDOW));

    state.resources.create_window(
        ClientId(14),
        CreateWindowRequest {
            depth: 24,
            window: DIALOG,
            parent: ROOT_WINDOW,
            x: 20,
            y: 20,
            width: 200,
            height: 120,
            border_width: 0,
            class: 1,
            visual: yserver_core::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let dialog_host = super::seed_window(&mut backend, 0x8000_7861, None, 20, 20);
    let _ = dialog_host;
    state
        .resources
        .window_mut(DIALOG)
        .expect("dialog core resource")
        .host_xid = yserver_core::backend::WindowHandle::from_raw(0x8000_7861);
    let _ = state.resources.map_window(DIALOG);
    Backend::sync_top_level_order(&mut backend, &state);

    damage_root(&mut backend);
    Backend::maybe_composite(&mut backend).expect("compose after dialog maps above W");
    assert_tearing_off(&backend);
    assert_eq!(backend.core.top_level_order, vec![WINDOW, 0x8000_7861]);
    assert_eq!(recorder.records().len(), 1);
    assert_eq!(recorder.records()[0].kind, AtomicCommitKind::Scene);
    assert!(
        !recorder.records()[0]
            .flags
            .contains(::drm::control::AtomicCommitFlags::PAGE_FLIP_ASYNC)
    );
    retire_composed_frame(&mut backend, &mut state, 63);
    assert!(!backend.scene.has_pending_page_flips());
    assert!(!backend.pstate.tearing.is_on());
}
#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: report Flip for a Copy, or complete it at scene-flip submission before its GPU ticket signals.
fn copies_keep_copy_completion_with_composed_async() {
    use yserver_core::{
        backend::{Backend, CompletedPresentEvent, PresentWake},
        resources::COMPOSITE_OVERLAY_WINDOW,
        server::{BackendCapabilities, ServerState},
    };
    use yserver_protocol::x11::{ClientId, CreatePixmapRequest, ResourceId};

    const WINDOW: ResourceId = ResourceId(0x0100_7865);
    const COPY0: ResourceId = ResourceId(0x0100_7866);
    const COPY1: ResourceId = ResourceId(0x0100_7867);

    let Some(mut backend) = live_scene_backend() else {
        return;
    };
    for device in &mut backend.platform.devices {
        device.async_page_flip = true;
    }
    let (pollable_device, _device_peer) = pollable_test_device();
    backend.platform.devices[0].device = pollable_device;
    let recorder = recording_sink(&mut backend, 0);
    let (outputs, modes) = Backend::randr_outputs_and_modes(&mut backend);
    let (screen_width, screen_height) = Backend::fb_dimensions(&backend);
    let mut state = ServerState::with_randr_outputs_and_modes(
        screen_width,
        screen_height,
        outputs,
        modes,
        BackendCapabilities::from_backend(&backend),
    );
    let stage_host = materialize_test_cow_stage(&mut backend, &mut state, ClientId(1), WINDOW);
    let root = backend.core.window_id;
    assert_eq!(
        backend.store.lookup(COMPOSITE_OVERLAY_WINDOW.0),
        backend.cow_id
    );
    assert_ne!(root, stage_host);
    let (width, height) = Backend::fb_dimensions(&backend);
    let copy0 = Backend::create_pixmap(&mut backend, None, 24, width, height)
        .expect("create first client Copy pixmap");
    let copy1 = Backend::create_pixmap(&mut backend, None, 24, width, height)
        .expect("create second client Copy pixmap");
    for (resource, host) in [(COPY0, copy0), (COPY1, copy1)] {
        state.resources.create_pixmap(
            ClientId(1),
            CreatePixmapRequest {
                depth: 24,
                pixmap: resource,
                drawable: WINDOW,
                width,
                height,
            },
        );
        assert!(state.resources.set_pixmap_host_xid(resource, host));
    }

    let _state = enter_tearing_without_direct_scanout_for_ids(
        &mut backend,
        WINDOW.0,
        stage_host,
        stage_host,
        14_300,
    );
    assert!(backend.pstate.tearing.is_on());
    assert_eq!(
        backend.scene.cursor_mode(),
        crate::kms::render::scene::CursorPlaneMode::Sw
    );
    let mut peer = install_test_core_client(&mut state);
    select_test_present_complete_events(&mut state, WINDOW.0);
    recorder.clear_records();

    // Leave a real composed Async flip in flight while the core handles two
    // client Presents. Copy CompleteNotify must follow GPU readiness without
    // waiting for this scene frame's page-flip retirement.
    damage_root(&mut backend);
    Backend::maybe_composite(&mut backend).expect("submit composed Async before client Copies");
    assert_eq!(recorder.records().len(), 1);
    assert_eq!(recorder.records()[0].kind, AtomicCommitKind::Async);
    assert!(backend.pstate.tearing.is_on());
    assert!(
        backend.scene.has_pending_page_flips(),
        "expected a pending scene page flip: {}",
        tearing_composite_diagnostics(&backend, None),
    );

    // Replace the real batch ticket with a controllable unsignalled test
    // ticket so readiness can be asserted without blocking lavapipe's queue.
    let vk = std::sync::Arc::clone(
        backend
            .platform
            .gpu
            .vk
            .as_ref()
            .expect("lavapipe scene fixture has a Vulkan context"),
    );
    let pending_batches_before_copy = backend.pending_present_batches.len();
    for _ in 0..8 {
        Backend::copy_area(
            &mut backend,
            None,
            copy0.as_raw(),
            stage_host,
            0,
            0,
            0,
            0,
            width,
            height,
        )
        .expect("record Copy work into W's COW backing");
    }
    Backend::note_present_pixmap(&mut backend, copy0.as_raw(), stage_host);
    Backend::enqueue_present_completion(
        &mut backend,
        CompletedPresentEvent {
            client_id: ClientId(1),
            serial: 0xfeed,
            host_xid: COPY0.0,
            dst_host_xid: WINDOW.0,
            options: yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
            present_id: 0xfeed,
            window_generation: 1,
            crtc_id: 0,
            crtc_epoch: 0,
            msc_offset: 0,
            completion_clock: None,
            wake: PresentWake::Pixmap { idle_fence_xid: 0 },
            completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
            emit_idle: true,
        },
        stage_host,
    );
    assert_eq!(
        backend.pending_present_batches.len(),
        pending_batches_before_copy + 1,
        "CopyArea adds one deferred completion batch"
    );
    let submitted_copy_ticket = backend
        .pending_present_batches
        .back()
        .unwrap()
        .ticket
        .clone()
        .expect("Copy completion batch retains its GPU ticket");
    submitted_copy_ticket
        .wait(&vk)
        .expect("wait for submitted Copy work before installing test readiness gate");
    let copy_gpu_ticket = crate::kms::render::gpu::FenceTicket::for_tests_unsignaled();
    backend
        .pending_present_batches
        .back_mut()
        .expect("Copy completion batch remains pending")
        .ticket = Some(copy_gpu_ticket.clone());
    assert!(backend.engine_copy_area_calls >= 8);
    assert!(
        !copy_gpu_ticket.poll_signaled(&vk),
        "precondition: Copy completion ticket must still be unsignalled"
    );
    assert_eq!(
        backend.pending_present_batches.len(),
        pending_batches_before_copy + 1
    );
    assert!(
        backend.scene.has_pending_page_flips(),
        "expected a pending scene page flip: {}",
        tearing_composite_diagnostics(&backend, None),
    );

    // Retire the older composed frame, then let exactly one pass submit the
    // scene frame for the Copy damage. Copy completion remains gated by its
    // own ticket even though this Async scene flip is now in flight.
    retire_composed_frame(&mut backend, &mut state, 66);
    assert!(!backend.scene.has_pending_page_flips());
    assert!(
        !copy_gpu_ticket.poll_signaled(&vk),
        "the Copy remains unready when the next scene frame is submitted"
    );
    Backend::maybe_composite(&mut backend).expect("submit composed frame for Copy damage");
    assert_eq!(recorder.records().len(), 2);
    assert_eq!(recorder.records()[1].kind, AtomicCommitKind::Async);
    assert!(
        backend.scene.has_pending_page_flips(),
        "expected a pending scene page flip: {}",
        tearing_composite_diagnostics(&backend, None),
    );
    assert!(
        Backend::drain_completed_present_events(&mut backend).is_empty(),
        "scene flip submission must not complete an unready Copy"
    );
    assert!(
        !copy_gpu_ticket.poll_signaled(&vk),
        "the Copy remains GPU-unready independently of scene submission"
    );

    copy_gpu_ticket.signal_for_tests();
    let ready_copy = Backend::drain_completed_present_events(&mut backend);
    assert_eq!(ready_copy.len(), 1);
    assert_eq!(ready_copy[0].serial, 0xfeed);
    assert_eq!(
        ready_copy[0].completion_mode,
        yserver_protocol::x11::present::COMPLETE_MODE_COPY
    );
    assert!(
        backend.scene.has_pending_page_flips(),
        "Copy CompleteNotify is ready before the composed Async flip retires"
    );

    let mut observed = Vec::new();
    let mut sequence = 1_u16;
    for pixmap in [COPY0, COPY1] {
        let (next_peer, next_sequence, completions) = run_core_present_pixmap(
            &mut state,
            &mut backend,
            peer,
            sequence,
            WINDOW.0,
            pixmap.0,
            yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
            Some(u32::from(sequence)),
        );
        peer = next_peer;
        sequence = next_sequence;
        observed.extend(completions);
        assert!(
            backend.scene.has_pending_page_flips(),
            "expected a pending scene page flip: {}",
            tearing_composite_diagnostics(&backend, None),
        );
        assert_eq!(recorder.records().len(), 2);
        assert_eq!(
            observed.last().expect("GPU-ready Copy notification").mode,
            yserver_protocol::x11::present::COMPLETE_MODE_COPY
        );
    }

    assert_eq!(
        observed
            .iter()
            .map(|event| event.serial)
            .collect::<Vec<_>>(),
        vec![1, 3]
    );
    assert!(
        observed
            .iter()
            .all(|event| { event.mode == yserver_protocol::x11::present::COMPLETE_MODE_COPY })
    );
    assert_eq!(observed.len(), 2);
    assert!(backend.pending_present_batches.is_empty());
    assert!(
        backend.scene.has_pending_page_flips(),
        "expected a pending scene page flip: {}",
        tearing_composite_diagnostics(&backend, None),
    );
    assert!(backend.pstate.tearing.is_on());

    retire_composed_frame(&mut backend, &mut state, 67);
    assert!(!backend.scene.has_pending_page_flips());
    assert!(backend.pstate.tearing.is_on());
    assert!(!backend.pstate.direct.root().active());
    assert!(!backend.pstate.direct.root().unflip_requested);
    assert!(!backend.pstate.direct.root().unflip_shadow_ready);
    assert_direct_queue_drained(&backend);
    assert_commit_sequence(
        &recorder,
        &[AtomicCommitKind::Async, AtomicCommitKind::Async],
    );
    let _keep_client_alive = peer;
}
#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: mark cursor positions submitted on an accepted async scene flip.
fn composed_async_accept_keeps_cursor_move_unsent() {
    use yserver_core::backend::Backend;

    let Some(mut backend) = live_scene_backend() else {
        return;
    };
    let recorder = recording_sink(&mut backend, 0);
    let mut state = enter_tearing_without_direct_scanout(&mut backend, 0x78a1, 15_100);
    backend.scene.for_tests_set_hw_cursor_mode();
    recorder.clear_records();
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
    backend.platform.devices[0]
        .cursor
        .crtc_states
        .get_mut(&position.crtc)
        .unwrap()
        .applied = None;
    let stream = crate::drm::async_flip::AsyncStream::Composed(position.crtc);
    damage_root(&mut backend);
    Backend::maybe_composite(&mut backend).expect("submit accepted composed Async");
    let records = recorder.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].kind, AtomicCommitKind::Async);
    assert!(
        !records[0]
            .properties
            .iter()
            .any(|update| update.object == position.plane.into())
    );
    assert!(
        backend
            .platform
            .for_tests_atomic_position_is_unsent(position.device_key, position.crtc,)
    );
    assert_eq!(
        backend.platform.async_outcomes.get(&stream),
        Some(&crate::drm::async_flip::AsyncOutcome::Accepted),
    );
    retire_composed_frame(&mut backend, &mut state, 1);
    assert!(!backend.scene.has_pending_page_flips());
    assert!(backend.pstate.tearing.is_on());
    assert!(backend.pstate.direct.root().pending.is_none());
    assert!(backend.pstate.direct.root().queued_successor.is_none());
    assert!(
        backend
            .platform
            .for_tests_atomic_position_is_unsent(position.device_key, position.crtc,)
    );
    assert_commit_sequence(&recorder, &[AtomicCommitKind::Async]);
    assert!(
        backend
            .platform
            .async_ledger
            .allows(stream, &scene_shape(&backend, 0),)
    );
    assert_eq!(
        backend.platform.async_outcomes.get(&stream),
        Some(&crate::drm::async_flip::AsyncOutcome::Accepted),
    );
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: keep the async ledger out of PlatformBackend::finish_frame.
fn composed_async_einval_suspends_after_two() {
    use yserver_core::backend::Backend;

    let Some(mut backend) = live_scene_backend() else {
        return;
    };
    let recorder = recording_sink(&mut backend, 0);
    let mut state = enter_tearing_without_direct_scanout(&mut backend, 0x78a2, 15_200);
    recorder.clear_records();
    for _ in 0..2 {
        recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EINVAL)));
        recorder.script_result(Ok(()));
    }
    let output = &backend.platform.outputs[0].output;
    let stream = crate::drm::async_flip::AsyncStream::Composed(output.crtc);
    damage_root(&mut backend);
    Backend::maybe_composite(&mut backend).expect("submit first composed Async attempt");
    let records = recorder.records();
    assert_eq!(records.len(), 2);
    assert_scene_async_vsync_pair(&records, 0);
    assert_eq!(
        backend.platform.async_outcomes.get(&stream),
        Some(&crate::drm::async_flip::AsyncOutcome::FellBack { suspended: false }),
    );
    retire_composed_frame(&mut backend, &mut state, 1);

    damage_root(&mut backend);
    Backend::maybe_composite(&mut backend).expect("submit second composed Async attempt");
    let records = recorder.records();
    assert_eq!(records.len(), 4);
    assert_scene_async_vsync_pair(&records, 2);
    assert_eq!(
        records[0].properties[0].value,
        records[1].properties[0].value
    );
    assert_eq!(
        records[2].properties[0].value,
        records[3].properties[0].value
    );
    assert_eq!(
        backend.platform.async_outcomes.get(&stream),
        Some(&crate::drm::async_flip::AsyncOutcome::FellBack { suspended: true }),
    );
    retire_composed_frame(&mut backend, &mut state, 2);

    damage_root(&mut backend);
    Backend::maybe_composite(&mut backend).expect("submit composed Vsync after suspension");
    let records = recorder.records();
    assert_eq!(records.len(), 5);
    assert_eq!(records[4].kind, AtomicCommitKind::Scene);
    retire_composed_frame(&mut backend, &mut state, 3);
    assert!(!backend.scene.has_pending_page_flips());
    assert!(
        !backend
            .platform
            .async_ledger
            .allows(stream, &scene_shape(&backend, 0))
    );
    assert_eq!(
        backend.platform.async_outcomes.get(&stream),
        Some(&crate::drm::async_flip::AsyncOutcome::NotRequested),
    );
    assert!(backend.pstate.tearing.is_on());
}

fn scene_shape(backend: &KmsBackend, output_idx: usize) -> crate::drm::async_flip::AsyncShape {
    let output = &backend.platform.outputs[output_idx].output;
    let Some(Some(crate::kms::vk::scanout::OutputScanout::Shared(pool))) =
        backend.platform.scanout_pools.get(output_idx)
    else {
        panic!("live-scene fixture has a shared scanout pool");
    };
    let bo = &pool.bos[0];
    let modifier = match pool.allocation_plan {
        crate::kms::vk::scanout::ScanoutAllocationPlan::GbmModifier(modifier)
        | crate::kms::vk::scanout::ScanoutAllocationPlan::DrmModifier(modifier) => modifier,
        crate::kms::vk::scanout::ScanoutAllocationPlan::PaddedExplicitLinear { .. }
        | crate::kms::vk::scanout::ScanoutAllocationPlan::ExplicitLinear
        | crate::kms::vk::scanout::ScanoutAllocationPlan::LegacyLinear => 0,
    };
    crate::drm::async_flip::AsyncShape {
        fourcc: 0x3432_5258,
        modifier,
        pitch: bo.pitch,
        width: bo.width,
        height: bo.height,
        crtcs: vec![output.crtc],
        direct: false,
    }
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: store only one current shape across composed CRTC streams.
fn two_outputs_composed_both_rejecting_each_reach_suspension() {
    let Some(mut backend) = live_scene_backend_with_outputs(2) else {
        return;
    };
    let recorder = recording_sink(&mut backend, 0);
    let mut state = enter_tearing_without_direct_scanout(&mut backend, 0x78a3, 15_300);
    assert!(backend.pstate.tearing.is_on());
    recorder.clear_records();
    for _ in 0..4 {
        recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EINVAL)));
        recorder.script_result(Ok(()));
    }
    recorder.script_result(Ok(()));
    recorder.script_result(Ok(()));
    let crtc_a = backend.platform.outputs[0].output.crtc;
    let crtc_b = backend.platform.outputs[1].output.crtc;
    let stream_a = crate::drm::async_flip::AsyncStream::Composed(crtc_a);
    let stream_b = crate::drm::async_flip::AsyncStream::Composed(crtc_b);

    for frame in 1..=2 {
        damage_root_and_composite(&mut backend);
        let records = recorder.records();
        let start = usize::try_from(frame - 1).unwrap() * 4;
        assert_eq!(records.len(), start + 4);
        assert_scene_async_vsync_pair(&records, start);
        assert_scene_async_vsync_pair(&records, start + 2);
        retire_composed_frame(&mut backend, &mut state, frame);
    }

    let shape_a = scene_shape(&backend, 0);
    let shape_b = scene_shape(&backend, 1);
    assert!(!backend.platform.async_ledger.allows(stream_a, &shape_a));
    assert!(!backend.platform.async_ledger.allows(stream_b, &shape_b));
    assert_eq!(
        backend
            .platform
            .async_ledger
            .for_tests_consecutive_einval(stream_a),
        2
    );
    assert_eq!(
        backend
            .platform
            .async_ledger
            .for_tests_consecutive_einval(stream_b),
        2
    );

    damage_root_and_composite(&mut backend);
    let records = recorder.records();
    assert_eq!(records.len(), 10);
    assert_eq!(records[8].kind, AtomicCommitKind::Scene);
    assert_eq!(records[9].kind, AtomicCommitKind::Scene);
    retire_composed_frame(&mut backend, &mut state, 3);

    let kinds: Vec<_> = recorder
        .records()
        .into_iter()
        .map(|record| record.kind)
        .collect();
    assert_eq!(
        kinds,
        [
            AtomicCommitKind::Async,
            AtomicCommitKind::Scene,
            AtomicCommitKind::Async,
            AtomicCommitKind::Scene,
            AtomicCommitKind::Async,
            AtomicCommitKind::Scene,
            AtomicCommitKind::Async,
            AtomicCommitKind::Scene,
            AtomicCommitKind::Scene,
            AtomicCommitKind::Scene,
        ]
    );
    assert!(!backend.scene.has_pending_page_flips());
    assert_eq!(
        backend.platform.async_outcomes.get(&stream_a),
        Some(&crate::drm::async_flip::AsyncOutcome::NotRequested),
    );
    assert_eq!(
        backend.platform.async_outcomes.get(&stream_b),
        Some(&crate::drm::async_flip::AsyncOutcome::NotRequested),
    );
}

fn clone_test_drm_output(output: &crate::platform::drm::Output) -> crate::platform::drm::Output {
    crate::platform::drm::Output {
        connector: output.connector,
        connector_name: output.connector_name.clone(),
        encoder: output.encoder,
        crtc: output.crtc,
        plane: output.plane,
        mode: output.mode,
        picked: output.picked.clone(),
        plane_fb_id_prop: output.plane_fb_id_prop,
        plane_crtc_id_prop: output.plane_crtc_id_prop,
        plane_src_x_prop: output.plane_src_x_prop,
        plane_src_y_prop: output.plane_src_y_prop,
        plane_src_w_prop: output.plane_src_w_prop,
        plane_src_h_prop: output.plane_src_h_prop,
        plane_crtc_x_prop: output.plane_crtc_x_prop,
        plane_crtc_y_prop: output.plane_crtc_y_prop,
        plane_crtc_w_prop: output.plane_crtc_w_prop,
        plane_crtc_h_prop: output.plane_crtc_h_prop,
        plane_in_fence_fd_prop: output.plane_in_fence_fd_prop,
        crtc_out_fence_ptr_prop: output.crtc_out_fence_ptr_prop,
        crtc_gamma_lut_prop: output.crtc_gamma_lut_prop,
        crtc_gamma_lut_size: output.crtc_gamma_lut_size,
        crtc_vrr_enabled_prop: output.crtc_vrr_enabled_prop,
        connector_vrr_capable: output.connector_vrr_capable,
        vrr_committed: std::cell::Cell::new(false),
        vrr_disabled: std::cell::Cell::new(false),
        vrr_clear_retry_used: std::cell::Cell::new(false),
        vrr_clear_retry_deadline: std::cell::Cell::new(None),
        vrr_clear_waiting_for_flip: std::cell::Cell::new(false),
        crtc_degamma_lut_prop: output.crtc_degamma_lut_prop,
        crtc_ctm_prop: output.crtc_ctm_prop,
        pending_gamma_lut: std::cell::RefCell::new(None),
        pending_gamma_deadline: std::cell::Cell::new(None),
        gamma_lut_disabled: std::cell::Cell::new(false),
        scanout_modifiers: output.scanout_modifiers.clone(),
        mm_width: output.mm_width,
        mm_height: output.mm_height,
        edid: output.edid.clone(),
        connector_type: output.connector_type.clone(),
        modes: output.modes.clone(),
    }
}

fn pollable_test_device() -> (
    std::rc::Rc<crate::drm::Device>,
    std::os::unix::net::UnixStream,
) {
    let (device_fd, peer) = std::os::unix::net::UnixStream::pair().expect("test socket pair");
    let device = crate::drm::Device::from_inherited_kms_fd(
        std::os::fd::OwnedFd::from(device_fd),
        "<pollable test fd>",
    );
    (std::rc::Rc::new(device), peer)
}

fn install_test_core_client(
    state: &mut yserver_core::server::ServerState,
) -> std::os::unix::net::UnixStream {
    install_test_core_client_as(state, yserver_protocol::x11::ClientId(1))
}

fn install_test_core_client_as(
    state: &mut yserver_core::server::ServerState,
    client_id: yserver_protocol::x11::ClientId,
) -> std::os::unix::net::UnixStream {
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        sync::{Arc, Mutex, atomic::AtomicU16},
        time::Duration,
    };
    use yserver_core::{server::ClientState, transport::Transport};

    let (transport, peer) = Transport::pair().expect("core test client transport");
    peer.set_read_timeout(Some(Duration::from_secs(3)))
        .expect("set core test peer timeout");
    state.clients.insert(
        client_id.0,
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
            event_masks: HashMap::new(),
            save_set: HashSet::new(),
            big_requests_enabled: false,
            xi2_masks: HashMap::new(),
            xi1_event_classes: HashSet::new(),
            xi1_window_event_classes: HashMap::new(),
            outbound: VecDeque::new(),
            watching_writable: false,
            write_failed: false,
            gone: false,
            focused_window: yserver_core::resources::ROOT_WINDOW,
            reader_control: None,
        },
    );
    peer
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TestPresentCompletion {
    serial: u32,
    mode: u8,
}

fn select_test_present_complete_events(state: &mut yserver_core::server::ServerState, window: u32) {
    state.present_event_selections.insert(
        0x0100_7efe,
        yserver_core::server::PresentEventSelection {
            owner: yserver_protocol::x11::ClientId(1),
            window: yserver_protocol::x11::ResourceId(window),
            event_mask: yserver_protocol::x11::present::EVENT_MASK_COMPLETE_NOTIFY,
        },
    );
}

fn run_core_present_pixmap(
    state: &mut yserver_core::server::ServerState,
    backend: &mut KmsBackend,
    peer: std::os::unix::net::UnixStream,
    sequence: u16,
    window: u32,
    pixmap: u32,
    options: u32,
    expected_serial: Option<u32>,
) -> (
    std::os::unix::net::UnixStream,
    u16,
    Vec<TestPresentCompletion>,
) {
    use std::{io::Read, thread};
    use yserver_core::core_loop::{
        CoreSender, Message, ResetPolicy, auth::AuthState, channel, poll_tokens::ClientIdAllocator,
        run_core,
    };
    use yserver_protocol::x11::{ClientId, RequestHeader, SequenceNumber, present};

    let (poll, sender, receiver) = channel().expect("core Present channel");
    let request_sender = sender.bind();
    let shutdown_sender = sender.clone_handle();
    struct ShutdownOnDrop(CoreSender);
    impl Drop for ShutdownOnDrop {
        fn drop(&mut self) {
            let _ = self.0.send(Message::Shutdown);
        }
    }
    let driver = thread::spawn(move || {
        let _shutdown_on_drop = ShutdownOnDrop(shutdown_sender);
        let client_id = ClientId(1);
        let mut present_body = vec![0_u8; 68];
        present_body[0..4].copy_from_slice(&window.to_le_bytes());
        present_body[4..8].copy_from_slice(&pixmap.to_le_bytes());
        present_body[8..12].copy_from_slice(&u32::from(sequence).to_le_bytes());
        present_body[36..40].copy_from_slice(&options.to_le_bytes());
        request_sender
            .send(Message::Request {
                id: client_id,
                sequence: SequenceNumber(sequence),
                accepted_at: None,
                header: RequestHeader {
                    opcode: 145,
                    data: present::PIXMAP,
                    length_units: 18,
                },
                body: present_body,
                attached_fd: None,
            })
            .expect("queue real Present::Pixmap request");
        request_sender
            .send(Message::Request {
                id: client_id,
                sequence: SequenceNumber(sequence.wrapping_add(1)),
                accepted_at: None,
                header: RequestHeader {
                    opcode: 145,
                    data: present::QUERY_CAPABILITIES,
                    length_units: 2,
                },
                body: 0_u32.to_le_bytes().to_vec(),
                attached_fd: None,
            })
            .expect("queue Present barrier request");

        let mut completions: Vec<TestPresentCompletion> = Vec::new();
        let mut got_reply = false;
        let mut peer = peer;
        while !got_reply
            || expected_serial
                .is_some_and(|serial| !completions.iter().any(|event| event.serial == serial))
        {
            let mut first = [0_u8; 1];
            peer.read_exact(&mut first)
                .expect("read Present event or barrier reply");
            match first[0] {
                35 => {
                    let mut event = [0_u8; 40];
                    event[0] = first[0];
                    peer.read_exact(&mut event[1..])
                        .expect("read CompleteNotify body");
                    assert_eq!(
                        u16::from_le_bytes([event[8], event[9]]),
                        u16::from(yserver_protocol::x11::present::EVENT_COMPLETE_NOTIFY),
                        "selected Present event is CompleteNotify"
                    );
                    completions.push(TestPresentCompletion {
                        serial: u32::from_le_bytes(event[20..24].try_into().unwrap()),
                        mode: event[11],
                    });
                }
                1 => {
                    let mut reply = [0_u8; 32];
                    reply[0] = first[0];
                    peer.read_exact(&mut reply[1..])
                        .expect("read Present barrier reply body");
                    assert_eq!(reply[0], 1, "Present barrier returned a reply");
                    got_reply = true;
                }
                response => panic!("unexpected X11 response {response} from core"),
            }
        }
        (peer, sequence.wrapping_add(2), completions)
    });

    run_core(
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
    )
    .expect("run core through Present::Pixmap and shutdown");
    driver.join().expect("Present request driver")
}

fn query_capabilities_in_core(
    state: &mut yserver_core::server::ServerState,
    backend: &mut KmsBackend,
    targets: &[u32],
) -> Vec<u32> {
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        io::Read,
        sync::{Arc, Mutex, atomic::AtomicU16},
        time::Duration,
    };
    use yserver_core::{
        core_loop::{
            CoreSender, Message, ResetPolicy, auth::AuthState, channel,
            poll_tokens::ClientIdAllocator, run_core,
        },
        server::ClientState,
        transport::Transport,
    };
    use yserver_protocol::x11::{
        ClientByteOrder, ClientId, RequestHeader, SequenceNumber, present as x11present,
    };

    let client_id = ClientId(1);
    let (transport, mut peer) = Transport::pair().expect("client socket pair");
    peer.set_read_timeout(Some(Duration::from_secs(3)))
        .expect("reply timeout");
    state.clients.insert(
        client_id.0,
        ClientState {
            writer: Arc::new(Mutex::new(transport)),
            is_local: true,
            fd_passing: true,
            internal: false,
            peer: yserver_core::peer::PeerIdentity::unknown_unix(),
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
            gone: false,
            focused_window: yserver_core::resources::ROOT_WINDOW,
            reader_control: None,
        },
    );

    let (poll, sender, receiver) = channel().expect("core channel");
    let request_sender = sender.bind();
    let shutdown_sender = sender.clone_handle();
    let targets = targets.to_vec();
    struct ShutdownOnDrop(CoreSender);
    impl Drop for ShutdownOnDrop {
        fn drop(&mut self) {
            let _ = self.0.send(Message::Shutdown);
        }
    }
    let driver = std::thread::spawn(move || {
        let _shutdown_on_drop = ShutdownOnDrop(shutdown_sender);
        let mut replies = Vec::with_capacity(targets.len());
        for (index, target) in targets.into_iter().enumerate() {
            request_sender
                .send(Message::Request {
                    id: client_id,
                    sequence: SequenceNumber(u16::try_from(index + 1).expect("sequence fits")),
                    accepted_at: None,
                    header: RequestHeader {
                        opcode: 145,
                        data: x11present::QUERY_CAPABILITIES,
                        length_units: 2,
                    },
                    body: target.to_le_bytes().to_vec(),
                    attached_fd: None,
                })
                .expect("queue Present::QueryCapabilities");

            let mut reply = [0; 32];
            peer.read_exact(&mut reply)
                .expect("read Present::QueryCapabilities reply");
            assert_eq!(reply[0], 1, "reply marker");
            replies.push(u32::from_le_bytes(reply[8..12].try_into().unwrap()));
        }
        replies
    });

    run_core(
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
    )
    .expect("run core through Present::QueryCapabilities and shutdown");
    driver.join().expect("query driver thread")
}

#[test]
fn present_capabilities_follow_the_windows_output_device() {
    use std::rc::Rc;
    use yserver_core::{
        backend::Backend,
        resources::{ROOT_VISUAL, ROOT_WINDOW},
        server::{BackendCapabilities, ServerState},
    };
    use yserver_protocol::x11::{ClientId, CreateWindowRequest, ResourceId};

    // Mutations killed: route every CRTC through the primary KMS device, or
    // hard-code `async_flip: true`; the latter also makes target 0 advertise Async.
    let mut backend = KmsBackend::for_tests();
    let (device_a, _device_a_peer) = pollable_test_device();
    backend.platform.devices[0].device = device_a;
    let device_a_key = backend.platform.devices[0].key;
    let device_b_key = crate::platform::drm::DrmDeviceKey {
        major: 226,
        minor: 99,
    };
    let commits_a =
        Rc::new(crate::drm::atomic::tests_support::RecordingAtomicCommitExecutor::default());
    backend.platform.devices[0].atomic_commit_sink =
        Rc::new(crate::drm::atomic::AtomicCommitSink::new(commits_a.clone()));
    let (device_b, _device_b_peer) = pollable_test_device();
    let commits_b =
        Rc::new(crate::drm::atomic::tests_support::RecordingAtomicCommitExecutor::default());
    backend
        .platform
        .devices
        .push(crate::kms::render::platform::KmsDevice {
            key: device_b_key,
            device: device_b,
            atomic_commit_sink: Rc::new(crate::drm::atomic::AtomicCommitSink::new(
                commits_b.clone(),
            )),
            page_flip_event_source: Rc::new(
                crate::kms::render::platform::flip_events::DrmPageFlipEventSource,
            ),
            #[cfg(test)]
            test_page_flip_event_source: None,
            cursor: crate::kms::render::platform::KmsCursorState::new(),
            async_page_flip: false,
        });

    backend.platform.devices[0].async_page_flip = true;
    let mut output_b = clone_test_drm_output(&backend.platform.outputs[0].output);
    output_b.connector_name = "test-b".to_owned();
    let route_b = crate::kms::scanout_route::ScanoutRoute::new(
        crate::kms::render::platform::RenderDeviceId::UnverifiedFallback,
        device_b_key,
        crate::kms::scanout_route::RenderKmsRelationship::Unknown,
    );
    backend
        .platform
        .outputs
        .push(crate::kms::backend::ActiveOutput::new(
            route_b,
            output_b,
            crate::drm::Swapchain::empty_for_tests(),
            800,
            0,
        ));

    let (outputs, modes) = Backend::randr_outputs_and_modes(&mut backend);
    let (width, height) = Backend::fb_dimensions(&backend);
    let mut state = ServerState::with_randr_outputs_and_modes(
        width,
        height,
        outputs,
        modes,
        BackendCapabilities::from_backend(&backend),
    );
    let window_a = ResourceId(0x0100_0001);
    let window_b = ResourceId(0x0100_0002);
    for (window, x) in [(window_a, 10), (window_b, 810)] {
        state.resources.create_window(
            ClientId(1),
            CreateWindowRequest {
                depth: 24,
                window,
                parent: ROOT_WINDOW,
                x,
                y: 10,
                width: 100,
                height: 100,
                class: 1,
                visual: ROOT_VISUAL,
                ..Default::default()
            },
        );
    }
    let crtc_a = state.randr.outputs[0].crtc_id;
    let crtc_b = state.randr.outputs[1].crtc_id;
    assert_ne!(device_a_key, device_b_key);
    assert_ne!(crtc_a, crtc_b);

    let first = query_capabilities_in_core(
        &mut state,
        &mut backend,
        &[window_a.0, window_b.0, crtc_a, crtc_b, 0],
    );
    assert_eq!(
        first.iter().map(|caps| caps & 0x1 != 0).collect::<Vec<_>>(),
        [true, false, true, false, false],
        "windows resolve to their output device and CRTC targets resolve directly",
    );
    assert!(first.iter().all(|caps| caps & 0x8 == 0));

    backend.platform.devices[0].async_page_flip = false;
    backend.platform.devices[1].async_page_flip = true;
    let swapped = query_capabilities_in_core(
        &mut state,
        &mut backend,
        &[window_a.0, window_b.0, crtc_a, crtc_b, 0],
    );
    assert_eq!(
        swapped
            .iter()
            .map(|caps| caps & 0x1 != 0)
            .collect::<Vec<_>>(),
        [false, true, false, true, false],
        "advertised Async follows the owning device when flags swap",
    );
    assert!(swapped.iter().all(|caps| caps & 0x8 == 0));

    assert!(state.present_pending_msc.is_empty());
    assert!(state.present_window_msc.is_empty());
    assert!(backend.pending_present_batches.is_empty());
    assert!(backend.pstate.pending_present_source_waits.is_empty());
    assert!(backend.retained_present_wakes.is_empty());
    assert!(backend.pending_completed_events_on_shutdown.is_empty());
    assert_eq!(backend.pstate.vt_state, crate::vt::state::VtState::Active);
    assert!(commits_a.records().is_empty());
    assert!(commits_b.records().is_empty());
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: cap only physical motion, remove the cap, or choose the slower output's interval.
fn task5_cursor_only_motion_recomposes_at_most_once_per_refresh() {
    use yserver_core::backend::Backend;

    const WINDOW: u32 = 0x7c50;
    {
        let Some(mut refresh_probe) = direct_backend(2) else {
            return;
        };
        refresh_probe.platform.outputs[0].output.picked.vrefresh = 60;
        refresh_probe.platform.outputs[1].output.picked.vrefresh = 75;
        assert_eq!(
            refresh_probe
                .pstate
                .tearing
                .cursor_compose_interval(&refresh_probe.platform, &[0, 1],),
            std::time::Duration::from_nanos(1_000_000_000 / 75),
            "a multi-output tearing window uses its shortest refresh interval"
        );
    }
    let Some((mut backend, recorder, mut state)) = live_tearing_cursor_backend(WINDOW, 15_000)
    else {
        return;
    };
    let interval = backend
        .pstate
        .tearing
        .cursor_compose_interval(&backend.platform, &backend.tearing_scope_outputs());

    Backend::warp_pointer_root(&mut backend, &mut state, 100, 100);
    Backend::maybe_composite(&mut backend).expect("compose first cursor motion");
    assert_eq!(recorder.records().len(), 1);
    assert_eq!(recorder.records()[0].kind, AtomicCommitKind::Async);
    retire_composed_frame(&mut backend, &mut state, 70);
    assert!(backend.pstate.tearing.last_cursor_compose.is_some());

    // Inject 1000 XTest master-pointer motions at a 1 kHz target inside one
    // refresh interval. These enter through Backend's shared pointer boundary
    // and must not enqueue one cursor-only compose per event.
    let x_test_last_compose = std::time::Instant::now() + std::time::Duration::from_secs(30);
    backend.pstate.tearing.last_cursor_compose = Some(x_test_last_compose);
    let x_test_expected_wake = x_test_last_compose + interval;
    for step in 0..1000_i32 {
        Backend::on_host_input(
            &mut backend,
            &mut state,
            yserver_core::core_loop::HostInputEvent::PointerMotion {
                origin: yserver_core::core_loop::InputOrigin::XTest(
                    yserver_core::xinput::DEVICEID_MASTER_POINTER,
                ),
                x: 200 + step % 300,
                y: 120 + step % 200,
                time: u32::try_from(step).unwrap(),
                relative: false,
                dx: 0,
                dy: 0,
                motion_delta: None,
            },
        );
        Backend::maybe_composite(&mut backend).expect("XTest cursor-only cap tick");
        assert_eq!(
            recorder.records().len(),
            1,
            "XTest motion {step} inside the refresh interval must not submit another frame"
        );
        assert_eq!(
            backend.pstate.tearing.deferred_cursor_wake,
            Some(x_test_expected_wake)
        );
    }
    assert_eq!(
        Backend::next_wakeup(&backend),
        Some(x_test_expected_wake),
        "XTest motion keeps the deferred wake armed"
    );
    assert_eq!(
        state.pointer_root,
        (299, 319),
        "the XTest stream moved the cursor"
    );

    // Pin the sampled compose time into the future so this test remains
    // deterministic under the required CPU load. Each warp and compose tick
    // still enters through Backend; only the timer state is seeded.
    let last_compose = std::time::Instant::now() + std::time::Duration::from_secs(30);
    backend.pstate.tearing.last_cursor_compose = Some(last_compose);
    let expected_wake = last_compose + interval;
    for offset in 0..20_i32 {
        Backend::warp_pointer_root(&mut backend, &mut state, 101 + offset, 100);
        Backend::maybe_composite(&mut backend).expect("cursor-only cap tick");
        assert_eq!(
            recorder.records().len(),
            1,
            "motion {offset} inside the refresh interval must not submit another frame"
        );
        assert_eq!(
            backend.pstate.tearing.deferred_cursor_wake,
            Some(expected_wake)
        );
    }
    assert_eq!(
        Backend::next_wakeup(&backend),
        Some(expected_wake),
        "the core loop must be armed for the deferred cursor wake"
    );

    // Advance the scheduled wake through the normal maybe_composite entry
    // without sleeping. Keep the synthetic deadline consistent with the
    // previous compose timestamp so the scheduler sees an elapsed interval.
    let deferred_wake = std::time::Instant::now();
    backend.pstate.tearing.last_cursor_compose = Some(deferred_wake - interval);
    backend.pstate.tearing.deferred_cursor_wake = Some(deferred_wake);
    Backend::maybe_composite(&mut backend).expect("compose deferred cursor motion");
    assert_eq!(recorder.records().len(), 2);
    assert_eq!(recorder.records()[1].kind, AtomicCommitKind::Async);
    retire_composed_frame(&mut backend, &mut state, 71);
    assert_commit_sequence(
        &recorder,
        &[AtomicCommitKind::Async, AtomicCommitKind::Async],
    );
    assert!(backend.pstate.tearing.is_on());
    assert_eq!(backend.pstate.tearing.deferred_cursor_wake, None);
    assert!(backend.pstate.tearing.last_cursor_compose.is_some());
    assert!(backend.kms_outputs_active);
    assert!(!backend.scene.has_pending_page_flips());
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: make a pending cursor-only deadline defer a new W Copy frame.
fn task5_client_frame_is_never_deferred_by_cursor_cap() {
    use yserver_core::{
        backend::Backend,
        server::{BackendCapabilities, ServerState},
    };
    use yserver_protocol::x11::{ClientId, ResourceId};

    const WINDOW: ResourceId = ResourceId(0x0100_7c60);
    let Some(mut backend) = live_scene_backend() else {
        return;
    };
    for device in &mut backend.platform.devices {
        device.async_page_flip = true;
    }
    let recorder = recording_sink(&mut backend, 0);
    let (outputs, modes) = Backend::randr_outputs_and_modes(&mut backend);
    let (screen_width, screen_height) = Backend::fb_dimensions(&backend);
    let mut scene_state = ServerState::with_randr_outputs_and_modes(
        screen_width,
        screen_height,
        outputs,
        modes,
        BackendCapabilities::from_backend(&backend),
    );
    let stage_host =
        materialize_test_cow_stage(&mut backend, &mut scene_state, ClientId(1), WINDOW);
    let (width, height) = Backend::fb_dimensions(&backend);
    let copy = Backend::create_pixmap(&mut backend, None, 24, width, height)
        .expect("create W Copy source");
    let mut state = enter_tearing_without_direct_scanout_for_ids(
        &mut backend,
        WINDOW.0,
        stage_host,
        stage_host,
        15_100,
    );
    recorder.clear_records();

    Backend::warp_pointer_root(&mut backend, &mut state, 100, 100);
    Backend::maybe_composite(&mut backend).expect("compose first cursor motion");
    assert_eq!(recorder.records().len(), 1);
    assert_eq!(recorder.records()[0].kind, AtomicCommitKind::Async);
    retire_composed_frame(&mut backend, &mut state, 72);
    assert_eq!(
        backend.scene.cursor_mode(),
        crate::kms::render::scene::CursorPlaneMode::Sw,
        "exit restores the visible software cursor policy"
    );

    let interval = backend
        .pstate
        .tearing
        .cursor_compose_interval(&backend.platform, &backend.tearing_scope_outputs());
    let last_compose = std::time::Instant::now() + std::time::Duration::from_secs(30);
    backend.pstate.tearing.last_cursor_compose = Some(last_compose);
    Backend::warp_pointer_root(&mut backend, &mut state, 101, 100);
    let expected_deferred_wake = last_compose + interval;
    assert_eq!(
        backend.pstate.tearing.deferred_cursor_wake,
        Some(expected_deferred_wake)
    );

    Backend::copy_area(
        &mut backend,
        None,
        copy.as_raw(),
        stage_host,
        0,
        0,
        0,
        0,
        width,
        height,
    )
    .expect("record W Copy damage");
    Backend::note_present_pixmap(&mut backend, copy.as_raw(), stage_host);
    assert!(
        Backend::next_wakeup(&backend)
            .is_some_and(|deadline| deadline <= std::time::Instant::now()),
        "client Copy damage must keep its immediate wake"
    );

    Backend::maybe_composite(&mut backend).expect("compose W Copy immediately");
    assert_eq!(recorder.records().len(), 2);
    assert_eq!(recorder.records()[1].kind, AtomicCommitKind::Async);
    assert_eq!(backend.pstate.tearing.deferred_cursor_wake, None);
    retire_composed_frame(&mut backend, &mut state, 73);
    assert_commit_sequence(
        &recorder,
        &[AtomicCommitKind::Async, AtomicCommitKind::Async],
    );
    assert!(backend.pstate.tearing.is_on());
    assert!(backend.pstate.tearing.last_cursor_compose.is_some());
    assert_eq!(backend.pstate.tearing.deferred_cursor_wake, None);
    assert!(!backend.scene.has_pending_page_flips());
    assert!(backend.pending_present_batches.is_empty());
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: global tearing scope; Root-only tearing.
fn output_owner_tears_while_other_output_composes() {
    use yserver_core::backend::Backend;

    let Some(mut fixture) = super::commit_identity::recording_scene::RecordingSceneFixture::
        with_output_admission_topology()
        .expect("dual-output admission fixture")
    else {
        eprintln!(
            "SKIP output_owner_tears_while_other_output_composes: Vulkan fixture unavailable"
        );
        return;
    };
    for device in &mut fixture.events.backend.platform.devices {
        device.async_page_flip = true;
    }
    Backend::set_cursor_hidden(&mut fixture.events.backend, true);

    let owner = fixture.install_direct_output_present(
        1,
        15_201,
        yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
    );
    let owner_record = enter_output_owner_through_backend(&mut fixture, &owner, 1);
    let crtc1 = fixture.events.backend.platform.outputs[1].output.crtc;
    let crtc0 = fixture.events.backend.platform.outputs[0].output.crtc;
    assert_eq!(owner_record.kind, AtomicCommitKind::Async);
    assert_eq!(owner_record.events.len(), 1);
    assert_eq!(owner_record.events[0].crtc, crtc1);
    assert!(matches!(
        fixture.events.backend.pstate.tearing.state,
        crate::kms::render::backend::TearingState::On { window, .. }
            if window == owner.candidate.dst_window_xid
    ));
    assert_eq!(
        fixture.events.backend.tearing_scope_outputs(),
        [1],
        "only Output(1) is owned by the tearing stream"
    );

    damage_root(&mut fixture.events.backend);
    let device_fd = fixture.kms_fd(1);
    let before_compose = fixture.events.recorders[&device_fd].records().len();
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("compose Output(0) while Output(1) tears");
    let scene0 = fixture.events.recorders[&device_fd]
        .records()
        .into_iter()
        .skip(before_compose)
        .find(|record| {
            matches!(
                record.kind,
                AtomicCommitKind::Scene | AtomicCommitKind::SceneWithCursor
            ) && record.events.iter().any(|event| event.crtc == crtc0)
        })
        .expect("Output(0) composes while Output(1) stays direct");
    assert!(
        !scene0
            .flags
            .contains(::drm::control::AtomicCommitFlags::PAGE_FLIP_ASYNC),
        "the unrelated composed output uses synchronized timing"
    );
    assert!(scene0.events.iter().all(|event| event.crtc == crtc0));
    let mut delivered = std::collections::HashSet::new();
    super::cursor_sprites::support::retire_pending_output(
        &mut fixture.events,
        &mut delivered,
        0,
        42,
    );

    let mut other = fixture.install_direct_output_present(
        0,
        15_202,
        yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
    );
    other.candidate.client_id = 2;
    other.event.client_id = yserver_protocol::x11::ClientId(2);
    let other_record = enter_output_owner_through_backend(&mut fixture, &other, 0);
    assert_eq!(other_record.kind, AtomicCommitKind::Scene);
    assert!(
        !other_record
            .flags
            .contains(::drm::control::AtomicCommitFlags::PAGE_FLIP_ASYNC),
        "a second async client receives synchronized direct flips"
    );
    assert_eq!(other_record.events.len(), 1);
    assert_eq!(other_record.events[0].crtc, crtc0);
    assert!(matches!(
        fixture.events.backend.pstate.tearing.state,
        crate::kms::render::backend::TearingState::On { window, .. }
            if window == owner.candidate.dst_window_xid
    ));
    assert!(
        fixture.events.backend.pstate.direct.outputs[1]
            .current
            .is_some()
    );
    assert!(fixture.events.backend.pstate.direct.outputs[0].active());
    fixture.finish();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: clear every record on exit; use one global tearing-probation record.
fn successor_takes_tearing_with_its_own_streak() {
    use yserver_core::backend::Backend;

    let Some(mut fixture) = super::commit_identity::recording_scene::RecordingSceneFixture::
        with_output_admission_topology()
        .expect("dual-output admission fixture")
    else {
        eprintln!("SKIP successor_takes_tearing_with_its_own_streak: Vulkan fixture unavailable");
        return;
    };
    for device in &mut fixture.events.backend.platform.devices {
        device.async_page_flip = true;
    }
    Backend::set_cursor_hidden(&mut fixture.events.backend, true);
    let first = fixture.install_direct_output_present(
        1,
        15_301,
        yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
    );
    let _first_record = enter_output_owner_through_backend(&mut fixture, &first, 1);
    let successor = fixture.install_direct_output_present(
        0,
        15_302,
        yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
    );
    let probation = usize::from(crate::kms::render::backend::TEARING_ENTRY_PROBATION);
    let mut delivered = std::collections::HashSet::new();
    for attempt in 0..probation {
        let accepted = output_probation_present(
            &mut fixture,
            &mut delivered,
            &successor,
            0,
            15_400 + u64::try_from(attempt).expect("Present id fits"),
            yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
            50 + u32::try_from(attempt).expect("frame fits"),
        );
        assert_eq!(
            accepted,
            attempt + 1 == probation,
            "the successor enters direct on its M2 threshold while the other output tears"
        );
        assert!(matches!(
            fixture.events.backend.pstate.tearing.state,
            crate::kms::render::backend::TearingState::On { window, .. }
                if window == first.candidate.dst_window_xid
        ));
    }
    assert_eq!(
        fixture.events.backend.pstate.direct.outputs[0]
            .tearing_probation
            .count,
        crate::kms::render::backend::TEARING_ENTRY_PROBATION
    );

    Backend::destroy_subwindow(&mut fixture.events.backend, None, first.target_xid)
        .expect("destroy the tearing owner's window through Backend");
    assert_eq!(
        fixture.events.backend.pstate.direct.outputs[1]
            .tearing_probation
            .count,
        0,
        "owner exit clears its own record immediately"
    );
    assert_eq!(
        fixture.events.backend.pstate.direct.outputs[1]
            .tearing_probation
            .window,
        None
    );
    assert_eq!(
        fixture.events.backend.pstate.direct.outputs[0]
            .tearing_probation
            .count,
        crate::kms::render::backend::TEARING_ENTRY_PROBATION,
        "the successor keeps its full streak across owner exit"
    );

    assert!(output_probation_present(
        &mut fixture,
        &mut delivered,
        &successor,
        0,
        15_500,
        yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
        70,
    ));
    assert!(matches!(
        fixture.events.backend.pstate.tearing.state,
        crate::kms::render::backend::TearingState::On { window, .. }
            if window == successor.candidate.dst_window_xid
    ));
    fixture.finish();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: use one global tearing-probation record; let foreign Presents reset the record.
fn foreign_presents_leave_partial_output_streak_unchanged() {
    let Some(mut fixture) = super::commit_identity::recording_scene::RecordingSceneFixture::
        with_output_admission_topology()
        .expect("dual-output admission fixture")
    else {
        eprintln!(
            "SKIP foreign_presents_leave_partial_output_streak_unchanged: Vulkan fixture unavailable"
        );
        return;
    };
    for device in &mut fixture.events.backend.platform.devices {
        device.async_page_flip = true;
    }
    let output0 = fixture.install_direct_output_present(
        0,
        15_601,
        yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
    );
    let output1 = fixture.install_direct_output_present(
        1,
        15_602,
        yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
    );
    fixture
        .events
        .backend
        .for_tests_set_scanout_m1_prober(super::present_scanout::record_scanout_m1_probe);
    let probation = usize::from(crate::kms::render::backend::TEARING_ENTRY_PROBATION);
    let mut delivered = std::collections::HashSet::new();
    for attempt in 0..3 {
        assert!(!output_probation_present(
            &mut fixture,
            &mut delivered,
            &output0,
            0,
            15_700 + u64::try_from(attempt).expect("Present id fits"),
            yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
            10 + u32::try_from(attempt).expect("frame fits"),
        ));
    }
    assert_eq!(
        fixture.events.backend.pstate.direct.outputs[0]
            .tearing_probation
            .count,
        3
    );

    assert!(!output_probation_present(
        &mut fixture,
        &mut delivered,
        &output1,
        1,
        15_710,
        yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
        20,
    ));
    assert!(!output_probation_present(
        &mut fixture,
        &mut delivered,
        &output1,
        1,
        15_711,
        0,
        21,
    ));
    assert_eq!(
        fixture.events.backend.pstate.direct.outputs[0]
            .tearing_probation
            .count,
        3,
        "Output(1)'s async and synchronized Presents leave Output(0)'s streak unchanged"
    );
    assert_eq!(
        fixture.events.backend.pstate.direct.outputs[0]
            .tearing_probation
            .window,
        Some(output0.candidate.dst_window_xid)
    );

    for attempt in 3..probation {
        let accepted = output_probation_present(
            &mut fixture,
            &mut delivered,
            &output0,
            0,
            15_700 + u64::try_from(attempt).expect("Present id fits"),
            yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
            30 + u32::try_from(attempt).expect("frame fits"),
        );
        assert_eq!(
            accepted,
            attempt + 1 == probation,
            "Output(0) reaches tearing after exactly its remaining qualifying Presents"
        );
    }
    assert!(matches!(
        fixture.events.backend.pstate.tearing.state,
        crate::kms::render::backend::TearingState::On { window, .. }
            if window == output0.candidate.dst_window_xid
    ));
    fixture.finish();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: release tearing when the owner has no recent Present.
fn paused_tearing_owner_keeps_tearing() {
    use yserver_core::backend::Backend;

    let Some(mut fixture) = super::commit_identity::recording_scene::RecordingSceneFixture::
        with_output_admission_topology()
        .expect("dual-output admission fixture")
    else {
        eprintln!("SKIP paused_tearing_owner_keeps_tearing: Vulkan fixture unavailable");
        return;
    };
    for device in &mut fixture.events.backend.platform.devices {
        device.async_page_flip = true;
    }
    Backend::set_cursor_hidden(&mut fixture.events.backend, true);
    let owner = fixture.install_direct_output_present(
        1,
        15_801,
        yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
    );
    let _owner_record = enter_output_owner_through_backend(&mut fixture, &owner, 1);
    let state = fixture.events.backend.pstate.tearing.state;
    assert!(fixture.events.backend.pstate.tearing.is_on());

    for _ in 0..4 {
        let _ = Backend::next_wakeup(&fixture.events.backend);
        Backend::maybe_composite(&mut fixture.events.backend)
            .expect("run an idle scheduling pass while the owner is paused");
        assert_eq!(
            fixture.events.backend.pstate.tearing.state, state,
            "idle scheduling leaves the tearing owner unchanged"
        );
        assert!(
            fixture.events.backend.pstate.direct.outputs[1]
                .current
                .is_some()
        );
    }
    fixture.finish();
}

mod round2;
mod round3;
mod task2;
