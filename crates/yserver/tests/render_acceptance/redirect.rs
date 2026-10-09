use super::*;

// ───── Stage 4a — resolve_paint_target via redirect routing ─────

/// Allocate two pixmaps W and B, install `redirected_target(W) =
/// Some(B)` via the test-only setter, then drive `fill_rectangle`
/// against W's xid. Pre-4a: paint would land in W's storage.
/// Post-4a: paint resolves through the redirect and lands in B.
/// GetImage on both reads back the redirected colour from B (also
/// resolved) and B (raw lookup); the same buffer in both cases.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn set_redirected_target_routes_fill_to_backing() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let w_xid = b.create_pixmap(None, 32, 8, 8).expect("W").as_raw();
    let bk_xid = b.create_pixmap(None, 32, 8, 8).expect("B").as_raw();
    // Pre-fill W with red and B with blue so we can tell which one
    // a subsequent paint actually hit.
    b.fill_rectangle(None, w_xid, 0xFFFF0000, 0, 0, 8, 8)
        .expect("seed W red");
    b.fill_rectangle(None, bk_xid, 0xFF0000FF, 0, 0, 8, 8)
        .expect("seed B blue");

    // Install the redirect AFTER the seed fills so the seed paints
    // landed in their respective storage (W has red, B has blue
    // pre-redirect).
    assert!(
        b.test_set_redirected_target(w_xid, bk_xid),
        "test_set_redirected_target failed — xids resolvable?",
    );

    // Paint green via W's xid. Under redirect this lands in B,
    // overwriting the blue.
    b.fill_rectangle(None, w_xid, 0xFF00FF00, 0, 0, 8, 8)
        .expect("redirected fill");

    // GetImage on B's xid (raw, no redirect on a Pixmap) returns
    // the green — the redirected fill landed here.
    let img_b = b
        .get_image_pixels_for_tests(bk_xid, 2, 0, 0, 8, 8, !0)
        .expect("get_image B")
        .expect("Some B bytes");
    assert_eq!(
        &img_b[..4],
        &[0x00, 0xFF, 0x00, 0xFF],
        "B's (0,0) must read green (BGRA) after the redirected fill",
    );

    // GetImage on W's xid ALSO resolves through the redirect per
    // Risk 1, so it reads the same green from B — NOT the seeded
    // red on W's own storage.
    let img_w = b
        .get_image_pixels_for_tests(w_xid, 2, 0, 0, 8, 8, !0)
        .expect("get_image W")
        .expect("Some W bytes");
    assert_eq!(
        &img_w[..4],
        &[0x00, 0xFF, 0x00, 0xFF],
        "GetImage(W) under redirect must read from B (green), \
         not the leaf storage (still red)",
    );
}

/// Xorg's unredirect points the window back at the SCREEN pixmap
/// (`compSetParentPixmap`, `composite/compalloc.c:649`) and copies
/// NOTHING — it does not need to, because the pixels the compositor
/// was showing are already in that pixmap. The redirect direction is
/// the one that copies (`compNewPixmap` seeds the new backing with
/// `CopyArea(parent, …, IncludeInferiors)`, `compalloc.c:556`), so
/// Xorg's invariant is that a window's pixels stay CONTINUOUS with
/// the screen across both transitions.
///
/// yserver keeps a private per-window leaf instead, which has been
/// stale since the redirect — `process_request.rs:889` says so
/// outright while declining a copy in the other direction: "W's
/// storage which under Manual is empty". `release_redirected_backing`
/// then ran `sync_window_leaf_storage_to_geometry`, re-initialising
/// that leaf from the background. So every window a compositor
/// unredirected lost its content.
///
/// Measured on HW (bee, sonicDE/KWin, 2026-09-11): mpv going
/// fullscreen sets `_NET_WM_BYPASS_COMPOSITOR`, KWin suspends
/// compositing for the whole screen, and dolphin's content blanked.
/// The blank tracked the LEAF's init colour — white before the
/// `background_none` fix, black after — which is what proves the
/// blank is this leaf and not a missing Expose.
/// `mpv --x11-bypass-compositor=no` suppressed it entirely, and
/// totem never triggered it because it does not set the property.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn unredirect_restores_the_window_leaf_from_the_backing() {
    use yserver_core::{backend::WindowHandle, host_x11::HostSubwindowVisual};

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let root = WindowHandle::from_raw(1).expect("root");
    let w = b
        .create_subwindow(
            None,
            root,
            0,
            0,
            16,
            16,
            0,
            HostSubwindowVisual::Explicit {
                depth: 32,
                visual_xid: 0,
                colormap_xid: 0,
            },
            // Background None — dolphin's shape, and the largest
            // category in the KWin trace (37 windows).
            None,
            None,
        )
        .expect("create W");
    let w_xid = w.as_raw();
    // The restore walks the plan `plan_backing_inferiors` builds, and
    // that walk prunes unmapped subtrees (X11: an unmapped window is
    // invisible), so W has to be mapped for any of this to be reached.
    b.map_window_for_tests(w_xid).expect("map W");

    // Redirect through the production path: allocates the backing,
    // seeds it parent → B and installs the route.
    let backing = b
        .allocate_redirected_backing(None, w, 16, 16, 32)
        .expect("allocate backing");

    // The client paints green. Under redirect this lands in the
    // BACKING, not in W's leaf — that is what the route is for, and
    // precisely why the leaf is stale when the compositor lets go.
    b.fill_rectangle(None, w_xid, 0xFF00FF00, 0, 0, 16, 16)
        .expect("redirected fill");

    // Unredirect. Both `UnredirectSubwindows` and a compositor crash
    // reach here via `teardown_redirect_for_window`.
    b.release_redirected_backing(None, backing)
        .expect("release backing");

    // The route is gone, so this reads W's own leaf. Xorg would still
    // be showing the green, because the window is back to reading the
    // screen pixmap the compositor had been painting.
    let leaf = b
        .get_image_pixels_for_tests(w_xid, 2, 0, 0, 16, 16, !0)
        .expect("get_image W")
        .expect("Some W bytes");
    let mut distinct = std::collections::BTreeMap::<[u8; 4], usize>::new();
    for px in leaf.chunks_exact(4) {
        *distinct.entry([px[0], px[1], px[2], px[3]]).or_default() += 1;
    }
    assert_eq!(
        distinct.keys().copied().collect::<Vec<_>>(),
        vec![[0x00, 0xFF, 0x00, 0xFF]],
        "after unredirect W's leaf must hold the content the compositor \
         was showing (green, BGRA), not a background re-init: {distinct:?}",
    );
}

/// The HW case the per-window restore missed. A compositor redirects
/// with `RedirectSubwindows(root)`, so the windows that own a backing
/// are the WM's FRAMES; the client's own window is reparented inside
/// one and is a GRANDCHILD of root. Its pixels live in the frame's
/// backing at an offset, and its own leaf is stale from the moment the
/// route is installed.
///
/// Measured on HW (bee, sonicDE/KWin, 2026-09-11): restoring only the
/// redirected window's leaf brought the frame back and left dolphin's
/// content black — and it repainted in full on hover, i.e. the client
/// could rebuild exactly what the server had dropped. So the restore
/// has to walk the subtree, which is what seeding already does in the
/// other direction (`overlay_backing_inferiors`).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn unredirect_restores_a_reparented_child_leaf_not_just_the_frame() {
    use yserver_core::{backend::WindowHandle, host_x11::HostSubwindowVisual};

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let root = WindowHandle::from_raw(1).expect("root");
    let visual = HostSubwindowVisual::Explicit {
        depth: 32,
        visual_xid: 0,
        colormap_xid: 0,
    };
    // The frame, as a WM would create it under root.
    let frame = b
        .create_subwindow(None, root, 0, 0, 16, 16, 0, visual, None, None)
        .expect("create frame");
    let frame_xid = frame.as_raw();
    b.map_window_for_tests(frame_xid).expect("map frame");
    // The client's window, reparented inside the frame at (2, 3) —
    // background None, dolphin's shape.
    let client = b
        .create_subwindow(None, frame, 2, 3, 8, 8, 0, visual, None, None)
        .expect("create client");
    let client_xid = client.as_raw();
    b.map_window_for_tests(client_xid).expect("map client");

    // The compositor redirects the FRAME, not the client window.
    let backing = b
        .allocate_redirected_backing(None, frame, 16, 16, 32)
        .expect("allocate backing");

    // The client paints itself green. This resolves through the
    // frame's route into the backing at (2, 3) — never into the
    // client's own leaf.
    b.fill_rectangle(None, client_xid, 0xFF00FF00, 0, 0, 8, 8)
        .expect("redirected child fill");

    b.release_redirected_backing(None, backing)
        .expect("release backing");

    // The client window's OWN leaf must now hold the green. Before the
    // subtree walk this read transparent black across all 64 px, which
    // is the dolphin symptom exactly.
    let leaf = b
        .get_image_pixels_for_tests(client_xid, 2, 0, 0, 8, 8, !0)
        .expect("get_image client")
        .expect("Some client bytes");
    let mut distinct = std::collections::BTreeMap::<[u8; 4], usize>::new();
    for px in leaf.chunks_exact(4) {
        *distinct.entry([px[0], px[1], px[2], px[3]]).or_default() += 1;
    }
    assert_eq!(
        distinct.keys().copied().collect::<Vec<_>>(),
        vec![[0x00, 0xFF, 0x00, 0xFF]],
        "after unredirect the reparented child's leaf must hold its own \
         content (green, BGRA), not a stale/blank leaf: {distinct:?}",
    );
}

/// Set up parent-W with a sub-child C at position (2, 3). Redirect
/// W to backing B. A fill rect at (1, 1, 4, 4) against C's xid must
/// land at (3, 4, 4, 4) in B — the C-relative offset accumulated
/// through `resolve_paint_target`. Tests the descendant-offset
/// path end-to-end through the Backend trait.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn set_redirected_target_descendant_fill_lands_at_offset() {
    use yserver_core::{backend::WindowHandle, host_x11::HostSubwindowVisual};

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // Create depth-32 W under root, 16×16; then C at (2, 3) under
    // W, 8×8. allocate_window_storage will fill both with the
    // depth-32 safe default (transparent black).
    let root = WindowHandle::from_raw(1).expect("root");
    let w = b
        .create_subwindow(
            None,
            root,
            0,
            0,
            16,
            16,
            0,
            HostSubwindowVisual::Explicit {
                depth: 32,
                visual_xid: 0,
                colormap_xid: 0,
            },
            None,
            None,
        )
        .expect("create W");
    let w_xid = w.as_raw();
    b.map_window_for_tests(w_xid).expect("map");
    let c = b
        .create_subwindow(
            None,
            w,
            2,
            3,
            8,
            8,
            0,
            HostSubwindowVisual::Explicit {
                depth: 32,
                visual_xid: 0,
                colormap_xid: 0,
            },
            None,
            None,
        )
        .expect("create C");
    let c_xid = c.as_raw();
    b.map_window_for_tests(c_xid).expect("map");

    // Allocate B (a pixmap) for the backing storage. Seed it black
    // so the post-fill check can detect green-at-offset.
    let bk_xid = b.create_pixmap(None, 32, 16, 16).expect("B").as_raw();
    b.fill_rectangle(None, bk_xid, 0xFF000000, 0, 0, 16, 16)
        .expect("seed B black");

    // Install the redirect W → B.
    assert!(
        b.test_set_redirected_target(w_xid, bk_xid),
        "redirect install (W={w_xid:#x}, B={bk_xid:#x})"
    );

    // Fill green on C at (1, 1, 4, 4) — C-window-local coords.
    // Expected outcome: paint resolves through C→W (ancestor walk)
    // with accumulated offset (2, 3), then through W's redirect
    // to B. Result: green rect at B coords (3, 4, 4, 4).
    b.fill_rectangle(None, c_xid, 0xFF00FF00, 1, 1, 4, 4)
        .expect("descendant fill");

    // GetImage on B directly. Stride for depth-32 is `w * 4`.
    let img = b
        .get_image_pixels_for_tests(bk_xid, 2, 0, 0, 16, 16, !0)
        .expect("get_image B")
        .expect("Some bytes");
    let pixel = |x: usize, y: usize| -> [u8; 4] {
        let off = (y * 16 + x) * 4;
        [img[off], img[off + 1], img[off + 2], img[off + 3]]
    };
    // Inside the redirected rect: (3,4)..(7,8).
    assert_eq!(
        pixel(3, 4),
        [0x00, 0xFF, 0x00, 0xFF],
        "B at (3,4) must be green — descendant offset (2,3) plus rect (1,1) sums to (3,4)",
    );
    assert_eq!(
        pixel(6, 7),
        [0x00, 0xFF, 0x00, 0xFF],
        "B at (6,7) — last pixel of the redirected rect — must also be green",
    );
    // Outside the rect: still the seeded black.
    assert_eq!(
        pixel(0, 0),
        [0x00, 0x00, 0x00, 0xFF],
        "B at (0,0) must stay black — fill lands at (3,4), not the origin",
    );
    assert_eq!(
        pixel(8, 8),
        [0x00, 0x00, 0x00, 0xFF],
        "B at (8,8) must stay black — past the redirected rect's bottom-right",
    );
}

// ───── Stage 4b — allocate_redirected_backing / name_window_pixmap /
// ───── release_redirected_backing
//
// Each test drives the Backend-trait surface for the COMPOSITE
// redirect lifecycle. v1's reference impls live in
// `crates/yserver/src/kms/backend.rs:9523-9607`; v2 mirrors the
// shape via `KmsCore.alias_registry` + `KmsCore.host_window_to_backing`
// (already in tree as shared state).

/// Plan §4b: `allocate_redirected_backing(W, w, h, depth)` allocates
/// a fresh backing pixmap, seeds `alias_registry` with refcount=1,
/// and maps `host_window_to_backing[W] = B`. The returned
/// `PixmapHandle` is what `name_window_pixmap(W)` returns on every
/// subsequent call (with incremented refcount).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn allocate_redirected_backing_seeds_refcount_and_map() {
    use yserver_core::backend::WindowHandle;

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    // Allocate a pixmap to act as the "window" — v2 doesn't care
    // about W being a real Window-kind drawable for the activation
    // path; what matters is the xid resolves in the store so the
    // `set_redirected_target` step succeeds. (In 4c real-app paths
    // W is a top-level Window-kind drawable; the seed-copy path
    // tested separately in `redirect_seed_copies_window_content`
    // exercises that shape.)
    let w_xid = b.create_pixmap(None, 32, 16, 16).expect("W").as_raw();
    let w_handle = WindowHandle::from_raw(w_xid).expect("WindowHandle");

    let backing = b
        .allocate_redirected_backing(None, w_handle, 16, 16, 32)
        .expect("allocate_redirected_backing must succeed in v2");
    let raw = backing.as_raw();
    assert_ne!(raw, 0, "backing handle is non-zero");
    assert_ne!(
        raw, w_xid,
        "backing xid distinct from window xid (fresh pixmap)",
    );

    // Inspect the shared state via the read-only test helper.
    let entry = b
        .test_alias_registry_get(raw)
        .expect("alias_registry must have a Reason-1 hold");
    assert_eq!(entry.refcount, 1, "Reason-1 seed → refcount = 1");
    assert_eq!(entry.width, 16);
    assert_eq!(entry.height, 16);
    assert_eq!(entry.depth, 32);

    let mapped = b
        .test_host_window_to_backing(w_xid)
        .expect("host_window_to_backing must point at the backing");
    assert_eq!(mapped, raw, "map points at the backing xid");
}

/// Plan §4b: a second `allocate_redirected_backing(W, …)` for an
/// already-redirected W returns the SAME handle with NO refcount
/// bump (it's the redirect-activation hold, not an alias). v1
/// idempotency path at `kms/backend.rs:9581-9588`.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn allocate_redirected_backing_is_idempotent() {
    use yserver_core::backend::WindowHandle;

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let w_xid = b.create_pixmap(None, 32, 8, 8).expect("W").as_raw();
    let w = WindowHandle::from_raw(w_xid).unwrap();
    let first = b.allocate_redirected_backing(None, w, 8, 8, 32).unwrap();
    let second = b.allocate_redirected_backing(None, w, 8, 8, 32).unwrap();
    assert_eq!(
        first.as_raw(),
        second.as_raw(),
        "idempotent allocation returns the same handle",
    );
    let entry = b.test_alias_registry_get(first.as_raw()).unwrap();
    assert_eq!(
        entry.refcount, 1,
        "no incref on the idempotent path — Reason-1 is single-instance",
    );
}

/// Plan §4b: `name_window_pixmap(W)` after activation returns the
/// existing backing and increments refcount (Reason-2 alias hold).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn name_window_pixmap_returns_existing_backing() {
    use yserver_core::backend::WindowHandle;

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let w_xid = b.create_pixmap(None, 32, 8, 8).expect("W").as_raw();
    let w = WindowHandle::from_raw(w_xid).unwrap();
    let backing = b.allocate_redirected_backing(None, w, 8, 8, 32).unwrap();
    let aliased = b.name_window_pixmap(None, w).unwrap();
    assert_eq!(
        aliased.as_raw(),
        backing.as_raw(),
        "alias handle equals backing handle (same xid on every call)",
    );
    let entry = b.test_alias_registry_get(backing.as_raw()).unwrap();
    assert_eq!(
        entry.refcount, 2,
        "alias bumps refcount to 2 (Reason-1 + Reason-2)",
    );
}

/// Plan §4b: `name_window_pixmap(W)` against an un-redirected W
/// returns `NotFound` (X11 protocol error → BadWindow upstream).
/// v1 uses `io::ErrorKind::NotFound` at `kms/backend.rs:9534`.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn name_window_pixmap_without_redirect_errors_not_found() {
    use yserver_core::backend::WindowHandle;

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let w_xid = b.create_pixmap(None, 32, 8, 8).expect("W").as_raw();
    let w = WindowHandle::from_raw(w_xid).unwrap();
    let err = b
        .name_window_pixmap(None, w)
        .expect_err("name without redirect must error");
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::NotFound,
        "v1-parity: NotFound (got {err:?})",
    );
}

/// Plan §4b: `release_redirected_backing` decrefs the Reason-1
/// hold; with no aliases held, the backing storage is destroyed.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn release_redirected_backing_drops_storage_when_no_aliases() {
    use yserver_core::backend::WindowHandle;

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let w_xid = b.create_pixmap(None, 32, 8, 8).expect("W").as_raw();
    let w = WindowHandle::from_raw(w_xid).unwrap();
    let backing = b.allocate_redirected_backing(None, w, 8, 8, 32).unwrap();
    let bxid = backing.as_raw();

    b.release_redirected_backing(None, backing).unwrap();

    assert!(
        b.test_alias_registry_get(bxid).is_none(),
        "alias_registry entry removed (refcount → 0)",
    );
    assert!(
        b.test_host_window_to_backing(w_xid).is_none(),
        "host_window_to_backing entry cleared",
    );
}

/// Audit #6 (2026-05-19) — Xorg parity. `compNewPixmap`
/// (composite/compalloc.c:541-606) seeds the backing pixmap from
/// the PARENT's storage at W's position (with IncludeInferiors),
/// NOT from W's own storage. This is the fix for the recurring
/// "black band on map" symptom: a freshly mapped window that's
/// redirected on map has a default-init (opaque black or
/// transparent) storage; copying that into B would show black
/// where W is until the client's first paint. Seeding from the
/// parent shows continuity with what was on-screen before W
/// appeared.
///
/// Repro: paint root red at the W-footprint area; create W as a
/// child of root with NO paint of its own; activate redirect.
/// The backing must read red — parent's pixels at W's position —
/// NOT W's default-init colour.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn redirect_seed_uses_parent_content_at_w_position() {
    use yserver_core::{backend::WindowHandle, host_x11::HostSubwindowVisual};

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let root = WindowHandle::from_raw(1).expect("root");
    let root_xid = root.as_raw();

    // Paint a known red into the root at the area that W will cover.
    // We paint a 16×16 region from (5, 7) so it strictly contains W
    // (8×8 at (5, 7) inside root).
    b.fill_rectangle(None, root_xid, 0xFFFF0000, 5, 7, 16, 16)
        .expect("seed root red at W footprint");

    let w_handle = b
        .create_subwindow(
            None,
            root,
            5,
            7,
            8,
            8,
            0,
            HostSubwindowVisual::Explicit {
                depth: 32,
                visual_xid: 0,
                colormap_xid: 0,
            },
            None,
            None,
        )
        .expect("create W as child of root");
    // Deliberately do NOT paint W — its storage stays at the
    // default init colour (depth-32 → (0, 0, 0, 0) transparent).

    let backing = b
        .allocate_redirected_backing(None, w_handle, 8, 8, 32)
        .expect("allocate must succeed");
    let bxid = backing.as_raw();

    let img = b
        .get_image_pixels_for_tests(bxid, 2, 0, 0, 8, 8, !0)
        .expect("get_image")
        .expect("Some bytes");
    let pixel = |x: usize, y: usize| -> [u8; 4] {
        let off = (y * 8 + x) * 4;
        [img[off], img[off + 1], img[off + 2], img[off + 3]]
    };

    // Pre-fix: backing reads W's default-init (0,0,0,0) — invisible /
    // black-band depending on the scene blend. Post-fix: parent's red
    // at the source position (5, 7), copied into B at (0, 0).
    assert_eq!(
        pixel(0, 0),
        [0x00, 0x00, 0xFF, 0xFF],
        "backing's (0, 0) must read parent's red at W's screen \
         position (5, 7); pre-fix the seed copied W's default-init \
         colour and produced (0, 0, 0, 0).",
    );
    assert_eq!(
        pixel(7, 7),
        [0x00, 0x00, 0xFF, 0xFF],
        "backing's (7, 7) must read parent's red (the W-footprint \
         region of root was filled red strictly larger than W).",
    );
}

/// Plan §4b: a `NameWindowPixmap` alias keeps the backing alive
/// past `release_redirected_backing` — the alias's FreePixmap
/// is what eventually drops the storage.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn release_redirected_backing_survives_named_alias() {
    use yserver_core::backend::WindowHandle;

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let w_xid = b.create_pixmap(None, 32, 8, 8).expect("W").as_raw();
    let w = WindowHandle::from_raw(w_xid).unwrap();
    let backing = b.allocate_redirected_backing(None, w, 8, 8, 32).unwrap();
    let bxid = backing.as_raw();
    let alias = b.name_window_pixmap(None, w).unwrap();
    assert_eq!(alias.as_raw(), bxid, "alias is the backing xid");

    // Drop Reason 1. Reason 2 (alias) keeps it alive.
    b.release_redirected_backing(None, backing).unwrap();
    let entry = b
        .test_alias_registry_get(bxid)
        .expect("alias still holds the backing");
    assert_eq!(entry.refcount, 1, "Reason-1 dropped, Reason-2 remains");
    assert!(
        b.test_host_window_to_backing(w_xid).is_none(),
        "redirect map cleared — only the alias refers to the backing now",
    );

    // FreePixmap on the alias must drop the storage.
    b.free_pixmap(None, alias.as_raw()).unwrap();
    assert!(
        b.test_alias_registry_get(bxid).is_none(),
        "alias FreePixmap drops the last hold",
    );
}

// ───── Stage 4c.5 — Vk-backed participation + mode-flip oracles ────
//
// Test #5 (`redirected_paint_lands_in_backing`) from the task spec
// is already covered by `set_redirected_target_routes_fill_to_backing`
// above — that test pre-fills B blue, installs the redirect, paints
// green through W's xid, and asserts B reads green. Skipped here to
// keep the suite mean (single-purpose oracles).

/// Stage 4c.5 — Automatic-mode redirect: paint through W's xid lands
/// in B (per 4a's `resolve_paint_target`) AND accumulates presentation
/// damage on B (since B's `scene_participating=true`). The scene
/// walk's `peek_presentation_damage` (scene.rs:1148 via 4c.3's
/// `source_id` indirection) is what picks up that damage; the
/// participation flag on B is the gate (`peek` returns None when
/// `!scene_participating`).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn automatic_redirect_backing_is_scene_participating() {
    use yserver_core::backend::{PixmapHandle, WindowHandle};

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // Use a depth-32 pixmap as W (the redirect surface). `for_tests`
    // doesn't drive a real CreateWindow flow; v2's
    // `allocate_redirected_backing` accepts any drawable xid in the
    // store (the `name_window_pixmap_returns_existing_backing` test
    // above uses the same shape).
    let w_xid = b.create_pixmap(None, 32, 8, 8).expect("W").as_raw();
    let w = WindowHandle::from_raw(w_xid).expect("WindowHandle");
    let backing = b
        .allocate_redirected_backing(None, w, 8, 8, 32)
        .expect("allocate backing");
    let bxid = backing.as_raw();
    let bk_handle = PixmapHandle::from_raw(bxid).expect("PixmapHandle");

    // Automatic-mode protocol pairing: W AND B both flip to
    // scene_participating=true.
    b.set_window_scene_participation(None, w, true)
        .expect("set_window_scene_participation(true)");
    b.set_backing_scene_participation(None, bk_handle, true)
        .expect("set_backing_scene_participation(true)");

    // Per-store assertion: B's scene_participating flipped on.
    // Reach into the doc-hidden test helpers via the public store
    // surface — `get_by_xid` is `pub(crate)`, so use the
    // presentation-damage probe below as the contract check.
    // First confirm the flag flipped by checking that
    // peek_presentation_damage doesn't `None` out (it would on
    // !scene_participating, even after we paint).

    // Paint green via W's xid. Per 4a's `resolve_paint_target` this
    // lands in B; per 3f's damage accounting that fires
    // `store.damage` on B's drawable, which (with B
    // scene_participating=true) accumulates as presentation damage.
    b.fill_rectangle(None, w_xid, 0xFF00FF00, 1, 2, 3, 4)
        .expect("redirected fill via W");

    // GetImage on B confirms the paint landed there (sanity — the
    // damage assertion below relies on the paint actually hitting).
    let img = b
        .get_image_pixels_for_tests(bxid, 2, 0, 0, 8, 8, !0)
        .expect("get_image B")
        .expect("Some B bytes");
    let pixel = |x: usize, y: usize| -> [u8; 4] {
        let off = (y * 8 + x) * 4;
        [img[off], img[off + 1], img[off + 2], img[off + 3]]
    };
    assert_eq!(
        pixel(1, 2),
        [0x00, 0xFF, 0x00, 0xFF],
        "B at (1,2) — top-left of the redirected fill — must be green",
    );

    // The key oracle: presentation damage accumulated on B (because
    // B is scene_participating=true). A pre-4c backing with the
    // default scene_participating=false would have produced a
    // damage record that `peek_presentation_damage` returns as None
    // (see store.rs:670 — the gate is the `scene_participating`
    // flag). `test_peek_presentation_damage_nonempty` rolls both
    // checks into one bool to keep this oracle terse.
    assert!(
        b.test_peek_presentation_damage_nonempty(bxid),
        "B must have peekable, non-empty presentation damage from the redirected fill \
         (false ⇒ either scene_participating=false or region empty at paint time)",
    );
}

/// Stage 4c.5 — mode-flip preserves the backing and any
/// `NameWindowPixmap` aliases. Per Stage 4 plan §"Cross-cutting:
/// Mode-flip semantics", `RedirectWindow(W, Mode)` issued a second
/// time on an already-redirected W must reuse the existing backing
/// (no destroy + recreate) so client aliases stay valid and content
/// is preserved. This test exercises the at-this-layer simulation:
///
/// - alloc backing for W
/// - name_window_pixmap(W) → alias bumps refcount to 2
/// - paint a sentinel into B
/// - simulate a Manual→Automatic mode flip by toggling participation
///   (Automatic-mode protocol pairing)
/// - assert: backing's xid unchanged, alias refcount unchanged, B's
///   sentinel content preserved
///
/// Note (per task spec): the protocol-handler `flip_redirect_target_mode`
/// path in `yserver-core/src/core_loop/process_request.rs` isn't
/// drivable from `tests/acceptance.rs` without protocol scaffolding
/// (see TODO comments below). The participation-toggle dance covers
/// the same backend-trait invariants the protocol handler exercises.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn mode_flip_preserves_backing_and_aliases() {
    use yserver_core::backend::{PixmapHandle, WindowHandle};

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let w_xid = b.create_pixmap(None, 32, 8, 8).expect("W").as_raw();
    let w = WindowHandle::from_raw(w_xid).expect("WindowHandle");

    // Initial Manual-mode setup: allocate backing, flip W off-scene.
    let backing = b
        .allocate_redirected_backing(None, w, 8, 8, 32)
        .expect("allocate backing");
    let bxid_pre_flip = backing.as_raw();
    b.set_window_scene_participation(None, w, false)
        .expect("Manual activation (W→false)");

    // Create a NameWindowPixmap alias — refcount goes 1 → 2.
    let alias = b.name_window_pixmap(None, w).expect("name_window_pixmap");
    assert_eq!(
        alias.as_raw(),
        bxid_pre_flip,
        "alias xid must equal the backing xid (Reason-2 incref on the same handle)",
    );
    let entry_before = b
        .test_alias_registry_get(bxid_pre_flip)
        .expect("alias_registry entry present");
    assert_eq!(
        entry_before.refcount, 2,
        "post-alias refcount = Reason-1 (1) + Reason-2 (1) = 2",
    );

    // Paint a sentinel into B before the flip — magenta at (0,0).
    b.fill_rectangle(None, bxid_pre_flip, 0xFFFF00FF, 0, 0, 8, 8)
        .expect("sentinel paint into B");
    let img_pre = b
        .get_image_pixels_for_tests(bxid_pre_flip, 2, 0, 0, 8, 8, !0)
        .expect("get_image pre-flip")
        .expect("Some bytes pre-flip");
    let pre_pixel: [u8; 4] = [img_pre[0], img_pre[1], img_pre[2], img_pre[3]];
    assert_eq!(
        pre_pixel,
        [0xFF, 0x00, 0xFF, 0xFF],
        "fixture sanity: B's (0,0) must read the sentinel magenta pre-flip",
    );

    // Mode flip: Manual → Automatic. The protocol handler's
    // `flip_redirect_target_mode` ultimately calls
    // `set_window_scene_participation(W, true)` +
    // `set_backing_scene_participation(B, true)`.
    let bk_handle = PixmapHandle::from_raw(bxid_pre_flip).expect("PixmapHandle");
    b.set_window_scene_participation(None, w, true)
        .expect("Automatic activation (W→true)");
    b.set_backing_scene_participation(None, bk_handle, true)
        .expect("Automatic activation (B→true)");

    // Backing xid unchanged.
    let bxid_post = b
        .test_host_window_to_backing(w_xid)
        .expect("host_window_to_backing still maps W → B");
    assert_eq!(
        bxid_post, bxid_pre_flip,
        "mode flip must NOT recreate the backing (xid must be stable)",
    );

    // Alias refcount unchanged (still Reason-1 + Reason-2).
    let entry_after = b
        .test_alias_registry_get(bxid_pre_flip)
        .expect("alias_registry entry still present post-flip");
    assert_eq!(
        entry_after.refcount, entry_before.refcount,
        "alias refcount must be preserved across mode flip \
         (pre={}, post={})",
        entry_before.refcount, entry_after.refcount,
    );

    // Content preserved — B's (0,0) still magenta.
    let img_post = b
        .get_image_pixels_for_tests(bxid_pre_flip, 2, 0, 0, 8, 8, !0)
        .expect("get_image post-flip")
        .expect("Some bytes post-flip");
    let post_pixel: [u8; 4] = [img_post[0], img_post[1], img_post[2], img_post[3]];
    assert_eq!(
        post_pixel, pre_pixel,
        "B's content must be preserved across mode flip \
         (pre={pre_pixel:?}, post={post_pixel:?})",
    );
}

// ───── Stage 4c.5 — deferred protocol-level tests ───────────────────
//
// The Stage 4b.9 / 4c plan also lists these protocol-level invariants
// that require driving the X11 wire bytes through
// `yserver-core::core_loop::process_request::handle_composite_request`.
// yserver-core has no test scaffolding for that path today, and
// building it is its own substage's worth of work. The hardware-smoke
// gate at 4c.6 is the actual coverage for these invariants until the
// scaffolding lands.
//
// TODO(4c.7 or post-4c): needs `handle_composite_request` test scaffolding
// - map_window_after_redirect_subwindows_keeps_manual_participation
//     RedirectSubwindows(parent, Manual) → MapWindow(child) — child's
//     participation must stay Manual (off-scene); the post-map hook
//     must not flip it back on.
//
// TODO(4c.7 or post-4c): needs `handle_composite_request` test scaffolding
// - map_subwindows_redirects_each_child
//     RedirectSubwindows(parent, Manual) → MapSubwindows(parent) —
//     every child gets its own `allocate_redirected_backing` call
//     via the per-child redirect hook.
//
// TODO(4c.7 or post-4c): needs `handle_composite_request` test scaffolding
// - name_window_pixmap_on_unviewable_returns_bad_match
//     NameWindowPixmap(W) on an unmapped (unviewable) window must
//     return `BadMatch` per the X11 COMPOSITE spec, not silently
//     succeed with an alias to whatever backing exists.
//
// existing_alias_survives_window_unmap: covered by the lib test
// `named_pixmap_survives_unmap_and_remap_gets_new_backing`.

/// Stage 4d — paint into the Composite Overlay Window via its xid
/// after `GetOverlayWindow`, and assert the paint lands on COW
/// storage with presentation damage accumulated. This is the load-
/// bearing v2 path for compositing WMs (marco-compositing,
/// xfwm4-compositing): pre-4d the COW xid resolved to nothing in
/// the store, so every `render_composite` against it gap-logged
/// and dropped paint.
///
/// Oracle shape: scanout dump integration is heavyweight (needs
/// `dump_scanout` wiring that test fixtures don't have); per the
/// stage brief, the acceptable surrogate is
/// `test_peek_presentation_damage_nonempty(0x103)` after a
/// `put_image` against the COW xid — confirms (a) the xid resolves,
/// (b) the storage is `scene_participating`, and (c) the paint
/// accumulated presentation damage that a scene tick would consume.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn cow_paint_appears_on_scanout() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // Step 1: GetOverlayWindow — allocates COW storage at xid 0x103.
    b.get_overlay_window(None).expect("get_overlay_window");
    let cow_xid = 0x103u32;

    // Step 2: paint a known red square at (0, 0). put_image with a
    // 4-byte BGRA pixel goes through the engine.put_image path; on
    // a Vk-backed fixture this lands on COW storage.
    let pixels: Vec<u8> = vec![
        // 2×2 of red (BGRA premul: B=0, G=0, R=0xFF, A=0xFF)
        0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0xFF,
        0xFF,
    ];
    b.put_image(None, cow_xid, 24, 2, 2, 0, 0, &pixels)
        .expect("put_image into COW xid");

    // Step 3: GetImage on the COW xid round-trips back the red
    // pixels — confirms the put_image actually landed on COW
    // storage (vs being dropped into the gap-logged no-op path).
    let img = b
        .get_image_pixels_for_tests(cow_xid, 2, 0, 0, 2, 2, !0)
        .expect("get_image COW")
        .expect("Some COW bytes");
    assert_eq!(
        &img[..4],
        &[0x00, 0x00, 0xFF, 0xFF],
        "COW (0,0) must round-trip the painted red",
    );

    // Step 4: presentation damage accumulated on COW. The scene
    // tick would consume this on next composite; here we assert
    // the storage is in the right state (scene_participating=true
    // + non-empty damage region) to be picked up by build_scene.
    assert!(
        b.test_peek_presentation_damage_nonempty(cow_xid),
        "COW must have non-empty presentation damage after put_image — \
         false ⇒ either xid resolved to nothing (pre-4d shape) or \
         scene_participating=false (4d wiring missing)",
    );

    // Step 5: release drops the storage; the xid must no longer
    // resolve.
    b.release_overlay_window(None).expect("release");
    let img_after = b.get_image_pixels_for_tests(cow_xid, 2, 0, 0, 2, 2, !0);
    assert!(
        img_after.is_err() || img_after.as_ref().unwrap().is_none(),
        "GetImage on COW xid after final release must fail or return None \
         (storage destroyed) — got {img_after:?}",
    );
}
