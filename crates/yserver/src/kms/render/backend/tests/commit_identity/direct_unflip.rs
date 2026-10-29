//! Direct-frame and unflip retirement by accepted atomic commit identity.

use super::{
    atomic_events::{complete_recorded_attempt, pending_ids},
    recording_scene::{DirectPresentSetup, P1SceneRoute, RecordingSceneFixture},
    support::accepted_event_id,
};
use crate::drm::atomic::{
    AtomicCommitEvent, AtomicCommitKind, AtomicCommitRequest,
    tests_support::{AtomicCommitRecord, ScriptedResult},
};
use ::drm::control::AtomicCommitFlags;
use std::{
    collections::{HashMap, HashSet},
    os::fd::RawFd,
    time::{Duration, Instant},
};
use yserver_core::backend::Backend;

const DIRECT_PROBATION: usize = 8;
const WAIT_DEADLINE: Duration = Duration::from_secs(5);

pub(crate) fn direct_fixture(
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

pub(crate) fn fd(fixture: &RecordingSceneFixture) -> RawFd {
    fixture.kms_fd(0)
}

pub(crate) fn record_count(fixture: &RecordingSceneFixture, device_fd: RawFd) -> usize {
    fixture.events.recorders[&device_fd].records().len()
}

pub(crate) fn records_since(
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

pub(super) fn enter_async_tearing(
    fixture: &mut RecordingSceneFixture,
    direct: &DirectPresentSetup,
) {
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
    assert!(fixture.events.backend.pstate.direct.root().unflip_requested);
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

pub(crate) fn output_owner_frame(
    fixture: &mut RecordingSceneFixture,
    direct: &DirectPresentSetup,
    output_idx: usize,
    target: crate::kms::render::backend::ScanoutM0Target,
    fallback_override: Option<crate::kms::render::store::DrawableId>,
    paint_host_override: Option<u32>,
) -> crate::kms::render::backend::DirectPresentFrame {
    use crate::kms::render::{backend::DirectPresentFrame, target::PaintTarget};
    use std::collections::HashSet;

    let backend = &mut fixture.events.backend;
    let output = &backend.platform.outputs[output_idx].output;
    let mut candidate = direct.candidate;
    candidate.crtc_id = u32::from(output.crtc);
    candidate.crtc_epoch = backend.present_crtc_clock_epoch(candidate.crtc_id);
    if let Some(paint_host_xid) = paint_host_override {
        candidate.paint_dst_host_xid = paint_host_xid;
    }
    let fallback_id = fallback_override.unwrap_or_else(|| {
        backend
            .store
            .lookup(direct.target_xid)
            .expect("direct setup has a fallback drawable")
    });
    let source_pin = backend.pin_direct_source(direct.source_id);
    let fallback_target_pin = backend.pin_direct_source(fallback_id);
    let mut event = direct.event.clone();
    event.crtc_id = candidate.crtc_id;
    event.crtc_epoch = candidate.crtc_epoch;
    DirectPresentFrame {
        source_pin,
        fallback_target_pin,
        source_id: direct.source_id,
        candidate,
        target,
        present_received_at: Instant::now(),
        present_received_monotonic: None,
        fallback_target: PaintTarget::new(fallback_id, (0, 0), None, 24),
        event,
        completion_output_idx: output_idx,
        completion_clock: None,
        awaiting_outputs: HashSet::new(),
        accepted_event_id: None,
        async_outcome: crate::drm::async_flip::AsyncOutcome::NotRequested,
        in_fence: None,
        fence_tally: None,
    }
}

pub(crate) fn seed_output_owner_successor(
    fixture: &mut RecordingSceneFixture,
    direct: &DirectPresentSetup,
    output_idx: usize,
    target: crate::kms::render::backend::ScanoutM0Target,
    fallback_override: Option<crate::kms::render::store::DrawableId>,
    paint_host_override: Option<u32>,
) {
    let frame = output_owner_frame(
        fixture,
        direct,
        output_idx,
        target,
        fallback_override,
        paint_host_override,
    );
    let output = &fixture.events.backend.platform.outputs[output_idx].output;
    let candidate = frame.candidate;
    fixture.events.backend.pstate.scanout_m1.entries.insert(
        (
            direct.source_id,
            crate::kms::render::backend::DirectScope::Output(output_idx),
        ),
        crate::kms::render::backend::ScanoutM1ProbeEntry::accepted_with_shape(
            crate::drm::modeset::DirectScanoutProbeFramebuffer::for_tests(
                super::recording_scene::raw(
                    0x8a00
                        + u32::try_from(candidate.present_id).expect("test Present id fits")
                        + u32::try_from(output_idx).expect("test output index fits"),
                ),
            ),
            crate::drm::async_flip::AsyncShape {
                fourcc: 0x3432_5258,
                modifier: 0,
                pitch: u32::from(candidate.src_width) * 4,
                width: u32::from(candidate.src_width),
                height: u32::from(candidate.src_height),
                crtcs: vec![output.crtc],
                direct: true,
            },
        ),
    );
    let owner = &mut fixture.events.backend.pstate.direct.outputs[output_idx];
    assert!(!owner.active(), "output owner slot starts empty");
    owner.queued_successor = Some(frame);
    owner.hold_direct = true;
}

pub(super) fn submit_output_present_via_backend(
    fixture: &mut RecordingSceneFixture,
    direct: &DirectPresentSetup,
    output_idx: usize,
) -> AtomicCommitRecord {
    submit_output_present_via_backend_for_row(fixture, direct, output_idx, "Output Present")
}

fn submit_output_present_via_backend_for_row(
    fixture: &mut RecordingSceneFixture,
    direct: &DirectPresentSetup,
    output_idx: usize,
    row: &str,
) -> AtomicCommitRecord {
    fixture
        .events
        .backend
        .for_tests_set_scanout_m1_prober(super::super::present_scanout::record_scanout_m1_probe);
    Backend::note_present_scanout_candidate(&mut fixture.events.backend, direct.candidate);

    let device_fd = fd(fixture);
    let start = record_count(fixture, device_fd);
    let probation = usize::from(crate::kms::render::backend::SCANOUT_M2_ELIGIBLE_ROOT_PROBATION);
    for attempt in 0..probation {
        let entered = Backend::try_present_direct(
            &mut fixture.events.backend,
            direct.candidate,
            direct.event.clone(),
        )
        .expect("Output Present through Backend");
        assert_eq!(
            entered,
            attempt + 1 == probation,
            "{row}: Output({output_idx}) enters after normal probation"
        );
    }

    let crtc = fixture.events.backend.platform.outputs[output_idx]
        .output
        .crtc;
    records_since(fixture, device_fd, start)
        .into_iter()
        .find(|record| {
            accepted_event_id(record).is_some()
                && record.events.len() == 1
                && record.events[0].crtc == crtc
        })
        .unwrap_or_else(|| panic!("Output({output_idx}) Present produced a direct commit"))
}

fn reconfigure_root_present_as_output(
    fixture: &mut RecordingSceneFixture,
    root: &DirectPresentSetup,
    output_idx: usize,
    present_id: u64,
) -> DirectPresentSetup {
    use crate::kms::render::store::DrawableKind;
    use yserver_core::host_x11::HostSubwindowConfig;

    let layout = &fixture.events.backend.platform.outputs[output_idx];
    let (x, y, width, height) = (layout.x, layout.y, layout.width, layout.height);
    Backend::configure_subwindow(
        &mut fixture.events.backend,
        None,
        root.target_xid,
        HostSubwindowConfig {
            x: Some(i16::try_from(x).expect("output x fits X11 coordinates")),
            y: Some(i16::try_from(y).expect("output y fits X11 coordinates")),
            width: Some(width),
            height: Some(height),
            ..Default::default()
        },
    )
    .expect("resize direct window through Backend");

    let source_xid = 0x7e00 + u32::try_from(present_id).expect("Present id fits XID");
    let mut source_storage = fixture
        .events
        .backend
        .platform
        .gpu
        .allocate_drawable_storage(width, height, 24)
        .expect("allocate output-sized Present source");
    // Direct sources model imported client buffers.
    source_storage.imported_dmabuf = Some(super::super::imported_dmabuf_for_test(width, height));
    let source_id = fixture
        .events
        .backend
        .store
        .allocate(source_xid, DrawableKind::Pixmap, 24, false, source_storage)
        .expect("register output-sized Present source");
    Backend::put_image(
        &mut fixture.events.backend,
        None,
        source_xid,
        24,
        1,
        1,
        0,
        0,
        &[0, 0, 0, 0],
    )
    .expect("initialize output-sized Present source through Backend");
    fixture.adopt_drawables([source_xid]);

    let crtc_id = u32::from(
        fixture.events.backend.platform.outputs[output_idx]
            .output
            .crtc,
    );
    let crtc_epoch = fixture.events.backend.present_crtc_clock_epoch(crtc_id);
    let mut candidate = root.candidate;
    candidate.present_id = present_id;
    candidate.crtc_id = crtc_id;
    candidate.crtc_epoch = crtc_epoch;
    candidate.src_pixmap_xid = source_xid;
    candidate.src_host_xid = source_xid;
    candidate.dst_window_xid = root.target_xid;
    candidate.paint_dst_host_xid = root.target_xid;
    candidate.completion_dst_host_xid = root.target_xid;
    candidate.src_width = width;
    candidate.src_height = height;

    let mut event = root.event.clone();
    event.serial = u32::try_from(present_id).expect("Present serial fits");
    event.host_xid = source_xid;
    event.dst_host_xid = root.target_xid;
    event.present_id = present_id;
    event.crtc_id = crtc_id;
    event.crtc_epoch = crtc_epoch;
    DirectPresentSetup {
        candidate,
        event,
        source_id,
        target_xid: root.target_xid,
    }
}

fn compose_and_complete_outputs(
    fixture: &mut RecordingSceneFixture,
    device_fd: RawFd,
    expected_outputs: &[usize],
) {
    let start = record_count(fixture, device_fd);
    damage_scene_root(fixture);
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("compose outputs after direct unflip through Backend");
    let scene_records = records_since(fixture, device_fd, start)
        .into_iter()
        .filter(|record| {
            record.kind == crate::drm::atomic::AtomicCommitKind::Scene
                && accepted_event_id(record).is_some()
        })
        .collect::<Vec<_>>();
    let output_crtcs = expected_outputs
        .iter()
        .map(|&output_idx| {
            fixture.events.backend.platform.outputs[output_idx]
                .output
                .crtc
        })
        .collect::<HashSet<_>>();
    let composed_crtcs = scene_records
        .iter()
        .flat_map(|record| record.events.iter().map(|event| event.crtc))
        .filter(|crtc| output_crtcs.contains(crtc))
        .collect::<HashSet<_>>();
    assert_eq!(
        composed_crtcs, output_crtcs,
        "the scheduling pass must compose every output before re-entry"
    );
    for record in &scene_records {
        complete_recorded_attempt(
            fixture,
            device_fd,
            record,
            accepted_event_id(record).expect("accepted scene record has an identity"),
            84,
        );
    }
    assert!(!fixture.events.backend.scene.has_pending_page_flips());
}

fn assert_plane_source_origin(
    fixture: &RecordingSceneFixture,
    record: &AtomicCommitRecord,
    output_idx: usize,
    row: &str,
) {
    let output = &fixture.events.backend.platform.outputs[output_idx].output;
    for (property, name) in [
        (output.plane_src_x_prop, "SRC_X"),
        (output.plane_src_y_prop, "SRC_Y"),
    ] {
        let value = record
            .properties
            .iter()
            .find(|update| update.object == output.plane.into() && update.property == property)
            .unwrap_or_else(|| panic!("{row}: output {output_idx} commit is missing {name}"))
            .value;
        assert_eq!(
            value, 0,
            "{row}: output {output_idx} {name} must start at the pixmap origin"
        );
    }
}

fn assert_atomic_record_touches_output_only(
    fixture: &RecordingSceneFixture,
    record: &AtomicCommitRecord,
    output_idx: usize,
    row: &str,
) {
    let output = &fixture.events.backend.platform.outputs[output_idx].output;
    let other = &fixture.events.backend.platform.outputs[1 - output_idx].output;
    assert_eq!(
        record.events.len(),
        1,
        "{row}: commit has only its output event"
    );
    assert_eq!(
        record.events[0].crtc, output.crtc,
        "{row}: commit uses its output CRTC"
    );
    assert!(
        record.properties.iter().any(|update| {
            update.property == output.plane_fb_id_prop && update.object == output.plane.into()
        }),
        "{row}: commit updates its output framebuffer"
    );
    assert!(
        record.properties.iter().any(|update| {
            update.property == output.plane_crtc_id_prop && update.object == output.plane.into()
        }),
        "{row}: commit updates its output CRTC"
    );
    let other_plane_properties = [
        other.plane_fb_id_prop,
        other.plane_crtc_id_prop,
        other.plane_src_x_prop,
        other.plane_src_y_prop,
        other.plane_src_w_prop,
        other.plane_src_h_prop,
        other.plane_crtc_x_prop,
        other.plane_crtc_y_prop,
        other.plane_crtc_w_prop,
        other.plane_crtc_h_prop,
    ];
    for property in other_plane_properties {
        assert!(
            !record
                .properties
                .iter()
                .any(|update| update.object == other.plane.into() && update.property == property),
            "{row}: commit leaves the other output's plane property untouched"
        );
    }
    for property in [
        other.crtc_out_fence_ptr_prop,
        other.crtc_vrr_enabled_prop,
        other.crtc_gamma_lut_prop,
        other.crtc_degamma_lut_prop,
        other.crtc_ctm_prop,
    ]
    .into_iter()
    .flatten()
    {
        assert!(
            !record
                .properties
                .iter()
                .any(|update| update.object == other.crtc.into() && update.property == property),
            "{row}: commit leaves the other output's CRTC property untouched"
        );
    }
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: D01 uses layout.x/y as source origin; D08 drains Root only;
// P14 applies the homogeneous-refresh rule to Output; D12 reads Root state
// for every Present output query.
fn output_direct_lifecycle() {
    // D01/P14: output 1 keeps its nonzero desktop origin and both rows use
    // the original mixed-refresh 75/240 Hz topology.
    for (row, output_idx, present_id) in [
        // P14: Output admission remains valid on the mixed-refresh topology.
        ("output_0_zero_origin", 0, 203),
        // D01/P14: the output-sized source starts at zero despite its layout origin.
        ("output_1_nonzero_origin", 1, 810),
    ] {
        let Some((fixture, direct)) =
            super::super::present_scanout::output_admission_fixture(output_idx, present_id)
        else {
            eprintln!("SKIP output_direct_lifecycle: Vulkan fixture unavailable row={row}");
            return;
        };
        let mut fixture = fixture.cleanup_guard();
        let device_fd = fd(&fixture);
        let own_crtc = fixture.events.backend.platform.outputs[output_idx]
            .output
            .crtc;
        let other_idx = 1 - output_idx;
        let other_crtc = fixture.events.backend.platform.outputs[other_idx]
            .output
            .crtc;
        assert_eq!(
            fixture.events.backend.platform.outputs[0]
                .output
                .picked
                .vrefresh,
            75,
            "{row}: output 0 retains the mixed-refresh control"
        );
        assert_eq!(
            fixture.events.backend.platform.outputs[1]
                .output
                .picked
                .vrefresh,
            240,
            "{row}: output 1 retains the mixed-refresh control"
        );
        if output_idx == 1 {
            assert_ne!(
                fixture.events.backend.platform.outputs[1].x, 0,
                "{row}: output 1 begins at a nonzero desktop origin"
            );
        }

        // D12: establish an output MSC through a real scheduled scene commit
        // before asking the core Present request path to park at next-MSC.
        let seed_start = record_count(&fixture, device_fd);
        damage_scene_root(&mut fixture);
        Backend::maybe_composite(&mut fixture.events.backend)
            .expect("seed output clocks through the scheduling pass");
        fixture.events.wait_for_submitted_gpu_work();
        let seed_records = accepted_records(&records_since(&fixture, device_fd, seed_start));
        assert!(
            seed_records.iter().any(|record| {
                record.kind == AtomicCommitKind::Scene
                    && record.events.iter().any(|event| event.crtc == own_crtc)
            }),
            "{row}: the scheduler records an initial scene flip for the queried output"
        );
        for (index, record) in seed_records
            .iter()
            .filter(|record| record.kind == AtomicCommitKind::Scene)
            .enumerate()
        {
            complete_recorded_attempt(
                &mut fixture,
                device_fd,
                record,
                accepted_event_id(record).expect("accepted clock seed has an ID"),
                100 + u32::try_from(index).expect("seed index fits"),
            );
        }
        assert!(
            Backend::present_get_ust_msc(&fixture.events.backend, u32::from(own_crtc)).0 > 0,
            "{row}: the queried output has a recorded MSC sample"
        );

        let direct_record =
            submit_output_present_via_backend_for_row(&mut fixture, &direct, output_idx, row);
        let direct_id = accepted_event_id(&direct_record)
            .unwrap_or_else(|| panic!("{row}: normal Output Present has an accepted event ID"));
        assert_atomic_record_touches_output_only(&fixture, &direct_record, output_idx, row);
        assert_plane_source_origin(&fixture, &direct_record, output_idx, row);
        assert!(
            fixture.events.backend.pstate.direct.outputs[output_idx]
                .pending
                .is_some(),
            "{row}: the normal Output admission owns its pending direct frame"
        );

        submit_next_msc_present_and_assert_parked(&mut fixture, &direct, output_idx, row);

        let compose_start = record_count(&fixture, device_fd);
        damage_scene_root(&mut fixture);
        Backend::maybe_composite(&mut fixture.events.backend)
            .expect("compose the unrelated output through the scheduling pass");
        fixture.events.wait_for_submitted_gpu_work();
        let concurrent_scene = accepted_records(&records_since(&fixture, device_fd, compose_start))
            .into_iter()
            .filter(|record| record.kind == AtomicCommitKind::Scene)
            .collect::<Vec<_>>();
        assert!(
            concurrent_scene
                .iter()
                .any(|record| { record.events.iter().any(|event| event.crtc == other_crtc) }),
            "{row}: the other output composes while this direct Present is pending"
        );
        assert!(
            concurrent_scene
                .iter()
                .all(|record| record.events.iter().all(|event| event.crtc != own_crtc)),
            "{row}: concurrent composition leaves the direct output untouched"
        );
        for record in &concurrent_scene {
            complete_recorded_attempt(
                &mut fixture,
                device_fd,
                record,
                accepted_event_id(record).expect("accepted concurrent scene has an ID"),
                110,
            );
        }

        let clock_msc = if output_idx == 0 { 75 } else { 240 };
        complete_recorded_attempt(
            &mut fixture,
            device_fd,
            &direct_record,
            direct_id,
            clock_msc,
        );
        assert!(
            fixture.events.backend.pstate.direct.outputs[output_idx]
                .current
                .is_some(),
            "{row}: the direct frame is current after its accepted page flip"
        );
        assert_eq!(
            fixture.events.backend.pstate.direct.active_scopes(),
            [crate::kms::render::backend::DirectScope::Output(output_idx)],
            "{row}: only the selected output owns direct scanout"
        );

        Backend::unmap_subwindow(&mut fixture.events.backend, None, direct.target_xid)
            .expect("unmap the direct target through Backend");
        let replacement_start = record_count(&fixture, device_fd);
        Backend::maybe_composite(&mut fixture.events.backend)
            .expect("submit the output's composed replacement through the scheduler");
        fixture.events.wait_for_submitted_gpu_work();
        let replacement = accepted_records(&records_since(&fixture, device_fd, replacement_start))
            .into_iter()
            .find(|record| record.events.len() == 1 && record.events[0].crtc == own_crtc)
            .unwrap_or_else(|| panic!("{row}: the output's composed replacement is accepted"));
        assert_atomic_record_touches_output_only(&fixture, &replacement, output_idx, row);
        let replacement_id = accepted_event_id(&replacement)
            .unwrap_or_else(|| panic!("{row}: the composed replacement has an accepted ID"));
        assert_eq!(
            fixture.events.backend.pstate.direct.outputs[output_idx].unflip_awaiting_outputs,
            HashMap::from([(output_idx, replacement_id)]),
            "{row}: the direct owner remains until its output replacement retires"
        );
        complete_recorded_attempt(
            &mut fixture,
            device_fd,
            &replacement,
            replacement_id,
            clock_msc + 1,
        );
        assert!(
            !fixture.events.backend.pstate.direct.outputs[output_idx].active(),
            "{row}: the output owner stops after replacement retirement"
        );
        assert!(
            fixture.events.backend.present_source_pins.is_empty(),
            "{row}: replacement retirement releases direct source pins"
        );
        assert!(
            fixture
                .events
                .backend
                .pstate
                .direct
                .active_scopes()
                .is_empty(),
            "{row}: no direct owner remains after replacement retirement"
        );

        // D08: both notifications remain available after the output owner
        // stops and each stream drains exactly once.
        let completed = Backend::drain_completed_present_events(&mut fixture.events.backend);
        assert_eq!(
            completed
                .iter()
                .map(|event| event.present_id)
                .collect::<Vec<_>>(),
            [direct.candidate.present_id],
            "{row}: output Complete survives owner stop and drains once"
        );
        assert_eq!(
            completed[0].completion_clock,
            Some(yserver_core::backend::PresentClockSample {
                msc: u64::from(clock_msc),
                ust: u64::from(clock_msc) * 10_000,
                source: yserver_core::backend::PresentClockSource::PageFlip,
            }),
            "{row}: CompleteNotify carries the selected output's page-flip clock"
        );
        let idled = Backend::drain_retired_present_idle_events(&mut fixture.events.backend);
        assert_eq!(
            idled
                .iter()
                .map(|event| event.present_id)
                .collect::<Vec<_>>(),
            [direct.candidate.present_id],
            "{row}: output Idle survives owner stop and drains once"
        );
        assert!(
            Backend::drain_completed_present_events(&mut fixture.events.backend).is_empty(),
            "{row}: output Complete does not drain twice"
        );
        assert!(
            Backend::drain_retired_present_idle_events(&mut fixture.events.backend).is_empty(),
            "{row}: output Idle does not drain twice"
        );
        fixture.finish_after_normal_drain();
    }
}

pub(crate) fn damage_scene_root(fixture: &mut RecordingSceneFixture) {
    let backend = &fixture.events.backend;
    let root = backend.core.window_id;
    let width = backend.platform.fb_w;
    let height = backend.platform.fb_h;
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
    .expect("damage scene root through Backend");
}

fn submit_next_msc_present_and_assert_parked(
    fixture: &mut RecordingSceneFixture,
    direct: &DirectPresentSetup,
    output_idx: usize,
    row: &str,
) {
    use std::sync::{Arc, Mutex, atomic::AtomicU16};
    use yserver_core::{
        core_loop::process_request::process_request,
        randr::{RR_ROTATE_0, RandrOutput, RandrState},
        resources::ROOT_WINDOW,
        server::ClientState,
        transport::Transport,
    };
    use yserver_protocol::x11::{
        ClientByteOrder, ClientId, CreatePixmapRequest, CreateWindowRequest, RequestHeader,
        ResourceId, SequenceNumber, present as x11present,
    };

    const CLIENT_ID: u32 = 0x31;
    const WINDOW_ID: u32 = 0x0031_0001;
    const PIXMAP_ID: u32 = 0x0031_0002;

    let layout = &fixture.events.backend.platform.outputs[output_idx];
    let crtc_id = u32::from(layout.output.crtc);
    let current_msc = Backend::present_get_ust_msc(&fixture.events.backend, crtc_id).0;
    assert!(
        current_msc > 0,
        "{row}: the output has a recorded MSC sample"
    );

    let state = &mut fixture.events.state;
    let (writer, _peer) = Transport::pair().expect("Present test client transport");
    state.clients.insert(
        CLIENT_ID,
        ClientState {
            writer: Arc::new(Mutex::new(writer)),
            is_local: true,
            fd_passing: true,
            internal: false,
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
            outbound: std::collections::VecDeque::new(),
            watching_writable: false,
            write_failed: false,
            gone: false,
            focused_window: ROOT_WINDOW,
            reader_control: None,
        },
    );
    state.randr = RandrState::from_outputs(
        1,
        vec![RandrOutput {
            name: format!("test-{output_idx}"),
            output_id: 0x31_0001,
            crtc_id,
            mode_id: 0x31_0002,
            connected: true,
            x: 0,
            y: 0,
            width: layout.width,
            height: layout.height,
            vrefresh: 60,
            timing: None,
            mm_width: 0,
            mm_height: 0,
            mode_ids: vec![0x31_0002],
            num_preferred: 1,
            pending_transform: Default::default(),
            current_transform: Default::default(),
            rotation: RR_ROTATE_0,
        }],
    );
    state.resources.create_window(
        ClientId(CLIENT_ID),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(WINDOW_ID),
            parent: ROOT_WINDOW,
            width: layout.width,
            height: layout.height,
            class: 1,
            visual: yserver_core::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(WINDOW_ID));
    state
        .resources
        .window_mut(ResourceId(WINDOW_ID))
        .expect("Present destination exists")
        .host_xid = yserver_core::backend::WindowHandle::from_raw(direct.target_xid);
    state.resources.create_pixmap(
        ClientId(CLIENT_ID),
        CreatePixmapRequest {
            depth: 24,
            pixmap: ResourceId(PIXMAP_ID),
            drawable: ResourceId(WINDOW_ID),
            width: layout.width,
            height: layout.height,
        },
    );
    assert!(
        state.resources.set_pixmap_host_xid(
            ResourceId(PIXMAP_ID),
            yserver_core::backend::PixmapHandle::from_raw(direct.candidate.src_host_xid)
                .expect("Present source host XID"),
        )
    );

    let mut body = vec![0; 68];
    body[0..4].copy_from_slice(&WINDOW_ID.to_le_bytes());
    body[4..8].copy_from_slice(&PIXMAP_ID.to_le_bytes());
    body[24..28].copy_from_slice(&crtc_id.to_le_bytes());
    process_request(
        state,
        &mut fixture.events.backend,
        ClientId(CLIENT_ID),
        SequenceNumber(1),
        RequestHeader {
            opcode: 145,
            data: x11present::PIXMAP,
            length_units: 18,
        },
        &body,
        None,
    )
    .expect("dispatch next-MSC Present through the core request path");

    let present_id = {
        let (present_id, pending) = state.present_pending_exec.iter().next().unwrap_or_else(|| {
            panic!("{row}: next-MSC Present parks while its output flip is pending")
        });
        assert_eq!(
            state.present_pending_exec.len(),
            1,
            "{row}: only this request is parked"
        );
        assert_eq!(
            pending.pending.crtc_id, crtc_id,
            "{row}: parked request uses the owned CRTC"
        );
        assert_eq!(
            pending.pending.effective_target_msc,
            Some(current_msc + 1),
            "{row}: parked request targets the next sampled MSC"
        );
        assert!(pending.source_ready, "{row}: parked source is ready");
        assert_eq!(
            pending.wait_id, None,
            "{row}: parking is due to output work, not source wait"
        );
        *present_id
    };

    let pending = state
        .present_pending_exec
        .remove(&present_id)
        .expect("parked test request is removed during fixture cleanup");
    if let Some(pin) = pending.pin {
        Backend::release_present_source(&mut fixture.events.backend, pin);
    }
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: keep scanout_commit_in_flight_for_outputs over every output in queued dispatch.
fn queued_successor_not_blocked_by_scene_flip_on_other_output() {
    let Some((mut fixture, direct)) = direct_fixture(2, P1SceneRoute::Shared, 209, 0) else {
        return;
    };
    let device_fd = fd(&fixture);
    let initial = record_count(&fixture, device_fd);
    damage_scene_root(&mut fixture);
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("submit composed scene frames before the queued direct successor");
    let scene_records = records_since(&fixture, device_fd, initial);
    let output0_scene = scene_records
        .iter()
        .find(|record| {
            record.kind == crate::drm::atomic::AtomicCommitKind::Scene
                && record.events.len() == 1
                && record.events[0].crtc == fixture.events.backend.platform.outputs[0].output.crtc
        })
        .expect("output 0 scene flip is in flight");
    let output1_scene = scene_records
        .iter()
        .find(|record| {
            record.kind == crate::drm::atomic::AtomicCommitKind::Scene
                && record.events.len() == 1
                && record.events[0].crtc == fixture.events.backend.platform.outputs[1].output.crtc
        })
        .expect("output 1 scene flip is available for retirement");
    complete_recorded_attempt(
        &mut fixture,
        device_fd,
        output1_scene,
        accepted_event_id(output1_scene).expect("output 1 scene ID"),
        60,
    );
    assert!(fixture.events.backend.scene.has_pending_page_flip(0));
    assert!(!fixture.events.backend.scene.has_pending_page_flip(1));

    seed_output_owner_successor(
        &mut fixture,
        &direct,
        1,
        crate::kms::render::backend::ScanoutM0Target::Unredirected,
        None,
        None,
    );
    let before_successor = record_count(&fixture, device_fd);
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("dispatch output 1's queued direct successor");
    let successor = records_since(&fixture, device_fd, before_successor)
        .into_iter()
        .find(|record| accepted_event_id(record).is_some())
        .expect("queued successor submitted while output 0 scene flip is in flight");
    assert_eq!(successor.events.len(), 1);
    assert_eq!(
        successor.events[0].crtc,
        fixture.events.backend.platform.outputs[1].output.crtc
    );
    assert!(fixture.events.backend.scene.has_pending_page_flip(0));
    assert!(
        fixture.events.backend.pstate.direct.outputs[1]
            .pending
            .is_some()
    );
    assert!(
        fixture.events.backend.pstate.direct.outputs[1]
            .queued_successor
            .is_none()
    );
    assert!(
        output0_scene
            .events
            .iter()
            .all(|event| event.crtc == fixture.events.backend.platform.outputs[0].output.crtc)
    );
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: D04 retires the first active owner instead of the output-selected
// owner; D05 unflips the first owner when the second enters.
fn independent_output_owners() {
    // D04: the second output retires first. D05: reverse entry and retirement
    // order, with the original per-CRTC MSC samples retained.
    for (row, present_ids, entry_order, retirement_order, clocks) in [
        // D04: retirement is selected by output even when Output(1) retires first.
        (
            "retire_output_1_first",
            [205, 206],
            [0, 1],
            [1, 0],
            [71, 61],
        ),
        // D05: entering Output(0) second leaves the first owner active.
        (
            "reverse_entry_and_retirement",
            [211, 212],
            [1, 0],
            [0, 1],
            [72, 61],
        ),
    ] {
        let Some(mut fixture) = RecordingSceneFixture::with_route(2, P1SceneRoute::Shared)
            .expect("two-output recording scene fixture construction")
        else {
            eprintln!("SKIP independent_output_owners: Vulkan fixture unavailable row={row}");
            return;
        };
        fixture.events.backend.platform.outputs[0]
            .output
            .picked
            .vrefresh = 75;
        fixture.events.backend.platform.outputs[1]
            .output
            .picked
            .vrefresh = 240;
        let directs = [
            fixture.install_direct_output_present(0, present_ids[0], 0),
            fixture.install_direct_output_present(1, present_ids[1], 0),
        ];
        let device_fd = fd(&fixture);
        let mut records: [Option<AtomicCommitRecord>; 2] = [None, None];
        let mut accepted_ids = [None, None];

        for (entry_count, output_idx) in entry_order.into_iter().enumerate() {
            let record = submit_output_present_via_backend_for_row(
                &mut fixture,
                &directs[output_idx],
                output_idx,
                row,
            );
            let id = accepted_event_id(&record)
                .unwrap_or_else(|| panic!("{row}: Output({output_idx}) commit has an accepted ID"));
            assert_atomic_record_touches_output_only(&fixture, &record, output_idx, row);
            records[output_idx] = Some(record);
            accepted_ids[output_idx] = Some(id);
            assert_eq!(
                fixture.events.backend.present_source_pins.len(),
                2 * (entry_count + 1),
                "{row}: each admitted owner retains its source and fallback pins"
            );
        }

        assert_ne!(
            accepted_ids[0], accepted_ids[1],
            "{row}: each output has an independent accepted commit identity"
        );
        assert_eq!(
            fixture.events.backend.pstate.direct.active_scopes(),
            [
                crate::kms::render::backend::DirectScope::Output(0),
                crate::kms::render::backend::DirectScope::Output(1),
            ],
            "{row}: both Output owners remain active after normal admission"
        );
        for (output_idx, owner) in fixture
            .events
            .backend
            .pstate
            .direct
            .outputs
            .iter()
            .enumerate()
        {
            assert!(
                !owner.unflip_requested,
                "{row}: Output({output_idx}) is not revoked by the other admission"
            );
        }
        assert_eq!(
            fixture.events.backend.present_source_pins.len(),
            4,
            "{row}: both active owners retain four source/fallback pins"
        );

        for (retired_count, output_idx) in retirement_order.into_iter().enumerate() {
            let record = records[output_idx]
                .as_ref()
                .expect("each output has its own normal admission record");
            let id = accepted_ids[output_idx].expect("accepted ID is stored per output");
            let frame = clocks[output_idx];
            complete_recorded_attempt(&mut fixture, device_fd, record, id, frame);
            let completed = Backend::drain_completed_present_events(&mut fixture.events.backend);
            assert_eq!(
                completed
                    .iter()
                    .map(|event| event.present_id)
                    .collect::<Vec<_>>(),
                [directs[output_idx].candidate.present_id],
                "{row}: Output({output_idx}) completion is independent"
            );
            assert_eq!(
                completed[0].completion_clock,
                Some(yserver_core::backend::PresentClockSample {
                    msc: u64::from(frame),
                    ust: u64::from(frame) * 10_000,
                    source: yserver_core::backend::PresentClockSource::PageFlip,
                }),
                "{row}: Output({output_idx}) uses its own CRTC clock"
            );
            assert!(
                fixture.events.backend.pstate.direct.outputs[output_idx]
                    .current
                    .is_some(),
                "{row}: Output({output_idx}) becomes current on its own event"
            );
            assert!(
                fixture.events.backend.pstate.direct.outputs[output_idx]
                    .pending
                    .is_none(),
                "{row}: Output({output_idx}) pending frame retires"
            );
            assert_eq!(
                fixture.events.backend.present_source_pins.len(),
                4,
                "{row}: retiring one owner leaves both owners' pins retained"
            );

            complete_recorded_attempt(&mut fixture, device_fd, record, id, frame + 1);
            assert!(
                Backend::drain_completed_present_events(&mut fixture.events.backend).is_empty(),
                "{row}: duplicate Output({output_idx}) event produces no second completion"
            );
            let other_idx = 1 - output_idx;
            if retired_count == 0 {
                assert!(
                    fixture.events.backend.pstate.direct.outputs[other_idx]
                        .pending
                        .is_some(),
                    "{row}: Output({output_idx}) event leaves Output({other_idx}) pending"
                );
            }
        }
        assert!(
            fixture.events.backend.pstate.direct.outputs[0]
                .current
                .is_some(),
            "{row}: Output(0) remains current after both independent events"
        );
        assert!(
            fixture.events.backend.pstate.direct.outputs[1]
                .current
                .is_some(),
            "{row}: Output(1) remains current after both independent events"
        );
        fixture.finish();
    }
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: admit Output(1) while the Root owner is still active.
fn root_to_output_transition_unflips_once() {
    let Some((mut fixture, root)) = direct_fixture(2, P1SceneRoute::Shared, 213, 0) else {
        return;
    };
    for output in &mut fixture.events.backend.platform.outputs {
        output.output.picked.vrefresh = 60;
    }
    let device_fd = fd(&fixture);
    let root_record = submit_direct(&mut fixture, &root, device_fd)
        .into_iter()
        .find(|record| accepted_event_id(record).is_some() && record.events.len() == 2)
        .expect("Root Present entered through Backend");
    let root_id = accepted_event_id(&root_record).expect("Root commit accepted");
    complete_direct_initial(&mut fixture, &root_record, root_id, device_fd);
    let completed = Backend::drain_completed_present_events(&mut fixture.events.backend);
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].present_id, root.candidate.present_id);
    assert!(Backend::drain_completed_present_events(&mut fixture.events.backend).is_empty());

    let output = reconfigure_root_present_as_output(&mut fixture, &root, 1, 214);
    fixture
        .events
        .backend
        .for_tests_set_scanout_m1_prober(super::super::present_scanout::record_scanout_m1_probe);
    Backend::note_present_scanout_candidate(&mut fixture.events.backend, output.candidate);
    let probation = usize::from(crate::kms::render::backend::SCANOUT_M2_ELIGIBLE_ROOT_PROBATION);
    for _ in 0..probation {
        assert!(
            !Backend::try_present_direct(
                &mut fixture.events.backend,
                output.candidate,
                output.event.clone(),
            )
            .expect("Output Present while Root still owns the outputs"),
            "Output(1) waits until the active Root owner has been replaced"
        );
    }
    assert!(fixture.events.backend.pstate.direct.root().unflip_requested);
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
    assert!(!fixture.events.backend.pstate.direct.outputs[1].active());

    let unflip_start = record_count(&fixture, device_fd);
    let unflip = grouped_unflip_record(&mut fixture, device_fd, unflip_start, 2);
    complete_recorded_attempt(
        &mut fixture,
        device_fd,
        &unflip,
        accepted_event_id(&unflip).expect("grouped unflip accepted"),
        73,
    );
    assert!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .active_scopes()
            .is_empty(),
        "Root ownership ends only after both CRTCs accept the composed replacement"
    );
    assert!(fixture.events.backend.present_source_pins.is_empty());
    assert!(Backend::drain_completed_present_events(&mut fixture.events.backend).is_empty());
    assert_eq!(
        Backend::drain_retired_present_idle_events(&mut fixture.events.backend)
            .iter()
            .map(|event| event.present_id)
            .collect::<Vec<_>>(),
        [root.candidate.present_id],
        "the Root Present idles once after its final CRTC replacement"
    );
    assert!(Backend::drain_retired_present_idle_events(&mut fixture.events.backend).is_empty());

    compose_and_complete_outputs(&mut fixture, device_fd, &[0, 1]);
    let output_record = submit_output_present_via_backend(&mut fixture, &output, 1);
    let output_id = accepted_event_id(&output_record).expect("Output(1) commit accepted");
    assert_atomic_record_touches_output_only(
        &fixture,
        &output_record,
        1,
        "root-to-output transition",
    );
    assert_eq!(
        fixture.events.backend.pstate.direct.active_scopes(),
        [crate::kms::render::backend::DirectScope::Output(1)]
    );
    complete_recorded_attempt(&mut fixture, device_fd, &output_record, output_id, 74);
    let completed = Backend::drain_completed_present_events(&mut fixture.events.backend);
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].present_id, output.candidate.present_id);
    assert_eq!(
        completed[0]
            .completion_clock
            .map(|clock| (clock.msc, clock.ust)),
        Some((74, 740_000))
    );
    fixture.finish();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: unflip only the Output owner on the Root Present's selected CRTC.
fn output_to_root_transition_unflips_every_output_owner() {
    let Some(mut fixture) = RecordingSceneFixture::with_route(2, P1SceneRoute::Shared)
        .expect("two-output recording scene fixture construction")
    else {
        eprintln!(
            "SKIP output_to_root_transition_unflips_every_output_owner: Vulkan fixture unavailable"
        );
        return;
    };
    for output in &mut fixture.events.backend.platform.outputs {
        output.output.picked.vrefresh = 60;
    }
    let first = fixture.install_direct_output_present(0, 215, 0);
    let second = fixture.install_direct_output_present(1, 216, 0);
    Backend::set_cursor_hidden(&mut fixture.events.backend, true);
    let device_fd = fd(&fixture);
    let first_record = submit_output_present_via_backend(&mut fixture, &first, 0);
    let second_record = submit_output_present_via_backend(&mut fixture, &second, 1);
    let first_id = accepted_event_id(&first_record).expect("Output(0) commit accepted");
    let second_id = accepted_event_id(&second_record).expect("Output(1) commit accepted");
    complete_recorded_attempt(&mut fixture, device_fd, &first_record, first_id, 75);
    complete_recorded_attempt(&mut fixture, device_fd, &second_record, second_id, 76);
    assert_eq!(
        Backend::drain_completed_present_events(&mut fixture.events.backend)
            .iter()
            .map(|event| event.present_id)
            .collect::<Vec<_>>(),
        [first.candidate.present_id, second.candidate.present_id]
    );

    let root = fixture.install_direct_present(217, 0);
    let entered = Backend::try_present_direct(
        &mut fixture.events.backend,
        root.candidate,
        root.event.clone(),
    )
    .expect("Root Present while both Output owners are active");
    assert!(!entered, "Root waits for all Output owners to unflip");
    assert!(fixture.events.backend.pstate.direct.outputs[0].unflip_requested);
    assert!(
        fixture.events.backend.pstate.direct.outputs[1].unflip_requested,
        "one Root Present requests a composed unflip for every Output owner"
    );
    assert!(!fixture.events.backend.pstate.direct.root().active());

    let start = record_count(&fixture, device_fd);
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("submit each Output owner's composed unflip through scheduler");
    let records = records_since(&fixture, device_fd, start);
    let output0_crtc = fixture.events.backend.platform.outputs[0].output.crtc;
    let output1_crtc = fixture.events.backend.platform.outputs[1].output.crtc;
    let unflip0 = records
        .iter()
        .find(|record| {
            accepted_event_id(record).is_some()
                && record.events.len() == 1
                && record.events[0].crtc == output0_crtc
        })
        .expect("Output(0) composed replacement accepted")
        .clone();
    let unflip1 = records
        .iter()
        .find(|record| {
            accepted_event_id(record).is_some()
                && record.events.len() == 1
                && record.events[0].crtc == output1_crtc
        })
        .expect("Output(1) composed replacement accepted")
        .clone();
    assert_atomic_record_touches_output_only(&fixture, &unflip0, 0, "output 0 root replacement");
    assert_atomic_record_touches_output_only(&fixture, &unflip1, 1, "output 1 root replacement");
    complete_recorded_attempt(
        &mut fixture,
        device_fd,
        &unflip0,
        accepted_event_id(&unflip0).unwrap(),
        77,
    );
    complete_recorded_attempt(
        &mut fixture,
        device_fd,
        &unflip1,
        accepted_event_id(&unflip1).unwrap(),
        78,
    );
    assert!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .active_scopes()
            .is_empty()
    );
    assert!(fixture.events.backend.present_source_pins.is_empty());
    let mut idled = Backend::drain_retired_present_idle_events(&mut fixture.events.backend)
        .into_iter()
        .map(|event| event.present_id)
        .collect::<Vec<_>>();
    idled.sort_unstable();
    assert_eq!(
        idled,
        [first.candidate.present_id, second.candidate.present_id]
    );
    assert!(Backend::drain_completed_present_events(&mut fixture.events.backend).is_empty());

    compose_and_complete_outputs(&mut fixture, device_fd, &[0, 1]);
    let root_records = submit_direct(&mut fixture, &root, device_fd);
    let root_record = root_records
        .into_iter()
        .find(|record| accepted_event_id(record).is_some() && record.events.len() == 2)
        .expect("Root enters after both Output owners retire and normal probation");
    let root_id = accepted_event_id(&root_record).expect("Root direct commit accepted");
    complete_direct_initial(&mut fixture, &root_record, root_id, device_fd);
    let completed = Backend::drain_completed_present_events(&mut fixture.events.backend);
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].present_id, root.candidate.present_id);
    assert_eq!(
        fixture.events.backend.pstate.direct.active_scopes(),
        [crate::kms::render::backend::DirectScope::Root]
    );
    fixture.finish();
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
            .direct
            .root()
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
            .direct
            .root()
            .completed
            .is_empty()
    );
    assert!(fixture.events.backend.pstate.direct.root().idled.is_empty());
    assert_eq!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
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
            .direct
            .root()
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
            .direct
            .root()
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
    assert_eq!(
        fixture.events.backend.pstate.direct.root().completed.len(),
        1
    );
    assert_eq!(
        fixture.events.backend.pstate.direct.root().completed[0].completion_clock,
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
            .direct
            .root()
            .unflip_outputs_to_replace
            .is_empty()
    );
    assert!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
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
            .direct
            .root()
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
    assert_eq!(pending_ids(&fixture, device_fd, crtc), [vsync_id]);
    complete_recorded_attempt(&mut fixture, device_fd, &attempts[1], vsync_id, 71);
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
    assert!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .unflip_awaiting_outputs
            .is_empty()
    );
    assert!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
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
    assert_eq!(
        pending_ids(&fixture, device_fd, accepted[0].events[0].crtc),
        [accepted_id]
    );
    complete_recorded_attempt(&mut fixture, device_fd, &accepted[0], accepted_id, 81);
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
            .direct
            .root()
            .unflip_awaiting_outputs,
        HashMap::from([(0, replacement_id), (1, replacement_id)])
    );
    assert_eq!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
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
            .direct
            .root()
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
            .direct
            .root()
            .unflip_awaiting_outputs,
        HashMap::from([(1, replacement_id)])
    );
    assert_eq!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .unflip_outputs_to_replace,
        HashSet::from([1])
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
            .direct
            .root()
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
            .direct
            .root()
            .unflip_awaiting_outputs
            .is_empty()
    );
    assert!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
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
            .direct
            .root()
            .unflip_outputs_to_replace,
        HashSet::from([0, 1])
    );
    assert_eq!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
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
            .direct
            .root()
            .unflip_outputs_to_replace
            .len(),
        2
    );
    assert_eq!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
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
            .direct
            .root()
            .unflip_outputs_to_replace,
        HashSet::from([waiting])
    );
    assert!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .unflip_awaiting_outputs
            .is_empty(),
        "no accepted replacement ID remains while one output still requires replacement"
    );
    assert!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .current
            .is_some(),
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
    assert!(fixture.events.backend.pstate.direct.root().unflip_requested);
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

    let later_offset = record_count(&fixture, device_fd);
    recorder.script_result(Ok(()));
    let later = scene_completion_for_output(&mut fixture, waiting, device_fd, later_offset);
    let later_id = accepted_event_id(&later).unwrap();
    assert_eq!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
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
            .direct
            .root()
            .unflip_outputs_to_replace
            .is_empty()
    );
    assert!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
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
            .direct
            .root()
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

struct ResizePendingUnflip {
    fixture: super::recording_scene::RecordingSceneFixtureGuard,
    direct_source_id: crate::kms::render::store::DrawableId,
    device_fd: RawFd,
    replacement: AtomicCommitRecord,
    replacement_id: u64,
    old_topology: u64,
    new_source_id: crate::kms::render::store::DrawableId,
}

fn resize_root_direct_present_before_unflip() -> Option<ResizePendingUnflip> {
    use crate::kms::render::store::DrawableKind;
    use yserver_core::{
        backend::{Backend, PixmapHandle, WindowHandle},
        resources::{ROOT_VISUAL, ROOT_WINDOW},
        server::{BackendCapabilities, ServerState},
    };
    use yserver_protocol::x11::{
        ClientId, CreatePixmapRequest, CreateWindowRequest, RequestHeader, ResourceId,
        SequenceNumber,
    };

    const CLIENT: u32 = 1;
    const INITIAL_PRESENT_ID: u64 = 0x711;
    const WINDOW_XID: u32 = 0x0071_1001;
    const PIXMAP_XID: u32 = 0x0071_1002;

    let (fixture, direct) = direct_fixture(1, P1SceneRoute::Shared, INITIAL_PRESENT_ID, 0)?;
    let mut fixture = fixture.cleanup_guard();
    let device_fd = fd(&fixture);
    let initial_size = (
        fixture.events.backend.platform.fb_w,
        fixture.events.backend.platform.fb_h,
    );
    // Let the normal observation path seed the cache signature for the old
    // root extent, then restore the fixture's synthetic accepted M1 owner.
    Backend::note_present_scanout_candidate(&mut fixture.events.backend, direct.candidate);
    fixture.events.backend.pstate.scanout_m1.entries.insert(
        (
            direct.source_id,
            crate::kms::render::backend::DirectScope::Root,
        ),
        crate::kms::render::backend::ScanoutM1ProbeEntry::accepted(
            crate::drm::modeset::DirectScanoutProbeFramebuffer::for_tests(
                ::drm::control::from_u32(0x8711).expect("nonzero synthetic framebuffer"),
            ),
        ),
    );
    let old_topology = fixture.events.backend.pstate.scanout_m1.topology_signature;
    assert!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m1
            .entries
            .contains_key(&(
                direct.source_id,
                crate::kms::render::backend::DirectScope::Root
            ))
    );

    let attempts = submit_direct(&mut fixture, &direct, device_fd);
    let initial = accepted_records(&attempts)
        .into_iter()
        .next()
        .expect("initial root direct commit accepted");
    let initial_id = accepted_event_id(&initial).expect("initial direct commit has an event ID");
    complete_direct_initial(&mut fixture, &initial, initial_id, device_fd);
    drain_present_notifications(&mut fixture, &[INITIAL_PRESENT_ID], &[]);
    assert_eq!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .current
            .as_ref()
            .map(|frame| frame.source_id),
        Some(direct.source_id),
        "the source whose M1 entry owns the FB is live on the root"
    );

    let new_size = (initial_size.0 + 128, initial_size.1);
    Backend::set_logical_screen_size(&mut fixture.events.backend, new_size.0, new_size.1)
        .expect("resize through the Backend entry point used by RRSetScreenSize");
    assert_eq!(
        fixture.events.backend.pstate.scanout_m1.topology_signature, old_topology,
        "resize requests unflip but does not synchronously clear the M1 cache"
    );
    assert!(fixture.events.backend.pstate.direct.root().unflip_requested);
    assert!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m1
            .entries
            .contains_key(&(
                direct.source_id,
                crate::kms::render::backend::DirectScope::Root
            ))
    );

    let replacement_offset = record_count(&fixture, device_fd);
    let replacement = grouped_unflip_record(&mut fixture, device_fd, replacement_offset, 1);
    let replacement_id =
        accepted_event_id(&replacement).expect("composed unflip replacement accepted");
    assert_eq!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .unflip_awaiting_outputs,
        HashMap::from([(0, replacement_id)]),
        "the replacement is accepted but has not retired"
    );
    assert_eq!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .current
            .as_ref()
            .map(|frame| frame.source_id),
        Some(direct.source_id),
        "the direct source remains current until the replacement flip event"
    );

    // Build the core resource view after the resize, then resize the existing
    // direct target through ConfigureWindow so the following wire Present has
    // both a new-root-sized source and a matching target rectangle.
    let (outputs, modes) = Backend::randr_outputs_and_modes(&mut fixture.events.backend);
    let capabilities = BackendCapabilities::from_backend(&fixture.events.backend);
    fixture.events.state = ServerState::with_randr_outputs_and_modes(
        new_size.0,
        new_size.1,
        outputs,
        modes,
        capabilities,
    );
    super::super::install_client_for_render(&mut fixture.events.state, CLIENT);
    fixture
        .events
        .state
        .resources
        .window_mut(ROOT_WINDOW)
        .expect("root resource")
        .host_xid = WindowHandle::from_raw(fixture.events.backend.core.window_id);
    let window = ResourceId(WINDOW_XID);
    fixture.events.state.resources.create_window(
        ClientId(CLIENT),
        CreateWindowRequest {
            depth: 24,
            window,
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: initial_size.0,
            height: initial_size.1,
            border_width: 0,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default()
        },
    );
    fixture
        .events
        .state
        .resources
        .window_mut(window)
        .expect("Present target resource")
        .host_xid = WindowHandle::from_raw(direct.target_xid);
    let _ = fixture.events.state.resources.map_window(window);

    let mut configure_body = Vec::with_capacity(12);
    configure_body.extend_from_slice(&WINDOW_XID.to_le_bytes());
    configure_body.extend_from_slice(&0x000c_u16.to_le_bytes()); // CWWidth | CWHeight
    configure_body.extend_from_slice(&0_u16.to_le_bytes());
    configure_body.extend_from_slice(&u32::from(new_size.0).to_le_bytes());
    configure_body.extend_from_slice(&u32::from(new_size.1).to_le_bytes());
    let events = fixture.events_mut();
    yserver_core::core_loop::process_request::process_request(
        &mut events.state,
        &mut events.backend,
        ClientId(CLIENT),
        SequenceNumber(1),
        RequestHeader {
            opcode: 12, // ConfigureWindow
            data: 0,
            length_units: 5,
        },
        &configure_body,
        None,
    )
    .expect("configure the target through the core request dispatcher");

    let source_host_xid = direct.target_xid + 2;
    let mut source_storage = fixture
        .events
        .backend
        .platform
        .gpu
        .allocate_drawable_storage(new_size.0, new_size.1, 24)
        .expect("allocate new-root-sized Present source");
    // Direct sources model imported client buffers.
    source_storage.imported_dmabuf = Some(super::super::imported_dmabuf_for_test(
        new_size.0, new_size.1,
    ));
    let new_source_id = fixture
        .events
        .backend
        .store
        .allocate(
            source_host_xid,
            DrawableKind::Pixmap,
            24,
            false,
            source_storage,
        )
        .expect("register Present source in backend store");
    Backend::put_image(
        &mut fixture.events.backend,
        None,
        source_host_xid,
        24,
        1,
        1,
        0,
        0,
        &[0, 0, 0, 0],
    )
    .expect("initialize Present source through Backend");
    fixture.adopt_drawables([source_host_xid]);

    let pixmap = ResourceId(PIXMAP_XID);
    fixture.events.state.resources.create_pixmap(
        ClientId(CLIENT),
        CreatePixmapRequest {
            depth: 24,
            pixmap,
            drawable: window,
            width: new_size.0,
            height: new_size.1,
        },
    );
    let _ = fixture.events.state.resources.set_pixmap_host_xid(
        pixmap,
        PixmapHandle::from_raw(source_host_xid).expect("nonzero source host XID"),
    );

    // PresentPixmap with no valid/update region and zero offsets. The core
    // request path calls observe_scanout_m0 before try_present_direct; the
    // latter declines because the resize's composed replacement is pending.
    dispatch_resized_root_present(&mut fixture, 2, 2);

    assert!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .unflip_awaiting_outputs
            .contains_key(&0)
    );
    assert_eq!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .current
            .as_ref()
            .map(|frame| frame.source_id),
        Some(direct.source_id),
        "the unflip replacement is still unretired while the Present is processed"
    );
    let copied_present = Backend::drain_completed_present_events(&mut fixture.events.backend);
    assert_eq!(
        copied_present.len(),
        1,
        "the resized root Present completed through its Copy fallback"
    );
    assert_eq!(copied_present[0].host_xid, PIXMAP_XID);
    assert!(Backend::drain_retired_present_idle_events(&mut fixture.events.backend).is_empty());

    Some(ResizePendingUnflip {
        fixture,
        direct_source_id: direct.source_id,
        device_fd,
        replacement,
        replacement_id,
        old_topology,
        new_source_id,
    })
}

fn dispatch_resized_root_present(
    fixture: &mut super::recording_scene::RecordingSceneFixtureGuard,
    sequence: u16,
    serial: u32,
) {
    use yserver_protocol::x11::{ClientId, RequestHeader, SequenceNumber, present};

    let mut present_body = vec![0_u8; 68];
    present_body[0..4].copy_from_slice(&0x0071_1001_u32.to_le_bytes());
    present_body[4..8].copy_from_slice(&0x0071_1002_u32.to_le_bytes());
    present_body[8..12].copy_from_slice(&serial.to_le_bytes());
    present_body[36..40]
        .copy_from_slice(&yserver_core::present_scheduler::PRESENT_OPTION_ASYNC.to_le_bytes());
    let events = fixture.events_mut();
    yserver_core::core_loop::process_request::process_request(
        &mut events.state,
        &mut events.backend,
        ClientId(1),
        SequenceNumber(sequence),
        RequestHeader {
            opcode: 145,
            data: present::PIXMAP,
            length_units: 18,
        },
        &present_body,
        None,
    )
    .expect("PresentPixmap through the core request dispatcher");
}

fn assert_live_resize_m1_owner_is_held(scenario: &ResizePendingUnflip, row: &str) {
    let backend = &scenario.fixture.events.backend;
    let entry = backend
        .pstate
        .scanout_m1
        .entries
        .get(&(
            scenario.direct_source_id,
            crate::kms::render::backend::DirectScope::Root,
        ))
        .unwrap_or_else(|| panic!("{row}: live direct source retains its M1 cache entry"));
    assert!(
        entry.framebuffer().is_some(),
        "{row}: the live source retains its M1 framebuffer owner"
    );
    assert_eq!(
        backend.pstate.scanout_m1.topology_signature, scenario.old_topology,
        "{row}: the live direct owner keeps the old M1 topology signature"
    );
    assert!(
        !backend.pstate.scanout_m1.entries.contains_key(&(
            scenario.new_source_id,
            crate::kms::render::backend::DirectScope::Root
        )),
        "{row}: the changed-topology candidate is deferred while an owner is active"
    );
    assert!(
        backend.pstate.direct.root().active(),
        "{row}: the direct owner remains active"
    );
    assert!(
        backend
            .pstate
            .direct
            .root()
            .unflip_awaiting_outputs
            .contains_key(&0),
        "{row}: the old owner awaits its composed replacement"
    );
    assert_eq!(
        backend
            .pstate
            .direct
            .root()
            .current
            .as_ref()
            .map(|frame| frame.source_id),
        Some(scenario.direct_source_id),
        "{row}: the old source remains current until replacement retirement"
    );
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: D10 clears the M1 cache on a signature change despite an active
// owner; D11 never clears stale M1 entries after owners retire.
fn m1_cache_owner_lifetime() {
    // D10 checks the live-owner checkpoint. D11 continues the same fresh
    // resize fixture to the next eligible Present after retirement.
    for (row, observe_after_retirement) in [
        // D10: preserve the framebuffer while the live direct owner exists.
        ("live_owner_checkpoint", false),
        // D11: evict the stale signature entry on the next observation.
        ("post_retirement_reprobe", true),
    ] {
        let Some(mut scenario) = resize_root_direct_present_before_unflip() else {
            eprintln!("SKIP m1_cache_owner_lifetime: Vulkan fixture unavailable row={row}");
            return;
        };
        assert_live_resize_m1_owner_is_held(&scenario, row);

        complete_recorded_attempt(
            &mut scenario.fixture,
            scenario.device_fd,
            &scenario.replacement,
            scenario.replacement_id,
            90,
        );
        let backend = &scenario.fixture.events.backend;
        let direct = backend.pstate.direct.root();
        assert!(
            direct.unflip_awaiting_outputs.is_empty(),
            "{row}: replacement retirement clears the awaiting output"
        );
        assert!(
            !direct.active(),
            "{row}: replacement retirement stops the direct owner"
        );
        assert!(
            direct.current.is_none(),
            "{row}: replacement retirement releases the current frame"
        );
        assert!(
            backend.pstate.scanout_m1.entries.contains_key(&(
                scenario.direct_source_id,
                crate::kms::render::backend::DirectScope::Root
            )),
            "{row}: the old M1 entry survives replacement retirement"
        );
        let idled =
            Backend::drain_retired_present_idle_events(&mut scenario.fixture.events.backend);
        assert_eq!(
            idled
                .iter()
                .map(|event| event.present_id)
                .collect::<Vec<_>>(),
            [0x711],
            "{row}: the retired direct Present idles once"
        );
        assert!(
            Backend::drain_retired_present_idle_events(&mut scenario.fixture.events.backend)
                .is_empty(),
            "{row}: the direct Present Idle event does not drain twice"
        );

        if observe_after_retirement {
            dispatch_resized_root_present(&mut scenario.fixture, 3, 3);
            let backend = &mut scenario.fixture.events.backend;
            assert_ne!(
                backend.pstate.scanout_m1.topology_signature, scenario.old_topology,
                "{row}: the next eligible Present records the resized topology signature"
            );
            assert!(
                !backend.pstate.scanout_m1.entries.contains_key(&(
                    scenario.direct_source_id,
                    crate::kms::render::backend::DirectScope::Root
                )),
                "{row}: the next observation removes the stale old-source cache entry"
            );
            let new_entry = backend
                .pstate
                .scanout_m1
                .entries
                .get(&(
                    scenario.new_source_id,
                    crate::kms::render::backend::DirectScope::Root,
                ))
                .unwrap_or_else(|| {
                    panic!("{row}: the resized source is probed under the new signature")
                });
            assert!(
                new_entry.framebuffer().is_none(),
                "{row}: the newly observed source has no accepted M1 framebuffer yet"
            );
            let copied_present = Backend::drain_completed_present_events(backend);
            assert_eq!(
                copied_present.len(),
                1,
                "{row}: resized Copy Present completes once"
            );
            assert_eq!(
                copied_present[0].host_xid, 0x0071_1002,
                "{row}: Copy completion belongs to the resized source"
            );
            assert!(
                Backend::drain_retired_present_idle_events(backend).is_empty(),
                "{row}: Copy completion does not duplicate the retired direct Idle"
            );
        }
        scenario.fixture.finish_after_normal_drain();
    }
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
            .direct
            .root()
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
            .direct
            .root()
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
            .direct
            .root()
            .unflip_outputs_to_replace
            .is_empty()
    );
    assert!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
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
            .direct
            .root()
            .unflip_awaiting_outputs
            .is_empty(),
        "source render readiness must not create an accepted sink identity"
    );
    assert_eq!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
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
            .direct
            .root()
            .unflip_awaiting_outputs
            .is_empty()
    );
    assert_eq!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
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
            .direct
            .root()
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
            .direct
            .root()
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
            .direct
            .root()
            .unflip_outputs_to_replace
            .is_empty()
    );
    assert!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
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

fn try_output_owner_admission(
    fixture: &mut RecordingSceneFixture,
    direct: &DirectPresentSetup,
) -> bool {
    fixture
        .events
        .backend
        .for_tests_set_scanout_m1_prober(super::super::present_scanout::record_scanout_m1_probe);
    Backend::note_present_scanout_candidate(&mut fixture.events.backend, direct.candidate);
    let probation = usize::from(crate::kms::render::backend::SCANOUT_M2_ELIGIBLE_ROOT_PROBATION);
    for _ in 0..probation {
        if Backend::try_present_direct(
            &mut fixture.events.backend,
            direct.candidate,
            direct.event.clone(),
        )
        .expect("Output candidate enters through Backend")
        {
            return true;
        }
    }
    false
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: make Output admission consult Root's reentry barrier after Root unflips.
fn root_unflip_barrier_is_per_output_for_output_admission() {
    let Some((fixture, root)) = direct_fixture(2, P1SceneRoute::Shared, 217, 0) else {
        eprintln!(
            "SKIP root_unflip_barrier_is_per_output_for_output_admission: Vulkan fixture unavailable"
        );
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    let device_fd = fd(&fixture);
    let root_record = submit_direct(&mut fixture, &root, device_fd)
        .into_iter()
        .find(|record| accepted_event_id(record).is_some() && record.events.len() == 2)
        .expect("Root direct commit accepted on both outputs");
    let root_id = accepted_event_id(&root_record).unwrap();
    complete_direct_initial(&mut fixture, &root_record, root_id, device_fd);
    drain_present_notifications(&mut fixture, &[root.candidate.present_id], &[]);

    request_unflip(&mut fixture, &root);
    let root_unflip_start = record_count(&fixture, device_fd);
    let root_unflip = grouped_unflip_record(&mut fixture, device_fd, root_unflip_start, 2);
    complete_recorded_attempt(
        &mut fixture,
        device_fd,
        &root_unflip,
        accepted_event_id(&root_unflip).unwrap(),
        88,
    );
    drain_present_notifications(&mut fixture, &[], &[root.candidate.present_id]);
    assert!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .current
            .is_none()
    );
    assert!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .reentry_blocked_until_composed
    );

    // Simulate an unrelated KMS client holding output 1 with an accepted
    // event-producing commit. The Backend scheduler must compose output 0
    // while the foreign event keeps output 1 unavailable.
    let output1_crtc = fixture.events.backend.platform.outputs[1].output.crtc;
    let device = &fixture.events.backend.platform.devices[0];
    let foreign_start = record_count(&fixture, device_fd);
    let foreign_id = device
        .atomic_commit_sink
        .submit_with_commit_id(
            &device.device,
            AtomicCommitFlags::PAGE_FLIP_EVENT | AtomicCommitFlags::NONBLOCK,
            AtomicCommitKind::GammaOnly,
            AtomicCommitRequest::new(),
            &[AtomicCommitEvent {
                crtc: output1_crtc,
                motion_timestamp: None,
            }],
        )
        .expect("record a foreign output 1 KMS hold");
    let foreign_hold = records_since(&fixture, device_fd, foreign_start)
        .into_iter()
        .find(|record| accepted_event_id(record) == Some(foreign_id))
        .expect("foreign output 1 hold is accepted by the recording KMS sink");
    assert!(
        fixture
            .events
            .backend
            .platform
            .scanout_commit_in_flight_for_output(1)
    );

    let output0 = fixture.install_direct_output_present(0, 218, 0);
    let output1 = fixture.install_direct_output_present(1, 219, 0);
    let compose_start = record_count(&fixture, device_fd);
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("compose output 0 while output 1's scene submission is held");
    fixture.events.wait_for_submitted_gpu_work();
    let held_records = records_since(&fixture, device_fd, compose_start);
    assert!(held_records.iter().any(|record| {
        record.kind == crate::drm::atomic::AtomicCommitKind::Scene
            && accepted_event_id(record).is_some()
            && record
                .events
                .iter()
                .any(|event| event.crtc == fixture.events.backend.platform.outputs[0].output.crtc)
    }));
    assert!(!held_records.iter().any(|record| {
        record.kind == crate::drm::atomic::AtomicCommitKind::Scene
            && accepted_event_id(record).is_some()
            && record
                .events
                .iter()
                .any(|event| event.crtc == fixture.events.backend.platform.outputs[1].output.crtc)
    }));
    assert!(!fixture.events.backend.pstate.direct.outputs[0].reentry_blocked_until_composed);
    assert!(fixture.events.backend.pstate.direct.outputs[1].reentry_blocked_until_composed);
    for record in accepted_records(&held_records) {
        let id = accepted_event_id(&record).unwrap();
        complete_recorded_attempt(&mut fixture, device_fd, &record, id, 89);
    }
    complete_recorded_attempt(&mut fixture, device_fd, &foreign_hold, foreign_id, 90);
    assert!(
        !fixture
            .events
            .backend
            .platform
            .scanout_commit_in_flight_for_output(1)
    );

    let output0_admission_start = record_count(&fixture, device_fd);
    assert!(
        try_output_owner_admission(&mut fixture, &output0),
        "Output(0) is admitted after its own composition even though Root's barrier remains"
    );
    let output0_admission_records = records_since(&fixture, device_fd, output0_admission_start);
    assert!(
        output0_admission_records.iter().any(|record| {
            accepted_event_id(record).is_some()
                && record.events.iter().any(|event| {
                    event.crtc == fixture.events.backend.platform.outputs[0].output.crtc
                })
        }),
        "Output(0) admission records its direct flip through Backend"
    );
    for record in accepted_records(&output0_admission_records) {
        complete_recorded_attempt(
            &mut fixture,
            device_fd,
            &record,
            accepted_event_id(&record).unwrap(),
            91,
        );
    }
    assert!(
        !try_output_owner_admission(&mut fixture, &output1),
        "Output(1) stays blocked while its own composition barrier remains"
    );

    let output1_compose_start = record_count(&fixture, device_fd);
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("compose output 1 after releasing its foreign KMS hold");
    fixture.events.wait_for_submitted_gpu_work();
    let output1_records = records_since(&fixture, device_fd, output1_compose_start);
    assert!(output1_records.iter().any(|record| {
        record.kind == crate::drm::atomic::AtomicCommitKind::Scene
            && accepted_event_id(record).is_some()
            && record
                .events
                .iter()
                .any(|event| event.crtc == fixture.events.backend.platform.outputs[1].output.crtc)
    }));
    for record in accepted_records(&output1_records) {
        complete_recorded_attempt(
            &mut fixture,
            device_fd,
            &record,
            accepted_event_id(&record).unwrap(),
            92,
        );
    }
    assert!(!fixture.events.backend.pstate.direct.outputs[1].reentry_blocked_until_composed);
    let output1_admission_start = record_count(&fixture, device_fd);
    let output1_admitted = try_output_owner_admission(&mut fixture, &output1);
    assert!(
        output1_admitted,
        "Output(1) is admitted after its own composition clears its barrier; root={:?}, output1={:?}, scene_pending=({}, {}), kms_pending=({}, {}), scope={:?}, cursor_mode={:?}",
        (
            fixture.events.backend.pstate.direct.root().active(),
            fixture.events.backend.pstate.direct.root().unflip_requested,
            fixture
                .events
                .backend
                .pstate
                .direct
                .root()
                .unflip_outputs_to_replace
                .len(),
            fixture
                .events
                .backend
                .pstate
                .direct
                .root()
                .unflip_awaiting_outputs
                .len(),
            fixture
                .events
                .backend
                .pstate
                .direct
                .root()
                .reentry_blocked_until_composed,
        ),
        (
            fixture.events.backend.pstate.direct.outputs[1].eligible_streak,
            fixture.events.backend.pstate.direct.outputs[1].active(),
            fixture.events.backend.pstate.direct.outputs[1].unflip_requested,
            fixture.events.backend.pstate.direct.outputs[1]
                .unflip_outputs_to_replace
                .len(),
            fixture.events.backend.pstate.direct.outputs[1]
                .unflip_awaiting_outputs
                .len(),
            fixture.events.backend.pstate.direct.outputs[1].reentry_blocked_until_composed,
        ),
        fixture.events.backend.scene.has_pending_page_flip(0),
        fixture.events.backend.scene.has_pending_page_flip(1),
        fixture
            .events
            .backend
            .platform
            .scanout_commit_in_flight_for_output(0),
        fixture
            .events
            .backend
            .platform
            .scanout_commit_in_flight_for_output(1),
        fixture.events.backend.direct_candidate_scope(
            output1.candidate,
            fixture.events.backend.scanout_m0_target(
                output1.candidate.paint_dst_host_xid,
                fixture
                    .events
                    .backend
                    .store
                    .lookup(output1.candidate.paint_dst_host_xid),
                fixture
                    .events
                    .backend
                    .resolve_paint_target(output1.candidate.paint_dst_host_xid)
                    .map(|target| target.backing_id()),
            ),
        ),
        fixture.events.backend.scene.cursor_mode_for_outputs(&[1]),
    );

    let output1_admission_records = records_since(&fixture, device_fd, output1_admission_start);
    assert!(
        output1_admission_records.iter().any(|record| {
            accepted_event_id(record).is_some()
                && record.events.iter().any(|event| {
                    event.crtc == fixture.events.backend.platform.outputs[1].output.crtc
                })
        }),
        "Output(1) admission records its direct flip through Backend"
    );
    for record in accepted_records(&output1_admission_records) {
        complete_recorded_attempt(
            &mut fixture,
            device_fd,
            &record,
            accepted_event_id(&record).unwrap(),
            93,
        );
    }
    drain_present_notifications(
        &mut fixture,
        &[output0.candidate.present_id, output1.candidate.present_id],
        &[],
    );

    Backend::unmap_subwindow(&mut fixture.events.backend, None, output0.target_xid)
        .expect("request Output(0) cleanup unflip through Backend");
    Backend::unmap_subwindow(&mut fixture.events.backend, None, output1.target_xid)
        .expect("request Output(1) cleanup unflip through Backend");
    let cleanup_start = record_count(&fixture, device_fd);
    Backend::maybe_composite(&mut fixture.events.backend).expect("cleanup Output owners");
    fixture.events.wait_for_submitted_gpu_work();
    let cleanup_records = records_since(&fixture, device_fd, cleanup_start);
    for record in accepted_records(&cleanup_records) {
        let id = accepted_event_id(&record).unwrap();
        complete_recorded_attempt(&mut fixture, device_fd, &record, id, 91);
    }
    assert!(fixture.events.backend.present_source_pins.is_empty());
    drain_present_notifications(
        &mut fixture,
        &[],
        &[output0.candidate.present_id, output1.candidate.present_id],
    );
    fixture.finish_after_normal_drain();
}
