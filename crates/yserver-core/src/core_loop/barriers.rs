//! Pure pointer-barrier geometry and clamp helpers.
//!
//! This is a direct port of the `Xi/xibarriers.c` segment and clamp
//! logic, but without any server-state side effects. The core motion
//! path uses these helpers to decide whether a relative motion crosses a
//! barrier and where to clamp it.

pub const POSITIVE_X: u32 = 1;
pub const POSITIVE_Y: u32 = 2;
pub const NEGATIVE_X: u32 = 4;
pub const NEGATIVE_Y: u32 = 8;

/// Minimal geometry view of a barrier for clamp math.
#[derive(Clone, Copy, Debug)]
pub struct BarrierGeom {
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
    pub directions: u32,
}

impl BarrierGeom {
    fn is_vertical(self) -> bool {
        self.x1 == self.x2
    }

    fn is_horizontal(self) -> bool {
        self.y1 == self.y2
    }
}

/// Direction bits of travel from `(x1, y1)` to `(x2, y2)`.
#[must_use]
pub fn direction_of(x1: i32, y1: i32, x2: i32, y2: i32) -> u32 {
    let mut d = 0;
    if x2 > x1 {
        d |= POSITIVE_X;
    } else if x2 < x1 {
        d |= NEGATIVE_X;
    }
    if y2 > y1 {
        d |= POSITIVE_Y;
    } else if y2 < y1 {
        d |= NEGATIVE_Y;
    }
    d
}

#[must_use]
fn blocks_direction(directions: u32, dir: u32) -> bool {
    (directions & dir) != dir
}

/// Xorg `inside_segment`: negative endpoints encode rays / infinite lines.
#[must_use]
fn inside_segment(v: i32, v1: i32, v2: i32) -> bool {
    if v1 < 0 && v2 < 0 {
        true
    } else if v1 < 0 {
        v <= v2
    } else if v2 < 0 {
        v >= v1
    } else {
        v1 <= v && v <= v2
    }
}

/// Return the distance from the motion origin to the barrier crossing
/// if the segment geometrically crosses the barrier.
#[must_use]
pub fn is_blocking(b: &BarrierGeom, x1: i32, y1: i32, x2: i32, y2: i32) -> Option<f64> {
    let (x1f, y1f, x2f, y2f) = (x1 as f64, y1 as f64, x2 as f64, y2 as f64);

    if b.is_vertical() {
        let bx = f64::from(b.x1);
        if (x2f - x1f).abs() < f64::EPSILON {
            return None;
        }
        let t = (bx - x1f) / (x2f - x1f);
        if !(0.0..=1.0).contains(&t) {
            return None;
        }
        if x2 > x1 && t == 0.0 {
            return None;
        }
        let y = t * (y1f - y2f) + y1f;
        #[allow(clippy::cast_possible_truncation)]
        if !inside_segment(y as i32, b.y1, b.y2) {
            return None;
        }
        Some(((y - y1f).powi(2) + (bx - x1f).powi(2)).sqrt())
    } else {
        let by = f64::from(b.y1);
        if (y2f - y1f).abs() < f64::EPSILON {
            return None;
        }
        let t = (by - y1f) / (y2f - y1f);
        if !(0.0..=1.0).contains(&t) {
            return None;
        }
        if y2 > y1 && t == 0.0 {
            return None;
        }
        let x = t * (x1f - x2f) + x1f;
        #[allow(clippy::cast_possible_truncation)]
        if !inside_segment(x as i32, b.x1, b.x2) {
            return None;
        }
        Some(((x - x1f).powi(2) + (by - y1f).powi(2)).sqrt())
    }
}

/// Clamp a point to the barrier edge relevant for `dir`.
pub fn clamp_to_barrier(b: &BarrierGeom, dir: u32, x: &mut i32, y: &mut i32) {
    if b.is_vertical() {
        if (dir & NEGATIVE_X != 0) && blocks_direction(b.directions, NEGATIVE_X) {
            *x = b.x1;
        }
        if (dir & POSITIVE_X != 0) && blocks_direction(b.directions, POSITIVE_X) {
            *x = b.x1 - 1;
        }
    }
    if b.is_horizontal() {
        if (dir & NEGATIVE_Y != 0) && blocks_direction(b.directions, NEGATIVE_Y) {
            *y = b.y1;
        }
        if (dir & POSITIVE_Y != 0) && blocks_direction(b.directions, POSITIVE_Y) {
            *y = b.y1 - 1;
        }
    }
}

/// Xorg `barrier_inside_hit_box`: hit-state stays armed until the
/// pointer leaves a small padded box around the barrier segment.
#[must_use]
pub fn inside_hit_box(b: &BarrierGeom, x: i32, y: i32) -> bool {
    const HIT_EDGE_EXTENTS: i32 = 2;
    let mut x1 = b.x1;
    let mut x2 = b.x2;
    let mut y1 = b.y1;
    let mut y2 = b.y2;
    let dir = !b.directions;

    if b.is_vertical() {
        if dir & POSITIVE_X != 0 {
            x1 -= HIT_EDGE_EXTENTS;
        }
        if dir & NEGATIVE_X != 0 {
            x2 += HIT_EDGE_EXTENTS;
        }
    }
    if b.is_horizontal() {
        if dir & POSITIVE_Y != 0 {
            y1 -= HIT_EDGE_EXTENTS;
        }
        if dir & NEGATIVE_Y != 0 {
            y2 += HIT_EDGE_EXTENTS;
        }
    }

    x >= x1 && x <= x2 && y >= y1 && y <= y2
}

/// Find the nearest barrier that blocks at least one direction in `dir`.
#[must_use]
pub fn find_nearest(
    candidates: &[(usize, BarrierGeom)],
    seen: &[usize],
    dir: u32,
    x1: i32,
    y1: i32,
    x2: i32,
    y2: i32,
) -> Option<(usize, f64, BarrierGeom)> {
    let mut best: Option<(usize, f64, BarrierGeom)> = None;
    for &(idx, b) in candidates {
        if seen.contains(&idx) {
            continue;
        }
        let mut dir_blocked = false;
        for bit in [POSITIVE_X, POSITIVE_Y, NEGATIVE_X, NEGATIVE_Y] {
            if dir & bit != 0 && blocks_direction(b.directions, bit) {
                dir_blocked = true;
                break;
            }
        }
        if !dir_blocked {
            continue;
        }
        let Some(dist) = is_blocking(&b, x1, y1, x2, y2) else {
            continue;
        };
        if best.is_none_or(|(_, best_dist, _)| dist < best_dist) {
            best = Some((idx, dist, b));
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vbar(x: i32, y1: i32, y2: i32, dirs: u32) -> BarrierGeom {
        BarrierGeom {
            x1: x,
            y1,
            x2: x,
            y2,
            directions: dirs,
        }
    }

    #[test]
    fn solid_vertical_from_left_clamps_to_x1_minus_1() {
        let b = vbar(100, 0, 200, 0);
        assert!(is_blocking(&b, 90, 50, 110, 50).is_some());
        let (mut x, mut y) = (110, 50);
        clamp_to_barrier(&b, direction_of(90, 50, 110, 50), &mut x, &mut y);
        assert_eq!((x, y), (99, 50));
    }

    #[test]
    fn solid_vertical_from_right_clamps_to_x1() {
        let b = vbar(100, 0, 200, 0);
        let (mut x, mut y) = (90, 50);
        clamp_to_barrier(&b, direction_of(110, 50, 90, 50), &mut x, &mut y);
        assert_eq!((x, y), (100, 50));
    }

    #[test]
    fn permitted_direction_passes_through() {
        let b = vbar(100, 0, 200, NEGATIVE_X);
        let (mut x, mut y) = (90, 50);
        clamp_to_barrier(&b, direction_of(110, 50, 90, 50), &mut x, &mut y);
        assert_eq!((x, y), (90, 50));
    }

    #[test]
    fn miss_outside_segment() {
        let b = vbar(100, 0, 200, 0);
        assert!(is_blocking(&b, 90, 300, 110, 300).is_none());
    }

    #[test]
    fn on_barrier_moving_away_not_blocking() {
        let b = vbar(100, 0, 200, 0);
        assert!(is_blocking(&b, 100, 50, 110, 50).is_none());
    }

    #[test]
    fn inside_segment_ray_semantics() {
        assert!(inside_segment(500, 0, -1));
        assert!(!inside_segment(-5, 0, -1));
        assert!(inside_segment(123, -1, -1));
    }

    #[test]
    fn hit_box_extends_only_blocked_sides() {
        let b = vbar(100, 0, 200, 0);
        assert!(inside_hit_box(&b, 102, 50));
        assert!(inside_hit_box(&b, 98, 50));
        assert!(!inside_hit_box(&b, 103, 50));
        assert!(!inside_hit_box(&b, 97, 50));
    }
}
