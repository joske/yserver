//! The window's size to the core: the view reports its size changes,
//! `ResizeCoalescer` picks what to post, and the drawable and the pointer
//! clamp follow what was posted. Main thread only.

use std::{
    cell::RefCell,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2_app_kit::{NSView, NSWindow};
use objc2_foundation::NSSize;
use objc2_quartz_core::{CAMetalLayer, CATransaction};

use super::input::ShellInput;
use crate::surface::{
    clock::FrameClock,
    link::{ShellEvent, ShellLink},
    resize::{ResizeCoalescer, Size, to_px},
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
}

impl Sizer {
    fn post(&self, size: Size, request: Option<u64>) {
        // Before the event: the swapchain the core rebuilds on it follows
        // the drawable.
        self.layer
            .setDrawableSize(NSSize::new(f64::from(size.0), f64::from(size.1)));
        self.input.set_fb(size);
        log::debug!("macos: window {}x{} (request {request:?})", size.0, size.1);
        self.link.post(ShellEvent::Resized {
            width: size.0,
            height: size.1,
            request,
        });
    }
}

thread_local! {
    static SIZER: RefCell<Option<Sizer>> = const { RefCell::new(None) };
}

/// Report `view`'s size from now on; the core starts at `start`.
pub(super) fn install(
    view: &NSView,
    start: Size,
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
    }));
    view_resized(view);
}

/// The view's content in X pixels (backing pixels).
fn content_px(view: &NSView) -> Option<Size> {
    let scale = view.window()?.backingScaleFactor();
    let size = view.bounds().size;
    Some((to_px(size.width, scale), to_px(size.height, scale)))
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
            let size = sizer.coalescer.answered(size);
            sizer.post(size, Some(token));
        }
    });
}
