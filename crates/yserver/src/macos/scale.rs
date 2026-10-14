//! `--scale` on the main thread: X pixels per point and the copy factor at
//! a backing scale, and the window's size in millimetres.

use std::cell::Cell;

use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSScreen, NSWindow};
use objc2_foundation::{NSNumber, NSSize, ns_string};

use crate::{
    launch::ScaleOption,
    surface::resize::{self, Size},
};

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGDisplayScreenSize(display: u32) -> NSSize;
}

thread_local! {
    static OPTION: Cell<ScaleOption> = const { Cell::new(ScaleOption::Auto) };
}

/// Use `option` from now on.
pub(super) fn set_option(option: ScaleOption) {
    OPTION.set(option);
}

/// X pixels per point in `window`.
pub(super) fn xpp(window: &NSWindow) -> f64 {
    Metrics::at(window.backingScaleFactor(), None).xpp
}

/// The scale of a window or screen and the screen's physical size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Metrics {
    /// Backing pixels per point.
    pub(super) backing: f64,
    /// X pixels per point.
    pub(super) xpp: f64,
    /// Backing pixels per X pixel per axis.
    pub(super) factor: u32,
    /// The screen in millimetres and in points; zero when unknown.
    display_mm: (f64, f64),
    display_pt: (f64, f64),
}

impl Metrics {
    /// `window`'s, on the screen it is on (the main screen before it is
    /// shown).
    pub(super) fn of_window(window: &NSWindow) -> Self {
        let screen = window
            .screen()
            .or_else(|| NSScreen::mainScreen(window.mtm()));
        Self::at(window.backingScaleFactor(), screen.as_deref())
    }

    /// The main screen's.
    pub(super) fn of_main_screen(mtm: MainThreadMarker) -> Self {
        let screen = NSScreen::mainScreen(mtm);
        let backing = screen.as_ref().map_or(1.0, |s| s.backingScaleFactor());
        Self::at(backing, screen.as_deref())
    }

    fn at(backing: f64, screen: Option<&NSScreen>) -> Self {
        let option = OPTION.get();
        let (display_mm, display_pt) = screen.map_or(((0.0, 0.0), (0.0, 0.0)), physical_size);
        Self {
            backing,
            xpp: resize::x_per_point(option, backing),
            factor: resize::copy_factor(option, backing),
            display_mm,
            display_pt,
        }
    }

    /// `size` X pixels in points.
    pub(super) fn points(&self, size: Size) -> NSSize {
        NSSize::new(f64::from(size.0) / self.xpp, f64::from(size.1) / self.xpp)
    }

    /// `size` X pixels in backing pixels: the drawable that shows them.
    pub(super) fn backing_px(&self, size: Size) -> NSSize {
        let factor = f64::from(self.factor);
        NSSize::new(f64::from(size.0) * factor, f64::from(size.1) * factor)
    }

    /// `size` X pixels in millimetres; `(0, 0)` when the screen does not
    /// say.
    pub(super) fn mm(&self, size: Size) -> (u32, u32) {
        resize::output_mm(size, self.xpp, self.display_mm, self.display_pt)
    }
}

/// `screen` in millimetres (CoreGraphics, zero when unknown) and in points.
fn physical_size(screen: &NSScreen) -> ((f64, f64), (f64, f64)) {
    let points = screen.frame().size;
    let display = screen
        .deviceDescription()
        .objectForKey(ns_string!("NSScreenNumber"))
        .and_then(|number| number.downcast::<NSNumber>().ok())
        .map(|number| number.unsignedIntValue());
    let Some(display) = display else {
        return ((0.0, 0.0), (points.width, points.height));
    };
    // SAFETY: a plain Quartz query; an unknown display gives a zero size.
    let mm = unsafe { CGDisplayScreenSize(display) };
    ((mm.width, mm.height), (points.width, points.height))
}
