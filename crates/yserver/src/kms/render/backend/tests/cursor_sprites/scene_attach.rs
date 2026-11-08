use super::{
    super::commit_identity::{recording_scene::P1SceneRoute, support::accepted_event_id},
    support::{
        AtomicPathFixture, PathKind, accept_cursor_detach, accepted_live_records,
        applied_cursor_binding, compose_until_cursor_commit, compose_until_cursor_commit_where,
        create_argb_cursor, damage_root, define_root_cursor, deliver_render_completion,
        drain_unretired_output_events, enter_held_direct, establish_hardware_cursor,
        establish_hardware_cursor_on_output, has_unretired_accepted_event, record_count,
        record_property, retire_cursor_record, retire_pending_output, sprite_bytes, sprite_users,
    },
};
use crate::{
    drm::atomic::{AtomicCommitKind, tests_support::ScriptedResult},
    kms::{
        cursor_plane::ops::recording::CursorBufferOperation, render::platform::ConnectorSnapshot,
    },
};
use std::{
    collections::HashSet,
    rc::Rc,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};
use yserver_core::backend::Backend;

fn install_direct_retry_clock(
    backend: &mut crate::kms::render::backend::KmsBackend,
) -> (std::sync::Arc<AtomicU64>, Instant) {
    let base = Instant::now();
    let elapsed_ns = std::sync::Arc::new(AtomicU64::new(0));
    let clock_elapsed_ns = std::sync::Arc::clone(&elapsed_ns);
    backend.for_tests_set_monotonic_clock(Rc::new(move || {
        base + Duration::from_nanos(clock_elapsed_ns.load(Ordering::SeqCst))
    }));
    (elapsed_ns, base)
}

fn advance_direct_retry_clock(elapsed_ns: &AtomicU64, duration: Duration) {
    elapsed_ns.fetch_add(
        u64::try_from(duration.as_nanos()).expect("retry duration fits u64 nanoseconds"),
        Ordering::SeqCst,
    );
}

fn cursor_hide_attempts_for_output(fixture: &AtomicPathFixture, output_idx: usize) -> usize {
    let fb_id =
        super::support::cursor_properties_for_output(&fixture.scene.events.backend, output_idx)
            .fb_id;
    let device_fd = fixture.scene.kms_fd(output_idx);
    let crtc = fixture.scene.events.backend.platform.outputs[output_idx]
        .output
        .crtc;
    fixture.scene.events.recorders[&device_fd]
        .records()
        .into_iter()
        .filter(|record| {
            record.kind == AtomicCommitKind::CursorOnly
                && record_property(record, fb_id) == Some(0)
                && record.events.iter().any(|event| event.crtc == crtc)
        })
        .count()
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: forward ShowOnRetire's captured (x, y) to show; CRTC_X/CRTC_Y must use P1 in both schedules.
fn retire_show_after_motion_uses_current_pointer() {
    for (route, move_before_acceptance) in [
        (P1SceneRoute::CopiedSameDevice, true),
        (P1SceneRoute::Shared, false),
    ] {
        let Some(mut fixture) = AtomicPathFixture::with_scene_route(PathKind::Physical, 1, route)
            .expect("cursor-retirement scene fixture construction")
        else {
            return;
        };
        let mut delivered = HashSet::new();
        if move_before_acceptance {
            // Drain the fixture's initial frame so the tested copied frame has
            // a real source-render completion boundary before KMS acceptance.
            damage_root(&mut fixture);
            Backend::maybe_composite(&mut fixture.scene.events.backend)
                .expect("submit the copied fixture's initial scene frame");
            for job_id in fixture.scene.pending_render_job_ids() {
                deliver_render_completion(&mut fixture, job_id);
            }
            drain_unretired_output_events(&mut fixture, &mut delivered, 0);
            assert!(
                !fixture.scene.events.backend.scene.has_pending_page_flip(0),
                "copied fixture is ready for the tested frame"
            );
        }
        let cursor = create_argb_cursor(
            &mut fixture.scene.events.backend,
            (4, 4),
            [0x12, 0x34, 0x56, 0xff],
            (1, 2),
        );
        define_root_cursor(&mut fixture.scene.events.backend, cursor);
        Backend::warp_pointer_root(
            &mut fixture.scene.events.backend,
            &mut fixture.scene.events.state,
            120,
            80,
        );
        damage_root(&mut fixture);

        let output_idx = 0;
        if move_before_acceptance {
            assert!(matches!(
                fixture.scene.events.backend.platform.scanout_pools[output_idx].as_ref(),
                Some(crate::kms::vk::scanout::OutputScanout::Copied(_))
            ));
        }
        let device_fd = fixture.scene.kms_fd(output_idx);
        let properties =
            super::support::cursor_properties_for_output(&fixture.scene.events.backend, output_idx);
        let record_start = record_count(&fixture, device_fd);

        if move_before_acceptance {
            let deadline = Instant::now() + std::time::Duration::from_secs(5);
            let mut pending_job = None;
            for _ in 0..16 {
                Backend::before_block(&mut fixture.scene.events.backend);
                damage_root(&mut fixture);
                Backend::maybe_composite(&mut fixture.scene.events.backend)
                    .expect("compose copied ShowOnRetire at P0 through Backend");
                pending_job = fixture.scene.pending_render_job_ids().last().copied();
                if pending_job.is_some() {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "copied composition did not register a source render"
                );
            }
            let job_id = pending_job
                .unwrap_or_else(|| {
                    panic!(
                        "copied composition registers a source render within 16 Backend ticks; dirty={}, ready={}, pending_flip={}, cursor_mode={:?}, records={:?}",
                        fixture.scene.events.backend.scene.scene_structure_dirty,
                        fixture.scene.events.backend.scene.output_ready_for_submit(
                            output_idx,
                            &fixture.scene.events.backend.platform,
                        ),
                        fixture.scene.events.backend.scene.has_pending_page_flip(output_idx),
                        fixture.scene.events.backend.scene.cursor_mode(),
                        fixture.scene.events.recorders[&device_fd]
                            .records()
                            .iter()
                            .map(|record| (record.kind, record.result, record.flags))
                            .collect::<Vec<_>>(),
                    )
                });
            assert!(
                !accepted_live_records(&fixture.scene.events.recorders[&device_fd])
                    .iter()
                    .skip(record_start)
                    .any(|record| record.kind == AtomicCommitKind::Scene),
                "copied frame is not accepted before pointer motion"
            );
            Backend::warp_pointer_root(
                &mut fixture.scene.events.backend,
                &mut fixture.scene.events.state,
                180,
                130,
            );
            deliver_render_completion(&mut fixture, job_id);
        } else {
            Backend::maybe_composite(&mut fixture.scene.events.backend)
                .expect("compose shared ShowOnRetire at P0 through Backend");
            assert!(
                accepted_live_records(&fixture.scene.events.recorders[&device_fd])
                    .iter()
                    .skip(record_start)
                    .any(|record| record.kind == AtomicCommitKind::Scene),
                "the shared frame is accepted before pointer motion"
            );
            Backend::warp_pointer_root(
                &mut fixture.scene.events.backend,
                &mut fixture.scene.events.state,
                180,
                130,
            );
        }

        let scene_id =
            retire_pending_output(&mut fixture.scene.events, &mut delivered, output_idx, 1);
        assert_ne!(scene_id, 0, "retirement uses the accepted scene event ID");
        let attach = accepted_live_records(&fixture.scene.events.recorders[&device_fd])
            .into_iter()
            .skip(record_start)
            .find(|record| {
                record.kind == AtomicCommitKind::CursorOnly
                    && record_property(record, properties.fb_id).is_some_and(|fb| fb != 0)
            })
            .expect("retirement submits the hardware cursor attach");
        assert_eq!(record_property(&attach, properties.crtc_x), Some(179));
        assert_eq!(record_property(&attach, properties.crtc_y), Some(128));
        assert_ne!(record_property(&attach, properties.crtc_x), Some(119));
        assert_ne!(record_property(&attach, properties.crtc_y), Some(78));
        assert_eq!(
            retire_pending_output(&mut fixture.scene.events, &mut delivered, output_idx, 2),
            attach.user_data,
            "cursor attach retires by its accepted ID"
        );

        Backend::set_cursor_hidden(&mut fixture.scene.events.backend, true);
        damage_root(&mut fixture);
        Backend::maybe_composite(&mut fixture.scene.events.backend)
            .expect("compose cursor detach through Backend");
        for job_id in fixture.scene.pending_render_job_ids() {
            deliver_render_completion(&mut fixture, job_id);
        }
        super::support::finish_atomic_fixture(fixture, &mut delivered);
    }
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: drop the captured-position fallback when no root pointer was recorded; CRTC_X/CRTC_Y must preserve the compose position.
fn retire_show_without_known_pointer_keeps_captured_position() {
    let Some(mut fixture) =
        AtomicPathFixture::new(PathKind::Physical).expect("cursor-retirement fixture construction")
    else {
        return;
    };
    let mut delivered = HashSet::new();
    let cursor = create_argb_cursor(
        &mut fixture.scene.events.backend,
        (4, 4),
        [0x12, 0x34, 0x56, 0xff],
        (1, 2),
    );
    define_root_cursor(&mut fixture.scene.events.backend, cursor);
    let captured_root = (
        fixture.scene.events.backend.core.cursor_x as i32,
        fixture.scene.events.backend.core.cursor_y as i32,
    );
    damage_root(&mut fixture);
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("compose ShowOnRetire without a pointer dispatch");

    let output_idx = 0;
    let device_fd = fixture.scene.kms_fd(output_idx);
    let properties =
        super::support::cursor_properties_for_output(&fixture.scene.events.backend, output_idx);
    let scene_id = retire_pending_output(&mut fixture.scene.events, &mut delivered, output_idx, 1);
    assert_ne!(scene_id, 0, "retirement uses the accepted scene event ID");
    let attach = accepted_live_records(&fixture.scene.events.recorders[&device_fd])
        .into_iter()
        .find(|record| {
            record.kind == AtomicCommitKind::CursorOnly
                && record_property(record, properties.fb_id).is_some_and(|fb| fb != 0)
        })
        .expect("retirement submits the hardware cursor attach");
    assert_eq!(
        record_property(&attach, properties.crtc_x),
        Some(u64::try_from(captured_root.0 - 1).expect("captured x is nonnegative")),
    );
    assert_eq!(
        record_property(&attach, properties.crtc_y),
        Some(u64::try_from(captured_root.1 - 2).expect("captured y is nonnegative")),
    );
    assert_eq!(
        retire_pending_output(&mut fixture.scene.events, &mut delivered, output_idx, 2),
        attach.user_data,
        "cursor attach retires by its accepted ID"
    );

    Backend::set_cursor_hidden(&mut fixture.scene.events.backend, true);
    damage_root(&mut fixture);
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("compose cursor detach through Backend");
    super::support::finish_atomic_fixture(fixture, &mut delivered);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: omit output 1's nonzero origin when translating the latest root pointer to CRTC_X/CRTC_Y.
fn retire_show_uses_output_local_pointer_coordinates() {
    let Some(mut fixture) = AtomicPathFixture::with_output_count(PathKind::Physical, 2)
        .expect("two-output cursor-retirement fixture construction")
    else {
        return;
    };
    let mut delivered = HashSet::new();
    let cursor = create_argb_cursor(
        &mut fixture.scene.events.backend,
        (4, 4),
        [0x12, 0x34, 0x56, 0xff],
        (1, 2),
    );
    define_root_cursor(&mut fixture.scene.events.backend, cursor);
    Backend::warp_pointer_root(
        &mut fixture.scene.events.backend,
        &mut fixture.scene.events.state,
        920,
        100,
    );
    damage_root(&mut fixture);
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("compose output 1 ShowOnRetire at P0 through Backend");
    Backend::warp_pointer_root(
        &mut fixture.scene.events.backend,
        &mut fixture.scene.events.state,
        980,
        140,
    );

    let output_idx = 1;
    let layout = &fixture.scene.events.backend.platform.outputs[output_idx];
    let (layout_origin, crtc) = ((layout.x, layout.y), layout.output.crtc);
    assert_eq!(layout_origin, (800, 0));
    let device_fd = fixture.scene.kms_fd(output_idx);
    let properties =
        super::support::cursor_properties_for_output(&fixture.scene.events.backend, output_idx);
    let scene_id = retire_pending_output(&mut fixture.scene.events, &mut delivered, output_idx, 1);
    assert_ne!(scene_id, 0, "retirement uses the accepted scene event ID");
    let attach = accepted_live_records(&fixture.scene.events.recorders[&device_fd])
        .into_iter()
        .find(|record| {
            record.kind == AtomicCommitKind::CursorOnly
                && record_property(record, properties.fb_id).is_some_and(|fb| fb != 0)
                && record_property(record, properties.crtc_id) == Some(u64::from(u32::from(crtc)))
        })
        .expect("output 1 retirement submits its hardware cursor attach");
    assert_eq!(record_property(&attach, properties.crtc_x), Some(179));
    assert_eq!(record_property(&attach, properties.crtc_y), Some(138));
    assert_ne!(record_property(&attach, properties.crtc_x), Some(979));
    assert_eq!(
        retire_pending_output(&mut fixture.scene.events, &mut delivered, output_idx, 2),
        attach.user_data,
        "output 1 cursor attach retires by its accepted ID"
    );
    assert_ne!(
        retire_pending_output(&mut fixture.scene.events, &mut delivered, 0, 3),
        0,
        "output 0 scene frame also retires by its accepted ID"
    );

    Backend::set_cursor_hidden(&mut fixture.scene.events.backend, true);
    damage_root(&mut fixture);
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("compose output cursor detach through Backend");
    super::support::finish_atomic_fixture(fixture, &mut delivered);
}
#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: classify ENODEV from sprite allocation as transient fallback; the unchanged Atomic path assertions must fail.
fn p2_atomic_cursor_upload_enodev_does_not_demote_the_atomic_path() {
    let Some(mut fixture) =
        AtomicPathFixture::new(PathKind::Physical).expect("Atomic upload fixture")
    else {
        return;
    };
    fixture
        .ledger
        .fail_after(CursorBufferOperation::Create, 0, libc::ENODEV);
    let cursor = create_argb_cursor(
        &mut fixture.scene.events.backend,
        (4, 4),
        [0x21, 0x43, 0x65, 0xff],
        (1, 2),
    );
    define_root_cursor(&mut fixture.scene.events.backend, cursor);
    Backend::warp_pointer_root(
        &mut fixture.scene.events.backend,
        &mut fixture.scene.events.state,
        100,
        90,
    );
    damage_root(&mut fixture);
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("submit scene before the failing cursor upload");
    let mut delivered = HashSet::new();
    retire_pending_output(&mut fixture.scene.events, &mut delivered, 0, 1);
    let device_key = fixture.scene.events.backend.platform.outputs[0]
        .key
        .device_key;
    assert_eq!(
        fixture.scene.events.backend.platform.devices[0]
            .cursor
            .state_path(),
        crate::kms::cursor_plane::topology::CursorStatePath::Atomic,
        "ENODEV from sprite allocation does not demote the static Atomic path"
    );
    assert!(
        !fixture
            .scene
            .events
            .backend
            .platform
            .for_tests_cursor_permanently_disabled(device_key)
    );
    assert_eq!(
        fixture
            .scene
            .events
            .backend
            .platform
            .for_tests_cursor_fallback_remaining(
                device_key,
                fixture.scene.events.backend.platform.outputs[0].output.crtc
            ),
        None,
        "only EINVAL attributes the failure to the cursor"
    );
    assert_eq!(
        fixture.ledger.failures_taken(),
        [CursorBufferOperation::Create]
    );
    Backend::set_cursor_hidden(&mut fixture.scene.events.backend, true);
    damage_root(&mut fixture);
    super::support::finish_atomic_fixture(fixture, &mut delivered);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: route non-EINVAL hide errors through rollback. EOPNOTSUPP must
// retain the binding without changing qualification; EINVAL still detaches.
fn p2_rejected_hide_preserves_errno_and_advances_fallback_once() {
    let run = |error_number: i32| {
        let mut fixture =
            AtomicPathFixture::new(PathKind::Physical).expect("Atomic hide fixture")?;
        let cursor = create_argb_cursor(
            &mut fixture.scene.events.backend,
            (4, 4),
            [0x32, 0x54, 0x76, 0xff],
            (1, 2),
        );
        let mut delivered = HashSet::new();
        let hardware = establish_hardware_cursor(&mut fixture, &mut delivered, cursor, (100, 90));
        let binding = applied_cursor_binding(&fixture.scene.events.backend, hardware.output_idx)
            .expect("applied cursor before hide");
        let device_key = fixture.scene.events.backend.platform.outputs[0]
            .key
            .device_key;
        let recorder = fixture.scene.events.recorders[&hardware.device_fd].clone();
        let offset = record_count(&fixture, hardware.device_fd);
        Backend::set_cursor_hidden(&mut fixture.scene.events.backend, true);
        damage_root(&mut fixture);
        Backend::maybe_composite(&mut fixture.scene.events.backend)
            .expect("submit scene before the rejected hide");
        recorder.script_result(Err(std::io::Error::from_raw_os_error(error_number)));
        retire_pending_output(
            &mut fixture.scene.events,
            &mut delivered,
            hardware.output_idx,
            2,
        );
        assert!(recorder.records().iter().skip(offset).any(|record| {
            record.kind == AtomicCommitKind::CursorOnly
                && record.result
                    == ScriptedResult::Error {
                        kind: std::io::Error::from_raw_os_error(error_number).kind(),
                        raw_os_error: Some(error_number),
                    }
        }));
        let state = &fixture.scene.events.backend;
        let permanent = state
            .platform
            .for_tests_cursor_permanently_disabled(device_key);
        let fallback = state
            .platform
            .for_tests_cursor_fallback_remaining(device_key, hardware.crtc);
        if error_number == libc::EINVAL {
            assert_eq!(
                applied_cursor_binding(state, hardware.output_idx),
                None,
                "attributed EINVAL rollback clears the applied binding"
            );
            assert_eq!(
                sprite_users(state, hardware.output_idx, binding.lease),
                Some(0),
                "attributed EINVAL releases the applied lease",
            );
        } else {
            assert_eq!(
                applied_cursor_binding(state, hardware.output_idx),
                Some(binding),
                "a generic hide error keeps the applied binding"
            );
            assert_eq!(
                sprite_users(state, hardware.output_idx, binding.lease),
                Some(1),
                "a generic hide error keeps the applied lease",
            );
        }
        if applied_cursor_binding(state, hardware.output_idx).is_some() {
            let cleanup_start = record_count(&fixture, hardware.device_fd);
            fixture
                .scene
                .events
                .backend
                .platform
                .cursor_plane_hide_on_crtc(hardware.output_idx)
                .expect("teardown retry hides the still-applied cursor");
            let cleanup = recorder
                .records()
                .into_iter()
                .skip(cleanup_start)
                .find(|record| {
                    record.kind == AtomicCommitKind::CursorOnly
                        && record
                            .events
                            .iter()
                            .any(|event| event.crtc == hardware.crtc)
                        && record.result == ScriptedResult::Ok
                })
                .expect("accepted teardown hide");
            retire_cursor_record(
                &mut fixture,
                &mut delivered,
                hardware.output_idx,
                &cleanup,
                3,
            );
        }
        Backend::set_cursor_hidden(&mut fixture.scene.events.backend, true);
        damage_root(&mut fixture);
        let ledger = Rc::clone(&fixture.ledger);
        super::support::finish_atomic_fixture(fixture, &mut delivered);
        assert_eq!(ledger.end_state(), (0, 0, 0));
        Some((permanent, fallback))
    };

    let Some((permanent, permanent_fallback)) = run(libc::EOPNOTSUPP) else {
        return;
    };
    assert!(
        !permanent,
        "a generic hide error does not change qualification"
    );
    assert_eq!(permanent_fallback, None);

    let Some((permanent, fallback)) = run(libc::EINVAL) else {
        return;
    };
    assert!(!permanent);
    assert_eq!(fallback, Some(1), "one rejected hide advances one fallback");
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: route show through the legacy executor, or drop the SRC_W 16-bit shift.
fn p2_first_hardware_cursor_attaches_with_full_state_and_retires_by_id() {
    let Some(mut fixture) =
        AtomicPathFixture::new(PathKind::Physical).expect("atomic path fixture construction")
    else {
        return;
    };
    let cursor_id = {
        let backend = &mut fixture.scene.events.backend;
        let cursor = create_argb_cursor(backend, (4, 4), [0x12, 0x34, 0x56, 0xff], (1, 2));
        let cursor_id = cursor.as_raw();
        define_root_cursor(backend, cursor);
        cursor_id
    };
    Backend::warp_pointer_root(
        &mut fixture.scene.events.backend,
        &mut fixture.scene.events.state,
        100,
        90,
    );
    damage_root(&mut fixture);
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("compose initial cursor scene");

    let output_idx = 0;
    let layout = &fixture.scene.events.backend.platform.outputs[output_idx];
    let crtc = layout.output.crtc;
    let device_fd = fixture.scene.kms_fd(output_idx);
    let properties =
        super::support::cursor_properties_for_output(&fixture.scene.events.backend, output_idx);
    let mut delivered = HashSet::new();
    let scene_id = retire_pending_output(&mut fixture.scene.events, &mut delivered, output_idx, 1);
    assert_ne!(scene_id, 0);

    let recorder = &fixture.scene.events.recorders[&device_fd];
    let accepted = accepted_live_records(recorder);
    let attach = accepted
        .iter()
        .find(|record| {
            record.kind == AtomicCommitKind::CursorOnly
                && record_property(record, properties.fb_id).is_some()
        })
        .cloned()
        .expect("Atomic first hardware cursor show must submit a CursorOnly attach");

    assert_eq!(attach.kind, AtomicCommitKind::CursorOnly);
    assert_eq!(
        attach.flags,
        ::drm::control::AtomicCommitFlags::NONBLOCK
            | ::drm::control::AtomicCommitFlags::PAGE_FLIP_EVENT
    );
    assert_ne!(attach.user_data, 0);
    assert_eq!(
        record_property(&attach, properties.crtc_id),
        Some(u64::from(u32::from(crtc)))
    );
    assert_eq!(record_property(&attach, properties.crtc_x), Some(99));
    assert_eq!(record_property(&attach, properties.crtc_y), Some(88));
    assert_eq!(record_property(&attach, properties.src_x), Some(0));
    assert_eq!(record_property(&attach, properties.src_y), Some(0));
    assert_eq!(
        record_property(&attach, properties.src_w),
        Some(64_u64 << 16)
    );
    assert_eq!(
        record_property(&attach, properties.src_h),
        Some(64_u64 << 16)
    );
    assert_eq!(record_property(&attach, properties.crtc_w), Some(64));
    assert_eq!(record_property(&attach, properties.crtc_h), Some(64));
    if let Some(hotspot_x) = properties.hotspot_x {
        assert_eq!(record_property(&attach, hotspot_x), None);
    }
    if let Some(hotspot_y) = properties.hotspot_y {
        assert_eq!(record_property(&attach, hotspot_y), None);
    }
    assert_eq!(attach.properties.len(), 10);
    assert!(
        fixture
            .scene
            .events
            .backend
            .platform
            .any_cursor_commit_in_flight()
    );

    let attach_id = attach.user_data;
    assert_eq!(
        retire_pending_output(&mut fixture.scene.events, &mut delivered, output_idx, 2),
        attach_id,
        "the attach is retired by its accepted ID"
    );
    assert!(
        !fixture
            .scene
            .events
            .backend
            .platform
            .any_cursor_commit_in_flight()
    );
    let applied = super::support::applied_cursor_binding(&fixture.scene.events.backend, output_idx)
        .expect("accepted cursor binding");
    assert_eq!((applied.x, applied.y), (99, 88));
    assert_eq!(
        super::support::sprite_users(&fixture.scene.events.backend, output_idx, applied.lease),
        Some(1)
    );

    Backend::set_cursor_hidden(&mut fixture.scene.events.backend, true);
    damage_root(&mut fixture);
    Backend::maybe_composite(&mut fixture.scene.events.backend).expect("compose cursor hide scene");
    retire_pending_output(&mut fixture.scene.events, &mut delivered, output_idx, 3);
    let detach = accepted_live_records(&fixture.scene.events.recorders[&device_fd])
        .into_iter()
        .find(|record| {
            record.kind == AtomicCommitKind::CursorOnly
                && record_property(record, properties.fb_id) == Some(0)
                && record_property(record, properties.crtc_id) == Some(0)
        })
        .expect("hide must submit an atomic plane detach");
    assert!(
        !fixture
            .scene
            .events
            .backend
            .platform
            .cursor_plane_visible_for_output(output_idx)
    );
    assert_eq!(
        super::support::sprite_users(&fixture.scene.events.backend, output_idx, applied.lease),
        Some(1),
        "the accepted detach retains the old sprite until its event"
    );
    assert_eq!(
        retire_pending_output(&mut fixture.scene.events, &mut delivered, output_idx, 4),
        detach.user_data
    );
    assert_eq!(
        super::support::sprite_users(&fixture.scene.events.backend, output_idx, applied.lease),
        Some(0)
    );
    let root = fixture.scene.events.backend.core.window_id;
    Backend::define_cursor(&mut fixture.scene.events.backend, None, root, 0x7f10)
        .expect("restore recording scene's sticky default cursor");
    Backend::free_cursor(&mut fixture.scene.events.backend, None, cursor_id)
        .expect("free test cursor after detach");
    super::support::finish_fixture(&mut fixture.scene.events, &mut delivered);
    fixture.scene.finish_after_normal_drain();
    assert_eq!(fixture.ledger.end_state(), (0, 0, 0));
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: omit HOTSPOT_X/HOTSPOT_Y on an Atomic plane that exposes them.
fn p2_paravirtualized_attach_carries_hotspot_properties() {
    let Some(mut fixture) = AtomicPathFixture::new(PathKind::Paravirtualized)
        .expect("paravirtualized cursor fixture construction")
    else {
        return;
    };
    let cursor_a = create_argb_cursor(
        &mut fixture.scene.events.backend,
        (4, 4),
        [0x23, 0x45, 0x67, 0xff],
        (1, 2),
    );
    let mut delivered = HashSet::new();
    let hardware = establish_hardware_cursor(&mut fixture, &mut delivered, cursor_a, (100, 90));
    let properties = super::support::cursor_properties_for_output(
        &fixture.scene.events.backend,
        hardware.output_idx,
    );
    let attach = accepted_live_records(&fixture.scene.events.recorders[&hardware.device_fd])
        .into_iter()
        .find(|record| record.user_data == hardware.attach_commit_id)
        .expect("accepted attach record remains in the recorder");
    assert_eq!(
        record_property(&attach, properties.hotspot_x.unwrap()),
        Some(1)
    );
    assert_eq!(
        record_property(&attach, properties.hotspot_y.unwrap()),
        Some(2)
    );

    let first = applied_cursor_binding(&fixture.scene.events.backend, hardware.output_idx)
        .expect("first cursor is applied");
    let cursor_b = create_argb_cursor(
        &mut fixture.scene.events.backend,
        (4, 4),
        [0x23, 0x45, 0x67, 0xff],
        (3, 4),
    );
    let cursor_b_id = cursor_b.as_raw();
    define_root_cursor(&mut fixture.scene.events.backend, cursor_b);
    let offset = record_count(&fixture, hardware.device_fd);
    let root = fixture.scene.events.backend.core.window_id;
    let width = fixture.scene.events.backend.platform.fb_w;
    let height = fixture.scene.events.backend.platform.fb_h;
    Backend::clear_area(
        &mut fixture.scene.events.backend,
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
    .expect("damage root for hotspot replacement");
    let replacement = compose_until_cursor_commit(
        &mut fixture,
        &mut delivered,
        hardware.output_idx,
        offset,
        properties.hotspot_x.unwrap(),
        3,
    );
    assert_ne!(
        record_property(&replacement, properties.fb_id),
        Some(first.framebuffer.into())
    );
    assert_eq!(
        record_property(&replacement, properties.hotspot_x.unwrap()),
        Some(3)
    );
    assert_eq!(
        record_property(&replacement, properties.hotspot_y.unwrap()),
        Some(4)
    );
    let second = applied_cursor_binding(&fixture.scene.events.backend, hardware.output_idx)
        .expect("second cursor is applied at acceptance");
    assert_eq!(
        second.lease.version(),
        fixture.scene.events.backend.cursor_records[&cursor_b_id].version
    );
    assert_eq!(
        sprite_users(
            &fixture.scene.events.backend,
            hardware.output_idx,
            first.lease
        ),
        Some(1)
    );
    assert_eq!(
        sprite_users(
            &fixture.scene.events.backend,
            hardware.output_idx,
            second.lease
        ),
        Some(1)
    );
    retire_cursor_record(
        &mut fixture,
        &mut delivered,
        hardware.output_idx,
        &replacement,
        6,
    );
    assert_eq!(
        sprite_users(
            &fixture.scene.events.backend,
            hardware.output_idx,
            first.lease
        ),
        Some(0)
    );

    let detach = accept_cursor_detach(&mut fixture, &mut delivered, hardware.output_idx);
    assert!(
        !fixture
            .scene
            .events
            .backend
            .platform
            .cursor_plane_visible_for_output(hardware.output_idx)
    );
    assert_eq!(
        sprite_users(
            &fixture.scene.events.backend,
            hardware.output_idx,
            second.lease
        ),
        Some(1)
    );
    retire_cursor_record(
        &mut fixture,
        &mut delivered,
        hardware.output_idx,
        &detach,
        7,
    );
    assert_eq!(
        sprite_users(
            &fixture.scene.events.backend,
            hardware.output_idx,
            second.lease
        ),
        Some(0)
    );

    super::support::finish_atomic_fixture(fixture, &mut delivered);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: release the old sprite when the replacement is accepted instead of when its ID retires.
fn p2_image_replacement_keeps_the_old_sprite_until_the_replacing_event() {
    let Some(mut fixture) =
        AtomicPathFixture::new(PathKind::Physical).expect("atomic cursor fixture construction")
    else {
        return;
    };
    let cursor_a = create_argb_cursor(
        &mut fixture.scene.events.backend,
        (4, 4),
        [0x31, 0x41, 0x59, 0xff],
        (1, 2),
    );
    let mut delivered = HashSet::new();
    let hardware = establish_hardware_cursor(&mut fixture, &mut delivered, cursor_a, (100, 90));
    let properties = super::support::cursor_properties_for_output(
        &fixture.scene.events.backend,
        hardware.output_idx,
    );
    let binding_a = applied_cursor_binding(&fixture.scene.events.backend, hardware.output_idx)
        .expect("cursor A applied");
    let bytes_a = sprite_bytes(
        &fixture.scene.events.backend,
        hardware.output_idx,
        binding_a.lease,
    )
    .expect("cursor A sprite bytes");

    let cursor_b = create_argb_cursor(
        &mut fixture.scene.events.backend,
        (4, 4),
        [0x62, 0x71, 0x82, 0xff],
        (1, 2),
    );
    define_root_cursor(&mut fixture.scene.events.backend, cursor_b);
    let offset = record_count(&fixture, hardware.device_fd);
    damage_root(&mut fixture);
    let replacement = compose_until_cursor_commit_where(
        &mut fixture,
        &mut delivered,
        hardware.output_idx,
        offset,
        properties.fb_id,
        |record| {
            record_property(record, properties.fb_id) != Some(u64::from(binding_a.framebuffer))
        },
    );
    assert_eq!(replacement.properties.len(), 1);
    assert_eq!(replacement.properties[0].property, properties.fb_id);
    let binding_b = applied_cursor_binding(&fixture.scene.events.backend, hardware.output_idx)
        .expect("cursor B becomes applied at acceptance");
    assert_ne!(binding_a.lease, binding_b.lease);
    assert_eq!(
        sprite_users(
            &fixture.scene.events.backend,
            hardware.output_idx,
            binding_a.lease
        ),
        Some(1)
    );
    assert_eq!(
        sprite_users(
            &fixture.scene.events.backend,
            hardware.output_idx,
            binding_b.lease
        ),
        Some(1)
    );
    assert_eq!(
        sprite_bytes(
            &fixture.scene.events.backend,
            hardware.output_idx,
            binding_a.lease
        ),
        Some(bytes_a.clone())
    );
    assert!(!fixture.ledger.records().iter().any(|record| matches!(
        record,
        crate::kms::cursor_plane::ops::recording::CursorBufferRecord::RemoveFb { fb, .. }
            if *fb == binding_a.framebuffer
    )));

    retire_cursor_record(
        &mut fixture,
        &mut delivered,
        hardware.output_idx,
        &replacement,
        6,
    );
    assert_eq!(
        sprite_users(
            &fixture.scene.events.backend,
            hardware.output_idx,
            binding_a.lease
        ),
        Some(0)
    );
    let replacement_id = replacement.user_data;
    fixture.scene.events.queue_commit_completion(
        hardware.device_fd,
        hardware.crtc,
        replacement_id,
        7,
        std::time::Duration::from_millis(70),
    );
    Backend::on_page_flip_ready(
        &mut fixture.scene.events.backend,
        &mut fixture.scene.events.state,
        hardware.device_fd,
    );
    assert_eq!(
        sprite_users(
            &fixture.scene.events.backend,
            hardware.output_idx,
            binding_a.lease
        ),
        Some(0)
    );
    assert_eq!(
        sprite_users(
            &fixture.scene.events.backend,
            hardware.output_idx,
            binding_b.lease
        ),
        Some(1)
    );

    let detach = accept_cursor_detach(&mut fixture, &mut delivered, hardware.output_idx);
    assert_eq!(
        sprite_users(
            &fixture.scene.events.backend,
            hardware.output_idx,
            binding_b.lease
        ),
        Some(1)
    );
    retire_cursor_record(
        &mut fixture,
        &mut delivered,
        hardware.output_idx,
        &detach,
        8,
    );
    assert_eq!(
        sprite_users(
            &fixture.scene.events.backend,
            hardware.output_idx,
            binding_b.lease
        ),
        Some(0)
    );
    super::support::finish_atomic_fixture(fixture, &mut delivered);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: allow an unmatched explicit cookie to retire the pending cursor replacement.
fn p2_wrong_explicit_id_is_dropped_before_cursor_retirement() {
    let Some(mut fixture) =
        AtomicPathFixture::new(PathKind::Physical).expect("atomic cursor fixture construction")
    else {
        return;
    };
    let cursor_a = create_argb_cursor(
        &mut fixture.scene.events.backend,
        (4, 4),
        [1, 2, 3, 255],
        (1, 2),
    );
    let mut delivered = HashSet::new();
    let hardware = establish_hardware_cursor(&mut fixture, &mut delivered, cursor_a, (100, 90));
    let properties = super::support::cursor_properties_for_output(&fixture.scene.events.backend, 0);
    let binding_a = applied_cursor_binding(&fixture.scene.events.backend, 0).unwrap();
    let cursor_b = create_argb_cursor(
        &mut fixture.scene.events.backend,
        (4, 4),
        [4, 5, 6, 255],
        (1, 2),
    );
    define_root_cursor(&mut fixture.scene.events.backend, cursor_b);
    let offset = record_count(&fixture, hardware.device_fd);
    damage_root(&mut fixture);
    let replacement = compose_until_cursor_commit_where(
        &mut fixture,
        &mut delivered,
        0,
        offset,
        properties.fb_id,
        |record| {
            record_property(record, properties.fb_id) != Some(u64::from(binding_a.framebuffer))
        },
    );
    let binding_b = applied_cursor_binding(&fixture.scene.events.backend, 0).unwrap();
    fixture.scene.events.queue_commit_completion(
        hardware.device_fd,
        hardware.crtc,
        u64::MAX,
        6,
        std::time::Duration::from_millis(60),
    );
    Backend::on_page_flip_ready(
        &mut fixture.scene.events.backend,
        &mut fixture.scene.events.state,
        hardware.device_fd,
    );
    assert!(
        fixture
            .scene
            .events
            .backend
            .platform
            .cursor_commit_in_flight_for_output(0)
    );
    assert_eq!(
        sprite_users(&fixture.scene.events.backend, 0, binding_a.lease),
        Some(1)
    );
    assert_eq!(
        sprite_users(&fixture.scene.events.backend, 0, binding_b.lease),
        Some(1)
    );
    retire_cursor_record(&mut fixture, &mut delivered, 0, &replacement, 7);
    assert_eq!(
        sprite_users(&fixture.scene.events.backend, 0, binding_a.lease),
        Some(0)
    );
    let detach = accept_cursor_detach(&mut fixture, &mut delivered, 0);
    retire_cursor_record(&mut fixture, &mut delivered, 0, &detach, 8);
    super::support::finish_atomic_fixture(fixture, &mut delivered);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: refuse zero-cookie FIFO attribution or release the prior before a matching event.
fn p2_zero_cookie_event_retires_the_pending_replacement_exactly_once() {
    let Some(mut fixture) =
        AtomicPathFixture::new(PathKind::Physical).expect("atomic cursor fixture construction")
    else {
        return;
    };
    let cursor_a = create_argb_cursor(
        &mut fixture.scene.events.backend,
        (4, 4),
        [7, 8, 9, 255],
        (1, 2),
    );
    let mut delivered = HashSet::new();
    let hardware = establish_hardware_cursor(&mut fixture, &mut delivered, cursor_a, (100, 90));
    let properties = super::support::cursor_properties_for_output(&fixture.scene.events.backend, 0);
    let binding_a = applied_cursor_binding(&fixture.scene.events.backend, 0).unwrap();
    let cursor_b = create_argb_cursor(
        &mut fixture.scene.events.backend,
        (4, 4),
        [10, 11, 12, 255],
        (1, 2),
    );
    define_root_cursor(&mut fixture.scene.events.backend, cursor_b);
    let offset = record_count(&fixture, hardware.device_fd);
    damage_root(&mut fixture);
    let replacement = compose_until_cursor_commit_where(
        &mut fixture,
        &mut delivered,
        0,
        offset,
        properties.fb_id,
        |record| {
            record_property(record, properties.fb_id) != Some(u64::from(binding_a.framebuffer))
        },
    );
    let binding_b = applied_cursor_binding(&fixture.scene.events.backend, 0).unwrap();
    fixture.scene.events.queue_commit_completion(
        hardware.device_fd,
        hardware.crtc,
        0,
        6,
        std::time::Duration::from_millis(60),
    );
    Backend::on_page_flip_ready(
        &mut fixture.scene.events.backend,
        &mut fixture.scene.events.state,
        hardware.device_fd,
    );
    assert!(
        !fixture
            .scene
            .events
            .backend
            .platform
            .cursor_commit_in_flight_for_output(0)
    );
    assert_eq!(
        sprite_users(&fixture.scene.events.backend, 0, binding_a.lease),
        Some(0)
    );
    assert_eq!(
        sprite_users(&fixture.scene.events.backend, 0, binding_b.lease),
        Some(1)
    );
    assert_ne!(replacement.user_data, 0);
    drain_unretired_output_events(&mut fixture, &mut delivered, 0);
    let sink_pending = fixture.scene.events.backend.platform.devices[0]
        .atomic_commit_sink
        .for_tests_pending_page_flip_entry_count();
    assert_eq!(
        sink_pending, 0,
        "the duplicate zero cookie has no pending association"
    );
    fixture.scene.events.queue_commit_completion(
        hardware.device_fd,
        hardware.crtc,
        0,
        7,
        std::time::Duration::from_millis(70),
    );
    Backend::on_page_flip_ready(
        &mut fixture.scene.events.backend,
        &mut fixture.scene.events.state,
        hardware.device_fd,
    );
    assert_eq!(
        sprite_users(&fixture.scene.events.backend, 0, binding_a.lease),
        Some(0)
    );
    assert_eq!(
        sprite_users(&fixture.scene.events.backend, 0, binding_b.lease),
        Some(1)
    );
    let detach = accept_cursor_detach(&mut fixture, &mut delivered, 0);
    retire_cursor_record(&mut fixture, &mut delivered, 0, &detach, 8);
    super::support::finish_atomic_fixture(fixture, &mut delivered);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: leave remember_atomic_position's pending move after the accepted show.
fn p2_accepted_show_leaves_no_pending_move() {
    let Some(mut fixture) =
        AtomicPathFixture::new(PathKind::Physical).expect("atomic cursor fixture construction")
    else {
        return;
    };
    let cursor = create_argb_cursor(
        &mut fixture.scene.events.backend,
        (4, 4),
        [13, 14, 15, 255],
        (1, 2),
    );
    let mut delivered = HashSet::new();
    let hardware = establish_hardware_cursor(&mut fixture, &mut delivered, cursor, (100, 90));
    let device_key = fixture.scene.events.backend.platform.outputs[0]
        .key
        .device_key;
    assert_eq!(
        fixture
            .scene
            .events
            .backend
            .platform
            .for_tests_atomic_cursor_pending_position(device_key, hardware.crtc),
        None
    );
    let offset = record_count(&fixture, hardware.device_fd);
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("composite without motion after the accepted attach");
    assert!(
        !accepted_live_records(&fixture.scene.events.recorders[&hardware.device_fd])
            .into_iter()
            .skip(offset)
            .any(|record| record.kind == AtomicCommitKind::CursorOnly)
    );
    let detach = accept_cursor_detach(&mut fixture, &mut delivered, 0);
    retire_cursor_record(&mut fixture, &mut delivered, 0, &detach, 7);
    super::support::finish_atomic_fixture(fixture, &mut delivered);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: hide the scene cursor without submitting its atomic FB_ID/CRTC_ID detach.
fn p2_hide_detaches_and_the_event_releases_the_sprite() {
    let Some(mut fixture) =
        AtomicPathFixture::new(PathKind::Physical).expect("atomic cursor fixture construction")
    else {
        return;
    };
    let cursor = create_argb_cursor(
        &mut fixture.scene.events.backend,
        (4, 4),
        [16, 17, 18, 255],
        (1, 2),
    );
    let mut delivered = HashSet::new();
    let hardware = establish_hardware_cursor(&mut fixture, &mut delivered, cursor, (100, 90));
    let properties = super::support::cursor_properties_for_output(&fixture.scene.events.backend, 0);
    let binding = applied_cursor_binding(&fixture.scene.events.backend, 0).unwrap();
    let detach = accept_cursor_detach(&mut fixture, &mut delivered, 0);
    assert_eq!(detach.kind, AtomicCommitKind::CursorOnly);
    assert_eq!(record_property(&detach, properties.fb_id), Some(0));
    assert_eq!(record_property(&detach, properties.crtc_id), Some(0));
    assert_eq!(detach.properties.len(), 2);
    assert!(
        !fixture
            .scene
            .events
            .backend
            .platform
            .cursor_plane_visible_for_output(0)
    );
    assert_eq!(
        sprite_users(&fixture.scene.events.backend, 0, binding.lease),
        Some(1)
    );
    retire_cursor_record(&mut fixture, &mut delivered, 0, &detach, 7);
    assert_eq!(
        sprite_users(&fixture.scene.events.backend, 0, binding.lease),
        Some(0)
    );
    assert!(
        !fixture
            .scene
            .events
            .backend
            .platform
            .cursor_commit_in_flight_for_output(0)
    );
    let _ = hardware;
    super::support::finish_atomic_fixture(fixture, &mut delivered);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: accept the new lease or reveal the software cursor when the Atomic show returns EBUSY.
fn p2_ebusy_show_defers_without_consuming_a_lease() {
    let Some(mut fixture) =
        AtomicPathFixture::new(PathKind::Physical).expect("atomic cursor fixture construction")
    else {
        return;
    };
    let cursor = create_argb_cursor(
        &mut fixture.scene.events.backend,
        (4, 4),
        [19, 20, 21, 255],
        (1, 2),
    );
    define_root_cursor(&mut fixture.scene.events.backend, cursor);
    Backend::warp_pointer_root(
        &mut fixture.scene.events.backend,
        &mut fixture.scene.events.state,
        100,
        90,
    );
    damage_root(&mut fixture);
    let device_fd = fixture.scene.kms_fd(0);
    let recorder = fixture.scene.events.recorders[&device_fd].clone();
    let start = record_count(&fixture, device_fd);
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("submit scene transition before the scripted cursor result");
    let mode_before = fixture.scene.events.backend.scene.cursor_mode();
    recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EBUSY)));
    let mut delivered = HashSet::new();
    retire_pending_output(&mut fixture.scene.events, &mut delivered, 0, 1);
    let failed = recorder
        .records()
        .into_iter()
        .skip(start)
        .find(|record| {
            record.device_fd == device_fd
                && record.kind == AtomicCommitKind::CursorOnly
                && record.result != ScriptedResult::Ok
        })
        .expect("scripted Atomic cursor show must return EBUSY");
    let properties = super::support::cursor_properties_for_output(&fixture.scene.events.backend, 0);
    let device_key = fixture.scene.events.backend.platform.outputs[0]
        .key
        .device_key;
    let crtc = fixture.scene.events.backend.platform.outputs[0].output.crtc;
    let candidate = {
        let plane = fixture.scene.events.backend.platform.devices[0]
            .cursor
            .plane
            .as_ref()
            .unwrap();
        assert_eq!(plane.sprites().prepared_user_count(), 0);
        let candidate = plane.sprites().candidate_lease().unwrap();
        assert_eq!(plane.sprites().user_count(candidate), Some(0));
        candidate
    };
    assert_ne!(failed.user_data, 0);
    assert!(
        !fixture
            .scene
            .events
            .backend
            .platform
            .cursor_commit_in_flight_for_output(0)
    );
    assert_eq!(
        applied_cursor_binding(&fixture.scene.events.backend, 0),
        None
    );
    assert_eq!(
        fixture.scene.events.backend.scene.cursor_mode(),
        mode_before
    );
    assert!(!matches!(
        mode_before,
        yserver_render::kms::render::scene::CursorPlaneMode::Sw
    ));
    assert!(!matches!(
        mode_before,
        yserver_render::kms::render::scene::CursorPlaneMode::Hidden
    ));
    assert_eq!(
        fixture
            .scene
            .events
            .backend
            .platform
            .for_tests_atomic_cursor_pending_position(device_key, crtc),
        None
    );

    let offset = record_count(&fixture, device_fd);
    let root = fixture.scene.events.backend.core.window_id;
    let width = fixture.scene.events.backend.platform.fb_w;
    let height = fixture.scene.events.backend.platform.fb_h;
    Backend::clear_area(
        &mut fixture.scene.events.backend,
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
    .expect("request the forced cursor retry repaint");
    let attach = compose_until_cursor_commit_where(
        &mut fixture,
        &mut delivered,
        0,
        offset,
        properties.fb_id,
        |record| record_property(record, properties.fb_id).is_some_and(|fb| fb != 0),
    );
    assert_ne!(
        attach.user_data, failed.user_data,
        "retry receives a fresh commit ID"
    );
    let plane = fixture.scene.events.backend.platform.devices[0]
        .cursor
        .plane
        .as_ref()
        .unwrap();
    assert_eq!(plane.sprites().user_count(candidate), Some(1));
    retire_cursor_record(&mut fixture, &mut delivered, 0, &attach, 3);
    let detach = accept_cursor_detach(&mut fixture, &mut delivered, 0);
    retire_cursor_record(&mut fixture, &mut delivered, 0, &detach, 4);
    super::support::finish_atomic_fixture(fixture, &mut delivered);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: restore the pending.version < version check; B-created-first must fail the named retry-version assertion.
fn p2_current_cursor_bind_clears_a_superseded_deferred_show_retry() {
    p2_current_cursor_bind_clears_a_superseded_deferred_show_retry_scenario(false);
    p2_current_cursor_bind_clears_a_superseded_deferred_show_retry_scenario(true);
}

fn p2_current_cursor_bind_clears_a_superseded_deferred_show_retry_scenario(
    create_b_before_a: bool,
) {
    let Some(mut fixture) =
        AtomicPathFixture::new(PathKind::Physical).expect("atomic cursor fixture construction")
    else {
        return;
    };
    let cursor_b = create_b_before_a.then(|| {
        create_argb_cursor(
            &mut fixture.scene.events.backend,
            (4, 4),
            [32, 33, 34, 255],
            (1, 2),
        )
    });
    let cursor_a = create_argb_cursor(
        &mut fixture.scene.events.backend,
        (4, 4),
        [19, 20, 21, 255],
        (1, 2),
    );
    let cursor_a_xid = cursor_a.as_raw();
    define_root_cursor(&mut fixture.scene.events.backend, cursor_a);
    Backend::warp_pointer_root(
        &mut fixture.scene.events.backend,
        &mut fixture.scene.events.state,
        100,
        90,
    );
    assert!(matches!(
        fixture.scene.events.backend.scene.cursor_mode(),
        yserver_render::kms::render::scene::CursorPlaneMode::Hidden
            | yserver_render::kms::render::scene::CursorPlaneMode::Sw
    ));
    damage_root(&mut fixture);

    let device_fd = fixture.scene.kms_fd(0);
    let recorder = fixture.scene.events.recorders[&device_fd].clone();
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("submit A's Hidden/Sw-to-Hw scene transition");
    recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EBUSY)));
    let mut delivered = HashSet::new();
    retire_pending_output(&mut fixture.scene.events, &mut delivered, 0, 1);

    let retry_a = fixture
        .scene
        .events
        .backend
        .scene
        .for_tests_cursor_retry_version(0)
        .expect("A's deferred show leaves a retry version");
    let cursor_b = cursor_b.unwrap_or_else(|| {
        create_argb_cursor(
            &mut fixture.scene.events.backend,
            (4, 4),
            [32, 33, 34, 255],
            (1, 2),
        )
    });
    let cursor_b_xid = cursor_b.as_raw();
    let version_a = fixture.scene.events.backend.cursor_records[&cursor_a_xid].version;
    let version_b = fixture.scene.events.backend.cursor_records[&cursor_b_xid].version;
    assert_eq!(retry_a, version_a, "the deferred retry belongs to A");
    assert_eq!(
        version_b < version_a,
        create_b_before_a,
        "B's cursor version reflects whether B was created before A"
    );
    assert_eq!(
        fixture.scene.events.backend.scene.cursor_mode(),
        yserver_render::kms::render::scene::CursorPlaneMode::Mixed,
        "a deferred Hidden/Sw show keeps the scene out of steady Hw mode"
    );
    assert_eq!(
        fixture.scene.events.backend.effective_cursor_xid,
        Some(cursor_a_xid),
        "A is current when its show is deferred"
    );
    assert_eq!(
        applied_cursor_binding(&fixture.scene.events.backend, 0),
        None,
        "A's deferred show did not bind"
    );

    define_root_cursor(&mut fixture.scene.events.backend, cursor_b);
    assert_eq!(
        fixture.scene.events.backend.effective_cursor_xid,
        Some(cursor_b_xid),
        "select B before retrying A's deferred show"
    );
    assert_eq!(
        fixture
            .scene
            .events
            .backend
            .scene
            .for_tests_cursor_retry_version(0),
        Some(retry_a),
        "B selection is still carrying A's superseded retry marker"
    );

    let offset = record_count(&fixture, device_fd);
    damage_root(&mut fixture);
    let properties = super::support::cursor_properties_for_output(&fixture.scene.events.backend, 0);
    let bind_b = compose_until_cursor_commit_where(
        &mut fixture,
        &mut delivered,
        0,
        offset,
        properties.fb_id,
        |record| record_property(record, properties.fb_id).is_some_and(|fb_id| fb_id != 0),
    );
    assert_eq!(
        fixture
            .scene
            .events
            .backend
            .scene
            .for_tests_cursor_retry_version(0),
        None,
        "successful B bind clears the superseded deferred retry version"
    );
    assert_eq!(
        fixture.scene.events.backend.scene.cursor_mode(),
        yserver_render::kms::render::scene::CursorPlaneMode::Hw,
        "the scene leaves Mixed after current cursor B binds"
    );
    assert!(
        fixture.scene.events.backend.direct_cursor_eligible_for(
            &(0..fixture.scene.events.backend.platform.outputs.len()).collect::<Vec<_>>(),
        ),
        "a bound current cursor makes the direct cursor gate eligible"
    );
    assert_eq!(
        applied_cursor_binding(&fixture.scene.events.backend, 0)
            .map(|binding| binding.lease.version()),
        Some(
            fixture.scene.events.backend.platform.devices[0]
                .cursor
                .plane
                .as_ref()
                .unwrap()
                .sprites()
                .candidate_version()
                .expect("B is the current sprite candidate")
        ),
        "the applied binding names the current B sprite"
    );
    retire_cursor_record(&mut fixture, &mut delivered, 0, &bind_b, 3);

    let motion_offset = record_count(&fixture, device_fd);
    Backend::warp_pointer_root(
        &mut fixture.scene.events.backend,
        &mut fixture.scene.events.state,
        108,
        96,
    );
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("process pointer motion after the deferred retry settles");
    let motion_records: Vec<_> = recorder
        .records()
        .into_iter()
        .skip(motion_offset)
        .filter(|record| record.device_fd == device_fd && record.result == ScriptedResult::Ok)
        .collect();
    assert!(
        motion_records.iter().all(|record| {
            !matches!(
                record.kind,
                AtomicCommitKind::Scene | AtomicCommitKind::SceneWithCursor
            )
        }),
        "pointer motion after B binds does not force a composed scene frame"
    );
    assert_eq!(
        fixture.scene.events.backend.scene.cursor_mode(),
        yserver_render::kms::render::scene::CursorPlaneMode::Hw
    );

    if let Some(motion) = motion_records
        .iter()
        .find(|record| record.kind == AtomicCommitKind::CursorOnly)
    {
        retire_cursor_record(&mut fixture, &mut delivered, 0, motion, 4);
    }
    let detach = accept_cursor_detach(&mut fixture, &mut delivered, 0);
    retire_cursor_record(&mut fixture, &mut delivered, 0, &detach, 5);
    super::support::finish_atomic_fixture(fixture, &mut delivered);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: retain an attach reservation after its cursor-only submit is rejected.
fn p2_rejected_attach_rolls_back_without_retaining_reservation() {
    let Some(mut fixture) =
        AtomicPathFixture::new(PathKind::Physical).expect("atomic cursor fixture construction")
    else {
        return;
    };
    let cursor = create_argb_cursor(
        &mut fixture.scene.events.backend,
        (4, 4),
        [22, 23, 24, 255],
        (1, 2),
    );
    define_root_cursor(&mut fixture.scene.events.backend, cursor);
    Backend::warp_pointer_root(
        &mut fixture.scene.events.backend,
        &mut fixture.scene.events.state,
        100,
        90,
    );
    damage_root(&mut fixture);
    let device_fd = fixture.scene.kms_fd(0);
    let recorder = fixture.scene.events.recorders[&device_fd].clone();
    let start = record_count(&fixture, device_fd);
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("submit scene transition before the scripted cursor result");
    recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EINVAL)));
    let mut delivered = HashSet::new();
    retire_pending_output(&mut fixture.scene.events, &mut delivered, 0, 1);
    let records = recorder.records();
    let rejected = records
        .iter()
        .skip(start)
        .find(|record| {
            record.device_fd == device_fd
                && record.kind == AtomicCommitKind::CursorOnly
                && record.result != ScriptedResult::Ok
        })
        .expect("scripted Atomic attach rejection");
    let device_key = fixture.scene.events.backend.platform.outputs[0]
        .key
        .device_key;
    let crtc = fixture.scene.events.backend.platform.outputs[0].output.crtc;
    let plane = fixture.scene.events.backend.platform.devices[0]
        .cursor
        .plane
        .as_ref()
        .unwrap();
    assert_ne!(rejected.user_data, 0);
    assert_eq!(plane.sprites().prepared_user_count(), 0);
    assert!(
        plane
            .sprites()
            .candidate_lease()
            .is_some_and(|lease| plane.sprites().user_count(lease) == Some(0))
    );
    assert!(
        !fixture
            .scene
            .events
            .backend
            .platform
            .cursor_commit_in_flight_for_output(0)
    );
    assert!(records.iter().skip(start).all(|record| {
        record.flags != ::drm::control::AtomicCommitFlags::empty() || !record.events.is_empty()
    }));
    assert_eq!(
        fixture
            .scene
            .events
            .backend
            .platform
            .for_tests_cursor_fallback_remaining(device_key, crtc),
        Some(1)
    );
    Backend::set_cursor_hidden(&mut fixture.scene.events.backend, true);
    damage_root(&mut fixture);
    let ledger = Rc::clone(&fixture.ledger);
    super::support::finish_atomic_fixture(fixture, &mut delivered);
    assert_eq!(ledger.end_state(), (0, 0, 0));
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: release the applied lease before blocking detach succeeds after replacement rejection.
fn p2_rejected_replace_detaches_the_applied_plane_and_releases_after_it() {
    let run = |blocking_error: Option<i32>| {
        let Some(mut fixture) =
            AtomicPathFixture::new(PathKind::Physical).expect("atomic cursor fixture construction")
        else {
            return;
        };
        let cursor_a = create_argb_cursor(
            &mut fixture.scene.events.backend,
            (4, 4),
            [25, 26, 27, 255],
            (1, 2),
        );
        let mut delivered = HashSet::new();
        let hardware = establish_hardware_cursor(&mut fixture, &mut delivered, cursor_a, (100, 90));
        let properties =
            super::support::cursor_properties_for_output(&fixture.scene.events.backend, 0);
        let binding_a = applied_cursor_binding(&fixture.scene.events.backend, 0).unwrap();
        let cursor_b = create_argb_cursor(
            &mut fixture.scene.events.backend,
            (4, 4),
            [28, 29, 30, 255],
            (1, 2),
        );
        define_root_cursor(&mut fixture.scene.events.backend, cursor_b);
        let offset = record_count(&fixture, hardware.device_fd);
        damage_root(&mut fixture);
        Backend::maybe_composite(&mut fixture.scene.events.backend)
            .expect("submit scene transition before scripted cursor rejection");
        let recorder = fixture.scene.events.recorders[&hardware.device_fd].clone();
        recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EINVAL)));
        if let Some(raw_error) = blocking_error {
            recorder.script_result(Err(std::io::Error::from_raw_os_error(raw_error)));
        }
        retire_pending_output(&mut fixture.scene.events, &mut delivered, 0, 5);
        let records = recorder.records();
        let failed_show = records
            .iter()
            .skip(offset)
            .find(|record| {
                record.device_fd == hardware.device_fd
                    && record.kind == AtomicCommitKind::CursorOnly
                    && record.result != ScriptedResult::Ok
                    && !record.flags.is_empty()
            })
            .expect("replacement cursor commit rejected with EINVAL");
        assert_ne!(failed_show.user_data, 0);
        let blocking_detach = records
            .iter()
            .skip(offset)
            .find(|record| {
                record.device_fd == hardware.device_fd
                    && record.kind == AtomicCommitKind::CursorOnly
                    && record.flags.is_empty()
                    && record.events.is_empty()
            })
            .cloned();
        let device_key = fixture.scene.events.backend.platform.outputs[0]
            .key
            .device_key;
        let crtc = fixture.scene.events.backend.platform.outputs[0].output.crtc;
        if blocking_error.is_some() {
            let blocking_detach = blocking_detach.expect("failed blocking Atomic rollback record");
            assert_ne!(blocking_detach.result, ScriptedResult::Ok);
            assert!(blocking_detach.flags.is_empty());
            assert!(blocking_detach.events.is_empty());
            assert_eq!(
                applied_cursor_binding(&fixture.scene.events.backend, 0),
                Some(binding_a)
            );
            assert_eq!(
                sprite_users(&fixture.scene.events.backend, 0, binding_a.lease),
                Some(1)
            );
            assert_eq!(
                fixture
                    .scene
                    .events
                    .backend
                    .platform
                    .for_tests_cursor_hide_recovery_attempts(device_key, crtc),
                Some(0)
            );
        } else {
            let blocking_detach = blocking_detach.expect("successful blocking Atomic rollback");
            assert_eq!(blocking_detach.result, ScriptedResult::Ok);
            assert_eq!(
                blocking_detach.flags,
                ::drm::control::AtomicCommitFlags::empty()
            );
            assert!(blocking_detach.events.is_empty());
            assert_eq!(blocking_detach.properties.len(), 2);
            assert!(
                blocking_detach
                    .properties
                    .iter()
                    .all(|update| update.object == properties.plane.into())
            );
            assert_eq!(record_property(&blocking_detach, properties.fb_id), Some(0));
            assert_eq!(
                record_property(&blocking_detach, properties.crtc_id),
                Some(0)
            );
            assert_eq!(
                applied_cursor_binding(&fixture.scene.events.backend, 0),
                None
            );
            assert_eq!(
                sprite_users(&fixture.scene.events.backend, 0, binding_a.lease),
                Some(0)
            );
        }

        if blocking_error.is_some() {
            let detach = accept_cursor_detach(&mut fixture, &mut delivered, 0);
            retire_cursor_record(&mut fixture, &mut delivered, 0, &detach, 6);
        }
        super::support::finish_atomic_fixture(fixture, &mut delivered);
    };
    run(None);
    run(Some(libc::EIO));
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: apply a cursor-only EINVAL by raw CRTC to every card with that
// handle; card B must keep its independent Atomic cursor qualification.
fn cursor_failure_classification_isolated_across_cards_with_same_raw_crtc() {
    let Some(mut fixture) =
        AtomicPathFixture::with_identical_crtcs_on_two_devices(PathKind::Physical)
            .expect("identical-CRTC atomic cursor fixture")
    else {
        return;
    };
    let backend = &mut fixture.scene.events.backend;
    assert_eq!(backend.platform.outputs.len(), 2);
    let key_a = backend.platform.outputs[0].key.device_key;
    let key_b = backend.platform.outputs[1].key.device_key;
    let crtc_a = backend.platform.outputs[0].output.crtc;
    let crtc_b = backend.platform.outputs[1].output.crtc;
    assert_ne!(key_a, key_b);
    assert_eq!(crtc_a, crtc_b, "the two cards reuse the raw CRTC handle");

    let mut delivered = HashSet::new();
    let cursor_a = create_argb_cursor(backend, (4, 4), [0x11, 0x22, 0x33, 0xff], (1, 1));
    let hardware_a = establish_hardware_cursor(&mut fixture, &mut delivered, cursor_a, (120, 90));
    for output_idx in 0..2 {
        drain_unretired_output_events(&mut fixture, &mut delivered, output_idx);
    }

    let recorder_a = fixture.scene.events.recorders[&hardware_a.device_fd].clone();
    recorder_a.script_result(Err(std::io::Error::from_raw_os_error(libc::EINVAL)));
    Backend::warp_pointer_root(
        &mut fixture.scene.events.backend,
        &mut fixture.scene.events.state,
        121,
        91,
    );
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("process the rejected cursor move through Backend");
    let saw_attributed_einval = recorder_a.records().iter().any(|record| {
        record.kind == crate::drm::atomic::AtomicCommitKind::CursorOnly
            && matches!(
                record.result,
                crate::drm::atomic::tests_support::ScriptedResult::Error {
                    raw_os_error: Some(libc::EINVAL),
                    ..
                }
            )
    });
    let fallback_a = fixture
        .scene
        .events
        .backend
        .platform
        .for_tests_cursor_fallback_remaining(key_a, crtc_a);
    let fallback_b_after_a = fixture
        .scene
        .events
        .backend
        .platform
        .for_tests_cursor_fallback_remaining(key_b, crtc_b);

    super::support::finish_atomic_fixture(fixture, &mut delivered);

    assert!(
        saw_attributed_einval,
        "card A recorded the scripted cursor EINVAL"
    );
    assert_eq!(fallback_a, Some(1), "EINVAL changes card A's matching CRTC");
    assert_eq!(
        fallback_b_after_a, None,
        "card B's same-numbered CRTC has no fallback entry"
    );
}

fn held_direct_with_cursor(
    present_id: u64,
) -> Option<(
    AtomicPathFixture,
    HashSet<super::support::CompletionKey>,
    super::support::HardwareCursor,
    super::super::commit_identity::recording_scene::DirectPresentSetup,
)> {
    let mut fixture = AtomicPathFixture::new(PathKind::Physical)
        .expect("atomic direct cursor fixture construction")?;
    let cursor = create_argb_cursor(
        &mut fixture.scene.events.backend,
        (4, 4),
        [31, 32, 33, 255],
        (1, 2),
    );
    let mut delivered = HashSet::new();
    let hardware = establish_hardware_cursor(&mut fixture, &mut delivered, cursor, (100, 90));
    let binding_before_direct =
        applied_cursor_binding(&fixture.scene.events.backend, hardware.output_idx)
            .expect("A is applied before direct scanout");
    let direct = match enter_held_direct(&mut fixture, &mut delivered, present_id, None) {
        Ok(direct) => direct,
        Err(gate_values) => {
            let detach = accept_cursor_detach(&mut fixture, &mut delivered, hardware.output_idx);
            retire_cursor_record(
                &mut fixture,
                &mut delivered,
                hardware.output_idx,
                &detach,
                29,
            );
            super::support::finish_atomic_fixture(fixture, &mut delivered);
            panic!("{gate_values}");
        }
    };
    assert!(fixture.scene.events.backend.pstate.direct.root().active());
    let applied = applied_cursor_binding(&fixture.scene.events.backend, hardware.output_idx)
        .expect("A is shown on the held-direct path");
    assert_eq!(applied, binding_before_direct);
    assert_eq!(
        sprite_users(&fixture.scene.events.backend, 0, applied.lease),
        Some(1)
    );
    Some((fixture, delivered, hardware, direct))
}

fn accept_direct_cursor_replacement(
    fixture: &mut AtomicPathFixture,
    hardware: super::support::HardwareCursor,
    pixel: [u8; 4],
) -> crate::drm::atomic::tests_support::AtomicCommitRecord {
    let binding_a = applied_cursor_binding(&fixture.scene.events.backend, hardware.output_idx)
        .expect("cursor A binding before direct replacement");
    let properties = super::support::cursor_properties_for_output(
        &fixture.scene.events.backend,
        hardware.output_idx,
    );
    let cursor_b = create_argb_cursor(&mut fixture.scene.events.backend, (4, 4), pixel, (1, 2));
    define_root_cursor(&mut fixture.scene.events.backend, cursor_b);
    let record = fixture.scene.events.recorders[&hardware.device_fd]
        .records()
        .into_iter()
        .rev()
        .find(|record| {
            record.device_fd == hardware.device_fd
                && record.result == ScriptedResult::Ok
                && record.kind == AtomicCommitKind::CursorOnly
                && record
                    .events
                    .iter()
                    .any(|event| event.crtc == hardware.crtc)
                && record_property(record, properties.fb_id)
                    .is_some_and(|fb| fb != u64::from(binding_a.framebuffer))
        })
        .expect("held-direct cursor refresh accepts a CursorOnly replacement");
    let binding_b = applied_cursor_binding(&fixture.scene.events.backend, hardware.output_idx)
        .expect("cursor B applied on the held-direct path");
    assert_ne!(binding_a.lease, binding_b.lease);
    assert_eq!(
        sprite_users(&fixture.scene.events.backend, 0, binding_a.lease),
        Some(1)
    );
    assert_eq!(
        sprite_users(&fixture.scene.events.backend, 0, binding_b.lease),
        Some(1)
    );
    record
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: request an unflip for CursorShowError::Deferred; the direct-state assertion must fail.
fn direct_deferred_cursor_show_keeps_direct_and_retries_on_event() {
    let Some((mut fixture, mut delivered, hardware, _direct)) = held_direct_with_cursor(703) else {
        return;
    };
    let recorder = Rc::clone(&fixture.scene.events.recorders[&hardware.device_fd]);
    let properties = super::support::cursor_properties_for_output(&fixture.scene.events.backend, 0);

    Backend::warp_pointer_root(
        &mut fixture.scene.events.backend,
        &mut fixture.scene.events.state,
        140,
        120,
    );
    let move_start = record_count(&fixture, hardware.device_fd);
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("submit the direct cursor position commit");
    let position = accepted_live_records(&recorder)
        .into_iter()
        .skip(move_start)
        .find(|record| {
            record.kind == AtomicCommitKind::CursorOnly
                && record_property(record, properties.crtc_x).is_some()
                && record_property(record, properties.fb_id).is_none()
        })
        .expect("direct scanout accepts a position-only cursor commit");

    let cursor_b = create_argb_cursor(
        &mut fixture.scene.events.backend,
        (4, 4),
        [49, 50, 51, 255],
        (1, 2),
    );
    let cursor_b_id = cursor_b.as_raw();
    define_root_cursor(&mut fixture.scene.events.backend, cursor_b);
    let version_b = fixture.scene.events.backend.cursor_records[&cursor_b_id].version;
    assert_eq!(
        fixture.scene.events.backend.platform.devices[0]
            .cursor
            .plane
            .as_ref()
            .unwrap()
            .sprites()
            .for_tests_last_prepare_outcome(),
        Some(crate::kms::cursor_plane::sprites::SpriteStage::Ready),
        "the image is prepared before the in-flight show is deferred"
    );

    Backend::warp_pointer_root(
        &mut fixture.scene.events.backend,
        &mut fixture.scene.events.state,
        220,
        170,
    );
    let stayed_direct = fixture.scene.events.backend.pstate.direct.root().active()
        && !fixture
            .scene
            .events
            .backend
            .pstate
            .direct
            .root()
            .unflip_requested;

    let retry_start = record_count(&fixture, hardware.device_fd);
    retire_cursor_record(&mut fixture, &mut delivered, 0, &position, 11);
    let refreshed = accepted_live_records(&recorder)
        .into_iter()
        .skip(retry_start)
        .rev()
        .find(|record| {
            record.kind == AtomicCommitKind::CursorOnly
                && record_property(record, properties.fb_id).is_some_and(|fb| fb != 0)
                && record_property(record, properties.crtc_x).is_some()
        });
    let retried_latest = refreshed.as_ref().is_some_and(|record| {
        applied_cursor_binding(&fixture.scene.events.backend, 0)
            .is_some_and(|binding| binding.lease.version() == version_b)
            && record_property(record, properties.crtc_x) == Some(219)
            && record_property(record, properties.crtc_y) == Some(168)
    });
    if let Some(refreshed) = refreshed.as_ref() {
        retire_cursor_record(&mut fixture, &mut delivered, 0, refreshed, 12);
    }
    if fixture.scene.events.backend.pstate.direct.root().active() {
        fixture
            .scene
            .events
            .backend
            .stop_direct_after_scanout_replaced(
                crate::kms::render::backend::DirectScope::Root,
                "test teardown",
            );
    }

    let detach = accept_cursor_detach(&mut fixture, &mut delivered, 0);
    retire_cursor_record(&mut fixture, &mut delivered, 0, &detach, 13);
    super::support::finish_atomic_fixture(fixture, &mut delivered);
    assert!(
        stayed_direct,
        "a deferred cursor show must keep direct scanout active"
    );
    assert!(
        retried_latest,
        "the matching CRTC event retries the live image at the current pointer"
    );
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: omit the scheduled direct-show retry. EIO must recover without a new CRTC event.
fn direct_cursor_generic_error_keeps_direct_scanout() {
    let Some((mut fixture, mut delivered, hardware, _direct)) = held_direct_with_cursor(704) else {
        eprintln!(
            "SKIP direct_cursor_generic_error_keeps_direct_scanout: Vulkan fixture unavailable"
        );
        return;
    };
    let (elapsed_ns, base) = install_direct_retry_clock(&mut fixture.scene.events.backend);
    let recorder = Rc::clone(&fixture.scene.events.recorders[&hardware.device_fd]);
    let properties = super::support::cursor_properties_for_output(&fixture.scene.events.backend, 0);
    let record_start = record_count(&fixture, hardware.device_fd);
    let old_binding = applied_cursor_binding(&fixture.scene.events.backend, 0)
        .expect("existing direct cursor binding");
    recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EIO)));

    let cursor_b = create_argb_cursor(
        &mut fixture.scene.events.backend,
        (4, 4),
        [52, 53, 54, 255],
        (1, 2),
    );
    define_root_cursor(&mut fixture.scene.events.backend, cursor_b);
    let still_direct = fixture.scene.events.backend.pstate.direct.root().active()
        && !fixture
            .scene
            .events
            .backend
            .pstate
            .direct
            .root()
            .unflip_requested
        && recorder
            .records()
            .into_iter()
            .skip(record_start)
            .any(|record| {
                record.kind == AtomicCommitKind::CursorOnly
                    && record_property(&record, properties.fb_id).is_some_and(|fb| fb != 0)
                    && matches!(
                        record.result,
                        ScriptedResult::Error {
                            raw_os_error: Some(errno),
                            ..
                        } if errno == libc::EIO
                    )
            });
    assert!(
        still_direct,
        "the initial generic show error keeps direct scanout"
    );
    let retry_pending = fixture
        .scene
        .events
        .backend
        .pstate
        .direct
        .root()
        .direct_cursor_refresh_pending
        .contains(&0);
    let old_lease_retained =
        sprite_users(&fixture.scene.events.backend, 0, old_binding.lease) == Some(1);
    let retry_deadline = base + Duration::from_millis(50);
    assert_eq!(
        fixture
            .scene
            .events
            .backend
            .platform
            .direct_cursor_retry_deadline(0),
        Some(retry_deadline),
        "a generic direct-show error stores the scheduled retry deadline",
    );
    assert!(
        Backend::next_wakeup(&fixture.scene.events.backend)
            .is_some_and(|wakeup| wakeup <= retry_deadline),
        "a generic direct-show error makes its retry deadline wake the Backend",
    );
    for _ in 0..3 {
        Backend::maybe_composite(&mut fixture.scene.events.backend)
            .expect("an early direct tick must not retry before its deadline");
    }
    assert_eq!(
        record_count(&fixture, hardware.device_fd),
        record_start + 1,
        "no CRTC event or new frame is needed before the retry deadline",
    );
    advance_direct_retry_clock(&elapsed_ns, Duration::from_millis(50));
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("the scheduled direct-show retry runs through Backend");
    let retry = accepted_live_records(&recorder)
        .into_iter()
        .skip(record_start)
        .find(|record| {
            record.kind == AtomicCommitKind::CursorOnly
                && record_property(record, properties.fb_id).is_some_and(|fb| fb != 0)
                && record
                    .events
                    .iter()
                    .any(|event| event.crtc == hardware.crtc)
        })
        .expect("the retry accepts the current cursor image");
    retire_cursor_record(&mut fixture, &mut delivered, 0, &retry, 14);
    let pending_cleared = fixture
        .scene
        .events
        .backend
        .pstate
        .direct
        .root()
        .direct_cursor_refresh_pending
        .is_empty();
    let still_direct = fixture.scene.events.backend.pstate.direct.root().active()
        && !fixture
            .scene
            .events
            .backend
            .pstate
            .direct
            .root()
            .unflip_requested;
    finish_direct_fixture(fixture, &mut delivered);
    assert!(
        still_direct,
        "generic cursor errors preserve direct scanout"
    );
    assert!(retry_pending, "the existing CRTC retry remains armed");
    assert!(old_lease_retained, "the old cursor lease remains owned");
    assert!(
        pending_cleared,
        "the accepted retry clears the pending state"
    );
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: omit pending-refresh cleanup from direct teardown; the Backend-driven unflip assertion must fail.
fn direct_pending_refresh_cleared_on_unflip_or_output_removal() {
    {
        let Some((mut fixture, mut delivered, hardware, direct)) = held_direct_with_cursor(705)
        else {
            eprintln!(
                "SKIP direct_pending_refresh_cleared_on_unflip_or_output_removal: Vulkan fixture unavailable"
            );
            return;
        };
        let recorder = Rc::clone(&fixture.scene.events.recorders[&hardware.device_fd]);
        let replacement =
            accept_direct_cursor_replacement(&mut fixture, hardware, [55, 56, 57, 255]);
        let cursor_c = create_argb_cursor(
            &mut fixture.scene.events.backend,
            (4, 4),
            [58, 59, 60, 255],
            (1, 2),
        );
        define_root_cursor(&mut fixture.scene.events.backend, cursor_c);
        assert_eq!(
            fixture.scene.events.backend.platform.devices[0]
                .cursor
                .plane
                .as_ref()
                .unwrap()
                .sprites()
                .for_tests_last_prepare_outcome(),
            Some(crate::kms::cursor_plane::sprites::SpriteStage::Deferred),
            "the second live image causes a real deferred refresh"
        );
        assert!(
            fixture
                .scene
                .events
                .backend
                .pstate
                .direct
                .root()
                .direct_cursor_refresh_pending
                .contains(&0)
        );

        // An ineligible Present uses the normal Backend path to request an
        // unflip while the cursor commit still owns its CRTC.
        let mut successor = direct.candidate;
        successor.present_id += 1;
        successor.valid_region_xid = 0x55;
        let mut successor_event = direct.event.clone();
        successor_event.present_id = successor.present_id;
        successor_event.serial = successor_event.serial.wrapping_add(1);
        assert!(
            !Backend::try_present_direct(
                &mut fixture.scene.events.backend,
                successor,
                successor_event,
            )
            .expect("ineligible successor asks Backend to unflip")
        );
        assert!(
            fixture
                .scene
                .events
                .backend
                .pstate
                .direct
                .root()
                .unflip_requested
        );
        retire_cursor_record(&mut fixture, &mut delivered, 0, &replacement, 30);
        assert!(
            fixture
                .scene
                .events
                .backend
                .pstate
                .direct
                .root()
                .unflip_requested
        );
        assert!(
            fixture
                .scene
                .events
                .backend
                .pstate
                .direct
                .root()
                .direct_cursor_refresh_pending
                .contains(&0)
        );

        let unflip_start = record_count(&fixture, hardware.device_fd);
        Backend::maybe_composite(&mut fixture.scene.events.backend)
            .expect("submit the composed unflip through Backend");
        assert!(
            accepted_live_records(&recorder)
                .into_iter()
                .skip(unflip_start)
                .any(|record| record.kind != AtomicCommitKind::CursorOnly),
            "the Backend path accepts the composed unflip"
        );
        retire_pending_output(&mut fixture.scene.events, &mut delivered, 0, 31);
        assert!(!fixture.scene.events.backend.pstate.direct.root().active());
        assert!(
            fixture
                .scene
                .events
                .backend
                .pstate
                .direct
                .root()
                .direct_cursor_refresh_pending
                .is_empty(),
            "retiring the Backend unflip clears pending cursor retries"
        );
        assert!(
            !fixture
                .scene
                .events
                .backend
                .platform
                .any_cursor_commit_in_flight()
        );

        let detach = accept_cursor_detach(&mut fixture, &mut delivered, 0);
        retire_cursor_record(&mut fixture, &mut delivered, 0, &detach, 32);
        super::support::finish_atomic_fixture(fixture, &mut delivered);
    }

    {
        let Some((mut fixture, mut delivered, hardware, _direct)) = held_direct_with_cursor(706)
        else {
            eprintln!(
                "SKIP direct_pending_refresh_cleared_on_unflip_or_output_removal: Vulkan fixture unavailable"
            );
            return;
        };
        let replacement =
            accept_direct_cursor_replacement(&mut fixture, hardware, [61, 62, 63, 255]);
        let cursor_c = create_argb_cursor(
            &mut fixture.scene.events.backend,
            (4, 4),
            [64, 65, 66, 255],
            (1, 2),
        );
        define_root_cursor(&mut fixture.scene.events.backend, cursor_c);
        assert_eq!(
            fixture.scene.events.backend.platform.devices[0]
                .cursor
                .plane
                .as_ref()
                .unwrap()
                .sprites()
                .for_tests_last_prepare_outcome(),
            Some(crate::kms::cursor_plane::sprites::SpriteStage::Deferred),
            "the output-removal case starts from a real deferred refresh"
        );
        assert!(
            fixture
                .scene
                .events
                .backend
                .pstate
                .direct
                .root()
                .direct_cursor_refresh_pending
                .contains(&0)
        );
        fixture.scene.events.queue_commit_completion(
            hardware.device_fd,
            hardware.crtc,
            accepted_event_id(&replacement).expect("replacement has an accepted event ID"),
            33,
            Duration::from_millis(33),
        );

        let mut snapshots: Vec<_> = fixture
            .scene
            .events
            .backend
            .platform
            .outputs
            .iter()
            .map(|output| ConnectorSnapshot {
                key: output.key.clone(),
                modes: vec![output.output.picked.clone()],
                mm_width: output.output.mm_width,
                mm_height: output.output.mm_height,
                edid: output.output.edid.clone(),
                connector_type: output.output.connector_type.clone(),
                vrr_capable: output.output.connector_vrr_capable,
            })
            .collect();
        snapshots[0].modes.clear();
        super::lifecycle::install_logical_topology(&mut fixture.scene.events.backend, snapshots);
        fixture
            .scene
            .events
            .backend
            .platform
            .for_tests_accept_output_power_commits();
        fixture.scene.events.backend.pstate.hotplug_rescan_deadline =
            Some(Instant::now() - Duration::from_millis(1));
        Backend::poll_deferred_input(
            &mut fixture.scene.events.backend,
            &mut fixture.scene.events.state,
        );

        assert!(fixture.scene.events.backend.platform.outputs.is_empty());
        assert!(!fixture.scene.events.backend.pstate.direct.root().active());
        assert!(
            fixture
                .scene
                .events
                .backend
                .pstate
                .direct
                .root()
                .direct_cursor_refresh_pending
                .is_empty(),
            "the Backend hotplug removal clears pending cursor retries before output compaction"
        );
        assert!(
            !fixture
                .scene
                .events
                .backend
                .platform
                .any_cursor_commit_in_flight()
        );
        super::support::finish_atomic_fixture(fixture, &mut delivered);
    }
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: skip the CRTC-event retry or overwrite a live sprite slot under pool pressure.
fn p2_direct_refresh_reaches_pool_pressure_and_keeps_only_the_newest_wanted_image() {
    let Some((mut fixture, mut delivered, hardware, _direct)) = held_direct_with_cursor(701) else {
        return;
    };
    let recorder = Rc::clone(&fixture.scene.events.recorders[&hardware.device_fd]);
    let properties = super::support::cursor_properties_for_output(&fixture.scene.events.backend, 0);
    let binding_a = applied_cursor_binding(&fixture.scene.events.backend, 0).unwrap();
    let replacement = accept_direct_cursor_replacement(&mut fixture, hardware, [34, 35, 36, 255]);
    let binding_b = applied_cursor_binding(&fixture.scene.events.backend, 0).unwrap();
    let bytes_a = sprite_bytes(&fixture.scene.events.backend, 0, binding_a.lease).unwrap();
    let bytes_b = sprite_bytes(&fixture.scene.events.backend, 0, binding_b.lease).unwrap();
    assert_eq!(
        fixture.scene.events.backend.platform.devices[0]
            .cursor
            .plane
            .as_ref()
            .unwrap()
            .sprites()
            .for_tests_max_buffer_count(),
        2
    );
    assert!(
        fixture
            .scene
            .events
            .backend
            .platform
            .cursor_commit_in_flight_for_output(0)
    );
    assert_eq!(
        sprite_users(&fixture.scene.events.backend, 0, binding_a.lease),
        Some(1)
    );
    assert_eq!(
        sprite_users(&fixture.scene.events.backend, 0, binding_b.lease),
        Some(1)
    );

    let cursor_c = create_argb_cursor(
        &mut fixture.scene.events.backend,
        (4, 4),
        [37, 38, 39, 255],
        (1, 2),
    );
    let cursor_c_id = cursor_c.as_raw();
    define_root_cursor(&mut fixture.scene.events.backend, cursor_c);
    let pool = fixture.scene.events.backend.platform.devices[0]
        .cursor
        .plane
        .as_ref()
        .unwrap()
        .sprites();
    assert_eq!(
        pool.for_tests_last_prepare_outcome(),
        Some(crate::kms::cursor_plane::sprites::SpriteStage::Deferred)
    );
    assert_eq!(
        pool.for_tests_wanted_identity(),
        Some((
            fixture.scene.events.backend.cursor_records[&cursor_c_id].version,
            1,
            2
        ))
    );
    assert!(
        !fixture
            .scene
            .events
            .backend
            .pstate
            .direct
            .root()
            .unflip_requested
    );
    assert!(
        fixture
            .scene
            .events
            .backend
            .pstate
            .direct
            .root()
            .direct_cursor_refresh_pending
            .contains(&0)
    );
    assert_eq!(
        sprite_users(&fixture.scene.events.backend, 0, binding_a.lease),
        Some(1)
    );
    assert_eq!(
        sprite_users(&fixture.scene.events.backend, 0, binding_b.lease),
        Some(1)
    );
    assert_eq!(
        sprite_bytes(&fixture.scene.events.backend, 0, binding_a.lease),
        Some(bytes_a.clone())
    );
    assert_eq!(
        sprite_bytes(&fixture.scene.events.backend, 0, binding_b.lease),
        Some(bytes_b.clone())
    );

    let cursor_d = create_argb_cursor(
        &mut fixture.scene.events.backend,
        (4, 4),
        [40, 41, 42, 255],
        (1, 2),
    );
    let cursor_d_id = cursor_d.as_raw();
    define_root_cursor(&mut fixture.scene.events.backend, cursor_d);
    let pool = fixture.scene.events.backend.platform.devices[0]
        .cursor
        .plane
        .as_ref()
        .unwrap()
        .sprites();
    assert_eq!(
        pool.for_tests_last_prepare_outcome(),
        Some(crate::kms::cursor_plane::sprites::SpriteStage::Deferred)
    );
    assert_eq!(
        pool.for_tests_wanted_identity(),
        Some((
            fixture.scene.events.backend.cursor_records[&cursor_d_id].version,
            1,
            2
        ))
    );
    assert!(fixture.scene.events.backend.pstate.direct.root().active());
    assert!(
        fixture
            .scene
            .events
            .backend
            .platform
            .cursor_commit_in_flight_for_output(0)
    );
    assert_eq!(
        sprite_users(&fixture.scene.events.backend, 0, binding_a.lease),
        Some(1)
    );
    assert_eq!(
        sprite_users(&fixture.scene.events.backend, 0, binding_b.lease),
        Some(1)
    );
    assert_eq!(
        sprite_bytes(&fixture.scene.events.backend, 0, binding_a.lease),
        Some(bytes_a)
    );
    assert_eq!(
        sprite_bytes(&fixture.scene.events.backend, 0, binding_b.lease),
        Some(bytes_b)
    );

    let retry_start = record_count(&fixture, hardware.device_fd);
    retire_cursor_record(&mut fixture, &mut delivered, 0, &replacement, 9);
    assert!(
        sprite_users(&fixture.scene.events.backend, 0, binding_a.lease)
            .is_none_or(|users| users == 0)
    );
    let refreshed = accepted_live_records(&recorder)
        .into_iter()
        .skip(retry_start)
        .find(|record| {
            record.kind == AtomicCommitKind::CursorOnly
                && record_property(record, properties.fb_id).is_some_and(|fb| fb != 0)
        })
        .expect("the matching CRTC event retries the newest deferred image");
    assert_eq!(
        applied_cursor_binding(&fixture.scene.events.backend, 0)
            .expect("deferred image is applied after the old commit retires")
            .lease
            .version(),
        fixture.scene.events.backend.cursor_records[&cursor_d_id].version
    );
    assert!(fixture.scene.events.backend.pstate.direct.root().active());
    assert!(
        !fixture
            .scene
            .events
            .backend
            .pstate
            .direct
            .root()
            .unflip_requested
    );
    assert!(
        fixture
            .scene
            .events
            .backend
            .pstate
            .direct
            .root()
            .direct_cursor_refresh_pending
            .is_empty()
    );
    retire_cursor_record(&mut fixture, &mut delivered, 0, &refreshed, 10);
    fixture
        .scene
        .events
        .backend
        .stop_direct_after_scanout_replaced(
            crate::kms::render::backend::DirectScope::Root,
            "test teardown",
        );
    let detach = accept_cursor_detach(&mut fixture, &mut delivered, 0);
    retire_cursor_record(&mut fixture, &mut delivered, 0, &detach, 11);
    super::support::finish_atomic_fixture(fixture, &mut delivered);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: keep Deferred as a direct-refresh failure; the state and retry assertions must fail.
fn p2_direct_hook_keeps_scanout_until_deferred_refresh_retries() {
    let Some((mut fixture, mut delivered, hardware, _direct)) = held_direct_with_cursor(702) else {
        return;
    };
    let recorder = Rc::clone(&fixture.scene.events.recorders[&hardware.device_fd]);
    let properties = super::support::cursor_properties_for_output(&fixture.scene.events.backend, 0);
    let binding_a = applied_cursor_binding(&fixture.scene.events.backend, 0).unwrap();
    let replacement = accept_direct_cursor_replacement(&mut fixture, hardware, [43, 44, 45, 255]);
    let binding_b = applied_cursor_binding(&fixture.scene.events.backend, 0).unwrap();
    let bytes_a = sprite_bytes(&fixture.scene.events.backend, 0, binding_a.lease).unwrap();
    let bytes_b = sprite_bytes(&fixture.scene.events.backend, 0, binding_b.lease).unwrap();
    let cursor_c = create_argb_cursor(
        &mut fixture.scene.events.backend,
        (4, 4),
        [46, 47, 48, 255],
        (1, 2),
    );
    let cursor_c_id = cursor_c.as_raw();
    define_root_cursor(&mut fixture.scene.events.backend, cursor_c);
    assert_eq!(
        fixture.scene.events.backend.platform.devices[0]
            .cursor
            .plane
            .as_ref()
            .unwrap()
            .sprites()
            .for_tests_last_prepare_outcome(),
        Some(crate::kms::cursor_plane::sprites::SpriteStage::Deferred)
    );
    assert!(
        !fixture
            .scene
            .events
            .backend
            .pstate
            .direct
            .root()
            .unflip_requested
    );
    assert!(
        fixture
            .scene
            .events
            .backend
            .pstate
            .direct
            .root()
            .direct_cursor_refresh_pending
            .contains(&0)
    );
    assert!(
        fixture
            .scene
            .events
            .backend
            .platform
            .cursor_commit_in_flight_for_output(0)
    );
    assert_eq!(
        sprite_users(&fixture.scene.events.backend, 0, binding_a.lease),
        Some(1)
    );
    assert_eq!(
        sprite_users(&fixture.scene.events.backend, 0, binding_b.lease),
        Some(1)
    );
    assert_eq!(
        sprite_bytes(&fixture.scene.events.backend, 0, binding_a.lease),
        Some(bytes_a)
    );
    assert_eq!(
        sprite_bytes(&fixture.scene.events.backend, 0, binding_b.lease),
        Some(bytes_b)
    );

    let retry_start = record_count(&fixture, hardware.device_fd);
    retire_cursor_record(&mut fixture, &mut delivered, 0, &replacement, 9);
    let refreshed = accepted_live_records(&recorder)
        .into_iter()
        .skip(retry_start)
        .find(|record| {
            record.kind == AtomicCommitKind::CursorOnly
                && record_property(record, properties.fb_id).is_some_and(|fb| fb != 0)
        })
        .expect("the matching CRTC event retries the deferred cursor image");
    assert_eq!(
        applied_cursor_binding(&fixture.scene.events.backend, 0)
            .expect("cursor refresh is applied")
            .lease
            .version(),
        fixture.scene.events.backend.cursor_records[&cursor_c_id].version
    );
    assert!(fixture.scene.events.backend.pstate.direct.root().active());
    assert!(
        !fixture
            .scene
            .events
            .backend
            .pstate
            .direct
            .root()
            .unflip_requested
    );
    retire_cursor_record(&mut fixture, &mut delivered, 0, &refreshed, 10);
    fixture
        .scene
        .events
        .backend
        .stop_direct_after_scanout_replaced(
            crate::kms::render::backend::DirectScope::Root,
            "test teardown",
        );
    let detach = accept_cursor_detach(&mut fixture, &mut delivered, 0);
    retire_cursor_record(&mut fixture, &mut delivered, 0, &detach, 11);
    super::support::finish_atomic_fixture(fixture, &mut delivered);
}

fn held_direct_with_two_outputs(
    present_id: u64,
) -> Option<(
    AtomicPathFixture,
    HashSet<super::support::CompletionKey>,
    super::support::HardwareCursor,
    super::super::commit_identity::recording_scene::DirectPresentSetup,
)> {
    let mut fixture = AtomicPathFixture::with_output_count(PathKind::Physical, 2)
        .expect("two-output atomic direct cursor fixture construction")?;
    let cursor = create_argb_cursor(
        &mut fixture.scene.events.backend,
        (4, 4),
        [71, 72, 73, 255],
        (1, 2),
    );
    let mut delivered = HashSet::new();
    let hardware = establish_hardware_cursor(&mut fixture, &mut delivered, cursor, (100, 90));
    drain_unretired_output_events(&mut fixture, &mut delivered, 1);
    let direct = enter_held_direct(&mut fixture, &mut delivered, present_id, None)
        .expect("enter two-output direct scanout through Backend");
    for output_idx in 0..2 {
        drain_unretired_output_events(&mut fixture, &mut delivered, output_idx);
    }
    assert!(fixture.scene.events.backend.pstate.direct.root().active());
    assert!(
        (0..2).all(|output_idx| fixture
            .scene
            .events
            .backend
            .platform
            .cursor_plane_visible_for_output(output_idx)),
        "the direct cursor is bound on both outputs"
    );
    Some((fixture, delivered, hardware, direct))
}

fn held_output_direct_with_two_outputs(
    present_id: u64,
    output_idx: usize,
    cursor_output_idx: usize,
    pointer: (i32, i32),
    options: u32,
) -> Option<(
    AtomicPathFixture,
    HashSet<super::support::CompletionKey>,
    super::support::HardwareCursor,
    super::super::commit_identity::recording_scene::DirectPresentSetup,
)> {
    let mut fixture = AtomicPathFixture::with_output_count(PathKind::Physical, 2)
        .expect("two-output atomic output-direct cursor fixture construction")?;
    let cursor = create_argb_cursor(
        &mut fixture.scene.events.backend,
        (4, 4),
        [71, 72, 73, 255],
        (1, 2),
    );
    let mut delivered = HashSet::new();
    let hardware = establish_hardware_cursor_on_output(
        &mut fixture,
        &mut delivered,
        cursor,
        cursor_output_idx,
        pointer,
    );
    for idx in 0..2 {
        drain_unretired_output_events(&mut fixture, &mut delivered, idx);
    }

    let direct = fixture
        .scene
        .install_direct_output_present(output_idx, present_id, options);
    Backend::set_cursor_hidden(&mut fixture.scene.events.backend, false);
    for frame in 30..34 {
        Backend::maybe_composite(&mut fixture.scene.events.backend)
            .expect("settle cursor transition before output direct Present");
        for idx in 0..2 {
            drain_unretired_output_events(&mut fixture, &mut delivered, idx);
        }
        let mode = fixture
            .scene
            .events
            .backend
            .scene
            .cursor_mode_for_outputs(&[output_idx]);
        if matches!(
            mode,
            crate::kms::render::scene::CursorPlaneMode::Hw
                | crate::kms::render::scene::CursorPlaneMode::Hidden
        ) {
            break;
        }
        let _ = frame;
    }
    assert!(matches!(
        fixture
            .scene
            .events
            .backend
            .scene
            .cursor_mode_for_outputs(&[output_idx]),
        crate::kms::render::scene::CursorPlaneMode::Hw
            | crate::kms::render::scene::CursorPlaneMode::Hidden
    ));

    fixture
        .scene
        .events
        .backend
        .for_tests_set_scanout_m1_prober(super::super::present_scanout::record_scanout_m1_probe);
    if options & yserver_core::present_scheduler::PRESENT_OPTION_ASYNC != 0 {
        for device in &mut fixture.scene.events.backend.platform.devices {
            device.async_page_flip = true;
        }
    }
    Backend::note_present_scanout_candidate(&mut fixture.scene.events.backend, direct.candidate);
    let device_fd = fixture.scene.kms_fd(output_idx);
    let start = record_count(&fixture, device_fd);
    for attempt in 0..usize::from(crate::kms::render::backend::SCANOUT_M2_ELIGIBLE_ROOT_PROBATION) {
        let accepted = Backend::try_present_direct(
            &mut fixture.scene.events.backend,
            direct.candidate,
            direct.event.clone(),
        )
        .expect("output-sized Present reaches direct scanout through Backend");
        assert_eq!(
            accepted,
            attempt + 1
                == usize::from(crate::kms::render::backend::SCANOUT_M2_ELIGIBLE_ROOT_PROBATION),
            "Output({output_idx}) enters after the normal direct probation"
        );
    }
    let crtc = fixture.scene.events.backend.platform.outputs[output_idx]
        .output
        .crtc;
    let direct_record = accepted_live_records(&fixture.scene.events.recorders[&device_fd])
        .into_iter()
        .skip(start)
        .find(|record| {
            record.kind != AtomicCommitKind::CursorOnly
                && record.events.len() == 1
                && record.events[0].crtc == crtc
                && accepted_event_id(record).is_some()
        })
        .expect("the selected output accepted its direct Present");
    retire_pending_output(&mut fixture.scene.events, &mut delivered, output_idx, 39);
    assert!(
        fixture.scene.events.backend.pstate.direct.outputs[output_idx]
            .current
            .is_some(),
        "the Present now owns Output({output_idx})"
    );
    let _ = direct_record;
    Some((fixture, delivered, hardware, direct))
}

fn count_cursor_commits_for_output(fixture: &AtomicPathFixture, output_idx: usize) -> usize {
    let device_fd = fixture.scene.kms_fd(output_idx);
    let crtc = fixture.scene.events.backend.platform.outputs[output_idx]
        .output
        .crtc;
    accepted_live_records(&fixture.scene.events.recorders[&device_fd])
        .into_iter()
        .filter(|record| {
            record.kind == AtomicCommitKind::CursorOnly
                && record.events.iter().any(|event| event.crtc == crtc)
        })
        .count()
}

fn finish_direct_fixture(
    mut fixture: AtomicPathFixture,
    delivered: &mut HashSet<super::support::CompletionKey>,
) {
    fixture
        .scene
        .events
        .backend
        .for_tests_set_monotonic_clock(Rc::new(Instant::now));
    for scope in fixture.scene.events.backend.pstate.direct.active_scopes() {
        fixture
            .scene
            .events
            .backend
            .stop_direct_after_scanout_replaced(scope, "test teardown");
    }
    for output_idx in 0..fixture.scene.events.backend.platform.outputs.len() {
        drain_unretired_output_events(&mut fixture, delivered, output_idx);
    }
    for output_idx in 0..fixture.scene.events.backend.platform.outputs.len() {
        if fixture
            .scene
            .events
            .backend
            .platform
            .cursor_plane_visible_for_output(output_idx)
        {
            fixture
                .scene
                .events
                .backend
                .platform
                .cursor_plane_hide_on_crtc(output_idx)
                .expect("detach test cursor during teardown");
        }
    }
    for output_idx in 0..fixture.scene.events.backend.platform.outputs.len() {
        drain_unretired_output_events(&mut fixture, delivered, output_idx);
    }
    super::support::finish_atomic_fixture(fixture, delivered);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: queue the replacement for every HW output; require all-output
// acknowledgement quiescence for external sync; return early while any direct
// owner exists instead of queueing the composed output's replacement.
fn cursor_image_change_with_pending_ack_on_other_output_keeps_owner() {
    let Some((mut fixture, mut delivered, hardware, direct)) =
        held_output_direct_with_two_outputs(715, 1, 1, (799, 90), 0)
    else {
        eprintln!(
            "SKIP cursor_image_change_with_pending_ack_on_other_output_keeps_owner: Vulkan fixture unavailable"
        );
        return;
    };
    assert!(
        fixture
            .scene
            .events
            .backend
            .platform
            .cursor_plane_visible_for_output(0)
    );
    assert!(
        fixture
            .scene
            .events
            .backend
            .platform
            .cursor_plane_visible_for_output(1)
    );

    damage_root(&mut fixture);
    let before_pending_scene = record_count(&fixture, hardware.device_fd);
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("compose output 0 while Output(1) remains direct");
    let output0 = fixture.scene.events.backend.platform.outputs[0].output.crtc;
    let pending_scene = accepted_live_records(&fixture.scene.events.recorders[&hardware.device_fd])
        .into_iter()
        .skip(before_pending_scene)
        .find(|record| {
            matches!(
                record.kind,
                AtomicCommitKind::Scene | AtomicCommitKind::SceneWithCursor
            ) && record.events.iter().any(|event| event.crtc == output0)
        })
        .expect("output 0 has a scene acknowledgement in flight");
    assert!(fixture.scene.events.backend.scene.has_pending_page_flip(0));
    assert!(
        fixture.scene.events.backend.pstate.direct.outputs[1]
            .current
            .is_some()
    );

    let cursor_b = create_argb_cursor(
        &mut fixture.scene.events.backend,
        (4, 4),
        [0x62, 0x71, 0x82, 0xff],
        (1, 2),
    );
    define_root_cursor(&mut fixture.scene.events.backend, cursor_b);
    let output1_props =
        super::support::cursor_properties_for_output(&fixture.scene.events.backend, 1);
    let output1_new_image =
        accepted_live_records(&fixture.scene.events.recorders[&hardware.device_fd])
            .into_iter()
            .rev()
            .find(|record| {
                record.kind == AtomicCommitKind::CursorOnly
                    && record_property(record, output1_props.fb_id).is_some_and(|fb| fb != 0)
                    && record.events.iter().any(|event| {
                        event.crtc == fixture.scene.events.backend.platform.outputs[1].output.crtc
                    })
            })
            .expect("Output(1) cursor plane accepts the new image immediately");
    assert!(
        fixture.scene.events.backend.pstate.direct.outputs[1]
            .current
            .is_some()
            && !fixture.scene.events.backend.pstate.direct.outputs[1].unflip_requested,
        "the pending acknowledgement on output 0 cannot revoke Output(1)"
    );
    assert_eq!(
        fixture
            .scene
            .events
            .backend
            .scene
            .cursor_mode_for_outputs(&[1]),
        crate::kms::render::scene::CursorPlaneMode::Hw,
        "refreshing Output(1) leaves its cursor mode out of Mixed"
    );

    retire_cursor_record(&mut fixture, &mut delivered, 1, &output1_new_image, 44);
    retire_cursor_record(&mut fixture, &mut delivered, 0, &pending_scene, 45);
    for frame in 46..50 {
        Backend::maybe_composite(&mut fixture.scene.events.backend)
            .expect("finish queued output 0 cursor image upload");
        if !has_unretired_accepted_event(&fixture.scene.events, &delivered, 0) {
            break;
        }
        retire_pending_output(&mut fixture.scene.events, &mut delivered, 0, frame);
    }

    let output0_binding = applied_cursor_binding(&fixture.scene.events.backend, 0)
        .expect("output 0 keeps its cursor binding");
    let output0_bytes = sprite_bytes(&fixture.scene.events.backend, 0, output0_binding.lease)
        .expect("output 0 replacement sprite bytes");
    assert_eq!(
        output0_bytes
            .chunks_exact(4)
            .filter(|pixel| *pixel == [0x62, 0x71, 0x82, 0xff])
            .count(),
        16,
        "output 0 receives the replacement's sixteen pixels after its scene frame retires"
    );
    assert_eq!(
        fixture.scene.events.backend.pstate.direct.active_scopes(),
        [crate::kms::render::backend::DirectScope::Output(1)],
        "Output(1) stays direct through output 0's cursor upload"
    );
    assert!(
        fixture.scene.events.backend.pstate.direct.outputs[1]
            .current
            .is_some()
    );
    assert_eq!(
        fixture
            .scene
            .events
            .backend
            .scene
            .cursor_mode_for_outputs(&[1]),
        crate::kms::render::scene::CursorPlaneMode::Hw,
        "Output(1) cursor mode stays hardware after output 0's cursor upload"
    );
    let mut next_candidate = direct.candidate;
    next_candidate.present_id += 1;
    let mut next_event = direct.event.clone();
    next_event.present_id = next_candidate.present_id;
    next_event.serial = u32::try_from(next_candidate.present_id).expect("next serial fits");
    Backend::note_present_scanout_candidate(&mut fixture.scene.events.backend, next_candidate);
    let next_start = record_count(&fixture, hardware.device_fd);
    assert!(
        Backend::try_present_direct(
            &mut fixture.scene.events.backend,
            next_candidate,
            next_event,
        )
        .expect("Output(1) accepts its next direct Present after the cursor upload")
    );
    let output1_next = accepted_live_records(&fixture.scene.events.recorders[&hardware.device_fd])
        .into_iter()
        .skip(next_start)
        .find(|record| {
            record.events.iter().any(|event| {
                event.crtc == fixture.scene.events.backend.platform.outputs[1].output.crtc
            }) && accepted_event_id(record).is_some()
        })
        .expect("Output(1)'s next direct Present has an accepted CRTC commit");
    assert!(
        matches!(
            output1_next.kind,
            AtomicCommitKind::Scene | AtomicCommitKind::SceneWithCursor
        ),
        "Output(1)'s successor is still a synchronized direct commit"
    );
    retire_pending_output(&mut fixture.scene.events, &mut delivered, 1, 50);
    assert_eq!(
        fixture
            .scene
            .events
            .backend
            .scene
            .cursor_mode_for_outputs(&[1]),
        crate::kms::render::scene::CursorPlaneMode::Hw,
        "Output(1) cursor mode stays hardware through its next retirement"
    );
    assert!(
        fixture.scene.events.backend.pstate.direct.outputs[1]
            .current
            .is_some()
            && !fixture.scene.events.backend.pstate.direct.outputs[1].unflip_requested,
        "Output(1) remains current after its next Present retires"
    );
    finish_direct_fixture(fixture, &mut delivered);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: make a Root-only tearing scope or let the second Output take tearing.
fn output_owner_async_presents_flip_synchronized() {
    let Some((mut fixture, mut delivered, hardware, output1_direct)) =
        held_output_direct_with_two_outputs(
            721,
            1,
            0,
            (100, 90),
            yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
        )
    else {
        eprintln!("SKIP output_owner_async_presents_flip_synchronized: Vulkan fixture unavailable");
        return;
    };
    let crtc1 = fixture.scene.events.backend.platform.outputs[1].output.crtc;
    let crtc0 = fixture.scene.events.backend.platform.outputs[0].output.crtc;
    let mut async_candidate = output1_direct.candidate;
    async_candidate.present_id += 1;
    let mut async_event = output1_direct.event.clone();
    async_event.present_id = async_candidate.present_id;
    async_event.serial = u32::try_from(async_candidate.present_id).expect("Present serial fits");
    Backend::note_present_scanout_candidate(&mut fixture.scene.events.backend, async_candidate);
    let async_start = record_count(&fixture, hardware.device_fd);
    assert!(
        Backend::try_present_direct(
            &mut fixture.scene.events.backend,
            async_candidate,
            async_event,
        )
        .expect("the Output(1) tearing owner accepts its async successor")
    );
    let async_record = accepted_live_records(&fixture.scene.events.recorders[&hardware.device_fd])
        .into_iter()
        .skip(async_start)
        .find(|record| {
            record.events.iter().any(|event| event.crtc == crtc1)
                && accepted_event_id(record).is_some()
        })
        .expect("Output(1) async successor has an accepted commit");
    assert_eq!(async_record.kind, AtomicCommitKind::Async);
    retire_pending_output(&mut fixture.scene.events, &mut delivered, 1, 40);
    let output1 = &fixture.scene.events.backend.pstate.direct.outputs[1];
    assert!(output1.current.is_some(), "Output(1) owns its direct frame");
    assert_eq!(
        output1
            .current
            .as_ref()
            .expect("Output(1) current frame")
            .async_outcome,
        crate::drm::async_flip::AsyncOutcome::Accepted,
        "Output(1)'s first post-probation Present tears"
    );
    assert!(fixture.scene.events.backend.pstate.tearing.is_on());
    assert_eq!(
        fixture.scene.events.backend.tearing_scope_outputs(),
        [1],
        "Output(1) is the single tearing owner"
    );
    assert!(
        fixture
            .scene
            .events
            .backend
            .platform
            .cursor_plane_visible_for_output(0),
        "the composed output keeps its hardware cursor"
    );
    assert!(
        !fixture
            .scene
            .events
            .backend
            .platform
            .cursor_plane_visible_for_output(1)
    );
    assert!(
        accepted_live_records(&fixture.scene.events.recorders[&hardware.device_fd])
            .iter()
            .filter(|record| record.events.iter().any(|event| event.crtc == crtc1))
            .any(|record| record
                .flags
                .contains(::drm::control::AtomicCommitFlags::PAGE_FLIP_ASYNC)),
        "Output(1) records an async direct commit"
    );

    damage_root(&mut fixture);
    let before_compose = record_count(&fixture, hardware.device_fd);
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("compose output 0 while Output(1) remains direct");
    let output0_scene = accepted_live_records(&fixture.scene.events.recorders[&hardware.device_fd])
        .into_iter()
        .skip(before_compose)
        .find(|record| {
            matches!(
                record.kind,
                AtomicCommitKind::Scene | AtomicCommitKind::SceneWithCursor
            ) && record.events.iter().any(|event| event.crtc == crtc0)
        })
        .expect("output 0 composes while output 1 remains direct");
    assert!(
        !output0_scene
            .flags
            .contains(::drm::control::AtomicCommitFlags::PAGE_FLIP_ASYNC)
    );
    if has_unretired_accepted_event(&fixture.scene.events, &delivered, 0) {
        retire_pending_output(&mut fixture.scene.events, &mut delivered, 0, 42);
    }
    Backend::set_cursor_hidden(&mut fixture.scene.events.backend, true);
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("detach cursors before the second direct owner enters");
    for output_idx in 0..2 {
        drain_unretired_output_events(&mut fixture, &mut delivered, output_idx);
    }

    let mut output0_direct = fixture.scene.install_direct_output_present(
        0,
        output1_direct.candidate.present_id + 100,
        yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
    );
    output0_direct.candidate.client_id = 2;
    output0_direct.event.client_id = yserver_protocol::x11::ClientId(2);
    drain_unretired_output_events(&mut fixture, &mut delivered, 1);
    Backend::note_present_scanout_candidate(
        &mut fixture.scene.events.backend,
        output0_direct.candidate,
    );
    let before_output0 = record_count(&fixture, hardware.device_fd);
    for attempt in 0..usize::from(crate::kms::render::backend::SCANOUT_M2_ELIGIBLE_ROOT_PROBATION) {
        assert_eq!(
            Backend::try_present_direct(
                &mut fixture.scene.events.backend,
                output0_direct.candidate,
                output0_direct.event.clone(),
            )
            .expect("another client's Output(0) async Present"),
            attempt + 1
                == usize::from(crate::kms::render::backend::SCANOUT_M2_ELIGIBLE_ROOT_PROBATION),
            "Output(0) enters after the direct scanout probation: output_state={:?}, pending_scene={}, owner_tearing={:?}",
            fixture.scene.events.backend.pstate.direct.outputs[0].eligible_streak,
            fixture.scene.events.backend.scene.has_pending_page_flip(0),
            fixture.scene.events.backend.pstate.tearing.state,
        );
    }
    let output0_record =
        accepted_live_records(&fixture.scene.events.recorders[&hardware.device_fd])
            .into_iter()
            .skip(before_output0)
            .find(|record| {
                record.events.iter().any(|event| event.crtc == crtc0)
                    && accepted_event_id(record).is_some()
            })
            .expect("Output(0) direct commit was recorded");
    assert!(
        !output0_record
            .flags
            .contains(::drm::control::AtomicCommitFlags::PAGE_FLIP_ASYNC)
    );
    retire_pending_output(&mut fixture.scene.events, &mut delivered, 0, 43);

    assert!(fixture.scene.events.backend.pstate.tearing.is_on());
    assert!(
        fixture.scene.events.backend.pstate.direct.outputs[0]
            .current
            .is_some()
    );
    assert!(
        fixture.scene.events.backend.pstate.direct.outputs[1]
            .current
            .is_some()
    );
    assert!(
        !fixture
            .scene
            .events
            .backend
            .platform
            .cursor_plane_visible_for_output(0)
    );
    finish_direct_fixture(fixture, &mut delivered);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: restore the global tearing early-return before Output cursor
// refresh; skip hardware motion whenever Root tearing is active. The accepted
// Output(1) image and position commits must both disappear.
fn output_owner_cursor_updates_during_root_tearing() {
    let Some(mut fixture) = AtomicPathFixture::with_output_count(PathKind::Physical, 2)
        .expect("two-output cursor/tearing fixture construction")
    else {
        eprintln!(
            "SKIP output_owner_cursor_updates_during_root_tearing: Vulkan fixture unavailable"
        );
        return;
    };
    let mut delivered = HashSet::new();
    for device in &mut fixture.scene.events.backend.platform.devices {
        device.async_page_flip = true;
    }

    // Keep the Root candidate out of direct scanout with a software cursor,
    // so Backend's normal async-Present path enters Root tearing.
    let large_cursor = create_argb_cursor(
        &mut fixture.scene.events.backend,
        (300, 4),
        [0x18, 0x29, 0x3a, 0xff],
        (1, 2),
    );
    define_root_cursor(&mut fixture.scene.events.backend, large_cursor);
    Backend::set_cursor_hidden(&mut fixture.scene.events.backend, false);
    Backend::warp_pointer_root(
        &mut fixture.scene.events.backend,
        &mut fixture.scene.events.state,
        100,
        90,
    );
    damage_root(&mut fixture);
    for frame in 1..=4 {
        Backend::maybe_composite(&mut fixture.scene.events.backend)
            .expect("settle software cursor before Root async Presents");
        for output_idx in 0..2 {
            if has_unretired_accepted_event(&fixture.scene.events, &delivered, output_idx) {
                retire_pending_output(&mut fixture.scene.events, &mut delivered, output_idx, frame);
            }
        }
        if fixture
            .scene
            .events
            .backend
            .scene
            .cursor_mode_for_outputs(&[0, 1])
            == crate::kms::render::scene::CursorPlaneMode::Sw
        {
            break;
        }
    }
    assert_eq!(
        fixture
            .scene
            .events
            .backend
            .scene
            .cursor_mode_for_outputs(&[0, 1]),
        crate::kms::render::scene::CursorPlaneMode::Sw,
        "the oversized cursor keeps Root candidates out of direct scanout"
    );

    fixture
        .scene
        .events
        .backend
        .for_tests_set_scanout_m1_prober(super::super::present_scanout::record_scanout_m1_probe);
    let root = fixture
        .scene
        .install_direct_present(7_221, yserver_core::present_scheduler::PRESENT_OPTION_ASYNC);
    Backend::set_cursor_hidden(&mut fixture.scene.events.backend, false);
    let probation = usize::from(crate::kms::render::backend::TEARING_ENTRY_PROBATION);
    for attempt in 0..probation {
        let mut candidate = root.candidate;
        candidate.present_id += u64::try_from(attempt).expect("attempt fits Present id");
        let mut event = root.event.clone();
        event.present_id = candidate.present_id;
        event.serial = u32::try_from(candidate.present_id).expect("Root serial fits");
        Backend::note_present_scanout_candidate(&mut fixture.scene.events.backend, candidate);
        assert!(
            !Backend::try_present_direct(&mut fixture.scene.events.backend, candidate, event,)
                .expect("Root async Present enters tearing through Backend"),
            "the software cursor keeps the Root Present composed"
        );
    }
    assert!(
        fixture.scene.events.backend.pstate.tearing.is_on(),
        "Root async Presents enter Root tearing"
    );
    assert!(
        !fixture.scene.events.backend.pstate.direct.root().active(),
        "Root tearing has no direct Root owner in this fixture"
    );
    let output1_x = fixture.scene.events.backend.platform.outputs[1].x;
    Backend::warp_pointer_root(
        &mut fixture.scene.events.backend,
        &mut fixture.scene.events.state,
        output1_x + 100,
        90,
    );

    let direct = fixture.scene.install_direct_output_present(
        1,
        7_222,
        yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
    );
    Backend::note_present_scanout_candidate(&mut fixture.scene.events.backend, direct.candidate);
    let device_fd = fixture.scene.kms_fd(1);
    let direct_start = record_count(&fixture, device_fd);
    for attempt in 0..usize::from(crate::kms::render::backend::SCANOUT_M2_ELIGIBLE_ROOT_PROBATION) {
        assert_eq!(
            Backend::try_present_direct(
                &mut fixture.scene.events.backend,
                direct.candidate,
                direct.event.clone(),
            )
            .expect("Output(1) Present stays direct during Root tearing"),
            attempt + 1
                == usize::from(crate::kms::render::backend::SCANOUT_M2_ELIGIBLE_ROOT_PROBATION),
            "Output(1) enters after direct scanout probation"
        );
    }
    let crtc1 = fixture.scene.events.backend.platform.outputs[1].output.crtc;
    let direct_record = accepted_live_records(&fixture.scene.events.recorders[&device_fd])
        .into_iter()
        .skip(direct_start)
        .find(|record| {
            record.events.iter().any(|event| event.crtc == crtc1)
                && accepted_event_id(record).is_some()
        })
        .expect("Output(1) direct Present has an accepted commit");
    assert!(
        !direct_record
            .flags
            .contains(::drm::control::AtomicCommitFlags::PAGE_FLIP_ASYNC),
        "Output(1) remains synchronized while Root tearing is active"
    );
    retire_pending_output(&mut fixture.scene.events, &mut delivered, 1, 40);
    assert!(fixture.scene.events.backend.pstate.tearing.is_on());
    assert!(
        fixture.scene.events.backend.pstate.direct.outputs[1]
            .current
            .is_some(),
        "Output(1) is the live direct owner"
    );

    let cursor = create_argb_cursor(
        &mut fixture.scene.events.backend,
        (4, 4),
        [0x62, 0x71, 0x82, 0xff],
        (1, 2),
    );
    let image_start = record_count(&fixture, device_fd);
    define_root_cursor(&mut fixture.scene.events.backend, cursor);
    Backend::set_cursor_hidden(&mut fixture.scene.events.backend, false);
    let image_record = accepted_live_records(&fixture.scene.events.recorders[&device_fd])
        .into_iter()
        .skip(image_start)
        .find(|record| {
            record.kind == AtomicCommitKind::CursorOnly
                && record_property(
                    record,
                    super::support::cursor_properties_for_output(&fixture.scene.events.backend, 1)
                        .fb_id,
                )
                .is_some_and(|fb| fb != 0)
                && record.events.iter().any(|event| event.crtc == crtc1)
        })
        .expect("Output(1) accepts the new hardware cursor image during Root tearing");
    retire_cursor_record(&mut fixture, &mut delivered, 1, &image_record, 41);
    let binding = applied_cursor_binding(&fixture.scene.events.backend, 1)
        .expect("Output(1) retains its hardware cursor binding");
    let bytes = sprite_bytes(&fixture.scene.events.backend, 1, binding.lease)
        .expect("Output(1) cursor sprite bytes");
    assert_eq!(
        bytes
            .chunks_exact(4)
            .filter(|pixel| *pixel == [0x62, 0x71, 0x82, 0xff])
            .count(),
        16,
        "Output(1) is showing the replacement sprite"
    );
    assert!(
        fixture
            .scene
            .events
            .backend
            .platform
            .cursor_plane_visible_for_output(1)
    );
    assert!(fixture.scene.events.backend.pstate.tearing.is_on());
    assert!(
        fixture.scene.events.backend.pstate.direct.outputs[1]
            .current
            .is_some(),
        "cursor refresh leaves Output(1) direct"
    );

    let properties1 =
        super::support::cursor_properties_for_output(&fixture.scene.events.backend, 1);
    Backend::warp_pointer_root(
        &mut fixture.scene.events.backend,
        &mut fixture.scene.events.state,
        output1_x + 140,
        120,
    );
    assert!(
        fixture.scene.events.backend.pstate.tearing.is_on(),
        "pointer motion enters the Output-owner path while Root tearing is still active"
    );
    let move_start = record_count(&fixture, device_fd);
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("submit Output(1) cursor motion during Root tearing");
    let position = accepted_live_records(&fixture.scene.events.recorders[&device_fd])
        .into_iter()
        .skip(move_start)
        .find(|record| {
            record.kind == AtomicCommitKind::CursorOnly
                && record_property(record, properties1.crtc_x).is_some()
                && record_property(record, properties1.fb_id).is_none()
                && record.events.iter().any(|event| event.crtc == crtc1)
        })
        .expect("Output(1) accepts a position-only cursor commit during Root tearing");
    let output1_layout = &fixture.scene.events.backend.platform.outputs[1];
    assert_eq!(
        record_property(&position, properties1.crtc_x),
        Some(
            u64::try_from(output1_x + 140 - 1 - output1_layout.x).expect("cursor x is nonnegative")
        )
    );
    assert_eq!(record_property(&position, properties1.crtc_y), Some(118));
    retire_cursor_record(&mut fixture, &mut delivered, 1, &position, 42);
    assert!(
        !fixture.scene.events.backend.pstate.tearing.is_on(),
        "the scheduling pass exits Root tearing after the new output window changes Root visibility"
    );
    assert!(
        fixture.scene.events.backend.pstate.direct.outputs[1]
            .current
            .is_some(),
        "Output(1) remains the direct owner after cursor motion"
    );
    finish_direct_fixture(fixture, &mut delivered);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: omit the refresh loop's per-scope hard-failure fallback; a Hidden
// retired cursor does not queue a scene retry to rescue the direct owner.
fn hard_direct_cursor_refresh_failure_unflips_hidden_output_owner() {
    let Some((mut fixture, mut delivered, _hardware, _direct)) =
        held_output_direct_with_two_outputs(7_202, 1, 1, (799, 90), 0)
    else {
        eprintln!(
            "SKIP hard_direct_cursor_refresh_failure_unflips_hidden_output_owner: Vulkan fixture unavailable"
        );
        return;
    };
    Backend::set_cursor_hidden(&mut fixture.scene.events.backend, true);
    assert_eq!(
        fixture
            .scene
            .events
            .backend
            .scene
            .cursor_mode_for_outputs(&[1]),
        crate::kms::render::scene::CursorPlaneMode::Hidden
    );
    assert!(
        fixture.scene.events.backend.pstate.direct.outputs[1]
            .current
            .is_some()
    );

    let cursor_b = create_argb_cursor(
        &mut fixture.scene.events.backend,
        (4, 4),
        [0x65, 0x76, 0x87, 0xff],
        (1, 2),
    );
    define_root_cursor(&mut fixture.scene.events.backend, cursor_b);
    fixture
        .ledger
        .fail_after(CursorBufferOperation::Write, 0, libc::EIO);
    Backend::set_cursor_hidden(&mut fixture.scene.events.backend, false);

    assert_eq!(
        fixture.ledger.failures_taken(),
        [CursorBufferOperation::Write],
        "the test injects a hard hardware-sprite upload failure"
    );
    assert!(
        fixture.scene.events.backend.pstate.direct.outputs[1]
            .current
            .is_some()
    );
    assert!(
        fixture.scene.events.backend.pstate.direct.outputs[1].unflip_requested,
        "hard cursor refresh failure requests fallback even when scene retry is ineligible"
    );
    assert_eq!(
        fixture.scene.events.backend.pstate.direct.outputs[1].unflip_reason,
        Some("direct_cursor_refresh_failed")
    );
    finish_direct_fixture(fixture, &mut delivered);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations C08/C09: keep the any-direct-owner membership guard; suppress
// position submission to every CRTC when another output changes membership.
fn cursor_crossing_with_output_owner() {
    for (row, present_id, direct_output, cursor_output, pointer) in [
        ("C08", 716, 1, 1, (900, 90)),
        ("C09", 7_217, 0, 0, (100, 90)),
    ] {
        let Some((mut fixture, mut delivered, hardware, _direct)) =
            held_output_direct_with_two_outputs(
                present_id,
                direct_output,
                cursor_output,
                pointer,
                0,
            )
        else {
            eprintln!("SKIP cursor_crossing_with_output_owner/{row}: Vulkan fixture unavailable");
            continue;
        };

        if row == "C08" {
            // Mutation C08: retain move_hw_cursor's any-direct-owner guard
            // around membership detection. The composed output must attach.
            assert!(
                fixture.scene.events.backend.pstate.direct.outputs[1]
                    .current
                    .is_some(),
                "{row}: Output(1) owns the direct Present"
            );
            assert!(
                fixture
                    .scene
                    .events
                    .backend
                    .platform
                    .cursor_plane_visible_for_output(1),
                "{row}: cursor starts on direct Output(1)"
            );
            assert!(
                !fixture
                    .scene
                    .events
                    .backend
                    .platform
                    .cursor_plane_visible_for_output(0),
                "{row}: cursor starts hidden on composed Output(0)"
            );

            Backend::warp_pointer_root(
                &mut fixture.scene.events.backend,
                &mut fixture.scene.events.state,
                100,
                90,
            );
            for frame in 50..54 {
                Backend::maybe_composite(&mut fixture.scene.events.backend).unwrap_or_else(
                    |error| panic!("{row}: crossing compose through Backend: {error}"),
                );
                if has_unretired_accepted_event(&fixture.scene.events, &delivered, 0) {
                    retire_pending_output(&mut fixture.scene.events, &mut delivered, 0, frame);
                }
                if fixture
                    .scene
                    .events
                    .backend
                    .platform
                    .cursor_plane_visible_for_output(0)
                {
                    break;
                }
            }

            let binding = applied_cursor_binding(&fixture.scene.events.backend, 0)
                .unwrap_or_else(|| panic!("{row}: composed Output(0) binds cursor after crossing"));
            assert_eq!(
                (binding.x, binding.y),
                (99, 88),
                "{row}: cursor binding position"
            );
            assert!(
                fixture
                    .scene
                    .events
                    .backend
                    .platform
                    .cursor_plane_visible_for_output(0),
                "{row}: composed Output(0) attaches the cursor"
            );
            assert!(
                fixture.scene.events.backend.pstate.direct.outputs[1]
                    .current
                    .is_some(),
                "{row}: Output(1) remains direct after crossing"
            );
        } else {
            // Mutation C09: skip motion for every CRTC when membership on
            // another output wakes composition. Direct Output(0) must move.
            assert_eq!(
                hardware.output_idx, 0,
                "{row}: hardware cursor starts on Output(0)"
            );
            assert_eq!(
                fixture
                    .scene
                    .events
                    .backend
                    .scene
                    .cursor_mode_for_outputs(&[1]),
                crate::kms::render::scene::CursorPlaneMode::Hidden,
                "{row}: Output(1) starts without a visible hardware cursor"
            );
            let properties =
                super::support::cursor_properties_for_output(&fixture.scene.events.backend, 0);
            let device_fd = fixture.scene.kms_fd(0);
            let start = record_count(&fixture, device_fd);

            Backend::warp_pointer_root(
                &mut fixture.scene.events.backend,
                &mut fixture.scene.events.state,
                850,
                90,
            );
            Backend::maybe_composite(&mut fixture.scene.events.backend)
                .unwrap_or_else(|error| panic!("{row}: process crossing through Backend: {error}"));

            let position = accepted_live_records(&fixture.scene.events.recorders[&device_fd])
                .into_iter()
                .skip(start)
                .find(|record| {
                    record.kind == AtomicCommitKind::CursorOnly
                        && record_property(record, properties.crtc_x).is_some()
                        && record_property(record, properties.fb_id).is_none()
                        && record.events.iter().any(|event| {
                            event.crtc
                                == fixture.scene.events.backend.platform.outputs[0].output.crtc
                        })
                })
                .unwrap_or_else(|| {
                    panic!("{row}: direct Output(0) submits position-only commit after Output(1) membership change")
                });
            assert_eq!(
                record_property(&position, properties.crtc_x),
                Some(849),
                "{row}: direct Output(0) CRTC_X"
            );
            assert_eq!(
                record_property(&position, properties.crtc_y),
                Some(88),
                "{row}: direct Output(0) CRTC_Y"
            );
            assert!(
                fixture.scene.events.backend.pstate.direct.outputs[0]
                    .current
                    .is_some(),
                "{row}: cursor motion does not unflip Output(0)"
            );
            assert!(
                fixture.scene.events.backend.pstate.direct.outputs[1]
                    .current
                    .is_none(),
                "{row}: Output(1) remains composed"
            );
            retire_cursor_record(&mut fixture, &mut delivered, 0, &position, 70);
        }
        finish_direct_fixture(fixture, &mut delivered);
    }
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: restore the device-wide permanent-disable flag after one CRTC
// exhausts recovery; clear the per-CRTC disable latch during all-off cleanup.
fn cursor_recovery_exhaustion_disables_only_that_crtc() {
    let run = |present_id: u64, direct_output: usize, disabled_output: usize| {
        let Some((mut fixture, mut delivered, hardware, _direct)) =
            held_output_direct_with_two_outputs(
                present_id,
                direct_output,
                direct_output,
                (799, 90),
                0,
            )
        else {
            eprintln!(
                "SKIP cursor_recovery_exhaustion_disables_only_that_crtc: Vulkan fixture unavailable"
            );
            return;
        };
        assert_eq!(hardware.output_idx, direct_output);
        assert!(
            (0..2).all(|output_idx| fixture
                .scene
                .events
                .backend
                .platform
                .cursor_plane_visible_for_output(output_idx)),
            "the boundary cursor is visible on both CRTC outputs before recovery"
        );
        assert!(
            fixture.scene.events.backend.pstate.direct.outputs[direct_output]
                .current
                .is_some()
        );

        let survivor_output = 1 - disabled_output;
        let device_fd = fixture.scene.kms_fd(0);
        let recorder = Rc::clone(&fixture.scene.events.recorders[&device_fd]);
        let survivor_properties = super::support::cursor_properties_for_output(
            &fixture.scene.events.backend,
            survivor_output,
        );
        let disabled_layout = &fixture.scene.events.backend.platform.outputs[disabled_output];
        let survivor_layout = &fixture.scene.events.backend.platform.outputs[survivor_output];
        let disabled_key = disabled_layout.key.device_key;
        let disabled_crtc = disabled_layout.output.crtc;
        let survivor_key = survivor_layout.key.device_key;
        let survivor_crtc = survivor_layout.output.crtc;

        let start = record_count(&fixture, device_fd);
        recorder.script_result(Ok(()));
        recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EINVAL)));
        recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EIO)));
        Backend::warp_pointer_root(
            &mut fixture.scene.events.backend,
            &mut fixture.scene.events.state,
            798,
            90,
        );
        Backend::maybe_composite(&mut fixture.scene.events.backend)
            .expect("reject the disabled CRTC cursor move through Backend");

        let records = recorder.records();
        let rejected = records
            .iter()
            .skip(start)
            .find(|record| {
                record.kind == AtomicCommitKind::CursorOnly
                    && record
                        .events
                        .iter()
                        .any(|event| event.crtc == disabled_crtc)
                    && record.result
                        == ScriptedResult::Error {
                            kind: std::io::ErrorKind::InvalidInput,
                            raw_os_error: Some(libc::EINVAL),
                        }
            })
            .expect("the disabled CRTC cursor move was rejected with EINVAL");
        let first_survivor_move = records
            .iter()
            .skip(start)
            .find(|record| {
                record.kind == AtomicCommitKind::CursorOnly
                    && record.result == ScriptedResult::Ok
                    && accepted_event_id(record).is_some()
                    && record_property(record, survivor_properties.crtc_x).is_some()
                    && record_property(record, survivor_properties.fb_id).is_none()
                    && record.events.iter().any(|event| event.crtc == survivor_crtc)
            })
            .cloned()
            .unwrap_or_else(|| {
                panic!(
                    "the other CRTC still submits its cursor move; direct_output={direct_output}, disabled_output={disabled_output}, pending0={:?}, pending1={:?}, recent={:?}",
                    fixture.scene.events.backend.platform.for_tests_atomic_cursor_pending_position(fixture.scene.events.backend.platform.outputs[0].key.device_key, fixture.scene.events.backend.platform.outputs[0].output.crtc),
                    fixture.scene.events.backend.platform.for_tests_atomic_cursor_pending_position(fixture.scene.events.backend.platform.outputs[1].key.device_key, fixture.scene.events.backend.platform.outputs[1].output.crtc),
                    records.iter().skip(start).map(|record| (record.kind, record.result, record.events.clone())).collect::<Vec<_>>(),
                )
            });
        assert_ne!(rejected.user_data, 0);
        assert_eq!(
            fixture
                .scene
                .events
                .backend
                .platform
                .for_tests_cursor_hide_recovery_attempts(disabled_key, disabled_crtc),
            Some(0),
            "the rejected move and failed rollback start per-CRTC recovery"
        );
        retire_cursor_record(
            &mut fixture,
            &mut delivered,
            survivor_output,
            &first_survivor_move,
            80,
        );

        for attempt in 1..=3 {
            let offset = record_count(&fixture, device_fd);
            recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EIO)));
            Backend::warp_pointer_root(
                &mut fixture.scene.events.backend,
                &mut fixture.scene.events.state,
                798,
                90 + attempt,
            );
            Backend::maybe_composite(&mut fixture.scene.events.backend)
                .expect("run one scheduled cursor recovery attempt");
            let move_record = recorder
                .records()
                .into_iter()
                .skip(offset)
                .find(|record| {
                    record.kind == AtomicCommitKind::CursorOnly
                        && record.result == ScriptedResult::Ok
                        && accepted_event_id(record).is_some()
                        && record_property(record, survivor_properties.crtc_x).is_some()
                        && record_property(record, survivor_properties.fb_id).is_none()
                        && record.events.iter().any(|event| event.crtc == survivor_crtc)
                })
                .unwrap_or_else(|| {
                    panic!(
                        "the unaffected CRTC keeps submitting its cursor position; attempt={attempt}, direct_output={direct_output}, disabled_output={disabled_output}, survivor_crtc={survivor_crtc:?}, survivor_x_prop={:?}, survivor_fb_prop={:?}, pending_survivor={:?}, current_direct={}, unflip_requested={}, recent={:?}",
                        survivor_properties.crtc_x,
                        survivor_properties.fb_id,
                        fixture.scene.events.backend.platform.for_tests_atomic_cursor_pending_position(survivor_key, survivor_crtc),
                        fixture.scene.events.backend.pstate.direct.outputs[direct_output].current.is_some(),
                        fixture.scene.events.backend.pstate.direct.outputs[direct_output].unflip_requested,
                        recorder.records().iter().skip(offset).map(|record| (record.kind, record.result, record.events.clone(), record.properties.clone())).collect::<Vec<_>>(),
                    )
                });
            let survivor_x = 798_i64
                - i64::from(fixture.scene.events.backend.platform.outputs[survivor_output].x)
                - 1;
            assert_eq!(
                record_property(&move_record, survivor_properties.crtc_x),
                Some(u64::from_ne_bytes(survivor_x.to_ne_bytes())),
                "the unaffected CRTC uses the current root pointer"
            );
            assert_eq!(
                record_property(&move_record, survivor_properties.crtc_y),
                Some(u64::try_from(88 + attempt).expect("cursor y fits u64")),
            );
            retire_cursor_record(
                &mut fixture,
                &mut delivered,
                survivor_output,
                &move_record,
                80 + u32::try_from(attempt).expect("attempt fits frame number"),
            );
        }

        assert!(
            fixture
                .scene
                .events
                .backend
                .platform
                .cursor_plane_available(),
            "the unaffected CRTC remains eligible for hardware cursor use"
        );
        assert!(
            fixture
                .scene
                .events
                .backend
                .platform
                .for_tests_cursor_crtc_permanently_disabled(disabled_key, disabled_crtc)
        );
        assert!(
            !fixture
                .scene
                .events
                .backend
                .platform
                .for_tests_cursor_crtc_permanently_disabled(survivor_key, survivor_crtc)
        );
        assert_eq!(
            fixture
                .scene
                .events
                .backend
                .platform
                .for_tests_atomic_cursor_pending_position(disabled_key, disabled_crtc),
            None,
            "exhaustion clears only the disabled CRTC's position and pending move"
        );
        assert_eq!(
            fixture
                .scene
                .events
                .backend
                .platform
                .for_tests_cursor_hide_recovery_attempts(disabled_key, disabled_crtc),
            None,
            "exhaustion clears only the disabled CRTC's recovery entry"
        );
        assert!(
            fixture.scene.events.backend.pstate.direct.outputs[direct_output]
                .current
                .is_some(),
            "recovery exhaustion leaves the other output's direct owner current"
        );
        assert!(
            !fixture.scene.events.backend.pstate.direct.outputs[direct_output].unflip_requested,
            "recovery exhaustion does not unflip the unrelated owner"
        );
        assert!(
            applied_cursor_binding(&fixture.scene.events.backend, disabled_output).is_some(),
            "the disabled CRTC retains its applied lease until normal release"
        );
        assert!(
            !fixture
                .scene
                .events
                .backend
                .platform
                .for_tests_cursor_permanently_disabled(disabled_key),
            "recovery exhaustion does not set device-wide disable"
        );
        for output_idx in 0..2 {
            drain_unretired_output_events(&mut fixture, &mut delivered, output_idx);
        }

        let (disabled_output_id, disabled_connector) = {
            let backend = &mut fixture.scene.events.backend;
            let key = backend.platform.outputs[disabled_output].key.clone();
            let output_id = backend.pstate.randr_id_alloc.ids_for(&key).output_id;
            backend
                .pstate
                .output_key_by_id
                .insert(output_id, key.clone());
            backend.platform.for_tests_accept_output_power_commits();
            (output_id, key.connector_name)
        };
        assert!(
            Backend::apply_crtc_config(
                &mut fixture.scene.events.backend,
                disabled_output_id,
                &disabled_connector,
                None,
                0,
                0,
            )
            .expect("remove the exhausted CRTC through Backend"),
            "the all-off CRTC removal completes"
        );
        assert!(
            fixture
                .scene
                .events
                .backend
                .platform
                .for_tests_cursor_crtc_permanently_disabled(disabled_key, disabled_crtc),
            "all-off lease cleanup retains the exhausted CRTC's permanent-disable latch"
        );
        finish_direct_fixture(fixture, &mut delivered);
    };

    run(7_218, 0, 1);
    run(7_220, 1, 0);
}

fn admit_two_output_direct_owners(
    fixture: &mut AtomicPathFixture,
    delivered: &mut HashSet<super::support::CompletionKey>,
    present_ids: [u64; 2],
    row: &str,
) {
    fixture.scene.events.backend.platform.outputs[0]
        .output
        .picked
        .vrefresh = 75;
    fixture.scene.events.backend.platform.outputs[1]
        .output
        .picked
        .vrefresh = 240;
    let directs = [
        fixture
            .scene
            .install_direct_output_present(0, present_ids[0], 0),
        fixture
            .scene
            .install_direct_output_present(1, present_ids[1], 0),
    ];
    Backend::set_cursor_hidden(&mut fixture.scene.events.backend, false);
    for output_idx in 0..2 {
        settle_cursor_on_output(fixture, delivered, output_idx, row);
    }
    for (output_idx, direct) in directs.iter().enumerate() {
        super::super::present_scanout::a3::a3_admit_output_owner(
            &mut fixture.scene,
            direct,
            output_idx,
        );
        retire_pending_output(
            &mut fixture.scene.events,
            delivered,
            output_idx,
            81 + u32::try_from(output_idx).expect("output index fits frame"),
        );
        assert_eq!(
            fixture.scene.events.backend.pstate.direct.outputs[output_idx]
                .current
                .as_ref()
                .map(|frame| frame.candidate.present_id),
            Some(direct.candidate.present_id),
            "{row}: Output({output_idx}) owns its accepted Present"
        );
    }
}

fn settle_cursor_on_output(
    fixture: &mut AtomicPathFixture,
    delivered: &mut HashSet<super::support::CompletionKey>,
    output_idx: usize,
    row: &str,
) {
    use crate::kms::render::scene::CursorPlaneMode::{Hidden, Hw};

    for _ in 0..4 {
        Backend::maybe_composite(&mut fixture.scene.events.backend)
            .unwrap_or_else(|error| panic!("{row}: settle cursor mode through Backend: {error}"));
        for idx in 0..2 {
            drain_unretired_output_events(fixture, delivered, idx);
        }
        if matches!(
            fixture
                .scene
                .events
                .backend
                .scene
                .cursor_mode_for_outputs(&[output_idx]),
            Hw | Hidden
        ) {
            return;
        }
    }
    panic!("{row}: cursor mode for output {output_idx} did not settle");
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations C11/C12: restore the Root-only check in handle_cursor_move_outcome.
fn cursor_rejection_isolation() {
    for row in ["C11", "C12"] {
        let Some(mut fixture) = AtomicPathFixture::with_output_count(PathKind::Physical, 2)
            .unwrap_or_else(|error| {
                panic!("{row}: two-output cursor fixture construction: {error}")
            })
        else {
            eprintln!("SKIP cursor_rejection_isolation/{row}: Vulkan fixture unavailable");
            continue;
        };
        let cursor = create_argb_cursor(
            &mut fixture.scene.events.backend,
            (4, 4),
            if row == "C11" {
                [0x71, 0x72, 0x73, 0xff]
            } else {
                [0x74, 0x75, 0x76, 0xff]
            },
            (1, 2),
        );
        let mut delivered = HashSet::new();
        establish_hardware_cursor_on_output(&mut fixture, &mut delivered, cursor, 1, (900, 90));
        for output_idx in 0..2 {
            drain_unretired_output_events(&mut fixture, &mut delivered, output_idx);
        }
        Backend::set_cursor_hidden(&mut fixture.scene.events.backend, false);
        settle_cursor_on_output(&mut fixture, &mut delivered, 0, row);
        settle_cursor_on_output(&mut fixture, &mut delivered, 1, row);
        admit_two_output_direct_owners(
            &mut fixture,
            &mut delivered,
            if row == "C11" {
                [7_221, 7_222]
            } else {
                [7_223, 7_224]
            },
            row,
        );

        let device_fd = fixture.scene.kms_fd(1);
        let recorder = Rc::clone(&fixture.scene.events.recorders[&device_fd]);
        let properties1 =
            super::support::cursor_properties_for_output(&fixture.scene.events.backend, 1);
        let direct1_crtc = fixture.scene.events.backend.platform.outputs[1].output.crtc;

        if row == "C11" {
            // Mutation C11: Root-only handling drops the position-only
            // rejection before it requests Output(1)'s fallback.
            let cursor_start = record_count(&fixture, device_fd);
            recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EINVAL)));
            Backend::warp_pointer_root(
                &mut fixture.scene.events.backend,
                &mut fixture.scene.events.state,
                920,
                100,
            );
            Backend::maybe_composite(&mut fixture.scene.events.backend).unwrap_or_else(|error| {
                panic!("{row}: reject cursor-only move through Backend: {error}")
            });

            let rejected = recorder
                .records()
                .into_iter()
                .skip(cursor_start)
                .find(|record| {
                    record.kind == AtomicCommitKind::CursorOnly
                        && record_property(record, properties1.crtc_x).is_some()
                        && record.result
                            == ScriptedResult::Error {
                                kind: std::io::ErrorKind::InvalidInput,
                                raw_os_error: Some(libc::EINVAL),
                            }
                        && record.events.iter().any(|event| event.crtc == direct1_crtc)
                })
                .unwrap_or_else(|| panic!("{row}: Output(1) cursor-only commit is rejected"));
            assert_ne!(
                rejected.user_data, 0,
                "{row}: rejected cursor commit keeps its request cookie"
            );
            assert!(
                fixture.scene.events.backend.pstate.direct.outputs[1]
                    .current
                    .is_some(),
                "{row}: Output(1) remains current until replacement"
            );
            assert!(
                fixture.scene.events.backend.pstate.direct.outputs[1].unflip_requested,
                "{row}: rejected position requests Output(1) fallback"
            );
            assert_eq!(
                fixture.scene.events.backend.pstate.direct.outputs[1].unflip_reason,
                Some("cursor_move_fallback_or_retry"),
                "{row}: position rejection records cursor fallback reason"
            );
            assert!(
                fixture.scene.events.backend.pstate.direct.outputs[0]
                    .current
                    .is_some(),
                "{row}: Output(0) remains current"
            );
            assert!(
                !fixture.scene.events.backend.pstate.direct.outputs[0].unflip_requested,
                "{row}: Output(0) is not asked to unflip"
            );
            for output_idx in 0..2 {
                drain_unretired_output_events(&mut fixture, &mut delivered, output_idx);
            }
        } else {
            // Mutation C12: the same Root-only handling must also fail when
            // reached from the deferred SceneWithCursor submission callback.
            assert!(
                fixture.scene.events.backend.pstate.direct.outputs[1]
                    .current
                    .is_some(),
                "{row}: Output(1) is current before unmap"
            );
            assert!(
                !fixture.scene.events.backend.pstate.direct.outputs[1].unflip_requested,
                "{row}: Output(1) has no pre-existing unflip request"
            );

            // Unmap starts the normal unflip. Two hard failures take the
            // scheduler through fresh and retained attempts into the existing
            // degraded scene-sink path.
            let scene_start = record_count(&fixture, device_fd);
            let output1_target = fixture.scene.events.backend.pstate.direct.outputs[1]
                .current
                .as_ref()
                .unwrap_or_else(|| panic!("{row}: Output(1) remains current before unmap"))
                .candidate
                .paint_dst_host_xid;
            Backend::unmap_subwindow(&mut fixture.scene.events.backend, None, output1_target)
                .unwrap_or_else(|error| panic!("{row}: unmap Output(1) through Backend: {error}"));
            recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EIO)));
            recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EIO)));
            recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EINVAL)));
            recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EINVAL)));
            Backend::maybe_composite(&mut fixture.scene.events.backend).unwrap_or_else(|error| {
                panic!("{row}: reject deferred cursor in degraded scene sink: {error}")
            });

            let rejected_scene = recorder
                .records()
                .into_iter()
                .skip(scene_start)
                .find(|record| {
                    record_property(record, properties1.crtc_x).is_some()
                        && record.events.iter().any(|event| event.crtc == direct1_crtc)
                        && record.kind == AtomicCommitKind::SceneWithCursor
                        && record.result
                            == ScriptedResult::Error {
                                kind: std::io::ErrorKind::InvalidInput,
                                raw_os_error: Some(libc::EINVAL),
                            }
                })
                .unwrap_or_else(|| {
                    panic!("{row}: SceneWithCursor carrying Output(1)'s cursor is rejected")
                });
            assert_ne!(
                rejected_scene.user_data, 0,
                "{row}: rejected scene commit keeps its request cookie"
            );
            assert!(
                fixture.scene.events.backend.pstate.direct.outputs[1]
                    .current
                    .is_some(),
                "{row}: Output(1) remains current until accepted replacement retires"
            );
            assert!(
                fixture.scene.events.backend.pstate.direct.outputs[1].unflip_requested,
                "{row}: deferred rejection retains Output(1)'s unflip request"
            );
            assert_eq!(
                fixture.scene.events.backend.pstate.direct.outputs[1]
                    .unflip_last_reason
                    .get(),
                Some("cursor_move_fallback_or_retry"),
                "{row}: deferred rejection reaches Output(1)'s cursor callback; records={:?}",
                recorder
                    .records()
                    .into_iter()
                    .skip(scene_start)
                    .map(|record| {
                        let cursor_x = record_property(&record, properties1.crtc_x);
                        (record.kind, record.result, record.events, cursor_x)
                    })
                    .collect::<Vec<_>>()
            );
            assert!(
                fixture.scene.events.backend.pstate.direct.outputs[0]
                    .current
                    .is_some(),
                "{row}: Output(0) remains current"
            );
            assert!(
                !fixture.scene.events.backend.pstate.direct.outputs[0].unflip_requested,
                "{row}: Output(0) is not asked to unflip"
            );

            let retry_start = record_count(&fixture, device_fd);
            let mut accepted_replacement = None;
            for _ in 0..8 {
                let deadline =
                    Backend::next_wakeup(&fixture.scene.events.backend).unwrap_or_else(|| {
                        panic!("{row}: degraded scene retry schedules a Backend wakeup")
                    });
                let until_retry = deadline.saturating_duration_since(Instant::now());
                if !until_retry.is_zero() {
                    // Wait for the scheduler's reported retry timer; scripted
                    // KMS results are fixed and are not ordered by this wait.
                    std::thread::sleep(until_retry);
                }
                Backend::before_block(&mut fixture.scene.events.backend);
                damage_root(&mut fixture);
                Backend::maybe_composite(&mut fixture.scene.events.backend).unwrap_or_else(
                    |error| {
                        panic!(
                            "{row}: retry Output(1)'s degraded replacement through Backend: {error}"
                        )
                    },
                );
                fixture.scene.events.wait_for_submitted_gpu_work();
                accepted_replacement =
                    recorder
                        .records()
                        .into_iter()
                        .skip(retry_start)
                        .find(|record| {
                            record.result == ScriptedResult::Ok
                                && accepted_event_id(record).is_some()
                                && record.events.iter().any(|event| event.crtc == direct1_crtc)
                                && matches!(
                                    record.kind,
                                    AtomicCommitKind::Scene | AtomicCommitKind::SceneWithCursor
                                )
                        });
                if accepted_replacement.is_some() {
                    break;
                }
            }
            let retry_records = recorder
                .records()
                .into_iter()
                .skip(retry_start)
                .collect::<Vec<_>>();
            let retry_record_summary = retry_records
                .iter()
                .map(|record| {
                    (
                        record.kind,
                        record.result,
                        record.events.clone(),
                        accepted_event_id(record),
                    )
                })
                .collect::<Vec<_>>();
            let accepted_replacement = accepted_replacement.unwrap_or_else(|| {
                panic!(
                    "{row}: replacement is accepted after cursor rejection; state=({:?}, {:?}, {}, {}, {:?}, {:?}), scene=(dirty={}, ready={}, retry={}, pending_flip={}, cursor={:?}, wake={:?}), records={retry_record_summary:?}",
                    fixture.scene.events.backend.pstate.direct.outputs[1].current.is_some(),
                    fixture.scene.events.backend.pstate.direct.outputs[1].pending.is_some(),
                    fixture.scene.events.backend.pstate.direct.outputs[1].unflip_requested,
                    fixture.scene.events.backend.pstate.direct.outputs[1].degraded_composed_unflip,
                    fixture.scene.events.backend.pstate.direct.outputs[1].unflip_outputs_to_replace,
                    fixture.scene.events.backend.pstate.direct.outputs[1].unflip_awaiting_outputs,
                    fixture.scene.events.backend.scene.scene_structure_dirty,
                    fixture.scene.events.backend.scene.output_ready_for_submit(1, &fixture.scene.events.backend.platform),
                    fixture.scene.events.backend.scene.output_submit_retry_pending(1),
                    fixture.scene.events.backend.scene.has_pending_page_flip(1),
                    fixture.scene.events.backend.scene.cursor_mode_for_outputs(&[1]),
                    Backend::next_wakeup(&fixture.scene.events.backend),
                )
            });
            let replacement_id = accepted_event_id(&accepted_replacement)
                .unwrap_or_else(|| panic!("{row}: accepted replacement has a commit identity"));
            assert_eq!(
                fixture.scene.events.backend.pstate.direct.outputs[1]
                    .unflip_awaiting_outputs
                    .get(&1),
                Some(&replacement_id),
                "{row}: Output(1) tracks its accepted replacement until retirement"
            );
            assert_eq!(
                retire_pending_output(&mut fixture.scene.events, &mut delivered, 1, 20),
                replacement_id,
                "{row}: Output(1) retires the accepted replacement ID"
            );
            drain_unretired_output_events(&mut fixture, &mut delivered, 0);
            assert!(
                fixture.scene.events.backend.pstate.direct.outputs[1]
                    .current
                    .is_none(),
                "{row}: Output(1) releases its direct owner after replacement retirement"
            );
            assert!(
                fixture.scene.events.backend.pstate.direct.outputs[0]
                    .current
                    .is_some(),
                "{row}: Output(0) remains direct through Output(1)'s replacement"
            );
        }
        finish_direct_fixture(fixture, &mut delivered);
    }
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: retry through all outputs; the output-1 cursor-commit count must stay unchanged.
fn direct_cursor_retry_on_crtc_event_touches_only_its_pending_output() {
    let Some((mut fixture, mut delivered, hardware, _direct)) = held_direct_with_two_outputs(707)
    else {
        eprintln!(
            "SKIP direct_cursor_retry_on_crtc_event_touches_only_its_pending_output: Vulkan fixture unavailable"
        );
        return;
    };
    let device_fd = hardware.device_fd;
    let properties0 =
        super::support::cursor_properties_for_output(&fixture.scene.events.backend, 0);
    let properties1 =
        super::support::cursor_properties_for_output(&fixture.scene.events.backend, 1);
    let start = accepted_live_records(&fixture.scene.events.recorders[&device_fd]).len();

    Backend::warp_pointer_root(
        &mut fixture.scene.events.backend,
        &mut fixture.scene.events.state,
        140,
        120,
    );
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("submit cursor positions on both direct outputs");
    let position0 = accepted_live_records(&fixture.scene.events.recorders[&device_fd])
        .into_iter()
        .skip(start)
        .find(|record| {
            record.kind == AtomicCommitKind::CursorOnly
                && record_property(record, properties0.crtc_x).is_some()
                && record_property(record, properties0.fb_id).is_none()
                && record.events.iter().any(|event| {
                    event.crtc == fixture.scene.events.backend.platform.outputs[0].output.crtc
                })
        })
        .expect("output 0 accepted a position-only cursor commit");
    let position1 = accepted_live_records(&fixture.scene.events.recorders[&device_fd])
        .into_iter()
        .skip(start)
        .find(|record| {
            record.kind == AtomicCommitKind::CursorOnly
                && record_property(record, properties1.crtc_x).is_some()
                && record_property(record, properties1.fb_id).is_none()
                && record.events.iter().any(|event| {
                    event.crtc == fixture.scene.events.backend.platform.outputs[1].output.crtc
                })
        })
        .expect("output 1 accepted a position-only cursor commit");
    retire_cursor_record(&mut fixture, &mut delivered, 1, &position1, 40);

    let cursor_b = create_argb_cursor(
        &mut fixture.scene.events.backend,
        (4, 4),
        [74, 75, 76, 255],
        (1, 2),
    );
    define_root_cursor(&mut fixture.scene.events.backend, cursor_b);
    assert_eq!(
        fixture
            .scene
            .events
            .backend
            .pstate
            .direct
            .root()
            .direct_cursor_refresh_pending,
        HashSet::from([0]),
        "only output 0's in-flight position defers its cursor show"
    );
    let before_output1_event = count_cursor_commits_for_output(&fixture, 1);
    let output1_show = accepted_live_records(&fixture.scene.events.recorders[&device_fd])
        .into_iter()
        .rev()
        .find(|record| {
            record.kind == AtomicCommitKind::CursorOnly
                && record_property(record, properties1.fb_id).is_some_and(|fb| fb != 0)
                && record.events.iter().any(|event| {
                    event.crtc == fixture.scene.events.backend.platform.outputs[1].output.crtc
                })
        })
        .expect("output 1 accepted the new cursor image");
    retire_cursor_record(&mut fixture, &mut delivered, 1, &output1_show, 41);
    let output1_event_did_not_retry_output0 =
        count_cursor_commits_for_output(&fixture, 1) == before_output1_event;
    let output0_remained_pending_after_output1_event = fixture
        .scene
        .events
        .backend
        .pstate
        .direct
        .root()
        .direct_cursor_refresh_pending
        .contains(&0);

    Backend::warp_pointer_root(
        &mut fixture.scene.events.backend,
        &mut fixture.scene.events.state,
        160,
        140,
    );
    let output0_before_retry = count_cursor_commits_for_output(&fixture, 0);
    let output1_before_retry = count_cursor_commits_for_output(&fixture, 1);
    retire_cursor_record(&mut fixture, &mut delivered, 0, &position0, 42);
    let output0_retry_count = count_cursor_commits_for_output(&fixture, 0) - output0_before_retry;
    let output1_retry_count = count_cursor_commits_for_output(&fixture, 1) - output1_before_retry;
    let pending_cleared_after_retry = fixture
        .scene
        .events
        .backend
        .pstate
        .direct
        .root()
        .direct_cursor_refresh_pending
        .is_empty();
    let retry = accepted_live_records(&fixture.scene.events.recorders[&device_fd])
        .into_iter()
        .rev()
        .find(|record| {
            record.kind == AtomicCommitKind::CursorOnly
                && record_property(record, properties0.fb_id).is_some_and(|fb| fb != 0)
                && record.events.iter().any(|event| {
                    event.crtc == fixture.scene.events.backend.platform.outputs[0].output.crtc
                })
        });
    let remained_direct = fixture.scene.events.backend.pstate.direct.root().active()
        && !fixture
            .scene
            .events
            .backend
            .pstate
            .direct
            .root()
            .unflip_requested;
    finish_direct_fixture(fixture, &mut delivered);
    assert!(
        output1_event_did_not_retry_output0,
        "output 1's CRTC event cannot retry output 0's pending cursor"
    );
    assert!(
        output0_remained_pending_after_output1_event,
        "output 0 remains pending until its own CRTC event"
    );
    assert_eq!(output0_retry_count, 1, "output 0 retries exactly once");
    assert_eq!(output1_retry_count, 0, "output 0 does not touch output 1");
    assert!(pending_cleared_after_retry);
    assert!(retry.is_some(), "output 0 accepts its deferred show");
    assert!(
        remained_direct,
        "cursor refresh deferral keeps direct scanout"
    );
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: retry pending cursor refreshes through all outputs; a CRTC event must exceed its per-output bound.
fn direct_cursor_retries_do_not_delay_direct_successors() {
    let Some((mut fixture, mut delivered, hardware, direct)) = held_direct_with_two_outputs(708)
    else {
        eprintln!(
            "SKIP direct_cursor_retries_do_not_delay_direct_successors: Vulkan fixture unavailable"
        );
        return;
    };
    let device_fd = hardware.device_fd;
    let start = accepted_live_records(&fixture.scene.events.recorders[&device_fd]).len();

    Backend::warp_pointer_root(
        &mut fixture.scene.events.backend,
        &mut fixture.scene.events.state,
        150,
        130,
    );
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("submit cursor position commits on each output");
    let positions: Vec<_> = (0..2)
        .map(|output_idx| {
            let properties = super::support::cursor_properties_for_output(
                &fixture.scene.events.backend,
                output_idx,
            );
            let crtc = fixture.scene.events.backend.platform.outputs[output_idx]
                .output
                .crtc;
            accepted_live_records(&fixture.scene.events.recorders[&device_fd])
                .into_iter()
                .skip(start)
                .find(|record| {
                    record.kind == AtomicCommitKind::CursorOnly
                        && record_property(record, properties.crtc_x).is_some()
                        && record_property(record, properties.fb_id).is_none()
                        && record.events.iter().any(|event| event.crtc == crtc)
                })
                .expect("output accepted its cursor position commit")
        })
        .collect();

    let cursor_b = create_argb_cursor(
        &mut fixture.scene.events.backend,
        (4, 4),
        [77, 78, 79, 255],
        (1, 2),
    );
    define_root_cursor(&mut fixture.scene.events.backend, cursor_b);
    assert_eq!(
        fixture
            .scene
            .events
            .backend
            .pstate
            .direct
            .root()
            .direct_cursor_refresh_pending,
        HashSet::from([0, 1]),
        "both in-flight positions defer their replacement shows"
    );

    let mut successor = direct.candidate;
    successor.present_id += 1;
    let mut successor_event = direct.event.clone();
    successor_event.present_id = successor.present_id;
    successor_event.serial = successor_event.serial.wrapping_add(1);
    assert!(
        Backend::try_present_direct(
            &mut fixture.scene.events.backend,
            successor,
            successor_event,
        )
        .expect("queue direct successor through Backend")
    );
    assert!(
        fixture
            .scene
            .events
            .backend
            .pstate
            .direct
            .root()
            .queued_successor
            .is_some()
    );
    Backend::warp_pointer_root(
        &mut fixture.scene.events.backend,
        &mut fixture.scene.events.state,
        180,
        160,
    );

    let before_first_event = [
        count_cursor_commits_for_output(&fixture, 0),
        count_cursor_commits_for_output(&fixture, 1),
    ];
    let mut first_event_did_not_submit_cursor_retry = true;
    for (output_idx, position) in positions.iter().enumerate() {
        retire_cursor_record(
            &mut fixture,
            &mut delivered,
            output_idx,
            position,
            50 + output_idx as u32,
        );
        if output_idx == 0 {
            first_event_did_not_submit_cursor_retry = [
                count_cursor_commits_for_output(&fixture, 0),
                count_cursor_commits_for_output(&fixture, 1),
            ] == before_first_event;
        }
    }
    let records_after_cursor_events =
        accepted_live_records(&fixture.scene.events.recorders[&device_fd]);
    let direct_successor_record = records_after_cursor_events
        .iter()
        .skip(start)
        .rev()
        .find(|record| record.kind != AtomicCommitKind::CursorOnly);
    let Some(direct_successor_record) = direct_successor_record else {
        finish_direct_fixture(fixture, &mut delivered);
        panic!("queued direct successor is submitted before deferred cursor retries");
    };
    assert_eq!(direct_successor_record.events.len(), 2);
    assert_eq!(
        [
            count_cursor_commits_for_output(&fixture, 0),
            count_cursor_commits_for_output(&fixture, 1),
        ],
        before_first_event,
        "the direct successor event is not preceded by cursor-only retry commits"
    );
    let direct_id = accepted_event_id(direct_successor_record)
        .expect("direct successor carries its accepted commit ID");

    let before_direct_events = [
        count_cursor_commits_for_output(&fixture, 0),
        count_cursor_commits_for_output(&fixture, 1),
    ];
    assert_eq!(
        retire_pending_output(&mut fixture.scene.events, &mut delivered, 0, 52),
        direct_id
    );
    assert_eq!(
        count_cursor_commits_for_output(&fixture, 0) - before_direct_events[0],
        1
    );
    assert_eq!(
        count_cursor_commits_for_output(&fixture, 1) - before_direct_events[1],
        0
    );
    let output0_retry = accepted_live_records(&fixture.scene.events.recorders[&device_fd])
        .into_iter()
        .rev()
        .find(|record| {
            record.kind == AtomicCommitKind::CursorOnly
                && record.events.iter().any(|event| {
                    event.crtc == fixture.scene.events.backend.platform.outputs[0].output.crtc
                })
                && accepted_event_id(record).is_some_and(|id| id != direct_id)
        })
        .expect("direct output 0 event retries only its pending cursor");
    assert!(
        fixture
            .scene
            .events
            .backend
            .pstate
            .direct
            .root()
            .direct_cursor_refresh_pending
            .contains(&1)
    );

    let before_output1_event = [
        count_cursor_commits_for_output(&fixture, 0),
        count_cursor_commits_for_output(&fixture, 1),
    ];
    assert_eq!(
        retire_pending_output(&mut fixture.scene.events, &mut delivered, 1, 53),
        direct_id
    );
    assert_eq!(
        count_cursor_commits_for_output(&fixture, 0) - before_output1_event[0],
        0
    );
    assert_eq!(
        count_cursor_commits_for_output(&fixture, 1) - before_output1_event[1],
        1
    );
    assert!(
        fixture
            .scene
            .events
            .backend
            .pstate
            .direct
            .root()
            .direct_cursor_refresh_pending
            .is_empty()
    );
    for (output_idx, retry) in [
        (0, output0_retry),
        (
            1,
            accepted_live_records(&fixture.scene.events.recorders[&device_fd])
                .into_iter()
                .rev()
                .find(|record| {
                    record.kind == AtomicCommitKind::CursorOnly
                        && record.events.iter().any(|event| {
                            event.crtc
                                == fixture.scene.events.backend.platform.outputs[1].output.crtc
                        })
                        && accepted_event_id(record).is_some_and(|id| id != direct_id)
                })
                .expect("direct output 1 event retries only its pending cursor"),
        ),
    ] {
        retire_cursor_record(
            &mut fixture,
            &mut delivered,
            output_idx,
            &retry,
            54 + output_idx as u32,
        );
    }
    let still_direct = fixture.scene.events.backend.pstate.direct.root().active()
        && !fixture
            .scene
            .events
            .backend
            .pstate
            .direct
            .root()
            .unflip_requested;
    finish_direct_fixture(fixture, &mut delivered);
    assert!(
        first_event_did_not_submit_cursor_retry,
        "a queued direct successor prevents cursor-only retries on both CRTCs"
    );
    assert!(still_direct);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: remove the two-event retry bound; the pending show and hide must fail to land.
fn direct_cursor_retry_lands_under_continuous_successor_stream() {
    for hide in [false, true] {
        let Some(mut fixture) = AtomicPathFixture::new(PathKind::Physical)
            .expect("one-output atomic direct cursor fixture construction")
        else {
            eprintln!(
                "SKIP direct_cursor_retry_lands_under_continuous_successor_stream: Vulkan fixture unavailable"
            );
            return;
        };
        let mut delivered = HashSet::new();
        let cursor = create_argb_cursor(
            &mut fixture.scene.events.backend,
            (4, 4),
            [71, 72, 73, 255],
            (1, 2),
        );
        let hardware = establish_hardware_cursor(&mut fixture, &mut delivered, cursor, (100, 90));
        let direct = enter_held_direct(
            &mut fixture,
            &mut delivered,
            if hide { 713 } else { 712 },
            None,
        )
        .expect("enter one-output direct scanout through Backend");
        drain_unretired_output_events(&mut fixture, &mut delivered, 0);
        let device_fd = hardware.device_fd;
        let properties =
            super::support::cursor_properties_for_output(&fixture.scene.events.backend, 0);
        let accepted_start =
            accepted_live_records(&fixture.scene.events.recorders[&device_fd]).len();

        Backend::warp_pointer_root(
            &mut fixture.scene.events.backend,
            &mut fixture.scene.events.state,
            140,
            120,
        );
        Backend::maybe_composite(&mut fixture.scene.events.backend)
            .expect("submit in-flight cursor position");
        accepted_live_records(&fixture.scene.events.recorders[&device_fd])
            .into_iter()
            .skip(accepted_start)
            .find(|record| {
                record.kind == AtomicCommitKind::CursorOnly
                    && record_property(record, properties.crtc_x).is_some()
                    && record_property(record, properties.fb_id).is_none()
            })
            .expect("cursor position commit is in flight");

        if hide {
            Backend::set_cursor_hidden(&mut fixture.scene.events.backend, true);
        } else {
            let cursor_b = create_argb_cursor(
                &mut fixture.scene.events.backend,
                (4, 4),
                [74, 75, 76, 255],
                (1, 2),
            );
            define_root_cursor(&mut fixture.scene.events.backend, cursor_b);
        }
        assert!(
            fixture
                .scene
                .events
                .backend
                .pstate
                .direct
                .root()
                .direct_cursor_refresh_pending
                .contains(&0)
        );
        assert!(
            !fixture
                .scene
                .events
                .backend
                .pstate
                .direct
                .root()
                .unflip_requested
        );

        let mut next_present_id = direct.candidate.present_id.wrapping_add(1);
        let queue_successor = |fixture: &mut AtomicPathFixture, present_id: u64| {
            let mut candidate = direct.candidate;
            candidate.present_id = present_id;
            let mut event = direct.event.clone();
            event.present_id = present_id;
            event.serial = event.serial.wrapping_add(1);
            assert!(
                Backend::try_present_direct(&mut fixture.scene.events.backend, candidate, event,)
                    .expect("queue direct successor through Backend")
            );
            assert!(
                fixture
                    .scene
                    .events
                    .backend
                    .pstate
                    .direct
                    .root()
                    .queued_successor
                    .is_some()
            );
        };
        let direct_count = |fixture: &AtomicPathFixture| {
            accepted_live_records(&fixture.scene.events.recorders[&device_fd])
                .into_iter()
                .filter(|record| record.kind != AtomicCommitKind::CursorOnly)
                .count()
        };
        let cursor_count =
            |fixture: &AtomicPathFixture| count_cursor_commits_for_output(fixture, 0);

        for event_idx in 0..5 {
            if fixture
                .scene
                .events
                .backend
                .pstate
                .direct
                .root()
                .queued_successor
                .is_none()
            {
                queue_successor(&mut fixture, next_present_id);
                next_present_id = next_present_id.wrapping_add(1);
            }
            assert!(
                fixture
                    .scene
                    .events
                    .backend
                    .pstate
                    .direct
                    .root()
                    .queued_successor
                    .is_some()
            );
            let direct_before = direct_count(&fixture);
            let cursor_before = cursor_count(&fixture);
            retire_pending_output(&mut fixture.scene.events, &mut delivered, 0, 60 + event_idx);
            let new_direct = direct_count(&fixture) - direct_before;
            let new_cursor = cursor_count(&fixture) - cursor_before;
            assert!(
                new_direct > 0
                    || new_cursor > 0
                    || fixture
                        .scene
                        .events
                        .backend
                        .pstate
                        .direct
                        .root()
                        .queued_successor
                        .is_some(),
                "event {event_idx} advances a successor or the bounded cursor retry"
            );
            if event_idx == 0 {
                assert_eq!(
                    new_direct, 1,
                    "the first postponed event submits its successor"
                );
                assert_eq!(new_cursor, 0, "direct frame keeps priority once");
                assert!(
                    fixture
                        .scene
                        .events
                        .backend
                        .pstate
                        .direct
                        .root()
                        .direct_cursor_refresh_pending
                        .contains(&0)
                );
            }
            if event_idx == 1 {
                assert_eq!(new_direct, 0);
                assert_eq!(
                    new_cursor, 1,
                    "retry lands after exactly two postponed events"
                );
                assert!(
                    fixture
                        .scene
                        .events
                        .backend
                        .pstate
                        .direct
                        .root()
                        .direct_cursor_refresh_pending
                        .is_empty()
                );
                assert!(
                    accepted_live_records(&fixture.scene.events.recorders[&device_fd])
                        .into_iter()
                        .skip(accepted_start)
                        .any(|record| {
                            record.kind == AtomicCommitKind::CursorOnly
                                && record_property(&record, properties.fb_id)
                                    .is_some_and(|fb_id| if hide { fb_id == 0 } else { fb_id != 0 })
                        }),
                    "bounded retry accepted the requested cursor state"
                );
            }
            if event_idx >= 2 {
                assert!(new_direct > 0, "successors continue after the cursor retry");
            }
        }
        assert!(fixture.scene.events.backend.pstate.direct.root().active());
        assert!(
            !fixture
                .scene
                .events
                .backend
                .pstate
                .direct
                .root()
                .unflip_requested
        );

        fixture
            .scene
            .events
            .backend
            .stop_direct_after_scanout_replaced(
                crate::kms::render::backend::DirectScope::Root,
                "continuous cursor retry test teardown",
            );
        drain_unretired_output_events(&mut fixture, &mut delivered, 0);
        if fixture
            .scene
            .events
            .backend
            .platform
            .cursor_plane_visible_for_output(0)
        {
            fixture
                .scene
                .events
                .backend
                .platform
                .cursor_plane_hide_on_crtc(0)
                .expect("detach test cursor during teardown");
        }
        drain_unretired_output_events(&mut fixture, &mut delivered, 0);
        assert!(
            fixture
                .scene
                .events
                .backend
                .pstate
                .direct
                .root()
                .direct_cursor_refresh_pending
                .is_empty()
        );
        assert!(
            fixture
                .scene
                .events
                .backend
                .pstate
                .direct
                .root()
                .direct_cursor_refresh_successor_deferrals
                .is_empty()
        );
        super::support::assert_no_cursor_resources(&fixture.scene.events.backend, None);
        super::support::finish_atomic_fixture(fixture, &mut delivered);
    }
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: treat a deferred direct hide as a failure; both direct outputs must remain active and pending.
fn direct_deferred_cursor_hide_keeps_direct_and_retries_on_event() {
    let Some((mut fixture, mut delivered, hardware, _direct)) = held_direct_with_two_outputs(709)
    else {
        eprintln!(
            "SKIP direct_deferred_cursor_hide_keeps_direct_and_retries_on_event: Vulkan fixture unavailable"
        );
        return;
    };
    let device_fd = hardware.device_fd;
    let properties: Vec<_> = (0..2)
        .map(|output_idx| {
            super::support::cursor_properties_for_output(&fixture.scene.events.backend, output_idx)
        })
        .collect();
    assert_eq!(device_fd, fixture.scene.kms_fd(1));
    let start = record_count(&fixture, device_fd);
    Backend::warp_pointer_root(
        &mut fixture.scene.events.backend,
        &mut fixture.scene.events.state,
        140,
        120,
    );
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("submit in-flight cursor positions on both direct outputs");
    let positions: Vec<_> = (0..2)
        .map(|output_idx| {
            let crtc = fixture.scene.events.backend.platform.outputs[output_idx]
                .output
                .crtc;
            accepted_live_records(&fixture.scene.events.recorders[&device_fd])
                .into_iter()
                .skip(start)
                .find(|record| {
                    record.kind == AtomicCommitKind::CursorOnly
                        && record_property(record, properties[output_idx].crtc_x).is_some()
                        && record_property(record, properties[output_idx].fb_id).is_none()
                        && record.events.iter().any(|event| event.crtc == crtc)
                })
                .expect("direct output accepted a cursor position commit")
        })
        .collect();

    let before_hide = [
        count_cursor_commits_for_output(&fixture, 0),
        count_cursor_commits_for_output(&fixture, 1),
    ];
    Backend::set_cursor_hidden(&mut fixture.scene.events.backend, true);
    let deferred_hide_kept_direct = fixture.scene.events.backend.pstate.direct.root().active()
        && !fixture
            .scene
            .events
            .backend
            .pstate
            .direct
            .root()
            .unflip_requested;
    let pending_after_hide = fixture
        .scene
        .events
        .backend
        .pstate
        .direct
        .root()
        .direct_cursor_refresh_pending
        .clone();
    let hide_deferred_without_second_commit = [
        count_cursor_commits_for_output(&fixture, 0),
        count_cursor_commits_for_output(&fixture, 1),
    ] == before_hide;

    retire_cursor_record(&mut fixture, &mut delivered, 0, &positions[0], 60);
    let counts_after_output0_event = [
        count_cursor_commits_for_output(&fixture, 0),
        count_cursor_commits_for_output(&fixture, 1),
    ];
    let hide0 = accepted_live_records(&fixture.scene.events.recorders[&device_fd])
        .into_iter()
        .skip(start)
        .rev()
        .find(|record| {
            record.kind == AtomicCommitKind::CursorOnly
                && record_property(record, properties[0].fb_id) == Some(0)
                && record.events.iter().any(|event| {
                    event.crtc == fixture.scene.events.backend.platform.outputs[0].output.crtc
                })
        });
    let output0_retry_is_scoped = counts_after_output0_event[0] - before_hide[0] == 1
        && counts_after_output0_event[1] - before_hide[1] == 0;
    let pending_after_output0_event = fixture
        .scene
        .events
        .backend
        .pstate
        .direct
        .root()
        .direct_cursor_refresh_pending
        .clone();
    let visibility_after_output0_event = [
        fixture
            .scene
            .events
            .backend
            .platform
            .cursor_plane_visible_for_output(0),
        fixture
            .scene
            .events
            .backend
            .platform
            .cursor_plane_visible_for_output(1),
    ];
    if let Some(hide) = &hide0 {
        retire_cursor_record(&mut fixture, &mut delivered, 0, hide, 61);
    }

    retire_cursor_record(&mut fixture, &mut delivered, 1, &positions[1], 62);
    let counts_after_output1_event = [
        count_cursor_commits_for_output(&fixture, 0),
        count_cursor_commits_for_output(&fixture, 1),
    ];
    let hide1 = accepted_live_records(&fixture.scene.events.recorders[&device_fd])
        .into_iter()
        .skip(start)
        .rev()
        .find(|record| {
            record.kind == AtomicCommitKind::CursorOnly
                && record_property(record, properties[1].fb_id) == Some(0)
                && record.events.iter().any(|event| {
                    event.crtc == fixture.scene.events.backend.platform.outputs[1].output.crtc
                })
        });
    if let Some(hide) = &hide1 {
        retire_cursor_record(&mut fixture, &mut delivered, 1, hide, 63);
    }
    let pending_after_output1_event = fixture
        .scene
        .events
        .backend
        .pstate
        .direct
        .root()
        .direct_cursor_refresh_pending
        .clone();
    let hidden_after_both_events = (0..2).all(|output_idx| {
        !fixture
            .scene
            .events
            .backend
            .platform
            .cursor_plane_visible_for_output(output_idx)
    });
    let no_cursor_commit_in_flight = !fixture
        .scene
        .events
        .backend
        .platform
        .any_cursor_commit_in_flight();
    let hide_retries_are_scoped = counts_after_output1_event[0] - counts_after_output0_event[0]
        == 0
        && counts_after_output1_event[1] - counts_after_output0_event[1] == 1;
    let still_direct = fixture.scene.events.backend.pstate.direct.root().active()
        && !fixture
            .scene
            .events
            .backend
            .pstate
            .direct
            .root()
            .unflip_requested;
    finish_direct_fixture(fixture, &mut delivered);
    assert!(
        deferred_hide_kept_direct,
        "a deferred hide keeps direct scanout"
    );
    assert!(
        pending_after_hide == HashSet::from([0, 1]),
        "both outputs record the deferred hide"
    );
    assert!(hide_deferred_without_second_commit);
    assert!(
        output0_retry_is_scoped,
        "output 0's event hides only output 0"
    );
    assert_eq!(pending_after_output0_event, HashSet::from([1]));
    assert_eq!(visibility_after_output0_event, [false, true]);
    assert!(hide0.is_some(), "output 0 accepted its deferred hide");
    assert!(
        hide_retries_are_scoped,
        "output 1's event hides only output 1"
    );
    assert_eq!(pending_after_output1_event, HashSet::new());
    assert!(hide1.is_some(), "output 1 accepted its deferred hide");
    assert!(hidden_after_both_events);
    assert!(no_cursor_commit_in_flight);
    assert!(
        still_direct,
        "successful deferred hides preserve direct scanout"
    );
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: replay the deferred hide instead of the current shown intent; both cursors must remain visible.
fn direct_deferred_hide_then_show_applies_newest_intent() {
    let Some((mut fixture, mut delivered, hardware, _direct)) = held_direct_with_two_outputs(710)
    else {
        eprintln!(
            "SKIP direct_deferred_hide_then_show_applies_newest_intent: Vulkan fixture unavailable"
        );
        return;
    };
    let device_fd = hardware.device_fd;
    let properties: Vec<_> = (0..2)
        .map(|output_idx| {
            super::support::cursor_properties_for_output(&fixture.scene.events.backend, output_idx)
        })
        .collect();
    let start = record_count(&fixture, device_fd);
    Backend::warp_pointer_root(
        &mut fixture.scene.events.backend,
        &mut fixture.scene.events.state,
        140,
        120,
    );
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("submit in-flight cursor positions on both direct outputs");
    let positions: Vec<_> = (0..2)
        .map(|output_idx| {
            let crtc = fixture.scene.events.backend.platform.outputs[output_idx]
                .output
                .crtc;
            accepted_live_records(&fixture.scene.events.recorders[&device_fd])
                .into_iter()
                .skip(start)
                .find(|record| {
                    record.kind == AtomicCommitKind::CursorOnly
                        && record_property(record, properties[output_idx].crtc_x).is_some()
                        && record_property(record, properties[output_idx].fb_id).is_none()
                        && record.events.iter().any(|event| event.crtc == crtc)
                })
                .expect("direct output accepted a cursor position commit")
        })
        .collect();

    Backend::set_cursor_hidden(&mut fixture.scene.events.backend, true);
    let hide_deferred_on_both = fixture
        .scene
        .events
        .backend
        .pstate
        .direct
        .root()
        .direct_cursor_refresh_pending
        == HashSet::from([0, 1]);
    Backend::set_cursor_hidden(&mut fixture.scene.events.backend, false);
    let shown_intent_is_current = !fixture.scene.events.backend.effective_cursor_hidden()
        && !fixture
            .scene
            .events
            .backend
            .pstate
            .direct
            .root()
            .unflip_requested;

    let before_retry = [
        count_cursor_commits_for_output(&fixture, 0),
        count_cursor_commits_for_output(&fixture, 1),
    ];
    let retry_start = record_count(&fixture, device_fd);
    retire_cursor_record(&mut fixture, &mut delivered, 0, &positions[0], 62);
    let after_output0_event = [
        count_cursor_commits_for_output(&fixture, 0),
        count_cursor_commits_for_output(&fixture, 1),
    ];
    let pending_after_output0_event = fixture
        .scene
        .events
        .backend
        .pstate
        .direct
        .root()
        .direct_cursor_refresh_pending
        .clone();
    retire_cursor_record(&mut fixture, &mut delivered, 1, &positions[1], 63);
    let after_output1_event = [
        count_cursor_commits_for_output(&fixture, 0),
        count_cursor_commits_for_output(&fixture, 1),
    ];
    let pending_after_output1_event = fixture
        .scene
        .events
        .backend
        .pstate
        .direct
        .root()
        .direct_cursor_refresh_pending
        .clone();
    let cursor_remained_visible = (0..2).all(|output_idx| {
        fixture
            .scene
            .events
            .backend
            .platform
            .cursor_plane_visible_for_output(output_idx)
    });
    let stale_hide_submitted = accepted_live_records(&fixture.scene.events.recorders[&device_fd])
        .into_iter()
        .skip(retry_start)
        .any(|record| {
            record.kind == AtomicCommitKind::CursorOnly
                && properties
                    .iter()
                    .any(|property| record_property(&record, property.fb_id) == Some(0))
        });
    let retry_commits_are_scoped =
        after_output0_event == before_retry && after_output1_event == after_output0_event;
    let no_cursor_commit_in_flight = !fixture
        .scene
        .events
        .backend
        .platform
        .any_cursor_commit_in_flight();
    let still_direct = fixture.scene.events.backend.pstate.direct.root().active()
        && !fixture
            .scene
            .events
            .backend
            .pstate
            .direct
            .root()
            .unflip_requested;
    finish_direct_fixture(fixture, &mut delivered);
    assert!(hide_deferred_on_both);
    assert!(shown_intent_is_current);
    assert_eq!(
        after_output0_event, before_retry,
        "output 0 needs no show commit"
    );
    assert_eq!(
        after_output1_event, after_output0_event,
        "output 1 needs no show commit"
    );
    assert_eq!(pending_after_output0_event, HashSet::from([1]));
    assert!(pending_after_output1_event.is_empty());
    assert!(retry_commits_are_scoped);
    assert!(cursor_remained_visible);
    assert!(
        !stale_hide_submitted,
        "the event does not apply the stale hide"
    );
    assert!(no_cursor_commit_in_flight);
    assert!(still_direct);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: retry only on a CRTC event. The idle output 0 hide must recover from its scheduled wake.
fn direct_cursor_hide_generic_error_keeps_direct_scanout() {
    let Some((mut fixture, mut delivered, hardware, _direct)) = held_direct_with_two_outputs(711)
    else {
        eprintln!(
            "SKIP direct_cursor_hide_generic_error_keeps_direct_scanout: Vulkan fixture unavailable"
        );
        return;
    };
    let device_fd = hardware.device_fd;
    let properties0 =
        super::support::cursor_properties_for_output(&fixture.scene.events.backend, 0);
    let properties1 =
        super::support::cursor_properties_for_output(&fixture.scene.events.backend, 1);
    let recorder = Rc::clone(&fixture.scene.events.recorders[&device_fd]);
    let (elapsed_ns, base) = install_direct_retry_clock(&mut fixture.scene.events.backend);
    let start = record_count(&fixture, device_fd);
    let output0_attempts_before = cursor_hide_attempts_for_output(&fixture, 0);
    recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EIO)));
    Backend::set_cursor_hidden(&mut fixture.scene.events.backend, true);
    let real_error = recorder.records().into_iter().skip(start).any(|record| {
        record.kind == AtomicCommitKind::CursorOnly
            && record_property(&record, properties0.fb_id) == Some(0)
            && record.events.iter().any(|event| {
                event.crtc == fixture.scene.events.backend.platform.outputs[0].output.crtc
            })
            && matches!(
                record.result,
                ScriptedResult::Error {
                    raw_os_error: Some(errno),
                    ..
                } if errno == libc::EIO
            )
    });
    let still_direct = fixture.scene.events.backend.pstate.direct.root().active()
        && !fixture
            .scene
            .events
            .backend
            .pstate
            .direct
            .root()
            .unflip_requested;
    let retry_pending = fixture
        .scene
        .events
        .backend
        .pstate
        .direct
        .root()
        .direct_cursor_refresh_pending
        .contains(&0);
    let output1_hide_succeeded = recorder.records().into_iter().skip(start).any(|record| {
        record.kind == AtomicCommitKind::CursorOnly
            && record_property(&record, properties1.fb_id) == Some(0)
            && record.events.iter().any(|event| {
                event.crtc == fixture.scene.events.backend.platform.outputs[1].output.crtc
            })
            && record.result == ScriptedResult::Ok
    });
    let output1_is_hidden = !fixture
        .scene
        .events
        .backend
        .platform
        .cursor_plane_visible_for_output(1);
    let output1_hide = recorder
        .records()
        .into_iter()
        .skip(start)
        .find(|record| {
            record.kind == AtomicCommitKind::CursorOnly
                && record_property(record, properties1.fb_id) == Some(0)
                && record.events.iter().any(|event| {
                    event.crtc == fixture.scene.events.backend.platform.outputs[1].output.crtc
                })
                && record.result == ScriptedResult::Ok
        })
        .expect("output 1's hide succeeds and owns its CRTC event");
    retire_cursor_record(&mut fixture, &mut delivered, 1, &output1_hide, 12);
    let output0_attempts_after_output1_event = cursor_hide_attempts_for_output(&fixture, 0);
    for _ in 0..2 {
        Backend::maybe_composite(&mut fixture.scene.events.backend)
            .expect("idle direct ticks wait for the scheduled retry deadline");
    }
    let output0_attempts_before_deadline = cursor_hide_attempts_for_output(&fixture, 0);
    let retry_deadline = base + Duration::from_millis(50);
    let wake_after_failure = Backend::next_wakeup(&fixture.scene.events.backend);
    let retry_deadline_is_armed = fixture
        .scene
        .events
        .backend
        .platform
        .direct_cursor_retry_deadline(0)
        == Some(retry_deadline);
    advance_direct_retry_clock(&elapsed_ns, Duration::from_millis(50));
    let due_wakeup = Backend::next_wakeup(&fixture.scene.events.backend);
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("the scheduled wake retries output 0 without a new frame");
    let output0_retry = accepted_live_records(&recorder)
        .into_iter()
        .skip(start)
        .rev()
        .find(|record| {
            record.kind == AtomicCommitKind::CursorOnly
                && record_property(record, properties0.fb_id) == Some(0)
                && record.events.iter().any(|event| {
                    event.crtc == fixture.scene.events.backend.platform.outputs[0].output.crtc
                })
                && record.result == ScriptedResult::Ok
        })
        .expect("output 0 accepts its hide after the injected error clears");
    retire_cursor_record(&mut fixture, &mut delivered, 0, &output0_retry, 13);
    let pending_cleared = fixture
        .scene
        .events
        .backend
        .pstate
        .direct
        .root()
        .direct_cursor_refresh_pending
        .is_empty();
    let output0_is_hidden = !fixture
        .scene
        .events
        .backend
        .platform
        .cursor_plane_visible_for_output(0);
    let no_new_frame = !recorder
        .records()
        .into_iter()
        .skip(start)
        .any(|record| record.kind != AtomicCommitKind::CursorOnly);
    assert!(
        still_direct,
        "the initial generic hide error keeps direct scanout"
    );
    assert!(retry_pending, "the initial direct hide remains pending");
    let still_direct = fixture.scene.events.backend.pstate.direct.root().active()
        && !fixture
            .scene
            .events
            .backend
            .pstate
            .direct
            .root()
            .unflip_requested;
    let output0_attempts_after_retry = cursor_hide_attempts_for_output(&fixture, 0);
    finish_direct_fixture(fixture, &mut delivered);
    assert!(
        real_error,
        "the scripted EIO reached the atomic cursor hide"
    );
    assert!(
        still_direct,
        "generic cursor hide errors preserve direct scanout"
    );
    assert_eq!(
        output0_attempts_after_output1_event,
        output0_attempts_before + 1
    );
    assert_eq!(
        output0_attempts_before_deadline,
        output0_attempts_after_output1_event
    );
    assert!(retry_deadline_is_armed);
    assert!(wake_after_failure.is_some_and(|wakeup| wakeup <= retry_deadline));
    assert!(due_wakeup.is_some_and(|wakeup| wakeup <= retry_deadline));
    assert_eq!(
        output0_attempts_after_retry,
        output0_attempts_after_output1_event + 1,
        "the idle retry is bounded to one successful retry",
    );
    assert!(
        pending_cleared,
        "the accepted retry clears the pending state"
    );
    assert!(output0_is_hidden, "output 0 is hidden after its retry");
    assert!(
        no_new_frame,
        "the recovery submits no direct successor frame"
    );
    assert!(output1_hide_succeeded, "output 1 still accepts its hide");
    assert!(output1_is_hidden, "output 1's accepted hide is reflected");
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: remove the three-attempt bound. Persistent EIO must request unflip after three failures.
fn direct_cursor_hide_persistent_error_unflips_after_recovery_bound() {
    let Some((mut fixture, mut delivered, hardware, _direct)) = held_direct_with_two_outputs(714)
    else {
        eprintln!(
            "SKIP direct_cursor_hide_persistent_error_unflips_after_recovery_bound: Vulkan fixture unavailable"
        );
        return;
    };
    let device_fd = hardware.device_fd;
    let properties1 =
        super::support::cursor_properties_for_output(&fixture.scene.events.backend, 1);
    let recorder = Rc::clone(&fixture.scene.events.recorders[&device_fd]);
    let (elapsed_ns, _) = install_direct_retry_clock(&mut fixture.scene.events.backend);
    let start = record_count(&fixture, device_fd);
    let attempts_before = cursor_hide_attempts_for_output(&fixture, 0);
    recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EIO)));
    Backend::set_cursor_hidden(&mut fixture.scene.events.backend, true);
    let output1_hide = recorder
        .records()
        .into_iter()
        .skip(start)
        .find(|record| {
            record.kind == AtomicCommitKind::CursorOnly
                && record_property(record, properties1.fb_id) == Some(0)
                && record.events.iter().any(|event| {
                    event.crtc == fixture.scene.events.backend.platform.outputs[1].output.crtc
                })
                && record.result == ScriptedResult::Ok
        })
        .expect("output 1 hide succeeds while output 0 fails");
    retire_cursor_record(&mut fixture, &mut delivered, 1, &output1_hide, 20);
    assert_eq!(
        cursor_hide_attempts_for_output(&fixture, 0),
        attempts_before + 1,
        "the initial output 0 failure is the first bounded attempt",
    );
    assert!(
        !fixture
            .scene
            .events
            .backend
            .pstate
            .direct
            .root()
            .unflip_requested,
        "the first failure keeps direct scanout active",
    );

    for attempt in 2..=3 {
        recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EIO)));
        for _ in 0..2 {
            Backend::maybe_composite(&mut fixture.scene.events.backend)
                .expect("busy direct ticks do not retry before the deadline");
        }
        assert_eq!(
            cursor_hide_attempts_for_output(&fixture, 0),
            attempts_before + attempt - 1,
            "non-due ticks do not create a per-vblank cursor error storm",
        );
        advance_direct_retry_clock(&elapsed_ns, Duration::from_millis(50));
        Backend::maybe_composite(&mut fixture.scene.events.backend)
            .expect("a due direct cursor retry runs through Backend");
        assert_eq!(
            cursor_hide_attempts_for_output(&fixture, 0),
            attempts_before + attempt,
            "each scheduled failure consumes one bounded recovery attempt",
        );
        if attempt < 3 {
            assert!(
                !fixture
                    .scene
                    .events
                    .backend
                    .pstate
                    .direct
                    .root()
                    .unflip_requested,
                "direct scanout remains active before the bound",
            );
        }
    }

    let unflip_after_bound = fixture
        .scene
        .events
        .backend
        .pstate
        .direct
        .root()
        .unflip_requested;
    let failed_attempts = cursor_hide_attempts_for_output(&fixture, 0);
    for _ in 0..3 {
        Backend::maybe_composite(&mut fixture.scene.events.backend)
            .expect("the requested unflip is not followed by cursor retry spam");
    }
    let attempts_after_unflip = cursor_hide_attempts_for_output(&fixture, 0);
    let unflip = accepted_live_records(&recorder)
        .into_iter()
        .skip(start)
        .find(|record| record.kind != AtomicCommitKind::CursorOnly)
        .expect("bounded cursor recovery submits a composed unflip");
    let unflip_id = accepted_event_id(&unflip).expect("accepted composed unflip has an ID");
    assert_eq!(
        unflip.events.len(),
        2,
        "the unflip replaces both direct outputs"
    );
    for (output_idx, frame) in [(0, 30), (1, 31)] {
        assert_eq!(
            retire_pending_output(&mut fixture.scene.events, &mut delivered, output_idx, frame),
            unflip_id,
            "the composed unflip retires on output {output_idx}",
        );
    }
    assert!(
        !fixture.scene.events.backend.pstate.direct.root().active(),
        "retiring the composed unflip releases direct scanout",
    );
    finish_direct_fixture(fixture, &mut delivered);
    assert!(
        unflip_after_bound,
        "three failed direct cursor attempts request unflip",
    );
    assert_eq!(failed_attempts, attempts_before + 3);
    assert_eq!(
        attempts_after_unflip, failed_attempts,
        "no further per-vblank cursor attempts occur after unflip",
    );
}
