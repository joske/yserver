use super::*;

// A3 uses the existing recording fixture for Vulkan and accepted KMS commits,
// but it does not seed ServerState window trees or drive their Backend events.
// These small helpers supply that missing scene setup and inspection while
// keeping each test on the normal Present, scheduler, and Backend paths.
fn a3_records_since(
    fixture: &super::commit_identity::recording_scene::RecordingSceneFixture,
    fd: std::os::fd::RawFd,
    offset: usize,
) -> Vec<crate::drm::atomic::tests_support::AtomicCommitRecord> {
    fixture.events.recorders[&fd]
        .records()
        .into_iter()
        .skip(offset)
        .filter(|record| record.device_fd == fd)
        .collect()
}

pub(crate) fn a3_admit_output_owner(
    fixture: &mut super::commit_identity::recording_scene::RecordingSceneFixture,
    direct: &super::commit_identity::recording_scene::DirectPresentSetup,
    output_idx: usize,
) -> Vec<crate::drm::atomic::tests_support::AtomicCommitRecord> {
    use yserver_core::backend::Backend;

    let fd = fixture.kms_fd(output_idx);
    let start = fixture.events.recorders[&fd].records().len();
    fixture
        .events
        .backend
        .for_tests_set_scanout_m1_prober(record_scanout_m1_probe);
    Backend::note_present_scanout_candidate(&mut fixture.events.backend, direct.candidate);
    let probation = usize::from(crate::kms::render::backend::SCANOUT_M2_ELIGIBLE_ROOT_PROBATION);
    for attempt in 0..probation {
        assert_eq!(
            Backend::try_present_direct(
                &mut fixture.events.backend,
                direct.candidate,
                direct.event.clone(),
            )
            .expect("Output Present enters through Backend"),
            attempt + 1 == probation,
            "Output({output_idx}) enters after normal probation"
        );
    }
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("scheduler submits the Output owner");
    fixture.events.wait_for_submitted_gpu_work();

    let owner = &fixture.events.backend.pstate.direct.outputs[output_idx];
    let pending_id = owner
        .pending
        .as_ref()
        .and_then(|frame| frame.accepted_event_id)
        .expect("recorded direct commit identity");
    let crtc = fixture.events.backend.platform.outputs[output_idx]
        .output
        .crtc;
    let records = a3_records_since(fixture, fd, start);
    assert!(
        records.iter().any(|record| {
            super::commit_identity::accepted_event_id(record) == Some(pending_id)
                && record.events.len() == 1
                && record.events[0].crtc == crtc
        }),
        "Output Present made one accepted CRTC commit on its owner"
    );
    records
}

pub(crate) fn a3_enter_output_owner(
    fixture: &mut super::commit_identity::recording_scene::RecordingSceneFixture,
    direct: &super::commit_identity::recording_scene::DirectPresentSetup,
    output_idx: usize,
) {
    use yserver_core::backend::Backend;

    let records = a3_admit_output_owner(fixture, direct, output_idx);
    a3_retire_records(fixture, &records);
    let completed = Backend::drain_completed_present_events(&mut fixture.events.backend);
    assert_eq!(completed.len(), 1, "one Present completes once");
    assert_eq!(completed[0].present_id, direct.candidate.present_id);
    let current = fixture.events.backend.pstate.direct.outputs[output_idx]
        .current
        .as_ref()
        .expect("owner is held after its CRTC retirement");
    assert_eq!(current.candidate.present_id, direct.candidate.present_id);
}

fn a3_enter_root_owner(
    fixture: &mut super::commit_identity::recording_scene::RecordingSceneFixture,
    direct: &super::commit_identity::recording_scene::DirectPresentSetup,
) {
    use yserver_core::backend::Backend;

    let fd = fixture.kms_fd(0);
    let start = fixture.events.recorders[&fd].records().len();
    let probation = usize::from(crate::kms::render::backend::SCANOUT_M2_ELIGIBLE_ROOT_PROBATION);
    for attempt in 0..probation {
        assert_eq!(
            Backend::try_present_direct(
                &mut fixture.events.backend,
                direct.candidate,
                direct.event.clone(),
            )
            .expect("Root Present enters through Backend"),
            attempt + 1 == probation,
            "Root enters after normal probation"
        );
    }
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("scheduler submits the Root owner");
    fixture.events.wait_for_submitted_gpu_work();
    let pending_id = fixture
        .events
        .backend
        .pstate
        .direct
        .root()
        .pending
        .as_ref()
        .and_then(|frame| frame.accepted_event_id)
        .expect("recorded grouped direct commit identity");
    let record = a3_records_since(fixture, fd, start)
        .into_iter()
        .find(|record| {
            super::commit_identity::accepted_event_id(record) == Some(pending_id)
                && record.events.len() == fixture.events.backend.platform.outputs.len()
        })
        .expect("Root Present made one grouped commit");
    for event in &record.events {
        fixture.events.queue_commit_completion(
            fd,
            event.crtc,
            pending_id,
            51,
            std::time::Duration::from_millis(510),
        );
    }
    Backend::on_page_flip_ready(&mut fixture.events.backend, &mut fixture.events.state, fd);
    let completed = Backend::drain_completed_present_events(&mut fixture.events.backend);
    assert_eq!(completed.len(), 1, "grouped Present completes once");
    assert_eq!(completed[0].present_id, direct.candidate.present_id);
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
}

fn a3_tick(
    fixture: &mut super::commit_identity::recording_scene::RecordingSceneFixture,
) -> Vec<crate::drm::atomic::tests_support::AtomicCommitRecord> {
    use yserver_core::backend::Backend;

    let fd = fixture.kms_fd(0);
    let start = fixture.events.recorders[&fd].records().len();
    Backend::maybe_composite(&mut fixture.events.backend).expect("Backend scheduler pass");
    fixture.events.wait_for_submitted_gpu_work();
    a3_records_since(fixture, fd, start)
}

fn a3_queue_first_output_entry(
    fixture: &mut super::commit_identity::recording_scene::RecordingSceneFixtureGuard,
    direct: &super::commit_identity::recording_scene::DirectPresentSetup,
    color: u32,
) -> (u64, u64) {
    use yserver_core::backend::Backend;

    fixture
        .events
        .backend
        .for_tests_set_scanout_m1_prober(super::present_scanout::record_scanout_m1_probe);
    Backend::fill_rectangle(
        &mut fixture.events.backend,
        None,
        direct.candidate.src_host_xid,
        color,
        0,
        0,
        direct.candidate.src_width,
        direct.candidate.src_height,
    )
    .expect("paint queued first-entry source through Backend");
    let gate = a3_submit_gamma_commit(fixture, 1);
    assert!(
        a3_offer_output_candidate_after_probation(fixture, direct),
        "busy Output candidate enters the existing successor slot"
    );
    let queued = fixture.events.backend.pstate.direct.outputs[1]
        .queued_successor
        .as_ref()
        .expect("first-entry Present is queued behind the CRTC commit");
    assert!(queued.source_pin != queued.fallback_target_pin);
    let pins = (queued.source_pin, queued.fallback_target_pin);
    a3_retire_records(fixture, std::slice::from_ref(&gate));
    assert!(
        fixture.events.backend.pstate.direct.outputs[1]
            .queued_successor
            .is_some(),
        "retiring the gate leaves the first-entry Present queued until a scheduler pass"
    );
    pins
}

fn a3_assert_queued_entry_copy_completion(
    fixture: &mut super::commit_identity::recording_scene::RecordingSceneFixtureGuard,
    present_id: u64,
    source_pin: u64,
    fallback_pin: u64,
) {
    use yserver_core::backend::Backend;

    fixture.events.wait_for_submitted_gpu_work();
    let completed = Backend::drain_completed_present_events(&mut fixture.events.backend);
    let matching: Vec<_> = completed
        .iter()
        .filter(|event| event.present_id == present_id)
        .collect();
    assert_eq!(matching.len(), 1, "queued fallback completes exactly once");
    assert_eq!(
        matching[0].completion_mode,
        yserver_protocol::x11::present::COMPLETE_MODE_COPY,
        "queued fallback completes as Copy"
    );
    assert!(
        !fixture
            .events
            .backend
            .present_source_pins
            .contains_key(&source_pin)
    );
    assert!(
        !fixture
            .events
            .backend
            .present_source_pins
            .contains_key(&fallback_pin)
    );
    assert!(
        Backend::drain_completed_present_events(&mut fixture.events.backend)
            .iter()
            .all(|event| event.present_id != present_id),
        "queued fallback never emits a second completion"
    );
}

fn a3_retire_records(
    fixture: &mut super::commit_identity::recording_scene::RecordingSceneFixture,
    records: &[crate::drm::atomic::tests_support::AtomicCommitRecord],
) {
    use yserver_core::backend::Backend;

    let fd = fixture.kms_fd(0);
    for record in records {
        let Some(event_id) = super::commit_identity::accepted_event_id(record) else {
            continue;
        };
        for event in &record.events {
            fixture.events.queue_commit_completion(
                fd,
                event.crtc,
                event_id,
                60,
                std::time::Duration::from_millis(600),
            );
        }
    }
    Backend::on_page_flip_ready(&mut fixture.events.backend, &mut fixture.events.state, fd);
}

fn a3_has_scene_commit_for(
    records: &[crate::drm::atomic::tests_support::AtomicCommitRecord],
    crtc: ::drm::control::crtc::Handle,
) -> bool {
    records.iter().any(|record| {
        matches!(
            record.kind,
            crate::drm::atomic::AtomicCommitKind::Scene
                | crate::drm::atomic::AtomicCommitKind::SceneWithCursor
        ) && record.events.iter().any(|event| event.crtc == crtc)
    })
}

fn a3_seed_ordered_window(
    fixture: &mut super::commit_identity::recording_scene::RecordingSceneFixture,
    state: &mut yserver_core::server::ServerState,
    resource_id: yserver_protocol::x11::ResourceId,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    mapped: bool,
) -> u32 {
    use crate::kms::render::store::DrawableKind;
    use yserver_core::{backend::Backend, resources::ROOT_WINDOW};

    super::seed_state_window(
        state,
        &mut fixture.events.backend,
        resource_id,
        ROOT_WINDOW,
        x,
        y,
        width,
        height,
    );
    let host_xid = super::synth_host_xid(resource_id);
    let backend = &mut fixture.events.backend;
    let old = backend
        .store
        .lookup(host_xid)
        .expect("seeded window has a storage record");
    backend.store.detach_xid(host_xid);
    backend.store_decref_with_invalidate(old);
    let storage = backend
        .platform
        .gpu
        .allocate_drawable_storage(width, height, 24)
        .expect("allocate test scene window image");
    backend
        .store
        .allocate(host_xid, DrawableKind::Window, 24, true, storage)
        .expect("register test scene window image");
    Backend::put_image(
        backend,
        None,
        host_xid,
        24,
        1,
        1,
        0,
        0,
        &[0x20, 0x40, 0x80, 0],
    )
    .expect("initialize test scene window pixels");
    if mapped {
        let _ = state.resources.map_window(resource_id);
    } else {
        backend.windows.get_mut(&host_xid).unwrap().mapped = false;
        backend
            .store
            .set_scene_participating(backend.store.lookup(host_xid).unwrap(), false);
    }
    backend.core.top_level_order.push(host_xid);
    host_xid
}

fn a3_add_existing_window_to_state(
    state: &mut yserver_core::server::ServerState,
    resource_id: yserver_protocol::x11::ResourceId,
    host_xid: u32,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    mapped: bool,
) {
    use yserver_core::{
        backend::WindowHandle,
        resources::{ROOT_VISUAL, ROOT_WINDOW},
    };
    use yserver_protocol::x11::{ClientId, CreateWindowRequest};

    state.resources.create_window(
        ClientId(14),
        CreateWindowRequest {
            depth: 24,
            window: resource_id,
            parent: ROOT_WINDOW,
            x,
            y,
            width,
            height,
            border_width: 0,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default()
        },
    );
    state
        .resources
        .window_mut(resource_id)
        .expect("register state window")
        .host_xid = WindowHandle::from_raw(host_xid);
    if mapped {
        let _ = state.resources.map_window(resource_id);
    }
}

fn a3_restack_top_level(
    fixture: &mut super::commit_identity::recording_scene::RecordingSceneFixture,
    state: &mut yserver_core::server::ServerState,
    window: yserver_protocol::x11::ResourceId,
    sibling: yserver_protocol::x11::ResourceId,
    window_host: u32,
    sibling_host: u32,
    stack_mode: u8,
) {
    use yserver_core::{backend::Backend, host_x11::HostSubwindowConfig};
    use yserver_protocol::x11::ConfigureWindowRequest;

    Backend::configure_subwindow(
        &mut fixture.events.backend,
        None,
        window_host,
        HostSubwindowConfig {
            sibling: Some(sibling_host),
            stack_mode: Some(stack_mode),
            ..Default::default()
        },
    )
    .expect("ConfigureWindow backend callback");
    state.resources.configure_window(ConfigureWindowRequest {
        window,
        value_mask: 0,
        x: None,
        y: None,
        width: None,
        height: None,
        border_width: None,
        sibling: Some(sibling),
        stack_mode: Some(stack_mode),
    });
    Backend::sync_top_level_order(&mut fixture.events.backend, state);
}

fn a3_output_fixture(
    output_idx: usize,
    present_id: u64,
) -> Option<(
    super::commit_identity::recording_scene::RecordingSceneFixture,
    super::commit_identity::recording_scene::DirectPresentSetup,
)> {
    super::commit_identity::recording_scene::RecordingSceneFixture::with_output_admission_topology()
        .expect("A3 two-output recording fixture")
        .map(|mut fixture| {
            let direct = fixture.install_direct_output_present(output_idx, present_id, 0);
            (fixture, direct)
        })
}

fn a3_root_fixture(
    present_id: u64,
) -> Option<(
    super::commit_identity::recording_scene::RecordingSceneFixture,
    super::commit_identity::recording_scene::DirectPresentSetup,
)> {
    super::commit_identity::recording_scene::RecordingSceneFixture::with_route(
        2,
        super::commit_identity::recording_scene::P1SceneRoute::Shared,
    )
    .expect("A3 same-refresh Root recording fixture")
    .map(|mut fixture| {
        let direct = fixture.install_direct_present(present_id, 0);
        (fixture, direct)
    })
}

fn a3_materialize_cow(
    fixture: &mut super::commit_identity::recording_scene::RecordingSceneFixture,
    state: &mut yserver_core::server::ServerState,
) -> u32 {
    use yserver_core::{backend::Backend, core_loop::process_request};
    use yserver_protocol::x11::{ClientId, RequestHeader, SequenceNumber, composite};

    super::super::install_client_for_render(state, 14);
    let body = yserver_core::resources::ROOT_WINDOW.0.to_le_bytes();
    process_request::process_request(
        state,
        &mut fixture.events.backend as &mut dyn Backend,
        ClientId(14),
        SequenceNumber(1),
        RequestHeader {
            opcode: 144,
            data: composite::GET_OVERLAY_WINDOW,
            length_units: 2,
        },
        &body,
        None,
    )
    .expect("materialize the Composite overlay through Backend");
    fixture
        .events
        .backend
        .cow_host_xid()
        .expect("GetOverlayWindow returns the materialized COW")
}

fn a3_install_root_present(
    fixture: &mut super::commit_identity::recording_scene::RecordingSceneFixture,
    present_id: u64,
    target_xid: u32,
) -> super::commit_identity::recording_scene::DirectPresentSetup {
    let mut direct = fixture.install_direct_present(present_id, 0);
    let backend = &mut fixture.events.backend;
    backend
        .core
        .top_level_order
        .retain(|&xid| xid != direct.target_xid);
    direct.candidate.dst_window_xid = target_xid;
    direct.candidate.paint_dst_host_xid = target_xid;
    direct.candidate.completion_dst_host_xid = target_xid;
    direct.event.dst_host_xid = target_xid;
    direct
}

fn a3_present_root_copy(
    fixture: &mut super::commit_identity::recording_scene::RecordingSceneFixture,
    direct: &super::commit_identity::recording_scene::DirectPresentSetup,
    color: u32,
) {
    use yserver_core::backend::Backend;

    let mut candidate = direct.candidate;
    candidate.valid_region_xid = 0xdead;
    Backend::fill_rectangle(
        &mut fixture.events.backend,
        None,
        candidate.src_host_xid,
        color,
        0,
        0,
        candidate.src_width,
        candidate.src_height,
    )
    .expect("paint the authoritative Present pixmap");
    assert!(
        !Backend::try_present_direct(&mut fixture.events.backend, candidate, direct.event.clone(),)
            .expect("ineligible authoritative Present through Backend"),
        "a nonzero valid-region id keeps the root-sized Present on Copy"
    );
    Backend::copy_area(
        &mut fixture.events.backend,
        None,
        candidate.src_host_xid,
        candidate.paint_dst_host_xid,
        0,
        0,
        0,
        0,
        candidate.src_width,
        candidate.src_height,
    )
    .expect("run the Present Copy fallback");
    Backend::note_present_pixmap(
        &mut fixture.events.backend,
        candidate.src_pixmap_xid,
        candidate.paint_dst_host_xid,
    );
}

fn a3_visible_root_pixel(
    fixture: &mut super::commit_identity::recording_scene::RecordingSceneFixture,
    x: i16,
    y: i16,
) -> Vec<u8> {
    crate::kms::render::backend::read_scanout_region(
        &mut fixture.events.backend,
        ash::vk::Rect2D {
            offset: ash::vk::Offset2D {
                x: i32::from(x),
                y: i32::from(y),
            },
            extent: ash::vk::Extent2D {
                width: 1,
                height: 1,
            },
        },
        crate::kms::render::backend::ScanoutReadSelection::PermissiveDump,
    )
    .expect("read the visible scanout buffer")
}

fn a3_offer_output_candidate_after_probation(
    fixture: &mut super::commit_identity::recording_scene::RecordingSceneFixtureGuard,
    direct: &super::commit_identity::recording_scene::DirectPresentSetup,
) -> bool {
    use yserver_core::backend::Backend;

    Backend::note_present_scanout_candidate(&mut fixture.events.backend, direct.candidate);
    let probation = usize::from(crate::kms::render::backend::SCANOUT_M2_ELIGIBLE_ROOT_PROBATION);
    let mut accepted = false;
    for attempt in 0..probation {
        accepted = Backend::try_present_direct(
            &mut fixture.events.backend,
            direct.candidate,
            direct.event.clone(),
        )
        .expect("Output Present enters through Backend");
        if accepted {
            assert_eq!(attempt + 1, probation, "Output enters after probation");
        }
    }
    accepted
}

fn a3_submit_gamma_commit(
    fixture: &mut super::commit_identity::recording_scene::RecordingSceneFixtureGuard,
    output_idx: usize,
) -> crate::drm::atomic::tests_support::AtomicCommitRecord {
    use std::os::fd::{AsFd, AsRawFd};
    use yserver_core::backend::Backend;

    fn handle<H: ::drm::control::ResourceHandle>(value: u32) -> H {
        ::drm::control::from_u32(value).expect("nonzero gamma property handle")
    }

    let output = &mut fixture.events.backend.platform.outputs[output_idx].output;
    output.crtc_gamma_lut_prop = Some(handle(70));
    output.crtc_degamma_lut_prop = Some(handle(71));
    output.crtc_ctm_prop = Some(handle(72));
    if output.plane_in_fence_fd_prop.is_none() {
        output.plane_in_fence_fd_prop = Some(handle(73));
    }
    if output.crtc_out_fence_ptr_prop.is_none() {
        output.crtc_out_fence_ptr_prop = Some(handle(74));
    }
    let crtc_id = u32::from(output.crtc);
    let crtc_xid = crtc_id;
    let output_key = fixture.events.backend.platform.outputs[output_idx]
        .key
        .clone();
    fixture
        .events
        .backend
        .pstate
        .crtc_key_by_id
        .insert(crtc_xid, output_key);
    let fd = fixture.events.backend.platform.devices[0]
        .device
        .as_fd()
        .as_raw_fd();
    let recorder = fixture.events.recorders[&fd].clone();
    recorder.set_gamma_size_for_tests(4);
    let before = recorder.records().len();
    Backend::set_crtc_gamma(
        &mut fixture.events.backend,
        crtc_xid,
        &[0x1111; 4],
        &[0x2222; 4],
        &[0x3333; 4],
    )
    .expect("set output gamma through Backend");
    fixture.events.backend.platform.outputs[output_idx]
        .output
        .pending_gamma_deadline
        .set(Some(std::time::Instant::now()));
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("scheduler submits the deferred gamma property commit");
    let record = recorder
        .records()
        .into_iter()
        .nth(before)
        .unwrap_or_else(|| {
            panic!("Backend gamma update submitted a property commit on output {output_idx}")
        });
    assert_eq!(
        record.kind,
        crate::drm::atomic::AtomicCommitKind::GammaOnly,
        "gamma update owns its output CRTC until retirement"
    );
    assert!(matches!(
        record.result,
        crate::drm::atomic::tests_support::ScriptedResult::Ok
    ));
    let scene_records: Vec<_> = a3_records_since(fixture, fd, before)
        .into_iter()
        .filter(|record| {
            matches!(
                record.kind,
                crate::drm::atomic::AtomicCommitKind::Scene
                    | crate::drm::atomic::AtomicCommitKind::SceneWithCursor
            )
        })
        .collect();
    // A property-only Backend tick may also submit an unrelated output's
    // already-dirty scene. Retire that frame so this helper leaves only the
    // requested CRTC commit in flight.
    a3_retire_records(fixture, &scene_records);
    record
}

fn a3_submit_composed_scene_commit(
    fixture: &mut super::commit_identity::recording_scene::RecordingSceneFixtureGuard,
    output_idx: usize,
) -> crate::drm::atomic::tests_support::AtomicCommitRecord {
    use std::os::fd::{AsFd, AsRawFd};
    use yserver_core::backend::Backend;

    let root = fixture.events.backend.core.window_id;
    let output = &fixture.events.backend.platform.outputs[output_idx];
    let crtc = output.output.crtc;
    let (x, y, width, height) = (
        i16::try_from(output.x).expect("test output x fits X coordinates"),
        i16::try_from(output.y).expect("test output y fits X coordinates"),
        output.width,
        output.height,
    );
    let fd = fixture.events.backend.platform.devices[0]
        .device
        .as_fd()
        .as_raw_fd();
    let recorder = fixture.events.recorders[&fd].clone();
    let before = recorder.records().len();
    Backend::clear_area(
        &mut fixture.events.backend,
        None,
        root,
        0,
        None,
        x,
        y,
        width,
        height,
        (0, 0),
    )
    .expect("damage the composed output through Backend");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        Backend::before_block(&mut fixture.events.backend);
        Backend::maybe_composite(&mut fixture.events.backend)
            .expect("schedule composed scene work through Backend");
        fixture.events.wait_for_submitted_gpu_work();
        {
            let events = fixture.events_mut();
            Backend::on_scanout_render_completion(&mut events.backend, &mut events.state);
        }
        if let Some(record) = recorder.records().into_iter().skip(before).find(|record| {
            matches!(
                record.kind,
                crate::drm::atomic::AtomicCommitKind::Scene
                    | crate::drm::atomic::AtomicCommitKind::SceneWithCursor
            ) && record.events.len() == 1
                && record.events[0].crtc == crtc
                && super::commit_identity::accepted_event_id(record).is_some()
        }) {
            assert_eq!(
                fixture
                    .events
                    .backend
                    .scene
                    .pending_page_flip_event_id(output_idx),
                super::commit_identity::accepted_event_id(&record),
                "the composed output owns its accepted scene flip"
            );
            return record;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "a Backend damage pass accepts a composed scene commit on Output({output_idx})"
        );
    }
}

fn a3_offer_direct_present(
    fixture: &mut super::commit_identity::recording_scene::RecordingSceneFixtureGuard,
    candidate: yserver_core::backend::PresentScanoutCandidate,
    event: yserver_core::backend::CompletedPresentEvent,
) -> bool {
    use yserver_core::backend::Backend;

    Backend::note_present_scanout_candidate(&mut fixture.events.backend, candidate);
    Backend::try_present_direct(&mut fixture.events.backend, candidate, event)
        .expect("Output Present enters through Backend")
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: decline instead of queue.
fn unpaced_output_client_enters_direct_on_first_retirement() {
    use std::os::fd::{AsFd, AsRawFd};
    use yserver_core::backend::Backend;

    let Some((fixture, _)) = a3_output_fixture(1, 980) else {
        eprintln!(
            "SKIP unpaced_output_client_enters_direct_on_first_retirement: Vulkan fixture unavailable"
        );
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    for device in &mut fixture.events.backend.platform.devices {
        device.async_page_flip = true;
    }

    let tearing = fixture.install_direct_output_present(
        0,
        981,
        yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
    );
    fixture
        .events
        .backend
        .for_tests_set_scanout_m1_prober(super::present_scanout::record_scanout_m1_probe);
    let entry_records = a3_admit_output_owner(&mut fixture, &tearing, 0);
    a3_retire_records(&mut fixture, &entry_records);
    assert_eq!(
        Backend::drain_completed_present_events(&mut fixture.events.backend)
            .iter()
            .filter(|event| event.present_id == tearing.candidate.present_id)
            .count(),
        1,
        "the entry Present completes once"
    );
    let mut tearing_follow_up = tearing.candidate;
    tearing_follow_up.present_id = 983;
    let mut tearing_follow_up_event = tearing.event.clone();
    tearing_follow_up_event.present_id = tearing_follow_up.present_id;
    tearing_follow_up_event.serial = 983;
    let fd = fixture.events.backend.platform.devices[0]
        .device
        .as_fd()
        .as_raw_fd();
    let before_follow_up = fixture.events.recorders[&fd].records().len();
    Backend::note_present_scanout_candidate(&mut fixture.events.backend, tearing_follow_up);
    assert!(
        Backend::try_present_direct(
            &mut fixture.events.backend,
            tearing_follow_up,
            tearing_follow_up_event.clone(),
        )
        .expect("follow-up async Present uses the active tearing owner")
    );
    let tearing_records = a3_records_since(&fixture, fd, before_follow_up);
    assert!(
        tearing_records
            .iter()
            .any(|record| record.kind == crate::drm::atomic::AtomicCommitKind::Async),
        "the other output is already the tearing owner"
    );
    a3_retire_records(&mut fixture, &tearing_records);
    assert_eq!(
        Backend::drain_completed_present_events(&mut fixture.events.backend)
            .iter()
            .filter(|event| event.present_id == tearing_follow_up.present_id)
            .count(),
        1,
        "the tearing owner completes once"
    );
    assert_eq!(
        fixture.events.backend.tearing_scope(),
        Some(crate::kms::render::backend::DirectScope::Output(0)),
        "Output(0) retains tearing while Output(1) enters"
    );

    let direct = fixture.install_direct_output_present(
        1,
        982,
        yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
    );
    let scene_record = a3_submit_composed_scene_commit(&mut fixture, 1);
    assert!(fixture.events.backend.scene.has_pending_page_flip(1));
    let fd = fixture.events.backend.platform.devices[0]
        .device
        .as_fd()
        .as_raw_fd();
    let record_offset = fixture.events.recorders[&fd].records().len();
    assert!(
        a3_offer_output_candidate_after_probation(&mut fixture, &direct),
        "the post-probation Output Present returns accepted direct while its CRTC is busy"
    );
    let queued = fixture.events.backend.pstate.direct.outputs[1]
        .queued_successor
        .as_ref()
        .expect("busy first entry occupies the existing successor slot");
    assert_eq!(queued.candidate.present_id, direct.candidate.present_id);
    assert!(
        fixture.events.backend.pstate.direct.outputs[1]
            .pending
            .is_none()
    );
    assert!(
        fixture.events.backend.pstate.direct.outputs[1]
            .current
            .is_none()
    );
    assert!(
        !fixture.events.backend.output_is_direct(1),
        "a queued first entry does not suppress Output(1) composition"
    );
    assert!(
        fixture
            .events
            .backend
            .platform
            .scanout_commit_in_flight_for_output(1),
        "the composed scene commit is the busy CRTC gate"
    );
    assert!(
        a3_records_since(&fixture, fd, record_offset)
            .iter()
            .all(|record| super::commit_identity::accepted_event_id(record).is_none()),
        "a queued first entry waits for the commit that already owns its CRTC"
    );

    a3_retire_records(&mut fixture, std::slice::from_ref(&scene_record));
    let after_retirement = a3_tick(&mut fixture);
    let owner = &fixture.events.backend.pstate.direct.outputs[1];
    let pending = owner
        .pending
        .as_ref()
        .expect("queued frame has a direct commit");
    assert_eq!(pending.candidate.present_id, direct.candidate.present_id);
    let pending_id = pending
        .accepted_event_id
        .expect("queued frame has an accepted commit identity");
    let output1_crtc = fixture.events.backend.platform.outputs[1].output.crtc;
    let output1_commits: Vec<_> = after_retirement
        .iter()
        .filter(|record| {
            record.events.len() == 1
                && record.events[0].crtc == output1_crtc
                && super::commit_identity::accepted_event_id(record).is_some()
        })
        .collect();
    assert_eq!(
        output1_commits.len(),
        1,
        "the queued frame submits on the first available CRTC retirement without an intervening compose"
    );
    assert_eq!(
        super::commit_identity::accepted_event_id(output1_commits[0]),
        Some(pending_id),
        "Output(1)'s only accepted commit is the queued direct frame"
    );
    assert_ne!(
        output1_commits[0].kind,
        crate::drm::atomic::AtomicCommitKind::Async,
        "the async requester enters synchronously while Output(0) owns tearing"
    );
    fixture.events.wait_for_submitted_gpu_work();
    fixture.finish();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: dispatch on the CRTC gate only.
fn queued_entry_waits_for_scene_pending_on_its_output() {
    use yserver_core::backend::Backend;

    let fixture = super::commit_identity::recording_scene::RecordingSceneFixture::with_copied_output_geometry(
        (3840, 1080),
        &[(0, 0, 1920, 1080, 75)],
    )
    .expect("cropped one-output copied recording fixture");
    let Some(fixture) = fixture else {
        eprintln!(
            "SKIP queued_entry_waits_for_scene_pending_on_its_output: Vulkan fixture unavailable"
        );
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    fixture
        .readiness
        .as_ref()
        .expect("copied fixture has a readiness observer")
        .hold_ready_jobs();

    let root = fixture.events.backend.core.window_id;
    let (width, height) = (
        fixture.events.backend.platform.fb_w,
        fixture.events.backend.platform.fb_h,
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
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
        .expect("damage the copied scene through Backend");
        Backend::maybe_composite(&mut fixture.events.backend)
            .expect("schedule copied scene work through Backend");
        if !fixture.pending_render_job_ids().is_empty()
            && fixture.events.backend.scene.has_pending_page_flip(0)
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "a damaged Backend tick registers copied render work"
        );
    }
    assert!(
        !fixture.pending_render_job_ids().is_empty(),
        "the copied renderer has scene work awaiting its real completion"
    );
    assert!(fixture.events.backend.scene.has_pending_page_flip(0));
    let direct = fixture.install_direct_output_present(0, 984, 0);
    fixture
        .events
        .backend
        .for_tests_set_scanout_m1_prober(super::present_scanout::record_scanout_m1_probe);
    assert_eq!(
        fixture.events.backend.scene.pending_page_flip_event_id(0),
        None,
        "Output(0) has scene work but no accepted KMS commit"
    );
    assert!(
        !fixture
            .events
            .backend
            .platform
            .scanout_commit_in_flight_for_output(0),
        "the CRTC commit gate is free while copied scene rendering is pending"
    );
    let fd = fixture.kms_fd(0);
    let before = fixture.events.recorders[&fd].records().len();
    assert!(
        a3_offer_output_candidate_after_probation(&mut fixture, &direct),
        "post-probation Output Present queues behind scene work"
    );
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("retry the queued entry during a Backend scheduling pass");
    let owner = &fixture.events.backend.pstate.direct.outputs[0];
    assert_eq!(
        owner
            .queued_successor
            .as_ref()
            .map(|frame| frame.candidate.present_id),
        Some(direct.candidate.present_id),
        "scene-pending work keeps the first-entry successor queued"
    );
    assert!(owner.pending.is_none() && owner.current.is_none());
    assert!(
        a3_records_since(&fixture, fd, before)
            .iter()
            .all(|record| super::commit_identity::accepted_event_id(record).is_none()),
        "no direct commit is accepted while Output(0)'s scene frame awaits render completion"
    );

    for job_id in fixture.pending_render_job_ids() {
        fixture
            .readiness
            .as_ref()
            .expect("copied fixture has a readiness gate")
            .release_ready_job(job_id);
    }
    {
        let events = fixture.events_mut();
        Backend::on_scanout_render_completion(&mut events.backend, &mut events.state);
    }
    let scene_record = a3_records_since(&fixture, fd, before)
        .into_iter()
        .find(|record| {
            record.events.len() == 1
                && record.events[0].crtc == fixture.events.backend.platform.outputs[0].output.crtc
                && super::commit_identity::accepted_event_id(record).is_some()
                && record.kind != crate::drm::atomic::AtomicCommitKind::GammaOnly
        })
        .expect("the copied scene output is accepted after render completion");
    assert_eq!(
        fixture.events.backend.scene.pending_page_flip_event_id(0),
        super::commit_identity::accepted_event_id(&scene_record),
        "the scene owns Output(0)'s accepted flip until retirement"
    );
    a3_retire_records(&mut fixture, std::slice::from_ref(&scene_record));
    let dispatch = a3_tick(&mut fixture);
    let owner = &fixture.events.backend.pstate.direct.outputs[0];
    let pending = owner
        .pending
        .as_ref()
        .expect("scene retirement dispatches direct");
    assert_eq!(pending.candidate.present_id, direct.candidate.present_id);
    let pending_id = pending
        .accepted_event_id
        .expect("queued direct frame accepted after scene retirement");
    assert!(dispatch.iter().any(|record| {
        super::commit_identity::accepted_event_id(record) == Some(pending_id)
            && record.events.len() == 1
            && record.events[0].crtc == fixture.events.backend.platform.outputs[0].output.crtc
    }));
    fixture.events.wait_for_submitted_gpu_work();
    fixture.finish();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: dispatch on the scene-pending state only, ignoring the CRTC commit gate.
fn queued_entry_waits_for_cursor_commit_on_its_crtc() {
    use yserver_core::backend::Backend;

    let Some((fixture, direct)) = a3_output_fixture(1, 985) else {
        eprintln!(
            "SKIP queued_entry_waits_for_cursor_commit_on_its_crtc: Vulkan fixture unavailable"
        );
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    fixture
        .events
        .backend
        .for_tests_set_scanout_m1_prober(super::present_scanout::record_scanout_m1_probe);
    let gamma_record = a3_submit_gamma_commit(&mut fixture, 1);
    assert!(!fixture.events.backend.scene.has_pending_page_flip(1));
    assert!(
        fixture
            .events
            .backend
            .platform
            .scanout_commit_in_flight_for_output(1),
        "the cursor/property commit gate is independently busy"
    );
    let fd = fixture.kms_fd(1);
    let before = fixture.events.recorders[&fd].records().len();
    assert!(
        a3_offer_output_candidate_after_probation(&mut fixture, &direct),
        "post-probation Output Present queues behind a cursor/property commit"
    );
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("retry the queued entry during a Backend scheduling pass");
    let owner = &fixture.events.backend.pstate.direct.outputs[1];
    assert_eq!(
        owner
            .queued_successor
            .as_ref()
            .map(|frame| frame.candidate.present_id),
        Some(direct.candidate.present_id),
        "a cursor/property commit keeps the queued first entry in its slot"
    );
    assert!(owner.pending.is_none() && owner.current.is_none());
    assert!(
        a3_records_since(&fixture, fd, before)
            .iter()
            .all(|record| super::commit_identity::accepted_event_id(record).is_none()),
        "the scheduler submits no direct frame while the output CRTC is busy"
    );

    a3_retire_records(&mut fixture, std::slice::from_ref(&gamma_record));
    let dispatched = a3_tick(&mut fixture);
    let pending = fixture.events.backend.pstate.direct.outputs[1]
        .pending
        .as_ref()
        .expect("queued entry submits after its CRTC commit retires");
    let pending_id = pending
        .accepted_event_id
        .expect("queued direct frame has an accepted commit identity");
    assert!(dispatched.iter().any(|record| {
        super::commit_identity::accepted_event_id(record) == Some(pending_id)
            && record.events.len() == 1
            && record.events[0].crtc == fixture.events.backend.platform.outputs[1].output.crtc
    }));
    fixture.events.wait_for_submitted_gpu_work();
    fixture.finish();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: submit the stale queued frame.
fn newer_present_supersedes_queued_entry() {
    use yserver_core::backend::Backend;

    let Some((fixture, direct)) = a3_output_fixture(1, 986) else {
        eprintln!("SKIP newer_present_supersedes_queued_entry: Vulkan fixture unavailable");
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    fixture
        .events
        .backend
        .for_tests_set_scanout_m1_prober(super::present_scanout::record_scanout_m1_probe);
    let scene_record = a3_submit_composed_scene_commit(&mut fixture, 1);
    assert!(fixture.events.backend.scene.has_pending_page_flip(1));
    assert!(
        a3_offer_output_candidate_after_probation(&mut fixture, &direct),
        "the first Present queues behind the composed scene commit"
    );
    assert_eq!(
        fixture.events.backend.pstate.direct.outputs[1]
            .queued_successor
            .as_ref()
            .map(|frame| frame.candidate.present_id),
        Some(direct.candidate.present_id),
        "the first Present occupies the successor slot before scene retirement"
    );

    a3_retire_records(&mut fixture, std::slice::from_ref(&scene_record));
    assert_eq!(
        fixture.events.backend.pstate.direct.outputs[1]
            .queued_successor
            .as_ref()
            .map(|frame| frame.candidate.present_id),
        Some(direct.candidate.present_id),
        "scene retirement leaves the queued frame for the next scheduling pass"
    );

    let mut newer = direct.candidate;
    newer.present_id += 1;
    let mut newer_event = direct.event.clone();
    newer_event.present_id = newer.present_id;
    newer_event.serial = u32::try_from(newer.present_id).expect("test Present id fits serial");
    newer_event.msc_offset = 123;
    assert!(
        a3_offer_direct_present(&mut fixture, newer, newer_event),
        "the newer Present replaces the queued first entry without an MSC gate"
    );
    let owner = &fixture.events.backend.pstate.direct.outputs[1];
    assert_eq!(
        owner
            .queued_successor
            .as_ref()
            .map(|frame| frame.candidate.present_id),
        Some(newer.present_id),
        "latest wins in the existing successor slot"
    );
    let replaced = Backend::drain_completed_present_events(&mut fixture.events.backend);
    let skipped: Vec<_> = replaced
        .iter()
        .filter(|event| {
            event.present_id == direct.candidate.present_id
                && event.completion_mode == yserver_protocol::x11::present::COMPLETE_MODE_SKIP
        })
        .collect();
    assert_eq!(
        skipped.len(),
        1,
        "replaced queued Present completes once as Skip"
    );
    assert!(
        fixture.events.backend.pstate.direct.outputs[1]
            .pending
            .is_none(),
        "the newer Present replaces the queued frame instead of bypassing it"
    );

    let dispatched = a3_tick(&mut fixture);
    let pending = fixture.events.backend.pstate.direct.outputs[1]
        .pending
        .as_ref()
        .expect("latest queued Present submits when the CRTC retires");
    assert_eq!(pending.candidate.present_id, newer.present_id);
    let pending_id = pending
        .accepted_event_id
        .expect("latest queued Present has an accepted commit identity");
    assert!(
        dispatched.iter().any(|record| {
            super::commit_identity::accepted_event_id(record) == Some(pending_id)
        })
    );
    let direct_records = dispatched
        .iter()
        .filter(|record| super::commit_identity::accepted_event_id(record) == Some(pending_id))
        .count();
    assert_eq!(direct_records, 1, "the newest frame is submitted once");
    a3_retire_records(&mut fixture, &dispatched);
    let completed = Backend::drain_completed_present_events(&mut fixture.events.backend);
    assert_eq!(
        completed
            .iter()
            .filter(|event| event.present_id == newer.present_id)
            .count(),
        1,
        "the submitted latest Present completes once"
    );
    assert!(!completed.iter().any(|event| {
        event.present_id == direct.candidate.present_id
            && event.completion_mode != yserver_protocol::x11::present::COMPLETE_MODE_SKIP
    }));
    fixture.finish();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: skip revalidation of a queued first-entry frame.
fn queued_entry_revalidates_before_submission() {
    use yserver_core::{backend::Backend, server::ServerState};
    use yserver_protocol::x11::ResourceId;

    let Some((fixture, direct)) = a3_output_fixture(1, 987) else {
        eprintln!("SKIP queued_entry_revalidates_before_submission: Vulkan fixture unavailable");
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    fixture
        .events
        .backend
        .for_tests_set_scanout_m1_prober(super::present_scanout::record_scanout_m1_probe);
    Backend::fill_rectangle(
        &mut fixture.events.backend,
        None,
        direct.candidate.src_host_xid,
        0x0012_3456,
        0,
        0,
        direct.candidate.src_width,
        direct.candidate.src_height,
    )
    .expect("paint queued Present source through Backend");
    let gamma_record = a3_submit_gamma_commit(&mut fixture, 1);
    let fd = fixture.kms_fd(1);
    let before = fixture.events.recorders[&fd].records().len();
    assert!(
        a3_offer_output_candidate_after_probation(&mut fixture, &direct),
        "the busy first entry is accepted into the successor slot"
    );
    let queued = fixture.events.backend.pstate.direct.outputs[1]
        .queued_successor
        .as_ref()
        .expect("first entry is queued behind the gamma commit");
    let source_pin = queued.source_pin;
    let target_pin = queued.fallback_target_pin;
    assert!(
        fixture
            .events
            .backend
            .present_source_pins
            .contains_key(&source_pin)
    );
    assert!(
        fixture
            .events
            .backend
            .present_source_pins
            .contains_key(&target_pin)
    );

    let mut state = ServerState::new();
    let overlay = a3_seed_ordered_window(
        &mut fixture,
        &mut state,
        ResourceId(0x3191),
        1920,
        0,
        1920,
        1080,
        false,
    );
    Backend::fill_rectangle(
        &mut fixture.events.backend,
        None,
        overlay,
        0x0001_2345,
        0,
        0,
        1920,
        1080,
    )
    .expect("paint an overlay that invalidates direct eligibility");
    Backend::map_subwindow(&mut fixture.events.backend, None, overlay)
        .expect("map overlay through Backend while the Present is queued");

    a3_retire_records(&mut fixture, std::slice::from_ref(&gamma_record));
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("revalidate the queued Present during the Backend scheduling pass");
    let owner = &fixture.events.backend.pstate.direct.outputs[1];
    assert!(
        owner.current.is_none() && owner.pending.is_none() && owner.queued_successor.is_none(),
        "the mapped overlay sends the queued first entry to Copy and clears direct ownership"
    );
    assert!(!fixture.events.backend.output_is_direct(1));
    let composed = a3_records_since(&fixture, fd, before);
    assert!(
        composed.iter().all(|record| {
            super::commit_identity::accepted_event_id(record).is_none()
                || record.events[0].crtc != fixture.events.backend.platform.outputs[1].output.crtc
                || a3_has_scene_commit_for(
                    std::slice::from_ref(record),
                    fixture.events.backend.platform.outputs[1].output.crtc,
                )
        }),
        "Output(1) accepts only its composed replacement, never a stale direct frame"
    );
    assert!(
        fixture
            .events
            .backend
            .present_source_pins
            .contains_key(&source_pin)
    );
    assert!(
        fixture
            .events
            .backend
            .present_source_pins
            .contains_key(&target_pin)
    );

    a3_retire_records(&mut fixture, &composed);
    assert!(
        !fixture.events.backend.scene.has_pending_page_flips(),
        "composed replacement retirement clears scene acknowledgements"
    );

    fixture.events.wait_for_submitted_gpu_work();
    let copied = fixture
        .events
        .backend
        .backing_pixels_for_tests(direct.target_xid)
        .expect("fallback target remains readable")
        .2;
    assert_eq!(
        &copied[..3],
        &[0x56, 0x34, 0x12],
        "Copy fallback writes Present pixels"
    );
    let completed = Backend::drain_completed_present_events(&mut fixture.events.backend);
    let copies: Vec<_> = completed
        .iter()
        .filter(|event| {
            event.present_id == direct.candidate.present_id
                && event.completion_mode == yserver_protocol::x11::present::COMPLETE_MODE_COPY
        })
        .collect();
    assert_eq!(copies.len(), 1, "the fallback Copy completes exactly once");
    assert!(
        !fixture
            .events
            .backend
            .present_source_pins
            .contains_key(&source_pin)
    );
    assert!(
        !fixture
            .events
            .backend
            .present_source_pins
            .contains_key(&target_pin)
    );
    assert!(
        Backend::drain_completed_present_events(&mut fixture.events.backend)
            .iter()
            .all(|event| event.present_id != direct.candidate.present_id),
        "Copy fallback does not emit a second completion"
    );
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: leave the owner set after failure.
fn queued_entry_submit_failure_falls_back_to_copy() {
    use std::os::fd::{AsFd, AsRawFd};
    use yserver_core::backend::Backend;

    let Some((fixture, direct)) = a3_output_fixture(1, 988) else {
        eprintln!(
            "SKIP queued_entry_submit_failure_falls_back_to_copy: Vulkan fixture unavailable"
        );
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    fixture
        .events
        .backend
        .for_tests_set_scanout_m1_prober(super::present_scanout::record_scanout_m1_probe);
    let gamma_record = a3_submit_gamma_commit(&mut fixture, 1);
    let fd = fixture.events.backend.platform.devices[0]
        .device
        .as_fd()
        .as_raw_fd();
    let before = fixture.events.recorders[&fd].records().len();
    assert!(
        a3_offer_output_candidate_after_probation(&mut fixture, &direct),
        "the busy first entry is accepted into the successor slot"
    );
    let queued = fixture.events.backend.pstate.direct.outputs[1]
        .queued_successor
        .as_ref()
        .expect("first entry is queued behind the gamma commit");
    let source_pin = queued.source_pin;
    let target_pin = queued.fallback_target_pin;
    a3_retire_records(&mut fixture, std::slice::from_ref(&gamma_record));
    fixture.events.recorders[&fd]
        .script_result(Err(std::io::Error::from_raw_os_error(libc::EBUSY)));
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("queued direct submit failure falls back during a Backend pass");
    let output1_crtc = fixture.events.backend.platform.outputs[1].output.crtc;
    let records = a3_records_since(&fixture, fd, before);
    assert!(
        records.iter().any(|record| {
            record.events.len() == 1
                && record.events[0].crtc == output1_crtc
                && matches!(
                    record.result,
                    crate::drm::atomic::tests_support::ScriptedResult::Error {
                        raw_os_error: Some(error),
                        ..
                    } if error == libc::EBUSY
                )
        }),
        "the recording KMS gate rejects the queued direct commit with EBUSY"
    );
    let owner = &fixture.events.backend.pstate.direct.outputs[1];
    assert!(
        owner.current.is_none()
            && owner.pending.is_none()
            && owner.queued_successor.is_none()
            && !owner.hold_direct
            && !owner.active(),
        "submit failure clears the queued-only direct owner"
    );
    assert!(!fixture.events.backend.output_is_direct(1));
    assert!(a3_has_scene_commit_for(&records, output1_crtc));
    assert!(
        fixture
            .events
            .backend
            .present_source_pins
            .contains_key(&source_pin)
    );
    assert!(
        fixture
            .events
            .backend
            .present_source_pins
            .contains_key(&target_pin)
    );

    a3_retire_records(&mut fixture, &records);
    assert!(
        !fixture.events.backend.scene.has_pending_page_flips(),
        "composed replacement retirement clears scene acknowledgements"
    );

    fixture.events.wait_for_submitted_gpu_work();
    let completed = Backend::drain_completed_present_events(&mut fixture.events.backend);
    let copies: Vec<_> = completed
        .iter()
        .filter(|event| {
            event.present_id == direct.candidate.present_id
                && event.completion_mode == yserver_protocol::x11::present::COMPLETE_MODE_COPY
        })
        .collect();
    assert_eq!(
        copies.len(),
        1,
        "submit-failure Copy completes exactly once"
    );
    assert!(
        !fixture
            .events
            .backend
            .present_source_pins
            .contains_key(&source_pin)
    );
    assert!(
        !fixture
            .events
            .backend
            .present_source_pins
            .contains_key(&target_pin)
    );
    assert!(
        Backend::drain_completed_present_events(&mut fixture.events.backend)
            .iter()
            .all(|event| event.present_id != direct.candidate.present_id),
        "submit-failure fallback does not emit a second completion"
    );
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: A01 makes every authoritative Present request a root-wide unflip;
// A02 treats every shaped COW as hidden from every output.
fn cow_present_visibility() {
    use yserver_core::backend::Backend;
    use yserver_protocol::x11::{ResourceId, xfixes::RegionRect};

    for (row, present_id, shape_width, expected_unflip) in [
        ("shaped-away", 940, 1920, false),
        ("intersecting", 943, 3840, true),
    ] {
        let Some((fixture, direct)) = a3_output_fixture(1, present_id) else {
            eprintln!("SKIP cow_present_visibility: Vulkan fixture unavailable");
            return;
        };
        let mut fixture = fixture.cleanup_guard();
        let mut state = yserver_core::server::ServerState::new();
        let game_xid = direct.target_xid;
        let (game_x, game_y, game_width, game_height) = {
            let output = &fixture.events.backend.platform.outputs[1];
            (
                output.x as i16,
                output.y as i16,
                output.width,
                output.height,
            )
        };
        a3_add_existing_window_to_state(
            &mut state,
            ResourceId(0x3160),
            game_xid,
            game_x,
            game_y,
            game_width,
            game_height,
            true,
        );
        Backend::sync_top_level_order(&mut fixture.events.backend, &state);
        a3_enter_output_owner(&mut fixture, &direct, 1);
        let current_present = fixture.events.backend.pstate.direct.outputs[1]
            .current
            .as_ref()
            .expect("Output(1) is held")
            .candidate
            .present_id;

        let cow_xid = a3_materialize_cow(&mut fixture, &mut state);
        Backend::set_shape_rectangles(
            &mut fixture.events.backend,
            None,
            cow_xid,
            0,
            Some(&[RegionRect {
                x: 0,
                y: 0,
                width: shape_width,
                height: 1080,
            }]),
        )
        .expect("shape the COW through Backend");
        let stage = a3_install_root_present(&mut fixture, present_id + 1, cow_xid);
        a3_present_root_copy(&mut fixture, &stage, 0x00d1_7b31);
        assert_eq!(
            fixture.events.backend.pstate.direct.outputs[1]
                .current
                .as_ref()
                .map(|frame| frame.candidate.present_id),
            Some(current_present),
            "{row}: the held direct owner remains until a replacement retires"
        );

        let owner = &fixture.events.backend.pstate.direct.outputs[1];
        assert_eq!(
            owner.unflip_requested, expected_unflip,
            "{row}: COW visibility controls only the intersecting output"
        );
        if expected_unflip {
            assert_eq!(
                owner.unflip_reason,
                Some("ineligible_authoritative_root_present"),
                "{row}: selected output records the authoritative Present reason"
            );
            assert_eq!(
                owner.unflip_fallback_source, None,
                "{row}: Output scope does not retain a fallback source"
            );
            assert!(
                owner.unflip_shadow_ready,
                "{row}: Copy prepares the replacement"
            );
            assert!(
                owner.current.is_some(),
                "{row}: direct frame awaits replacement"
            );
        }

        let records = a3_tick(&mut fixture);
        let output0_crtc = fixture.events.backend.platform.outputs[0].output.crtc;
        let output1_crtc = fixture.events.backend.platform.outputs[1].output.crtc;
        assert_eq!(
            [
                a3_has_scene_commit_for(&records, output0_crtc),
                a3_has_scene_commit_for(&records, output1_crtc),
            ],
            if expected_unflip {
                [true, true]
            } else {
                [true, false]
            },
            "{row}: only outputs selected by the shaped COW submit scene commits"
        );
        if expected_unflip {
            let replacement = records.iter().find(|record| {
                super::commit_identity::accepted_event_id(record).is_some()
                    && a3_has_scene_commit_for(std::slice::from_ref(*record), output1_crtc)
            });
            assert!(
                replacement.is_some(),
                "{row}: output 1 replacement is accepted"
            );
            a3_retire_records(&mut fixture, &records);
            assert!(
                fixture.events.backend.pstate.direct.outputs[1]
                    .current
                    .is_none(),
                "{row}: output 1 owner releases after its replacement retires"
            );
        } else {
            a3_retire_records(&mut fixture, &records);
            assert_eq!(
                &a3_visible_root_pixel(&mut fixture, 150, 150)[..3],
                &[0x31, 0x7b, 0xd1],
                "{row}: output 0 displays the copied stage pixels"
            );
            let owner = &fixture.events.backend.pstate.direct.outputs[1];
            assert!(!owner.unflip_requested, "{row}: output 1 remains direct");
            assert_eq!(
                owner.current.as_ref().unwrap().candidate.present_id,
                current_present,
                "{row}: output 1 stays current through output 0 retirement"
            );
        }
        drop(fixture);
    }
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: treat every root-covering Unredirected Present as visible on every output.
fn root_window_below_game_presenting_keeps_output_owner() {
    use yserver_core::backend::Backend;
    use yserver_protocol::x11::ResourceId;

    let Some((mut fixture, direct)) = a3_output_fixture(1, 944) else {
        eprintln!(
            "SKIP root_window_below_game_presenting_keeps_output_owner: Vulkan fixture unavailable"
        );
        return;
    };
    let mut state = yserver_core::server::ServerState::new();
    let root_width = fixture.events.backend.platform.fb_w;
    let root_height = fixture.events.backend.platform.fb_h;
    let stage_resource = ResourceId(0x3162);
    let stage_xid = a3_seed_ordered_window(
        &mut fixture,
        &mut state,
        stage_resource,
        0,
        0,
        root_width,
        root_height,
        true,
    );
    let game_resource = ResourceId(0x3163);
    let game = &fixture.events.backend.platform.outputs[1];
    a3_add_existing_window_to_state(
        &mut state,
        game_resource,
        direct.target_xid,
        game.x as i16,
        game.y as i16,
        game.width,
        game.height,
        true,
    );
    Backend::sync_top_level_order(&mut fixture.events.backend, &state);
    a3_enter_output_owner(&mut fixture, &direct, 1);

    let root_present = fixture.install_direct_present(945, 0);
    fixture
        .events
        .backend
        .core
        .top_level_order
        .retain(|&xid| xid != root_present.target_xid);
    let mut candidate = root_present.candidate;
    candidate.dst_window_xid = stage_xid;
    candidate.paint_dst_host_xid = stage_xid;
    candidate.completion_dst_host_xid = stage_xid;
    candidate.valid_region_xid = 0xdead;
    let mut event = root_present.event.clone();
    event.dst_host_xid = stage_xid;

    Backend::fill_rectangle(
        &mut fixture.events.backend,
        None,
        candidate.src_host_xid,
        0x00d1_7b31,
        0,
        0,
        candidate.src_width,
        candidate.src_height,
    )
    .expect("paint the below-game root Present");
    assert!(
        !Backend::try_present_direct(&mut fixture.events.backend, candidate, event.clone())
            .expect("below-game root Present remains a Copy"),
        "the valid-region gate serves this Present by Copy"
    );
    assert!(
        !fixture.events.backend.pstate.direct.outputs[1].unflip_requested,
        "the root window covers output 0 only in the current stacking order"
    );
    Backend::copy_area(
        &mut fixture.events.backend,
        None,
        candidate.src_host_xid,
        candidate.paint_dst_host_xid,
        0,
        0,
        0,
        0,
        candidate.src_width,
        candidate.src_height,
    )
    .expect("copy the below-game Present");
    Backend::note_present_pixmap(
        &mut fixture.events.backend,
        candidate.src_pixmap_xid,
        candidate.paint_dst_host_xid,
    );
    let records = a3_tick(&mut fixture);
    assert!(
        a3_has_scene_commit_for(
            &records,
            fixture.events.backend.platform.outputs[0].output.crtc
        ),
        "the below-game root window is visible on output 0"
    );
    assert!(!fixture.events.backend.pstate.direct.outputs[1].unflip_requested);
    a3_retire_records(&mut fixture, &records);
    assert_eq!(
        &a3_visible_root_pixel(&mut fixture, 150, 150)[..3],
        &[0x31, 0x7b, 0xd1],
        "output 0 displays the root window below the game"
    );

    a3_restack_top_level(
        &mut fixture,
        &mut state,
        stage_resource,
        game_resource,
        stage_xid,
        direct.target_xid,
        0,
    );
    candidate.present_id = 946;
    event.present_id = 946;
    event.serial = 946;
    assert!(
        !Backend::try_present_direct(&mut fixture.events.backend, candidate, event)
            .expect("raised root Present remains a Copy"),
        "the valid-region gate keeps the raised Present composed"
    );
    let owner = &fixture.events.backend.pstate.direct.outputs[1];
    assert!(
        owner.unflip_requested,
        "the raised root window now covers output 1"
    );
    assert_eq!(
        owner.unflip_reason,
        Some("ineligible_authoritative_root_present")
    );
    fixture.finish();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: leave Copy fallback readiness updates restricted to the Root owner.
fn output_owner_copy_fallback_is_marked_ready() {
    use yserver_core::backend::Backend;
    use yserver_protocol::x11::ResourceId;

    for (present_id, cow_descendant) in [(947, false), (949, true)] {
        let Some((mut fixture, direct)) = a3_output_fixture(1, present_id) else {
            eprintln!(
                "SKIP output_owner_copy_fallback_is_marked_ready: Vulkan fixture unavailable"
            );
            return;
        };
        let mut state = yserver_core::server::ServerState::new();
        let root_width = fixture.events.backend.platform.fb_w;
        let root_height = fixture.events.backend.platform.fb_h;
        let stage_resource = ResourceId(if cow_descendant { 0x3167 } else { 0x3166 });
        let stage_xid = a3_seed_ordered_window(
            &mut fixture,
            &mut state,
            stage_resource,
            0,
            0,
            root_width,
            root_height,
            true,
        );
        let game = &fixture.events.backend.platform.outputs[1];
        a3_add_existing_window_to_state(
            &mut state,
            ResourceId(if cow_descendant { 0x3165 } else { 0x3164 }),
            direct.target_xid,
            game.x as i16,
            game.y as i16,
            game.width,
            game.height,
            true,
        );
        Backend::sync_top_level_order(&mut fixture.events.backend, &state);
        let cow_xid = cow_descendant.then(|| a3_materialize_cow(&mut fixture, &mut state));
        if cow_descendant {
            let cow_xid = cow_xid.unwrap();
            fixture
                .events
                .backend
                .windows
                .get_mut(&direct.target_xid)
                .expect("direct Output target window")
                .parent = Some(cow_xid);
            fixture
                .events
                .backend
                .core
                .top_level_order
                .retain(|&xid| xid != direct.target_xid);
            assert!(
                fixture
                    .events
                    .backend
                    .test_set_redirected_target(cow_xid, cow_xid),
                "the COW descendant resolves into the affected COW backing"
            );
            assert!(
                fixture
                    .events
                    .backend
                    .test_set_redirected_target(stage_xid, cow_xid),
                "the root Present Copy updates the same backing as the direct fallback"
            );
        }
        Backend::fill_rectangle(
            &mut fixture.events.backend,
            None,
            direct.candidate.src_host_xid,
            0x0000_00ff,
            0,
            0,
            direct.candidate.src_width,
            direct.candidate.src_height,
        )
        .expect("paint the retained direct source blue");
        a3_enter_output_owner(&mut fixture, &direct, 1);
        let owner = &fixture.events.backend.pstate.direct.outputs[1];
        if cow_descendant {
            assert_eq!(
                owner.current.as_ref().unwrap().fallback_target.backing_id(),
                fixture.events.backend.cow_id.unwrap(),
                "the COW-descendant fallback shares the root Present backing"
            );
        } else {
            a3_restack_top_level(
                &mut fixture,
                &mut state,
                stage_resource,
                ResourceId(0x3164),
                stage_xid,
                direct.target_xid,
                0,
            );
        }

        let stage = a3_install_root_present(&mut fixture, present_id + 1, stage_xid);
        a3_present_root_copy(&mut fixture, &stage, 0x00d1_7b31);
        let owner = &fixture.events.backend.pstate.direct.outputs[1];
        assert!(
            owner.unflip_requested,
            "visible authoritative Copy revokes Output(1)"
        );
        assert_eq!(owner.unflip_fallback_source, None);
        assert!(
            owner.unflip_shadow_ready,
            "the Copy already prepared the replacement"
        );

        let records = a3_tick(&mut fixture);
        assert!(
            a3_has_scene_commit_for(
                &records,
                fixture.events.backend.platform.outputs[1].output.crtc
            ),
            "output 1 submits its composed replacement"
        );
        a3_retire_records(&mut fixture, &records);
        assert!(
            !fixture.events.backend.pstate.direct.outputs[1].active(),
            "Output(1) retired its composed replacement"
        );
        assert_eq!(
            &a3_visible_root_pixel(&mut fixture, 1930, 20)[..3],
            &[0x31, 0x7b, 0xd1],
            "the replacement shows the new Present pixels, not the retained blue source"
        );
        fixture.finish();
    }
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: A05 revokes every owner on any top-level restack; A06 omits stacking
// from tick revalidation; A10 revalidates before the core tree's stacking reprojection.
fn output_restack_revalidation() {
    use yserver_core::{backend::Backend, host_x11::HostSubwindowConfig, server::ServerState};
    use yserver_protocol::x11::{ConfigureWindowRequest, ResourceId};

    for (row, present_id) in [
        ("restack-elsewhere", 930),
        ("raise-over-owner", 931),
        ("move-and-lower", 934),
    ] {
        let Some((fixture, direct)) = a3_output_fixture(1, present_id) else {
            eprintln!("SKIP output_restack_revalidation: Vulkan fixture unavailable");
            return;
        };
        let mut fixture = fixture.cleanup_guard();
        let mut state = ServerState::new();
        let restack_windows = if row == "restack-elsewhere" {
            let lower = a3_seed_ordered_window(
                &mut fixture,
                &mut state,
                ResourceId(0x3100),
                100,
                100,
                500,
                500,
                true,
            );
            let upper = a3_seed_ordered_window(
                &mut fixture,
                &mut state,
                ResourceId(0x3101),
                100,
                100,
                500,
                500,
                true,
            );
            Backend::fill_rectangle(
                &mut fixture.events.backend,
                None,
                lower,
                0x00ff_0000,
                0,
                0,
                500,
                500,
            )
            .expect("paint the lower output 0 window red");
            Backend::fill_rectangle(
                &mut fixture.events.backend,
                None,
                upper,
                0x0000_00ff,
                0,
                0,
                500,
                500,
            )
            .expect("paint the upper output 0 window blue");
            Some((lower, upper))
        } else {
            None
        };

        let game_rid = ResourceId(0x3111);
        let game_xid = direct.target_xid;
        let mut raised = None;
        let mut mover = None;
        if row == "raise-over-owner" {
            raised = Some(a3_seed_ordered_window(
                &mut fixture,
                &mut state,
                ResourceId(0x3110),
                1920,
                0,
                1920,
                1080,
                true,
            ));
        }
        a3_add_existing_window_to_state(&mut state, game_rid, game_xid, 1920, 0, 1920, 1080, true);
        if row == "move-and-lower" {
            mover = Some(a3_seed_ordered_window(
                &mut fixture,
                &mut state,
                ResourceId(0x3140),
                100,
                0,
                500,
                500,
                true,
            ));
        }
        Backend::sync_top_level_order(&mut fixture.events.backend, &state);
        a3_enter_output_owner(&mut fixture, &direct, 1);
        let owner_present = fixture.events.backend.pstate.direct.outputs[1]
            .current
            .as_ref()
            .expect("Output(1) owner held")
            .candidate
            .present_id;

        match row {
            "restack-elsewhere" => {
                let (lower, upper) = restack_windows.expect("restack row has output 0 windows");
                a3_restack_top_level(
                    &mut fixture,
                    &mut state,
                    ResourceId(0x3100),
                    ResourceId(0x3101),
                    lower,
                    upper,
                    0,
                );
                let records = a3_tick(&mut fixture);
                let owner = &fixture.events.backend.pstate.direct.outputs[1];
                assert!(
                    !owner.unflip_requested,
                    "{row}: output 0 restack leaves Output(1) held"
                );
                assert_eq!(
                    owner.current.as_ref().unwrap().candidate.present_id,
                    owner_present,
                    "{row}: Output(1) retains the same direct Present"
                );
                assert!(
                    a3_has_scene_commit_for(
                        &records,
                        fixture.events.backend.platform.outputs[0].output.crtc
                    ),
                    "{row}: output 0 submits its restacked scene"
                );
                a3_retire_records(&mut fixture, &records);
                let root = fixture.events.backend.core.window_id;
                let visible = fixture
                    .events
                    .backend
                    .get_image_pixels_for_tests(root, 2, 150, 150, 1, 1, !0)
                    .expect("read restacked output 0 pixel")
                    .expect("restacked pixel bytes");
                assert_eq!(
                    &visible[..3],
                    &[0x00, 0x00, 0xff],
                    "{row}: raised lower window is visible in the committed scene"
                );
                assert_eq!(
                    fixture.events.backend.pstate.direct.outputs[1]
                        .current
                        .as_ref()
                        .unwrap()
                        .candidate
                        .present_id,
                    owner_present,
                    "{row}: Output(1) stays current through output 0 retirement"
                );
            }
            "raise-over-owner" => {
                a3_restack_top_level(
                    &mut fixture,
                    &mut state,
                    ResourceId(0x3110),
                    game_rid,
                    raised.expect("raise row has an overlay"),
                    game_xid,
                    0,
                );
                assert!(
                    !fixture.events.backend.pstate.direct.outputs[1].unflip_requested,
                    "{row}: revalidation waits for the scheduler tick"
                );
                let records = a3_tick(&mut fixture);
                let owner = &fixture.events.backend.pstate.direct.outputs[1];
                assert!(
                    owner.unflip_requested,
                    "{row}: intersecting raised window revokes the owner"
                );
                assert_eq!(
                    owner.unflip_reason,
                    Some("composite_tick_output_scene_changed"),
                    "{row}: tick records the scene-change reason"
                );
                assert!(
                    a3_has_scene_commit_for(
                        &records,
                        fixture.events.backend.platform.outputs[1].output.crtc
                    ),
                    "{row}: tick accepts the output 1 replacement"
                );
                a3_retire_records(&mut fixture, &records);
                assert!(
                    fixture.events.backend.pstate.direct.outputs[1]
                        .current
                        .is_none(),
                    "{row}: output 1 owner retires after replacement acknowledgement"
                );
            }
            "move-and-lower" => {
                let mover = mover.expect("combined configure row has a mover");
                let mover_rid = ResourceId(0x3140);
                Backend::configure_subwindow(
                    &mut fixture.events.backend,
                    None,
                    mover,
                    HostSubwindowConfig {
                        x: Some(2000),
                        sibling: Some(game_xid),
                        stack_mode: Some(1),
                        ..Default::default()
                    },
                )
                .expect("combined ConfigureWindow backend callback");
                state.resources.configure_window(ConfigureWindowRequest {
                    window: mover_rid,
                    value_mask: 0,
                    x: Some(2000),
                    y: None,
                    width: None,
                    height: None,
                    border_width: None,
                    sibling: Some(game_rid),
                    stack_mode: Some(1),
                });
                Backend::sync_top_level_order(&mut fixture.events.backend, &state);
                let records = a3_tick(&mut fixture);
                let owner = &fixture.events.backend.pstate.direct.outputs[1];
                assert!(
                    !owner.unflip_requested,
                    "{row}: final core stacking order keeps the game topmost"
                );
                assert_eq!(
                    owner
                        .current
                        .as_ref()
                        .map(|frame| frame.candidate.present_id),
                    Some(owner_present),
                    "{row}: the same direct owner survives configure reprojection"
                );
                assert!(
                    !a3_has_scene_commit_for(
                        &records,
                        fixture.events.backend.platform.outputs[1].output.crtc
                    ),
                    "{row}: no output 1 replacement is submitted for the final eligible scene"
                );
                a3_retire_records(&mut fixture, &records);
            }
            _ => unreachable!("known Q10 row"),
        }
        drop(fixture);
    }
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: A07 removes tick scene revalidation; A09 ignores configured geometry
// during tick revalidation; A13 accepts an Output target classified as Other.
fn paused_output_scene_change() {
    use yserver_core::{backend::Backend, host_x11::HostSubwindowConfig, server::ServerState};
    use yserver_protocol::x11::ResourceId;

    for (row, present_id) in [("map", 932), ("geometry-move", 933), ("redirect", 938)] {
        let Some((fixture, direct)) = a3_output_fixture(1, present_id) else {
            eprintln!("SKIP paused_output_scene_change: Vulkan fixture unavailable");
            return;
        };
        let mut fixture = fixture.cleanup_guard();
        a3_enter_output_owner(&mut fixture, &direct, 1);
        let owner_present = fixture.events.backend.pstate.direct.outputs[1]
            .current
            .as_ref()
            .expect("Output(1) owner held")
            .candidate
            .present_id;
        let mut state = ServerState::new();

        match row {
            "map" => {
                let unrelated = a3_seed_ordered_window(
                    &mut fixture,
                    &mut state,
                    ResourceId(0x3120),
                    0,
                    0,
                    1920,
                    1080,
                    false,
                );
                Backend::map_subwindow(&mut fixture.events.backend, None, unrelated)
                    .expect("map output 0 window through Backend");
                let unrelated_records = a3_tick(&mut fixture);
                assert!(
                    !fixture.events.backend.pstate.direct.outputs[1].unflip_requested,
                    "{row}: unrelated map leaves Output(1) held"
                );
                assert!(
                    a3_has_scene_commit_for(
                        &unrelated_records,
                        fixture.events.backend.platform.outputs[0].output.crtc
                    ),
                    "{row}: output 0 composes its newly mapped window"
                );
                a3_retire_records(&mut fixture, &unrelated_records);
                assert_eq!(
                    fixture.events.backend.pstate.direct.outputs[1]
                        .current
                        .as_ref()
                        .unwrap()
                        .candidate
                        .present_id,
                    owner_present,
                    "{row}: unrelated map preserves the same owner"
                );

                let overlay = a3_seed_ordered_window(
                    &mut fixture,
                    &mut state,
                    ResourceId(0x3122),
                    1920,
                    0,
                    1920,
                    1080,
                    false,
                );
                Backend::fill_rectangle(
                    &mut fixture.events.backend,
                    None,
                    overlay,
                    0x00d1_7b31,
                    0,
                    0,
                    1920,
                    1080,
                )
                .expect("paint a distinguishable mapped overlay");
                Backend::map_subwindow(&mut fixture.events.backend, None, overlay)
                    .expect("map the intersecting window through Backend");
                let records = a3_tick(&mut fixture);
                let owner = &fixture.events.backend.pstate.direct.outputs[1];
                assert!(
                    owner.unflip_requested,
                    "{row}: intersecting map revokes the paused owner"
                );
                assert_eq!(
                    owner.unflip_reason,
                    Some("composite_tick_output_scene_changed"),
                    "{row}: tick records the scene-change reason"
                );
                assert_eq!(
                    owner.current.as_ref().unwrap().candidate.present_id,
                    owner_present,
                    "{row}: the held direct frame remains until replacement retirement"
                );
                assert!(
                    owner.pending.is_none() && owner.queued_successor.is_none(),
                    "{row}: map revalidation does not create another Present"
                );
                assert!(
                    a3_has_scene_commit_for(
                        &records,
                        fixture.events.backend.platform.outputs[1].output.crtc
                    ),
                    "{row}: replacement is composed without a new Present"
                );
                a3_retire_records(&mut fixture, &records);
                let root = fixture.events.backend.core.window_id;
                let visible = fixture
                    .events
                    .backend
                    .get_image_pixels_for_tests(root, 2, 1930, 20, 1, 1, !0)
                    .expect("read output 1 after replacement")
                    .expect("replacement pixel bytes");
                assert_eq!(
                    &visible[..3],
                    &[0x31, 0x7b, 0xd1],
                    "{row}: replacement displays the mapped overlay pixels"
                );
            }
            "geometry-move" => {
                let mover = a3_seed_ordered_window(
                    &mut fixture,
                    &mut state,
                    ResourceId(0x3130),
                    100,
                    0,
                    500,
                    500,
                    true,
                );
                Backend::configure_subwindow(
                    &mut fixture.events.backend,
                    None,
                    mover,
                    HostSubwindowConfig {
                        x: Some(300),
                        ..Default::default()
                    },
                )
                .expect("move within output 0 through Backend");
                let unrelated_records = a3_tick(&mut fixture);
                assert!(
                    !fixture.events.backend.pstate.direct.outputs[1].unflip_requested,
                    "{row}: geometry remaining on output 0 keeps the owner"
                );
                assert!(
                    a3_has_scene_commit_for(
                        &unrelated_records,
                        fixture.events.backend.platform.outputs[0].output.crtc
                    ),
                    "{row}: output 0 composes the moved window"
                );
                a3_retire_records(&mut fixture, &unrelated_records);
                assert_eq!(
                    fixture.events.backend.pstate.direct.outputs[1]
                        .current
                        .as_ref()
                        .unwrap()
                        .candidate
                        .present_id,
                    owner_present,
                    "{row}: unrelated geometry change preserves the owner"
                );

                Backend::configure_subwindow(
                    &mut fixture.events.backend,
                    None,
                    mover,
                    HostSubwindowConfig {
                        x: Some(2000),
                        ..Default::default()
                    },
                )
                .expect("move the mapped window onto output 1 through Backend");
                let records = a3_tick(&mut fixture);
                let owner = &fixture.events.backend.pstate.direct.outputs[1];
                assert!(
                    owner.unflip_requested,
                    "{row}: tick revalidation notices geometry newly intersecting output 1"
                );
                assert_eq!(
                    owner.unflip_reason,
                    Some("composite_tick_output_scene_changed"),
                    "{row}: configured geometry records the tick-revalidation reason"
                );
                assert_eq!(
                    owner.current.as_ref().unwrap().candidate.present_id,
                    owner_present,
                    "{row}: direct frame remains until output 1 replacement retires"
                );
                assert!(
                    owner.pending.is_none() && owner.queued_successor.is_none(),
                    "{row}: geometry revalidation does not submit a new Present"
                );
                assert!(
                    a3_has_scene_commit_for(
                        &records,
                        fixture.events.backend.platform.outputs[1].output.crtc
                    ),
                    "{row}: tick accepts the composed replacement"
                );
                a3_retire_records(&mut fixture, &records);
                assert!(
                    fixture.events.backend.pstate.direct.outputs[1]
                        .current
                        .is_none(),
                    "{row}: owner retires after replacement acknowledgement"
                );
            }
            "redirect" => {
                Backend::allocate_redirected_backing(
                    &mut fixture.events.backend,
                    None,
                    yserver_core::backend::WindowHandle::from_raw_panicking(direct.target_xid),
                    direct.candidate.src_width,
                    direct.candidate.src_height,
                    24,
                )
                .expect("redirect the held window through Backend");
                let records = a3_tick(&mut fixture);
                let owner = &fixture.events.backend.pstate.direct.outputs[1];
                assert!(
                    owner.unflip_requested,
                    "{row}: redirected target is no longer eligible"
                );
                assert_eq!(
                    owner.unflip_reason,
                    Some("composite_tick_output_scene_changed"),
                    "{row}: redirect records the tick scene-change reason"
                );
                assert_eq!(
                    owner.current.as_ref().unwrap().candidate.present_id,
                    owner_present,
                    "{row}: retained direct frame awaits replacement"
                );
                assert!(
                    owner.pending.is_none() && owner.queued_successor.is_none(),
                    "{row}: redirect revalidation does not create a new Present"
                );
                assert!(
                    a3_has_scene_commit_for(
                        &records,
                        fixture.events.backend.platform.outputs[1].output.crtc
                    ),
                    "{row}: output 1 replacement is composed without a Present"
                );
                a3_retire_records(&mut fixture, &records);
                assert!(
                    fixture.events.backend.pstate.direct.outputs[1]
                        .current
                        .is_none(),
                    "{row}: redirected owner retires after replacement acknowledgement"
                );
            }
            _ => unreachable!("known Q11 row"),
        }
        fixture.finish_after_normal_drain();
    }
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: submit a queued successor after a mapped window makes its scene ineligible.
fn queued_successor_waits_for_scene_revalidation_after_overlay_map() {
    use yserver_core::{backend::Backend, server::ServerState};
    use yserver_protocol::x11::ResourceId;

    let Some((fixture, direct)) = a3_output_fixture(1, 945) else {
        eprintln!(
            "SKIP queued_successor_waits_for_scene_revalidation_after_overlay_map: Vulkan fixture unavailable"
        );
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    a3_enter_output_owner(&mut fixture, &direct, 1);

    let fd = fixture.kms_fd(1);
    let before = fixture.events.recorders[&fd].records().len();
    let mut pending_candidate = direct.candidate;
    pending_candidate.present_id = 946;
    let mut pending_event = direct.event.clone();
    pending_event.present_id = 946;
    pending_event.serial = 946;
    Backend::note_present_scanout_candidate(&mut fixture.events.backend, pending_candidate);
    assert!(
        Backend::try_present_direct(
            &mut fixture.events.backend,
            pending_candidate,
            pending_event,
        )
        .expect("submit the first successor through Backend Present")
    );
    let pending_id = fixture.events.backend.pstate.direct.outputs[1]
        .pending
        .as_ref()
        .and_then(|frame| frame.accepted_event_id)
        .expect("first successor has an accepted commit identity");
    let pending_record = a3_records_since(&fixture, fd, before)
        .into_iter()
        .find(|record| super::commit_identity::accepted_event_id(record) == Some(pending_id))
        .expect("record the pending direct successor commit");
    assert_eq!(pending_record.events.len(), 1);
    assert_eq!(
        pending_record.events[0].crtc,
        fixture.events.backend.platform.outputs[1].output.crtc
    );

    let mut successor_candidate = pending_candidate;
    successor_candidate.present_id = 947;
    let mut successor_event = direct.event.clone();
    successor_event.present_id = 947;
    successor_event.serial = 947;
    Backend::note_present_scanout_candidate(&mut fixture.events.backend, successor_candidate);
    assert!(
        Backend::try_present_direct(
            &mut fixture.events.backend,
            successor_candidate,
            successor_event,
        )
        .expect("queue the next Present through Backend")
    );
    assert_eq!(
        fixture.events.backend.pstate.direct.outputs[1]
            .queued_successor
            .as_ref()
            .map(|frame| frame.candidate.present_id),
        Some(947),
        "the successor waits behind the pending direct commit"
    );

    let mut state = ServerState::new();
    let overlay = a3_seed_ordered_window(
        &mut fixture,
        &mut state,
        ResourceId(0x3190),
        1920,
        0,
        1920,
        1080,
        false,
    );
    Backend::fill_rectangle(
        &mut fixture.events.backend,
        None,
        overlay,
        0x0001_2345,
        0,
        0,
        1920,
        1080,
    )
    .expect("paint distinguishable overlay pixels");
    Backend::map_subwindow(&mut fixture.events.backend, None, overlay)
        .expect("map overlay through Backend");

    a3_retire_records(&mut fixture, std::slice::from_ref(&pending_record));
    let owner = &fixture.events.backend.pstate.direct.outputs[1];
    assert!(
        owner.unflip_requested,
        "the newly ineligible scene requests unflip before queued direct submission"
    );
    assert!(
        owner.pending.is_none(),
        "the queued successor was not submitted after the pending frame retires"
    );

    let replacement = a3_tick(&mut fixture);
    assert!(
        a3_has_scene_commit_for(
            &replacement,
            fixture.events.backend.platform.outputs[1].output.crtc,
        ),
        "the next Backend scheduler pass composes output 1"
    );
    a3_retire_records(&mut fixture, &replacement);
    assert!(
        fixture
            .events
            .backend
            .direct_scanout_frame_for_output(1)
            .is_none(),
        "the invalid direct owner retires after the composed replacement"
    );
    let root_xid = fixture.events.backend.core.window_id;
    let visible = fixture
        .events
        .backend
        .get_image_pixels_for_tests(root_xid, 2, 1930, 20, 1, 1, !0)
        .expect("read output 1 after replacement")
        .expect("replacement pixel bytes");
    assert_eq!(
        &visible[..3],
        &[0x45, 0x23, 0x01],
        "the composed replacement shows the newly mapped overlay, not a direct successor"
    );
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: drop the Output-owner ancestor walk from direct_scopes_referencing_host_drawable.
fn ancestor_reparent_revokes_output_owner_immediately() {
    use yserver_core::backend::Backend;

    let Some((mut fixture, direct)) = a3_output_fixture(1, 935) else {
        eprintln!(
            "SKIP ancestor_reparent_revokes_output_owner_immediately: Vulkan fixture unavailable"
        );
        return;
    };
    let frame = 0x5200;
    let new_parent = 0x5201;
    super::seed_window(&mut fixture.events.backend, frame, None, 1920, 0);
    super::seed_window(&mut fixture.events.backend, new_parent, None, 0, 0);
    fixture
        .events
        .backend
        .windows
        .get_mut(&frame)
        .unwrap()
        .width = 1920;
    fixture
        .events
        .backend
        .windows
        .get_mut(&frame)
        .unwrap()
        .height = 1080;
    Backend::reparent_subwindow(
        &mut fixture.events.backend,
        None,
        direct.target_xid,
        frame,
        0,
        0,
    )
    .expect("put the direct window inside its WM frame before Present");
    fixture.events.backend.core.top_level_order = vec![new_parent, frame];
    a3_enter_output_owner(&mut fixture, &direct, 1);

    Backend::reparent_subwindow(
        &mut fixture.events.backend,
        None,
        frame,
        new_parent,
        1920,
        0,
    )
    .expect("reparent the ancestor at the same absolute position");
    let owner = &fixture.events.backend.pstate.direct.outputs[1];
    assert!(
        owner.unflip_requested,
        "ancestor changes revoke immediately"
    );
    assert_eq!(owner.unflip_reason, Some("reparent_direct_frame_chain"));
    assert_eq!(
        owner.current.as_ref().unwrap().candidate.present_id,
        direct.candidate.present_id,
        "the retained frame is still pinned until the composed replacement"
    );
    let records = a3_tick(&mut fixture);
    assert!(a3_has_scene_commit_for(
        &records,
        fixture.events.backend.platform.outputs[1].output.crtc
    ));
    a3_retire_records(&mut fixture, &records);
    fixture.finish();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: A12 makes drawable-specific unflip revoke every owner; A14 makes
// SHAPE revoke every scope immediately instead of waiting for tick revalidation.
fn drawable_change_isolation() {
    use yserver_core::backend::Backend;
    use yserver_protocol::x11::xfixes::RegionRect;

    for (row, first_id, second_id) in [("unmap", 936, 937), ("shape", 939, 940)] {
        let Some(fixture) = super::commit_identity::recording_scene::RecordingSceneFixture::with_output_admission_topology()
            .expect("A3 dual-owner fixture")
        else {
            eprintln!("SKIP drawable_change_isolation: Vulkan fixture unavailable");
            return;
        };
        let mut fixture = fixture.cleanup_guard();
        let first = fixture.install_direct_output_present(0, first_id, 0);
        let second = fixture.install_direct_output_present(1, second_id, 0);
        a3_enter_output_owner(&mut fixture, &first, 0);
        a3_enter_output_owner(&mut fixture, &second, 1);

        match row {
            "unmap" => {
                Backend::unmap_subwindow(&mut fixture.events.backend, None, first.target_xid)
                    .expect("unmap Output(0) drawable through Backend");
                let output0 = &fixture.events.backend.pstate.direct.outputs[0];
                let output1 = &fixture.events.backend.pstate.direct.outputs[1];
                assert!(
                    output0.unflip_requested,
                    "{row}: changed drawable revokes its owner"
                );
                assert_eq!(
                    output0.unflip_reason,
                    Some("unmap_direct_frame_drawable"),
                    "{row}: unmap keeps its scoped reason"
                );
                assert!(!output1.unflip_requested, "{row}: Output(1) stays held");
                assert_eq!(
                    output1.current.as_ref().unwrap().candidate.present_id,
                    second_id,
                    "{row}: Output(1) keeps its independent current frame"
                );
                let records = a3_tick(&mut fixture);
                let output0_crtc = fixture.events.backend.platform.outputs[0].output.crtc;
                let output1_crtc = fixture.events.backend.platform.outputs[1].output.crtc;
                assert!(
                    a3_has_scene_commit_for(&records, output0_crtc),
                    "{row}: Output(0) replacement is accepted"
                );
                assert!(
                    !a3_has_scene_commit_for(&records, output1_crtc),
                    "{row}: unaffected Output(1) gets no replacement"
                );
                a3_retire_records(&mut fixture, &records);
                let survivor = &fixture.events.backend.pstate.direct.outputs[1];
                assert!(
                    !survivor.unflip_requested,
                    "{row}: Output(1) survives retirement"
                );
                assert_eq!(
                    survivor.current.as_ref().unwrap().candidate.present_id,
                    second_id,
                    "{row}: Output(1) remains current through Output(0) retirement"
                );
            }
            "shape" => {
                Backend::set_shape_rectangles(
                    &mut fixture.events.backend,
                    None,
                    second.target_xid,
                    0,
                    Some(&[RegionRect {
                        x: 0,
                        y: 0,
                        width: 1000,
                        height: 1080,
                    }]),
                )
                .expect("clip Output(1) target through Backend SHAPE");
                assert!(
                    !fixture.events.backend.pstate.direct.outputs[1].unflip_requested,
                    "{row}: SHAPE waits for the next scheduler pass"
                );
                assert!(
                    !fixture.events.backend.pstate.direct.outputs[0].unflip_requested,
                    "{row}: SHAPE does not eagerly revoke Output(0)"
                );
                let records = a3_tick(&mut fixture);
                let output0 = &fixture.events.backend.pstate.direct.outputs[0];
                let output1 = &fixture.events.backend.pstate.direct.outputs[1];
                assert!(
                    output1.unflip_requested,
                    "{row}: tick revalidation revokes Output(1)"
                );
                assert_eq!(
                    output1.unflip_reason,
                    Some("composite_tick_output_scene_changed"),
                    "{row}: SHAPE retains the tick scene-change reason"
                );
                assert!(!output0.unflip_requested, "{row}: Output(0) remains active");
                assert_eq!(
                    output0.current.as_ref().unwrap().candidate.present_id,
                    first_id,
                    "{row}: Output(0) keeps its direct frame"
                );
                assert!(
                    a3_has_scene_commit_for(
                        &records,
                        fixture.events.backend.platform.outputs[1].output.crtc
                    ),
                    "{row}: Output(1) replacement is accepted"
                );
                assert!(
                    !a3_has_scene_commit_for(
                        &records,
                        fixture.events.backend.platform.outputs[0].output.crtc
                    ),
                    "{row}: Output(0) gets no replacement"
                );
                a3_retire_records(&mut fixture, &records);
                let survivor = &fixture.events.backend.pstate.direct.outputs[0];
                assert!(
                    !survivor.unflip_requested,
                    "{row}: Output(0) survives Output(1) retirement"
                );
                assert_eq!(
                    survivor.current.as_ref().unwrap().candidate.present_id,
                    first_id,
                    "{row}: Output(0) remains current after Output(1) retirement"
                );
            }
            _ => unreachable!("known Q12 row"),
        }
        drop(fixture);
    }
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: on_top_level_order_changed omits Root's top-level unflip.
fn root_owner_triggers_unchanged() {
    use yserver_core::{backend::Backend, host_x11::HostSubwindowConfig, server::ServerState};
    use yserver_protocol::x11::{ResourceId, xfixes::RegionRect};

    let Some((mut fixture, direct)) = a3_root_fixture(941) else {
        eprintln!("SKIP root_owner_triggers_unchanged: Vulkan fixture unavailable");
        return;
    };
    a3_enter_root_owner(&mut fixture, &direct);
    let mut state = ServerState::new();
    let first = a3_seed_ordered_window(
        &mut fixture,
        &mut state,
        ResourceId(0x3150),
        10,
        10,
        100,
        100,
        true,
    );
    let second = a3_seed_ordered_window(
        &mut fixture,
        &mut state,
        ResourceId(0x3151),
        200,
        10,
        100,
        100,
        true,
    );
    let unmapped = a3_seed_ordered_window(
        &mut fixture,
        &mut state,
        ResourceId(0x3152),
        300,
        10,
        100,
        100,
        false,
    );
    let root_width = fixture.events.backend.platform.fb_w;
    let root_height = fixture.events.backend.platform.fb_h;
    a3_add_existing_window_to_state(
        &mut state,
        ResourceId(0x3153),
        direct.target_xid,
        0,
        0,
        root_width,
        root_height,
        true,
    );
    Backend::map_subwindow(&mut fixture.events.backend, None, unmapped)
        .expect("map unrelated window through Backend");
    Backend::configure_subwindow(
        &mut fixture.events.backend,
        None,
        first,
        HostSubwindowConfig {
            x: Some(30),
            ..Default::default()
        },
    )
    .expect("configure unrelated window through Backend");
    assert!(
        !fixture.events.backend.pstate.direct.root().unflip_requested,
        "unrelated map and configure keep Root direct as before"
    );

    a3_restack_top_level(
        &mut fixture,
        &mut state,
        ResourceId(0x3150),
        ResourceId(0x3151),
        first,
        second,
        0,
    );
    assert!(fixture.events.backend.pstate.direct.root().unflip_requested);
    assert_eq!(
        fixture.events.backend.pstate.direct.root().unflip_reason,
        Some("top_level_stack_changed")
    );
    Backend::set_shape_rectangles(
        &mut fixture.events.backend,
        None,
        direct.target_xid,
        0,
        Some(&[RegionRect {
            x: 0,
            y: 0,
            width: 1000,
            height: root_height,
        }]),
    )
    .expect("shape the Root direct window through Backend");
    assert_eq!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .unflip_last_reason,
        Some("shape_clips_direct_frame")
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
    fixture.finish();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations by row: D02 returns after fresh unflip / uses a degraded-only pass mask;
// A16 returns after a submitted unflip / uses a degraded-only mask / excludes the degraded owner;
// T06 returns globally while an Output owner is held; T07 excludes unrelated output in degraded mode;
// T08 ignores scene damage when an Output owner is held.
fn other_output_progresses() {
    use yserver_core::backend::Backend;

    fn retire_held_output_owner(
        fixture: &mut super::commit_identity::recording_scene::RecordingSceneFixtureGuard,
        direct: &super::commit_identity::recording_scene::DirectPresentSetup,
        row: &str,
    ) {
        Backend::unmap_subwindow(&mut fixture.events.backend, None, direct.target_xid)
            .expect("retire the held Output owner through Backend");
        let records = a3_tick(fixture);
        assert!(
            a3_has_scene_commit_for(
                &records,
                fixture.events.backend.platform.outputs[1].output.crtc
            ),
            "{row}: cleanup submits the owner replacement"
        );
        a3_retire_records(fixture, &records);
        assert!(
            fixture.events.backend.pstate.direct.outputs[1]
                .current
                .is_none(),
            "{row}: cleanup retires the held owner"
        );
        let _ = Backend::drain_completed_present_events(&mut fixture.events.backend);
        let _ = Backend::drain_retired_present_idle_events(&mut fixture.events.backend);
    }

    for (row, present_id) in [
        ("pending-direct", 956),
        ("held-direct", 957),
        ("fresh-unflip", 958),
        ("degraded-unflip", 959),
    ] {
        let Some((fixture, direct)) = a3_output_fixture(1, present_id) else {
            eprintln!("SKIP other_output_progresses: Vulkan fixture unavailable");
            return;
        };
        let mut fixture = fixture.cleanup_guard();
        let fd = fixture.kms_fd(0);
        let output0_crtc = fixture.events.backend.platform.outputs[0].output.crtc;
        let output1_crtc = fixture.events.backend.platform.outputs[1].output.crtc;
        a3_enter_output_owner(&mut fixture, &direct, 1);
        let warmup = a3_tick(&mut fixture);
        a3_retire_records(&mut fixture, &warmup);
        assert!(
            !fixture.events.backend.scene.has_pending_page_flip(0),
            "{row}: composed output is settled before the scenario damage"
        );
        assert_eq!(
            fixture.events.backend.pstate.direct.outputs[1]
                .current
                .as_ref()
                .map(|frame| frame.candidate.present_id),
            Some(direct.candidate.present_id),
            "{row}: scheduler warm-up leaves the held owner unchanged"
        );

        match row {
            // D02: an output 0 scene commit is accepted while a later output 1 direct frame is pending.
            "pending-direct" => {
                let before = fixture.events.recorders[&fd].records().len();
                let mut candidate = direct.candidate;
                candidate.present_id += 1;
                let mut event = direct.event.clone();
                event.present_id = candidate.present_id;
                event.serial = event.serial.wrapping_add(1);
                Backend::note_present_scanout_candidate(&mut fixture.events.backend, candidate);
                assert!(
                    Backend::try_present_direct(&mut fixture.events.backend, candidate, event)
                        .expect("pending successor through Backend Present"),
                    "{row}: successor is accepted while the owner remains current"
                );
                let pending_id = fixture.events.backend.pstate.direct.outputs[1]
                    .pending
                    .as_ref()
                    .and_then(|frame| frame.accepted_event_id)
                    .expect("pending output 1 frame has an accepted event identity");
                let pending_record = a3_records_since(&fixture, fd, before)
                    .into_iter()
                    .find(|record| {
                        super::commit_identity::accepted_event_id(record) == Some(pending_id)
                    })
                    .expect("record accepted output 1 pending commit");
                assert_eq!(
                    pending_record.events.len(),
                    1,
                    "{row}: pending commit has one CRTC event"
                );
                assert_eq!(
                    pending_record.events[0].crtc, output1_crtc,
                    "{row}: pending direct commit belongs to output 1"
                );

                super::commit_identity::direct_unflip::damage_scene_root(&mut fixture);
                let records = a3_tick(&mut fixture);
                let output0_record = records
                    .iter()
                    .find(|record| {
                        super::commit_identity::accepted_event_id(record).is_some()
                            && a3_has_scene_commit_for(std::slice::from_ref(*record), output0_crtc)
                    })
                    .expect("output 0 scene damage is accepted while output 1 is pending");
                let output0_id = super::commit_identity::accepted_event_id(output0_record).unwrap();
                assert_ne!(
                    output0_id, pending_id,
                    "{row}: independent CRTC acknowledgements use distinct event IDs"
                );
                let mut accepted_crtcs: Vec<_> = std::iter::once(&pending_record)
                    .chain(records.iter())
                    .filter(|record| super::commit_identity::accepted_event_id(record).is_some())
                    .flat_map(|record| record.events.iter().map(|event| event.crtc))
                    .collect();
                accepted_crtcs.sort_by_key(|crtc| u32::from(*crtc));
                let mut expected_crtcs = vec![output0_crtc, output1_crtc];
                expected_crtcs.sort_by_key(|crtc| u32::from(*crtc));
                assert_eq!(
                    accepted_crtcs, expected_crtcs,
                    "{row}: exact accepted commits cover the pending owner and damaged output"
                );
                assert_eq!(
                    fixture.events.backend.scene.pending_page_flip_event_id(0),
                    Some(output0_id),
                    "{row}: output 0 tracks its scene acknowledgement"
                );
                assert_eq!(
                    fixture.events.backend.pstate.direct.outputs[1]
                        .pending
                        .as_ref()
                        .and_then(|frame| frame.accepted_event_id),
                    Some(pending_id),
                    "{row}: output 1 tracks its direct acknowledgement independently"
                );
                assert!(
                    fixture.events.backend.pstate.direct.outputs[1]
                        .pending
                        .is_some(),
                    "{row}: output 1 remains pending after output 0 submission"
                );
                a3_retire_records(&mut fixture, std::slice::from_ref(output0_record));
                assert!(
                    fixture.events.backend.pstate.direct.outputs[1]
                        .pending
                        .is_some(),
                    "{row}: output 0 retirement leaves output 1 pending"
                );
                a3_retire_records(&mut fixture, std::slice::from_ref(&pending_record));
                assert_eq!(
                    fixture.events.backend.pstate.direct.outputs[1]
                        .current
                        .as_ref()
                        .map(|frame| frame.candidate.present_id),
                    Some(candidate.present_id),
                    "{row}: output 1 advances only after its own acknowledgement"
                );
                retire_held_output_owner(&mut fixture, &direct, row);
            }
            // T06/T08: a held owner must not suppress an immediate wake or output 0's scene commit.
            "held-direct" => {
                super::commit_identity::direct_unflip::damage_scene_root(&mut fixture);
                let wakeup = Backend::next_wakeup(&fixture.events.backend);
                assert!(
                    wakeup.is_some_and(|deadline| deadline <= std::time::Instant::now()),
                    "{row}: output 0 scene damage arms an immediate wake"
                );
                assert!(
                    fixture.events.backend.pstate.direct.outputs[1]
                        .pending
                        .is_none(),
                    "{row}: wake assertion is driven only by held direct state, not pending work"
                );
                let records = a3_tick(&mut fixture);
                let accepted_crtcs: Vec<_> = records
                    .iter()
                    .filter(|record| super::commit_identity::accepted_event_id(record).is_some())
                    .flat_map(|record| record.events.iter().map(|event| event.crtc))
                    .collect();
                assert_eq!(
                    accepted_crtcs,
                    [output0_crtc],
                    "{row}: exact accepted CRTC set contains only composed output 0"
                );
                assert_eq!(
                    fixture.events.backend.pstate.direct.outputs[1]
                        .current
                        .as_ref()
                        .map(|frame| frame.candidate.present_id),
                    Some(direct.candidate.present_id),
                    "{row}: held Output(1) remains the same owner"
                );
                a3_retire_records(&mut fixture, &records);
                assert_eq!(
                    fixture.events.backend.pstate.direct.outputs[1]
                        .current
                        .as_ref()
                        .map(|frame| frame.candidate.present_id),
                    Some(direct.candidate.present_id),
                    "{row}: output 0 retirement leaves Output(1) held"
                );
                retire_held_output_owner(&mut fixture, &direct, row);
            }
            // D02/A16: fresh replacement and unrelated scene damage are both accepted in one pass.
            "fresh-unflip" | "degraded-unflip" => {
                Backend::unmap_subwindow(&mut fixture.events.backend, None, direct.target_xid)
                    .expect("request fresh unflip through Backend");
                super::commit_identity::direct_unflip::damage_scene_root(&mut fixture);
                if row == "degraded-unflip" {
                    let recorder = fixture.events.recorders[&fd].clone();
                    recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EIO)));
                    recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EIO)));
                }
                let records = a3_tick(&mut fixture);
                let mut accepted_crtcs: Vec<_> = records
                    .iter()
                    .filter(|record| super::commit_identity::accepted_event_id(record).is_some())
                    .flat_map(|record| record.events.iter().map(|event| event.crtc))
                    .collect();
                accepted_crtcs.sort_by_key(|crtc| u32::from(*crtc));
                let mut expected_crtcs = vec![output0_crtc, output1_crtc];
                expected_crtcs.sort_by_key(|crtc| u32::from(*crtc));
                assert_eq!(
                    accepted_crtcs, expected_crtcs,
                    "{row}: exact accepted CRTC set includes composed output and unflip replacement"
                );
                let output0_record = records
                    .iter()
                    .find(|record| {
                        super::commit_identity::accepted_event_id(record).is_some()
                            && a3_has_scene_commit_for(std::slice::from_ref(*record), output0_crtc)
                    })
                    .expect("output 0 scene frame accepted while output 1 is unflipping");
                let replacement = records
                    .iter()
                    .find(|record| {
                        super::commit_identity::accepted_event_id(record).is_some()
                            && a3_has_scene_commit_for(std::slice::from_ref(*record), output1_crtc)
                    })
                    .expect("output 1 replacement accepted in the same pass");
                let output0_id = super::commit_identity::accepted_event_id(output0_record).unwrap();
                let replacement_id =
                    super::commit_identity::accepted_event_id(replacement).unwrap();
                if row == "fresh-unflip" {
                    assert_ne!(
                        output0_id, replacement_id,
                        "{row}: each output has an independent accepted acknowledgement ID"
                    );
                    assert_eq!(
                        fixture.events.backend.scene.pending_page_flip_event_id(0),
                        Some(output0_id),
                        "{row}: output 0 tracks its own scene acknowledgement"
                    );
                    assert_eq!(
                        fixture.events.backend.scene.pending_page_flip_event_id(1),
                        Some(replacement_id),
                        "{row}: output 1 tracks its replacement acknowledgement"
                    );
                    assert!(
                        fixture.events.backend.pstate.direct.outputs[1].fresh_composed_unflip,
                        "{row}: accepted replacement is classified as a fresh unflip"
                    );
                } else {
                    assert!(
                        fixture.events.backend.pstate.direct.outputs[1].degraded_composed_unflip,
                        "{row}: EIO enters degraded replacement"
                    );
                    assert!(
                        fixture.events.backend.pstate.direct.outputs[1]
                            .unflip_outputs_to_replace
                            .contains(&1),
                        "{row}: degraded replacement obligation remains until retirement"
                    );
                }
                assert_eq!(
                    fixture.events.backend.pstate.direct.outputs[1]
                        .unflip_awaiting_outputs
                        .get(&1),
                    Some(&replacement_id),
                    "{row}: replacement identity remains assigned to output 1"
                );
                a3_retire_records(&mut fixture, std::slice::from_ref(output0_record));
                assert!(
                    fixture.events.backend.pstate.direct.outputs[1]
                        .current
                        .is_some(),
                    "{row}: output 0 retirement does not release output 1's owner"
                );
                assert!(
                    fixture.events.backend.scene.has_pending_page_flip(1),
                    "{row}: output 1 acknowledgement remains pending independently"
                );
                a3_retire_records(&mut fixture, std::slice::from_ref(replacement));
                assert!(
                    fixture.events.backend.pstate.direct.outputs[1]
                        .current
                        .is_none(),
                    "{row}: output 1 owner releases when its replacement retires"
                );
                assert!(
                    fixture.events.backend.present_source_pins.is_empty(),
                    "{row}: replacement retirement releases source pins"
                );
            }
            _ => unreachable!("known Q06 row"),
        }
        fixture.finish_after_normal_drain();
    }
}
#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: A17 invalidates every output's buffers; it also marks output 0
// structurally dirty during fresh preparation, retirement, degraded setup, or retry.
fn output_unflip_preserves_other_damage() {
    use yserver_core::backend::Backend;

    fn request_output_unflip(
        fixture: &mut super::commit_identity::recording_scene::RecordingSceneFixtureGuard,
        direct: &super::commit_identity::recording_scene::DirectPresentSetup,
        output_idx: usize,
        row: &str,
    ) {
        let mut successor = direct.candidate;
        successor.present_id += 1;
        successor.valid_region_xid = 0x55;
        let mut event = direct.event.clone();
        event.present_id = successor.present_id;
        event.serial = event.serial.wrapping_add(1);
        assert!(
            !Backend::try_present_direct(&mut fixture.events.backend, successor, event)
                .expect("ineligible Output successor through Backend"),
            "{row}: successor requests composed unflip"
        );
        assert!(
            fixture.events.backend.pstate.direct.outputs[output_idx].unflip_requested,
            "{row}: ineligible successor marks its output for unflip"
        );
    }

    fn settle_other_output(
        fixture: &mut super::commit_identity::recording_scene::RecordingSceneFixtureGuard,
        output0_crtc: ::drm::control::crtc::Handle,
        row: &str,
    ) {
        for _ in 0..3 {
            let records = a3_tick(fixture);
            let has_output0_scene = a3_has_scene_commit_for(&records, output0_crtc);
            a3_retire_records(fixture, &records);
            if !has_output0_scene {
                break;
            }
        }
        assert!(
            !fixture.events.backend.scene.has_pending_page_flip(0),
            "{row}: output 0 starts with no scene acknowledgement in flight"
        );
    }

    for (row, present_id, degraded) in [("fresh", 958, false), ("degraded-retry", 959, true)] {
        let Some((fixture, direct)) = a3_output_fixture(1, present_id) else {
            eprintln!("SKIP output_unflip_preserves_other_damage: Vulkan fixture unavailable");
            return;
        };
        let mut fixture = fixture.cleanup_guard();
        let fd = fixture.kms_fd(0);
        a3_enter_output_owner(&mut fixture, &direct, 1);
        let output0_crtc = fixture.events.backend.platform.outputs[0].output.crtc;
        settle_other_output(&mut fixture, output0_crtc, row);
        let output0_before = fixture
            .events
            .backend
            .scene
            .retirement_observation_for_tests(0);
        assert!(
            output0_before.contains("structure_damage=RegionSet { rects: [] }"),
            "{row}: output 0 starts clean: {output0_before}"
        );
        request_output_unflip(&mut fixture, &direct, 1, row);

        let recorder = fixture.events.recorders[&fd].clone();
        if degraded {
            recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EIO)));
            recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EIO)));
            // Reject the first per-output replacement too, forcing the Backend retry path.
            recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EIO)));
        }
        let setup_start = recorder.records().len();
        Backend::maybe_composite(&mut fixture.events.backend)
            .expect("prepare output 1 composed unflip through Backend");
        fixture.events.wait_for_submitted_gpu_work();
        let setup_records = a3_records_since(&fixture, fd, setup_start);

        if !degraded {
            assert!(
                fixture.events.backend.pstate.direct.outputs[1].fresh_composed_unflip,
                "{row}: output 1 enters the fresh replacement path"
            );
            assert!(
                !a3_has_scene_commit_for(&setup_records, output0_crtc),
                "{row}: fresh preparation does not repaint output 0"
            );
            assert_eq!(
                fixture
                    .events
                    .backend
                    .scene
                    .retirement_observation_for_tests(0),
                output0_before,
                "{row}: fresh preparation preserves output 0 buffer history and structure damage"
            );
            let replacement = setup_records
                .iter()
                .find(|record| {
                    super::commit_identity::accepted_event_id(record).is_some()
                        && a3_has_scene_commit_for(
                            std::slice::from_ref(*record),
                            fixture.events.backend.platform.outputs[1].output.crtc,
                        )
                })
                .expect("fresh output 1 replacement accepted");
            a3_retire_records(&mut fixture, std::slice::from_ref(replacement));
            assert!(
                fixture.events.backend.pstate.direct.outputs[1]
                    .current
                    .is_none(),
                "{row}: fresh replacement retirement releases the output 1 owner"
            );
            assert_eq!(
                fixture
                    .events
                    .backend
                    .scene
                    .retirement_observation_for_tests(0),
                output0_before,
                "{row}: fresh replacement retirement preserves output 0 history and structure damage"
            );
        } else {
            let owner = &fixture.events.backend.pstate.direct.outputs[1];
            assert!(
                owner.degraded_composed_unflip,
                "{row}: scripted EIO enters degraded replacement"
            );
            assert!(
                owner.unflip_awaiting_outputs.is_empty(),
                "{row}: rejected degraded setup has no accepted replacement identity"
            );
            assert!(
                owner.unflip_outputs_to_replace.contains(&1),
                "{row}: output 1 replacement obligation remains tracked"
            );
            assert!(
                owner.current.is_some(),
                "{row}: owner remains active before replacement retirement"
            );
            assert_eq!(
                fixture.events.backend.pstate.direct.active_scopes(),
                [crate::kms::render::backend::DirectScope::Output(1)],
                "{row}: degraded work remains reachable through its owner scope"
            );
            assert!(
                !a3_has_scene_commit_for(&setup_records, output0_crtc),
                "{row}: degraded setup does not repaint output 0"
            );
            assert_eq!(
                fixture
                    .events
                    .backend
                    .scene
                    .retirement_observation_for_tests(0),
                output0_before,
                "{row}: degraded setup preserves output 0 history and structure damage"
            );
            assert!(
                setup_records.iter().any(|record| {
                    record.result != crate::drm::atomic::tests_support::ScriptedResult::Ok
                        && a3_has_scene_commit_for(
                            std::slice::from_ref(record),
                            fixture.events.backend.platform.outputs[1].output.crtc,
                        )
                }),
                "{row}: recorder confirms the first output 1 replacement was rejected"
            );
            assert!(
                setup_records
                    .iter()
                    .all(|record| super::commit_identity::accepted_event_id(record).is_none()),
                "{row}: setup contains no accepted replacement event"
            );

            let retry_deadline = Backend::next_wakeup(&fixture.events.backend)
                .expect("rejected replacement schedules a Backend retry");
            let until_retry = retry_deadline.saturating_duration_since(std::time::Instant::now());
            if !until_retry.is_zero() {
                std::thread::sleep(until_retry);
            }
            Backend::before_block(&mut fixture.events.backend);
            let retry_start = recorder.records().len();
            Backend::maybe_composite(&mut fixture.events.backend)
                .expect("retry the rejected degraded replacement through Backend");
            fixture.events.wait_for_submitted_gpu_work();
            let retry_records = a3_records_since(&fixture, fd, retry_start);
            assert!(
                !a3_has_scene_commit_for(&retry_records, output0_crtc),
                "{row}: degraded retry does not repaint output 0"
            );
            assert_eq!(
                fixture
                    .events
                    .backend
                    .scene
                    .retirement_observation_for_tests(0),
                output0_before,
                "{row}: degraded retry preserves output 0 history and structure damage"
            );
            let replacement = retry_records
                .iter()
                .find(|record| {
                    super::commit_identity::accepted_event_id(record).is_some()
                        && a3_has_scene_commit_for(
                            std::slice::from_ref(*record),
                            fixture.events.backend.platform.outputs[1].output.crtc,
                        )
                })
                .expect("degraded retry accepts output 1 replacement");
            a3_retire_records(&mut fixture, std::slice::from_ref(replacement));
            assert!(
                fixture.events.backend.pstate.direct.outputs[1]
                    .current
                    .is_none(),
                "{row}: retry retirement releases output 1's owner"
            );
            assert_eq!(
                fixture
                    .events
                    .backend
                    .scene
                    .retirement_observation_for_tests(0),
                output0_before,
                "{row}: retry replacement retirement keeps output 0 history and structure damage"
            );
        }
        fixture.finish_after_normal_drain();
    }
}
#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: use the global scene predicate when deciding whether to skip output 0's cursor-only commit.
fn retained_direct_damage_does_not_block_other_output_cursor_commit() {
    use yserver_core::backend::Backend;

    let Some(mut atomic_fixture) =
        super::cursor_sprites::support::AtomicPathFixture::with_output_count(
            super::cursor_sprites::support::PathKind::Physical,
            2,
        )
        .expect("two-output cursor scheduling fixture construction")
    else {
        eprintln!(
            "SKIP retained_direct_damage_does_not_block_other_output_cursor_commit: Vulkan fixture unavailable"
        );
        return;
    };
    let mut delivered = std::collections::HashSet::new();
    let cursor = super::cursor_sprites::support::create_bitmap_cursor(
        &mut atomic_fixture.scene.events.backend,
        true,
    );
    super::cursor_sprites::support::establish_hardware_cursor_on_output(
        &mut atomic_fixture,
        &mut delivered,
        cursor,
        0,
        (100, 100),
    );
    for output_idx in 0..2 {
        super::cursor_sprites::support::drain_unretired_output_events(
            &mut atomic_fixture,
            &mut delivered,
            output_idx,
        );
    }
    let direct = atomic_fixture
        .scene
        .install_direct_output_present(1, 958, 0);
    let fixture = &mut atomic_fixture.scene;
    let device_fd = fixture.kms_fd(0);
    let output1 = &fixture.events.backend.platform.outputs[1];
    let (x, y, width, height) = (
        output1.x as i16,
        output1.y as i16,
        output1.width,
        output1.height,
    );
    a3_add_existing_window_to_state(
        &mut fixture.events.state,
        yserver_protocol::x11::ResourceId(0x31a0),
        direct.target_xid,
        x,
        y,
        width,
        height,
        true,
    );
    Backend::sync_top_level_order(&mut fixture.events.backend, &fixture.events.state);
    Backend::set_cursor_hidden(&mut fixture.events.backend, false);
    for _ in 0..4 {
        Backend::maybe_composite(&mut fixture.events.backend)
            .expect("settle output 0's hardware cursor through Backend");
        fixture.events.wait_for_submitted_gpu_work();
        for output_idx in 0..2 {
            super::cursor_sprites::support::drain_output_events(
                &mut fixture.events,
                &mut delivered,
                output_idx,
            );
        }
    }
    assert_eq!(
        fixture.events.backend.scene.cursor_mode_for_outputs(&[0]),
        crate::kms::render::scene::CursorPlaneMode::Hw,
        "output 0 has an attached hardware cursor before Output(1) enters direct scanout"
    );
    Backend::warp_pointer_root(
        &mut fixture.events.backend,
        &mut fixture.events.state,
        100,
        100,
    );
    super::commit_identity::direct_unflip::damage_scene_root(fixture);
    for _ in 0..4 {
        Backend::maybe_composite(&mut fixture.events.backend)
            .expect("compose the output window and establish output 0's cursor");
        fixture.events.wait_for_submitted_gpu_work();
        for output_idx in 0..2 {
            super::cursor_sprites::support::drain_output_events(
                &mut fixture.events,
                &mut delivered,
                output_idx,
            );
        }
    }
    let direct_records = a3_admit_output_owner(fixture, &direct, 1);
    let direct_id = direct_records
        .iter()
        .find_map(super::commit_identity::accepted_event_id)
        .expect("Output(1) direct request is accepted");
    assert_eq!(
        super::cursor_sprites::support::retire_pending_output(
            &mut fixture.events,
            &mut delivered,
            1,
            40,
        ),
        direct_id,
        "Output(1)'s direct owner retires by its accepted event ID"
    );
    assert_eq!(
        Backend::drain_completed_present_events(&mut fixture.events.backend)
            .into_iter()
            .map(|event| event.present_id)
            .collect::<Vec<_>>(),
        [direct.candidate.present_id]
    );

    Backend::fill_rectangle(
        &mut fixture.events.backend,
        None,
        direct.candidate.src_host_xid,
        0x0000_8fda,
        0,
        0,
        width,
        height,
    )
    .expect("paint the output-sized Present source through Backend");
    Backend::copy_area(
        &mut fixture.events.backend,
        None,
        direct.candidate.src_host_xid,
        direct.target_xid,
        0,
        0,
        0,
        0,
        width,
        height,
    )
    .expect("retain output-window damage through the Present Copy fallback");
    Backend::note_present_pixmap(
        &mut fixture.events.backend,
        direct.candidate.src_pixmap_xid,
        direct.target_xid,
    );
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("compose output 0 and clear the global wake while output 1 is direct");
    fixture.events.wait_for_submitted_gpu_work();
    assert!(
        !fixture.events.backend.scene.scene_structure_dirty,
        "the scheduler clears the global wake after output 0 inspects its clean scene"
    );
    for output_idx in 0..2 {
        super::cursor_sprites::support::drain_output_events(
            &mut fixture.events,
            &mut delivered,
            output_idx,
        );
    }
    assert!(
        fixture
            .events
            .backend
            .scene
            .output_wants_compose(1, &fixture.events.backend.store),
        "output 1 retains its direct-window damage; scene_dirty={}, armed={:?}",
        fixture.events.backend.scene.scene_structure_dirty,
        fixture.events.backend.store.armed_damaged_ids()
    );
    assert!(
        !fixture
            .events
            .backend
            .scene
            .output_wants_compose(0, &fixture.events.backend.store),
        "output 0 has no scene work"
    );

    Backend::warp_pointer_root(
        &mut fixture.events.backend,
        &mut fixture.events.state,
        110,
        100,
    );
    assert!(
        Backend::next_wakeup(&fixture.events.backend)
            .is_some_and(|wakeup| wakeup <= std::time::Instant::now()),
        "Backend exposes the unsent pointer position as an immediate wake"
    );
    let before = fixture.events.recorders[&device_fd].records().len();

    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("schedule output 0 cursor motion while output 1 damage is retained");
    fixture.events.wait_for_submitted_gpu_work();

    let records = a3_records_since(fixture, device_fd, before);
    let cursor_only: Vec<_> = records
        .iter()
        .filter(|record| {
            record.kind == crate::drm::atomic::AtomicCommitKind::CursorOnly
                && super::commit_identity::accepted_event_id(record).is_some()
                && record.events.iter().any(|event| {
                    event.crtc == fixture.events.backend.platform.outputs[0].output.crtc
                })
        })
        .collect();
    assert!(
        cursor_only.len() == 1,
        "pointer motion submits one output 0 cursor-only request: cursor={cursor_only:?}, all={records:?}"
    );
    assert!(
        fixture
            .events
            .backend
            .scene
            .output_wants_compose(1, &fixture.events.backend.store),
        "output 1's retained damage remains pending"
    );

    for output_idx in 0..2 {
        super::cursor_sprites::support::drain_output_events(
            &mut fixture.events,
            &mut delivered,
            output_idx,
        );
    }

    Backend::unmap_subwindow(&mut fixture.events.backend, None, direct.target_xid)
        .expect("request Output(1) cleanup unflip through Backend");
    super::commit_identity::direct_unflip::damage_scene_root(fixture);
    let cleanup_start = fixture.events.recorders[&device_fd].records().len();
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("replace Output(1)'s direct scanout");
    fixture.events.wait_for_submitted_gpu_work();
    let cleanup = a3_records_since(fixture, device_fd, cleanup_start);
    for output_idx in 0..2 {
        super::cursor_sprites::support::drain_output_events(
            &mut fixture.events,
            &mut delivered,
            output_idx,
        );
    }
    assert!(
        fixture.events.backend.pstate.direct.outputs[1]
            .current
            .is_none(),
        "cleanup retires the Output(1) owner; state={:?}, scene_pending={}, kms_pending={}, cursor={:?}, cleanup={cleanup:?}",
        (
            fixture.events.backend.pstate.direct.outputs[1].unflip_requested,
            fixture.events.backend.pstate.direct.outputs[1]
                .unflip_outputs_to_replace
                .len(),
            fixture.events.backend.pstate.direct.outputs[1]
                .unflip_awaiting_outputs
                .len(),
        ),
        fixture.events.backend.scene.has_pending_page_flip(1),
        fixture
            .events
            .backend
            .platform
            .scanout_commit_in_flight_for_output(1),
        fixture.events.backend.scene.cursor_mode_for_outputs(&[1]),
    );
    assert!(fixture.events.backend.present_source_pins.is_empty());
    assert!(Backend::drain_completed_present_events(&mut fixture.events.backend).is_empty());
    assert_eq!(
        Backend::drain_retired_present_idle_events(&mut fixture.events.backend)
            .into_iter()
            .map(|event| event.present_id)
            .collect::<Vec<_>>(),
        [direct.candidate.present_id],
        "Output(1)'s idle notification follows its completed replacement"
    );
    Backend::set_cursor_hidden(&mut fixture.events.backend, true);
    super::commit_identity::direct_unflip::damage_scene_root(fixture);
    for _ in 0..4 {
        Backend::maybe_composite(&mut fixture.events.backend)
            .expect("detach the fixture cursor through Backend");
        fixture.events.wait_for_submitted_gpu_work();
        for output_idx in 0..2 {
            super::cursor_sprites::support::drain_output_events(
                &mut fixture.events,
                &mut delivered,
                output_idx,
            );
        }
    }
    assert!(!fixture.events.backend.platform.any_cursor_plane_visible());
    super::cursor_sprites::support::finish_atomic_fixture(atomic_fixture, &mut delivered);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: leave the queued-only owner set (composition stays suppressed / pins held).
fn queued_entry_cancelled_by_unflip_cause() {
    use yserver_core::backend::Backend;

    let Some((fixture, direct)) = a3_output_fixture(1, 1101) else {
        eprintln!("SKIP queued_entry_cancelled_by_unflip_cause: Vulkan fixture unavailable");
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    let (source_pin, fallback_pin) =
        a3_queue_first_output_entry(&mut fixture, &direct, 0x0012_3456);
    assert!(
        fixture
            .events
            .backend
            .present_source_pins
            .contains_key(&source_pin)
    );
    assert!(
        fixture
            .events
            .backend
            .present_source_pins
            .contains_key(&fallback_pin)
    );

    Backend::unmap_subwindow(&mut fixture.events.backend, None, direct.target_xid)
        .expect("unmap queued direct window through Backend");
    let owner = &fixture.events.backend.pstate.direct.outputs[1];
    assert!(
        owner.current.is_none()
            && owner.pending.is_none()
            && owner.queued_successor.is_none()
            && !owner.hold_direct
            && !owner.active(),
        "unmap cancels queued-only ownership without an unflip commit"
    );
    assert!(
        fixture
            .events
            .backend
            .present_source_pins
            .contains_key(&source_pin)
            && fixture
                .events
                .backend
                .present_source_pins
                .contains_key(&fallback_pin),
        "Copy source and target pins stay held until the GPU consumes the fallback"
    );
    assert!(!fixture.events.backend.output_is_direct(1));

    let fd = fixture.kms_fd(1);
    let before = fixture.events.recorders[&fd].records().len();
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("compose Output(1) after queued-entry cancellation");
    let crtc = fixture.events.backend.platform.outputs[1].output.crtc;
    let records = a3_records_since(&fixture, fd, before);
    assert!(
        a3_has_scene_commit_for(&records, crtc),
        "the revoked Output composes instead of suppressing scene work"
    );
    fixture.events.wait_for_submitted_gpu_work();
    a3_retire_records(&mut fixture, &records);
    assert!(!fixture.events.backend.scene.has_pending_page_flips());
    let copied = fixture
        .events
        .backend
        .backing_pixels_for_tests(direct.target_xid)
        .expect("queued fallback target remains readable")
        .2;
    assert_eq!(&copied[..3], &[0x56, 0x34, 0x12]);
    a3_assert_queued_entry_copy_completion(
        &mut fixture,
        direct.candidate.present_id,
        source_pin,
        fallback_pin,
    );
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: route the global unflip back through KmsBackendState only.
// A queued server-source-retired case is impossible: promoted server-owned sources are admitted
// only as Root candidates, while the queued first-entry owner is Output(i).
fn queued_entry_cancelled_by_global_unflip() {
    use yserver_core::backend::Backend;

    let Some((fixture, direct)) = a3_output_fixture(1, 1106) else {
        eprintln!("SKIP queued_entry_cancelled_by_global_unflip: Vulkan fixture unavailable");
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    let (source_pin, fallback_pin) =
        a3_queue_first_output_entry(&mut fixture, &direct, 0x0012_3456);
    {
        let events = fixture.events_mut();
        events
            .backend
            .rebuild_randr_state(&mut events.state, None, true);
    }

    let half = yserver_core::randr::FIXED_ONE / 2;
    fixture.events.state.randr.outputs[0].current_transform =
        yserver_core::randr::CrtcTransform::new(
            [half, 0, 0, 0, half, 0, 0, 0, yserver_core::randr::FIXED_ONE],
            None,
            Vec::new(),
        )
        .expect("valid scale transform");
    {
        let events = fixture.events_mut();
        Backend::randr_layout_changed(&mut events.backend, &mut events.state);
    }
    assert_eq!(fixture.events.backend.platform.output_transforms.len(), 1);

    let owner = &fixture.events.backend.pstate.direct.outputs[1];
    assert!(
        owner.current.is_none()
            && owner.pending.is_none()
            && owner.queued_successor.is_none()
            && !owner.hold_direct
            && !owner.active(),
        "global transform unflip cancels queued-only Output ownership"
    );
    assert!(
        fixture
            .events
            .backend
            .present_source_pins
            .contains_key(&source_pin)
            && fixture
                .events
                .backend
                .present_source_pins
                .contains_key(&fallback_pin),
        "Copy pins stay held until the fallback is consumed"
    );

    let fd = fixture.kms_fd(1);
    let before = fixture.events.recorders[&fd].records().len();
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("compose Output(1) after global queued-entry cancellation");
    fixture.events.wait_for_submitted_gpu_work();
    let crtc = fixture.events.backend.platform.outputs[1].output.crtc;
    let records = a3_records_since(&fixture, fd, before);
    assert!(
        a3_has_scene_commit_for(&records, crtc),
        "the revoked Output composes after its Copy fallback"
    );
    a3_retire_records(&mut fixture, &records);
    assert!(!fixture.events.backend.scene.has_pending_page_flips());
    let copied = fixture
        .events
        .backend
        .backing_pixels_for_tests(direct.target_xid)
        .expect("queued fallback target remains readable")
        .2;
    assert_eq!(&copied[..3], &[0x56, 0x34, 0x12]);
    a3_assert_queued_entry_copy_completion(
        &mut fixture,
        direct.candidate.present_id,
        source_pin,
        fallback_pin,
    );
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: leave the queued-only owner set (composition stays suppressed / pins held).
fn queued_entry_cancelled_by_output_removal() {
    use yserver_core::backend::Backend;

    let Some((fixture, direct)) = a3_output_fixture(1, 1102) else {
        eprintln!("SKIP queued_entry_cancelled_by_output_removal: Vulkan fixture unavailable");
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    let (source_pin, fallback_pin) =
        a3_queue_first_output_entry(&mut fixture, &direct, 0x0012_3456);
    fixture
        .events
        .backend
        .platform
        .for_tests_accept_output_power_commits();
    let (outputs, _) = Backend::randr_outputs_and_modes(&mut fixture.events.backend);
    let removed = &outputs[1];
    assert!(
        Backend::apply_crtc_config(
            &mut fixture.events.backend,
            removed.output_id,
            &removed.name,
            None,
            0,
            0,
        )
        .expect("remove Output(1) through the RandR Backend path")
    );
    assert_eq!(fixture.events.backend.platform.outputs.len(), 1);
    assert!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .active_scopes()
            .is_empty(),
        "removing the output clears its queued-only direct owner"
    );
    assert!(
        fixture
            .events
            .backend
            .present_source_pins
            .contains_key(&source_pin)
            && fixture
                .events
                .backend
                .present_source_pins
                .contains_key(&fallback_pin),
        "removed output's Copy pins stay held until fallback consumption"
    );
    a3_assert_queued_entry_copy_completion(
        &mut fixture,
        direct.candidate.present_id,
        source_pin,
        fallback_pin,
    );
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: leave the queued-only owner set (composition stays suppressed / pins held).
fn queued_entry_cancelled_by_vt_release() {
    use yserver_core::{backend::Backend, core_loop::input_inventory::InputInventory};

    let Some((fixture, direct)) = a3_output_fixture(1, 1103) else {
        eprintln!("SKIP queued_entry_cancelled_by_vt_release: Vulkan fixture unavailable");
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    let (source_pin, fallback_pin) =
        a3_queue_first_output_entry(&mut fixture, &direct, 0x0012_3456);
    fixture
        .events
        .backend
        .platform
        .for_tests_accept_output_power_commits();
    super::commit_identity::install_recording_vt_io(&mut fixture);
    Backend::begin_vt_release(&mut fixture.events.backend);
    let events = fixture.events_mut();
    Backend::finish_vt_release(
        &mut events.backend,
        &mut events.state,
        &InputInventory::default(),
    );
    assert!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .active_scopes()
            .is_empty(),
        "VT release clears queued-only direct ownership"
    );
    assert!(
        fixture
            .events
            .backend
            .present_source_pins
            .contains_key(&source_pin)
            && fixture
                .events
                .backend
                .present_source_pins
                .contains_key(&fallback_pin),
        "VT teardown retains Copy pins until fallback consumption"
    );
    a3_assert_queued_entry_copy_completion(
        &mut fixture,
        direct.candidate.present_id,
        source_pin,
        fallback_pin,
    );
    fixture.finish();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: leave the queued-only owner set (composition stays suppressed / pins held).
fn queued_entry_cancelled_by_dpms_off() {
    use yserver_core::backend::Backend;

    let Some((fixture, direct)) = a3_output_fixture(1, 1104) else {
        eprintln!("SKIP queued_entry_cancelled_by_dpms_off: Vulkan fixture unavailable");
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    let (source_pin, fallback_pin) =
        a3_queue_first_output_entry(&mut fixture, &direct, 0x0012_3456);
    fixture
        .events
        .backend
        .platform
        .for_tests_accept_output_power_commits();
    Backend::set_dpms_power(&mut fixture.events.backend, 3)
        .expect("turn outputs off through the DPMS Backend path");
    assert!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .active_scopes()
            .is_empty(),
        "DPMS off clears queued-only direct ownership"
    );
    assert!(
        fixture
            .events
            .backend
            .present_source_pins
            .contains_key(&source_pin)
            && fixture
                .events
                .backend
                .present_source_pins
                .contains_key(&fallback_pin),
        "DPMS teardown retains Copy pins until fallback consumption"
    );
    a3_assert_queued_entry_copy_completion(
        &mut fixture,
        direct.candidate.present_id,
        source_pin,
        fallback_pin,
    );
    fixture.finish();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: leave the queued-only owner set (composition stays suppressed / pins held).
fn queued_entry_cancelled_by_newer_copy_present() {
    use yserver_core::backend::Backend;

    let Some((fixture, direct)) = a3_output_fixture(1, 1105) else {
        eprintln!("SKIP queued_entry_cancelled_by_newer_copy_present: Vulkan fixture unavailable");
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    let (source_pin, fallback_pin) =
        a3_queue_first_output_entry(&mut fixture, &direct, 0x0012_3456);
    let mut newer = fixture.install_direct_output_present(1, 1106, 0).candidate;
    newer.dst_window_xid = direct.target_xid;
    newer.paint_dst_host_xid = direct.target_xid;
    newer.completion_dst_host_xid = direct.target_xid;
    newer.valid_region_xid = 0xdead;
    Backend::fill_rectangle(
        &mut fixture.events.backend,
        None,
        newer.src_host_xid,
        0x0065_4321,
        0,
        0,
        newer.src_width,
        newer.src_height,
    )
    .expect("paint newer Copy Present source through Backend");
    let mut newer_event = direct.event.clone();
    newer_event.present_id = newer.present_id;
    newer_event.serial = u32::try_from(newer.present_id).expect("test Present id fits serial");
    newer_event.host_xid = newer.src_host_xid;
    newer_event.dst_host_xid = newer.paint_dst_host_xid;
    assert!(
        !Backend::try_present_direct(&mut fixture.events.backend, newer, newer_event)
            .expect("newer ineligible Present takes the Copy path through Backend"),
        "newer Present is not accepted as direct"
    );
    Backend::copy_area(
        &mut fixture.events.backend,
        None,
        newer.src_host_xid,
        newer.paint_dst_host_xid,
        0,
        0,
        0,
        0,
        newer.src_width,
        newer.src_height,
    )
    .expect("run newer Present Copy through Backend");
    Backend::note_present_pixmap(
        &mut fixture.events.backend,
        newer.src_pixmap_xid,
        newer.paint_dst_host_xid,
    );
    let owner = &fixture.events.backend.pstate.direct.outputs[1];
    assert!(
        owner.current.is_none()
            && owner.pending.is_none()
            && owner.queued_successor.is_none()
            && !owner.hold_direct
            && !owner.active(),
        "newer Copy Present cancels the queued-only owner"
    );
    assert!(
        fixture
            .events
            .backend
            .present_source_pins
            .contains_key(&source_pin)
            && fixture
                .events
                .backend
                .present_source_pins
                .contains_key(&fallback_pin),
        "queued frame pins remain until its Copy is consumed"
    );
    assert!(!fixture.events.backend.output_is_direct(1));

    let fd = fixture.kms_fd(1);
    let before = fixture.events.recorders[&fd].records().len();
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("compose after newer Copy Present revoked queued ownership");
    let crtc = fixture.events.backend.platform.outputs[1].output.crtc;
    let records = a3_records_since(&fixture, fd, before);
    assert!(a3_has_scene_commit_for(&records, crtc));
    fixture.events.wait_for_submitted_gpu_work();
    a3_retire_records(&mut fixture, &records);
    assert!(!fixture.events.backend.scene.has_pending_page_flips());
    let copied = fixture
        .events
        .backend
        .backing_pixels_for_tests(direct.target_xid)
        .expect("newer Copy target remains readable")
        .2;
    assert_eq!(&copied[..3], &[0x21, 0x43, 0x65]);
    a3_assert_queued_entry_copy_completion(
        &mut fixture,
        direct.candidate.present_id,
        source_pin,
        fallback_pin,
    );
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: retire the direct owner using the kernel flip clock after freezing.
fn direct_scanout_retires_on_frozen_crtc() {
    use std::{cell::Cell, rc::Rc, time::Instant};
    use yserver_core::{backend::Backend, server::ServerState};
    use yserver_protocol::x11::ResourceId;

    let Some((fixture, direct)) = a3_output_fixture(1, 0x7d10) else {
        eprintln!("SKIP direct_scanout_retires_on_frozen_crtc: Vulkan fixture unavailable");
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    let mut state = ServerState::new();
    let output = &fixture.events.backend.platform.outputs[1];
    a3_add_existing_window_to_state(
        &mut state,
        ResourceId(0x7d11),
        direct.target_xid,
        output.x as i16,
        output.y as i16,
        output.width,
        output.height,
        true,
    );
    Backend::sync_top_level_order(&mut fixture.events.backend, &state);
    let fd = fixture.kms_fd(1);
    let source = fixture.events.event_sources[&fd].clone();
    let key = crate::kms::render::platform::CrtcKey::for_output(
        &fixture.events.backend.platform.outputs[1],
    );
    let records = a3_admit_output_owner(&mut fixture, &direct, 1);
    let projected = Backend::randr_outputs_and_modes(&mut fixture.events.backend).0;
    let crtc_id = projected[1].crtc_id;
    source.set_legacy_vblank(Ok(5));
    fixture
        .events
        .backend
        .pstate
        .crtc_queue_sequence_unsupported_devices
        .insert(key.device_key);
    fixture
        .events
        .backend
        .platform
        .vblank_fallback
        .pipes
        .insert(key, 1);
    let base = Instant::now();
    let now = Rc::new(Cell::new(base));
    let clock = Rc::clone(&now);
    fixture
        .events
        .backend
        .for_tests_set_monotonic_clock(Rc::new(move || clock.get()));
    let backend = &mut fixture.events.backend;
    assert!(
        Backend::arm_present_shared_wake(backend, crtc_id)
            .expect("direct scanout CRTC kernel wake")
    );
    now.set(base + crate::kms::render::backend::KmsBackend::FROZEN_VBLANK_WINDOW);
    backend.poll_deferred_input(&mut state);
    let frozen = backend.platform.vblank_fallback.frozen[&key];

    now.set(frozen.anchor_at + frozen.interval);
    for record in &records {
        let Some(event_id) = super::commit_identity::accepted_event_id(record) else {
            continue;
        };
        for event in &record.events {
            fixture.events.queue_commit_completion(
                fd,
                event.crtc,
                event_id,
                60,
                std::time::Duration::from_millis(600),
            );
        }
    }
    {
        let events = fixture.events_mut();
        Backend::on_page_flip_ready(&mut events.backend, &mut events.state, fd);
    }
    let completed = Backend::drain_completed_present_events(&mut fixture.events.backend);
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].present_id, direct.candidate.present_id);
    let completion_clock = completed[0]
        .completion_clock
        .expect("direct retirement carries its completion clock");
    assert_eq!(completion_clock.msc, frozen.anchor_msc + 1);
    assert_eq!(
        completion_clock.ust,
        frozen.anchor_ust + u64::try_from(frozen.interval.as_micros()).unwrap()
    );
    assert_eq!(
        Backend::present_get_ust_msc(&fixture.events.backend, crtc_id).0,
        completion_clock.msc
    );
    Backend::unmap_subwindow(&mut fixture.events.backend, None, direct.target_xid)
        .expect("retire the held Output owner through Backend");
    let cleanup = a3_tick(&mut fixture);
    assert!(a3_has_scene_commit_for(
        &cleanup,
        fixture.events.backend.platform.outputs[1].output.crtc
    ));
    a3_retire_records(&mut fixture, &cleanup);
    assert!(
        fixture.events.backend.pstate.direct.outputs[1]
            .current
            .is_none(),
        "the Output owner's direct buffer retires on its replacement flip"
    );
    let _ = Backend::drain_completed_present_events(&mut fixture.events.backend);
    let _ = Backend::drain_retired_present_idle_events(&mut fixture.events.backend);
    fixture.finish_after_normal_drain();
}
