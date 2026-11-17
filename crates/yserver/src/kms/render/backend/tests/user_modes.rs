//! RANDR user modes on KMS (`docs/superpowers/plans/2026-11-17-randr-user-modes.md`
//! §3.5): a mode the connector does not list is programmed by its full
//! timing, as Xorg's modesetting driver builds it (`xf86RandRModeConvert`,
//! `drmmode_ConvertToKMode`), and stays programmed across rescans, DPMS and
//! VT switches.

use super::{
    commit_identity::recording_scene::build_recording_scene_backend,
    per_crtc_config::use_same_device_route,
    randr::{
        ScriptedConnectorProbe, TestCrtcConfigProbeExecutor, TestCrtcConfigProbeState,
        async_crtc_config_test_backend, clone_test_drm_output, relight_test_snapshot,
    },
};
use crate::{
    kms::render::{
        backend::{CrtcConfigProbeCompletion, KmsBackend},
        platform::OutputPowerExecutor,
    },
    platform::drm::Output,
};
use std::{cell::RefCell, io, rc::Rc};
use yserver_core::{
    backend::{Backend, CrtcConfigApply, ModeSpec, UserTiming},
    core_loop::input_inventory::InputInventory,
    randr::ModeTiming,
    server::ServerState,
};
use yserver_protocol::x11::randr::ModeInfo;

/// Every enable commit as `(connector, kernel mode)`; refuses `refused`.
#[derive(Default)]
struct ModeLog {
    enables: RefCell<Vec<(String, ::drm::control::Mode)>>,
    refused: RefCell<Option<::drm::control::Mode>>,
}

impl ModeLog {
    fn enables_of(&self, connector: &str) -> Vec<::drm::control::Mode> {
        self.enables
            .borrow()
            .iter()
            .filter(|(name, _)| name == connector)
            .map(|(_, mode)| *mode)
            .collect()
    }
}

impl OutputPowerExecutor for ModeLog {
    fn disable(&self, _device: &crate::drm::Device, _output: &Output) -> io::Result<()> {
        Ok(())
    }

    fn enable(
        &self,
        _device: &crate::drm::Device,
        output: &Output,
        _framebuffer: ::drm::control::framebuffer::Handle,
    ) -> io::Result<()> {
        self.enables
            .borrow_mut()
            .push((output.connector_name.clone(), output.mode));
        if *self.refused.borrow() == Some(output.mode) {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        Ok(())
    }

    fn disable_device(&self, _device: &crate::drm::Device) -> io::Result<()> {
        Ok(())
    }
}

/// VESA DMT 800x600@60 (40.000 MHz, 800 840 968 1056, 600 601 605 628,
/// +hsync +vsync) as a client sends it, with a sub-kHz remainder and a skew
/// so the conversion is visible. Its refresh rounds to 60, the fixture's.
fn vesa_800x600() -> ModeInfo {
    ModeInfo {
        id: 0,
        width: 800,
        height: 600,
        dot_clock: 40_000_999,
        hsync_start: 840,
        hsync_end: 968,
        htotal: 1056,
        hskew: 3,
        vsync_start: 601,
        vsync_end: 605,
        vtotal: 628,
        name_len: 0,
        mode_flags: 0x5,
    }
}

/// What the core hands the backend for [`vesa_800x600`].
fn user_spec() -> ModeSpec {
    let user = UserTiming::from_mode_info(&vesa_800x600());
    ModeSpec {
        width: 800,
        height: 600,
        vrefresh: user.vrefresh(),
        user_timing: Some(user),
    }
}

/// The kernel mode Xorg commits for [`vesa_800x600`]: Clock = dotClock /
/// 1000, Flags & FLAG_BITS, no name, type 0, vscan and vrefresh 0.
fn expected_kernel_mode() -> ::drm::control::Mode {
    ::drm::control::Mode::from(drm_ffi::drm_mode_modeinfo {
        clock: 40_000,
        hdisplay: 800,
        hsync_start: 840,
        hsync_end: 968,
        htotal: 1056,
        hskew: 3,
        vdisplay: 600,
        vsync_start: 601,
        vsync_end: 605,
        vtotal: 628,
        vscan: 0,
        vrefresh: 0,
        flags: 0x5,
        type_: 0,
        name: [0; 32],
    })
}

fn expected_timing() -> ModeTiming {
    ModeTiming {
        clock_khz: 40_000,
        hsync_start: 840,
        hsync_end: 968,
        htotal: 1056,
        hskew: 3,
        vsync_start: 601,
        vsync_end: 605,
        vtotal: 628,
        mode_flags: 0x5,
    }
}

/// Two 800x600@60 heads on one device; the second's connector lists only
/// its synthetic 800x600@60, so the VESA timing is not in its list.
fn two_heads() -> Option<(KmsBackend, Rc<ModeLog>, String, u32)> {
    let mut backend = build_recording_scene_backend(2, false, false)
        .unwrap_or_else(|error| panic!("recording scene fixture unavailable: {error}"))?;
    use_same_device_route(&mut backend);
    let log = Rc::new(ModeLog::default());
    backend
        .platform
        .for_tests_set_output_power_executor(log.clone());
    let name = backend.platform.outputs[1].key.connector_name.clone();
    let id = Backend::randr_outputs_and_modes(&mut backend)
        .0
        .into_iter()
        .find(|output| output.name == name)
        .expect("output in the projection")
        .output_id;
    Some((backend, log, name, id))
}

/// Let the next modeset rediscover the second head's connector as the
/// kernel would: its own list, its preferred mode.
fn rediscover(backend: &mut KmsBackend, fresh: &Output) {
    backend.pstate.crtc_config_discovery_override = Some(clone_test_drm_output(fresh));
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: resolve the user mode by size and refresh (no commit, the same
// nominal mode is current), or compare only (w, h, refresh) in the no-op
// guard.
fn a_user_mode_off_the_list_is_programmed_by_its_timing_at_the_same_origin() {
    let Some((mut backend, log, name, id)) = two_heads() else {
        return;
    };
    let fresh = clone_test_drm_output(&backend.platform.outputs[1].output);
    let before_id = Backend::randr_outputs_and_modes(&mut backend).0[1].mode_id;
    assert_eq!(user_spec().vrefresh, 60, "same nominal mode as the current");

    rediscover(&mut backend, &fresh);
    assert!(
        Backend::apply_crtc_config(&mut backend, id, &name, Some(user_spec()), 800, 0)
            .expect("the user mode is programmed")
    );
    assert_eq!(log.enables_of(&name).last(), Some(&expected_kernel_mode()));
    assert_eq!(
        backend.platform.outputs[1].output.mode,
        expected_kernel_mode()
    );
    assert_eq!(
        (backend.platform.outputs[1].x, backend.platform.outputs[1].y),
        (800, 0)
    );

    // The backend reports the programmed timing under a new backend id.
    let (outputs, modes) = Backend::randr_outputs_and_modes(&mut backend);
    let output = outputs.iter().find(|o| o.output_id == id).unwrap();
    assert_eq!(output.timing, Some(expected_timing()));
    assert_eq!(
        (output.width, output.height, output.vrefresh),
        (800, 600, 60)
    );
    assert_ne!(output.mode_id, before_id);
    assert!(
        modes
            .iter()
            .any(|m| m.mode_id == output.mode_id && m.timing == Some(expected_timing()))
    );

    // Asking for it again is a no-op.
    let commits = log.enables.borrow().len();
    rediscover(&mut backend, &fresh);
    assert!(
        !Backend::apply_crtc_config(&mut backend, id, &name, Some(user_spec()), 800, 0)
            .expect("re-assert")
    );
    assert_eq!(log.enables.borrow().len(), commits);

    // The listed 800x600@60 has the same size and refresh but not the same
    // timing: switching back is a modeset to the kernel's own mode.
    let listed = ModeSpec {
        width: 800,
        height: 600,
        vrefresh: 60,
        user_timing: None,
    };
    rediscover(&mut backend, &fresh);
    assert!(
        Backend::apply_crtc_config(&mut backend, id, &name, Some(listed), 800, 0)
            .expect("back to the listed mode")
    );
    assert_eq!(log.enables_of(&name).last(), Some(&fresh.mode));
    assert_eq!(backend.platform.outputs[1].output.picked, fresh.picked);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: classify a same-(w, h, refresh) user mode at a new origin as a
// position-only change (no commit).
fn a_user_mode_at_a_new_origin_is_a_modeset_not_a_move() {
    let Some((mut backend, log, name, id)) = two_heads() else {
        return;
    };
    let fresh = clone_test_drm_output(&backend.platform.outputs[1].output);
    rediscover(&mut backend, &fresh);
    assert!(
        Backend::apply_crtc_config(&mut backend, id, &name, Some(user_spec()), 900, 0)
            .expect("the user mode is programmed at the new origin")
    );
    assert_eq!(log.enables_of(&name).last(), Some(&expected_kernel_mode()));
    assert_eq!(
        backend.platform.outputs[1].output.mode,
        expected_kernel_mode()
    );
    assert_eq!(
        (backend.platform.outputs[1].x, backend.platform.outputs[1].y),
        (900, 0)
    );
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: install the user mode before the commit succeeds.
fn a_refused_user_mode_leaves_the_output_as_it_was() {
    let Some((mut backend, log, name, id)) = two_heads() else {
        return;
    };
    *log.refused.borrow_mut() = Some(expected_kernel_mode());
    let fresh = clone_test_drm_output(&backend.platform.outputs[1].output);
    let summary = |backend: &mut KmsBackend| {
        Backend::randr_outputs_and_modes(backend)
            .0
            .into_iter()
            .map(|o| (o.output_id, o.mode_id, o.x, o.y, o.timing))
            .collect::<Vec<_>>()
    };
    let before = summary(&mut backend);
    rediscover(&mut backend, &fresh);

    let error = Backend::apply_crtc_config(&mut backend, id, &name, Some(user_spec()), 800, 0)
        .expect_err("the kernel refuses the timing");
    assert_eq!(error.raw_os_error(), Some(libc::EINVAL));

    assert!(log.enables_of(&name).contains(&expected_kernel_mode()));
    assert_eq!(backend.platform.outputs.len(), 2);
    assert_eq!(backend.platform.outputs[1].output.mode, fresh.mode);
    assert_eq!(backend.platform.outputs[1].output.picked, fresh.picked);
    assert_eq!(summary(&mut backend), before);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: re-resolve the live mode by (w, h, refresh) in the rescanned
// connector list, relight with the connector's mode on DPMS on or VT
// resume, or retire a route whose mode the connector does not list.
fn the_programmed_user_mode_survives_rescan_dpms_and_vt_switch() {
    let Some((mut backend, log, name, id)) = two_heads() else {
        return;
    };
    let fresh = clone_test_drm_output(&backend.platform.outputs[1].output);
    let snapshot: Vec<_> = backend
        .platform
        .outputs
        .iter()
        .map(|layout| relight_test_snapshot(&layout.key, layout.output.modes.clone()))
        .collect();
    backend
        .for_tests_set_vt_resume_executor(Rc::new(ScriptedConnectorProbe(RefCell::new(snapshot))));
    let mut state = ServerState::new();
    backend.rebuild_randr_state(&mut state, None, true);
    let crtc_id = state
        .randr
        .outputs
        .iter()
        .find(|o| o.output_id == id)
        .unwrap()
        .crtc_id;
    // The client's user mode, on the output's user list.
    let mut info = vesa_800x600();
    let user_id = state.randr_user_modes.next_free_id().unwrap();
    info.id = user_id;
    state
        .randr_user_modes
        .insert(user_id, b"800x600_vesa".to_vec(), info);
    state.randr_user_modes.add_to_output(id, user_id);

    rediscover(&mut backend, &fresh);
    assert!(
        Backend::apply_crtc_config(&mut backend, id, &name, Some(user_spec()), 800, 0)
            .expect("the user mode is programmed")
    );
    let reports_user_mode = |backend: &mut KmsBackend, state: &mut ServerState, when: &str| {
        Backend::refresh_randr_state_set_time(backend, state, 7);
        assert_eq!(
            backend.platform.outputs[1].output.mode,
            expected_kernel_mode(),
            "{when}"
        );
        assert_eq!(
            state.randr_view().crtc_info(crtc_id, 0).unwrap().mode_id,
            user_id,
            "GetCrtcInfo reports the user mode {when}"
        );
    };
    reports_user_mode(&mut backend, &mut state, "after the set");

    // A rescan whose connector list lacks the timing.
    backend.pstate.hotplug_rescan_deadline = Some(std::time::Instant::now());
    backend.backend_session_poll_deferred_input(&mut state);
    assert_eq!(backend.platform.outputs.len(), 2);
    reports_user_mode(&mut backend, &mut state, "after a rescan");

    log.enables.borrow_mut().clear();
    Backend::set_dpms_power(&mut backend, 3).expect("DPMS off");
    Backend::set_dpms_power(&mut backend, 0).expect("DPMS on");
    assert_eq!(
        log.enables_of(&name),
        vec![expected_kernel_mode()],
        "DPMS on relights the user mode"
    );
    reports_user_mode(&mut backend, &mut state, "after DPMS off/on");

    log.enables.borrow_mut().clear();
    Backend::finish_vt_release(&mut backend, &mut state, &InputInventory::default());
    Backend::on_vt_acquire(&mut backend, &mut state);
    assert_eq!(backend.pstate.vt_state, crate::vt::state::VtState::Active);
    assert_eq!(
        log.enables_of(&name),
        vec![expected_kernel_mode()],
        "VT resume relights the user mode"
    );
    reports_user_mode(&mut backend, &mut state, "after a VT switch");
}

#[test]
// Mutations: drop the user timing from the helper request, or keep checking
// the connector's list for it before enqueueing or when the result comes.
fn a_cross_device_user_mode_reaches_the_qualifier_with_its_timing() {
    let (mut backend, output_id, _listed) = async_crtc_config_test_backend();
    let probes = Rc::new(RefCell::new(TestCrtcConfigProbeState::default()));
    backend.set_crtc_config_probe_executor(Box::new(TestCrtcConfigProbeExecutor::new(
        Rc::clone(&probes),
        None,
    )));
    // VESA DMT 1024x768@60 (65.000 MHz, -hsync -vsync); neither the
    // registry nor the discovered connector lists it.
    let user = UserTiming::from_mode_info(&ModeInfo {
        id: 0,
        width: 1024,
        height: 768,
        dot_clock: 65_000_000,
        hsync_start: 1048,
        hsync_end: 1184,
        htotal: 1344,
        hskew: 0,
        vsync_start: 771,
        vsync_end: 777,
        vtotal: 806,
        name_len: 0,
        mode_flags: 0xa,
    });
    let requested = ModeSpec {
        width: 1024,
        height: 768,
        vrefresh: user.vrefresh(),
        user_timing: Some(user),
    };
    assert_eq!(requested.vrefresh, 60);
    let key = backend.platform.outputs[0].key.clone();
    let entry = backend.pstate.randr_id_alloc.entry_mut(&key);
    entry.modes.retain(|m| (m.width, m.height) != (1024, 768));
    if let Some(discovered) = backend.pstate.crtc_config_discovery_override.as_mut() {
        discovered
            .modes
            .retain(|m| (m.width, m.height) != (1024, 768));
    }

    let apply = Backend::begin_crtc_config(&mut backend, output_id, "test", Some(requested), 0, 0)
        .expect("enqueue the qualification");
    let CrtcConfigApply::Pending(token) = apply else {
        panic!("a cross-device reallocation is asynchronous");
    };
    assert_eq!(probes.borrow().enqueued_requests.len(), 1);
    assert_eq!(probes.borrow().enqueued_requests[0].mode, requested);

    // The helper's verdict decides, not the connector's list.
    probes
        .borrow_mut()
        .completions
        .push_back(CrtcConfigProbeCompletion {
            token,
            result: Err(io::Error::other("helper refused the timing")),
        });
    assert_eq!(Backend::drain_ready_crtc_configs(&mut backend), vec![token]);
    let error = Backend::finish_crtc_config(&mut backend, token).expect_err("refused");
    assert_eq!(error.to_string(), "helper refused the timing");
}
