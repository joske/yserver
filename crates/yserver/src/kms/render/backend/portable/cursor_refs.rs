use crate::kms::render::backend::{WindowGeometry, XBackend, seam::Platform};

// Xorg holds one cursor reference per window attribute (`dix/window.c:968,
// 1536`) and per animation frame (`render/animcur.c:247, 360`); these helpers
// keep the backend's corresponding counts in step with its window projection.
impl<P: Platform> XBackend<P> {
    pub(in crate::kms::render::backend) fn window_geom_insert(
        &mut self,
        host_xid: u32,
        geom: WindowGeometry,
    ) -> Option<WindowGeometry> {
        let new_cursor = geom.cursor;
        let replaced = self.windows.insert(host_xid, geom);
        if let Some(xid) = new_cursor {
            self.cursor_ref_add(xid);
        }
        if let Some(xid) = replaced.as_ref().and_then(|old| old.cursor) {
            self.cursor_ref_drop(xid);
        }
        replaced
    }

    pub(in crate::kms::render::backend) fn window_geom_remove(
        &mut self,
        host_xid: u32,
    ) -> Option<WindowGeometry> {
        let removed = self.windows.remove(&host_xid);
        if let Some(xid) = removed.as_ref().and_then(|geom| geom.cursor) {
            self.cursor_ref_drop(xid);
        }
        removed
    }

    pub(in crate::kms::render::backend) fn window_set_cursor(
        &mut self,
        host_xid: u32,
        cursor: Option<u32>,
    ) -> bool {
        let Some(geom) = self.windows.get_mut(&host_xid) else {
            return false;
        };
        let old_cursor = geom.cursor;
        if old_cursor == cursor {
            return true;
        }
        geom.cursor = cursor;
        if let Some(xid) = cursor {
            self.cursor_ref_add(xid);
        }
        if let Some(xid) = old_cursor {
            self.cursor_ref_drop(xid);
        }
        true
    }

    pub(in crate::kms::render::backend) fn cursor_ref_add(&mut self, xid: u32) {
        *self.cursor_refs.entry(xid).or_insert(0) += 1;
    }

    pub(in crate::kms::render::backend) fn cursor_ref_drop(&mut self, xid: u32) {
        let reaches_zero = if let Some(count) = self.cursor_refs.get_mut(&xid) {
            *count -= 1;
            *count == 0
        } else {
            false
        };
        if reaches_zero {
            self.cursor_refs.remove(&xid);
            self.reclaim_pending.push(xid);
        }
    }

    pub(in crate::kms::render::backend) fn cursor_release_candidate(&mut self, xid: u32) {
        self.reclaim_pending.push(xid);
    }
}
