//! Vulkan-direct drawing-op recorders (sub-phase 4.1.4).
//!
//! Spec: docs/superpowers/specs/2026-05-07-phase4-1-vulkan-compositor-design.md
//! "Data flow (drawing pipeline)".
//!
//! Each module records a single op family into a caller-supplied
//! `vk::CommandBuffer`. Ops bracket their own
//! `vkCmdBeginRendering` / `vkCmdEndRendering` and the matching
//! `SHADER_READ_ONLY_OPTIMAL ↔ COLOR_ATTACHMENT_OPTIMAL` layout
//! transitions, leaving the mirror in `SHADER_READ_ONLY_OPTIMAL`
//! when they return so the next composite-pass read sees a layout
//! it can sample.
//!
//! 4.1.4.1: solid-fill family ([`fill`]).
//! 4.1.4.2-4.1.4.9: each lands its own module here.

pub mod copy;
pub mod fill;
pub mod image;
pub mod render;
pub mod scanout_logic_fill;
pub mod text;
pub mod traps;

use std::{ptr::NonNull, sync::Arc};

use ash::vk;

use super::device::VkContext;

/// RAII wrapper around the per-backend drawing-op command pool —
/// separate pool from `MirrorUploader`'s transfer pool so graphics
/// and transfer workloads don't share CB lifetimes. One-shot
/// `with_ops_cb` allocates a primary CB from this pool, records,
/// submits + waits idle, and returns the CB to the pool via
/// `vkResetCommandBuffer` (the pool was created with the
/// `RESET_COMMAND_BUFFER` flag).
pub struct OpsCommandPool {
    vk: Arc<VkContext>,
    pool: vk::CommandPool,
}

impl OpsCommandPool {
    pub fn new(vk: Arc<VkContext>) -> Result<Self, vk::Result> {
        let pool_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(vk.graphics_queue_family)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        let pool = unsafe { vk.device.create_command_pool(&pool_info, None)? };
        Ok(Self { vk, pool })
    }

    pub fn handle(&self) -> vk::CommandPool {
        self.pool
    }
}

impl Drop for OpsCommandPool {
    fn drop(&mut self) {
        unsafe {
            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
            self.vk.device.destroy_command_pool(self.pool, None);
        }
    }
}

/// Allocate a one-shot primary CB from `pool`, run `record`
/// against it, submit + wait, free the CB.
///
/// Free function (not a method on `KmsBackend`) so the caller can
/// hold a `&mut DrawableImage` borrow into the closure without
/// fighting the borrow checker over `&self` vs `&mut self.windows`.
/// Per-op submit + wait is the simple cadence used through
/// the 4.1.4 family port; batch CBs land with 4.1.4.6 RENDER
/// `Composite` where op rate spikes.
///
/// Phase 5 retires the trailing `vkQueueWaitIdle` in favour of a
/// per-op `VkFence` + `vkWaitForFences`. Caller semantics are
/// unchanged (still blocking; data is back on return); wait scope
/// is narrower (this submission only, not the whole queue).
///
/// 5-path failure taxonomy (extends `PaintBatch::submit_and_wait`'s
/// 4-path model — `run_one_shot_op` has the additional pre-submit
/// failure window of `record(...)` and `end_command_buffer`):
///   0. pre-submit failure (begin/record/end): CB safe to free.
///   1a. fence-create failure: CB safe to free.
///   1b. submit failure: destroy fence, CB safe to free.
///   2.  wait failure (CB in flight): LEAK CB + fence. Renderer
///       must be torn down.
///   3.  success: destroy fence + free CB; Ok(()).
/// See inline comments for the exact branching.
pub fn run_one_shot_op<F>(
    vk: &VkContext,
    pool: vk::CommandPool,
    record: F,
) -> Result<(), vk::Result>
where
    F: FnOnce(&VkContext, vk::CommandBuffer) -> Result<(), vk::Result>,
{
    run_one_shot_op_with_wait(vk, pool, None, record)
}

/// [`run_one_shot_op`] with one optional binary semaphore wait. The caller
/// retains semaphore ownership through return: success proves the wait was
/// consumed, while any post-submit error requires the caller to quarantine
/// both the command resources and semaphore with the failed renderer.
pub(crate) fn run_one_shot_op_with_wait<F>(
    vk: &VkContext,
    pool: vk::CommandPool,
    wait_semaphore: Option<vk::Semaphore>,
    record: F,
) -> Result<(), vk::Result>
where
    F: FnOnce(&VkContext, vk::CommandBuffer) -> Result<(), vk::Result>,
{
    let alloc_info = vk::CommandBufferAllocateInfo::default()
        .command_pool(pool)
        .level(vk::CommandBufferLevel::PRIMARY)
        .command_buffer_count(1);
    let cb = unsafe { vk.device.allocate_command_buffers(&alloc_info)?[0] };

    // 5-T1: per-op fence instead of vkQueueWaitIdle. Same
    // blocking semantics for the caller (data is back on return)
    // but narrower wait scope — only waits for THIS submission,
    // not every prior submission on the graphics queue (which
    // today includes composite-side work).
    //
    // 5-path failure taxonomy (extends Phase 4's submit_and_wait
    // model — `run_one_shot_op` has an extra failure window
    // before fence-create because `record(...)` and
    // `end_command_buffer` can fail):
    //
    //   0a. begin_command_buffer fails: CB allocated but never
    //       recorded. Free CB, return Err.
    //   0b. record(...) callback returns Err: CB partially
    //       recorded but never submitted. Free CB, return Err.
    //   0c. end_command_buffer fails: same — CB never submitted.
    //       Free CB, return Err.
    //   1a. create_fence fails: CB recorded, no fence yet. Free
    //       CB, return Err. No fence to destroy.
    //   1b. queue_submit2 fails: CB never queued. Destroy fence,
    //       free CB, return Err.
    //   2.  wait_for_fences fails: CB IS in flight or device is
    //       lost. ABANDON the CB and the fence — Vulkan handles
    //       are leaked until VkContext::Drop. Same leak-not-UB
    //       contract as Phase 4's submit_and_wait. Returns Err;
    //       caller MUST treat the renderer as fatal.
    //   3.  wait_for_fences Ok: destroy fence, free CB, Ok(()).
    //
    // Implementation: the closure tracks whether the failure was
    // pre-submit (CB free is safe) or post-submit (CB free is
    // UB). The simplest encoding is a flag returned alongside
    // the Result, or — as below — a custom enum the outer free
    // matches on. The flag-out-of-closure approach (used here)
    // keeps the closure body simple at the cost of one extra
    // mutable binding.
    let mut cb_safe_to_free = true;
    let result = (|| -> Result<(), vk::Result> {
        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        // Paths 0a / 0b / 0c — pre-submit failures. cb_safe_to_free
        // stays true; the outer block frees the CB on the Err
        // returned here.
        crate::vk_count!(begin_command_buffer);
        unsafe { vk.device.begin_command_buffer(cb, &begin)? };
        record(vk, cb)?;
        crate::vk_count!(end_command_buffer);
        unsafe { vk.device.end_command_buffer(cb)? };

        // Path 1a — fence creation failure (pre-submit).
        let fence_info = vk::FenceCreateInfo::default();
        let fence = unsafe { vk.device.create_fence(&fence_info, None) }?;

        // Path 1b — submit failure. Destroy the fence; the CB is
        // still safe to free (cb_safe_to_free stays true).
        let cb_info = [vk::CommandBufferSubmitInfo::default().command_buffer(cb)];
        let waits = wait_semaphore.map(|semaphore| {
            [vk::SemaphoreSubmitInfo::default()
                .semaphore(semaphore)
                .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)]
        });
        let mut submit = vk::SubmitInfo2::default().command_buffer_infos(&cb_info);
        if let Some(waits) = waits.as_ref() {
            submit = submit.wait_semaphore_infos(waits);
        }
        let submit = [submit];
        crate::vk_count!(queue_submit2);
        crate::vk_count!(submit_one_shot);
        if let Err(e) = unsafe { vk.device.queue_submit2(vk.graphics_queue, &submit, fence) } {
            unsafe { vk.device.destroy_fence(fence, None) };
            return Err(e);
        }

        // From this point on, the CB is in flight; on any Err
        // before fence-destroy, freeing the CB is UB.
        let fences = [fence];
        match unsafe { vk.device.wait_for_fences(&fences, true, u64::MAX) } {
            Ok(()) => {
                // Path 3: clean. Destroy fence; CB free happens
                // outside the closure.
                unsafe { vk.device.destroy_fence(fence, None) };
                Ok(())
            }
            Err(e) => {
                // Path 2: device-lost or similar. Leak the CB AND
                // the fence — both handles are abandoned. Caller
                // observes Err and treats the renderer as failed.
                cb_safe_to_free = false;
                log::error!(
                    "run_one_shot_op: wait_for_fences failed ({e:?}); \
                     CB and fence abandoned. KMS renderer is in an \
                     unrecoverable state — caller MUST tear down or disable."
                );
                Err(e)
            }
        }
    })();

    // Free the CB on every path EXCEPT path 2 (post-submit wait
    // failure). Pre-submit failures (paths 0a/0b/0c/1a/1b) leave
    // the CB unsubmitted, so freeing it is safe. Path 3 (clean
    // success) frees it. Path 2 leaves cb_safe_to_free = false
    // and the CB is abandoned.
    if cb_safe_to_free {
        unsafe { vk.device.free_command_buffers(pool, &[cb]) };
    }
    result
}

/// Host-mapped, growable staging buffer used by image-transfer ops
/// (`PutImage`, `GetImage`, `MitShmPutImage`, `MitShmGetImage`,
/// `MitShmCreatePixmap`). One per backend, reused across ops.
///
/// `TRANSFER_SRC | TRANSFER_DST` so the same buffer serves uploads
/// (`vkCmdCopyBufferToImage`) and readbacks (`vkCmdCopyImageToBuffer`).
/// `HOST_VISIBLE | HOST_COHERENT` so the CPU can memcpy in (PutImage)
/// and read back (GetImage) without explicit flush/invalidate.
pub struct OpsStaging {
    vk: Arc<VkContext>,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    mapped: NonNull<u8>,
    size: u64,
}

unsafe impl Send for OpsStaging {}
unsafe impl Sync for OpsStaging {}

impl OpsStaging {
    pub fn new(vk: Arc<VkContext>, initial_size: u64) -> Result<Self, vk::Result> {
        let (buffer, memory, mapped) = allocate_ops_staging(&vk, initial_size)?;
        Ok(Self {
            vk,
            buffer,
            memory,
            mapped,
            size: initial_size,
        })
    }

    pub fn buffer(&self) -> vk::Buffer {
        self.buffer
    }

    pub fn mapped_ptr(&self) -> *mut u8 {
        self.mapped.as_ptr()
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// Grow if needed so `at_least` bytes fit. After 5-T6, no
    /// synchronous wait — callers all wait per-op via
    /// `run_one_shot_op`'s VkFence, so the OLD buffer's last
    /// referencing CB has already retired.
    pub fn ensure(&mut self, at_least: u64) -> Result<(), vk::Result> {
        if at_least <= self.size {
            return Ok(());
        }
        let mut new_size = self.size.max(1024 * 1024);
        while new_size < at_least {
            new_size = new_size.checked_mul(2).unwrap_or(at_least);
        }
        let (buffer, memory, mapped) = allocate_ops_staging(&self.vk, new_size)?;
        // 5-T6: no queue_wait_idle. All callers of `OpsStaging`
        // (`hw_cursor_refresh`, `read_mirror_pixels`,
        // `try_vk_get_image_pixels`, `dump_scanout_one`) go through
        // `run_one_shot_op` which after 5-T1 waits on a per-op
        // fence before returning. The immediately-prior readback's
        // CB therefore has retired before we get here, and the OLD
        // staging buffer can be freed without any additional wait.
        // If a future caller takes this buffer through a non-waiting
        // path, this comment block becomes the audit point — DO NOT
        // remove without re-auditing.
        unsafe {
            self.vk.device.unmap_memory(self.memory);
            self.vk.device.destroy_buffer(self.buffer, None);
            self.vk.device.free_memory(self.memory, None);
        }
        self.buffer = buffer;
        self.memory = memory;
        self.mapped = mapped;
        self.size = new_size;
        Ok(())
    }
}

impl Drop for OpsStaging {
    fn drop(&mut self) {
        unsafe {
            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
            self.vk.device.unmap_memory(self.memory);
            self.vk.device.destroy_buffer(self.buffer, None);
            self.vk.device.free_memory(self.memory, None);
        }
    }
}

fn allocate_ops_staging(
    vk: &VkContext,
    size: u64,
) -> Result<(vk::Buffer, vk::DeviceMemory, NonNull<u8>), vk::Result> {
    let buf_info = vk::BufferCreateInfo::default()
        .size(size)
        .usage(vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let buffer = unsafe { vk.device.create_buffer(&buf_info, None)? };
    let mem_reqs = unsafe { vk.device.get_buffer_memory_requirements(buffer) };
    let mem_props = unsafe {
        vk.instance
            .get_physical_device_memory_properties(vk.physical_device)
    };
    let want = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
    let memory_type_index = (0..mem_props.memory_type_count)
        .find(|&i| {
            mem_reqs.memory_type_bits & (1 << i) != 0
                && mem_props.memory_types[i as usize]
                    .property_flags
                    .contains(want)
        })
        .ok_or(vk::Result::ERROR_FEATURE_NOT_PRESENT);
    let memory_type_index = match memory_type_index {
        Ok(i) => i,
        Err(e) => {
            unsafe { vk.device.destroy_buffer(buffer, None) };
            return Err(e);
        }
    };
    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_reqs.size)
        .memory_type_index(memory_type_index);
    let memory = match unsafe { vk.device.allocate_memory(&alloc_info, None) } {
        Ok(m) => m,
        Err(e) => {
            unsafe { vk.device.destroy_buffer(buffer, None) };
            return Err(e);
        }
    };
    if let Err(e) = unsafe { vk.device.bind_buffer_memory(buffer, memory, 0) } {
        unsafe {
            vk.device.free_memory(memory, None);
            vk.device.destroy_buffer(buffer, None);
        }
        return Err(e);
    }
    let mapped_ptr = match unsafe {
        vk.device
            .map_memory(memory, 0, size, vk::MemoryMapFlags::empty())
    } {
        Ok(p) => p,
        Err(e) => {
            unsafe {
                vk.device.free_memory(memory, None);
                vk.device.destroy_buffer(buffer, None);
            }
            return Err(e);
        }
    };
    let mapped = NonNull::new(mapped_ptr.cast::<u8>()).expect("vkMapMemory returned non-null");
    Ok((buffer, memory, mapped))
}
