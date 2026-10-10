use super::*;

impl CopyFreeScanoutError {
    pub(super) fn into_io_error(self) -> io::Error {
        match self {
            Self::Candidates(error) => error,
            Self::TerminalDisposableProbe(error) => terminal_disposable_probe_io_error(error),
            Self::LiveRendererLost(error) => io::Error::other(Self::LiveRendererLost(error)),
        }
    }
}

fn terminal_disposable_probe_io_error(source: io::Error) -> io::Error {
    io::Error::new(source.kind(), TerminalDisposableProbeMarker { source })
}

pub(crate) fn is_terminal_disposable_probe_error(error: &io::Error) -> bool {
    fn contains(error: &(dyn std::error::Error + 'static)) -> bool {
        if error
            .downcast_ref::<TerminalDisposableProbeMarker>()
            .is_some()
        {
            return true;
        }
        if let Some(io_error) = error.downcast_ref::<io::Error>()
            && let Some(inner) = io_error.get_ref()
        {
            return contains(inner);
        }
        error.source().is_some_and(contains)
    }

    contains(error)
}

impl CopiedScanoutError {
    pub(super) fn into_io_error(self) -> io::Error {
        match self {
            Self::Candidates(error) => error,
            Self::TerminalDisposableProbe(error) => terminal_disposable_probe_io_error(error),
            error @ Self::LiveDeviceLost { .. } => io::Error::other(error),
        }
    }
}

pub(super) fn require_copied_sink_explicit_dmabuf_layout_import(supported: bool) -> io::Result<()> {
    if supported {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "copied scanout requires VK_EXT_image_drm_format_modifier on the sink renderer to import the source DMA-BUF with its exact modifier, offset, and pitch",
    ))
}

pub(super) fn route_requires_copy_free_probe(route: ScanoutRoute) -> bool {
    route.relationship != RenderKmsRelationship::Same
}

fn scanout_qualification_vk_init_error(
    stage: &str,
    error: VkInitError,
) -> ScanoutQualificationError {
    let device_lost = matches!(
        &error,
        VkInitError::Vk(result) if *result == vk::Result::ERROR_DEVICE_LOST
    );
    let error = io::Error::other(format!("{stage}: {error}"));
    if device_lost {
        ScanoutQualificationError::DeviceLost(error)
    } else {
        ScanoutQualificationError::Rejected(error)
    }
}

pub(super) fn classify_copy_free_qualification_error(
    error: CopyFreeScanoutError,
) -> ScanoutQualificationError {
    match error {
        CopyFreeScanoutError::Candidates(error)
            if crate::kms::vk::scanout::scanout_error_is_device_lost(&error) =>
        {
            ScanoutQualificationError::DeviceLost(error)
        }
        CopyFreeScanoutError::Candidates(error) => ScanoutQualificationError::Rejected(error),
        CopyFreeScanoutError::TerminalDisposableProbe(error) => {
            ScanoutQualificationError::Indeterminate(error)
        }
        CopyFreeScanoutError::LiveRendererLost(error) => {
            ScanoutQualificationError::DeviceLost(error)
        }
    }
}

pub(super) fn classify_copied_qualification_error(
    error: CopiedScanoutError,
) -> ScanoutQualificationError {
    match error {
        CopiedScanoutError::Candidates(error)
            if crate::kms::vk::scanout::scanout_error_is_device_lost(&error) =>
        {
            ScanoutQualificationError::DeviceLost(error)
        }
        CopiedScanoutError::Candidates(error) => ScanoutQualificationError::Rejected(error),
        CopiedScanoutError::TerminalDisposableProbe(error) => {
            ScanoutQualificationError::Indeterminate(error)
        }
        CopiedScanoutError::LiveDeviceLost { context, source } => {
            ScanoutQualificationError::DeviceLost(io::Error::new(
                source.kind(),
                format!("{context}: {source}"),
            ))
        }
    }
}

/// Create a disposable compositor-profile context from a stable selector.
///
/// `VkContext` intentionally exposes selector-based construction only for the
/// minimal transfer profile. Use that submission-free context as an exact
/// physical-device anchor for the compositor profile, then mark the anchor
/// quiescent before it leaves this function.
fn new_disposable_compositor_for_selector(
    selector: VulkanDeviceSelector,
) -> Result<Arc<VkContext>, VkInitError> {
    let selector_anchor = VkContext::new_disposable_transfer_for_device(selector)?;
    let compositor = VkContext::new_disposable_for_same_physical_device(&selector_anchor);
    selector_anchor.mark_disposable_probe_quiescent();
    compositor
}

/// Apply the worker candidate policy independently of Vulkan/DRM mechanics:
/// copy-free candidates precede copied candidates, ordinary rejection advances
/// the sequence, and an indeterminate/device-lost result stops immediately.
pub(super) fn qualify_scanout_candidates_in_order<S, C>(
    shared_candidates: impl IntoIterator<Item = S>,
    mut qualify_shared: impl FnMut(S) -> Result<QualifiedScanoutPlan, ScanoutQualificationError>,
    copied_candidates: impl FnOnce() -> Result<Vec<C>, ScanoutQualificationError>,
    mut qualify_copied: impl FnMut(C) -> Result<QualifiedScanoutPlan, ScanoutQualificationError>,
) -> Result<QualifiedScanoutPlan, ScanoutQualificationError> {
    let mut failures = Vec::new();
    for candidate in shared_candidates {
        match qualify_shared(candidate) {
            Ok(qualified) => return Ok(qualified),
            Err(ScanoutQualificationError::Rejected(error)) => {
                failures.push(format!("copy-free: {error}"));
            }
            Err(error) => return Err(error),
        }
    }

    let copied_candidates = match copied_candidates() {
        Ok(candidates) => candidates,
        Err(ScanoutQualificationError::Rejected(error)) => {
            failures.push(format!("copied: {error}"));
            Vec::new()
        }
        Err(error) => return Err(error),
    };
    for candidate in copied_candidates {
        match qualify_copied(candidate) {
            Ok(qualified) => return Ok(qualified),
            Err(ScanoutQualificationError::Rejected(error)) => {
                failures.push(format!("copied: {error}"));
            }
            Err(error) => return Err(error),
        }
    }

    Err(ScanoutQualificationError::Rejected(io::Error::other(
        format!(
            "every disposable scanout candidate failed: {}",
            failures.join("; ")
        ),
    )))
}

/// Qualify a cross-device route entirely on fresh disposable Vulkan contexts.
///
/// The returned plan contains only scalar identities. Copy-free candidates are
/// exhausted first; copied candidates preserve their native-modifier-before-
/// LINEAR ordering. Every exact candidate gets a newly-created context set, so
/// a rejected candidate cannot contaminate the next one. Only ordinary,
/// proven-quiescent rejection advances the sequence.
#[allow(clippy::too_many_arguments)]
pub(crate) fn qualify_scanout_route_for_worker(
    render_selector: VulkanDeviceSelector,
    copied_sink: Option<CopiedQualificationSink>,
    scanout_device: Rc<drm::Device>,
    output: &crate::platform::drm::Output,
    route: ScanoutRoute,
    width: u32,
    height: u32,
    fence_timeout_ns: u64,
) -> Result<QualifiedScanoutPlan, ScanoutQualificationError> {
    if !route_requires_copy_free_probe(route) {
        return Err(ScanoutQualificationError::Rejected(io::Error::new(
            io::ErrorKind::InvalidInput,
            "worker qualification is only valid for a cross-device scanout route",
        )));
    }
    if width == 0 || height == 0 {
        return Err(ScanoutQualificationError::Rejected(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("worker qualification received invalid extent {width}x{height}"),
        )));
    }
    if fence_timeout_ns == 0 {
        return Err(ScanoutQualificationError::Rejected(io::Error::new(
            io::ErrorKind::InvalidInput,
            "worker qualification requires a non-zero per-fence timeout",
        )));
    }

    let shared_inventory = new_disposable_compositor_for_selector(render_selector)
        .map_err(|error| scanout_qualification_vk_init_error("copy-free plan inventory", error))?;
    let shared_candidates = ScanoutBoPool::exact_allocation_plans(
        &shared_inventory,
        &scanout_device,
        width,
        &output.scanout_modifiers,
    );
    shared_inventory.mark_disposable_probe_quiescent();
    drop(shared_inventory);

    qualify_scanout_candidates_in_order(
        shared_candidates,
        |plan| {
            let probe_vk =
                new_disposable_compositor_for_selector(render_selector).map_err(|error| {
                    scanout_qualification_vk_init_error(
                        &format!("{} disposable source renderer", plan.describe()),
                        error,
                    )
                })?;
            qualify_copy_free_scanout_plan(
                probe_vk,
                Rc::clone(&scanout_device),
                output,
                route,
                width,
                height,
                &output.scanout_modifiers,
                plan,
                fence_timeout_ns,
            )
            .map_err(classify_copy_free_qualification_error)
        },
        || {
            let sink = copied_sink.ok_or_else(|| {
                ScanoutQualificationError::Rejected(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "no exact sink renderer is available for copied scanout",
                ))
            })?;
            let render_inventory = new_disposable_compositor_for_selector(render_selector)
                .map_err(|error| {
                    scanout_qualification_vk_init_error("copied plan source inventory", error)
                })?;
            let sink_inventory = match VkContext::new_disposable_transfer_for_device(sink.selector)
            {
                Ok(context) => context,
                Err(error) => {
                    render_inventory.mark_disposable_probe_quiescent();
                    return Err(scanout_qualification_vk_init_error(
                        "copied plan sink inventory",
                        error,
                    ));
                }
            };
            let candidates = CopiedScanoutPool::exact_allocation_plans(
                &render_inventory,
                &sink_inventory,
                &scanout_device,
                width,
                &output.scanout_modifiers,
            );
            render_inventory.mark_disposable_probe_quiescent();
            sink_inventory.mark_disposable_probe_quiescent();
            drop(render_inventory);
            drop(sink_inventory);
            Ok(candidates)
        },
        |plan| {
            let sink = copied_sink.expect("copied candidates require a sink identity");
            let probe_render_vk =
                new_disposable_compositor_for_selector(render_selector).map_err(|error| {
                    scanout_qualification_vk_init_error(
                        &format!("{} disposable source renderer", plan.describe()),
                        error,
                    )
                })?;
            let probe_sink_vk = match VkContext::new_disposable_transfer_for_device(sink.selector) {
                Ok(context) => context,
                Err(error) => {
                    probe_render_vk.mark_disposable_probe_quiescent();
                    return Err(scanout_qualification_vk_init_error(
                        &format!("{} disposable sink renderer", plan.describe()),
                        error,
                    ));
                }
            };
            let destination_route =
                ScanoutRoute::new(sink.id, route.kms_device_key, RenderKmsRelationship::Same);
            qualify_copied_scanout_plan(
                probe_render_vk,
                probe_sink_vk,
                Rc::clone(&scanout_device),
                output,
                route,
                destination_route,
                width,
                height,
                &output.scanout_modifiers,
                plan,
                fence_timeout_ns,
            )
            .map_err(classify_copied_qualification_error)
        },
    )
}

fn test_scanout_pool(
    scanout_device: &drm::Device,
    output: &crate::platform::drm::Output,
    pool: &ScanoutBoPool,
) -> io::Result<()> {
    for (index, bo) in pool.bos.iter().enumerate() {
        let framebuffer = bo.fb_handle.ok_or_else(|| {
            io::Error::other(format!("scanout pool BO {index} has no framebuffer"))
        })?;
        crate::drm::modeset::test_modeset(scanout_device, output, framebuffer).map_err(|err| {
            io::Error::new(
                err.kind(),
                format!("scanout pool BO {index} atomic TEST_ONLY failed: {err}"),
            )
        })?;
    }
    Ok(())
}

fn test_disposable_scanout_pool(
    scanout_device: &drm::Device,
    output: &crate::platform::drm::Output,
    pool: &ScanoutBoPool,
) -> Result<(), DisposableProbeError> {
    for (index, bo) in pool.bos.iter().enumerate() {
        let framebuffer = bo.fb_handle.ok_or_else(|| {
            DisposableProbeError::from(io::Error::other(format!(
                "disposable scanout pool BO {index} has no framebuffer"
            )))
        })?;
        if let Err(error) =
            crate::drm::modeset::test_modeset_strict(scanout_device, output, framebuffer)
        {
            let blob_cleanup_failed = error.blob_cleanup_failed();
            let source = error.into_io_error();
            let source = io::Error::new(
                source.kind(),
                format!("scanout pool BO {index} atomic TEST_ONLY failed: {source}"),
            );
            return Err(if blob_cleanup_failed {
                DisposableProbeError::terminal_cleanup(source)
            } else {
                DisposableProbeError::from(source)
            });
        }
    }
    Ok(())
}

pub(super) fn copy_free_candidate_error(
    plan: ScanoutAllocationPlan,
    stage: &str,
    error: &io::Error,
) -> String {
    format!("{} {stage}: {error}", plan.describe())
}

/// Qualify one exact copy-free representation using only a disposable Vulkan
/// context. The returned value owns no Vulkan or DRM resource and can be handed
/// to a later live replay boundary.
#[allow(clippy::too_many_arguments)]
pub(crate) fn qualify_copy_free_scanout_plan(
    probe_vk: Arc<VkContext>,
    scanout_device: Rc<drm::Device>,
    output: &crate::platform::drm::Output,
    route: ScanoutRoute,
    width: u32,
    height: u32,
    scanout_modifiers: &[u64],
    plan: ScanoutAllocationPlan,
    fence_timeout_ns: u64,
) -> Result<QualifiedScanoutPlan, CopyFreeScanoutError> {
    debug_assert!(route_requires_copy_free_probe(route));
    let probe_pool = match ScanoutBoPool::allocate_exact_for_disposable_probe(
        Arc::clone(&probe_vk),
        Rc::clone(&scanout_device),
        route,
        width,
        height,
        SCANOUT_POOL_DEPTH,
        scanout_modifiers,
        plan,
    ) {
        Ok(pool) => pool,
        Err(error) => {
            probe_vk.mark_disposable_probe_quiescent();
            let abort_candidate_search = error.abort_candidate_search();
            let failure =
                error.into_io_error_with_context(format!("{} probe allocation", plan.describe()));
            if abort_candidate_search {
                return Err(CopyFreeScanoutError::TerminalDisposableProbe(failure));
            }
            return Err(CopyFreeScanoutError::Candidates(failure));
        }
    };
    if let Err(error) = test_disposable_scanout_pool(&scanout_device, output, &probe_pool) {
        let error = probe_pool
            .finish_disposable_probe(Err(error))
            .expect_err("failed TEST_ONLY cannot become a successful probe");
        let abort_candidate_search = error.abort_candidate_search();
        let failure =
            error.into_io_error_with_context(format!("{} probe TEST_ONLY", plan.describe()));
        if abort_candidate_search {
            return Err(CopyFreeScanoutError::TerminalDisposableProbe(failure));
        }
        return Err(CopyFreeScanoutError::Candidates(failure));
    }
    if let Err(error) = probe_pool.probe_renderer_access(fence_timeout_ns) {
        let abort_candidate_search = error.abort_candidate_search();
        let failure =
            error.into_io_error_with_context(format!("{} probe rendering", plan.describe()));
        if abort_candidate_search {
            return Err(CopyFreeScanoutError::TerminalDisposableProbe(failure));
        }
        return Err(CopyFreeScanoutError::Candidates(failure));
    }

    Ok(QualifiedScanoutPlan::Shared(plan))
}

/// Replay one already-qualified copy-free representation on the live context.
/// Live allocation receives its own TEST_ONLY pass before an optional first
/// modeset. A recoverable live-only rejection is returned separately so the
/// synchronous compatibility wrapper can preserve its established fallback.
#[allow(clippy::too_many_arguments)]
pub(crate) fn replay_copy_free_scanout_plan(
    live_vk: Arc<VkContext>,
    scanout_device: Rc<drm::Device>,
    output: &crate::platform::drm::Output,
    route: ScanoutRoute,
    width: u32,
    height: u32,
    scanout_modifiers: &[u64],
    qualified: QualifiedScanoutPlan,
    commit_first_framebuffer: bool,
) -> Result<ExactPlanReplay<PreparedScanoutPool>, CopyFreeScanoutError> {
    let QualifiedScanoutPlan::Shared(plan) = qualified else {
        return Err(CopyFreeScanoutError::Candidates(io::Error::new(
            io::ErrorKind::InvalidInput,
            "copy-free replay received a copied qualification result",
        )));
    };

    let live_pool = match ScanoutBoPool::allocate_exact(
        Arc::clone(&live_vk),
        Rc::clone(&scanout_device),
        route,
        width,
        height,
        SCANOUT_POOL_DEPTH,
        scanout_modifiers,
        plan,
    ) {
        Ok(pool) => pool,
        Err(error) => {
            if crate::kms::vk::scanout::scanout_error_is_device_lost(&error) {
                return Err(CopyFreeScanoutError::LiveRendererLost(io::Error::new(
                    error.kind(),
                    format!("{} live allocation: {error}", plan.describe()),
                )));
            }
            return Ok(ExactPlanReplay::Rejected(io::Error::new(
                error.kind(),
                copy_free_candidate_error(plan, "live allocation", &error),
            )));
        }
    };
    if let Err(error) = test_scanout_pool(&scanout_device, output, &live_pool) {
        return Ok(ExactPlanReplay::Rejected(io::Error::new(
            error.kind(),
            copy_free_candidate_error(plan, "live TEST_ONLY", &error),
        )));
    }

    let mut live_pool = live_pool;
    let committed_framebuffer = if commit_first_framebuffer {
        let (front_index, framebuffer) = live_pool
            .bos
            .iter()
            .enumerate()
            .find_map(|(index, bo)| bo.fb_handle.map(|framebuffer| (index, framebuffer)))
            .ok_or_else(|| {
                CopyFreeScanoutError::Candidates(io::Error::other(format!(
                    "{} live pool has no framebuffer",
                    plan.describe(),
                )))
            })?;
        if let Err(error) =
            crate::drm::modeset::commit_modeset(&scanout_device, output, framebuffer)
        {
            return Ok(ExactPlanReplay::Rejected(io::Error::new(
                error.kind(),
                copy_free_candidate_error(plan, "live modeset", &error),
            )));
        }
        // The successful synchronous commit has already made this BO the
        // hardware front. Mark it before returning so no fallible caller work
        // or structure-state gap can drop/acquire the scanned BO.
        live_pool.bos[front_index]
            .state
            .mark_on_screen_after_modeset();
        Some(framebuffer)
    } else {
        None
    };

    Ok(ExactPlanReplay::Prepared(PreparedScanoutPool {
        pool: live_pool,
        committed_framebuffer,
    }))
}

/// Preserve the synchronous candidate order while keeping disposable
/// qualification and live exact-plan replay as separate operations.
pub(super) fn allocate_copy_free_scanout_pool(
    live_vk: Arc<VkContext>,
    scanout_device: Rc<drm::Device>,
    output: &crate::platform::drm::Output,
    route: ScanoutRoute,
    width: u32,
    height: u32,
    scanout_modifiers: &[u64],
    commit_first_framebuffer: bool,
) -> Result<PreparedScanoutPool, CopyFreeScanoutError> {
    debug_assert!(route_requires_copy_free_probe(route));
    let plans =
        ScanoutBoPool::exact_allocation_plans(&live_vk, &scanout_device, width, scanout_modifiers);
    let mut failures = Vec::new();

    for plan in plans {
        let probe_vk = match VkContext::new_disposable_for_same_physical_device(&live_vk) {
            Ok(vk) => vk,
            Err(error) => {
                failures.push(format!(
                    "{} disposable Vulkan device: {error}",
                    plan.describe()
                ));
                continue;
            }
        };
        let qualified = match qualify_copy_free_scanout_plan(
            probe_vk,
            Rc::clone(&scanout_device),
            output,
            route,
            width,
            height,
            scanout_modifiers,
            plan,
            PRIME_RENDER_PROBE_TIMEOUT_NS,
        ) {
            Ok(qualified) => qualified,
            Err(CopyFreeScanoutError::Candidates(error)) => {
                failures.push(error.to_string());
                continue;
            }
            Err(CopyFreeScanoutError::TerminalDisposableProbe(error)) => {
                let error_kind = error.kind();
                failures.push(error.to_string());
                return Err(CopyFreeScanoutError::TerminalDisposableProbe(
                    io::Error::new(
                        error_kind,
                        format!(
                            "copy-free scanout probing stopped after a terminal disposable-probe \
                             failure: {}",
                            failures.join("; ")
                        ),
                    ),
                ));
            }
            Err(error @ CopyFreeScanoutError::LiveRendererLost(_)) => return Err(error),
        };

        match replay_copy_free_scanout_plan(
            Arc::clone(&live_vk),
            Rc::clone(&scanout_device),
            output,
            route,
            width,
            height,
            scanout_modifiers,
            qualified,
            commit_first_framebuffer,
        )? {
            ExactPlanReplay::Prepared(prepared) => {
                log::info!(
                    "copy-free scanout probe selected {} for {route:?}",
                    plan.describe()
                );
                return Ok(prepared);
            }
            ExactPlanReplay::Rejected(error) => failures.push(error.to_string()),
        }
    }

    Err(CopyFreeScanoutError::Candidates(io::Error::other(format!(
        "every copy-free scanout candidate failed for {route:?}: {}",
        failures.join("; ")
    ))))
}

/// Qualify one exact copied representation using only disposable A/B contexts.
/// The KMS destination passes TEST_ONLY before any submitted content probe.
#[allow(clippy::too_many_arguments)]
pub(crate) fn qualify_copied_scanout_plan(
    probe_render_vk: Arc<VkContext>,
    probe_sink_vk: Arc<VkContext>,
    scanout_device: Rc<drm::Device>,
    output: &crate::platform::drm::Output,
    route: ScanoutRoute,
    destination_route: ScanoutRoute,
    width: u32,
    height: u32,
    scanout_modifiers: &[u64],
    plan: CopiedScanoutPlan,
    fence_timeout_ns: u64,
) -> Result<QualifiedScanoutPlan, CopiedScanoutError> {
    debug_assert!(route_requires_copy_free_probe(route));
    debug_assert_eq!(destination_route.relationship, RenderKmsRelationship::Same);
    if let Err(error) =
        require_copied_sink_explicit_dmabuf_layout_import(probe_sink_vk.image_drm_format_modifier)
    {
        probe_render_vk.mark_disposable_probe_quiescent();
        probe_sink_vk.mark_disposable_probe_quiescent();
        return Err(CopiedScanoutError::Candidates(error));
    }

    let probe_pool = match CopiedScanoutPool::allocate_exact_for_disposable_probe(
        Arc::clone(&probe_render_vk),
        Arc::clone(&probe_sink_vk),
        Rc::clone(&scanout_device),
        route,
        destination_route,
        width,
        height,
        SCANOUT_POOL_DEPTH,
        scanout_modifiers,
        plan,
    ) {
        Ok(pool) => pool,
        Err(error) => {
            probe_render_vk.mark_disposable_probe_quiescent();
            probe_sink_vk.mark_disposable_probe_quiescent();
            let abort_candidate_search = error.abort_candidate_search();
            let failure =
                error.into_io_error_with_context(format!("{} probe allocation", plan.describe()));
            if abort_candidate_search {
                return Err(CopiedScanoutError::TerminalDisposableProbe(failure));
            }
            return Err(CopiedScanoutError::Candidates(failure));
        }
    };
    if let Err(error) =
        test_disposable_scanout_pool(&scanout_device, output, &probe_pool.destinations)
    {
        let error = probe_pool
            .finish_disposable_probe(Err(error))
            .expect_err("failed TEST_ONLY cannot become a successful copied probe");
        let abort_candidate_search = error.abort_candidate_search();
        let failure =
            error.into_io_error_with_context(format!("{} probe TEST_ONLY", plan.describe()));
        if abort_candidate_search {
            return Err(CopiedScanoutError::TerminalDisposableProbe(failure));
        }
        return Err(CopiedScanoutError::Candidates(failure));
    }
    if let Err(error) = probe_pool.probe_copy_all(fence_timeout_ns) {
        let abort_candidate_search = error.abort_candidate_search();
        let failure = error
            .into_io_error_with_context(format!("{} probe render/copy/readback", plan.describe()));
        if abort_candidate_search {
            return Err(CopiedScanoutError::TerminalDisposableProbe(failure));
        }
        return Err(CopiedScanoutError::Candidates(failure));
    }

    Ok(QualifiedScanoutPlan::Copied {
        sink_id: destination_route.render_device_id,
        plan,
    })
}

/// Replay one already-qualified copied representation on the live A/B
/// contexts, repeating TEST_ONLY before an optional first modeset.
#[allow(clippy::too_many_arguments)]
pub(crate) fn replay_copied_scanout_plan(
    live_render_vk: Arc<VkContext>,
    live_sink_vk: Arc<VkContext>,
    scanout_device: Rc<drm::Device>,
    output: &crate::platform::drm::Output,
    route: ScanoutRoute,
    destination_route: ScanoutRoute,
    width: u32,
    height: u32,
    scanout_modifiers: &[u64],
    qualified: QualifiedScanoutPlan,
    commit_first_framebuffer: bool,
) -> Result<ExactPlanReplay<PreparedCopiedScanoutPool>, CopiedScanoutError> {
    let QualifiedScanoutPlan::Copied { sink_id, plan } = qualified else {
        return Err(CopiedScanoutError::Candidates(io::Error::new(
            io::ErrorKind::InvalidInput,
            "copied replay received a shared qualification result",
        )));
    };
    if sink_id != destination_route.render_device_id {
        return Err(CopiedScanoutError::Candidates(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "copied replay sink {sink_id:?} does not match destination route {:?}",
                destination_route.render_device_id
            ),
        )));
    }

    let live_pool = match CopiedScanoutPool::allocate_exact(
        Arc::clone(&live_render_vk),
        Arc::clone(&live_sink_vk),
        Rc::clone(&scanout_device),
        route,
        destination_route,
        width,
        height,
        SCANOUT_POOL_DEPTH,
        scanout_modifiers,
        plan,
    ) {
        Ok(pool) => pool,
        Err(error) => {
            if crate::kms::vk::scanout::scanout_error_is_device_lost(&error) {
                return Err(CopiedScanoutError::LiveDeviceLost {
                    context: format!("{} live allocation", plan.describe()),
                    source: error,
                });
            }
            return Ok(ExactPlanReplay::Rejected(io::Error::new(
                error.kind(),
                format!("{} live allocation: {error}", plan.describe()),
            )));
        }
    };
    if let Err(error) = test_scanout_pool(&scanout_device, output, &live_pool.destinations) {
        return Ok(ExactPlanReplay::Rejected(io::Error::new(
            error.kind(),
            format!("{} live TEST_ONLY: {error}", plan.describe()),
        )));
    }

    let mut live_pool = live_pool;
    let committed_framebuffer = if commit_first_framebuffer {
        let (front_index, framebuffer) = live_pool
            .destinations
            .bos
            .iter()
            .enumerate()
            .find_map(|(index, bo)| bo.fb_handle.map(|framebuffer| (index, framebuffer)))
            .ok_or_else(|| {
                CopiedScanoutError::Candidates(io::Error::other(format!(
                    "{} live destination pool has no framebuffer",
                    plan.describe()
                )))
            })?;
        if let Err(error) =
            crate::drm::modeset::commit_modeset(&scanout_device, output, framebuffer)
        {
            return Ok(ExactPlanReplay::Rejected(io::Error::new(
                error.kind(),
                format!("{} live modeset: {error}", plan.describe()),
            )));
        }
        live_pool.destinations.bos[front_index]
            .state
            .mark_on_screen_after_modeset();
        live_pool
            .note_kms_modeset_installed(front_index)
            .map_err(CopiedScanoutError::Candidates)?;
        Some(framebuffer)
    } else {
        None
    };

    Ok(ExactPlanReplay::Prepared(PreparedCopiedScanoutPool {
        pool: live_pool,
        committed_framebuffer,
    }))
}

/// Preserve the synchronous copied candidate order while keeping disposable
/// qualification and live exact-plan replay as separate operations.
#[allow(clippy::too_many_arguments)]
pub(super) fn allocate_copied_scanout_pool(
    live_render_vk: Arc<VkContext>,
    live_sink_vk: Arc<VkContext>,
    scanout_device: Rc<drm::Device>,
    output: &crate::platform::drm::Output,
    route: ScanoutRoute,
    destination_route: ScanoutRoute,
    width: u32,
    height: u32,
    scanout_modifiers: &[u64],
    commit_first_framebuffer: bool,
) -> Result<PreparedCopiedScanoutPool, CopiedScanoutError> {
    debug_assert!(route_requires_copy_free_probe(route));
    debug_assert_eq!(destination_route.relationship, RenderKmsRelationship::Same);
    require_copied_sink_explicit_dmabuf_layout_import(live_sink_vk.image_drm_format_modifier)
        .map_err(CopiedScanoutError::Candidates)?;
    let plans = CopiedScanoutPool::exact_allocation_plans(
        &live_render_vk,
        &live_sink_vk,
        &scanout_device,
        width,
        scanout_modifiers,
    );
    let mut failures = Vec::new();

    for plan in plans {
        let probe_render_vk =
            match VkContext::new_disposable_for_same_physical_device(&live_render_vk) {
                Ok(vk) => vk,
                Err(error) => {
                    failures.push(format!(
                        "{} disposable source renderer: {error}",
                        plan.describe()
                    ));
                    continue;
                }
            };
        let probe_sink_vk =
            match VkContext::new_disposable_transfer_for_device(live_sink_vk.device_selector()) {
                Ok(vk) => vk,
                Err(error) => {
                    probe_render_vk.mark_disposable_probe_quiescent();
                    failures.push(format!(
                        "{} disposable sink renderer: {error}",
                        plan.describe()
                    ));
                    continue;
                }
            };
        let qualified = match qualify_copied_scanout_plan(
            probe_render_vk,
            probe_sink_vk,
            Rc::clone(&scanout_device),
            output,
            route,
            destination_route,
            width,
            height,
            scanout_modifiers,
            plan,
            PRIME_RENDER_PROBE_TIMEOUT_NS,
        ) {
            Ok(qualified) => qualified,
            Err(CopiedScanoutError::Candidates(error)) => {
                failures.push(error.to_string());
                continue;
            }
            Err(CopiedScanoutError::TerminalDisposableProbe(error)) => {
                let error_kind = error.kind();
                failures.push(error.to_string());
                return Err(CopiedScanoutError::TerminalDisposableProbe(io::Error::new(
                    error_kind,
                    format!(
                        "copied scanout probing stopped after a terminal disposable-probe \
                         failure: {}",
                        failures.join("; ")
                    ),
                )));
            }
            Err(error @ CopiedScanoutError::LiveDeviceLost { .. }) => return Err(error),
        };

        match replay_copied_scanout_plan(
            Arc::clone(&live_render_vk),
            Arc::clone(&live_sink_vk),
            Rc::clone(&scanout_device),
            output,
            route,
            destination_route,
            width,
            height,
            scanout_modifiers,
            qualified,
            commit_first_framebuffer,
        )? {
            ExactPlanReplay::Prepared(prepared) => {
                log::info!(
                    "copied scanout probe selected {} for {route:?}",
                    plan.describe()
                );
                return Ok(prepared);
            }
            ExactPlanReplay::Rejected(error) => {
                failures.push(error.to_string());
                continue;
            }
        }
    }

    Err(CopiedScanoutError::Candidates(io::Error::other(format!(
        "every copied scanout candidate failed for {route:?}: {}",
        failures.join("; ")
    ))))
}
