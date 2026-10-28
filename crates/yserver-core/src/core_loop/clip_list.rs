//! Xorg's per-window clip regions, computed from the window tree on demand:
//! the clip list (what of a window shows, its children out) and the
//! universe its children are cut from (`NotClippedByChildren`), as
//! `miComputeClips` keeps them (`mi/mivaltree.c:194-470`). Graphics
//! exposures (`miHandleExposures`) and the Expose events of a map or an
//! unmap (`miHandleValidateExposures`) are made of them.
//!
//! Regions are in a window's content space and in pixman's canonical form
//! ([`canonical`]), so the rects a client is sent match Xorg's one for one.

use std::collections::{HashMap, HashSet};

use yserver_protocol::x11::{ResourceId, xfixes::RegionRect};

use crate::{
    region::Region,
    resources::{MapState, ROOT_WINDOW, WindowClass},
    server::{CompositeRedirectMode, ServerState},
};

/// Xorg's `RECTLIMIT` (`mi/miexpose.c:107`): past this many rects an
/// exposure is sent as its extents.
pub(crate) const RECTLIMIT: usize = 25;

/// pixman's canonical form of the union of `rects`: y-x banded, each band
/// split only where the set of x spans changes, adjacent bands with the
/// same spans merged. Unique for a region, so two servers that agree on
/// the region agree on the rects.
pub(crate) fn canonical(rects: Vec<RegionRect>) -> Vec<RegionRect> {
    let banded = crate::nested::normalize_region_rects(rects);
    let mut out: Vec<RegionRect> = Vec::with_capacity(banded.len());
    // The previous band: its index range in `out`.
    let mut prev: Option<(usize, usize)> = None;
    let mut i = 0;
    while i < banded.len() {
        let y = banded[i].y;
        let mut j = i;
        while j < banded.len() && banded[j].y == y {
            j += 1;
        }
        let band = &banded[i..j];
        let merged = prev.is_some_and(|(s, e)| {
            let p = &out[s..e];
            i32::from(p[0].y) + i32::from(p[0].height) == i32::from(y)
                && p.len() == band.len()
                && p.iter()
                    .zip(band)
                    .all(|(a, b)| a.x == b.x && a.width == b.width)
        });
        if merged {
            let (s, e) = prev.expect("merged implies a previous band");
            for r in &mut out[s..e] {
                r.height = r.height.saturating_add(band[0].height);
            }
        } else {
            let s = out.len();
            out.extend_from_slice(band);
            prev = Some((s, out.len()));
        }
        i = j;
    }
    out
}

/// `a ∩ b`, canonical.
pub(crate) fn intersect(a: &[RegionRect], b: &[RegionRect]) -> Vec<RegionRect> {
    Region::from_rects(a)
        .intersect(&Region::from_rects(b))
        .to_rects()
}

/// `a − b`, canonical.
pub(crate) fn subtract(a: &[RegionRect], b: &[RegionRect]) -> Vec<RegionRect> {
    Region::from_rects(a)
        .subtract(&Region::from_rects(b))
        .to_rects()
}

/// `rects` moved by `(dx, dy)`, saturating at the wire's range.
pub(crate) fn translate(mut rects: Vec<RegionRect>, dx: i32, dy: i32) -> Vec<RegionRect> {
    for r in &mut rects {
        r.x = (i32::from(r.x) + dx).clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
        r.y = (i32::from(r.y) + dy).clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
    }
    rects
}

/// Whether `rect` lies wholly inside `region` (`RegionContainsRect == rgnIN`).
pub(crate) fn contains(region: &[RegionRect], rect: RegionRect) -> bool {
    rect.width == 0
        || rect.height == 0
        || Region::from_rects(&[rect])
            .subtract(&Region::from_rects(region))
            .is_empty()
}

/// A viewable `InputOutput` window: what takes part in clipping. An
/// `InputOnly` window is never viewable (`RealizeTree`, `dix/window.c`).
fn viewable(state: &ServerState, w: ResourceId) -> bool {
    w == ROOT_WINDOW
        || state.resources.window(w).is_some_and(|win| {
            win.map_state == MapState::Viewable && win.class == WindowClass::InputOutput
        })
}

/// A Manual-redirected window clips neither its siblings nor its parent
/// (`TreatAsTransparent`, `mi/mivaltree.c:171`).
fn transparent(state: &ServerState, w: ResourceId) -> bool {
    state.composite_redirects.window_mode(w) == Some(CompositeRedirectMode::Manual)
}

/// Xorg's `borderSize` of `w` in its parent's content space: its outer
/// rect, cut to its bounding shape (`SetBorderSize`, `dix/window.c:1747`).
pub(crate) fn border_size_in_parent(state: &ServerState, w: ResourceId) -> Vec<RegionRect> {
    border_size(state, w).to_rects()
}

fn border_size(state: &ServerState, w: ResourceId) -> Region {
    let Some(win) = state.resources.window(w) else {
        return Region::default();
    };
    let bw2 = win.border_width.saturating_mul(2);
    let outer = Region::rect(RegionRect {
        x: win.x,
        y: win.y,
        width: win.width.saturating_add(bw2),
        height: win.height.saturating_add(bw2),
    });
    match state
        .shape_windows
        .get(&w)
        .and_then(|s| s.bounding_region())
    {
        Some(shape) => {
            let bw = i32::from(win.border_width);
            let shape = shape
                .clone()
                .translate(i32::from(win.x) + bw, i32::from(win.y) + bw);
            outer.intersect(&shape)
        }
        None => outer,
    }
}

/// Xorg's `winSize` of `w` in its own content space, short of its
/// parent's: its content rect cut to its bounding and clip shapes
/// (`SetWinSize`, `dix/window.c:1710`).
fn win_size(state: &ServerState, w: ResourceId) -> Region {
    let Some(win) = state.resources.window(w) else {
        return Region::default();
    };
    let mut region = Region::rect(RegionRect {
        x: 0,
        y: 0,
        width: win.width,
        height: win.height,
    });
    if let Some(shape) = state.shape_windows.get(&w) {
        for s in [shape.bounding_region(), shape.clip_region()]
            .into_iter()
            .flatten()
        {
            region = region.intersect(s);
        }
    }
    region
}

/// `w`'s offset from its parent's content origin to its own: its position
/// plus its border.
fn content_offset(state: &ServerState, w: ResourceId) -> Option<(i32, i32, i32)> {
    state.resources.window(w).map(|c| {
        let bw = i32::from(c.border_width);
        (i32::from(c.x) + bw, i32::from(c.y) + bw, bw)
    })
}

/// What of `w` shows, its inferiors included, in its content space:
/// Xorg's `borderClip ∩ winSize`, which `NotClippedByChildren` returns and
/// its children's universes are cut from. A redirected window's is its own
/// shape, not clipped by its parent or the screen (`mi/mivaltree.c:233-
/// 239`); every other window's is its parent's less the windows stacked
/// above it there.
pub(crate) fn not_clipped_by_children(state: &ServerState, w: ResourceId) -> Vec<RegionRect> {
    universe(state, w).to_rects()
}

fn universe(state: &ServerState, w: ResourceId) -> Region {
    if !viewable(state, w) {
        return Region::default();
    }
    if w == ROOT_WINDOW {
        return state
            .resources
            .window(ROOT_WINDOW)
            .map_or_else(Region::default, |r| {
                Region::rect(RegionRect {
                    x: 0,
                    y: 0,
                    width: r.width,
                    height: r.height,
                })
            });
    }
    if state.composite_redirects.window_mode(w).is_some() {
        return win_size(state, w);
    }
    let Some(parent) = state.resources.window(w).map(|win| win.parent) else {
        return Region::default();
    };
    let universe = universe(state, parent);
    child_universe(state, &universe, parent, w, &[])
}

/// [`not_clipped_by_children`] of `w` while the `still_redirected` windows,
/// Manual redirects not yet released, stay transparent: the clip of
/// `compRestoreWindow` when `compFreeClientSubwindows` unredirects one
/// child at a time (`composite/compalloc.c:450-452`).
pub(crate) fn not_clipped_by_children_while(
    state: &ServerState,
    w: ResourceId,
    still_redirected: &[ResourceId],
) -> Vec<RegionRect> {
    let Some(parent) = state.resources.window(w).map(|win| win.parent) else {
        return Vec::new();
    };
    if w == ROOT_WINDOW || state.composite_redirects.window_mode(w).is_some() {
        return not_clipped_by_children(state, w);
    }
    let universe = universe(state, parent);
    child_universe(state, &universe, parent, w, still_redirected).to_rects()
}

/// The universe of the single `child` from its parent's: the parent's less
/// `child`'s higher siblings, one subtraction per sibling. A whole layer
/// goes through [`split_universe`] instead.
fn child_universe(
    state: &ServerState,
    parent_universe: &Region,
    parent: ResourceId,
    child: ResourceId,
    also_transparent: &[ResourceId],
) -> Region {
    if !viewable(state, child) {
        return Region::default();
    }
    if state.composite_redirects.window_mode(child).is_some() {
        return win_size(state, child);
    }
    if parent_universe.is_empty() {
        return Region::default();
    }
    uncovered_in_child_space(state, parent_universe, parent, child, also_transparent)
        .intersect(&win_size(state, child))
}

/// `parent_universe` less `child`'s higher siblings, in `child`'s content
/// space: what its `borderSize` is cut to (`miComputeClips`).
fn uncovered_in_child_space(
    state: &ServerState,
    parent_universe: &Region,
    parent: ResourceId,
    child: ResourceId,
    also_transparent: &[ResourceId],
) -> Region {
    let Some((dx, dy, _)) = content_offset(state, child) else {
        return Region::default();
    };
    let mut universe = parent_universe.clone();
    let siblings = state.resources.children(parent);
    if let Some(at) = siblings.iter().position(|s| *s == child) {
        for s in &siblings[at + 1..] {
            if universe.is_empty() {
                break;
            }
            if viewable(state, *s) && !transparent(state, *s) && !also_transparent.contains(s) {
                universe = universe.subtract(&border_size(state, *s));
            }
        }
    }
    universe.translate(-dx, -dy)
}

/// The visible part of `w`'s border, in its content space: Xorg's
/// `borderClip − winSize` (`mi/mivaltree.c:362-380`). A redirected window's
/// `borderClip` is its own `borderSize` (`mi/mivaltree.c:233-239`). Empty
/// without a border (`HasBorder`).
#[cfg(test)]
pub(crate) fn border_ring_clip(state: &ServerState, w: ResourceId) -> Vec<RegionRect> {
    border_ring(state, w).to_rects()
}

fn border_ring(state: &ServerState, w: ResourceId) -> Region {
    if w == ROOT_WINDOW || !viewable(state, w) {
        return Region::default();
    }
    let Some(parent) = state.resources.window(w).map(|c| c.parent) else {
        return Region::default();
    };
    let Some((dx, dy, bw)) = content_offset(state, w) else {
        return Region::default();
    };
    if bw == 0 {
        return Region::default();
    }
    let own = border_size(state, w).translate(-dx, -dy);
    let clip = if state.composite_redirects.window_mode(w).is_some() {
        own
    } else {
        let universe = universe(state, parent);
        uncovered_in_child_space(state, &universe, parent, w, &[]).intersect(&own)
    };
    clip.subtract(&win_size(state, w))
}

/// One viewable child's share of its parent's universe, from
/// [`split_universe`]: its own universe and visible border ring, both in
/// its content space.
struct ChildClip {
    window: ResourceId,
    universe: Region,
    ring: Region,
}

/// `miComputeClips` over `parent`'s children: walk them once, top-most
/// first, carrying what of `universe` (the parent's) they leave uncovered.
/// Each viewable child for which `want` (its index in `children`, its id
/// and its `borderSize`) holds gets its universe, `borderClip ∩ winSize`,
/// and, with `borders`, its ring, `borderClip − winSize`. A redirected
/// child's are its own (`mi/mivaltree.c:233-239`). A child then covers the
/// rest of the parent unless it is Manual-redirected. What is left at the
/// end is the parent's clip list.
fn split_universe(
    state: &ServerState,
    parent: ResourceId,
    universe: Region,
    borders: bool,
    mut want: impl FnMut(usize, ResourceId, &Region) -> bool,
) -> (Region, Vec<ChildClip>) {
    let mut uncovered = universe;
    let mut wanted = Vec::new();
    let children = state.resources.children(parent);
    for (i, &c) in children.iter().enumerate().rev() {
        if !viewable(state, c) {
            continue;
        }
        let Some((dx, dy, bw)) = content_offset(state, c) else {
            continue;
        };
        let size = border_size(state, c);
        if want(i, c, &size) {
            let win = win_size(state, c);
            let redirected = state.composite_redirects.window_mode(c).is_some();
            // `borderClip`, in the child's content space.
            let border_clip = if redirected {
                size.clone()
            } else {
                uncovered.intersect(&size)
            }
            .translate(-dx, -dy);
            let ring = if borders && bw != 0 {
                border_clip.subtract(&win)
            } else {
                Region::default()
            };
            let universe = if redirected {
                win
            } else {
                border_clip.intersect(&win)
            };
            wanted.push(ChildClip {
                window: c,
                universe,
                ring,
            });
        }
        if !uncovered.is_empty() && !transparent(state, c) {
            uncovered = uncovered.subtract(&size);
        }
    }
    (uncovered, wanted)
}

/// One window's clip list and visible border ring, for an exposure diff.
pub(crate) type ClipEntry = (ResourceId, Region, Region);

/// What a change exposes of one window: `region` of its content and
/// `border` of its border (`after.exposed` and `after.borderExposed`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Exposure {
    pub(crate) window: ResourceId,
    pub(crate) region: Vec<RegionRect>,
    pub(crate) border: Vec<RegionRect>,
}

/// `w`'s clip list in its content space: what of it shows and is not under
/// a child (ClipByChildren).
pub(crate) fn clip_list(state: &ServerState, w: ResourceId) -> Vec<RegionRect> {
    let mut region = universe(state, w);
    for c in state.resources.children(w) {
        if region.is_empty() {
            break;
        }
        if viewable(state, *c) && !transparent(state, *c) {
            region = region.subtract(&border_size(state, *c));
        }
    }
    region.to_rects()
}

/// The clip lists of `w` and every viewable window inside it, in the order
/// Xorg delivers exposures (`miHandleValidateExposures`, `mi/miwindow.c`):
/// a window before its children, children top-most first.
pub(crate) fn subtree_clip_lists(
    state: &ServerState,
    w: ResourceId,
) -> Vec<(ResourceId, Vec<RegionRect>)> {
    let mut out = Vec::new();
    walk(
        state,
        w,
        universe(state, w),
        Region::default(),
        false,
        &mut out,
    );
    without_rings(out)
}

/// [`subtree_clip_lists`] of each of `tops` in turn, siblings mapped
/// together (MapSubwindows): their parent's universe is split among them
/// once instead of once per window.
pub(crate) fn subtrees_clip_lists(
    state: &ServerState,
    tops: &[ResourceId],
) -> Vec<(ResourceId, Vec<RegionRect>)> {
    let parent = tops
        .first()
        .and_then(|t| state.resources.window(*t))
        .map(|w| w.parent);
    let siblings = tops.len() > 1
        && tops
            .iter()
            .all(|t| *t != ROOT_WINDOW && state.resources.window(*t).map(|w| w.parent) == parent);
    let (Some(parent), true) = (parent, siblings) else {
        return tops
            .iter()
            .flat_map(|t| subtree_clip_lists(state, *t))
            .collect();
    };
    let members: HashSet<ResourceId> = tops.iter().copied().collect();
    let (_, kids) = split_universe(state, parent, universe(state, parent), false, |_, c, _| {
        members.contains(&c)
    });
    let mut universes: HashMap<ResourceId, Region> =
        kids.into_iter().map(|k| (k.window, k.universe)).collect();
    let mut out = Vec::new();
    for t in tops {
        if let Some(u) = universes.remove(t) {
            walk(state, *t, u, Region::default(), false, &mut out);
        }
    }
    without_rings(out)
}

fn without_rings(entries: Vec<ClipEntry>) -> Vec<(ResourceId, Vec<RegionRect>)> {
    entries
        .into_iter()
        .map(|(w, clip, _)| (w, clip.to_rects()))
        .collect()
}

/// Push the entries of `w` (its clip list, from `universe`, and `ring`) and
/// of its viewable inferiors in exposure order.
fn walk(
    state: &ServerState,
    w: ResourceId,
    universe: Region,
    ring: Region,
    borders: bool,
    out: &mut Vec<ClipEntry>,
) {
    if !viewable(state, w) {
        return;
    }
    let (clip, kids) = split_universe(state, w, universe, borders, |_, _, _| true);
    out.push((w, clip, ring));
    for k in kids {
        walk(state, k.window, k.universe, k.ring, borders, out);
    }
}

/// The clip lists and border rings unmapping `w` can grow, before or after
/// it: its parent's and those of the siblings stacked under it that it
/// overlaps, with their subtrees, in exposure order (`UnmapWindow`
/// validates and exposes from the parent, `dix/window.c:2856-2866`).
pub(crate) fn clip_entries_under(state: &ServerState, w: ResourceId) -> Vec<ClipEntry> {
    let Some(parent) = state.resources.window(w).map(|win| win.parent) else {
        return Vec::new();
    };
    let footprint = border_size(state, w);
    let at = state
        .resources
        .children(parent)
        .iter()
        .position(|s| *s == w);
    let (clip, kids) = split_universe(
        state,
        parent,
        universe(state, parent),
        true,
        |i, _, size| at.is_some_and(|at| i < at) && !size.intersect(&footprint).is_empty(),
    );
    let mut out = vec![(parent, clip, border_ring(state, parent))];
    for k in kids {
        walk(state, k.window, k.universe, k.ring, true, &mut out);
    }
    out
}

/// `w`'s outer rect in its parent's content space, border included.
fn outer_rect(state: &ServerState, w: ResourceId) -> Option<RegionRect> {
    let win = state.resources.window(w)?;
    let bw2 = win.border_width.saturating_mul(2);
    Some(RegionRect {
        x: win.x,
        y: win.y,
        width: win.width.saturating_add(bw2),
        height: win.height.saturating_add(bw2),
    })
}

fn overlaps(a: RegionRect, b: RegionRect) -> bool {
    let (ax, ay, bx, by) = (
        i32::from(a.x),
        i32::from(a.y),
        i32::from(b.x),
        i32::from(b.y),
    );
    ax < bx + i32::from(b.width)
        && bx < ax + i32::from(a.width)
        && ay < by + i32::from(b.height)
        && by < ay + i32::from(a.height)
}

/// The clip lists and rings of `parent` and of those of its children in
/// `members` with their subtrees, in exposure order
/// (`miHandleValidateExposures` from the layer's parent): the parent, then
/// the members top-most first.
fn clip_entries_of(
    state: &ServerState,
    parent: ResourceId,
    members: &HashSet<ResourceId>,
) -> Vec<ClipEntry> {
    let (clip, kids) = split_universe(state, parent, universe(state, parent), true, |_, c, _| {
        members.contains(&c)
    });
    let mut out = vec![(parent, clip, border_ring(state, parent))];
    for k in kids {
        walk(state, k.window, k.universe, k.ring, true, &mut out);
    }
    out
}

/// A change to a viewable window's geometry, stacking or shape, as Xorg
/// validates one (`ValidateTree` + `HandleExposures` from the parent,
/// `dix/window.c:2149-2176`, `mi/miwindow.c`): the clip lists it can
/// alter, taken before the change, so that [`TreeChange::exposed`] can
/// diff them against the ones after. Those are the parent's and those of
/// the siblings (the window included) that meet the window's outer rect
/// before or after, with their subtrees; no other window's clip list
/// depends on the change.
pub(crate) struct TreeChange {
    parent: ResourceId,
    members: HashSet<ResourceId>,
    before: Vec<ClipEntry>,
}

impl TreeChange {
    /// Snapshot before changing `w`; `reach` is the outer rect (parent
    /// content space) the change moves or sizes it to, if any. `None` for
    /// a window that is not viewable: its change exposes nothing.
    pub(crate) fn begin(
        state: &ServerState,
        w: ResourceId,
        reach: Option<RegionRect>,
    ) -> Option<Self> {
        if w == ROOT_WINDOW || !viewable(state, w) {
            return None;
        }
        let parent = state.resources.window(w)?.parent;
        let mut extent = vec![outer_rect(state, w)?];
        extent.extend(reach);
        let members: HashSet<ResourceId> = state
            .resources
            .children(parent)
            .iter()
            .copied()
            .filter(|s| {
                *s == w
                    || (viewable(state, *s)
                        && outer_rect(state, *s)
                            .is_some_and(|r| extent.iter().any(|e| overlaps(r, *e))))
            })
            .collect();
        let before = clip_entries_of(state, parent, &members);
        Some(Self {
            parent,
            members,
            before,
        })
    }

    /// Per window, in exposure order, what it shows now that it did not
    /// before (`miComputeClips`: the new clip list less the old one, both
    /// in the window's own space, `mi/mivaltree.c:453-458`). `whole` gets
    /// its entire clip list instead: a resized window's contents are
    /// forfeit (`mi/miwindow.c:466-472`).
    pub(crate) fn exposed(self, state: &ServerState, whole: Option<ResourceId>) -> Vec<Exposure> {
        let after = clip_entries_of(state, self.parent, &self.members);
        newly_exposed(&self.before, after, whole)
    }
}

/// Per window, what `after` shows that `before` did not: the exposures a
/// change between the two makes, in `after`'s order. `whole`, if any,
/// gains its entire clip list; its border stays a diff (`borderVisible`,
/// `mi/miwindow.c:418-429`). The border gained is the visible ring less
/// the old one (`mi/mivaltree.c:362-380`).
pub(crate) fn newly_exposed(
    before: &[ClipEntry],
    after: Vec<ClipEntry>,
    whole: Option<ResourceId>,
) -> Vec<Exposure> {
    let mut index: HashMap<ResourceId, usize> = HashMap::with_capacity(before.len());
    for (i, (w, _, _)) in before.iter().enumerate() {
        index.entry(*w).or_insert(i);
    }
    after
        .into_iter()
        .filter_map(|(w, region, ring)| {
            let old = index.get(&w).map(|i| &before[*i]);
            let region = match old.filter(|_| Some(w) != whole) {
                Some((_, old, _)) => region.subtract(old),
                None => region,
            };
            let border = match old {
                Some((_, _, old_ring)) => ring.subtract(old_ring),
                None => ring,
            };
            (!region.is_empty() || !border.is_empty()).then(|| Exposure {
                window: w,
                region: region.to_rects(),
                border: border.to_rects(),
            })
        })
        .collect()
}

#[cfg(test)]
mod equivalence;
#[cfg(test)]
mod oracle;

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: i16, y: i16, width: u16, height: u16) -> RegionRect {
        RegionRect {
            x,
            y,
            width,
            height,
        }
    }

    /// pixman merges bands whose spans agree: the union of two stacked
    /// rects with the same x range is one rect, and a rect minus an inner
    /// one is four, top band, two sides, bottom band.
    #[test]
    fn canonical_coalesces_bands_like_pixman() {
        assert_eq!(
            canonical(vec![r(0, 0, 10, 5), r(0, 5, 10, 5)]),
            vec![r(0, 0, 10, 10)]
        );
        assert_eq!(
            subtract(&[r(0, 0, 10, 10)], &[r(3, 3, 4, 4)]),
            vec![r(0, 0, 10, 3), r(0, 3, 3, 4), r(7, 3, 3, 4), r(0, 7, 10, 3)]
        );
        // UnmapSubwindows in draw-clip's expose-probe: P1 ∪ P5 seen on P.
        assert_eq!(
            canonical(vec![r(40, 10, 70, 50), r(40, 40, 40, 60)]),
            vec![r(40, 10, 70, 50), r(40, 60, 40, 40)]
        );
    }

    fn window(
        state: &mut ServerState,
        id: u32,
        parent: ResourceId,
        geom: (i16, i16, u16, u16),
        class: u16,
    ) {
        state.resources.create_window(
            yserver_protocol::x11::ClientId(1),
            yserver_protocol::x11::CreateWindowRequest {
                depth: 24,
                window: ResourceId(id),
                parent,
                x: geom.0,
                y: geom.1,
                width: geom.2,
                height: geom.3,
                border_width: 0,
                class,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        state
            .resources
            .window_mut(ResourceId(id))
            .unwrap()
            .map_state = MapState::Viewable;
    }

    /// MapWindow of tools/vng-scenarios/expose-probe.c's P, measured on
    /// Xorg without a compositor: P at (-40,200) 300x200, children P1
    /// (10,10 100x50), P2 (50,40 100x80) above it, an InputOnly P3 over
    /// all of P and P4 (200,150 150x100). P's exposure is its part on the
    /// screen less P1, P2 and P4 in pixman's seven rects, P1's what P2
    /// leaves of it, and P3 none; children come top-most first.
    #[test]
    fn map_exposures_are_the_clip_lists_in_xorgs_order() {
        let mut state = ServerState::with_geometry(1024, 768);
        let (p, p1, p2, p3, p4) = (
            0x0020_0100,
            0x0020_0101,
            0x0020_0102,
            0x0020_0103,
            0x0020_0104,
        );
        window(&mut state, p, ROOT_WINDOW, (-40, 200, 300, 200), 1);
        window(&mut state, p1, ResourceId(p), (10, 10, 100, 50), 1);
        window(&mut state, p2, ResourceId(p), (50, 40, 100, 80), 1);
        window(&mut state, p3, ResourceId(p), (0, 0, 300, 200), 2);
        window(&mut state, p4, ResourceId(p), (200, 150, 150, 100), 1);
        let got = subtree_clip_lists(&state, ResourceId(p));
        let ids: Vec<u32> = got.iter().map(|(w, _)| w.0).collect();
        assert_eq!(ids, vec![p, p4, p2, p1], "P3 is InputOnly");
        assert_eq!(
            got[0].1,
            vec![
                r(40, 0, 260, 10),
                r(110, 10, 190, 30),
                r(150, 40, 150, 20),
                r(40, 60, 10, 60),
                r(150, 60, 150, 60),
                r(40, 120, 260, 30),
                r(40, 150, 160, 50),
            ]
        );
        assert_eq!(got[1].1, vec![r(0, 0, 100, 50)]);
        assert_eq!(got[2].1, vec![r(0, 0, 100, 80)]);
        assert_eq!(got[3].1, vec![r(30, 0, 70, 30), r(30, 30, 10, 20)]);
    }

    fn configure(
        state: &mut ServerState,
        id: u32,
        geom: Option<(i16, i16, u16, u16)>,
        stack: Option<(u8, Option<u32>)>,
    ) {
        state
            .resources
            .configure_window(yserver_protocol::x11::ConfigureWindowRequest {
                window: ResourceId(id),
                value_mask: 0,
                x: geom.map(|g| g.0),
                y: geom.map(|g| g.1),
                width: geom.map(|g| g.2),
                height: geom.map(|g| g.3),
                border_width: None,
                sibling: stack.and_then(|s| s.1).map(ResourceId),
                stack_mode: stack.map(|s| s.0),
            });
    }

    fn change(
        state: &mut ServerState,
        id: u32,
        geom: Option<(i16, i16, u16, u16)>,
        stack: Option<(u8, Option<u32>)>,
        whole: bool,
    ) -> Vec<(u32, Vec<RegionRect>)> {
        let reach = geom.map(|g| r(g.0, g.1, g.2, g.3));
        let change = TreeChange::begin(state, ResourceId(id), reach).unwrap();
        configure(state, id, geom, stack);
        change
            .exposed(state, whole.then_some(ResourceId(id)))
            .into_iter()
            .map(|e| (e.window.0, e.region))
            .collect()
    }

    /// tools/vng-scenarios/restack-expose-probe.c's top-levels, measured
    /// on Xorg without a compositor (goldens/restack-expose.txt): A and B
    /// at (40,40) 200x150, A stacked under B, C at (180,120) 120x100 on
    /// top. Each restack, move and resize exposes the new clip list less
    /// the old one, a resized window its whole clip list, the higher
    /// window first.
    #[test]
    fn tree_changes_expose_what_xorg_exposes() {
        const ABOVE: u8 = 0;
        const BELOW: u8 = 1;
        let mut state = ServerState::with_geometry(1024, 768);
        let (a, b, c) = (0x0020_0001, 0x0020_0002, 0x0020_0003);
        window(&mut state, b, ROOT_WINDOW, (40, 40, 200, 150), 1);
        window(&mut state, a, ROOT_WINDOW, (40, 40, 200, 150), 1);
        configure(&mut state, a, None, Some((BELOW, None)));
        let root = ROOT_WINDOW.0;

        let got = change(&mut state, a, None, Some((ABOVE, None)), false);
        assert_eq!(got, vec![(a, vec![r(0, 0, 200, 150)])], "A raised");
        let got = change(&mut state, a, None, Some((BELOW, None)), false);
        assert_eq!(got, vec![(b, vec![r(0, 0, 200, 150)])], "A lowered");

        window(&mut state, c, ROOT_WINDOW, (180, 120, 120, 100), 1);
        let got = change(&mut state, a, None, Some((BELOW, Some(c))), false);
        assert_eq!(
            got,
            vec![(a, vec![r(0, 0, 200, 80), r(0, 80, 140, 70)])],
            "A raised to just below C"
        );
        let got = change(&mut state, a, None, Some((ABOVE, None)), false);
        assert_eq!(got, vec![(a, vec![r(140, 80, 60, 70)])], "A raised over C");
        let got = change(&mut state, a, None, Some((BELOW, None)), false);
        assert_eq!(
            got,
            vec![
                (c, vec![r(0, 0, 60, 70)]),
                (b, vec![r(0, 0, 200, 80), r(0, 80, 140, 70)]),
            ],
            "A lowered under B and C"
        );

        let got = change(&mut state, b, Some((70, 60, 200, 150)), None, false);
        assert_eq!(
            got.iter()
                .filter(|(w, _)| *w != root)
                .cloned()
                .collect::<Vec<_>>(),
            vec![(a, vec![r(0, 0, 200, 20), r(0, 20, 30, 130)])],
            "B moved by 30,20"
        );
        let got = change(&mut state, b, Some((70, 60, 230, 170)), None, true);
        assert_eq!(
            got,
            vec![(
                b,
                vec![r(0, 0, 230, 60), r(0, 60, 110, 100), r(0, 160, 230, 10)]
            )],
            "B grown to 230x170"
        );
        let got = change(&mut state, b, Some((70, 60, 150, 100)), None, true);
        assert_eq!(
            got.iter()
                .filter(|(w, _)| *w != root)
                .cloned()
                .collect::<Vec<_>>(),
            vec![
                (b, vec![r(0, 0, 150, 60), r(0, 60, 110, 40)]),
                (a, vec![r(180, 20, 20, 60), r(30, 120, 110, 30)]),
            ],
            "B shrunk to 150x100"
        );
    }

    /// The same under an Automatic RedirectSubwindows of the root, measured
    /// on Xorg: a redirected window's clip list is its own shape, so a
    /// restack exposes nothing and a resize the whole window.
    #[test]
    fn redirected_top_levels_are_not_exposed_by_restacks() {
        let mut state = ServerState::with_geometry(1024, 768);
        let (a, b) = (0x0020_0001, 0x0020_0002);
        window(&mut state, b, ROOT_WINDOW, (40, 40, 200, 150), 1);
        window(&mut state, a, ROOT_WINDOW, (40, 40, 200, 150), 1);
        configure(&mut state, a, None, Some((1, None)));
        for w in [a, b] {
            state
                .composite_redirects
                .redirect_window(
                    ResourceId(w),
                    crate::server::RedirectRecord {
                        owner: yserver_protocol::x11::ClientId(1),
                        mode: CompositeRedirectMode::Automatic,
                    },
                )
                .unwrap();
        }
        let got = change(&mut state, a, None, Some((0, None)), false);
        assert!(
            got.iter().all(|(w, _)| *w == ROOT_WINDOW.0),
            "A raised: {got:?}"
        );
        let got = change(&mut state, b, Some((40, 40, 230, 170)), None, true);
        assert_eq!(
            got.iter()
                .filter(|(w, _)| *w != ROOT_WINDOW.0)
                .cloned()
                .collect::<Vec<_>>(),
            vec![(b, vec![r(0, 0, 230, 170)])]
        );
    }

    fn set_border(state: &mut ServerState, id: u32, bw: u16) {
        state
            .resources
            .window_mut(ResourceId(id))
            .unwrap()
            .border_width = bw;
    }

    fn redirect_manual(state: &mut ServerState, id: u32) {
        state
            .composite_redirects
            .redirect_window(
                ResourceId(id),
                crate::server::RedirectRecord {
                    owner: yserver_protocol::x11::ClientId(1),
                    mode: CompositeRedirectMode::Manual,
                },
            )
            .unwrap();
    }

    fn borders(got: Vec<Exposure>) -> Vec<(u32, Vec<RegionRect>)> {
        got.into_iter()
            .filter(|e| !e.border.is_empty())
            .map(|e| (e.window.0, e.border))
            .collect()
    }

    /// `miComputeClips`' `borderExposed` (`mi/mivaltree.c:362-380`) for the
    /// probe's C: a child at (80,80) 50x50 bw 4 in a redirected W that grows
    /// from 100 to 200. Its extents match Xorg's border damage `80,80 58x58`.
    #[test]
    fn border_exposed_of_a_child_crossing_the_grown_edge() {
        let mut state = ServerState::with_geometry(1024, 768);
        let (w, c) = (0x0020_0201, 0x0020_0202);
        window(&mut state, w, ROOT_WINDOW, (300, 300, 100, 100), 1);
        redirect_manual(&mut state, w);
        window(&mut state, c, ResourceId(w), (80, 80, 50, 50), 1);
        set_border(&mut state, c, 4);
        let reach = Some(r(300, 300, 200, 200));
        let change = TreeChange::begin(&state, ResourceId(w), reach).unwrap();
        configure(&mut state, w, Some((300, 300, 200, 200)), None);
        let got = borders(change.exposed(&state, Some(ResourceId(w))));
        assert_eq!(
            got,
            vec![(
                c,
                vec![
                    r(16, -4, 38, 4),
                    r(50, 0, 4, 16),
                    r(-4, 16, 4, 34),
                    r(50, 16, 4, 34),
                    r(-4, 50, 58, 4),
                ]
            )]
        );
    }

    /// U: a sibling S (0,0) 30x30 over C (10,10) 50x50 bw 4 unmaps; the
    /// part of C's ring S covered is exposed (Xorg damage `10,10 20x20`).
    #[test]
    fn border_exposed_when_an_occluding_sibling_unmaps() {
        let mut state = ServerState::with_geometry(1024, 768);
        let (w, c, sib) = (0x0020_0211, 0x0020_0212, 0x0020_0213);
        window(&mut state, w, ROOT_WINDOW, (400, 300, 120, 120), 1);
        redirect_manual(&mut state, w);
        window(&mut state, c, ResourceId(w), (10, 10, 50, 50), 1);
        set_border(&mut state, c, 4);
        window(&mut state, sib, ResourceId(w), (0, 0, 30, 30), 1);
        let before = clip_entries_under(&state, ResourceId(sib));
        let _ = state.resources.unmap_window(ResourceId(sib));
        let after = clip_entries_under(&state, ResourceId(sib));
        let got = borders(newly_exposed(&before, after, None));
        assert_eq!(got, vec![(c, vec![r(-4, -4, 20, 4), r(-4, 0, 4, 16)])]);
    }

    /// `HasBorder` is false at `bw == 0`: no border is ever exposed.
    #[test]
    fn unbordered_windows_expose_no_border() {
        let mut state = ServerState::with_geometry(1024, 768);
        let (w, c, sib) = (0x0020_0221, 0x0020_0222, 0x0020_0223);
        window(&mut state, w, ROOT_WINDOW, (0, 0, 120, 120), 1);
        window(&mut state, c, ResourceId(w), (10, 10, 50, 50), 1);
        window(&mut state, sib, ResourceId(w), (0, 0, 30, 30), 1);
        let before = clip_entries_under(&state, ResourceId(sib));
        let _ = state.resources.unmap_window(ResourceId(sib));
        let after = clip_entries_under(&state, ResourceId(sib));
        assert!(borders(newly_exposed(&before, after, None)).is_empty());
    }
}
