use super::*;

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
    /// `None` when the backend could not back an otherwise valid Picture: it still
    /// exists at protocol level, and every op on it is a no-op.
    pub host_picture_xid: Option<crate::backend::PictureHandle>,
    pub host_owned_pixmap: Option<crate::backend::PixmapHandle>,
    pub kind: PictureKind,
    /// For `PictureKind::Drawable` pictures: the client-visible XID of
    /// the backing window or pixmap. Used by RENDER paint handlers to
    /// accumulate damage on the right drawable after painting.
    /// `None` for `Sourceless` pictures (SolidFill / gradient).
    pub drawable: Option<ResourceId>,
    /// The window this Picture was created on (`None` for pixmap and
    /// sourceless Pictures). Destroying that window frees the Picture,
    /// whichever client owns it (Xorg `PictureDestroyWindow`,
    /// `render/picture.c:67`).
    pub window: Option<ResourceId>,
}

#[derive(Debug)]
pub struct GlyphSetState {
    pub client: ClientId,
    pub host_glyphset_xid: crate::backend::GlyphSetHandle,
}

impl ResourceTable {
    pub fn create_picture(&mut self, id: ResourceId, state: PictureState) {
        self.pictures.insert(id.0, state);
    }

    pub fn free_picture(&mut self, id: ResourceId) -> Option<PictureState> {
        self.pictures.remove(&id.0)
    }

    pub fn picture(&self, id: ResourceId) -> Option<&PictureState> {
        self.pictures.get(&id.0)
    }

    /// Remove every Picture, of any client, created on one of `windows`.
    /// Returns `(host picture, host-owned pixmap)` for the backend frees.
    pub fn remove_pictures_on_windows(
        &mut self,
        windows: &[ResourceId],
    ) -> Vec<(u32, Option<u32>)> {
        self.pictures
            .extract_if(|_, p| p.window.is_some_and(|w| windows.contains(&w)))
            .filter_map(|(_, p)| {
                Some((
                    p.host_picture_xid?.as_raw(),
                    p.host_owned_pixmap.map(|h| h.as_raw()),
                ))
            })
            .collect()
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

    pub(super) fn release_host_glyphset_ref(&mut self, host_xid: u32) -> bool {
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

    /// XFIXES `SetCursorName`. The name belongs to the cursor object, so
    /// every XID aliasing the same host cursor (after `ChangeCursor`) sees
    /// it, and it stays recorded against the host handle.
    pub fn set_cursor_name_atom(&mut self, id: ResourceId, atom: yserver_protocol::x11::AtomId) {
        let Some(cursor) = self.cursors.get_mut(&id.0) else {
            return;
        };
        cursor.name_atom = Some(atom);
        let Some(host) = cursor.host_xid.map(|h| h.as_raw()) else {
            return;
        };
        for c in self.cursors.values_mut() {
            if c.host_xid.map(|h| h.as_raw()) == Some(host) {
                c.name_atom = Some(atom);
            }
        }
        self.cursor_host_names.insert(host, atom);
    }

    #[must_use]
    pub fn cursor_name_atom(&self, id: ResourceId) -> Option<yserver_protocol::x11::AtomId> {
        self.cursors.get(&id.0)?.name_atom
    }

    /// XFIXES name of the cursor behind host handle `host`, whether or not
    /// a client XID still refers to it. `None` when it was never named.
    #[must_use]
    pub fn cursor_name_for_host(&self, host: u32) -> Option<yserver_protocol::x11::AtomId> {
        self.cursor_host_names.get(&host).copied()
    }

    /// Host handles of every cursor named `atom` (XFIXES
    /// `ChangeCursorByName`'s match set), sorted for a stable order.
    #[must_use]
    pub fn cursor_hosts_named(&self, atom: yserver_protocol::x11::AtomId) -> Vec<u32> {
        let mut hosts: Vec<u32> = self
            .cursor_host_names
            .iter()
            .filter_map(|(host, name)| (*name == atom).then_some(*host))
            .collect();
        hosts.sort_unstable();
        hosts
    }

    /// XFIXES `ChangeCursor` on the resource database (Xorg
    /// `ReplaceCursorLookup` → `ChangeResourceValue`): every cursor XID
    /// that refers to host cursor `old_host` now refers to `source`'s
    /// cursor object — its handle, name and animation flag. Returns
    /// whether any XID was retargeted. The old host handle's name is
    /// dropped: nothing displays it after the backend's replace.
    pub fn retarget_cursor_host(&mut self, old_host: u32, source: ResourceId) -> bool {
        let Some(src) = self.cursors.get(&source.0).cloned() else {
            return false;
        };
        let mut changed = false;
        for c in self.cursors.values_mut() {
            if c.host_xid.map(|h| h.as_raw()) == Some(old_host) {
                c.host_xid = src.host_xid;
                c.name_atom = src.name_atom;
                c.anim = src.anim;
                changed = true;
            }
        }
        self.cursor_host_names.remove(&old_host);
        for w in self.windows.values_mut() {
            if w.cursor_host.map(|h| h.as_raw()) == Some(old_host) {
                w.cursor_host = src.host_xid;
            }
        }
        changed
    }

    /// True iff some cursor XID or some window's cursor attribute still
    /// refers to host cursor `host` (Xorg `pCurs->refcnt`).
    #[must_use]
    pub fn cursor_host_referenced(&self, host: u32) -> bool {
        self.cursors
            .values()
            .any(|c| c.host_xid.map(|h| h.as_raw()) == Some(host))
            || self
                .windows
                .values()
                .any(|w| w.cursor_host.map(|h| h.as_raw()) == Some(host))
    }

    /// Host cursors a window dropped since the last call that nothing
    /// references any more: the caller frees each on the backend, once.
    pub fn take_unreferenced_cursor_hosts(&mut self) -> Vec<u32> {
        let mut hosts = std::mem::take(&mut self.dropped_cursor_hosts);
        hosts.sort_unstable();
        hosts.dedup();
        hosts.retain(|&host| !self.cursor_host_referenced(host));
        hosts
    }

    /// Remove a cursor from the table and return its host XID when this
    /// was the last XID referring to that host cursor, so the caller can
    /// free it on the host. After XFIXES `ChangeCursor` several XIDs share
    /// one host cursor; freeing it while another XID still names it would
    /// release the host object under that XID. Caller's responsibility to
    /// dispatch `backend.free_cursor` — keeping the resource layer
    /// backend-agnostic.
    pub fn free_cursor(&mut self, id: ResourceId) -> Option<u32> {
        let host_xid = self.cursors.get(&id.0).and_then(|c| c.host_xid);
        self.cursors.remove(&id.0);
        let freed = host_xid
            .map(|h| h.as_raw())
            .filter(|host| !self.cursor_host_referenced(*host));
        if let Some(host) = freed {
            self.cursor_host_released(host);
        }
        freed
    }

    /// The caller frees host cursor `host` itself: no pending window drop
    /// may free it again.
    pub fn cursor_host_released(&mut self, host: u32) {
        self.dropped_cursor_hosts.retain(|&h| h != host);
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
