use super::{
    super::{atomic_events, direct_unflip, recording_scene, support},
    *,
};
use crate::drm::atomic::{AtomicCommitKind, tests_support::AtomicCommitRecord};
use std::{
    collections::HashSet,
    sync::{Arc, atomic::AtomicU64},
    time::{Duration, Instant},
};
use yserver_core::backend::Backend;

const IDLE: Duration = Duration::from_millis(100);

fn present_record(harness: &VrrCursorHarness, present_id: u64) -> AtomicCommitRecord {
    let owner = harness.scene.events.backend.pstate.direct.root();
    let frame = [owner.pending.as_ref(), owner.current.as_ref()]
        .into_iter()
        .flatten()
        .find(|frame| frame.candidate.present_id == present_id)
        .unwrap_or_else(|| panic!("direct Present {present_id} is not in the owner slot"));
    let id = frame
        .accepted_event_id
        .expect("accepted direct Present has an event ID");
    super::vrr_cursor_records_since(harness, 0)
        .into_iter()
        .find(|record| support::accepted_event_id(record) == Some(id))
        .expect("accepted direct Present has a recorded request")
}

fn complete_record(
    harness: &mut VrrCursorHarness,
    record: &AtomicCommitRecord,
    frame: u32,
    event_at: Duration,
) {
    let id = support::accepted_event_id(record).expect("accepted commit event ID");
    let device_fd = harness.device_fd;
    let fixture = harness.fixture.as_mut().expect("cursor fixture is live");
    fixture.scene.events.wait_for_submitted_gpu_work();
    for event in &record.events {
        fixture.scene.events.queue_commit_completion_at(
            device_fd,
            event.crtc,
            id,
            frame,
            Duration::from_millis(u64::from(frame)),
            event_at,
        );
    }
    Backend::on_page_flip_ready(
        &mut fixture.scene.events.backend,
        &mut fixture.scene.events.state,
        device_fd,
    );
    for event in &record.events {
        harness.delivered.insert(
            super::super::super::cursor_sprites::support::CompletionKey {
                device_fd,
                crtc: event.crtc,
                commit_id: id,
            },
        );
    }
}

fn retire_present(
    harness: &mut VrrCursorHarness,
    record: &AtomicCommitRecord,
    completed: &[u64],
    idled: &[u64],
    frame: u32,
) {
    complete_record(harness, record, frame, Duration::ZERO);
    direct_unflip::drain_present_notifications(&mut harness.scene, completed, idled);
}

fn accepted_cursor_records(records: &[AtomicCommitRecord]) -> Vec<AtomicCommitRecord> {
    records
        .iter()
        .filter(|record| {
            record.kind == AtomicCommitKind::CursorOnly
                && support::accepted_event_id(record).is_some()
        })
        .cloned()
        .collect()
}

fn assert_record_cursor_position_output(
    backend: &KmsBackend,
    record: &AtomicCommitRecord,
    output_idx: usize,
    expected_root: (i32, i32),
) {
    let layout = &backend.platform.outputs[output_idx];
    let properties = super::super::super::cursor_sprites::support::cursor_properties_for_output(
        backend, output_idx,
    );
    let expected_x = expected_root.0 - layout.x;
    let expected_y = expected_root.1 - layout.y;
    assert!(record.properties.iter().any(|update| {
        update.object == properties.plane.into()
            && update.property == properties.crtc_x
            && update.value == i64::from(expected_x) as u64
    }));
    assert!(record.properties.iter().any(|update| {
        update.object == properties.plane.into()
            && update.property == properties.crtc_y
            && update.value == i64::from(expected_y) as u64
    }));
}

fn advance_clock(elapsed_ns: &Arc<AtomicU64>, duration: Duration) {
    super::advance_vrr_cursor_clock(elapsed_ns, duration);
}

fn submit_and_retire_root_successor(
    harness: &mut VrrCursorHarness,
    elapsed_ns: &Arc<AtomicU64>,
    duration: Duration,
    frame_number: u32,
) -> AtomicCommitRecord {
    advance_clock(elapsed_ns, duration);
    let mut direct = harness.direct.take().expect("direct scanout is active");
    let previous_present = direct.candidate.present_id;
    let device_fd = harness.device_fd;
    let record = super::submit_direct_successor(&mut harness.scene, &mut direct, device_fd);
    let present_id = direct.candidate.present_id;
    retire_present(
        harness,
        &record,
        &[present_id],
        &[previous_present],
        frame_number,
    );
    harness.direct = Some(direct);
    record
}

fn root_harness(present_id: u64, test: &str) -> Option<VrrCursorHarness> {
    let harness = super::vrr_cursor_harness(present_id, true);
    if harness.is_none() {
        eprintln!("SKIP {test}: no Vulkan scene fixture available");
    }
    harness
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: ignore idle in the hold.
fn vrr_cursor_hold_lifts_after_idle() {
    let Some(mut harness) = root_harness(28_001, "vrr_cursor_hold_lifts_after_idle") else {
        return;
    };
    let (_, elapsed_ns) = harness.controlled_clock();
    let offset = direct_unflip::record_count(&harness.scene, harness.device_fd);
    advance_clock(&elapsed_ns, IDLE);
    harness.warp_pointer(180, 130);
    Backend::maybe_composite(&mut harness.scene.events.backend)
        .expect("schedule an idle cursor move immediately");
    let records = super::vrr_cursor_records_since(&harness, offset);
    let cursor = accepted_cursor_records(&records);
    assert_eq!(
        cursor.len(),
        1,
        "one cursor-only commit follows the idle boundary"
    );
    assert_record_cursor_position(&harness, &cursor[0], (180, 130));
    harness.retire_cursor_move(101);
    harness.finish();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: lift the hold always.
fn vrr_cursor_hold_applies_while_presenting() {
    let Some(mut harness) = root_harness(28_002, "vrr_cursor_hold_applies_while_presenting") else {
        return;
    };
    let (_, elapsed_ns) = harness.controlled_clock();
    let offset = direct_unflip::record_count(&harness.scene, harness.device_fd);
    submit_and_retire_root_successor(&mut harness, &elapsed_ns, Duration::from_millis(16), 102);
    advance_clock(&elapsed_ns, Duration::from_millis(1));
    harness.warp_pointer(180, 130);
    Backend::maybe_composite(&mut harness.scene.events.backend)
        .expect("hold motion between recent direct Presents");
    assert!(
        accepted_cursor_records(&super::vrr_cursor_records_since(&harness, offset)).is_empty(),
        "a fresh owner does not get a cursor-only request"
    );
    let frame =
        submit_and_retire_root_successor(&mut harness, &elapsed_ns, Duration::from_millis(15), 103);
    assert_record_cursor_position(&harness, &frame, (180, 130));
    assert!(
        accepted_cursor_records(&super::vrr_cursor_records_since(&harness, offset)).is_empty(),
        "the move rides the next direct frame"
    );
    harness.finish();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: `>` instead of `>=`.
fn vrr_cursor_hold_idle_boundary() {
    let Some(mut harness) = root_harness(28_003, "vrr_cursor_hold_idle_boundary") else {
        return;
    };
    let (base, elapsed_ns) = harness.controlled_clock();
    let offset = direct_unflip::record_count(&harness.scene, harness.device_fd);
    advance_clock(&elapsed_ns, IDLE - Duration::from_nanos(1));
    harness.warp_pointer(180, 130);
    Backend::maybe_composite(&mut harness.scene.events.backend)
        .expect("hold a move one nanosecond before idle");
    assert!(
        accepted_cursor_records(&super::vrr_cursor_records_since(&harness, offset)).is_empty(),
        "the move stays held at last Present + 99.999 ms"
    );
    assert_eq!(
        Backend::next_wakeup(&harness.scene.events.backend),
        Some(base + IDLE + Duration::from_millis(33) - Duration::from_nanos(1)),
        "the pending move retains its existing 33 ms deadline before the boundary"
    );
    advance_clock(&elapsed_ns, Duration::from_nanos(1));
    assert_eq!(
        Backend::next_wakeup(&harness.scene.events.backend),
        Some(base + IDLE),
        "the scheduler becomes immediate at exact idle equality"
    );
    Backend::maybe_composite(&mut harness.scene.events.backend)
        .expect("submit the pending move at the exact boundary");
    let cursor = accepted_cursor_records(&super::vrr_cursor_records_since(&harness, offset));
    assert_eq!(cursor.len(), 1, "one cursor-only request crosses equality");
    assert_record_cursor_position(&harness, &cursor[0], (180, 130));
    harness.retire_cursor_move(104);
    harness.finish();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: record the time only once per owner.
fn vrr_cursor_hold_returns_on_next_present() {
    let Some(mut harness) = root_harness(28_004, "vrr_cursor_hold_returns_on_next_present") else {
        return;
    };
    let (base, elapsed_ns) = harness.controlled_clock();
    let offset = direct_unflip::record_count(&harness.scene, harness.device_fd);
    advance_clock(&elapsed_ns, IDLE + Duration::from_millis(1));
    harness.warp_pointer(180, 130);
    Backend::maybe_composite(&mut harness.scene.events.backend)
        .expect("an idle move commits without a Present");
    let cursor = accepted_cursor_records(&super::vrr_cursor_records_since(&harness, offset));
    assert_eq!(cursor.len(), 1, "idle owner releases its cursor move");
    harness.retire_cursor_move(105);

    let frame =
        submit_and_retire_root_successor(&mut harness, &elapsed_ns, Duration::from_millis(19), 106);
    assert!(support::accepted_event_id(&frame).is_some());
    advance_clock(&elapsed_ns, Duration::from_millis(5));
    let next_move_offset = direct_unflip::record_count(&harness.scene, harness.device_fd);
    harness.warp_pointer(190, 140);
    Backend::maybe_composite(&mut harness.scene.events.backend)
        .expect("the next Present restores the cursor hold");
    assert!(
        accepted_cursor_records(&super::vrr_cursor_records_since(&harness, next_move_offset))
            .is_empty(),
        "a move five milliseconds after the new Present stays held"
    );
    assert_eq!(
        Backend::next_wakeup(&harness.scene.events.backend),
        Some(base + Duration::from_millis(158)),
        "the restored hold uses the existing 33 ms cursor deadline"
    );
    harness.finish();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: stamp the time at dispatch.
fn vrr_cursor_hold_queued_dispatch_does_not_renew() {
    let Some(mut harness) = root_harness(28_005, "vrr_cursor_hold_queued_dispatch_does_not_renew")
    else {
        return;
    };
    let (base, elapsed_ns) = harness.controlled_clock();
    let mut direct = harness.direct.take().expect("direct scanout is active");
    let initial_id = direct.candidate.present_id;
    let device_fd = harness.device_fd;
    let _first = super::submit_direct_successor(&mut harness.scene, &mut direct, device_fd);
    let first_id = direct.candidate.present_id;
    advance_clock(&elapsed_ns, Duration::from_millis(90));
    super::queue_direct_successor(&mut harness.scene, &mut direct);
    let queued_id = direct.candidate.present_id;
    assert!(
        harness
            .scene
            .events
            .backend
            .pstate
            .direct
            .root()
            .queued_successor
            .is_some(),
        "the accepted successor remains queued behind its predecessor"
    );

    advance_clock(&elapsed_ns, Duration::from_millis(100));
    let first = present_record(&harness, first_id);
    let dispatch_offset = direct_unflip::record_count(&harness.scene, harness.device_fd);
    retire_present(&mut harness, &first, &[first_id], &[initial_id], 107);
    let dispatched = present_record(&harness, queued_id);
    assert!(
        direct_unflip::records_since(&harness.scene, harness.device_fd, dispatch_offset)
            .iter()
            .any(|record| support::accepted_event_id(record).is_some()),
        "retiring the predecessor dispatches the queued Present"
    );
    retire_present(&mut harness, &dispatched, &[queued_id], &[first_id], 108);
    advance_clock(&elapsed_ns, Duration::from_millis(1));
    harness.warp_pointer(180, 130);
    let cursor_offset = direct_unflip::record_count(&harness.scene, harness.device_fd);
    Backend::maybe_composite(&mut harness.scene.events.backend)
        .expect("dispatch does not make the idle owner active again");
    let cursor = accepted_cursor_records(&super::vrr_cursor_records_since(&harness, cursor_offset));
    assert_eq!(
        cursor.len(),
        1,
        "the queued Present's acceptance time is already idle after dispatch at {} ms",
        (base + Duration::from_millis(191))
            .duration_since(base)
            .as_millis()
    );
    harness.retire_cursor_move(109);
    harness.direct = Some(direct);
    harness.finish();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: do not stamp queued acceptances.
fn vrr_cursor_hold_queued_acceptance_renews() {
    let Some(mut harness) = root_harness(28_006, "vrr_cursor_hold_queued_acceptance_renews") else {
        return;
    };
    let (base, elapsed_ns) = harness.controlled_clock();
    let mut direct = harness.direct.take().expect("direct scanout is active");
    let initial_id = direct.candidate.present_id;
    let device_fd = harness.device_fd;
    let _first = super::submit_direct_successor(&mut harness.scene, &mut direct, device_fd);
    let first_id = direct.candidate.present_id;
    advance_clock(&elapsed_ns, Duration::from_millis(90));
    super::queue_direct_successor(&mut harness.scene, &mut direct);
    let queued_id = direct.candidate.present_id;
    assert_eq!(
        harness
            .scene
            .events
            .backend
            .pstate
            .direct
            .root()
            .last_direct_present_at,
        Some(base + Duration::from_millis(90)),
        "accepted queued successor B records its acceptance time"
    );

    advance_clock(&elapsed_ns, Duration::from_millis(30));
    let first = present_record(&harness, first_id);
    retire_present(&mut harness, &first, &[first_id], &[initial_id], 110);
    let dispatched = present_record(&harness, queued_id);
    retire_present(&mut harness, &dispatched, &[queued_id], &[first_id], 111);
    advance_clock(&elapsed_ns, Duration::from_millis(5));
    harness.warp_pointer(180, 130);
    let cursor_offset = direct_unflip::record_count(&harness.scene, harness.device_fd);
    Backend::maybe_composite(&mut harness.scene.events.backend)
        .expect("a queued acceptance within 100 ms keeps the cursor held");
    assert!(
        accepted_cursor_records(&super::vrr_cursor_records_since(&harness, cursor_offset))
            .is_empty(),
        "the move at 125 ms waits for a direct frame accepted at 90 ms"
    );
    assert_eq!(
        Backend::next_wakeup(&harness.scene.events.backend),
        Some(base + Duration::from_millis(158)),
        "the renewed hold still uses the 33 ms cursor deadline"
    );
    harness.direct = Some(direct);
    harness.finish();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: stamp on a cursor commit or its retirement.
fn vrr_cursor_hold_cursor_commits_do_not_renew() {
    let Some(mut harness) = root_harness(28_007, "vrr_cursor_hold_cursor_commits_do_not_renew")
    else {
        return;
    };
    let (base, elapsed_ns) = harness.controlled_clock();
    let offset = direct_unflip::record_count(&harness.scene, harness.device_fd);
    advance_clock(&elapsed_ns, IDLE);
    harness.warp_pointer(180, 130);
    Backend::maybe_composite(&mut harness.scene.events.backend)
        .expect("submit a cursor-only move for the idle owner");
    let first = accepted_cursor_records(&super::vrr_cursor_records_since(&harness, offset));
    assert_eq!(
        first.len(),
        1,
        "the idle move commits immediately at 100 ms"
    );
    advance_clock(&elapsed_ns, Duration::from_millis(1));
    harness.retire_cursor_move(112);
    advance_clock(&elapsed_ns, Duration::from_millis(1));
    harness.warp_pointer(190, 140);
    Backend::maybe_composite(&mut harness.scene.events.backend)
        .expect("cursor commit and retirement do not renew Present activity");
    let cursor = accepted_cursor_records(&super::vrr_cursor_records_since(&harness, offset));
    assert_eq!(
        cursor.len(),
        2,
        "the second move also commits at 102 ms without a direct Present (base + {} ms)",
        (base + Duration::from_millis(102))
            .duration_since(base)
            .as_millis()
    );
    assert_record_cursor_position(&harness, &cursor[1], (190, 140));
    harness.retire_cursor_move(113);
    harness.finish();
}

struct OutputHarness {
    fixture: super::super::super::cursor_sprites::support::AtomicPathFixture,
    delivered: HashSet<super::super::super::cursor_sprites::support::CompletionKey>,
    directs: [Option<recording_scene::DirectPresentSetup>; 2],
    base: Instant,
    elapsed_ns: Arc<AtomicU64>,
    x: [i32; 2],
    crtcs: [::drm::control::crtc::Handle; 2],
}

impl OutputHarness {
    fn accept_output_present(
        &mut self,
        output_idx: usize,
        present_id: u64,
        frame: u32,
    ) -> AtomicCommitRecord {
        use super::super::super::cursor_sprites::support as cursor_support;

        let mut direct = self
            .fixture
            .scene
            .install_direct_output_present(output_idx, present_id, 0);
        super::per_output::set_present_client(&mut direct, output_idx as u32 + 1);
        Backend::set_cursor_hidden(&mut self.fixture.scene.events.backend, false);
        super::per_output::settle_vrr_cursor_output(
            &mut self.fixture,
            &mut self.delivered,
            output_idx,
        );
        let record = super::super::direct_unflip::submit_output_present_via_backend(
            &mut self.fixture.scene,
            &direct,
            output_idx,
        );
        assert!(
            super::contains_vrr_property(&record, self.crtcs[output_idx], 1),
            "Output({output_idx}) Present commits VRR"
        );
        super::per_output::retire_output_direct(&mut self.fixture.scene, &direct, &record, frame);
        super::per_output::remember_retired_cursor_records(
            std::slice::from_ref(&record),
            &mut self.delivered,
        );
        cursor_support::drain_unretired_output_events(
            &mut self.fixture,
            &mut self.delivered,
            output_idx,
        );
        self.directs[output_idx] = Some(direct);
        record
    }

    fn accept_output_successor(&mut self, output_idx: usize, frame: u32) -> AtomicCommitRecord {
        use super::super::super::cursor_sprites::support as cursor_support;

        let direct = self.directs[output_idx]
            .as_mut()
            .expect("output direct owner is active");
        let device_fd = self.fixture.scene.kms_fd(output_idx);
        let record = super::submit_direct_successor(&mut self.fixture.scene, direct, device_fd);
        super::per_output::retire_output_direct(&mut self.fixture.scene, direct, &record, frame);
        super::per_output::remember_retired_cursor_records(
            std::slice::from_ref(&record),
            &mut self.delivered,
        );
        cursor_support::drain_unretired_output_events(
            &mut self.fixture,
            &mut self.delivered,
            output_idx,
        );
        record
    }

    fn finish(mut self) {
        use super::super::super::cursor_sprites::support as cursor_support;

        for output_idx in (0..2).rev() {
            if let Some(direct) = self.directs[output_idx].as_ref() {
                Backend::unmap_subwindow(
                    &mut self.fixture.scene.events.backend,
                    None,
                    direct.target_xid,
                )
                .expect("unmap output direct owner through Backend");
            }
        }
        for frame in 1..=4 {
            Backend::maybe_composite(&mut self.fixture.scene.events.backend)
                .expect("schedule output direct replacements");
            for output_idx in 0..2 {
                if cursor_support::has_unretired_accepted_event(
                    &self.fixture.scene.events,
                    &self.delivered,
                    output_idx,
                ) {
                    cursor_support::retire_pending_output(
                        &mut self.fixture.scene.events,
                        &mut self.delivered,
                        output_idx,
                        frame + u32::try_from(output_idx).expect("output index fits"),
                    );
                }
            }
        }
        Backend::set_cursor_hidden(&mut self.fixture.scene.events.backend, true);
        for output_idx in 0..2 {
            if self
                .fixture
                .scene
                .events
                .backend
                .platform
                .cursor_plane_visible_for_output(output_idx)
            {
                self.fixture
                    .scene
                    .events
                    .backend
                    .platform
                    .cursor_plane_hide_on_crtc(output_idx)
                    .expect("detach the test cursor from its CRTC");
            }
        }
        for output_idx in 0..2 {
            cursor_support::drain_unretired_output_events(
                &mut self.fixture,
                &mut self.delivered,
                output_idx,
            );
        }
        cursor_support::finish_atomic_fixture(self.fixture, &mut self.delivered);
    }
}

fn output_harness(present_id: u64) -> Option<OutputHarness> {
    use super::super::super::cursor_sprites::support as cursor_support;

    let mut fixture =
        cursor_support::AtomicPathFixture::with_output_count(cursor_support::PathKind::Physical, 2)
            .expect("two-output VRR cursor fixture construction")?;
    let mut delivered = HashSet::new();
    let cursor = cursor_support::create_argb_cursor(
        &mut fixture.scene.events.backend,
        (8, 8),
        [0xff, 0xff, 0xff, 0xff],
        (0, 0),
    );
    let x = [
        fixture.scene.events.backend.platform.outputs[0].x + 120,
        fixture.scene.events.backend.platform.outputs[1].x + 120,
    ];
    for (output_idx, &pointer_x) in x.iter().enumerate() {
        cursor_support::establish_hardware_cursor_on_output(
            &mut fixture,
            &mut delivered,
            cursor,
            output_idx,
            (pointer_x, 110),
        );
        cursor_support::drain_unretired_output_events(&mut fixture, &mut delivered, 1 - output_idx);
    }
    Backend::warp_pointer_root(
        &mut fixture.scene.events.backend,
        &mut fixture.scene.events.state,
        x[0],
        110,
    );
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("settle the pointer before Output direct admission");
    for output_idx in 0..2 {
        cursor_support::drain_unretired_output_events(&mut fixture, &mut delivered, output_idx);
    }

    fixture.scene.events.backend.pstate.vrr_mode = crate::launch::VrrMode::Property;
    for output_idx in 0..2 {
        super::per_output::set_output_vrr_capable(&mut fixture.scene, output_idx, true);
    }
    let base_and_elapsed = support::install_controlled_clock(&mut fixture.scene.events.backend);
    let base = base_and_elapsed.0;
    let elapsed_ns = Arc::clone(&base_and_elapsed.1);
    let crtcs = [
        fixture.scene.events.backend.platform.outputs[0].output.crtc,
        fixture.scene.events.backend.platform.outputs[1].output.crtc,
    ];
    let mut harness = OutputHarness {
        fixture,
        delivered,
        directs: [None, None],
        base,
        elapsed_ns,
        x,
        crtcs,
    };
    harness.accept_output_present(0, present_id, 120);
    Some(harness)
}

fn run_root_pending_move_case(present_id: u64, reevaluate_at_105: bool) -> bool {
    let Some(mut harness) =
        root_harness(present_id, "vrr_cursor_hold_pending_move_crosses_boundary")
    else {
        return false;
    };
    let (base, elapsed_ns) = harness.controlled_clock();
    let offset = direct_unflip::record_count(&harness.scene, harness.device_fd);
    advance_clock(&elapsed_ns, Duration::from_millis(90));
    harness.warp_pointer(180, 130);
    Backend::maybe_composite(&mut harness.scene.events.backend)
        .expect("record a move held behind the idle boundary");
    assert_eq!(
        Backend::next_wakeup(&harness.scene.events.backend),
        Some(base + Duration::from_millis(123)),
        "the real Backend scheduler retains the move's original 33 ms wake"
    );
    if reevaluate_at_105 {
        advance_clock(&elapsed_ns, Duration::from_millis(15));
        assert_eq!(
            Backend::next_wakeup(&harness.scene.events.backend),
            Some(base + Duration::from_millis(105)),
            "reevaluation after idle makes the held movement immediately runnable"
        );
    } else {
        advance_clock(&elapsed_ns, Duration::from_millis(33));
    }
    Backend::maybe_composite(&mut harness.scene.events.backend)
        .expect("honor the real scheduled wake without another input event");
    let cursor = accepted_cursor_records(&super::vrr_cursor_records_since(&harness, offset));
    assert_eq!(
        cursor.len(),
        1,
        "one cursor-only commit carries the pending move"
    );
    assert_record_cursor_position(&harness, &cursor[0], (180, 130));
    harness.retire_cursor_move(121);
    harness.finish();
    true
}

fn run_output_pending_move_case(present_id: u64, reevaluate_at_105: bool) -> bool {
    use super::super::super::cursor_sprites::support as cursor_support;

    let Some(mut harness) = output_harness(present_id) else {
        eprintln!(
            "SKIP vrr_cursor_hold_pending_move_crosses_boundary: two-output Vulkan fixture unavailable"
        );
        return false;
    };
    let offset = super::super::direct_unflip::record_count(
        &harness.fixture.scene,
        harness.fixture.scene.kms_fd(0),
    );
    advance_clock(&harness.elapsed_ns, Duration::from_millis(90));
    Backend::warp_pointer_root(
        &mut harness.fixture.scene.events.backend,
        &mut harness.fixture.scene.events.state,
        harness.x[0] + 60,
        130,
    );
    Backend::maybe_composite(&mut harness.fixture.scene.events.backend)
        .expect("record an Output-owner move held behind the idle boundary");
    let now_90 = harness.base + Duration::from_millis(90);
    let wake = Backend::next_wakeup(&harness.fixture.scene.events.backend);
    assert_eq!(
        wake,
        Some(harness.base + Duration::from_millis(123)),
        "Output owner exposes the pending move's actual 33 ms Backend wake"
    );
    if reevaluate_at_105 {
        advance_clock(&harness.elapsed_ns, Duration::from_millis(15));
        assert_eq!(
            Backend::next_wakeup(&harness.fixture.scene.events.backend),
            Some(harness.base + Duration::from_millis(105)),
            "Output owner becomes immediately runnable when reevaluated after idle"
        );
    } else {
        let deadline = wake.expect("the first scheduler query returned the held move deadline");
        advance_clock(&harness.elapsed_ns, deadline.duration_since(now_90));
    }
    Backend::maybe_composite(&mut harness.fixture.scene.events.backend)
        .expect("honor the Backend wake without another motion or Present");
    let fd = harness.fixture.scene.kms_fd(0);
    let cursor = accepted_cursor_records(&super::super::direct_unflip::records_since(
        &harness.fixture.scene,
        fd,
        offset,
    ));
    assert_eq!(
        cursor.len(),
        1,
        "one Output cursor-only request crosses the boundary"
    );
    assert_record_cursor_position_output(
        &harness.fixture.scene.events.backend,
        &cursor[0],
        0,
        (harness.x[0] + 60, 130),
    );
    cursor_support::retire_pending_output(
        &mut harness.fixture.scene.events,
        &mut harness.delivered,
        0,
        122,
    );
    harness.finish();
    true
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: lose the cursor wake when idle removes the held deadline.
fn vrr_cursor_hold_pending_move_crosses_boundary() {
    assert!(run_root_pending_move_case(28_008, false));
    assert!(run_root_pending_move_case(28_009, true));
    assert!(run_output_pending_move_case(28_010, false));
    assert!(run_output_pending_move_case(28_011, true));
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: read one scope for every output.
fn vrr_cursor_hold_idle_is_per_output() {
    use super::super::super::cursor_sprites::support as cursor_support;

    let Some(mut harness) = output_harness(28_012) else {
        eprintln!("SKIP vrr_cursor_hold_idle_is_per_output: two-output Vulkan fixture unavailable");
        return;
    };
    harness.accept_output_present(1, 28_013, 123);
    advance_clock(&harness.elapsed_ns, IDLE + Duration::from_millis(1));
    harness.accept_output_successor(1, 124);
    assert!(
        harness.fixture.scene.events.backend.platform.outputs[0]
            .output
            .vrr_committed
            .get()
            && harness.fixture.scene.events.backend.platform.outputs[1]
                .output
                .vrr_committed
                .get(),
        "both independent Output owners keep VRR committed"
    );

    let fd = harness.fixture.scene.kms_fd(0);
    let idle_offset = super::super::direct_unflip::record_count(&harness.fixture.scene, fd);
    Backend::warp_pointer_root(
        &mut harness.fixture.scene.events.backend,
        &mut harness.fixture.scene.events.state,
        harness.x[0] + 5,
        120,
    );
    Backend::maybe_composite(&mut harness.fixture.scene.events.backend)
        .expect("the idle output sends cursor motion immediately");
    let idle_cursors = accepted_cursor_records(&super::super::direct_unflip::records_since(
        &harness.fixture.scene,
        fd,
        idle_offset,
    ));
    assert_eq!(
        idle_cursors.len(),
        1,
        "Output(0)'s idle owner commits its cursor immediately"
    );
    assert!(
        idle_cursors[0]
            .events
            .iter()
            .any(|event| event.crtc == harness.crtcs[0])
    );
    assert!(
        idle_cursors[0]
            .events
            .iter()
            .all(|event| event.crtc != harness.crtcs[1])
    );
    cursor_support::retire_pending_output(
        &mut harness.fixture.scene.events,
        &mut harness.delivered,
        0,
        125,
    );

    let fresh_offset = super::super::direct_unflip::record_count(&harness.fixture.scene, fd);
    Backend::warp_pointer_root(
        &mut harness.fixture.scene.events.backend,
        &mut harness.fixture.scene.events.state,
        harness.x[1] + 5,
        120,
    );
    Backend::maybe_composite(&mut harness.fixture.scene.events.backend)
        .expect("the fresh output keeps its cursor hold");
    let fresh_records =
        super::super::direct_unflip::records_since(&harness.fixture.scene, fd, fresh_offset);
    assert!(
        fresh_records.iter().all(|record| {
            record.kind != AtomicCommitKind::CursorOnly
                || support::accepted_event_id(record).is_none()
                || record
                    .events
                    .iter()
                    .all(|event| event.crtc != harness.crtcs[1])
        }),
        "Output(1)'s fresh owner remains held while Output(0) is idle"
    );
    harness.finish();
}

fn configure_gamma(harness: &mut VrrCursorHarness) -> (::drm::control::crtc::Handle, u32) {
    let output = &mut harness.scene.events.backend.platform.outputs[0].output;
    output.crtc_gamma_lut_prop = Some(handle(70));
    output.crtc_degamma_lut_prop = Some(handle(71));
    output.crtc_ctm_prop = Some(handle(72));
    output.crtc_gamma_lut_size = Some(4);
    let crtc = output.crtc;
    let crtc_xid = u32::from(crtc);
    let output_key = harness.scene.events.backend.platform.outputs[0].key.clone();
    harness
        .scene
        .events
        .backend
        .pstate
        .crtc_key_by_id
        .insert(crtc_xid, output_key);
    harness.scene.events.recorders[&harness.device_fd].set_gamma_size_for_tests(4);
    (crtc, crtc_xid)
}

fn set_gamma_ramp(harness: &mut VrrCursorHarness, crtc_xid: u32) {
    Backend::set_crtc_gamma(
        &mut harness.scene.events.backend,
        crtc_xid,
        &[0x1111; 4],
        &[0x2222; 4],
        &[0x3333; 4],
    )
    .expect("set gamma ramp through Backend/RandR");
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation, one per caller: feed the idle-aware value to the gamma path, or to the property retry.
fn vrr_cursor_hold_idle_leaves_vrr_alone() {
    let Some(mut immediate) = root_harness(28_015, "vrr_cursor_hold_idle_leaves_vrr_alone") else {
        return;
    };
    let (_, elapsed_ns) = immediate.controlled_clock();
    let (crtc, crtc_xid) = configure_gamma(&mut immediate);
    advance_clock(&elapsed_ns, IDLE + Duration::from_millis(1));
    let crtc_key = crate::kms::render::platform::CrtcKey::for_output(
        &immediate.scene.events.backend.platform.outputs[0],
    );
    immediate
        .scene
        .events
        .backend
        .platform
        .last_page_flip_received_at
        .remove(&crtc_key);
    let offset = direct_unflip::record_count(&immediate.scene, immediate.device_fd);
    set_gamma_ramp(&mut immediate, crtc_xid);
    let gamma = super::vrr_cursor_records_since(&immediate, offset)
        .into_iter()
        .find(|record| {
            record.kind == AtomicCommitKind::GammaOnly
                && support::accepted_event_id(record).is_some()
        })
        .expect("idle gamma is submitted as a GammaOnly request");
    assert!(
        !super::has_vrr_property(&gamma, crtc),
        "immediate gamma submission does not clear VRR_ENABLED"
    );
    immediate.retire_cursor_move(128);
    immediate.finish();

    let Some(mut retry) = root_harness(28_016, "vrr_cursor_hold_idle_leaves_vrr_alone") else {
        return;
    };
    let (base, elapsed_ns) = retry.controlled_clock();
    let (crtc, crtc_xid) = configure_gamma(&mut retry);
    advance_clock(&elapsed_ns, IDLE + Duration::from_millis(1));
    let offset = direct_unflip::record_count(&retry.scene, retry.device_fd);
    retry.warp_pointer(180, 130);
    Backend::maybe_composite(&mut retry.scene.events.backend)
        .expect("idle cursor motion submits while gamma will wait behind it");
    let cursor = accepted_cursor_records(&super::vrr_cursor_records_since(&retry, offset));
    assert_eq!(cursor.len(), 1, "idle cursor commit is in flight");
    set_gamma_ramp(&mut retry, crtc_xid);
    let deadline = retry.scene.events.backend.platform.outputs[0]
        .output
        .pending_gamma_deadline
        .get()
        .expect("gamma held behind the cursor commit retains its refresh deadline");
    assert!(
        super::vrr_cursor_records_since(&retry, offset)
            .iter()
            .all(|record| record.kind != AtomicCommitKind::GammaOnly),
        "gamma stays behind the cursor commit until its existing deadline and event"
    );
    let now = base + IDLE + Duration::from_millis(1);
    advance_clock(&elapsed_ns, deadline.duration_since(now));
    assert_eq!(
        Backend::next_wakeup(&retry.scene.events.backend),
        Some(deadline),
        "the existing gamma deadline remains the scheduler wake"
    );
    Backend::maybe_composite(&mut retry.scene.events.backend)
        .expect("honor the gamma deadline while the cursor event is still in flight");
    retry.retire_cursor_move(130);
    let gamma = super::vrr_cursor_records_since(&retry, offset)
        .into_iter()
        .find(|record| {
            record.kind == AtomicCommitKind::GammaOnly
                && support::accepted_event_id(record).is_some()
        })
        .expect("retiring the cursor event retries the queued gamma request");
    assert!(
        !super::has_vrr_property(&gamma, crtc),
        "property retry does not clear VRR_ENABLED for an idle direct owner"
    );
    retry.retire_cursor_move(131);
    retry.finish();
}

fn gamma_commit_gate(
    fixture: &mut recording_scene::RecordingSceneFixture,
    output_idx: usize,
    base: Instant,
) -> AtomicCommitRecord {
    use std::os::fd::{AsFd as _, AsRawFd as _};

    let output = &mut fixture.events.backend.platform.outputs[output_idx].output;
    output.crtc_gamma_lut_prop = Some(handle(70));
    output.crtc_degamma_lut_prop = Some(handle(71));
    output.crtc_ctm_prop = Some(handle(72));
    output.crtc_gamma_lut_size = Some(4);
    let crtc_xid = u32::from(output.crtc);
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
    let offset = recorder.records().len();
    Backend::set_crtc_gamma(
        &mut fixture.events.backend,
        crtc_xid,
        &[0x1111; 4],
        &[0x2222; 4],
        &[0x3333; 4],
    )
    .expect("submit gamma through Backend before direct entry");
    if !recorder.records().iter().skip(offset).any(|record| {
        record.kind == AtomicCommitKind::GammaOnly && support::accepted_event_id(record).is_some()
    }) {
        fixture.events.backend.platform.outputs[output_idx]
            .output
            .pending_gamma_deadline
            .set(Some(base));
        Backend::maybe_composite(&mut fixture.events.backend)
            .expect("submit the due gamma gate through the Backend scheduler");
    }
    recorder
        .records()
        .into_iter()
        .skip(offset)
        .find(|record| {
            record.kind == AtomicCommitKind::GammaOnly
                && support::accepted_event_id(record).is_some()
        })
        .expect("GammaOnly page flip holds the output CRTC")
}

fn offer_output_candidate_after_probation(
    fixture: &mut recording_scene::RecordingSceneFixture,
    direct: &recording_scene::DirectPresentSetup,
) {
    Backend::note_present_scanout_candidate(&mut fixture.events.backend, direct.candidate);
    let probation = usize::from(crate::kms::render::backend::SCANOUT_M2_ELIGIBLE_ROOT_PROBATION);
    for attempt in 0..probation {
        let accepted = Backend::try_present_direct(
            &mut fixture.events.backend,
            direct.candidate,
            direct.event.clone(),
        )
        .expect("Output Present through Backend");
        assert_eq!(
            accepted,
            attempt + 1 == probation,
            "queued first entry is accepted after normal direct probation"
        );
    }
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: skip the reset on either path.
fn vrr_cursor_hold_reset_on_teardown() {
    let Some(mut harness) = root_harness(28_017, "vrr_cursor_hold_reset_on_teardown") else {
        return;
    };
    let present_id = harness
        .direct
        .as_ref()
        .expect("direct scanout is active")
        .candidate
        .present_id;
    assert!(
        harness
            .scene
            .events
            .backend
            .pstate
            .direct
            .root()
            .last_direct_present_at
            .is_some(),
        "a successful direct Present establishes Some(time) before unflip"
    );
    let device_fd = harness.device_fd;
    let direct = harness.direct.take().expect("direct scanout is active");
    direct_unflip::request_unflip(&mut harness.scene, &direct);
    let offset = direct_unflip::record_count(&harness.scene, device_fd);
    let unflip = direct_unflip::grouped_unflip_record(&mut harness.scene, device_fd, offset, 1);
    let unflip_id = support::accepted_event_id(&unflip).expect("unflip event ID");
    atomic_events::complete_recorded_attempt(
        &mut harness.scene,
        device_fd,
        &unflip,
        unflip_id,
        132,
    );
    direct_unflip::drain_present_notifications(&mut harness.scene, &[], &[present_id]);
    assert_eq!(
        harness
            .scene
            .events
            .backend
            .pstate
            .direct
            .root()
            .last_direct_present_at,
        None,
        "Backend-driven unflip clears the timestamp with the other owner state"
    );
    harness.finish();

    let Some(mut fixture) = super::per_output::two_output_vrr_fixture() else {
        eprintln!("SKIP vrr_cursor_hold_reset_on_teardown: two-output Vulkan fixture unavailable");
        return;
    };
    let (base, _) = support::install_controlled_clock(&mut fixture.events.backend);
    let output = &mut fixture.events.backend.platform.outputs[0].output;
    output.crtc_vrr_enabled_prop = Some(handle(VRR_PROP_ID));
    output.connector_vrr_capable = true;
    let gate = gamma_commit_gate(&mut fixture, 0, base);
    fixture.events.backend.for_tests_set_scanout_m1_prober(
        super::super::super::present_scanout::record_scanout_m1_probe,
    );
    let direct = fixture.install_direct_output_present(0, 28_018, 0);
    offer_output_candidate_after_probation(&mut fixture, &direct);
    let owner = &fixture.events.backend.pstate.direct.outputs[0];
    assert!(owner.queued_successor.is_some());
    assert_eq!(
        owner.last_direct_present_at,
        Some(base),
        "accepted queued first entry establishes Some(time) before fallback"
    );
    Backend::unmap_subwindow(&mut fixture.events.backend, None, direct.target_xid)
        .expect("unmap cancels the queued first entry through Backend");
    assert_eq!(
        fixture.events.backend.pstate.direct.outputs[0].last_direct_present_at, None,
        "queued first-entry Copy fallback clears the owner timestamp"
    );

    let gamma_id = support::accepted_event_id(&gate).expect("gamma gate event ID");
    atomic_events::complete_recorded_attempt(&mut fixture, gate.device_fd, &gate, gamma_id, 133);
    let _ = Backend::drain_completed_present_events(&mut fixture.events.backend);
    let _ = Backend::drain_retired_present_idle_events(&mut fixture.events.backend);
    let pending_ids: HashSet<_> = (0..fixture.events.backend.platform.outputs.len())
        .flat_map(|output_idx| {
            let crtc = fixture.events.backend.platform.outputs[output_idx]
                .output
                .crtc;
            atomic_events::pending_ids(&fixture, gate.device_fd, crtc)
        })
        .collect();
    let pending_records: Vec<_> = direct_unflip::records_since(&fixture, gate.device_fd, 0)
        .into_iter()
        .filter(|record| {
            support::accepted_event_id(record).is_some_and(|id| pending_ids.contains(&id))
        })
        .collect();
    super::per_output::complete_output_records(&mut fixture, &pending_records, 134);
    fixture.finish_after_normal_drain();
}
