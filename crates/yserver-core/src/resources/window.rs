use super::*;

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

/// X11 window-gravity offset for a child when its parent's inner size
/// changes by `(dw, dh)`. Mirrors Xorg `dix/window.c::GravityTranslate`
/// (verified against ../xserver). NorthWest (1), Unmap/Forget (0), unknown,
/// and Static (10) on a pure resize with no parent-origin shift all leave
/// the child in place → `(0, 0)`. Integer division matches Xorg (`dw / 2`).
#[must_use]
pub(crate) fn win_gravity_delta(gravity: u8, dw: i32, dh: i32) -> (i32, i32) {
    match gravity {
        2 => (dw / 2, 0),      // North
        3 => (dw, 0),          // NorthEast
        4 => (0, dh / 2),      // West
        5 => (dw / 2, dh / 2), // Center
        6 => (dw, dh / 2),     // East
        7 => (0, dh),          // SouthWest
        8 => (dw / 2, dh),     // South
        9 => (dw, dh),         // SouthEast
        _ => (0, 0),           // NorthWest(1) / Static(10) pure resize / Unmap(0) / unknown
    }
}

impl ResourceTable {
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
        // Xorg `dix/window.c:879`: a new window INHERITS its parent's
        // border (pixel as pixel, pixmap as pixmap — the pixmap
        // "refcnt++" is the shared host-handle snapshot here), it does
        // not default to `Pixel(0)`. Request attributes then override:
        // CWBorderPixmap = CopyFromParent (XID 0) re-adopts the parent's
        // border, and CWBorderPixel overrides CWBorderPixmap when both
        // bits are set (`dix/window.c:1298`).
        let parent_border = parent.map_or(BorderSource::Pixel(0), |p| p.border);
        let border = match (request.border_pixmap, request.border_pixel) {
            (_, Some(pixel)) => BorderSource::Pixel(pixel),
            (Some(pixmap), None) if pixmap.0 == 0 => parent_border,
            (Some(pixmap), None) => BorderSource::Pixmap {
                id: pixmap,
                host_xid: self.pixmaps.get(&pixmap.0).and_then(|pm| pm.host_xid),
            },
            (None, None) => parent_border,
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
            border,
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
            cursor: request.cursor,
            cursor_host: request
                .cursor
                .filter(|c| c.0 != 0)
                .and_then(|c| self.cursors.get(&c.0).and_then(|c| c.host_xid)),
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
        if request.parent == ROOT_WINDOW
            && log::log_enabled!(target: "yserver::input::restack", log::Level::Trace)
        {
            log::trace!(
                target: "yserver::input::restack",
                "CREATE win=0x{:x} parent=ROOT -> inserted at top | root after: [{}]",
                request.window.0,
                self.debug_root_order(),
            );
        }
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
    /// retained ATTRIBUTE-pixmap host XID — background AND border. Caller
    /// frees the ones that turn out to be orphaned
    /// ([`Self::host_xid_still_referenced`]).
    ///
    /// Borders were missing here (#133), so a tile kept alive past its
    /// `FreePixmap` by a window's border reference was never released when
    /// that window died: afterwards no resource owned it and no window
    /// referenced it, so nothing was left to notice. Deduplicated, because one
    /// tile can be both the background and the border — of one window, or of
    /// two in the same subtree — and the caller turns each entry into a host
    /// free.
    pub fn collect_attribute_pixmap_host_xids(&self, root: ResourceId) -> Vec<u32> {
        let mut out = Vec::new();
        self.collect_attribute_pixmap_host_xids_inner(root, &mut out);
        out.sort_unstable();
        out.dedup();
        out
    }

    fn collect_attribute_pixmap_host_xids_inner(&self, id: ResourceId, out: &mut Vec<u32>) {
        let Some(window) = self.windows.get(&id.0) else {
            return;
        };
        if let Some(xid) = window.background_pixmap_host_xid {
            out.push(xid.as_raw());
        }
        if let BorderSource::Pixmap {
            host_xid: Some(xid),
            ..
        } = window.border
        {
            out.push(xid.as_raw());
        }
        for child in &window.children {
            self.collect_attribute_pixmap_host_xids_inner(*child, out);
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
        if let Some(host) = window.cursor_host {
            self.dropped_cursor_hosts.push(host.as_raw());
        }
        destroyed.push(id);
        for child in window.children {
            self.destroy_window_inner(child, destroyed);
        }
    }

    /// Apply attribute changes. Returns the pixmap host handles the apply
    /// released — the caller frees each on the host once fully orphaned
    /// (no other window background/border references it and no live
    /// pixmap resource owns it).
    pub fn change_window_attributes(
        &mut self,
        request: ChangeWindowAttributesRequest,
    ) -> ReleasedAttrPixmaps {
        let mut released = ReleasedAttrPixmaps::default();
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
        // Pre-compute the new border source before the mutable borrow.
        // CWBorderPixel overrides CWBorderPixmap when both bits are set
        // (Xorg `dix/window.c:1298` clears the pixmap bit so the ddx
        // layer never sees both), so the pixmap is not even resolved
        // then. CWBorderPixmap = CopyFromParent (XID 0) adopts the
        // parent's CURRENT border — pixel AS pixel, pixmap AS pixmap
        // (`dix/window.c:1251`).
        let new_border_source: Option<BorderSource> =
            match (request.border_pixmap, request.border_pixel) {
                (_, Some(pixel)) => Some(BorderSource::Pixel(pixel)),
                (Some(pixmap), None) if pixmap.0 == 0 => {
                    let parent_border = self
                        .windows
                        .get(&request.window.0)
                        .and_then(|w| self.windows.get(&w.parent.0))
                        .map(|p| p.border);
                    Some(parent_border.unwrap_or(BorderSource::Pixel(0)))
                }
                (Some(pixmap), None) => Some(BorderSource::Pixmap {
                    id: pixmap,
                    host_xid: self.pixmaps.get(&pixmap.0).and_then(|p| p.host_xid),
                }),
                (None, None) => None,
            };

        if let Some(window) = self.windows.get_mut(&request.window.0) {
            if let Some(bg_pixmap) = request.background_pixmap {
                let new_resource_id = if bg_pixmap.0 == 0 {
                    None
                } else {
                    Some(bg_pixmap)
                };
                if window.background_pixmap_host_xid != new_bg_host_xid.flatten() {
                    released.background = window.background_pixmap_host_xid;
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
            if let Some(new_source) = new_border_source {
                // Replacing a pixmap border releases the window's
                // reference to it (`dix/window.c:1290` DestroyPixmap);
                // re-installing the same source releases nothing.
                if window.border != new_source
                    && let BorderSource::Pixmap {
                        host_xid: Some(old),
                        ..
                    } = window.border
                {
                    released.border = Some(old);
                }
                window.border = new_source;
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
                let host = (cursor.0 != 0)
                    .then(|| self.cursors.get(&cursor.0).and_then(|c| c.host_xid))
                    .flatten();
                if let Some(old) = window.cursor_host.filter(|old| Some(*old) != host) {
                    self.dropped_cursor_hosts.push(old.as_raw());
                }
                window.cursor_host = host;
            }
        }

        released
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

    /// X11 parent-resize window gravity: reposition each child of `parent`
    /// per its `win_gravity` when the parent's inner size changes from
    /// `(old_w, old_h)` to `(new_w, new_h)`. Returns the children that
    /// actually moved, with their new parent-relative `(x, y)` — the caller
    /// propagates each to the host mirror and emits `GravityNotify`.
    ///
    /// Mirrors Xorg `dix/window.c::ResizeChildrenWinSize` +
    /// `GravityTranslate` (verified against ../xserver): only children with
    /// `win_gravity` past NorthWest move; Static on a pure resize (no
    /// parent-origin shift) and Unmap/NorthWest never move — all covered by
    /// [`win_gravity_delta`] returning `(0, 0)`. Grandchildren keep their
    /// parent-relative origin and move implicitly with the child.
    pub fn apply_win_gravity(
        &mut self,
        parent: ResourceId,
        old_w: u16,
        old_h: u16,
        new_w: u16,
        new_h: u16,
    ) -> Vec<(ResourceId, i16, i16)> {
        let dw = i32::from(new_w) - i32::from(old_w);
        let dh = i32::from(new_h) - i32::from(old_h);
        if dw == 0 && dh == 0 {
            return Vec::new();
        }
        let Some(children) = self.windows.get(&parent.0).map(|w| w.children.clone()) else {
            return Vec::new();
        };
        let mut moved = Vec::new();
        for child in children {
            let Some(w) = self.windows.get_mut(&child.0) else {
                continue;
            };
            let (ddx, ddy) = win_gravity_delta(w.win_gravity, dw, dh);
            if ddx == 0 && ddy == 0 {
                continue;
            }
            let nx = (i32::from(w.x) + ddx).clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
            let ny = (i32::from(w.y) + ddy).clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
            if nx == w.x && ny == w.y {
                continue;
            }
            w.x = nx;
            w.y = ny;
            moved.push((child, nx, ny));
        }
        moved
    }

    /// Diagnostic: ROOT's children top-to-bottom (front-to-back) as hex
    /// ids. Used by the restack tracer to show how the click hit-test
    /// stacking order evolves vs. the compositor's visual order.
    #[must_use]
    pub(super) fn debug_root_order(&self) -> String {
        self.windows.get(&ROOT_WINDOW.0).map_or_else(
            || "<no root>".to_string(),
            |root| {
                root.children
                    .iter()
                    .rev()
                    .map(|c| format!("0x{:x}", c.0))
                    .collect::<Vec<_>>()
                    .join(" ")
            },
        )
    }

    pub(super) fn restack_window(
        &mut self,
        window_id: ResourceId,
        sibling: Option<ResourceId>,
        stack_mode: Option<u8>,
    ) {
        if stack_mode.is_some()
            && log::log_enabled!(target: "yserver::input::restack", log::Level::Trace)
        {
            let parent = self.windows.get(&window_id.0).map_or(0, |w| w.parent.0);
            log::trace!(
                target: "yserver::input::restack",
                "RESTACK win=0x{:x} parent=0x{parent:x} sibling={:?} stack_mode={:?} | root before: [{}]",
                window_id.0,
                sibling.map(|s| s.0),
                stack_mode,
                self.debug_root_order(),
            );
        }
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
        if log::log_enabled!(target: "yserver::input::restack", log::Level::Trace) {
            log::trace!(
                target: "yserver::input::restack",
                "RESTACK applied {action:?} | root after: [{}]",
                self.debug_root_order(),
            );
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

    pub fn map_window(&mut self, id: ResourceId) -> MapTransition {
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
        let mut delta = ViewabilityDelta::default();
        let was_unmapped = if let Some(window) = self.windows.get_mut(&id.0) {
            let was_unmapped = window.map_state == MapState::Unmapped;
            let was_viewable = window.map_state == MapState::Viewable;
            window.map_state = if parent_viewable {
                MapState::Viewable
            } else {
                MapState::Unviewable
            };
            if !was_viewable && parent_viewable {
                delta.became_viewable.push(id);
            } else if was_viewable && !parent_viewable {
                delta.became_unviewable.push(id);
            }
            was_unmapped
        } else {
            return MapTransition::default();
        };
        // If we just transitioned to Viewable, promote any descendant
        // that was Unviewable (i.e. mapped before its ancestor became
        // viewable — e.g. xclock's child window after the WM frame
        // reparent: child was MapSubwindows'd while parent was unmapped,
        // staying Unviewable; mapping the parent must propagate down or
        // Expose-fanout silently skips it because of the Viewable filter).
        if parent_viewable {
            self.promote_unviewable_descendants(id, &mut delta.became_viewable);
        }
        MapTransition {
            mapping_changed: was_unmapped,
            delta,
        }
    }

    pub(super) fn promote_unviewable_descendants(
        &mut self,
        root: ResourceId,
        promoted_descendants: &mut Vec<ResourceId>,
    ) {
        let children: Vec<ResourceId> = self
            .windows
            .get(&root.0)
            .map(|w| w.children.clone())
            .unwrap_or_default();
        for child in children {
            let promoted = if let Some(w) = self.windows.get_mut(&child.0) {
                if w.map_state == MapState::Unviewable {
                    w.map_state = MapState::Viewable;
                    promoted_descendants.push(child);
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
                self.promote_unviewable_descendants(child, promoted_descendants);
            }
        }
    }

    /// Inverse of [`Self::promote_unviewable_descendants`]: cascade
    /// Viewable → Unviewable down the subtree when an ancestor stops being
    /// viewable (e.g. a mapped window reparented under a non-viewable parent).
    /// Unmapped descendants halt the cascade (they were never viewable).
    pub(super) fn demote_viewable_descendants(
        &mut self,
        root: ResourceId,
        demoted_descendants: &mut Vec<ResourceId>,
    ) {
        let children: Vec<ResourceId> = self
            .windows
            .get(&root.0)
            .map(|w| w.children.clone())
            .unwrap_or_default();
        for child in children {
            let demoted = if let Some(w) = self.windows.get_mut(&child.0) {
                if w.map_state == MapState::Viewable {
                    w.map_state = MapState::Unviewable;
                    true
                } else {
                    false
                }
            } else {
                false
            };
            // Recurse only through descendants that were on-screen (now
            // demoted, or already Unviewable under us); Unmapped subtrees
            // stay Unmapped and halt the cascade. Post-order: child first.
            if demoted {
                self.demote_viewable_descendants(child, demoted_descendants);
                demoted_descendants.push(child);
            }
        }
    }

    pub fn unmap_window(&mut self, id: ResourceId) -> MapTransition {
        if id == ROOT_WINDOW {
            return MapTransition::default();
        }
        let Some(window) = self.windows.get_mut(&id.0) else {
            return MapTransition::default();
        };
        let was_mapped = window.map_state != MapState::Unmapped;
        let was_viewable = window.map_state == MapState::Viewable;
        window.map_state = MapState::Unmapped;
        // Mirror of `promote_unviewable_descendants`: X11 defines
        // Viewable as "mapped AND every ancestor mapped", so unmapping
        // this window demotes every still-mapped descendant to
        // Unviewable. Without this the asymmetry is protocol-visible —
        // `GetWindowAttributes` on a descendant reports IsViewable where
        // Xorg reports IsUnviewable — and it was the root cause of
        // issue #97: i3 unmaps only its frame on a workspace switch, the
        // terminal reparented inside stays mapped and keeps drawing, and
        // every viewability-gated path (damage in particular) treated
        // that subtree as on-screen, so the compositor kept compositing
        // a window that had left the workspace.
        // X11 sends NO UnmapNotify for descendants of an unmapped window;
        // the demoted list is the viewability delta, not an event fanout.
        let mut delta = ViewabilityDelta::default();
        self.demote_viewable_descendants(id, &mut delta.became_unviewable);
        if was_viewable {
            delta.became_unviewable.push(id);
        }
        MapTransition {
            mapping_changed: was_mapped,
            delta,
        }
    }
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
pub(super) fn window_bounding_box(w: &Window) -> (i32, i32, i32, i32) {
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
    /// Border source; CreateWindow inherits the parent's
    /// (`dix/window.c:879`), `CWBorderPixmap`/`CWBorderPixel` replace it.
    pub border: BorderSource,
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
    /// Host cursor `cursor` named when it was set. The window holds it
    /// (Xorg `pCurs->refcnt`, `dix/window.c:1536`) after its XID is freed,
    /// so a freed cursor stays this window's until the window drops it.
    pub cursor_host: Option<crate::backend::CursorHandle>,
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

impl Window {
    /// `border_width` as a signed coordinate delta. X11 carries it as a
    /// CARD16, but every coordinate it participates in is INT16, so the
    /// conversion belongs in exactly one place. The saturation is a
    /// lint-clean total conversion, not a meaningful bound: a border
    /// wider than 32767 cannot be drawn on any real screen.
    #[must_use]
    pub fn border_delta(&self) -> i16 {
        i16::try_from(self.border_width).unwrap_or(i16::MAX)
    }

    /// Translate a point given in this window's PARENT's content space
    /// into this window's own CONTENT space (#133 step 8, P9).
    ///
    /// `x`/`y` locate the window's OUTER upper-left corner relative to
    /// the parent's content origin, so the window's own origin — where
    /// its drawing surface and its children start — sits `border_width`
    /// further in on both axes. Xorg keeps that sum pre-computed:
    ///
    /// ```c
    /// pWin->drawable.x = pParent->drawable.x + x + (int) bw;
    /// ```
    ///
    /// (`dix/window.c:888`), and `PointInWindowIsVisible`
    /// (`dix/window.c:2987`) expresses both the input shape and the
    /// reported coordinates against `drawable.x` / `drawable.y`:
    ///
    /// ```c
    /// RegionContainsPoint(wInputShape(pWin),
    ///                     x - pWin->drawable.x,
    ///                     y - pWin->drawable.y, &box)
    /// ```
    ///
    /// i.e. the CONTENT origin, not the outer corner. A point on the
    /// left or top border therefore has a NEGATIVE window-relative
    /// coordinate. That is X11-correct: it must be neither rejected nor
    /// clamped, and INT16 event fields carry it faithfully.
    ///
    /// Collapses to `parent - self.x` at `border_width == 0`.
    #[must_use]
    pub fn to_content_coords(&self, parent_x: i16, parent_y: i16) -> (i16, i16) {
        let bw = self.border_delta();
        (
            parent_x.wrapping_sub(self.x).wrapping_sub(bw),
            parent_y.wrapping_sub(self.y).wrapping_sub(bw),
        )
    }

    /// Exact inverse of [`Self::to_content_coords`]: lift a point from
    /// this window's content space up into its parent's content space.
    ///
    /// Used by the event-propagation walks, which climb the ancestry
    /// looking for a subscriber and must report the coordinate relative
    /// to whichever window they stop at. The walk up has to undo
    /// precisely what the hit-test walk down did, border term included —
    /// otherwise an event propagated to an ancestor lands `border_width`
    /// off per level crossed.
    #[must_use]
    pub fn to_parent_coords(&self, x: i16, y: i16) -> (i16, i16) {
        let bw = self.border_delta();
        (
            x.wrapping_add(self.x).wrapping_add(bw),
            y.wrapping_add(self.y).wrapping_add(bw),
        )
    }

    /// Border-inclusive containment test, with `x`/`y` in this window's
    /// CONTENT space (as produced by [`Self::to_content_coords`]).
    ///
    /// Xorg tests the point against `pWin->borderClip`
    /// (`dix/window.c:2993`, inside `PointInWindowIsVisible`), and
    /// `SetBorderSize` builds `borderSize`/`borderClip` from the OUTER
    /// rectangle — border included. Expressed against the content
    /// origin that region is `[-bw, width + bw)` × `[-bw, height + bw)`:
    /// **the border ring belongs to the window**, which is why a WM's
    /// resize grip on the frame border is grabbable at all.
    ///
    /// Collapses to `0 <= x < width && 0 <= y < height` — the exact
    /// pre-#133 test — at `border_width == 0`.
    #[must_use]
    pub fn outer_contains_content_point(&self, x: i16, y: i16) -> bool {
        let bw = self.border_delta();
        let right = i16::try_from(self.width)
            .unwrap_or(i16::MAX)
            .saturating_add(bw);
        let bottom = i16::try_from(self.height)
            .unwrap_or(i16::MAX)
            .saturating_add(bw);
        x >= -bw && y >= -bw && x < right && y < bottom
    }

    pub(super) fn placeholder(id: ResourceId) -> Self {
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
        }
    }
}
