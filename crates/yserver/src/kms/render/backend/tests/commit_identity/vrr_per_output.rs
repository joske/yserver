use super::*;

pub(super) fn set_output_vrr_capable(
    fixture: &mut super::super::recording_scene::RecordingSceneFixture,
    output_idx: usize,
    capable: bool,
) {
    let output = &mut fixture.events.backend.platform.outputs[output_idx].output;
    output.crtc_vrr_enabled_prop = Some(handle(VRR_PROP_ID));
    output.connector_vrr_capable = capable;
    output.vrr_committed.set(false);
    output.vrr_disabled.set(false);
    output.vrr_clear_retry_used.set(false);
}

pub(super) fn two_output_vrr_fixture()
-> Option<super::super::recording_scene::RecordingSceneFixture> {
    let mut fixture = super::super::recording_scene::RecordingSceneFixture::with_route(
        2,
        super::super::recording_scene::P1SceneRoute::Shared,
    )
    .expect("two-output VRR recording scene")?;
    fixture.events.backend.pstate.vrr_mode = crate::launch::VrrMode::Property;
    Some(fixture)
}

pub(super) fn set_present_client(
    direct: &mut super::super::recording_scene::DirectPresentSetup,
    client_id: u32,
) {
    direct.candidate.client_id = client_id;
    direct.event.client_id = yserver_protocol::x11::ClientId(client_id);
}

pub(super) fn retire_output_direct(
    fixture: &mut super::super::recording_scene::RecordingSceneFixture,
    direct: &super::super::recording_scene::DirectPresentSetup,
    record: &AtomicCommitRecord,
    frame: u32,
) {
    let fd = super::super::direct_unflip::fd(fixture);
    super::super::atomic_events::complete_recorded_attempt(
        fixture,
        fd,
        record,
        super::super::support::accepted_event_id(record).expect("direct commit ID"),
        frame,
    );
    let completed = Backend::drain_completed_present_events(&mut fixture.events.backend);
    assert_eq!(
        completed
            .iter()
            .map(|event| event.present_id)
            .collect::<Vec<_>>(),
        [direct.candidate.present_id],
        "the output direct Present completes at its own CRTC"
    );
}

pub(super) fn complete_output_records(
    fixture: &mut super::super::recording_scene::RecordingSceneFixture,
    records: &[AtomicCommitRecord],
    first_frame: u32,
) {
    let fd = super::super::direct_unflip::fd(fixture);
    for (offset, record) in records.iter().enumerate() {
        let Some(id) = super::super::support::accepted_event_id(record) else {
            continue;
        };
        super::super::atomic_events::complete_recorded_attempt(
            fixture,
            fd,
            record,
            id,
            first_frame + u32::try_from(offset).expect("record index fits"),
        );
    }
    let _ = Backend::drain_completed_present_events(&mut fixture.events.backend);
    let _ = Backend::drain_retired_present_idle_events(&mut fixture.events.backend);
}

fn unmap_output_direct(
    fixture: &mut super::super::recording_scene::RecordingSceneFixture,
    direct: &super::super::recording_scene::DirectPresentSetup,
    output_idx: usize,
    first_frame: u32,
) -> Vec<AtomicCommitRecord> {
    let fd = super::super::direct_unflip::fd(fixture);
    let offset = super::super::direct_unflip::record_count(fixture, fd);
    Backend::unmap_subwindow(&mut fixture.events.backend, None, direct.target_xid)
        .expect("unmap output direct owner through Backend");
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("schedule output direct replacement");
    fixture.events.wait_for_submitted_gpu_work();
    let records = super::super::direct_unflip::records_since(fixture, fd, offset);
    assert!(
        records.iter().any(|record| {
            record.events.iter().any(|event| {
                event.crtc
                    == fixture.events.backend.platform.outputs[output_idx]
                        .output
                        .crtc
            }) && super::super::support::accepted_event_id(record).is_some()
        }),
        "output owner exit submits its replacement on the same CRTC"
    );
    complete_output_records(fixture, &records, first_frame);
    records
}

pub(super) fn settle_vrr_cursor_output(
    fixture: &mut super::super::super::cursor_sprites::support::AtomicPathFixture,
    delivered: &mut HashSet<super::super::super::cursor_sprites::support::CompletionKey>,
    output_idx: usize,
) {
    use super::super::super::cursor_sprites::support as cursor_support;

    for _ in 0..4 {
        Backend::maybe_composite(&mut fixture.scene.events.backend)
            .expect("settle the VRR cursor transition");
        for idx in 0..2 {
            cursor_support::drain_unretired_output_events(fixture, delivered, idx);
        }
        if matches!(
            fixture
                .scene
                .events
                .backend
                .scene
                .cursor_mode_for_outputs(&[output_idx]),
            crate::kms::render::scene::CursorPlaneMode::Hw
                | crate::kms::render::scene::CursorPlaneMode::Hidden
        ) {
            break;
        }
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
}

pub(super) fn remember_retired_cursor_records(
    records: &[AtomicCommitRecord],
    delivered: &mut HashSet<super::super::super::cursor_sprites::support::CompletionKey>,
) {
    for record in records {
        let Some(commit_id) = super::super::support::accepted_event_id(record) else {
            continue;
        };
        for event in &record.events {
            delivered.insert(
                super::super::super::cursor_sprites::support::CompletionKey {
                    device_fd: record.device_fd,
                    crtc: event.crtc,
                    commit_id,
                },
            );
        }
    }
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: clearing VRR for one output owner also clears every output.
fn vrr_two_direct_outputs_clear_independently_on_owner_exit() {
    let Some(fixture) = two_output_vrr_fixture() else {
        eprintln!(
            "SKIP vrr_two_direct_outputs_clear_independently_on_owner_exit: Vulkan fixture unavailable"
        );
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    set_output_vrr_capable(&mut fixture, 0, true);
    set_output_vrr_capable(&mut fixture, 1, true);
    let output0 = fixture.install_direct_output_present(0, 20_100, 0);
    let mut output1 = fixture.install_direct_output_present(1, 20_101, 0);
    set_present_client(&mut output1, 2);
    let crtcs = [
        fixture.events.backend.platform.outputs[0].output.crtc,
        fixture.events.backend.platform.outputs[1].output.crtc,
    ];

    let record0 =
        super::super::direct_unflip::submit_output_present_via_backend(&mut fixture, &output0, 0);
    assert!(contains_vrr_property(&record0, crtcs[0], 1));
    assert!(!has_vrr_property(&record0, crtcs[1]));
    retire_output_direct(&mut fixture, &output0, &record0, 101);
    let record1 =
        super::super::direct_unflip::submit_output_present_via_backend(&mut fixture, &output1, 1);
    assert!(contains_vrr_property(&record1, crtcs[1], 1));
    assert!(!has_vrr_property(&record1, crtcs[0]));
    retire_output_direct(&mut fixture, &output1, &record1, 102);
    assert!(
        fixture.events.backend.platform.outputs[0]
            .output
            .vrr_committed
            .get()
    );
    assert!(
        fixture.events.backend.platform.outputs[1]
            .output
            .vrr_committed
            .get()
    );

    let exit_records = unmap_output_direct(&mut fixture, &output1, 1, 103);
    assert!(
        exit_records
            .iter()
            .any(|record| contains_vrr_property(record, crtcs[1], 0)),
        "the exited owner's CRTC receives the VRR clear"
    );
    assert!(
        exit_records
            .iter()
            .all(|record| !has_vrr_property(record, crtcs[0])),
        "the surviving output receives no VRR update"
    );
    assert!(
        !fixture.events.backend.platform.outputs[1]
            .output
            .vrr_committed
            .get()
    );
    assert!(
        fixture.events.backend.platform.outputs[0]
            .output
            .vrr_committed
            .get()
    );
    assert!(
        fixture.events.backend.pstate.direct.outputs[0]
            .current
            .is_some(),
        "the other client's direct owner remains current"
    );

    let _ = unmap_output_direct(&mut fixture, &output0, 0, 104);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: apply the capable output's VRR intent to every output.
fn vrr_incapable_output_never_enables_while_capable_output_does() {
    let Some(fixture) = two_output_vrr_fixture() else {
        eprintln!(
            "SKIP vrr_incapable_output_never_enables_while_capable_output_does: Vulkan fixture unavailable"
        );
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    set_output_vrr_capable(&mut fixture, 0, true);
    set_output_vrr_capable(&mut fixture, 1, false);
    let output0 = fixture.install_direct_output_present(0, 20_110, 0);
    let output1 = fixture.install_direct_output_present(1, 20_111, 0);
    let crtc0 = fixture.events.backend.platform.outputs[0].output.crtc;
    let crtc1 = fixture.events.backend.platform.outputs[1].output.crtc;

    let record0 =
        super::super::direct_unflip::submit_output_present_via_backend(&mut fixture, &output0, 0);
    assert!(contains_vrr_property(&record0, crtc0, 1));
    retire_output_direct(&mut fixture, &output0, &record0, 111);
    let record1 =
        super::super::direct_unflip::submit_output_present_via_backend(&mut fixture, &output1, 1);
    assert!(!has_vrr_property(&record1, crtc1));
    assert!(!contains_vrr_property(&record1, crtc1, 1));
    retire_output_direct(&mut fixture, &output1, &record1, 112);
    assert!(
        fixture.events.backend.platform.outputs[0]
            .output
            .vrr_committed
            .get()
    );
    assert!(
        !fixture.events.backend.platform.outputs[1]
            .output
            .vrr_committed
            .get()
    );

    let _ = unmap_output_direct(&mut fixture, &output1, 1, 113);
    let _ = unmap_output_direct(&mut fixture, &output0, 0, 114);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: copy a direct owner's VRR intent onto scene flips for other outputs.
fn vrr_direct_output_does_not_enable_composed_output() {
    let Some(fixture) = two_output_vrr_fixture() else {
        eprintln!(
            "SKIP vrr_direct_output_does_not_enable_composed_output: Vulkan fixture unavailable"
        );
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    set_output_vrr_capable(&mut fixture, 0, true);
    set_output_vrr_capable(&mut fixture, 1, true);
    let output1 = fixture.install_direct_output_present(1, 20_120, 0);
    let crtc0 = fixture.events.backend.platform.outputs[0].output.crtc;
    let crtc1 = fixture.events.backend.platform.outputs[1].output.crtc;
    let direct_record =
        super::super::direct_unflip::submit_output_present_via_backend(&mut fixture, &output1, 1);
    assert!(contains_vrr_property(&direct_record, crtc1, 1));
    retire_output_direct(&mut fixture, &output1, &direct_record, 121);

    let scene_records = super::super::atomic_events::submit_damaged_frame(&mut fixture)
        .into_iter()
        .map(|(_, record)| record)
        .collect::<Vec<_>>();
    let output0_scene = scene_records.iter().find(|record| {
        matches!(
            record.kind,
            AtomicCommitKind::Scene | AtomicCommitKind::SceneWithCursor
        ) && record.events.iter().any(|event| event.crtc == crtc0)
            && super::super::support::accepted_event_id(record).is_some()
    });
    assert!(
        output0_scene.is_some(),
        "the unowned output continues composing"
    );
    assert!(
        scene_records
            .iter()
            .all(|record| !contains_vrr_property(record, crtc0, 1)),
        "scene work on the composed CRTC never enables VRR"
    );
    complete_output_records(&mut fixture, &scene_records, 122);

    let _ = unmap_output_direct(&mut fixture, &output1, 1, 123);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: a property change on one output clears VRR on every output.
fn vrr_property_delete_is_scoped_to_its_output_client() {
    let Some(fixture) = two_output_vrr_fixture() else {
        eprintln!(
            "SKIP vrr_property_delete_is_scoped_to_its_output_client: Vulkan fixture unavailable"
        );
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    set_output_vrr_capable(&mut fixture, 0, true);
    set_output_vrr_capable(&mut fixture, 1, true);
    let output0 = fixture.install_direct_output_present(0, 20_130, 0);
    let mut output1 = fixture.install_direct_output_present(1, 20_131, 0);
    set_present_client(&mut output1, 2);
    let mut state = present_request_state(&output0);
    add_present_test_client(&mut state, 2);
    add_property_window_for_client(&mut state, 2, output1.target_xid, output1.target_xid);
    let property = state.atoms.intern("_VARIABLE_REFRESH", false).0;
    request_vrr_property(
        &mut state,
        &mut fixture.events.backend,
        output0.target_xid,
        property,
        yserver_core::xinput::XA_CARDINAL.0,
        32,
        &[1],
        1,
    );
    request_vrr_property_for_client(
        &mut state,
        &mut fixture.events.backend,
        2,
        output1.target_xid,
        property,
        yserver_core::xinput::XA_CARDINAL.0,
        32,
        &[1],
        1,
    );
    let crtcs = [
        fixture.events.backend.platform.outputs[0].output.crtc,
        fixture.events.backend.platform.outputs[1].output.crtc,
    ];

    let record0 =
        super::super::direct_unflip::submit_output_present_via_backend(&mut fixture, &output0, 0);
    assert!(contains_vrr_property(&record0, crtcs[0], 1));
    retire_output_direct(&mut fixture, &output0, &record0, 131);
    let record1 =
        super::super::direct_unflip::submit_output_present_via_backend(&mut fixture, &output1, 1);
    assert!(contains_vrr_property(&record1, crtcs[1], 1));
    retire_output_direct(&mut fixture, &output1, &record1, 132);
    assert!(
        fixture.events.backend.platform.outputs[0]
            .output
            .vrr_committed
            .get()
    );
    assert!(
        fixture.events.backend.platform.outputs[1]
            .output
            .vrr_committed
            .get()
    );

    let fd = super::super::direct_unflip::fd(&fixture);
    let offset = super::super::direct_unflip::record_count(&fixture, fd);
    delete_vrr_property_for_client(
        &mut state,
        &mut fixture.events.backend,
        2,
        output1.target_xid,
        property,
        2,
    );
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("schedule the output 1 property clear");
    let clear_records = super::super::direct_unflip::records_since(&fixture, fd, offset);
    assert!(
        clear_records
            .iter()
            .any(|record| contains_vrr_property(record, crtcs[1], 0)),
        "the deleted output client's CRTC clears VRR"
    );
    assert!(
        clear_records
            .iter()
            .all(|record| !has_vrr_property(record, crtcs[0])),
        "the other client's CRTC receives no property update"
    );
    assert!(
        fixture.events.backend.platform.outputs[0]
            .output
            .vrr_committed
            .get()
    );
    assert!(
        !fixture.events.backend.platform.outputs[1]
            .output
            .vrr_committed
            .get()
    );
    assert!(
        fixture.events.backend.pstate.direct.outputs[0]
            .current
            .as_ref()
            .is_some_and(|frame| frame.candidate.variable_refresh)
    );
    assert!(
        fixture.events.backend.pstate.direct.outputs[1]
            .current
            .as_ref()
            .is_some_and(|frame| !frame.candidate.variable_refresh)
    );
    complete_output_records(&mut fixture, &clear_records, 133);

    let _ = unmap_output_direct(&mut fixture, &output1, 1, 134);
    let _ = unmap_output_direct(&mut fixture, &output0, 0, 135);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: use one global VRR cursor hold and deadline for every CRTC.
fn vrr_cursor_deadline_is_independent_per_output_crtc() {
    use super::super::super::cursor_sprites::support as cursor_support;

    let Some(mut fixture) =
        cursor_support::AtomicPathFixture::with_output_count(cursor_support::PathKind::Physical, 2)
            .expect("two-output VRR cursor fixture construction")
    else {
        eprintln!(
            "SKIP vrr_cursor_deadline_is_independent_per_output_crtc: Vulkan fixture unavailable"
        );
        return;
    };
    let mut delivered = HashSet::new();
    let cursor = cursor_support::create_argb_cursor(
        &mut fixture.scene.events.backend,
        (8, 8),
        [0xff, 0xff, 0xff, 0xff],
        (0, 0),
    );
    let x0 = fixture.scene.events.backend.platform.outputs[0].x + 120;
    let x1 = fixture.scene.events.backend.platform.outputs[1].x + 120;
    cursor_support::establish_hardware_cursor_on_output(
        &mut fixture,
        &mut delivered,
        cursor,
        0,
        (x0, 110),
    );
    cursor_support::drain_unretired_output_events(&mut fixture, &mut delivered, 1);
    cursor_support::establish_hardware_cursor_on_output(
        &mut fixture,
        &mut delivered,
        cursor,
        1,
        (x1, 110),
    );
    cursor_support::drain_unretired_output_events(&mut fixture, &mut delivered, 0);
    Backend::warp_pointer_root(
        &mut fixture.scene.events.backend,
        &mut fixture.scene.events.state,
        x0,
        110,
    );
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("settle the pointer on output 0 before direct scanout");
    for output_idx in 0..2 {
        cursor_support::drain_unretired_output_events(&mut fixture, &mut delivered, output_idx);
    }

    fixture.scene.events.backend.pstate.vrr_mode = crate::launch::VrrMode::Property;
    set_output_vrr_capable(&mut fixture.scene, 0, true);
    set_output_vrr_capable(&mut fixture.scene, 1, true);
    let crtc0 = fixture.scene.events.backend.platform.outputs[0].output.crtc;
    let crtc1 = fixture.scene.events.backend.platform.outputs[1].output.crtc;
    let device_fd = fixture.scene.kms_fd(1);
    let (base, elapsed_ns) =
        super::super::support::install_controlled_clock(&mut fixture.scene.events.backend);

    let output0 = fixture.scene.install_direct_output_present(0, 20_140, 0);
    Backend::set_cursor_hidden(&mut fixture.scene.events.backend, false);
    settle_vrr_cursor_output(&mut fixture, &mut delivered, 0);
    let direct_record0 = super::super::direct_unflip::submit_output_present_via_backend(
        &mut fixture.scene,
        &output0,
        0,
    );
    assert!(contains_vrr_property(&direct_record0, crtc0, 1));
    retire_output_direct(&mut fixture.scene, &output0, &direct_record0, 140);
    remember_retired_cursor_records(&[direct_record0], &mut delivered);
    cursor_support::drain_unretired_output_events(&mut fixture, &mut delivered, 0);

    let mut output1 = fixture.scene.install_direct_output_present(1, 20_141, 0);
    output1.candidate.variable_refresh = false;
    Backend::set_cursor_hidden(&mut fixture.scene.events.backend, false);
    settle_vrr_cursor_output(&mut fixture, &mut delivered, 1);
    let direct_record = super::super::direct_unflip::submit_output_present_via_backend(
        &mut fixture.scene,
        &output1,
        1,
    );
    assert!(!contains_vrr_property(&direct_record, crtc1, 1));
    retire_output_direct(&mut fixture.scene, &output1, &direct_record, 142);
    remember_retired_cursor_records(&[direct_record], &mut delivered);
    cursor_support::drain_unretired_output_events(&mut fixture, &mut delivered, 1);
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
        fixture.scene.events.backend.platform.outputs[0]
            .output
            .vrr_committed
            .get(),
        "output 0 owns a committed VRR frame"
    );
    assert!(
        !fixture.scene.events.backend.platform.outputs[1]
            .output
            .vrr_committed
            .get()
    );

    let start = super::super::direct_unflip::record_count(&fixture.scene, device_fd);
    Backend::warp_pointer_root(
        &mut fixture.scene.events.backend,
        &mut fixture.scene.events.state,
        x0 + 5,
        120,
    );
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("start output 0's independent 33 ms cursor deadline");
    let initial_cursor_records =
        super::super::direct_unflip::records_since(&fixture.scene, device_fd, start);
    assert!(
        initial_cursor_records.iter().any(|record| {
            record.kind == AtomicCommitKind::CursorOnly
                && record.result == ScriptedResult::Ok
                && record.events.iter().any(|event| event.crtc == crtc1)
                && super::super::support::accepted_event_id(record).is_some()
        }),
        "output 1 commits its cursor while its direct frame still has VRR off"
    );
    assert!(
        initial_cursor_records.iter().all(|record| {
            record.kind != AtomicCommitKind::CursorOnly
                || !record.events.iter().any(|event| event.crtc == crtc0)
        }),
        "output 0's VRR cursor remains held until its own deadline"
    );
    cursor_support::retire_pending_output(&mut fixture.scene.events, &mut delivered, 1, 143);
    assert_eq!(
        Backend::next_wakeup(&fixture.scene.events.backend),
        Some(base + Duration::from_millis(33)),
        "output 0 starts its own cursor deadline"
    );

    advance_vrr_cursor_clock(&elapsed_ns, Duration::from_millis(10));
    output1.candidate.variable_refresh = true;
    let vrr_enable_record =
        super::submit_direct_successor(&mut fixture.scene, &mut output1, device_fd);
    assert!(contains_vrr_property(&vrr_enable_record, crtc1, 1));
    retire_output_direct(&mut fixture.scene, &output1, &vrr_enable_record, 144);
    remember_retired_cursor_records(&[vrr_enable_record], &mut delivered);
    assert!(
        fixture.scene.events.backend.platform.outputs[1]
            .output
            .vrr_committed
            .get()
    );

    let cursor_start = super::super::direct_unflip::record_count(&fixture.scene, device_fd);
    Backend::warp_pointer_root(
        &mut fixture.scene.events.backend,
        &mut fixture.scene.events.state,
        x1,
        130,
    );
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("hold output 1 cursor motion without resetting output 0's deadline");
    let records =
        super::super::direct_unflip::records_since(&fixture.scene, device_fd, cursor_start);
    assert!(
        records.iter().all(|record| {
            record.kind != AtomicCommitKind::CursorOnly
                || super::super::support::accepted_event_id(record).is_none()
        }),
        "both output cursor moves remain held before their own deadlines; records={records:?}"
    );
    assert_eq!(
        Backend::next_wakeup(&fixture.scene.events.backend),
        Some(base + Duration::from_millis(33)),
        "output 1's later motion does not reset output 0's 33 ms deadline"
    );

    advance_vrr_cursor_clock(&elapsed_ns, Duration::from_millis(23));
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("service output 0's deadline while output 1 remains held");
    let output0_deadline_records =
        super::super::direct_unflip::records_since(&fixture.scene, device_fd, cursor_start)
            .into_iter()
            .filter(|record| {
                record.kind == AtomicCommitKind::CursorOnly
                    && record.result == ScriptedResult::Ok
                    && record.events.iter().any(|event| event.crtc == crtc0)
                    && super::super::support::accepted_event_id(record).is_some()
            })
            .collect::<Vec<_>>();
    assert_eq!(
        output0_deadline_records.len(),
        1,
        "output 0 commits at its original deadline despite output 1 motion"
    );
    assert!(
        super::super::direct_unflip::records_since(&fixture.scene, device_fd, cursor_start)
            .iter()
            .all(|record| {
                record.kind != AtomicCommitKind::CursorOnly
                    || !record.events.iter().any(|event| event.crtc == crtc1)
            }),
        "output 1 remains held until 33 ms after its own motion; records={:?}; next_wakeup={:?}",
        super::super::direct_unflip::records_since(&fixture.scene, device_fd, cursor_start),
        Backend::next_wakeup(&fixture.scene.events.backend)
    );
    cursor_support::retire_pending_output(&mut fixture.scene.events, &mut delivered, 0, 145);
    assert_eq!(
        Backend::next_wakeup(&fixture.scene.events.backend),
        Some(base + Duration::from_millis(43)),
        "output 1 retains its own deadline 33 ms after its later motion"
    );

    advance_vrr_cursor_clock(&elapsed_ns, Duration::from_millis(9));
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("output 1 remains held one millisecond before its deadline");
    assert!(
        super::super::direct_unflip::records_since(&fixture.scene, device_fd, cursor_start)
            .iter()
            .all(|record| {
                record.kind != AtomicCommitKind::CursorOnly
                    || !record.events.iter().any(|event| event.crtc == crtc1)
            }),
        "output 1 has no cursor-only commit before its own 33 ms deadline"
    );
    advance_vrr_cursor_clock(&elapsed_ns, Duration::from_millis(1));
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("service output 1's deadline");
    let deadline_records =
        super::super::direct_unflip::records_since(&fixture.scene, device_fd, cursor_start)
            .into_iter()
            .filter(|record| {
                record.kind == AtomicCommitKind::CursorOnly
                    && record.result == ScriptedResult::Ok
                    && record.events.iter().any(|event| event.crtc == crtc1)
                    && super::super::support::accepted_event_id(record).is_some()
            })
            .collect::<Vec<_>>();
    assert_eq!(
        deadline_records.len(),
        1,
        "output 1's pending cursor position commits once when its own deadline expires"
    );
    cursor_support::retire_pending_output(&mut fixture.scene.events, &mut delivered, 1, 146);

    let output0_replacement = unmap_output_direct(&mut fixture.scene, &output0, 0, 147);
    remember_retired_cursor_records(&output0_replacement, &mut delivered);
    let output1_replacement = unmap_output_direct(&mut fixture.scene, &output1, 1, 149);
    remember_retired_cursor_records(&output1_replacement, &mut delivered);
    Backend::set_cursor_hidden(&mut fixture.scene.events.backend, true);
    for output_idx in 0..2 {
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
                .expect("detach test cursor during fixture cleanup");
        }
    }
    for output_idx in 0..2 {
        cursor_support::drain_unretired_output_events(&mut fixture, &mut delivered, output_idx);
    }
    cursor_support::finish_atomic_fixture(fixture, &mut delivered);
}
