//! Per-category accounting of every `vkAllocateMemory` this process makes.
//!
//! `vram.rs` reports the driver's per-process heap usage; this ledger says
//! which of our allocations hold it. The difference is driver-internal
//! memory (pipelines, descriptor pools, command buffers).
//!
//! Keyed on the raw `vk::DeviceMemory` handle. [`allocate_memory`] and
//! [`free_memory`] are the only callers of the raw `ash::Device` methods;
//! `clippy.toml` disallows them everywhere else. Allocation is rare relative
//! to the cost of `vkAllocateMemory`, so one global mutex is fine.
//!
//! Besides the live totals, the ledger keeps cumulative per-[`ChurnClass`]
//! allocate/free counters (#177). A live-count ledger sampled once a second
//! cannot see an allocation that lives 15 ms, and those short-lived
//! allocations are what drives libdrm's `amdgpu_vamgr_free_va` cost on
//! RADV. The counters are bumped under the ledger lock the call already
//! takes, so they add no synchronisation; `vram churn [1s]` diffs two
//! [`ChurnSnapshot`]s.

use std::{
    collections::HashMap,
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
};

use ash::vk::{self, Handle};

/// What an allocation is used for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MemCategory {
    /// Storage backing an X window.
    WindowStorage,
    /// Composite redirect backing pixmap of a redirected window.
    RedirectBacking,
    /// Client pixmap storage (and default for drawable storage).
    Pixmap,
    /// Storage parked in the pixmap pool, not owned by any drawable.
    PoolIdle,
    /// Client dma-buf imported via DRI3 (driver may not count it as ours).
    Dri3Import,
    /// Exportable (dma-buf) storage made for TFP / DRI3 export.
    TfpExport,
    /// A redirect backing promoted to exportable storage for TFP.
    RedirectExport,
    /// Scanout buffers and copied-scanout sources.
    Scanout,
    /// Intermediate images of CRTCs scanned out through a RANDR transform.
    Transform,
    /// Per-op scratch images (copy, mask, dst readback, engine scratch).
    Scratch,
    /// Host-visible staging / upload / readback buffers.
    Staging,
    /// Glyph atlases and glyph caches.
    Glyph,
    /// Everything else (solid-colour images, gradients, diagnostics).
    Other,
}

impl MemCategory {
    /// Every category, in log order.
    pub const ALL: [Self; 13] = [
        Self::WindowStorage,
        Self::RedirectBacking,
        Self::Pixmap,
        Self::PoolIdle,
        Self::Dri3Import,
        Self::TfpExport,
        Self::RedirectExport,
        Self::Scanout,
        Self::Transform,
        Self::Scratch,
        Self::Staging,
        Self::Glyph,
        Self::Other,
    ];

    /// Short name used in the telemetry line.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::WindowStorage => "window",
            Self::RedirectBacking => "redirect",
            Self::Pixmap => "pixmap",
            Self::PoolIdle => "pool_idle",
            Self::Dri3Import => "dri3_import",
            Self::TfpExport => "tfp_export",
            Self::RedirectExport => "redirect_export",
            Self::Scanout => "scanout",
            Self::Transform => "transform",
            Self::Scratch => "scratch",
            Self::Staging => "staging",
            Self::Glyph => "glyph",
            Self::Other => "other",
        }
    }

    const fn index(self) -> usize {
        self as usize
    }
}

/// Finer-grained "who asked for this memory" key for the churn counters.
///
/// [`MemCategory`] answers "what holds VRAM now"; this answers "which call
/// site allocates and frees at what rate". Most sites take the class their
/// [`MemCategory`] implies ([`ChurnClass::for_category`]); the per-request
/// buffers that dominate #177 name theirs explicitly through
/// [`allocate_memory_as`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChurnClass {
    /// Per-request glyph-run instance buffer (CompositeGlyphs text).
    GlyphRun,
    /// Per-request ImageText8/16 instance buffer.
    ImageText,
    /// Per-request trapezoid / triangle vertex buffer.
    Traps,
    /// Glyph pixel upload staging on first use of a glyph.
    GlyphUpload,
    /// Shared blocks of the per-frame upload arena. The four classes
    /// above now only allocate for a request too large for a block.
    UploadArena,
    /// PutImage upload staging allocated on a `StagingPool` miss.
    StagingPool,
    /// GetImage readback staging.
    Readback,
    /// Any other host-visible staging buffer.
    StagingOther,
    /// Drawable storage for a pixmap with both dimensions within the
    /// pixmap pool's `MAX_POOLED_DIM` (so allocated only on a pool miss).
    PixmapSmall,
    /// Drawable storage for a pixmap larger than the pool takes.
    PixmapLarge,
    /// Window storage.
    Window,
    /// Composite redirect backing.
    Redirect,
    /// Scanout buffers.
    Scanout,
    /// RANDR transform intermediates.
    Transform,
    /// Gradient LUT images and their upload staging.
    Gradient,
    /// Per-op scratch images.
    Scratch,
    /// DRI3 dma-buf imports.
    DmabufImport,
    /// Exportable (dma-buf) storage for TFP / DRI3 export.
    DmabufExport,
    /// Glyph atlases and glyph caches.
    GlyphCache,
    /// Everything else.
    Other,
}

impl ChurnClass {
    /// Every class, in log order.
    pub const ALL: [Self; 20] = [
        Self::GlyphRun,
        Self::ImageText,
        Self::Traps,
        Self::GlyphUpload,
        Self::UploadArena,
        Self::StagingPool,
        Self::Readback,
        Self::StagingOther,
        Self::PixmapSmall,
        Self::PixmapLarge,
        Self::Window,
        Self::Redirect,
        Self::Scanout,
        Self::Transform,
        Self::Gradient,
        Self::Scratch,
        Self::DmabufImport,
        Self::DmabufExport,
        Self::GlyphCache,
        Self::Other,
    ];

    /// Short name used in the telemetry line.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::GlyphRun => "glyph_run",
            Self::ImageText => "image_text",
            Self::Traps => "traps",
            Self::GlyphUpload => "glyph_upload",
            Self::UploadArena => "upload_arena",
            Self::StagingPool => "staging_pool",
            Self::Readback => "readback",
            Self::StagingOther => "staging_other",
            Self::PixmapSmall => "pixmap_small",
            Self::PixmapLarge => "pixmap_large",
            Self::Window => "window",
            Self::Redirect => "redirect",
            Self::Scanout => "scanout",
            Self::Transform => "transform",
            Self::Gradient => "gradient",
            Self::Scratch => "scratch",
            Self::DmabufImport => "dmabuf_import",
            Self::DmabufExport => "dmabuf_export",
            Self::GlyphCache => "glyph_cache",
            Self::Other => "other",
        }
    }

    const fn index(self) -> usize {
        self as usize
    }

    /// The class an allocation accounted under `category` takes when its
    /// site names none. `small` says whether drawable storage fits the
    /// pixmap pool; it only splits [`MemCategory::Pixmap`].
    #[must_use]
    pub const fn for_category(category: MemCategory, small: bool) -> Self {
        match category {
            MemCategory::WindowStorage => Self::Window,
            MemCategory::RedirectBacking => Self::Redirect,
            MemCategory::Pixmap | MemCategory::PoolIdle => {
                if small {
                    Self::PixmapSmall
                } else {
                    Self::PixmapLarge
                }
            }
            MemCategory::Dri3Import => Self::DmabufImport,
            MemCategory::TfpExport | MemCategory::RedirectExport => Self::DmabufExport,
            MemCategory::Scanout => Self::Scanout,
            MemCategory::Transform => Self::Transform,
            MemCategory::Scratch => Self::Scratch,
            MemCategory::Staging => Self::StagingOther,
            MemCategory::Glyph => Self::GlyphCache,
            MemCategory::Other => Self::Other,
        }
    }
}

/// Cumulative allocate/free counters for one [`ChurnClass`], plus what
/// the class holds live right now.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ChurnTally {
    pub allocs: u64,
    pub frees: u64,
    pub alloc_bytes: u64,
    pub free_bytes: u64,
    pub live: Tally,
}

/// Hit/miss/return counters of a reuse pool, cumulative.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PoolCounters {
    /// Requests served from the pool (no `vkAllocateMemory`).
    pub hits: u64,
    /// Requests that fell through to a fresh allocation.
    pub misses: u64,
    /// Returned buffers the pool kept (no `vkFreeMemory`).
    pub kept: u64,
    /// Returned buffers the pool refused, so they were freed.
    pub dropped: u64,
}

impl PoolCounters {
    /// Counter deltas from `prev` to `self`, saturating at zero.
    #[must_use]
    pub fn since(&self, prev: &Self) -> Self {
        Self {
            hits: self.hits.saturating_sub(prev.hits),
            misses: self.misses.saturating_sub(prev.misses),
            kept: self.kept.saturating_sub(prev.kept),
            dropped: self.dropped.saturating_sub(prev.dropped),
        }
    }
}

/// Per-frame upload arena counters (#177). Block allocations and frees
/// are ledger events under [`ChurnClass::UploadArena`]; these count what
/// the ledger cannot see: sub-allocations, which allocate nothing.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ArenaCounters {
    /// Requests served from a shared block, cumulative.
    pub suballocs: u64,
    /// Bytes of those requests, cumulative.
    pub suballoc_bytes: u64,
    /// Requests too large for a block, served by a dedicated allocation,
    /// cumulative.
    pub dedicated: u64,
    /// Blocks on the idle list right now (a gauge, not a counter).
    pub idle_blocks: u64,
}

/// Cumulative churn counters, indexed by [`ChurnClass::ALL`] order.
/// Diff two with [`format_churn_line`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ChurnSnapshot {
    pub classes: [ChurnTally; ChurnClass::ALL.len()],
    /// The engine's PutImage `StagingPool`.
    pub staging_pool: PoolCounters,
    /// The engine's per-frame upload arena.
    pub upload_arena: ArenaCounters,
}

impl ChurnSnapshot {
    #[must_use]
    pub fn class(&self, class: ChurnClass) -> ChurnTally {
        self.classes[class.index()]
    }

    /// Live `VkDeviceMemory` objects across every class and memory type.
    #[must_use]
    pub fn live_count(&self) -> u64 {
        self.classes.iter().map(|c| c.live.count).sum()
    }

    /// Bytes behind [`Self::live_count`].
    #[must_use]
    pub fn live_bytes(&self) -> u64 {
        self.classes.iter().map(|c| c.live.bytes).sum()
    }
}

/// Bytes and allocation count for one category on one side of the split.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Tally {
    pub bytes: u64,
    pub count: u64,
}

/// Point-in-time totals, indexed by [`MemCategory::ALL`] order.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MemSnapshot {
    pub device_local: [Tally; MemCategory::ALL.len()],
    pub host: [Tally; MemCategory::ALL.len()],
}

impl MemSnapshot {
    #[must_use]
    pub fn device_local(&self, category: MemCategory) -> Tally {
        self.device_local[category.index()]
    }

    #[must_use]
    pub fn host(&self, category: MemCategory) -> Tally {
        self.host[category.index()]
    }

    #[must_use]
    pub fn total_device_local_bytes(&self) -> u64 {
        self.device_local.iter().map(|t| t.bytes).sum()
    }

    #[must_use]
    pub fn total_host_bytes(&self) -> u64 {
        self.host.iter().map(|t| t.bytes).sum()
    }
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    size: u64,
    category: MemCategory,
    device_local: bool,
    class: ChurnClass,
    /// Drawable storage that fits the pixmap pool.
    small: bool,
    /// Not yet recategorised since allocation. The first recategorise of
    /// a fresh entry is the site labelling what it just made (a redirect
    /// backing comes out of `create_pixmap`), so the allocation moves to
    /// the new class with it.
    fresh: bool,
}

#[derive(Default)]
struct Ledger {
    entries: HashMap<u64, Entry>,
    totals: MemSnapshot,
    churn: [ChurnTally; ChurnClass::ALL.len()],
}

impl Ledger {
    fn side(&mut self, device_local: bool) -> &mut [Tally; MemCategory::ALL.len()] {
        if device_local {
            &mut self.totals.device_local
        } else {
            &mut self.totals.host
        }
    }

    fn add(&mut self, e: Entry) {
        let t = &mut self.side(e.device_local)[e.category.index()];
        t.bytes += e.size;
        t.count += 1;
        let live = &mut self.churn[e.class.index()].live;
        live.bytes += e.size;
        live.count += 1;
    }

    fn sub(&mut self, e: Entry) {
        let t = &mut self.side(e.device_local)[e.category.index()];
        t.bytes = t.bytes.saturating_sub(e.size);
        t.count = t.count.saturating_sub(1);
        let live = &mut self.churn[e.class.index()].live;
        live.bytes = live.bytes.saturating_sub(e.size);
        live.count = live.count.saturating_sub(1);
    }

    fn alloc(&mut self, key: u64, e: Entry) {
        if let Some(old) = self.entries.insert(key, e) {
            self.sub(old);
        }
        self.add(e);
        let c = &mut self.churn[e.class.index()];
        c.allocs += 1;
        c.alloc_bytes += e.size;
    }

    fn free(&mut self, key: u64) {
        if let Some(old) = self.entries.remove(&key) {
            self.sub(old);
            let c = &mut self.churn[old.class.index()];
            c.frees += 1;
            c.free_bytes += old.size;
        }
    }

    fn recategorise(&mut self, key: u64, category: MemCategory) {
        let Some(old) = self.entries.get(&key).copied() else {
            return;
        };
        if old.category == category {
            return;
        }
        // Parking in the pool is a state, not a use: the entry keeps the
        // class it was last used as, and a pool drop is attributed there.
        let class = if category == MemCategory::PoolIdle {
            old.class
        } else {
            ChurnClass::for_category(category, old.small)
        };
        if old.fresh && class != old.class {
            // Move the allocation event with the label (see `Entry::fresh`).
            // A move that straddles a telemetry tick steps the old class's
            // cumulative count back by one; the line's deltas saturate.
            let from = &mut self.churn[old.class.index()];
            from.allocs = from.allocs.saturating_sub(1);
            from.alloc_bytes = from.alloc_bytes.saturating_sub(old.size);
            let to = &mut self.churn[class.index()];
            to.allocs += 1;
            to.alloc_bytes += old.size;
        }
        self.sub(old);
        let new = Entry {
            category,
            class,
            fresh: false,
            ..old
        };
        self.entries.insert(key, new);
        self.add(new);
    }
}

/// Cumulative `StagingPool` counters. Atomics because the pool itself is
/// engine state, not ledger state; one relaxed add per PutImage.
static STAGING_POOL_HITS: AtomicU64 = AtomicU64::new(0);
static STAGING_POOL_MISSES: AtomicU64 = AtomicU64::new(0);
static STAGING_POOL_KEPT: AtomicU64 = AtomicU64::new(0);
static STAGING_POOL_DROPPED: AtomicU64 = AtomicU64::new(0);

/// What happened to one reuse-pool request or return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolEvent {
    Hit,
    Miss,
    Kept,
    Dropped,
}

/// Count one `StagingPool` event for `vram churn`.
pub fn note_staging_pool(event: PoolEvent) {
    let counter = match event {
        PoolEvent::Hit => &STAGING_POOL_HITS,
        PoolEvent::Miss => &STAGING_POOL_MISSES,
        PoolEvent::Kept => &STAGING_POOL_KEPT,
        PoolEvent::Dropped => &STAGING_POOL_DROPPED,
    };
    counter.fetch_add(1, Ordering::Relaxed);
}

fn staging_pool_counters() -> PoolCounters {
    PoolCounters {
        hits: STAGING_POOL_HITS.load(Ordering::Relaxed),
        misses: STAGING_POOL_MISSES.load(Ordering::Relaxed),
        kept: STAGING_POOL_KEPT.load(Ordering::Relaxed),
        dropped: STAGING_POOL_DROPPED.load(Ordering::Relaxed),
    }
}

/// Per-frame upload arena counters (#177). Atomics for the same reason as
/// the `StagingPool` ones: the arena is engine state.
static ARENA_SUBALLOCS: AtomicU64 = AtomicU64::new(0);
static ARENA_SUBALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
static ARENA_DEDICATED: AtomicU64 = AtomicU64::new(0);
static ARENA_IDLE_BLOCKS: AtomicU64 = AtomicU64::new(0);

/// Count one upload-arena request of `bytes` for `vram churn`:
/// `dedicated` when it was too large for a block.
pub fn note_upload_arena_request(bytes: u64, dedicated: bool) {
    if dedicated {
        ARENA_DEDICATED.fetch_add(1, Ordering::Relaxed);
    } else {
        ARENA_SUBALLOCS.fetch_add(1, Ordering::Relaxed);
        ARENA_SUBALLOC_BYTES.fetch_add(bytes, Ordering::Relaxed);
    }
}

/// Publish how many blocks sit on the upload arena's idle list.
pub fn set_upload_arena_idle_blocks(blocks: usize) {
    ARENA_IDLE_BLOCKS.store(blocks as u64, Ordering::Relaxed);
}

fn upload_arena_counters() -> ArenaCounters {
    ArenaCounters {
        suballocs: ARENA_SUBALLOCS.load(Ordering::Relaxed),
        suballoc_bytes: ARENA_SUBALLOC_BYTES.load(Ordering::Relaxed),
        dedicated: ARENA_DEDICATED.load(Ordering::Relaxed),
        idle_blocks: ARENA_IDLE_BLOCKS.load(Ordering::Relaxed),
    }
}

static LEDGER: Mutex<Option<Ledger>> = Mutex::new(None);

fn ledger() -> MutexGuard<'static, Option<Ledger>> {
    LEDGER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn with_ledger<R>(f: impl FnOnce(&mut Ledger) -> R) -> R {
    let mut guard = ledger();
    f(guard.get_or_insert_with(Ledger::default))
}

/// Category for exportable memory that replaces storage currently
/// accounted as `current` (TFP / DRI3 promotion).
#[must_use]
pub fn export_category_for(current: Option<MemCategory>) -> MemCategory {
    match current {
        Some(MemCategory::RedirectBacking | MemCategory::RedirectExport) => {
            MemCategory::RedirectExport
        }
        _ => MemCategory::TfpExport,
    }
}

/// Whether memory type `type_index` carries `DEVICE_LOCAL`.
#[must_use]
pub fn is_device_local(props: &vk::PhysicalDeviceMemoryProperties, type_index: u32) -> bool {
    props
        .memory_types
        .get(type_index as usize)
        .is_some_and(|t| {
            t.property_flags
                .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
        })
}

/// Record a successful allocation. Re-noting a live handle replaces it.
fn note_alloc(
    mem: vk::DeviceMemory,
    size: u64,
    category: MemCategory,
    device_local: bool,
    class: ChurnClass,
    small: bool,
) {
    if mem == vk::DeviceMemory::null() {
        return;
    }
    with_ledger(|l| {
        l.alloc(
            mem.as_raw(),
            Entry {
                size,
                category,
                device_local,
                class,
                small,
                fresh: true,
            },
        );
    });
}

/// `vkAllocateMemory`, accounted under `category`. `props` must be the
/// properties `info.memory_type_index` was chosen from.
///
/// # Errors
///
/// The `vkAllocateMemory` result.
pub(crate) fn allocate_memory(
    device: &ash::Device,
    info: &vk::MemoryAllocateInfo<'_>,
    category: MemCategory,
    props: &vk::PhysicalDeviceMemoryProperties,
) -> Result<vk::DeviceMemory, vk::Result> {
    allocate_memory_as(
        device,
        info,
        category,
        ChurnClass::for_category(category, false),
        props,
    )
}

/// [`allocate_memory`], counting the allocation under churn `class`
/// instead of the one `category` implies.
///
/// # Errors
///
/// The `vkAllocateMemory` result.
pub(crate) fn allocate_memory_as(
    device: &ash::Device,
    info: &vk::MemoryAllocateInfo<'_>,
    category: MemCategory,
    class: ChurnClass,
    props: &vk::PhysicalDeviceMemoryProperties,
) -> Result<vk::DeviceMemory, vk::Result> {
    allocate_impl(device, info, category, class, false, props)
}

/// [`allocate_memory`] for drawable storage. `pool_sized` says whether
/// both dimensions fit the pixmap pool; it splits pixmap churn into
/// `pixmap_small` / `pixmap_large` and survives later recategorisation
/// (a pool entry can serve a window, then a pixmap again).
///
/// # Errors
///
/// The `vkAllocateMemory` result.
pub(crate) fn allocate_storage_memory(
    device: &ash::Device,
    info: &vk::MemoryAllocateInfo<'_>,
    category: MemCategory,
    pool_sized: bool,
    props: &vk::PhysicalDeviceMemoryProperties,
) -> Result<vk::DeviceMemory, vk::Result> {
    allocate_impl(
        device,
        info,
        category,
        ChurnClass::for_category(category, pool_sized),
        pool_sized,
        props,
    )
}

fn allocate_impl(
    device: &ash::Device,
    info: &vk::MemoryAllocateInfo<'_>,
    category: MemCategory,
    class: ChurnClass,
    small: bool,
    props: &vk::PhysicalDeviceMemoryProperties,
) -> Result<vk::DeviceMemory, vk::Result> {
    // SAFETY: `info` is a valid allocate-info chain built by the caller.
    #[allow(clippy::disallowed_methods)]
    let mem = unsafe { device.allocate_memory(info, None)? };
    #[cfg(test)]
    THREAD_ALLOC_CALLS.with(|c| c.set(c.get() + 1));
    note_alloc(
        mem,
        info.allocation_size,
        category,
        is_device_local(props, info.memory_type_index),
        class,
        small,
    );
    Ok(mem)
}

#[cfg(test)]
thread_local! {
    static THREAD_ALLOC_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Successful [`allocate_memory`] calls made on the current thread. The
/// ledger is process-global and tests run in parallel; a backend under test
/// allocates on its own thread, so a delta of this counter is exactly that
/// backend's `vkAllocateMemory` count.
#[cfg(test)]
pub(crate) fn thread_alloc_calls() -> u64 {
    THREAD_ALLOC_CALLS.with(std::cell::Cell::get)
}

/// `vkFreeMemory`, removing `mem` from the ledger first.
///
/// # Safety
///
/// Same contract as `ash::Device::free_memory`: `mem` was allocated from
/// `device` and nothing still uses it.
pub(crate) unsafe fn free_memory(device: &ash::Device, mem: vk::DeviceMemory) {
    note_free(mem);
    // SAFETY: forwarded from this function's contract.
    #[allow(clippy::disallowed_methods)]
    unsafe {
        device.free_memory(mem, None);
    }
}

/// Record a `free_memory`. Unknown handles are ignored.
fn note_free(mem: vk::DeviceMemory) {
    if mem == vk::DeviceMemory::null() {
        return;
    }
    with_ledger(|l| l.free(mem.as_raw()));
}

/// Move a live allocation to `category`. Unknown handles are ignored.
pub fn recategorise(mem: vk::DeviceMemory, category: MemCategory) {
    if mem == vk::DeviceMemory::null() {
        return;
    }
    with_ledger(|l| l.recategorise(mem.as_raw(), category));
}

/// The category a live handle is currently accounted under.
#[must_use]
pub fn category_of(mem: vk::DeviceMemory) -> Option<MemCategory> {
    ledger()
        .as_ref()
        .and_then(|l| l.entries.get(&mem.as_raw()).map(|e| e.category))
}

/// Size and category of a live handle.
pub(crate) fn entry_of(mem: vk::DeviceMemory) -> Option<(u64, MemCategory)> {
    ledger()
        .as_ref()
        .and_then(|l| l.entries.get(&mem.as_raw()).map(|e| (e.size, e.category)))
}

/// Current per-category totals.
#[must_use]
pub fn snapshot() -> MemSnapshot {
    ledger().as_ref().map(|l| l.totals).unwrap_or_default()
}

/// Current cumulative churn counters and per-class live totals.
#[must_use]
pub fn churn_snapshot() -> ChurnSnapshot {
    let classes = ledger().as_ref().map(|l| l.churn).unwrap_or_default();
    ChurnSnapshot {
        classes,
        staging_pool: staging_pool_counters(),
        upload_arena: upload_arena_counters(),
    }
}

/// The `vram churn [1s]` telemetry line body: allocate/free rates per
/// class between `prev` and `cur` (taken `elapsed_secs` apart), each
/// class's live count/bytes at `cur`, the total live `VkDeviceMemory`
/// count (what libdrm's VA free-list cost scales with), and the reuse
/// section: the `StagingPool`'s hit/miss/keep/drop rates, the upload
/// arena's blocks (live, idle, allocated and freed per second) and the
/// requests it served (sub-allocations per second and their bytes, and
/// oversize requests that fell back to a dedicated allocation), and the
/// pixmap pool's rates. `pixmap_pool` is the pixmap pool's counter delta
/// over the same interval, when the pool exists.
#[must_use]
pub fn format_churn_line(
    prev: &ChurnSnapshot,
    cur: &ChurnSnapshot,
    elapsed_secs: f64,
    pixmap_pool: Option<PoolCounters>,
) -> String {
    use std::fmt::Write as _;
    let secs = if elapsed_secs > 0.0 {
        elapsed_secs
    } else {
        1.0
    };
    let per_s = |n: u64| n as f64 / secs;
    let mib_s = |b: u64| b as f64 / (1024.0 * 1024.0) / secs;
    let mib = |b: u64| b as f64 / (1024.0 * 1024.0);
    let mut total = ChurnTally::default();
    let mut body = String::new();
    for c in ChurnClass::ALL {
        let (p, n) = (prev.class(c), cur.class(c));
        let d = ChurnTally {
            allocs: n.allocs.saturating_sub(p.allocs),
            frees: n.frees.saturating_sub(p.frees),
            alloc_bytes: n.alloc_bytes.saturating_sub(p.alloc_bytes),
            free_bytes: n.free_bytes.saturating_sub(p.free_bytes),
            live: n.live,
        };
        total.allocs += d.allocs;
        total.frees += d.frees;
        total.alloc_bytes += d.alloc_bytes;
        total.free_bytes += d.free_bytes;
        let _ = write!(
            body,
            " {}[alloc={:.0}/s free={:.0}/s in={:.2}MiB/s out={:.2}MiB/s live={}/{:.1}MiB]",
            c.label(),
            per_s(d.allocs),
            per_s(d.frees),
            mib_s(d.alloc_bytes),
            mib_s(d.free_bytes),
            d.live.count,
            mib(d.live.bytes),
        );
    }
    let mut s = String::from("vram churn [1s]:");
    let _ = write!(
        s,
        " live_allocs={} live={:.1}MiB alloc={:.0}/s free={:.0}/s in={:.2}MiB/s out={:.2}MiB/s |",
        cur.live_count(),
        mib(cur.live_bytes()),
        per_s(total.allocs),
        per_s(total.frees),
        mib_s(total.alloc_bytes),
        mib_s(total.free_bytes),
    );
    s.push_str(&body);
    s.push_str(" | reuse");
    let pool = |s: &mut String, name: &str, d: PoolCounters| {
        let _ = write!(
            s,
            " {name}[hit={:.0}/s miss={:.0}/s kept={:.0}/s dropped={:.0}/s]",
            per_s(d.hits),
            per_s(d.misses),
            per_s(d.kept),
            per_s(d.dropped),
        );
    };
    pool(
        &mut s,
        "staging",
        cur.staging_pool.since(&prev.staging_pool),
    );
    let (pa, ca) = (
        prev.class(ChurnClass::UploadArena),
        cur.class(ChurnClass::UploadArena),
    );
    let (pr, cr) = (prev.upload_arena, cur.upload_arena);
    let _ = write!(
        s,
        " arena[blocks={} idle={} block_alloc={:.0}/s block_free={:.0}/s sub={:.0}/s \
         sub_in={:.2}MiB/s dedicated={:.0}/s]",
        ca.live.count,
        cr.idle_blocks,
        per_s(ca.allocs.saturating_sub(pa.allocs)),
        per_s(ca.frees.saturating_sub(pa.frees)),
        per_s(cr.suballocs.saturating_sub(pr.suballocs)),
        mib_s(cr.suballoc_bytes.saturating_sub(pr.suballoc_bytes)),
        per_s(cr.dedicated.saturating_sub(pr.dedicated)),
    );
    if let Some(d) = pixmap_pool {
        pool(&mut s, "pixmap", d);
    }
    s
}

/// The `vram by use [1s]` telemetry line body. `heap_usage` is the
/// device-local `heapUsage` sample, when available.
#[must_use]
pub fn format_line(snap: &MemSnapshot, heap_usage: Option<u64>) -> String {
    use std::fmt::Write as _;
    let mib = |b: u64| b as f64 / (1024.0 * 1024.0);
    let mut s = String::from("vram by use [1s]:");
    for c in MemCategory::ALL {
        let t = snap.device_local(c);
        let _ = write!(s, " {}={:.1}MiB/{}", c.label(), mib(t.bytes), t.count);
    }
    let tracked = snap.total_device_local_bytes();
    let _ = write!(
        s,
        " tracked_device_local={:.1}MiB host={:.1}MiB",
        mib(tracked),
        mib(snap.total_host_bytes())
    );
    if let Some(usage) = heap_usage {
        let untracked = i128::from(usage) - i128::from(tracked);
        let _ = write!(
            s,
            " untracked={:.1}MiB",
            untracked as f64 / (1024.0 * 1024.0)
        );
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(size: u64, category: MemCategory, device_local: bool) -> Entry {
        Entry {
            size,
            category,
            device_local,
            class: ChurnClass::for_category(category, false),
            small: false,
            fresh: true,
        }
    }

    fn ec(size: u64, category: MemCategory, class: ChurnClass) -> Entry {
        Entry {
            size,
            category,
            device_local: true,
            class,
            small: class == ChurnClass::PixmapSmall,
            fresh: true,
        }
    }

    fn churn_of(l: &Ledger) -> ChurnSnapshot {
        ChurnSnapshot {
            classes: l.churn,
            ..ChurnSnapshot::default()
        }
    }

    #[test]
    fn short_lived_alloc_shows_in_churn_not_live() {
        // The #177 signature: allocated and freed between two samples.
        let mut l = Ledger::default();
        l.alloc(9, ec(4096, MemCategory::Pixmap, ChurnClass::PixmapLarge));
        let before = churn_of(&l);
        let live_before = l.totals;
        l.alloc(1, ec(1024, MemCategory::Staging, ChurnClass::GlyphRun));
        l.free(1);
        l.alloc(2, ec(1024, MemCategory::Staging, ChurnClass::GlyphRun));
        l.free(2);
        l.alloc(3, ec(8192, MemCategory::Staging, ChurnClass::Traps));
        l.free(3);
        let after = churn_of(&l);
        // The live ledger cannot tell anything happened.
        assert_eq!(l.totals, live_before);
        let g = after.class(ChurnClass::GlyphRun);
        assert_eq!((g.allocs, g.frees), (2, 2));
        assert_eq!((g.alloc_bytes, g.free_bytes), (2048, 2048));
        assert_eq!(g.live, Tally::default());
        let t = after.class(ChurnClass::Traps);
        assert_eq!((t.allocs, t.frees, t.alloc_bytes), (1, 1, 8192));
        assert_eq!(after.live_count(), 1);
        assert_eq!(after.live_bytes(), 4096);
        let line = format_churn_line(&before, &after, 1.0, None);
        assert!(
            line.contains(
                " glyph_run[alloc=2/s free=2/s in=0.00MiB/s out=0.00MiB/s live=0/0.0MiB]"
            ),
            "{line}"
        );
        assert!(line.contains(" traps[alloc=1/s free=1/s "), "{line}");
        assert!(
            line.starts_with("vram churn [1s]: live_allocs=1 live=0.0MiB alloc=3/s free=3/s "),
            "{line}"
        );
        assert!(
            line.contains(
                " pixmap_large[alloc=0/s free=0/s in=0.00MiB/s out=0.00MiB/s live=1/0.0MiB]"
            ),
            "{line}"
        );
    }

    #[test]
    fn churn_line_normalises_by_elapsed_and_reports_bytes() {
        let mut l = Ledger::default();
        let before = churn_of(&l);
        for k in 0..4 {
            l.alloc(k, ec(1 << 20, MemCategory::Pixmap, ChurnClass::PixmapLarge));
        }
        l.free(0);
        l.free(1);
        let after = churn_of(&l);
        let line = format_churn_line(&before, &after, 2.0, None);
        assert!(
            line.contains(
                " pixmap_large[alloc=2/s free=1/s in=2.00MiB/s out=1.00MiB/s live=2/2.0MiB]"
            ),
            "{line}"
        );
        assert!(
            line.contains(
                "live_allocs=2 live=2.0MiB alloc=2/s free=1/s in=2.00MiB/s out=1.00MiB/s |"
            )
        );
        // Every class appears, in order, so the line is awk-able.
        let mut at = 0;
        for c in ChurnClass::ALL {
            let pos = line[at..]
                .find(&format!(" {}[", c.label()))
                .unwrap_or_else(|| panic!("{} missing or out of order: {line}", c.label()));
            at += pos + 1;
        }
    }

    #[test]
    fn churn_line_pool_rates() {
        let prev = ChurnSnapshot::default();
        let cur = ChurnSnapshot {
            staging_pool: PoolCounters {
                hits: 10,
                misses: 2,
                kept: 9,
                dropped: 1,
            },
            ..ChurnSnapshot::default()
        };
        let pix = PoolCounters {
            hits: 4,
            misses: 3,
            kept: 5,
            dropped: 0,
        };
        let line = format_churn_line(&prev, &cur, 1.0, Some(pix));
        assert!(
            line.contains(" | reuse staging[hit=10/s miss=2/s kept=9/s dropped=1/s]"),
            "{line}"
        );
        assert!(
            line.ends_with(" pixmap[hit=4/s miss=3/s kept=5/s dropped=0/s]"),
            "{line}"
        );
        assert!(!format_churn_line(&prev, &cur, 1.0, None).contains(" pixmap["));
        // A counter that stepped back (see `Ledger::recategorise`) saturates.
        assert_eq!(
            prev.staging_pool.since(&cur.staging_pool),
            PoolCounters::default()
        );
    }

    #[test]
    fn churn_line_reports_the_upload_arena() {
        // Over 2 s: two blocks allocated, two freed, two live; 2 MiB
        // served as 512 sub-allocations, and two oversize requests.
        let mut l = Ledger::default();
        for k in 0..2 {
            l.alloc(
                k,
                ec(256 * 1024, MemCategory::Staging, ChurnClass::UploadArena),
            );
        }
        let mut prev = churn_of(&l);
        prev.upload_arena = ArenaCounters {
            suballocs: 100,
            suballoc_bytes: 4096,
            dedicated: 7,
            idle_blocks: 9,
        };
        for k in 2..4 {
            l.alloc(
                k,
                ec(256 * 1024, MemCategory::Staging, ChurnClass::UploadArena),
            );
        }
        l.free(0);
        l.free(1);
        let mut cur = churn_of(&l);
        cur.upload_arena = ArenaCounters {
            suballocs: 612,
            suballoc_bytes: 4096 + 2 * 1024 * 1024,
            dedicated: 9,
            idle_blocks: 1,
        };
        let line = format_churn_line(&prev, &cur, 2.0, Some(PoolCounters::default()));
        assert!(
            line.contains(
                " staging[hit=0/s miss=0/s kept=0/s dropped=0/s] arena[blocks=2 idle=1 \
                 block_alloc=1/s block_free=1/s sub=256/s sub_in=1.00MiB/s dedicated=1/s] \
                 pixmap["
            ),
            "{line}"
        );
        // The blocks are also an ordinary churn class.
        assert!(line.contains(" upload_arena[alloc=1/s free=1/s "), "{line}");
    }

    #[test]
    fn fresh_relabel_moves_the_alloc_event() {
        // A redirect backing is allocated as a pixmap, then relabelled.
        let mut l = Ledger::default();
        l.alloc(1, ec(100, MemCategory::Pixmap, ChurnClass::PixmapLarge));
        l.recategorise(1, MemCategory::RedirectBacking);
        let s = churn_of(&l);
        assert_eq!(s.class(ChurnClass::PixmapLarge), ChurnTally::default());
        let r = s.class(ChurnClass::Redirect);
        assert_eq!((r.allocs, r.alloc_bytes, r.live.count), (1, 100, 1));
        l.free(1);
        let r = churn_of(&l).class(ChurnClass::Redirect);
        assert_eq!((r.frees, r.free_bytes, r.live.count), (1, 100, 0));
    }

    #[test]
    fn pool_park_and_reuse_keep_alloc_history() {
        let mut l = Ledger::default();
        l.alloc(1, ec(64, MemCategory::Pixmap, ChurnClass::PixmapSmall));
        // Returned to the pixmap pool: class unchanged, not fresh anymore.
        l.recategorise(1, MemCategory::PoolIdle);
        let s = churn_of(&l);
        assert_eq!(s.class(ChurnClass::PixmapSmall).allocs, 1);
        assert_eq!(s.class(ChurnClass::PixmapSmall).live.count, 1);
        // Taken back out as window storage: live moves, the old alloc stays.
        l.recategorise(1, MemCategory::WindowStorage);
        let s = churn_of(&l);
        assert_eq!(s.class(ChurnClass::PixmapSmall).allocs, 1);
        assert_eq!(s.class(ChurnClass::PixmapSmall).live.count, 0);
        assert_eq!(s.class(ChurnClass::Window).allocs, 0);
        assert_eq!(s.class(ChurnClass::Window).live.count, 1);
        // Back to a pixmap: `small` survives the round trip.
        l.recategorise(1, MemCategory::Pixmap);
        assert_eq!(churn_of(&l).class(ChurnClass::PixmapSmall).live.count, 1);
        l.free(1);
        let s = churn_of(&l);
        assert_eq!(s.class(ChurnClass::PixmapSmall).frees, 1);
        assert_eq!(s.live_count(), 0);
    }

    #[test]
    fn pool_sized_window_storage_later_counts_as_small_pixmap() {
        let mut l = Ledger::default();
        let mut w = ec(64, MemCategory::WindowStorage, ChurnClass::Window);
        w.small = true;
        l.alloc(1, w);
        l.recategorise(1, MemCategory::PoolIdle);
        l.recategorise(1, MemCategory::Pixmap);
        l.free(1);
        let s = churn_of(&l);
        assert_eq!(s.class(ChurnClass::Window).allocs, 1);
        assert_eq!(s.class(ChurnClass::PixmapSmall).frees, 1);
        assert_eq!(s.class(ChurnClass::PixmapLarge), ChurnTally::default());
    }

    #[test]
    fn category_defaults() {
        assert_eq!(
            ChurnClass::for_category(MemCategory::Staging, false),
            ChurnClass::StagingOther
        );
        assert_eq!(
            ChurnClass::for_category(MemCategory::Pixmap, true),
            ChurnClass::PixmapSmall
        );
        assert_eq!(
            ChurnClass::for_category(MemCategory::Pixmap, false),
            ChurnClass::PixmapLarge
        );
        assert_eq!(
            ChurnClass::for_category(MemCategory::RedirectExport, false),
            ChurnClass::DmabufExport
        );
        assert_eq!(
            ChurnClass::for_category(MemCategory::Dri3Import, false),
            ChurnClass::DmabufImport
        );
        for (i, c) in ChurnClass::ALL.iter().enumerate() {
            assert_eq!(c.index(), i);
        }
    }

    #[test]
    fn alloc_free_totals() {
        let mut l = Ledger::default();
        l.alloc(1, e(100, MemCategory::Pixmap, true));
        l.alloc(2, e(50, MemCategory::Pixmap, true));
        l.alloc(3, e(7, MemCategory::Staging, false));
        let t = l.totals;
        assert_eq!(
            t.device_local(MemCategory::Pixmap),
            Tally {
                bytes: 150,
                count: 2
            }
        );
        assert_eq!(t.host(MemCategory::Staging), Tally { bytes: 7, count: 1 });
        assert_eq!(t.host(MemCategory::Pixmap), Tally::default());
        l.free(1);
        assert_eq!(
            l.totals.device_local(MemCategory::Pixmap),
            Tally {
                bytes: 50,
                count: 1
            }
        );
        l.free(2);
        l.free(3);
        assert_eq!(l.totals, MemSnapshot::default());
        assert!(l.entries.is_empty());
    }

    #[test]
    fn unknown_free_is_noop() {
        let mut l = Ledger::default();
        l.alloc(1, e(100, MemCategory::Glyph, true));
        l.free(42);
        l.free(1);
        l.free(1);
        assert_eq!(l.totals, MemSnapshot::default());
    }

    #[test]
    fn recategorise_moves_bytes() {
        let mut l = Ledger::default();
        l.alloc(1, e(100, MemCategory::Pixmap, true));
        l.recategorise(1, MemCategory::PoolIdle);
        assert_eq!(l.totals.device_local(MemCategory::Pixmap), Tally::default());
        assert_eq!(
            l.totals.device_local(MemCategory::PoolIdle),
            Tally {
                bytes: 100,
                count: 1
            }
        );
        l.recategorise(1, MemCategory::PoolIdle);
        l.recategorise(9, MemCategory::Other);
        assert_eq!(l.totals.total_device_local_bytes(), 100);
        l.free(1);
        assert_eq!(l.totals, MemSnapshot::default());
    }

    #[test]
    fn double_alloc_replaces() {
        let mut l = Ledger::default();
        l.alloc(1, e(100, MemCategory::Pixmap, true));
        l.alloc(1, e(30, MemCategory::Scratch, false));
        assert_eq!(l.totals.device_local(MemCategory::Pixmap), Tally::default());
        assert_eq!(
            l.totals.host(MemCategory::Scratch),
            Tally {
                bytes: 30,
                count: 1
            }
        );
        assert_eq!(l.entries.len(), 1);
    }

    #[test]
    fn format_line_reports_untracked() {
        let mut l = Ledger::default();
        l.alloc(1, e(3 << 20, MemCategory::WindowStorage, true));
        l.alloc(2, e(1 << 20, MemCategory::Staging, false));
        let line = format_line(&l.totals, Some(10 << 20));
        assert!(line.starts_with("vram by use [1s]: window=3.0MiB/1 redirect=0.0MiB/0"));
        assert!(line.contains(" tracked_device_local=3.0MiB host=1.0MiB untracked=7.0MiB"));
        assert!(!format_line(&l.totals, None).contains("untracked"));
    }

    #[test]
    fn promotion_keeps_redirect_backings_distinct() {
        assert_eq!(
            export_category_for(Some(MemCategory::RedirectBacking)),
            MemCategory::RedirectExport
        );
        assert_eq!(
            export_category_for(Some(MemCategory::RedirectExport)),
            MemCategory::RedirectExport
        );
        for c in [
            MemCategory::Pixmap,
            MemCategory::WindowStorage,
            MemCategory::PoolIdle,
            MemCategory::TfpExport,
        ] {
            assert_eq!(export_category_for(Some(c)), MemCategory::TfpExport);
        }
        assert_eq!(export_category_for(None), MemCategory::TfpExport);
        assert_eq!(MemCategory::RedirectExport.label(), "redirect_export");
    }

    #[test]
    fn global_api_roundtrip() {
        // A handle value no driver will hand out in this test process.
        let mem = vk::DeviceMemory::from_raw(0xdead_beef_0000_0001);
        note_alloc(
            mem,
            4096,
            MemCategory::Scanout,
            true,
            ChurnClass::Scanout,
            false,
        );
        assert_eq!(category_of(mem), Some(MemCategory::Scanout));
        recategorise(mem, MemCategory::Other);
        assert_eq!(category_of(mem), Some(MemCategory::Other));
        note_free(mem);
        assert_eq!(category_of(mem), None);
        note_free(mem);
    }
}
