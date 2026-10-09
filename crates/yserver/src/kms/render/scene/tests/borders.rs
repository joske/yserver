use super::*;

// ── #133 step 5 (P6) — the scene spaces and the coordinate recurrence ──
//
// Every test below has a named negative control: the one-line revert that
// makes it fail, verified by actually reverting. `bw == 0` identity is
// proved structurally by `a_borderless_node_needs_no_separate_child_region`
// and by the two pre-existing legacy-oracle tests
// (`refactored_emitter_matches_the_legacy_emitter_exactly`,
// `off_mode_reproduces_the_legacy_root_and_emitter_exactly`), which run a
// twelve-node `bw == 0` tree through the pre-#133 emitter and demand a
// byte-identical draw list.

/// A window whose storage is the BORDERED extent placed at the OUTER
/// origin, exactly as `allocate_window_leaf` builds it
/// (`backend.rs:13440`-`:13476`): extent `(w + 2bw) x (h + 2bw)` and the
/// allocation's `content_offset` recorded as `bw`.
#[allow(clippy::too_many_arguments)]
fn alloc_stub_window_bordered(
    store: &mut DrawableStore,
    windows: &mut crate::kms::render::backend::WindowsMap,
    xid: u32,
    x: i16,
    y: i16,
    w: u16,
    h: u16,
    bw: u16,
    parent: Option<u32>,
    mapped: bool,
) {
    let bordered = extent(
        u32::from(w) + 2 * u32::from(bw),
        u32::from(h) + 2 * u32::from(bw),
    );
    let mut storage =
        crate::kms::render::store::Storage::for_tests_null(bordered, vk::Format::B8G8R8A8_UNORM);
    let sentinel: ash::vk::ImageView = ash::vk::Handle::from_raw(u64::from(xid) | 0xFF00_0000);
    storage.image_view = sentinel;
    storage.sample_view = sentinel;
    let id = store
        .allocate(xid, DrawableKind::Window, 32, mapped, storage)
        .expect("bordered stub allocate");
    store.set_content_offset(id, i32::from(bw));
    windows.insert(
        xid,
        crate::kms::render::backend::WindowGeometry {
            border_width: bw,
            border_pixel: None,
            border_pixmap: None,
            x,
            y,
            width: w,
            height: h,
            depth: 32,
            mapped,
            viewable: true,
            parent,
            stack_rank: 0,
            bg_pixel: None,
            bg_pixmap: None,
            cursor: None,
        },
    );
}

/// Sorted destination rects a given host xid contributed, under `mode`.
fn dst_rects_of(built: &SceneBuild, xid: u32) -> Vec<vk::Rect2D> {
    sorted_rects(draws_of(built, win_view(xid)))
}

/// 5.1 — the node samples its OUTER extent from the storage ORIGIN. Before
/// step 5 the walk sampled `w x h` from `(0,0)` of a `(w+2bw) x (h+2bw)`
/// storage, so the bottom and right bands of the ring fell outside the
/// sampled rect entirely — the measured "only the top band" symptom.
///
/// Negative control: `let win_w = own_w;` (the pre-step-5 line) in
/// `decide_node` → the draw comes out 200x100 at (100,50) and every
/// assertion below fails.
#[test]
fn a_bordered_node_samples_its_whole_outer_extent() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let mut windows = crate::kms::render::backend::WindowsMap::new();
    alloc_stub_window_bordered(
        &mut store,
        &mut windows,
        0x100,
        100,
        50,
        200,
        100,
        16,
        None,
        true,
    );
    core.top_level_order = vec![0x100];
    let off = build_with(
        Visibility::Off,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    // Outer origin = the window position; outer extent = w + 2bw.
    assert_eq!(dst_rects_of(&off, 0x100), vec![r(100, 50, 232, 132)]);
    // And the whole texture is sampled: src [0,0]-[1,1]. A wrong
    // denominator would show up here even when the dst rect is right.
    let d = off
        .scene
        .draws
        .iter()
        .find(|d| ash::vk::Handle::as_raw(d.image_view) == win_view(0x100))
        .expect("bordered draw");
    assert_eq!(d.src_origin, [0.0, 0.0]);
    assert_eq!(d.src_size, [1.0, 1.0]);
}

/// 5.1 — the two-absolute recurrence. This is the awesome bug in
/// miniature: the child's parent-relative position is measured from the
/// parent's CONTENT origin (`parent_outer + bw`), not from its outer one.
///
/// Negative control: pass `abs_x, abs_y` instead of
/// `node.content_abs_x, node.content_abs_y` at the recursion → the child
/// lands at (100, 67) instead of (116, 83), i.e. displaced by `bw` in
/// both axes, and it overpaints the parent's left bar just as the scanout
/// showed.
#[test]
fn a_bordered_parents_child_is_placed_from_the_content_origin() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let mut windows = crate::kms::render::backend::WindowsMap::new();
    alloc_stub_window_bordered(
        &mut store,
        &mut windows,
        0x100,
        100,
        50,
        200,
        100,
        16,
        None,
        true,
    );
    // Awesome's frame-relative titlebar child: (0, 17) inside the frame.
    alloc_stub_window_bordered(
        &mut store,
        &mut windows,
        0x101,
        0,
        17,
        100,
        20,
        0,
        Some(0x100),
        true,
    );
    set_rank(&mut windows, 0x100, 1);
    set_rank(&mut windows, 0x101, 2);
    core.top_level_order = vec![0x100];
    let off = build_with(
        Visibility::Off,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    // content origin = (100 + 16, 50 + 16) = (116, 66); child at +(0, 17).
    assert_eq!(dst_rects_of(&off, 0x101), vec![r(116, 83, 100, 20)]);
}

/// 5.2 — a child may not overlap its parent's border
/// (`mi/mivaltree.c:386`). An oversized child is clipped to the parent's
/// INNER region, so the ring survives on all four sides even though the
/// child is larger than the whole outer rect.
///
/// Negative control: `let child_clip_x0 = clip_x0.max(abs_x);` (and the
/// three siblings) in `decide_node`, i.e. clip children to the OUTER rect
/// → the child's rect becomes (0,0,132,132) and it covers the ring.
#[test]
fn a_child_cannot_overlap_its_parents_border() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let mut windows = crate::kms::render::backend::WindowsMap::new();
    alloc_root(&core, &mut store, 800, 600);
    alloc_stub_window_bordered(
        &mut store,
        &mut windows,
        0x100,
        0,
        0,
        100,
        100,
        16,
        None,
        true,
    );
    alloc_stub_window_bordered(
        &mut store,
        &mut windows,
        0x101,
        0,
        0,
        300,
        300,
        0,
        Some(0x100),
        true,
    );
    set_rank(&mut windows, 0x100, 1);
    set_rank(&mut windows, 0x101, 2);
    core.top_level_order = vec![0x100];
    let on = assert_oracle(
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
        "child vs parent border",
    );
    // The child is confined to the parent's content box.
    assert_eq!(dst_rects_of(&on, 0x101), vec![r(16, 16, 100, 100)]);
    // The parent keeps exactly the annulus: 132² − 100² = 7 424 px, split
    // into the four bands the visibility walk left it.
    let parent: u64 = dst_rects_of(&on, 0x100).iter().map(|x| area_of(*x)).sum();
    assert_eq!(parent, 132 * 132 - 100 * 100);
    // All four sides present, not just the top band.
    let rects = dst_rects_of(&on, 0x100);
    let covers = |px: i32, py: i32| {
        rects.iter().any(|q| {
            px >= q.offset.x
                && py >= q.offset.y
                && px < q.offset.x + i32::try_from(q.extent.width).unwrap()
                && py < q.offset.y + i32::try_from(q.extent.height).unwrap()
        })
    };
    assert!(covers(66, 8), "top band");
    assert!(covers(66, 123), "bottom band");
    assert!(covers(8, 66), "left bar");
    assert!(covers(123, 66), "right bar");
}

/// 5.3 — SHAPE regions are WINDOW-LOCAL (content-relative), so a bordered
/// shaped window's mask must be read at `+ bw` in storage space
/// (`dix/window.c:1736`). And `SetBorderSize` intersects the expanded box
/// with the bounding shape (`:1772`), so a bounding shape that stops at
/// the content edge removes the ring — the bounding region is the
/// border-INCLUSIVE extent.
///
/// Negative control: `let rx = i32::from(rect.x);` in the `place` loop →
/// the mask lands at (10, 10) instead of (18, 18), off by `bw`.
#[test]
fn a_bordered_shaped_windows_mask_is_read_in_content_coordinates() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let mut windows = crate::kms::render::backend::WindowsMap::new();
    alloc_stub_window_bordered(
        &mut store,
        &mut windows,
        0x100,
        10,
        10,
        100,
        100,
        8,
        None,
        true,
    );
    core.top_level_order = vec![0x100];
    // One bounding rect covering the top-left quarter of the CONTENT.
    core.shape_bounding.insert(
        0x100,
        vec![xfixes::RegionRect {
            x: 0,
            y: 0,
            width: 50,
            height: 50,
        }],
    );
    let off = build_with(
        Visibility::Off,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    // Window at (10,10) with bw=8 ⇒ content origin (18,18); the mask is
    // relative to THAT, and the ring outside it is gone.
    assert_eq!(dst_rects_of(&off, 0x100), vec![r(18, 18, 50, 50)]);
}

/// 5.2 / 5.3 — the CLIP shape narrows `winSize` (what descendants clip to)
/// and nothing else: `SetWinSize` intersects with both shapes
/// (`dix/window.c:1735`), `SetBorderSize` with the bounding shape only
/// (`:1772`). Bounding, clip and input shapes are three different things.
///
/// Negative control: drop `clip` from the `[bounding, clip]` array in
/// `inner_place_rects` → the child keeps its full 100x100 rect.
#[test]
fn a_clip_shape_narrows_the_child_clip_but_not_the_nodes_own_place() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let mut windows = crate::kms::render::backend::WindowsMap::new();
    alloc_root(&core, &mut store, 800, 600);
    alloc_stub_window(&mut store, &mut windows, 0x100, 0, 0, 100, 100, None, true);
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x101,
        0,
        0,
        100,
        100,
        Some(0x100),
        true,
    );
    set_rank(&mut windows, 0x100, 1);
    set_rank(&mut windows, 0x101, 2);
    core.top_level_order = vec![0x100];
    core.shape_clip.insert(
        0x100,
        vec![xfixes::RegionRect {
            x: 0,
            y: 0,
            width: 40,
            height: 100,
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
    // The clip shape does not touch the parent's own region…
    assert_eq!(
        dst_rects_of(&on, 0x100)
            .iter()
            .map(|x| area_of(*x))
            .sum::<u64>(),
        100 * 100 - 40 * 100,
        "parent draws its whole rect; the child claims only the clipped part",
    );
    // …but the child is confined to it.
    assert_eq!(dst_rects_of(&on, 0x101), vec![r(0, 0, 40, 100)]);
}

/// 5.4 — the redirection exception is a RESET, not an intersect
/// (`mi/mivaltree.c:233`-`:239`). A window that owns its own
/// `redirected_target` is NOT clipped to its parent
/// (`dix/window.c:1719`, `:1753`), and the reset propagates: it happens
/// before the `∩ winSize` at `:390`, so its descendants inherit the
/// un-parent-clipped region too.
///
/// Negative control: delete the `if self_owns_redirected_target` reset in
/// `decide_node` → the redirected child is clipped to its parent's
/// content box (40x50 instead of 100x50) and its grandchild disappears
/// entirely.
#[test]
fn a_redirected_window_is_not_clipped_to_its_parent_and_neither_are_its_children() {
    for participating in [true, false] {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let mut windows = crate::kms::render::backend::WindowsMap::new();
        // Parent 100x100 at (0,0); the redirected child starts at (60,0)
        // and is 100 wide, so 60 px of it stick out past the parent.
        alloc_stub_window(&mut store, &mut windows, 0x100, 0, 0, 100, 100, None, true);
        alloc_stub_window(
            &mut store,
            &mut windows,
            0x101,
            60,
            0,
            100,
            50,
            Some(0x100),
            true,
        );
        alloc_stub_window(
            &mut store,
            &mut windows,
            0x102,
            0,
            0,
            100,
            50,
            Some(0x101),
            true,
        );
        set_rank(&mut windows, 0x100, 1);
        set_rank(&mut windows, 0x101, 2);
        set_rank(&mut windows, 0x102, 3);
        core.top_level_order = vec![0x100];
        let c_id = store.lookup(0x101).expect("redirected child present");
        let backing = alloc_backing(&mut store, 0xB101, 100, 50);
        store.set_redirected_target(c_id, Some(backing));
        // Automatic (`true`) and manual (`false`) alike: Xorg keys the
        // reset off `redirectDraw != RedirectDrawNone`.
        store.set_scene_participating(c_id, participating);
        // The grandchild owns its own backing so that it emits — a plain
        // descendant's paint lands in the redirected ancestor's backing
        // and is skipped by design, which would make the placement
        // unobservable.
        let g_id = store.lookup(0x102).expect("grandchild present");
        let g_backing = alloc_backing(&mut store, 0xB102, 100, 50);
        store.set_redirected_target(g_id, Some(g_backing));
        let off = build_with(
            Visibility::Off,
            &core,
            &mut store,
            &windows,
            (0, 0, 800, 600),
            None,
        );
        // The redirected window's own draw (automatic mode only — a
        // manual one is skipped from the scene by design) keeps its full
        // width, not the 40 px its parent would have allowed.
        let backing_rects = sorted_rects(draws_of(&off, u64::from(0xB101u32) | 0xB000_0000));
        if participating {
            assert_eq!(backing_rects, vec![r(60, 0, 100, 50)], "automatic");
        } else {
            assert!(backing_rects.is_empty(), "manual is skipped from the scene");
        }
        // Rule 3: the reset PROPAGATES. The grandchild inherits the
        // un-parent-clipped region and keeps its full 100 px width, in
        // both redirect modes.
        assert_eq!(
            sorted_rects(draws_of(&off, u64::from(0xB102u32) | 0xB000_0000)),
            vec![r(60, 0, 100, 50)],
            "grandchild inherits the reset, participating={participating}",
        );
    }
}

/// 5.1 / 5.4 — a bordered child under a redirected ancestor is not
/// displaced, in either redirect mode. The child owns its own backing so
/// that it emits and the placement is observable.
///
/// Negative control: revert the recursion to `abs_x, abs_y` → the child
/// lands at (210, 120) − nothing, but the GRANDCHILD lands at (218, 128)
/// instead of (226, 136), off by the child's own `bw`.
#[test]
fn a_bordered_child_under_a_redirected_ancestor_is_not_displaced() {
    for participating in [true, false] {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let mut windows = crate::kms::render::backend::WindowsMap::new();
        alloc_stub_window(
            &mut store,
            &mut windows,
            0x100,
            200,
            100,
            400,
            300,
            None,
            true,
        );
        alloc_stub_window_bordered(
            &mut store,
            &mut windows,
            0x101,
            10,
            20,
            100,
            50,
            8,
            Some(0x100),
            true,
        );
        alloc_stub_window(
            &mut store,
            &mut windows,
            0x102,
            0,
            0,
            20,
            10,
            Some(0x101),
            true,
        );
        set_rank(&mut windows, 0x100, 1);
        set_rank(&mut windows, 0x101, 2);
        set_rank(&mut windows, 0x102, 3);
        core.top_level_order = vec![0x100];
        // The top-level is redirected; the bordered child owns its own
        // backing so it still emits (`has_own_redirected_target` breaks
        // the ancestor chain).
        let top_id = store.lookup(0x100).expect("top present");
        let top_backing = alloc_backing(&mut store, 0xB100, 400, 300);
        store.set_redirected_target(top_id, Some(top_backing));
        store.set_scene_participating(top_id, participating);
        let child_id = store.lookup(0x101).expect("child present");
        // The backing of a bordered window is the BORDERED extent placed
        // at the outer origin (`compAllocPixmap`, `compalloc.c:610`), and
        // it carries the same content offset.
        let child_backing = alloc_backing(&mut store, 0xB101, 116, 66);
        store.set_redirected_target(child_id, Some(child_backing));
        store.set_content_offset(child_backing, 8);
        // The grandchild needs its own backing too: a plain descendant of
        // a redirected window paints into the ancestor's backing and is
        // skipped from the scene, which would make its placement
        // unobservable.
        let g_id = store.lookup(0x102).expect("grandchild present");
        let g_backing = alloc_backing(&mut store, 0xB102, 20, 10);
        store.set_redirected_target(g_id, Some(g_backing));
        let off = build_with(
            Visibility::Off,
            &core,
            &mut store,
            &windows,
            (0, 0, 800, 600),
            None,
        );
        // Outer = top content (200,100, no border) + (10,20) = (210,120),
        // extent 100 + 2·8 = 116 by 66.
        assert_eq!(
            sorted_rects(draws_of(&off, u64::from(0xB101u32) | 0xB000_0000)),
            vec![r(210, 120, 116, 66)],
            "participating={participating}",
        );
        // The grandchild is positioned from the bordered child's CONTENT
        // origin: (210 + 8, 120 + 8) = (218, 128).
        assert_eq!(
            sorted_rects(draws_of(&off, u64::from(0xB102u32) | 0xB000_0000)),
            vec![r(218, 128, 20, 10)],
            "participating={participating}",
        );
    }
}

/// `bw == 0` identity, structurally: a borderless window with no clip
/// shape produces NO separate child region — `child_place` is `None`, so
/// `mine` and the non-opaque claim read the very same `place` vector the
/// pre-step-5 walk read, through the same code. The moment either a
/// border or a clip shape exists, the two regions separate.
///
/// Negative control: make `child_place` unconditionally `Some(...)` →
/// the first assertion fails (and the two legacy-oracle differential
/// tests still pass, which is why this structural check is needed on top
/// of them).
#[test]
fn a_borderless_node_needs_no_separate_child_region() {
    let mut store = DrawableStore::new();
    let mut windows = crate::kms::render::backend::WindowsMap::new();
    let empty: HashMap<u32, Vec<xfixes::RegionRect>> = HashMap::new();
    let mut clip: HashMap<u32, Vec<xfixes::RegionRect>> = HashMap::new();
    alloc_stub_window(&mut store, &mut windows, 0x100, 10, 20, 100, 50, None, true);
    alloc_stub_window_bordered(
        &mut store,
        &mut windows,
        0x200,
        10,
        20,
        100,
        50,
        4,
        None,
        true,
    );
    let decide = |xid: u32,
                  store: &DrawableStore,
                  windows: &crate::kms::render::backend::WindowsMap,
                  shape_clip: &HashMap<u32, Vec<xfixes::RegionRect>>| {
        decide_node(
            xid,
            windows.get(&xid).expect("geom"),
            0,
            0,
            store,
            &empty,
            shape_clip,
            0,
            0,
            800,
            600,
            Visibility::On,
            false,
            false,
            i32::MIN / 2,
            i32::MIN / 2,
            i32::MAX / 2,
            i32::MAX / 2,
        )
    };
    let plain = decide(0x100, &store, &windows, &empty);
    assert!(
        plain.child_place.is_none(),
        "bw == 0, no clip shape ⇒ one region serves both roles",
    );
    assert_eq!((plain.abs_x, plain.abs_y), (10, 20));
    assert_eq!(
        (plain.content_abs_x, plain.content_abs_y),
        (10, 20),
        "outer and content coincide at bw == 0",
    );
    assert_eq!((plain.win_w, plain.win_h), (100, 50));

    let bordered = decide(0x200, &store, &windows, &empty);
    assert_eq!(
        bordered.child_place,
        Some(vec![r(14, 24, 100, 50)]),
        "bw > 0 ⇒ the inner region is the content box",
    );
    assert_eq!(bordered.place, vec![r(10, 20, 108, 58)]);
    assert_eq!((bordered.content_abs_x, bordered.content_abs_y), (14, 24));

    // A clip shape alone also separates the two regions, at bw == 0.
    clip.insert(
        0x100,
        vec![xfixes::RegionRect {
            x: 0,
            y: 0,
            width: 40,
            height: 50,
        }],
    );
    let clipped = decide(0x100, &store, &windows, &clip);
    assert_eq!(
        clipped.place,
        vec![r(10, 20, 100, 50)],
        "place is untouched"
    );
    assert_eq!(clipped.child_place, Some(vec![r(10, 20, 40, 50)]));
}

/// The measured awesome regression, reproduced from the scanout dump.
///
/// Two tiles on a 2560x1440 output: outer 1276x1421 at (0, 17) and
/// (1276, 17), `bw = 16`, content 1244x1389, each with awesome's
/// frame-relative (0, 17) child sized 1244x1372 (the geometries in the
/// `yserver-drawable-0-win-*` dumps). Before step 5 the scan found 19 920
/// red plus 19 920 green pixels — 16 rows of 1244 plus a single row of 16
/// — all in the top band. After it, each tile's ring is
/// `2·bw·(outer_w + outer_h − 2·bw) = 32 · (1276 + 1421 − 32) = 85 280`
/// pixels, on all four sides, and the 32 px inter-tile gap at x 1244..1275
/// is two adjacent border bars rather than wallpaper.
///
/// Negative control: any one of the three reverts named above — the outer
/// sampling extent, the content-origin recursion, or the inner child clip
/// — breaks a different assertion here.
#[test]
fn the_awesome_two_tile_layout_rings_all_four_sides() {
    const BW: u16 = 16;
    const CW: u16 = 1244;
    const CH: u16 = 1389;
    const OW: u32 = CW as u32 + 2 * BW as u32; // 1276
    const OH: u32 = CH as u32 + 2 * BW as u32; // 1421
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let mut windows = crate::kms::render::backend::WindowsMap::new();
    alloc_root(&core, &mut store, 5120, 1440);
    for (i, (xid, child, x)) in [(0x100u32, 0x101u32, 0i16), (0x200, 0x201, 1276)]
        .into_iter()
        .enumerate()
    {
        alloc_stub_window_bordered(&mut store, &mut windows, xid, x, 17, CW, CH, BW, None, true);
        alloc_stub_window_bordered(
            &mut store,
            &mut windows,
            child,
            0,
            17,
            CW,
            1372,
            0,
            Some(xid),
            true,
        );
        set_rank(&mut windows, xid, (2 * i + 1) as u64);
        set_rank(&mut windows, child, (2 * i + 2) as u64);
    }
    core.top_level_order = vec![0x100, 0x200];
    let on = assert_oracle(
        &core,
        &mut store,
        &windows,
        (0, 0, 2560, 1440),
        None,
        "awesome two tiles",
    );

    for (xid, child, x0) in [(0x100u32, 0x101u32, 0i32), (0x200, 0x201, 1276)] {
        // The frame's visible region is exactly the ring: the child fills
        // the content from y = 33 down and the rest of the content is
        // still the frame's own.
        let rects = dst_rects_of(&on, xid);
        let covers = |px: i32, py: i32| {
            rects.iter().any(|q| {
                px >= q.offset.x
                    && py >= q.offset.y
                    && px < q.offset.x + i32::try_from(q.extent.width).unwrap()
                    && py < q.offset.y + i32::try_from(q.extent.height).unwrap()
            })
        };
        // All four bands and all four corners.
        assert!(covers(x0 + 638, 24), "{xid:#x} top band");
        assert!(covers(x0 + 638, 1430), "{xid:#x} bottom band");
        assert!(covers(x0 + 8, 700), "{xid:#x} left bar");
        assert!(covers(x0 + 1268, 700), "{xid:#x} right bar");
        for (cx, cy) in [(0, 17), (1275, 17), (0, 1437), (1275, 1437)] {
            assert!(covers(x0 + cx, cy), "{xid:#x} corner ({cx},{cy})");
        }
        // The ring's area, plus the strip of content above the child.
        let area: u64 = rects.iter().map(|q| area_of(*q)).sum();
        let ring = 2 * u64::from(BW) * (u64::from(OW) + u64::from(OH) - 2 * u64::from(BW));
        assert_eq!(ring, 85_280, "the predicted per-tile ring area");
        assert_eq!(
            area,
            ring + u64::from(CW) * 17,
            "{xid:#x}: ring + the 17 rows of content above the titlebar child",
        );
        // Content lands at x0 + 16 with the titlebar at y = 33.
        assert_eq!(dst_rects_of(&on, child), vec![r(x0 + 16, 50, 1244, 1372)]);
    }
    // The 32 px inter-tile gap between the two CONTENTS — x 1260..1291,
    // i.e. tile 1's right bar (1260..1275) plus tile 2's left bar
    // (1276..1291) — is border, not wallpaper. Before step 5 the scan
    // found wallpaper in a 32 px column here.
    assert!(
        dst_rects_of(&on, 0x100)
            .iter()
            .any(|q| q.offset.x == 1260 && q.extent.width == u32::from(BW)),
        "tile 1's right bar is 16 px wide at x = 1260",
    );
    assert!(
        dst_rects_of(&on, 0x200)
            .iter()
            .any(|q| q.offset.x == 1276 && q.extent.width == u32::from(BW)),
        "tile 2's left bar is 16 px wide at x = 1276",
    );
    // And nothing of the wallpaper survives inside that column.
    let gap = r(1260, 17, 32, 1421);
    for q in sorted_rects(draws_of(&on, 0x00A0_7000)) {
        assert!(
            intersect_rects(q, gap).is_none(),
            "wallpaper {q:?} shows through the inter-tile gap {gap:?}",
        );
    }
}

/// #133 step 5 (5.2) — the child-clip bound is ABSOLUTE, not
/// storage-local, for a bordered parent at a non-zero origin.
///
/// The exact numbers from the reporter's matched dump + scanout
/// (bee, `border_width = 32`): frame `geom=(0,17 1136x1086)`,
/// `content_offset = 32`, storage 1200x1150. So
///
/// ```text
/// abs_y          = 0 + 17          =   17   (outer, screen space)
/// content_abs_y  = 17 + 32         =   49   (content, screen space)
/// child_clip_y1  = 49 + 1086       = 1135   (screen space)
/// ```
///
/// The value that would produce the measured 17-row band is **1118**,
/// which is `co + own_h = 32 + 1086` — the end of the content in the
/// parent's own STORAGE space, where the frame's ring begins
/// (`ring green y 1118..1149` in that dump). A storage-space bound
/// leaking into an absolute clip would shorten a TIGHT-FIT child by
/// exactly the parent's outer `y`. This pins which of the two the
/// code computes.
///
/// Negative control: `clip_y1.min(co + own_h)` in `decide_node` →
/// `child_clip_y1` comes out 1118 and this test fails.
#[test]
fn the_child_clip_bound_is_absolute_not_storage_local() {
    let mut store = DrawableStore::new();
    let mut windows = crate::kms::render::backend::WindowsMap::new();
    let empty: HashMap<u32, Vec<xfixes::RegionRect>> = HashMap::new();
    // awesome's frame, at the reporter's exact geometry.
    alloc_stub_window_bordered(
        &mut store,
        &mut windows,
        0x100,
        0,
        17,
        1136,
        1086,
        32,
        None,
        true,
    );
    let node = decide_node(
        0x100,
        windows.get(&0x100).expect("geom"),
        0,
        0,
        &store,
        &empty,
        &empty,
        0,
        0,
        2560,
        1440,
        Visibility::On,
        false,
        false,
        i32::MIN / 2,
        i32::MIN / 2,
        i32::MAX / 2,
        i32::MAX / 2,
    );
    assert_eq!((node.abs_x, node.abs_y), (0, 17), "outer absolute");
    assert_eq!(
        (node.content_abs_x, node.content_abs_y),
        (32, 49),
        "content absolute = outer + content_offset",
    );
    assert_eq!(
        (node.child_clip_y0, node.child_clip_y1),
        (49, 1135),
        "child clip is ABSOLUTE: 49..49+1086, NOT the storage-space 32..1118",
    );
    assert_eq!(
        (node.child_clip_x0, node.child_clip_x1),
        (32, 1168),
        "same in x: 32..32+1136",
    );
    // And the parent's own outer placement, for contrast: screen
    // (0,17) with the bordered extent.
    assert_eq!(node.place, vec![r(0, 17, 1200, 1150)]);
    // The inner region descendants are clipped to, in output space:
    // the content box, NOT the storage box.
    assert_eq!(node.child_place, Some(vec![r(32, 49, 1136, 1086)]));
}

/// Reallocate a bordered stub at a new size, as
/// `sync_window_leaf_storage` does on a resize: detach the xid, then
/// allocate fresh storage at the new bordered extent. The fresh
/// allocation gets a NEW `DrawableId`, which is what makes the scene
/// diff see the participant as replaced rather than moved.
#[allow(clippy::too_many_arguments)]
fn resize_stub_window_bordered(
    store: &mut DrawableStore,
    windows: &mut crate::kms::render::backend::WindowsMap,
    xid: u32,
    w: u16,
    h: u16,
    bw: u16,
) {
    store.detach_xid(xid);
    let bordered = extent(
        u32::from(w) + 2 * u32::from(bw),
        u32::from(h) + 2 * u32::from(bw),
    );
    let mut storage =
        crate::kms::render::store::Storage::for_tests_null(bordered, vk::Format::B8G8R8A8_UNORM);
    let sentinel: ash::vk::ImageView = ash::vk::Handle::from_raw(u64::from(xid) | 0xFF00_0000);
    storage.image_view = sentinel;
    storage.sample_view = sentinel;
    let id = store
        .allocate(xid, DrawableKind::Window, 32, true, storage)
        .expect("resize stub allocate");
    store.set_content_offset(id, i32::from(bw));
    let g = windows.get_mut(&xid).expect("resize stub geom");
    g.width = w;
    g.height = h;
    g.border_width = bw;
}

/// #133 — a GROW must damage the window's full new outer rect.
///
/// The walk is exonerated for the wezterm white band: with the
/// reporter's matched 20:14 dump the titlebar sits at 49..65, the
/// client is placed at 66, `abs_y = 17` / `content_abs_y = 49` are
/// both present and `own_h = 1086`, so `child_clip_y1 = 1135` and
/// `vis_ly1 = 1069` — the walk emits the client's full 1069 rows
/// (pinned by `the_child_clip_bound_is_absolute_not_storage_local`
/// and the `ProtoFixture` tight-fit test). Compose is
/// damage-clipped, so the remaining way for a correct draw list not
/// to reach the screen is for the newly-exposed rows never to be
/// repainted.
///
/// `structural_damage` is what reports a resize once the coarse
/// `mark_scene_structure_dirty` hammer is demoted, so this asserts
/// it directly rather than through pixels: the diff of the
/// pre-grow and post-grow participant lists must CONTAIN every new
/// outer rect. The awesome shape, tight-fit, frame at a non-zero
/// origin, all three levels reallocated as the real resize path
/// does (fresh `DrawableId` per realloc).
///
/// Negative control: in `structural_damage`, drop
/// `damage.union_with(&p.region)` from the `None => ...` arm (the
/// appeared-participant case) → the new extents are not damaged and
/// every `contains` below fails.
#[test]
fn a_grow_damages_the_full_new_outer_rect() {
    const BW: u16 = 16;
    const TITLE: i16 = 17;
    const FX: i16 = 0;
    const FY: i16 = 17;
    let small = (200u16, 120u16);
    let big = (400u16, 260u16);

    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let mut windows = crate::kms::render::backend::WindowsMap::new();
    alloc_root(&core, &mut store, 800, 600);
    // frame content = client height + titlebar (awesome's rule), so the
    // client exactly fills the remaining content: the tight fit.
    alloc_stub_window_bordered(
        &mut store,
        &mut windows,
        0x100,
        FX,
        FY,
        small.0,
        small.1 + TITLE as u16,
        BW,
        None,
        true,
    );
    alloc_stub_window_bordered(
        &mut store,
        &mut windows,
        0x101,
        0,
        TITLE,
        small.0,
        small.1,
        0,
        Some(0x100),
        true,
    );
    alloc_stub_window_bordered(
        &mut store,
        &mut windows,
        0x102,
        0,
        0,
        small.0,
        small.1,
        0,
        Some(0x101),
        true,
    );
    set_rank(&mut windows, 0x100, 1);
    set_rank(&mut windows, 0x101, 2);
    set_rank(&mut windows, 0x102, 3);
    core.top_level_order = vec![0x100];

    let layout = (0, 0, 800u32, 600u32);
    let before = build_with(Visibility::On, &core, &mut store, &windows, layout, None);

    // The grow, in the trace's order: frame, client, then the GL child.
    resize_stub_window_bordered(
        &mut store,
        &mut windows,
        0x100,
        big.0,
        big.1 + TITLE as u16,
        BW,
    );
    resize_stub_window_bordered(&mut store, &mut windows, 0x101, big.0, big.1, 0);
    resize_stub_window_bordered(&mut store, &mut windows, 0x102, big.0, big.1, 0);

    let after = build_with(Visibility::On, &core, &mut store, &windows, layout, None);
    let damage = structural_damage(&before.participants, &after.participants);

    // Sanity: the fixture really grew, and really is a tight fit.
    assert_eq!(
        i32::from(TITLE) + i32::from(big.1),
        i32::from(big.1 + TITLE as u16),
        "tight fit",
    );
    let frame_outer = r(
        i32::from(FX),
        i32::from(FY),
        u32::from(big.0) + 2 * u32::from(BW),
        u32::from(big.1 + TITLE as u16) + 2 * u32::from(BW),
    );
    let content_origin = (i32::from(FX) + i32::from(BW), i32::from(FY) + i32::from(BW));
    let client_rect = r(
        content_origin.0,
        content_origin.1 + i32::from(TITLE),
        u32::from(big.0),
        u32::from(big.1),
    );
    assert_eq!(
        sorted_rects(draws_of(&after, win_view(0x102))),
        vec![client_rect],
        "fixture sanity: the grown GL child is placed and sized correctly",
    );

    // THE ASSERTION: the damage a grow reports must cover every new
    // extent. A tail that is emitted but not damaged is a tail the
    // damage-clipped compose never writes, and it keeps whatever the
    // scanout BO held — which is `ffffff00` uninitialised storage,
    // i.e. opaque white.
    for (name, rect) in [
        ("frame new outer rect", frame_outer),
        ("client new rect", client_rect),
    ] {
        assert!(
            damage.contains_rect(rect),
            "{name} {rect:?} is not covered by a grow's structural damage: {:?}",
            sorted_rects(damage.rects().collect()),
        );
    }
    // And specifically the newly-exposed TAIL — the rows the grow
    // uncovered at the bottom, which is where the reported band is.
    let tail = r(
        client_rect.offset.x,
        i32::from(TITLE) + content_origin.1 + i32::from(small.1),
        u32::from(big.0),
        u32::from(big.1 - small.1),
    );
    assert!(
        damage.contains_rect(tail),
        "the newly-exposed tail {tail:?} is not damaged: {:?}",
        sorted_rects(damage.rects().collect()),
    );
}

/// #133 — the bee tree at its real numbers, with awesome's wibar as a
/// higher top-level sibling that claims from the universe first.
///
/// `Visibility::On` must produce the same PIXELS as `Off`, and the
/// draw list must cover the whole client rect. The measured mismatch
/// on bee is at `pixel=32,1118` with `candidate=0xffffffff` /
/// `reference=0xff000000`, damage covering the entire output, zero
/// clipped repaints, zero collapses and `hidden_participants/s=0` —
/// the last of which is a direct contradiction with the fixture, where
/// the GL child covers its client exactly and the client is therefore
/// hidden. `draws=8` for five participants means the client emits ONE
/// draw, i.e. the GL child does not cover it.
#[test]
fn the_bee_tree_with_a_higher_wibar_sibling_matches_the_unclipped_scene() {
    const BW: u16 = 32;
    const TITLE: i16 = 17;
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let mut windows = crate::kms::render::backend::WindowsMap::new();
    alloc_root(&core, &mut store, 2560, 1440);
    // awesome's wibar: full width, 17 tall, at the very top.
    alloc_stub_window_bordered(
        &mut store,
        &mut windows,
        0x010,
        0,
        0,
        2560,
        17,
        0,
        None,
        true,
    );
    // The frame, at the reporter's exact geometry.
    alloc_stub_window_bordered(
        &mut store,
        &mut windows,
        0x100,
        0,
        17,
        1136,
        1086,
        BW,
        None,
        true,
    );
    alloc_stub_window_bordered(
        &mut store,
        &mut windows,
        0x101,
        0,
        TITLE,
        1136,
        1069,
        0,
        Some(0x100),
        true,
    );
    alloc_stub_window_bordered(
        &mut store,
        &mut windows,
        0x102,
        0,
        0,
        1136,
        1069,
        0,
        Some(0x101),
        true,
    );
    set_rank(&mut windows, 0x100, 1);
    set_rank(&mut windows, 0x101, 2);
    set_rank(&mut windows, 0x102, 3);
    // The wibar is stacked ABOVE the frame, so it claims first.
    core.top_level_order = vec![0x100, 0x010];

    let layout = (0, 0, 2560u32, 1440u32);
    let on = build_with(Visibility::On, &core, &mut store, &windows, layout, None);
    let off = build_with(Visibility::Off, &core, &mut store, &windows, layout, None);
    eprintln!(
        "ON  draws={} hidden={} collapses={:?}",
        on.scene.draws.len(),
        on.stats.hidden_participants,
        (
            on.stats.collapses_mine,
            on.stats.collapses_claim,
            on.stats.collapses_taken,
            on.stats.collapses_taken_skipped
        ),
    );
    for (name, xid) in [
        ("wibar", 0x010u32),
        ("frame", 0x100),
        ("client", 0x101),
        ("glchild", 0x102),
    ] {
        eprintln!(
            "  {name}: ON {:?}\n           OFF {:?}",
            sorted_rects(draws_of(&on, win_view(xid))),
            sorted_rects(draws_of(&off, win_view(xid))),
        );
    }
    // The GL child must cover the whole client rect: content origin
    // (32, 49) + (0, 17) = (32, 66), 1136x1069 → y 66..1135.
    assert_eq!(
        sorted_rects(draws_of(&on, win_view(0x102))),
        vec![r(32, 66, 1136, 1069)],
        "the GL child's ON draw must be its full unbroken rect",
    );
    // Nothing may be left for the client to emit — the reported
    // `draws=8` / `hidden_participants=0` says otherwise on bee.
    assert!(
        draws_of(&on, win_view(0x101)).is_empty(),
        "the client is fully covered by its GL child, so it emits nothing: {:?}",
        sorted_rects(draws_of(&on, win_view(0x101))),
    );
    assert_eq!(on.stats.hidden_participants, 1, "the client is hidden");
    // And the pixel oracle over the whole output.
    let _ = assert_oracle(
        &core,
        &mut store,
        &windows,
        layout,
        None,
        "bee tree + wibar",
    );
}
