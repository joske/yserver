//! The window's size to the core: `WM_SIZE` reports the client area,
//! `ResizeCoalescer` picks what to post (a modal size loop is a live
//! resize), and the pointer clamp follows what was posted. The swapchain
//! takes the client area's size by itself. UI thread only.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use super::{input::ShellInput, window};
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
    /// The primary display in millimetres and pixels.
    display: ((f64, f64), (f64, f64)),
}

impl Sizer {
    /// The core starts at `start`; `tick_wanted` is the frame clock hook's.
    pub(super) fn new(
        start: Size,
        link: Arc<ShellLink>,
        request: Arc<AtomicU64>,
        tick_wanted: Arc<AtomicBool>,
    ) -> Self {
        Self {
            link,
            request,
            tick_wanted,
            coalescer: ResizeCoalescer::new(start),
            display: window::display_metrics(),
        }
    }

    /// `size` in millimetres.
    pub(super) fn mm(&self, size: Size) -> (u32, u32) {
        window::output_mm(size, self.display)
    }

    fn post(&self, input: &ShellInput, size: Size, request: Option<u64>) {
        let mm = self.mm(size);
        input.set_fb(size);
        log::debug!(
            "windows: window {}x{} {}x{} mm (request {request:?})",
            size.0,
            size.1,
            mm.0,
            mm.1
        );
        self.link.post(ShellEvent::Resized {
            width: size.0,
            height: size.1,
            scale: 1,
            mm,
            request,
        });
    }

    /// The client area is now `size`: the core hears of it when the
    /// coalescer says.
    pub(super) fn resized(&mut self, input: &ShellInput, size: Size) {
        if self.request.load(Ordering::Acquire) != 0 {
            // `answer_request` posts the outcome.
            return;
        }
        if let Some(size) = self.coalescer.resized(size) {
            self.post(input, size, None);
        } else if self.coalescer.waiting() {
            self.tick_wanted.store(true, Ordering::Release);
        }
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
    /// client area under its token, changed or not.
    pub(super) fn answer_request(&mut self, input: &ShellInput, size: Size) {
        let token = self.request.swap(0, Ordering::AcqRel);
        if token != 0 {
            let size = self.coalescer.post_now(size);
            self.post(input, size, Some(token));
        }
    }
}
