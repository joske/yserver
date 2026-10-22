//! #281: leaving direct scanout flips freshly composed frames, not the frame
//! composed before direct scanout began.

use super::{
    atomic_events::complete_recorded_attempt,
    direct_unflip::{
        accepted_records, complete_direct_initial, direct_fixture, direct_source_pins,
        drain_present_notifications, fd, grouped_unflip_record, inject_output, record_count,
        records_since, request_unflip, submit_direct,
    },
    recording_scene::{DirectPresentSetup, P1SceneRoute, RecordingSceneFixture},
    support::accepted_event_id,
};
use crate::{
    drm::atomic::tests_support::{AtomicCommitRecord, ScriptedResult},
    kms::vk::scanout::{BoPhase, OutputScanout},
};
use ash::vk;
use std::time::Duration;
use yserver_core::backend::Backend;

/// What the window shows before direct scanout begins.
const PRE_DIRECT: [u8; 4] = [0x11, 0x22, 0x33, 0x00];
/// What the direct source shows when the unflip runs.
const DIRECT: [u8; 4] = [0xaa, 0xbb, 0xcc, 0x00];

fn fill(fixture: &mut RecordingSceneFixture, xid: u32, pixel: [u8; 4]) {
    let backend = &mut fixture.events.backend;
    let width = backend.platform.fb_w;
    let height = backend.platform.fb_h;
    let data = pixel.repeat(usize::from(width) * usize::from(height));
    Backend::put_image(backend, None, xid, 24, width, height, 0, 0, &data)
        .expect("fill drawable through Backend");
}

/// Composed pixels of the BO `output_idx` shows (or is about to show).
fn output_pixels(fixture: &mut RecordingSceneFixture, output_idx: usize) -> Vec<u8> {
    let layout = &fixture.events.backend.platform.outputs[output_idx];
    let rect = vk::Rect2D {
        offset: vk::Offset2D {
            x: layout.x,
            y: layout.y,
        },
        extent: vk::Extent2D {
            width: u32::from(layout.width),
            height: u32::from(layout.height),
        },
    };
    crate::kms::render::backend::read_scanout_region(
        &mut fixture.events.backend,
        rect,
        crate::kms::render::backend::ScanoutReadSelection::PermissiveDump,
    )
    .expect("read composed scanout BO")
}

fn assert_output_shows(fixture: &mut RecordingSceneFixture, output_idx: usize, pixel: [u8; 4]) {
    let pixels = output_pixels(fixture, output_idx);
    let wrong = pixels
        .chunks_exact(4)
        .filter(|px| px[..3] != pixel[..3])
        .count();
    assert_eq!(
        wrong,
        0,
        "output {output_idx}: {wrong} of {} pixels differ from {pixel:02x?} (first {:02x?})",
        pixels.len() / 4,
        &pixels[..4]
    );
}

/// The primary-plane `(FB_ID, IN_FENCE_FD)` values of `output_idx` in `record`.
fn plane_values(
    fixture: &RecordingSceneFixture,
    record: &AtomicCommitRecord,
    output_idx: usize,
) -> (Option<u64>, Option<u64>) {
    let output = &fixture.events.backend.platform.outputs[output_idx].output;
    let value = |property| {
        record
            .properties
            .iter()
            .find(|update| update.object == output.plane.into() && update.property == property)
            .map(|update| update.value)
    };
    (
        value(output.plane_fb_id_prop),
        output.plane_in_fence_fd_prop.and_then(value),
    )
}

fn retained_framebuffers(fixture: &RecordingSceneFixture) -> Vec<u64> {
    let platform = &fixture.events.backend.platform;
    (0..platform.outputs.len())
        .map(|output_idx| {
            u64::from(u32::from(
                platform
                    .retained_composed_framebuffer(output_idx)
                    .expect("output has a retained composed framebuffer"),
            ))
        })
        .collect()
}

/// Index and phase of the BO behind framebuffer `fb` on `output_idx`.
fn bo_for_framebuffer(
    fixture: &RecordingSceneFixture,
    output_idx: usize,
    fb: u64,
) -> Option<(usize, BoPhase)> {
    let Some(Some(OutputScanout::Shared(pool))) = fixture
        .events
        .backend
        .platform
        .scanout_pools
        .get(output_idx)
    else {
        return None;
    };
    pool.bos.iter().enumerate().find_map(|(bo_idx, bo)| {
        (bo.fb_handle.map(|handle| u64::from(u32::from(handle))) == Some(fb))
            .then_some((bo_idx, bo.state.phase))
    })
}

/// Compose and retire a frame showing `PRE_DIRECT`, put `DIRECT` into the
/// Present source, then enter direct scanout and retire it.
fn enter_direct_over_stale_frame(
    output_count: usize,
    present_id: u64,
) -> Option<(RecordingSceneFixture, DirectPresentSetup)> {
    let (mut fixture, direct) = direct_fixture(output_count, P1SceneRoute::Shared, present_id, 0)?;
    fill(&mut fixture, direct.target_xid, PRE_DIRECT);
    fixture.drain_deferred_pool_releases(Duration::from_secs(5));
    for output_idx in 0..output_count {
        assert_output_shows(&mut fixture, output_idx, PRE_DIRECT);
    }
    fill(&mut fixture, direct.candidate.src_pixmap_xid, DIRECT);
    let device_fd = fd(&fixture);
    let attempts = submit_direct(&mut fixture, &direct, device_fd);
    let initial = accepted_records(&attempts).remove(0);
    let direct_id = accepted_event_id(&initial).expect("direct frame accepted");
    complete_direct_initial(&mut fixture, &initial, direct_id, device_fd);
    drain_present_notifications(&mut fixture, &[direct.candidate.present_id], &[]);
    Some((fixture, direct))
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: flip the retained framebuffers (skip the fresh path); omit IN_FENCE_FD; let the backend retire the scene's BOs.
fn fresh_unflip_flips_fresh_frames_and_retires_through_scene() {
    let Some((mut fixture, direct)) = enter_direct_over_stale_frame(2, 301) else {
        return;
    };
    let device_fd = fd(&fixture);
    let retained = retained_framebuffers(&fixture);
    request_unflip(&mut fixture, &direct);
    let offset = record_count(&fixture, device_fd);
    let grouped = grouped_unflip_record(&mut fixture, device_fd, offset, 2);
    assert_eq!(
        records_since(&fixture, device_fd, offset).len(),
        1,
        "the fresh frames leave direct scanout in one atomic commit"
    );
    let grouped_id = accepted_event_id(&grouped).unwrap();
    let mut fresh_bos = Vec::new();
    for (output_idx, &retained_fb) in retained.iter().enumerate() {
        let (fb, in_fence) = plane_values(&fixture, &grouped, output_idx);
        let fb = fb.expect("grouped commit sets every primary FB_ID");
        assert_ne!(
            fb, retained_fb,
            "output {output_idx} flips the frame composed before direct scanout"
        );
        assert!(
            in_fence.is_some(),
            "output {output_idx} flips without waiting on its compose"
        );
        let (bo_idx, phase) =
            bo_for_framebuffer(&fixture, output_idx, fb).expect("FB_ID names a pool BO");
        assert_eq!(phase, BoPhase::Pending);
        assert_eq!(
            fixture
                .events
                .backend
                .scene
                .pending_page_flip_event_id(output_idx),
            Some(grouped_id),
            "the scene frame awaits the grouped commit"
        );
        fresh_bos.push((bo_idx, fb));
    }

    // The direct frame stays pinned until the last output retires.
    fixture.events.wait_for_submitted_gpu_work();
    inject_output(
        &mut fixture,
        device_fd,
        &grouped,
        0,
        grouped_id,
        120,
        Duration::from_millis(12_000),
    );
    assert!(fixture.events.backend.pstate.scanout_m2.current.is_some());
    assert_eq!(direct_source_pins(&fixture, direct.source_id), 1);
    assert!(!fixture.events.backend.scene.has_pending_page_flip(0));
    assert!(fixture.events.backend.scene.has_pending_page_flip(1));
    drain_present_notifications(&mut fixture, &[], &[]);
    inject_output(
        &mut fixture,
        device_fd,
        &grouped,
        1,
        grouped_id,
        121,
        Duration::from_millis(12_100),
    );
    assert!(fixture.events.backend.pstate.scanout_m2.current.is_none());
    assert!(fixture.events.backend.present_source_pins.is_empty());
    assert!(!fixture.events.backend.scene.has_pending_page_flips());
    assert!(
        !fixture
            .events
            .backend
            .pstate
            .scanout_m2
            .fresh_composed_unflip
    );
    drain_present_notifications(&mut fixture, &[], &[direct.candidate.present_id]);
    for (output_idx, &(bo_idx, fb)) in fresh_bos.iter().enumerate() {
        assert_eq!(
            bo_for_framebuffer(&fixture, output_idx, fb),
            Some((bo_idx, BoPhase::OnScreen))
        );
        assert_output_shows(&mut fixture, output_idx, DIRECT);
    }
    fixture.finish_after_normal_drain();
}

/// After a failed fresh unflip: the retained-framebuffer unflip was accepted
/// on every output, and each abandoned fresh BO waits for its compose fence
/// in the scene's failed-submit recycler rather than being free for reuse.
fn assert_fell_back_to_retained_unflip(
    fixture: &mut RecordingSceneFixture,
    direct: &DirectPresentSetup,
    retained: &[u64],
    attempts: &[AtomicCommitRecord],
    abandoned_outputs: &[usize],
) {
    let device_fd = fd(fixture);
    let accepted = accepted_records(attempts);
    assert_eq!(accepted.len(), 1, "one accepted replacement: {attempts:?}");
    let fallback = &accepted[0];
    assert_eq!(fallback.events.len(), retained.len());
    for (output_idx, &retained_fb) in retained.iter().enumerate() {
        assert_eq!(
            plane_values(fixture, fallback, output_idx),
            (Some(retained_fb), None),
            "output {output_idx} falls back to its retained framebuffer"
        );
    }
    let retry_state = fixture.events.backend.scene.submit_retry_state_for_tests();
    for (output_idx, &(_, _, failed_submits)) in retry_state.iter().enumerate() {
        let abandoned = abandoned_outputs.contains(&output_idx);
        assert_eq!(
            failed_submits,
            usize::from(abandoned),
            "output {output_idx} failed-submit recycler"
        );
        assert!(
            !fixture
                .events
                .backend
                .scene
                .has_pending_page_flip(output_idx)
        );
        let Some(Some(OutputScanout::Shared(pool))) = fixture
            .events
            .backend
            .platform
            .scanout_pools
            .get(output_idx)
        else {
            panic!("shared pool");
        };
        let phases: Vec<_> = pool.bos.iter().map(|bo| bo.state.phase).collect();
        assert_eq!(
            phases
                .iter()
                .filter(|&&phase| phase == BoPhase::Recording)
                .count(),
            usize::from(abandoned),
            "output {output_idx}: an abandoned fresh BO must stay reserved: {phases:?}"
        );
    }

    let fallback_id = accepted_event_id(fallback).unwrap();
    complete_recorded_attempt(fixture, device_fd, fallback, fallback_id, 130);
    assert!(fixture.events.backend.present_source_pins.is_empty());
    drain_present_notifications(fixture, &[], &[direct.candidate.present_id]);
    for (output_idx, &retained_fb) in retained.iter().enumerate() {
        assert_eq!(
            bo_for_framebuffer(fixture, output_idx, retained_fb).map(|(_, phase)| phase),
            Some(BoPhase::OnScreen)
        );
    }
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: free the abandoned BOs immediately; skip the fallback on a rejected grouped commit.
fn fresh_unflip_rejected_falls_back_to_retained_frames() {
    let Some((mut fixture, direct)) = enter_direct_over_stale_frame(2, 302) else {
        return;
    };
    let device_fd = fd(&fixture);
    let retained = retained_framebuffers(&fixture);
    request_unflip(&mut fixture, &direct);
    let offset = record_count(&fixture, device_fd);
    fixture.events.recorders[&device_fd]
        .script_result(Err(std::io::Error::from_raw_os_error(libc::ENOSPC)));
    Backend::maybe_composite(&mut fixture.events.backend).expect("unflip through Backend");
    let attempts = records_since(&fixture, device_fd, offset);
    let rejected = attempts
        .iter()
        .find(|record| record.result != ScriptedResult::Ok)
        .expect("fresh grouped commit attempted");
    assert_eq!(rejected.events.len(), 2);
    for (output_idx, &retained_fb) in retained.iter().enumerate() {
        let (fb, in_fence) = plane_values(&fixture, rejected, output_idx);
        assert_ne!(fb, Some(retained_fb));
        assert!(in_fence.is_some());
    }
    assert_fell_back_to_retained_unflip(&mut fixture, &direct, &retained, &attempts, &[0, 1]);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: commit the fresh frames that did render when an output produced none.
fn fresh_unflip_missing_output_frame_falls_back_after_first_render() {
    let Some((mut fixture, direct)) = enter_direct_over_stale_frame(2, 303) else {
        return;
    };
    let device_fd = fd(&fixture);
    let retained = retained_framebuffers(&fixture);
    request_unflip(&mut fixture, &direct);
    // Output 1 has no free BO for this tick, so only output 0 renders.
    let reserved: Vec<usize> = {
        let Some(Some(OutputScanout::Shared(pool))) =
            fixture.events.backend.platform.scanout_pools.get_mut(1)
        else {
            panic!("shared pool");
        };
        pool.bos
            .iter_mut()
            .enumerate()
            .filter(|(_, bo)| bo.state.phase == BoPhase::Free)
            .map(|(bo_idx, bo)| {
                bo.state.phase = BoPhase::Retiring;
                bo_idx
            })
            .collect()
    };
    assert!(!reserved.is_empty());
    let offset = record_count(&fixture, device_fd);
    Backend::maybe_composite(&mut fixture.events.backend).expect("unflip through Backend");
    {
        let Some(Some(OutputScanout::Shared(pool))) =
            fixture.events.backend.platform.scanout_pools.get_mut(1)
        else {
            panic!("shared pool");
        };
        for bo_idx in reserved {
            pool.bos[bo_idx].state.phase = BoPhase::Free;
        }
    }
    let attempts = records_since(&fixture, device_fd, offset);
    assert_eq!(attempts.len(), 1, "no fresh grouped commit: {attempts:?}");
    assert_fell_back_to_retained_unflip(&mut fixture, &direct, &retained, &attempts, &[0]);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: drop the shared-route preflight so a copied output renders for the fresh path.
fn fresh_unflip_copied_output_uses_retained_unflip() {
    let Some((mut fixture, direct)) = direct_fixture(1, P1SceneRoute::CopiedSameDevice, 304, 0)
    else {
        return;
    };
    let device_fd = fd(&fixture);
    let initial_attempts = submit_direct(&mut fixture, &direct, device_fd);
    let initial = accepted_records(&initial_attempts).remove(0);
    let direct_id = accepted_event_id(&initial).unwrap();
    complete_direct_initial(&mut fixture, &initial, direct_id, device_fd);
    drain_present_notifications(&mut fixture, &[direct.candidate.present_id], &[]);
    let retained = retained_framebuffers(&fixture);
    request_unflip(&mut fixture, &direct);
    let offset = record_count(&fixture, device_fd);
    let grouped = grouped_unflip_record(&mut fixture, device_fd, offset, 1);
    assert_eq!(records_since(&fixture, device_fd, offset).len(), 1);
    assert_eq!(
        plane_values(&fixture, &grouped, 0),
        (Some(retained[0]), None),
        "a copied output leaves direct scanout on its retained framebuffer"
    );
    assert!(fixture.pending_render_job_ids().is_empty());
    assert!(!fixture.events.backend.scene.has_pending_page_flips());
    let grouped_id = accepted_event_id(&grouped).unwrap();
    complete_recorded_attempt(&mut fixture, device_fd, &grouped, grouped_id, 140);
    assert!(fixture.events.backend.present_source_pins.is_empty());
    drain_present_notifications(&mut fixture, &[], &[direct.candidate.present_id]);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: commit the fresh frames although one output's compose was truncated.
fn fresh_unflip_incomplete_compose_falls_back_to_retained_frames() {
    let Some((mut fixture, direct)) = enter_direct_over_stale_frame(2, 305) else {
        return;
    };
    let device_fd = fd(&fixture);
    let retained = retained_framebuffers(&fixture);
    request_unflip(&mut fixture, &direct);
    // Output 1's fresh compose runs out of descriptor sets before its first
    // draw, so it records only a prefix of the scene.
    fixture
        .events
        .backend
        .scene
        .set_compose_descriptor_limit_for_tests(1, Some(0));
    let offset = record_count(&fixture, device_fd);
    Backend::maybe_composite(&mut fixture.events.backend).expect("unflip through Backend");
    fixture
        .events
        .backend
        .scene
        .set_compose_descriptor_limit_for_tests(1, None);
    let attempts = records_since(&fixture, device_fd, offset);
    assert_eq!(attempts.len(), 1, "no fresh grouped commit: {attempts:?}");
    assert_fell_back_to_retained_unflip(&mut fixture, &direct, &retained, &attempts, &[0, 1]);
    fixture.finish_after_normal_drain();
}
