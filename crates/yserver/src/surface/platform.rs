//! [`SurfacePlatform`]: the scene's output sink and the backend's platform
//! for window-system surfaces. It has no surfaces yet, so it composes over
//! no outputs and the hooks it overrides are the platform-neutral ones.

use std::{io, os::fd::OwnedFd, sync::Arc};

use ash::vk;
use yserver_core::core_loop::{CoreSender, Message};

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
    vk::{device::VkContext, ops::OpsCommandPool, pixmap_pool::PixmapPool},
};

/// The surface platform: the GPU and the logical screen size.
pub(crate) struct SurfacePlatform {
    gpu: GpuCore,
    fb_size: (u16, u16),
}

/// Backend state the surface platform keeps next to the portable half.
#[derive(Default)]
pub(crate) struct SurfaceBackendState {
    /// The core channel, for asking the core loop to shut down.
    core_sender: Option<CoreSender>,
    /// A renderer loss already requested the shutdown.
    shutdown_requested: bool,
}

impl SurfacePlatform {
    #[must_use]
    pub(crate) fn new(gpu: GpuCore, fb_size: (u16, u16)) -> Self {
        Self { gpu, fb_size }
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
    let ops_command_pool = OpsCommandPool::new(Arc::clone(&vk))
        .map_err(|e| io::Error::other(format!("ops command pool: {e:?}")))?;
    let fence_pool = FencePool::new(Arc::clone(&vk));
    let pixmap_pool = Arc::new(PixmapPool::new(Arc::clone(&vk)));
    crate::kms::vk::pixmap_pool::register_for_telemetry(&pixmap_pool);
    let gpu = GpuCore::new(vk, ops_command_pool, fence_pool, Some(pixmap_pool));
    let platform = SurfacePlatform::new(gpu, fb_size);

    let mut core = KmsCore::new(fb_size.0, fb_size.1, layout)?;
    core.cursor_x = f32::from(fb_size.0 / 2);
    core.cursor_y = f32::from(fb_size.1 / 2);
    let engine = RenderEngine::new(&platform.gpu)
        .map_err(|e| io::Error::other(format!("render RenderEngine::new failed: {e:?}")))?;
    let scene = SceneCompositor::new(&platform, &[])
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

    fn route(&self, _output_idx: usize) -> OutputRoute {
        OutputRoute::None
    }

    fn output_commit_in_flight(&self, _output_idx: usize) -> bool {
        false
    }

    fn slot_count(&self, _output_idx: usize) -> Option<usize> {
        None
    }

    fn acquire_slot(&mut self, _output_idx: usize) -> Option<SlotToken> {
        None
    }

    fn slot_image(&self, _output_idx: usize, _bo_idx: usize) -> Option<vk::Image> {
        None
    }

    fn check_frame_target(&self, _output_idx: usize) -> Result<(), FrameTargetError> {
        Err(FrameTargetError::NoSlot)
    }

    fn finish_frame(
        &mut self,
        _output_idx: usize,
        _bo_idx: usize,
        _record: &mut RecordFrame<'_>,
    ) -> FinishOutcome {
        FinishOutcome::Unavailable(FrameTargetError::NoSlot)
    }

    fn invalidate_slot(&mut self, _output_idx: usize, _bo_idx: usize) {}

    fn cancel_slot(&mut self, _output_idx: usize, _bo_idx: usize) {}

    fn recycle_failed_slot(&mut self, _output_idx: usize, _bo_idx: usize) -> io::Result<()> {
        Ok(())
    }

    fn on_page_flip_complete(&mut self, _output_idx: usize) -> Option<FlipRetirement> {
        None
    }

    fn commit_slot_present(&mut self, _output_idx: usize, _bo_idx: usize, _generation: u64) {}

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

impl Platform for SurfacePlatform {
    type BackendState = SurfaceBackendState;

    fn fb_size(&self) -> (u16, u16) {
        self.fb_size
    }

    fn output_layouts(&self) -> Vec<OutputLayout> {
        Vec::new()
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
        b.gpu_work_deadline(std::time::Instant::now())
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
