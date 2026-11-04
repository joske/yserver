//! A `VulkanSurface` window's per-tick work (spec §4.5, §4.5.1 GPU row):
//! its surface once set up, the swapchain, the full-extent present.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use ash::vk;
use yserver_core::rootless::{Rect, ToplevelId};

use super::{
    cpu,
    frames::{Readiness, WindowFrames},
    host::RootlessHost,
    window_swapchain::{Presented, Retiring, WindowSwapchain},
};
use crate::kms::{
    render::{
        gpu::GpuCore,
        store::{DrawableId, DrawableStore},
    },
    vk::device::VkContext,
};

pub(crate) struct GpuState {
    vk: Option<Arc<VkContext>>,
    /// The window, once it has a surface.
    id: Option<ToplevelId>,
    surface: Option<vk::SurfaceKHR>,
    pub(crate) swapchain: Option<WindowSwapchain>,
    backing: Option<DrawableId>,
    /// A frame is owed whatever the store says: a new window or generation.
    needs_frame: bool,
    /// The host could not make a surface.
    failed: bool,
    /// After an out-of-date surface: when to try again, and the delay.
    retry: Option<(Instant, Duration)>,
}

/// The first retry after an out-of-date surface; it doubles up to the cap.
const FIRST_RETRY: Duration = Duration::from_millis(16);
const MAX_RETRY: Duration = Duration::from_millis(250);

/// What the tick needs of the backend.
pub(crate) struct GpuTick<'a> {
    pub vk: &'a Arc<VkContext>,
    pub store: &'a mut DrawableStore,
    pub gpu: &'a GpuCore,
    pub maintenance1: bool,
    pub retiring: &'a mut Vec<Retiring>,
}

impl GpuState {
    pub(crate) fn new() -> Self {
        Self {
            vk: None,
            id: None,
            surface: None,
            swapchain: None,
            backing: None,
            needs_frame: true,
            failed: false,
            retry: None,
        }
    }

    /// A host event (resize, `SetupDone`, `Ready`): try again at once.
    pub(crate) fn host_event(&mut self) {
        self.retry = None;
    }

    pub(crate) fn new_generation(&mut self, backing: DrawableId) {
        self.backing = Some(backing);
        self.needs_frame = true;
        self.retry = None;
    }

    pub(crate) fn has_work(&self, store: &DrawableStore) -> bool {
        self.needs_frame
            || self.backing.is_some_and(|b| {
                store
                    .peek_presentation_damage(b)
                    .is_some_and(|s| !s.region.is_empty())
            })
    }

    /// The tick's and `next_wakeup`'s predicate.
    pub(crate) fn wants_frame(
        &self,
        frames: &WindowFrames,
        store: &DrawableStore,
        now: Instant,
    ) -> bool {
        self.retry.is_none_or(|(at, _)| at <= now) && self.could_present(frames, store)
    }

    /// A frame waits only for the backoff: when to try again.
    pub(crate) fn retry_at(&self, frames: &WindowFrames, store: &DrawableStore) -> Option<Instant> {
        self.retry
            .map(|(at, _)| at)
            .filter(|_| self.could_present(frames, store))
    }

    fn could_present(&self, frames: &WindowFrames, store: &DrawableStore) -> bool {
        frames.can_submit() && !self.failed && self.backing.is_some() && self.has_work(store)
    }

    /// Present a frame when one is wanted; `true` when one was shown.
    pub(crate) fn tick(
        &mut self,
        id: ToplevelId,
        depth: u8,
        frames: &mut WindowFrames,
        host: &mut dyn RootlessHost,
        t: &mut GpuTick<'_>,
    ) -> bool {
        if self.surface.is_none() && frames.readiness() != Readiness::Setup && !self.failed {
            match host.create_surface(id, t.vk) {
                Ok(surface) => {
                    self.surface = Some(surface);
                    self.vk = Some(Arc::clone(t.vk));
                    self.id = Some(id);
                }
                Err(e) => {
                    log::warn!("rootless: no surface for {id:?}: {e}");
                    self.failed = true;
                }
            }
        }
        let (Some(surface), Some(backing)) = (self.surface, self.backing) else {
            return false;
        };
        if !self.wants_frame(frames, t.store, Instant::now()) {
            return false;
        }
        if self.swapchain.is_none() {
            match WindowSwapchain::new(t.vk, surface, depth, t.maintenance1) {
                Ok(sc) => self.swapchain = Some(sc),
                Err(e) => {
                    log::warn!("rootless: swapchain setup for {id:?}: {e:?}");
                    self.failed = true;
                    return false;
                }
            }
        }
        let sc = self.swapchain.as_mut().expect("set above");
        let snapshot = t
            .store
            .peek_presentation_damage(backing)
            .filter(|s| !s.region.is_empty());
        let extent = t.store.get(backing).map(|d| d.storage.extent);
        sc.set_copy_scale(host.copy_scale());
        if let Some(e) = extent.filter(|&e| sc.rebuild_due(e, frames.backing_gen())) {
            host.surface_resizing(id, (e.width, e.height));
        }
        let result = sc.present(t.gpu, t.store, backing, frames.backing_gen());
        t.retiring.extend(sc.take_replaced());
        let rebuilt = sc.take_rebuilt();
        let shown = match result {
            Ok(Presented::Shown) => {
                let extent = extent.unwrap_or_default();
                let full = Rect {
                    x: 0,
                    y: 0,
                    width: u16::try_from(extent.width).unwrap_or(u16::MAX),
                    height: u16::try_from(extent.height).unwrap_or(u16::MAX),
                };
                // A successful present is the delivery (§4.5 ack rule).
                let seq = frames.submitted(vec![full], snapshot.map(|s| s.epoch));
                let delivery = frames.on_delivered(seq);
                cpu::ack(t.store, Some(backing), delivery.ack_epoch);
                if delivery.first {
                    host.first_frame(id);
                }
                self.needs_frame = false;
                self.retry = None;
                true
            }
            Ok(Presented::NotReady) => {
                frames.wait_for_ready();
                false
            }
            // Rebuilt next time; a surface that stays out of date must not
            // spin the loop.
            Ok(Presented::OutOfDate) => {
                let delay = self
                    .retry
                    .map_or(FIRST_RETRY, |(_, last)| (last * 2).min(MAX_RETRY));
                log::debug!("rootless: {id:?} out of date, retrying in {delay:?}");
                self.retry = Some((Instant::now() + delay, delay));
                false
            }
            Err(e) => {
                // Not acked; the host's next `Ready` retries (§4.5.1).
                log::warn!("rootless: present for {id:?} failed: {e:?}");
                frames.wait_for_ready();
                false
            }
        };
        if rebuilt {
            host.surface_rebuilt(id);
        }
        shown
    }

    /// `Unmapped`: everything to retirement, the surface last.
    pub(crate) fn retire(mut self, id: ToplevelId) -> Vec<Retiring> {
        self.take_retiring(id)
    }

    fn take_retiring(&mut self, id: ToplevelId) -> Vec<Retiring> {
        match (self.swapchain.take(), self.surface.take(), self.vk.take()) {
            (Some(sc), _, _) => sc.retire(Some(id)),
            (None, Some(surface), Some(vk)) => vec![Retiring::surface_only(&vk, id, surface)],
            _ => Vec::new(),
        }
    }
}

impl Drop for GpuState {
    /// Presenter teardown: the retirements finish (one queue-idle) as they drop.
    fn drop(&mut self) {
        if let Some(id) = self.id {
            drop(self.take_retiring(id));
        }
    }
}
