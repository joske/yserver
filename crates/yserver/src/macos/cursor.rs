//! The X cursor as an `NSCursor` over the content view, and pointer warps.
//! Main thread only.

use std::{cell::RefCell, collections::HashMap};

use objc2::{AnyThread, MainThreadMarker, rc::Retained};
use objc2_app_kit::{
    NSApplication, NSBitmapImageRep, NSCursor, NSDeviceRGBColorSpace, NSImage, NSScreen, NSView,
    NSWindow,
};
use objc2_foundation::{NSPoint, NSSize};

use crate::surface::ui::CursorImage;

/// Cursors kept for animation frames and cursor switches.
const CACHE_LIMIT: usize = 64;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGWarpMouseCursorPosition(new_cursor_position: NSPoint) -> i32;
    fn CGAssociateMouseAndMouseCursorPosition(connected: u32) -> i32;
}

#[derive(Default)]
struct Cursors {
    by_image: HashMap<(u64, (u16, u16)), Retained<NSCursor>>,
    /// The cursor the view shows; `None` before the core sent one.
    current: Option<Retained<NSCursor>>,
    blank: Option<Retained<NSCursor>>,
}

thread_local! {
    static CURSORS: RefCell<Cursors> = RefCell::new(Cursors::default());
}

/// The window and its content view.
fn window_and_view(mtm: MainThreadMarker) -> Option<(Retained<NSWindow>, Retained<NSView>)> {
    let window = NSApplication::sharedApplication(mtm)
        .windows()
        .firstObject()?;
    let view = window.contentView()?;
    Some((window, view))
}

/// Whether the pointer is over the view of the key window.
fn pointer_inside(window: &NSWindow, view: &NSView) -> bool {
    if !window.isKeyWindow() {
        return false;
    }
    let at = view.convertPoint_fromView(window.mouseLocationOutsideOfEventStream(), None);
    let bounds = view.bounds();
    (0.0..bounds.size.width).contains(&at.x) && (0.0..bounds.size.height).contains(&at.y)
}

/// Show `image` (`None`: no cursor) over the view.
pub(super) fn set(mtm: MainThreadMarker, image: Option<CursorImage>) {
    log::debug!(
        target: "yserver::macos::diag",
        "DIAG cursor set {:?}",
        image.as_ref().map(|i| (i.version, i.width, i.height, i.hot))
    );
    let Some((window, view)) = window_and_view(mtm) else {
        return;
    };
    let scale = window.backingScaleFactor();
    let cursor = CURSORS.with_borrow_mut(|cursors| {
        let cursor = match image {
            Some(image) => {
                if cursors.by_image.len() >= CACHE_LIMIT {
                    cursors.by_image.clear();
                }
                cursors
                    .by_image
                    .entry((image.version, image.hot))
                    .or_insert_with(|| build(&image, scale))
                    .clone()
            }
            None => cursors.blank.get_or_insert_with(blank).clone(),
        };
        cursors.current = Some(cursor.clone());
        cursor
    });
    if pointer_inside(&window, &view) {
        cursor.set();
    }
}

/// The view's `cursorUpdate:`: show the X cursor (the arrow until the core
/// sent one).
pub(super) fn apply_current() {
    log::debug!(target: "yserver::macos::diag", "DIAG cursorUpdate");
    let current = CURSORS.with_borrow(|cursors| cursors.current.clone());
    current.unwrap_or_else(NSCursor::arrowCursor).set();
}

/// An `NSCursor` of `image`, `scale` backing pixels per point, so one X
/// pixel is one backing pixel.
fn build(image: &CursorImage, scale: f64) -> Retained<NSCursor> {
    let rgba = image.rgba();
    let (width, height) = (image.width.max(1), image.height.max(1));
    let rep = bitmap(width, height, &rgba);
    let size = NSSize::new(f64::from(width) / scale, f64::from(height) / scale);
    let ns_image = NSImage::initWithSize(NSImage::alloc(), size);
    if let Some(rep) = rep {
        ns_image.addRepresentation(&rep);
    }
    let hot = NSPoint::new(
        f64::from(image.hot.0) / scale,
        f64::from(image.hot.1) / scale,
    );
    NSCursor::initWithImage_hotSpot(NSCursor::alloc(), &ns_image, hot)
}

/// A fully transparent cursor.
fn blank() -> Retained<NSCursor> {
    let rep = bitmap(1, 1, &[0, 0, 0, 0]);
    let ns_image = NSImage::initWithSize(NSImage::alloc(), NSSize::new(1.0, 1.0));
    if let Some(rep) = rep {
        ns_image.addRepresentation(&rep);
    }
    NSCursor::initWithImage_hotSpot(NSCursor::alloc(), &ns_image, NSPoint::new(0.0, 0.0))
}

/// A premultiplied RGBA bitmap holding `rgba` (`width * height * 4` bytes).
fn bitmap(width: u32, height: u32, rgba: &[u8]) -> Option<Retained<NSBitmapImageRep>> {
    let (w, h) = (width as isize, height as isize);
    // SAFETY: null planes make the rep allocate its own buffer; the
    // colour space name is a static AppKit constant.
    let rep = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            w,
            h,
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            w * 4,
            32,
        )
    }?;
    let data = rep.bitmapData();
    let len = rgba.len().min(width as usize * height as usize * 4);
    if !data.is_null() {
        // SAFETY: the rep owns `bytesPerRow * height` bytes at `data`.
        unsafe { std::ptr::copy_nonoverlapping(rgba.as_ptr(), data, len) };
    }
    Some(rep)
}

/// Move the host pointer to X pixel `(x, y)` of the view, while the window
/// is key and the pointer is over it.
pub(super) fn warp(mtm: MainThreadMarker, x: i32, y: i32) {
    let Some((window, view)) = window_and_view(mtm) else {
        return;
    };
    if !pointer_inside(&window, &view) {
        log::debug!(target: "yserver::macos::diag", "DIAG warp x={x} y={y} skipped: outside");
        return;
    }
    let scale = window.backingScaleFactor();
    let height = view.bounds().size.height;
    let in_view = NSPoint::new(f64::from(x) / scale, height - f64::from(y) / scale);
    let in_window = view.convertPoint_toView(in_view, None);
    let on_screen = window.convertPointToScreen(in_window);
    // Quartz global coordinates: top-left origin of the primary screen.
    let Some(primary) = NSScreen::screens(mtm).firstObject() else {
        return;
    };
    let global = NSPoint::new(on_screen.x, primary.frame().size.height - on_screen.y);
    log::debug!(
        target: "yserver::macos::diag",
        "DIAG warp x={x} y={y} global=({}, {})",
        global.x,
        global.y
    );
    // SAFETY: plain Quartz calls; re-associating avoids the post-warp freeze.
    unsafe {
        CGWarpMouseCursorPosition(global);
        CGAssociateMouseAndMouseCursorPosition(1);
    }
}
