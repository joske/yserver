use proptest::prelude::*;
use yserver_protocol::x11::xfixes::RegionRect;

use super::*;
use crate::{core_loop::clip_list::canonical, nested};

fn r(x: i16, y: i16, width: u16, height: u16) -> RegionRect {
    RegionRect {
        x,
        y,
        width,
        height,
    }
}

/// Rects on a small grid, so that edges coincide often, including empty
/// and negative ones, in arbitrary order and overlapping.
fn rects() -> impl Strategy<Value = Vec<RegionRect>> {
    prop::collection::vec((-3i16..9, -3i16..9, 0u16..7, 0u16..7), 0..7)
        .prop_map(|v| v.into_iter().map(|(x, y, w, h)| r(x, y, w, h)).collect())
}

const GRID: std::ops::Range<i32> = -3..16;

/// The grid cells `rects` cover: an oracle independent of both
/// implementations.
fn raster(rects: &[RegionRect]) -> Vec<bool> {
    let mut cells = Vec::new();
    for y in GRID {
        for x in GRID {
            cells.push(rects.iter().any(|r| {
                let b = Box32::from_rect(*r);
                b.x1 <= x && x < b.x2 && b.y1 <= y && y < b.y2
            }));
        }
    }
    cells
}

/// The cells of `GRID` moved by `(bx, by)` that `boxes` cover.
fn raster_boxes(boxes: &[Box32], bx: i32, by: i32) -> Vec<bool> {
    let mut cells = Vec::new();
    for y in GRID {
        for x in GRID {
            let (x, y) = (x + bx, y + by);
            cells.push(
                boxes
                    .iter()
                    .any(|b| b.x1 <= x && x < b.x2 && b.y1 <= y && y < b.y2),
            );
        }
    }
    cells
}

/// Grid origins at the wire's limits as well as at zero.
fn bases() -> impl Strategy<Value = i32> {
    prop_oneof![
        Just(0),
        Just(i32::from(i16::MIN) + 3),
        Just(i32::from(i16::MAX) - 8),
        Just(i32::from(i16::MAX) - 2),
    ]
}

fn wire(v: i32) -> i16 {
    v.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

fn check(
    a: &[RegionRect],
    b: &[RegionRect],
    new: &Region,
    old: Vec<RegionRect>,
    cell: fn(bool, bool) -> bool,
) {
    let got = new.to_rects();
    assert!(is_canonical(&new.boxes), "not canonical: {got:?}");
    let want_cells: Vec<bool> = raster(a)
        .into_iter()
        .zip(raster(b))
        .map(|(p, q)| cell(p, q))
        .collect();
    assert_eq!(raster(&got), want_cells, "a={a:?} b={b:?} got={got:?}");
    assert_eq!(got, canonical(old), "a={a:?} b={b:?}");
}

proptest! {
    #[test]
    fn union_matches_the_old_ops_and_the_raster(a in rects(), b in rects()) {
        let new = Region::from_rects(&a).union(&Region::from_rects(&b));
        check(&a, &b, &new, nested::union_regions(&canonical(a.clone()), &b), |p, q| p || q);
    }

    #[test]
    fn intersect_matches_the_old_ops_and_the_raster(a in rects(), b in rects()) {
        let new = Region::from_rects(&a).intersect(&Region::from_rects(&b));
        check(&a, &b, &new, nested::intersect_regions(&a, &b), |p, q| p && q);
    }

    #[test]
    fn subtract_matches_the_old_ops_and_the_raster(a in rects(), b in rects()) {
        let new = Region::from_rects(&a).subtract(&Region::from_rects(&b));
        check(&a, &b, &new, nested::subtract_regions(&a, &b), |p, q| p && !q);
    }

    /// A chain of operations on canonical regions stays canonical without
    /// ever normalising.
    #[test]
    fn chained_operations_stay_canonical(a in rects(), b in rects(), c in rects()) {
        let (ra, rb, rc) = (Region::from_rects(&a), Region::from_rects(&b), Region::from_rects(&c));
        let new = ra.union(&rb).subtract(&rc).intersect(&ra.union(&rc));
        let want: Vec<bool> = raster(&a).iter().zip(raster(&b)).zip(raster(&c))
            .map(|((p, q), s)| (*p || q) && !s && (*p || s))
            .collect();
        prop_assert!(is_canonical(&new.boxes));
        prop_assert_eq!(raster(&new.to_rects()), want);
    }

    /// Arbitrary input becomes canonical boxes covering exactly its pixels,
    /// and, in the wire's range, the same rects as the wire normaliser.
    #[test]
    fn from_rects_normalises_to_the_raster(a in rects()) {
        let region = Region::from_rects(&a);
        prop_assert!(is_canonical(&region.boxes), "not canonical: {:?}", region.boxes);
        prop_assert_eq!(raster_boxes(&region.boxes, 0, 0), raster(&a));
        prop_assert_eq!(region.to_rects(), canonical(a));
    }

    /// The same near the wire's limits, where boxes end past `i16::MAX`:
    /// checked on the internal boxes, which are never clamped.
    #[test]
    fn from_rects_keeps_pixels_past_the_wire_limits(
        (bx, by) in (bases(), bases()),
        offsets in prop::collection::vec((-3i32..9, -3i32..9, 0u16..7, 0u16..7), 0..7),
    ) {
        let a: Vec<RegionRect> = offsets
            .into_iter()
            .map(|(x, y, w, h)| r(wire(bx + x), wire(by + y), w, h))
            .collect();
        let region = Region::from_rects(&a);
        prop_assert!(is_canonical(&region.boxes), "not canonical: {:?}", region.boxes);
        let rects: Vec<Box32> = a.iter().map(|r| Box32::from_rect(*r)).collect();
        prop_assert_eq!(raster_boxes(&region.boxes, bx, by), raster_boxes(&rects, bx, by), "a={:?}", a);
    }

    #[test]
    fn canonical_rects_are_adopted_as_they_are(a in rects()) {
        let c = canonical(a);
        prop_assert!(is_canonical(&Region::from_rects(&c).boxes));
        prop_assert_eq!(Region::from_rects(&c).to_rects(), c);
    }
}

/// pixman merges bands whose spans agree, and a hole splits a rect into a
/// top band, two sides and a bottom band.
#[test]
fn coalescing_follows_pixman() {
    let a = Region::from_rects(&[r(0, 0, 10, 5)]);
    let b = Region::from_rects(&[r(0, 5, 10, 5)]);
    assert_eq!(a.union(&b).to_rects(), vec![r(0, 0, 10, 10)]);
    let whole = Region::from_rects(&[r(0, 0, 10, 10)]);
    let hole = Region::from_rects(&[r(3, 3, 4, 4)]);
    assert_eq!(
        whole.subtract(&hole).to_rects(),
        vec![r(0, 0, 10, 3), r(0, 3, 3, 4), r(7, 3, 3, 4), r(0, 7, 10, 3)]
    );
    assert_eq!(whole.subtract(&hole).union(&hole), whole);
}

/// Borders left of and above the parent, extents at the wire's limits:
/// the same rects as the old operations, as long as no piece the old
/// operations cut off starts past `i16::MAX` (see
/// [`a_hole_past_the_wire_origin_limit_is_kept`]).
#[test]
fn wire_limits_match_the_old_operations() {
    let cases = [
        (vec![r(-5, -5, 20, 20)], vec![r(-10, 0, 8, 3)]),
        (
            vec![r(i16::MIN, i16::MIN, u16::MAX, u16::MAX)],
            vec![r(0, 0, 10, 10)],
        ),
        (
            vec![r(i16::MAX - 4, 0, 100, 10)],
            vec![r(i16::MAX - 2, 2, 1, 1)],
        ),
        (
            vec![r(0, 0, i16::MAX as u16, 1)],
            vec![r(100, 0, 1, 1), r(i16::MAX - 5, 0, 5, 1)],
        ),
        (vec![r(0, 0, 0, 5)], vec![r(0, 0, 5, 5)]),
    ];
    for (a, b) in cases {
        let (ra, rb) = (Region::from_rects(&a), Region::from_rects(&b));
        assert_eq!(
            ra.subtract(&rb).to_rects(),
            canonical(nested::subtract_regions(&a, &b)),
            "subtract {a:?} {b:?}"
        );
        assert_eq!(
            ra.intersect(&rb).to_rects(),
            canonical(nested::intersect_regions(&a, &b)),
            "intersect {a:?} {b:?}"
        );
    }
}

/// A hole starting past `i16::MAX` stays a hole in the region; only the
/// rects sent out are clamped to the wire.
#[test]
fn a_hole_past_the_wire_origin_limit_is_kept() {
    let a = Region::from_rects(&[r(0, 0, u16::MAX, 1)]);
    let b = Region::from_rects(&[r(100, 0, 1, 1), r(i16::MAX, 0, 5, 1)]);
    let b32 = |x1, x2| Box32 {
        x1,
        y1: 0,
        x2,
        y2: 1,
    };
    // The old operations lost this hole by clamping mid-operation.
    assert_eq!(
        a.subtract(&b).boxes,
        vec![b32(0, 100), b32(101, 32767), b32(32772, 65535)]
    );
}

#[test]
fn empty_regions() {
    let e = Region::default();
    let a = Region::from_rects(&[r(1, 1, 2, 2)]);
    assert_eq!(e.union(&a), a);
    assert_eq!(a.union(&e), a);
    assert!(e.intersect(&a).is_empty());
    assert!(a.intersect(&e).is_empty());
    assert_eq!(a.subtract(&e), a);
    assert!(e.subtract(&a).is_empty());
    assert!(Region::from_rects(&[r(0, 0, 0, 0)]).is_empty());
}

/// Input whose pieces start past `i16::MAX` keeps every pixel and stays
/// canonical: the constructor never goes through the wire's range.
#[test]
fn arbitrary_input_keeps_internal_bands_above_wire_origin_limit() {
    let input = [r(0, 32760, 10, 20), r(20, 32767, 10, 10)];
    let region = Region::from_rects(&input);
    for y in 32760..32780 {
        for x in 0..30 {
            let want = input.iter().any(|r| {
                let b = Box32::from_rect(*r);
                b.x1 <= x && x < b.x2 && b.y1 <= y && y < b.y2
            });
            let got = region
                .boxes
                .iter()
                .any(|b| b.x1 <= x && x < b.x2 && b.y1 <= y && y < b.y2);
            assert_eq!(
                got, want,
                "constructor changed the pixel set at ({x},{y}): {:?}",
                region.boxes
            );
        }
    }
    assert!(
        is_canonical(&region.boxes),
        "constructor must always return canonical boxes"
    );
}
