//! The content view: hosts the `CAMetalLayer`, turns mouse and scroll
//! events into input for the shell and shows the X cursor.

use std::rc::Rc;

use objc2::{
    AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::Retained, runtime::AnyObject,
};
use objc2_app_kit::{NSEvent, NSResponder, NSTrackingArea, NSTrackingAreaOptions, NSView};
use objc2_foundation::{NSObject, NSRect, NSSize};

use super::input::ShellInput;

pub(super) struct ViewIvars {
    input: Rc<ShellInput>,
}

define_class!(
    // SAFETY: NSView has no subclassing requirements beyond the main thread,
    // and the class has no `Drop`.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = ViewIvars]
    pub(super) struct YserverView;

    impl YserverView {
        // SAFETY: the signature matches `-[NSResponder acceptsFirstResponder]`.
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        // SAFETY: the signature matches `-[NSView acceptsFirstMouse:]`.
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            false
        }

        // SAFETY: the signature matches `-[NSResponder mouseMoved:]`.
        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            self.ivars().input.motion(self, event);
        }

        // SAFETY: the signature matches `-[NSResponder mouseDragged:]`.
        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            self.ivars().input.motion(self, event);
        }

        // SAFETY: the signature matches `-[NSResponder rightMouseDragged:]`.
        #[unsafe(method(rightMouseDragged:))]
        fn right_mouse_dragged(&self, event: &NSEvent) {
            self.ivars().input.motion(self, event);
        }

        // SAFETY: the signature matches `-[NSResponder otherMouseDragged:]`.
        #[unsafe(method(otherMouseDragged:))]
        fn other_mouse_dragged(&self, event: &NSEvent) {
            self.ivars().input.motion(self, event);
        }

        // SAFETY: the signature matches `-[NSResponder mouseDown:]`.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            self.ivars().input.button(self, event, true);
        }

        // SAFETY: the signature matches `-[NSResponder mouseUp:]`.
        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            self.ivars().input.button(self, event, false);
        }

        // SAFETY: the signature matches `-[NSResponder rightMouseDown:]`.
        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, event: &NSEvent) {
            self.ivars().input.button(self, event, true);
        }

        // SAFETY: the signature matches `-[NSResponder rightMouseUp:]`.
        #[unsafe(method(rightMouseUp:))]
        fn right_mouse_up(&self, event: &NSEvent) {
            self.ivars().input.button(self, event, false);
        }

        // SAFETY: the signature matches `-[NSResponder otherMouseDown:]`.
        #[unsafe(method(otherMouseDown:))]
        fn other_mouse_down(&self, event: &NSEvent) {
            self.ivars().input.button(self, event, true);
        }

        // SAFETY: the signature matches `-[NSResponder otherMouseUp:]`.
        #[unsafe(method(otherMouseUp:))]
        fn other_mouse_up(&self, event: &NSEvent) {
            self.ivars().input.button(self, event, false);
        }

        // SAFETY: the signature matches `-[NSResponder cursorUpdate:]`.
        #[unsafe(method(cursorUpdate:))]
        fn cursor_update(&self, _event: &NSEvent) {
            super::cursor::apply_current();
        }

        // SAFETY: the signature matches `-[NSResponder scrollWheel:]`.
        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            self.ivars().input.scroll(event);
        }

        // SAFETY: the signature matches `-[NSView setFrameSize:]`.
        #[unsafe(method(setFrameSize:))]
        fn set_frame_size(&self, size: NSSize) {
            // SAFETY: the superclass method of the same signature.
            let () = unsafe { msg_send![super(self), setFrameSize: size] };
            super::resize::view_resized(self);
        }

        // SAFETY: the signature matches `-[NSView viewDidChangeBackingProperties]`.
        #[unsafe(method(viewDidChangeBackingProperties))]
        fn view_did_change_backing_properties(&self) {
            // SAFETY: the superclass method of the same signature.
            let () = unsafe { msg_send![super(self), viewDidChangeBackingProperties] };
            super::resize::backing_changed(self);
        }

        // SAFETY: the signature matches `-[NSView viewWillStartLiveResize]`.
        #[unsafe(method(viewWillStartLiveResize))]
        fn view_will_start_live_resize(&self) {
            // SAFETY: the superclass method of the same signature.
            let () = unsafe { msg_send![super(self), viewWillStartLiveResize] };
            super::resize::live_started();
        }

        // SAFETY: the signature matches `-[NSView viewDidEndLiveResize]`.
        #[unsafe(method(viewDidEndLiveResize))]
        fn view_did_end_live_resize(&self) {
            // SAFETY: the superclass method of the same signature.
            let () = unsafe { msg_send![super(self), viewDidEndLiveResize] };
            super::resize::live_ended();
        }
    }
);

impl YserverView {
    /// A view of `frame` points feeding `input`, tracking the pointer
    /// whether or not the window is key.
    pub(super) fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        input: Rc<ShellInput>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ViewIvars { input });
        // SAFETY: `NSView`'s designated initialiser.
        let view: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        let motion = NSTrackingAreaOptions::MouseMoved
            | NSTrackingAreaOptions::MouseEnteredAndExited
            | NSTrackingAreaOptions::ActiveAlways
            | NSTrackingAreaOptions::InVisibleRect;
        view.add_tracking_area(motion);
        // `cursorUpdate:` never comes from an `ActiveAlways` area.
        view.add_tracking_area(
            NSTrackingAreaOptions::CursorUpdate
                | NSTrackingAreaOptions::ActiveInKeyWindow
                | NSTrackingAreaOptions::InVisibleRect,
        );
        view
    }

    fn add_tracking_area(&self, options: NSTrackingAreaOptions) {
        let owner: &AnyObject = self;
        // SAFETY: `InVisibleRect` areas ignore the rect; the owner is this
        // view, which outlives its tracking areas.
        let area = unsafe {
            NSTrackingArea::initWithRect_options_owner_userInfo(
                NSTrackingArea::alloc(),
                NSRect::ZERO,
                options,
                Some(owner),
                None,
            )
        };
        self.addTrackingArea(&area);
    }
}
