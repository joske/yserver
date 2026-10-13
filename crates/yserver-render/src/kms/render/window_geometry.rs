//! The backend's per-window geometry, as the scene reads it.

use std::collections::HashMap;

#[derive(Debug, Clone, Copy)]
pub struct WindowGeometry {
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    pub depth: u8,
    pub mapped: bool,
    /// Mirror of core viewability, kept by `realize_window_storage` / `release_window_storage`.
    pub viewable: bool,
    pub parent: Option<u32>,
    pub stack_rank: u64,
    pub bg_pixel: Option<u32>,
    pub bg_pixmap: Option<u32>,
    /// #133 step 2 (P3) — the window's border width, in pixels, as the
    /// core tree has it. Carried on the wire by `create_subwindow` and
    /// by `HostSubwindowConfig::border_width`; recorded here so the
    /// render side has the same value as `Window::border_width` without
    /// reaching back into resources. Nothing consumes it yet: storage
    /// sizing is step 3 and the ring fill is step 4.
    pub border_width: u16,
    /// #133 step 2 (P3) — the resolved border source, flattened to
    /// primitives exactly as `bg_pixel` / `bg_pixmap` are, so the
    /// `yserver` crate need not know `resources::BorderSource`. Xorg's
    /// `PixUnion border` + `borderIsPixel` (`include/windowstr.h:146`)
    /// is an either/or, so at most one of these is `Some`: a tile
    /// pixmap wins when it has host storage, otherwise the pixel.
    /// Set from `change_subwindow_attributes` (CWBorderPixmap 0x04 /
    /// CWBorderPixel 0x08), which core forwards both at create time and
    /// on every border-attribute change.
    pub border_pixel: Option<u32>,
    pub border_pixmap: Option<u32>,
    /// Stage 5 Phase A — per-window X11 cursor attribute. `None`
    /// means inherit from the parent chain; `Some(xid)` pins a
    /// specific cursor on hover-in. Mutated by `define_cursor` only —
    /// `change_subwindow_attributes` does not decode CWCursor (the
    /// cursor arrives through `Backend::define_cursor`, which core
    /// calls for a CWA cursor change as well).
    pub cursor: Option<u32>,
}

pub type WindowsMap = HashMap<u32, WindowGeometry>;
