//! Per-CRTC `SetCrtcConfig` (#275): only the CRTC a client reconfigures is
//! touched, as in Xorg's `xf86RandR12CrtcSet` → `drmmode_set_mode_major`; every
//! other output keeps scanning out and presenting.

use super::{
    commit_identity::{
        recording_scene::{P1SceneRoute, RecordingSceneFixture, recording_scanout_pool},
        support::accepted_event_id,
    },
    randr::{clone_test_drm_output, test_render_device},
    test_device_key,
};
use crate::{
    kms::{
        render::{
            backend::{KmsBackend, PendingCrtcConfigProbe, PerCrtcChange},
            platform::{OutputPowerExecutor, RenderDeviceId},
        },
        scanout_route::{RenderKmsRelationship, ScanoutRoute},
        vk::scanout::{BoPhase, OutputScanout},
    },
    platform::drm::Output,
};
use std::{
    cell::{Cell, RefCell},
    io,
    os::fd::{AsFd as _, AsRawFd as _, RawFd},
    rc::Rc,
    time::Duration,
};
use yserver_core::backend::{Backend, CrtcConfigToken, ModeSpec};

/// Every modeset and disable the backend issues, as `(kind, crtc)`.
#[derive(Default)]
struct PowerLog(RefCell<Vec<(&'static str, u32)>>);

impl PowerLog {
    fn entries(&self) -> Vec<(&'static str, u32)> {
        self.0.borrow().clone()
    }
}

impl OutputPowerExecutor for PowerLog {
    fn disable(&self, _device: &crate::drm::Device, output: &Output) -> io::Result<()> {
        self.0.borrow_mut().push(("off", u32::from(output.crtc)));
        Ok(())
    }

    fn enable(
        &self,
        _device: &crate::drm::Device,
        output: &Output,
        _framebuffer: ::drm::control::framebuffer::Handle,
    ) -> io::Result<()> {
        self.0.borrow_mut().push(("on", u32::from(output.crtc)));
        Ok(())
    }

    fn disable_device(&self, _device: &crate::drm::Device) -> io::Result<()> {
        self.0.borrow_mut().push(("device", 0));
        Ok(())
    }
}

struct RejectModeset;

impl OutputPowerExecutor for RejectModeset {
    fn disable(&self, _device: &crate::drm::Device, _output: &Output) -> io::Result<()> {
        Ok(())
    }

    fn enable(
        &self,
        _device: &crate::drm::Device,
        _output: &Output,
        _framebuffer: ::drm::control::framebuffer::Handle,
    ) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(libc::EIO))
    }

    fn disable_device(&self, _device: &crate::drm::Device) -> io::Result<()> {
        Ok(())
    }
}

const MODE_800: ModeSpec = ModeSpec {
    width: 800,
    height: 600,
    vrefresh: 60,
};

/// Render on the display device itself: the only route the per-CRTC path
/// takes.
fn use_same_device_route(backend: &mut KmsBackend) -> ScanoutRoute {
    let display = backend.platform.devices[0].key;
    let render_id = RenderDeviceId::DrmRender(test_device_key(128));
    backend.platform.render_devices = vec![test_render_device(render_id, Some(display))];
    backend.platform.selected_render_device = Some(render_id);
    let route = ScanoutRoute::new(render_id, display, RenderKmsRelationship::Same);
    for output in &mut backend.platform.outputs {
        output.scanout_route = route;
    }
    for pool in backend.platform.scanout_pools.iter_mut().flatten() {
        pool.display_pool_mut().route = route;
    }
    route
}

fn install_power_log(backend: &mut KmsBackend) -> Rc<PowerLog> {
    let log = Rc::new(PowerLog::default());
    backend
        .platform
        .for_tests_set_output_power_executor(log.clone());
    log
}

fn output_id(backend: &mut KmsBackend, connector: &str) -> u32 {
    Backend::randr_outputs_and_modes(backend)
        .0
        .into_iter()
        .find(|output| output.name == connector)
        .expect("output in the RandR projection")
        .output_id
}

fn handle<H: ::drm::control::ResourceHandle>(raw: u32) -> H {
    ::drm::control::from_u32(raw).expect("nonzero handle")
}

fn crtc(backend: &KmsBackend, idx: usize) -> u32 {
    u32::from(backend.platform.outputs[idx].output.crtc)
}

fn connector(backend: &KmsBackend, idx: usize) -> String {
    backend.platform.outputs[idx].key.connector_name.clone()
}

/// Two side-by-side 800×600 outputs on one device, no Vulkan.
fn two_heads() -> (KmsBackend, Rc<PowerLog>) {
    let mut backend = KmsBackend::for_tests();
    super::push_test_output(&mut backend, 201);
    backend.platform.outputs[1].x = 800;
    backend.platform.scanout_pools.resize_with(2, || None);
    backend.platform.bo_generations.resize_with(2, Vec::new);
    backend.platform.first_pageflip_logged.resize(2, false);
    backend.platform.recompute_fb_extent();
    use_same_device_route(&mut backend);
    let log = install_power_log(&mut backend);
    (backend, log)
}

#[test]
// Mutation: truncate the tail owner slot when output 0 is removed.
fn output_removal_keeps_owner_state_with_its_output_per_crtc_off() {
    use yserver_core::backend::{CompletedPresentEvent, PresentWake};

    let (mut backend, log) = two_heads();
    let make_event = |present_id, completion_mode, emit_idle| CompletedPresentEvent {
        client_id: yserver_protocol::x11::ClientId(1),
        serial: u32::try_from(present_id).expect("test serial fits"),
        host_xid: 0,
        dst_host_xid: 0,
        options: 0,
        present_id,
        window_generation: 0,
        crtc_id: 0,
        crtc_epoch: 0,
        msc_offset: 0,
        completion_clock: None,
        wake: PresentWake::Pixmap { idle_fence_xid: 0 },
        completion_mode,
        emit_idle,
    };
    {
        let direct = &mut backend.pstate.direct;
        direct.root_mut().completed.push(make_event(
            100,
            yserver_protocol::x11::present::COMPLETE_MODE_COPY,
            true,
        ));
        direct.root_mut().deferred_successor_skips.push(make_event(
            110,
            yserver_protocol::x11::present::COMPLETE_MODE_SKIP,
            false,
        ));
        direct.root_mut().idled.push(make_event(
            120,
            yserver_protocol::x11::present::COMPLETE_MODE_COPY,
            true,
        ));
        direct.outputs[0].completed.push(make_event(
            101,
            yserver_protocol::x11::present::COMPLETE_MODE_COPY,
            true,
        ));
        direct.outputs[0].deferred_successor_skips.push(make_event(
            111,
            yserver_protocol::x11::present::COMPLETE_MODE_SKIP,
            false,
        ));
        direct.outputs[0].idled.push(make_event(
            121,
            yserver_protocol::x11::present::COMPLETE_MODE_COPY,
            true,
        ));
        direct.outputs[1].eligible_streak = 3;
        direct.outputs[1].reentry_blocked_until_composed = true;
    }
    let removed_crtc = crtc(&backend, 0);
    let survivor_crtc = crtc(&backend, 1);
    let removed_key =
        crate::kms::render::platform::CrtcKey::for_output(&backend.platform.outputs[0]);
    let survivor_key =
        crate::kms::render::platform::CrtcKey::for_output(&backend.platform.outputs[1]);
    super::seed_vblank_arm_watch(
        &mut backend,
        removed_key,
        super::SHARED_WAKE_VBLANK_ARM_SLOT,
    );
    super::seed_vblank_arm_watch(
        &mut backend,
        survivor_key,
        super::SHARED_WAKE_VBLANK_ARM_SLOT,
    );
    let name = connector(&backend, 0);
    let id = output_id(&mut backend, &name);

    assert!(
        Backend::apply_crtc_config(&mut backend, id, &name, None, 0, 0)
            .expect("turn output 0 off through Backend")
    );

    assert_eq!(
        log.entries(),
        vec![("off", removed_crtc)],
        "per-CRTC off route"
    );
    assert_eq!(backend.platform.outputs.len(), 1);
    assert_eq!(crtc(&backend, 0), survivor_crtc);
    assert!(!super::vblank_arm_is_watched(
        &backend,
        removed_key,
        super::SHARED_WAKE_VBLANK_ARM_SLOT
    ));
    assert!(super::vblank_arm_is_watched(
        &backend,
        survivor_key,
        super::SHARED_WAKE_VBLANK_ARM_SLOT
    ));
    assert_eq!(backend.pstate.direct.outputs.len(), 1);
    assert_eq!(backend.pstate.direct.outputs[0].eligible_streak, 3);
    assert!(backend.pstate.direct.outputs[0].reentry_blocked_until_composed);
    assert_eq!(
        backend
            .pstate
            .direct
            .root()
            .completed
            .iter()
            .map(|event| event.present_id)
            .collect::<Vec<_>>(),
        [100, 101],
        "the removed slot's completions append to Root"
    );
    assert_eq!(
        backend
            .pstate
            .direct
            .root()
            .deferred_successor_skips
            .iter()
            .map(|event| event.present_id)
            .collect::<Vec<_>>(),
        [110, 111],
        "the removed slot's deferred skips append to Root"
    );
    assert_eq!(
        backend
            .pstate
            .direct
            .root()
            .idled
            .iter()
            .map(|event| event.present_id)
            .collect::<Vec<_>>(),
        [120, 121],
        "the removed slot's Idle events append to Root"
    );
}

#[test]
// Mutation: drop the position branch in apply_crtc_config (the global
// quiesce turns both CRTCs off and back on).
fn position_change_moves_one_crtc_without_any_commit() {
    let (mut backend, log) = two_heads();
    let first = (
        crtc(&backend, 0),
        backend.platform.outputs[0].x,
        backend.platform.outputs[0].y,
    );
    let name = connector(&backend, 1);
    let id = output_id(&mut backend, &name);

    assert!(
        Backend::apply_crtc_config(&mut backend, id, &name, Some(MODE_800), 1000, 0)
            .expect("move the second output")
    );

    assert!(log.entries().is_empty(), "commits: {:?}", log.entries());
    assert_eq!(
        (backend.platform.outputs[1].x, backend.platform.outputs[1].y),
        (1000, 0)
    );
    assert_eq!(
        (
            crtc(&backend, 0),
            backend.platform.outputs[0].x,
            backend.platform.outputs[0].y
        ),
        first
    );
    assert_eq!((backend.platform.fb_w, backend.platform.fb_h), (1800, 600));
}

#[test]
// Mutation: refresh the whole device's cursor topology on a position change
// (every CRTC's queued cursor move is dropped).
fn position_change_keeps_the_other_crtcs_queued_cursor_move() {
    let (mut backend, _log) = two_heads();
    let device_key = backend.platform.devices[0].key;
    let queued0 = backend.platform.for_tests_seed_atomic_cursor_position(
        0,
        handle(91),
        handle(92),
        handle(93),
        100,
        100,
        0,
        0,
    );
    backend.platform.for_tests_seed_atomic_cursor_position(
        1,
        handle(91),
        handle(92),
        handle(93),
        900,
        100,
        0,
        0,
    );
    let (crtc0, crtc1) = (
        backend.platform.outputs[0].output.crtc,
        backend.platform.outputs[1].output.crtc,
    );
    let name = connector(&backend, 1);
    let id = output_id(&mut backend, &name);

    assert!(
        Backend::apply_crtc_config(&mut backend, id, &name, Some(MODE_800), 1000, 0)
            .expect("move the second output")
    );

    assert_eq!(
        backend
            .platform
            .for_tests_atomic_cursor_pending_position(device_key, crtc0),
        Some(queued0),
        "the untouched CRTC keeps its queued move"
    );
    assert_eq!(
        backend
            .platform
            .for_tests_atomic_cursor_pending_position(device_key, crtc1),
        None,
        "the moved CRTC's move was computed from its old origin"
    );
}

#[test]
// Mutation: drop bump_crtc_config_topology_epoch from apply_crtc_position.
fn position_change_retires_an_older_crtc_probe() {
    let (mut backend, _log) = two_heads();
    let name = connector(&backend, 1);
    let id = output_id(&mut backend, &name);
    let token = CrtcConfigToken(77);
    let output_key = backend.platform.outputs[0].key.clone();
    let route = backend.platform.outputs[0].scanout_route;
    let pending = PendingCrtcConfigProbe {
        output_id: id,
        output_key,
        connector: name.clone(),
        mode: MODE_800,
        x: 0,
        y: 0,
        route,
        prepared_output: None,
        topology_signature: 0,
        topology_epoch: backend.pstate.crtc_config_topology_epoch,
        vt_state: backend.pstate.vt_state,
        was_active: true,
    };
    backend
        .pstate
        .pending_crtc_config_probes
        .insert(token, pending);

    assert!(
        Backend::apply_crtc_config(&mut backend, id, &name, Some(MODE_800), 0, 600)
            .expect("move the second output below the first")
    );

    let result = backend
        .pstate
        .ready_crtc_config_results
        .get(&token)
        .expect("the older probe was retired by the layout change");
    assert_eq!(
        result.as_ref().map_err(io::Error::kind).err(),
        Some(io::ErrorKind::Interrupted)
    );
}

#[test]
// Mutation: let per_crtc_fallback_reason accept a cross-device route.
fn cross_device_route_still_takes_the_global_quiesce() {
    let (mut backend, log) = two_heads();
    let render = backend.platform.outputs[1].scanout_route.render_device_id;
    backend.platform.outputs[1].scanout_route = ScanoutRoute::new(
        render,
        backend.platform.devices[0].key,
        RenderKmsRelationship::Different,
    );
    backend.pstate.crtc_config_discovery_override =
        Some(clone_test_drm_output(&backend.platform.outputs[1].output));
    let name = connector(&backend, 1);
    let id = output_id(&mut backend, &name);
    let untouched = crtc(&backend, 0);

    // The fd-less fixture cannot relight; only which path ran matters.
    let _ = Backend::apply_crtc_config(&mut backend, id, &name, Some(MODE_800), 1000, 0);

    assert!(
        log.entries().contains(&("off", untouched)),
        "the global quiesce turns the untouched CRTC off too: {:?}",
        log.entries()
    );
}

#[test]
// Mutation: drop the moved-route filter in apply_crtc_config (the per-CRTC
// modeset would leave the old CRTC lit without its connector).
fn mode_change_whose_discovery_moves_the_crtc_takes_the_global_quiesce() {
    let (mut backend, log) = two_heads();
    let mut discovered = clone_test_drm_output(&backend.platform.outputs[1].output);
    discovered.crtc = ::drm::control::from_u32(202).expect("nonzero CRTC");
    discovered.picked = advertised(640, 480);
    discovered.modes.push(advertised(640, 480));
    backend.pstate.crtc_config_discovery_override = Some(discovered);
    let name = connector(&backend, 1);
    let id = output_id(&mut backend, &name);
    let untouched = crtc(&backend, 0);
    let mode = ModeSpec {
        width: 640,
        height: 480,
        vrefresh: 60,
    };

    // The fd-less fixture cannot relight; only which path ran matters.
    let _ = Backend::apply_crtc_config(&mut backend, id, &name, Some(mode), 800, 0);

    assert!(
        log.entries().contains(&("off", untouched)),
        "the global quiesce turns the untouched CRTC off too: {:?}",
        log.entries()
    );
}

#[test]
// Mutation: proceed with the per-CRTC modeset after the wait times out.
fn a_flip_that_never_retires_falls_back_to_the_global_quiesce() {
    let (mut backend, log) = two_heads();
    // Output 1 owes a flip whose event never arrives.
    backend.scene.test_set_flip_in_flight(true);
    let mut discovered = clone_test_drm_output(&backend.platform.outputs[1].output);
    discovered.picked = advertised(640, 480);
    discovered.modes.push(advertised(640, 480));
    backend.pstate.crtc_config_discovery_override = Some(discovered);
    let name = connector(&backend, 1);
    let id = output_id(&mut backend, &name);
    let untouched = crtc(&backend, 0);
    let mode = ModeSpec {
        width: 640,
        height: 480,
        vrefresh: 60,
    };

    // The fd-less fixture cannot relight; only which path ran matters.
    let _ = Backend::apply_crtc_config(&mut backend, id, &name, Some(mode), 800, 0);

    assert!(
        log.entries().contains(&("off", untouched)),
        "the global quiesce turns the untouched CRTC off too: {:?}",
        log.entries()
    );
    assert_eq!(backend.pstate.crtc_config_gated_crtc, None);
}

#[test]
// Mutation: drop the scanout_m2.active() check from per_crtc_fallback_reason.
fn direct_scanout_takes_the_global_quiesce() {
    let (mut backend, _log) = two_heads();
    let key = backend.platform.outputs[1].key.clone();
    assert_eq!(
        backend.per_crtc_fallback_reason(&key, PerCrtcChange::Mode),
        None
    );
    let target_xid = 0x5710;
    let fallback_id = super::seed_window(&mut backend, target_xid, None, 0, 0);
    super::install_direct_frame_for_target_test(&mut backend, target_xid, fallback_id, true);
    for change in [
        PerCrtcChange::Position,
        PerCrtcChange::Mode,
        PerCrtcChange::Off,
    ] {
        assert_eq!(
            backend.per_crtc_fallback_reason(&key, change),
            Some("direct scanout active")
        );
    }
}

// ── Live scene (lavapipe) ────────────────────────────────────────────────

fn live_two_heads() -> Option<(RecordingSceneFixture, Rc<PowerLog>, ScanoutRoute)> {
    let mut fixture =
        RecordingSceneFixture::new(2).expect("two-output recording scene fixture construction")?;
    let route = use_same_device_route(&mut fixture.events.backend);
    let log = install_power_log(&mut fixture.events.backend);
    Some((fixture, log, route))
}

fn install_fake_clock(backend: &mut KmsBackend) -> Rc<Cell<std::time::Instant>> {
    let now = Rc::new(Cell::new(std::time::Instant::now()));
    let clock = Rc::clone(&now);
    backend.for_tests_set_monotonic_clock(Rc::new(move || clock.get()));
    now
}

fn freeze_output_for_test(
    backend: &mut KmsBackend,
    output_idx: usize,
    now: &Cell<std::time::Instant>,
) -> (
    crate::kms::render::platform::CrtcKey,
    u32,
    Rc<crate::kms::render::platform::flip_events::RecordingPageFlipEventSource>,
) {
    use yserver_core::{backend::Backend, server::ServerState};

    let key =
        crate::kms::render::platform::CrtcKey::for_output(&backend.platform.outputs[output_idx]);
    let projected = Backend::randr_outputs_and_modes(backend).0;
    let crtc_id = projected
        .get(output_idx)
        .expect("live output has a RandR projection")
        .crtc_id;
    let source = backend.platform.devices[0]
        .test_page_flip_event_source
        .clone()
        .expect("recording fixture has a page-flip source");
    source.set_legacy_vblank(Ok(5));
    backend
        .pstate
        .crtc_queue_sequence_unsupported_devices
        .insert(key.device_key);
    backend
        .platform
        .vblank_fallback
        .pipes
        .insert(key, output_idx as u32);
    assert!(
        Backend::arm_present_shared_wake(backend, crtc_id)
            .expect("arm the selected CRTC's kernel wake")
    );
    let armed_at = now.get();
    now.set(armed_at + KmsBackend::FROZEN_VBLANK_WINDOW);
    backend.poll_deferred_input(&mut ServerState::new());
    assert!(backend.platform.vblank_fallback.frozen.contains_key(&key));
    (key, crtc_id, source)
}

fn device_fd(fixture: &RecordingSceneFixture) -> RawFd {
    fixture.events.backend.platform.devices[0]
        .device
        .as_fd()
        .as_raw_fd()
}

/// Damage the whole root and compose once. Returns each accepted scene flip
/// as `(crtc, commit id)`.
fn submit_frame(fixture: &mut RecordingSceneFixture) -> Vec<(u32, u64)> {
    let fd = device_fd(fixture);
    let offset = fixture.events.recorders[&fd].records().len();
    let backend = &mut fixture.events.backend;
    let root = backend.core.window_id;
    let (width, height) = (backend.platform.fb_w, backend.platform.fb_h);
    Backend::clear_area(backend, None, root, 0, None, 0, 0, width, height, (0, 0))
        .expect("damage root through Backend");
    Backend::maybe_composite(backend).expect("compose through Backend");
    fixture.events.recorders[&fd]
        .records()
        .into_iter()
        .skip(offset)
        .filter_map(|record| {
            let id = accepted_event_id(&record)?;
            Some(
                record
                    .events
                    .iter()
                    .map(|event| (u32::from(event.crtc), id))
                    .collect::<Vec<_>>(),
            )
        })
        .flatten()
        .collect()
}

fn flip_for(flips: &[(u32, u64)], crtc: u32) -> u64 {
    flips
        .iter()
        .find(|(flipped, _)| *flipped == crtc)
        .unwrap_or_else(|| panic!("no scene flip on CRTC {crtc}: {flips:?}"))
        .1
}

/// The kernel has the flip's completion ready on the DRM fd.
fn queue_completion(
    fixture: &mut RecordingSceneFixture,
    crtc: u32,
    id: u64,
    frame: u32,
    received_at: Option<Duration>,
) {
    fixture.events.wait_for_submitted_gpu_work();
    let fd = device_fd(fixture);
    let crtc = ::drm::control::from_u32(crtc).expect("nonzero CRTC");
    let ust = Duration::from_millis(u64::from(frame) * 10);
    if let Some(received_at) = received_at {
        fixture
            .events
            .queue_commit_completion_at(fd, crtc, id, frame, ust, received_at);
    } else {
        fixture
            .events
            .queue_commit_completion(fd, crtc, id, frame, ust);
    }
}

/// Deliver a flip completion through the core loop's DRM handler.
fn retire(
    fixture: &mut RecordingSceneFixture,
    crtc: u32,
    id: u64,
    frame: u32,
    received_at: Option<Duration>,
) {
    queue_completion(fixture, crtc, id, frame, received_at);
    let fd = device_fd(fixture);
    Backend::on_page_flip_ready(&mut fixture.events.backend, &mut fixture.events.state, fd);
}

fn retire_all(
    fixture: &mut RecordingSceneFixture,
    flips: &[(u32, u64)],
    frame: u32,
    received_at: Option<Duration>,
) {
    if let Some(received_at) = received_at {
        for &(crtc, id) in flips {
            queue_completion(fixture, crtc, id, frame, Some(received_at));
        }
        let fd = device_fd(fixture);
        Backend::on_page_flip_ready(&mut fixture.events.backend, &mut fixture.events.state, fd);
    } else {
        for &(crtc, id) in flips {
            retire(fixture, crtc, id, frame, None);
        }
    }
}

/// Completions queued on the DRM fd and not yet delivered.
fn undelivered(fixture: &RecordingSceneFixture) -> usize {
    fixture.events.event_sources[&device_fd(fixture)].queued_flip_count()
}

fn bo_phases(backend: &KmsBackend, idx: usize) -> Vec<BoPhase> {
    backend.platform.scanout_pools[idx]
        .as_ref()
        .expect("live output has a pool")
        .display_pool()
        .bos
        .iter()
        .map(|bo| bo.state.phase)
        .collect()
}

fn count(phases: &[BoPhase], phase: BoPhase) -> usize {
    phases
        .iter()
        .filter(|candidate| **candidate == phase)
        .count()
}

fn advertised(width: u16, height: u16) -> crate::platform::drm::Mode {
    crate::platform::drm::Mode {
        name: format!("{width}x{height}"),
        width,
        height,
        vrefresh: 60,
        ..Default::default()
    }
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: take the global quiesce for a mode change (both CRTCs are turned
// off), drain every scene output instead of the changed one (output 0's
// in-flight frame is lost), or retain the changed CRTC's shared wake slot.
fn mode_change_modesets_only_its_crtc_while_the_other_keeps_presenting() {
    let Some((mut fixture, log, route)) = live_two_heads() else {
        return;
    };
    let steady = submit_frame(&mut fixture);
    retire_all(&mut fixture, &steady, 1, None);
    let (crtc0, crtc1) = (
        crtc(&fixture.events.backend, 0),
        crtc(&fixture.events.backend, 1),
    );
    let crtc_key0 = crate::kms::render::platform::CrtcKey::for_output(
        &fixture.events.backend.platform.outputs[0],
    );
    let crtc_key1 = crate::kms::render::platform::CrtcKey::for_output(
        &fixture.events.backend.platform.outputs[1],
    );
    super::seed_vblank_arm_watch(
        &mut fixture.events.backend,
        crtc_key0,
        super::SHARED_WAKE_VBLANK_ARM_SLOT,
    );
    super::seed_vblank_arm_watch(
        &mut fixture.events.backend,
        crtc_key1,
        super::SHARED_WAKE_VBLANK_ARM_SLOT,
    );
    let in_flight = submit_frame(&mut fixture);
    let (flip0, flip1) = (flip_for(&in_flight, crtc0), flip_for(&in_flight, crtc1));
    // CRTC 1's flip completes while the change waits for it; CRTC 0's stays
    // in flight across the whole change.
    queue_completion(&mut fixture, crtc1, flip1, 2, None);

    let backend = &mut fixture.events.backend;
    let key1 = backend.platform.outputs[1].key.clone();
    let mut discovered = clone_test_drm_output(&backend.platform.outputs[1].output);
    discovered.picked = advertised(640, 480);
    discovered.modes.push(advertised(640, 480));
    backend.pstate.crtc_config_discovery_override = Some(discovered);
    let pool = recording_scanout_pool(backend, &key1, route, (640, 480), 0x6b00, 0x5300)
        .expect("replacement 640x480 pool");
    backend.pstate.crtc_config_discovery_pool = Some((route, pool));
    let name = connector(backend, 1);
    let id = output_id(backend, &name);
    let epoch = backend.pstate.crtc_config_topology_epoch;
    let mode = ModeSpec {
        width: 640,
        height: 480,
        vrefresh: 60,
    };

    assert!(
        Backend::apply_crtc_config(backend, id, &name, Some(mode), 800, 0)
            .expect("mode change on the second output")
    );

    assert_eq!(log.entries(), vec![("on", crtc1)]);
    assert!(!super::vblank_arm_is_watched(
        backend,
        crtc_key1,
        super::SHARED_WAKE_VBLANK_ARM_SLOT
    ));
    assert!(super::vblank_arm_is_watched(
        backend,
        crtc_key0,
        super::SHARED_WAKE_VBLANK_ARM_SLOT
    ));
    let changed_crtc_randr_id = backend
        .pstate
        .randr_id_alloc
        .entry(&backend.platform.outputs[1].key)
        .expect("changed CRTC keeps its RandR resource")
        .ids
        .crtc_id;
    let wake_source = backend.platform.devices[0]
        .test_page_flip_event_source
        .clone()
        .expect("fixture records vblank requests");
    backend
        .pstate
        .crtc_queue_sequence_unsupported_devices
        .insert(crtc_key1.device_key);
    backend.platform.vblank_fallback.pipes.insert(crtc_key1, 1);
    wake_source.set_legacy_vblank(Ok(5));
    assert!(
        Backend::arm_present_shared_wake(backend, changed_crtc_randr_id)
            .expect("changed CRTC can arm a fresh wake")
    );
    assert_eq!(wake_source.legacy_vblank_arms().len(), 1);
    assert_ne!(backend.pstate.crtc_config_topology_epoch, epoch);
    assert_eq!(
        undelivered(&fixture),
        0,
        "CRTC 1's flip retired before its modeset"
    );
    let backend = &fixture.events.backend;
    assert_eq!(backend.platform.outputs[1].width, 640);
    assert_eq!(backend.pstate.crtc_config_gated_crtc, None);
    assert!(
        backend.scene.has_pending_page_flip(0),
        "output 0's frame is still in flight"
    );
    assert_eq!(count(&bo_phases(backend, 0), BoPhase::Pending), 1);
    assert!(!backend.scene.has_pending_page_flip(1));

    retire(&mut fixture, crtc0, flip0, 2, None);
    let backend = &fixture.events.backend;
    assert!(!backend.scene.has_pending_page_flip(0));
    assert_eq!(count(&bo_phases(backend, 0), BoPhase::Pending), 0);

    let next = submit_frame(&mut fixture);
    flip_for(&next, crtc0);
    flip_for(&next, crtc1);
    retire_all(&mut fixture, &next, 3, None);
    fixture.adopt_drawables(0x6b00..0x6b03);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: clear the arm slots before a modeset that is rejected.
fn rejected_mode_change_preserves_arm_slots_and_watch_deadlines() {
    let Some((mut fixture, _log, _route)) = live_two_heads() else {
        return;
    };
    let backend = &mut fixture.events.backend;
    let key = crate::kms::render::platform::CrtcKey::for_output(&backend.platform.outputs[1]);
    let crtc_id = Backend::randr_outputs_and_modes(backend).0[1].crtc_id;
    let source = backend.platform.devices[0]
        .test_page_flip_event_source
        .clone()
        .expect("fixture records vblank requests");
    source.set_legacy_vblank(Ok(5));
    backend
        .pstate
        .crtc_queue_sequence_unsupported_devices
        .insert(key.device_key);
    backend.platform.vblank_fallback.pipes.insert(key, 1);
    let now = install_fake_clock(backend);

    assert!(Backend::arm_present_shared_wake(backend, crtc_id).expect("arm shared wake"));
    assert_eq!(
        Backend::arm_idle_vblanks(backend, crtc_id, &[9]).expect("arm idle slot"),
        1
    );
    let watches_before = backend.platform.vblank_fallback.kernel_arm_watch.clone();
    let arms_before = source.legacy_vblank_arms().len();
    assert_eq!(watches_before.len(), 2, "both independent slots are armed");

    let output_key = backend.platform.outputs[1].key.clone();
    let mut discovered = clone_test_drm_output(&backend.platform.outputs[1].output);
    discovered.picked.vrefresh = 75;
    let mut alternate = advertised(800, 600);
    alternate.vrefresh = 75;
    discovered.modes.push(alternate);
    backend.pstate.crtc_config_discovery_override = Some(discovered);
    backend
        .platform
        .for_tests_set_output_power_executor(Rc::new(RejectModeset));
    let connector_name = connector(backend, 1);
    let output_id = output_id(backend, &connector_name);
    let error = Backend::apply_crtc_config(
        backend,
        output_id,
        &connector_name,
        Some(ModeSpec {
            width: 800,
            height: 600,
            vrefresh: 75,
        }),
        800,
        0,
    )
    .expect_err("recording power executor rejects the modeset");
    assert_eq!(error.raw_os_error(), Some(libc::EIO));
    assert_eq!(backend.platform.outputs[1].key, output_key);
    assert_eq!(backend.platform.outputs[1].output.picked.vrefresh, 60);

    now.set(now.get() + Duration::from_secs(2));
    assert!(Backend::arm_present_shared_wake(backend, crtc_id).expect("existing wake covers"));
    assert_eq!(
        Backend::arm_idle_vblanks(backend, crtc_id, &[9]).expect("existing idle slot covers"),
        0
    );
    assert_eq!(source.legacy_vblank_arms().len(), arms_before);
    assert_eq!(
        backend.platform.vblank_fallback.kernel_arm_watch, watches_before,
        "a refused modeset preserves each outstanding slot's original deadline"
    );
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: remove a different scene slot than the platform (or none), or
// drop the survivor's pending acknowledgement: its flip then never retires
// against its pool.
fn turning_one_output_off_migrates_the_survivors_in_flight_frame() {
    let Some((mut fixture, log, _route)) = live_two_heads() else {
        return;
    };
    let steady = submit_frame(&mut fixture);
    retire_all(&mut fixture, &steady, 1, None);
    let (crtc0, crtc1) = (
        crtc(&fixture.events.backend, 0),
        crtc(&fixture.events.backend, 1),
    );
    let in_flight = submit_frame(&mut fixture);
    let (flip0, flip1) = (flip_for(&in_flight, crtc0), flip_for(&in_flight, crtc1));
    queue_completion(&mut fixture, crtc0, flip0, 2, None);

    let backend = &mut fixture.events.backend;
    let name = connector(backend, 0);
    let id = output_id(backend, &name);
    let epoch = backend.pstate.crtc_config_topology_epoch;
    assert!(
        Backend::apply_crtc_config(backend, id, &name, None, 0, 0)
            .expect("turn the first output off")
    );

    assert_eq!(log.entries(), vec![("off", crtc0)]);
    assert_ne!(backend.pstate.crtc_config_topology_epoch, epoch);
    assert_eq!(
        undelivered(&fixture),
        0,
        "CRTC 0's flip retired before its disable"
    );
    let backend = &fixture.events.backend;
    assert_eq!(backend.platform.outputs.len(), 1);
    assert_eq!(crtc(backend, 0), crtc1);
    assert!(
        backend.scene.has_pending_page_flip(0),
        "the survivor's in-flight frame moved to index 0 with its pool"
    );
    assert_eq!(count(&bo_phases(backend, 0), BoPhase::Pending), 1);

    retire(&mut fixture, crtc1, flip1, 2, None);
    let backend = &fixture.events.backend;
    assert!(!backend.scene.has_pending_page_flip(0));
    let phases = bo_phases(backend, 0);
    assert_eq!(count(&phases, BoPhase::OnScreen), 1, "{phases:?}");
    assert_eq!(
        count(&phases, BoPhase::Free),
        phases.len() - 1,
        "{phases:?}"
    );

    let next = submit_frame(&mut fixture);
    assert_eq!(next.len(), 1, "{next:?}");
    flip_for(&next, crtc1);
    retire_all(&mut fixture, &next, 3, None);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: take the global quiesce for an enable (the lit CRTC 0 is turned
// off and on again).
fn enabling_an_off_output_modesets_only_its_crtc() {
    let Some((mut fixture, log, route)) = live_two_heads() else {
        return;
    };
    let steady = submit_frame(&mut fixture);
    retire_all(&mut fixture, &steady, 1, None);
    let backend = &mut fixture.events.backend;
    let (crtc0, crtc1) = (crtc(backend, 0), crtc(backend, 1));
    let key1 = backend.platform.outputs[1].key.clone();
    let saved = clone_test_drm_output(&backend.platform.outputs[1].output);
    let name = connector(backend, 1);
    let id = output_id(backend, &name);
    assert!(
        Backend::apply_crtc_config(backend, id, &name, None, 0, 0)
            .expect("turn the second output off")
    );
    assert_eq!(log.entries(), vec![("off", crtc1)]);

    backend.pstate.crtc_config_discovery_override = Some(saved);
    let pool = recording_scanout_pool(backend, &key1, route, (800, 600), 0x6b00, 0x5300)
        .expect("pool for the re-enabled output");
    backend.pstate.crtc_config_discovery_pool = Some((route, pool));
    let epoch = backend.pstate.crtc_config_topology_epoch;
    assert!(
        Backend::apply_crtc_config(backend, id, &name, Some(MODE_800), 800, 0)
            .expect("enable the second output again")
    );

    assert_eq!(log.entries(), vec![("off", crtc1), ("on", crtc1)]);
    assert_ne!(backend.pstate.crtc_config_topology_epoch, epoch);
    assert_eq!(backend.platform.outputs.len(), 2);
    assert_eq!(crtc(backend, 0), crtc0);
    let next = submit_frame(&mut fixture);
    flip_for(&next, crtc0);
    flip_for(&next, crtc1);
    retire_all(&mut fixture, &next, 2, None);
    fixture.adopt_drawables(0x6b00..0x6b03);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: clear the mark on a same-route modeset, or keep the old interval.
fn same_route_modeset_keeps_the_mark_and_never_rewinds() {
    let Some((mut fixture, _log, _route)) = live_two_heads() else {
        println!(
            "SKIP same_route_modeset_keeps_the_mark_and_never_rewinds: Vulkan fixture unavailable"
        );
        return;
    };
    let now = install_fake_clock(&mut fixture.events.backend);
    let (key, crtc_id, _source) = freeze_output_for_test(&mut fixture.events.backend, 1, &now);
    now.set(now.get() + Duration::from_secs(1));
    let before = Backend::present_get_ust_msc(&fixture.events.backend, crtc_id);

    let backend = &mut fixture.events.backend;
    let mut discovered = clone_test_drm_output(&backend.platform.outputs[1].output);
    let mut timing = advertised(800, 600);
    timing.vrefresh = 240;
    timing.clock_khz = 0;
    timing.htotal = 0;
    timing.vtotal = 0;
    discovered.picked = timing.clone();
    discovered.modes.push(timing);
    backend.pstate.crtc_config_discovery_override = Some(discovered);
    let connector_name = connector(backend, 1);
    let output_id = output_id(backend, &connector_name);
    assert!(
        Backend::apply_crtc_config(
            backend,
            output_id,
            &connector_name,
            Some(ModeSpec {
                width: 800,
                height: 600,
                vrefresh: 240,
            }),
            800,
            0,
        )
        .expect("same-route 240 Hz mode change")
    );

    let frozen = backend.platform.vblank_fallback.frozen[&key];
    assert_eq!(
        frozen.anchor_msc, before.0,
        "modeset anchors at the current MSC"
    );
    assert_eq!(
        frozen.anchor_ust, before.1,
        "modeset anchors at the current UST"
    );
    assert_eq!(frozen.anchor_at, now.get());
    assert_eq!(frozen.interval, Duration::from_nanos(1_000_000_000 / 240));
    now.set(now.get() + frozen.interval * 2);
    assert_eq!(
        Backend::present_get_ust_msc(backend, crtc_id).0,
        before.0 + 2,
        "the re-anchored clock advances at the new mode's rate"
    );
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: retain a frozen mark after disable, preventing the kernel arm on enable.
fn disable_and_enable_returns_to_the_kernel_clock() {
    let Some((mut fixture, _log, route)) = live_two_heads() else {
        println!("SKIP disable_and_enable_returns_to_the_kernel_clock: Vulkan fixture unavailable");
        return;
    };
    let now = install_fake_clock(&mut fixture.events.backend);
    let (key, crtc_id, source) = freeze_output_for_test(&mut fixture.events.backend, 1, &now);
    let backend = &mut fixture.events.backend;
    let output_key = backend.platform.outputs[1].key.clone();
    let saved = clone_test_drm_output(&backend.platform.outputs[1].output);
    let connector_name = connector(backend, 1);
    let output_id = output_id(backend, &connector_name);
    assert!(
        Backend::apply_crtc_config(backend, output_id, &connector_name, None, 0, 0,)
            .expect("disable frozen CRTC")
    );
    assert!(!backend.platform.vblank_fallback.frozen.contains_key(&key));
    assert!(
        !backend
            .platform
            .vblank_fallback
            .kernel_arm_watch
            .keys()
            .any(|(watched, _)| *watched == key)
    );

    backend.pstate.crtc_config_discovery_override = Some(saved);
    let pool = recording_scanout_pool(backend, &output_key, route, (800, 600), 0x6b00, 0x5300)
        .expect("replacement pool for re-enable");
    backend.pstate.crtc_config_discovery_pool = Some((route, pool));
    assert!(
        Backend::apply_crtc_config(backend, output_id, &connector_name, Some(MODE_800), 800, 0,)
            .expect("re-enable CRTC")
    );
    assert!(!backend.platform.vblank_fallback.frozen.contains_key(&key));
    backend
        .pstate
        .crtc_queue_sequence_unsupported_devices
        .insert(key.device_key);
    backend.platform.vblank_fallback.pipes.insert(key, 1);
    source.set_legacy_vblank(Ok(5));
    assert!(
        Backend::arm_present_shared_wake(backend, crtc_id)
            .expect("re-enabled CRTC arms the kernel wake")
    );
    assert!(super::vblank_arm_is_watched(
        backend,
        key,
        super::SHARED_WAKE_VBLANK_ARM_SLOT
    ));
    assert!(
        backend
            .platform
            .vblank_fallback
            .kernel_arm_watch
            .contains_key(&(key, super::SHARED_WAKE_VBLANK_ARM_SLOT,))
    );
    assert!(!backend.platform.vblank_fallback.software.contains_key(&key));
    fixture.adopt_drawables(0x6b00..0x6b03);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: omit standalone gamma commits from the topology event-wait predicate.
fn standalone_gamma_event_retires_before_disable_and_reenable() {
    let Some((fixture, log, route)) = live_two_heads() else {
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    let initial = submit_frame(&mut fixture);
    let stale_receipt = crate::drm::atomic::monotonic_timestamp()
        .expect("CLOCK_MONOTONIC is available in the test environment")
        .checked_sub(Duration::from_secs(3))
        .expect("test CLOCK_MONOTONIC age is available");
    retire_all(&mut fixture, &initial, 90, Some(stale_receipt));
    let output_idx = 1;
    let device_fd = device_fd(&fixture);
    let recorder = fixture.events.recorders[&device_fd].clone();
    let (output_key, connector_name, output_id, crtc_id, crtc, saved, x, y) = {
        let backend = &mut fixture.events.backend;
        let connector_name = connector(backend, output_idx);
        let output_id = output_id(backend, &connector_name);
        let crtc = crtc(backend, output_idx);
        let crtc_id = Backend::randr_outputs_and_modes(backend)
            .0
            .into_iter()
            .find(|output| output.name == connector_name)
            .expect("output has a RandR CRTC id")
            .crtc_id;
        let layout = &mut backend.platform.outputs[output_idx];
        layout.output.crtc_gamma_lut_prop = Some(handle(90));
        layout.output.crtc_degamma_lut_prop = Some(handle(91));
        layout.output.crtc_ctm_prop = Some(handle(92));
        let output_key = layout.key.clone();
        let saved = clone_test_drm_output(&layout.output);
        (
            output_key,
            connector_name,
            output_id,
            crtc_id,
            crtc,
            saved,
            layout.x,
            layout.y,
        )
    };
    recorder.set_gamma_size_for_tests(4);
    Backend::set_crtc_gamma(
        &mut fixture.events.backend,
        crtc_id,
        &[0x1111; 4],
        &[0x2222; 4],
        &[0x3333; 4],
    )
    .expect("submit standalone gamma through Backend");
    let gamma = recorder
        .records()
        .into_iter()
        .find(|record| record.kind == crate::drm::atomic::AtomicCommitKind::GammaOnly)
        .expect("idle CRTC owns a standalone gamma event");
    let gamma_id = accepted_event_id(&gamma).expect("standalone gamma commit is accepted");
    fixture.events.wait_for_submitted_gpu_work();
    fixture.events.queue_commit_completion(
        device_fd,
        handle(crtc),
        gamma_id,
        91,
        Duration::from_millis(910),
    );

    assert!(
        Backend::apply_crtc_config(
            &mut fixture.events.backend,
            output_id,
            &connector_name,
            None,
            0,
            0,
        )
        .expect("disable the output after its standalone gamma commit")
    );
    // If topology setup failed to wait, this drains the old CRTC event after
    // the output mapping has already disappeared, matching the event loss path.
    let events = fixture.events_mut();
    Backend::on_page_flip_ready(&mut events.backend, &mut events.state, device_fd);
    let event_retired_before_disable = {
        let device = &fixture.events.backend.platform.devices[0];
        device
            .atomic_commit_sink
            .for_tests_pending_page_flip_commit_ids(&device.device, handle(crtc))
            .is_empty()
    };

    let discovered = saved;
    let pool = recording_scanout_pool(
        &mut fixture.events.backend,
        &output_key,
        route,
        (800, 600),
        0x6b00,
        0x5300,
    )
    .expect("replacement scanout pool for re-enabled output");
    fixture.events.backend.pstate.crtc_config_discovery_override = Some(discovered);
    fixture.events.backend.pstate.crtc_config_discovery_pool = Some((route, pool));
    let frame_offset = recorder.records().len();
    let enabled = Backend::apply_crtc_config(
        &mut fixture.events.backend,
        output_id,
        &connector_name,
        Some(MODE_800),
        x,
        y,
    )
    .expect("re-enable the same output after gamma event retirement");

    let gamma_crtc = handle::<::drm::control::crtc::Handle>(crtc);
    let mut flips = submit_frame(&mut fixture);
    let pending_output_events = |fixture: &RecordingSceneFixture| {
        let device = &fixture.events.backend.platform.devices[0];
        fixture
            .events
            .backend
            .platform
            .outputs
            .iter()
            .flat_map(|output| {
                device
                    .atomic_commit_sink
                    .for_tests_pending_page_flip_commit_ids(&device.device, output.output.crtc)
                    .into_iter()
                    .map(move |id| (u32::from(output.output.crtc), id))
            })
            .collect::<Vec<_>>()
    };
    let accepted_gamma = || {
        recorder
            .records()
            .into_iter()
            .skip(frame_offset)
            .find(|record| {
                record.properties.iter().any(|update| {
                    update.object == gamma_crtc.into() && update.property == handle(90)
                }) && accepted_event_id(record).is_some()
            })
    };
    if accepted_gamma().is_none() {
        // On the broken path, the event was drained while the CRTC mapping
        // was absent. Retire any remaining identities after restoring the
        // output so the assertion below runs after fixture cleanup can finish.
        for pass in 0..4u32 {
            let pending = pending_output_events(&fixture);
            if pending.is_empty() {
                break;
            }
            retire_all(&mut fixture, &pending, 100 + pass, None);
        }
        flips = submit_frame(&mut fixture);
    }
    let gamma_after_enable = accepted_gamma();
    let gamma_was_applied = if let Some(gamma_after_enable) = gamma_after_enable {
        let gamma_event_id = accepted_event_id(&gamma_after_enable)
            .expect("re-enabled CRTC gamma request owns an event");
        if gamma_after_enable.kind == crate::drm::atomic::AtomicCommitKind::GammaOnly {
            retire(&mut fixture, crtc, gamma_event_id, 92, None);
            let next = submit_frame(&mut fixture);
            let frame_accepted = next.iter().any(|(flipped_crtc, _)| *flipped_crtc == crtc);
            if !next.is_empty() {
                retire_all(&mut fixture, &next, 93, None);
            }
            if !flips.is_empty() {
                retire_all(&mut fixture, &flips, 94, None);
            }
            frame_accepted
        } else {
            let frame_accepted = flips
                .iter()
                .any(|(flipped_crtc, id)| *flipped_crtc == crtc && *id == gamma_event_id);
            if !flips.is_empty() {
                retire_all(&mut fixture, &flips, 92, None);
            }
            frame_accepted
        }
    } else {
        false
    };
    let no_pending_gamma_event = pending_output_events(&fixture).is_empty();
    let topology_changes_applied = log.entries() == vec![("off", crtc), ("on", crtc)]
        && fixture.events.backend.platform.outputs.len() == 2
        && enabled;
    fixture.adopt_drawables(0x6b00..0x6b03);
    fixture.finish_after_normal_drain();
    assert!(
        event_retired_before_disable,
        "gamma event retires before CRTC removal"
    );
    assert!(
        topology_changes_applied,
        "the CRTC disables and re-enables through Backend"
    );
    assert!(
        gamma_was_applied,
        "the cached LUT reaches the re-enabled CRTC"
    );
    assert!(
        no_pending_gamma_event,
        "the re-enabled CRTC has no stale event identity"
    );
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: drop the `OutputScanout::Shared` check from
// per_crtc_fallback_reason (a same-device copied pool is moved per CRTC).
fn same_device_copied_route_takes_the_global_quiesce() {
    let Some(mut fixture) = RecordingSceneFixture::with_route(1, P1SceneRoute::CopiedSameDevice)
        .expect("copied recording scene fixture construction")
    else {
        return;
    };
    let backend = &mut fixture.events.backend;
    let route = use_same_device_route(backend);
    let Some(OutputScanout::Copied(pool)) = backend.platform.scanout_pools[0].as_mut() else {
        panic!("the copied fixture installs a copied pool");
    };
    // The copy's sink is the display device itself.
    pool.route = route;
    let log = install_power_log(backend);
    backend.pstate.crtc_config_discovery_override =
        Some(clone_test_drm_output(&backend.platform.outputs[0].output));
    let name = connector(backend, 0);
    let id = output_id(backend, &name);
    let lit = crtc(backend, 0);

    let _ = Backend::apply_crtc_config(backend, id, &name, Some(MODE_800), 0, 100);

    assert!(
        log.entries().contains(&("off", lit)),
        "a copied pool takes the global quiesce: {:?}",
        log.entries()
    );
    fixture.finish();
}

/// Queue an unsent atomic cursor move on both outputs; returns the one
/// queued for `survivor`.
fn queue_cursor_moves(
    backend: &mut KmsBackend,
    survivor: usize,
) -> crate::drm::atomic::CursorPlanePosition {
    let mut queued = Vec::new();
    for idx in 0..2 {
        let root_x = backend.platform.outputs[idx].x + 100;
        queued.push(backend.platform.for_tests_seed_atomic_cursor_position(
            idx,
            handle(91),
            handle(92),
            handle(93),
            root_x,
            100,
            0,
            0,
        ));
    }
    queued[survivor]
}

fn queued_cursor_move(
    backend: &KmsBackend,
    crtc: u32,
) -> Option<crate::drm::atomic::CursorPlanePosition> {
    backend
        .platform
        .for_tests_atomic_cursor_pending_position(backend.platform.devices[0].key, handle(crtc))
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: let enable_connector_with_pool refresh the whole device's cursor
// topology (the untouched CRTC's queued move is dropped).
fn mode_change_keeps_the_other_crtcs_queued_cursor_move() {
    let Some((mut fixture, log, _route)) = live_two_heads() else {
        return;
    };
    let backend = &mut fixture.events.backend;
    let (crtc0, crtc1) = (crtc(backend, 0), crtc(backend, 1));
    let queued0 = queue_cursor_moves(backend, 0);
    let mut faster = advertised(800, 600);
    faster.vrefresh = 75;
    let mut discovered = clone_test_drm_output(&backend.platform.outputs[1].output);
    discovered.picked = faster.clone();
    discovered.modes.push(faster);
    backend.pstate.crtc_config_discovery_override = Some(discovered);
    let name = connector(backend, 1);
    let id = output_id(backend, &name);
    let mode = ModeSpec {
        width: 800,
        height: 600,
        vrefresh: 75,
    };

    assert!(
        Backend::apply_crtc_config(backend, id, &name, Some(mode), 800, 0)
            .expect("refresh-rate change on the second output")
    );

    assert_eq!(log.entries(), vec![("on", crtc1)]);
    assert_eq!(queued_cursor_move(backend, crtc0), Some(queued0));
    assert_eq!(queued_cursor_move(backend, crtc1), None);
    fixture.finish();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: let disable_connector refresh the whole device's cursor topology
// (the survivor's queued move is dropped as it shifts to index 0).
fn turning_one_output_off_keeps_the_survivors_queued_cursor_move() {
    let Some((mut fixture, log, _route)) = live_two_heads() else {
        return;
    };
    let backend = &mut fixture.events.backend;
    let (crtc0, crtc1) = (crtc(backend, 0), crtc(backend, 1));
    let queued1 = queue_cursor_moves(backend, 1);
    let name = connector(backend, 0);
    let id = output_id(backend, &name);

    assert!(
        Backend::apply_crtc_config(backend, id, &name, None, 0, 0)
            .expect("turn the first output off")
    );

    assert_eq!(log.entries(), vec![("off", crtc0)]);
    assert_eq!(crtc(backend, 0), crtc1);
    assert_eq!(queued_cursor_move(backend, crtc1), Some(queued1));
    assert_eq!(queued_cursor_move(backend, crtc0), None);
    fixture.finish();
}
