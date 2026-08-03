//! Stage 5 frame-builder Phase A: multi-CB single-submit accumulator.
//!
//! `SubmitGroup` buffers `(VkCommandBuffer, signal_semaphore?)` entries
//! between calls to `flush()`, plus the shared `FenceTicket` used as
//! the group's I6a retirement gate (spec Model A1). `flush()` issues
//! ONE `vkQueueSubmit2` with all buffered CBs and signal semaphores,
//! signaling the shared fence.
//!
//! No-Vk paths are still legal: `append` records the CB into the
//! buffer; `flush` on an empty group is a no-op; `flush` on a fixture
//! without Vk fails with a recognised error.

use ash::vk;

use super::platform::FenceTicket;

/// Reason a flush was triggered. Bumped into telemetry on every
/// non-empty flush so we can tell what's driving the submit cadence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FlushReason {
    SyncBoundary,
    PresentCompletionSignal,
    SceneCompose,
    PageflipRetire,
    MaxSize,
    Shutdown,
    /// Phase B.1: a frame-builder close drove this flush. Used by
    /// `RenderEngine::close_open_frame` regardless of the underlying
    /// `CloseReason`; the close-reason histogram is reported
    /// separately under `frame_builder_close_reason_*`.
    FrameBuilder,
    /// Deferred DamageNotify delivery is about to run and recorded
    /// writes to exported (DRI3/GLX-TFP) backings must be submitted so
    /// their dma-buf WRITE fences exist before the event bytes depart
    /// (`Backend::flush_exported_render_writes`, discussion #100).
    DamagePublish,
}

/// Single buffered command-buffer entry. Mirrors the inputs to the
/// `vk::CommandBufferSubmitInfo` that flush() builds.
#[derive(Debug)]
pub(crate) struct GroupEntry {
    pub(crate) cb: vk::CommandBuffer,
    /// Optional COW present-completion semaphore. Attached by
    /// `close_open_frame` (N10 path); the group appends it to the
    /// eventual submit's `signal_semaphore_infos`.
    pub(crate) signal: Option<vk::Semaphore>,
}

#[derive(Debug)]
pub(crate) struct SubmitGroup {
    entries: Vec<GroupEntry>,
    /// Shared ticket. Lazily acquired on first `open_ticket` call;
    /// cleared on `flush`. None when group is empty.
    ticket: Option<FenceTicket>,
    /// Hard cap on `entries.len()` before append forces a
    /// `MaxSize` flush. Phase B Invariant M1: default 1 for the
    /// duration of B.1–B.4; recovers at B.5.
    max_size: usize,
}

impl SubmitGroup {
    pub(crate) fn new() -> Self {
        // Phase B Invariant M1: every queue submission carries at
        // most ONE command buffer for the duration of the B.1 → B.4
        // sub-phase rollout. The frame builder collapses paint into
        // one CB per frame itself; non-ported paint ops fall back
        // to the pre-Phase-A per-op submit cadence. Bee MATE survives
        // this trivially (see status.md § "2026-05-23 bee MATE-load
        // freeze" — the cap=1 row); other platforms see a temporary
        // submit-rate regression that recovers in B.5 when the
        // SubmitGroup retires entirely.
        Self {
            entries: Vec::new(),
            ticket: None,
            max_size: 1,
        }
    }

    /// Test helper: override the cap. Production-side cap is bumped
    /// via `set_max_size` from `PlatformBackend::open_with_commit`
    /// once Task 4 lands.
    pub(crate) fn set_max_size(&mut self, n: usize) {
        self.max_size = n.max(1);
    }

    /// `#[cfg(test)]` introspection: peek at the buffered entries in
    /// append order. Tests that need to assert "upload CB was
    /// appended before draw CB" use this; without it the only signal
    /// we have is `size()`, which is too weak to catch reorderings.
    #[cfg(test)]
    pub(crate) fn peek_entries(&self) -> &[GroupEntry] {
        &self.entries
    }

    pub(crate) fn is_open(&self) -> bool {
        self.ticket.is_some()
    }

    pub(crate) fn size(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn max_size(&self) -> usize {
        self.max_size
    }

    pub(crate) fn ticket(&self) -> Option<&FenceTicket> {
        self.ticket.as_ref()
    }

    /// Seed the group with a freshly-acquired ticket if not open.
    /// Returns a clone of the (now open) ticket for the caller.
    pub(crate) fn open_with(&mut self, ticket: FenceTicket) -> FenceTicket {
        if self.ticket.is_none() {
            self.ticket = Some(ticket);
        }
        self.ticket.as_ref().expect("just set").clone()
    }

    /// Append a buffered entry. Caller is responsible for forcing a
    /// flush BEFORE calling this if `size() >= max_size`.
    pub(crate) fn append(&mut self, cb: vk::CommandBuffer, signal: Option<vk::Semaphore>) {
        self.entries.push(GroupEntry { cb, signal });
    }

    /// Take all buffered entries + the shared ticket, leaving the
    /// group empty. Caller (PlatformBackend::flush_submit_group)
    /// performs the `vkQueueSubmit2` against the returned data.
    pub(crate) fn take(&mut self) -> (Vec<GroupEntry>, Option<FenceTicket>) {
        (std::mem::take(&mut self.entries), self.ticket.take())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ash::vk::Handle;

    fn fake_cb(n: u64) -> vk::CommandBuffer {
        vk::CommandBuffer::from_raw(n)
    }

    fn fake_sem(n: u64) -> vk::Semaphore {
        vk::Semaphore::from_raw(n)
    }

    #[test]
    fn fresh_group_is_empty_and_closed_with_default_max_size_one() {
        let g = SubmitGroup::new();
        assert!(!g.is_open());
        assert_eq!(g.size(), 0);
        // Phase B Invariant M1: default is 1 for the duration of B.1–B.4.
        assert_eq!(g.max_size(), 1);
    }

    #[test]
    fn peek_entries_returns_in_append_order() {
        let mut g = SubmitGroup::new();
        g.append(fake_cb(11), None);
        g.append(fake_cb(22), Some(fake_sem(99)));
        let entries = g.peek_entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].cb, fake_cb(11));
        assert_eq!(entries[0].signal, None);
        assert_eq!(entries[1].cb, fake_cb(22));
        assert_eq!(entries[1].signal, Some(fake_sem(99)));
    }

    #[test]
    fn append_grows_entries_but_does_not_open_without_ticket() {
        let mut g = SubmitGroup::new();
        g.append(fake_cb(1), None);
        g.append(fake_cb(2), Some(fake_sem(7)));
        assert_eq!(g.size(), 2);
        assert!(!g.is_open(), "no ticket seeded yet");
    }

    #[test]
    fn take_leaves_group_empty_and_closed() {
        let mut g = SubmitGroup::new();
        g.append(fake_cb(1), None);
        g.append(fake_cb(2), Some(fake_sem(7)));
        let (entries, ticket) = g.take();
        assert_eq!(entries.len(), 2);
        assert!(ticket.is_none(), "no ticket was seeded in this fixture");
        assert_eq!(g.size(), 0);
        assert!(!g.is_open());
    }

    #[test]
    fn signal_semaphore_attached_to_entry_survives_take() {
        let mut g = SubmitGroup::new();
        g.append(fake_cb(1), None);
        g.append(fake_cb(2), Some(fake_sem(42)));
        let (entries, _) = g.take();
        assert_eq!(entries[0].signal, None);
        assert_eq!(entries[1].signal, Some(fake_sem(42)));
    }

    #[test]
    fn set_max_size_clamps_growth_signal() {
        let mut g = SubmitGroup::new();
        g.set_max_size(4);
        assert_eq!(g.max_size(), 4);
    }
}
