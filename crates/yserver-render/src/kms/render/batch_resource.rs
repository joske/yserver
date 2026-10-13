//! `BatchResource` — deferred-release trait used by the v2 frame
//! builder's `retired_resources` pin slot.
//!
//! Lived under `kms::scheduler::paint_batch` during the v1 era when
//! it was driven by v1's `PaintBatch` state machine; v1 retired
//! 2026-05-26 but the trait stayed load-bearing for v2's frame
//! builder retire-pin path (every Vk resource that outlives its
//! containing CB submission lands here as a `Box<dyn BatchResource>`
//! and gets `release()`'d when the frame's `FenceTicket` signals).
//!
//! Implementors: `RetiredMaskScratchImage`, `RetiredCopyScratchImage`,
//! `RetiredDstReadbackImage`, `PooledPixmapReturn`, `GradientPicture`
//! (all in `kms::vk`).

use crate::kms::vk::device::VkContext;

/// A resource whose destruction must be deferred until the GPU fence
/// covering its last use has signaled. The frame builder pins these
/// in `OpenFrame::retired_resources` and calls `release` after the
/// corresponding `FenceTicket` retires.
pub trait BatchResource: Send + std::fmt::Debug {
    fn release(self: Box<Self>, vk: &VkContext);
}
