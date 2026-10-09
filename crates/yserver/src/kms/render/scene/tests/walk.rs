use super::*;

#[test]
fn refactored_emitter_matches_the_legacy_emitter_exactly() {
    for layout in [
        (0, 0, 800u32, 600u32),
        (100, 50, 2560, 1440),
        (-300, -200, 800, 600),
        (123, 45, 640, 480),
    ] {
        for cow in [None, Some(0x800u32)] {
            let (core, mut store, windows) = differential_fixture();
            let legacy = walk_with(true, &core, &mut store, &windows, layout, cow);
            let new = walk_with(false, &core, &mut store, &windows, layout, cow);
            assert!(
                !legacy.draws.is_empty(),
                "fixture sanity: the tree must emit something at layout {layout:?}"
            );
            assert_eq!(new, legacy, "layout {layout:?} cow {cow:?}");
        }
    }
}

/// The fixture exercises the gates it claims to: count what each case
/// contributes so a silent no-op fixture cannot pass the test above.
#[test]
fn differential_fixture_exercises_every_gate() {
    let (core, mut store, windows) = differential_fixture();
    let out = walk_with(
        false,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        Some(0x800),
    );
    let views: Vec<u64> = out.draws.iter().map(|d| d.view).collect();
    let has = |xid: u32| views.contains(&(u64::from(xid) | 0xFF00_0000));
    let has_backing = |xid: u32| views.contains(&(u64::from(xid) | 0xB000_0000));
    assert!(
        has(0x100) && has(0x101) && has(0x102),
        "nesting emits all three"
    );
    assert!(!has(0x103) && !has(0x104), "unmapped subtree emits nothing");
    assert!(has(0x200) && has(0x201), "overlapping siblings both emit");
    assert_eq!(
        out.draws
            .iter()
            .filter(|d| d.view == u64::from(0x300u32) | 0xFF00_0000)
            .count(),
        5,
        "shaped node emits one draw per rect (the fifth clamped, not dropped)"
    );
    assert!(has(0x401), "oversized child emits, clipped to its parent");
    assert!(
        !has(0x402),
        "grandchild clipped away entirely emits nothing"
    );
    assert!(has(0x500), "straddling window emits");
    assert!(!has(0x501), "off-output window does not emit");
    assert!(
        !has(0x600) && !has_backing(0xB600),
        "manual-redirected node never emits"
    );
    assert!(
        has_backing(0xB601),
        "automatic child under a manual ancestor emits its backing"
    );
    assert!(
        !has(0x602),
        "plain child under a manual ancestor paints into the ancestor"
    );
    assert!(
        has_backing(0xB700) && !has(0x700),
        "automatic top-level samples its backing"
    );
    assert!(
        out.draws.iter().filter(|d| d.alpha_passthrough).count() == 2,
        "exactly the COW and its stage are alpha_passthrough"
    );
    assert!(!has(0x900), "geometry without storage emits nothing");
}

// ── Step 1 stage A: opaque cover is a union, tested by subtraction ────

#[test]
fn opaque_cover_accepts_a_union_of_draws_across_a_window_edge() {
    // Root fragmented around a window at (200,150)-(400,350), plus the window.
    let draws = [
        draw_at(0.0, 0.0, 800.0, 150.0, false),     // above
        draw_at(0.0, 350.0, 800.0, 250.0, false),   // below
        draw_at(0.0, 150.0, 200.0, 200.0, false),   // left
        draw_at(400.0, 150.0, 400.0, 200.0, false), // right
        draw_at(200.0, 150.0, 200.0, 200.0, false), // the window
    ];
    assert!(opaque_cover_exists(&draws, audit_rect(150, 100, 100, 100)));
    assert!(opaque_cover_exists(&draws, audit_rect(0, 0, 800, 600)));
}

#[test]
fn opaque_cover_rejects_a_one_pixel_gap_in_the_union() {
    let draws = [
        draw_at(0.0, 0.0, 800.0, 150.0, false),
        draw_at(0.0, 351.0, 800.0, 249.0, false), // leaves row 350 uncovered
        draw_at(0.0, 150.0, 200.0, 200.0, false),
        draw_at(400.0, 150.0, 400.0, 200.0, false),
        draw_at(200.0, 150.0, 200.0, 200.0, false),
    ];
    assert!(!opaque_cover_exists(&draws, audit_rect(150, 300, 100, 100)));
    // A translucent draw over the gap does not close it.
    let mut with_alpha = draws.to_vec();
    with_alpha.push(draw_at(0.0, 340.0, 800.0, 20.0, true));
    assert!(!opaque_cover_exists(
        &with_alpha,
        audit_rect(150, 300, 100, 100)
    ));
}

#[test]
fn clipped_path_with_two_scissors_covered_by_different_draws() {
    // No root: two opaque windows, each covering one damage rect.
    let draws = [
        draw_at(0.0, 0.0, 400.0, 600.0, false),
        draw_at(400.0, 0.0, 400.0, 600.0, false),
    ];
    // Two small rects far apart so the bbox is wasteful and 4.5 splits.
    let damage = region_of(&[audit_rect(10, 10, 20, 20), audit_rect(700, 500, 20, 20)]);
    let plan = plan_repaint(&damage, &draws, extent(800, 600), true, true);
    assert!(plan.full_reason.is_none(), "{:?}", plan.full_reason);
    assert_eq!(plan.scissors.len(), 2);
    assert!(
        plan.scissors
            .iter()
            .all(|r| opaque_cover_exists(&draws, *r))
    );
}

// ── Step 1 stage A: an incomplete submit never stages `painted` ──────

/// After an `invalidate` the model owes a full repaint; a tick that finds no
/// producer damage must compose anyway, not take the EmptyDamage skip.
#[test]
fn an_owed_repaint_is_not_an_empty_damage_skip() {
    assert!(skip_for_empty_damage(true, false, false), "idle skips");
    assert!(
        !skip_for_empty_damage(false, false, false),
        "damage composes"
    );
    assert!(
        !skip_for_empty_damage(true, true, false),
        "first frame composes"
    );
    assert!(
        !skip_for_empty_damage(true, false, true),
        "owed repaint (invalidated BO) must compose"
    );
}

#[test]
fn incomplete_submit_invalidates_instead_of_staging() {
    let ext = extent(800, 600);
    let mut damage = ScanoutDamage::new(2, ext);
    let repaint = region_of(&[audit_rect(10, 10, 40, 40)]);
    // Complete: staged as usual.
    stage_submitted_frame(&mut damage, true, 0, &repaint, &repaint);
    assert!(damage.has_staged_frame());
    damage.retire_success();
    assert!(!damage.has_staged_frame());
    // Incomplete: nothing staged, and every BO owes the whole output again.
    stage_submitted_frame(&mut damage, false, 1, &repaint, &repaint);
    assert!(!damage.has_staged_frame());
    for bo in 0..2 {
        assert_eq!(
            damage.missing_area(bo),
            u64::from(ext.width) * u64::from(ext.height),
            "bo {bo} must owe the full output after an incomplete submit"
        );
    }
}

/// The reversal proof with a root present: under `Visibility::Off`,
/// `build_scene` produces exactly what the pre-step-1 code produced — the
/// root draw first, then the legacy emitter's list — bit for bit.
#[test]
fn off_mode_reproduces_the_legacy_root_and_emitter_exactly() {
    for layout in [
        (0, 0, 800u32, 600u32),
        (2560, 0, 2560, 1440),
        (-300, -200, 800, 600),
    ] {
        for cow in [None, Some(0x800u32)] {
            let (core, mut store, windows) = differential_fixture();
            alloc_root(&core, &mut store, 5120, 1440);
            let legacy = walk_with(true, &core, &mut store, &windows, layout, cow);
            let off = build_with(Visibility::Off, &core, &mut store, &windows, layout, cow);
            // The legacy root draw, as the old `build_scene` pushed it.
            let root_key = DrawKey {
                view: 0x00A0_7000,
                dst_origin: [(-layout.0) as f32, (-layout.1) as f32].map(f32::to_bits),
                dst_size: [5120.0f32, 1440.0f32].map(f32::to_bits),
                src_origin: [0.0f32, 0.0f32].map(f32::to_bits),
                src_size: [1.0f32, 1.0f32].map(f32::to_bits),
                alpha_passthrough: false,
            };
            let mut expect_draws = vec![root_key];
            expect_draws.extend(legacy.draws);
            let got = walk_out_of(&off);
            assert_eq!(
                got.draws, expect_draws,
                "draws, layout {layout:?} cow {cow:?}"
            );
            assert_eq!(
                got.participants.len(),
                legacy.participants.len() + 1,
                "one root presence plus the legacy ones"
            );
            assert_eq!(got.participants[0].id.role, SceneRole::Root);
            assert_eq!(&got.participants[1..], &legacy.participants[..]);
            assert_eq!(got.sampled.len(), legacy.sampled.len() + 1);
            assert_eq!(&got.sampled[1..], &legacy.sampled[..]);
            assert_eq!(got.stats_free_snapshots(), legacy.snapshots.len() + 1);
        }
    }
}

/// The pixel oracle over the whole differential fixture, with a root, on
/// several layouts, with and without the COW subtree.
#[test]
fn visibility_shows_the_same_pixels_as_the_unclipped_scene() {
    for layout in [
        (0, 0, 800u32, 600u32),
        (100, 50, 700, 500),
        (-300, -200, 800, 600),
        (2560, 0, 640, 480),
    ] {
        for cow in [None, Some(0x800u32)] {
            let (core, mut store, windows) = differential_fixture();
            alloc_root(&core, &mut store, 5120, 1440);
            let on = assert_oracle(&core, &mut store, &windows, layout, cow, "fixture");
            assert!(on.stats.nodes_visited > 0);
            assert!(
                on.stats.draws_emitted == u64::try_from(on.scene.draws.len()).unwrap(),
                "stats count what was emitted"
            );
        }
    }
}

/// An opaque top-level fully covering a lower one: the lower emits nothing
/// but is still a participant; the root emits the output minus the cover.
#[test]
fn a_fully_covered_window_emits_nothing_and_stays_a_participant() {
    let (core, mut store, windows) = two_windows((100, 100, 50, 50), (80, 80, 100, 100));
    let on = assert_oracle(
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
        "full cover",
    );
    assert!(
        draws_of(&on, win_view(0x100)).is_empty(),
        "covered window emits nothing"
    );
    assert_eq!(draws_of(&on, win_view(0x200)).len(), 1);
    let root: u64 = draws_of(&on, 0x00A0_7000).iter().map(|r| area_of(*r)).sum();
    assert_eq!(
        root,
        800 * 600 - 100 * 100,
        "root = output − the opaque cover"
    );
    assert_eq!(on.stats.hidden_participants, 1);
    let hidden = on
        .participants
        .iter()
        .find(|p| p.id.xid == 0x100)
        .expect("hidden window is still a participant");
    assert!(hidden.visible.is_empty());
    assert_eq!(
        hidden.region.bounding_rect(),
        Some(audit_rect(100, 100, 50, 50))
    );
    // Placement, not visibility: the presence region is the full window.
    assert_eq!(hidden.region.area(), 50 * 50);
}

/// Partial cover: the lower window emits only its visible pieces.
#[test]
fn a_partly_covered_window_emits_only_its_visible_pieces() {
    let (core, mut store, windows) = two_windows((100, 100, 200, 200), (250, 150, 200, 200));
    let on = assert_oracle(
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
        "partial",
    );
    let lower: u64 = draws_of(&on, win_view(0x100))
        .iter()
        .map(|r| area_of(*r))
        .sum();
    // 200×200 minus the 50×150 overlap.
    assert_eq!(lower, 200 * 200 - 50 * 150);
    assert!(
        draws_of(&on, win_view(0x100)).len() > 1,
        "emitted as pieces"
    );
    assert_eq!(on.stats.hidden_participants, 0);
}

/// The parent-bounding-shape fix, written before the walk: an EMPTY parent
/// shape suppresses its children; a partial shape clips them. Under the
/// pre-step-1 emitter the child clip came from the parent's rect alone.
#[test]
fn an_empty_parent_shape_suppresses_its_children() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let mut windows = crate::kms::render::backend::WindowsMap::new();
    alloc_root(&core, &mut store, 800, 600);
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x100,
        100,
        100,
        200,
        200,
        None,
        true,
    );
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x101,
        10,
        10,
        50,
        50,
        Some(0x100),
        true,
    );
    core.top_level_order = vec![0x100];
    core.shape_bounding.insert(0x100, Vec::new());
    let on = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    assert!(
        draws_of(&on, win_view(0x100)).is_empty(),
        "empty shape: parent draws nothing"
    );
    assert!(
        draws_of(&on, win_view(0x101)).is_empty(),
        "…and neither do its children"
    );
    let root: u64 = draws_of(&on, 0x00A0_7000).iter().map(|r| area_of(*r)).sum();
    assert_eq!(root, 800 * 600, "the root shows through the whole hole");
}

#[test]
fn a_partial_parent_shape_clips_its_children() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let mut windows = crate::kms::render::backend::WindowsMap::new();
    alloc_root(&core, &mut store, 800, 600);
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x100,
        100,
        100,
        200,
        200,
        None,
        true,
    );
    // Child spans the whole parent; the parent's shape is its left half.
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x101,
        0,
        0,
        200,
        200,
        Some(0x100),
        true,
    );
    core.top_level_order = vec![0x100];
    core.shape_bounding.insert(
        0x100,
        vec![xfixes::RegionRect {
            x: 0,
            y: 0,
            width: 100,
            height: 200,
        }],
    );
    let on = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    let child = draws_of(&on, win_view(0x101));
    let child_area: u64 = child.iter().map(|r| area_of(*r)).sum();
    assert_eq!(child_area, 100 * 200, "child clipped to the parent's shape");
    assert!(
        child
            .iter()
            .all(|r| r.offset.x + i32::try_from(r.extent.width).unwrap() <= 200),
        "no child pixel outside the shaped half: {child:?}"
    );
    // The parent is entirely under its child within the shape: nothing left.
    assert!(draws_of(&on, win_view(0x100)).is_empty());
    let root: u64 = draws_of(&on, 0x00A0_7000).iter().map(|r| area_of(*r)).sum();
    assert_eq!(root, 800 * 600 - 100 * 200);
}

/// COW subtree above opaque windows: the COW claims nothing, so the windows
/// below still emit in full, and the COW blends over them.
#[test]
fn a_cow_subtree_claims_nothing() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let mut windows = crate::kms::render::backend::WindowsMap::new();
    alloc_root(&core, &mut store, 800, 600);
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x100,
        100,
        100,
        200,
        200,
        None,
        true,
    );
    alloc_stub_window(&mut store, &mut windows, 0x800, 0, 0, 800, 600, None, true);
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x801,
        0,
        0,
        800,
        600,
        Some(0x800),
        true,
    );
    set_rank(&mut windows, 0x100, 1);
    set_rank(&mut windows, 0x800, 2);
    core.top_level_order = vec![0x100, 0x800];
    let on = assert_oracle(
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        Some(0x800),
        "cow",
    );
    assert_eq!(
        draws_of(&on, win_view(0x100)).len(),
        1,
        "window under the COW emits whole"
    );
    let root: u64 = draws_of(&on, 0x00A0_7000).iter().map(|r| area_of(*r)).sum();
    assert_eq!(
        root,
        800 * 600 - 200 * 200,
        "root loses only the opaque window"
    );
    assert_eq!(on.stats.hidden_participants, 0);
}

/// A manual-redirected top-level emits nothing itself but its opaque
/// automatic child claims through it: the root loses the child's area.
#[test]
fn an_opaque_automatic_child_claims_through_a_manual_parent() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let mut windows = crate::kms::render::backend::WindowsMap::new();
    alloc_root(&core, &mut store, 800, 600);
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x600,
        50,
        400,
        200,
        100,
        None,
        true,
    );
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x601,
        10,
        10,
        60,
        40,
        Some(0x600),
        true,
    );
    let m_id = store.lookup(0x600).unwrap();
    let m_backing = alloc_backing(&mut store, 0xB600, 200, 100);
    store.set_redirected_target(m_id, Some(m_backing));
    store.set_scene_participating(m_id, false);
    let a_id = store.lookup(0x601).unwrap();
    let a_backing = alloc_backing(&mut store, 0xB601, 60, 40);
    store.set_redirected_target(a_id, Some(a_backing));
    core.top_level_order = vec![0x600];
    let on = assert_oracle(
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
        "manual",
    );
    let backing_view = u64::from(0xB601u32) | 0xB000_0000;
    assert_eq!(draws_of(&on, backing_view).len(), 1);
    let root = draws_of(&on, 0x00A0_7000);
    let root_area: u64 = root.iter().map(|r| area_of(*r)).sum();
    assert_eq!(
        root_area,
        800 * 600 - 60 * 40,
        "root loses exactly the child's area"
    );
    assert!(
        root.iter()
            .all(|r| !rects_intersect(*r, audit_rect(60, 410, 60, 40))),
        "no root piece under the automatic child"
    );
}

/// Straddling window and non-zero layout origin: pieces sample the same
/// texels as the unclipped draw (checked by the oracle) and lie on the
/// output.
#[test]
fn straddling_windows_and_layout_origins_sample_the_right_texels() {
    let (core, mut store, windows) = two_windows((-50, -50, 100, 100), (700, 500, 200, 200));
    let on = assert_oracle(
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
        "straddle",
    );
    for d in &on.scene.draws {
        let r = draw_dst_rect_inward(d).unwrap();
        assert!(r.offset.x >= 0 && r.offset.y >= 0, "output-clipped: {r:?}");
        assert!(
            r.offset.x + i32::try_from(r.extent.width).unwrap() <= 800
                && r.offset.y + i32::try_from(r.extent.height).unwrap() <= 600,
            "output-clipped: {r:?}"
        );
    }
    // The top-left straddler shows its bottom-right quarter: src starts at 0.5.
    let piece = on
        .scene
        .draws
        .iter()
        .find(|d| ash::vk::Handle::as_raw(d.image_view) == win_view(0x100))
        .expect("straddler emits");
    assert_eq!(piece.dst_origin, [0.0, 0.0]);
    assert_eq!(piece.src_origin, [0.5, 0.5]);
    assert_eq!(piece.src_size, [0.5, 0.5]);
    // Same tree on the second output of a side-by-side layout.
    let (core, mut store, windows) = two_windows((2500, 100, 120, 100), (2700, 300, 50, 50));
    let on = assert_oracle(
        &core,
        &mut store,
        &windows,
        (2560, 0, 640, 480),
        None,
        "x0=2560",
    );
    let piece = on
        .scene
        .draws
        .iter()
        .find(|d| ash::vk::Handle::as_raw(d.image_view) == win_view(0x100))
        .expect("emits");
    // Window at logical x=2500 is 60px off the left edge of this output.
    assert_eq!(piece.dst_origin, [0.0, 100.0]);
    assert_eq!(piece.src_origin, [0.5, 0.0]);
    assert_eq!(piece.src_size, [0.5, 1.0]);
}

/// A redirected window whose backing outgrew it (a shrink keeps the old
/// backing): `src` divides by the SAMPLED source's extent, so the piece
/// samples the window's texels at the backing's origin rather than
/// stretching the whole backing. The pre-step-1 emitter divided by the
/// host size — kept under `Off` only — so this case has no oracle and is
/// asserted directly.
#[test]
fn a_redirected_backing_larger_than_its_host_is_sampled_unstretched() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let mut windows = crate::kms::render::backend::WindowsMap::new();
    alloc_root(&core, &mut store, 800, 600);
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x700,
        100,
        100,
        80,
        60,
        None,
        true,
    );
    let r_id = store.lookup(0x700).unwrap();
    let backing = alloc_backing(&mut store, 0xB700, 160, 120); // 2× the host
    store.set_redirected_target(r_id, Some(backing));
    core.top_level_order = vec![0x700];
    let on = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    let d = on
        .scene
        .draws
        .iter()
        .find(|d| ash::vk::Handle::as_raw(d.image_view) == (u64::from(0xB700u32) | 0xB000_0000))
        .expect("redirected window emits its backing");
    assert_eq!(d.dst_origin, [100.0, 100.0]);
    assert_eq!(d.dst_size, [80.0, 60.0]);
    assert_eq!(d.src_origin, [0.0, 0.0]);
    assert_eq!(
        d.src_size,
        [0.5, 0.5],
        "80/160 × 60/120: the host's texels only"
    );
    // And the presence signature agrees with the unclipped draw.
    let p = on.participants.iter().find(|p| p.id.xid == 0x700).unwrap();
    assert_eq!(
        p.signature,
        PresenceSignature::new(d.image_view, d.src_origin, d.src_size, false)
    );
}

/// More than 32 opaque fragments over the root: the root's universe
/// collapses to its bounding box (a superset), so the root over-emits —
/// and the oracle still holds, because painter's order repaints the extra.
#[test]
fn a_collapsed_universe_over_emits_but_shows_the_same_pixels() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let mut windows = crate::kms::render::backend::WindowsMap::new();
    alloc_root(&core, &mut store, 800, 600);
    let mut rank = 1;
    for i in 0..7i16 {
        for j in 0..7i16 {
            let xid = 0x1000 + u32::try_from(i * 7 + j).unwrap();
            alloc_stub_window(
                &mut store,
                &mut windows,
                xid,
                20 + i * 100,
                20 + j * 70,
                40,
                30,
                None,
                true,
            );
            set_rank(&mut windows, xid, rank);
            rank += 1;
            core.top_level_order.push(xid);
        }
    }
    let on = assert_oracle(
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
        "collapse",
    );
    assert!(
        on.stats.collapses() > 0,
        "49 disjoint claims must overflow the 32-box cap"
    );
    let root: u64 = draws_of(&on, 0x00A0_7000).iter().map(|r| area_of(*r)).sum();
    assert!(
        root > 800 * 600 - 49 * 40 * 30,
        "root over-emits after the collapse (superset), never under"
    );
    assert!(root <= 800 * 600);
}

/// Step 2 under step 1: a window hidden by an unrelated move above it
/// contributes no structural damage of its own — the mover's old ∪ new
/// covers it — and its rank is unchanged, so nothing reads as restacked.
#[test]
fn hiding_a_window_by_moving_another_over_it_damages_only_the_mover() {
    // Frame 1: B beside A. Frame 2: B moved onto A, covering it entirely.
    let (core, mut store, windows) = two_windows((100, 100, 50, 50), (400, 400, 100, 100));
    let before = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    let (core2, mut store2, windows2) = two_windows((100, 100, 50, 50), (80, 80, 100, 100));
    let after = build_with(
        Visibility::On,
        &core2,
        &mut store2,
        &windows2,
        (0, 0, 800, 600),
        None,
    );
    // Participant identity uses the store generation; both fixtures allocate
    // in the same order so the ids line up.
    assert_eq!(
        before.participants.iter().map(|p| p.id).collect::<Vec<_>>(),
        after.participants.iter().map(|p| p.id).collect::<Vec<_>>(),
        "same participants in the same (painter's) order — nothing restacked, \
             and the hidden window is still listed"
    );
    let damage = structural_damage(&before.participants, &after.participants);
    let mut expect = Region::from_rect(audit_rect(400, 400, 100, 100));
    expect.union_with(&Region::from_rect(audit_rect(80, 80, 100, 100)));
    assert_eq!(damage, expect, "exactly the mover's old ∪ new");
    assert!(
        after
            .participants
            .iter()
            .any(|p| p.id.xid == 0x100 && p.visible.is_empty()),
        "the covered window is present with an empty visible region"
    );
}

/// A pure restack of two overlapping top-levels owes only their overlap: the
/// step-2 rule damages pairwise intersections of participants whose relative
/// order flipped, not the whole region of everything whose rank moved.
#[test]
fn swapping_two_overlapping_top_levels_damages_only_their_overlap() {
    let lower = (100i16, 100i16, 300u16, 300u16);
    let upper = (250i16, 250i16, 300u16, 300u16);
    let (core, mut store, windows) = two_windows(lower, upper);
    let before = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    let (mut core2, mut store2, mut windows2) = two_windows(lower, upper);
    // Swap the stacking: 0x200 now below 0x100.
    set_rank(&mut windows2, 0x100, 2);
    set_rank(&mut windows2, 0x200, 1);
    core2.top_level_order = vec![0x200, 0x100];
    let after = build_with(
        Visibility::On,
        &core2,
        &mut store2,
        &windows2,
        (0, 0, 800, 600),
        None,
    );
    let damage = structural_damage(&before.participants, &after.participants);
    let overlap = Region::from_rect(audit_rect(250, 250, 150, 150));
    assert_eq!(damage, overlap, "exactly lower ∩ upper");
}

/// The `Off` scene keeps `visible == region` for every participant, so the
/// audit's reference carries the same presences as before step 1.
#[test]
fn off_mode_presences_are_fully_visible() {
    let (core, mut store, windows) = differential_fixture();
    let off = build_with(
        Visibility::Off,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    for p in &off.participants {
        assert_eq!(p.visible, p.region, "{:?}", p.id);
    }
    assert_eq!(off.stats.hidden_participants, 0);
    assert_eq!(off.stats.collapses(), 0);
}
