//! RAII wrapper for a `vk::Semaphore` so it can be `Arc`-shared for the
//! deferred PRESENT completion path. Destruction happens on the last Arc drop
//! (via `vkDestroySemaphore`), independent of the X11 resource id's lifetime.
//!
//! This backs XSync `Fence` resources only. DRI3 1.4 syncobjs are
//! `ImportedSyncobj` — they are DRM objects and never enter Vulkan.

use std::sync::Arc;

use ash::vk;

use crate::kms::vk::device::VkContext;

pub struct OwnedSemaphore {
    vk: Arc<VkContext>,
    semaphore: vk::Semaphore,
}

impl OwnedSemaphore {
    pub fn new(vk: Arc<VkContext>, semaphore: vk::Semaphore) -> Self {
        Self { vk, semaphore }
    }

    pub fn semaphore(&self) -> vk::Semaphore {
        self.semaphore
    }
}

impl std::fmt::Debug for OwnedSemaphore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnedSemaphore")
            .field("semaphore", &self.semaphore)
            .finish_non_exhaustive()
    }
}

impl Drop for OwnedSemaphore {
    fn drop(&mut self) {
        if self.semaphore == vk::Semaphore::null() {
            return;
        }
        unsafe {
            self.vk.device.destroy_semaphore(self.semaphore, None);
        }
    }
}
