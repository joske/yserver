use super::*;
use std::time::Duration;

type HeldVsyncDirectCursor = (
    KmsBackend,
    std::rc::Rc<RecordingAtomicCommitExecutor>,
    yserver_core::server::ServerState,
    std::rc::Rc<super::RecordingCursorShowExecutor>,
    std::rc::Rc<RecordingCursorHideExecutor>,
);

fn held_vsync_direct_scanout_with_transparent_cursor(
    window: u32,
    present_id: u64,
    output_count: usize,
) -> Option<HeldVsyncDirectCursor> {
    use yserver_core::{backend::Backend, server::ServerState};

    let mut backend = live_scene_backend_with_outputs(output_count)?;
    let recorder = recording_sink(&mut backend, 0);
    let (show_executor, hide_executor) = install_cursor_plane_with_successful_show(&mut backend);
    let transparent = create_bitmap_cursor_via_backend(&mut backend, false, false);
    let root = backend.core.window_id;
    Backend::define_cursor(&mut backend, None, root, transparent.as_raw())
        .expect("select transparent root cursor");
    assert!(backend.effective_cursor_hidden());
    assert_eq!(
        backend.scene.cursor_mode(),
        crate::kms::render::scene::CursorPlaneMode::Hidden
    );

    let mut state = ServerState::new();
    Backend::warp_pointer_root(&mut backend, &mut state, 780, 100);
    backend.pstate.scanout_m2.eligible_root_streak =
        crate::kms::render::backend::SCANOUT_M2_ELIGIBLE_ROOT_PROBATION - 1;
    backend.platform.requested_flip_timing = crate::drm::async_flip::FlipTiming::Vsync;
    let (width, height) = backend.platform.fb_dimensions();
    assert!(
        try_direct_present_with_live_storage(&mut backend, present_id, window, width, height, 0,)
            .is_some()
    );
    assert_eq!(recorder.records().len(), 1);
    assert_eq!(recorder.records()[0].kind, AtomicCommitKind::Scene);
    assert!(
        !recorder.records()[0]
            .flags
            .contains(::drm::control::AtomicCommitFlags::PAGE_FLIP_ASYNC)
    );
    retire_direct_present(&mut backend, &mut state, 80);
    assert!(backend.pstate.scanout_m2.active());
    assert!(backend.pstate.scanout_m2.hold_direct);
    assert!(!backend.pstate.scanout_m2.unflip_requested);
    assert!(!backend.platform.any_cursor_plane_visible());
    assert_tearing_off(&backend);

    Some((backend, recorder, state, show_executor, hide_executor))
}

fn install_controlled_scheduler_clock(
    backend: &mut crate::kms::render::backend::KmsBackend,
) -> std::sync::Arc<std::sync::atomic::AtomicU64> {
    use std::{rc::Rc, sync::atomic::Ordering, time::Instant};

    let base = Instant::now();
    let elapsed_ns = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let clock_elapsed_ns = std::sync::Arc::clone(&elapsed_ns);
    backend.for_tests_set_monotonic_clock(Rc::new(move || {
        base + Duration::from_nanos(clock_elapsed_ns.load(Ordering::SeqCst))
    }));
    elapsed_ns
}

fn seed_direct_cursor_positions(
    backend: &mut KmsBackend,
    root_x: i32,
    root_y: i32,
) -> Vec<crate::drm::atomic::CursorPlanePosition> {
    (0..backend.platform.outputs.len())
        .map(|output_idx| {
            let suffix = u32::try_from(output_idx).expect("test output index fits");
            backend.platform.for_tests_seed_atomic_cursor_position(
                output_idx,
                raw(0xb100 + suffix),
                raw(0xb200 + suffix),
                raw(0xb300 + suffix),
                root_x,
                root_y,
                0,
                0,
            )
        })
        .collect()
}

fn assert_recorded_cursor_position(
    record: &crate::drm::atomic::tests_support::AtomicCommitRecord,
    position: crate::drm::atomic::CursorPlanePosition,
) {
    assert_eq!(record.kind, AtomicCommitKind::CursorOnly);
    for (property, value) in [
        (position.crtc_x, i64::from(position.x) as u64),
        (position.crtc_y, i64::from(position.y) as u64),
    ] {
        assert!(
            record.properties.iter().any(|update| {
                update.object == position.plane.into()
                    && update.property == property
                    && update.value == value
            }),
            "cursor position commit omitted {property:?}={value} on plane {:?}: {:?}",
            position.plane,
            record.properties,
        );
    }
}

fn move_held_direct_cursor(
    backend: &mut KmsBackend,
    state: &mut yserver_core::server::ServerState,
    recorder: &std::rc::Rc<RecordingAtomicCommitExecutor>,
    root_x: i32,
    root_y: i32,
    frame: u32,
) {
    use yserver_core::backend::Backend;

    let outputs: Vec<_> = (0..backend.platform.outputs.len()).collect();
    Backend::warp_pointer_root(backend, state, root_x, root_y);
    let positions = backend
        .platform
        .cursor_plane_atomic_positions_for_outputs(&outputs);
    assert_eq!(positions.len(), outputs.len());
    for position in &positions {
        let output = backend
            .platform
            .outputs
            .iter()
            .find(|output| output.output.crtc == position.crtc)
            .expect("cursor CRTC belongs to a test output");
        assert_eq!(position.x, root_x - output.x);
        assert_eq!(position.y, root_y - output.y);
    }

    let before = recorder.records().len();
    Backend::maybe_composite(backend).expect("schedule bound-plane cursor positions");
    let records = recorder.records();
    assert_eq!(records.len(), before + outputs.len());
    for position in &positions {
        let record = records[before..]
            .iter()
            .find(|record| {
                record.properties.iter().any(|update| {
                    update.object == position.plane.into() && update.property == position.crtc_x
                })
            })
            .expect("one cursor-only commit carries each output position");
        assert_recorded_cursor_position(record, *position);
    }
    assert!(
        !backend.scene.has_pending_page_flips(),
        "pointer motion must not queue a scene repaint behind held direct scanout"
    );
    assert!(backend.pstate.scanout_m2.active());
    assert!(backend.pstate.scanout_m2.hold_direct);
    assert!(!backend.pstate.scanout_m2.unflip_requested);
    retire_recorded_direct_flip(backend, state, frame);
    assert!(
        !backend
            .platform
            .cursor_commit_in_flight_for_outputs(&outputs)
    );
    for position in &positions {
        assert!(
            !backend
                .platform
                .for_tests_atomic_position_is_unsent(position.device_key, position.crtc,)
        );
    }
}

fn finish_held_direct_cursor_test(
    backend: &mut KmsBackend,
    state: &mut yserver_core::server::ServerState,
    recorder: &std::rc::Rc<RecordingAtomicCommitExecutor>,
    window: u32,
    first_frame: u32,
    visible_cursor_output: Option<usize>,
) {
    use yserver_core::backend::Backend;

    Backend::unmap_subwindow(backend, None, window).expect("unmap direct-frame window");
    assert!(backend.pstate.scanout_m2.unflip_requested);
    assert!(!backend.pstate.scanout_m2.hold_direct);
    let before_unflip = recorder.records().len();
    Backend::maybe_composite(backend).expect("submit composed replacement for direct scanout");
    let records = recorder.records();
    assert_eq!(records.len(), before_unflip + 1);
    assert_eq!(
        records[before_unflip].kind,
        if visible_cursor_output.is_some() {
            AtomicCommitKind::SceneWithCursor
        } else {
            AtomicCommitKind::Scene
        },
        "the grouped unflip carries the bound cursor position when visible"
    );
    assert_eq!(
        backend.pstate.scanout_m2.unflip_outputs_to_replace,
        (0..backend.platform.outputs.len()).collect()
    );
    retire_composed_frame(backend, state, first_frame);
    assert!(!backend.pstate.scanout_m2.active());

    let mut frame = first_frame + 1;
    for _ in 0..4 {
        Backend::maybe_composite(backend).expect("compose after direct unflip retirement");
        if backend.scene.has_pending_page_flips() {
            retire_composed_frame(backend, state, frame);
            frame += 1;
        } else if !backend.scene.scene_structure_dirty {
            break;
        }
    }

    assert!(!backend.scene.has_pending_page_flips());
    assert!(!backend.scene.scene_structure_dirty);
    assert!(!backend.for_tests_scene_wants_scene());
    assert!(!backend.pstate.scanout_m2.active());
    assert!(!backend.pstate.scanout_m2.hold_direct);
    assert!(!backend.pstate.scanout_m2.unflip_requested);
    assert!(
        backend
            .pstate
            .scanout_m2
            .unflip_outputs_to_replace
            .is_empty()
    );
    assert!(!backend.pstate.scanout_m2.reentry_blocked_until_composed);
    assert_direct_queue_drained(backend);
    assert_tearing_off(backend);

    let outputs: Vec<_> = (0..backend.platform.outputs.len()).collect();
    assert!(
        !backend
            .platform
            .cursor_commit_in_flight_for_outputs(&outputs)
    );
    for output_idx in &outputs {
        let visible = visible_cursor_output == Some(*output_idx);
        assert_eq!(
            backend
                .platform
                .cursor_plane_visible_for_output(*output_idx),
            visible,
            "the composed scene and physical cursor plane agree on output {output_idx}"
        );
        assert_eq!(
            backend
                .scene
                .for_tests_cursor_footprint_was_presented(*output_idx),
            visible,
            "only the output with the visible plane retains a cursor footprint"
        );
    }
    assert_eq!(
        backend.scene.cursor_mode(),
        if visible_cursor_output.is_some() {
            crate::kms::render::scene::CursorPlaneMode::Hw
        } else {
            crate::kms::render::scene::CursorPlaneMode::Hidden
        }
    );
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: leave the scene Hidden after DefineCursor rebind, so Hw|Mixed-only routing misses motion.
fn define_cursor_rebind_routes_motion_to_bound_plane_during_vsync_direct_scanout() {
    use yserver_core::backend::Backend;

    const WINDOW: u32 = 0x78c0;
    let Some((mut backend, recorder, mut state, show_executor, hide_executor)) =
        held_vsync_direct_scanout_with_transparent_cursor(WINDOW, 15_400, 2)
    else {
        return;
    };
    let visible = create_bitmap_cursor_via_backend(&mut backend, true, true);
    let root = backend.core.window_id;
    Backend::define_cursor(&mut backend, None, root, visible.as_raw())
        .expect("select opaque cursor while direct scanout is held");

    assert!(!backend.effective_cursor_hidden());
    assert_eq!(
        backend.scene.cursor_mode(),
        crate::kms::render::scene::CursorPlaneMode::Hw,
        "the scene records the plane rebind before routing motion"
    );
    assert!(backend.platform.any_cursor_plane_visible());
    assert!(backend.pstate.scanout_m2.active());
    assert!(backend.pstate.scanout_m2.hold_direct);
    assert!(!backend.pstate.scanout_m2.unflip_requested);
    assert!(show_executor.calls().len() >= 2);
    assert!(hide_executor.calls().is_empty());

    let positions = seed_direct_cursor_positions(&mut backend, 780, 100);
    assert_eq!(positions.len(), 2);
    for (frame, x) in [790, 795, 805, 810, 815].into_iter().enumerate() {
        move_held_direct_cursor(
            &mut backend,
            &mut state,
            &recorder,
            x,
            100,
            81 + u32::try_from(frame).expect("frame index fits"),
        );
    }

    finish_held_direct_cursor_test(&mut backend, &mut state, &recorder, WINDOW, 90, Some(1));
    // The unflip's fresh scene frames retire the scene's cursor state: the
    // plane bound on output 0 during direct scanout is hidden there.
    assert_eq!(
        hide_executor.calls(),
        [backend.platform.outputs[0].output.crtc]
    );
    let mut expected = vec![AtomicCommitKind::Scene];
    expected.extend([AtomicCommitKind::CursorOnly; 10]);
    expected.push(AtomicCommitKind::SceneWithCursor);
    expected.extend([AtomicCommitKind::Scene, AtomicCommitKind::SceneWithCursor]);
    assert_commit_sequence(&recorder, &expected);
    backend.platform.wait_idle_bounded();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: XFIXES ShowCursor rebinds the plane but leaves the scene Hidden.
fn xfixes_show_rebind_routes_motion_and_transparent_roundtrip_during_vsync_direct_scanout() {
    use yserver_core::backend::Backend;

    const WINDOW: u32 = 0x78c1;
    let Some((mut backend, recorder, mut state, show_executor, hide_executor)) =
        held_vsync_direct_scanout_with_transparent_cursor(WINDOW, 15_500, 2)
    else {
        return;
    };

    Backend::set_cursor_hidden(&mut backend, true);
    assert!(backend.effective_cursor_hidden());
    assert_eq!(
        backend.scene.cursor_mode(),
        crate::kms::render::scene::CursorPlaneMode::Hidden
    );
    assert!(!backend.platform.any_cursor_plane_visible());

    let visible = create_bitmap_cursor_via_backend(&mut backend, true, true);
    let root = backend.core.window_id;
    Backend::define_cursor(&mut backend, None, root, visible.as_raw())
        .expect("define opaque cursor while XFIXES-hidden");
    assert!(backend.effective_cursor_hidden());
    assert_eq!(
        backend.scene.cursor_mode(),
        crate::kms::render::scene::CursorPlaneMode::Hidden
    );

    Backend::set_cursor_hidden(&mut backend, false);
    assert!(!backend.effective_cursor_hidden());
    assert_eq!(
        backend.scene.cursor_mode(),
        crate::kms::render::scene::CursorPlaneMode::Hw,
        "ShowCursor records the plane that it rebound"
    );
    assert!(backend.platform.any_cursor_plane_visible());
    assert!(backend.pstate.scanout_m2.active());
    assert!(backend.pstate.scanout_m2.hold_direct);
    assert!(!backend.pstate.scanout_m2.unflip_requested);
    assert_eq!(show_executor.calls().len(), 2);
    assert!(hide_executor.calls().is_empty());

    let positions = seed_direct_cursor_positions(&mut backend, 780, 100);
    assert_eq!(positions.len(), 2);
    for (frame, x) in [790, 795, 805, 810, 815].into_iter().enumerate() {
        move_held_direct_cursor(
            &mut backend,
            &mut state,
            &recorder,
            x,
            100,
            91 + u32::try_from(frame).expect("frame index fits"),
        );
    }

    let transparent = create_bitmap_cursor_via_backend(&mut backend, false, false);
    Backend::define_cursor(&mut backend, None, root, transparent.as_raw())
        .expect("switch back to a fully transparent cursor while held");
    assert!(backend.effective_cursor_hidden());
    assert_eq!(
        backend.scene.cursor_mode(),
        crate::kms::render::scene::CursorPlaneMode::Hidden
    );
    assert!(!backend.platform.any_cursor_plane_visible());
    let before_hidden_motion = recorder.records().len();
    Backend::warp_pointer_root(&mut backend, &mut state, 820, 100);
    Backend::maybe_composite(&mut backend).expect("transparent cursor motion is a no-op");
    assert_eq!(recorder.records().len(), before_hidden_motion);
    assert!(
        backend
            .platform
            .cursor_plane_atomic_positions_for_outputs(&[0, 1])
            .is_empty()
    );
    assert!(!backend.scene.has_pending_page_flips());

    finish_held_direct_cursor_test(&mut backend, &mut state, &recorder, WINDOW, 100, None);
    let mut expected = vec![AtomicCommitKind::Scene];
    expected.extend([AtomicCommitKind::CursorOnly; 10]);
    expected.push(AtomicCommitKind::Scene);
    expected.extend([AtomicCommitKind::Scene; 2]);
    assert_commit_sequence(&recorder, &expected);
    backend.platform.wait_idle_bounded();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: let generic request mark_dirty calls force ordinary scene wakes while tearing.
fn task6_present_and_xtest_motion_keep_cursor_compose_cap_under_run_core_load() {
    // Exercise eager flip delivery and the reviewer's delayed-until-input-end ordering.
    if !run_task6_present_and_xtest_motion_keep_cursor_compose_cap_under_run_core_load(false) {
        return;
    }
    assert!(run_task6_present_and_xtest_motion_keep_cursor_compose_cap_under_run_core_load(true));
}

fn run_task6_present_and_xtest_motion_keep_cursor_compose_cap_under_run_core_load(
    delay_fake_flip_delivery_until_input_end: bool,
) -> bool {
    use std::{
        collections::VecDeque,
        io::{self, Read},
        os::fd::{AsFd as _, AsRawFd as _, OwnedFd},
        rc::Rc,
        sync::{
            Arc, Condvar, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        thread,
        time::{Duration, Instant},
    };
    use yserver_core::{
        backend::Backend,
        core_loop::{
            CoreSender, Message, ResetPolicy, auth::AuthState, channel,
            poll_tokens::ClientIdAllocator, run_core,
        },
        server::{BackendCapabilities, ServerState},
    };
    use yserver_protocol::x11::{
        ClientId, CreatePixmapRequest, RequestHeader, ResourceId, SequenceNumber,
    };

    const WINDOW: ResourceId = ResourceId(0x0100_7d80);
    const PIXMAP: ResourceId = ResourceId(0x0100_7d81);
    const PIXMAP_ALTERNATE: ResourceId = ResourceId(0x0100_7d82);

    type SharedFlips =
        Arc<Mutex<VecDeque<crate::kms::render::platform::flip_events::KernelPageFlip>>>;

    #[derive(Default)]
    struct CommitProgress {
        kinds: Vec<AtomicCommitKind>,
        drained: usize,
    }

    struct SharedCommitRecorder {
        progress: Mutex<CommitProgress>,
        changed: Condvar,
    }

    struct ProducerShutdownOnDrop {
        sender: CoreSender,
        reader_stop: Arc<AtomicBool>,
        active: bool,
    }

    impl ProducerShutdownOnDrop {
        fn shutdown(&mut self) {
            self.reader_stop.store(true, Ordering::SeqCst);
            let _ = self.sender.send(Message::Shutdown);
            self.active = false;
        }
    }

    impl Drop for ProducerShutdownOnDrop {
        fn drop(&mut self) {
            if self.active {
                self.reader_stop.store(true, Ordering::SeqCst);
                let _ = self.sender.send(Message::Shutdown);
            }
        }
    }

    fn wait_for_commit_progress<'a>(
        commits: &'a SharedCommitRecorder,
        deadline: Instant,
        description: &str,
        predicate: impl Fn(&CommitProgress) -> bool,
    ) -> std::sync::MutexGuard<'a, CommitProgress> {
        let mut progress = commits.progress.lock().expect("commit progress");
        while !predicate(&progress) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let accepted = progress.kinds.len();
                let drained = progress.drained;
                drop(progress);
                panic!(
                    "timed out waiting for {description}: accepted={accepted} drained={drained}"
                );
            }
            let (next, timeout) = commits
                .changed
                .wait_timeout(progress, remaining)
                .expect("commit progress wait");
            progress = next;
            if timeout.timed_out() && !predicate(&progress) {
                let accepted = progress.kinds.len();
                let drained = progress.drained;
                drop(progress);
                panic!(
                    "timed out waiting for {description}: accepted={accepted} drained={drained}"
                );
            }
        }
        progress
    }

    struct RunCoreFlipSource {
        flips: SharedFlips,
        commits: Arc<SharedCommitRecorder>,
    }

    impl crate::kms::render::platform::flip_events::PageFlipEventSource for RunCoreFlipSource {
        fn drain_events(
            &self,
            device: &crate::drm::Device,
        ) -> io::Result<(
            Vec<crate::kms::render::platform::flip_events::KernelPageFlip>,
            Vec<crate::kms::render::platform::flip_events::KernelCrtcSequence>,
        )> {
            let fd = device.as_fd().as_raw_fd();
            loop {
                let mut poll_fd = libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: `poll_fd` is one initialized record for this readiness check.
                let ready = unsafe { libc::poll(&mut poll_fd, 1, 0) };
                if ready <= 0 || poll_fd.revents & libc::POLLIN == 0 {
                    break;
                }
                let mut wake_bytes = [0_u8; 64];
                // SAFETY: `wake_bytes` is writable for its full length; the test DRM socket is
                // nonblocking and readable only after the driver has queued flip records.
                let read =
                    unsafe { libc::read(fd, wake_bytes.as_mut_ptr().cast(), wake_bytes.len()) };
                if read <= 0 {
                    break;
                }
            }
            let flips: Vec<_> = self.flips.lock().expect("flip queue").drain(..).collect();
            if !flips.is_empty() {
                let mut progress = self.commits.progress.lock().expect("commit progress");
                progress.drained += flips.len();
                self.commits.changed.notify_all();
            }
            Ok((flips, Vec::new()))
        }

        fn drain_ready_events(
            &self,
            device: &crate::drm::Device,
        ) -> io::Result<
            Option<(
                Vec<crate::kms::render::platform::flip_events::KernelPageFlip>,
                Vec<crate::kms::render::platform::flip_events::KernelCrtcSequence>,
            )>,
        > {
            if self.flips.lock().expect("flip queue").is_empty() {
                return Ok(None);
            }
            self.drain_events(device).map(Some)
        }
    }

    struct RunCoreCommitExecutor(Arc<SharedCommitRecorder>);

    impl crate::drm::atomic::AtomicCommitExecutor for RunCoreCommitExecutor {
        fn atomic_commit(
            &self,
            _device: &crate::drm::Device,
            _flags: ::drm::control::AtomicCommitFlags,
            kind: AtomicCommitKind,
            _request: crate::drm::atomic::AtomicCommitRequest,
            _events: &[crate::drm::atomic::AtomicCommitEvent],
        ) -> io::Result<()> {
            let mut progress = self.0.progress.lock().expect("commit progress");
            progress.kinds.push(kind);
            self.0.changed.notify_all();
            Ok(())
        }
    }

    let Some(mut backend) = live_scene_backend() else {
        return false;
    };
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
    let pixmap = Backend::create_pixmap(&mut backend, None, 24, screen_width, screen_height)
        .expect("create Present source pixmap");
    Backend::fill_rectangle(
        &mut backend,
        None,
        pixmap.as_raw(),
        0xFF40_6080,
        0,
        0,
        screen_width,
        screen_height,
    )
    .expect("paint Present source pixmap");
    let alternate_pixmap =
        Backend::create_pixmap(&mut backend, None, 24, screen_width, screen_height)
            .expect("create alternate Present source pixmap");
    Backend::fill_rectangle(
        &mut backend,
        None,
        alternate_pixmap.as_raw(),
        0xFF80_4020,
        0,
        0,
        screen_width,
        screen_height,
    )
    .expect("paint alternate Present source pixmap");
    state.resources.create_pixmap(
        ClientId(1),
        CreatePixmapRequest {
            depth: 24,
            pixmap: PIXMAP,
            drawable: WINDOW,
            width: screen_width,
            height: screen_height,
        },
    );
    assert!(state.resources.set_pixmap_host_xid(PIXMAP, pixmap));
    state.resources.create_pixmap(
        ClientId(1),
        CreatePixmapRequest {
            depth: 24,
            pixmap: PIXMAP_ALTERNATE,
            drawable: WINDOW,
            width: screen_width,
            height: screen_height,
        },
    );
    assert!(
        state
            .resources
            .set_pixmap_host_xid(PIXMAP_ALTERNATE, alternate_pixmap)
    );
    enter_tearing_without_direct_scanout_for_state_ids(
        &mut backend,
        &mut state,
        WINDOW.0,
        stage_host,
        stage_host,
        16_000,
    );
    recorder.clear_records();
    assert!(backend.pstate.tearing.is_on());
    assert_eq!(
        backend.scene.cursor_mode(),
        crate::kms::render::scene::CursorPlaneMode::Sw
    );
    assert!(!backend.scene.has_pending_page_flips());

    let refresh_interval = backend
        .pstate
        .tearing
        .cursor_compose_interval(&backend.platform);
    let (device_stream, event_peer) =
        std::os::unix::net::UnixStream::pair().expect("DRM test socket");
    device_stream
        .set_nonblocking(true)
        .expect("nonblocking test DRM endpoint");
    let device = crate::drm::Device::from_inherited_kms_fd(
        OwnedFd::from(device_stream),
        "run_core tearing test DRM socket",
    );
    backend.platform.devices[0].device = Rc::new(device);

    let flips: SharedFlips = Arc::new(Mutex::new(VecDeque::new()));
    let commits = Arc::new(SharedCommitRecorder {
        progress: Mutex::new(CommitProgress::default()),
        changed: Condvar::new(),
    });
    let allow_fake_flip_delivery =
        Arc::new(AtomicBool::new(!delay_fake_flip_delivery_until_input_end));
    backend.platform.devices[0].page_flip_event_source = Rc::new(RunCoreFlipSource {
        flips: Arc::clone(&flips),
        commits: Arc::clone(&commits),
    });
    backend.platform.devices[0].test_page_flip_event_source = None;

    backend.platform.devices[0].atomic_commit_sink = Rc::new(AtomicCommitSink::new(Rc::new(
        RunCoreCommitExecutor(Arc::clone(&commits)),
    )));
    backend.platform.devices[0]
        .atomic_commit_sink
        .set_tearing_on(true);
    backend.pstate.tearing.last_cursor_compose = None;
    backend.pstate.tearing.deferred_cursor_wake = None;
    let scheduler_time_ns = install_controlled_scheduler_clock(&mut backend);
    recorder.clear_records();

    let mpv_peer = install_test_core_client(&mut state);
    let _xtest_peer = install_test_core_client_as(&mut state, ClientId(2));
    let mut endstate_peer = install_test_core_client_as(&mut state, ClientId(3));
    endstate_peer
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("bound core round-trip waits");
    state
        .clients
        .get_mut(&ClientId(1).0)
        .expect("Present client")
        .event_masks
        .insert(WINDOW, 0x0000_0040); // PointerMotionMask

    let (poll, sender, receiver) = channel().expect("core loop channel");
    let request_sender = sender.bind();
    let reaction_sender = sender.bind();
    let shutdown_sender = sender.clone_handle();
    let client_sequence = Arc::new(Mutex::new(1_u16));
    let reaction_sequence = Arc::clone(&client_sequence);
    let motion_notifications = Arc::new(AtomicUsize::new(0));
    let reactions = Arc::new(AtomicUsize::new(0));
    let motion_progress = Arc::new((Mutex::new(0_usize), Condvar::new()));
    let reader_notifications = Arc::clone(&motion_notifications);
    let reader_reactions = Arc::clone(&reactions);
    let reader_motion_progress = Arc::clone(&motion_progress);
    let reader_stop = Arc::new(AtomicBool::new(false));
    let reader_stop_thread = Arc::clone(&reader_stop);
    mpv_peer
        .set_read_timeout(Some(Duration::from_millis(20)))
        .expect("set synthetic mpv event timeout");
    let mpv_reader = thread::spawn(move || {
        let mut peer = mpv_peer;
        let mut event = [0_u8; 32];
        loop {
            match peer.read_exact(&mut event) {
                Ok(()) if event[0] & 0x7f == 6 => {
                    let mut sequence = reaction_sequence.lock().expect("mpv sequence");
                    reaction_sender
                        .send(Message::Request {
                            id: ClientId(1),
                            sequence: SequenceNumber(*sequence),
                            accepted_at: None,
                            header: RequestHeader {
                                opcode: 127,
                                data: 0,
                                length_units: 1,
                            },
                            body: Vec::new(),
                            attached_fd: None,
                        })
                        .expect("queue mpv response to MotionNotify");
                    *sequence = (*sequence).wrapping_add(1);
                    reader_reactions.fetch_add(1, Ordering::SeqCst);
                    reader_notifications.fetch_add(1, Ordering::SeqCst);
                    let (count, changed) = &*reader_motion_progress;
                    *count.lock().expect("MotionNotify progress") += 1;
                    changed.notify_all();
                }
                Ok(()) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    if reader_stop_thread.load(Ordering::SeqCst) {
                        return;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return,
                Err(error) => panic!("read synthetic mpv events: {error}"),
            }
        }
    });

    let producer_commits = Arc::clone(&commits);
    let expected_motion_notifications = Arc::clone(&motion_notifications);
    let producer_motion_progress = Arc::clone(&motion_progress);
    let producer_scheduler_time_ns = Arc::clone(&scheduler_time_ns);
    let producer_allow_fake_flip_delivery = Arc::clone(&allow_fake_flip_delivery);
    let producer_reader_stop = Arc::clone(&reader_stop);
    let producer_client_sequence = Arc::clone(&client_sequence);
    let motion_duration = refresh_interval * 7;
    let motion_duration_ns = u64::try_from(motion_duration.as_nanos()).unwrap();
    let refresh_interval_ns = u64::try_from(refresh_interval.as_nanos()).unwrap();
    let refresh_windows = motion_duration
        .as_nanos()
        .div_ceil(refresh_interval.as_nanos())
        .max(1);
    let producer = thread::spawn(move || {
        let mut shutdown_on_drop = ProducerShutdownOnDrop {
            sender: shutdown_sender,
            reader_stop: Arc::clone(&producer_reader_stop),
            active: true,
        };
        let client_id = ClientId(1);
        let xtest_client_id = ClientId(2);
        let present_interval_ns = 1_000_000_000_u64 / 130;
        let mut next_present_ns = 0_u64;
        let mut xtest_sequence = 1_u16;
        let mut barrier_sequence = 1_u16;
        let mut sent_motion = 0_usize;
        let mut sent_presents = 0_usize;
        let mut final_pointer_position = (0_i32, 0_i32);
        let mut core_barrier = |peer: &mut std::os::unix::net::UnixStream| {
            request_sender
                .send(Message::Request {
                    id: ClientId(3),
                    sequence: SequenceNumber(barrier_sequence),
                    accepted_at: None,
                    header: RequestHeader {
                        opcode: 43, // GetInputFocus: a state-free run_core round-trip.
                        data: 0,
                        length_units: 1,
                    },
                    body: Vec::new(),
                    attached_fd: None,
                })
                .expect("queue core round-trip barrier");
            barrier_sequence = barrier_sequence.wrapping_add(1);
            let mut reply = [0_u8; 32];
            peer.read_exact(&mut reply)
                .expect("read core round-trip barrier reply");
            assert_eq!(reply[0], 1, "GetInputFocus barrier gets an X11 reply");
        };
        let wait_for_motion = |expected: usize| {
            let (count, changed) = &*producer_motion_progress;
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut count = count.lock().expect("MotionNotify progress");
            while *count < expected {
                let remaining = deadline.saturating_duration_since(Instant::now());
                assert!(!remaining.is_zero(), "timed out waiting for MotionNotify");
                let (next, timeout) = changed
                    .wait_timeout(count, remaining)
                    .expect("MotionNotify barrier wait");
                count = next;
                assert!(!timeout.timed_out() || *count >= expected);
            }
        };
        let mut now_ns = 0_u64;
        while now_ns < motion_duration_ns {
            producer_scheduler_time_ns.store(now_ns, Ordering::SeqCst);
            let step = i16::try_from(sent_motion).expect("test pointer step fits");
            let x = 100 + step.rem_euclid(i16::try_from(screen_width - 100).unwrap());
            let y = 100
                + step
                    .wrapping_mul(7)
                    .rem_euclid(i16::try_from(screen_height - 100).unwrap());
            final_pointer_position = (i32::from(x), i32::from(y));
            let mut body = vec![0_u8; 32];
            body[0] = 6; // MotionNotify, as sent by xdotool through XTEST.
            body[20..22].copy_from_slice(&x.to_le_bytes());
            body[22..24].copy_from_slice(&y.to_le_bytes());
            request_sender
                .send(Message::Request {
                    id: xtest_client_id,
                    sequence: SequenceNumber(xtest_sequence),
                    accepted_at: None,
                    header: RequestHeader {
                        opcode: 146,
                        data: 2,
                        length_units: 8,
                    },
                    body,
                    attached_fd: None,
                })
                .expect("queue XTEST FakeInput motion request");
            xtest_sequence = xtest_sequence.wrapping_add(1);
            sent_motion += 1;

            if now_ns >= next_present_ns {
                let mut present_body = vec![0_u8; 68];
                present_body[0..4].copy_from_slice(&WINDOW.0.to_le_bytes());
                let pixmap = [PIXMAP, PIXMAP_ALTERNATE][sent_presents & 1];
                present_body[4..8].copy_from_slice(&pixmap.0.to_le_bytes());
                present_body[8..12]
                    .copy_from_slice(&u32::try_from(sent_presents + 1).unwrap().to_le_bytes());
                present_body[36..40].copy_from_slice(
                    &yserver_core::present_scheduler::PRESENT_OPTION_ASYNC.to_le_bytes(),
                );
                let mut sequence = producer_client_sequence.lock().expect("Present sequence");
                request_sender
                    .send(Message::Request {
                        id: client_id,
                        sequence: SequenceNumber(*sequence),
                        accepted_at: None,
                        header: RequestHeader {
                            opcode: 145,
                            data: yserver_protocol::x11::present::PIXMAP,
                            length_units: 18,
                        },
                        body: present_body,
                        attached_fd: None,
                    })
                    .expect("queue PresentPixmap request");
                *sequence = (*sequence).wrapping_add(1);
                sent_presents += 1;
                next_present_ns = next_present_ns.saturating_add(present_interval_ns);
            }
            // Each barrier makes run_core consume this virtual 3 ms step
            // before the next one. The observed MotionNotify and its client
            // reaction form the second barrier, so neither scheduling speed
            // nor sleeps decide when the next virtual deadline is reached.
            core_barrier(&mut endstate_peer);
            wait_for_motion(sent_motion);
            core_barrier(&mut endstate_peer);
            now_ns = now_ns.saturating_add(3_000_000);
        }

        if delay_fake_flip_delivery_until_input_end {
            producer_allow_fake_flip_delivery.store(true, Ordering::SeqCst);
            let _progress = producer_commits.progress.lock().expect("commit progress");
            producer_commits.changed.notify_all();
        }

        let observed_motion = expected_motion_notifications.load(Ordering::SeqCst);

        // Drain work from the input stream and observe two unchanged commit
        // snapshots before advancing the cursor deadline. These round-trips
        // settle earlier iteration tails before the post-deadline quiescence
        // barrier.
        let settle_deadline = Instant::now() + Duration::from_secs(10);
        let mut settled_commit_count = None;
        loop {
            assert!(
                !settle_deadline
                    .saturating_duration_since(Instant::now())
                    .is_zero(),
                "timed out settling pre-deadline fake flips"
            );
            core_barrier(&mut endstate_peer);
            core_barrier(&mut endstate_peer);
            let progress = wait_for_commit_progress(
                &producer_commits,
                settle_deadline,
                "pre-deadline fake flip retirements",
                |progress| progress.drained >= progress.kinds.len(),
            );
            let accepted = progress.kinds.len();
            drop(progress);
            if settled_commit_count == Some(accepted) {
                break;
            }
            settled_commit_count = Some(accepted);
        }
        // The delayed-flip variant releases its fake events only after the
        // input stream ends. Advance past the cursor deadline, then wait for
        // quiescence: every accepted commit's event is drained, and two
        // unchanged run_core round-trips let the deadline wake and any work it
        // creates settle. A settling Present may already include the final
        // cursor position, so quiescence does not require another submission.
        producer_scheduler_time_ns.store(
            motion_duration_ns.saturating_add(refresh_interval_ns * 2),
            Ordering::SeqCst,
        );
        let quiescence_deadline = Instant::now() + Duration::from_secs(10);
        let mut stable_commit_count = None;
        loop {
            core_barrier(&mut endstate_peer);
            core_barrier(&mut endstate_peer);
            let progress = wait_for_commit_progress(
                &producer_commits,
                quiescence_deadline,
                "post-deadline quiescence (all accepted fake flips drained)",
                |progress| progress.drained == progress.kinds.len(),
            );
            let accepted = progress.kinds.len();
            let drained = progress.drained;
            drop(progress);
            assert_eq!(
                accepted, drained,
                "every recorded commit's fake flip event is retired before Shutdown"
            );
            if stable_commit_count == Some(accepted) {
                break;
            }
            stable_commit_count = Some(accepted);
        }
        shutdown_on_drop.shutdown();
        (
            sent_motion,
            sent_presents,
            observed_motion,
            final_pointer_position,
            motion_duration,
        )
    });

    let event_flips = Arc::clone(&flips);
    let event_commits = Arc::clone(&commits);
    let event_allow_fake_flip_delivery = Arc::clone(&allow_fake_flip_delivery);
    let event_peer = event_peer;
    let event_stop = Arc::new(AtomicBool::new(false));
    let event_stop_thread = Arc::clone(&event_stop);
    let crtc = backend.platform.outputs[0].output.crtc;
    let event_thread = thread::spawn(move || {
        use std::io::Write;
        let mut peer = event_peer;
        let mut sent_events = 0_usize;
        loop {
            let mut progress = event_commits.progress.lock().expect("commit progress");
            while (progress.kinds.len() == sent_events
                || !event_allow_fake_flip_delivery.load(Ordering::SeqCst))
                && !event_stop_thread.load(Ordering::SeqCst)
            {
                let (next, _) = event_commits
                    .changed
                    .wait_timeout(progress, Duration::from_millis(20))
                    .expect("commit notification wait");
                progress = next;
            }
            if progress.kinds.len() == sent_events && event_stop_thread.load(Ordering::SeqCst) {
                return;
            }
            let newly_accepted = progress.kinds.len() - sent_events;
            sent_events = progress.kinds.len();
            drop(progress);
            for index in 0..newly_accepted {
                let frame = u32::try_from(sent_events - newly_accepted + index + 1)
                    .expect("test frame fits");
                event_flips.lock().expect("flip queue").push_back(
                    crate::kms::render::platform::flip_events::KernelPageFlip {
                        crtc,
                        frame,
                        commit_id: 0,
                        ust: Duration::ZERO,
                        event_at_monotonic: crate::drm::atomic::monotonic_timestamp(),
                        event_at_boottime: crate::drm::atomic::boottime_timestamp(),
                    },
                );
                peer.write_all(&[1]).expect("signal fake DRM flip event");
            }
        }
    });

    let build_calls_before = backend.telemetry.lifetime.build_scene_calls;
    run_core(
        poll,
        receiver,
        sender.clone_handle(),
        &mut state,
        &mut backend,
        [],
        &ClientIdAllocator::new(),
        AuthState::new(None),
        ResetPolicy::NoReset,
        None,
    )
    .expect("run_core drives PresentPixmap, XTEST motion, MotionNotify and fake CRTC retirements");
    let producer_result = producer.join();
    mpv_reader
        .join()
        .expect("MotionNotify client response thread");
    {
        let _progress = commits.progress.lock().expect("commit progress");
        event_stop.store(true, Ordering::SeqCst);
        commits.changed.notify_all();
    }
    event_thread.join().expect("fake DRM event producer");
    let (sent_motion, sent_presents, observed_motion, final_pointer_position, motion_elapsed) =
        producer_result.expect("mixed Present/motion producer");

    let progress = commits.progress.lock().expect("commit progress");
    let accepted_kinds = progress.kinds.clone();
    let drained = progress.drained;
    drop(progress);
    let build_calls = backend
        .telemetry
        .lifetime
        .build_scene_calls
        .saturating_sub(build_calls_before);
    assert!(
        motion_elapsed >= refresh_interval * 5,
        "the run_core request stream must span at least five refresh intervals"
    );
    let motion_rate = f64::from(u32::try_from(sent_motion).unwrap()) / motion_elapsed.as_secs_f64();
    let present_rate =
        f64::from(u32::try_from(sent_presents).unwrap()) / motion_elapsed.as_secs_f64();
    assert!(
        (250.0..=1_000.0).contains(&motion_rate),
        "XTEST motion rate was {motion_rate:.1}/s"
    );
    assert!(
        (110.0..=150.0).contains(&present_rate),
        "PresentPixmap rate was {present_rate:.1}/s"
    );
    assert!(
        sent_presents >= 8,
        "the client must submit repeated PresentPixmap frames"
    );
    assert_eq!(
        observed_motion, sent_motion,
        "every XTEST motion reaches the fullscreen client"
    );
    assert_eq!(reactions.load(Ordering::SeqCst), observed_motion);
    assert_eq!(
        accepted_kinds,
        vec![AtomicCommitKind::Async; accepted_kinds.len()],
        "every accepted composed frame remains async while tearing is On"
    );
    assert!(
        !accepted_kinds.is_empty(),
        "client frames and pointer motion must compose"
    );
    assert!(
        accepted_kinds.len() as u128 <= sent_presents as u128 + refresh_windows,
        "{} async composed flips exceed {} client frames + {} virtual refresh intervals",
        accepted_kinds.len(),
        sent_presents,
        refresh_windows
    );
    assert!(
        build_calls as u128 <= sent_presents as u128 + refresh_windows,
        "{build_calls} build_scene calls exceed {sent_presents} client frames + {refresh_windows} refresh intervals"
    );
    assert!(backend.pstate.tearing.is_on());
    // With scene and cursor wakes clear and no flip pending below, the last
    // committed software-cursor position is the core's final pointer position.
    assert_eq!(
        state.pointer_root,
        (
            i16::try_from(final_pointer_position.0).expect("final pointer x fits root coords"),
            i16::try_from(final_pointer_position.1).expect("final pointer y fits root coords"),
        ),
        "run_core leaves the server pointer at the final XTEST motion"
    );
    assert_eq!(
        (backend.core.cursor_x, backend.core.cursor_y),
        (
            final_pointer_position.0 as f32,
            final_pointer_position.1 as f32,
        ),
        "quiescent scene cursor state matches the final pointer position"
    );
    assert_eq!(backend.pstate.tearing.deferred_cursor_wake, None);
    assert!(!backend.scene.cursor_only_wake_pending());
    assert!(!backend.scene.scene_structure_dirty);
    assert!(
        !backend.scene.has_pending_page_flips(),
        "pending flips after observable fake flip retirement: accepted={} drained={} queued={}",
        accepted_kinds.len(),
        drained,
        flips.lock().expect("flip queue").len()
    );
    assert_eq!(drained, accepted_kinds.len());
    assert!(
        flips.lock().expect("flip queue").is_empty(),
        "all accepted fake flip events are consumed before shutdown"
    );
    assert!(backend.kms_outputs_active);
    assert!(backend.scanout_allowed());
    true
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: ignore the active grouped cursor handoff when applying the compose interval cap.
fn task5_handoff_unflip_starts_cursor_compose_interval() {
    use std::sync::atomic::Ordering;
    use yserver_core::{backend::Backend, server::ServerState};

    const WINDOW: u32 = 0x7c58;
    let Some(mut backend) = live_scene_backend() else {
        return;
    };
    let recorder = recording_sink(&mut backend, 0);
    qualify_test_direct_async(&mut backend, &recorder, WINDOW, 15_050);
    let mut state = ServerState::new();
    let shape = try_direct_present_with_live_storage(
        &mut backend,
        15_149,
        WINDOW,
        800,
        600,
        yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
    )
    .expect("hold direct async frame");
    retire_direct_present(&mut backend, &mut state, 74);
    assert!(backend.pstate.scanout_m2.current.is_some());
    assert!(
        backend
            .platform
            .async_ledger
            .allows(crate::drm::async_flip::AsyncStream::Direct, &shape)
    );
    recorder.clear_records();

    // This pending timer is stale while the cursor is hidden on held direct scanout.
    // Keep the previous compose time absent so only the grouped unflip can start
    // the interval used by the following visible cursor motion. The injected
    // monotonic clock keeps that pointer operation inside the interval under load.
    let scheduler_time_ns = install_controlled_scheduler_clock(&mut backend);
    backend.pstate.tearing.last_cursor_compose = None;
    backend.pstate.tearing.deferred_cursor_wake =
        Some(std::time::Instant::now() + std::time::Duration::from_secs(30));
    Backend::set_cursor_hidden(&mut backend, false);
    assert_eq!(
        backend.pstate.scanout_m2.unflip_reason,
        Some("tearing_cursor_handoff")
    );
    Backend::maybe_composite(&mut backend).expect("submit grouped cursor handoff");
    assert_eq!(recorder.records().len(), 1);
    assert_eq!(recorder.records()[0].kind, AtomicCommitKind::Scene);
    assert!(backend.pstate.scanout_m2.current.is_some());

    let compose_time = backend
        .pstate
        .tearing
        .last_cursor_compose
        .expect("successful composed unflip starts the cursor interval");
    assert_eq!(backend.pstate.tearing.deferred_cursor_wake, None);
    let interval = backend
        .pstate
        .tearing
        .cursor_compose_interval(&backend.platform);
    let interval_ns = u64::try_from(interval.as_nanos()).unwrap();
    scheduler_time_ns.store(interval_ns / 2, Ordering::SeqCst);
    Backend::warp_pointer_root(&mut backend, &mut state, 121, 100);
    assert_eq!(
        backend.pstate.tearing.deferred_cursor_wake,
        Some(compose_time + interval),
        "cursor-only motion during the interval gets one deferred wake"
    );
    Backend::maybe_composite(&mut backend).expect("cursor motion waits behind the unflip");
    assert_eq!(
        recorder.records().len(),
        1,
        "cursor-only motion must not submit another composed frame immediately"
    );

    retire_composed_frame(&mut backend, &mut state, 75);
    assert!(backend.pstate.scanout_m2.current.is_none());
    assert!(!backend.pstate.scanout_m2.unflip_requested);
    assert!(
        backend
            .pstate
            .scanout_m2
            .unflip_outputs_to_replace
            .is_empty()
    );
    assert_eq!(
        backend.pstate.tearing.deferred_cursor_wake,
        Some(compose_time + interval)
    );

    // Advance the scheduled timer through the production entry without sleeping.
    scheduler_time_ns.store(interval_ns, Ordering::SeqCst);
    Backend::maybe_composite(&mut backend).expect("compose the deferred cursor motion");
    assert_eq!(recorder.records().len(), 2);
    assert_eq!(recorder.records()[1].kind, AtomicCommitKind::Async);
    retire_composed_frame(&mut backend, &mut state, 76);

    assert!(backend.pstate.tearing.is_on());
    assert_eq!(backend.pstate.tearing.deferred_cursor_wake, None);
    assert!(backend.pstate.tearing.last_cursor_compose.is_some());
    assert!(!backend.pstate.scanout_m2.active());
    assert!(!backend.pstate.scanout_m2.unflip_requested);
    assert!(
        backend
            .pstate
            .scanout_m2
            .unflip_outputs_to_replace
            .is_empty()
    );
    assert!(backend.pstate.scanout_m2.pending.is_none());
    assert!(backend.pstate.scanout_m2.queued_successor.is_none());
    assert!(!backend.scene.has_pending_page_flips());
    assert_direct_queue_drained(&backend);
    assert_commit_sequence(
        &recorder,
        &[AtomicCommitKind::Scene, AtomicCommitKind::Async],
    );
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
        crate::kms::render::scene::CursorPlaneMode::Sw
    );

    let interval = backend
        .pstate
        .tearing
        .cursor_compose_interval(&backend.platform);
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
// Mutation: leave the cursor suppression/deadline policy in place after exit.
fn task5_exit_restores_cursor_policy_per_state() {
    use yserver_core::{
        backend::Backend,
        randr::{CrtcTransform, FIXED_ONE, Filter},
        server::{BackendCapabilities, ServerState},
    };

    // (a) A visible cursor on an untransformed output returns to HW through
    // the next Vsync scene commit.
    let Some((mut hw, hw_recorder, mut hw_state)) = live_tearing_cursor_backend(0x7c70, 15_200)
    else {
        return;
    };
    let (show_executor, _hide_executor) = install_cursor_plane_with_successful_show(&mut hw);
    seed_pending_tearing_cursor_wake(&mut hw);
    exit_tearing_with_copy_present(&mut hw, 0x7c70, 15_300);
    assert_tearing_off(&hw);
    Backend::mark_dirty(&mut hw);
    Backend::maybe_composite(&mut hw).expect("compose after tearing exit to HW");
    assert_eq!(hw_recorder.records().len(), 1);
    assert_eq!(hw_recorder.records()[0].kind, AtomicCommitKind::Scene);
    retire_composed_frame(&mut hw, &mut hw_state, 74);
    assert_eq!(
        hw.scene.cursor_mode(),
        crate::kms::render::scene::CursorPlaneMode::Hw
    );
    assert!(hw.platform.any_cursor_plane_visible());
    assert_eq!(show_executor.calls().len(), 1);
    assert_tearing_off(&hw);
    assert!(hw.kms_outputs_active);
    assert_commit_sequence(&hw_recorder, &[AtomicCommitKind::Scene]);

    // (b) A transformed output keeps a visible cursor in the composed scene.
    let Some((mut sw, sw_recorder, mut sw_state)) = live_tearing_cursor_backend(0x7c71, 15_400)
    else {
        return;
    };
    seed_pending_tearing_cursor_wake(&mut sw);
    let (outputs, modes) = Backend::randr_outputs_and_modes(&mut sw);
    let (screen_width, screen_height) = Backend::fb_dimensions(&sw);
    let mut randr_state = ServerState::with_randr_outputs_and_modes(
        screen_width,
        screen_height,
        outputs,
        modes,
        BackendCapabilities::from_backend(&sw),
    );
    randr_state.randr.outputs[0].current_transform = CrtcTransform::new(
        [2 * FIXED_ONE, 0, 0, 0, 2 * FIXED_ONE, 0, 0, 0, FIXED_ONE],
        Some(Filter::Bilinear),
        Vec::new(),
    )
    .expect("construct test output transform");
    Backend::randr_layout_changed(&mut sw, &mut randr_state);
    exit_tearing_with_copy_present(&mut sw, 0x7c71, 15_500);
    assert_tearing_off(&sw);
    assert!(sw.platform.any_output_transformed());
    sw_recorder.clear_records();
    Backend::mark_dirty(&mut sw);
    Backend::maybe_composite(&mut sw).expect("compose after transformed exit");
    assert_eq!(sw_recorder.records().len(), 1);
    assert_eq!(sw_recorder.records()[0].kind, AtomicCommitKind::Scene);
    retire_composed_frame(&mut sw, &mut sw_state, 75);
    assert_eq!(
        sw.scene.cursor_mode(),
        crate::kms::render::scene::CursorPlaneMode::Sw
    );
    assert!(!sw.platform.any_cursor_plane_visible());
    assert_tearing_off(&sw);
    assert_commit_sequence(&sw_recorder, &[AtomicCommitKind::Scene]);

    // (c) XFIXES-hidden stays Hidden after the same exit.
    let Some((mut hidden, hidden_recorder, mut hidden_state)) =
        live_tearing_cursor_backend(0x7c72, 15_600)
    else {
        return;
    };
    Backend::set_cursor_hidden(&mut hidden, true);
    seed_pending_tearing_cursor_wake(&mut hidden);
    exit_tearing_with_copy_present(&mut hidden, 0x7c72, 15_700);
    assert_tearing_off(&hidden);
    Backend::mark_dirty(&mut hidden);
    Backend::maybe_composite(&mut hidden).expect("compose after hidden cursor exit");
    assert_eq!(hidden_recorder.records().len(), 1);
    assert_eq!(hidden_recorder.records()[0].kind, AtomicCommitKind::Scene);
    retire_composed_frame(&mut hidden, &mut hidden_state, 76);
    assert_eq!(
        hidden.scene.cursor_mode(),
        crate::kms::render::scene::CursorPlaneMode::Hidden
    );
    assert!(!hidden.platform.any_cursor_plane_visible());
    assert_tearing_off(&hidden);
    assert_commit_sequence(&hidden_recorder, &[AtomicCommitKind::Scene]);

    // (d) DPMS off clears tearing state without submitting a cursor plane
    // commit; resume owns reapplying the cursor policy.
    let Some((mut dark, dark_recorder, _dark_state)) = live_tearing_cursor_backend(0x7c73, 15_800)
    else {
        return;
    };
    dark.platform.for_tests_accept_output_power_commits();
    seed_pending_tearing_cursor_wake(&mut dark);
    Backend::set_dpms_power(&mut dark, 3).expect("turn test output off");
    assert_tearing_off(&dark);
    assert!(!dark.kms_outputs_active);
    dark_recorder.clear_records();
    Backend::maybe_composite(&mut dark).expect("dark scene pass is gated");
    assert!(dark_recorder.records().is_empty());
    assert_tearing_off(&dark);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: skip exit_tearing_for_window from the Backend destroy entry.
fn task5_tearing_window_destroy_exits_and_restores_cursor_policy() {
    use yserver_core::backend::Backend;

    const WINDOW: u32 = 0x7c80;
    let Some((mut backend, recorder, mut state)) = live_tearing_cursor_backend(WINDOW, 15_900)
    else {
        return;
    };
    seed_pending_tearing_cursor_wake(&mut backend);

    Backend::destroy_subwindow(&mut backend, None, WINDOW)
        .expect("destroy tearing owner through Backend entry");
    assert_tearing_off(&backend);
    assert!(!backend.windows.contains_key(&WINDOW));
    damage_root(&mut backend);
    Backend::maybe_composite(&mut backend).expect("compose after owner destruction");
    assert_eq!(recorder.records().len(), 1);
    assert_eq!(recorder.records()[0].kind, AtomicCommitKind::Scene);
    retire_composed_frame(&mut backend, &mut state, 77);
    assert_tearing_off(&backend);
    assert_eq!(
        backend.scene.cursor_mode(),
        crate::kms::render::scene::CursorPlaneMode::Sw
    );
    assert!(!backend.platform.any_cursor_plane_visible());
    assert!(backend.kms_outputs_active);
    assert!(!backend.scene.has_pending_page_flips());
    assert_commit_sequence(&recorder, &[AtomicCommitKind::Scene]);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: restore the direct-ownership guard on resume recovery so the stale plane stays bound.
fn hidden_cursor_while_vt_suspended_retries_failed_plane_hide_on_resume() {
    use std::rc::Rc;
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
                })
                .collect())
        }
    }

    let Some(mut backend) = live_scene_backend() else {
        return;
    };
    backend.for_tests_set_vt_resume_executor(Rc::new(LogicalVtResume));
    backend.platform.for_tests_accept_output_power_commits();
    let recorder = recording_sink(&mut backend, 0);

    let visible_cursor = create_bitmap_cursor_via_backend(&mut backend, true, true);
    let root = backend.core.window_id;
    Backend::define_cursor(&mut backend, None, root, visible_cursor.as_raw())
        .expect("define visible cursor through Backend");
    assert!(!backend.effective_cursor_hidden());

    let crtc = backend.platform.outputs[0].output.crtc;
    let device = &mut backend.platform.devices[0];
    let mut plane =
        crate::kms::cursor_plane::CursorPlane::for_tests_stub(Rc::clone(&device.device), 64, 64);
    plane.for_tests_set_visible(crtc, true);
    let hide_executor = Rc::new(RecordingCursorHideExecutor::default());
    hide_executor.script([
        TestCursorHideResult::Error(libc::EIO),
        TestCursorHideResult::Error(libc::EIO),
        TestCursorHideResult::Ok,
    ]);
    let hide_executor_trait: Rc<dyn crate::kms::cursor_plane::CursorHideExecutor> =
        hide_executor.clone();
    plane.for_tests_set_hide_executor(hide_executor_trait);
    device.cursor.plane = Some(plane);
    assert!(backend.platform.any_cursor_plane_visible());

    let mut state = ServerState::new();
    Backend::finish_vt_release(&mut backend, &mut state, &InputInventory::default());
    assert!(!backend.pstate.scanout_m2.active());
    assert_eq!(hide_executor.calls(), [crtc, crtc]);
    assert!(backend.platform.any_cursor_plane_visible());

    Backend::set_cursor_hidden(&mut backend, true);
    assert!(backend.effective_cursor_hidden());
    assert!(backend.platform.any_cursor_plane_visible());

    Backend::on_vt_acquire(&mut backend, &mut state);
    assert_eq!(hide_executor.calls(), [crtc, crtc, crtc]);
    assert!(!backend.platform.any_cursor_plane_visible());
    assert!(backend.effective_cursor_hidden());
    assert_tearing_off(&backend);
    assert_eq!(backend.pstate.vt_state, crate::vt::state::VtState::Active);
    assert!(backend.scanout_allowed());
    assert!(backend.kms_outputs_active);
    assert_eq!(backend.pstate.tearing.deferred_cursor_wake, None);
    assert_direct_queue_drained(&backend);
    assert!(!backend.scene.has_pending_page_flips());
    assert_commit_sequence(&recorder, &[]);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: count EINVAL and non-OS errors as accepted Async commits, count Presents only at submission, or retain tearing state on exit.
fn tearing_summary_counts_async_and_fallbacks() {
    use yserver_core::backend::Backend;

    const WINDOW: u32 = 0x7a60;
    let Some(mut backend) = live_scene_backend() else {
        return;
    };
    let recorder = recording_sink(&mut backend, 0);
    qualify_test_direct_async(&mut backend, &recorder, WINDOW, 700);
    assert!(backend.pstate.tearing.is_on());

    let sink = std::rc::Rc::clone(&backend.platform.devices[0].atomic_commit_sink);
    let before = sink.for_tests_tearing_counts();
    let present_latency_before = sink.for_tests_present_latency_state();
    let mut state = ServerState::new();
    let (width, height) = backend.platform.fb_dimensions();
    let mut expected = Vec::new();

    recorder.script_result(Err(std::io::Error::other("scripted non-OS error")));
    let (candidate, event, _) = prepare_direct_present_for_window(
        &mut backend,
        900,
        WINDOW,
        width,
        height,
        yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
    );
    let before_non_os_latency = sink.for_tests_present_latency_state();
    let before_non_os_error = sink.for_tests_tearing_counts();
    let error = Backend::try_present_direct(&mut backend, candidate, event)
        .expect_err("the custom non-OS async error must be returned");
    assert_eq!(error.raw_os_error(), None);
    assert_eq!(
        sink.for_tests_tearing_counts().async_direct,
        before_non_os_error.async_direct,
        "a non-OS error is not an accepted async commit"
    );
    assert_eq!(
        sink.for_tests_present_latency_state().3.len(),
        before_non_os_latency.3.len(),
        "a non-OS error must not add a successful submit sample"
    );
    expected.push(AtomicCommitKind::Async);
    for index in 0..u64::from(crate::kms::render::backend::TEARING_ENTRY_PROBATION - 1) {
        assert!(
            try_direct_present_for_window(
                &mut backend,
                901 + index,
                WINDOW,
                width,
                height,
                yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
            )
            .is_none(),
            "eligible warmup Present {index} rebuilds probation without submitting"
        );
    }
    let before_stream = sink.for_tests_tearing_counts();
    for index in 0..3_u64 {
        let present_id = 800 + index;
        assert!(
            try_direct_present_for_window(
                &mut backend,
                present_id,
                WINDOW,
                width,
                height,
                yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
            )
            .is_some(),
            "async direct Present {present_id} must be accepted"
        );
        assert_eq!(
            recorder.records().last().unwrap().kind,
            AtomicCommitKind::Async
        );
        expected.push(AtomicCommitKind::Async);
        retire_direct_present(&mut backend, &mut state, index as u32 + 30);
    }

    recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EINVAL)));
    recorder.script_result(Ok(()));
    assert!(
        try_direct_present_for_window(
            &mut backend,
            803,
            WINDOW,
            width,
            height,
            yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
        )
        .is_some(),
        "the EINVAL retry must retain the direct Present"
    );
    expected.extend([AtomicCommitKind::Async, AtomicCommitKind::Scene]);
    assert_eq!(
        recorder.records().last().unwrap().kind,
        AtomicCommitKind::Scene
    );
    retire_direct_present(&mut backend, &mut state, 40);

    let after = sink.for_tests_tearing_counts();
    assert_eq!(after.async_direct - before_stream.async_direct, 3);
    assert_eq!(after.async_composed, before.async_composed);
    assert_eq!(
        after.async_einval_direct - before_stream.async_einval_direct,
        1
    );
    assert_eq!(after.async_einval_composed, before.async_einval_composed);
    assert_eq!(after.vsync_while_on - before_stream.vsync_while_on, 1);
    assert_eq!(
        after.client_present_direct - before_stream.client_present_direct,
        4
    );
    assert_eq!(
        after.direct_turnarounds - before_stream.direct_turnarounds,
        3
    );
    assert_eq!(
        after.direct_present_to_flip_events - before_stream.direct_present_to_flip_events,
        3
    );
    assert_eq!(
        after.composed_present_to_flip_events,
        before.composed_present_to_flip_events
    );
    assert_eq!(
        backend
            .platform
            .async_ledger
            .for_tests_consecutive_einval(crate::drm::async_flip::AsyncStream::Direct),
        1
    );

    // Two eligible Presents arrive behind the first in-flight commit. The
    // latest-wins slot replaces 804 with 805, so all three receipts count
    // even though only 804 and 806 reach the recording atomic sink.
    for present_id in [804, 805, 806] {
        assert!(
            try_direct_present_for_window(
                &mut backend,
                present_id,
                WINDOW,
                width,
                height,
                yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
            )
            .is_some(),
            "eligible direct Present {present_id} must be retained"
        );
    }
    assert_eq!(
        recorder.records().len(),
        7,
        "only the in-flight Present submits"
    );
    retire_direct_present(&mut backend, &mut state, 41);
    assert_eq!(
        recorder.records().len(),
        8,
        "the latest successor submits at retirement"
    );
    retire_direct_present(&mut backend, &mut state, 42);
    assert_direct_queue_drained(&backend);
    let after_successors = sink.for_tests_tearing_counts();
    assert_eq!(
        after_successors.client_present_direct - before_stream.client_present_direct,
        7
    );

    // Seed a pending scheduler deadline after all async counters are checked;
    // the real Backend Present entry below exits tearing and must clear it.
    seed_pending_tearing_cursor_wake(&mut backend);
    assert!(backend.pstate.tearing.deferred_cursor_wake.is_some());
    assert!(
        try_direct_present_for_window(&mut backend, 807, WINDOW, width, height, 0).is_some(),
        "the non-Async Present exits tearing and submits its Vsync frame"
    );
    assert_tearing_off(&backend);
    assert_eq!(
        backend.scene.cursor_mode(),
        crate::kms::render::scene::CursorPlaneMode::Hidden,
        "exit restores the XFIXES-hidden cursor policy"
    );
    assert!(!backend.platform.any_cursor_plane_visible());
    assert_eq!(backend.platform.tearing_copy_received_at, None);
    assert_eq!(backend.platform.tearing_copy_received_monotonic, None);
    let reset = sink.for_tests_tearing_counts();
    assert_eq!(reset.async_direct, 0);
    assert_eq!(reset.async_composed, 0);
    assert_eq!(reset.async_einval_direct, 0);
    assert_eq!(reset.async_einval_composed, 0);
    assert_eq!(reset.vsync_while_on, 0);
    assert_eq!(reset.client_present_direct, 0);
    assert_eq!(reset.direct_turnarounds, 0);
    assert_eq!(reset.composed_turnarounds, 0);
    assert_eq!(reset.direct_present_to_flip_events, 0);
    assert_eq!(reset.composed_present_to_flip_events, 0);
    retire_direct_present(&mut backend, &mut state, 43);
    assert_direct_queue_drained(&backend);
    assert!(!backend.scene.has_pending_page_flips());

    let records = recorder.records();
    assert_eq!(records.len(), 9);
    assert_eq!(
        records[4].result,
        crate::drm::atomic::tests_support::ScriptedResult::Error {
            kind: std::io::ErrorKind::InvalidInput,
            raw_os_error: Some(libc::EINVAL),
        }
    );
    assert_eq!(
        records[5].result,
        crate::drm::atomic::tests_support::ScriptedResult::Ok
    );
    assert_eq!(
        records[0].result,
        crate::drm::atomic::tests_support::ScriptedResult::Error {
            kind: std::io::ErrorKind::Other,
            raw_os_error: None,
        }
    );
    expected.extend([AtomicCommitKind::Async, AtomicCommitKind::Async]);
    expected.push(AtomicCommitKind::Scene);
    assert_commit_sequence(&recorder, &expected);
    assert_tearing_off(&backend);
    assert!(
        backend
            .pstate
            .scanout_m2
            .deferred_successor_skips
            .is_empty()
    );
    let (eligible_presents, flips, skips, present_to_submit, present_to_flip) =
        sink.for_tests_present_latency_state();
    assert_eq!(
        eligible_presents - present_latency_before.0,
        16,
        "receipt rate includes the Off FIFO Present"
    );
    assert_eq!(
        flips - present_latency_before.1,
        7,
        "each accepted direct or fallback frame has its own event"
    );
    assert_eq!(skips - present_latency_before.2, 0);
    assert_eq!(present_to_submit.len() - present_latency_before.3.len(), 7);
    assert_eq!(present_to_flip.len() - present_latency_before.4.len(), 7);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: keep the oldest ready successor instead of replacing it with the latest frame.
fn tearing_on_uncapped_async_stream_reaches_backend_latest_wins_slot() {
    use yserver_core::server::ServerState;

    const WINDOW: u32 = 0x7a61;
    const FIRST_PRESENT: u64 = 2_000;
    const FRAME_COUNT: u64 = 20;

    let Some(mut backend) = live_scene_backend() else {
        return;
    };
    let recorder = recording_sink(&mut backend, 0);
    qualify_test_direct_async(&mut backend, &recorder, WINDOW, 2_100);
    assert!(backend.pstate.tearing.is_on());

    let sink = std::rc::Rc::clone(&backend.platform.devices[0].atomic_commit_sink);
    let before = sink.for_tests_tearing_counts();
    let mut state = ServerState::new();
    let (width, height) = backend.platform.fb_dimensions();

    // The delayed-acquire companion sends frames through this entry only
    // after its 3 ms / 4 ms logical wait schedule.
    for frame in 0..FRAME_COUNT {
        let present_id = FIRST_PRESENT + frame;
        assert!(
            try_direct_present_for_window(
                &mut backend,
                present_id,
                WINDOW,
                width,
                height,
                yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
            )
            .is_some(),
            "ready frame {present_id} must reach the Backend while the prior flip is in flight"
        );
    }

    assert_eq!(
        backend
            .pstate
            .scanout_m2
            .pending
            .as_ref()
            .map(|frame| frame.candidate.present_id),
        Some(FIRST_PRESENT),
        "the first frame remains the in-flight flip"
    );
    assert_eq!(
        backend
            .pstate
            .scanout_m2
            .queued_successor
            .as_ref()
            .map(|frame| frame.candidate.present_id),
        Some(FIRST_PRESENT + FRAME_COUNT - 1),
        "the newest ready frame occupies the single backend successor slot"
    );
    assert_eq!(
        backend
            .pstate
            .scanout_m2
            .deferred_successor_skips
            .iter()
            .map(|event| event.present_id)
            .collect::<Vec<_>>(),
        (FIRST_PRESENT + 1..FIRST_PRESENT + FRAME_COUNT - 1).collect::<Vec<_>>(),
        "only the older ready successors are skipped by backend latest-wins"
    );
    assert_eq!(
        recorder.records().len(),
        1,
        "only the in-flight frame submits"
    );

    retire_direct_present(&mut backend, &mut state, 50);
    assert_eq!(
        backend
            .pstate
            .scanout_m2
            .completed
            .iter()
            .map(|event| (event.present_id, event.completion_mode))
            .collect::<Vec<_>>(),
        std::iter::once((
            FIRST_PRESENT,
            yserver_protocol::x11::present::COMPLETE_MODE_FLIP,
        ))
        .chain(
            (FIRST_PRESENT + 1..FIRST_PRESENT + FRAME_COUNT - 1).map(|present_id| {
                (
                    present_id,
                    yserver_protocol::x11::present::COMPLETE_MODE_SKIP,
                )
            })
        )
        .collect::<Vec<_>>(),
        "the retired flip completes before ordered skips for replaced successors"
    );
    assert_eq!(
        recorder.records().len(),
        2,
        "the newest successor submits at retirement"
    );
    retire_direct_present(&mut backend, &mut state, 51);
    assert_direct_queue_drained(&backend);

    assert_eq!(
        sink.for_tests_tearing_counts().client_present_direct - before.client_present_direct,
        FRAME_COUNT,
        "all uncapped frames are counted at Backend entry, including replaced successors"
    );

    seed_pending_tearing_cursor_wake(&mut backend);
    assert!(
        try_direct_present_for_window(&mut backend, 2_020, WINDOW, width, height, 0).is_some(),
        "a non-Async Backend Present exits tearing"
    );
    assert_tearing_off(&backend);
    retire_direct_present(&mut backend, &mut state, 52);
    assert_direct_queue_drained(&backend);
    assert!(!backend.scene.has_pending_page_flips());
    assert_commit_sequence(
        &recorder,
        &[
            AtomicCommitKind::Async,
            AtomicCommitKind::Async,
            AtomicCommitKind::Scene,
        ],
    );
    assert_tearing_off(&backend);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: scrap fence-waiting victims in supersede_covered_pending_presents.
fn tearing_on_delayed_acquire_stream_survives_core_supersession() {
    use std::{
        io::Read,
        os::{
            fd::{AsFd as _, AsRawFd as _, BorrowedFd, OwnedFd},
            unix::net::UnixStream,
        },
        sync::{Arc, Condvar, Mutex},
        thread,
    };
    use yserver_core::{
        backend::{Backend, SyncobjHandle},
        core_loop::{
            BoundSender, CoreSender, Message, ResetPolicy, auth::AuthState, channel,
            poll_tokens::ClientIdAllocator, run_core,
        },
        server::{BackendCapabilities, ServerState},
    };
    use yserver_protocol::x11::{
        ClientId, CreatePixmapRequest, RequestHeader, ResourceId, SequenceNumber, present,
    };

    const WINDOW: ResourceId = ResourceId(0x0100_7d90);
    const PIXMAP_BASE: u32 = 0x0100_7da0;
    const FIRST_SERIAL: u32 = 4_000;
    const FRAME_COUNT: u64 = 20;
    const ACQUIRE_SYNCOBJ: u32 = 0x00d0_0a11;
    const RELEASE_SYNCOBJ: u32 = 0x00d0_0a12;

    #[derive(Debug, Default)]
    struct VirtualTime {
        now_ms: u64,
        generation: u64,
        observed_generation: u64,
    }

    #[derive(Debug, Default)]
    struct AcquireTimeline {
        time: Mutex<VirtualTime>,
        observed: Condvar,
    }

    impl AcquireTimeline {
        fn advance_to(&self, now_ms: u64) -> u64 {
            let mut time = self.time.lock().expect("virtual acquire clock");
            assert!(now_ms >= time.now_ms, "virtual acquire time is monotonic");
            time.now_ms = now_ms;
            time.generation += 1;
            time.generation
        }

        fn wait_until_observed(&self, generation: u64) {
            let mut time = self.time.lock().expect("virtual acquire clock");
            while time.observed_generation < generation {
                time = self
                    .observed
                    .wait(time)
                    .expect("wait for the production source-wait drain");
            }
        }
    }

    impl SyncobjHandle for AcquireTimeline {
        fn signal(&self, _value: u64) -> std::io::Result<()> {
            Ok(())
        }

        fn import_sync_file(&self, _value: u64, _fd: BorrowedFd<'_>) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl crate::kms::render::imported_syncobj::PresentSyncobj for AcquireTimeline {
        fn timeline_value(&self) -> std::io::Result<u64> {
            let mut time = self.time.lock().expect("virtual acquire clock");
            time.observed_generation = time.generation;
            self.observed.notify_all();
            Ok(time.now_ms)
        }

        fn signaled_eventfd(&self, _value: u64) -> std::io::Result<OwnedFd> {
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "test timeline uses the polling fallback",
            ))
        }
    }

    #[derive(Debug, Default)]
    struct ReleaseTimeline {
        signaled: Mutex<Vec<u64>>,
    }

    impl SyncobjHandle for ReleaseTimeline {
        fn signal(&self, value: u64) -> std::io::Result<()> {
            self.signaled.lock().expect("release timeline").push(value);
            Ok(())
        }

        fn import_sync_file(&self, _value: u64, _fd: BorrowedFd<'_>) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl crate::kms::render::imported_syncobj::PresentSyncobj for ReleaseTimeline {
        fn timeline_value(&self) -> std::io::Result<u64> {
            Ok(self
                .signaled
                .lock()
                .expect("release timeline")
                .last()
                .copied()
                .unwrap_or(0))
        }

        fn signaled_eventfd(&self, _value: u64) -> std::io::Result<OwnedFd> {
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "test release timeline does not export eventfds",
            ))
        }
    }

    struct ShutdownOnDrop {
        sender: CoreSender,
        active: bool,
    }

    impl Drop for ShutdownOnDrop {
        fn drop(&mut self) {
            if self.active {
                let _ = self.sender.send(Message::Shutdown);
            }
        }
    }

    fn read_barrier(
        peer: &mut UnixStream,
        request_sender: &BoundSender,
        sequence: &mut u16,
    ) -> Vec<super::TestPresentCompletion> {
        let current_sequence = *sequence;
        *sequence = sequence.wrapping_add(1);
        request_sender
            .send(Message::Request {
                id: ClientId(1),
                sequence: SequenceNumber(current_sequence),
                accepted_at: None,
                header: RequestHeader {
                    opcode: 43, // GetInputFocus: core-loop round trip.
                    data: 0,
                    length_units: 1,
                },
                body: Vec::new(),
                attached_fd: None,
            })
            .expect("queue core-loop barrier");

        let mut completions = Vec::new();
        loop {
            let mut first = [0_u8; 1];
            peer.read_exact(&mut first)
                .expect("read core-loop response");
            match first[0] {
                35 => {
                    let mut event = [0_u8; 40];
                    event[0] = first[0];
                    peer.read_exact(&mut event[1..])
                        .expect("read Present CompleteNotify");
                    assert_eq!(
                        u16::from_le_bytes([event[8], event[9]]),
                        u16::from(present::EVENT_COMPLETE_NOTIFY),
                    );
                    completions.push(super::TestPresentCompletion {
                        serial: u32::from_le_bytes(event[20..24].try_into().unwrap()),
                        mode: event[11],
                    });
                }
                1 => {
                    let mut reply = [0_u8; 32];
                    reply[0] = first[0];
                    peer.read_exact(&mut reply[1..])
                        .expect("read GetInputFocus reply");
                    break;
                }
                response => panic!("unexpected X11 response {response} from run_core"),
            }
        }
        completions
    }

    fn run_core_for_barriers(
        state: &mut ServerState,
        backend: &mut KmsBackend,
        peer: &mut UnixStream,
        sequence: &mut u16,
        barrier_count: usize,
    ) -> Vec<super::TestPresentCompletion> {
        let (poll, sender, receiver) = channel().expect("core-loop flush channel");
        let request_sender = sender.bind();
        let shutdown_sender = sender.clone_handle();
        let mut producer_peer = peer.try_clone().expect("clone core client peer");
        let mut producer_sequence = *sequence;
        let driver = thread::spawn(move || {
            let mut shutdown = ShutdownOnDrop {
                sender: shutdown_sender,
                active: true,
            };
            let mut completed = Vec::new();
            for _ in 0..barrier_count {
                completed.extend(read_barrier(
                    &mut producer_peer,
                    &request_sender,
                    &mut producer_sequence,
                ));
            }
            shutdown
                .sender
                .send(Message::Shutdown)
                .expect("stop core-loop flush");
            shutdown.active = false;
            (producer_sequence, completed)
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
        .expect("run_core flush");
        let (new_sequence, completed) = driver.join().expect("core-loop flush driver panicked");
        *sequence = new_sequence;
        completed
    }

    let Some(mut backend) = live_scene_backend() else {
        return;
    };
    let recorder = recording_sink(&mut backend, 0);
    let (pollable_device, _device_peer) = pollable_test_device();
    backend.platform.devices[0].device = pollable_device;
    let (outputs, modes) = Backend::randr_outputs_and_modes(&mut backend);
    let (width, height) = Backend::fb_dimensions(&backend);
    let mut state = ServerState::with_randr_outputs_and_modes(
        width,
        height,
        outputs,
        modes,
        BackendCapabilities::from_backend(&backend),
    );
    let stage_host =
        super::materialize_test_cow_stage(&mut backend, &mut state, ClientId(1), WINDOW);
    super::enter_tearing_without_direct_scanout_for_state_ids(
        &mut backend,
        &mut state,
        WINDOW.0,
        stage_host,
        stage_host,
        3_100,
    );
    Backend::set_cursor_hidden(&mut backend, true);
    super::damage_root_and_composite(&mut backend);
    assert!(backend.scene.has_pending_page_flips());
    super::retire_composed_frame(&mut backend, &mut state, 70);
    assert!(backend.pstate.tearing.is_on());
    for index in 0..7_u64 {
        assert!(
            super::try_direct_present_for_ids(
                &mut backend,
                3_200 + index,
                WINDOW.0,
                stage_host,
                stage_host,
                width,
                height,
                yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
            )
            .is_none()
        );
    }
    assert_eq!(backend.pstate.scanout_m2.eligible_root_streak, 7);
    recorder.clear_records();
    let sink = std::rc::Rc::clone(&backend.platform.devices[0].atomic_commit_sink);
    let before_direct = sink.for_tests_tearing_counts().client_present_direct;
    let before_skips = backend.telemetry.lifetime.present_skips;
    let mut peer = super::install_test_core_client(&mut state);
    peer.set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .expect("bound core-loop response waits");

    let acquire = Arc::new(AcquireTimeline::default());
    let release = Arc::new(ReleaseTimeline::default());
    backend
        .pstate
        .dri3_syncobjs
        .insert(ACQUIRE_SYNCOBJ, (ClientId(1), acquire.clone()));
    backend
        .pstate
        .dri3_syncobjs
        .insert(RELEASE_SYNCOBJ, (ClientId(1), release.clone()));
    // Force the existing timeline-poll fallback. run_core still discovers
    // readiness through Backend::drain_ready_present_source_waits.
    backend.pstate.syncobj_eventfd_supported = Some(false);

    let mut crtcs: Vec<_> = backend
        .platform
        .outputs
        .iter()
        .map(|layout| layout.output.crtc)
        .collect();
    crtcs.sort_by_key(|crtc| u32::from(*crtc));
    let direct_shape = crate::drm::async_flip::AsyncShape {
        fourcc: 0x3432_5258,
        modifier: 0,
        pitch: u32::from(width) * 4,
        width: u32::from(width),
        height: u32::from(height),
        crtcs,
        direct: true,
    };
    // Establish the M1 topology signature before injecting accepted probe
    // results for the synthetic scanout buffers below. The fixture's mode
    // geometry intentionally cannot run a real TEST_ONLY probe.
    let (topology_candidate, _, _) = super::prepare_direct_present_for_ids(
        &mut backend,
        3_300,
        WINDOW.0,
        stage_host,
        width,
        height,
        yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
    );
    Backend::note_present_scanout_candidate(&mut backend, topology_candidate);
    for frame in 0..FRAME_COUNT {
        let pixmap = ResourceId(PIXMAP_BASE + u32::try_from(frame).unwrap());
        let host_pixmap = Backend::create_pixmap(&mut backend, None, 24, width, height)
            .expect("create explicit-sync Present source");
        state.resources.create_pixmap(
            ClientId(1),
            CreatePixmapRequest {
                depth: 24,
                pixmap,
                drawable: WINDOW,
                width,
                height,
            },
        );
        assert!(state.resources.set_pixmap_host_xid(pixmap, host_pixmap));
        let source_id = backend
            .store
            .lookup(host_pixmap.as_raw())
            .expect("Present source has KMS storage");
        backend.pstate.scanout_m1.entries.insert(
            source_id,
            crate::kms::render::backend::ScanoutM1ProbeEntry::accepted_with_shape(
                crate::drm::modeset::DirectScanoutProbeFramebuffer::for_tests(raw(
                    0x8700 + u32::try_from(frame).unwrap()
                )),
                direct_shape.clone(),
            ),
        );
    }
    let first_present_id = state.present_next_id;
    super::select_test_present_complete_events(&mut state, WINDOW.0);

    let (poll, sender, receiver) = channel().expect("core-loop request channel");
    let request_sender = sender.bind();
    let shutdown_sender = sender.clone_handle();
    let producer_peer = peer.try_clone().expect("clone core client peer");
    let driver = thread::spawn(move || {
        let mut shutdown = ShutdownOnDrop {
            sender: shutdown_sender,
            active: true,
        };
        let mut producer_peer = producer_peer;
        let mut request_sequence = 1_u16;
        let mut completions = Vec::new();
        for frame in 0..FRAME_COUNT {
            let arrival_ms = frame * 3;
            acquire.advance_to(arrival_ms);
            let pixmap = PIXMAP_BASE + u32::try_from(frame).unwrap();
            let serial = FIRST_SERIAL + u32::try_from(frame).unwrap();
            let mut body = vec![0_u8; 84];
            body[0..4].copy_from_slice(&WINDOW.0.to_le_bytes());
            body[4..8].copy_from_slice(&pixmap.to_le_bytes());
            body[8..12].copy_from_slice(&serial.to_le_bytes());
            body[28..32].copy_from_slice(&ACQUIRE_SYNCOBJ.to_le_bytes());
            body[32..36].copy_from_slice(&RELEASE_SYNCOBJ.to_le_bytes());
            body[36..44].copy_from_slice(&(arrival_ms + 4).to_le_bytes());
            body[44..52].copy_from_slice(&(frame + 1).to_le_bytes());
            body[52..56].copy_from_slice(
                &yserver_core::present_scheduler::PRESENT_OPTION_ASYNC.to_le_bytes(),
            );
            let present_sequence = request_sequence;
            request_sequence = request_sequence.wrapping_add(1);
            request_sender
                .send(Message::Request {
                    id: ClientId(1),
                    sequence: SequenceNumber(present_sequence),
                    accepted_at: None,
                    header: RequestHeader {
                        opcode: 145,
                        data: present::PIXMAP_SYNCED,
                        length_units: 22,
                    },
                    body,
                    attached_fd: None,
                })
                .expect("queue real PresentPixmapSynced request");
            completions.extend(read_barrier(
                &mut producer_peer,
                &request_sender,
                &mut request_sequence,
            ));

            // At frame i's arrival + 1 ms, frame i-1's acquire point is
            // ready. The barrier wakes run_core, and the generation wait
            // proves the production source-wait drain observed this time.
            let prior_signal_ms = arrival_ms + 1;
            let generation = acquire.advance_to(prior_signal_ms);
            completions.extend(read_barrier(
                &mut producer_peer,
                &request_sender,
                &mut request_sequence,
            ));
            acquire.wait_until_observed(generation);
        }

        let final_signal_ms = (FRAME_COUNT - 1) * 3 + 4;
        let generation = acquire.advance_to(final_signal_ms);
        completions.extend(read_barrier(
            &mut producer_peer,
            &request_sender,
            &mut request_sequence,
        ));
        acquire.wait_until_observed(generation);

        shutdown
            .sender
            .send(Message::Shutdown)
            .expect("stop request core");
        shutdown.active = false;
        (request_sequence, completions)
    });
    run_core(
        poll,
        receiver,
        sender.clone_handle(),
        &mut state,
        &mut backend,
        [],
        &ClientIdAllocator::new(),
        AuthState::new(None),
        ResetPolicy::NoReset,
        None,
    )
    .expect("run_core PresentPixmapSynced stream");
    let (request_sequence, mut completions) = driver.join().expect("Present driver panicked");

    assert!(state.present_pending_exec.is_empty());
    assert!(state.present_wait_to_id.is_empty());
    assert!(backend.pstate.pending_present_source_waits.is_empty());
    assert_eq!(
        sink.for_tests_tearing_counts().client_present_direct - before_direct,
        FRAME_COUNT,
        "all 20 core requests reach the direct Backend entry; tearing={:?}, eligible_root_streak={}, unflip={:?}, completions={completions:?}",
        backend.pstate.tearing.state,
        backend.pstate.scanout_m2.eligible_root_streak,
        backend.pstate.scanout_m2.unflip_reason,
    );
    assert_eq!(
        backend
            .pstate
            .scanout_m2
            .pending
            .as_ref()
            .map(|frame| frame.candidate.present_id),
        Some(first_present_id),
        "first ready frame remains the in-flight async flip"
    );
    assert_eq!(
        backend
            .pstate
            .scanout_m2
            .queued_successor
            .as_ref()
            .map(|frame| frame.candidate.present_id),
        Some(first_present_id + FRAME_COUNT - 1),
        "latest ready frame remains in the Backend successor slot"
    );
    let backend_successor_skips: Vec<_> = backend
        .pstate
        .scanout_m2
        .deferred_successor_skips
        .iter()
        .map(|event| event.present_id)
        .collect();
    assert_eq!(
        backend_successor_skips,
        (first_present_id + 1..first_present_id + FRAME_COUNT - 1).collect::<Vec<_>>(),
        "only executed frames replaced by the in-flight Backend successor are skipped"
    );
    assert_eq!(
        backend.telemetry.lifetime.present_skips - before_skips,
        u64::try_from(backend_successor_skips.len()).unwrap(),
        "note_present_skip records only Backend successor replacements, with zero core fence-wait scraps"
    );
    assert_commit_sequence(&recorder, &[AtomicCommitKind::Async]);

    // Retire the two async flips through Backend's real page-flip callback;
    // core completion delivery still runs through run_core after each event.
    let retire = |backend: &mut KmsBackend, state: &mut ServerState, frame: u32| {
        let source = backend.platform.devices[0]
            .test_page_flip_event_source
            .as_ref()
            .expect("recording page-flip source");
        for output in &backend.platform.outputs {
            source.queue_flip(output.output.crtc, frame);
        }
        let drm_fd = backend.platform.devices[0].device.as_fd().as_raw_fd();
        Backend::on_page_flip_ready(backend, state, drm_fd);
        Backend::maybe_composite(backend).expect("settle retired async Present");
    };
    let mut next_flush_sequence = request_sequence;
    retire(&mut backend, &mut state, 100);
    assert_eq!(recorder.records().len(), 2);
    assert_commit_sequence(
        &recorder,
        &[AtomicCommitKind::Async, AtomicCommitKind::Async],
    );
    completions.extend(run_core_for_barriers(
        &mut state,
        &mut backend,
        &mut peer,
        &mut next_flush_sequence,
        2,
    ));
    retire(&mut backend, &mut state, 101);
    completions.extend(run_core_for_barriers(
        &mut state,
        &mut backend,
        &mut peer,
        &mut next_flush_sequence,
        2,
    ));

    let expected_completions: Vec<_> = (0..FRAME_COUNT)
        .map(|frame| super::TestPresentCompletion {
            serial: FIRST_SERIAL + u32::try_from(frame).unwrap(),
            mode: if frame == 0 || frame == FRAME_COUNT - 1 {
                present::COMPLETE_MODE_FLIP
            } else {
                present::COMPLETE_MODE_SKIP
            },
        })
        .collect();
    assert_eq!(completions, expected_completions);
    assert_eq!(
        state.present_next_id,
        first_present_id + FRAME_COUNT,
        "all 20 Present requests allocated and completed their core IDs"
    );
    assert!(state.present_pending_exec.is_empty());
    assert!(state.present_wait_to_id.is_empty());
    assert!(state.present_pending_complete.is_empty());
    assert!(state.present_complete_gate.is_empty());
    assert!(backend.pstate.pending_present_source_waits.is_empty());
    assert_direct_queue_drained(&backend);
    assert!(!backend.scene.has_pending_page_flips());
    assert!(backend.pstate.tearing.is_on());
    assert_eq!(
        backend.telemetry.lifetime.present_skips - before_skips,
        FRAME_COUNT - 2,
        "all skips belong to the 18 Backend latest-wins successor replacements"
    );
    assert!(
        backend
            .platform
            .async_ledger
            .allows(crate::drm::async_flip::AsyncStream::Direct, &direct_shape)
    );
    assert_eq!(
        backend
            .platform
            .async_ledger
            .for_tests_consecutive_einval(crate::drm::async_flip::AsyncStream::Direct),
        0,
        "accepted async direct flips leave no suspension history"
    );
    let mut release_points = release.signaled.lock().expect("release points").clone();
    release_points.sort_unstable();
    assert_eq!(
        release_points,
        (1..FRAME_COUNT).collect::<Vec<_>>(),
        "the 19 retired sources signal release; the final displayed frame remains current"
    );
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: retain the Copy receipt timestamp unless the async ioctl succeeds, even after its Vsync fallback succeeds.
fn tearing_copy_receipt_is_consumed_by_successful_vsync_fallback() {
    use yserver_core::{
        backend::Backend,
        server::{BackendCapabilities, ServerState},
    };
    use yserver_protocol::x11::{ClientId, ResourceId};

    const WINDOW: ResourceId = ResourceId(0x0100_7d00);
    let Some(mut backend) = live_scene_backend() else {
        return;
    };
    for device in &mut backend.platform.devices {
        device.async_page_flip = true;
    }
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
    let copy =
        Backend::create_pixmap(&mut backend, None, 24, width, height).expect("create Copy source");
    let mut state = enter_tearing_without_direct_scanout_for_ids(
        &mut backend,
        WINDOW.0,
        stage_host,
        stage_host,
        12_000,
    );
    recorder.clear_records();

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
    .expect("record Copy damage");
    Backend::note_present_pixmap(&mut backend, copy.as_raw(), stage_host);
    assert!(backend.platform.tearing_copy_received_at.is_some());
    recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EINVAL)));
    recorder.script_result(Ok(()));

    Backend::maybe_composite(&mut backend).expect("submit Copy damage with Vsync fallback");
    assert_eq!(
        backend.platform.tearing_copy_received_at, None,
        "the accepted Vsync retry consumed the damage receipt"
    );
    assert_eq!(backend.platform.tearing_copy_received_monotonic, None);
    assert_eq!(recorder.records().len(), 2);
    assert_eq!(
        recorder.records()[0].kind,
        AtomicCommitKind::Async,
        "the stream first attempted Async"
    );
    assert_eq!(recorder.records()[1].kind, AtomicCommitKind::Scene);
    retire_composed_frame(&mut backend, &mut state, 80);

    assert!(
        try_direct_present_for_window(&mut backend, 15_501, WINDOW.0, width, height, 0).is_none(),
        "a non-Async Present exits tearing while the visible SW cursor blocks direct scanout"
    );
    assert_tearing_off(&backend);
    assert_eq!(
        backend.scene.cursor_mode(),
        crate::kms::render::scene::CursorPlaneMode::Sw
    );
    assert!(!backend.scene.has_pending_page_flips());
    assert!(backend.kms_outputs_active);
    assert_commit_sequence(
        &recorder,
        &[AtomicCommitKind::Async, AtomicCommitKind::Scene],
    );
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: measure async turnaround with raw DRM UST instead of CLOCK_MONOTONIC event time.
fn composed_async_turnaround_uses_its_own_commit_flip_timestamp() {
    use std::{
        cell::Cell,
        os::fd::{AsFd as _, AsRawFd as _},
        rc::Rc,
        time::Duration,
    };
    use yserver_core::backend::Backend;

    const WINDOW: u32 = 0x7c88;
    let Some((mut backend, recorder, mut state)) = live_tearing_cursor_backend(WINDOW, 12_000)
    else {
        return;
    };
    let accepted_at = Rc::new(Cell::new(
        crate::drm::atomic::monotonic_timestamp().expect("CLOCK_MONOTONIC timestamp"),
    ));
    let clock_value = Rc::clone(&accepted_at);
    let sink = Rc::new(AtomicCommitSink::new_with_monotonic_clock(
        recorder.clone(),
        Rc::new(move || Some(clock_value.get())),
    ));
    sink.set_tearing_on(true);
    backend.platform.devices[0].atomic_commit_sink = Rc::clone(&sink);

    Backend::warp_pointer_root(&mut backend, &mut state, 300, 160);
    Backend::maybe_composite(&mut backend).expect("submit cursor-only composed Async frame");
    assert_eq!(recorder.records().len(), 1);
    assert_eq!(recorder.records()[0].kind, AtomicCommitKind::Async);

    let event_at = accepted_at.get() + Duration::from_micros(150);
    let crtc = backend.platform.outputs[0].output.crtc;
    backend.platform.devices[0]
        .test_page_flip_event_source
        .as_ref()
        .expect("recording page-flip event source")
        .queue_flip_with_timestamps(crtc, 91, Duration::from_micros(1_321_315), event_at);
    let drm_fd = backend.platform.devices[0].device.as_fd().as_raw_fd();
    Backend::on_page_flip_ready(&mut backend, &mut state, drm_fd);

    assert_eq!(sink.for_tests_composed_turnaround_samples(), [150]);
    assert!(!backend.scene.has_pending_page_flips());
    assert!(
        try_direct_present_for_window(&mut backend, 15_001, WINDOW, 800, 600, 0).is_none(),
        "the Backend Present entry exits tearing while the visible SW cursor blocks direct scanout"
    );
    assert_tearing_off(&backend);
    assert_eq!(
        backend.scene.cursor_mode(),
        crate::kms::render::scene::CursorPlaneMode::Sw
    );
    assert_eq!(backend.pstate.tearing.deferred_cursor_wake, None);
    assert!(!backend.scene.has_pending_page_flips());
    assert!(backend.kms_outputs_active);
    assert_commit_sequence(&recorder, &[AtomicCommitKind::Async]);
}

fn submit_direct_async_for_clock_test(
    backend: &mut KmsBackend,
    recorder: std::rc::Rc<RecordingAtomicCommitExecutor>,
    window: u32,
    present_id: u64,
) -> (
    std::rc::Rc<AtomicCommitSink>,
    Duration,
    ::drm::control::crtc::Handle,
) {
    use std::{cell::Cell, rc::Rc};
    use yserver_core::backend::Backend;

    let accepted_at = Rc::new(Cell::new(None));
    let accepted_clock = Rc::clone(&accepted_at);
    let sink = Rc::new(AtomicCommitSink::new_with_monotonic_clock(
        recorder.clone(),
        Rc::new(move || {
            let now = crate::drm::atomic::monotonic_timestamp();
            accepted_clock.set(now);
            now
        }),
    ));
    sink.set_tearing_on(true);
    backend.platform.devices[0].atomic_commit_sink = Rc::clone(&sink);

    let (width, height) = backend.platform.fb_dimensions();
    let (candidate, event, _) = prepare_direct_present_for_window(
        backend,
        present_id,
        window,
        width,
        height,
        yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
    );
    assert!(
        Backend::try_present_direct(backend, candidate, event)
            .expect("submit direct Async through the Backend Present entry")
    );
    assert_eq!(recorder.records().len(), 1);
    assert_eq!(recorder.records()[0].kind, AtomicCommitKind::Async);
    let accepted_at = accepted_at
        .get()
        .expect("accepted ioctl records CLOCK_MONOTONIC time");
    let crtc = backend.platform.outputs[0].output.crtc;
    (sink, accepted_at, crtc)
}

fn deliver_clock_test_flip(
    backend: &mut KmsBackend,
    state: &mut yserver_core::server::ServerState,
    crtc: ::drm::control::crtc::Handle,
    frame: u32,
    kernel_ust: Duration,
    received_at_monotonic: Duration,
    received_at_boottime: Duration,
) {
    use std::os::fd::{AsFd as _, AsRawFd as _};
    use yserver_core::backend::Backend;

    backend.platform.devices[0]
        .test_page_flip_event_source
        .as_ref()
        .expect("recording page-flip event source")
        .queue_flip_with_clock_samples(
            crtc,
            frame,
            kernel_ust,
            Some(received_at_monotonic),
            Some(received_at_boottime),
        );
    let drm_fd = backend.platform.devices[0].device.as_fd().as_raw_fd();
    Backend::on_page_flip_ready(backend, state, drm_fd);
}

fn expected_clock_test_present_latency(
    sink: &AtomicCommitSink,
    device: &crate::drm::Device,
    crtc: ::drm::control::crtc::Handle,
    kernel_completion_at_monotonic: Duration,
) -> u64 {
    let present_received = sink
        .for_tests_pending_direct_present_received_at(device, crtc)
        .expect("the accepted direct Present is paired with its flip");
    u64::try_from(
        kernel_completion_at_monotonic
            .saturating_sub(present_received)
            .as_micros(),
    )
    .expect("Present-to-flip latency fits u64 microseconds")
}

fn finish_clock_test_direct_async(
    backend: &mut KmsBackend,
    recorder: &std::rc::Rc<RecordingAtomicCommitExecutor>,
    state: &mut yserver_core::server::ServerState,
    window: u32,
    next_present_id: u64,
) {
    let (width, height) = backend.platform.fb_dimensions();
    assert!(
        try_direct_present_for_window(backend, next_present_id, window, width, height, 0,)
            .is_some()
    );
    assert_tearing_off(backend);
    assert_eq!(
        backend.platform.tearing_copy_received_monotonic, None,
        "tearing exit clears deferred Copy receipt time"
    );
    assert_eq!(
        backend.pstate.tearing.deferred_cursor_wake, None,
        "tearing exit clears deferred cursor wake"
    );
    retire_direct_present(backend, state, 201);
    assert_direct_queue_drained(backend);
    assert!(!backend.scene.has_pending_page_flips());
    assert_tearing_off(backend);
    assert_commit_sequence(
        recorder,
        &[AtomicCommitKind::Async, AtomicCommitKind::Scene],
    );
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: use event receipt for Present-to-flip latency, or kernel completion for turnaround.
fn delayed_flip_delivery_keeps_turnaround_and_present_latency_in_their_clock_domains() {
    use yserver_core::server::ServerState;

    const WINDOW: u32 = 0x7c89;
    let Some(mut backend) = live_scene_backend() else {
        return;
    };
    let recorder = recording_sink(&mut backend, 0);
    qualify_test_direct_async(&mut backend, &recorder, WINDOW, 16_000);
    let mut state = ServerState::new();
    let (sink, accepted_at, crtc) =
        submit_direct_async_for_clock_test(&mut backend, recorder.clone(), WINDOW, 16_100);

    let kernel_completion_at = accepted_at + Duration::from_micros(150);
    let received_at_monotonic = accepted_at + Duration::from_millis(20);
    let received_at_boottime = received_at_monotonic + Duration::from_millis(1_300);
    let expected_present_latency = expected_clock_test_present_latency(
        &sink,
        &backend.platform.devices[0].device,
        crtc,
        kernel_completion_at,
    );
    deliver_clock_test_flip(
        &mut backend,
        &mut state,
        crtc,
        95,
        kernel_completion_at,
        received_at_monotonic,
        received_at_boottime,
    );

    assert_eq!(sink.for_tests_direct_turnaround_samples(), [20_000]);
    assert_eq!(
        sink.for_tests_direct_present_to_flip_samples(),
        [expected_present_latency]
    );
    assert!(!backend.scene.has_pending_page_flips());
    finish_clock_test_direct_async(&mut backend, &recorder, &mut state, WINDOW, 16_101);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: subtract an unconverted BOOTTIME UST from the Present's MONOTONIC receipt.
fn boottime_flip_timestamp_is_converted_before_present_latency_is_sampled() {
    use yserver_core::server::ServerState;

    const WINDOW: u32 = 0x7c8a;
    let Some(mut backend) = live_scene_backend() else {
        return;
    };
    let recorder = recording_sink(&mut backend, 0);
    qualify_test_direct_async(&mut backend, &recorder, WINDOW, 16_200);
    let mut state = ServerState::new();
    let (sink, accepted_at, crtc) =
        submit_direct_async_for_clock_test(&mut backend, recorder.clone(), WINDOW, 16_300);

    let suspend_offset = Duration::from_millis(1_300);
    let kernel_completion_at = accepted_at + Duration::from_micros(150);
    let received_at_monotonic = accepted_at + Duration::from_millis(20);
    let received_at_boottime = received_at_monotonic + suspend_offset;
    let kernel_ust_boottime = kernel_completion_at + suspend_offset;
    let expected_present_latency = expected_clock_test_present_latency(
        &sink,
        &backend.platform.devices[0].device,
        crtc,
        kernel_completion_at,
    );
    deliver_clock_test_flip(
        &mut backend,
        &mut state,
        crtc,
        96,
        kernel_ust_boottime,
        received_at_monotonic,
        received_at_boottime,
    );

    assert_eq!(sink.for_tests_direct_turnaround_samples(), [20_000]);
    assert_eq!(
        sink.for_tests_direct_present_to_flip_samples(),
        [expected_present_latency]
    );
    assert!(!backend.scene.has_pending_page_flips());
    finish_clock_test_direct_async(&mut backend, &recorder, &mut state, WINDOW, 16_301);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: replace an implausible kernel timestamp with event receipt time instead of reporting n/a.
fn implausible_flip_timestamp_is_reported_as_unavailable() {
    use yserver_core::server::ServerState;

    const WINDOW: u32 = 0x7c8b;
    let Some(mut backend) = live_scene_backend() else {
        return;
    };
    let recorder = recording_sink(&mut backend, 0);
    qualify_test_direct_async(&mut backend, &recorder, WINDOW, 16_400);
    let mut state = ServerState::new();
    let (sink, accepted_at, crtc) =
        submit_direct_async_for_clock_test(&mut backend, recorder.clone(), WINDOW, 16_500);

    let received_at_monotonic = accepted_at + Duration::from_millis(20);
    let received_at_boottime = received_at_monotonic + Duration::from_millis(1_300);
    deliver_clock_test_flip(
        &mut backend,
        &mut state,
        crtc,
        97,
        Duration::ZERO,
        received_at_monotonic,
        received_at_boottime,
    );

    assert_eq!(sink.for_tests_direct_turnaround_samples(), [20_000]);
    assert!(sink.for_tests_direct_present_to_flip_samples().is_empty());
    assert_eq!(
        sink.for_tests_direct_present_to_flip_summary(),
        ("n/a".to_owned(), "n/a".to_owned(), "n/a".to_owned())
    );
    assert!(!backend.scene.has_pending_page_flips());
    finish_clock_test_direct_async(&mut backend, &recorder, &mut state, WINDOW, 16_501);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: keep a per-CRTC accepted-commit telemetry entry after discarding its old DRM event.
fn discarded_vt_flip_events_do_not_shift_the_next_async_turnaround_match() {
    use std::{
        cell::Cell,
        os::fd::{AsFd as _, AsRawFd as _},
        rc::Rc,
        time::Duration,
    };
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
                })
                .collect())
        }
    }

    const WINDOW: u32 = 0x7d90;
    let Some((mut backend, recorder, mut state)) = live_tearing_cursor_backend(WINDOW, 16_100)
    else {
        return;
    };
    backend.for_tests_set_vt_resume_executor(Rc::new(LogicalVtResume));
    backend.platform.for_tests_accept_output_power_commits();

    let accepted_at = Rc::new(Cell::new(Duration::from_millis(100)));
    let clock = Rc::clone(&accepted_at);
    let sink = Rc::new(AtomicCommitSink::new_with_monotonic_clock(
        recorder.clone(),
        Rc::new(move || Some(clock.get())),
    ));
    backend.platform.devices[0].atomic_commit_sink = Rc::clone(&sink);
    sink.set_tearing_on(true);
    backend.pstate.tearing.last_cursor_compose = None;
    Backend::warp_pointer_root(&mut backend, &mut state, 300, 180);
    Backend::maybe_composite(&mut backend).expect("submit first composed Async commit");
    assert_commit_sequence(&recorder, &[AtomicCommitKind::Async]);
    assert!(backend.scene.has_pending_page_flips());

    let crtc = backend.platform.outputs[0].output.crtc;
    let event_source = Rc::clone(
        backend.platform.devices[0]
            .test_page_flip_event_source
            .as_ref()
            .expect("recording page-flip source"),
    );
    event_source.queue_flip_at(crtc, 92, Duration::from_millis(100));
    Backend::finish_vt_release(&mut backend, &mut state, &InputInventory::default());
    assert_tearing_off(&backend);
    assert_eq!(
        event_source.queued_flip_count(),
        0,
        "VT discard consumed the old event"
    );
    assert!(!backend.scene.has_pending_page_flips());
    assert!(!backend.scanout_allowed());

    Backend::on_vt_acquire(&mut backend, &mut state);
    assert_tearing_off(&backend);
    assert_eq!(backend.pstate.vt_state, crate::vt::state::VtState::Active);
    assert!(backend.scanout_allowed());
    damage_root_and_composite(&mut backend);
    assert!(backend.scene.has_pending_page_flips());
    retire_composed_frame(&mut backend, &mut state, 93);

    state = enter_tearing_without_direct_scanout(&mut backend, WINDOW, 16_200);
    assert!(backend.pstate.tearing.is_on());
    assert_eq!(
        backend.scene.cursor_mode(),
        crate::kms::render::scene::CursorPlaneMode::Sw
    );
    recorder.clear_records();
    accepted_at.set(Duration::from_millis(200));
    backend.pstate.tearing.last_cursor_compose = None;
    Backend::warp_pointer_root(&mut backend, &mut state, 340, 220);
    Backend::maybe_composite(&mut backend).expect("submit resumed composed Async commit");
    assert_commit_sequence(&recorder, &[AtomicCommitKind::Async]);
    assert!(backend.scene.has_pending_page_flips());

    let event_at = accepted_at.get() + Duration::from_micros(175);
    event_source.queue_flip_at(crtc, 94, event_at);
    let drm_fd = backend.platform.devices[0].device.as_fd().as_raw_fd();
    Backend::on_page_flip_ready(&mut backend, &mut state, drm_fd);

    assert_eq!(sink.for_tests_composed_turnaround_samples(), [175]);
    assert_commit_sequence(&recorder, &[AtomicCommitKind::Async]);
    assert!(backend.pstate.tearing.is_on());
    assert_eq!(backend.pstate.tearing.deferred_cursor_wake, None);
    assert!(!backend.scene.has_pending_page_flips());
    assert!(!backend.scene.scene_structure_dirty);
    assert_eq!(event_source.queued_flip_count(), 0);
    assert!(backend.scanout_allowed());
    assert!(backend.kms_outputs_active);
}
