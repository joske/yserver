use super::*;

/// Stage 3f.6 — `build_scene` walks top-level → mapped
/// descendants and produces draw entries in absolute coords.
/// Top-level at (50, 60), child at (10, 20) relative → child
/// emits at output coords (60, 80).
#[test]
fn build_scene_recurses_into_mapped_children() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let platform = PlatformBackend::for_tests();
    let mut windows = crate::kms::render::backend::WindowsMap::new();

    // Top-level @ (50, 60), 200×100.
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x100,
        50,
        60,
        200,
        100,
        None,
        true,
    );
    core.top_level_order.push(0x100);

    // Child @ (10, 20) relative to top-level, 40×30.
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x101,
        10,
        20,
        40,
        30,
        Some(0x100),
        true,
    );

    let built = build_scene(
        &core,
        &mut store,
        &windows,
        0,
        &platform,
        None,
        None,
        None,
        false,
        Visibility::Off,
    );
    let scene = built.scene;
    assert_eq!(scene.draws.len(), 2, "expected top-level + child draw");

    // Top-level at output (50, 60) since output layout origin is (0,0).
    let top = scene
        .draws
        .iter()
        .find(|d| d.dst_size[0] == 200.0 && d.dst_size[1] == 100.0)
        .expect("top-level draw present");
    assert_eq!(top.dst_origin, [50.0, 60.0]);

    // Child at absolute (60, 80) = top (50, 60) + child rel (10, 20).
    let child = scene
        .draws
        .iter()
        .find(|d| d.dst_size[0] == 40.0 && d.dst_size[1] == 30.0)
        .expect("child draw present");
    assert_eq!(child.dst_origin, [60.0, 80.0]);
}

/// X11 parent-clipping: a child window is clipped to its parent's
/// rectangle. fvwm (and other WMs) park oversized frame-decoration
/// windows in a tiny off-screen holding window so they're invisible;
/// yserver must not paint the whole child. Regression: fvwm's 1146×23
/// title bar, parked in a 10×10 holding window at (-10,-10), leaked a
/// ~1136×13 white strip onto the top-left of the screen because the
/// scene drew the child at full size (air/silence HW 2026-07-02).
#[test]
fn build_scene_clips_child_to_parent_bounds() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let platform = PlatformBackend::for_tests();
    let mut windows = crate::kms::render::backend::WindowsMap::new();

    // Small parent @ (100, 100), 10×10 (the holding window).
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x100,
        100,
        100,
        10,
        10,
        None,
        true,
    );
    core.top_level_order.push(0x100);

    // Oversized child @ (2, 3) relative, 100×50 — far larger than the
    // 10×10 parent. Only the intersection with the parent may show.
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x101,
        2,
        3,
        100,
        50,
        Some(0x100),
        true,
    );

    let built = build_scene(
        &core,
        &mut store,
        &windows,
        0,
        &platform,
        None,
        None,
        None,
        false,
        Visibility::Off,
    );
    let scene = built.scene;

    // The child draw must be clipped to the parent's rect, NOT the
    // full 100×50. Child abs (102,103) ∩ parent (100,100,110,110)
    // = (102,103)-(110,110) → 8×7.
    let child = scene
        .draws
        .iter()
        .find(|d| d.dst_origin == [102.0, 103.0])
        .expect("child draw present at its absolute origin");
    assert_eq!(
        child.dst_size,
        [8.0, 7.0],
        "child must be clipped to the parent's 10×10 bounds, not drawn \
             at full 100×50 (parent-clipping); got {:?}",
        child.dst_size,
    );
    assert!(
        (child.src_size[0] - 8.0 / 100.0).abs() < 1e-5
            && (child.src_size[1] - 7.0 / 50.0).abs() < 1e-5,
        "src_size must sample only the visible sub-region, got {:?}",
        child.src_size,
    );
    // No draw may exceed the parent's footprint.
    assert!(
        !scene
            .draws
            .iter()
            .any(|d| d.dst_size[0] > 10.0 || d.dst_size[1] > 10.0),
        "no draw may exceed the 10×10 parent, got {:?}",
        scene.draws,
    );
}

/// SHAPE bounding region clips the window's scene draw. Marco
/// uses `SHAPE-Request: Rectangles destination=Bounding` to set
/// a rounded-corner mask on frame windows; without honouring it
/// the scene paints the full rectangle and shows the scanout
/// clear colour (black) in the corners instead of the layer
/// below — diagnosed 2026-05-30 on non-composited MATE.
#[test]
fn build_scene_clips_window_to_shape_bounding() {
    use yserver_protocol::x11::xfixes::RegionRect;
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let platform = PlatformBackend::for_tests();
    let mut windows = crate::kms::render::backend::WindowsMap::new();

    // Top-level @ (50, 60), 200×100.
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x100,
        50,
        60,
        200,
        100,
        None,
        true,
    );
    core.top_level_order.push(0x100);

    // Bounding mask: a single sub-rect inset (10, 14) from the
    // window's top-left, 180×80 — analogous to one of marco's
    // rounded-corner approximation strips.
    core.shape_bounding.insert(
        0x100,
        vec![RegionRect {
            x: 10,
            y: 14,
            width: 180,
            height: 80,
        }],
    );

    let built = build_scene(
        &core,
        &mut store,
        &windows,
        0,
        &platform,
        None,
        None,
        None,
        false,
        Visibility::Off,
    );
    let scene = built.scene;

    // Exactly one draw for this window, clipped to the bounding
    // rect — NOT a full-window 200×100 draw.
    let window_draws: Vec<_> = scene
        .draws
        .iter()
        .filter(|d| d.dst_size != [200.0, 100.0])
        .collect();
    assert_eq!(
        window_draws.len(),
        1,
        "expected one draw per bounding rect, got {}: {:?}",
        scene.draws.len(),
        scene.draws,
    );
    let d = window_draws[0];
    // dst: window absolute origin + bounding-rect offset.
    assert_eq!(d.dst_origin, [60.0, 74.0], "dst_origin = (50+10, 60+14)");
    assert_eq!(d.dst_size, [180.0, 80.0], "dst_size = bounding rect");
    // src UV: the sub-region of the window's texture that
    // corresponds to the bounding rect.
    assert!(
        (d.src_origin[0] - 10.0 / 200.0).abs() < 1e-5
            && (d.src_origin[1] - 14.0 / 100.0).abs() < 1e-5,
        "src_origin = (10/200, 14/100), got {:?}",
        d.src_origin,
    );
    assert!(
        (d.src_size[0] - 180.0 / 200.0).abs() < 1e-5 && (d.src_size[1] - 80.0 / 100.0).abs() < 1e-5,
        "src_size = (180/200, 80/100), got {:?}",
        d.src_size,
    );
}

/// DRIFT 1 (findings 2026-06-18), live render half: the empty-vs-
/// absent bounding-shape distinction the Step-1a `Option` API
/// preserves. An EXPLICIT empty bounding region (`Some([])`, stored
/// as an empty Vec) must clip the window to nothing — zero draws —
/// whereas an ABSENT entry renders the full window. Before Step 1a
/// the backend deleted empty rects, collapsing the two so an empty
/// region wrongly rendered as a full window.
#[test]
fn build_scene_empty_bounding_emits_no_draw() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let platform = PlatformBackend::for_tests();
    let mut windows = crate::kms::render::backend::WindowsMap::new();

    alloc_stub_window(
        &mut store,
        &mut windows,
        0x100,
        50,
        60,
        200,
        100,
        None,
        true,
    );
    core.top_level_order.push(0x100);
    // Explicit EMPTY bounding region: entry present, zero rects.
    core.shape_bounding.insert(0x100, Vec::new());

    let built = build_scene(
        &core,
        &mut store,
        &windows,
        0,
        &platform,
        None,
        None,
        None,
        false,
        Visibility::Off,
    );
    assert!(
        built.scene.draws.is_empty(),
        "an explicit empty bounding region must emit no draw (window \
             clipped to nothing), got {:?}",
        built.scene.draws,
    );
}

#[test]
fn build_scene_absent_bounding_emits_full_window() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let platform = PlatformBackend::for_tests();
    let mut windows = crate::kms::render::backend::WindowsMap::new();

    alloc_stub_window(
        &mut store,
        &mut windows,
        0x100,
        50,
        60,
        200,
        100,
        None,
        true,
    );
    core.top_level_order.push(0x100);
    // No shape_bounding entry at all (absent) → full-window draw.

    let built = build_scene(
        &core,
        &mut store,
        &windows,
        0,
        &platform,
        None,
        None,
        None,
        false,
        Visibility::Off,
    );
    let window_draws: Vec<_> = built
        .scene
        .draws
        .iter()
        .filter(|d| d.dst_size == [200.0, 100.0])
        .collect();
    assert_eq!(
        window_draws.len(),
        1,
        "absent bounding shape must emit one full-window draw, got {:?}",
        built.scene.draws,
    );
    assert_eq!(window_draws[0].dst_origin, [50.0, 60.0]);
}

/// Stage 3f.6 — unmapped parent hides the entire subtree per
/// X11 MapWindow cascade semantics. Child stays scene-
/// participating but doesn't render because its ancestor is
/// unmapped.
#[test]
fn build_scene_unmapped_parent_hides_subtree() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let platform = PlatformBackend::for_tests();
    let mut windows = crate::kms::render::backend::WindowsMap::new();

    alloc_stub_window(
        &mut store,
        &mut windows,
        0x200,
        10,
        10,
        100,
        100,
        None,
        false, /* parent NOT mapped */
    );
    core.top_level_order.push(0x200);
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x201,
        0,
        0,
        50,
        50,
        Some(0x200),
        true, /* child IS mapped, but parent isn't */
    );

    let scene = build_scene(
        &core,
        &mut store,
        &windows,
        0,
        &platform,
        None,
        None,
        None,
        false,
        Visibility::Off,
    )
    .scene;
    assert!(
        scene.draws.is_empty(),
        "unmapped parent must short-circuit subtree (got {} draws)",
        scene.draws.len()
    );
}

/// Stage 3f.8 — when `cursor` is `Some`, `build_scene` emits an
/// additional top-of-z draw entry at the cursor's
/// hot-spot-adjusted position. The entry is the LAST element of
/// `draws` (last = topmost in z-order) and has
/// `alpha_passthrough=true` so the sprite's alpha actually
/// blends.
#[test]
fn build_scene_appends_cursor_draw_at_top_of_z() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let platform = PlatformBackend::for_tests();
    let mut windows = crate::kms::render::backend::WindowsMap::new();

    // One mapped top-level so we can verify "cursor is on top".
    alloc_stub_window(&mut store, &mut windows, 0x100, 0, 0, 400, 300, None, true);
    core.top_level_order.push(0x100);

    // Allocate a stub cursor storage entry (synthetic xid).
    let mut storage = crate::kms::render::store::Storage::for_tests_null(
        extent(16, 16),
        vk::Format::B8G8R8A8_UNORM,
    );
    // SAFETY: opaque u64 Vk handle for the cursor's view; the
    // stub Storage's `is_test_stub` flag means Drop won't free
    // it. Stamp both views so scene binds the sample-side.
    let cur_sentinel: ash::vk::ImageView = ash::vk::Handle::from_raw(0xCAFE_BABE);
    storage.image_view = cur_sentinel;
    storage.sample_view = cur_sentinel;
    let cursor_id = store
        .allocate(0xCAFE_0001, DrawableKind::Pixmap, 32, false, storage)
        .expect("alloc cursor stub");

    core.cursor_x = 50.0;
    core.cursor_y = 60.0;
    let cursor = CursorEntry {
        id: cursor_id,
        extent: extent(16, 16),
        hot_x: 0,
        hot_y: 0,
        record_version: 0,
        bgra_bytes: None,
    };

    let scene = build_scene(
        &core,
        &mut store,
        &windows,
        0,
        &platform,
        Some(cursor),
        None,
        None,
        false,
        Visibility::Off,
    )
    .scene;
    // 1 top-level + 1 cursor = 2.
    assert_eq!(scene.draws.len(), 2);
    let cursor_draw = scene.draws.last().expect("cursor draw");
    assert_eq!(cursor_draw.dst_origin, [50.0, 60.0]);
    assert_eq!(cursor_draw.dst_size, [16.0, 16.0]);
    assert!(
        cursor_draw.alpha_passthrough,
        "cursor must blend (sprite has transparent border)"
    );
}

/// Stage 4c.3 / 4c.5 — Automatic-mode invariant.
///
/// When a window W has `redirected_target = Some(B)` AND
/// `scene_participating == true` (Automatic redirect), the scene
/// entry for W blits FROM B's storage (its `image_view`), not
/// from W's own storage. W's geometry (`dst_origin`, `dst_size`)
/// stays driven by `windows[W]`. `sampled_ids` carries B_id
/// (not W_id) so damage/fence accounting follows the source the
/// scene actually read from. B is also marked
/// `scene_participating=true` per Stage 4c's Automatic-mode
/// pairing (the protocol handler issues
/// `set_backing_scene_participation(true)` alongside W's flip).
///
/// 4c.5 rename: framed around the Automatic-mode invariant per
/// task 4c.5 self-review — the assertion shape already matches.
#[test]
fn build_scene_automatic_redirect_keeps_window_via_backing_storage() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let platform = PlatformBackend::for_tests();
    let mut windows = crate::kms::render::backend::WindowsMap::new();

    // Window W @ (50, 60), 200×100 — emits at output coords
    // (50, 60) since the test output layout origin is (0, 0).
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x100,
        50,
        60,
        200,
        100,
        None,
        true,
    );
    core.top_level_order.push(0x100);

    // Allocate a separate backing pixmap B with its OWN sentinel
    // image_view, distinct from W's. B is allocated with
    // `scene_participating=false` (Pixmap default) — that's fine
    // for the build-scene path since the resolution looks up
    // storage directly; only the peek for B's damage needs the
    // flag, which we toggle below to verify the snapshot path
    // keys off `source_id`.
    let mut b_storage = crate::kms::render::store::Storage::for_tests_null(
        extent(200, 100),
        vk::Format::B8G8R8A8_UNORM,
    );
    let b_view: vk::ImageView = ash::vk::Handle::from_raw(0xB000_BEEF);
    b_storage.image_view = b_view;
    // Stub both views to the same sentinel — see
    // `alloc_stub_window` for rationale; tests verify
    // routing, not swizzle semantics.
    b_storage.sample_view = b_view;
    let b_id = store
        .allocate(0xB001, DrawableKind::Pixmap, 32, true, b_storage)
        .expect("alloc backing stub");

    // Confirm W and B have distinct image_views.
    let w_id = store.lookup(0x100).expect("w_id present");
    let w_view = store.get(w_id).expect("w drawable").storage.image_view;
    assert_ne!(
        w_view, b_view,
        "fixture sanity: W and B must have distinct sentinel views"
    );

    // Fixture sanity (4c.5 Automatic-mode invariant): W stays
    // scene_participating=true under Automatic redirect; the
    // backing also flips to scene_participating=true (the
    // protocol-side pairing). `alloc_stub_window(mapped=true)`
    // and the `allocate(..., true, _)` above wire both flags.
    assert!(
        store.get(w_id).unwrap().scene_participating,
        "Automatic redirect: W must stay scene_participating=true",
    );
    assert!(
        store.get(b_id).unwrap().scene_participating,
        "Automatic redirect: B must be scene_participating=true",
    );

    // Wire the redirect route: W's source-storage now resolves
    // through B.
    store.set_redirected_target(w_id, Some(b_id));

    let built = build_scene(
        &core,
        &mut store,
        &windows,
        0,
        &platform,
        None,
        None,
        None,
        false,
        Visibility::Off,
    );
    let scene = &built.scene;
    assert_eq!(
        scene.draws.len(),
        1,
        "expected one draw entry for W (geometry unchanged by redirect)"
    );
    let w_draw = &scene.draws[0];

    // Geometry still W's.
    assert_eq!(
        w_draw.dst_origin,
        [50.0, 60.0],
        "redirected W's on-screen rect must remain W's geometry"
    );
    assert_eq!(
        w_draw.dst_size,
        [200.0, 100.0],
        "redirected W's on-screen size must remain W's geometry"
    );

    // Storage handle reroutes to B. The stub fixture stamps
    // both `image_view` and `sample_view` to the same sentinel,
    // so this also implicitly verifies the scene-α fix is
    // binding the sample-side view (no separate handle to
    // distinguish in the stub world — production builds them
    // distinct via `PlatformBackend::build_sample_view`).
    assert_eq!(
        w_draw.image_view, b_view,
        "redirected W must sample FROM B's view, not W's"
    );

    // `sampled_ids` parallels `draws`; the entry for W must
    // carry B_id (the source the scene actually read from) so
    // damage/fence accounting follows the right drawable.
    assert_eq!(built.sampled_ids.len(), 1);
    assert_eq!(
        built.sampled_ids[0], b_id,
        "sampled_ids must carry source_id (B_id) for damage / fence keying"
    );
}

/// Stage 4c.5 — Manual-mode invariant.
///
/// `build_scene`'s `scene_participating` filter (scene.rs:1110 and
/// :922) drops any drawable with `scene_participating == false`
/// from the per-output draw list. Manual-redirected windows carry
/// `scene_participating=false` (the protocol handler issues
/// `set_window_scene_participation(W, false)` on Manual activation)
/// so they MUST NOT appear in `scene.draws` nor in
/// `built.sampled_ids`. Plain unredirected/Automatic windows
/// stay participating and continue to emit.
///
/// Setup: two top-level windows W1 + W2, both mapped and same
/// geometry shape (so the filter is the only thing distinguishing
/// them). W1 stays `scene_participating=true`; W2 is flipped to
/// `false` post-allocation via `set_scene_participating` to
/// mimic the Manual-redirect activation path. The build must
/// emit one draw (W1) and zero entries for W2.
#[test]
fn build_scene_skips_manual_redirected_window() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let platform = PlatformBackend::for_tests();
    let mut windows = crate::kms::render::backend::WindowsMap::new();

    // W1 @ (10, 20), 50×40 — Automatic / unredirected
    // (scene_participating=true via `alloc_stub_window`'s
    // `mapped` arg, which the helper forwards as the
    // `scene_participating` flag in `store.allocate`).
    alloc_stub_window(&mut store, &mut windows, 0x111, 10, 20, 50, 40, None, true);
    core.top_level_order.push(0x111);

    // W2 @ (100, 200), 60×30 — geometry that doesn't overlap
    // W1 so a stray draw entry would be unambiguous.
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x222,
        100,
        200,
        60,
        30,
        None,
        true,
    );
    core.top_level_order.push(0x222);

    // Flip W2 off the scene (Manual-redirect activation). Use
    // the store's setter directly — the backend method does
    // more bookkeeping (damage clear + scene-structure damage
    // rect) than this no-Vk scene-walk test needs.
    let w2_id = store.lookup(0x222).expect("w2 lookup");
    store.set_scene_participating(w2_id, false);
    let w1_id = store.lookup(0x111).expect("w1 lookup");
    assert!(
        store.get(w1_id).unwrap().scene_participating,
        "fixture sanity: W1 stays scene_participating=true",
    );
    assert!(
        !store.get(w2_id).unwrap().scene_participating,
        "fixture sanity: W2 must be scene_participating=false",
    );

    let built = build_scene(
        &core,
        &mut store,
        &windows,
        0,
        &platform,
        None,
        None,
        None,
        false,
        Visibility::Off,
    );
    let scene = &built.scene;

    // Only W1's draw entry must be present.
    assert_eq!(
        scene.draws.len(),
        1,
        "Manual-redirected W2 must be filtered from scene.draws (saw {} entries: {:?})",
        scene.draws.len(),
        scene.draws,
    );
    let w1_draw = &scene.draws[0];
    assert_eq!(
        w1_draw.dst_origin,
        [10.0, 20.0],
        "the surviving draw must be W1 (origin (10,20)), NOT W2 (origin (100,200))",
    );
    assert_eq!(
        w1_draw.dst_size,
        [50.0, 40.0],
        "the surviving draw must be W1 (50×40), NOT W2 (60×30)",
    );

    // sampled_ids mirrors draws — must carry W1's id only.
    assert_eq!(built.sampled_ids.len(), 1);
    assert_eq!(
        built.sampled_ids[0], w1_id,
        "sampled_ids must reference W1; W2 was filtered before push",
    );
}

/// Stage 4d — `build_scene` must skip non-redirected descendants
/// of a Manual-redirected ancestor. The descendants' paint
/// routes through `resolve_paint_target` to the ancestor's B;
/// emitting their own (stale) storage on top of the ancestor's B
/// would muddy the compositor output.
///
/// Audit #3 follow-up (2026-05-19): the test was originally
/// written against the degenerate state where the parent has
/// `scene_participating=false` *without* a redirected backing —
/// that state doesn't occur in real life (Manual-redirect
/// activation always sets `redirected_target` BEFORE flipping
/// `scene_participating=false`, see
/// `activate_redirect_backing_for`). Updated to mirror the
/// realistic state: frame has both a backing AND
/// `scene_participating=false`.
///
/// Phase 3.1 update: the parent is Manual-redirected so it ALSO
/// no longer emits (the compositor reads its backing via
/// `NameWindowPixmap` and re-emits it on the COW). The remaining
/// invariant is "the non-redirected child must NOT leak into
/// scene.draws"; the bystander stands in as a positive control.
#[test]
fn build_scene_prunes_descendants_of_manual_redirected_ancestor() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let platform = PlatformBackend::for_tests();
    let mut windows = crate::kms::render::backend::WindowsMap::new();

    // Frame W @ (100, 200), 200×150 — the manually-redirected
    // ancestor (CC's marco-decorated frame in production).
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x111,
        100,
        200,
        200,
        150,
        None,
        true,
    );
    core.top_level_order.push(0x111);

    // Child C inside frame W at relative (11, 41), 100×80.
    // scene_participating=true (regular window — only the
    // ancestor is redirected). This is CC's GtkWindow in
    // production: a regular window whose paints route to the
    // frame's redirected backing via resolve_paint_target's
    // ancestor walk.
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x112,
        11,
        41,
        100,
        80,
        Some(0x111),
        true,
    );

    // Bystander top-level W @ (500, 500) so a "did anything
    // get emitted?" assertion isn't ambiguous.
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x222,
        500,
        500,
        60,
        30,
        None,
        true,
    );
    core.top_level_order.push(0x222);

    // Set up realistic Manual-redirect state on frame W: allocate
    // a backing, point W's `redirected_target` at it, then flip
    // `scene_participating=false`. Child stays participating —
    // its paint will resolve to frame_B via
    // `resolve_paint_target`'s ancestor walk, NOT to its own
    // storage; so the child's storage stays stale, and emitting
    // it would muddy the frame_B emit underneath.
    let w_frame_id = store.lookup(0x111).expect("frame lookup");
    let mut frame_backing = crate::kms::render::store::Storage::for_tests_null(
        extent(200, 150),
        vk::Format::B8G8R8A8_UNORM,
    );
    let frame_backing_view: vk::ImageView = ash::vk::Handle::from_raw(0xBEEF_F111);
    frame_backing.image_view = frame_backing_view;
    frame_backing.sample_view = frame_backing_view;
    let frame_backing_id = store
        .allocate(0xB111, DrawableKind::Pixmap, 32, true, frame_backing)
        .expect("alloc frame backing");
    store.set_redirected_target(w_frame_id, Some(frame_backing_id));
    store.set_scene_participating(w_frame_id, false);
    let child_id = store.lookup(0x112).expect("child lookup");
    assert!(
        store.get(child_id).unwrap().scene_participating,
        "fixture sanity: child stays scene_participating=true",
    );

    let built = build_scene(
        &core,
        &mut store,
        &windows,
        0,
        &platform,
        None,
        None,
        None,
        false,
        Visibility::Off,
    );
    let scene = &built.scene;

    // Phase 3.1 — only the bystander emits. The Manual-redirected
    // frame is unconditionally skipped (compositor consumes its
    // backing offscreen via NameWindowPixmap); the non-redirected
    // child must also stay out (its paint resolves to frame_B via
    // the ancestor walk, so emitting its stale storage would muddy
    // the compositor's re-emit on the COW).
    assert_eq!(
        scene.draws.len(),
        1,
        "expected bystander only; got {} — Manual-redirected frame and \
             its non-redirected child must both stay out of scene.draws: {:?}",
        scene.draws.len(),
        scene.draws,
    );
    assert!(
        scene.draws.iter().any(|d| d.dst_origin == [500.0, 500.0]),
        "bystander draw missing: {:?}",
        scene.draws
    );
    // The "must not leak" property — frame backing AND child draw
    // entries must both be absent from scene.draws.
    assert!(
        !scene
            .draws
            .iter()
            .any(|d| d.dst_origin == [100.0, 200.0] && d.dst_size == [200.0, 150.0]),
        "Manual-redirected frame leaked into scene.draws: {:?}",
        scene.draws,
    );
    assert!(
        !scene.draws.iter().any(|d| d.dst_origin == [111.0, 241.0]),
        "non-redirected child of Manual-redirected ancestor leaked into scene.draws: {:?}",
        scene.draws,
    );
    // sampled_ids mirrors draws — bystander only, no frame_B, no child.
    let bystander_id = store.lookup(0x222).expect("bystander lookup");
    assert_eq!(built.sampled_ids.len(), 1);
    assert!(!built.sampled_ids.contains(&frame_backing_id));
    assert!(built.sampled_ids.contains(&bystander_id));
}

// Phase 3.1 — the legacy `build_scene_emits_manual_redirected_parent_backing_but_prunes_descendants`
// test was deleted here. Its sole purpose was to assert that a
// Manual-redirected top-level emits its backing directly into
// scanout — exactly the bug-shaped state Task 3.1 closes. The
// compositor (in production) reads the backing via
// `NameWindowPixmap` and re-emits it on the COW; the X server
// must never short-circuit that. `manual_redirected_top_level_skips_emit_unconditional`
// covers the replacement invariant.

/// Audit #3 (2026-05-19) — a Manual-redirected parent still
/// prunes its NON-redirected descendants (their paint resolves
/// to the parent's B via `resolve_paint_target` so the parent
/// emit covers them), but Automatic-redirected descendants have
/// their OWN backing — `resolve_paint_target` stops at them —
/// and MUST still emit. Pre-fix `prune_subtree=true` dropped
/// them unconditionally, matching the audit's "GTK/marco CSD
/// pattern: RedirectWindow(frame, Manual) +
/// RedirectSubwindows(frame, Automatic) makes Automatic
/// widgets vanish" symptom (Control Center missing menus /
/// widgets).
///
/// Phase 3.1 update: the Manual-redirected parent ALSO no longer
/// emits (compositor reads its backing via NameWindowPixmap).
/// The load-bearing assertion of this test is still "Automatic
/// child backing emits despite Manual ancestor"; the parent emit
/// is dropped from the expectation set.
#[test]
fn build_scene_emits_automatic_descendant_under_manual_ancestor() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let platform = PlatformBackend::for_tests();
    let mut windows = crate::kms::render::backend::WindowsMap::new();

    // Frame F at (100, 200), 200×150 — Manual-redirected
    // (scene_participating=false) with its own backing F_B.
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x111,
        100,
        200,
        200,
        150,
        None,
        true,
    );
    core.top_level_order.push(0x111);
    let frame_id = store.lookup(0x111).expect("frame lookup");

    let mut frame_backing = crate::kms::render::store::Storage::for_tests_null(
        extent(200, 150),
        vk::Format::B8G8R8A8_UNORM,
    );
    let frame_backing_view: vk::ImageView = ash::vk::Handle::from_raw(0xBEEF_F000);
    frame_backing.image_view = frame_backing_view;
    frame_backing.sample_view = frame_backing_view;
    let frame_backing_id = store
        .allocate(0xB111, DrawableKind::Pixmap, 32, true, frame_backing)
        .expect("alloc frame backing");
    store.set_redirected_target(frame_id, Some(frame_backing_id));
    store.set_scene_participating(frame_id, false);

    // Automatic-redirected child C at (11, 41) inside F — own
    // backing C_B; scene_participating=true (Automatic).
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x112,
        11,
        41,
        100,
        80,
        Some(0x111),
        true,
    );
    let child_id = store.lookup(0x112).expect("child lookup");

    let mut child_backing = crate::kms::render::store::Storage::for_tests_null(
        extent(100, 80),
        vk::Format::B8G8R8A8_UNORM,
    );
    let child_backing_view: vk::ImageView = ash::vk::Handle::from_raw(0xBEEF_C000);
    child_backing.image_view = child_backing_view;
    child_backing.sample_view = child_backing_view;
    let child_backing_id = store
        .allocate(0xB112, DrawableKind::Pixmap, 32, true, child_backing)
        .expect("alloc child backing");
    store.set_redirected_target(child_id, Some(child_backing_id));
    // Automatic mode → child window stays scene_participating=true.
    assert!(
        store.get(child_id).unwrap().scene_participating,
        "fixture sanity: Automatic-redirected child stays scene_participating=true",
    );

    let built = build_scene(
        &core,
        &mut store,
        &windows,
        0,
        &platform,
        None,
        None,
        None,
        false,
        Visibility::Off,
    );
    let scene = &built.scene;

    // Phase 3.1 — only the Automatic child backing emits, at
    // (111, 241) (= F.pos + C.pos relative). Parent F is
    // Manual-redirected so it stays out of scene.draws; the
    // compositor consumes its backing offscreen via
    // NameWindowPixmap.
    assert_eq!(
        scene.draws.len(),
        1,
        "expected automatic-child backing only (Manual parent skipped); got {:?}",
        scene.draws
    );
    assert!(
        !scene
            .draws
            .iter()
            .any(|d| d.dst_origin == [100.0, 200.0] && d.dst_size == [200.0, 150.0]),
        "Manual parent backing must NOT emit: {:?}",
        scene.draws
    );
    assert!(
        scene
            .draws
            .iter()
            .any(|d| d.dst_origin == [111.0, 241.0] && d.dst_size == [100.0, 80.0]),
        "automatic child backing draw missing: {:?}",
        scene.draws
    );
    assert!(!built.sampled_ids.contains(&frame_backing_id));
    assert!(built.sampled_ids.contains(&child_backing_id));
}

/// Phase 1 pre-cleanup — when no COW is registered
/// (`cow=None`), `build_scene` walks the top-level order and
/// emits a draw entry per mapped top-level. This preserves the
/// legacy non-redirected path that Phase 1 (COW-authoritative)
/// leaves unchanged; the `cow=Some` shape (top-levels stripped)
/// gets its own dedicated test.
#[test]
fn build_scene_cow_none_emits_top_levels() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let platform = PlatformBackend::for_tests();
    let mut windows = crate::kms::render::backend::WindowsMap::new();

    // Two mapped top-levels.
    alloc_stub_window(&mut store, &mut windows, 0x100, 0, 0, 100, 80, None, true);
    core.top_level_order.push(0x100);
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x101,
        200,
        150,
        120,
        90,
        None,
        true,
    );
    core.top_level_order.push(0x101);

    let built = build_scene(
        &core,
        &mut store,
        &windows,
        0,
        &platform,
        None, // no cursor in this fixture
        None,
        None, // cow_host_xid — Phase 2.6 (None = no compositor active)
        false,
        Visibility::Off,
    );
    let scene = &built.scene;

    // Expect: top-level 0x100, top-level 0x101. Two entries
    // total (no cursor, no COW).
    assert_eq!(
        scene.draws.len(),
        2,
        "expected 2 top-levels, got {} draws: {:?}",
        scene.draws.len(),
        scene.draws,
    );

    // Top-level 0x100 at (0, 0) sized 100×80.
    assert_eq!(
        scene.draws[0].dst_origin,
        [0.0, 0.0],
        "first top-level origin",
    );
    assert_eq!(
        scene.draws[0].dst_size,
        [100.0, 80.0],
        "first top-level size",
    );
    // Top-level 0x101 at (200, 150) sized 120×90.
    assert_eq!(
        scene.draws[1].dst_origin,
        [200.0, 150.0],
        "second top-level origin",
    );
    assert_eq!(
        scene.draws[1].dst_size,
        [120.0, 90.0],
        "second top-level size",
    );

    // No draw should be screen-extent (no COW present).
    for d in &scene.draws {
        assert_ne!(
            d.dst_size,
            [800.0, 600.0],
            "no draw should be screen-extent when cow=None: {:?}",
            d,
        );
    }
}

/// Phase 1 pre-cleanup — when no COW is registered
/// (`cow=None`), the cursor draw must still be appended at
/// the top of z above the top-level draws. This preserves the
/// legacy non-redirected cursor-on-top assertion that Phase 1
/// leaves unchanged. The COW-present cursor ordering (top-levels
/// stripped, COW below cursor) gets its own dedicated test.
#[test]
fn build_scene_cow_none_cursor_at_top() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let platform = PlatformBackend::for_tests();
    let mut windows = crate::kms::render::backend::WindowsMap::new();

    // One mapped top-level so the scene has anchor content.
    alloc_stub_window(&mut store, &mut windows, 0x100, 0, 0, 400, 300, None, true);
    core.top_level_order.push(0x100);

    // Cursor sprite.
    let mut cursor_storage = crate::kms::render::store::Storage::for_tests_null(
        extent(16, 16),
        vk::Format::B8G8R8A8_UNORM,
    );
    let cur2_sentinel: ash::vk::ImageView = ash::vk::Handle::from_raw(0xCAFE_BABE);
    cursor_storage.image_view = cur2_sentinel;
    cursor_storage.sample_view = cur2_sentinel;
    let cursor_id = store
        .allocate(0xCAFE_0002, DrawableKind::Pixmap, 32, false, cursor_storage)
        .expect("alloc cursor stub");
    core.cursor_x = 50.0;
    core.cursor_y = 60.0;
    let cursor = CursorEntry {
        id: cursor_id,
        extent: extent(16, 16),
        hot_x: 0,
        hot_y: 0,
        record_version: 0,
        bgra_bytes: None,
    };

    let built = build_scene(
        &core,
        &mut store,
        &windows,
        0,
        &platform,
        Some(cursor),
        None,
        None, // cow_host_xid — Phase 2.6 (None = no compositor active)
        false,
        Visibility::Off,
    );
    let scene = &built.scene;

    // Expect: top-level, cursor — 2 draws, in that order.
    assert_eq!(
        scene.draws.len(),
        2,
        "expected top-level + cursor = 2 draws, got {}: {:?}",
        scene.draws.len(),
        scene.draws,
    );
    // Last draw = cursor (16×16).
    assert_eq!(
        scene.draws.last().expect("cursor").dst_size,
        [16.0, 16.0],
        "cursor must be the top-of-z draw",
    );
    // First draw = top-level (400×300).
    assert_eq!(
        scene.draws[0].dst_size,
        [400.0, 300.0],
        "top-level must be below cursor",
    );
}

/// Phase 2.6 — `under_cow_subtree` recursion flag propagates
/// `alpha_passthrough = true` to every `CompositeDraw` emitted
/// inside the COW subtree (the COW top-level itself + all of its
/// descendants). Non-COW top-levels (the no-compositor path)
/// emit with `alpha_passthrough = false`.
#[test]
fn cow_subtree_draws_inherit_alpha_passthrough_true() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let platform = PlatformBackend::for_tests();
    let mut windows = crate::kms::render::backend::WindowsMap::new();

    // Non-COW top-level W @ (0, 0), 200×200.
    alloc_stub_window(&mut store, &mut windows, 0xA1, 0, 0, 200, 200, None, true);
    core.top_level_order.push(0xA1);

    // COW host xid @ (0, 0), 800×600 — matches PlatformBackend::for_tests output.
    let cow_xid: u32 = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
    alloc_stub_window(
        &mut store,
        &mut windows,
        cow_xid,
        0,
        0,
        800,
        600,
        None,
        true,
    );
    core.top_level_order.push(cow_xid);

    // Compositor stage as child of COW @ (0, 0), 800×600.
    alloc_stub_window(
        &mut store,
        &mut windows,
        0xB1,
        0,
        0,
        800,
        600,
        Some(cow_xid),
        true,
    );

    let built = build_scene(
        &core,
        &mut store,
        &windows,
        0,
        &platform,
        None,
        None,
        Some(cow_xid),
        false,
        Visibility::Off,
    );
    let scene = &built.scene;

    // The non-COW W (200×200) must have alpha_passthrough=false.
    let w_draw = scene
        .draws
        .iter()
        .find(|d| d.dst_size == [200.0, 200.0])
        .expect("W draw present");
    assert!(
        !w_draw.alpha_passthrough,
        "non-COW top-level uses opaque blend (alpha_passthrough=false)",
    );

    // COW + stage (both 800×600) must have alpha_passthrough=true.
    let cow_or_stage_draws: Vec<_> = scene
        .draws
        .iter()
        .filter(|d| d.dst_size == [800.0, 600.0])
        .collect();
    assert!(
        !cow_or_stage_draws.is_empty(),
        "COW and stage emitted: {:?}",
        scene.draws,
    );
    for d in cow_or_stage_draws {
        assert!(
            d.alpha_passthrough,
            "COW subtree draw must have alpha_passthrough=true: {:?}",
            d,
        );
    }
}

/// Phase 2.7 — the COW must emit via the normal `top_level_order`
/// walk, NOT via a special post-walk append. With the COW as the
/// sole top-level, the scene contains exactly one draw sourced
/// from the COW's storage (alpha_passthrough=true from Task 2.6),
/// not two.
#[test]
fn build_scene_does_not_append_cow_after_top_level_walk() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let platform = PlatformBackend::for_tests();
    let mut windows = crate::kms::render::backend::WindowsMap::new();

    let cow_xid: u32 = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
    alloc_stub_window(
        &mut store,
        &mut windows,
        cow_xid,
        0,
        0,
        800,
        600,
        None,
        true,
    );
    core.top_level_order.push(cow_xid);

    let built = build_scene(
        &core,
        &mut store,
        &windows,
        0,
        &platform,
        None,
        None,
        Some(cow_xid),
        false,
        Visibility::Off,
    );
    let scene = &built.scene;

    let cow_draws: Vec<_> = scene
        .draws
        .iter()
        .filter(|d| d.dst_size == [800.0, 600.0])
        .collect();
    assert_eq!(
        cow_draws.len(),
        1,
        "exactly one COW draw — no special append on top of top_level_order walk; got {:?}",
        scene.draws,
    );
    assert!(
        cow_draws[0].alpha_passthrough,
        "COW draw still has alpha_passthrough=true",
    );
}

/// Phase 3.1 — a Manual-redirected top-level (own
/// `redirected_target` + `scene_participating=false`) must NEVER
/// emit a `CompositeDraw` from its backing, regardless of whether
/// the COW is materialized. Xorg's `compCheckRedirect` ensures
/// Manual-redirected windows go offscreen for the compositor to
/// read via `NameWindowPixmap`; the X server must not also blit
/// the backing into scanout.
#[test]
fn manual_redirected_top_level_skips_emit_unconditional() {
    for cow_host_xid in [None, Some(0x103_u32)] {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let platform = PlatformBackend::for_tests();
        let mut windows = crate::kms::render::backend::WindowsMap::new();

        // W with a redirected backing (Manual mode:
        // scene_participating=false). Unique sentinel handle so
        // a stray draw entry is unambiguous.
        let w: u32 = 0xA1;
        alloc_stub_window(&mut store, &mut windows, w, 100, 100, 50, 50, None, true);
        let w_id = store.lookup(w).expect("w lookup");
        let mut backing = crate::kms::render::store::Storage::for_tests_null(
            extent(50, 50),
            PlatformBackend::format_for_depth(24),
        );
        let view: vk::ImageView = ash::vk::Handle::from_raw(0xBEEF_0000);
        backing.image_view = view;
        backing.sample_view = view;
        let b_id = store
            .allocate(0xB0A1, DrawableKind::Pixmap, 24, true, backing)
            .expect("alloc manual backing");
        store.set_redirected_target(w_id, Some(b_id));
        store.set_scene_participating(w_id, false);
        core.top_level_order.push(w);

        if let Some(cow_xid) = cow_host_xid {
            alloc_stub_window(
                &mut store,
                &mut windows,
                cow_xid,
                0,
                0,
                800,
                600,
                None,
                true,
            );
            core.top_level_order.push(cow_xid);
        }

        let built = build_scene(
            &core,
            &mut store,
            &windows,
            0,
            &platform,
            None,
            None,
            cow_host_xid,
            false,
            Visibility::Off,
        );
        let scene = &built.scene;

        let w_draws: Vec<_> = scene
            .draws
            .iter()
            .filter(|d| d.dst_size == [50.0, 50.0])
            .collect();
        assert!(
            w_draws.is_empty(),
            "Manual-redirected W must NOT emit (cow={cow_host_xid:?}): {:?}",
            scene.draws,
        );
    }
}

/// Issue #98 — an opaque, output-covering UNREDIRECTED top-level must
/// suppress the COW even when the compositor keeps a helper window
/// stacked ABOVE it, provided that helper lies entirely off-output.
///
/// Measured on eiger (Asahi, Cinnamon session): muffin parks 1x1
/// helper windows at (-200,-200) and raises them above the managed
/// stack. The top-down probe stopped on one of those, concluded "the
/// topmost window does not cover the output", and left the COW
/// painting the desktop composite over the window muffin had just
/// unredirected — so fullscreen video/games rendered as the wallpaper
/// while their audio kept playing.
#[test]
fn offscreen_helper_above_fullscreen_still_suppresses_cow() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let platform = PlatformBackend::for_tests();
    let mut windows = crate::kms::render::backend::WindowsMap::new();

    // The unredirected fullscreen window: covers the 800x600 output,
    // opaque (depth != 32), scene-participating (drawn by us).
    let fs: u32 = 0x00F5;
    alloc_stub_window(&mut store, &mut windows, fs, 0, 0, 800, 600, None, true);
    windows.get_mut(&fs).expect("fs geom").depth = 24;
    core.top_level_order.push(fs);

    // muffin's off-screen 1x1 helper, stacked above `fs`.
    let helper: u32 = 0x00AE;
    alloc_stub_window(
        &mut store,
        &mut windows,
        helper,
        -200,
        -200,
        1,
        1,
        None,
        true,
    );
    windows.get_mut(&helper).expect("helper geom").depth = 24;
    core.top_level_order.push(helper);

    // The COW, always on top.
    let cow: u32 = 0x0103;
    alloc_stub_window(&mut store, &mut windows, cow, 0, 0, 800, 600, None, true);
    core.top_level_order.push(cow);

    let built = build_scene(
        &core,
        &mut store,
        &windows,
        0,
        &platform,
        None,
        None,
        Some(cow),
        false,
        Visibility::Off,
    );

    let cow_view: vk::ImageView = ash::vk::Handle::from_raw(u64::from(cow) | 0xFF00_0000);
    assert!(
        !built.scene.draws.iter().any(|d| d.image_view == cow_view),
        "COW must be suppressed by the opaque fullscreen unredirected \
             window even with an off-output helper stacked above it: {:?}",
        built.scene.draws,
    );
    let fs_view: vk::ImageView = ash::vk::Handle::from_raw(u64::from(fs) | 0xFF00_0000);
    assert!(
        built.scene.draws.iter().any(|d| d.image_view == fs_view),
        "the fullscreen window itself must still emit: {:?}",
        built.scene.draws,
    );
}

/// Issue #98 negative — the off-output filter must not turn into
/// blanket over-suppression. A window that IS on this output and does
/// NOT cover it (an ordinary floating window above the fullscreen one)
/// still keeps the COW alive; suppressing it there would erase every
/// redirected window the compositor draws, i.e. the whole desktop.
#[test]
fn on_output_non_covering_window_above_fullscreen_keeps_cow() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let platform = PlatformBackend::for_tests();
    let mut windows = crate::kms::render::backend::WindowsMap::new();

    let fs: u32 = 0x00F5;
    alloc_stub_window(&mut store, &mut windows, fs, 0, 0, 800, 600, None, true);
    windows.get_mut(&fs).expect("fs geom").depth = 24;
    core.top_level_order.push(fs);

    // On-output, non-covering window stacked above the fullscreen one.
    let float: u32 = 0x00BF;
    alloc_stub_window(
        &mut store,
        &mut windows,
        float,
        100,
        100,
        200,
        150,
        None,
        true,
    );
    windows.get_mut(&float).expect("float geom").depth = 24;
    core.top_level_order.push(float);

    let cow: u32 = 0x0103;
    alloc_stub_window(&mut store, &mut windows, cow, 0, 0, 800, 600, None, true);
    core.top_level_order.push(cow);

    let built = build_scene(
        &core,
        &mut store,
        &windows,
        0,
        &platform,
        None,
        None,
        Some(cow),
        false,
        Visibility::Off,
    );

    let cow_view: vk::ImageView = ash::vk::Handle::from_raw(u64::from(cow) | 0xFF00_0000);
    assert!(
        built.scene.draws.iter().any(|d| d.image_view == cow_view),
        "COW must survive when the topmost on-output window does not \
             cover the output: {:?}",
        built.scene.draws,
    );
}

/// Phase 3.1 negative — an Automatic-redirected top-level (own
/// `redirected_target` + `scene_participating=true`) still emits
/// a draw. Only the Manual mode (the bug-shaped case the gate
/// closes) is unconditionally skipped.
#[test]
fn automatic_redirected_top_level_still_emits() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let platform = PlatformBackend::for_tests();
    let mut windows = crate::kms::render::backend::WindowsMap::new();

    let w: u32 = 0xA2;
    alloc_stub_window(&mut store, &mut windows, w, 100, 100, 50, 50, None, true);
    let w_id = store.lookup(w).expect("w lookup");
    let mut backing = crate::kms::render::store::Storage::for_tests_null(
        extent(50, 50),
        PlatformBackend::format_for_depth(24),
    );
    let view: vk::ImageView = ash::vk::Handle::from_raw(0xBEEF_0001);
    backing.image_view = view;
    backing.sample_view = view;
    let b_id = store
        .allocate(0xB0A2, DrawableKind::Pixmap, 24, true, backing)
        .expect("alloc automatic backing");
    store.set_redirected_target(w_id, Some(b_id));
    // scene_participating left as default true (Automatic).
    core.top_level_order.push(w);

    let built = build_scene(
        &core,
        &mut store,
        &windows,
        0,
        &platform,
        None,
        None,
        None,
        false,
        Visibility::Off,
    );
    let scene = &built.scene;

    let w_draws: Vec<_> = scene
        .draws
        .iter()
        .filter(|d| d.dst_size == [50.0, 50.0])
        .collect();
    assert_eq!(
        w_draws.len(),
        1,
        "Automatic-redirected W still emits one draw: {:?}",
        scene.draws,
    );
}

/// Phase 6.1 — full compositor flow in one scenario. Exercises the
/// structural facts the COW redesign delivers, headless via a
/// direct `build_scene` call (no live Vulkan device required):
///
/// 1. A materialized COW (`windows` entry + `top_level_order`
///    slot, per Task 2.2) emits exactly once via the normal
///    `top_level_order` walk (Phase 2.7), with
///    `alpha_passthrough=true` (Phase 2.6).
/// 2. A stage child of the COW with content emits exactly once via
///    the COW-subtree recursion (Phase 2.6/2.7), also
///    `alpha_passthrough=true`.
/// 3. A Manual-redirected sibling top-level (own
///    `redirected_target` + `scene_participating=false`) emits
///    ZERO draws (Phase 3.1) — even though the COW is materialized.
/// 4. Ordering: the COW-subtree draws appear after the earlier
///    non-COW top-level (the Manual sibling contributes nothing).
///
/// Sizes are chosen so each source is unambiguously identifiable by
/// `dst_size`:
///   - early non-COW top-level W: 200×200
///   - Manual-redirected sibling S: 50×50  (must not appear)
///   - COW host:                    800×600
///   - stage (COW child):           640×480
#[test]
fn compositor_stage_under_cow_emits_via_recursion_and_manual_siblings_skip() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let platform = PlatformBackend::for_tests();
    let mut windows = crate::kms::render::backend::WindowsMap::new();

    // (1) An earlier, ordinary non-COW top-level W @ (0,0), 200×200.
    // Establishes a "before" position to anchor ordering.
    let w: u32 = 0xC001;
    alloc_stub_window(&mut store, &mut windows, w, 0, 0, 200, 200, None, true);
    core.top_level_order.push(w);

    // (2) A Manual-redirected sibling top-level S @ (100,100), 50×50.
    // redirected_target + scene_participating=false → Manual mode.
    let s: u32 = 0xC002;
    alloc_stub_window(&mut store, &mut windows, s, 100, 100, 50, 50, None, true);
    let s_id = store.lookup(s).expect("s lookup");
    let mut s_backing = crate::kms::render::store::Storage::for_tests_null(
        extent(50, 50),
        PlatformBackend::format_for_depth(24),
    );
    let s_view: vk::ImageView = ash::vk::Handle::from_raw(0xDEAD_0050);
    s_backing.image_view = s_view;
    s_backing.sample_view = s_view;
    let s_backing_id = store
        .allocate(0xB0C2, DrawableKind::Pixmap, 24, true, s_backing)
        .expect("alloc manual sibling backing");
    store.set_redirected_target(s_id, Some(s_backing_id));
    store.set_scene_participating(s_id, false);
    core.top_level_order.push(s);

    // (3) The materialized COW host @ (0,0), 800×600 (matches the
    // PlatformBackend::for_tests output extent). This stands in for
    // GetOverlayWindow having created the windows entry +
    // top_level_order slot (Task 2.2).
    let cow_xid: u32 = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
    alloc_stub_window(
        &mut store,
        &mut windows,
        cow_xid,
        0,
        0,
        800,
        600,
        None,
        true,
    );
    core.top_level_order.push(cow_xid);

    // (4) The compositor stage as a child of the COW @ (0,0),
    // 640×480 — content the WM paints into the overlay.
    let stage: u32 = 0xC003;
    alloc_stub_window(
        &mut store,
        &mut windows,
        stage,
        0,
        0,
        640,
        480,
        Some(cow_xid),
        true,
    );

    let built = build_scene(
        &core,
        &mut store,
        &windows,
        0,
        &platform,
        None,
        None,
        Some(cow_xid),
        false,
        Visibility::Off,
    );
    let scene = &built.scene;

    // Fact A — Manual-redirected sibling S emits ZERO draws.
    let s_draws = scene
        .draws
        .iter()
        .filter(|d| d.dst_size == [50.0, 50.0])
        .count();
    assert_eq!(
        s_draws, 0,
        "Manual-redirected sibling must not emit, even with COW materialized: {:?}",
        scene.draws,
    );

    // Fact B — stage (COW child) emits exactly ONE draw with
    // alpha_passthrough=true.
    let stage_draws: Vec<_> = scene
        .draws
        .iter()
        .filter(|d| d.dst_size == [640.0, 480.0])
        .collect();
    assert_eq!(
        stage_draws.len(),
        1,
        "stage emits exactly once via COW subtree recursion: {:?}",
        scene.draws,
    );
    assert!(
        stage_draws[0].alpha_passthrough,
        "stage draw inherits alpha_passthrough=true from the COW subtree: {:?}",
        stage_draws[0],
    );

    // Fact C — COW emits exactly ONE draw with alpha_passthrough=true
    // via the normal top_level_order walk (no special post-walk append).
    let cow_draws: Vec<_> = scene
        .draws
        .iter()
        .filter(|d| d.dst_size == [800.0, 600.0])
        .collect();
    assert_eq!(
        cow_draws.len(),
        1,
        "COW emits exactly once via top_level_order walk: {:?}",
        scene.draws,
    );
    assert!(
        cow_draws[0].alpha_passthrough,
        "COW draw has alpha_passthrough=true: {:?}",
        cow_draws[0],
    );

    // The earlier non-COW top-level W emits one opaque draw.
    let w_pos = scene
        .draws
        .iter()
        .position(|d| d.dst_size == [200.0, 200.0])
        .expect("W draw present");
    assert!(
        !scene.draws[w_pos].alpha_passthrough,
        "non-COW top-level W uses opaque blend (alpha_passthrough=false)",
    );

    // Fact D — ordering: the COW-subtree draws (COW host + stage)
    // appear AFTER the earlier non-COW top-level W. The Manual
    // sibling contributes nothing in between.
    let cow_pos = scene
        .draws
        .iter()
        .position(|d| d.dst_size == [800.0, 600.0])
        .expect("COW draw present");
    let stage_pos = scene
        .draws
        .iter()
        .position(|d| d.dst_size == [640.0, 480.0])
        .expect("stage draw present");
    assert!(
        w_pos < cow_pos && w_pos < stage_pos,
        "COW subtree draws come after the earlier top-level W: w={w_pos} cow={cow_pos} stage={stage_pos}",
    );
    // Within the COW subtree the host emits before its stage child.
    assert!(
        cow_pos < stage_pos,
        "COW host draw precedes its stage child in the subtree recursion: cow={cow_pos} stage={stage_pos}",
    );
}
