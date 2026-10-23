//! The window: a view hosting a `CAMetalLayer` that shows the X screen at
//! the `--scale` copy factor, and the Vulkan surface on that layer.

use std::{ffi::c_void, rc::Rc};

use ash::vk;
use objc2::{ClassType, MainThreadMarker, MainThreadOnly, rc::Retained};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSColor, NSResponder, NSScreen, NSView, NSWindow,
    NSWindowStyleMask,
};
use objc2_foundation::{NSObjectProtocol, NSPoint, NSRect, NSString};
use objc2_quartz_core::{CALayer, CAMetalLayer, CATransaction, kCAGravityTopLeft};

use super::{input::ShellInput, scale::Metrics, view::YserverView};
use crate::surface::{
    resize::{MIN_SCREEN, to_px},
    swapchain::NativeSurface,
};

/// A titled, closable, minimisable, resizable window of `size` X pixels
/// (`size / xpp` points on the main screen), whose content is a
/// `CAMetalLayer` and whose view feeds `input`.
pub(super) fn create(
    mtm: MainThreadMarker,
    size: (u16, u16),
    input: Rc<ShellInput>,
) -> (Retained<NSWindow>, Retained<CAMetalLayer>) {
    let metrics = Metrics::of_main_screen(mtm);
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), metrics.points(size));
    let style = style();
    // SAFETY: a fresh allocation; released-when-closed is turned off below.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            style,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    // SAFETY: the shell owns the window outside a window controller.
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str("yserver"));
    window.setBackgroundColor(Some(&*NSColor::blackColor()));
    window.setContentMinSize(metrics.points(MIN_SCREEN));

    let layer = CAMetalLayer::new();
    layer.setContentsScale(metrics.backing);
    layer.setDrawableSize(metrics.backing_px(size));
    layer.setOpaque(true);
    // A drawable not yet at the new window size stays unstretched.
    // SAFETY: an immutable CoreAnimation constant.
    layer.setContentsGravity(unsafe { kCAGravityTopLeft });
    let view = YserverView::new(mtm, frame, input);
    // A layer set before `wantsLayer` makes the view layer-hosting.
    let host: &CALayer = &layer;
    view.setLayer(Some(host));
    view.setWantsLayer(true);
    let content: &NSView = &view;
    window.setContentView(Some(content));
    // No `setAcceptsMouseMovedEvents`: the view's `MouseMoved` tracking
    // area already delivers `mouseMoved:`, and both together doubled it.
    let responder: &NSResponder = &view;
    let _ = window.makeFirstResponder(Some(responder));
    window.center();
    (window, layer)
}

/// The window showing the X screen; others (Settings, the About panel)
/// come and go beside it.
pub(super) fn root_window(mtm: MainThreadMarker) -> Option<Retained<NSWindow>> {
    NSApplication::sharedApplication(mtm)
        .windows()
        .iter()
        .find(|window| {
            window
                .contentView()
                .is_some_and(|view| view.isKindOfClass(YserverView::class()))
        })
}

/// The window's style.
fn style() -> NSWindowStyleMask {
    NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable
        | NSWindowStyleMask::Resizable
}

/// The largest content the main screen shows without clamping the window,
/// in X pixels.
pub(super) fn max_content_size(mtm: MainThreadMarker) -> Option<(u16, u16)> {
    let screen = NSScreen::mainScreen(mtm)?;
    let content = NSWindow::contentRectForFrameRect_styleMask(screen.visibleFrame(), style(), mtm);
    let xpp = Metrics::of_main_screen(mtm).xpp;
    Some((
        to_px(content.size.width, xpp),
        to_px(content.size.height, xpp),
    ))
}

/// The layer's address for `vkCreateMetalSurfaceEXT` on the core thread.
struct LayerPtr(*const c_void);

// SAFETY: only handed to `vkCreateMetalSurfaceEXT`; the main thread keeps the
// layer retained until the process exits.
unsafe impl Send for LayerPtr {}

impl LayerPtr {
    fn get(&self) -> *const c_void {
        self.0
    }
}

/// A `VK_EXT_metal_surface` surface on `layer`.
pub(super) fn metal_surface(layer: &CAMetalLayer) -> NativeSurface {
    let layer = LayerPtr(std::ptr::from_ref(layer).cast());
    NativeSurface::new(ash::ext::metal_surface::NAME, move |entry, instance| {
        let loader = ash::ext::metal_surface::Instance::new(entry, instance);
        let info = vk::MetalSurfaceCreateInfoEXT::default().layer(layer.get());
        // SAFETY: a live CAMetalLayer and an instance with the extension.
        unsafe { loader.create_metal_surface(&info, None) }
    })
    // The driver sets layer properties on the core thread, which has no run
    // loop to commit them: they would show only once the main thread commits.
    .with_commit(CATransaction::flush)
}
