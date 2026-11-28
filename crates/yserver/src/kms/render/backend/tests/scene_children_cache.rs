use super::{commit_identity::recording_scene::RecordingSceneFixture, *};
use crate::drm::atomic::tests_support::AtomicCommitRecord;
use std::{collections::HashMap, os::fd::RawFd, time::Duration};
use yserver_core::{
    backend::Backend, core_loop::process_request, resources::ROOT_WINDOW, server::ServerState,
};
use yserver_protocol::x11::{ClientId, RequestHeader, ResourceId, SequenceNumber};

const ROOT_A: ResourceId = ResourceId(0x0040_0100);
const CHILD_A: ResourceId = ResourceId(0x0040_0101);
const CHILD_B: ResourceId = ResourceId(0x0040_0102);
const SPAN_A: ResourceId = ResourceId(0x0040_0103);
const SPAN_B: ResourceId = ResourceId(0x0040_0104);
const NEW_CHILD: ResourceId = ResourceId(0x0040_0105);

fn live_fixture() -> RecordingSceneFixture {
    RecordingSceneFixture::new(2)
        .expect("two-output recording scene fixture construction")
        .expect("live Vulkan recording scene fixture is available")
}

fn create_scene_windows(fixture: &mut RecordingSceneFixture) -> (u32, u32, u32, u32, u32) {
    let state = &mut fixture.events.state;
    let backend = &mut fixture.events.backend;

    let root_a = super::create_live_window(state, backend, ROOT_A, ROOT_WINDOW, 100, 40, 400, 300);
    let child_a = super::create_live_window(state, backend, CHILD_A, ROOT_A, 20, 20, 160, 120);
    let child_b = super::create_live_window(state, backend, CHILD_B, ROOT_A, 70, 50, 160, 120);
    let span_a = super::create_live_window(state, backend, SPAN_A, ROOT_WINDOW, 0, 400, 1600, 80);
    let span_b = super::create_live_window(state, backend, SPAN_B, ROOT_WINDOW, 0, 500, 1600, 80);
    Backend::sync_top_level_order(backend, state);

    (
        root_a.as_raw(),
        child_a.as_raw(),
        child_b.as_raw(),
        span_a.as_raw(),
        span_b.as_raw(),
    )
}

fn record_offsets(fixture: &RecordingSceneFixture) -> HashMap<RawFd, usize> {
    fixture
        .events
        .recorders
        .iter()
        .map(|(&fd, recorder)| (fd, recorder.records().len()))
        .collect()
}

fn accepted_records_since(
    fixture: &RecordingSceneFixture,
    offsets: &HashMap<RawFd, usize>,
) -> Vec<(RawFd, AtomicCommitRecord)> {
    fixture
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
                        && super::commit_identity::accepted_event_id(record).is_some()
                        && !record.events.is_empty()
                })
                .map(move |record| (fd, record))
        })
        .collect()
}

/// Add drawable damage and enter one real backend scene tick. The caller
/// checks the tick counters before retiring the accepted flips or walking the
/// scene for draw-list assertions.
fn tick_with_damage(
    fixture: &mut RecordingSceneFixture,
    target: u32,
    rectangle: Option<(i16, i16, u16, u16)>,
) -> Vec<(RawFd, AtomicCommitRecord)> {
    let offsets = record_offsets(fixture);
    let (x, y, width, height) = rectangle.unwrap_or((0, 0, 1, 1));
    let mut wire = Vec::with_capacity(8);
    wire.extend_from_slice(&x.to_le_bytes());
    wire.extend_from_slice(&y.to_le_bytes());
    wire.extend_from_slice(&width.to_le_bytes());
    wire.extend_from_slice(&height.to_le_bytes());
    Backend::poly_fill_rectangle(
        &mut fixture.events.backend,
        None,
        target,
        0xff00_00ff,
        &wire,
    )
    .expect("damage a live window through poly_fill_rectangle");
    Backend::maybe_composite(&mut fixture.events.backend).expect("tick scene through Backend");
    let records = accepted_records_since(fixture, &offsets);
    assert!(
        !records.is_empty(),
        "a scene tick with drawable damage must submit accepted flips"
    );
    records
}

fn retire_accepted(fixture: &mut RecordingSceneFixture, records: &[(RawFd, AtomicCommitRecord)]) {
    fixture.events.wait_for_submitted_gpu_work();
    for (offset, (fd, record)) in records.iter().enumerate() {
        let frame = 100 + u32::try_from(offset).expect("flip index fits in a frame number");
        for event in &record.events {
            fixture.events.queue_commit_completion(
                *fd,
                event.crtc,
                record.user_data,
                frame,
                Duration::from_millis(u64::from(frame) * 10),
            );
        }
        Backend::on_page_flip_ready(&mut fixture.events.backend, &mut fixture.events.state, *fd);
    }
}

fn tick_counts(fixture: &RecordingSceneFixture) -> (u64, u64) {
    let indexed: &crate::kms::render::window_geometry::IndexedWindows =
        fixture.events.backend.windows.indexed();
    (
        fixture.events.backend.telemetry.lifetime.build_scene_calls,
        indexed.children_index_builds(),
    )
}

fn assert_tick_counts(
    before: (u64, u64),
    after: (u64, u64),
    expected_walks: std::ops::RangeInclusive<u64>,
    expected_builds: u64,
    label: &str,
) {
    assert!(
        expected_walks.contains(&(after.0 - before.0)),
        "{label}: expected walk delta {expected_walks:?}, got {}",
        after.0 - before.0
    );
    assert_eq!(
        after.1 - before.1,
        expected_builds,
        "{label}: children index build delta"
    );
}

fn draw_rects(fixture: &mut RecordingSceneFixture) -> Vec<(i32, i32, u32, u32)> {
    fixture.events.backend.scene_draw_rects_for_tests()
}

fn destroy_window(state: &mut ServerState, backend: &mut KmsBackend, window: ResourceId) {
    process_request::process_request(
        state,
        backend as &mut dyn Backend,
        ClientId(14),
        SequenceNumber(1),
        RequestHeader {
            opcode: 4, // DestroyWindow
            data: 0,
            length_units: 2,
        },
        &window.0.to_le_bytes(),
        None,
    )
    .expect("DestroyWindow process_request must succeed");
    Backend::sync_top_level_order(backend, state);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
fn children_index_built_once_across_damage_only_ticks() {
    let mut fixture = live_fixture().cleanup_guard();
    let (_, child_a, _, _, _) = create_scene_windows(&mut fixture);

    let before = tick_counts(&fixture);
    let root = fixture.events.backend.core.window_id;
    let initial_records = tick_with_damage(&mut fixture, root, None);
    let after = tick_counts(&fixture);
    assert_tick_counts(before, after, 2..=2, 1, "first two-output tick");
    retire_accepted(&mut fixture, &initial_records);
    let initial_draws = draw_rects(&mut fixture);

    for tick in 1..=5 {
        let before = tick_counts(&fixture);
        let records = tick_with_damage(&mut fixture, child_a, Some((1, 1, 20, 20)));
        let after = tick_counts(&fixture);
        assert_tick_counts(
            before,
            after,
            1..=u64::MAX,
            0,
            &format!("damage-only tick {tick}"),
        );
        retire_accepted(&mut fixture, &records);
    }

    let final_draws = draw_rects(&mut fixture);
    assert_eq!(
        final_draws, initial_draws,
        "damage-only ticks preserve the emitted scene draw order"
    );
    assert_eq!(
        tick_counts(&fixture).1,
        1,
        "damage-only ticks leave the final children index build count at one"
    );
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
fn children_index_follows_tree_changes() {
    let mut fixture = live_fixture().cleanup_guard();
    let (_, _, _, _span_a, _span_b) = create_scene_windows(&mut fixture);

    let before = tick_counts(&fixture);
    let root = fixture.events.backend.core.window_id;
    let initial_records = tick_with_damage(&mut fixture, root, None);
    let after = tick_counts(&fixture);
    assert_tick_counts(before, after, 2..=2, 1, "first two-output tick");
    retire_accepted(&mut fixture, &initial_records);
    let mut previous_draws = draw_rects(&mut fixture);

    let mutations = ["restack", "reparent", "map new child", "destroy top-level"];
    for (step, mutation) in mutations.into_iter().enumerate() {
        match step {
            0 => {
                state_configure_child_above(&mut fixture, CHILD_A, CHILD_B);
            }
            1 => {
                let events = fixture.events_mut();
                let state = &mut events.state;
                let backend = &mut events.backend;
                super::dispatch_reparent_window(state, backend, CHILD_A, SPAN_A, 30, 20);
            }
            2 => {
                let events = fixture.events_mut();
                let state = &mut events.state;
                let backend = &mut events.backend;
                super::create_live_window(state, backend, NEW_CHILD, SPAN_A, 230, 15, 70, 70);
                Backend::sync_top_level_order(backend, state);
            }
            3 => {
                let events = fixture.events_mut();
                destroy_window(&mut events.state, &mut events.backend, SPAN_B);
            }
            _ => unreachable!(),
        }

        let before = tick_counts(&fixture);
        let records = tick_with_damage(&mut fixture, root, None);
        let after = tick_counts(&fixture);
        assert_tick_counts(before, after, 1..=u64::MAX, 1, mutation);
        retire_accepted(&mut fixture, &records);

        let current_draws = draw_rects(&mut fixture);
        let fresh_draws = fixture
            .events
            .backend
            .scene_draw_rects_with_fresh_index_for_tests();
        assert_eq!(
            current_draws, fresh_draws,
            "cached draw list matches a fresh reference after {mutation}"
        );
        assert_ne!(
            current_draws, previous_draws,
            "output 0 draw list changes after {mutation}"
        );
        previous_draws = current_draws;
    }

    assert_eq!(
        tick_counts(&fixture).1,
        5,
        "the initial tree plus four visible tree changes build one final index per map version"
    );
    fixture.finish_after_normal_drain();
}

fn state_configure_child_above(
    fixture: &mut RecordingSceneFixture,
    child: ResourceId,
    sibling: ResourceId,
) {
    let state = &mut fixture.events.state;
    let backend = &mut fixture.events.backend;
    let mut body = Vec::with_capacity(16);
    body.extend_from_slice(&child.0.to_le_bytes());
    let value_mask = (1u16 << 5) | (1u16 << 6);
    body.extend_from_slice(&value_mask.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes());
    body.extend_from_slice(&sibling.0.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // Above
    process_request::process_request(
        state,
        backend as &mut dyn Backend,
        ClientId(14),
        SequenceNumber(1),
        RequestHeader {
            opcode: 12, // ConfigureWindow
            data: 0,
            length_units: 5,
        },
        &body,
        None,
    )
    .expect("ConfigureWindow must forward a sibling restack through Backend");
}
