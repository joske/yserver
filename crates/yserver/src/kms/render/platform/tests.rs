use super::*;

#[test]
fn prime_render_probe_fence_timeout_is_two_hundred_milliseconds() {
    assert_eq!(PRIME_RENDER_PROBE_TIMEOUT_NS, 200_000_000);
}

#[test]
fn qualified_scanout_plan_is_resource_free_and_preserves_the_exact_choice() {
    fn assert_copy<T: Copy>() {}

    assert_copy::<QualifiedScanoutPlan>();
    assert!(!std::mem::needs_drop::<QualifiedScanoutPlan>());

    let shared_plan = ScanoutAllocationPlan::ExplicitLinear;
    assert_eq!(
        QualifiedScanoutPlan::Shared(shared_plan),
        QualifiedScanoutPlan::Shared(ScanoutAllocationPlan::ExplicitLinear)
    );

    let sink_id = RenderDeviceId::DrmRender(drm_key(7));
    let copied_plan = CopiedScanoutPlan {
        source: crate::kms::vk::scanout::CopiedSourcePlan::DrmModifier(0),
        destination: ScanoutAllocationPlan::LegacyLinear,
    };
    assert_eq!(
        QualifiedScanoutPlan::Copied {
            sink_id,
            plan: copied_plan,
        },
        QualifiedScanoutPlan::Copied {
            sink_id,
            plan: copied_plan,
        }
    );
}

#[test]
fn worker_qualification_exhausts_copy_free_before_preserving_copied_order() {
    let calls = RefCell::new(Vec::new());
    let sink_id = RenderDeviceId::DrmRender(drm_key(9));
    let copied_plan = CopiedScanoutPlan {
        source: crate::kms::vk::scanout::CopiedSourcePlan::DrmModifier(0x91),
        destination: ScanoutAllocationPlan::ExplicitLinear,
    };

    let qualified = qualify_scanout_candidates_in_order(
        ["shared-native", "shared-linear"],
        |candidate| {
            calls.borrow_mut().push(candidate);
            Err(ScanoutQualificationError::Rejected(io::Error::other(
                candidate,
            )))
        },
        || {
            calls.borrow_mut().push("copied-inventory");
            Ok(vec![
                ("copied-native", copied_plan),
                ("copied-linear", copied_plan),
            ])
        },
        |(candidate, plan)| {
            calls.borrow_mut().push(candidate);
            Ok(QualifiedScanoutPlan::Copied { sink_id, plan })
        },
    )
    .expect("the first copied candidate should win");

    assert_eq!(
        calls.into_inner(),
        vec![
            "shared-native",
            "shared-linear",
            "copied-inventory",
            "copied-native"
        ]
    );
    assert_eq!(
        qualified,
        QualifiedScanoutPlan::Copied {
            sink_id,
            plan: copied_plan,
        }
    );
}

#[test]
fn worker_qualification_stops_immediately_after_indeterminate_submission() {
    let calls = RefCell::new(Vec::new());
    let result = qualify_scanout_candidates_in_order(
        ["shared-first", "shared-must-not-run"],
        |candidate| {
            calls.borrow_mut().push(candidate);
            Err(ScanoutQualificationError::Indeterminate(io::Error::new(
                io::ErrorKind::TimedOut,
                "submitted fence did not complete",
            )))
        },
        || {
            calls.borrow_mut().push("copied-must-not-enumerate");
            Ok(vec!["copied-must-not-run"])
        },
        |candidate| {
            calls.borrow_mut().push(candidate);
            Ok(QualifiedScanoutPlan::Shared(
                ScanoutAllocationPlan::ExplicitLinear,
            ))
        },
    );

    assert!(matches!(
        result,
        Err(ScanoutQualificationError::Indeterminate(_))
    ));
    assert_eq!(calls.into_inner(), vec!["shared-first"]);
}

#[test]
fn terminal_disposable_probe_errors_cannot_be_mistaken_for_candidates() {
    let copy_free = CopyFreeScanoutError::TerminalDisposableProbe(io::Error::new(
        io::ErrorKind::TimedOut,
        "copy-free probe expired",
    ));
    assert!(matches!(
        &copy_free,
        CopyFreeScanoutError::TerminalDisposableProbe(_)
    ));
    let copy_free = copy_free.into_io_error();
    assert_eq!(copy_free.kind(), io::ErrorKind::TimedOut);
    assert!(is_terminal_disposable_probe_error(&copy_free));

    let copied = CopiedScanoutError::TerminalDisposableProbe(io::Error::new(
        io::ErrorKind::TimedOut,
        "copied probe expired",
    ));
    assert!(matches!(
        &copied,
        CopiedScanoutError::TerminalDisposableProbe(_)
    ));
    let copied = copied.into_io_error();
    assert_eq!(copied.kind(), io::ErrorKind::TimedOut);
    assert!(is_terminal_disposable_probe_error(&copied));
    assert!(!is_terminal_disposable_probe_error(&io::Error::other(
        "ordinary candidate failure"
    )));
}

#[test]
fn terminal_startup_retains_existing_gpu_owners_without_drop() {
    use std::{cell::Cell, rc::Rc};

    struct DropSpy(Rc<Cell<u8>>);
    impl Drop for DropSpy {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    let pool_drops = Rc::new(Cell::new(0));
    let mut pools = vec![Some(DropSpy(Rc::clone(&pool_drops)))];
    retain_initialized_scanout_pools(&mut pools);
    assert!(pools[0].is_none());
    assert_eq!(pool_drops.get(), 0);

    let owner_drops = Rc::new(Cell::new(0));
    retain_startup_gpu_owners(
        DropSpy(Rc::clone(&owner_drops)),
        DropSpy(Rc::clone(&owner_drops)),
        DropSpy(Rc::clone(&owner_drops)),
    );
    assert_eq!(owner_drops.get(), 0);
}

fn drm_key(minor: u32) -> crate::platform::drm::DrmDeviceKey {
    crate::platform::drm::DrmDeviceKey { major: 226, minor }
}

fn test_render_device(
    id: RenderDeviceId,
    primary: Option<crate::platform::drm::DrmDeviceKey>,
    selector_seed: u8,
) -> RenderDevice {
    RenderDevice {
        id,
        physical_device: vk::PhysicalDevice::default(),
        selector: VulkanDeviceSelector::for_tests(selector_seed),
        advertised_primary_node: primary,
        advertised_render_node: match id {
            RenderDeviceId::DrmRender(key) => Some(key),
            RenderDeviceId::UnverifiedFallback => None,
        },
        render_node: None,
        render_node_device: None,
        syncobj_timeline: false,
    }
}

#[test]
fn copied_sink_resolution_requires_one_exact_distinct_primary_match() {
    let selected = RenderDeviceId::DrmRender(drm_key(128));
    let sink = RenderDeviceId::DrmRender(drm_key(129));
    let renderers = vec![
        test_render_device(selected, Some(drm_key(0)), 1),
        test_render_device(sink, Some(drm_key(1)), 2),
    ];

    assert_eq!(
        resolve_copied_sink_renderer(&renderers, selected, drm_key(1))
            .expect("one exact sink renderer"),
        (sink, VulkanDeviceSelector::for_tests(2)),
    );
    assert!(
        resolve_copied_sink_renderer(&renderers, selected, drm_key(0)).is_err(),
        "the selected renderer is never reused as copied sink B"
    );
    assert!(
        resolve_copied_sink_renderer(&renderers, selected, drm_key(2)).is_err(),
        "a display-only sink must not trigger generic Vulkan rescoring"
    );
}

#[test]
fn copied_sink_resolution_rejects_ambiguous_primary_claims() {
    let selected = RenderDeviceId::DrmRender(drm_key(128));
    let renderers = vec![
        test_render_device(selected, Some(drm_key(0)), 1),
        test_render_device(RenderDeviceId::DrmRender(drm_key(129)), Some(drm_key(1)), 2),
        test_render_device(RenderDeviceId::DrmRender(drm_key(130)), Some(drm_key(1)), 3),
    ];

    let error = resolve_copied_sink_renderer(&renderers, selected, drm_key(1))
        .expect_err("ambiguous sink identity must not be guessed");
    assert!(error.to_string().contains("multiple Vulkan renderers"));
    assert!(error.to_string().contains("minor: 129"));
    assert!(error.to_string().contains("minor: 130"));
}

#[test]
fn copied_live_device_lost_survives_platform_error_wrapping() {
    let error = CopiedScanoutError::LiveDeviceLost {
        context: "test copied live allocation".to_owned(),
        source: crate::kms::vk::scanout::device_lost_scanout_error_for_tests(),
    }
    .into_io_error();

    assert!(crate::kms::vk::scanout::scanout_error_is_device_lost(
        &error
    ));
    assert!(error.to_string().contains("test copied live allocation"));
}

#[test]
fn disposable_device_lost_survives_plan_context_and_classification() {
    let copied_error =
        DisposableProbeError::from(crate::kms::vk::scanout::device_lost_scanout_error_for_tests())
            .into_io_error_with_context("test copied disposable content probe");
    let copied = classify_copied_qualification_error(CopiedScanoutError::Candidates(copied_error));

    let ScanoutQualificationError::DeviceLost(error) = copied else {
        panic!("disposable Vulkan device loss must not become route rejection");
    };
    assert!(crate::kms::vk::scanout::scanout_error_is_device_lost(
        &error
    ));
    assert!(
        error
            .to_string()
            .contains("test copied disposable content probe")
    );

    let copy_free_error =
        DisposableProbeError::from(crate::kms::vk::scanout::device_lost_scanout_error_for_tests())
            .into_io_error_with_context("test copy-free disposable content probe");
    let copy_free =
        classify_copy_free_qualification_error(CopyFreeScanoutError::Candidates(copy_free_error));
    let ScanoutQualificationError::DeviceLost(error) = copy_free else {
        panic!("copy-free Vulkan device loss must not become route rejection");
    };
    assert!(crate::kms::vk::scanout::scanout_error_is_device_lost(
        &error
    ));
    assert!(
        error
            .to_string()
            .contains("test copy-free disposable content probe")
    );
}

#[test]
fn copied_scanout_rejects_sink_without_explicit_dmabuf_layout_import() {
    let error = require_copied_sink_explicit_dmabuf_layout_import(false)
        .expect_err("implicit linear layout import is unsafe for copied scanout");
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    assert!(
        error
            .to_string()
            .contains("VK_EXT_image_drm_format_modifier")
    );
    require_copied_sink_explicit_dmabuf_layout_import(true)
        .expect("explicit modifier-layout import is accepted");
}

#[test]
fn topology_reset_cancels_completion_for_a_surviving_output() {
    use nix::sys::eventfd::{EfdFlags, EventFd};

    let mut platform = PlatformBackend::for_tests();
    let output_key = platform.outputs[0].key.clone();
    let ready: OwnedFd =
        EventFd::from_value_and_flags(1, EfdFlags::EFD_CLOEXEC | EfdFlags::EFD_NONBLOCK)
            .expect("ready eventfd")
            .into();
    platform
        .register_scanout_render_completion(output_key, 1, Some(ready))
        .expect("register pollable copied completion");

    platform
        .reset_scanout_bos_for_suspend()
        .expect("Vk-less fixture reset");

    assert!(
        platform.drain_scanout_render_completions().is_empty(),
        "the topology-quiesce reset must cancel jobs even when the output survives",
    );
}

#[test]
fn scanout_render_completion_drain_is_not_queue_front_blocked() {
    use nix::sys::eventfd::{EfdFlags, EventFd};

    let mut platform = PlatformBackend::for_tests();
    let output_key = platform.outputs[0].key.clone();
    let blocked: OwnedFd =
        EventFd::from_value_and_flags(0, EfdFlags::EFD_CLOEXEC | EfdFlags::EFD_NONBLOCK)
            .expect("blocked eventfd")
            .into();
    let ready: OwnedFd =
        EventFd::from_value_and_flags(1, EfdFlags::EFD_CLOEXEC | EfdFlags::EFD_NONBLOCK)
            .expect("ready eventfd")
            .into();
    let first_job = platform
        .register_scanout_render_completion(output_key.clone(), 0, Some(blocked))
        .expect("register first job");
    let second_job = platform
        .register_scanout_render_completion(output_key.clone(), 1, Some(ready))
        .expect("register second job");

    let completions = platform.drain_scanout_render_completions();
    assert_eq!(completions.len(), 1);
    assert_eq!(completions[0].job_id, second_job);
    assert_eq!(completions[0].output_key, output_key);
    assert_eq!(completions[0].bo_idx, 1);
    assert_eq!(
        platform.pending_scanout_render_completions[0].job_id, first_job,
        "an unreadable earlier job remains registered"
    );

    platform.cancel_scanout_render_completions_for_output(&output_key);
    assert!(platform.pending_scanout_render_completions.is_empty());
}

#[test]
fn already_signalled_scanout_completion_is_immediately_ready_without_fd() {
    let mut platform = PlatformBackend::for_tests();
    let output_key = platform.outputs[0].key.clone();
    let job = platform
        .register_scanout_render_completion(output_key.clone(), 2, None)
        .expect("register Vulkan fd=-1 completion");

    let completions = platform.drain_scanout_render_completions();
    assert_eq!(completions.len(), 1);
    assert_eq!(completions[0].job_id, job);
    assert_eq!(completions[0].output_key, output_key);
    assert_eq!(completions[0].bo_idx, 2);
    assert!(completions[0].fd.is_none());
    assert!(platform.pending_scanout_render_completions.is_empty());
}

#[test]
fn non_picked_mode_lookup_uses_the_carried_connector_handle() {
    let connector = ::drm::control::from_u32(17).expect("non-zero connector handle");
    let protocol_name = "HDMI-1";
    let drm_diagnostic_name = "HDMI-A-1";
    assert_ne!(protocol_name, drm_diagnostic_name);

    let selected = mode_via_connector_handle(
        connector,
        yserver_core::backend::ModeSpec {
            width: 1920,
            height: 1080,
            vrefresh: 60,
        },
        |queried| {
            assert_eq!(queried, connector);
            Ok(vec![(1280_u16, 720_u16, 60_u32), (1920, 1080, 60)])
        },
        |mode| *mode,
    )
    .expect("connector query should succeed");

    assert_eq!(selected, Some((1920, 1080, 60)));
}

#[test]
fn connector_handle_mode_lookup_preserves_query_error_kind() {
    let connector = ::drm::control::from_u32(17).expect("non-zero connector handle");
    let error = mode_via_connector_handle::<(u16, u16, u32)>(
        connector,
        yserver_core::backend::ModeSpec {
            width: 1920,
            height: 1080,
            vrefresh: 60,
        },
        |_| {
            Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "injected query failure",
            ))
        },
        |mode| *mode,
    )
    .expect_err("query failure must propagate");

    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
}

#[test]
fn equal_raw_connector_handles_stay_scoped_to_the_output_device_key() {
    let mut platform = PlatformBackend::for_tests();
    let first_key = platform.devices[0].key;
    let second_key = drm_key(7);
    platform.devices.push(test_kms_device(second_key));

    let first = test_active_output_for(first_key, "HDMI-1", 17);
    let second = test_active_output_for(second_key, "HDMI-1", 17);
    assert_eq!(first.output.connector, second.output.connector);
    assert_eq!(
        platform
            .device_for_output(&first.key)
            .expect("first output device")
            .key,
        first_key
    );
    assert_eq!(
        platform
            .device_for_output(&second.key)
            .expect("second output device")
            .key,
        second_key
    );
}

#[test]
fn verified_render_device_accepts_its_advertised_node() {
    assert!(
        validate_render_node_attachment(RenderDeviceId::DrmRender(drm_key(128)), drm_key(128))
            .is_ok()
    );
}

#[test]
fn verified_render_device_rejects_a_different_opened_node() {
    let error =
        validate_render_node_attachment(RenderDeviceId::DrmRender(drm_key(128)), drm_key(129))
            .expect_err("a verified renderer must not acquire another node's resources");
    assert!(error.to_string().contains("226:128"));
    assert!(error.to_string().contains("226:129"));
}

#[test]
fn unverified_fallback_can_attach_the_resolved_node_without_claiming_identity() {
    assert!(
        validate_render_node_attachment(RenderDeviceId::UnverifiedFallback, drm_key(129)).is_ok()
    );
}

fn test_kms_device(key: crate::platform::drm::DrmDeviceKey) -> KmsDevice {
    KmsDevice {
        key,
        device: Rc::new(drm::Device::for_tests().expect("test DRM device")),
        cursor: KmsCursorState::new(),
    }
}

#[test]
fn renderer_primary_relationship_preserves_same_different_and_unknown() {
    let kms = test_kms_device(drm_key(0));
    let renderer = |id, primary| RenderDevice {
        id,
        physical_device: vk::PhysicalDevice::default(),
        selector: VulkanDeviceSelector::for_tests(1),
        advertised_primary_node: primary,
        advertised_render_node: None,
        render_node: None,
        render_node_device: None,
        syncobj_timeline: false,
    };

    assert_eq!(
        renderer(RenderDeviceId::UnverifiedFallback, None).relationship_to(&kms),
        RenderKmsRelationship::Unknown
    );
    assert_eq!(
        renderer(RenderDeviceId::UnverifiedFallback, Some(drm_key(0))).relationship_to(&kms),
        RenderKmsRelationship::Same
    );
    assert_eq!(
        renderer(RenderDeviceId::UnverifiedFallback, Some(drm_key(1))).relationship_to(&kms),
        RenderKmsRelationship::Different
    );
    assert_eq!(
        renderer(RenderDeviceId::DrmRender(drm_key(128)), Some(drm_key(0))).relationship_to(&kms),
        RenderKmsRelationship::Same,
        "same-device detection compares the advertised primary node, not the render node"
    );
    assert_eq!(
        renderer(RenderDeviceId::DrmRender(drm_key(128)), Some(drm_key(1))).relationship_to(&kms),
        RenderKmsRelationship::Different
    );
}

#[test]
fn split_gpu_route_keeps_renderer_and_kms_endpoint_identities() {
    let kms = test_kms_device(drm_key(0));
    let renderer = RenderDevice {
        id: RenderDeviceId::DrmRender(drm_key(128)),
        physical_device: vk::PhysicalDevice::default(),
        selector: VulkanDeviceSelector::for_tests(1),
        advertised_primary_node: Some(drm_key(1)),
        advertised_render_node: Some(drm_key(128)),
        render_node: None,
        render_node_device: None,
        syncobj_timeline: false,
    };

    assert_eq!(
        renderer.scanout_route_to(&kms),
        ScanoutRoute::new(
            RenderDeviceId::DrmRender(drm_key(128)),
            drm_key(0),
            RenderKmsRelationship::Different,
        ),
        "an Asahi-style split route is valid and must retain both endpoints"
    );
}

#[test]
fn every_non_same_route_requires_real_copy_free_probing() {
    let render = RenderDeviceId::DrmRender(drm_key(128));
    let kms = drm_key(0);
    assert!(!route_requires_copy_free_probe(ScanoutRoute::new(
        render,
        kms,
        RenderKmsRelationship::Same,
    )));
    assert!(route_requires_copy_free_probe(ScanoutRoute::new(
        render,
        kms,
        RenderKmsRelationship::Different,
    )));
    assert!(route_requires_copy_free_probe(ScanoutRoute::new(
        render,
        kms,
        RenderKmsRelationship::Unknown,
    )));
}

#[test]
fn copy_free_candidate_diagnostics_name_the_exact_plan_and_stage() {
    let diagnostic = copy_free_candidate_error(
        ScanoutAllocationPlan::LegacyLinear,
        "probe rendering",
        &io::Error::other("synthetic device loss"),
    );
    assert_eq!(
        diagnostic,
        "legacy-linear probe rendering: synthetic device loss"
    );
}

#[test]
fn route_or_pool_mismatch_reallocates_a_same_size_scanout_pool() {
    let platform = PlatformBackend::for_tests();
    let output = &platform.outputs[0];
    assert!(!scanout_pool_needs_reallocation(
        Some(output),
        Some(output.scanout_route),
        output.width,
        output.height,
        output.scanout_route,
    ));

    let changed_route = ScanoutRoute::new(
        RenderDeviceId::DrmRender(drm_key(128)),
        output.key.device_key,
        RenderKmsRelationship::Unknown,
    );
    assert!(scanout_pool_needs_reallocation(
        Some(output),
        Some(output.scanout_route),
        output.width,
        output.height,
        changed_route,
    ));
    assert!(scanout_pool_needs_reallocation(
        Some(output),
        Some(changed_route),
        output.width,
        output.height,
        output.scanout_route,
    ));
    assert!(scanout_pool_needs_reallocation(
        Some(output),
        None,
        output.width,
        output.height,
        output.scanout_route,
    ));
}

fn install_test_cursor_plane(
    device: &mut KmsDevice,
    crtcs: &[::drm::control::crtc::Handle],
    boundary: &str,
) {
    let plane =
        crate::kms::cursor_plane::CursorPlane::for_tests_stub(Rc::clone(&device.device), 64, 64);
    install_cursor_plane_for_device(device, crtcs, boundary, plane);
}

fn test_active_output_for(
    device_key: crate::platform::drm::DrmDeviceKey,
    connector_name: &str,
    raw_crtc: u32,
) -> ActiveOutput {
    let mut seed = PlatformBackend::for_tests();
    let mut output = seed.outputs.remove(0);
    output.key = OutputKey::new(device_key, connector_name);
    output.scanout_route = ScanoutRoute::new(
        output.scanout_route.render_device_id,
        device_key,
        RenderKmsRelationship::Unknown,
    );
    output.output.connector_name = connector_name.to_string();
    output.output.connector = ::drm::control::from_u32(raw_crtc).unwrap();
    output.output.encoder = ::drm::control::from_u32(raw_crtc).unwrap();
    output.output.crtc = ::drm::control::from_u32(raw_crtc).unwrap();
    output.output.plane = ::drm::control::from_u32(raw_crtc).unwrap();
    output
}

#[test]
fn unqualified_initial_scanout_rollback_guard_fires_and_can_be_disarmed() {
    let mut platform = PlatformBackend::for_tests();
    let active = platform.outputs.remove(0);
    let mut initial_outputs = [PlatformInitOutput {
        key: active.key,
        output: active.output,
        swapchain: active.swapchain,
        x: active.x,
        y: active.y,
        width: active.width,
        height: active.height,
    }];
    let calls = Rc::new(Cell::new(0_u32));

    {
        let calls = Rc::clone(&calls);
        let _guard = InitialScanoutRollbackGuard::new_with(
            &platform.devices,
            &mut initial_outputs,
            move |_device, _output| {
                calls.set(calls.get() + 1);
                Ok(())
            },
        );
    }
    assert_eq!(calls.get(), 1, "armed guard must run rollback on drop");

    {
        let calls = Rc::clone(&calls);
        let mut guard = InitialScanoutRollbackGuard::new_with(
            &platform.devices,
            &mut initial_outputs,
            move |_device, _output| {
                calls.set(calls.get() + 1);
                Ok(())
            },
        );
        guard.disarm();
    }
    assert_eq!(calls.get(), 1, "disarmed guard must not roll back");

    let route = ScanoutRoute::new(
        RenderDeviceId::DrmRender(drm_key(128)),
        initial_outputs[0].key.device_key,
        RenderKmsRelationship::Unknown,
    );
    let qualified = initial_outputs
        .into_iter()
        .next()
        .expect("one init output")
        .qualify(route);
    assert_eq!(qualified.scanout_route, route);
    assert_eq!(qualified.key.device_key, route.kms_device_key);
}

#[test]
fn zero_device_platform_has_no_drm_poll_source_or_rescan_work() {
    let mut platform = PlatformBackend::for_tests();
    platform.devices.clear();
    platform.outputs.clear();

    assert!(platform.primary_device().is_none());
    assert!(
        platform
            .poll_fds()
            .iter()
            .all(|(_, kind)| !matches!(kind, BackendFdKind::Drm)),
        "a zero-device platform must not register a DRM poll source"
    );

    let snapshot = platform
        .probe_connector_snapshot()
        .expect("zero-device probe is an empty no-op");
    let rescan = platform.apply_connector_snapshot(snapshot, &HashSet::new());
    assert!(rescan.added_keys.is_empty());
    assert!(rescan.dropped_keys.is_empty());
    assert!(rescan.dropped_old_indices.is_empty());
    assert_eq!(rescan.added_count, 0);
}

#[test]
fn old_event_drain_accepts_an_empty_zero_device_epoch() {
    let mut platform = PlatformBackend::for_tests();
    platform.devices.clear();

    platform
        .discard_old_drm_events_after_all_off(&HashSet::new(), std::time::Duration::ZERO)
        .expect("an empty topology epoch has no DRM event to retire");
}

#[test]
fn old_event_drain_rejects_pending_work_without_an_owning_device() {
    let mut platform = PlatformBackend::for_tests();
    let pending = HashSet::from([CrtcKey::for_output(&platform.outputs[0])]);
    platform.devices.clear();

    let error = platform
        .discard_old_drm_events_after_all_off(&pending, std::time::Duration::ZERO)
        .expect_err("pending work cannot be proven retired without its DRM fd");
    assert!(matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::NotFound
    ));
}

#[test]
fn connector_snapshot_refreshes_live_metadata_without_changing_its_route() {
    let mut platform = PlatformBackend::for_tests();
    let key = platform.outputs[0].key.clone();
    let old_crtc = platform.outputs[0].output.crtc;
    let old_plane = platform.outputs[0].output.plane;
    let snapshot = ConnectorSnapshot {
        key: key.clone(),
        modes: vec![
            crate::platform::drm::Mode {
                name: "800x600".into(),
                width: 800,
                height: 600,
                vrefresh: 60,
                preferred: true,
                ..Default::default()
            },
            crate::platform::drm::Mode {
                name: "1024x768".into(),
                width: 1024,
                height: 768,
                vrefresh: 75,
                preferred: false,
                ..Default::default()
            },
        ],
        mm_width: 520,
        mm_height: 290,
        edid: vec![1, 2, 3, 4],
        connector_type: "DisplayPort".into(),
    };
    let known_connected = HashSet::from([key]);

    let rescan = platform.apply_connector_snapshot(vec![snapshot], &known_connected);

    assert!(rescan.dropped_old_indices.is_empty());
    let output = &platform.outputs[0].output;
    assert_eq!(output.crtc, old_crtc);
    assert_eq!(output.plane, old_plane);
    assert_eq!((output.mm_width, output.mm_height), (520, 290));
    assert_eq!(output.edid, vec![1, 2, 3, 4]);
    assert_eq!(output.connector_type, "DisplayPort");
    assert_eq!(output.modes[1].vrefresh, 75);
}

#[test]
fn metadata_only_snapshot_preserves_active_layout_and_extent() {
    let mut platform = PlatformBackend::for_tests();
    let key = platform.outputs[0].key.clone();
    platform.outputs[0].x = 123;
    platform.outputs[0].y = 45;
    platform.fb_w = 923;
    platform.fb_h = 645;
    let snapshot = ConnectorSnapshot {
        key: key.clone(),
        modes: platform.outputs[0].output.modes.clone(),
        mm_width: 520,
        mm_height: 290,
        edid: vec![1, 2, 3, 4],
        connector_type: "DisplayPort".into(),
    };

    let rescan = platform.apply_connector_snapshot(vec![snapshot], &HashSet::from([key]));

    assert!(rescan.dropped_old_indices.is_empty());
    assert_eq!((platform.outputs[0].x, platform.outputs[0].y), (123, 45));
    assert_eq!(platform.fb_dimensions(), (923, 645));
}

/// Place a live output on the fixture's device at `(x, y)` with a
/// `width x height` mode, and extend the parallel per-output vectors so
/// the snapshot's index bookkeeping stays in step.
fn push_placed_test_output(
    platform: &mut PlatformBackend,
    connector_name: &str,
    raw_crtc: u32,
    x: i32,
    y: i32,
    width: u16,
    height: u16,
) -> OutputKey {
    let device_key = platform.devices[0].key;
    let mut output = test_active_output_for(device_key, connector_name, raw_crtc);
    let mode = crate::platform::drm::Mode {
        name: format!("{width}x{height}"),
        width,
        height,
        vrefresh: 60,
        preferred: true,
        ..Default::default()
    };
    output.output.picked = mode.clone();
    output.output.modes = vec![mode];
    output.x = x;
    output.y = y;
    output.width = width;
    output.height = height;
    let key = output.key.clone();
    platform.outputs.push(output);
    platform.scanout_pools.push(None);
    platform.bo_generations.push(Vec::new());
    platform.first_pageflip_logged.push(false);
    key
}

fn clear_test_outputs(platform: &mut PlatformBackend) {
    platform.outputs.clear();
    platform.scanout_pools.clear();
    platform.bo_generations.clear();
    platform.first_pageflip_logged.clear();
}

#[test]
fn connector_snapshot_reports_dropped_rectangles_and_leaves_layout_to_the_caller() {
    let mut platform = PlatformBackend::for_tests();
    // Put the sole route somewhere the old in-snapshot compaction would
    // have flattened, and pin an extent the old in-snapshot recompute
    // would have shrunk. Both are the caller's job now.
    platform.outputs[0].x = 1000;
    platform.outputs[0].y = 0;
    platform.fb_w = 1800;
    platform.fb_h = 600;
    let key = platform.outputs[0].key.clone();

    let rescan = platform.apply_connector_snapshot(Vec::new(), &HashSet::from([key.clone()]));

    assert!(platform.outputs.is_empty());
    assert_eq!(
        rescan.dropped_layouts,
        vec![DroppedRoute {
            key,
            x: 1000,
            y: 0,
            width: 800,
            height: 600,
            vrefresh: 60,
        }],
        "the snapshot reports the rectangle it destroyed",
    );
    assert_eq!(
        platform.fb_dimensions(),
        (1800, 600),
        "the snapshot no longer recomputes the extent",
    );
}

#[test]
fn recompaction_packs_survivors_past_a_reserved_slot() {
    let mut platform = PlatformBackend::for_tests();
    clear_test_outputs(&mut platform);
    push_placed_test_output(&mut platform, "B", 2, 1920, 0, 3200, 1440);
    // The slot a departed-but-restorable 1920x1080 route still holds.
    let reserved = vec![(0, 0, 1920u16, 1080u16)];

    platform.recompact_horizontal_layout(&HashSet::new(), &reserved);
    platform.recompute_fb_extent_with_reservations(&reserved);

    assert_eq!(
        (platform.outputs[0].x, platform.outputs[0].y),
        (1920, 0),
        "an auto-layout survivor must not pack into a reserved slot",
    );
    assert_eq!(
        platform.fb_dimensions(),
        (5120, 1440),
        "the extent unions the reserved slot, so it does not shrink",
    );
}

#[test]
fn recompaction_without_a_reservation_still_packs_survivors() {
    let mut platform = PlatformBackend::for_tests();
    clear_test_outputs(&mut platform);
    push_placed_test_output(&mut platform, "B", 2, 1920, 0, 3200, 1440);

    platform.recompact_horizontal_layout(&HashSet::new(), &[]);
    platform.recompute_fb_extent_with_reservations(&[]);

    assert_eq!(
        (platform.outputs[0].x, platform.outputs[0].y),
        (0, 0),
        "dropping a never-enabled route reserves nothing, so survivors compact",
    );
    assert_eq!(platform.fb_dimensions(), (3200, 1440));
}

#[test]
fn a_reserved_slot_does_not_move_a_client_positioned_output() {
    let mut platform = PlatformBackend::for_tests();
    clear_test_outputs(&mut platform);
    let pinned = push_placed_test_output(&mut platform, "B", 2, 4000, 0, 1024, 768);
    let reserved = vec![(0, 0, 1920u16, 1080u16)];

    platform.recompact_horizontal_layout(&HashSet::from([pinned]), &reserved);
    platform.recompute_fb_extent_with_reservations(&reserved);

    assert_eq!((platform.outputs[0].x, platform.outputs[0].y), (4000, 0));
    assert_eq!(platform.fb_dimensions(), (5024, 1080));
}

#[test]
fn packing_steps_past_every_reservation_it_straddles() {
    // Stepping past one reservation can push the placement into the next,
    // so the skip has to repeat rather than fire once.
    assert_eq!(
        super::advance_past_reservations(0, 1000, &[(0, 0, 500, 100), (500, 0, 500, 100)]),
        1000,
    );
    assert_eq!(
        super::advance_past_reservations(0, 100, &[(100, 0, 500, 100)]),
        0,
        "a reservation the placement does not reach must not move it",
    );
    assert_eq!(
        super::advance_past_reservations(0, 100, &[(0, 0, 0, 100)]),
        0,
        "a zero-width reservation blocks nothing",
    );
}

#[test]
fn connector_snapshot_preserves_a_live_route_across_mode_list_replacement() {
    let mut platform = PlatformBackend::for_tests();
    let key = platform.outputs[0].key.clone();
    let snapshot = ConnectorSnapshot {
        key: key.clone(),
        modes: vec![crate::platform::drm::Mode {
            name: "1024x768".into(),
            width: 1024,
            height: 768,
            vrefresh: 75,
            preferred: true,
            ..Default::default()
        }],
        mm_width: 520,
        mm_height: 290,
        edid: vec![1, 2, 3, 4],
        connector_type: "DisplayPort".into(),
    };

    let rescan = platform.apply_connector_snapshot(vec![snapshot], &HashSet::from([key.clone()]));

    assert_eq!(platform.outputs.len(), 1);
    assert!(rescan.dropped_old_indices.is_empty());
    assert!(rescan.dropped_keys.is_empty());
    assert_eq!(
        rescan.connected.len(),
        1,
        "the connector remains physically connected"
    );
}

#[test]
fn crtc_identity_and_present_clocks_include_the_drm_device() {
    let mut platform = PlatformBackend::for_tests();
    let live = CrtcKey::for_output(&platform.outputs[0]);
    let colliding = CrtcKey::new(
        crate::platform::drm::DrmDeviceKey {
            major: live.device_key.major,
            minor: live.device_key.minor + 1,
        },
        live.crtc,
    );

    assert_ne!(live, colliding);
    assert_eq!(platform.output_index_for_crtc(live), Some(0));
    assert_eq!(platform.output_index_for_crtc(colliding), None);

    for key in [live, colliding] {
        platform.ust_msc.insert(key, (7, 11));
        platform.completion_clocks.insert(
            key,
            PresentClockSample {
                msc: 7,
                ust: 11,
                source: PresentClockSource::PageFlip,
            },
        );
        platform.software_msc.insert(key, 7);
    }

    platform.prune_present_clocks_to_live_outputs();
    assert_eq!(platform.ust_msc.keys().copied().collect::<Vec<_>>(), [live]);
    assert_eq!(
        platform
            .completion_clocks
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        [live]
    );
    assert_eq!(
        platform.software_msc.keys().copied().collect::<Vec<_>>(),
        [live]
    );
}

#[test]
fn cursor_route_qualifies_colliding_raw_crtcs_by_device() {
    let mut platform = PlatformBackend::for_tests();
    let raw_crtc = platform.outputs[0].output.crtc;
    let second_key = crate::platform::drm::DrmDeviceKey {
        major: 226,
        minor: 9,
    };
    platform.devices.push(KmsDevice {
        key: second_key,
        device: Rc::new(drm::Device::for_tests().expect("second test DRM device")),
        cursor: KmsCursorState::new(),
    });
    platform.outputs[0].key.device_key = second_key;

    assert_eq!(platform.outputs[0].output.crtc, raw_crtc);
    assert_eq!(platform.cursor_output_route(0).unwrap().0, 1);
}

#[test]
fn drm_device_fd_lookup_distinguishes_same_kind_poll_sources() {
    let mut platform = PlatformBackend::for_tests();
    let first_fd = platform.devices[0].device.as_fd().as_raw_fd();
    let second_device = Rc::new(drm::Device::for_tests().expect("second test DRM device"));
    let second_fd = second_device.as_fd().as_raw_fd();
    platform.devices.push(KmsDevice {
        key: crate::platform::drm::DrmDeviceKey {
            major: 226,
            minor: 1,
        },
        device: second_device,
        cursor: KmsCursorState::new(),
    });

    assert_ne!(first_fd, second_fd);
    assert_eq!(platform.drm_device_index_for_fd(first_fd), Some(0));
    assert_eq!(platform.drm_device_index_for_fd(second_fd), Some(1));
    assert_eq!(platform.drm_device_index_for_fd(-1), None);
}

/// A connected output that yielded no scanout pool must abort
/// bring-up rather than run invisibly (the RPi 4/400 split-GPU
/// black-screen case).
#[test]
fn one_output_no_live_pool_refuses() {
    let err = check_scanout_liveness(1, 0, &["output 0 (1360x768): boom".to_string()])
        .expect_err("1 output, 0 live pools must refuse");
    assert!(err.contains("no displayable output"), "message: {err}");
    assert!(
        err.contains("boom"),
        "must surface the per-output error: {err}"
    );
}

/// A working output proceeds.
#[test]
fn one_output_one_live_pool_proceeds() {
    assert!(check_scanout_liveness(1, 1, &[]).is_ok());
}

/// Partial success (one of two outputs live) proceeds on the good one.
#[test]
fn partial_liveness_proceeds() {
    assert!(check_scanout_liveness(2, 1, &["output 1: nope".to_string()]).is_ok());
}

/// Zero outputs is a headless start, not a failure — runtime hotplug
/// may attach a display later.
#[test]
fn zero_outputs_is_not_fatal() {
    assert!(check_scanout_liveness(0, 0, &[]).is_ok());
}

/// HW-cursor auto-fallback policy: an ioctl error that means the
/// driver doesn't implement the (legacy) cursor ioctls must latch
/// the strategy off so the scene falls back to the SW cursor.
/// Apple's DCP driver (Asahi) returns `ENXIO`; some drivers return
/// `ENODEV` / `EOPNOTSUPP`. Recoverable errors (`EBUSY`) must NOT
/// latch — those are transient and latching would needlessly kill
/// the HW cursor on drivers that DO support it (e.g. amdgpu).
#[test]
fn cursor_err_disables_hw_only_for_unsupported_errnos() {
    use std::io::Error;
    // Asahi / Apple DCP: legacy cursor ioctl unimplemented.
    assert!(cursor_err_disables_hw(&Error::from_raw_os_error(
        libc::ENXIO
    )));
    assert!(cursor_err_disables_hw(&Error::from_raw_os_error(
        libc::ENODEV
    )));
    assert!(cursor_err_disables_hw(&Error::from_raw_os_error(
        libc::EOPNOTSUPP
    )));
    // Transient / recoverable: must keep the HW path alive.
    assert!(!cursor_err_disables_hw(&Error::from_raw_os_error(
        libc::EBUSY
    )));
    // Generic / ambiguous: don't latch on these either.
    assert!(!cursor_err_disables_hw(&Error::from_raw_os_error(
        libc::EINVAL
    )));
    assert!(!cursor_err_disables_hw(&Error::other("not an os error")));
}

#[test]
fn cursor_failure_pair_uses_permanent_precedence_and_clears_pending() {
    let crtc = ::drm::control::from_u32(17).unwrap();

    let mut transient = KmsCursorState::new();
    transient.pending_move = Some((1, 2, 3, 4));
    assert_eq!(
        transient.note_cursor_failure_pair(
            crtc,
            &io::Error::from_raw_os_error(libc::EINVAL),
            Some(&io::Error::from_raw_os_error(libc::EIO)),
        ),
        CursorFailureDisposition::Transient
    );
    assert!(!transient.permanently_disabled);
    assert_eq!(transient.pending_move, None);
    assert_eq!(
        transient.transient_fallback_crtcs[&crtc].remaining_sw_retires, 1,
        "one operation+rollback pair records exactly one failure"
    );

    for (operation_errno, rollback_errno) in
        [(libc::EINVAL, libc::ENODEV), (libc::ENODEV, libc::EINVAL)]
    {
        let mut permanent = KmsCursorState::new();
        permanent.pending_move = Some((1, 2, 3, 4));
        assert_eq!(
            permanent.note_cursor_failure_pair(
                crtc,
                &io::Error::from_raw_os_error(operation_errno),
                Some(&io::Error::from_raw_os_error(rollback_errno)),
            ),
            CursorFailureDisposition::Permanent
        );
        assert!(permanent.permanently_disabled);
        assert_eq!(permanent.pending_move, None);
        assert!(permanent.transient_fallback_crtcs.is_empty());
    }

    let mut unchanged = KmsCursorState::new();
    unchanged.pending_move = Some((1, 2, 3, 4));
    assert_eq!(
        unchanged.note_cursor_failure_pair(
            crtc,
            &io::Error::from_raw_os_error(libc::EBUSY),
            Some(&io::Error::from_raw_os_error(libc::EIO)),
        ),
        CursorFailureDisposition::Unchanged
    );
    assert_eq!(unchanged.pending_move, Some((1, 2, 3, 4)));
    assert!(unchanged.transient_fallback_crtcs.is_empty());
}

#[test]
fn still_visible_show_failure_records_owning_fallback_without_stale_move() {
    let crtc = ::drm::control::from_u32(18).unwrap();
    let mut transient = KmsCursorState::new();
    transient.pending_move = Some((1, 2, 3, 4));
    let bind_einval = crate::kms::cursor_plane::CursorShowError::StillVisible {
        operation_error: io::Error::from_raw_os_error(libc::EINVAL),
        rollback_error: None,
    };
    assert_eq!(
        apply_cursor_show_failure_state(&mut transient, crtc, &bind_einval, (10, 20, 5, 6),),
        CursorFailureDisposition::Transient
    );
    assert_eq!(transient.pending_move, None);
    assert_eq!(
        transient.transient_fallback_crtcs[&crtc].remaining_sw_retires,
        1
    );

    let mut unsupported = KmsCursorState::new();
    let bind_enodev = crate::kms::cursor_plane::CursorShowError::StillVisible {
        operation_error: io::Error::from_raw_os_error(libc::ENODEV),
        rollback_error: None,
    };
    assert_eq!(
        apply_cursor_show_failure_state(&mut unsupported, crtc, &bind_enodev, (10, 20, 5, 6),),
        CursorFailureDisposition::Permanent
    );
    assert!(unsupported.permanently_disabled);

    let mut rollback_wins = KmsCursorState::new();
    let move_einval_hide_enodev = crate::kms::cursor_plane::CursorShowError::StillVisible {
        operation_error: io::Error::from_raw_os_error(libc::EINVAL),
        rollback_error: Some(io::Error::from_raw_os_error(libc::ENODEV)),
    };
    assert_eq!(
        apply_cursor_show_failure_state(
            &mut rollback_wins,
            crtc,
            &move_einval_hide_enodev,
            (10, 20, 5, 6),
        ),
        CursorFailureDisposition::Permanent
    );
    assert!(rollback_wins.permanently_disabled);
    assert!(rollback_wins.transient_fallback_crtcs.is_empty());
    assert_eq!(rollback_wins.pending_move, None);
}

#[test]
fn hide_failure_classification_is_bounded_and_success_does_not_clear_it() {
    let crtc = ::drm::control::from_u32(19).unwrap();
    let mut state = KmsCursorState::new();
    let einval = Err(io::Error::from_raw_os_error(libc::EINVAL));
    assert_eq!(
        apply_cursor_operation_result(&mut state, crtc, &einval),
        CursorFailureDisposition::Transient
    );
    assert_eq!(
        state.transient_fallback_crtcs[&crtc].remaining_sw_retires,
        1
    );
    assert_eq!(
        apply_cursor_operation_result(&mut state, crtc, &einval),
        CursorFailureDisposition::Transient
    );
    assert_eq!(
        state.transient_fallback_crtcs[&crtc].remaining_sw_retires,
        2
    );

    assert_eq!(
        apply_cursor_operation_result(&mut state, crtc, &Ok(())),
        CursorFailureDisposition::Unchanged
    );
    assert_eq!(
        state.transient_fallback_crtcs[&crtc].remaining_sw_retires, 2,
        "a successful hide is not proof that a later full Show is valid"
    );

    let other_crtc = ::drm::control::from_u32(20).unwrap();
    assert_eq!(
        apply_cursor_operation_result(
            &mut state,
            other_crtc,
            &Err(io::Error::from_raw_os_error(libc::EBUSY)),
        ),
        CursorFailureDisposition::Unchanged
    );
    assert!(!state.transient_fallback_crtcs.contains_key(&other_crtc));

    assert_eq!(
        apply_cursor_operation_result(
            &mut state,
            crtc,
            &Err(io::Error::from_raw_os_error(libc::ENODEV)),
        ),
        CursorFailureDisposition::Permanent
    );
    assert!(state.permanently_disabled);
    assert!(state.transient_fallback_crtcs.is_empty());
}

#[test]
fn actual_upload_invalid_input_enters_one_bounded_local_fallback() {
    let crtc = ::drm::control::from_u32(21).unwrap();
    let mut state = KmsCursorState::new();
    let upload = Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "cursor bytes shorter than width*height*4",
    ));
    assert_eq!(
        apply_cursor_operation_result(&mut state, crtc, &upload),
        CursorFailureDisposition::Transient
    );
    assert_eq!(state.transient_fallback_crtcs.len(), 1);
    assert_eq!(
        state.transient_fallback_crtcs[&crtc].remaining_sw_retires,
        1
    );
    assert!(!state.permanently_disabled);
}

#[test]
fn cursor_failure_classification_isolated_across_cards_with_same_raw_crtc() {
    let crtc = ::drm::control::from_u32(22).unwrap();
    let mut card_a = KmsCursorState::new();
    let card_b = KmsCursorState::new();
    card_a.note_cursor_failure_pair(crtc, &io::Error::from_raw_os_error(libc::EINVAL), None);
    assert!(card_a.transient_fallback_crtcs.contains_key(&crtc));
    assert!(card_b.transient_fallback_crtcs.is_empty());
    assert!(!card_b.permanently_disabled);

    card_a.note_cursor_failure_pair(crtc, &io::Error::from_raw_os_error(libc::ENODEV), None);
    assert!(card_a.permanently_disabled);
    assert!(card_a.transient_fallback_crtcs.is_empty());
    assert!(card_b.transient_fallback_crtcs.is_empty());
    assert!(!card_b.permanently_disabled);
}

#[test]
fn active_startup_transient_init_retries_only_at_explicit_active_boundary() {
    let mut state = KmsCursorState::new();
    assert!(
        !state.should_retry_initialization(true),
        "a genuinely headless-deferred device is not a lifecycle retry"
    );
    assert!(state.should_initialize_headless_deferred(true));

    state.note_initialization_failure(&io::Error::from_raw_os_error(libc::ENOMEM));
    assert!(!state.headless_deferred);
    assert!(!state.permanently_disabled);
    assert!(!state.should_retry_initialization(false));
    assert!(state.should_retry_initialization(true));

    state.note_initialization_failure(&io::Error::from_raw_os_error(libc::ENODEV));
    assert!(state.permanently_disabled);
    assert!(!state.should_retry_initialization(true));
}

#[test]
fn primary_first_explicit_enable_initializes_and_exposes_fresh_upload_state() {
    let mut platform = PlatformBackend::for_tests();
    let key = platform.devices[0].key;
    let crtc = platform.outputs[0].output.crtc;
    assert!(platform.devices[0].cursor.headless_deferred);

    let calls = Cell::new(0_u32);
    assert!(platform.initialize_headless_cursor_for_device_with(
        key,
        "test primary first enable",
        |device, crtcs, boundary| {
            calls.set(calls.get() + 1);
            assert_eq!(device.key, key);
            assert_eq!(crtcs, &[crtc]);
            assert_eq!(boundary, "test primary first enable");
            install_test_cursor_plane(device, crtcs, boundary);
        },
    ));
    assert_eq!(calls.get(), 1);
    assert!(!platform.devices[0].cursor.headless_deferred);
    assert!(platform.cursor_plane_available_for_output(0));
    assert_eq!(platform.cursor_plane_uploaded_version_for_output(0), None);

    let bytes = vec![0_u8; 16 * 16 * 4];
    platform
        .cursor_plane_upload_image_for_output(0, 7, 16, 16, &bytes)
        .expect("fresh lazy plane accepts its first retire-time upload");
    assert_eq!(
        platform.cursor_plane_uploaded_version_for_output(0),
        Some(7)
    );
}

#[test]
fn secondary_first_enable_uses_owning_device_despite_raw_crtc_collision() {
    let mut platform = PlatformBackend::for_tests();
    let primary_key = platform.devices[0].key;
    let raw_crtc = platform.outputs[0].output.crtc;
    let secondary_key = crate::platform::drm::DrmDeviceKey {
        major: 226,
        minor: 93,
    };
    platform.devices.push(test_kms_device(secondary_key));
    let primary_fd = platform.devices[0].device.as_fd().as_raw_fd();
    let secondary_fd = platform.devices[1].device.as_fd().as_raw_fd();
    platform.outputs.push(test_active_output_for(
        secondary_key,
        "secondary",
        u32::from(raw_crtc),
    ));

    let calls = Cell::new(0_u32);
    assert!(platform.initialize_headless_cursor_for_device_with(
        secondary_key,
        "test secondary first enable",
        |device, crtcs, boundary| {
            calls.set(calls.get() + 1);
            assert_eq!(device.key, secondary_key);
            assert_eq!(device.device.as_fd().as_raw_fd(), secondary_fd);
            assert_ne!(device.device.as_fd().as_raw_fd(), primary_fd);
            assert_eq!(crtcs, &[raw_crtc]);
            install_test_cursor_plane(device, crtcs, boundary);
        },
    ));

    assert_eq!(calls.get(), 1);
    assert_eq!(platform.devices[0].key, primary_key);
    assert!(platform.devices[0].cursor.plane.is_none());
    assert!(platform.devices[0].cursor.headless_deferred);
    assert!(platform.devices[1].cursor.plane.is_some());
    assert!(platform.cursor_plane_available_for_output(1));
    assert!(!platform.cursor_plane_available_for_output(0));
}

#[test]
fn failed_enable_never_reaches_deferred_cursor_factory() {
    let mut platform = PlatformBackend::for_tests();
    let active = platform.outputs.remove(0);
    let output_key = active.key;
    let output = active.output;
    platform.scanout_pools.clear();
    platform.bo_generations.clear();
    platform.first_pageflip_logged.clear();
    let mode = yserver_core::backend::ModeSpec {
        width: output.picked.width,
        height: output.picked.height,
        vrefresh: output.picked.vrefresh,
    };
    let calls = Cell::new(0_u32);

    let result = platform.enable_connector_with_cursor_factory(
        &output_key,
        output,
        mode,
        0,
        0,
        |_device, _crtcs, _boundary| calls.set(calls.get() + 1),
    );

    assert!(result.is_err(), "test fixture has no initial fb handle");
    assert_eq!(calls.get(), 0);
    assert!(platform.outputs.is_empty());
    assert!(platform.devices[0].cursor.headless_deferred);
}

#[test]
fn deferred_init_failure_policy_is_device_local_and_retries_later_only() {
    let mut platform = PlatformBackend::for_tests();
    let primary_key = platform.devices[0].key;
    let secondary_key = crate::platform::drm::DrmDeviceKey {
        major: 226,
        minor: 94,
    };
    platform.devices.push(test_kms_device(secondary_key));
    platform
        .outputs
        .push(test_active_output_for(secondary_key, "secondary", 2));

    let first_calls = Cell::new(0_u32);
    assert!(platform.initialize_headless_cursor_for_device_with(
        secondary_key,
        "first explicit enable",
        |device, _crtcs, _boundary| {
            first_calls.set(first_calls.get() + 1);
            device
                .cursor
                .note_initialization_failure(&io::Error::from_raw_os_error(libc::ENOMEM));
        },
    ));
    assert_eq!(first_calls.get(), 1);
    assert!(!platform.devices[1].cursor.headless_deferred);
    assert!(platform.devices[1].cursor.initialization_retryable);
    assert!(!platform.devices[1].cursor.permanently_disabled);
    assert!(!platform.initialize_headless_cursor_for_device_with(
        secondary_key,
        "same boundary must not retry",
        |_device, _crtcs, _boundary| first_calls.set(first_calls.get() + 1),
    ));
    assert_eq!(first_calls.get(), 1);

    let retry_calls = Cell::new(0_u32);
    platform.refresh_cursor_topology_for_devices_with(
        &HashSet::from([secondary_key]),
        |device, crtcs, boundary| {
            retry_calls.set(retry_calls.get() + 1);
            assert_eq!(device.key, secondary_key);
            assert_eq!(boundary, "lifecycle retry");
            install_test_cursor_plane(device, crtcs, boundary);
        },
    );
    assert_eq!(retry_calls.get(), 1);
    assert!(platform.devices[1].cursor.plane.is_some());
    assert!(!platform.devices[1].cursor.initialization_retryable);
    assert!(platform.devices[0].cursor.headless_deferred);
    assert!(!platform.devices[0].cursor.permanently_disabled);

    // A separate card's permanent failure latches only that owner.
    let tertiary_key = crate::platform::drm::DrmDeviceKey {
        major: 226,
        minor: 95,
    };
    platform.devices.push(test_kms_device(tertiary_key));
    platform
        .outputs
        .push(test_active_output_for(tertiary_key, "tertiary", 3));
    assert!(platform.initialize_headless_cursor_for_device_with(
        tertiary_key,
        "first explicit enable",
        |device, _crtcs, _boundary| {
            device
                .cursor
                .note_initialization_failure(&io::Error::from_raw_os_error(libc::ENODEV));
        },
    ));
    assert!(platform.devices[2].cursor.permanently_disabled);
    assert!(!platform.devices[2].cursor.initialization_retryable);
    assert!(platform.devices[2].cursor.plane.is_none());
    assert!(platform.devices[1].cursor.plane.is_some());
    assert!(!platform.devices[1].cursor.permanently_disabled);
    assert_eq!(platform.devices[0].key, primary_key);
}

#[test]
fn initialized_cursor_plane_persists_but_reuploads_across_last_disable_and_reenable() {
    let mut platform = PlatformBackend::for_tests();
    let key = platform.devices[0].key;
    assert!(platform.initialize_headless_cursor_for_device_with(
        key,
        "first explicit enable",
        install_test_cursor_plane,
    ));
    platform
        .cursor_plane_upload_image_for_output(0, 9, 16, 16, &[0_u8; 16 * 16 * 4])
        .unwrap();
    assert_eq!(
        platform.devices[0]
            .cursor
            .plane
            .as_ref()
            .and_then(crate::kms::cursor_plane::CursorPlane::uploaded_version),
        Some(9)
    );

    platform
        .cursor_plane_hide_all()
        .expect("topology quiesce retains the plane while invalidating its upload");
    assert_eq!(
        platform.devices[0]
            .cursor
            .plane
            .as_ref()
            .and_then(crate::kms::cursor_plane::CursorPlane::uploaded_version),
        None
    );

    platform.remove_connector_at(0);
    assert!(platform.outputs.is_empty());
    assert!(platform.devices[0].cursor.plane.is_some());
    assert!(!platform.devices[0].cursor.headless_deferred);
    assert_eq!(
        platform.devices[0]
            .cursor
            .plane
            .as_ref()
            .and_then(crate::kms::cursor_plane::CursorPlane::uploaded_version),
        None,
        "last-output removal retains the allocation, not stale pixels"
    );

    platform
        .outputs
        .push(test_active_output_for(key, "reenabled", 1));
    platform.refresh_cursor_topology_for_devices(&HashSet::from([key]));
    assert!(platform.cursor_plane_available_for_output(0));
    assert_eq!(platform.cursor_plane_uploaded_version_for_output(0), None);
    platform
        .cursor_plane_upload_image_for_output(0, 10, 16, 16, &[0_u8; 16 * 16 * 4])
        .expect("first retirement after re-enable refreshes retained storage");
    assert_eq!(
        platform.cursor_plane_uploaded_version_for_output(0),
        Some(10)
    );
}

#[test]
fn connected_off_probe_and_zero_card_do_not_run_deferred_factory() {
    let mut platform = PlatformBackend::for_tests();
    let key = platform.outputs[0].key.clone();
    platform.outputs.clear();
    platform.scanout_pools.clear();
    platform.bo_generations.clear();
    platform.first_pageflip_logged.clear();
    let snapshot = ConnectorSnapshot {
        key: key.clone(),
        modes: vec![crate::platform::drm::Mode {
            name: "800x600".into(),
            width: 800,
            height: 600,
            vrefresh: 60,
            preferred: true,
            ..Default::default()
        }],
        mm_width: 520,
        mm_height: 290,
        edid: vec![1, 2, 3, 4],
        connector_type: "DisplayPort".into(),
    };

    let rescan = platform.apply_connector_snapshot(vec![snapshot], &HashSet::new());
    assert_eq!(rescan.added_keys, vec![key.clone()]);
    assert!(platform.outputs.is_empty());
    assert!(platform.devices[0].cursor.headless_deferred);
    assert!(platform.devices[0].cursor.plane.is_none());

    let calls = Cell::new(0_u32);
    platform.devices.clear();
    assert!(!platform.initialize_headless_cursor_for_device_with(
        key.device_key,
        "zero-card",
        |_device, _crtcs, _boundary| calls.set(calls.get() + 1),
    ));
    assert_eq!(calls.get(), 0);
}

#[test]
fn cursor_capacity_is_evaluated_per_card() {
    assert!(cursor_dimensions_fit(128, 128, 96, 96));
    assert!(!cursor_dimensions_fit(64, 64, 96, 96));
}

#[test]
fn nvidia_cursor_policy_follows_the_output_owner() {
    let mut platform = PlatformBackend::for_tests();
    let mesa_key = platform.devices[0].key;
    let nvidia_key = crate::platform::drm::DrmDeviceKey {
        major: 226,
        minor: 77,
    };
    platform.devices.push(KmsDevice {
        key: nvidia_key,
        device: Rc::new(drm::Device::for_tests().expect("test DRM device")),
        cursor: KmsCursorState::new_with_nvidia_policy(true),
    });

    let policy_disabled = |platform: &PlatformBackend| {
        let output = &platform.outputs[0];
        platform
            .device_for_key(output.key.device_key)
            .expect("output owner")
            .cursor
            .nvidia_policy_disabled
    };
    platform.outputs[0].key.device_key = mesa_key;
    assert!(!policy_disabled(&platform));
    platform.outputs[0].key.device_key = nvidia_key;
    assert!(policy_disabled(&platform));

    platform.devices.swap(0, 1);
    assert!(policy_disabled(&platform));
    platform.outputs[0].key.device_key = mesa_key;
    assert!(!policy_disabled(&platform));
}

#[test]
fn topology_refresh_on_card_b_preserves_card_a_cursor_retry_state() {
    let mut platform = PlatformBackend::for_tests();
    let card_a = platform.devices[0].key;
    let raw_crtc = platform.outputs[0].output.crtc;
    let card_b = crate::platform::drm::DrmDeviceKey {
        major: 226,
        minor: 88,
    };
    platform.devices.push(test_kms_device(card_b));
    platform.devices[0].cursor.pending_move = Some((100, 200, 3, 4));
    platform.devices[0].cursor.note_einval(raw_crtc);
    platform.devices[1].cursor.pending_move = Some((300, 400, 5, 6));
    platform.devices[1].cursor.note_einval(raw_crtc);

    platform.refresh_cursor_topology_for_devices(&HashSet::from([card_b]));

    assert_eq!(platform.devices[0].key, card_a);
    assert_eq!(
        platform.devices[0].cursor.pending_move,
        Some((100, 200, 3, 4))
    );
    assert_eq!(
        platform.devices[0]
            .cursor
            .transient_fallback_crtcs
            .get(&raw_crtc)
            .map(|retry| retry.remaining_sw_retires),
        Some(1)
    );
    assert_eq!(platform.devices[1].cursor.pending_move, None);
    assert!(
        platform.devices[1]
            .cursor
            .transient_fallback_crtcs
            .is_empty()
    );
}

#[test]
fn einval_backoff_advances_only_on_owning_output_retirements() {
    let mut platform = PlatformBackend::for_tests();
    let raw_crtc = platform.outputs[0].output.crtc;
    let card_b = crate::platform::drm::DrmDeviceKey {
        major: 226,
        minor: 89,
    };
    platform.devices.push(test_kms_device(card_b));
    platform.devices[0].cursor.note_einval(raw_crtc);
    platform.devices[1].cursor.note_einval(raw_crtc);

    assert!(platform.cursor_plane_note_composed_retirement(0));
    assert_eq!(
        platform.devices[0].cursor.transient_fallback_crtcs[&raw_crtc].remaining_sw_retires,
        0
    );
    assert_eq!(
        platform.devices[1].cursor.transient_fallback_crtcs[&raw_crtc].remaining_sw_retires,
        1
    );

    platform.devices[0].cursor.note_einval(raw_crtc);
    assert_eq!(
        platform.devices[0].cursor.transient_fallback_crtcs[&raw_crtc].remaining_sw_retires, 2,
        "a repeated EINVAL is rate-limited for two own-card SW retirements"
    );
}

#[test]
fn failed_move_hide_rollback_records_fallback_and_scene_owned_retry() {
    let crtc = ::drm::control::from_u32(17).unwrap();
    let mut state = KmsCursorState::new();
    state.pending_move = Some((1, 2, 3, 4));
    let mut outcome = CursorMoveOutcome::default();
    let keep_pending = apply_cursor_move_rollback_result(
        &mut state,
        crtc,
        &io::Error::from_raw_os_error(libc::EINVAL),
        Err(io::Error::from_raw_os_error(libc::EIO)),
        &mut outcome,
    );

    assert!(!keep_pending);
    assert!(outcome.retry_required);
    assert!(outcome.fallback_changed);
    assert!(!state.permanently_disabled);
    assert_eq!(state.pending_move, None);
    assert_eq!(
        state.transient_fallback_crtcs[&crtc].remaining_sw_retires,
        1
    );
}

#[test]
fn failed_move_hide_rollback_uses_permanent_precedence() {
    let crtc = ::drm::control::from_u32(23).unwrap();
    for (move_errno, hide_errno) in [(libc::EINVAL, libc::ENODEV), (libc::ENODEV, libc::EINVAL)] {
        let mut state = KmsCursorState::new();
        state.pending_move = Some((1, 2, 3, 4));
        let mut outcome = CursorMoveOutcome::default();
        let keep_pending = apply_cursor_move_rollback_result(
            &mut state,
            crtc,
            &io::Error::from_raw_os_error(move_errno),
            Err(io::Error::from_raw_os_error(hide_errno)),
            &mut outcome,
        );
        assert!(!keep_pending);
        assert!(outcome.retry_required);
        assert!(outcome.fallback_changed);
        assert!(state.permanently_disabled);
        assert_eq!(state.pending_move, None);
        assert!(state.transient_fallback_crtcs.is_empty());
    }
}

#[test]
fn cursor_move_outcome_merge_keeps_cross_device_retry_liveness() {
    let mut aggregate = CursorMoveOutcome {
        ebusy_count: 2,
        fallback_changed: false,
        retry_required: false,
    };
    aggregate.merge(CursorMoveOutcome {
        ebusy_count: 3,
        fallback_changed: true,
        retry_required: true,
    });
    assert_eq!(aggregate.ebusy_count, 5);
    assert!(aggregate.fallback_changed);
    assert!(aggregate.retry_required);
}

/// Once a show/bind fails with an unsupported errno, the plane is
/// no longer reported available, so `tick_one_output`'s `hw_can_run`
/// gate closes and `build_scene` collapses every assignment to SW.
/// The latch is sticky across subsequent queries.
#[test]
fn unsupported_cursor_failure_latches_plane_unavailable() {
    let mut p = PlatformBackend::for_tests();
    let key = p.devices[0].key;
    let crtc = p.outputs[0].output.crtc;
    assert!(!p.hw_cursor_disabled_for_device(key));
    assert!(p.note_unbound_cursor_failure(
        0,
        crtc,
        &std::io::Error::from_raw_os_error(libc::ENXIO)
    ));
    assert!(p.hw_cursor_disabled_for_device(key));
    assert!(!p.cursor_plane_available());
    assert!(!p.note_unbound_cursor_failure(
        0,
        crtc,
        &std::io::Error::from_raw_os_error(libc::EBUSY)
    ));
    assert!(p.hw_cursor_disabled_for_device(key));
}

/// Test fixture works at all: open `for_tests`, query
/// dimensions, query poll_fds, no Vk required.
#[test]
fn for_tests_constructs() {
    let p = PlatformBackend::for_tests();
    assert_eq!(p.fb_dimensions(), (800, 600));
    assert_eq!(p.outputs.len(), 1);
    assert!(p.vk.is_none()); // for_tests skips Vk
    let fds = p.poll_fds();
    // No input_ctx, one DRM fd.
    assert!(fds.iter().any(|(_, k)| matches!(k, BackendFdKind::Drm)));
}

#[test]
fn recompute_fb_extent_matches_issue9_dual_2560x1440() {
    // Side-by-side (y=0): fb = 5120x1440.
    let layouts = &[
        (0i32, 0i32, 2560u16, 1440u16),
        (2560i32, 0i32, 2560u16, 1440u16),
    ];
    assert_eq!(super::recompute_fb_extent_from(layouts), (5120, 1440));
}

#[test]
fn recompute_fb_extent_2d_vertical_stack() {
    // Stacked (second monitor below at y=1440): fb = 2560x2880.
    let layouts = &[
        (0i32, 0i32, 2560u16, 1440u16),
        (0i32, 1440i32, 2560u16, 1440u16),
    ];
    assert_eq!(super::recompute_fb_extent_from(layouts), (2560, 2880));
}

/// Fence acquire on a no-Vk fixture returns the
/// "init failed" error (since fence_pool is None). This
/// confirms the guard is wired; real fence allocation is
/// covered by Stage 2c+ Vk-backed tests.
#[test]
fn for_tests_fence_acquire_errors_without_vk() {
    let p = PlatformBackend::for_tests();
    let result = p.acquire_fence_ticket();
    assert!(matches!(
        result,
        Err(vk::Result::ERROR_INITIALIZATION_FAILED)
    ));
}

/// BO acquire on a no-Vk fixture returns None (the single
/// stub output has no pool).
#[test]
fn for_tests_scanout_acquire_returns_none() {
    let mut p = PlatformBackend::for_tests();
    assert!(p.acquire_scanout_bo(0).is_none());
}

/// Pending-move slot is None on a fresh backend and stays None
/// when the cursor plane is unavailable (the for_tests fixture
/// has no real DRM device, so `cursor_plane_move` returns Err
/// and never touches the slot).
#[test]
fn cursor_pending_move_starts_empty_and_unavailable_path_does_not_set_it() {
    let mut p = PlatformBackend::for_tests();
    assert_eq!(p.devices[0].cursor.pending_move, None);
    // Unavailable plane → Err return → pending stays None.
    assert!(p.cursor_plane_move(100, 200, 0, 0).is_err());
    assert_eq!(p.devices[0].cursor.pending_move, None);
    // Drain on empty slot is Ok(0) (early-exit before any
    // plane access). The path that returns Err is only the
    // populated-slot retry that hits the unavailable plane —
    // tested separately in `cursor_pending_move_is_latest_wins`.
    assert_eq!(
        p.cursor_plane_drain_pending_move_for_output(0).ok(),
        Some(CursorMoveOutcome::default())
    );
    assert_eq!(p.devices[0].cursor.pending_move, None);
}

/// Hide-all clears any pending move (VT-leave invariant).
#[test]
fn cursor_plane_hide_all_clears_pending_move() {
    let mut p = PlatformBackend::for_tests();
    p.devices[0].cursor.pending_move = Some((123, 456, 7, 9));
    // hide_all returns Err on the unavailable fixture, but the
    // pending-clear MUST happen before the early-return so a
    // hide-failure mid-recovery leaves no stale pending.
    let _ = p.cursor_plane_hide_all();
    assert_eq!(p.devices[0].cursor.pending_move, None);
}

/// Latest-wins: explicitly setting pending then overwriting
/// reflects the latest position. This is the same in-place mutation
/// that `cursor_plane_move` does internally on EBUSY — by exercising
/// it directly (since we can't drive a real EBUSY without a kernel),
/// we lock in the "old pending is discarded" invariant.
#[test]
fn cursor_pending_move_is_latest_wins() {
    let mut p = PlatformBackend::for_tests();
    p.devices[0].cursor.pending_move = Some((100, 100, 1, 2));
    p.devices[0].cursor.pending_move = Some((200, 250, 7, 9));
    assert_eq!(p.devices[0].cursor.pending_move, Some((200, 250, 7, 9)));
    // Drain consumes; on the unavailable fixture this errors but
    // the test's invariant is the slot mechanics, not the drain.
    let _ = p.cursor_plane_drain_pending_move_for_output(0);
    // Slot still holds because drain Err'd before clearing.
    assert_eq!(p.devices[0].cursor.pending_move, Some((200, 250, 7, 9)));
}

#[test]
fn cursor_root_to_crtc_local_subtracts_hotspot() {
    assert_eq!(
        cursor_root_to_crtc_local(200, 300, 10, 20, 7, 9),
        (183, 271)
    );
}

/// `cursor_footprint_intersects_output` is the membership rule the
/// pointer fast path uses to detect a CRTC-boundary crossing
/// (regression: cursor stayed frozen on screen 1, invisible on
/// screen 2, once the idle compositor stopped reassigning it).
/// Modelled on a side-by-side dual-head layout: left [0,0,2560,1440],
/// right [2560,0,2560,1440], a 64×64 sprite, hotspot (0,0).
#[test]
fn cursor_footprint_intersects_output_dual_head_seam() {
    // root-space x relative to each output's origin (hotspot 0).
    let on_left = |rx: i32, ry: i32| cursor_footprint_intersects_output(rx, ry, 64, 64, 2560, 1440);
    let on_right =
        |rx: i32, ry: i32| cursor_footprint_intersects_output(rx - 2560, ry, 64, 64, 2560, 1440);

    // Fully on the left screen.
    assert!(on_left(100, 100));
    assert!(!on_right(100, 100));

    // Fully on the right screen.
    assert!(!on_left(3000, 100));
    assert!(on_right(3000, 100));

    // Straddling the seam — present on BOTH screens (matches the
    // scene clipping a 64px sprite onto both outputs).
    assert!(on_left(2540, 100));
    assert!(on_right(2540, 100));

    // Below both outputs (y past height) — on neither.
    assert!(!on_left(100, 2000));
    assert!(!on_right(3000, 2000));
}

/// `invalidate_bo` on a missing entry is a no-op (doesn't
/// panic). With no pool entries there's nothing to flag,
/// but the call must remain safe.
#[test]
fn for_tests_invalidate_bo_is_noop_on_missing_entry() {
    let mut p = PlatformBackend::for_tests();
    p.invalidate_bo(0, 0); // empty bo_generations[0]
    p.invalidate_bo(99, 0); // out-of-range output_idx
}

/// `on_page_flip_complete` without a prior `present_scanout`
/// is a no-op (no Pending BO to retire).
#[test]
fn for_tests_on_page_flip_complete_without_pending_is_none() {
    let mut p = PlatformBackend::for_tests();
    assert!(p.on_page_flip_complete(0).is_none());
}

/// `record_present` advances `next_present_generation`
/// monotonically.
#[test]
fn record_present_advances_generation() {
    let mut p = PlatformBackend::for_tests();
    let g1 = p.record_present(0, 0);
    let g2 = p.record_present(0, 0);
    assert_eq!(g1 + 1, g2);
    assert!(g1 > 0); // first generation is 1, not 0
}

/// `commit_bo_present` is a no-op on a missing entry, but
/// the `record_present` counter still advances and survives
/// a subsequent successful entry write.
#[test]
fn commit_bo_present_is_safe_on_missing_entry() {
    let mut p = PlatformBackend::for_tests();
    let g = p.record_present(0, 0);
    p.commit_bo_present(0, 0, g); // bo_generations[0] is empty — no-op
    p.commit_bo_present(99, 99, g); // out-of-range — no-op
}

#[test]
fn platform_starts_with_empty_closed_submit_group() {
    let p = PlatformBackend::for_tests();
    assert!(!p.submit_group_is_open(), "fresh platform has closed group");
    assert_eq!(p.submit_group_size(), 0);
}

#[test]
fn flush_submit_group_empty_is_noop() {
    let mut p = PlatformBackend::for_tests();
    // Fixture has no Vk; should NOT attempt queue_submit2.
    let outcome = p
        .flush_submit_group(FlushReason::SceneCompose)
        .expect("empty-group flush is always Ok");
    assert_eq!(outcome.flushed_entries, 0);
    assert!(!p.submit_group_is_open());
}

// ── Task 3 test helpers ──────────────────────────────────────

#[cfg(test)]
impl PlatformBackend {
    pub(crate) fn submit_group_max_size_for_tests(&self) -> usize {
        self.submit_group.max_size()
    }

    pub(crate) fn queue_submit2_count_for_tests(&self) -> u64 {
        crate::kms::vk::call_stats::queue_submit2_count()
    }

    pub(crate) fn force_next_submit_failure_for_tests(&mut self) {
        self.force_next_submit_failure = true;
    }
}

#[test]
fn present_completion_epfd_present_at_init_and_poll_fds() {
    // Use the headless fixture — production VkContext init isn't
    // required to exercise the inner-epoll FD.
    let p = PlatformBackend::for_tests();
    let fds = p.poll_fds();
    let present_kind = yserver_core::backend::BackendFdKind::PresentCompletion;
    assert!(
        fds.iter().any(|(_, k)| *k == present_kind),
        "platform.poll_fds() must report a PresentCompletion FD"
    );
    // The FD should be stable: a second call returns the same raw value.
    let raw1 = fds.iter().find(|(_, k)| *k == present_kind).unwrap().0;
    let raw2 = p
        .poll_fds()
        .iter()
        .find(|(_, k)| *k == present_kind)
        .unwrap()
        .0;
    assert_eq!(
        raw1, raw2,
        "the inner epfd is stable across poll_fds() calls"
    );
}

/// Mirrors `descriptor_pool_ring::tests::vk_or_skip` — needed
/// because `VkContext::new()` requires a live Vulkan ICD which
/// isn't always available in CI.
fn vk_or_skip() -> Option<Arc<VkContext>> {
    match VkContext::new() {
        Ok(vk) => Some(vk),
        Err(e) => {
            eprintln!("skipping: no Vk: {e:?}");
            None
        }
    }
}

/// Regression: `KmsBackend`'s field-drop order runs `platform`
/// (containing `fence_pool`) BEFORE `store` / `engine` / `scene`,
/// all of which hold `FenceTicket`s. Pre-fix those tickets
/// dropped after the pool was gone, `FenceTicketInner::drop`
/// bailed on `Weak::upgrade() == None`, and leaked every VkFence
/// handle (1471 leaked at SIGTERM on bee/MATE 2026-05-31, all
/// `VkFence` per the validation layer's first-10 list). Fix
/// added a strong `Arc<VkContext>` on `FenceTicketInner` so the
/// fallback `Drop` path destroys the fence directly. This test
/// simulates the order bug by dropping the pool first and then
/// the ticket; it verifies the device is still usable after
/// (which a leaked-handle path would still allow, but a
/// use-after-free wouldn't). Validation-layer leak verification
/// is via the smoke recipe with VK_LAYER_KHRONOS_validation.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn fence_ticket_destroys_fence_when_pool_dropped_first() {
    let Some(vk) = vk_or_skip() else { return };
    let pool = FencePool::new(Arc::clone(&vk));
    let ticket = pool.acquire().expect("acquire");

    // Simulate KmsBackend's drop-order bug: pool drops while
    // ticket is still alive (held by store/engine/scene state).
    drop(pool);

    // The ticket's strong Arc<VkContext> + ours keep the device
    // alive. Pre-fix this drop leaked the fence handle; post-fix
    // it calls destroy_fence directly.
    drop(ticket);

    // Device still usable — wait_idle returns Ok and we can
    // create + destroy another fence cleanly.
    unsafe { vk.device.device_wait_idle().expect("wait_idle") };
    let f = unsafe {
        vk.device
            .create_fence(&vk::FenceCreateInfo::default(), None)
            .expect("create_fence")
    };
    unsafe { vk.device.destroy_fence(f, None) };
}

/// `for_tests_stub` constructs a `FenceTicket` with no real
/// device. The fallback `Drop` path must no-op cleanly in that
/// case (null fence + `vk: None`) and not segfault attempting
/// to call `destroy_fence` on a null Arc.
#[test]
fn for_tests_stub_drops_cleanly_without_vk() {
    let ticket = FenceTicket::for_tests_stub();
    // Drop runs at end of scope; no-op expected.
    drop(ticket);
}

/// Imported SYNC_FD wait semaphores must attach to the shared ticket
/// inner, so every clone observes the same submission-lifetime pins.
#[test]
fn fence_ticket_retains_imported_wait_semaphores_across_clones() {
    let ticket = FenceTicket::for_tests_stub();
    let clone = ticket.clone();
    ticket.retain_imported_wait_semaphores(vec![vk::Semaphore::null()]);
    assert_eq!(clone.inner.imported_wait_semaphores.borrow().len(), 1);
}
