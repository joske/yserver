use super::*;

/// Stage 5 Task 6.1 — verify that `drain_completed_present_events`
/// force-fires every queued entry when the platform's
/// `renderer_failed` flag is set. This is the "renderer is stuck"
/// escape valve: rather than livelock on fences that will never
/// signal, the drain unconditionally pops + signals every entry so
/// the X11 PRESENT serial bookkeeping doesn't pile up at the loop.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn drain_force_fires_all_pending_on_renderer_failed() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // Enqueue 3 entries against unsignaled fences via a real
    // copy_area path (so each pins a live FenceTicket).
    let src = b.create_pixmap(None, 32, 4, 4).expect("src");
    let cow = b.create_pixmap(None, 32, 4, 4).expect("cow");
    for serial in 1..=3 {
        b.copy_area(None, src.as_raw(), cow.as_raw(), 0, 0, 0, 0, 4, 4)
            .expect("copy_area");
        b.enqueue_present_completion(
            yserver_core::backend::CompletedPresentEvent {
                client_id: yserver_protocol::x11::ClientId(0),
                serial,
                host_xid: src.as_raw(),
                dst_host_xid: cow.as_raw(),
                options: 0,
                present_id: 0,
                window_generation: 0,
                crtc_id: 0,
                crtc_epoch: 0,
                msc_offset: 0,
                completion_clock: None,
                wake: yserver_core::backend::PresentWake::Pixmap { idle_fence_xid: 0 },
                completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
                emit_idle: true,
            },
            cow.as_raw(),
        );
    }
    assert_eq!(
        b.pending_present_events_len_for_tests(),
        3,
        "three entries queued before drain",
    );

    // Force-fire branch: flip renderer_failed; drain returns all
    // entries unconditionally.
    b.set_renderer_failed_for_tests(true);
    let drained = b.drain_completed_present_events_for_tests();
    assert_eq!(drained.len(), 3, "force-fire returns all 3 entries",);
    assert_eq!(
        b.pending_present_events_len_for_tests(),
        0,
        "force-fire empties the queue",
    );
}

/// Stage 5 Task 6.1 site #1 (Task 12 of the deferred-PRESENT plan)
/// — verifies that the v2 backend's `enqueue_present_completion`
/// returns quickly (i.e. does *not* synchronously wait on the
/// underlying fence). This is the load-bearing property the
/// `PRESENT::Pixmap` handler now relies on: the synchronous
/// `wait_for_drawable_idle` has been replaced by an enqueue that
/// must hand control back to the main loop without blocking.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn present_pixmap_enqueues_pending_and_defers_emission() {
    use yserver_core::backend::{CompletedPresentEvent, PresentWake};
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let src_pix = b.create_pixmap(None, 32, 4, 4).expect("src pixmap");
    let cow_pix = b.create_pixmap(None, 32, 4, 4).expect("cow pixmap");
    b.copy_area(None, src_pix.as_raw(), cow_pix.as_raw(), 0, 0, 0, 0, 4, 4)
        .expect("copy_area");
    let before = std::time::Instant::now();
    b.enqueue_present_completion(
        CompletedPresentEvent {
            client_id: yserver_protocol::x11::ClientId(0),
            serial: 1,
            host_xid: src_pix.as_raw(),
            dst_host_xid: cow_pix.as_raw(),
            options: 0,
            present_id: 0,
            window_generation: 0,
            crtc_id: 0,
            crtc_epoch: 0,
            msc_offset: 0,
            completion_clock: None,
            wake: PresentWake::Pixmap { idle_fence_xid: 0 },
            completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
            emit_idle: true,
        },
        cow_pix.as_raw(),
    );
    let elapsed = before.elapsed();
    assert!(
        elapsed.as_millis() < 50,
        "enqueue must be fast (< 50 ms); was {} ms",
        elapsed.as_millis()
    );
    // Drain returns empty since fence isn't signaled yet — but
    // lavapipe completes the small copy synchronously so this may
    // also return Some entries. Either is fine; the load-bearing
    // assertion is the fast-enqueue time.
    let _drained = b.drain_completed_present_events_for_tests();
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn present_pixmap_synced_enqueues_with_release_syncobj_wake() {
    use yserver_core::backend::{CompletedPresentEvent, PresentWake};
    #[derive(Debug)]
    struct UnusedSyncobj;
    impl yserver_core::backend::SyncobjHandle for UnusedSyncobj {
        fn signal(&self, _value: u64) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let src_pix = b.create_pixmap(None, 32, 4, 4).expect("src");
    let cow_pix = b.create_pixmap(None, 32, 4, 4).expect("cow");
    b.copy_area(None, src_pix.as_raw(), cow_pix.as_raw(), 0, 0, 0, 0, 4, 4)
        .expect("copy");
    b.enqueue_present_completion(
        CompletedPresentEvent {
            client_id: yserver_protocol::x11::ClientId(0),
            serial: 2,
            host_xid: src_pix.as_raw(),
            dst_host_xid: cow_pix.as_raw(),
            options: 0,
            present_id: 0,
            window_generation: 0,
            crtc_id: 0,
            crtc_epoch: 0,
            msc_offset: 0,
            completion_clock: None,
            wake: PresentWake::PixmapSynced {
                release: std::sync::Arc::new(UnusedSyncobj),
                release_syncobj: 0, // 0 = no wake object; just exercises enqueue
                release_value: 42,
            },
            completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
            emit_idle: true,
        },
        cow_pix.as_raw(),
    );
    // Drain may return entries quickly under lavapipe; assertion is
    // that enqueue didn't panic + the queue can be drained.
    let _drained = b.drain_completed_present_events_for_tests();
}

/// Stage 5 Task 6.1 (Task 14 of the deferred-PRESENT plan) — verifies
/// that `disable_output` flushes open cow/render batches, drains the
/// pending PRESENT events queue, and hands the deferred event payloads
/// back via `take_shutdown_present_events` so `lib.rs::run` can fan
/// them out to clients before the socket is torn down.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn disable_output_flushes_pending_batches_before_drain_all() {
    use yserver_core::backend::{CompletedPresentEvent, PresentWake};
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    // Open a cow_batch + enqueue a pending PRESENT entry.
    let src = b.create_pixmap(None, 32, 4, 4).expect("src");
    let cow = b.create_pixmap(None, 32, 4, 4).expect("cow");
    b.copy_area(None, src.as_raw(), cow.as_raw(), 0, 0, 0, 0, 4, 4)
        .expect("copy");
    b.enqueue_present_completion(
        CompletedPresentEvent {
            client_id: yserver_protocol::x11::ClientId(0),
            serial: 1,
            host_xid: src.as_raw(),
            dst_host_xid: cow.as_raw(),
            options: 0,
            present_id: 0,
            window_generation: 0,
            crtc_id: 0,
            crtc_epoch: 0,
            msc_offset: 0,
            completion_clock: None,
            wake: PresentWake::Pixmap { idle_fence_xid: 0 },
            completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
            emit_idle: true,
        },
        cow.as_raw(),
    );

    let pre_pending = b.pending_present_events_len_for_tests();
    assert!(
        pre_pending >= 1,
        "pending events should include the just-enqueued one"
    );

    // Call disable_output. The platform-level KMS commit may fail on
    // the test harness (no real connector); the load-bearing
    // assertions are on the drain + take_shutdown_present_events
    // behaviour, which run before the platform commit.
    let _ = b.disable_output();

    // Post-shutdown: pending queue is empty, take_shutdown_present_events
    // has the deferred event ready to hand to lib.rs::run.
    assert_eq!(
        b.pending_present_events_len_for_tests(),
        0,
        "disable_output empties the pending queue"
    );
    let shutdown_events = b.take_shutdown_present_events();
    assert!(
        !shutdown_events.is_empty(),
        "shutdown should hand at least one event back"
    );
}

/// Phase A T6 regression gate: the non-COW PRESENT enqueue path must
/// call `flush_submit_group(PresentCompletionSignal)` before the
/// signal-only submit, so prior paint CBs are queued before the
/// semaphore signals.
///
/// Setup: create a pixmap, drain setup CBs, issue a fill_rectangle
/// (paint op parks a CB in the SubmitGroup). Then call
/// `enqueue_present_completion` for the same pixmap against a
/// *non-COW* destination (no cow_id set). The flush must happen before
/// the signal-only submit, draining the parked CB.
///
/// Spec § "Phase A — concrete scope" trigger 2.
#[test]
#[ignore = "lavapipe vk"]
fn submit_group_flushes_before_non_cow_present_completion_signal() {
    use yserver_core::backend::{Backend, CompletedPresentEvent, PresentWake};

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // Close the construction frame (init_root_storage's fill keeps a
    // frame — and with it the group ticket — open since B.3 ported
    // fill_rect to the frame builder), then drain buffered CBs.
    if b.frame_builder_is_open_for_tests() {
        b.engine_close_open_frame_for_timeout_for_tests()
            .expect("close construction frame");
    }
    b.engine_flush_submit_group_for_tests()
        .expect("baseline drain");
    assert!(
        !b.platform_submit_group_is_open_for_tests(),
        "baseline: submit group closed after drain"
    );

    // Create a 4×4 depth-32 pixmap that will be the PRESENT destination.
    let dst_pix = b.create_pixmap(None, 32, 4, 4).expect("dst pixmap");
    let dst_xid = dst_pix.as_raw();

    // Drain any CBs from the create_pixmap itself.
    b.engine_flush_submit_group_for_tests()
        .expect("post-create drain");

    // Issue a paint op (fill_rectangle). Since B.3 it records into an
    // open frame-builder frame (deferred CB) rather than parking a
    // one-shot CB directly in the group — either form counts as
    // "paint buffered but not yet on the queue".
    b.fill_rectangle(None, dst_xid, 0xFF0000FF, 0, 0, 4, 4)
        .expect("fill_rectangle");
    assert!(
        b.frame_builder_is_open_for_tests()
            || b.platform_submit_group_size_for_tests() >= 1
            || b.engine_pending_group_ops_count_for_tests() >= 1,
        "paint buffered (open frame or parked CB) before enqueue_present_completion"
    );

    // Invoke the non-COW PRESENT enqueue path.  cow_id is unset on
    // for_tests_with_vk(), so this exercise the non-COW fallback.
    b.enqueue_present_completion(
        CompletedPresentEvent {
            client_id: yserver_protocol::x11::ClientId(0),
            serial: 99,
            host_xid: dst_xid,
            dst_host_xid: dst_xid,
            options: 0,
            present_id: 0,
            window_generation: 0,
            crtc_id: 0,
            crtc_epoch: 0,
            msc_offset: 0,
            completion_clock: None,
            wake: PresentWake::Pixmap { idle_fence_xid: 0 },
            completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
            emit_idle: true,
        },
        dst_xid,
    );

    // After the call the close-frame (B.1 trigger 1b) +
    // PresentCompletionSignal flush must have graduated all deferred
    // paint to `submitted` and closed the group.
    assert!(
        !b.frame_builder_is_open_for_tests(),
        "open frame closed before the signal-only submit (B.1 trigger 1b)"
    );
    assert_eq!(
        b.platform_submit_group_size_for_tests(),
        0,
        "submit group drained by PresentCompletionSignal flush"
    );
    assert_eq!(
        b.engine_pending_group_ops_count_for_tests(),
        0,
        "parked op graduated to submitted before signal-only submit"
    );
}

fn non_cow_present_event(serial: u32, xid: u32) -> yserver_core::backend::CompletedPresentEvent {
    yserver_core::backend::CompletedPresentEvent {
        client_id: yserver_protocol::x11::ClientId(0),
        serial,
        host_xid: xid,
        dst_host_xid: xid,
        options: 0,
        present_id: 0,
        window_generation: 0,
        crtc_id: 0,
        crtc_epoch: 0,
        msc_offset: 0,
        completion_clock: None,
        wake: yserver_core::backend::PresentWake::Pixmap { idle_fence_xid: 0 },
        completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
        emit_idle: true,
    }
}

/// A Vk backend with the construction frame closed and the group drained,
/// plus a `src` and a `dst` 4×4 depth-32 pixmap.
fn present_fixture() -> Option<(KmsBackend, u32, u32)> {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return None;
        }
    };
    let src = b.create_pixmap(None, 32, 4, 4).expect("src").as_raw();
    let dst = b.create_pixmap(None, 32, 4, 4).expect("dst").as_raw();
    if b.frame_builder_is_open_for_tests() {
        b.engine_close_open_frame_for_timeout_for_tests()
            .expect("close construction frame");
    }
    b.engine_flush_submit_group_for_tests().expect("drain");
    Some((b, src, dst))
}

/// The serials delivered once `expected` completions arrived (or 5 s passed).
fn delivered_serials(b: &mut KmsBackend, expected: usize) -> Vec<u32> {
    drain_present_events_until(b, |e| e.len() >= expected)
        .iter()
        .map(|e| e.serial)
        .collect()
}

/// #214: a non-COW Present whose copy is still in the open frame attaches
/// its completion to that frame and closes it — ONE vkQueueSubmit2 (the
/// paint submit carries the export signal), not paint + signal-only — and
/// the completion is still delivered.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn non_cow_present_completion_rides_the_paint_submit() {
    let Some((mut b, src, dst)) = present_fixture() else {
        return;
    };
    b.copy_area(None, src, dst, 0, 0, 0, 0, 4, 4)
        .expect("copy_area");
    assert!(
        b.frame_builder_is_open_for_tests(),
        "the Present copy is recorded into the open frame"
    );
    let before = b.platform_queue_submit_count_for_tests();
    b.enqueue_present_completion(non_cow_present_event(0x214, dst), dst);
    assert_eq!(
        b.platform_queue_submit_count_for_tests() - before,
        1,
        "the completion signal rides the paint submit"
    );
    assert!(
        !b.frame_builder_is_open_for_tests(),
        "the frame closed at once"
    );
    assert_eq!(delivered_serials(&mut b, 1), vec![0x214]);
}

/// #214 fallback: when the open frame does not hold a write to the
/// destination (the copy was already submitted), the completion keeps the
/// signal-only submit after the frame close, and is still delivered.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn non_cow_present_completion_without_the_copy_in_the_frame_keeps_the_signal_submit() {
    let Some((mut b, src, dst)) = present_fixture() else {
        return;
    };
    b.copy_area(None, src, dst, 0, 0, 0, 0, 4, 4)
        .expect("copy_area");
    b.engine_close_open_frame_for_timeout_for_tests()
        .expect("submit the copy");
    // Unrelated paint keeps a frame open that does not write `dst`.
    b.copy_area(None, dst, src, 0, 0, 0, 0, 4, 4)
        .expect("copy_area");
    assert!(b.frame_builder_is_open_for_tests());
    let before = b.platform_queue_submit_count_for_tests();
    b.enqueue_present_completion(non_cow_present_event(0x215, dst), dst);
    assert_eq!(
        b.platform_queue_submit_count_for_tests() - before,
        2,
        "frame close + signal-only submit"
    );
    assert_eq!(delivered_serials(&mut b, 1), vec![0x215]);
}

/// #214: a Present into a non-redirected child of a redirected parent (a
/// client window inside a reparenting WM's redirected frame) copies into
/// the PARENT's backing; the completion must key on that backing too and
/// ride the paint submit, not fall back to a signal-only submit.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn present_into_child_of_redirected_parent_rides_the_paint_submit() {
    use yserver_core::{backend::WindowHandle, host_x11::HostSubwindowVisual};

    let Some((mut b, src, _)) = present_fixture() else {
        return;
    };
    let visual = HostSubwindowVisual::Explicit {
        depth: 32,
        visual_xid: 0,
        colormap_xid: 0,
    };
    let root = WindowHandle::from_raw(1).expect("root");
    let frame = b
        .create_subwindow(None, root, 10, 10, 16, 16, 0, visual, None, None)
        .expect("frame");
    b.map_window_for_tests(frame.as_raw()).expect("map frame");
    let backing = b.create_pixmap(None, 32, 16, 16).expect("frame backing");
    assert!(b.test_set_redirected_target(frame.as_raw(), backing.as_raw()));
    let client = b
        .create_subwindow(None, frame, 2, 2, 8, 8, 0, visual, None, None)
        .expect("client");
    let client_xid = client.as_raw();
    b.map_window_for_tests(client_xid).expect("map client");
    if b.frame_builder_is_open_for_tests() {
        b.engine_close_open_frame_for_timeout_for_tests()
            .expect("close setup frame");
    }
    b.engine_flush_submit_group_for_tests().expect("drain");

    b.copy_area(None, src, client_xid, 0, 0, 0, 0, 4, 4)
        .expect("copy_area");
    assert!(b.frame_builder_is_open_for_tests());
    let before = b.platform_queue_submit_count_for_tests();
    b.enqueue_present_completion(non_cow_present_event(0x216, client_xid), client_xid);
    assert_eq!(
        b.platform_queue_submit_count_for_tests() - before,
        1,
        "the completion signal rides the paint submit into the parent's backing"
    );
    assert!(!b.frame_builder_is_open_for_tests());
    assert_eq!(delivered_serials(&mut b, 1), vec![0x216]);
}

/// #214 failure path: a frame close that fails while recording (before any
/// submit) must not drop the completion attached to it.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn non_cow_present_completion_survives_a_frame_record_failure() {
    let Some((mut b, src, dst)) = present_fixture() else {
        return;
    };
    b.copy_area(None, src, dst, 0, 0, 0, 0, 4, 4)
        .expect("copy_area");
    b.force_next_frame_record_failure_for_tests();
    let before = b.platform_queue_submit_count_for_tests();
    b.enqueue_present_completion(non_cow_present_event(0x216, dst), dst);
    assert_eq!(
        b.platform_queue_submit_count_for_tests() - before,
        0,
        "nothing submitted"
    );
    assert_eq!(
        b.pending_present_events_len_for_tests(),
        1,
        "the entry is queued for delivery, not dropped"
    );
    assert_eq!(delivered_serials(&mut b, 1), vec![0x216]);
}

/// #214 failure path: a frame whose submit fails keeps the completion too.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn non_cow_present_completion_survives_a_frame_submit_failure() {
    let Some((mut b, src, dst)) = present_fixture() else {
        return;
    };
    b.copy_area(None, src, dst, 0, 0, 0, 0, 4, 4)
        .expect("copy_area");
    b.platform_force_next_submit_failure_for_tests();
    b.enqueue_present_completion(non_cow_present_event(0x217, dst), dst);
    assert_eq!(b.pending_present_events_len_for_tests(), 1);
    assert_eq!(delivered_serials(&mut b, 1), vec![0x217]);
}
