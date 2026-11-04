//! The window's size to the core: `WM_SIZE` reports the client area in
//! device pixels, the metrics turn it into X pixels, `ResizeCoalescer`
//! picks what to post (a modal size loop is a live resize), and the
//! pointer clamp follows what was posted. A DPI or monitor change posts at
//! once when it changes the size, the copy factor or the millimetres. The
//! swapchain takes the client area's size by itself. UI thread only.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use super::{input::ShellInput, scale::Metrics};
use crate::surface::{
    link::{ShellEvent, ShellLink},
    resize::{ResizeCoalescer, Size},
};

pub(super) struct Sizer {
    link: Arc<ShellLink>,
    /// The token of the core's window request in progress (0: none).
    request: Arc<AtomicU64>,
    /// A size waits for the next frame-clock tick.
    tick_wanted: Arc<AtomicBool>,
    coalescer: ResizeCoalescer,
    /// The window's scale and monitor, as last seen.
    metrics: Metrics,
    /// The copy factor and millimetres last posted.
    posted: (u32, (u32, u32)),
}

impl Sizer {
    /// The core starts at `start` with `metrics`; `tick_wanted` is the
    /// frame clock hook's.
    pub(super) fn new(
        start: Size,
        metrics: Metrics,
        link: Arc<ShellLink>,
        request: Arc<AtomicU64>,
        tick_wanted: Arc<AtomicBool>,
    ) -> Self {
        Self {
            link,
            request,
            tick_wanted,
            coalescer: ResizeCoalescer::new(start),
            metrics,
            posted: (metrics.factor(), metrics.mm(start)),
        }
    }

    pub(super) fn metrics(&self) -> Metrics {
        self.metrics
    }

    fn post(&mut self, input: &ShellInput, size: Size, request: Option<u64>) {
        let scale = self.metrics.factor();
        let mm = self.metrics.mm(size);
        input.set_fb(size);
        input.set_factor(scale);
        log::debug!(
            "windows: window {}x{} scale {scale} {}x{} mm (request {request:?})",
            size.0,
            size.1,
            mm.0,
            mm.1
        );
        self.posted = (scale, mm);
        self.link.post(ShellEvent::Resized {
            width: size.0,
            height: size.1,
            scale,
            mm,
            request,
        });
    }

    /// The client area is now `device` pixels: the core hears of it when
    /// the coalescer says.
    pub(super) fn resized(&mut self, input: &ShellInput, device: (i32, i32)) {
        if self.request.load(Ordering::Acquire) != 0 {
            // `answer_request` posts the outcome.
            return;
        }
        let size = self.metrics.scale.to_x(device);
        if let Some(size) = self.coalescer.resized(size) {
            self.post(input, size, None);
        } else if self.coalescer.waiting() {
            self.tick_wanted.store(true, Ordering::Release);
        }
    }

    /// The window's DPI or monitor is now `metrics`, before the window takes
    /// its new size (`WM_SIZE` converts with them).
    pub(super) fn set_metrics(&mut self, metrics: Metrics) {
        self.metrics = metrics;
    }

    /// After a DPI or monitor change, with the client area at `device`
    /// pixels: the core hears at once of a new size, copy factor or
    /// millimetres, even mid live resize.
    pub(super) fn metrics_changed(&mut self, input: &ShellInput, device: (i32, i32)) {
        if self.request.load(Ordering::Acquire) != 0 {
            // `answer_request` posts with the new metrics.
            return;
        }
        let size = self.metrics.scale.to_x(device);
        if size == self.coalescer.posted()
            && self.posted == (self.metrics.factor(), self.metrics.mm(size))
        {
            return;
        }
        let size = self.coalescer.post_now(size);
        self.post(input, size, None);
    }

    /// The modal size loop started.
    pub(super) fn live_started(&mut self) {
        self.coalescer.live_started();
    }

    /// The modal size loop ended: the final size goes out.
    pub(super) fn live_ended(&mut self, input: &ShellInput) {
        if let Some(size) = self.coalescer.live_ended() {
            self.post(input, size, None);
        }
    }

    /// A frame-clock tick a waiting size asked for.
    pub(super) fn tick(&mut self, input: &ShellInput) {
        if let Some(size) = self.coalescer.tick() {
            self.post(input, size, None);
        }
    }

    /// The core's window request ended (`SetWindowPos` returned): post the
    /// client area of `device` pixels under its token, changed or not.
    pub(super) fn answer_request(&mut self, input: &ShellInput, device: (i32, i32)) {
        let token = self.request.swap(0, Ordering::AcqRel);
        if token != 0 {
            let size = self.coalescer.post_now(self.metrics.scale.to_x(device));
            self.post(input, size, Some(token));
        }
    }
}
