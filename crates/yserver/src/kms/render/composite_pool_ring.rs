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
    capacities: [u32; RING_LEN],
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
        let mut pools = [vk::DescriptorPool::null(); RING_LEN];
        for i in 0..RING_LEN {
            pools[i] = match create_pool(&vk, max_sets_per_pool.max(1)) {
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
            capacities: [max_sets_per_pool.max(1); RING_LEN],
            tracker: SlotTracker::default(),
            vk,
        })
    }

    /// Borrow an empty slot with capacity for the complete frame. Only a
    /// free slot can grow; pools held by other frames are never touched.
    /// A failed growth leaves the old pool available for a later retry.
    pub fn acquire(&mut self, required_sets: usize) -> Result<Option<usize>, vk::Result> {
        let vk = Arc::clone(&self.vk);
        self.acquire_with(required_sets, |capacity| create_pool(&vk, capacity))
    }

    fn acquire_with(
        &mut self,
        required_sets: usize,
        create: impl FnOnce(u32) -> Result<vk::DescriptorPool, vk::Result>,
    ) -> Result<Option<usize>, vk::Result> {
        let required =
            u32::try_from(required_sets).map_err(|_| vk::Result::ERROR_OUT_OF_POOL_MEMORY)?;
        let Some(slot) = self.tracker.acquire() else {
            return Ok(None);
        };
        if required.max(1) > self.capacities[slot] {
            let capacity = required
                .max(1)
                .checked_next_power_of_two()
                .unwrap_or(required);
            let replacement = match create(capacity) {
                Ok(pool) => pool,
                Err(error) => {
                    self.tracker.release(slot);
                    return Err(error);
                }
            };
            // Acquire selected a free slot: its preceding frame has retired.
            // Create first so allocation failure preserves the usable pool.
            unsafe {
                self.vk
                    .device
                    .destroy_descriptor_pool(self.pools[slot], None)
            };
            self.pools[slot] = replacement;
            self.capacities[slot] = capacity;
        }
        Ok(Some(slot))
    }

    /// Return a pool slot to the ring. Resets the pool (invalidates
    /// any descriptor sets allocated from it) and marks the slot
    /// available for the next `acquire`.
    pub fn release(&mut self, slot: usize) {
        assert!(
            self.tracker.in_use[slot],
            "releasing an unused composite pool"
        );
        let reset = unsafe {
            self.vk
                .device
                .reset_descriptor_pool(self.pools[slot], vk::DescriptorPoolResetFlags::empty())
        };
        if let Err(error) = reset {
            // The sets may still occupy the pool. Never advertise its old
            // capacity as empty; acquire will replace it before recording.
            log::warn!("render compose: descriptor pool reset failed: {error:?}");
            self.capacities[slot] = 0;
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

fn create_pool(vk: &VkContext, capacity: u32) -> Result<vk::DescriptorPool, vk::Result> {
    let sizes = [vk::DescriptorPoolSize::default()
        .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
        .descriptor_count(capacity)];
    let info = vk::DescriptorPoolCreateInfo::default()
        .max_sets(capacity)
        .pool_sizes(&sizes);
    unsafe { vk.device.create_descriptor_pool(&info, None) }
}

impl Drop for CompositePoolRing {
    fn drop(&mut self) {
        // The frames that owned these pools are gone; destroy them.
        // Drop runs only when the parent OutputLayout is itself
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

    /// Diagnostic for PR #112. Run explicitly with Lavapipe selected:
    /// VK_DRIVER_FILES=/usr/share/vulkan/icd.d/lvp_icd.json cargo test -p yserver \
    /// pr112_software_pool_pressure -- --ignored --nocapture
    ///
    /// This exercises real Vulkan allocation and ring ownership, without
    /// KMS or queue submission. It does not qualify GPU fence retirement.
    #[test]
    #[ignore = "requires an explicitly selected CPU Vulkan ICD"]
    fn pr112_software_pool_pressure() {
        use crate::kms::vk::pipeline::INITIAL_DESCRIPTOR_SETS_PER_FRAME;
        use std::collections::HashSet;

        let vk = VkContext::new().expect("software Vulkan must initialize; do not silently skip");
        let properties = unsafe {
            vk.instance
                .get_physical_device_properties(vk.physical_device)
        };
        assert_eq!(properties.device_type, vk::PhysicalDeviceType::CPU);
        let bindings = [vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)];
        let layout = unsafe {
            vk.device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                None,
            )
        }
        .expect("descriptor layout");
        let mut rings: Vec<_> = (0..2)
            .map(|_| {
                CompositePoolRing::new(Arc::clone(&vk), INITIAL_DESCRIPTOR_SETS_PER_FRAME).unwrap()
            })
            .collect();

        for cycle in 0..4 {
            let mut live_sets = HashSet::new();
            for (output, ring) in rings.iter_mut().enumerate() {
                for expected_slot in 0..RING_LEN {
                    let slot = ring.acquire(4225).unwrap().expect("unused slot");
                    assert_eq!(slot, expected_slot);
                    assert!(ring.capacities[slot] >= 4225);
                    let layouts = [layout];
                    let allocation = vk::DescriptorSetAllocateInfo::default()
                        .descriptor_pool(ring.pool_at(slot))
                        .set_layouts(&layouts);
                    let mut allocated = 0;
                    let mut failure = None;
                    for _ in 0..(65 * 65) {
                        match unsafe { vk.device.allocate_descriptor_sets(&allocation) } {
                            Ok(sets) => {
                                assert!(live_sets.insert(sets[0]), "live set handle reused");
                                allocated += 1;
                            }
                            Err(error) => {
                                assert!(matches!(
                                    error,
                                    vk::Result::ERROR_OUT_OF_POOL_MEMORY
                                        | vk::Result::ERROR_FRAGMENTED_POOL
                                ));
                                failure = Some(error);
                                break;
                            }
                        }
                    }
                    // Vulkan permits overallocation on some implementations;
                    // the configured capacity is guaranteed, not a mandated
                    // failure index. Report the observed index instead.
                    assert!(allocated >= INITIAL_DESCRIPTOR_SETS_PER_FRAME);
                    assert_eq!(ring.tracker.slots_in_use(), slot + 1);
                    eprintln!(
                        "PR112 cycle={cycle} output={output} slot={slot} requested=4225 \
                         allocated={allocated} failure={failure:?}"
                    );
                }
                assert_eq!(
                    ring.acquire(4225).unwrap(),
                    None,
                    "pressure must not recycle a held slot"
                );
            }
            // No work was submitted; releasing every held slot is safe.
            for ring in &mut rings {
                for slot in 0..RING_LEN {
                    ring.release(slot);
                }
            }
        }
        drop(rings);
        unsafe { vk.device.destroy_descriptor_set_layout(layout, None) };
    }

    #[test]
    #[ignore = "requires an explicitly selected CPU Vulkan ICD"]
    fn software_growth_failure_preserves_held_pools_and_retry_capacity() {
        let vk = VkContext::new().expect("CPU Vulkan required");
        let properties = unsafe {
            vk.instance
                .get_physical_device_properties(vk.physical_device)
        };
        assert_eq!(properties.device_type, vk::PhysicalDeviceType::CPU);
        let mut ring = CompositePoolRing::new(Arc::clone(&vk), 1024).unwrap();
        let first = ring.acquire(1024).unwrap().unwrap();
        let second = ring.acquire(4225).unwrap().unwrap();
        let before = ring.pools;
        let capacities = ring.capacities;
        assert_eq!(
            ring.acquire_with(8193, |capacity| {
                assert_eq!(capacity, 16384);
                Err(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY)
            }),
            Err(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY)
        );
        assert_eq!(ring.pools, before);
        assert_eq!(ring.capacities, capacities);
        assert_eq!(ring.tracker.slots_in_use(), 2);
        let third = ring
            .acquire_with(1024, |_| panic!("old pool is still usable"))
            .unwrap()
            .unwrap();
        assert_eq!(
            ring.acquire_with(65536, |_| panic!("must not grow a held pool"))
                .unwrap(),
            None
        );
        ring.release(second);
        let again = ring
            .acquire_with(4225, |_| panic!("reuse enlarged pool"))
            .unwrap()
            .unwrap();
        assert_eq!(again, second);
        assert_eq!(ring.pools, before);
        ring.release(again);
        ring.release(first);
        ring.release(third);
        if let Some(overflow) = (u32::MAX as usize).checked_add(1) {
            assert_eq!(
                ring.acquire(overflow),
                Err(vk::Result::ERROR_OUT_OF_POOL_MEMORY)
            );
            assert_eq!(ring.tracker.slots_in_use(), 0);
        }
        let empty = ring.acquire(0).unwrap().unwrap();
        ring.release(empty);
    }

    #[test]
    #[ignore = "requires an explicitly selected CPU Vulkan ICD"]
    fn software_failed_reset_replaces_pool_before_reuse() {
        unsafe extern "system" fn fail_reset(
            _: vk::Device,
            _: vk::DescriptorPool,
            _: vk::DescriptorPoolResetFlags,
        ) -> vk::Result {
            vk::Result::ERROR_OUT_OF_DEVICE_MEMORY
        }

        let mut vk = VkContext::new().expect("CPU Vulkan required");
        let properties = unsafe {
            vk.instance
                .get_physical_device_properties(vk.physical_device)
        };
        assert_eq!(properties.device_type, vk::PhysicalDeviceType::CPU);
        let context = Arc::get_mut(&mut vk).unwrap();
        let mut functions = context.device.fp_v1_0().clone();
        functions.reset_descriptor_pool = fail_reset;
        context.device = ash::Device::from_parts_1_3(
            context.device.handle(),
            functions,
            context.device.fp_v1_1().clone(),
            context.device.fp_v1_2().clone(),
            context.device.fp_v1_3().clone(),
        );
        let mut ring = CompositePoolRing::new(vk, 1024).unwrap();
        let slot = ring.acquire(1).unwrap().unwrap();
        let old = ring.pool_at(slot);
        ring.release(slot);
        assert_eq!(ring.capacities[slot], 0);
        let reused = ring.acquire(1).unwrap().unwrap();
        assert_eq!(reused, slot);
        assert_ne!(
            ring.pool_at(slot),
            old,
            "replacement is created before destroying old pool"
        );
        assert_eq!(ring.capacities[slot], 1);
        ring.release(slot);
    }

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
