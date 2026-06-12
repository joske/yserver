#![allow(dead_code)]

use std::collections::HashMap;

use yserver_protocol::x11::{
    AtomId, ChangeWindowAttributesRequest, ClientId, ClipRectangles, ConfigureWindowRequest,
    CreateGcRequest, CreatePixmapRequest, CreateWindowRequest, FontMetrics, GcChange,
    ReparentWindowRequest, ResourceId, SetClipRectanglesRequest,
};

use crate::{
    backend::{
        ArcMode, CapStyle, ClipState, DrawState, FillRule, FillState, FillStyle, FontHandle,
        GcFunction, JoinStyle, LineStyle, SubwindowMode,
    },
    properties::PropertyValue,
};

pub const SERVER_OWNER: ClientId = ClientId(0);

#[derive(Debug, Default)]
pub struct ClientRemovedResources {
    pub closed_fonts: Vec<u32>,
    pub freed_pixmaps: Vec<u32>,
    pub freed_pictures: Vec<(u32, Option<u32>)>,
    pub freed_glyphsets: Vec<u32>,
    pub freed_cursors: Vec<u32>,
}

pub const ROOT_WINDOW: ResourceId = ResourceId(0x100);
pub const ROOT_COLORMAP: ResourceId = ResourceId(0x101);
pub const ROOT_VISUAL: ResourceId = ResourceId(0x102);
/// Composite overlay window returned by `XCompositeGetOverlayWindow`.
/// A real, distinct XID is required — marco's compositor calls
/// `XSelectInput(overlay, ExposureMask)`, which would otherwise clobber
/// its own WM event mask on root (dropping SubstructureRedirect) if we
/// returned root here.
pub const COMPOSITE_OVERLAY_WINDOW: ResourceId = ResourceId(0x103);
pub const ARGB_VISUAL: ResourceId = ResourceId(0x103);
pub const ARGB_COLORMAP: ResourceId = ResourceId(0x104);

/// X11 visual class codes (subset we care about). The setup reply
/// advertises these per-visual; clients pass a visual ID into
/// `CreateWindow`, `CreateColormap`, and RENDER `CreatePicture`.
pub const VISUAL_CLASS_TRUE_COLOR: u8 = 4;

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReparentResult {
    pub window: ResourceId,
    pub old_parent: ResourceId,
    pub new_parent: ResourceId,
    pub x: i16,
    pub y: i16,
    pub override_redirect: bool,
    pub host_xid: Option<crate::backend::WindowHandle>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReparentWindowError {
    BadWindow,
    BadMatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PictureKind {
    /// Backed by a real drawable (window or pixmap) — valid as a
    /// Composite/Trapezoids/Triangles/FillRectangles/CompositeGlyphs
    /// destination.
    Drawable,
    /// Backed by a 1x1 SolidFill / LinearGradient / RadialGradient /
    /// ConicalGradient. Has no underlying drawable, so cannot be
    /// used as a destination — RENDER opcodes that try must raise
    /// `BadDrawable`.
    Sourceless,
}

#[derive(Debug)]
pub struct PictureState {
    pub client: ClientId,
    pub host_picture_xid: crate::backend::PictureHandle,
    pub host_owned_pixmap: Option<crate::backend::PixmapHandle>,
    pub kind: PictureKind,
    /// For `PictureKind::Drawable` pictures: the client-visible XID of
    /// the backing window or pixmap. Used by RENDER paint handlers to
    /// accumulate damage on the right drawable after painting.
    /// `None` for `Sourceless` pictures (SolidFill / gradient).
    pub drawable: Option<ResourceId>,
}

#[derive(Debug)]
pub struct GlyphSetState {
    pub client: ClientId,
    pub host_glyphset_xid: crate::backend::GlyphSetHandle,
}

#[derive(Debug)]
pub struct ResourceTable {
    windows: HashMap<u32, Window>,
    pixmaps: HashMap<u32, Pixmap>,
    gcs: HashMap<u32, Gc>,
    fonts: HashMap<u32, Font>,
    cursors: HashMap<u32, Cursor>,
    pub pictures: HashMap<u32, PictureState>,
    pub glyphsets: HashMap<u32, GlyphSetState>,
    host_glyphset_refcounts: HashMap<u32, usize>,
    visuals: HashMap<u32, Visual>,
    colormaps: HashMap<u32, Colormap>,
}

impl Default for ResourceTable {
    fn default() -> Self {
        let mut windows = HashMap::new();
        windows.insert(
            ROOT_WINDOW.0,
            Window {
                id: ROOT_WINDOW,
                parent: ROOT_WINDOW,
                children: Vec::new(),
                x: 0,
                y: 0,
                width: 800,
                height: 600,
                border_width: 0,
                depth: 24,
                visual: ROOT_VISUAL,
                class: WindowClass::InputOutput,
                map_state: MapState::Viewable,
                background_pixel: 0x00ff_ffff,
                background_pixmap: None,
                background_none: false,
                background_pixmap_host_xid: None,
                border_pixmap_host_xid: None,
                override_redirect: false,
                bit_gravity: 0,
                win_gravity: 1,
                backing_store: 0,
                backing_planes: u32::MAX,
                backing_pixel: 0,
                save_under: false,
                do_not_propagate_mask: 0,
                colormap: ROOT_COLORMAP,
                cursor: None,
                owner: SERVER_OWNER,
                properties: HashMap::new(),
                host_xid: None,
                composite_named_pixmaps: Vec::new(),
                redirected_backing: None,
            },
        );

        // COW is NOT pre-seeded. Per the COW lifecycle spec (invariant 4),
        // the overlay window exists fully or not at all; it is materialized
        // on the 0→1 refcount transition of `GetOverlayWindow` and torn
        // down on the 1→0 of `ReleaseOverlayWindow`.

        // Seed the visual + colormap tables with the same pair the setup
        // reply advertises. `host_visual_xid` / `host_colormap_xid` stay
        // `None` until `HostX11` init pushes the probed values in via
        // [`set_visual_host_xid`] / [`set_colormap_host_xid`].
        let mut visuals = HashMap::new();
        visuals.insert(
            ROOT_VISUAL.0,
            Visual {
                id: ROOT_VISUAL,
                class: VISUAL_CLASS_TRUE_COLOR,
                depth: 24,
                bits_per_rgb: 8,
                colormap_entries: 256,
                red_mask: 0x00ff_0000,
                green_mask: 0x0000_ff00,
                blue_mask: 0x0000_00ff,
                alpha_mask: 0,
                host_visual_xid: None,
            },
        );
        visuals.insert(
            ARGB_VISUAL.0,
            Visual {
                id: ARGB_VISUAL,
                class: VISUAL_CLASS_TRUE_COLOR,
                depth: 32,
                bits_per_rgb: 8,
                colormap_entries: 256,
                red_mask: 0x00ff_0000,
                green_mask: 0x0000_ff00,
                blue_mask: 0x0000_00ff,
                alpha_mask: 0xff00_0000,
                host_visual_xid: None,
            },
        );

        let mut colormaps = HashMap::new();
        colormaps.insert(
            ROOT_COLORMAP.0,
            Colormap {
                id: ROOT_COLORMAP,
                visual: ROOT_VISUAL,
                host_colormap_xid: None,
                owner: yserver_protocol::x11::ClientId(0),
            },
        );
        colormaps.insert(
            ARGB_COLORMAP.0,
            Colormap {
                id: ARGB_COLORMAP,
                visual: ARGB_VISUAL,
                host_colormap_xid: None,
                owner: yserver_protocol::x11::ClientId(0),
            },
        );

        Self {
            windows,
            pixmaps: HashMap::new(),
            gcs: HashMap::new(),
            fonts: HashMap::new(),
            cursors: HashMap::new(),
            pictures: HashMap::new(),
            glyphsets: HashMap::new(),
            host_glyphset_refcounts: HashMap::new(),
            visuals,
            colormaps,
        }
    }
}

/// Index where a new "top" child should be inserted under `parent`. If
/// the COW is currently the topmost child (last in the `children` vec
/// per yserver's `children[len-1] == top` convention), insert below it;
/// otherwise insert at the very end. Mirrors Xorg's
/// `CompositeRealChildHead` (composite/compwindow.c:761-792).
///
/// `parent` is typically `ROOT_WINDOW` (COW's parent); calling on other
/// parents is a no-op (returns `children.len()`).
#[must_use]
pub(crate) fn cow_aware_top_index(parent: &Window) -> usize {
    match parent.children.last() {
        Some(&last) if last == COMPOSITE_OVERLAY_WINDOW => parent.children.len() - 1,
        _ => parent.children.len(),
    }
}

impl ResourceTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn visual(&self, id: ResourceId) -> Option<&Visual> {
        self.visuals.get(&id.0)
    }

    /// Returns `true` if `id` corresponds to any allocated resource
    /// (window, pixmap, gc, font, cursor, colormap, picture, or
    /// glyphset). Used by `CreateXxx` opcodes to detect a
    /// `BadIDChoice` violation when a client tries to reuse an ID.
    ///
    /// NOTE: covers ResourceTable namespaces only; XC-MISC's
    /// `ServerState::xid_occupied` additionally covers the extension
    /// maps — extend BOTH when adding an XID namespace.
    pub fn xid_in_use(&self, id: ResourceId) -> bool {
        let id = id.0;
        self.windows.contains_key(&id)
            || self.pixmaps.contains_key(&id)
            || self.gcs.contains_key(&id)
            || self.fonts.contains_key(&id)
            || self.cursors.contains_key(&id)
            || self.colormaps.contains_key(&id)
            || self.pictures.contains_key(&id)
            || self.glyphsets.contains_key(&id)
    }

    /// Append all table-owned XIDs within `base..=base|mask` to `out`
    /// (the 8 ResourceTable namespaces; extension-map namespaces are
    /// appended by `ServerState::used_xids_in`). XC-MISC GetXIDRange
    /// support.
    pub fn collect_xids_in(&self, base: u32, mask: u32, out: &mut Vec<u32>) {
        let in_range = |id: &&u32| (**id & !mask) == base;
        out.extend(self.windows.keys().filter(in_range));
        out.extend(self.pixmaps.keys().filter(in_range));
        out.extend(self.gcs.keys().filter(in_range));
        out.extend(self.fonts.keys().filter(in_range));
        out.extend(self.cursors.keys().filter(in_range));
        out.extend(self.colormaps.keys().filter(in_range));
        out.extend(self.pictures.keys().filter(in_range));
        out.extend(self.glyphsets.keys().filter(in_range));
    }

    /// Seed a minimal GC into the resource table. For use in tests only.
    #[cfg(test)]
    pub fn seed_gc_for_test(&mut self, owner: ClientId, id: ResourceId) {
        self.gcs
            .insert(id.0, Gc::with_defaults(id, ROOT_WINDOW, owner));
    }

    /// Seed a minimal Font into the resource table. For use in tests only.
    #[cfg(test)]
    pub fn seed_font_for_test(&mut self, owner: ClientId, id: ResourceId) {
        use crate::backend::FontHandle;
        self.fonts.insert(
            id.0,
            Font {
                id,
                name: String::new(),
                host_xid: FontHandle::from_raw_for_test(1),
                metrics: FontMetrics::default(),
                owner,
            },
        );
    }

    pub fn visuals_iter(&self) -> impl Iterator<Item = &Visual> {
        self.visuals.values()
    }

    pub fn is_known_visual(&self, id: ResourceId) -> bool {
        self.visuals.contains_key(&id.0)
    }

    pub fn set_visual_host_xid(&mut self, id: ResourceId, host_xid: u32) -> bool {
        match self.visuals.get_mut(&id.0) {
            Some(v) => {
                v.host_visual_xid = crate::backend::VisualHandle::from_raw(host_xid);
                true
            }
            None => false,
        }
    }

    pub fn create_colormap(
        &mut self,
        owner: yserver_protocol::x11::ClientId,
        id: ResourceId,
        visual: ResourceId,
    ) {
        self.colormaps.insert(
            id.0,
            Colormap {
                id,
                visual,
                host_colormap_xid: None,
                owner,
            },
        );
    }

    pub fn colormap(&self, id: ResourceId) -> Option<&Colormap> {
        self.colormaps.get(&id.0)
    }

    pub fn set_colormap_host_xid(&mut self, id: ResourceId, host_xid: u32) -> bool {
        match self.colormaps.get_mut(&id.0) {
            Some(c) => {
                c.host_colormap_xid = crate::backend::ColormapHandle::from_raw(host_xid);
                true
            }
            None => false,
        }
    }

    /// First colormap entry whose `visual` matches; we currently keep
    /// one colormap per visual so this is unambiguous.
    /// Remove a colormap from the resource table. Returns `true` if it
    /// existed, `false` otherwise. Caller is responsible for emitting
    /// `ColormapNotify` events and removing from `ServerState::installed_colormaps`.
    /// The default colormap (`ROOT_COLORMAP`) is not freeable per X11
    /// spec — caller should reject before calling.
    pub fn free_colormap(&mut self, id: ResourceId) -> bool {
        self.colormaps.remove(&id.0).is_some()
    }

    pub fn colormap_for_visual(&self, visual: ResourceId) -> Option<&Colormap> {
        self.colormaps.values().find(|c| c.visual == visual)
    }

    pub fn create_window(&mut self, owner: ClientId, request: CreateWindowRequest) {
        // CopyFromParent on visual / depth must inherit from the parent's
        // visual and depth, not from the root. ARGB-parent + CopyFromParent
        // child must produce an ARGB child; otherwise the child's host
        // CreateWindow would forward depth=24 against an ARGB parent and
        // produce a host BadMatch.
        let parent = self.windows.get(&request.parent.0);
        let resolved_depth = if request.depth == 0 {
            parent.map_or(24, |p| p.depth)
        } else {
            request.depth
        };
        let resolved_visual = if request.visual.0 == 0 {
            parent.map_or(ROOT_VISUAL, |p| p.visual)
        } else {
            request.visual
        };
        // Per Xorg `dix/window.c::CreateWindow:769-770`: class
        // CopyFromParent (wire value 0) inherits the parent's
        // resolved class — *not* stored as a sentinel. Storing the
        // literal sentinel made GetWindowAttributes report class=0
        // (xts5 Xlib4 XCreateSimpleWindow-1 / XCreateWindow-7), and
        // confuses any downstream class-based validation (e.g. the
        // child-of-InputOnly BadMatch gate that needs to know the
        // child's effective class).
        let resolved_class = match request.class {
            0 => parent.map_or(WindowClass::InputOutput, |p| p.class),
            other => WindowClass::from_protocol(other),
        };
        let window = Window {
            id: request.window,
            parent: request.parent,
            children: Vec::new(),
            x: request.x,
            y: request.y,
            width: request.width,
            height: request.height,
            border_width: request.border_width,
            depth: resolved_depth,
            visual: resolved_visual,
            class: resolved_class,
            map_state: MapState::Unmapped,
            background_pixel: request.background_pixel.unwrap_or(0x00ff_ffff),
            background_pixmap: request.background_pixmap.filter(|p| p.0 != 0),
            background_pixmap_host_xid: request
                .background_pixmap
                .filter(|p| p.0 > 1)
                .and_then(|p| self.pixmaps.get(&p.0).and_then(|pm| pm.host_xid)),
            // X11 §CreateWindow: background-pixmap DEFAULTS to None;
            // an explicit background-pixel (or a real pixmap /
            // ParentRelative) provides a background.
            background_none: request.background_pixel.is_none()
                && !matches!(request.background_pixmap, Some(p) if p.0 != 0),
            border_pixmap_host_xid: None,
            override_redirect: request.override_redirect.unwrap_or(false),
            bit_gravity: request.bit_gravity.unwrap_or(0),
            win_gravity: request.win_gravity.unwrap_or(1),
            backing_store: request.backing_store.unwrap_or(0),
            backing_planes: request.backing_planes.unwrap_or(u32::MAX),
            backing_pixel: request.backing_pixel.unwrap_or(0),
            save_under: request.save_under.unwrap_or(false),
            do_not_propagate_mask: request.do_not_propagate_mask.unwrap_or(0),
            // CopyFromParent (CW value 0 → Some(None)) or unset → inherit
            // parent's colormap; explicit XID → take it.
            colormap: match request.colormap {
                Some(Some(id)) => id,
                Some(None) | None => parent.map_or(ROOT_COLORMAP, |p| p.colormap),
            },
            cursor: None,
            owner,
            properties: HashMap::new(),
            host_xid: None,
            composite_named_pixmaps: Vec::new(),
            redirected_backing: None,
        };

        let parent_entry = self
            .windows
            .entry(request.parent.0)
            .or_insert_with(|| Window::placeholder(request.parent));
        let insert_at = cow_aware_top_index(parent_entry);
        parent_entry.children.insert(insert_at, request.window);
        self.windows.insert(request.window.0, window);
    }

    pub fn destroy_window(&mut self, id: ResourceId) -> Vec<ResourceId> {
        let mut destroyed = Vec::new();
        self.destroy_window_inner(id, &mut destroyed);
        destroyed
    }

    /// Stage 4e — create the Composite Overlay Window resource record
    /// as a child of root, populate its `host_xid`, and insert it as
    /// the topmost child via `cow_aware_top_index`.
    ///
    /// **Precondition: COW is not currently materialized.** Called by
    /// the core `GetOverlayWindow` handler only on the 0→1 refcount
    /// transition (the backend's `get_overlay_window` returned
    /// `Ok(true)`). Panics if a COW resource record already exists;
    /// repeated `GetOverlayWindow` calls without an intervening release
    /// are refcount-only and never reach this function. This guard is
    /// load-bearing: the COW's resource record may carry live state
    /// across an interactive session (event-mask selections, properties,
    /// children — the compositor stage gets reparented under it),
    /// rebuilding it would silently drop that state.
    pub fn materialize_cow_resource(&mut self, host_xid: crate::backend::WindowHandle) {
        assert!(
            !self.windows.contains_key(&COMPOSITE_OVERLAY_WINDOW.0),
            "materialize_cow_resource: COW already materialized; \
             core handler must only call this on get_overlay_window's \
             Ok(true) (0→1) return, never on subsequent claims"
        );
        // Build the resource record. Geometry mirrors Xorg
        // `compoverlay.c:compCreateOverlayWindow`: full screen, depth =
        // root depth, override-redirect, no automatic background.
        let root_w = self.window(ROOT_WINDOW).map_or(1, |r| r.width);
        let root_h = self.window(ROOT_WINDOW).map_or(1, |r| r.height);
        let root_visual = self.window(ROOT_WINDOW).map_or(ROOT_VISUAL, |r| r.visual);
        let cow_window = Window {
            id: COMPOSITE_OVERLAY_WINDOW,
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: root_w,
            height: root_h,
            depth: 24,
            visual: root_visual,
            class: WindowClass::InputOutput,
            map_state: MapState::Viewable,
            override_redirect: true,
            host_xid: Some(host_xid),
            // Remaining fields default per `Window::placeholder` (background
            // none, empty properties, no cursor, etc.).
            ..Window::placeholder(COMPOSITE_OVERLAY_WINDOW)
        };
        self.windows.insert(COMPOSITE_OVERLAY_WINDOW.0, cow_window);

        // Insert COW into root.children at the cow-aware top slot. The
        // post-condition: COW is the last entry in root.children.
        if let Some(root) = self.windows.get_mut(&ROOT_WINDOW.0) {
            let idx = cow_aware_top_index(root);
            root.children.insert(idx, COMPOSITE_OVERLAY_WINDOW);
        }
    }

    /// Stage 4e — symmetric teardown. Remove the COW from root.children
    /// and drop the resource record. Called by the core
    /// `ReleaseOverlayWindow` handler only on the 1→0 refcount
    /// transition (`backend.release_overlay_window` returned
    /// `Ok(true)`). Drops any live COW-local state (event-mask
    /// selections, property store, child window list) — by definition
    /// the compositor has explicitly released, so this is intentional.
    pub fn destroy_cow_resource(&mut self) {
        if let Some(root) = self.windows.get_mut(&ROOT_WINDOW.0) {
            root.children.retain(|&c| c != COMPOSITE_OVERLAY_WINDOW);
        }
        self.windows.remove(&COMPOSITE_OVERLAY_WINDOW.0);
    }

    #[must_use]
    pub fn configure_notify_above_sibling(&self, id: ResourceId) -> Option<ResourceId> {
        let window = self.windows.get(&id.0)?;
        let parent = self.windows.get(&window.parent.0)?;
        let index = parent.children.iter().position(|child| *child == id)?;
        index
            .checked_sub(1)
            .and_then(|i| parent.children.get(i))
            .copied()
    }

    /// Walk the about-to-be-destroyed window subtree and collect every
    /// retained bg-pixmap host XID. Caller frees them on the host.
    pub fn collect_bg_pixmap_host_xids(&self, root: ResourceId) -> Vec<u32> {
        let mut out = Vec::new();
        self.collect_bg_pixmap_host_xids_inner(root, &mut out);
        out
    }

    fn collect_bg_pixmap_host_xids_inner(&self, id: ResourceId, out: &mut Vec<u32>) {
        let Some(window) = self.windows.get(&id.0) else {
            return;
        };
        if let Some(xid) = window.background_pixmap_host_xid {
            out.push(xid.as_raw());
        }
        for child in &window.children {
            self.collect_bg_pixmap_host_xids_inner(*child, out);
        }
    }

    fn destroy_window_inner(&mut self, id: ResourceId, destroyed: &mut Vec<ResourceId>) {
        // X11 spec: "If the argument window is a root window, then this
        // request has no effect." Without this guard a misbehaved client
        // (or xts5 Xlib4/XDestroyWindow assertion 5, which calls
        // `XDestroyWindow(root)` and verifies the root and a sibling
        // window are still valid) would silently delete the root entry
        // from the windows table — leaving every subsequent client to
        // receive `BadWindow` from `GetGeometry` / `QueryTree` /
        // `GetWindowAttributes` on `ROOT_WINDOW`.
        if id == ROOT_WINDOW {
            return;
        }
        let Some(window) = self.windows.remove(&id.0) else {
            return;
        };
        if let Some(parent) = self.windows.get_mut(&window.parent.0) {
            parent.children.retain(|child| *child != id);
        }
        destroyed.push(id);
        for child in window.children {
            self.destroy_window_inner(child, destroyed);
        }
    }

    /// Apply attribute changes. Returns the previous bg-pixmap host XID if it
    /// was replaced — the caller should free it on the host since the X server
    /// no longer needs it.
    pub fn change_window_attributes(
        &mut self,
        request: ChangeWindowAttributesRequest,
    ) -> Option<crate::backend::PixmapHandle> {
        let mut previous_bg_host_xid: Option<crate::backend::PixmapHandle> = None;
        let new_bg_host_xid: Option<Option<crate::backend::PixmapHandle>> =
            if let Some(bg_pixmap) = request.background_pixmap {
                if bg_pixmap.0 == 0 {
                    Some(None)
                } else {
                    let host = self.pixmaps.get(&bg_pixmap.0).and_then(|p| p.host_xid);
                    Some(host)
                }
            } else {
                None
            };
        // Resolve `CWColormap = CopyFromParent` (XID 0) against the parent
        // colormap *before* the borrow below.
        let resolved_colormap: Option<ResourceId> = match request.colormap {
            Some(Some(id)) => Some(id),
            Some(None) => {
                let parent_id = self
                    .windows
                    .get(&request.window.0)
                    .map(|w| w.parent)
                    .unwrap_or(ROOT_WINDOW);
                Some(
                    self.windows
                        .get(&parent_id.0)
                        .map_or(ROOT_COLORMAP, |p| p.colormap),
                )
            }
            None => None,
        };

        if let Some(window) = self.windows.get_mut(&request.window.0) {
            if let Some(bg_pixmap) = request.background_pixmap {
                let new_resource_id = if bg_pixmap.0 == 0 {
                    None
                } else {
                    Some(bg_pixmap)
                };
                if window.background_pixmap_host_xid != new_bg_host_xid.flatten() {
                    previous_bg_host_xid = window.background_pixmap_host_xid;
                }
                window.background_pixmap = new_resource_id;
                if let Some(host) = new_bg_host_xid {
                    window.background_pixmap_host_xid = host;
                }
                // CWBackPixmap = 0 → background None (clears leave
                // contents untouched); any other value provides a bg.
                window.background_none = bg_pixmap.0 == 0;
            }
            if let Some(background_pixel) = request.background_pixel {
                window.background_pixel = background_pixel;
                // An explicit background-pixel always provides a bg
                // (precedence over background-pixmap, X11 §CWA).
                window.background_none = false;
            }
            if let Some(v) = request.bit_gravity {
                window.bit_gravity = v;
            }
            if let Some(v) = request.win_gravity {
                window.win_gravity = v;
            }
            if let Some(v) = request.backing_store {
                window.backing_store = v;
            }
            if let Some(v) = request.backing_planes {
                window.backing_planes = v;
            }
            if let Some(v) = request.backing_pixel {
                window.backing_pixel = v;
            }
            if let Some(v) = request.override_redirect {
                window.override_redirect = v;
            }
            if let Some(v) = request.save_under {
                window.save_under = v;
            }
            if let Some(v) = request.do_not_propagate_mask {
                window.do_not_propagate_mask = v;
            }
            if let Some(cm) = resolved_colormap {
                window.colormap = cm;
            }
            if let Some(cursor) = request.cursor {
                window.cursor = Some(cursor);
            }
        }

        previous_bg_host_xid
    }

    pub fn configure_window(&mut self, request: ConfigureWindowRequest) -> Option<&Window> {
        {
            let window = self.windows.get_mut(&request.window.0)?;
            if let Some(x) = request.x {
                window.x = x;
            }
            if let Some(y) = request.y {
                window.y = y;
            }
            if let Some(width) = request.width {
                window.width = width;
            }
            if let Some(height) = request.height {
                window.height = height;
            }
            if let Some(border_width) = request.border_width {
                window.border_width = border_width;
            }
        }
        self.restack_window(request.window, request.sibling, request.stack_mode);
        self.windows.get(&request.window.0)
    }

    fn restack_window(
        &mut self,
        window_id: ResourceId,
        sibling: Option<ResourceId>,
        stack_mode: Option<u8>,
    ) {
        let Some(stack_mode) = stack_mode else {
            return;
        };
        let Some(parent_id) = self.windows.get(&window_id.0).map(|window| window.parent) else {
            return;
        };
        let Some(action) = self.resolve_restack_action(window_id, parent_id, sibling, stack_mode)
        else {
            return;
        };
        let Some(parent) = self.windows.get_mut(&parent_id.0) else {
            return;
        };
        let Some(index) = parent.children.iter().position(|child| *child == window_id) else {
            return;
        };
        match action {
            RestackAction::NoOp => {}
            RestackAction::Top => {
                let window = parent.children.remove(index);
                let insert_at = cow_aware_top_index(parent);
                parent.children.insert(insert_at, window);
            }
            RestackAction::Bottom => {
                let window = parent.children.remove(index);
                parent.children.insert(0, window);
            }
            RestackAction::AboveSibling(sibling_id) => {
                let window = parent.children.remove(index);
                let insert_at = if sibling_id == COMPOSITE_OVERLAY_WINDOW {
                    // Cap: cannot land above COW. Use the same just-below-COW slot
                    // the cow_aware_top_index helper computes.
                    cow_aware_top_index(parent)
                } else {
                    let sibling_index = parent
                        .children
                        .iter()
                        .position(|child| *child == sibling_id);
                    sibling_index.map_or(parent.children.len(), |i| i + 1)
                };
                parent.children.insert(insert_at, window);
            }
            RestackAction::BelowSibling(sibling_id) => {
                let window = parent.children.remove(index);
                let sibling_index = parent
                    .children
                    .iter()
                    .position(|child| *child == sibling_id);
                let insert_at = sibling_index.unwrap_or(0);
                parent.children.insert(insert_at, window);
            }
        }
    }

    /// Resolve a `ConfigureWindow` stack-mode + optional sibling to the
    /// concrete restack action per the X11 protocol. `None` means the
    /// request is malformed (window not in parent's child list, sibling
    /// not actually a sibling, unknown stack mode) and should be skipped.
    ///
    /// X11 stack-mode codes: 0=Above, 1=Below, 2=TopIf, 3=BottomIf,
    /// 4=Opposite. TopIf/BottomIf/Opposite are *conditional* on the
    /// current occlusion state; Above/Below are unconditional.
    fn resolve_restack_action(
        &self,
        window_id: ResourceId,
        parent_id: ResourceId,
        sibling: Option<ResourceId>,
        stack_mode: u8,
    ) -> Option<RestackAction> {
        let parent = self.windows.get(&parent_id.0)?;
        let window_index = parent.children.iter().position(|c| *c == window_id)?;
        if let Some(sibling_id) = sibling
            && !parent.children.contains(&sibling_id)
        {
            return None;
        }

        Some(match stack_mode {
            0 => match sibling {
                Some(sib) => RestackAction::AboveSibling(sib),
                None => RestackAction::Top,
            },
            1 => match sibling {
                Some(sib) => RestackAction::BelowSibling(sib),
                None => RestackAction::Bottom,
            },
            2 => {
                if self.any_sibling_occludes_window(parent_id, window_index, sibling) {
                    RestackAction::Top
                } else {
                    RestackAction::NoOp
                }
            }
            3 => {
                if self.window_occludes_any_sibling(parent_id, window_index, sibling) {
                    RestackAction::Bottom
                } else {
                    RestackAction::NoOp
                }
            }
            4 => {
                if self.any_sibling_occludes_window(parent_id, window_index, sibling) {
                    RestackAction::Top
                } else if self.window_occludes_any_sibling(parent_id, window_index, sibling) {
                    RestackAction::Bottom
                } else {
                    RestackAction::NoOp
                }
            }
            _ => return None,
        })
    }

    /// True iff some sibling currently stacked above `window` (at
    /// `window_index` in the parent's child list) overlaps it. Both
    /// windows must be mapped per X11's occlusion definition. With
    /// `sibling = Some(_)`, only that sibling is considered.
    fn any_sibling_occludes_window(
        &self,
        parent_id: ResourceId,
        window_index: usize,
        sibling: Option<ResourceId>,
    ) -> bool {
        let Some(parent) = self.windows.get(&parent_id.0) else {
            return false;
        };
        let Some(window_id) = parent.children.get(window_index) else {
            return false;
        };
        let Some(window) = self.windows.get(&window_id.0) else {
            return false;
        };
        if window.map_state == MapState::Unmapped {
            return false;
        }
        for (i, other_id) in parent.children.iter().enumerate() {
            if i <= window_index {
                continue;
            }
            if let Some(sib) = sibling
                && *other_id != sib
            {
                continue;
            }
            let Some(other) = self.windows.get(&other_id.0) else {
                continue;
            };
            if other.map_state == MapState::Unmapped {
                continue;
            }
            if window_rects_overlap(window, other) {
                return true;
            }
        }
        false
    }

    /// True iff `window` (at `window_index` in the parent's child list)
    /// currently occludes some sibling stacked below it. With `sibling =
    /// Some(_)`, only that sibling is considered.
    fn window_occludes_any_sibling(
        &self,
        parent_id: ResourceId,
        window_index: usize,
        sibling: Option<ResourceId>,
    ) -> bool {
        let Some(parent) = self.windows.get(&parent_id.0) else {
            return false;
        };
        let Some(window_id) = parent.children.get(window_index) else {
            return false;
        };
        let Some(window) = self.windows.get(&window_id.0) else {
            return false;
        };
        if window.map_state == MapState::Unmapped {
            return false;
        }
        for (i, other_id) in parent.children.iter().enumerate() {
            if i >= window_index {
                continue;
            }
            if let Some(sib) = sibling
                && *other_id != sib
            {
                continue;
            }
            let Some(other) = self.windows.get(&other_id.0) else {
                continue;
            };
            if other.map_state == MapState::Unmapped {
                continue;
            }
            if window_rects_overlap(window, other) {
                return true;
            }
        }
        false
    }

    #[must_use]
    pub fn map_window(&mut self, id: ResourceId) -> bool {
        // A window is Viewable only if it is mapped AND all ancestors
        // up to the root are also mapped (Viewable). If any ancestor
        // is not Viewable, the window becomes Unviewable instead.
        let parent_id = self.windows.get(&id.0).map(|w| w.parent);
        let parent_viewable = match parent_id {
            Some(pid) if pid.0 == id.0 => true, // root: mapping its parent (itself) is N/A
            Some(pid) => self
                .windows
                .get(&pid.0)
                .is_some_and(|p| p.map_state == MapState::Viewable),
            None => false,
        };
        let was_unmapped = if let Some(window) = self.windows.get_mut(&id.0) {
            let was_unmapped = window.map_state == MapState::Unmapped;
            window.map_state = if parent_viewable {
                MapState::Viewable
            } else {
                MapState::Unviewable
            };
            was_unmapped
        } else {
            return false;
        };
        // If we just transitioned to Viewable, promote any descendant
        // that was Unviewable (i.e. mapped before its ancestor became
        // viewable — e.g. xclock's child window after the WM frame
        // reparent: child was MapSubwindows'd while parent was unmapped,
        // staying Unviewable; mapping the parent must propagate down or
        // Expose-fanout silently skips it because of the Viewable filter).
        if parent_viewable {
            self.promote_unviewable_descendants(id);
        }
        was_unmapped
    }

    fn promote_unviewable_descendants(&mut self, root: ResourceId) {
        let children: Vec<ResourceId> = self
            .windows
            .get(&root.0)
            .map(|w| w.children.clone())
            .unwrap_or_default();
        for child in children {
            let promoted = if let Some(w) = self.windows.get_mut(&child.0) {
                if w.map_state == MapState::Unviewable {
                    w.map_state = MapState::Viewable;
                    true
                } else {
                    false
                }
            } else {
                false
            };
            // Only recurse into descendants whose own state allows
            // visibility (Viewable now, or already Viewable). Stopping
            // at Unmapped descendants matches X11's "first Unmapped
            // ancestor halts the cascade" semantics.
            let should_recurse = self
                .windows
                .get(&child.0)
                .is_some_and(|w| w.map_state == MapState::Viewable);
            if promoted || should_recurse {
                self.promote_unviewable_descendants(child);
            }
        }
    }

    #[must_use]
    pub fn unmap_window(&mut self, id: ResourceId) -> bool {
        if id == ROOT_WINDOW {
            return false;
        }
        let Some(window) = self.windows.get_mut(&id.0) else {
            return false;
        };
        let was_mapped = window.map_state != MapState::Unmapped;
        window.map_state = MapState::Unmapped;
        was_mapped
    }

    pub fn window(&self, id: ResourceId) -> Option<&Window> {
        self.windows.get(&id.0)
    }

    pub fn window_mut(&mut self, id: ResourceId) -> Option<&mut Window> {
        self.windows.get_mut(&id.0)
    }

    pub fn children(&self, parent: ResourceId) -> &[ResourceId] {
        self.windows
            .get(&parent.0)
            .map_or(&[], |window| window.children.as_slice())
    }

    /// Phase-2 naive CirculateWindow: rotate the back child to the front
    /// (`direction = 0`, RaiseLowest) or the front child to the back
    /// (`direction = 1`, LowerHighest). Returns the moved child if any.
    /// Real obscuring detection is a Phase 4+ compositor concern.
    ///
    /// On `ROOT_WINDOW`, the Composite Overlay Window (COW) is excluded from
    /// the rotation — it stays pinned at the top, mirroring Xorg's
    /// `CompositeRealChildHead` semantics.
    pub fn circulate_window(&mut self, container: ResourceId, direction: u8) -> Option<ResourceId> {
        let parent = self.windows.get_mut(&container.0)?;
        // Slice end: exclude COW when operating on root. Anywhere else this
        // is just `children.len()`.
        let top = if container == ROOT_WINDOW {
            cow_aware_top_index(parent)
        } else {
            parent.children.len()
        };
        let kids = &mut parent.children;
        if top < 2 {
            return None;
        }
        match direction {
            0 => {
                let last = kids.remove(top - 1);
                kids.insert(0, last);
                Some(last)
            }
            1 => {
                let first = kids.remove(0);
                // `top` was the slice end before the remove; after removing
                // index 0 the slice end shifts left by one, so insert at
                // `top - 1` to land just below COW (or at the very end when
                // COW is absent).
                kids.insert(top - 1, first);
                Some(first)
            }
            _ => None,
        }
    }

    pub fn mapped_children_bottom_to_top(&self, parent: ResourceId) -> Option<Vec<ResourceId>> {
        let parent = self.windows.get(&parent.0)?;
        Some(
            parent
                .children
                .iter()
                .copied()
                .filter(|child| {
                    self.windows
                        .get(&child.0)
                        .is_some_and(|w| w.map_state != MapState::Unmapped)
                })
                .collect(),
        )
    }

    #[must_use]
    pub fn is_descendant_of(&self, candidate: ResourceId, ancestor: ResourceId) -> bool {
        let mut current = candidate;
        let mut seen = 0usize;
        while current != ROOT_WINDOW && seen <= self.windows.len() {
            let Some(window) = self.windows.get(&current.0) else {
                return false;
            };
            if window.parent == ancestor {
                return true;
            }
            if window.parent == current {
                return false;
            }
            current = window.parent;
            seen += 1;
        }
        false
    }

    pub fn window_owner(&self, id: ResourceId) -> Option<ClientId> {
        self.windows.get(&id.0).map(|w| w.owner)
    }

    pub fn parent_of(&self, id: ResourceId) -> Option<ResourceId> {
        self.windows.get(&id.0).map(|w| w.parent)
    }

    /// Walk mapped descendants of `top_level` and return the intersection
    /// rectangles (in each descendant's local coordinates) that overlap an
    /// expose region given in top-level coordinates. Used to synthesize
    /// `Expose` events for sub-windows when only the top-level subwindow has a
    /// host counterpart — without this, dragging a window across another
    /// leaves child sub-windows (titlebars, content panes) unrepainted because
    /// the host server never knows they exist.
    #[must_use]
    pub fn descendants_in_exposed_area(
        &self,
        top_level: ResourceId,
        ex: i16,
        ey: i16,
        ew: u16,
        eh: u16,
    ) -> Vec<ExposedRect> {
        let mut out = Vec::new();
        let er = i32::from(ex) + i32::from(ew);
        let eb = i32::from(ey) + i32::from(eh);
        self.descendants_in_exposed_area_inner(
            top_level,
            0,
            0,
            i32::from(ex),
            i32::from(ey),
            er,
            eb,
            &mut out,
        );
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn descendants_in_exposed_area_inner(
        &self,
        parent_id: ResourceId,
        parent_x: i32,
        parent_y: i32,
        ex: i32,
        ey: i32,
        er: i32,
        eb: i32,
        out: &mut Vec<ExposedRect>,
    ) {
        let Some(parent) = self.windows.get(&parent_id.0) else {
            return;
        };
        for child_id in &parent.children {
            let Some(child) = self.windows.get(&child_id.0) else {
                continue;
            };
            if child.map_state == MapState::Unmapped {
                continue;
            }
            let cx = parent_x + i32::from(child.x);
            let cy = parent_y + i32::from(child.y);
            let cr = cx + i32::from(child.width);
            let cb = cy + i32::from(child.height);
            let ix = ex.max(cx);
            let iy = ey.max(cy);
            let ir = er.min(cr);
            let ib = eb.min(cb);
            if ir <= ix || ib <= iy {
                continue;
            }
            let local_x = i16::try_from(ix - cx).unwrap_or(0);
            let local_y = i16::try_from(iy - cy).unwrap_or(0);
            let local_w = u16::try_from(ir - ix).unwrap_or(0);
            let local_h = u16::try_from(ib - iy).unwrap_or(0);
            if local_w > 0 && local_h > 0 {
                out.push(ExposedRect {
                    window: *child_id,
                    x: local_x,
                    y: local_y,
                    width: local_w,
                    height: local_h,
                });
            }
            self.descendants_in_exposed_area_inner(*child_id, cx, cy, ex, ey, er, eb, out);
        }
    }

    pub fn pointer_target_at(
        &self,
        top_level: ResourceId,
        x: i16,
        y: i16,
    ) -> Option<(ResourceId, i16, i16)> {
        let top = self.windows.get(&top_level.0)?;
        if top.map_state == MapState::Unmapped {
            return None;
        }
        let mut best = (top_level, x, y);
        self.pointer_target_at_inner(top_level, x, y, &mut best);
        Some(best)
    }

    fn pointer_target_at_inner(
        &self,
        parent: ResourceId,
        parent_x: i16,
        parent_y: i16,
        best: &mut (ResourceId, i16, i16),
    ) {
        let Some(parent_window) = self.windows.get(&parent.0) else {
            return;
        };
        for child_id in parent_window.children.iter().rev() {
            let Some(child) = self.windows.get(&child_id.0) else {
                continue;
            };
            if child.map_state == MapState::Unmapped {
                continue;
            }
            let child_x = parent_x.wrapping_sub(child.x);
            let child_y = parent_y.wrapping_sub(child.y);
            if child_x < 0
                || child_y < 0
                || child_x >= i16::try_from(child.width).unwrap_or(i16::MAX)
                || child_y >= i16::try_from(child.height).unwrap_or(i16::MAX)
            {
                continue;
            }
            *best = (*child_id, child_x, child_y);
            self.pointer_target_at_inner(*child_id, child_x, child_y, best);
            return;
        }
    }

    pub fn reparent_window(
        &mut self,
        request: ReparentWindowRequest,
    ) -> Result<ReparentResult, ReparentWindowError> {
        if request.window == ROOT_WINDOW
            || request.window == request.parent
            || self.is_descendant_of(request.parent, request.window)
        {
            return Err(ReparentWindowError::BadMatch);
        }
        let Some(window) = self.windows.get(&request.window.0) else {
            return Err(ReparentWindowError::BadWindow);
        };
        if !self.windows.contains_key(&request.parent.0) {
            return Err(ReparentWindowError::BadWindow);
        }

        let old_parent = window.parent;
        let override_redirect = window.override_redirect;
        let host_xid = window.host_xid;

        if let Some(parent) = self.windows.get_mut(&old_parent.0) {
            parent.children.retain(|child| *child != request.window);
        }
        if let Some(parent) = self.windows.get_mut(&request.parent.0) {
            let insert_at = cow_aware_top_index(parent);
            parent.children.insert(insert_at, request.window);
        }
        let window = self
            .windows
            .get_mut(&request.window.0)
            .expect("window validated above");
        window.parent = request.parent;
        window.x = request.x;
        window.y = request.y;
        // Phase 3.6 Step 4a forwards XReparentWindow to the host, so
        // the host subwindow stays alive and continues to be the
        // rendering target. (Pre-Step-4a code destroyed the host
        // subwindow when a top-level moved away from root and cleared
        // host_xid here; that's no longer correct.)

        Ok(ReparentResult {
            window: request.window,
            old_parent,
            new_parent: request.parent,
            x: request.x,
            y: request.y,
            override_redirect,
            host_xid,
        })
    }

    #[must_use]
    pub fn window_property(&self, w: ResourceId, atom: AtomId) -> Option<&PropertyValue> {
        self.windows.get(&w.0)?.properties.get(&atom)
    }

    pub fn set_window_property(&mut self, w: ResourceId, atom: AtomId, value: PropertyValue) {
        if let Some(window) = self.windows.get_mut(&w.0) {
            window.properties.insert(atom, value);
        }
    }

    pub fn delete_window_property(&mut self, w: ResourceId, atom: AtomId) -> Option<PropertyValue> {
        self.windows.get_mut(&w.0)?.properties.remove(&atom)
    }

    pub fn create_pixmap(&mut self, owner: ClientId, request: CreatePixmapRequest) {
        self.pixmaps.insert(
            request.pixmap.0,
            Pixmap {
                id: request.pixmap,
                drawable: request.drawable,
                width: request.width,
                height: request.height,
                depth: request.depth,
                owner,
                host_xid: None,
            },
        );
    }

    pub fn free_pixmap(&mut self, id: ResourceId) -> Option<Pixmap> {
        self.pixmaps.remove(&id.0)
    }

    pub fn pixmap(&self, id: ResourceId) -> Option<&Pixmap> {
        self.pixmaps.get(&id.0)
    }

    #[must_use]
    pub fn composite_named_pixmap_owner_window(&self, pixmap: ResourceId) -> Option<ResourceId> {
        self.windows.values().find_map(|window| {
            window
                .composite_named_pixmaps
                .iter()
                .any(|alias| alias.client_pixmap == pixmap)
                .then_some(window.id)
        })
    }

    #[must_use]
    pub fn set_pixmap_host_xid(
        &mut self,
        id: ResourceId,
        host_handle: crate::backend::PixmapHandle,
    ) -> bool {
        if let Some(pixmap) = self.pixmaps.get_mut(&id.0) {
            pixmap.host_xid = Some(host_handle);
            true
        } else {
            false
        }
    }

    pub fn window_background_pixmap_host_xid(&self, window_id: ResourceId) -> Option<u32> {
        // Use the snapshotted host XID rather than re-resolving the pixmap, so
        // it remains valid after the client frees the original pixmap (X11
        // semantics: the server retains the bg pixmap independent of refs).
        self.windows
            .get(&window_id.0)?
            .background_pixmap_host_xid
            .map(|h| h.as_raw())
    }

    /// Resolve the effective background by following `ParentRelative`
    /// links upward until a concrete pixel or pixmap is found.
    pub fn window_resolved_background(
        &self,
        window_id: ResourceId,
    ) -> Option<ResolvedWindowBackground> {
        self.window_resolved_background_inner(window_id, &mut Vec::new())
    }

    fn window_resolved_background_inner(
        &self,
        window_id: ResourceId,
        seen: &mut Vec<ResourceId>,
    ) -> Option<ResolvedWindowBackground> {
        if seen.contains(&window_id) {
            return None;
        }
        seen.push(window_id);

        let window = self.windows.get(&window_id.0)?;
        if matches!(window.background_pixmap, Some(bg) if bg.0 == 1) {
            // ParentRelative: inherit the parent's background; the
            // tile stays aligned to the PARENT's origin, so this
            // window samples it shifted by its own position.
            let mut resolved = self.window_resolved_background_inner(window.parent, seen)?;
            resolved.tile_origin_offset.0 += i32::from(window.x);
            resolved.tile_origin_offset.1 += i32::from(window.y);
            return Some(resolved);
        }
        if window.background_none {
            // Background None: clears/exposes leave contents
            // untouched (X11 §ClearArea / miPaintWindow early-out).
            return None;
        }

        Some(ResolvedWindowBackground {
            background_pixel: window.background_pixel,
            background_pixmap_host_xid: window.background_pixmap_host_xid,
            tile_origin_offset: (0, 0),
        })
    }

    /// Returns true if any window currently uses `host_xid` as its background.
    /// Used by FreePixmap to skip releasing host pixmaps still owned by a window.
    pub fn host_xid_referenced_by_window_bg(&self, host_xid: crate::backend::PixmapHandle) -> bool {
        self.windows
            .values()
            .any(|w| w.background_pixmap_host_xid == Some(host_xid))
    }

    /// Returns true if any GC currently retains `host_xid` as its clip mask,
    /// tile, or stipple backing. Used by FreePixmap to keep host-side pixmaps
    /// alive after the client frees the source pixmap resource.
    pub fn host_xid_referenced_by_gc(&self, host_xid: crate::backend::PixmapHandle) -> bool {
        self.gcs.values().any(|gc| {
            gc.clip_pixmap_host_xid == Some(host_xid)
                || gc.tile_host_xid == Some(host_xid)
                || gc.stipple_host_xid == Some(host_xid)
        })
    }

    /// Returns true if a live client pixmap resource still owns `host_xid`.
    ///
    /// X11 lifetime rule for window backgrounds: installing a pixmap as a
    /// window background does NOT transfer ownership to the server — the
    /// client's pixmap stays fully usable (drawable, re-installable as bg)
    /// until the client calls FreePixmap. The host-side pixmap may only be
    /// released once BOTH are true: the client resource is gone AND no
    /// window background references it. The CWA bg-replacement path freed
    /// on the second condition alone, destroying client-owned pixmaps —
    /// observed as e16 menu items blanking on hover (e16 swaps the item
    /// window's bg between a kept "normal" pixmap and a transient hilite
    /// pixmap; the swap to hilite host-freed the still-owned normal
    /// pixmap, so the un-hover restore painted nothing).
    pub fn host_xid_owned_by_pixmap(&self, host_xid: crate::backend::PixmapHandle) -> bool {
        self.pixmaps.values().any(|p| p.host_xid == Some(host_xid))
    }

    #[must_use]
    pub fn host_drawable_target(&self, id: ResourceId) -> Option<HostDrawableTarget> {
        // Phase 3.6 Step 6: every InputOutput window has its own host_xid
        // (Step 2 invariant) and the host tree mirrors the local tree
        // (Step 4 reparent + configure forwarding), so drawing on a
        // sub-window targets that sub-window's host xid directly with no
        // coordinate translation. Windows without their own host_xid
        // (InputOnly, transient pre-init state) yield None — the caller
        // drops the draw silently, same as before (drawing on InputOnly
        // is undefined / spec-error territory).
        if let Some(window) = self.windows.get(&id.0) {
            // L2 plan B.7 — if the window is redirected, paint
            // routes to the off-screen backing instead of the
            // window's own host XID. The `Window` variant is still
            // emitted so the caller's damage / scissor logic stays
            // window-keyed via `nested`; only the host-side paint
            // XID switches.
            //
            // The variant carries the backing's pixmap raw inside
            // a `WindowHandle` newtype — a typed lie, since the
            // backend treats both pixmap and window XIDs as raw
            // `u32` keys against `self.pixmaps` / `self.windows`
            // (it consults both maps). No production caller
            // pattern-matches on `Window { host_xid: WindowHandle, .. }`
            // and routes it back into a window-only API.
            if let Some(backing) = window.redirected_backing.as_ref() {
                let host_xid =
                    crate::backend::WindowHandle::from_raw_panicking(backing.host_pixmap.as_raw());
                return Some(HostDrawableTarget::Window {
                    nested: id,
                    host_xid,
                    depth: backing.depth,
                });
            }
            let host_xid = window.host_xid?;
            return Some(HostDrawableTarget::Window {
                nested: id,
                host_xid,
                depth: window.depth,
            });
        }

        let pixmap = self.pixmaps.get(&id.0)?;
        Some(HostDrawableTarget::Pixmap {
            nested: id,
            host_xid: pixmap.host_xid?,
            width: pixmap.width,
            height: pixmap.height,
            depth: pixmap.depth,
        })
    }

    pub fn create_gc(&mut self, owner: ClientId, request: CreateGcRequest) {
        let clip_pixmap = match request.clip_mask {
            Some(Some(pixmap)) => Some(pixmap),
            _ => None,
        };
        let clip_pixmap_host_xid = clip_pixmap
            .and_then(|pixmap| self.pixmaps.get(&pixmap.0))
            .and_then(|p| p.host_xid);
        let mut gc = Gc::with_defaults(request.gc, request.drawable, owner);
        gc.depth = self
            .window(request.drawable)
            .map(|w| w.depth)
            .or_else(|| self.pixmap(request.drawable).map(|p| p.depth))
            .unwrap_or(0);
        gc.tile_host_xid = request
            .tile
            .and_then(|pixmap| self.pixmaps.get(&pixmap.0))
            .and_then(|p| p.host_xid);
        gc.stipple_host_xid = request
            .stipple
            .and_then(|pixmap| self.pixmaps.get(&pixmap.0))
            .and_then(|p| p.host_xid);
        gc.clip_pixmap = clip_pixmap;
        gc.clip_pixmap_host_xid = clip_pixmap_host_xid;
        Self::apply_gc_change(
            &mut gc,
            GcChangeView {
                function: request.function,
                plane_mask: request.plane_mask,
                foreground: request.foreground,
                background: request.background,
                line_width: request.line_width,
                line_style: request.line_style,
                cap_style: request.cap_style,
                join_style: request.join_style,
                fill_style: request.fill_style,
                fill_rule: request.fill_rule,
                tile: request.tile,
                stipple: request.stipple,
                tile_x_origin: request.tile_x_origin,
                tile_y_origin: request.tile_y_origin,
                font: request.font,
                subwindow_mode: request.subwindow_mode,
                graphics_exposures: request.graphics_exposures,
                clip_x_origin: request.clip_x_origin,
                clip_y_origin: request.clip_y_origin,
                // CreateGC's clip-mask is consumed by the explicit
                // `clip_pixmap` assignment above; passing it again here
                // would re-clear `clip_rectangles` (irrelevant on a
                // fresh GC) but otherwise harmless. Pass `None` so the
                // helper is purely additive.
                clip_mask: None,
                dash_offset: request.dash_offset,
                dashes: request.dashes,
                arc_mode: request.arc_mode,
            },
        );
        self.gcs.insert(request.gc.0, gc);
    }

    /// Apply a ChangeGC. Caller MUST have validated `request.gc`
    /// exists in the resource table first (handler-side BadGC gate);
    /// otherwise this is a silent no-op. Xorg's `ProcChangeGC`
    /// (`dix/dispatch.c:1580`) emits BadGC via `dixLookupGC` before
    /// any state mutation — yserver mirrors that with the handler-
    /// side check + `get_mut` here.
    pub fn change_gc(&mut self, _requester: ClientId, request: GcChange) {
        let clip_pixmap_host_xid = request
            .clip_mask
            .flatten()
            .and_then(|pixmap| self.pixmaps.get(&pixmap.0))
            .and_then(|p| p.host_xid);
        let tile_host_xid = request
            .tile
            .and_then(|pixmap| self.pixmaps.get(&pixmap.0))
            .and_then(|p| p.host_xid);
        let stipple_host_xid = request
            .stipple
            .and_then(|pixmap| self.pixmaps.get(&pixmap.0))
            .and_then(|p| p.host_xid);
        let Some(gc) = self.gcs.get_mut(&request.gc.0) else {
            return;
        };
        Self::apply_gc_change(
            gc,
            GcChangeView {
                function: request.function,
                plane_mask: request.plane_mask,
                foreground: request.foreground,
                background: request.background,
                line_width: request.line_width,
                line_style: request.line_style,
                cap_style: request.cap_style,
                join_style: request.join_style,
                fill_style: request.fill_style,
                fill_rule: request.fill_rule,
                tile: request.tile,
                stipple: request.stipple,
                tile_x_origin: request.tile_x_origin,
                tile_y_origin: request.tile_y_origin,
                font: request.font,
                subwindow_mode: request.subwindow_mode,
                graphics_exposures: request.graphics_exposures,
                clip_x_origin: request.clip_x_origin,
                clip_y_origin: request.clip_y_origin,
                clip_mask: request.clip_mask,
                dash_offset: request.dash_offset,
                dashes: request.dashes,
                arc_mode: request.arc_mode,
            },
        );
        if request.clip_mask.is_some() {
            gc.clip_pixmap_host_xid = clip_pixmap_host_xid;
        }
        if request.tile.is_some() {
            gc.tile_host_xid = tile_host_xid;
        }
        if request.stipple.is_some() {
            gc.stipple_host_xid = stipple_host_xid;
        }
    }

    /// Apply the `Some`-valued attributes of a CreateGC / ChangeGC
    /// request onto an existing `Gc`. Shared between the two request
    /// paths so all 23 attribute slots are handled the same way.
    fn apply_gc_change(gc: &mut Gc, change: GcChangeView) {
        if let Some(function) = change.function {
            gc.function = GcFunction::from_protocol(function);
        }
        if let Some(plane_mask) = change.plane_mask {
            gc.plane_mask = plane_mask;
        }
        if let Some(foreground) = change.foreground {
            gc.foreground = foreground;
        }
        if let Some(background) = change.background {
            gc.background = background;
        }
        if let Some(line_width) = change.line_width {
            gc.line_width = line_width;
        }
        if let Some(line_style) = change.line_style {
            gc.line_style = LineStyle::from_protocol(line_style);
        }
        if let Some(cap_style) = change.cap_style {
            gc.cap_style = CapStyle::from_protocol(cap_style);
        }
        if let Some(join_style) = change.join_style {
            gc.join_style = JoinStyle::from_protocol(join_style);
        }
        if let Some(fs) = change.fill_style {
            gc.fill_style = FillStyle::from_protocol(fs);
        }
        if let Some(fill_rule) = change.fill_rule {
            gc.fill_rule = FillRule::from_protocol(fill_rule);
        }
        if let Some(tile) = change.tile {
            gc.tile = Some(tile);
        }
        if let Some(stipple) = change.stipple {
            gc.stipple = Some(stipple);
        }
        if let Some(x) = change.tile_x_origin {
            gc.tile_x_origin = x;
        }
        if let Some(y) = change.tile_y_origin {
            gc.tile_y_origin = y;
        }
        if let Some(font) = change.font {
            gc.font = Some(font);
        }
        if let Some(submode) = change.subwindow_mode {
            gc.subwindow_mode = SubwindowMode::from_protocol(submode);
        }
        if let Some(graphics_exposures) = change.graphics_exposures {
            gc.graphics_exposures = graphics_exposures;
        }
        if let Some(x) = change.clip_x_origin {
            gc.clip_x_origin = x;
        }
        if let Some(y) = change.clip_y_origin {
            gc.clip_y_origin = y;
        }
        // CPClipMask: Some(None) = clear, Some(Some(p)) = pixmap. Setting
        // a clip-mask supersedes any prior `SetClipRectangles` per spec.
        if let Some(mask) = change.clip_mask {
            gc.clip_rectangles = None;
            gc.clip_pixmap = mask;
        }
        if let Some(offset) = change.dash_offset {
            gc.dash_offset = offset as i16;
        }
        // CPDashList in CreateGC/ChangeGC is a single byte: store it as
        // the on/off pattern `[n, n]` and reset dash_offset per the X11
        // protocol semantics. The full SetDashes opcode (58) remains
        // unimplemented.
        if let Some(n) = change.dashes
            && n != 0
        {
            gc.dashes = vec![n, n];
            gc.dash_offset = 0;
        }
        if let Some(arc_mode) = change.arc_mode {
            gc.arc_mode = ArcMode::from_protocol(arc_mode);
        }
    }

    /// `SetDashes` (opcode 58). `dashes` is the multi-byte pattern in
    /// pixel units; spec § "If the list is of odd length, then it is
    /// effectively concatenated with itself to produce an even-length
    /// list", so callers normalize before storing — we do it here so
    /// every read site sees an even-length cycle.
    /// SetDashes — silent no-op if `gc_id` is unknown (caller MUST
    /// have BadGC-gated; Xorg's `ProcSetDashes` (`dix/dispatch.c:1626`)
    /// emits BadGC via `dixLookupGC` first).
    pub fn set_dashes(
        &mut self,
        _requester: ClientId,
        gc_id: ResourceId,
        dash_offset: u16,
        dashes: &[u8],
    ) {
        let Some(gc) = self.gcs.get_mut(&gc_id.0) else {
            return;
        };
        gc.dash_offset = dash_offset as i16;
        if dashes.is_empty() {
            return;
        }
        if dashes.len().is_multiple_of(2) {
            gc.dashes = dashes.to_vec();
        } else {
            let mut doubled = Vec::with_capacity(dashes.len() * 2);
            doubled.extend_from_slice(dashes);
            doubled.extend_from_slice(dashes);
            gc.dashes = doubled;
        }
    }

    /// SetClipRectangles — silent no-op if `request.gc` is unknown
    /// (caller MUST have BadGC-gated; Xorg's `ProcSetClipRectangles`
    /// (`dix/dispatch.c:1651`) emits BadGC via `dixLookupGC` first).
    pub fn set_clip_rectangles(&mut self, _requester: ClientId, request: SetClipRectanglesRequest) {
        let Some(gc) = self.gcs.get_mut(&request.gc.0) else {
            return;
        };
        // SetClipRectangles supersedes any prior clip-mask pixmap.
        gc.clip_pixmap = None;
        gc.clip_pixmap_host_xid = None;
        gc.clip_rectangles = Some(request.clip);
    }

    pub fn clear_gc_clip(&mut self, gc: ResourceId) {
        if let Some(g) = self.gcs.get_mut(&gc.0) {
            g.clip_rectangles = None;
            g.clip_pixmap = None;
            g.clip_pixmap_host_xid = None;
        }
    }

    pub fn copy_gc(&mut self, src: ResourceId, dst: ResourceId, value_mask: u32) {
        // Snapshot the source GC under the immutable borrow so we can
        // then take a mutable borrow of dst. Cheap because the only
        // owned field copied here is `dashes`.
        let Some(src_gc) = self.gcs.get(&src.0).cloned() else {
            return;
        };
        let Some(dst_gc) = self.gcs.get_mut(&dst.0) else {
            return;
        };
        if value_mask & 0x0000_0001 != 0 {
            dst_gc.function = src_gc.function;
        }
        if value_mask & 0x0000_0002 != 0 {
            dst_gc.plane_mask = src_gc.plane_mask;
        }
        if value_mask & 0x0000_0004 != 0 {
            dst_gc.foreground = src_gc.foreground;
        }
        if value_mask & 0x0000_0008 != 0 {
            dst_gc.background = src_gc.background;
        }
        if value_mask & 0x0000_0010 != 0 {
            dst_gc.line_width = src_gc.line_width;
        }
        if value_mask & 0x0000_0020 != 0 {
            dst_gc.line_style = src_gc.line_style;
        }
        if value_mask & 0x0000_0040 != 0 {
            dst_gc.cap_style = src_gc.cap_style;
        }
        if value_mask & 0x0000_0080 != 0 {
            dst_gc.join_style = src_gc.join_style;
        }
        if value_mask & 0x0000_0100 != 0 {
            dst_gc.fill_style = src_gc.fill_style;
        }
        if value_mask & 0x0000_0200 != 0 {
            dst_gc.fill_rule = src_gc.fill_rule;
        }
        if value_mask & 0x0000_0400 != 0 {
            dst_gc.tile = src_gc.tile;
            dst_gc.tile_host_xid = src_gc.tile_host_xid;
        }
        if value_mask & 0x0000_0800 != 0 {
            dst_gc.stipple = src_gc.stipple;
            dst_gc.stipple_host_xid = src_gc.stipple_host_xid;
        }
        if value_mask & 0x0000_1000 != 0 {
            dst_gc.tile_x_origin = src_gc.tile_x_origin;
        }
        if value_mask & 0x0000_2000 != 0 {
            dst_gc.tile_y_origin = src_gc.tile_y_origin;
        }
        if value_mask & 0x0000_4000 != 0 {
            dst_gc.font = src_gc.font;
        }
        if value_mask & 0x0000_8000 != 0 {
            dst_gc.subwindow_mode = src_gc.subwindow_mode;
        }
        if value_mask & 0x0001_0000 != 0 {
            dst_gc.graphics_exposures = src_gc.graphics_exposures;
        }
        if value_mask & 0x0002_0000 != 0 {
            dst_gc.clip_x_origin = src_gc.clip_x_origin;
        }
        if value_mask & 0x0004_0000 != 0 {
            dst_gc.clip_y_origin = src_gc.clip_y_origin;
        }
        if value_mask & 0x0008_0000 != 0 {
            // GCClipMask — copy both the rectangle-list and pixmap
            // clip-mask members. Either may be `None`; whichever the
            // source has set wins per X11 semantics (a clip-mask and a
            // rectangle-list cannot coexist).
            dst_gc.clip_rectangles = src_gc.clip_rectangles.clone();
            dst_gc.clip_pixmap = src_gc.clip_pixmap;
            dst_gc.clip_pixmap_host_xid = src_gc.clip_pixmap_host_xid;
        }
        if value_mask & 0x0010_0000 != 0 {
            dst_gc.dash_offset = src_gc.dash_offset;
        }
        if value_mask & 0x0020_0000 != 0 {
            dst_gc.dashes = src_gc.dashes.clone();
        }
        if value_mask & 0x0040_0000 != 0 {
            dst_gc.arc_mode = src_gc.arc_mode;
        }
    }

    pub fn free_gc(&mut self, id: ResourceId) {
        self.gcs.remove(&id.0);
    }

    pub fn gc(&self, id: ResourceId) -> Option<&Gc> {
        self.gcs.get(&id.0)
    }

    pub fn gc_foreground(&self, id: ResourceId) -> u32 {
        self.gc(id).map_or(0, |gc| gc.foreground)
    }

    pub fn gc_background(&self, id: ResourceId) -> u32 {
        self.gc(id).map_or(0x00ff_ffff, |gc| gc.background)
    }

    pub fn gc_clip_rectangles(&self, id: ResourceId) -> Option<ClipRectangles> {
        self.gc(id).and_then(|gc| gc.clip_rectangles.clone())
    }

    /// Return the GC's effective clip-state, resolved against the host:
    /// either a list of rectangles, a host-pixmap clip-mask (with
    /// origin), or `None` (no clipping). Returns `None` for an unknown
    /// GC, or when the GC names a `clip_pixmap` whose host-side backing
    /// is missing — both equivalent to "draw unclipped" rather than
    /// erroring out the request.
    pub fn gc_clip_state(&self, id: ResourceId) -> GcClipState {
        let Some(gc) = self.gc(id) else {
            return GcClipState::None;
        };
        if let Some(rects) = gc.clip_rectangles.clone() {
            return GcClipState::Rectangles(rects);
        }
        if let Some(host_pixmap) = gc.clip_pixmap_host_xid {
            return GcClipState::Pixmap {
                host_pixmap,
                clip_x_origin: gc.clip_x_origin,
                clip_y_origin: gc.clip_y_origin,
            };
        }
        GcClipState::None
    }

    /// Resolve the GC's effective fill state (Solid / Tiled / Stippled /
    /// OpaqueStippled) plus the host pixmap xid for the tile/stipple if any.
    /// Returns `None` for the default Solid case (no setup needed) or when
    /// the GC is unknown. e16 paints popup backgrounds via Tiled fill — the
    /// PolyFillRectangle / PolyFillArc / FillPoly handlers must call
    /// `apply_gc_fill_state` before forwarding the draw and reset to Solid
    /// after, otherwise the host's shared GC silently fills with the GC's
    /// foreground (typically 0 = black).
    pub fn gc_fill_state(&self, id: ResourceId) -> GcFillState {
        let Some(gc) = self.gc(id) else {
            return GcFillState::Solid;
        };
        match gc.fill_style {
            FillStyle::Tiled => {
                let host_pixmap = gc.tile_host_xid;
                match host_pixmap {
                    Some(host_pixmap) => GcFillState::Tiled {
                        host_pixmap,
                        tile_x_origin: gc.tile_x_origin,
                        tile_y_origin: gc.tile_y_origin,
                    },
                    // Tile pixmap missing or no host backing — fall back to
                    // Solid so the draw doesn't blow up; the host will fill
                    // with foreground colour, same as before this fix.
                    None => GcFillState::Solid,
                }
            }
            // Stippled / OpaqueStippled: not yet plumbed end-to-end on the
            // host shared GC; degrade to Solid so the draw doesn't blow up.
            // `resolve_draw_state` exposes the full FillState the host can
            // honour once the surface plumbing is wired up.
            _ => GcFillState::Solid,
        }
    }

    /// Resolve the GC's full `DrawState` snapshot for use by drawing
    /// call sites. Returns `None` only when the GC id is unknown — this
    /// is the BadGC case in the X11 protocol. Missing pixmap backing
    /// for a tile / stipple / clip-mask degrades the relevant component
    /// to its safe default (Solid fill, unclipped) rather than failing
    /// the whole request, mirroring `gc_fill_state` / `gc_clip_state`
    /// pre-Phase-6.2 behavior.
    pub fn resolve_draw_state(&self, gc_id: ResourceId) -> Option<DrawState> {
        let gc = self.gcs.get(&gc_id.0)?;

        // Clip resolution: rectangles take priority over pixmap, both
        // shifted by (clip_x_origin, clip_y_origin). Clip pixmaps use the
        // snapshotted host handle so the GC remains clipped after the
        // client frees the source pixmap.
        let clip = if let Some(rects) = gc.clip_rectangles.clone() {
            ClipState::Rectangles {
                origin: (gc.clip_x_origin, gc.clip_y_origin),
                rects,
            }
        } else if let Some(pixmap) = gc.clip_pixmap_host_xid {
            ClipState::Pixmap {
                origin: (gc.clip_x_origin, gc.clip_y_origin),
                pixmap,
            }
        } else {
            ClipState::None
        };

        // Fill resolution: degrade to Solid if the named tile/stipple
        // pixmap is missing host backing. The host's shared GC then
        // fills with foreground (existing pre-Phase-6.2 fallback).
        let fill = match gc.fill_style {
            FillStyle::Solid => FillState::Solid,
            FillStyle::Tiled => gc
                .tile_host_xid
                .map(|pixmap| FillState::Tiled {
                    pixmap,
                    origin: (gc.tile_x_origin, gc.tile_y_origin),
                })
                .unwrap_or(FillState::Solid),
            FillStyle::Stippled => gc
                .stipple_host_xid
                .map(|pixmap| FillState::Stippled {
                    pixmap,
                    origin: (gc.tile_x_origin, gc.tile_y_origin),
                })
                .unwrap_or(FillState::Solid),
            FillStyle::OpaqueStippled => gc
                .stipple_host_xid
                .map(|pixmap| FillState::OpaqueStippled {
                    pixmap,
                    origin: (gc.tile_x_origin, gc.tile_y_origin),
                })
                .unwrap_or(FillState::Solid),
        };

        let font: Option<FontHandle> = gc
            .font
            .and_then(|f| self.fonts.get(&f.0))
            .map(|f| f.host_xid);

        Some(DrawState {
            foreground: gc.foreground,
            background: gc.background,
            line_width: gc.line_width,
            line_style: gc.line_style,
            cap_style: gc.cap_style,
            join_style: gc.join_style,
            fill_style: gc.fill_style,
            fill_rule: gc.fill_rule,
            function: gc.function,
            plane_mask: gc.plane_mask,
            font,
            clip,
            fill,
            subwindow_mode: gc.subwindow_mode,
            graphics_exposures: gc.graphics_exposures,
            dashes: gc.dashes.clone(),
            dash_offset: gc.dash_offset,
            arc_mode: gc.arc_mode,
        })
    }

    pub fn create_picture(&mut self, id: ResourceId, state: PictureState) {
        self.pictures.insert(id.0, state);
    }

    pub fn free_picture(&mut self, id: ResourceId) -> Option<PictureState> {
        self.pictures.remove(&id.0)
    }

    pub fn picture(&self, id: ResourceId) -> Option<&PictureState> {
        self.pictures.get(&id.0)
    }

    pub fn create_glyphset(&mut self, id: ResourceId, state: GlyphSetState) {
        if let Some(old) = self.glyphsets.remove(&id.0) {
            let _ = self.release_host_glyphset_ref(old.host_glyphset_xid.as_raw());
        }
        *self
            .host_glyphset_refcounts
            .entry(state.host_glyphset_xid.as_raw())
            .or_insert(0) += 1;
        self.glyphsets.insert(id.0, state);
    }

    pub fn free_glyphset(&mut self, id: ResourceId) -> Option<GlyphSetState> {
        let state = self.glyphsets.remove(&id.0)?;
        if self.release_host_glyphset_ref(state.host_glyphset_xid.as_raw()) {
            Some(state)
        } else {
            None
        }
    }

    pub fn glyphset(&self, id: ResourceId) -> Option<&GlyphSetState> {
        self.glyphsets.get(&id.0)
    }

    pub fn reference_glyphset(
        &mut self,
        client: ClientId,
        new_id: ResourceId,
        existing_id: ResourceId,
    ) -> bool {
        let Some(existing) = self.glyphsets.get(&existing_id.0) else {
            return false;
        };
        self.create_glyphset(
            new_id,
            GlyphSetState {
                client,
                host_glyphset_xid: existing.host_glyphset_xid,
            },
        );
        true
    }

    fn release_host_glyphset_ref(&mut self, host_xid: u32) -> bool {
        let Some(count) = self.host_glyphset_refcounts.get_mut(&host_xid) else {
            return true;
        };
        if *count > 1 {
            *count -= 1;
            false
        } else {
            self.host_glyphset_refcounts.remove(&host_xid);
            true
        }
    }

    pub fn install_font(
        &mut self,
        owner: ClientId,
        id: ResourceId,
        name: String,
        host_xid: crate::backend::FontHandle,
        metrics: FontMetrics,
    ) {
        self.fonts.insert(
            id.0,
            Font {
                id,
                name,
                host_xid,
                metrics,
                owner,
            },
        );
    }

    pub fn close_font(&mut self, id: ResourceId) -> Option<Font> {
        self.fonts.remove(&id.0)
    }

    /// Commit a PolyText embedded font-change to the GC (X11 §8:
    /// "the font item ... is stored in the GC").
    pub fn set_gc_font(&mut self, gc_id: ResourceId, font: ResourceId) {
        if let Some(gc) = self.gcs.get_mut(&gc_id.0) {
            gc.font = Some(font);
        }
    }

    pub fn font(&self, id: ResourceId) -> Option<&Font> {
        self.fonts.get(&id.0)
    }

    /// Resolve a FONTABLE id (either a Font or a GC carrying a font) to a `&Font`.
    pub fn fontable(&self, id: ResourceId) -> Option<&Font> {
        if let Some(font) = self.fonts.get(&id.0) {
            return Some(font);
        }
        let gc_font = self.gcs.get(&id.0).and_then(|gc| gc.font)?;
        self.fonts.get(&gc_font.0)
    }

    pub fn create_glyph_cursor(&mut self, owner: ClientId, id: ResourceId) {
        self.cursors.insert(
            id.0,
            Cursor {
                id,
                owner,
                host_xid: None,
                name_atom: None,
                anim: false,
            },
        );
    }

    pub fn create_cursor(&mut self, owner: ClientId, id: ResourceId) {
        self.cursors.insert(
            id.0,
            Cursor {
                id,
                owner,
                host_xid: None,
                name_atom: None,
                anim: false,
            },
        );
    }

    pub fn set_cursor_host_xid(&mut self, id: ResourceId, handle: crate::backend::CursorHandle) {
        if let Some(c) = self.cursors.get_mut(&id.0) {
            c.host_xid = Some(handle);
        }
    }

    pub fn cursor_host_xid(&self, id: ResourceId) -> Option<u32> {
        self.cursors.get(&id.0)?.host_xid.map(|h| h.as_raw())
    }

    /// True iff `id` is a live cursor in the resource table — the
    /// `BadCursor` validation probe for both the `CWCursor` attribute
    /// path (xts5 Xlib4-25) and the grab-request handlers (Xorg
    /// `dixLookupResourceByType` with `X11_RESTYPE_CURSOR`).
    #[must_use]
    pub fn cursor_exists(&self, id: ResourceId) -> bool {
        self.cursors.contains_key(&id.0)
    }

    pub fn set_cursor_name_atom(&mut self, id: ResourceId, atom: yserver_protocol::x11::AtomId) {
        if let Some(c) = self.cursors.get_mut(&id.0) {
            c.name_atom = Some(atom);
        }
    }

    #[must_use]
    pub fn cursor_name_atom(&self, id: ResourceId) -> Option<yserver_protocol::x11::AtomId> {
        self.cursors.get(&id.0)?.name_atom
    }

    /// Remove a cursor from the table and return the host XID (if any)
    /// so the caller can free it on the host. Caller's responsibility to
    /// dispatch `backend.free_cursor` — keeping the resource layer
    /// backend-agnostic.
    pub fn free_cursor(&mut self, id: ResourceId) -> Option<u32> {
        let host_xid = self.cursors.get(&id.0).and_then(|c| c.host_xid);
        self.cursors.remove(&id.0);
        host_xid.map(|h| h.as_raw())
    }

    /// Mark a cursor as animated (RENDER CreateAnimCursor product).
    pub fn set_cursor_anim(&mut self, id: ResourceId) {
        if let Some(c) = self.cursors.get_mut(&id.0) {
            c.anim = true;
        }
    }

    /// True iff `id` is a live animated cursor. Unknown ids are false.
    #[must_use]
    pub fn cursor_is_anim(&self, id: ResourceId) -> bool {
        self.cursors.get(&id.0).is_some_and(|c| c.anim)
    }

    /// Windows whose `attributes.colormap` references `cmap`. Used by
    /// `InstallColormap` / `UninstallColormap` to enumerate
    /// `ColormapNotify` recipients per X11 spec ("a ColormapNotify
    /// event is generated on every window having this colormap as
    /// an attribute").
    pub fn windows_with_colormap(&self, cmap: ResourceId) -> Vec<ResourceId> {
        let mut out = Vec::new();
        for (raw_id, w) in &self.windows {
            if w.colormap == cmap {
                out.push(ResourceId(*raw_id));
            }
        }
        out
    }

    /// Top-level windows owned by `client`: windows whose parent is *not*
    /// owned by the same client. Reachable descendants (regardless of
    /// owner) get destroyed transitively when each root is destroyed.
    pub fn collect_owned_window_roots(&self, client: ClientId, out: &mut Vec<ResourceId>) {
        for (raw_id, w) in &self.windows {
            if w.owner != client {
                continue;
            }
            let parent_owner = self.windows.get(&w.parent.0).map(|p| p.owner);
            if parent_owner != Some(client) {
                out.push(ResourceId(*raw_id));
            }
        }
    }

    /// Remove every non-window resource owned by `client`. Returns the
    /// `host_xid` of every removed font and every removed host-backed pixmap so
    /// the caller can issue host-side `CloseFont` / `FreePixmap` after dropping
    /// the `ServerState` lock.
    pub fn remove_non_window_resources_owned_by(
        &mut self,
        client: ClientId,
    ) -> ClientRemovedResources {
        let mut freed_pixmaps = Vec::new();
        self.pixmaps.retain(|_, p| {
            if p.owner == client {
                if let Some(xid) = p.host_xid {
                    freed_pixmaps.push(xid.as_raw());
                }
                false
            } else {
                true
            }
        });
        self.gcs.retain(|_, g| g.owner != client);
        let mut freed_colormaps = Vec::new();
        self.colormaps.retain(|_, c| {
            if c.owner == client {
                if let Some(host) = c.host_colormap_xid {
                    freed_colormaps.push(host.as_raw());
                }
                false
            } else {
                true
            }
        });
        let mut freed_cursors = Vec::new();
        self.cursors.retain(|_, c| {
            if c.owner == client {
                if let Some(xid) = c.host_xid {
                    freed_cursors.push(xid.as_raw());
                }
                false
            } else {
                true
            }
        });
        let mut closed_fonts = Vec::new();
        self.fonts.retain(|_, f| {
            if f.owner == client {
                closed_fonts.push(f.host_xid.as_raw());
                false
            } else {
                true
            }
        });
        let mut freed_pictures: Vec<(u32, Option<u32>)> = Vec::new();
        self.pictures.retain(|_, p| {
            if p.client == client {
                freed_pictures.push((
                    p.host_picture_xid.as_raw(),
                    p.host_owned_pixmap.map(|h| h.as_raw()),
                ));
                false
            } else {
                true
            }
        });
        let mut freed_glyphsets = Vec::new();
        let removed_glyphsets = self
            .glyphsets
            .extract_if(|_, g| g.client == client)
            .map(|(_, g)| g.host_glyphset_xid.as_raw())
            .collect::<Vec<_>>();
        for host_xid in removed_glyphsets {
            if self.release_host_glyphset_ref(host_xid) {
                freed_glyphsets.push(host_xid);
            }
        }
        ClientRemovedResources {
            closed_fonts,
            freed_pixmaps,
            freed_pictures,
            freed_glyphsets,
            freed_cursors,
        }
    }

    /// Look up the current owner of `id` across every core resource
    /// table. Returns `None` if the ID doesn't name any known resource.
    /// Server-static resources (root window, overlay, default colormap /
    /// visuals) carry `SERVER_OWNER`.
    #[must_use]
    pub fn resource_owner(&self, id: ResourceId) -> Option<ClientId> {
        if let Some(w) = self.windows.get(&id.0) {
            return Some(w.owner);
        }
        if let Some(p) = self.pixmaps.get(&id.0) {
            return Some(p.owner);
        }
        if let Some(g) = self.gcs.get(&id.0) {
            return Some(g.owner);
        }
        if let Some(f) = self.fonts.get(&id.0) {
            return Some(f.owner);
        }
        if let Some(c) = self.cursors.get(&id.0) {
            return Some(c.owner);
        }
        if let Some(p) = self.pictures.get(&id.0) {
            return Some(p.client);
        }
        if let Some(g) = self.glyphsets.get(&id.0) {
            return Some(g.client);
        }
        None
    }

    #[must_use]
    pub fn any_resource_exists(&self, id: ResourceId) -> bool {
        self.windows.contains_key(&id.0)
            || self.pixmaps.contains_key(&id.0)
            || self.gcs.contains_key(&id.0)
            || self.fonts.contains_key(&id.0)
            || self.cursors.contains_key(&id.0)
    }

    /// Returns the screen-absolute (x, y) of the top-left corner of `id`.
    /// Walks up the parent chain accumulating x/y offsets. Returns (0, 0) for ROOT_WINDOW.
    #[must_use]
    pub fn window_absolute_position(&self, id: ResourceId) -> (i32, i32) {
        if id == ROOT_WINDOW {
            return (0, 0);
        }
        let mut ax: i32 = 0;
        let mut ay: i32 = 0;
        let mut current = id;
        let mut depth = 0usize;
        while current != ROOT_WINDOW && depth < 256 {
            let Some(w) = self.windows.get(&current.0) else {
                break;
            };
            ax += i32::from(w.x);
            ay += i32::from(w.y);
            if w.parent == current {
                break;
            }
            current = w.parent;
            depth += 1;
        }
        (ax, ay)
    }

    /// Returns the child of `parent` (in the nested window tree) that contains
    /// the screen-absolute point (abs_x, abs_y), or None if no mapped child contains it.
    /// Children are checked in reverse order (top of stacking = last in list).
    #[must_use]
    pub fn child_containing_point(
        &self,
        parent: ResourceId,
        abs_x: i32,
        abs_y: i32,
    ) -> Option<ResourceId> {
        let (px, py) = self.window_absolute_position(parent);
        let local_x = abs_x - px;
        let local_y = abs_y - py;
        let w = self.windows.get(&parent.0)?;
        for &child_id in w.children.iter().rev() {
            let Some(child) = self.windows.get(&child_id.0) else {
                continue;
            };
            if child.map_state == MapState::Unmapped {
                continue;
            }
            let cx = i32::from(child.x);
            let cy = i32::from(child.y);
            let cw = i32::from(child.width);
            let ch = i32::from(child.height);
            if local_x >= cx && local_x < cx + cw && local_y >= cy && local_y < cy + ch {
                return Some(child_id);
            }
        }
        None
    }
}

/// Effective clip-state of a GC at the moment a draw op runs. Either
/// the GC has a `SetClipRectangles` list, a `ChangeGC(clip_mask=Pixmap)`
/// host-mask, or no clip at all.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GcClipState {
    None,
    Rectangles(ClipRectangles),
    Pixmap {
        host_pixmap: crate::backend::PixmapHandle,
        clip_x_origin: i16,
        clip_y_origin: i16,
    },
}

/// Effective fill-style of a GC. Solid = use foreground; Tiled = tile a
/// pixmap onto the destination. Stippled / OpaqueStippled would belong
/// here too but no observed client uses them in the popup/menu paths
/// currently exercised — they fall through to Solid for now.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GcFillState {
    Solid,
    Tiled {
        host_pixmap: crate::backend::PixmapHandle,
        tile_x_origin: i16,
        tile_y_origin: i16,
    },
}

/// One client-issued `Composite::NameWindowPixmap` alias on a window. The
/// COMPOSITE spec allows a client to call `NameWindowPixmap` repeatedly;
/// each call returns a distinct `Pixmap` resource pointing at the
/// window's redirected backing store. All aliases on a window are
/// invalidated together by a resize and freed on destroy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NamedCompositePixmap {
    pub client_pixmap: ResourceId,
    pub host_pixmap: crate::backend::PixmapHandle,
    pub width: u16,
    pub height: u16,
}

/// The concrete restack a `ConfigureWindow` request resolves to after
/// applying X11's stack-mode + occlusion semantics. Built by
/// [`ResourceTable::resolve_restack_action`] from the immutable child
/// list, then applied by [`ResourceTable::restack_window`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RestackAction {
    NoOp,
    Top,
    Bottom,
    AboveSibling(ResourceId),
    BelowSibling(ResourceId),
}

/// True iff the bounding rectangles of `a` and `b` (including border)
/// have a non-empty intersection. Used by occlusion checks for
/// `TopIf` / `BottomIf` / `Opposite` stack modes.
fn window_rects_overlap(a: &Window, b: &Window) -> bool {
    let (ax0, ay0, ax1, ay1) = window_bounding_box(a);
    let (bx0, by0, bx1, by1) = window_bounding_box(b);
    ax0 < bx1 && bx0 < ax1 && ay0 < by1 && by0 < ay1
}

/// X11 bounding box of a window: the outer rectangle including border.
/// Returns (left, top, right, bottom) in parent coords.
fn window_bounding_box(w: &Window) -> (i32, i32, i32, i32) {
    let bw = i32::from(w.border_width);
    let x0 = i32::from(w.x);
    let y0 = i32::from(w.y);
    let x1 = x0 + i32::from(w.width) + 2 * bw;
    let y1 = y0 + i32::from(w.height) + 2 * bw;
    (x0, y0, x1, y1)
}

#[derive(Clone, Debug)]
pub struct Window {
    pub id: ResourceId,
    pub parent: ResourceId,
    pub children: Vec<ResourceId>,
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    pub border_width: u16,
    pub depth: u8,
    pub visual: ResourceId,
    pub class: WindowClass,
    pub map_state: MapState,
    pub background_pixel: u32,
    pub background_pixmap: Option<ResourceId>,
    /// True when the effective background is None (X11 CreateWindow
    /// DEFAULT, or explicit CWBackPixmap=0 without a pixel):
    /// ClearArea/expose painting leaves contents untouched.
    pub background_none: bool,
    /// Host XID of the bg pixmap, snapshotted at attrs-change time so
    /// it survives FreePixmap (X11 servers retain bg pixmaps independent
    /// of client refs).
    pub background_pixmap_host_xid: Option<crate::backend::PixmapHandle>,
    /// Host XID of the border pixmap (if any). Parallel to
    /// `background_pixmap_host_xid`, retained for the same reason.
    pub border_pixmap_host_xid: Option<crate::backend::PixmapHandle>,
    pub override_redirect: bool,
    pub bit_gravity: u8,
    pub win_gravity: u8,
    pub backing_store: u8,
    pub backing_planes: u32,
    pub backing_pixel: u32,
    pub save_under: bool,
    pub do_not_propagate_mask: u16,
    pub colormap: ResourceId,
    pub cursor: Option<ResourceId>,
    pub owner: ClientId,
    pub properties: HashMap<AtomId, PropertyValue>,
    pub host_xid: Option<crate::backend::WindowHandle>,
    /// Per-window list of `Composite::NameWindowPixmap` aliases. All are
    /// invalidated together on resize per the COMPOSITE spec.
    pub composite_named_pixmaps: Vec<NamedCompositePixmap>,
    /// Off-screen backing image for redirected windows (L2 plan
    /// B.2). `None` for unredirected windows; populated by
    /// `RedirectWindow` / `RedirectSubwindows` activation (B.6a)
    /// and cleared by Unredirect (B.6c) or DestroyWindow (B.15).
    /// On resize under redirect the backing is *rotated* to a
    /// new image (B.6d) so existing `NameWindowPixmap` aliases
    /// stay valid against the pre-resize content.
    pub redirected_backing: Option<RedirectedBacking>,
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

impl Window {
    fn placeholder(id: ResourceId) -> Self {
        Self {
            id,
            parent: ROOT_WINDOW,
            children: Vec::new(),
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            border_width: 0,
            depth: 24,
            visual: ROOT_VISUAL,
            class: WindowClass::InputOutput,
            map_state: MapState::Unmapped,
            background_pixel: 0x00ff_ffff,
            background_pixmap: None,
            background_none: false,
            background_pixmap_host_xid: None,
            border_pixmap_host_xid: None,
            override_redirect: false,
            bit_gravity: 0,
            win_gravity: 1,
            backing_store: 0,
            backing_planes: u32::MAX,
            backing_pixel: 0,
            save_under: false,
            do_not_propagate_mask: 0,
            colormap: ROOT_COLORMAP,
            cursor: None,
            owner: SERVER_OWNER,
            properties: HashMap::new(),
            host_xid: None,
            composite_named_pixmaps: Vec::new(),
            redirected_backing: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowClass {
    CopyFromParent,
    InputOutput,
    InputOnly,
    Other(u16),
}

impl WindowClass {
    fn from_protocol(value: u16) -> Self {
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

#[derive(Clone, Debug)]
pub struct Pixmap {
    pub id: ResourceId,
    pub drawable: ResourceId,
    pub width: u16,
    pub height: u16,
    pub depth: u8,
    pub owner: ClientId,
    pub host_xid: Option<crate::backend::PixmapHandle>,
}

#[derive(Clone, Debug)]
pub struct Gc {
    pub id: ResourceId,
    pub drawable: ResourceId,
    pub depth: u8,
    pub foreground: u32,
    pub background: u32,
    pub line_width: u16,
    pub font: Option<ResourceId>,
    pub clip_rectangles: Option<ClipRectangles>,
    /// Pixmap-based clip-mask, set via `ChangeGC` with `CPClipMask`. When
    /// `Some`, draws through the GC are clipped to the 1-bits of the
    /// referenced depth-1 pixmap shifted by `(clip_x_origin,
    /// clip_y_origin)`. wmaker uses this for window-decoration symbols
    /// (close-button "X", miniaturize dot).
    pub clip_pixmap: Option<ResourceId>,
    /// Host handle snapshotted when the clip-mask is installed into the
    /// GC. GCs retain clip-mask semantics after the client frees the
    /// source pixmap, so resolution must not depend solely on the live
    /// pixmap resource table entry.
    pub clip_pixmap_host_xid: Option<crate::backend::PixmapHandle>,
    pub clip_x_origin: i16,
    pub clip_y_origin: i16,
    /// X11 GC `fill-style`. e16 paints popup backgrounds via Tiled fill,
    /// so PolyFillRectangle on the destination pixmap tiles the theme
    /// pixmap onto it. Without honoring this, the destination stays the
    /// default solid foreground (typically 0 = black).
    pub fill_style: FillStyle,
    pub tile: Option<ResourceId>,
    pub stipple: Option<ResourceId>,
    pub tile_host_xid: Option<crate::backend::PixmapHandle>,
    pub stipple_host_xid: Option<crate::backend::PixmapHandle>,
    pub tile_x_origin: i16,
    pub tile_y_origin: i16,
    // Phase 6.2 additive scope: stored per-GC so they can be forwarded
    // to the host's shared GC at draw time. Pre-Phase-6.2 ynest silently
    // ignored these and drew with host-GC defaults; honoring them is a
    // behavioral improvement (e.g. drag-rectangle Xor now works).
    pub line_style: LineStyle,
    pub cap_style: CapStyle,
    pub join_style: JoinStyle,
    pub fill_rule: FillRule,
    pub function: GcFunction,
    pub plane_mask: u32,
    pub subwindow_mode: SubwindowMode,
    pub graphics_exposures: bool,
    pub dashes: Vec<u8>,
    pub dash_offset: i16,
    pub arc_mode: ArcMode,
    pub owner: ClientId,
}

/// Internal projection of CreateGC / ChangeGC's value-list onto a
/// single struct. Both request paths build one of these and feed it to
/// `apply_gc_change`, so all 23 attribute slots are handled identically.
#[derive(Clone, Copy, Debug, Default)]
struct GcChangeView {
    function: Option<u8>,
    plane_mask: Option<u32>,
    foreground: Option<u32>,
    background: Option<u32>,
    line_width: Option<u16>,
    line_style: Option<u8>,
    cap_style: Option<u8>,
    join_style: Option<u8>,
    fill_style: Option<u8>,
    fill_rule: Option<u8>,
    tile: Option<ResourceId>,
    stipple: Option<ResourceId>,
    tile_x_origin: Option<i16>,
    tile_y_origin: Option<i16>,
    font: Option<ResourceId>,
    subwindow_mode: Option<u8>,
    graphics_exposures: Option<bool>,
    clip_x_origin: Option<i16>,
    clip_y_origin: Option<i16>,
    clip_mask: Option<Option<ResourceId>>,
    dash_offset: Option<u16>,
    dashes: Option<u8>,
    arc_mode: Option<u8>,
}

impl Gc {
    /// Construct a GC with all-default attributes for the given id /
    /// drawable / owner. Used by the `change_gc` and
    /// `set_clip_rectangles` paths when the GC has not been seen before.
    fn with_defaults(id: ResourceId, drawable: ResourceId, owner: ClientId) -> Self {
        Self {
            id,
            drawable,
            depth: 0,
            foreground: 0,
            background: 0x00ff_ffff,
            line_width: 0,
            font: None,
            clip_rectangles: None,
            clip_pixmap: None,
            clip_pixmap_host_xid: None,
            clip_x_origin: 0,
            clip_y_origin: 0,
            fill_style: FillStyle::Solid,
            tile: None,
            stipple: None,
            tile_host_xid: None,
            stipple_host_xid: None,
            tile_x_origin: 0,
            tile_y_origin: 0,
            line_style: LineStyle::Solid,
            cap_style: CapStyle::Butt,
            join_style: JoinStyle::Miter,
            fill_rule: FillRule::EvenOdd,
            function: GcFunction::Copy,
            plane_mask: u32::MAX,
            subwindow_mode: SubwindowMode::ClipByChildren,
            graphics_exposures: true,
            dashes: vec![4, 4],
            dash_offset: 0,
            arc_mode: ArcMode::PieSlice,
            owner,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Font {
    pub id: ResourceId,
    pub name: String,
    pub host_xid: crate::backend::FontHandle,
    pub metrics: FontMetrics,
    pub owner: ClientId,
}

#[derive(Clone, Debug)]
pub struct Cursor {
    pub id: ResourceId,
    pub owner: ClientId,
    pub host_xid: Option<crate::backend::CursorHandle>,
    /// XFixes `SetCursorName` interns the client-supplied name as an
    /// atom and stores it on the cursor; `GetCursorName` reads it back.
    /// `None` (until first `SetCursorName`) reports as X11 `None` atom
    /// (xid 0) on get, matching Xorg `xfixes/cursor.c`'s
    /// `pCursor->name == 0` initial state.
    pub name_atom: Option<yserver_protocol::x11::AtomId>,
    /// True iff this cursor was created by RENDER `CreateAnimCursor`.
    /// Consulted to reject nested animated cursors with `BadMatch`
    /// (Xorg `render/animcur.c:316` refuses them). Set on EVERY
    /// successful CreateAnimCursor — including backends that
    /// degenerate to the static first frame.
    pub anim: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use yserver_protocol::x11::{ChangeWindowAttributesRequest, ClientId, CreateWindowRequest};

    fn make_window(table: &mut ResourceTable, id: u32) {
        table.create_window(
            ClientId(1),
            CreateWindowRequest {
                depth: 24,
                window: ResourceId(id),
                parent: ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 100,
                height: 100,
                border_width: 0,
                class: 1,
                visual: ROOT_VISUAL,
                ..Default::default()
            },
        );
    }

    fn make_top_level_with_host_xid(table: &mut ResourceTable, id: u32, host_xid: u32) {
        make_window(table, id);
        table.windows.get_mut(&id).unwrap().host_xid =
            Some(crate::backend::WindowHandle::from_raw_for_test(host_xid));
    }

    fn make_child(table: &mut ResourceTable, id: u32, parent: u32, x: i16, y: i16) {
        table.create_window(
            ClientId(1),
            CreateWindowRequest {
                depth: 24,
                window: ResourceId(id),
                parent: ResourceId(parent),
                x,
                y,
                width: 50,
                height: 50,
                border_width: 0,
                class: 1,
                visual: ROOT_VISUAL,
                ..Default::default()
            },
        );
    }

    #[derive(Debug, Clone, Copy)]
    enum InitialState {
        Viewable,
        Unviewable,
        Unmapped,
    }

    fn arb_initial() -> impl Strategy<Value = InitialState> {
        prop_oneof![
            Just(InitialState::Viewable),
            Just(InitialState::Unviewable),
            Just(InitialState::Unmapped),
        ]
    }

    /// Regression for the "leaked XID → BadIDChoice after recycling"
    /// family. `xid_in_use` walks every core resource map; for the
    /// IdAllocator recycle path to be sound, EVERY map it checks must
    /// be drained by `remove_non_window_resources_owned_by` (windows
    /// go through `destroy_window` separately).
    ///
    /// Populates one entry per map for `ClientId(K)`, runs the
    /// disconnect cleanup, and asserts each test XID reads
    /// `xid_in_use == false`. Adding a new map to `xid_in_use` should
    /// be accompanied by a new line in this test + cleanup logic in
    /// `remove_non_window_resources_owned_by`; without that the test
    /// stays passing for the existing maps but a new leak ships silent.
    #[test]
    fn disconnect_cleanup_drains_every_xid_in_use_map_for_owner() {
        use crate::backend::{
            ColormapHandle, CursorHandle, FontHandle, GlyphSetHandle, PictureHandle, PixmapHandle,
        };

        let mut table = ResourceTable::new();
        let owner = ClientId(7);
        let other = ClientId(8);

        // Windows handled separately via destroy_window; this test
        // focuses on the non-window family that `remove_non_window_
        // resources_owned_by` is responsible for.

        // Pixmap
        let pix_id = ResourceId(0x0700_0001);
        table.pixmaps.insert(
            pix_id.0,
            Pixmap {
                id: pix_id,
                drawable: ROOT_WINDOW,
                width: 1,
                height: 1,
                depth: 24,
                owner,
                host_xid: Some(PixmapHandle::from_raw_for_test(0xa01)),
            },
        );

        // GC
        let gc_id = ResourceId(0x0700_0002);
        table
            .gcs
            .insert(gc_id.0, Gc::with_defaults(gc_id, ROOT_WINDOW, owner));

        // Font
        let font_id = ResourceId(0x0700_0003);
        table.fonts.insert(
            font_id.0,
            Font {
                id: font_id,
                name: "fixed".to_string(),
                host_xid: FontHandle::from_raw_for_test(0xa03),
                metrics: FontMetrics::default(),
                owner,
            },
        );

        // Cursor
        let cur_id = ResourceId(0x0700_0004);
        table.cursors.insert(
            cur_id.0,
            Cursor {
                id: cur_id,
                owner,
                host_xid: Some(CursorHandle::from_raw_for_test(0xa04)),
                name_atom: None,
                anim: false,
            },
        );

        // Colormap
        let cmap_id = ResourceId(0x0700_0005);
        table.colormaps.insert(
            cmap_id.0,
            Colormap {
                id: cmap_id,
                visual: ROOT_VISUAL,
                host_colormap_xid: Some(ColormapHandle::from_raw_for_test(0xa05)),
                owner,
            },
        );

        // Picture
        let pic_id = ResourceId(0x0700_0006);
        table.pictures.insert(
            pic_id.0,
            PictureState {
                client: owner,
                host_picture_xid: PictureHandle::from_raw_for_test(0xa06),
                host_owned_pixmap: None,
                kind: PictureKind::Sourceless,
                drawable: None,
            },
        );

        // GlyphSet
        let gs_id = ResourceId(0x0700_0007);
        table.glyphsets.insert(
            gs_id.0,
            GlyphSetState {
                client: owner,
                host_glyphset_xid: GlyphSetHandle::from_raw_for_test(0xa07),
            },
        );

        // Sanity: everything is currently in use, and a resource
        // owned by a DIFFERENT client (not the disconnecting one)
        // should survive cleanup.
        let other_gc = ResourceId(0x0800_0001);
        table
            .gcs
            .insert(other_gc.0, Gc::with_defaults(other_gc, ROOT_WINDOW, other));

        for id in [
            pix_id, gc_id, font_id, cur_id, cmap_id, pic_id, gs_id, other_gc,
        ] {
            assert!(
                table.xid_in_use(id),
                "pre-cleanup: xid 0x{:x} should be in use",
                id.0,
            );
        }

        // Run the disconnect cleanup for `owner`.
        let _ = table.remove_non_window_resources_owned_by(owner);

        // Every map drained for `owner`; `other`'s GC survived.
        for id in [pix_id, gc_id, font_id, cur_id, cmap_id, pic_id, gs_id] {
            assert!(
                !table.xid_in_use(id),
                "post-cleanup: xid 0x{:x} still in use — \
                 a resource map is missing from remove_non_window_resources_owned_by",
                id.0,
            );
        }
        assert!(
            table.xid_in_use(other_gc),
            "other client's GC must survive cleanup of `owner`",
        );
    }

    #[test]
    fn fresh_window_has_no_redirected_backing() {
        let placeholder = Window::placeholder(ResourceId(0x100_0042));
        assert!(placeholder.redirected_backing.is_none());
    }

    #[test]
    fn resolved_background_follows_parent_relative_links() {
        let mut table = ResourceTable::new();
        make_window(&mut table, 0x200001);
        make_child(&mut table, 0x200002, 0x200001, 0, 0);

        {
            let parent = table.windows.get_mut(&0x200001).unwrap();
            parent.background_pixel = 0x00aa_bbcc;
            parent.background_pixmap = None;
            parent.background_pixmap_host_xid = None;
            // Direct struct poke must mirror what a real CWA
            // (background-pixel set) does to the None flag.
            parent.background_none = false;
        }
        {
            let child = table.windows.get_mut(&0x200002).unwrap();
            child.background_pixmap = Some(ResourceId(1));
            child.background_pixmap_host_xid = None;
        }

        let resolved = table
            .window_resolved_background(ResourceId(0x200002))
            .expect("resolved background");
        assert_eq!(resolved.background_pixel, 0x00aa_bbcc);
        assert_eq!(resolved.background_pixmap_host_xid, None);

        let parent_host = crate::backend::PixmapHandle::from_raw_for_test(0xdead_beef);
        {
            let parent = table.windows.get_mut(&0x200001).unwrap();
            parent.background_pixmap = Some(ResourceId(0x200010));
            parent.background_pixmap_host_xid = Some(parent_host);
        }
        let resolved = table
            .window_resolved_background(ResourceId(0x200002))
            .expect("resolved background");
        assert_eq!(resolved.background_pixel, 0x00aa_bbcc);
        assert_eq!(resolved.background_pixmap_host_xid, Some(parent_host));
    }

    #[test]
    fn reference_glyphset_alias_frees_host_only_after_last_alias() {
        let mut table = ResourceTable::new();
        table.create_glyphset(
            ResourceId(0x200),
            GlyphSetState {
                client: ClientId(1),
                host_glyphset_xid: crate::backend::GlyphSetHandle::from_raw_for_test(0xabc),
            },
        );

        assert!(table.reference_glyphset(ClientId(1), ResourceId(0x201), ResourceId(0x200)));
        assert_eq!(
            table
                .glyphset(ResourceId(0x201))
                .map(|g| g.host_glyphset_xid.as_raw()),
            Some(0xabc)
        );

        assert!(table.free_glyphset(ResourceId(0x200)).is_none());
        assert_eq!(
            table
                .free_glyphset(ResourceId(0x201))
                .map(|g| g.host_glyphset_xid.as_raw()),
            Some(0xabc)
        );
    }

    #[test]
    fn cw_fields_persist_through_create_and_change() {
        // Bucket 3: validate that the new Window struct fields round-trip
        // through CreateWindow + ChangeWindowAttributes + GetWindowAttributes.
        let mut table = ResourceTable::new();
        let win = ResourceId(0xa00);
        table.create_window(
            ClientId(1),
            CreateWindowRequest {
                depth: 24,
                window: win,
                parent: ROOT_WINDOW,
                width: 100,
                height: 100,
                class: 1,
                visual: ROOT_VISUAL,
                bit_gravity: Some(7),
                win_gravity: Some(8),
                backing_store: Some(2),
                backing_planes: Some(0xdead_beef),
                backing_pixel: Some(0x1234_5678),
                save_under: Some(true),
                do_not_propagate_mask: Some(0x0040), // PointerMotion
                ..Default::default()
            },
        );
        let w = table.window(win).expect("window");
        assert_eq!(w.bit_gravity, 7);
        assert_eq!(w.win_gravity, 8);
        assert_eq!(w.backing_store, 2);
        assert_eq!(w.backing_planes, 0xdead_beef);
        assert_eq!(w.backing_pixel, 0x1234_5678);
        assert!(w.save_under);
        assert_eq!(w.do_not_propagate_mask, 0x0040);
        // Default colormap inherits from parent (root → ROOT_COLORMAP).
        assert_eq!(w.colormap, ROOT_COLORMAP);

        // Mutate via ChangeWindowAttributes.
        let _ = table.change_window_attributes(ChangeWindowAttributesRequest {
            window: win,
            bit_gravity: Some(3),
            backing_store: Some(1),
            save_under: Some(false),
            do_not_propagate_mask: Some(0x0004), // ButtonPress
            ..Default::default()
        });
        let w = table.window(win).expect("window");
        assert_eq!(w.bit_gravity, 3);
        assert_eq!(w.win_gravity, 8); // untouched
        assert_eq!(w.backing_store, 1);
        assert!(!w.save_under);
        assert_eq!(w.do_not_propagate_mask, 0x0004);
    }

    #[test]
    fn remove_client_frees_shared_glyphset_once() {
        let mut table = ResourceTable::new();
        table.create_glyphset(
            ResourceId(0x200),
            GlyphSetState {
                client: ClientId(1),
                host_glyphset_xid: crate::backend::GlyphSetHandle::from_raw_for_test(0xabc),
            },
        );
        assert!(table.reference_glyphset(ClientId(1), ResourceId(0x201), ResourceId(0x200)));

        let removed = table.remove_non_window_resources_owned_by(ClientId(1));

        assert_eq!(removed.freed_glyphsets, vec![0xabc]);
        assert!(table.glyphset(ResourceId(0x200)).is_none());
        assert!(table.glyphset(ResourceId(0x201)).is_none());
    }

    #[test]
    fn unmap_window_returns_true_on_transition_from_viewable() {
        let mut table = ResourceTable::new();
        make_window(&mut table, 0x100002);
        let _ = table.map_window(ResourceId(0x100002));
        assert_eq!(
            table.window(ResourceId(0x100002)).unwrap().map_state,
            MapState::Viewable
        );
        let was_mapped = table.unmap_window(ResourceId(0x100002));
        assert!(was_mapped);
        assert_eq!(
            table.window(ResourceId(0x100002)).unwrap().map_state,
            MapState::Unmapped
        );
    }

    #[test]
    fn unmap_window_returns_true_on_transition_from_unviewable() {
        let mut table = ResourceTable::new();
        make_window(&mut table, 0x100002);
        // Force Unviewable directly — no public setter, but the field is pub.
        table.windows.get_mut(&0x100002).unwrap().map_state = MapState::Unviewable;
        let was_mapped = table.unmap_window(ResourceId(0x100002));
        assert!(was_mapped);
        assert_eq!(
            table.window(ResourceId(0x100002)).unwrap().map_state,
            MapState::Unmapped
        );
    }

    #[test]
    fn unmap_window_returns_false_when_already_unmapped() {
        let mut table = ResourceTable::new();
        make_window(&mut table, 0x100002);
        // create_window leaves new windows Unmapped.
        assert_eq!(
            table.window(ResourceId(0x100002)).unwrap().map_state,
            MapState::Unmapped
        );
        let first = table.unmap_window(ResourceId(0x100002));
        assert!(!first);
        let second = table.unmap_window(ResourceId(0x100002));
        assert!(!second);
    }

    #[test]
    fn unmap_window_returns_false_for_unknown_window() {
        let mut table = ResourceTable::new();
        let was_mapped = table.unmap_window(ResourceId(0x9999_9999));
        assert!(!was_mapped);
    }

    #[test]
    fn unmap_window_no_ops_on_root() {
        let mut table = ResourceTable::new();
        assert_eq!(
            table.window(ROOT_WINDOW).unwrap().map_state,
            MapState::Viewable
        );
        let was_mapped = table.unmap_window(ROOT_WINDOW);
        assert!(!was_mapped);
        assert_eq!(
            table.window(ROOT_WINDOW).unwrap().map_state,
            MapState::Viewable
        );
    }

    #[test]
    fn host_drawable_target_redirected_window_returns_backing_xid() {
        // L2 plan B.7: a window with `redirected_backing` set routes
        // paint to the backing's host XID, carrying the backing's
        // depth. The `nested` ResourceId stays the window itself so
        // damage and event delivery keep the public XID.
        let mut table = ResourceTable::new();
        make_top_level_with_host_xid(&mut table, 0x0010_0002, 0x42);
        let backing = crate::backend::PixmapHandle::from_raw_for_test(0x9999);
        table
            .windows
            .get_mut(&0x0010_0002)
            .unwrap()
            .redirected_backing = Some(crate::resources::RedirectedBacking {
            host_pixmap: backing,
            width: 100,
            height: 50,
            depth: 24,
        });
        match table.host_drawable_target(ResourceId(0x0010_0002)).unwrap() {
            HostDrawableTarget::Window {
                nested,
                host_xid,
                depth,
            } => {
                assert_eq!(nested, ResourceId(0x0010_0002));
                assert_eq!(host_xid.as_raw(), 0x9999);
                assert_eq!(depth, 24);
            }
            _ => panic!("expected Window variant routing to backing"),
        }
    }

    #[test]
    fn host_drawable_target_top_level_window_with_host_xid() {
        let mut table = ResourceTable::new();
        make_top_level_with_host_xid(&mut table, 0x0010_0002, 0xAA);
        let target = table.host_drawable_target(ResourceId(0x0010_0002));
        assert_eq!(
            target,
            Some(HostDrawableTarget::Window {
                nested: ResourceId(0x0010_0002),
                host_xid: crate::backend::WindowHandle::from_raw_for_test(0xAA),
                depth: 24,
            })
        );
    }

    #[test]
    fn host_drawable_target_child_window_targets_own_host_xid() {
        // Phase 3.6 Step 6: every InputOutput window has its own host_xid;
        // a child without host_xid set yields None (drop) rather than
        // walking up to the parent.
        let mut table = ResourceTable::new();
        make_top_level_with_host_xid(&mut table, 0x0010_0002, 0xBB);
        make_child(&mut table, 0x0010_0003, 0x0010_0002, 10, 20);
        if let Some(child) = table.window_mut(ResourceId(0x0010_0003)) {
            child.host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(0xCC));
        }
        let target = table.host_drawable_target(ResourceId(0x0010_0003));
        assert_eq!(
            target,
            Some(HostDrawableTarget::Window {
                nested: ResourceId(0x0010_0003),
                host_xid: crate::backend::WindowHandle::from_raw_for_test(0xCC),
                depth: 24,
            })
        );
    }

    #[test]
    fn host_drawable_target_window_without_host_xid_returns_none() {
        let mut table = ResourceTable::new();
        make_top_level_with_host_xid(&mut table, 0x0010_0002, 0xBB);
        make_child(&mut table, 0x0010_0003, 0x0010_0002, 10, 20);
        // child has no host_xid; drawing on it drops silently.
        assert_eq!(table.host_drawable_target(ResourceId(0x0010_0003)), None);
    }

    #[test]
    fn host_drawable_target_pixmap_with_host_xid() {
        let mut table = ResourceTable::new();
        let request = CreatePixmapRequest {
            pixmap: ResourceId(0x0020_0002),
            drawable: ROOT_WINDOW,
            width: 256,
            height: 256,
            depth: 32,
        };
        table.create_pixmap(ClientId(1), request);
        assert!(table.set_pixmap_host_xid(
            ResourceId(0x0020_0002),
            crate::backend::PixmapHandle::from_raw_for_test(0xDEAD_BEEF),
        ));
        let target = table.host_drawable_target(ResourceId(0x0020_0002));
        assert_eq!(
            target,
            Some(HostDrawableTarget::Pixmap {
                nested: ResourceId(0x0020_0002),
                host_xid: crate::backend::PixmapHandle::from_raw_for_test(0xDEAD_BEEF),
                width: 256,
                height: 256,
                depth: 32,
            })
        );
    }

    #[test]
    fn host_drawable_target_pixmap_without_host_xid_returns_none() {
        let mut table = ResourceTable::new();
        let request = CreatePixmapRequest {
            pixmap: ResourceId(0x0020_0002),
            drawable: ROOT_WINDOW,
            width: 128,
            height: 128,
            depth: 24,
        };
        table.create_pixmap(ClientId(1), request);
        // host_xid is None by default
        let target = table.host_drawable_target(ResourceId(0x0020_0002));
        assert_eq!(target, None);
    }

    #[test]
    fn host_drawable_target_unknown_drawable_returns_none() {
        let table = ResourceTable::new();
        let target = table.host_drawable_target(ResourceId(0x9999_9999));
        assert_eq!(target, None);
    }

    #[test]
    fn set_pixmap_host_xid_unknown_id_returns_false() {
        let mut table = ResourceTable::new();
        let result = table.set_pixmap_host_xid(
            ResourceId(0xDEAD),
            crate::backend::PixmapHandle::from_raw_for_test(0x1234),
        );
        assert!(!result);
    }

    #[test]
    fn set_pixmap_host_xid_sets_value_and_returns_true() {
        let mut table = ResourceTable::new();
        let request = CreatePixmapRequest {
            pixmap: ResourceId(0x0020_0002),
            drawable: ROOT_WINDOW,
            width: 128,
            height: 128,
            depth: 24,
        };
        table.create_pixmap(ClientId(1), request);
        let result = table.set_pixmap_host_xid(
            ResourceId(0x0020_0002),
            crate::backend::PixmapHandle::from_raw_for_test(0x5678),
        );
        assert!(result);
        assert_eq!(
            table
                .pixmap(ResourceId(0x0020_0002))
                .unwrap()
                .host_xid
                .map(|h| h.as_raw()),
            Some(0x5678)
        );
    }

    #[test]
    fn host_drawable_target_window_depth_matches_window_depth() {
        let mut table = ResourceTable::new();
        make_top_level_with_host_xid(&mut table, 0x0010_0002, 0x1234);
        // Set depth to 32
        table.windows.get_mut(&0x0010_0002).unwrap().depth = 32;
        let target = table.host_drawable_target(ResourceId(0x0010_0002));
        if let Some(HostDrawableTarget::Window { depth, .. }) = target {
            assert_eq!(depth, 32);
        } else {
            panic!("Expected Window variant with depth 32");
        }
    }

    #[test]
    fn is_descendant_of_handles_child_grandchild_and_unrelated() {
        let mut table = ResourceTable::new();
        make_window(&mut table, 0x0010_0002);
        make_child(&mut table, 0x0010_0003, 0x0010_0002, 0, 0);
        make_child(&mut table, 0x0010_0004, 0x0010_0003, 0, 0);
        make_window(&mut table, 0x0010_0005);

        assert!(table.is_descendant_of(ResourceId(0x0010_0003), ResourceId(0x0010_0002)));
        assert!(table.is_descendant_of(ResourceId(0x0010_0004), ResourceId(0x0010_0002)));
        assert!(!table.is_descendant_of(ResourceId(0x0010_0005), ResourceId(0x0010_0002)));
        assert!(!table.is_descendant_of(ResourceId(0xdead_beef), ResourceId(0x0010_0002)));
    }

    #[test]
    fn mapped_children_bottom_to_top_filters_unmapped_and_preserves_order() {
        let mut table = ResourceTable::new();
        make_window(&mut table, 0x0010_0002);
        make_child(&mut table, 0x0010_0003, 0x0010_0002, 0, 0);
        make_child(&mut table, 0x0010_0004, 0x0010_0002, 0, 0);
        make_child(&mut table, 0x0010_0005, 0x0010_0002, 0, 0);
        let _ = table.map_window(ResourceId(0x0010_0003));
        let _ = table.map_window(ResourceId(0x0010_0005));

        assert_eq!(
            table.mapped_children_bottom_to_top(ResourceId(0x0010_0002)),
            Some(vec![ResourceId(0x0010_0003), ResourceId(0x0010_0005)])
        );
        assert_eq!(
            table.mapped_children_bottom_to_top(ResourceId(0xdead_beef)),
            None
        );
    }

    #[test]
    fn reparent_window_moves_child_and_updates_position() {
        let mut table = ResourceTable::new();
        make_window(&mut table, 0x0010_0002);
        make_window(&mut table, 0x0010_0003);
        make_child(&mut table, 0x0010_0004, 0x0010_0002, 1, 2);

        let result = table
            .reparent_window(ReparentWindowRequest {
                window: ResourceId(0x0010_0004),
                parent: ResourceId(0x0010_0003),
                x: 10,
                y: 20,
            })
            .unwrap();

        assert_eq!(result.old_parent, ResourceId(0x0010_0002));
        assert_eq!(result.new_parent, ResourceId(0x0010_0003));
        assert!(
            !table
                .children(ResourceId(0x0010_0002))
                .contains(&ResourceId(0x0010_0004))
        );
        assert_eq!(
            table.children(ResourceId(0x0010_0003)),
            &[ResourceId(0x0010_0004)]
        );
        let window = table.window(ResourceId(0x0010_0004)).unwrap();
        assert_eq!(window.parent, ResourceId(0x0010_0003));
        assert_eq!((window.x, window.y), (10, 20));
    }

    #[test]
    fn reparent_window_rejects_invalid_relationships() {
        let mut table = ResourceTable::new();
        make_window(&mut table, 0x0010_0002);
        make_child(&mut table, 0x0010_0003, 0x0010_0002, 0, 0);

        assert_eq!(
            table.reparent_window(ReparentWindowRequest {
                window: ROOT_WINDOW,
                parent: ResourceId(0x0010_0002),
                x: 0,
                y: 0,
            }),
            Err(ReparentWindowError::BadMatch)
        );
        assert_eq!(
            table.reparent_window(ReparentWindowRequest {
                window: ResourceId(0x0010_0002),
                parent: ResourceId(0x0010_0002),
                x: 0,
                y: 0,
            }),
            Err(ReparentWindowError::BadMatch)
        );
        assert_eq!(
            table.reparent_window(ReparentWindowRequest {
                window: ResourceId(0x0010_0002),
                parent: ResourceId(0x0010_0003),
                x: 0,
                y: 0,
            }),
            Err(ReparentWindowError::BadMatch)
        );
    }

    #[test]
    fn reparent_window_rejects_unknown_windows() {
        let mut table = ResourceTable::new();
        make_window(&mut table, 0x0010_0002);

        assert_eq!(
            table.reparent_window(ReparentWindowRequest {
                window: ResourceId(0xdead_beef),
                parent: ResourceId(0x0010_0002),
                x: 0,
                y: 0,
            }),
            Err(ReparentWindowError::BadWindow)
        );
        assert_eq!(
            table.reparent_window(ReparentWindowRequest {
                window: ResourceId(0x0010_0002),
                parent: ResourceId(0xdead_beef),
                x: 0,
                y: 0,
            }),
            Err(ReparentWindowError::BadWindow)
        );
    }

    #[test]
    fn pointer_target_at_returns_deepest_mapped_child_and_relative_coords() {
        let mut table = ResourceTable::new();
        make_window(&mut table, 0x0010_0002);
        make_child(&mut table, 0x0010_0003, 0x0010_0002, 10, 20);
        make_child(&mut table, 0x0010_0004, 0x0010_0003, 5, 6);
        let _ = table.map_window(ResourceId(0x0010_0002));
        let _ = table.map_window(ResourceId(0x0010_0003));
        let _ = table.map_window(ResourceId(0x0010_0004));

        assert_eq!(
            table.pointer_target_at(ResourceId(0x0010_0002), 20, 30),
            Some((ResourceId(0x0010_0004), 5, 4))
        );
    }

    #[test]
    fn pointer_target_at_falls_back_to_top_level_outside_children() {
        let mut table = ResourceTable::new();
        make_window(&mut table, 0x0010_0002);
        make_child(&mut table, 0x0010_0003, 0x0010_0002, 10, 20);
        let _ = table.map_window(ResourceId(0x0010_0002));
        let _ = table.map_window(ResourceId(0x0010_0003));

        assert_eq!(
            table.pointer_target_at(ResourceId(0x0010_0002), 2, 3),
            Some((ResourceId(0x0010_0002), 2, 3))
        );
    }

    proptest! {
        #[test]
        fn unmap_window_state_machine(
            initial in arb_initial(),
            n in 1usize..=5,
        ) {
            let mut table = ResourceTable::new();
            make_window(&mut table, 0x100002);
            let target = ResourceId(0x100002);
            let initial_map_state = match initial {
                InitialState::Viewable => MapState::Viewable,
                InitialState::Unviewable => MapState::Unviewable,
                InitialState::Unmapped => MapState::Unmapped,
            };
            table.windows.get_mut(&target.0).unwrap().map_state = initial_map_state;

            let mut results = Vec::with_capacity(n);
            for _ in 0..n {
                results.push(table.unmap_window(target));
            }

            let expected_first = !matches!(initial, InitialState::Unmapped);
            prop_assert_eq!(results[0], expected_first);
            for r in results.iter().skip(1) {
                prop_assert!(!*r, "subsequent calls must return false");
            }
            prop_assert_eq!(
                table.window(target).unwrap().map_state,
                MapState::Unmapped
            );
        }

    }

    #[test]
    fn circulate_window_raises_lowest_to_top() {
        let mut t = ResourceTable::new();
        make_child(&mut t, 0x200, ROOT_WINDOW.0, 0, 0);
        make_child(&mut t, 0x300, ROOT_WINDOW.0, 0, 0);
        make_child(&mut t, 0x400, ROOT_WINDOW.0, 0, 0);
        let moved = t.circulate_window(ROOT_WINDOW, 0).unwrap();
        assert_eq!(moved, ResourceId(0x400));
        assert_eq!(
            t.children(ROOT_WINDOW),
            &[ResourceId(0x400), ResourceId(0x200), ResourceId(0x300)]
        );
    }

    #[test]
    fn circulate_window_lowers_highest_to_bottom() {
        let mut t = ResourceTable::new();
        make_child(&mut t, 0x200, ROOT_WINDOW.0, 0, 0);
        make_child(&mut t, 0x300, ROOT_WINDOW.0, 0, 0);
        let moved = t.circulate_window(ROOT_WINDOW, 1).unwrap();
        assert_eq!(moved, ResourceId(0x200));
        assert_eq!(
            t.children(ROOT_WINDOW),
            &[ResourceId(0x300), ResourceId(0x200)]
        );
    }

    #[test]
    fn circulate_window_noop_with_lt_two_children() {
        let mut t = ResourceTable::new();
        make_child(&mut t, 0x200, ROOT_WINDOW.0, 0, 0);
        assert!(t.circulate_window(ROOT_WINDOW, 0).is_none());
    }

    #[test]
    fn circulate_raise_on_root_skips_cow() {
        let mut t = ResourceTable::new();
        // Root children, bottom-to-top: [A=0x200 (occluded), B=0x300, COW].
        // We need positions where A is occluded by B for Raise to act on A;
        // for the geometry-free unit test, set both A and B at (0,0,50x50) so
        // B occludes A.
        make_child(&mut t, 0x200, ROOT_WINDOW.0, 0, 0);
        make_child(&mut t, 0x300, ROOT_WINDOW.0, 0, 0);
        let _ = t.map_window(ResourceId(0x200));
        let _ = t.map_window(ResourceId(0x300));
        t.windows
            .get_mut(&ROOT_WINDOW.0)
            .unwrap()
            .children
            .push(COMPOSITE_OVERLAY_WINDOW);
        // Circulate Raise on root: A should rise to "top of the non-COW slice",
        // i.e. just below COW. COW stays last.
        let _ = t.circulate_window(ROOT_WINDOW, 0);
        let kids = &t.window(ROOT_WINDOW).unwrap().children;
        assert_eq!(
            kids.last().copied(),
            Some(COMPOSITE_OVERLAY_WINDOW),
            "COW stays at top across circulate"
        );
        assert_eq!(
            kids[kids.len() - 2],
            ResourceId(0x200),
            "Raise must land 0x200 just below COW, not above it"
        );
    }

    #[test]
    fn configure_window_stack_mode_above_raises_child() {
        let mut t = ResourceTable::new();
        make_child(&mut t, 0x200, ROOT_WINDOW.0, 0, 0);
        make_child(&mut t, 0x300, ROOT_WINDOW.0, 0, 0);
        make_child(&mut t, 0x400, ROOT_WINDOW.0, 0, 0);

        let configured = t.configure_window(ConfigureWindowRequest {
            window: ResourceId(0x200),
            value_mask: 1 << 6,
            x: None,
            y: None,
            width: None,
            height: None,
            border_width: None,
            sibling: None,
            stack_mode: Some(0),
        });

        assert!(configured.is_some());
        assert_eq!(
            t.children(ROOT_WINDOW),
            &[ResourceId(0x300), ResourceId(0x400), ResourceId(0x200)]
        );
    }

    /// Build a request that only sets sibling + stack_mode. Geometry
    /// fields are `None` so existing position/size is preserved.
    fn restack_request(
        window: u32,
        sibling: Option<u32>,
        stack_mode: u8,
    ) -> ConfigureWindowRequest {
        ConfigureWindowRequest {
            window: ResourceId(window),
            value_mask: 0,
            x: None,
            y: None,
            width: None,
            height: None,
            border_width: None,
            sibling: sibling.map(ResourceId),
            stack_mode: Some(stack_mode),
        }
    }

    /// Children A=0x200, B=0x300, C=0x400 under root, all 50×50, mapped.
    /// `a_x` / `b_x` / `c_x` set the x position of each — y is always 0.
    fn three_mapped_children(a_x: i16, b_x: i16, c_x: i16) -> ResourceTable {
        let mut t = ResourceTable::new();
        make_child(&mut t, 0x200, ROOT_WINDOW.0, a_x, 0);
        make_child(&mut t, 0x300, ROOT_WINDOW.0, b_x, 0);
        make_child(&mut t, 0x400, ROOT_WINDOW.0, c_x, 0);
        let _ = t.map_window(ROOT_WINDOW);
        let _ = t.map_window(ResourceId(0x200));
        let _ = t.map_window(ResourceId(0x300));
        let _ = t.map_window(ResourceId(0x400));
        t
    }

    #[test]
    fn stack_mode_above_with_sibling_places_just_above() {
        // A B C ; place A above B → B A C
        let mut t = three_mapped_children(0, 0, 0);
        assert!(
            t.configure_window(restack_request(0x200, Some(0x300), 0))
                .is_some()
        );
        assert_eq!(
            t.children(ROOT_WINDOW),
            &[ResourceId(0x300), ResourceId(0x200), ResourceId(0x400)]
        );
    }

    #[test]
    fn stack_mode_below_with_sibling_places_just_below() {
        // A B C ; place C below B → A C B
        let mut t = three_mapped_children(0, 0, 0);
        assert!(
            t.configure_window(restack_request(0x400, Some(0x300), 1))
                .is_some()
        );
        assert_eq!(
            t.children(ROOT_WINDOW),
            &[ResourceId(0x200), ResourceId(0x400), ResourceId(0x300)]
        );
    }

    #[test]
    fn stack_mode_below_no_sibling_lowers_to_bottom() {
        // A B C ; lower C → C A B
        let mut t = three_mapped_children(0, 0, 0);
        assert!(
            t.configure_window(restack_request(0x400, None, 1))
                .is_some()
        );
        assert_eq!(
            t.children(ROOT_WINDOW),
            &[ResourceId(0x400), ResourceId(0x200), ResourceId(0x300)]
        );
    }

    #[test]
    fn top_if_with_overlapping_higher_sibling_raises_to_top() {
        // A B C all overlapping at (0,0); TopIf on A with sibling=C → C is
        // above A and overlaps → A goes to top.
        let mut t = three_mapped_children(0, 0, 0);
        assert!(
            t.configure_window(restack_request(0x200, Some(0x400), 2))
                .is_some()
        );
        assert_eq!(
            t.children(ROOT_WINDOW),
            &[ResourceId(0x300), ResourceId(0x400), ResourceId(0x200)]
        );
    }

    #[test]
    fn top_if_with_lower_sibling_is_noop() {
        // A B C ; TopIf on C with sibling=A → A is below C, cannot occlude
        // → no-op.
        let mut t = three_mapped_children(0, 0, 0);
        assert!(
            t.configure_window(restack_request(0x400, Some(0x200), 2))
                .is_some()
        );
        assert_eq!(
            t.children(ROOT_WINDOW),
            &[ResourceId(0x200), ResourceId(0x300), ResourceId(0x400)]
        );
    }

    #[test]
    fn top_if_with_higher_sibling_no_overlap_is_noop() {
        // A at x=0, B at x=200, C at x=400 — no geometric overlap.
        let mut t = three_mapped_children(0, 200, 400);
        assert!(
            t.configure_window(restack_request(0x200, Some(0x400), 2))
                .is_some()
        );
        assert_eq!(
            t.children(ROOT_WINDOW),
            &[ResourceId(0x200), ResourceId(0x300), ResourceId(0x400)]
        );
    }

    #[test]
    fn top_if_no_sibling_raises_when_any_higher_overlaps() {
        // A at x=0, B at x=200 (no overlap with A), C at x=20 (overlaps A
        // and is above A) → TopIf on A with no sibling → top.
        let mut t = three_mapped_children(0, 200, 20);
        assert!(
            t.configure_window(restack_request(0x200, None, 2))
                .is_some()
        );
        assert_eq!(
            t.children(ROOT_WINDOW),
            &[ResourceId(0x300), ResourceId(0x400), ResourceId(0x200)]
        );
    }

    #[test]
    fn bottom_if_with_lower_overlapping_sibling_lowers_to_bottom() {
        // A B C all overlapping; BottomIf on C with sibling=A → C is above
        // A and overlaps → C lowers to bottom.
        let mut t = three_mapped_children(0, 0, 0);
        assert!(
            t.configure_window(restack_request(0x400, Some(0x200), 3))
                .is_some()
        );
        assert_eq!(
            t.children(ROOT_WINDOW),
            &[ResourceId(0x400), ResourceId(0x200), ResourceId(0x300)]
        );
    }

    #[test]
    fn bottom_if_with_higher_sibling_is_noop() {
        // A B C ; BottomIf on A with sibling=C → A cannot occlude C
        // (A is below C) → no-op.
        let mut t = three_mapped_children(0, 0, 0);
        assert!(
            t.configure_window(restack_request(0x200, Some(0x400), 3))
                .is_some()
        );
        assert_eq!(
            t.children(ROOT_WINDOW),
            &[ResourceId(0x200), ResourceId(0x300), ResourceId(0x400)]
        );
    }

    #[test]
    fn bottom_if_no_sibling_no_overlap_is_noop() {
        let mut t = three_mapped_children(0, 200, 400);
        assert!(
            t.configure_window(restack_request(0x400, None, 3))
                .is_some()
        );
        assert_eq!(
            t.children(ROOT_WINDOW),
            &[ResourceId(0x200), ResourceId(0x300), ResourceId(0x400)]
        );
    }

    #[test]
    fn opposite_with_higher_overlapping_sibling_goes_to_top() {
        // A B C ; Opposite on A with sibling=C → C above + overlaps → top.
        let mut t = three_mapped_children(0, 0, 0);
        assert!(
            t.configure_window(restack_request(0x200, Some(0x400), 4))
                .is_some()
        );
        assert_eq!(
            t.children(ROOT_WINDOW),
            &[ResourceId(0x300), ResourceId(0x400), ResourceId(0x200)]
        );
    }

    #[test]
    fn opposite_with_lower_overlapping_sibling_goes_to_bottom() {
        // A B C ; Opposite on C with sibling=A → A is below + overlaps
        // (window-occludes-sibling holds) → bottom.
        let mut t = three_mapped_children(0, 0, 0);
        assert!(
            t.configure_window(restack_request(0x400, Some(0x200), 4))
                .is_some()
        );
        assert_eq!(
            t.children(ROOT_WINDOW),
            &[ResourceId(0x400), ResourceId(0x200), ResourceId(0x300)]
        );
    }

    #[test]
    fn opposite_no_overlap_is_noop() {
        let mut t = three_mapped_children(0, 200, 400);
        assert!(
            t.configure_window(restack_request(0x300, None, 4))
                .is_some()
        );
        assert_eq!(
            t.children(ROOT_WINDOW),
            &[ResourceId(0x200), ResourceId(0x300), ResourceId(0x400)]
        );
    }

    #[test]
    fn occlusion_ignores_unmapped_siblings() {
        // A B C all overlapping; unmap C; TopIf on A with no sibling.
        // C is unmapped so cannot occlude; B is above A and overlaps →
        // A still raises to top. Order after restack: [B, C, A].
        let mut t = three_mapped_children(0, 0, 0);
        let _ = t.unmap_window(ResourceId(0x400));
        assert!(
            t.configure_window(restack_request(0x200, None, 2))
                .is_some()
        );
        assert_eq!(
            t.children(ROOT_WINDOW),
            &[ResourceId(0x300), ResourceId(0x400), ResourceId(0x200)]
        );

        // Same shape but with B unmapped too: no mapped occluder remains
        // → no-op.
        let mut t = three_mapped_children(0, 0, 0);
        let _ = t.unmap_window(ResourceId(0x300));
        let _ = t.unmap_window(ResourceId(0x400));
        assert!(
            t.configure_window(restack_request(0x200, None, 2))
                .is_some()
        );
        assert_eq!(
            t.children(ROOT_WINDOW),
            &[ResourceId(0x200), ResourceId(0x300), ResourceId(0x400)]
        );
    }

    #[test]
    fn pointer_hit_test_follows_top_if_restack() {
        // A B at (0,0) overlapping; B is on top so pointer at (10,10) hits
        // B. After TopIf on A → A on top → pointer hits A.
        let mut t = ResourceTable::new();
        make_child(&mut t, 0x200, ROOT_WINDOW.0, 0, 0);
        make_child(&mut t, 0x300, ROOT_WINDOW.0, 0, 0);
        let _ = t.map_window(ROOT_WINDOW);
        let _ = t.map_window(ResourceId(0x200));
        let _ = t.map_window(ResourceId(0x300));

        assert_eq!(
            t.pointer_target_at(ROOT_WINDOW, 10, 10).map(|h| h.0),
            Some(ResourceId(0x300))
        );

        assert!(
            t.configure_window(restack_request(0x200, None, 2))
                .is_some()
        );

        assert_eq!(
            t.pointer_target_at(ROOT_WINDOW, 10, 10).map(|h| h.0),
            Some(ResourceId(0x200))
        );
    }

    #[test]
    fn pointer_hit_test_follows_bottom_if_restack() {
        // A B at (0,0) overlapping; B is on top → pointer hits B.
        // BottomIf on B (no sibling) → B occludes A → bottom → pointer hits A.
        let mut t = ResourceTable::new();
        make_child(&mut t, 0x200, ROOT_WINDOW.0, 0, 0);
        make_child(&mut t, 0x300, ROOT_WINDOW.0, 0, 0);
        let _ = t.map_window(ROOT_WINDOW);
        let _ = t.map_window(ResourceId(0x200));
        let _ = t.map_window(ResourceId(0x300));

        assert!(
            t.configure_window(restack_request(0x300, None, 3))
                .is_some()
        );

        assert_eq!(
            t.pointer_target_at(ROOT_WINDOW, 10, 10).map(|h| h.0),
            Some(ResourceId(0x200))
        );
    }

    #[test]
    fn configure_notify_above_sibling_tracks_restacked_order() {
        let mut t = three_mapped_children(0, 0, 0);
        assert_eq!(t.configure_notify_above_sibling(ResourceId(0x200)), None);
        assert_eq!(
            t.configure_notify_above_sibling(ResourceId(0x300)),
            Some(ResourceId(0x200))
        );
        assert_eq!(
            t.configure_notify_above_sibling(ResourceId(0x400)),
            Some(ResourceId(0x300))
        );

        assert!(
            t.configure_window(restack_request(0x400, Some(0x200), 1))
                .is_some()
        );
        assert_eq!(
            t.children(ROOT_WINDOW),
            &[ResourceId(0x400), ResourceId(0x200), ResourceId(0x300)]
        );
        assert_eq!(t.configure_notify_above_sibling(ResourceId(0x400)), None);
        assert_eq!(
            t.configure_notify_above_sibling(ResourceId(0x200)),
            Some(ResourceId(0x400))
        );
        assert_eq!(
            t.configure_notify_above_sibling(ResourceId(0x300)),
            Some(ResourceId(0x200))
        );
    }

    #[test]
    fn restack_with_sibling_not_in_parent_is_noop() {
        // 0x999 is not a child of root → request must be a no-op (in a
        // real server it would raise BadMatch; the resolver's contract is
        // to leave the local state untouched).
        let mut t = three_mapped_children(0, 0, 0);
        assert!(
            t.configure_window(restack_request(0x200, Some(0x999), 0))
                .is_some()
        );
        assert_eq!(
            t.children(ROOT_WINDOW),
            &[ResourceId(0x200), ResourceId(0x300), ResourceId(0x400)]
        );
    }

    #[test]
    fn visual_table_seeded_with_root_and_argb() {
        let t = ResourceTable::new();
        let root = t.visual(ROOT_VISUAL).expect("root visual seeded");
        assert_eq!(root.depth, 24);
        assert_eq!(root.alpha_mask, 0);
        assert_eq!(root.host_visual_xid, None);
        let argb = t.visual(ARGB_VISUAL).expect("argb visual seeded");
        assert_eq!(argb.depth, 32);
        assert_eq!(argb.alpha_mask, 0xff00_0000);
        assert_eq!(argb.host_visual_xid, None);
    }

    #[test]
    fn colormap_for_visual_returns_matching_entry() {
        let t = ResourceTable::new();
        assert_eq!(
            t.colormap_for_visual(ROOT_VISUAL).map(|c| c.id),
            Some(ROOT_COLORMAP)
        );
        assert_eq!(
            t.colormap_for_visual(ARGB_VISUAL).map(|c| c.id),
            Some(ARGB_COLORMAP)
        );
    }

    #[test]
    fn set_visual_host_xid_persists() {
        let mut t = ResourceTable::new();
        assert!(t.set_visual_host_xid(ARGB_VISUAL, 0x4711));
        assert_eq!(
            t.visual(ARGB_VISUAL)
                .and_then(|v| v.host_visual_xid)
                .map(|h| h.as_raw()),
            Some(0x4711)
        );
        assert!(!t.set_visual_host_xid(ResourceId(0xdead), 0x42));
    }

    #[test]
    fn set_colormap_host_xid_persists() {
        let mut t = ResourceTable::new();
        assert!(t.set_colormap_host_xid(ARGB_COLORMAP, 0x9999));
        assert_eq!(
            t.colormap(ARGB_COLORMAP)
                .and_then(|c| c.host_colormap_xid)
                .map(|h| h.as_raw()),
            Some(0x9999)
        );
    }

    #[test]
    fn is_known_visual_distinguishes_table_entries() {
        let t = ResourceTable::new();
        assert!(t.is_known_visual(ROOT_VISUAL));
        assert!(t.is_known_visual(ARGB_VISUAL));
        // 0 is the wire encoding for CopyFromParent — not in the table; the
        // CreateWindow handler validates separately and never queries this.
        assert!(!t.is_known_visual(ResourceId(0)));
        assert!(!t.is_known_visual(ResourceId(0xdead_beef)));
    }

    #[test]
    fn copy_from_parent_visual_inherits_argb_parent() {
        let mut t = ResourceTable::new();
        // Create an ARGB top-level then a CopyFromParent child of it.
        t.create_window(
            ClientId(1),
            CreateWindowRequest {
                depth: 32,
                window: ResourceId(0x200),
                parent: ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 50,
                height: 50,
                border_width: 0,
                class: 1,
                visual: ARGB_VISUAL,
                ..Default::default()
            },
        );
        t.create_window(
            ClientId(1),
            CreateWindowRequest {
                depth: 0,
                window: ResourceId(0x300),
                parent: ResourceId(0x200),
                x: 0,
                y: 0,
                width: 25,
                height: 25,
                border_width: 0,
                class: 1,
                visual: ResourceId(0), // CopyFromParent
                ..Default::default()
            },
        );
        let child = t.window(ResourceId(0x300)).expect("child created");
        assert_eq!(child.visual, ARGB_VISUAL);
        assert_eq!(child.depth, 32);
    }

    #[test]
    fn newly_created_window_has_no_border_pixmap_host_xid() {
        let mut t = ResourceTable::new();
        t.create_window(
            ClientId(1),
            CreateWindowRequest {
                depth: 0,
                window: ResourceId(0x200),
                parent: ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 50,
                height: 50,
                border_width: 1,
                class: 1,
                visual: ResourceId(0),
                ..Default::default()
            },
        );
        assert_eq!(
            t.window(ResourceId(0x200)).unwrap().border_pixmap_host_xid,
            None
        );
    }

    #[test]
    fn copy_from_parent_visual_inherits_root_visual_for_root_child() {
        let mut t = ResourceTable::new();
        t.create_window(
            ClientId(1),
            CreateWindowRequest {
                depth: 0,
                window: ResourceId(0x200),
                parent: ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 50,
                height: 50,
                border_width: 0,
                class: 1,
                visual: ResourceId(0),
                ..Default::default()
            },
        );
        let w = t.window(ResourceId(0x200)).expect("created");
        assert_eq!(w.visual, ROOT_VISUAL);
        assert_eq!(w.depth, 24);
    }

    fn empty_create_gc_request(gc: ResourceId, drawable: ResourceId) -> CreateGcRequest {
        CreateGcRequest {
            gc,
            drawable,
            function: None,
            plane_mask: None,
            foreground: None,
            background: None,
            line_width: None,
            line_style: None,
            cap_style: None,
            join_style: None,
            fill_style: None,
            fill_rule: None,
            tile: None,
            stipple: None,
            tile_x_origin: None,
            tile_y_origin: None,
            font: None,
            subwindow_mode: None,
            graphics_exposures: None,
            clip_x_origin: None,
            clip_y_origin: None,
            clip_mask: None,
            dash_offset: None,
            dashes: None,
            arc_mode: None,
        }
    }

    fn empty_change_gc(gc: ResourceId) -> GcChange {
        GcChange {
            gc,
            function: None,
            plane_mask: None,
            foreground: None,
            background: None,
            line_width: None,
            line_style: None,
            cap_style: None,
            join_style: None,
            fill_style: None,
            fill_rule: None,
            tile: None,
            stipple: None,
            tile_x_origin: None,
            tile_y_origin: None,
            font: None,
            subwindow_mode: None,
            graphics_exposures: None,
            clip_mask: None,
            clip_x_origin: None,
            clip_y_origin: None,
            dash_offset: None,
            dashes: None,
            arc_mode: None,
        }
    }

    fn install_pixmap_with_host_xid(table: &mut ResourceTable, id: u32, host_xid: u32) {
        table.create_pixmap(
            ClientId(1),
            CreatePixmapRequest {
                depth: 24,
                pixmap: ResourceId(id),
                drawable: ROOT_WINDOW,
                width: 16,
                height: 16,
            },
        );
        assert!(table.set_pixmap_host_xid(
            ResourceId(id),
            crate::backend::PixmapHandle::from_raw_for_test(host_xid),
        ));
    }

    #[test]
    fn change_gc_clip_mask_none_clears_clip_rectangles() {
        let mut t = ResourceTable::new();
        t.create_gc(
            ClientId(1),
            empty_create_gc_request(ResourceId(0x500), ROOT_WINDOW),
        );
        t.set_clip_rectangles(
            yserver_protocol::x11::ClientId(1),
            SetClipRectanglesRequest {
                gc: ResourceId(0x500),
                clip: ClipRectangles {
                    ordering: 0,
                    x_origin: 0,
                    y_origin: 0,
                    rectangles: vec![0, 0, 0, 0, 10, 0, 10, 0],
                },
            },
        );
        assert!(t.gc_clip_rectangles(ResourceId(0x500)).is_some());

        let mut clear = empty_change_gc(ResourceId(0x500));
        clear.clip_mask = Some(None);
        t.change_gc(yserver_protocol::x11::ClientId(1), clear);

        assert!(t.gc_clip_rectangles(ResourceId(0x500)).is_none());
    }

    fn install_dummy_gc(table: &mut ResourceTable, id: u32) {
        table.create_gc(
            ClientId(1),
            empty_create_gc_request(ResourceId(id), ROOT_WINDOW),
        );
    }

    fn install_font_with_host_xid(table: &mut ResourceTable, id: u32, host_xid: u32) {
        table.install_font(
            ClientId(1),
            ResourceId(id),
            "fixed".to_string(),
            crate::backend::FontHandle::from_raw_for_test(host_xid),
            FontMetrics::default(),
        );
    }

    // ---- Phase 6.2 Step 3: copy_gc unit tests for the new fields. ----

    #[test]
    fn copy_gc_function_and_plane_mask() {
        let mut t = ResourceTable::new();
        install_dummy_gc(&mut t, 0x500);
        install_dummy_gc(&mut t, 0x501);
        // Mutate src.
        let mut chg = empty_change_gc(ResourceId(0x500));
        chg.function = Some(GcFunction::Xor.protocol_value());
        chg.plane_mask = Some(0x00ff_00ff);
        t.change_gc(yserver_protocol::x11::ClientId(1), chg);
        // Copy GCFunction (1<<0) + GCPlaneMask (1<<1).
        t.copy_gc(ResourceId(0x500), ResourceId(0x501), 0x0000_0003);
        let dst = t.gc(ResourceId(0x501)).unwrap();
        assert_eq!(dst.function, GcFunction::Xor);
        assert_eq!(dst.plane_mask, 0x00ff_00ff);
    }

    #[test]
    fn copy_gc_line_join_cap_styles() {
        let mut t = ResourceTable::new();
        install_dummy_gc(&mut t, 0x500);
        install_dummy_gc(&mut t, 0x501);
        let mut chg = empty_change_gc(ResourceId(0x500));
        chg.line_style = Some(LineStyle::OnOffDash.protocol_value());
        chg.cap_style = Some(CapStyle::Round.protocol_value());
        chg.join_style = Some(JoinStyle::Bevel.protocol_value());
        t.change_gc(yserver_protocol::x11::ClientId(1), chg);
        // line_style (1<<5) | cap_style (1<<6) | join_style (1<<7) = 0xE0
        t.copy_gc(ResourceId(0x500), ResourceId(0x501), 0x0000_00E0);
        let dst = t.gc(ResourceId(0x501)).unwrap();
        assert_eq!(dst.line_style, LineStyle::OnOffDash);
        assert_eq!(dst.cap_style, CapStyle::Round);
        assert_eq!(dst.join_style, JoinStyle::Bevel);
    }

    #[test]
    fn copy_gc_fill_rule_and_subwindow_mode() {
        let mut t = ResourceTable::new();
        install_dummy_gc(&mut t, 0x500);
        install_dummy_gc(&mut t, 0x501);
        let mut chg = empty_change_gc(ResourceId(0x500));
        chg.fill_rule = Some(FillRule::Winding.protocol_value());
        chg.subwindow_mode = Some(SubwindowMode::IncludeInferiors.protocol_value());
        t.change_gc(yserver_protocol::x11::ClientId(1), chg);
        // fill_rule (1<<9) | subwindow_mode (1<<15) = 0x8200
        t.copy_gc(ResourceId(0x500), ResourceId(0x501), 0x0000_8200);
        let dst = t.gc(ResourceId(0x501)).unwrap();
        assert_eq!(dst.fill_rule, FillRule::Winding);
        assert_eq!(dst.subwindow_mode, SubwindowMode::IncludeInferiors);
    }

    #[test]
    fn copy_gc_graphics_exposures_and_dash_offset() {
        let mut t = ResourceTable::new();
        install_dummy_gc(&mut t, 0x500);
        install_dummy_gc(&mut t, 0x501);
        let mut chg = empty_change_gc(ResourceId(0x500));
        chg.graphics_exposures = Some(false);
        chg.dash_offset = Some(7);
        t.change_gc(yserver_protocol::x11::ClientId(1), chg);
        // graphics_exposures (1<<16) | dash_offset (1<<20) = 0x0011_0000
        t.copy_gc(ResourceId(0x500), ResourceId(0x501), 0x0011_0000);
        let dst = t.gc(ResourceId(0x501)).unwrap();
        assert!(!dst.graphics_exposures);
        assert_eq!(dst.dash_offset, 7);
    }

    #[test]
    fn copy_gc_dashes_and_arc_mode() {
        let mut t = ResourceTable::new();
        install_dummy_gc(&mut t, 0x500);
        install_dummy_gc(&mut t, 0x501);
        let mut chg = empty_change_gc(ResourceId(0x500));
        chg.dashes = Some(9);
        chg.arc_mode = Some(ArcMode::Chord.protocol_value());
        t.change_gc(yserver_protocol::x11::ClientId(1), chg);
        // dashes (1<<21) | arc_mode (1<<22) = 0x00600000
        t.copy_gc(ResourceId(0x500), ResourceId(0x501), 0x0060_0000);
        let dst = t.gc(ResourceId(0x501)).unwrap();
        assert_eq!(dst.dashes, vec![9, 9]);
        assert_eq!(dst.arc_mode, ArcMode::Chord);
    }

    #[test]
    fn copy_gc_zero_mask_copies_nothing() {
        let mut t = ResourceTable::new();
        install_dummy_gc(&mut t, 0x500);
        install_dummy_gc(&mut t, 0x501);
        let mut chg = empty_change_gc(ResourceId(0x500));
        chg.function = Some(GcFunction::Xor.protocol_value());
        t.change_gc(yserver_protocol::x11::ClientId(1), chg);
        t.copy_gc(ResourceId(0x500), ResourceId(0x501), 0);
        let dst = t.gc(ResourceId(0x501)).unwrap();
        assert_eq!(dst.function, GcFunction::Copy);
    }

    // ---- Phase 6.2 Step 3: resolve_draw_state unit tests. ----

    #[test]
    fn resolve_draw_state_default_gc() {
        let mut t = ResourceTable::new();
        install_dummy_gc(&mut t, 0x500);
        let state = t.resolve_draw_state(ResourceId(0x500)).expect("known gc");
        // A fresh GC should match the DrawState::default() for the
        // attribute-only fields.
        let d = DrawState::default();
        assert_eq!(state.foreground, d.foreground);
        assert_eq!(state.background, d.background);
        assert_eq!(state.line_width, d.line_width);
        assert_eq!(state.line_style, d.line_style);
        assert_eq!(state.cap_style, d.cap_style);
        assert_eq!(state.join_style, d.join_style);
        assert_eq!(state.fill_style, d.fill_style);
        assert_eq!(state.fill_rule, d.fill_rule);
        assert_eq!(state.function, d.function);
        assert_eq!(state.plane_mask, d.plane_mask);
        assert_eq!(state.font, None);
        assert_eq!(state.clip, ClipState::None);
        assert_eq!(state.fill, FillState::Solid);
        assert_eq!(state.subwindow_mode, d.subwindow_mode);
        assert!(state.graphics_exposures);
        assert_eq!(state.dashes, d.dashes);
        assert_eq!(state.dash_offset, d.dash_offset);
        assert_eq!(state.arc_mode, d.arc_mode);
    }

    #[test]
    fn resolve_draw_state_tiled_fill_resolves_pixmap_handle() {
        let mut t = ResourceTable::new();
        install_dummy_gc(&mut t, 0x500);
        install_pixmap_with_host_xid(&mut t, 0x600, 0x12345);
        let mut chg = empty_change_gc(ResourceId(0x500));
        chg.fill_style = Some(FillStyle::Tiled.protocol_value());
        chg.tile = Some(ResourceId(0x600));
        chg.tile_x_origin = Some(3);
        chg.tile_y_origin = Some(5);
        t.change_gc(yserver_protocol::x11::ClientId(1), chg);
        let state = t.resolve_draw_state(ResourceId(0x500)).unwrap();
        match state.fill {
            FillState::Tiled { pixmap, origin } => {
                assert_eq!(pixmap.as_raw(), 0x12345);
                assert_eq!(origin, (3, 5));
            }
            other => panic!("expected Tiled, got {other:?}"),
        }
    }

    #[test]
    fn resolve_draw_state_stippled_fill_resolves_pixmap_handle() {
        let mut t = ResourceTable::new();
        install_dummy_gc(&mut t, 0x500);
        install_pixmap_with_host_xid(&mut t, 0x600, 0xabcde);
        let mut chg = empty_change_gc(ResourceId(0x500));
        chg.fill_style = Some(FillStyle::Stippled.protocol_value());
        chg.stipple = Some(ResourceId(0x600));
        t.change_gc(yserver_protocol::x11::ClientId(1), chg);
        let state = t.resolve_draw_state(ResourceId(0x500)).unwrap();
        match state.fill {
            FillState::Stippled { pixmap, origin } => {
                assert_eq!(pixmap.as_raw(), 0xabcde);
                assert_eq!(origin, (0, 0));
            }
            other => panic!("expected Stippled, got {other:?}"),
        }
    }

    #[test]
    fn resolve_draw_state_clip_rectangles_with_origin() {
        let mut t = ResourceTable::new();
        install_dummy_gc(&mut t, 0x500);
        let rect_bytes = vec![0u8, 0, 0, 0, 10, 0, 10, 0];
        t.set_clip_rectangles(
            yserver_protocol::x11::ClientId(1),
            SetClipRectanglesRequest {
                gc: ResourceId(0x500),
                clip: ClipRectangles {
                    ordering: 0,
                    x_origin: 4,
                    y_origin: 7,
                    rectangles: rect_bytes.clone(),
                },
            },
        );
        let mut chg = empty_change_gc(ResourceId(0x500));
        chg.clip_x_origin = Some(11);
        chg.clip_y_origin = Some(13);
        t.change_gc(yserver_protocol::x11::ClientId(1), chg);
        let state = t.resolve_draw_state(ResourceId(0x500)).unwrap();
        match state.clip {
            ClipState::Rectangles { origin, rects } => {
                // origin comes from the GC's clip-x/y-origin (set above),
                // not from the SetClipRectangles x/y_origin (which lives
                // inside the rectangles payload itself).
                assert_eq!(origin, (11, 13));
                assert_eq!(rects.x_origin, 4);
                assert_eq!(rects.y_origin, 7);
                assert_eq!(rects.rectangles, rect_bytes);
            }
            other => panic!("expected Rectangles, got {other:?}"),
        }
    }

    #[test]
    fn resolve_draw_state_pixmap_clip_with_origin() {
        let mut t = ResourceTable::new();
        install_dummy_gc(&mut t, 0x500);
        install_pixmap_with_host_xid(&mut t, 0x600, 0xdead);
        let mut chg = empty_change_gc(ResourceId(0x500));
        chg.clip_mask = Some(Some(ResourceId(0x600)));
        chg.clip_x_origin = Some(2);
        chg.clip_y_origin = Some(3);
        t.change_gc(yserver_protocol::x11::ClientId(1), chg);
        let state = t.resolve_draw_state(ResourceId(0x500)).unwrap();
        match state.clip {
            ClipState::Pixmap { origin, pixmap } => {
                assert_eq!(origin, (2, 3));
                assert_eq!(pixmap.as_raw(), 0xdead);
            }
            other => panic!("expected Pixmap, got {other:?}"),
        }
    }

    #[test]
    fn resolve_draw_state_unknown_gc_returns_none() {
        let t = ResourceTable::new();
        assert!(t.resolve_draw_state(ResourceId(0x999)).is_none());
    }

    #[test]
    fn resolve_draw_state_tiled_with_freed_tile_pixmap_preserves_fill() {
        let mut t = ResourceTable::new();
        install_dummy_gc(&mut t, 0x500);
        install_pixmap_with_host_xid(&mut t, 0x600, 0xbeef);
        let mut chg = empty_change_gc(ResourceId(0x500));
        chg.fill_style = Some(FillStyle::Tiled.protocol_value());
        chg.tile = Some(ResourceId(0x600));
        t.change_gc(yserver_protocol::x11::ClientId(1), chg);
        // Freeing the source pixmap must not clear the GC's retained tile.
        let _ = t.free_pixmap(ResourceId(0x600));
        let state = t.resolve_draw_state(ResourceId(0x500)).unwrap();
        assert_eq!(
            state.fill,
            FillState::Tiled {
                pixmap: crate::backend::PixmapHandle::from_raw(0xbeef).unwrap(),
                origin: (0, 0),
            }
        );
    }

    #[test]
    fn resolve_draw_state_stippled_with_freed_pixmap_preserves_fill() {
        let mut t = ResourceTable::new();
        install_dummy_gc(&mut t, 0x500);
        install_pixmap_with_host_xid(&mut t, 0x600, 0xabcd);
        let mut chg = empty_change_gc(ResourceId(0x500));
        chg.fill_style = Some(FillStyle::Stippled.protocol_value());
        chg.stipple = Some(ResourceId(0x600));
        chg.tile_x_origin = Some(9);
        chg.tile_y_origin = Some(17);
        t.change_gc(yserver_protocol::x11::ClientId(1), chg);
        let _ = t.free_pixmap(ResourceId(0x600));
        let state = t.resolve_draw_state(ResourceId(0x500)).unwrap();
        assert_eq!(
            state.fill,
            FillState::Stippled {
                pixmap: crate::backend::PixmapHandle::from_raw(0xabcd).unwrap(),
                origin: (9, 17),
            }
        );
    }

    #[test]
    fn resolve_draw_state_clip_pixmap_freed_preserves_clip() {
        let mut t = ResourceTable::new();
        install_dummy_gc(&mut t, 0x500);
        install_pixmap_with_host_xid(&mut t, 0x600, 0xcafe);
        let mut chg = empty_change_gc(ResourceId(0x500));
        chg.clip_mask = Some(Some(ResourceId(0x600)));
        t.change_gc(yserver_protocol::x11::ClientId(1), chg);
        let _ = t.free_pixmap(ResourceId(0x600));
        let state = t.resolve_draw_state(ResourceId(0x500)).unwrap();
        assert_eq!(
            state.clip,
            ClipState::Pixmap {
                origin: (0, 0),
                pixmap: crate::backend::PixmapHandle::from_raw(0xcafe).unwrap(),
            }
        );
    }

    #[test]
    fn host_xid_referenced_by_gc_reports_retained_clip_tile_and_stipple() {
        let mut t = ResourceTable::new();
        install_dummy_gc(&mut t, 0x500);
        install_pixmap_with_host_xid(&mut t, 0x601, 0xaaa1);
        install_pixmap_with_host_xid(&mut t, 0x602, 0xaaa2);
        install_pixmap_with_host_xid(&mut t, 0x603, 0xaaa3);

        let mut chg = empty_change_gc(ResourceId(0x500));
        chg.clip_mask = Some(Some(ResourceId(0x601)));
        chg.tile = Some(ResourceId(0x602));
        chg.stipple = Some(ResourceId(0x603));
        t.change_gc(yserver_protocol::x11::ClientId(1), chg);

        let _ = t.free_pixmap(ResourceId(0x601));
        let _ = t.free_pixmap(ResourceId(0x602));
        let _ = t.free_pixmap(ResourceId(0x603));

        assert!(
            t.host_xid_referenced_by_gc(crate::backend::PixmapHandle::from_raw(0xaaa1).unwrap())
        );
        assert!(
            t.host_xid_referenced_by_gc(crate::backend::PixmapHandle::from_raw(0xaaa2).unwrap())
        );
        assert!(
            t.host_xid_referenced_by_gc(crate::backend::PixmapHandle::from_raw(0xaaa3).unwrap())
        );
    }

    #[test]
    fn resolve_draw_state_font_resolves_handle() {
        let mut t = ResourceTable::new();
        install_dummy_gc(&mut t, 0x500);
        install_font_with_host_xid(&mut t, 0x700, 0x4242);
        let mut chg = empty_change_gc(ResourceId(0x500));
        chg.font = Some(ResourceId(0x700));
        t.change_gc(yserver_protocol::x11::ClientId(1), chg);
        let state = t.resolve_draw_state(ResourceId(0x500)).unwrap();
        let f = state.font.expect("font handle");
        assert_eq!(f.as_raw(), 0x4242);
    }

    #[test]
    fn resource_owner_returns_owner_across_table_kinds() {
        let mut t = ResourceTable::new();
        assert_eq!(t.resource_owner(ROOT_WINDOW), Some(SERVER_OWNER));
        t.create_pixmap(
            ClientId(7),
            CreatePixmapRequest {
                pixmap: ResourceId(0x0700_0010),
                drawable: ROOT_WINDOW,
                width: 16,
                height: 16,
                depth: 24,
            },
        );
        assert_eq!(t.resource_owner(ResourceId(0x0700_0010)), Some(ClientId(7)));
        assert_eq!(t.resource_owner(ResourceId(0xdead_beef)), None);
    }

    #[test]
    fn resource_owner_distinguishes_separate_clients() {
        // Regression test for the "shared retain bucket" bug: with two
        // retained clients (32 and 33) both holding a pixmap, killing
        // one of them by resource ID must not affect the other.
        let mut t = ResourceTable::new();
        t.create_pixmap(
            ClientId(32),
            CreatePixmapRequest {
                pixmap: ResourceId(0x0200_0001),
                drawable: ROOT_WINDOW,
                width: 8,
                height: 8,
                depth: 24,
            },
        );
        t.create_pixmap(
            ClientId(33),
            CreatePixmapRequest {
                pixmap: ResourceId(0x0210_0001),
                drawable: ROOT_WINDOW,
                width: 8,
                height: 8,
                depth: 24,
            },
        );
        assert_eq!(
            t.resource_owner(ResourceId(0x0200_0001)),
            Some(ClientId(32))
        );
        assert_eq!(
            t.resource_owner(ResourceId(0x0210_0001)),
            Some(ClientId(33))
        );
        // Removing one client's resources must leave the other's
        // intact.
        let _ = t.remove_non_window_resources_owned_by(ClientId(32));
        assert!(t.resource_owner(ResourceId(0x0200_0001)).is_none());
        assert_eq!(
            t.resource_owner(ResourceId(0x0210_0001)),
            Some(ClientId(33))
        );
    }

    #[test]
    fn cow_aware_top_index_with_no_cow_returns_end() {
        let mut t = ResourceTable::new();
        make_child(&mut t, 0x200, ROOT_WINDOW.0, 0, 0);
        make_child(&mut t, 0x300, ROOT_WINDOW.0, 0, 0);
        let root = t.window(ROOT_WINDOW).unwrap();
        assert_eq!(cow_aware_top_index(root), root.children.len());
    }

    #[test]
    fn cow_aware_top_index_with_cow_at_top_returns_just_below() {
        let mut t = ResourceTable::new();
        make_child(&mut t, 0x200, ROOT_WINDOW.0, 0, 0);
        // Simulate the post-Phase-2 state where COW is the last (topmost) root child.
        // For this isolated unit test, push it directly so we can exercise the helper.
        t.windows
            .get_mut(&ROOT_WINDOW.0)
            .unwrap()
            .children
            .push(COMPOSITE_OVERLAY_WINDOW);
        let root = t.window(ROOT_WINDOW).unwrap();
        assert_eq!(cow_aware_top_index(root), root.children.len() - 1);
    }

    #[test]
    fn create_window_with_cow_present_inserts_below_cow() {
        let mut t = ResourceTable::new();
        t.windows
            .get_mut(&ROOT_WINDOW.0)
            .unwrap()
            .children
            .push(COMPOSITE_OVERLAY_WINDOW);
        make_child(&mut t, 0x500, ROOT_WINDOW.0, 0, 0);
        let kids = &t.window(ROOT_WINDOW).unwrap().children;
        assert_eq!(
            kids.last().copied(),
            Some(COMPOSITE_OVERLAY_WINDOW),
            "COW must stay at top after create_window"
        );
        assert_eq!(
            kids[kids.len() - 2],
            ResourceId(0x500),
            "new top-level lands just below COW"
        );
    }

    #[test]
    fn restack_top_with_cow_present_lands_just_below_cow() {
        let mut t = ResourceTable::new();
        make_child(&mut t, 0x200, ROOT_WINDOW.0, 0, 0);
        make_child(&mut t, 0x300, ROOT_WINDOW.0, 0, 0);
        t.windows
            .get_mut(&ROOT_WINDOW.0)
            .unwrap()
            .children
            .push(COMPOSITE_OVERLAY_WINDOW);
        // children now: [0x200, 0x300, COW]
        let _ = t.configure_window(ConfigureWindowRequest {
            window: ResourceId(0x200),
            value_mask: 1 << 6,
            x: None,
            y: None,
            width: None,
            height: None,
            border_width: None,
            sibling: None,
            stack_mode: Some(0), // 0 = Above (with no sibling = top)
        });
        let kids = &t.window(ROOT_WINDOW).unwrap().children;
        assert_eq!(
            kids,
            &[
                ResourceId(0x300),
                ResourceId(0x200),
                COMPOSITE_OVERLAY_WINDOW
            ]
        );
    }

    #[test]
    fn restack_above_cow_caps_to_just_below_cow() {
        let mut t = ResourceTable::new();
        make_child(&mut t, 0x200, ROOT_WINDOW.0, 0, 0);
        make_child(&mut t, 0x300, ROOT_WINDOW.0, 0, 0);
        t.windows
            .get_mut(&ROOT_WINDOW.0)
            .unwrap()
            .children
            .push(COMPOSITE_OVERLAY_WINDOW);
        // children: [0x200, 0x300, COW]; ask for 0x200 above COW.
        let _ = t.configure_window(ConfigureWindowRequest {
            window: ResourceId(0x200),
            value_mask: 0,
            x: None,
            y: None,
            width: None,
            height: None,
            border_width: None,
            sibling: Some(COMPOSITE_OVERLAY_WINDOW),
            stack_mode: Some(0), // Above
        });
        let kids = &t.window(ROOT_WINDOW).unwrap().children;
        assert_eq!(
            kids.last().copied(),
            Some(COMPOSITE_OVERLAY_WINDOW),
            "COW must remain at top after AboveSibling=COW"
        );
        assert_eq!(
            kids[kids.len() - 2],
            ResourceId(0x200),
            "0x200 must land just below COW (capped)"
        );
    }

    #[test]
    fn reparent_to_root_with_cow_present_lands_below_cow() {
        let mut t = ResourceTable::new();
        // Build: root → container; container → child.
        make_child(&mut t, 0xc0, ROOT_WINDOW.0, 0, 0);
        make_child(&mut t, 0xd0, 0xc0, 0, 0);
        // Put COW at top of root.
        t.windows
            .get_mut(&ROOT_WINDOW.0)
            .unwrap()
            .children
            .push(COMPOSITE_OVERLAY_WINDOW);
        // Reparent the inner child up to root.
        let _ = t.reparent_window(ReparentWindowRequest {
            window: ResourceId(0xd0),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
        });
        let kids = &t.window(ROOT_WINDOW).unwrap().children;
        assert_eq!(
            kids.last().copied(),
            Some(COMPOSITE_OVERLAY_WINDOW),
            "COW must remain topmost after reparent-to-root"
        );
        assert!(
            kids.contains(&ResourceId(0xd0)),
            "the reparented child must appear in root.children"
        );
        assert_ne!(
            kids.last().copied(),
            Some(ResourceId(0xd0)),
            "the reparented child must NOT be above COW"
        );
    }

    #[test]
    fn fresh_resources_does_not_contain_cow() {
        let t = ResourceTable::new();
        assert!(
            t.window(COMPOSITE_OVERLAY_WINDOW).is_none(),
            "COW must NOT be pre-seeded; it materializes only on GetOverlayWindow"
        );
        assert!(
            !t.window(ROOT_WINDOW)
                .unwrap()
                .children
                .contains(&COMPOSITE_OVERLAY_WINDOW),
            "fresh root.children must not contain COW"
        );
    }

    #[test]
    fn materialize_cow_resource_creates_record_and_inserts_at_top() {
        let mut t = ResourceTable::new();
        make_child(&mut t, 0x200, ROOT_WINDOW.0, 0, 0);
        let host_xid = crate::backend::WindowHandle::from_raw_panicking(COMPOSITE_OVERLAY_WINDOW.0);
        t.materialize_cow_resource(host_xid);

        let cow = t
            .window(COMPOSITE_OVERLAY_WINDOW)
            .expect("COW resource exists after materialize");
        assert_eq!(cow.host_xid, Some(host_xid));
        assert_eq!(cow.parent, ROOT_WINDOW);
        assert!(cow.override_redirect);
        assert_eq!(cow.depth, 24);
        assert_eq!(cow.class, WindowClass::InputOutput);
        assert_eq!(cow.map_state, MapState::Viewable);

        let root = t.window(ROOT_WINDOW).unwrap();
        // COW geometry matches root extent.
        assert_eq!(cow.width, root.width, "COW width mirrors root");
        assert_eq!(cow.height, root.height, "COW height mirrors root");

        let kids = &root.children;
        assert_eq!(
            kids.last().copied(),
            Some(COMPOSITE_OVERLAY_WINDOW),
            "COW lands at the top of root.children",
        );
    }

    #[test]
    #[should_panic(expected = "COW already materialized")]
    fn materialize_cow_resource_panics_if_already_materialized() {
        let mut t = ResourceTable::new();
        let host_xid = crate::backend::WindowHandle::from_raw_panicking(COMPOSITE_OVERLAY_WINDOW.0);
        t.materialize_cow_resource(host_xid);
        // Second call without intervening destroy must panic — repeated
        // GetOverlayWindow calls without ReleaseOverlayWindow are
        // refcount-only and must not reach this function.
        t.materialize_cow_resource(host_xid);
    }

    #[test]
    fn destroy_cow_resource_removes_record_and_root_child() {
        let mut t = ResourceTable::new();
        let host_xid = crate::backend::WindowHandle::from_raw_panicking(COMPOSITE_OVERLAY_WINDOW.0);
        t.materialize_cow_resource(host_xid);
        t.destroy_cow_resource();
        assert!(t.window(COMPOSITE_OVERLAY_WINDOW).is_none());
        assert!(
            !t.window(ROOT_WINDOW)
                .unwrap()
                .children
                .contains(&COMPOSITE_OVERLAY_WINDOW)
        );
    }

    #[test]
    fn reparent_cow_under_its_own_descendant_is_bad_match() {
        // Materialize COW (child of root), then create a child C under COW.
        let mut t = ResourceTable::new();
        let host_xid = crate::backend::WindowHandle::from_raw_panicking(0x4000_0103);
        t.materialize_cow_resource(host_xid);
        make_child(&mut t, 0xC0, COMPOSITE_OVERLAY_WINDOW.0, 0, 0);

        // ReparentWindow(window=COW, parent=C) would make COW a descendant
        // of itself -> BadMatch via the GENERIC cycle rule (no COW-special
        // code). is_descendant_of(parent=C, window=COW) is true because C's
        // parent chain reaches COW.
        let err = t.reparent_window(ReparentWindowRequest {
            window: COMPOSITE_OVERLAY_WINDOW,
            parent: ResourceId(0xC0),
            x: 0,
            y: 0,
        });
        assert!(
            matches!(err, Err(ReparentWindowError::BadMatch)),
            "reparenting COW under its own child must be BadMatch (generic cycle rule)"
        );
    }

    #[test]
    fn reparent_cow_to_a_normal_toplevel_succeeds_xorg_faithful() {
        // Xorg does NOT reject moving the COW to an unrelated window. yserver
        // must match: a reparent to a non-descendant, non-root window succeeds.
        let mut t = ResourceTable::new();
        make_child(&mut t, 0x200, ROOT_WINDOW.0, 0, 0); // unrelated top-level W
        let host_xid = crate::backend::WindowHandle::from_raw_panicking(0x4000_0103);
        t.materialize_cow_resource(host_xid);

        let res = t.reparent_window(ReparentWindowRequest {
            window: COMPOSITE_OVERLAY_WINDOW,
            parent: ResourceId(0x200),
            x: 0,
            y: 0,
        });
        assert!(
            res.is_ok(),
            "reparenting COW to an unrelated window succeeds — matches Xorg \
             (compositing breaks, but that's the compositor's bug, not ours)"
        );
        assert_eq!(
            t.window(COMPOSITE_OVERLAY_WINDOW).unwrap().parent,
            ResourceId(0x200)
        );
    }

    #[test]
    fn cursor_anim_flag_default_false_settable_and_freed() {
        let mut table = ResourceTable::new();
        let id = ResourceId(0x500);
        table.create_cursor(ClientId(1), id);
        assert!(!table.cursor_is_anim(id), "fresh cursor must not be anim");
        table.set_cursor_anim(id);
        assert!(table.cursor_is_anim(id));
        // Unknown id is never anim.
        assert!(!table.cursor_is_anim(ResourceId(0x501)));
        // create_glyph_cursor also defaults to false.
        let gid = ResourceId(0x502);
        table.create_glyph_cursor(ClientId(1), gid);
        assert!(!table.cursor_is_anim(gid));
        // After free, the cursor is gone; cursor_is_anim must not return true.
        table.free_cursor(id);
        assert!(!table.cursor_is_anim(id), "freed cursor must not be anim");
    }
}
