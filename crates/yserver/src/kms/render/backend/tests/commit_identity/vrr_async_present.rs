use super::*;

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
fn future_target_async_present_on_vrr_window_keeps_vrr() {
    use std::collections::HashSet;

    let async_option = yserver_core::present_scheduler::PRESENT_OPTION_ASYNC;
    let Some((fixture, direct)) =
        direct_fixture_with_vrr_mode(18_250, 0, crate::launch::VrrMode::Property)
    else {
        eprintln!("SKIP future_target_async_present_on_vrr_window_keeps_vrr: no Vulkan fixture");
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    for device in &mut fixture.events.backend.platform.devices {
        device.async_page_flip = true;
    }
    set_vrr_capable(&mut fixture);

    let fd = super::super::direct_unflip::fd(&fixture);
    let crtc = fixture.events.backend.platform.outputs[0].output.crtc;
    let crtc_id = u32::from(crtc);
    let mut state = present_request_state(&direct);
    let window = direct.target_xid;
    let pixmap = direct.candidate.src_pixmap_xid;
    let property = state.atoms.intern("_VARIABLE_REFRESH", false).0;

    // A real retired scene frame establishes the general clock used by each
    // dispatched Present target. The test never injects a completion sample.
    let initial_scene = super::super::atomic_events::submit_damaged_frame(&mut fixture)
        .into_iter()
        .find(|(device_fd, record)| {
            *device_fd == fd
                && record.events.iter().any(|event| event.crtc == crtc)
                && super::super::support::accepted_event_id(record).is_some()
        })
        .map(|(_, record)| record)
        .expect("initial composed scene commit is accepted");
    super::super::atomic_events::complete_recorded_attempt(
        &mut fixture,
        fd,
        &initial_scene,
        super::super::support::accepted_event_id(&initial_scene).expect("scene commit ID"),
        50,
    );
    prepare_property_present_source(&mut fixture, &direct);
    let mut expected_core_gates = HashSet::new();
    let mut previous_direct_present_id = None;
    let mut sequence = 2_u16;
    let mut serial = 18_251_u32;

    // Current-target requests retain Async and build the existing tearing
    // probation through the actual PresentPixmap request dispatcher.
    fixture
        .events
        .backend
        .pstate
        .direct
        .root_mut()
        .eligible_streak = crate::kms::render::backend::SCANOUT_M2_ELIGIBLE_ROOT_PROBATION - 1;
    let warmup_limit = usize::from(crate::kms::render::backend::TEARING_ENTRY_PROBATION) + 2;
    let mut admitted_async = false;
    for _ in 0..warmup_limit {
        let current_msc = Backend::present_get_ust_msc(&fixture.events.backend, crtc_id).0;
        assert!(current_msc > 0, "the request has a current CRTC MSC");
        let before_gates: HashSet<_> = state.present_complete_gate.keys().copied().collect();
        let offset = super::super::direct_unflip::record_count(&fixture, fd);
        request_present_with_options(
            &mut state,
            &mut fixture.events.backend,
            window,
            pixmap,
            serial,
            async_option,
            current_msc,
            sequence,
        );
        let async_record = super::super::direct_unflip::records_since(&fixture, fd, offset)
            .into_iter()
            .find(|record| {
                super::super::support::accepted_event_id(record).is_some()
                    && record.events.iter().any(|event| event.crtc == crtc)
            })
            .expect("current-target Present submits without waiting");
        assert!(
            matches!(
                async_record.kind,
                AtomicCommitKind::Scene | AtomicCommitKind::Async
            ),
            "warm-up Present is accepted by the direct owner"
        );
        assert!(!has_vrr_property(&async_record, crtc));
        let committed_async = async_record.kind == AtomicCommitKind::Async;
        let event_id =
            super::super::support::accepted_event_id(&async_record).expect("async event ID");
        super::super::atomic_events::complete_recorded_attempt(
            &mut fixture,
            fd,
            &async_record,
            event_id,
            u32::try_from(current_msc + 1).expect("retirement MSC fits"),
        );
        let completions = Backend::drain_completed_present_events(&mut fixture.events.backend);
        assert_eq!(completions.len(), 1, "one current-target Present retires");
        assert_eq!(completions[0].serial, serial);
        let idled = Backend::drain_retired_present_idle_events(&mut fixture.events.backend);
        assert_eq!(
            idled
                .iter()
                .map(|event| event.present_id)
                .collect::<Vec<_>>(),
            previous_direct_present_id.into_iter().collect::<Vec<_>>(),
            "each retirement idles only the previous direct Present"
        );
        let gate_ids: Vec<_> = state
            .present_complete_gate
            .keys()
            .filter(|present_id| !before_gates.contains(present_id))
            .copied()
            .collect();
        assert_eq!(gate_ids, vec![completions[0].present_id]);
        expected_core_gates.insert(completions[0].present_id);
        previous_direct_present_id = Some(completions[0].present_id);
        serial = serial.wrapping_add(1);
        sequence = sequence.wrapping_add(1);
        if committed_async {
            admitted_async = true;
            break;
        }
    }
    assert!(admitted_async, "tearing probation records an Async commit");
    assert_eq!(
        fixture.events.backend.tearing_scope(),
        Some(crate::kms::render::backend::DirectScope::Root),
        "current-target Async requests admit tearing before the property change"
    );

    request_vrr_property(
        &mut state,
        &mut fixture.events.backend,
        window,
        property,
        yserver_core::xinput::XA_CARDINAL.0,
        32,
        &[1],
        sequence,
    );
    assert!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .current
            .as_ref()
            .is_some_and(|frame| frame.candidate.variable_refresh),
        "ChangeProperty dispatch updates VRR permission on the live window"
    );
    sequence = sequence.wrapping_add(1);

    for enable_vrr in [true, false] {
        let current_msc = Backend::present_get_ust_msc(&fixture.events.backend, crtc_id).0;
        let target_msc = current_msc + 1;
        let before_gates: HashSet<_> = state.present_complete_gate.keys().copied().collect();
        let offset = super::super::direct_unflip::record_count(&fixture, fd);
        request_present_with_options(
            &mut state,
            &mut fixture.events.backend,
            window,
            pixmap,
            serial,
            async_option,
            target_msc,
            sequence,
        );
        let record = super::super::direct_unflip::records_since(&fixture, fd, offset)
            .into_iter()
            .find(|record| {
                super::super::support::accepted_event_id(record).is_some()
                    && record.events.iter().any(|event| event.crtc == crtc)
            })
            .expect("future-target request submits immediately when ready");
        assert_ne!(record.kind, AtomicCommitKind::Async);
        assert_eq!(
            contains_vrr_property(&record, crtc, 1),
            enable_vrr,
            "VRR_ENABLED is emitted on the first synced enable only"
        );
        assert!(
            fixture.events.backend.platform.outputs[0]
                .output
                .vrr_committed
                .get(),
            "a synced future-target candidate retains committed VRR"
        );
        assert_eq!(
            fixture.events.backend.tearing_scope(),
            None,
            "the future-target synced candidate exits tearing"
        );
        let event_id = super::super::support::accepted_event_id(&record).expect("synced event ID");
        super::super::atomic_events::complete_recorded_attempt(
            &mut fixture,
            fd,
            &record,
            event_id,
            u32::try_from(target_msc).expect("target MSC fits"),
        );
        assert_eq!(
            Backend::present_get_ust_msc(&fixture.events.backend, crtc_id).0,
            target_msc,
            "the synced flip retires on its requested MSC"
        );
        let completions = Backend::drain_completed_present_events(&mut fixture.events.backend);
        assert_eq!(completions.len(), 1, "one future-target Present retires");
        assert_eq!(completions[0].serial, serial);
        let idled = Backend::drain_retired_present_idle_events(&mut fixture.events.backend);
        assert_eq!(
            idled
                .iter()
                .map(|event| event.present_id)
                .collect::<Vec<_>>(),
            previous_direct_present_id.into_iter().collect::<Vec<_>>(),
            "each retirement idles only the previous direct Present"
        );
        assert_eq!(
            completions[0]
                .completion_clock
                .expect("direct retirement carries its clock")
                .msc,
            target_msc
        );
        let gate_ids: Vec<_> = state
            .present_complete_gate
            .keys()
            .filter(|present_id| !before_gates.contains(present_id))
            .copied()
            .collect();
        assert_eq!(gate_ids, vec![completions[0].present_id]);
        expected_core_gates.insert(completions[0].present_id);
        previous_direct_present_id = Some(completions[0].present_id);
        serial = serial.wrapping_add(1);
        sequence = sequence.wrapping_add(1);
    }

    let current = fixture
        .events
        .backend
        .pstate
        .direct
        .root()
        .current
        .as_ref()
        .expect("last direct frame remains held until unflip");
    let live_direct = super::super::recording_scene::DirectPresentSetup {
        candidate: current.candidate,
        event: current.event.clone(),
        source_id: current.source_id,
        target_xid: current.candidate.dst_window_xid,
    };
    finish_direct_scanout(&mut fixture, &live_direct, fd);
    assert!(
        Backend::drain_completed_present_events(&mut fixture.events.backend).is_empty(),
        "backend-held completions are drained after the unflip"
    );
    assert!(state.present_pending_exec.is_empty());
    assert!(state.present_pending_complete.is_empty());
    assert_eq!(
        state
            .present_complete_gate
            .keys()
            .copied()
            .collect::<HashSet<_>>(),
        expected_core_gates,
        "only direct completions awaiting the core sweep retain gates"
    );
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
fn future_target_async_present_not_direct_eligible_composes_vsync() {
    use yserver_core::backend::Backend;
    use yserver_protocol::x11::{ClientId, RequestHeader, SequenceNumber};

    let async_option = yserver_core::present_scheduler::PRESENT_OPTION_ASYNC;
    let Some((fixture, direct)) = direct_fixture(18_260, 0) else {
        eprintln!(
            "SKIP future_target_async_present_not_direct_eligible_composes_vsync: no Vulkan fixture"
        );
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    let fd = super::super::direct_unflip::fd(&fixture);
    let crtc = fixture.events.backend.platform.outputs[0].output.crtc;
    let crtc_id = u32::from(crtc);
    let window = direct.target_xid;
    let pixmap = direct.candidate.src_pixmap_xid;
    let mut state = present_request_state(&direct);

    let initial_scene = super::super::atomic_events::submit_damaged_frame(&mut fixture)
        .into_iter()
        .find(|(device_fd, record)| {
            *device_fd == fd
                && record.events.iter().any(|event| event.crtc == crtc)
                && super::super::support::accepted_event_id(record).is_some()
        })
        .map(|(_, record)| record)
        .expect("initial composed scene commit is accepted");
    super::super::atomic_events::complete_recorded_attempt(
        &mut fixture,
        fd,
        &initial_scene,
        super::super::support::accepted_event_id(&initial_scene).expect("scene commit ID"),
        60,
    );
    prepare_property_present_source(&mut fixture, &direct);
    let current_msc = Backend::present_get_ust_msc(&fixture.events.backend, crtc_id).0;
    assert!(current_msc > 0);

    // A small mapped top-level window overlaps the direct candidate, which
    // forces the normal Present Copy fallback. Create and map it through the
    // request dispatcher so the fixture follows the client path.
    let overlap_xid = 0x6f00_0001_u32;
    let mut create_body = Vec::with_capacity(28);
    create_body.extend_from_slice(&overlap_xid.to_le_bytes());
    create_body.extend_from_slice(&yserver_core::resources::ROOT_WINDOW.0.to_le_bytes());
    create_body.extend_from_slice(&0_i16.to_le_bytes());
    create_body.extend_from_slice(&0_i16.to_le_bytes());
    create_body.extend_from_slice(&100_u16.to_le_bytes());
    create_body.extend_from_slice(&100_u16.to_le_bytes());
    create_body.extend_from_slice(&0_u16.to_le_bytes());
    create_body.extend_from_slice(&1_u16.to_le_bytes());
    create_body.extend_from_slice(&yserver_core::resources::ROOT_VISUAL.0.to_le_bytes());
    create_body.extend_from_slice(&0_u32.to_le_bytes());
    yserver_core::core_loop::process_request::process_request(
        &mut state,
        &mut fixture.events.backend,
        ClientId(1),
        SequenceNumber(2),
        RequestHeader {
            opcode: 1,
            data: 24,
            length_units: 8,
        },
        &create_body,
        None,
    )
    .expect("CreateWindow dispatch for overlap");
    yserver_core::core_loop::process_request::process_request(
        &mut state,
        &mut fixture.events.backend,
        ClientId(1),
        SequenceNumber(3),
        RequestHeader {
            opcode: 8,
            data: 0,
            length_units: 2,
        },
        &overlap_xid.to_le_bytes(),
        None,
    )
    .expect("MapWindow dispatch for overlap");
    let (marker_x, marker_y) = (200_i16, 200_i16);
    Backend::put_image(
        &mut fixture.events.backend,
        None,
        pixmap,
        24,
        1,
        1,
        marker_x,
        marker_y,
        &[0x51, 0x82, 0xb3, 0],
    )
    .expect("write the unique source marker through Backend");
    let source_marker = Backend::get_image(
        &mut fixture.events.backend,
        None,
        pixmap,
        2,
        marker_x,
        marker_y,
        1,
        1,
        0x00ff_ffff,
    )
    .expect("read back the source marker")
    .expect("source marker is available");
    assert_eq!(source_marker.len(), 36, "one 24-bit pixel reply");

    let serial = 18_261;
    let sequence = 4;
    let target_msc = current_msc + 1;
    let before_request = super::super::direct_unflip::record_count(&fixture, fd);
    request_present_with_options(
        &mut state,
        &mut fixture.events.backend,
        window,
        pixmap,
        serial,
        async_option,
        target_msc,
        sequence,
    );
    assert!(
        super::super::direct_unflip::records_since(&fixture, fd, before_request)
            .iter()
            .all(|record| super::super::support::accepted_event_id(record).is_none()),
        "the overlapped window declines direct scanout and reaches the Copy path"
    );
    assert!(state.present_pending_exec.is_empty());
    assert!(state.present_pending_complete.is_empty());
    let copied = Backend::drain_completed_present_events(&mut fixture.events.backend);
    assert_eq!(
        copied.len(),
        1,
        "the Copy fallback completes this Present once"
    );
    assert_eq!(
        copied[0].serial, serial,
        "Copy completion identifies the request"
    );
    assert_eq!(
        state
            .present_complete_gate
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        vec![copied[0].present_id],
        "the Copy completion retains only its expected gate for the core sweep"
    );
    assert_eq!(
        copied[0].completion_mode,
        yserver_protocol::x11::present::COMPLETE_MODE_COPY
    );
    assert_eq!(copied[0].options & async_option, async_option);

    let scene_start = super::super::direct_unflip::record_count(&fixture, fd);
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("submit the composed scene carrying the Present copy");
    fixture.events.wait_for_submitted_gpu_work();
    let scene_records = super::super::direct_unflip::records_since(&fixture, fd, scene_start);
    let scene_record = scene_records
        .iter()
        .find(|record| {
            super::super::support::accepted_event_id(record).is_some()
                && record.events.iter().any(|event| event.crtc == crtc)
        })
        .expect("a composed commit on the Present CRTC carries the copied region");
    assert!(
        matches!(
            scene_record.kind,
            AtomicCommitKind::Scene | AtomicCommitKind::SceneWithCursor
        ),
        "the commit caused by serial {serial} is a composed scene commit"
    );
    assert_ne!(scene_record.kind, AtomicCommitKind::Async);

    let target_marker = Backend::get_image(
        &mut fixture.events.backend,
        None,
        window,
        2,
        marker_x,
        marker_y,
        1,
        1,
        0x00ff_ffff,
    )
    .expect("read the copied destination marker")
    .expect("destination marker is available");
    assert_eq!(
        &target_marker[32..36],
        &source_marker[32..36],
        "Present serial {serial} copied its unique source region into the scene target"
    );

    super::super::atomic_events::complete_recorded_attempt(
        &mut fixture,
        fd,
        scene_record,
        super::super::support::accepted_event_id(scene_record).expect("scene commit ID"),
        u32::try_from(target_msc).expect("target MSC fits"),
    );
    let visible_marker = crate::kms::render::backend::read_scanout_region(
        &mut fixture.events.backend,
        ash::vk::Rect2D {
            offset: ash::vk::Offset2D {
                x: i32::from(marker_x),
                y: i32::from(marker_y),
            },
            extent: ash::vk::Extent2D {
                width: 1,
                height: 1,
            },
        },
        crate::kms::render::backend::ScanoutReadSelection::PermissiveDump,
    )
    .expect("read the composed scanout marker");
    assert_eq!(
        &visible_marker[..4],
        &source_marker[32..36],
        "the scene commit made this Present's copied region visible"
    );
    assert!(
        Backend::drain_completed_present_events(&mut fixture.events.backend).is_empty(),
        "the scene retirement has no unrelated Present completion"
    );
    assert!(fixture.events.backend.present_source_pins.is_empty());
    fixture.finish_after_normal_drain();
    assert!(state.present_pending_exec.is_empty());
    assert!(state.present_pending_complete.is_empty());
    assert_eq!(
        state
            .present_complete_gate
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        vec![copied[0].present_id],
        "the copied Present remains the one expected core completion gate"
    );
}
