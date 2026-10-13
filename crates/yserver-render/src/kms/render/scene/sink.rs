//! The output seam: what the scene asks of whatever drives its outputs.
//!
//! The scene composes; an [`OutputSink`] owns the frame slots the scene
//! composes into, presents them, and reports their retirement. Every method
//! borrows the sink for the duration of the call only: the scene never holds
//! a reference obtained from the sink across another sink call. Everything
//! that crosses is plain data, a Vulkan handle, or a [`ComposeRenderTarget`]
//! lent to the record closure of [`OutputSink::finish_frame`].

use std::{io, os::fd::OwnedFd};

use ash::vk;

use crate::kms::{
    render::{
        gpu::GpuCore,
        scene::{ComposeRenderTarget, ComposeSubmit, SceneError},
    },
    vk::compositor::PresentError,
};

/// A frame slot handed out by [`OutputSink::acquire_slot`], with what the
/// buffer-age repaint planning needs to know about it.
#[derive(Debug, Clone, Copy)]
pub struct SlotToken {
    pub output_idx: usize,
    pub bo_idx: usize,
    pub extent: vk::Extent2D,
    /// Generation of the last frame this slot presented; `None` when it never
    /// presented or was invalidated since.
    pub last_present_generation: Option<u64>,
    /// Set after a failed present left this slot's content indeterminate;
    /// cleared by the next successful present.
    pub content_invalidated: bool,
}

/// One retired page flip, from [`OutputSink::on_page_flip_complete`]: the slot
/// that went on screen and the one it replaced.
#[derive(Debug, Clone, Copy)]
pub struct FlipRetirement {
    pub retired_bo_idx: Option<usize>,
    pub presented_bo_idx: usize,
    pub generation: u64,
}

/// How an output's frames reach the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputRoute {
    /// The scene composes straight into the scanout buffer.
    Shared,
    /// The scene composes into a source the sink copies to the scanout
    /// device once the render completes (two-GPU route).
    Copied,
    /// The output has no frame slots.
    None,
}

/// Why a frame cannot start on an output, before anything ran.
#[derive(Debug)]
pub enum FrameTargetError {
    /// No device drives the output.
    NoDevice(io::Error),
    /// The output has no frame slots, or not the requested one.
    NoSlot,
}

/// How a finished frame leaves the sink.
pub enum FrameHandoff {
    /// Composed and flipped; retirement comes as a page flip.
    FlipPending(ComposeSubmit),
    /// Composed into the copied route's source; the completion fd (`None` is
    /// the already-signalled payload) goes to
    /// [`OutputSink::register_render_completion`].
    RenderCompletion(Option<OwnedFd>),
}

/// The result of [`OutputSink::finish_frame`].
pub enum FinishOutcome {
    /// The slot is gone; nothing ran.
    Unavailable(FrameTargetError),
    /// The frame sequence ran, successfully or not.
    Finished {
        result: Result<FrameHandoff, PresentError>,
        /// GPU render time of the slot's previous compose.
        previous_gpu_ns: Option<u64>,
        /// The failure leaves the renderer unusable; the scene latches
        /// `renderer_failed`.
        fail_stop: bool,
    },
}

impl From<FrameTargetError> for SceneError {
    fn from(error: FrameTargetError) -> Self {
        match error {
            FrameTargetError::NoDevice(error) => Self::Present(PresentError::Io(error)),
            FrameTargetError::NoSlot => Self::NoVk,
        }
    }
}

/// A copied-route render completion that became ready.
pub struct RenderCompletion {
    pub job_id: u64,
    /// The output it belongs to, or the removed output's identity formatted
    /// for the log.
    pub output: Result<usize, String>,
    pub bo_idx: usize,
    /// `None` is the already-signalled payload.
    pub fd: Option<OwnedFd>,
}

/// The record step of [`OutputSink::finish_frame`]: compose and submit into
/// the lent target.
pub type RecordFrame<'a> =
    dyn FnMut(&mut dyn ComposeRenderTarget) -> Result<ComposeSubmit, PresentError> + 'a;

/// Frame slots, presentation and retirement of the outputs the scene draws.
pub trait OutputSink {
    fn gpu(&self) -> &GpuCore;
    fn gpu_mut(&mut self) -> &mut GpuCore;
    fn cursor_plane(&self) -> &dyn CursorPlaneSink;
    fn cursor_plane_mut(&mut self) -> &mut dyn CursorPlaneSink;

    fn route(&self, output_idx: usize) -> OutputRoute;
    /// Number of frame slots of the output, `None` without slots.
    fn slot_count(&self, output_idx: usize) -> Option<usize>;
    fn acquire_slot(&mut self, output_idx: usize) -> Option<SlotToken>;
    /// The image the slot composes into.
    fn slot_image(&self, output_idx: usize, bo_idx: usize) -> Option<vk::Image>;
    /// Whether a frame can start on the output at all.
    ///
    /// # Errors
    ///
    /// The output has no device or no frame slots.
    fn check_frame_target(&self, output_idx: usize) -> Result<(), FrameTargetError>;
    /// Run the frame sequence on the slot, with `record` composing into it.
    fn finish_frame(
        &mut self,
        output_idx: usize,
        bo_idx: usize,
        record: &mut RecordFrame<'_>,
    ) -> FinishOutcome;
    fn invalidate_slot(&mut self, output_idx: usize, bo_idx: usize);
    fn cancel_slot(&mut self, output_idx: usize, bo_idx: usize);
    /// Make a slot whose present failed after its GPU work finished usable.
    ///
    /// # Errors
    ///
    /// Recovery of the slot failed.
    fn recycle_failed_slot(&mut self, output_idx: usize, bo_idx: usize) -> io::Result<()>;

    fn on_page_flip_complete(&mut self, output_idx: usize) -> Option<FlipRetirement>;
    fn commit_slot_present(&mut self, output_idx: usize, bo_idx: usize, generation: u64);

    /// Copied route: register a source render completion.
    ///
    /// # Errors
    ///
    /// The completion could not be registered.
    fn register_render_completion(
        &mut self,
        output_idx: usize,
        bo_idx: usize,
        fd: Option<OwnedFd>,
    ) -> io::Result<u64>;
    fn drain_render_completions(&mut self) -> Vec<RenderCompletion>;
    fn clear_render_completions(&mut self);
    /// Copied route: copy the completed source and present it.
    ///
    /// # Errors
    ///
    /// The copy or the present failed.
    fn submit_copied(
        &mut self,
        output_idx: usize,
        bo_idx: usize,
        fd: Option<OwnedFd>,
    ) -> io::Result<()>;
}

/// A cursor show that failed, and whether the old sprite is still visible.
#[derive(Debug)]
pub struct CursorShowFailure {
    pub remains_visible: bool,
    pub error: Box<dyn std::error::Error + Send + Sync>,
}

impl std::fmt::Display for CursorShowFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, f)
    }
}

/// The hardware cursor plane of the outputs the scene draws.
pub trait CursorPlaneSink {
    fn fits(&self, output_idx: usize, width: u32, height: u32) -> bool;
    fn visible(&self, output_idx: usize) -> bool;
    fn uploaded_version(&self, output_idx: usize) -> Option<u64>;
    /// # Errors
    ///
    /// The upload failed.
    fn upload(
        &mut self,
        output_idx: usize,
        version: u64,
        width: u32,
        height: u32,
        bgra_bytes: &[u8],
    ) -> io::Result<()>;
    /// # Errors
    ///
    /// The show failed.
    fn show(
        &mut self,
        output_idx: usize,
        hot_x: u16,
        hot_y: u16,
        x: i32,
        y: i32,
    ) -> Result<(), CursorShowFailure>;
    /// # Errors
    ///
    /// The hide failed.
    fn hide(&mut self, output_idx: usize) -> io::Result<()>;
    /// # Errors
    ///
    /// The plane is unavailable.
    fn hide_all(&mut self) -> io::Result<()>;
    fn note_composed_retirement(&mut self, output_idx: usize) -> bool;
}
