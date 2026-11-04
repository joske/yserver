//! GPU frames (spec §4.5, `VulkanSurface`): a full-extent copy of the backing
//! into a per-window swapchain image, then present. Swapchains are never
//! destroyed in place: an old one is retired and destroyed once its presents
//! (present fences, swapchain maintenance1) and copies have finished, or
//! after one queue-idle without maintenance1 (plan RL1 Task 7).

use std::sync::Arc;

use ash::vk;
use yserver_core::rootless::ToplevelId;

use crate::{
    kms::{
        render::{
            gpu::{FenceTicket, GpuCore},
            store::{DrawableId, DrawableStore},
        },
        vk::device::VkContext,
    },
    surface::swapchain::{
        chain::{Acquire, AlphaMode, Chain, Present, Swapchain},
        copy_region,
    },
};

/// Acquire semaphores (and copy command buffers) in rotation, as
/// `SurfaceOutput`.
const SYNC_COUNT: usize = 3;

/// One rotation slot: reused only once the copy that waited on its acquire
/// semaphore finished.
struct FrameSync {
    acquire: vk::Semaphore,
    cb: vk::CommandBuffer,
    copy: Option<FenceTicket>,
}

/// One present on a swapchain: its copy and, with maintenance1, the fence
/// the presentation engine signals.
struct PresentRecord {
    copy: FenceTicket,
    fence: Option<vk::Fence>,
}

impl PresentRecord {
    fn done(&self, vk: &VkContext) -> bool {
        self.copy.poll_signaled(vk)
            && self
                .fence
                .is_none_or(|f| unsafe { vk.device.get_fence_status(f) }.unwrap_or(true))
    }
}

/// What a present did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Presented {
    Shown,
    /// No image or sync slot free yet.
    NotReady,
    /// The swapchain no longer matches the surface; rebuilt next time.
    OutOfDate,
}

pub(crate) struct WindowSwapchain {
    vk: Arc<VkContext>,
    chain: Chain,
    surface: vk::SurfaceKHR,
    alpha: AlphaMode,
    maintenance1: bool,
    /// The backing generation and extent the swapchain was built for.
    backing_gen: u64,
    extent: vk::Extent2D,
    /// Swapchain pixels per backing pixel per axis, as the host asked.
    copy_scale: u32,
    /// The device blits backing → swapchain image; without it the copy
    /// stays 1:1 whatever the scale.
    blit: bool,
    pool: vk::CommandPool,
    syncs: Vec<FrameSync>,
    next_sync: usize,
    presents: Vec<PresentRecord>,
    /// Presents whose fence may never signal (an out-of-date present).
    uncertain: bool,
    needs_rebuild: bool,
    /// A (re)build was attempted (`take_rebuilt`).
    rebuilt: bool,
    /// Swapchains replaced since the presenter last collected them.
    replaced: Vec<Retiring>,
    #[cfg(test)]
    pub(crate) fail_next_submit: bool,
    /// Every acquire reports the surface out of date; counted.
    #[cfg(test)]
    pub(crate) always_out_of_date: bool,
    #[cfg(test)]
    pub(crate) out_of_date_count: u64,
    /// The next `vkQueuePresentKHR` fails before it is queued.
    #[cfg(test)]
    pub(crate) fail_next_present: bool,
    /// The next copy ticket / present fence allocation fails.
    #[cfg(test)]
    pub(crate) fail_next_ticket: bool,
    #[cfg(test)]
    pub(crate) fail_next_fence: bool,
    #[cfg(test)]
    pub(crate) last_copy: Option<FenceTicket>,
    /// Read the next presented image back into this buffer (tests).
    #[cfg(test)]
    pub(crate) capture: Option<Capture>,
}

impl WindowSwapchain {
    /// Swapchains of a depth-`depth` window on `surface`, which stays the
    /// caller's until [`Self::retire`].
    pub(crate) fn new(
        vk: &Arc<VkContext>,
        surface: vk::SurfaceKHR,
        depth: u8,
        maintenance1: bool,
    ) -> Result<Self, vk::Result> {
        let pool_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(vk.graphics_queue_family)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        let pool = unsafe { vk.device.create_command_pool(&pool_info, None) }?;
        let mut sc = Self {
            vk: Arc::clone(vk),
            chain: Chain::new(vk, surface),
            surface,
            alpha: if depth == 32 {
                AlphaMode::Premultiplied
            } else {
                AlphaMode::Opaque
            },
            maintenance1,
            backing_gen: 0,
            extent: vk::Extent2D::default(),
            copy_scale: 1,
            blit: false,
            pool,
            syncs: Vec::new(),
            next_sync: 0,
            presents: Vec::new(),
            uncertain: false,
            needs_rebuild: true,
            rebuilt: false,
            replaced: Vec::new(),
            #[cfg(test)]
            fail_next_submit: false,
            #[cfg(test)]
            always_out_of_date: false,
            #[cfg(test)]
            out_of_date_count: 0,
            #[cfg(test)]
            fail_next_present: false,
            #[cfg(test)]
            fail_next_ticket: false,
            #[cfg(test)]
            fail_next_fence: false,
            #[cfg(test)]
            last_copy: None,
            #[cfg(test)]
            capture: None,
        };
        let cb_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(u32::try_from(SYNC_COUNT).unwrap_or(u32::MAX));
        let cbs = unsafe { vk.device.allocate_command_buffers(&cb_info) }?;
        for cb in cbs {
            let acquire = unsafe {
                vk.device
                    .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
            }?;
            sc.syncs.push(FrameSync {
                acquire,
                cb,
                copy: None,
            });
        }
        Ok(sc)
    }

    /// Read the image the next present shows back (tests): the swapchain
    /// is rebuilt with `TRANSFER_SRC` usage.
    /// `false` when the surface cannot make readable images.
    #[cfg(test)]
    pub(crate) fn capture_next(&mut self) -> bool {
        let readable = self.chain.capabilities().is_ok_and(|caps| {
            caps.supported_usage_flags
                .contains(vk::ImageUsageFlags::TRANSFER_SRC)
        });
        if readable {
            self.chain.extra_usage = vk::ImageUsageFlags::TRANSFER_SRC;
            self.needs_rebuild = true;
            let scale = self.effective_scale();
            let image = vk::Extent2D {
                width: self.extent.width * scale,
                height: self.extent.height * scale,
            };
            self.capture = Some(Capture::new(&self.vk, image));
        }
        readable
    }

    /// Show each backing pixel as `scale`x`scale` swapchain pixels (nearest)
    /// from the next present on.
    pub(crate) fn set_copy_scale(&mut self, scale: u32) {
        let scale = scale.max(1);
        if scale != self.copy_scale {
            self.copy_scale = scale;
            self.needs_rebuild = true;
        }
    }

    /// The copy scale the device can do.
    fn effective_scale(&self) -> u32 {
        if self.blit { self.copy_scale } else { 1 }
    }

    /// Whether `format` blits from and to itself (optimal tiling).
    fn blits(&self, format: vk::Format) -> bool {
        // SAFETY: a physical device of this instance.
        let features = unsafe {
            self.vk
                .instance
                .get_physical_device_format_properties(self.vk.physical_device, format)
        }
        .optimal_tiling_features;
        features.contains(vk::FormatFeatureFlags::BLIT_SRC | vk::FormatFeatureFlags::BLIT_DST)
    }

    /// Whether `present` at this backing `extent` and generation (re)builds
    /// the swapchain first.
    pub(crate) fn rebuild_due(&self, extent: vk::Extent2D, backing_gen: u64) -> bool {
        self.needs_rebuild
            || self.chain.current().is_none()
            || backing_gen != self.backing_gen
            || extent != self.extent
    }

    /// Whether a (re)build was attempted since the last call.
    pub(crate) fn take_rebuilt(&mut self) -> bool {
        std::mem::take(&mut self.rebuilt)
    }

    /// The current swapchain (tests).
    #[cfg(test)]
    pub(crate) fn handle(&self) -> Option<vk::SwapchainKHR> {
        self.chain.current().map(|sc| sc.handle)
    }

    /// Swapchains replaced by rebuilds, for the presenter's retiring list.
    pub(crate) fn take_replaced(&mut self) -> Vec<Retiring> {
        std::mem::take(&mut self.replaced)
    }

    /// Build a swapchain for `extent` over the current one, which retires.
    fn rebuild(&mut self, extent: vk::Extent2D) -> Result<(), vk::Result> {
        let caps = self.chain.capabilities()?;
        let formats = self.chain.formats()?;
        let format = formats
            .iter()
            .find(|f| {
                f.format == vk::Format::B8G8R8A8_UNORM
                    && f.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR
            })
            .or_else(|| {
                formats
                    .iter()
                    .find(|f| f.format == vk::Format::B8G8R8A8_UNORM)
            })
            .copied()
            .ok_or(vk::Result::ERROR_FORMAT_NOT_SUPPORTED)?;
        if !caps
            .supported_usage_flags
            .contains(vk::ImageUsageFlags::TRANSFER_DST)
        {
            return Err(vk::Result::ERROR_FEATURE_NOT_PRESENT);
        }
        self.blit = self.copy_scale == 1 || self.blits(format.format);
        if !self.blit {
            log::warn!(
                "rootless: no backing → swapchain blit on this device; scale {} shown 1:1",
                self.copy_scale
            );
        }
        let scale = self.effective_scale();
        let image_extent = if caps.current_extent.width == u32::MAX {
            vk::Extent2D {
                width: extent
                    .width
                    .saturating_mul(scale)
                    .clamp(caps.min_image_extent.width, caps.max_image_extent.width),
                height: extent
                    .height
                    .saturating_mul(scale)
                    .clamp(caps.min_image_extent.height, caps.max_image_extent.height),
            }
        } else {
            caps.current_extent
        };
        log::debug!(
            "rootless: swapchain for {}x{}: currentExtent {}x{}, min {}x{}, max {}x{} -> {}x{}; \
             composite alphas {:?}, {:?}",
            extent.width,
            extent.height,
            caps.current_extent.width,
            caps.current_extent.height,
            caps.min_image_extent.width,
            caps.min_image_extent.height,
            caps.max_image_extent.width,
            caps.max_image_extent.height,
            image_extent.width,
            image_extent.height,
            caps.supported_composite_alpha,
            self.alpha,
        );
        let old = self.chain.take();
        let created = self
            .chain
            .create(&caps, format, image_extent, self.alpha, old.as_ref());
        if let Some(old) = old {
            self.replaced.push(Retiring {
                chain: Chain::new(&self.vk, self.surface),
                vk: Arc::clone(&self.vk),
                swapchains: vec![(old, std::mem::take(&mut self.presents))],
                syncs: Vec::new(),
                pool: vk::CommandPool::null(),
                surface: None,
                wait_idle: !self.maintenance1 || std::mem::take(&mut self.uncertain),
                released: None,
            });
        }
        created?;
        self.needs_rebuild = false;
        Ok(())
    }

    /// Acquire (non-blocking), copy the whole backing via the store's
    /// external-read barriers, present. The copy's ticket is the backing's
    /// render ticket, so a backing freed meanwhile outlives the copy.
    ///
    /// # Errors
    ///
    /// Vulkan failed on rebuild, acquire, record, submit or present.
    pub(crate) fn present(
        &mut self,
        gpu: &GpuCore,
        store: &mut DrawableStore,
        backing: DrawableId,
        backing_gen: u64,
    ) -> Result<Presented, vk::Result> {
        let Some(extent) = store.get(backing).map(|d| d.storage.extent) else {
            return Ok(Presented::NotReady);
        };
        if self.rebuild_due(extent, backing_gen) {
            self.rebuilt = true;
            self.rebuild(extent)?;
            self.backing_gen = backing_gen;
            self.extent = extent;
        }
        let vk = Arc::clone(&self.vk);
        let slot = self.next_sync;
        if self.syncs[slot]
            .copy
            .as_ref()
            .is_some_and(|t| !t.poll_signaled(&vk))
        {
            return Ok(Presented::NotReady);
        }
        // Allocated before the acquire, so its failure cannot strand an image.
        #[cfg(test)]
        if std::mem::take(&mut self.fail_next_fence) {
            return Err(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY);
        }
        let fence = if self.maintenance1 {
            Some(unsafe {
                vk.device
                    .create_fence(&vk::FenceCreateInfo::default(), None)
            }?)
        } else {
            None
        };
        let drop_fence = |fence: Option<vk::Fence>| {
            if let Some(f) = fence {
                unsafe { vk.device.destroy_fence(f, None) };
            }
        };
        #[cfg(test)]
        if self.always_out_of_date {
            drop_fence(fence);
            self.out_of_date_count += 1;
            self.needs_rebuild = true;
            return Ok(Presented::OutOfDate);
        }
        let index = match self.chain.acquire(self.syncs[slot].acquire) {
            Ok(Acquire::Image { index, suboptimal }) => {
                self.needs_rebuild |= suboptimal;
                index
            }
            Ok(Acquire::NotReady) => {
                drop_fence(fence);
                return Ok(Presented::NotReady);
            }
            Ok(Acquire::OutOfDate) => {
                drop_fence(fence);
                self.needs_rebuild = true;
                return Ok(Presented::OutOfDate);
            }
            Err(e) => {
                drop_fence(fence);
                return Err(e);
            }
        };
        let (image, ready, image_extent) = {
            let sc = self.chain.current().expect("acquired from it");
            (
                sc.images[index as usize],
                sc.ready[index as usize],
                sc.extent,
            )
        };
        #[cfg(test)]
        let ticket = if std::mem::take(&mut self.fail_next_ticket) {
            Err(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY)
        } else {
            gpu.acquire_fence_ticket()
        };
        #[cfg(not(test))]
        let ticket = gpu.acquire_fence_ticket();
        let ticket = match ticket {
            Ok(ticket) => ticket,
            Err(e) => {
                drop_fence(fence);
                self.abandon(slot, None)?;
                return Err(e);
            }
        };
        let cb = self.syncs[slot].cb;
        let recorded = self.record_copy(store, backing, cb, image, image_extent);
        let submitted = recorded.and_then(|()| {
            #[cfg(test)]
            if std::mem::take(&mut self.fail_next_submit) {
                return Err(vk::Result::ERROR_OUT_OF_HOST_MEMORY);
            }
            let waits = [vk::SemaphoreSubmitInfo::default()
                .semaphore(self.syncs[slot].acquire)
                .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];
            let signals = [vk::SemaphoreSubmitInfo::default()
                .semaphore(ready)
                .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];
            let cbs = [vk::CommandBufferSubmitInfo::default().command_buffer(cb)];
            let submit = [vk::SubmitInfo2::default()
                .wait_semaphore_infos(&waits)
                .command_buffer_infos(&cbs)
                .signal_semaphore_infos(&signals)];
            crate::vk_count!(queue_submit2);
            // SAFETY: the queue is used from the core thread only.
            unsafe {
                vk.device
                    .queue_submit2(vk.graphics_queue, &submit, ticket.fence())
            }
        });
        self.next_sync = (slot + 1) % self.syncs.len();
        if let Err(e) = submitted {
            drop_fence(fence);
            self.abandon(slot, Some(ticket))?;
            return Err(e);
        }
        store.touch_render_fence(backing, ticket.clone());
        self.syncs[slot].copy = Some(ticket.clone());
        #[cfg(test)]
        {
            self.last_copy = Some(ticket.clone());
        }
        #[cfg(test)]
        let outcome = if std::mem::take(&mut self.fail_next_present) {
            Err(vk::Result::ERROR_OUT_OF_HOST_MEMORY)
        } else {
            self.chain.present_with_fence(index, fence)
        };
        #[cfg(not(test))]
        let outcome = self.chain.present_with_fence(index, fence);
        self.presents.push(PresentRecord {
            copy: ticket,
            fence,
        });
        self.prune(&vk);
        match outcome {
            // SUBOPTIMAL is a success: queued, its fence signals.
            Ok(Present::Queued { suboptimal }) => {
                self.needs_rebuild |= suboptimal;
                Ok(Presented::Shown)
            }
            // Maybe not queued (vkQueuePresentKHR may fail before it is): the
            // fence may never signal and the image may stay acquired. This
            // swapchain retires with a queue-idle.
            Ok(Present::OutOfDate) => {
                self.needs_rebuild = true;
                self.uncertain = true;
                Ok(Presented::OutOfDate)
            }
            Err(e) => {
                self.needs_rebuild = true;
                self.uncertain = true;
                Err(e)
            }
        }
    }

    /// An acquired image that will not be presented: consume its acquire
    /// semaphore with an empty batch (signalling `ticket`, else waiting for
    /// the queue to idle), then rebuild the swapchain. Never
    /// `vkReleaseSwapchainImagesEXT`: Mesa before 26.0 advertises it but its
    /// WSI (headless, macOS) calls a null `release_images`.
    fn abandon(&mut self, slot: usize, ticket: Option<FenceTicket>) -> Result<(), vk::Result> {
        let vk = &self.vk;
        let waits = [vk::SemaphoreSubmitInfo::default()
            .semaphore(self.syncs[slot].acquire)
            .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];
        let submit = [vk::SubmitInfo2::default().wait_semaphore_infos(&waits)];
        let fence = ticket
            .as_ref()
            .map_or(vk::Fence::null(), FenceTicket::fence);
        // SAFETY: the queue is used from the core thread only.
        unsafe { vk.device.queue_submit2(vk.graphics_queue, &submit, fence) }?;
        match ticket {
            Some(ticket) => self.syncs[slot].copy = Some(ticket),
            None => unsafe { vk.device.queue_wait_idle(vk.graphics_queue) }?,
        }
        self.needs_rebuild = true;
        Ok(())
    }

    /// Forget presents that finished.
    fn prune(&mut self, vk: &VkContext) {
        self.presents.retain(|p| {
            if p.done(vk) {
                if let Some(f) = p.fence {
                    unsafe { vk.device.destroy_fence(f, None) };
                }
                false
            } else {
                true
            }
        });
    }

    fn record_copy(
        &self,
        store: &mut DrawableStore,
        backing: DrawableId,
        cb: vk::CommandBuffer,
        image: vk::Image,
        image_extent: vk::Extent2D,
    ) -> Result<(), vk::Result> {
        let device = &self.vk.device;
        unsafe {
            device.reset_command_buffer(cb, vk::CommandBufferResetFlags::empty())?;
            device.begin_command_buffer(
                cb,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
        }
        let Some((src, src_extent)) = store.begin_external_read(backing, &self.vk, cb) else {
            unsafe { device.end_command_buffer(cb) }?;
            return Err(vk::Result::ERROR_UNKNOWN);
        };
        #[cfg(test)]
        let capture = self
            .capture
            .as_ref()
            .filter(|c| c.extent == image_extent && !c.taken.get());
        #[cfg(not(test))]
        let capture: Option<&()> = None;
        // SAFETY: a recording command buffer; both images are this device's.
        unsafe {
            record_backing_copy(
                device,
                cb,
                src,
                src_extent,
                image,
                image_extent,
                self.effective_scale(),
                if capture.is_some() {
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL
                } else {
                    vk::ImageLayout::PRESENT_SRC_KHR
                },
            );
        }
        #[cfg(test)]
        if let Some(capture) = capture {
            // SAFETY: as above; the buffer holds the whole image.
            unsafe { capture.record(device, cb, image) };
        }
        store.end_external_read(backing, &self.vk, cb);
        unsafe { device.end_command_buffer(cb) }
    }

    /// Hand everything to retirement; never destroys in place. `surface`
    /// (the window's) goes too and is destroyed after the last swapchain.
    pub(crate) fn retire(mut self, surface: Option<ToplevelId>) -> Vec<Retiring> {
        let mut out = std::mem::take(&mut self.replaced);
        let current = self.chain.take();
        out.push(Retiring {
            chain: Chain::new(&self.vk, self.surface),
            vk: Arc::clone(&self.vk),
            swapchains: current
                .map(|sc| (sc, std::mem::take(&mut self.presents)))
                .into_iter()
                .collect(),
            syncs: std::mem::take(&mut self.syncs),
            pool: std::mem::replace(&mut self.pool, vk::CommandPool::null()),
            surface: surface.map(|id| (id, self.surface)),
            wait_idle: !self.maintenance1 || self.uncertain,
            released: None,
        });
        out
    }
}

impl Drop for WindowSwapchain {
    fn drop(&mut self) {
        if self.pool == vk::CommandPool::null() {
            return;
        }
        // Teardown without `retire` (presenter dropped): finish everything.
        let syncs = std::mem::take(&mut self.syncs);
        let presents = std::mem::take(&mut self.presents);
        let mut retiring = Retiring {
            chain: Chain::new(&self.vk, self.surface),
            vk: Arc::clone(&self.vk),
            swapchains: self
                .chain
                .take()
                .map(|sc| (sc, presents))
                .into_iter()
                .collect(),
            syncs,
            pool: std::mem::replace(&mut self.pool, vk::CommandPool::null()),
            surface: None,
            wait_idle: true,
            released: None,
        };
        retiring.try_destroy();
    }
}

/// Swapchains (and, at unmap, a window's syncs and surface) waiting until
/// the GPU and the presentation engine are done with them.
pub(crate) struct Retiring {
    chain: Chain,
    vk: Arc<VkContext>,
    swapchains: Vec<(Swapchain, Vec<PresentRecord>)>,
    syncs: Vec<FrameSync>,
    pool: vk::CommandPool,
    surface: Option<(ToplevelId, vk::SurfaceKHR)>,
    /// No present fences to poll: one `vkQueueWaitIdle` instead.
    wait_idle: bool,
    released: Option<ToplevelId>,
}

impl Retiring {
    /// A window's surface alone (no swapchain was ever built on it).
    pub(crate) fn surface_only(
        vk: &Arc<VkContext>,
        id: ToplevelId,
        surface: vk::SurfaceKHR,
    ) -> Self {
        Self {
            chain: Chain::new(vk, surface),
            vk: Arc::clone(vk),
            swapchains: Vec::new(),
            syncs: Vec::new(),
            pool: vk::CommandPool::null(),
            surface: Some((id, surface)),
            wait_idle: false,
            released: None,
        }
    }

    fn done(&self) -> bool {
        self.swapchains
            .iter()
            .all(|(_, presents)| presents.iter().all(|p| p.done(&self.vk)))
            && self
                .syncs
                .iter()
                .all(|s| s.copy.as_ref().is_none_or(|t| t.poll_signaled(&self.vk)))
    }

    /// Destroy everything once finished: a status poll with present
    /// fences, else one queue-idle. `true` when gone.
    pub(crate) fn try_destroy(&mut self) -> bool {
        if self.wait_idle {
            #[cfg(test)]
            QUEUE_IDLES.with(|n| n.set(n.get() + 1));
            // SAFETY: the queue is used from the core thread only.
            if let Err(e) = unsafe { self.vk.device.queue_wait_idle(self.vk.graphics_queue) } {
                log::warn!("rootless: queue idle before a swapchain retirement: {e:?}");
            }
            self.wait_idle = false;
        } else if !self.done() {
            return false;
        }
        let device = &self.vk.device;
        for (swapchain, presents) in self.swapchains.drain(..) {
            for p in presents {
                if let Some(f) = p.fence {
                    unsafe { device.destroy_fence(f, None) };
                }
            }
            self.chain.destroy(Some(swapchain));
        }
        for sync in self.syncs.drain(..) {
            unsafe { device.destroy_semaphore(sync.acquire, None) };
        }
        if self.pool != vk::CommandPool::null() {
            unsafe { device.destroy_command_pool(self.pool, None) };
            self.pool = vk::CommandPool::null();
        }
        if let Some((id, surface)) = self.surface.take() {
            unsafe { self.chain.surface_fns().destroy_surface(surface, None) };
            self.released = Some(id);
        }
        true
    }

    /// The window whose surface this retirement freed.
    pub(crate) fn released(&self) -> Option<ToplevelId> {
        self.released
    }
}

impl Drop for Retiring {
    fn drop(&mut self) {
        if !self.swapchains.is_empty()
            || !self.syncs.is_empty()
            || self.pool != vk::CommandPool::null()
            || self.surface.is_some()
        {
            // Dropped before it finished (presenter teardown).
            self.wait_idle = true;
            self.try_destroy();
        }
    }
}

/// A host-visible copy of the image the next present shows (tests).
#[cfg(test)]
pub(crate) struct Capture {
    vk: Arc<VkContext>,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    pub(crate) extent: vk::Extent2D,
    taken: std::cell::Cell<bool>,
}

#[cfg(test)]
impl Capture {
    /// A buffer for one `extent` BGRA image.
    pub(crate) fn new(vk: &Arc<VkContext>, extent: vk::Extent2D) -> Self {
        let size = u64::from(extent.width) * u64::from(extent.height) * 4;
        let buffer = unsafe {
            vk.device.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(size)
                    .usage(vk::BufferUsageFlags::TRANSFER_DST),
                None,
            )
        }
        .expect("capture buffer");
        let reqs = unsafe { vk.device.get_buffer_memory_requirements(buffer) };
        let props = unsafe {
            vk.instance
                .get_physical_device_memory_properties(vk.physical_device)
        };
        let want = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        let index = (0..props.memory_type_count)
            .find(|&i| {
                reqs.memory_type_bits & (1 << i) != 0
                    && props.memory_types[i as usize].property_flags.contains(want)
            })
            .expect("host-visible memory");
        let memory = crate::kms::vk::mem_accounting::allocate_memory(
            &vk.device,
            &vk::MemoryAllocateInfo::default()
                .allocation_size(reqs.size)
                .memory_type_index(index),
            crate::kms::vk::mem_accounting::MemCategory::Staging,
            &props,
        )
        .expect("capture memory");
        unsafe { vk.device.bind_buffer_memory(buffer, memory, 0) }.expect("bind");
        Self {
            vk: Arc::clone(vk),
            buffer,
            memory,
            extent,
            taken: std::cell::Cell::new(false),
        }
    }

    /// Copy `image` (`TRANSFER_SRC_OPTIMAL`) into the buffer, then hand the
    /// image to the present.
    unsafe fn record(&self, device: &ash::Device, cb: vk::CommandBuffer, image: vk::Image) {
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        let written = [vk::MemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
            .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::COPY)
            .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)];
        let region = [vk::BufferImageCopy::default()
            .image_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
            )
            .image_extent(vk::Extent3D {
                width: self.extent.width,
                height: self.extent.height,
                depth: 1,
            })];
        let to_present = [vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::COPY)
            .src_access_mask(vk::AccessFlags2::TRANSFER_READ)
            .dst_stage_mask(vk::PipelineStageFlags2::NONE)
            .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .new_layout(vk::ImageLayout::PRESENT_SRC_KHR)
            .image(image)
            .subresource_range(range)];
        unsafe {
            device.cmd_pipeline_barrier2(
                cb,
                &vk::DependencyInfo::default().memory_barriers(&written),
            );
            device.cmd_copy_image_to_buffer(
                cb,
                image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                self.buffer,
                &region,
            );
            device.cmd_pipeline_barrier2(
                cb,
                &vk::DependencyInfo::default().image_memory_barriers(&to_present),
            );
        }
        self.taken.set(true);
    }

    /// The captured bytes, once the copy's fence signalled.
    pub(crate) fn bytes(&self) -> Option<Vec<u8>> {
        if !self.taken.get() {
            return None;
        }
        let size = u64::from(self.extent.width) * u64::from(self.extent.height) * 4;
        let ptr = unsafe {
            self.vk
                .device
                .map_memory(self.memory, 0, size, vk::MemoryMapFlags::empty())
        }
        .ok()?;
        let bytes = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), size as usize) }.to_vec();
        unsafe { self.vk.device.unmap_memory(self.memory) };
        Some(bytes)
    }
}

#[cfg(test)]
impl Drop for Capture {
    fn drop(&mut self) {
        unsafe {
            self.vk.device.destroy_buffer(self.buffer, None);
            crate::kms::vk::mem_accounting::free_memory(&self.vk.device, self.memory);
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Retirement queue-idles on this thread.
    pub(crate) static QUEUE_IDLES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Copy `src` (`TRANSFER_SRC_OPTIMAL`) into `image` at the origin, as much
/// as fits, clearing the rest, and leave `image` in `final_layout`.
///
/// # Safety
///
/// `cb` is recording; both images belong to `device`; a `scale` above 1
/// needs both formats to blit.
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn record_backing_copy(
    device: &ash::Device,
    cb: vk::CommandBuffer,
    src: vk::Image,
    src_extent: vk::Extent2D,
    image: vk::Image,
    image_extent: vk::Extent2D,
    scale: u32,
    final_layout: vk::ImageLayout,
) {
    let range = vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1);
    let to_dst = [vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
        .dst_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
        .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
        .old_layout(vk::ImageLayout::UNDEFINED)
        .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
        .image(image)
        .subresource_range(range)];
    let region = copy_region(src_extent, image_extent, scale);
    let layers = vk::ImageSubresourceLayers::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .layer_count(1);
    unsafe {
        device.cmd_pipeline_barrier2(
            cb,
            &vk::DependencyInfo::default().image_memory_barriers(&to_dst),
        );
        if region.clear {
            device.cmd_clear_color_image(
                cb,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &vk::ClearColorValue {
                    float32: [0.0, 0.0, 0.0, 0.0],
                },
                &[range],
            );
            let cleared = [vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::CLEAR)
                .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
                .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .image(image)
                .subresource_range(range)];
            device.cmd_pipeline_barrier2(
                cb,
                &vk::DependencyInfo::default().image_memory_barriers(&cleared),
            );
        }
        if region.src.width == 0 || region.src.height == 0 {
            // Nothing of the backing fits.
        } else if region.scale == 1 {
            let copy = [vk::ImageCopy::default()
                .src_subresource(layers)
                .dst_subresource(layers)
                .extent(vk::Extent3D {
                    width: region.src.width,
                    height: region.src.height,
                    depth: 1,
                })];
            device.cmd_copy_image(
                cb,
                src,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &copy,
            );
        } else {
            let corner = |extent: vk::Extent2D| vk::Offset3D {
                x: i32::try_from(extent.width).unwrap_or(i32::MAX),
                y: i32::try_from(extent.height).unwrap_or(i32::MAX),
                z: 1,
            };
            let blit = [vk::ImageBlit::default()
                .src_subresource(layers)
                .src_offsets([vk::Offset3D::default(), corner(region.src)])
                .dst_subresource(layers)
                .dst_offsets([vk::Offset3D::default(), corner(region.dst)])];
            device.cmd_blit_image(
                cb,
                src,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &blit,
                vk::Filter::NEAREST,
            );
        }
        let after = [vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
            .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::NONE)
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(final_layout)
            .image(image)
            .subresource_range(range)];
        device.cmd_pipeline_barrier2(
            cb,
            &vk::DependencyInfo::default().image_memory_barriers(&after),
        );
    }
}
