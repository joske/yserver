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
//! Bounds across buckets (#196): a global byte budget and entry cap,
//! enforced by evicting least-recently-returned entries down to a
//! low-water mark, and an idle-age trim driven from the backend's
//! `before_block`. Takes are LIFO, so a bucket's working set keeps
//! fresh return stamps and only the tail a burst left behind ages out.
//!
//! Lifetime: pool entries are returned via a `BatchResource`
//! adopted into the currently-open paint batch (Phase 5 T2
//! defer-release mechanism). When the batch retires, the
//! BatchResource's `release` returns the entry to the pool if the
//! bucket has room, else destroys it directly.

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
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

/// Global budget on idle entries, in real allocation bytes (#196).
/// 64 MiB is ~18× the reporter's 3-day pre-burst residency (≤3.5 MiB)
/// and ~2× the measured drag working set (three full 8 MiB menu-row
/// buckets, ~33 MiB at the observed 1.37× allocation overhead); the
/// #169 burst parked 970 MiB.
pub const PIXMAP_POOL_BUDGET_BYTES: u64 = 64 * 1024 * 1024;

/// Over-budget eviction stops here, so a pool sitting at the budget
/// edge evicts in batches instead of once per return.
pub const PIXMAP_POOL_LOW_WATER_BYTES: u64 = 48 * 1024 * 1024;

/// Global cap on idle entries: ~10× the reporter's steady state (389).
/// Bounds live BO / VA-range count where tiny entries fit the byte
/// budget thousands at a time (each makes amdgpu allocation dearer).
pub const PIXMAP_POOL_MAX_ENTRIES: usize = 4096;

/// Entry-count counterpart to [`PIXMAP_POOL_LOW_WATER_BYTES`].
pub const PIXMAP_POOL_LOW_WATER_ENTRIES: usize = 3072;

/// An entry not taken this long after its return is destroyed. The
/// churn the pool exists for reuses within milliseconds (thousands of
/// cycles/s); a minute spares anything redrawn even occasionally.
pub const PIXMAP_POOL_IDLE_EVICT_AFTER: Duration = Duration::from_secs(60);

/// Slack on the idle-trim wakeup, so a pool draining entries returned
/// over a long stretch wakes the loop at most ~once a second.
pub const PIXMAP_POOL_TRIM_SLACK: Duration = Duration::from_secs(1);

/// The pool's cross-bucket bounds. [`Default`] is the production
/// policy; tests construct tighter ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixmapPoolLimits {
    pub budget_bytes: u64,
    pub low_water_bytes: u64,
    pub max_entries: usize,
    pub low_water_entries: usize,
    pub idle_evict_after: Duration,
}

impl Default for PixmapPoolLimits {
    fn default() -> Self {
        Self {
            budget_bytes: PIXMAP_POOL_BUDGET_BYTES,
            low_water_bytes: PIXMAP_POOL_LOW_WATER_BYTES,
            max_entries: PIXMAP_POOL_MAX_ENTRIES,
            low_water_entries: PIXMAP_POOL_LOW_WATER_ENTRIES,
            idle_evict_after: PIXMAP_POOL_IDLE_EVICT_AFTER,
        }
    }
}

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
/// Live occupancy of the pool. See [`PixmapPool::residency`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PixmapPoolResidency {
    /// Distinct `(w, h, format)` buckets holding entries — a bucket is
    /// removed when its last entry leaves.
    pub buckets: u64,
    /// Entries held across all buckets.
    pub entries: u64,
    /// Allocation bytes held, as budgeted (`mem_reqs.size` per entry).
    pub bytes: u64,
    /// `w * h * bytes_per_pixel` over live entries: see [`PixmapPool::residency`].
    pub nominal_bytes: u64,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct PixmapPoolStats {
    pub total_takes_hit: u64,
    pub total_takes_miss: u64,
    pub total_returns_accepted: u64,
    pub total_returns_rejected_bucket_full: u64,
    pub total_returns_rejected_oversize: u64,
    pub total_returns_rejected_oversize_by_bucket: [u64; 4],
    /// Entries destroyed to bring the pool back under its budget.
    pub total_evicted_budget: u64,
    /// Entries destroyed by the idle-age trim.
    pub total_evicted_idle: u64,
}

impl PixmapPoolStats {
    /// The counters `vram churn` reports: a hit or an accepted return is
    /// a `vkAllocateMemory` / `vkFreeMemory` the pool saved; a rejected
    /// return (bucket full, or larger than `MAX_POOLED_DIM`) or an
    /// eviction is a free.
    #[must_use]
    pub fn pool_counters(&self) -> crate::kms::vk::mem_accounting::PoolCounters {
        crate::kms::vk::mem_accounting::PoolCounters {
            hits: self.total_takes_hit,
            misses: self.total_takes_miss,
            kept: self.total_returns_accepted,
            dropped: self.total_returns_rejected_bucket_full
                + self.total_returns_rejected_oversize
                + self.total_evicted_budget
                + self.total_evicted_idle,
        }
    }
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

/// Live-occupancy counterpart to [`telemetry_snapshot`].
///
/// Reported separately from the cumulative stats because residency
/// is the question the stats cannot answer: inferring it from the
/// counters is a difference of large numbers rather than a
/// measurement.
#[must_use]
pub fn residency_snapshot() -> Option<PixmapPoolResidency> {
    let g = GLOBAL_LATEST_POOL.lock().ok()?;
    Some(g.upgrade()?.residency())
}

/// One idle entry plus what the cross-bucket bounds need to know
/// about it.
#[derive(Debug)]
struct Slot<E> {
    entry: E,
    bytes: u64,
    seq: u64,
}

/// The pool's bookkeeping, generic over the entry so the eviction
/// policy is unit tested without a Vulkan device. Never destroys
/// anything itself: entries leaving by eviction are handed back for
/// the caller to destroy outside the lock.
///
/// Within a bucket, slots sit in return order (`seq` ascending), so a
/// bucket's front is its least recently returned slot and `lru`'s
/// first key always names some bucket's front.
#[derive(Debug)]
struct PoolCore<E> {
    limits: PixmapPoolLimits,
    buckets: HashMap<PixmapPoolKey, VecDeque<Slot<E>>>,
    /// Every held slot by return sequence → its bucket and return time.
    lru: BTreeMap<u64, (PixmapPoolKey, Instant)>,
    next_seq: u64,
    bytes: u64,
}

/// Why [`PoolCore::put`] declined an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reject {
    Oversize,
    BucketFull,
}

impl<E> PoolCore<E> {
    fn new(limits: PixmapPoolLimits) -> Self {
        Self {
            limits,
            buckets: HashMap::new(),
            lru: BTreeMap::new(),
            next_seq: 0,
            bytes: 0,
        }
    }

    fn entries(&self) -> usize {
        self.lru.len()
    }

    /// Most recently returned entry for `key`: LIFO keeps a bucket's
    /// working set fresh, so only what it did not reuse ages out.
    fn take(&mut self, key: PixmapPoolKey) -> Option<E> {
        let bucket = self.buckets.get_mut(&key)?;
        let slot = bucket.pop_back()?;
        if bucket.is_empty() {
            self.buckets.remove(&key);
        }
        self.lru.remove(&slot.seq);
        self.bytes -= slot.bytes;
        Some(slot.entry)
    }

    /// Accept `entry` (costing `bytes`) at `now`. On acceptance, returns
    /// whatever had to be evicted to get back under the limits.
    fn put(
        &mut self,
        key: PixmapPoolKey,
        entry: E,
        bytes: u64,
        now: Instant,
    ) -> Result<Vec<E>, (E, Reject)> {
        if !PixmapPool::eligible(key) {
            return Err((entry, Reject::Oversize));
        }
        let bucket = self.buckets.entry(key).or_default();
        if bucket.len() >= PixmapPool::bucket_cap(key) {
            return Err((entry, Reject::BucketFull));
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        bucket.push_back(Slot { entry, bytes, seq });
        self.lru.insert(seq, (key, now));
        self.bytes += bytes;
        let mut evicted = Vec::new();
        if self.bytes > self.limits.budget_bytes || self.entries() > self.limits.max_entries {
            // Down to the low-water mark, not just under the limit: at
            // the edge a per-return eviction would turn every return
            // into a free.
            while self.bytes > self.limits.low_water_bytes
                || self.entries() > self.limits.low_water_entries
            {
                let Some(e) = self.evict_oldest() else { break };
                evicted.push(e);
            }
        }
        Ok(evicted)
    }

    fn evict_oldest(&mut self) -> Option<E> {
        let (seq, (key, _)) = self.lru.pop_first()?;
        let bucket = self.buckets.get_mut(&key)?;
        let slot = bucket.pop_front()?;
        debug_assert_eq!(slot.seq, seq, "bucket front is its oldest slot");
        if bucket.is_empty() {
            self.buckets.remove(&key);
        }
        self.bytes -= slot.bytes;
        Some(slot.entry)
    }

    /// Evict every entry returned `idle_evict_after` or longer before
    /// `now` — by construction none of them was taken in that time.
    fn trim_idle(&mut self, now: Instant) -> Vec<E> {
        let mut evicted = Vec::new();
        while self.lru.first_key_value().is_some_and(|(_, (_, at))| {
            now.saturating_duration_since(*at) >= self.limits.idle_evict_after
        }) {
            let Some(e) = self.evict_oldest() else { break };
            evicted.push(e);
        }
        evicted
    }

    /// When the oldest entry becomes trimmable, plus
    /// [`PIXMAP_POOL_TRIM_SLACK`]. `None` while empty.
    fn next_trim_deadline(&self) -> Option<Instant> {
        self.lru
            .first_key_value()
            .map(|(_, (_, at))| *at + self.limits.idle_evict_after + PIXMAP_POOL_TRIM_SLACK)
    }

    fn drain(&mut self) -> Vec<E> {
        self.lru.clear();
        self.bytes = 0;
        self.buckets
            .drain()
            .flat_map(|(_, bucket)| bucket.into_iter().map(|slot| slot.entry))
            .collect()
    }

    fn residency(&self) -> PixmapPoolResidency {
        let mut out = PixmapPoolResidency {
            buckets: self.buckets.len() as u64,
            entries: self.entries() as u64,
            bytes: self.bytes,
            ..Default::default()
        };
        for (key, bucket) in &self.buckets {
            let entry_bytes = u64::from(key.width)
                .saturating_mul(u64::from(key.height))
                .saturating_mul(u64::from(format_bytes_per_pixel(key.format)));
            out.nominal_bytes = out
                .nominal_bytes
                .saturating_add(entry_bytes * bucket.len() as u64);
        }
        out
    }
}

pub struct PixmapPool {
    vk: Arc<VkContext>,
    // Mutex (not RefCell) so PooledPixmapReturn's Arc<PixmapPool>
    // satisfies BatchResource's Send bound. Single-threaded core
    // loop means contention is zero; Mutex is the cheapest Send-safe
    // option (one atomic CAS per pool op).
    core: Mutex<PoolCore<PooledPixmapImage>>,
    stats: Mutex<PixmapPoolStats>,
}

impl std::fmt::Debug for PixmapPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // VkContext does not implement Debug; show bucket count +
        // stats so logs are still useful without trying to print
        // raw Vulkan handles.
        let buckets_len = self
            .core
            .lock()
            .map(|c| c.buckets.len())
            .unwrap_or(usize::MAX);
        f.debug_struct("PixmapPool")
            .field("buckets", &buckets_len)
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

impl PixmapPool {
    pub fn new(vk: Arc<VkContext>) -> Self {
        Self::with_limits(vk, PixmapPoolLimits::default())
    }

    /// [`Self::new`] with explicit cross-bucket bounds.
    pub fn with_limits(vk: Arc<VkContext>, limits: PixmapPoolLimits) -> Self {
        Self {
            vk,
            core: Mutex::new(PoolCore::new(limits)),
            stats: Mutex::new(PixmapPoolStats::default()),
        }
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
        let entry_bytes = Self::nominal_bytes(key)
            // A zero-extent key costs no memory; `max(1)` keeps the
            // division defined and lands it on the ceiling below.
            .max(1);
        let by_budget = PIXMAP_POOL_BUCKET_BUDGET_BYTES / entry_bytes;
        let by_budget = usize::try_from(by_budget).unwrap_or(PIXMAP_POOL_BUCKET_CAP_MAX);
        by_budget.clamp(PIXMAP_POOL_BUCKET_CAP_MIN, PIXMAP_POOL_BUCKET_CAP_MAX)
    }

    fn nominal_bytes(key: PixmapPoolKey) -> u64 {
        u64::from(key.width)
            .saturating_mul(u64::from(key.height))
            .saturating_mul(u64::from(format_bytes_per_pixel(key.format)))
    }

    /// What the pool is holding *right now*, as opposed to the
    /// cumulative counters in [`PixmapPoolStats`].
    ///
    /// `bytes` is what the budget is enforced against: the ledger's
    /// size for each entry's memory, i.e. `mem_reqs.size` of an
    /// OPTIMAL-tiled image, each its own kernel BO (no suballocator).
    /// `nominal_bytes` is `w * h * bytes_per_pixel`, a floor; the gap
    /// between the two is the allocation overhead (~1.37× in #169).
    #[must_use]
    pub fn residency(&self) -> PixmapPoolResidency {
        self.core.lock().map(|c| c.residency()).unwrap_or_default()
    }

    /// Take a recycled entry for `key`, or `None` if the bucket is
    /// empty.
    pub fn try_take(&self, key: PixmapPoolKey) -> Option<PooledPixmapImage> {
        if !Self::eligible(key) {
            return None;
        }
        let entry = self
            .core
            .lock()
            .expect("pixmap pool mutex poisoned")
            .take(key);
        let mut stats = self.stats.lock().expect("pixmap pool stats mutex poisoned");
        if let Some(e) = entry.as_ref() {
            // The caller recategorises again if it is not a pixmap.
            crate::kms::vk::mem_accounting::recategorise(
                e.memory,
                crate::kms::vk::mem_accounting::MemCategory::Pixmap,
            );
            stats.total_takes_hit += 1;
        } else {
            stats.total_takes_miss += 1;
        }
        entry
    }

    /// Try to return `entry` to the pool. Returns `Ok(())` if
    /// accepted; `Err(entry)` if the bucket was full or the key is
    /// ineligible — caller must destroy the entry.
    ///
    /// **Caller guarantees the GPU is done with `entry`** (the
    /// drawable's last render ticket has signalled), the same
    /// guarantee a rejected return relies on to destroy it at once.
    /// That is what lets an accepted entry be destroyed later by
    /// eviction or [`Self::trim_idle`] without a fence.
    pub fn try_return(
        &self,
        key: PixmapPoolKey,
        entry: PooledPixmapImage,
    ) -> Result<(), PooledPixmapImage> {
        // Budget against the real allocation, not the nominal extent:
        // the ledger knows `mem_reqs.size`.
        let bytes = crate::kms::vk::mem_accounting::entry_of(entry.memory)
            .map_or_else(|| Self::nominal_bytes(key), |(size, _)| size);
        let memory = entry.memory;
        let result = self.core.lock().expect("pixmap pool mutex poisoned").put(
            key,
            entry,
            bytes,
            Instant::now(),
        );
        let mut stats = self.stats.lock().expect("pixmap pool stats mutex poisoned");
        match result {
            Ok(evicted) => {
                crate::kms::vk::mem_accounting::recategorise(
                    memory,
                    crate::kms::vk::mem_accounting::MemCategory::PoolIdle,
                );
                stats.total_returns_accepted += 1;
                stats.total_evicted_budget += evicted.len() as u64;
                drop(stats);
                for e in evicted {
                    self.destroy_entry(e);
                }
                Ok(())
            }
            Err((entry, Reject::Oversize)) => {
                let bin = oversize_bin_index(key.width.max(key.height));
                stats.total_returns_rejected_oversize += 1;
                stats.total_returns_rejected_oversize_by_bucket[bin] += 1;
                Err(entry)
            }
            Err((entry, Reject::BucketFull)) => {
                stats.total_returns_rejected_bucket_full += 1;
                Err(entry)
            }
        }
    }

    /// Destroy every entry idle for [`PixmapPoolLimits::idle_evict_after`].
    /// Cheap when nothing is due (one map lookup); the backend calls it
    /// every dispatch iteration from `before_block`.
    pub fn trim_idle(&self, now: Instant) {
        let evicted = self
            .core
            .lock()
            .expect("pixmap pool mutex poisoned")
            .trim_idle(now);
        if evicted.is_empty() {
            return;
        }
        self.stats
            .lock()
            .expect("pixmap pool stats mutex poisoned")
            .total_evicted_idle += evicted.len() as u64;
        for e in evicted {
            self.destroy_entry(e);
        }
    }

    /// When [`Self::trim_idle`] next has work, so an otherwise idle
    /// server still wakes to drain a burst. `None` while empty.
    #[must_use]
    pub fn next_trim_deadline(&self) -> Option<Instant> {
        self.core.lock().ok()?.next_trim_deadline()
    }

    /// Synchronously destroy every pooled entry. Called on backend
    /// shutdown after the scheduler has drained its in-flight
    /// batches (so no `BatchResource` can still hold a back-ref).
    pub fn drain(&self) {
        let entries = self
            .core
            .lock()
            .expect("pixmap pool mutex poisoned")
            .drain();
        for entry in entries {
            self.destroy_entry(entry);
        }
    }

    fn destroy_entry(&self, entry: PooledPixmapImage) {
        unsafe {
            self.vk.device.destroy_image_view(entry.view, None);
            self.vk.device.destroy_image(entry.image, None);
            crate::kms::vk::mem_accounting::free_memory(&self.vk.device, entry.memory);
        }
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
        self.drain();
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

    #[test]
    fn pool_counters_count_both_reject_paths_as_drops() {
        let stats = PixmapPoolStats {
            total_takes_hit: 7,
            total_takes_miss: 3,
            total_returns_accepted: 5,
            total_returns_rejected_bucket_full: 2,
            total_returns_rejected_oversize: 4,
            total_returns_rejected_oversize_by_bucket: [0, 1, 1, 2],
            total_evicted_budget: 10,
            total_evicted_idle: 20,
        };
        assert_eq!(
            stats.pool_counters(),
            crate::kms::vk::mem_accounting::PoolCounters {
                hits: 7,
                misses: 3,
                kept: 5,
                dropped: 36,
            }
        );
    }

    fn key(width: u32, height: u32) -> PixmapPoolKey {
        PixmapPoolKey {
            width,
            height,
            format: vk::Format::B8G8R8A8_UNORM,
        }
    }

    const KIB: u64 = 1024;

    /// Byte bounds in KiB, entry bounds generous, a 60 s idle age.
    fn limits(budget_kib: u64, low_kib: u64) -> PixmapPoolLimits {
        PixmapPoolLimits {
            budget_bytes: budget_kib * KIB,
            low_water_bytes: low_kib * KIB,
            max_entries: 1000,
            low_water_entries: 1000,
            idle_evict_after: Duration::from_secs(60),
        }
    }

    fn put(core: &mut PoolCore<u32>, k: PixmapPoolKey, id: u32, kib: u64, at: Instant) -> Vec<u32> {
        core.put(k, id, kib * KIB, at).expect("accepted")
    }

    #[test]
    fn take_is_lifo_and_an_emptied_bucket_is_removed() {
        let t0 = Instant::now();
        let mut core = PoolCore::new(limits(1024, 512));
        put(&mut core, key(10, 10), 1, 1, t0);
        put(&mut core, key(10, 10), 2, 1, t0);
        assert_eq!(core.take(key(10, 10)), Some(2), "most recent return first");
        assert_eq!(core.take(key(10, 10)), Some(1));
        assert_eq!(core.take(key(10, 10)), None);
        assert!(core.buckets.is_empty(), "no empty bucket left behind");
        assert_eq!((core.entries(), core.bytes), (0, 0));
    }

    #[test]
    fn over_budget_evicts_least_recently_returned_across_buckets_to_low_water() {
        let t0 = Instant::now();
        let mut core = PoolCore::new(limits(10, 6));
        // Ten 1 KiB entries over three buckets, returned in id order.
        for id in 0..10u32 {
            let evicted = put(&mut core, key(1 + id % 3, 1), id, 1, t0);
            assert!(evicted.is_empty(), "at the budget is not over it");
        }
        assert_eq!(core.bytes, 10 * KIB);
        // The eleventh crosses the budget: evict oldest-first to 6 KiB.
        let evicted = put(&mut core, key(1, 1), 10, 1, t0);
        assert_eq!(evicted, vec![0, 1, 2, 3, 4]);
        assert_eq!((core.entries(), core.bytes), (6, 6 * KIB));
        // Hysteresis: refilling to the budget evicts nothing.
        for id in 11..15u32 {
            assert!(put(&mut core, key(4, 1), id, 1, t0).is_empty());
        }
        assert_eq!(core.bytes, 10 * KIB);
        // Residency agrees with the bookkeeping; no empty bucket lingers.
        let r = core.residency();
        assert_eq!((r.entries, r.bytes), (10, 10 * KIB));
        assert_eq!(r.buckets as usize, core.buckets.len());
        assert!(core.buckets.values().all(|b| !b.is_empty()));
    }

    #[test]
    fn eviction_removes_buckets_it_empties() {
        let t0 = Instant::now();
        let mut core = PoolCore::new(limits(2, 1));
        put(&mut core, key(1, 1), 0, 1, t0);
        put(&mut core, key(2, 2), 1, 1, t0);
        assert_eq!(put(&mut core, key(3, 3), 2, 1, t0), vec![0, 1]);
        assert_eq!(
            core.buckets.keys().copied().collect::<Vec<_>>(),
            vec![key(3, 3)]
        );
    }

    #[test]
    fn a_taken_entry_no_longer_counts_toward_the_budget() {
        let t0 = Instant::now();
        let mut core = PoolCore::new(limits(2, 1));
        put(&mut core, key(1, 1), 0, 1, t0);
        put(&mut core, key(1, 1), 1, 1, t0);
        assert_eq!(core.take(key(1, 1)), Some(1));
        assert!(put(&mut core, key(2, 2), 2, 1, t0).is_empty());
        assert_eq!(core.bytes, 2 * KIB);
    }

    #[test]
    fn the_entry_cap_evicts_like_the_byte_budget() {
        let t0 = Instant::now();
        let mut core = PoolCore::new(PixmapPoolLimits {
            max_entries: 4,
            low_water_entries: 2,
            ..limits(1 << 20, 1 << 20)
        });
        for id in 0..4u32 {
            assert!(put(&mut core, key(1 + id, 1), id, 1, t0).is_empty());
        }
        assert_eq!(put(&mut core, key(9, 1), 4, 1, t0), vec![0, 1, 2]);
        assert_eq!(core.entries(), 2);
    }

    #[test]
    fn rejections_leave_the_pool_untouched() {
        let t0 = Instant::now();
        let mut core = PoolCore::new(limits(1 << 20, 1 << 20));
        let big = key(MAX_POOLED_DIM + 1, 1);
        assert_eq!(core.put(big, 7, KIB, t0), Err((7, Reject::Oversize)));
        let k = key(MAX_POOLED_DIM, MAX_POOLED_DIM);
        for id in 0..PixmapPool::bucket_cap(k) {
            put(&mut core, k, u32::try_from(id).unwrap(), 1, t0);
        }
        assert_eq!(core.put(k, 99, KIB, t0), Err((99, Reject::BucketFull)));
        assert_eq!(core.entries(), PixmapPool::bucket_cap(k));
    }

    #[test]
    fn idle_trim_evicts_only_entries_unused_for_the_age() {
        let t0 = Instant::now();
        let age = Duration::from_secs(60);
        let mut core = PoolCore::new(limits(1 << 20, 1 << 20));
        put(&mut core, key(5, 5), 1, 1, t0);
        put(&mut core, key(6, 6), 2, 1, t0 + Duration::from_secs(10));
        assert!(
            core.trim_idle(t0 + age - Duration::from_millis(1))
                .is_empty()
        );
        assert_eq!(core.trim_idle(t0 + age), vec![1]);
        assert_eq!(
            core.buckets.keys().copied().collect::<Vec<_>>(),
            vec![key(6, 6)]
        );
        assert_eq!(core.trim_idle(t0 + age + Duration::from_secs(10)), vec![2]);
        assert_eq!((core.entries(), core.bytes), (0, 0));
        assert!(core.buckets.is_empty());
    }

    /// The steady-state working set must survive: a bucket cycled
    /// every few seconds keeps its entries, while the tail a burst
    /// left in the SAME bucket ages out (LIFO takes never touch it).
    #[test]
    fn idle_trim_spares_a_bucket_reused_every_few_seconds() {
        let t0 = Instant::now();
        let mut core = PoolCore::new(limits(1 << 20, 1 << 20));
        let hot = key(230, 51);
        // A burst parks 20 entries; afterwards the client cycles 2 at a time.
        for id in 0..20u32 {
            put(&mut core, hot, id, 1, t0);
        }
        put(&mut core, key(7, 7), 100, 1, t0); // never reused
        let mut evicted = Vec::new();
        for step in 1..=60u32 {
            let now = t0 + Duration::from_secs(u64::from(step) * 5);
            let a = core.take(hot).expect("hit");
            let b = core.take(hot).expect("hit");
            put(&mut core, hot, b, 1, now);
            put(&mut core, hot, a, 1, now);
            evicted.extend(core.trim_idle(now));
        }
        // Every take hit; only the burst tail and the unused bucket went.
        assert_eq!(core.buckets.get(&hot).map(VecDeque::len), Some(2));
        assert_eq!(evicted.len(), 19);
        assert!(evicted.contains(&100));
        assert!(!core.buckets.contains_key(&key(7, 7)));
    }

    #[test]
    fn next_trim_deadline_tracks_the_oldest_entry() {
        let t0 = Instant::now();
        let mut core = PoolCore::new(limits(1 << 20, 1 << 20));
        assert_eq!(core.next_trim_deadline(), None);
        put(&mut core, key(1, 1), 1, 1, t0);
        put(&mut core, key(2, 2), 2, 1, t0 + Duration::from_secs(3));
        let due = t0 + Duration::from_secs(60) + PIXMAP_POOL_TRIM_SLACK;
        assert_eq!(core.next_trim_deadline(), Some(due));
        // Waking at the deadline drains everything due by then.
        assert_eq!(core.trim_idle(due), vec![1]);
        assert_eq!(
            core.next_trim_deadline(),
            Some(due + Duration::from_secs(3))
        );
        assert_eq!(core.take(key(2, 2)), Some(2));
        assert_eq!(core.next_trim_deadline(), None);
    }

    /// The reporter's pre-burst steady state (#169: ~390 entries over
    /// ~104 buckets, ≤3.5 MiB) cycled under the production limits:
    /// every take must hit and nothing may be evicted.
    #[test]
    fn production_limits_never_evict_the_reported_steady_state() {
        let t0 = Instant::now();
        let mut core = PoolCore::new(PixmapPoolLimits::default());
        let keys: Vec<_> = (0..104u32).map(|i| key(16 + i, 16 + i % 7)).collect();
        let mut id = 0u32;
        for &k in &keys {
            for _ in 0..4 {
                // ~9 KiB each, the reporter's 3.5 MiB / 389 entries.
                put(&mut core, k, id, 9, t0);
                id += 1;
            }
        }
        for step in 1..=600u32 {
            let now = t0 + Duration::from_secs(u64::from(step));
            for &k in &keys {
                // The client holds its whole working set, then frees it.
                let held: Vec<_> = (0..4)
                    .map(|_| core.take(k).expect("steady state always hits"))
                    .collect();
                for e in held {
                    assert!(core.put(k, e, 9 * KIB, now).expect("accepted").is_empty());
                }
            }
            assert!(core.trim_idle(now).is_empty(), "step {step}");
        }
        assert_eq!(core.entries(), 416);
    }

    #[test]
    fn production_limits_cap_the_reported_burst() {
        // #169: 9723 entries over 2736 buckets, ~100 KiB each real.
        let t0 = Instant::now();
        let mut core = PoolCore::new(PixmapPoolLimits::default());
        let mut id = 0u32;
        for b in 0..2736u32 {
            for _ in 0..4 {
                let _ = core.put(key(1 + b % 256, 1 + b / 256), id, 100 * KIB, t0);
                id += 1;
            }
        }
        assert!(core.bytes <= PIXMAP_POOL_BUDGET_BYTES);
        assert!(core.entries() <= PIXMAP_POOL_MAX_ENTRIES);
        let drained = core.trim_idle(t0 + PIXMAP_POOL_IDLE_EVICT_AFTER);
        assert!(!drained.is_empty());
        assert_eq!((core.entries(), core.bytes), (0, 0));
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
