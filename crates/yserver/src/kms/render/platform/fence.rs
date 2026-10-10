use super::*;

impl std::fmt::Debug for FenceTicketInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `VkContext` owns raw Vulkan handles and doesn't impl
        // `Debug`; opaque-print the `vk` field rather than dragging
        // a Debug derive through the whole device chain.
        f.debug_struct("FenceTicketInner")
            .field("fence", &self.fence)
            .field("signaled_cache", &self.signaled_cache.get())
            .field("pool", &"<weak>")
            .field("vk", &self.vk.as_ref().map(|_| "<Arc<VkContext>>"))
            .field(
                "imported_wait_semaphores",
                &self
                    .imported_wait_semaphores
                    .try_borrow()
                    .map(|waits| waits.len())
                    .unwrap_or_default(),
            )
            .finish()
    }
}

/// Logs a failed `vkGetFenceStatus`. A lost device fails every fence at
/// once (hundreds within a second at teardown, #214), so only the first
/// `ERROR_DEVICE_LOST` is logged; the rest go to debug.
fn log_fence_status_error(site: &str, error: vk::Result) {
    static DEVICE_LOST_LOGGED: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    if error != vk::Result::ERROR_DEVICE_LOST {
        log::warn!("{site}: get_fence_status: {error:?}");
    } else if !DEVICE_LOST_LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        log::error!(
            "{site}: get_fence_status: {error:?}; further device-lost fence errors are \
             logged at debug"
        );
    } else {
        log::debug!("{site}: get_fence_status: {error:?}");
    }
}

impl FenceTicket {
    fn note_status_failure(&self) {
        if let Some(pool) = self.inner.pool.upgrade()
            && let Ok(mut pool) = pool.try_borrow_mut()
        {
            pool.renderer_failed = true;
        }
    }

    /// Non-blocking status query that preserves Vulkan errors for callers
    /// owning resources gated by this ticket.
    pub(crate) fn poll_signaled_result(&self, vk: &VkContext) -> Result<bool, vk::Result> {
        if self.inner.signaled_cache.get() {
            return Ok(true);
        }
        match unsafe { vk.device.get_fence_status(self.inner.fence) } {
            Ok(true) => {
                self.inner.signaled_cache.set(true);
                Ok(true)
            }
            Ok(false) => Ok(false),
            Err(error) => {
                self.note_status_failure();
                Err(error)
            }
        }
    }

    /// Non-blocking signaled check. Caches `true` once observed
    /// so subsequent calls don't hit the driver.
    pub(crate) fn poll_signaled(&self, vk: &VkContext) -> bool {
        match self.poll_signaled_result(vk) {
            Ok(signaled) => signaled,
            Err(e) => {
                log_fence_status_error("FenceTicket::poll_signaled", e);
                false
            }
        }
    }

    /// Synchronous wait. **Off the hot path** — used by
    /// `get_image` readback and shutdown teardown.
    pub(crate) fn wait(&self, vk: &VkContext) -> Result<(), vk::Result> {
        if self.inner.signaled_cache.get() {
            return Ok(());
        }
        // 5 second timeout — long enough to cover any realistic
        // GPU work; if we hit it the device is hung anyway.
        match unsafe {
            vk.device
                .wait_for_fences(&[self.inner.fence], true, 5_000_000_000)
        } {
            Ok(()) => {
                self.inner.signaled_cache.set(true);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Raw fence handle for `vkQueueSubmit2`. Caller MUST NOT
    /// destroy or reset this fence — the ticket owns its
    /// lifetime via the pool.
    pub(crate) fn fence(&self) -> vk::Fence {
        self.inner.fence
    }

    /// Keep this submission's exported signal semaphore alive until its
    /// fence retires. Called only after a successful `vkQueueSubmit2`,
    /// once the semaphore's sync_file has been exported — destroying it
    /// any earlier destroys a semaphore the queue is still using.
    pub(super) fn retain_signal_semaphore(&self, semaphore: vk::Semaphore) {
        self.inner
            .imported_wait_semaphores
            .borrow_mut()
            .push(semaphore);
    }

    /// Keep imported binary wait semaphores alive until this submission's
    /// fence retires. Called only after a successful `vkQueueSubmit2`.
    pub(super) fn retain_imported_wait_semaphores(&self, semaphores: Vec<vk::Semaphore>) {
        if semaphores.is_empty() {
            return;
        }
        self.inner
            .imported_wait_semaphores
            .borrow_mut()
            .extend(semaphores);
    }

    /// Test-only constructor: returns a ticket whose `poll_signaled`
    /// returns `true` and `wait` returns `Ok(())` without ever touching
    /// a real VkDevice. Built with a null fence, `signaled_cache` pre-set
    /// to `true`, and a dangling pool Weak so Drop becomes a no-op.
    /// Use ONLY in unit tests that need a `FenceTicket` value without
    /// constructing a real fence.
    #[cfg(test)]
    pub(crate) fn for_tests_stub() -> Self {
        Self {
            inner: Rc::new(FenceTicketInner {
                fence: vk::Fence::null(),
                signaled_cache: Cell::new(true),
                pool: Weak::<RefCell<FencePoolInner>>::new(),
                vk: None,
                imported_wait_semaphores: RefCell::new(Vec::new()),
            }),
        }
    }
}

impl Drop for FenceTicketInner {
    fn drop(&mut self) {
        let Some(pool) = self.pool.upgrade() else {
            // Pool already gone — `KmsBackend`'s field-drop order
            // runs `platform` (containing `fence_pool`) before
            // `store` / `engine` / `scene`, all of which hold
            // tickets that only release at this point. The
            // `VkContext` is still alive (we kept a strong `Arc`),
            // so destroy the fence handle directly. Pre-2026-05-31
            // this branch bailed out, leaking the fence — 1471
            // VkFences leaked at SIGTERM on bee/MATE. `None` is
            // the `for_tests_stub` shape (no real device); also
            // no-op for `vk::Fence::null()`.
            if let Some(vk) = self.vk.as_ref()
                && self.fence != vk::Fence::null()
            {
                unsafe {
                    for semaphore in self.imported_wait_semaphores.get_mut().drain(..) {
                        vk.device.destroy_semaphore(semaphore, None);
                    }
                    vk.device.destroy_fence(self.fence, None);
                }
            }
            return;
        };
        let mut pool = pool.borrow_mut();
        let signaled = self.signaled_cache.get()
            || match unsafe { pool.vk.device.get_fence_status(self.fence) } {
                Ok(true) => {
                    self.signaled_cache.set(true);
                    true
                }
                Ok(false) => false,
                Err(e) => {
                    log_fence_status_error("FenceTicketInner::drop", e);
                    pool.renderer_failed = true;
                    false
                }
            };
        if signaled {
            unsafe {
                for semaphore in self.imported_wait_semaphores.get_mut().drain(..) {
                    pool.vk.device.destroy_semaphore(semaphore, None);
                }
            }
            pool.recycle(self.fence);
        } else {
            // Unsignaled drop: per the spec, recycling here
            // would race the still-pending GPU work that names
            // this fence (it might be referenced by an
            // in-flight submit). Leak the handle and flag the
            // renderer as failed so the next op surfaces the
            // condition. Once the renderer has failed (a lost device
            // fails every pending fence) the leak is expected: debug.
            if pool.renderer_failed {
                log::debug!(
                    "FenceTicket: leaked unsignaled fence {:?} on drop (renderer failed)",
                    self.fence,
                );
            } else {
                log::error!(
                    "FenceTicket: leaked unsignaled fence {:?} on drop \
                     — renderer_failed will be set on next platform access",
                    self.fence,
                );
            }
            pool.renderer_failed = true;
            pool.leaked_fences.push(self.fence);
        }
    }
}

impl PresentCompletionSignal {
    #[must_use]
    pub(crate) fn semaphore(&self) -> vk::Semaphore {
        self.semaphore
    }

    /// Give up ownership of the semaphore without destroying it, for a
    /// caller that ties its lifetime to a submission fence instead.
    /// The `Arc<VkContext>` is still released normally.
    pub(super) fn into_raw(mut self) -> vk::Semaphore {
        std::mem::replace(&mut self.semaphore, vk::Semaphore::null())
    }

    pub(crate) fn export_sync_file_fd(&self) -> Result<Option<OwnedFd>, vk::Result> {
        let info = vk::SemaphoreGetFdInfoKHR::default()
            .semaphore(self.semaphore)
            .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
        let raw = unsafe { self.vk.semaphore_fd_ext()?.get_semaphore_fd(&info)? };
        crate::kms::vk::optional_sync_fd_from_vk(raw, "vkGetSemaphoreFdKHR(SYNC_FD)")
    }
}

pub(super) fn create_present_completion_signal(
    vk: Arc<VkContext>,
) -> Result<PresentCompletionSignal, vk::Result> {
    let mut export_info = vk::ExportSemaphoreCreateInfo::default()
        .handle_types(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
    let create_info = vk::SemaphoreCreateInfo::default().push_next(&mut export_info);
    let semaphore = unsafe { vk.device.create_semaphore(&create_info, None)? };
    Ok(PresentCompletionSignal { vk, semaphore })
}

impl Drop for PresentCompletionSignal {
    fn drop(&mut self) {
        // Null after `into_raw`: ownership moved to a fence ticket.
        if self.semaphore == vk::Semaphore::null() {
            return;
        }
        unsafe {
            self.vk.device.destroy_semaphore(self.semaphore, None);
        }
    }
}

impl FencePoolInner {
    fn recycle(&mut self, fence: vk::Fence) {
        // Reset to unsignaled so the next acquire can re-pass
        // the handle straight to vkQueueSubmit2 (which requires
        // unsignaled).
        if let Err(e) = unsafe { self.vk.device.reset_fences(&[fence]) } {
            log::warn!("FencePool::recycle: reset_fences: {e:?} — leaking fence");
            self.leaked_fences.push(fence);
            return;
        }
        self.free.push(fence);
    }
}

impl FencePool {
    pub(crate) fn new(vk: Arc<VkContext>) -> Self {
        Self {
            inner: Rc::new(RefCell::new(FencePoolInner {
                vk,
                free: Vec::with_capacity(8),
                leaked_fences: Vec::new(),
                renderer_failed: false,
            })),
        }
    }

    pub(crate) fn acquire(&self) -> Result<FenceTicket, vk::Result> {
        let mut pool = self.inner.borrow_mut();
        let fence = if let Some(f) = pool.free.pop() {
            f
        } else {
            let info = vk::FenceCreateInfo::default();
            unsafe { pool.vk.device.create_fence(&info, None)? }
        };
        let vk = Arc::clone(&pool.vk);
        drop(pool);
        Ok(FenceTicket {
            inner: Rc::new(FenceTicketInner {
                fence,
                signaled_cache: Cell::new(false),
                pool: Rc::downgrade(&self.inner),
                vk: Some(vk),
                imported_wait_semaphores: RefCell::new(Vec::new()),
            }),
        })
    }

    pub(crate) fn renderer_failed(&self) -> bool {
        self.inner
            .try_borrow()
            .map(|p| p.renderer_failed)
            .unwrap_or(true)
    }
}

impl Drop for FencePool {
    fn drop(&mut self) {
        let pool = self.inner.borrow();
        // Best-effort wait so any still-in-flight fence
        // (shouldn't happen but be defensive) is safe to
        // destroy.
        unsafe {
            let _ = pool.vk.device.device_wait_idle();
            for &f in &pool.free {
                pool.vk.device.destroy_fence(f, None);
            }
            for &f in &pool.leaked_fences {
                pool.vk.device.destroy_fence(f, None);
            }
        }
    }
}

impl PlatformBackend {
    pub(crate) fn acquire_present_completion_signal(
        &self,
    ) -> Result<PresentCompletionSignal, vk::Result> {
        let vk = self
            .vk
            .as_ref()
            .ok_or(vk::Result::ERROR_INITIALIZATION_FAILED)?;
        create_present_completion_signal(Arc::clone(vk))
    }

    // ── I6a: FenceTicket primitives ─────────────────────────────

    /// Acquire a fresh, unsignaled fence. Caller passes
    /// `ticket.fence()` to `vkQueueSubmit2` as the signal fence.
    /// Cloned across consumers; final-drop recycles or leaks.
    ///
    /// # Errors
    ///
    /// Returns `Err` if Vk is not initialised (test fixture) or
    /// fence creation fails.
    pub(crate) fn acquire_fence_ticket(&self) -> Result<FenceTicket, vk::Result> {
        let pool = self
            .fence_pool
            .as_ref()
            .ok_or(vk::Result::ERROR_INITIALIZATION_FAILED)?;
        pool.acquire()
    }

    /// Propagate a fence-status failure observed through any cloned ticket to
    /// the platform-wide fatal renderer latch. This closes the gap where a
    /// failed status query otherwise looked like perpetual NOT_READY and held
    /// a failed-submit BO forever.
    pub(crate) fn refresh_fence_pool_failure(&mut self) {
        if self
            .fence_pool
            .as_ref()
            .is_some_and(FencePool::renderer_failed)
        {
            self.renderer_failed = true;
        }
    }
}
