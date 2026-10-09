use super::*;

fn test_render_device(id: RenderDeviceId, primary: Option<DrmDeviceKey>) -> RenderDevice {
    let selector_seed = match id {
        RenderDeviceId::DrmRender(render) => u8::try_from(render.minor).unwrap_or(1),
        RenderDeviceId::UnverifiedFallback => u8::MAX,
    };
    RenderDevice {
        id,
        physical_device: ash::vk::PhysicalDevice::default(),
        selector: crate::kms::vk::device::VulkanDeviceSelector::for_tests(selector_seed),
        advertised_primary_node: primary,
        advertised_render_node: match id {
            RenderDeviceId::DrmRender(render) => Some(render),
            RenderDeviceId::UnverifiedFallback => None,
        },
        render_node: None,
        render_node_device: None,
        syncobj_timeline: false,
    }
}

#[derive(Default)]
struct TestCrtcConfigProbeState {
    enqueued: Vec<CrtcConfigToken>,
    enqueued_requests: Vec<RouteProbeRequest>,
    cancelled: Vec<CrtcConfigToken>,
    completions: VecDeque<CrtcConfigProbeCompletion>,
}

struct TestCrtcConfigProbeExecutor {
    state: Rc<RefCell<TestCrtcConfigProbeState>>,
    sender: Option<CoreSender>,
    auto_complete: Option<QualifiedScanoutPlan>,
}

impl TestCrtcConfigProbeExecutor {
    fn new(
        state: Rc<RefCell<TestCrtcConfigProbeState>>,
        auto_complete: Option<QualifiedScanoutPlan>,
    ) -> Self {
        Self {
            state,
            sender: None,
            auto_complete,
        }
    }
}

impl CrtcConfigProbeExecutor for TestCrtcConfigProbeExecutor {
    fn set_core_sender(&mut self, sender: CoreSender) {
        self.sender = Some(sender);
    }

    fn enqueue(&mut self, job: CrtcConfigProbeJob) -> io::Result<()> {
        let token = job.request.token;
        let mut state = self.state.borrow_mut();
        state.enqueued.push(token);
        state.enqueued_requests.push(job.request);
        if let Some(plan) = self.auto_complete {
            state.completions.push_back(CrtcConfigProbeCompletion {
                token,
                result: Ok(plan),
            });
            drop(state);
            if let Some(sender) = self.sender.as_ref() {
                sender.send(Message::CrtcConfigReady)?;
            }
        }
        Ok(())
    }

    fn drain_ready(&mut self) -> Vec<CrtcConfigProbeCompletion> {
        self.state.borrow_mut().completions.drain(..).collect()
    }

    fn cancel(&mut self, token: CrtcConfigToken) {
        self.state.borrow_mut().cancelled.push(token);
    }
}

fn clone_test_drm_output(output: &crate::platform::drm::Output) -> crate::platform::drm::Output {
    crate::platform::drm::Output {
        connector: output.connector,
        connector_name: output.connector_name.clone(),
        encoder: output.encoder,
        crtc: output.crtc,
        plane: output.plane,
        mode: output.mode,
        picked: output.picked.clone(),
        plane_fb_id_prop: output.plane_fb_id_prop,
        plane_crtc_id_prop: output.plane_crtc_id_prop,
        plane_src_x_prop: output.plane_src_x_prop,
        plane_src_y_prop: output.plane_src_y_prop,
        plane_src_w_prop: output.plane_src_w_prop,
        plane_src_h_prop: output.plane_src_h_prop,
        plane_crtc_x_prop: output.plane_crtc_x_prop,
        plane_crtc_y_prop: output.plane_crtc_y_prop,
        plane_crtc_w_prop: output.plane_crtc_w_prop,
        plane_crtc_h_prop: output.plane_crtc_h_prop,
        plane_in_fence_fd_prop: output.plane_in_fence_fd_prop,
        crtc_out_fence_ptr_prop: output.crtc_out_fence_ptr_prop,
        scanout_modifiers: output.scanout_modifiers.clone(),
        mm_width: output.mm_width,
        mm_height: output.mm_height,
        edid: output.edid.clone(),
        connector_type: output.connector_type.clone(),
        modes: output.modes.clone(),
    }
}

fn async_crtc_config_test_backend() -> (KmsBackend, u32, ModeSpec) {
    let mut backend = KmsBackend::for_tests();
    let display_key = backend.platform.devices[0].key;
    let render_id = RenderDeviceId::DrmRender(test_device_key(128));
    let sink_id = RenderDeviceId::DrmRender(test_device_key(129));
    backend.platform.render_devices = vec![
        test_render_device(render_id, Some(test_device_key(9))),
        test_render_device(sink_id, Some(display_key)),
    ];
    backend.platform.selected_render_device = Some(render_id);
    let route = ScanoutRoute::new(render_id, display_key, RenderKmsRelationship::Different);
    backend.platform.outputs[0].scanout_route = route;
    backend
        .initialize_provider_output_sources()
        .expect("initialize split provider policy");

    let output_key = backend.platform.outputs[0].key.clone();
    let output_id = backend.randr_id_alloc.ids_for(&output_key).output_id;
    backend
        .output_key_by_id
        .insert(output_id, output_key.clone());
    let requested = ModeSpec {
        width: 1024,
        height: 768,
        vrefresh: 60,
    };
    backend
        .randr_id_alloc
        .entry_mut(&output_key)
        .modes
        .push(test_advertised_mode(
            requested.width,
            requested.height,
            requested.vrefresh,
            false,
        ));
    let mut discovered = clone_test_drm_output(&backend.platform.outputs[0].output);
    discovered.modes.push(test_advertised_mode(
        requested.width,
        requested.height,
        requested.vrefresh,
        false,
    ));
    backend.crtc_config_discovery_override = Some(discovered);
    (backend, output_id, requested)
}

#[test]
fn async_crtc_config_begin_keeps_old_topology_lit() {
    let (mut backend, output_id, requested) = async_crtc_config_test_backend();
    let state = Rc::new(RefCell::new(TestCrtcConfigProbeState::default()));
    backend.set_crtc_config_probe_executor(Box::new(TestCrtcConfigProbeExecutor::new(
        Rc::clone(&state),
        None,
    )));

    let route = backend.platform.outputs[0].scanout_route;
    let display_key = backend.platform.outputs[0].key.device_key;
    let (source_selector, copied_sink) = backend
        .platform
        .scanout_qualification_devices_for_kms(display_key)
        .expect("qualification device identities");
    assert!(
        copied_sink.is_some(),
        "fixture must exercise copied sink wire data"
    );
    let discovered = backend
        .crtc_config_discovery_override
        .as_ref()
        .expect("fixture has prepared output");
    let kms = ProbeKmsHandles {
        connector: discovered.connector,
        encoder: discovered.encoder,
        crtc: discovered.crtc,
        plane: discovered.plane,
    };

    let before = (
        backend.platform.outputs.len(),
        backend.platform.outputs[0].key.clone(),
        backend.platform.outputs[0].x,
        backend.platform.outputs[0].y,
        backend.platform.outputs[0].width,
        backend.platform.outputs[0].height,
        backend.platform.outputs[0].scanout_route,
        backend.kms_outputs_active,
        backend.crtc_config_topology_epoch,
    );
    let apply =
        Backend::begin_crtc_config(&mut backend, output_id, "test", Some(requested), 100, 50)
            .expect("enqueue asynchronous qualification");
    let CrtcConfigApply::Pending(token) = apply else {
        panic!("cross-device reallocation must be asynchronous");
    };

    assert_eq!(state.borrow().enqueued, vec![token]);
    assert_eq!(
        state.borrow().enqueued_requests,
        vec![RouteProbeRequest {
            token,
            mode: requested,
            source_route: route,
            source_selector: source_selector.into(),
            copied_sink: copied_sink.map(Into::into),
            kms,
            fence_timeout_ns: crate::kms::render::platform::PRIME_RENDER_PROBE_TIMEOUT_NS,
        }],
        "begin must pass the exact scalar route, device identities, KMS assignment, mode, and per-fence budget",
    );
    assert_eq!(
        (
            backend.platform.outputs.len(),
            backend.platform.outputs[0].key.clone(),
            backend.platform.outputs[0].x,
            backend.platform.outputs[0].y,
            backend.platform.outputs[0].width,
            backend.platform.outputs[0].height,
            backend.platform.outputs[0].scanout_route,
            backend.kms_outputs_active,
            backend.crtc_config_topology_epoch,
        ),
        before,
        "begin must not quiesce, modeset, or mutate the old live topology",
    );
    Backend::cancel_crtc_config(&mut backend, token);
    assert_eq!(state.borrow().cancelled, vec![token]);
}

#[test]
fn pending_probe_invalidation_wakes_once_and_suppresses_late_success() {
    let (mut backend, output_id, requested) = async_crtc_config_test_backend();
    let state = Rc::new(RefCell::new(TestCrtcConfigProbeState::default()));
    let (_poll, sender, receiver) = yserver_core::core_loop::channel().unwrap();
    Backend::set_input_sender(&mut backend, sender);
    backend.set_crtc_config_probe_executor(Box::new(TestCrtcConfigProbeExecutor::new(
        Rc::clone(&state),
        None,
    )));
    let topology_before = (
        backend.platform.outputs.len(),
        backend.platform.outputs[0].key.clone(),
        backend.platform.outputs[0].x,
        backend.platform.outputs[0].y,
        backend.platform.outputs[0].width,
        backend.platform.outputs[0].height,
        backend.platform.outputs[0].scanout_route,
        backend.kms_outputs_active,
    );

    let CrtcConfigApply::Pending(token) =
        Backend::begin_crtc_config(&mut backend, output_id, "test", Some(requested), 40, 20)
            .expect("enqueue asynchronous qualification")
    else {
        panic!("cross-device reallocation must be asynchronous");
    };
    assert!(receiver.try_recv_all().next().is_none());

    backend.bump_crtc_config_topology_epoch("test topology invalidation");
    assert_eq!(state.borrow().cancelled, vec![token]);
    assert!(backend.pending_crtc_config_probes.contains_key(&token));
    assert_eq!(
        backend
            .ready_crtc_config_results
            .get(&token)
            .expect("synthetic invalidation result")
            .as_ref()
            .unwrap_err()
            .kind(),
        io::ErrorKind::Interrupted,
    );
    assert_eq!(
        backend.ready_crtc_config_announcements,
        VecDeque::from([token])
    );
    assert_eq!(
        receiver
            .try_recv_all()
            .filter(|message| matches!(message, Message::CrtcConfigReady))
            .count(),
        1,
        "invalidation must wake the parked request immediately",
    );

    // Another global epoch transition before the core drains this token
    // must neither re-cancel nor announce a second client completion.
    backend.bump_crtc_config_topology_epoch("second topology invalidation");
    assert_eq!(state.borrow().cancelled, vec![token]);
    assert!(receiver.try_recv_all().next().is_none());

    // Model a running helper returning success after logical cancellation.
    // The synthetic Interrupted result remains authoritative.
    state
        .borrow_mut()
        .completions
        .push_back(CrtcConfigProbeCompletion {
            token,
            result: Ok(QualifiedScanoutPlan::Shared(
                ScanoutAllocationPlan::LegacyLinear,
            )),
        });
    assert_eq!(Backend::drain_ready_crtc_configs(&mut backend), vec![token]);
    assert!(Backend::drain_ready_crtc_configs(&mut backend).is_empty());

    let error = Backend::finish_crtc_config(&mut backend, token)
        .expect_err("invalidated request must finish promptly with Interrupted");
    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    assert!(backend.pending_crtc_config_probes.is_empty());
    assert!(backend.ready_crtc_config_results.is_empty());
    assert!(backend.ready_crtc_config_announcements.is_empty());
    assert_eq!(
        (
            backend.platform.outputs.len(),
            backend.platform.outputs[0].key.clone(),
            backend.platform.outputs[0].x,
            backend.platform.outputs[0].y,
            backend.platform.outputs[0].width,
            backend.platform.outputs[0].height,
            backend.platform.outputs[0].scanout_route,
            backend.kms_outputs_active,
        ),
        topology_before,
        "logical invalidation itself must not darken or mutate the live topology",
    );
}

#[test]
fn vt_transition_promptly_interrupts_and_cancels_pending_probe() {
    let (mut backend, output_id, requested) = async_crtc_config_test_backend();
    let probe_state = Rc::new(RefCell::new(TestCrtcConfigProbeState::default()));
    let (_poll, sender, receiver) = yserver_core::core_loop::channel().unwrap();
    Backend::set_input_sender(&mut backend, sender);
    backend.set_crtc_config_probe_executor(Box::new(TestCrtcConfigProbeExecutor::new(
        Rc::clone(&probe_state),
        None,
    )));
    let CrtcConfigApply::Pending(token) =
        Backend::begin_crtc_config(&mut backend, output_id, "test", Some(requested), 0, 0)
            .expect("enqueue asynchronous qualification")
    else {
        panic!("cross-device reallocation must be asynchronous");
    };

    let mut server_state = ServerState::new();
    backend.inject_seat_event_for_test(&mut server_state, false);
    assert_eq!(probe_state.borrow().cancelled, vec![token]);
    assert_eq!(
        receiver
            .try_recv_all()
            .filter(|message| matches!(message, Message::CrtcConfigReady))
            .count(),
        1,
    );
    assert_eq!(Backend::drain_ready_crtc_configs(&mut backend), vec![token]);
    let error = Backend::finish_crtc_config(&mut backend, token)
        .expect_err("VT transition must interrupt the parked request");
    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    assert!(backend.pending_crtc_config_probes.is_empty());
}

#[test]
fn cancelling_invalidated_probe_removes_undrained_announcement() {
    let (mut backend, output_id, requested) = async_crtc_config_test_backend();
    let probe_state = Rc::new(RefCell::new(TestCrtcConfigProbeState::default()));
    backend.set_crtc_config_probe_executor(Box::new(TestCrtcConfigProbeExecutor::new(
        Rc::clone(&probe_state),
        None,
    )));
    let CrtcConfigApply::Pending(token) =
        Backend::begin_crtc_config(&mut backend, output_id, "test", Some(requested), 0, 0)
            .expect("enqueue asynchronous qualification")
    else {
        panic!("cross-device reallocation must be asynchronous");
    };
    backend.bump_crtc_config_topology_epoch("test cancellation boundary");
    assert_eq!(
        backend.ready_crtc_config_announcements,
        VecDeque::from([token])
    );

    Backend::cancel_crtc_config(&mut backend, token);
    assert!(backend.pending_crtc_config_probes.is_empty());
    assert!(backend.ready_crtc_config_results.is_empty());
    assert!(backend.ready_crtc_config_announcements.is_empty());
    assert!(Backend::drain_ready_crtc_configs(&mut backend).is_empty());
    assert_eq!(probe_state.borrow().cancelled, vec![token, token]);
}

#[test]
fn dpms_transition_promptly_invalidates_pending_probe_after_no_op_guard() {
    let (mut backend, output_id, requested) = async_crtc_config_test_backend();
    let probe_state = Rc::new(RefCell::new(TestCrtcConfigProbeState::default()));
    let (_poll, sender, receiver) = yserver_core::core_loop::channel().unwrap();
    Backend::set_input_sender(&mut backend, sender);
    backend.set_crtc_config_probe_executor(Box::new(TestCrtcConfigProbeExecutor::new(
        Rc::clone(&probe_state),
        None,
    )));
    let CrtcConfigApply::Pending(token) =
        Backend::begin_crtc_config(&mut backend, output_id, "test", Some(requested), 0, 0)
            .expect("enqueue asynchronous qualification")
    else {
        panic!("cross-device reallocation must be asynchronous");
    };
    let epoch_before = backend.crtc_config_topology_epoch;

    Backend::set_dpms_power(&mut backend, 0).expect("already-on DPMS request is a no-op");
    assert_eq!(backend.crtc_config_topology_epoch, epoch_before);
    assert!(probe_state.borrow().cancelled.is_empty());
    assert!(receiver.try_recv_all().next().is_none());

    // The synthetic DRM fixture may reject the subsequent all-off commit,
    // but invalidation must happen before that fallible transition.
    let _ = Backend::set_dpms_power(&mut backend, 3);
    assert_eq!(
        backend.crtc_config_topology_epoch,
        epoch_before.wrapping_add(1)
    );
    assert_eq!(probe_state.borrow().cancelled, vec![token]);
    assert_eq!(
        receiver
            .try_recv_all()
            .filter(|message| matches!(message, Message::CrtcConfigReady))
            .count(),
        1,
    );
    assert_eq!(Backend::drain_ready_crtc_configs(&mut backend), vec![token]);
    assert_eq!(
        Backend::finish_crtc_config(&mut backend, token)
            .expect_err("DPMS transition must interrupt the parked request")
            .kind(),
        io::ErrorKind::Interrupted,
    );
}

#[test]
fn async_crtc_config_completion_wakes_core_and_drains_token() {
    let (mut backend, output_id, requested) = async_crtc_config_test_backend();
    let state = Rc::new(RefCell::new(TestCrtcConfigProbeState::default()));
    let (_poll, sender, receiver) = yserver_core::core_loop::channel().unwrap();
    Backend::set_input_sender(&mut backend, sender);
    backend.set_crtc_config_probe_executor(Box::new(TestCrtcConfigProbeExecutor::new(
        Rc::clone(&state),
        Some(QualifiedScanoutPlan::Shared(
            ScanoutAllocationPlan::LegacyLinear,
        )),
    )));

    let CrtcConfigApply::Pending(token) =
        Backend::begin_crtc_config(&mut backend, output_id, "test", Some(requested), 0, 0)
            .expect("enqueue asynchronous qualification")
    else {
        panic!("cross-device reallocation must be asynchronous");
    };

    assert!(
        receiver
            .try_recv_all()
            .any(|message| matches!(message, Message::CrtcConfigReady))
    );
    assert_eq!(Backend::drain_ready_crtc_configs(&mut backend), vec![token]);
    assert!(Backend::drain_ready_crtc_configs(&mut backend).is_empty());

    Backend::cancel_crtc_config(&mut backend, token);
    assert_eq!(state.borrow().cancelled, vec![token]);
}

#[test]
fn topology_invalidation_overwrites_announced_result_without_duplicate_wake() {
    let (mut backend, output_id, requested) = async_crtc_config_test_backend();
    let state = Rc::new(RefCell::new(TestCrtcConfigProbeState::default()));
    let (_poll, sender, receiver) = yserver_core::core_loop::channel().unwrap();
    Backend::set_input_sender(&mut backend, sender);
    backend.set_crtc_config_probe_executor(Box::new(TestCrtcConfigProbeExecutor::new(
        Rc::clone(&state),
        Some(QualifiedScanoutPlan::Shared(
            ScanoutAllocationPlan::LegacyLinear,
        )),
    )));

    let CrtcConfigApply::Pending(token) =
        Backend::begin_crtc_config(&mut backend, output_id, "test", Some(requested), 0, 0)
            .expect("enqueue asynchronous qualification")
    else {
        panic!("cross-device reallocation must be asynchronous");
    };
    assert!(
        receiver
            .try_recv_all()
            .any(|message| matches!(message, Message::CrtcConfigReady))
    );
    assert_eq!(Backend::drain_ready_crtc_configs(&mut backend), vec![token]);

    backend.bump_crtc_config_topology_epoch("test after ready announcement");
    assert_eq!(state.borrow().cancelled, vec![token]);
    assert!(receiver.try_recv_all().next().is_none());
    assert!(backend.ready_crtc_config_announcements.is_empty());
    assert_eq!(
        backend
            .ready_crtc_config_results
            .get(&token)
            .expect("announced result remains consumable")
            .as_ref()
            .unwrap_err()
            .kind(),
        io::ErrorKind::Interrupted,
    );

    assert_eq!(
        Backend::finish_crtc_config(&mut backend, token)
            .expect_err("topology invalidation must replace the stale qualified plan")
            .kind(),
        io::ErrorKind::Interrupted,
    );
}

#[test]
fn async_crtc_config_finish_rejects_vt_stale_result_without_quiesce() {
    let (mut backend, output_id, requested) = async_crtc_config_test_backend();
    let state = Rc::new(RefCell::new(TestCrtcConfigProbeState::default()));
    backend.set_crtc_config_probe_executor(Box::new(TestCrtcConfigProbeExecutor::new(
        Rc::clone(&state),
        Some(QualifiedScanoutPlan::Shared(
            ScanoutAllocationPlan::LegacyLinear,
        )),
    )));
    let before = (
        backend.platform.outputs.len(),
        backend.platform.outputs[0].key.clone(),
        backend.platform.outputs[0].width,
        backend.platform.outputs[0].height,
        backend.kms_outputs_active,
        backend.crtc_config_topology_epoch,
    );

    let CrtcConfigApply::Pending(token) =
        Backend::begin_crtc_config(&mut backend, output_id, "test", Some(requested), 0, 0)
            .expect("enqueue asynchronous qualification")
    else {
        panic!("cross-device reallocation must be asynchronous");
    };
    assert_eq!(Backend::drain_ready_crtc_configs(&mut backend), vec![token]);

    // Model a release arriving after helper completion but before the
    // parked request is finalized. Direct assignment keeps this focused
    // on finish's pre-replay guard: a real drive_vt_event also bumps the
    // topology epoch and would be rejected for both reasons.
    backend.vt_state = crate::vt::state::VtState::Suspended;
    let error = Backend::finish_crtc_config(&mut backend, token)
        .expect_err("VT-stale qualification must not be installed");
    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    assert!(error.to_string().contains("VT/DRM-master"));
    assert_eq!(
        (
            backend.platform.outputs.len(),
            backend.platform.outputs[0].key.clone(),
            backend.platform.outputs[0].width,
            backend.platform.outputs[0].height,
            backend.kms_outputs_active,
            backend.crtc_config_topology_epoch,
        ),
        before,
        "stale finish must not quiesce or mutate the old topology",
    );
    assert!(backend.pending_crtc_config_probes.is_empty());
    assert!(backend.ready_crtc_config_results.is_empty());
    assert_eq!(state.borrow().cancelled, vec![token]);
}

#[test]
fn randr_rebuild_preserves_primary_while_output_resource_exists() {
    let mut state = ServerState::new();
    let output = state.randr.outputs[0].output_id;

    // An untouched topology-derived primary is not sticky. Leave the
    // freshly rebuilt fallback in place when no client selected it.
    state.randr.primary_output = 0;
    assert!(!restore_primary_output_after_rebuild(
        output,
        false,
        &mut state.randr,
    ));
    assert_eq!(state.randr.primary_output, 0);

    // Disconnection does not retire the RANDR output resource, so a
    // client-selected primary remains meaningful and must survive.
    state.randr.outputs[0].connected = false;
    state.randr.primary_output = 0;
    assert!(restore_primary_output_after_rebuild(
        output,
        true,
        &mut state.randr,
    ));
    assert_eq!(state.randr.primary_output, output);

    // An explicit None primary is likewise client-owned state.
    state.randr.primary_output = output;
    assert!(restore_primary_output_after_rebuild(
        0,
        true,
        &mut state.randr,
    ));
    assert_eq!(state.randr.primary_output, 0);

    // Once the old resource is absent, keep the freshly rebuilt state's
    // fallback instead of restoring a dangling xid.
    state.randr.primary_output = output;
    assert!(!restore_primary_output_after_rebuild(
        0x00de_ad01,
        true,
        &mut state.randr,
    ));
    assert_eq!(state.randr.primary_output, output);
}

#[test]
fn mode_timing_passes_kernel_timing_and_masks_drm_only_flags() {
    // A real 2560x1440@59.95 mode: timing must pass through verbatim,
    // and DRM-only flag bits above the RANDR RR_* range must be masked
    // so we never advertise a bit RANDR would misinterpret.
    let m = crate::platform::drm::Mode {
        name: "2560x1440".into(),
        width: 2560,
        height: 1440,
        vrefresh: 60,
        preferred: true,
        clock_khz: 241_500,
        hsync_start: 2608,
        hsync_end: 2640,
        htotal: 2720,
        vsync_start: 1443,
        vsync_end: 1448,
        vtotal: 1481,
        vscan: 1,
        // PHSYNC(0x1) | PVSYNC(0x4) kept; DBLCLK(0x1000, DRM-only) dropped.
        flags: 0x1 | 0x4 | 0x1000,
    };
    let t = mode_timing(&m).expect("real timing => Some");
    assert_eq!(t.clock_khz, 241_500);
    assert_eq!(t.htotal, 2720);
    assert_eq!(t.vtotal, 1481);
    assert_eq!(t.hsync_start, 2608);
    assert_eq!(t.vsync_end, 1448);
    assert_eq!(t.mode_flags, 0x5, "DRM-only bits masked to RR_* range");

    // Synthetic/nested mode (no clock) => None => RANDR synthesises.
    let synthetic = crate::platform::drm::Mode {
        width: 800,
        height: 600,
        vrefresh: 60,
        ..Default::default()
    };
    assert!(mode_timing(&synthetic).is_none(), "clock_khz==0 => None");
}

#[test]
fn kms_gamma_off_connector_seeds_identity_ramp_at_256() {
    let mut b = KmsBackend::for_tests();
    let crtc = 0x4000;
    b.crtc_key_by_id.insert(crtc, test_output_key(0, "DP-1"));

    assert_eq!(b.crtc_gamma_size(crtc), 256);
    let (red, green, blue) = b.get_crtc_gamma(crtc);
    assert_eq!(red.len(), 256);
    assert_eq!(red[0], 0);
    assert_eq!(red[255], 65535);
    assert_eq!((green[255], blue[255]), (65535, 65535));
}

#[test]
fn kms_gamma_off_connector_set_roundtrips_cached_values() {
    let mut b = KmsBackend::for_tests();
    let crtc = 0x4000;
    b.crtc_key_by_id.insert(crtc, test_output_key(0, "DP-1"));
    let red = vec![1u16; 256];
    let green = vec![2u16; 256];
    let blue = vec![3u16; 256];
    b.set_crtc_gamma(crtc, &red, &green, &blue)
        .expect("set gamma cache");
    assert_eq!(b.get_crtc_gamma(crtc), (red, green, blue));
}

#[test]
fn kms_gamma_same_connector_name_on_two_devices_stays_independent() {
    let mut b = KmsBackend::for_tests();
    let crtc_a = 0x4000;
    let crtc_b = 0x4001;
    b.crtc_key_by_id.insert(crtc_a, test_output_key(0, "DP-1"));
    b.crtc_key_by_id.insert(crtc_b, test_output_key(1, "DP-1"));

    let a = (vec![1u16; 256], vec![2u16; 256], vec![3u16; 256]);
    let b_lut = (vec![4u16; 256], vec![5u16; 256], vec![6u16; 256]);
    b.set_crtc_gamma(crtc_a, &a.0, &a.1, &a.2)
        .expect("set first card gamma cache");
    b.set_crtc_gamma(crtc_b, &b_lut.0, &b_lut.1, &b_lut.2)
        .expect("set second card gamma cache");

    assert_eq!(b.get_crtc_gamma(crtc_a), a);
    assert_eq!(b.get_crtc_gamma(crtc_b), b_lut);
}

#[test]
fn randr_ids_are_stable_across_drop() {
    let mut alloc = RandrIdAllocator::default();
    let dp1 = test_output_key(0, "DP-1");
    let hdmi1 = test_output_key(0, "HDMI-A-1");
    let dp2 = test_output_key(0, "DP-2");
    let a = alloc.ids_for(&dp1);
    let b = alloc.ids_for(&hdmi1);
    assert_ne!(a.output_id, b.output_id);
    let a2 = alloc.ids_for(&dp1);
    assert_eq!(a, a2, "a surviving/returning connector keeps its IDs");
    let c = alloc.ids_for(&dp2);
    assert_ne!(c.output_id, a.output_id);
    assert_ne!(c.output_id, b.output_id);
    assert_ne!(c.crtc_id, a.crtc_id);
}

#[test]
fn randr_ids_distinguish_equal_connector_names_on_different_devices() {
    let mut alloc = RandrIdAllocator::default();
    let first = test_output_key(0, "HDMI-A-1");
    let second = test_output_key(1, "HDMI-A-1");

    let first_ids = alloc.ids_for(&first);
    let second_ids = alloc.ids_for(&second);

    assert_ne!(first_ids.output_id, second_ids.output_id);
    assert_ne!(first_ids.crtc_id, second_ids.crtc_id);
    assert_eq!(alloc.ids_for(&first), first_ids);
    assert_eq!(alloc.ids_for(&second), second_ids);
}

#[test]
fn randr_provider_ids_are_stable_and_share_the_xid_namespace() {
    let mut alloc = RandrIdAllocator::default();
    let first_device = test_device_key(0);
    let second_device = test_device_key(1);
    let first_endpoint = RandrProviderEndpoint::Kms(first_device);
    let second_endpoint = RandrProviderEndpoint::Kms(second_device);
    let tagged_render_endpoint =
        RandrProviderEndpoint::Render(RenderDeviceId::DrmRender(first_device));
    let first_provider = alloc.provider_id_for(first_endpoint);
    let output = alloc.ids_for(&test_output_key(0, "DP-1"));
    let mode = alloc.mode_id(&test_advertised_mode(1920, 1080, 60, false));
    let second_provider = alloc.provider_id_for(second_endpoint);
    let tagged_render_provider = alloc.provider_id_for(tagged_render_endpoint);

    assert_eq!(alloc.provider_id_for(first_endpoint), first_provider);
    assert_eq!(
        alloc.provider_endpoint_for_id(first_provider),
        Some(first_endpoint)
    );
    assert_eq!(
        alloc.provider_endpoint_for_id(tagged_render_provider),
        Some(tagged_render_endpoint),
        "tagged render and KMS endpoints must reverse-resolve distinctly"
    );
    assert_eq!(alloc.provider_endpoint_for_id(u32::MAX), None);
    assert_eq!(
        alloc.provider_id_for(tagged_render_endpoint),
        tagged_render_provider
    );
    let ids = [
        first_provider,
        output.output_id,
        output.crtc_id,
        mode,
        second_provider,
        tagged_render_provider,
    ];
    let unique: std::collections::HashSet<u32> = ids.into_iter().collect();
    assert_eq!(unique.len(), ids.len());
}

#[test]
fn randr_provider_projection_is_per_kms_device_and_includes_disconnected_connectors() {
    let mut backend = KmsBackend::for_tests();
    let first_device = backend.platform.devices[0].key;
    let second_device = test_device_key(1);
    push_test_device(&mut backend, second_device);

    let first_live = backend.platform.outputs[0].key.clone();
    let first_disconnected = OutputKey::new(first_device, "DP-9");
    let second_disconnected = OutputKey::new(second_device, "HDMI-A-1");
    let first_live_ids = backend.randr_id_alloc.ids_for(&first_live);
    let first_disconnected_ids = backend.randr_id_alloc.ids_for(&first_disconnected);
    let second_disconnected_ids = backend.randr_id_alloc.ids_for(&second_disconnected);

    let providers = backend.randr_providers();
    assert_eq!(providers.len(), 2);
    let first_id = backend
        .randr_id_alloc
        .provider_id_for(RandrProviderEndpoint::Kms(first_device));
    let second_id = backend
        .randr_id_alloc
        .provider_id_for(RandrProviderEndpoint::Kms(second_device));
    let first = providers
        .iter()
        .find(|provider| provider.provider_id == first_id)
        .expect("first KMS provider");
    let second = providers
        .iter()
        .find(|provider| provider.provider_id == second_id)
        .expect("second KMS provider");

    let mut expected_first_outputs =
        vec![first_live_ids.output_id, first_disconnected_ids.output_id];
    expected_first_outputs.sort_unstable();
    let mut expected_first_crtcs = vec![first_live_ids.crtc_id, first_disconnected_ids.crtc_id];
    expected_first_crtcs.sort_unstable();
    assert_eq!(first.outputs, expected_first_outputs);
    assert_eq!(first.crtcs, expected_first_crtcs);
    assert_eq!(second.outputs, vec![second_disconnected_ids.output_id]);
    assert_eq!(second.crtcs, vec![second_disconnected_ids.crtc_id]);
    assert_eq!(first.capabilities, 0);
    assert_eq!(second.capabilities, 0);
    assert!(first.associations.is_empty());
    assert!(second.associations.is_empty());
    assert_eq!(first.name, "null");
    assert_eq!(second.name, "null");
}

#[test]
fn provider_output_source_projection_is_endpoint_qualified_and_symmetric() {
    use yserver_protocol::x11::randr::{
        PROVIDER_CAPABILITY_SINK_OUTPUT, PROVIDER_CAPABILITY_SOURCE_OUTPUT,
    };

    let mut backend = KmsBackend::for_tests();
    let source_key = backend.platform.devices[0].key;
    let sink_key = test_device_key(1);
    push_test_device(&mut backend, sink_key);
    let sink_output = OutputKey::new(sink_key, "DP-9");
    let _ = backend.randr_id_alloc.ids_for(&sink_output);
    let render_id = RenderDeviceId::DrmRender(test_device_key(128));
    backend.platform.render_devices = vec![test_render_device(render_id, Some(source_key))];
    backend.platform.selected_render_device = Some(render_id);
    let source_endpoint = RandrProviderEndpoint::Kms(source_key);
    backend
        .provider_output_sources
        .insert(sink_key, source_endpoint);

    let providers = backend.randr_providers();
    let source_id = backend.randr_id_alloc.providers[&source_endpoint];
    let sink_endpoint = RandrProviderEndpoint::Kms(sink_key);
    let sink_id = backend.randr_id_alloc.providers[&sink_endpoint];
    let source = providers
        .iter()
        .find(|provider| provider.provider_id == source_id)
        .expect("coalesced source provider");
    let sink = providers
        .iter()
        .find(|provider| provider.provider_id == sink_id)
        .expect("KMS sink provider");

    assert_eq!(source.capabilities, PROVIDER_CAPABILITY_SOURCE_OUTPUT);
    assert_eq!(sink.capabilities, PROVIDER_CAPABILITY_SINK_OUTPUT);
    assert!(!source.is_gpu);
    assert!(sink.is_gpu);
    assert_eq!(
        source.associations,
        vec![yserver_core::randr::RandrProviderAssociation {
            provider_id: sink_id,
            capability: PROVIDER_CAPABILITY_SINK_OUTPUT,
        }]
    );
    assert_eq!(
        sink.associations,
        vec![yserver_core::randr::RandrProviderAssociation {
            provider_id: source_id,
            capability: PROVIDER_CAPABILITY_SOURCE_OUTPUT,
        }]
    );
    assert!(backend.provider_output_source_allows(source_key));
    assert!(backend.provider_output_source_allows(sink_key));
}

#[test]
fn automatic_inventoryless_sink_waits_for_connector_projection() {
    use yserver_protocol::x11::randr::PROVIDER_CAPABILITY_SOURCE_OUTPUT;

    let mut backend = KmsBackend::for_tests();
    let source_key = backend.platform.devices[0].key;
    let inventoryless_key = test_device_key(9);
    push_test_device(&mut backend, inventoryless_key);
    let render_id = RenderDeviceId::DrmRender(test_device_key(128));
    backend.platform.render_devices = vec![test_render_device(render_id, Some(source_key))];
    backend.platform.selected_render_device = Some(render_id);
    backend.platform.outputs[0].scanout_route =
        ScanoutRoute::new(render_id, source_key, RenderKmsRelationship::Same);
    backend
        .initialize_provider_output_sources()
        .expect("initialize automatic provider policy");

    let providers = backend.randr_providers();
    let source_id = backend.randr_id_alloc.providers[&RandrProviderEndpoint::Kms(source_key)];
    let inventoryless_id =
        backend.randr_id_alloc.providers[&RandrProviderEndpoint::Kms(inventoryless_key)];
    assert_eq!(
        providers
            .iter()
            .find(|provider| provider.provider_id == source_id)
            .expect("source")
            .capabilities,
        PROVIDER_CAPABILITY_SOURCE_OUTPUT,
    );
    assert_eq!(
        providers
            .iter()
            .find(|provider| provider.provider_id == inventoryless_id)
            .expect("inventory-less KMS provider")
            .capabilities,
        0,
    );
    assert!(
        providers
            .iter()
            .find(|provider| provider.provider_id == inventoryless_id)
            .expect("inventory-less KMS provider")
            .is_gpu,
        "a distinct KMS endpoint remains logically a GPU provider even before connectors exist"
    );
    assert!(backend.provider_output_source_allows(inventoryless_key));
    assert_eq!(
        backend.provider_output_sources.get(&inventoryless_key),
        Some(&RandrProviderEndpoint::Kms(source_key)),
        "automatic policy is retained before the sink can be projected"
    );
    assert!(
        providers
            .iter()
            .find(|provider| provider.provider_id == inventoryless_id)
            .expect("inventory-less KMS provider")
            .associations
            .is_empty(),
        "capability-zero providers do not project a premature association"
    );

    let _ = backend
        .randr_id_alloc
        .ids_for(&OutputKey::new(inventoryless_key, "DP-9"));
    let providers = backend.randr_providers();
    let sink = providers
        .iter()
        .find(|provider| provider.provider_id == inventoryless_id)
        .expect("newly eligible KMS sink");
    assert_eq!(
        sink.capabilities,
        yserver_protocol::x11::randr::PROVIDER_CAPABILITY_SINK_OUTPUT
    );
    assert_eq!(sink.associations.len(), 1);
}

#[test]
fn startup_automatically_binds_asahi_first_and_only_kms_sink() {
    use yserver_protocol::x11::randr::{
        PROVIDER_CAPABILITY_SINK_OUTPUT, PROVIDER_CAPABILITY_SOURCE_OUTPUT,
    };

    let mut backend = KmsBackend::for_tests();
    let display_key = backend.platform.devices[0].key;
    let render_id = RenderDeviceId::DrmRender(test_device_key(128));
    backend.platform.render_devices = vec![test_render_device(render_id, Some(test_device_key(7)))];
    backend.platform.selected_render_device = Some(render_id);
    backend.platform.outputs[0].scanout_route =
        ScanoutRoute::new(render_id, display_key, RenderKmsRelationship::Different);

    backend
        .initialize_provider_output_sources()
        .expect("initialize Asahi-shaped automatic policy");
    let source_endpoint = RandrProviderEndpoint::Render(render_id);
    assert_eq!(
        backend.provider_output_sources.get(&display_key),
        Some(&source_endpoint)
    );
    assert!(backend.provider_output_source_allows(display_key));

    let providers = backend.randr_providers();
    assert_eq!(providers.len(), 2);
    let source = providers
        .iter()
        .find(|provider| provider.provider_id == backend.randr_id_alloc.providers[&source_endpoint])
        .expect("distinct render source");
    let display = providers
        .iter()
        .find(|provider| {
            provider.provider_id
                == backend.randr_id_alloc.providers[&RandrProviderEndpoint::Kms(display_key)]
        })
        .expect("display sink");
    assert_eq!(source.capabilities, PROVIDER_CAPABILITY_SOURCE_OUTPUT);
    assert_eq!(display.capabilities, PROVIDER_CAPABILITY_SINK_OUTPUT);
    assert!(!source.is_gpu);
    assert!(display.is_gpu);
    assert_eq!(source.associations.len(), 1);
    assert_eq!(display.associations.len(), 1);
}

#[test]
fn coalesced_renderer_automatically_binds_inactive_secondary_sink() {
    let mut backend = KmsBackend::for_tests();
    let kms_key = backend.platform.devices[0].key;
    let inactive_sink_key = test_device_key(1);
    push_test_device(&mut backend, inactive_sink_key);
    let _ = backend
        .randr_id_alloc
        .ids_for(&OutputKey::new(inactive_sink_key, "DP-9"));
    let render_id = RenderDeviceId::DrmRender(test_device_key(128));
    backend.platform.render_devices = vec![test_render_device(render_id, Some(kms_key))];
    backend.platform.selected_render_device = Some(render_id);
    backend.platform.outputs[0].scanout_route =
        ScanoutRoute::new(render_id, kms_key, RenderKmsRelationship::Same);

    backend
        .initialize_provider_output_sources()
        .expect("initialize coalesced automatic policy");
    assert_eq!(backend.provider_output_sources.len(), 1);
    assert_eq!(
        backend.provider_output_sources.get(&inactive_sink_key),
        Some(&RandrProviderEndpoint::Kms(kms_key))
    );
    assert!(!backend.provider_output_sources.contains_key(&kms_key));
    assert!(backend.provider_output_source_allows(kms_key));
    assert!(backend.provider_output_source_allows(inactive_sink_key));

    let providers = backend.randr_providers();
    let source_id = backend.randr_id_alloc.providers[&RandrProviderEndpoint::Kms(kms_key)];
    let sink_id = backend.randr_id_alloc.providers[&RandrProviderEndpoint::Kms(inactive_sink_key)];
    let source = providers
        .iter()
        .find(|provider| provider.provider_id == source_id)
        .expect("coalesced source");
    let sink = providers
        .iter()
        .find(|provider| provider.provider_id == sink_id)
        .expect("inactive sink");
    assert_eq!(source.associations.len(), 1);
    assert_eq!(sink.associations.len(), 1);
}

#[test]
fn automatic_provider_policy_validates_live_route_endpoints_before_binding() {
    let mut backend = KmsBackend::for_tests();
    let display_key = backend.platform.devices[0].key;
    let secondary_key = test_device_key(1);
    push_test_device(&mut backend, secondary_key);
    let selected_id = RenderDeviceId::DrmRender(test_device_key(128));
    let stale_id = RenderDeviceId::DrmRender(test_device_key(129));
    backend.platform.render_devices = vec![test_render_device(selected_id, Some(display_key))];
    backend.platform.selected_render_device = Some(selected_id);
    backend.platform.outputs[0].scanout_route =
        ScanoutRoute::new(stale_id, display_key, RenderKmsRelationship::Unknown);

    let error = backend
        .initialize_provider_output_sources()
        .expect_err("stale renderer endpoint must reject startup policy");
    assert!(error.to_string().contains("records renderer"));
    assert!(backend.provider_output_sources.is_empty());

    backend.platform.outputs[0].scanout_route =
        ScanoutRoute::new(selected_id, secondary_key, RenderKmsRelationship::Different);
    let error = backend
        .initialize_provider_output_sources()
        .expect_err("wrong KMS endpoint must reject startup policy");
    assert!(error.to_string().contains("records KMS endpoint"));
    assert!(backend.provider_output_sources.is_empty());
}

#[test]
fn stale_allocated_provider_id_is_not_a_current_endpoint() {
    let mut backend = KmsBackend::for_tests();
    let stale = RandrProviderEndpoint::Render(RenderDeviceId::DrmRender(test_device_key(199)));
    let stale_id = backend.randr_id_alloc.provider_id_for(stale);
    assert_eq!(
        backend.randr_id_alloc.provider_endpoint_for_id(stale_id),
        Some(stale)
    );
    assert_eq!(backend.current_provider_endpoint_for_id(stale_id), None);
}

#[test]
fn randr_rebuild_restores_the_stable_provider_projection() {
    let mut backend = KmsBackend::for_tests();
    let second_device = test_device_key(1);
    push_test_device(&mut backend, second_device);
    let disconnected = OutputKey::new(second_device, "DP-2");
    let disconnected_ids = backend.randr_id_alloc.ids_for(&disconnected);
    let render_id = RenderDeviceId::DrmRender(test_device_key(128));
    backend.platform.render_devices = vec![test_render_device(render_id, None)];
    backend.platform.selected_render_device = Some(render_id);
    let capabilities = yserver_core::server::BackendCapabilities::from_backend(&backend);
    let (outputs, modes) = backend.randr_outputs_and_modes();
    let (width, height) = backend.fb_dimensions();
    let mut state =
        ServerState::with_randr_outputs_and_modes(width, height, outputs, modes, capabilities);

    backend.rebuild_randr_state(&mut state, None, false);
    let first = state.randr.providers.clone();
    assert_eq!(first.len(), 3);
    assert!(
        first
            .iter()
            .any(|provider| provider.outputs.contains(&disconnected_ids.output_id))
    );

    let first_provider_ids: Vec<_> = first.iter().map(|provider| provider.provider_id).collect();
    let added_key = OutputKey::new(second_device, "HDMI-A-9");
    assert!(
        !backend
            .reconcile_connector_registry(
                &[ConnectorSnapshot {
                    key: added_key.clone(),
                    modes: Vec::new(),
                    mm_width: 0,
                    mm_height: 0,
                    edid: Vec::new(),
                    connector_type: "unknown".to_string(),
                }],
                &[],
                &[],
            )
            .is_empty()
    );
    let added_ids = backend
        .randr_id_alloc
        .entry(&added_key)
        .expect("reconciled connector entry")
        .ids;

    state.randr.providers.clear();
    backend.rebuild_randr_state(&mut state, None, false);
    assert_eq!(
        state
            .randr
            .providers
            .iter()
            .map(|provider| provider.provider_id)
            .collect::<Vec<_>>(),
        first_provider_ids,
        "connector allocation and rebuild must not renumber providers"
    );
    assert!(
        state
            .randr
            .providers
            .iter()
            .any(|provider| provider.outputs.contains(&added_ids.output_id))
    );
}

#[test]
fn provider_output_source_attach_detach_persists_without_timestamp_changes() {
    let mut backend = KmsBackend::for_tests();
    let source_key = backend.platform.devices[0].key;
    let sink_key = test_device_key(1);
    push_test_device(&mut backend, sink_key);
    let _ = backend
        .randr_id_alloc
        .ids_for(&OutputKey::new(sink_key, "DP-9"));
    let render_id = RenderDeviceId::DrmRender(test_device_key(128));
    backend.platform.render_devices = vec![test_render_device(render_id, Some(source_key))];
    backend.platform.selected_render_device = Some(render_id);
    let providers = backend.randr_providers();
    let source_id = backend.randr_id_alloc.providers[&RandrProviderEndpoint::Kms(source_key)];
    let sink_id = backend.randr_id_alloc.providers[&RandrProviderEndpoint::Kms(sink_key)];
    let (outputs, modes) = backend.randr_outputs_and_modes();
    let mut state = ServerState::with_randr_outputs_and_modes(
        backend.platform.fb_w,
        backend.platform.fb_h,
        outputs,
        modes,
        yserver_core::server::BackendCapabilities::from_backend(&backend),
    );
    state.randr.set_providers(providers);
    state.randr.timestamp = 41;
    state.randr.config_timestamp = 37;

    assert!(
        backend
            .set_provider_output_source(&mut state, sink_id, Some(source_id))
            .expect("attach selected source")
    );
    assert_eq!(state.randr.timestamp, 41);
    assert_eq!(state.randr.config_timestamp, 37);
    assert!(backend.provider_output_source_allows(sink_key));
    let sink = state.randr.provider(sink_id).expect("projected sink");
    assert_eq!(sink.associations.len(), 1);
    assert_eq!(sink.associations[0].provider_id, source_id);

    backend.rebuild_randr_state(&mut state, None, false);
    assert_eq!(
        state
            .randr
            .provider(sink_id)
            .expect("sink survives rebuild")
            .associations
            .len(),
        1,
        "endpoint policy must survive an unrelated RANDR rebuild"
    );
    assert_eq!(state.randr.timestamp, 41);
    assert_eq!(state.randr.config_timestamp, 37);

    assert!(
        backend
            .set_provider_output_source(&mut state, sink_id, None)
            .expect("detach inactive sink")
    );
    assert!(!backend.provider_output_source_allows(sink_key));
    assert_eq!(state.randr.timestamp, 41);
    assert_eq!(state.randr.config_timestamp, 37);
    assert!(
        !backend
            .set_provider_output_source(&mut state, sink_id, None)
            .expect("repeat detach is idempotent")
    );
}

#[test]
fn explicit_detach_survives_rebuild_disconnect_and_reconnect() {
    let mut backend = KmsBackend::for_tests();
    let source_key = backend.platform.devices[0].key;
    let sink_key = test_device_key(1);
    push_test_device(&mut backend, sink_key);
    let sink_output = OutputKey::new(sink_key, "DP-9");
    let _ = backend.randr_id_alloc.ids_for(&sink_output);
    assert!(
        !backend
            .reconcile_connector_registry(
                &[ConnectorSnapshot {
                    key: sink_output.clone(),
                    modes: Vec::new(),
                    mm_width: 0,
                    mm_height: 0,
                    edid: Vec::new(),
                    connector_type: "DisplayPort".to_string(),
                }],
                &[],
                &[],
            )
            .is_empty()
    );
    let render_id = RenderDeviceId::DrmRender(test_device_key(128));
    backend.platform.render_devices = vec![test_render_device(render_id, Some(source_key))];
    backend.platform.selected_render_device = Some(render_id);
    backend.platform.outputs[0].scanout_route =
        ScanoutRoute::new(render_id, source_key, RenderKmsRelationship::Same);
    backend
        .initialize_provider_output_sources()
        .expect("initialize automatic provider policy");
    let providers = backend.randr_providers();
    let source_id = backend.randr_id_alloc.providers[&RandrProviderEndpoint::Kms(source_key)];
    let sink_id = backend.randr_id_alloc.providers[&RandrProviderEndpoint::Kms(sink_key)];
    let (outputs, modes) = backend.randr_outputs_and_modes();
    let mut state = ServerState::with_randr_outputs_and_modes(
        backend.platform.fb_w,
        backend.platform.fb_h,
        outputs,
        modes,
        yserver_core::server::BackendCapabilities::from_backend(&backend),
    );
    state.randr.set_providers(providers);

    assert!(backend.provider_output_source_allows(sink_key));
    assert_eq!(
        state
            .randr
            .provider(sink_id)
            .expect("attached sink")
            .associations[0]
            .provider_id,
        source_id
    );

    assert!(
        backend
            .set_provider_output_source(&mut state, sink_id, None)
            .expect("explicitly detach automatic source")
    );
    assert!(!backend.provider_output_source_allows(sink_key));
    assert!(
        state
            .randr
            .provider(sink_id)
            .expect("detached sink")
            .associations
            .is_empty()
    );

    backend.rebuild_randr_state(&mut state, None, false);
    assert!(
        state
            .randr
            .provider(sink_id)
            .expect("detached sink survives rebuild")
            .associations
            .is_empty(),
        "ordinary RANDR rebuild must not recreate automatic policy"
    );

    assert!(
        !backend
            .reconcile_connector_registry(&[], std::slice::from_ref(&sink_output), &[])
            .is_empty()
    );
    backend.rebuild_randr_state(&mut state, None, true);
    assert!(
        state
            .randr
            .provider(sink_id)
            .expect("disconnected sink remains a provider")
            .associations
            .is_empty(),
        "disconnect must not recreate explicitly removed policy"
    );

    assert!(
        !backend
            .reconcile_connector_registry(
                &[ConnectorSnapshot {
                    key: sink_output,
                    modes: Vec::new(),
                    mm_width: 0,
                    mm_height: 0,
                    edid: Vec::new(),
                    connector_type: "DisplayPort".to_string(),
                }],
                &[],
                &[],
            )
            .is_empty()
    );
    backend.rebuild_randr_state(&mut state, None, true);
    assert!(
        state
            .randr
            .provider(sink_id)
            .expect("reconnected sink")
            .associations
            .is_empty(),
        "reconnect must preserve explicit detach"
    );
    assert!(!backend.provider_output_sources.contains_key(&sink_key));
}

#[test]
fn active_split_sink_cannot_be_detached_while_dpms_dark() {
    let mut backend = KmsBackend::for_tests();
    let display_key = backend.platform.devices[0].key;
    let render_id = RenderDeviceId::UnverifiedFallback;
    backend.platform.render_devices = vec![test_render_device(render_id, None)];
    backend.platform.selected_render_device = Some(render_id);
    backend.platform.outputs[0].scanout_route =
        ScanoutRoute::new(render_id, display_key, RenderKmsRelationship::Unknown);
    backend
        .initialize_provider_output_sources()
        .expect("initialize active unknown split route");
    let providers = backend.randr_providers();
    let sink_id = backend.randr_id_alloc.providers[&RandrProviderEndpoint::Kms(display_key)];
    let mut state = ServerState::new();
    state.randr.set_providers(providers);
    backend.kms_outputs_active = false;

    let error = backend
        .set_provider_output_source(&mut state, sink_id, None)
        .expect_err("DPMS-dark ActiveOutput still owns the route");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("active sink provider"));
    assert!(
        error
            .to_string()
            .contains(&backend.platform.outputs[0].key.connector_name)
    );
    assert!(backend.provider_output_source_allows(display_key));
}

#[test]
fn split_enable_gate_runs_before_idempotent_active_reassert() {
    let mut backend = KmsBackend::for_tests();
    let display_key = backend.platform.devices[0].key;
    let render_id = RenderDeviceId::DrmRender(test_device_key(128));
    backend.platform.render_devices = vec![test_render_device(render_id, None)];
    backend.platform.selected_render_device = Some(render_id);
    backend.platform.outputs[0].scanout_route =
        ScanoutRoute::new(render_id, display_key, RenderKmsRelationship::Unknown);
    let layout = &backend.platform.outputs[0];
    let connector = layout.key.connector_name.clone();
    let mode = yserver_core::backend::ModeSpec {
        width: layout.width,
        height: layout.height,
        vrefresh: layout.output.picked.vrefresh,
    };
    let (x, y) = (layout.x, layout.y);
    let output_id = backend
        .randr_outputs_and_modes()
        .0
        .into_iter()
        .find(|output| output.name == connector)
        .expect("active output projection")
        .output_id;

    let error = backend
        .apply_crtc_config(output_id, &connector, Some(mode), x, y)
        .expect_err("missing split policy must not hide behind idempotency");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("SetProviderOutputSource"));
}

#[test]
fn automatically_bound_live_split_route_passes_gate_before_idempotent_reassert() {
    let mut backend = KmsBackend::for_tests();
    let display_key = backend.platform.devices[0].key;
    let render_id = RenderDeviceId::DrmRender(test_device_key(128));
    backend.platform.render_devices = vec![test_render_device(render_id, None)];
    backend.platform.selected_render_device = Some(render_id);
    backend.platform.outputs[0].scanout_route =
        ScanoutRoute::new(render_id, display_key, RenderKmsRelationship::Unknown);
    backend
        .initialize_provider_output_sources()
        .expect("initialize the already-live split route");

    let layout = &backend.platform.outputs[0];
    let connector = layout.key.connector_name.clone();
    let mode = yserver_core::backend::ModeSpec {
        width: layout.width,
        height: layout.height,
        vrefresh: layout.output.picked.vrefresh,
    };
    let (x, y) = (layout.x, layout.y);
    let output_id = backend
        .randr_outputs_and_modes()
        .0
        .into_iter()
        .find(|output| output.name == connector)
        .expect("active output projection")
        .output_id;

    assert!(backend.provider_output_source_allows(display_key));
    assert!(
        !backend
            .apply_crtc_config(output_id, &connector, Some(mode), x, y)
            .expect("automatic policy lets the active reassert reach idempotency"),
        "an unchanged active configuration remains a no-op after policy authorization"
    );
}

#[test]
fn disabling_an_off_split_sink_does_not_require_attachment() {
    let mut backend = KmsBackend::for_tests();
    let source_key = backend.platform.devices[0].key;
    let sink_key = test_device_key(1);
    push_test_device(&mut backend, sink_key);
    let render_id = RenderDeviceId::DrmRender(test_device_key(128));
    backend.platform.render_devices = vec![test_render_device(render_id, Some(source_key))];
    backend.platform.selected_render_device = Some(render_id);
    let output_key = OutputKey::new(sink_key, "DP-9");
    let ids = backend.randr_id_alloc.ids_for(&output_key);
    backend.output_key_by_id.insert(ids.output_id, output_key);

    assert!(!backend.provider_output_source_allows(sink_key));
    assert!(
        !backend
            .apply_crtc_config(ids.output_id, "DP-9", None, 0, 0)
            .expect("an already-off output can always remain disabled")
    );
}

#[test]
fn conventional_renderer_coalesces_with_its_matching_kms_provider() {
    let mut backend = KmsBackend::for_tests();
    let kms_key = backend.platform.devices[0].key;
    let selected_id = RenderDeviceId::DrmRender(test_device_key(128));
    let unselected_id = RenderDeviceId::DrmRender(test_device_key(129));
    backend.platform.render_devices = vec![
        test_render_device(selected_id, Some(kms_key)),
        test_render_device(unselected_id, None),
    ];
    backend.platform.selected_render_device = Some(selected_id);

    let (outputs, _) = backend.randr_outputs_and_modes();
    let providers = backend.randr_providers();
    assert_eq!(providers.len(), 1);
    assert!(!providers[0].is_gpu);
    assert_eq!(
        providers[0].capabilities,
        yserver_protocol::x11::randr::PROVIDER_CAPABILITY_SOURCE_OUTPUT
    );
    assert_eq!(providers[0].outputs, vec![outputs[0].output_id]);
    assert_eq!(providers[0].crtcs, vec![outputs[0].crtc_id]);
    assert_eq!(
        providers[0].provider_id,
        backend
            .randr_id_alloc
            .provider_id_for(RandrProviderEndpoint::Kms(kms_key))
    );
    assert!(
        !backend
            .randr_id_alloc
            .providers
            .contains_key(&RandrProviderEndpoint::Render(selected_id)),
        "same-device renderer must not allocate a second provider"
    );
    assert!(
        !backend
            .randr_id_alloc
            .providers
            .contains_key(&RandrProviderEndpoint::Render(unselected_id)),
        "metadata-only unselected render inventory is not a provider"
    );
}

#[test]
fn unverified_renderer_coalesces_when_its_primary_matches_kms() {
    let mut backend = KmsBackend::for_tests();
    let kms_key = backend.platform.devices[0].key;
    let render_id = RenderDeviceId::UnverifiedFallback;
    backend.platform.render_devices = vec![test_render_device(render_id, Some(kms_key))];
    backend.platform.selected_render_device = Some(render_id);

    let providers = backend.randr_providers();
    assert_eq!(providers.len(), 1);
    assert!(!providers[0].is_gpu);
    assert_eq!(
        providers[0].capabilities,
        yserver_protocol::x11::randr::PROVIDER_CAPABILITY_SOURCE_OUTPUT
    );
    assert_eq!(
        providers[0].provider_id,
        backend
            .randr_id_alloc
            .provider_id_for(RandrProviderEndpoint::Kms(kms_key))
    );
    assert!(
        !backend
            .randr_id_alloc
            .providers
            .contains_key(&RandrProviderEndpoint::Render(render_id)),
        "coalescing keys on advertised primary identity, not RenderDeviceId"
    );
}

#[test]
fn headless_selected_renderer_is_the_only_provider() {
    let mut backend = KmsBackend::for_tests();
    backend.platform.devices.clear();
    backend.platform.outputs.clear();
    backend.randr_id_alloc = RandrIdAllocator::default();
    let render_id = RenderDeviceId::DrmRender(test_device_key(128));
    backend.platform.render_devices = vec![test_render_device(render_id, None)];
    backend.platform.selected_render_device = Some(render_id);

    let providers = backend.randr_providers();
    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0].name, "drm-render-226:128");
    assert!(!providers[0].is_gpu);
    assert_eq!(
        providers[0].capabilities,
        yserver_protocol::x11::randr::PROVIDER_CAPABILITY_SOURCE_OUTPUT
    );
    assert!(providers[0].outputs.is_empty());
    assert!(providers[0].crtcs.is_empty());
    assert_eq!(
        providers[0].provider_id,
        backend
            .randr_id_alloc
            .provider_id_for(RandrProviderEndpoint::Render(render_id))
    );
}

#[test]
fn split_asahi_shape_exposes_distinct_display_and_render_providers() {
    let mut backend = KmsBackend::for_tests();
    let display_key = test_device_key(2);
    let renderer_primary_key = test_device_key(0);
    let render_id = RenderDeviceId::DrmRender(test_device_key(128));
    backend.platform.devices[0].key = display_key;
    backend.platform.outputs[0].key.device_key = display_key;
    backend.randr_id_alloc = RandrIdAllocator::default();
    backend.platform.render_devices =
        vec![test_render_device(render_id, Some(renderer_primary_key))];
    backend.platform.selected_render_device = Some(render_id);

    let (outputs, _) = backend.randr_outputs_and_modes();
    let providers = backend.randr_providers();
    assert_eq!(backend.platform.devices.len(), 1);
    assert_eq!(backend.platform.render_devices.len(), 1);
    assert_eq!(providers.len(), 2);
    let display_id = backend
        .randr_id_alloc
        .provider_id_for(RandrProviderEndpoint::Kms(display_key));
    let renderer_id = backend
        .randr_id_alloc
        .provider_id_for(RandrProviderEndpoint::Render(render_id));
    let display = providers
        .iter()
        .find(|provider| provider.provider_id == display_id)
        .expect("apple-drm display provider");
    let renderer = providers
        .iter()
        .find(|provider| provider.provider_id == renderer_id)
        .expect("AGX render provider");
    assert_eq!(display.outputs, vec![outputs[0].output_id]);
    assert_eq!(display.crtcs, vec![outputs[0].crtc_id]);
    assert!(display.is_gpu);
    assert_eq!(
        display.capabilities,
        yserver_protocol::x11::randr::PROVIDER_CAPABILITY_SINK_OUTPUT
    );
    assert!(renderer.outputs.is_empty());
    assert!(renderer.crtcs.is_empty());
    assert!(!renderer.is_gpu);
    assert_eq!(
        renderer.capabilities,
        yserver_protocol::x11::randr::PROVIDER_CAPABILITY_SOURCE_OUTPUT
    );
    assert_eq!(renderer.name, "drm-render-226:128");
}

#[test]
fn provider_order_is_deterministic_and_reserves_selected_renderer_first() {
    let mut backend = KmsBackend::for_tests();
    let display_key = test_device_key(2);
    let render_id = RenderDeviceId::DrmRender(test_device_key(128));
    backend.platform.devices[0].key = display_key;
    backend.platform.outputs[0].key.device_key = display_key;
    backend.randr_id_alloc = RandrIdAllocator::default();
    backend.platform.render_devices = vec![test_render_device(render_id, Some(test_device_key(0)))];
    backend.platform.selected_render_device = Some(render_id);

    let (outputs, _) = backend.randr_outputs_and_modes();
    let providers = backend.randr_providers();
    let providers_again = backend.randr_providers();
    let renderer_xid = backend
        .randr_id_alloc
        .provider_id_for(RandrProviderEndpoint::Render(render_id));
    let display_xid = backend
        .randr_id_alloc
        .provider_id_for(RandrProviderEndpoint::Kms(display_key));

    assert!(renderer_xid < display_xid);
    assert!(display_xid < outputs[0].output_id);
    assert!(display_xid < outputs[0].crtc_id);
    assert_eq!(
        providers
            .iter()
            .map(|provider| provider.provider_id)
            .collect::<Vec<_>>(),
        vec![renderer_xid, display_xid]
    );
    assert_eq!(providers_again, providers);
}

#[test]
fn unverified_selected_renderer_has_a_tagged_stable_provider() {
    let mut backend = KmsBackend::for_tests();
    backend.platform.devices.clear();
    backend.platform.outputs.clear();
    backend.randr_id_alloc = RandrIdAllocator::default();
    let render_id = RenderDeviceId::UnverifiedFallback;
    backend.platform.render_devices = vec![test_render_device(render_id, None)];
    backend.platform.selected_render_device = Some(render_id);

    let first = backend.randr_providers();
    let second = backend.randr_providers();
    assert_eq!(first, second);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].name, "vulkan-unverified");
    assert!(!first[0].is_gpu);
    assert_eq!(
        first[0].capabilities,
        yserver_protocol::x11::randr::PROVIDER_CAPABILITY_SOURCE_OUTPUT
    );
    assert!(first[0].outputs.is_empty());
    assert!(first[0].crtcs.is_empty());
    assert_eq!(
        first[0].provider_id,
        backend
            .randr_id_alloc
            .provider_id_for(RandrProviderEndpoint::Render(render_id))
    );
}

#[test]
fn no_kms_and_no_selected_renderer_has_no_providers() {
    let mut backend = KmsBackend::for_tests();
    backend.platform.devices.clear();
    backend.platform.outputs.clear();
    backend.platform.render_devices.clear();
    backend.platform.selected_render_device = None;
    backend.randr_id_alloc = RandrIdAllocator::default();

    assert!(backend.randr_providers().is_empty());
}

#[test]
fn randr_projection_retains_device_identity_for_equal_connector_names() {
    let mut b = KmsBackend::for_tests();
    let first = test_output_key(1, "HDMI-A-1");
    let second = test_output_key(2, "HDMI-A-1");
    let first_ids = b.randr_id_alloc.ids_for(&first);
    let second_ids = b.randr_id_alloc.ids_for(&second);

    let (outputs, _) = b.randr_outputs_and_modes();
    let matching: Vec<_> = outputs
        .iter()
        .filter(|output| output.name == "HDMI-A-1")
        .collect();

    assert_eq!(matching.len(), 2);
    assert_ne!(first_ids.output_id, second_ids.output_id);
    assert_eq!(b.output_key_by_id.get(&first_ids.output_id), Some(&first));
    assert_eq!(b.output_key_by_id.get(&second_ids.output_id), Some(&second));
}

#[test]
fn active_test_output_carries_its_owning_device_key() {
    let b = KmsBackend::for_tests();
    assert_eq!(b.platform.devices.len(), 1);
    assert_eq!(b.platform.outputs.len(), 1);
    assert_eq!(
        b.platform.outputs[0].key.device_key,
        b.platform
            .primary_device()
            .expect("test fixture has a DRM device")
            .key
    );
}

#[test]
fn connector_only_probe_reconciles_state_and_retains_disconnected_modes() {
    use crate::platform::drm::{ConnectorProbe, Mode};

    fn mode(width: u16, height: u16, preferred: bool) -> Mode {
        Mode {
            name: format!("{width}x{height}"),
            width,
            height,
            vrefresh: 60,
            preferred,
            ..Mode::default()
        }
    }

    let mut b = KmsBackend::for_tests();
    let device_key = b
        .platform
        .primary_device()
        .expect("test fixture has a DRM device")
        .key;
    let dp1_key = OutputKey::new(device_key, "DP-1");
    let dp2_key = OutputKey::new(device_key, "DP-2");
    let dp3_key = OutputKey::new(device_key, "DP-3");
    let hdmi_key = OutputKey::new(device_key, "HDMI-A-1");
    let other_device_key = test_output_key(1, "DP-9");
    {
        let dp = b.randr_id_alloc.entry_mut(&dp1_key);
        dp.connected = true;
        dp.modes = vec![mode(2560, 1440, true)];
        dp.edid = vec![0x01, 0x02];
        dp.mm_width = 600;
        dp.mm_height = 340;
        dp.connector_type = "DisplayPort".into();
    }
    // A connector absent from the kernel resource list must also be
    // marked disconnected.
    b.randr_id_alloc.entry_mut(&dp2_key).connected = true;
    // A connector on another device is outside this probe's scope.
    b.randr_id_alloc.entry_mut(&other_device_key).connected = true;

    let probes = vec![
        ConnectorProbe {
            connector_name: "DP-1".into(),
            connected: false,
            modes: Vec::new(),
        },
        ConnectorProbe {
            connector_name: "HDMI-A-1".into(),
            connected: true,
            modes: vec![mode(1920, 1080, true)],
        },
        ConnectorProbe {
            connector_name: "DP-3".into(),
            connected: false,
            modes: Vec::new(),
        },
    ];
    let delta = reconcile_connector_probe(&mut b.randr_id_alloc, device_key, &probes);
    assert!(!delta.is_empty());
    assert!(delta.config_changed);

    let dp1 = b.randr_id_alloc.connectors.get(&dp1_key).unwrap();
    assert!(!dp1.connected);
    assert_eq!(
        dp1.modes,
        vec![mode(2560, 1440, true)],
        "disconnect retains the last-known mode list"
    );
    assert!(dp1.edid.is_empty(), "disconnect invalidates monitor EDID");
    assert_eq!((dp1.mm_width, dp1.mm_height), (0, 0));
    assert_eq!(dp1.connector_type, "DisplayPort");
    assert!(!b.randr_id_alloc.connectors.get(&dp2_key).unwrap().connected);
    assert!(
        !b.randr_id_alloc.connectors.get(&dp3_key).unwrap().connected,
        "a newly-known disconnected connector still receives a stable output XID",
    );
    assert!(
        b.randr_id_alloc
            .connectors
            .get(&other_device_key)
            .unwrap()
            .connected,
        "probing one DRM device must not disconnect another device's outputs"
    );
    let hdmi = b.randr_id_alloc.connectors.get(&hdmi_key).unwrap();
    assert!(hdmi.connected);
    assert_eq!(hdmi.modes, vec![mode(1920, 1080, true)]);

    assert!(
        reconcile_connector_probe(&mut b.randr_id_alloc, device_key, &probes).is_empty(),
        "an identical forced probe must not bump RANDR config time"
    );
}

// ── P1: hotplug relight, remembered routes and reserved slots ────────
//
// Design:
// `docs/superpowers/specs/2026-09-17-randr-crtc-model-and-hotplug-relight-design.md`,
// "P1 — restore the reconnect relight".

fn relight_test_snapshot(
    key: &OutputKey,
    modes: Vec<crate::platform::drm::Mode>,
) -> ConnectorSnapshot {
    ConnectorSnapshot {
        key: key.clone(),
        modes,
        mm_width: 0,
        mm_height: 0,
        edid: Vec::new(),
        connector_type: "unknown".to_string(),
    }
}

fn rects_overlap(
    a: crate::kms::render::platform::LayoutRect,
    b: crate::kms::render::platform::LayoutRect,
) -> bool {
    let (ax, ay, aw, ah) = a;
    let (bx, by, bw, bh) = b;
    ax < bx + i32::from(bw)
        && bx < ax + i32::from(aw)
        && ay < by + i32::from(bh)
        && by < ay + i32::from(ah)
}

#[test]
fn a_physical_disconnect_remembers_the_route_and_the_reconnect_relights_it() {
    let mut backend = KmsBackend::for_tests();
    clear_test_outputs(&mut backend);
    let key = push_enabled_test_output(&mut backend, "HDMI-3", 7, 0, 0, 1920, 1080);
    let snapshot = relight_test_snapshot(&key, vec![test_advertised_mode(1920, 1080, 60, true)]);

    // ── Physical departure ───────────────────────────────────────────
    let rescan = backend
        .platform
        .apply_connector_snapshot(Vec::new(), &std::collections::HashSet::from([key.clone()]));
    let _ = backend.reconcile_connector_registry(
        &rescan.connected,
        &rescan.dropped_keys,
        &rescan.dropped_layouts,
    );

    let entry = backend.randr_id_alloc.entry(&key).unwrap();
    assert!(!entry.connected);
    assert_eq!(
        entry.config,
        crate::kms::render::backend::ConnectorConfig::Off
    );
    assert_eq!(
        entry.last_enabled,
        Some(crate::kms::render::backend::ConnectorConfig::Enabled {
            mode_w: 1920,
            mode_h: 1080,
            vrefresh: 60,
            x: 0,
            y: 0,
        }),
        "the departed route is remembered from the layout that actually departed",
    );
    assert_eq!(backend.reserved_layout_slots(), vec![(0, 0, 1920, 1080)]);
    assert!(
        backend.take_relight_requests().is_empty(),
        "a connector that has not come back is not relit",
    );
    assert!(
        backend
            .randr_id_alloc
            .entry(&key)
            .unwrap()
            .last_enabled
            .is_some(),
        "and it keeps its reservation while it is away",
    );

    // ── Reconnect ────────────────────────────────────────────────────
    let _ = backend.reconcile_connector_registry(std::slice::from_ref(&snapshot), &[], &[]);
    let requests = backend.take_relight_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].key, key);
    assert_eq!(
        requests[0].mode,
        yserver_core::backend::ModeSpec {
            width: 1920,
            height: 1080,
            vrefresh: 60,
        },
    );
    assert_eq!((requests[0].x, requests[0].y), (0, 0));

    backend.commit_relit_route(&requests[0]);
    let entry = backend.randr_id_alloc.entry(&key).unwrap();
    assert_eq!(
        entry.config,
        crate::kms::render::backend::ConnectorConfig::Enabled {
            mode_w: 1920,
            mode_h: 1080,
            vrefresh: 60,
            x: 0,
            y: 0,
        },
    );
    assert!(
        !entry.client_configured,
        "an auto-relight restores a previous state; it is not a client intent",
    );
    assert!(entry.last_enabled.is_none(), "the reservation is released");
    assert!(backend.reserved_layout_slots().is_empty());
}

#[test]
fn a_route_that_was_already_off_when_it_departed_is_never_relit() {
    // A client's explicit SetCrtcConfig(mode=None) leaves the entry Off,
    // so the later unplug has no route to remember. Unplugging a
    // deliberately disabled monitor must not resurrect it (invariant 6).
    let mut backend = KmsBackend::for_tests();
    clear_test_outputs(&mut backend);
    let key = push_enabled_test_output(&mut backend, "HDMI-3", 7, 0, 0, 1920, 1080);
    backend.randr_id_alloc.entry_mut(&key).config =
        crate::kms::render::backend::ConnectorConfig::Off;
    backend.randr_id_alloc.entry_mut(&key).client_configured = true;
    let snapshot = relight_test_snapshot(&key, vec![test_advertised_mode(1920, 1080, 60, true)]);

    let rescan = backend
        .platform
        .apply_connector_snapshot(Vec::new(), &std::collections::HashSet::from([key.clone()]));
    assert_eq!(
        rescan.dropped_layouts.len(),
        1,
        "the live route was still there to drop",
    );
    let _ = backend.reconcile_connector_registry(
        &rescan.connected,
        &rescan.dropped_keys,
        &rescan.dropped_layouts,
    );

    assert!(
        backend
            .randr_id_alloc
            .entry(&key)
            .unwrap()
            .last_enabled
            .is_none(),
    );
    assert!(backend.reserved_layout_slots().is_empty());

    let _ = backend.reconcile_connector_registry(std::slice::from_ref(&snapshot), &[], &[]);
    assert!(backend.take_relight_requests().is_empty());
}

#[test]
fn a_client_config_on_a_departed_output_releases_its_reservation() {
    // Design, "Layout policy — reserved slots" point 4: a client
    // SetCrtcConfig on a departed output releases the reservation, and
    // releasing it is exactly clearing `last_enabled`.
    let mut backend = KmsBackend::for_tests();
    clear_test_outputs(&mut backend);
    let key = push_enabled_test_output(&mut backend, "HDMI-3", 7, 0, 0, 1920, 1080);
    let output_id = backend.randr_id_alloc.ids_for(&key).output_id;
    let snapshot = relight_test_snapshot(&key, vec![test_advertised_mode(1920, 1080, 60, true)]);

    let rescan = backend
        .platform
        .apply_connector_snapshot(Vec::new(), &std::collections::HashSet::from([key.clone()]));
    let _ = backend.reconcile_connector_registry(
        &rescan.connected,
        &rescan.dropped_keys,
        &rescan.dropped_layouts,
    );
    assert_eq!(backend.reserved_layout_slots(), vec![(0, 0, 1920, 1080)]);

    // Publish, so the request path can resolve the output XID.
    let (outputs, modes) = backend.randr_outputs_and_modes();
    let mut state = ServerState::with_randr_outputs_and_modes(
        backend.platform.fb_w,
        backend.platform.fb_h,
        outputs,
        modes,
        yserver_core::server::BackendCapabilities::from_backend(&backend),
    );
    backend.rebuild_randr_state(&mut state, None, false);

    // The route is already gone, so this disable is the no-op path: it
    // touches no hardware but is still an explicit client statement.
    assert!(
        !backend
            .apply_crtc_config(output_id, "HDMI-3", None, 0, 0)
            .expect("disabling an already-off output is a no-op"),
    );

    assert!(
        backend
            .randr_id_alloc
            .entry(&key)
            .unwrap()
            .last_enabled
            .is_none(),
    );
    assert!(backend.reserved_layout_slots().is_empty());
    let _ = backend.reconcile_connector_registry(std::slice::from_ref(&snapshot), &[], &[]);
    assert!(backend.take_relight_requests().is_empty());
}

#[test]
fn an_incompatible_mode_reconnect_leaves_the_output_off_and_does_not_re_reserve() {
    let mut backend = KmsBackend::for_tests();
    clear_test_outputs(&mut backend);
    let key = push_enabled_test_output(&mut backend, "HDMI-3", 7, 0, 0, 1920, 1080);
    // The monitor is replaced by a panel that cannot do 1920x1080@60.
    let replacement = relight_test_snapshot(&key, vec![test_advertised_mode(1024, 768, 60, true)]);

    let rescan = backend
        .platform
        .apply_connector_snapshot(Vec::new(), &std::collections::HashSet::from([key.clone()]));
    let _ = backend.reconcile_connector_registry(
        &rescan.connected,
        &rescan.dropped_keys,
        &rescan.dropped_layouts,
    );
    assert_eq!(backend.reserved_layout_slots(), vec![(0, 0, 1920, 1080)]);

    // First rescan after the replacement panel appears.
    let _ = backend.reconcile_connector_registry(std::slice::from_ref(&replacement), &[], &[]);
    assert!(backend.take_relight_requests().is_empty());
    let entry = backend.randr_id_alloc.entry(&key).unwrap();
    assert_eq!(
        entry.config,
        crate::kms::render::backend::ConnectorConfig::Off
    );
    assert!(
        entry.last_enabled.is_none(),
        "the remembered route is cleared, which is what releases the slot",
    );
    assert!(backend.reserved_layout_slots().is_empty());

    // A SECOND rescan: a stale reservation would re-reserve the slot here,
    // and the single-rescan assertion above would not have caught it.
    let _ = backend.reconcile_connector_registry(std::slice::from_ref(&replacement), &[], &[]);
    assert!(backend.take_relight_requests().is_empty());
    assert!(backend.reserved_layout_slots().is_empty());
    assert_eq!(
        backend.randr_id_alloc.entry(&key).unwrap().config,
        crate::kms::render::backend::ConnectorConfig::Off,
    );
}

#[test]
fn a_reserved_slot_keeps_the_survivor_in_place_and_the_relight_cannot_overlap_it() {
    // codex's counterexample: A at x=0, B at x=1920. Without the
    // reservation, unplugging A compacts B to x=0 and shrinks the extent,
    // so restoring A at its remembered x=0 overlaps B (invariant 7).
    let mut backend = KmsBackend::for_tests();
    clear_test_outputs(&mut backend);
    let a = push_enabled_test_output(&mut backend, "A", 7, 0, 0, 1920, 1080);
    let b = push_enabled_test_output(&mut backend, "B", 8, 1920, 0, 3200, 1440);
    backend.platform.fb_w = 5120;
    backend.platform.fb_h = 1440;
    let snapshot_a = relight_test_snapshot(&a, vec![test_advertised_mode(1920, 1080, 60, true)]);
    let snapshot_b = relight_test_snapshot(&b, vec![test_advertised_mode(3200, 1440, 60, true)]);

    // ── Unplug A ─────────────────────────────────────────────────────
    let rescan = backend.platform.apply_connector_snapshot(
        vec![snapshot_b.clone()],
        &std::collections::HashSet::from([a.clone(), b.clone()]),
    );
    let _ = backend.reconcile_connector_registry(
        &rescan.connected,
        &rescan.dropped_keys,
        &rescan.dropped_layouts,
    );
    let reserved = backend.reserved_layout_slots();
    assert_eq!(reserved, vec![(0, 0, 1920, 1080)]);
    let configured = backend.randr_id_alloc.client_configured_keys();
    assert!(configured.is_empty(), "a bare session pins nothing");
    backend
        .platform
        .recompact_horizontal_layout(&configured, &reserved);
    backend
        .platform
        .recompute_fb_extent_with_reservations(&reserved);

    assert_eq!(backend.platform.outputs.len(), 1);
    assert_eq!(
        (backend.platform.outputs[0].x, backend.platform.outputs[0].y),
        (1920, 0),
        "B must not move into A's reserved slot",
    );
    assert_eq!(backend.platform.fb_dimensions(), (5120, 1440));

    // ── Replug A ─────────────────────────────────────────────────────
    let _ = backend.reconcile_connector_registry(&[snapshot_a, snapshot_b], &[], &[]);
    let requests = backend.take_relight_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].key, a);
    assert_eq!(
        (requests[0].x, requests[0].y),
        (0, 0),
        "A returns to its own slot"
    );

    let a_rect = (
        requests[0].x,
        requests[0].y,
        requests[0].mode.width,
        requests[0].mode.height,
    );
    let survivor = &backend.platform.outputs[0];
    let b_rect = (survivor.x, survivor.y, survivor.width, survivor.height);
    assert!(
        !rects_overlap(a_rect, b_rect),
        "the relit route {a_rect:?} must not overlap the survivor {b_rect:?}",
    );
}

/// The reservation feeds the DERIVED extent only. `rebuild_randr_state`
/// still carries a client-set logical size forward over it, so a desktop
/// that laid itself out with `RRSetScreenSize` keeps the size it asked
/// for while a slot is reserved (Xorg keeps `pScreen->width/height`
/// until the client resizes).
#[test]
fn a_client_set_logical_size_survives_a_rebuild_while_a_slot_is_reserved() {
    let mut backend = KmsBackend::for_tests();
    clear_test_outputs(&mut backend);
    let a = push_enabled_test_output(&mut backend, "A", 7, 0, 0, 1920, 1080);
    let b = push_enabled_test_output(&mut backend, "B", 8, 1920, 0, 1920, 1080);
    let snapshot_a = relight_test_snapshot(&a, vec![test_advertised_mode(1920, 1080, 60, true)]);

    // B departs physically: its slot is reserved, the extent holds.
    let rescan = backend
        .platform
        .apply_connector_snapshot(vec![snapshot_a], &std::collections::HashSet::from([a, b]));
    let _ = backend.reconcile_connector_registry(
        &rescan.connected,
        &rescan.dropped_keys,
        &rescan.dropped_layouts,
    );
    let reserved = backend.reserved_layout_slots();
    assert_eq!(reserved, vec![(1920, 0, 1920, 1080)]);
    backend
        .platform
        .recompute_fb_extent_with_reservations(&reserved);
    assert_eq!(backend.platform.fb_dimensions(), (3840, 1080));

    let (outputs, modes) = backend.randr_outputs_and_modes();
    let (fb_w, fb_h) = backend.fb_dimensions();
    let mut state = ServerState::with_randr_outputs_and_modes(
        fb_w,
        fb_h,
        outputs,
        modes,
        yserver_core::server::BackendCapabilities::from_backend(&backend),
    );
    // The desktop shrank the logical screen around the survivor.
    state.randr.set_logical_size(1920, 1080, 508, 286);

    backend.rebuild_randr_state(&mut state, None, true);

    assert_eq!(
        (
            state.randr.screen_width,
            state.randr.screen_height,
            state.randr.width_mm,
            state.randr.height_mm,
        ),
        (1920, 1080, 508, 286),
        "the client-owned logical size is carried forward verbatim",
    );
}

#[test]
fn dropping_a_never_enabled_output_reserves_nothing_and_survivors_compact() {
    // The reservation must not freeze the layout unconditionally.
    let mut backend = KmsBackend::for_tests();
    clear_test_outputs(&mut backend);
    let a = push_enabled_test_output(&mut backend, "A", 7, 0, 0, 1920, 1080);
    let b = push_enabled_test_output(&mut backend, "B", 8, 1920, 0, 3200, 1440);
    // A was never enabled — it is connected-but-Off, as a runtime-added
    // connector enters the registry.
    backend.randr_id_alloc.entry_mut(&a).config = crate::kms::render::backend::ConnectorConfig::Off;
    let snapshot_b = relight_test_snapshot(&b, vec![test_advertised_mode(3200, 1440, 60, true)]);

    let rescan = backend.platform.apply_connector_snapshot(
        vec![snapshot_b],
        &std::collections::HashSet::from([a.clone(), b]),
    );
    let _ = backend.reconcile_connector_registry(
        &rescan.connected,
        &rescan.dropped_keys,
        &rescan.dropped_layouts,
    );

    let reserved = backend.reserved_layout_slots();
    assert!(reserved.is_empty(), "nothing enabled, nothing to reserve");
    let configured = backend.randr_id_alloc.client_configured_keys();
    backend
        .platform
        .recompact_horizontal_layout(&configured, &reserved);
    backend
        .platform
        .recompute_fb_extent_with_reservations(&reserved);

    assert_eq!(
        (backend.platform.outputs[0].x, backend.platform.outputs[0].y),
        (0, 0),
    );
    assert_eq!(backend.platform.fb_dimensions(), (3200, 1440));
}

#[test]
fn startup_inventory_reserves_xids_but_heavy_snapshot_owns_identity_and_state() {
    use crate::platform::drm::ConnectorProbe;

    let mut backend = KmsBackend::for_tests();
    let key = backend.platform.outputs[0].key.clone();
    let probes = vec![(
        key.device_key,
        vec![ConnectorProbe {
            connector_name: key.connector_name.clone(),
            connected: true,
            modes: backend.platform.outputs[0].output.modes.clone(),
        }],
    )];
    let heavy_mode = test_advertised_mode(1200, 1600, 60, true);
    let snapshot = ConnectorSnapshot {
        key: key.clone(),
        modes: vec![heavy_mode.clone()],
        edid: vec![0xde, 0xad],
        mm_width: 203,
        mm_height: 271,
        connector_type: "HDMI".into(),
    };

    backend.randr_id_alloc = RandrIdAllocator::default();
    backend.seed_connector_topology_from_probes(&probes, std::slice::from_ref(&snapshot));
    let entry = backend.randr_id_alloc.entry(&key).unwrap();
    let ids = entry.ids;
    assert!(entry.connected);
    assert_eq!(entry.modes, vec![heavy_mode]);
    assert_eq!(entry.edid, vec![0xde, 0xad]);
    assert_eq!((entry.mm_width, entry.mm_height), (203, 271));
    assert_eq!(entry.connector_type, "HDMI");
    assert!(matches!(
        entry.config,
        crate::kms::render::backend::ConnectorConfig::Enabled { .. }
    ));

    backend.randr_id_alloc = RandrIdAllocator::default();
    backend.seed_connector_topology_from_probes(&probes, &[]);
    let entry = backend.randr_id_alloc.entry(&key).unwrap();
    assert_eq!(
        entry.ids, ids,
        "deterministic startup order keeps connector XIDs"
    );
    assert!(
        !entry.connected,
        "forced snapshot overrides cached connected state"
    );
    assert_eq!(
        entry.config,
        crate::kms::render::backend::ConnectorConfig::Off
    );
    let output = backend
        .randr_outputs()
        .into_iter()
        .find(|output| output.output_id == ids.output_id)
        .unwrap();
    assert!(
        !output.connected,
        "stale ActiveOutput must not revive startup state"
    );
}

#[test]
fn lightweight_mode_change_invalidates_identity_until_heavy_refresh() {
    use crate::platform::drm::ConnectorProbe;

    let mut backend = KmsBackend::for_tests();
    let key = backend.platform.outputs[0].key.clone();
    let replacement_mode = test_advertised_mode(1200, 1600, 60, true);
    {
        let entry = backend.randr_id_alloc.entry_mut(&key);
        entry.edid = vec![0x01, 0x02];
        entry.mm_width = 600;
        entry.mm_height = 340;
        entry.connector_type = "DisplayPort".into();
    }
    let probe = ConnectorProbe {
        connector_name: key.connector_name.clone(),
        connected: true,
        modes: vec![replacement_mode.clone()],
    };

    assert!(
        !reconcile_connector_probe(
            &mut backend.randr_id_alloc,
            key.device_key,
            std::slice::from_ref(&probe),
        )
        .is_empty()
    );
    let entry = backend.randr_id_alloc.entry(&key).unwrap();
    assert_eq!(entry.modes, vec![replacement_mode]);
    assert!(entry.edid.is_empty());
    assert_eq!((entry.mm_width, entry.mm_height), (0, 0));
    assert_eq!(entry.connector_type, "DisplayPort");
    assert!(
        reconcile_connector_probe(&mut backend.randr_id_alloc, key.device_key, &[probe],)
            .is_empty(),
        "the same lightweight evidence is idempotent"
    );
}

#[test]
fn lightweight_same_modes_cannot_detect_edid_only_replacement() {
    use crate::platform::drm::ConnectorProbe;

    let mut backend = KmsBackend::for_tests();
    let key = backend.platform.outputs[0].key.clone();
    let entry = backend.randr_id_alloc.entry_mut(&key);
    entry.edid = vec![0xaa, 0xbb];
    entry.mm_width = 500;
    entry.mm_height = 300;
    let probe = ConnectorProbe {
        connector_name: key.connector_name.clone(),
        connected: true,
        modes: entry.modes.clone(),
    };

    assert!(
        reconcile_connector_probe(&mut backend.randr_id_alloc, key.device_key, &[probe],)
            .is_empty()
    );
    let entry = backend.randr_id_alloc.entry(&key).unwrap();
    assert_eq!(entry.edid, vec![0xaa, 0xbb]);
    assert_eq!((entry.mm_width, entry.mm_height), (500, 300));
}

#[test]
fn connector_registry_detects_edid_only_replacement_and_keeps_xids() {
    let mut backend = KmsBackend::for_tests();
    let key = test_output_key(0, "HDMI-A-1");
    let first = ConnectorSnapshot {
        key: key.clone(),
        modes: vec![test_advertised_mode(1600, 1200, 60, true)],
        edid: vec![0x01, 0x02],
        mm_width: 600,
        mm_height: 340,
        connector_type: "HDMI".to_string(),
    };
    let replacement = ConnectorSnapshot {
        edid: vec![0x03, 0x04],
        ..first.clone()
    };

    assert!(
        !backend
            .reconcile_connector_registry(std::slice::from_ref(&first), &[], &[])
            .is_empty()
    );
    let ids = backend.randr_id_alloc.entry(&key).unwrap().ids;
    let replacement_delta =
        backend.reconcile_connector_registry(std::slice::from_ref(&replacement), &[], &[]);
    assert!(
        !replacement_delta.is_empty(),
        "a changed EDID must invalidate the heavy RANDR projection"
    );
    assert!(
        !replacement_delta.config_changed,
        "identity-only metadata does not advance lastConfigTime",
    );
    let entry = backend.randr_id_alloc.entry(&key).unwrap();
    assert_eq!(entry.ids, ids, "monitor replacement reuses connector XIDs");
    assert_eq!(entry.edid, replacement.edid);
    assert!(
        backend
            .reconcile_connector_registry(std::slice::from_ref(&replacement), &[], &[])
            .is_empty(),
        "the refreshed heavy snapshot is idempotent"
    );
}

#[test]
fn live_randr_projection_uses_heavy_registry_identity() {
    let mut backend = KmsBackend::for_tests();
    let key = backend.platform.outputs[0].key.clone();
    let initial = backend.randr_outputs();
    let initial_output = initial.iter().find(|output| output.name == "test").unwrap();
    let initial_ids = (initial_output.output_id, initial_output.crtc_id);
    let replacement = ConnectorSnapshot {
        key,
        modes: vec![test_advertised_mode(1200, 1600, 60, true)],
        edid: vec![0x00, 0xff, 0xff, 0xff, 0xff],
        mm_width: 203,
        mm_height: 271,
        connector_type: "HDMI".to_string(),
    };

    assert!(
        !backend
            .reconcile_connector_registry(std::slice::from_ref(&replacement), &[], &[])
            .is_empty()
    );
    let (refreshed, mode_table) = backend.randr_outputs_and_modes();
    let output = refreshed
        .iter()
        .find(|output| output.name == "test")
        .unwrap();

    assert_eq!((output.output_id, output.crtc_id), initial_ids);
    assert_eq!((output.mm_width, output.mm_height), (203, 271));
    assert!(
        mode_table.iter().any(|mode| mode.mode_id == output.mode_id),
        "the preserved programmed mode must remain in screen resources"
    );
    assert_eq!(
        backend.output_identity(output.output_id),
        Some((replacement.edid, "HDMI".to_string()))
    );
}

#[test]
fn connected_off_output_exposes_identity_and_disconnect_clears_it() {
    let mut backend = KmsBackend::for_tests();
    let key = test_output_key(0, "HDMI-A-1");
    let snapshot = ConnectorSnapshot {
        key: key.clone(),
        modes: vec![test_advertised_mode(1200, 1600, 60, true)],
        edid: vec![0x00, 0xff, 0xff, 0xff],
        mm_width: 203,
        mm_height: 271,
        connector_type: "HDMI".to_string(),
    };
    assert!(
        !backend
            .reconcile_connector_registry(std::slice::from_ref(&snapshot), &[], &[])
            .is_empty()
    );
    let ids = backend.randr_id_alloc.entry(&key).unwrap().ids;

    let (outputs, _) = backend.randr_outputs_and_modes();
    let connected = outputs
        .iter()
        .find(|output| output.output_id == ids.output_id)
        .unwrap();
    assert!(connected.connected);
    assert_eq!(connected.mode_id, 0);
    assert_eq!((connected.mm_width, connected.mm_height), (203, 271));
    assert_eq!(
        backend.output_identity(ids.output_id),
        Some((snapshot.edid, "HDMI".to_string()))
    );

    assert!(
        !backend
            .reconcile_connector_registry(&[], std::slice::from_ref(&key), &[])
            .is_empty()
    );
    let (outputs, _) = backend.randr_outputs_and_modes();
    let disconnected = outputs
        .iter()
        .find(|output| output.output_id == ids.output_id)
        .unwrap();
    assert!(!disconnected.connected);
    assert_eq!((disconnected.mm_width, disconnected.mm_height), (0, 0));
    assert_eq!(backend.output_identity(ids.output_id), None);
}

#[test]
fn lightweight_active_disconnect_retains_crtc_then_heavy_detach_publishes_off() {
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        io::Read,
        os::unix::net::UnixStream,
        sync::{Arc, Mutex, atomic::AtomicU16},
    };
    use yserver_core::server::ClientState;
    use yserver_protocol::x11::{ClientByteOrder, randr as x11randr};

    let mut backend = KmsBackend::for_tests();
    let key = backend.platform.outputs[0].key.clone();
    let device_key = key.device_key;
    let (outputs, modes) = backend.randr_outputs_and_modes();
    let live = outputs
        .iter()
        .find(|output| output.name == key.connector_name)
        .unwrap();
    let (output_id, crtc_id, mode_id) = (live.output_id, live.crtc_id, live.mode_id);
    let mut state = ServerState::with_randr_outputs_and_modes(
        backend.platform.fb_w,
        backend.platform.fb_h,
        outputs,
        modes,
        yserver_core::server::BackendCapabilities::from_backend(&backend),
    );
    state.randr.timestamp = 41;
    state.randr.config_timestamp = 37;

    let (mut peer, writer) = UnixStream::pair().unwrap();
    writer.set_nonblocking(true).unwrap();
    state.clients.insert(
        7,
        ClientState {
            writer: Arc::new(Mutex::new(yserver_core::transport::Transport::Unix(writer))),
            byte_order: ClientByteOrder::LittleEndian,
            last_sequence: Arc::new(AtomicU16::new(9)),
            resource_id_base: 0,
            resource_id_mask: 0,
            event_masks: HashMap::new(),
            save_set: HashSet::new(),
            big_requests_enabled: false,
            xi2_masks: HashMap::new(),
            xi1_event_classes: HashSet::new(),
            xi1_window_event_classes: HashMap::new(),
            outbound: VecDeque::new(),
            watching_writable: false,
            write_failed: false,
            focused_window: yserver_core::resources::ROOT_WINDOW,
            reader_control: None,
            is_local: true,
            fd_passing: true,
        },
    );
    state.randr_select_masks.insert(
        (7, yserver_core::resources::ROOT_WINDOW),
        x11randr::NOTIFY_MASK_CRTC_CHANGE | x11randr::NOTIFY_MASK_OUTPUT_CHANGE,
    );
    let probes = vec![(
        device_key,
        vec![crate::platform::drm::ConnectorProbe {
            connector_name: key.connector_name.clone(),
            connected: false,
            modes: Vec::new(),
        }],
    )];

    assert!(backend.publish_connector_probes(&mut state, &probes));
    assert_eq!(
        state.randr.timestamp, 41,
        "force query preserves lastSetTime"
    );
    assert_ne!(state.randr.config_timestamp, 37);

    let mut wire = vec![0; 32];
    peer.read_exact(&mut wire).unwrap();
    let event = wire.as_slice();
    assert_eq!(
        u32::from_le_bytes(event[16..20].try_into().unwrap()),
        output_id,
        "only the dirty output is notified",
    );
    assert_eq!(u32::from_le_bytes(event[4..8].try_into().unwrap()), 41);
    assert_eq!(
        u32::from_le_bytes(event[8..12].try_into().unwrap()),
        state.randr.config_timestamp,
    );
    assert_eq!(
        u32::from_le_bytes(event[20..24].try_into().unwrap()),
        crtc_id,
        "a lightweight disconnect does not detach the current CRTC",
    );
    assert_eq!(
        u32::from_le_bytes(event[24..28].try_into().unwrap()),
        mode_id
    );
    assert_eq!(event[30], x11randr::CONNECTION_DISCONNECTED);
    assert_eq!(event[1], x11randr::NOTIFY_OUTPUT_CHANGE);

    let projected = state
        .randr
        .outputs
        .iter()
        .find(|output| output.output_id == output_id)
        .unwrap();
    assert!(!projected.connected);
    assert_eq!((projected.crtc_id, projected.mode_id), (crtc_id, mode_id));
    let output_info = state
        .randr
        .output_info(output_id, state.randr.config_timestamp)
        .unwrap();
    assert_eq!(output_info.connection, x11randr::CONNECTION_DISCONNECTED);
    assert_eq!((output_info.crtc, output_info.mode_id), (crtc_id, mode_id));

    let light_config_timestamp = state.randr.config_timestamp;
    assert!(
        backend
            .reconcile_connector_registry(&[], std::slice::from_ref(&key), &[])
            .is_empty(),
        "the later heavy boundary sees no second advertised connector delta",
    );
    // Model the heavy topology apply retiring the ActiveOutput. The
    // registry's Off config prevents a stale platform row from reviving
    // it; clearing the row is the production apply's corresponding side.
    backend.platform.outputs.clear();
    backend.rebuild_randr_state(&mut state, None, false);
    assert_eq!(state.randr.timestamp, 41);
    assert_eq!(state.randr.config_timestamp, light_config_timestamp);
    let detached = state
        .randr
        .outputs
        .iter()
        .find(|output| output.output_id == output_id)
        .unwrap();
    assert!(!detached.connected);
    assert_eq!(detached.mode_id, 0);

    let changed = [(detached.output_id, detached.crtc_id, detached.mode_id)];
    yserver_core::core_loop::run::emit_randr_connector_change_notifications(
        &mut state, &changed, &changed,
    );
    let mut wire = [0; 64];
    peer.read_exact(&mut wire).unwrap();
    let crtc_event = &wire[..32];
    let output_event = &wire[32..];
    assert_eq!(crtc_event[1], x11randr::NOTIFY_CRTC_CHANGE);
    assert_eq!(output_event[1], x11randr::NOTIFY_OUTPUT_CHANGE);
    assert_eq!(
        u32::from_le_bytes(crtc_event[12..16].try_into().unwrap()),
        crtc_id,
    );
    assert_eq!(
        u32::from_le_bytes(crtc_event[16..20].try_into().unwrap()),
        0
    );
    assert_eq!(
        u32::from_le_bytes(output_event[20..24].try_into().unwrap()),
        0
    );
    assert_eq!(
        u32::from_le_bytes(output_event[24..28].try_into().unwrap()),
        0
    );
    assert_eq!(output_event[30], x11randr::CONNECTION_DISCONNECTED);
    assert_eq!(
        u32::from_le_bytes(output_event[8..12].try_into().unwrap()),
        light_config_timestamp,
        "heavy detach after a published light disconnect does not advance lastConfigTime",
    );
}

#[test]
fn light_mode_change_then_heavy_connected_route_retirement_is_not_a_second_config_delta() {
    use crate::platform::drm::ConnectorProbe;

    let mut backend = KmsBackend::for_tests();
    let key = backend.platform.outputs[0].key.clone();
    let replacement_mode = test_advertised_mode(1024, 768, 60, true);
    let light_delta = reconcile_connector_probe(
        &mut backend.randr_id_alloc,
        key.device_key,
        &[ConnectorProbe {
            connector_name: key.connector_name.clone(),
            connected: true,
            modes: vec![replacement_mode.clone()],
        }],
    );
    assert!(light_delta.config_changed);
    assert!(matches!(
        backend.randr_id_alloc.entry(&key).unwrap().config,
        crate::kms::render::backend::ConnectorConfig::Enabled { .. }
    ));

    let (outputs, modes) = backend.randr_outputs_and_modes();
    let mut state = ServerState::with_randr_outputs_and_modes(
        backend.platform.fb_w,
        backend.platform.fb_h,
        outputs,
        modes,
        yserver_core::server::BackendCapabilities::from_backend(&backend),
    );
    state.randr.timestamp = 41;
    state.randr.config_timestamp = 37;

    let entry = backend.randr_id_alloc.entry(&key).unwrap();
    let snapshot = ConnectorSnapshot {
        key: key.clone(),
        modes: vec![replacement_mode],
        edid: entry.edid.clone(),
        mm_width: entry.mm_width,
        mm_height: entry.mm_height,
        connector_type: entry.connector_type.clone(),
    };
    let heavy_delta = backend.reconcile_connector_registry(
        std::slice::from_ref(&snapshot),
        std::slice::from_ref(&key),
        &[],
    );
    assert!(heavy_delta.is_empty());
    assert!(!heavy_delta.config_changed);
    assert_eq!(
        backend.randr_id_alloc.entry(&key).unwrap().config,
        crate::kms::render::backend::ConnectorConfig::Off,
        "the unusable live route is still retired internally",
    );

    backend.platform.outputs.clear();
    backend.rebuild_randr_state(&mut state, None, heavy_delta.config_changed);
    assert_eq!(
        (state.randr.timestamp, state.randr.config_timestamp),
        (41, 37)
    );
    let output = state
        .randr
        .outputs
        .iter()
        .find(|output| output.name == key.connector_name)
        .unwrap();
    assert!(output.connected);
    assert_eq!(output.mode_id, 0);
}

#[test]
fn heavy_disconnect_projection_does_not_revive_stale_active_output() {
    let mut backend = KmsBackend::for_tests();
    let key = backend.platform.outputs[0].key.clone();
    let initial = backend.randr_outputs();
    let ids = initial
        .iter()
        .find(|output| output.name == "test")
        .map(|output| (output.output_id, output.crtc_id))
        .unwrap();

    assert!(
        !backend
            .reconcile_connector_registry(&[], std::slice::from_ref(&key), &[])
            .is_empty()
    );
    let projected = backend.randr_outputs();
    let output = projected
        .iter()
        .find(|output| output.output_id == ids.0)
        .unwrap();

    assert_eq!(output.crtc_id, ids.1);
    assert!(!output.connected);
    assert_eq!(output.mode_id, 0);
    let mut state = ServerState::new();
    backend.rebuild_randr_state(&mut state, None, false);
    assert_eq!(
        state
            .randr
            .output_info(ids.0, 0)
            .expect("disconnected output remains queryable")
            .crtc,
        ids.1,
        "physical loss retains the former CRTC association until a client disables it",
    );
    assert!(
        !backend.randr_id_alloc.entry(&key).unwrap().connected,
        "stale platform output state must not overwrite a heavy disconnect"
    );
}

#[test]
fn randr_mode_ids_dedup_exact_modes_but_distinguish_timings() {
    let mut alloc = RandrIdAllocator::default();
    let mode = test_advertised_mode(2560, 1440, 60, true);
    let same_mode_different_preference = crate::platform::drm::Mode {
        preferred: false,
        ..mode.clone()
    };
    let different_timing = crate::platform::drm::Mode {
        clock_khz: 241_500,
        hsync_start: 2608,
        hsync_end: 2640,
        htotal: 2720,
        vsync_start: 1443,
        vsync_end: 1448,
        vtotal: 1481,
        ..mode.clone()
    };
    let m1 = alloc.mode_id(&mode);
    let m2 = alloc.mode_id(&same_mode_different_preference);
    let m3 = alloc.mode_id(&different_timing);
    assert_eq!(m1, m2);
    assert_ne!(m1, m3);
}

#[test]
fn same_signature_monitor_replacement_keeps_exact_timings_distinct() {
    let mut backend = KmsBackend::for_tests();
    let initial = backend.randr_outputs();
    let initial = initial.iter().find(|output| output.name == "test").unwrap();
    let current_mode_id = initial.mode_id;
    let key = backend.platform.outputs[0].key.clone();
    let replacement_mode = crate::platform::drm::Mode {
        name: "test".to_string(),
        width: 800,
        height: 600,
        vrefresh: 60,
        preferred: true,
        clock_khz: 40_000,
        hsync_start: 840,
        hsync_end: 968,
        htotal: 1056,
        vsync_start: 601,
        vsync_end: 605,
        vtotal: 628,
        flags: 0x5,
        ..Default::default()
    };
    let replacement = ConnectorSnapshot {
        key,
        modes: vec![replacement_mode.clone()],
        edid: vec![0x00, 0xff, 0xff, 0xff, 0x02],
        mm_width: 211,
        mm_height: 158,
        connector_type: "HDMI".to_string(),
    };

    assert!(
        !backend
            .reconcile_connector_registry(&[replacement], &[], &[])
            .is_empty()
    );
    let (outputs, modes) = backend.randr_outputs_and_modes();
    let output = outputs.iter().find(|output| output.name == "test").unwrap();
    let advertised_mode_id = output.mode_ids[0];

    assert_eq!(output.mode_id, current_mode_id);
    assert_ne!(
        output.mode_id, advertised_mode_id,
        "the programmed old timing and replacement timing need distinct XIDs"
    );
    assert_eq!(
        modes
            .iter()
            .find(|mode| mode.mode_id == output.mode_id)
            .and_then(|mode| mode.timing),
        None,
    );
    assert_eq!(
        modes
            .iter()
            .find(|mode| mode.mode_id == advertised_mode_id)
            .and_then(|mode| mode.timing),
        mode_timing(&replacement_mode),
    );
}

/// Issue #48: connector-local duplicate nominal timings are collapsed
/// before projection; the output's XID list must also remain duplicate
/// free when exact copies reach the registry through synthetic fixtures.
#[test]
fn randr_output_mode_ids_have_no_duplicate_xids() {
    let mut b = KmsBackend::for_tests();
    let hdmi_key = OutputKey::new(
        b.platform
            .primary_device()
            .expect("test fixture has a DRM device")
            .key,
        "HDMI-A-1",
    );
    {
        let e = b.randr_id_alloc.entry_mut(&hdmi_key);
        e.connected = true;
        e.config = crate::kms::render::backend::ConnectorConfig::Off;
        e.modes = vec![
            test_advertised_mode(1920, 1080, 60, true),
            test_advertised_mode(1920, 1080, 60, false),
            test_advertised_mode(1920, 1080, 60, false),
        ];
    }

    let (outs, _modes) = b.randr_outputs_and_modes();
    let hdmi = outs
        .iter()
        .find(|o| o.name == "HDMI-A-1")
        .expect("HDMI-A-1 present");

    let unique: std::collections::HashSet<u32> = hdmi.mode_ids.iter().copied().collect();
    assert_eq!(
        hdmi.mode_ids.len(),
        unique.len(),
        "GetOutputInfo must not repeat the same mode XID: {:?}",
        hdmi.mode_ids,
    );
}

// Task 5.2: a hotplugged-but-not-yet-enabled output (registry
// connected=true, config=Off, NOT in platform.outputs) must report
// RR_Connected with mode=0 — "connected, dark" — so a client can
// enable it, NOT RR_Connected+enabled (the old auto-enable bug that
// made Display settings show it "on") and NOT RR_Disconnected. A
// physically-absent connector still reports RR_Disconnected.
#[test]
fn not_live_output_reports_connected_off_vs_disconnected() {
    let mut b = KmsBackend::for_tests();
    let device_key = b
        .platform
        .primary_device()
        .expect("test fixture has a DRM device")
        .key;
    let hdmi_key = OutputKey::new(device_key, "HDMI-A-1");
    let dp_key = OutputKey::new(device_key, "DP-3");

    // Hotplugged, registered OFF (the Task 5.2 add-path outcome).
    {
        let e = b.randr_id_alloc.entry_mut(&hdmi_key);
        e.connected = true;
        e.config = crate::kms::render::backend::ConnectorConfig::Off;
        e.modes = vec![test_advertised_mode(1920, 1080, 60, true)];
    }
    // Physically disconnected (default connected=false).
    let _ = b.randr_id_alloc.entry_mut(&dp_key);

    let (outs, _modes) = b.randr_outputs_and_modes();

    let hdmi = outs
        .iter()
        .find(|o| o.name == "HDMI-A-1")
        .expect("HDMI-A-1 present");
    assert!(
        hdmi.connected,
        "hotplugged-off output must report RR_Connected",
    );
    assert_eq!(hdmi.mode_id, 0, "off output has no current mode");
    assert!(
        !hdmi.mode_ids.is_empty(),
        "off output still advertises its modes so a client can enable it",
    );

    let dp = outs
        .iter()
        .find(|o| o.name == "DP-3")
        .expect("DP-3 present");
    assert!(
        !dp.connected,
        "physically-absent connector reports RR_Disconnected",
    );
}
