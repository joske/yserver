use super::*;

// ─── Stage 3f.1: poly_* + fill_poly logic tests ────────────

/// `poly_line_origin_mode_offsets_correctly` per plan §3f tests.
/// Build a 3-point path under both Origin (absolute) and
/// Previous (delta) coordinate modes; assert the produced
/// rasterised-pixel set is the same. Drives Bresenham via the
/// public crate-level helper.
#[test]
fn poly_line_origin_mode_offsets_correctly() {
    use crate::kms::{
        backend::{bresenham_segment, read_i16_pair},
        cpu_types::Rectangle16,
    };

    // Path: (10, 10) → (10, 13) → (13, 13) — an L shape.
    let absolute_pts: [(i16, i16); 3] = [(10, 10), (10, 13), (13, 13)];
    // Same path under Previous mode: first pt absolute, then deltas.
    let delta_pts: [(i16, i16); 3] = [(10, 10), (0, 3), (3, 0)];

    let rasterise = |points: &[u8], mode: u8| -> Vec<Rectangle16> {
        let mut rects: Vec<Rectangle16> = Vec::new();
        let mut prev: Option<(i32, i32)> = None;
        let mut offset = 0;
        while let Some((x, y)) = read_i16_pair(points, offset) {
            offset += 4;
            let (xi, yi) = if mode == 1 {
                if let Some((px, py)) = prev {
                    (px + i32::from(x), py + i32::from(y))
                } else {
                    (i32::from(x), i32::from(y))
                }
            } else {
                (i32::from(x), i32::from(y))
            };
            if let Some((px, py)) = prev {
                bresenham_segment(px, py, xi, yi, &mut rects);
            }
            prev = Some((xi, yi));
        }
        rects
    };

    let pack = |pts: &[(i16, i16)]| -> Vec<u8> {
        let mut out = Vec::with_capacity(pts.len() * 4);
        for (x, y) in pts {
            out.extend_from_slice(&x.to_le_bytes());
            out.extend_from_slice(&y.to_le_bytes());
        }
        out
    };

    let abs_rects = rasterise(&pack(&absolute_pts), 0);
    let prev_rects = rasterise(&pack(&delta_pts), 1);

    // Both modes must produce the same rasterised pixel set. Expand each
    // rect to its covered pixels: `bresenham_segment` coalesces axis-aligned
    // runs into spans, so a rect is no longer necessarily 1×1.
    let to_set = |rs: &[Rectangle16]| -> std::collections::BTreeSet<(i16, i16)> {
        let mut s = std::collections::BTreeSet::new();
        for r in rs {
            for dy in 0..i32::from(r.height) {
                for dx in 0..i32::from(r.width) {
                    s.insert(((i32::from(r.x) + dx) as i16, (i32::from(r.y) + dy) as i16));
                }
            }
        }
        s
    };
    assert_eq!(to_set(&abs_rects), to_set(&prev_rects));
    // Sanity: pixel set covers the L's expected vertices.
    let set = to_set(&abs_rects);
    for p in [(10, 10), (10, 13), (13, 13)] {
        assert!(set.contains(&p), "missing endpoint {p:?}");
    }
}

/// `fill_poly_scanline_correctness` per plan §3f tests. A 5-point
/// convex polygon (axis-aligned diamond) round-trips through
/// `scanline_fill_polygon` and produces the expected horizontal
/// span set. Even-odd-rule fill, half-open scanline range.
#[test]
fn fill_poly_scanline_correctness() {
    use crate::kms::{backend::scanline_fill_polygon, cpu_types::Rectangle16};

    // Square with one mid-edge vertex injected — still convex,
    // and 5 distinct vertices as the test name advertises. Vertex
    // list: (0,0) (4,0) (4,2) (4,4) (0,4) — a 4×4 square with an
    // extra vertex on the right edge. Filled region is rows
    // y ∈ [0, 4) with x ∈ [0, 4) at each row.
    let verts = [(0, 0), (4, 0), (4, 2), (4, 4), (0, 4)];
    let mut rects: Vec<Rectangle16> = Vec::new();
    scanline_fill_polygon(&verts, &mut rects);

    // Collect (y, x_start, x_end) per row. Each row should be a
    // single span; we union rects on shared y if needed.
    let mut rows: std::collections::BTreeMap<i16, (i16, i16)> = std::collections::BTreeMap::new();
    for r in &rects {
        let x_start = r.x;
        let x_end = r.x + r.width as i16;
        rows.entry(r.y)
            .and_modify(|cur| {
                cur.0 = cur.0.min(x_start);
                cur.1 = cur.1.max(x_end);
            })
            .or_insert((x_start, x_end));
    }
    // Expected: rows 0..=3 each span x ∈ [0, 4). Row 4 is the
    // top edge of the polygon under half-open [y0, y1) semantics
    // — no horizontal scan crosses it.
    for y in 0..4 {
        let span = rows.get(&y).copied().unwrap_or_else(|| {
            panic!("row {y} missing");
        });
        assert_eq!(span, (0, 4), "row {y} span");
    }
    assert!(!rows.contains_key(&4), "row 4 must not be filled");
}

#[test]
fn root_include_inferiors_xor_routes_to_overlay() {
    let mut b = KmsBackend::for_tests();
    b.core.current_function = yserver_core::backend::GcFunction::Invert;
    b.core.current_subwindow_mode = yserver_core::backend::SubwindowMode::IncludeInferiors;
    let origin = Some(yserver_core::backend::OriginContext {
        client_id: yserver_protocol::x11::ClientId(3),
        nested_seq: 0,
        opcode: 70,
    });
    let rects = [Rectangle16 {
        x: 10,
        y: 10,
        width: 50,
        height: 1,
    }];
    let root = b.core.window_id;
    assert!(b.is_root_overlay_draw(root));
    b.capture_root_overlay(origin, !0, &rects);
    assert!(!b.scene.root_overlay.is_empty(), "op landed in overlay");
}

#[test]
fn client_disconnected_clears_overlay() {
    let mut b = KmsBackend::for_tests();
    b.scene.root_overlay_toggle(
        yserver_protocol::x11::ClientId(5),
        0xffffff,
        &[ash::vk::Rect2D {
            offset: ash::vk::Offset2D { x: 0, y: 0 },
            extent: ash::vk::Extent2D {
                width: 4,
                height: 4,
            },
        }],
    );
    assert!(!b.scene.root_overlay.is_empty());
    yserver_core::backend::Backend::client_disconnected(&mut b, yserver_protocol::x11::ClientId(5));
    assert!(b.scene.root_overlay.is_empty());
}

#[test]
#[ignore = "needs a DRM render node"]
fn client_disconnected_does_not_own_syncobj_lifetime() {
    use std::os::fd::AsFd;
    use yserver_protocol::x11::ClientId;

    // Shared helper from Task 1 — never hardcode renderD128.
    let Some(drm) = crate::kms::render::imported_syncobj::tests::render_node() else {
        eprintln!("skipping: no render node");
        return;
    };
    let mk = |_value: u64| {
        let handle =
            ::drm::control::Device::create_syncobj(drm.as_ref(), false).expect("create syncobj");
        let fd =
            ::drm::control::Device::syncobj_to_fd(drm.as_ref(), handle, false).expect("export fd");
        let imported =
            crate::kms::render::imported_syncobj::ImportedSyncobj::import(drm.clone(), fd.as_fd())
                .expect("import");
        ::drm::control::Device::destroy_syncobj(drm.as_ref(), handle).expect("destroy");
        std::sync::Arc::new(imported)
    };

    let mut b = KmsBackend::for_tests();
    b.dri3_syncobjs.insert(0x0040_0001, (ClientId(7), mk(1)));
    b.dri3_syncobjs.insert(0x0040_0002, (ClientId(8), mk(1)));

    yserver_core::backend::Backend::client_disconnected(&mut b, ClientId(7));
    assert!(
        b.dri3_syncobjs.contains_key(&0x0040_0002),
        "another client's syncobj must survive the disconnect",
    );
    assert!(
        b.dri3_syncobjs.contains_key(&0x0040_0001),
        "the generic disconnect hook must not bypass core resource retention",
    );
}

#[test]
#[ignore = "needs a DRM render node"]
fn dri3_syncobj_owned_is_client_scoped() {
    use std::os::fd::AsFd;
    use yserver_protocol::x11::ClientId;

    // Shared helper from Task 1 — never hardcode renderD128.
    let Some(drm) = crate::kms::render::imported_syncobj::tests::render_node() else {
        eprintln!("skipping: no render node");
        return;
    };
    let handle =
        ::drm::control::Device::create_syncobj(drm.as_ref(), false).expect("create syncobj");
    let fd = ::drm::control::Device::syncobj_to_fd(drm.as_ref(), handle, false).expect("export fd");
    let imported =
        crate::kms::render::imported_syncobj::ImportedSyncobj::import(drm.clone(), fd.as_fd())
            .expect("import");
    ::drm::control::Device::destroy_syncobj(drm.as_ref(), handle).expect("destroy");

    let xid = 0x0040_0001_u32;
    let mut b = KmsBackend::for_tests();
    b.dri3_syncobjs
        .insert(xid, (ClientId(7), std::sync::Arc::new(imported)));

    assert!(
        yserver_core::backend::Backend::dri3_syncobj_owned(&b, ClientId(7), xid),
        "the importing client owns its syncobj",
    );
    assert!(
        !yserver_core::backend::Backend::dri3_syncobj_owned(&b, ClientId(8), xid),
        "a different client must not own the syncobj",
    );
    assert!(
        !yserver_core::backend::Backend::dri3_syncobj_owned(&b, ClientId(7), xid + 1),
        "an unknown syncobj xid must not be owned",
    );
}

#[test]
fn none_origin_reversible_root_draw_not_routed_to_overlay() {
    let mut b = KmsBackend::for_tests();
    // stale GC scratch state as if a prior root+II XOR draw ran
    b.core.current_function = yserver_core::backend::GcFunction::Invert;
    b.core.current_subwindow_mode = yserver_core::backend::SubwindowMode::IncludeInferiors;
    let root = b.core.window_id;
    assert!(b.is_root_overlay_draw(root));
    // A None-origin op (ClearArea bg clear / put_image decomposition) must
    // NOT route to the overlay — it must fall through to backing paint.
    assert!(!b.should_route_root_overlay(root, None));
    assert!(b.should_route_root_overlay(
        root,
        Some(yserver_core::backend::OriginContext {
            client_id: yserver_protocol::x11::ClientId(3),
            nested_seq: 0,
            opcode: 70,
        })
    ));
}

/// Sanity: the v2 GC-clip intersection helper matches v1's shape.
/// A single source rect clipped against a 2-rect clip yields the
/// 2 expected intersection rectangles in dst space (clip origin
/// already applied).
#[test]
fn poly_fill_rectangle_honours_gc_clip() {
    use crate::kms::cpu_types::Rectangle16;
    use yserver_core::backend::ClipState;
    use yserver_protocol::x11::ClipRectangles;

    let mut b = KmsBackend::for_tests();
    // Two 4×8 clip rects side-by-side starting at (5, 5), with
    // clip origin (10, 10) → effective dst-coord rects at
    // (15, 15)-(19, 23) and (25, 15)-(29, 23).
    let mut wire: Vec<u8> = Vec::new();
    for (x, y, w, h) in [(5_i16, 5_i16, 4_u16, 8_u16), (15, 5, 4, 8)] {
        wire.extend_from_slice(&x.to_le_bytes());
        wire.extend_from_slice(&y.to_le_bytes());
        wire.extend_from_slice(&w.to_le_bytes());
        wire.extend_from_slice(&h.to_le_bytes());
    }
    b.core.current_clip = ClipState::Rectangles {
        origin: (10, 10),
        rects: ClipRectangles {
            ordering: 0,
            x_origin: 0,
            y_origin: 0,
            rectangles: wire,
        },
    };

    // Single source rect that spans both clip rects horizontally
    // and overflows top + bottom of the clip vertically.
    let src = [Rectangle16 {
        x: 0,
        y: 0,
        width: 100,
        height: 100,
    }];
    let out = b.intersect_with_current_clip(&src);
    assert_eq!(out.len(), 2);
    // First intersection — left clip rect after origin shift.
    assert_eq!(out[0].x, 15);
    assert_eq!(out[0].y, 15);
    assert_eq!(out[0].width, 4);
    assert_eq!(out[0].height, 8);
    // Second intersection — right clip rect after origin shift.
    assert_eq!(out[1].x, 25);
    assert_eq!(out[1].y, 15);
    assert_eq!(out[1].width, 4);
    assert_eq!(out[1].height, 8);
}

/// `gxcopy_planemask_diverts_to_logic_fill` per plan §3f tests.
/// Asserts that switching `KmsCore.current_function` to a
/// non-`Copy` value (here `Xor`) doesn't emit the
/// `fill_rects_non_gxcopy` or `copy_plane_non_gxcopy` gaps —
/// proves the Stage 3f.2 routing change took effect. Engine
/// itself returns `NoVk` on the stub fixture, so we can't assert
/// pixel correctness here (that's the Vk acceptance test); but
/// the gap absence is the load-bearing observable that the
/// diversion is wired through `fill_solid_rects` →
/// `engine.logic_fill` rather than the pre-3f.2 short-circuit.
#[test]
fn gxcopy_planemask_diverts_to_logic_fill() {
    use yserver_core::backend::GcFunction;
    let mut b = KmsBackend::for_tests();
    b.core.current_function = GcFunction::Xor;

    // Single rect: x=0 y=0 w=1 h=1.
    let mut wire = Vec::with_capacity(8);
    wire.extend_from_slice(&0_i16.to_le_bytes());
    wire.extend_from_slice(&0_i16.to_le_bytes());
    wire.extend_from_slice(&1_u16.to_le_bytes());
    wire.extend_from_slice(&1_u16.to_le_bytes());
    b.poly_fill_rectangle(None, 0xDEAD_BEEF, 0xFFFFFFFF, &wire)
        .expect("ok");
    let gaps = b.logged_gaps.borrow();
    assert!(
        !gaps.contains("fill_rects_non_gxcopy"),
        "stage 3f.1 fill_rects_non_gxcopy gap must not fire post-3f.2"
    );
    assert!(
        !gaps.contains("copy_plane_non_gxcopy"),
        "stage 3e.1 copy_plane_non_gxcopy gap must not fire post-3f.2"
    );
}

/// `set_clip_pixmap_stores_pixmap_clip` — Stage 3f.3 bookkeeping
/// gate. The pre-3f.3 stub logged a gap and cleared the clip to
/// `None`; 3f.3 stores the `ClipState::Pixmap` with the origin
/// preserved (mask sampling itself is deferred). A subsequent
/// `clear_clip_rectangles` returns to `None`.
#[test]
fn set_clip_pixmap_stores_pixmap_clip() {
    use yserver_core::backend::ClipState;
    let mut b = KmsBackend::for_tests();
    b.set_clip_pixmap(None, 0xABCD_EF01, 12, 34).expect("ok");
    match &b.core.current_clip {
        ClipState::Pixmap { origin, pixmap } => {
            assert_eq!(origin.0, 12);
            assert_eq!(origin.1, 34);
            assert_eq!(pixmap.as_raw(), 0xABCD_EF01);
        }
        other => panic!("expected ClipState::Pixmap, got {other:?}"),
    }
    // pre-3f.3 stub bumped a `set_clip_pixmap` gap; 3f.3 stores
    // bookkeeping cleanly.
    assert!(
        !b.logged_gaps.borrow().contains("set_clip_pixmap"),
        "set_clip_pixmap must not log a gap post-3f.3"
    );
    b.clear_clip_rectangles(None).expect("ok");
    assert!(matches!(b.core.current_clip, ClipState::None));
}

#[test]
fn apply_clip_state_preserves_cached_pixmap_mask_after_free_and_origin_change() {
    use yserver_core::backend::{ClipState, PixmapHandle};

    let mut b = KmsBackend::for_tests();
    // xid 0xABCD_EF01 is not in the store → store.lookup returns None →
    // clip_cache_reusable treats it as "source freed" → frozen snapshot
    // survives the apply without a re-readback.
    b.clip_mask_cache = Some(crate::kms::backend::ClipMaskCache {
        pixmap_xid: 0xABCD_EF01,
        drawable_id: crate::kms::render::store::DrawableId::for_tests(0),
        content_version: 0,
        origin: (0, 0),
        width: 5,
        height: 5,
        depth: 1,
        row_stride: 4,
        cpu_bytes_pending: false,
        bytes: vec![
            0x1f, 0, 0, 0, 0x1f, 0, 0, 0, 0x1f, 0, 0, 0, 0x1f, 0, 0, 0, 0x1f, 0, 0, 0,
        ],
    });
    b.apply_clip_state(
        None,
        &ClipState::Pixmap {
            origin: (7, 9),
            pixmap: PixmapHandle::from_raw(0xABCD_EF01).unwrap(),
        },
    )
    .expect("apply_clip_state");
    let cache = b.clip_mask_cache.as_ref().expect("cache");
    assert_eq!(cache.pixmap_xid, 0xABCD_EF01);
    assert_eq!(cache.origin, (7, 9));
}

/// Frozen-snapshot: clip→None then re-install same pixmap must NOT
/// trigger a re-readback. The cache bytes survive the None transition
/// (X11 retain-after-free snapshot contract).
///
/// The xid is registered in the store so that `store.lookup(xid)` resolves
/// to a live entry and `read_clip_mask_bytes` would actually record a
/// `GetImageSite::ClipMask` read (incrementing `clip_mask_reads`) if the
/// cache were absent or stale. That makes the reads_before == reads_after
/// assertion load-bearing: a cache miss on the live xid WOULD increment
/// the counter.
#[test]
fn clip_cache_retained_across_clip_none_same_pixmap_no_reread() {
    use crate::kms::render::store::DrawableKind;
    use ash::vk;
    use yserver_core::backend::{ClipState, PixmapHandle};

    let mut b = KmsBackend::for_tests();
    let xid: u32 = 0xBEEF_CAFE;

    // Allocate a real depth-1 pixmap in the store so store.lookup(xid)
    // resolves. read_clip_mask_bytes early-exits at the lookup and would
    // record a ClipMask read only when the entry IS found — making a
    // missed-cache observable via clip_mask_reads.
    let storage = Storage::for_tests_null(
        vk::Extent2D {
            width: 1,
            height: 1,
        },
        vk::Format::R8_UNORM,
    );
    let did = b
        .store
        .allocate(xid, DrawableKind::Pixmap, 1, false, storage)
        .expect("allocate");
    let version = b.store.get(did).expect("drawable").content_version;

    let expected_bytes = vec![0xAA, 0, 0, 0];
    b.clip_mask_cache = Some(crate::kms::backend::ClipMaskCache {
        pixmap_xid: xid,
        drawable_id: did,
        content_version: version,
        origin: (1, 2),
        width: 1,
        height: 1,
        depth: 1,
        row_stride: 4,
        cpu_bytes_pending: false,
        bytes: expected_bytes.clone(),
    });

    // Transition to clip=None — must NOT clear the cache.
    b.apply_clip_state(None, &ClipState::None)
        .expect("apply None");

    // Snapshot reads before the re-install. Because xid IS live in the
    // store, a cache miss here would call read_clip_mask_bytes and
    // increment clip_mask_reads. The assertion below is therefore
    // non-vacuous: it proves the cache was hit, not just that the counter
    // was already 0 due to an unregistered xid.
    let reads_before = b.telemetry.lifetime.clip_mask_reads;
    b.apply_clip_state(
        None,
        &ClipState::Pixmap {
            origin: (5, 6),
            pixmap: PixmapHandle::from_raw(xid).unwrap(),
        },
    )
    .expect("apply Pixmap");
    let reads_after = b.telemetry.lifetime.clip_mask_reads;
    assert_eq!(reads_after, reads_before, "no re-readback expected");
    let cache = b.clip_mask_cache.as_ref().expect("cache present");
    assert_eq!(cache.pixmap_xid, xid);
    assert_eq!(cache.bytes, expected_bytes);
    assert_eq!(cache.origin, (5, 6));
}

/// Frozen-snapshot: freeing the source pixmap must NOT evict the cache
/// (retain-after-free — the predicate returns true when lookup is None).
#[test]
fn clip_cache_retained_after_source_pixmap_freed() {
    use crate::kms::render::store::DrawableKind;
    use ash::vk;

    let mut b = KmsBackend::for_tests();
    let xid: u32 = 0xCAFE_0001;
    let storage = Storage::for_tests_null(
        vk::Extent2D {
            width: 4,
            height: 4,
        },
        vk::Format::R8_UNORM,
    );
    let did = b
        .store
        .allocate(xid, DrawableKind::Pixmap, 1, false, storage)
        .expect("allocate");
    // Seed cache for the live drawable.
    b.clip_mask_cache = Some(crate::kms::backend::ClipMaskCache {
        pixmap_xid: xid,
        drawable_id: did,
        content_version: 0,
        origin: (0, 0),
        width: 4,
        height: 4,
        depth: 1,
        row_stride: 4,
        cpu_bytes_pending: false,
        bytes: vec![0xFF, 0, 0, 0, 0xFF, 0, 0, 0, 0xFF, 0, 0, 0, 0xFF, 0, 0, 0],
    });
    // Free the source pixmap (decref with no-op callback).
    b.store.decref(&mut b.platform, did, |_| {});
    // store.lookup(xid) is now None → clip_cache_reusable must return true.
    assert!(
        b.clip_cache_reusable(xid),
        "cache must be reusable after source free (retain-after-free)"
    );
    assert!(
        b.clip_mask_cache.is_some(),
        "cache must not be evicted by decref"
    );
}

/// Frozen-snapshot is invalidated when the live drawable's content_version
/// changes (e.g. after a paint op on the mask pixmap).
#[test]
fn clip_cache_invalidated_on_content_version_bump() {
    use crate::kms::render::store::DrawableKind;
    use ash::vk;

    let mut b = KmsBackend::for_tests();
    let xid: u32 = 0xCAFE_0002;
    let storage = Storage::for_tests_null(
        vk::Extent2D {
            width: 4,
            height: 4,
        },
        vk::Format::R8_UNORM,
    );
    let did = b
        .store
        .allocate(xid, DrawableKind::Pixmap, 1, false, storage)
        .expect("allocate");
    let version_at_read = b.store.get(did).expect("drawable").content_version;
    b.clip_mask_cache = Some(crate::kms::backend::ClipMaskCache {
        pixmap_xid: xid,
        drawable_id: did,
        content_version: version_at_read,
        origin: (0, 0),
        width: 4,
        height: 4,
        depth: 1,
        row_stride: 4,
        cpu_bytes_pending: false,
        bytes: vec![0xFF, 0, 0, 0, 0xFF, 0, 0, 0, 0xFF, 0, 0, 0, 0xFF, 0, 0, 0],
    });
    // Cache is valid before bump.
    assert!(b.clip_cache_reusable(xid), "must be reusable before bump");
    // Simulate a write to the mask pixmap.
    b.store.mark_contents_modified(did);
    // Now content_version diverges → cache must be stale.
    assert!(
        !b.clip_cache_reusable(xid),
        "cache must be invalid after content_version bump"
    );
}

/// XID reuse: free a pixmap, re-allocate a new one at the same XID.
/// The new DrawableId won't match the cached one → stale hit prevented.
#[test]
fn clip_cache_free_realloc_same_xid_no_stale_hit() {
    use crate::kms::render::store::DrawableKind;
    use ash::vk;

    let mut b = KmsBackend::for_tests();
    let xid: u32 = 0xCAFE_0003;
    let storage_a = Storage::for_tests_null(
        vk::Extent2D {
            width: 4,
            height: 4,
        },
        vk::Format::R8_UNORM,
    );
    let did_a = b
        .store
        .allocate(xid, DrawableKind::Pixmap, 1, false, storage_a)
        .expect("allocate A");
    b.clip_mask_cache = Some(crate::kms::backend::ClipMaskCache {
        pixmap_xid: xid,
        drawable_id: did_a,
        content_version: 0,
        origin: (0, 0),
        width: 4,
        height: 4,
        depth: 1,
        row_stride: 4,
        cpu_bytes_pending: false,
        bytes: vec![0xAA, 0, 0, 0, 0xAA, 0, 0, 0, 0xAA, 0, 0, 0, 0xAA, 0, 0, 0],
    });
    // Free pixmap A.
    b.store.decref(&mut b.platform, did_a, |_| {});
    // Re-allocate at same XID → new DrawableId.
    let storage_b = Storage::for_tests_null(
        vk::Extent2D {
            width: 4,
            height: 4,
        },
        vk::Format::R8_UNORM,
    );
    let _did_b = b
        .store
        .allocate(xid, DrawableKind::Pixmap, 1, false, storage_b)
        .expect("allocate B");
    // DrawableId mismatch → cache must not be reusable.
    assert!(
        !b.clip_cache_reusable(xid),
        "cache must be invalid after xid reuse with new DrawableId"
    );
}

#[test]
fn clip_cache_install_defers_cpu_readback_until_cpu_clip_use() {
    use crate::kms::render::store::DrawableKind;
    use ash::vk;
    use yserver_core::backend::{ClipState, PixmapHandle};

    let mut b = KmsBackend::for_tests();
    let xid: u32 = 0xCAFE_0100;
    let storage = Storage::for_tests_null(
        vk::Extent2D {
            width: 8,
            height: 8,
        },
        vk::Format::R8_UNORM,
    );
    let did = b
        .store
        .allocate(xid, DrawableKind::Pixmap, 1, false, storage)
        .expect("allocate");

    let reads_before = b.telemetry.lifetime.clip_mask_reads;
    b.apply_clip_state(
        None,
        &ClipState::Pixmap {
            origin: (3, 4),
            pixmap: PixmapHandle::from_raw(xid).unwrap(),
        },
    )
    .expect("apply_clip_state");
    let reads_after = b.telemetry.lifetime.clip_mask_reads;

    assert_eq!(
        reads_after, reads_before,
        "installing a live clip pixmap must not synchronously read CPU bytes"
    );
    let cache = b.clip_mask_cache.as_ref().expect("cache");
    assert_eq!(cache.pixmap_xid, xid);
    assert_eq!(cache.drawable_id, did);
    assert_eq!(cache.origin, (3, 4));
    assert!(cache.cpu_bytes_pending, "CPU bytes should stay deferred");
    assert!(
        cache.bytes.is_empty(),
        "deferred CPU cache should not carry stale bytes"
    );
}

/// `set_gc_fill_tiled_stores_fill_state` — Stage 3f.3 bookkeeping
/// gate. Pre-3f.3 stub logged a gap; 3f.3 stores
/// `FillState::Tiled { pixmap, origin }` so subsequent fill ops
/// can route through the tiled-fill RENDER composite. xid=0
/// degenerates to `FillState::Solid`.
#[test]
fn set_gc_fill_tiled_stores_fill_state() {
    use yserver_core::backend::FillState;
    let mut b = KmsBackend::for_tests();
    b.set_gc_fill_tiled(None, 0xDEAD_BEEF, 5, 7).expect("ok");
    match &b.core.current_fill {
        FillState::Tiled { pixmap, origin } => {
            assert_eq!(pixmap.as_raw(), 0xDEAD_BEEF);
            assert_eq!(origin.0, 5);
            assert_eq!(origin.1, 7);
        }
        other => panic!("expected FillState::Tiled, got {other:?}"),
    }
    // xid=0 means PixmapHandle::from_raw returns None — falls
    // back to FillState::Solid (defensive; the dispatcher never
    // passes 0 here).
    b.set_gc_fill_tiled(None, 0, 0, 0).expect("ok");
    assert!(matches!(b.core.current_fill, FillState::Solid));

    assert!(
        !b.logged_gaps.borrow().contains("set_gc_fill_tiled"),
        "set_gc_fill_tiled must not log a gap post-3f.3"
    );
}

// ── depth-1 GXcopy GPU fast-path tests (Task 4, 2026-06-21) ──

/// depth-1 + GXcopy + full plane-mask MUST NOT increment
/// `cpufill_depth_lt8` or `cpufill_depth1_gxcopy`: the new fast-path
/// intercepts before the CPU-fallback gate. The stub engine returns
/// NoVk (no Vulkan in this fixture), so no paint_submit happens, but
/// the routing observable — the CPU-fallback counters stay zero — is
/// the load-bearing signal.
#[test]
fn depth1_gxcopy_fill_routes_to_gpu_not_cpu_readback() {
    use crate::kms::render::store::DrawableKind;
    use ash::vk;
    use yserver_core::backend::GcFunction;

    let mut b = KmsBackend::for_tests();
    let xid: u32 = 0xD100_0001;
    let storage = Storage::for_tests_null(
        vk::Extent2D {
            width: 4,
            height: 4,
        },
        vk::Format::R8_UNORM,
    );
    b.store
        .allocate(xid, DrawableKind::Pixmap, 1, false, storage)
        .expect("allocate depth-1");

    // Default state: GcFunction::Copy, current_plane_mask=u32::MAX → full
    assert!(matches!(b.core.current_function, GcFunction::Copy));

    let before_cpufill = b.telemetry.lifetime.cpufill_depth_lt8;
    let before_d1gxcopy = b.telemetry.lifetime.cpufill_depth1_gxcopy;

    b.fill_rectangle(None, xid, 1, 0, 0, 4, 4)
        .expect("fill_rectangle");

    assert_eq!(
        b.telemetry.lifetime.cpufill_depth_lt8, before_cpufill,
        "depth-1 GXcopy must NOT hit the CPU fallback (cpufill_depth_lt8)"
    );
    assert_eq!(
        b.telemetry.lifetime.cpufill_depth1_gxcopy, before_d1gxcopy,
        "depth-1 GXcopy must NOT hit the CPU fallback (cpufill_depth1_gxcopy)"
    );
}

/// depth-1 + non-Copy function (GXxor) MUST still use the CPU fallback
/// (boolean logic hazard in R8 byte-wise ops — the GPU path is only
/// valid for GXcopy). Asserts `cpufill_depth_lt8` increments by 1.
#[test]
fn depth1_noncopy_fill_still_cpu_fallback() {
    use crate::kms::render::store::DrawableKind;
    use ash::vk;
    use yserver_core::backend::GcFunction;

    let mut b = KmsBackend::for_tests();
    let xid: u32 = 0xD100_0002;
    let storage = Storage::for_tests_null(
        vk::Extent2D {
            width: 4,
            height: 4,
        },
        vk::Format::R8_UNORM,
    );
    b.store
        .allocate(xid, DrawableKind::Pixmap, 1, false, storage)
        .expect("allocate depth-1");

    b.core.current_function = GcFunction::Xor;

    let before = b.telemetry.lifetime.cpufill_depth_lt8;
    b.fill_rectangle(None, xid, 1, 0, 0, 4, 4)
        .expect("fill_rectangle");
    assert_eq!(
        b.telemetry.lifetime.cpufill_depth_lt8,
        before + 1,
        "depth-1 non-Copy must fall back to CPU path"
    );
}

/// depth-4 + GXcopy MUST still use the CPU fallback (no equivalence
/// proof for R8 depth-4 fills). Asserts `cpufill_depth_lt8` increments.
#[test]
fn depth4_fill_still_cpu_fallback() {
    use crate::kms::render::store::DrawableKind;
    use ash::vk;
    use yserver_core::backend::GcFunction;

    let mut b = KmsBackend::for_tests();
    let xid: u32 = 0xD400_0001;
    let storage = Storage::for_tests_null(
        vk::Extent2D {
            width: 4,
            height: 4,
        },
        vk::Format::R8_UNORM,
    );
    b.store
        .allocate(xid, DrawableKind::Pixmap, 4, false, storage)
        .expect("allocate depth-4");

    // Default: GcFunction::Copy
    assert!(matches!(b.core.current_function, GcFunction::Copy));

    let before = b.telemetry.lifetime.cpufill_depth_lt8;
    b.fill_rectangle(None, xid, 0x0F, 0, 0, 4, 4)
        .expect("fill_rectangle");
    assert_eq!(
        b.telemetry.lifetime.cpufill_depth_lt8,
        before + 1,
        "depth-4 GXcopy must still fall back to CPU path"
    );
}

/// End-to-end: writing a depth-1 pixmap via the GPU fast-path MUST
/// bump the drawable's `content_version`, so a stale clip-mask cache
/// entry (built before the fill) is invalidated. Requires a live
/// Vulkan ICD (lavapipe) to actually commit the logic_fill.
#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
fn clip_cache_invalidated_on_mask_depth1_gpu_fill() {
    use yserver_core::backend::GcFunction;

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // Allocate a depth-1 pixmap via the Backend trait (real Vk storage).
    let pix = b
        .create_pixmap(None, 1, 4, 4)
        .expect("create_pixmap depth-1");
    let xid = pix.as_raw();

    // Resolve the DrawableId for the xid.
    let did = b.store.lookup(xid).expect("drawable in store");

    // Snapshot the content_version before the fill.
    let version_before = b.store.get(did).expect("drawable").content_version;

    // Seed a clip-mask cache pretending this pixmap was read at
    // `version_before` — simulates having read it before the fill.
    b.clip_mask_cache = Some(crate::kms::backend::ClipMaskCache {
        pixmap_xid: xid,
        drawable_id: did,
        content_version: version_before,
        origin: (0, 0),
        width: 4,
        height: 4,
        depth: 1,
        row_stride: 4,
        cpu_bytes_pending: false,
        bytes: vec![0x00, 0, 0, 0, 0x00, 0, 0, 0, 0x00, 0, 0, 0, 0x00, 0, 0, 0],
    });
    assert!(
        b.clip_cache_reusable(xid),
        "cache must be reusable before the fill"
    );

    // Fill via the GPU fast-path (depth-1, GXcopy, full plane-mask).
    assert!(matches!(b.core.current_function, GcFunction::Copy));
    b.fill_rectangle(None, xid, 1, 0, 0, 4, 4)
        .expect("fill_rectangle depth-1");

    // The GPU fill MUST have bumped content_version.
    let version_after = b.store.get(did).expect("drawable").content_version;
    assert!(
        version_after > version_before,
        "GPU fill must bump content_version (before={version_before}, after={version_after})"
    );

    // Consequently the clip-mask cache is now stale.
    assert!(
        !b.clip_cache_reusable(xid),
        "clip-mask cache must be invalid after GPU fill bumped content_version"
    );
}
