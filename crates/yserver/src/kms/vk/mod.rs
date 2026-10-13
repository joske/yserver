//! Vulkan backend for the KMS compositor (Phase 4.1).
//!
//! The renderer half lives in the `yserver-render` crate and is re-exported
//! here so `crate::kms::vk::…` paths keep resolving; `scanout` (KMS scanout
//! BOs) stays in this crate.

pub use yserver_render::kms::vk::{
    call_stats, compositor, copy_scratch, device, dri3, dst_readback, glyph, gradient, instance,
    logic_fill_pipeline, mask_scratch, mem_accounting, ops, pipeline, pixmap_pool, render_pipeline,
    submit_stats, sync, target, text_pipeline, trap_pipeline, vram,
};
pub(crate) use yserver_render::kms::vk::{
    image_format_properties2, optional_sync_fd_from_vk, owned_fd_from_vk, probe_digest,
    probe_pattern,
};

pub mod scanout;
