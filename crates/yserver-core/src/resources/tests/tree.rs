use super::*;

#[test]
fn is_descendant_of_handles_child_grandchild_and_unrelated() {
    let mut table = ResourceTable::new();
    make_window(&mut table, 0x0010_0002);
    make_child(&mut table, 0x0010_0003, 0x0010_0002, 0, 0);
    make_child(&mut table, 0x0010_0004, 0x0010_0003, 0, 0);
    make_window(&mut table, 0x0010_0005);

    assert!(table.is_descendant_of(ResourceId(0x0010_0003), ResourceId(0x0010_0002)));
    assert!(table.is_descendant_of(ResourceId(0x0010_0004), ResourceId(0x0010_0002)));
    assert!(!table.is_descendant_of(ResourceId(0x0010_0005), ResourceId(0x0010_0002)));
    assert!(!table.is_descendant_of(ResourceId(0xdead_beef), ResourceId(0x0010_0002)));
}

#[test]
fn reparent_cascades_viewability_to_descendants() {
    let mut table = ResourceTable::new();
    // Viewable chain: P (top-level) -> W -> C, all mapped.
    make_window(&mut table, 0x0010_0010);
    make_child(&mut table, 0x0010_0011, 0x0010_0010, 0, 0);
    make_child(&mut table, 0x0010_0012, 0x0010_0011, 0, 0);
    let _ = table.map_window(ResourceId(0x0010_0010));
    let _ = table.map_window(ResourceId(0x0010_0011));
    let _ = table.map_window(ResourceId(0x0010_0012));
    assert_eq!(
        table.window(ResourceId(0x0010_0011)).unwrap().map_state,
        MapState::Viewable
    );
    assert_eq!(
        table.window(ResourceId(0x0010_0012)).unwrap().map_state,
        MapState::Viewable
    );

    // Reparent W under an UNMAPPED parent Q: W and its child C must both
    // become Unviewable (still mapped, but no viewable ancestor).
    make_window(&mut table, 0x0010_0020); // Q, left unmapped
    table
        .reparent_window(ReparentWindowRequest {
            window: ResourceId(0x0010_0011),
            parent: ResourceId(0x0010_0020),
            x: 0,
            y: 0,
        })
        .unwrap();
    assert_eq!(
        table.window(ResourceId(0x0010_0011)).unwrap().map_state,
        MapState::Unviewable,
        "W under an unmapped parent is Unviewable"
    );
    assert_eq!(
        table.window(ResourceId(0x0010_0012)).unwrap().map_state,
        MapState::Unviewable,
        "descendant C cascades to Unviewable"
    );

    // Reparent W back under the viewable P: W and C promote to Viewable.
    table
        .reparent_window(ReparentWindowRequest {
            window: ResourceId(0x0010_0011),
            parent: ResourceId(0x0010_0010),
            x: 0,
            y: 0,
        })
        .unwrap();
    assert_eq!(
        table.window(ResourceId(0x0010_0011)).unwrap().map_state,
        MapState::Viewable,
        "W back under a viewable parent is Viewable"
    );
    assert_eq!(
        table.window(ResourceId(0x0010_0012)).unwrap().map_state,
        MapState::Viewable,
        "descendant C promoted back to Viewable"
    );
}

#[test]
fn mapped_children_bottom_to_top_filters_unmapped_and_preserves_order() {
    let mut table = ResourceTable::new();
    make_window(&mut table, 0x0010_0002);
    make_child(&mut table, 0x0010_0003, 0x0010_0002, 0, 0);
    make_child(&mut table, 0x0010_0004, 0x0010_0002, 0, 0);
    make_child(&mut table, 0x0010_0005, 0x0010_0002, 0, 0);
    let _ = table.map_window(ResourceId(0x0010_0003));
    let _ = table.map_window(ResourceId(0x0010_0005));

    assert_eq!(
        table.mapped_children_bottom_to_top(ResourceId(0x0010_0002)),
        Some(vec![ResourceId(0x0010_0003), ResourceId(0x0010_0005)])
    );
    assert_eq!(
        table.mapped_children_bottom_to_top(ResourceId(0xdead_beef)),
        None
    );
}

#[test]
fn reparent_window_moves_child_and_updates_position() {
    let mut table = ResourceTable::new();
    make_window(&mut table, 0x0010_0002);
    make_window(&mut table, 0x0010_0003);
    make_child(&mut table, 0x0010_0004, 0x0010_0002, 1, 2);

    let result = table
        .reparent_window(ReparentWindowRequest {
            window: ResourceId(0x0010_0004),
            parent: ResourceId(0x0010_0003),
            x: 10,
            y: 20,
        })
        .unwrap();

    assert_eq!(result.old_parent, ResourceId(0x0010_0002));
    assert_eq!(result.new_parent, ResourceId(0x0010_0003));
    assert!(
        !table
            .children(ResourceId(0x0010_0002))
            .contains(&ResourceId(0x0010_0004))
    );
    assert_eq!(
        table.children(ResourceId(0x0010_0003)),
        &[ResourceId(0x0010_0004)]
    );
    let window = table.window(ResourceId(0x0010_0004)).unwrap();
    assert_eq!(window.parent, ResourceId(0x0010_0003));
    assert_eq!((window.x, window.y), (10, 20));
}

#[test]
fn reparent_window_rejects_invalid_relationships() {
    let mut table = ResourceTable::new();
    make_window(&mut table, 0x0010_0002);
    make_child(&mut table, 0x0010_0003, 0x0010_0002, 0, 0);

    assert_eq!(
        table.reparent_window(ReparentWindowRequest {
            window: ROOT_WINDOW,
            parent: ResourceId(0x0010_0002),
            x: 0,
            y: 0,
        }),
        Err(ReparentWindowError::BadMatch)
    );
    assert_eq!(
        table.reparent_window(ReparentWindowRequest {
            window: ResourceId(0x0010_0002),
            parent: ResourceId(0x0010_0002),
            x: 0,
            y: 0,
        }),
        Err(ReparentWindowError::BadMatch)
    );
    assert_eq!(
        table.reparent_window(ReparentWindowRequest {
            window: ResourceId(0x0010_0002),
            parent: ResourceId(0x0010_0003),
            x: 0,
            y: 0,
        }),
        Err(ReparentWindowError::BadMatch)
    );
}

#[test]
fn reparent_window_rejects_unknown_windows() {
    let mut table = ResourceTable::new();
    make_window(&mut table, 0x0010_0002);

    assert_eq!(
        table.reparent_window(ReparentWindowRequest {
            window: ResourceId(0xdead_beef),
            parent: ResourceId(0x0010_0002),
            x: 0,
            y: 0,
        }),
        Err(ReparentWindowError::BadWindow)
    );
    assert_eq!(
        table.reparent_window(ReparentWindowRequest {
            window: ResourceId(0x0010_0002),
            parent: ResourceId(0xdead_beef),
            x: 0,
            y: 0,
        }),
        Err(ReparentWindowError::BadWindow)
    );
}

/// #133 step 8 (P9) fixtures. A root child at (100, 200), 300x400,
/// `border_width = 16` — awesome's configured width. Its CONTENT
/// origin is therefore (116, 216) in root coordinates
/// (`dix/window.c:888`: `drawable.x = parent->drawable.x + x + bw`),
/// and its border-inclusive OUTER box is
/// x [100, 432) x y [200, 632).
const BW: i16 = 16;
const FRAME: u32 = 0x0010_0100;
const FRAME_X: i16 = 100;
const FRAME_Y: i16 = 200;
const FRAME_W: u16 = 300;
const FRAME_H: u16 = 400;

fn make_bordered_child(
    table: &mut ResourceTable,
    id: u32,
    parent: u32,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    border_width: u16,
) {
    table.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(id),
            parent: ResourceId(parent),
            x,
            y,
            width,
            height,
            border_width,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = table.map_window(ResourceId(id));
}

fn bordered_frame_table() -> ResourceTable {
    let mut table = ResourceTable::new();
    make_bordered_child(
        &mut table,
        FRAME,
        ROOT_WINDOW.0,
        FRAME_X,
        FRAME_Y,
        FRAME_W,
        FRAME_H,
        u16::try_from(BW).unwrap(),
    );
    table
}

/// The eight border probes: root-absolute point → the CONTENT-space
/// coordinate the client must be told. Content origin = (116, 216),
/// so the expected value is simply `abs - content_origin`, and every
/// left/top probe is NEGATIVE — Xorg reports `x - pWin->drawable.x`
/// (`dix/window.c:2995`) with no clamp.
const BORDER_PROBES: &[(&str, i16, i16, i16, i16)] = &[
    // (name, abs_x, abs_y, want_x, want_y)
    ("left edge", 100, 416, -16, 200),
    ("top edge", 266, 200, 150, -16),
    ("right edge", 431, 416, 315, 200),
    ("bottom edge", 266, 631, 150, 415),
    ("top-left corner", 100, 200, -16, -16),
    ("top-right corner", 431, 200, 315, -16),
    ("bottom-left corner", 100, 631, -16, 415),
    ("bottom-right corner", 431, 631, 315, 415),
];

/// One pixel outside the outer box on each of the four sides. These
/// must MISS: the fix widens the hit region by exactly `bw`, not
/// more.
const OUTSIDE_PROBES: &[(&str, i16, i16)] = &[
    ("left of left border", 99, 416),
    ("above top border", 266, 199),
    ("right of right border", 432, 416),
    ("below bottom border", 266, 632),
];

#[test]
fn hit_test_border_sides_and_corners_report_content_relative_coords() {
    let table = bordered_frame_table();
    for &(name, ax, ay, wx, wy) in BORDER_PROBES {
        assert_eq!(
            table.pointer_target_at(ROOT_WINDOW, ax, ay),
            Some((ResourceId(FRAME), wx, wy)),
            "{name}: root ({ax},{ay}) must hit the frame at content ({wx},{wy})"
        );
    }
}

#[test]
fn hit_test_rejects_one_pixel_outside_the_border() {
    let table = bordered_frame_table();
    for &(name, ax, ay) in OUTSIDE_PROBES {
        assert_eq!(
            table.pointer_target_at(ROOT_WINDOW, ax, ay),
            Some((ROOT_WINDOW, ax, ay)),
            "{name}: root ({ax},{ay}) is outside the outer box and must not hit"
        );
    }
}

/// The live #133 symptom, as a coordinate assertion. awesome draws
/// its maximize button at content (250, 4); with the pre-fix
/// arithmetic the client was handed `abs - outer_origin`, i.e. 16
/// too large on both axes, so the pointer that was over the button
/// reported the point 16px down-right of it (in the terminal
/// content), while the point 16px UP from the button reported the
/// button.
#[test]
fn hit_test_button_hover_lands_on_the_button_not_a_border_width_away() {
    let table = bordered_frame_table();
    // Pointer physically over the widget the client painted at
    // content (250, 4): root = content_origin + (250, 4).
    let (hit, x, y) = table
        .pointer_target_at(ROOT_WINDOW, FRAME_X + BW + 250, FRAME_Y + BW + 4)
        .expect("frame hit");
    assert_eq!((hit, x, y), (ResourceId(FRAME), 250, 4));
    // And the point one border width ABOVE it is now the border,
    // reported with a negative y — not the widget.
    let (hit, x, y) = table
        .pointer_target_at(ROOT_WINDOW, FRAME_X + BW + 250, FRAME_Y + BW - 16)
        .expect("frame hit");
    assert_eq!((hit, x, y), (ResourceId(FRAME), 250, -16));
}

/// The recurrence: a child's `x`/`y` are relative to its parent's
/// CONTENT origin, so each level of the descent subtracts that
/// level's `x + border_width`.
#[test]
fn hit_test_border_term_accumulates_per_level() {
    let mut table = bordered_frame_table();
    // Child at (10, 20) inside the frame's content, itself with a
    // 4px border. Its content origin is
    //   116 + 10 + 4 = 130, 216 + 20 + 4 = 240.
    let child = 0x0010_0101;
    make_bordered_child(&mut table, child, FRAME, 10, 20, 100, 100, 4);
    assert_eq!(
        table.pointer_target_at(ROOT_WINDOW, 130, 240),
        Some((ResourceId(child), 0, 0)),
        "the child's own content origin must report (0, 0)"
    );
    // Its top-left border corner: 4px up-left of that.
    assert_eq!(
        table.pointer_target_at(ROOT_WINDOW, 126, 236),
        Some((ResourceId(child), -4, -4))
    );
    // One pixel further out is the frame's content, not the child.
    assert_eq!(
        table.pointer_target_at(ROOT_WINDOW, 125, 236),
        Some((ResourceId(FRAME), 9, 20))
    );
}

/// `bw == 0` is the entire current user base (MATE, e16, Cinnamon,
/// XFCE...). The new arithmetic must collapse to the old exactly.
#[test]
fn hit_test_is_identity_at_border_width_zero() {
    let mut table = ResourceTable::new();
    make_bordered_child(
        &mut table,
        0x0010_0200,
        ROOT_WINDOW.0,
        100,
        200,
        300,
        400,
        0,
    );
    make_bordered_child(&mut table, 0x0010_0201, 0x0010_0200, 10, 20, 100, 100, 0);
    // Content origin == outer origin, so the reported coordinate is
    // the plain difference and nothing is ever negative.
    assert_eq!(
        table.pointer_target_at(ROOT_WINDOW, 100, 200),
        Some((ResourceId(0x0010_0200), 0, 0))
    );
    assert_eq!(
        table.pointer_target_at(ROOT_WINDOW, 110, 220),
        Some((ResourceId(0x0010_0201), 0, 0))
    );
    // Old right/bottom exclusive bound: x < width.
    assert_eq!(
        table.pointer_target_at(ROOT_WINDOW, 399, 599),
        Some((ResourceId(0x0010_0200), 299, 399))
    );
    assert_eq!(
        table.pointer_target_at(ROOT_WINDOW, 400, 599),
        Some((ROOT_WINDOW, 400, 599))
    );
    assert_eq!(
        table.pointer_target_at(ROOT_WINDOW, 99, 200),
        Some((ROOT_WINDOW, 99, 200))
    );
}

/// `child_containing_point` is the second implementation named by
/// the plan (8.4). It takes root-absolute coordinates and returns
/// only the window, so agreement is on the WINDOW; the coordinate
/// half is pinned by `pointer_target_at` above and by the
/// `ServerState` cross-check in `server.rs`.
#[test]
fn child_containing_point_agrees_with_pointer_target_at_on_borders() {
    let table = bordered_frame_table();
    for &(name, ax, ay, _, _) in BORDER_PROBES {
        assert_eq!(
            table.child_containing_point(ROOT_WINDOW, i32::from(ax), i32::from(ay)),
            Some(ResourceId(FRAME)),
            "{name}: child_containing_point must include the border ring"
        );
        assert_eq!(
            table
                .pointer_target_at(ROOT_WINDOW, ax, ay)
                .map(|(id, _, _)| id),
            table.child_containing_point(ROOT_WINDOW, i32::from(ax), i32::from(ay)),
            "{name}: the two implementations must not diverge"
        );
    }
    for &(name, ax, ay) in OUTSIDE_PROBES {
        assert_eq!(
            table.child_containing_point(ROOT_WINDOW, i32::from(ax), i32::from(ay)),
            None,
            "{name}: must be outside for child_containing_point too"
        );
    }
}

/// Same agreement one level down, where the border term has to be
/// applied twice — once to reach the frame's content space and once
/// for the child's own ring.
#[test]
fn child_containing_point_agrees_one_level_down() {
    let mut table = bordered_frame_table();
    let child = 0x0010_0101;
    make_bordered_child(&mut table, child, FRAME, 10, 20, 100, 100, 4);
    // The child's own border ring, in root-absolute coords.
    for (ax, ay) in [(126, 236), (129, 300), (233, 343), (126, 343)] {
        assert_eq!(
            table.child_containing_point(ResourceId(FRAME), ax, ay),
            Some(ResourceId(child)),
            "({ax},{ay}) is on the child's border ring"
        );
        assert_eq!(
            table
                .pointer_target_at(
                    ROOT_WINDOW,
                    i16::try_from(ax).unwrap(),
                    i16::try_from(ay).unwrap()
                )
                .map(|(id, _, _)| id),
            Some(ResourceId(child)),
        );
    }
    // Just outside the child's ring on the left: the frame.
    assert_eq!(
        table.child_containing_point(ResourceId(FRAME), 125, 300),
        None
    );
}

/// `Window::to_parent_coords` must be the exact inverse of
/// `to_content_coords` — the event-propagation walks depend on it.
#[test]
fn content_and_parent_coord_translations_are_inverses() {
    let table = bordered_frame_table();
    let w = table.window(ResourceId(FRAME)).expect("frame");
    for (px, py) in [(0i16, 0i16), (100, 200), (-5, 7), (431, 631)] {
        let (cx, cy) = w.to_content_coords(px, py);
        assert_eq!(w.to_parent_coords(cx, cy), (px, py));
    }
    // And the border term is actually present.
    assert_eq!(w.to_content_coords(FRAME_X, FRAME_Y), (-BW, -BW));
    assert_eq!(w.to_parent_coords(0, 0), (FRAME_X + BW, FRAME_Y + BW));
}

#[test]
fn pointer_target_at_returns_deepest_mapped_child_and_relative_coords() {
    let mut table = ResourceTable::new();
    make_window(&mut table, 0x0010_0002);
    make_child(&mut table, 0x0010_0003, 0x0010_0002, 10, 20);
    make_child(&mut table, 0x0010_0004, 0x0010_0003, 5, 6);
    let _ = table.map_window(ResourceId(0x0010_0002));
    let _ = table.map_window(ResourceId(0x0010_0003));
    let _ = table.map_window(ResourceId(0x0010_0004));

    assert_eq!(
        table.pointer_target_at(ResourceId(0x0010_0002), 20, 30),
        Some((ResourceId(0x0010_0004), 5, 4))
    );
}

#[test]
fn pointer_target_at_falls_back_to_top_level_outside_children() {
    let mut table = ResourceTable::new();
    make_window(&mut table, 0x0010_0002);
    make_child(&mut table, 0x0010_0003, 0x0010_0002, 10, 20);
    let _ = table.map_window(ResourceId(0x0010_0002));
    let _ = table.map_window(ResourceId(0x0010_0003));

    assert_eq!(
        table.pointer_target_at(ResourceId(0x0010_0002), 2, 3),
        Some((ResourceId(0x0010_0002), 2, 3))
    );
}

#[test]
fn circulate_child_restacks_to_the_top_or_the_bottom() {
    let mut t = ResourceTable::new();
    make_child(&mut t, 0x200, ROOT_WINDOW.0, 0, 0);
    make_child(&mut t, 0x300, ROOT_WINDOW.0, 0, 0);
    make_child(&mut t, 0x400, ROOT_WINDOW.0, 0, 0);
    t.circulate_child(ResourceId(0x200), true);
    assert_eq!(
        t.children(ROOT_WINDOW),
        &[ResourceId(0x300), ResourceId(0x400), ResourceId(0x200)]
    );
    t.circulate_child(ResourceId(0x200), false);
    assert_eq!(
        t.children(ROOT_WINDOW),
        &[ResourceId(0x200), ResourceId(0x300), ResourceId(0x400)]
    );
}

#[test]
fn circulate_raise_on_root_skips_cow() {
    let mut t = ResourceTable::new();
    make_child(&mut t, 0x200, ROOT_WINDOW.0, 0, 0);
    make_child(&mut t, 0x300, ROOT_WINDOW.0, 0, 0);
    t.windows
        .get_mut(&ROOT_WINDOW.0)
        .unwrap()
        .children
        .push(COMPOSITE_OVERLAY_WINDOW);
    t.circulate_child(ResourceId(0x200), true);
    let kids = &t.window(ROOT_WINDOW).unwrap().children;
    assert_eq!(
        kids.last().copied(),
        Some(COMPOSITE_OVERLAY_WINDOW),
        "COW stays at top across circulate"
    );
    assert_eq!(
        kids[kids.len() - 2],
        ResourceId(0x200),
        "Raise must land 0x200 just below COW, not above it"
    );
}

#[test]
fn configure_window_stack_mode_above_raises_child() {
    let mut t = ResourceTable::new();
    make_child(&mut t, 0x200, ROOT_WINDOW.0, 0, 0);
    make_child(&mut t, 0x300, ROOT_WINDOW.0, 0, 0);
    make_child(&mut t, 0x400, ROOT_WINDOW.0, 0, 0);

    let configured = t.configure_window(ConfigureWindowRequest {
        window: ResourceId(0x200),
        value_mask: 1 << 6,
        x: None,
        y: None,
        width: None,
        height: None,
        border_width: None,
        sibling: None,
        stack_mode: Some(0),
    });

    assert!(configured.is_some());
    assert_eq!(
        t.children(ROOT_WINDOW),
        &[ResourceId(0x300), ResourceId(0x400), ResourceId(0x200)]
    );
}

/// Build a request that only sets sibling + stack_mode. Geometry
/// fields are `None` so existing position/size is preserved.
fn restack_request(window: u32, sibling: Option<u32>, stack_mode: u8) -> ConfigureWindowRequest {
    ConfigureWindowRequest {
        window: ResourceId(window),
        value_mask: 0,
        x: None,
        y: None,
        width: None,
        height: None,
        border_width: None,
        sibling: sibling.map(ResourceId),
        stack_mode: Some(stack_mode),
    }
}

/// Children A=0x200, B=0x300, C=0x400 under root, all 50×50, mapped.
/// `a_x` / `b_x` / `c_x` set the x position of each — y is always 0.
fn three_mapped_children(a_x: i16, b_x: i16, c_x: i16) -> ResourceTable {
    let mut t = ResourceTable::new();
    make_child(&mut t, 0x200, ROOT_WINDOW.0, a_x, 0);
    make_child(&mut t, 0x300, ROOT_WINDOW.0, b_x, 0);
    make_child(&mut t, 0x400, ROOT_WINDOW.0, c_x, 0);
    let _ = t.map_window(ROOT_WINDOW);
    let _ = t.map_window(ResourceId(0x200));
    let _ = t.map_window(ResourceId(0x300));
    let _ = t.map_window(ResourceId(0x400));
    t
}

#[test]
fn stack_mode_above_with_sibling_places_just_above() {
    // A B C ; place A above B → B A C
    let mut t = three_mapped_children(0, 0, 0);
    assert!(
        t.configure_window(restack_request(0x200, Some(0x300), 0))
            .is_some()
    );
    assert_eq!(
        t.children(ROOT_WINDOW),
        &[ResourceId(0x300), ResourceId(0x200), ResourceId(0x400)]
    );
}

#[test]
fn stack_mode_below_with_sibling_places_just_below() {
    // A B C ; place C below B → A C B
    let mut t = three_mapped_children(0, 0, 0);
    assert!(
        t.configure_window(restack_request(0x400, Some(0x300), 1))
            .is_some()
    );
    assert_eq!(
        t.children(ROOT_WINDOW),
        &[ResourceId(0x200), ResourceId(0x400), ResourceId(0x300)]
    );
}

#[test]
fn stack_mode_below_no_sibling_lowers_to_bottom() {
    // A B C ; lower C → C A B
    let mut t = three_mapped_children(0, 0, 0);
    assert!(
        t.configure_window(restack_request(0x400, None, 1))
            .is_some()
    );
    assert_eq!(
        t.children(ROOT_WINDOW),
        &[ResourceId(0x400), ResourceId(0x200), ResourceId(0x300)]
    );
}

#[test]
fn top_if_with_overlapping_higher_sibling_raises_to_top() {
    // A B C all overlapping at (0,0); TopIf on A with sibling=C → C is
    // above A and overlaps → A goes to top.
    let mut t = three_mapped_children(0, 0, 0);
    assert!(
        t.configure_window(restack_request(0x200, Some(0x400), 2))
            .is_some()
    );
    assert_eq!(
        t.children(ROOT_WINDOW),
        &[ResourceId(0x300), ResourceId(0x400), ResourceId(0x200)]
    );
}

#[test]
fn top_if_with_lower_sibling_is_noop() {
    // A B C ; TopIf on C with sibling=A → A is below C, cannot occlude
    // → no-op.
    let mut t = three_mapped_children(0, 0, 0);
    assert!(
        t.configure_window(restack_request(0x400, Some(0x200), 2))
            .is_some()
    );
    assert_eq!(
        t.children(ROOT_WINDOW),
        &[ResourceId(0x200), ResourceId(0x300), ResourceId(0x400)]
    );
}

#[test]
fn top_if_with_higher_sibling_no_overlap_is_noop() {
    // A at x=0, B at x=200, C at x=400 — no geometric overlap.
    let mut t = three_mapped_children(0, 200, 400);
    assert!(
        t.configure_window(restack_request(0x200, Some(0x400), 2))
            .is_some()
    );
    assert_eq!(
        t.children(ROOT_WINDOW),
        &[ResourceId(0x200), ResourceId(0x300), ResourceId(0x400)]
    );
}

#[test]
fn top_if_no_sibling_raises_when_any_higher_overlaps() {
    // A at x=0, B at x=200 (no overlap with A), C at x=20 (overlaps A
    // and is above A) → TopIf on A with no sibling → top.
    let mut t = three_mapped_children(0, 200, 20);
    assert!(
        t.configure_window(restack_request(0x200, None, 2))
            .is_some()
    );
    assert_eq!(
        t.children(ROOT_WINDOW),
        &[ResourceId(0x300), ResourceId(0x400), ResourceId(0x200)]
    );
}

#[test]
fn bottom_if_with_lower_overlapping_sibling_lowers_to_bottom() {
    // A B C all overlapping; BottomIf on C with sibling=A → C is above
    // A and overlaps → C lowers to bottom.
    let mut t = three_mapped_children(0, 0, 0);
    assert!(
        t.configure_window(restack_request(0x400, Some(0x200), 3))
            .is_some()
    );
    assert_eq!(
        t.children(ROOT_WINDOW),
        &[ResourceId(0x400), ResourceId(0x200), ResourceId(0x300)]
    );
}

#[test]
fn bottom_if_with_higher_sibling_is_noop() {
    // A B C ; BottomIf on A with sibling=C → A cannot occlude C
    // (A is below C) → no-op.
    let mut t = three_mapped_children(0, 0, 0);
    assert!(
        t.configure_window(restack_request(0x200, Some(0x400), 3))
            .is_some()
    );
    assert_eq!(
        t.children(ROOT_WINDOW),
        &[ResourceId(0x200), ResourceId(0x300), ResourceId(0x400)]
    );
}

#[test]
fn bottom_if_no_sibling_no_overlap_is_noop() {
    let mut t = three_mapped_children(0, 200, 400);
    assert!(
        t.configure_window(restack_request(0x400, None, 3))
            .is_some()
    );
    assert_eq!(
        t.children(ROOT_WINDOW),
        &[ResourceId(0x200), ResourceId(0x300), ResourceId(0x400)]
    );
}

#[test]
fn opposite_with_higher_overlapping_sibling_goes_to_top() {
    // A B C ; Opposite on A with sibling=C → C above + overlaps → top.
    let mut t = three_mapped_children(0, 0, 0);
    assert!(
        t.configure_window(restack_request(0x200, Some(0x400), 4))
            .is_some()
    );
    assert_eq!(
        t.children(ROOT_WINDOW),
        &[ResourceId(0x300), ResourceId(0x400), ResourceId(0x200)]
    );
}

#[test]
fn opposite_with_lower_overlapping_sibling_goes_to_bottom() {
    // A B C ; Opposite on C with sibling=A → A is below + overlaps
    // (window-occludes-sibling holds) → bottom.
    let mut t = three_mapped_children(0, 0, 0);
    assert!(
        t.configure_window(restack_request(0x400, Some(0x200), 4))
            .is_some()
    );
    assert_eq!(
        t.children(ROOT_WINDOW),
        &[ResourceId(0x400), ResourceId(0x200), ResourceId(0x300)]
    );
}

#[test]
fn opposite_no_overlap_is_noop() {
    let mut t = three_mapped_children(0, 200, 400);
    assert!(
        t.configure_window(restack_request(0x300, None, 4))
            .is_some()
    );
    assert_eq!(
        t.children(ROOT_WINDOW),
        &[ResourceId(0x200), ResourceId(0x300), ResourceId(0x400)]
    );
}

#[test]
fn occlusion_ignores_unmapped_siblings() {
    // A B C all overlapping; unmap C; TopIf on A with no sibling.
    // C is unmapped so cannot occlude; B is above A and overlaps →
    // A still raises to top. Order after restack: [B, C, A].
    let mut t = three_mapped_children(0, 0, 0);
    let _ = t.unmap_window(ResourceId(0x400));
    assert!(
        t.configure_window(restack_request(0x200, None, 2))
            .is_some()
    );
    assert_eq!(
        t.children(ROOT_WINDOW),
        &[ResourceId(0x300), ResourceId(0x400), ResourceId(0x200)]
    );

    // Same shape but with B unmapped too: no mapped occluder remains
    // → no-op.
    let mut t = three_mapped_children(0, 0, 0);
    let _ = t.unmap_window(ResourceId(0x300));
    let _ = t.unmap_window(ResourceId(0x400));
    assert!(
        t.configure_window(restack_request(0x200, None, 2))
            .is_some()
    );
    assert_eq!(
        t.children(ROOT_WINDOW),
        &[ResourceId(0x200), ResourceId(0x300), ResourceId(0x400)]
    );
}

#[test]
fn pointer_hit_test_follows_top_if_restack() {
    // A B at (0,0) overlapping; B is on top so pointer at (10,10) hits
    // B. After TopIf on A → A on top → pointer hits A.
    let mut t = ResourceTable::new();
    make_child(&mut t, 0x200, ROOT_WINDOW.0, 0, 0);
    make_child(&mut t, 0x300, ROOT_WINDOW.0, 0, 0);
    let _ = t.map_window(ROOT_WINDOW);
    let _ = t.map_window(ResourceId(0x200));
    let _ = t.map_window(ResourceId(0x300));

    assert_eq!(
        t.pointer_target_at(ROOT_WINDOW, 10, 10).map(|h| h.0),
        Some(ResourceId(0x300))
    );

    assert!(
        t.configure_window(restack_request(0x200, None, 2))
            .is_some()
    );

    assert_eq!(
        t.pointer_target_at(ROOT_WINDOW, 10, 10).map(|h| h.0),
        Some(ResourceId(0x200))
    );
}

#[test]
fn pointer_hit_test_follows_bottom_if_restack() {
    // A B at (0,0) overlapping; B is on top → pointer hits B.
    // BottomIf on B (no sibling) → B occludes A → bottom → pointer hits A.
    let mut t = ResourceTable::new();
    make_child(&mut t, 0x200, ROOT_WINDOW.0, 0, 0);
    make_child(&mut t, 0x300, ROOT_WINDOW.0, 0, 0);
    let _ = t.map_window(ROOT_WINDOW);
    let _ = t.map_window(ResourceId(0x200));
    let _ = t.map_window(ResourceId(0x300));

    assert!(
        t.configure_window(restack_request(0x300, None, 3))
            .is_some()
    );

    assert_eq!(
        t.pointer_target_at(ROOT_WINDOW, 10, 10).map(|h| h.0),
        Some(ResourceId(0x200))
    );
}

#[test]
fn configure_notify_above_sibling_tracks_restacked_order() {
    let mut t = three_mapped_children(0, 0, 0);
    assert_eq!(t.configure_notify_above_sibling(ResourceId(0x200)), None);
    assert_eq!(
        t.configure_notify_above_sibling(ResourceId(0x300)),
        Some(ResourceId(0x200))
    );
    assert_eq!(
        t.configure_notify_above_sibling(ResourceId(0x400)),
        Some(ResourceId(0x300))
    );

    assert!(
        t.configure_window(restack_request(0x400, Some(0x200), 1))
            .is_some()
    );
    assert_eq!(
        t.children(ROOT_WINDOW),
        &[ResourceId(0x400), ResourceId(0x200), ResourceId(0x300)]
    );
    assert_eq!(t.configure_notify_above_sibling(ResourceId(0x400)), None);
    assert_eq!(
        t.configure_notify_above_sibling(ResourceId(0x200)),
        Some(ResourceId(0x400))
    );
    assert_eq!(
        t.configure_notify_above_sibling(ResourceId(0x300)),
        Some(ResourceId(0x200))
    );
}

#[test]
fn restack_with_sibling_not_in_parent_is_noop() {
    // 0x999 is not a child of root → request must be a no-op (in a
    // real server it would raise BadMatch; the resolver's contract is
    // to leave the local state untouched).
    let mut t = three_mapped_children(0, 0, 0);
    assert!(
        t.configure_window(restack_request(0x200, Some(0x999), 0))
            .is_some()
    );
    assert_eq!(
        t.children(ROOT_WINDOW),
        &[ResourceId(0x200), ResourceId(0x300), ResourceId(0x400)]
    );
}

/// #133 step 7 (P7) — a window's absolute origin includes the border
/// width of every window in the chain, because x/y locate the OUTER
/// corner while the contents start `bw` further in. Xorg pre-sums it
/// as `pWin->drawable.x = pParent->drawable.x + x + bw`
/// (`dix/window.c:888`).
///
/// This is the assertion `d08d6933` reverted away. Awesome is the
/// case that made it matter: with a 16px border the missing term put
/// pointer hit-spots a border width from the widget the client drew.
#[test]
fn window_absolute_position_includes_the_border_width_of_every_ancestor() {
    let mut t = ResourceTable::new();
    // parent at (10,20) bw 3 -> its content origin is (13,23)
    t.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(0x200),
            parent: ROOT_WINDOW,
            x: 10,
            y: 20,
            width: 100,
            height: 100,
            border_width: 3,
            class: 1,
            ..Default::default()
        },
    );
    // child at (5,7) bw 2 inside the parent's CONTENT frame
    t.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(0x201),
            parent: ResourceId(0x200),
            x: 5,
            y: 7,
            width: 20,
            height: 20,
            border_width: 2,
            class: 1,
            ..Default::default()
        },
    );
    assert_eq!(
        t.window_absolute_position(ResourceId(0x200)),
        (13, 23),
        "parent: outer (10,20) + its own bw 3"
    );
    // 10 + 3 (parent bw) + 5 + 2 (own bw) = 20; 20 + 3 + 7 + 2 = 32.
    // Without the border terms this reads (15, 27) — the pre-step-7
    // value, short by one bw per level.
    assert_eq!(
        t.window_absolute_position(ResourceId(0x201)),
        (20, 32),
        "child: accumulates BOTH border widths, not just x/y"
    );
}
