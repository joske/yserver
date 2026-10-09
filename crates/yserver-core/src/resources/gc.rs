use super::*;

impl ResourceTable {
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
    ///
    /// Returns the host pixmaps the change displaced from the GC, for the
    /// caller's orphan gate (Xorg drops the GC's ref on the old tile /
    /// stipple at `dix/gc.c:256`/`:273`).
    pub fn change_gc(
        &mut self,
        _requester: ClientId,
        request: GcChange,
    ) -> Vec<crate::backend::PixmapHandle> {
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
            return Vec::new();
        };
        let before = gc.held_host_pixmaps();
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
        Gc::displaced(before, gc.held_host_pixmaps())
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
    /// Returns the clip-mask host pixmap it displaced, as [`Self::change_gc`].
    pub fn set_clip_rectangles(
        &mut self,
        _requester: ClientId,
        request: SetClipRectanglesRequest,
    ) -> Vec<crate::backend::PixmapHandle> {
        let Some(gc) = self.gcs.get_mut(&request.gc.0) else {
            return Vec::new();
        };
        let before = gc.held_host_pixmaps();
        // SetClipRectangles supersedes any prior clip-mask pixmap, and its
        // origin is the GC's clip origin (`SetClipRects`, `dix/gc.c:1021`):
        // a later ChangeGC of the origin moves the same rectangles.
        gc.clip_pixmap = None;
        gc.clip_pixmap_host_xid = None;
        gc.clip_x_origin = request.clip.x_origin;
        gc.clip_y_origin = request.clip.y_origin;
        gc.clip_rectangles = Some(request.clip);
        Gc::displaced(before, gc.held_host_pixmaps())
    }

    /// The GC's clip origin alone, as XFixesSetGCClipRegion sets it even
    /// for region None (`xfixes/region.c:617-619`).
    pub fn set_gc_clip_origin(&mut self, gc: ResourceId, x: i16, y: i16) {
        if let Some(g) = self.gcs.get_mut(&gc.0) {
            g.clip_x_origin = x;
            g.clip_y_origin = y;
        }
    }

    /// Returns the clip-mask host pixmap it displaced, as [`Self::change_gc`].
    pub fn clear_gc_clip(&mut self, gc: ResourceId) -> Vec<crate::backend::PixmapHandle> {
        let Some(g) = self.gcs.get_mut(&gc.0) else {
            return Vec::new();
        };
        let before = g.held_host_pixmaps();
        g.clip_rectangles = None;
        g.clip_pixmap = None;
        g.clip_pixmap_host_xid = None;
        Gc::displaced(before, g.held_host_pixmaps())
    }

    /// Returns the host pixmaps copied over in `dst`, as [`Self::change_gc`]
    /// (Xorg `CopyGC` drops the old tile / stipple, `dix/gc.c:673`/`:685`).
    pub fn copy_gc(
        &mut self,
        src: ResourceId,
        dst: ResourceId,
        value_mask: u32,
    ) -> Vec<crate::backend::PixmapHandle> {
        // Snapshot the source GC under the immutable borrow so we can
        // then take a mutable borrow of dst. Cheap because the only
        // owned field copied here is `dashes`.
        let Some(src_gc) = self.gcs.get(&src.0).cloned() else {
            return Vec::new();
        };
        let Some(dst_gc) = self.gcs.get_mut(&dst.0) else {
            return Vec::new();
        };
        let before = dst_gc.held_host_pixmaps();
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
        Gc::displaced(before, dst_gc.held_host_pixmaps())
    }

    /// Returns the host pixmaps the GC held (Xorg `FreeGC`, `dix/gc.c:776-781`).
    pub fn free_gc(&mut self, id: ResourceId) -> Vec<crate::backend::PixmapHandle> {
        self.gcs
            .remove(&id.0)
            .map(|gc| Gc::displaced(gc.held_host_pixmaps(), [None; 3]))
            .unwrap_or_default()
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

impl Gc {
    /// Host pixmaps this GC keeps alive: clip mask, tile, stipple.
    pub(super) fn held_host_pixmaps(&self) -> [Option<crate::backend::PixmapHandle>; 3] {
        [
            self.clip_pixmap_host_xid,
            self.tile_host_xid,
            self.stipple_host_xid,
        ]
    }

    /// Handles in `before` that `after` no longer holds, deduplicated.
    fn displaced(
        before: [Option<crate::backend::PixmapHandle>; 3],
        after: [Option<crate::backend::PixmapHandle>; 3],
    ) -> Vec<crate::backend::PixmapHandle> {
        let mut out: Vec<_> = before
            .into_iter()
            .flatten()
            .filter(|h| !after.contains(&Some(*h)))
            .collect();
        out.sort_unstable_by_key(|h| h.as_raw());
        out.dedup();
        out
    }
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
    pub(super) fn with_defaults(id: ResourceId, drawable: ResourceId, owner: ClientId) -> Self {
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
