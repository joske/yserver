use super::*;

#[test]
fn win_gravity_delta_matches_x11_table() {
    // X11 window gravity: on a parent resize of (dw,dh), a child moves
    // by (gx·dw, gy·dh) where (gx,gy) ∈ {0, ½, 1} per gravity. Verify
    // the full table with dw=100, dh=200 (even → exact halves).
    let (dw, dh) = (100, 200);
    assert_eq!(win_gravity_delta(1, dw, dh), (0, 0)); // NorthWest
    assert_eq!(win_gravity_delta(2, dw, dh), (50, 0)); // North
    assert_eq!(win_gravity_delta(3, dw, dh), (100, 0)); // NorthEast
    assert_eq!(win_gravity_delta(4, dw, dh), (0, 100)); // West
    assert_eq!(win_gravity_delta(5, dw, dh), (50, 100)); // Center
    assert_eq!(win_gravity_delta(6, dw, dh), (100, 100)); // East
    assert_eq!(win_gravity_delta(7, dw, dh), (0, 200)); // SouthWest
    assert_eq!(win_gravity_delta(8, dw, dh), (50, 200)); // South
    assert_eq!(win_gravity_delta(9, dw, dh), (100, 200)); // SouthEast
    assert_eq!(win_gravity_delta(10, dw, dh), (0, 0)); // Static (pure resize)
    assert_eq!(win_gravity_delta(0, dw, dh), (0, 0)); // Unmap → no shift
}

#[test]
fn win_gravity_south_child_follows_parent_grow() {
    // fvwm shade/unshade: a South-gravity child parked above the fold
    // returns into view when the holder grows. Child at (0,-972);
    // holder grows 1136×97 → 1136×1069 (dh=+972); South(8): y += 972 → 0.
    let mut table = ResourceTable::new();
    let (parent, child) = (0x100, 0x101);
    table.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(parent),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 1136,
            height: 97,
            border_width: 0,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = table.map_window(ResourceId(parent));
    table.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(child),
            parent: ResourceId(parent),
            x: 0,
            y: -972,
            width: 1136,
            height: 1069,
            border_width: 0,
            class: 1,
            visual: ROOT_VISUAL,
            win_gravity: Some(8), // South
            ..Default::default()
        },
    );
    let _ = table.map_window(ResourceId(child));

    let moves = table.apply_win_gravity(ResourceId(parent), 1136, 97, 1136, 1069);
    assert_eq!(moves, vec![(ResourceId(child), 0, 0)]);
    let c = table.window(ResourceId(child)).expect("child");
    assert_eq!(
        (c.x, c.y),
        (0, 0),
        "South-gravity child moves down by the full height delta",
    );
}

#[test]
fn win_gravity_default_northwest_is_noop() {
    // A default (NorthWest) child does not move when the parent resizes.
    let mut table = ResourceTable::new();
    let (parent, child) = (0x200, 0x201);
    table.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(parent),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            border_width: 0,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = table.map_window(ResourceId(parent));
    table.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(child),
            parent: ResourceId(parent),
            x: 10,
            y: 20,
            width: 30,
            height: 30,
            border_width: 0,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default() // win_gravity defaults to NorthWest (1)
        },
    );
    let _ = table.map_window(ResourceId(child));

    let moves = table.apply_win_gravity(ResourceId(parent), 100, 100, 400, 300);
    assert!(moves.is_empty(), "NorthWest child must not move");
    let c = table.window(ResourceId(child)).expect("child");
    assert_eq!((c.x, c.y), (10, 20));
}

#[derive(Debug, Clone, Copy)]
enum InitialState {
    Viewable,
    Unviewable,
    Unmapped,
}

fn arb_initial() -> impl Strategy<Value = InitialState> {
    prop_oneof![
        Just(InitialState::Viewable),
        Just(InitialState::Unviewable),
        Just(InitialState::Unmapped),
    ]
}

#[test]
fn fresh_window_has_no_redirected_backing() {
    let placeholder = Window::placeholder(ResourceId(0x100_0042));
    assert!(placeholder.redirected_backing.is_none());
}

#[test]
fn resolved_background_follows_parent_relative_links() {
    let mut table = ResourceTable::new();
    make_window(&mut table, 0x200001);
    make_child(&mut table, 0x200002, 0x200001, 0, 0);

    {
        let parent = table.windows.get_mut(&0x200001).unwrap();
        parent.background_pixel = 0x00aa_bbcc;
        parent.background_pixmap = None;
        parent.background_pixmap_host_xid = None;
        // Direct struct poke must mirror what a real CWA
        // (background-pixel set) does to the None flag.
        parent.background_none = false;
    }
    {
        let child = table.windows.get_mut(&0x200002).unwrap();
        child.background_pixmap = Some(ResourceId(1));
        child.background_pixmap_host_xid = None;
    }

    let resolved = table
        .window_resolved_background(ResourceId(0x200002))
        .expect("resolved background");
    assert_eq!(resolved.background_pixel, 0x00aa_bbcc);
    assert_eq!(resolved.background_pixmap_host_xid, None);

    let parent_host = crate::backend::PixmapHandle::from_raw_for_test(0xdead_beef);
    {
        let parent = table.windows.get_mut(&0x200001).unwrap();
        parent.background_pixmap = Some(ResourceId(0x200010));
        parent.background_pixmap_host_xid = Some(parent_host);
    }
    let resolved = table
        .window_resolved_background(ResourceId(0x200002))
        .expect("resolved background");
    assert_eq!(resolved.background_pixel, 0x00aa_bbcc);
    assert_eq!(resolved.background_pixmap_host_xid, Some(parent_host));
}

/// #133 — a ParentRelative child's tile origin is the distance
/// between the two windows' CONTENT origins, so each level of the
/// walk contributes `x + border_width` (Xorg computes the same
/// difference as `pWin->drawable.x - drawable->x`,
/// `mi/miexpose.c:424-431`). Omitting `bw` misaligns the inherited
/// tile by the child's border width.
#[test]
fn parent_relative_tile_origin_includes_the_border_width() {
    let mut table = ResourceTable::new();
    make_window(&mut table, 0x300001);
    make_child(&mut table, 0x300002, 0x300001, 20, 10);
    let parent_host = crate::backend::PixmapHandle::from_raw_for_test(0x1234);
    {
        let parent = table.windows.get_mut(&0x300001).unwrap();
        parent.background_pixmap = Some(ResourceId(0x300010));
        parent.background_pixmap_host_xid = Some(parent_host);
        parent.background_none = false;
    }
    {
        let child = table.windows.get_mut(&0x300002).unwrap();
        // ParentRelative is background_pixmap == 1.
        child.background_pixmap = Some(ResourceId(1));
        child.background_pixmap_host_xid = None;
        child.border_width = 2;
    }
    let resolved = table
        .window_resolved_background(ResourceId(0x300002))
        .expect("resolved background");
    assert_eq!(resolved.background_pixmap_host_xid, Some(parent_host));
    assert_eq!(
        resolved.tile_origin_offset,
        (22, 12),
        "the tile origin is the child's CONTENT origin in the parent's \
             content space: x + bw, y + bw",
    );

    // bw == 0 keeps the pre-#133 value.
    {
        let child = table.windows.get_mut(&0x300002).unwrap();
        child.border_width = 0;
    }
    let resolved = table
        .window_resolved_background(ResourceId(0x300002))
        .expect("resolved background");
    assert_eq!(resolved.tile_origin_offset, (20, 10));
}

#[test]
fn cw_fields_persist_through_create_and_change() {
    // Bucket 3: validate that the new Window struct fields round-trip
    // through CreateWindow + ChangeWindowAttributes + GetWindowAttributes.
    let mut table = ResourceTable::new();
    let win = ResourceId(0xa00);
    table.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: win,
            parent: ROOT_WINDOW,
            width: 100,
            height: 100,
            class: 1,
            visual: ROOT_VISUAL,
            bit_gravity: Some(7),
            win_gravity: Some(8),
            backing_store: Some(2),
            backing_planes: Some(0xdead_beef),
            backing_pixel: Some(0x1234_5678),
            save_under: Some(true),
            do_not_propagate_mask: Some(0x0040), // PointerMotion
            ..Default::default()
        },
    );
    let w = table.window(win).expect("window");
    assert_eq!(w.bit_gravity, 7);
    assert_eq!(w.win_gravity, 8);
    assert_eq!(w.backing_store, 2);
    assert_eq!(w.backing_planes, 0xdead_beef);
    assert_eq!(w.backing_pixel, 0x1234_5678);
    assert!(w.save_under);
    assert_eq!(w.do_not_propagate_mask, 0x0040);
    // Default colormap inherits from parent (root → ROOT_COLORMAP).
    assert_eq!(w.colormap, ROOT_COLORMAP);

    // Mutate via ChangeWindowAttributes.
    let _ = table.change_window_attributes(ChangeWindowAttributesRequest {
        window: win,
        bit_gravity: Some(3),
        backing_store: Some(1),
        save_under: Some(false),
        do_not_propagate_mask: Some(0x0004), // ButtonPress
        ..Default::default()
    });
    let w = table.window(win).expect("window");
    assert_eq!(w.bit_gravity, 3);
    assert_eq!(w.win_gravity, 8); // untouched
    assert_eq!(w.backing_store, 1);
    assert!(!w.save_under);
    assert_eq!(w.do_not_propagate_mask, 0x0004);
}

#[test]
fn unmap_window_returns_true_on_transition_from_viewable() {
    let mut table = ResourceTable::new();
    make_window(&mut table, 0x100002);
    let _ = table.map_window(ResourceId(0x100002));
    assert_eq!(
        table.window(ResourceId(0x100002)).unwrap().map_state,
        MapState::Viewable
    );
    let was_mapped = table.unmap_window(ResourceId(0x100002)).mapping_changed;
    assert!(was_mapped);
    assert_eq!(
        table.window(ResourceId(0x100002)).unwrap().map_state,
        MapState::Unmapped
    );
}

#[test]
fn unmap_window_returns_true_on_transition_from_unviewable() {
    let mut table = ResourceTable::new();
    make_window(&mut table, 0x100002);
    // Force Unviewable directly — no public setter, but the field is pub.
    table.windows.get_mut(&0x100002).unwrap().map_state = MapState::Unviewable;
    let was_mapped = table.unmap_window(ResourceId(0x100002)).mapping_changed;
    assert!(was_mapped);
    assert_eq!(
        table.window(ResourceId(0x100002)).unwrap().map_state,
        MapState::Unmapped
    );
}

#[test]
fn unmap_window_returns_false_when_already_unmapped() {
    let mut table = ResourceTable::new();
    make_window(&mut table, 0x100002);
    // create_window leaves new windows Unmapped.
    assert_eq!(
        table.window(ResourceId(0x100002)).unwrap().map_state,
        MapState::Unmapped
    );
    let first = table.unmap_window(ResourceId(0x100002)).mapping_changed;
    assert!(!first);
    let second = table.unmap_window(ResourceId(0x100002)).mapping_changed;
    assert!(!second);
}

#[test]
fn unmap_window_returns_false_for_unknown_window() {
    let mut table = ResourceTable::new();
    let was_mapped = table.unmap_window(ResourceId(0x9999_9999)).mapping_changed;
    assert!(!was_mapped);
}

#[test]
fn unmap_window_no_ops_on_root() {
    let mut table = ResourceTable::new();
    assert_eq!(
        table.window(ROOT_WINDOW).unwrap().map_state,
        MapState::Viewable
    );
    let was_mapped = table.unmap_window(ROOT_WINDOW).mapping_changed;
    assert!(!was_mapped);
    assert_eq!(
        table.window(ROOT_WINDOW).unwrap().map_state,
        MapState::Viewable
    );
}

#[test]
fn host_drawable_target_redirected_window_returns_backing_xid() {
    // L2 plan B.7: a window with `redirected_backing` set routes
    // paint to the backing's host XID, carrying the backing's
    // depth. The `nested` ResourceId stays the window itself so
    // damage and event delivery keep the public XID.
    let mut table = ResourceTable::new();
    make_top_level_with_host_xid(&mut table, 0x0010_0002, 0x42);
    let backing = crate::backend::PixmapHandle::from_raw_for_test(0x9999);
    table
        .windows
        .get_mut(&0x0010_0002)
        .unwrap()
        .redirected_backing = Some(crate::resources::RedirectedBacking {
        host_pixmap: backing,
        width: 100,
        height: 50,
        depth: 24,
    });
    match table.host_drawable_target(ResourceId(0x0010_0002)).unwrap() {
        HostDrawableTarget::Window {
            nested,
            host_xid,
            depth,
        } => {
            assert_eq!(nested, ResourceId(0x0010_0002));
            assert_eq!(host_xid.as_raw(), 0x9999);
            assert_eq!(depth, 24);
        }
        _ => panic!("expected Window variant routing to backing"),
    }
}

#[test]
fn host_drawable_target_top_level_window_with_host_xid() {
    let mut table = ResourceTable::new();
    make_top_level_with_host_xid(&mut table, 0x0010_0002, 0xAA);
    let target = table.host_drawable_target(ResourceId(0x0010_0002));
    assert_eq!(
        target,
        Some(HostDrawableTarget::Window {
            nested: ResourceId(0x0010_0002),
            host_xid: crate::backend::WindowHandle::from_raw_for_test(0xAA),
            depth: 24,
        })
    );
}

#[test]
fn host_drawable_target_child_window_targets_own_host_xid() {
    // Phase 3.6 Step 6: every InputOutput window has its own host_xid;
    // a child without host_xid set yields None (drop) rather than
    // walking up to the parent.
    let mut table = ResourceTable::new();
    make_top_level_with_host_xid(&mut table, 0x0010_0002, 0xBB);
    make_child(&mut table, 0x0010_0003, 0x0010_0002, 10, 20);
    if let Some(child) = table.window_mut(ResourceId(0x0010_0003)) {
        child.host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(0xCC));
    }
    let target = table.host_drawable_target(ResourceId(0x0010_0003));
    assert_eq!(
        target,
        Some(HostDrawableTarget::Window {
            nested: ResourceId(0x0010_0003),
            host_xid: crate::backend::WindowHandle::from_raw_for_test(0xCC),
            depth: 24,
        })
    );
}

#[test]
fn host_drawable_target_window_without_host_xid_returns_none() {
    let mut table = ResourceTable::new();
    make_top_level_with_host_xid(&mut table, 0x0010_0002, 0xBB);
    make_child(&mut table, 0x0010_0003, 0x0010_0002, 10, 20);
    // child has no host_xid; drawing on it drops silently.
    assert_eq!(table.host_drawable_target(ResourceId(0x0010_0003)), None);
}

#[test]
fn host_drawable_target_pixmap_with_host_xid() {
    let mut table = ResourceTable::new();
    let request = CreatePixmapRequest {
        pixmap: ResourceId(0x0020_0002),
        drawable: ROOT_WINDOW,
        width: 256,
        height: 256,
        depth: 32,
    };
    table.create_pixmap(ClientId(1), request);
    assert!(table.set_pixmap_host_xid(
        ResourceId(0x0020_0002),
        crate::backend::PixmapHandle::from_raw_for_test(0xDEAD_BEEF),
    ));
    let target = table.host_drawable_target(ResourceId(0x0020_0002));
    assert_eq!(
        target,
        Some(HostDrawableTarget::Pixmap {
            nested: ResourceId(0x0020_0002),
            host_xid: crate::backend::PixmapHandle::from_raw_for_test(0xDEAD_BEEF),
            width: 256,
            height: 256,
            depth: 32,
        })
    );
}

#[test]
fn host_drawable_target_pixmap_without_host_xid_returns_none() {
    let mut table = ResourceTable::new();
    let request = CreatePixmapRequest {
        pixmap: ResourceId(0x0020_0002),
        drawable: ROOT_WINDOW,
        width: 128,
        height: 128,
        depth: 24,
    };
    table.create_pixmap(ClientId(1), request);
    // host_xid is None by default
    let target = table.host_drawable_target(ResourceId(0x0020_0002));
    assert_eq!(target, None);
}

#[test]
fn host_drawable_target_unknown_drawable_returns_none() {
    let table = ResourceTable::new();
    let target = table.host_drawable_target(ResourceId(0x9999_9999));
    assert_eq!(target, None);
}

#[test]
fn host_drawable_target_window_depth_matches_window_depth() {
    let mut table = ResourceTable::new();
    make_top_level_with_host_xid(&mut table, 0x0010_0002, 0x1234);
    // Set depth to 32
    table.windows.get_mut(&0x0010_0002).unwrap().depth = 32;
    let target = table.host_drawable_target(ResourceId(0x0010_0002));
    if let Some(HostDrawableTarget::Window { depth, .. }) = target {
        assert_eq!(depth, 32);
    } else {
        panic!("Expected Window variant with depth 32");
    }
}

// Viewability-delta fixture: F{ A{ A1, A2{ A2a } }, B{ B1 }, C }, a
// top-level G, all created unmapped. Viewable iff it and every ancestor
// is mapped (dix/window.c MapWindow / UnmapWindow).
const VD_F: u32 = 0x0010_0100;
const VD_A: u32 = 0x0010_0101;
const VD_A1: u32 = 0x0010_0102;
const VD_A2: u32 = 0x0010_0103;
const VD_A2A: u32 = 0x0010_0104;
const VD_B: u32 = 0x0010_0105;
const VD_B1: u32 = 0x0010_0106;
const VD_C: u32 = 0x0010_0107;
const VD_G: u32 = 0x0010_0108;

fn ids(raw: &[u32]) -> Vec<ResourceId> {
    raw.iter().copied().map(ResourceId).collect()
}

/// Builds the fixture with A, A1, A2a, B1 and C mapped (all Unviewable,
/// F still unmapped); A2 and B stay unmapped.
fn viewability_fixture() -> ResourceTable {
    let mut t = ResourceTable::new();
    make_window(&mut t, VD_F);
    make_window(&mut t, VD_G);
    make_child(&mut t, VD_A, VD_F, 0, 0);
    make_child(&mut t, VD_A1, VD_A, 0, 0);
    make_child(&mut t, VD_A2, VD_A, 0, 0);
    make_child(&mut t, VD_A2A, VD_A2, 0, 0);
    make_child(&mut t, VD_B, VD_F, 0, 0);
    make_child(&mut t, VD_B1, VD_B, 0, 0);
    make_child(&mut t, VD_C, VD_F, 0, 0);
    for w in [VD_A, VD_A1, VD_A2A, VD_B1, VD_C] {
        let tr = t.map_window(ResourceId(w));
        assert!(tr.mapping_changed);
        assert!(tr.delta.is_empty(), "0x{w:x} maps under an unmapped F");
    }
    t
}

#[test]
fn map_window_delta_is_newly_viewable_subtree_in_pre_order() {
    let mut t = viewability_fixture();
    let tr = t.map_window(ResourceId(VD_F));
    assert!(tr.mapping_changed);
    assert_eq!(tr.delta.became_viewable, ids(&[VD_F, VD_A, VD_A1, VD_C]));
    assert!(tr.delta.became_unviewable.is_empty());

    // Mapping A2 now exposes A2a beneath it: parent before child.
    let tr = t.map_window(ResourceId(VD_A2));
    assert!(tr.mapping_changed);
    assert_eq!(tr.delta.became_viewable, ids(&[VD_A2, VD_A2A]));
    assert!(tr.delta.became_unviewable.is_empty());
}

#[test]
fn unmap_window_delta_is_viewable_subtree_in_post_order() {
    let mut t = viewability_fixture();
    let _ = t.map_window(ResourceId(VD_F));
    let _ = t.map_window(ResourceId(VD_A2));

    let tr = t.unmap_window(ResourceId(VD_A));
    assert!(tr.mapping_changed);
    assert!(tr.delta.became_viewable.is_empty());
    assert_eq!(
        tr.delta.became_unviewable,
        ids(&[VD_A1, VD_A2A, VD_A2, VD_A])
    );

    let tr = t.unmap_window(ResourceId(VD_F));
    assert!(tr.mapping_changed);
    assert_eq!(tr.delta.became_unviewable, ids(&[VD_C, VD_F]));
}

#[test]
fn remap_of_viewable_window_has_no_transition() {
    let mut t = viewability_fixture();
    let _ = t.map_window(ResourceId(VD_F));
    for w in [VD_F, VD_A, VD_C] {
        assert_eq!(t.map_window(ResourceId(w)), MapTransition::default());
    }
}

#[test]
fn mapping_changes_without_viewability_under_unmapped_ancestor() {
    let mut t = viewability_fixture();
    // Map under an unmapped parent: Unmapped -> Unviewable.
    let tr = t.map_window(ResourceId(VD_A2));
    assert!(tr.mapping_changed);
    assert!(tr.delta.is_empty());
    assert_eq!(
        t.window(ResourceId(VD_A2)).unwrap().map_state,
        MapState::Unviewable
    );
    // Unmap of an unviewable window: Unviewable -> Unmapped.
    let tr = t.unmap_window(ResourceId(VD_A));
    assert!(tr.mapping_changed);
    assert!(tr.delta.is_empty());
    assert_eq!(
        t.window(ResourceId(VD_A)).unwrap().map_state,
        MapState::Unmapped
    );
}

#[test]
fn reparent_window_delta_follows_new_parent_viewability() {
    let mut t = viewability_fixture();
    let _ = t.map_window(ResourceId(VD_F));
    let _ = t.map_window(ResourceId(VD_A2));
    let reparent = |t: &mut ResourceTable, window: u32, parent: u32| {
        t.reparent_window(ReparentWindowRequest {
            window: ResourceId(window),
            parent: ResourceId(parent),
            x: 0,
            y: 0,
        })
        .unwrap()
        .delta
    };

    // Viewable A under unmapped G: the subtree leaves, child first.
    let delta = reparent(&mut t, VD_A, VD_G);
    assert!(delta.became_viewable.is_empty());
    assert_eq!(delta.became_unviewable, ids(&[VD_A1, VD_A2A, VD_A2, VD_A]));

    // Back under viewable F: the subtree returns, parent first.
    let delta = reparent(&mut t, VD_A, VD_F);
    assert_eq!(delta.became_viewable, ids(&[VD_A, VD_A1, VD_A2, VD_A2A]));
    assert!(delta.became_unviewable.is_empty());

    // Viewable -> viewable parent and an unmapped window: no change.
    assert!(reparent(&mut t, VD_C, VD_A).is_empty());
    assert!(reparent(&mut t, VD_B, VD_G).is_empty());
    assert!(reparent(&mut t, VD_B, VD_F).is_empty());

    // B was reparented while unmapped; mapping it now exposes B1.
    let tr = t.map_window(ResourceId(VD_B));
    assert_eq!(tr.delta.became_viewable, ids(&[VD_B, VD_B1]));
}

proptest! {
    #[test]
    fn unmap_window_state_machine(
        initial in arb_initial(),
        n in 1usize..=5,
    ) {
        let mut table = ResourceTable::new();
        make_window(&mut table, 0x100002);
        let target = ResourceId(0x100002);
        let initial_map_state = match initial {
            InitialState::Viewable => MapState::Viewable,
            InitialState::Unviewable => MapState::Unviewable,
            InitialState::Unmapped => MapState::Unmapped,
        };
        table.windows.get_mut(&target.0).unwrap().map_state = initial_map_state;

        let mut results = Vec::with_capacity(n);
        for _ in 0..n {
            results.push(table.unmap_window(target).mapping_changed);
        }

        let expected_first = !matches!(initial, InitialState::Unmapped);
        prop_assert_eq!(results[0], expected_first);
        for r in results.iter().skip(1) {
            prop_assert!(!*r, "subsequent calls must return false");
        }
        prop_assert_eq!(
            table.window(target).unwrap().map_state,
            MapState::Unmapped
        );
    }

}

#[test]
fn copy_from_parent_visual_inherits_argb_parent() {
    let mut t = ResourceTable::new();
    // Create an ARGB top-level then a CopyFromParent child of it.
    t.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 32,
            window: ResourceId(0x200),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 50,
            height: 50,
            border_width: 0,
            class: 1,
            visual: ARGB_VISUAL,
            ..Default::default()
        },
    );
    t.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 0,
            window: ResourceId(0x300),
            parent: ResourceId(0x200),
            x: 0,
            y: 0,
            width: 25,
            height: 25,
            border_width: 0,
            class: 1,
            visual: ResourceId(0), // CopyFromParent
            ..Default::default()
        },
    );
    let child = t.window(ResourceId(0x300)).expect("child created");
    assert_eq!(child.visual, ARGB_VISUAL);
    assert_eq!(child.depth, 32);
}

#[test]
fn newly_created_window_inherits_parent_border() {
    let mut t = ResourceTable::new();
    t.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 0,
            window: ResourceId(0x200),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 50,
            height: 50,
            border_width: 1,
            class: 1,
            visual: ResourceId(0),
            ..Default::default()
        },
    );
    // Xorg `dix/window.c:879` — a new window inherits the parent's
    // border, it does not default to a fresh `Pixel(0)`. For a root
    // child that inheritance happens to yield the root's own
    // `Pixel(0)`, so assert against the parent rather than the
    // literal to keep the test about inheritance.
    let parent_border = t.window(ROOT_WINDOW).unwrap().border;
    assert_eq!(t.window(ResourceId(0x200)).unwrap().border, parent_border);
}

/// Inheritance carries a PIXMAP border too, not just a pixel — Xorg
/// bumps the pixmap's refcount rather than falling back to a pixel
/// (`dix/window.c:881`). The root-child case above cannot see this
/// because the root's border is always a pixel.
#[test]
fn newly_created_window_inherits_a_pixmap_border_from_its_parent() {
    let mut t = ResourceTable::new();
    t.create_pixmap(
        ClientId(1),
        CreatePixmapRequest {
            pixmap: ResourceId(0x300),
            drawable: ROOT_WINDOW,
            width: 8,
            height: 8,
            depth: 24,
        },
    );
    assert!(t.set_pixmap_host_xid(
        ResourceId(0x300),
        crate::backend::PixmapHandle::from_raw(0xabc).unwrap()
    ));
    // Parent takes the pixmap border explicitly.
    t.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(0x200),
            parent: ROOT_WINDOW,
            width: 50,
            height: 50,
            class: 1,
            border_pixmap: Some(ResourceId(0x300)),
            ..Default::default()
        },
    );
    let parent_border = t.window(ResourceId(0x200)).unwrap().border;
    assert!(
        matches!(parent_border, BorderSource::Pixmap { id, .. } if id == ResourceId(0x300)),
        "parent should hold the pixmap border, got {parent_border:?}"
    );
    // Child supplies no border attribute at all → inherits it.
    t.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(0x201),
            parent: ResourceId(0x200),
            width: 20,
            height: 20,
            class: 1,
            ..Default::default()
        },
    );
    assert_eq!(t.window(ResourceId(0x201)).unwrap().border, parent_border);
}

/// An EXPLICIT `CWBorderPixmap = CopyFromParent` — the mask bit SET
/// with value 0 — is a different arm from omitting the attribute:
/// the request carries `Some(ResourceId(0))`, not `None`. Both have
/// to land on the parent's border. Only the depth-mismatch FAILURE
/// of the explicit form was pinned; this is its success path, plus
/// the ChangeWindowAttributes twin, which X11 also allows to name
/// CopyFromParent.
#[test]
fn an_explicit_copy_from_parent_border_resolves_to_the_parents_border() {
    let mut t = ResourceTable::new();
    t.create_pixmap(
        ClientId(1),
        CreatePixmapRequest {
            pixmap: ResourceId(0x300),
            drawable: ROOT_WINDOW,
            width: 8,
            height: 8,
            depth: 24,
        },
    );
    assert!(t.set_pixmap_host_xid(
        ResourceId(0x300),
        crate::backend::PixmapHandle::from_raw(0xabc).unwrap()
    ));
    t.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(0x200),
            parent: ROOT_WINDOW,
            width: 50,
            height: 50,
            class: 1,
            border_pixmap: Some(ResourceId(0x300)),
            ..Default::default()
        },
    );
    let parent_border = t.window(ResourceId(0x200)).unwrap().border;
    assert!(
        matches!(parent_border, BorderSource::Pixmap { id, .. } if id == ResourceId(0x300)),
        "parent must hold the pixmap border, got {parent_border:?}"
    );

    // CreateWindow, attribute PRESENT and zero.
    t.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(0x201),
            parent: ResourceId(0x200),
            width: 20,
            height: 20,
            class: 1,
            border_pixmap: Some(ResourceId(0)),
            ..Default::default()
        },
    );
    assert_eq!(
        t.window(ResourceId(0x201)).unwrap().border,
        parent_border,
        "explicit CopyFromParent on CreateWindow must resolve to the parent",
    );

    // A sibling that starts with a PIXEL border, so the change below
    // has somewhere to move from and cannot pass by already matching.
    t.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(0x202),
            parent: ResourceId(0x200),
            width: 20,
            height: 20,
            class: 1,
            border_pixel: Some(0x00ff_0000),
            ..Default::default()
        },
    );
    assert_eq!(
        t.window(ResourceId(0x202)).unwrap().border,
        BorderSource::Pixel(0x00ff_0000),
        "the sibling must start on a pixel border",
    );
    t.change_window_attributes(ChangeWindowAttributesRequest {
        window: ResourceId(0x202),
        value_mask: 0x0004,
        border_pixmap: Some(ResourceId(0)),
        ..Default::default()
    });
    assert_eq!(
        t.window(ResourceId(0x202)).unwrap().border,
        parent_border,
        "explicit CopyFromParent on ChangeWindowAttributes must resolve to \
             the parent",
    );
}

/// Replacing a pixmap border hands the old host handle back so the
/// caller can free it once orphaned (`dix/window.c:1290`
/// DestroyPixmap). Re-installing the SAME source releases nothing —
/// otherwise the handle still in use would be freed.
#[test]
fn replacing_a_pixmap_border_releases_the_old_host_handle() {
    let mut t = ResourceTable::new();
    let handle_a = crate::backend::PixmapHandle::from_raw(0xaaa).unwrap();
    let handle_b = crate::backend::PixmapHandle::from_raw(0xbbb).unwrap();
    for (id, handle) in [(0x300u32, handle_a), (0x301, handle_b)] {
        t.create_pixmap(
            ClientId(1),
            CreatePixmapRequest {
                pixmap: ResourceId(id),
                drawable: ROOT_WINDOW,
                width: 8,
                height: 8,
                depth: 24,
            },
        );
        assert!(t.set_pixmap_host_xid(ResourceId(id), handle));
    }
    t.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(0x200),
            parent: ROOT_WINDOW,
            width: 50,
            height: 50,
            class: 1,
            border_pixmap: Some(ResourceId(0x300)),
            ..Default::default()
        },
    );

    // Re-installing the same pixmap releases nothing.
    let released = t.change_window_attributes(ChangeWindowAttributesRequest {
        window: ResourceId(0x200),
        value_mask: 0x0004,
        border_pixmap: Some(ResourceId(0x300)),
        ..Default::default()
    });
    assert_eq!(released.border, None, "re-install must not release");

    // Swapping to a different pixmap releases the first.
    let released = t.change_window_attributes(ChangeWindowAttributesRequest {
        window: ResourceId(0x200),
        value_mask: 0x0004,
        border_pixmap: Some(ResourceId(0x301)),
        ..Default::default()
    });
    assert_eq!(released.border, Some(handle_a));

    // And a pixel border releases the pixmap it displaced.
    let released = t.change_window_attributes(ChangeWindowAttributesRequest {
        window: ResourceId(0x200),
        value_mask: 0x0008,
        border_pixel: Some(0x00ff_0000),
        ..Default::default()
    });
    assert_eq!(released.border, Some(handle_b));
    assert_eq!(
        t.window(ResourceId(0x200)).unwrap().border,
        BorderSource::Pixel(0x00ff_0000)
    );
}

#[test]
fn copy_from_parent_visual_inherits_root_visual_for_root_child() {
    let mut t = ResourceTable::new();
    t.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 0,
            window: ResourceId(0x200),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 50,
            height: 50,
            border_width: 0,
            class: 1,
            visual: ResourceId(0),
            ..Default::default()
        },
    );
    let w = t.window(ResourceId(0x200)).expect("created");
    assert_eq!(w.visual, ROOT_VISUAL);
    assert_eq!(w.depth, 24);
}

#[test]
fn cow_aware_top_index_with_no_cow_returns_end() {
    let mut t = ResourceTable::new();
    make_child(&mut t, 0x200, ROOT_WINDOW.0, 0, 0);
    make_child(&mut t, 0x300, ROOT_WINDOW.0, 0, 0);
    let root = t.window(ROOT_WINDOW).unwrap();
    assert_eq!(cow_aware_top_index(root), root.children.len());
}

#[test]
fn cow_aware_top_index_with_cow_at_top_returns_just_below() {
    let mut t = ResourceTable::new();
    make_child(&mut t, 0x200, ROOT_WINDOW.0, 0, 0);
    // Simulate the post-Phase-2 state where COW is the last (topmost) root child.
    // For this isolated unit test, push it directly so we can exercise the helper.
    t.windows
        .get_mut(&ROOT_WINDOW.0)
        .unwrap()
        .children
        .push(COMPOSITE_OVERLAY_WINDOW);
    let root = t.window(ROOT_WINDOW).unwrap();
    assert_eq!(cow_aware_top_index(root), root.children.len() - 1);
}

#[test]
fn create_window_with_cow_present_inserts_below_cow() {
    let mut t = ResourceTable::new();
    t.windows
        .get_mut(&ROOT_WINDOW.0)
        .unwrap()
        .children
        .push(COMPOSITE_OVERLAY_WINDOW);
    make_child(&mut t, 0x500, ROOT_WINDOW.0, 0, 0);
    let kids = &t.window(ROOT_WINDOW).unwrap().children;
    assert_eq!(
        kids.last().copied(),
        Some(COMPOSITE_OVERLAY_WINDOW),
        "COW must stay at top after create_window"
    );
    assert_eq!(
        kids[kids.len() - 2],
        ResourceId(0x500),
        "new top-level lands just below COW"
    );
}

#[test]
fn restack_top_with_cow_present_lands_just_below_cow() {
    let mut t = ResourceTable::new();
    make_child(&mut t, 0x200, ROOT_WINDOW.0, 0, 0);
    make_child(&mut t, 0x300, ROOT_WINDOW.0, 0, 0);
    t.windows
        .get_mut(&ROOT_WINDOW.0)
        .unwrap()
        .children
        .push(COMPOSITE_OVERLAY_WINDOW);
    // children now: [0x200, 0x300, COW]
    let _ = t.configure_window(ConfigureWindowRequest {
        window: ResourceId(0x200),
        value_mask: 1 << 6,
        x: None,
        y: None,
        width: None,
        height: None,
        border_width: None,
        sibling: None,
        stack_mode: Some(0), // 0 = Above (with no sibling = top)
    });
    let kids = &t.window(ROOT_WINDOW).unwrap().children;
    assert_eq!(
        kids,
        &[
            ResourceId(0x300),
            ResourceId(0x200),
            COMPOSITE_OVERLAY_WINDOW
        ]
    );
}

#[test]
fn restack_above_cow_caps_to_just_below_cow() {
    let mut t = ResourceTable::new();
    make_child(&mut t, 0x200, ROOT_WINDOW.0, 0, 0);
    make_child(&mut t, 0x300, ROOT_WINDOW.0, 0, 0);
    t.windows
        .get_mut(&ROOT_WINDOW.0)
        .unwrap()
        .children
        .push(COMPOSITE_OVERLAY_WINDOW);
    // children: [0x200, 0x300, COW]; ask for 0x200 above COW.
    let _ = t.configure_window(ConfigureWindowRequest {
        window: ResourceId(0x200),
        value_mask: 0,
        x: None,
        y: None,
        width: None,
        height: None,
        border_width: None,
        sibling: Some(COMPOSITE_OVERLAY_WINDOW),
        stack_mode: Some(0), // Above
    });
    let kids = &t.window(ROOT_WINDOW).unwrap().children;
    assert_eq!(
        kids.last().copied(),
        Some(COMPOSITE_OVERLAY_WINDOW),
        "COW must remain at top after AboveSibling=COW"
    );
    assert_eq!(
        kids[kids.len() - 2],
        ResourceId(0x200),
        "0x200 must land just below COW (capped)"
    );
}

#[test]
fn reparent_to_root_with_cow_present_lands_below_cow() {
    let mut t = ResourceTable::new();
    // Build: root → container; container → child.
    make_child(&mut t, 0xc0, ROOT_WINDOW.0, 0, 0);
    make_child(&mut t, 0xd0, 0xc0, 0, 0);
    // Put COW at top of root.
    t.windows
        .get_mut(&ROOT_WINDOW.0)
        .unwrap()
        .children
        .push(COMPOSITE_OVERLAY_WINDOW);
    // Reparent the inner child up to root.
    let _ = t.reparent_window(ReparentWindowRequest {
        window: ResourceId(0xd0),
        parent: ROOT_WINDOW,
        x: 0,
        y: 0,
    });
    let kids = &t.window(ROOT_WINDOW).unwrap().children;
    assert_eq!(
        kids.last().copied(),
        Some(COMPOSITE_OVERLAY_WINDOW),
        "COW must remain topmost after reparent-to-root"
    );
    assert!(
        kids.contains(&ResourceId(0xd0)),
        "the reparented child must appear in root.children"
    );
    assert_ne!(
        kids.last().copied(),
        Some(ResourceId(0xd0)),
        "the reparented child must NOT be above COW"
    );
}

#[test]
fn fresh_resources_does_not_contain_cow() {
    let t = ResourceTable::new();
    assert!(
        t.window(COMPOSITE_OVERLAY_WINDOW).is_none(),
        "COW must NOT be pre-seeded; it materializes only on GetOverlayWindow"
    );
    assert!(
        !t.window(ROOT_WINDOW)
            .unwrap()
            .children
            .contains(&COMPOSITE_OVERLAY_WINDOW),
        "fresh root.children must not contain COW"
    );
}

#[test]
fn materialize_cow_resource_creates_record_and_inserts_at_top() {
    let mut t = ResourceTable::new();
    make_child(&mut t, 0x200, ROOT_WINDOW.0, 0, 0);
    let host_xid = crate::backend::WindowHandle::from_raw_panicking(COMPOSITE_OVERLAY_WINDOW.0);
    t.materialize_cow_resource(host_xid);

    let cow = t
        .window(COMPOSITE_OVERLAY_WINDOW)
        .expect("COW resource exists after materialize");
    assert_eq!(cow.host_xid, Some(host_xid));
    assert_eq!(cow.parent, ROOT_WINDOW);
    assert!(cow.override_redirect);
    assert_eq!(cow.depth, 24);
    assert_eq!(cow.class, WindowClass::InputOutput);
    assert_eq!(cow.map_state, MapState::Viewable);

    let root = t.window(ROOT_WINDOW).unwrap();
    // COW geometry matches root extent.
    assert_eq!(cow.width, root.width, "COW width mirrors root");
    assert_eq!(cow.height, root.height, "COW height mirrors root");

    let kids = &root.children;
    assert_eq!(
        kids.last().copied(),
        Some(COMPOSITE_OVERLAY_WINDOW),
        "COW lands at the top of root.children",
    );
}

#[test]
#[should_panic(expected = "COW already materialized")]
fn materialize_cow_resource_panics_if_already_materialized() {
    let mut t = ResourceTable::new();
    let host_xid = crate::backend::WindowHandle::from_raw_panicking(COMPOSITE_OVERLAY_WINDOW.0);
    t.materialize_cow_resource(host_xid);
    // Second call without intervening destroy must panic — repeated
    // GetOverlayWindow calls without ReleaseOverlayWindow are
    // refcount-only and must not reach this function.
    t.materialize_cow_resource(host_xid);
}

#[test]
fn destroy_cow_resource_removes_record_and_root_child() {
    let mut t = ResourceTable::new();
    let host_xid = crate::backend::WindowHandle::from_raw_panicking(COMPOSITE_OVERLAY_WINDOW.0);
    t.materialize_cow_resource(host_xid);
    t.destroy_cow_resource();
    assert!(t.window(COMPOSITE_OVERLAY_WINDOW).is_none());
    assert!(
        !t.window(ROOT_WINDOW)
            .unwrap()
            .children
            .contains(&COMPOSITE_OVERLAY_WINDOW)
    );
}

#[test]
fn reparent_cow_under_its_own_descendant_is_bad_match() {
    // Materialize COW (child of root), then create a child C under COW.
    let mut t = ResourceTable::new();
    let host_xid = crate::backend::WindowHandle::from_raw_panicking(0x4000_0103);
    t.materialize_cow_resource(host_xid);
    make_child(&mut t, 0xC0, COMPOSITE_OVERLAY_WINDOW.0, 0, 0);

    // ReparentWindow(window=COW, parent=C) would make COW a descendant
    // of itself -> BadMatch via the GENERIC cycle rule (no COW-special
    // code). is_descendant_of(parent=C, window=COW) is true because C's
    // parent chain reaches COW.
    let err = t.reparent_window(ReparentWindowRequest {
        window: COMPOSITE_OVERLAY_WINDOW,
        parent: ResourceId(0xC0),
        x: 0,
        y: 0,
    });
    assert!(
        matches!(err, Err(ReparentWindowError::BadMatch)),
        "reparenting COW under its own child must be BadMatch (generic cycle rule)"
    );
}

#[test]
fn reparent_cow_to_a_normal_toplevel_succeeds_xorg_faithful() {
    // Xorg does NOT reject moving the COW to an unrelated window. yserver
    // must match: a reparent to a non-descendant, non-root window succeeds.
    let mut t = ResourceTable::new();
    make_child(&mut t, 0x200, ROOT_WINDOW.0, 0, 0); // unrelated top-level W
    let host_xid = crate::backend::WindowHandle::from_raw_panicking(0x4000_0103);
    t.materialize_cow_resource(host_xid);

    let res = t.reparent_window(ReparentWindowRequest {
        window: COMPOSITE_OVERLAY_WINDOW,
        parent: ResourceId(0x200),
        x: 0,
        y: 0,
    });
    assert!(
        res.is_ok(),
        "reparenting COW to an unrelated window succeeds — matches Xorg \
             (compositing breaks, but that's the compositor's bug, not ours)"
    );
    assert_eq!(
        t.window(COMPOSITE_OVERLAY_WINDOW).unwrap().parent,
        ResourceId(0x200)
    );
}
