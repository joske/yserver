use super::*;

/// Render per damage rect rather than under one bounding box once the box wastes
/// this much.
///
/// Measured on silence with the phased workload: bounding-box waste is 0% while
/// idle, 19% resizing and **36% dragging** — a moved window's damage is exactly
/// two disjoint rects, old and new, and their box spans both plus the empty gap
/// between them. 1.5 sits below the 1.57 the drag phase produces and well above
/// the 1.0 of a single contiguous rect.
///
/// An earlier reading of whole-session medians put the waste at 0.5-1.8% and
/// concluded this was not worth building. That average was dominated by idle
/// frames; a median over a whole session cannot answer a question about one kind
/// of frame.
const MULTI_RECT_MIN_GAIN: f64 = 1.5;

/// Cap on scissor rects per frame.
///
/// Set to the region's own rect cap, which means it never binds: a `Region`
/// collapses to its extents above [`Region::MAX_RECTS`], so the list handed here
/// is already bounded.
///
/// It was 8, on the reasoning that each scissor re-issues every draw that
/// intersects it and the draw-call count would explode. **That reasoning was
/// wrong, and measurably so.** The count that matters is the *post-cull* draw
/// count, and on MATE that is 4.0 per compose against 53.6 pre-cull — the
/// scissor cull removes 92% of draws because damage is a small fraction of the
/// screen and most windows do not intersect it. So the cost is scissors × ~4,
/// not scissors × ~53.
///
/// The 8-rect cap cost real work: on MATE the panels and desktop fragment a drag
/// region past 8, so it fell back to the bounding box and 34% of the painted
/// area was the empty gap between a window's old and new position — the exact
/// waste 4.5 exists to remove, reappearing on the more realistic desktop while
/// the tiling-WM measurement looked fine.
const MAX_SCISSOR_RECTS: usize = Region::MAX_RECTS;

/// Above this fraction of the output, clipping costs more than it saves.
///
/// Measured on bee: at a damage fraction of 0.857 a clipped compose cost
/// 208.7 µs against 199.3 µs for a full one — a sub-rect pass still pays scissor
/// setup and every draw call, so only fragment work shrinks. Without the
/// threshold, clipped repaint is a net loss on exactly the frames that are
/// whole-output today.
///
/// Applied to the area that will be **painted** (the bounding box), never to the
/// damage region: sparse damage spread across the screen has a small region and
/// a near-full bbox, and thresholding on the region would pick the clipped path
/// and then rasterise almost everything anyway, with the LOAD and scissor
/// overhead on top.
///
/// The bee number is a fast GPU; re-measure the crossover on the z400 and adjust
/// once. A constant, deliberately not an environment variable.
pub(super) const CLIPPED_REPAINT_MAX_FRACTION: f64 = 0.6;

/// A draw's destination rect, rounded **inward**.
///
/// `dst_origin`/`dst_size` are `f32`. Rounding the origin up and the far edge
/// down means a fractional edge never counts as covered, so the opaque-cover
/// guard can only ever be conservative.
pub(super) fn draw_dst_rect_inward(draw: &CompositeDraw) -> Option<vk::Rect2D> {
    let x0 = draw.dst_origin[0].ceil();
    let y0 = draw.dst_origin[1].ceil();
    let x1 = (draw.dst_origin[0] + draw.dst_size[0]).floor();
    let y1 = (draw.dst_origin[1] + draw.dst_size[1]).floor();
    if !(x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite())
        || x1 <= x0
        || y1 <= y0
    {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)]
    Some(vk::Rect2D {
        offset: vk::Offset2D {
            x: x0 as i32,
            y: y0 as i32,
        },
        extent: vk::Extent2D {
            width: (x1 - x0) as u32,
            height: (y1 - y0) as u32,
        },
    })
}

/// True if the **opaque** draws together cover every pixel of `rect`.
///
/// This is what makes `loadOp = LOAD` + scissor equal a full compose inside the
/// region, and it is why step 4 needs no background algebra: yserver already
/// draws an opaque bottom layer. `alpha_passthrough == false` selects the
/// force-opaque pipeline variant, whose fragment shader sets `src.a = 1` against
/// `ONE / ONE_MINUS_SRC_ALPHA` blending, so such a draw fully overwrites the
/// destination and whatever the previous compose left is irrelevant.
///
/// A **union** of draws, not a single one, because step 1 clips the root to
/// `output − opaque windows`: a repaint rect that straddles a window edge is
/// then covered by the root fragment on one side and the window on the other,
/// and no single draw contains it. Computed by subtracting each opaque draw from
/// a remainder rather than by unioning the draws and testing containment: the
/// 32-box cap collapses a `Region` to its bounding box, and a collapsed
/// *remainder* is a superset (⇒ "not covered" ⇒ Full, safe) while a collapsed
/// *union* claims coverage it does not have — the defect that broke the naive
/// occlusion cull (`findings/2026-09-03-naive-occlusion-cull-postmortem.md`).
///
/// Note what is never an opaque bottom layer: every COW-subtree draw is
/// `alpha_passthrough = true` by construction, and so is the software cursor. A
/// compositing desktop therefore usually fails this gate and renders Full —
/// which is correct and costs nothing, because a compositor presents a
/// full-screen surface every frame regardless.
pub(super) fn opaque_cover_exists(draws: &[CompositeDraw], rect: vk::Rect2D) -> bool {
    let mut remainder = Region::from_rect(rect);
    for d in draws {
        if remainder.is_empty() {
            break;
        }
        if d.alpha_passthrough {
            continue;
        }
        if let Some(dst) = draw_dst_rect_inward(d)
            && rects_intersect(dst, rect)
        {
            remainder.subtract(&Region::from_rect(dst));
        }
    }
    remainder.is_empty()
}

/// Decide how to repaint, and report what that will paint.
///
/// Every gate here is a documented way to corrupt the screen under
/// `loadOp = LOAD`; each one falls back to Full rather than trying to be clever.
impl RepaintPlan {
    pub(super) fn full(extent: vk::Extent2D, reason: FullReason) -> Self {
        let whole = vk::Rect2D {
            offset: vk::Offset2D::default(),
            extent,
        };
        Self {
            repaint: Repaint::Full(extent),
            scissors: vec![whole],
            painted: Region::from_rect(whole),
            full_reason: Some(reason),
        }
    }
}

pub(super) fn plan_repaint(
    requested: &Region,
    draws: &[CompositeDraw],
    extent: vk::Extent2D,
    loadable: bool,
    shared_route: bool,
) -> RepaintPlan {
    let full = |reason: FullReason| RepaintPlan::full(extent, reason);

    if !shared_route {
        return full(FullReason::CopiedRoute);
    }
    if draws.is_empty() {
        // The Clipped/LOAD path would preserve each BO's prior-generation
        // content — including a pre-`bg_pixel`-update black — and never
        // re-clear it. Full so `loadOp = CLEAR` paints the current `bg_color`
        // across the whole BO.
        return full(FullReason::EmptyDrawList);
    }
    if !loadable {
        return full(FullReason::UnloadableBo);
    }
    let Some(bbox) = requested.bounding_rect() else {
        // Nothing to paint at all. Conservative rather than clever: a degenerate
        // case should not be the one path with bespoke handling.
        return full(FullReason::EmptyDrawList);
    };

    // Per-rect or bounding box? Decided before the threshold, because per-rect
    // paints less and so keeps frames on the clipped path that a box would push
    // over the line.
    let rects: Vec<vk::Rect2D> = requested.rects().collect();
    let bbox_area = u64::from(bbox.extent.width) * u64::from(bbox.extent.height);
    #[allow(clippy::cast_precision_loss)]
    let wasteful =
        requested.area() > 0 && bbox_area as f64 > MULTI_RECT_MIN_GAIN * requested.area() as f64;
    let (scissors, painted) = if rects.len() > 1 && rects.len() <= MAX_SCISSOR_RECTS && wasteful {
        (rects, requested.clone())
    } else {
        (vec![bbox], Region::from_rect(bbox))
    };

    let output_area = u64::from(extent.width) * u64::from(extent.height);
    #[allow(clippy::cast_precision_loss)]
    let fraction = if output_area == 0 {
        1.0
    } else {
        painted.area() as f64 / output_area as f64
    };
    if fraction >= CLIPPED_REPAINT_MAX_FRACTION {
        return full(FullReason::Threshold);
    }

    // Every pixel that will be painted needs some opaque draw over it, so the
    // check is per scissor: different rects may legitimately be covered by
    // different draws.
    if !scissors.iter().all(|r| opaque_cover_exists(draws, *r)) {
        return full(FullReason::NoOpaqueCover);
    }

    RepaintPlan {
        repaint: Repaint::Clipped(bbox),
        scissors,
        painted,
        full_reason: None,
    }
}

/// The draws that intersect `rect`, in unchanged order.
///
/// Culling before descriptor allocation is where the per-compose CPU floor comes
/// down: the audit's fit put 40-110 µs per compose in draw calls, descriptor
/// binds and pipeline switches that clipping fragment work alone never removes.
///
/// **The result is a separate product and must never replace `built.scene`.**
/// The full list is what the drawable snapshots, the audit's reference oracle
/// and (from step 2) the previous-frame scene diff all read. Recording the culled
/// list as the frame's scene would make every culled draw read as "disappeared"
/// next frame, manufacturing structural damage over all of them — the screen
/// would look perfectly correct while the entire saving evaporated.
pub(super) fn cull_scene_to_region(scene: &CompositeScene, keep: &Region) -> CompositeScene {
    CompositeScene {
        bg_color: scene.bg_color,
        draws: scene
            .draws
            .iter()
            .filter(|d| draw_dst_rect_inward(d).is_none_or(|dst| keep.intersects_rect(dst)))
            .copied()
            .collect(),
    }
}

fn all_zero(c: [f32; 4]) -> bool {
    c[0] == 0.0 && c[1] == 0.0 && c[2] == 0.0 && c[3] == 0.0
}
