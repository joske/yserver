use super::*;

/// #133 step 3 (3.3) — the bordered storage extent, `w + (bw << 1)`
/// per `compAllocPixmap` (`composite/compalloc.c:610`). Collapses to
/// exactly `(w, h)` at `bw == 0`: that identity is what keeps every
/// `bw == 0` desktop on the pre-#133 allocation.
#[test]
fn bordered_storage_extent_collapses_at_bw_zero() {
    assert_eq!(
        crate::kms::render::backend::bordered_storage_extent(100, 50, 0),
        (100, 50)
    );
    assert_eq!(
        crate::kms::render::backend::bordered_storage_extent(100, 50, 16),
        (132, 82)
    );
    assert_eq!(
        crate::kms::render::backend::bordered_storage_extent(1, 1, 1),
        (3, 3)
    );
}

// ───── #133 step 4 (P5) — the ring fill ─────

fn vkrect(x: i32, y: i32, w: u32, h: u32) -> ash::vk::Rect2D {
    ash::vk::Rect2D {
        offset: ash::vk::Offset2D { x, y },
        extent: ash::vk::Extent2D {
            width: w,
            height: h,
        },
    }
}

/// #133 step 4 (4.1) — the ring is `outer − content`, which is what
/// Xorg passes to `miPaintWindow(..., PW_BORDER)`
/// (`RegionSubtract(&exposed, &pWin->borderClip, &pWin->winSize)`,
/// `dix/window.c:1586`). Four disjoint rects, covering the annulus
/// exactly once, and NOTHING inside the content.
#[test]
fn border_ring_rects_tile_the_annulus_exactly_once() {
    // 100x50 content at (16, 16) inside 132x82 storage: bw = 16.
    let ring = crate::kms::render::backend::border_ring_rects(
        vkrect(0, 0, 132, 82),
        vkrect(16, 16, 100, 50),
    );
    assert_eq!(
        ring,
        vec![
            vkrect(0, 0, 132, 16),   // top band, full width
            vkrect(0, 66, 132, 16),  // bottom band, full width
            vkrect(0, 16, 16, 50),   // left bar
            vkrect(116, 16, 16, 50), // right bar
        ]
    );
    // Area check: outer − inner, and no double-covered pixel.
    let area: u32 = ring.iter().map(|r| r.extent.width * r.extent.height).sum();
    assert_eq!(area, 132 * 82 - 100 * 50);
    let mut covered = vec![0u8; (132 * 82) as usize];
    for r in &ring {
        for y in r.offset.y..r.offset.y + r.extent.height as i32 {
            for x in r.offset.x..r.offset.x + r.extent.width as i32 {
                covered[(y * 132 + x) as usize] += 1;
            }
        }
    }
    for y in 0..82i32 {
        for x in 0..132i32 {
            let inside_content = (16..116).contains(&x) && (16..66).contains(&y);
            assert_eq!(
                covered[(y * 132 + x) as usize],
                u8::from(!inside_content),
                "pixel ({x}, {y}) covered {} times",
                covered[(y * 132 + x) as usize],
            );
        }
    }
}

/// #133 step 4 — `bw == 0` IDENTITY at the geometry level: with the
/// content equal to the storage there is no ring, so no rect, so no
/// submit. Every WM in the smoke set is this path.
#[test]
fn border_ring_rects_is_empty_at_bw_zero() {
    assert!(
        crate::kms::render::backend::border_ring_rects(
            vkrect(0, 0, 100, 50),
            vkrect(0, 0, 100, 50)
        )
        .is_empty()
    );
}

/// A 1-px border still produces all four sides (the xts `makewin`
/// default, `xts5/src/lib/makewin2.c:232`), and a ring around a
/// content rect that is not at the storage origin — the redirected
/// ancestor's backing case — is placed relative to the content, not
/// to the storage.
#[test]
fn border_ring_rects_offset_content_and_one_pixel_border() {
    assert_eq!(
        crate::kms::render::backend::border_ring_rects(vkrect(0, 0, 3, 3), vkrect(1, 1, 1, 1)),
        vec![
            vkrect(0, 0, 3, 1),
            vkrect(0, 2, 3, 1),
            vkrect(0, 1, 1, 1),
            vkrect(2, 1, 1, 1),
        ]
    );
    // Content (40, 30, 10, 10) with bw = 5 inside a larger backing.
    let ring = crate::kms::render::backend::border_ring_rects(
        vkrect(35, 25, 20, 20),
        vkrect(40, 30, 10, 10),
    );
    assert_eq!(
        ring,
        vec![
            vkrect(35, 25, 20, 5),
            vkrect(35, 40, 20, 5),
            vkrect(35, 30, 5, 10),
            vkrect(50, 30, 5, 10),
        ]
    );
}

/// #133 step 4 — the ring thickness comes from the ALLOCATION, not
/// from the live `border_width` (step 3's invariant,
/// `storage_content_offset`). A `border_width` change that has not
/// reallocated must not move the ring, because it has not moved the
/// content either — re-basing here would paint border colour over
/// client pixels.
#[test]
fn border_ring_thickness_follows_the_allocation_not_the_geometry() {
    let mut b = KmsBackend::for_tests();
    let id = seed_bordered_window(&mut b, 0x4901, None, 10, 10, 100, 50, 16);
    let target = b.resolve_paint_target(0x4901).expect("resolve");
    assert_eq!(b.border_ring_thickness(0x4901, &target), 16);
    // XSetWindowBorderWidth(w, 3) with no reallocation: the
    // allocation still says 16, and so must the ring.
    b.windows.get_mut(&0x4901).expect("geom").border_width = 3;
    let target = b.resolve_paint_target(0x4901).expect("resolve");
    assert_eq!(b.border_ring_thickness(0x4901, &target), 16);
    // …and once the allocation is re-recorded, the ring follows it.
    b.store.set_content_offset(id, 3);
    let target = b.resolve_paint_target(0x4901).expect("resolve");
    assert_eq!(b.border_ring_thickness(0x4901, &target), 3);
}

/// #133 step 4 (4.3) — Xorg's depth-32 alpha rule
/// (`mi/miexpose.c:491-511`): a depth-32 window whose parent chain
/// reaches a depth-24 ancestor gets `fill.pixel |= 0xff000000`,
/// "Make sure alpha will sample as 1.0 for opaque windows". A
/// depth-32 chain keeps the client's alpha, and a non-32 window is
/// untouched here (`decode_x11_pixel_for_storage` forces α = 1.0
/// for it anyway).
#[test]
fn border_solid_pixel_applies_the_depth32_alpha_rule() {
    use crate::kms::render::store::{DrawableKind, Storage};
    let mut b = KmsBackend::for_tests();
    let mut seed = |xid: u32, parent: Option<u32>, depth: u8| {
        b.windows.insert(
            xid,
            crate::kms::render::backend::WindowGeometry {
                border_width: 4,
                border_pixel: Some(0x0012_3456),
                border_pixmap: None,
                x: 0,
                y: 0,
                width: 10,
                height: 10,
                depth,
                mapped: true,
                viewable: true,
                parent,
                stack_rank: 0,
                bg_pixel: None,
                bg_pixmap: None,
                cursor: None,
            },
        );
        b.store
            .allocate(
                xid,
                DrawableKind::Window,
                depth,
                true,
                Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: 18,
                        height: 18,
                    },
                    ash::vk::Format::B8G8R8A8_UNORM,
                ),
            )
            .expect("allocate")
    };
    // depth-24 top-level P24; depth-32 child C32 under it.
    let p24 = seed(0x4910, None, 24);
    let c32 = seed(0x4911, Some(0x4910), 32);
    // depth-32 top-level (parent is the root, which Xorg's
    // `while (orig_pWin && orig_pWin->parent)` never examines).
    let t32 = seed(0x4912, None, 32);

    let geom = |b: &KmsBackend, xid: u32| *b.windows.get(&xid).expect("geom");
    // Depth 24: no rule, the pixel passes through untouched.
    assert_eq!(b.border_solid_pixel(geom(&b, 0x4910), p24), 0x0012_3456);
    // Depth 32 under a depth-24 ancestor: alpha forced opaque.
    assert_eq!(b.border_solid_pixel(geom(&b, 0x4911), c32), 0xff12_3456);
    // Depth 32 all the way to the root: the client's alpha stands.
    assert_eq!(b.border_solid_pixel(geom(&b, 0x4912), t32), 0x0012_3456);
    // A depth-24 window painting into a depth-32 BACKING (a child
    // of a redirected depth-32 frame): Xorg gates on the PIXMAP's
    // depth and starts `effective_depth` from the WINDOW's, so this
    // takes the alpha with no parent walk at all. Getting it wrong
    // here is a transparent ring, because the depth-32 storage
    // reads alpha straight out of the pixel.
    assert_eq!(b.border_solid_pixel(geom(&b, 0x4910), c32), 0xff12_3456);
}

/// #133 step 3 (P4) case 1 — LEAF STORAGE. An unredirected window
/// paints into its own storage: the content offset is its own
/// `(bw, bw)` and the content clip is `(bw, bw, w, h)`.
#[test]
fn resolve_paint_target_leaf_storage_offsets_by_own_border() {
    let mut b = KmsBackend::for_tests();
    seed_bordered_window(&mut b, 0x4001, None, 30, 40, 100, 50, 16);
    let (offset, clip, bordered) = b
        .paint_target_shape_for_tests(0x4001)
        .expect("resolve leaf");
    assert_eq!(offset, (16, 16), "content starts at (bw, bw)");
    assert_eq!(clip, Some((16, 16, 100, 50)), "content clip");
    assert!(bordered);
    // The bw == 0 control: identity, on the pre-#133 arithmetic.
    seed_bordered_window(&mut b, 0x4002, None, 30, 40, 100, 50, 0);
    assert_eq!(
        b.paint_target_shape_for_tests(0x4002),
        Some(((0, 0), None, false)),
    );
}

/// #133 step 3 (P4) case 2 — ANCESTOR BACKING. Child C under
/// redirected ancestor W: the one-level translation is
/// `W.border_width + C.x + C.border_width` (spec P4), and the clip
/// is W's content ∩ C's content, both in backing coordinates.
#[test]
fn resolve_paint_target_ancestor_backing_adds_both_borders() {
    use crate::kms::render::store::{DrawableKind, Storage};
    let mut b = KmsBackend::for_tests();
    // W: 100x50, bw 8 → backing 116x66, W content at (8, 8).
    seed_bordered_window(&mut b, 0x4010, None, 0, 0, 100, 50, 8);
    // C: 20x10, bw 4, at (10, 20) inside W's content.
    seed_bordered_window(&mut b, 0x4011, Some(0x4010), 10, 20, 20, 10, 4);
    let w_id = b.store.lookup(0x4010).expect("W id");
    let b_id = b
        .store
        .allocate(
            0x4012,
            DrawableKind::Pixmap,
            24,
            true,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 116,
                    height: 66,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("backing allocate");
    b.store.set_redirected_target(w_id, Some(b_id));
    // Production records the backing's layout in
    // `allocate_redirected_backing`; mirror that here.
    b.store.set_content_offset(b_id, 8);

    let (offset, clip, bordered) = b.paint_target_shape_for_tests(0x4011).expect("resolve C");
    // 8 + 10 + 4 = 22 horizontally; 8 + 20 + 4 = 32 vertically.
    assert_eq!(offset, (22, 32), "W.bw + C.x + C.bw");
    assert!(bordered);
    // W's content (8, 8, 100, 50) ∩ C's content (22, 32, 20, 10).
    assert_eq!(clip, Some((22, 32, 20, 10)));

    // W itself, painting into its own backing: content clip derives
    // from W ALONE — no ancestor term — per spec §The redirection
    // exception rule 1 (`SetWinSize`, `dix/window.c:1720`).
    assert_eq!(
        b.paint_target_shape_for_tests(0x4010),
        Some(((8, 8), Some((8, 8, 100, 50)), true)),
    );
}

/// #133 step 3 (P4) case 3 — ROOT REDIRECTION. A top-level under a
/// redirected root: `T.x + T.border_width`, and the root itself
/// contributes no border term (the root window has no border).
#[test]
fn resolve_paint_target_root_redirection_adds_own_border_only() {
    use crate::kms::render::store::{DrawableKind, Storage};
    let mut b = KmsBackend::for_tests();
    // root storage is already seeded by `KmsCore::for_tests()`
    // (`init_root_storage`); it is deliberately NOT in `windows`.
    let root_id = b.store.lookup(b.core.window_id).expect("root id");
    let b_id = b
        .store
        .allocate(
            0x4021,
            DrawableKind::Pixmap,
            24,
            true,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 800,
                    height: 600,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("root backing allocate");
    b.store.set_redirected_target(root_id, Some(b_id));
    // Top-level T at (30, 40), 100x50, bw 6 (parent = None is the
    // production representation for a top-level).
    seed_bordered_window(&mut b, 0x4020, None, 30, 40, 100, 50, 6);

    let (offset, clip, bordered) = b.paint_target_shape_for_tests(0x4020).expect("resolve T");
    assert_eq!(offset, (36, 46), "T.x + T.bw, no root border term");
    assert_eq!(clip, Some((36, 46, 100, 50)));
    assert!(bordered);
}

/// #133 step 3 (P4) case 4 — NESTED DESCENDANTS. C under P under
/// redirected W accumulates `+ child.x` then `+ parent.bw` at every
/// level: `W.bw + P.x + P.bw + C.x + C.bw`.
#[test]
fn resolve_paint_target_nested_descendants_accumulate_every_level() {
    use crate::kms::render::store::{DrawableKind, Storage};
    let mut b = KmsBackend::for_tests();
    seed_bordered_window(&mut b, 0x4030, None, 0, 0, 200, 100, 8); // W, bw 8
    seed_bordered_window(&mut b, 0x4031, Some(0x4030), 10, 10, 100, 60, 4); // P, bw 4
    seed_bordered_window(&mut b, 0x4032, Some(0x4031), 5, 7, 20, 10, 2); // C, bw 2
    let w_id = b.store.lookup(0x4030).expect("W id");
    let b_id = b
        .store
        .allocate(
            0x4033,
            DrawableKind::Pixmap,
            24,
            true,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 216,
                    height: 116,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("backing allocate");
    b.store.set_redirected_target(w_id, Some(b_id));
    b.store.set_content_offset(b_id, 8);

    let (offset, clip, bordered) = b.paint_target_shape_for_tests(0x4032).expect("resolve C");
    // 8 + 10 + 4 + 5 + 2 = 29; 8 + 10 + 4 + 7 + 2 = 31.
    assert_eq!(offset, (29, 31));
    assert!(bordered);
    // Every bordered level contributes: W (8,8,200,100),
    // P (22,22,100,60) and C (29,31,20,10) intersected.
    assert_eq!(clip, Some((29, 31, 20, 10)));

    // The `bw == 0` control on the SAME shape: no border term, so the
    // walk lands on the pre-#133 offset, clipped only to C inside its
    // ancestors (C (15,17,20,10) ∩ P (10,10,100,60) ∩ W).
    let mut b0 = KmsBackend::for_tests();
    seed_bordered_window(&mut b0, 0x4040, None, 0, 0, 200, 100, 0);
    seed_bordered_window(&mut b0, 0x4041, Some(0x4040), 10, 10, 100, 60, 0);
    seed_bordered_window(&mut b0, 0x4042, Some(0x4041), 5, 7, 20, 10, 0);
    let w0 = b0.store.lookup(0x4040).expect("W id");
    let b0_id = b0
        .store
        .allocate(
            0x4043,
            DrawableKind::Pixmap,
            24,
            true,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 200,
                    height: 100,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("backing allocate");
    b0.store.set_redirected_target(w0, Some(b0_id));
    assert_eq!(
        b0.paint_target_shape_for_tests(0x4042),
        Some(((15, 17), Some((15, 17, 20, 10)), false)),
        "bw == 0 keeps the pre-#133 (x, y) accumulation and no border clip",
    );
}

/// #133 step 3 (P4) — a MANUALLY redirected window (its backing not
/// scene-participating) gets the same treatment as an automatic
/// one: Xorg keys the clip off `redirectDraw != RedirectDrawNone`
/// with no manual/automatic distinction (`SetWinSize`,
/// `dix/window.c:1720`; `SetBorderSize`, `:1747`).
#[test]
fn resolve_paint_target_redirect_clip_ignores_the_redirect_mode() {
    use crate::kms::render::store::{DrawableKind, Storage};
    for participating in [true, false] {
        let mut b = KmsBackend::for_tests();
        seed_bordered_window(&mut b, 0x4050, None, 0, 0, 100, 50, 8);
        let w_id = b.store.lookup(0x4050).expect("W id");
        let b_id = b
            .store
            .allocate(
                0x4051,
                DrawableKind::Pixmap,
                24,
                true,
                Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: 116,
                        height: 66,
                    },
                    ash::vk::Format::B8G8R8A8_UNORM,
                ),
            )
            .expect("backing allocate");
        b.store.set_redirected_target(w_id, Some(b_id));
        b.store.set_content_offset(b_id, 8);
        b.store.set_scene_participating(w_id, participating);
        assert_eq!(
            b.paint_target_shape_for_tests(0x4050),
            Some(((8, 8), Some((8, 8, 100, 50)), true)),
            "redirect mode (participating={participating}) must not change \
                 the content clip",
        );
    }
}

/// #133 step 3 (3.3) — a width/height resize reallocates storage at
/// the BORDERED extent. It goes through the same `Migrate` door as
/// a `border_width` change (#143); this pins the extent, the
/// content is pinned by the acceptance suite.
#[test]
fn resize_reallocates_storage_at_the_bordered_extent() {
    let mut b = KmsBackend::for_tests();
    seed_bordered_window(&mut b, 0x4060, None, 0, 0, 100, 50, 16);
    assert_eq!(b.storage_extent_for_tests(0x4060), Some((132, 82)));
    // Grow the content: storage follows as (w + 2bw) x (h + 2bw).
    if let Some(g) = b.windows.get_mut(&0x4060) {
        g.width = 200;
        g.height = 120;
    }
    b.sync_window_leaf_storage(0x4060, crate::kms::render::backend::LeafContent::Migrate);
    assert_eq!(b.storage_extent_for_tests(0x4060), Some((232, 152)));
    // A re-sync with nothing changed must NOT reallocate (the
    // compare-and-skip is against the bordered extent).
    let id_before = b.store.lookup(0x4060);
    b.sync_window_leaf_storage(0x4060, crate::kms::render::backend::LeafContent::Migrate);
    assert_eq!(b.store.lookup(0x4060), id_before, "no needless realloc");
}

/// #133 step 3 round 2 (P4) — the `RepeatNone` source-domain clip
/// is IDENTITY for every pre-#133 source: a pixmap (no domain at
/// all) and a `bw == 0` window (whose content IS its storage) both
/// yield `None`, so the composite clip list is byte-identical
/// there. A bordered window yields its own extent.
#[test]
fn picture_source_domain_clip_is_identity_without_a_border() {
    use crate::kms::render::engine::{ResolvedSource, SourceDrawable};
    let mut b = KmsBackend::for_tests();
    // bw == 0: storage IS the content.
    let plain = seed_bordered_window(&mut b, 0x4080, None, 0, 0, 100, 50, 0);
    let plain_src = ResolvedSource::Drawable(SourceDrawable::content(
        plain,
        (0, 0),
        ash::vk::Extent2D {
            width: 100,
            height: 50,
        },
    ));
    assert_eq!(
        crate::kms::render::backend::picture_source_domain_clip(
            &b.store,
            &plain_src,
            Repeat::None,
            None
        ),
        None,
        "bw == 0 must not add a clip term",
    );
    // A pixmap source carries no domain at all.
    let pix_src = ResolvedSource::Drawable(SourceDrawable::whole(plain));
    assert_eq!(
        crate::kms::render::backend::picture_source_domain_clip(
            &b.store,
            &pix_src,
            Repeat::None,
            None
        ),
        None,
    );
    // bw > 0: the window's own extent restricts the storage.
    let bordered = seed_bordered_window(&mut b, 0x4081, None, 0, 0, 100, 50, 16);
    let bordered_src = ResolvedSource::Drawable(SourceDrawable::content(
        bordered,
        (16, 16),
        ash::vk::Extent2D {
            width: 100,
            height: 50,
        },
    ));
    assert_eq!(
        crate::kms::render::backend::picture_source_domain_clip(
            &b.store,
            &bordered_src,
            Repeat::None,
            None
        ),
        Some(vec![Rectangle16 {
            x: 0,
            y: 0,
            width: 100,
            height: 50,
        }]),
    );
    // …but only for RepeatNone with no transform: the other repeat
    // modes wrap/clamp against the sampled image and a transformed
    // domain does not project to a rect in dst space.
    assert_eq!(
        crate::kms::render::backend::picture_source_domain_clip(
            &b.store,
            &bordered_src,
            Repeat::Normal,
            None
        ),
        None,
    );
    assert_eq!(
        crate::kms::render::backend::picture_source_domain_clip(
            &b.store,
            &bordered_src,
            Repeat::None,
            Some(&crate::kms::cpu_types::PictTransform::IDENTITY),
        ),
        None,
    );
}

/// #133 step 3 round 4 — the xts `Xlib4/XSetWindowBackgroundPixmap`
/// purpose-2 crash, root cause.
///
/// That purpose is the only Xlib4 purpose that sets a border width
/// on the window it then clears and reads
/// (`XSetWindowBorderWidth(display, w, 2)`), and `checktile`
/// (`xts5/src/lib/checktile.c:160-183`) reads it back with
/// `getsize()` → `XGetImage(d, 0, 0, width, height, …)` →
/// `XGetPixel` over the full `width x height`.
///
/// #133 step 6 (6.1) — `XSetWindowBorderWidth` on a window created
/// with `border_width = 0`: the awesome reproduction's shape and
/// the shape six xts5 `Xlib4` purposes use (`crechild` creates with
/// `bw = 0`, `xts5/src/lib/crechild.c:189`, and the purpose then
/// calls `XSetWindowBorderWidth`).
///
/// Before step 6 this changed the geometry mirror and nothing else,
/// so the allocation kept its unbordered layout and the ring stayed
/// 0 px wide however wide the client asked for. Now the bordered
/// extent moved, so the storage is reallocated and the new content
/// offset recorded — and, since the layout is a property of the
/// allocation, the resolver's content origin and clip follow in the
/// same step rather than describing a rectangle the storage cannot
/// accommodate.
#[test]
fn border_width_change_reallocates_and_records_the_new_layout() {
    use yserver_core::host_x11::HostSubwindowConfig;

    let mut b = KmsBackend::for_tests();
    seed_bordered_window(&mut b, 0x4090, None, 0, 0, 16, 8, 0);
    assert_eq!(
        b.paint_target_shape_for_tests(0x4090),
        Some(((0, 0), None, false)),
        "baseline: unbordered layout",
    );

    b.configure_subwindow(
        None,
        0x4090,
        HostSubwindowConfig {
            border_width: Some(2),
            ..HostSubwindowConfig::default()
        },
    )
    .expect("configure border width");
    assert_eq!(
        b.windows[&0x4090].border_width, 2,
        "the geometry mirror carries the new border width",
    );
    assert_eq!(
        b.storage_extent_for_tests(0x4090),
        Some((20, 12)),
        "the bordered extent moved, so the storage is reallocated at \
             (w + 2bw) x (h + 2bw)",
    );
    assert_eq!(
        b.paint_target_shape_for_tests(0x4090),
        Some(((2, 2), Some((2, 2, 16, 8)), true)),
        "…and the content origin is (bw, bw) in the new allocation",
    );
}

/// #133 step 6 (6.2) — the case prose alone would let an
/// implementer skip: `w=100,bw=2 → w=98,bw=3` in ONE configure.
/// The outer extent is 104 either way, so nothing is reallocated
/// (Xorg's `compReallocPixmap` takes its `else` branch,
/// `composite/compalloc.c:706`) — and the content offset still has
/// to move from 2 to 3, with the pixels.
///
/// Asserted here on the LAYOUT (no Vk in a unit test, so the copy
/// itself is a no-op); the pixels are asserted in
/// `render_acceptance.rs`'s
/// `border_change_with_unchanged_outer_extent_migrates_the_content`.
#[test]
fn border_change_with_unchanged_outer_extent_relocates_without_realloc() {
    use yserver_core::host_x11::HostSubwindowConfig;

    let mut b = KmsBackend::for_tests();
    seed_bordered_window(&mut b, 0x4091, None, 0, 0, 100, 60, 2);
    assert_eq!(b.storage_extent_for_tests(0x4091), Some((104, 64)));
    let id_before = b.store.lookup(0x4091);

    b.configure_subwindow(
        None,
        0x4091,
        HostSubwindowConfig {
            width: Some(98),
            height: Some(58),
            border_width: Some(3),
            ..HostSubwindowConfig::default()
        },
    )
    .expect("configure w + border width");

    assert_eq!(
        b.storage_extent_for_tests(0x4091),
        Some((104, 64)),
        "outer extent unchanged: 98 + 6 == 100 + 4",
    );
    assert_eq!(
        b.store.lookup(0x4091),
        id_before,
        "and therefore NOT reallocated (6.1)",
    );
    assert_eq!(
        b.paint_target_shape_for_tests(0x4091),
        Some(((3, 3), Some((3, 3, 98, 58)), true)),
        "the content offset moved with the border width (6.2)",
    );
}

/// #133 step 6 (6.4) — a pure `x`/`y` move changes the window's
/// screen origin, which the scene walk applies from the geometry,
/// and must relocate NOTHING storage-local.
///
/// Seeded with an allocation whose content offset deliberately
/// disagrees with the live `border_width` — the state step 3's
/// invariant exists for — because that is what makes the assertion
/// bite: an implementation that reacted to any geometry change by
/// re-basing the content off `border_width` would move pixels here.
#[test]
fn pure_move_relocates_nothing_storage_local() {
    use yserver_core::host_x11::HostSubwindowConfig;

    let mut b = KmsBackend::for_tests();
    let id = seed_bordered_window(&mut b, 0x4092, None, 10, 10, 100, 60, 4);
    // The allocation says 4; the live geometry says 9. Only step 6's
    // border-width path may reconcile that, and only by moving the
    // pixels — a move must leave it exactly as it is.
    b.windows.get_mut(&0x4092).expect("geom").border_width = 9;

    b.configure_subwindow(
        None,
        0x4092,
        HostSubwindowConfig {
            x: Some(200),
            y: Some(120),
            ..HostSubwindowConfig::default()
        },
    )
    .expect("configure move");

    assert_eq!(b.store.lookup(0x4092), Some(id), "no reallocation");
    assert_eq!(
        b.storage_extent_for_tests(0x4092),
        Some((108, 68)),
        "storage extent untouched by a move",
    );
    assert_eq!(
        b.store.get(id).map(|d| d.content_offset),
        Some(4),
        "the content offset is a property of the ALLOCATION; a move \
             changes the screen origin only",
    );
    assert_eq!(
        b.windows[&0x4092].x, 200,
        "…while the geometry mirror does move",
    );
}

/// #133 step 6 — the `bw == 0` identity, on the two configures a
/// `bw == 0` desktop actually sends.
///
/// A `CWBorderWidth` value equal to the current one is not a
/// change, so it must not reach the step-6 path at all (Xorg
/// short-circuits the same comparison: `if ((mask & CWBorderWidth)
/// && (bw != wBorderWidth(pWin)))`, `dix/window.c:2347`). And a
/// pure resize keeps the pre-#133 path: reallocate at `(w, h)`,
/// content offset 0, content DISCARDED (see `LeafContent`).
#[test]
fn bw_zero_configures_keep_the_pre_133_paths() {
    use yserver_core::host_x11::HostSubwindowConfig;

    let mut b = KmsBackend::for_tests();
    let id = seed_bordered_window(&mut b, 0x4093, None, 0, 0, 100, 60, 0);

    // CWBorderWidth = 0 on a bw == 0 window: not a change.
    b.configure_subwindow(
        None,
        0x4093,
        HostSubwindowConfig {
            border_width: Some(0),
            ..HostSubwindowConfig::default()
        },
    )
    .expect("configure bw 0 -> 0");
    assert_eq!(b.store.lookup(0x4093), Some(id), "no reallocation");
    assert_eq!(b.storage_extent_for_tests(0x4093), Some((100, 60)));
    assert_eq!(
        b.paint_target_shape_for_tests(0x4093),
        Some(((0, 0), None, false)),
        "no border clip term at bw == 0",
    );

    // A pure resize: the pre-#133 reallocate-and-discard path.
    b.configure_subwindow(
        None,
        0x4093,
        HostSubwindowConfig {
            width: Some(200),
            height: Some(120),
            ..HostSubwindowConfig::default()
        },
    )
    .expect("configure resize");
    assert_eq!(b.storage_extent_for_tests(0x4093), Some((200, 120)));
    assert_eq!(
        b.paint_target_shape_for_tests(0x4093),
        Some(((0, 0), None, false)),
        "still no border clip term",
    );
}

/// #133 step 6 (6.2) — the copy geometry itself: the source is the
/// OLD content rect (derived from the old allocation alone) clipped
/// to the new content extent, and the destination is the new
/// content origin, so a pixel at content `(cx, cy)` stays at
/// content `(cx, cy)`.
#[test]
fn migrated_content_copy_maps_content_origin_to_content_origin() {
    let ext = |w: u32, h: u32| ash::vk::Extent2D {
        width: w,
        height: h,
    };
    // The spec's case: 104x64 storage at offset 2 (content 100x60)
    // → content 98x58 at offset 3.
    assert_eq!(
        crate::kms::render::backend::migrated_content_copy(ext(104, 64), 2, 98, 58, 3),
        Some((vkrect(2, 2, 98, 58), ash::vk::Offset2D { x: 3, y: 3 })),
    );
    // Growing the border with the content unchanged (the awesome
    // reproduction): the whole old content moves outward.
    assert_eq!(
        crate::kms::render::backend::migrated_content_copy(ext(100, 60), 0, 100, 60, 16),
        Some((vkrect(0, 0, 100, 60), ash::vk::Offset2D { x: 16, y: 16 })),
    );
    // Shrinking the border to nothing (xts's bw -> 0).
    assert_eq!(
        crate::kms::render::backend::migrated_content_copy(ext(102, 62), 1, 100, 60, 0),
        Some((vkrect(1, 1, 100, 60), ash::vk::Offset2D { x: 0, y: 0 })),
    );
    // The intersection, when the content shrinks by more than the
    // border grows: only the surviving part is copied.
    assert_eq!(
        crate::kms::render::backend::migrated_content_copy(ext(104, 64), 2, 40, 20, 3),
        Some((vkrect(2, 2, 40, 20), ash::vk::Offset2D { x: 3, y: 3 })),
    );
    // Degenerate: an offset that swallows the whole allocation has
    // no content to move.
    assert_eq!(
        crate::kms::render::backend::migrated_content_copy(ext(4, 4), 2, 10, 10, 1),
        None
    );
}

/// #133 step 3 (3.5) — the direct-scanout gate's INPUT: a bordered
/// window's resolved paint target reports a border clip, which is
/// what `scanout_direct_eligible` rejects on. A fullscreen bordered
/// window therefore cannot take the flip path.
#[test]
fn bordered_fullscreen_window_reports_a_border_clip_to_the_scanout_gate() {
    let mut b = KmsBackend::for_tests();
    // Fullscreen-shaped candidate at the origin, but bordered.
    seed_bordered_window(&mut b, 0x4070, None, 0, 0, 800, 600, 2);
    let (_, _, bordered) = b
        .paint_target_shape_for_tests(0x4070)
        .expect("resolve bordered fullscreen");
    assert!(bordered, "bordered chain must report a border clip");
    assert!(
        !crate::kms::render::backend::scanout_direct_eligible(
            true, true, true, true, true, !bordered, 0, 0, 0
        ),
        "a bordered candidate must never be admitted to direct scanout",
    );
    // The same window unbordered is admitted (so the rejection is
    // attributable to the border, not to the rest of the gate).
    seed_bordered_window(&mut b, 0x4071, None, 0, 0, 800, 600, 0);
    let (_, _, plain) = b
        .paint_target_shape_for_tests(0x4071)
        .expect("resolve plain fullscreen");
    assert!(!plain);
    assert!(crate::kms::render::backend::scanout_direct_eligible(
        true, true, true, true, true, !plain, 0, 0, 0
    ));
}
