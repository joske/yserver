//! What a native host implements and receives (spec §4.5).

use std::io;

use ash::vk;
use yserver_core::{
    backend::ActiveCursorImage,
    fd::OwnedFd,
    rootless::{Rect, RootlessEvent, ToplevelId},
};

use crate::kms::vk::device::VkContext;

/// Per top-level, monotonic.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub(crate) struct FrameSeq(pub u64);

/// How a host takes a top-level's pixels.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum HandoffKind {
    /// A Vulkan surface on the same device; the presenter owns its swapchain.
    VulkanSurface,
    /// A dmabuf of the backing (Linux). RL1 answers it with `Cpu` (D4).
    Dmabuf,
    /// Bytes.
    Cpu,
}

/// An owned CPU frame; every byte equals the canonical image at `seq`
/// (spec §4.5.2).
#[derive(Debug)]
pub(crate) struct CpuFrame {
    pub id: ToplevelId,
    pub seq: FrameSeq,
    /// The backing allocation the bytes came from.
    pub backing_gen: u64,
    /// Backing extent = native content (spec §4.0).
    pub width: u16,
    pub height: u16,
    pub stride: u32,
    /// 24: alpha undefined; 32: premultiplied ARGB.
    pub depth: u8,
    /// B, G, R, A rows, full extent.
    pub pixels: Vec<u8>,
    /// Backing pixels changed since the last delivered frame.
    pub damage: Vec<Rect>,
}

#[allow(dead_code, reason = "the dmabuf handoff is RL3 (D4)")]
#[derive(Debug)]
pub(crate) struct DmabufFrame {
    pub id: ToplevelId,
    pub seq: FrameSeq,
    pub backing_gen: u64,
    pub fds: Vec<OwnedFd>,
    pub stride: u32,
    pub modifier: u64,
    pub format: u32,
    pub damage: Vec<Rect>,
}

/// Host → presenter, over the platform link (spec §4.5, §4.5.1).
#[derive(Debug)]
pub(crate) enum FrameEvent {
    /// The setup handshake is done: ready for the first frame.
    SetupDone { id: ToplevelId },
    /// Can take another frame.
    Ready { id: ToplevelId },
    /// Shown / committed.
    Delivered { id: ToplevelId, seq: FrameSeq },
    /// Could not be shown.
    Dropped { id: ToplevelId, seq: FrameSeq },
    /// A `CpuFrame`'s bytes, back for reuse.
    Released {
        id: ToplevelId,
        seq: FrameSeq,
        pixels: Vec<u8>,
    },
}

/// A native window system's side of rootless mode (spec §4.5), driven on the
/// core thread by the presenter.
pub(crate) trait RootlessHost {
    /// Asked once at `Mapped`. `Dmabuf` is answered with `Cpu` until RL3 (D4).
    fn handoff(&self, id: ToplevelId, depth: u8) -> HandoffKind;
    /// `VulkanSurface`: called by the presenter on `SetupDone`.
    fn create_surface(&mut self, id: ToplevelId, vk: &VkContext) -> io::Result<vk::SurfaceKHR>;
    fn submit_cpu(&mut self, frame: CpuFrame);
    #[allow(dead_code, reason = "the dmabuf handoff is RL3 (D4)")]
    fn submit_dmabuf(&mut self, frame: DmabufFrame) {
        drop(frame);
    }
    /// The first frame was delivered (GPU: presented): make the window visible.
    fn first_frame(&mut self, id: ToplevelId);
    /// Lifecycle, after the presenter applied the batch.
    fn on_events(&mut self, events: &[RootlessEvent]);
    /// `VulkanSurface`: swapchain pixels per X pixel per axis, for a window
    /// system whose surfaces are in device pixels (Windows); 1 shows the
    /// backing 1:1.
    fn copy_scale(&self) -> u32 {
        1
    }
    /// `id` has a frame waiting for `Ready`: arm the pacing signal (D7).
    #[allow(dead_code, reason = "readiness gating (RL1 Task 5)")]
    fn frame_wanted(&mut self, _id: ToplevelId) {}
    /// Before a swapchain (re)build at `extent` backing pixels.
    fn surface_resizing(&mut self, _id: ToplevelId, _extent: (u32, u32)) {}
    /// After it, whatever the outcome: commit what the driver changed on
    /// the native surface.
    fn surface_rebuilt(&mut self, _id: ToplevelId) {}
    /// The presenter destroyed the last swapchain and the surface of `id`:
    /// the native window may be freed now.
    fn surface_released(&mut self, _id: ToplevelId) {}
    /// The cursor to show over the host's windows, when it changed; `None`
    /// shows none (the `None` cursor, or one hidden). Its pixels are
    /// straight-alpha BGRA. For a host with no shell of its own to tell.
    #[cfg_attr(not(kms), allow(dead_code))]
    fn set_cursor(&mut self, _cursor: Option<&ActiveCursorImage>) {}
}
