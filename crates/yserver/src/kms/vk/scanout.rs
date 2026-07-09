//! Per-bo state machine + scanout-bo allocation (sub-phase 4.1.2).
//!
//! Spec: docs/superpowers/specs/2026-05-07-phase4-1-vulkan-compositor-design.md
//! §"Per-buffer release fence" — table of transitions and fence-handle
//! ownership rules.
//!
//! ## Allocation direction
//!
//! **GBM-first, Vulkan-fallback.** The preferred path allocates the
//! scanout BO via `gbm_bo_create_with_modifiers(RENDERING|SCANOUT)`
//! and imports the resulting dma-buf into Vulkan as the compose
//! render target (`VK_EXT_image_drm_format_modifier` +
//! `VkImportMemoryFdInfoKHR`). This is the ecosystem-standard path
//! (Xorg modesetting DDX, mutter, GNOME) and the only one that
//! produces a display-correct tiled scanout buffer on NVIDIA
//! proprietary — Vulkan-allocated block-linear images lack the
//! display-engine layout the driver applies through GBM, so all
//! gob-height modifier variants garble on Pascal (HW-confirmed on
//! GTX 1050). See
//! docs/superpowers/specs/2026-07-20-nvidia-gbm-scanout-allocation.md.
//!
//! Fallback: allocate the `VkImage` first, export via
//! `vkGetMemoryFdKHR`, import into DRM via `PRIME_FD_TO_HANDLE`.
//! Kept for the Venus (virtio-gpu blob) path where the GBM-import
//! direction aborts the driver, and for drivers/planes with no
//! Vulkan-importable modifier on offer. Both paths hand the same
//! GEM handle to `AddFB2WithModifiers`.
//!
//! ### What NVIDIA actually ends up with
//!
//! **Block-linear tiled, on every NVIDIA card measured — one path, not
//! three.** NVIDIA's GBM rejects `gbm_bo_create_with_modifiers` for
//! `DRM_FORMAT_MOD_LINEAR` with `EINVAL` on a `RENDERING|SCANOUT` BO, so
//! the GBM-LINEAR plan always fails and the first tiled variant wins.
//! HW-confirmed across generation, driver and pitch alignment:
//!
//! - GTX 1050 (Pascal, 2560x1440, `0x3000000004fe015`) — 2026-07-30,
//!   with the GBM-LINEAR `EINVAL` logged explicitly.
//! - GTX 1060 (Pascal, 3440x1440 ultrawide, `0x3000000004fe015`,
//!   pitch 13760) — 2026-07-26, 91-second session, no device-lost.
//! - RTX 3060 Ti (Ampere, driver 595.71.05, 1920x1080,
//!   `0x300000000606015`) — 2026-07-29, issue #32 telemetry.
//!
//! All three display correctly, and the 1050 has been dogfooded on this
//! path since `5fdb56eb` (2026-07-22) — `8cf45085`'s "nvidia box smooth"
//! validation on 2026-07-26 was itself run on GBM tiled scanout.
//!
//! This matters for reading the LINEAR-preference policy below:
//! [`scanout_prefers_linear`] was written for the Vulkan-alloc era
//! (2026-06-21, before GBM-first) and since 2026-07-22 it only governs
//! the Vulkan-alloc fallback plans — on NVIDIA it reorders a candidate
//! list whose LINEAR entry is guaranteed to fail. It is still correct
//! and still needed *there*, because Vulkan-allocated block-linear
//! genuinely does garble; it is simply not what NVIDIA runs today.

use std::{
    io,
    os::fd::{AsFd, FromRawFd, IntoRawFd, OwnedFd},
    rc::Rc,
    sync::{Arc, OnceLock},
};

use ash::vk;
use drm::{
    buffer::{DrmFourcc, DrmModifier, Handle as DrmBufferHandle, PlanarBuffer as DrmPlanarBuffer},
    control::{Device as DrmControlDevice, FbCmd2Flags, framebuffer},
};

/// Type alias for the GBM device we hold per pool. Instantiated with
/// the KMS DRM device the pool was constructed against — allocations
/// go through this driver-side allocator so the resulting BO gets the
/// scanout-correct layout the display engine expects.
type GbmDevice = gbm::Device<Rc<crate::drm::Device>>;

use super::device::VkContext;

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

/// Fence-fd handles + the current phase. Owns no DRM/Vulkan state
/// directly — callers thread the actual `VkImage` / framebuffer
/// alongside.
#[derive(Debug, Default)]
pub struct BoState {
    pub phase: BoPhase,
    /// Fence we exported from `vkGetSemaphoreFdKHR` after submit and
    /// will pass to KMS as `IN_FENCE_FD`. We own it until the kernel
    /// consumes it on atomic accept.
    pub in_fence_fd: Option<i32>,
    /// Fence the kernel allocated and handed back via `OUT_FENCE_PTR`.
    /// Signalled when the next flip retires this bo.
    pub release_fence_fd: Option<i32>,
}

impl BoState {
    /// `Free → Recording`: acquire for next frame's render target.
    pub fn transition_to_recording(&mut self) {
        debug_assert_eq!(self.phase, BoPhase::Free);
        self.phase = BoPhase::Recording;
    }

    /// `Recording → Submitted`: `vkQueueSubmit2` issued. Caller
    /// already exported `IN_FENCE_FD` and passes it in.
    pub fn transition_to_submitted(&mut self, in_fence_fd: i32) {
        debug_assert_eq!(self.phase, BoPhase::Recording);
        self.phase = BoPhase::Submitted;
        self.in_fence_fd = (in_fence_fd >= 0).then_some(in_fence_fd);
    }

    /// `Submitted → Pending`: atomic accepted. Returns the in-fence
    /// fd so the caller can close it (the kernel takes a reference
    /// to the underlying `sync_file` during the commit but does NOT
    /// own the fd — userspace must close it). Adopts the out-fence
    /// fd from KMS as the release fence.
    #[must_use = "in-fence fd must be closed by the caller"]
    pub fn transition_to_pending(&mut self, out_fence_fd: i32) -> Option<i32> {
        debug_assert_eq!(self.phase, BoPhase::Submitted);
        self.phase = BoPhase::Pending;
        let in_fence = self.in_fence_fd.take();
        self.release_fence_fd = (out_fence_fd >= 0).then_some(out_fence_fd);
        in_fence
    }

    /// `Submitted → Recording`: atomic returned `-EBUSY`. Caller is
    /// responsible for closing the returned in-fence fd.
    pub fn transition_to_recording_after_atomic_reject(&mut self) -> Option<i32> {
        debug_assert_eq!(self.phase, BoPhase::Submitted);
        self.phase = BoPhase::Recording;
        self.in_fence_fd.take()
    }

    /// `Submitted → Free`: modeset preempts (CRTC reconfigure
    /// mid-flight). Caller has already host-waited on the in-flight
    /// GPU work and must close the returned fd.
    pub fn transition_to_free_after_modeset_preempt(&mut self) -> Option<i32> {
        debug_assert_eq!(self.phase, BoPhase::Submitted);
        self.phase = BoPhase::Free;
        self.in_fence_fd.take()
    }

    /// `Pending → OnScreen`: first pageflip-complete event for this
    /// bo arrived.
    pub fn transition_to_on_screen(&mut self) {
        debug_assert_eq!(self.phase, BoPhase::Pending);
        self.phase = BoPhase::OnScreen;
    }

    /// `OnScreen → Retiring`: next flip's pageflip-complete arrived.
    /// Release fence is now signal-pending (will be signalled by
    /// KMS).
    pub fn transition_to_retiring(&mut self) {
        debug_assert_eq!(self.phase, BoPhase::OnScreen);
        self.phase = BoPhase::Retiring;
    }

    /// `Retiring → Free`: all GPU readers are done; caller will close
    /// the returned release fence fd.
    pub fn transition_to_free_after_retire(&mut self) -> Option<i32> {
        debug_assert_eq!(self.phase, BoPhase::Retiring);
        self.phase = BoPhase::Free;
        self.release_fence_fd.take()
    }

    /// `any → Free` on modeset reset (hotunplug, mode change). Caller
    /// must close every returned fd. The two slots may both be
    /// populated if the bo was Submitted-then-immediately-Pending
    /// somehow; in normal flow only one is.
    pub fn transition_to_free_after_modeset_reset(&mut self) -> ModesetReleased {
        let in_fence = self.in_fence_fd.take();
        let release_fence = self.release_fence_fd.take();
        self.phase = BoPhase::Free;
        ModesetReleased {
            in_fence,
            release_fence,
        }
    }
}

/// Fences released when a bo is force-reset on modeset. Caller closes
/// each `Some(fd)` exactly once.
#[derive(Debug)]
pub struct ModesetReleased {
    pub in_fence: Option<i32>,
    pub release_fence: Option<i32>,
}

/// One scanout buffer object: a Vulkan-allocated `VkImage` exported
/// as a dma-buf and imported into the DRM device for KMS scanout.
///
/// All fields are populated after `allocate()` returns successfully.
/// Drop unwinds them in the right order (DRM framebuffer → GEM handle
/// close → VkImage → memory → semaphore → command pool).
#[allow(dead_code)] // most fields used by 4.1.2.5+ atomic-commit driver.
pub struct ScanoutBo {
    pub state: BoState,
    pub width: u32,
    pub height: u32,
    /// `true` for client-imported alien BOs (Phase 4.2.4 Flip /
    /// DirectScanout); `false` for pool-allocated server BOs. Alien
    /// BOs share the framebuffer-registration code path but skip the
    /// allocator: they're wired in by `ScanoutBoPool::register_alien`.
    pub is_alien: bool,
    /// Row pitch in bytes — what the driver chose for our
    /// `TILING_LINEAR` image. Passed to KMS as `pitch[0]` and to the
    /// blit copy as the destination row stride.
    pub pitch: u32,
    /// Compose GPU-render time (ns) measured on THIS bo's PREVIOUS
    /// compose via its timestamp pool, read at the start of the next
    /// compose (prior fence signaled → no wait) and surfaced to
    /// `tick_one_output` → `telemetry.record_gpu_render_ns`. `.take()`n
    /// each frame. `None` until the bo has composed at least twice / on
    /// devices without timestamp support.
    pub last_gpu_render_ns: Option<u64>,
    pub vk_image: vk::Image,
    pub vk_memory: vk::DeviceMemory,
    /// Color image view bound by the composite pass's
    /// `vkCmdBeginRendering` as the color attachment. Lives as long
    /// as `vk_image`. Built lazily on first use to avoid forcing
    /// every PixmanShadow-only deployment to allocate a view it
    /// never reads.
    pub vk_image_view: vk::ImageView,
    /// Long-lived binary semaphore used as `signalSemaphore` on the
    /// per-frame composite submit. Its payload is exported as a
    /// SYNC_FD after every submit and handed to KMS as `IN_FENCE_FD`.
    /// Object reused for the bo's whole lifetime; only the fd
    /// payload churns.
    pub vk_semaphore: vk::Semaphore,
    /// DRM framebuffer registered against this bo's GEM handle.
    /// `Option` so Drop can take it.
    pub fb_handle: Option<framebuffer::Handle>,
    /// GEM handle from `PRIME_FD_TO_HANDLE`. Closed via `GEM_CLOSE`
    /// in Drop. `Option` so Drop can take it.
    pub gem_handle: Option<DrmBufferHandle>,
    /// Per-bo transfer resources: command pool + a single command
    /// buffer recycled across frames, a host-mapped staging buffer
    /// sized for the bo (XRGB8888 → 4 bytes × width × height), and
    /// the device memory backing it.
    pub vk_transfer: TransferResources,
    /// Shared DRM device handle (for un-registering the framebuffer
    /// + closing the GEM handle in Drop).
    drm: Rc<crate::drm::Device>,
    /// Held to keep image+memory destructors anchored to a live
    /// device. Cloned per bo from the pool's Arc so individual bos
    /// can be moved/dropped independently.
    vk: Arc<VkContext>,
    /// When `true`, `Drop` early-returns: no explicit
    /// `destroy_framebuffer`, no GEM close, no Vk teardown.
    /// Resources are then leaked until process-exit DRM-fd close +
    /// VkDevice teardown — the kernel reaps GEM/FB on device-fd close
    /// and the userspace heap goes away with the process. This is a
    /// deliberate last-resort leak path, not a normal cleanup route.
    /// Set by `disarm()` from the shutdown path when atomic
    /// `disable_output` failed for this BO's CRTC — KMS may still
    /// hold the FB, so user-side teardown would corrupt kernel state.
    ///
    /// **ONLY safe to use at final process exit.** This Drop
    /// short-circuit bypasses Vk image / memory / GEM / FB cleanup
    /// but does NOT prevent Rust from dropping other fields (like
    /// the `Arc<VkContext>`). Using disarm at runtime (hotplug,
    /// modeset recovery) could produce a zombie VkImage when the
    /// VkContext's refcount expires.
    disarmed: bool,
    /// GBM buffer object backing this bo, when the GBM-first
    /// allocation path was used. `None` for Vulkan-first (fallback)
    /// allocations. Kept alive here so the `gbm_bo` outlives the
    /// dependent GEM handle, DRM framebuffer, and imported Vulkan
    /// image / memory — declared last so Rust drops it after the
    /// explicit `Drop` impl has torn those down.
    gbm_bo: Option<gbm::BufferObject<()>>,
}

/// Per-bo transfer-side resources (command pool/buffer + staging
/// buffer).
#[allow(dead_code)] // exercised by 4.1.2.5 atomic-commit driver.
pub struct TransferResources {
    pub command_pool: vk::CommandPool,
    pub command_buffer: vk::CommandBuffer,
    pub staging_buffer: vk::Buffer,
    pub staging_memory: vk::DeviceMemory,
    pub staging_mapped: std::ptr::NonNull<u8>,
    pub staging_size: u64,
    /// 2-query TIMESTAMP pool bracketing the compose GPU work (TOP at
    /// CB start, BOTTOM before end). Read on the NEXT compose of this
    /// BO (its prior fence has signaled — no wait) to derive
    /// `gpu_render_ns`. `null` if the device has no timestamp support.
    pub timestamp_pool: vk::QueryPool,
}

// Intentionally `!Send + !Sync`: the mapped staging pointer and every
// scanout resource stay on the single core/backend thread.

/// One pool per CRTC; holds N bos that rotate through the state
/// machine. Three is the documented sweet spot (design §2): one
/// scanning out, one queued, one being recorded into.
#[allow(dead_code)] // wired in via KmsBackend in a later commit.
pub struct ScanoutBoPool {
    pub bos: Vec<ScanoutBo>,
    pub width: u32,
    pub height: u32,
    /// GBM device on the pool's KMS DRM fd. Populated when
    /// `gbm_create_device` succeeds; individual BOs use this to
    /// allocate driver-side scanout-layout buffers (ecosystem-
    /// standard, and the only way tiled scanout is correct on
    /// NVIDIA — see 2026-07-20-nvidia-gbm-scanout-allocation.md).
    /// `None` degrades to the Vulkan-first legacy allocator.
    /// Declared last so it drops AFTER every BO in `bos` — GBM BOs
    /// hold their own device refcount, but the pool-level owner
    /// dying first would still be a lifetime foot-gun in future
    /// refactors.
    #[allow(dead_code)]
    gbm_device: Option<Rc<GbmDevice>>,
}

impl ScanoutBo {
    /// Allocate one scanout bo: GBM-alloc or Vulkan-alloc dma-buf +
    /// DRM framebuffer registration. All steps must succeed; partial
    /// allocations are unwound on error so the returned `Err` leaves
    /// no resources leaked.
    pub fn allocate(
        vk: Arc<VkContext>,
        drm: Rc<crate::drm::Device>,
        gbm: Option<Rc<GbmDevice>>,
        width: u32,
        height: u32,
        scanout_modifiers: &[u64],
    ) -> io::Result<Self> {
        let modifier_candidates = scanout_modifier_candidates(&vk, scanout_modifiers);
        let plans = scanout_allocation_plans(&vk, &modifier_candidates, width, gbm.is_some());
        let mut errors = Vec::new();

        for plan in plans {
            match Self::allocate_with_plan(
                Arc::clone(&vk),
                Rc::clone(&drm),
                gbm.as_ref().map(Rc::clone),
                width,
                height,
                plan,
            ) {
                Ok(bo) => {
                    log::info!(
                        "scanout bo: {} succeeded ({}x{}, pitch {})",
                        plan.describe(),
                        width,
                        height,
                        bo.pitch,
                    );
                    return Ok(bo);
                }
                Err(e) => {
                    // Log every rejected plan, not just the winner. Which
                    // plans a card silently falls THROUGH is the thing that
                    // distinguishes one NVIDIA generation from another (an
                    // Ampere box on driver 595 fails GBM-LINEAR and scans out
                    // block-linear tiled; a Pascal GTX 1050 takes LINEAR), and
                    // it was invisible in every user log until now because
                    // `errors` only ever surfaced when EVERY plan failed.
                    // INFO, not WARN: falling through is normal operation —
                    // the aggregate failure below is the actual error.
                    log::info!("scanout bo: {} failed: {e}", plan.describe());
                    errors.push(format!("{}: {e}", plan.describe()));
                }
            }
        }

        Err(io::Error::other(format!(
            "scanout allocation failed for every path: {}",
            errors.join("; ")
        )))
    }

    fn allocate_with_plan(
        vk: Arc<VkContext>,
        drm: Rc<crate::drm::Device>,
        gbm: Option<Rc<GbmDevice>>,
        width: u32,
        height: u32,
        plan: ScanoutAllocationPlan,
    ) -> io::Result<Self> {
        // 1. Allocate the source dma-buf + import into Vulkan
        //    (GBM plans) OR allocate the `VkImage` and export as
        //    dma-buf (Vulkan-alloc plans).
        let img = match plan {
            ScanoutAllocationPlan::GbmModifier(modifier) => {
                let gbm_device = gbm.as_ref().ok_or_else(|| {
                    io::Error::other("gbm plan requested but pool has no gbm_device")
                })?;
                allocate_gbm_scanout_image(&vk, gbm_device, width, height, modifier)
                    .map_err(|e| io::Error::other(format!("gbm scanout image: {e}")))?
            }
            _ => allocate_vk_scanout_image(&vk, width, height, plan)
                .map_err(|e| io::Error::other(format!("vk scanout image: {e}")))?,
        };
        let VkScanoutImage {
            image,
            memory,
            dmabuf,
            pitch,
            offset,
            modifier,
            gbm_bo,
        } = img;

        // 2. PRIME_FD_TO_HANDLE on the DRM device. Same DRM fd the
        //    GBM device (if any) was created on, so this returns the
        //    existing GEM handle rather than creating a new one when
        //    the source is a gbm_bo — kernel refcounts the underlying
        //    dma-buf either way.
        let gem_handle = match drm.prime_fd_to_buffer(dmabuf.as_fd()) {
            Ok(h) => h,
            Err(e) => {
                destroy_scanout_image(&vk, image, memory);
                return Err(io::Error::other(format!("drm prime_fd_to_buffer: {e}")));
            }
        };
        // The GEM handle holds its own reference; close the dma-buf
        // fd we no longer need.
        drop(dmabuf);

        // 3. add_fb2. Modifier-backed paths must pass the MODIFIERS
        // flag even for DRM_FORMAT_MOD_LINEAR; the legacy fallback
        // deliberately keeps the old untagged shape.
        let fb_handle = match drm.add_planar_framebuffer(
            &VkScanoutFb {
                gem_handle,
                width,
                height,
                pitch,
                offset,
                modifier,
            },
            addfb_flags_for_modifier(modifier),
        ) {
            Ok(h) => h,
            Err(e) => {
                let _ = drm.close_buffer(gem_handle);
                destroy_scanout_image(&vk, image, memory);
                return Err(io::Error::other(format!("drm add_fb: {e}")));
            }
        };

        // 4. Long-lived export semaphore.
        let vk_semaphore = match create_export_semaphore(&vk) {
            Ok(s) => s,
            Err(e) => {
                let _ = drm.destroy_framebuffer(fb_handle);
                let _ = drm.close_buffer(gem_handle);
                unsafe {
                    vk.device.destroy_image(image, None);
                    vk.device.free_memory(memory, None);
                }
                return Err(io::Error::other(format!("vk semaphore: {e}")));
            }
        };

        // 5. Per-bo transfer resources (always present now —
        //    every bo has a live VkImage to upload into).
        let vk_transfer = match allocate_transfer_resources(&vk, width, height) {
            Ok(t) => t,
            Err(e) => {
                unsafe {
                    vk.device.destroy_semaphore(vk_semaphore, None);
                    vk.device.destroy_image(image, None);
                    vk.device.free_memory(memory, None);
                }
                let _ = drm.destroy_framebuffer(fb_handle);
                let _ = drm.close_buffer(gem_handle);
                return Err(io::Error::other(format!("vk transfer: {e}")));
            }
        };

        // 6. Color image view used by the 4.1.3.4 composite pass
        //    `vkCmdBeginRendering` as the color attachment.
        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(vk::Format::B8G8R8A8_UNORM)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        let vk_image_view = match unsafe { vk.device.create_image_view(&view_info, None) } {
            Ok(v) => v,
            Err(e) => {
                unsafe {
                    vk.device.unmap_memory(vk_transfer.staging_memory);
                    vk.device.destroy_buffer(vk_transfer.staging_buffer, None);
                    vk.device.free_memory(vk_transfer.staging_memory, None);
                    vk.device
                        .destroy_command_pool(vk_transfer.command_pool, None);
                    vk.device.destroy_semaphore(vk_semaphore, None);
                    vk.device.destroy_image(image, None);
                    vk.device.free_memory(memory, None);
                }
                let _ = drm.destroy_framebuffer(fb_handle);
                let _ = drm.close_buffer(gem_handle);
                return Err(io::Error::other(format!("vk image view: {e}")));
            }
        };

        Ok(Self {
            state: BoState::default(),
            width,
            height,
            is_alien: false,
            pitch,
            last_gpu_render_ns: None,
            vk_image: image,
            vk_memory: memory,
            vk_image_view,
            vk_semaphore,
            fb_handle: Some(fb_handle),
            gem_handle: Some(gem_handle),
            vk_transfer,
            drm,
            vk,
            disarmed: false,
            gbm_bo,
        })
    }

    /// Export a SYNC_FD payload from this bo's signal semaphore. Call
    /// this after `vkQueueSubmit2` with `signalSemaphore = vk_semaphore`
    /// — it returns the freshly-payloaded fd to hand KMS as
    /// `IN_FENCE_FD`. `None` maps to the KMS `-1` no-fence sentinel.
    #[allow(dead_code)] // wired in by Task 2.5 (atomic-commit fence path).
    pub fn export_signaled_fd(&self) -> Result<Option<OwnedFd>, vk::Result> {
        let ext = self.vk.external_semaphore_fd.clone();
        let info = vk::SemaphoreGetFdInfoKHR::default()
            .semaphore(self.vk_semaphore)
            .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
        let raw_fd = unsafe { ext.get_semaphore_fd(&info)? };
        super::optional_sync_fd_from_vk(raw_fd, "vkGetSemaphoreFdKHR(SYNC_FD)")
    }

    /// Mark this BO as "let process-exit clean up." Subsequent
    /// `Drop` is a no-op. Idempotent.
    /// **Only valid at final process exit** — see field doc.
    pub fn disarm(&mut self) {
        self.disarmed = true;
    }
}

impl Drop for ScanoutBo {
    fn drop(&mut self) {
        if self.disarmed {
            // Disarmed by shutdown-failed-disable path; let DRM-fd
            // close (process exit) reap GEM/FB and VkDevice teardown
            // releases the userspace handles. We are DELIBERATELY
            // leaking: this Drop deliberately skips vkDestroyImage,
            // vkFreeMemory, destroy_framebuffer, close_buffer(gem),
            // etc. — because touching them while KMS may still hold
            // the FB produces the `atomic remove_fb failed with -22`
            // warning that strands Wayland host sessions.
            log::warn!(
                "ScanoutBo disarmed (atomic disable_output failed); \
                 leaking FB/GEM/Vk to be reaped by DRM-fd close"
            );
            return;
        }
        // Defensive fence-fd cleanup. If the bo was Submitted /
        // Pending / OnScreen / Retiring at drop time (mid-flight
        // shutdown, or modeset that didn't go through the explicit
        // drain path), close any held fence fds so they don't leak.
        // Kernel-side sync_file refs survive our fd close until the
        // DRM device closes — atomic flip will still complete or
        // fail safely on its own.
        let released = self.state.transition_to_free_after_modeset_reset();
        if let Some(fd) = released.in_fence {
            // SAFETY: fd was inserted by transition_to_submitted; we
            // are the unique owner.
            drop(unsafe { OwnedFd::from_raw_fd(fd) });
        }
        if let Some(fd) = released.release_fence {
            drop(unsafe { OwnedFd::from_raw_fd(fd) });
        }

        // DRM-side teardown next: framebuffer references the GEM
        // handle; both must be released before we free the underlying
        // memory the dma-buf was exported from.
        if let Some(fb) = self.fb_handle.take()
            && let Err(e) = self.drm.destroy_framebuffer(fb)
        {
            log::warn!("drm destroy_framebuffer failed: {e}");
        }
        if let Some(h) = self.gem_handle.take()
            && let Err(e) = self.drm.close_buffer(h)
        {
            log::warn!("drm close_buffer (gem) failed: {e}");
        }

        unsafe {
            // Transfer resources (staging mapping must release before
            // memory is freed; command pool releases its CB).
            let t = std::mem::replace(
                &mut self.vk_transfer,
                TransferResources {
                    command_pool: vk::CommandPool::null(),
                    command_buffer: vk::CommandBuffer::null(),
                    staging_buffer: vk::Buffer::null(),
                    staging_memory: vk::DeviceMemory::null(),
                    staging_mapped: std::ptr::NonNull::dangling(),
                    staging_size: 0,
                    timestamp_pool: vk::QueryPool::null(),
                },
            );
            if t.command_pool != vk::CommandPool::null() {
                self.vk.device.unmap_memory(t.staging_memory);
                self.vk.device.destroy_buffer(t.staging_buffer, None);
                self.vk.device.free_memory(t.staging_memory, None);
                self.vk.device.destroy_command_pool(t.command_pool, None);
                if t.timestamp_pool != vk::QueryPool::null() {
                    self.vk.device.destroy_query_pool(t.timestamp_pool, None);
                }
            }

            // Image view before image, image before memory, then
            // semaphore.
            if self.vk_image_view != vk::ImageView::null() {
                self.vk.device.destroy_image_view(self.vk_image_view, None);
            }
            self.vk.device.destroy_image(self.vk_image, None);
            self.vk.device.free_memory(self.vk_memory, None);
            if self.vk_semaphore != vk::Semaphore::null() {
                self.vk.device.destroy_semaphore(self.vk_semaphore, None);
            }
        }
    }
}

/// Handle returned by [`ScanoutBoPool::register_alien`] — index into
/// `pool.bos` plus a generation token so a stale handle can't access
/// a re-used slot. Phase 4.2.4 design §3.3.2.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AlienBoHandle {
    pub index: u32,
}

impl ScanoutBoPool {
    /// Register a client-imported `DrawableImage` as an alien BO in
    /// the pool. The DrawableImage's underlying `VkDeviceMemory` is
    /// already allocated; we run the same `add_fb2` framebuffer
    /// registration the pool's owned BOs use, with the imported
    /// memory's GEM handle plus its DRM modifier.
    ///
    /// Phase 4.2.4 first-cut: returns `Err` because the
    /// VkDeviceMemory → GEM handle bridge is non-trivial and lives
    /// behind the live KMS Flip integration. The wire surface is in
    /// place so the dispatcher's choose_path Flip / DirectScanout
    /// branches plumb correctly; live registration arrives with the
    /// vng + Venus smoke for §5.5 hardware coverage.
    pub fn register_alien(
        &mut self,
        _drawable: &super::target::DrawableImage,
    ) -> io::Result<AlienBoHandle> {
        Err(io::Error::other(
            "ScanoutBoPool::register_alien: live KMS Flip integration not yet wired \
             (Phase 4.2.4 design §5.5 hardware coverage smoke)",
        ))
    }

    /// Drop a previously registered alien BO. Releases the framebuffer
    /// registration and removes the entry from `bos`. No-op if the
    /// handle's index is out of range.
    #[allow(dead_code)]
    pub fn unregister_alien(&mut self, _handle: AlienBoHandle) -> io::Result<()> {
        // Counterpart to register_alien — unimplemented for the same
        // reason. The plan's Task 29 test covers the round-trip once
        // both halves land.
        Ok(())
    }

    /// Reset every bo in the pool to `Free`, draining any in-flight
    /// fence fds. Used by the modeset / hot-config path (resize, mode
    /// change, hotplug — design §2 "Modeset / hot-config events").
    ///
    /// Order of operations:
    ///
    /// 1. `vkDeviceWaitIdle` on the device — heavy hammer that waits
    ///    for any in-flight `vkQueueSubmit2` work to complete. Cheap
    ///    in steady state (no work) and conservatively correct for
    ///    `Submitted`-phase bos which would otherwise have GPU work
    ///    racing the DRM tear-down.
    /// 2. For each bo, advance state machine to `Free` via
    ///    `transition_to_free_after_modeset_reset` and close any
    ///    returned fence fds.
    ///
    /// Pool dimensions stay the same; this is "reset state machine,
    /// keep the bos." Re-allocating bos with new dimensions is the
    /// caller's responsibility (drop the pool, allocate a fresh one
    /// with `ScanoutBoPool::allocate`).
    // Consumers: Drop, and `PlatformBackend::reset_scanout_bos_for_suspend`
    // (VT-switch suspend reclaims orphaned scanout BOs after master loss).
    pub fn drain_all_pending(&mut self, vk: &VkContext) {
        if let Err(e) = unsafe { vk.device.device_wait_idle() } {
            log::warn!("scanout pool drain: vkDeviceWaitIdle: {e}");
        }
        for bo in &mut self.bos {
            let released = bo.state.transition_to_free_after_modeset_reset();
            if let Some(fd) = released.in_fence {
                // SAFETY: fd inserted by transition_to_submitted; unique owner.
                drop(unsafe { OwnedFd::from_raw_fd(fd) });
            }
            if let Some(fd) = released.release_fence {
                drop(unsafe { OwnedFd::from_raw_fd(fd) });
            }
        }
    }

    /// True if any bo in this pool is in `BoPhase::Pending` —
    /// i.e. an atomic flip was accepted by KMS and the kernel
    /// hasn't yet emitted its pageflip-complete event for that
    /// flip. Used by the shutdown sequence to wait until KMS
    /// quiesces before issuing `disable_output`. Calling
    /// `disable_output` while a Pending bo exists is what
    /// produces the `atomic remove_fb failed with -22` kernel
    /// warning that leaves Wayland host compositors stranded.
    pub fn has_pending_pageflip(&self) -> bool {
        self.bos.iter().any(|b| b.state.phase == BoPhase::Pending)
    }

    /// Allocate `count` bos for one output. Phase 4.1.2 uses 3 bos
    /// per pool (design §2). Opens the per-pool `gbm_device` on the
    /// KMS DRM fd so BOs can go through the GBM-first path. GBM
    /// device open failure is non-fatal — bos fall back to the
    /// Vulkan-first legacy allocator. On BO allocation failure the
    /// partial pool is dropped (each successful bo cleans up via
    /// `ScanoutBo::Drop`).
    pub fn allocate(
        vk: Arc<VkContext>,
        drm: Rc<crate::drm::Device>,
        width: u32,
        height: u32,
        count: usize,
        scanout_modifiers: &[u64],
    ) -> io::Result<Self> {
        let gbm_device = match GbmDevice::new(Rc::clone(&drm)) {
            Ok(g) => Some(Rc::new(g)),
            Err(e) => {
                log::warn!(
                    "gbm_create_device failed on KMS fd ({e}); scanout allocation will \
                     fall back to Vulkan-alloc, where NVIDIA/Intel take LINEAR \
                     (see scanout_prefers_linear) because Vulkan-allocated tiled \
                     scanout garbles there"
                );
                None
            }
        };
        let mut bos = Vec::with_capacity(count);
        for _ in 0..count {
            bos.push(ScanoutBo::allocate(
                Arc::clone(&vk),
                Rc::clone(&drm),
                gbm_device.as_ref().map(Rc::clone),
                width,
                height,
                scanout_modifiers,
            )?);
        }
        Ok(Self {
            bos,
            width,
            height,
            gbm_device,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScanoutAllocationPlan {
    /// Preferred path: allocate via GBM with the given DRM modifier,
    /// then import the dma-buf into Vulkan as the compose render
    /// target. Xorg modesetting DDX, mutter, GNOME all do this — and
    /// on NVIDIA it's the ONLY path that produces a display-correct
    /// tiled scanout buffer (Vulkan-alloc block-linear garbles).
    GbmModifier(u64),
    /// Fallback: allocate the `VkImage` first with the given DRM
    /// modifier, export via `vkGetMemoryFdKHR`, import into DRM.
    /// Kept for Venus (virtio-gpu blob) and for drivers/planes with
    /// no Vulkan-importable modifier on offer.
    DrmModifier(u64),
    /// LINEAR VkImage created via an EXPLICIT DRM-modifier layout
    /// (`VK_EXT_image_drm_format_modifier`) with a forced, 256-aligned
    /// `row_pitch` — for NVIDIA/Intel widths (e.g. 3440 ultrawide) whose tight
    /// LINEAR pitch the display engine rejects at atomic commit. Keeps the
    /// known-good LINEAR render path; only the stride is padded.
    PaddedExplicitLinear { row_pitch: u32 },
    /// Linear VkImage, but register the DRM framebuffer with an
    /// explicit DRM_FORMAT_MOD_LINEAR modifier.
    ExplicitLinear,
    /// Historical fallback: linear VkImage, untagged addfb2.
    LegacyLinear,
}

impl ScanoutAllocationPlan {
    fn describe(self) -> String {
        match self {
            Self::GbmModifier(modifier) => format!("gbm-modifier=0x{modifier:x}"),
            Self::DrmModifier(modifier) => format!("modifier=0x{modifier:x}"),
            Self::PaddedExplicitLinear { row_pitch } => {
                format!("padded-explicit-linear(pitch={row_pitch})")
            }
            Self::ExplicitLinear => "explicit-linear".to_string(),
            Self::LegacyLinear => "legacy-linear".to_string(),
        }
    }
}

fn scanout_allocation_plans(
    vk: &VkContext,
    modifier_candidates: &[u64],
    width: u32,
    gbm_available: bool,
) -> Vec<ScanoutAllocationPlan> {
    let mut plans = Vec::new();
    // GBM-allocated modifiers go FIRST — that's the ecosystem-standard path and
    // the only one that produces correct tiled scanout on NVIDIA. Per-modifier
    // Vulkan-import gating is checked at allocation time (IMPORTABLE, not the
    // Vulkan-alloc EXPORTABLE gate) so unsupported entries fall through cleanly
    // to the next plan rather than being pruned here. LINEAR (padded/legacy)
    // remains as an automatic fallback below when GBM can't produce a scanout
    // BO (e.g. modifier-less Polaris → legacy-linear).
    if gbm_available && vk.image_drm_format_modifier {
        plans.extend(
            modifier_candidates
                .iter()
                .copied()
                .map(ScanoutAllocationPlan::GbmModifier),
        );
    }
    // On drivers that prefer LINEAR (NVIDIA/Intel — the tiled/block-linear
    // scanout path renders garbled there via Vulkan-alloc), a tight LINEAR
    // pitch that isn't 256-aligned is rejected by the display engine at atomic
    // commit (EINVAL → device lost). Keep LINEAR but force an aligned (padded)
    // pitch via an explicit DRM-modifier layout. Only meaningful when the
    // modifier extension is present (explicit-layout create needs it). See
    // [`SCANOUT_PITCH_ALIGN`].
    //
    // Deliberately the RAW driver policy, not [`resolve_prefer_linear`]: this
    // is a fallback plan, not a preference. Someone running
    // `YSERVER_SCANOUT_MODIFIER=tiled-first` on an unaligned-pitch NVIDIA
    // width (3440 ultrawide) must still keep the known-good padded-LINEAR
    // plan behind the tiled attempts, or a garbling tiled modifier leaves no
    // survivable path to a display.
    if vk.image_drm_format_modifier
        && scanout_prefers_linear(vk.driver_id)
        && !linear_scanout_stride_aligned(width)
    {
        plans.push(ScanoutAllocationPlan::PaddedExplicitLinear {
            row_pitch: padded_linear_pitch(width),
        });
    }
    if vk.image_drm_format_modifier {
        plans.extend(
            modifier_candidates
                .iter()
                .copied()
                .map(ScanoutAllocationPlan::DrmModifier),
        );
    }
    plans.push(ScanoutAllocationPlan::ExplicitLinear);
    plans.push(ScanoutAllocationPlan::LegacyLinear);
    plans
}

/// Whether scanout BO allocation should try `LINEAR` before the tiled
/// DRM modifiers (see [`order_scanout_modifier_candidates`]).
///
/// **Scope: the Vulkan-alloc plans only, in practice.** This policy predates
/// the GBM-first path (`5fdb56eb`, 2026-07-22). On NVIDIA the GBM-LINEAR plan
/// now fails with `EINVAL` before this ordering can matter, so NVIDIA runs
/// GBM block-linear tiled and reaches the plans this policy governs only when
/// `gbm_create_device` itself failed. Do NOT read a `prefer_linear=true` log
/// line as "this card is scanning out LINEAR" — check which plan *succeeded*.
/// See the module header for the measurements.
///
/// Driver-split policy, each entry HW-confirmed against a real dithered/
/// corrupted scanout **on the Vulkan-alloc path**:
/// - **NVIDIA proprietary** (GTX 1050/Pascal): a Vulkan-allocated
///   BLOCK_LINEAR_2D image produces a dithered display, for every gob-height
///   variant. The same modifier allocated through GBM is clean — the driver
///   applies a display-engine layout Vulkan-alloc doesn't reproduce.
/// - **Intel Mesa (ANV)** (Kaby Lake i5-7200U): the I915 Y_TILED modifier
///   (`0x0100000000000002`) selected first produces the same dithering.
///
/// AMD (RADV) is deliberately NOT in this set: RDNA4/gfx12 *requires* the
/// tiled modifier — a LINEAR scanout buffer corrupts there (issue #48) —
/// and RDNA2 scans out tiled fine. Other drivers default to tiled-first;
/// add them here only after a confirmed dithering report, not speculatively
/// (e.g. Asahi/M1 has not shown the problem and stays tiled-first).
fn scanout_prefers_linear(driver_id: vk::DriverId) -> bool {
    matches!(
        driver_id,
        vk::DriverId::NVIDIA_PROPRIETARY | vk::DriverId::INTEL_OPEN_SOURCE_MESA
    )
}

/// Byte alignment the KMS scanout pitch must satisfy on the display engines
/// that otherwise prefer LINEAR (NVIDIA/Intel). NVIDIA's display controller
/// requires a 256-byte-aligned scanout stride; a Vulkan `LINEAR` image has a
/// TIGHT pitch (`width * 4` bytes for B8G8R8A8), so at widths whose byte-pitch
/// isn't 256-aligned the LINEAR framebuffer is rejected at atomic commit
/// (`EINVAL` → BO invalidated → `ERROR_DEVICE_LOST` → respawn loop).
///
/// HW-confirmed **2026-07-20, before the GBM-first path** (`5fdb56eb`,
/// 2026-07-22): GTX 1050 @ 2560 wide → pitch 10240 = 256×40 (OK, scanned out
/// LINEAR); GTX 1060 @ 3440 ultrawide → tight pitch 13760 (mod 256 = 192,
/// rejected at atomic commit → device lost). Same driver — only the stride
/// alignment differed; both 2560 and 1920 (aligned) rendered clean via LINEAR
/// on the 1060. So when the tight LINEAR pitch is unaligned we keep LINEAR but
/// allocate it with an explicit padded (aligned) pitch — see
/// [`padded_linear_pitch`] / `ScanoutAllocationPlan::PaddedExplicitLinear`.
///
/// Two corrections since those measurements, both from the module header's
/// 2026-07-30 data:
///
/// 1. The old claim here that "the tiled (block-linear) modifier is NOT a
///    usable escape — yserver's tiled scanout renders garbled on NVIDIA" was
///    true only of the *Vulkan-alloc* tiled path. GBM-allocated block-linear
///    displays correctly on NVIDIA, including on that same GTX 1050.
/// 2. Consequently the 1050 no longer "scans out LINEAR" at all: GBM-LINEAR
///    fails with `EINVAL` and it runs GBM tiled.
///
/// 3. This plan is consequently UNREACHABLE whenever GBM works. The 1060 was
///    re-measured 2026-07-26 (yserver 1.3.0 `46439bc67d89`, XFCE, 3440x1440 on
///    HDMI-1) and took `gbm-modifier=0x3000000004fe015` at **pitch 13760** —
///    the very unaligned pitch this constant exists to avoid — for a healthy
///    91-second session with zero device-lost, EINVAL or respawn signatures.
///    The unaligned-pitch rejection is specific to a *LINEAR* framebuffer; a
///    block-linear one at the same width is fine.
///
/// So the padded-pitch plan now guards only the no-GBM fallback. It stays:
/// unreachable costs nothing, and `gbm_create_device` failing on an ultrawide
/// NVIDIA box without it costs a respawn loop.
const SCANOUT_PITCH_ALIGN: u32 = 256;
/// Scanout format is `B8G8R8A8_UNORM` → 4 bytes/pixel.
const SCANOUT_BYTES_PER_PIXEL: u32 = 4;

/// True if a tight `LINEAR` scanout buffer `width` px wide has a display-engine-
/// acceptable (256-byte-aligned) pitch. See [`SCANOUT_PITCH_ALIGN`].
fn linear_scanout_stride_aligned(width: u32) -> bool {
    width
        .checked_mul(SCANOUT_BYTES_PER_PIXEL)
        .is_some_and(|pitch| pitch.is_multiple_of(SCANOUT_PITCH_ALIGN))
}

/// Pad a tight LINEAR scanout pitch up to [`SCANOUT_PITCH_ALIGN`]. Used to give
/// the display engine an aligned stride at widths (e.g. 3440 ultrawide) whose
/// tight `width*4` pitch it would otherwise reject at atomic commit.
fn padded_linear_pitch(width: u32) -> u32 {
    let tight = width.saturating_mul(SCANOUT_BYTES_PER_PIXEL);
    tight
        .div_ceil(SCANOUT_PITCH_ALIGN)
        .saturating_mul(SCANOUT_PITCH_ALIGN)
}

/// Diagnostic override of the scanout modifier policy, read once from
/// `YSERVER_SCANOUT_MODIFIER`.
///
/// [`scanout_prefers_linear`] is a per-driver policy inferred from a handful
/// of machines, and the two questions it raises can only be answered by
/// LOOKING at a display: does the GBM tiled path garble on THIS card, and
/// which of the six block-linear gob-height variants (0x…10 … 0x…15) is
/// clean? Both need the allocator pointed somewhere other than where the
/// policy points, on hardware the maintainers may not own — hence an env
/// knob rather than a patched branch per reporter.
///
/// Values (case-insensitive, `_` interchangeable with `-`):
/// - `tiled-first` — order tiled modifiers ahead of LINEAR, overriding a
///   driver that prefers LINEAR (the NVIDIA question).
/// - `linear-first` — order LINEAR first, overriding a driver that prefers
///   tiled (reproduces the RDNA4 corruption of issue #48).
/// - `0x<hex>` / `<hex>` — try exactly this modifier before all others,
///   whatever the policy says.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScanoutModifierOverride {
    TiledFirst,
    LinearFirst,
    First(u64),
}

impl ScanoutModifierOverride {
    fn describe(self) -> String {
        match self {
            Self::TiledFirst => "tiled-first".to_string(),
            Self::LinearFirst => "linear-first".to_string(),
            Self::First(modifier) => format!("0x{modifier:x}"),
        }
    }
}

/// Parse one `YSERVER_SCANOUT_MODIFIER` value. `None` for anything
/// unrecognised — a typo in a diagnostic env var must not keep the display
/// server from starting, so the caller warns and falls back to the driver
/// policy. Pure so the accepted spellings are unit-testable.
fn parse_scanout_modifier_override(raw: &str) -> Option<ScanoutModifierOverride> {
    let token = raw.trim();
    if token.is_empty() {
        return None;
    }
    match token.to_ascii_lowercase().replace('_', "-").as_str() {
        "tiled-first" => return Some(ScanoutModifierOverride::TiledFirst),
        "linear-first" => return Some(ScanoutModifierOverride::LinearFirst),
        _ => {}
    }
    // Modifier values are logged as `0x…` by `format_modifiers`, so accept
    // that spelling verbatim; also accept bare hex and `_` digit grouping.
    let hex = token
        .strip_prefix("0x")
        .or_else(|| token.strip_prefix("0X"))
        .unwrap_or(token);
    u64::from_str_radix(&hex.replace('_', ""), 16)
        .ok()
        .map(ScanoutModifierOverride::First)
}

/// The process-wide `YSERVER_SCANOUT_MODIFIER` setting. Read once: scanout
/// pools are re-allocated on every modeset, and re-warning per BO would bury
/// the log the override exists to produce.
fn scanout_modifier_override() -> Option<ScanoutModifierOverride> {
    static OVERRIDE: OnceLock<Option<ScanoutModifierOverride>> = OnceLock::new();
    *OVERRIDE.get_or_init(|| {
        let raw = std::env::var("YSERVER_SCANOUT_MODIFIER").ok()?;
        let parsed = parse_scanout_modifier_override(&raw);
        match parsed {
            Some(over) => log::warn!(
                "YSERVER_SCANOUT_MODIFIER={raw} — overriding the per-driver scanout \
                 modifier policy with {}. This is a diagnostic knob; a wrong choice \
                 shows up as a garbled or dithered display, not as an error.",
                over.describe()
            ),
            None => log::warn!(
                "YSERVER_SCANOUT_MODIFIER={raw} is not a recognised value \
                 (expected tiled-first, linear-first, or a 0x<hex> modifier) — \
                 ignoring it and keeping the per-driver policy"
            ),
        }
        parsed
    })
}

/// Whether to order LINEAR ahead of the tiled modifiers, combining the
/// per-driver policy with any [`ScanoutModifierOverride`].
///
/// `First(_)` deliberately leaves the policy alone: it pins ONE modifier at
/// the front, and the rest of the list stays in the order the driver policy
/// asks for, so a failed pin degrades to normal behaviour.
fn resolve_prefer_linear(driver_id: vk::DriverId, over: Option<ScanoutModifierOverride>) -> bool {
    match over {
        Some(ScanoutModifierOverride::TiledFirst) => false,
        Some(ScanoutModifierOverride::LinearFirst) => true,
        Some(ScanoutModifierOverride::First(_)) | None => scanout_prefers_linear(driver_id),
    }
}

/// Move `modifier` to the front of `candidates`, inserting it if absent.
///
/// Absent is legal on purpose: the GBM plan checks Vulkan importability at
/// allocation time and falls through to the next plan when it fails (see
/// [`scanout_allocation_plans`]), so pinning a modifier the Vulkan side did
/// not advertise is a survivable experiment — and one worth running, since
/// GBM can allocate layouts Vulkan declines to export.
fn hoist_modifier_first(candidates: &mut Vec<u64>, modifier: u64) {
    candidates.retain(|&m| m != modifier);
    candidates.insert(0, modifier);
}

fn scanout_modifier_candidates(vk: &VkContext, kms_scanout_modifiers: &[u64]) -> Vec<u64> {
    if kms_scanout_modifiers.is_empty() {
        return Vec::new();
    }

    // Probe with the scanout image's actual usage (color attachment and
    // transfers, never sampled) so LINEAR survives on drivers that only
    // withhold it for SAMPLED (v3dv). Must stay in sync with
    // `scanout_image_usage()`.
    let vulkan =
        super::dri3::supported_modifiers(vk, vk::Format::B8G8R8A8_UNORM, scanout_image_usage());
    let over = scanout_modifier_override();
    let prefer_linear = resolve_prefer_linear(vk.driver_id, over);
    let mut candidates = order_scanout_modifier_candidates(
        kms_scanout_modifiers,
        &vulkan,
        prefer_linear,
        |modifier| scanout_modifier_is_single_plane_exportable(vk, modifier),
    );
    if let Some(ScanoutModifierOverride::First(modifier)) = over {
        if !candidates.contains(&modifier) {
            log::warn!(
                "YSERVER_SCANOUT_MODIFIER pins 0x{modifier:x}, which is not in the \
                 KMS/Vulkan intersection — trying it first anyway (GBM may still \
                 allocate it); allocation falls through to the normal order if it fails"
            );
        }
        hoist_modifier_first(&mut candidates, modifier);
    }
    // Diagnostic for scanout-corruption reports (issue #48): show what
    // the plane offered vs. what survived the Vulkan/exportable filter,
    // so a card that simply has no tiled scanout modifier on offer is
    // distinguishable from one whose tiled modifier we rejected. `override`
    // is included so a log captured during a triage round can't be mistaken
    // for the shipped policy's behaviour.
    log::info!(
        "scanout modifier select: kms_plane={} vulkan_supports={} \
         prefer_linear={prefer_linear} override={} -> candidates={}",
        format_modifiers(kms_scanout_modifiers),
        format_modifiers(&vulkan),
        over.map_or_else(|| "none".to_string(), ScanoutModifierOverride::describe),
        format_modifiers(&candidates),
    );
    candidates
}

fn format_modifiers(modifiers: &[u64]) -> String {
    if modifiers.is_empty() {
        return "[]".to_string();
    }
    let joined = modifiers
        .iter()
        .map(|m| format!("0x{m:x}"))
        .collect::<Vec<_>>()
        .join(",");
    format!("[{joined}]")
}

/// Order the KMS/Vulkan modifier intersection into the sequence the
/// allocator tries.
///
/// Default (`prefer_linear = false`): **tiled modifiers first, `LINEAR` last.**
/// This fixes corruption on RDNA4/gfx12 (RX 9070 XT, issue #48) where a
/// Vulkan-rendered linear scanout buffer produces horizontal tiling
/// artifacts. RADV on gfx8/Polaris doesn't expose
/// `VK_EXT_image_drm_format_modifier`, so those cards never reach this
/// path and allocation falls through to the untagged-linear plan.
///
/// `prefer_linear = true`: **LINEAR first, tiled as fallback.**
/// Used for `NVIDIA_PROPRIETARY` where a *Vulkan-allocated* BLOCK_LINEAR_2D
/// image produces a dithered/scrambled display on Pascal hardware (GTX 1050,
/// GP107) even though allocation and KMS import succeed.
///
/// Note this does NOT mirror what GBM does — the opposite is true, and the
/// earlier claim here that "GBM implicitly selects LINEAR for scanout on
/// those cards" was wrong. NVIDIA's GBM *refuses* LINEAR for a
/// `RENDERING|SCANOUT` BO (`EINVAL`), so on the GBM path this ordering only
/// costs one guaranteed-failed attempt before a tiled variant wins. See
/// [`scanout_prefers_linear`] and the module header.
///
/// Pure (no Vulkan calls of its own) so the ordering policy is unit
/// testable; `is_exportable` is the per-modifier single-plane check.
fn order_scanout_modifier_candidates(
    kms_scanout_modifiers: &[u64],
    vulkan_supported: &[u64],
    prefer_linear: bool,
    is_exportable: impl Fn(u64) -> bool,
) -> Vec<u64> {
    let mut candidates = Vec::new();

    // When LINEAR is preferred (NVIDIA), add it first if both sides advertise it.
    if prefer_linear
        && kms_scanout_modifiers.contains(&super::dri3::DRM_FORMAT_MOD_LINEAR)
        && vulkan_supported.contains(&super::dri3::DRM_FORMAT_MOD_LINEAR)
    {
        candidates.push(super::dri3::DRM_FORMAT_MOD_LINEAR);
    }

    // Non-LINEAR modifiers in KMS-advertised order.
    for &modifier in kms_scanout_modifiers {
        if modifier == super::dri3::DRM_FORMAT_MOD_LINEAR {
            continue;
        }
        if vulkan_supported.contains(&modifier)
            && is_exportable(modifier)
            && !candidates.contains(&modifier)
        {
            candidates.push(modifier);
        }
    }

    // When tiled is preferred (default), LINEAR comes last.
    if !prefer_linear
        && kms_scanout_modifiers.contains(&super::dri3::DRM_FORMAT_MOD_LINEAR)
        && vulkan_supported.contains(&super::dri3::DRM_FORMAT_MOD_LINEAR)
        && !candidates.contains(&super::dri3::DRM_FORMAT_MOD_LINEAR)
    {
        candidates.push(super::dri3::DRM_FORMAT_MOD_LINEAR);
    }

    candidates
}

fn scanout_modifier_is_single_plane_importable(vk: &VkContext, modifier: u64) -> bool {
    scanout_modifier_single_plane_supports_feature(
        vk,
        modifier,
        vk::ExternalMemoryFeatureFlags::IMPORTABLE,
    )
}

fn scanout_modifier_is_single_plane_exportable(vk: &VkContext, modifier: u64) -> bool {
    scanout_modifier_single_plane_supports_feature(
        vk,
        modifier,
        vk::ExternalMemoryFeatureFlags::EXPORTABLE,
    )
}

fn scanout_modifier_single_plane_supports_feature(
    vk: &VkContext,
    modifier: u64,
    feature: vk::ExternalMemoryFeatureFlags,
) -> bool {
    use std::ffi::c_void;

    let mut modifier_info = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
        .drm_format_modifier(modifier)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let mut external_info = vk::PhysicalDeviceExternalImageFormatInfo::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    external_info.p_next = std::ptr::from_mut(&mut modifier_info).cast::<c_void>();

    let mut format_info = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(vk::Format::B8G8R8A8_UNORM)
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(scanout_image_usage());
    format_info.p_next = std::ptr::from_mut(&mut external_info).cast::<c_void>();

    let mut external_props = vk::ExternalImageFormatProperties::default();
    let mut props2 = vk::ImageFormatProperties2::default().push_next(&mut external_props);
    if unsafe {
        vk.instance.get_physical_device_image_format_properties2(
            vk.physical_device,
            &format_info,
            &mut props2,
        )
    }
    .is_err()
    {
        return false;
    }

    external_props
        .external_memory_properties
        .external_memory_features
        .contains(feature)
        && external_props
            .external_memory_properties
            .compatible_handle_types
            .contains(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
        && drm_modifier_plane_count(vk, modifier) == Some(1)
}

fn drm_modifier_plane_count(vk: &VkContext, modifier: u64) -> Option<u32> {
    let modifier_count = {
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
        let mut format_props = vk::FormatProperties2::default().push_next(&mut list);
        unsafe {
            vk.instance.get_physical_device_format_properties2(
                vk.physical_device,
                vk::Format::B8G8R8A8_UNORM,
                &mut format_props,
            );
        }
        list.drm_format_modifier_count
    };
    if modifier_count == 0 {
        return None;
    }

    let mut props_storage =
        vec![vk::DrmFormatModifierPropertiesEXT::default(); modifier_count as usize];
    let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
        .drm_format_modifier_properties(&mut props_storage);
    let mut format_props = vk::FormatProperties2::default().push_next(&mut list);
    unsafe {
        vk.instance.get_physical_device_format_properties2(
            vk.physical_device,
            vk::Format::B8G8R8A8_UNORM,
            &mut format_props,
        );
    }
    let entries = list.drm_format_modifier_count as usize;
    props_storage
        .iter()
        .take(entries)
        .find(|p| p.drm_format_modifier == modifier)
        .map(|p| p.drm_format_modifier_plane_count)
}

fn scanout_image_usage() -> vk::ImageUsageFlags {
    // The scanout image is only ever a compose render target (color
    // attachment), a transfer destination (initial clear / upload), and a
    // transfer source (root screenshots and diagnostic scanout dumps).
    // It is NEVER sampled — RENDER PictOps target pixmap/window mirrors,
    // not the scanout BO, and the compose pass samples those mirrors and
    // blends into this attachment.
    vk::ImageUsageFlags::COLOR_ATTACHMENT
        | vk::ImageUsageFlags::TRANSFER_SRC
        | vk::ImageUsageFlags::TRANSFER_DST
}

fn addfb_flags_for_modifier(modifier: Option<u64>) -> FbCmd2Flags {
    if modifier.is_some() {
        FbCmd2Flags::MODIFIERS
    } else {
        FbCmd2Flags::empty()
    }
}

fn destroy_scanout_image(vk: &VkContext, image: vk::Image, memory: vk::DeviceMemory) {
    unsafe {
        vk.device.destroy_image(image, None);
        vk.device.free_memory(memory, None);
    }
}

/// Outputs of [`allocate_vk_scanout_image`] / [`allocate_gbm_scanout_image`]:
/// a bound VkImage (either allocated directly or imported from GBM),
/// its memory, the dma-buf fd (either exported from Vulkan or read
/// from the gbm_bo), the row pitch, the plane-0 byte offset, the DRM
/// modifier to use for framebuffer registration, and — for the
/// GBM-alloc path — the source gbm_bo the imported VkImage must
/// outlive.
struct VkScanoutImage {
    image: vk::Image,
    memory: vk::DeviceMemory,
    dmabuf: OwnedFd,
    pitch: u32,
    /// Plane-0 byte offset from `VkSubresourceLayout.offset` (Vulkan-alloc)
    /// or `gbm_bo_get_offset(bo, 0)` (GBM-alloc). Passed to AddFB2 —
    /// a tiled block-linear image can encode a non-zero plane offset
    /// and the display engine reads scanout from that offset.
    offset: u32,
    modifier: Option<u64>,
    /// Present only for the GBM-alloc path — the source gbm_bo whose
    /// dma-buf we imported into Vulkan. Kept alive by the caller
    /// (`ScanoutBo`) so it outlives the derived Vulkan memory / GEM
    /// handle / DRM framebuffer.
    gbm_bo: Option<gbm::BufferObject<()>>,
}

/// Allocate a scanout `VkImage` whose memory is dma-buf-exportable;
/// bind memory; export the dma-buf; query the row pitch the driver
/// picked.
fn allocate_vk_scanout_image(
    vk: &VkContext,
    width: u32,
    height: u32,
    plan: ScanoutAllocationPlan,
) -> Result<VkScanoutImage, vk::Result> {
    // GbmModifier is routed via allocate_gbm_scanout_image; the
    // Vulkan-alloc path never sees it.
    debug_assert!(
        !matches!(plan, ScanoutAllocationPlan::GbmModifier(_)),
        "GbmModifier plans must be dispatched via allocate_gbm_scanout_image"
    );
    let ext_memory_fd = vk
        .external_memory_fd
        .as_ref()
        .ok_or(vk::Result::ERROR_EXTENSION_NOT_PRESENT)?;

    let drm_modifier = match plan {
        ScanoutAllocationPlan::DrmModifier(modifier) => Some(modifier),
        ScanoutAllocationPlan::PaddedExplicitLinear { .. }
        | ScanoutAllocationPlan::ExplicitLinear
        | ScanoutAllocationPlan::LegacyLinear => None,
        ScanoutAllocationPlan::GbmModifier(_) => unreachable!(),
    };
    let padded_pitch = match plan {
        ScanoutAllocationPlan::PaddedExplicitLinear { row_pitch } => Some(row_pitch),
        _ => None,
    };

    let mut external_info = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let modifier_storage = [drm_modifier.unwrap_or(super::dri3::DRM_FORMAT_MOD_LINEAR)];
    let mut modifier_list = vk::ImageDrmFormatModifierListCreateInfoEXT::default()
        .drm_format_modifiers(if drm_modifier.is_some() {
            &modifier_storage
        } else {
            &[]
        });
    // Explicit single-plane LINEAR layout carrying the padded (aligned) stride.
    // `size = 0` lets the implementation compute the plane size for the pitch.
    let explicit_plane_layouts = [vk::SubresourceLayout {
        offset: 0,
        size: 0,
        row_pitch: u64::from(padded_pitch.unwrap_or(0)),
        array_pitch: 0,
        depth_pitch: 0,
    }];
    let mut explicit_modifier_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
        .drm_format_modifier(super::dri3::DRM_FORMAT_MOD_LINEAR)
        .plane_layouts(&explicit_plane_layouts);

    let tiling = match plan {
        ScanoutAllocationPlan::DrmModifier(_)
        | ScanoutAllocationPlan::PaddedExplicitLinear { .. } => {
            vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT
        }
        ScanoutAllocationPlan::ExplicitLinear | ScanoutAllocationPlan::LegacyLinear => {
            vk::ImageTiling::LINEAR
        }
        ScanoutAllocationPlan::GbmModifier(_) => unreachable!(),
    };

    let image_info_base = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(vk::Format::B8G8R8A8_UNORM)
        .extent(vk::Extent3D {
            width,
            height,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(tiling)
        .usage(scanout_image_usage())
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED);

    let image_info = match plan {
        ScanoutAllocationPlan::DrmModifier(_) => image_info_base
            .push_next(&mut external_info)
            .push_next(&mut modifier_list),
        ScanoutAllocationPlan::PaddedExplicitLinear { .. } => image_info_base
            .push_next(&mut external_info)
            .push_next(&mut explicit_modifier_info),
        ScanoutAllocationPlan::ExplicitLinear | ScanoutAllocationPlan::LegacyLinear => {
            image_info_base.push_next(&mut external_info)
        }
        ScanoutAllocationPlan::GbmModifier(_) => unreachable!(),
    };

    let image = unsafe { vk.device.create_image(&image_info, None)? };

    // 3. Memory: dma-buf-exportable + dedicated to this image.
    let mem_reqs = unsafe { vk.device.get_image_memory_requirements(image) };
    let mem_props = unsafe {
        vk.instance
            .get_physical_device_memory_properties(vk.physical_device)
    };
    let memory_type_index = match pick_memory_type(
        &mem_props,
        mem_reqs.memory_type_bits,
        vk::MemoryPropertyFlags::DEVICE_LOCAL,
    )
    .or_else(|| {
        pick_memory_type(
            &mem_props,
            mem_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::empty(),
        )
    }) {
        Some(i) => i,
        None => {
            unsafe { vk.device.destroy_image(image, None) };
            return Err(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY);
        }
    };

    let mut export_info = vk::ExportMemoryAllocateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_reqs.size)
        .memory_type_index(memory_type_index)
        .push_next(&mut export_info)
        .push_next(&mut dedicated);

    let memory = match unsafe { vk.device.allocate_memory(&alloc_info, None) } {
        Ok(m) => m,
        Err(e) => {
            unsafe { vk.device.destroy_image(image, None) };
            return Err(e);
        }
    };

    if let Err(e) = unsafe { vk.device.bind_image_memory(image, memory, 0) } {
        unsafe {
            vk.device.free_memory(memory, None);
            vk.device.destroy_image(image, None);
        }
        return Err(e);
    }

    let selected_modifier = match plan {
        ScanoutAllocationPlan::DrmModifier(_) => {
            let Some(ext) = vk.image_drm_format_modifier_ext.as_ref() else {
                unsafe {
                    vk.device.free_memory(memory, None);
                    vk.device.destroy_image(image, None);
                }
                return Err(vk::Result::ERROR_EXTENSION_NOT_PRESENT);
            };
            let mut props = vk::ImageDrmFormatModifierPropertiesEXT::default();
            if let Err(e) =
                unsafe { ext.get_image_drm_format_modifier_properties(image, &mut props) }
            {
                unsafe {
                    vk.device.free_memory(memory, None);
                    vk.device.destroy_image(image, None);
                }
                return Err(e);
            }
            Some(props.drm_format_modifier)
        }
        // Created with an explicit LINEAR modifier — no need to re-query it.
        ScanoutAllocationPlan::PaddedExplicitLinear { .. } => {
            Some(super::dri3::DRM_FORMAT_MOD_LINEAR)
        }
        ScanoutAllocationPlan::ExplicitLinear => Some(super::dri3::DRM_FORMAT_MOD_LINEAR),
        ScanoutAllocationPlan::LegacyLinear => None,
        ScanoutAllocationPlan::GbmModifier(_) => unreachable!(),
    };

    // Row pitch from the driver. We need this for KMS addfb2.
    // Modifier-tiled images MUST be queried with a MEMORY_PLANE aspect;
    // COLOR is a validation error (the single-plane scanout buffer is
    // plane 0). LINEAR-tiled fallbacks keep the COLOR aspect. The
    // padded-explicit-LINEAR image is a DRM-modifier image too → MEMORY_PLANE_0.
    let layout_aspect = match plan {
        ScanoutAllocationPlan::DrmModifier(_)
        | ScanoutAllocationPlan::PaddedExplicitLinear { .. } => {
            vk::ImageAspectFlags::MEMORY_PLANE_0_EXT
        }
        ScanoutAllocationPlan::ExplicitLinear | ScanoutAllocationPlan::LegacyLinear => {
            vk::ImageAspectFlags::COLOR
        }
        ScanoutAllocationPlan::GbmModifier(_) => unreachable!(),
    };
    let layout = unsafe {
        vk.device.get_image_subresource_layout(
            image,
            vk::ImageSubresource {
                aspect_mask: layout_aspect,
                mip_level: 0,
                array_layer: 0,
            },
        )
    };
    let pitch = u32::try_from(layout.row_pitch).unwrap_or(u32::MAX);

    // 4. Export the bound memory as a dma-buf fd.
    let get_fd_info = vk::MemoryGetFdInfoKHR::default()
        .memory(memory)
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let raw_fd = match unsafe { ext_memory_fd.get_memory_fd(&get_fd_info) } {
        Ok(fd) => fd,
        Err(e) => {
            unsafe {
                vk.device.free_memory(memory, None);
                vk.device.destroy_image(image, None);
            }
            return Err(e);
        }
    };
    let dmabuf = super::owned_fd_from_vk(raw_fd, "vkGetMemoryFdKHR(DMA_BUF)")?;

    let offset = u32::try_from(layout.offset).unwrap_or(0);
    Ok(VkScanoutImage {
        image,
        memory,
        dmabuf,
        pitch,
        offset,
        modifier: selected_modifier,
        gbm_bo: None,
    })
}

/// GBM-allocate a single-plane scanout BO with the given DRM
/// modifier, then import its dma-buf into Vulkan as the compose
/// render target. This is the preferred allocation path — see the
/// module doc-comment for why.
///
/// The returned `VkScanoutImage.gbm_bo` MUST be kept alive at least
/// until the returned VkImage/VkDeviceMemory/GEM/framebuffer have all
/// been torn down. `ScanoutBo` handles that by declaring `gbm_bo`
/// last in its field list so Rust drops it after the explicit `Drop`
/// impl has released the derived resources.
fn allocate_gbm_scanout_image(
    vk: &VkContext,
    gbm: &Rc<GbmDevice>,
    width: u32,
    height: u32,
    modifier: u64,
) -> Result<VkScanoutImage, GbmScanoutError> {
    let ext_memory_fd = vk
        .external_memory_fd
        .as_ref()
        .ok_or(GbmScanoutError::MissingExtension(
            "VK_KHR_external_memory_fd",
        ))?;
    if vk.image_drm_format_modifier_ext.is_none() {
        return Err(GbmScanoutError::MissingExtension(
            "VK_EXT_image_drm_format_modifier",
        ));
    }

    // Codex gate: verify Vulkan can IMPORT (not just export) a
    // COLOR_ATTACHMENT image with this exact modifier as DMA_BUF.
    if !scanout_modifier_is_single_plane_importable(vk, modifier) {
        return Err(GbmScanoutError::NotImportable(modifier));
    }

    // 1. GBM allocation — driver-side scanout-layout buffer.
    let modifier_iter = std::iter::once(gbm::Modifier::from(modifier));
    let bo = gbm
        .create_buffer_object_with_modifiers2::<()>(
            width,
            height,
            gbm::Format::Xrgb8888,
            modifier_iter,
            gbm::BufferObjectFlags::RENDERING | gbm::BufferObjectFlags::SCANOUT,
        )
        .map_err(GbmScanoutError::GbmCreate)?;

    // Multi-plane modifiers (e.g. AMD DCC compression) are out of
    // scope for the first cut — see codex correction in the spec.
    let plane_count = bo.plane_count();
    if plane_count != 1 {
        return Err(GbmScanoutError::MultiPlane(plane_count));
    }
    let gbm_modifier: u64 = bo.modifier().into();
    let stride = bo.stride_for_plane(0);
    let offset = bo.offset(0);

    // 2. Bo dma-buf fd. Vulkan takes ownership of the fd we hand to
    //    ImportMemoryFdInfoKHR ONLY on vkAllocateMemory success —
    //    dup so we retain a copy for PRIME_FD_TO_HANDLE afterwards
    //    (matches the DRI3 importer's ownership rule at
    //    target.rs:355).
    let bo_fd = bo.fd().map_err(|_| GbmScanoutError::InvalidBoFd)?;
    let vk_fd_owned = bo_fd.try_clone().map_err(GbmScanoutError::FdDup)?;
    let vk_fd_raw = vk_fd_owned.into_raw_fd();

    // 3. Create the VkImage against GBM's stride/offset via the
    //    explicit-modifier layout struct. Same usage tuple as the
    //    IMPORTABLE gate above (`scanout_image_usage()` + DMA_BUF external
    //    memory + DRM_FORMAT_MODIFIER_EXT tiling).
    let plane_layouts = [vk::SubresourceLayout {
        offset: u64::from(offset),
        size: 0,
        row_pitch: u64::from(stride),
        array_pitch: 0,
        depth_pitch: 0,
    }];
    let mut explicit_modifier_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
        .drm_format_modifier(gbm_modifier)
        .plane_layouts(&plane_layouts);
    let mut external_info = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let image_info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(vk::Format::B8G8R8A8_UNORM)
        .extent(vk::Extent3D {
            width,
            height,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(scanout_image_usage())
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(&mut external_info)
        .push_next(&mut explicit_modifier_info);
    let image = match unsafe { vk.device.create_image(&image_info, None) } {
        Ok(i) => i,
        Err(e) => {
            unsafe { libc::close(vk_fd_raw) };
            return Err(GbmScanoutError::Vk(e));
        }
    };

    // 4. Memory-type selection intersects image requirements with
    //    the dma-buf's own compatible memory types
    //    (vkGetMemoryFdPropertiesKHR). Codex flagged that the DRI3
    //    importer at target.rs:371 skips this — it's mandated for
    //    robust external import and NVIDIA proprietary in particular
    //    exposes distinct memory types for imported vs local BOs.
    let mem_reqs = unsafe { vk.device.get_image_memory_requirements(image) };
    let mut fd_props = vk::MemoryFdPropertiesKHR::default();
    if let Err(e) = unsafe {
        ext_memory_fd.get_memory_fd_properties(
            vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
            vk_fd_raw,
            &mut fd_props,
        )
    } {
        unsafe {
            vk.device.destroy_image(image, None);
            libc::close(vk_fd_raw);
        }
        return Err(GbmScanoutError::Vk(e));
    }
    let mem_props = unsafe {
        vk.instance
            .get_physical_device_memory_properties(vk.physical_device)
    };
    let effective_type_bits = mem_reqs.memory_type_bits & fd_props.memory_type_bits;
    if effective_type_bits == 0 {
        unsafe {
            vk.device.destroy_image(image, None);
            libc::close(vk_fd_raw);
        }
        return Err(GbmScanoutError::NoImportableMemoryType);
    }
    let memory_type_index = pick_memory_type(
        &mem_props,
        effective_type_bits,
        vk::MemoryPropertyFlags::DEVICE_LOCAL,
    )
    .or_else(|| {
        pick_memory_type(
            &mem_props,
            effective_type_bits,
            vk::MemoryPropertyFlags::empty(),
        )
    });
    let Some(memory_type_index) = memory_type_index else {
        unsafe {
            vk.device.destroy_image(image, None);
            libc::close(vk_fd_raw);
        }
        return Err(GbmScanoutError::NoImportableMemoryType);
    };

    let mut import_info = vk::ImportMemoryFdInfoKHR::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
        .fd(vk_fd_raw);
    let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_reqs.size)
        .memory_type_index(memory_type_index)
        .push_next(&mut import_info)
        .push_next(&mut dedicated);
    let memory = match unsafe { vk.device.allocate_memory(&alloc_info, None) } {
        Ok(m) => m,
        Err(e) => {
            unsafe {
                vk.device.destroy_image(image, None);
                // vkAllocateMemory consumes vk_fd_raw only on success.
                libc::close(vk_fd_raw);
            }
            return Err(GbmScanoutError::Vk(e));
        }
    };
    // On success `memory` owns `vk_fd_raw`; do NOT close it here.
    if let Err(e) = unsafe { vk.device.bind_image_memory(image, memory, 0) } {
        unsafe {
            vk.device.free_memory(memory, None);
            vk.device.destroy_image(image, None);
        }
        return Err(GbmScanoutError::Vk(e));
    }

    // Sanity-check what the GBM-imported layout looks like from
    // Vulkan's side. If GBM's stride/offset disagrees with what
    // Vulkan reports back for the same modifier, that's a driver
    // bug worth surfacing; the AddFB2 side always gets GBM's
    // numbers regardless (they came from the same driver that
    // laid out the BO).
    let layout = unsafe {
        vk.device.get_image_subresource_layout(
            image,
            vk::ImageSubresource {
                aspect_mask: vk::ImageAspectFlags::MEMORY_PLANE_0_EXT,
                mip_level: 0,
                array_layer: 0,
            },
        )
    };
    if layout.row_pitch != u64::from(stride) || layout.offset != u64::from(offset) {
        log::warn!(
            "scanout gbm import: layout mismatch — gbm(stride={stride},offset={offset}) \
             vk(row_pitch={},offset={}); using gbm values",
            layout.row_pitch,
            layout.offset,
        );
    }

    // We still need a dma-buf fd for PRIME_FD_TO_HANDLE. Vulkan
    // owns the dup we handed it; reuse the original bo_fd we kept
    // around.
    Ok(VkScanoutImage {
        image,
        memory,
        dmabuf: bo_fd,
        pitch: stride,
        offset,
        modifier: Some(gbm_modifier),
        gbm_bo: Some(bo),
    })
}

#[derive(Debug)]
enum GbmScanoutError {
    MissingExtension(&'static str),
    NotImportable(u64),
    GbmCreate(io::Error),
    MultiPlane(u32),
    InvalidBoFd,
    FdDup(io::Error),
    NoImportableMemoryType,
    Vk(vk::Result),
}

impl std::fmt::Display for GbmScanoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingExtension(name) => write!(f, "missing Vulkan extension: {name}"),
            Self::NotImportable(m) => write!(
                f,
                "modifier 0x{m:x} is not IMPORTABLE + DMA_BUF for COLOR_ATTACHMENT B8G8R8A8_UNORM"
            ),
            Self::GbmCreate(e) => write!(f, "gbm_bo_create_with_modifiers: {e}"),
            Self::MultiPlane(n) => write!(
                f,
                "multi-plane modifier not supported (plane_count={n}); first cut is \
                 single-plane only"
            ),
            Self::InvalidBoFd => write!(f, "gbm_bo_get_fd returned an invalid fd"),
            Self::FdDup(e) => write!(f, "dup(gbm_bo_fd) failed: {e}"),
            Self::NoImportableMemoryType => write!(
                f,
                "no memory type satisfies image requirements ∩ dma-buf-import requirements"
            ),
            Self::Vk(r) => write!(f, "vk error: {r:?}"),
        }
    }
}

/// Adapter that lets a freshly-imported GEM handle be passed to
/// drm 0.15's `add_planar_framebuffer` as a `PlanarBuffer`. Single
/// plane; modifier is present for explicit-modifier addfb2 paths and
/// absent only for the legacy untagged-linear fallback.
struct VkScanoutFb {
    gem_handle: DrmBufferHandle,
    width: u32,
    height: u32,
    pitch: u32,
    offset: u32,
    modifier: Option<u64>,
}

impl DrmPlanarBuffer for VkScanoutFb {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    fn format(&self) -> DrmFourcc {
        DrmFourcc::Xrgb8888
    }
    fn modifier(&self) -> Option<DrmModifier> {
        self.modifier.map(DrmModifier::from)
    }
    fn pitches(&self) -> [u32; 4] {
        [self.pitch, 0, 0, 0]
    }
    fn handles(&self) -> [Option<DrmBufferHandle>; 4] {
        [Some(self.gem_handle), None, None, None]
    }
    fn offsets(&self) -> [u32; 4] {
        [self.offset, 0, 0, 0]
    }
}

/// Create a binary `VkSemaphore` whose payload can be exported as a
/// SYNC_FD via `vkGetSemaphoreFdKHR`. Reused for the bo's full
/// lifetime; the fd payload churns per submit.
fn create_export_semaphore(vk: &VkContext) -> Result<vk::Semaphore, vk::Result> {
    let mut export_info = vk::ExportSemaphoreCreateInfo::default()
        .handle_types(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
    let create_info = vk::SemaphoreCreateInfo::default().push_next(&mut export_info);
    unsafe { vk.device.create_semaphore(&create_info, None) }
}

/// Allocate per-bo transfer resources: command pool + 1 command
/// buffer; staging buffer + host-mapped device memory sized for one
/// XRGB8888 frame at (width × height).
fn allocate_transfer_resources(
    vk: &VkContext,
    width: u32,
    height: u32,
) -> Result<TransferResources, vk::Result> {
    let pool_info = vk::CommandPoolCreateInfo::default()
        .queue_family_index(vk.graphics_queue_family)
        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
    let command_pool = unsafe { vk.device.create_command_pool(&pool_info, None)? };

    let cb_info = vk::CommandBufferAllocateInfo::default()
        .command_pool(command_pool)
        .level(vk::CommandBufferLevel::PRIMARY)
        .command_buffer_count(1);
    let command_buffers = match unsafe { vk.device.allocate_command_buffers(&cb_info) } {
        Ok(cbs) => cbs,
        Err(e) => {
            unsafe { vk.device.destroy_command_pool(command_pool, None) };
            return Err(e);
        }
    };
    let command_buffer = command_buffers[0];

    let staging_size: u64 = u64::from(width) * u64::from(height) * 4;
    let buf_info = vk::BufferCreateInfo::default()
        .size(staging_size)
        .usage(vk::BufferUsageFlags::TRANSFER_SRC)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let staging_buffer = match unsafe { vk.device.create_buffer(&buf_info, None) } {
        Ok(b) => b,
        Err(e) => {
            unsafe { vk.device.destroy_command_pool(command_pool, None) };
            return Err(e);
        }
    };

    let mem_reqs = unsafe { vk.device.get_buffer_memory_requirements(staging_buffer) };
    let mem_props = unsafe {
        vk.instance
            .get_physical_device_memory_properties(vk.physical_device)
    };
    let want_strict = vk::MemoryPropertyFlags::HOST_VISIBLE
        | vk::MemoryPropertyFlags::HOST_COHERENT
        | vk::MemoryPropertyFlags::DEVICE_LOCAL;
    let want_loose = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
    let memory_type_index = pick_memory_type(&mem_props, mem_reqs.memory_type_bits, want_strict)
        .or_else(|| pick_memory_type(&mem_props, mem_reqs.memory_type_bits, want_loose))
        .ok_or(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY);
    let memory_type_index = match memory_type_index {
        Ok(i) => i,
        Err(e) => {
            unsafe {
                vk.device.destroy_buffer(staging_buffer, None);
                vk.device.destroy_command_pool(command_pool, None);
            }
            return Err(e);
        }
    };

    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_reqs.size)
        .memory_type_index(memory_type_index);
    let staging_memory = match unsafe { vk.device.allocate_memory(&alloc_info, None) } {
        Ok(m) => m,
        Err(e) => {
            unsafe {
                vk.device.destroy_buffer(staging_buffer, None);
                vk.device.destroy_command_pool(command_pool, None);
            }
            return Err(e);
        }
    };
    if let Err(e) = unsafe {
        vk.device
            .bind_buffer_memory(staging_buffer, staging_memory, 0)
    } {
        unsafe {
            vk.device.free_memory(staging_memory, None);
            vk.device.destroy_buffer(staging_buffer, None);
            vk.device.destroy_command_pool(command_pool, None);
        }
        return Err(e);
    }

    let mapped_ptr = match unsafe {
        vk.device
            .map_memory(staging_memory, 0, staging_size, vk::MemoryMapFlags::empty())
    } {
        Ok(p) => p,
        Err(e) => {
            unsafe {
                vk.device.free_memory(staging_memory, None);
                vk.device.destroy_buffer(staging_buffer, None);
                vk.device.destroy_command_pool(command_pool, None);
            }
            return Err(e);
        }
    };
    let staging_mapped =
        std::ptr::NonNull::new(mapped_ptr.cast::<u8>()).expect("vkMapMemory returned non-null");

    // 2-query TIMESTAMP pool for the compose GPU-render timer. Created
    // even if the device lacks timestamp support (creation succeeds
    // regardless); `record_composite_command_buffer` gates use on
    // `vk.timestamp_period > 0.0`. Created last so no earlier error path
    // needs to reap it.
    let timestamp_pool = {
        let info = vk::QueryPoolCreateInfo::default()
            .query_type(vk::QueryType::TIMESTAMP)
            .query_count(2);
        unsafe { vk.device.create_query_pool(&info, None)? }
    };

    Ok(TransferResources {
        command_pool,
        command_buffer,
        staging_buffer,
        staging_memory,
        staging_mapped,
        staging_size,
        timestamp_pool,
    })
}

fn pick_memory_type(
    props: &vk::PhysicalDeviceMemoryProperties,
    type_bits: u32,
    required: vk::MemoryPropertyFlags,
) -> Option<u32> {
    (0..props.memory_type_count).find(|&i| {
        let candidate = type_bits & (1 << i) != 0;
        candidate
            && props.memory_types[i as usize]
                .property_flags
                .contains(required)
    })
}

// Compile-only check that `export_signaled_fd`'s call into
// `external_semaphore_fd::Device::get_semaphore_fd` keeps the same
// argument shape. If ash bumps and the signature changes, this
// function fails to compile and breaks the build before any
// integration test runs.
#[cfg(test)]
#[allow(dead_code)]
fn _compile_check_export_signature(
    ext: &ash::khr::external_semaphore_fd::Device,
    semaphore: vk::Semaphore,
) {
    let info = vk::SemaphoreGetFdInfoKHR::default()
        .semaphore(semaphore)
        .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
    let _: Result<i32, vk::Result> = unsafe { ext.get_semaphore_fd(&info) };
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINEAR: u64 = super::super::dri3::DRM_FORMAT_MOD_LINEAR;
    // Representative tiled modifiers (real AMD GFX9+ vendor-tiled values).
    const TILED_A: u64 = 0x0200_0000_0000_0008;
    const TILED_B: u64 = 0x0200_0000_0000_000a;

    #[test]
    fn scanout_usage_matches_render_and_readback_paths() {
        let usage = scanout_image_usage();
        assert!(usage.contains(vk::ImageUsageFlags::COLOR_ATTACHMENT));
        assert!(usage.contains(vk::ImageUsageFlags::TRANSFER_SRC));
        assert!(usage.contains(vk::ImageUsageFlags::TRANSFER_DST));
        assert!(!usage.contains(vk::ImageUsageFlags::SAMPLED));
    }

    #[test]
    fn modifier_order_prefers_tiled_over_linear() {
        // KMS advertises linear first, then a tiled modifier; both are
        // Vulkan-supported and exportable. Tiled must win — issue #48.
        let candidates = order_scanout_modifier_candidates(
            &[LINEAR, TILED_A],
            &[LINEAR, TILED_A],
            false,
            |_| true,
        );
        assert_eq!(
            candidates,
            vec![TILED_A, LINEAR],
            "tiled modifier must precede LINEAR"
        );
    }

    #[test]
    fn modifier_order_keeps_kms_order_among_tiled() {
        let candidates = order_scanout_modifier_candidates(
            &[TILED_B, TILED_A, LINEAR],
            &[TILED_A, TILED_B, LINEAR],
            false,
            |_| true,
        );
        assert_eq!(candidates, vec![TILED_B, TILED_A, LINEAR]);
    }

    #[test]
    fn modifier_order_drops_tiled_not_supported_by_vulkan() {
        let candidates =
            order_scanout_modifier_candidates(&[TILED_A, LINEAR], &[LINEAR], false, |_| true);
        assert_eq!(candidates, vec![LINEAR], "TILED_A not in the Vulkan set");
    }

    #[test]
    fn modifier_order_drops_non_exportable_tiled() {
        // A multi-plane (DCC) tiled modifier fails the single-plane
        // exportability check and must be skipped, leaving LINEAR.
        let candidates =
            order_scanout_modifier_candidates(&[TILED_A, LINEAR], &[TILED_A, LINEAR], false, |m| {
                m != TILED_A
            });
        assert_eq!(candidates, vec![LINEAR]);
    }

    #[test]
    fn modifier_order_linear_only_plane_yields_linear() {
        let candidates = order_scanout_modifier_candidates(&[LINEAR], &[LINEAR], false, |_| true);
        assert_eq!(candidates, vec![LINEAR]);
    }

    #[test]
    fn modifier_order_empty_when_no_intersection() {
        let candidates = order_scanout_modifier_candidates(&[TILED_A], &[TILED_B], false, |_| true);
        assert!(candidates.is_empty());
    }

    #[test]
    fn linear_scanout_stride_alignment_matches_hw_observations() {
        // GTX 1050 @ 2560 wide → pitch 10240 = 256×40 → aligned → LINEAR OK.
        assert!(linear_scanout_stride_aligned(2560));
        // GTX 1060 @ 3440 ultrawide → pitch 13760, mod 256 = 192 → unaligned.
        assert!(!linear_scanout_stride_aligned(3440));
        // Common aligned widths.
        assert!(linear_scanout_stride_aligned(1920)); // 7680 = 256×30
        assert!(linear_scanout_stride_aligned(1280)); // 5120 = 256×20
        assert!(linear_scanout_stride_aligned(3840)); // 15360 = 256×60 (4K)
        // 1366 laptop → 5464, mod 256 = 88 → unaligned.
        assert!(!linear_scanout_stride_aligned(1366));
    }

    #[test]
    fn padded_linear_pitch_rounds_up_to_alignment() {
        // 3440 → tight 13760 → padded up to 13824 = 256×54.
        assert_eq!(padded_linear_pitch(3440), 13824);
        assert!(padded_linear_pitch(3440).is_multiple_of(SCANOUT_PITCH_ALIGN));
        // Already-aligned widths are unchanged.
        assert_eq!(padded_linear_pitch(2560), 10240); // 256×40
        assert_eq!(padded_linear_pitch(1920), 7680); // 256×30
        // 1366 → tight 5464 → padded 5632 = 256×22.
        assert_eq!(padded_linear_pitch(1366), 5632);
    }

    #[test]
    fn scanout_linear_policy_covers_dithering_drivers_not_amd() {
        use super::vk::DriverId;
        // HW-confirmed dithering on tiled scanout → must prefer LINEAR.
        assert!(scanout_prefers_linear(DriverId::NVIDIA_PROPRIETARY));
        assert!(scanout_prefers_linear(DriverId::INTEL_OPEN_SOURCE_MESA));
        // AMD needs tiled (RDNA4 LINEAR corrupts, issue #48) — must NOT prefer.
        assert!(!scanout_prefers_linear(DriverId::MESA_RADV));
        assert!(!scanout_prefers_linear(DriverId::AMD_PROPRIETARY));
        // Unconfirmed drivers stay on the tiled-first default.
        assert!(!scanout_prefers_linear(DriverId::MESA_LLVMPIPE));
    }

    // NVIDIA prefer_linear path — mirrors what GBM does on Pascal (GTX 1050):
    // LINEAR selected first even though tiled modifiers are also advertised.
    #[test]
    fn modifier_order_nvidia_prefers_linear_over_tiled() {
        // NVIDIA KMS advertises tiled first, then LINEAR — but prefer_linear=true
        // must put LINEAR at the front.
        let candidates = order_scanout_modifier_candidates(
            &[TILED_A, TILED_B, LINEAR],
            &[TILED_A, TILED_B, LINEAR],
            true,
            |_| true,
        );
        assert_eq!(candidates, vec![LINEAR, TILED_A, TILED_B]);
    }

    #[test]
    fn modifier_order_nvidia_linear_only_plane_yields_linear() {
        let candidates = order_scanout_modifier_candidates(&[LINEAR], &[LINEAR], true, |_| true);
        assert_eq!(candidates, vec![LINEAR]);
    }

    #[test]
    fn modifier_order_nvidia_no_linear_falls_back_to_tiled() {
        // If LINEAR is absent from the KMS plane, even prefer_linear=true should
        // yield the tiled modifiers (they're the only option).
        let candidates = order_scanout_modifier_candidates(
            &[TILED_A, TILED_B],
            &[TILED_A, TILED_B],
            true,
            |_| true,
        );
        assert_eq!(candidates, vec![TILED_A, TILED_B]);
    }

    #[test]
    fn fresh_bo_is_free() {
        let bo = BoState::default();
        assert_eq!(bo.phase, BoPhase::Free);
        assert!(bo.in_fence_fd.is_none());
        assert!(bo.release_fence_fd.is_none());
    }

    #[test]
    fn record_then_submit_transitions_to_submitted() {
        let mut bo = BoState::default();
        bo.transition_to_recording();
        assert_eq!(bo.phase, BoPhase::Recording);
        bo.transition_to_submitted(/* in_fence */ 42);
        assert_eq!(bo.phase, BoPhase::Submitted);
        assert_eq!(bo.in_fence_fd, Some(42));
    }

    #[test]
    fn submit_with_no_fence_sentinel_does_not_store_fd() {
        let mut bo = BoState::default();
        bo.transition_to_recording();
        bo.transition_to_submitted(/* no fence */ -1);
        assert_eq!(bo.phase, BoPhase::Submitted);
        assert!(bo.in_fence_fd.is_none());
    }

    #[test]
    fn atomic_accept_returns_in_fence_for_caller_to_close_and_stores_out_fence() {
        let mut bo = BoState::default();
        bo.transition_to_recording();
        bo.transition_to_submitted(42);
        let reclaimed = bo.transition_to_pending(/* out_fence */ 99);
        assert_eq!(bo.phase, BoPhase::Pending);
        assert_eq!(
            reclaimed,
            Some(42),
            "caller closes the in-fence fd; kernel only refs the sync_file"
        );
        assert!(bo.in_fence_fd.is_none(), "moved out into reclaimed");
        assert_eq!(bo.release_fence_fd, Some(99));
    }

    #[test]
    fn atomic_accept_with_no_out_fence_sentinel_does_not_store_release_fd() {
        let mut bo = BoState::default();
        bo.transition_to_recording();
        bo.transition_to_submitted(42);
        let reclaimed = bo.transition_to_pending(/* no out fence */ -1);
        assert_eq!(reclaimed, Some(42));
        assert!(bo.release_fence_fd.is_none());
    }

    #[test]
    fn atomic_reject_returns_to_recording_and_we_still_own_in_fence() {
        let mut bo = BoState::default();
        bo.transition_to_recording();
        bo.transition_to_submitted(42);
        let reclaimed = bo.transition_to_recording_after_atomic_reject();
        assert_eq!(bo.phase, BoPhase::Recording);
        assert_eq!(reclaimed, Some(42), "caller closes the fd");
        assert!(bo.in_fence_fd.is_none(), "moved out into reclaimed");
    }

    #[test]
    fn modeset_preempt_from_submitted_returns_in_fence() {
        let mut bo = BoState::default();
        bo.transition_to_recording();
        bo.transition_to_submitted(7);
        let in_fence = bo.transition_to_free_after_modeset_preempt();
        assert_eq!(bo.phase, BoPhase::Free);
        assert_eq!(in_fence, Some(7));
    }

    #[test]
    fn pending_then_onscreen_then_retiring_then_free_releases_fence() {
        let mut bo = BoState::default();
        bo.transition_to_recording();
        bo.transition_to_submitted(11);
        let _ = bo.transition_to_pending(22);
        assert_eq!(bo.phase, BoPhase::Pending);

        bo.transition_to_on_screen();
        assert_eq!(bo.phase, BoPhase::OnScreen);
        assert_eq!(
            bo.release_fence_fd,
            Some(22),
            "release fence stays attached while on-screen"
        );

        bo.transition_to_retiring();
        assert_eq!(bo.phase, BoPhase::Retiring);

        let release = bo.transition_to_free_after_retire();
        assert_eq!(bo.phase, BoPhase::Free);
        assert_eq!(release, Some(22), "caller closes the release fence");
        assert!(bo.release_fence_fd.is_none());
    }

    /// 4.1.2.7 fence-cycle integration test (host-pure variant per
    /// the plan: "mock the GPU side if Vulkan creation under
    /// lavapipe is awkward inside `cargo test`"). Drives a 3-bo
    /// pool through 6 frames in the steady-state cycle and asserts
    /// every fence fd issued is closed exactly once. No real GPU.
    /// The accounting catches state-machine bugs that leak fence
    /// fds (which is exactly the class of bug we hit on bare metal
    /// with the original IN_FENCE_FD ownership confusion).
    #[test]
    fn six_frames_cycle_through_pool_without_leaking_fences() {
        let mut bos: Vec<BoState> = (0..3).map(|_| BoState::default()).collect();
        let mut issued = 0u32;
        let mut closed = 0u32;
        let mut next_fd = 100i32;

        let alloc_fd = |issued: &mut u32, next_fd: &mut i32| -> i32 {
            *issued += 1;
            let fd = *next_fd;
            *next_fd += 1;
            fd
        };
        let close = |fd: Option<i32>, closed: &mut u32| {
            if fd.is_some() {
                *closed += 1;
            }
        };

        for _frame in 0..6 {
            // 1. Acquire Free bo and submit.
            let bo_idx = bos.iter().position(|b| b.phase == BoPhase::Free).expect(
                "with 3 bos and the cycle-advance below, at least one bo \
                 should be Free every frame",
            );
            let bo = &mut bos[bo_idx];
            bo.transition_to_recording();
            let in_fence = alloc_fd(&mut issued, &mut next_fd);
            bo.transition_to_submitted(in_fence);

            // 2. Atomic accept → Pending; closes the in-fence we just
            //    issued.
            let out_fence = alloc_fd(&mut issued, &mut next_fd);
            close(bo.transition_to_pending(out_fence), &mut closed);

            // 3. Pageflip-complete advance (mirrors
            //    `advance_pool_on_pageflip_complete` in backend.rs).
            let phases: Vec<BoPhase> = bos.iter().map(|b| b.phase).collect();
            for (i, phase) in phases.into_iter().enumerate() {
                match phase {
                    BoPhase::Retiring => {
                        close(bos[i].transition_to_free_after_retire(), &mut closed);
                    }
                    BoPhase::OnScreen => bos[i].transition_to_retiring(),
                    BoPhase::Pending => bos[i].transition_to_on_screen(),
                    _ => {}
                }
            }
        }

        // Drain remaining bos (simulates shutdown).
        for bo in &mut bos {
            let r = bo.transition_to_free_after_modeset_reset();
            close(r.in_fence, &mut closed);
            close(r.release_fence, &mut closed);
        }

        assert_eq!(
            issued, closed,
            "every fence fd issued must be closed exactly once \
             (issued={issued}, closed={closed})"
        );
        assert_eq!(
            issued, 12,
            "6 frames × (1 in_fence + 1 release_fence) = 12 fds expected"
        );
    }

    #[test]
    fn modeset_reset_returns_all_currently_held_fences() {
        // Pending: in-fence already returned to caller for closing,
        // live release fence still held by the bo.
        let mut bo = BoState::default();
        bo.transition_to_recording();
        bo.transition_to_submitted(5);
        let _ = bo.transition_to_pending(60);
        let released = bo.transition_to_free_after_modeset_reset();
        assert_eq!(bo.phase, BoPhase::Free);
        assert_eq!(released.in_fence, None);
        assert_eq!(released.release_fence, Some(60));

        // Submitted: still own the in-fence.
        let mut bo = BoState::default();
        bo.transition_to_recording();
        bo.transition_to_submitted(5);
        let released = bo.transition_to_free_after_modeset_reset();
        assert_eq!(released.in_fence, Some(5));
        assert_eq!(released.release_fence, None);

        // Recording: nothing held.
        let mut bo = BoState::default();
        bo.transition_to_recording();
        let released = bo.transition_to_free_after_modeset_reset();
        assert_eq!(released.in_fence, None);
        assert_eq!(released.release_fence, None);
    }

    #[test]
    fn addfb_modifier_flag_tracks_modifier_presence() {
        assert_eq!(addfb_flags_for_modifier(None), FbCmd2Flags::empty());
        assert_eq!(
            addfb_flags_for_modifier(Some(crate::kms::vk::dri3::DRM_FORMAT_MOD_LINEAR)),
            FbCmd2Flags::MODIFIERS
        );
    }

    // ── YSERVER_SCANOUT_MODIFIER override ────────────────────────────

    #[test]
    fn modifier_override_parses_order_keywords() {
        assert_eq!(
            parse_scanout_modifier_override("tiled-first"),
            Some(ScanoutModifierOverride::TiledFirst)
        );
        assert_eq!(
            parse_scanout_modifier_override("linear-first"),
            Some(ScanoutModifierOverride::LinearFirst)
        );
        // Underscores and case are accepted — this is typed by hand on a
        // console during a hardware triage round.
        assert_eq!(
            parse_scanout_modifier_override("TILED_FIRST"),
            Some(ScanoutModifierOverride::TiledFirst)
        );
        assert_eq!(
            parse_scanout_modifier_override("  Linear-First  "),
            Some(ScanoutModifierOverride::LinearFirst)
        );
    }

    #[test]
    fn modifier_override_parses_explicit_modifier() {
        // The block-linear modifier an RTX 3060 Ti / driver 595 actually
        // scans out with (issue #32 telemetry).
        assert_eq!(
            parse_scanout_modifier_override("0x300000000606015"),
            Some(ScanoutModifierOverride::First(0x0300_0000_0060_6015))
        );
        // Bare hex (no 0x) is accepted: the log prints values with 0x, but
        // a copy-paste that loses the prefix should still work.
        assert_eq!(
            parse_scanout_modifier_override("300000000606015"),
            Some(ScanoutModifierOverride::First(0x0300_0000_0060_6015))
        );
        assert_eq!(
            parse_scanout_modifier_override("0X0"),
            Some(ScanoutModifierOverride::First(LINEAR))
        );
    }

    #[test]
    fn modifier_override_rejects_garbage_without_panicking() {
        // A typo must not take the display server down: unparseable values
        // are ignored (with a warning) and the driver policy stands.
        assert_eq!(parse_scanout_modifier_override(""), None);
        assert_eq!(parse_scanout_modifier_override("   "), None);
        assert_eq!(parse_scanout_modifier_override("tiled"), None);
        assert_eq!(parse_scanout_modifier_override("0xzz"), None);
        // Wider than u64.
        assert_eq!(
            parse_scanout_modifier_override("0x1_0000_0000_0000_0000"),
            None
        );
    }

    #[test]
    fn modifier_override_forces_order_against_driver_policy() {
        use super::vk::DriverId;
        // No override → driver policy stands (NVIDIA prefers LINEAR).
        assert!(resolve_prefer_linear(DriverId::NVIDIA_PROPRIETARY, None));
        assert!(!resolve_prefer_linear(DriverId::MESA_RADV, None));
        // tiled-first overrides NVIDIA's LINEAR preference — this is the
        // knob that answers "does GBM tiled scan out clean on Pascal?".
        assert!(!resolve_prefer_linear(
            DriverId::NVIDIA_PROPRIETARY,
            Some(ScanoutModifierOverride::TiledFirst)
        ));
        // linear-first overrides AMD's tiled preference (reproduces #48).
        assert!(resolve_prefer_linear(
            DriverId::MESA_RADV,
            Some(ScanoutModifierOverride::LinearFirst)
        ));
        // An explicit modifier does not change the LINEAR-vs-tiled policy
        // for the REST of the list; hoisting handles the pinned entry.
        assert!(resolve_prefer_linear(
            DriverId::NVIDIA_PROPRIETARY,
            Some(ScanoutModifierOverride::First(TILED_A))
        ));
    }

    #[test]
    fn modifier_override_hoists_pinned_modifier_to_front() {
        // Present in the list → moved to front, relative order preserved.
        let mut candidates = vec![LINEAR, TILED_A, TILED_B];
        hoist_modifier_first(&mut candidates, TILED_B);
        assert_eq!(candidates, vec![TILED_B, LINEAR, TILED_A]);

        // Already first → unchanged.
        let mut candidates = vec![TILED_A, LINEAR];
        hoist_modifier_first(&mut candidates, TILED_A);
        assert_eq!(candidates, vec![TILED_A, LINEAR]);

        // Absent → prepended anyway. The GBM plan checks importability at
        // allocation time and falls through cleanly, so pinning a modifier
        // the Vulkan side did not advertise is a legal experiment rather
        // than a boot failure.
        let mut candidates = vec![LINEAR];
        hoist_modifier_first(&mut candidates, TILED_A);
        assert_eq!(candidates, vec![TILED_A, LINEAR]);

        // Empty candidate list (no modifier survived the filters) still
        // yields the pinned modifier for the GBM path to try.
        let mut candidates = Vec::new();
        hoist_modifier_first(&mut candidates, TILED_A);
        assert_eq!(candidates, vec![TILED_A]);
    }
}
