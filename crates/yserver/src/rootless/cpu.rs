//! CPU frames (spec §4.5, §4.5.2): an asynchronous readback of the backing's
//! damage into a canonical image, owned frames filled from a small pool.

use ash::vk;
use yserver_core::rootless::{Rect, ToplevelId};

use super::{
    frames::{FillPlan, POOL, WindowFrames},
    host::{CpuFrame, FrameSeq, RootlessHost},
};
use crate::kms::render::{
    engine::{PendingReadback, RenderEngine},
    gpu::GpuCore,
    store::{DamageSnapshot, DrawableId, DrawableStore, RegionSet},
};

/// A full-extent CPU copy of the backing at one generation.
struct Canonical {
    backing_gen: u64,
    width: u16,
    height: u16,
    pixels: Vec<u8>,
}

impl Canonical {
    fn stride(&self) -> usize {
        usize::from(self.width) * 4
    }
}

/// A frame buffer; `pixels` is `None` while the host holds it.
struct PoolBuffer {
    seq: FrameSeq,
    backing_gen: u64,
    pixels: Option<Vec<u8>>,
}

struct InFlight {
    backing_gen: u64,
    full: bool,
    extent: (u16, u16),
    snapshot: Option<DamageSnapshot>,
    readback: PendingReadback,
}

#[derive(Default)]
pub(crate) struct CpuState {
    canonical: Option<Canonical>,
    pool: Vec<PoolBuffer>,
    in_flight: Option<InFlight>,
    /// The next readback is full-extent: a new window or generation, or a
    /// dropped frame nothing at this generation was delivered after.
    needs_full: bool,
    backing: Option<DrawableId>,
}

/// What the tick needs of the backend, borrowed once.
pub(crate) struct Gpu<'a> {
    pub store: &'a mut DrawableStore,
    pub engine: &'a mut RenderEngine,
    pub gpu: &'a mut GpuCore,
}

fn to_rect(r: vk::Rect2D) -> Rect {
    Rect {
        x: r.offset.x,
        y: r.offset.y,
        width: u16::try_from(r.extent.width).unwrap_or(u16::MAX),
        height: u16::try_from(r.extent.height).unwrap_or(u16::MAX),
    }
}

fn full_rect((width, height): (u16, u16)) -> Rect {
    Rect {
        x: 0,
        y: 0,
        width,
        height,
    }
}

/// Copy `rects` of `src` (stride `stride`) into `dst`, rows clipped to it.
fn copy_rects(src: &[u8], dst: &mut [u8], stride: usize, rects: &[Rect]) {
    let height = src.len() / stride.max(1);
    for r in rects {
        let x0 = usize::try_from(r.x.max(0)).unwrap_or(0).min(stride / 4);
        let x1 = usize::try_from((r.x + i32::from(r.width)).max(0))
            .unwrap_or(0)
            .min(stride / 4);
        let y0 = usize::try_from(r.y.max(0)).unwrap_or(0).min(height);
        let y1 = usize::try_from((r.y + i32::from(r.height)).max(0))
            .unwrap_or(0)
            .min(height);
        if x0 >= x1 {
            continue;
        }
        for y in y0..y1 {
            let row = y * stride;
            dst[row + x0 * 4..row + x1 * 4].copy_from_slice(&src[row + x0 * 4..row + x1 * 4]);
        }
    }
}

impl CpuState {
    pub(crate) fn new() -> Self {
        Self {
            needs_full: true,
            ..Self::default()
        }
    }

    /// The backing changed allocation (or is first seen): start over.
    pub(crate) fn new_generation(&mut self, backing: DrawableId) {
        self.backing = Some(backing);
        self.canonical = None;
        self.needs_full = true;
    }

    /// The host made the native window again: a readback under way would
    /// be a frame for the window before, and the next one is whole.
    pub(crate) fn restart(&mut self) {
        drop(self.take_in_flight());
        self.needs_full = true;
    }

    pub(crate) fn backing(&self) -> Option<DrawableId> {
        self.backing
    }

    pub(crate) fn in_flight(&self) -> bool {
        self.in_flight.is_some()
    }

    /// A pool buffer is released or one more may be allocated.
    pub(crate) fn has_buffer(&self) -> bool {
        self.pool.len() < POOL || self.pool.iter().any(|b| b.pixels.is_some())
    }

    pub(crate) fn needs_full(&self) -> bool {
        self.needs_full || self.canonical.is_none()
    }

    /// Something to read: a full readback is owed, or the store holds
    /// damage of the backing.
    pub(crate) fn has_work(&self, store: &DrawableStore) -> bool {
        self.needs_full()
            || self.backing.is_some_and(|b| {
                store
                    .peek_presentation_damage(b)
                    .is_some_and(|s| !s.region.is_empty())
            })
    }

    /// The one predicate of the tick and `next_wakeup` (`scene_wants_scene`'s
    /// rule, compose.rs): disagreeing would spin the loop or strand paint.
    pub(crate) fn wants_frame(&self, frames: &WindowFrames, store: &DrawableStore) -> bool {
        frames.can_submit() && !self.in_flight() && self.has_buffer() && self.has_work(store)
    }

    /// `Dropped`: when nothing at this generation reached the host, the
    /// retry must be full whatever the store still holds (§4.5.1 liveness).
    pub(crate) fn on_dropped(&mut self, frames: &WindowFrames) {
        if frames.last_delivered().is_none() {
            self.needs_full = true;
        }
    }

    /// `Released`: the host gave a buffer back.
    pub(crate) fn on_released(&mut self, seq: FrameSeq, pixels: Vec<u8>) {
        if let Some(b) = self
            .pool
            .iter_mut()
            .find(|b| b.seq == seq && b.pixels.is_none())
        {
            b.pixels = Some(pixels);
        }
    }

    /// The readback to give up at teardown. The engine keeps its staging
    /// buffer until the copy's fence signalled.
    pub(crate) fn take_in_flight(&mut self) -> Option<PendingReadback> {
        self.in_flight.take().map(|f| f.readback)
    }

    /// Start a readback of the backing's damage (or all of it) when there is
    /// something to read. `true` when one started.
    pub(crate) fn start(&mut self, frames: &WindowFrames, g: &mut Gpu<'_>) -> bool {
        let Some(backing) = self.backing else {
            return false;
        };
        let Some(storage) = g.store.get(backing).map(|d| d.storage.extent) else {
            return false;
        };
        let extent = (
            u16::try_from(storage.width).unwrap_or(u16::MAX),
            u16::try_from(storage.height).unwrap_or(u16::MAX),
        );
        let snapshot = g
            .store
            .peek_presentation_damage(backing)
            .filter(|s| !s.region.is_empty());
        let full = self.needs_full();
        let rects: Vec<vk::Rect2D> = if full {
            vec![vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: storage,
            }]
        } else if let Some(snapshot) = &snapshot {
            snapshot.region.rects().to_vec()
        } else {
            return false;
        };
        match g.engine.begin_readback(g.store, g.gpu, backing, &rects) {
            Ok(readback) => {
                self.in_flight = Some(InFlight {
                    backing_gen: frames.backing_gen(),
                    full,
                    extent,
                    snapshot,
                    readback,
                });
                true
            }
            Err(e) => {
                log::warn!("rootless: readback of the backing failed: {e:?}");
                false
            }
        }
    }

    /// Finish a completed readback and hand the host a frame. `true` when
    /// the readback is done (delivered or discarded).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn complete(
        &mut self,
        id: ToplevelId,
        depth: u8,
        frames: &mut WindowFrames,
        host: &mut dyn RootlessHost,
        vk: &crate::kms::vk::device::VkContext,
    ) -> bool {
        let Some(in_flight) = self.in_flight.as_ref() else {
            return false;
        };
        match in_flight.readback.poll(vk) {
            Ok(true) => {}
            Ok(false) => return false,
            Err(e) => {
                log::warn!("rootless: readback status: {e:?}");
                self.in_flight = None;
                self.needs_full = true;
                return true;
            }
        }
        let in_flight = self.in_flight.take().expect("checked");
        if in_flight.backing_gen != frames.backing_gen() {
            // Stale: never delivered, its snapshot never acked (§4.5).
            self.needs_full = true;
            return true;
        }
        let (width, height) = in_flight.extent;
        let reuse = self.canonical.as_ref().is_some_and(|c| {
            c.backing_gen == in_flight.backing_gen && (c.width, c.height) == (width, height)
        });
        if !reuse {
            self.canonical = Some(Canonical {
                backing_gen: in_flight.backing_gen,
                width,
                height,
                pixels: vec![0; usize::from(width) * usize::from(height) * 4],
            });
        }
        let canonical = self.canonical.as_mut().expect("set above");
        let stride = canonical.stride();
        if let Err(e) = in_flight.readback.write_into(&mut canonical.pixels, stride) {
            log::warn!("rootless: readback copy failed: {e:?}");
            self.needs_full = true;
            return true;
        }
        if in_flight.full {
            self.needs_full = false;
        }
        let damage = if in_flight.full {
            vec![full_rect(in_flight.extent)]
        } else {
            in_flight.readback.regions().map(to_rect).collect()
        };

        // A released buffer of this generation (newest content first), any
        // released one, or a new one.
        let backing_gen = in_flight.backing_gen;
        let pick = self
            .pool
            .iter()
            .enumerate()
            .filter(|(_, b)| b.pixels.is_some())
            .max_by_key(|(_, b)| (b.backing_gen == backing_gen, b.seq))
            .map(|(i, _)| i);
        let (index, known) = match pick {
            Some(i) => (i, Some((self.pool[i].seq, self.pool[i].backing_gen))),
            None if self.pool.len() < POOL => {
                self.pool.push(PoolBuffer {
                    seq: FrameSeq(0),
                    backing_gen,
                    pixels: Some(Vec::new()),
                });
                (self.pool.len() - 1, None)
            }
            None => {
                // The canonical image holds the content; the store snapshot
                // was not acked, so the next readback re-covers it.
                log::warn!("rootless: no free frame buffer");
                return true;
            }
        };
        let seq = frames.submitted(damage, in_flight.snapshot.as_ref().map(|s| s.epoch));
        let mut pixels = self.pool[index].pixels.take().expect("a free buffer");
        let canonical = self.canonical.as_ref().expect("written above");
        let plan = if pixels.len() == canonical.pixels.len() {
            frames.fill_plan(known, seq)
        } else {
            FillPlan::Full
        };
        match plan {
            FillPlan::Full => {
                pixels.clear();
                pixels.extend_from_slice(&canonical.pixels);
            }
            FillPlan::Rects(rects) => copy_rects(&canonical.pixels, &mut pixels, stride, &rects),
        }
        self.pool[index].seq = seq;
        self.pool[index].backing_gen = backing_gen;
        host.submit_cpu(CpuFrame {
            id,
            seq,
            backing_gen,
            width,
            height,
            stride: u32::try_from(stride).unwrap_or(u32::MAX),
            depth,
            pixels,
            damage: frames.frame_damage(seq, full_rect(in_flight.extent)),
        });
        true
    }
}

/// `Delivered`: ack the store with the newest held snapshot (§4.5).
pub(crate) fn ack(store: &mut DrawableStore, backing: Option<DrawableId>, epoch: Option<u64>) {
    if let (Some(id), Some(epoch)) = (backing, epoch) {
        store.ack_presentation_damage(DamageSnapshot {
            id,
            epoch,
            region: RegionSet::new(),
        });
    }
}
