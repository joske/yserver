//! Per-frame upload arena for small per-request upload data (#177).
//!
//! Every glyph run, ImageText request and trapezoid/triangle request needs a
//! few hundred bytes to a few KiB of host-written instance/vertex data, and
//! every newly interned glyph a few hundred bytes of atlas-upload staging.
//! All of it is read by the command buffer of the frame that recorded it and
//! by nothing else. Giving each request its own `VkDeviceMemory` made these
//! sites 85–98% of all `vkAllocateMemory`/`vkFreeMemory` calls; on RADV every
//! allocation and free walks libdrm's process-wide VA hole list
//! (`amdgpu_va_range_alloc2` / `amdgpu_vamgr_free_va`), and every live
//! allocation is a range that makes the others dearer. A size-classed reuse
//! pool (fcbc89aa) still kept one allocation per in-flight request: a
//! text-heavy client issues more of them per frame than a pool can keep idle.
//!
//! Instead, each open frame bump-allocates its upload data from large blocks
//! ([`BLOCK_BYTES`]), chaining a new block when the current one fills.
//! A request larger than a block gets a dedicated block of its own.
//!
//! **Lifetime.** A frame's blocks are owned by its [`FrameUploads`], which
//! lives in the frame's pin set and moves with it from the open frame to the
//! `pending_frames` queue at close. [`UploadArena::retire`] is called only by
//! the engine's `pending_frames` retire walk, after the frame's fence has
//! signalled, so a block handed out again by [`UploadArena::alloc`] is never
//! read by any submitted work. A frame never shares a block with another
//! frame: each frame starts on a block of its own and bumps only within the
//! blocks it owns. On any path where a frame is torn down without retiring
//! (close failure, shutdown), its `FrameUploads` simply drops and the blocks
//! are destroyed, exactly like any other pinned resource on that path.
//!
//! **Bounds.** At most [`IDLE_CAP`] blocks sit idle ([`IDLE_CAP`] ×
//! [`BLOCK_BYTES`] = 4 MiB); a block returned beyond the cap is destroyed,
//! and one idle for [`IDLE_EVICT_AFTER`] is destroyed by
//! [`UploadArena::trim`]. `alloc` reuses the most recently returned block,
//! so only the surplus over the working set ages out.
//!
//! The arena is generic over the block type so the bookkeeping is testable
//! without a Vulkan device; the engine instantiates it with its mapped,
//! host-coherent `StagingBuffer`.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

/// Size of one shared block. A block is one `VkDeviceMemory`.
///
/// 256 KiB covers a whole busy frame in one block: the #177 reporter's
/// peak was ~3650 glyph-run requests/s, every one ≤ 4 KiB, which at the
/// ~60 frame closes/s of a busy session is ~60 requests and ≤ 240 KiB per
/// frame (typical runs are ~1 KiB, so usually far less). Larger blocks
/// would only raise the floor every open frame holds and the idle
/// footprint; smaller ones would chain several per busy frame. A request
/// above this size is served by a dedicated allocation.
pub(crate) const BLOCK_BYTES: u64 = 256 * 1024;

/// Idle blocks kept for reuse. Covers the blocks of the frames in flight
/// at once (each frame holds at least one block while open and until its
/// fence signals), with headroom for bursts. 16 × 256 KiB = 4 MiB.
pub(crate) const IDLE_CAP: usize = 16;

/// An idle block older than this is destroyed by [`UploadArena::trim`].
pub(crate) const IDLE_EVICT_AFTER: Duration = Duration::from_secs(2);

/// Which kind of block `alloc` asks its constructor for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlockKind {
    /// A shared block of [`BLOCK_BYTES`], recycled through the idle list.
    Shared,
    /// A block sized for one oversize request, destroyed at retire.
    Dedicated,
}

/// Where a sub-allocation landed within its frame's blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Placement {
    /// Index into the frame's shared blocks.
    Shared(usize),
    /// Index into the frame's dedicated blocks.
    Dedicated(usize),
}

/// One sub-allocation: `size` bytes at `offset` in the block at
/// `placement`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SubAlloc {
    pub(crate) placement: Placement,
    pub(crate) offset: u64,
}

/// The blocks one frame owns, and its bump cursor in the last shared one.
#[derive(Debug)]
pub(crate) struct FrameUploads<B> {
    /// Shared blocks in chain order; the last is the one being filled.
    shared: Vec<B>,
    /// Bytes used in the last shared block.
    head_used: u64,
    /// Oversize requests, one block each.
    dedicated: Vec<B>,
}

impl<B> Default for FrameUploads<B> {
    fn default() -> Self {
        Self {
            shared: Vec::new(),
            head_used: 0,
            dedicated: Vec::new(),
        }
    }
}

impl<B> FrameUploads<B> {
    /// The block a sub-allocation lives in.
    pub(crate) fn block(&self, placement: Placement) -> &B {
        match placement {
            Placement::Shared(i) => &self.shared[i],
            Placement::Dedicated(i) => &self.dedicated[i],
        }
    }

    /// Shared blocks this frame owns.
    #[cfg(test)]
    pub(crate) fn shared_len(&self) -> usize {
        self.shared.len()
    }

    /// Dedicated blocks this frame owns.
    #[cfg(test)]
    pub(crate) fn dedicated_len(&self) -> usize {
        self.dedicated.len()
    }
}

/// Lifetime counters, logged at shutdown.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UploadArenaStats {
    /// Sub-allocations served from a shared block.
    pub(crate) suballocs: u64,
    /// Bytes of those sub-allocations (before alignment padding).
    pub(crate) suballoc_bytes: u64,
    /// Oversize requests served by a dedicated block.
    pub(crate) dedicated: u64,
    /// Shared blocks freshly constructed (an allocation).
    pub(crate) block_allocs: u64,
    /// Shared blocks taken from the idle list (no allocation).
    pub(crate) block_reuses: u64,
    /// Retired shared blocks kept on the idle list.
    pub(crate) returned: u64,
    /// Retired shared blocks destroyed because the idle list was full.
    pub(crate) rejected: u64,
    /// Idle blocks destroyed by `trim` for being idle too long.
    pub(crate) evicted: u64,
}

/// The idle shared blocks, most recently returned at the back.
pub(crate) struct UploadArena<B> {
    idle: VecDeque<(B, Instant)>,
    stats: UploadArenaStats,
}

impl<B> Default for UploadArena<B> {
    fn default() -> Self {
        Self {
            idle: VecDeque::new(),
            stats: UploadArenaStats::default(),
        }
    }
}

/// `offset` rounded up to `align`, a power of two.
fn align_up(offset: u64, align: u64) -> u64 {
    debug_assert!(
        align.is_power_of_two(),
        "alignment {align} is not a power of two"
    );
    (offset + align - 1) & !(align - 1)
}

impl<B> UploadArena<B> {
    /// Reserve `size` bytes (at least 1) at an `align`-aligned offset in
    /// `frame`'s blocks. `align` must be a power of two no larger than
    /// [`BLOCK_BYTES`]. When the current block cannot fit the request, the
    /// frame chains a block from the idle list, or from
    /// `new_block(BLOCK_BYTES, BlockKind::Shared)` when the list is empty;
    /// a request above [`BLOCK_BYTES`] gets
    /// `new_block(size, BlockKind::Dedicated)` at offset 0. If `new_block`
    /// fails, `frame` is unchanged.
    pub(crate) fn alloc<E>(
        &mut self,
        frame: &mut FrameUploads<B>,
        size: u64,
        align: u64,
        new_block: impl FnOnce(u64, BlockKind) -> Result<B, E>,
    ) -> Result<SubAlloc, E> {
        let size = size.max(1);
        debug_assert!(align <= BLOCK_BYTES);
        if size > BLOCK_BYTES {
            let block = new_block(size, BlockKind::Dedicated)?;
            frame.dedicated.push(block);
            self.stats.dedicated += 1;
            return Ok(SubAlloc {
                placement: Placement::Dedicated(frame.dedicated.len() - 1),
                offset: 0,
            });
        }
        if !frame.shared.is_empty() {
            let offset = align_up(frame.head_used, align);
            if offset + size <= BLOCK_BYTES {
                frame.head_used = offset + size;
                self.note_suballoc(size);
                return Ok(SubAlloc {
                    placement: Placement::Shared(frame.shared.len() - 1),
                    offset,
                });
            }
        }
        let block = if let Some((block, _)) = self.idle.pop_back() {
            self.stats.block_reuses += 1;
            block
        } else {
            let block = new_block(BLOCK_BYTES, BlockKind::Shared)?;
            self.stats.block_allocs += 1;
            block
        };
        frame.shared.push(block);
        frame.head_used = size;
        self.note_suballoc(size);
        Ok(SubAlloc {
            placement: Placement::Shared(frame.shared.len() - 1),
            offset: 0,
        })
    }

    fn note_suballoc(&mut self, size: u64) {
        self.stats.suballocs += 1;
        self.stats.suballoc_bytes += size;
    }

    /// Take back a retired frame's blocks. **The caller guarantees the GPU
    /// is done with every one of them** (the frame's fence has signalled).
    /// Shared blocks go to the idle list up to [`IDLE_CAP`]; the rest, and
    /// every dedicated block, are dropped (destroyed).
    pub(crate) fn retire(&mut self, frame: FrameUploads<B>, now: Instant) {
        for block in frame.shared {
            if self.idle.len() >= IDLE_CAP {
                self.stats.rejected += 1;
                continue; // `block` drops here
            }
            self.stats.returned += 1;
            self.idle.push_back((block, now));
        }
        // `frame.dedicated` drops here.
    }

    /// Destroy every idle block returned [`IDLE_EVICT_AFTER`] or longer
    /// ago.
    pub(crate) fn trim(&mut self, now: Instant) {
        while self
            .idle
            .front()
            .is_some_and(|(_, at)| now.saturating_duration_since(*at) >= IDLE_EVICT_AFTER)
        {
            self.idle.pop_front();
            self.stats.evicted += 1;
        }
    }

    /// Idle blocks currently held.
    pub(crate) fn idle_len(&self) -> usize {
        self.idle.len()
    }

    pub(crate) fn stats(&self) -> UploadArenaStats {
        self.stats
    }

    /// Destroy every idle block. Call when nothing can be in flight.
    pub(crate) fn drain(&mut self) {
        self.idle.clear();
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use super::*;

    /// Block that records its size and kind and counts its destruction.
    #[derive(Debug)]
    struct Probe {
        id: u32,
        bytes: u64,
        kind: BlockKind,
        drops: Rc<Cell<u32>>,
    }

    impl Drop for Probe {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
        }
    }

    /// Block constructor that hands out sequential ids and counts calls.
    struct Maker {
        next: Cell<u32>,
        drops: Rc<Cell<u32>>,
    }

    impl Maker {
        fn new() -> Self {
            Self {
                next: Cell::new(0),
                drops: Rc::new(Cell::new(0)),
            }
        }

        fn make(&self) -> impl FnOnce(u64, BlockKind) -> Result<Probe, ()> + '_ {
            move |bytes, kind| {
                let id = self.next.get();
                self.next.set(id + 1);
                Ok(Probe {
                    id,
                    bytes,
                    kind,
                    drops: Rc::clone(&self.drops),
                })
            }
        }

        fn made(&self) -> u32 {
            self.next.get()
        }
    }

    #[test]
    fn bump_allocates_aligned_offsets_in_one_block() {
        let m = Maker::new();
        let mut arena = UploadArena::default();
        let mut frame = FrameUploads::default();
        let a = arena.alloc(&mut frame, 36, 16, m.make()).expect("a");
        let b = arena.alloc(&mut frame, 100, 16, m.make()).expect("b");
        let c = arena.alloc(&mut frame, 3, 4, m.make()).expect("c");
        let d = arena.alloc(&mut frame, 0, 256, m.make()).expect("d");
        assert_eq!(
            a,
            SubAlloc {
                placement: Placement::Shared(0),
                offset: 0
            }
        );
        assert_eq!(b.offset, 48, "36 rounded up to 16");
        assert_eq!(c.offset, 148, "148 is already a multiple of 4");
        assert_eq!(
            d.offset, 256,
            "151 rounded up to 256; a 0-byte request takes 1 byte"
        );
        for s in [b, c, d] {
            assert_eq!(s.placement, Placement::Shared(0));
        }
        assert_eq!(m.made(), 1, "one block for all four");
        assert_eq!(frame.block(Placement::Shared(0)).bytes, BLOCK_BYTES);
        assert_eq!(frame.block(Placement::Shared(0)).kind, BlockKind::Shared);
        let s = arena.stats();
        assert_eq!((s.suballocs, s.suballoc_bytes, s.block_allocs), (4, 140, 1));
    }

    #[test]
    fn full_block_chains_a_new_one() {
        let m = Maker::new();
        let mut arena = UploadArena::default();
        let mut frame = FrameUploads::default();
        let a = arena
            .alloc(&mut frame, BLOCK_BYTES - 8, 16, m.make())
            .expect("a");
        // Exactly fills the rest of block 0.
        let b = arena.alloc(&mut frame, 8, 8, m.make()).expect("b");
        assert_eq!(a.offset, 0);
        assert_eq!(
            b,
            SubAlloc {
                placement: Placement::Shared(0),
                offset: BLOCK_BYTES - 8
            }
        );
        // Nothing fits any more: chain block 1, starting at offset 0.
        let c = arena.alloc(&mut frame, 1, 1, m.make()).expect("c");
        assert_eq!(
            c,
            SubAlloc {
                placement: Placement::Shared(1),
                offset: 0
            }
        );
        // Padding that would cross the end also chains.
        let d = arena
            .alloc(&mut frame, BLOCK_BYTES - 16, 16, m.make())
            .expect("d");
        assert_eq!(
            d,
            SubAlloc {
                placement: Placement::Shared(1),
                offset: 16
            }
        );
        let e = arena.alloc(&mut frame, 1, 16, m.make()).expect("e");
        assert_eq!(
            e,
            SubAlloc {
                placement: Placement::Shared(2),
                offset: 0
            }
        );
        assert_eq!(frame.shared_len(), 3);
        assert_eq!(m.made(), 3);
    }

    #[test]
    fn oversize_request_gets_a_dedicated_block_that_is_not_recycled() {
        let m = Maker::new();
        let mut arena = UploadArena::default();
        let mut frame = FrameUploads::default();
        let a = arena.alloc(&mut frame, 100, 16, m.make()).expect("a");
        let big = arena
            .alloc(&mut frame, BLOCK_BYTES + 1, 16, m.make())
            .expect("big");
        assert_eq!(
            big,
            SubAlloc {
                placement: Placement::Dedicated(0),
                offset: 0
            }
        );
        let block = frame.block(big.placement);
        assert_eq!(
            (block.bytes, block.kind),
            (BLOCK_BYTES + 1, BlockKind::Dedicated)
        );
        // The shared head is untouched by the oversize request.
        let b = arena.alloc(&mut frame, 100, 16, m.make()).expect("b");
        assert_eq!(
            b,
            SubAlloc {
                placement: Placement::Shared(0),
                offset: 112
            }
        );
        assert_eq!(a.placement, Placement::Shared(0));
        // A request of exactly one block is still shared.
        let whole = arena
            .alloc(&mut frame, BLOCK_BYTES, 16, m.make())
            .expect("whole");
        assert_eq!(
            whole,
            SubAlloc {
                placement: Placement::Shared(1),
                offset: 0
            }
        );
        assert_eq!(arena.stats().dedicated, 1);

        arena.retire(frame, Instant::now());
        assert_eq!(
            m.drops.get(),
            1,
            "the dedicated block is destroyed at retire"
        );
        assert_eq!(arena.idle_len(), 2, "both shared blocks are kept");
    }

    #[test]
    fn failed_block_construction_leaves_the_frame_unchanged() {
        let mut arena: UploadArena<Probe> = UploadArena::default();
        let mut frame = FrameUploads::default();
        let r = arena.alloc(&mut frame, 10, 4, |_, _| Err("oom"));
        assert_eq!(r, Err("oom"));
        let r = arena.alloc(&mut frame, BLOCK_BYTES * 2, 4, |_, _| Err("oom"));
        assert_eq!(r, Err("oom"));
        assert_eq!((frame.shared_len(), frame.dedicated_len()), (0, 0));
        assert_eq!(arena.stats(), UploadArenaStats::default());
    }

    #[test]
    fn a_block_is_reused_only_after_its_frame_retires() {
        let m = Maker::new();
        let mut arena = UploadArena::default();
        let mut f1 = FrameUploads::default();
        arena.alloc(&mut f1, 64, 16, m.make()).expect("f1");
        let f1_block = f1.block(Placement::Shared(0)).id;

        // f1 is closed and in flight (not retired): a second frame must
        // never land in f1's block, not even past f1's cursor.
        let mut f2 = FrameUploads::default();
        let s2 = arena.alloc(&mut f2, 64, 16, m.make()).expect("f2");
        assert_eq!(s2.offset, 0);
        assert_ne!(
            f2.block(s2.placement).id,
            f1_block,
            "in-flight block handed out"
        );
        assert_eq!(m.made(), 2);

        // f1's fence signals: its block goes idle and the next frame
        // reuses it from offset 0 without constructing anything.
        arena.retire(f1, Instant::now());
        assert_eq!(arena.idle_len(), 1);
        let mut f3 = FrameUploads::default();
        let s3 = arena.alloc(&mut f3, 64, 16, m.make()).expect("f3");
        assert_eq!(s3.offset, 0);
        assert_eq!(f3.block(s3.placement).id, f1_block);
        assert_eq!(m.made(), 2, "reused, not constructed");
        assert_eq!(arena.idle_len(), 0);
        let s = arena.stats();
        assert_eq!((s.block_allocs, s.block_reuses, s.returned), (2, 1, 1));
        assert_eq!(m.drops.get(), 0);
    }

    #[test]
    fn alloc_reuses_the_most_recently_returned_block() {
        let m = Maker::new();
        let mut arena = UploadArena::default();
        let t0 = Instant::now();
        let mut f1 = FrameUploads::default();
        arena.alloc(&mut f1, 1, 1, m.make()).expect("f1");
        let mut f2 = FrameUploads::default();
        arena.alloc(&mut f2, 1, 1, m.make()).expect("f2");
        arena.retire(f1, t0);
        arena.retire(f2, t0 + Duration::from_millis(10));
        let mut f3 = FrameUploads::default();
        arena.alloc(&mut f3, 1, 1, m.make()).expect("f3");
        assert_eq!(f3.block(Placement::Shared(0)).id, 1, "warmest block first");
    }

    #[test]
    fn retire_beyond_the_idle_cap_destroys_the_surplus() {
        let m = Maker::new();
        let mut arena = UploadArena::default();
        let mut frame = FrameUploads::default();
        // One frame chains IDLE_CAP + 3 blocks.
        for _ in 0..IDLE_CAP + 3 {
            arena
                .alloc(&mut frame, BLOCK_BYTES, 16, m.make())
                .expect("block");
        }
        assert_eq!(frame.shared_len(), IDLE_CAP + 3);
        arena.retire(frame, Instant::now());
        assert_eq!(arena.idle_len(), IDLE_CAP);
        assert_eq!(m.drops.get(), 3);
        assert_eq!(arena.stats().rejected, 3);
    }

    #[test]
    fn trim_evicts_only_blocks_idle_past_the_limit() {
        let m = Maker::new();
        let mut arena = UploadArena::default();
        let t0 = Instant::now();
        for at in [t0, t0, t0 + Duration::from_secs(1)] {
            let mut f = FrameUploads::default();
            arena.alloc(&mut f, 1, 1, m.make()).expect("alloc");
            arena.retire(f, at);
        }
        // Each frame took the block the previous one returned: one block.
        assert_eq!(arena.idle_len(), 1);
        assert_eq!(m.made(), 1);

        // Two frames in flight at once need two blocks; both go idle.
        let mut fa = FrameUploads::default();
        let mut fb = FrameUploads::default();
        arena.alloc(&mut fa, 1, 1, m.make()).expect("fa");
        arena.alloc(&mut fb, 1, 1, m.make()).expect("fb");
        arena.retire(fa, t0);
        arena.retire(fb, t0 + Duration::from_secs(1));
        assert_eq!(arena.idle_len(), 2);

        arena.trim(t0 + IDLE_EVICT_AFTER - Duration::from_millis(1));
        assert_eq!(arena.idle_len(), 2, "nothing is old enough yet");
        arena.trim(t0 + IDLE_EVICT_AFTER);
        assert_eq!(arena.idle_len(), 1, "the block returned at t0 is evicted");
        assert_eq!(m.drops.get(), 1);
        arena.trim(t0 + Duration::from_secs(1) + IDLE_EVICT_AFTER);
        assert_eq!(arena.idle_len(), 0, "an idle server holds no blocks");
        assert_eq!(arena.stats().evicted, 2);
    }

    #[test]
    fn drain_destroys_every_idle_block() {
        let m = Maker::new();
        let mut arena = UploadArena::default();
        let mut frame = FrameUploads::default();
        for _ in 0..3 {
            arena
                .alloc(&mut frame, BLOCK_BYTES, 16, m.make())
                .expect("block");
        }
        arena.retire(frame, Instant::now());
        arena.drain();
        assert_eq!(arena.idle_len(), 0);
        assert_eq!(m.drops.get(), 3);
    }
}
