//! Retained front-buffer overlay for legacy root-window `IncludeInferiors`
//! XOR/invert drawing (import rubber-band, WM wireframes). See
//! docs/superpowers/specs/2026-07-14-root-includeinferiors-overlay-design.md.
//!
//! The retained state here is captured by the backend paint path
//! (`capture_root_overlay`) and applied as a final XOR pass at the end
//! of each compose by `SceneCompositor::tick` → `tick_one_output` →
//! `record_command_buffer` (via `apply_list_for_output`).

use ash::vk;
use std::collections::HashMap;
use yserver_protocol::x11::ClientId;

/// Max retained rects before we give up and clear (safe degradation — never
/// bbox-collapse an active XOR overlay: that changes visible pixels and breaks
/// exact-match erase symmetry). Real outlines are a handful of thin rects.
const MAX_OVERLAY_RECTS: usize = 4096;

/// What one [`RootOverlay::toggle`] did. `changed == false` means the batch was
/// empty and nothing needs damaging.
#[derive(Default, Clone, Copy, Debug)]
pub(crate) struct ToggleOutcome {
    pub(crate) changed: bool,
    pub(crate) removed: usize,
    pub(crate) inserted: usize,
    pub(crate) total: usize,
}

#[derive(Default)]
pub(crate) struct RootOverlay {
    /// xor_value -> active rects toggled by that value (root-absolute coords).
    xor_ops: HashMap<u32, Vec<vk::Rect2D>>,
    owner_clients: std::collections::HashSet<ClientId>,
}

impl RootOverlay {
    pub(crate) fn is_empty(&self) -> bool {
        self.xor_ops.is_empty()
    }

    /// Toggle a batch of rects for one xor_value by EXACT match (present ->
    /// remove, absent -> insert). Records the owner. Returns true if state
    /// changed, plus how many rects were removed and inserted — an erase that
    /// does not match what was drawn INSERTS instead, and two copies of a rect
    /// XOR to identity, so fresh composes look clean while already-inverted
    /// pixels stay stale on screen. That is indistinguishable from lost damage
    /// by eye, so the counts are reported for the caller to log.
    pub(crate) fn toggle(
        &mut self,
        client: ClientId,
        value: u32,
        rects: &[vk::Rect2D],
    ) -> ToggleOutcome {
        if rects.is_empty() {
            return ToggleOutcome::default();
        }
        self.owner_clients.insert(client);
        let mut removed = 0usize;
        let mut inserted = 0usize;
        let list = self.xor_ops.entry(value).or_default();
        for r in rects {
            if let Some(pos) = list.iter().position(|e| rect_eq(e, r)) {
                let _ = list.swap_remove(pos);
                removed += 1;
            } else {
                list.push(*r);
                inserted += 1;
            }
        }
        if list.is_empty() {
            self.xor_ops.remove(&value);
        }
        if self.total_rects() > MAX_OVERLAY_RECTS {
            log::warn!(
                "root overlay exceeded {MAX_OVERLAY_RECTS} rects; clearing (misbehaving client)"
            );
            self.clear();
        }
        ToggleOutcome {
            changed: true,
            removed,
            inserted,
            total: self.total_rects(),
        }
    }

    fn total_rects(&self) -> usize {
        self.xor_ops.values().map(Vec::len).sum()
    }

    /// Clear the whole overlay (RandR/topology change, cap overflow).
    pub(crate) fn clear(&mut self) {
        self.xor_ops.clear();
        self.owner_clients.clear();
    }

    /// Drop one client's contribution on disconnect. Phase-1 simplification:
    /// if the disconnecting client was an owner, clear the whole overlay.
    pub(crate) fn on_client_disconnect(&mut self, client: ClientId) -> bool {
        if self.owner_clients.contains(&client) {
            self.clear();
            true
        } else {
            false
        }
    }

    /// All root-absolute rects across every value (for damage injection).
    pub(crate) fn all_rects(&self) -> Vec<vk::Rect2D> {
        self.xor_ops.values().flatten().copied().collect()
    }

    /// Per-output apply list: for each (value, rect) intersecting `output`
    /// (root-absolute x,y,w,h), the output-LOCAL rect and its xor value.
    pub(crate) fn apply_list_for_output(
        &self,
        output: (i32, i32, u32, u32),
    ) -> Vec<(u32, vk::Rect2D)> {
        let (ox, oy, ow, oh) = output;
        let mut out = Vec::new();
        for (value, rects) in &self.xor_ops {
            for r in rects {
                if let Some(local) = intersect_to_local(*r, ox, oy, ow, oh) {
                    out.push((*value, local));
                }
            }
        }
        out
    }
}

fn rect_eq(a: &vk::Rect2D, b: &vk::Rect2D) -> bool {
    a.offset.x == b.offset.x
        && a.offset.y == b.offset.y
        && a.extent.width == b.extent.width
        && a.extent.height == b.extent.height
}

/// Intersect a root-absolute rect with an output rect; return the intersection
/// in output-LOCAL coords, or None if disjoint.
fn intersect_to_local(r: vk::Rect2D, ox: i32, oy: i32, ow: u32, oh: u32) -> Option<vk::Rect2D> {
    let rx0 = r.offset.x;
    let ry0 = r.offset.y;
    let rx1 = rx0 + r.extent.width as i32;
    let ry1 = ry0 + r.extent.height as i32;
    let ix0 = rx0.max(ox);
    let iy0 = ry0.max(oy);
    let ix1 = rx1.min(ox + ow as i32);
    let iy1 = ry1.min(oy + oh as i32);
    if ix0 >= ix1 || iy0 >= iy1 {
        return None;
    }
    Some(vk::Rect2D {
        offset: vk::Offset2D {
            x: ix0 - ox,
            y: iy0 - oy,
        },
        extent: vk::Extent2D {
            width: (ix1 - ix0) as u32,
            height: (iy1 - iy0) as u32,
        },
    })
}

/// Normalize a reversible GC function + foreground + depth-plane-mask to the
/// per-pixel XOR value. GXinvert ignores fg (`dst = ~dst = dst ^ plane_mask`);
/// GXxor uses `dst ^= fg`. Returns None for non-reversible functions.
pub(crate) fn xor_value_for(
    function: yserver_core::backend::GcFunction,
    foreground: u32,
    plane_mask: u32,
) -> Option<u32> {
    use yserver_core::backend::GcFunction;
    match function {
        GcFunction::Invert => Some(plane_mask),
        GcFunction::Xor => Some(foreground & plane_mask),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: i32, y: i32, w: u32, h: u32) -> vk::Rect2D {
        vk::Rect2D {
            offset: vk::Offset2D { x, y },
            extent: vk::Extent2D {
                width: w,
                height: h,
            },
        }
    }
    const C: ClientId = ClientId(7);

    #[test]
    fn toggle_draw_then_identical_erase_is_empty() {
        let mut o = RootOverlay::default();
        o.toggle(C, 0xffffff, &[r(10, 10, 100, 1), r(10, 10, 1, 80)]);
        assert!(!o.is_empty());
        o.toggle(C, 0xffffff, &[r(10, 10, 100, 1), r(10, 10, 1, 80)]);
        assert!(o.is_empty(), "identical erase cancels draw");
    }

    #[test]
    fn toggle_erase_old_draw_new_nets_new() {
        let mut o = RootOverlay::default();
        let old = r(10, 10, 100, 1);
        let new = r(10, 10, 120, 1);
        o.toggle(C, 0xffffff, &[old]);
        o.toggle(C, 0xffffff, &[old, new]);
        let rects = o.all_rects();
        assert_eq!(rects, vec![new], "net is the new rect only");
    }

    #[test]
    fn xor_value_normalization() {
        use yserver_core::backend::GcFunction;
        assert_eq!(
            xor_value_for(GcFunction::Invert, 0x123456, 0xffffff),
            Some(0xffffff)
        );
        assert_eq!(
            xor_value_for(GcFunction::Xor, 0x12345678, 0xffffff),
            Some(0x345678)
        );
        assert_eq!(xor_value_for(GcFunction::Copy, 0xffffff, 0xffffff), None);
    }

    #[test]
    fn apply_list_splits_per_output_to_local() {
        let mut o = RootOverlay::default();
        o.toggle(C, 0xffffff, &[r(2500, 100, 200, 2)]);
        let left = o.apply_list_for_output((0, 0, 2560, 1440));
        let right = o.apply_list_for_output((2560, 0, 2560, 1440));
        assert_eq!(left, vec![(0xffffff, r(2500, 100, 60, 2))]);
        assert_eq!(right, vec![(0xffffff, r(0, 100, 140, 2))]);
    }

    #[test]
    fn disconnect_owner_clears() {
        let mut o = RootOverlay::default();
        o.toggle(C, 0xffffff, &[r(0, 0, 10, 10)]);
        assert!(!o.on_client_disconnect(ClientId(99)));
        assert!(!o.is_empty());
        assert!(o.on_client_disconnect(C));
        assert!(o.is_empty());
    }

    #[test]
    fn cap_overflow_clears() {
        let mut o = RootOverlay::default();
        let many: Vec<_> = (0..MAX_OVERLAY_RECTS as i32 + 10)
            .map(|i| r(i, 0, 1, 1))
            .collect();
        o.toggle(C, 0xffffff, &many);
        assert!(o.is_empty(), "overflow clears rather than bbox-collapsing");
    }
}
