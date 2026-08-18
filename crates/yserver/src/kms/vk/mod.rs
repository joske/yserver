//! Vulkan backend for the KMS compositor (Phase 4.1).
//!
//! Spec: docs/superpowers/specs/2026-05-07-phase4-1-vulkan-compositor-design.md
//!
//! Sub-phase 4.1.1: instance/device/allocator init, idle. Drawing
//! still runs through pixman; this module brings up Vulkan in
//! parallel.

pub mod call_stats;
pub mod compositor;
pub mod copy_scratch;
pub mod device;
pub mod dri3;
pub mod dst_readback;
pub mod glyph;
pub mod gradient;
pub mod instance;
pub mod logic_fill_pipeline;
pub mod mask_scratch;
pub(crate) mod masked_blit_pipeline;
pub mod memory;
pub mod ops;
pub mod pipeline;
pub mod pixmap_pool;
pub(crate) mod probe_digest;
pub(crate) mod probe_pattern;
pub mod render_pipeline;
pub mod scanout;
pub mod sync;
pub mod target;
pub mod text_pipeline;
pub mod trap_pipeline;
// `pub mod upload;` retired in 4.1.5 — pixman → mirror upload pump
// gone with the rest of the pixman canonical-store machinery.

use ash::vk as ash_vk;
use std::os::fd::{FromRawFd, OwnedFd};

fn owned_fd_from_vk(raw_fd: i32, call: &str) -> Result<OwnedFd, ash_vk::Result> {
    if raw_fd < 0 {
        log::warn!("vk: {call} returned invalid fd {raw_fd}");
        return Err(ash_vk::Result::ERROR_OUT_OF_HOST_MEMORY);
    }

    // SAFETY: Vulkan fd-export calls return a fresh fd owned by the
    // caller. The negative-fd case is handled above.
    Ok(unsafe { OwnedFd::from_raw_fd(raw_fd) })
}

pub(crate) fn optional_sync_fd_from_vk(
    raw_fd: i32,
    call: &str,
) -> Result<Option<OwnedFd>, ash_vk::Result> {
    if raw_fd == -1 {
        return Ok(None);
    }
    owned_fd_from_vk(raw_fd, call).map(Some)
}

#[cfg(test)]
mod tests {
    use ash::vk;

    #[test]
    fn owned_fd_from_vk_rejects_negative_fd() {
        let err = super::owned_fd_from_vk(-1, "test").unwrap_err();
        assert_eq!(err, vk::Result::ERROR_OUT_OF_HOST_MEMORY);
    }

    #[test]
    fn optional_sync_fd_from_vk_accepts_no_fence_sentinel() {
        let fd = super::optional_sync_fd_from_vk(-1, "test").unwrap();
        assert!(fd.is_none());
    }
}
