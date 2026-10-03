//! yserver-local input event enum.
//!
//! Deliberately minimal: keycodes, pointer deltas, button + state.
//! No keysym translation — that's xkbcommon's job and lives in C.

use yserver_core::{core_loop::DeviceInfo, xinput::InputSourceId};

#[derive(Debug, Clone)]
pub enum InputEvent {
    KeyPress {
        source_id: InputSourceId,
        keycode: u32,
    },
    KeyRelease {
        source_id: InputSourceId,
        keycode: u32,
    },
    /// Relative pointer motion (mouse).
    PointerMotion {
        source_id: InputSourceId,
        dx: f64,
        dy: f64,
    },
    /// Absolute pointer motion (tablet).  Coordinates are in 0..1 over the
    /// device's logical surface; the backend scales to scanout dimensions.
    PointerMotionAbsolute {
        source_id: InputSourceId,
        x_norm: f64,
        y_norm: f64,
    },
    Button {
        source_id: InputSourceId,
        code: u32,
        pressed: bool,
    },
    /// Pointer scroll wheel / two-finger / continuous scroll, in v120
    /// high-resolution units. 120 v120 ≈ one "click" of a discrete wheel.
    /// `dx_v120 > 0` is scroll-right, `dy_v120 > 0` is scroll-down (matches
    /// libinput's convention).
    PointerScroll {
        source_id: InputSourceId,
        dx_v120: i32,
        dy_v120: i32,
    },
    /// Two-finger scroll ended (fingers lifted). libinput emits a
    /// finger-scroll event with all axes 0 to mark the stop; we forward it
    /// so the backend can emit an XI2 delta-0 scroll motion, which GDK turns
    /// into `scroll.is_stop = TRUE`. Firefox's SwipeTracker uses that stop to
    /// commit a horizontal-swipe history navigation (bug 1539730); without it
    /// the swipe arrow appears but never fires. Only `ScrollFinger` produces
    /// this — `ScrollContinuous`/`ScrollWheel` have no finger-lift.
    PointerScrollStop { source_id: InputSourceId },
    /// A new input device has been enumerated by libinput.  Carries a
    /// snapshot of its identity and configuration; forwarded to the
    /// process-lifetime source inventory.
    DeviceAdded(DeviceInfo),
    /// The source remains present but its libinput attachment was retired
    /// for VT release.
    DeviceSuspended { source_id: InputSourceId },
    /// A paused source continued on its original kernel endpoint.
    DeviceResumed(DeviceInfo),
    /// An input device has been removed. The source ID is its runtime identity.
    DeviceRemoved { source_id: InputSourceId },
}
