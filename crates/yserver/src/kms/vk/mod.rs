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
pub(crate) mod damage_audit_compare;
pub mod device;
pub mod dri3;
pub mod dst_readback;
pub mod glyph;
pub mod gradient;
pub mod instance;
pub mod logic_fill_pipeline;
pub mod mask_scratch;
pub(crate) mod masked_blit_pipeline;
pub mod mem_accounting;
pub mod memory;
pub mod ops;
pub mod pipeline;
pub mod pixmap_pool;
pub(crate) mod probe_digest;
pub(crate) mod probe_pattern;
pub mod render_pipeline;
pub(crate) mod scale_pipeline;
pub mod scanout;
pub mod sync;
pub mod target;
pub mod text_pipeline;
pub mod trap_pipeline;
pub mod vram;
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

/// `vkGetPhysicalDeviceImageFormatProperties2`, logging each rejection with
/// the code that asked.
///
/// RADV prints `unsupported VkExternalMemoryHandleTypeFlagBits …` through
/// our debug messenger for every rejected dma-buf query, with no hint of
/// which query it was. A rejection is a legitimate answer, but a busy
/// session showed hundreds that grew with activity, so this line — on the
/// `yserver::format_query` target, emitted right after RADV's — names the
/// caller. `#[track_caller]` here and on the query functions makes
/// `Location::caller()` the site that called the query function, one level
/// up, which is what tells a per-import check from a startup probe.
#[track_caller]
pub(crate) fn image_format_properties2(
    vk: &device::VkContext,
    site: &'static str,
    modifier: Option<u64>,
    info: &ash::vk::PhysicalDeviceImageFormatInfo2<'_>,
    props: &mut ash::vk::ImageFormatProperties2<'_>,
) -> Result<(), ash::vk::Result> {
    // SAFETY: an instance-level query with no externally synchronised
    // handle; `vk` keeps the instance alive for the duration.
    let result = unsafe {
        vk.instance
            .get_physical_device_image_format_properties2(vk.physical_device, info, props)
    };
    if let Err(e) = result {
        let caller = std::panic::Location::caller();
        log::debug!(
            target: "yserver::format_query",
            "format query rejected: {site} format={:?} tiling={:?} usage={:?} modifier={} -> {e:?} (asked from {}:{})",
            info.format,
            info.tiling,
            info.usage,
            modifier.map_or_else(|| "-".to_owned(), |m| format!("{m:#x}")),
            caller.file(),
            caller.line(),
        );
    }
    result
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
