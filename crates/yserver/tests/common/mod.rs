//! Shared scaffolding for `crates/yserver/tests/*.rs` integration tests.
//!
//! Each integration-test binary in this directory compiles `common`
//! independently and uses a different subset of its surface. The
//! `dead_code` allow is the standard Rust pattern for shared test
//! modules — each test crate's view of `common` is partial.

#![allow(dead_code)]

use std::os::fd::OwnedFd;
use yserver::kms::vk::device::VkContext;

/// True (after an `eprintln!("skipping: ...")`) when this Vulkan ICD cannot
/// back a dma-buf export. Some lavapipe builds (GitHub's Ubuntu runner)
/// advertise the extensions but fail the exportable allocation.
pub fn skip_without_dmabuf_export(vk: &std::sync::Arc<VkContext>) -> bool {
    use yserver::kms::vk::{dri3::export_backing, target};
    let probe = target::allocate_exportable(vk, 16, 16, target::EXPORT_FORMAT_BGRA8)
        .and_then(|img| export_backing(vk, &img).map(drop));
    match probe {
        Ok(()) => false,
        Err(e) => {
            eprintln!("skipping: this Vulkan ICD cannot export dma-bufs ({e:?})");
            true
        }
    }
}

/// True (after an `eprintln!("skipping: ...")`) when the device lacks
/// `VK_KHR_external_semaphore_fd`, so no `SYNC_FD` import/export exists.
pub fn skip_without_sync_fd(vk: &VkContext) -> bool {
    if vk.supports_sync_fd() {
        return false;
    }
    eprintln!("skipping: this Vulkan device lacks VK_KHR_external_semaphore_fd");
    true
}

/// Create an already-signaled `sync_file` fd by exporting a Vulkan binary
/// semaphore that was signaled via a signal-only `vkQueueSubmit2`.
///
/// The kernel's dma-buf IMPORT_SYNC_FILE ioctl requires a sync_file fd
/// representing a fence that is already signaled or will be signaled
/// eventually.  For tests we need one that is already done.
///
/// Panics if Vulkan operations fail — this is test scaffolding.
pub fn signaled_sync_file(vk: &VkContext) -> OwnedFd {
    use ash::vk;

    // 1. Create a binary semaphore with SYNC_FD export capability.
    let mut export_info = vk::ExportSemaphoreCreateInfo::default()
        .handle_types(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
    let create_info = vk::SemaphoreCreateInfo::default().push_next(&mut export_info);
    let semaphore =
        unsafe { vk.device.create_semaphore(&create_info, None) }.expect("create export semaphore");

    // 2. Signal it via a signal-only vkQueueSubmit2 (no wait semaphores,
    //    no command buffers — only the signal semaphore info).
    let sig_info = [vk::SemaphoreSubmitInfo::default()
        .semaphore(semaphore)
        .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];
    let submit = [vk::SubmitInfo2::default().signal_semaphore_infos(&sig_info)];
    unsafe {
        vk.device
            .queue_submit2(vk.graphics_queue, &submit, vk::Fence::null())
    }
    .expect("signal-only queue_submit2");

    // Wait for the submit to complete so the semaphore payload is ready
    // to export.
    unsafe { vk.device.queue_wait_idle(vk.graphics_queue) }
        .expect("queue_wait_idle after signal submit");

    // 3. Export the signaled payload as a sync_file fd.
    let sync_fd = yserver::kms::vk::sync::export_sync_file(vk, semaphore)
        .expect("export_sync_file on signaled semaphore");

    // The semaphore payload has been exported (consumed by SYNC_FD
    // semantics); destroy the Vulkan handle.
    // SAFETY: we created `semaphore`, the device is alive, and the prior
    // queue_wait_idle guarantees no submission still references it.
    unsafe { vk.device.destroy_semaphore(semaphore, None) };

    sync_fd
}

/// True iff `fd` (a dma-buf fd) can be re-imported as a Vulkan image via
/// the production `DrawableImage::from_dmabuf` path. Used to prove that a
/// backing's exported dma-buf is still live after a `FreePixmap` while a
/// GLX consumer holds a reference. Uses Vulkan re-import (NOT mmap) — the
/// exported memory is DEVICE_LOCAL and may not be CPU-mappable on a dGPU.
pub fn dmabuf_is_importable(
    vk: &std::sync::Arc<VkContext>,
    fd: std::os::fd::BorrowedFd<'_>,
    width: u32,
    height: u32,
    modifier: u64,
    offset: u64,
    stride: u32,
) -> bool {
    use std::os::fd::AsFd;
    use yserver::kms::vk::target::{DrawableImage, EXPORT_FORMAT_BGRA8};

    let dup = match fd.try_clone_to_owned() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("dmabuf_is_importable: dup failed: {e}");
            return false;
        }
    };
    // Re-import with the ACTUAL exported DRM-format-modifier + plane
    // layout (offset/stride). Even for the LINEAR modifier the driver
    // pads the row stride (e.g. 256 for a 32px BGRA8 row on lavapipe),
    // and it may negotiate a non-LINEAR modifier on other drivers.
    // Guessing `width*4` / `LINEAR` here makes Vulkan reject the import
    // with ERROR_INVALID_DRM_FORMAT_MODIFIER_PLANE_LAYOUT_EXT even though
    // the buffer is perfectly live.
    match DrawableImage::from_dmabuf(
        std::sync::Arc::clone(vk),
        dup.as_fd().try_clone_to_owned().expect("dup2"),
        width,
        height,
        EXPORT_FORMAT_BGRA8,
        modifier,
        &[offset],
        &[stride],
    ) {
        Ok(_img) => true,
        Err(e) => {
            eprintln!("dmabuf_is_importable: from_dmabuf failed: {e:?}");
            false
        }
    }
}
