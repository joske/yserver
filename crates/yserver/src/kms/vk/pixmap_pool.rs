//! Backend-owned pool of recycled `VkImage` + `VkImageView` +
//! `VkDeviceMemory` triples for server-owned X pixmaps.
//!
//! Motivation: adapta-nokto theme apply + mate-cc launcher fire
//! hundreds of `CreatePixmap`/`FreePixmap` cycles per second for
//! 16×16 / 32×32 widget pixmaps; silence's dual-output MATE drag
//! pushes that to 6000–9000 oversize-reject pixmaps/sec dominated
//! by `<=256` icon-theme / Cairo intermediates (2026-05-26). The
//! kernel allocator (amdgpu / i915) serializes under that burst
//! rate. This pool recycles the Vulkan allocations so a fresh
//! `CreatePixmap` of a recently-freed `(extent, format)` hits the
//! pool instead of round-tripping the kernel.
//!
//! Keyed by `(width, height, format)`. `usage` is the constant
//! `COLOR_ATTACHMENT | TRANSFER_DST | TRANSFER_SRC | SAMPLED`
//! across all server-owned pixmaps, so it's not part of the key.
//!
//! Per-bucket cap (`PIXMAP_POOL_BUCKET_CAP`). Max pooled dimension
//! (`MAX_POOLED_DIM`) — pixmaps above this skip the pool (both on
//! return and on take) since they exhibit much lower reuse rates
//! and have quadratically larger backing memory.
//!
//! Lifetime: pool entries are returned via a `BatchResource`
//! adopted into the currently-open paint batch (Phase 5 T2
//! defer-release mechanism). When the batch retires, the
//! BatchResource's `release` returns the entry to the pool if the
//! bucket has room, else destroys it directly.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex, Weak},
    time::Instant,
};

use ash::vk;

use crate::kms::{render::batch_resource::BatchResource, vk::device::VkContext};

/// Per-bucket **memory** budget, from which the per-bucket entry cap
/// is derived by [`PixmapPool::bucket_cap`]. 8 MiB is exactly what the
/// previous flat 32-entry cap already permitted for the largest
/// poolable BGRA8 entry (32 × 256 × 256 × 4), so worst-case
/// per-bucket memory is unchanged; what changes is that *smaller*
/// entries now get proportionally deeper buckets instead of being
/// capped at a count that only ever made sense for 256×256.
///
/// Why this matters (measured, 2026-07-27, silence dual-output MATE
/// drag under adapta-nokto): the theme churns 230×51 / 230×57 /
/// 230×26 BGRA8 menu-row intermediates — 430,882 of 439,177
/// `CreatePixmap` in the matched Xorg xtrace. At 46,920 B/entry the
/// flat 32-cap held only 1.5 MB of a possible 8 MB, so ~1,800
/// returns/sec were rejected `bucket_full`, and that number showed up
/// 1:1 as ~1,800 `takes_miss`/sec — kernel `vkCreateImage` +
/// `vkAllocateMemory` round trips the pool exists to avoid, on the
/// hot path of a drag.
pub const PIXMAP_POOL_BUCKET_BUDGET_BYTES: u64 = 8 * 1024 * 1024;

/// Floor for the derived per-bucket cap, so an entry larger than the
/// whole budget still keeps a usable bucket instead of degenerating
/// to zero (which would disable pooling for that key entirely).
pub const PIXMAP_POOL_BUCKET_CAP_MIN: usize = 32;

/// Ceiling for the derived per-bucket cap. Tiny entries would other-
/// wise permit thousands per bucket; the real cost there is not
/// memory but live `VkImage`/`VkImageView` handle count and
/// `VecDeque` growth, which this bounds.
pub const PIXMAP_POOL_BUCKET_CAP_MAX: usize = 256;

/// Global entry ceiling across every `(width, height, format)` bucket.
///
/// The per-bucket budget alone is not a global bound: games create thousands
/// of distinct extents over a long session, and each new key can claim another
/// bucket. Issue #115 reached 5,421 pooled Vulkan images after one hour. A
/// 2,048-entry ceiling preserves the measured 178-entry MATE menu-row working
/// set while bounding image/view/allocation handle count across stale keys.
pub const PIXMAP_POOL_GLOBAL_ENTRY_CAP: usize = 2_048;

/// Pixmaps with `width > MAX_POOLED_DIM || height > MAX_POOLED_DIM`
/// skip the pool. Above this size reuse rates drop and per-entry
/// memory grows quadratically.
///
/// Set to 256 after silence dual-output telemetry (2026-05-26)
/// showed 99.3 % of oversize rejects landing in the `<=256` bin at
/// peak burst (8026/s out of 8080/s rejected). The previous 128
/// cap predated the silence workload; the new value captures the
/// real Cairo / GTK / icon-theme intermediates that churn under
/// MATE drag without ballooning memory into the >512 range where
/// reuse rates collapse and per-entry cost is 4 MB+.
pub const MAX_POOLED_DIM: u32 = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PixmapPoolKey {
    pub width: u32,
    pub height: u32,
    pub format: vk::Format,
}

/// One recycled pixmap-backing triple.
#[derive(Debug)]
pub struct PooledPixmapImage {
    pub image: vk::Image,
    pub view: vk::ImageView,
    pub memory: vk::DeviceMemory,
    pub current_layout: vk::ImageLayout,
}

/// Pool statistics for synthetic tests + telemetry. Reset on
/// backend shutdown.
///
/// `total_returns_rejected_oversize_by_bucket` partitions the
/// oversize-reject counter by `max(width, height)` into bins to
/// guide `MAX_POOLED_DIM` tuning: silence's dual-output workload
/// rejected 6-9K oversized returns/sec at peak with the 2026-05-26
/// capture, but the dominant size class was unknown without a
/// breakdown. Bin layout:
/// - `[0]` — `max_dim ≤ 256`
/// - `[1]` — `max_dim ≤ 512`
/// - `[2]` — `max_dim ≤ 1024`
/// - `[3]` — `max_dim > 1024`
///
/// Indices match `OVERSIZE_BIN_THRESHOLDS` below — the helper keeps
/// the print order stable and self-documenting.
#[derive(Debug, Default, Clone, Copy)]
pub struct PixmapPoolStats {
    pub total_takes_hit: u64,
    pub total_takes_miss: u64,
    pub total_returns_accepted: u64,
    pub total_returns_rejected_bucket_full: u64,
    pub total_returns_rejected_oversize: u64,
    pub total_returns_rejected_oversize_by_bucket: [u64; 4],
    pub total_global_evictions: u64,
    pub total_fresh_allocations: u64,
    pub total_fresh_allocation_bytes: u64,
    pub total_fresh_allocation_ns: u64,
    pub total_destroyed_images: u64,
    pub total_destroy_ns: u64,
    /// Live gauges, updated with every take/return/drain.
    pub live_entries: usize,
    pub live_buckets: usize,
    /// Extent × format bytes-per-pixel. Vulkan allocation padding is not
    /// included, so this is a stable nominal gauge rather than exact VRAM.
    pub live_nominal_bytes: u64,
}

/// Upper bound of each oversize-reject bin, indexed in lockstep
/// with `PixmapPoolStats::total_returns_rejected_oversize_by_bucket`.
/// The last entry (`u32::MAX`) is the "everything else" catch-all.
pub const OVERSIZE_BIN_THRESHOLDS: [u32; 4] = [256, 512, 1024, u32::MAX];

/// Bytes per pixel for the formats server-owned pixmaps are allocated
/// in (depth 1 → `R8_UNORM`, depth 24/32 → `B8G8R8A8_UNORM`). Only
/// used to size pool buckets, so an unrecognised format falls back to
/// 4 — the common case, and an over-estimate merely yields a shallower
/// bucket rather than an over-budget one.
#[must_use]
pub fn format_bytes_per_pixel(format: vk::Format) -> u32 {
    match format {
        vk::Format::R8_UNORM | vk::Format::R8_UINT | vk::Format::S8_UINT => 1,
        vk::Format::R8G8_UNORM | vk::Format::R16_UNORM | vk::Format::R5G6B5_UNORM_PACK16 => 2,
        vk::Format::R16G16B16A16_UNORM | vk::Format::R16G16B16A16_SFLOAT => 8,
        vk::Format::R32G32B32A32_SFLOAT | vk::Format::R32G32B32A32_UINT => 16,
        _ => 4,
    }
}

/// Map `max(width, height)` to its `OVERSIZE_BIN_THRESHOLDS` index.
#[must_use]
pub fn oversize_bin_index(max_dim: u32) -> usize {
    OVERSIZE_BIN_THRESHOLDS
        .iter()
        .position(|&threshold| max_dim <= threshold)
        .unwrap_or(OVERSIZE_BIN_THRESHOLDS.len() - 1)
}

/// Telemetry-side handle to the latest constructed pool. Set by
/// `PixmapPool::new`; read by the telemetry thread in
/// `yserver::run` to log per-second deltas. `Weak` so the pool can
/// still drop cleanly on backend teardown.
pub static GLOBAL_LATEST_POOL: Mutex<Weak<PixmapPool>> = Mutex::new(Weak::new());

/// Capture-the-most-recent-pool hook. Called by `PixmapPool::new`
/// via an `Arc::new_cyclic`-style indirection — but since the pool
/// is constructed via plain `Arc::new(PixmapPool::new(..))` we
/// expose a helper the construction site uses immediately after.
pub fn register_for_telemetry(pool: &Arc<PixmapPool>) {
    if let Ok(mut g) = GLOBAL_LATEST_POOL.lock() {
        *g = Arc::downgrade(pool);
    }
}

/// Telemetry-side snapshot accessor. Returns `None` if no pool has
/// been registered, or the registered pool has been dropped.
#[must_use]
pub fn telemetry_snapshot() -> Option<PixmapPoolStats> {
    let weak = GLOBAL_LATEST_POOL.lock().ok()?.clone();
    weak.upgrade().map(|p| p.stats())
}

pub struct PixmapPool {
    vk: Arc<VkContext>,
    // Mutex (not RefCell) so PooledPixmapReturn's Arc<PixmapPool>
    // satisfies BatchResource's Send bound. Single-threaded core
    // loop means contention is zero; Mutex is the cheapest Send-safe
    // option (one atomic CAS per pool op).
    buckets: Mutex<PixmapPoolBuckets>,
    stats: Mutex<PixmapPoolStats>,
}

#[derive(Default)]
struct PixmapPoolBuckets {
    by_key: HashMap<PixmapPoolKey, VecDeque<PooledPixmapImage>>,
    /// Last successful take/return generation for each live bucket. This map
    /// is bounded by `by_key` and lets global eviction retain the game's hot
    /// extent set instead of depending on arbitrary `HashMap` iteration.
    last_used: HashMap<PixmapPoolKey, u64>,
    use_generation: u64,
    entries: usize,
    nominal_bytes: u64,
}

impl PixmapPoolBuckets {
    fn touch(&mut self, key: PixmapPoolKey) {
        self.use_generation = self.use_generation.saturating_add(1);
        self.last_used.insert(key, self.use_generation);
    }

    /// Evict one entry from the least-recently-used bucket other than the
    /// incoming key. The scan is O(live buckets), but only runs after a cache
    /// miss at the global ceiling and its metadata is bounded by the entry
    /// cap. Keeping hot extent buckets resident avoids turning the hard cap
    /// into sustained Vulkan image-allocation churn.
    fn evict_one_for(&mut self, incoming_key: PixmapPoolKey) -> Option<PooledPixmapImage> {
        let evict_key = self
            .by_key
            .keys()
            .copied()
            .filter(|candidate| *candidate != incoming_key)
            .min_by_key(|candidate| self.last_used.get(candidate).copied().unwrap_or(0))
            .or_else(|| self.by_key.keys().next().copied())?;
        let old = self.by_key.get_mut(&evict_key)?.pop_front()?;
        self.entries -= 1;
        self.nominal_bytes = self
            .nominal_bytes
            .saturating_sub(PixmapPool::nominal_bytes(evict_key));
        if self.by_key.get(&evict_key).is_some_and(VecDeque::is_empty) {
            self.by_key.remove(&evict_key);
            self.last_used.remove(&evict_key);
        }
        Some(old)
    }
}

impl std::fmt::Debug for PixmapPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // VkContext does not implement Debug; show bucket count +
        // stats so logs are still useful without trying to print
        // raw Vulkan handles.
        let buckets_len = self
            .buckets
            .lock()
            .map(|b| b.by_key.len())
            .unwrap_or(usize::MAX);
        f.debug_struct("PixmapPool")
            .field("buckets", &buckets_len)
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

impl PixmapPool {
    pub fn new(vk: Arc<VkContext>) -> Self {
        Self {
            vk,
            buckets: Mutex::new(PixmapPoolBuckets::default()),
            stats: Mutex::new(PixmapPoolStats::default()),
        }
    }

    fn nominal_bytes(key: PixmapPoolKey) -> u64 {
        u64::from(key.width)
            .saturating_mul(u64::from(key.height))
            .saturating_mul(u64::from(format_bytes_per_pixel(key.format)))
    }

    fn publish_live_gauges(stats: &mut PixmapPoolStats, buckets: &PixmapPoolBuckets) {
        stats.live_entries = buckets.entries;
        stats.live_buckets = buckets.by_key.len();
        stats.live_nominal_bytes = buckets.nominal_bytes;
    }

    /// True if the pool would accept an entry for `key`. Used by
    /// callers to skip building a `PooledPixmapReturn` for sizes
    /// the pool won't accept anyway.
    #[must_use]
    pub fn eligible(key: PixmapPoolKey) -> bool {
        key.width <= MAX_POOLED_DIM && key.height <= MAX_POOLED_DIM
    }

    /// Per-bucket entry cap for `key`, derived from
    /// [`PIXMAP_POOL_BUCKET_BUDGET_BYTES`] so every bucket costs about
    /// the same *memory* rather than holding the same *count*. Clamped
    /// to [`PIXMAP_POOL_BUCKET_CAP_MIN`]..=[`PIXMAP_POOL_BUCKET_CAP_MAX`].
    ///
    /// Pure function of the key — the pool's sizing policy is unit
    /// tested without a Vulkan device.
    #[must_use]
    pub fn bucket_cap(key: PixmapPoolKey) -> usize {
        let entry_bytes = u64::from(key.width)
            .saturating_mul(u64::from(key.height))
            .saturating_mul(u64::from(format_bytes_per_pixel(key.format)))
            // A zero-extent key costs no memory; `max(1)` keeps the
            // division defined and lands it on the ceiling below.
            .max(1);
        let by_budget = PIXMAP_POOL_BUCKET_BUDGET_BYTES / entry_bytes;
        let by_budget = usize::try_from(by_budget).unwrap_or(PIXMAP_POOL_BUCKET_CAP_MAX);
        by_budget.clamp(PIXMAP_POOL_BUCKET_CAP_MIN, PIXMAP_POOL_BUCKET_CAP_MAX)
    }

    /// Take a recycled entry for `key`, or `None` if the bucket is
    /// empty.
    pub fn try_take(&self, key: PixmapPoolKey) -> Option<PooledPixmapImage> {
        if !Self::eligible(key) {
            return None;
        }
        let mut buckets = self
            .buckets
            .lock()
            .expect("pixmap pool buckets mutex poisoned");
        let mut stats = self.stats.lock().expect("pixmap pool stats mutex poisoned");
        let entry = buckets.by_key.get_mut(&key).and_then(VecDeque::pop_front);
        if entry.is_some() {
            buckets.entries -= 1;
            buckets.nominal_bytes = buckets
                .nominal_bytes
                .saturating_sub(Self::nominal_bytes(key));
            if buckets.by_key.get(&key).is_some_and(VecDeque::is_empty) {
                buckets.by_key.remove(&key);
                buckets.last_used.remove(&key);
            } else {
                buckets.touch(key);
            }
            stats.total_takes_hit += 1;
        } else {
            stats.total_takes_miss += 1;
        }
        Self::publish_live_gauges(&mut stats, &buckets);
        entry
    }

    /// Try to return `entry` to the pool. Returns `Ok(())` if
    /// accepted; `Err(entry)` if the bucket was full or the key is
    /// ineligible — caller must destroy the entry.
    pub fn try_return(
        &self,
        key: PixmapPoolKey,
        entry: PooledPixmapImage,
    ) -> Result<(), PooledPixmapImage> {
        if !Self::eligible(key) {
            let max_dim = key.width.max(key.height);
            let bin = oversize_bin_index(max_dim);
            let mut stats = self.stats.lock().expect("pixmap pool stats mutex poisoned");
            stats.total_returns_rejected_oversize += 1;
            stats.total_returns_rejected_oversize_by_bucket[bin] += 1;
            return Err(entry);
        }
        let mut buckets = self
            .buckets
            .lock()
            .expect("pixmap pool buckets mutex poisoned");
        let cap = Self::bucket_cap(key);
        if buckets
            .by_key
            .get(&key)
            .is_some_and(|bucket| bucket.len() >= cap)
        {
            self.stats
                .lock()
                .expect("pixmap pool stats mutex poisoned")
                .total_returns_rejected_bucket_full += 1;
            return Err(entry);
        }

        // A hit temporarily lowers `entries`, so returning the same working
        // image fits without eviction. Only a miss/new allocation at the
        // global ceiling displaces an older pooled entry, which lets the
        // bounded cache adapt as applications introduce new size keys.
        let mut evicted = Vec::new();
        while buckets.entries >= PIXMAP_POOL_GLOBAL_ENTRY_CAP {
            let Some(old) = buckets.evict_one_for(key) else {
                break;
            };
            evicted.push(old);
        }

        buckets.by_key.entry(key).or_default().push_back(entry);
        buckets.touch(key);
        buckets.entries += 1;
        buckets.nominal_bytes = buckets
            .nominal_bytes
            .saturating_add(Self::nominal_bytes(key));
        let mut stats = self.stats.lock().expect("pixmap pool stats mutex poisoned");
        stats.total_returns_accepted += 1;
        stats.total_global_evictions = stats
            .total_global_evictions
            .saturating_add(u64::try_from(evicted.len()).unwrap_or(u64::MAX));
        Self::publish_live_gauges(&mut stats, &buckets);
        drop(stats);
        drop(buckets);
        for old in evicted {
            self.destroy_entry(old);
        }
        Ok(())
    }

    /// Synchronously destroy every pooled entry. Called on backend
    /// shutdown after the scheduler has drained its in-flight
    /// batches (so no `BatchResource` can still hold a back-ref).
    pub fn drain(&self) {
        let mut buckets = self
            .buckets
            .lock()
            .expect("pixmap pool buckets mutex poisoned");
        for (_, bucket) in buckets.by_key.drain() {
            for entry in bucket {
                self.destroy_entry(entry);
            }
        }
        buckets.last_used.clear();
        buckets.use_generation = 0;
        buckets.entries = 0;
        buckets.nominal_bytes = 0;
        let mut stats = self.stats.lock().expect("pixmap pool stats mutex poisoned");
        Self::publish_live_gauges(&mut stats, &buckets);
    }

    fn destroy_entry(&self, entry: PooledPixmapImage) {
        let started = Instant::now();
        unsafe {
            self.vk.device.destroy_image_view(entry.view, None);
            self.vk.device.destroy_image(entry.image, None);
            self.vk.device.free_memory(entry.memory, None);
        }
        self.record_destroy(started.elapsed());
    }

    pub(crate) fn record_fresh_allocation(
        &self,
        allocation_bytes: u64,
        elapsed: std::time::Duration,
    ) {
        let mut stats = self.stats.lock().expect("pixmap pool stats mutex poisoned");
        stats.total_fresh_allocations = stats.total_fresh_allocations.saturating_add(1);
        stats.total_fresh_allocation_bytes = stats
            .total_fresh_allocation_bytes
            .saturating_add(allocation_bytes);
        stats.total_fresh_allocation_ns = stats
            .total_fresh_allocation_ns
            .saturating_add(u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX));
    }

    pub(crate) fn record_destroy(&self, elapsed: std::time::Duration) {
        let mut stats = self.stats.lock().expect("pixmap pool stats mutex poisoned");
        stats.total_destroyed_images = stats.total_destroyed_images.saturating_add(1);
        stats.total_destroy_ns = stats
            .total_destroy_ns
            .saturating_add(u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX));
    }

    #[must_use]
    pub fn stats(&self) -> PixmapPoolStats {
        *self.stats.lock().expect("pixmap pool stats mutex poisoned")
    }
}

impl Drop for PixmapPool {
    fn drop(&mut self) {
        // Defensive: callers should have called `drain()` after the
        // scheduler drained its in-flight batches. If we reach Drop
        // with entries remaining, destroy them — there's no race
        // (single-threaded core loop) and the VkContext is still
        // alive (Drop order: pixmap_pool before VkContext).
        unsafe {
            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
        }
        let entries: Vec<_> = self
            .buckets
            .lock()
            .expect("pixmap pool buckets mutex poisoned")
            .by_key
            .drain()
            .flat_map(|(_, bucket)| bucket.into_iter())
            .collect();
        for entry in entries {
            self.destroy_entry(entry);
        }
    }
}

/// `BatchResource` impl that releases by attempting to return the
/// pixmap-backing to a pool. Adopted into the open paint batch via
/// `RenderScheduler::defer_resource_release`.
#[derive(Debug)]
pub struct PooledPixmapReturn {
    pub pool: Arc<PixmapPool>,
    pub key: PixmapPoolKey,
    pub entry: Option<PooledPixmapImage>,
}

impl BatchResource for PooledPixmapReturn {
    fn release(mut self: Box<Self>, _vk: &VkContext) {
        let Some(entry) = self.entry.take() else {
            // Defensive: already released. Shouldn't happen but no UB.
            return;
        };
        if let Err(entry) = self.pool.try_return(self.key, entry) {
            self.pool.destroy_entry(entry);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // PixmapPool needs Arc<VkContext> to construct, which is not
    // unit-testable without a real Vulkan device. Pure-decision
    // logic (eligible, bucket-cap check, key hashing) is testable
    // standalone via these helpers.

    fn null_entry() -> PooledPixmapImage {
        PooledPixmapImage {
            image: vk::Image::null(),
            view: vk::ImageView::null(),
            memory: vk::DeviceMemory::null(),
            current_layout: vk::ImageLayout::UNDEFINED,
        }
    }

    #[test]
    fn global_eviction_prefers_a_stale_key_and_updates_population() {
        let incoming = PixmapPoolKey {
            width: 230,
            height: 51,
            format: vk::Format::B8G8R8A8_UNORM,
        };
        let stale = PixmapPoolKey {
            width: 17,
            height: 19,
            format: vk::Format::B8G8R8A8_UNORM,
        };
        let mut buckets = PixmapPoolBuckets::default();
        buckets
            .by_key
            .entry(incoming)
            .or_default()
            .push_back(null_entry());
        buckets
            .by_key
            .entry(stale)
            .or_default()
            .push_back(null_entry());
        buckets.touch(stale);
        buckets.touch(incoming);
        buckets.entries = 2;
        buckets.nominal_bytes =
            PixmapPool::nominal_bytes(incoming) + PixmapPool::nominal_bytes(stale);

        assert!(buckets.evict_one_for(incoming).is_some());
        assert_eq!(buckets.entries, 1);
        assert_eq!(buckets.by_key.len(), 1);
        assert!(buckets.by_key.contains_key(&incoming));
        assert!(!buckets.by_key.contains_key(&stale));
        assert_eq!(buckets.nominal_bytes, PixmapPool::nominal_bytes(incoming));
    }

    #[test]
    fn global_eviction_uses_bucket_lru_instead_of_hash_order() {
        let stale = PixmapPoolKey {
            width: 17,
            height: 19,
            format: vk::Format::B8G8R8A8_UNORM,
        };
        let hot = PixmapPoolKey {
            width: 230,
            height: 51,
            format: vk::Format::B8G8R8A8_UNORM,
        };
        let incoming = PixmapPoolKey {
            width: 64,
            height: 64,
            format: vk::Format::B8G8R8A8_UNORM,
        };
        let mut buckets = PixmapPoolBuckets::default();
        for key in [stale, hot, incoming] {
            buckets
                .by_key
                .entry(key)
                .or_default()
                .push_back(null_entry());
            buckets.entries += 1;
            buckets.nominal_bytes += PixmapPool::nominal_bytes(key);
        }
        buckets.touch(stale);
        buckets.touch(hot);
        buckets.touch(incoming);

        assert!(buckets.evict_one_for(incoming).is_some());
        assert!(!buckets.by_key.contains_key(&stale));
        assert!(buckets.by_key.contains_key(&hot));
        assert!(buckets.by_key.contains_key(&incoming));
        assert!(!buckets.last_used.contains_key(&stale));
    }

    #[test]
    fn global_cap_preserves_the_measured_single_bucket_working_set() {
        let menu_row = PixmapPoolKey {
            width: 230,
            height: 51,
            format: vk::Format::B8G8R8A8_UNORM,
        };
        assert!(PIXMAP_POOL_GLOBAL_ENTRY_CAP > PixmapPool::bucket_cap(menu_row));
    }

    #[test]
    fn eligible_under_max_dim() {
        assert!(PixmapPool::eligible(PixmapPoolKey {
            width: 32,
            height: 32,
            format: vk::Format::B8G8R8A8_UNORM,
        }));
        assert!(PixmapPool::eligible(PixmapPoolKey {
            width: MAX_POOLED_DIM,
            height: MAX_POOLED_DIM,
            format: vk::Format::R8_UNORM,
        }));
    }

    #[test]
    fn oversize_bin_index_maps_to_expected_bucket() {
        // Bins: [<=256, <=512, <=1024, >1024]
        assert_eq!(oversize_bin_index(129), 0);
        assert_eq!(oversize_bin_index(256), 0);
        assert_eq!(oversize_bin_index(257), 1);
        assert_eq!(oversize_bin_index(512), 1);
        assert_eq!(oversize_bin_index(513), 2);
        assert_eq!(oversize_bin_index(1024), 2);
        assert_eq!(oversize_bin_index(1025), 3);
        assert_eq!(oversize_bin_index(u32::MAX), 3);
    }

    #[test]
    fn ineligible_over_max_dim() {
        assert!(!PixmapPool::eligible(PixmapPoolKey {
            width: MAX_POOLED_DIM + 1,
            height: 32,
            format: vk::Format::B8G8R8A8_UNORM,
        }));
        assert!(!PixmapPool::eligible(PixmapPoolKey {
            width: 32,
            height: MAX_POOLED_DIM + 1,
            format: vk::Format::B8G8R8A8_UNORM,
        }));
    }

    /// The 2026-07-27 MATE/adapta-nokto drag capture is the sizing
    /// oracle here: the theme churns 230×51 / 230×57 / 230×26 BGRA8
    /// menu-row intermediates (430,882 of 439,177 CreatePixmap in the
    /// matched Xorg xtrace), and the flat 32-entry cap rejected
    /// ~1,800 returns/sec as bucket-full — which showed up 1:1 as
    /// ~1,800 takes_miss/sec, i.e. kernel image allocations the pool
    /// existed to avoid.
    #[test]
    fn bucket_cap_is_deep_for_the_measured_menu_row_size() {
        let key = PixmapPoolKey {
            width: 230,
            height: 51,
            format: vk::Format::B8G8R8A8_UNORM,
        };
        // 230*51*4 = 46_920 B/entry; 8 MiB / 46_920 = 178.
        assert_eq!(PixmapPool::bucket_cap(key), 178);
    }

    /// The budget is calibrated so the largest poolable BGRA8 entry
    /// keeps exactly the historical cap — the change must not grow
    /// worst-case per-bucket memory beyond what 32×256×256×4 already
    /// allowed.
    #[test]
    fn bucket_cap_at_max_pooled_dim_matches_legacy_flat_cap() {
        let key = PixmapPoolKey {
            width: MAX_POOLED_DIM,
            height: MAX_POOLED_DIM,
            format: vk::Format::B8G8R8A8_UNORM,
        };
        assert_eq!(PixmapPool::bucket_cap(key), 32);
        // And the budget it derives from is the same 8 MiB the flat
        // cap implied for this size.
        assert_eq!(
            PIXMAP_POOL_BUCKET_BUDGET_BYTES,
            32 * u64::from(MAX_POOLED_DIM) * u64::from(MAX_POOLED_DIM) * 4,
        );
    }

    #[test]
    fn bucket_cap_scales_inversely_with_bytes_per_entry() {
        let bgra = PixmapPoolKey {
            width: MAX_POOLED_DIM,
            height: MAX_POOLED_DIM,
            format: vk::Format::B8G8R8A8_UNORM,
        };
        let r8 = PixmapPoolKey {
            format: vk::Format::R8_UNORM,
            ..bgra
        };
        // Same extent, 1/4 the bytes per pixel → 4× the entries.
        assert_eq!(PixmapPool::bucket_cap(r8), 4 * PixmapPool::bucket_cap(bgra));
    }

    #[test]
    fn bucket_cap_clamps_tiny_entries_to_the_count_ceiling() {
        let key = PixmapPoolKey {
            width: 16,
            height: 16,
            format: vk::Format::B8G8R8A8_UNORM,
        };
        // 8 MiB / 1_024 B = 8_192 entries by budget; the count
        // ceiling bounds handle/VecDeque growth instead.
        assert_eq!(PixmapPool::bucket_cap(key), PIXMAP_POOL_BUCKET_CAP_MAX);
    }

    #[test]
    fn bucket_cap_never_drops_below_the_floor() {
        // A hypothetical entry far larger than the budget still keeps
        // a usable bucket rather than degenerating to zero.
        let key = PixmapPoolKey {
            width: MAX_POOLED_DIM,
            height: MAX_POOLED_DIM,
            format: vk::Format::R32G32B32A32_SFLOAT, // 16 B/px
        };
        assert_eq!(PixmapPool::bucket_cap(key), PIXMAP_POOL_BUCKET_CAP_MIN);
    }

    #[test]
    fn bucket_cap_handles_zero_extent_without_dividing_by_zero() {
        let key = PixmapPoolKey {
            width: 0,
            height: 0,
            format: vk::Format::B8G8R8A8_UNORM,
        };
        assert_eq!(PixmapPool::bucket_cap(key), PIXMAP_POOL_BUCKET_CAP_MAX);
    }

    #[test]
    fn key_hash_distinguishes_dims_and_formats() {
        use std::collections::HashMap;
        let mut m: HashMap<PixmapPoolKey, u32> = HashMap::new();
        m.insert(
            PixmapPoolKey {
                width: 16,
                height: 16,
                format: vk::Format::R8_UNORM,
            },
            1,
        );
        m.insert(
            PixmapPoolKey {
                width: 16,
                height: 16,
                format: vk::Format::B8G8R8A8_UNORM,
            },
            2,
        );
        m.insert(
            PixmapPoolKey {
                width: 32,
                height: 16,
                format: vk::Format::R8_UNORM,
            },
            3,
        );
        assert_eq!(m.len(), 3);
    }
}
