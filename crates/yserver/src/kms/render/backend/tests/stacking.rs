use super::*;

// ────────────────────────────────────────────────────────────
// Phase 2 root-cause pin (Task 7.5).
//
// The yserver-core sibling tests
// (`reparent_*_redirect_*` in
// `crates/yserver-core/src/core_loop/process_request.rs`) pin
// the backing-existence state on the resource layer. This test
// exercises the actual user-visible path: drive a
// `ReparentWindow` through the full `process_request`
// dispatcher, then assert
// `resolve_paint_target(nm_applet_host_xid)` routes through
// mate-panel's redirected ancestor with the correct
// screen-coord offset — the symptom that breaks the live
// mate-panel tray.
// ────────────────────────────────────────────────────────────

// ── DRIFT 1 regression test: input-shape empty-vs-absent (backend) ──
//
// Companion to the core-side regression tests in
// crates/yserver-core/src/server.rs. GREEN as of Step 1a (findings
// 2026-06-18): set_shape_rectangles now carries an Option so an
// explicit empty region `Some([])` is stored faithfully (not deleted),
// and cursor_inside_shape reads `Some([])` as click-through — matching
// core. Before Step 1a the API collapsed `None` and `Some([])` to an
// empty slice and deleted the entry, so this returned `above` (opaque).
#[test]
fn drift1_backend_empty_input_shape_is_click_through() {
    let mut b = KmsBackend::for_tests();

    // `below` (0x1000, opaque) under `above` (0x2000, topmost); full
    // overlap; cursor in the overlap.
    for (xid, rank) in [(0x1000_u32, 0_u64), (0x2000_u32, 1_u64)] {
        b.windows.insert(
            xid,
            crate::kms::render::backend::WindowGeometry {
                border_width: 0,
                border_pixel: None,
                border_pixmap: None,
                x: 0,
                y: 0,
                width: 200,
                height: 200,
                depth: 32,
                mapped: true,
                viewable: true,
                parent: None,
                stack_rank: rank,
                bg_pixel: None,
                bg_pixmap: None,
                cursor: None,
            },
        );
    }
    b.core.top_level_order.push(0x1000); // below
    b.core.top_level_order.push(0x2000); // above (topmost)
    b.core.cursor_x = 50.0;
    b.core.cursor_y = 50.0;

    // `above` is given an explicit EMPTY input region — Some(&[]),
    // NOT None (which would mean "unset" → opaque). This is the same
    // client-facing path a compositor uses for a click-through window.
    b.set_shape_rectangles(None, 0x2000, 2 /* input */, Some(&[]))
        .expect("set explicit empty input shape");

    // The empty input region lets the click fall through to `below`.
    assert_eq!(
        b.window_under_cursor(),
        Some(0x1000),
        "empty input shape must be click-through at the backend hit-test, \
             matching core_empty_input_shape_makes_window_click_through"
    );
}

// ── DRIFT 2 regression: top-level order projects core children ──
//
// GREEN as of Step 2 (findings 2026-06-18 §8): the backend's top-level
// z-order is a pure projection of core children via
// `sync_top_level_order`, so it inherits core's conditional TopIf
// occlusion for free. A TopIf that core treats as a no-op (no
// overlapping higher sibling) leaves the projected order unchanged —
// whereas the deleted independent `restack_top_level` raised
// unconditionally and drifted.
#[test]
fn drift2_topif_noop_keeps_backend_order_in_sync_with_core() {
    use yserver_core::resources::ROOT_WINDOW;
    use yserver_protocol::x11::{ConfigureWindowRequest, ResourceId};

    let mut state = ServerState::new();
    let mut b = KmsBackend::for_tests();

    // Three NON-overlapping top-levels A<mid<C (A bottom, C top).
    let a = ResourceId(0x0010_0200);
    let mid = ResourceId(0x0010_0300);
    let c = ResourceId(0x0010_0400);
    seed_state_window(&mut state, &mut b, a, ROOT_WINDOW, 0, 0, 100, 100);
    seed_state_window(&mut state, &mut b, mid, ROOT_WINDOW, 200, 0, 100, 100);
    seed_state_window(&mut state, &mut b, c, ROOT_WINDOW, 400, 0, 100, 100);
    for w in [a, mid, c] {
        let _ = state.resources.map_window(w);
    }
    // Seed a deliberately WRONG backend order to prove the sync derives
    // from core children, not from prior backend state.
    b.core.top_level_order = vec![synth_host_xid(c), synth_host_xid(a)];

    // Client sends TopIf (stack_mode=2) on A with no sibling. CORE: A
    // has no overlapping higher sibling → no-op; children stays [A,mid,C].
    state.resources.configure_window(ConfigureWindowRequest {
        window: a,
        value_mask: 0,
        x: None,
        y: None,
        width: None,
        height: None,
        border_width: None,
        sibling: None,
        stack_mode: Some(2),
    });
    // Production calls this from the ConfigureWindow handler after
    // configure_window.
    b.sync_top_level_order(&state);

    let expected: Vec<u32> = state
        .resources
        .children(ROOT_WINDOW)
        .iter()
        .map(|id| synth_host_xid(*id))
        .collect();
    assert_eq!(
        b.core.top_level_order, expected,
        "top_level_order must project core children order"
    );
    assert_eq!(
        b.core.top_level_order,
        vec![synth_host_xid(a), synth_host_xid(mid), synth_host_xid(c)],
        "a no-op TopIf leaves the projected order [A, mid, C]"
    );
}

// ── DRIFT 2b regression: COW stays topmost across a raise ──
//
// GREEN as of Step 2: `top_level_order` projects core children, and
// core caps a raise just below the COW (`cow_aware_top_index`), so the
// projection keeps the COW last. The deleted `restack_top_level`
// pushed a raised window ABOVE the COW, breaking always-on-top.
#[test]
fn drift2_raise_keeps_cow_on_top() {
    use yserver_core::resources::{COMPOSITE_OVERLAY_WINDOW, ROOT_WINDOW};
    use yserver_protocol::x11::{ConfigureWindowRequest, ResourceId};

    let mut state = ServerState::new();
    let mut b = KmsBackend::for_tests();

    // A normal top-level, then materialize the COW on top of it.
    let win = ResourceId(0x0010_0800);
    seed_state_window(&mut state, &mut b, win, ROOT_WINDOW, 0, 0, 100, 100);
    let _ = state.resources.map_window(win);
    state.resources.materialize_cow_resource(
        yserver_core::backend::WindowHandle::from_raw_panicking(COMPOSITE_OVERLAY_WINDOW.0),
    );

    // Client/WM raises `win` to the top. CORE caps it just below the COW.
    state.resources.configure_window(ConfigureWindowRequest {
        window: win,
        value_mask: 0,
        x: None,
        y: None,
        width: None,
        height: None,
        border_width: None,
        sibling: None,
        stack_mode: Some(0), // Above, no sibling → raise to top
    });
    b.sync_top_level_order(&state);

    assert_eq!(
        b.core.top_level_order.last(),
        Some(&COMPOSITE_OVERLAY_WINDOW.0),
        "the Composite Overlay Window must stay topmost after a raise-to-top"
    );
    assert!(
        b.core.top_level_order.contains(&synth_host_xid(win)),
        "the raised window is still present (just below the COW)"
    );
}

// ── DRIFT 2 projection shape: unmapped included, subwindows excluded ──
//
// Codex review 2026-06-18: `sync_top_level_order` must include unmapped
// root children (X11 stacking order survives unmap/remap) and exclude
// subwindows (reached via descendant recursion, not the top-level walk).
#[test]
fn sync_top_level_order_includes_unmapped_excludes_subwindows() {
    use yserver_core::resources::ROOT_WINDOW;
    use yserver_protocol::x11::ResourceId;

    let mut state = ServerState::new();
    let mut b = KmsBackend::for_tests();

    let mapped = ResourceId(0x0010_0a00);
    let unmapped = ResourceId(0x0010_0a01);
    let sub = ResourceId(0x0010_0a02);
    seed_state_window(&mut state, &mut b, mapped, ROOT_WINDOW, 0, 0, 100, 100);
    seed_state_window(&mut state, &mut b, unmapped, ROOT_WINDOW, 0, 0, 100, 100);
    seed_state_window(&mut state, &mut b, sub, mapped, 0, 0, 50, 50);
    let _ = state.resources.map_window(mapped);
    // `unmapped` deliberately left unmapped; `sub` is a child of `mapped`.
    let _ = state.resources.map_window(sub);

    b.sync_top_level_order(&state);

    assert_eq!(
        b.core.top_level_order,
        vec![synth_host_xid(mapped), synth_host_xid(unmapped)],
        "projection keeps unmapped root children (order survives unmap) and \
             excludes subwindows (reached via descendant recursion)"
    );
}

#[test]
fn sync_top_level_order_restack_requests_direct_unflip() {
    use yserver_core::resources::ROOT_WINDOW;
    use yserver_protocol::x11::{ConfigureWindowRequest, ResourceId};

    let mut state = ServerState::new();
    let mut b = KmsBackend::for_tests();
    let below = ResourceId(0x0010_0a10);
    let above = ResourceId(0x0010_0a20);
    seed_state_window(&mut state, &mut b, below, ROOT_WINDOW, 0, 0, 100, 100);
    seed_state_window(&mut state, &mut b, above, ROOT_WINDOW, 0, 0, 100, 100);
    let _ = state.resources.map_window(below);
    let _ = state.resources.map_window(above);
    b.sync_top_level_order(&state);

    b.get_overlay_window(None).expect("materialize COW");
    let cow_id = b.cow_id.expect("COW id");
    let _ = install_direct_frame_for_target_test(&mut b, synth_host_xid(below), cow_id, true);

    state.resources.configure_window(ConfigureWindowRequest {
        window: below,
        value_mask: 0,
        x: None,
        y: None,
        width: None,
        height: None,
        border_width: None,
        sibling: None,
        stack_mode: Some(0), // Above, no sibling -> raise to top.
    });
    b.sync_top_level_order(&state);

    assert!(b.scanout_m2.unflip_requested);
    assert!(!b.scanout_m2.hold_direct);
}

#[test]
fn sync_top_level_order_without_restack_keeps_direct_scanout_active() {
    use yserver_core::resources::ROOT_WINDOW;
    use yserver_protocol::x11::ResourceId;

    let mut state = ServerState::new();
    let mut b = KmsBackend::for_tests();
    let target = ResourceId(0x0010_0a30);
    seed_state_window(&mut state, &mut b, target, ROOT_WINDOW, 0, 0, 100, 100);
    let _ = state.resources.map_window(target);
    b.sync_top_level_order(&state);

    b.get_overlay_window(None).expect("materialize COW");
    let cow_id = b.cow_id.expect("COW id");
    let _ = install_direct_frame_for_target_test(&mut b, synth_host_xid(target), cow_id, true);

    b.sync_top_level_order(&state);

    assert!(!b.scanout_m2.unflip_requested);
    assert!(b.scanout_m2.hold_direct);
}

/// A compositor's frame (the COW, or a window inside it — Muffin's output
/// window) stacks above every top-level, so a restack beneath it cannot
/// change the screen and must NOT unflip. Cinnamon restacks on every
/// raise, tooltip and notification, and each needless unflip showed a
/// stale frame. The ordinary-window case above still unflips.
fn restack_under_direct_target_is_ignored(direct_target_under_cow: fn(&mut KmsBackend) -> u32) {
    use yserver_core::resources::ROOT_WINDOW;
    use yserver_protocol::x11::{ConfigureWindowRequest, ResourceId};

    let mut state = ServerState::new();
    let mut b = KmsBackend::for_tests();
    let below = ResourceId(0x0010_0b10);
    let above = ResourceId(0x0010_0b20);
    seed_state_window(&mut state, &mut b, below, ROOT_WINDOW, 0, 0, 100, 100);
    seed_state_window(&mut state, &mut b, above, ROOT_WINDOW, 0, 0, 100, 100);
    let _ = state.resources.map_window(below);
    let _ = state.resources.map_window(above);
    b.sync_top_level_order(&state);

    b.get_overlay_window(None).expect("materialize COW");
    let cow_id = b.cow_id.expect("COW id");
    let target = direct_target_under_cow(&mut b);
    let _ = install_direct_frame_for_target_test(&mut b, target, cow_id, true);

    state.resources.configure_window(ConfigureWindowRequest {
        window: below,
        value_mask: 0,
        x: None,
        y: None,
        width: None,
        height: None,
        border_width: None,
        sibling: None,
        stack_mode: Some(0), // Above, no sibling -> raise to top.
    });
    b.sync_top_level_order(&state);

    assert!(
        !b.scanout_m2.unflip_requested,
        "a restack under the COW must not leave direct scanout"
    );
    assert!(b.scanout_m2.hold_direct);
}

#[test]
fn sync_top_level_order_restack_under_direct_cow_keeps_direct_scanout() {
    restack_under_direct_target_is_ignored(|_| yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0);
}

#[test]
fn sync_top_level_order_restack_under_direct_cow_descendant_keeps_direct_scanout() {
    restack_under_direct_target_is_ignored(|b| {
        let child = 0x00F0_0D00;
        let _ = seed_window(
            b,
            child,
            Some(yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0),
            0,
            0,
        );
        child
    });
}

/// A direct Present bypasses the scene compositor, so a fullscreen
/// unredirected window may only scan out directly while it remains the
/// frontmost mapped top-level on that output.  Without this gate,
/// `sync_top_level_order`'s composed unflip is just a one-frame pulse:
/// the next Present from the now-covered fullscreen window re-enters
/// direct scanout and hides the raised window again (#160).
#[test]
fn unredirected_direct_scanout_rejects_a_fullscreen_window_covered_by_a_raised_top_level() {
    use yserver_core::resources::ROOT_WINDOW;
    use yserver_protocol::x11::ResourceId;

    let mut state = ServerState::new();
    let mut b = KmsBackend::for_tests();
    let fullscreen = ResourceId(0x0010_0a40);
    let raised = ResourceId(0x0010_0a41);
    seed_state_window(&mut state, &mut b, fullscreen, ROOT_WINDOW, 0, 0, 100, 100);
    seed_state_window(&mut state, &mut b, raised, ROOT_WINDOW, 20, 20, 40, 40);
    b.windows.get_mut(&synth_host_xid(raised)).unwrap().mapped = false;
    let _ = state.resources.map_window(fullscreen);
    b.sync_top_level_order(&state);

    assert!(
        b.unredirected_direct_scene_eligible(synth_host_xid(fullscreen), (100, 100)),
        "the mapped fullscreen window is eligible while no mapped top-level covers it"
    );

    let _ = state.resources.map_window(raised);
    b.windows.get_mut(&synth_host_xid(raised)).unwrap().mapped = true;
    b.sync_top_level_order(&state);

    assert!(
        !b.unredirected_direct_scene_eligible(synth_host_xid(fullscreen), (100, 100)),
        "a raised mapped top-level must keep the covered fullscreen window out of direct scanout"
    );
}

/// Window depth is not a statement about whether the currently presented
/// pixels are opaque. In particular, full-screen GL/EGL clients commonly
/// use a depth-32 visual and have always been eligible for direct scanout.
#[test]
fn unredirected_direct_scanout_keeps_an_uncovered_depth32_fullscreen_window_eligible() {
    use yserver_core::resources::ROOT_WINDOW;
    use yserver_protocol::x11::ResourceId;

    let mut state = ServerState::new();
    let mut b = KmsBackend::for_tests();
    let fullscreen = ResourceId(0x0010_0a42);
    seed_state_window(&mut state, &mut b, fullscreen, ROOT_WINDOW, 0, 0, 100, 100);
    b.windows
        .get_mut(&synth_host_xid(fullscreen))
        .unwrap()
        .depth = 32;
    let _ = state.resources.map_window(fullscreen);
    b.sync_top_level_order(&state);

    assert!(
        b.unredirected_direct_scene_eligible(synth_host_xid(fullscreen), (100, 100)),
        "an uncovered depth-32 fullscreen window must retain direct-scanout eligibility"
    );
}

// ── Cross-layer agreement regression gates ──
//
// These pin scenarios where the core hit-test and the backend
// hit-test already AGREE today (findings 2026-06-18 §8.1). They are
// green; the backend demotion must keep them green. Where the red
// drift1_/drift2_ acceptance tests pin disagreement to be fixed,
// these pin agreement to be preserved — together they bracket the
// demotion.

/// Map a core hit-test result to the synthetic host xid used by
/// `seed_state_window`, for comparison against backend hit-test results.
#[cfg(test)]
fn core_target_host(state: &yserver_core::server::ServerState, x: i16, y: i16) -> Option<u32> {
    state
        .root_pointer_target_at(x, y)
        .map(|(id, _, _)| synth_host_xid(id))
}

#[test]
fn crosslayer_overlapping_top_levels_agree_on_topmost() {
    use yserver_core::resources::ROOT_WINDOW;
    use yserver_protocol::x11::ResourceId;

    let mut state = ServerState::new();
    let mut b = KmsBackend::for_tests();

    // Two fully-overlapping top-levels; `above` created last → topmost
    // in both stores. Default (absent) input shape → both opaque.
    let below = ResourceId(0x0010_0500);
    let above = ResourceId(0x0010_0600);
    seed_state_window(&mut state, &mut b, below, ROOT_WINDOW, 0, 0, 200, 200);
    seed_state_window(&mut state, &mut b, above, ROOT_WINDOW, 0, 0, 200, 200);
    let _ = state.resources.map_window(below);
    let _ = state.resources.map_window(above);
    b.core.top_level_order = vec![synth_host_xid(below), synth_host_xid(above)];
    b.core.cursor_x = 50.0;
    b.core.cursor_y = 50.0;

    // Both layers resolve the topmost overlapping window.
    assert_eq!(
        core_target_host(&state, 50, 50),
        Some(synth_host_xid(above))
    );
    assert_eq!(b.window_under_cursor(), Some(synth_host_xid(above)));
}

#[test]
fn crosslayer_framed_client_both_layers_resolve_client() {
    use yserver_core::resources::ROOT_WINDOW;
    use yserver_protocol::x11::ResourceId;

    let mut state = ServerState::new();
    let mut b = KmsBackend::for_tests();

    // WM frame (top-level) containing a reparented client (subwindow).
    let frame = ResourceId(0x0010_0700);
    let client = ResourceId(0x0010_0701);
    seed_state_window(&mut state, &mut b, frame, ROOT_WINDOW, 0, 0, 300, 300);
    seed_state_window(&mut state, &mut b, client, frame, 10, 10, 100, 100);
    let _ = state.resources.map_window(frame);
    let _ = state.resources.map_window(client);
    // Only the frame is a top-level; the client is a subwindow.
    b.core.top_level_order = vec![synth_host_xid(frame)];
    b.core.cursor_x = 50.0;
    b.core.cursor_y = 50.0;

    // Both layers descend the frame and resolve the deepest hit — the
    // client — and the core maps it back up to the frame top-level.
    assert_eq!(
        core_target_host(&state, 50, 50),
        Some(synth_host_xid(client)),
        "core hit-test descends frame → client"
    );
    assert_eq!(
        b.window_under_cursor(),
        Some(synth_host_xid(client)),
        "backend hit-test descends frame → client"
    );
    assert_eq!(
        state.top_level_for_target(client),
        frame,
        "the client's top-level is the frame"
    );
}
