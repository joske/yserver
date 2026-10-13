//! Vulkan-side composite types + errors shared with the compose
//! recorder (sub-phase 4.1.3.4).
//!
//! Spec: docs/superpowers/specs/2026-05-07-phase4-1-vulkan-compositor-design.md
//! "Frame composite pass".
//!
//! The runtime compose entry point is `kms::render::scene::record_compose`
//! (the buffer-age-aware v2 fork). This module only carries the shared
//! error/scene/draw types.

use ash::vk;

/// Backend switch for which scanout path is active. Phase 4.1.5
/// retired the pixman alternatives — Vulkan composite is the sole
/// path. Kept as an enum so the `kms_xts_tooling` crate's `Default`
/// callers don't break their match arms.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum VkScanoutMode {
    /// Per-window composite (sub-phase 4.1.3.4): drawing ops fill
    /// per-drawable VkImage mirrors directly; the composite pass
    /// walks the window tree drawing one quad per visible drawable
    /// sampling its mirror, ending with an atomic flip with explicit
    /// IN/OUT fences.
    #[default]
    VkComposite,
}

/// Per-bo phase. The lifecycle is roughly
/// `Free → Recording → Submitted → Pending → OnScreen → Retiring → Free`.
/// `Submitted` can also revert to `Recording` on atomic-EBUSY, or jump
/// to `Free` on modeset preempt.
#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
pub enum BoPhase {
    /// Not in flight; GPU may write into it. No fences attached.
    #[default]
    Free,
    /// Composite CB being recorded for this bo.
    Recording,
    /// `vkQueueSubmit2` issued; we still own the `IN_FENCE_FD` until
    /// the atomic commit either accepts (kernel consumes it) or
    /// rejects (we close it).
    Submitted,
    /// `drmModeAtomicCommit` accepted; `IN_FENCE_FD` ownership
    /// transferred to kernel; we now own the `OUT_FENCE_FD` (the
    /// release fence).
    Pending,
    /// Pageflip-complete arrived. Bo is on-screen. The release fence
    /// is signal-pending (KMS signals it when the next flip retires
    /// this bo).
    OnScreen,
    /// A later flip's pageflip-complete arrived; this bo is no
    /// longer on screen. Release fence is signalled. Returns to
    /// `Free` once all GPU readers (e.g. damage-diff sources)
    /// complete.
    Retiring,
}

/// Errors from the compose + atomic-flip path
/// (`kms::render::scene::record_compose`).
#[derive(Debug, thiserror::Error)]
pub enum PresentError {
    #[error("vulkan: {0}")]
    Vk(vk::Result),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("scanout bo has no DRM framebuffer (allocation incomplete?)")]
    NoFb,
    #[error("scanout bo state machine wrong phase: {0:?}")]
    WrongPhase(BoPhase),
}

impl From<vk::Result> for PresentError {
    fn from(r: vk::Result) -> Self {
        PresentError::Vk(r)
    }
}

// The compose walks a [`CompositeScene`] (built by the backend from
// the window tree, in stacking order) and emits one textured quad
// per visible drawable into the target `ScanoutBo`. The atomic flip
// handshake — signalSemaphore on submit → exported as `IN_FENCE_FD`
// → `OUT_FENCE_PTR` adopted as release fence — is the fence model
// the design spec describes for "Per scanout / per CRTC".

// `CompositePushConsts` + `CompositorPipeline` live on in `super::pipeline`.

/// One quad to draw in the composite pass. The backend assembles
/// these in the order they should rasterise (back-to-front).
#[derive(Debug, Clone, Copy)]
pub struct CompositeDraw {
    /// Mirror image view to sample. Must be in
    /// `SHADER_READ_ONLY_OPTIMAL` (the layout `MirrorUploader`
    /// leaves it in after the upload).
    pub image_view: vk::ImageView,
    /// Top-left corner of the destination rect in scanout pixel
    /// coords (after layout-offset translation by the caller).
    pub dst_origin: [f32; 2],
    /// Width × height of the destination rect.
    pub dst_size: [f32; 2],
    /// Source UV origin in normalised texture coords (0..1). For
    /// most draws this is `[0.0, 0.0]` (sample the whole texture);
    /// the bg_pixmap path sets it to the per-output slice of a
    /// virtual-screen-sized wallpaper.
    pub src_origin: [f32; 2],
    /// Source UV size in normalised texture coords (0..1). `[1, 1]`
    /// for the whole-texture case.
    pub src_size: [f32; 2],
    /// `true` selects the pass-through composite pipeline (the
    /// mirror's sampled α reaches the scanout's blend stage). Used
    /// by cursor + window-mirror draws post-L1 task A.16. `false`
    /// selects the force-opaque variant — the bg-pixmap root draw
    /// stays here because the root mirror is always fully painted
    /// and forcing α=1.0 sidesteps any α invariant on it.
    pub alpha_passthrough: bool,
}

/// One frame's worth of composite work for a single output. Built
/// fresh each frame by the backend.
#[derive(Debug, Clone)]
pub struct CompositeScene {
    /// `[r, g, b, a]` clear value for the scanout, in linear
    /// 0..1. Replaces pixman's "fill rect with bg_pixel" step.
    pub bg_color: [f32; 4],
    /// Draws in stacking order, back-to-front: bg pixmap (if any)
    /// first, then visible windows + descendants depth-first, then
    /// cursor last.
    pub draws: Vec<CompositeDraw>,
}
