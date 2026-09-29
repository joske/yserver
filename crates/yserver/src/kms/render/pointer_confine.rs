//! Pointer confinement to the CRTCs' root rectangles (spec D5b), as Xorg's
//! RANDR does it: `RRConstrainCursorHarder` on every move and warp
//! (randr/rrcrtc.c:1889-1980), `RRPointerScreenConfigured` after a layout
//! change (randr/rrpointer.c:52-160). A rectangle is a CRTC's footprint at
//! its origin, `(x, y, width, height)`.

pub(crate) type CrtcRect = (i32, i32, u32, u32);

fn right(r: CrtcRect) -> i32 {
    r.0.saturating_add_unsigned(r.2)
}

fn bottom(r: CrtcRect) -> i32 {
    r.1.saturating_add_unsigned(r.3)
}

fn contains(r: CrtcRect, x: i32, y: i32) -> bool {
    x >= r.0 && x < right(r) && y >= r.1 && y < bottom(r)
}

/// `crtcs_adjacent`: closed boxes, so touching counts.
fn adjacent(a: CrtcRect, b: CrtcRect) -> bool {
    a.0.max(b.0) <= right(a).min(right(b)) && a.1.max(b.1) <= bottom(a).min(bottom(b))
}

/// `RRComputeContiguity`: every CRTC reachable from the first by adjacency.
pub(crate) fn crtcs_contiguous(rects: &[CrtcRect]) -> bool {
    let mut reachable = vec![false; rects.len()];
    let mut stack = Vec::new();
    if !rects.is_empty() {
        reachable[0] = true;
        stack.push(0);
    }
    while let Some(cur) = stack.pop() {
        for (i, r) in rects.iter().enumerate() {
            if !reachable[i] && adjacent(rects[cur], *r) {
                reachable[i] = true;
                stack.push(i);
            }
        }
    }
    reachable.iter().all(|&r| r)
}

/// `RRConstrainCursorHarder`: a move to `to` that leaves every CRTC is
/// clamped to the CRTC the pointer is coming `from`. A discontiguous layout
/// is left alone ("intentional dead space -> let it float"), as is a pointer
/// already outside every CRTC.
pub(crate) fn constrain_to_crtcs(
    rects: &[CrtcRect],
    from: (f32, f32),
    to: (f32, f32),
) -> (f32, f32) {
    if rects.is_empty() || !crtcs_contiguous(rects) {
        return to;
    }
    #[allow(clippy::cast_possible_truncation)]
    let (tx, ty) = (to.0.floor() as i32, to.1.floor() as i32);
    if rects.iter().any(|&r| contains(r, tx, ty)) {
        return to;
    }
    #[allow(clippy::cast_possible_truncation)]
    let (fx, fy) = (from.0 as i32, from.1 as i32);
    let Some(&r) = rects.iter().find(|&&r| contains(r, fx, fy)) else {
        return to;
    };
    #[allow(clippy::cast_precision_loss)]
    let clamp = |v: f32, lo: i32, hi: i32| {
        if v < lo as f32 {
            lo as f32
        } else if v >= hi as f32 {
            (hi - 1) as f32
        } else {
            v
        }
    };
    (clamp(to.0, r.0, right(r)), clamp(to.1, r.1, bottom(r)))
}

/// `RRPointerToNearestCrtc` with no CRTC skipped: the position on the
/// nearest CRTC when `(x, y)` is outside all of them, else `None`.
pub(crate) fn nearest_crtc_position(rects: &[CrtcRect], x: i32, y: i32) -> Option<(i32, i32)> {
    let mut best: Option<(i64, i32, i32)> = None;
    for &r in rects {
        let dx = if x < r.0 {
            r.0 - x
        } else if x > right(r) - 1 {
            right(r) - 1 - x
        } else {
            0
        };
        let dy = if y < r.1 {
            r.1 - y
        } else if y > bottom(r) - 1 {
            bottom(r) - 1 - y
        } else {
            0
        };
        let dist = i64::from(dx) * i64::from(dx) + i64::from(dy) * i64::from(dy);
        if best.is_none_or(|(b, _, _)| dist < b) {
            best = Some((dist, dx, dy));
        }
    }
    let (_, dx, dy) = best?;
    (dx != 0 || dy != 0).then_some((x + dx, y + dy))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cinnamon scale-up 125% on two 2560×1440 outputs (spec, "What muffin
    /// sends"): CRTC 4 at 0,0 through 0.5, CRTC 6 at 2560,0 through
    /// 0.799988; footprints 1280×720 and 2048×1152 leave a hole between.
    const SCALE_UP: [CrtcRect; 2] = [(0, 0, 1280, 720), (2560, 0, 2048, 1152)];
    /// Scale-down 100%: identity at 0,0 and 2.0 at 2560,0.
    const SCALE_DOWN: [CrtcRect; 2] = [(0, 0, 2560, 1440), (2560, 0, 5120, 2880)];

    #[test]
    fn contiguity_follows_touching_footprints() {
        assert!(crtcs_contiguous(&SCALE_DOWN));
        assert!(!crtcs_contiguous(&SCALE_UP));
        assert!(crtcs_contiguous(&[(0, 0, 800, 600)]));
    }

    #[test]
    fn a_move_into_a_hole_clamps_to_the_crtc_it_left() {
        // Below the identity output's bottom edge is a hole of the root.
        assert_eq!(
            constrain_to_crtcs(&SCALE_DOWN, (100.0, 1400.0), (100.0, 2000.0)),
            (100.0, 1439.0)
        );
        // Leaving the scaled output leftwards under the identity one.
        assert_eq!(
            constrain_to_crtcs(&SCALE_DOWN, (2600.0, 2000.0), (2500.0, 2000.0)),
            (2560.0, 2000.0)
        );
        // Crossing between the outputs is a move inside a CRTC.
        assert_eq!(
            constrain_to_crtcs(&SCALE_DOWN, (2550.0, 100.0), (2570.5, 100.0)),
            (2570.5, 100.0)
        );
    }

    #[test]
    fn a_discontiguous_layout_lets_the_pointer_float() {
        assert_eq!(
            constrain_to_crtcs(&SCALE_UP, (1270.0, 300.0), (1500.0, 300.0)),
            (1500.0, 300.0)
        );
    }

    #[test]
    fn a_reconfiguration_moves_a_pointer_in_a_hole_to_the_nearest_crtc() {
        // Nearer to CRTC 4's right edge (221 px) than to CRTC 6 (1060 px).
        assert_eq!(
            nearest_crtc_position(&SCALE_UP, 1500, 300),
            Some((1279, 300))
        );
        assert_eq!(
            nearest_crtc_position(&SCALE_UP, 2400, 1000),
            Some((2560, 1000))
        );
        assert_eq!(nearest_crtc_position(&SCALE_UP, 100, 100), None);
        assert_eq!(nearest_crtc_position(&[], 100, 100), None);
    }
}
