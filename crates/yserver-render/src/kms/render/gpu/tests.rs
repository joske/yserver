use super::*;

// ── Task 3 test helpers ──────────────────────────────────────

#[cfg(test)]
impl GpuCore {
    pub(crate) fn submit_group_max_size_for_tests(&self) -> usize {
        self.submit_group.max_size()
    }

    pub(crate) fn force_next_submit_failure_for_tests(&mut self) {
        self.force_next_submit_failure = true;
    }
}

/// Fence acquire on a no-Vk fixture returns the
/// "init failed" error (since fence_pool is None). This
/// confirms the guard is wired; real fence allocation is
/// covered by Stage 2c+ Vk-backed tests.
#[test]
fn for_tests_fence_acquire_errors_without_vk() {
    let p = GpuCore::for_tests();
    let result = p.acquire_fence_ticket();
    assert!(matches!(
        result,
        Err(vk::Result::ERROR_INITIALIZATION_FAILED)
    ));
}

#[test]
fn platform_starts_with_empty_closed_submit_group() {
    let p = GpuCore::for_tests();
    assert!(!p.submit_group_is_open(), "fresh platform has closed group");
    assert_eq!(p.submit_group_size(), 0);
}

#[test]
fn flush_submit_group_empty_is_noop() {
    let mut p = GpuCore::for_tests();
    // Fixture has no Vk; should NOT attempt queue_submit2.
    let outcome = p
        .flush_submit_group(FlushReason::SceneCompose)
        .expect("empty-group flush is always Ok");
    assert_eq!(outcome.flushed_entries, 0);
    assert!(!p.submit_group_is_open());
}

/// Mirrors `descriptor_pool_ring::tests::vk_or_skip` — needed
/// because `VkContext::new()` requires a live Vulkan ICD which
/// isn't always available in CI.
fn vk_or_skip() -> Option<Arc<VkContext>> {
    match VkContext::new() {
        Ok(vk) => Some(vk),
        Err(e) => {
            eprintln!("skipping: no Vk: {e:?}");
            None
        }
    }
}

/// Regression: `KmsBackend`'s field-drop order runs `platform`
/// (containing `fence_pool`) BEFORE `store` / `engine` / `scene`,
/// all of which hold `FenceTicket`s. Pre-fix those tickets
/// dropped after the pool was gone, `FenceTicketInner::drop`
/// bailed on `Weak::upgrade() == None`, and leaked every VkFence
/// handle (1471 leaked at SIGTERM on bee/MATE 2026-05-31, all
/// `VkFence` per the validation layer's first-10 list). Fix
/// added a strong `Arc<VkContext>` on `FenceTicketInner` so the
/// fallback `Drop` path destroys the fence directly. This test
/// simulates the order bug by dropping the pool first and then
/// the ticket; it verifies the device is still usable after
/// (which a leaked-handle path would still allow, but a
/// use-after-free wouldn't). Validation-layer leak verification
/// is via the smoke recipe with VK_LAYER_KHRONOS_validation.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn fence_ticket_destroys_fence_when_pool_dropped_first() {
    let Some(vk) = vk_or_skip() else { return };
    let pool = FencePool::new(Arc::clone(&vk));
    let ticket = pool.acquire().expect("acquire");

    // Simulate KmsBackend's drop-order bug: pool drops while
    // ticket is still alive (held by store/engine/scene state).
    drop(pool);

    // The ticket's strong Arc<VkContext> + ours keep the device
    // alive. Pre-fix this drop leaked the fence handle; post-fix
    // it calls destroy_fence directly.
    drop(ticket);

    // Device still usable — wait_idle returns Ok and we can
    // create + destroy another fence cleanly.
    unsafe { vk.device.device_wait_idle().expect("wait_idle") };
    let f = unsafe {
        vk.device
            .create_fence(&vk::FenceCreateInfo::default(), None)
            .expect("create_fence")
    };
    unsafe { vk.device.destroy_fence(f, None) };
}

/// `for_tests_stub` constructs a `FenceTicket` with no real
/// device. The fallback `Drop` path must no-op cleanly in that
/// case (null fence + `vk: None`) and not segfault attempting
/// to call `destroy_fence` on a null Arc.
#[test]
fn for_tests_stub_drops_cleanly_without_vk() {
    let ticket = FenceTicket::for_tests_stub();
    // Drop runs at end of scope; no-op expected.
    drop(ticket);
}

/// Imported SYNC_FD wait semaphores must attach to the shared ticket
/// inner, so every clone observes the same submission-lifetime pins.
#[test]
fn fence_ticket_retains_imported_wait_semaphores_across_clones() {
    let ticket = FenceTicket::for_tests_stub();
    let clone = ticket.clone();
    ticket.retain_imported_wait_semaphores(vec![vk::Semaphore::null()]);
    assert_eq!(clone.inner.imported_wait_semaphores.borrow().len(), 1);
}

/// Without SYNC_FD the completion signal is a plain semaphore whose batch
/// polls; no export is attempted.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn present_completion_signal_without_sync_fd_is_not_exportable() {
    let options = crate::kms::vk::device::VkContextOptions {
        wants_kms_interop: false,
        ..crate::kms::vk::device::VkContextOptions::KMS
    };
    let vk = match VkContext::new_with_options(options) {
        Ok(vk) => vk,
        Err(e) => {
            eprintln!("skipping: no Vk: {e:?}");
            return;
        }
    };
    let signal =
        super::fence::create_present_completion_signal(Arc::clone(&vk)).expect("plain semaphore");
    assert!(!signal.sync_file_exportable());
    let wait = crate::kms::render::present_batch::PresentBatchWait::from_signal_export(
        "test",
        signal.sync_file_exportable(),
        || panic!("export attempted without SYNC_FD"),
    );
    assert!(matches!(
        wait,
        crate::kms::render::present_batch::PresentBatchWait::Poll
    ));
}
