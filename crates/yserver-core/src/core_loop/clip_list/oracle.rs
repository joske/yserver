//! The clip-list traversal as it was before the single top-down pass: each
//! child's universe re-subtracts all of its higher siblings and every
//! border ring is computed from the root. Slow but verified against Xorg's
//! exposures, so the equivalence tests compare the new traversal to it.

use yserver_protocol::x11::{ResourceId, xfixes::RegionRect};

use super::{intersect, subtract, translate};
use crate::{
    resources::{MapState, ROOT_WINDOW, WindowClass},
    server::{CompositeRedirectMode, ServerState},
};

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
pub(super) fn border_size_in_parent(state: &ServerState, w: ResourceId) -> Vec<RegionRect> {
    let Some(win) = state.resources.window(w) else {
        return Vec::new();
    };
    let bw2 = win.border_width.saturating_mul(2);
    let outer = RegionRect {
        x: win.x,
        y: win.y,
        width: win.width.saturating_add(bw2),
        height: win.height.saturating_add(bw2),
    };
    match state
        .shape_windows
        .get(&w)
        .and_then(|s| s.bounding().cloned())
    {
        Some(shape) => {
            let bw = i32::from(win.border_width);
            let shape = translate(shape, i32::from(win.x) + bw, i32::from(win.y) + bw);
            intersect(&[outer], &shape)
        }
        None => vec![outer],
    }
}

/// Xorg's `winSize` of `w` in its own content space, short of its
/// parent's: its content rect cut to its bounding and clip shapes
/// (`SetWinSize`, `dix/window.c:1710`).
fn win_size(state: &ServerState, w: ResourceId) -> Vec<RegionRect> {
    let Some(win) = state.resources.window(w) else {
        return Vec::new();
    };
    let mut region = vec![RegionRect {
        x: 0,
        y: 0,
        width: win.width,
        height: win.height,
    }];
    if let Some(shape) = state.shape_windows.get(&w) {
        for s in [shape.bounding(), shape.clip()].into_iter().flatten() {
            region = intersect(&region, s);
        }
    }
    region
}

/// What of `w` shows, its inferiors included, in its content space:
/// Xorg's `borderClip ∩ winSize`, which `NotClippedByChildren` returns and
/// its children's universes are cut from. A redirected window's is its own
/// shape, not clipped by its parent or the screen (`mi/mivaltree.c:233-
/// 239`); every other window's is its parent's less the windows stacked
/// above it there.
pub(super) fn not_clipped_by_children(state: &ServerState, w: ResourceId) -> Vec<RegionRect> {
    if !viewable(state, w) {
        return Vec::new();
    }
    if w == ROOT_WINDOW {
        return state
            .resources
            .window(ROOT_WINDOW)
            .map_or_else(Vec::new, |r| {
                vec![RegionRect {
                    x: 0,
                    y: 0,
                    width: r.width,
                    height: r.height,
                }]
            });
    }
    if state.composite_redirects.window_mode(w).is_some() {
        return win_size(state, w);
    }
    let Some(parent) = state.resources.window(w).map(|win| win.parent) else {
        return Vec::new();
    };
    let universe = not_clipped_by_children(state, parent);
    child_universe(state, &universe, parent, w)
}

/// [`not_clipped_by_children`] of `w` while the `still_redirected` windows,
/// Manual redirects not yet released, stay transparent: the clip of
/// `compRestoreWindow` when `compFreeClientSubwindows` unredirects one
/// child at a time (`composite/compalloc.c:450-452`).
pub(super) fn not_clipped_by_children_while(
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
    let universe = not_clipped_by_children(state, parent);
    child_universe_with(state, &universe, parent, w, still_redirected)
}

/// [`not_clipped_by_children`] of `child` from its parent's.
fn child_universe(
    state: &ServerState,
    parent_universe: &[RegionRect],
    parent: ResourceId,
    child: ResourceId,
) -> Vec<RegionRect> {
    child_universe_with(state, parent_universe, parent, child, &[])
}

fn child_universe_with(
    state: &ServerState,
    parent_universe: &[RegionRect],
    parent: ResourceId,
    child: ResourceId,
    also_transparent: &[ResourceId],
) -> Vec<RegionRect> {
    if !viewable(state, child) {
        return Vec::new();
    }
    if state.composite_redirects.window_mode(child).is_some() {
        return win_size(state, child);
    }
    if parent_universe.is_empty() {
        return Vec::new();
    }
    let universe =
        uncovered_in_child_space(state, parent_universe, parent, child, also_transparent);
    intersect(&universe, &win_size(state, child))
}

/// `parent_universe` less `child`'s higher siblings, in `child`'s content
/// space: what its `borderSize` is cut to (`miComputeClips`).
fn uncovered_in_child_space(
    state: &ServerState,
    parent_universe: &[RegionRect],
    parent: ResourceId,
    child: ResourceId,
    also_transparent: &[ResourceId],
) -> Vec<RegionRect> {
    let Some((x, y, bw)) = state
        .resources
        .window(child)
        .map(|c| (i32::from(c.x), i32::from(c.y), i32::from(c.border_width)))
    else {
        return Vec::new();
    };
    let mut universe = parent_universe.to_vec();
    let siblings = state.resources.children(parent);
    if let Some(at) = siblings.iter().position(|s| *s == child) {
        for s in &siblings[at + 1..] {
            if viewable(state, *s) && !transparent(state, *s) && !also_transparent.contains(s) {
                universe = subtract(&universe, &border_size_in_parent(state, *s));
            }
        }
    }
    translate(universe, -(x + bw), -(y + bw))
}

/// The visible part of `w`'s border, in its content space: Xorg's
/// `borderClip − winSize` (`mi/mivaltree.c:362-380`). A redirected window's
/// `borderClip` is its own `borderSize` (`mi/mivaltree.c:233-239`). Empty
/// without a border (`HasBorder`).
pub(super) fn border_ring_clip(state: &ServerState, w: ResourceId) -> Vec<RegionRect> {
    if w == ROOT_WINDOW || !viewable(state, w) {
        return Vec::new();
    }
    let Some((x, y, bw, parent)) = state.resources.window(w).map(|c| {
        (
            i32::from(c.x),
            i32::from(c.y),
            i32::from(c.border_width),
            c.parent,
        )
    }) else {
        return Vec::new();
    };
    if bw == 0 {
        return Vec::new();
    }
    let own = translate(border_size_in_parent(state, w), -(x + bw), -(y + bw));
    let clip = if state.composite_redirects.window_mode(w).is_some() {
        own
    } else {
        let universe = not_clipped_by_children(state, parent);
        intersect(
            &uncovered_in_child_space(state, &universe, parent, w, &[]),
            &own,
        )
    };
    subtract(&clip, &win_size(state, w))
}

/// One window's clip list and visible border ring, for an exposure diff.
pub(super) type ClipEntry = (ResourceId, Vec<RegionRect>, Vec<RegionRect>);

/// `clips` with each window's [`border_ring_clip`] alongside.
pub(super) fn with_borders(
    state: &ServerState,
    clips: Vec<(ResourceId, Vec<RegionRect>)>,
) -> Vec<ClipEntry> {
    clips
        .into_iter()
        .map(|(w, clip)| {
            let ring = border_ring_clip(state, w);
            (w, clip, ring)
        })
        .collect()
}

/// What a change exposes of one window: `region` of its content and
/// `border` of its border (`after.exposed` and `after.borderExposed`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Exposure {
    pub(super) window: ResourceId,
    pub(super) region: Vec<RegionRect>,
    pub(super) border: Vec<RegionRect>,
}

/// `w`'s clip list in its content space: what of it shows and is not under
/// a child (ClipByChildren).
pub(super) fn clip_list(state: &ServerState, w: ResourceId) -> Vec<RegionRect> {
    clip_list_from(state, w, not_clipped_by_children(state, w))
}

fn clip_list_from(
    state: &ServerState,
    w: ResourceId,
    universe: Vec<RegionRect>,
) -> Vec<RegionRect> {
    let mut region = universe;
    for c in state.resources.children(w) {
        if region.is_empty() {
            break;
        }
        if viewable(state, *c) && !transparent(state, *c) {
            region = subtract(&region, &border_size_in_parent(state, *c));
        }
    }
    region
}

/// The clip lists of `w` and every viewable window inside it, in the order
/// Xorg delivers exposures (`miHandleValidateExposures`, `mi/miwindow.c`):
/// a window before its children, children top-most first.
pub(super) fn subtree_clip_lists(
    state: &ServerState,
    w: ResourceId,
) -> Vec<(ResourceId, Vec<RegionRect>)> {
    let mut out = Vec::new();
    let universe = not_clipped_by_children(state, w);
    walk(state, w, universe, &mut out);
    out
}

fn walk(
    state: &ServerState,
    w: ResourceId,
    universe: Vec<RegionRect>,
    out: &mut Vec<(ResourceId, Vec<RegionRect>)>,
) {
    if !viewable(state, w) {
        return;
    }
    let children: Vec<ResourceId> = state.resources.children(w).iter().rev().copied().collect();
    let child_universes: Vec<(ResourceId, Vec<RegionRect>)> = children
        .iter()
        .map(|c| (*c, child_universe(state, &universe, w, *c)))
        .collect();
    out.push((w, clip_list_from(state, w, universe)));
    for (c, u) in child_universes {
        walk(state, c, u, out);
    }
}

/// The clip lists unmapping `w` can grow, before or after it: its
/// parent's and those of the siblings stacked under it that it overlaps,
/// with their subtrees, in exposure order (`UnmapWindow` validates and
/// exposes from the parent, `dix/window.c:2856-2866`).
pub(super) fn clip_lists_under(
    state: &ServerState,
    w: ResourceId,
) -> Vec<(ResourceId, Vec<RegionRect>)> {
    let mut out = Vec::new();
    let Some(parent) = state.resources.window(w).map(|win| win.parent) else {
        return out;
    };
    let universe = not_clipped_by_children(state, parent);
    out.push((parent, clip_list_from(state, parent, universe.clone())));
    let footprint = border_size_in_parent(state, w);
    let siblings = state.resources.children(parent);
    let Some(at) = siblings.iter().position(|s| *s == w) else {
        return out;
    };
    for s in siblings[..at].iter().rev() {
        if intersect(&border_size_in_parent(state, *s), &footprint).is_empty() {
            continue;
        }
        let u = child_universe(state, &universe, parent, *s);
        walk(state, *s, u, &mut out);
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

/// The clip lists of `parent` and of those of its children in `members`
/// with their subtrees, in exposure order (`miHandleValidateExposures`
/// from the layer's parent): the parent, then the members top-most first.
fn clip_lists_of(
    state: &ServerState,
    parent: ResourceId,
    members: &[ResourceId],
) -> Vec<(ResourceId, Vec<RegionRect>)> {
    let universe = not_clipped_by_children(state, parent);
    let mut out = vec![(parent, clip_list_from(state, parent, universe.clone()))];
    for c in state.resources.children(parent).iter().rev() {
        if members.contains(c) {
            let u = child_universe(state, &universe, parent, *c);
            walk(state, *c, u, &mut out);
        }
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
pub(super) struct TreeChange {
    parent: ResourceId,
    members: Vec<ResourceId>,
    before: Vec<ClipEntry>,
}

impl TreeChange {
    /// Snapshot before changing `w`; `reach` is the outer rect (parent
    /// content space) the change moves or sizes it to, if any. `None` for
    /// a window that is not viewable: its change exposes nothing.
    pub(super) fn begin(
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
        let members: Vec<ResourceId> = state
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
        let before = with_borders(state, clip_lists_of(state, parent, &members));
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
    pub(super) fn exposed(self, state: &ServerState, whole: Option<ResourceId>) -> Vec<Exposure> {
        let after = with_borders(state, clip_lists_of(state, self.parent, &self.members));
        newly_exposed(&self.before, after, whole)
    }
}

/// Per window, what `after` shows that `before` did not: the exposures a
/// change between the two makes, in `after`'s order. `whole`, if any,
/// gains its entire clip list; its border stays a diff (`borderVisible`,
/// `mi/miwindow.c:418-429`). The border gained is the visible ring less
/// the old one (`mi/mivaltree.c:362-380`).
pub(super) fn newly_exposed(
    before: &[ClipEntry],
    after: Vec<ClipEntry>,
    whole: Option<ResourceId>,
) -> Vec<Exposure> {
    after
        .into_iter()
        .filter_map(|(w, region, ring)| {
            let old = before.iter().find(|(b, _, _)| *b == w);
            let region = match old.filter(|_| Some(w) != whole) {
                Some((_, old, _)) => subtract(&region, old),
                None => region,
            };
            let border = match old {
                Some((_, _, old_ring)) => subtract(&ring, old_ring),
                None => ring,
            };
            (!region.is_empty() || !border.is_empty()).then_some(Exposure {
                window: w,
                region,
                border,
            })
        })
        .collect()
}
