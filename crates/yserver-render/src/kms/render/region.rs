//! Y-X banded region with real geometric set algebra.
//!
//! Step 0 of
//! `docs/superpowers/plans/2026-09-01-damage-derived-scene-repaint-plan.md`.
//!
//! # Why this exists alongside `RegionSet`
//!
//! [`super::store::RegionSet`] is a `Vec<Rect2D>` whose `subtract` removes
//! **exact rect matches** as a multiset. That is deliberate and correct where it
//! is used: on the drawable snapshot/ack path an identical damage rect can
//! legitimately arrive twice while the first snapshot is in flight, and
//! geometric subtraction would drop the newer one.
//!
//! It cannot express the per-BO damage bookkeeping, which needs
//! `missing[bo] -= painted` — geometric subtraction of one region from another.
//! Used there, exact-match subtract would match nothing, `missing` would grow to
//! its rect cap, collapse to extents, and pin every frame to a full repaint for
//! ever: safe, silent, and useless.
//!
//! So both types exist, with a clean split: `RegionSet` for presentation damage,
//! `Region` for the scene and per-BO damage state.
//!
//! # Representation
//!
//! A canonical y-x banded list of half-open boxes, held to three invariants:
//!
//! 1. sorted by `(y0, x0)`;
//! 2. boxes are pairwise disjoint, and within one band (identical `y0`/`y1`) no
//!    two boxes touch — `a.x1 < b.x0` strictly;
//! 3. two vertically adjacent bands never carry an identical x-span list, so
//!    such bands are merged into one.
//!
//! Canonical form makes [`Region::area`] a plain sum (boxes are disjoint) and
//! makes equality of two regions equality of their box lists.
//!
//! # Algorithm
//!
//! Set operations decompose on y rather than merging bands incrementally the way
//! pixman does. For a binary op the distinct y edges of both operands are
//! collected, and in each resulting y-slice the operands' x-spans are combined
//! with a 1-D interval op; adjacent slices with identical spans are then merged.
//!
//! That is `O(n·m)` in box counts against pixman's `O(n+m)`, which is the right
//! trade at this size: [`Region::MAX_RECTS`] is 32, the operands are damage
//! regions of a handful of boxes, and 1-D interval algebra is far easier to get
//! right — and to test exhaustively — than incremental band merging. The cost is
//! bounded and constant; a correctness bug here is indistinguishable from a
//! damage bug on screen.
//!
//! # Partially wired
//!
//! Step 3 is the first consumer — `scanout_damage.rs` and the tick's damage
//! feed. The clipping and occlusion operations land with steps 4 and 1, so some
//! items are still dead outside the tests.
//! `allow`, not `expect`: once the module is public API of the
//! `yserver-render` crate, rustc no longer reports its items as dead and an
//! `expect` would be unfulfilled. Gated on `not(test)` because the tests below
//! do exercise every item.

#![cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "the clipping and occlusion operations land with steps 4 and 1"
    )
)]

use ash::vk;

/// Half-open box in output-local coordinates: `[x0, x1) × [y0, y1)`.
///
/// Half-open avoids the off-by-one that plagues inclusive-bound rectangle
/// algebra, and every conversion to and from [`vk::Rect2D`] happens at this
/// module's edge.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Box2D {
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
}

impl Box2D {
    fn from_rect(r: vk::Rect2D) -> Option<Self> {
        if r.extent.width == 0 || r.extent.height == 0 {
            return None;
        }
        let x1 = r.offset.x.saturating_add_unsigned(r.extent.width);
        let y1 = r.offset.y.saturating_add_unsigned(r.extent.height);
        if x1 <= r.offset.x || y1 <= r.offset.y {
            return None;
        }
        Some(Self {
            x0: r.offset.x,
            y0: r.offset.y,
            x1,
            y1,
        })
    }

    fn to_rect(self) -> vk::Rect2D {
        vk::Rect2D {
            offset: vk::Offset2D {
                x: self.x0,
                y: self.y0,
            },
            extent: vk::Extent2D {
                width: u32::try_from(self.x1 - self.x0).unwrap_or(0),
                height: u32::try_from(self.y1 - self.y0).unwrap_or(0),
            },
        }
    }
}

/// Which way [`Region::combine`] merges two operands' x-spans.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Op {
    Union,
    Subtract,
    Intersect,
}

/// A canonical y-x banded region. See the module docs.
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct Region {
    boxes: Vec<Box2D>,
}

impl Region {
    /// Cap on retained box count. Past this the region collapses to its bounding
    /// box.
    ///
    /// **The cap is only safe in one direction.** Collapsing a region that will
    /// be *added* over-damages, which costs pixels. Collapsing a region that
    /// will be *subtracted* over-subtracts, which leaves stale pixels on screen.
    /// So a capped region may only be subtracted from `missing` when that
    /// bounding box was itself painted — which is exactly what the plan's
    /// `painted` region guarantees, and why `painted` and not the requested
    /// repaint region is what retirement subtracts.
    ///
    /// 32 rather than [`super::store::RegionSet`]'s 256 because this feeds a
    /// scissor list, not a damage log.
    pub const MAX_RECTS: usize = 32;

    pub fn new() -> Self {
        Self { boxes: Vec::new() }
    }

    pub fn from_rect(rect: vk::Rect2D) -> Self {
        Self {
            boxes: Box2D::from_rect(rect).into_iter().collect(),
        }
    }

    /// Build from any rect sequence — the conversion boundary from
    /// [`super::store::RegionSet`], whose rects may overlap and touch. The
    /// result is canonical regardless.
    pub fn from_rects<I: IntoIterator<Item = vk::Rect2D>>(rects: I) -> Self {
        let mut out = Self::new();
        for r in rects {
            out.add_rect(r);
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.boxes.is_empty()
    }

    pub(crate) fn clear(&mut self) {
        self.boxes.clear();
    }

    pub fn rect_count(&self) -> usize {
        self.boxes.len()
    }

    /// The region's boxes, in canonical order — sorted by `(y, x)`, pairwise
    /// disjoint. Disjointness is what lets a caller use these directly as a
    /// scissor list without painting any pixel twice.
    pub fn rects(&self) -> impl Iterator<Item = vk::Rect2D> + '_ {
        self.boxes.iter().copied().map(Box2D::to_rect)
    }

    /// Total covered pixels. Exact, because canonical boxes do not overlap.
    pub fn area(&self) -> u64 {
        self.boxes
            .iter()
            .map(|b| {
                let w = u64::from(u32::try_from(b.x1 - b.x0).unwrap_or(0));
                let h = u64::from(u32::try_from(b.y1 - b.y0).unwrap_or(0));
                w * h
            })
            .sum()
    }

    pub fn bounding_rect(&self) -> Option<vk::Rect2D> {
        let mut it = self.boxes.iter().copied();
        let first = it.next()?;
        let bounds = it.fold(first, |a, b| Box2D {
            x0: a.x0.min(b.x0),
            y0: a.y0.min(b.y0),
            x1: a.x1.max(b.x1),
            y1: a.y1.max(b.y1),
        });
        Some(bounds.to_rect())
    }

    pub fn add_rect(&mut self, rect: vk::Rect2D) {
        if let Some(b) = Box2D::from_rect(rect) {
            self.merge(&Self { boxes: vec![b] }, Op::Union);
        }
    }

    pub fn union_with(&mut self, other: &Region) {
        if other.is_empty() {
            return;
        }
        self.merge(other, Op::Union);
    }

    /// Geometric subtraction: remove every pixel of `other` from `self`.
    ///
    /// Unlike [`super::store::RegionSet::subtract`] this is a true set
    /// difference, not a multiset removal of matching rects.
    pub fn subtract(&mut self, other: &Region) {
        if self.is_empty() || other.is_empty() {
            return;
        }
        self.merge(other, Op::Subtract);
    }

    pub(crate) fn intersect_with(&mut self, other: &Region) {
        if self.is_empty() {
            return;
        }
        if other.is_empty() {
            self.clear();
            return;
        }
        self.merge(other, Op::Intersect);
    }

    pub fn intersect_rect(&mut self, rect: vk::Rect2D) {
        *self = self.clip_to_rect(rect);
    }

    /// `self ∩ rect`, without the slice decomposition.
    ///
    /// Clipping a canonical region to one rectangle keeps it canonical except
    /// for one thing: two bands that differed only in x-spans outside the rect
    /// now have identical spans and must merge vertically (invariant 3). Boxes
    /// within a band stay disjoint (clipping never closes a gap) and the band
    /// order is preserved, so a single linear pass restores the invariant.
    /// The box count never grows, so the cap cannot fire. Tested against
    /// [`Self::intersect_with`] on randomised inputs; the two are equal.
    ///
    /// This is the visibility walk's hot operation (`universe ∩ place rect`,
    /// several times per node), which is why it avoids `combine`'s allocations.
    pub fn clip_to_rect(&self, rect: vk::Rect2D) -> Region {
        let Some(q) = Box2D::from_rect(rect) else {
            return Region::new();
        };
        let mut out: Vec<Box2D> = Vec::with_capacity(self.boxes.len());
        // Start index in `out` of the most recently emitted band.
        let mut prev_band: Option<usize> = None;
        let mut i = 0;
        while i < self.boxes.len() {
            // One input band: the run of boxes sharing (y0, y1).
            let (by0, by1) = (self.boxes[i].y0, self.boxes[i].y1);
            let mut j = i;
            while j < self.boxes.len() && self.boxes[j].y0 == by0 && self.boxes[j].y1 == by1 {
                j += 1;
            }
            let (y0, y1) = (by0.max(q.y0), by1.min(q.y1));
            if y0 < y1 {
                let band_start = out.len();
                for b in &self.boxes[i..j] {
                    let (x0, x1) = (b.x0.max(q.x0), b.x1.min(q.x1));
                    if x0 < x1 {
                        out.push(Box2D { x0, y0, x1, y1 });
                    }
                }
                if out.len() == band_start {
                    // Whole band clipped away in x.
                } else if let Some(p) = prev_band
                    && out[p].y1 == y0
                    && out.len() - band_start == band_start - p
                    && out[p..band_start]
                        .iter()
                        .zip(&out[band_start..])
                        .all(|(a, b)| a.x0 == b.x0 && a.x1 == b.x1)
                {
                    // Same spans, contiguous in y: fold into the band above.
                    for b in &mut out[p..band_start] {
                        b.y1 = y1;
                    }
                    out.truncate(band_start);
                } else {
                    prev_band = Some(band_start);
                }
            }
            i = j;
        }
        Region { boxes: out }
    }

    /// True if any pixel of `rect` lies in the region.
    pub fn intersects_rect(&self, rect: vk::Rect2D) -> bool {
        let Some(q) = Box2D::from_rect(rect) else {
            return false;
        };
        self.boxes
            .iter()
            .any(|b| b.x0 < q.x1 && q.x0 < b.x1 && b.y0 < q.y1 && q.y0 < b.y1)
    }

    /// True if any pixel is in both regions.
    ///
    /// A named predicate rather than clone-intersect-`is_empty`, because that
    /// idiom is what the step-3 retirement assertion would otherwise be written
    /// as, and it is easy to get subtly wrong at a call site.
    pub(crate) fn intersects(&self, other: &Region) -> bool {
        self.boxes
            .iter()
            .any(|b| other.intersects_rect(b.to_rect()))
    }

    /// True if every pixel of `other` lies in the region.
    ///
    /// This is what `ScanoutDamage` asserts at submit: what was *painted* must
    /// cover what was asked to be repainted. Painting less than the repaint
    /// region while recording it as painted is the mistake that leaves stale
    /// pixels and clears `missing` for them anyway.
    pub fn contains(&self, other: &Region) -> bool {
        let mut remainder = other.clone();
        remainder.subtract(self);
        remainder.is_empty()
    }

    /// True if every pixel of `rect` lies in the region.
    ///
    /// This is the opaque-cover guard's primitive: a clipped repaint is only
    /// safe where the repaint region is fully covered by an opaque draw.
    pub fn contains_rect(&self, rect: vk::Rect2D) -> bool {
        let Some(q) = Box2D::from_rect(rect) else {
            return true;
        };
        let mut remainder = Self { boxes: vec![q] };
        remainder.subtract(self);
        remainder.is_empty()
    }

    fn merge(&mut self, other: &Region, op: Op) {
        self.merge_reporting(other, op);
    }

    /// Like [`Self::merge`], and reports whether the cap collapsed the result to
    /// its bounding box. The result is identical either way; the flag exists
    /// because step 1's visibility walk must know when a region it holds has
    /// become a superset — safe for a universe, unsafe for a claim.
    fn merge_reporting(&mut self, other: &Region, op: Op) -> bool {
        let combined = Self::combine(self, other, op);
        let collapsed = combined.boxes.len() > Self::MAX_RECTS;
        *self = combined;
        self.enforce_cap();
        collapsed
    }

    /// [`Self::union_with`] that also says whether the cap collapsed the
    /// result. See [`Self::merge_reporting`].
    pub fn union_with_reporting(&mut self, other: &Region) -> bool {
        if other.is_empty() {
            return false;
        }
        self.merge_reporting(other, Op::Union)
    }

    /// [`Self::subtract`] that also says whether the cap collapsed the result.
    /// A collapsed remainder is a **superset** of the true remainder, so a
    /// caller about to claim that remainder as covered must not.
    pub fn subtract_reporting(&mut self, other: &Region) -> bool {
        if self.is_empty() || other.is_empty() {
            return false;
        }
        self.merge_reporting(other, Op::Subtract)
    }

    /// Collapse to the bounding box once the box count exceeds the cap. A safe
    /// superset for damage; see [`Self::MAX_RECTS`] for the direction in which
    /// it is *not* safe.
    fn enforce_cap(&mut self) {
        if self.boxes.len() <= Self::MAX_RECTS {
            return;
        }
        if let Some(bounds) = self.bounding_rect().and_then(Box2D::from_rect) {
            self.boxes.clear();
            self.boxes.push(bounds);
        }
    }

    /// Y-slice decomposition. See the module docs for why this shape was chosen
    /// over incremental band merging.
    ///
    /// Buffers for the per-slice spans are allocated once and reused across
    /// slices; the algorithm is unchanged.
    fn combine(a: &Region, b: &Region, op: Op) -> Region {
        // Distinct y edges of both operands, ascending. Every box boundary in
        // the result falls on one of these, so each slice between consecutive
        // edges has constant x-spans in both operands.
        let mut ys: Vec<i32> = Vec::with_capacity((a.boxes.len() + b.boxes.len()) * 2);
        for r in a.boxes.iter().chain(b.boxes.iter()) {
            ys.push(r.y0);
            ys.push(r.y1);
        }
        ys.sort_unstable();
        ys.dedup();

        let mut boxes: Vec<Box2D> = Vec::with_capacity(a.boxes.len() + b.boxes.len());
        // x-spans of the slice most recently emitted, for the vertical merge.
        let mut prev_spans: Vec<(i32, i32)> = Vec::new();
        let mut prev_y: Option<(i32, i32)> = None;
        let mut spans_a: Vec<(i32, i32)> = Vec::new();
        let mut spans_b: Vec<(i32, i32)> = Vec::new();
        let mut spans: Vec<(i32, i32)> = Vec::new();

        for pair in ys.windows(2) {
            let (ys0, ys1) = (pair[0], pair[1]);
            a.spans_in_slice(ys0, ys1, &mut spans_a);
            b.spans_in_slice(ys0, ys1, &mut spans_b);
            match op {
                Op::Union => union_1d(&spans_a, &spans_b, &mut spans),
                Op::Subtract => subtract_1d(&spans_a, &spans_b, &mut spans),
                Op::Intersect => intersect_1d(&spans_a, &spans_b, &mut spans),
            }

            // Invariant 3: fold this slice into the band above when the spans
            // match and the two are contiguous in y.
            if let Some((py0, py1)) = prev_y
                && py1 == ys0
                && prev_spans == spans
            {
                for bx in boxes.iter_mut().rev().take(spans.len()) {
                    bx.y1 = ys1;
                }
                prev_y = Some((py0, ys1));
                continue;
            }

            if spans.is_empty() {
                prev_spans.clear();
                prev_y = None;
                continue;
            }
            for &(x0, x1) in &spans {
                boxes.push(Box2D {
                    x0,
                    y0: ys0,
                    x1,
                    y1: ys1,
                });
            }
            std::mem::swap(&mut prev_spans, &mut spans);
            prev_y = Some((ys0, ys1));
        }

        Region { boxes }
    }

    /// x-spans covering the y-slice `[ys0, ys1)`, ascending and disjoint,
    /// written into `out`.
    ///
    /// Relies on the canonical invariants: bands are disjoint in y, so a box
    /// either spans the whole slice or misses it entirely, and boxes are already
    /// sorted by x within a band.
    fn spans_in_slice(&self, ys0: i32, ys1: i32, out: &mut Vec<(i32, i32)>) {
        out.clear();
        out.extend(
            self.boxes
                .iter()
                .filter(|b| b.y0 <= ys0 && b.y1 >= ys1)
                .map(|b| (b.x0, b.x1)),
        );
        out.sort_unstable();
        coalesce_1d(out);
    }
}

/// Merge overlapping and touching intervals in a sorted list, in place.
/// Touching intervals must merge, or invariant 2 breaks and the vertical-merge
/// comparison in [`Region::combine`] stops recognising identical bands.
fn coalesce_1d(v: &mut Vec<(i32, i32)>) {
    if v.len() < 2 {
        return;
    }
    let mut w = 0usize;
    for i in 1..v.len() {
        let (s, e) = v[i];
        if s <= v[w].1 {
            v[w].1 = v[w].1.max(e);
        } else {
            w += 1;
            v[w] = (s, e);
        }
    }
    v.truncate(w + 1);
}

fn union_1d(a: &[(i32, i32)], b: &[(i32, i32)], out: &mut Vec<(i32, i32)>) {
    out.clear();
    out.extend_from_slice(a);
    out.extend_from_slice(b);
    out.sort_unstable();
    coalesce_1d(out);
}

fn intersect_1d(a: &[(i32, i32)], b: &[(i32, i32)], out: &mut Vec<(i32, i32)>) {
    out.clear();
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        let s = a[i].0.max(b[j].0);
        let e = a[i].1.min(b[j].1);
        if s < e {
            out.push((s, e));
        }
        if a[i].1 < b[j].1 {
            i += 1;
        } else {
            j += 1;
        }
    }
    coalesce_1d(out);
}

/// `a - b`, both sorted and disjoint.
fn subtract_1d(a: &[(i32, i32)], b: &[(i32, i32)], out: &mut Vec<(i32, i32)>) {
    out.clear();
    let mut bi = 0usize;
    for &(start, end) in a {
        let mut s = start;
        // `b` is sorted, and `a`'s intervals ascend, so intervals of `b` that
        // end at or before this one's start can never matter again.
        while bi < b.len() && b[bi].1 <= s {
            bi += 1;
        }
        let mut j = bi;
        while j < b.len() && b[j].0 < end {
            let (bs, be) = b[j];
            if bs > s {
                out.push((s, bs.min(end)));
            }
            s = s.max(be);
            if s >= end {
                break;
            }
            j += 1;
        }
        if s < end {
            out.push((s, end));
        }
    }
    coalesce_1d(out);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn r(x: i32, y: i32, w: u32, h: u32) -> vk::Rect2D {
        vk::Rect2D {
            offset: vk::Offset2D { x, y },
            extent: vk::Extent2D {
                width: w,
                height: h,
            },
        }
    }

    fn region(rects: &[vk::Rect2D]) -> Region {
        let mut g = Region::new();
        for &rect in rects {
            g.add_rect(rect);
        }
        g
    }

    // ── the oracle ───────────────────────────────────────────────────
    //
    // Every set operation is checked against brute-force pixel sets rather than
    // against hand-written expected box lists. Hand-written expectations are how
    // a wrong region implementation gets locked in as "correct": the
    // implementation and the expectation come from the same misunderstanding.

    fn pixels(g: &Region) -> BTreeSet<(i32, i32)> {
        let mut s = BTreeSet::new();
        for rect in g.rects() {
            for y in rect.offset.y..rect.offset.y + rect.extent.height as i32 {
                for x in rect.offset.x..rect.offset.x + rect.extent.width as i32 {
                    // A duplicate here means the region is not disjoint.
                    assert!(s.insert((x, y)), "overlapping boxes at ({x},{y})");
                }
            }
        }
        s
    }

    fn pixels_of(rects: &[vk::Rect2D]) -> BTreeSet<(i32, i32)> {
        let mut s = BTreeSet::new();
        for rect in rects {
            for y in rect.offset.y..rect.offset.y + rect.extent.height as i32 {
                for x in rect.offset.x..rect.offset.x + rect.extent.width as i32 {
                    s.insert((x, y));
                }
            }
        }
        s
    }

    /// Assert the three canonical invariants directly, not just the pixel set.
    /// A region can cover the right pixels while being non-canonical, and the
    /// vertical-merge comparison in `combine` silently degrades if it is.
    fn assert_canonical(g: &Region) {
        let b = &g.boxes;
        for w in b.windows(2) {
            assert!(
                (w[0].y0, w[0].x0) < (w[1].y0, w[1].x0),
                "not sorted by (y, x): {:?} then {:?}",
                w[0],
                w[1]
            );
        }
        for (i, p) in b.iter().enumerate() {
            for q in &b[i + 1..] {
                assert!(
                    !(p.x0 < q.x1 && q.x0 < p.x1 && p.y0 < q.y1 && q.y0 < p.y1),
                    "boxes overlap: {p:?} and {q:?}"
                );
            }
        }
        // Within a band, no two boxes may touch.
        for w in b.windows(2) {
            if w[0].y0 == w[1].y0 && w[0].y1 == w[1].y1 {
                assert!(
                    w[0].x1 < w[1].x0,
                    "touching boxes in one band: {:?} and {:?}",
                    w[0],
                    w[1]
                );
            }
        }
        // Vertically adjacent bands must not carry identical x-spans.
        // (y0, y1, x-spans) per band.
        type Band = (i32, i32, Vec<(i32, i32)>);
        let mut bands: Vec<Band> = Vec::new();
        for bx in b {
            match bands.last_mut() {
                Some((y0, y1, spans)) if *y0 == bx.y0 && *y1 == bx.y1 => {
                    spans.push((bx.x0, bx.x1));
                }
                _ => bands.push((bx.y0, bx.y1, vec![(bx.x0, bx.x1)])),
            }
        }
        for w in bands.windows(2) {
            assert!(
                !(w[0].1 == w[1].0 && w[0].2 == w[1].2),
                "adjacent bands with identical spans were not merged: {:?} {:?}",
                w[0],
                w[1]
            );
        }
    }

    // ── construction ─────────────────────────────────────────────────

    #[test]
    fn empty_region_is_empty() {
        let g = Region::new();
        assert!(g.is_empty());
        assert_eq!(g.area(), 0);
        assert_eq!(g.bounding_rect(), None);
        assert_eq!(g.rect_count(), 0);
    }

    #[test]
    fn zero_extent_rects_are_dropped() {
        let mut g = Region::new();
        g.add_rect(r(5, 5, 0, 10));
        g.add_rect(r(5, 5, 10, 0));
        assert!(g.is_empty());
    }

    #[test]
    fn adding_the_same_rect_twice_is_idempotent() {
        let a = region(&[r(10, 10, 20, 20)]);
        let b = region(&[r(10, 10, 20, 20), r(10, 10, 20, 20)]);
        assert_eq!(a, b);
        assert_eq!(b.area(), 400);
        assert_canonical(&b);
    }

    #[test]
    fn touching_rects_merge_into_one_box() {
        // Two rects sharing an edge cover one rectangle; canonical form must say
        // so, or every later comparison sees two bands where there is one.
        let g = region(&[r(0, 0, 10, 10), r(10, 0, 10, 10)]);
        assert_canonical(&g);
        assert_eq!(g.rect_count(), 1);
        assert_eq!(g.area(), 200);
    }

    #[test]
    fn stacked_touching_rects_merge_vertically() {
        let g = region(&[r(0, 0, 10, 10), r(0, 10, 10, 10)]);
        assert_canonical(&g);
        assert_eq!(g.rect_count(), 1);
        assert_eq!(g.area(), 200);
    }

    // ── the ops, against the oracle ──────────────────────────────────

    #[test]
    fn union_matches_pixel_oracle() {
        let mut g = region(&[r(0, 0, 10, 10)]);
        g.union_with(&region(&[r(5, 5, 10, 10)]));
        assert_canonical(&g);
        assert_eq!(pixels(&g), pixels_of(&[r(0, 0, 10, 10), r(5, 5, 10, 10)]));
    }

    #[test]
    fn subtract_punches_a_hole() {
        let mut g = region(&[r(0, 0, 30, 30)]);
        g.subtract(&region(&[r(10, 10, 10, 10)]));
        assert_canonical(&g);
        let mut expect = pixels_of(&[r(0, 0, 30, 30)]);
        for (x, y) in pixels_of(&[r(10, 10, 10, 10)]) {
            expect.remove(&(x, y));
        }
        assert_eq!(pixels(&g), expect);
        assert_eq!(g.area(), 900 - 100);
    }

    #[test]
    fn self_subtract_is_empty() {
        let mut g = region(&[r(0, 0, 10, 10), r(20, 20, 5, 5), r(3, 40, 7, 2)]);
        let same = g.clone();
        g.subtract(&same);
        assert!(g.is_empty(), "left over: {:?}", g.boxes);
    }

    #[test]
    fn subtract_of_disjoint_region_changes_nothing() {
        let mut g = region(&[r(0, 0, 10, 10)]);
        let before = g.clone();
        g.subtract(&region(&[r(100, 100, 10, 10)]));
        assert_eq!(g, before);
    }

    #[test]
    fn subtract_that_splits_a_box_in_two() {
        // A vertical bar removed from the middle leaves two boxes in one band.
        let mut g = region(&[r(0, 0, 30, 10)]);
        g.subtract(&region(&[r(10, 0, 10, 10)]));
        assert_canonical(&g);
        assert_eq!(g.rect_count(), 2);
        assert_eq!(g.area(), 200);
    }

    #[test]
    fn intersect_matches_pixel_oracle() {
        let mut g = region(&[r(0, 0, 20, 20)]);
        g.intersect_with(&region(&[r(10, 10, 20, 20)]));
        assert_canonical(&g);
        assert_eq!(pixels(&g), pixels_of(&[r(10, 10, 10, 10)]));
    }

    #[test]
    fn intersect_with_empty_clears() {
        let mut g = region(&[r(0, 0, 20, 20)]);
        g.intersect_with(&Region::new());
        assert!(g.is_empty());
    }

    #[test]
    fn intersect_rect_clips() {
        let mut g = region(&[r(-10, -10, 40, 40)]);
        g.intersect_rect(r(0, 0, 20, 20));
        assert_eq!(pixels(&g), pixels_of(&[r(0, 0, 20, 20)]));
    }

    // ── predicates ───────────────────────────────────────────────────

    #[test]
    fn contains_rect_is_true_only_for_full_cover() {
        let g = region(&[r(0, 0, 10, 10), r(10, 0, 10, 10)]);
        assert!(g.contains_rect(r(0, 0, 20, 10)));
        assert!(g.contains_rect(r(5, 5, 5, 5)));
        assert!(!g.contains_rect(r(0, 0, 20, 11)));
        assert!(!g.contains_rect(r(19, 0, 2, 10)));
    }

    #[test]
    fn contains_rect_is_false_across_a_hole() {
        let mut g = region(&[r(0, 0, 30, 30)]);
        g.subtract(&region(&[r(10, 10, 10, 10)]));
        assert!(g.contains_rect(r(0, 0, 30, 10)));
        assert!(!g.contains_rect(r(0, 0, 30, 30)));
    }

    #[test]
    fn intersects_predicates() {
        let g = region(&[r(0, 0, 10, 10)]);
        assert!(g.intersects_rect(r(9, 9, 5, 5)));
        assert!(
            !g.intersects_rect(r(10, 0, 5, 5)),
            "edge-touching is disjoint"
        );
        assert!(g.intersects(&region(&[r(5, 5, 20, 20)])));
        assert!(!g.intersects(&region(&[r(50, 50, 5, 5)])));
        assert!(!g.intersects(&Region::new()));
    }

    // ── the cap ──────────────────────────────────────────────────────

    #[test]
    fn exceeding_the_cap_collapses_to_extents() {
        // A checkerboard of isolated boxes cannot be represented within the cap,
        // so the region must become a superset rather than lose pixels.
        let mut g = Region::new();
        for i in 0..40 {
            g.add_rect(r(i * 4, i * 4, 2, 2));
        }
        assert!(g.rect_count() <= Region::MAX_RECTS);
        assert_canonical(&g);
        // Superset, never a subset: every original pixel is still covered.
        for i in 0..40 {
            assert!(g.contains_rect(r(i * 4, i * 4, 2, 2)));
        }
    }

    // ── randomised differential test ─────────────────────────────────

    #[test]
    fn ops_match_the_oracle_over_random_regions() {
        // Deterministic LCG: reproducible failures matter more than entropy.
        let mut seed = 0x2026_0902_u64;
        let mut next = move || {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            (seed >> 33) as u32
        };
        for _case in 0..400 {
            let mut ra = Vec::new();
            let mut rb = Vec::new();
            for (list, count) in [(&mut ra, 3), (&mut rb, 3)] {
                for _ in 0..count {
                    let x = i32::try_from(next() % 12).unwrap();
                    let y = i32::try_from(next() % 12).unwrap();
                    let w = next() % 6 + 1;
                    let h = next() % 6 + 1;
                    list.push(r(x, y, w, h));
                }
            }
            let ga = region(&ra);
            let gb = region(&rb);
            assert_canonical(&ga);
            assert_canonical(&gb);

            let pa = pixels_of(&ra);
            let pb = pixels_of(&rb);

            let mut u = ga.clone();
            u.union_with(&gb);
            assert_canonical(&u);
            assert_eq!(
                pixels(&u),
                pa.union(&pb).copied().collect(),
                "union {ra:?} {rb:?}"
            );

            let mut d = ga.clone();
            d.subtract(&gb);
            assert_canonical(&d);
            assert_eq!(
                pixels(&d),
                pa.difference(&pb).copied().collect(),
                "subtract {ra:?} {rb:?}"
            );

            let mut i = ga.clone();
            i.intersect_with(&gb);
            assert_canonical(&i);
            assert_eq!(
                pixels(&i),
                pa.intersection(&pb).copied().collect(),
                "intersect {ra:?} {rb:?}"
            );

            // `area` must agree with the oracle, since the per-BO threshold and
            // the damage telemetry are both computed from it.
            assert_eq!(u.area(), pa.union(&pb).count() as u64);
            assert_eq!(d.area(), pa.difference(&pb).count() as u64);
            assert_eq!(i.area(), pa.intersection(&pb).count() as u64);
        }
    }

    #[test]
    fn negative_coordinates_survive_the_ops() {
        let mut g = region(&[r(-20, -20, 30, 30)]);
        g.subtract(&region(&[r(-10, -10, 10, 10)]));
        assert_canonical(&g);
        let mut expect = pixels_of(&[r(-20, -20, 30, 30)]);
        for p in pixels_of(&[r(-10, -10, 10, 10)]) {
            expect.remove(&p);
        }
        assert_eq!(pixels(&g), expect);
    }

    /// `clip_to_rect` is `intersect_with` without the slice decomposition; the
    /// two must agree as canonical regions (not just as pixel sets) on
    /// randomised inputs, including the vertical merges clipping can create.
    #[test]
    fn clip_to_rect_matches_intersect_with_canonically() {
        let mut seed = 0x9E37_79B9u32;
        let mut next = move |n: i32| -> i32 {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            (seed % u32::try_from(n).unwrap()) as i32
        };
        for case in 0..2000 {
            let mut g = Region::new();
            for _ in 0..(1 + next(9)) {
                g.add_rect(r(
                    next(40),
                    next(40),
                    1 + next(25) as u32,
                    1 + next(25) as u32,
                ));
            }
            let clip = r(
                next(50) - 5,
                next(50) - 5,
                1 + next(40) as u32,
                1 + next(40) as u32,
            );
            let fast = g.clip_to_rect(clip);
            let mut slow = g.clone();
            slow.intersect_with(&Region::from_rect(clip));
            assert_eq!(fast, slow, "case {case}: {g:?} ∩ {clip:?}");
            // And the fast path is used by `intersect_rect`.
            let mut via = g.clone();
            via.intersect_rect(clip);
            assert_eq!(via, slow);
        }
    }
}
