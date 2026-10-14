//! [`SurfacePlatform`]: the scene's output sink and the backend's platform
//! for window-system surfaces. Output `i` is surface `i`; with no surface the
//! server is headless and composes over no outputs.

use std::{
    io,
    os::fd::OwnedFd,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use ash::vk;
use yserver_core::{
    backend::Backend as _,
    core_loop::{CoreSender, Message, host_screen_resized, run::emit_randr_change_notifications},
    randr::{RR_ROTATE_0, RandrLimits},
    server::ServerState,
};

use super::{
    input::translate::caps_lock_sync,
    link::{ShellEvent, ShellLink},
    resize::MIN_SCREEN,
    swapchain::SurfaceOutput,
    ui::{CursorImage, ShellUi},
};
use crate::kms::{
    core::KmsCore,
    render::{
        Platform, ResizeFault, XBackend,
        engine::RenderEngine,
        gpu::{FencePool, GpuCore},
        resize_fault,
        scene::{
            CursorPlaneSink, CursorShowFailure, FinishOutcome, FlipRetirement, FrameTargetError,
            OffscreenTarget, OutputLayout, OutputRoute, OutputSink, PreparedOutputs, RecordFrame,
            RenderCompletion, SceneCompositor, SlotToken,
        },
    },
    vk::{
        device::VkContext,
        ops::{OpsCommandPool, OpsStaging},
        pixmap_pool::PixmapPool,
    },
};

/// The surface platform: the GPU, the root and output sizes and the
/// surfaces.
pub(crate) struct SurfacePlatform {
    gpu: GpuCore,
    /// The root (X screen) size: root and COW storage.
    fb_size: (u16, u16),
    /// The window output's size (its CRTC mode): the slots and swapchain.
    /// The root's top-left part of this size is shown.
    pub(crate) output_size: (u16, u16),
    /// The largest output the window system shows unclamped: the mode list
    /// offers only modes that fit.
    pub(crate) max_output: (u16, u16),
    /// The window output's physical size in millimetres; 0: unknown.
    pub(crate) output_mm: (u32, u32),
    /// The largest screen side the GPU takes (`RRGetScreenSizeRange`).
    max_screen: u16,
    pub(crate) outputs: Vec<SurfaceOutput>,
    cursor: HostCursor,
}

/// The X cursor as the window system shows it: the scene's cursor plane.
#[derive(Default)]
struct HostCursor {
    /// The latest uploaded image, with the hotspot of the latest show.
    image: Option<CursorImage>,
    shown: bool,
    /// What the shell was last told: `(version, hotspot)`, or hidden (and
    /// at startup, where the shell shows its own cursor until the first).
    sent: Option<(u64, (u16, u16))>,
}

impl HostCursor {
    fn wanted(&self) -> Option<(u64, (u16, u16))> {
        self.image
            .as_ref()
            .filter(|_| self.shown)
            .map(|image| (image.version, image.hot))
    }
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
    /// The window shell's UI commands.
    pub(crate) ui: Option<Arc<dyn ShellUi>>,
    /// Established X clients, for the shell's quit confirmation.
    pub(crate) client_count: Arc<AtomicUsize>,
    /// Host-visible staging for on-screen reads.
    readback: Option<OpsStaging>,
    /// The token of the window resize last asked of the shell, until the
    /// shell reports it done.
    window_request: Option<u64>,
    next_window_request: u64,
}

impl SurfacePlatform {
    #[must_use]
    pub(crate) fn new(gpu: GpuCore, fb_size: (u16, u16), outputs: Vec<SurfaceOutput>) -> Self {
        let max_screen = gpu.vk.as_ref().map_or(MAX_SCREEN_SIDE, |vk| {
            // SAFETY: a physical device of this instance.
            let limits = unsafe {
                vk.instance
                    .get_physical_device_properties(vk.physical_device)
                    .limits
            };
            u16::try_from(
                limits
                    .max_image_dimension2_d
                    .min(u32::from(MAX_SCREEN_SIDE)),
            )
            .unwrap_or(MAX_SCREEN_SIDE)
        });
        Self {
            gpu,
            fb_size,
            output_size: fb_size,
            max_output: (max_screen, max_screen),
            output_mm: (0, 0),
            max_screen,
            outputs,
            cursor: HostCursor::default(),
        }
    }

    /// The largest output the window system shows unclamped; the mode list
    /// offers only modes that fit it.
    #[cfg_attr(kms, allow(dead_code))] // Called by `server`, built without KMS.
    pub(crate) fn set_max_output(&mut self, (width, height): (u16, u16)) {
        self.max_output = (width.min(self.max_screen), height.min(self.max_screen));
    }

    /// One layout per surface, each showing the root's top-left `size`.
    fn layouts_at(&self, (width, height): (u16, u16)) -> Vec<OutputLayout> {
        self.outputs
            .iter()
            .map(|_| OutputLayout {
                root_rect: (0, 0, u32::from(width), u32::from(height)),
                mode: (width, height),
                transform: None,
            })
            .collect()
    }

    /// The RandR limits: a CRTC without rotations or transforms, and
    /// screens from 320x200 up to the GPU's image limit (at most 8192).
    pub(crate) fn randr_limits(&self) -> RandrLimits {
        RandrLimits {
            rotations: RR_ROTATE_0,
            transforms: false,
            min_size: MIN_SCREEN,
            max_size: (self.max_screen, self.max_screen),
        }
    }
}

/// The largest screen side offered, whatever the GPU takes.
const MAX_SCREEN_SIDE: u16 = 8192;

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

/// The window system draws the cursor: the scene treats it as a cursor
/// plane, and the backend hands the shown image to the shell.
impl CursorPlaneSink for SurfacePlatform {
    fn fits(&self, output_idx: usize, _width: u32, _height: u32) -> bool {
        output_idx < self.outputs.len()
    }

    fn visible(&self, _output_idx: usize) -> bool {
        self.cursor.shown
    }

    fn uploaded_version(&self, _output_idx: usize) -> Option<u64> {
        self.cursor.image.as_ref().map(|image| image.version)
    }

    fn upload(
        &mut self,
        _output_idx: usize,
        version: u64,
        width: u32,
        height: u32,
        bgra_bytes: &[u8],
    ) -> io::Result<()> {
        let hot = self.cursor.image.as_ref().map_or((0, 0), |image| image.hot);
        self.cursor.image = Some(CursorImage {
            version,
            width,
            height,
            hot,
            bgra: Arc::from(bgra_bytes),
        });
        Ok(())
    }

    fn show(
        &mut self,
        _output_idx: usize,
        hot_x: u16,
        hot_y: u16,
        _x: i32,
        _y: i32,
    ) -> Result<(), CursorShowFailure> {
        let Some(image) = self.cursor.image.as_mut() else {
            return Err(CursorShowFailure {
                remains_visible: self.cursor.shown,
                error: "no cursor image uploaded".into(),
            });
        };
        image.hot = (hot_x, hot_y);
        self.cursor.shown = true;
        Ok(())
    }

    fn hide(&mut self, _output_idx: usize) -> io::Result<()> {
        self.cursor.shown = false;
        Ok(())
    }

    fn hide_all(&mut self) -> io::Result<()> {
        self.cursor.shown = false;
        Ok(())
    }

    fn note_composed_retirement(&mut self, _output_idx: usize) -> bool {
        false
    }
}

impl XBackend<SurfacePlatform> {
    /// Tell the shell when the cursor it shows changed.
    fn flush_host_cursor(&mut self) {
        let wanted = self.platform.cursor.wanted();
        if self.platform.cursor.sent == wanted {
            return;
        }
        let Some(ui) = self.pstate.ui.as_ref() else {
            return;
        };
        self.platform.cursor.sent = wanted;
        let image = wanted.and(self.platform.cursor.image.clone());
        ui.set_cursor(image);
    }

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
        self.flush_host_cursor();
    }

    /// Resize the root to `root` and the output to `output`: every
    /// fallible step (root/COW storage, slots, scene outputs) first, then
    /// an infallible swap. The caller tells the clients.
    ///
    /// # Errors
    ///
    /// A prepare step failed; the old sizes and storage are kept.
    pub(crate) fn resize_screen(&mut self, root: (u16, u16), output: (u16, u16)) -> io::Result<()> {
        let root = (root.0.max(1), root.1.max(1));
        let output = (output.0.max(1), output.1.max(1));
        let root_prepared = if root == self.platform.fb_size {
            None
        } else {
            Some(self.prepare_root_storage(root.0, root.1)?)
        };
        let output_prepared = if output == self.platform.output_size {
            Ok(None)
        } else {
            self.prepare_output(output).map(Some)
        };
        let output_prepared = match output_prepared {
            Ok(prepared) => prepared,
            Err(e) => {
                if let Some(prepared) = root_prepared {
                    self.discard_root_storage(prepared);
                }
                return Err(e);
            }
        };
        // Nothing below can fail.
        if let Some((slots, scene)) = output_prepared {
            let extent = vk::Extent2D {
                width: u32::from(output.0),
                height: u32::from(output.1),
            };
            // The scene's in-flight frames retire before their slots go.
            self.scene.drain_all(&mut self.platform);
            for (surface, targets) in self.platform.outputs.iter_mut().zip(slots) {
                surface.install_slots(targets, extent);
            }
            self.platform.output_size = output;
            self.scene.install_outputs(scene);
            self.scene.invalidate_all_scanout_damage();
            self.scene.wake_for_damage();
        }
        if let Some(prepared) = root_prepared {
            self.platform.fb_size = root;
            self.commit_root_storage(prepared);
        }
        log::info!(
            "surface: root {}x{}, output {}x{}",
            root.0,
            root.1,
            output.0,
            output.1
        );
        Ok(())
    }

    /// New slots for every surface and the scene's output state at
    /// `size`, nothing installed.
    fn prepare_output(
        &mut self,
        size: (u16, u16),
    ) -> io::Result<(Vec<Vec<OffscreenTarget>>, PreparedOutputs)> {
        let (w, h) = size;
        let extent = vk::Extent2D {
            width: u32::from(w),
            height: u32::from(h),
        };
        let slots = self
            .platform
            .outputs
            .iter()
            .map(|output| output.prepare_slots(extent))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| io::Error::other(format!("surface slots at {w}x{h}: {e:?}")))?;
        if resize_fault(ResizeFault::Outputs) {
            return Err(io::Error::other("injected scene output failure"));
        }
        let scene = self
            .scene
            .prepare_outputs(&self.platform, &self.platform.layouts_at(size))
            .map_err(|e| io::Error::other(format!("surface scene at {w}x{h}: {e:?}")))?;
        Ok((slots, scene))
    }

    /// `RRSetCrtcConfig` on the window output: only a mode at (0,0) inside
    /// the root. The output takes the mode and the shell is asked to size
    /// the window to it. Anything else fails the request with
    /// `RRSetConfigFailed`, as from a DDX whose `crtcSet` refuses
    /// (rrcrtc.c:1462-1465).
    fn apply_window_crtc_config(
        &mut self,
        mode: Option<yserver_core::backend::ModeSpec>,
        x: i32,
        y: i32,
    ) -> io::Result<bool> {
        let Some(mode) = mode else {
            return Err(io::Error::other("the window output cannot be disabled"));
        };
        if (x, y) != (0, 0) {
            return Err(io::Error::other("the window output sits at 0,0"));
        }
        let size = (mode.width, mode.height);
        if size == self.platform.output_size {
            return Ok(false);
        }
        let root = self.platform.fb_size;
        if size.0 > root.0 || size.1 > root.1 {
            return Err(io::Error::other("the mode does not fit the screen"));
        }
        self.resize_screen(root, size)?;
        self.request_window_size(size);
        Ok(true)
    }

    /// Ask the shell to size the window to `size` X pixels, under a fresh
    /// token its resize event echoes.
    fn request_window_size(&mut self, size: (u16, u16)) {
        let Some(ui) = self.pstate.ui.as_ref() else {
            return;
        };
        self.pstate.next_window_request += 1;
        let token = self.pstate.next_window_request;
        self.pstate.window_request = Some(token);
        ui.set_content_size(size.0, size.1, token);
    }

    /// The window's content became `size` X pixels. The user's resize
    /// (`request` none) takes the root and the output to it; the
    /// completion of our request `token` moves only the output, within the
    /// root. A host resize cannot be refused: a failure keeps the old sizes
    /// and the window shows them cropped or cleared.
    fn window_resized(&mut self, state: &mut ServerState, size: (u16, u16), request: Option<u64>) {
        if size.0 == 0 || size.1 == 0 {
            return;
        }
        let max = self.platform.max_screen;
        let size = (size.0.min(max), size.1.min(max));
        let Some(token) = request else {
            if size == self.platform.output_size {
                return;
            }
            if let Err(e) = self.resize_screen(size, size) {
                log::warn!(
                    "surface: root {:?} and output {:?} kept under a {size:?} window: {e}",
                    self.platform.fb_size,
                    self.platform.output_size,
                );
                return;
            }
            host_screen_resized(state, self, size.0, size.1);
            return;
        };
        if self.pstate.window_request != Some(token) {
            return;
        }
        self.pstate.window_request = None;
        let root = self.platform.fb_size;
        let size = (size.0.min(root.0), size.1.min(root.1));
        if size == self.platform.output_size {
            return;
        }
        if let Err(e) = self.resize_screen(root, size) {
            log::warn!(
                "surface: output {:?} kept in a {size:?} window: {e}",
                self.platform.output_size,
            );
            return;
        }
        let now = state.timestamp_now();
        self.surface_refresh_randr(state, now);
        let changed: Vec<(u32, u32, u32)> = state
            .randr
            .outputs
            .first()
            .map(|o| (o.output_id, o.crtc_id, o.mode_id))
            .into_iter()
            .collect();
        emit_randr_change_notifications(state, &changed);
    }

    /// Apply the shell's events.
    fn apply_shell_events(&mut self, state: &mut ServerState, events: Vec<ShellEvent>) {
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
                ShellEvent::Resized {
                    width,
                    height,
                    scale,
                    mm,
                    request,
                } => {
                    // The shell resized the drawable before posting: rebuild
                    // every swapchain even when the output keeps its size.
                    for output in &mut self.platform.outputs {
                        output.set_copy_scale(scale);
                        output.note_out_of_date();
                    }
                    self.platform.output_mm = mm;
                    self.window_resized(state, (width, height), request);
                    self.publish_output_mm(state);
                    self.scene.invalidate_all_scanout_damage();
                    self.scene.wake_for_damage();
                }
            }
        }
    }

    /// Tell clients the output's millimetres when a resize event changed
    /// only those (a window moved to another display).
    fn publish_output_mm(&mut self, state: &mut ServerState) {
        let mm = self.platform.output_mm;
        let Some(output) = state.randr.outputs.first() else {
            return;
        };
        if (output.mm_width, output.mm_height) == mm {
            return;
        }
        let now = state.timestamp_now();
        self.surface_refresh_randr(state, now);
        let changed: Vec<(u32, u32, u32)> = state
            .randr
            .outputs
            .first()
            .map(|o| (o.output_id, o.crtc_id, o.mode_id))
            .into_iter()
            .collect();
        emit_randr_change_notifications(state, &changed);
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
        self.layouts_at(self.output_size)
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
        let cursor_frame = b
            .any_surface_visible()
            .then(|| b.cursor_anim_deadline())
            .flatten();
        b.gpu_work_deadline(now)
            .into_iter()
            .chain(scene)
            .chain(cursor_frame)
            .min()
    }

    fn on_platform_event(b: &mut XBackend<Self>, state: &mut ServerState) {
        if let Some(link) = b.pstate.link.as_ref() {
            let events = link.drain();
            b.apply_shell_events(state, events);
        }
        b.service_surfaces();
    }

    fn note_client_count(b: &mut XBackend<Self>, count: usize) {
        b.pstate.client_count.store(count, Ordering::Release);
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
        b.tick_cursor_animation();
        b.compose_tick();
        b.drain_paint_submit_telemetry();
        b.flush_host_cursor();
        Ok(())
    }

    /// The window system moves its cursor itself: nothing to compose.
    fn move_hw_cursor(
        _b: &mut XBackend<Self>,
        _mixed: bool,
        _pos: (i32, i32),
        _hot: (u16, u16),
        _size: (i32, i32),
    ) {
    }

    /// `RRSetScreenSize` resizes the root only; the output keeps its mode
    /// (the core refuses a size that would crop it).
    fn set_logical_screen_size(b: &mut XBackend<Self>, w: u16, h: u16) -> io::Result<()> {
        let output = b.platform.output_size;
        b.resize_screen((w, h), output)
    }

    fn apply_crtc_config(
        b: &mut XBackend<Self>,
        _output_id: u32,
        _connector: &str,
        mode: Option<yserver_core::backend::ModeSpec>,
        x: i32,
        y: i32,
    ) -> io::Result<bool> {
        b.apply_window_crtc_config(mode, x, y)
    }

    fn refresh_randr_state_set_time(
        b: &mut XBackend<Self>,
        state: &mut ServerState,
        set_time: u32,
    ) {
        b.surface_refresh_randr(state, set_time);
    }

    fn randr_limits(b: &XBackend<Self>) -> RandrLimits {
        b.platform.randr_limits()
    }

    fn host_pointer_warped(b: &mut XBackend<Self>, x: i32, y: i32) {
        if let Some(ui) = b.pstate.ui.as_ref() {
            ui.warp_pointer(x, y);
        }
    }
}
