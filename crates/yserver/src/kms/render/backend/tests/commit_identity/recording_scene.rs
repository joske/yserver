use super::support::AtomicEventFixture;
use crate::{
    drm::atomic::tests_support::ScriptedResult,
    kms::{
        backend::ActiveOutput,
        render::{
            backend::KmsBackend, platform::flip_events::RecordingRenderCompletionReadiness,
            scene::SceneCompositor,
        },
        vk::scanout::{BoPhase, OutputScanout, ScanoutBo, ScanoutBoPool},
    },
    platform::drm::Output,
};
use std::{
    collections::HashMap,
    io,
    os::fd::RawFd,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};
use yserver_core::backend::Backend;

pub(crate) struct DirectPresentSetup {
    pub(crate) candidate: yserver_core::backend::PresentScanoutCandidate,
    pub(crate) event: yserver_core::backend::CompletedPresentEvent,
    pub(crate) source_id: crate::kms::render::store::DrawableId,
    pub(crate) target_xid: u32,
}

/// How the P1 scene fixture reaches KMS. These are test setup choices, not
/// production route or recovery types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum P1SceneRoute {
    /// The renderer draws straight into the scanout BOs.
    Shared,
    /// Renderer A copies into the sink's BOs; A and the sink share a KMS device.
    CopiedSameDevice,
    /// As above, but the sink's KMS device is a different DRM fd.
    CopiedSeparateDevice,
}

pub(crate) struct RecordingSceneFixture {
    pub(crate) events: AtomicEventFixture,
    /// Copied routes only: withholds delivery of real source completions.
    pub(crate) readiness: Option<Rc<RecordingRenderCompletionReadiness>>,
    extra_drawables: Vec<u32>,
}

impl RecordingSceneFixture {
    pub(crate) fn new(output_count: usize) -> Result<Option<Self>, String> {
        Self::with_route(output_count, P1SceneRoute::Shared)
    }

    /// Build the fixture for one of the scene routes. A copied route also
    /// installs a bystander (same-device) or the sink (separate-device) KMS
    /// device and the render-completion observer, before the recorders are
    /// attached to the devices.
    pub(crate) fn with_route(
        output_count: usize,
        route: P1SceneRoute,
    ) -> Result<Option<Self>, String> {
        let Some(mut backend) = build_recording_scene_backend(output_count, false, false)? else {
            return Ok(None);
        };
        let mut readiness = None;
        if route != P1SceneRoute::Shared {
            if output_count != 1 {
                return Err("copied P1 scene routes use one output".to_owned());
            }
            install_copied_route(&mut backend, route == P1SceneRoute::CopiedSeparateDevice)?;
            let observer = Rc::new(RecordingRenderCompletionReadiness::default());
            backend
                .platform
                .for_tests_set_render_completion_readiness(Rc::clone(&observer));
            readiness = Some(observer);
        }
        Ok(Some(Self {
            events: AtomicEventFixture::from_backend(backend),
            readiness,
            extra_drawables: Vec::new(),
        }))
    }

    pub(crate) fn new_with_identical_crtcs_on_two_devices() -> Result<Option<Self>, String> {
        let backend = build_recording_scene_backend(2, false, true)?;
        Ok(backend.map(|backend| Self {
            events: AtomicEventFixture::from_backend(backend),
            readiness: None,
            extra_drawables: Vec::new(),
        }))
    }

    /// Install a live root-sized Present source and fallback window before a
    /// direct lifecycle test begins. The framebuffer is synthetic, while the
    /// Vulkan images let the normal unflip materialization path run.
    pub(crate) fn install_direct_present(
        &mut self,
        present_id: u64,
        options: u32,
    ) -> DirectPresentSetup {
        use crate::kms::render::store::DrawableKind;
        use yserver_core::backend::{CompletedPresentEvent, PresentScanoutCandidate, PresentWake};

        let backend = &mut self.events.backend;
        let width = backend.platform.fb_w;
        let height = backend.platform.fb_h;
        let suffix = u32::try_from(present_id).expect("test Present id fits XID");
        let target_xid = 0x7300 + suffix * 0x10;
        let source_xid = target_xid + 1;
        let old_target = super::super::seed_window(backend, target_xid, None, 0, 0);
        let target = backend
            .windows
            .get_mut(&target_xid)
            .expect("direct fallback window");
        target.width = width;
        target.height = height;
        backend.core.top_level_order.push(target_xid);
        backend.store.detach_xid(target_xid);
        backend.store_decref_with_invalidate(old_target);
        let target_storage = backend
            .platform
            .gpu
            .allocate_drawable_storage(width, height, 24)
            .expect("allocate direct fallback image");
        backend
            .store
            .allocate(target_xid, DrawableKind::Window, 24, true, target_storage)
            .expect("register direct fallback image");
        let source_storage = backend
            .platform
            .gpu
            .allocate_drawable_storage(width, height, 24)
            .expect("allocate direct source image");
        let source_id = backend
            .store
            .allocate(source_xid, DrawableKind::Pixmap, 24, false, source_storage)
            .expect("register direct source image");
        Backend::put_image(backend, None, target_xid, 24, 1, 1, 0, 0, &[0, 0, 0, 0])
            .expect("initialize direct fallback image through Backend");
        Backend::put_image(backend, None, source_xid, 24, 1, 1, 0, 0, &[0, 0, 0, 0])
            .expect("initialize direct source image through Backend");

        let mut crtcs: Vec<_> = backend
            .platform
            .outputs
            .iter()
            .map(|layout| layout.output.crtc)
            .collect();
        crtcs.sort_by_key(|crtc| u32::from(*crtc));
        let shape = crate::drm::async_flip::AsyncShape {
            fourcc: 0x3432_5258,
            modifier: 0,
            pitch: u32::from(width) * 4,
            width: u32::from(width),
            height: u32::from(height),
            crtcs,
            direct: true,
        };
        backend.pstate.scanout_m1.entries.insert(
            source_id,
            crate::kms::render::backend::ScanoutM1ProbeEntry::accepted_with_shape(
                crate::drm::modeset::DirectScanoutProbeFramebuffer::for_tests(raw(0x8100 + suffix)),
                shape,
            ),
        );

        let crtc_id = u32::from(backend.platform.outputs[0].output.crtc);
        let crtc_epoch = backend.present_crtc_clock_epoch(crtc_id);
        let candidate = PresentScanoutCandidate {
            client_id: 1,
            present_id,
            crtc_id,
            crtc_epoch,
            src_pixmap_xid: source_xid,
            dst_window_xid: target_xid,
            src_host_xid: source_xid,
            paint_dst_host_xid: target_xid,
            completion_dst_host_xid: target_xid,
            src_width: width,
            src_height: height,
            x_off: 0,
            y_off: 0,
            valid_region_xid: 0,
            update_region_xid: 0,
            update_is_full: true,
            explicit_sync: false,
            options,
        };
        let event = CompletedPresentEvent {
            client_id: yserver_protocol::x11::ClientId(1),
            serial: u32::try_from(present_id).expect("test Present serial fits"),
            host_xid: source_xid,
            dst_host_xid: target_xid,
            options,
            present_id,
            window_generation: 0,
            crtc_id,
            crtc_epoch,
            msc_offset: 0,
            completion_clock: None,
            wake: PresentWake::Pixmap { idle_fence_xid: 0 },
            completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
            emit_idle: true,
        };
        self.extra_drawables.extend([target_xid, source_xid]);
        Backend::set_cursor_hidden(backend, true);
        DirectPresentSetup {
            candidate,
            event,
            source_id,
            target_xid,
        }
    }

    /// DRM fd of the KMS device that owns output `output_idx` (the sink for a
    /// copied route).
    pub(crate) fn kms_fd(&self, output_idx: usize) -> RawFd {
        let platform = &self.events.backend.platform;
        let key = &platform.outputs[output_idx].key;
        std::os::fd::AsRawFd::as_raw_fd(&std::os::fd::AsFd::as_fd(
            &platform
                .device_for_output(key)
                .expect("output has a KMS device")
                .device,
        ))
    }

    /// DRM fd of a live device that does not own output `output_idx`.
    pub(crate) fn other_fd(&self, output_idx: usize) -> RawFd {
        let own = self.kms_fd(output_idx);
        self.events
            .backend
            .platform
            .devices
            .iter()
            .map(|device| {
                std::os::fd::AsRawFd::as_raw_fd(&std::os::fd::AsFd::as_fd(&device.device))
            })
            .find(|&fd| fd != own)
            .expect("copied fixture routes keep a second device")
    }

    /// Copy ownership of every copied output slot (empty for shared routes):
    /// destination phase and ownership state plus retained wait semaphores.
    pub(crate) fn copy_ownership_observation(&self) -> Vec<String> {
        self.events
            .backend
            .platform
            .scanout_pools
            .iter()
            .filter_map(Option::as_ref)
            .filter_map(|scanout| match scanout {
                OutputScanout::Copied(pool) => Some(pool.for_tests_copy_ownership()),
                OutputScanout::Shared(_) => None,
            })
            .flatten()
            .collect()
    }

    /// Job ids of copied source renders whose completion is not yet delivered.
    pub(crate) fn pending_render_job_ids(&self) -> Vec<u64> {
        self.events
            .backend
            .platform
            .for_tests_pending_scanout_render_job_ids()
    }

    pub(crate) fn assert_quiescent(&self) {
        self.events.assert_quiescent();
        let backend = &self.events.backend;
        let direct = &backend.pstate.scanout_m2;
        // Mutation: retain direct/successor/unflip obligations or a source pin after normal completion.
        assert!(
            direct.pending.is_none(),
            "a direct submission is still pending"
        );
        assert!(
            direct.queued_successor.is_none(),
            "a direct successor is still queued"
        );
        assert!(direct.current.is_none(), "a direct frame is still held");
        assert!(
            direct.completed.is_empty(),
            "direct completions remain unpublished"
        );
        assert!(
            direct.deferred_successor_skips.is_empty(),
            "direct successor skip completions remain deferred"
        );
        assert!(
            direct.idled.is_empty(),
            "direct idle completions remain unpublished"
        );
        assert!(!direct.hold_direct, "direct ownership remains held");
        assert!(!direct.unflip_requested, "an unflip is still required");
        assert!(
            direct.unflip_outputs_to_replace.is_empty(),
            "outputs still await an unflip replacement"
        );
        assert!(
            direct.unflip_awaiting_outputs.is_empty(),
            "accepted unflip replacement identities remain"
        );
        assert!(
            direct.unflip_fallback_source.is_none(),
            "an unflip fallback source remains held"
        );
        assert!(!direct.unflip_shadow_ready, "unflip shadow remains armed");
        assert!(
            !direct.degraded_composed_unflip,
            "degraded composed unflip remains active"
        );
        assert!(
            backend.present_source_pins.is_empty(),
            "direct or fallback Present source pins remain held"
        );
        assert!(
            backend.pstate.pending_present_source_waits.is_empty(),
            "Present source waits remain pending"
        );
        assert_eq!(
            backend.scene.pending_pool_release_count_for_tests(),
            0,
            "recording scene has deferred descriptor-pool releases"
        );
        assert!(
            !backend.scene.has_pending_page_flips(),
            "recording scene has pending page-flip acknowledgements"
        );
        assert!(
            !backend.store.has_pending_presentation_damage(),
            "live drawable presentation damage remains after normal completion"
        );
        for row in self.copy_ownership_observation() {
            assert!(
                !(row.contains("phase=Free") && row.contains("ForeignPendingKms")),
                "a free copied slot still owes a KMS retirement: {row}"
            );
            // After a bounded normal drain no wait semaphore is retained and
            // no source slot is mid-handoff. Master legitimately keeps the
            // renderer-return fence of every slot the renderer has used
            // (`ForeignAwaitingRenderer` with a retained return completion,
            // consumed when the renderer next reuses the slot, either as an fd
            // or as Vulkan's already-signalled fd=-1 payload); a never-used
            // slot is `RendererFirstUse` with none.
            assert!(
                row.contains("sink_wait=false") && row.contains("renderer_wait=false"),
                "a copied slot retains a wait semaphore: {row}"
            );
            let awaiting_renderer = row.contains("source=Some(ForeignAwaitingRenderer)");
            let first_use = row.contains("source=Some(RendererFirstUse)");
            assert!(
                (awaiting_renderer
                    && (row.contains("return=fd") || row.contains("return=already-signalled")))
                    || (first_use && row.contains("return=none")),
                "a copied source slot is mid-handoff or its return completion is inconsistent: {row}"
            );
        }
        for output_idx in 0..backend.platform.outputs.len() {
            assert!(
                backend
                    .scene
                    .retirement_observation_for_tests(output_idx)
                    .starts_with("acks=[]"),
                "output {output_idx} keeps a scene acknowledgment or copy obligation"
            );
        }
        assert!(
            backend
                .platform
                .scanout_pools
                .iter()
                .filter_map(Option::as_ref)
                .flat_map(|scanout| scanout.display_pool().bos.iter())
                .all(|bo| matches!(bo.state.phase, BoPhase::Free | BoPhase::OnScreen)),
            "recording scene has an in-flight (Pending/Submitted/Recording) scanout BO"
        );
    }

    pub(crate) fn drain_deferred_pool_releases(&mut self, timeout: Duration) {
        if self
            .events
            .backend
            .scene
            .pending_pool_release_count_for_tests()
            == 0
            && self
                .events
                .backend
                .platform
                .for_tests_pending_scanout_render_job_ids()
                .is_empty()
            && !self.events.backend.store.has_pending_presentation_damage()
            && !self.events.backend.scene.scene_structure_dirty
            && !self.events.backend.scene.owes_repaint()
        {
            return;
        }
        let deadline = Instant::now() + timeout;
        let unmatched_before: HashMap<RawFd, u64> = self
            .events
            .backend
            .platform
            .devices
            .iter()
            .map(|device| {
                (
                    std::os::fd::AsRawFd::as_raw_fd(&std::os::fd::AsFd::as_fd(&device.device)),
                    device
                        .atomic_commit_sink
                        .for_tests_unmatched_page_flip_event_count(),
                )
            })
            .collect();
        let mut record_offsets: HashMap<RawFd, usize> = self
            .events
            .recorders
            .iter()
            .map(|(&fd, recorder)| (fd, recorder.records().len()))
            .collect();
        let mut submitted_frame = false;
        let mut scene_damage_requested = false;
        if let Some(readiness) = &self.readiness {
            readiness.resume_ready_jobs();
        }
        loop {
            if submitted_frame
                && self
                    .events
                    .backend
                    .scene
                    .pending_pool_release_count_for_tests()
                    == 0
                && !self.events.backend.scene.has_pending_page_flips()
                && self
                    .events
                    .backend
                    .platform
                    .for_tests_pending_scanout_render_job_ids()
                    .is_empty()
                && self.events.backend.platform.devices.iter().all(|device| {
                    device
                        .atomic_commit_sink
                        .for_tests_pending_page_flip_entry_count()
                        == 0
                })
            {
                break;
            }
            if Instant::now() >= deadline {
                break;
            }

            Backend::before_block(&mut self.events.backend);
            if !scene_damage_requested {
                let root = self.events.backend.core.window_id;
                let width = self.events.backend.platform.fb_w;
                let height = self.events.backend.platform.fb_h;
                Backend::clear_area(
                    &mut self.events.backend,
                    None,
                    root,
                    0,
                    None,
                    0,
                    0,
                    width,
                    height,
                    (0, 0),
                )
                .expect("damage root through Backend while draining scene pool releases");
                scene_damage_requested = true;
            }
            Backend::maybe_composite(&mut self.events.backend)
                .expect("drain deferred scene pool releases through a damaged Backend tick");
            // A copied frame submits to KMS only once its source render
            // completion is delivered; a shared frame has none (no-op).
            Backend::on_scanout_render_completion(&mut self.events.backend, &mut self.events.state);
            // A fresh damaged tick can submit a frame while it polls the prior
            // frame's deferred pool release. The descriptor-pool ring can
            // return NoPool while those releases are still pending, so keep
            // polling through these Backend entries without requiring a
            // submission on every pass. Retire each resulting event through
            // the normal backend entry before checking quiescence.
            let record_batches: Vec<_> = self
                .events
                .recorders
                .iter()
                .map(|(&fd, recorder)| (fd, recorder.records()))
                .collect();
            let mut completion_fds = Vec::new();
            let mut queued_events = 0;
            for (fd, records) in record_batches {
                let offset = record_offsets
                    .get_mut(&fd)
                    .expect("record offset exists for fixture device");
                let new_records = records.iter().skip(*offset);
                *offset = records.len();
                for record in new_records {
                    // A shared recorder appears under every fd, but each recorded ioctl belongs to one device only.
                    if record.device_fd != fd {
                        continue;
                    }
                    if record.result != ScriptedResult::Ok
                        || record
                            .flags
                            .contains(::drm::control::AtomicCommitFlags::TEST_ONLY)
                        || !record
                            .flags
                            .contains(::drm::control::AtomicCommitFlags::PAGE_FLIP_EVENT)
                    {
                        continue;
                    }
                    for event in &record.events {
                        self.events.queue_commit_completion(
                            fd,
                            event.crtc,
                            record.user_data,
                            52,
                            Duration::from_millis(1600),
                        );
                        queued_events += 1;
                    }
                    if !record.events.is_empty() {
                        completion_fds.push(fd);
                    }
                }
            }
            submitted_frame |= queued_events > 0;
            if queued_events > 0 {
                // Stand in for the kernel waiting on the frame's IN_FENCE_FD.
                self.events.wait_for_submitted_gpu_work();
            }
            completion_fds.sort_unstable();
            completion_fds.dedup();
            for fd in completion_fds {
                Backend::on_page_flip_ready(&mut self.events.backend, &mut self.events.state, fd);
            }
        }
        // Mutation: suppress root damage; this eventual-submission assertion
        // must fail because maybe_composite then skips scene.tick.
        assert!(
            submitted_frame,
            "a damaged Backend tick must eventually submit a frame for explicit completion"
        );
        assert_eq!(
            self.events
                .backend
                .scene
                .pending_pool_release_count_for_tests(),
            0,
            "deferred scene pool releases did not drain at a damaged Backend scene tick before the deadline"
        );
        // Mutation: omit the per-fd recorder filter so a shared recorder injects the other device's event.
        for device in &self.events.backend.platform.devices {
            let fd = std::os::fd::AsRawFd::as_raw_fd(&std::os::fd::AsFd::as_fd(&device.device));
            assert_eq!(
                device
                    .atomic_commit_sink
                    .for_tests_unmatched_page_flip_event_count(),
                unmatched_before[&fd],
                "normal drain attributed a recorder event to another DRM fd"
            );
        }
    }

    /// Free these pixmaps with the fixture's own at the end of
    /// [`Self::finish_after_normal_drain`].
    pub(crate) fn adopt_drawables(&mut self, xids: impl IntoIterator<Item = u32>) {
        self.extra_drawables.extend(xids);
    }

    pub(crate) fn finish(self) {
        let super::support::AtomicEventFixture { mut backend, .. } = self.events;
        // XBackend drops PlatformBackend before SceneCompositor. Hold the
        // Vulkan owner until the scene's child handles have dropped too.
        let vk_keepalive = backend.platform.gpu.vk.as_ref().map(Arc::clone);
        let scene = std::mem::replace(&mut backend.scene, SceneCompositor::stub());
        drop(backend);
        drop(scene);
        drop(vk_keepalive);
    }

    pub(crate) fn finish_after_normal_drain(mut self) {
        self.drain_deferred_pool_releases(Duration::from_secs(5));
        self.assert_quiescent();
        let root = self.events.backend.core.window_id;
        // The tearing entry selects a sprite cursor on the root. Release it
        // the way a client would. Master keeps a root DefineCursor as the
        // sticky server-wide default (`active_cursor`), so the sprite stays
        // referenced: assert that behaviour instead of forcing it away.
        if self.events.backend.cursor_records.contains_key(&0x7f10) {
            Backend::define_cursor(&mut self.events.backend, None, root, 0)
                .expect("deselect the fixture cursor through Backend");
            Backend::free_cursor(&mut self.events.backend, None, 0x7f10)
                .expect("free the fixture cursor through Backend");
            Backend::before_block(&mut self.events.backend);
            assert_eq!(
                self.events.backend.core.active_cursor,
                Some(0x7f10),
                "master keeps the root's cursor as the sticky default"
            );
            assert!(self.events.backend.cursor_records.contains_key(&0x7f10));
        }
        // Wait for the GPU work the normal drains observed as retired before
        // any drawable it referenced is freed.
        self.events.wait_for_submitted_gpu_work();
        for xid in [0x6a00, 0x6a01, 0x6a02, root, 0x6a10, 0x6a11, 0x6a12]
            .into_iter()
            .chain(self.extra_drawables.iter().copied())
        {
            if self.events.backend.store.lookup(xid).is_some() {
                Backend::free_pixmap(&mut self.events.backend, None, xid)
                    .expect("free recording scene drawable through Backend");
            }
        }
        for _ in 0..32 {
            Backend::before_block(&mut self.events.backend);
        }
        if self.readiness.is_some() {
            // Copied sink handoffs may enqueue queue work while the final
            // Backend reclaim passes run. Fence every source/sink queue after
            // those passes, then let Backend consume any completion notice.
            self.events.wait_for_submitted_gpu_work();
            Backend::on_scanout_render_completion(&mut self.events.backend, &mut self.events.state);
            assert!(self.pending_render_job_ids().is_empty());
            self.events.wait_for_submitted_gpu_work();
        }
        assert_eq!(self.events.backend.store.pending_retire_count(), 0);
        assert!(self.events.backend.store.lookup(root).is_none());
        self.events
            .backend
            .scene
            .drain_all(&mut self.events.backend.platform);
        self.finish();
    }
}

pub(in crate::kms::render::backend::tests) fn init_tearing_test_logger() {
    let _ = env_logger::Builder::new()
        .is_test(true)
        .filter_level(log::LevelFilter::Error)
        .filter_module("yserver", log::LevelFilter::Warn)
        .filter_module("yserver_render", log::LevelFilter::Warn)
        .try_init();
}

pub(in crate::kms::render::backend::tests) fn supports_vulkan_fed_scanout(
    supports_sync_fd: bool,
    supports_drm_format_modifier: bool,
) -> bool {
    supports_sync_fd && supports_drm_format_modifier
}

pub(in crate::kms::render::backend::tests) fn skip_unless_vulkan_fed_scanout(
    backend: &KmsBackend,
) -> bool {
    let supported = backend.platform.gpu.vk.as_ref().is_some_and(|vk| {
        supports_vulkan_fed_scanout(vk.supports_sync_fd(), vk.image_drm_format_modifier)
    });
    if !supported {
        eprintln!(
            "SKIP recording_scene: Vulkan device lacks SYNC_FD export or DRM format modifier support"
        );
    }
    supported
}

pub(in crate::kms::render::backend::tests) fn raw<H: ::drm::control::ResourceHandle>(
    value: u32,
) -> H {
    ::drm::control::from_u32(value).expect("nonzero test handle")
}

pub(in crate::kms::render::backend::tests) fn set_scene_fence_properties(output: &mut Output) {
    output.plane_in_fence_fd_prop = Some(raw(81));
    output.crtc_out_fence_ptr_prop = Some(raw(82));
}

pub(in crate::kms::render::backend::tests) fn set_unique_direct_plane_properties(
    output: &mut Output,
) {
    output.plane_fb_id_prop = raw(71);
    output.plane_crtc_id_prop = raw(72);
    output.plane_src_x_prop = raw(73);
    output.plane_src_y_prop = raw(74);
    output.plane_src_w_prop = raw(75);
    output.plane_src_h_prop = raw(76);
    output.plane_crtc_x_prop = raw(77);
    output.plane_crtc_y_prop = raw(78);
    output.plane_crtc_w_prop = raw(79);
    output.plane_crtc_h_prop = raw(80);
}

pub(in crate::kms::render::backend::tests) fn build_recording_scene_backend(
    output_count: usize,
    dead_zone: bool,
    second_output_uses_second_device: bool,
) -> Result<Option<KmsBackend>, String> {
    init_tearing_test_logger();
    let screen_width = match (output_count, dead_zone) {
        (2, true) => 1700,
        (2, false) => 1600,
        (1, false) => 800,
        _ => return Err("unsupported recording scene fixture topology".to_owned()),
    };
    let mut backend = match KmsBackend::for_tests_with_vk_dimensions(screen_width, 600) {
        Ok(backend) => backend,
        Err(error) if is_expected_vulkan_absence(&error) => {
            eprintln!("SKIP recording_scene: Vulkan ICD unavailable ({error})");
            return Ok(None);
        }
        Err(error) => return Err(format!("Vulkan fixture unavailable: {error}")),
    };
    if !skip_unless_vulkan_fed_scanout(&backend) {
        return Ok(None);
    }
    if output_count == 2 {
        if second_output_uses_second_device {
            let device_key = super::super::test_device_key(1);
            super::super::push_test_device(&mut backend, device_key);
            let route = crate::kms::scanout_route::ScanoutRoute::new(
                crate::kms::render::platform::RenderDeviceId::UnverifiedFallback,
                device_key,
                crate::kms::scanout_route::RenderKmsRelationship::Unknown,
            );
            backend.platform.outputs.push(ActiveOutput::new(
                route,
                super::support::test_output("test-device-1".to_owned()),
                crate::drm::Swapchain::empty_for_tests(),
                800,
                0,
            ));
        } else {
            super::super::push_test_output(&mut backend, 201);
        }
        if dead_zone {
            backend.platform.outputs[1].x = 900;
            backend.platform.outputs[1].height = 400;
            backend.platform.outputs[1].output.picked.height = 400;
            for mode in &mut backend.platform.outputs[1].output.modes {
                mode.height = 400;
            }
        } else {
            backend.platform.outputs[1].x = 800;
        }
    }
    let layouts = backend.platform.scene_output_layouts();
    backend.scene = SceneCompositor::new(&backend.platform, &layouts)
        .map_err(|error| format!("live SceneCompositor unavailable: {error:?}"))?;
    backend
        .platform
        .scanout_pools
        .resize_with(output_count, || None);
    backend
        .platform
        .bo_generations
        .resize_with(output_count, Vec::new);

    for output_idx in 0..output_count {
        let width = u32::from(backend.platform.outputs[output_idx].width);
        let height = u32::from(backend.platform.outputs[output_idx].height);
        let route = backend.platform.outputs[output_idx].scanout_route;
        let key = backend.platform.outputs[output_idx].key.clone();
        let output = &mut backend.platform.outputs[output_idx].output;
        set_scene_fence_properties(output);
        set_unique_direct_plane_properties(output);
        let index = u32::try_from(output_idx).expect("output index fits");
        let pool = recording_scanout_pool(
            &mut backend,
            &key,
            route,
            (width, height),
            0x6a00 + index * 0x10,
            0x5200 + index * 2,
        )?;
        backend.platform.bo_generations[output_idx] =
            vec![crate::kms::render::platform::BoGenerationEntry::default(); 2];
        backend.platform.scanout_pools[output_idx] = Some(pool);
    }
    for output in &backend.platform.outputs {
        backend
            .pstate
            .crtc_key_by_id
            .insert(u32::from(output.output.crtc), output.key.clone());
    }
    backend.refresh_present_crtc_clock_epochs();
    Ok(Some(backend))
}

/// Three Vulkan-backed compose BOs with synthetic framebuffer handles
/// `fb_base..fb_base + 3`, their images registered as pixmaps `xid_base..`.
/// The first BO starts on screen.
pub(in crate::kms::render::backend::tests) fn recording_scanout_pool(
    backend: &mut KmsBackend,
    key: &crate::kms::backend::OutputKey,
    route: crate::kms::scanout_route::ScanoutRoute,
    (width, height): (u32, u32),
    xid_base: u32,
    fb_base: u32,
) -> Result<OutputScanout, String> {
    let vk = Arc::clone(
        backend
            .platform
            .gpu
            .vk
            .as_ref()
            .expect("Vk-backed test fixture has a context"),
    );
    let drm = std::rc::Rc::clone(
        &backend
            .platform
            .device_for_key(key.device_key)
            .expect("test output has a KMS device")
            .device,
    );
    let mut bos = Vec::with_capacity(3);
    for bo_idx in 0..3 {
        let storage = backend
            .platform
            .gpu
            .allocate_drawable_storage(
                u16::try_from(width).expect("test width fits"),
                u16::try_from(height).expect("test height fits"),
                32,
            )
            .map_err(|error| format!("test target image allocation failed: {error:?}"))?;
        let image = storage.image;
        let image_view = storage.image_view;
        backend
            .store
            .allocate(
                xid_base + bo_idx,
                crate::kms::render::store::DrawableKind::Pixmap,
                32,
                false,
                storage,
            )
            .map_err(|error| format!("test target storage registration failed: {error:?}"))?;
        let mut bo = ScanoutBo::for_tests_compose_target(
            Arc::clone(&vk),
            std::rc::Rc::clone(&drm),
            raw(fb_base + bo_idx),
            width,
            height,
            image,
            image_view,
        )
        .map_err(|error| format!("test compose BO allocation failed: {error}"))?;
        if bo_idx == 0 {
            bo.state.phase = BoPhase::OnScreen;
        }
        bos.push(bo);
    }
    Ok(OutputScanout::Shared(
        ScanoutBoPool::for_tests_recording_pool(route, width, height, bos),
    ))
}

/// Convert the shared recording pools to copied pools (A copies into the
/// recording sink BOs). A second KMS device is always installed: the sink
/// itself for the separate-device route, an unrelated bystander otherwise, so
/// tests can inject events on a device that does not own the output.
fn install_copied_route(backend: &mut KmsBackend, separate_sink: bool) -> Result<(), String> {
    let other_key = super::super::test_device_key(1);
    super::super::push_test_device(backend, other_key);
    let render_device_id = backend.platform.outputs[0].scanout_route.render_device_id;
    let sink_key = if separate_sink {
        other_key
    } else {
        backend.platform.outputs[0].key.device_key
    };
    let route = crate::kms::scanout_route::ScanoutRoute::new(
        render_device_id,
        sink_key,
        crate::kms::scanout_route::RenderKmsRelationship::Different,
    );
    for output in &mut backend.platform.outputs {
        output.key.device_key = sink_key;
        output.scanout_route = route;
    }
    for output in &backend.platform.outputs {
        backend
            .pstate
            .crtc_key_by_id
            .insert(u32::from(output.output.crtc), output.key.clone());
    }
    backend.refresh_present_crtc_clock_epochs();
    let vk = Arc::clone(
        backend
            .platform
            .gpu
            .vk
            .as_ref()
            .expect("Vk-backed test fixture has a context"),
    );
    for output_idx in 0..backend.platform.scanout_pools.len() {
        let Some(Some(OutputScanout::Shared(mut destinations))) = backend
            .platform
            .scanout_pools
            .get_mut(output_idx)
            .map(Option::take)
        else {
            return Err(format!(
                "copied route requires a shared destination pool for output {output_idx}"
            ));
        };
        destinations.route = route;
        let mut copied = crate::kms::vk::scanout::CopiedScanoutPool::for_tests_recording_pool(
            Arc::clone(&vk),
            destinations,
        )
        .map_err(|error| format!("allocate copied recording route: {error}"))?;
        if let Some(front_idx) = copied
            .destinations
            .bos
            .iter()
            .position(|bo| bo.state.phase == BoPhase::OnScreen)
        {
            copied
                .note_kms_modeset_installed(front_idx)
                .map_err(|error| format!("record copied fixture front ownership: {error}"))?;
        }
        backend.platform.scanout_pools[output_idx] = Some(OutputScanout::Copied(copied));
    }
    let layouts = backend.platform.scene_output_layouts();
    backend.scene = SceneCompositor::new(&backend.platform, &layouts)
        .map_err(|error| format!("rebuild copied route scene: {error:?}"))?;
    Ok(())
}

fn is_expected_vulkan_absence(error: &io::Error) -> bool {
    crate::kms::render::backend::tests::vulkan_absence_is_expected(error)
}
