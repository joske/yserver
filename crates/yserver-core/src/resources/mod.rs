#![allow(dead_code)]

mod cleanup;
mod gc;
mod picture_font_cursor;
mod pixmap_props;
#[cfg(test)]
mod tests;
mod tree;
mod types;
mod window;

pub use gc::*;
pub use picture_font_cursor::*;
pub use pixmap_props::*;
pub use types::*;
pub use window::*;

use std::collections::{HashMap, HashSet};

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

pub const ROOT_WINDOW: ResourceId = ResourceId(0x100);
pub const ROOT_COLORMAP: ResourceId = ResourceId(0x101);
pub const ROOT_VISUAL: ResourceId = ResourceId(0x102);
/// The root's background until a client sets one: what the backend fills
/// the root storage with, so that root exposures repaint the same colour.
pub const ROOT_DEFAULT_BACKGROUND_PIXEL: u32 = 0x0050_5050;
/// Composite overlay window returned by `XCompositeGetOverlayWindow`.
/// A real, distinct XID is required — marco's compositor calls
/// `XSelectInput(overlay, ExposureMask)`, which would otherwise clobber
/// its own WM event mask on root (dropping SubstructureRedirect) if we
/// returned root here.
pub const COMPOSITE_OVERLAY_WINDOW: ResourceId = ResourceId(0x103);
pub const ARGB_VISUAL: ResourceId = ResourceId(0x103);
pub const ARGB_COLORMAP: ResourceId = ResourceId(0x104);
/// Opaque depth-24 visual dedicated to the stencil-free GLX FBConfig used by
/// glmark2. It must not share ROOT_VISUAL with the stencil-8 configuration.
pub const GLMARK_VISUAL: ResourceId = ResourceId(0x105);
/// The MIT-SCREEN-SAVER window, server-owned: Xorg allocates
/// `pScreen->screensaver.wid` once per screen and reports it in
/// `QueryInfo` and `ScreenSaverNotify` whether or not it exists
/// (`Xext/saver.c:665`, `:424`); it does while a client's
/// `SetAttributes` are shown.
pub const SCREEN_SAVER_WINDOW: ResourceId = ResourceId(0x106);

/// The X11 depth of the root window, as advertised in the setup reply.
///
/// This is a CLIENT-VISIBLE protocol constant, not a storage property. Our
/// scanout and root readback storage is 32-bit BGRA, but the root DRAWABLE is
/// depth 24, and every reply that names a depth — `GetImage`, `GetGeometry`,
/// plane-mask truncation — has to say 24. Reading the storage's depth instead
/// makes the server claim depth 32 for the root, which is what
/// `tools/depth32-bg-probe.c` measured against Xorg 21.1.24's 24.
pub const ROOT_DEPTH: u8 = 24;

/// X11 visual class codes (subset we care about). The setup reply
/// advertises these per-visual; clients pass a visual ID into
/// `CreateWindow`, `CreateColormap`, and RENDER `CreatePicture`.
pub const VISUAL_CLASS_TRUE_COLOR: u8 = 4;

#[derive(Debug)]
pub struct ResourceTable {
    windows: HashMap<u32, Window>,
    pixmaps: HashMap<u32, Pixmap>,
    gcs: HashMap<u32, Gc>,
    fonts: HashMap<u32, Font>,
    cursors: HashMap<u32, Cursor>,
    /// XFIXES cursor names keyed by host cursor handle. In Xorg the name
    /// lives on the cursor object, so it outlives the client's cursor XID
    /// while a window still displays the cursor (`XFreeCursor` right after
    /// `XDefineCursor` is the common Xlib idiom) — `CursorNotify` and
    /// `ChangeCursorByName` must still see it.
    cursor_host_names: HashMap<u32, yserver_protocol::x11::AtomId>,
    pub pictures: HashMap<u32, PictureState>,
    pub glyphsets: HashMap<u32, GlyphSetState>,
    host_glyphset_refcounts: HashMap<u32, usize>,
    visuals: HashMap<u32, Visual>,
    colormaps: HashMap<u32, Colormap>,
    /// Core-side DRI3 syncobj resource records. The DRM handles live in the
    /// backend, but the XIDs belong to the ordinary global X resource
    /// namespace and obey close-down retention semantics.
    dri3_syncobjs: HashMap<u32, ClientId>,
    /// Backings whose last freed name's alias ref waits for a background/border/GC release site.
    deferred_name_refs: HashSet<u32>,
    /// Host cursors a window let go of (cursor change, destroy); drained by
    /// [`Self::take_unreferenced_cursor_hosts`].
    dropped_cursor_hosts: Vec<u32>,
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
                background_pixel: ROOT_DEFAULT_BACKGROUND_PIXEL,
                background_pixmap: None,
                background_none: false,
                background_pixmap_host_xid: None,
                // Xorg `CreateRootWindow`: borderIsPixel + blackPixel.
                border: BorderSource::Pixel(0),
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
                cursor_host: None,
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
        visuals.insert(
            GLMARK_VISUAL.0,
            Visual {
                id: GLMARK_VISUAL,
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
            cursor_host_names: HashMap::new(),
            pictures: HashMap::new(),
            glyphsets: HashMap::new(),
            host_glyphset_refcounts: HashMap::new(),
            visuals,
            colormaps,
            dri3_syncobjs: HashMap::new(),
            deferred_name_refs: HashSet::new(),
            dropped_cursor_hosts: Vec::new(),
        }
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
    /// (window, pixmap, gc, font, cursor, colormap, picture, glyphset, or
    /// DRI3 syncobj). Used by `CreateXxx` opcodes to detect a
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
            || self.dri3_syncobjs.contains_key(&id)
    }

    /// Append all table-owned XIDs within `base..=base|mask` to `out`
    /// (the 9 ResourceTable namespaces; extension-map namespaces are
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
        out.extend(self.dri3_syncobjs.keys().filter(in_range));
    }

    pub fn register_dri3_syncobj(&mut self, id: ResourceId, owner: ClientId) -> bool {
        self.dri3_syncobjs.insert(id.0, owner).is_none()
    }

    pub fn dri3_syncobj_owner(&self, id: ResourceId) -> Option<ClientId> {
        self.dri3_syncobjs.get(&id.0).copied()
    }

    pub fn remove_dri3_syncobj(&mut self, id: ResourceId) -> Option<ClientId> {
        self.dri3_syncobjs.remove(&id.0)
    }

    /// Seed a minimal GC into the resource table. For use in tests only.
    #[cfg(test)]
    pub fn seed_gc_for_test(&mut self, owner: ClientId, id: ResourceId) {
        self.gcs
            .insert(id.0, Gc::with_defaults(id, ROOT_WINDOW, owner));
    }

    /// Seed a GC that names `host_xid` as its tile — the GC arm of
    /// [`Self::host_xid_still_referenced`]. For use in tests only.
    #[cfg(test)]
    pub fn seed_gc_with_tile_for_test(
        &mut self,
        owner: ClientId,
        id: ResourceId,
        host_xid: crate::backend::PixmapHandle,
    ) {
        let mut gc = Gc::with_defaults(id, ROOT_WINDOW, owner);
        gc.tile_host_xid = Some(host_xid);
        self.gcs.insert(id.0, gc);
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
}
