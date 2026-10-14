//! [`SurfacePlatform`]: the scene's output sink and the backend's platform
//! for window-system surfaces. Output `i` is surface `i`; with no surface the
//! server is headless and composes over no outputs.

use std::{io, os::fd::OwnedFd, sync::Arc};

use ash::vk;
use yserver_core::{
    backend::Backend as _,
    core_loop::{CoreSender, Message},
    server::ServerState,
};

use super::{
    input::translate::caps_lock_sync,
    link::{ShellEvent, ShellLink},
    swapchain::SurfaceOutput,
};
use crate::kms::{
    core::KmsCore,
    render::{
        Platform, XBackend,
        engine::RenderEngine,
        gpu::{FencePool, GpuCore},
        scene::{
            CursorPlaneSink, CursorShowFailure, FinishOutcome, FlipRetirement, FrameTargetError,
            OutputLayout, OutputRoute, OutputSink, RecordFrame, RenderCompletion, SceneCompositor,
            SlotToken,
        },
    },
    vk::{
        device::VkContext,
        ops::{OpsCommandPool, OpsStaging},
        pixmap_pool::PixmapPool,
    },
};

/// The surface platform: the GPU, the logical screen size and the surfaces.
pub(crate) struct SurfacePlatform {
    gpu: GpuCore,
    fb_size: (u16, u16),
    pub(crate) outputs: Vec<SurfaceOutput>,
}

/// Backend state the surface platform keeps next to the portable half.
#[derive(Default)]
pub(crate) struct SurfaceBackendState {
    /// The core channel, for asking the core loop to shut down.
    core_sender: Option<CoreSender>,
    /// A renderer loss already requested the shutdown.
    shutdown_requested: bool,
    /// The window shell's channel, when a shell drives this server.
    pub(crate) link: Option<Arc<ShellLink>>,
    /// Host-visible staging for on-screen reads.
    readback: Option<OpsStaging>,
}

impl SurfacePlatform {
    #[must_use]
    pub(crate) fn new(gpu: GpuCore, fb_size: (u16, u16), outputs: Vec<SurfaceOutput>) -> Self {
        Self {
            gpu,
            fb_size,
            outputs,
        }
    }
}

/// A backend over a surface platform with no surfaces: everything but
/// presentation works, nothing is shown.
///
/// # Errors
///
/// The command pool, render engine, scene or keymap could not be created.
pub(crate) fn headless_backend(
    vk: Arc<VkContext>,
    fb_size: (u16, u16),
    layout: Option<String>,
) -> io::Result<XBackend<SurfacePlatform>> {
    surface_backend(vk, fb_size, layout, Vec::new())
}

/// A backend presenting the screen on `outputs`, each showing the whole root.
///
/// # Errors
///
/// The command pool, render engine, scene or keymap could not be created.
pub(crate) fn surface_backend(
    vk: Arc<VkContext>,
    fb_size: (u16, u16),
    layout: Option<String>,
    outputs: Vec<SurfaceOutput>,
) -> io::Result<XBackend<SurfacePlatform>> {
    let ops_command_pool = OpsCommandPool::new(Arc::clone(&vk))
        .map_err(|e| io::Error::other(format!("ops command pool: {e:?}")))?;
    let fence_pool = FencePool::new(Arc::clone(&vk));
    let pixmap_pool = Arc::new(PixmapPool::new(Arc::clone(&vk)));
    crate::kms::vk::pixmap_pool::register_for_telemetry(&pixmap_pool);
    let gpu = GpuCore::new(vk, ops_command_pool, fence_pool, Some(pixmap_pool));
    let platform = SurfacePlatform::new(gpu, fb_size, outputs);

    let mut core = KmsCore::new(fb_size.0, fb_size.1, layout)?;
    core.cursor_x = f32::from(fb_size.0 / 2);
    core.cursor_y = f32::from(fb_size.1 / 2);
    let engine = RenderEngine::new(&platform.gpu)
        .map_err(|e| io::Error::other(format!("render RenderEngine::new failed: {e:?}")))?;
    let scene = SceneCompositor::new(&platform, &platform.output_layouts())
        .map_err(|e| io::Error::other(format!("render SceneCompositor::new failed: {e:?}")))?;
    let mut backend = XBackend::assemble(
        core,
        platform,
        engine,
        scene,
        SurfaceBackendState::default(),
        true,
    );
    backend.init_root_storage();
    if let Err(e) = backend.init_cursor_sprite() {
        log::warn!("surface: software cursor init failed: {e:?}");
    }
    Ok(backend)
}

impl OutputSink for SurfacePlatform {
    fn gpu(&self) -> &GpuCore {
        &self.gpu
    }

    fn gpu_mut(&mut self) -> &mut GpuCore {
        &mut self.gpu
    }

    fn cursor_plane(&self) -> &dyn CursorPlaneSink {
        self
    }

    fn cursor_plane_mut(&mut self) -> &mut dyn CursorPlaneSink {
        self
    }

    fn route(&self, output_idx: usize) -> OutputRoute {
        if output_idx < self.outputs.len() {
            OutputRoute::Shared
        } else {
            OutputRoute::None
        }
    }

    fn output_commit_in_flight(&self, _output_idx: usize) -> bool {
        false
    }

    fn slot_count(&self, output_idx: usize) -> Option<usize> {
        self.outputs.get(output_idx).map(SurfaceOutput::slot_count)
    }

    fn acquire_slot(&mut self, output_idx: usize) -> Option<SlotToken> {
        self.outputs.get_mut(output_idx)?.acquire_slot(output_idx)
    }

    fn slot_image(&self, output_idx: usize, bo_idx: usize) -> Option<vk::Image> {
        self.outputs.get(output_idx)?.slot_image(bo_idx)
    }

    fn check_frame_target(&self, output_idx: usize) -> Result<(), FrameTargetError> {
        match self.outputs.get(output_idx) {
            Some(_) => Ok(()),
            None => Err(FrameTargetError::NoSlot),
        }
    }

    fn finish_frame(
        &mut self,
        output_idx: usize,
        bo_idx: usize,
        record: &mut RecordFrame<'_>,
    ) -> FinishOutcome {
        match self.outputs.get_mut(output_idx) {
            Some(output) => output.finish_frame(bo_idx, record),
            None => FinishOutcome::Unavailable(FrameTargetError::NoSlot),
        }
    }

    fn invalidate_slot(&mut self, output_idx: usize, bo_idx: usize) {
        if let Some(output) = self.outputs.get_mut(output_idx) {
            output.invalidate_slot(bo_idx);
        }
    }

    fn cancel_slot(&mut self, output_idx: usize, bo_idx: usize) {
        if let Some(output) = self.outputs.get_mut(output_idx) {
            output.release_slot(bo_idx);
        }
    }

    fn recycle_failed_slot(&mut self, output_idx: usize, bo_idx: usize) -> io::Result<()> {
        if let Some(output) = self.outputs.get_mut(output_idx) {
            output.release_slot(bo_idx);
        }
        Ok(())
    }

    fn on_page_flip_complete(&mut self, output_idx: usize) -> Option<FlipRetirement> {
        self.outputs.get_mut(output_idx)?.on_page_flip_complete()
    }

    fn commit_slot_present(&mut self, output_idx: usize, bo_idx: usize, generation: u64) {
        if let Some(output) = self.outputs.get_mut(output_idx) {
            output.commit_slot_present(bo_idx, generation);
        }
    }

    fn register_render_completion(
        &mut self,
        _output_idx: usize,
        _bo_idx: usize,
        _fd: Option<OwnedFd>,
    ) -> io::Result<u64> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }

    fn drain_render_completions(&mut self) -> Vec<RenderCompletion> {
        Vec::new()
    }

    fn clear_render_completions(&mut self) {}

    fn submit_copied(
        &mut self,
        _output_idx: usize,
        _bo_idx: usize,
        _fd: Option<OwnedFd>,
    ) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
}

/// No hardware cursor plane: the scene paints the cursor.
impl CursorPlaneSink for SurfacePlatform {
    fn fits(&self, _output_idx: usize, _width: u32, _height: u32) -> bool {
        false
    }

    fn visible(&self, _output_idx: usize) -> bool {
        false
    }

    fn uploaded_version(&self, _output_idx: usize) -> Option<u64> {
        None
    }

    fn upload(
        &mut self,
        _output_idx: usize,
        _version: u64,
        _width: u32,
        _height: u32,
        _bgra_bytes: &[u8],
    ) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }

    fn show(
        &mut self,
        _output_idx: usize,
        _hot_x: u16,
        _hot_y: u16,
        _x: i32,
        _y: i32,
    ) -> Result<(), CursorShowFailure> {
        Err(CursorShowFailure {
            remains_visible: false,
            error: "no cursor plane".into(),
        })
    }

    fn hide(&mut self, _output_idx: usize) -> io::Result<()> {
        Ok(())
    }

    fn hide_all(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn note_composed_retirement(&mut self, _output_idx: usize) -> bool {
        false
    }
}

impl XBackend<SurfacePlatform> {
    /// Present composed frames that still wait for a swapchain image and
    /// retire the ones whose copy finished; re-arm the tick while any frame
    /// is pending.
    pub(crate) fn service_surfaces(&mut self) {
        let mut retired_any = false;
        for idx in 0..self.platform.outputs.len() {
            if let Err(e) = self.platform.outputs[idx].present_pending() {
                log::warn!("surface {idx}: present failed: {e:?}");
                if e == vk::Result::ERROR_DEVICE_LOST {
                    self.platform.gpu.renderer_failed = true;
                }
            }
            if self.platform.outputs[idx].retirable()
                && self
                    .scene
                    .handle_page_flip_complete(idx, &mut self.store, &mut self.platform)
            {
                self.telemetry.record_frame_present();
                retired_any = true;
            }
            let output = &self.platform.outputs[idx];
            if output.has_pending() {
                output.clock().arm_tick();
            }
        }
        if retired_any {
            self.reclaim_retired_gpu_work();
            if let Err(e) = self.engine.flush_render_batch(
                &mut self.store,
                &mut self.platform.gpu,
                crate::kms::render::engine::RenderFlushReason::Present,
            ) {
                log::warn!("surface: flush_render_batch failed: {e:?}");
            }
            if let Err(e) = self.engine.flush_submit_group(
                &mut self.store,
                &mut self.platform.gpu,
                crate::kms::render::submit_group::FlushReason::PageflipRetire,
            ) {
                log::warn!("surface: flush_submit_group failed: {e:?}");
            }
        }
    }

    /// Apply the shell's events.
    fn apply_shell_events(&mut self, events: Vec<ShellEvent>) {
        for event in events {
            match event {
                ShellEvent::Visible(visible) => {
                    let mut shown = false;
                    for output in &mut self.platform.outputs {
                        shown |= visible && output.hidden();
                        output.set_hidden(!visible);
                    }
                    log::debug!("surface: window visible={visible}");
                    if shown {
                        // The swapchain missed every frame while hidden.
                        self.scene.invalidate_all_scanout_damage();
                        self.scene.wake_for_damage();
                    }
                }
                ShellEvent::LockState { caps } => self.sync_caps_lock(caps),
            }
        }
    }

    /// Toggle the X Caps Lock through the input channel when it differs
    /// from the window system's `host_caps`.
    fn sync_caps_lock(&mut self, host_caps: bool) {
        const LOCK_MASK: u8 = 0x02;
        let x_caps = self.current_xkb_mods().3 & LOCK_MASK != 0;
        let Some(sender) = self.pstate.core_sender.as_ref() else {
            return;
        };
        let time = crate::clock::server_time_ms();
        for event in caps_lock_sync(x_caps, host_caps, time) {
            let _ = sender.send(Message::HostInput(event));
        }
    }

    /// Whether some surface can show a frame.
    fn any_surface_visible(&self) -> bool {
        self.platform.outputs.iter().any(|o| !o.hidden())
    }

    /// Read `rect` of output `output_idx`'s `image` (left `GENERAL`).
    pub(super) fn read_image_region(
        &mut self,
        image: vk::Image,
        local: vk::Rect2D,
    ) -> io::Result<Vec<u8>> {
        let vk = self
            .platform
            .gpu
            .vk
            .clone()
            .ok_or_else(|| io::Error::other("no vulkan context"))?;
        let pool = self
            .platform
            .gpu
            .ops_command_pool_handle()
            .ok_or_else(|| io::Error::other("no ops command pool"))?;
        let bytes = u64::from(local.extent.width) * u64::from(local.extent.height) * 4;
        if self.pstate.readback.is_none() {
            self.pstate.readback = Some(
                OpsStaging::new(Arc::clone(&vk), bytes.max(1))
                    .map_err(|e| io::Error::other(format!("readback staging: {e:?}")))?,
            );
        }
        let staging = self.pstate.readback.as_mut().expect("just ensured");
        staging
            .ensure(bytes)
            .map_err(|e| io::Error::other(format!("readback staging: {e:?}")))?;
        let buffer = staging.buffer();
        crate::kms::vk::ops::run_one_shot_op(&vk, pool, |vk, cb| {
            record_general_image_read(vk, cb, image, buffer, local);
            Ok(())
        })
        .map_err(|e| io::Error::other(format!("surface readback: {e:?}")))?;
        let len = usize::try_from(bytes).map_err(io::Error::other)?;
        // SAFETY: the staging buffer is host-coherent, at least `len` bytes,
        // and the copy's fence has signalled.
        Ok(unsafe { std::slice::from_raw_parts(staging.mapped_ptr(), len) }.to_vec())
    }
}

/// Copy `local` of a `GENERAL` image into `buffer`, leaving it `GENERAL`.
fn record_general_image_read(
    vk: &VkContext,
    cb: vk::CommandBuffer,
    image: vk::Image,
    buffer: vk::Buffer,
    local: vk::Rect2D,
) {
    let range = vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1);
    let before = [vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
        .src_access_mask(vk::AccessFlags2::MEMORY_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::COPY)
        .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
        .old_layout(vk::ImageLayout::GENERAL)
        .new_layout(vk::ImageLayout::GENERAL)
        .image(image)
        .subresource_range(range)];
    let region = [vk::BufferImageCopy::default()
        .image_subresource(
            vk::ImageSubresourceLayers::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .layer_count(1),
        )
        .image_offset(vk::Offset3D {
            x: local.offset.x,
            y: local.offset.y,
            z: 0,
        })
        .image_extent(vk::Extent3D {
            width: local.extent.width,
            height: local.extent.height,
            depth: 1,
        })];
    let after = [vk::BufferMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COPY)
        .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::HOST)
        .dst_access_mask(vk::AccessFlags2::HOST_READ)
        .buffer(buffer)
        .size(vk::WHOLE_SIZE)];
    unsafe {
        vk.device.cmd_pipeline_barrier2(
            cb,
            &vk::DependencyInfo::default().image_memory_barriers(&before),
        );
        vk.device
            .cmd_copy_image_to_buffer(cb, image, vk::ImageLayout::GENERAL, buffer, &region);
        vk.device.cmd_pipeline_barrier2(
            cb,
            &vk::DependencyInfo::default().buffer_memory_barriers(&after),
        );
    }
}

impl Platform for SurfacePlatform {
    type BackendState = SurfaceBackendState;

    fn fb_size(&self) -> (u16, u16) {
        self.fb_size
    }

    fn output_layouts(&self) -> Vec<OutputLayout> {
        let (width, height) = self.fb_size;
        self.outputs
            .iter()
            .map(|_| OutputLayout {
                root_rect: (0, 0, u32::from(width), u32::from(height)),
                mode: (width, height),
                transform: None,
            })
            .collect()
    }

    fn set_input_sender(b: &mut XBackend<Self>, sender: CoreSender) {
        b.pstate.core_sender = Some(sender);
    }

    fn before_block(b: &mut XBackend<Self>) {
        b.reclaim_retired_gpu_work();
    }

    fn flush_before_damage_notify(b: &mut XBackend<Self>) {
        b.flush_before_damage_exposure();
    }

    fn next_wakeup(b: &XBackend<Self>) -> Option<std::time::Instant> {
        let now = std::time::Instant::now();
        let scene = (b.any_surface_visible() && b.scene_wants_scene())
            .then(|| {
                if b.scene.has_output_ready_for_submit(&b.platform) {
                    Some(now)
                } else {
                    b.scene.earliest_retry_deadline()
                }
            })
            .flatten();
        b.gpu_work_deadline(now).into_iter().chain(scene).min()
    }

    fn on_platform_event(b: &mut XBackend<Self>, _state: &mut ServerState) {
        if let Some(link) = b.pstate.link.as_ref() {
            let events = link.drain();
            b.apply_shell_events(events);
        }
        b.service_surfaces();
    }

    fn randr_outputs_and_modes(
        b: &mut XBackend<Self>,
    ) -> (
        Vec<yserver_core::randr::RandrOutput>,
        Vec<yserver_core::randr::RandrMode>,
    ) {
        b.surface_randr_outputs_and_modes()
    }

    fn present_get_ust_msc(b: &XBackend<Self>, _crtc_id: u32) -> (u64, u64) {
        b.surface_ust_msc()
    }

    fn arm_idle_vblanks(
        b: &mut XBackend<Self>,
        _crtc_id: u32,
        target_mscs: &[u64],
    ) -> io::Result<usize> {
        b.arm_surface_msc(target_mscs)
    }

    fn arm_present_completion_idle_vblanks(
        b: &mut XBackend<Self>,
        _crtc_id: u32,
        target_mscs: &[u64],
    ) -> io::Result<usize> {
        b.arm_surface_msc(target_mscs)
    }

    fn present_flip_in_flight(b: &XBackend<Self>, _crtc_id: u32) -> bool {
        b.scene.has_pending_page_flips()
    }

    fn present_display_idle(b: &XBackend<Self>, _crtc_id: u32) -> bool {
        b.surface_display_idle()
    }

    fn read_on_screen_region(b: &mut XBackend<Self>, rect: vk::Rect2D) -> io::Result<Vec<u8>> {
        if b.platform.outputs.is_empty() {
            return Err(io::Error::other("no on-screen output to read"));
        }
        let image = match b.fresh_root_readback(0, rect) {
            Some(image) => image,
            None => b.platform.outputs[0]
                .on_screen_image()
                .ok_or_else(|| io::Error::other("no frame on screen yet"))?,
        };
        b.read_image_region(image, rect)
    }

    fn maybe_composite(b: &mut XBackend<Self>) -> io::Result<()> {
        if b.platform.gpu.renderer_failed {
            // A lost device does not come back in-process.
            if !b.pstate.shutdown_requested {
                b.pstate.shutdown_requested = true;
                log::error!("surface: renderer device lost — requesting shutdown");
                if let Some(sender) = b.pstate.core_sender.as_ref() {
                    let _ = sender.send(Message::Shutdown);
                }
            }
            return Ok(());
        }
        if let Err(e) = b
            .engine
            .close_open_frame_if_timed_out(&mut b.store, b.platform.gpu_mut())
        {
            log::warn!("surface maybe_composite: timeout close failed: {e:?}");
        }
        b.compose_tick();
        b.drain_paint_submit_telemetry();
        Ok(())
    }
}
