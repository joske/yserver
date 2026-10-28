//! Y-x banded regions with pixman's set operations (`pixman-region.c`,
//! `pixman_op` and its `union_o` / `intersect_o` / `subtract_o` overlap
//! functions). A [`Region`] is always in pixman's canonical form: boxes
//! sorted by band then x, every box of a band sharing its y range, boxes in
//! a band neither overlapping nor touching, and two adjacent bands with the
//! same x spans merged. The operations take canonical regions and return
//! one without normalising, which is what makes them cheap.

use yserver_protocol::x11::xfixes::RegionRect;

/// A half-open box, `x1 <= x < x2`, `y1 <= y < y2`, in `i32` so that
/// offsets and borders never wrap; rects are clamped to the wire's range
/// only on the way out ([`Region::to_rects`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Box32 {
    pub(crate) x1: i32,
    pub(crate) y1: i32,
    pub(crate) x2: i32,
    pub(crate) y2: i32,
}

impl Box32 {
    fn from_rect(r: RegionRect) -> Self {
        let x1 = i32::from(r.x);
        let y1 = i32::from(r.y);
        Self {
            x1,
            y1,
            x2: x1 + i32::from(r.width),
            y2: y1 + i32::from(r.height),
        }
    }

    fn to_rect(self) -> RegionRect {
        let wire = |v: i32| v.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
        let len = |v: i32| v.clamp(0, i32::from(u16::MAX)) as u16;
        RegionRect {
            x: wire(self.x1),
            y: wire(self.y1),
            width: len(self.x2 - self.x1),
            height: len(self.y2 - self.y1),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Region {
    boxes: Vec<Box32>,
}

impl Region {
    pub(crate) fn is_empty(&self) -> bool {
        self.boxes.is_empty()
    }

    /// The region `rects` cover. Rects already in canonical form are taken
    /// as they are; anything else (SHAPE and XFIXES input: unsorted,
    /// overlapping) is normalised once here, in `i32` throughout: each
    /// non-empty rect becomes a one-box region and the regions are unioned
    /// pairwise, a tree of `log n` rounds.
    pub(crate) fn from_rects(rects: &[RegionRect]) -> Self {
        let mut boxes: Vec<Box32> = rects.iter().map(|r| Box32::from_rect(*r)).collect();
        if is_canonical(&boxes) {
            return Self { boxes };
        }
        boxes.retain(|b| b.x1 < b.x2 && b.y1 < b.y2);
        // Sorted, neighbours in the tree mostly share bands, which keeps
        // the intermediate regions small.
        boxes.sort_unstable_by_key(|b| (b.y1, b.x1));
        let mut level: Vec<Self> = boxes.into_iter().map(|b| Self { boxes: vec![b] }).collect();
        while level.len() > 1 {
            let mut next = Vec::with_capacity(level.len().div_ceil(2));
            let mut it = level.into_iter();
            while let Some(a) = it.next() {
                next.push(match it.next() {
                    Some(b) => a.union(&b),
                    None => a,
                });
            }
            level = next;
        }
        level.pop().unwrap_or_default()
    }

    /// The region of one rect: empty if it has no area.
    pub(crate) fn rect(r: RegionRect) -> Self {
        let b = Box32::from_rect(r);
        if b.x1 < b.x2 && b.y1 < b.y2 {
            Self { boxes: vec![b] }
        } else {
            Self::default()
        }
    }

    /// The region moved by `(dx, dy)`; moving keeps it canonical.
    pub(crate) fn translate(mut self, dx: i32, dy: i32) -> Self {
        if dx != 0 || dy != 0 {
            for b in &mut self.boxes {
                *b = Box32 {
                    x1: b.x1 + dx,
                    y1: b.y1 + dy,
                    x2: b.x2 + dx,
                    y2: b.y2 + dy,
                };
            }
        }
        self
    }

    pub(crate) fn to_rects(&self) -> Vec<RegionRect> {
        self.boxes.iter().map(|b| b.to_rect()).collect()
    }

    fn extents(&self) -> Option<Box32> {
        let first = self.boxes.first()?;
        let last = self.boxes.last()?;
        let (x1, x2) = self.boxes.iter().fold((i32::MAX, i32::MIN), |(lo, hi), b| {
            (lo.min(b.x1), hi.max(b.x2))
        });
        Some(Box32 {
            x1,
            y1: first.y1,
            x2,
            y2: last.y2,
        })
    }

    pub(crate) fn union(&self, other: &Self) -> Self {
        if self.is_empty() {
            return other.clone();
        }
        if other.is_empty() {
            return self.clone();
        }
        op(self, other, Overlap::Union, true, true)
    }

    pub(crate) fn intersect(&self, other: &Self) -> Self {
        match (self.extents(), other.extents()) {
            (Some(a), Some(b)) if disjoint(a, b) => Self::default(),
            (Some(_), Some(_)) => op(self, other, Overlap::Intersect, false, false),
            _ => Self::default(),
        }
    }

    pub(crate) fn subtract(&self, other: &Self) -> Self {
        match (self.extents(), other.extents()) {
            (Some(a), Some(b)) if !disjoint(a, b) => {
                op(self, other, Overlap::Subtract, true, false)
            }
            _ => self.clone(),
        }
    }
}

fn disjoint(a: Box32, b: Box32) -> bool {
    a.x2 <= b.x1 || b.x2 <= a.x1 || a.y2 <= b.y1 || b.y2 <= a.y1
}

/// Whether `boxes` already are a canonical region (see the module doc).
fn is_canonical(boxes: &[Box32]) -> bool {
    let mut prev_band: Option<(usize, usize)> = None;
    let mut i = 0;
    while i < boxes.len() {
        let (y1, y2) = (boxes[i].y1, boxes[i].y2);
        if y1 >= y2 {
            return false;
        }
        let mut j = i;
        while j < boxes.len() && boxes[j].y1 == y1 {
            let b = boxes[j];
            if b.y2 != y2 || b.x1 >= b.x2 || (j > i && boxes[j - 1].x2 >= b.x1) {
                return false;
            }
            j += 1;
        }
        if let Some((ps, pe)) = prev_band {
            let prev_y2 = boxes[ps].y2;
            if prev_y2 > y1 {
                return false;
            }
            let same_spans = pe - ps == j - i
                && boxes[ps..pe]
                    .iter()
                    .zip(&boxes[i..j])
                    .all(|(a, b)| a.x1 == b.x1 && a.x2 == b.x2);
            if prev_y2 == y1 && same_spans {
                return false;
            }
        }
        prev_band = Some((i, j));
        i = j;
    }
    true
}

#[derive(Clone, Copy)]
enum Overlap {
    Union,
    Intersect,
    Subtract,
}

/// The end of the band starting at `start`.
fn band_end(boxes: &[Box32], start: usize) -> usize {
    let y1 = boxes[start].y1;
    let mut end = start;
    while end < boxes.len() && boxes[end].y1 == y1 {
        end += 1;
    }
    end
}

/// `pixman_coalesce`: merge the band at `cur` into the one at `prev` when
/// they touch and have the same spans. Returns where the last band now
/// starts.
fn coalesce(out: &mut Vec<Box32>, prev: usize, cur: usize) -> usize {
    let n = cur - prev;
    if n == 0 || n != out.len() - cur || out[prev].y2 != out[cur].y1 {
        return cur;
    }
    if (0..n).any(|k| out[prev + k].x1 != out[cur + k].x1 || out[prev + k].x2 != out[cur + k].x2) {
        return cur;
    }
    let y2 = out[cur].y2;
    for b in &mut out[prev..cur] {
        b.y2 = y2;
    }
    out.truncate(cur);
    prev
}

fn append_non(out: &mut Vec<Box32>, band: &[Box32], y1: i32, y2: i32) {
    out.extend(band.iter().map(|b| Box32 {
        x1: b.x1,
        y1,
        x2: b.x2,
        y2,
    }));
}

fn overlap(kind: Overlap, out: &mut Vec<Box32>, a: &[Box32], b: &[Box32], y1: i32, y2: i32) {
    match kind {
        Overlap::Union => union_o(out, a, b, y1, y2),
        Overlap::Intersect => intersect_o(out, a, b, y1, y2),
        Overlap::Subtract => subtract_o(out, a, b, y1, y2),
    }
}

/// `pixman_op`: walk both regions band by band, handing the y ranges only
/// one of them covers to `append_non` (when that side's flag is set) and
/// the ranges both cover to the overlap function, coalescing as it goes.
fn op(a: &Region, b: &Region, kind: Overlap, append_a: bool, append_b: bool) -> Region {
    let (ra, rb) = (&a.boxes, &b.boxes);
    let mut out: Vec<Box32> = Vec::with_capacity(ra.len() + rb.len());
    let (mut i, mut j) = (0, 0);
    let mut ybot = ra[0].y1.min(rb[0].y1);
    let mut prev_band = 0;
    while i < ra.len() && j < rb.len() {
        let (ia, jb) = (band_end(ra, i), band_end(rb, j));
        let (a1, b1) = (ra[i].y1, rb[j].y1);
        let ytop = if a1 < b1 {
            if append_a {
                let top = a1.max(ybot);
                let bot = ra[i].y2.min(b1);
                if top != bot {
                    let cur = out.len();
                    append_non(&mut out, &ra[i..ia], top, bot);
                    prev_band = coalesce(&mut out, prev_band, cur);
                }
            }
            b1
        } else if b1 < a1 {
            if append_b {
                let top = b1.max(ybot);
                let bot = rb[j].y2.min(a1);
                if top != bot {
                    let cur = out.len();
                    append_non(&mut out, &rb[j..jb], top, bot);
                    prev_band = coalesce(&mut out, prev_band, cur);
                }
            }
            a1
        } else {
            a1
        };
        ybot = ra[i].y2.min(rb[j].y2);
        if ybot > ytop {
            let cur = out.len();
            overlap(kind, &mut out, &ra[i..ia], &rb[j..jb], ytop, ybot);
            prev_band = coalesce(&mut out, prev_band, cur);
        }
        if ra[i].y2 == ybot {
            i = ia;
        }
        if rb[j].y2 == ybot {
            j = jb;
        }
    }
    let rest = |out: &mut Vec<Box32>, prev_band: usize, r: &[Box32], k: usize| {
        let end = band_end(r, k);
        let cur = out.len();
        append_non(out, &r[k..end], r[k].y1.max(ybot), r[k].y2);
        coalesce(out, prev_band, cur);
        out.extend_from_slice(&r[end..]);
    };
    if i < ra.len() && append_a {
        rest(&mut out, prev_band, ra, i);
    } else if j < rb.len() && append_b {
        rest(&mut out, prev_band, rb, j);
    }
    Region { boxes: out }
}

fn union_o(out: &mut Vec<Box32>, a: &[Box32], b: &[Box32], y1: i32, y2: i32) {
    let (mut i, mut j) = (0, 0);
    let first = if a[0].x1 < b[0].x1 {
        i += 1;
        a[0]
    } else {
        j += 1;
        b[0]
    };
    let (mut x1, mut x2) = (first.x1, first.x2);
    let mut merge = |out: &mut Vec<Box32>, r: Box32| {
        if r.x1 <= x2 {
            x2 = x2.max(r.x2);
        } else {
            out.push(Box32 { x1, y1, x2, y2 });
            x1 = r.x1;
            x2 = r.x2;
        }
    };
    while i < a.len() && j < b.len() {
        if a[i].x1 < b[j].x1 {
            merge(out, a[i]);
            i += 1;
        } else {
            merge(out, b[j]);
            j += 1;
        }
    }
    for r in a[i..].iter().chain(&b[j..]) {
        merge(out, *r);
    }
    out.push(Box32 { x1, y1, x2, y2 });
}

fn intersect_o(out: &mut Vec<Box32>, a: &[Box32], b: &[Box32], y1: i32, y2: i32) {
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let x1 = a[i].x1.max(b[j].x1);
        let x2 = a[i].x2.min(b[j].x2);
        if x1 < x2 {
            out.push(Box32 { x1, y1, x2, y2 });
        }
        if a[i].x2 == x2 {
            i += 1;
        }
        if b[j].x2 == x2 {
            j += 1;
        }
    }
}

fn subtract_o(out: &mut Vec<Box32>, m: &[Box32], s: &[Box32], y1: i32, y2: i32) {
    let (mut i, mut j) = (0, 0);
    let mut x1 = m[0].x1;
    while i < m.len() && j < s.len() {
        if s[j].x2 <= x1 {
            // The subtrahend box is wholly left of what is left of m[i].
            j += 1;
        } else if s[j].x1 <= x1 {
            // It covers m[i]'s left part.
            x1 = s[j].x2;
            if x1 >= m[i].x2 {
                i += 1;
                if i < m.len() {
                    x1 = m[i].x1;
                }
            } else {
                j += 1;
            }
        } else if s[j].x1 < m[i].x2 {
            // It starts inside m[i]: keep what lies left of it.
            out.push(Box32 {
                x1,
                y1,
                x2: s[j].x1,
                y2,
            });
            x1 = s[j].x2;
            if x1 >= m[i].x2 {
                i += 1;
                if i < m.len() {
                    x1 = m[i].x1;
                }
            } else {
                j += 1;
            }
        } else {
            // It starts right of m[i]: keep the rest of m[i].
            if m[i].x2 > x1 {
                out.push(Box32 {
                    x1,
                    y1,
                    x2: m[i].x2,
                    y2,
                });
            }
            i += 1;
            if i < m.len() {
                x1 = m[i].x1;
            }
        }
    }
    while i < m.len() {
        out.push(Box32 {
            x1,
            y1,
            x2: m[i].x2,
            y2,
        });
        i += 1;
        if i < m.len() {
            x1 = m[i].x1;
        }
    }
}

#[cfg(test)]
mod tests;
