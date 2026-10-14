//! Frame-slot bookkeeping of one surface: which offscreen slot is free,
//! being composed, composed but not yet in the swapchain, presented, or the
//! latest on screen, and what each slot last presented (buffer age).

/// Where a slot is in its frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SlotPhase {
    Free,
    /// The scene's record step runs on it.
    Recording,
    /// Composed; the copy into the swapchain has not been submitted.
    Composed,
    /// Copied into a swapchain image and queued for present.
    Presented,
    /// The latest retired frame.
    OnScreen,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SlotState {
    pub(crate) phase: SlotPhase,
    pub(crate) last_present_generation: Option<u64>,
    pub(crate) content_invalidated: bool,
}

/// The slots of one surface; at most one frame is pending at a time.
#[derive(Debug)]
pub(crate) struct SlotRing {
    slots: Vec<SlotState>,
    /// The slot whose frame awaits retirement.
    pending: Option<usize>,
}

impl SlotRing {
    pub(crate) fn new(count: usize) -> Self {
        Self {
            slots: vec![
                SlotState {
                    phase: SlotPhase::Free,
                    last_present_generation: None,
                    content_invalidated: false,
                };
                count
            ],
            pending: None,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.slots.len()
    }

    pub(crate) fn get(&self, idx: usize) -> Option<&SlotState> {
        self.slots.get(idx)
    }

    /// The first free slot.
    pub(crate) fn first_free(&self) -> Option<usize> {
        self.slots.iter().position(|s| s.phase == SlotPhase::Free)
    }

    /// `Free → Recording`; `false` if the slot is not free.
    pub(crate) fn begin_recording(&mut self, idx: usize) -> bool {
        match self.slots.get_mut(idx) {
            Some(slot) if slot.phase == SlotPhase::Free && self.pending.is_none() => {
                slot.phase = SlotPhase::Recording;
                true
            }
            _ => false,
        }
    }

    /// `Recording → Composed`: the frame is pending.
    pub(crate) fn composed(&mut self, idx: usize) {
        if let Some(slot) = self.slots.get_mut(idx)
            && slot.phase == SlotPhase::Recording
        {
            slot.phase = SlotPhase::Composed;
            self.pending = Some(idx);
        }
    }

    /// The pending frame's slot and phase.
    pub(crate) fn pending(&self) -> Option<(usize, SlotPhase)> {
        self.pending.map(|idx| (idx, self.slots[idx].phase))
    }

    /// The pending `Composed` frame was copied and queued for present.
    pub(crate) fn presented(&mut self) {
        if let Some(idx) = self.pending
            && self.slots[idx].phase == SlotPhase::Composed
        {
            self.slots[idx].phase = SlotPhase::Presented;
        }
    }

    /// Retire the pending frame: it becomes `OnScreen` and the previous
    /// `OnScreen` slot is freed. Returns `(retired, presented)`.
    pub(crate) fn retire(&mut self) -> Option<(Option<usize>, usize)> {
        let presented = self.pending.take()?;
        let retired = self
            .slots
            .iter()
            .position(|s| s.phase == SlotPhase::OnScreen);
        if let Some(prev) = retired {
            self.slots[prev].phase = SlotPhase::Free;
        }
        self.slots[presented].phase = SlotPhase::OnScreen;
        Some((retired, presented))
    }

    /// The latest retired frame's slot.
    pub(crate) fn on_screen(&self) -> Option<usize> {
        self.slots
            .iter()
            .position(|s| s.phase == SlotPhase::OnScreen)
    }

    /// A record that failed before or after its submit: the slot is free
    /// again (the scene recycles a submitted one only after its fence).
    pub(crate) fn release(&mut self, idx: usize) {
        if let Some(slot) = self.slots.get_mut(idx)
            && slot.phase != SlotPhase::OnScreen
        {
            slot.phase = SlotPhase::Free;
            if self.pending == Some(idx) {
                self.pending = None;
            }
        }
    }

    /// The slot's content is indeterminate: the next frame in it repaints
    /// in full.
    pub(crate) fn invalidate(&mut self, idx: usize) {
        if let Some(slot) = self.slots.get_mut(idx) {
            slot.content_invalidated = true;
            slot.last_present_generation = None;
        }
    }

    /// The slot presented frame `generation`.
    pub(crate) fn commit(&mut self, idx: usize, generation: u64) {
        if let Some(slot) = self.slots.get_mut(idx) {
            slot.last_present_generation = Some(generation);
            slot.content_invalidated = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{SlotPhase, SlotRing};

    #[test]
    fn frames_rotate_through_free_pending_and_on_screen() {
        let mut ring = SlotRing::new(3);
        let mut on_screen = None;
        for frame in 1..=4u64 {
            let idx = ring.first_free().expect("a free slot");
            assert!(ring.begin_recording(idx));
            ring.composed(idx);
            assert_eq!(ring.pending(), Some((idx, SlotPhase::Composed)));
            ring.presented();
            assert_eq!(ring.pending(), Some((idx, SlotPhase::Presented)));
            let (retired, presented) = ring.retire().expect("pending");
            assert_eq!((retired, presented), (on_screen, idx));
            ring.commit(idx, frame);
            on_screen = Some(idx);
            assert_eq!(ring.on_screen(), on_screen);
            assert_eq!(ring.get(idx).unwrap().last_present_generation, Some(frame));
        }
        assert!(ring.retire().is_none(), "nothing pending");
    }

    #[test]
    fn one_frame_pending_at_a_time() {
        let mut ring = SlotRing::new(3);
        assert!(ring.begin_recording(0));
        ring.composed(0);
        assert!(!ring.begin_recording(1), "a frame is pending");
    }

    #[test]
    fn a_failed_record_frees_the_slot_and_invalidation_drops_its_age() {
        let mut ring = SlotRing::new(2);
        assert!(ring.begin_recording(0));
        ring.release(0);
        assert_eq!(ring.get(0).unwrap().phase, SlotPhase::Free);
        ring.commit(1, 7);
        ring.invalidate(1);
        let slot = ring.get(1).unwrap();
        assert!(slot.content_invalidated);
        assert_eq!(slot.last_present_generation, None);
    }
}
