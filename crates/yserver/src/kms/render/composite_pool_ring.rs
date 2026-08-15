//! Per-output ring of descriptor pools for composite recording.
//!
//! Phase 2: replaces the single shared `CompositorPipeline.descriptor_pool`
//! that was reset at the start of every composite pass. With per-frame
//! ownership of a pool, multiple per-output composites can be in flight
//! simultaneously without one's reset invalidating another's sets.
//!
//! Sizing rationale: a pool slot is held in_use from `acquire` (at
//! composite record time) until the matching `InFlightFrame` reaches
//! `fully_retired()`. Full retirement requires the frame's scanout BO
//! to advance to `BoPhase::Free`, which takes three pageflip-complete
//! events from submit (`Pending → OnScreen → Retiring → Free`). The
//! existing flip-pending skip paces records at one per pageflip cycle,
//! so the steady-state pipeline has up to three concurrent
//! `InFlightFrame`s per output (one in each of OnScreen/Retiring
//! plus the just-submitted Pending). Each holds its acquired slot.
//! The composite about to record acquires the next available slot.
//! `RING_LEN = 3` is the minimum that keeps a slot available at
//! record time; smaller values exhaust the ring under steady-state
//! vsync rendering with a 3-BO scanout pool.

use std::sync::Arc;

use ash::vk;

use crate::kms::vk::device::VkContext;

/// Number of descriptor pools per output. Sized to match the depth of
/// the scanout `BoPhase` retirement pipeline; see the module-level
/// "Sizing rationale" doc.
pub const RING_LEN: usize = 3;

/// Slot-tracking state. Extracted from `CompositePoolRing` so the
/// state machine can be unit-tested without a real `VkContext`.
#[derive(Debug, Default)]
struct SlotTracker {
    in_use: [bool; RING_LEN],
}

impl SlotTracker {
    fn acquire(&mut self) -> Option<usize> {
        for i in 0..RING_LEN {
            if !self.in_use[i] {
                self.in_use[i] = true;
                return Some(i);
            }
        }
        None
    }

    fn release(&mut self, slot: usize) {
        debug_assert!(slot < RING_LEN, "SlotTracker::release: slot out of range");
        debug_assert!(self.in_use[slot], "SlotTracker::release: slot not in use");
        self.in_use[slot] = false;
    }

    #[cfg(test)]
    fn slots_in_use(&self) -> usize {
        self.in_use.iter().filter(|&&b| b).count()
    }
}

pub struct CompositePoolRing {
    pools: [vk::DescriptorPool; RING_LEN],
    tracker: SlotTracker,
    vk: Arc<VkContext>,
}

impl std::fmt::Debug for CompositePoolRing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompositePoolRing")
            .field("tracker", &self.tracker)
            .finish_non_exhaustive()
    }
}

impl CompositePoolRing {
    /// Create a new ring with `RING_LEN` descriptor pools, each
    /// sized for `max_sets_per_pool` COMBINED_IMAGE_SAMPLER sets.
    pub fn new(vk: Arc<VkContext>, max_sets_per_pool: u32) -> Result<Self, vk::Result> {
        let pool_sizes = [vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(max_sets_per_pool)];
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(max_sets_per_pool)
            .pool_sizes(&pool_sizes);

        let mut pools = [vk::DescriptorPool::null(); RING_LEN];
        for i in 0..RING_LEN {
            pools[i] = match unsafe { vk.device.create_descriptor_pool(&pool_info, None) } {
                Ok(p) => p,
                Err(e) => {
                    // Roll back previously created pools.
                    for &p in &pools[..i] {
                        unsafe { vk.device.destroy_descriptor_pool(p, None) };
                    }
                    return Err(e);
                }
            };
        }

        Ok(Self {
            pools,
            tracker: SlotTracker::default(),
            vk,
        })
    }

    /// Borrow a free pool slot. Returns its index, or `None` if all
    /// `RING_LEN` slots are in use (backpressure). Caller is
    /// responsible for calling `release(slot)` when the slot's
    /// frame fully retires.
    pub fn acquire(&mut self) -> Option<usize> {
        self.tracker.acquire()
    }

    /// Return a pool slot to the ring. Resets the pool (invalidates
    /// any descriptor sets allocated from it) and marks the slot
    /// available for the next `acquire`.
    pub fn release(&mut self, slot: usize) {
        unsafe {
            let _ = self
                .vk
                .device
                .reset_descriptor_pool(self.pools[slot], vk::DescriptorPoolResetFlags::empty());
        }
        self.tracker.release(slot);
    }

    /// Return the raw `VkDescriptorPool` handle for `slot`. Caller
    /// uses this with `vkAllocateDescriptorSets` against the shared
    /// `descriptor_set_layout`.
    pub fn pool_at(&self, slot: usize) -> vk::DescriptorPool {
        self.pools[slot]
    }
}

impl Drop for CompositePoolRing {
    fn drop(&mut self) {
        // The frames that owned these pools are gone; destroy them.
        // Drop runs only when the parent ActiveOutput is itself
        // being torn down (hotplug-remove or server shutdown).
        unsafe {
            let _ = self.vk.device.device_wait_idle();
            for &p in &self.pools {
                if p != vk::DescriptorPool::null() {
                    self.vk.device.destroy_descriptor_pool(p, None);
                }
            }
        }
    }
}

// CompositePoolRing handles VkDescriptorPool which is !Send/!Sync
// in ash's safe shim, but the single-threaded-core invariant
// (Phase 6.8) means the backend never crosses threads with this.
// Mark Send so KmsBackend stays Send for the existing trait.
unsafe impl Send for CompositePoolRing {}

#[cfg(test)]
mod tests {
    use super::*;

    // `SlotTracker` is exercised here; the full `CompositePoolRing`
    // needs a real `VkContext` (for both `new` and `Drop`) and is
    // validated by the hardware smoke in T8.

    #[test]
    fn fresh_tracker_has_no_slots_in_use() {
        let t = SlotTracker::default();
        assert_eq!(t.slots_in_use(), 0);
    }

    #[test]
    fn acquire_returns_monotonic_slots_until_full() {
        let mut t = SlotTracker::default();
        let a = t.acquire();
        let b = t.acquire();
        let c = t.acquire();
        let d = t.acquire();
        assert_eq!(a, Some(0));
        assert_eq!(b, Some(1));
        assert_eq!(c, Some(2));
        assert_eq!(
            d, None,
            "fourth acquire on a RING_LEN=3 ring must return None"
        );
        assert_eq!(t.slots_in_use(), 3);
    }

    #[test]
    fn release_frees_slot_for_reuse() {
        let mut t = SlotTracker::default();
        let _ = t.acquire();
        let _ = t.acquire();
        t.release(0);
        let reacquired = t.acquire();
        assert_eq!(reacquired, Some(0), "released slot must be re-acquirable");
    }

    #[test]
    #[should_panic(expected = "slot not in use")]
    fn release_of_already_free_slot_panics_in_debug() {
        let mut t = SlotTracker::default();
        t.release(0);
    }

    #[test]
    fn slots_in_use_reflects_acquire_and_release() {
        let mut t = SlotTracker::default();
        assert_eq!(t.slots_in_use(), 0);
        t.acquire();
        assert_eq!(t.slots_in_use(), 1);
        t.acquire();
        assert_eq!(t.slots_in_use(), 2);
        t.release(0);
        assert_eq!(t.slots_in_use(), 1);
        t.release(1);
        assert_eq!(t.slots_in_use(), 0);
    }
}
