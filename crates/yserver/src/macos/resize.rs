//! The window's size to the core: the view reports its size changes,
//! `ResizeCoalescer` picks what to post, and the drawable and the pointer
//! clamp follow what was posted. A backing scale or screen change posts at
//! once when it changes the size, the copy factor or the millimetres.
//! Main thread only.

use std::{
    cell::RefCell,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use dispatch2::DispatchQueue;
use objc2::{MainThreadOnly, rc::Retained};
use objc2_app_kit::{NSView, NSWindow};
use objc2_quartz_core::{CAMetalLayer, CATransaction};

use super::{input::ShellInput, scale::Metrics};
use crate::surface::{
    clock::FrameClock,
    link::{ShellEvent, ShellLink},
    resize::{MIN_SCREEN, ResizeCoalescer, Size, to_px},
};

struct Sizer {
    link: Arc<ShellLink>,
    /// The token of the core's window request in progress (0: none).
    request: Arc<AtomicU64>,
    /// A size waits for the next frame-clock tick.
    tick_wanted: Arc<AtomicBool>,
    layer: Retained<CAMetalLayer>,
    input: Rc<ShellInput>,
    coalescer: ResizeCoalescer,
    /// The window's scale and screen, as last seen.
    metrics: Metrics,
    /// The copy factor and millimetres last posted.
    posted: (u32, (u32, u32)),
}

impl Sizer {
    fn post(&mut self, size: Size, request: Option<u64>) {
        let scale = self.metrics.factor;
        let mm = self.metrics.mm(size);
        // Before the event: the swapchain the core rebuilds on it follows
        // the drawable.
        self.layer.setDrawableSize(self.metrics.backing_px(size));
        self.input.set_fb(size);
        log::debug!(
            "macos: window {}x{} scale {scale} {}x{} mm (request {request:?})",
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
}

thread_local! {
    static SIZER: RefCell<Option<Sizer>> = const { RefCell::new(None) };
}

/// Report `view`'s size from now on; the core starts at `start` with
/// `metrics`.
pub(super) fn install(
    view: &NSView,
    start: Size,
    metrics: Metrics,
    link: Arc<ShellLink>,
    request: Arc<AtomicU64>,
    layer: Retained<CAMetalLayer>,
    input: Rc<ShellInput>,
    clock: &FrameClock,
) {
    let tick_wanted = Arc::new(AtomicBool::new(false));
    let wanted = Arc::clone(&tick_wanted);
    clock.set_tick_hook(move || {
        if wanted.swap(false, Ordering::AcqRel) {
            DispatchQueue::main().exec_async(tick);
        }
    });
    SIZER.set(Some(Sizer {
        link,
        request,
        tick_wanted,
        layer,
        input,
        coalescer: ResizeCoalescer::new(start),
        metrics,
        posted: (metrics.factor, metrics.mm(start)),
    }));
    view_resized(view);
}

/// The view's content in X pixels.
fn content_px(view: &NSView) -> Option<Size> {
    let window = view.window()?;
    let xpp = super::scale::xpp(&window);
    let size = view.bounds().size;
    Some((to_px(size.width, xpp), to_px(size.height, xpp)))
}

/// The view's size changed: the layer follows now, the core when the
/// coalescer says.
pub(super) fn view_resized(view: &NSView) {
    let Some(size) = content_px(view) else {
        return;
    };
    SIZER.with_borrow_mut(|sizer| {
        let Some(sizer) = sizer else {
            return;
        };
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        sizer.layer.setFrame(view.bounds());
        CATransaction::commit();
        if sizer.request.load(Ordering::Acquire) != 0 {
            // `answer_request` posts the outcome.
            return;
        }
        if let Some(size) = sizer.coalescer.resized(size) {
            sizer.post(size, None);
        } else if sizer.coalescer.waiting() {
            sizer.tick_wanted.store(true, Ordering::Release);
        }
    });
}

pub(super) fn live_started() {
    SIZER.with_borrow_mut(|sizer| {
        if let Some(sizer) = sizer {
            sizer.coalescer.live_started();
        }
    });
}

pub(super) fn live_ended() {
    SIZER.with_borrow_mut(|sizer| {
        let Some(sizer) = sizer else {
            return;
        };
        if let Some(size) = sizer.coalescer.live_ended() {
            sizer.post(size, None);
        }
    });
}

/// The window's backing scale or screen changed: the layer and the cursor
/// take the new scale, and the core hears at once of a new size (an
/// explicit `--scale` keeps the window's points), copy factor or
/// millimetres, even mid live resize.
pub(super) fn backing_changed(view: &NSView) {
    let Some(window) = view.window() else {
        return;
    };
    let Some(size) = content_px(view) else {
        return;
    };
    let metrics = Metrics::of_window(&window);
    window.setContentMinSize(metrics.points(MIN_SCREEN));
    super::cursor::rescale(view.mtm());
    SIZER.with_borrow_mut(|sizer| {
        let Some(sizer) = sizer else {
            return;
        };
        sizer.layer.setContentsScale(metrics.backing);
        sizer.metrics = metrics;
        if sizer.request.load(Ordering::Acquire) != 0 {
            // `answer_request` posts with the new metrics.
            return;
        }
        if size == sizer.coalescer.posted() && sizer.posted == (metrics.factor, metrics.mm(size)) {
            return;
        }
        let size = sizer.coalescer.post_now(size);
        sizer.post(size, None);
    });
}

/// A frame-clock tick a waiting size asked for (main queue).
fn tick() {
    SIZER.with_borrow_mut(|sizer| {
        let Some(sizer) = sizer else {
            return;
        };
        if let Some(size) = sizer.coalescer.tick() {
            sizer.post(size, None);
        }
    });
}

/// The core's window request ended (`setContentSize:` returned): post the
/// window's size under its token, changed or not.
pub(super) fn answer_request(window: &NSWindow) {
    let size = window.contentView().and_then(|view| content_px(&view));
    SIZER.with_borrow_mut(|sizer| {
        let Some(sizer) = sizer else {
            return;
        };
        let token = sizer.request.swap(0, Ordering::AcqRel);
        let Some(size) = size else {
            return;
        };
        if token != 0 {
            let size = sizer.coalescer.post_now(size);
            sizer.post(size, Some(token));
        }
    });
}
