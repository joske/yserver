use super::*;

/// `build_with` for one output of a multi-output layout: `elsewhere` is
/// what the OTHER output(s) showed at their last walk (their `pieces_ids`).
fn build_with_elsewhere(
    mode: Visibility,
    core: &KmsCore,
    store: &mut DrawableStore,
    windows: &crate::kms::render::backend::WindowsMap,
    layout: (i32, i32, u32, u32),
    cow_host_xid: Option<u32>,
    elsewhere: &std::collections::HashSet<crate::kms::render::store::DrawableId>,
) -> SceneBuild {
    let platform = platform_with_layout(layout);
    build_scene_with(
        core,
        store,
        windows,
        0,
        &platform,
        None,
        None,
        cow_host_xid,
        false,
        mode,
        elsewhere,
    )
}

// ── Step 1 stage C: content damage clipped to visibility ─────────────

fn drawable_of(store: &DrawableStore, xid: u32) -> crate::kms::render::store::DrawableId {
    store.lookup(xid).expect("fixture window has a drawable")
}

fn projected_sorted(built: &SceneBuild) -> Vec<vk::Rect2D> {
    sorted_rects(built.projected_damage.rects().to_vec())
}

/// A paint into the covered part of a window changes no pixel on screen:
/// it projects nothing, classifies `Hidden`, and must not force a compose.
/// The snapshot is still carried (it acks if something else composes).
#[test]
fn hidden_paint_projects_nothing_and_does_not_force() {
    // Lower 0x100 fully under upper 0x200.
    let (core, mut store, windows) = two_windows((100, 100, 50, 50), (80, 80, 100, 100));
    let lower = drawable_of(&store, 0x100);
    store.damage(lower, rect(5, 5, 20, 20));
    let built = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    assert!(
        built.projected_damage.is_empty(),
        "hidden paint projected {:?}",
        built.projected_damage.rects()
    );
    assert_eq!(built.stats.content_hidden, 1);
    assert_eq!(built.stats.content_off_output, 0);
    assert_eq!(built.stats.content_visible, 0);
    assert!(
        !built.stats.off_output_damage_forces_compose(),
        "hidden damage must take the EmptyDamage skip, not force a Full compose"
    );
    assert!(
        !built.snapshots.iter().any(|s| s.id == lower),
        "a hidden snapshot must NOT ride this output's build: it was not presented \
             here, and carrying it would let this output's retire ack it globally \
             (the multi-output ack race, 2026-09-04)"
    );
    // Un-acked: the store still holds it for the next walk.
    assert!(
        !store
            .peek_presentation_damage(lower)
            .unwrap()
            .region
            .is_empty()
    );
}

/// Codex, post-merge review of `02bafec3` (finding 1): a drawable whose
/// damage classified `Hidden` was still counted as drawn, so
/// `reconcile_offscreen_no_draw` never flagged it and
/// `has_pending_presentation_damage` kept waking the tick — ~1850 walks/s
/// at 2 composes/s with mpv under a terminal. The presented set must leave
/// it out; a drawable with visible damage stays in.
#[test]
fn hidden_damage_is_not_presented_so_the_scheduler_can_go_dormant() {
    // Lower 0x100 fully under upper 0x200; both painted.
    let (core, mut store, windows) = two_windows((100, 100, 50, 50), (80, 80, 100, 100));
    let lower = drawable_of(&store, 0x100);
    let upper = drawable_of(&store, 0x200);
    store.damage(lower, rect(5, 5, 20, 20));
    store.damage(upper, rect(1, 1, 5, 5));
    let built = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    assert_eq!(built.stats.content_hidden, 1);
    assert!(
        built.sampled_ids.contains(&lower),
        "sampling bookkeeping is unchanged: the hidden node is still a participant"
    );
    assert!(
        !built.presented_ids.contains(&lower),
        "hidden damage was not presented: {:?}",
        built.presented_ids
    );
    assert!(built.presented_ids.contains(&upper));
    assert!(
        !built.pieces_ids.contains(&lower),
        "fully covered: no pieces either ⇒ NoPieces, stays dormant across paints"
    );
    let drawn: std::collections::HashSet<_> = built.presented_ids.iter().copied().collect();
    let pieces: std::collections::HashSet<_> = built.pieces_ids.iter().copied().collect();
    store.reconcile_offscreen_no_draw(&drawn, &pieces);
    assert_eq!(
        store.get(lower).unwrap().dormant,
        Some(crate::kms::render::store::DormantReason::NoPieces),
        "flagged out of the scheduler"
    );
    assert!(store.get(upper).unwrap().dormant.is_none());
    assert!(
        !store
            .peek_presentation_damage(lower)
            .unwrap()
            .region
            .is_empty(),
        "damage is preserved, only the flag changes"
    );

    // A PARTIALLY covered node whose damage lies entirely under the cover:
    // it emits pieces (it is drawn) but presented nothing of its paint.
    let (core, mut store, windows) = two_windows((100, 100, 200, 200), (200, 100, 200, 200));
    let lower = drawable_of(&store, 0x100);
    // Storage-local x 150..190 → output x 250..290, under the cover (x ≥ 200).
    store.damage(lower, rect(150, 50, 40, 20));
    let built = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    assert_eq!(built.stats.content_hidden, 1);
    assert!(built.stats.draws_emitted > 0);
    assert!(built.sampled_ids.contains(&lower));
    assert!(!built.presented_ids.contains(&lower));
    assert!(
        built.pieces_ids.contains(&lower),
        "partially covered: it emitted pieces ⇒ HiddenDamage, re-armed by the next paint"
    );
    let drawn: std::collections::HashSet<_> = built.presented_ids.iter().copied().collect();
    let pieces: std::collections::HashSet<_> = built.pieces_ids.iter().copied().collect();
    store.reconcile_offscreen_no_draw(&drawn, &pieces);
    assert_eq!(
        store.get(lower).unwrap().dormant,
        Some(crate::kms::render::store::DormantReason::HiddenDamage)
    );
    assert!(
        !store.has_pending_presentation_damage(),
        "nothing presentable is pending: the scheduler must go dormant"
    );
    // The next paint lands in the VISIBLE part (storage-local x 10..40 →
    // output 110..140, left of the cover): it must re-arm and present.
    store.damage(lower, rect(10, 10, 30, 30));
    assert!(
        store.has_pending_presentation_damage(),
        "a paint into a HiddenDamage-dormant window re-arms the scheduler"
    );
    let built = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    assert_eq!(built.stats.content_visible, 1);
    assert!(built.presented_ids.contains(&lower));
}

/// Two outputs: hidden on output 0, visible on output 1. The union of the
/// two outputs' presented sets contains it, so the drawable stays armed and
/// output 1 composes it.
#[test]
fn damage_visible_on_one_output_keeps_the_drawable_armed() {
    let (core, mut store, windows) = two_windows((700, 100, 200, 100), (650, 0, 150, 600));
    let w = drawable_of(&store, 0x100);
    store.damage(w, rect(0, 0, 200, 100));
    let out0 = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    let out1 = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (800, 0, 800, 600),
        None,
    );
    assert_eq!(out0.stats.content_hidden, 1);
    assert!(!out0.presented_ids.contains(&w));
    assert_eq!(out1.stats.content_visible, 1);
    assert!(out1.presented_ids.contains(&w));
    let mut drawn: std::collections::HashSet<_> = out0.presented_ids.iter().copied().collect();
    drawn.extend(out1.presented_ids.iter().copied());
    let mut pieces: std::collections::HashSet<_> = out0.pieces_ids.iter().copied().collect();
    pieces.extend(out1.pieces_ids.iter().copied());
    store.reconcile_offscreen_no_draw(&drawn, &pieces);
    assert!(store.get(w).unwrap().dormant.is_none());
    assert!(store.has_pending_presentation_damage());
}

/// Paint straddling a cover's edge projects only the visible side.
#[test]
fn paint_across_a_cover_edge_projects_only_the_visible_side() {
    // Lower 0x100 at x 100..300; upper 0x200 covers x 200..400.
    let (core, mut store, windows) = two_windows((100, 100, 200, 200), (200, 100, 200, 200));
    let lower = drawable_of(&store, 0x100);
    // Storage-local (50,50)+(100x20) → output (150,150)+(100x20); visible x 150..200.
    store.damage(lower, rect(50, 50, 100, 20));
    let built = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    assert_eq!(projected_sorted(&built), vec![rect(150, 150, 50, 20)]);
    assert_eq!(built.stats.content_visible, 1);
    assert_eq!(built.stats.content_hidden, 0);
    // Paint entirely in the visible half projects whole.
    let (core, mut store, windows) = two_windows((100, 100, 200, 200), (200, 100, 200, 200));
    let lower = drawable_of(&store, 0x100);
    store.damage(lower, rect(10, 10, 30, 30));
    let built = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    assert_eq!(projected_sorted(&built), vec![rect(110, 110, 30, 30)]);
}

/// The xfce-submenu case is pinned: a paint whose projection misses the
/// output entirely still forces a compose, so the snapshot can ack.
#[test]
fn off_output_paint_still_forces_a_compose() {
    // 0x100 straddles the right edge: x 750..850 on an 800-wide output.
    let (core, mut store, windows) = two_windows((750, 100, 100, 50), (10, 10, 20, 20));
    let w = drawable_of(&store, 0x100);
    // Storage-local x 60..90 → output x 810..840: off the output.
    store.damage(w, rect(60, 5, 30, 10));
    let built = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    assert!(built.projected_damage.is_empty());
    assert_eq!(built.stats.content_off_output, 1);
    assert_eq!(built.stats.content_hidden, 0);
    assert!(built.stats.off_output_damage_forces_compose());
    // And a paint on the on-output part of the same window is Visible.
    let (core, mut store, windows) = two_windows((750, 100, 100, 50), (10, 10, 20, 20));
    let w = drawable_of(&store, 0x100);
    store.damage(w, rect(10, 5, 30, 10));
    let built = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    assert_eq!(projected_sorted(&built), vec![rect(760, 105, 30, 10)]);
    assert_eq!(built.stats.content_visible, 1);
    assert!(!built.stats.off_output_damage_forces_compose());
}

/// Hidden damage is not acked; it accumulates and shows when uncovered.
/// Frame 1: W paints under A (Hidden). Frame 2: A moves away — A's
/// structural old ∪ new covers W, and W's accumulated damage projects.
#[test]
fn uncovering_a_window_surfaces_its_accumulated_hidden_paint() {
    let (core, mut store, mut windows) = two_windows((100, 100, 50, 50), (80, 80, 100, 100));
    let lower = drawable_of(&store, 0x100);
    store.damage(lower, rect(5, 5, 20, 20));
    let frame1 = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    assert!(frame1.projected_damage.is_empty());
    assert_eq!(frame1.stats.content_hidden, 1);
    // Not acked (no compose happened): a second hidden paint accumulates.
    store.damage(lower, rect(30, 30, 10, 10));
    // Frame 2: A moves off W.
    let a = windows.get_mut(&0x200).expect("A");
    a.x = 400;
    a.y = 400;
    let frame2 = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    assert_eq!(
        projected_sorted(&frame2),
        vec![rect(105, 105, 20, 20), rect(130, 130, 10, 10)],
        "both accumulated paints project once W is visible"
    );
    assert_eq!(frame2.stats.content_visible, 1);
    let structural = structural_damage(&frame1.participants, &frame2.participants);
    assert!(
        structural.contains_rect(rect(100, 100, 50, 50)),
        "the mover's old ∪ new covers the uncovered window: {structural:?}"
    );
}

/// Two outputs, one store: W hidden on output 0 is visible on output 1.
/// Output 0 classifies Hidden and does not force; output 1 projects it. No
/// ack happens on the hidden side, which is what keeps output 1 correct.
#[test]
fn hidden_on_one_output_visible_on_the_other() {
    // W 0x100 at x 700..900 spans both outputs; A 0x200 covers x 650..800.
    let (core, mut store, windows) = two_windows((700, 100, 200, 100), (650, 0, 150, 600));
    let w = drawable_of(&store, 0x100);
    store.damage(w, rect(0, 0, 200, 100));
    let out0 = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    assert!(out0.projected_damage.is_empty());
    assert_eq!(out0.stats.content_hidden, 1);
    assert!(!out0.stats.off_output_damage_forces_compose());
    // Still in the store for output 1's walk.
    assert!(!store.peek_presentation_damage(w).unwrap().region.is_empty());
    let out1 = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (800, 0, 800, 600),
        None,
    );
    assert_eq!(projected_sorted(&out1), vec![rect(0, 100, 100, 100)]);
    assert_eq!(out1.stats.content_visible, 1);
    // Only the presenting output carries the snapshot into its PendingAck.
    assert!(
        !out0.snapshots.iter().any(|s| s.id == w),
        "output 0 (hidden) must not carry W's snapshot"
    );
    assert!(
        out1.snapshots
            .iter()
            .any(|s| s.id == w && !s.region.is_empty()),
        "output 1 (visible) carries it and acks it at its retire"
    );
}

/// Hidden on output 0 (walked), but output 1 skipped this tick and its
/// retained pieces include the drawable: it may present it once it walks,
/// so it stays armed.
#[test]
fn dormancy_keeps_a_drawable_armed_when_a_skipped_output_may_present_it() {
    let none = set(&[]);
    let out0_pieces = set(&[7, 9]);
    let out0_presented = set(&[9]);
    let out1_last = set(&[7]);
    let reports = [
        OutputWalkReport {
            walked: true,
            presented: &out0_presented,
            last_pieces: &out0_pieces,
        },
        OutputWalkReport {
            walked: false,
            presented: &none,
            last_pieces: &out1_last,
        },
    ];
    let (keep_armed, pieces) = dormancy_inputs(&reports);
    assert!(keep_armed.contains(&set(&[7]).into_iter().next().unwrap()));
    assert!(keep_armed.contains(&set(&[9]).into_iter().next().unwrap()));
    assert!(pieces.contains(&set(&[7]).into_iter().next().unwrap()));
}

/// Same, but output 1 walked and did not present it either: dormant with
/// reason `HiddenDamage`, since it has pieces.
#[test]
fn dormancy_flags_hidden_damage_when_every_walked_output_declined_it() {
    let out0_pieces = set(&[7, 9]);
    let out0_presented = set(&[9]);
    let out1_pieces = set(&[7]);
    let out1_presented = set(&[]);
    let reports = [
        OutputWalkReport {
            walked: true,
            presented: &out0_presented,
            last_pieces: &out0_pieces,
        },
        OutputWalkReport {
            walked: true,
            presented: &out1_presented,
            last_pieces: &out1_pieces,
        },
    ];
    let (keep_armed, pieces) = dormancy_inputs(&reports);
    let seven = set(&[7]).into_iter().next().unwrap();
    assert!(!keep_armed.contains(&seven));
    assert!(pieces.contains(&seven), "⇒ HiddenDamage");
}

/// In no output's pieces, output 1 skipped: `NoPieces`.
#[test]
fn dormancy_flags_no_pieces_when_no_output_shows_it() {
    let out0_pieces = set(&[9]);
    let out0_presented = set(&[9]);
    let none = set(&[]);
    let out1_last = set(&[11]);
    let reports = [
        OutputWalkReport {
            walked: true,
            presented: &out0_presented,
            last_pieces: &out0_pieces,
        },
        OutputWalkReport {
            walked: false,
            presented: &none,
            last_pieces: &out1_last,
        },
    ];
    let (keep_armed, pieces) = dormancy_inputs(&reports);
    let seven = set(&[7]).into_iter().next().unwrap();
    assert!(!keep_armed.contains(&seven));
    assert!(!pieces.contains(&seven), "⇒ NoPieces");
    // And 11, shown only on the skipped output, stays armed.
    assert!(keep_armed.contains(&set(&[11]).into_iter().next().unwrap()));
}

/// The hardware case (silence/MATE 2026-09-04): the root's damage lies
/// under covers on both outputs; output 1 keeps skipping as NothingPending.
/// After output 0's walk alone the root must go dormant (`HiddenDamage`:
/// it has pieces on both), or it is re-peeked and re-classified Hidden
/// ~1000×/s forever.
#[test]
fn dormancy_runs_without_every_output_walking() {
    let root = set(&[1]);
    let out0_pieces = set(&[1, 5]);
    let out0_presented = set(&[5]);
    let none = set(&[]);
    let out1_last = set(&[1, 6]);
    let reports = [
        OutputWalkReport {
            walked: true,
            presented: &out0_presented,
            last_pieces: &out0_pieces,
        },
        OutputWalkReport {
            walked: false,
            presented: &none,
            last_pieces: &out1_last,
        },
    ];
    let (keep_armed, pieces) = dormancy_inputs(&reports);
    let one = root.into_iter().next().unwrap();
    // Output 1 has pieces for the root and did not walk ⇒ it MAY present
    // it ⇒ armed. That is the sound rule; what makes it terminate on
    // hardware is that output 1's predicate then walks (root armed and in
    // its last_pieces), declines it (Hidden), and the NEXT reconciliation
    // sees both outputs decline ⇒ dormant.
    assert!(keep_armed.contains(&one));
    let out1_pieces = set(&[1, 6]);
    let out1_presented = set(&[6]);
    let reports = [
        OutputWalkReport {
            walked: true,
            presented: &out0_presented,
            last_pieces: &out0_pieces,
        },
        OutputWalkReport {
            walked: true,
            presented: &out1_presented,
            last_pieces: &out1_pieces,
        },
    ];
    let (keep_armed, pieces2) = dormancy_inputs(&reports);
    assert!(!keep_armed.contains(&one));
    assert!(pieces2.contains(&one), "⇒ HiddenDamage");
    let _ = pieces;
}

/// The multi-output ack race, first half (silence/MATE, 2026-09-04): a
/// paint into a window spanning both outputs, landing inside output 0 only.
/// Output 1 knows (from output 0's retained pieces) that the window is shown
/// there, so it classifies `OtherOutput`: no force, no snapshot, not
/// presented — output 0 owns that damage. Before this rule output 1 read it
/// as `OffOutput`, forced a Full compose, and its retire acked the damage
/// before output 0 had composed it.
#[test]
fn other_output_damage_is_neither_forced_nor_carried_here() {
    // W 0x100 at x 700..900 spans the boundary at 800; 0x200 is far away.
    let (core, mut store, windows) = two_windows((700, 100, 200, 100), (0, 0, 10, 10));
    let w = drawable_of(&store, 0x100);
    // Storage-local x 0..50 → output-0 x 700..750 only.
    store.damage(w, rect(0, 0, 50, 100));
    let out0 = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    assert_eq!(out0.stats.content_visible, 1);
    assert!(
        out0.snapshots
            .iter()
            .any(|s| s.id == w && !s.region.is_empty())
    );
    assert!(out0.presented_ids.contains(&w));
    let elsewhere: std::collections::HashSet<_> = out0.pieces_ids.iter().copied().collect();
    let out1 = build_with_elsewhere(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (800, 0, 800, 600),
        None,
        &elsewhere,
    );
    assert_eq!(out1.stats.content_other_output, 1, "{:?}", out1.stats);
    assert_eq!(out1.stats.content_off_output, 0);
    assert!(
        !out1.stats.off_output_damage_forces_compose(),
        "damage another output presents must not force a Full compose here"
    );
    assert!(
        !out1.snapshots.iter().any(|s| s.id == w),
        "output 1 must not carry (and later ack) damage it did not present"
    );
    assert!(!out1.presented_ids.contains(&w));
    assert!(
        out1.pieces_ids.contains(&w),
        "W's right half is visible on output 1, so it has pieces there"
    );
    assert!(out1.projected_damage.is_empty());
}

/// The xfce-submenu rule, pinned: an off-output paint still FORCES a
/// compose here, so a paint whose projection is empty is not left
/// undrained. It is deliberately **not carried and not presented** — the
/// forced compose displays none of those pixels, and carrying the snapshot
/// is what let a cold `elsewhere` turn this branch into the multi-output
/// ack race (2026-09-04: caja's spanning desktop, 247 unhealed audit
/// mismatches). The drain comes from dormancy instead: not presented ⇒
/// dormant ⇒ no re-forcing until the next paint. A window entirely off the
/// output never reaches the classifier at all (the intersects gate), so the
/// fixture is a spanning window whose damage projects off output 0.
#[test]
fn damage_off_output_forces_a_compose_but_is_never_carried() {
    let (core, mut store, windows) = two_windows((700, 100, 200, 100), (0, 0, 10, 10));
    let w = drawable_of(&store, 0x100);
    // Storage-local x 150..200 → x 850..900: off output 0, on output 1.
    store.damage(w, rect(150, 0, 50, 100));
    let nowhere = std::collections::HashSet::new();
    let out0 = build_with_elsewhere(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
        &nowhere,
    );
    assert_eq!(out0.stats.content_off_output, 1, "{:?}", out0.stats);
    assert_eq!(out0.stats.content_other_output, 0);
    assert!(
        out0.stats.off_output_damage_forces_compose(),
        "an empty projection must still force a compose (xfce submenu)"
    );
    assert!(
        !out0.snapshots.iter().any(|s| s.id == w),
        "the forced compose shows none of those pixels, so it must not ack them"
    );
    assert!(
        !out0.presented_ids.contains(&w),
        "not presented ⇒ dormancy stops the forcing until the next paint"
    );
    // Once output 1's pieces are known, the same paint is output 1's.
    let out1 = build_with_elsewhere(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (800, 0, 800, 600),
        None,
        &nowhere,
    );
    assert_eq!(out1.stats.content_visible, 1);
    let elsewhere: std::collections::HashSet<_> = out1.pieces_ids.iter().copied().collect();
    let out0 = build_with_elsewhere(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
        &elsewhere,
    );
    assert_eq!(out0.stats.content_other_output, 1);
    assert!(!out0.stats.off_output_damage_forces_compose());
    assert!(!out0.snapshots.iter().any(|s| s.id == w));
}

/// The multi-output ack race, second half: output 0 is flip-pending when
/// the paint lands, so only output 1 walks; output 1 composes for its own
/// reasons, retires, and acks what it carried. W's damage must survive that
/// ack, and output 0's next walk must project it.
#[test]
fn an_output_never_acks_damage_it_did_not_present() {
    let (core, mut store, windows) = two_windows((700, 100, 200, 100), (0, 0, 10, 10));
    let w = drawable_of(&store, 0x100);
    // Output 0's most recent walk saw W (its retained pieces); no paint yet.
    let warm = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    let elsewhere: std::collections::HashSet<_> = warm.pieces_ids.iter().copied().collect();
    assert!(elsewhere.contains(&w));
    // The paint lands while output 0 is flip-pending: only output 1 walks.
    store.damage(w, rect(0, 0, 50, 100));
    let out1 = build_with_elsewhere(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (800, 0, 800, 600),
        None,
        &elsewhere,
    );
    // Output 1 composes (say, for its own cursor) and retires: it acks
    // exactly what it carried.
    for snap in out1.snapshots {
        store.ack_presentation_damage(snap);
    }
    assert!(
        !store.peek_presentation_damage(w).unwrap().region.is_empty(),
        "output 1 never presented W's damage, so its retire must not have acked it"
    );
    // Output 0 retires and walks: the highlight is still there to compose.
    let out0 = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    assert_eq!(projected_sorted(&out0), vec![rect(700, 100, 50, 100)]);
    assert!(
        out0.snapshots
            .iter()
            .any(|s| s.id == w && !s.region.is_empty())
    );
}

/// The multi-output ack race for a paint spanning both outputs: output 0
/// is flip-pending when it lands, output 1 composes it, and output 1's
/// retire acks it for everyone. The compose carries it in root
/// coordinates, and output 0 is handed its share as structure damage.
#[test]
fn a_spanning_paint_is_handed_to_the_output_that_did_not_compose_it() {
    let (core, mut store, windows) = two_windows((700, 100, 200, 100), (0, 0, 10, 10));
    let w = drawable_of(&store, 0x100);
    let warm = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    let out0_pieces: std::collections::HashSet<_> = warm.pieces_ids.iter().copied().collect();
    let out0_presented: std::collections::HashMap<_, _> =
        warm.snapshots.iter().map(|s| (s.id, s.epoch)).collect();
    store.damage(w, rect(0, 0, 200, 100));
    let out1 = build_with_elsewhere(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (800, 0, 800, 600),
        None,
        &out0_pieces,
    );
    let carried: Vec<_> = out1.carried.iter().filter(|c| c.id == w).collect();
    assert_eq!(carried.len(), 1);
    assert_eq!(carried[0].root, vec![rect(700, 100, 200, 100)]);
    let mut out0_damage = RegionSet::new();
    assert!(fan_out_to_output(
        (0, 0),
        extent(800, 600),
        &out0_pieces,
        &out0_presented,
        &mut out0_damage,
        &out1.carried,
    ));
    assert_eq!(out0_damage.rects(), &[rect(700, 100, 100, 100)]);
    for snap in out1.snapshots {
        store.ack_presentation_damage(snap);
    }
    let out0 = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    assert!(
        projected_sorted(&out0).is_empty(),
        "the store no longer holds it: the structure damage is all output 0 gets"
    );
}

/// An output that composed the drawable at that epoch or newer, or on
/// which it had no pieces, takes nothing.
#[test]
fn fan_out_skips_outputs_that_show_it_or_cannot() {
    let (_, store, _) = two_windows((700, 100, 200, 100), (0, 0, 10, 10));
    let w = drawable_of(&store, 0x100);
    let carried = [CarriedDamage {
        id: w,
        epoch: 5,
        root: vec![rect(700, 100, 200, 100)],
    }];
    let pieces: std::collections::HashSet<_> = [w].into_iter().collect();
    let mut damage = RegionSet::new();
    let at = |e: u64| -> std::collections::HashMap<_, _> { [(w, e)].into_iter().collect() };
    for presented in [at(5), at(6)] {
        assert!(!fan_out_to_output(
            (0, 0),
            extent(800, 600),
            &pieces,
            &presented,
            &mut damage,
            &carried,
        ));
    }
    assert!(!fan_out_to_output(
        (0, 0),
        extent(800, 600),
        &std::collections::HashSet::new(),
        &at(4),
        &mut damage,
        &carried,
    ));
    assert!(damage.is_empty());
    assert!(fan_out_to_output(
        (0, 0),
        extent(800, 600),
        &pieces,
        &at(4),
        &mut damage,
        &carried,
    ));
}

/// `Off` keeps the unclipped projection: what the legacy emitter damaged.
#[test]
fn off_mode_projection_is_unchanged_by_stage_c() {
    let (core, mut store, windows) = differential_fixture();
    // Paint into several windows, including ones the fixture covers.
    let xids: Vec<u32> = windows.keys().copied().collect();
    for (i, xid) in xids.iter().enumerate() {
        if let Some(id) = store.lookup(*xid) {
            #[allow(clippy::cast_possible_truncation)]
            let k = (i % 7) as i32;
            store.damage(id, rect(k, k, 8 + k as u32, 6));
        }
    }
    for layout in [(0, 0, 800, 600), (100, 50, 800, 600)] {
        let legacy = walk_with(true, &core, &mut store, &windows, layout, None);
        let off = walk_with(false, &core, &mut store, &windows, layout, None);
        assert!(
            !legacy.projected.is_empty(),
            "fixture damage must project at layout {layout:?}"
        );
        assert_eq!(off.projected, legacy.projected, "layout {layout:?}");
    }
    // And `On` projects a subset of `Off` (never more).
    let off = build_with(
        Visibility::Off,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    let on = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    let off_region = Region::from_rects(off.projected_damage.rects().iter().copied());
    for r in on.projected_damage.rects() {
        assert!(
            off_region.contains_rect(*r),
            "On projected {r:?} outside Off"
        );
    }
}

#[test]
fn only_off_output_damage_forces() {
    let mut s = WalkStats::default();
    assert!(!s.off_output_damage_forces_compose());
    s.content_hidden = 5;
    s.content_visible = 2;
    assert!(!s.off_output_damage_forces_compose());
    s.content_off_output = 1;
    assert!(s.off_output_damage_forces_compose());
}

#[test]
fn intersect_rects_clips_and_rejects_disjoint() {
    assert_eq!(
        intersect_rects(rect(0, 0, 10, 10), rect(5, 5, 10, 10)),
        Some(rect(5, 5, 5, 5))
    );
    assert_eq!(intersect_rects(rect(0, 0, 10, 10), rect(10, 0, 5, 5)), None);
    assert_eq!(
        intersect_rects(rect(0, 0, 10, 10), rect(-5, -5, 30, 30)),
        Some(rect(0, 0, 10, 10))
    );
}

// ── Step 1 stage B: walk cost bench ──────────────────────────────────

/// An e16-like tree on one 2560×1440 output: 10 unshaped top-levels, each
/// with 6 shaped leaf children of 8 rects, plus one large opaque window
/// covering half the screen, over a root.
fn e16_like_fixture() -> (
    KmsCore,
    DrawableStore,
    crate::kms::render::backend::WindowsMap,
) {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let mut windows = crate::kms::render::backend::WindowsMap::new();
    alloc_root(&core, &mut store, 2560, 1440);
    let mut rank = 1u64;
    for t in 0..10i16 {
        let top = 0x1000 + u32::try_from(t).unwrap() * 0x10;
        let (tx, ty) = (40 + (t % 5) * 480, 60 + (t / 5) * 640);
        alloc_stub_window(&mut store, &mut windows, top, tx, ty, 440, 560, None, true);
        set_rank(&mut windows, top, rank);
        rank += 1;
        core.top_level_order.push(top);
        for c in 0..6i16 {
            let child = top + 1 + u32::try_from(c).unwrap();
            let (cx, cy) = (10 + (c % 3) * 140, 20 + (c / 3) * 260);
            alloc_stub_window(
                &mut store,
                &mut windows,
                child,
                cx,
                cy,
                128,
                240,
                Some(top),
                true,
            );
            set_rank(&mut windows, child, rank);
            rank += 1;
            // A frame-like shape: 4 edges + 4 corner nubs, all disjoint.
            core.shape_bounding.insert(
                child,
                vec![
                    xfixes::RegionRect {
                        x: 0,
                        y: 0,
                        width: 128,
                        height: 8,
                    },
                    xfixes::RegionRect {
                        x: 0,
                        y: 232,
                        width: 128,
                        height: 8,
                    },
                    xfixes::RegionRect {
                        x: 0,
                        y: 8,
                        width: 8,
                        height: 224,
                    },
                    xfixes::RegionRect {
                        x: 120,
                        y: 8,
                        width: 8,
                        height: 224,
                    },
                    xfixes::RegionRect {
                        x: 8,
                        y: 8,
                        width: 16,
                        height: 16,
                    },
                    xfixes::RegionRect {
                        x: 104,
                        y: 8,
                        width: 16,
                        height: 16,
                    },
                    xfixes::RegionRect {
                        x: 8,
                        y: 216,
                        width: 16,
                        height: 16,
                    },
                    xfixes::RegionRect {
                        x: 104,
                        y: 216,
                        width: 16,
                        height: 16,
                    },
                ],
            );
        }
    }
    // The big opaque window on top, covering the right half.
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x9000,
        1280,
        0,
        1280,
        1440,
        None,
        true,
    );
    set_rank(&mut windows, 0x9000, rank);
    core.top_level_order.push(0x9000);
    (core, store, windows)
}

/// `cargo test --release -p yserver -- --ignored walk_bench --nocapture`
#[test]
#[ignore = "bench: prints µs per walk, run in release with --nocapture"]
fn walk_bench() {
    let (core, mut store, windows) = e16_like_fixture();
    let layout = (0, 0, 2560u32, 1440u32);
    let platform = platform_with_layout(layout);
    let run = |mode: Visibility, store: &mut DrawableStore| {
        build_scene(
            &core, store, &windows, 0, &platform, None, None, None, false, mode,
        )
    };
    let warm = run(Visibility::On, &mut store);
    eprintln!("walk_bench: collapse split {:?}", warm.stats);
    eprintln!(
        "walk_bench: nodes={} draws={} hidden={} collapses={} (off draws={})",
        warm.stats.nodes_visited,
        warm.scene.draws.len(),
        warm.stats.hidden_participants,
        warm.stats.collapses(),
        run(Visibility::Off, &mut store).scene.draws.len(),
    );
    for mode in [Visibility::Off, Visibility::On] {
        let mut best = f64::MAX;
        for _ in 0..5 {
            let start = std::time::Instant::now();
            for _ in 0..1000 {
                let b = run(mode, &mut store);
                std::hint::black_box(&b);
            }
            let per = start.elapsed().as_secs_f64() * 1e6 / 1000.0;
            best = best.min(per);
        }
        eprintln!("walk_bench: {mode:?}: best of 5 runs = {best:.1} µs/walk");
    }
}
