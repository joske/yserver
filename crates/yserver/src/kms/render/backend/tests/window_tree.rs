use super::*;

/// Stage 3f.6 close: `change_subwindow_attributes` stores
/// `bg_pixel` + `bg_pixmap` into the v2 window record instead of
/// logging a gap. value_mask=0x03 (CWBackPixmap + CWBackPixel)
/// with values [pixmap_xid, pixel] lands both. value_mask=0x02
/// alone lands the pixel only. value_mask=0x01 with pixmap=0
/// resolves to bg_pixmap=None per X11 semantics.
#[test]
fn change_subwindow_attributes_stores_bg_state() {
    let mut b = KmsBackend::for_tests();
    // Seed a window in windows directly (allocate fails on
    // for_tests because there's no Vk; geometry insert still
    // works in production via the no-Vk branch).
    b.windows.insert(
        0xCAFE_BABE,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            depth: 32,
            mapped: false,
            viewable: true,
            parent: None,
            stack_rank: 0,
            bg_pixel: None,
            bg_pixmap: None,
            cursor: None,
        },
    );

    // CWBackPixmap (0x01) + CWBackPixel (0x02), values =
    // [0xABCD_1234, 0xFF0000FF].
    b.change_subwindow_attributes(None, 0xCAFE_BABE, 0x03, &[0xABCD_1234, 0xFF00_00FF])
        .expect("ok");
    let geom = b.windows[&0xCAFE_BABE];
    assert_eq!(geom.bg_pixmap, Some(0xABCD_1234));
    assert_eq!(geom.bg_pixel, Some(0xFF00_00FF));

    // CWBackPixmap=0 → None (inherit-from-parent). bg_pixel
    // stays as the previous value (CWBackPixel bit clear).
    b.change_subwindow_attributes(None, 0xCAFE_BABE, 0x01, &[0])
        .expect("ok");
    let geom = b.windows[&0xCAFE_BABE];
    assert_eq!(geom.bg_pixmap, None);
    assert_eq!(geom.bg_pixel, Some(0xFF00_00FF));

    // The pre-3f.6 stub bumped a `change_subwindow_attributes`
    // gap; 3f.6 stores bookkeeping cleanly.
    assert!(
        !b.logged_gaps
            .borrow()
            .contains("change_subwindow_attributes"),
        "change_subwindow_attributes must not log a gap post-3f.6"
    );
}

/// #133 step 2 (P3): the CWA value list is POSITIONAL and ordered
/// by ascending mask bit, so adding the border bits (0x04, 0x08)
/// must not disturb the background bits (0x01, 0x02) when a single
/// call carries all four. A mis-ordered `idx` walk would read the
/// border pixmap out of the background-pixel slot and vice versa,
/// which is exactly the regression this asserts against — hence
/// the background assertions in a border test.
#[test]
fn change_subwindow_attributes_stores_border_state() {
    let mut b = KmsBackend::for_tests();
    b.windows.insert(
        0xCAFE_0100,
        crate::kms::render::backend::WindowGeometry {
            border_width: 4,
            border_pixel: None,
            border_pixmap: None,
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            depth: 24,
            mapped: false,
            viewable: true,
            parent: None,
            stack_rank: 0,
            bg_pixel: None,
            bg_pixmap: None,
            cursor: None,
        },
    );

    // All four bits in one call: CWBackPixmap | CWBackPixel |
    // CWBorderPixmap | CWBorderPixel, values in ascending bit
    // order.
    b.change_subwindow_attributes(
        None,
        0xCAFE_0100,
        0x01 | 0x02 | 0x04 | 0x08,
        &[0xAAAA_0001, 0xFF00_0000, 0xBBBB_0002, 0x00FF_0000],
    )
    .expect("ok");
    let geom = b.windows[&0xCAFE_0100];
    assert_eq!(geom.bg_pixmap, Some(0xAAAA_0001), "bg pixmap slot 0");
    assert_eq!(geom.bg_pixel, Some(0xFF00_0000), "bg pixel slot 1");
    // CWBorderPixel wins over CWBorderPixmap in the same call: the
    // border is an either/or and the pixel is the later bit, so it
    // is what the window ends up with (core resolves this before
    // forwarding too, `dix/window.c:1298`).
    assert_eq!(geom.border_pixel, Some(0x00FF_0000), "border pixel slot 3");
    assert_eq!(geom.border_pixmap, None, "pixel overrides pixmap");
    assert_eq!(geom.border_width, 4, "CWA never touches border_width");

    // Border pixmap alone installs the tile and clears the pixel.
    b.change_subwindow_attributes(None, 0xCAFE_0100, 0x04, &[0xBBBB_0002])
        .expect("ok");
    let geom = b.windows[&0xCAFE_0100];
    assert_eq!(geom.border_pixmap, Some(0xBBBB_0002));
    assert_eq!(geom.border_pixel, None);

    // CWBorderPixmap = 0 is X11 None; the mirror drops the tile.
    b.change_subwindow_attributes(None, 0xCAFE_0100, 0x04, &[0])
        .expect("ok");
    assert_eq!(b.windows[&0xCAFE_0100].border_pixmap, None);

    // A border-only change leaves the background alone.
    b.change_subwindow_attributes(None, 0xCAFE_0100, 0x08, &[0x0000_00FF])
        .expect("ok");
    let geom = b.windows[&0xCAFE_0100];
    assert_eq!(geom.border_pixel, Some(0x0000_00FF));
    assert_eq!(geom.bg_pixmap, Some(0xAAAA_0001), "background untouched");
    assert_eq!(geom.bg_pixel, Some(0xFF00_0000), "background untouched");
}

/// Stage 3f.6 — `create_subwindow` records the parent xid + the
/// background-pixel hint so subsequent `build_scene` traversals
/// can reach the new window and an initial bg_pixel fill can
/// run. Engine fill itself returns `NoVk` on the test fixture;
/// the load-bearing observable is the geometry record.
#[test]
fn create_subwindow_records_parent_and_bg_pixel() {
    use yserver_core::{backend::WindowHandle, host_x11::HostSubwindowVisual};
    let mut b = KmsBackend::for_tests();
    let parent = WindowHandle::from_raw(0x1234_5678).unwrap();
    let child = b
        .create_subwindow(
            None,
            parent,
            10,
            20,
            100,
            50,
            0,
            HostSubwindowVisual::CopyFromParent,
            Some(0xFF11_2233),
            None,
        )
        .expect("create_subwindow");
    let geom = b.windows[&child.as_raw()];
    assert_eq!(geom.parent, Some(0x1234_5678));
    assert_eq!(geom.bg_pixel, Some(0xFF11_2233));
    assert_eq!(geom.x, 10);
    assert_eq!(geom.y, 20);
    assert_eq!(geom.width, 100);
    assert_eq!(geom.height, 50);
    assert_eq!(
        geom.depth, 24,
        "root/untracked CopyFromParent inherits root depth"
    );
    assert!(!geom.mapped, "mapped is set later via map_subwindow");
}

/// #133 step 2 (P3): `create_subwindow` records the `border_width`
/// it has always been handed (and discarded as `_border_width`),
/// and `configure_subwindow` updates it. Recording only — storage
/// stays content-sized until step 3, so this asserts the mirror,
/// not a layout.
#[test]
fn create_and_configure_subwindow_record_border_width() {
    use yserver_core::{
        backend::WindowHandle,
        host_x11::{HostSubwindowConfig, HostSubwindowVisual},
    };
    let mut b = KmsBackend::for_tests();
    let parent = WindowHandle::from_raw(0x1234_5678).unwrap();
    let child = b
        .create_subwindow(
            None,
            parent,
            10,
            20,
            100,
            50,
            16,
            HostSubwindowVisual::CopyFromParent,
            None,
            None,
        )
        .expect("create_subwindow");
    let xid = child.as_raw();
    assert_eq!(b.windows[&xid].border_width, 16, "create records bw");

    // A configure that carries only the border width updates it and
    // touches nothing else.
    b.configure_subwindow(
        None,
        xid,
        HostSubwindowConfig {
            border_width: Some(3),
            ..Default::default()
        },
    )
    .expect("configure_subwindow");
    let geom = b.windows[&xid];
    assert_eq!(geom.border_width, 3);
    assert_eq!(geom.width, 100, "border-only configure keeps the size");
    assert_eq!(geom.height, 50);

    // A configure with no border-width field leaves the recorded
    // value in place (X11 ConfigureWindow is a sparse value list).
    b.configure_subwindow(
        None,
        xid,
        HostSubwindowConfig {
            width: Some(120),
            ..Default::default()
        },
    )
    .expect("configure_subwindow");
    let geom = b.windows[&xid];
    assert_eq!(geom.border_width, 3, "absent field is not a reset to 0");
    assert_eq!(geom.width, 120);
}

#[test]
fn copy_from_parent_child_inherits_argb_parent_depth() {
    use yserver_core::{backend::WindowHandle, host_x11::HostSubwindowVisual};

    let mut b = KmsBackend::for_tests();
    b.windows.insert(
        0x2000,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 0,
            y: 0,
            width: 80,
            height: 40,
            depth: 32,
            mapped: true,
            viewable: true,
            parent: None,
            stack_rank: 0,
            bg_pixel: None,
            bg_pixmap: None,
            cursor: None,
        },
    );
    let child = b
        .create_subwindow(
            None,
            WindowHandle::from_raw(0x2000).unwrap(),
            1,
            2,
            30,
            20,
            0,
            HostSubwindowVisual::CopyFromParent,
            None,
            None,
        )
        .expect("create_subwindow");
    assert_eq!(b.windows[&child.as_raw()].depth, 32);
}

#[test]
fn depth_only_visual_preserves_argb_top_level_depth() {
    use yserver_core::{backend::WindowHandle, host_x11::HostSubwindowVisual};

    let mut b = KmsBackend::for_tests();
    let child = b
        .create_subwindow(
            None,
            WindowHandle::from_raw(b.window_id()).unwrap(),
            0,
            0,
            2944,
            1840,
            0,
            HostSubwindowVisual::DepthOnly { depth: 32 },
            Some(0),
            None,
        )
        .expect("create_subwindow");
    assert_eq!(b.windows[&child.as_raw()].depth, 32);
}

/// Stage 3f.11: reparenting a top-level window INTO another
/// window removes it from `core.top_level_order` so
/// `build_scene` only emits it once (via the recurse from the
/// new parent). Reproducer for the MATE clock-applet duplicate-
/// render: clock was first registered as a top-level under
/// root, then reparented INTO mate-panel's container. Pre-fix,
/// build_scene emitted it twice — once at child-relative coords
/// (treated as absolute) and once at real screen position.
#[test]
fn reparent_into_container_removes_from_top_level_order() {
    let mut b = KmsBackend::for_tests();
    // Two stub windows: the parent container, and the would-be
    // child (initially registered as a top-level).
    b.windows.insert(
        0xC0FFEE,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 0,
            y: 0,
            width: 200,
            height: 100,
            depth: 32,
            mapped: true,
            viewable: true,
            parent: None,
            stack_rank: 0,
            bg_pixel: None,
            bg_pixmap: None,
            cursor: None,
        },
    );
    b.windows.insert(
        0xCAFED00D,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 0,
            y: 0,
            width: 50,
            height: 20,
            depth: 32,
            mapped: true,
            viewable: true,
            parent: None,
            stack_rank: 1,
            bg_pixel: None,
            bg_pixmap: None,
            cursor: None,
        },
    );
    // Reparent 0xCAFED00D under 0xC0FFEE at (30, 10).
    b.reparent_subwindow(None, 0xCAFED00D, 0xC0FFEE, 30, 10)
        .expect("reparent");

    // Step 2 (DRIFT 2): the double-emit fix is now the windows.parent
    // update — build_scene recurses by parent, so a child with a tracked
    // parent is reached only via the recurse, never the top-level walk.
    // (top_level_order membership itself is projected from core children
    // by the reparent core handler's sync_top_level_order, not here.)
    let geom = b.windows[&0xCAFED00D];
    assert_eq!(geom.parent, Some(0xC0FFEE));
    assert_eq!(geom.x, 30);
    assert_eq!(geom.y, 10);
}

/// Phase 4.1 (COW structural redesign): protocol-validation
/// guarantees the host_parent exists in `windows` by the time
/// `reparent_subwindow` is invoked. A missing entry means projection
/// drift between resources and backend — a fatal internal-
/// consistency failure, not a recoverable "treat as top-level".
/// The old silent fallback masked the cinnamon COW bug for weeks;
/// surfacing the drift via panic is the spec'd behaviour.
#[test]
#[should_panic(expected = "reparent_subwindow")]
fn reparent_subwindow_panics_when_host_parent_missing() {
    let mut b = KmsBackend::for_tests();
    // Pre-seed the child so reparent_subwindow has something to
    // operate on; the panic must come from the missing host_parent
    // lookup, not from a missing child.
    let child_xid: u32 = 0x0040_0050;
    b.windows.insert(
        child_xid,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            depth: 24,
            mapped: true,
            viewable: true,
            parent: None,
            stack_rank: 0,
            bg_pixel: None,
            bg_pixmap: None,
            cursor: None,
        },
    );
    // Reparent to a host_parent that doesn't exist in windows.
    // 0 is the legitimate "reparent to root" convention; 0xDEADBEEF
    // is genuinely absent and must trip the drift panic.
    let _ = b.reparent_subwindow(None, child_xid, 0xDEAD_BEEF, 0, 0);
}

/// Task 4.1 regression: the production reparent-to-root path
/// (`process_request.rs` `handle_reparent_window`) computes
/// `host_parent` as `backend.window_id()` (== `core.window_id` == 1)
/// for `ReparentWindow(child -> ROOT_WINDOW)`, NOT 0. Root is never
/// tracked in `windows`, so the missing-parent panic guard must
/// treat `core.window_id` as a root sentinel (-> top-level) instead
/// of crashing. Without the fix this panics on every WM window-
/// withdraw / frame-teardown.
#[test]
fn reparent_subwindow_to_root_via_window_id_does_not_panic() {
    let mut b = KmsBackend::for_tests();
    let root_xid = b.window_id();
    assert_eq!(root_xid, 1, "core.window_id sentinel changed");
    let child_xid: u32 = 0x0040_0050;
    b.windows.insert(
        child_xid,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 5,
            y: 7,
            width: 100,
            height: 100,
            depth: 24,
            mapped: true,
            viewable: true,
            // Originally a sub-window under some frame; the WM now
            // reparents it back to root (withdraw / frame teardown).
            parent: Some(0x0040_0060),
            stack_rank: 0,
            bg_pixel: None,
            bg_pixmap: None,
            cursor: None,
        },
    );
    // host_parent == root's real host xid (core.window_id), exactly
    // what the production call site passes for reparent-to-ROOT.
    let res = b.reparent_subwindow(None, child_xid, root_xid, 0, 0);
    assert!(res.is_ok(), "reparent-to-root must succeed, got {res:?}");
    // Child is now a top-level under root (parent cleared to None). Its
    // top_level_order membership is projected from core by the reparent
    // core handler's sync_top_level_order, not by this backend method.
    assert_eq!(b.windows.get(&child_xid).unwrap().parent, None);
}

// (Removed `restack_below_no_sibling_moves_to_bottom` /
// `restack_above_no_sibling_moves_to_top`: `restack_top_level` is
// deleted in Step 2 — top-level order is a projection of core children
// via `sync_top_level_order`. The Below/Above-to-bottom/top semantics
// are now exercised by core's restack tests in resources.rs plus the
// `drift2_*` projection gates.)

/// Stage 3f.11 follow-up: subwindow restack updates sibling order
/// within a shared parent instead of relying on HashMap iteration.
#[test]
fn restack_subwindow_updates_sibling_order() {
    let mut b = KmsBackend::for_tests();
    b.windows.insert(
        0xCAFE,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 0,
            y: 0,
            width: 10,
            height: 10,
            depth: 32,
            mapped: true,
            viewable: true,
            parent: Some(0xBEEF),
            stack_rank: 0,
            bg_pixel: None,
            bg_pixmap: None,
            cursor: None,
        },
    );
    b.windows.insert(
        0xD00D,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 0,
            y: 0,
            width: 10,
            height: 10,
            depth: 32,
            mapped: true,
            viewable: true,
            parent: Some(0xBEEF),
            stack_rank: 1,
            bg_pixel: None,
            bg_pixmap: None,
            cursor: None,
        },
    );
    b.restack_subwindow(0xD00D, 1, Some(0xCAFE));
    assert!(b.windows[&0xD00D].stack_rank < b.windows[&0xCAFE].stack_rank);
}

/// Stage 3f.11 / Step 2: reparenting back to root clears the window's
/// `windows.parent` to `None` so it resumes top-level rendering (its
/// `top_level_order` membership is projected from core children by the
/// reparent core handler). The Backend trait's reparent call carries
/// the new parent xid; `host_parent==0` or an untracked xid (root is
/// `core.window_id`, not in `windows`) maps to `parent=None`.
#[test]
fn reparent_to_root_clears_parent() {
    let mut b = KmsBackend::for_tests();
    b.windows.insert(
        0xC0FFEE,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 0,
            y: 0,
            width: 200,
            height: 100,
            depth: 32,
            mapped: true,
            viewable: true,
            parent: None,
            stack_rank: 0,
            bg_pixel: None,
            bg_pixmap: None,
            cursor: None,
        },
    );
    b.windows.insert(
        0xCAFED00D,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 30,
            y: 10,
            width: 50,
            height: 20,
            depth: 32,
            mapped: true,
            viewable: true,
            parent: Some(0xC0FFEE),
            stack_rank: 1,
            bg_pixel: None,
            bg_pixmap: None,
            cursor: None,
        },
    );
    // Start: child is a sub-window of 0xC0FFEE.
    assert_eq!(b.windows[&0xCAFED00D].parent, Some(0xC0FFEE));

    // Reparent to root (host_parent=0 maps to parent=None).
    b.reparent_subwindow(None, 0xCAFED00D, 0, 100, 200)
        .expect("reparent");

    // Now a top-level (parent cleared); position updated.
    let geom = b.windows[&0xCAFED00D];
    assert_eq!(geom.parent, None);
    assert_eq!(geom.x, 100);
    assert_eq!(geom.y, 200);
}
