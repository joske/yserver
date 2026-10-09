use super::*;

/// Stage 4d regression: `ChangeWindowAttributes` on a window
/// under COMPOSITE redirect must NOT trigger a backing wipe.
/// Pre-fix `change_subwindow_attributes` eagerly called
/// `clear_window_area_with_background`, which routes through
/// `resolve_paint_target` into the redirected backing B and
/// fills it with depth-24 default black — exactly the
/// "mate-control-center turns opaque black on drag" symptom
/// observed in hardware smoke (marco re-asserts CWA on every
/// drag-induced configure; v2 interprets that as a paint
/// command and wipes B).
///
/// X11 spec: CWA's background attribute change does not
/// repaint. The bg setting only affects future
/// `ClearArea` / Expose handling. v2's eager clear was a
/// Stage 3f.6 over-reach.
#[test]
fn cwa_on_redirected_window_does_not_clear_backing() {
    use crate::kms::render::store::{DrawableKind, Storage};
    use yserver_core::backend::Backend;

    let mut b = KmsBackend::for_tests();

    // Set up W as a top-level window in windows + the store.
    let w_xid: u32 = 0x100_0001;
    let stack_rank = b.alloc_window_stack_rank();
    b.windows.insert(
        w_xid,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 100,
            y: 100,
            width: 200,
            height: 200,
            depth: 24,
            mapped: true,
            viewable: true,
            parent: None,
            bg_pixel: None,
            bg_pixmap: None,
            stack_rank,
            cursor: None,
        },
    );
    let w_storage = Storage::for_tests_null(
        ash::vk::Extent2D {
            width: 200,
            height: 200,
        },
        ash::vk::Format::B8G8R8A8_UNORM,
    );
    let _w_id = b
        .store
        .allocate(w_xid, DrawableKind::Window, 24, true, w_storage)
        .expect("alloc W");

    // Set up B as a pixmap, then install the redirect route
    // W → B. This is the load-bearing precondition for the
    // bug: the fill path would route through W's redirect.
    let b_xid: u32 = 0x100_0002;
    let b_storage = Storage::for_tests_null(
        ash::vk::Extent2D {
            width: 200,
            height: 200,
        },
        ash::vk::Format::B8G8R8A8_UNORM,
    );
    let b_id = b
        .store
        .allocate(b_xid, DrawableKind::Pixmap, 24, false, b_storage)
        .expect("alloc B");
    assert!(b.test_set_redirected_target(w_xid, b_xid));
    // Sanity: resolve_paint_target on W now lands at B, not W.
    let resolved = b
        .resolve_paint_target(w_xid)
        .expect("resolve_paint_target W");
    assert_eq!(
        resolved.backing_id(),
        b_id,
        "fixture sanity: W's paint must route to B before issuing CWA",
    );

    // Snapshot the clear counter.
    let calls_before = b.clear_window_area_calls;

    // Issue CWA with CWBackPixmap = None (value=0). That's
    // marco's "no background pixmap" attribute change, sent
    // on every drag-induced configure. v2 must NOT interpret
    // this as a paint command on the redirected backing.
    b.change_subwindow_attributes(None, w_xid, 0x01, &[0])
        .expect("change_subwindow_attributes");

    assert_eq!(
        b.clear_window_area_calls, calls_before,
        "CWA on a redirected window must not call clear_window_area_with_background \
             (pre-fix this fired and wiped B with depth-24 default black, destroying \
             the compositor's painted pixels — the 'opaque black on drag' bug)",
    );

    // Same test with CWBackPixel — also a clear-trigger pre-fix.
    b.change_subwindow_attributes(None, w_xid, 0x02, &[0x00FF_FFFF])
        .expect("change_subwindow_attributes CWBackPixel");
    assert_eq!(
        b.clear_window_area_calls, calls_before,
        "CWBackPixel on a redirected window must also skip the eager clear",
    );

    // Sanity: bg state IS stored (CWA still records the
    // values; only the eager paint is skipped).
    let geom = b.windows.get(&w_xid).expect("W in windows");
    assert_eq!(geom.bg_pixel, Some(0x00FF_FFFF));
    assert_eq!(geom.bg_pixmap, None);
}

/// X11 spec generalisation: CWA's background attribute change
/// MUST NOT repaint, regardless of whether the window is under
/// composite redirect. The bg attribute only affects future
/// `ClearArea` / Expose handling. The earlier
/// `cwa_on_redirected_window_does_not_clear_backing` guard only
/// caught the redirect case; the non-redirect case still wiped
/// the client's storage. Live trigger (2026-05-30): non-
/// composited mate-control-center sidebar going black when
/// caja takes focus over CC — marco re-asserts CWA on CC's
/// client window, yserver clears its 975×600 storage to bg=0
/// (black), and GTK gets no Expose so the bg never repaints.
/// Widgets come back on hover (per-widget redraw), bg stays
/// black.
#[test]
fn cwa_on_non_redirected_window_does_not_clear_storage() {
    use crate::kms::render::store::{DrawableKind, Storage};
    use yserver_core::backend::Backend;

    let mut b = KmsBackend::for_tests();

    let w_xid: u32 = 0x200_0001;
    let stack_rank = b.alloc_window_stack_rank();
    b.windows.insert(
        w_xid,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 100,
            y: 100,
            width: 300,
            height: 200,
            depth: 24,
            mapped: true,
            viewable: true,
            parent: None,
            bg_pixel: None,
            bg_pixmap: None,
            stack_rank,
            cursor: None,
        },
    );
    let w_storage = Storage::for_tests_null(
        ash::vk::Extent2D {
            width: 300,
            height: 200,
        },
        ash::vk::Format::B8G8R8A8_UNORM,
    );
    let _w_id = b
        .store
        .allocate(w_xid, DrawableKind::Window, 24, true, w_storage)
        .expect("alloc W");
    // Sanity: not under redirect — paint target is the leaf
    // itself, which is the path the existing guard fails to
    // catch.
    let leaf = b.store.lookup(w_xid).expect("W leaf");
    assert!(b.store.redirected_target(leaf).is_none(), "fixture sanity");
    let resolved = b.resolve_paint_target(w_xid).expect("resolve");
    assert_eq!(
        resolved.backing_id(),
        leaf,
        "fixture sanity: paint stays at leaf"
    );

    let calls_before = b.clear_window_area_calls;

    // CWBackPixmap = None (marco's per-configure churn).
    b.change_subwindow_attributes(None, w_xid, 0x01, &[0])
        .expect("CWA");
    assert_eq!(
        b.clear_window_area_calls, calls_before,
        "CWA on a non-redirected window must not clear storage \
             (pre-fix this fired and wiped the client's pixmap to bg=0, \
             producing the 'CC sidebar black on focus-uncover' symptom)",
    );

    // CWBackPixel — second clear-trigger pre-fix.
    b.change_subwindow_attributes(None, w_xid, 0x02, &[0x00FF_FFFF])
        .expect("CWA bg_pixel");
    assert_eq!(
        b.clear_window_area_calls, calls_before,
        "CWBackPixel on a non-redirected window must also skip the eager clear",
    );

    // Sanity: bg state IS still stored.
    let geom = b.windows.get(&w_xid).expect("W in windows");
    assert_eq!(geom.bg_pixel, Some(0x00FF_FFFF));
    assert_eq!(geom.bg_pixmap, None);
}

/// Follow-up to `cwa_on_redirected_window_does_not_clear_backing`:
/// the original Stage 4d fix only checks `redirected_target(W)`
/// — i.e. W has its OWN backing. That misses the case where W
/// has no own backing but its paints route to an ANCESTOR's
/// backing via `resolve_paint_target`'s ancestor walk. Live
/// trigger: mate-panel tray applets — reparented away from root
/// into mate-panel sockets, they have no own backing but paint
/// to mate-panel's backing via the ancestor chain. marco's CWA
/// bg_pixmap=None per drag-induced configure lands a transparent
/// fill into mate-panel's backing at the applet's screen
/// position, wiping the icon. Symptom: applets visible briefly
/// then disappear.
#[test]
fn cwa_on_descendant_routed_to_redirected_ancestor_does_not_clear() {
    use yserver_core::{backend::Backend, resources::ROOT_WINDOW, server::ServerState};
    use yserver_protocol::x11::ResourceId;

    let mut state = ServerState::new();
    let mut backend = KmsBackend::for_tests();

    let mate_panel_xid = ResourceId(0x110_0001);
    let applet_xid = ResourceId(0x140_0001);

    // mate-panel: child of root, has its own redirected backing.
    seed_state_window(
        &mut state,
        &mut backend,
        mate_panel_xid,
        ROOT_WINDOW,
        0,
        0,
        2560,
        28,
    );
    seed_redirected_backing(&mut state, &mut backend, mate_panel_xid);
    // applet: child of mate-panel, no own backing.
    seed_state_window(
        &mut state,
        &mut backend,
        applet_xid,
        mate_panel_xid,
        0,
        1,
        24,
        24,
    );

    let applet_host_xid = synth_host_xid(applet_xid);

    // Fixture sanity: applet has its own leaf drawable but no
    // own redirected target, while `resolve_paint_target` walks
    // up to mate-panel's backing. That's the load-bearing
    // precondition: pre-fix the existing `is_redirected` check
    // returns false (no own backing), but a CWA-time clear
    // would still route through `resolve_paint_target` and land
    // on mate-panel's backing.
    let applet_leaf = backend
        .store
        .lookup(applet_host_xid)
        .expect("applet has its own leaf drawable");
    assert!(
        backend.store.redirected_target(applet_leaf).is_none(),
        "applet must not have its own backing"
    );
    let resolved = backend
        .resolve_paint_target(applet_host_xid)
        .expect("applet paint must resolve");
    assert_ne!(
        resolved.backing_id(),
        applet_leaf,
        "applet paints must route to an ancestor's backing, not its own leaf"
    );

    let calls_before = backend.clear_window_area_calls;

    // marco's "bg_pixmap = None" CWA on the applet. Pre-fix
    // this routes a transparent fill into mate-panel's backing
    // at the applet's screen position, wiping any content there.
    backend
        .change_subwindow_attributes(None, applet_host_xid, 0x01, &[0])
        .expect("change_subwindow_attributes");

    assert_eq!(
        backend.clear_window_area_calls, calls_before,
        "CWA on a window whose paints route to a redirected ancestor must \
             not call clear_window_area_with_background (pre-fix this fired \
             and wiped mate-panel's backing where the tray applet icon was \
             painted — the 'tray applet visible briefly then disappears' \
             symptom)"
    );

    // Same check for CWBackPixel (the other clear-trigger).
    backend
        .change_subwindow_attributes(None, applet_host_xid, 0x02, &[0x00FF_FFFF])
        .expect("change_subwindow_attributes CWBackPixel");
    assert_eq!(
        backend.clear_window_area_calls, calls_before,
        "CWBackPixel on a window routed to a redirected ancestor must \
             also skip the eager clear"
    );
}

/// v2 backend's `copy_area` does ITS OWN ClipByChildren split
/// (independent of the protocol-layer split in
/// `copy_area_effective_dst_rects`). That second pass had no
/// manual-redirect exemption, so even when the protocol layer
/// delivered a non-empty sub-rect for a tray-style scenario,
/// the v2 layer re-clipped it to empty and the engine call
/// never fired. Pin the v2 side's exemption directly: parent
/// with mapped child fully overlapping, child's scene
/// participation is false (Manual-redirect semantics) → the
/// engine.copy_area dispatch loop must run at least once.
#[test]
fn copy_area_clip_by_children_skips_manually_redirected_child() {
    use crate::kms::render::store::{DrawableKind, Storage};
    use yserver_core::backend::Backend;

    let mut b = KmsBackend::for_tests();

    let parent_xid: u32 = 0x100_0001;
    let child_xid: u32 = 0x100_0002;
    let src_pixmap_xid: u32 = 0x100_0003;

    let parent_stack_rank = b.alloc_window_stack_rank();
    b.windows.insert(
        parent_xid,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 0,
            y: 0,
            width: 100,
            height: 80,
            depth: 24,
            mapped: true,
            viewable: true,
            parent: None,
            bg_pixel: None,
            bg_pixmap: None,
            stack_rank: parent_stack_rank,
            cursor: None,
        },
    );
    b.store
        .allocate(
            parent_xid,
            DrawableKind::Window,
            24,
            true,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 100,
                    height: 80,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("alloc parent");

    // Manually-redirected child fully covering the parent.
    // scene_participating=false is the v2-store reflection of
    // Manual-redirect semantics (X server stops auto-painting
    // it into the scene/parent backing).
    let child_stack_rank = b.alloc_window_stack_rank();
    b.windows.insert(
        child_xid,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 0,
            y: 0,
            width: 100,
            height: 80,
            depth: 24,
            mapped: true,
            viewable: true,
            parent: Some(parent_xid),
            bg_pixel: None,
            bg_pixmap: None,
            stack_rank: child_stack_rank,
            cursor: None,
        },
    );
    let child_id = b
        .store
        .allocate(
            child_xid,
            DrawableKind::Window,
            24,
            false, // scene_participating=false → Manual semantics
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 100,
                    height: 80,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("alloc manual-redirected child");
    // Allocate the child's redirected backing pixmap so the
    // scene_participating=false + has-backing combination
    // matches Manual semantics (not just an unmapped or
    // input-only window).
    let backing_id = b
        .store
        .allocate(
            0x100_0099,
            DrawableKind::Pixmap,
            24,
            false,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 100,
                    height: 80,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("alloc child backing");
    assert!(b.test_set_redirected_target(child_xid, 0x100_0099));
    let _ = (child_id, backing_id);

    // Source pixmap for the copy.
    b.store
        .allocate(
            src_pixmap_xid,
            DrawableKind::Pixmap,
            24,
            false,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 100,
                    height: 80,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("alloc src pixmap");

    // Pre-call snapshot — engine isn't a real Vk so each
    // engine.copy_area returns NoVk, but the counter increments
    // *before* the call, which is what we're measuring (the
    // surviving-sub-rect count, not engine success).
    let calls_before = b.engine_copy_area_calls;

    b.copy_area(None, src_pixmap_xid, parent_xid, 0, 0, 0, 0, 100, 80)
        .expect("copy_area must not return Err");

    assert!(
        b.engine_copy_area_calls > calls_before,
        "engine.copy_area must dispatch at least once when the only \
             child fully overlapping the dst is manually redirected; \
             pre-fix the v2 ClipByChildren clipped the rect to empty and \
             the loop never ran (counter unchanged) — the live symptom \
             being notification-area-applet's CopyArea silently dropped"
    );
}

/// Regression guard: an AUTOMATIC-redirected child (i.e. one
/// whose own backing exists but `scene_participating=true`)
/// must STILL be subtracted by v2's ClipByChildren. The
/// manual-only exemption in
/// `copy_area_clip_by_children_skips_manually_redirected_child`
/// must not loosen this case — under Automatic mode the X
/// server auto-composites the child's backing into the parent's
/// pixmap, so the parent's own paint must avoid those rects.
#[test]
fn copy_area_clip_by_children_still_subtracts_automatic_child_in() {
    use crate::kms::render::store::{DrawableKind, Storage};
    use yserver_core::backend::Backend;

    let mut b = KmsBackend::for_tests();

    let parent_xid: u32 = 0x200_0001;
    let child_xid: u32 = 0x200_0002;
    let src_pixmap_xid: u32 = 0x200_0003;

    let parent_stack_rank = b.alloc_window_stack_rank();
    b.windows.insert(
        parent_xid,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 0,
            y: 0,
            width: 100,
            height: 80,
            depth: 24,
            mapped: true,
            viewable: true,
            parent: None,
            bg_pixel: None,
            bg_pixmap: None,
            stack_rank: parent_stack_rank,
            cursor: None,
        },
    );
    b.store
        .allocate(
            parent_xid,
            DrawableKind::Window,
            24,
            true,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 100,
                    height: 80,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("alloc parent");

    // Automatic-redirected child fully covering the parent —
    // distinguished by `scene_participating=true` even though it
    // has its own redirected backing.
    let child_stack_rank = b.alloc_window_stack_rank();
    b.windows.insert(
        child_xid,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 0,
            y: 0,
            width: 100,
            height: 80,
            depth: 24,
            mapped: true,
            viewable: true,
            parent: Some(parent_xid),
            bg_pixel: None,
            bg_pixmap: None,
            stack_rank: child_stack_rank,
            cursor: None,
        },
    );
    b.store
        .allocate(
            child_xid,
            DrawableKind::Window,
            24,
            true, // scene_participating=true → Automatic semantics
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 100,
                    height: 80,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("alloc automatic-redirected child");
    b.store
        .allocate(
            0x200_0099,
            DrawableKind::Pixmap,
            24,
            false,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 100,
                    height: 80,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("alloc child backing");
    assert!(b.test_set_redirected_target(child_xid, 0x200_0099));

    b.store
        .allocate(
            src_pixmap_xid,
            DrawableKind::Pixmap,
            24,
            false,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 100,
                    height: 80,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("alloc src pixmap");

    let calls_before = b.engine_copy_area_calls;

    b.copy_area(None, src_pixmap_xid, parent_xid, 0, 0, 0, 0, 100, 80)
        .expect("copy_area must not return Err");

    assert_eq!(
        b.engine_copy_area_calls, calls_before,
        "Automatic-redirected child fully covering dst must still be \
             subtracted (clip to empty → no engine.copy_area dispatch). \
             The manual-only exemption must not loosen this case."
    );
}

/// Tk/gitk hover regression under Cinnamon (2026-06-19): a
/// lower sibling's CopyArea routed into a shared redirected
/// backing must not clobber the higher sibling's visible rect.
/// With a middle vertical overlap the surviving paint is two
/// disjoint side bands, so the dispatch loop must run twice.
#[test]
fn copy_area_into_lower_sibling_excludes_higher_sibling_in_shared_backing() {
    use crate::kms::render::store::{DrawableKind, Storage};
    use yserver_core::backend::Backend;

    let mut b = KmsBackend::for_tests();

    let parent_xid: u32 = 0x300_0001;
    let lower_xid: u32 = 0x300_0002;
    let upper_xid: u32 = 0x300_0003;
    let backing_xid: u32 = 0x300_0004;
    let src_pixmap_xid: u32 = 0x300_0005;

    let parent_rank = b.alloc_window_stack_rank();
    b.windows.insert(
        parent_xid,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 0,
            y: 0,
            width: 100,
            height: 80,
            depth: 24,
            mapped: true,
            viewable: true,
            parent: None,
            bg_pixel: None,
            bg_pixmap: None,
            stack_rank: parent_rank,
            cursor: None,
        },
    );
    b.store
        .allocate(
            parent_xid,
            DrawableKind::Window,
            24,
            true,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 100,
                    height: 80,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("alloc parent");
    b.store
        .allocate(
            backing_xid,
            DrawableKind::Pixmap,
            24,
            false,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 100,
                    height: 80,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("alloc parent backing");
    assert!(b.test_set_redirected_target(parent_xid, backing_xid));

    let lower_rank = b.alloc_window_stack_rank();
    b.windows.insert(
        lower_xid,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 0,
            y: 0,
            width: 100,
            height: 80,
            depth: 24,
            mapped: true,
            viewable: true,
            parent: Some(parent_xid),
            bg_pixel: None,
            bg_pixmap: None,
            stack_rank: lower_rank,
            cursor: None,
        },
    );
    b.store
        .allocate(
            lower_xid,
            DrawableKind::Window,
            24,
            true,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 100,
                    height: 80,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("alloc lower sibling");

    let upper_rank = b.alloc_window_stack_rank();
    b.windows.insert(
        upper_xid,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 25,
            y: 0,
            width: 50,
            height: 80,
            depth: 24,
            mapped: true,
            viewable: true,
            parent: Some(parent_xid),
            bg_pixel: None,
            bg_pixmap: None,
            stack_rank: upper_rank,
            cursor: None,
        },
    );
    b.store
        .allocate(
            upper_xid,
            DrawableKind::Window,
            24,
            true,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 50,
                    height: 80,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("alloc upper sibling");

    b.store
        .allocate(
            src_pixmap_xid,
            DrawableKind::Pixmap,
            24,
            false,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 100,
                    height: 80,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("alloc src pixmap");

    let calls_before = b.engine_copy_area_calls;

    b.copy_area(None, src_pixmap_xid, lower_xid, 0, 0, 0, 0, 100, 80)
        .expect("copy_area must not return Err");

    assert_eq!(
        b.engine_copy_area_calls,
        calls_before + 2,
        "a higher sibling overlapping the middle of the lower sibling \
             must split the copy into the two uncovered side bands; pre-fix \
             v2 emitted one full unsplit copy into the shared redirected backing"
    );
}

/// Steam CEF nests its full-window Present target below several wrapper
/// windows while putting the browser body in a separate, higher-stacked
/// branch. Both branches flatten into the top-level redirect backing. A
/// clip walk limited to the Present target's immediate siblings therefore
/// misses the body and lets every full-window Present overwrite it.
#[test]
fn copy_area_excludes_higher_cousin_in_shared_redirect_backing() {
    use crate::kms::render::store::{DrawableKind, Storage};
    use yserver_core::backend::Backend;

    let mut b = KmsBackend::for_tests();

    let owner_xid = 0x310_0001;
    let lower_branch_xid = 0x310_0002;
    let present_window_xid = 0x310_0003;
    let upper_cousin_xid = 0x310_0004;
    let backing_xid = 0x310_0005;
    let src_pixmap_xid = 0x310_0006;

    let owner_rank = b.alloc_window_stack_rank();
    b.windows.insert(
        owner_xid,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 0,
            y: 0,
            width: 100,
            height: 80,
            depth: 24,
            mapped: true,
            viewable: true,
            parent: None,
            bg_pixel: None,
            bg_pixmap: None,
            stack_rank: owner_rank,
            cursor: None,
        },
    );
    b.store
        .allocate(
            owner_xid,
            DrawableKind::Window,
            24,
            true,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 100,
                    height: 80,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("alloc redirect owner");
    b.store
        .allocate(
            backing_xid,
            DrawableKind::Pixmap,
            24,
            false,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 100,
                    height: 80,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("alloc redirect backing");
    assert!(b.test_set_redirected_target(owner_xid, backing_xid));

    let lower_rank = b.alloc_window_stack_rank();
    b.windows.insert(
        lower_branch_xid,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 0,
            y: 0,
            width: 100,
            height: 80,
            depth: 24,
            mapped: true,
            viewable: true,
            parent: Some(owner_xid),
            bg_pixel: None,
            bg_pixmap: None,
            stack_rank: lower_rank,
            cursor: None,
        },
    );
    b.store
        .allocate(
            lower_branch_xid,
            DrawableKind::Window,
            24,
            true,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 100,
                    height: 80,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("alloc lower wrapper branch");

    let present_rank = b.alloc_window_stack_rank();
    b.windows.insert(
        present_window_xid,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 0,
            y: 0,
            width: 100,
            height: 80,
            depth: 24,
            mapped: true,
            viewable: true,
            parent: Some(lower_branch_xid),
            bg_pixel: None,
            bg_pixmap: None,
            stack_rank: present_rank,
            cursor: None,
        },
    );
    b.store
        .allocate(
            present_window_xid,
            DrawableKind::Window,
            24,
            true,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 100,
                    height: 80,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("alloc nested Present window");

    let upper_rank = b.alloc_window_stack_rank();
    b.windows.insert(
        upper_cousin_xid,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 1,
            y: 20,
            width: 98,
            height: 40,
            depth: 24,
            mapped: true,
            viewable: true,
            parent: Some(owner_xid),
            bg_pixel: None,
            bg_pixmap: None,
            stack_rank: upper_rank,
            cursor: None,
        },
    );
    b.store
        .allocate(
            upper_cousin_xid,
            DrawableKind::Window,
            24,
            true,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 98,
                    height: 40,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("alloc higher CEF body cousin");

    b.store
        .allocate(
            src_pixmap_xid,
            DrawableKind::Pixmap,
            24,
            false,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 100,
                    height: 80,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("alloc Present source pixmap");

    let calls_before = b.engine_copy_area_calls;
    b.copy_area(
        None,
        src_pixmap_xid,
        present_window_xid,
        0,
        0,
        0,
        0,
        100,
        80,
    )
    .expect("copy_area must not return Err");

    assert_eq!(
        b.engine_copy_area_calls,
        calls_before + 4,
        "the higher cousin must split the full-window Present into top, \
             bottom, left, and right strips instead of overwriting its body"
    );
}

// ───── Stage 4a — resolve_paint_target ─────────────────────────

/// Seed a window in `windows` and a matching no-Vk store
/// entry, returning the new DrawableId. Used by the 4a
/// resolver tests so the ancestor walk has something to chew
/// on without touching Vk.
/// The target of a window painting into an ancestor's backing:
/// clipped to `(x, y, w, h)`, the window inside its ancestors, with no
/// border term.
fn in_ancestor_backing(
    id: crate::kms::render::store::DrawableId,
    offset: (i32, i32),
    (x, y, width, height): (i32, i32, u32, u32),
    depth: u8,
) -> PaintTarget {
    PaintTarget::new(id, offset, None, depth).within_window_bounds(Some(ash::vk::Rect2D {
        offset: ash::vk::Offset2D { x, y },
        extent: ash::vk::Extent2D { width, height },
    }))
}

/// Integration guard: the real `render_composite` op must RETURN the
/// ClipByChildren clipList (window − mapped child ∩ op bbox) in
/// window-local coords — the region the core damages. Complements the
/// pure `render_dst_cliplist_local` unit tests by exercising the full
/// op body (bbox derivation + helper call + RegionRect conversion).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn mem_accounting_tracks_drawable_storage_by_use() {
    use crate::kms::vk::mem_accounting::{self, MemCategory};
    use yserver_core::{
        backend::{Backend, WindowHandle},
        host_x11::HostSubwindowVisual,
    };
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    // Submit pending clears, wait, and retire freed drawables.
    fn settle(b: &mut KmsBackend) {
        b.engine
            .close_open_frame(
                &mut b.store,
                &mut b.platform,
                crate::kms::render::frame_builder::CloseReason::SyncWait,
            )
            .expect("close frame");
        b.engine
            .flush_submit_group(
                &mut b.store,
                &mut b.platform,
                crate::kms::render::submit_group::FlushReason::SyncBoundary,
            )
            .expect("flush");
        b.platform.wait_idle_bounded();
        b.poll_pending_retire_with_invalidate();
    }
    // The ledger is process-global and tests run in parallel: check our
    // own handles, and use odd extents no other test allocates.
    let memory_of = |b: &KmsBackend, xid: u32| {
        let id = b.store.lookup(xid).expect("drawable in store");
        b.store.get(id).expect("drawable").storage.memory
    };
    let (pw, ph) = (1021u16, 1019u16);
    let pixmap_floor = u64::from(pw) * u64::from(ph) * 4;
    let pixmaps: Vec<u32> = (0..4)
        .map(|_| b.create_pixmap(None, 32, pw, ph).expect("pixmap").as_raw())
        .collect();
    let pixmap_mems: Vec<_> = pixmaps.iter().map(|&x| memory_of(&b, x)).collect();
    let mut ours = 0;
    for &m in &pixmap_mems {
        let (size, cat) = mem_accounting::entry_of(m).expect("pixmap memory tracked");
        assert_eq!(cat, MemCategory::Pixmap);
        assert!(size >= pixmap_floor, "size {size} < {pixmap_floor}");
        ours += size;
    }
    let snap = mem_accounting::snapshot();
    let pixmap_total =
        snap.device_local(MemCategory::Pixmap).bytes + snap.host(MemCategory::Pixmap).bytes;
    assert!(
        pixmap_total >= ours,
        "pixmap total {pixmap_total} < ours {ours}"
    );

    let root = WindowHandle::from_raw(1).expect("root");
    let w = b
        .create_subwindow(
            None,
            root,
            0,
            0,
            509,
            503,
            0,
            HostSubwindowVisual::Explicit {
                depth: 32,
                visual_xid: 0,
                colormap_xid: 0,
            },
            None,
            None,
        )
        .expect("create window");
    // Created unmapped: no storage until it becomes viewable.
    assert!(b.store.lookup(w.as_raw()).is_none(), "no leaf before map");
    b.map_window_for_tests(w.as_raw()).expect("map window");
    let w_mem = memory_of(&b, w.as_raw());
    assert_eq!(
        mem_accounting::entry_of(w_mem).map(|e| e.1),
        Some(MemCategory::WindowStorage)
    );
    let backing = b
        .allocate_redirected_backing(None, w, 509, 503, 32)
        .expect("redirect backing");
    let backing_mem = memory_of(&b, backing.as_raw());
    assert_eq!(
        mem_accounting::entry_of(backing_mem).map(|e| e.1),
        Some(MemCategory::RedirectBacking)
    );

    for &x in &pixmaps {
        b.free_pixmap(None, x).expect("free pixmap");
    }
    settle(&mut b);
    for &m in &pixmap_mems {
        // Freed, or (never here: the fixture has no pool) parked idle.
        match mem_accounting::entry_of(m) {
            None | Some((_, MemCategory::PoolIdle)) => {}
            Some((size, cat)) => assert!(
                !(cat == MemCategory::Pixmap && size >= pixmap_floor),
                "freed pixmap memory still accounted as {cat:?} ({size} B)"
            ),
        }
    }

    // Pool round trip: a return parks the memory as PoolIdle, a hit
    // hands the same memory back as Pixmap.
    let vk = std::sync::Arc::clone(b.platform.vk.as_ref().expect("vk"));
    let pool = std::sync::Arc::new(crate::kms::vk::pixmap_pool::PixmapPool::new(vk));
    b.platform.pixmap_pool = Some(std::sync::Arc::clone(&pool));
    let small = b.create_pixmap(None, 32, 61, 59).expect("small").as_raw();
    let small_mem = memory_of(&b, small);
    assert_eq!(
        mem_accounting::entry_of(small_mem).map(|e| e.1),
        Some(MemCategory::Pixmap)
    );
    b.free_pixmap(None, small).expect("free small");
    settle(&mut b);
    assert_eq!(
        mem_accounting::entry_of(small_mem).map(|e| e.1),
        Some(MemCategory::PoolIdle)
    );
    let again = b.create_pixmap(None, 32, 61, 59).expect("again").as_raw();
    assert_eq!(memory_of(&b, again), small_mem, "pool hit reuses memory");
    assert_eq!(
        mem_accounting::entry_of(small_mem).map(|e| e.1),
        Some(MemCategory::Pixmap)
    );
    eprintln!(
        "{}",
        mem_accounting::format_line(&mem_accounting::snapshot(), None)
    );
    b.free_pixmap(None, again).expect("free again");
    settle(&mut b);
    pool.drain();
    assert_eq!(mem_accounting::entry_of(small_mem), None);
}

#[test]
fn collect_fill_rects_for_inferiors_translates_root_to_top_level_child() {
    let mut b = KmsBackend::for_tests();
    let root_xid = b.core.window_id;
    let _top = seed_window(&mut b, 0x200, Some(root_xid), 10, 20);
    let out = b.collect_fill_rects_for_inferiors(
        b.core.window_id,
        &[Rectangle16 {
            x: 15,
            y: 25,
            width: 20,
            height: 20,
        }],
    );
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].0, 0x200);
    assert_eq!(
        out[0].1,
        vec![Rectangle16 {
            x: 5,
            y: 5,
            width: 20,
            height: 20,
        }]
    );
}

/// Unknown xid → `None`. Neither the drawable store nor
/// `windows` knows about it, so there is no leaf identity
/// target and no ancestor chain to walk.
#[test]
fn resolve_paint_target_unknown_xid_returns_none() {
    let b = KmsBackend::for_tests();
    assert_eq!(b.resolve_paint_target(0xDEAD_BEEF), None);
}

/// Pixmap xid (not in `windows`) with no redirect →
/// identity result. Covers the pre-loop short-circuit so the
/// ancestor walk never reads `None` off a pixmap.
#[test]
fn resolve_paint_target_pixmap_returns_identity() {
    use crate::kms::render::store::{DrawableKind, Storage};
    let mut b = KmsBackend::for_tests();
    let pix_id = b
        .store
        .allocate(
            0x2000,
            DrawableKind::Pixmap,
            32,
            false,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 64,
                    height: 64,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("pixmap allocate");
    let pt = b.resolve_paint_target(0x2000).expect("resolve");
    assert_eq!(pt, PaintTarget::new(pix_id, (0, 0), None, 32));
}

/// Top-level window with no redirect → identity result.
/// `parent == None` reaches the explicit fall-through arm; the
/// resolver must NOT short-circuit to `None` via `?` on the
/// missing parent.
#[test]
fn resolve_paint_target_unredirected_top_level_returns_identity() {
    let mut b = KmsBackend::for_tests();
    let w_id = seed_window(&mut b, 0x100, None, 0, 0);
    let pt = b.resolve_paint_target(0x100).expect("resolve");
    assert_eq!(pt, PaintTarget::new(w_id, (0, 0), None, 24));
}

/// `set_redirected_target(W, Some(B))` routes paint against
/// `W`'s xid to `B`'s drawable. Offset stays `(0, 0)` —
/// `W` is the redirected node itself, not a descendant.
#[test]
fn resolve_paint_target_redirected_window_routes_to_backing() {
    use crate::kms::render::store::{DrawableKind, Storage};
    let mut b = KmsBackend::for_tests();
    let w_id = seed_window(&mut b, 0x100, None, 0, 0);
    let b_id = b
        .store
        .allocate(
            0x900,
            DrawableKind::RedirectedBacking,
            32,
            false,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 100,
                    height: 100,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("backing allocate");
    b.store.set_redirected_target(w_id, Some(b_id));
    let pt = b.resolve_paint_target(0x100).expect("resolve");
    assert_eq!(pt, PaintTarget::new(b_id, (0, 0), None, 24));
}

/// Descendant paint accumulates `(x, y)` offsets up the
/// ancestor chain. W at root with redirect to B; child C at
/// (10, 20) under W; grandchild G at (3, 4) under C. Paint on
/// G's xid resolves to `(B, (13, 24))` — the sum of the
/// child offsets traversed.
/// xfce4-screensaver-preferences under xfwm4's compositor: frame F
/// (756x534) redirected, client C at (5,29) 746x500, GTK's bin window
/// V at (8,8) 730x531, all `bw == 0`. V paints into F's backing at
/// (13,37) and, as its Xorg clipList (`mi/mivaltree.c:390`), only
/// down to C's bottom edge: 500 - 8 = 492 rows, not 531 over the
/// frame's bottom border. The clip is no border term, so the
/// direct-scanout gate (`has_border_clip`) does not see it.
#[test]
fn resolve_paint_target_clips_a_child_to_its_parent_in_the_shared_backing() {
    use crate::kms::render::store::{DrawableKind, Storage};
    let mut b = KmsBackend::for_tests();
    let f_id = seed_bordered_window(&mut b, 0x100, None, 902, 441, 756, 534, 0);
    seed_bordered_window(&mut b, 0x200, Some(0x100), 5, 29, 746, 500, 0);
    seed_bordered_window(&mut b, 0x300, Some(0x200), 8, 8, 730, 531, 0);
    let b_id = b
        .store
        .allocate(
            0x900,
            DrawableKind::RedirectedBacking,
            32,
            false,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 756,
                    height: 534,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("backing allocate");
    b.store.set_redirected_target(f_id, Some(b_id));
    assert_eq!(
        b.paint_target_shape_for_tests(0x300),
        Some(((13, 37), Some((13, 37, 730, 492)), false))
    );
    assert_eq!(
        b.paint_target_shape_for_tests(0x200),
        Some(((5, 29), Some((5, 29, 746, 500)), false))
    );
    // F paints into its own backing: nothing but the backing clips it.
    assert_eq!(
        b.paint_target_shape_for_tests(0x100),
        Some(((0, 0), None, false))
    );
}

/// The clip a window gets in a backing it shares, measured against
/// Xorg by tools/vng-scenarios/child-clip-probe.c: frame F redirected,
/// client C at (5,20) 190x100, sibling H at (130,15) 30x20 stacked
/// above C, and C's child V at (100,10) 80x120 bounding-shaped to its
/// top 60 rows (GDK's viewport clip). C may not paint under H; V may
/// not paint under H, below C, or outside its shape; and C keeps the
/// part of V's rect outside V's shape (`SetBorderSize`,
/// `dix/window.c:1747-1770`; `miComputeClips`,
/// `mi/mivaltree.c:390-437`).
#[test]
fn shared_backing_draw_clip_takes_out_higher_siblings_and_shapes() {
    use crate::kms::render::store::{DrawableKind, Storage};
    let mut b = KmsBackend::for_tests();
    let f_id = seed_bordered_window(&mut b, 0x100, None, 0, 0, 200, 150, 0);
    seed_bordered_window(&mut b, 0x200, Some(0x100), 5, 20, 190, 100, 0);
    seed_bordered_window(&mut b, 0x300, Some(0x100), 130, 15, 30, 20, 0);
    seed_bordered_window(&mut b, 0x400, Some(0x200), 100, 10, 80, 120, 0);
    b.windows.get_mut(&0x300).unwrap().stack_rank = 1;
    b.core.shape_bounding.insert(
        0x400,
        vec![yserver_protocol::x11::xfixes::RegionRect {
            x: 0,
            y: 0,
            width: 80,
            height: 60,
        }],
    );
    let b_id = b
        .store
        .allocate(
            0x900,
            DrawableKind::RedirectedBacking,
            32,
            false,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 200,
                    height: 150,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("backing allocate");
    b.store.set_redirected_target(f_id, Some(b_id));
    let covered = |rects: &[ash::vk::Rect2D], x: i32, y: i32| {
        rects.iter().any(|r| {
            x >= r.offset.x
                && y >= r.offset.y
                && x < r.offset.x + i32::try_from(r.extent.width).unwrap()
                && y < r.offset.y + i32::try_from(r.extent.height).unwrap()
        })
    };
    let clip_of = |xid| {
        let t = b.resolve_paint_target(xid).expect("resolve");
        b.shared_backing_draw_clip(xid, &t).expect("narrowed")
    };
    let c = clip_of(0x200);
    // H covers C-local (125,-5)..(155,15).
    assert!(!covered(&c, 130, 5) && covered(&c, 130, 15) && covered(&c, 0, 0));
    assert!(covered(&c, 189, 99) && !covered(&c, 190, 0));
    let v = clip_of(0x400);
    // H covers V-local (25,-15)..(55,5); the shape ends at row 60.
    assert!(!covered(&v, 30, 2) && covered(&v, 30, 10) && covered(&v, 10, 50));
    assert!(!covered(&v, 10, 70) && !covered(&v, 10, 95));
    // F draws into its own backing: nothing narrows it.
    let f = b.resolve_paint_target(0x100).unwrap();
    assert_eq!(b.shared_backing_draw_clip(0x100, &f), None);
    // ClipByChildren on C takes V's shape out of C, not V's rect.
    let geom = *b.windows.get(&0x400).unwrap();
    let content_box = ash::vk::Rect2D {
        offset: ash::vk::Offset2D { x: 100, y: 10 },
        extent: ash::vk::Extent2D {
            width: 80,
            height: 120,
        },
    };
    assert_eq!(
        b.child_clip_region(0x400, &geom, content_box),
        vec![ash::vk::Rect2D {
            offset: ash::vk::Offset2D { x: 100, y: 10 },
            extent: ash::vk::Extent2D {
                width: 80,
                height: 60,
            },
        }]
    );
}

/// GTK scrolls that bin window by moving it: V from (8,8) to (8,-42)
/// in C, i.e. from backing origin (13,37) to (13,-13). Xorg's
/// `fbCopyWindow` fills the new borderClip from the old one
/// translated; both stay inside C (backing rows 29..529), so the copy
/// covers V's local rows 42..492 — the old visible rows 0..492 moved
/// up 50 and cut at C's top. Nothing lands in the title bar above
/// row 29, nor is anything read from below row 529.
#[test]
fn shared_backing_move_pieces_stay_inside_the_parent_at_both_ends() {
    let rect = |x, y, width, height| ash::vk::Rect2D {
        offset: ash::vk::Offset2D { x, y },
        extent: ash::vk::Extent2D { width, height },
    };
    let outer = rect(0, 0, 730, 531);
    let parent = Some(rect(5, 29, 746, 500));
    assert_eq!(
        shared_backing_move_pieces(outer, None, &[], &[], parent, (13, 37), (13, -13)),
        vec![rect(0, 42, 730, 450)]
    );
    // Scrolling back down: the old rows 42..492 return to 0..450.
    assert_eq!(
        shared_backing_move_pieces(outer, None, &[], &[], parent, (13, -13), (13, 37)),
        vec![rect(0, 42, 730, 450)]
    );
    // A parent that is the whole backing: the whole window moves.
    assert_eq!(
        shared_backing_move_pieces(outer, None, &[], &[], None, (13, 37), (13, -13)),
        vec![outer]
    );
    // Shaped to its top 60 rows, as GDK clips it to the viewport: only
    // those rows are its to carry.
    let shape = [rect(0, 0, 730, 60)];
    assert_eq!(
        shared_backing_move_pieces(outer, Some(&shape), &[], &[], None, (13, 37), (13, -13)),
        vec![rect(0, 0, 730, 60)]
    );
}

#[test]
fn resolve_paint_target_descendant_accumulates_offset() {
    use crate::kms::render::store::{DrawableKind, Storage};
    let mut b = KmsBackend::for_tests();
    let w_id = seed_window(&mut b, 0x100, None, 0, 0);
    let _c_id = seed_window(&mut b, 0x200, Some(0x100), 10, 20);
    let _g_id = seed_window(&mut b, 0x300, Some(0x200), 3, 4);
    let b_id = b
        .store
        .allocate(
            0x900,
            DrawableKind::RedirectedBacking,
            32,
            false,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 200,
                    height: 200,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("backing allocate");
    b.store.set_redirected_target(w_id, Some(b_id));
    let pt = b.resolve_paint_target(0x300).expect("resolve");
    // Clipped to G ∩ C ∩ W, all 100x100: Xorg's clipList never leaves
    // the parent's.
    assert_eq!(
        pt,
        in_ancestor_backing(b_id, (13, 24), (13, 24, 87, 76), 24)
    );
}

/// Top-level whose `parent == Some(root_xid)` (root isn't in
/// `windows`) walks one step and finds root's redirect
/// state. With root un-redirected, paint stays on the leaf.
/// Regression for the resolver-returns-None bug that surfaced
/// when `subwindow_resize_clears_old_paint` started routing
/// `fill_rectangle` through `resolve_paint_target` and the
/// previous `windows.get(parent_xid)?` chain poisoned the
/// outer Option for any parent==root case.
#[test]
fn resolve_paint_target_parent_root_falls_back_to_identity() {
    let mut b = KmsBackend::for_tests();
    // root_xid is seeded via `KmsCore::for_tests()` and present
    // in the store (init_root_storage); but NOT in windows.
    let root_xid = b.core.window_id;
    assert!(!b.windows.contains_key(&root_xid));
    let w_id = seed_window(&mut b, 0x100, Some(root_xid), 0, 0);
    let pt = b.resolve_paint_target(0x100).expect("resolve");
    assert_eq!(pt, PaintTarget::new(w_id, (0, 0), None, 24));
}

/// Root itself can be the redirect target — a compositor that
/// runs `RedirectWindow(root, …)` sets `redirected_target` on
/// root's drawable. Paint against root or its descendants
/// resolves through the root-backing.
///
/// Codex round-7 finding: top-level windows are recorded with
/// `parent == None` (NOT `Some(root_xid)`) by
/// `create_subwindow` because root isn't tracked in
/// `windows`. The pre-fix resolver's `None` arm returned
/// identity without consulting root, so real top-level
/// descendants bypassed the root backing.
#[test]
fn resolve_paint_target_redirected_root_routes_descendants() {
    use crate::kms::render::store::{DrawableKind, Storage};
    let mut b = KmsBackend::for_tests();
    let root_xid = b.core.window_id;
    let root_id = b.store.lookup(root_xid).expect("root id");
    // Use the production representation: `parent = None`
    // marks a top-level whose host_parent is root_xid (see
    // `create_subwindow`'s `if !windows.contains_key →
    // parent = None` branch). Also seed a descendant whose
    // parent IS the top-level so we exercise the full walk.
    let _w_id = seed_window(&mut b, 0x100, None, 50, 60);
    let _c_id = seed_window(&mut b, 0x101, Some(0x100), 3, 4);
    let backing_id = b
        .store
        .allocate(
            0x900,
            DrawableKind::RedirectedBacking,
            32,
            false,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 800,
                    height: 600,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("backing allocate");
    b.store.set_redirected_target(root_id, Some(backing_id));
    // Root paint (direct) resolves through the leaf-level
    // pre-loop short-circuit.
    let pt_root = b.resolve_paint_target(root_xid).expect("resolve root");
    assert_eq!(
        pt_root,
        PaintTarget::new(
            backing_id,
            (0, 0),
            None,
            // Painting directly on root reports ROOT's logical depth (32,
            // the framebuffer depth), not a child window's 24.
            32,
        )
    );
    // Top-level (parent=None production rep) must walk into
    // root's redirect with its own (x, y) accumulated.
    let pt_w = b.resolve_paint_target(0x100).expect("resolve W");
    assert_eq!(
        pt_w,
        in_ancestor_backing(backing_id, (50, 60), (50, 60, 100, 100), 24)
    );
    // Descendant of a top-level: accumulates C-in-W (3, 4)
    // then W-in-root (50, 60) → (53, 64), clipped to W.
    let pt_c = b.resolve_paint_target(0x101).expect("resolve C");
    assert_eq!(
        pt_c,
        in_ancestor_backing(backing_id, (53, 64), (53, 64, 97, 96), 24)
    );
}

/// Plan §4a (Tests, line 644-646): clearing a redirect via
/// `set_redirected_target(W, None)` falls back to leaf-storage
/// routing. The store-level `set_redirected_target_none_clears_route`
/// verifies the field is cleared; this end-to-end check
/// asserts the resolver flow honours that. Catches a regression
/// where a missing branch / wrong `?` could special-case the
/// cleared state.
#[test]
fn resolve_paint_target_after_clear_falls_back_to_identity() {
    use crate::kms::render::store::{DrawableKind, Storage};
    let mut b = KmsBackend::for_tests();
    let w_id = seed_window(&mut b, 0x100, None, 0, 0);
    let backing_id = b
        .store
        .allocate(
            0x900,
            DrawableKind::RedirectedBacking,
            32,
            false,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 100,
                    height: 100,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("backing allocate");
    // Install the redirect, then immediately clear it.
    b.store.set_redirected_target(w_id, Some(backing_id));
    b.store.set_redirected_target(w_id, None);
    let pt = b.resolve_paint_target(0x100).expect("resolve");
    assert_eq!(
        pt,
        PaintTarget::new(w_id, (0, 0), None, 24),
        "cleared redirect must fall through to leaf identity",
    );
}

/// Nearest redirected ancestor wins. W→B_W and C→B_C both
/// redirected; grandchild G under C must route to B_C with
/// the C-relative offset, NOT to B_W with the
/// W-relative offset.
#[test]
fn resolve_paint_target_stops_at_nearest_redirected_ancestor() {
    use crate::kms::render::store::{DrawableKind, Storage};
    let mut b = KmsBackend::for_tests();
    let w_id = seed_window(&mut b, 0x100, None, 0, 0);
    let c_id = seed_window(&mut b, 0x200, Some(0x100), 10, 20);
    let _g_id = seed_window(&mut b, 0x300, Some(0x200), 3, 4);
    let bw_id = b
        .store
        .allocate(
            0x900,
            DrawableKind::RedirectedBacking,
            32,
            false,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 200,
                    height: 200,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("B_W");
    let bc_id = b
        .store
        .allocate(
            0x901,
            DrawableKind::RedirectedBacking,
            32,
            false,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 100,
                    height: 100,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("B_C");
    b.store.set_redirected_target(w_id, Some(bw_id));
    b.store.set_redirected_target(c_id, Some(bc_id));
    let pt = b.resolve_paint_target(0x300).expect("resolve");
    assert_eq!(pt, in_ancestor_backing(bc_id, (3, 4), (3, 4, 97, 96), 24));
}

/// XOR-safe dedup contract for `IncludeInferiors` stroke collection.
/// Tree: root -> W (redirected to backing B) -> C (NOT redirected).
/// Both W and C resolve to B. A root stroke crossing both must yield
/// backing B EXACTLY ONCE (C is covered by W's entry) so a GXinvert
/// pass over B's pixels does not cancel itself.
#[test]
fn stroke_inferior_targets_dedup_redirected_ancestor() {
    use crate::kms::render::store::{DrawableKind, Storage};
    let mut b = KmsBackend::for_tests();
    let root = b.core.window_id;
    // W: top-level (parent = None production rep) at root origin.
    let _w_id = seed_window(&mut b, 0x100, None, 0, 0);
    // C: non-redirected child of W, placed at W-local (10, 0) so the
    // stroke's top edge crosses it as well as W.
    let _c_id = seed_window(&mut b, 0x200, Some(0x100), 10, 0);
    let backing_id = b
        .store
        .allocate(
            0x900,
            DrawableKind::RedirectedBacking,
            32,
            false,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 100,
                    height: 100,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("backing allocate");
    assert!(b.test_set_redirected_target(0x100, 0x900));

    // Stroke rects in root-local coords, crossing both W and C.
    let rects = vec![Rectangle16 {
        x: 0,
        y: 0,
        width: 100,
        height: 2,
    }];
    let targets = b.collect_stroke_inferior_targets(root, &rects);

    let hits: Vec<_> = targets
        .iter()
        .filter(|(t, _)| t.backing_id() == backing_id)
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "redirected backing B must appear exactly once (XOR-safe), got {}",
        hits.len()
    );
}

/// A descendant window can temporarily lose its xid→DrawableId
/// mapping while its geometry remains live (e.g. resize /
/// reparent churn). Paint should still route into the nearest
/// redirected ancestor backing using the window-tree offsets,
/// rather than failing the leaf lookup and dropping the op.
#[test]
fn resolve_paint_target_detached_leaf_still_routes_to_redirected_ancestor() {
    use crate::kms::render::store::{DrawableKind, Storage};
    let mut b = KmsBackend::for_tests();
    let w_id = seed_window(&mut b, 0x100, None, 0, 0);
    let _c_id = seed_window(&mut b, 0x200, Some(0x100), 10, 20);
    let backing_id = b
        .store
        .allocate(
            0x900,
            DrawableKind::RedirectedBacking,
            32,
            false,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 200,
                    height: 200,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("backing allocate");
    b.store.set_redirected_target(w_id, Some(backing_id));
    b.store.detach_xid(0x200);

    let pt = b.resolve_paint_target(0x200).expect("resolve");
    assert_eq!(
        pt,
        in_ancestor_backing(backing_id, (10, 20), (10, 20, 90, 80), 24)
    );
}

/// A depth-24 child painting into a depth-32 redirected backing must keep
/// depth-24 X11 semantics: plane-mask `0x00ff_ffff` is the full mask and
/// the stored alpha must be forced opaque. Regression for the picom+xterm
/// bug where redirected depth-24 text drew transparent rows into the
/// depth-32 frame backing.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn redirected_depth24_fill_into_depth32_backing_forces_opaque_alpha() {
    use yserver_core::backend::DrawState;

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let host_xid = b
        .allocate_test_pixmap_bgra(8, 8)
        .expect("allocate_test_pixmap_bgra");
    let target_id = b.store.lookup(host_xid).expect("target id");

    let draw_state = DrawState {
        plane_mask: 0x00ff_ffff,
        ..DrawState::default()
    };
    b.apply_draw_state(None, &draw_state)
        .expect("apply_draw_state");
    b.fill_solid_rects(
        PaintTarget::new(target_id, (0, 0), None, 24),
        0x00ff_0000,
        &[Rectangle16 {
            x: 0,
            y: 0,
            width: 8,
            height: 8,
        }],
    );
    b.engine_close_open_frame_for_timeout_for_tests()
        .expect("close open frame");
    b.engine_drain_all_for_tests();

    let bytes = b
        .engine
        .get_image(
            &mut b.store,
            &mut b.platform,
            crate::kms::render::target::Src::server_internal(target_id),
            ash::vk::Rect2D {
                offset: ash::vk::Offset2D::default(),
                extent: ash::vk::Extent2D {
                    width: 8,
                    height: 8,
                },
            },
            32,
        )
        .expect("get_image");
    assert_eq!(
        bytes[3], 0xff,
        "depth-24 paint routed into a depth-32 backing must store opaque alpha"
    );
}

// ────────────────────────────────────────────────────────────────
// Stage 4c.2 — `window_absolute_rect` helper
// ────────────────────────────────────────────────────────────────

/// Top-level W at (50, 60) size 100×80, parent=None. Absolute
/// rect echoes its own (x, y, w, h) — there's no ancestor to
/// accumulate through.
#[test]
fn window_absolute_rect_top_level() {
    let mut b = KmsBackend::for_tests();
    let w_id = seed_window(&mut b, 0x100, None, 50, 60);
    // `seed_window` hard-codes 100×100; resize via the geom entry.
    b.windows.get_mut(&0x100).unwrap().width = 100;
    b.windows.get_mut(&0x100).unwrap().height = 80;
    let rect = b.window_absolute_rect(w_id).expect("rect");
    assert_eq!(
        rect,
        ash::vk::Rect2D {
            offset: ash::vk::Offset2D { x: 50, y: 60 },
            extent: ash::vk::Extent2D {
                width: 100,
                height: 80
            },
        }
    );
}

/// Three-level chain: W(50, 60) → C(10, 20) → G(3, 4) size 8×8.
/// G's absolute rect is at (63, 84) with G's own 8×8 extent.
#[test]
fn window_absolute_rect_descendant() {
    let mut b = KmsBackend::for_tests();
    let _w_id = seed_window(&mut b, 0x100, None, 50, 60);
    let _c_id = seed_window(&mut b, 0x200, Some(0x100), 10, 20);
    let g_id = seed_window(&mut b, 0x300, Some(0x200), 3, 4);
    // `seed_window` defaults C/G to 100×100; shrink to plan sizes.
    {
        let c = b.windows.get_mut(&0x200).unwrap();
        c.width = 30;
        c.height = 30;
    }
    {
        let g = b.windows.get_mut(&0x300).unwrap();
        g.width = 8;
        g.height = 8;
    }
    let rect = b.window_absolute_rect(g_id).expect("rect");
    assert_eq!(
        rect,
        ash::vk::Rect2D {
            offset: ash::vk::Offset2D { x: 63, y: 84 },
            extent: ash::vk::Extent2D {
                width: 8,
                height: 8
            },
        }
    );
}

/// `DrawableId` that the store no longer knows about → None.
/// Allocate a window then `decref` it down to retirement so
/// the id no longer resolves; `store.get` returns None and the
/// helper short-circuits without poking `windows`.
#[test]
fn window_absolute_rect_unknown_drawable_returns_none() {
    let mut b = KmsBackend::for_tests();
    let w_id = seed_window(&mut b, 0x100, None, 50, 60);
    // Tear it back down so the DrawableId no longer resolves.
    // `decref` with no Vk treats the ticket as signaled and
    // calls `destroy_now`, removing the id from `entries`.
    let _ = b.store.decref(&mut b.platform, w_id, |_| {});
    // Also clear the windows entry — otherwise the helper
    // would early-return on the xid lookup, not the id lookup
    // we want to exercise.
    b.windows.remove(&0x100);
    assert!(b.store.get(w_id).is_none());
    assert_eq!(b.window_absolute_rect(w_id), None);
}

/// Pixmaps live in the store but not in `windows`. The
/// helper has no geometry to walk → None.
#[test]
fn window_absolute_rect_pixmap_returns_none() {
    use crate::kms::render::store::{DrawableKind, Storage};
    let mut b = KmsBackend::for_tests();
    let pix_id = b
        .store
        .allocate(
            0x2000,
            DrawableKind::Pixmap,
            32,
            false,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 64,
                    height: 64,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("pixmap allocate");
    assert_eq!(b.window_absolute_rect(pix_id), None);
}

/// Dangling parent xid: W has `parent = Some(0xDEAD)` and
/// 0xDEAD is neither root nor in `windows`. Conservative
/// choice per plan: bail with None rather than return a
/// half-accumulated rect that callers can't act on.
#[test]
fn window_absolute_rect_dangling_parent_returns_none() {
    let mut b = KmsBackend::for_tests();
    let w_id = seed_window(&mut b, 0x100, Some(0xDEAD), 50, 60);
    // 0xDEAD is not in windows and is not root_xid.
    assert!(!b.windows.contains_key(&0xDEAD));
    assert_ne!(b.core.window_id, 0xDEAD);
    assert_eq!(b.window_absolute_rect(w_id), None);
}

// ────────────────────────────────────────────────────────────────
// Stage 4c.4 — set_window_scene_participation /
// set_backing_scene_participation
// ────────────────────────────────────────────────────────────────

/// `participating=false` on a window with pending presentation
/// damage must delegate to `DrawableStore::set_scene_participating`
/// — that store method clears the damage and bumps the epoch.
/// This verifies the v2 backend actually wires the call (rather
/// than e.g. silently returning Ok).
#[test]
fn set_window_scene_participation_false_clears_window_damage() {
    use yserver_core::backend::WindowHandle;
    let mut b = KmsBackend::for_tests();
    let w_id = seed_window(&mut b, 0x100, None, 0, 0);
    // Seed presentation damage so the store actually has work
    // to clear when participation flips off.
    b.store.damage(
        w_id,
        ash::vk::Rect2D {
            offset: ash::vk::Offset2D::default(),
            extent: ash::vk::Extent2D {
                width: 4,
                height: 4,
            },
        },
    );
    assert_eq!(
        b.store.get(w_id).unwrap().presentation_damage.rects().len(),
        1,
    );
    let epoch_before = b.store.get(w_id).unwrap().presentation_damage_epoch;

    let handle = WindowHandle::from_raw(0x100).expect("WindowHandle");
    b.set_window_scene_participation(None, handle, false)
        .expect("set_window_scene_participation");

    let d = b.store.get(w_id).expect("drawable still alive");
    assert!(
        d.presentation_damage.is_empty(),
        "presentation_damage must clear on participating=false transition: {:?}",
        d.presentation_damage.rects(),
    );
    assert!(
        d.presentation_damage_epoch > epoch_before,
        "epoch must bump on participating=false transition (before={epoch_before}, after={})",
        d.presentation_damage_epoch,
    );
    assert!(
        !d.scene_participating,
        "scene_participating flag must be cleared",
    );
}

/// `set_window_scene_participation` must fire scene-structure
/// damage for the redirect transition. On the stub-mode scene
/// (`for_tests` fixture has `inner: None`), we can only observe
/// the `scene_structure_dirty` bit — the per-output rect
/// dispatch is covered in `scene::tests::
/// dispatch_clip_rects_lands_per_output_clipped` (4c.1 follow-up).
/// This test pins the contract that the backend CALLS the rect
/// setter (or the coarse fallback) rather than leaving the
/// scene-structure state untouched.
#[test]
fn set_window_scene_participation_fires_scene_structure_damage_rect() {
    use yserver_core::backend::WindowHandle;
    let mut b = KmsBackend::for_tests();
    let _w_id = seed_window(&mut b, 0x100, None, 50, 60);
    // Sanity: pre-flip rect lookup is non-None (Test 2 requires
    // the rect path, not the coarse fallback path).
    let pre_flip = b
        .window_absolute_rect(b.store.lookup(0x100).unwrap())
        .expect("pre-flip rect known");
    assert_eq!(pre_flip.offset.x, 50);
    assert_eq!(pre_flip.offset.y, 60);

    // Start with the dirty bit cleared so the assertion proves
    // THIS call set it (not some setup side effect).
    b.scene.scene_structure_dirty = false;

    let handle = WindowHandle::from_raw(0x100).expect("WindowHandle");
    b.set_window_scene_participation(None, handle, false)
        .expect("set_window_scene_participation");

    assert!(
        b.scene.scene_structure_dirty,
        "scene_structure_dirty must be set after a participation flip",
    );
}

/// `set_backing_scene_participation` flips the backing's
/// `scene_participating` flag via the store but must NOT fire
/// scene-structure damage — geometric damage is the W-side
/// call's responsibility (backings have no on-screen geometry
/// of their own).
#[test]
fn set_backing_scene_participation_flips_flag_no_damage() {
    use crate::kms::render::store::{DrawableKind, Storage};
    use yserver_core::backend::PixmapHandle;
    let mut b = KmsBackend::for_tests();
    let b_id = b
        .store
        .allocate(
            0x2000,
            DrawableKind::Pixmap,
            32,
            false,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 64,
                    height: 64,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("pixmap allocate");
    // Pixmaps start with scene_participating=false (per
    // `DrawableStore::allocate`'s `scene_participating` arg).
    assert!(!b.store.get(b_id).unwrap().scene_participating);
    // Capture the prior dirty bit (whatever setup left it at).
    // The assertion below is "no CHANGE", not "is false".
    let dirty_before = b.scene.scene_structure_dirty;

    let handle = PixmapHandle::from_raw(0x2000).expect("PixmapHandle");
    b.set_backing_scene_participation(None, handle, true)
        .expect("set_backing_scene_participation");

    assert!(
        b.store.get(b_id).unwrap().scene_participating,
        "backing scene_participating must flip to true",
    );
    assert_eq!(
        b.scene.scene_structure_dirty, dirty_before,
        "set_backing_scene_participation must NOT fire scene-structure damage",
    );
}

// ────────────────────────────────────────────────────────────────
// Stage 4c.5 — Manual-redirect lifecycle through the Backend
// surface (deferred from 4b.9 / Stage 4c plan §"Tests Vk-backed").
//
// These exercise the no-Vk pathway: `allocate_redirected_backing`
// skips the store-side wiring when no Vk is attached (the
// `create_pixmap` fallback doesn't seed a store entry for the
// backing — see backend.rs:3214 `create_pixmap_no_vk`), but the
// `alias_registry.insert` + `host_window_to_backing.insert` still
// fire. That's enough for the participation-flip assertions to
// observe `scene_structure_dirty`.
//
// The per-output rect dispatch goes through scene.rs:412's
// stub-mode guard — `dispatch_clip_rects_lands_per_output_clipped`
// covers that branch directly.
// ────────────────────────────────────────────────────────────────

/// Simulate `RedirectWindow(W, Manual)`: allocate the backing,
/// then flip W to `scene_participating=false`. The participation
/// flip MUST fire scene-structure damage so the next composite
/// repaints the region W used to occupy (under Manual mode the
/// scene drops W; whatever's underneath must redraw).
#[test]
fn manual_redirect_path_marks_scene_structure_damage() {
    use yserver_core::backend::WindowHandle;
    let mut b = KmsBackend::for_tests();
    let _w_id = seed_window(&mut b, 0x100, None, 30, 40);
    let w = WindowHandle::from_raw(0x100).expect("WindowHandle");

    // Step 1: allocate the backing. On no-Vk the store-side
    // wiring is skipped (logged as a warn) but the alias-registry
    // + host_window_to_backing entries install. That's enough
    // for the protocol-side state machine; scene-structure
    // damage comes from the next call.
    let _backing = b
        .allocate_redirected_backing(None, w, 100, 100, 32)
        .expect("allocate_redirected_backing");

    // Clear the dirty bit so the post-flip assertion proves
    // the participation call set it, not the allocation above.
    b.scene.scene_structure_dirty = false;

    // Step 2: flip W to non-participating (Manual activation).
    b.set_window_scene_participation(None, w, false)
        .expect("set_window_scene_participation(false)");

    assert!(
        b.scene.scene_structure_dirty,
        "Manual-redirect participation flip (W→false) must fire \
             scene-structure damage so the region W used to occupy \
             gets repainted by whatever's underneath",
    );
}

/// Full Manual-redirect lifecycle: activate (Manual), then
/// un-redirect. Both transitions must fire scene-structure
/// damage. Clear the dirty bit between the two flips so the
/// final assertion proves the SECOND call set it independently.
#[test]
fn unredirect_restores_participation_and_marks_damage() {
    use yserver_core::backend::WindowHandle;
    let mut b = KmsBackend::for_tests();
    let _w_id = seed_window(&mut b, 0x100, None, 30, 40);
    let w = WindowHandle::from_raw(0x100).expect("WindowHandle");

    // Manual activation: allocate + flip W off-scene.
    let backing = b
        .allocate_redirected_backing(None, w, 100, 100, 32)
        .expect("allocate_redirected_backing");
    b.set_window_scene_participation(None, w, false)
        .expect("set_window_scene_participation(false)");
    assert!(
        b.scene.scene_structure_dirty,
        "fixture sanity: Manual activation already fires scene-structure damage \
             (covered by manual_redirect_path_marks_scene_structure_damage)",
    );

    // Clear so the post-un-redirect assertion is sharp.
    b.scene.scene_structure_dirty = false;

    // Un-redirect: drop the backing hold and flip W back on-scene.
    b.release_redirected_backing(None, backing)
        .expect("release_redirected_backing");
    // `release_redirected_backing` doesn't touch W's scene flag;
    // un-redirect-to-mapped is the W-side caller's responsibility.
    b.set_window_scene_participation(None, w, true)
        .expect("set_window_scene_participation(true)");

    assert!(
        b.scene.scene_structure_dirty,
        "Un-redirect participation flip (W→true) must ALSO fire \
             scene-structure damage so W's region gets composited \
             back into the scene from W's own storage",
    );

    // Sanity: W is back to participating; the backing's
    // alias-registry entry is gone (release dropped Reason-1
    // and there were no aliases).
    let w_id = b.store.lookup(0x100).expect("w still in store");
    assert!(
        b.store.get(w_id).unwrap().scene_participating,
        "W must end in scene_participating=true after un-redirect",
    );
    assert!(
        b.test_alias_registry_get(backing.as_raw()).is_none(),
        "backing alias-registry entry must be cleared after release_redirected_backing",
    );
    assert!(
        b.test_host_window_to_backing(0x100).is_none(),
        "host_window_to_backing must be cleared after release",
    );
}

#[test]
fn configure_subwindow_redirected_resize_skips_leaf_realloc() {
    use yserver_core::{backend::Backend, host_x11::HostSubwindowConfig};

    let mut b = KmsBackend::for_tests();
    let _w_id = seed_window(&mut b, 0x100, None, 30, 40);
    let leaf_before = b.store.lookup(0x100).expect("leaf before");
    let extent_before = b.store.get(leaf_before).unwrap().storage.extent;
    let backing_id = seed_backing_drawable(&mut b, 0x900);
    b.store.set_redirected_target(leaf_before, Some(backing_id));
    b.configure_subwindow(
        None,
        0x100,
        HostSubwindowConfig {
            x: None,
            y: None,
            width: Some(180),
            height: Some(140),
            border_width: None,
            sibling: None,
            stack_mode: None,
        },
    )
    .expect("configure_subwindow");

    let leaf_after = b.store.lookup(0x100).expect("leaf after");
    let extent_after = b.store.get(leaf_after).unwrap().storage.extent;
    assert_eq!(
        leaf_after, leaf_before,
        "redirected resize must not churn the hidden leaf DrawableId",
    );
    assert_eq!(
        extent_after, extent_before,
        "redirected resize must leave hidden leaf storage untouched",
    );
    assert_eq!(b.windows.get(&0x100).unwrap().width, 180);
    assert_eq!(b.windows.get(&0x100).unwrap().height, 140);
}

#[test]
fn unredirect_reconciles_leaf_storage_after_deferred_redirected_resize() {
    use yserver_core::{
        backend::{Backend, WindowHandle},
        host_x11::HostSubwindowConfig,
    };

    let mut b = KmsBackend::for_tests();
    let _w_id = seed_window(&mut b, 0x100, None, 30, 40);
    let w = WindowHandle::from_raw(0x100).expect("WindowHandle");

    let backing = b
        .allocate_redirected_backing(None, w, 100, 100, 32)
        .expect("allocate_redirected_backing");
    let leaf_id = b.store.lookup(0x100).expect("leaf id");
    let backing_id = seed_backing_drawable(&mut b, backing.as_raw());
    b.store.set_redirected_target(leaf_id, Some(backing_id));
    b.configure_subwindow(
        None,
        0x100,
        HostSubwindowConfig {
            x: None,
            y: None,
            width: Some(180),
            height: Some(140),
            border_width: None,
            sibling: None,
            stack_mode: None,
        },
    )
    .expect("configure_subwindow");

    let leaf_during_redirect = b.store.lookup(0x100).expect("leaf during redirect");
    let extent_during_redirect = b.store.get(leaf_during_redirect).unwrap().storage.extent;
    assert_eq!(extent_during_redirect.width, 100);
    assert_eq!(extent_during_redirect.height, 100);

    b.release_redirected_backing(None, backing)
        .expect("release_redirected_backing");

    let leaf_after = b.store.lookup(0x100).expect("leaf after unredirect");
    let extent_after = b.store.get(leaf_after).unwrap().storage.extent;
    assert_eq!(extent_after.width, 180);
    assert_eq!(extent_after.height, 140);
}

/// #143 (rendering half) — the reuse test is an EQUALITY on the
/// backing's STORAGE extent.
///
/// Until 2026-09-16 this test asserted `can_fit(180, 140) == true`
/// against 200x150 storage: it PINNED the high-water-mark reuse that
/// turned out to be the bug. A shrink kept the oversized backing,
/// `update_redirected_backing_geometry` moved the logical alias
/// geometry, and nothing re-laid the border ring or re-seeded the
/// storage — measured under awesome+picom as a backing left at
/// 1282x708 (ring at columns 1280..1281) for a window that had
/// shrunk to 1276 content-pixels wide, with an alpha-0 band where
/// content should be. Xorg reallocates on inequality in EITHER
/// direction: `compReallocPixmap` compares `pix_w !=
/// pOld->drawable.width || pix_h != pOld->drawable.height`
/// (../xserver/composite/compalloc.c:698).
///
/// The extents below are those measured numbers; `width`/`height`
/// are bordered extents (`bw = 2`).
#[test]
fn redirected_backing_reuse_requires_the_exact_storage_extent() {
    use crate::kms::{
        core::AliasEntry,
        render::store::{DrawableKind, Storage},
    };
    use yserver_core::backend::{Backend, PixmapHandle};

    let mut b = KmsBackend::for_tests();
    let backing = PixmapHandle::from_raw_panicking(0x9000_0001);
    let id = b
        .store
        .allocate(
            backing.as_raw(),
            DrawableKind::RedirectedBacking,
            32,
            false,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 1282,
                    height: 708,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("backing allocate");
    b.core.alias_registry.insert(
        backing,
        AliasEntry {
            refcount: 1,
            width: 1282,
            height: 708,
            depth: 32,
        },
    );
    let storage_extent = |b: &KmsBackend| b.store.get(id).expect("backing drawable").storage.extent;
    assert_eq!(storage_extent(&b).width, 1282);
    assert_eq!(storage_extent(&b).height, 708);

    // The shrink that regressed: the storage is big ENOUGH, but it is
    // not the right size, so it must NOT be reused.
    assert!(
        !b.redirected_backing_can_fit(backing, 1280, 708, 32),
        "a shrink must reallocate — an oversized backing still has its \
             ring and its parent seed laid out for the OLD extent",
    );
    // The grow direction rotated before #143 too; keep it that way.
    assert!(!b.redirected_backing_can_fit(backing, 1284, 708, 32));
    assert!(!b.redirected_backing_can_fit(backing, 1282, 712, 32));
    // Exact match: reuse, no reallocation.
    assert!(b.redirected_backing_can_fit(backing, 1282, 708, 32));
    // Depth is still part of the test.
    assert!(!b.redirected_backing_can_fit(backing, 1282, 708, 24));

    // What a TRUE verdict authorises is a metadata-only update — and
    // it provably does NOT touch storage. That asymmetry is why a
    // test reading only the alias geometry would have passed against
    // the buggy predicate, and why the assertions above read the
    // store's extent instead.
    b.update_redirected_backing_geometry(None, backing, 1280, 708, 32)
        .expect("update logical geometry");
    let alias = b
        .test_alias_registry_get(backing.as_raw())
        .expect("alias entry");
    assert_eq!(alias.width, 1280);
    assert_eq!(alias.height, 708);
    assert_eq!(alias.depth, 32);
    assert_eq!(
        storage_extent(&b).width,
        1282,
        "the metadata path moves the logical geometry and leaves the \
             storage at its allocated extent",
    );
}

// ────────────────────────────────────────────────────────────────
// 2026-06-11 — IncludeInferiors backing-seed planner
// (`plan_backing_inferiors`). The compiz --replace half-drawn-panel
// fix: on re-redirect the fresh backing must be seeded from the
// window's OWN content, not just the parent/wallpaper layer.
// ────────────────────────────────────────────────────────────────

/// Allocate a sized store drawable to stand in for the backing
/// (no-Vk `allocate_redirected_backing` skips store wiring, so the
/// extent gate in `plan_backing_inferiors` needs a real entry).
fn seed_backing_drawable(b: &mut KmsBackend, xid: u32) -> crate::kms::render::store::DrawableId {
    use crate::kms::render::store::{DrawableKind, Storage};
    b.store
        .allocate(
            xid,
            DrawableKind::Pixmap,
            32,
            true,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 100,
                    height: 100,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("seed_backing_drawable allocate")
}

/// The plan must include the redirected window itself first, then
/// its mapped descendants bottom-to-top by `stack_rank`, with
/// offsets accumulated relative to W (== the backing origin).
#[test]
fn plan_backing_inferiors_walks_subtree_in_stack_order() {
    let mut b = KmsBackend::for_tests();
    let _w = seed_window(&mut b, 0x100, None, 0, 0);
    let _c_top = seed_window(&mut b, 0x200, Some(0x100), 10, 10);
    let _c_bot = seed_window(&mut b, 0x300, Some(0x100), 20, 20);
    b.windows.get_mut(&0x200).unwrap().stack_rank = 5; // topmost
    b.windows.get_mut(&0x300).unwrap().stack_rank = 1; // bottom
    let b_id = seed_backing_drawable(&mut b, 0x999);

    let plan = b.plan_backing_inferiors(0x100, b_id);
    let order: Vec<(u32, i32, i32)> = plan
        .iter()
        .map(|d| (b.store.get(d.leaf_id).unwrap().xid, d.dst_x, d.dst_y))
        .collect();
    assert_eq!(
        order,
        vec![(0x100, 0, 0), (0x300, 20, 20), (0x200, 10, 10)],
        "expected W first, then children bottom-to-top by stack_rank, \
             offsets relative to W",
    );
}

/// A descendant that owns its own `redirected_target` (an
/// independently-redirected child — systray icons) and its whole
/// subtree must be PRUNED: the compositor composites that child's
/// backing separately, so its pixels must not be baked into W's.
#[test]
fn plan_backing_inferiors_prunes_independently_redirected_descendant() {
    let mut b = KmsBackend::for_tests();
    let _w = seed_window(&mut b, 0x100, None, 0, 0);
    let c = seed_window(&mut b, 0x200, Some(0x100), 10, 10);
    let _gc = seed_window(&mut b, 0x300, Some(0x200), 5, 5);
    // C is independently redirected (any Some target triggers prune).
    b.store.set_redirected_target(c, Some(c));
    let b_id = seed_backing_drawable(&mut b, 0x999);

    let plan = b.plan_backing_inferiors(0x100, b_id);
    let xids: Vec<u32> = plan
        .iter()
        .map(|d| b.store.get(d.leaf_id).unwrap().xid)
        .collect();
    assert_eq!(
        xids,
        vec![0x100],
        "C (independently redirected) and its grandchild must be pruned; only W remains",
    );
}

/// An unmapped window and its whole subtree are invisible and must
/// not appear in the plan.
#[test]
fn plan_backing_inferiors_skips_unmapped_subtree() {
    let mut b = KmsBackend::for_tests();
    let _w = seed_window(&mut b, 0x100, None, 0, 0);
    let _c = seed_window(&mut b, 0x200, Some(0x100), 10, 10);
    let _gc = seed_window(&mut b, 0x300, Some(0x200), 5, 5);
    b.windows.get_mut(&0x200).unwrap().mapped = false;
    let b_id = seed_backing_drawable(&mut b, 0x999);

    let plan = b.plan_backing_inferiors(0x100, b_id);
    let xids: Vec<u32> = plan
        .iter()
        .map(|d| b.store.get(d.leaf_id).unwrap().xid)
        .collect();
    assert_eq!(
        xids,
        vec![0x100],
        "unmapped child 0x200 and its subtree (0x300) must be excluded",
    );
}

// ────────────────────────────────────────────────────────────────
// Stage 4d — Composite Overlay Window (COW) lifecycle.
//
// These exercise the no-Vk pathway: `allocate_drawable_storage`
// returns `ERROR_INITIALIZATION_FAILED` on `for_tests()`; the
// get_overlay_window override falls back to a `Storage::for_tests_null`
// stub so the store-side wiring (xid mapping, scene registration) is
// still exercised. The Vk-backed test in `tests/acceptance.rs` covers
// the actual paint+scanout path.
//
// The backend sees only two edges — materialize and final teardown —
// and counts nothing: `ServerState::cow_claims` in core is the single
// authority on how many holds the overlay has. Pairing, ownership and
// non-final releases are proved at the handler level in
// `yserver-core/src/core_loop/process_request.rs`.
// ────────────────────────────────────────────────────────────────

/// Materialize edge: COW xid resolves in store, backend `cow_id` set.
#[test]
fn cow_get_overlay_first_call_allocates_storage() {
    let mut b = KmsBackend::for_tests();
    assert!(b.cow_id.is_none());
    // Pre-flight: COW xid is NOT in the store yet.
    assert!(
        b.store
            .lookup(yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0)
            .is_none(),
        "COW xid must not resolve before GetOverlayWindow",
    );

    b.get_overlay_window(None).expect("get_overlay_window");

    assert!(b.cow_id.is_some(), "backend.cow_id must be set after GET");
    assert!(
        b.store
            .lookup(yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0)
            .is_some(),
        "COW xid must resolve in the store after GetOverlayWindow",
    );
    // Storage shape: depth-24 screen-extent, scene-participating,
    // DrawableKind::Window so build_scene's window-kind gating
    // doesn't filter it.
    let cow_id = b.cow_id.expect("cow_id set");
    let cow = b.store.get(cow_id).expect("cow drawable");
    assert_eq!(cow.depth, 24, "COW must be depth-24");
    assert!(
        cow.scene_participating,
        "COW must be scene_participating=true so build_scene includes it",
    );
    assert!(
        matches!(cow.kind, crate::kms::render::store::DrawableKind::Window),
        "COW must be DrawableKind::Window",
    );
    assert_eq!(cow.storage.extent.width, u32::from(b.platform.fb_w));
    assert_eq!(cow.storage.extent.height, u32::from(b.platform.fb_h));
}

#[test]
fn note_present_pixmap_tracks_non_cow_stage_sources_for_drawable_dump() {
    let mut b = KmsBackend::for_tests();
    let stage = b
        .store
        .allocate(
            0x4000_2000,
            crate::kms::render::store::DrawableKind::Window,
            32,
            true,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 800,
                    height: 600,
                },
                PlatformBackend::format_for_depth(32),
            ),
        )
        .expect("allocate stage");
    let _src = b
        .store
        .allocate(
            0x4000_3000,
            crate::kms::render::store::DrawableKind::Pixmap,
            32,
            true,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 800,
                    height: 600,
                },
                PlatformBackend::format_for_depth(32),
            ),
        )
        .expect("allocate src pixmap");
    assert!(b.store.get(stage).is_some(), "stage must exist");

    b.note_present_pixmap(0x4000_3000, 0x4000_2000);

    assert_eq!(
        b.recent_present_pixmaps.back(),
        Some(&(0x4000_3000, 0x4000_2000)),
        "non-COW PresentPixmap must still land in the general diagnostic ring",
    );
}

/// Teardown edge drops the storage. `cow_id` clears; xid no longer
/// resolves in the store (so a fresh `GetOverlayWindow` reallocates
/// clean — the protocol guarantees the COW xid is reusable after
/// every release-to-zero).
#[test]
fn cow_final_release_drops_storage() {
    let mut b = KmsBackend::for_tests();
    b.get_overlay_window(None).expect("get");

    let tore_down = b.release_overlay_window(None).expect("release");
    assert!(
        tore_down,
        "release_overlay_window must report Ok(true) when it destroyed a \
             COW, so core mirrors the resources-side record down with it",
    );

    assert!(b.cow_id.is_none(), "cow_id must clear on the teardown edge");
    assert!(
        b.store
            .lookup(yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0)
            .is_none(),
        "COW xid must NOT resolve after the final release — \
             the store has destroyed (or detached) the entry so a \
             subsequent GetOverlayWindow can reallocate at the \
             same xid",
    );

    // The edges round-trip: nothing on the backend remembers the
    // previous claim, because the backend keeps no count at all.
    b.get_overlay_window(None)
        .expect("re-get after final release");
    assert!(b.cow_id.is_some());
}

/// Defensive branch: a teardown with nothing materialized (core and
/// backend somehow out of step) must be a clean `Ok(false)` no-op —
/// `cow_id` stays `None` and the scene's COW entry stays
/// unregistered.
#[test]
fn cow_release_without_prior_get_is_noop() {
    let mut b = KmsBackend::for_tests();
    assert!(b.cow_id.is_none());

    let tore_down = b.release_overlay_window(None).expect("noop release");
    assert!(
        !tore_down,
        "with nothing materialized there is nothing for core to mirror \
             down, so the backend reports Ok(false)",
    );

    assert!(
        b.cow_id.is_none(),
        "unmatched release must NOT spuriously set cow_id",
    );
    // Subsequent get_overlay_window still works (defensive
    // branch hasn't poisoned any state).
    b.get_overlay_window(None).expect("get after noop release");
    assert!(b.cow_id.is_some());
}

/// Allocate a redirected backing for an already-seeded window
/// and install `set_redirected_target(W, Some(B))` so the
/// resolver routes paint against `W`'s host xid through `B`.
/// Also sets the resource-layer `redirected_backing` so the
/// reconciliation predicates in production code see the
/// "backing already present" state.
fn seed_redirected_backing(
    state: &mut yserver_core::server::ServerState,
    backend: &mut KmsBackend,
    xid: yserver_protocol::x11::ResourceId,
) {
    let host_xid = synth_host_xid(xid);
    let backing_xid = 0x9000_0000 | xid.0;
    let (width, height, depth) = state
        .resources
        .window(xid)
        .map(|w| (w.width, w.height, w.depth))
        .expect("seed_redirected_backing: window must exist");
    // Allocate the backing in the v2 store.
    let backing_id = backend
        .store
        .allocate(
            backing_xid,
            crate::kms::render::store::DrawableKind::RedirectedBacking,
            depth,
            false,
            crate::kms::render::store::Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: u32::from(width.max(1)),
                    height: u32::from(height.max(1)),
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("seed_redirected_backing allocate");
    let w_id = backend
        .store
        .lookup(host_xid)
        .expect("window's drawable id");
    backend.store.set_redirected_target(w_id, Some(backing_id));
    // Mirror on the resource layer so the production
    // reconciliation predicates see a backing present.
    if let Some(w) = state.resources.window_mut(xid) {
        w.redirected_backing = Some(yserver_core::resources::RedirectedBacking {
            host_pixmap: yserver_core::backend::PixmapHandle::from_raw(backing_xid)
                .expect("non-zero PixmapHandle"),
            width,
            height,
            depth,
        });
    }
}

/// Look up the v2 backing `DrawableId` for the redirected
/// `xid`. Returns `None` if no redirect was installed (or if
/// the window itself isn't in the store).
fn backing_drawable_id(
    backend: &KmsBackend,
    xid: yserver_protocol::x11::ResourceId,
) -> Option<crate::kms::render::store::DrawableId> {
    let host_xid = synth_host_xid(xid);
    let w_id = backend.store.lookup(host_xid)?;
    backend.store.redirected_target(w_id)
}

#[test]
fn resolve_paint_target_after_reparent_out_routes_to_new_redirected_ancestor() {
    use yserver_core::{
        resources::ROOT_WINDOW,
        server::{CompositeRedirectMode, RedirectRecord, ServerState},
    };
    use yserver_protocol::x11::{ClientId, ResourceId};

    // Phase 2 root-cause pin: build a tree mirroring the live
    // mate-panel case (root → mate-panel, root → nm-applet),
    // dispatch a ReparentWindow that moves nm-applet under
    // mate-panel's socket, then assert resolve_paint_target
    // returns mate-panel's backing with the right offset.
    //
    // Pre-fix: nm-applet's stale Manual-redirect backing wins,
    // resolve_paint_target returns it with offset (0, 0).
    // Post-fix (handle_reparent_window's reconciliation): the
    // backing is freed, resolve_paint_target walks up the
    // ancestor chain to mate-panel's backing with the offset
    // = nm-applet's screen-coord position within mate-panel.

    let mut state = ServerState::new();
    let mut backend = KmsBackend::for_tests();
    install_client_for_render(&mut state, 14);

    let root_xid = ROOT_WINDOW;
    let mate_panel_xid = ResourceId(0x110_0003);
    let socket_xid = ResourceId(0x210_0013);
    let nm_applet_xid = ResourceId(0x180_000b);

    // Pre-state: root has RedirectSubwindows(Manual). mate-panel
    // is a redirected direct child. socket is a child of mate-
    // panel (not directly redirected). nm-applet is currently
    // a direct child of root (and therefore inherits redirect).
    state
        .composite_redirects
        .redirect_subwindows(
            root_xid,
            &[],
            RedirectRecord {
                mode: CompositeRedirectMode::Manual,
                owner: ClientId(14),
            },
        )
        .unwrap();

    seed_state_window(
        &mut state,
        &mut backend,
        mate_panel_xid,
        root_xid,
        0,
        0,
        2560,
        28,
    );
    // As CreateWindow does for a child of a subwindows-redirected parent.
    state
        .composite_redirects
        .redirect_new_subwindow(root_xid, mate_panel_xid);
    seed_redirected_backing(&mut state, &mut backend, mate_panel_xid);
    let mate_panel_backing_id =
        backing_drawable_id(&backend, mate_panel_xid).expect("mate-panel backing drawable id");
    seed_state_window(
        &mut state,
        &mut backend,
        socket_xid,
        mate_panel_xid,
        2387,
        0,
        26,
        27,
    );
    seed_state_window(
        &mut state,
        &mut backend,
        nm_applet_xid,
        root_xid,
        0,
        0,
        26,
        27,
    );
    state
        .composite_redirects
        .redirect_new_subwindow(root_xid, nm_applet_xid);
    seed_redirected_backing(&mut state, &mut backend, nm_applet_xid);

    dispatch_reparent_window(
        &mut state,
        &mut backend,
        nm_applet_xid,
        socket_xid,
        /* x */ 0,
        /* y */ 0,
    );

    let nm_applet_host_xid = synth_host_xid(nm_applet_xid);

    let resolved = backend
        .resolve_paint_target(nm_applet_host_xid)
        .expect("resolve must succeed");

    assert_eq!(
        resolved.backing_id(),
        mate_panel_backing_id,
        "paints into nm-applet must route to mate-panel's redirected backing post-reparent"
    );
    assert_eq!(
        resolved.offset(),
        (2387, 0),
        "offset must place the paint at nm-applet's screen-coord position within mate-panel's backing"
    );
}

// ────────────────────────────────────────────────────────────────
// COW structural redesign — Phase 2 Task 2.2: get/release_overlay_window
// own the FULL backend lifecycle (storage + windows +
// top_level_order). The bool return signals the 0→1 / 1→0
// transition so the core handler drives the symmetric resources-
// side materialization once per lifecycle event.
// ────────────────────────────────────────────────────────────────

#[test]
fn get_overlay_window_first_claim_materializes_full_backend_state() {
    let mut b = KmsBackend::for_tests();
    let fb_w = u32::from(b.platform.fb_w);
    let fb_h = u32::from(b.platform.fb_h);

    let was_first_claim = b.get_overlay_window(None).expect("get_overlay_window");
    assert!(was_first_claim, "0→1 transition must return Ok(true)");

    let cow_host_xid = b
        .cow_host_xid()
        .expect("cow_host_xid getter must return Some after first claim");
    let geom = b
        .windows
        .get(&cow_host_xid)
        .expect("COW must be present in windows after first claim");
    assert!(geom.mapped);
    assert_eq!(geom.depth, 24);
    assert_eq!(u32::from(geom.width), fb_w);
    assert_eq!(u32::from(geom.height), fb_h);
    assert_eq!(geom.parent, None);
    // Step 2 (DRIFT 2): get_overlay_window no longer pushes the COW into
    // top_level_order — that membership is projected from core children
    // by the GetOverlayWindow core handler (after materialize_cow_resource)
    // via sync_top_level_order. Covered end-to-end by the
    // `drift2_raise_keeps_cow_on_top` projection gate.
}

#[test]
fn release_overlay_window_final_release_tears_down_full_backend_state() {
    let mut b = KmsBackend::for_tests();
    b.get_overlay_window(None).expect("get");
    let cow_host_xid = b.cow_host_xid().expect("present");

    let was_final_release = b.release_overlay_window(None).expect("release");
    assert!(was_final_release, "1→0 transition must return Ok(true)");
    assert!(
        !b.windows.contains_key(&cow_host_xid),
        "COW removed from windows on final release"
    );
    assert!(
        !b.core.top_level_order.contains(&cow_host_xid),
        "COW removed from top_level_order"
    );
    assert!(
        b.cow_host_xid().is_none(),
        "cow_host_xid getter returns None after final release"
    );
    assert!(
        b.core.xid_map.contains_key(&cow_host_xid),
        "COW stays in the pointer xid map until core unregisters it"
    );
}

/// An unshaped COW takes the pointer (Xorg `compCreateOverlayWindow`), so
/// moving between it and a root sibling crosses Nonlinear both ways, as
/// measured on Xvfb 21.1 (tools/vng-scenarios/cow-input-shape).
#[test]
fn pointer_crossing_between_cow_and_a_sibling_is_nonlinear() {
    use yserver_core::{backend::Backend, host_x11::PointerEventKind, server::ServerState};
    let cow = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW;
    let mut b = KmsBackend::for_tests();
    let mut state = ServerState::new();
    let app = create_live_window(
        &mut state,
        &mut b,
        yserver_protocol::x11::ResourceId(0x0020_0001),
        yserver_core::resources::ROOT_WINDOW,
        100,
        100,
        200,
        200,
    )
    .as_raw();
    b.get_overlay_window(None).expect("get");
    state.resources.materialize_cow_resource(
        yserver_core::backend::WindowHandle::from_raw_panicking(cow.0),
    );

    let crossings = |b: &mut KmsBackend| -> Vec<(PointerEventKind, u32, u8)> {
        std::mem::take(&mut b.core.pending_pointer_events)
            .into_iter()
            .map(|e| (e.kind, e.host_xid, e.detail))
            .collect()
    };
    b.core.prev_pointer_window = Some(app);
    b.update_pointer_window(
        &state,
        cow.0,
        0,
        yserver_core::core_loop::InputOrigin::NestedHost,
    );
    assert_eq!(
        crossings(&mut b),
        vec![
            (PointerEventKind::LeaveNotify, app, 3),
            (PointerEventKind::EnterNotify, cow.0, 3),
        ],
    );
    b.update_pointer_window(
        &state,
        app,
        0,
        yserver_core::core_loop::InputOrigin::NestedHost,
    );
    assert_eq!(
        crossings(&mut b),
        vec![
            (PointerEventKind::LeaveNotify, cow.0, 3),
            (PointerEventKind::EnterNotify, app, 3),
        ],
    );
}
