use super::*;

/// Stage 4c.1 — code-quality follow-up. The plural setter test
/// above only proves the dirty bit gets set on the stub-mode
/// compositor (where `inner` is `None` and the dispatch for-loop
/// is unreachable). `clip_rect_to_output_extent_handles_all_cases`
/// only covers the helper math in isolation. Their union does
/// NOT cover the dispatch wiring — a regression that swapped
/// `damage.add(clipped)` for a no-op (or dropped the per-output
/// loop entirely) would pass both tests. This test exercises
/// the extracted `dispatch_clip_rects_to_outputs` helper that
/// `mark_scene_structure_damage_rects` delegates to, with two
/// synthetic outputs of different extents, and asserts that:
///
/// - a rect wholly inside lands unchanged on every output;
/// - a rect spilling off the right edge lands clipped (NOT in
///   its original form) on the output where it spills;
/// - a rect that's fully outside an output is dropped for that
///   output but still lands on the other output if it fits there;
/// - per-output clipping is independent (extent of output A does
///   not influence what lands on output B).
#[test]
fn dispatch_clip_rects_lands_per_output_clipped() {
    // Output 0: 800×600, Output 1: 400×400. Same input rect set.
    let ext_a = extent(800, 600);
    let ext_b = extent(400, 400);
    let mut damage_a = RegionSet::new();
    let mut damage_b = RegionSet::new();

    let inside = rect(10, 20, 100, 50); // fits both outputs
    let spilling_right = rect(700, 0, 200, 50); // spills A on right; fully outside B
    let outside_a_inside_b = rect(350, 350, 30, 30); // fits both (B clips to 50×50)
    let fully_outside = rect(2000, 2000, 50, 50); // outside both

    let rects = [inside, spilling_right, outside_a_inside_b, fully_outside];

    // Build a `Vec` of tuples so the slice carries a stable lifetime for
    // the iterator; the production callsite produces
    // `(origin, extent, &mut damage)`. Both outputs sit at the origin here,
    // which is the case where root-absolute and output-local coincide — see
    // the test below for the case where they do not.
    let mut outs: Vec<((i32, i32), vk::Extent2D, &mut RegionSet)> = vec![
        ((0, 0), ext_a, &mut damage_a),
        ((0, 0), ext_b, &mut damage_b),
    ];
    dispatch_clip_rects_to_outputs(outs.drain(..), &rects);

    // Output A (800×600):
    //   - inside (10,20,100,50): identity
    //   - spilling_right (700,0,200,50): clipped width 200→100
    //   - outside_a_inside_b (350,350,30,30): identity (fits A)
    //   - fully_outside: dropped
    let a_rects = damage_a.rects();
    assert!(
        a_rects.contains(&inside),
        "inside rect must land unchanged on output A: {a_rects:?}",
    );
    let spilling_clipped_a = rect(700, 0, 100, 50);
    assert!(
        a_rects.contains(&spilling_clipped_a),
        "spilling rect must land CLIPPED on output A (expected {spilling_clipped_a:?}), got {a_rects:?}",
    );
    assert!(
        !a_rects.contains(&spilling_right),
        "spilling rect must NOT land in its original (unclipped) form on output A: {a_rects:?}",
    );
    assert!(
        a_rects.contains(&outside_a_inside_b),
        "rect that fits output A unchanged must land: {a_rects:?}",
    );
    assert!(
        !a_rects.iter().any(|r| r.offset.x >= 800
            || r.offset.y >= 600
            || i64::from(r.offset.x) + i64::from(r.extent.width) > 800
            || i64::from(r.offset.y) + i64::from(r.extent.height) > 600),
        "no rect on output A may spill its 800×600 extent: {a_rects:?}",
    );

    // Output B (400×400):
    //   - inside (10,20,100,50): identity
    //   - spilling_right (700,0,...): fully outside → dropped
    //   - outside_a_inside_b (350,350,30,30): clipped → (350,350,30,30) fits in 400×400
    //   - fully_outside: dropped
    let b_rects = damage_b.rects();
    assert!(
        b_rects.contains(&inside),
        "inside rect must land unchanged on output B: {b_rects:?}",
    );
    assert!(
        !b_rects.iter().any(|r| r.offset.x >= 700),
        "spilling-right (x=700) is fully outside output B and must be dropped: {b_rects:?}",
    );
    assert!(
        b_rects.contains(&outside_a_inside_b),
        "rect that fits output B unchanged must land: {b_rects:?}",
    );
    assert!(
        !b_rects
            .iter()
            .any(|r| i64::from(r.offset.x) + i64::from(r.extent.width) > 400
                || i64::from(r.offset.y) + i64::from(r.extent.height) > 400),
        "no rect on output B may spill its 400×400 extent: {b_rects:?}",
    );
}

/// Stage 4c.1 — the helper clips a rect to the output's extent
/// (offset assumed (0,0) — output-local coords). Wholly inside
/// → identity. Partially overlapping → clipped intersection.
/// Fully outside or zero-area → `None`.
#[test]
fn clip_rect_to_output_extent_handles_all_cases() {
    let ext = extent(800, 600);

    // Wholly inside — identity.
    assert_eq!(
        clip_rect_to_output_extent(rect(10, 20, 100, 50), ext),
        Some(rect(10, 20, 100, 50)),
    );

    // Right edge spills — clip width.
    assert_eq!(
        clip_rect_to_output_extent(rect(700, 0, 200, 50), ext),
        Some(rect(700, 0, 100, 50)),
    );

    // Bottom edge spills — clip height.
    assert_eq!(
        clip_rect_to_output_extent(rect(0, 500, 50, 200), ext),
        Some(rect(0, 500, 50, 100)),
    );

    // Negative offset — clamp to 0, clip width.
    assert_eq!(
        clip_rect_to_output_extent(rect(-30, -20, 100, 80), ext),
        Some(rect(0, 0, 70, 60)),
    );

    // Wholly to the right — None.
    assert_eq!(clip_rect_to_output_extent(rect(900, 0, 50, 50), ext), None);

    // Wholly below — None.
    assert_eq!(clip_rect_to_output_extent(rect(0, 700, 50, 50), ext), None);

    // Zero-width — None.
    assert_eq!(clip_rect_to_output_extent(rect(10, 10, 0, 50), ext), None);

    // Zero-height — None.
    assert_eq!(clip_rect_to_output_extent(rect(10, 10, 50, 0), ext), None);
}

#[test]
fn buffer_age_ring_trims_to_depth() {
    let mut ring = BufferAgeRing::new(3);
    for g in 1..=5 {
        let mut r = RegionSet::new();
        r.add(rect(0, 0, 4, 4));
        ring.push(g, r);
    }
    assert_eq!(ring.entries.len(), 3);
    // Oldest entries trimmed: 1, 2 gone; 3, 4, 5 remain.
    let gens: Vec<u64> = ring.entries.iter().map(|(g, _)| *g).collect();
    assert_eq!(gens, vec![3, 4, 5]);
}

#[test]
fn buffer_age_contains_all_strict_window() {
    let mut ring = BufferAgeRing::new(4);
    let mut r = RegionSet::new();
    r.add(rect(0, 0, 4, 4));
    ring.push(3, r.clone());
    ring.push(4, r.clone());
    // BO last_gen=2, frame_gen=5 → intervening gens 3, 4.
    assert!(ring.contains_all(2, 5));
    // BO last_gen=2, frame_gen=6 → needs 3, 4, 5 — 5 missing.
    assert!(!ring.contains_all(2, 6));
    // No intervening gens (frame_gen == last_gen+1).
    assert!(ring.contains_all(2, 3));
}

/// An opaque full-output bottom layer, i.e. what the root draw is.
fn opaque_root(w: f32, h: f32) -> CompositeDraw {
    draw_at(0.0, 0.0, w, h, false)
}

fn small_damage() -> Region {
    region_of(&[audit_rect(10, 10, 40, 40)])
}

#[test]
fn clipped_path_is_taken_for_small_damage_under_an_opaque_root() {
    let draws = [opaque_root(800.0, 600.0)];
    let plan = plan_repaint(&small_damage(), &draws, extent(800, 600), true, true);
    assert!(plan.full_reason.is_none());
    let Repaint::Clipped(rect) = plan.repaint else {
        panic!("expected Clipped, got {:?}", plan.repaint);
    };
    assert_eq!(rect, audit_rect(10, 10, 40, 40));
    // `painted` is what the recorder will cover: the bbox.
    assert_eq!(plan.painted.bounding_rect(), Some(rect));
}

#[test]
fn painted_always_covers_what_was_requested() {
    // The invariant `commit_submitted` asserts. Checked here for every gate
    // outcome, because a Full fallback must also claim the whole output.
    let cases: [(&[CompositeDraw], bool, bool); 4] = [
        (&[opaque_root(800.0, 600.0)], true, true),
        (&[], true, true),
        (&[opaque_root(800.0, 600.0)], false, true),
        (&[draw_at(0.0, 0.0, 800.0, 600.0, true)], true, true),
    ];
    for (draws, loadable, shared) in cases {
        let requested = small_damage();
        let plan = plan_repaint(&requested, draws, extent(800, 600), loadable, shared);
        assert!(
            plan.painted.contains(&requested),
            "painted must cover requested for {:?}",
            plan.full_reason
        );
    }
}

#[test]
fn empty_draw_list_forces_full() {
    let plan = plan_repaint(&small_damage(), &[], extent(800, 600), true, true);
    assert_eq!(plan.full_reason, Some(FullReason::EmptyDrawList));
    assert!(matches!(plan.repaint, Repaint::Full(_)));
}

#[test]
fn unloadable_bo_forces_full() {
    let draws = [opaque_root(800.0, 600.0)];
    let plan = plan_repaint(&small_damage(), &draws, extent(800, 600), false, true);
    assert_eq!(plan.full_reason, Some(FullReason::UnloadableBo));
}

#[test]
fn copied_route_forces_full() {
    let draws = [opaque_root(800.0, 600.0)];
    let plan = plan_repaint(&small_damage(), &draws, extent(800, 600), true, false);
    assert_eq!(plan.full_reason, Some(FullReason::CopiedRoute));
}

#[test]
fn a_blended_bottom_layer_forces_full() {
    // Every COW-subtree draw is alpha_passthrough by construction, so a
    // compositing desktop lands here — correctly, and at no cost, since a
    // compositor presents a full-screen surface every frame anyway.
    let draws = [draw_at(0.0, 0.0, 800.0, 600.0, true)];
    let plan = plan_repaint(&small_damage(), &draws, extent(800, 600), true, true);
    assert_eq!(plan.full_reason, Some(FullReason::NoOpaqueCover));
}

#[test]
fn an_opaque_draw_that_does_not_reach_the_damage_forces_full() {
    // Opaque, but only over part of the output: the uncovered part of the
    // region would show whatever the previous compose of this BO left.
    let draws = [draw_at(0.0, 0.0, 20.0, 20.0, false)];
    let plan = plan_repaint(&small_damage(), &draws, extent(800, 600), true, true);
    assert_eq!(plan.full_reason, Some(FullReason::NoOpaqueCover));
}

#[test]
fn a_fractional_edge_does_not_count_as_covering() {
    // dst is f32; rounding inward means a half-pixel short of the damage is
    // not cover. The guard can only ever be conservative.
    let requested = region_of(&[audit_rect(0, 0, 800, 600)]);
    let draws = [draw_at(0.5, 0.0, 800.0, 600.0, false)];
    assert!(!opaque_cover_exists(
        &draws,
        requested.bounding_rect().expect("non-empty")
    ));
}

#[test]
fn damage_above_the_threshold_renders_full() {
    // Below the threshold clips, above it does not; the constant is the only
    // thing that moves between these two.
    let draws = [opaque_root(800.0, 600.0)];
    let below = region_of(&[audit_rect(0, 0, 800, 300)]); // 0.5
    assert!(
        plan_repaint(&below, &draws, extent(800, 600), true, true)
            .full_reason
            .is_none()
    );
    let above = region_of(&[audit_rect(0, 0, 800, 420)]); // 0.7
    assert_eq!(
        plan_repaint(&above, &draws, extent(800, 600), true, true).full_reason,
        Some(FullReason::Threshold)
    );
}

#[test]
fn the_threshold_is_measured_on_what_will_be_painted() {
    // Superseded the earlier "measured on the bounding box" rule when 4.5
    // landed: the box is only what gets painted when the frame renders under
    // a single scissor. Two small rects at opposite corners have a near-full
    // box and a tiny area — under 4.5 they render per rect and must stay
    // clipped, because the box is never rasterised.
    let draws = [opaque_root(800.0, 600.0)];
    let sparse = region_of(&[audit_rect(0, 0, 8, 8), audit_rect(790, 590, 8, 8)]);
    assert!(sparse.area() < 200, "region really is tiny");
    let plan = plan_repaint(&sparse, &draws, extent(800, 600), true, true);
    assert!(plan.full_reason.is_none(), "per-rect keeps this clipped");
    assert_eq!(plan.scissors.len(), 2);

    // The converse — one big scissor being measured on its own area — is
    // pinned by `damage_above_the_threshold_renders_full`.
}

// ── 4.5: per-rect rendering ──────────────────────────────────

#[test]
fn a_single_contiguous_damage_rect_renders_under_one_scissor() {
    let draws = [opaque_root(800.0, 600.0)];
    let plan = plan_repaint(&small_damage(), &draws, extent(800, 600), true, true);
    assert_eq!(plan.scissors.len(), 1);
    assert_eq!(plan.scissors[0], audit_rect(10, 10, 40, 40));
}

#[test]
fn two_separated_rects_render_per_rect_and_painted_excludes_the_gap() {
    // The drag shape: a window's old and new positions. The bounding box
    // spans both plus the empty gap, which measured 36% waste on hardware.
    let draws = [opaque_root(800.0, 600.0)];
    let dragged = region_of(&[audit_rect(0, 0, 100, 100), audit_rect(300, 0, 100, 100)]);
    let plan = plan_repaint(&dragged, &draws, extent(800, 600), true, true);
    assert!(plan.full_reason.is_none());
    assert_eq!(plan.scissors.len(), 2, "should render per rect");
    assert_eq!(plan.painted.area(), 2 * 100 * 100, "the gap is not painted");
    // `repaint` stays the bbox: it is the render area, not the scissor.
    assert!(matches!(plan.repaint, Repaint::Clipped(_)));
}

#[test]
fn adjacent_rects_stay_under_one_scissor() {
    // Touching rects coalesce in the Region, so there is no gap to save and
    // no reason to pay for a second pass.
    let draws = [opaque_root(800.0, 600.0)];
    let touching = region_of(&[audit_rect(0, 0, 50, 50), audit_rect(50, 0, 50, 50)]);
    let plan = plan_repaint(&touching, &draws, extent(800, 600), true, true);
    assert_eq!(plan.scissors.len(), 1);
}

#[test]
fn per_rect_rendering_keeps_frames_off_the_full_path() {
    // Two rects whose bounding box is over the threshold but whose actual
    // area is well under it. Thresholding on the box would render Full and
    // paint the whole screen; thresholding on what will be painted clips.
    let draws = [opaque_root(800.0, 600.0)];
    let spread = region_of(&[audit_rect(0, 0, 100, 100), audit_rect(700, 500, 100, 100)]);
    let bbox_fraction = 800.0 * 600.0 / (800.0 * 600.0);
    assert!(
        bbox_fraction >= CLIPPED_REPAINT_MAX_FRACTION,
        "bbox is the screen"
    );
    let plan = plan_repaint(&spread, &draws, extent(800, 600), true, true);
    assert!(
        plan.full_reason.is_none(),
        "per-rect should have kept this clipped, got {:?}",
        plan.full_reason
    );
    assert_eq!(plan.painted.area(), 2 * 100 * 100);
}

#[test]
fn a_fragmented_region_still_renders_per_rect() {
    // Superseded "too many rects falls back to the box", which pinned the
    // 8-rect cap. That cap made 4.5 stop engaging on MATE, where panels and
    // the desktop fragment a drag region past 8 — reintroducing the 34% box
    // waste the step exists to remove. The bound that matters is the
    // region's own rect cap, and the draw-call cost is scissors × the
    // POST-cull draw count, which is ~4.
    let draws = [opaque_root(800.0, 600.0)];
    let mut scattered = Region::new();
    for i in 0..12 {
        scattered.add_rect(audit_rect(i * 60, i * 40, 10, 10));
    }
    assert_eq!(scattered.rect_count(), 12);
    let plan = plan_repaint(&scattered, &draws, extent(800, 600), true, true);
    assert_eq!(
        plan.scissors.len(),
        12,
        "each fragment gets its own scissor"
    );
    assert_eq!(
        plan.painted.area(),
        12 * 100,
        "and the gaps are not painted"
    );
}

#[test]
fn a_region_past_its_own_cap_arrives_already_collapsed() {
    // `Region` collapses to its extents above MAX_RECTS, so `plan_repaint`
    // can never see an unbounded list — which is why the scissor cap no
    // longer needs to bind.
    let draws = [opaque_root(800.0, 600.0)];
    let mut many = Region::new();
    for i in 0..(Region::MAX_RECTS + 10) {
        many.add_rect(audit_rect((i as i32 % 40) * 20, (i as i32 / 40) * 20, 5, 5));
    }
    assert!(many.rect_count() <= Region::MAX_RECTS);
    let plan = plan_repaint(&many, &draws, extent(800, 600), true, true);
    assert!(plan.scissors.len() <= Region::MAX_RECTS);
}

#[test]
fn scissors_are_disjoint_which_the_overlay_xor_depends_on() {
    // The root IncludeInferiors overlay is not idempotent, so each of its
    // pixels must fall in exactly ONE scissor. Region rects are disjoint by
    // construction; this pins it, including across a band boundary, which is
    // where a hand-rolled rect list would overlap.
    let draws = [opaque_root(800.0, 600.0)];
    let straddling = region_of(&[
        audit_rect(0, 0, 100, 30),
        audit_rect(0, 30, 40, 30),
        audit_rect(300, 0, 100, 100),
    ]);
    let plan = plan_repaint(&straddling, &draws, extent(800, 600), true, true);
    let s = &plan.scissors;
    for (i, a) in s.iter().enumerate() {
        for b in &s[i + 1..] {
            assert!(!rects_intersect(*a, *b), "scissors overlap: {a:?} {b:?}");
        }
    }
    // And they cover exactly the damage, no more.
    let mut covered = Region::new();
    for r in s {
        covered.add_rect(*r);
    }
    if s.len() > 1 {
        assert_eq!(covered, straddling);
    }
}

#[test]
fn a_full_fallback_still_claims_the_whole_output() {
    let plan = plan_repaint(&small_damage(), &[], extent(800, 600), true, true);
    assert_eq!(plan.scissors, vec![audit_rect(0, 0, 800, 600)]);
    assert_eq!(plan.painted.area(), 800 * 600);
}

#[test]
fn culling_drops_draws_outside_the_rect_and_keeps_order() {
    let scene = CompositeScene {
        bg_color: [0.0, 0.0, 0.0, 1.0],
        draws: vec![
            opaque_root(800.0, 600.0),
            draw_at(400.0, 400.0, 50.0, 50.0, true),
            draw_at(10.0, 10.0, 20.0, 20.0, true),
        ],
    };
    let culled = cull_scene_to_region(&scene, &Region::from_rect(audit_rect(0, 0, 100, 100)));
    assert_eq!(culled.draws.len(), 2, "the far draw is culled");
    assert_eq!(culled.draws[0].dst_size, [800.0, 600.0], "root stays first");
    assert_eq!(culled.draws[1].dst_origin, [10.0, 10.0]);
    assert_eq!(culled.bg_color, scene.bg_color);
    // The guard the clipped path depends on must survive its own cull.
    assert!(opaque_cover_exists(
        &culled.draws,
        audit_rect(0, 0, 100, 100)
    ));
}
