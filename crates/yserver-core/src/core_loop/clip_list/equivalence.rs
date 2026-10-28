//! The clip-list traversal against [`super::oracle`], the traversal it
//! replaced: every query and every exposure diff must give the same rects
//! in the same order.

use proptest::prelude::*;
use yserver_protocol::x11::{
    ClientId, ResourceId,
    shape::{KIND_BOUNDING, KIND_CLIP},
    xfixes::RegionRect,
};

use super::oracle;
use crate::{
    resources::{MapState, ROOT_WINDOW},
    server::{CompositeRedirectMode, RedirectRecord, ServerState},
};

type Rects = Vec<RegionRect>;

fn r(x: i16, y: i16, width: u16, height: u16) -> RegionRect {
    RegionRect {
        x,
        y,
        width,
        height,
    }
}

const ABOVE: u8 = 0;
const BELOW: u8 = 1;

/// One window of a generated tree: created in this order, so a later
/// window of the same parent stacks above an earlier one.
#[derive(Debug, Clone)]
struct Spec {
    /// Index into the windows created before this one; out of range
    /// means the root.
    parent: usize,
    geom: (i16, i16, u16, u16),
    border: u16,
    input_only: bool,
    mapped: bool,
    redirect: Option<CompositeRedirectMode>,
    bounding: Option<Rects>,
    clip: Option<Rects>,
}

fn id(i: usize) -> ResourceId {
    ResourceId(0x0020_0001 + u32::try_from(i).unwrap())
}

fn redirect(state: &mut ServerState, w: ResourceId, mode: CompositeRedirectMode) {
    state
        .composite_redirects
        .redirect_window(
            w,
            RedirectRecord {
                owner: ClientId(1),
                mode,
            },
        )
        .unwrap();
}

fn create(state: &mut ServerState, w: ResourceId, parent: ResourceId, s: &Spec) {
    state.resources.create_window(
        ClientId(1),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: w,
            parent,
            x: s.geom.0,
            y: s.geom.1,
            width: s.geom.2,
            height: s.geom.3,
            border_width: s.border,
            class: if s.input_only { 2 } else { 1 },
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let parent_viewable = parent == ROOT_WINDOW
        || state
            .resources
            .window(parent)
            .is_some_and(|p| p.map_state == MapState::Viewable);
    state.resources.window_mut(w).unwrap().map_state = match (s.mapped, parent_viewable) {
        (false, _) => MapState::Unmapped,
        (true, true) => MapState::Viewable,
        (true, false) => MapState::Unviewable,
    };
    if let Some(mode) = s.redirect {
        redirect(state, w, mode);
    }
    if s.bounding.is_some() || s.clip.is_some() {
        let shape = state.shape_windows.entry(w).or_default();
        shape.set_rects(KIND_BOUNDING, s.bounding.clone());
        shape.set_rects(KIND_CLIP, s.clip.clone());
    }
}

fn build(specs: &[Spec]) -> (ServerState, Vec<ResourceId>) {
    let mut state = ServerState::with_geometry(64, 48);
    let mut ids = Vec::new();
    for (i, s) in specs.iter().enumerate() {
        let parent = ids.get(s.parent).copied().unwrap_or(ROOT_WINDOW);
        create(&mut state, id(i), parent, s);
        ids.push(id(i));
    }
    (state, ids)
}

// The traversal under test, reduced to rects so that it compares with the
// oracle's.

fn new_entries_under(state: &ServerState, w: ResourceId) -> Vec<(ResourceId, Rects, Rects)> {
    rings_as_rects(super::clip_entries_under(state, w))
}

fn rings_as_rects(entries: Vec<super::ClipEntry>) -> Vec<(ResourceId, Rects, Rects)> {
    entries
        .into_iter()
        .map(|(w, clip, ring)| (w, clip.to_rects(), ring.to_rects()))
        .collect()
}

fn new_exposed(
    change: super::TreeChange,
    state: &ServerState,
    whole: Option<ResourceId>,
) -> Vec<(ResourceId, Rects, Rects)> {
    change
        .exposed(state, whole)
        .into_iter()
        .map(|e| (e.window, e.region, e.border))
        .collect()
}

fn new_batch(state: &ServerState, tops: &[ResourceId]) -> Vec<(ResourceId, Rects)> {
    super::subtrees_clip_lists(state, tops)
}

fn old_exposed(
    change: oracle::TreeChange,
    state: &ServerState,
    whole: Option<ResourceId>,
) -> Vec<(ResourceId, Rects, Rects)> {
    change
        .exposed(state, whole)
        .into_iter()
        .map(|e| (e.window, e.region, e.border))
        .collect()
}

fn old_entries_under(state: &ServerState, w: ResourceId) -> Vec<(ResourceId, Rects, Rects)> {
    oracle::with_borders(state, oracle::clip_lists_under(state, w))
}

/// Every per-window query, the root included, old against new.
fn assert_queries_match(state: &ServerState, ids: &[ResourceId]) {
    for w in std::iter::once(ROOT_WINDOW).chain(ids.iter().copied()) {
        assert_eq!(
            super::not_clipped_by_children(state, w),
            oracle::not_clipped_by_children(state, w),
            "not_clipped_by_children {w:?}"
        );
        assert_eq!(
            super::clip_list(state, w),
            oracle::clip_list(state, w),
            "clip_list {w:?}"
        );
        assert_eq!(
            super::border_ring_clip(state, w),
            oracle::border_ring_clip(state, w),
            "border_ring_clip {w:?}"
        );
        assert_eq!(
            super::border_size_in_parent(state, w),
            oracle::border_size_in_parent(state, w),
            "border_size_in_parent {w:?}"
        );
        assert_eq!(
            super::subtree_clip_lists(state, w),
            oracle::subtree_clip_lists(state, w),
            "subtree_clip_lists {w:?}"
        );
        if w != ROOT_WINDOW {
            assert_eq!(
                new_entries_under(state, w),
                old_entries_under(state, w),
                "entries under {w:?}"
            );
            let manual: Vec<ResourceId> = state
                .resources
                .children(state.resources.window(w).unwrap().parent)
                .to_vec();
            assert_eq!(
                super::not_clipped_by_children_while(state, w, &manual),
                oracle::not_clipped_by_children_while(state, w, &manual),
                "not_clipped_by_children_while {w:?}"
            );
        }
    }
    // MapSubwindows' batch: every child of a parent, top-most first.
    for p in std::iter::once(ROOT_WINDOW).chain(ids.iter().copied()) {
        let tops: Vec<ResourceId> = state.resources.children(p).iter().rev().copied().collect();
        let want: Vec<(ResourceId, Rects)> = tops
            .iter()
            .flat_map(|t| oracle::subtree_clip_lists(state, *t))
            .collect();
        assert_eq!(new_batch(state, &tops), want, "batch under {p:?}");
    }
}

/// A configure of `w` (geometry and/or stacking) through both
/// `TreeChange`s.
fn assert_change_matches(
    state: &mut ServerState,
    w: ResourceId,
    geom: Option<(i16, i16, u16, u16)>,
    stack: Option<(u8, Option<ResourceId>)>,
) {
    let bw = state.resources.window(w).map_or(0, |win| win.border_width);
    let reach = geom.map(|g| r(g.0, g.1, g.2 + 2 * bw, g.3 + 2 * bw));
    let new = super::TreeChange::begin(state, w, reach);
    let old = oracle::TreeChange::begin(state, w, reach);
    assert_eq!(new.is_some(), old.is_some());
    state
        .resources
        .configure_window(yserver_protocol::x11::ConfigureWindowRequest {
            window: w,
            value_mask: 0,
            x: geom.map(|g| g.0),
            y: geom.map(|g| g.1),
            width: geom.map(|g| g.2),
            height: geom.map(|g| g.3),
            border_width: None,
            sibling: stack.and_then(|s| s.1),
            stack_mode: stack.map(|s| s.0),
        });
    let whole = geom.map(|_| w);
    if let (Some(new), Some(old)) = (new, old) {
        assert_eq!(
            new_exposed(new, state, whole),
            old_exposed(old, state, whole),
            "configure {w:?} to {geom:?} {stack:?}"
        );
    }
}

/// An unmap of `w` diffed both ways, as UnmapWindow and DestroyWindow do.
fn assert_unmap_matches(state: &mut ServerState, w: ResourceId) {
    let new_before = super::clip_entries_under(state, w);
    let old_before = old_entries_under(state, w);
    assert_eq!(
        rings_as_rects(new_before.clone()),
        old_before,
        "before unmapping {w:?}"
    );
    let _ = state.resources.unmap_window(w);
    let new: Vec<(ResourceId, Rects, Rects)> =
        super::newly_exposed(&new_before, super::clip_entries_under(state, w), None)
            .into_iter()
            .map(|e| (e.window, e.region, e.border))
            .collect();
    let old: Vec<(ResourceId, Rects, Rects)> =
        oracle::newly_exposed(&old_before, old_entries_under(state, w), None)
            .into_iter()
            .map(|e| (e.window, e.region, e.border))
            .collect();
    assert_eq!(new, old, "unmapping {w:?}");
}

/// A bounding shape change of `w`, validated as ShapeRectangles does.
fn assert_shape_matches(state: &mut ServerState, w: ResourceId, bounding: Option<Rects>) {
    let new = super::TreeChange::begin(state, w, None);
    let old = oracle::TreeChange::begin(state, w, None);
    state
        .shape_windows
        .entry(w)
        .or_default()
        .set_rects(KIND_BOUNDING, bounding);
    if let (Some(new), Some(old)) = (new, old) {
        assert_eq!(
            new_exposed(new, state, None),
            old_exposed(old, state, None),
            "reshaping {w:?}"
        );
    }
}

// Hand-written cases: the redirect rules of the design
// (docs/superpowers/specs/2026-10-28-cached-clip-lists-design.md, Phase 2).

fn spec(parent: usize, geom: (i16, i16, u16, u16), border: u16) -> Spec {
    Spec {
        parent,
        geom,
        border,
        input_only: false,
        mapped: true,
        redirect: None,
        bounding: None,
        clip: None,
    }
}

const ROOT: usize = usize::MAX;

/// A redirected child under a higher sibling and one its parent clips
/// away, each Automatic and Manual, with and without borders.
#[test]
fn redirected_children_match_the_oracle() {
    for mode in [
        CompositeRedirectMode::Automatic,
        CompositeRedirectMode::Manual,
    ] {
        for border in [0, 3] {
            // 0: a parent; 1: redirected child; 2: a sibling over it;
            // 3: a redirected child outside the parent; 4: its child;
            // 5: a sibling under them all.
            let mut redirected = spec(0, (4, 4, 20, 16), border);
            redirected.redirect = Some(mode);
            let mut outside = spec(0, (30, 30, 20, 20), border);
            outside.redirect = Some(mode);
            let specs = vec![
                spec(ROOT, (2, 2, 40, 30), border),
                spec(0, (0, 0, 40, 30), border),
                redirected,
                spec(0, (8, 6, 10, 10), border),
                outside,
                spec(4, (2, 2, 6, 6), border),
            ];
            let (mut state, ids) = build(&specs);
            assert_queries_match(&state, &ids);
            assert_change_matches(&mut state, ids[3], None, Some((BELOW, None)));
            assert_queries_match(&state, &ids);
            assert_change_matches(&mut state, ids[2], Some((-3, -2, 30, 20)), None);
            assert_change_matches(&mut state, ids[1], None, Some((ABOVE, None)));
            assert_unmap_matches(&mut state, ids[3]);
            assert_unmap_matches(&mut state, ids[1]);
            assert_queries_match(&state, &ids);
        }
    }
}

/// Bordered windows overlapping in a stack, and shaped siblings.
#[test]
fn bordered_and_shaped_siblings_match_the_oracle() {
    let mut shaped = spec(ROOT, (10, 10, 30, 20), 2);
    shaped.bounding = Some(vec![r(0, 0, 10, 10), r(20, 5, 10, 15), r(-2, 18, 40, 4)]);
    let mut clipped = spec(ROOT, (5, 20, 20, 20), 1);
    clipped.clip = Some(vec![r(2, 2, 8, 8)]);
    let specs = vec![
        spec(ROOT, (0, 0, 40, 30), 4),
        spec(ROOT, (20, 10, 30, 30), 2),
        shaped,
        clipped,
        spec(2, (3, 3, 10, 10), 1),
    ];
    let (mut state, ids) = build(&specs);
    assert_queries_match(&state, &ids);
    assert_change_matches(&mut state, ids[0], None, Some((ABOVE, None)));
    assert_change_matches(&mut state, ids[2], Some((12, 8, 30, 20)), None);
    assert_shape_matches(&mut state, ids[2], Some(vec![r(5, 5, 20, 10)]));
    assert_shape_matches(&mut state, ids[2], None);
    assert_unmap_matches(&mut state, ids[1]);
    assert_queries_match(&state, &ids);
}

/// A shape stored as the request path stores it (banded, bands with the
/// same spans not merged, so not canonical): the cached regions give the
/// clips the stored rects give, before and after a change.
#[test]
fn shapes_stored_by_the_request_path_match_the_oracle() {
    let specs = vec![
        spec(ROOT, (0, 0, 40, 30), 1),
        spec(ROOT, (10, 10, 30, 20), 2),
        spec(1, (3, 3, 10, 10), 1),
    ];
    let (mut state, ids) = build(&specs);
    let bands = vec![
        r(0, 0, 10, 5),
        r(0, 5, 10, 5),
        r(12, 0, 4, 10),
        r(-2, 12, 40, 4),
    ];
    assert!(crate::nested::set_shape_rects(
        &mut state,
        ids[1],
        KIND_BOUNDING,
        bands.clone()
    ));
    assert!(crate::nested::set_shape_rects(
        &mut state,
        ids[1],
        KIND_CLIP,
        vec![r(1, 1, 8, 3), r(1, 4, 8, 3), r(20, 1, 4, 6)]
    ));
    let shape = &state.shape_windows[&ids[1]];
    let stored = shape.bounding().unwrap();
    let canonical = crate::region::Region::from_rects(stored);
    assert_ne!(
        &canonical.to_rects(),
        stored,
        "the stored shape is meant to be non-canonical"
    );
    assert_eq!(shape.bounding_region(), Some(&canonical));
    assert_eq!(
        shape.clip_region(),
        Some(&crate::region::Region::from_rects(shape.clip().unwrap()))
    );
    assert_queries_match(&state, &ids);

    let new = super::TreeChange::begin(&state, ids[1], None).unwrap();
    let old = oracle::TreeChange::begin(&state, ids[1], None).unwrap();
    crate::nested::set_shape_rects(
        &mut state,
        ids[1],
        KIND_BOUNDING,
        vec![r(0, 0, 6, 6), r(0, 6, 6, 6), r(8, 2, 20, 2)],
    );
    assert_eq!(
        new_exposed(new, &state, None),
        old_exposed(old, &state, None)
    );
    assert_queries_match(&state, &ids);

    let new = super::TreeChange::begin(&state, ids[1], None).unwrap();
    let old = oracle::TreeChange::begin(&state, ids[1], None).unwrap();
    crate::nested::clear_shape_rects(&mut state, ids[1], KIND_BOUNDING);
    assert_eq!(state.shape_windows[&ids[1]].bounding_region(), None);
    assert_eq!(
        new_exposed(new, &state, None),
        old_exposed(old, &state, None)
    );
    assert_queries_match(&state, &ids);
}

// Random trees.

fn shape() -> impl Strategy<Value = Option<Rects>> {
    prop_oneof![
        3 => Just(None),
        1 => prop::collection::vec((-3i16..20, -3i16..20, 0u16..14, 0u16..14), 0..4)
            .prop_map(|v| Some(v.into_iter().map(|(x, y, w, h)| r(x, y, w, h)).collect())),
    ]
}

fn specs() -> impl Strategy<Value = Vec<Spec>> {
    let one = (
        (0usize..12, prop::bool::weighted(0.3)),
        (-8i16..50, -8i16..40, 1u16..36, 1u16..30),
        prop_oneof![3 => Just(0u16), 1 => 1u16..4],
        prop::bool::weighted(0.1),
        prop::bool::weighted(0.85),
        prop_oneof![
            4 => Just(None),
            1 => Just(Some(CompositeRedirectMode::Automatic)),
            1 => Just(Some(CompositeRedirectMode::Manual)),
        ],
        shape(),
        shape(),
    )
        .prop_map(
            |((parent, at_root), geom, border, input_only, mapped, redirect, bounding, clip)| {
                Spec {
                    parent: if at_root { ROOT } else { parent },
                    geom,
                    border,
                    input_only,
                    mapped,
                    redirect,
                    bounding,
                    clip,
                }
            },
        );
    prop::collection::vec(one, 1..12).prop_map(|mut v| {
        // A parent is created before its children.
        for (i, s) in v.iter_mut().enumerate() {
            if s.parent >= i {
                s.parent = ROOT;
            }
        }
        v
    })
}

#[derive(Debug, Clone)]
enum Op {
    Configure {
        pick: usize,
        geom: Option<(i16, i16, u16, u16)>,
        stack: Option<(u8, Option<usize>)>,
    },
    Unmap(usize),
    Reshape(usize, Option<Rects>),
}

fn ops() -> impl Strategy<Value = Vec<Op>> {
    let op = prop_oneof![
        3 => (
            0usize..12,
            prop::option::of((-8i16..50, -8i16..40, 1u16..36, 1u16..30)),
            prop::option::of((0u8..2, prop::option::of(0usize..12))),
        )
            .prop_map(|(pick, geom, stack)| Op::Configure { pick, geom, stack }),
        1 => (0usize..12).prop_map(Op::Unmap),
        1 => (0usize..12, shape()).prop_map(|(p, s)| Op::Reshape(p, s)),
    ];
    prop::collection::vec(op, 0..4)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    #[test]
    fn random_trees_match_the_oracle(specs in specs(), ops in ops()) {
        let (mut state, ids) = build(&specs);
        assert_queries_match(&state, &ids);
        for op in ops {
            match op {
                Op::Configure { pick, geom, stack } => {
                    let w = ids[pick % ids.len()];
                    // A sibling, if the pick names one of the same parent.
                    let parent = state.resources.window(w).unwrap().parent;
                    let stack = stack.map(|(mode, sib)| {
                        let sib = sib
                            .map(|s| ids[s % ids.len()])
                            .filter(|s| *s != w && state.resources.window(*s)
                                .is_some_and(|win| win.parent == parent));
                        (mode, sib)
                    });
                    assert_change_matches(&mut state, w, geom, stack);
                }
                Op::Unmap(pick) => {
                    let w = ids[pick % ids.len()];
                    if state.resources.window(w).unwrap().map_state == MapState::Viewable {
                        assert_unmap_matches(&mut state, w);
                    }
                }
                Op::Reshape(pick, bounding) => {
                    assert_shape_matches(&mut state, ids[pick % ids.len()], bounding);
                }
            }
            assert_queries_match(&state, &ids);
        }
    }
}
