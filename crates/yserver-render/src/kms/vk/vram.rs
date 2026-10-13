//! Device-memory (VRAM) accounting for telemetry.
//!
//! Motivation: GH discussion 56 (2026-09-22) reported yserver holding
//! 3.5–4.0 GiB of VRAM against Xorg's 800–900 MiB on the same 4K
//! desktop, with a ~1.4 GiB floor remaining after every client had
//! exited. We could neither confirm nor attribute that, because the
//! server had no memory instrumentation at all — the only figures
//! available were a contributor's external `radeontop`/`xrestop`
//! readings.
//!
//! `VK_EXT_memory_budget` closes that gap from the inside. Its
//! `heapUsage` is specified as an estimate of how much memory **the
//! calling process** is using in each heap, so a reading is not
//! polluted by another compositor running on a different VT — which
//! is the normal way this gets tested here (GNOME/Wayland on the
//! main seat, yserver on a spare VT). `heapBudget` is the
//! complement: what this process may still allocate, which *does*
//! shrink as other processes consume the heap.
//!
//! The numbers are driver estimates, not an exact ledger, and they
//! include allocations the driver makes on our behalf. That is the
//! right denominator for comparing against an external per-process
//! reading, which also sees driver overhead. It is the wrong tool
//! for attributing bytes to one of our caches — that needs its own
//! counters at the allocation sites.
//!
//! Threading: the sampling call needs an `ash::Instance`, whose
//! lifetime is owned by `VkContext`. The telemetry thread is
//! detached and outlives shutdown, so handing it a bare handle
//! would let it call into a destroyed instance. Instead the handle
//! lives behind a mutex that `VkContext::drop` clears *before*
//! `destroy_instance`: a sample in flight holds the lock and
//! teardown waits for it, and every sample afterwards finds `None`.
//! See [`register`], [`deregister`] and [`sample`].

use std::{ffi::CStr, sync::Mutex};

use ash::vk;

/// The device extension a populated sample requires. Purely
/// diagnostic: when the device does not expose it, sampling is
/// skipped and telemetry omits the line rather than reporting zeros,
/// which would read as "we use no memory".
pub const REQUIRED_EXTENSION: &CStr = vk::EXT_MEMORY_BUDGET_NAME;

/// One reading of this process's device-memory use.
///
/// `device_local_*` sum every heap carrying
/// `MEMORY_HEAP_DEVICE_LOCAL`, which on a discrete GPU is VRAM
/// proper and on an integrated one is the shared pool. `other_usage`
/// carries the remaining heaps (host-visible system memory used for
/// staging), kept separate so an integrated part's shared heap does
/// not silently double-count.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct VramSample {
    /// Bytes this process is using across device-local heaps.
    pub device_local_usage: u64,
    /// Bytes this process may still allocate from device-local
    /// heaps, as reported by the driver. Shrinks when *other*
    /// processes consume the heap, so a small budget alongside a
    /// small usage means someone else is the tenant.
    pub device_local_budget: u64,
    /// Bytes this process is using across non-device-local heaps.
    pub other_usage: u64,
    /// Number of heaps that contributed to `device_local_*`.
    pub device_local_heaps: u32,
}

/// The handles a sample needs, published by `VkContext` for the
/// lifetime of the instance.
///
/// `enabled` records whether [`REQUIRED_EXTENSION`] made it onto the
/// device. Without it the budget struct comes back zeroed, which
/// would read as "using nothing" rather than "cannot tell" — so a
/// sample is refused instead.
struct Registered {
    instance: ash::Instance,
    physical_device: vk::PhysicalDevice,
    enabled: bool,
}

/// `None` before Vulkan init and after teardown. A `Mutex` rather
/// than a lock-free cell precisely because teardown must be able to
/// *wait* for an in-flight sample.
static REGISTERED: Mutex<Option<Registered>> = Mutex::new(None);

/// Publish the instance for sampling. Called once, at the end of
/// `VkContext` construction.
///
/// `enabled` is whether the device enabled [`REQUIRED_EXTENSION`].
pub fn register(instance: &ash::Instance, physical_device: vk::PhysicalDevice, enabled: bool) {
    if let Ok(mut g) = REGISTERED.lock() {
        *g = Some(Registered {
            instance: instance.clone(),
            physical_device,
            enabled,
        });
    }
}

/// Withdraw the instance. MUST be called before `destroy_instance`,
/// otherwise a concurrent [`sample`] can call into a destroyed
/// instance.
pub fn deregister() {
    if let Ok(mut g) = REGISTERED.lock() {
        *g = None;
    }
}

/// Query the driver for this process's current device-memory use.
///
/// Returns `None` when Vulkan is not up, has been torn down, or the
/// device lacks [`REQUIRED_EXTENSION`]. Callers should omit their
/// output entirely in that case rather than print zeros.
///
/// Cheap enough to call from the once-a-second telemetry thread: it
/// is a single instance-level query with no synchronisation beyond
/// this module's mutex.
#[must_use]
pub fn sample() -> Option<VramSample> {
    let guard = REGISTERED.lock().ok()?;
    let reg = guard.as_ref()?;
    if !reg.enabled {
        return None;
    }
    let mut budget = vk::PhysicalDeviceMemoryBudgetPropertiesEXT::default();
    let mut props = vk::PhysicalDeviceMemoryProperties2::default().push_next(&mut budget);
    // SAFETY: `get_physical_device_memory_properties2` is core in
    // Vulkan 1.1 and takes no externally-synchronised handle. The
    // instance is live for the duration of this call because
    // `deregister` — which `VkContext::drop` runs before
    // `destroy_instance` — needs the lock we are holding.
    unsafe {
        reg.instance
            .get_physical_device_memory_properties2(reg.physical_device, &mut props);
    }
    Some(summarise(&props.memory_properties, &budget))
}

/// Fold the per-heap arrays into the reported totals.
///
/// Split out from [`sample`] so the partitioning is testable without
/// a device: a wrong `DEVICE_LOCAL` test would misreport an
/// integrated GPU's single shared heap as "no VRAM in use", which is
/// exactly the failure that would make this instrument useless on
/// the Intel boxes.
fn summarise(
    props: &vk::PhysicalDeviceMemoryProperties,
    budget: &vk::PhysicalDeviceMemoryBudgetPropertiesEXT<'_>,
) -> VramSample {
    let mut out = VramSample::default();
    // `memory_heap_count` bounds the meaningful prefix of both
    // arrays; the tails are unspecified, so folding the whole
    // MAX_MEMORY_HEAPS array would add garbage.
    let n = props.memory_heap_count as usize;
    for i in 0..n.min(vk::MAX_MEMORY_HEAPS) {
        if props.memory_heaps[i]
            .flags
            .contains(vk::MemoryHeapFlags::DEVICE_LOCAL)
        {
            out.device_local_usage = out.device_local_usage.saturating_add(budget.heap_usage[i]);
            out.device_local_budget = out
                .device_local_budget
                .saturating_add(budget.heap_budget[i]);
            out.device_local_heaps += 1;
        } else {
            out.other_usage = out.other_usage.saturating_add(budget.heap_usage[i]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a properties struct in one expression — clippy's
    /// `field_reassign_with_default` forbids the mutate-after-default
    /// shape, and CI runs it as `-D warnings`.
    ///
    /// `count` is passed separately from `heaps` so a test can
    /// populate array slots *past* `memory_heap_count`, which is what
    /// the unspecified-tail case needs.
    fn props_with(count: u32, heaps: &[(u64, bool)]) -> vk::PhysicalDeviceMemoryProperties {
        let mut memory_heaps = [vk::MemoryHeap::default(); vk::MAX_MEMORY_HEAPS];
        for (slot, &(size, device_local)) in memory_heaps.iter_mut().zip(heaps) {
            *slot = vk::MemoryHeap {
                size,
                flags: if device_local {
                    vk::MemoryHeapFlags::DEVICE_LOCAL
                } else {
                    vk::MemoryHeapFlags::empty()
                },
            };
        }
        vk::PhysicalDeviceMemoryProperties {
            memory_heap_count: count,
            memory_heaps,
            ..Default::default()
        }
    }

    /// `(usage, budget)` per heap, in heap order.
    fn budget_with(
        per_heap: &[(u64, u64)],
    ) -> vk::PhysicalDeviceMemoryBudgetPropertiesEXT<'static> {
        let mut heap_usage = [0u64; vk::MAX_MEMORY_HEAPS];
        let mut heap_budget = [0u64; vk::MAX_MEMORY_HEAPS];
        for (i, &(usage, budget)) in per_heap.iter().enumerate() {
            heap_usage[i] = usage;
            heap_budget[i] = budget;
        }
        vk::PhysicalDeviceMemoryBudgetPropertiesEXT {
            heap_usage,
            heap_budget,
            ..Default::default()
        }
    }

    /// A discrete part: one VRAM heap plus a host-visible system
    /// heap. The system heap must not land in the VRAM total.
    #[test]
    fn discrete_gpu_splits_vram_from_host_memory() {
        let props = props_with(2, &[(8 << 30, true), (16 << 30, false)]);
        let budget = budget_with(&[(1400 << 20, 7 << 30), (64 << 20, 0)]);

        let s = summarise(&props, &budget);
        assert_eq!(s.device_local_usage, 1400 << 20);
        assert_eq!(s.device_local_budget, 7 << 30);
        assert_eq!(s.other_usage, 64 << 20);
        assert_eq!(s.device_local_heaps, 1);
    }

    /// An integrated part reports one shared DEVICE_LOCAL heap. It
    /// must be counted as device-local, not dropped — otherwise the
    /// instrument reads zero on every Intel/AMD APU box, which is
    /// where we most need it.
    #[test]
    fn integrated_gpu_single_shared_heap_counts_as_device_local() {
        let props = props_with(1, &[(20 << 30, true)]);
        let budget = budget_with(&[(512 << 20, 18 << 30)]);

        let s = summarise(&props, &budget);
        assert_eq!(s.device_local_usage, 512 << 20);
        assert_eq!(s.other_usage, 0);
        assert_eq!(s.device_local_heaps, 1);
    }

    /// Heaps past `memory_heap_count` are unspecified. Folding them
    /// would inflate the total with whatever the driver left in the
    /// array tail.
    #[test]
    fn heaps_past_the_count_are_ignored() {
        let props = props_with(1, &[(8 << 30, true), (99 << 30, true)]);
        let budget = budget_with(&[(100, 0), (999_999, 0)]);

        let s = summarise(&props, &budget);
        assert_eq!(s.device_local_usage, 100);
        assert_eq!(s.device_local_heaps, 1);
    }

    /// Several device-local heaps sum rather than last-one-wins.
    #[test]
    fn multiple_device_local_heaps_sum() {
        let props = props_with(3, &[(4 << 30, true), (4 << 30, true), (16 << 30, false)]);
        let budget = budget_with(&[(100, 1000), (200, 2000), (9, 0)]);

        let s = summarise(&props, &budget);
        assert_eq!(s.device_local_usage, 300);
        assert_eq!(s.device_local_budget, 3000);
        assert_eq!(s.other_usage, 9);
        assert_eq!(s.device_local_heaps, 2);
    }

    /// Sampling with nothing registered must say "cannot tell"
    /// rather than report zero usage.
    #[test]
    fn sampling_without_a_registered_instance_is_refused() {
        deregister();
        assert!(sample().is_none());
    }
}
