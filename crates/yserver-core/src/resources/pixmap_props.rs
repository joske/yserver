use super::*;

impl ResourceTable {
    pub fn pixmaps_iter(&self) -> impl Iterator<Item = &Pixmap> {
        self.pixmaps.values()
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
                composite_name: false,
            },
        );
    }

    /// Mark `id` as a `NameWindowPixmap` name: it owns one alias ref on its host backing.
    pub fn mark_pixmap_composite_name(&mut self, id: ResourceId) {
        if let Some(p) = self.pixmaps.get_mut(&id.0) {
            p.composite_name = true;
        }
    }

    pub fn free_pixmap(&mut self, id: ResourceId) -> Option<Pixmap> {
        let removed = self.pixmaps.remove(&id.0)?;
        if removed.composite_name {
            self.forget_composite_names(&[id]);
        }
        Some(removed)
    }

    /// Drop freed names from their windows' alias lists, so a resize never retargets a dead name.
    pub(super) fn forget_composite_names(&mut self, names: &[ResourceId]) {
        for w in self.windows.values_mut() {
            w.composite_named_pixmaps
                .retain(|alias| !names.contains(&alias.client_pixmap));
        }
    }

    /// True iff a live `NameWindowPixmap` name still aliases `host_xid`.
    #[must_use]
    pub fn host_xid_named_by_pixmap(&self, host_xid: crate::backend::PixmapHandle) -> bool {
        self.pixmaps
            .values()
            .any(|p| p.composite_name && p.host_xid == Some(host_xid))
    }

    /// True iff a freed name's alias ref on `host_xid` is waiting for an attribute release site.
    #[must_use]
    pub fn has_deferred_name_ref(&self, host_xid: crate::backend::PixmapHandle) -> bool {
        self.deferred_name_refs.contains(&host_xid.as_raw())
    }

    /// Record that a freed name's alias ref on `host_xid` is left to the attribute release sites.
    pub fn defer_name_ref(&mut self, host_xid: crate::backend::PixmapHandle) {
        self.deferred_name_refs.insert(host_xid.as_raw());
    }

    /// An orphan-rule site freed `host_xid`: any deferred name ref on it is now dropped.
    pub fn host_pixmap_freed(&mut self, host_xid: u32) {
        self.deferred_name_refs.remove(&host_xid);
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

    pub fn update_pixmap_geometry(
        &mut self,
        id: ResourceId,
        width: u16,
        height: u16,
        depth: u8,
    ) -> bool {
        if let Some(pixmap) = self.pixmaps.get_mut(&id.0) {
            pixmap.width = width;
            pixmap.height = height;
            pixmap.depth = depth;
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
            //
            // #133 — that shift is the distance between the two
            // windows' CONTENT origins, so each level contributes
            // `x + border_width`: `x` is this window's OUTER origin
            // relative to the parent's content origin
            // (`dix/window.c`: `drawable.x = parent->drawable.x + x +
            // bw`). Xorg takes the same difference directly, in screen
            // coordinates: `tile_x_off = pWin->drawable.x -
            // drawable->x` after walking up the ParentRelative chain
            // (`mi/miexpose.c:424-431`, `miPaintWindow`/PW_BACKGROUND).
            //
            // Omitting `bw` misaligns a bordered ParentRelative child's
            // background tile by exactly its border width — the xts
            // `Xlib4/XSetWindowBackgroundPixmap` purpose-2 report "Bad
            // pixel in tiled area at (0, 0)", which predates #133.
            // Identity at `bw == 0`.
            let mut resolved = self.window_resolved_background_inner(window.parent, seen)?;
            resolved.tile_origin_offset.0 += i32::from(window.x) + i32::from(window.border_width);
            resolved.tile_origin_offset.1 += i32::from(window.y) + i32::from(window.border_width);
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

    /// Returns true if any window currently uses `host_xid` as its border
    /// source. Same retention rule as the background variant: a border
    /// pixmap survives `FreePixmap` of the source resource.
    pub fn host_xid_referenced_by_window_border(
        &self,
        host_xid: crate::backend::PixmapHandle,
    ) -> bool {
        self.windows.values().any(
            |w| matches!(w.border, BorderSource::Pixmap { host_xid: Some(h), .. } if h == host_xid),
        )
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

    /// The single orphan rule: is `host_xid` still reachable from ANY live
    /// reference? A window background, a window border, a GC's clip mask /
    /// tile / stipple, or a pixmap resource that still owns it.
    ///
    /// Every host-side `free_pixmap` decision must go through this, because
    /// the four sites that make that decision — `FreePixmap`,
    /// `ChangeWindowAttributes` replacing an attribute, `DestroyWindow`
    /// tearing down a subtree, and client disconnect — each carried their OWN
    /// subset of the checks, and every omission is either a use-after-free or
    /// a leak. The border reference was missing from three of the four
    /// (#133), the GC reference from two, and the disconnect path checked
    /// nothing at all — which frees a tile another client is still bordering
    /// with. Add new reference kinds HERE, not at a call site.
    #[must_use]
    pub fn host_xid_still_referenced(&self, host_xid: crate::backend::PixmapHandle) -> bool {
        self.host_xid_referenced_by_window_bg(host_xid)
            || self.host_xid_referenced_by_window_border(host_xid)
            || self.host_xid_referenced_by_gc(host_xid)
            || self.host_xid_owned_by_pixmap(host_xid)
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

#[derive(Clone, Debug)]
pub struct Pixmap {
    pub id: ResourceId,
    pub drawable: ResourceId,
    pub width: u16,
    pub height: u16,
    pub depth: u8,
    pub owner: ClientId,
    pub host_xid: Option<crate::backend::PixmapHandle>,
    /// A `NameWindowPixmap` name: owns one alias ref on its backing (Xorg `compext.c:260`).
    pub composite_name: bool,
}
