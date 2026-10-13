//! Long-lived descriptor pool ring for the v2 RENDER paint path.
//!
//! Stage 5 Task 4 layer 1; spec at
//! `docs/superpowers/specs/2026-05-21-descriptor-pool-ring-design.md`.
//!
//! Replaces the per-RENDER-call `BatchDescriptorArena` instantiation
//! (which created + destroyed a `vk::DescriptorPool` per op) with a
//! cycle of Free/Active/InFlight slots keyed by an engine-supplied
//! `acquire_generation` watermark. Pools are reset on retirement, not
//! destroyed — the steady-state `vkCreateDescriptorPool/s` should
//! drop from thousands to near-zero after warm-up.

#![allow(dead_code)] // Skeleton; fields + constants wired in Tasks 3-5.

use std::sync::Arc;

use ash::vk;

use crate::kms::vk::device::VkContext;

const SETS_PER_POOL: u32 = 256;
const SAMPLERS_PER_POOL: u32 = 1024;
const UNIFORMS_PER_POOL: u32 = 256;
const STORAGE_PER_POOL: u32 = 64;

#[derive(Debug, PartialEq, Eq)]
enum PoolState {
    Free,
    Active,
    InFlight,
    Poisoned,
}

struct PoolSlot {
    pool: vk::DescriptorPool,
    state: PoolState,
    high_water_generation: u64,
    sets_remaining: u32,
}

pub(crate) struct DescriptorPoolRing {
    vk: Arc<VkContext>,
    pools: Vec<PoolSlot>,
    active: Option<usize>,
    lifetime_creates: u64,
    lifetime_resets: u64,
    #[cfg(test)]
    inject_next_allocate_error: Option<vk::Result>,
    #[cfg(test)]
    force_next_reset_failure: bool,
}

impl DescriptorPoolRing {
    pub(crate) fn new(vk: Arc<VkContext>) -> Self {
        Self {
            vk,
            pools: Vec::new(),
            active: None,
            lifetime_creates: 0,
            lifetime_resets: 0,
            #[cfg(test)]
            inject_next_allocate_error: None,
            #[cfg(test)]
            force_next_reset_failure: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn test_inject_next_allocate_error(&mut self, err: vk::Result) {
        self.inject_next_allocate_error = Some(err);
    }

    #[cfg(test)]
    pub(crate) fn test_force_next_reset_failure(&mut self) {
        self.force_next_reset_failure = true;
    }

    pub(crate) fn pool_count(&self) -> usize {
        self.pools.len()
    }

    /// (Free, Active, InFlight, Poisoned)
    pub(crate) fn state_counts(&self) -> (usize, usize, usize, usize) {
        let mut counts = (0, 0, 0, 0);
        for s in &self.pools {
            match s.state {
                PoolState::Free => counts.0 += 1,
                PoolState::Active => counts.1 += 1,
                PoolState::InFlight => counts.2 += 1,
                PoolState::Poisoned => counts.3 += 1,
            }
        }
        counts
    }

    pub(crate) fn lifetime_creates(&self) -> u64 {
        self.lifetime_creates
    }

    pub(crate) fn lifetime_resets(&self) -> u64 {
        self.lifetime_resets
    }

    /// Phase B.2 Task 3 test introspection: max `high_water_generation`
    /// across all currently-resident pools (Active or InFlight). Used
    /// by the Mechanism 2 watermark integration test to assert that
    /// every `acquire_set` during an open frame tags the active pool
    /// with the frame's captured `frame_generation`.
    ///
    /// Returns `0` if no pool has yet been acquired against.
    pub(crate) fn max_high_water_generation(&self) -> u64 {
        self.pools
            .iter()
            .map(|s| s.high_water_generation)
            .max()
            .unwrap_or(0)
    }

    /// Acquire one descriptor set with `layout`, tagging the issuing
    /// pool with `generation`. Spec § "Architecture" / § "Internals
    /// on acquire_set": (1) ensure an Active pool exists + has
    /// capacity, (2) `vkAllocateDescriptorSets`, (3) bump the slot's
    /// `high_water_generation` and decrement `sets_remaining`.
    ///
    /// Errors map directly from Vk: `ERROR_OUT_OF_HOST_MEMORY`,
    /// `ERROR_OUT_OF_DEVICE_MEMORY`, etc. — propagated to the engine
    /// which converts to `RenderError::Vk`.
    pub(crate) fn acquire_set(
        &mut self,
        layout: vk::DescriptorSetLayout,
        generation: u64,
    ) -> Result<vk::DescriptorSet, vk::Result> {
        if self.pools.iter().any(|s| s.state == PoolState::Poisoned) {
            return Err(vk::Result::ERROR_UNKNOWN);
        }
        self.ensure_active_with_capacity()?;
        match self.try_allocate_one(layout) {
            Ok(set) => {
                self.note_acquired(generation);
                Ok(set)
            }
            Err(vk::Result::ERROR_OUT_OF_POOL_MEMORY | vk::Result::ERROR_FRAGMENTED_POOL) => {
                if let Some(idx) = self.active {
                    self.pools[idx].sets_remaining = 0;
                }
                self.ensure_active_with_capacity()?;
                let set = self.try_allocate_one(layout)?;
                self.note_acquired(generation);
                Ok(set)
            }
            Err(e) => Err(e),
        }
    }

    fn try_allocate_one(
        &mut self,
        layout: vk::DescriptorSetLayout,
    ) -> Result<vk::DescriptorSet, vk::Result> {
        #[cfg(test)]
        if let Some(e) = self.inject_next_allocate_error.take() {
            return Err(e);
        }
        let idx = self.active.expect("ensure_active_with_capacity set active");
        let pool = self.pools[idx].pool;
        let layouts = [layout];
        let alloc_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(pool)
            .set_layouts(&layouts);
        let sets = unsafe { self.vk.device.allocate_descriptor_sets(&alloc_info)? };
        Ok(sets[0])
    }

    fn note_acquired(&mut self, generation: u64) {
        let idx = self.active.expect("acquire path established active");
        let slot = &mut self.pools[idx];
        slot.sets_remaining = slot.sets_remaining.saturating_sub(1);
        if slot.high_water_generation < generation {
            slot.high_water_generation = generation;
        }
    }

    /// Caller signals "all submissions up to and including generation
    /// `retired_watermark` have retired." InFlight slots whose
    /// `high_water_generation <= retired_watermark` move to Free via
    /// `vkResetDescriptorPool`. Active pool is untouched.
    ///
    /// Returns the count of pools reclaimed for telemetry. On
    /// `vkResetDescriptorPool` error the slot is moved to Poisoned;
    /// the count of resets returned reflects only the `Ok` resets.
    pub(crate) fn release_up_to(&mut self, retired_watermark: u64) -> usize {
        let mut reclaimed = 0usize;
        for i in 0..self.pools.len() {
            let candidate = {
                let slot = &self.pools[i];
                slot.state == PoolState::InFlight && slot.high_water_generation <= retired_watermark
            };
            if !candidate {
                continue;
            }
            let pool_handle = self.pools[i].pool;
            match self.perform_reset(pool_handle) {
                Ok(()) => {
                    let slot = &mut self.pools[i];
                    slot.state = PoolState::Free;
                    slot.sets_remaining = SETS_PER_POOL;
                    slot.high_water_generation = 0;
                    reclaimed += 1;
                }
                Err(e) => {
                    log::error!(
                        "DescriptorPoolRing: vkResetDescriptorPool failed on \
                         pool {pool_handle:?}: {e:?} — poisoning slot",
                    );
                    self.pools[i].state = PoolState::Poisoned;
                }
            }
        }
        self.lifetime_resets = self.lifetime_resets.saturating_add(reclaimed as u64);
        reclaimed
    }

    fn perform_reset(&mut self, pool: vk::DescriptorPool) -> Result<(), vk::Result> {
        #[cfg(test)]
        if self.force_next_reset_failure {
            self.force_next_reset_failure = false;
            return Err(vk::Result::ERROR_DEVICE_LOST);
        }
        unsafe {
            self.vk
                .device
                .reset_descriptor_pool(pool, vk::DescriptorPoolResetFlags::empty())
        }
    }

    /// Make sure there is an Active pool with at least one set
    /// remaining. Picks order:
    ///   1. current Active still has capacity → no-op.
    ///   2. Active is exhausted → rotate it to InFlight.
    ///   3. Pick the first Free pool → promote to Active.
    ///   4. Else create a new Active pool.
    fn ensure_active_with_capacity(&mut self) -> Result<(), vk::Result> {
        if let Some(idx) = self.active {
            if self.pools[idx].sets_remaining > 0 {
                return Ok(());
            }
            self.pools[idx].state = PoolState::InFlight;
            self.active = None;
        }
        for i in 0..self.pools.len() {
            if self.pools[i].state == PoolState::Free {
                self.pools[i].state = PoolState::Active;
                self.pools[i].sets_remaining = SETS_PER_POOL;
                self.pools[i].high_water_generation = 0;
                self.active = Some(i);
                return Ok(());
            }
        }
        self.create_pool_active()
    }

    fn create_pool_active(&mut self) -> Result<(), vk::Result> {
        let pool_sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(SAMPLERS_PER_POOL),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::UNIFORM_BUFFER)
                .descriptor_count(UNIFORMS_PER_POOL),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(STORAGE_PER_POOL),
        ];
        let info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(SETS_PER_POOL)
            .pool_sizes(&pool_sizes);
        let pool = unsafe { self.vk.device.create_descriptor_pool(&info, None)? };
        self.lifetime_creates += 1;
        self.pools.push(PoolSlot {
            pool,
            state: PoolState::Active,
            high_water_generation: 0,
            sets_remaining: SETS_PER_POOL,
        });
        self.active = Some(self.pools.len() - 1);
        Ok(())
    }
}

impl Drop for DescriptorPoolRing {
    fn drop(&mut self) {
        for slot in self.pools.drain(..) {
            unsafe {
                self.vk.device.destroy_descriptor_pool(slot.pool, None);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kms::vk::device::VkContext;

    fn vk_or_skip() -> Option<Arc<VkContext>> {
        match VkContext::new() {
            Ok(vk) => Some(vk),
            Err(e) => {
                eprintln!("skipping: no Vk: {e:?}");
                None
            }
        }
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn new_ring_is_empty() {
        let Some(vk) = vk_or_skip() else { return };
        let ring = DescriptorPoolRing::new(vk);
        assert_eq!(ring.pool_count(), 0);
        assert_eq!(ring.state_counts(), (0, 0, 0, 0));
        assert_eq!(ring.lifetime_creates(), 0);
        assert_eq!(ring.lifetime_resets(), 0);
        // Drop runs at end of scope; must not panic on empty ring.
    }

    fn make_layout(vk: &VkContext) -> vk::DescriptorSetLayout {
        let bindings = [vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)];
        let info = vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
        unsafe {
            vk.device
                .create_descriptor_set_layout(&info, None)
                .expect("layout")
        }
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn acquire_grows_when_no_free_pool() {
        let Some(vk) = vk_or_skip() else { return };
        let layout = make_layout(&vk);
        let mut ring = DescriptorPoolRing::new(Arc::clone(&vk));
        let set = ring.acquire_set(layout, 1).expect("acquire");
        assert_ne!(set, vk::DescriptorSet::null());
        assert_eq!(ring.pool_count(), 1);
        // (Free, Active, InFlight, Poisoned)
        assert_eq!(ring.state_counts(), (0, 1, 0, 0));
        assert_eq!(ring.lifetime_creates(), 1);
        unsafe { vk.device.destroy_descriptor_set_layout(layout, None) };
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn drop_destroys_all_pools() {
        let Some(vk) = vk_or_skip() else { return };
        let layout = make_layout(&vk);
        {
            let mut ring = DescriptorPoolRing::new(Arc::clone(&vk));
            let _ = ring.acquire_set(layout, 1).expect("acquire");
            // Drop the ring here. If destroy_descriptor_pool leaks,
            // the validation layer flags it on VkDevice destroy.
        }
        unsafe { vk.device.destroy_descriptor_set_layout(layout, None) };
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn acquire_fills_active_then_rotates() {
        let Some(vk) = vk_or_skip() else { return };
        let layout = make_layout(&vk);
        let mut ring = DescriptorPoolRing::new(Arc::clone(&vk));
        // SETS_PER_POOL = 256. Acquire 257 sets at the same gen.
        for i in 0..257u64 {
            ring.acquire_set(layout, 1).unwrap_or_else(|e| {
                panic!("acquire #{i} failed: {e:?}");
            });
        }
        assert_eq!(ring.pool_count(), 2);
        // First pool rotated to InFlight, second is Active.
        assert_eq!(ring.state_counts(), (0, 1, 1, 0));
        assert_eq!(ring.lifetime_creates(), 2);
        unsafe { vk.device.destroy_descriptor_set_layout(layout, None) };
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn release_moves_inflight_to_free() {
        let Some(vk) = vk_or_skip() else { return };
        let layout = make_layout(&vk);
        let mut ring = DescriptorPoolRing::new(Arc::clone(&vk));
        // Fill pool A at gen=5, force rotate, then acquire gen=6 from
        // a fresh pool B so A is InFlight with high_water=5.
        for _ in 0..256 {
            ring.acquire_set(layout, 5).unwrap();
        }
        ring.acquire_set(layout, 6).unwrap();
        assert_eq!(ring.state_counts(), (0, 1, 1, 0));
        // release_up_to(5) reclaims pool A.
        let n = ring.release_up_to(5);
        assert_eq!(n, 1);
        assert_eq!(ring.state_counts(), (1, 1, 0, 0));
        assert_eq!(ring.lifetime_resets(), 1);
        unsafe { vk.device.destroy_descriptor_set_layout(layout, None) };
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn release_below_watermark_is_noop() {
        let Some(vk) = vk_or_skip() else { return };
        let layout = make_layout(&vk);
        let mut ring = DescriptorPoolRing::new(Arc::clone(&vk));
        for _ in 0..256 {
            ring.acquire_set(layout, 10).unwrap();
        }
        ring.acquire_set(layout, 11).unwrap();
        // release_up_to(9) — strictly below pool A's high_water=10.
        let n = ring.release_up_to(9);
        assert_eq!(n, 0);
        assert_eq!(ring.state_counts(), (0, 1, 1, 0));
        unsafe { vk.device.destroy_descriptor_set_layout(layout, None) };
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn interleaved_generations_partial_release() {
        let Some(vk) = vk_or_skip() else { return };
        let layout = make_layout(&vk);
        let mut ring = DescriptorPoolRing::new(Arc::clone(&vk));
        // Pool A: gen=100, fill.
        for _ in 0..256 {
            ring.acquire_set(layout, 100).unwrap();
        }
        // Pool B: gen=101, partial fill (1 set).
        ring.acquire_set(layout, 101).unwrap();
        assert_eq!(ring.state_counts(), (0, 1, 1, 0));
        // Release watermark=100 → pool A frees, pool B stays Active.
        assert_eq!(ring.release_up_to(100), 1);
        assert_eq!(ring.state_counts(), (1, 1, 0, 0));

        // Roll the cycle one more time: rotate B by filling it.
        for _ in 0..255 {
            ring.acquire_set(layout, 102).unwrap();
        }
        // B now has high_water=102 and is exhausted; one more acquire
        // rotates B and reuses the Free A.
        ring.acquire_set(layout, 103).unwrap();
        assert_eq!(ring.pool_count(), 2);
        assert_eq!(ring.state_counts(), (0, 1, 1, 0));
        // Release 101 → not enough; B's high_water=102 > 101.
        assert_eq!(ring.release_up_to(101), 0);
        // Release 102 → B reclaims to Free.
        assert_eq!(ring.release_up_to(102), 1);
        assert_eq!(ring.state_counts(), (1, 1, 0, 0));
        assert_eq!(ring.lifetime_resets(), 2);

        unsafe { vk.device.destroy_descriptor_set_layout(layout, None) };
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn out_of_pool_memory_retry() {
        let Some(vk) = vk_or_skip() else { return };
        let layout = make_layout(&vk);
        let mut ring = DescriptorPoolRing::new(Arc::clone(&vk));
        ring.test_inject_next_allocate_error(vk::Result::ERROR_OUT_OF_POOL_MEMORY);
        let set = ring.acquire_set(layout, 1).expect("retry should succeed");
        assert_ne!(set, vk::DescriptorSet::null());
        // The first alloc consumed an Active pool then errored; the
        // retry rotates and creates pool 2.
        assert_eq!(ring.pool_count(), 2);
        assert_eq!(ring.state_counts(), (0, 1, 1, 0));
        unsafe { vk.device.destroy_descriptor_set_layout(layout, None) };
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn fragmented_pool_retry() {
        let Some(vk) = vk_or_skip() else { return };
        let layout = make_layout(&vk);
        let mut ring = DescriptorPoolRing::new(Arc::clone(&vk));
        ring.test_inject_next_allocate_error(vk::Result::ERROR_FRAGMENTED_POOL);
        let set = ring.acquire_set(layout, 1).expect("retry should succeed");
        assert_ne!(set, vk::DescriptorSet::null());
        assert_eq!(ring.pool_count(), 2);
        unsafe { vk.device.destroy_descriptor_set_layout(layout, None) };
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn reset_failure_poisons_slot_and_drops_acquire() {
        let Some(vk) = vk_or_skip() else { return };
        let layout = make_layout(&vk);
        let mut ring = DescriptorPoolRing::new(Arc::clone(&vk));
        // Fill + rotate so pool A is InFlight at gen=1.
        for _ in 0..256 {
            ring.acquire_set(layout, 1).unwrap();
        }
        ring.acquire_set(layout, 2).unwrap();
        ring.test_force_next_reset_failure();
        let reclaimed = ring.release_up_to(1);
        // Slot poisoned, not reclaimed.
        assert_eq!(reclaimed, 0);
        let (free, active, in_flight, poisoned) = ring.state_counts();
        assert_eq!(poisoned, 1, "pool A must be poisoned");
        assert_eq!(free, 0);
        assert_eq!(active, 1, "pool B remains active");
        assert_eq!(in_flight, 0);
        // Next acquire short-circuits with ERROR_UNKNOWN.
        let err = ring.acquire_set(layout, 3).expect_err("poisoned ring");
        assert_eq!(err, vk::Result::ERROR_UNKNOWN);
        unsafe { vk.device.destroy_descriptor_set_layout(layout, None) };
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn pool_create_count_zero_after_warmup() {
        let Some(vk) = vk_or_skip() else { return };
        let layout = make_layout(&vk);
        let mut ring = DescriptorPoolRing::new(Arc::clone(&vk));
        const N: u64 = 5000;
        for gen_id in 1..=N {
            ring.acquire_set(layout, gen_id).expect("acquire");
            // Retire the gen we just acquired before the next iter,
            // mirroring the engine's "submit → retire" loop in the
            // single-in-flight steady state.
            ring.release_up_to(gen_id.saturating_sub(1));
        }
        // After warm-up: ≤ 2 pools resident. Lifetime creates should
        // be a small multiple of (N / SETS_PER_POOL = 19.5) but
        // bounded — anything ≤ 3 means the ring isn't growing on
        // every cycle.
        assert!(
            ring.pool_count() <= 2,
            "pool_count = {}, expected ≤ 2",
            ring.pool_count()
        );
        assert!(
            ring.lifetime_creates() <= 3,
            "lifetime_creates = {}; expected ≤ 3 after warm-up",
            ring.lifetime_creates()
        );
        assert!(
            ring.lifetime_resets() > 0,
            "lifetime_resets = 0; recycle path never ran"
        );
        unsafe { vk.device.destroy_descriptor_set_layout(layout, None) };
    }
}
