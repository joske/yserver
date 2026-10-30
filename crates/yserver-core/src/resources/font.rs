//! Core fonts with Xorg's lifetime: a font lives while its client id or any
//! GC holds it (`pFont->refcnt`, `dix/dixfonts.c:462`), so `CloseFont` frees
//! only the id and a GC keeps drawing with the font it was given.

use crate::resources::{DEFAULT_FONT, ResourceTable, SERVER_OWNER};
use yserver_protocol::x11::{ClientId, FontMetrics, ResourceId};

/// A reference to an opened font, independent of the client id that named
/// it: the id may be closed and reused for another font while a GC still
/// holds this one.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct FontRef(u64);

#[derive(Clone, Debug)]
pub struct Font {
    /// The id the font was opened under; it may since have been closed.
    pub id: ResourceId,
    pub name: String,
    pub host_xid: crate::backend::FontHandle,
    pub metrics: FontMetrics,
    pub owner: ClientId,
}

#[derive(Debug)]
pub(super) struct FontObject {
    pub(super) font: Font,
    /// The client id (while open), each GC holding it, the default-font slot.
    refs: u32,
}

impl ResourceTable {
    pub fn install_font(
        &mut self,
        owner: ClientId,
        id: ResourceId,
        name: String,
        host_xid: crate::backend::FontHandle,
        metrics: FontMetrics,
    ) {
        let key = FontRef(self.next_font_ref);
        self.next_font_ref += 1;
        let font = Font {
            id,
            name,
            host_xid,
            metrics,
            owner,
        };
        self.font_objects.insert(key, FontObject { font, refs: 1 });
        if let Some(old) = self.fonts.insert(id.0, key) {
            self.unref_font(old);
        }
        // The server keeps its default font for new GCs even once a client
        // closes the id (Xorg keeps `defaultFont` while GCs hold it).
        if owner == SERVER_OWNER && id == DEFAULT_FONT {
            self.set_font_slot_default(Some(key));
        }
    }

    fn set_font_slot_default(&mut self, key: Option<FontRef>) {
        if let Some(new) = key {
            self.ref_font(new);
        }
        if let Some(old) = std::mem::replace(&mut self.default_font, key) {
            self.unref_font(old);
        }
    }

    /// Free the client id `id` (Xorg `ProcCloseFont` → `FreeResource`); the
    /// font itself goes once no GC holds it. False when `id` names no font.
    pub fn close_font(&mut self, id: ResourceId) -> bool {
        let Some(key) = self.fonts.remove(&id.0) else {
            return false;
        };
        self.unref_font(key);
        true
    }

    /// Host fonts whose last reference went since the last call: the caller
    /// closes each on the backend.
    pub fn take_released_fonts(&mut self) -> Vec<u32> {
        std::mem::take(&mut self.released_fonts)
    }

    /// Drop every font, referenced or not (server reset), returning the host
    /// fonts to close.
    pub fn take_all_fonts(&mut self) -> Vec<u32> {
        self.fonts.clear();
        self.default_font = None;
        for gc in self.gcs.values_mut() {
            gc.font = None;
        }
        let mut out = std::mem::take(&mut self.released_fonts);
        out.extend(
            self.font_objects
                .drain()
                .map(|(_, o)| o.font.host_xid.as_raw()),
        );
        out
    }

    pub(super) fn ref_font(&mut self, key: FontRef) {
        if let Some(o) = self.font_objects.get_mut(&key) {
            o.refs += 1;
        }
    }

    pub(super) fn unref_font(&mut self, key: FontRef) {
        let Some(o) = self.font_objects.get_mut(&key) else {
            return;
        };
        o.refs -= 1;
        if o.refs == 0
            && let Some(o) = self.font_objects.remove(&key)
        {
            self.released_fonts.push(o.font.host_xid.as_raw());
        }
    }

    /// The font object the client id `id` names.
    #[must_use]
    pub fn font_ref(&self, id: ResourceId) -> Option<FontRef> {
        self.fonts.get(&id.0).copied()
    }

    /// The font a new GC starts with (Xorg `CreateGC`, `dix/gc.c:507`).
    #[must_use]
    pub fn default_font(&self) -> Option<FontRef> {
        self.default_font
    }

    /// Point GC `gc_id` at `font`, new reference first as Xorg's `ChangeGC`
    /// GCFont does (`dix/gc.c:290`).
    pub(super) fn set_gc_font_ref(&mut self, gc_id: ResourceId, font: Option<FontRef>) {
        let Some(gc) = self.gcs.get_mut(&gc_id.0) else {
            return;
        };
        if gc.font == font {
            return;
        }
        let old = std::mem::replace(&mut gc.font, font);
        if let Some(new) = font {
            self.ref_font(new);
        }
        if let Some(old) = old {
            self.unref_font(old);
        }
    }

    /// Commit a PolyText embedded font-change to the GC (X11 §8:
    /// "the font item ... is stored in the GC").
    pub fn set_gc_font(&mut self, gc_id: ResourceId, font: ResourceId) {
        if let Some(key) = self.font_ref(font) {
            self.set_gc_font_ref(gc_id, Some(key));
        }
    }

    /// The font the client id `id` names, if it is open.
    #[must_use]
    pub fn font(&self, id: ResourceId) -> Option<&Font> {
        self.fonts.get(&id.0).and_then(|key| self.font_object(*key))
    }

    #[must_use]
    pub fn font_object(&self, key: FontRef) -> Option<&Font> {
        self.font_objects.get(&key).map(|o| &o.font)
    }

    /// Resolve a FONTABLE id (either a Font or a GC carrying a font) to a
    /// `&Font` (Xorg `dixLookupFontable`).
    #[must_use]
    pub fn fontable(&self, id: ResourceId) -> Option<&Font> {
        if let Some(font) = self.font(id) {
            return Some(font);
        }
        let key = self.gcs.get(&id.0).and_then(|gc| gc.font)?;
        self.font_object(key)
    }

    /// Open font ids `owner` opened.
    pub(super) fn font_ids_owned_by(&self, owner: ClientId) -> Vec<ResourceId> {
        self.fonts
            .iter()
            .filter(|(_, key)| self.font_object(**key).is_some_and(|f| f.owner == owner))
            .map(|(id, _)| ResourceId(*id))
            .collect()
    }

    /// Fonts alive, with or without a client id.
    #[must_use]
    pub fn live_font_count(&self) -> usize {
        self.font_objects.len()
    }
}
