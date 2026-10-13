//! The backend's per-window geometry, as the scene reads it.

use std::collections::HashMap;

#[derive(Debug, Clone, Copy)]
pub(crate) struct WindowGeometry {
    pub(crate) x: i16,
    pub(crate) y: i16,
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) depth: u8,
    pub(crate) mapped: bool,
    /// Mirror of core viewability, kept by `realize_window_storage` / `release_window_storage`.
    pub(crate) viewable: bool,
    pub(crate) parent: Option<u32>,
    pub(crate) stack_rank: u64,
    pub(crate) bg_pixel: Option<u32>,
    pub(crate) bg_pixmap: Option<u32>,
    /// #133 step 2 (P3) — the window's border width, in pixels, as the
    /// core tree has it. Carried on the wire by `create_subwindow` and
    /// by `HostSubwindowConfig::border_width`; recorded here so the
    /// render side has the same value as `Window::border_width` without
    /// reaching back into resources. Nothing consumes it yet: storage
    /// sizing is step 3 and the ring fill is step 4.
    pub(crate) border_width: u16,
    /// #133 step 2 (P3) — the resolved border source, flattened to
    /// primitives exactly as `bg_pixel` / `bg_pixmap` are, so the
    /// `yserver` crate need not know `resources::BorderSource`. Xorg's
    /// `PixUnion border` + `borderIsPixel` (`include/windowstr.h:146`)
    /// is an either/or, so at most one of these is `Some`: a tile
    /// pixmap wins when it has host storage, otherwise the pixel.
    /// Set from `change_subwindow_attributes` (CWBorderPixmap 0x04 /
    /// CWBorderPixel 0x08), which core forwards both at create time and
    /// on every border-attribute change.
    pub(crate) border_pixel: Option<u32>,
    pub(crate) border_pixmap: Option<u32>,
    /// Stage 5 Phase A — per-window X11 cursor attribute. `None`
    /// means inherit from the parent chain; `Some(xid)` pins a
    /// specific cursor on hover-in. Mutated by `define_cursor` only —
    /// `change_subwindow_attributes` does not decode CWCursor (the
    /// cursor arrives through `Backend::define_cursor`, which core
    /// calls for a CWA cursor change as well).
    pub(crate) cursor: Option<u32>,
}

pub(super) type WindowsMap = HashMap<u32, WindowGeometry>;
