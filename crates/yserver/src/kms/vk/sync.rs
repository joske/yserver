//! XSync / DRI3 `VkSemaphore` import / export helpers (Phase 4.2.2,
//! design §3.4).
//!
//! - **Binary semaphore from a `sync_file` fd** — [`import_sync_file`].
//!   Used by DRI3 `FenceFromFD`. The imported payload is TEMPORARY:
//!   it lasts only until the first wait, which matches XSync
//!   `Fence`'s one-shot trigger/wait semantics.
//! - [`export_sync_file`] — symmetric to `import_sync_file`, used by
//!   DRI3 `FDFromFence` to hand the client back a `sync_file` fd.
//!
//! **fd ownership rule** (design §3.2). `vkImportSemaphoreFdKHR`
//! consumes the fd only on `VK_SUCCESS`. The helpers in this module
//! take `OwnedFd` by value: on success, the fd's ownership transfers
//! into the resulting `VkSemaphore`; on failure, we re-claim the
//! raw fd and close it before returning `Err`.

use ash::vk;
use std::os::fd::{AsRawFd, IntoRawFd, OwnedFd, RawFd};

use super::device::VkContext;

/// Import a `sync_file` fd as a fresh **binary** `VkSemaphore`.
/// Phase 4.2.2 — backs DRI3 `FenceFromFD`.
pub fn import_sync_file(vk: &VkContext, fd: OwnedFd) -> Result<vk::Semaphore, vk::Result> {
    import_optional_sync_file(vk, Some(fd))
}

/// Import a `sync_file` payload, including Vulkan's `fd = -1`
/// already-signalled sentinel, as a fresh temporary binary semaphore.
///
/// `VK_KHR_external_semaphore_fd` gives `-1` real synchronization semantics:
/// importing it produces a signalled temporary payload.  It must therefore
/// remain a semaphore wait in cross-device submissions rather than being
/// optimized away merely because there is no userspace fd to poll.
///
/// On success Vulkan consumes only a real fd. On failure `fd` remains owned by
/// this function and drops normally; the sentinel is never passed to
/// `close(2)`.
pub(crate) fn import_optional_sync_file(
    vk: &VkContext,
    fd: Option<OwnedFd>,
) -> Result<vk::Semaphore, vk::Result> {
    let create_info = vk::SemaphoreCreateInfo::default();
    let semaphore = unsafe { vk.device.create_semaphore(&create_info, None)? };
    let raw = optional_sync_file_raw(fd.as_ref().map(AsRawFd::as_raw_fd));
    let import_info = vk::ImportSemaphoreFdInfoKHR::default()
        .semaphore(semaphore)
        .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD)
        // SYNC_FD import is required to be TEMPORARY by spec.
        .flags(vk::SemaphoreImportFlags::TEMPORARY)
        .fd(raw);
    let result = unsafe { vk.external_semaphore_fd.import_semaphore_fd(&import_info) };
    match result {
        Ok(()) => {
            if let Some(fd) = fd {
                // Vulkan consumed this real fd on success. Relinquish Rust's
                // close-on-drop ownership without closing the transferred
                // descriptor.
                let _ = fd.into_raw_fd();
            }
            Ok(semaphore)
        }
        Err(e) => {
            // `fd` is still owned on failure and closes automatically when
            // this function returns. `None` represented raw -1, so there is
            // deliberately no descriptor to close.
            unsafe { vk.device.destroy_semaphore(semaphore, None) };
            Err(e)
        }
    }
}

fn optional_sync_file_raw(fd: Option<RawFd>) -> RawFd {
    fd.unwrap_or(-1)
}

/// Export a `VkSemaphore`'s current payload as a fresh `sync_file`
/// fd. Phase 4.2.2 — backs DRI3 `FDFromFence`. The returned fd's
/// ownership transfers to the caller (Vulkan's internal copy is
/// disjoint from this fd).
pub fn export_sync_file(vk: &VkContext, semaphore: vk::Semaphore) -> Result<OwnedFd, vk::Result> {
    let info = vk::SemaphoreGetFdInfoKHR::default()
        .semaphore(semaphore)
        .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
    let raw = unsafe { vk.external_semaphore_fd.get_semaphore_fd(&info)? };
    super::owned_fd_from_vk(raw, "vkGetSemaphoreFdKHR(SYNC_FD)")
}

#[cfg(test)]
mod tests {
    #[test]
    fn optional_sync_file_preserves_vulkan_already_signalled_sentinel() {
        assert_eq!(super::optional_sync_file_raw(None), -1);
        assert_eq!(super::optional_sync_file_raw(Some(17)), 17);
    }

    // The Vulkan import / export calls themselves require live handles and
    // remain covered by the integration smoke under vng or bare metal.
}
