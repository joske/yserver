//! One window-system surface: its Vulkan swapchain, the offscreen slots the
//! scene composes into, and the copy of a composed slot into a swapchain
//! image followed by `vkQueuePresentKHR`. The slots are X-sized; at a copy
//! scale above 1 the copy is a nearest-filter blit to that many image
//! pixels per slot pixel.
//!
//! The compose is submitted first; the copy is a second submit on the same
//! queue, after a non-blocking acquire. A failed acquire (no image yet,
//! hidden window) leaves the composed slot pending, and the next tick
//! retries, so a compose never strands an acquired image or semaphore.

use std::{ffi::CStr, sync::Arc};

use ash::vk;

use super::{
    clock::FrameClock,
    slots::{SlotPhase, SlotRing, SlotState},
};
use crate::kms::{
    render::scene::{
        ComposeRenderTarget, FinishOutcome, FlipRetirement, FrameHandoff, FrameTargetError,
        OffscreenTarget, RecordFrame, SlotToken,
    },
    vk::{
        compositor::PresentError,
        device::{SurfaceProbe, VkContext, VkContextOptions},
    },
};

/// Slots per surface: the latest on screen and the next one. The scene keeps
/// one frame pending per output, so a third slot would never be composed.
const SLOT_COUNT: usize = 2;
/// Acquire semaphore + copy fence pairs in rotation.
const SYNC_COUNT: usize = 3;
/// The slots' format (`OffscreenTarget`).
const SLOT_FORMAT: vk::Format = vk::Format::B8G8R8A8_UNORM;

/// Creates a window-system surface on a Vulkan instance.
pub(crate) type SurfaceCreator =
    dyn Fn(&ash::Entry, &ash::Instance) -> Result<vk::SurfaceKHR, vk::Result> + Send;

/// Commits what the driver changed on the shell's surface from the calling
/// thread.
pub(crate) type SurfaceCommit = dyn Fn() + Send + Sync;

/// A shell's surface: its instance extension, how to create it and how to
/// commit the driver's changes to it.
pub(crate) struct NativeSurface {
    extension: &'static CStr,
    create: Box<SurfaceCreator>,
    commit: Option<Arc<SurfaceCommit>>,
}

impl NativeSurface {
    pub(crate) fn new(
        extension: &'static CStr,
        create: impl Fn(&ash::Entry, &ash::Instance) -> Result<vk::SurfaceKHR, vk::Result>
        + Send
        + 'static,
    ) -> Self {
        Self {
            extension,
            create: Box::new(create),
            commit: None,
        }
    }

    /// Run `commit` on the core thread after each swapchain change.
    #[cfg_attr(all(kms, not(test)), allow(dead_code))] // A window shell's.
    #[must_use]
    pub(crate) fn with_commit(mut self, commit: impl Fn() + Send + Sync + 'static) -> Self {
        self.commit = Some(Arc::new(commit));
        self
    }

    /// `VK_EXT_headless_surface`: presents nowhere (tests).
    #[cfg(test)]
    pub(crate) fn headless() -> Self {
        Self::new(ash::ext::headless_surface::NAME, |entry, instance| {
            let loader = ash::ext::headless_surface::Instance::new(entry, instance);
            // SAFETY: a valid instance with the extension enabled.
            unsafe {
                loader.create_headless_surface(&vk::HeadlessSurfaceCreateInfoEXT::default(), None)
            }
        })
    }

    /// The context a presenting consumer of this surface needs.
    pub(crate) fn context_options(&self) -> VkContextOptions {
        VkContextOptions {
            wants_present: true,
            surface_extension: Some(self.extension),
            wants_kms_interop: false,
        }
    }

    /// A context on a device that presents to this surface.
    ///
    /// # Errors
    ///
    /// No such device, or Vulkan failed.
    pub(crate) fn create_context(&self) -> std::io::Result<Arc<VkContext>> {
        let mut probe =
            |entry: &ash::Entry, instance: &ash::Instance| (self.create)(entry, instance);
        let probe: &mut SurfaceProbe<'_> = &mut probe;
        VkContext::new_with_surface(self.context_options(), probe)
            .map_err(|e| std::io::Error::other(format!("vulkan: {e}")))
    }
}

/// An acquire semaphore and the fence of the copy that waited on it.
struct FrameSync {
    acquire: vk::Semaphore,
    copied: vk::Fence,
}

/// The swapchain and its per-image present semaphores.
struct Swapchain {
    handle: vk::SwapchainKHR,
    images: Vec<vk::Image>,
    extent: vk::Extent2D,
    /// Signalled by the copy into image `i`, waited by its present.
    ready: Vec<vk::Semaphore>,
}

/// One surface: swapchain, slots, clock.
pub(crate) struct SurfaceOutput {
    vk: Arc<VkContext>,
    surface_fns: ash::khr::surface::Instance,
    swapchain_fns: ash::khr::swapchain::Device,
    surface: vk::SurfaceKHR,
    swapchain: Option<Swapchain>,
    /// The X output size, which is the slot size.
    extent: vk::Extent2D,
    targets: Vec<OffscreenTarget>,
    copy_command_buffers: Vec<vk::CommandBuffer>,
    command_pool: vk::CommandPool,
    slots: SlotRing,
    syncs: Vec<FrameSync>,
    next_sync: usize,
    /// The sync of the presented pending frame.
    pending_sync: Option<usize>,
    needs_recreate: bool,
    hidden: bool,
    /// Swapchain pixels per slot pixel per axis, as the shell asked.
    copy_scale: u32,
    /// The device blits slot → swapchain image; without it the copy stays
    /// 1:1 whatever the scale.
    blit: bool,
    blit_warned: bool,
    clock: Arc<FrameClock>,
    commit: Option<Arc<SurfaceCommit>>,
    #[cfg(test)]
    frames_presented: u64,
    #[cfg(test)]
    pub(super) trace: Vec<&'static str>,
}

impl SurfaceOutput {
    /// A surface of `extent` pixels on `native`, presenting through `vk`.
    ///
    /// # Errors
    ///
    /// The surface, swapchain, slots or sync objects could not be created.
    pub(crate) fn new(
        vk: Arc<VkContext>,
        native: &NativeSurface,
        extent: vk::Extent2D,
        clock: Arc<FrameClock>,
    ) -> Result<Self, vk::Result> {
        let surface_fns = ash::khr::surface::Instance::new(&vk.entry, &vk.instance);
        let swapchain_fns = ash::khr::swapchain::Device::new(&vk.instance, &vk.device);
        let surface = (native.create)(&vk.entry, &vk.instance)?;
        // From here `Drop` releases whatever exists.
        let mut output = Self {
            vk: Arc::clone(&vk),
            surface_fns,
            swapchain_fns,
            surface,
            swapchain: None,
            extent,
            targets: Vec::new(),
            copy_command_buffers: Vec::new(),
            command_pool: vk::CommandPool::null(),
            slots: SlotRing::new(SLOT_COUNT),
            syncs: Vec::new(),
            next_sync: 0,
            pending_sync: None,
            needs_recreate: false,
            hidden: false,
            copy_scale: 1,
            blit: false,
            blit_warned: false,
            clock,
            commit: native.commit.clone(),
            #[cfg(test)]
            frames_presented: 0,
            #[cfg(test)]
            trace: Vec::new(),
        };
        // SAFETY: the surface belongs to this instance and device.
        let supported = unsafe {
            output.surface_fns.get_physical_device_surface_support(
                vk.physical_device,
                vk.graphics_queue_family,
                surface,
            )
        }?;
        if !supported {
            return Err(vk::Result::ERROR_SURFACE_LOST_KHR);
        }
        for _ in 0..SLOT_COUNT {
            output
                .targets
                .push(OffscreenTarget::new(Arc::clone(&vk), extent)?);
        }
        let pool_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(vk.graphics_queue_family)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        output.command_pool = unsafe { vk.device.create_command_pool(&pool_info, None) }?;
        let cb_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(output.command_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(u32::try_from(SLOT_COUNT).unwrap_or(u32::MAX));
        output.copy_command_buffers = unsafe { vk.device.allocate_command_buffers(&cb_info) }?;
        for _ in 0..SYNC_COUNT {
            let acquire = unsafe {
                vk.device
                    .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
            }?;
            let copied = match unsafe {
                vk.device.create_fence(
                    &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
                    None,
                )
            } {
                Ok(fence) => fence,
                Err(e) => {
                    unsafe { vk.device.destroy_semaphore(acquire, None) };
                    return Err(e);
                }
            };
            output.syncs.push(FrameSync { acquire, copied });
        }
        output.recreate_swapchain()?;
        Ok(output)
    }

    /// The scene's slot count.
    pub(crate) fn slot_count(&self) -> usize {
        self.slots.len()
    }

    /// A free slot to compose into; none while hidden or a frame is pending.
    pub(crate) fn acquire_slot(&mut self, output_idx: usize) -> Option<SlotToken> {
        if self.hidden || self.slots.pending().is_some() {
            return None;
        }
        let bo_idx = self.slots.first_free()?;
        let SlotState {
            last_present_generation,
            content_invalidated,
            ..
        } = *self.slots.get(bo_idx)?;
        Some(SlotToken {
            output_idx,
            bo_idx,
            extent: self.extent,
            last_present_generation,
            content_invalidated,
        })
    }

    pub(crate) fn slot_image(&self, bo_idx: usize) -> Option<vk::Image> {
        self.targets.get(bo_idx).map(ComposeRenderTarget::image)
    }

    pub(crate) fn has_slot(&self, bo_idx: usize) -> bool {
        bo_idx < self.targets.len()
    }

    /// The latest retired frame's image (`GENERAL` layout).
    pub(crate) fn on_screen_image(&self) -> Option<vk::Image> {
        self.slots.on_screen().and_then(|idx| self.slot_image(idx))
    }

    /// Compose into slot `bo_idx`, then try to present it.
    pub(crate) fn finish_frame(
        &mut self,
        bo_idx: usize,
        record: &mut RecordFrame<'_>,
    ) -> FinishOutcome {
        if !self.has_slot(bo_idx) || !self.slots.begin_recording(bo_idx) {
            return FinishOutcome::Unavailable(FrameTargetError::NoSlot);
        }
        #[cfg(test)]
        self.trace.push("record");
        let target = &mut self.targets[bo_idx];
        let result = record(target);
        let previous_gpu_ns = target.take_last_gpu_render_ns();
        let result = result.map(|submitted| {
            self.slots.composed(bo_idx);
            if let Err(e) = self.present_pending() {
                log::warn!("surface: present failed: {e:?}");
            }
            self.clock.arm_tick();
            FrameHandoff::FlipPending(submitted)
        });
        let fail_stop = matches!(result, Err(PresentError::Vk(vk::Result::ERROR_DEVICE_LOST)));
        FinishOutcome::Finished {
            result,
            previous_gpu_ns,
            fail_stop,
        }
    }

    /// Copy the pending composed slot into a swapchain image and present
    /// it; `Ok(false)` when no image (or sync) is free yet or the surface
    /// is hidden.
    ///
    /// # Errors
    ///
    /// Vulkan failed on acquire, submit or present.
    pub(crate) fn present_pending(&mut self) -> Result<bool, vk::Result> {
        let Some((bo_idx, SlotPhase::Composed)) = self.slots.pending() else {
            return Ok(false);
        };
        if self.hidden {
            return Ok(false);
        }
        if self.needs_recreate || self.swapchain.is_none() {
            self.recreate_swapchain()?;
            if self.swapchain.is_none() {
                return Ok(false);
            }
        }
        let sync_idx = self.next_sync;
        let sync = &self.syncs[sync_idx];
        // SAFETY: a fence of this device.
        if !unsafe { self.vk.device.get_fence_status(sync.copied) }? {
            return Ok(false);
        }
        let Some(swapchain) = self.swapchain.as_ref() else {
            return Ok(false);
        };
        // SAFETY: the swapchain and an unsignalled acquire semaphore (its last
        // wait completed: the fence above signalled).
        let acquired = unsafe {
            self.swapchain_fns.acquire_next_image(
                swapchain.handle,
                0,
                sync.acquire,
                vk::Fence::null(),
            )
        };
        let image_idx = match acquired {
            Ok((image_idx, suboptimal)) => {
                self.needs_recreate |= suboptimal;
                image_idx
            }
            Err(vk::Result::NOT_READY | vk::Result::TIMEOUT) => return Ok(false),
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                self.note_out_of_date();
                return Ok(false);
            }
            Err(e) => return Err(e),
        };
        #[cfg(test)]
        self.trace.push("acquire");
        let image = swapchain.images[image_idx as usize];
        let ready = swapchain.ready[image_idx as usize];
        let image_extent = swapchain.extent;
        let handle = swapchain.handle;
        let cb = self.copy_command_buffers[bo_idx];
        let region = copy_region(self.extent, image_extent, self.effective_scale());
        let device = &self.vk.device;
        // SAFETY: a command buffer of this device that no pending submit
        // uses (its slot's previous copy fence signalled).
        unsafe {
            device.reset_command_buffer(cb, vk::CommandBufferResetFlags::empty())?;
            device.begin_command_buffer(
                cb,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
            record_slot_copy(
                device,
                cb,
                self.targets[bo_idx].image(),
                image,
                region,
                vk::ImageLayout::PRESENT_SRC_KHR,
            );
            device.end_command_buffer(cb)?;
        }

        let device = &self.vk.device;
        unsafe { device.reset_fences(&[sync.copied]) }?;
        let waits = [vk::SemaphoreSubmitInfo::default()
            .semaphore(sync.acquire)
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
            device.queue_submit2(self.vk.graphics_queue, &submit, sync.copied)?;
        }
        #[cfg(test)]
        self.trace.push("copy-submit");
        self.next_sync = (sync_idx + 1) % self.syncs.len();
        self.pending_sync = Some(sync_idx);
        self.slots.presented();

        let swapchains = [handle];
        let indices = [image_idx];
        let present_waits = [ready];
        let present = vk::PresentInfoKHR::default()
            .wait_semaphores(&present_waits)
            .swapchains(&swapchains)
            .image_indices(&indices);
        // SAFETY: the image was acquired above and its copy submitted.
        match unsafe {
            self.swapchain_fns
                .queue_present(self.vk.graphics_queue, &present)
        } {
            Ok(suboptimal) => self.needs_recreate |= suboptimal,
            // The present's semaphore wait still executes.
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => self.note_out_of_date(),
            Err(e) => return Err(e),
        }
        #[cfg(test)]
        {
            self.trace.push("present");
            self.frames_presented += 1;
        }
        Ok(true)
    }

    /// Slots of `extent` for [`Self::install_slots`]: the fallible half of
    /// an output resize. Dropping them frees them.
    ///
    /// # Errors
    ///
    /// A slot could not be allocated.
    pub(crate) fn prepare_slots(
        &self,
        extent: vk::Extent2D,
    ) -> Result<Vec<OffscreenTarget>, vk::Result> {
        if crate::kms::render::resize_fault(crate::kms::render::ResizeFault::Alloc) {
            return Err(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY);
        }
        (0..SLOT_COUNT)
            .map(|_| OffscreenTarget::new(Arc::clone(&self.vk), extent))
            .collect()
    }

    /// The output is now `extent`: replace the slots with `targets`,
    /// dropping their frames (the next compose repaints in full), and
    /// rebuild the swapchain before the next present. The caller drained
    /// the scene.
    pub(crate) fn install_slots(&mut self, targets: Vec<OffscreenTarget>, extent: vk::Extent2D) {
        // No compose or copy may still use the old slots.
        if let Err(e) = unsafe { self.vk.device.queue_wait_idle(self.vk.graphics_queue) } {
            log::error!("surface: queue idle before a slot swap failed: {e:?}");
        }
        self.targets = targets;
        self.extent = extent;
        self.slots = SlotRing::new(SLOT_COUNT);
        self.pending_sync = None;
        self.needs_recreate = true;
    }

    /// The swapchain no longer matches the surface: rebuild it before the
    /// next present. The slots keep their pixels (they are X-sized).
    pub(crate) fn note_out_of_date(&mut self) {
        self.needs_recreate = true;
    }

    /// Show each slot pixel as `scale`x`scale` swapchain pixels from the
    /// next present on.
    pub(crate) fn set_copy_scale(&mut self, scale: u32) {
        let scale = scale.max(1);
        if scale != self.copy_scale {
            self.copy_scale = scale;
            self.needs_recreate = true;
        }
    }

    /// The copy scale the device can do.
    fn effective_scale(&self) -> u32 {
        if self.blit { self.copy_scale } else { 1 }
    }

    /// Whether the pending frame may retire: its copy finished, or it was
    /// composed while hidden and will not be presented.
    pub(crate) fn retirable(&self) -> bool {
        match self.slots.pending() {
            Some((_, SlotPhase::Presented)) => self.pending_sync.is_none_or(|idx| {
                // SAFETY: a fence of this device.
                unsafe { self.vk.device.get_fence_status(self.syncs[idx].copied) }.unwrap_or(true)
            }),
            Some((_, SlotPhase::Composed)) => self.hidden,
            _ => false,
        }
    }

    /// Whether a frame awaits presentation or retirement.
    pub(crate) fn has_pending(&self) -> bool {
        self.slots.pending().is_some()
    }

    /// Retire the pending frame (called by the scene's flip handler).
    pub(crate) fn on_page_flip_complete(&mut self) -> Option<FlipRetirement> {
        if !self.retirable() {
            return None;
        }
        let (retired, presented) = self.slots.retire()?;
        self.pending_sync = None;
        #[cfg(test)]
        self.trace.push("retire");
        Some(FlipRetirement {
            retired_bo_idx: retired,
            presented_bo_idx: presented,
            generation: 0,
        })
    }

    pub(crate) fn commit_slot_present(&mut self, bo_idx: usize, generation: u64) {
        self.slots.commit(bo_idx, generation);
    }

    pub(crate) fn invalidate_slot(&mut self, bo_idx: usize) {
        self.slots.invalidate(bo_idx);
    }

    pub(crate) fn release_slot(&mut self, bo_idx: usize) {
        self.slots.release(bo_idx);
    }

    /// The window was occluded or minimised (`true`), or shown again.
    pub(crate) fn set_hidden(&mut self, hidden: bool) {
        self.hidden = hidden;
    }

    pub(crate) fn hidden(&self) -> bool {
        self.hidden
    }

    pub(crate) fn clock(&self) -> &Arc<FrameClock> {
        &self.clock
    }

    /// Frames copied into the swapchain and queued for present.
    #[cfg(test)]
    pub(crate) fn frames_presented(&self) -> u64 {
        self.frames_presented
    }

    /// The slot extent and the swapchain's (none while there is none).
    #[cfg(test)]
    pub(crate) fn extents(&self) -> (vk::Extent2D, Option<vk::Extent2D>) {
        (self.extent, self.swapchain.as_ref().map(|s| s.extent))
    }

    #[cfg(test)]
    pub(crate) fn vk(&self) -> &Arc<VkContext> {
        &self.vk
    }

    /// Copy the on-screen slot into `image` (`image_extent`, left
    /// `GENERAL`) as a present at `scale` would, and wait for it.
    #[cfg(test)]
    pub(crate) fn copy_on_screen_into(
        &self,
        image: vk::Image,
        image_extent: vk::Extent2D,
        scale: u32,
    ) -> Result<(), vk::Result> {
        let slot = self.on_screen_image().ok_or(vk::Result::ERROR_UNKNOWN)?;
        let region = copy_region(self.extent, image_extent, scale);
        crate::kms::vk::ops::run_one_shot_op(&self.vk, self.command_pool, |vk, cb| {
            // SAFETY: a recording command buffer; both images are this
            // device's and match `region`.
            unsafe {
                record_slot_copy(
                    &vk.device,
                    cb,
                    slot,
                    image,
                    region,
                    vk::ImageLayout::GENERAL,
                );
            }
            Ok(())
        })
    }

    /// The current swapchain handle (null while there is none).
    #[cfg(test)]
    pub(crate) fn swapchain_handle(&self) -> vk::SwapchainKHR {
        self.swapchain
            .as_ref()
            .map_or(vk::SwapchainKHR::null(), |s| s.handle)
    }

    /// (Re)create the swapchain for the surface's current size; none while
    /// the surface has zero size (minimised). The driver's changes to the
    /// surface are committed after, whatever the outcome.
    fn recreate_swapchain(&mut self) -> Result<(), vk::Result> {
        let result = self.rebuild_swapchain();
        if let Some(commit) = &self.commit {
            commit();
        }
        result
    }

    fn rebuild_swapchain(&mut self) -> Result<(), vk::Result> {
        let pd = self.vk.physical_device;
        // SAFETY: the surface belongs to this instance.
        let caps = unsafe {
            self.surface_fns
                .get_physical_device_surface_capabilities(pd, self.surface)
        }?;
        let formats = unsafe {
            self.surface_fns
                .get_physical_device_surface_formats(pd, self.surface)
        }?;
        let format = pick_surface_format(&formats);
        self.blit = format.is_some_and(|format| self.blits_to(format.format));
        if self.copy_scale > 1 && !self.blit && !self.blit_warned {
            self.blit_warned = true;
            log::warn!(
                "surface: no slot → swapchain blit on this device; scale {} shown 1:1",
                self.copy_scale
            );
        }
        let scale = self.effective_scale();
        let extent = if caps.current_extent.width == u32::MAX {
            vk::Extent2D {
                width: self
                    .extent
                    .width
                    .saturating_mul(scale)
                    .clamp(caps.min_image_extent.width, caps.max_image_extent.width),
                height: self
                    .extent
                    .height
                    .saturating_mul(scale)
                    .clamp(caps.min_image_extent.height, caps.max_image_extent.height),
            }
        } else {
            caps.current_extent
        };
        // Wait for every copy and present before the old swapchain goes.
        unsafe { self.vk.device.queue_wait_idle(self.vk.graphics_queue) }?;
        let old = self.swapchain.take();
        self.needs_recreate = false;
        if extent.width == 0 || extent.height == 0 {
            self.destroy_swapchain(old);
            return Ok(());
        }
        if !caps
            .supported_usage_flags
            .contains(vk::ImageUsageFlags::TRANSFER_DST)
        {
            self.destroy_swapchain(old);
            return Err(vk::Result::ERROR_FEATURE_NOT_PRESENT);
        }
        let Some(format) = format else {
            self.destroy_swapchain(old);
            return Err(vk::Result::ERROR_FORMAT_NOT_SUPPORTED);
        };
        let mut image_count = caps.min_image_count.max(3);
        if caps.max_image_count != 0 {
            image_count = image_count.min(caps.max_image_count);
        }
        let alpha = [
            vk::CompositeAlphaFlagsKHR::OPAQUE,
            vk::CompositeAlphaFlagsKHR::INHERIT,
            vk::CompositeAlphaFlagsKHR::PRE_MULTIPLIED,
            vk::CompositeAlphaFlagsKHR::POST_MULTIPLIED,
        ]
        .into_iter()
        .find(|flag| caps.supported_composite_alpha.contains(*flag))
        .unwrap_or(vk::CompositeAlphaFlagsKHR::OPAQUE);
        let info = vk::SwapchainCreateInfoKHR::default()
            .surface(self.surface)
            .min_image_count(image_count)
            .image_format(format.format)
            .image_color_space(format.color_space)
            .image_extent(extent)
            .image_array_layers(1)
            .image_usage(vk::ImageUsageFlags::TRANSFER_DST)
            .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
            .pre_transform(caps.current_transform)
            .composite_alpha(alpha)
            .present_mode(vk::PresentModeKHR::FIFO)
            .clipped(true)
            .old_swapchain(old.as_ref().map_or(vk::SwapchainKHR::null(), |s| s.handle));
        // SAFETY: valid surface and (optional) retired old swapchain.
        let created = unsafe { self.swapchain_fns.create_swapchain(&info, None) };
        self.destroy_swapchain(old);
        let handle = created?;
        let images = match unsafe { self.swapchain_fns.get_swapchain_images(handle) } {
            Ok(images) => images,
            Err(e) => {
                unsafe { self.swapchain_fns.destroy_swapchain(handle, None) };
                return Err(e);
            }
        };
        let mut ready = Vec::with_capacity(images.len());
        for _ in &images {
            match unsafe {
                self.vk
                    .device
                    .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
            } {
                Ok(semaphore) => ready.push(semaphore),
                Err(e) => {
                    self.destroy_swapchain(Some(Swapchain {
                        handle,
                        images,
                        extent,
                        ready,
                    }));
                    return Err(e);
                }
            }
        }
        log::info!(
            "surface: swapchain {}x{} with {} images ({:?})",
            extent.width,
            extent.height,
            images.len(),
            format.format
        );
        self.swapchain = Some(Swapchain {
            handle,
            images,
            extent,
            ready,
        });
        Ok(())
    }

    /// Whether a slot blits into an optimal-tiling `format` image.
    fn blits_to(&self, format: vk::Format) -> bool {
        let features = |format| {
            // SAFETY: a physical device of this instance.
            unsafe {
                self.vk
                    .instance
                    .get_physical_device_format_properties(self.vk.physical_device, format)
            }
            .optimal_tiling_features
        };
        features(SLOT_FORMAT).contains(vk::FormatFeatureFlags::BLIT_SRC)
            && features(format).contains(vk::FormatFeatureFlags::BLIT_DST)
    }

    /// Destroy a swapchain whose presents and copies have completed.
    fn destroy_swapchain(&self, swapchain: Option<Swapchain>) {
        let Some(swapchain) = swapchain else {
            return;
        };
        unsafe {
            for semaphore in swapchain.ready {
                self.vk.device.destroy_semaphore(semaphore, None);
            }
            self.swapchain_fns.destroy_swapchain(swapchain.handle, None);
        }
    }
}

/// Where a slot lands in a swapchain image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CopyRegion {
    /// The slot's top-left part that is shown.
    pub(super) src: vk::Extent2D,
    /// The image's top-left part it covers, `src` times the scale.
    pub(super) dst: vk::Extent2D,
    pub(super) scale: u32,
    /// The rest of the image is cleared.
    pub(super) clear: bool,
}

/// A `slot`-sized slot in an `image`-sized image at `scale` image pixels
/// per slot pixel: as much of the slot as fits whole.
pub(super) fn copy_region(slot: vk::Extent2D, image: vk::Extent2D, scale: u32) -> CopyRegion {
    let scale = scale.max(1);
    let src = vk::Extent2D {
        width: slot.width.min(image.width / scale),
        height: slot.height.min(image.height / scale),
    };
    let dst = vk::Extent2D {
        width: src.width * scale,
        height: src.height * scale,
    };
    CopyRegion {
        src,
        dst,
        scale,
        clear: dst != image,
    }
}

/// Record `region` of `slot` (`GENERAL`) into `image`, leaving `image` in
/// `final_layout`; the acquire wait covers the image's previous use.
///
/// # Safety
///
/// `cb` is recording; both images belong to `device` and match `region`.
pub(super) unsafe fn record_slot_copy(
    device: &ash::Device,
    cb: vk::CommandBuffer,
    slot: vk::Image,
    image: vk::Image,
    region: CopyRegion,
    final_layout: vk::ImageLayout,
) {
    let range = vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1);
    let transfer = vk::PipelineStageFlags2::ALL_TRANSFER;
    let before = [
        vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .src_access_mask(vk::AccessFlags2::MEMORY_WRITE)
            .dst_stage_mask(transfer)
            .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
            .old_layout(vk::ImageLayout::GENERAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .image(slot)
            .subresource_range(range),
        vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .src_access_mask(vk::AccessFlags2::empty())
            .dst_stage_mask(transfer)
            .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .image(image)
            .subresource_range(range),
    ];
    let layers = vk::ImageSubresourceLayers::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .layer_count(1);
    let corner = |extent: vk::Extent2D| vk::Offset3D {
        x: i32::try_from(extent.width).unwrap_or(i32::MAX),
        y: i32::try_from(extent.height).unwrap_or(i32::MAX),
        z: 1,
    };
    // SAFETY: the caller's.
    unsafe {
        device.cmd_pipeline_barrier2(
            cb,
            &vk::DependencyInfo::default().image_memory_barriers(&before),
        );
        if region.clear {
            // An image larger than the scaled slot shows black outside it.
            device.cmd_clear_color_image(
                cb,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &vk::ClearColorValue {
                    float32: [0.0, 0.0, 0.0, 1.0],
                },
                &[range],
            );
            let cleared = [vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::CLEAR)
                .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .dst_stage_mask(transfer)
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
            // Nothing of the slot fits.
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
                slot,
                vk::ImageLayout::GENERAL,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &copy,
            );
        } else {
            let blit = [vk::ImageBlit::default()
                .src_subresource(layers)
                .src_offsets([vk::Offset3D::default(), corner(region.src)])
                .dst_subresource(layers)
                .dst_offsets([vk::Offset3D::default(), corner(region.dst)])];
            device.cmd_blit_image(
                cb,
                slot,
                vk::ImageLayout::GENERAL,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &blit,
                vk::Filter::NEAREST,
            );
        }
        let after = [vk::ImageMemoryBarrier2::default()
            .src_stage_mask(transfer)
            .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::NONE)
            .dst_access_mask(vk::AccessFlags2::empty())
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

/// B8G8R8A8_UNORM in sRGB-nonlinear colour space, else any B8G8R8A8_UNORM.
fn pick_surface_format(formats: &[vk::SurfaceFormatKHR]) -> Option<vk::SurfaceFormatKHR> {
    let bgra = |f: &&vk::SurfaceFormatKHR| f.format == vk::Format::B8G8R8A8_UNORM;
    formats
        .iter()
        .filter(bgra)
        .find(|f| f.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR)
        .or_else(|| formats.iter().find(bgra))
        .copied()
}

impl Drop for SurfaceOutput {
    fn drop(&mut self) {
        let device = &self.vk.device;
        // Nothing of this surface may still run; a lost device has nothing
        // left to wait for.
        let _ = unsafe { device.device_wait_idle() };
        let swapchain = self.swapchain.take();
        self.destroy_swapchain(swapchain);
        unsafe {
            for sync in self.syncs.drain(..) {
                device.destroy_semaphore(sync.acquire, None);
                device.destroy_fence(sync.copied, None);
            }
            if self.command_pool != vk::CommandPool::null() {
                device.destroy_command_pool(self.command_pool, None);
            }
        }
        self.targets.clear();
        unsafe { self.surface_fns.destroy_surface(self.surface, None) };
    }
}

#[cfg(test)]
mod tests {
    use ash::vk;

    use super::{CopyRegion, copy_region, pick_surface_format};

    fn extent(width: u32, height: u32) -> vk::Extent2D {
        vk::Extent2D { width, height }
    }

    #[test]
    fn a_scaled_slot_covers_what_fits_whole_and_clears_the_rest() {
        let slot = extent(64, 48);
        let region = |image, scale| copy_region(slot, image, scale);
        assert_eq!(
            region(extent(128, 96), 2),
            CopyRegion {
                src: slot,
                dst: extent(128, 96),
                scale: 2,
                clear: false
            }
        );
        assert_eq!(
            region(extent(130, 90), 2),
            CopyRegion {
                src: extent(64, 45),
                dst: extent(128, 90),
                scale: 2,
                clear: true
            },
            "45 slot rows fit in 90; the 2 extra columns are cleared"
        );
        assert_eq!(
            region(extent(64, 48), 1),
            CopyRegion {
                src: slot,
                dst: slot,
                scale: 1,
                clear: false
            }
        );
        assert_eq!(
            region(extent(32, 100), 1),
            CopyRegion {
                src: extent(32, 48),
                dst: extent(32, 48),
                scale: 1,
                clear: true
            }
        );
        assert_eq!(
            region(extent(1, 1), 2),
            CopyRegion {
                src: extent(0, 0),
                dst: extent(0, 0),
                scale: 2,
                clear: true
            },
            "nothing fits"
        );
        assert_eq!(region(extent(64, 48), 0).scale, 1, "0 means 1");
    }

    fn format(format: vk::Format, color_space: vk::ColorSpaceKHR) -> vk::SurfaceFormatKHR {
        vk::SurfaceFormatKHR {
            format,
            color_space,
        }
    }

    #[test]
    fn surface_format_prefers_bgra_unorm_in_srgb_nonlinear() {
        let formats = [
            format(vk::Format::B8G8R8A8_SRGB, vk::ColorSpaceKHR::SRGB_NONLINEAR),
            format(
                vk::Format::B8G8R8A8_UNORM,
                vk::ColorSpaceKHR::DISPLAY_P3_NONLINEAR_EXT,
            ),
            format(
                vk::Format::B8G8R8A8_UNORM,
                vk::ColorSpaceKHR::SRGB_NONLINEAR,
            ),
        ];
        assert_eq!(pick_surface_format(&formats), Some(formats[2]));
        assert_eq!(pick_surface_format(&formats[..2]), Some(formats[1]));
        assert_eq!(pick_surface_format(&formats[..1]), None);
    }
}
