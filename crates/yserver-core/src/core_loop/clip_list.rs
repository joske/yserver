//! Xorg's per-window clip regions, computed from the window tree on demand:
//! the clip list (what of a window shows, its children out) and the
//! universe its children are cut from (`NotClippedByChildren`), as
//! `miComputeClips` keeps them (`mi/mivaltree.c:194-470`). Graphics
//! exposures (`miHandleExposures`) and the Expose events of a map or an
//! unmap (`miHandleValidateExposures`) are made of them.
//!
//! Regions are in a window's content space and in pixman's canonical form
//! ([`canonical`]), so the rects a client is sent match Xorg's one for one.

use yserver_protocol::x11::{ResourceId, xfixes::RegionRect};

use crate::{
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
    canonical(crate::nested::intersect_regions(a, b))
}

/// `a − b`, canonical.
pub(crate) fn subtract(a: &[RegionRect], b: &[RegionRect]) -> Vec<RegionRect> {
    canonical(crate::nested::subtract_regions(a, b))
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
    rect.width == 0 || rect.height == 0 || subtract(&[rect], region).is_empty()
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
    match state.shape_windows.get(&w).and_then(|s| s.bounding.clone()) {
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
        for s in [&shape.bounding, &shape.clip].into_iter().flatten() {
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
pub(crate) fn not_clipped_by_children(state: &ServerState, w: ResourceId) -> Vec<RegionRect> {
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

/// [`not_clipped_by_children`] of `child` from its parent's.
fn child_universe(
    state: &ServerState,
    parent_universe: &[RegionRect],
    parent: ResourceId,
    child: ResourceId,
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
            if viewable(state, *s) && !transparent(state, *s) {
                universe = subtract(&universe, &border_size_in_parent(state, *s));
            }
        }
    }
    let universe = translate(universe, -(x + bw), -(y + bw));
    intersect(&universe, &win_size(state, child))
}

/// `w`'s clip list in its content space: what of it shows and is not under
/// a child (ClipByChildren).
pub(crate) fn clip_list(state: &ServerState, w: ResourceId) -> Vec<RegionRect> {
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
pub(crate) fn subtree_clip_lists(
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
pub(crate) fn clip_lists_under(
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

/// Per window, what `after` shows that `before` did not: the exposures a
/// change between the two makes, in `after`'s order.
pub(crate) fn newly_exposed(
    before: &[(ResourceId, Vec<RegionRect>)],
    after: Vec<(ResourceId, Vec<RegionRect>)>,
) -> Vec<(ResourceId, Vec<RegionRect>)> {
    after
        .into_iter()
        .filter_map(|(w, region)| {
            let old = before
                .iter()
                .find(|(b, _)| *b == w)
                .map(|(_, r)| r.as_slice());
            let gained = match old {
                Some(old) => subtract(&region, old),
                None => region,
            };
            (!gained.is_empty()).then_some((w, gained))
        })
        .collect()
}

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
}
