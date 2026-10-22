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
    cell::RefCell,
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
fn queue_completion(fixture: &mut RecordingSceneFixture, crtc: u32, id: u64, frame: u32) {
    fixture.events.wait_for_submitted_gpu_work();
    let fd = device_fd(fixture);
    fixture.events.queue_commit_completion(
        fd,
        ::drm::control::from_u32(crtc).expect("nonzero CRTC"),
        id,
        frame,
        Duration::from_millis(u64::from(frame) * 10),
    );
}

/// Deliver a flip completion through the core loop's DRM handler.
fn retire(fixture: &mut RecordingSceneFixture, crtc: u32, id: u64, frame: u32) {
    queue_completion(fixture, crtc, id, frame);
    let fd = device_fd(fixture);
    Backend::on_page_flip_ready(&mut fixture.events.backend, &mut fixture.events.state, fd);
}

fn retire_all(fixture: &mut RecordingSceneFixture, flips: &[(u32, u64)], frame: u32) {
    for &(crtc, id) in flips {
        retire(fixture, crtc, id, frame);
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
// off), or drain every scene output instead of the changed one (output 0's
// in-flight frame is lost).
fn mode_change_modesets_only_its_crtc_while_the_other_keeps_presenting() {
    let Some((mut fixture, log, route)) = live_two_heads() else {
        return;
    };
    let steady = submit_frame(&mut fixture);
    retire_all(&mut fixture, &steady, 1);
    let (crtc0, crtc1) = (
        crtc(&fixture.events.backend, 0),
        crtc(&fixture.events.backend, 1),
    );
    let in_flight = submit_frame(&mut fixture);
    let (flip0, flip1) = (flip_for(&in_flight, crtc0), flip_for(&in_flight, crtc1));
    // CRTC 1's flip completes while the change waits for it; CRTC 0's stays
    // in flight across the whole change.
    queue_completion(&mut fixture, crtc1, flip1, 2);

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

    retire(&mut fixture, crtc0, flip0, 2);
    let backend = &fixture.events.backend;
    assert!(!backend.scene.has_pending_page_flip(0));
    assert_eq!(count(&bo_phases(backend, 0), BoPhase::Pending), 0);

    let next = submit_frame(&mut fixture);
    flip_for(&next, crtc0);
    flip_for(&next, crtc1);
    retire_all(&mut fixture, &next, 3);
    fixture.adopt_drawables(0x6b00..0x6b03);
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
    retire_all(&mut fixture, &steady, 1);
    let (crtc0, crtc1) = (
        crtc(&fixture.events.backend, 0),
        crtc(&fixture.events.backend, 1),
    );
    let in_flight = submit_frame(&mut fixture);
    let (flip0, flip1) = (flip_for(&in_flight, crtc0), flip_for(&in_flight, crtc1));
    queue_completion(&mut fixture, crtc0, flip0, 2);

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

    retire(&mut fixture, crtc1, flip1, 2);
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
    retire_all(&mut fixture, &next, 3);
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
    retire_all(&mut fixture, &steady, 1);
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
    retire_all(&mut fixture, &next, 2);
    fixture.adopt_drawables(0x6b00..0x6b03);
    fixture.finish_after_normal_drain();
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
