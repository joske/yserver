use crate::kms::{
    render::{
        gpu::{FencePool, GpuCore},
        submit_group::SubmitGroup,
    },
    vk::{device::VkContext, ops::OpsCommandPool, pixmap_pool::PixmapPool},
};
use std::sync::Arc;

impl GpuCore {
    /// The live renderer's GPU state, as `PlatformBackend::open_with_commit`
    /// builds it.
    pub fn new(
        vk: Arc<VkContext>,
        ops_command_pool: OpsCommandPool,
        fence_pool: FencePool,
        pixmap_pool: Option<Arc<PixmapPool>>,
    ) -> Self {
        Self {
            vk: Some(vk),
            ops_command_pool: Some(ops_command_pool),
            fence_pool: Some(fence_pool),
            pixmap_pool,
            renderer_failed: false,
            submit_group: SubmitGroup::new(),
            last_flush_outcome: None,
            force_next_submit_failure: false,
            next_submit_cause: None,
            force_next_frame_record_failure: false,
            queue_submits: 0,
        }
    }

    /// Vk-less test state: no device, no pools. Submits and allocations
    /// return `ERROR_INITIALIZATION_FAILED`; flushes of an empty group are
    /// no-ops.
    #[doc(hidden)]
    pub fn for_tests() -> Self {
        Self {
            vk: None,
            ops_command_pool: None,
            fence_pool: None,
            pixmap_pool: None,
            renderer_failed: false,
            submit_group: SubmitGroup::new(),
            last_flush_outcome: None,
            force_next_submit_failure: false,
            next_submit_cause: None,
            force_next_frame_record_failure: false,
            queue_submits: 0,
        }
    }

    /// [`Self::for_tests`] with a live Vulkan context, ops command pool and
    /// fence pool (no pixmap pool). `None` when no Vulkan device is
    /// available.
    #[cfg(test)]
    pub(crate) fn for_tests_with_vk() -> Option<Self> {
        let vk = VkContext::new().ok()?;
        let ops_command_pool = OpsCommandPool::new(Arc::clone(&vk)).ok()?;
        let fence_pool = FencePool::new(Arc::clone(&vk));
        let mut gpu = Self::for_tests();
        gpu.vk = Some(vk);
        gpu.ops_command_pool = Some(ops_command_pool);
        gpu.fence_pool = Some(fence_pool);
        Some(gpu)
    }
}
