use super::*;

#[derive(Debug, Default)]
pub struct ClientRemovedResources {
    pub closed_fonts: Vec<u32>,
    /// Host pixmaps of the removed pixmap resources, plus those the removed
    /// GCs held as clip mask / tile / stipple: candidates for the orphan gate.
    pub freed_pixmaps: Vec<u32>,
    /// One host backing per removed `NameWindowPixmap` name, NOT deduplicated: each owns a ref.
    pub freed_names: Vec<crate::backend::PixmapHandle>,
    pub freed_pictures: Vec<(u32, Option<u32>)>,
    pub freed_glyphsets: Vec<u32>,
    pub freed_cursors: Vec<u32>,
    /// Client-visible DRI3 syncobj XIDs whose backend rows must be removed.
    pub freed_dri3_syncobjs: Vec<u32>,
}

/// A visual exposed to clients via the setup reply. We currently
/// expose a fixed pair (root TrueColor at 24-bit, ARGB TrueColor at
/// 32-bit). The `host_visual_xid` field is filled in once we've
/// probed the host server's setup; it stays `None` until then so
/// that early CreateWindow forwarding can be detected and skipped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Visual {
    pub id: ResourceId,
    pub class: u8,
    pub depth: u8,
    pub bits_per_rgb: u8,
    pub colormap_entries: u16,
    pub red_mask: u32,
    pub green_mask: u32,
    pub blue_mask: u32,
    pub alpha_mask: u32,
    pub host_visual_xid: Option<crate::backend::VisualHandle>,
}

/// A colormap. We currently expose one per visual (root colormap +
/// ARGB colormap). The `host_colormap_xid` is allocated on the host
/// once during `HostX11` init and pushed in via
/// [`ResourceTable::set_colormap_host_xid`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Colormap {
    pub id: ResourceId,
    pub visual: ResourceId,
    pub host_colormap_xid: Option<crate::backend::ColormapHandle>,
    /// Client that created this colormap, used by
    /// `remove_non_window_resources_owned_by` so disconnect cleanup
    /// frees the entry. Server-allocated default colormaps
    /// (`ROOT_COLORMAP`, `ARGB_COLORMAP`) use `ClientId(0)` as the
    /// sentinel owner — they outlive every client connection.
    pub owner: yserver_protocol::x11::ClientId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostDrawableTarget {
    Window {
        nested: ResourceId,
        host_xid: crate::backend::WindowHandle,
        depth: u8,
    },
    Pixmap {
        nested: ResourceId,
        host_xid: crate::backend::PixmapHandle,
        width: u16,
        height: u16,
        depth: u8,
    },
}

impl HostDrawableTarget {
    pub fn host_xid(self) -> u32 {
        match self {
            Self::Window { host_xid, .. } => host_xid.as_raw(),
            Self::Pixmap { host_xid, .. } => host_xid.as_raw(),
        }
    }

    pub fn host_handle(self) -> crate::backend::AnyHandle {
        match self {
            Self::Window { host_xid, .. } => crate::backend::AnyHandle::Window(host_xid),
            Self::Pixmap { host_xid, .. } => crate::backend::AnyHandle::Pixmap(host_xid),
        }
    }

    pub fn depth(self) -> u8 {
        match self {
            Self::Window { depth, .. } | Self::Pixmap { depth, .. } => depth,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExposedRect {
    pub window: ResourceId,
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
}

/// Windows whose viewability changed in one operation: `became_viewable` in
/// pre-order (parent before child), `became_unviewable` in post-order.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ViewabilityDelta {
    pub became_viewable: Vec<ResourceId>,
    pub became_unviewable: Vec<ResourceId>,
}

impl ViewabilityDelta {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.became_viewable.is_empty() && self.became_unviewable.is_empty()
    }

    /// Appends a disjoint sibling subtree's delta, keeping both orders.
    pub fn extend(&mut self, other: ViewabilityDelta) {
        self.became_viewable.extend(other.became_viewable);
        self.became_unviewable.extend(other.became_unviewable);
    }
}

/// Result of a map/unmap: `mapping_changed` is the Unmapped<->mapped flip
/// that drives MapNotify/UnmapNotify, independent of the viewability delta.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[must_use]
pub struct MapTransition {
    pub mapping_changed: bool,
    pub delta: ViewabilityDelta,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReparentResult {
    pub window: ResourceId,
    pub old_parent: ResourceId,
    pub new_parent: ResourceId,
    pub x: i16,
    pub y: i16,
    pub override_redirect: bool,
    pub host_xid: Option<crate::backend::WindowHandle>,
    pub old_map_state: MapState,
    pub new_map_state: MapState,
    pub delta: ViewabilityDelta,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReparentWindowError {
    BadWindow,
    BadMatch,
}

/// The border source of a window — Xorg's `PixUnion border` +
/// `borderIsPixel` (`include/windowstr.h:146`). A border is EITHER a
/// solid pixel OR a tile pixmap, never both.
///
/// The `Pixmap` variant snapshots the host handle at install time
/// (same retention rule as `Window::background_pixmap_host_xid`): the
/// server retains the border pixmap independent of client refs, so a
/// later `FreePixmap` of the source must not drop the host storage out
/// from under the border.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BorderSource {
    Pixel(u32),
    Pixmap {
        id: ResourceId,
        host_xid: Option<crate::backend::PixmapHandle>,
    },
}

/// Resolved background attributes for a window after following
/// any `ParentRelative` links up the ancestry chain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolvedWindowBackground {
    pub background_pixel: u32,
    pub background_pixmap_host_xid: Option<crate::backend::PixmapHandle>,
    /// Accumulated child→bg-owner offset across ParentRelative hops:
    /// the tile pattern aligns to the OWNING window's origin, so the
    /// child samples the tile at (x + offset) (X11 §window
    /// background: "aligned with the parent's origin").
    pub tile_origin_offset: (i32, i32),
}

/// Off-screen mirror backing a redirected window. `host_pixmap` is
/// the backend pixmap handle (the X11 resource layer treats it as
/// a synthetic pixmap drawable for paint-time routing). Width /
/// height / depth snapshot the window's geometry at the moment the
/// backing was allocated — used so resize can decide whether to
/// rotate the backing.
#[derive(Clone, Copy, Debug)]
pub struct RedirectedBacking {
    pub host_pixmap: crate::backend::PixmapHandle,
    pub width: u16,
    pub height: u16,
    pub depth: u8,
}

/// Pixmap host handles released by a `ChangeWindowAttributes` apply.
/// The caller frees each on the backend only once fully orphaned: no
/// surviving window background/border references it AND no live pixmap
/// resource still owns it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReleasedAttrPixmaps {
    pub background: Option<crate::backend::PixmapHandle>,
    pub border: Option<crate::backend::PixmapHandle>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowClass {
    CopyFromParent,
    InputOutput,
    InputOnly,
    Other(u16),
}

impl WindowClass {
    pub(super) fn from_protocol(value: u16) -> Self {
        match value {
            0 => Self::CopyFromParent,
            1 => Self::InputOutput,
            2 => Self::InputOnly,
            value => Self::Other(value),
        }
    }

    pub fn protocol_value(self) -> u16 {
        match self {
            Self::CopyFromParent => 0,
            Self::InputOutput => 1,
            Self::InputOnly => 2,
            Self::Other(value) => value,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MapState {
    Unmapped,
    Unviewable,
    Viewable,
}

impl MapState {
    pub fn protocol_value(self) -> u8 {
        match self {
            Self::Unmapped => 0,
            Self::Unviewable => 1,
            Self::Viewable => 2,
        }
    }
}
